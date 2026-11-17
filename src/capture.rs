// SPDX-FileCopyrightText: 2026 Su Kang Yin <paradyse@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use crate::model::*;
use crate::platform;
use crate::transport::Host;
use anyhow::{Context, Result, ensure};
use std::path::PathBuf;

/// Panes whose foreground program is protected, by pane id, split by
/// whether this capture proved the command it saved for them; and agent
/// panes saved without a command because no exact session could be found.
#[derive(Debug, Default)]
pub struct Protected {
    pub unproven: std::collections::HashMap<String, Running>,
    pub proven: Proven,
    /// A new agent runs in each of these panes: an agent session saved there
    /// earlier is not theirs, so `save` must not carry it over.
    pub unidentified: std::collections::HashSet<String>,
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
    attempt_twice(
        CLAUDE_RECORD_BUDGET,
        warnings,
        keep_discarded,
        |warnings, record_until| {
            capture_once(host, session, workspace, config, warnings, record_until)
        },
    )
}

/// Runs `once` and, when it discards its capture, once more. Both get the
/// same deadline for record waits: a second attempt with its own would let
/// a save hold the session lock for two.
fn attempt_twice<T>(
    budget: std::time::Duration,
    warnings: &mut Vec<String>,
    keep_discarded: bool,
    mut once: impl FnMut(&mut Vec<String>, std::time::Instant) -> Result<Option<T>>,
) -> Result<T> {
    let start = warnings.len();
    let record_until = std::time::Instant::now() + budget;
    if let Some(captured) = once(warnings, record_until)? {
        return Ok(captured);
    }
    if !keep_discarded {
        warnings.truncate(start);
    }
    // A metadata wait can span unrelated pane/layout changes, and a program
    // can start between two process-table reads. Discard the first capture
    // and take a fresh process table and complete host view.
    once(warnings, record_until)?.context(
        "panes kept changing during capture (agent metadata or a starting program); retry capture",
    )
}

