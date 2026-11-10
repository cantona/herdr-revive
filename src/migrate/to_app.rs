//! TUI -> app: one `space.new` per workspace (with its first tab), one
//! `tab.new` per further tab, one `pane.split` per split, walked from the
//! TUI's binary tree. Split sizes are not copied: the app's commands take
//! none.

use super::external::{self, AppDo, AppLs, Made};
use super::ledger::{
    self, AppCreated, AppSource, Entry, Ledger, Same, Seen, WorkspaceRecord, live_of, match_live,
    names_nothing,
};
use super::tree::{self, Launch};
use super::tui::Export;
use super::{
    Applied, PlanLine, Report, Request, Side, Status, created_text, drive, earlier_run,
    missing_note, uncaptured_reason,
};
use crate::layout::{Node, WorkspaceLayout};
use crate::model::CommandSpec;
use crate::{platform, store};
use anyhow::{Context, Result, bail};
use serde_json::json;
use std::collections::{BTreeMap, HashSet};
use std::io::{BufRead, Write};

pub struct Item {
    pub line: PlanLine,
    pub source: AppSource,
    pub workspace: WorkspaceLayout,
    /// This ledger's partial copies that name nothing and are this
    /// workspace (`ledger::live_of`), as (source, at): a full copy of it
    /// drops them.
    pub stale: Vec<(AppSource, u64)>,
}

/// A TUI workspace as another machine's export records it.
fn recorded_seen<'a>(terminal_ids: &'a Option<Vec<String>>, label: &'a Option<String>) -> Seen<'a> {
    Seen {
        terminal_ids: terminal_ids.as_deref(),
        label: label.as_deref(),
    }
}

/// The spaces and the terminals the app lists.
fn listing(ls: &AppLs) -> (HashSet<&str>, HashSet<&str>) {
    let present = ls.spaces.iter().map(|space| space.id.as_str()).collect();
    let listed = ls
        .spaces
        .iter()
        .flat_map(|space| &space.tabs)
        .flat_map(|tab| &tab.terminals)
        .map(|listed| listed.terminal.as_str())
        .collect();
    (present, listed)
}

/// Fails on a layout leaf with no launch, so a malformed export (say, from
/// another version on another machine) stops before anything is created.
pub fn plan(
    export: &Export,
    machine: Option<&str>,
    ledger: &Ledger,
    ls: &AppLs,
) -> Result<Vec<Item>> {
    let (present, listed) = listing(ls);
    let lives = export.lives();
    let mut items = Vec::new();
    for workspace in &export.snapshot.layout {
        for tab in &workspace.tabs {
            let mut leaves = Vec::new();
            tab.root
                .leaves(0, &mut leaves)
                .with_context(|| format!("workspace {}: invalid layout", workspace.workspace_id))?;
            if let Some(pane) = leaves
                .into_iter()
                .find(|pane| !export.launches.contains_key(*pane))
            {
                bail!(
                    "workspace {}: pane {pane} has no launch in the export",
                    workspace.workspace_id
                );
            }
        }
        let panes: Vec<_> = export
            .snapshot
            .panes
            .iter()
            .filter(|pane| pane.workspace_id == workspace.workspace_id)
            .collect();
        let owned = |id: &&String| panes.iter().any(|pane| &pane.pane_id == *id);
        let live = export.seen(&workspace.workspace_id);
        let source = AppSource {
            machine: machine.map(String::from),
            session: export.session.clone(),
            workspace_id: workspace.workspace_id.clone(),
            terminal_ids: live.terminal_ids.map(<[String]>::to_vec),
            label: Some(tree::space_name(&workspace.label)),
        };
        let mut line = PlanLine::new(
            machine,
            &workspace.workspace_id,
            tree::space_name(&workspace.label),
        );
        line.tabs = workspace.tabs.len();
        line.panes = panes.len();
        // A pane whose folder is gone starts as a shell (`Export::missing_folders`).
        let missing: Vec<String> = panes
            .iter()
            .filter_map(|pane| export.missing_folders.get(&pane.pane_id).cloned())
            .collect();
        // An agent the source's policy denies starts as a shell: it is
        // counted in `denied` only, and nothing resumes it a second time.
        line.agents = panes
            .iter()
            .filter(|pane| matches!(pane.command, Some(CommandSpec::Agent { .. })))
            .filter(|pane| !export.missing_folders.contains_key(&pane.pane_id))
            .filter(|pane| !export.denied.contains(&pane.pane_id))
            .count();
        line.denied = export.denied.iter().filter(owned).count();
        line.live_agents = export
            .live_agents
            .iter()
            .filter(owned)
            .filter(|pane| !export.denied.contains(pane))
            .cloned()
            .collect();
        let id = &workspace.workspace_id;
        // Every record of this TUI session, matched to this workspace by its
        // id, or, after the TUI renumbered its workspaces, by a terminal or a
        // label only it has (`ledger::match_live`). Copies into another
        // machine's app, its copies back and its partial copies come from
        // that machine's export, named as `herdr-app` here lists them.
        let app_entries = ledger.app_entries(machine, &export.session);
        let mut copies: Vec<WorkspaceRecord<String>> = app_entries
            .iter()
            .filter_map(|recorded| {
                Some(WorkspaceRecord {
                    value: recorded.created.space.clone()?,
                    id: &recorded.source.workspace_id,
                    seen: recorded.source.seen(),
                    at: recorded.at,
                })
            })
            .collect();
        // This ledger has only this machine's TUI copies, and a session id (a
        // hash of the socket path) can repeat on another machine, so another
        // machine's copies come from its export.
        let mut origins: Vec<WorkspaceRecord<(String, Option<u64>)>> = Vec::new();
        let tui_entries = ledger.tui_entries(&export.session);
        let app_partials = ledger.app_partials(machine, &export.session);
        let mut partials: Vec<WorkspaceRecord<AppCreated>> = app_partials
            .iter()
            .map(|partial| WorkspaceRecord {
                value: partial.created.clone(),
                id: &partial.source.workspace_id,
                seen: partial.source.seen(),
                at: partial.at,
            })
            .collect();
        match machine {
            None => origins.extend(tui_entries.iter().map(|recorded| WorkspaceRecord {
                value: (recorded.source.space.clone(), Some(recorded.at)),
                id: &recorded.created.workspace_id,
                seen: recorded.created.seen(),
                at: recorded.at,
            })),
            Some(machine) => {
                let prefixed = |id: &String| format!("{machine}/{id}");
                copies.extend(export.app_copies.iter().map(|copy| WorkspaceRecord {
                    value: prefixed(&copy.space),
                    id: &copy.workspace_id,
                    seen: recorded_seen(&copy.terminal_ids, &copy.label),
                    at: copy.at,
                }));
                origins.extend(
                    export
                        .tui_copies
                        .iter()
                        .filter(|copy| copy.session == export.session)
                        .map(|copy| WorkspaceRecord {
                            value: (prefixed(&copy.space), copy.at),
                            id: &copy.workspace_id,
                            seen: recorded_seen(&copy.terminal_ids, &copy.label),
                            at: copy.at.unwrap_or(0),
                        }),
                );
                partials.extend(export.partial_copies.iter().map(|copy| WorkspaceRecord {
                    value: AppCreated {
                        space: copy.created.space.as_ref().map(prefixed),
                        terminals: copy.created.terminals.iter().map(prefixed).collect(),
                        unknown: copy.created.unknown,
                    },
                    id: &copy.workspace_id,
                    seen: recorded_seen(&copy.terminal_ids, &copy.label),
                    at: copy.at,
                }));
            }
        }
        // A copy or an origin counts while its space exists, so a closed
        // one neither matches nor makes a label ambiguous.
        copies.retain(|copy| present.contains(copy.value.as_str()));
        origins.retain(|origin| present.contains(origin.value.0.as_str()));
        let copy = match_live(&copies, &lives, id);
        let origin = match_live(&origins, &lives, id);
        let open = |created: &AppCreated| partial_open(created, &present, &listed);
        // A copy that timed out before it knew its space may have left one
        // nothing ties to it: it goes with the live workspace it is
        // (`ledger::live_of`), not with whichever now has its id. One that
        // knows its space is checked like any other.
        let unknowns: Vec<&WorkspaceRecord<AppCreated>> = partials
            .iter()
            .filter(|partial| partial.value.unknown && partial.value.space.is_none())
            .collect();
        let unknown_pool: Vec<(&str, Seen)> = unknowns
            .iter()
            .map(|partial| (partial.id, partial.seen))
            .collect();
        let unknown = unknowns
            .iter()
            .filter(|partial| {
                live_of(partial, &unknown_pool, &lives).is_some_and(|(live, ..)| live == *id)
            })
            .max_by_key(|partial| partial.at)
            .map(|partial| (partial.value.clone(), partial.at));
        let nothing_pool: Vec<(&str, Seen)> = app_partials
            .iter()
            .filter(|partial| names_nothing(partial.created))
            .map(|partial| (partial.source.workspace_id.as_str(), partial.source.seen()))
            .collect();
        let stale: Vec<(AppSource, u64)> = app_partials
            .iter()
            .filter(|partial| names_nothing(partial.created))
            .filter(|partial| {
                let record = WorkspaceRecord {
                    value: (),
                    id: &partial.source.workspace_id,
                    seen: partial.source.seen(),
                    at: partial.at,
                };
                live_of(&record, &nothing_pool, &lives)
                    .is_some_and(|(live, same, _)| live == *id && same != Same::No)
            })
            .map(|partial| (partial.source.clone(), partial.at))
            .collect();
        partials.retain(|partial| open(&partial.value));
        let open_partial = match_live(&partials, &lives, id);
        if panes.is_empty() {
            line = line.skip("no panes");
        } else if let Some(copy) = copy.as_ref().filter(|copy| copy.same != Same::No) {
            line.status = Status::Unchanged;
            line.copy = Some(copy.value.clone());
            match &copy.was {
                Some(was) => line.notes.push(ledger::moved_note(
                    copy.same,
                    was,
                    &copy.value,
                    &format!(
                        "this is another workspace, close {} or rename this one",
                        copy.value
                    ),
                )),
                None if copy.same == Same::Label => {
                    line.notes.push(label_copy_note(copy.at, id, &copy.value));
                }
                None => {}
            }
        } else if let Some(origin) = origin.as_ref().filter(|origin| origin.same != Same::No) {
            let (space, at) = &origin.value;
            line = line.skip(&format!("copy of app space {space}"));
            match &origin.was {
                Some(was) => line.notes.push(ledger::moved_note(
                    origin.same,
                    was,
                    &format!("from app space {space}"),
                    &format!("this is another workspace, close {space} or rename this one"),
                )),
                None if origin.same == Same::Label => {
                    line.notes.push(label_origin_note(*at, id, space));
                }
                None => {}
            }
        } else if let Some(partial) = open_partial
            .as_ref()
            .filter(|partial| partial.same != Same::No)
        {
            // A second copy would leave the first one's agents running too,
            // and the ledger would no longer tie that space to this source.
            line = line.skip(&partial_reason(&partial.value));
            line.partial = Some(partial.value.clone());
            match &partial.was {
                Some(was) => line.notes.push(ledger::moved_note(
                    partial.same,
                    was,
                    &partial_name(&partial.value),
                    "this is another workspace, close it or rename this one",
                )),
                None if partial.same == Same::Label => line.notes.push(format!(
                    "the partial copy {} matches this workspace only by its label: if it is \
                     another workspace, close it or rename this one to copy it",
                    partial_name(&partial.value)
                )),
                None => {}
            }
        } else {
            // herdr gives a closed workspace's id to a new one after its
            // server restarts; a record under this id that is neither this
            // workspace nor matched elsewhere is named, not trusted.
            if let Some(copy) = &copy {
                let space = &copy.value;
                line.notes.push(format!(
                    "{} copied a TUI workspace {id} to space {space}; this may be another \
                     workspace that took its id: if it is the same, close {space} first",
                    earlier_run(Some(copy.at))
                ));
            }
            if let Some(origin) = &origin {
                let (space, at) = &origin.value;
                line.notes.push(format!(
                    "{} made a TUI workspace {id} from app space {space}; this may be another \
                     workspace that took its id: if it is the same, this copies it back",
                    earlier_run(*at)
                ));
            }
            // An open partial copy of a workspace that had this id before is
            // named, not trusted.
            if let Some(partial) = &open_partial {
                line.notes.push(format!(
                    "an earlier partial copy {} may be from another workspace that had this \
                     id: if it is the same, close it first",
                    partial_name(&partial.value)
                ));
            }
            if let Some((created, at)) = &unknown {
                line.notes.push(unknown_partial_note(created, *at));
            }
        }
        if line.status == Status::Create && !missing.is_empty() {
            line.notes.push(missing_note(&missing));
        }
        items.push(Item {
            line,
            source,
            workspace: workspace.clone(),
            stale,
        });
    }
    // A workspace the capture could not read is shown, not copied.
    for uncaptured in &export.uncaptured {
        let name = tree::space_name(&uncaptured.label);
        let line = PlanLine::new(machine, &uncaptured.workspace_id, name.clone())
            .skip(&uncaptured_reason(&uncaptured.reason));
        items.push(Item {
            line,
            source: AppSource {
                machine: machine.map(String::from),
                session: export.session.clone(),
                workspace_id: uncaptured.workspace_id.clone(),
                terminal_ids: export
                    .terminal_ids_of(&uncaptured.workspace_id)
                    .map(<[String]>::to_vec),
                label: Some(name),
            },
            workspace: WorkspaceLayout {
                workspace_id: uncaptured.workspace_id.clone(),
                label: uncaptured.label.clone(),
                active_tab_id: None,
                tabs: Vec::new(),
            },
            stale: Vec::new(),
        });
    }
    Ok(items)
}

