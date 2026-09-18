use crate::{
    model::*,
    platform,
    store::{self, Store},
    transport::Host,
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashSet};
use std::path::Path;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceLayout {
    pub workspace_id: String,
    pub label: String,
    pub active_tab_id: Option<String>,
    pub tabs: Vec<TabLayout>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct TabLayout {
    pub tab_id: String,
    pub label: String,
    pub focused_pane_id: String,
    pub root: Node,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Node {
    Pane {
        pane_id: String,
        label: Option<String>,
    },
    Split {
        direction: Direction,
        ratio: f64,
        first: Box<Node>,
        second: Box<Node>,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Right,
    Down,
}

impl Node {
    pub fn from_host(value: &Value, depth: usize) -> Result<Self> {
        ensure!(depth <= 64, "layout tree exceeds depth limit");
        match value["type"].as_str() {
            Some("pane") => Ok(Self::Pane {
                pane_id: value["pane_id"]
                    .as_str()
                    .context("layout leaf has no pane ID")?
                    .into(),
                label: value
                    .get("label")
                    .filter(|v| !v.is_null())
                    .map(|v| v.as_str().context("invalid pane label").map(String::from))
                    .transpose()?,
            }),
            Some("split") => Ok(Self::Split {
                direction: serde_json::from_value(value["direction"].clone())
                    .context("invalid split direction")?,
                ratio: value["ratio"].as_f64().context("split has no ratio")?,
                first: Box::new(Self::from_host(&value["first"], depth + 1)?),
                second: Box::new(Self::from_host(&value["second"], depth + 1)?),
            }),
            _ => anyhow::bail!("invalid layout node"),
        }
    }
    pub fn leaves<'a>(&'a self, depth: usize, leaves: &mut Vec<&'a str>) -> Result<()> {
        ensure!(depth <= 64, "layout tree exceeds depth limit");
        match self {
            Self::Pane { pane_id, label } => {
                validate_id(pane_id)?;
                if let Some(label) = label {
                    validate_text(label)?;
                }
                leaves.push(pane_id);
            }
            Self::Split {
                ratio,
                first,
                second,
                ..
            } => {
                ensure!(
                    ratio.is_finite() && *ratio > 0.0 && *ratio < 1.0,
                    "invalid split ratio"
                );
                first.leaves(depth + 1, leaves)?;
                second.leaves(depth + 1, leaves)?;
            }
        }
        Ok(())
    }
    fn fits_apply(&self, depth: usize) -> Option<usize> {
        if depth > 16 {
            return None;
        }
        match self {
            Self::Pane { .. } => Some(1),
            Self::Split { first, second, .. } => {
                Some(first.fits_apply(depth + 1)? + second.fits_apply(depth + 1)?)
            }
        }
    }
    fn first_leaf(&self) -> &Self {
        match self {
            Self::Pane { .. } => self,
            Self::Split { first, .. } => first.first_leaf(),
        }
    }
    fn launch_tree(&self, panes: &BTreeMap<&str, &SavedPane>, config: &Config) -> Result<Value> {
        match self {
            Self::Pane { pane_id, label } => {
                let pane = panes
                    .get(pane_id.as_str())
                    .context("layout leaf not in snapshot")?;
                let mut value = json!({"type":"pane", "cwd":pane.cwd});
                if let Some(label) = label {
                    value["label"] = json!(label);
                }
                if let Some(command) = &pane.command {
                    // Denied commands leave a fresh shell; the plan exposes that decision.
                    if let Ok(argv) = command.allowed_argv(config) {
                        value["command"] = json!(argv);
                    }
                }
                Ok(value)
            }
            Self::Split {
                direction,
                ratio,
                first,
                second,
            } => Ok(json!({"type":"split", "direction":direction,
                "ratio":ratio, "first":first.launch_tree(panes,config)?, "second":second.launch_tree(panes,config)?})),
        }
    }
}

pub fn validate(snapshot: &Snapshot) -> Result<()> {
    if snapshot.layout.is_empty() {
        return Ok(());
    }
    let panes: BTreeMap<_, _> = snapshot
        .panes
        .iter()
        .map(|p| (p.pane_id.as_str(), p))
        .collect();
    let mut workspaces = HashSet::new();
    let mut tabs = HashSet::new();
    let mut leaves = HashSet::new();
    for workspace in &snapshot.layout {
        validate_id(&workspace.workspace_id)?;
        validate_text(&workspace.label)?;
        ensure!(
            workspaces.insert(workspace.workspace_id.as_str()) && !workspace.tabs.is_empty(),
            "duplicate or empty layout workspace"
        );
        if let Some(active) = &workspace.active_tab_id {
            ensure!(
                workspace.tabs.iter().any(|t| &t.tab_id == active),
                "active tab not in saved workspace"
            );
        }
        for tab in &workspace.tabs {
            validate_id(&tab.tab_id)?;
            validate_text(&tab.label)?;
            ensure!(tabs.insert(tab.tab_id.as_str()), "duplicate layout tab");
            let mut ordered = Vec::new();
            tab.root.leaves(0, &mut ordered)?;
            ensure!(
                ordered.contains(&tab.focused_pane_id.as_str()),
                "focused pane not in saved tab"
            );
            for id in ordered {
                ensure!(leaves.insert(id), "duplicate layout leaf");
                let pane = panes.get(id).context("layout leaf has no saved pane")?;
                ensure!(
                    pane.workspace_id == workspace.workspace_id && pane.tab_id == tab.tab_id,
                    "layout ancestry mismatch"
                );
            }
        }
    }
    ensure!(leaves.len() == panes.len(), "layout omits saved panes");
    Ok(())
}

pub fn capture(
    host: &mut impl Host,
    live: &LiveSnapshot,
    selected: Option<&str>,
) -> Result<Vec<WorkspaceLayout>> {
    let mut layouts = Vec::new();
    for workspace in live
        .workspaces
        .iter()
        .filter(|w| selected.is_none_or(|s| s == w.workspace_id))
    {
        let mut tabs = Vec::new();
        for tab in live
            .tabs
            .iter()
            .filter(|t| t.workspace_id == workspace.workspace_id)
        {
            let reply = host.api(
                "layout.export",
                json!({"tab_id":tab.tab_id}),
                "layout_export",
            )?;
            let layout = &reply["layout"];
            ensure!(
                layout["workspace_id"] == workspace.workspace_id && layout["tab_id"] == tab.tab_id,
                "exported layout identity mismatch"
            );
            tabs.push(TabLayout {
                tab_id: tab.tab_id.clone(),
                label: tab.label.clone(),
                focused_pane_id: layout["focused_pane_id"]
                    .as_str()
                    .context("layout focus unavailable")?
                    .into(),
                root: Node::from_host(&layout["root"], 0)?,
            });
        }
        layouts.push(WorkspaceLayout {
            workspace_id: workspace.workspace_id.clone(),
            label: workspace.label.clone(),
            active_tab_id: workspace.active_tab_id.clone(),
            tabs,
        });
    }
    Ok(layouts)
}

pub fn preview(snapshot: &Snapshot, config: &Config) -> Result<Value> {
    snapshot.validate()?;
    ensure!(
        !snapshot.layout.is_empty(),
        "snapshot has no reconstructable layout; capture a new snapshot"
    );
    let workspaces: Vec<_> = snapshot
        .layout
        .iter()
        .map(|w| {
            json!({"source_workspace_id":w.workspace_id,"label":w.label,
        "tabs":w.tabs.iter().map(|t| json!({"label":t.label,"root":t.root})).collect::<Vec<_>>()})
        })
        .collect();
    let commands: Vec<_> = snapshot.panes.iter().map(|p| json!({"pane_id":p.pane_id,"decision":match &p.command {
        None => "shell", Some(c) if c.allowed_argv(config).is_ok() => "launch", Some(_) => "denied_by_policy"
    }})).collect();
    Ok(
        json!({"mode":"recreate", "creates_new_workspaces":true, "workspaces":workspaces,"commands":commands}),
    )
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RebuildJournal {
    pub tool: String,
    pub schema: u32,
    pub session: String,
    pub operation: String,
    pub generation: String,
    pub state: String,
    pub acknowledged: bool,
    pub snapshot_hash: String,
    pub steps: Vec<Step>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Step {
    pub method: String,
    pub source_id: String,
    pub outcome: String,
    pub workspace_id: Option<String>,
    pub tab_id: Option<String>,
    pub target_pane_id: Option<String>,
    pub target_terminal_id: Option<String>,
    pub created_pane_id: Option<String>,
}

pub fn pending(store: &Store) -> Result<Option<RebuildJournal>> {
    let path = store.root.join("rebuild-pending.json");
    match std::fs::symlink_metadata(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
        Ok(_) => {
            let journal: RebuildJournal = store::read_json(&path)?;
            ensure!(
                native_tool(&journal.tool)
                    && journal.schema == SCHEMA
                    && journal.session == store.session,
                "corrupt rebuild evidence"
            );
            ensure!(
                journal.operation.len() == 64
                    && journal.operation.bytes().all(|b| b.is_ascii_hexdigit()),
                "invalid rebuild operation ID"
            );
            Ok(Some(journal))
        }
    }
}

pub fn acknowledge(store: &Store, operation: &str) -> Result<()> {
    let mut journal = pending(store)?.context("no pending reconstruction")?;
    ensure!(
        journal.operation == operation,
        "reconstruction acknowledgement ID mismatch"
    );
    journal.acknowledged = true;
    finish(store, &journal)
}

fn finish(store: &Store, journal: &RebuildJournal) -> Result<()> {
    let dir = store.root.join("operations");
    store::private_dir(&dir)?;
    store::atomic_json(&dir.join(format!("{}.json", journal.operation)), journal)?;
    std::fs::remove_file(store.root.join("rebuild-pending.json"))?;
    std::fs::File::open(&store.root)?.sync_all()?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn mutate(
    host: &mut impl Host,
    store: &Store,
    socket: &Path,
    journal: &mut RebuildJournal,
    source: &str,
    method: &str,
    params: Value,
    result_type: &str,
) -> Result<Value> {
    ensure!(
        platform::generation(socket)? == journal.generation,
        "server changed before mutation"
    );
    let target = params["pane_id"]
        .as_str()
        .or(params["target_pane_id"].as_str());
    let target_terminal = target
        .map(|id| host.pane(id).map(|pane| pane.terminal_id))
        .transpose()?;
    journal.steps.push(Step {
        method: method.into(),
        source_id: source.into(),
        outcome: "sending".into(),
        workspace_id: None,
        tab_id: None,
        target_pane_id: target.map(String::from),
        target_terminal_id: target_terminal,
        created_pane_id: None,
    });
    store::atomic_json(&store.root.join("rebuild-pending.json"), journal)?;
    let result = host.api(method, params, result_type)?;
    let step = journal.steps.last_mut().context("missing journal step")?;
    step.outcome = "applied".into();
    step.workspace_id = result["workspace"]["workspace_id"]
        .as_str()
        .or(result["layout"]["workspace_id"].as_str())
        .or(result["pane"]["workspace_id"].as_str())
        .map(String::from);
    step.tab_id = result["tab"]["tab_id"]
        .as_str()
        .or(result["layout"]["tab_id"].as_str())
        .or(result["pane"]["tab_id"].as_str())
        .map(String::from);
    if method == "pane.split" {
        step.created_pane_id = result["pane"]["pane_id"].as_str().map(String::from);
    }
    store::atomic_json(&store.root.join("rebuild-pending.json"), journal)?;
    Ok(result)
}

pub fn rebuild(
    host: &mut impl Host,
    store: &Store,
    socket: &Path,
    config_dir: &Path,
    snapshot: &Snapshot,
    focus: bool,
) -> Result<Value> {
    crate::app::require_clear(store)?;
    let config = store::load_config(config_dir)?;
    preview(snapshot, &config)?;
    let generation = platform::generation(socket)?;
    let snapshot_hash = store::digest(&serde_json::to_vec(snapshot)?);
    let operation = store::digest(
        format!(
            "{}:{}:{}",
            snapshot_hash,
            store::now_ms()?,
            std::process::id()
        )
        .as_bytes(),
    );
    let mut journal = RebuildJournal {
        tool: TOOL.into(),
        schema: SCHEMA,
        session: store.session.clone(),
        operation,
        generation: generation.clone(),
        state: "restoring".into(),
        acknowledged: false,
        snapshot_hash,
        steps: vec![],
    };
    let archive = store.root.join("operations");
    store::private_dir(&archive)?;
    store::atomic_json(
        &archive.join(format!("{}-snapshot.json", journal.operation)),
        snapshot,
    )?;
    store::atomic_json(&store.root.join("rebuild-pending.json"), &journal)?;
    let panes: BTreeMap<_, _> = snapshot
        .panes
        .iter()
        .map(|p| (p.pane_id.as_str(), p))
        .collect();
    let result: Result<Value> = (|| {
        let mut created = Vec::new();
        let mut focused = None;
        for workspace in &snapshot.layout {
            ensure!(
                platform::generation(socket)? == generation,
                "server changed during reconstruction"
            );
            let mut first_leaves = Vec::new();
            workspace.tabs[0].root.leaves(0, &mut first_leaves)?;
            let cwd = &panes[first_leaves[0]].cwd;
            let reply = mutate(
                host,
                store,
                socket,
                &mut journal,
                &workspace.workspace_id,
                "workspace.create",
                json!({"label":workspace.label,"cwd":cwd,"focus":false}),
                "workspace_created",
            )?;
            let new_workspace = reply["workspace"]["workspace_id"]
                .as_str()
                .context("created workspace ID missing")?
                .to_string();
            let initial_tab = reply["tab"]["tab_id"]
                .as_str()
                .context("created tab ID missing")?
                .to_string();
            created.push(new_workspace.clone());
            let mut active_focus = None;
            for (index, tab) in workspace.tabs.iter().enumerate() {
                ensure!(
                    platform::generation(socket)? == generation,
                    "server changed before layout apply"
                );
                let config = store::load_config(config_dir)?;
                let bulk = tab.root.fits_apply(1).is_some_and(|leaves| leaves <= 24);
                let initial = if bulk {
                    &tab.root
                } else {
                    tab.root.first_leaf()
                };
                let root = initial.launch_tree(&panes, &config)?;
                let mut params = json!({"tab_label":tab.label,"focus":false,"root":root});
                if index == 0 {
                    params["tab_id"] = json!(initial_tab);
                } else {
                    params["workspace_id"] = json!(new_workspace);
                }
                let reply = mutate(
                    host,
                    store,
                    socket,
                    &mut journal,
                    &tab.tab_id,
                    "layout.apply",
                    params,
                    "layout_apply",
                )?;
                ensure!(
                    reply["layout"]["workspace_id"] == new_workspace,
                    "layout applied to unexpected workspace"
                );
                let reply = if bulk {
                    reply
                } else {
                    let tab_id = reply["layout"]["tab_id"]
                        .as_str()
                        .context("created tab has no ID")?;
                    let anchor = reply["layout"]["root"]["pane_id"]
                        .as_str()
                        .context("created anchor missing")?;
                    let mut launches = Vec::new();
                    expand_tree(
                        host,
                        store,
                        socket,
                        &mut journal,
                        &tab.root,
                        anchor,
                        tab_id,
                        &new_workspace,
                        &panes,
                        &mut launches,
                    )?;
                    launch_splits(
                        host,
                        store,
                        socket,
                        &mut journal,
                        &launches,
                        &panes,
                        config_dir,
                    )?;
                    host.api("layout.export", json!({"tab_id":tab_id}), "layout_export")?
                };
                let tree = Node::from_host(&reply["layout"]["root"], 0)?;
                let mut old = Vec::new();
                let mut new = Vec::new();
                tab.root.leaves(0, &mut old)?;
                tree.leaves(0, &mut new)?;
                ensure!(old.len() == new.len(), "recreated leaf count mismatch");
                let pos = old
                    .iter()
                    .position(|p| *p == tab.focused_pane_id)
                    .context("saved focus not found")?;
                let pane = new[pos].to_string();
                if focus {
                    mutate(
                        host,
                        store,
                        socket,
                        &mut journal,
                        &tab.tab_id,
                        "pane.focus",
                        json!({"pane_id":pane}),
                        "pane_info",
                    )?;
                }
                if workspace
                    .active_tab_id
                    .as_ref()
                    .is_none_or(|id| id == &tab.tab_id)
                {
                    active_focus = Some(pane);
                }
            }
            if focus && let Some(pane) = active_focus {
                mutate(
                    host,
                    store,
                    socket,
                    &mut journal,
                    &workspace.workspace_id,
                    "pane.focus",
                    json!({"pane_id":pane}),
                    "pane_info",
                )?;
                if focused.is_none()
                    || snapshot
                        .focused_workspace_id
                        .as_ref()
                        .is_none_or(|id| id == &workspace.workspace_id)
                {
                    focused = Some(pane);
                }
            }
        }
        if focus && let Some(pane) = focused {
            ensure!(
                platform::generation(socket)? == generation,
                "server changed before focus"
            );
            mutate(
                host,
                store,
                socket,
                &mut journal,
                "focus",
                "pane.focus",
                json!({"pane_id":pane}),
                "pane_info",
            )?;
        }
        Ok(json!({"status":"recreated","workspaces":created,"operation":journal.operation}))
    })();
    match result {
        Ok(value) => {
            journal.state = "done".into();
            finish(store, &journal)?;
            Ok(value)
        }
        Err(error) => {
            journal.state = "failed".into();
            store::atomic_json(&store.root.join("rebuild-pending.json"), &journal)?;
            Err(error).context("reconstruction stopped; inspect created workspaces and acknowledge recovery evidence; no automatic retry")
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn expand_tree(
    host: &mut impl Host,
    store: &Store,
    socket: &Path,
    journal: &mut RebuildJournal,
    node: &Node,
    anchor: &str,
    tab: &str,
    workspace: &str,
    panes: &BTreeMap<&str, &SavedPane>,
    launches: &mut Vec<CreatedCommand>,
) -> Result<()> {
    let Node::Split {
        direction,
        ratio,
        first,
        second,
    } = node
    else {
        return Ok(());
    };
    let leaf = second.first_leaf();
    let Node::Pane { pane_id, label } = leaf else {
        unreachable!()
    };
    let saved = panes[pane_id.as_str()];
    let reply = mutate(
        host,
        store,
        socket,
        journal,
        pane_id,
        "pane.split",
        json!({"target_pane_id":anchor,"focus":false,"direction":direction,
            "ratio":ratio,"cwd":saved.cwd}),
        "pane_info",
    )?;
    let moved = reply["pane"]["pane_id"]
        .as_str()
        .context("created split pane missing")?;
    ensure!(
        reply["pane"]["tab_id"] == tab && reply["pane"]["workspace_id"] == workspace,
        "split created in wrong tab"
    );
    let terminal = reply["pane"]["terminal_id"]
        .as_str()
        .context("created terminal missing")?;
    if let Some(label) = label {
        mutate(
            host,
            store,
            socket,
            journal,
            pane_id,
            "pane.rename",
            json!({"pane_id":moved,"label":label}),
            "pane_info",
        )?;
    }
    if saved.command.is_some() {
        launches.push(CreatedCommand {
            source: pane_id.clone(),
            target: moved.into(),
            terminal: terminal.into(),
            tab: tab.into(),
        });
    }
    expand_tree(
        host, store, socket, journal, first, anchor, tab, workspace, panes, launches,
    )?;
    expand_tree(
        host, store, socket, journal, second, moved, tab, workspace, panes, launches,
    )
}

struct CreatedCommand {
    source: String,
    target: String,
    terminal: String,
    tab: String,
}

fn launch_splits(
    host: &mut impl Host,
    store: &Store,
    socket: &Path,
    journal: &mut RebuildJournal,
    launches: &[CreatedCommand],
    panes: &BTreeMap<&str, &SavedPane>,
    config_dir: &Path,
) -> Result<()> {
    if launches.is_empty() {
        return Ok(());
    }
    let config = store::load_config(config_dir)?;
    std::thread::sleep(std::time::Duration::from_millis(config.settle_ms));
    for CreatedCommand {
        source,
        target,
        terminal,
        tab,
    } in launches
    {
        let saved = panes[source.as_str()];
        let command = saved.command.as_ref().context("missing split command")?;
        let config = store::load_config(config_dir)?;
        if command.allowed_argv(&config).is_err() {
            continue;
        }
        let deadline =
            std::time::Instant::now() + std::time::Duration::from_millis(config.timeout_ms);
        let shell = loop {
            let info = host.process_info(target)?;
            if let Some(shell) = platform::idle_shell(&info)? {
                break shell;
            }
            ensure!(
                std::time::Instant::now() < deadline,
                "new pane did not become idle before deadline"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        let config = store::load_config(config_dir)?;
        let argv = command.allowed_argv(&config)?;
        let live = host.pane(target)?;
        ensure!(
            &live.terminal_id == terminal && &live.tab_id == tab && live.agent.is_none(),
            "created pane identity changed before command launch"
        );
        let text = platform::shell_command(&shell, &saved.cwd, &argv)?;
        mutate(
            host,
            store,
            socket,
            journal,
            source,
            "pane.send_input",
            json!({"pane_id":target,"text":text,"keys":["Enter"]}),
            "ok",
        )?;
    }
    Ok(())
}
