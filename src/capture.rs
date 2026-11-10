use crate::model::*;
use crate::platform;
use crate::transport::Host;
use anyhow::{Context, Result, ensure};
use std::path::PathBuf;

/// Panes whose foreground program is protected, by pane id, split by
/// whether this capture proved the command it saved for them.
#[derive(Debug, Default)]
pub struct Protected {
    pub unproven: std::collections::HashMap<String, Running>,
    pub proven: Proven,
}

/// Commands proven for protected processes, by pane id, kept between saves
/// (`protected.json`): a later save that cannot prove the command again
/// reuses it only while the very same process runs there.
pub type Proven = std::collections::BTreeMap<String, ProvenCommand>;

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProvenCommand {
    /// pid, parent, group and start of the process the proof was made for.
    pub identity: [u64; 4],
    pub argv: Vec<String>,
    /// Where it ran when proven. Carried with the argv: a job resumed with
    /// `fg` after a `cd` runs elsewhere than its shell now is.
    #[serde(default)]
    pub cwd: Option<String>,
}

/// A protected process capture could not prove: an earlier proof carries
/// to it only while it is the same process.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Running {
    pub identity: [u64; 4],
}

pub fn identity_of(process: &platform::Process) -> [u64; 4] {
    [
        u64::from(process.pid),
        u64::from(process.parent),
        u64::from(process.group),
        process.start,
    ]
}

pub fn capture(
    host: &mut impl Host,
    session: &str,
    workspace: Option<&str>,
    config: &Config,
) -> Result<(Snapshot, Protected)> {
    let mut warnings = Vec::new();
    let captured = attempts(host, session, workspace, config, &mut warnings, true);
    for warning in &warnings {
        eprintln!("herdr-revive: warning: {warning}");
    }
    captured
}

/// `capture` for a caller that reports the warnings itself (migrate puts
/// them in its report), without those of a discarded attempt.
pub fn capture_noting(
    host: &mut impl Host,
    session: &str,
    workspace: Option<&str>,
    config: &Config,
) -> Result<(Snapshot, Protected, Vec<String>)> {
    let mut warnings = Vec::new();
    let (snapshot, protected) = attempts(host, session, workspace, config, &mut warnings, false)?;
    Ok((snapshot, protected, warnings))
}

fn attempts(
    host: &mut impl Host,
    session: &str,
    workspace: Option<&str>,
    config: &Config,
    warnings: &mut Vec<String>,
    keep_discarded: bool,
) -> Result<(Snapshot, Protected)> {
    let start = warnings.len();
    if let Some(captured) = capture_once(host, session, workspace, config, warnings)? {
        return Ok(captured);
    }
    if !keep_discarded {
        warnings.truncate(start);
    }
    // A metadata wait can span unrelated pane/layout changes, and a program
    // can start between two process-table reads. Discard the first capture
    // and take a fresh process table and complete host view.
    capture_once(host, session, workspace, config, warnings)?.context(
        "panes kept changing during capture (agent metadata or a starting program); retry capture",
    )
}