fn capture_once(
    host: &mut impl Host,
    session: &str,
    workspace: Option<&str>,
    config: &Config,
    warnings: &mut Vec<String>,
    record_until: std::time::Instant,
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
            // A session taken from Claude's record that the argv does not
            // name may be one no message has made a transcript for yet (a
            // bare start, or a /clear after --resume), so it is checked.
            let mut unnamed_record = false;
            let (mut command, native_session, refreshed) = match &attached {
                Some(resolved) => (command_for(resolved, None)?, false, false),
                None if !host.reports_agent_sessions() => {
                    match unreported_agent_command(
                        identity,
                        &argv,
                        CLAUDE_RECORD_GRACE,
                        record_until,
                    )? {
                        Unreported::Known {
                            command,
                            recorded,
                            unnamed,
                            note,
                        } => {
                            if let Some(note) = note {
                                warnings.push(format!("pane {}: {note}", pane.pane_id));
                            }
                            unnamed_record = unnamed;
                            (command, recorded, false)
                        }
                        Unreported::Unidentified(reason) => {
                            // Waiting gains nothing here: no session metadata
                            // ever arrives. The other panes still save.
                            save_unidentified(
                                pane,
                                cwd,
                                &reason,
                                warnings,
                                &mut protected_panes,
                                &mut panes,
                            );
                            continue;
                        }
                    }
                }
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
                    && (((native_session || attached.is_some()) && !has_explicit_resume(&argv))
                        || unnamed_record);
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
                    // An attach can be typed in any directory, and a session
                    // from Claude's record may be one `/resume` opened from
                    // another project: look for either's transcript anywhere.
                    let anywhere = attached.is_some() || unnamed_record;
                    let profile = environment
                        .as_deref()
                        .context("Claude environment is unavailable")
                        .and_then(|environment| {
                            claude_create_profile(
                                environment,
                                config,
                                executable,
                                session_id,
                                &cwd,
                                &argv,
                                anywhere,
                            )
                        });
                    match profile {
                        Ok(None) => {}
                        Ok(Some((config_dir, home))) => {
                            *session_mode = AgentSessionMode::Create;
                            *saved_claude_config_dir = Some(config_dir);
                            *saved_claude_home = home;
                        }
                        // On herdr-core no metadata will settle this later;
                        // one pane cannot stop the others from saving.
                        Err(error) if !host.reports_agent_sessions() => {
                            let reason = format!("claude session {session_id}: {error:#}");
                            save_unidentified(
                                pane,
                                cwd,
                                &reason,
                                warnings,
                                &mut protected_panes,
                                &mut panes,
                            );
                            continue;
                        }
                        Err(error) => return Err(error),
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
        return agent_command(argv, agent, session_id).map(Some);
    }
    // Never turn an unrelated foreground process into a stale stored agent session.
    let command = CommandSpec::Program {
        argv: argv.to_vec(),
    };
    command.argv()?;
    Ok(Some(command))
}

/// The profile a Claude session must be created in on restore because no
/// message has written its transcript yet, or `None` when it resumes.
fn claude_create_profile(
    environment: &[u8],
    config: &Config,
    executable: &str,
    session_id: &str,
    cwd: &str,
    argv: &[String],
    anywhere: bool,
) -> Result<Option<(String, Option<String>)>> {
    let (config_dir, home) = claude_profile(environment)?;
    let transcript_exists = if anywhere {
        claude_transcript_exists(&config_dir, session_id)?
    } else {
        match claude_project_transcript_exists(&config_dir, std::path::Path::new(cwd), session_id)?
        {
            Some(exists) if exists || !has_implicit_resume(argv) => exists,
            _ => claude_transcript_exists(&config_dir, session_id)?,
        }
    };
    if transcript_exists {
        return Ok(None);
    }
    let (configured_dir, configured_home) = configured_claude_profile(config, executable)?;
    ensure!(
        config_dir == configured_dir && home == configured_home,
        "Claude configuration profile cannot be reproduced; select the launcher with CLAUDE_CONFIG_DIR"
    );
    let config_dir = config_dir
        .to_str()
        .context("non-UTF-8 Claude configuration path")?
        .to_string();
    let home = home
        .map(|home| {
            home.into_os_string()
                .into_string()
                .map_err(|_| anyhow::anyhow!("non-UTF-8 Claude HOME"))
        })
        .transpose()?;
    Ok(Some((config_dir, home)))
}

/// Saves an agent pane whose session cannot be told exactly with its cwd and
/// no command, and keeps any earlier agent session off it.
fn save_unidentified(
    pane: &LivePane,
    cwd: String,
    reason: &str,
    warnings: &mut Vec<String>,
    protected: &mut Protected,
    panes: &mut Vec<SavedPane>,
) {
    warnings.push(format!(
        "pane {}: {reason}; saved without a command",
        pane.pane_id
    ));
    protected.unidentified.insert(pane.pane_id.clone());
    panes.push(SavedPane {
        workspace_id: pane.workspace_id.clone(),
        tab_id: pane.tab_id.clone(),
        pane_id: pane.pane_id.clone(),
        cwd,
        command: None,
    });
}

fn agent_command(argv: &[String], agent: AgentKind, session_id: String) -> Result<CommandSpec> {
    validate_session_id(&session_id)?;
    Ok(CommandSpec::Agent {
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
    })
}

/// The command of a pane on a host that never reports agent sessions
/// (herdr-core), or why its agent's session cannot be told exactly.
enum Unreported {
    Known {
        command: Option<CommandSpec>,
        /// The session came from the agent's own record of the process.
        recorded: bool,
        /// That recorded session is not the one the argv names.
        unnamed: bool,
        /// Why the record could not be used, when the argv decided instead.
        note: Option<String>,
    },
    Unidentified(String),
}

/// Claude's own record names the session the process has open now, so it
/// wins over a `--resume` in its argv, which an in-session `/resume` leaves
/// behind, as herdr's metadata does on the TUI host.
fn unreported_agent_command(
    identity: &platform::Process,
    argv: &[String],
    grace: std::time::Duration,
    until: std::time::Instant,
) -> Result<Unreported> {
    let Some(agent) = agent_launcher(argv) else {
        return Ok(Unreported::Known {
            command: command_for(argv, None)?,
            recorded: false,
            unnamed: false,
            note: None,
        });
    };
    let from_argv = command_for(argv, None);
    // An argv that names the session needs no wait for the record, which
    // would only confirm or update it.
    let grace = if matches!(from_argv, Ok(Some(CommandSpec::Agent { .. }))) {
        std::time::Duration::ZERO
    } else {
        grace
    };
    let recorded = if agent == AgentKind::Claude {
        claude_recorded_session_waiting(identity, grace, until)
    } else {
        Ok(None)
    };
    if let Ok(Some(session_id)) = recorded {
        let unnamed = !matches!(
            &from_argv,
            Ok(Some(CommandSpec::Agent { session_id: named, .. })) if *named == session_id
        );
        return Ok(Unreported::Known {
            command: Some(agent_command(argv, agent, session_id)?),
            recorded: true,
            unnamed,
            note: None,
        });
    }
    Ok(match from_argv {
        Ok(command) => Unreported::Known {
            command,
            recorded: false,
            unnamed: false,
            note: recorded.err().map(|error| {
                format!(
                    "Claude's session record unused: {error:#}; saved the session its argv names"
                )
            }),
        },
        Err(error) => Unreported::Unidentified(match recorded {
            Err(record) => format!(
                "{} session not identified: {error:#}; Claude's session record: {record:#}",
                agent.name()
            ),
            Ok(_) => format!("{} session not identified: {error:#}", agent.name()),
        }),
    })
}

/// The fields revive reads from Claude Code's `<config>/sessions/<pid>.json`.
#[derive(serde::Deserialize)]
struct ClaudeSessionRecord {
    pid: u32,
    #[serde(rename = "sessionId")]
    session_id: String,
    #[serde(rename = "startedAt", default)]
    started_at: Option<u64>,
    #[serde(rename = "procStart", default)]
    proc_start: Option<String>,
    /// `linux:<machine id>:pid:[<pid namespace>]`: the machine and pid
    /// namespace the pid belongs to. It does not tell boots apart.
    #[serde(rename = "pidDomain", default)]
    pid_domain: Option<String>,
}

/// How the writer of a session record is told apart on this platform.
enum Writer<'a> {
    /// The process's pid domain as Claude writes it (`None` when the machine
    /// id cannot be read).
    Linux {
        domain: Option<&'a str>,
    },
    Macos,
}

