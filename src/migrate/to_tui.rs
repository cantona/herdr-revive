//! App -> TUI: each app space becomes a one-workspace `Snapshot` that
//! `layout::rebuild` (the `restore --recreate` path and its journal)
//! creates in the TUI. The app's layout document and terminals are only read.

use super::external;
use super::ledger::{self, Entry, Ledger, Same, TuiCreated, TuiSource, WorkspaceRecord, live_of};
use super::tree;
use super::tui::{self, Target};
use super::{
    Applied, PlanLine, Report, Request, Side, Status, TuiSession, drive, earlier_run, missing_note,
    uncaptured_reason,
};
use crate::core::hook::quote;
use crate::core::host::{CoreHost, RunMode, address_of};
use crate::core::paths;
use crate::layout::{self, TabLayout, WorkspaceLayout};
use crate::model::*;
use crate::store::{self, Store};
use crate::transport::{Host, Transport};
use anyhow::{Context as _, Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, Write};
use std::path::Path;

#[derive(Clone, Debug, Deserialize)]
pub struct DocSpace {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub folder: String,
    #[serde(default)]
    pub machine: Option<String>,
    #[serde(default)]
    pub tabs: Vec<DocTab>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct DocTab {
    pub id: String,
    #[serde(default)]
    pub name: String,
    pub root: Value,
}

impl DocSpace {
    fn machine(&self) -> Option<&str> {
        self.machine
            .as_deref()
            .filter(|machine| !machine.is_empty())
    }
}

pub fn read_spaces(doc: &Value) -> Result<Vec<DocSpace>> {
    let spaces = doc
        .pointer("/value/spaces")
        .context("the app's layout document has no spaces")?;
    serde_json::from_value(spaces.clone())
        .context("the app's layout document has an unexpected shape")
}

/// The core capture's panes by terminal address, which is what a doc
/// block's `content.terminal` names.
pub fn by_address(captured: &Snapshot) -> Result<BTreeMap<String, SavedPane>> {
    let mut index = BTreeMap::new();
    for pane in &captured.panes {
        index.insert(address_of(&pane.pane_id)?.to_string(), pane.clone());
    }
    Ok(index)
}

pub struct Built {
    pub snapshot: Option<Snapshot>,
    pub tabs: usize,
    pub panes: usize,
    pub agents: usize,
    pub denied: usize,
    pub mod_blocks: usize,
    /// The gone folders of the panes that start as a shell in HOME instead.
    pub missing: Vec<String>,
}

pub fn build_snapshot(
    space: &DocSpace,
    index: usize,
    label: &str,
    context: &Context,
) -> Result<Built> {
    let (session, captured, config, home) = (
        context.session,
        context.captured,
        context.config,
        context.home,
    );
    let workspace_id = format!("a{index}");
    let fallback_cwd = if space.folder.starts_with('/') {
        space.folder.clone()
    } else {
        home.to_string()
    };
    let mut panes: Vec<SavedPane> = Vec::new();
    let mut tabs = Vec::new();
    let mut mod_blocks = 0;
    let mut missing = Vec::new();
    for tab in &space.tabs {
        let Some((node, _)) = tree::parse_app_node(&tab.root, 0, &mut mod_blocks)? else {
            continue;
        };
        // Numbered among the tabs kept: a mod-only tab is not copied, and the
        // TUI numbers an unnamed tab by its position in the workspace.
        let number = tabs.len() + 1;
        let tab_id = format!("{workspace_id}:t{number}");
        let mut leaf = |address: &str| -> Result<String> {
            let pane_id = format!("{workspace_id}:p{}", panes.len() + 1);
            let (mut cwd, mut command) = match captured.get(address) {
                Some(saved) => (saved.cwd.clone(), saved.command.clone()),
                None => (fallback_cwd.clone(), None),
            };
            // A command would run in the wrong place; the plan says so.
            match (context.existing_folder)(&cwd) {
                Some(folder) => cwd = folder.to_string(),
                None => {
                    missing.push(tree::shown_folder(&cwd).to_string());
                    cwd = home.to_string();
                    command = None;
                }
            }
            panes.push(SavedPane {
                workspace_id: workspace_id.clone(),
                tab_id: tab_id.clone(),
                pane_id: pane_id.clone(),
                cwd,
                command,
            });
            Ok(pane_id)
        };
        let root = tree::to_binary(&node, &mut leaf)?;
        tabs.push(TabLayout {
            tab_id: tab_id.clone(),
            label: if tab.name.is_empty() {
                number.to_string()
            } else {
                tab.name.clone()
            },
            focused_pane_id: root.first_pane_id().to_string(),
            root,
        });
    }
    // An agent the policy denies starts as a shell: counted in `denied`
    // only, so it is not named as one that keeps running in the app.
    let agents = panes
        .iter()
        .filter(|pane| match &pane.command {
            Some(command @ CommandSpec::Agent { .. }) => command.allowed_argv(config).is_ok(),
            _ => false,
        })
        .count();
    let denied = panes
        .iter()
        .filter(|pane| {
            pane.command
                .as_ref()
                .is_some_and(|command| command.allowed_argv(config).is_err())
        })
        .count();
    let (tab_count, pane_count) = (tabs.len(), panes.len());
    let snapshot = if tabs.is_empty() {
        None
    } else {
        let snapshot = Snapshot {
            tool: TOOL.into(),
            schema: SCHEMA,
            session: session.into(),
            created_ms: store::now_ms()?,
            scope: Scope::Session,
            panes,
            layout: vec![WorkspaceLayout {
                workspace_id,
                label: label.into(),
                active_tab_id: None,
                tabs,
            }],
            focused_workspace_id: None,
        };
        snapshot
            .validate()
            .with_context(|| format!("space {} cannot be copied", space.id))?;
        Some(snapshot)
    };
    Ok(Built {
        snapshot,
        tabs: tab_count,
        panes: pane_count,
        agents,
        denied,
        mod_blocks,
        missing,
    })
}

pub struct Item {
    pub line: PlanLine,
    pub source: TuiSource,
    pub snapshot: Option<Snapshot>,
}

/// What planning reads besides the doc: the core capture by address, the
/// source policy, display names of unnamed spaces, the TUI's live
/// workspaces, the TUI store session and the HOME fallback cwd.
pub struct Context<'a> {
    pub captured: &'a BTreeMap<String, SavedPane>,
    pub config: &'a Config,
    pub names: &'a BTreeMap<String, String>,
    pub live: &'a LiveSnapshot,
    pub session: &'a str,
    pub home: &'a str,
    /// `platform::existing_folder`, which tests replace.
    pub existing_folder: fn(&str) -> Option<&str>,
    /// The spaces whose terminals the core capture could not read, with why.
    pub uncaptured: &'a BTreeMap<String, String>,
}

/// What the app calls an unnamed space when it cannot see its terminals:
/// the folder's last part, `~` for HOME or no folder.
fn folder_name(folder: &str, home: &str) -> String {
    let trimmed = folder.trim_end_matches('/');
    if folder.is_empty() || trimmed == home.trim_end_matches('/') {
        return "~".into();
    }
    match trimmed.rsplit('/').find(|part| !part.is_empty()) {
        Some(last) => last.into(),
        None => "/".into(),
    }
}

/// The TUI session a copy goes into, as a plan reports it and compares
/// earlier copies with.
pub fn tui_session(session: &str, live: &LiveSnapshot, spaces: &[DocSpace]) -> TuiSession {
    TuiSession {
        session: session.into(),
        workspaces: live
            .workspaces
            .iter()
            .map(|workspace| workspace.workspace_id.clone())
            .collect(),
        terminal_ids: tui::terminal_ids(live),
        labels: live
            .workspaces
            .iter()
            .map(|workspace| (workspace.workspace_id.clone(), workspace.label.clone()))
            .collect(),
        spaces: Some(present_spaces(spaces)),
    }
}

/// The app spaces of this machine (not another machine's).
fn present_spaces(spaces: &[DocSpace]) -> Vec<String> {
    spaces
        .iter()
        .filter(|space| space.machine().is_none())
        .map(|space| space.id.clone())
        .collect()
}