fn capture_once(
    host: &mut impl Host,
    session: &str,
    workspace: Option<&str>,
    config: &Config,
    warnings: &mut Vec<String>,
) -> Result<Option<(Snapshot, Protected)>> {
    let live = host.snapshot()?;
    crate::planner::index_live(&live)?;
    if let Some(id) = workspace {
        ensure!(
            live.workspaces.iter().any(|w| w.workspace_id == id),
            "named workspace is not present"
        );
    }
    let mut table = platform::process_table()?;
    let mut listings = ClaudeListings::default();
    let mut panes = Vec::new();
    let mut protected_panes = Protected::default();
    for pane in live
        .panes
        .iter()
        .filter(|p| workspace.is_none_or(|id| id == p.workspace_id))
    {
        let info = host.process_info(&pane.pane_id)?;
        let shell = info.shell_pid.context("capture requires shell PID")?;
        let fg = info
            .foreground_process_group_id
            .context("foreground process group unavailable")?;
        let shell_process = table
            .get(&shell)
            .context("shell is absent from process table")?
            .clone();
        ensure!(
            !info.foreground_processes.is_empty(),
            "foreground process information unavailable"
        );
        ensure!(
            platform::process(shell)? == shell_process,
            "shell identity changed during capture"
        );
        let mut cwd = pane.cwd.clone().context("pane cwd unavailable")?;
        let command = if platform::is_shell_process(shell)?
            && fg == shell_process.group
            && info.foreground_processes.iter().all(|p| p.pid == shell)
        {
            None
        } else {
            let leader = info
                .foreground_processes
                .iter()
                .find(|p| p.pid == fg)
                .context("foreground group leader unavailable; cannot capture unambiguously")?;
            // The core host looks the foreground up in a process table it
            // reads after this one, so a program that started in between is
            // not here yet: read the table again (a pane running a loop of
            // short commands would miss every time a whole capture is
            // retried), and only then capture again.
            if !table.contains_key(&leader.pid) {
                table = platform::process_table()?;
            }
            let Some(identity) = table.get(&leader.pid) else {
                return Ok(None);
            };
            platform::validate_foreground_tree(&table, identity)?;
            let (argv, process_cwd) = match platform::argv_and_cwd(identity) {
                Ok(found) => found,
                Err(error) if permission_denied(&error) => {
                    // A setuid program such as Apple's top hides its identity
                    // from an unprivileged reader. What the platform still
                    // reveals, or the line the hook typed, stands in; for its
                    // unreadable cwd, its shell's, which the program started
                    // in and the shell cannot change while it runs. (On
                    // herdr-core the pane's own cwd is the foreground's folder
                    // only when core could read it, which it cannot for this
                    // program; it is then where the terminal was created.)
                    // A detected agent is never relaunched this
                    // way: a plain program would start a fresh session.
                    // When even the shell's cwd cannot be read, no command is
                    // saved: the terminal's own cwd would run it elsewhere.
                    let shell_cwd_known = match platform::argv_and_cwd(&shell_process) {
                        Ok((_, shell_cwd)) => {
                            cwd = shell_cwd;
                            true
                        }
                        Err(_) => false,
                    };
                    let agent = pane.agent.as_deref().and_then(AgentKind::from_name);
                    let on_terminal = |pid| {
                        platform::terminal_stdio(pid, shell).is_ok_and(|null| null == [false; 3])
                    };
                    let streams_on_terminal = on_terminal(identity.pid);
                    // The typed line says nothing about a redirection the shell
                    // itself carries (`exec >file`), so it only counts when the
                    // shell's own streams are the terminal.
                    let hint = host
                        .command_hint(&pane.pane_id)
                        .filter(|_| on_terminal(shell));
                    let protected = platform::protected_argv(identity).ok();
                    let command = match &protected {
                        Some((argv, exact)) if agent.is_none() && shell_cwd_known => {
                            protected_command(
                                argv.clone(),
                                *exact,
                                hint.as_deref(),
                                streams_on_terminal,
                            )
                            .map(|argv| CommandSpec::Program { argv })
                            // An argv the snapshot would refuse fails every save.
                            .filter(|command| command.argv().is_ok())
                        }
                        _ => None,
                    };
                    match &command {
                        Some(CommandSpec::Program { argv }) => {
                            protected_panes.proven.insert(
                                pane.pane_id.clone(),
                                ProvenCommand {
                                    identity: identity_of(identity),
                                    argv: argv.clone(),
                                    cwd: Some(cwd.clone()),
                                },
                            );
                        }
                        // Unproven even when the platform revealed nothing:
                        // a protected program runs there, not an earlier agent.
                        _ if agent.is_none() => {
                            protected_panes.unproven.insert(
                                pane.pane_id.clone(),
                                Running {
                                    identity: identity_of(identity),
                                },
                            );
                        }
                        _ => {}
                    }
                    let saved = match &command {
                        Some(CommandSpec::Program { argv }) => format!("{argv:?}"),
                        _ => "no command".to_string(),
                    };
                    warnings.push(format!(
                        "pane {}: foreground process {} is protected ({error:#}); saved with {saved} in the pane's cwd",
                        pane.pane_id, identity.pid
                    ));
                    panes.push(SavedPane {
                        workspace_id: pane.workspace_id.clone(),
                        tab_id: pane.tab_id.clone(),
                        pane_id: pane.pane_id.clone(),
                        cwd,
                        command,
                    });
                    continue;
                }
                Err(error) => return Err(error),
            };
            let mut null_stdio = match platform::terminal_stdio(identity.pid, shell) {
                Ok(streams) => streams,
                Err(_)
                    if std::path::Path::new(&argv[0])
                        .file_name()
                        .is_some_and(|name| name == "git") =>
                {
                    platform::git_pager_stdio(&table, identity, shell)
                        .with_context(|| format!("pane {} Git pager streams", pane.pane_id))?;
                    [false; 3]
                }
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("pane {} standard streams", pane.pane_id));
                }
            };
            if is_ssh(&argv[0]) && null_stdio[1] {
                ensure!(!null_stdio[2], "SSH stderr redirection is unsupported");
                platform::ssh_terminal_stdout(identity.pid)
                    .with_context(|| format!("pane {} SSH streams", pane.pane_id))?;
                ensure!(
                    platform::process(identity.pid)? == *identity,
                    "SSH process changed during capture"
                );
                null_stdio[1] = false;
            }
            cwd = process_cwd;
            // Only an agent revive can resume must come from a launcher it can
            // parse; a pane reporting some other agent (the file-annotator review
            // pane reports "annotator") is captured as the program it runs.
            let resumable = pane.agent.as_deref().and_then(AgentKind::from_name);
            if resumable.is_some() && agent_launcher(&argv).is_none() {
                anyhow::bail!(
                    "detected agent uses an unsupported launcher; exact resume cannot be established"
                );
            }
            // A resolved attach names the session its client opened. herdr's
            // record for such a pane comes from the daemon's hooks, which report
            // to whichever pane started the daemon, so it does not override the
            // attach. Unresolved, herdr's record is used as before when present.
            let attached = match resolve_claude_attach(
                &argv,
                identity,
                &mut listings,
                std::time::Duration::from_millis(config.timeout_ms),
            ) {
                Ok(resolved) => resolved,
                Err(error) if pane.agent_session.is_some() => {
                    warnings.push(format!(
                        "pane {}: {error:#}; using herdr's session record",
                        pane.pane_id
                    ));
                    None
                }
                Err(error) => return Err(error),
            };
            let (mut command, native_session, refreshed) = match &attached {
                Some(resolved) => (command_for(resolved, None)?, false, false),
                None => command_with_metadata_retry(host, pane, identity, &argv, &cwd, config)?,
            };
            if refreshed {
                return Ok(None);
            }
            if let Some(CommandSpec::Agent {
                agent,
                executable,
                session_id,
                session_mode,
                claude_config_dir: saved_claude_config_dir,
                claude_home: saved_claude_home,
                ..
            }) = &mut command
            {
                let launchers: Vec<_> = config
                    .agent_launchers
                    .iter()
                    .filter(|launcher| launcher.agent == *agent)
                    .collect();
                let detect_empty_claude = *agent == AgentKind::Claude
                    && (native_session || attached.is_some())
                    && !has_explicit_resume(&argv);
                let environment = if !launchers.is_empty() || detect_empty_claude {
                    Some(platform::environment(identity)?)
                } else {
                    None
                };
                if let Some(environment) = environment.as_deref()
                    && !launchers.is_empty()
                    && let Some(launcher) = matching_launcher(&launchers, environment)?
                {
                    *executable = launcher.executable.clone();
                }
                if detect_empty_claude {
                    let (config_dir, home) = claude_profile(
                        environment
                            .as_deref()
                            .context("Claude environment is unavailable")?,
                    )?;
                    // An attach can be typed in any directory, so its session's
                    // transcript need not be under the pane's cwd project.
                    let transcript_exists = if attached.is_some() {
                        claude_transcript_exists(&config_dir, session_id)?
                    } else {
                        match claude_project_transcript_exists(
                            &config_dir,
                            std::path::Path::new(&cwd),
                            session_id,
                        )? {
                            Some(exists) if exists || !has_implicit_resume(&argv) => exists,
                            _ => claude_transcript_exists(&config_dir, session_id)?,
                        }
                    };
                    if !transcript_exists {
                        let (configured_dir, configured_home) =
                            configured_claude_profile(config, executable)?;
                        ensure!(
                            config_dir == configured_dir && home == configured_home,
                            "Claude configuration profile cannot be reproduced; select the launcher with CLAUDE_CONFIG_DIR"
                        );
                        *session_mode = AgentSessionMode::Create;
                        *saved_claude_config_dir = Some(
                            config_dir
                                .to_str()
                                .context("non-UTF-8 Claude configuration path")?
                                .into(),
                        );
                        *saved_claude_home = home
                            .map(|home| {
                                home.into_os_string()
                                    .into_string()
                                    .map_err(|_| anyhow::anyhow!("non-UTF-8 Claude HOME"))
                            })
                            .transpose()?;
                    }
                }
            }
            if null_stdio.iter().any(|value| *value) {
                match command {
                    Some(CommandSpec::Program { argv }) => {
                        Some(CommandSpec::ProgramNullStdio { argv, null_stdio })
                    }
                    Some(CommandSpec::Agent {
                        executable,
                        agent,
                        session_id,
                        session_mode,
                        claude_config_dir,
                        claude_home,
                        ..
                    }) => Some(CommandSpec::Agent {
                        executable,
                        agent,
                        session_id,
                        session_mode,
                        claude_config_dir,
                        claude_home,
                        null_stdio,
                    }),
                    _ => anyhow::bail!("redirected streams are unsupported"),
                }
            } else {
                command
            }
        };
        panes.push(SavedPane {
            workspace_id: pane.workspace_id.clone(),
            tab_id: pane.tab_id.clone(),
            pane_id: pane.pane_id.clone(),
            cwd,
            command,
        });
    }
    panes.sort_by(|a, b| a.pane_id.cmp(&b.pane_id));
    let layout = if host.supports_layout() {
        crate::layout::capture(host, &live, workspace)?
    } else {
        Vec::new()
    };
    let snapshot = Snapshot {
        tool: TOOL.into(),
        schema: SCHEMA,
        session: session.into(),
        created_ms: crate::store::now_ms()?,
        scope: workspace.map_or(Scope::Session, |id| Scope::Space {
            workspace_id: id.into(),
        }),
        panes,
        layout,
        focused_workspace_id: live.focused_workspace_id,
    };
    snapshot.validate()?;
    Ok(Some((snapshot, protected_panes)))
}

