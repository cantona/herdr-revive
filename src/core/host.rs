use crate::core::rpc::Client;
use crate::model::*;
use crate::platform;
use crate::transport::Host;
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::Duration;

pub const SPACE_LABEL: &str = "herdr.space";
pub const TAB_LABEL: &str = "herdr.tab";

/// Where `run` puts the command: into the pane through `terminal.send`, or
/// into `emitted` for `init`, whose stdout the pane's own shell evaluates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunMode {
    Send,
    Emit,
}

pub struct CoreHost {
    client: Client,
    pub mode: RunMode,
    pub emitted: Vec<String>,
    /// One process table per capture: filled by the first `process_info`
    /// after `snapshot`, dropped by `snapshot`, `pane` and `run` so restore
    /// and relaunch still judge each pane against a fresh table.
    table: Option<HashMap<u32, platform::Process>>,
    table_reads: usize,
    listed: HashSet<String>,
    startup: HashSet<String>,
}

impl CoreHost {
    pub fn connect(socket: &Path, config: &Config, mode: RunMode) -> Result<Self> {
        let client = Client::connect(socket, Duration::from_millis(config.timeout_ms))
            .context("cannot reach herdr-core")?;
        Ok(Self {
            client,
            mode,
            emitted: Vec::new(),
            table: None,
            table_reads: 0,
            listed: HashSet::new(),
            startup: HashSet::new(),
        })
    }

    pub fn requests(&self) -> usize {
        self.client.requests
    }

    /// The pane IDs of every terminal the last `snapshot` listed, in any
    /// state, including those that are not live.
    pub fn listed_panes(&self) -> &HashSet<String> {
        &self.listed
    }

    /// The panes whose shell has, since the last `snapshot`, been seen still
    /// starting: its own process group in the foreground with another member,
    /// as while it runs rc commands before job control is on. A shell started
    /// from a prompt leads a group of its own and never counts.
    pub fn startup_panes(&self) -> &HashSet<String> {
        &self.startup
    }

    /// How many times this host has read the whole process table.
    pub fn table_reads(&self) -> usize {
        self.table_reads
    }

    pub fn terminal(&mut self, address: &str) -> Result<Value> {
        validate_id(address)?;
        self.client
            .call("terminal.get", json!({"terminal": address}))
    }

    fn record(&mut self, pane: &str) -> Result<Value> {
        let address = address_of(pane)?.to_string();
        self.terminal(&address)
    }

    /// Core's own `foreground` is polled about twice a second; the kernel's
    /// answer for the shell's terminal is current, so that is what capture gets.
    /// A group missing from the cached table started after it was read, so the
    /// table is read again before the group is taken to be empty.
    fn foreground_info(&mut self, record: &Value, pane: &str) -> Result<ProcessInfo> {
        ensure!(
            record["state"].as_str() == Some("live"),
            "terminal {} is not live",
            record["address"].as_str().unwrap_or(pane)
        );
        let shell = record["pid"]
            .as_u64()
            .and_then(|pid| u32::try_from(pid).ok())
            .context("terminal has no shell process")?;
        let group = platform::terminal_foreground_group(shell)?;
        let mut foreground = match self.table.as_ref() {
            Some(table) => members(table, group),
            None => Vec::new(),
        };
        if foreground.is_empty() {
            self.table_reads += 1;
            let table = self.table.insert(platform::process_table()?);
            foreground = members(table, group);
        }
        let shell_group = self
            .table
            .as_ref()
            .and_then(|table| table.get(&shell))
            .map(|process| process.group);
        if shell_group == Some(group) && foreground.iter().any(|p| p.pid != shell) {
            self.startup.insert(pane.to_string());
        }
        Ok(ProcessInfo {
            pane_id: pane.into(),
            shell_pid: Some(shell),
            foreground_process_group_id: Some(group),
            foreground_processes: foreground,
        })
    }
}

fn members(table: &HashMap<u32, platform::Process>, group: u32) -> Vec<ForegroundProcess> {
    let mut members: Vec<ForegroundProcess> = table
        .values()
        .filter(|p| p.group == group)
        .map(|p| ForegroundProcess { pid: p.pid })
        .collect();
    members.sort_by_key(|p| p.pid);
    members
}

/// `Snapshot::validate` wants `<workspace>:t...` tabs and `<workspace>:p...`
/// panes, so the space label is the workspace, the tab label and the
/// terminal address are suffixed.
pub fn pane_id(space: &str, address: &str) -> String {
    format!("{space}:p{address}")
}

pub fn tab_id(space: &str, tab: &str) -> String {
    format!("{space}:t{tab}")
}