/// Whether `process` wrote the record, rather than an earlier process that
/// had the same pid and left its record behind.
fn record_written_by(
    process: &platform::Process,
    record: &ClaudeSessionRecord,
    writer: &Writer,
) -> bool {
    if record.pid != process.pid {
        return false;
    }
    match writer {
        // procStart is the start time field of /proc/<pid>/stat, as `start`
        // is. It counts clock ticks since boot, so a record from an earlier
        // boot can match it too: `claude_recorded_session` checks the boot.
        // pidDomain names the machine and pid namespace.
        Writer::Linux { domain } => {
            record.proc_start.as_deref() == Some(process.start.to_string().as_str())
                && match (record.pid_domain.as_deref(), domain) {
                    (Some(written), Some(domain)) => written == *domain,
                    _ => true,
                }
        }
        // `start` is microseconds since the epoch, `startedAt` milliseconds,
        // taken once the process runs; an earlier process wrote before this
        // one started.
        Writer::Macos => {
            process.start != 0
                && record.started_at.is_some_and(|started| {
                    started.saturating_mul(1000).saturating_add(999) >= process.start
                })
        }
    }
}

/// The pid domain Claude writes on Linux for process `pid`:
/// `linux:<machine id>:pid:[<pid namespace>]`; `None` without a machine id.
fn linux_pid_domain(pid: u32) -> Result<Option<String>> {
    let Ok(machine) = std::fs::read_to_string("/etc/machine-id") else {
        return Ok(None);
    };
    let namespace = std::fs::read_link(format!("/proc/{pid}/ns/pid"))
        .with_context(|| format!("cannot read the pid namespace of {pid}"))?;
    Ok(Some(format!(
        "linux:{}:{}",
        machine.trim(),
        namespace.to_string_lossy()
    )))
}

/// When this boot began, in ms since the epoch (`btime` in /proc/stat).
fn linux_boot_ms() -> Result<u64> {
    let stat = std::fs::read_to_string("/proc/stat").context("cannot read /proc/stat")?;
    let seconds: u64 = stat
        .lines()
        .find_map(|line| line.strip_prefix("btime "))
        .context("/proc/stat has no btime")?
        .trim()
        .parse()
        .context("/proc/stat btime is not a number")?;
    Ok(seconds.saturating_mul(1000))
}