/// The agent session metadata of a pane did not arrive in time. `save`
/// says that the previous snapshot is kept; migrate, which keeps none, does
/// not.
#[derive(Debug)]
pub struct MetadataTimeout(pub String);

impl std::fmt::Display for MetadataTimeout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "pane {}: agent session metadata did not arrive", self.0)
    }
}

fn command_with_metadata_retry(
    host: &mut impl Host,
    pane: &LivePane,
    identity: &platform::Process,
    argv: &[String],
    cwd: &str,
    config: &Config,
) -> Result<(Option<CommandSpec>, bool, bool)> {
    let mut initial = command_for(argv, pane.agent_session.as_ref());
    let expected = detected_session_conflict(pane, argv)?;
    if expected.is_some() {
        initial = Err(anyhow::anyhow!(
            "new agent invocation conflicts with retained native session"
        ));
    } else if initial.is_ok() || pane.agent_session.is_some() || agent_launcher(argv).is_none() {
        return initial.map(|command| (command, pane.agent_session.is_some(), false));
    }
    let deadline =
        std::time::Instant::now() + std::time::Duration::from_millis(config.timeout_ms.min(1000));
    loop {
        let current = host.pane(&pane.pane_id)?;
        ensure!(
            current.terminal_id == pane.terminal_id
                && current.workspace_id == pane.workspace_id
                && current.tab_id == pane.tab_id,
            "pane changed while waiting for agent session metadata"
        );
        let info = host.process_info(&pane.pane_id)?;
        ensure!(
            info.foreground_process_group_id == Some(identity.group)
                && platform::process(identity.pid)? == *identity,
            "agent changed while waiting for session metadata"
        );
        if let Some(native) = current.agent_session
            && expected.as_ref().is_none_or(|id| *id == native.value)
        {
            let (current_argv, current_cwd) = platform::argv_and_cwd(identity)?;
            ensure!(
                current_argv == argv && current_cwd == cwd,
                "agent arguments changed during metadata retry"
            );
            platform::validate_foreground_tree(&platform::process_table()?, identity)?;
            let confirmed = host.pane(&pane.pane_id)?;
            ensure!(
                confirmed.terminal_id == pane.terminal_id
                    && confirmed.agent_session.as_ref() == Some(&native),
                "agent session changed during metadata retry; retry capture"
            );
            return command_for(argv, Some(&native)).map(|command| (command, true, true));
        }
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return initial
                .with_context(|| MetadataTimeout(pane.pane_id.clone()))
                .map(|command| (command, false, false));
        }
        std::thread::sleep(remaining.min(std::time::Duration::from_millis(50)));
    }
}

