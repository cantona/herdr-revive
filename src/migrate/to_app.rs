//! TUI -> app: one `space.new` per workspace (with its first tab), one
//! `tab.new` per further tab, one `pane.split` per split, walked from the
//! TUI's binary tree. Split sizes are not copied: the app's commands take
//! none.

use super::external::{self, AppDo, AppLs, Made};
use super::ledger::{AppCreated, AppSource, Entry, Ledger, Same, same_workspace};
use super::tree::{self, Launch};
use super::tui::Export;
use super::{
    Applied, PlanLine, Report, Request, Side, Status, created_text, drive, earlier_run,
    missing_note,
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
}

/// Fails on a layout leaf with no launch, so a malformed export (say, from
/// another version on another machine) stops before anything is created.
pub fn plan(
    export: &Export,
    machine: Option<&str>,
    ledger: &Ledger,
    ls: &AppLs,
) -> Result<Vec<Item>> {
    let present: HashSet<&str> = ls.spaces.iter().map(|space| space.id.as_str()).collect();
    let listed: HashSet<&str> = ls
        .spaces
        .iter()
        .flat_map(|space| &space.tabs)
        .flat_map(|tab| &tab.terminals)
        .map(|listed| listed.terminal.as_str())
        .collect();
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
        let copy = ledger
            .copied_to_app(&source)
            .and_then(|recorded| {
                let space = recorded.created.space.clone()?;
                let same = same_workspace(recorded.source.seen(), live);
                Some((space, same, recorded.at))
            })
            .filter(|(space, ..)| present.contains(space.as_str()));
        // Only this machine's TUI copies are in this ledger, and a session id
        // (a hash of the socket path) can repeat on another machine. The best
        // match is taken before the app is asked whether its space is still
        // there: an older copy under a reused id is not the origin.
        let origin = ledger
            .tui_workspace_origin(&export.session, &workspace.workspace_id, live)
            .filter(|_| machine.is_none())
            .map(|(origin, same)| (origin.source.space.clone(), same, Some(origin.at)))
            .filter(|(space, ..)| present.contains(space.as_str()));
        let id = &workspace.workspace_id;
        if panes.is_empty() {
            line = line.skip("no panes");
        } else if let Some((space, same, at)) = copy.as_ref().filter(|copy| copy.1 != Same::No) {
            line.status = Status::Unchanged;
            line.copy = Some(space.clone());
            if *same == Same::Label {
                line.notes.push(label_copy_note(*at, id, space));
            }
        } else if let Some((space, same, at)) =
            origin.as_ref().filter(|origin| origin.1 != Same::No)
        {
            line = line.skip(&format!("copy of app space {space}"));
            if *same == Same::Label {
                line.notes.push(label_origin_note(*at, id, space));
            }
        } else {
            // herdr gives a closed workspace's id to a new one after its
            // server restarts; one with neither a recorded terminal nor the
            // recorded label is named, not trusted.
            if let Some((space, _, at)) = &copy {
                line.notes.push(format!(
                    "{} copied a TUI workspace {id} to space {space}; this may be another \
                     workspace that took its id: if it is the same, close {space} first",
                    earlier_run(Some(*at))
                ));
            }
            if let Some((space, _, at)) = &origin {
                line.notes.push(format!(
                    "{} made a TUI workspace {id} from app space {space}; this may be another \
                     workspace that took its id: if it is the same, this copies it back",
                    earlier_run(*at)
                ));
            }
            if let Some(partial) = ledger.partial_to_app(&source) {
                if partial.created.unknown {
                    line.notes
                        .push(unknown_partial_note(partial.created, partial.at));
                } else if match &partial.created.space {
                    Some(space) => present.contains(space.as_str()),
                    None => partial
                        .created
                        .terminals
                        .iter()
                        .any(|terminal| listed.contains(terminal.as_str())),
                } {
                    line.partial = Some(partial.created.clone());
                }
            }
        }
        if line.status == Status::Create && !missing.is_empty() {
            line.notes.push(missing_note(&missing));
        }
        items.push(Item {
            line,
            source,
            workspace: workspace.clone(),
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
        "{} stopped waiting for the app and may have left a space{known}: check the app and close it first",
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

pub fn build_workspace(
    app: &mut impl AppDo,
    workspace: &WorkspaceLayout,
    launches: &BTreeMap<String, Launch>,
    machine: Option<&str>,
    created: &mut AppCreated,
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
                ("tab.new", made)
            }
        };
        let terminal = made
            .terminal
            .with_context(|| format!("{command} did not report the terminal it made"))?;
        let pane = pane_of(app, &terminal)?;
        split(app, &tab.root, &pane, launches, created)?;
    }
    Ok(())
}

