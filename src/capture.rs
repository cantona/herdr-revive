use crate::model::*;
use crate::platform;
use crate::transport::Host;
use anyhow::{Context, Result, ensure};

pub fn capture(
    host: &mut impl Host,
    session: &str,
    workspace: Option<&str>,
    config: &Config,
) -> Result<Snapshot> {
    let live = host.snapshot()?;
    crate::planner::index_live(&live)?;
    if let Some(id) = workspace {
        ensure!(
            live.workspaces.iter().any(|w| w.workspace_id == id),
            "named workspace is not present"
        );
    }
    let table = platform::process_table()?;
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
            if pane.agent.is_some() && agent_launcher(&argv).is_none() {
                anyhow::bail!(
                    "detected agent uses an unsupported launcher; exact resume cannot be established"
                );
            }
            let mut command = command_for(&argv, pane.agent_session.as_ref())?;
            if let Some(CommandSpec::Agent {
                agent, executable, ..
            }) = &mut command
            {
                let launchers: Vec<_> = config
                    .agent_launchers
                    .iter()
                    .filter(|launcher| launcher.agent == *agent)
                    .collect();
                if !launchers.is_empty() {
                    let environment = platform::environment(identity)?;
                    if let Some(launcher) = matching_launcher(&launchers, &environment)? {
                        *executable = launcher.executable.clone();
                    }
                }
            }
            if null_stdio.iter().any(|value| *value) {
                match command {
                    Some(CommandSpec::Program { argv }) => {
                        Some(CommandSpec::ProgramNullStdio { argv, null_stdio })
                    }
                    _ => anyhow::bail!("redirected agent streams are unsupported"),
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
    Ok(snapshot)
}

pub fn matching_launcher<'a>(
    launchers: &[&'a AgentLauncher],
    environment: &[u8],
) -> Result<Option<&'a AgentLauncher>> {
    let mut matched = None;
    for launcher in launchers {
        let mut matches = true;
        for (key, value) in &launcher.match_env {
            let prefix = format!("{key}=");
            let mut values = environment
                .split(|byte| *byte == 0)
                .filter_map(|entry| entry.strip_prefix(prefix.as_bytes()));
            let actual = values.next();
            ensure!(
                values.next().is_none(),
                "duplicate agent launcher environment key; capture refused"
            );
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