fn detected_session_conflict(pane: &LivePane, argv: &[String]) -> Result<Option<String>> {
    let Some(native) = &pane.agent_session else {
        return Ok(None);
    };
    if std::env::var("HERDR_PLUGIN_EVENT").as_deref() != Ok("pane.agent_detected") {
        return Ok(None);
    }
    let Ok(Some(CommandSpec::Agent { session_id, .. })) = command_for(argv, None) else {
        return Ok(None);
    };
    if session_id == native.value {
        return Ok(None);
    }
    let Ok(payload) = std::env::var("HERDR_PLUGIN_EVENT_JSON") else {
        return Ok(None);
    };
    ensure!(
        payload.len() <= MAX_BYTES,
        "event payload exceeds size limit"
    );
    let payload: serde_json::Value =
        serde_json::from_str(&payload).context("invalid agent event payload")?;
    // Scope this guard to a new invocation, not an in-process /resume whose
    // original launch argv legitimately names an older conversation.
    Ok((payload
        .pointer("/data/pane_id")
        .and_then(serde_json::Value::as_str)
        == Some(pane.pane_id.as_str()))
    .then_some(session_id))
}

pub fn matching_launcher<'a>(
    launchers: &[&'a AgentLauncher],
    environment: &[u8],
) -> Result<Option<&'a AgentLauncher>> {
    let mut matched = None;
    for launcher in launchers {
        let mut matches = true;
        for (key, value) in &launcher.match_env {
            let actual = environment_value(environment, key)?;
            matches &= actual == Some(value.as_bytes());
        }
        if matches {
            ensure!(
                matched.is_none(),
                "ambiguous agent launcher environment; capture refused"
            );
            matched = Some(*launcher);
        }
    }
    Ok(matched)
}

fn environment_value<'a>(environment: &'a [u8], key: &str) -> Result<Option<&'a [u8]>> {
    let prefix = format!("{key}=");
    let mut values = environment
        .split(|byte| *byte == 0)
        .filter_map(|entry| entry.strip_prefix(prefix.as_bytes()));
    let value = values.next();
    ensure!(
        values.next().is_none(),
        "duplicate agent environment key; capture refused"
    );
    Ok(value)
}

fn claude_profile(environment: &[u8]) -> Result<(PathBuf, Option<PathBuf>)> {
    let (path, home) = if let Some(value) = environment_value(environment, "CLAUDE_CONFIG_DIR")? {
        (
            PathBuf::from(
                std::str::from_utf8(value).context("non-UTF-8 Claude configuration path")?,
            ),
            None,
        )
    } else {
        let home = environment_value(environment, "HOME")?.context("Claude HOME is unavailable")?;
        let home = PathBuf::from(std::str::from_utf8(home).context("non-UTF-8 Claude HOME")?);
        ensure!(home.is_absolute(), "Claude HOME must be absolute");
        let home = home.canonicalize().context("Claude HOME is inaccessible")?;
        (home.join(".claude"), Some(home))
    };
    ensure!(
        path.is_absolute(),
        "Claude configuration directory must be absolute"
    );
    Ok((
        path.canonicalize()
            .context("Claude configuration directory is inaccessible")?,
        home,
    ))
}

fn has_explicit_resume(argv: &[String]) -> bool {
    has_claude_option(argv, |arg| {
        matches!(arg, "--resume" | "-r")
            || arg.starts_with("--resume=")
            || arg.starts_with("-r=")
            || arg
                .strip_prefix("-r")
                .is_some_and(|id| validate_session_id(id).is_ok())
    })
}

