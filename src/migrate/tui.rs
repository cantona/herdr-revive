//! The TUI side of a migration: which herdr server `--session` names, the
//! plugin's directories outside the plugin, and the read-only capture a
//! copy is made from. Nothing here saves a snapshot or changes the TUI.

use super::TuiArgs;
use super::external;
use super::ledger::Seen;
use super::tree::{self, Launch};
use crate::model::*;
use crate::store::{self, Store};
use crate::transport::{Host, Transport};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub struct Target {
    pub socket: PathBuf,
    pub session: String,
    pub state_dir: PathBuf,
    pub config_dir: PathBuf,
    pub binary: Option<PathBuf>,
}

#[derive(Deserialize)]
struct SessionList {
    sessions: Vec<SessionEntry>,
}

#[derive(Deserialize)]
struct SessionEntry {
    name: String,
    #[serde(default)]
    default: bool,
    running: bool,
    socket_path: PathBuf,
}

/// The socket of session `name`, or of the default session, in the output
/// of `herdr session list --json`.
pub fn session_socket(list: &Value, name: Option<&str>) -> Result<PathBuf> {
    let list: SessionList = serde_json::from_value(list.clone())
        .context("herdr session list printed an unexpected shape")?;
    let entry = match name {
        Some(name) => list
            .sessions
            .iter()
            .find(|session| session.name == name)
            .with_context(|| format!("no herdr session named {name}"))?,
        None => list
            .sessions
            .iter()
            .find(|session| session.default)
            .context("herdr session list names no default session")?,
    };
    ensure!(
        entry.running,
        "the herdr session {} is not running; start it with `herdr --session {}`",
        entry.name,
        entry.name
    );
    Ok(entry.socket_path.clone())
}

pub fn plugin_dir(
    explicit: Option<PathBuf>,
    from_plugin: Option<PathBuf>,
    xdg: Option<PathBuf>,
    home: &Path,
    fallback: &str,
    tail: &str,
) -> Result<PathBuf> {
    let path = match (explicit, from_plugin, xdg.filter(|base| base.is_absolute())) {
        (Some(path), _, _) | (None, Some(path), _) => path,
        (None, None, Some(base)) => base.join(tail),
        (None, None, None) => home.join(fallback).join(tail),
    };
    ensure!(
        path.is_absolute(),
        "TUI plugin directory {} must be absolute",
        path.display()
    );
    Ok(path)
}

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn list_sessions(binary: Option<&Path>) -> Result<Value> {
    let program = binary.map_or_else(
        || "herdr".to_string(),
        |path| path.to_string_lossy().into_owned(),
    );
    external::run_json(
        &program,
        &["session".into(), "list".into(), "--json".into()],
        Duration::from_secs(10),
    )
}

impl Target {
    /// Creates the state directories; only a run that writes calls this.
    pub fn store(&self) -> Result<Store> {
        Store::new(&self.state_dir, self.session.clone())
    }
}

pub fn resolve(args: &TuiArgs) -> Result<Target> {
    ensure!(
        args.session.is_none() || args.socket.is_none(),
        "--session and --socket cannot be used together"
    );
    let binary = args
        .herdr_bin
        .clone()
        .or_else(|| env_path("HERDR_BIN_PATH"));
    let socket = match (&args.socket, &args.session) {
        (Some(socket), _) => socket.clone(),
        (None, Some(name)) => session_socket(&list_sessions(binary.as_deref())?, Some(name))?,
        (None, None) => match env_path("HERDR_SOCKET_PATH") {
            Some(socket) => socket,
            None => session_socket(&list_sessions(binary.as_deref())?, None)?,
        },
    };
    let (socket, session) = store::session_identity(&socket)?;
    let home = crate::core::paths::home_dir()?;
    let config_dir = plugin_dir(
        args.config_dir.clone(),
        env_path("HERDR_PLUGIN_CONFIG_DIR"),
        env_path("XDG_CONFIG_HOME"),
        &home,
        ".config",
        &format!("herdr/plugins/config/{PLUGIN_ID}"),
    )
    .context("TUI plugin config directory")?;
    let state_dir = plugin_dir(
        args.state_dir.clone(),
        env_path("HERDR_PLUGIN_STATE_DIR"),
        env_path("XDG_STATE_HOME"),
        &home,
        ".local/state",
        &format!("herdr/plugins/{PLUGIN_ID}"),
    )
    .context("TUI plugin state directory")?;
    Ok(Target {
        socket,
        session,
        state_dir,
        config_dir,
        binary,
    })
}