pub fn plan(
    spaces: &[DocSpace],
    only: &[String],
    context: &Context,
    ledger: &Ledger,
) -> Result<Vec<Item>> {
    for id in only {
        match spaces.iter().find(|space| &space.id == id) {
            None => bail!("no space {id} in the app"),
            Some(space) => {
                if let Some(machine) = space.machine() {
                    bail!("space {id} is on {machine}; use --machine {machine}");
                }
            }
        }
    }
    let selected = spaces
        .iter()
        .filter(|space| space.machine().is_none())
        .filter(|space| only.is_empty() || only.contains(&space.id));
    let tui_session = tui_session(context.session, context.live, spaces);
    let lives = tui_session.lives();
    // Only the records of spaces the app still has count: a closed one
    // neither matches nor makes a label ambiguous.
    let present = present_spaces(spaces);
    let listed = |space: &str| present.iter().any(|id| id == space);
    let tui_entries = ledger.tui_entries(context.session);
    let copy_pool: Vec<(&str, ledger::Seen)> = tui_entries
        .iter()
        .filter(|recorded| listed(&recorded.source.space))
        .map(|recorded| {
            (
                recorded.created.workspace_id.as_str(),
                recorded.created.seen(),
            )
        })
        .collect();
    let app_records: Vec<_> = ledger
        .app_entries(None, context.session)
        .into_iter()
        .chain(ledger.app_partials(None, context.session))
        .collect();
    let origin_pool: Vec<(&str, ledger::Seen)> = app_records
        .iter()
        .filter(|recorded| recorded.created.space.as_deref().is_some_and(listed))
        .map(|recorded| {
            (
                recorded.source.workspace_id.as_str(),
                recorded.source.seen(),
            )
        })
        .collect();
    let mut items = Vec::new();
    for (position, space) in selected.enumerate() {
        // The TUI shows any label it is given, an empty one included.
        let label = if !space.name.is_empty() {
            space.name.clone()
        } else if let Some(name) = context.names.get(&space.id).filter(|name| !name.is_empty()) {
            name.clone()
        } else {
            folder_name(&space.folder, context.home)
        };
        let source = TuiSource {
            space: space.id.clone(),
        };
        // A space whose terminals the core capture could not read is shown,
        // not copied.
        if let Some(reason) = context.uncaptured.get(&space.id) {
            items.push(Item {
                line: PlanLine::new(None, &space.id, label).skip(&uncaptured_reason(reason)),
                source,
                snapshot: None,
            });
            continue;
        }
        let built = build_snapshot(space, position + 1, &label, context)?;
        let mut line = PlanLine::new(None, &space.id, label);
        line.tabs = built.tabs;
        line.panes = built.panes;
        line.agents = built.agents;
        line.denied = built.denied;
        line.mod_blocks = built.mod_blocks;
        let missing = built.missing;
        // Only a copy into this TUI session makes the space done here. Its
        // workspace is found by the id it was copied to, or, after the TUI
        // renumbered its workspaces, by a terminal or a label only it has
        // (`ledger::live_of`).
        let copy = ledger
            .copied_to_tui(&source, context.session)
            .and_then(|recorded| {
                let record = WorkspaceRecord {
                    value: (),
                    id: &recorded.created.workspace_id,
                    seen: recorded.created.seen(),
                    at: recorded.at,
                };
                live_of(&record, &copy_pool, &lives)
                    .map(|(workspace, same, was)| (workspace, same, recorded.at, was))
            });
        // A space `--to app` made from a workspace of this session that is
        // still there would come back as a second copy of it.
        let origin = ledger
            .app_space_origin(&space.id)
            .filter(|recorded| {
                recorded.source.machine.is_none() && recorded.source.session == context.session
            })
            .and_then(|recorded| {
                let record = WorkspaceRecord {
                    value: (),
                    id: &recorded.source.workspace_id,
                    seen: recorded.source.seen(),
                    at: recorded.at,
                };
                live_of(&record, &origin_pool, &lives)
                    .map(|(workspace, same, was)| (workspace, same, recorded.at, was))
            });
        if built.snapshot.is_none() {
            line = line.skip("only mod panes");
        } else if let Some((workspace, same, at, was)) =
            copy.as_ref().filter(|copy| copy.1 != Same::No)
        {
            line.status = Status::Unchanged;
            line.copy = Some(workspace.clone());
            match was {
                Some(was) => line.notes.push(moved_copy_note(*same, was, workspace)),
                None if *same == Same::Label => line.notes.push(label_copy_note(*at, workspace)),
                None => {}
            }
        } else if let Some((workspace, same, at, was)) =
            origin.as_ref().filter(|origin| origin.1 != Same::No)
        {
            line = line.skip(&format!("copy of TUI workspace {workspace}"));
            match was {
                Some(was) => line.notes.push(moved_origin_note(*same, was, workspace)),
                None if *same == Same::Label => {
                    line.notes.push(label_origin_note(Some(*at), workspace));
                }
                None => {}
            }
        } else {
            // herdr gives a closed workspace's id to a new one after its
            // server restarts; a record under a live id that is neither that
            // workspace nor matched elsewhere is named, not trusted.
            if let Some((workspace, _, at, _)) = &copy {
                line.notes.push(copy_note(*at, workspace));
            }
            if let Some((workspace, _, at, _)) = &origin {
                line.notes.push(origin_note(Some(*at), workspace));
            }
        }
        if line.status == Status::Create && !missing.is_empty() {
            line.notes.push(missing_note(&missing));
        }
        // The core capture records an agent only from its running process,
        // so every agent copied here is still running in its app pane.
        if line.status == Status::Create && line.agents > 0 {
            line.notes.push(live_agents_note(line.agents));
        }
        items.push(Item {
            line,
            source,
            snapshot: built.snapshot,
        });
    }
    Ok(items)
}

/// The copied agents keep running in the app, as `--to app`'s live agents
/// keep running in the TUI.
pub fn live_agents_note(agents: usize) -> String {
    let (agents, run) = if agents == 1 {
        ("1 copied agent".to_string(), "runs")
    } else {
        (format!("{agents} copied agents"), "run")
    };
    format!("{agents} still {run} in the app: both resume the same session until one is closed")
}

/// An earlier copy of this space into a TUI workspace whose id another
/// workspace may have taken.
pub fn copy_note(at: u64, workspace: &str) -> String {
    format!(
        "{} copied this space to TUI workspace {workspace}; {workspace} may now be another \
         workspace that took its id: if it is the copy, close it first",
        earlier_run(Some(at))
    )
}

/// This space was made from a TUI workspace whose id another workspace may
/// have taken.
pub fn origin_note(at: Option<u64>, workspace: &str) -> String {
    format!(
        "{} made this space from TUI workspace {workspace}; {workspace} may now be another \
         workspace that took its id: if it is the same, this copies it back",
        earlier_run(at)
    )
}

/// The earlier copy of this space is a live workspace with another id than
/// the one it was copied to (`ledger::moved_note`).
pub fn moved_copy_note(same: Same, was: &str, workspace: &str) -> String {
    ledger::moved_note(
        same,
        was,
        &format!("in TUI workspace {workspace}"),
        &format!("{workspace} is another workspace, close or rename it to copy this space"),
    )
}

/// This space was made from a TUI workspace that now has another id
/// (`ledger::moved_note`).
pub fn moved_origin_note(same: Same, was: &str, workspace: &str) -> String {
    ledger::moved_note(
        same,
        was,
        &format!("from TUI workspace {workspace}"),
        &format!("{workspace} is another workspace, close or rename it to copy this space"),
    )
}

/// The earlier copy of this space is a live workspace that matches it only
/// by its label, which another workspace in the same folder can share.
pub fn label_copy_note(at: u64, workspace: &str) -> String {
    format!(
        "{} copied this space to TUI workspace {workspace}, which matches the copy only by \
         its label: if {workspace} is another workspace, rename it to copy this space again",
        earlier_run(Some(at))
    )
}