/// A Claude session handed off to the background is reopened as
/// `claude attach <id>`, where `<id>` is the short id `claude agents --json`
/// lists. Map it to the exact session and return the invocation as
/// `--resume <uuid>`, which reattaches while the session still runs in the
/// background and resumes it once stopped. A name instead of an id is refused:
/// attach matches names by substring, so it does not identify one session.
/// The listing is per Claude profile, so it is read with the pane process's
/// CLAUDE_CONFIG_DIR and HOME, and with --all so a session whose background
/// process has ended is still found while its attach client stays open.
pub fn resolve_claude_attach(
    argv: &[String],
    identity: &platform::Process,
    listings: &mut ClaudeListings,
    timeout: std::time::Duration,
) -> Result<Option<Vec<String>>> {
    if agent_launcher(argv) != Some(AgentKind::Claude) {
        return Ok(None);
    }
    let start = if is_launcher(&argv[0]) { 2 } else { 1 };
    let args = &argv[start..];
    if args.first().map(String::as_str) != Some("attach") {
        return Ok(None);
    }
    ensure!(
        args.len() == 2,
        "claude attach must name exactly one session"
    );
    let id = &args[1];
    let session = if validate_session_id(id).is_ok() {
        id.clone()
    } else {
        ensure!(
            id.len() == 8 && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')),
            "claude attach by name cannot be resolved to an exact session"
        );
        let environment = platform::environment(identity)?;
        let profile: Vec<(&str, Option<Vec<u8>>)> = ["CLAUDE_CONFIG_DIR", "HOME"]
            .into_iter()
            .map(|key| {
                Ok((
                    key,
                    environment_value(&environment, key)?.map(<[u8]>::to_vec),
                ))
            })
            .collect::<Result<_>>()?;
        let mut program = argv[..start].to_vec();
        program[0] = on_pane_path(&program[0], environment_value(&environment, "PATH")?)?;
        let listing = listings
            .get(&program, &profile, timeout)
            .context("claude agents --json --all")?;
        let entries: Vec<serde_json::Value> =
            serde_json::from_slice(listing).context("invalid claude agents listing")?;
        let mut sessions: Vec<&str> = entries
            .iter()
            .filter(|entry| entry["id"].as_str() == Some(id.as_str()))
            .filter_map(|entry| entry["sessionId"].as_str())
            .collect();
        sessions.sort_unstable();
        sessions.dedup();
        ensure!(
            sessions.len() == 1,
            "claude agents lists {} sessions for attach id {id}",
            sessions.len()
        );
        sessions[0].to_owned()
    };
    validate_session_id(&session)?;
    let mut resolved = argv[..start].to_vec();
    resolved.extend(["--resume".to_owned(), session]);
    Ok(Some(resolved))
}

/// A bare program name is looked up on the pane's PATH, not revive's: herdr
/// may run plugins with a PATH that lacks the user's own bin directories.
/// Relative PATH entries are skipped, since they name the pane's cwd.
fn on_pane_path(name: &str, path: Option<&[u8]>) -> Result<String> {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::PermissionsExt;
    if name.contains('/') {
        return Ok(name.to_owned());
    }
    let path = path.with_context(|| format!("pane has no PATH to find {name} on"))?;
    for dir in path.split(|byte| *byte == b':') {
        let dir = std::path::Path::new(std::ffi::OsStr::from_bytes(dir));
        if !dir.is_absolute() {
            continue;
        }
        let candidate = dir.join(name);
        if candidate
            .metadata()
            .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        {
            return candidate
                .into_os_string()
                .into_string()
                .map_err(|_| anyhow::anyhow!("{name} resolves to a non-UTF-8 path"));
        }
    }
    anyhow::bail!("{name} is not on the pane's PATH")
}

type Profile<'a> = [(&'a str, Option<Vec<u8>>)];
type ListingKey = (Vec<String>, Vec<(String, Option<Vec<u8>>)>);

/// Listings already read during one capture pass. Each query costs a process
/// spawn while the session lock is held and attach panes usually share one
/// profile, so a pass reads each launcher and profile once. The cache must not
/// outlive the pass: the timer captures repeatedly in one process, and a session
/// handed off after an earlier pass would be missing from a stale listing.
#[derive(Default)]
pub struct ClaudeListings(Vec<(ListingKey, Vec<u8>)>);

impl ClaudeListings {
    fn get(
        &mut self,
        program: &[String],
        profile: &Profile,
        timeout: std::time::Duration,
    ) -> Result<&[u8]> {
        let key: ListingKey = (
            program.to_vec(),
            profile
                .iter()
                .map(|(k, v)| ((*k).to_owned(), v.clone()))
                .collect(),
        );
        let index = match self.0.iter().position(|(cached, _)| *cached == key) {
            Some(index) => index,
            None => {
                let listing =
                    run_bounded(program, &["agents", "--json", "--all"], profile, timeout)?;
                self.0.push((key, listing));
                self.0.len() - 1
            }
        };
        Ok(&self.0[index].1)
    }
}