/// What a copy is made from: the capture, each pane's launch under the
/// TUI's own policy, and the panes whose agent still runs.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Export {
    pub tool: String,
    pub schema: u32,
    pub session: String,
    pub snapshot: Snapshot,
    pub launches: BTreeMap<String, Launch>,
    pub live_agents: Vec<String>,
    pub denied: Vec<String>,
    /// Each exported workspace's terminal ids (`ledger::same_workspace`).
    /// Exports from before this field have none.
    #[serde(default)]
    pub terminal_ids: BTreeMap<String, Vec<String>>,
    /// Each pane whose folder is gone on the exporting machine, with that
    /// folder (without Linux's ` (deleted)` mark); its launch is then a shell
    /// in the home folder. Exports from
    /// before this field have none.
    #[serde(default)]
    pub missing_folders: BTreeMap<String, String>,
}

impl Export {
    pub fn terminal_ids_of(&self, workspace_id: &str) -> Option<&[String]> {
        self.terminal_ids.get(workspace_id).map(Vec::as_slice)
    }

    /// An exported workspace as `ledger::same_workspace` compares it.
    pub fn seen(&self, workspace_id: &str) -> Seen<'_> {
        Seen {
            terminal_ids: self.terminal_ids_of(workspace_id),
            label: self
                .snapshot
                .layout
                .iter()
                .find(|workspace| workspace.workspace_id == workspace_id)
                .map(|workspace| workspace.label.as_str()),
        }
    }
}

/// Each workspace's terminal ids in a live snapshot.
pub fn terminal_ids(live: &LiveSnapshot) -> BTreeMap<String, Vec<String>> {
    let mut ids: BTreeMap<String, Vec<String>> = live
        .workspaces
        .iter()
        .map(|workspace| (workspace.workspace_id.clone(), Vec::new()))
        .collect();
    for pane in &live.panes {
        ids.entry(pane.workspace_id.clone())
            .or_default()
            .push(pane.terminal_id.clone());
    }
    ids
}

/// The workspaces `--only` names, in the session's order; all of them
/// without `--only`.
pub fn select(live: &LiveSnapshot, only: &[String]) -> Result<Vec<String>> {
    for id in only {
        ensure!(
            live.workspaces.iter().any(|w| &w.workspace_id == id),
            "no workspace {id} in the TUI session"
        );
    }
    Ok(live
        .workspaces
        .iter()
        .map(|w| w.workspace_id.clone())
        .filter(|id| only.is_empty() || only.contains(id))
        .collect())
}