fn split(
    app: &mut impl AppDo,
    node: &Node,
    pane: &str,
    launches: &BTreeMap<String, Launch>,
    created: &mut AppCreated,
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
    let new = made
        .pane
        .context("pane.split did not report the pane it made")?;
    split(app, first, pane, launches, created)?;
    split(app, second, &new, launches, created)
}

pub fn apply(
    items: &[Item],
    export: &Export,
    machine: Option<&str>,
    ledger: &mut Ledger,
    app: &mut impl AppDo,
    report: &mut Report,
) -> Result<()> {
    for item in items
        .iter()
        .filter(|item| item.line.status == Status::Create)
    {
        let mut created = AppCreated::default();
        // Taken first so a clock error cannot hide what a failed build made.
        let at = store::now_ms()?;
        if let Err(error) = build_workspace(
            app,
            &item.workspace,
            &export.launches,
            machine,
            &mut created,
        ) {
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
            if let Err(note) = ledger.record_partial(Entry::ToApp {
                source: item.source.clone(),
                created,
                at,
            }) {
                error = error.context(format!("the partial copy was not recorded: {note:#}"));
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
        report.record(Applied {
            source: item.line.source.clone(),
            created: serde_json::to_value(&created)?,
        });
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
    let report = Report::new(
        Side::App,
        machine,
        items.iter().map(|item| item.line.clone()).collect(),
    );
    drive(
        report,
        request.confirm,
        request.json,
        input,
        output,
        errors,
        tty,
        |report| {
            apply(
                &items,
                export,
                machine,
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
            terminal_ids: BTreeMap::from([(
                "w1".to_string(),
                vec!["term_fixture_1".to_string(), "term_fixture_2".to_string()],
            )]),
            missing_folders: BTreeMap::new(),
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
            // s9 closed: w1 is copied back, and s1 is not named for it.
            let items = plan(&export(), None, &ledger, &ls(&["s1"])).unwrap();
            assert_eq!(items[0].line.status, Status::Create);
            assert_eq!(items[0].line.notes, Vec::<String>::new());
        }
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
                "an earlier run at {at} stopped waiting for the app and may have left a space: \
                 check the app and close it first"
            )],
            "nothing listed ties to it, so it is named anyway"
        );

        let mut app = FakeApp::new(None);
        app.time_out_at = Some(3);
        apply(&items, &export, None, &mut ledger, &mut app, &mut report)
            .err()
            .unwrap();
        let items = plan(&export, None, &ledger, &ls(&[])).unwrap();
        assert!(
            items[0].line.notes[0]
                .contains("may have left a space (at least space s1, terminals t1 t2):"),
            "{:?}",
            items[0].line.notes
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
        assert_eq!(ledger.partial_to_app(&source()), None);
        let items = plan(&export, None, &ledger, &ls(&[])).unwrap();
        assert_eq!(items[0].line.notes, Vec::<String>::new());
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
        let text = format!("{:#}", result.err().unwrap());
        assert!(text.contains("space s1, terminals t1 t2 t3 t4"), "{text}");
        assert_eq!(report.applied, Some(vec![]));
    }
}