/// Runs a short read-only query and returns its stdout, killing it past the
/// deadline. HERDR_* is stripped: if the query starts Claude's daemon, the
/// daemon must not inherit this plugin's pane and report sessions to it.
/// `profile` sets each variable to the pane's value, or removes it when the
/// pane had none.
fn run_bounded(
    program: &[String],
    args: &[&str],
    profile: &Profile,
    timeout: std::time::Duration,
) -> Result<Vec<u8>> {
    use nix::fcntl::{FcntlArg, OFlag, fcntl};
    use std::io::Read;
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;
    let mut command = std::process::Command::new(&program[0]);
    command.args(&program[1..]).args(args);
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("HERDR_") {
            command.env_remove(key);
        }
    }
    for (key, value) in profile {
        match value {
            Some(value) => command.env(key, std::ffi::OsStr::from_bytes(value)),
            None => command.env_remove(key),
        };
    }
    let mut child = command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .with_context(|| format!("cannot start {}", program[0]))?;
    let result = (|| {
        let mut stdout = child.stdout.take().context("stdout missing")?;
        fcntl(stdout.as_raw_fd(), FcntlArg::F_SETFL(OFlag::O_NONBLOCK))?;
        let deadline = std::time::Instant::now() + timeout;
        let mut output = Vec::new();
        let mut buffer = [0; 8192];
        loop {
            ensure!(std::time::Instant::now() < deadline, "deadline exceeded");
            match stdout.read(&mut buffer) {
                Ok(0) => {
                    if let Some(status) = child.try_wait()? {
                        ensure!(status.success(), "exited with {status}");
                        return Ok(output);
                    }
                }
                Ok(n) => {
                    output.extend_from_slice(&buffer[..n]);
                    ensure!(output.len() <= MAX_BYTES, "output exceeds size limit");
                    continue;
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                    ) => {}
                Err(e) => return Err(e.into()),
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    })();
    if result.is_err() {
        let _ = child.kill();
    }
    let _ = child.wait();
    result
}

fn has_implicit_resume(argv: &[String]) -> bool {
    has_claude_option(argv, |arg| {
        matches!(arg, "--continue" | "-c" | "--session-id") || arg.starts_with("--session-id=")
    })
}

fn has_claude_option(argv: &[String], matches: impl Fn(&str) -> bool) -> bool {
    let start = usize::from(is_launcher(&argv[0])) + 1;
    let mut index = start;
    while let Some(arg) = argv.get(index) {
        if arg == "--" || !arg.starts_with('-') {
            return false;
        }
        if matches(arg) {
            return true;
        }
        index += if agent_option_takes_value(AgentKind::Claude, arg) {
            2
        } else {
            1
        };
    }
    false
}

/// The argv to save for a protected program: the platform's exact argv when
/// it has one, else the hook's typed line when it names the same program and
/// is plain words, else the bare program name.
/// The file name of a program path, which is what a protected process's
/// name is compared by; macOS keeps only its first 15 characters.
pub fn program_name(program: &str) -> Option<String> {
    std::path::Path::new(program)
        .file_name()
        .and_then(|name| name.to_str())
        .map(str::to_string)
}

/// Whether `typed` (a file name) is the program the platform names `name`.
/// macOS keeps 15 characters of a longer name (MAXCOMLEN 16 with its NUL).
pub fn names_program(typed: &str, name: &str) -> bool {
    typed == name || (name.len() == 15 && typed.starts_with(name))
}

/// The command a protected program is saved with, or None when nothing
/// proves it runs without redirection: its streams cannot be read, so a
/// relaunch from argv alone could drop a `< file` or `> file` the user typed.
/// Proof is either `streams_on_terminal` (all three streams were read and
/// are the pane's terminal) or a typed line made of plain words, which
/// `shell_words` refuses when it holds any redirection. With an exact argv
/// the typed words must be that argv; with only a name they must name it.
fn protected_command(
    argv: Vec<String>,
    exact: bool,
    hint: Option<&str>,
    streams_on_terminal: bool,
) -> Option<Vec<String>> {
    // `command prog ...` (how a manual restore types a relaunch) runs prog
    // itself, past any function or alias: its words are prog's.
    let typed = hint
        .and_then(shell_words)
        .map(|words| match words.as_slice() {
            [first, next, ..] if first == "command" && !next.starts_with('-') => {
                words[1..].to_vec()
            }
            _ => words,
        });
    if exact {
        return (streams_on_terminal || typed.as_ref() == Some(&argv)).then_some(argv);
    }
    if let Some(words) = typed
        && program_name(&words[0]).is_some_and(|typed| names_program(&typed, &argv[0]))
    {
        return Some(words);
    }
    streams_on_terminal.then_some(argv)
}

/// Splits a command line the way the shell would when it holds only plain
/// words, single or double quotes and backslash escapes. Anything the shell
/// would expand or treat as syntax yields None: the exact argv is unknown.
fn shell_words(text: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                in_word = true;
                loop {
                    match chars.next()? {
                        '\'' => break,
                        other => word.push(other),
                    }
                }
            }
            '"' => {
                in_word = true;
                loop {
                    match chars.next()? {
                        '"' => break,
                        // Bash keeps the backslash unless it escapes one of
                        // these; `"\d"` stays two characters.
                        '\\' => match chars.next()? {
                            '\n' => {}
                            c @ ('$' | '`' | '"' | '\\') => word.push(c),
                            c => {
                                word.push('\\');
                                word.push(c);
                            }
                        },
                        '$' | '`' => return None,
                        other => word.push(other),
                    }
                }
            }
            '\\' => {
                in_word = true;
                word.push(chars.next()?);
            }
            ' ' | '\t' => {
                if in_word {
                    words.push(std::mem::take(&mut word));
                    in_word = false;
                }
            }
            '|' | '&' | ';' | '<' | '>' | '(' | ')' | '$' | '`' | '*' | '?' | '[' | ']' | '{'
            | '}' | '~' | '#' | '!' | '\n' => return None,
            other => {
                in_word = true;
                word.push(other);
            }
        }
    }
    if in_word {
        words.push(word);
    }
    if words.is_empty() || words[0].contains('=') {
        return None;
    }
    Some(words)
}