/// The earlier copy of a workspace that matches it only by its label, which
/// another workspace in the same folder can share.
fn label_copy_note(at: u64, workspace: &str, space: &str) -> String {
    format!(
        "{} copied a TUI workspace {workspace} to space {space}, and this one matches it \
         only by its label: if it is another workspace, close {space} or rename this one \
         to copy it",
        earlier_run(Some(at))
    )
}

/// The app space a workspace that matches only by its label was made from.
fn label_origin_note(at: Option<u64>, workspace: &str, space: &str) -> String {
    format!(
        "{} made a TUI workspace {workspace} from app space {space}, and this one matches \
         it only by its label: if it is another workspace, rename it to copy it",
        earlier_run(at)
    )
}

fn record_terminal(created: &mut AppCreated, made: &Made) {
    if let Some(terminal) = &made.terminal {
        created.terminals.push(terminal.clone());
    }
}

fn launch_of<'a>(launches: &'a BTreeMap<String, Launch>, pane: &str) -> Result<&'a Launch> {
    launches
        .get(pane)
        .with_context(|| format!("pane {pane} has no launch in the export"))
}

/// Ids on another machine carry its `<machine>/` prefix in both
/// `herdr-app do` and `herdr-app ls` (seen live: `canix/t7` with pane
/// `canix/p...` in both), so they compare as they are.
fn pane_of(app: &mut impl AppDo, terminal: &str) -> Result<String> {
    app.ls()?
        .spaces
        .iter()
        .flat_map(|space| &space.tabs)
        .flat_map(|tab| &tab.terminals)
        .find(|listed| listed.terminal == terminal)
        .and_then(|listed| listed.pane.clone())
        .with_context(|| format!("herdr-app ls does not list the pane of terminal {terminal}"))
}

/// Whether the app still lists a partial copy's space, or, without one, one
/// of its terminals.
fn partial_open(created: &AppCreated, present: &HashSet<&str>, listed: &HashSet<&str>) -> bool {
    match &created.space {
        Some(space) => present.contains(space.as_str()),
        None => created
            .terminals
            .iter()
            .any(|terminal| listed.contains(terminal.as_str())),
    }
}

/// A partial copy as the plan names it: its space, else what it made.
fn partial_name(created: &AppCreated) -> String {
    match &created.space {
        Some(space) => space.clone(),
        None => format!("({})", created_text(created)),
    }
}

fn partial_reason(created: &AppCreated) -> String {
    format!(
        "partial copy {} still open: close it first",
        partial_name(created)
    )
}

/// A partial copy whose `herdr-app do` timed out may have left a space no
/// listing can tie to it, so it is named until that source is copied.
fn unknown_partial_note(created: &AppCreated, at: u64) -> String {
    let known = created_text(created);
    let known = if known == "nothing" {
        String::new()
    } else {
        format!(" (at least {known})")
    };
    format!(
        "{} stopped before it knew what it made and may have left a space{known}: check the app and close it first",
        earlier_run(Some(at))
    )
}

/// A `herdr-app do` that timed out may still have made what it was asked.
fn unknown_text(created: &AppCreated) -> String {
    let known = created_text(created);
    if known == "nothing" {
        "unknown: check the app".into()
    } else {
        format!("unknown: check the app (at least {known})")
    }
}

/// `on_made` is told what exists after each step, so a run stopped part way
/// leaves the space it made named.
pub fn build_workspace(
    app: &mut impl AppDo,
    workspace: &WorkspaceLayout,
    launches: &BTreeMap<String, Launch>,
    machine: Option<&str>,
    created: &mut AppCreated,
    on_made: &mut dyn FnMut(&AppCreated) -> Result<()>,
) -> Result<()> {
    let mut space: Option<String> = None;
    for tab in &workspace.tabs {
        let launch = launch_of(launches, tab.root.first_pane_id())?;
        let mut args = json!({"folder": launch.cwd});
        // The app runs this argv as a job of its own `$SHELL -ic`, which
        // stays as the pane's shell, so only the program's login shell is
        // added (a split pane has no such shell and keeps one itself).
        if let Some(argv) = &launch.argv {
            args["argv"] = json!(platform::login_job(&launch.cwd, argv));
        }
        let (command, made) = match &space {
            None => {
                let name = tree::space_name(&workspace.label);
                if !name.is_empty() {
                    args["name"] = json!(name);
                }
                if let Some(tab_name) = tree::tab_name(&tab.label) {
                    args["tab"] = json!(tab_name);
                }
                if let Some(machine) = machine {
                    args["machine"] = json!(machine);
                }
                let made = app.run("space.new", args)?;
                record_terminal(created, &made);
                let id = made
                    .space
                    .clone()
                    .context("space.new did not report the space it made")?;
                created.space = Some(id.clone());
                on_made(created)?;
                space = Some(id);
                ("space.new", made)
            }
            Some(id) => {
                args["space"] = json!(id);
                if let Some(tab_name) = tree::tab_name(&tab.label) {
                    args["name"] = json!(tab_name);
                }
                let made = app.run("tab.new", args)?;
                record_terminal(created, &made);
                on_made(created)?;
                ("tab.new", made)
            }
        };
        let terminal = made
            .terminal
            .with_context(|| format!("{command} did not report the terminal it made"))?;
        let pane = pane_of(app, &terminal)?;
        split(app, &tab.root, &pane, launches, created, on_made)?;
    }
    Ok(())
}

fn split(
    app: &mut impl AppDo,
    node: &Node,
    pane: &str,
    launches: &BTreeMap<String, Launch>,
    created: &mut AppCreated,
    on_made: &mut dyn FnMut(&AppCreated) -> Result<()>,
) -> Result<()> {
    let Node::Split {
        direction,
        first,
        second,
        ..
    } = node
    else {
        return Ok(());
    };
    let launch = launch_of(launches, second.first_pane_id())?;
    let made = app.run(
        "pane.split",
        json!({"direction": direction, "pane": pane, "argv": tree::wrapped(launch)}),
    )?;
    record_terminal(created, &made);
    on_made(created)?;
    let new = made
        .pane
        .context("pane.split did not report the pane it made")?;
    split(app, first, pane, launches, created, on_made)?;
    split(app, second, &new, launches, created, on_made)
}

/// A copy into another machine's app: that machine, and how a copy is
/// recorded in its own ledger too (`remote::record_on`, with `true` for a
/// partial copy; a stand-in in tests).
pub struct OnMachine<'a> {
    pub name: &'a str,
    pub record: &'a mut dyn FnMut(&str, &Entry, bool) -> Result<()>,
}

/// What `apply` warns about without stopping: a copy into another machine's
/// app, whole or partial, that machine could not record. The copy stands,
/// and this ledger has it.
fn unrecorded(
    machine: &str,
    source: &str,
    entry: &Entry,
    partial: bool,
    error: &anyhow::Error,
) -> String {
    let made = match entry {
        Entry::ToApp { created, .. } => created.clone(),
        Entry::ToTui { .. } => AppCreated::default(),
    };
    let command = record_command(entry, partial);
    if partial {
        format!(
            "the copy of {source} on {machine} stopped part way, and {machine} did not record \
             what it made ({}; {error:#}); a migrate started on {machine} will not know it is a \
             partial copy of {source}; to record it, run on {machine}: {command}",
            created_text(&made)
        )
    } else {
        let space = made.space.as_deref().unwrap_or("?");
        format!(
            "copied {source} into space {space} on {machine}, but {machine} did not record it \
             ({error:#}); a migrate started on {machine} will not know that {space} is a copy of \
             {source}; to record it, run on {machine}: {command}"
        )
    }
}

/// The command that records `entry` on its machine by hand.
fn record_command(entry: &Entry, partial: bool) -> String {
    serde_json::to_string(entry)
        .map(|json| {
            let partial = if partial { " --partial" } else { "" };
            format!(
                "herdr-revive migrate --record {}{partial}",
                crate::core::hook::quote(&json)
            )
        })
        .unwrap_or_else(|_| "herdr-revive migrate --record ...".into())
}

/// What `apply` warns about when the machine it copies to could not record
/// the space a build has just made: the build goes on, and its end records
/// it again, but a run stopped before then leaves that machine without it.
fn unrecorded_in_progress(
    machine: &str,
    source: &str,
    entry: &Entry,
    error: &anyhow::Error,
) -> String {
    let space = match entry {
        Entry::ToApp { created, .. } => created.space.clone(),
        Entry::ToTui { .. } => None,
    };
    let space = space.as_deref().unwrap_or("?");
    format!(
        "{machine} did not record space {space}, the copy of {source} being built ({error:#}); \
         if this run stops before the copy ends, a migrate started on {machine} will not know \
         that {space} is a partial copy of {source}; to record it, run on {machine}: {}",
        record_command(entry, true)
    )
}

