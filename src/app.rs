use crate::core::host::{CoreHost, RunMode};
use crate::model::*;
use crate::store::Store;
use crate::transport::{Host, Transport};
use crate::{capture, planner, platform, store};
use anyhow::{Context, Result, anyhow, bail, ensure};
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

#[derive(Parser, Clone)]
#[command(version, about = "Session-scoped command restoration for Herdr")]
pub struct Cli {
    #[arg(long, global = true)]
    pub config_dir: Option<PathBuf>,
    #[arg(long, global = true)]
    pub state_dir: Option<PathBuf>,
    #[arg(long, global = true)]
    pub socket: Option<PathBuf>,
    #[arg(long, global = true)]
    pub herdr_bin: Option<PathBuf>,
    #[arg(long, global = true, value_enum)]
    pub host: Option<HostKind>,
    #[command(subcommand)]
    pub command: Action,
}

impl Cli {
    /// Whether the caller named the paths a host needs.
    pub fn explicit_paths(&self) -> bool {
        self.config_dir.is_some() || self.state_dir.is_some() || self.socket.is_some()
    }
}

#[derive(Subcommand, Clone)]
pub enum Action {
    /// Save the current session without running pane commands.
    Save,
    /// Inspect restore decisions. Does not query busy state or execute commands.
    Preview {
        #[arg(long, alias = "file")]
        snapshot: Option<PathBuf>,
    },
    /// Rehydrate existing panes and recreate missing workspaces. Preview first.
    Restore {
        #[arg(long, alias = "file")]
        snapshot: Option<PathBuf>,
        #[arg(long)]
        dry_run: bool,
        #[arg(long, conflicts_with = "rehydrate")]
        recreate: bool,
        #[arg(long)]
        rehydrate: bool,
    },
    /// List retained session snapshots.
    List,
    /// Startup and lifecycle event handler; both use the same boot claim.
    Event,
    /// Perform a debounced save if auto_save is enabled.
    Autosave {
        #[arg(long)]
        force: bool,
    },
    /// Foreground timer with a final save on orderly exit.
    Timer {
        #[arg(long)]
        interval_seconds: Option<u64>,
    },
    /// Save and repeatedly open named workspace layouts.
    Space {
        #[command(subcommand)]
        command: SpaceAction,
    },
    /// Inspect interrupted restore evidence.
    Recovery {
        #[command(subcommand)]
        command: RecoveryAction,
    },
    /// Print the default TOML configuration.
    Config,
    /// Print the shell hook for the herdr-core host; the rc file evaluates it.
    Init {
        #[arg(value_enum)]
        shell: InitShell,
    },
    /// Show, add or remove the rc line that loads the hook (herdr-core host).
    Install {
        #[arg(long, conflicts_with = "remove")]
        write: bool,
        #[arg(long)]
        remove: bool,
    },
    /// One read-only JSON document: latest snapshot, relaunch plan, recovery, setup.
    Status,
}

#[derive(Subcommand, Clone)]
pub enum SpaceAction {
    Save {
        name: String,
        #[arg(long)]
        workspace: Option<String>,
    },
    Preview {
        name: String,
    },
    #[command(alias = "restore")]
    Open {
        name: String,
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        no_focus: bool,
    },
    List,
    /// Print one saved name per line for interactive pickers.
    Names,
    /// Import a native space snapshot without executing it.
    Import {
        name: String,
        file: PathBuf,
        #[arg(long)]
        mapping: Option<PathBuf>,
    },
    Delete {
        name: String,
    },
}