fn permission_denied(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == std::io::ErrorKind::PermissionDenied)
    })
}

pub fn command_for(argv: &[String], native: Option<&AgentSession>) -> Result<Option<CommandSpec>> {
    validate_argv(argv)?;
    let agent = agent_launcher(argv);
    if let Some(agent) = agent {
        let session_id = if let Some(native) = native {
            ensure!(
                native.source == format!("herdr:{}", agent.name())
                    && native.agent == agent.name()
                    && native.kind == "id",
                "untrusted native agent reference"
            );
            native.value.clone()
        } else {
            let start = if is_launcher(&argv[0]) { 2 } else { 1 };
            explicit_resume_id(agent, &argv[start..])?
        };
        validate_session_id(&session_id)?;
        return Ok(Some(CommandSpec::Agent {
            executable: if is_launcher(&argv[0]) {
                agent.executable().into()
            } else {
                argv[0].clone()
            },
            agent,
            session_id,
            session_mode: AgentSessionMode::Resume,
            claude_config_dir: None,
            claude_home: None,
            null_stdio: [false; 3],
        }));
    }
    // Never turn an unrelated foreground process into a stale stored agent session.
    let command = CommandSpec::Program {
        argv: argv.to_vec(),
    };
    command.argv()?;
    Ok(Some(command))
}

pub fn explicit_resume_id(agent: AgentKind, args: &[String]) -> Result<String> {
    let mut index = 0;
    let mut found = None;
    while index < args.len() {
        let arg = &args[index];
        let is_resume = arg == "--resume" || (agent == AgentKind::Codex && arg == "resume");
        if is_resume {
            let value = args.get(index + 1).context("resume option has no ID")?;
            validate_session_id(value)?;
            ensure!(
                found.replace(value.clone()).is_none(),
                "multiple resume IDs"
            );
            index += 2;
        } else if let Some(value) = arg.strip_prefix("--resume=") {
            validate_session_id(value)?;
            ensure!(found.replace(value.into()).is_none(), "multiple resume IDs");
            index += 1;
        } else if agent_option_takes_value(agent, arg) {
            let value = args.get(index + 1).context("agent option has no value")?;
            ensure!(
                !value.starts_with('-'),
                "agent option value cannot be another option"
            );
            index += 2;
        } else if agent_boolean_option(agent, arg) {
            index += 1;
        } else {
            anyhow::bail!("unknown agent option or prompt; exact resume ID cannot be parsed");
        }
    }
    found.context("agent has no explicit exact resume ID")
}