pub fn export(target: &Target, only: &[String]) -> Result<Export> {
    let config = store::load_config(&target.config_dir)?;
    let mut host = Transport::new(target.socket.clone(), target.binary.clone(), &config);
    let before = host
        .snapshot()
        .context("cannot read the TUI session; is its server running?")?;
    let selected = select(&before, only)?;
    let snapshot = super::capture_each(
        &mut host,
        &target.session,
        &selected,
        before.focused_workspace_id.clone(),
        &config,
        "TUI workspace",
    )?;
    let live = host.snapshot()?;
    let home = crate::core::paths::home_dir()?;
    let home = home.to_str().context("HOME is not UTF-8")?;
    let mut launches = BTreeMap::new();
    let mut denied = Vec::new();
    let mut missing_folders = BTreeMap::new();
    for pane in &snapshot.panes {
        let Some(cwd) = crate::platform::existing_folder(&pane.cwd) else {
            missing_folders.insert(
                pane.pane_id.clone(),
                tree::shown_folder(&pane.cwd).to_string(),
            );
            launches.insert(
                pane.pane_id.clone(),
                Launch {
                    cwd: home.into(),
                    argv: None,
                },
            );
            continue;
        };
        let argv = match &pane.command {
            None => None,
            Some(command) => match command.allowed_argv(&config) {
                Ok(argv) => Some(argv),
                Err(error) => {
                    eprintln!(
                        "warning: pane {} command is not allowed by the policy and opens a shell instead: {error:#}",
                        pane.pane_id
                    );
                    denied.push(pane.pane_id.clone());
                    None
                }
            },
        };
        launches.insert(
            pane.pane_id.clone(),
            Launch {
                cwd: cwd.to_string(),
                argv,
            },
        );
    }
    let live_agents = live
        .panes
        .iter()
        .filter(|live| {
            live.agent
                .as_deref()
                .and_then(AgentKind::from_name)
                .is_some()
        })
        .filter(|live| {
            snapshot.panes.iter().any(|saved| {
                saved.pane_id == live.pane_id
                    && matches!(saved.command, Some(CommandSpec::Agent { .. }))
                    && !missing_folders.contains_key(&saved.pane_id)
                    && !denied.contains(&saved.pane_id)
            })
        })
        .map(|live| live.pane_id.clone())
        .collect();
    let terminal_ids = terminal_ids(&live)
        .into_iter()
        .filter(|(workspace, _)| selected.contains(workspace))
        .collect();
    Ok(Export {
        tool: TOOL.into(),
        schema: SCHEMA,
        session: target.session.clone(),
        snapshot,
        launches,
        live_agents,
        denied,
        terminal_ids,
        missing_folders,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::{Direction, Node, TabLayout, WorkspaceLayout};
    use serde_json::json;

    #[test]
    fn a_named_session_or_the_default_one_gives_the_socket() {
        let list = json!({"sessions": [
            {"default": true, "name": "default", "running": true,
             "session_dir": "/h/.config/herdr", "socket_path": "/h/.config/herdr/herdr.sock"},
            {"default": false, "name": "com", "running": false,
             "session_dir": "/h/.config/herdr/sessions/com",
             "socket_path": "/h/.config/herdr/sessions/com/herdr.sock"}
        ]});
        assert_eq!(
            session_socket(&list, None).unwrap(),
            PathBuf::from("/h/.config/herdr/herdr.sock")
        );
        let stopped = session_socket(&list, Some("com")).err().unwrap();
        assert!(stopped.to_string().contains("not running"));
        let missing = session_socket(&list, Some("nope")).err().unwrap();
        assert!(missing.to_string().contains("no herdr session named nope"));
    }

    #[test]
    fn plugin_dirs_prefer_flags_then_the_plugin_then_xdg_then_home() {
        let home = Path::new("/h");
        let tail = "herdr/plugins/cantona.herdr-revive";
        let dir = |e: Option<&str>, p: Option<&str>, x: Option<&str>| {
            plugin_dir(
                e.map(PathBuf::from),
                p.map(PathBuf::from),
                x.map(PathBuf::from),
                home,
                ".local/state",
                tail,
            )
        };
        assert_eq!(
            dir(Some("/e"), Some("/p"), Some("/x")).unwrap(),
            PathBuf::from("/e")
        );
        assert_eq!(
            dir(None, Some("/p"), Some("/x")).unwrap(),
            PathBuf::from("/p")
        );
        assert_eq!(
            dir(None, None, Some("/x")).unwrap(),
            PathBuf::from("/x/herdr/plugins/cantona.herdr-revive")
        );
        assert_eq!(
            dir(None, None, Some("relative")).unwrap(),
            PathBuf::from("/h/.local/state/herdr/plugins/cantona.herdr-revive")
        );
        assert!(dir(Some("relative"), None, None).is_err());
        assert!(dir(None, Some("relative"), None).is_err());
        assert!(dir(Some("relative"), None, Some("/x")).is_err());
        let error = dir(Some("relative"), None, None).err().unwrap();
        assert!(error.to_string().contains("relative"));
    }

    #[test]
    fn session_lists_without_a_default_or_in_a_bad_shape_are_errors() {
        let none = json!({"sessions": [
            {"default": false, "name": "a", "running": true, "socket_path": "/s"}
        ]});
        let error = session_socket(&none, None).err().unwrap();
        assert!(error.to_string().contains("no default session"));
        let bad = json!({"sessions": [{"name": "a", "default": true}]});
        let error = session_socket(&bad, None).err().unwrap();
        assert!(error.to_string().contains("unexpected shape"));
    }

    #[test]
    fn resolving_creates_nothing_and_refuses_session_with_socket() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        let args = TuiArgs {
            socket: Some(dir.path().join("herdr.sock")),
            config_dir: Some(dir.path().join("config")),
            state_dir: Some(state.clone()),
            ..TuiArgs::default()
        };
        let target = resolve(&args).unwrap();
        assert!(!state.exists());
        assert!(!dir.path().join("config").exists());
        target.store().unwrap();
        assert!(state.exists());
        let both = TuiArgs {
            session: Some("com".into()),
            ..args
        };
        let error = resolve(&both).err().unwrap();
        assert!(error.to_string().contains("cannot be used together"));
    }

    fn snapshot() -> Snapshot {
        let pane = |ws: &str, tab: &str, id: &str| SavedPane {
            workspace_id: ws.into(),
            tab_id: tab.into(),
            pane_id: id.into(),
            cwd: "/srv".into(),
            command: None,
        };
        let leaf = |id: &str| Node::Pane {
            pane_id: id.into(),
            label: None,
        };
        Snapshot {
            tool: TOOL.into(),
            schema: SCHEMA,
            session: "a".repeat(64),
            created_ms: 1,
            scope: Scope::Session,
            panes: vec![
                pane("w1", "w1:t1", "w1:p1"),
                pane("w1", "w1:t1", "w1:p2"),
                pane("w2", "w2:t1", "w2:p1"),
            ],
            layout: vec![
                WorkspaceLayout {
                    workspace_id: "w1".into(),
                    label: "[1] work".into(),
                    active_tab_id: None,
                    tabs: vec![TabLayout {
                        tab_id: "w1:t1".into(),
                        label: "1".into(),
                        focused_pane_id: "w1:p1".into(),
                        root: Node::Split {
                            direction: Direction::Right,
                            ratio: 0.5,
                            first: Box::new(leaf("w1:p1")),
                            second: Box::new(leaf("w1:p2")),
                        },
                    }],
                },
                WorkspaceLayout {
                    workspace_id: "w2".into(),
                    label: "api".into(),
                    active_tab_id: None,
                    tabs: vec![TabLayout {
                        tab_id: "w2:t1".into(),
                        label: "1".into(),
                        focused_pane_id: "w2:p1".into(),
                        root: leaf("w2:p1"),
                    }],
                },
            ],
            focused_workspace_id: Some("w2".into()),
        }
    }

    #[test]
    fn only_keeps_the_named_workspaces_in_session_order_and_refuses_unknown_ones() {
        let live = LiveSnapshot {
            focused_pane_id: None,
            workspaces: ["w1", "w2", "w3"]
                .iter()
                .map(|id| Workspace {
                    workspace_id: id.to_string(),
                    label: String::new(),
                    active_tab_id: None,
                })
                .collect(),
            tabs: vec![],
            panes: vec![],
            focused_workspace_id: None,
        };
        assert_eq!(
            select(&live, &["w3".into(), "w1".into()]).unwrap(),
            ["w1", "w3"]
        );
        assert_eq!(select(&live, &[]).unwrap(), ["w1", "w2", "w3"]);
        let error = select(&live, &["w9".into()]).err().unwrap();
        assert!(error.to_string().contains("no workspace w9"));
    }

    #[test]
    fn the_export_contract_keys() {
        let export = Export {
            tool: TOOL.into(),
            schema: SCHEMA,
            session: "a".repeat(64),
            snapshot: snapshot(),
            launches: BTreeMap::from([(
                "w1:p1".to_string(),
                Launch {
                    cwd: "/srv".into(),
                    argv: None,
                },
            )]),
            live_agents: vec![],
            denied: vec![],
            terminal_ids: BTreeMap::from([("w1".to_string(), vec!["term_1".to_string()])]),
            missing_folders: BTreeMap::from([("w1:p2".to_string(), "/gone".to_string())]),
        };
        let value = serde_json::to_value(&export).unwrap();
        let keys: Vec<&str> = value
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            [
                "tool",
                "schema",
                "session",
                "snapshot",
                "launches",
                "live_agents",
                "denied",
                "terminal_ids",
                "missing_folders"
            ]
        );
        assert_eq!(
            value["launches"]["w1:p1"],
            json!({"cwd": "/srv", "argv": null})
        );
        assert_eq!(value["terminal_ids"], json!({"w1": ["term_1"]}));
        assert_eq!(value["missing_folders"], json!({"w1:p2": "/gone"}));
        let back: Export = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(back, export);
        let mut older = value;
        older.as_object_mut().unwrap().remove("terminal_ids");
        older.as_object_mut().unwrap().remove("missing_folders");
        let back: Export = serde_json::from_value(older).unwrap();
        assert_eq!(back.missing_folders, BTreeMap::new());
        assert_eq!(back.terminal_ids_of("w1"), None);
        let seen = back.seen("w1");
        assert_eq!(seen.terminal_ids, None);
        assert_eq!(seen.label, Some("[1] work"));
    }

    #[test]
    fn terminal_ids_group_a_live_snapshot_by_workspace() {
        let live: LiveSnapshot = serde_json::from_value(json!({
            "focused_pane_id": null, "focused_workspace_id": null,
            "workspaces": [{"workspace_id": "w1", "active_tab_id": null},
                           {"workspace_id": "w2", "active_tab_id": null}],
            "tabs": [],
            "panes": [
                {"workspace_id": "w1", "tab_id": "w1:t1", "pane_id": "w1:p1",
                 "terminal_id": "term_a", "cwd": null, "agent": null, "agent_session": null},
                {"workspace_id": "w1", "tab_id": "w1:t2", "pane_id": "w1:p2",
                 "terminal_id": "term_b", "cwd": null, "agent": null, "agent_session": null}]
        }))
        .unwrap();
        assert_eq!(
            terminal_ids(&live),
            BTreeMap::from([
                (
                    "w1".to_string(),
                    vec!["term_a".to_string(), "term_b".to_string()]
                ),
                ("w2".to_string(), vec![]),
            ])
        );
    }
}