#[derive(Subcommand, Clone)]
pub enum RecoveryAction {
    Inspect,
    /// Preserve evidence and mark the claim done; never retry commands.
    Acknowledge {
        generation: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum HostKind {
    Herdr,
    Core,
}

impl HostKind {
    /// `herdr` is implied when Herdr itself runs the plugin, and when the
    /// caller names its paths (`--config-dir`, `--state-dir`, `--socket`), as
    /// every invocation outside a plugin had to before the core host existed.
    pub fn resolve(explicit: Option<Self>, explicit_paths: bool) -> Result<Self> {
        if let Some(kind) = explicit {
            return Ok(kind);
        }
        match std::env::var("HERDR_REVIVE_HOST") {
            Ok(value) => match value.as_str() {
                "herdr" => Ok(Self::Herdr),
                "core" => Ok(Self::Core),
                _ => bail!("HERDR_REVIVE_HOST must be herdr or core"),
            },
            Err(std::env::VarError::NotPresent) => {
                ensure!(
                    explicit_paths || std::env::var_os("HERDR_PLUGIN_STATE_DIR").is_some(),
                    "--host herdr|core is required outside a Herdr plugin environment"
                );
                Ok(Self::Herdr)
            }
            Err(_) => bail!("HERDR_REVIVE_HOST must be herdr or core"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum InitShell {
    Bash,
}

pub enum HostImpl {
    Herdr(Transport),
    Core(CoreHost),
}

impl HostImpl {
    pub fn requests(&self) -> usize {
        match self {
            Self::Herdr(host) => host.requests,
            Self::Core(host) => host.requests(),
        }
    }
    pub fn children(&self) -> usize {
        match self {
            Self::Herdr(host) => host.children,
            Self::Core(_) => 0,
        }
    }
    pub fn emitted(&mut self) -> Vec<String> {
        match self {
            Self::Herdr(_) => Vec::new(),
            Self::Core(host) => std::mem::take(&mut host.emitted),
        }
    }
}

impl Host for HostImpl {
    fn supports_layout(&self) -> bool {
        match self {
            Self::Herdr(host) => host.supports_layout(),
            Self::Core(host) => host.supports_layout(),
        }
    }
    fn snapshot(&mut self) -> Result<LiveSnapshot> {
        match self {
            Self::Herdr(host) => host.snapshot(),
            Self::Core(host) => host.snapshot(),
        }
    }
    fn process_info(&mut self, pane: &str) -> Result<ProcessInfo> {
        match self {
            Self::Herdr(host) => host.process_info(pane),
            Self::Core(host) => host.process_info(pane),
        }
    }
    fn pane(&mut self, pane: &str) -> Result<LivePane> {
        match self {
            Self::Herdr(host) => host.pane(pane),
            Self::Core(host) => host.pane(pane),
        }
    }
    fn run(&mut self, pane: &str, text: &str) -> Result<()> {
        match self {
            Self::Herdr(host) => host.run(pane, text),
            Self::Core(host) => host.run(pane, text),
        }
    }
    fn api(&mut self, method: &str, params: Value, result_type: &str) -> Result<Value> {
        match self {
            Self::Herdr(host) => host.api(method, params, result_type),
            Self::Core(host) => host.api(method, params, result_type),
        }
    }
}

pub fn config_dir_for(kind: HostKind, explicit: Option<PathBuf>) -> Result<PathBuf> {
    match kind {
        HostKind::Herdr => env_path(explicit, "HERDR_PLUGIN_CONFIG_DIR"),
        HostKind::Core => crate::core::paths::config_dir(explicit),
    }
}

pub fn state_dir_for(kind: HostKind, explicit: Option<PathBuf>) -> Result<PathBuf> {
    match kind {
        HostKind::Herdr => env_path(explicit, "HERDR_PLUGIN_STATE_DIR"),
        HostKind::Core => crate::core::paths::state_dir(explicit),
    }
}

pub fn socket_for(kind: HostKind, explicit: Option<PathBuf>) -> Result<(PathBuf, String)> {
    match kind {
        HostKind::Herdr => store::session_identity(&env_path(explicit, "HERDR_SOCKET_PATH")?),
        HostKind::Core => {
            let socket = match explicit {
                Some(socket) => socket,
                None => crate::core::paths::resolve_socket(
                    &crate::core::paths::SocketInputs::from_env()?,
                ),
            };
            crate::core::paths::session_identity(&socket)
        }
    }
}

/// Hooks wait a bounded time for the session lock instead of skipping: an
/// `init` that skipped would silently lose this pane's relaunch.
pub fn wait_lock(store: &Store, config: &Config) -> Result<store::Lock> {
    let deadline = std::time::Instant::now()
        + std::time::Duration::from_millis((config.timeout_ms + config.settle_ms).min(60_000));
    let mut pause_ms = 5;
    loop {
        if let Some(lock) = store.try_lock()? {
            return Ok(lock);
        }
        ensure!(
            std::time::Instant::now() < deadline,
            "another operation holds the session lock"
        );
        std::thread::sleep(std::time::Duration::from_millis(pause_ms));
        pause_ms = (pause_ms * 2).min(50);
    }
}

/// `init` is evaluated by an interactive shell as it starts, so it exits 0.
/// A failure before the hook exists prints nothing; a later failure still
/// prints the hook, which parses and so cannot break the shell. A nested
/// shell, whose parent is not the pane's shell, gets nothing.
/// `REVIVE_DEBUG=1` shows the reason.
fn init(cli: &Cli, shell: InitShell) -> Value {
    match init_inner(cli, shell) {
        Ok(text) => json!({"text": text}),
        Err(error) => {
            init_debug(&error);
            json!({"text": ""})
        }
    }
}

/// Whether `pid` is herdr-core's holder (`herdr-core hold ...`), the process
/// that starts every pane's shell.
fn parent_is_core_holder(pid: u32) -> bool {
    crate::platform::process(pid)
        .and_then(|process| crate::platform::argv_and_cwd(&process))
        .is_ok_and(|(argv, _)| {
            argv.first()
                .and_then(|arg| std::path::Path::new(arg).file_name())
                .is_some_and(|name| name == "herdr-core")
                && argv.get(1).is_some_and(|arg| arg == "hold")
        })
}

fn init_debug(error: &anyhow::Error) {
    if std::env::var("REVIVE_DEBUG").as_deref() == Ok("1") {
        eprintln!("herdr-revive: init: {error:#}");
    }
}

fn init_inner(cli: &Cli, shell: InitShell) -> Result<String> {
    let kind = HostKind::resolve(cli.host, cli.explicit_paths())?;
    ensure!(
        kind == HostKind::Core,
        "init is only for the herdr-core host"
    );
    let InitShell::Bash = shell;
    let terminal = std::env::var("HC_TERMINAL_ID").context("HC_TERMINAL_ID is not set")?;
    validate_id(&terminal)?;
    let config_dir = config_dir_for(kind, cli.config_dir.clone())?;
    let config = store::load_config(&config_dir)?;
    if !config.auto_save && !config.auto_restore {
        return Ok(String::new());
    }
    let binary = std::env::current_exe().context("own executable path unavailable")?;
    let binary = binary.to_str().context("non-UTF-8 executable path")?;
    let hook = if config.auto_save {
        crate::core::hook::bash_snippet(&config, binary)
    } else {
        String::new()
    };
    let (socket, session) = socket_for(kind, cli.socket.clone())?;
    let parent = u32::try_from(nix::unistd::getppid().as_raw())?;
    let (core, record) = match CoreHost::connect(&socket, &config, RunMode::Emit)
        .and_then(|mut core| core.terminal(&terminal).map(|record| (core, record)))
    {
        Ok(found) => found,
        Err(error) => {
            init_debug(&error);
            // Without core the parent check below cannot run. `init` runs in
            // the shell's own command substitution, so `parent` is that shell;
            // only a shell the core's holder started is a pane's shell. A
            // nested one would install a hook whose saves are all dropped.
            let grandparent = crate::platform::process(parent).map(|shell| shell.parent);
            return Ok(if grandparent.is_ok_and(parent_is_core_holder) {
                hook
            } else {
                String::new()
            });
        }
    };
    ensure!(
        record["pid"].as_u64() == Some(u64::from(parent)),
        "init was not started by the pane's shell"
    );
    if !config.auto_restore {
        return Ok(hook);
    }
    let paths = InitPaths {
        state_dir: state_dir_for(kind, cli.state_dir.clone())?,
        session,
        socket,
        config_dir,
    };
    match init_relaunch(core, &record, &paths, &config, parent) {
        Ok(Relaunch::Command(text)) => Ok(text + &hook),
        Ok(Relaunch::Notice(text)) => Ok(hook + text),
        Ok(Relaunch::Nothing) => Ok(hook),
        Err(error) => {
            init_debug(&error);
            Ok(hook)
        }
    }
}

/// What `init` adds to the hook. The relaunch goes before it: both put an
/// entry first in `PROMPT_COMMAND`, and the hook's `__revive_rc=$?` has to
/// end up ahead of `__revive_relaunch`.
enum Relaunch {
    Nothing,
    Command(String),
    Notice(&'static str),
}

struct InitPaths {
    state_dir: PathBuf,
    session: String,
    socket: PathBuf,
    config_dir: PathBuf,
}

const AUTO_OFF_NOTICE: &str =
    "printf '\\033[2mherdr-revive: automatic relaunch skipped (REVIVE_AUTO=0)\\033[0m\\n' >&2\n";

const LOCK_NOTICE: &str = "printf '\\033[2mherdr-revive: automatic relaunch skipped: another operation held the session lock (herdr-revive --host core restore --rehydrate)\\033[0m\\n' >&2\n";

const RECOVERY_NOTICE: &str = "printf '\\033[2mherdr-revive: automatic relaunch skipped: recovery evidence is pending (herdr-revive --host core recovery inspect, then herdr-revive --host core recovery acknowledge ID)\\033[0m\\n' >&2\n";

fn init_relaunch(
    core: CoreHost,
    record: &Value,
    paths: &InitPaths,
    config: &Config,
    parent: u32,
) -> Result<Relaunch> {
    let Some(pane) = crate::core::host::live_pane(record)? else {
        return Ok(Relaunch::Nothing);
    };
    let store = Store::new(&paths.state_dir, paths.session.clone())?;
    let live = crate::core::host::live_snapshot(std::slice::from_ref(record))?;
    // Most new shells have nothing to relaunch; learn that before waiting
    // for the lock, which a running save holds for seconds while the prompt
    // waits. The snapshot is replaced atomically, so an unlocked read sees
    // a whole one, and the plan is made again under the lock.
    if matches!(
        plan_pane(&store, &paths.socket, config, &live, &pane),
        Ok(None)
    ) {
        return Ok(Relaunch::Nothing);
    }
    let lock = match wait_lock(&store, config) {
        Ok(lock) => lock,
        Err(error) => {
            init_debug(&error);
            return Ok(Relaunch::Notice(LOCK_NOTICE));
        }
    };
    if let Err(error) = require_clear(&store) {
        init_debug(&error);
        return Ok(Relaunch::Notice(RECOVERY_NOTICE));
    }
    let Some(planned) = plan_pane(&store, &paths.socket, config, &live, &pane)? else {
        return Ok(Relaunch::Nothing);
    };
    if std::env::var("REVIVE_AUTO").as_deref() == Ok("0") {
        return Ok(if planned.entry.decision == planner::Decision::Candidate {
            Relaunch::Notice(AUTO_OFF_NOTICE)
        } else {
            Relaunch::Nothing
        });
    }
    let mut host = HostImpl::Core(core);
    relaunch_pane(
        &mut host,
        &store,
        &lock,
        &paths.socket,
        &paths.config_dir,
        &planned,
        parent,
    )?;
    let emitted = host.emitted();
    if emitted.is_empty() {
        return Ok(Relaunch::Nothing);
    }
    Ok(Relaunch::Command(crate::core::hook::bash_relaunch(
        &emitted.join("\n"),
    )))
}

struct PlannedPane {
    generation: String,
    saved: Snapshot,
    entry: planner::PlanEntry,
}

/// The saved plan entry for one live pane, or `None` when there is nothing to
/// do: no snapshot, no entry for the pane, or this boot already handled it.
fn plan_pane(
    store: &Store,
    socket: &Path,
    config: &Config,
    live: &LiveSnapshot,
    pane: &LivePane,
) -> Result<Option<PlannedPane>> {
    require_clear(store)?;
    let generation = platform::generation(socket)?;
    let path = store.snapshot_path(None)?;
    match std::fs::symlink_metadata(&path) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let saved = store.read_snapshot(&path)?;
    let plan = planner::plan(&saved, &store.session, live, config)?;
    let Some(entry) = plan.entries.into_iter().find(|e| e.pane_id == pane.pane_id) else {
        return Ok(None);
    };
    if boot(store, &generation)?
        .is_some_and(|j| j.entries.iter().any(|e| e.pane_id == pane.pane_id))
    {
        return Ok(None);
    }
    Ok(Some(PlannedPane {
        generation,
        saved,
        entry,
    }))
}

fn relaunch_pane(
    host: &mut HostImpl,
    store: &Store,
    lock: &store::Lock,
    socket: &Path,
    config_dir: &Path,
    planned: &PlannedPane,
    parent: u32,
) -> Result<Option<Outcome>> {
    let PlannedPane {
        generation,
        saved,
        entry,
    } = planned;
    let saved_pane = saved
        .panes
        .iter()
        .find(|p| p.pane_id == entry.pane_id)
        .context("plan references unknown pane")?;
    execute_pane(
        host,
        store,
        lock,
        saved,
        saved_pane,
        entry,
        generation,
        |_, saved, entry, host| {
            ensure!(
                &platform::generation(socket)? == generation,
                "server changed during relaunch"
            );
            let current = store::load_config(config_dir)?;
            ensure!(
                current.auto_restore,
                "automatic restore disabled during execution"
            );
            let argv = saved
                .command
                .as_ref()
                .context("missing command")?
                .allowed_argv(&current)?;
            let live = match host.pane(&saved.pane_id) {
                Ok(live) => live,
                Err(error) => {
                    init_debug(&error.context("relaunch skipped: terminal.get failed"));
                    return Ok(None);
                }
            };
            ensure!(
                live.workspace_id == saved.workspace_id
                    && live.tab_id == saved.tab_id
                    && Some(&live.terminal_id) == entry.terminal_id.as_ref(),
                "pane identity changed during relaunch"
            );
            let info = match host.process_info(&saved.pane_id) {
                Ok(info) => info,
                Err(error) => {
                    init_debug(&error.context("relaunch skipped: process_info failed"));
                    return Ok(None);
                }
            };
            let Some(shell) = platform::startup_shell(&info, parent)? else {
                init_debug(&anyhow!(
                    "relaunch skipped: {}",
                    if info.shell_pid == Some(parent) {
                        "the shell does not hold its terminal's foreground group"
                    } else {
                        "the pane's shell is not the shell running init"
                    }
                ));
                return Ok(None);
            };
            ensure!(shell == "bash", "init bash was evaluated by another shell");
            Ok(Some(platform::shell_command(&shell, &saved.cwd, &argv)?))
        },
    )
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BootState {
    Restoring,
    Done,
    Failed,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Pending,
    Sending,
    Applied,
    Skipped,
    Failed,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JournalEntry {
    pub pane_id: String,
    pub outcome: Outcome,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Journal {
    pub tool: String,
    pub schema: u32,
    pub session: String,
    pub generation: String,
    pub snapshot_hash: Option<String>,
    pub state: BootState,
    pub acknowledged: bool,
    pub entries: Vec<JournalEntry>,
}

impl Journal {
    pub fn new(session: &str, generation: &str) -> Self {
        Self {
            tool: TOOL.into(),
            schema: SCHEMA,
            session: session.into(),
            generation: generation.into(),
            snapshot_hash: None,
            state: BootState::Restoring,
            acknowledged: false,
            entries: vec![],
        }
    }
    pub fn validate(&self, store: &Store) -> Result<()> {
        ensure!(
            native_tool(&self.tool) && self.schema == SCHEMA && self.session == store.session,
            "corrupt or cross-session boot record"
        );
        ensure!(
            self.generation.len() == 64 && self.generation.bytes().all(|b| b.is_ascii_hexdigit()),
            "invalid boot generation"
        );
        let mut seen = std::collections::HashSet::new();
        for entry in &self.entries {
            validate_id(&entry.pane_id)?;
            ensure!(seen.insert(&entry.pane_id), "duplicate journal pane");
        }
        Ok(())
    }
}

fn env_path(explicit: Option<PathBuf>, name: &str) -> Result<PathBuf> {
    let value = explicit
        .or_else(|| std::env::var_os(name).map(PathBuf::from))
        .with_context(|| format!("{name} is required"))?;
    ensure!(value.is_absolute(), "{name} must be absolute");
    Ok(value)
}

fn maybe_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(Some(store::read_json(path)?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

pub fn pending(store: &Store) -> Result<Option<Journal>> {
    let record: Option<Journal> = maybe_json(&store.root.join("pending.json"))?;
    if let Some(record) = &record {
        record.validate(store)?;
    }
    Ok(record)
}

fn boot_path(store: &Store, generation: &str) -> PathBuf {
    store.root.join("boots").join(format!("{generation}.json"))
}

pub fn boot(store: &Store, generation: &str) -> Result<Option<Journal>> {
    let record: Option<Journal> = maybe_json(&boot_path(store, generation))?;
    if let Some(record) = &record {
        record.validate(store)?;
        ensure!(
            record.generation == generation,
            "boot filename/identity mismatch"
        );
        ensure!(
            record.state == BootState::Done,
            "unfinished boot record requires inspection"
        );
    }
    Ok(record)
}

pub fn require_clear(store: &Store) -> Result<()> {
    ensure!(
        crate::layout::pending(store)?.is_none(),
        "reconstruction evidence requires recovery inspection and acknowledgement"
    );
    ensure!(
        pending(store)?.is_none(),
        "restore evidence requires inspection: run recovery inspect, then recovery acknowledge GENERATION"
    );
    Ok(())
}

fn persist_pending(store: &Store, journal: &Journal) -> Result<()> {
    journal.validate(store)?;
    store::atomic_json(&store.root.join("pending.json"), journal)
}

pub fn finish(store: &Store, journal: &Journal) -> Result<()> {
    journal.validate(store)?;
    ensure!(
        journal.state == BootState::Done,
        "cannot finish an unresolved restore"
    );
    store::atomic_json(&boot_path(store, &journal.generation), journal)?;
    let path = store.root.join("pending.json");
    if std::fs::symlink_metadata(&path).is_ok() {
        std::fs::remove_file(path)?;
        std::fs::File::open(&store.root)?.sync_all()?;
    }
    Ok(())
}

pub fn execute(
    host: &mut impl Host,
    store: &Store,
    saved: &Snapshot,
    plan: &planner::Plan,
    generation: &str,
    mut revalidate: impl FnMut(
        &mut JournalEntry,
        &SavedPane,
        &planner::PlanEntry,
        &mut dyn Host,
    ) -> Result<Option<String>>,
) -> Result<Journal> {
    require_clear(store)?;
    ensure!(
        boot(store, generation)?.is_none(),
        "this boot already has a restore claim"
    );
    let mut journal = Journal::new(&store.session, generation);
    journal.snapshot_hash = Some(store::digest(&serde_json::to_vec(saved)?));
    journal.entries = plan
        .entries
        .iter()
        .map(|e| JournalEntry {
            pane_id: e.pane_id.clone(),
            outcome: if e.decision == planner::Decision::Candidate {
                Outcome::Pending
            } else {
                Outcome::Skipped
            },
        })
        .collect();
    persist_pending(store, &journal)?;
    for i in 0..journal.entries.len() {
        if journal.entries[i].outcome == Outcome::Skipped {
            continue;
        }
        let pane = saved
            .panes
            .iter()
            .find(|p| p.pane_id == journal.entries[i].pane_id)
            .context("plan references unknown pane")?;
        send_entry(
            host,
            store,
            &mut journal,
            i,
            pane,
            &plan.entries[i],
            &mut revalidate,
            "restore stopped; inspect recovery evidence before any further save/restore",
        )?;
        persist_pending(store, &journal)?;
    }
    journal.state = BootState::Done;
    finish(store, &journal)?;
    Ok(journal)
}

/// Sends journal entry `i` and records its outcome in memory; the caller
/// persists a success. A failure is persisted here as `Failed` evidence.
#[allow(clippy::too_many_arguments)]
fn send_entry(
    host: &mut impl Host,
    store: &Store,
    journal: &mut Journal,
    i: usize,
    pane: &SavedPane,
    entry: &planner::PlanEntry,
    revalidate: &mut impl FnMut(
        &mut JournalEntry,
        &SavedPane,
        &planner::PlanEntry,
        &mut dyn Host,
    ) -> Result<Option<String>>,
    stopped: &'static str,
) -> Result<Outcome> {
    // Persist Sending before any operation that might write to a pane.
    journal.entries[i].outcome = Outcome::Sending;
    persist_pending(store, journal)?;
    let result: Result<Outcome> = (|| {
        let Some(text) = revalidate(&mut journal.entries[i], pane, entry, host)? else {
            return Ok(Outcome::Skipped);
        };
        host.run(&pane.pane_id, &text)?;
        Ok(Outcome::Applied)
    })();
    match result {
        Ok(outcome) => {
            journal.entries[i].outcome = outcome.clone();
            Ok(outcome)
        }
        Err(error) => {
            journal.entries[i].outcome = Outcome::Failed;
            journal.state = BootState::Failed;
            persist_pending(store, journal)
                .context("failed to persist failure; Sending evidence remains")?;
            Err(error).context(stopped)
        }
    }
}

/// One pane's relaunch at its shell's start. A generation's boot record grows
/// one entry per pane; a pane already recorded in this generation is left
/// alone (`None`). Non-candidates are recorded as skipped without running.
/// In a per-pane boot record `acknowledged` describes only the latest
/// unresolved send; `pending.json` existing is what means unresolved.
#[allow(clippy::too_many_arguments)]
pub fn execute_pane(
    host: &mut impl Host,
    store: &Store,
    _lock: &store::Lock,
    saved: &Snapshot,
    pane: &SavedPane,
    entry: &planner::PlanEntry,
    generation: &str,
    mut revalidate: impl FnMut(
        &mut JournalEntry,
        &SavedPane,
        &planner::PlanEntry,
        &mut dyn Host,
    ) -> Result<Option<String>>,
) -> Result<Option<Outcome>> {
    require_clear(store)?;
    ensure!(
        entry.pane_id == pane.pane_id,
        "plan entry does not match pane"
    );
    let mut journal = match boot(store, generation)? {
        Some(record) => record,
        None => {
            let mut record = Journal::new(&store.session, generation);
            record.snapshot_hash = Some(store::digest(&serde_json::to_vec(saved)?));
            record
        }
    };
    if journal.entries.iter().any(|e| e.pane_id == pane.pane_id) {
        return Ok(None);
    }
    journal.acknowledged = false;
    if entry.decision != planner::Decision::Candidate {
        journal.entries.push(JournalEntry {
            pane_id: pane.pane_id.clone(),
            outcome: Outcome::Skipped,
        });
        journal.state = BootState::Done;
        finish(store, &journal)?;
        return Ok(Some(Outcome::Skipped));
    }
    journal.state = BootState::Restoring;
    journal.entries.push(JournalEntry {
        pane_id: pane.pane_id.clone(),
        outcome: Outcome::Pending,
    });
    let i = journal.entries.len() - 1;
    let outcome = send_entry(
        host,
        store,
        &mut journal,
        i,
        pane,
        entry,
        &mut revalidate,
        "relaunch stopped; inspect recovery evidence before any further save/restore",
    )?;
    journal.state = BootState::Done;
    finish(store, &journal)?;
    Ok(Some(outcome))
}

/// A hook save runs detached and can land after herdr-core restarted, when
/// the terminal it names belongs to a new shell; saving then would overwrite
/// the snapshot without the command the old shell was running. Without the
/// hook's `REVIVE_SHELL_PID` (a manual save) there is nothing to compare.
fn hook_shell_is_current(core: &mut CoreHost) -> Result<bool> {
    let (Ok(shell), Ok(terminal)) = (
        std::env::var("REVIVE_SHELL_PID"),
        std::env::var("HC_TERMINAL_ID"),
    ) else {
        return Ok(true);
    };
    let shell: u64 = shell
        .parse()
        .context("REVIVE_SHELL_PID is not a process ID")?;
    let record = match core.terminal(&terminal) {
        Ok(record) => record,
        Err(error) if error.downcast_ref::<crate::core::rpc::RpcError>().is_some() => {
            return Ok(false);
        }
        Err(error) => return Err(error),
    };
    Ok(record["state"].as_str() == Some("live") && record["pid"].as_u64() == Some(shell))
}

const CORE_UNSUPPORTED: &str =
    "named spaces and layout rebuild are not supported by the herdr-core host";

pub fn run(cli: Cli) -> Result<Value> {
    if matches!(cli.command, Action::Timer { .. }) {
        return crate::timer::run(cli);
    }
    if matches!(cli.command, Action::Config) {
        return Ok(json!({"config": toml::to_string_pretty(&Config::default())?}));
    }
    if let Action::Init { shell } = cli.command {
        return Ok(init(&cli, shell));
    }
    let kind = HostKind::resolve(cli.host, cli.explicit_paths())?;
    if let Action::Install { write, remove } = cli.command {
        ensure!(
            kind == HostKind::Core,
            "install is only for the herdr-core host"
        );
        return crate::core::install::run(&crate::core::paths::home_dir()?, write, remove);
    }
    if kind == HostKind::Core
        && matches!(
            cli.command,
            Action::Space { .. } | Action::Restore { recreate: true, .. }
        )
    {
        bail!("{CORE_UNSUPPORTED}");
    }
    if let Ok(id) = std::env::var("HERDR_PLUGIN_ID") {
        ensure!(id == PLUGIN_ID, "wrong plugin identity");
    }
    let config_dir = config_dir_for(kind, cli.config_dir)?;
    let config = store::load_config(&config_dir)?;
    if matches!(cli.command, Action::Event) && !config.auto_save && !config.auto_restore {
        return Ok(json!({"status": "disabled"}));
    }
    if matches!(cli.command, Action::Autosave { force: false }) && !config.auto_save {
        return Ok(json!({"status": "disabled"}));
    }
    // The hook's saves carry --force, and a shell started while auto_save
    // was on keeps sending them: the setting decides, not the shell's age.
    if matches!(cli.command, Action::Autosave { force: true })
        && kind == HostKind::Core
        && std::env::var_os("REVIVE_SHELL_PID").is_some()
        && !config.auto_save
    {
        return Ok(json!({"status": "disabled"}));
    }
    let state_dir = state_dir_for(kind, cli.state_dir)?;
    let (socket, session) = socket_for(kind, cli.socket)?;
    let store = Store::new(&state_dir, session)?;
    let force_save = matches!(cli.command, Action::Autosave { force: true });
    let hook = matches!(
        cli.command,
        Action::Event | Action::Autosave { .. } | Action::Status
    );
    let mut acquired = if kind == HostKind::Core && force_save {
        Some(wait_lock(&store, &config)?)
    } else {
        store.try_lock()?
    };
    let contended_event =
        acquired.is_none() && hook && std::env::var_os("HERDR_PLUGIN_EVENT").is_some();
    // Real lifecycle notifications must not disappear behind a capture that
    // is still waiting for the very metadata this notification announces.
    if contended_event {
        let deadline = std::time::Instant::now()
            + std::time::Duration::from_millis((config.timeout_ms + config.settle_ms).min(60_000));
        let mut pause_ms = 5;
        while acquired.is_none() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(pause_ms));
            acquired = store.try_lock()?;
            pause_ms = (pause_ms * 2).min(50);
        }
        ensure!(
            acquired.is_some(),
            "lifecycle event could not acquire session lock; retry autosave"
        );
    }
    let Some(_lock) = acquired else {
        if hook {
            return Ok(json!({"status": "operation_in_progress"}));
        }
        bail!("another operation holds the session lock");
    };
    let _space_lock = if matches!(cli.command, Action::Space { .. }) {
        Some(
            store
                .try_space_lock()?
                .context("another operation holds the named-space library lock")?,
        )
    } else {
        None
    };
    let binary = cli
        .herdr_bin
        .or_else(|| std::env::var_os("HERDR_BIN_PATH").map(PathBuf::from));
    let mut host = match kind {
        HostKind::Herdr => HostImpl::Herdr(Transport::new(socket.clone(), binary, &config)),
        HostKind::Core => HostImpl::Core(CoreHost::connect(&socket, &config, RunMode::Send)?),
    };
    if let HostImpl::Core(core) = &mut host
        && force_save
        && !hook_shell_is_current(core)?
    {
        return Ok(json!({"status": "skipped", "reason": "shell replaced"}));
    }
    let result = match cli.command {
        Action::Config | Action::Timer { .. } | Action::Init { .. } | Action::Install { .. } => {
            unreachable!()
        }
        Action::Restore {
            snapshot,
            dry_run,
            recreate: true,
            ..
        } => {
            let saved = read_selected(&store, snapshot.as_deref(), None)?;
            if dry_run {
                crate::layout::preview(&saved, &config)?
            } else {
                ensure!(host.supports_layout(), "{CORE_UNSUPPORTED}");
                crate::layout::rebuild(&mut host, &store, &socket, &config_dir, &saved, true)?
            }
        }
        Action::List => json!({"snapshots": store.list("snapshots")?}),
        Action::Preview { snapshot } => manual_restore(
            &mut host,
            &store,
            &socket,
            &config_dir,
            &config,
            snapshot.as_deref(),
            true,
            false,
        )?,
        Action::Restore {
            snapshot,
            dry_run,
            rehydrate,
            ..
        } => manual_restore(
            &mut host,
            &store,
            &socket,
            &config_dir,
            &config,
            snapshot.as_deref(),
            dry_run,
            rehydrate,
        )?,
        Action::Save => save(&mut host, &store, &socket, &config, None, None, false)?,
        Action::Event | Action::Autosave { .. } => {
            require_clear(&store)?;
            let generation = platform::generation(&socket)?;
            if kind == HostKind::Herdr
                && !force_save
                && config.auto_restore
                && boot(&store, &generation)?.is_none()
            {
                let path = store.snapshot_path(None)?;
                match std::fs::symlink_metadata(&path) {
                    Ok(_) => restore(
                        &mut host,
                        &store,
                        &socket,
                        &config_dir,
                        &config,
                        None,
                        None,
                        true,
                    )?,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        let mut record = Journal::new(&store.session, &generation);
                        record.state = BootState::Done;
                        finish(&store, &record)?;
                        json!({"status": "no_snapshot"})
                    }
                    Err(e) => return Err(e.into()),
                }
            } else if config.auto_save || force_save {
                let last: Option<SaveTime> = maybe_json(&store.root.join("last-save.json"))?;
                let now = store::now_ms()?;
                if !force_save
                    && (!contended_event
                        || std::env::var("HERDR_PLUGIN_EVENT").as_deref()
                            == Ok("pane.agent_status_changed"))
                    && last.is_some_and(|last| {
                        last.generation == generation
                            && now.saturating_sub(last.time_ms) < config.debounce_ms
                    })
                    && !agent_session_needs_capture(&mut host, &store)?
                {
                    json!({"status": "debounced"})
                } else {
                    save(&mut host, &store, &socket, &config, None, None, true)?
                }
            } else {
                json!({"status": "boot_done"})
            }
        }
        Action::Space { command } => match command {
            SpaceAction::Names => {
                let names = store
                    .list("spaces")?
                    .iter()
                    .map(|p| {
                        p.file_stem()
                            .and_then(|s| s.to_str())
                            .context("invalid stored space filename")
                            .map(String::from)
                    })
                    .collect::<Result<Vec<_>>>()?;
                return Ok(
                    json!({"text": names.join("\n") + if names.is_empty() { "" } else { "\n" }}),
                );
            }
            SpaceAction::List => json!({"spaces": store.list("spaces")?}),
            SpaceAction::Save { name, workspace } => {
                let workspace = workspace
                    .or_else(|| std::env::var("HERDR_WORKSPACE_ID").ok())
                    .context("--workspace or HERDR_WORKSPACE_ID is required")?;
                save(
                    &mut host,
                    &store,
                    &socket,
                    &config,
                    Some(&name),
                    Some(&workspace),
                    false,
                )?
            }
            SpaceAction::Preview { name }
            | SpaceAction::Open {
                name,
                dry_run: true,
                ..
            } => crate::layout::preview(&read_selected(&store, None, Some(&name))?, &config)?,
            SpaceAction::Open {
                name,
                dry_run: false,
                no_focus,
            } => crate::layout::rebuild(
                &mut host,
                &store,
                &socket,
                &config_dir,
                &read_selected(&store, None, Some(&name))?,
                !no_focus,
            )?,
            SpaceAction::Import {
                name,
                file,
                mapping,
            } => import_space(&store, &mut host, &name, &file, mapping.as_deref(), &config)?,
            SpaceAction::Delete { name } => {
                require_clear(&store)?;
                std::fs::remove_file(store.snapshot_path(Some(&name))?)?;
                std::fs::File::open(&store.spaces)?.sync_all()?;
                json!({"status": "deleted"})
            }
        },
        Action::Status => status(&mut host, &store, &config_dir, &config, kind)?,
        Action::Recovery { command } => match command {
            RecoveryAction::Inspect => {
                json!({"pending": pending(&store)?, "rebuild": crate::layout::pending(&store)?, "boots": store.list("boots")?})
            }
            RecoveryAction::Acknowledge { generation } => {
                if crate::layout::pending(&store)?.is_some() {
                    crate::layout::acknowledge(&store, &generation)?;
                    return Ok(json!({"status":"acknowledged_without_retry"}));
                }
                let mut record = pending(&store)?.context("no unresolved restore")?;
                ensure!(
                    record.generation == generation,
                    "acknowledgement generation mismatch"
                );
                record.acknowledged = true;
                record.state = BootState::Done;
                finish(&store, &record)?;
                json!({"status": "acknowledged_without_retry"})
            }
        },
    };
    Ok(
        json!({"result": result, "requests": host.requests(), "herdr_children": host.children(), "identity_connections": platform::identity_connections()}),
    )
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SaveTime {
    generation: String,
    time_ms: u64,
    /// Panes this save carried forward from the snapshot a core recovery
    /// started from; only the next automatic save in this generation may
    /// carry them again.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    carried: Vec<String>,
}

// Only agent lifecycle events need this check. Ordinary debounced events keep
// their zero-request fast path, and unchanged agent sessions remain debounced.
fn agent_session_needs_capture(host: &mut impl Host, store: &Store) -> Result<bool> {
    if std::env::var("HERDR_PLUGIN_EVENT").as_deref() == Ok("pane.agent_detected") {
        // A new invocation may reuse the UUID but change cwd or launcher.
        return Ok(true);
    }
    if !matches!(
        std::env::var("HERDR_PLUGIN_EVENT").as_deref(),
        Ok("pane.agent_detected" | "pane.agent_status_changed")
    ) {
        return Ok(false);
    }
    let Ok(payload) = std::env::var("HERDR_PLUGIN_EVENT_JSON") else {
        return Ok(false);
    };
    ensure!(
        payload.len() <= MAX_BYTES,
        "event payload exceeds size limit"
    );
    let payload: Value = serde_json::from_str(&payload).context("invalid agent event payload")?;
    let Some(id) = payload.pointer("/data/pane_id").and_then(Value::as_str) else {
        return Ok(false);
    };
    let pane = host.pane(id)?;
    // Capture saves an agent revive cannot resume as a plain program, so its
    // events have no session to catch up on and stay debounced; bypassing for
    // them would write a snapshot per status change and rotate the retained
    // ones out. The live agent decides: herdr keeps a session after its agent
    // exits and even after another agent starts in the pane, so the session's
    // name counts only when no agent is running.
    let agent = pane.agent.as_deref().or(pane
        .agent_session
        .as_ref()
        .map(|session| session.agent.as_str()));
    if agent.and_then(AgentKind::from_name).is_none() {
        return Ok(false);
    }
    let Some(native) = pane.agent_session else {
        // A detection event may precede the session hook. Capture performs a
        // bounded metadata retry; it never replaces the snapshot on failure.
        return Ok(true);
    };
    let Some(previous) = maybe_json::<Snapshot>(&store.snapshot_path(None)?)? else {
        return Ok(true);
    };
    previous.validate()?;
    ensure!(
        previous.session == store.session,
        "cross-session snapshot refused"
    );
    Ok(!previous.panes.iter().any(|saved| saved.pane_id == id
        && saved.workspace_id == pane.workspace_id && saved.tab_id == pane.tab_id
        && pane.cwd.as_deref().is_some_and(|cwd| cwd == saved.cwd
            || Path::new(cwd).canonicalize().is_ok_and(|current|
                Path::new(&saved.cwd).canonicalize().is_ok_and(|old| current == old)))
        && matches!(&saved.command, Some(CommandSpec::Agent { agent, session_id, .. })
            if native.source == format!("herdr:{}", agent.name())
                && native.agent == agent.name() && native.kind == "id" && *session_id == native.value)))
}

fn retain_autosaved_agents(snapshot: &mut Snapshot, previous: &Snapshot) -> usize {
    let index: std::collections::HashMap<_, _> = previous
        .panes
        .iter()
        .map(|pane| (pane.pane_id.as_str(), pane))
        .collect();
    let mut retained = 0;
    for pane in &mut snapshot.panes {
        if pane.command.is_none()
            && let Some(old) = index.get(pane.pane_id.as_str())
            && old.workspace_id == pane.workspace_id
            && old.tab_id == pane.tab_id
            && old.cwd == pane.cwd
            && matches!(old.command, Some(CommandSpec::Agent { .. }))
        {
            pane.command.clone_from(&old.command);
            retained += 1;
        }
    }
    retained
}

/// During a core recovery each pane is relaunched by its own shell's `init`.
/// Until that init has recorded the pane in this generation's boot record,
/// the shell may still be reading its rc file (`startup`): capture then
/// records the shell itself, or drops a terminal that is not live yet, and
/// the pane's relaunch would read that. Such a pane keeps the entry the
/// recovery started from. Any other capture is the pane's real state and ends
/// the carry, also for panes whose init never records them. The candidates are
/// every pane of a snapshot saved before this generation, or, once this
/// generation has saved, the panes its last save carried, so a pane that
/// first appeared in this generation is always captured live.
fn carry_unrelaunched(
    snapshot: &mut Snapshot,
    previous: &Snapshot,
    last: Option<&SaveTime>,
    handled: &std::collections::HashSet<String>,
    listed: &std::collections::HashSet<String>,
    startup: &std::collections::HashSet<String>,
    generation: &str,
) -> Vec<String> {
    let candidates: Vec<&str> = match last {
        Some(last) if last.generation == generation => {
            last.carried.iter().map(String::as_str).collect()
        }
        _ => previous.panes.iter().map(|p| p.pane_id.as_str()).collect(),
    };
    let mut carried = Vec::new();
    for id in candidates {
        if handled.contains(id) || !listed.contains(id) {
            continue;
        }
        let Some(old) = previous.panes.iter().find(|p| p.pane_id == id) else {
            continue;
        };
        match snapshot.panes.iter_mut().find(|p| p.pane_id == id) {
            None => snapshot.panes.push(old.clone()),
            Some(pane) if startup.contains(id) => pane.clone_from(old),
            Some(_) => continue,
        }
        carried.push(id.to_string());
    }
    carried
}

fn save(
    host: &mut HostImpl,
    store: &Store,
    socket: &Path,
    config: &Config,
    name: Option<&str>,
    workspace: Option<&str>,
    automatic: bool,
) -> Result<Value> {
    require_clear(store)?;
    if let Some(name) = name {
        store::validate_name(name)?;
    }
    let generation = platform::generation(socket)?;
    let mut snapshot = capture::capture(host, &store.session, workspace, config)?;
    let core_recovery = automatic && config.auto_restore && matches!(host, HostImpl::Core(_));
    let previous = if automatic
        && (core_recovery || snapshot.panes.iter().any(|pane| pane.command.is_none()))
    {
        maybe_json::<Snapshot>(&store.snapshot_path(None)?)?
    } else {
        None
    };
    let mut retained_agents = 0;
    let mut carried = Vec::new();
    if let Some(previous) = &previous {
        previous.validate()?;
        ensure!(
            previous.session == store.session,
            "cross-session snapshot refused"
        );
        retained_agents = retain_autosaved_agents(&mut snapshot, previous);
        if core_recovery && let HostImpl::Core(core) = host {
            let last: Option<SaveTime> = maybe_json(&store.root.join("last-save.json"))?;
            let handled = boot(store, &generation)?
                .map(|record| record.entries.into_iter().map(|e| e.pane_id).collect())
                .unwrap_or_default();
            carried = carry_unrelaunched(
                &mut snapshot,
                previous,
                last.as_ref(),
                &handled,
                core.listed_panes(),
                core.startup_panes(),
                &generation,
            );
        }
    }
    ensure!(
        platform::generation(socket)? == generation,
        "server changed during capture"
    );
    // The herdr-core hook saves at every allowlisted command and prompt,
    // forced, with no debounce: one that changes nothing but the time is not
    // archived again, or it would push the distinct states out of retention;
    // only the latest snapshot takes its time.
    let unchanged = name.is_none()
        && std::env::var_os("REVIVE_SHELL_PID").is_some()
        && matches!(host, HostImpl::Core(_))
        && store
            .read_snapshot(&store.snapshot_path(None)?)
            .is_ok_and(|latest| {
                Snapshot {
                    created_ms: snapshot.created_ms,
                    ..latest
                } == snapshot
            });
    if unchanged {
        store.refresh_latest(&snapshot)?;
    } else {
        store.save(&snapshot, name, config.retention)?;
    }
    let carried_panes = carried.len();
    if name.is_none() {
        store::atomic_json(
            &store.root.join("last-save.json"),
            &SaveTime {
                generation,
                time_ms: store::now_ms()?,
                carried,
            },
        )?;
    }
    let mut result = json!({"status": "saved", "panes": snapshot.panes.len(), "retained_agents": retained_agents, "path": store.snapshot_path(name)?});
    if matches!(host, HostImpl::Core(_)) {
        result["carried_panes"] = json!(carried_panes);
    }
    Ok(result)
}

fn read_selected(store: &Store, file: Option<&Path>, name: Option<&str>) -> Result<Snapshot> {
    let path = match file {
        Some(file) => file.to_path_buf(),
        None => store.snapshot_path(name)?,
    };
    let snapshot = if name.is_some() {
        let snapshot: Snapshot = store::read_json(&path)?;
        snapshot.validate()?;
        snapshot
    } else {
        store.read_snapshot(&path)?
    };
    ensure!(
        matches!(
            (&snapshot.scope, name),
            (Scope::Session, None) | (Scope::Space { .. }, Some(_))
        ),
        "snapshot scope does not match operation"
    );
    Ok(snapshot)
}

#[allow(clippy::too_many_arguments)]
fn manual_restore(
    host: &mut impl Host,
    store: &Store,
    socket: &Path,
    config_dir: &Path,
    config: &Config,
    file: Option<&Path>,
    dry_run: bool,
    rehydrate_only: bool,
) -> Result<Value> {
    let saved = read_selected(store, file, None)?;
    let live = host.snapshot()?;
    let plan = planner::plan(&saved, &store.session, &live, config)?;
    let mut missing = saved.clone();
    missing.layout.retain(|w| {
        !rehydrate_only
            && !live
                .workspaces
                .iter()
                .any(|l| l.workspace_id == w.workspace_id)
    });
    missing.panes.retain(|p| {
        missing
            .layout
            .iter()
            .any(|w| w.workspace_id == p.workspace_id)
    });
    missing.focused_workspace_id = missing
        .focused_workspace_id
        .filter(|id| missing.layout.iter().any(|w| &w.workspace_id == id));
    let reconstruction = if missing.layout.is_empty() {
        None
    } else {
        Some(crate::layout::preview(&missing, config)?)
    };
    if dry_run {
        return Ok(
            json!({"plan":plan, "reconstruction":reconstruction, "execution_revalidation_required":true}),
        );
    }
    ensure!(
        reconstruction.is_none() || host.supports_layout(),
        "{CORE_UNSUPPORTED}"
    );
    let mut result = restore(host, store, socket, config_dir, config, file, None, false)?;
    if reconstruction.is_some() {
        result["reconstruction"] =
            crate::layout::rebuild(host, store, socket, config_dir, &missing, true)?;
    }
    Ok(result)
}

fn status(
    host: &mut impl Host,
    store: &Store,
    config_dir: &Path,
    config: &Config,
    kind: HostKind,
) -> Result<Value> {
    let latest_path = store.snapshot_path(None)?;
    let (latest, panes) = match maybe_json::<Snapshot>(&latest_path)? {
        Some(saved) => {
            saved.validate()?;
            ensure!(
                saved.session == store.session,
                "cross-session snapshot refused"
            );
            let plan = planner::plan(&saved, &store.session, &host.snapshot()?, config)?;
            let panes: Vec<Value> = plan
                .entries
                .iter()
                .map(|entry| {
                    let pane = saved.panes.iter().find(|p| p.pane_id == entry.pane_id);
                    json!({
                        "pane_id": entry.pane_id,
                        "terminal_id": entry.terminal_id,
                        "decision": entry.decision,
                        "cwd": pane.map(|p| p.cwd.clone()),
                        "program": pane
                            .and_then(|p| p.command.as_ref())
                            .and_then(|command| command.argv().ok())
                            .and_then(|argv| argv.first().cloned()),
                    })
                })
                .collect();
            (
                Some(json!({
                    "path": latest_path,
                    "created_ms": saved.created_ms,
                    "panes": saved.panes.len(),
                    "commands": saved.panes.iter().filter(|p| p.command.is_some()).count(),
                })),
                Some(panes),
            )
        }
        None => (None, None),
    };
    let install = match kind {
        HostKind::Core => crate::core::install::inspect(&crate::core::paths::home_dir()?),
        HostKind::Herdr => Value::Null,
    };
    Ok(json!({
        "host": kind,
        "session": store.session,
        "latest": latest,
        "panes": panes,
        "snapshots": store.list("snapshots")?,
        "recovery": {
            "pending": pending(store)?,
            "rebuild": crate::layout::pending(store)?,
            "boots": store.list("boots")?,
        },
        "config": {
            "dir": config_dir,
            "present": config_dir.join("config.toml").exists(),
            "auto_save": config.auto_save,
            "auto_restore": config.auto_restore,
            "allowed_programs": config.allowed_programs,
        },
        "install": install,
    }))
}

#[allow(clippy::too_many_arguments)]
fn restore(
    host: &mut impl Host,
    store: &Store,
    socket: &Path,
    config_dir: &Path,
    config: &Config,
    file: Option<&Path>,
    name: Option<&str>,
    automatic: bool,
) -> Result<Value> {
    require_clear(store)?;
    let generation = platform::generation(socket)?;
    if automatic && let Some(record) = boot(store, &generation)? {
        ensure!(
            record.state == BootState::Done,
            "unfinished boot record requires inspection"
        );
        return Ok(json!({"status": "already_claimed"}));
    }
    let saved = read_selected(store, file, name)?;
    std::thread::sleep(std::time::Duration::from_millis(config.settle_ms));
    let plan = planner::plan(&saved, &store.session, &host.snapshot()?, config)?;
    let claim = if boot(store, &generation)?.is_some() {
        store::digest(
            format!(
                "manual:{generation}:{}:{}",
                store::now_ms()?,
                std::process::id()
            )
            .as_bytes(),
        )
    } else {
        generation.clone()
    };
    let record = execute(
        host,
        store,
        &saved,
        &plan,
        &claim,
        |_, saved, entry, host| {
            ensure!(
                platform::generation(socket)? == generation,
                "server changed during restore"
            );
            let current_config = store::load_config(config_dir)?;
            ensure!(
                !automatic || current_config.auto_restore,
                "automatic restore disabled during execution"
            );
            let argv = saved
                .command
                .as_ref()
                .context("missing command")?
                .allowed_argv(&current_config)?;
            let live = host.pane(&saved.pane_id)?;
            ensure!(
                live.workspace_id == saved.workspace_id
                    && live.tab_id == saved.tab_id
                    && Some(&live.terminal_id) == entry.terminal_id.as_ref(),
                "pane identity changed during restore"
            );
            // Agent metadata can survive a Herdr restart after the process has
            // gone away. Treat the process tree as authoritative: a real
            // running agent makes the shell non-idle, while a stale label must
            // not prevent the saved session from being resumed.
            let info = host.process_info(&saved.pane_id)?;
            let Some(shell) = platform::idle_shell(&info)? else {
                return Ok(None);
            };
            ensure!(
                platform::generation(socket)? == generation,
                "server changed before run"
            );
            Ok(Some(platform::shell_command(&shell, &saved.cwd, &argv)?))
        },
    )?;
    Ok(json!({"status": "restored", "journal": record}))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Mapping {
    source_pane_id: String,
    workspace_id: String,
    tab_id: String,
    pane_id: String,
}

fn import_space(
    store: &Store,
    host: &mut impl Host,
    name: &str,
    file: &Path,
    mapping: Option<&Path>,
    config: &Config,
) -> Result<Value> {
    require_clear(store)?;
    store::validate_name(name)?;
    let mut saved: Snapshot = store::read_json(file)?;
    saved.validate()?;
    ensure!(
        matches!(saved.scope, Scope::Space { .. }),
        "only explicit native space imports are supported"
    );
    let Some(mapping) = mapping else {
        store.save(&saved, Some(name), config.retention)?;
        return Ok(json!({"status":"imported_without_execution"}));
    };
    let maps: Vec<Mapping> = store::read_json(mapping)?;
    ensure!(
        maps.len() == saved.panes.len() && !maps.is_empty(),
        "mapping must cover every source pane exactly once"
    );
    let live = host.snapshot()?;
    let index = planner::index_live(&live)?;
    let mut sources = std::collections::BTreeMap::new();
    for map in &maps {
        ensure!(
            sources.insert(map.source_pane_id.as_str(), map).is_none(),
            "duplicate source mapping"
        );
    }
    for pane in &mut saved.panes {
        let map = sources
            .get(pane.pane_id.as_str())
            .context("mapping source not found")?;
        let target = index
            .get(map.pane_id.as_str())
            .context("mapping target not found")?;
        ensure!(
            target.workspace_id == map.workspace_id && target.tab_id == map.tab_id,
            "mapping target ancestry mismatch"
        );
        pane.workspace_id = map.workspace_id.clone();
        pane.tab_id = map.tab_id.clone();
        pane.pane_id = map.pane_id.clone();
    }
    saved.session = store.session.clone();
    saved.layout = crate::layout::capture(host, &live, Some(&maps[0].workspace_id))?;
    saved.scope = Scope::Space {
        workspace_id: maps[0].workspace_id.clone(),
    };
    saved.created_ms = store::now_ms()?;
    saved.validate()?;
    store.save(&saved, Some(name), config.retention)?;
    Ok(json!({"status": "imported_without_execution"}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_paths_imply_the_herdr_host_outside_a_plugin() {
        // Only meaningful when neither variable is ambient in the test run.
        if std::env::var_os("HERDR_PLUGIN_STATE_DIR").is_some()
            || std::env::var_os("HERDR_REVIVE_HOST").is_some()
        {
            return;
        }
        assert!(HostKind::resolve(None, false).is_err());
        assert_eq!(HostKind::resolve(None, true).unwrap(), HostKind::Herdr);
        assert_eq!(
            HostKind::resolve(Some(HostKind::Core), true).unwrap(),
            HostKind::Core
        );
    }
}