#[cfg(test)]
mod tests {
    #[test]
    fn shell_words_accepts_plain_words_and_quotes_only() {
        let words = |text: &str| super::shell_words(text);
        assert_eq!(
            words("top -o cpu -s 5"),
            Some(
                vec!["top", "-o", "cpu", "-s", "5"]
                    .into_iter()
                    .map(String::from)
                    .collect()
            )
        );
        assert_eq!(
            words(r#"sudo "a b" 'c d' e\ f"#),
            Some(
                vec!["sudo", "a b", "c d", "e f"]
                    .into_iter()
                    .map(String::from)
                    .collect()
            )
        );
        assert_eq!(words("  top  "), Some(vec!["top".to_string()]));
        for rejected in [
            "",
            "   ",
            "top | less",
            "top; ls",
            "top > out",
            "ls *",
            "echo $HOME",
            "echo \"$HOME\"",
            "FOO=1 top",
            "top &",
            "ls ~",
            "top 'unterminated",
            "top \\",
        ] {
            assert_eq!(words(rejected), None, "{rejected:?}");
        }
    }

    #[test]
    fn a_protected_command_needs_proof_that_it_runs_without_redirection() {
        let argv = |items: &[&str]| items.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let pc = super::protected_command;
        // Exact argv: proven by the streams, or by the same typed words.
        assert_eq!(
            pc(argv(&["sudo", "-i"]), true, None, true),
            Some(argv(&["sudo", "-i"]))
        );
        assert_eq!(
            pc(argv(&["sudo", "-i"]), true, Some("sudo -i"), false),
            Some(argv(&["sudo", "-i"]))
        );
        assert_eq!(
            pc(argv(&["sudo", "-i"]), true, Some("sudo -s"), false),
            None
        );
        assert_eq!(
            pc(
                argv(&["sudo", "dd", "of=/dev/sdb"]),
                true,
                Some("sudo dd of=/dev/sdb < disk.img"),
                false
            ),
            None
        );
        assert_eq!(pc(argv(&["ping", "host"]), true, None, false), None);
        // A manual restore types `command prog ...`: prog's own words.
        assert_eq!(
            pc(
                argv(&["ping", "host"]),
                true,
                Some("command 'ping' 'host'"),
                false
            ),
            Some(argv(&["ping", "host"]))
        );
        assert_eq!(
            pc(
                argv(&["top"]),
                false,
                Some("command 'top' '-o' 'cpu'"),
                false
            ),
            Some(argv(&["top", "-o", "cpu"]))
        );
        assert_eq!(
            pc(argv(&["top"]), false, Some("command -p top"), false),
            None
        );
        // Name only: a matching typed line supplies the arguments.
        assert_eq!(
            pc(argv(&["top"]), false, Some("top -o cpu -s 5"), false),
            Some(argv(&["top", "-o", "cpu", "-s", "5"]))
        );
        assert_eq!(
            pc(argv(&["top"]), false, Some("/usr/bin/top -o cpu"), false),
            Some(argv(&["/usr/bin/top", "-o", "cpu"]))
        );
        assert_eq!(
            pc(argv(&["top"]), false, Some("htop -d 5"), true),
            Some(argv(&["top"]))
        );
        assert_eq!(pc(argv(&["top"]), false, Some("htop -d 5"), false), None);
        assert_eq!(
            pc(argv(&["top"]), false, Some("top > top.log"), false),
            None
        );
        assert_eq!(
            pc(argv(&["top"]), false, Some("top | less"), true),
            Some(argv(&["top"]))
        );
        assert_eq!(pc(argv(&["top"]), false, None, true), Some(argv(&["top"])));
        assert_eq!(pc(argv(&["top"]), false, None, false), None);
        assert_eq!(
            pc(
                argv(&["averyveryverylong"]),
                false,
                Some("averyveryverylongname -x"),
                false
            ),
            None
        );
        assert_eq!(
            pc(
                argv(&["sixteencharsname"]),
                false,
                Some("sixteencharsname-plus -x"),
                false
            ),
            None,
            "macOS never reports 16 characters"
        );
        // macOS keeps the first 15 characters of a longer name.
        assert_eq!(
            pc(
                argv(&["abcdefghijklmno"]),
                false,
                Some("abcdefghijklmnopqrstu -x"),
                false
            ),
            Some(argv(&["abcdefghijklmnopqrstu", "-x"]))
        );
    }

    #[test]
    fn a_backslash_in_double_quotes_is_kept_unless_bash_would_drop_it() {
        let words = |text: &str| super::shell_words(text).unwrap();
        assert_eq!(words(r#"grep "\d+" f"#), ["grep", r"\d+", "f"]);
        assert_eq!(
            words(r#"echo "a\"b" "c\\d" "e\$f" "g\`h""#),
            ["echo", "a\"b", r"c\d", "e$f", "g`h"]
        );
        assert_eq!(words(r"echo \d"), ["echo", "d"]);
    }

    #[test]
    fn only_a_permission_error_anywhere_in_the_chain_is_a_protected_process() {
        use std::io::{Error, ErrorKind};
        let denied = anyhow::Error::from(Error::from(ErrorKind::PermissionDenied))
            .context("process identity unavailable");
        assert!(super::permission_denied(&denied));
        assert!(!super::permission_denied(&anyhow::anyhow!(
            "process changed during capture"
        )));
        assert!(!super::permission_denied(
            &anyhow::Error::from(Error::from(ErrorKind::NotFound)).context("gone")
        ));
    }

    use super::*;

    #[test]
    fn claude_resume_detection_respects_prompt_and_option_values() {
        let args = |values: &[&str]| {
            values
                .iter()
                .map(|value| (*value).into())
                .collect::<Vec<String>>()
        };
        assert!(has_explicit_resume(&args(&["claude", "--resume", "id"])));
        assert!(has_explicit_resume(&args(&["claude", "-r", "id"])));
        assert!(has_explicit_resume(&args(&[
            "claude",
            "-r01234567-89ab-cdef-0123-456789abcdef"
        ])));
        assert!(has_explicit_resume(&args(&[
            "claude",
            "-r=01234567-89ab-cdef-0123-456789abcdef"
        ])));
        assert!(!has_explicit_resume(&args(&["claude", "-rc"])));
        assert!(!has_explicit_resume(&args(&["claude", "--", "--resume"])));
        assert!(!has_explicit_resume(&args(&[
            "claude", "prompt", "--resume"
        ])));
        assert!(!has_explicit_resume(&args(&[
            "claude", "--model", "--resume"
        ])));
        assert!(has_implicit_resume(&args(&[
            "claude",
            "--session-id",
            "id"
        ])));
        assert!(!has_implicit_resume(&args(&["claude", "--", "--continue"])));
        assert!(has_explicit_resume(&args(&[
            "node",
            "claude",
            "--model",
            "sonnet",
            "--resume=id"
        ])));
    }
}