pub fn apply(
    items: &[Item],
    export: &Export,
    mut on: Option<&mut OnMachine>,
    ledger: &mut Ledger,
    app: &mut impl AppDo,
    report: &mut Report,
) -> Result<()> {
    let machine = on.as_ref().map(|on| on.name);
    for item in items
        .iter()
        .filter(|item| item.line.status == Status::Create)
    {
        let mut created = AppCreated::default();
        // Taken first so a clock error cannot hide what a failed build made.
        let at = ledger.fresh_at(&item.source, store::now_ms()?);
        // A run stopped part way (killed, interrupted, the dialog's time
        // limit) records nothing at its end, so this entry, under the run's
        // own `at`, is kept up to date as the build goes: first naming
        // nothing, then each step's space and terminals, which the next plan
        // finds open (and `--to tui` finds as a copy). The build's end
        // replaces it. `unknown`: the step after the last one recorded may
        // have made more. Another machine M is told once, as soon as the
        // space exists (an `hc exec` per step would slow every build), so
        // M's own plans, and a third machine's through M's export, know the
        // space even when this run is killed; the build's end records the
        // whole copy there. A run killed before M is told, or after this
        // ledger records the whole copy but before M does, leaves M without
        // it or with only the partial.
        let source = item.source.clone();
        let mut told = false;
        let warnings = &mut report.warnings;
        let mut progress = |made: &AppCreated| {
            let pending = AppCreated {
                unknown: true,
                ..made.clone()
            };
            ledger.record_partial(Entry::ToApp {
                source: source.clone(),
                created: pending.clone(),
                at,
            })?;
            if !told
                && pending.space.is_some()
                && let Some(on) = on.as_deref_mut()
            {
                told = true;
                let theirs = super::remote::entry_for_machine(on.name, &source, &pending, at);
                if let Err(error) = (on.record)(on.name, &theirs, true) {
                    let warning =
                        unrecorded_in_progress(on.name, &item.line.source, &theirs, &error);
                    warnings.push(warning);
                }
            }
            Ok(())
        };
        progress(&AppCreated::default())?;
        let built = build_workspace(
            app,
            &item.workspace,
            &export.launches,
            machine,
            &mut created,
            &mut progress,
        );
        if let Err(error) = built {
            created.unknown = external::timed_out(&error);
            let made = if created.unknown {
                unknown_text(&created)
            } else {
                created_text(&created)
            };
            let mut error = error.context(format!(
                "copy of {} stopped; created so far: {made}; close those before migrating {} again",
                item.line.source, item.line.source
            ));
            // A copy that made nothing leaves nothing to close, and would
            // only hide an earlier partial copy (a timed-out one may have
            // made a space, so it is kept).
            if names_nothing(&created) && !created.unknown {
                if let Err(note) = ledger.forget_partial(&item.source, at) {
                    return Err(
                        error.context(format!("its pending record could not be removed: {note:#}"))
                    );
                }
                return Err(error);
            }
            if let Err(note) = ledger.record_partial(Entry::ToApp {
                source: item.source.clone(),
                created: created.clone(),
                at,
            }) {
                error = error.context(format!("the partial copy was not recorded: {note:#}"));
            }
            // M keeps the partial copy too, so its own plans name it and do
            // not copy that space back.
            if let Some(on) = on.as_deref_mut() {
                let theirs = super::remote::entry_for_machine(on.name, &item.source, &created, at);
                if let Err(note) = (on.record)(on.name, &theirs, true) {
                    let warning = unrecorded(on.name, &item.line.source, &theirs, true, &note);
                    report.warnings.push(warning);
                }
            }
            return Err(error);
        }
        ledger
            .record(Entry::ToApp {
                source: item.source.clone(),
                created: created.clone(),
                at,
            })
            .with_context(|| {
                format!(
                    "copied {} but could not record it; it made {}",
                    item.line.source,
                    created_text(&created)
                )
            })?;
        // The record of the build in progress names the copy's own space.
        ledger.forget_partial(&item.source, at).with_context(|| {
            format!(
                "copied and recorded {}, but its record in progress stays: it names {}",
                item.line.source,
                created_text(&created)
            )
        })?;
        // An earlier run's partial copy that names nothing and is this
        // workspace is settled by this copy (`ledger.record` drops only one
        // it can tell is the same workspace without the live session).
        for (source, at) in &item.stale {
            ledger.forget_partial(source, *at).with_context(|| {
                format!(
                    "copied and recorded {}, but an earlier partial copy of it stays",
                    item.line.source
                )
            })?;
        }
        report.record(Applied {
            source: item.line.source.clone(),
            created: serde_json::to_value(&created)?,
        });
        if let Some(on) = on.as_deref_mut() {
            let theirs = super::remote::entry_for_machine(on.name, &item.source, &created, at);
            if let Err(error) = (on.record)(on.name, &theirs, false) {
                let warning = unrecorded(on.name, &item.line.source, &theirs, false, &error);
                report.warnings.push(warning);
            }
        }
    }
    Ok(())
}

