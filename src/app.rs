use crate::model::*;
use crate::store::Store;
use crate::transport::{Host, Transport};
use crate::{capture, planner, platform, store};
use anyhow::{Context, Result, bail, ensure};
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
    #[command(subcommand)]
    pub command: Action,
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
        // Persist Sending before any operation that might write to a pane.
        journal.entries[i].outcome = Outcome::Sending;
        persist_pending(store, &journal)?;
        let result: Result<Outcome> = (|| {
            let Some(text) = revalidate(&mut journal.entries[i], pane, &plan.entries[i], host)?
            else {
                return Ok(Outcome::Skipped);
            };
            host.run(&pane.pane_id, &text)?;
            Ok(Outcome::Applied)
        })();
        match result {
            Ok(outcome) => {
                journal.entries[i].outcome = outcome;
                persist_pending(store, &journal)?;
            }
            Err(error) => {
                journal.entries[i].outcome = Outcome::Failed;
                journal.state = BootState::Failed;
                persist_pending(store, &journal)
                    .context("failed to persist failure; Sending evidence remains")?;
                return Err(error).context(
                    "restore stopped; inspect recovery evidence before any further save/restore",
                );
            }
        }
    }
    journal.state = BootState::Done;
    finish(store, &journal)?;
    Ok(journal)
}

pub fn run(cli: Cli) -> Result<Value> {
    if matches!(cli.command, Action::Timer { .. }) {
        return crate::timer::run(cli);
    }
    if matches!(cli.command, Action::Config) {
        return Ok(json!({"config": toml::to_string_pretty(&Config::default())?}));
    }
    if let Ok(id) = std::env::var("HERDR_PLUGIN_ID") {
        ensure!(id == PLUGIN_ID, "wrong plugin identity");
    }
    let config_dir = env_path(cli.config_dir, "HERDR_PLUGIN_CONFIG_DIR")?;
    let config = store::load_config(&config_dir)?;
    if matches!(cli.command, Action::Event) && !config.auto_save && !config.auto_restore {
        return Ok(json!({"status": "disabled"}));
    }
    if matches!(cli.command, Action::Autosave { force: false }) && !config.auto_save {
        return Ok(json!({"status": "disabled"}));
    }
    let state_dir = env_path(cli.state_dir, "HERDR_PLUGIN_STATE_DIR")?;
    let (socket, session) = store::session_identity(&env_path(cli.socket, "HERDR_SOCKET_PATH")?)?;
    let store = Store::new(&state_dir, session)?;
    let force_save = matches!(cli.command, Action::Autosave { force: true });
    let hook = matches!(cli.command, Action::Event | Action::Autosave { .. });
    let mut acquired = store.try_lock()?;
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
    let mut host = Transport::new(socket.clone(), binary, &config);
    let result = match cli.command {
        Action::Config | Action::Timer { .. } => unreachable!(),
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
            if !force_save && config.auto_restore && boot(&store, &generation)?.is_none() {
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
        json!({"result": result, "requests": host.requests, "herdr_children": host.children, "identity_connections": platform::identity_connections()}),
    )
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SaveTime {
    generation: String,
    time_ms: u64,
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
    let Some(native) = pane.agent_session else {
        // A detection event may precede the session hook. Capture performs a
        // bounded metadata retry; it never replaces the snapshot on failure.
        return Ok(pane.agent.is_some());
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

fn save(
    host: &mut impl Host,
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
    let mut retained_agents = 0;
    if automatic
        && snapshot.panes.iter().any(|pane| pane.command.is_none())
        && let Some(previous) = maybe_json::<Snapshot>(&store.snapshot_path(None)?)?
    {
        previous.validate()?;
        ensure!(
            previous.session == store.session,
            "cross-session snapshot refused"
        );
        retained_agents = retain_autosaved_agents(&mut snapshot, &previous);
    }
    ensure!(
        platform::generation(socket)? == generation,
        "server changed during capture"
    );
    store.save(&snapshot, name, config.retention)?;
    if name.is_none() {
        store::atomic_json(
            &store.root.join("last-save.json"),
            &SaveTime {
                generation,
                time_ms: store::now_ms()?,
            },
        )?;
    }
    Ok(
        json!({"status": "saved", "panes": snapshot.panes.len(), "retained_agents": retained_agents, "path": store.snapshot_path(name)?}),
    )
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
    let mut result = restore(host, store, socket, config_dir, config, file, None, false)?;
    if reconstruction.is_some() {
        result["reconstruction"] =
            crate::layout::rebuild(host, store, socket, config_dir, &missing, true)?;
    }
    Ok(result)
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