pub fn address_of(pane: &str) -> Result<&str> {
    validate_id(pane)?;
    let (_, address) = pane
        .rsplit_once(":p")
        .context("pane ID does not name a core terminal")?;
    ensure!(
        !address.is_empty() && !address.contains(':'),
        "pane ID does not name a core terminal"
    );
    Ok(address)
}

fn is_plain(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// The space label, tab label and address of a terminal the pipeline can
/// own, whatever its state; `None` when it is parked (empty labels),
/// unlabeled, labeled with an id revive cannot store, or on another machine
/// (`vps/t7`).
fn identity(record: &Value) -> Result<Option<(&str, &str, &str)>> {
    let address = record["address"]
        .as_str()
        .context("terminal record without address")?;
    let labels = &record["labels"];
    let (Some(space), Some(tab)) = (labels[SPACE_LABEL].as_str(), labels[TAB_LABEL].as_str())
    else {
        return Ok(None);
    };
    if !is_plain(space) || !is_plain(tab) || !is_plain(address) {
        return Ok(None);
    }
    Ok(Some((space, tab, address)))
}

/// `None` for terminals the pipeline cannot own (see `identity`) and for
/// finished ones.
pub fn live_pane(record: &Value) -> Result<Option<LivePane>> {
    let Some((space, tab, address)) = identity(record)? else {
        return Ok(None);
    };
    if record["state"].as_str() != Some("live") {
        return Ok(None);
    }
    Ok(Some(LivePane {
        workspace_id: space.into(),
        tab_id: tab_id(space, tab),
        pane_id: pane_id(space, address),
        terminal_id: address.into(),
        cwd: record["cwd"].as_str().map(String::from),
        agent: record
            .pointer("/status/app")
            .and_then(Value::as_str)
            .map(String::from),
        agent_session: None,
    }))
}

pub fn listed_pane_ids(records: &[Value]) -> Result<HashSet<String>> {
    let mut panes = HashSet::new();
    for record in records {
        if let Some((space, _, address)) = identity(record)? {
            panes.insert(pane_id(space, address));
        }
    }
    Ok(panes)
}

pub fn live_snapshot(records: &[Value]) -> Result<LiveSnapshot> {
    let mut workspaces: Vec<Workspace> = Vec::new();
    let mut tabs: Vec<Tab> = Vec::new();
    let mut panes = Vec::new();
    for record in records {
        let Some(pane) = live_pane(record)? else {
            continue;
        };
        if !workspaces
            .iter()
            .any(|w| w.workspace_id == pane.workspace_id)
        {
            workspaces.push(Workspace {
                workspace_id: pane.workspace_id.clone(),
                label: String::new(),
                active_tab_id: None,
            });
        }
        if !tabs.iter().any(|t| t.tab_id == pane.tab_id) {
            tabs.push(Tab {
                workspace_id: pane.workspace_id.clone(),
                tab_id: pane.tab_id.clone(),
                label: String::new(),
            });
        }
        panes.push(pane);
    }
    Ok(LiveSnapshot {
        focused_pane_id: None,
        workspaces,
        tabs,
        panes,
        focused_workspace_id: None,
    })
}

impl Host for CoreHost {
    fn supports_layout(&self) -> bool {
        false
    }
    fn snapshot(&mut self) -> Result<LiveSnapshot> {
        self.table = None;
        self.startup.clear();
        let reply = self.client.call("terminal.list", json!({}))?;
        let records = reply["terminals"]
            .as_array()
            .context("terminal.list reply without terminals")?;
        let live = live_snapshot(records)?;
        self.listed = listed_pane_ids(records)?;
        Ok(live)
    }
    fn process_info(&mut self, pane: &str) -> Result<ProcessInfo> {
        let record = self.record(pane)?;
        self.foreground_info(&record, pane)
    }
    fn pane(&mut self, pane: &str) -> Result<LivePane> {
        self.table = None;
        let record = self.record(pane)?;
        let live = live_pane(&record)?.context("terminal is no longer a shown app pane")?;
        ensure!(live.pane_id == pane, "pane identity changed");
        Ok(live)
    }
    fn run(&mut self, pane: &str, text: &str) -> Result<()> {
        self.table = None;
        let address = address_of(pane)?.to_string();
        match self.mode {
            RunMode::Emit => {
                self.emitted.push(text.to_string());
                Ok(())
            }
            RunMode::Send => {
                self.client
                    .call("terminal.send", json!({"terminal": address, "text": text}))?;
                Ok(())
            }
        }
    }
    fn api(&mut self, _: &str, _: Value, _: &str) -> Result<Value> {
        bail!("not supported by the herdr-core host")
    }
}