/// Claude writes its record a second or more after it starts (1.3 to 3.3 s
/// on Linux, measured), later than the hook's save of the command that
/// started it. A save that missed it would keep no session, and a quiet
/// pane makes no later save.
const CLAUDE_RECORD_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

/// How long one save may wait for Claude records in all, across its panes
/// and both capture attempts. A save made 0.5 s after Claude starts thus
/// covers a record written up to 4.5 s in. The save holds the session lock
/// meanwhile; `app::wait_lock` waits longer than this for it.
pub const CLAUDE_RECORD_BUDGET: std::time::Duration = std::time::Duration::from_secs(4);

/// A record Claude may be about to replace: one not yet whole, or one left
/// by an earlier boot. A wait reads it again; other errors end it.
#[derive(Debug)]
struct RecordNotReady(String);

impl std::fmt::Display for RecordNotReady {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for RecordNotReady {}

/// `claude_recorded_session`, waiting until the process is `grace` old, but
/// not past `until`, for a record it may still be about to write. An older
/// process without one is not waited for: Claude before 2.1.319 may write
/// none.
fn claude_recorded_session_waiting(
    identity: &platform::Process,
    grace: std::time::Duration,
    until: std::time::Instant,
) -> Result<Option<String>> {
    // Fixed once: the age need not advance (a /proc/uptime virtualised in a
    // container, a wall clock stepped back on macOS), the deadline does.
    let deadline = (std::time::Instant::now()
        + grace.saturating_sub(platform::process_age(identity)?))
    .min(until);
    loop {
        let recorded = claude_recorded_session(identity);
        let pending = match &recorded {
            Ok(None) => true,
            Ok(Some(_)) => false,
            Err(error) => error.downcast_ref::<RecordNotReady>().is_some(),
        };
        if !pending || std::time::Instant::now() >= deadline {
            return recorded;
        }
        ensure!(
            platform::process(identity.pid)? == *identity,
            "agent changed while waiting for its session record"
        );
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        std::thread::sleep(left.min(std::time::Duration::from_millis(100)));
    }
}

/// The session a running Claude process has open, from its own record;
/// `None` when it wrote none (an older Claude, or one not yet started).
fn claude_recorded_session(identity: &platform::Process) -> Result<Option<String>> {
    let (config_dir, _) = claude_profile(&platform::environment(identity)?)?;
    let path = config_dir
        .join("sessions")
        .join(format!("{}.json", identity.pid));
    match std::fs::symlink_metadata(&path) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("cannot read {}", path.display())),
    }
    let record: ClaudeSessionRecord = serde_json::from_slice(&crate::store::read_bytes(&path)?)
        .map_err(|error| {
            let message = format!("{} is not a Claude session record: {error}", path.display());
            // Only a record cut short may still be completed; one that is
            // whole but does not fit never will.
            if error.classify() == serde_json::error::Category::Eof {
                anyhow::Error::new(RecordNotReady(message))
            } else {
                anyhow::anyhow!(message)
            }
        })?;
    let domain = if cfg!(target_os = "linux") {
        linux_pid_domain(identity.pid)?
    } else {
        None
    };
    let writer = if cfg!(target_os = "linux") {
        Writer::Linux {
            domain: domain.as_deref(),
        }
    } else {
        Writer::Macos
    };
    if !record_written_by(identity, &record, &writer) {
        return Ok(None);
    }
    if cfg!(target_os = "linux")
        && let Some(started) = record.started_at
    {
        // Not a quiet `None`: when the clock stepped forward after Claude
        // started, a real record lands here, and the warning should say so.
        let boot = linux_boot_ms()?;
        if started < boot {
            return Err(RecordNotReady(format!(
                "{} was written at {started} ms, before this boot began ({boot} ms by the clock now): \
                 left by an earlier boot, or the clock stepped forward since Claude started",
                path.display()
            ))
            .into());
        }
    }
    validate_session_id(&record.session_id)
        .with_context(|| format!("{} names no exact session", path.display()))?;
    Ok(Some(record.session_id))
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

    use crate::model::CommandSpec;

    fn owned(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    struct Child(std::process::Child);

    impl Drop for Child {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    /// A sleeper whose environment can be read: macOS hides the environment
    /// of its own platform binaries such as /bin/sleep, which Claude is not.
    /// An ad-hoc signed copy is no platform binary.
    fn sleeper(dir: &std::path::Path) -> std::path::PathBuf {
        if !cfg!(target_os = "macos") {
            return "sleep".into();
        }
        let copy = dir.join("sleep");
        std::fs::copy("/bin/sleep", &copy).unwrap();
        let signed = std::process::Command::new("codesign")
            .args(["--force", "--sign", "-"])
            .arg(&copy)
            .status()
            .unwrap();
        assert!(signed.success(), "codesign failed for {}", copy.display());
        copy
    }

    /// A live process whose environment names `config` as Claude's
    /// configuration directory, as a running Claude's would.
    fn claude_like(config: &std::path::Path) -> (Child, super::platform::Process) {
        let child = std::process::Command::new(sleeper(config))
            .arg("30")
            .env_clear()
            .env("CLAUDE_CONFIG_DIR", config)
            .spawn()
            .unwrap();
        let process = super::platform::process(child.id()).unwrap();
        (Child(child), process)
    }

    const UUID: &str = "3f2b8c1e-0000-4000-8000-000000000000";

    fn write_record(config: &std::path::Path, process: &super::platform::Process, text: &str) {
        std::fs::create_dir_all(config.join("sessions")).unwrap();
        std::fs::write(
            config
                .join("sessions")
                .join(format!("{}.json", process.pid)),
            text,
        )
        .unwrap();
    }

    /// A record shaped like the ones Claude Code 2.1.327 writes, for
    /// `process`: on Linux with the pidDomain it writes there.
    fn own_record(process: &super::platform::Process) -> String {
        let mut record = serde_json::json!({
            "pid": process.pid,
            "sessionId": UUID,
            "cwd": "/",
            "startedAt": crate::store::now_ms().unwrap(),
            "procStart": process.start.to_string(),
            "version": "2.1.327",
            "kind": "interactive",
            "entrypoint": "cli",
            "status": "idle",
        });
        if cfg!(target_os = "linux") {
            let machine = std::fs::read_to_string("/etc/machine-id").unwrap();
            let namespace = std::fs::read_link(format!("/proc/{}/ns/pid", process.pid)).unwrap();
            record["pidDomain"] = serde_json::json!(format!(
                "linux:{}:{}",
                machine.trim(),
                namespace.to_string_lossy()
            ));
        }
        record.to_string()
    }

    #[test]
    fn a_session_record_counts_only_for_the_process_that_wrote_it() {
        use super::{ClaudeSessionRecord, Writer, record_written_by};
        let process = |start| super::platform::Process {
            pid: 42,
            parent: 1,
            group: 42,
            start,
        };
        let record =
            |pid, started_at: Option<u64>, proc_start: Option<&str>, domain: Option<&str>| {
                ClaudeSessionRecord {
                    pid,
                    session_id: UUID.into(),
                    started_at,
                    proc_start: proc_start.map(String::from),
                    pid_domain: domain.map(String::from),
                }
            };
        let domain = "linux:0123456789abcdef0123456789abcdef:pid:[4026531836]";
        let here = Writer::Linux {
            domain: Some(domain),
        };
        let linux =
            |start, record: &ClaudeSessionRecord| record_written_by(&process(start), record, &here);
        let after = Some(1_794_900_060_000);
        assert!(linux(
            76105459,
            &record(42, after, Some("76105459"), Some(domain))
        ));
        assert!(linux(76105459, &record(42, None, Some("76105459"), None)));
        assert!(!linux(
            76105459,
            &record(
                42,
                after,
                Some("76105459"),
                Some("linux:0000:pid:[4026531836]")
            )
        ));
        assert!(!linux(
            76105459,
            &record(42, after, Some("76100000"), Some(domain))
        ));
        assert!(!linux(76105459, &record(42, after, None, Some(domain))));
        assert!(!linux(
            76105459,
            &record(7, after, Some("76105459"), Some(domain))
        ));
        let no_machine_id = Writer::Linux { domain: None };
        assert!(record_written_by(
            &process(76105459),
            &record(42, after, Some("76105459"), Some(domain)),
            &no_machine_id
        ));

        let macos = |start, record: &ClaudeSessionRecord| {
            record_written_by(&process(start), record, &Writer::Macos)
        };
        let started_us = 1_794_631_207_000_000;
        assert!(macos(
            started_us,
            &record(42, Some(1_794_631_207_889), None, None)
        ));
        assert!(macos(
            started_us + 500,
            &record(42, Some(1_794_631_207_000), None, None)
        ));
        assert!(!macos(
            started_us,
            &record(42, Some(1_794_631_206_000), None, None)
        ));
        assert!(!macos(started_us, &record(42, None, None, None)));
        assert!(!macos(0, &record(42, Some(1_794_631_207_889), None, None)));
    }

    #[test]
    fn claudes_own_record_gives_the_session_of_that_very_process() {
        let config = tempfile::tempdir().unwrap();
        let config = config.path().canonicalize().unwrap();
        let (_child, process) = claude_like(&config);
        assert_eq!(super::claude_recorded_session(&process).unwrap(), None);

        write_record(&config, &process, &own_record(&process));
        assert_eq!(
            super::claude_recorded_session(&process).unwrap().as_deref(),
            Some(UUID)
        );

        let stale = serde_json::json!({"pid": process.pid, "sessionId": UUID,
            "startedAt": 1, "procStart": "1"});
        write_record(&config, &process, &stale.to_string());
        assert_eq!(super::claude_recorded_session(&process).unwrap(), None);

        if cfg!(target_os = "linux") {
            let mut record: serde_json::Value =
                serde_json::from_str(&own_record(&process)).unwrap();
            record["pidDomain"] =
                serde_json::json!("linux:00000000000000000000000000000000:pid:[1]");
            write_record(&config, &process, &record.to_string());
            assert_eq!(super::claude_recorded_session(&process).unwrap(), None);
            let mut record: serde_json::Value =
                serde_json::from_str(&own_record(&process)).unwrap();
            record["startedAt"] = serde_json::json!(super::linux_boot_ms().unwrap() - 1);
            write_record(&config, &process, &record.to_string());
            let error = format!(
                "{:#}",
                super::claude_recorded_session(&process).unwrap_err()
            );
            assert!(
                error.contains("before this boot began") && error.contains("clock stepped forward"),
                "a record from an earlier boot with the same pid and start tick: {error}"
            );
        }

        write_record(&config, &process, "{\"pid\": ");
        assert!(super::claude_recorded_session(&process).is_err());
    }

    #[test]
    fn a_young_claude_is_given_time_to_write_its_record() {
        let config = tempfile::tempdir().unwrap();
        let config = config.path().canonicalize().unwrap();
        let (_child, process) = claude_like(&config);
        let grace = std::time::Duration::from_secs(5);
        let writer = {
            let (config, process) = (config.clone(), process.clone());
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(300));
                write_record(&config, &process, &own_record(&process));
            })
        };
        let started = std::time::Instant::now();
        let session = super::claude_recorded_session_waiting(
            &process,
            grace,
            std::time::Instant::now() + std::time::Duration::from_secs(60),
        )
        .unwrap();
        writer.join().unwrap();
        assert_eq!(session.as_deref(), Some(UUID));
        assert!(started.elapsed() >= std::time::Duration::from_millis(300));
        assert!(started.elapsed() < grace);

        let silent = tempfile::tempdir().unwrap();
        let silent = silent.path().canonicalize().unwrap();
        let (_child, old) = claude_like(&silent);
        let started = std::time::Instant::now();
        assert_eq!(
            super::claude_recorded_session_waiting(
                &old,
                std::time::Duration::ZERO,
                std::time::Instant::now() + std::time::Duration::from_secs(60)
            )
            .unwrap(),
            None,
            "a process past the grace period is not waited for"
        );
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
    }

    #[test]
    fn a_half_written_record_is_read_again_until_the_deadline() {
        let config = tempfile::tempdir().unwrap();
        let config = config.path().canonicalize().unwrap();
        let (_child, process) = claude_like(&config);
        write_record(&config, &process, "{\"pid\": ");
        let writer = {
            let (config, process) = (config.clone(), process.clone());
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(300));
                write_record(&config, &process, &own_record(&process));
            })
        };
        let session = super::claude_recorded_session_waiting(
            &process,
            std::time::Duration::from_secs(5),
            std::time::Instant::now() + std::time::Duration::from_secs(60),
        );
        writer.join().unwrap();
        assert_eq!(session.unwrap().as_deref(), Some(UUID));

        write_record(&config, &process, "{\"pid\": ");
        let started = std::time::Instant::now();
        let error = super::claude_recorded_session_waiting(
            &process,
            std::time::Duration::ZERO,
            std::time::Instant::now() + std::time::Duration::from_secs(60),
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("not a Claude session record"));
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
    }

    #[test]
    fn the_lock_waits_outlast_a_save_waiting_for_records() {
        let config = |timeout_ms, settle_ms| Config {
            timeout_ms,
            settle_ms,
            ..Config::default()
        };
        let wait = |c: &Config| crate::app::lock_wait(c);
        let floor = super::CLAUDE_RECORD_BUDGET + std::time::Duration::from_millis(1500);
        assert_eq!(wait(&Config::default()), floor);
        assert_eq!(
            wait(&config(3000, 0)),
            floor,
            "settle_ms 0 does not shorten it"
        );
        assert_eq!(wait(&config(10, 0)), floor);
        assert_eq!(
            wait(&config(20_000, 5_000)),
            std::time::Duration::from_secs(25)
        );
        assert_eq!(
            wait(&config(60_000, 60_000)),
            std::time::Duration::from_secs(60)
        );
    }

    #[test]
    fn both_capture_attempts_share_one_record_deadline() {
        let mut deadlines = Vec::new();
        let mut warnings = vec!["kept".to_string()];
        let captured = super::attempt_twice(
            std::time::Duration::from_secs(4),
            &mut warnings,
            false,
            |warnings, until| {
                deadlines.push(until);
                warnings.push(format!("attempt {}", deadlines.len()));
                std::thread::sleep(std::time::Duration::from_millis(20));
                Ok((deadlines.len() == 2).then_some("second"))
            },
        )
        .unwrap();
        assert_eq!(captured, "second");
        assert_eq!(deadlines.len(), 2);
        assert_eq!(
            deadlines[0], deadlines[1],
            "the second attempt gets no new budget"
        );
        assert_eq!(
            warnings,
            ["kept", "attempt 2"],
            "the discarded attempt's warnings go"
        );

        let error =
            super::attempt_twice(std::time::Duration::ZERO, &mut Vec::new(), true, |_, _| {
                Ok(None::<()>)
            })
            .unwrap_err();
        assert!(error.to_string().contains("panes kept changing"));
    }

    #[test]
    fn a_record_wait_ends_by_the_budget_deadline() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path().canonicalize().unwrap();
        let (_child, process) = claude_like(&dir);
        let started = std::time::Instant::now();
        let until = started + std::time::Duration::from_millis(200);
        let session = super::claude_recorded_session_waiting(
            &process,
            std::time::Duration::from_secs(30),
            until,
        )
        .unwrap();
        assert_eq!(session, None);
        let waited = started.elapsed();
        assert!(
            waited >= std::time::Duration::from_millis(200),
            "{waited:?}"
        );
        assert!(waited < std::time::Duration::from_secs(2), "{waited:?}");
    }

    #[test]
    fn a_whole_record_that_does_not_fit_ends_the_wait() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path().canonicalize().unwrap();
        let (_child, process) = claude_like(&dir);
        write_record(&dir, &process, "{\"pid\": \"not a number\"}");
        let started = std::time::Instant::now();
        let error = super::claude_recorded_session_waiting(
            &process,
            std::time::Duration::from_secs(30),
            started + std::time::Duration::from_secs(30),
        )
        .unwrap_err();
        assert!(
            format!("{error:#}").contains("not a Claude session record"),
            "{error:#}"
        );
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
    }

    #[test]
    fn an_error_that_cannot_clear_ends_the_wait() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path().canonicalize().unwrap();
        let (_child, process) = claude_like(&dir);
        let mut record: serde_json::Value = serde_json::from_str(&own_record(&process)).unwrap();
        record["sessionId"] = serde_json::json!("not-a-uuid");
        write_record(&dir, &process, &record.to_string());
        let started = std::time::Instant::now();
        let error = super::claude_recorded_session_waiting(
            &process,
            std::time::Duration::from_secs(30),
            started + std::time::Duration::from_secs(30),
        )
        .unwrap_err();
        assert!(
            format!("{error:#}").contains("names no exact session"),
            "{error:#}"
        );
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
    }

    #[test]
    fn an_argv_that_names_the_session_does_not_wait_for_the_record() {
        let config = tempfile::tempdir().unwrap();
        let config = config.path().canonicalize().unwrap();
        let (_child, process) = claude_like(&config);
        let started = std::time::Instant::now();
        let decided = super::unreported_agent_command(
            &process,
            &owned(&["claude", "--resume", UUID]),
            std::time::Duration::from_secs(30),
            std::time::Instant::now() + std::time::Duration::from_secs(60),
        )
        .unwrap();
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
        assert!(matches!(
            decided,
            super::Unreported::Known {
                recorded: false,
                ..
            }
        ));
    }

    #[test]
    fn without_native_metadata_an_unknown_agent_session_is_not_a_failure() {
        use super::{Unreported, unreported_agent_command};
        let config = tempfile::tempdir().unwrap();
        let config = config.path().canonicalize().unwrap();
        let (_child, process) = claude_like(&config);
        let decide = |argv: &[&str]| {
            unreported_agent_command(
                &process,
                &owned(argv),
                std::time::Duration::ZERO,
                std::time::Instant::now() + std::time::Duration::from_secs(60),
            )
            .unwrap()
        };
        let known = |argv: &[&str]| match decide(argv) {
            Unreported::Known {
                command,
                recorded,
                unnamed,
                note,
            } => (command, recorded, unnamed, note),
            Unreported::Unidentified(reason) => panic!("{argv:?} not identified: {reason}"),
        };
        let session = |command: Option<CommandSpec>| match command {
            Some(CommandSpec::Agent { session_id, .. }) => session_id,
            other => panic!("not an agent: {other:?}"),
        };

        let Unreported::Unidentified(reason) = decide(&["claude"]) else {
            panic!("a bare claude without its record is not identified");
        };
        assert!(reason.contains("claude session not identified"), "{reason}");
        let Unreported::Unidentified(reason) = decide(&["codex"]) else {
            panic!("a bare codex is not identified");
        };
        assert!(reason.contains("codex session not identified"), "{reason}");
        let (command, recorded, unnamed, note) = known(&["codex", "resume", UUID]);
        assert_eq!(
            (session(command), recorded, unnamed, note),
            (UUID.to_string(), false, false, None)
        );
        let (command, recorded, _, _) = known(&["sleep", "30"]);
        assert!(matches!(command, Some(CommandSpec::Program { .. })) && !recorded);

        write_record(&config, &process, &own_record(&process));
        let other = "01234567-89ab-cdef-0123-456789abcdef";
        let (command, recorded, unnamed, _) = known(&["claude"]);
        assert_eq!(
            (session(command), recorded, unnamed),
            (UUID.to_string(), true, true)
        );
        let (command, recorded, unnamed, _) = known(&["claude", "--resume", other]);
        assert_eq!(
            (session(command), recorded, unnamed),
            (UUID.to_string(), true, true),
            "the record wins over a stale --resume, and its transcript must be checked"
        );
        let (command, recorded, unnamed, _) = known(&["claude", "--resume", UUID]);
        assert_eq!(
            (session(command), recorded, unnamed),
            (UUID.to_string(), true, false)
        );

        write_record(&config, &process, "not json");
        let Unreported::Unidentified(reason) = decide(&["claude"]) else {
            panic!("an unreadable record identifies nothing");
        };
        assert!(reason.contains("not a Claude session record"), "{reason}");
        let (command, recorded, _, note) = known(&["claude", "--resume", other]);
        assert_eq!((session(command), recorded), (other.to_string(), false));
        let note = note.expect("the unusable record is reported");
        assert!(note.contains("not a Claude session record"), "{note}");
    }
}