/// This space was made from a TUI workspace that a live one matches only by
/// its label.
pub fn label_origin_note(at: Option<u64>, workspace: &str) -> String {
    format!(
        "{} made this space from TUI workspace {workspace}, which matches it only by its \
         label: if {workspace} is another workspace, rename it to copy this space",
        earlier_run(at)
    )
}

pub fn apply(
    items: &[Item],
    host: &mut impl Host,
    target: &Target,
    store: &Store,
    policy_dir: &Path,
    ledger: &mut Ledger,
    report: &mut Report,
) -> Result<()> {
    for item in items
        .iter()
        .filter(|item| item.line.status == Status::Create)
    {
        let snapshot = item
            .snapshot
            .as_ref()
            .context("a planned space has no layout")?;
        let result = layout::rebuild(
            host,
            store,
            &target.socket,
            policy_dir,
            snapshot,
            false,
            layout::Start::LoginShell,
        )
        .map_err(|error| {
            let mut text = format!(
                "copy of {} stopped; TUI workspaces created for this space: {}",
                item.line.source,
                created_for_space(store)
            );
            if let Ok(Some(journal)) = layout::pending(store) {
                text.push_str("; ");
                text.push_str(&recovery_sentence(target, &journal.operation));
            }
            error.context(text)
        })?;
        let workspace = result["workspaces"][0]
            .as_str()
            .context("the rebuild reported no workspace")?;
        // The TUI shows the label it is given; what it lists is recorded
        // when it answers.
        let asked = snapshot.layout.first().map(|layout| layout.label.clone());
        let (terminal_ids, label) = match host.snapshot() {
            Ok(live) => (
                tui::terminal_ids(&live).remove(workspace),
                live.workspaces
                    .into_iter()
                    .find(|listed| listed.workspace_id == workspace)
                    .map(|listed| listed.label)
                    .or(asked),
            ),
            Err(error) => {
                report.warnings.push(format!(
                    "the TUI did not list workspace {workspace} after the copy ({error:#}); \
                     the next plan knows it only by its label"
                ));
                (None, asked)
            }
        };
        record_copy(
            ledger,
            report,
            item,
            &target.session,
            workspace,
            terminal_ids,
            label,
        )?;
    }
    Ok(())
}

/// Where the recovery sentence starts its commands: a run for another
/// machine puts "on M: " there.
pub const RECOVERY_RUN: &str = "acknowledged: run `";

pub fn on_machine(text: &str, machine: &str) -> String {
    text.replace(RECOVERY_RUN, &format!("acknowledged: on {machine}: run `"))
}

fn shell_word(path: &Path) -> String {
    let text = path.to_string_lossy();
    if !text.is_empty()
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"/._-+:@%,=".contains(&b))
    {
        text.into_owned()
    } else {
        quote(&text)
    }
}

/// The evidence a failed rebuild leaves stops the TUI plugin's own saves
/// and restores too, and the `recovery` commands need the paths the plugin
/// environment normally gives, so they are spelled out in full.
fn recovery_sentence(target: &Target, operation: &str) -> String {
    let base = format!(
        "herdr-revive --host herdr --config-dir {} --state-dir {} --socket {} recovery",
        shell_word(&target.config_dir),
        shell_word(&target.state_dir),
        shell_word(&target.socket)
    );
    format!(
        "TUI saves and restores are paused until the recovery evidence is {RECOVERY_RUN}{base} inspect`, then `{base} acknowledge {operation}`"
    )
}

/// Evidence left by an earlier failure is named with the commands that
/// clear it.
fn require_clear(store: &Store, target: &Target) -> Result<()> {
    crate::app::require_clear(store).map_err(|error| {
        let operation = match layout::pending(store) {
            Ok(Some(journal)) => journal.operation,
            _ => "GENERATION".into(),
        };
        anyhow::anyhow!("{error:#}; {}", recovery_sentence(target, &operation))
    })
}

/// The workspaces the failed rebuild's journal records.
fn created_for_space(store: &Store) -> String {
    match layout::pending(store) {
        Err(error) => format!("unknown (journal unreadable: {error:#})"),
        Ok(None) => "none".into(),
        Ok(Some(journal)) => {
            let made: BTreeSet<String> = journal
                .steps
                .into_iter()
                .filter_map(|step| step.workspace_id)
                .collect();
            if made.is_empty() {
                "none".into()
            } else {
                made.into_iter().collect::<Vec<_>>().join(" ")
            }
        }
    }
}

/// A successful rebuild has already cleared its journal, so a failed ledger
/// write is the only place left that can name the workspace for clean-up.
fn record_copy(
    ledger: &mut Ledger,
    report: &mut Report,
    item: &Item,
    session: &str,
    workspace: &str,
    terminal_ids: Option<Vec<String>>,
    label: Option<String>,
) -> Result<()> {
    report.record(Applied {
        source: item.line.source.clone(),
        created: json!({"session": session, "workspace_id": workspace}),
    });
    ledger
        .record(Entry::ToTui {
            source: item.source.clone(),
            created: TuiCreated {
                session: session.into(),
                workspace_id: workspace.into(),
                terminal_ids,
                label: label.map(|label| tree::space_name(&label)),
            },
            at: store::now_ms()?,
        })
        .with_context(|| {
            format!(
                "TUI workspace {workspace} was created for {} but the ledger did not record it; \
                 a later run would copy it again",
                item.line.source
            )
        })
}

