use crate::model::*;
use crate::platform;
use crate::transport::Host;
use anyhow::{Context, Result, ensure};
use std::path::PathBuf;

pub fn capture(
    host: &mut impl Host,
    session: &str,
    workspace: Option<&str>,
    config: &Config,
) -> Result<Snapshot> {
    if let Some(snapshot) = capture_once(host, session, workspace, config)? {
        return Ok(snapshot);
    }
    // A metadata wait can span unrelated pane/layout changes. Discard the
    // pre-wait capture and take a fresh process table and complete host view.
    capture_once(host, session, workspace, config)?
        .context("agent metadata did not stabilize; retry capture")
}

fn capture_once(
    host: &mut impl Host,
    session: &str,
    workspace: Option<&str>,
    config: &Config,
) -> Result<Option<Snapshot>> {
    let live = host.snapshot()?;
    crate::planner::index_live(&live)?;
    if let Some(id) = workspace {
        ensure!(
            live.workspaces.iter().any(|w| w.workspace_id == id),
            "named workspace is not present"
        );
    }
    let table = platform::process_table()?;
    let mut listings = ClaudeListings::default();
    let mut panes = Vec::new();
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
            .context("shell is absent from process table")?;
        ensure!(
            !info.foreground_processes.is_empty(),
            "foreground process information unavailable"
        );
        ensure!(
            platform::process(shell)? == *shell_process,
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
            let identity = table
                .get(&leader.pid)
                .context("foreground process disappeared")?;
            platform::validate_foreground_tree(&table, identity)?;
            let (argv, process_cwd) = platform::argv_and_cwd(identity)?;
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
                    eprintln!(
                        "herdr-revive: warning: pane {}: {error:#}; using herdr's session record",
                        pane.pane_id
                    );
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
    let layout = crate::layout::capture(host, &live, workspace)?;
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
    Ok(Some(snapshot))
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
                .context("agent session metadata did not arrive; previous snapshot retained")
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