/// `ledger` was opened before `ls` was read: the plan decides from both,
/// and another run must not record a copy between the two reads.
#[allow(clippy::too_many_arguments)] // the ledger lock travels with the listing it covers
pub fn run(
    request: &Request,
    mut ledger: Ledger,
    ls: &AppLs,
    export: &Export,
    input: &mut dyn BufRead,
    output: &mut dyn Write,
    errors: &mut dyn Write,
    tty: bool,
) -> Result<i32> {
    let machine = request.machine.as_deref();
    let items = plan(export, machine, &ledger, ls)?;
    let mut report = Report::new(
        Side::App,
        machine,
        items.iter().map(|item| item.line.clone()).collect(),
    );
    report.warnings = export.warnings.clone();
    let mut record = super::remote::record_on;
    let mut on = machine.map(|name| OnMachine {
        name,
        record: &mut record,
    });
    drive(
        report,
        request.confirm,
        request.json,
        input,
        output,
        errors,
        tty,
        |report| {
            // Partial copies the user has closed since are forgotten.
            // Only this run's machine: another machine's spaces may be
            // missing from the listing only because that machine is offline.
            let (present, listed) = listing(ls);
            ledger.prune_partials(machine, |created| partial_open(created, &present, &listed))?;
            apply(
                &items,
                export,
                on.as_mut(),
                &mut ledger,
                &mut super::external::AppCli,
                report,
            )
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::{Direction, TabLayout};
    use crate::migrate::external::{AppSpace, AppTab, AppTerminal};
    use crate::migrate::tree::CD_SHELL;
    use crate::model::*;
    use crate::platform::{LOGIN_JOB, LOGIN_PANE};
    use anyhow::bail;
    use serde_json::{Value, json};

    const UUID: &str = "123e4567-e89b-12d3-a456-426614174000";

    /// Answers the way `herdr-app` does: `space.new` and `tab.new` report
    /// no pane, only `pane.split` does, and `ls` maps every terminal made so
    /// far to its pane. On another machine every id carries `<machine>/`.
    struct FakeApp {
        calls: Vec<(String, Value)>,
        fail_at: Option<usize>,
        time_out_at: Option<usize>,
        prefix: String,
        listed: Vec<AppTerminal>,
        list_terminals: bool,
        space_new_reports_space: bool,
        ls_calls: usize,
    }

    impl FakeApp {
        fn new(fail_at: Option<usize>) -> Self {
            Self {
                calls: vec![],
                fail_at,
                time_out_at: None,
                prefix: String::new(),
                listed: vec![],
                list_terminals: true,
                space_new_reports_space: true,
                ls_calls: 0,
            }
        }
    }

    impl AppDo for FakeApp {
        fn run(&mut self, command: &str, args: Value) -> Result<Made> {
            self.calls.push((command.into(), args.clone()));
            let n = self.calls.len();
            if self.fail_at == Some(n) {
                bail!("herdr-app do {command} failed");
            }
            if self.time_out_at == Some(n) {
                return Err(anyhow::Error::new(external::TimedOut::new(
                    "herdr-app",
                    std::time::Duration::from_secs(30),
                ))
                .context(format!("herdr-app do {command}")));
            }
            let id = |kind: &str| format!("{}{kind}{n}", self.prefix);
            let terminal = id("t");
            self.listed.push(AppTerminal {
                terminal: terminal.clone(),
                pane: Some(id("p")),
            });
            Ok(match command {
                "space.new" => Made {
                    space: self.space_new_reports_space.then(|| id("s")),
                    tab: Some(id("tb")),
                    pane: None,
                    terminal: Some(terminal),
                },
                "tab.new" => Made {
                    space: args["space"].as_str().map(String::from),
                    tab: Some(id("tb")),
                    pane: None,
                    terminal: Some(terminal),
                },
                "pane.split" => Made {
                    space: None,
                    tab: None,
                    pane: Some(id("p")),
                    terminal: Some(terminal),
                },
                other => bail!("unexpected command {other}"),
            })
        }

        fn ls(&mut self) -> Result<AppLs> {
            self.ls_calls += 1;
            let terminals = if self.list_terminals {
                self.listed.clone()
            } else {
                vec![]
            };
            Ok(AppLs {
                spaces: vec![AppSpace {
                    id: format!("{}s1", self.prefix),
                    name: String::new(),
                    folder: String::new(),
                    machine: None,
                    tabs: vec![AppTab {
                        id: format!("{}tb1", self.prefix),
                        name: String::new(),
                        terminals,
                    }],
                }],
            })
        }
    }

    fn leaf(id: &str) -> Node {
        Node::Pane {
            pane_id: id.into(),
            label: None,
        }
    }

    /// `[1] work`: tab `build` = p1 | (p2 / p4), tab `2` = p3.
    fn export() -> Export {
        let pane = |tab: &str, id: &str, cwd: &str, command: Option<CommandSpec>| SavedPane {
            workspace_id: "w1".into(),
            tab_id: tab.into(),
            pane_id: id.into(),
            cwd: cwd.into(),
            command,
        };
        let claude = CommandSpec::Agent {
            executable: "claude".into(),
            agent: AgentKind::Claude,
            session_id: UUID.into(),
            session_mode: AgentSessionMode::Resume,
            claude_config_dir: None,
            claude_home: None,
            null_stdio: [false; 3],
        };
        let ssh = CommandSpec::Program {
            argv: vec!["ssh".into(), "host".into()],
        };
        let snapshot = Snapshot {
            tool: TOOL.into(),
            schema: SCHEMA,
            session: "a".repeat(64),
            created_ms: 1,
            scope: Scope::Session,
            panes: vec![
                pane("w1:t1", "w1:p1", "/w", None),
                pane("w1:t1", "w1:p2", "/w/b c", Some(claude)),
                pane("w1:t2", "w1:p3", "/w", Some(ssh)),
                pane("w1:t1", "w1:p4", "/w/d", None),
            ],
            layout: vec![WorkspaceLayout {
                workspace_id: "w1".into(),
                label: "[1] work".into(),
                active_tab_id: None,
                tabs: vec![
                    TabLayout {
                        tab_id: "w1:t1".into(),
                        label: "build".into(),
                        focused_pane_id: "w1:p1".into(),
                        root: Node::Split {
                            direction: Direction::Right,
                            ratio: 0.5,
                            first: Box::new(leaf("w1:p1")),
                            second: Box::new(Node::Split {
                                direction: Direction::Down,
                                ratio: 0.3,
                                first: Box::new(leaf("w1:p2")),
                                second: Box::new(leaf("w1:p4")),
                            }),
                        },
                    },
                    TabLayout {
                        tab_id: "w1:t2".into(),
                        label: "2".into(),
                        focused_pane_id: "w1:p3".into(),
                        root: leaf("w1:p3"),
                    },
                ],
            }],
            focused_workspace_id: None,
        };
        snapshot.validate().unwrap();
        let launch = |cwd: &str, argv: Option<Vec<&str>>| Launch {
            cwd: cwd.into(),
            argv: argv.map(|a| a.into_iter().map(String::from).collect()),
        };
        Export {
            tool: TOOL.into(),
            schema: SCHEMA,
            session: "a".repeat(64),
            snapshot,
            launches: BTreeMap::from([
                ("w1:p1".to_string(), launch("/w", None)),
                (
                    "w1:p2".to_string(),
                    launch("/w/b c", Some(vec!["claude", "--resume", UUID])),
                ),
                ("w1:p3".to_string(), launch("/w", Some(vec!["ssh", "host"]))),
                ("w1:p4".to_string(), launch("/w/d", None)),
            ]),
            live_agents: vec!["w1:p2".into()],
            denied: vec![],
            tui_copies: vec![],
            terminal_ids: BTreeMap::from([(
                "w1".to_string(),
                vec!["term_fixture_1".to_string(), "term_fixture_2".to_string()],
            )]),
            missing_folders: BTreeMap::new(),
            app_copies: vec![],
            partial_copies: vec![],
            uncaptured: vec![],
            warnings: vec![],
            live_workspaces: vec![],
        }
    }

    #[test]
    fn a_two_tab_workspace_is_built_in_this_exact_order() {
        let export = export();
        let mut app = FakeApp::new(None);
        let mut created = AppCreated::default();
        build_workspace(
            &mut app,
            &export.snapshot.layout[0],
            &export.launches,
            None,
            &mut created,
            &mut |_| Ok(()),
        )
        .unwrap();
        assert_eq!(
            app.calls,
            vec![
                (
                    "space.new".into(),
                    json!({"folder": "/w", "name": "work", "tab": "build"})
                ),
                (
                    "pane.split".into(),
                    json!({"direction": "right", "pane": "p1",
                           "argv": ["sh", "-c", LOGIN_PANE, "/w/b c", "claude", "--resume", UUID]})
                ),
                (
                    "pane.split".into(),
                    json!({"direction": "down", "pane": "p2",
                           "argv": ["sh", "-c", CD_SHELL, "/w/d"]})
                ),
                (
                    "tab.new".into(),
                    json!({"folder": "/w", "argv": ["sh", "-c", LOGIN_JOB, "/w", "ssh", "host"], "space": "s1"})
                ),
            ]
        );
        assert_eq!(app.ls_calls, 2, "one lookup per space.new and tab.new");
        assert_eq!(
            created,
            AppCreated {
                space: Some("s1".into()),
                terminals: vec!["t1".into(), "t2".into(), "t3".into(), "t4".into()],
                unknown: false,
            }
        );
    }

    #[test]
    fn the_machine_goes_only_to_space_new_and_its_prefixed_ids_are_reused() {
        let export = export();
        let mut app = FakeApp::new(None);
        app.prefix = "canix/".into();
        let mut created = AppCreated::default();
        build_workspace(
            &mut app,
            &export.snapshot.layout[0],
            &export.launches,
            Some("canix"),
            &mut created,
            &mut |_| Ok(()),
        )
        .unwrap();
        assert_eq!(app.calls[0].1["machine"], "canix");
        assert!(
            app.calls[1..]
                .iter()
                .all(|(_, args)| args.get("machine").is_none())
        );
        assert_eq!(app.calls[1].1["pane"], "canix/p1");
        assert_eq!(app.calls[2].1["pane"], "canix/p2");
        assert_eq!(app.calls[3].1["space"], "canix/s1");
        assert_eq!(created.space.as_deref(), Some("canix/s1"));
    }

    #[test]
    fn each_step_of_a_build_is_reported_as_it_is_made() {
        let export = export();
        let mut app = FakeApp::new(None);
        let mut created = AppCreated::default();
        let mut steps: Vec<AppCreated> = Vec::new();
        build_workspace(
            &mut app,
            &export.snapshot.layout[0],
            &export.launches,
            None,
            &mut created,
            &mut |made| {
                steps.push(made.clone());
                Ok(())
            },
        )
        .unwrap();
        let named: Vec<(Option<&str>, usize)> = steps
            .iter()
            .map(|step| (step.space.as_deref(), step.terminals.len()))
            .collect();
        // space.new, two splits of the first tab, tab.new.
        assert_eq!(
            named,
            [
                (Some("s1"), 1),
                (Some("s1"), 2),
                (Some("s1"), 3),
                (Some("s1"), 4)
            ]
        );
        assert_eq!(steps.last(), Some(&created));
    }

    #[test]
    fn a_terminal_the_listing_does_not_show_stops_the_build() {
        let export = export();
        let mut app = FakeApp::new(None);
        app.list_terminals = false;
        let mut created = AppCreated::default();
        let error = build_workspace(
            &mut app,
            &export.snapshot.layout[0],
            &export.launches,
            None,
            &mut created,
            &mut |_| Ok(()),
        )
        .err()
        .unwrap();
        let text = format!("{error:#}");
        assert!(text.contains("terminal t1"), "{text}");
        assert_eq!(app.calls.len(), 1);
        assert_eq!(
            created,
            AppCreated {
                space: Some("s1".into()),
                terminals: vec!["t1".into()],
                unknown: false,
            }
        );
    }

    fn source() -> AppSource {
        AppSource {
            machine: None,
            session: "a".repeat(64),
            workspace_id: "w1".into(),
            terminal_ids: Some(vec!["term_fixture_1".into(), "term_fixture_2".into()]),
            label: Some("work".into()),
        }
    }

    fn ls(spaces: &[&str]) -> AppLs {
        serde_json::from_value(json!({"spaces": spaces.iter()
            .map(|id| json!({"id": id, "name": "", "tabs": []})).collect::<Vec<_>>()}))
        .unwrap()
    }

    #[test]
    fn the_plan_counts_and_follows_the_ledger() {
        let root = tempfile::tempdir().unwrap();
        let mut ledger = Ledger::open(root.path()).unwrap();
        let items = plan(&export(), None, &ledger, &ls(&[])).unwrap();
        let line = &items[0].line;
        assert_eq!(line.status, Status::Create);
        assert_eq!(
            (line.name.as_str(), line.tabs, line.panes, line.agents),
            ("work", 2, 4, 1)
        );
        assert_eq!(line.live_agents, ["w1:p2"]);
        ledger
            .record(Entry::ToApp {
                source: source(),
                created: AppCreated {
                    space: Some("s7".into()),
                    terminals: vec![],
                    unknown: false,
                },
                at: 1,
            })
            .unwrap();
        let items = plan(&export(), None, &ledger, &ls(&["s7"])).unwrap();
        assert_eq!(items[0].line.status, Status::Unchanged);
        assert_eq!(items[0].line.copy.as_deref(), Some("s7"));
        let items = plan(&export(), None, &ledger, &ls(&["s8"])).unwrap();
        assert_eq!(items[0].line.status, Status::Create);
    }

    #[test]
    fn a_pane_whose_folder_is_gone_is_named_and_not_counted_as_an_agent() {
        let root = tempfile::tempdir().unwrap();
        let ledger = Ledger::open(root.path()).unwrap();
        let mut export = export();
        // The exporting machine found the claude pane's folder gone and gave
        // it a shell in its home folder.
        export
            .missing_folders
            .insert("w1:p2".into(), "/w/b c".into());
        let items = plan(&export, None, &ledger, &ls(&[])).unwrap();
        let line = &items[0].line;
        assert_eq!(line.status, Status::Create);
        assert_eq!((line.panes, line.agents), (4, 0));
        assert_eq!(line.notes, [missing_note(&["/w/b c".into()])]);
        let mut done = export.clone();
        done.missing_folders.clear();
        assert!(
            plan(&done, None, &ledger, &ls(&[])).unwrap()[0]
                .line
                .notes
                .is_empty()
        );
    }

    #[test]
    fn an_agent_the_policy_denies_is_not_counted_or_named_as_live() {
        let root = tempfile::tempdir().unwrap();
        let ledger = Ledger::open(root.path()).unwrap();
        // w1:p2 runs claude and is live in the TUI; its policy denies it.
        let mut export = export();
        export.denied = vec!["w1:p2".into()];
        let line = &plan(&export, None, &ledger, &ls(&[])).unwrap()[0].line;
        assert_eq!((line.agents, line.denied), (0, 1));
        assert_eq!(line.live_agents, Vec::<String>::new());
        let text = crate::migrate::describe(line);
        assert!(!text.contains("still live"), "{text}");
        export.denied.clear();
        let allowed = &plan(&export, None, &ledger, &ls(&[])).unwrap()[0].line;
        assert_eq!((allowed.agents, allowed.live_agents.len()), (1, 1));
    }

    #[test]
    fn a_workspace_the_capture_could_not_read_is_a_skip_line() {
        let root = tempfile::tempdir().unwrap();
        let ledger = Ledger::open(root.path()).unwrap();
        let mut export = export();
        export.uncaptured = vec![crate::migrate::tui::Uncaptured {
            workspace_id: "w9".into(),
            label: "[2] api".into(),
            reason: "pane w9:p1: agent session metadata did not arrive".into(),
        }];
        let items = plan(&export, Some("canix"), &ledger, &ls(&[])).unwrap();
        let lines: Vec<_> = items
            .iter()
            .map(|item| {
                (
                    item.line.source.as_str(),
                    item.line.status,
                    item.line.reason.as_deref(),
                )
            })
            .collect();
        assert_eq!(
            lines,
            [
                ("w1", Status::Create, None),
                (
                    "w9",
                    Status::Skip,
                    Some("cannot capture: pane w9:p1: agent session metadata did not arrive")
                )
            ]
        );
        assert_eq!(items[1].line.name, "api");
        assert_eq!(items[1].line.machine.as_deref(), Some("canix"));
    }

    #[test]
    fn a_failed_create_stops_names_what_it_made_and_records_a_partial() {
        let root = tempfile::tempdir().unwrap();
        let mut ledger = Ledger::open(root.path()).unwrap();
        let export = export();
        let items = plan(&export, None, &ledger, &ls(&[])).unwrap();
        let mut report = Report::new(
            Side::App,
            None,
            items.iter().map(|i| i.line.clone()).collect(),
        );
        let mut app = FakeApp::new(Some(3));
        let error = apply(&items, &export, None, &mut ledger, &mut app, &mut report)
            .err()
            .unwrap();
        let text = format!("{error:#}");
        assert!(text.contains("space s1, terminals t1 t2"), "{text}");
        assert_eq!(report.applied, Some(vec![]));
        assert_eq!(ledger.copied_to_app(&source()), None);
        assert_eq!(
            ledger
                .partial_to_app(&source())
                .unwrap()
                .created
                .space
                .as_deref(),
            Some("s1")
        );
        let items = plan(&export, None, &ledger, &ls(&["s1"])).unwrap();
        assert!(items[0].line.partial.is_some());
        let items = plan(&export, None, &ledger, &ls(&[])).unwrap();
        assert_eq!(items[0].line.partial, None, "the user closed it");
    }

    #[test]
    fn a_partial_copy_into_a_machines_app_is_recorded_there_as_partial() {
        let export = export();
        let mine = tempfile::tempdir().unwrap();
        let theirs = tempfile::tempdir().unwrap();
        let mut ledger = Ledger::open(mine.path()).unwrap();
        let items = plan(&export, Some("canix"), &ledger, &ls(&[])).unwrap();
        let mut report = Report::new(Side::App, Some("canix"), vec![]);
        let mut app = FakeApp::new(Some(3));
        app.prefix = "canix/".into();
        let mut record = |_: &str, entry: &Entry, partial: bool| -> Result<()> {
            assert!(partial, "only the partial copy is recorded");
            let args = crate::migrate::remote::record_args(entry, partial)?;
            crate::migrate::remote::record_here(&args[2], partial, theirs.path())
        };
        let mut on = OnMachine {
            name: "canix",
            record: &mut record,
        };
        apply(
            &items,
            &export,
            Some(&mut on),
            &mut ledger,
            &mut app,
            &mut report,
        )
        .unwrap_err();
        assert_eq!(report.warnings, Vec::<String>::new());
        drop(ledger);
        // canix's own plans treat it as a partial copy made there: its
        // `--to app` names it, and its `--to tui` does not copy s1 back.
        let there = Ledger::open(theirs.path()).unwrap();
        let mut here = source();
        here.terminal_ids = Some(vec!["term_fixture_1".into(), "term_fixture_2".into()]);
        assert_eq!(there.copied_to_app(&here), None);
        let partial = there.partial_to_app(&here).unwrap();
        assert_eq!(partial.created.space.as_deref(), Some("s1"));
        assert_eq!(partial.created.terminals, ["t1", "t2"]);
        let items = plan(&export, None, &there, &ls(&["s1"])).unwrap();
        assert!(items[0].line.partial.is_some());
        assert!(there.app_space_origin("s1").is_some());
    }

    #[test]
    fn a_partial_copy_the_machine_cannot_record_is_named_in_a_warning() {
        let export = export();
        let root = tempfile::tempdir().unwrap();
        let mut ledger = Ledger::open(root.path()).unwrap();
        let items = plan(&export, Some("canix"), &ledger, &ls(&[])).unwrap();
        let mut report = Report::new(Side::App, Some("canix"), vec![]);
        let mut app = FakeApp::new(Some(3));
        app.prefix = "canix/".into();
        let mut record = |_: &str, _: &Entry, _: bool| -> Result<()> { bail!("lock held") };
        let mut on = OnMachine {
            name: "canix",
            record: &mut record,
        };
        apply(
            &items,
            &export,
            Some(&mut on),
            &mut ledger,
            &mut app,
            &mut report,
        )
        .unwrap_err();
        assert_eq!(report.warnings.len(), 2, "the space as made, then the stop");
        assert!(report.warnings[0].starts_with("canix did not record space s1, the copy of w1"));
        report.warnings.remove(0);
        assert!(
            report.warnings[0].starts_with(
                "the copy of w1 on canix stopped part way, and canix did not record what it \
                 made (space s1, terminals t1 t2; lock held); a migrate started on canix will \
                 not know it is a partial copy of w1; to record it, run on canix: herdr-revive \
                 migrate --record '{"
            ),
            "{}",
            report.warnings[0]
        );
        assert!(report.warnings[0].ends_with("' --partial"));
    }

    #[test]
    fn a_partial_copy_left_open_stays_known_through_a_retry_and_a_full_copy() {
        let root = tempfile::tempdir().unwrap();
        let mut ledger = Ledger::open(root.path()).unwrap();
        let export = export();
        let items = plan(&export, None, &ledger, &ls(&[])).unwrap();
        let mut report = Report::new(Side::App, None, vec![]);
        // 1. The copy stops part way and leaves s1.
        apply(
            &items,
            &export,
            None,
            &mut ledger,
            &mut FakeApp::new(Some(3)),
            &mut report,
        )
        .unwrap_err();
        let skipped = plan(&export, None, &ledger, &ls(&["s1"])).unwrap();
        assert_eq!(
            (skipped[0].line.status, skipped[0].line.reason.as_deref()),
            (
                Status::Skip,
                Some("partial copy s1 still open: close it first")
            ),
            "neither the CLI nor the dialog's Copy runs it again"
        );
        assert_eq!(
            skipped[0]
                .line
                .partial
                .as_ref()
                .and_then(|p| p.space.as_deref()),
            Some("s1")
        );
        // 2. A retry anyway (s1 not listed to its plan) makes nothing: s1 stays known.
        apply(
            &items,
            &export,
            None,
            &mut ledger,
            &mut FakeApp::new(Some(1)),
            &mut report,
        )
        .unwrap_err();
        assert_eq!(ledger.partials_to_app(&source()).len(), 1);
        let skipped = plan(&export, None, &ledger, &ls(&["s1"])).unwrap();
        assert_eq!(skipped[0].line.status, Status::Skip);
        // 3. A full copy (full-s1): s1 stays known to the next plan and to
        // `--to tui`.
        let mut full = FakeApp::new(None);
        full.prefix = "full-".into();
        apply(&items, &export, None, &mut ledger, &mut full, &mut report).unwrap();
        assert!(ledger.app_space_origin("s1").is_some());
        assert!(ledger.app_space_origin("full-s1").is_some());
        assert!(
            ledger
                .partials_to_app(&source())
                .iter()
                .all(|partial| partial.created.space.as_deref() == Some("s1")),
            "the record of the full copy's build in progress is gone"
        );
        let both = plan(&export, None, &ledger, &ls(&["s1", "full-s1"])).unwrap();
        assert_eq!(
            both[0].line.status,
            Status::Unchanged,
            "full-s1 is the copy"
        );
        let s1_only = plan(&export, None, &ledger, &ls(&["s1"])).unwrap();
        assert_eq!(
            s1_only[0].line.status,
            Status::Skip,
            "full-s1 closed, s1 still open"
        );
        // Closed, s1 is forgotten at the next apply.
        ledger.prune_partials(None, |_| false).unwrap();
        assert!(ledger.app_space_origin("s1").is_none());
        let items = plan(&export, None, &ledger, &ls(&[])).unwrap();
        assert_eq!(items[0].line.status, Status::Create);
    }

    #[test]
    fn an_open_partial_copy_of_another_workspace_that_had_the_id_is_named_not_obeyed() {
        let root = tempfile::tempdir().unwrap();
        let mut ledger = Ledger::open(root.path()).unwrap();
        let export = export();
        let items = plan(&export, None, &ledger, &ls(&[])).unwrap();
        let mut report = Report::new(Side::App, None, vec![]);
        apply(
            &items,
            &export,
            None,
            &mut ledger,
            &mut FakeApp::new(Some(3)),
            &mut report,
        )
        .unwrap_err();
        // After a TUI restart, w1 is another workspace.
        let mut other = export.clone();
        other.snapshot.layout[0].label = "[1] elsewhere".into();
        other
            .terminal_ids
            .insert("w1".into(), vec!["term_new".into()]);
        let line = &plan(&other, None, &ledger, &ls(&["s1"])).unwrap()[0].line;
        assert_eq!(line.status, Status::Create);
        assert_eq!(
            line.notes,
            [
                "an earlier partial copy s1 may be from another workspace that had this id: if \
              it is the same, close it first"
            ]
        );
        // The same workspace restored (new terminals, same label) waits, and
        // says that only its label ties it.
        let mut restored = export.clone();
        restored
            .terminal_ids
            .insert("w1".into(), vec!["term_new".into()]);
        let line = &plan(&restored, None, &ledger, &ls(&["s1"])).unwrap()[0].line;
        assert_eq!(line.status, Status::Skip);
        assert_eq!(
            line.notes,
            [
                "the partial copy s1 matches this workspace only by its label: if it is another \
              workspace, close it or rename this one to copy it"
            ]
        );
        let line = &plan(&export, None, &ledger, &ls(&["s1"])).unwrap()[0].line;
        assert_eq!((line.status, line.notes.len()), (Status::Skip, 0));
    }

    #[test]
    fn a_run_prunes_only_its_own_machines_partial_copies() {
        let root = tempfile::tempdir().unwrap();
        let mut ledger = Ledger::open(root.path()).unwrap();
        let mut far = source();
        far.machine = Some("canix".into());
        for (source, space) in [(source(), "s1"), (far.clone(), "canix/s1")] {
            ledger
                .record_partial(Entry::ToApp {
                    source,
                    created: AppCreated {
                        space: Some(space.into()),
                        terminals: vec![],
                        unknown: false,
                    },
                    at: 1,
                })
                .unwrap();
        }
        // A local run while canix is offline: canix/s1 is not listed, and stays.
        ledger.prune_partials(None, |_| false).unwrap();
        assert!(ledger.partials_to_app(&source()).is_empty());
        assert_eq!(ledger.partials_to_app(&far).len(), 1);
        let line = &plan(&export(), Some("canix"), &ledger, &ls(&["canix/s1"])).unwrap()[0].line;
        assert_eq!(
            line.reason.as_deref(),
            Some("partial copy canix/s1 still open: close it first")
        );
        ledger.prune_partials(Some("canix"), |_| false).unwrap();
        assert!(ledger.partials_to_app(&far).is_empty());
    }

    #[test]
    fn a_third_machine_skips_a_source_whose_partial_copy_on_the_machine_is_open() {
        // canix's ledger has a partial copy of its w1 made from elsewhere;
        // canix's export ships it.
        let mut export = export();
        let entries = [Entry::ToApp {
            source: source(),
            created: AppCreated {
                space: Some("s1".into()),
                terminals: vec!["t1".into()],
                unknown: false,
            },
            at: 4,
        }];
        export.partial_copies = crate::migrate::tui::partial_copies(&entries, &export);
        assert_eq!(export.partial_copies.len(), 1);
        let third = Ledger::open(tempfile::tempdir().unwrap().path()).unwrap();
        let line = &plan(&export, Some("canix"), &third, &ls(&["canix/s1"])).unwrap()[0].line;
        assert_eq!(
            (line.status, line.reason.as_deref()),
            (
                Status::Skip,
                Some("partial copy canix/s1 still open: close it first")
            )
        );
        assert_eq!(
            line.partial.as_ref().map(|p| p.terminals.clone()),
            Some(vec!["canix/t1".to_string()])
        );
        let line = &plan(&export, Some("canix"), &third, &ls(&[])).unwrap()[0].line;
        assert_eq!(line.status, Status::Create, "closed");
        let line = &plan(&export, None, &third, &ls(&["s1"])).unwrap()[0].line;
        assert_eq!(
            line.status,
            Status::Create,
            "an export's partials are for --machine"
        );
    }

    #[test]
    fn a_partial_copy_that_made_nothing_is_not_recorded_here_or_on_the_machine() {
        let root = tempfile::tempdir().unwrap();
        let mut ledger = Ledger::open(root.path()).unwrap();
        let export = export();
        let items = plan(&export, Some("canix"), &ledger, &ls(&[])).unwrap();
        let mut report = Report::new(Side::App, Some("canix"), vec![]);
        let mut calls = 0;
        let mut record = |_: &str, _: &Entry, _: bool| -> Result<()> {
            calls += 1;
            Ok(())
        };
        let mut on = OnMachine {
            name: "canix",
            record: &mut record,
        };
        apply(
            &items,
            &export,
            Some(&mut on),
            &mut ledger,
            &mut FakeApp::new(Some(1)),
            &mut report,
        )
        .unwrap_err();
        assert_eq!(calls, 0, "no hc exec for nothing");
        let mut source = source();
        source.machine = Some("canix".into());
        assert!(ledger.partials_to_app(&source).is_empty());
    }

    fn ls_with_terminal(terminal: &str) -> AppLs {
        serde_json::from_value(json!({"spaces": [{"id": "s5", "tabs": [{"id": "tb5",
            "terminals": [{"terminal": terminal, "pane": "p5"}]}]}]}))
        .unwrap()
    }

    #[test]
    fn a_partial_copy_without_a_space_is_named_while_a_terminal_of_it_is_listed() {
        let root = tempfile::tempdir().unwrap();
        let mut ledger = Ledger::open(root.path()).unwrap();
        ledger
            .record_partial(Entry::ToApp {
                source: source(),
                created: AppCreated {
                    space: None,
                    terminals: vec!["t1".into()],
                    unknown: false,
                },
                at: 1,
            })
            .unwrap();
        let items = plan(&export(), None, &ledger, &ls_with_terminal("t1")).unwrap();
        assert!(items[0].line.partial.is_some());
        let items = plan(&export(), None, &ledger, &ls_with_terminal("t2")).unwrap();
        assert_eq!(items[0].line.partial, None);
    }

    #[test]
    fn a_timed_out_create_says_what_it_made_is_unknown() {
        let root = tempfile::tempdir().unwrap();
        let mut ledger = Ledger::open(root.path()).unwrap();
        let export = export();
        let items = plan(&export, None, &ledger, &ls(&[])).unwrap();
        let mut report = Report::new(Side::App, None, vec![]);
        let mut app = FakeApp::new(None);
        app.time_out_at = Some(1);
        let error = apply(&items, &export, None, &mut ledger, &mut app, &mut report)
            .err()
            .unwrap();
        let text = format!("{error:#}");
        assert!(
            text.contains("created so far: unknown: check the app;"),
            "{text}"
        );
        let mut app = FakeApp::new(None);
        app.time_out_at = Some(3);
        let error = apply(&items, &export, None, &mut ledger, &mut app, &mut report)
            .err()
            .unwrap();
        let text = format!("{error:#}");
        assert!(
            text.contains(
                "created so far: unknown: check the app (at least space s1, terminals t1 t2);"
            ),
            "{text}"
        );
    }

    fn copied_back(ledger: &mut Ledger, session: String) {
        ledger
            .record(Entry::ToTui {
                source: crate::migrate::ledger::TuiSource { space: "s9".into() },
                created: crate::migrate::ledger::TuiCreated {
                    session,
                    workspace_id: "w1".into(),
                    terminal_ids: Some(vec!["term_fixture_2".into()]),
                    label: Some("work".into()),
                },
                at: 1,
            })
            .unwrap();
    }

    #[test]
    fn a_workspace_copied_from_a_live_app_space_is_not_copied_back() {
        let root = tempfile::tempdir().unwrap();
        let mut ledger = Ledger::open(root.path()).unwrap();
        copied_back(&mut ledger, "a".repeat(64));
        let items = plan(&export(), None, &ledger, &ls(&["s9"])).unwrap();
        assert_eq!(items[0].line.status, Status::Skip);
        assert_eq!(
            items[0].line.reason.as_deref(),
            Some("copy of app space s9")
        );
        let gone = plan(&export(), None, &ledger, &ls(&[])).unwrap();
        assert_eq!(gone[0].line.status, Status::Create, "its space was closed");
        let remote = plan(&export(), Some("canix"), &ledger, &ls(&["s9"])).unwrap();
        assert_eq!(
            remote[0].line.status,
            Status::Create,
            "another machine's TUI"
        );
        let root = tempfile::tempdir().unwrap();
        let mut ledger = Ledger::open(root.path()).unwrap();
        copied_back(&mut ledger, "c".repeat(64));
        let other = plan(&export(), None, &ledger, &ls(&["s9"])).unwrap();
        assert_eq!(other[0].line.status, Status::Create, "another TUI session");
    }

    #[test]
    fn a_workspace_another_machine_copied_from_its_live_app_space_is_not_copied_back() {
        let root = tempfile::tempdir().unwrap();
        let ledger = Ledger::open(root.path()).unwrap();
        let copy = |session: String, workspace: &str| crate::migrate::tui::TuiCopy {
            space: "sp7".into(),
            session,
            workspace_id: workspace.into(),
            terminal_ids: Some(vec!["term_fixture_1".into()]),
            label: Some("work".into()),
            at: Some(1),
        };
        let mut export = export();
        export.tui_copies = vec![copy("a".repeat(64), "w1")];
        let items = plan(&export, Some("canix"), &ledger, &ls(&["canix/sp7"])).unwrap();
        assert_eq!(items[0].line.status, Status::Skip);
        assert_eq!(
            items[0].line.reason.as_deref(),
            Some("copy of app space canix/sp7")
        );
        let gone = plan(&export, Some("canix"), &ledger, &ls(&["sp7"])).unwrap();
        assert_eq!(
            gone[0].line.status,
            Status::Create,
            "only canix/sp7 is the machine's space here"
        );
        let local = plan(&export, None, &ledger, &ls(&["canix/sp7", "sp7"])).unwrap();
        assert_eq!(
            local[0].line.status,
            Status::Create,
            "an export's copies are another machine's"
        );
        let mut elsewhere = copy("a".repeat(64), "w2");
        elsewhere.terminal_ids = Some(vec!["term_other".into()]);
        elsewhere.label = Some("other".into());
        export.tui_copies = vec![copy("c".repeat(64), "w1"), elsewhere];
        let other = plan(&export, Some("canix"), &ledger, &ls(&["canix/sp7"])).unwrap();
        assert_eq!(
            other[0].line.status,
            Status::Create,
            "another session or workspace"
        );
        export.tui_copies = vec![copy("a".repeat(64), "w2")];
        let moved = plan(&export, Some("canix"), &ledger, &ls(&["canix/sp7"])).unwrap();
        assert_eq!(moved[0].line.status, Status::Skip, "w1 was w2");
        assert_eq!(
            moved[0].line.notes,
            ["matched an earlier copy from app space canix/sp7 by terminal (was w2)"]
        );
    }

    /// The fixture's workspace as `id` with `label` on `terminal`, in a
    /// session whose live workspaces are `lives` (id, label, terminal).
    fn export_as(id: &str, label: &str, terminal: &str, lives: &[(&str, &str, &str)]) -> Export {
        let text = serde_json::to_string(&export())
            .unwrap()
            .replace("\"w1", &format!("\"{id}"))
            .replace("[1] work", label);
        let mut export: Export = serde_json::from_str(&text).unwrap();
        export.terminal_ids = BTreeMap::from([(id.to_string(), vec![terminal.to_string()])]);
        export.live_workspaces = lives
            .iter()
            .map(
                |(id, label, terminal)| crate::migrate::tui::LiveTuiWorkspace {
                    workspace_id: id.to_string(),
                    label: label.to_string(),
                    terminal_ids: vec![terminal.to_string()],
                },
            )
            .collect();
        export
    }

    #[test]
    fn copies_are_found_after_the_tui_renumbers_its_workspaces() {
        use crate::migrate::tui::AppCopy;
        // Copied as w2 "api" and w3 "web"; a rebuild numbered them w1 and
        // w2 again, with new terminals.
        let copied = [("w2", "api", "s_api", 1), ("w3", "web", "s_web", 2)];
        let root = tempfile::tempdir().unwrap();
        let mut ledger = Ledger::open(root.path()).unwrap();
        for (workspace, label, space, at) in copied {
            ledger
                .record(Entry::ToApp {
                    source: AppSource {
                        workspace_id: workspace.into(),
                        terminal_ids: Some(vec![format!("old_{label}")]),
                        label: Some(label.into()),
                        ..source()
                    },
                    created: AppCreated {
                        space: Some(space.into()),
                        terminals: vec![],
                        unknown: false,
                    },
                    at,
                })
                .unwrap();
        }
        let empty_root = tempfile::tempdir().unwrap();
        let empty = Ledger::open(empty_root.path()).unwrap();
        let app_copies: Vec<AppCopy> = copied
            .iter()
            .map(|(workspace, label, space, at)| AppCopy {
                workspace_id: workspace.to_string(),
                space: space.to_string(),
                terminal_ids: Some(vec![format!("old_{label}")]),
                label: Some(label.to_string()),
                at: *at,
            })
            .collect();
        let lives = [("w1", "[1] api", "new_api"), ("w2", "[2] web", "new_web")];
        for (id, label, terminal, space, was) in [
            ("w1", "[1] api", "new_api", "s_api", "w2"),
            ("w2", "[2] web", "new_web", "s_web", "w3"),
        ] {
            let local = export_as(id, label, terminal, &lives);
            let items = plan(&local, None, &ledger, &ls(&["s_api", "s_web"])).unwrap();
            let mut remote = export_as(id, label, terminal, &lives);
            remote.app_copies = app_copies.clone();
            let listed = ls(&["canix/s_api", "canix/s_web"]);
            let machine = plan(&remote, Some("canix"), &empty, &listed).unwrap();
            for (line, space) in [
                (&items[0].line, space.to_string()),
                (&machine[0].line, format!("canix/{space}")),
            ] {
                assert_eq!(line.status, Status::Unchanged, "{id}");
                assert_eq!(line.copy.as_deref(), Some(space.as_str()), "{id}");
                let note = format!(
                    "matched an earlier copy {space} by label (was {was}): if this is another \
                     workspace, close {space} or rename this one"
                );
                assert_eq!(line.notes, [note], "{id}");
            }
        }
        // Two live workspaces are named "web": the label tells neither apart,
        // and the copy recorded under w2 is named, not trusted.
        let twins = [("w1", "[1] web", "new_api"), ("w2", "[2] web", "new_web")];
        let export = export_as("w2", "[2] web", "new_web", &twins);
        let items = plan(&export, None, &ledger, &ls(&["s_api", "s_web"])).unwrap();
        let line = &items[0].line;
        assert_eq!(line.status, Status::Create);
        assert_eq!(line.notes.len(), 1);
        assert!(
            line.notes[0].contains("copied a TUI workspace w2 to space s_api; this may be"),
            "{:?}",
            line.notes
        );
    }

    #[test]
    fn a_partial_that_names_nothing_stays_with_its_workspace_after_a_renumbering() {
        // A copy of w2 "api" stopped before the app named its space; then a
        // rebuild made api w1 and "web" w2, with new terminals.
        let root = tempfile::tempdir().unwrap();
        let mut ledger = Ledger::open(root.path()).unwrap();
        let pending = AppSource {
            workspace_id: "w2".into(),
            terminal_ids: Some(vec!["old_api".into()]),
            label: Some("api".into()),
            ..source()
        };
        ledger
            .record_partial(Entry::ToApp {
                source: pending.clone(),
                created: AppCreated {
                    space: None,
                    terminals: vec![],
                    unknown: true,
                },
                at: 7,
            })
            .unwrap();
        let lives = [("w1", "[1] api", "new_api"), ("w2", "[2] web", "new_web")];
        let api = export_as("w1", "[1] api", "new_api", &lives);
        let web = export_as("w2", "[2] web", "new_web", &lives);
        let unknown = unknown_partial_note(&AppCreated::default(), 7);
        let items = plan(&api, None, &ledger, &ls(&[])).unwrap();
        assert_eq!(items[0].line.status, Status::Create);
        assert!(items[0].line.notes.contains(&unknown), "api has it");
        assert_eq!(items[0].stale, [(pending.clone(), 7)]);
        let items = plan(&web, None, &ledger, &ls(&[])).unwrap();
        assert!(!items[0].line.notes.contains(&unknown), "web does not");
        assert_eq!(items[0].stale, []);

        // Copying web (now w2) keeps it; copying api drops it.
        let mut report = Report::new(Side::App, None, vec![]);
        apply(
            &items,
            &web,
            None,
            &mut ledger,
            &mut FakeApp::new(None),
            &mut report,
        )
        .unwrap();
        assert_eq!(ledger.app_partials(None, &web.session).len(), 1);
        let items = plan(&api, None, &ledger, &ls(&[])).unwrap();
        apply(
            &items,
            &api,
            None,
            &mut ledger,
            &mut FakeApp::new(None),
            &mut report,
        )
        .unwrap();
        assert_eq!(ledger.app_partials(None, &api.session), []);
    }

    fn copied_as(ledger: &mut Ledger, terminal_ids: Option<Vec<&str>>, label: Option<&str>) {
        let mut source = source();
        source.terminal_ids = terminal_ids.map(|ids| ids.into_iter().map(String::from).collect());
        source.label = label.map(String::from);
        ledger
            .record(Entry::ToApp {
                source,
                created: AppCreated {
                    space: Some("s7".into()),
                    terminals: vec![],
                    unknown: false,
                },
                at: 1_793_000_000_000,
            })
            .unwrap();
    }

    #[test]
    fn a_workspace_that_took_a_copied_workspaces_id_is_copied_with_a_note() {
        let root = tempfile::tempdir().unwrap();
        let mut ledger = Ledger::open(root.path()).unwrap();
        let note = "an earlier run at 2026-10-26 07:33 UTC copied a TUI workspace w1 to space \
                    s7; this may be another workspace that took its id: if it is the same, \
                    close s7 first";
        // The export's w1 is "[1] work" on term_fixture_1 and term_fixture_2.
        for (ids, label, why) in [
            (
                Some(vec!["term_old"]),
                Some("frontend"),
                "reused: new label, new terminals",
            ),
            (None, None, "an entry from before ids and labels"),
            (Some(vec!["term_old"]), None, "an entry from before labels"),
            (None, Some(""), "an empty label tells nothing"),
        ] {
            copied_as(&mut ledger, ids, label);
            let items = plan(&export(), None, &ledger, &ls(&["s7"])).unwrap();
            let line = &items[0].line;
            assert_eq!(line.status, Status::Create, "{why}");
            assert_eq!(line.copy, None, "{why}");
            assert_eq!(line.notes, [note], "{why}");
        }
        let by_label = "an earlier run at 2026-10-26 07:33 UTC copied a TUI workspace w1 to \
                        space s7, and this one matches it only by its label: if it is another \
                        workspace, close s7 or rename this one to copy it";
        for (ids, label, notes, why) in [
            (
                Some(vec!["term_old"]),
                Some("work"),
                vec![by_label],
                "restarted: same label, new terminals",
            ),
            (
                None,
                Some("work"),
                vec![by_label],
                "an entry with a label and no ids",
            ),
            (
                Some(vec!["term_closed", "term_fixture_2"]),
                Some("old name"),
                vec![],
                "renamed without a restart: a terminal is left",
            ),
            (
                Some(vec!["term_fixture_1"]),
                Some("work"),
                vec![],
                "a terminal is left and the label is the same",
            ),
        ] {
            copied_as(&mut ledger, ids, label);
            let items = plan(&export(), None, &ledger, &ls(&["s7"])).unwrap();
            let line = &items[0].line;
            assert_eq!(line.status, Status::Unchanged, "{why}");
            assert_eq!(line.copy.as_deref(), Some("s7"), "{why}");
            assert_eq!(line.notes, notes, "{why}");
        }
        assert_eq!(
            plan(&export(), None, &ledger, &ls(&["s7"])).unwrap()[0].source,
            source(),
            "a copy records the terminals and label the workspace has now"
        );
        let mut older = export();
        older.terminal_ids.clear();
        copied_as(&mut ledger, Some(vec!["term_fixture_1"]), Some("frontend"));
        let items = plan(&older, None, &ledger, &ls(&["s7"])).unwrap();
        assert_eq!(
            items[0].line.status,
            Status::Create,
            "an export without terminal ids and another label cannot tell"
        );
        assert_eq!(items[0].source.terminal_ids, None);
        let items = plan(&export(), None, &ledger, &ls(&[])).unwrap();
        assert_eq!(items[0].line.notes, Vec::<String>::new(), "s7 was closed");
    }

    fn made_from_app(ledger: &mut Ledger, ids: Option<Vec<&str>>, label: Option<&str>) {
        ledger
            .record(Entry::ToTui {
                source: crate::migrate::ledger::TuiSource { space: "s9".into() },
                created: crate::migrate::ledger::TuiCreated {
                    session: "a".repeat(64),
                    workspace_id: "w1".into(),
                    terminal_ids: ids.map(|ids| ids.into_iter().map(String::from).collect()),
                    label: label.map(String::from),
                },
                at: 1,
            })
            .unwrap();
    }

    #[test]
    fn a_workspace_that_took_the_id_of_a_copy_from_the_app_is_copied_with_a_note() {
        let root = tempfile::tempdir().unwrap();
        let mut ledger = Ledger::open(root.path()).unwrap();
        made_from_app(&mut ledger, Some(vec!["term_old"]), Some("frontend"));
        let items = plan(&export(), None, &ledger, &ls(&["s9"])).unwrap();
        assert_eq!(items[0].line.status, Status::Create);
        assert_eq!(
            items[0].line.notes,
            [
                "an earlier run at 1970-01-01 00:00 UTC made a TUI workspace w1 from app space \
              s9; this may be another workspace that took its id: if it is the same, this \
              copies it back"
            ]
        );
        made_from_app(&mut ledger, Some(vec!["term_old"]), Some("work"));
        let items = plan(&export(), None, &ledger, &ls(&["s9"])).unwrap();
        assert_eq!(
            (items[0].line.status, items[0].line.reason.as_deref()),
            (Status::Skip, Some("copy of app space s9")),
            "restarted: same label, new terminals"
        );
        assert_eq!(
            items[0].line.notes,
            [
                "an earlier run at 1970-01-01 00:00 UTC made a TUI workspace w1 from app space \
                 s9, and this one matches it only by its label: if it is another workspace, \
                 rename it to copy it"
            ]
        );
        made_from_app(&mut ledger, Some(vec!["term_fixture_2"]), Some("work"));
        let items = plan(&export(), None, &ledger, &ls(&["s9"])).unwrap();
        assert_eq!(items[0].line.status, Status::Skip, "a terminal is left");
        assert_eq!(
            items[0].line.notes,
            Vec::<String>::new(),
            "a terminal is left"
        );
        let mut export = export();
        let copy =
            |terminal_ids: Option<Vec<String>>, label: Option<&str>| crate::migrate::tui::TuiCopy {
                space: "sp7".into(),
                session: "a".repeat(64),
                workspace_id: "w1".into(),
                terminal_ids,
                label: label.map(String::from),
                at: None,
            };
        export.tui_copies = vec![copy(None, None)];
        let empty_root = tempfile::tempdir().unwrap();
        let empty = Ledger::open(empty_root.path()).unwrap();
        let items = plan(&export, Some("canix"), &empty, &ls(&["canix/sp7"])).unwrap();
        assert_eq!(
            items[0].line.status,
            Status::Create,
            "an older machine's copy"
        );
        assert_eq!(
            items[0].line.notes,
            [
                "an earlier run made a TUI workspace w1 from app space canix/sp7; this may be \
              another workspace that took its id: if it is the same, this copies it back"
            ]
        );
        export.tui_copies = vec![copy(Some(vec!["term_old".into()]), Some("[2] work"))];
        let items = plan(&export, Some("canix"), &empty, &ls(&["canix/sp7"])).unwrap();
        assert_eq!(
            items[0].line.status,
            Status::Skip,
            "same label on the machine"
        );
        assert_eq!(
            items[0].line.notes,
            [
                "an earlier run made a TUI workspace w1 from app space canix/sp7, and this one \
                 matches it only by its label: if it is another workspace, rename it to copy it"
            ]
        );
    }

    #[test]
    fn of_two_copies_under_a_reused_tui_id_the_one_that_matches_is_the_origin() {
        // A ledger from before `record` dropped the copy a reused id outlived:
        // s1 was copied into w1, closed, and after a TUI restart s9 got w1.
        // The fixture's live w1 is "[1] work" on term_fixture_1 and 2.
        let entry = |space: &str, terminal: &str, label: &str, at: u64| {
            json!({"direction": "to_tui", "source": {"space": space},
                   "created": {"session": "a".repeat(64), "workspace_id": "w1",
                               "terminal_ids": [terminal], "label": label}, "at": at})
        };
        let (s1, s9) = (
            entry("s1", "term_gone", "api", 1),
            entry("s9", "term_fixture_2", "work", 2),
        );
        for entries in [[s1.clone(), s9.clone()], [s9.clone(), s1.clone()]] {
            let root = tempfile::tempdir().unwrap();
            std::fs::write(
                root.path().join("migrated.json"),
                json!({"tool": crate::model::TOOL, "schema": 1, "entries": entries,
                       "partial": []})
                .to_string(),
            )
            .unwrap();
            let ledger = Ledger::open(root.path()).unwrap();
            for present in [&["s1", "s9"][..], &["s9"]] {
                let items = plan(&export(), None, &ledger, &ls(present)).unwrap();
                let line = &items[0].line;
                assert_eq!(
                    (line.status, line.reason.as_deref()),
                    (Status::Skip, Some("copy of app space s9")),
                    "{present:?}"
                );
                assert_eq!(line.notes, Vec::<String>::new(), "{present:?}");
            }
            // s9 closed: w1 is copied back. Only open spaces are matched, so
            // s1, still open and recorded under w1, is named as a copy that
            // may be another workspace's, as an open copy would be.
            let items = plan(&export(), None, &ledger, &ls(&["s1"])).unwrap();
            assert_eq!(items[0].line.status, Status::Create);
            assert_eq!(
                items[0].line.notes,
                [
                    "an earlier run at 1970-01-01 00:00 UTC made a TUI workspace w1 from app \
                  space s1; this may be another workspace that took its id: if it is the \
                  same, this copies it back"
                ]
            );
        }
        // The same choice for another machine's copies.
        let ledger = Ledger::open(tempfile::tempdir().unwrap().path()).unwrap();
        let copy = |space: &str, terminal: &str, label: &str, at| crate::migrate::tui::TuiCopy {
            space: space.into(),
            session: "a".repeat(64),
            workspace_id: "w1".into(),
            terminal_ids: Some(vec![terminal.into()]),
            label: Some(label.into()),
            at: Some(at),
        };
        let mut export = export();
        export.tui_copies = vec![
            copy("s1", "term_gone", "api", 1),
            copy("s9", "term_fixture_2", "work", 2),
        ];
        let items = plan(
            &export,
            Some("canix"),
            &ledger,
            &ls(&["canix/s1", "canix/s9"]),
        )
        .unwrap();
        assert_eq!(
            items[0].line.reason.as_deref(),
            Some("copy of app space canix/s9")
        );
    }

    #[test]
    fn a_copy_into_a_machines_app_is_known_on_that_machine_and_to_a_third() {
        // This machine copies canix's w1 into canix's app.
        let export = export();
        let mine = tempfile::tempdir().unwrap();
        let theirs = tempfile::tempdir().unwrap();
        let mut ledger = Ledger::open(mine.path()).unwrap();
        let items = plan(&export, Some("canix"), &ledger, &ls(&[])).unwrap();
        let mut app = FakeApp::new(None);
        app.prefix = "canix/".into();
        let mut report = Report::new(Side::App, Some("canix"), vec![]);
        // What `hc exec` carries to canix's `migrate --record`.
        let mut told = Vec::new();
        let mut record = |machine: &str, entry: &Entry, partial: bool| -> Result<()> {
            assert_eq!(machine, "canix");
            if let Entry::ToApp { created, .. } = entry {
                told.push((partial, created.space.clone(), created.unknown));
            }
            let args = crate::migrate::remote::record_args(entry, partial)?;
            crate::migrate::remote::record_here(&args[2], partial, theirs.path())
        };
        let mut on = OnMachine {
            name: "canix",
            record: &mut record,
        };
        apply(
            &items,
            &export,
            Some(&mut on),
            &mut ledger,
            &mut app,
            &mut report,
        )
        .unwrap();
        assert_eq!(report.warnings, Vec::<String>::new());
        drop(ledger);
        // Once as soon as `space.new` made the space, a partial copy that may
        // have more; then the whole copy, which replaces it there.
        assert_eq!(
            told,
            [
                (true, Some("s1".to_string()), true),
                (false, Some("s1".to_string()), false)
            ]
        );

        // On canix, its own `--to app` finds the copy under canix's bare id.
        let there = Ledger::open(theirs.path()).unwrap();
        assert_eq!(there.app_partials(None, &export.session), []);
        let items = plan(&export, None, &there, &ls(&["s1"])).unwrap();
        assert_eq!(
            (items[0].line.status, items[0].line.copy.as_deref()),
            (Status::Unchanged, Some("s1"))
        );
        // A third machine has no record of it, but canix's export carries it.
        let entries = crate::migrate::ledger::read_copies(theirs.path()).unwrap();
        drop(there);
        let mut shipped = export.clone();
        shipped.app_copies = crate::migrate::tui::app_copies(&entries, &shipped);
        assert_eq!(shipped.app_copies.len(), 1);
        let third = Ledger::open(tempfile::tempdir().unwrap().path()).unwrap();
        let items = plan(&shipped, Some("canix"), &third, &ls(&["canix/s1"])).unwrap();
        assert_eq!(
            (items[0].line.status, items[0].line.copy.as_deref()),
            (Status::Unchanged, Some("canix/s1"))
        );
        let items = plan(&shipped, Some("canix"), &third, &ls(&[])).unwrap();
        assert_eq!(items[0].line.status, Status::Create, "the copy was closed");
    }

    #[test]
    fn a_copy_the_machine_cannot_record_stands_with_a_warning() {
        let export = export();
        let root = tempfile::tempdir().unwrap();
        let mut ledger = Ledger::open(root.path()).unwrap();
        let items = plan(&export, Some("canix"), &ledger, &ls(&[])).unwrap();
        let mut app = FakeApp::new(None);
        app.prefix = "canix/".into();
        let mut report = Report::new(Side::App, Some("canix"), vec![]);
        let mut record = |_: &str, _: &Entry, _: bool| -> Result<()> { bail!("hc exec timed out") };
        let mut on = OnMachine {
            name: "canix",
            record: &mut record,
        };
        apply(
            &items,
            &export,
            Some(&mut on),
            &mut ledger,
            &mut app,
            &mut report,
        )
        .unwrap();
        assert_eq!(report.warnings.len(), 2);
        assert!(
            report.warnings[0].starts_with(
                "canix did not record space s1, the copy of w1 being built (hc exec timed out); \
                 if this run stops before the copy ends, a migrate started on canix will not \
                 know that s1 is a partial copy of w1; to record it, run on canix: herdr-revive \
                 migrate --record '{"
            ),
            "{}",
            report.warnings[0]
        );
        assert!(report.warnings[0].ends_with("' --partial"));
        assert!(
            report.warnings[1].starts_with(
                "copied w1 into space s1 on canix, but canix did not record it (hc exec timed \
                 out); a migrate started on canix will not know that s1 is a copy of w1; to \
                 record it, run on canix: herdr-revive migrate --record '{"
            ),
            "{}",
            report.warnings[1]
        );
        assert_eq!(report.applied.as_ref().map(Vec::len), Some(1));
        let mut source = source();
        source.machine = Some("canix".into());
        assert!(
            ledger.copied_to_app(&source).is_some(),
            "this ledger keeps it"
        );
    }

    #[test]
    fn a_timed_out_create_leaves_a_partial_the_next_plan_names_with_its_time() {
        let root = tempfile::tempdir().unwrap();
        let mut ledger = Ledger::open(root.path()).unwrap();
        let export = export();
        let items = plan(&export, None, &ledger, &ls(&[])).unwrap();
        let mut report = Report::new(Side::App, None, vec![]);
        let mut app = FakeApp::new(None);
        app.time_out_at = Some(1);
        apply(&items, &export, None, &mut ledger, &mut app, &mut report)
            .err()
            .unwrap();
        let partial = ledger.partial_to_app(&source()).unwrap();
        assert_eq!(
            partial.created,
            &AppCreated {
                space: None,
                terminals: vec![],
                unknown: true,
            }
        );
        let at = super::super::utc_minute(partial.at);
        let items = plan(&export, None, &ledger, &ls(&[])).unwrap();
        let line = &items[0].line;
        assert_eq!(line.status, Status::Create);
        assert_eq!(line.partial, None);
        assert_eq!(
            line.notes,
            [format!(
                "an earlier run at {at} stopped before it knew what it made and may have left a space: \
                 check the app and close it first"
            )],
            "nothing listed ties to it, so it is named anyway"
        );

        let mut app = FakeApp::new(None);
        app.time_out_at = Some(3);
        apply(&items, &export, None, &mut ledger, &mut app, &mut report)
            .err()
            .unwrap();
        // It knows its space now: while the app lists s1 the source waits.
        let open = plan(&export, None, &ledger, &ls(&["s1"])).unwrap();
        assert_eq!(
            (open[0].line.status, open[0].line.reason.as_deref()),
            (
                Status::Skip,
                Some("partial copy s1 still open: close it first")
            )
        );
        let items = plan(&export, None, &ledger, &ls(&[])).unwrap();
        assert_eq!(items[0].line.status, Status::Create, "s1 was closed");
        assert_eq!(
            items[0].line.notes.len(),
            1,
            "the first one still names nothing"
        );

        let mut app = FakeApp::new(Some(2));
        apply(&items, &export, None, &mut ledger, &mut app, &mut report)
            .err()
            .unwrap();
        assert!(
            !ledger.partial_to_app(&source()).unwrap().created.unknown,
            "a plain failure knows what it made"
        );
        let mut app = FakeApp::new(None);
        apply(&items, &export, None, &mut ledger, &mut app, &mut report).unwrap();
        assert!(
            ledger
                .partials_to_app(&source())
                .iter()
                .all(|partial| partial.created.space.as_deref() == Some("s1")),
            "the one that named nothing went with the full copy; s1's stay while s1 may be open"
        );
        let items = plan(&export, None, &ledger, &ls(&[])).unwrap();
        assert_eq!(items[0].line.notes, Vec::<String>::new(), "s1 is closed");
    }

    #[test]
    fn a_listed_terminal_without_a_pane_fails_only_its_lookup() {
        let mut app = FakeApp::new(None);
        app.listed.push(AppTerminal {
            terminal: "t9".into(),
            pane: None,
        });
        let error = pane_of(&mut app, "t9").err().unwrap();
        assert!(error.to_string().contains("terminal t9"), "{error}");
    }

    #[test]
    fn a_finished_copy_is_recorded_and_reported() {
        let root = tempfile::tempdir().unwrap();
        let mut ledger = Ledger::open(root.path()).unwrap();
        let export = export();
        let items = plan(&export, None, &ledger, &ls(&[])).unwrap();
        let mut report = Report::new(
            Side::App,
            None,
            items.iter().map(|i| i.line.clone()).collect(),
        );
        let mut app = FakeApp::new(None);
        apply(&items, &export, None, &mut ledger, &mut app, &mut report).unwrap();
        let applied = report.applied.unwrap();
        assert_eq!(applied[0].source, "w1");
        assert_eq!(applied[0].created["space"], "s1");
        assert_eq!(
            ledger
                .copied_to_app(&source())
                .unwrap()
                .created
                .space
                .as_deref(),
            Some("s1")
        );
    }

    #[test]
    fn a_space_new_without_a_space_id_still_records_its_terminal() {
        let export = export();
        let mut app = FakeApp::new(None);
        app.space_new_reports_space = false;
        let mut created = AppCreated::default();
        let error = build_workspace(
            &mut app,
            &export.snapshot.layout[0],
            &export.launches,
            None,
            &mut created,
            &mut |_| Ok(()),
        )
        .err()
        .unwrap();
        assert!(format!("{error:#}").contains("did not report the space"));
        assert_eq!(
            created,
            AppCreated {
                space: None,
                terminals: vec!["t1".into()],
                unknown: false,
            }
        );
    }

    #[test]
    fn a_leaf_without_a_launch_is_a_plan_error() {
        let root = tempfile::tempdir().unwrap();
        let ledger = Ledger::open(root.path()).unwrap();
        let mut export = export();
        export.launches.remove("w1:p4");
        let error = plan(&export, None, &ledger, &ls(&[])).err().unwrap();
        let text = format!("{error:#}");
        assert!(text.contains("workspace w1"), "{text}");
        assert!(text.contains("pane w1:p4"), "{text}");
    }

    #[test]
    fn a_ledger_that_cannot_record_a_finished_copy_names_what_was_made() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let mut ledger = Ledger::open(root.path()).unwrap();
        let export = export();
        let items = plan(&export, None, &ledger, &ls(&[])).unwrap();
        let mut report = Report::new(
            Side::App,
            None,
            items.iter().map(|i| i.line.clone()).collect(),
        );
        let mut app = FakeApp::new(None);
        let mode = |bits| std::fs::Permissions::from_mode(bits);
        std::fs::set_permissions(root.path(), mode(0o500)).unwrap();
        let result = apply(&items, &export, None, &mut ledger, &mut app, &mut report);
        std::fs::set_permissions(root.path(), mode(0o700)).unwrap();
        // The pending record comes first, so nothing is built that the
        // ledger could not name.
        assert!(result.is_err());
        assert_eq!(app.calls, Vec::<(String, Value)>::new());
        assert_eq!(report.applied, Some(vec![]));
    }
}