pub fn run(
    request: &Request,
    input: &mut dyn BufRead,
    output: &mut dyn Write,
    errors: &mut dyn Write,
    tty: bool,
) -> Result<i32> {
    let target = tui::resolve(&request.tui)?;
    let store = target.store()?;
    require_clear(&store, &target)?;
    let tui_config = store::load_config(&target.config_dir)?;
    let mut tui_host = Transport::new(target.socket.clone(), target.binary.clone(), &tui_config);
    // Taken before the TUI is listed: the plan compares the ledger with the
    // live workspaces, and a run that overlaps this one must not record a
    // copy the listing has not seen yet.
    let mut ledger = Ledger::open(&ledger::dir()?)?;
    let live = tui_host
        .snapshot()
        .context("the TUI server does not answer")?;
    let core_socket = paths::resolve_socket(&paths::SocketInputs::from_env()?);
    let policy_dir = paths::config_dir(None)?;
    let core_config = store::load_config(&policy_dir)?;
    let mut core = CoreHost::connect(&core_socket, &core_config, RunMode::Emit)?;
    let spaces = read_spaces(&core.doc("herdr-app")?)?;
    let (_, core_session) = paths::session_identity(&core_socket)?;
    let terminals = core.snapshot().context("cannot read the app's terminals")?;
    // The core groups terminals by the space that made them.
    let selected: Vec<String> = terminals
        .workspaces
        .iter()
        .map(|workspace| workspace.workspace_id.clone())
        .filter(|id| request.only.is_empty() || request.only.contains(id))
        .collect();
    let super::Captured {
        snapshot: captured,
        warnings: captured_warnings,
        uncaptured,
    } = super::capture_each(&mut core, &core_session, &selected, None, &core_config)?;
    let captured = by_address(&captured)?;
    // Display names only matter for unnamed spaces, and come from the open
    // window; without one, `herdr-app` fails at once (nothing listens on its
    // socket, or it is not installed on this machine) and the space folder
    // names the workspace.
    let names: BTreeMap<String, String> = if spaces.iter().any(|space| space.name.is_empty()) {
        external::app_ls()
            .map(|ls| {
                ls.spaces
                    .into_iter()
                    .map(|space| (space.id, space.name))
                    .collect()
            })
            .unwrap_or_default()
    } else {
        BTreeMap::new()
    };
    let home = paths::home_dir()?;
    let home = home.to_str().context("HOME is not UTF-8")?;
    let context = Context {
        captured: &captured,
        config: &core_config,
        names: &names,
        live: &live,
        session: &target.session,
        home,
        existing_folder: crate::platform::existing_folder,
        uncaptured: &uncaptured,
    };
    let items = plan(&spaces, &request.only, &context, &ledger)?;
    let mut report = Report::new(
        Side::Tui,
        None,
        items.iter().map(|item| item.line.clone()).collect(),
    );
    report.tui_session = Some(tui_session(&target.session, &live, &spaces));
    report.warnings = captured_warnings;
    drive(
        report,
        request.confirm,
        request.json,
        input,
        output,
        errors,
        tty,
        |report| {
            // Taken only now, with waiting, so a plan or a prompt left open
            // does not hold off the plugin's own saves.
            let _lock = crate::app::wait_lock(&store, &tui_config)
                .context("cannot take the TUI session lock")?;
            require_clear(&store, &target)?;
            apply(
                &items,
                &mut tui_host,
                &target,
                &store,
                &policy_dir,
                &mut ledger,
                report,
            )
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::{Direction, Node};
    use crate::migrate::describe;
    use serde_json::json;

    const UUID: &str = "123e4567-e89b-12d3-a456-426614174000";

    fn doc() -> Value {
        json!({"version": 3, "value": {"spaces": [
            {"id": "s1", "name": "api", "folder": "/srv/api", "tabs": [
                {"id": "tb1", "name": "main", "root": {"type": "row", "id": "r1", "children": [
                    {"type": "block", "id": "b1", "content": {"terminal": "t1"}, "size": {"weight": 2}},
                    {"type": "block", "id": "b2", "content": {"terminal": "t2"}, "size": {"weight": 1}},
                    {"type": "block", "id": "b9", "content": {"mod": "files"}}
                ]}},
                {"id": "tb2", "name": "", "root": {"type": "block", "id": "b3", "content": {"terminal": "t3"}}}
            ]},
            {"id": "s2", "name": "", "folder": "", "tabs": [
                {"id": "tb3", "name": "", "root": {"type": "block", "id": "b8", "content": {}}}
            ]},
            {"id": "s3", "name": "far", "folder": "/x", "machine": "canix", "tabs": []}
        ]}})
    }

    /// `platform::existing_folder` for paths that do not exist here: a folder
    /// ending in `gone` is gone, any other is there, without its mark.
    fn fake_folder(cwd: &str) -> Option<&str> {
        let path = tree::shown_folder(cwd);
        (!path.ends_with("gone")).then_some(path)
    }

    fn captured() -> BTreeMap<String, SavedPane> {
        let pane = |address: &str, cwd: &str, command: Option<CommandSpec>| {
            (
                address.to_string(),
                SavedPane {
                    workspace_id: "s1".into(),
                    tab_id: "s1:ttb1".into(),
                    pane_id: format!("s1:p{address}"),
                    cwd: cwd.into(),
                    command,
                },
            )
        };
        BTreeMap::from([
            pane(
                "t1",
                "/srv/api",
                Some(CommandSpec::Agent {
                    executable: "claude".into(),
                    agent: AgentKind::Claude,
                    session_id: UUID.into(),
                    session_mode: AgentSessionMode::Resume,
                    claude_config_dir: None,
                    claude_home: None,
                    null_stdio: [false; 3],
                }),
            ),
            pane("t2", "/srv/api/web", None),
        ])
    }

    fn live(workspaces: &[&str]) -> LiveSnapshot {
        LiveSnapshot {
            focused_pane_id: None,
            workspaces: workspaces
                .iter()
                .map(|id| Workspace {
                    workspace_id: id.to_string(),
                    label: format!("[1] ws {id}"),
                    active_tab_id: None,
                })
                .collect(),
            tabs: vec![],
            // One pane each, on terminal `term_<workspace>`; labelled
            // `[1] ws <workspace>`.
            panes: workspaces
                .iter()
                .map(|id| LivePane {
                    workspace_id: id.to_string(),
                    tab_id: format!("{id}:t1"),
                    pane_id: format!("{id}:p1"),
                    terminal_id: format!("term_{id}"),
                    cwd: None,
                    agent: None,
                    agent_session: None,
                })
                .collect(),
            focused_workspace_id: None,
        }
    }

    fn planned(only: &[String], live_workspaces: &[&str], ledger: &Ledger) -> Result<Vec<Item>> {
        let names = BTreeMap::from([("s2".to_string(), "web".to_string())]);
        planned_named(only, live_workspaces, ledger, &names)
    }

    fn planned_named(
        only: &[String],
        live_workspaces: &[&str],
        ledger: &Ledger,
        names: &BTreeMap<String, String>,
    ) -> Result<Vec<Item>> {
        planned_live(only, &live(live_workspaces), ledger, names)
    }

    fn planned_live(
        only: &[String],
        live: &LiveSnapshot,
        ledger: &Ledger,
        names: &BTreeMap<String, String>,
    ) -> Result<Vec<Item>> {
        let config = Config {
            allowed_programs: vec!["claude".into()],
            ..Config::default()
        };
        let captured = captured();
        let session = "b".repeat(64);
        let context = Context {
            captured: &captured,
            config: &config,
            names,
            live,
            session: &session,
            home: "/home/u",
            existing_folder: fake_folder,
            uncaptured: &BTreeMap::new(),
        };
        plan(&read_spaces(&doc())?, only, &context, ledger)
    }

    fn leaf(id: &str) -> Node {
        Node::Pane {
            pane_id: id.into(),
            label: None,
        }
    }

    fn copied(ledger: &mut Ledger, session: String, workspace_id: &str) {
        let terminals = Some(vec![format!("term_{workspace_id}")]);
        copied_with(ledger, session, workspace_id, terminals, None);
    }

    fn copied_with(
        ledger: &mut Ledger,
        session: String,
        workspace_id: &str,
        terminal_ids: Option<Vec<String>>,
        label: Option<&str>,
    ) {
        ledger
            .record(Entry::ToTui {
                source: TuiSource { space: "s1".into() },
                created: TuiCreated {
                    session,
                    workspace_id: workspace_id.into(),
                    terminal_ids,
                    label: label.map(String::from),
                },
                at: 1_793_000_000_000,
            })
            .unwrap();
    }

    #[test]
    fn a_space_becomes_a_one_workspace_snapshot() {
        let root = tempfile::tempdir().unwrap();
        let ledger = Ledger::open(root.path()).unwrap();
        let items = planned(&[], &[], &ledger).unwrap();
        assert_eq!(items.len(), 2, "the canix space is left out");
        let line = &items[0].line;
        assert_eq!(line.status, Status::Create);
        assert_eq!(
            (
                line.name.as_str(),
                line.tabs,
                line.panes,
                line.agents,
                line.denied,
                line.mod_blocks
            ),
            ("api", 2, 3, 1, 0, 1)
        );
        let snapshot = items[0].snapshot.as_ref().unwrap();
        snapshot.validate().unwrap();
        assert_eq!(snapshot.session, "b".repeat(64));
        let workspace = &snapshot.layout[0];
        assert_eq!(
            (workspace.workspace_id.as_str(), workspace.label.as_str()),
            ("a1", "api")
        );
        assert_eq!(workspace.tabs[0].label, "main");
        assert_eq!(
            workspace.tabs[0].root,
            Node::Split {
                direction: Direction::Right,
                ratio: 2.0 / 3.0,
                first: Box::new(leaf("a1:p1")),
                second: Box::new(leaf("a1:p2")),
            }
        );
        assert_eq!(workspace.tabs[1].label, "2");
        assert_eq!(workspace.tabs[1].root, leaf("a1:p3"));
        let cwds: Vec<&str> = snapshot.panes.iter().map(|p| p.cwd.as_str()).collect();
        assert_eq!(cwds, ["/srv/api", "/srv/api/web", "/srv/api"]);
        assert!(matches!(
            snapshot.panes[0].command,
            Some(CommandSpec::Agent { .. })
        ));
        assert_eq!(snapshot.panes[2].command, None);
    }

    #[test]
    fn an_unnamed_space_the_window_cannot_name_takes_its_folder_name() {
        let root = tempfile::tempdir().unwrap();
        let ledger = Ledger::open(root.path()).unwrap();
        let unnamed = BTreeMap::from([("s2".to_string(), String::new())]);
        for names in [BTreeMap::new(), unnamed] {
            let items = planned_named(&[], &[], &ledger, &names).unwrap();
            assert_eq!(items[1].line.name, "~", "no folder is HOME");
        }
        assert_eq!(folder_name("/srv/api/", "/home/u"), "api");
        assert_eq!(folder_name("/home/u/", "/home/u"), "~");
        assert_eq!(folder_name("/home/u/x y", "/home/u"), "x y");
        assert_eq!(folder_name("/", "/home/u"), "/");
    }

    #[test]
    fn a_space_of_only_mod_panes_is_skipped_with_its_display_name() {
        let root = tempfile::tempdir().unwrap();
        let ledger = Ledger::open(root.path()).unwrap();
        let items = planned(&[], &[], &ledger).unwrap();
        let line = &items[1].line;
        assert_eq!(line.status, Status::Skip);
        assert_eq!(line.reason.as_deref(), Some("only mod panes"));
        assert_eq!(line.name, "web");
        assert_eq!(line.mod_blocks, 1);
        assert!(items[1].snapshot.is_none());
    }

    #[test]
    fn only_names_local_spaces() {
        let root = tempfile::tempdir().unwrap();
        let ledger = Ledger::open(root.path()).unwrap();
        let far = planned(&["s3".into()], &[], &ledger).err().unwrap();
        assert!(far.to_string().contains("--machine canix"));
        let missing = planned(&["zz".into()], &[], &ledger).err().unwrap();
        assert!(missing.to_string().contains("no space zz"));
        let one = planned(&["s1".into()], &[], &ledger).unwrap();
        assert_eq!(one.len(), 1);
    }

    #[test]
    fn a_space_is_unchanged_while_its_tui_workspace_exists() {
        let root = tempfile::tempdir().unwrap();
        let mut ledger = Ledger::open(root.path()).unwrap();
        copied(&mut ledger, "b".repeat(64), "w9");
        let items = planned(&[], &["w9"], &ledger).unwrap();
        assert_eq!(items[0].line.status, Status::Unchanged);
        assert_eq!(items[0].line.copy.as_deref(), Some("w9"));
        let items = planned(&[], &["w1"], &ledger).unwrap();
        assert_eq!(items[0].line.status, Status::Create);
        assert_eq!(items[0].line.copy, None);
    }

    #[test]
    fn a_workspace_that_took_the_copys_id_does_not_make_the_space_unchanged() {
        let root = tempfile::tempdir().unwrap();
        let mut ledger = Ledger::open(root.path()).unwrap();
        let note = "an earlier run at 2026-10-26 07:33 UTC copied this space to TUI \
                    workspace w9; w9 may now be another workspace that took its id: if it is \
                    the copy, close it first";
        // The live w9 is "[1] ws w9" on term_w9.
        let ids = |ids: &[&str]| Some(ids.iter().map(|id| id.to_string()).collect::<Vec<_>>());
        for (recorded, label, why) in [
            (
                ids(&["term_old"]),
                Some("api"),
                "reused: new label, new terminals",
            ),
            (ids(&[]), None, "no terminals, no label"),
            (None, None, "an entry from before ids and labels"),
        ] {
            copied_with(&mut ledger, "b".repeat(64), "w9", recorded, label);
            let items = planned(&[], &["w9"], &ledger).unwrap();
            let line = &items[0].line;
            assert_eq!(line.status, Status::Create, "{why}");
            assert_eq!(line.copy, None);
            assert_eq!(line.notes, [note.to_string(), live_agents_note(1)], "{why}");
            assert!(describe(line).contains(note));
        }
        let by_label = "an earlier run at 2026-10-26 07:33 UTC copied this space to TUI \
                        workspace w9, which matches the copy only by its label: if w9 is \
                        another workspace, rename it to copy this space again";
        for (recorded, label, notes, why) in [
            (
                ids(&["term_old"]),
                Some("ws w9"),
                vec![by_label],
                "restarted: same label, new terminals",
            ),
            (
                ids(&["term_closed", "term_w9"]),
                Some("api"),
                vec![],
                "renamed without a restart: a terminal is left",
            ),
            (
                ids(&["term_w9"]),
                Some("ws w9"),
                vec![],
                "a terminal is left and the label is the same",
            ),
        ] {
            copied_with(&mut ledger, "b".repeat(64), "w9", recorded, label);
            let items = planned(&[], &["w9"], &ledger).unwrap();
            assert_eq!(items[0].line.status, Status::Unchanged, "{why}");
            assert_eq!(items[0].line.notes, notes, "{why}");
        }
        let items = planned(&[], &["w1"], &ledger).unwrap();
        assert_eq!(items[0].line.notes, [live_agents_note(1)], "w9 is gone");
    }

    #[test]
    fn a_copy_in_another_tui_session_does_not_count() {
        let root = tempfile::tempdir().unwrap();
        let mut ledger = Ledger::open(root.path()).unwrap();
        copied(&mut ledger, "c".repeat(64), "w9");
        let items = planned(&[], &["w9"], &ledger).unwrap();
        assert_eq!(items[0].line.status, Status::Create);
        assert_eq!(items[0].line.copy, None);
        assert!(items[0].snapshot.is_some());
    }

    #[test]
    fn a_copy_into_another_tui_session_keeps_the_record_of_the_first() {
        let root = tempfile::tempdir().unwrap();
        let mut ledger = Ledger::open(root.path()).unwrap();
        copied(&mut ledger, "b".repeat(64), "w5");
        let items = planned(&[], &["w5"], &ledger).unwrap();
        assert_eq!(items[0].line.status, Status::Unchanged);
        copied(&mut ledger, "c".repeat(64), "w7");
        let items = planned(&[], &["w5"], &ledger).unwrap();
        assert_eq!(
            (items[0].line.status, items[0].line.copy.as_deref()),
            (Status::Unchanged, Some("w5")),
            "the copy into c does not erase the one into b"
        );
        assert_eq!(items[0].line.notes, Vec::<String>::new());
        let (origin, _) = ledger
            .tui_workspace_origin(&"b".repeat(64), "w5", ledger::Seen::default())
            .unwrap();
        assert_eq!(origin.source.space, "s1");
        let (origin, _) = ledger
            .tui_workspace_origin(&"c".repeat(64), "w7", ledger::Seen::default())
            .unwrap();
        assert_eq!(origin.source.space, "s1");
    }

    #[test]
    fn captured_panes_are_found_by_terminal_address() {
        let snapshot = Snapshot {
            tool: TOOL.into(),
            schema: SCHEMA,
            session: "b".repeat(64),
            created_ms: 1,
            scope: Scope::Session,
            panes: captured().into_values().collect(),
            layout: vec![],
            focused_workspace_id: None,
        };
        let index = by_address(&snapshot).unwrap();
        assert_eq!(index["t2"].cwd, "/srv/api/web");
    }

    fn built(space: &DocSpace, captured: &BTreeMap<String, SavedPane>) -> Built {
        let config = Config {
            allowed_programs: vec!["claude".into(), "make".into()],
            ..Config::default()
        };
        let live = live(&[]);
        let context = Context {
            captured,
            config: &config,
            names: &BTreeMap::new(),
            live: &live,
            session: &"b".repeat(64),
            home: "/home/u",
            existing_folder: fake_folder,
            uncaptured: &BTreeMap::new(),
        };
        build_snapshot(space, 1, "x", &context).unwrap()
    }

    #[test]
    fn a_pane_whose_folder_is_gone_starts_a_shell_at_home_and_the_plan_says_so() {
        let root = tempfile::tempdir().unwrap();
        let ledger = Ledger::open(root.path()).unwrap();
        let space = DocSpace {
            folder: "/srv/gone".into(),
            ..read_spaces(&doc()).unwrap().remove(0)
        };
        let mut captured = captured();
        if let Some(agent) = captured.get_mut("t1") {
            agent.cwd = "/srv/api/gone (deleted)".into();
        }
        if let Some(program) = captured.get_mut("t2") {
            program.cwd = "/srv/api/web (deleted)".into();
            program.command = Some(CommandSpec::Program {
                argv: vec!["make".into(), "watch".into()],
            });
        }
        // t1 (claude) is in a folder that is gone; t3 is not captured and takes
        // the space folder, which is gone too; t2's folder was removed and
        // made again, so it is where it was, with its command.
        let built = built(&space, &captured);
        assert_eq!((built.panes, built.agents), (3, 0));
        assert_eq!(built.missing, ["/srv/api/gone", "/srv/gone"]);
        let panes = &built.snapshot.as_ref().unwrap().panes;
        let at = |pane: &str| {
            let pane = panes.iter().find(|p| p.pane_id == pane).unwrap();
            (pane.cwd.as_str(), pane.command.is_some())
        };
        assert_eq!(at("a1:p1"), ("/home/u", false));
        assert_eq!(at("a1:p2"), ("/srv/api/web", true));
        assert_eq!(at("a1:p3"), ("/home/u", false));

        let spaces = vec![space];
        let live = live(&[]);
        let config = Config::default();
        let session = "b".repeat(64);
        let context = Context {
            captured: &captured,
            config: &config,
            names: &BTreeMap::new(),
            live: &live,
            session: &session,
            home: "/home/u",
            existing_folder: fake_folder,
            uncaptured: &BTreeMap::new(),
        };
        let items = plan(&spaces, &[], &context, &ledger).unwrap();
        assert_eq!(
            items[0].line.notes,
            [missing_note(&["/srv/api/gone".into(), "/srv/gone".into()])]
        );
    }

    #[test]
    fn a_copied_agent_is_named_as_still_running_in_the_app() {
        let root = tempfile::tempdir().unwrap();
        let mut ledger = Ledger::open(root.path()).unwrap();
        // s1 copies one claude pane (t1); s2 copies no agent.
        let items = planned(&[], &[], &ledger).unwrap();
        assert_eq!(items[0].line.agents, 1);
        assert_eq!(items[0].line.notes, [live_agents_note(1)]);
        assert!(describe(&items[0].line).ends_with(
            "; 1 copied agent still runs in the app: both resume the same session until \
                 one is closed"
        ));
        assert_eq!(
            live_agents_note(2),
            "2 copied agents still run in the app: both resume the same session until one \
             is closed"
        );
        // Not when the space is not copied.
        copied(&mut ledger, "b".repeat(64), "w9");
        let items = planned(&[], &["w9"], &ledger).unwrap();
        assert_eq!(items[0].line.status, Status::Unchanged);
        assert_eq!(items[0].line.notes, Vec::<String>::new());
    }

    #[test]
    fn an_agent_the_policy_denies_is_not_named_as_still_running_in_the_app() {
        let root = tempfile::tempdir().unwrap();
        let ledger = Ledger::open(root.path()).unwrap();
        let captured = captured();
        let live = live(&[]);
        let session = "b".repeat(64);
        // The core's policy allows nothing: s1's claude pane (t1) opens a shell.
        let config = Config::default();
        let context = Context {
            captured: &captured,
            config: &config,
            names: &BTreeMap::new(),
            live: &live,
            session: &session,
            home: "/home/u",
            existing_folder: fake_folder,
            uncaptured: &BTreeMap::new(),
        };
        let items = plan(&read_spaces(&doc()).unwrap(), &[], &context, &ledger).unwrap();
        let line = &items[0].line;
        assert_eq!((line.agents, line.denied), (0, 1));
        assert_eq!(line.notes, Vec::<String>::new(), "no live-agents note");
    }

    #[test]
    fn a_space_the_core_capture_could_not_read_is_a_skip_line() {
        let root = tempfile::tempdir().unwrap();
        let ledger = Ledger::open(root.path()).unwrap();
        let captured = captured();
        let live = live(&[]);
        let session = "b".repeat(64);
        let config = Config::default();
        let uncaptured = BTreeMap::from([("s1".to_string(), "terminal t1 is gone".to_string())]);
        let context = Context {
            captured: &captured,
            config: &config,
            names: &BTreeMap::new(),
            live: &live,
            session: &session,
            home: "/home/u",
            existing_folder: fake_folder,
            uncaptured: &uncaptured,
        };
        let items = plan(&read_spaces(&doc()).unwrap(), &[], &context, &ledger).unwrap();
        assert_eq!(
            (
                items[0].line.status,
                items[0].line.reason.as_deref(),
                items[0].line.name.as_str()
            ),
            (
                Status::Skip,
                Some("cannot capture: terminal t1 is gone"),
                "api"
            )
        );
        assert!(items[0].snapshot.is_none());
        assert_eq!(items[1].line.source, "s2", "the other spaces are planned");
    }

    #[test]
    fn a_command_the_source_policy_denies_is_counted() {
        // Two allowed commands, one denied and one shell, so neither an
        // inverted policy check nor counting shells gives 1.
        let mut captured = captured();
        let program = |argv: &[&str]| {
            let mut pane = captured["t2"].clone();
            pane.command = Some(CommandSpec::Program {
                argv: argv.iter().map(|arg| arg.to_string()).collect(),
            });
            pane
        };
        let (allowed, denied) = (program(&["make", "test"]), program(&["vim", "notes"]));
        captured.insert("t2".into(), allowed);
        captured.insert("t3".into(), denied);
        let space: DocSpace =
            serde_json::from_value(json!({"id": "s6", "folder": "/srv", "tabs": [
                {"id": "tb7", "root": {"type": "row", "children": [
                    {"type": "block", "content": {"terminal": "t1"}},
                    {"type": "block", "content": {"terminal": "t2"}},
                    {"type": "block", "content": {"terminal": "t3"}},
                    {"type": "block", "content": {"terminal": "t4"}}
                ]}}
            ]}))
            .unwrap();
        let built = built(&space, &captured);
        assert_eq!((built.panes, built.agents, built.denied), (4, 1, 1));
    }

    #[test]
    fn an_uncaptured_terminal_in_a_space_without_a_folder_starts_in_home() {
        let space: DocSpace = serde_json::from_value(json!({"id": "s4", "tabs": [
            {"id": "tb4", "root": {"type": "block", "content": {"terminal": "t8"}}}
        ]}))
        .unwrap();
        let built = built(&space, &captured());
        let snapshot = built.snapshot.unwrap();
        assert_eq!(snapshot.panes[0].cwd, "/home/u");
        assert_eq!(snapshot.panes[0].command, None);
    }

    #[test]
    fn an_unnamed_tab_is_numbered_among_the_tabs_kept() {
        let space: DocSpace =
            serde_json::from_value(json!({"id": "s5", "folder": "/srv", "tabs": [
                {"id": "tb5", "name": "", "root": {"type": "block", "content": {"mod": "files"}}},
                {"id": "tb6", "name": "", "root": {"type": "block", "content": {"terminal": "t9"}}}
            ]}))
            .unwrap();
        let built = built(&space, &BTreeMap::new());
        let tabs = &built.snapshot.unwrap().layout[0].tabs;
        assert_eq!(tabs.len(), 1);
        assert_eq!(
            (tabs[0].tab_id.as_str(), tabs[0].label.as_str()),
            ("a1:t1", "1")
        );
    }

    fn journal(store: &Store, workspaces: &[Option<&str>]) {
        let step = |workspace: &Option<&str>| layout::Step {
            method: "workspace.create".into(),
            source_id: "a1".into(),
            outcome: "applied".into(),
            workspace_id: workspace.map(String::from),
            tab_id: None,
            target_pane_id: None,
            target_terminal_id: None,
            created_pane_id: None,
        };
        let journal = layout::RebuildJournal {
            tool: TOOL.into(),
            schema: SCHEMA,
            session: store.session.clone(),
            operation: "e".repeat(64),
            generation: "g".into(),
            state: "failed".into(),
            acknowledged: false,
            snapshot_hash: "f".repeat(64),
            steps: workspaces.iter().map(step).collect(),
        };
        store::atomic_json(&store.root.join("rebuild-pending.json"), &journal).unwrap();
    }

    #[test]
    fn a_stopped_copy_names_what_its_journal_records() {
        let root = tempfile::tempdir().unwrap();
        let store = Store::new(root.path(), "b".repeat(64)).unwrap();
        assert_eq!(created_for_space(&store), "none");
        journal(&store, &[Some("w5"), None, Some("w5")]);
        assert_eq!(created_for_space(&store), "w5");
        std::fs::write(store.root.join("rebuild-pending.json"), b"{").unwrap();
        let unknown = created_for_space(&store);
        assert!(
            unknown.starts_with("unknown (journal unreadable: "),
            "{unknown}"
        );
    }

    #[test]
    fn a_copy_the_ledger_cannot_record_is_still_named() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("ledger");
        let mut ledger = Ledger::open(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500)).unwrap();
        let writable = std::fs::write(dir.join("probe"), b"").is_ok();
        let item = Item {
            line: PlanLine::new(None, "s1", "api".into()),
            source: TuiSource { space: "s1".into() },
            snapshot: None,
        };
        let mut report = Report::new(Side::Tui, None, vec![]);
        let result = record_copy(
            &mut ledger,
            &mut report,
            &item,
            &"b".repeat(64),
            "w7",
            Some(vec!["term_w7".into()]),
            Some("[3] api".into()),
        );
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let applied = report.applied.unwrap();
        assert_eq!(applied.len(), 1);
        assert_eq!(applied[0].created["workspace_id"], "w7");
        if writable {
            return; // running as root: the directory mode does not stop the write
        }
        let error = format!("{:#}", result.err().unwrap());
        assert!(error.contains("TUI workspace w7"), "{error}");
        assert!(error.contains("s1"), "{error}");
    }

    fn copied_from_tui(ledger: &mut Ledger, session: String, workspace_id: &str) {
        let terminals = Some(vec![format!("term_{workspace_id}")]);
        copied_from_tui_with(ledger, session, workspace_id, terminals, None);
    }

    fn copied_from_tui_with(
        ledger: &mut Ledger,
        session: String,
        workspace_id: &str,
        terminal_ids: Option<Vec<String>>,
        label: Option<&str>,
    ) {
        ledger
            .record(Entry::ToApp {
                source: ledger::AppSource {
                    machine: None,
                    session,
                    workspace_id: workspace_id.into(),
                    terminal_ids,
                    label: label.map(String::from),
                },
                created: ledger::AppCreated {
                    space: Some("s1".into()),
                    terminals: vec![],
                    unknown: false,
                },
                at: 1,
            })
            .unwrap();
    }

    #[test]
    fn a_space_copied_from_a_live_tui_workspace_is_not_copied_back() {
        let root = tempfile::tempdir().unwrap();
        let mut ledger = Ledger::open(root.path()).unwrap();
        copied_from_tui(&mut ledger, "b".repeat(64), "w6");
        let items = planned(&[], &["w6"], &ledger).unwrap();
        assert_eq!(items[0].line.status, Status::Skip);
        assert_eq!(
            items[0].line.reason.as_deref(),
            Some("copy of TUI workspace w6")
        );
        let items = planned(&[], &["w1"], &ledger).unwrap();
        assert_eq!(items[0].line.status, Status::Create, "w6 was closed");

        let root = tempfile::tempdir().unwrap();
        let mut ledger = Ledger::open(root.path()).unwrap();
        copied_from_tui(&mut ledger, "c".repeat(64), "w6");
        let items = planned(&[], &["w6"], &ledger).unwrap();
        assert_eq!(items[0].line.status, Status::Create, "another TUI session");
    }

    #[test]
    fn a_space_another_machine_copied_here_is_not_copied_back() {
        // Another machine's `--to app --machine` made s1 here from this TUI's
        // w5, and recorded it here through `migrate --record`.
        let root = tempfile::tempdir().unwrap();
        let source = ledger::AppSource {
            machine: Some("this".into()),
            session: "b".repeat(64),
            workspace_id: "w5".into(),
            terminal_ids: Some(vec!["term_w5".into()]),
            label: Some("api".into()),
        };
        let created = ledger::AppCreated {
            space: Some("this/s1".into()),
            terminals: vec!["this/t1".into()],
            unknown: false,
        };
        let entry = crate::migrate::remote::entry_for_machine("this", &source, &created, 3);
        let args = crate::migrate::remote::record_args(&entry, false).unwrap();
        crate::migrate::remote::record_here(&args[2], false, root.path()).unwrap();
        let ledger = Ledger::open(root.path()).unwrap();
        let items = planned(&[], &["w5"], &ledger).unwrap();
        assert_eq!(
            (items[0].line.status, items[0].line.reason.as_deref()),
            (Status::Skip, Some("copy of TUI workspace w5"))
        );
    }

    /// Live workspaces `(id, label)` on terminals `new_<id>`.
    fn relabelled(workspaces: &[(&str, &str)]) -> LiveSnapshot {
        let ids: Vec<&str> = workspaces.iter().map(|(id, _)| *id).collect();
        let mut live = live(&ids);
        for (workspace, (_, label)) in live.workspaces.iter_mut().zip(workspaces) {
            workspace.label = label.to_string();
        }
        for pane in &mut live.panes {
            pane.terminal_id = format!("new_{}", pane.workspace_id);
        }
        live
    }

    #[test]
    fn spaces_are_matched_after_the_tui_renumbers_its_workspaces() {
        let made_from = |ledger: &mut Ledger, space: &str, workspace: &str, label: &str, at| {
            ledger
                .record(Entry::ToApp {
                    source: ledger::AppSource {
                        machine: None,
                        session: "b".repeat(64),
                        workspace_id: workspace.into(),
                        terminal_ids: Some(vec![format!("old_{label}")]),
                        label: Some(label.into()),
                    },
                    created: ledger::AppCreated {
                        space: Some(space.into()),
                        terminals: vec![],
                        unknown: false,
                    },
                    at,
                })
                .unwrap();
        };
        let copied_to = |ledger: &mut Ledger, space: &str, workspace: &str, label: &str, at| {
            ledger
                .record(Entry::ToTui {
                    source: TuiSource {
                        space: space.into(),
                    },
                    created: TuiCreated {
                        session: "b".repeat(64),
                        workspace_id: workspace.into(),
                        terminal_ids: Some(vec![format!("old_{label}")]),
                        label: Some(label.into()),
                    },
                    at,
                })
                .unwrap();
        };
        let only = ["s1".to_string()];
        let names = BTreeMap::new();
        // w2 "api" and w3 "web" were numbered w1 and w2 again by a rebuild,
        // with new terminals.
        let renumbered = relabelled(&[("w1", "[1] api"), ("w2", "[2] web")]);
        for (s1, s9, live, was) in [
            (("w2", "api"), ("w3", "web"), "w1", "w2"),
            (("w3", "web"), ("w2", "api"), "w2", "w3"),
        ] {
            let root = tempfile::tempdir().unwrap();
            let mut ledger = Ledger::open(root.path()).unwrap();
            made_from(&mut ledger, "s1", s1.0, s1.1, 1);
            made_from(&mut ledger, "s9", s9.0, s9.1, 2);
            let items = planned_live(&only, &renumbered, &ledger, &names).unwrap();
            let line = &items[0].line;
            assert_eq!(line.status, Status::Skip, "{s1:?}");
            let reason = format!("copy of TUI workspace {live}");
            assert_eq!(line.reason.as_deref(), Some(reason.as_str()));
            assert_eq!(line.notes, [moved_origin_note(Same::Label, was, live)]);

            let root = tempfile::tempdir().unwrap();
            let mut ledger = Ledger::open(root.path()).unwrap();
            copied_to(&mut ledger, "s1", s1.0, s1.1, 1);
            copied_to(&mut ledger, "s9", s9.0, s9.1, 2);
            let items = planned_live(&only, &renumbered, &ledger, &names).unwrap();
            let line = &items[0].line;
            assert_eq!(line.status, Status::Unchanged, "{s1:?}");
            assert_eq!(line.copy.as_deref(), Some(live));
            assert_eq!(line.notes, [moved_copy_note(Same::Label, was, live)]);
        }
        // Two live workspaces are named "api": the label tells neither apart,
        // and the record under w2, now "web", is named, not trusted.
        let twins = relabelled(&[("w1", "[1] api"), ("w2", "[2] web"), ("w3", "[3] api")]);
        let root = tempfile::tempdir().unwrap();
        let mut ledger = Ledger::open(root.path()).unwrap();
        made_from(&mut ledger, "s1", "w2", "api", 1);
        let items = planned_live(&only, &twins, &ledger, &names).unwrap();
        assert_eq!(items[0].line.status, Status::Create);
        assert_eq!(
            items[0].line.notes,
            [origin_note(Some(1), "w2"), live_agents_note(1)]
        );
        let root = tempfile::tempdir().unwrap();
        let mut ledger = Ledger::open(root.path()).unwrap();
        copied_to(&mut ledger, "s1", "w2", "api", 1);
        let items = planned_live(&only, &twins, &ledger, &names).unwrap();
        assert_eq!(items[0].line.status, Status::Create);
        assert_eq!(
            items[0].line.notes,
            [copy_note(1, "w2"), live_agents_note(1)]
        );
    }

    #[test]
    fn a_closed_spaces_record_does_not_make_a_label_ambiguous() {
        // w2 "api" was copied to s0, which was closed; renumbered to w1, api
        // was copied again to s1. Renumbered again, api is w3 with new
        // terminals. s0 is not in the app.
        let root = tempfile::tempdir().unwrap();
        let mut ledger = Ledger::open(root.path()).unwrap();
        for (space, workspace, at) in [("s0", "w2", 1), ("s1", "w1", 2)] {
            ledger
                .record(Entry::ToApp {
                    source: ledger::AppSource {
                        machine: None,
                        session: "b".repeat(64),
                        workspace_id: workspace.into(),
                        terminal_ids: Some(vec![format!("old_{workspace}")]),
                        label: Some("api".into()),
                    },
                    created: ledger::AppCreated {
                        space: Some(space.into()),
                        terminals: vec![],
                        unknown: false,
                    },
                    at,
                })
                .unwrap();
        }
        let names = BTreeMap::new();
        for live in [
            relabelled(&[("w3", "[1] api")]),
            relabelled(&[("w1", "[1] web"), ("w3", "[2] api")]),
        ] {
            let items = planned_live(&["s1".to_string()], &live, &ledger, &names).unwrap();
            let line = &items[0].line;
            assert_eq!(
                (line.status, line.reason.as_deref()),
                (Status::Skip, Some("copy of TUI workspace w3"))
            );
            assert_eq!(line.notes, [moved_origin_note(Same::Label, "w1", "w3")]);
        }
        assert_eq!(
            moved_origin_note(Same::Label, "w1", "w3"),
            "matched an earlier copy from TUI workspace w3 by label (was w1): if w3 is another \
             workspace, close or rename it to copy this space"
        );
        assert_eq!(
            moved_copy_note(Same::Terminal, "w1", "w3"),
            "matched an earlier copy in TUI workspace w3 by terminal (was w1)"
        );
    }

    #[test]
    fn a_space_copied_from_a_workspace_whose_id_was_taken_is_copied_with_a_note() {
        let root = tempfile::tempdir().unwrap();
        let mut ledger = Ledger::open(root.path()).unwrap();
        for (recorded, label) in [
            (Some(vec!["term_old".to_string()]), Some("api")),
            (None, None),
        ] {
            copied_from_tui_with(&mut ledger, "b".repeat(64), "w6", recorded.clone(), label);
            let items = planned(&[], &["w6"], &ledger).unwrap();
            let line = &items[0].line;
            assert_eq!(line.status, Status::Create, "{recorded:?}");
            assert_eq!(
                line.notes,
                [origin_note(Some(1), "w6"), live_agents_note(1)],
                "{recorded:?}"
            );
        }
        let restarted = Some(vec!["term_old".to_string()]);
        copied_from_tui_with(&mut ledger, "b".repeat(64), "w6", restarted, Some("ws w6"));
        let items = planned(&[], &["w6"], &ledger).unwrap();
        assert_eq!(
            items[0].line.status,
            Status::Skip,
            "restarted: same label, new terminals"
        );
        assert_eq!(items[0].line.notes, [label_origin_note(Some(1), "w6")]);
        assert_eq!(
            label_origin_note(None, "w6"),
            "an earlier run made this space from TUI workspace w6, which matches it only by \
             its label: if w6 is another workspace, rename it to copy this space"
        );
        let left = Some(vec!["term_w6".to_string()]);
        copied_from_tui_with(&mut ledger, "b".repeat(64), "w6", left, Some("ws w6"));
        let items = planned(&[], &["w6"], &ledger).unwrap();
        assert_eq!(items[0].line.status, Status::Skip, "a terminal is left");
        assert_eq!(
            items[0].line.notes,
            Vec::<String>::new(),
            "a terminal is left"
        );
        assert_eq!(
            origin_note(None, "w6"),
            "an earlier run made this space from TUI workspace w6; w6 may now be another \
             workspace that took its id: if it is the same, this copies it back"
        );
    }

    #[test]
    fn a_space_from_another_machines_tui_is_copied() {
        let root = tempfile::tempdir().unwrap();
        let mut ledger = Ledger::open(root.path()).unwrap();
        ledger
            .record(Entry::ToApp {
                source: ledger::AppSource {
                    machine: Some("canix".into()),
                    session: "b".repeat(64),
                    workspace_id: "w6".into(),
                    terminal_ids: Some(vec!["term_fixture_1".into()]),
                    label: None,
                },
                created: ledger::AppCreated {
                    space: Some("s1".into()),
                    terminals: vec![],
                    unknown: false,
                },
                at: 1,
            })
            .unwrap();
        let items = planned(&[], &["w6"], &ledger).unwrap();
        assert_eq!(items[0].line.status, Status::Create);
    }

    fn target(root: &Path) -> Target {
        Target {
            socket: root.join("herdr.sock"),
            session: "b".repeat(64),
            state_dir: root.join("my state"),
            config_dir: root.join("config"),
            binary: None,
        }
    }

    #[test]
    fn the_recovery_commands_name_every_path_the_plugin_would_give() {
        let target = Target {
            socket: "/run/h/herdr.sock".into(),
            session: "b".repeat(64),
            state_dir: "/s/it's here".into(),
            config_dir: "/c".into(),
            binary: None,
        };
        let base = "herdr-revive --host herdr --config-dir /c --state-dir '/s/it'\\''s here' \
                    --socket /run/h/herdr.sock recovery";
        assert_eq!(
            recovery_sentence(&target, "OP"),
            format!(
                "TUI saves and restores are paused until the recovery evidence is acknowledged: \
                 run `{base} inspect`, then `{base} acknowledge OP`"
            )
        );
        assert_eq!(
            on_machine(&recovery_sentence(&target, "OP"), "canix"),
            format!(
                "TUI saves and restores are paused until the recovery evidence is acknowledged: \
                 on canix: run `{base} inspect`, then `{base} acknowledge OP`"
            )
        );
    }

    #[test]
    fn pending_evidence_is_refused_with_its_exact_commands() {
        let root = tempfile::tempdir().unwrap();
        let target = target(root.path());
        let store = Store::new(&target.state_dir, target.session.clone()).unwrap();
        require_clear(&store, &target).unwrap();
        journal(&store, &[Some("w5")]);
        let error = format!("{:#}", require_clear(&store, &target).unwrap_err());
        assert!(
            error.starts_with("reconstruction evidence requires recovery inspection"),
            "{error}"
        );
        assert!(
            error.ends_with(&format!("recovery acknowledge {}`", "e".repeat(64))),
            "{error}"
        );
        assert!(error.contains("--state-dir '"), "{error}");
    }
}
