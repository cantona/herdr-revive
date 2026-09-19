use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

pub const TOOL: &str = "herdr-revive";
pub const PLUGIN_ID: &str = "cantona.herdr-revive";
pub const SCHEMA: u32 = 1;
pub const MAX_BYTES: usize = 8 * 1024 * 1024;
pub const PROTOCOL: u32 = 22;

pub fn native_tool(tool: &str) -> bool {
    matches!(tool, TOOL | "herde-revive")
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub auto_restore: bool,
    pub auto_save: bool,
    pub debounce_ms: u64,
    pub settle_ms: u64,
    pub timeout_ms: u64,
    pub retention: usize,
    pub interval_seconds: u64,
    pub transport: TransportKind,
    pub allowed_programs: Vec<String>,
    pub match_program_basename: bool,
    pub resume_agents: bool,
    pub agent_extra_args: std::collections::BTreeMap<String, Vec<String>>,
    pub agent_launchers: Vec<AgentLauncher>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AgentLauncher {
    pub agent: AgentKind,
    pub executable: String,
    pub match_env: std::collections::BTreeMap<String, String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TransportKind {
    Cli,
    Direct,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            auto_restore: false,
            auto_save: false,
            debounce_ms: 5000,
            settle_ms: 2500,
            timeout_ms: 3000,
            retention: 10,
            interval_seconds: 900,
            transport: TransportKind::Direct,
            allowed_programs: vec![],
            match_program_basename: false,
            resume_agents: true,
            agent_extra_args: std::collections::BTreeMap::new(),
            agent_launchers: vec![],
        }
    }
}

impl Config {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            (10..=60_000).contains(&self.timeout_ms),
            "timeout_ms must be 10..60000"
        );
        ensure!(self.settle_ms <= 60_000, "settle_ms exceeds 60000");
        ensure!(
            self.debounce_ms <= 86_400_000,
            "debounce_ms exceeds one day"
        );
        ensure!(
            (1..=1000).contains(&self.retention),
            "retention must be 1..1000"
        );
        ensure!(
            (1..=86400).contains(&self.interval_seconds),
            "interval_seconds must be 1..86400"
        );
        let mut seen = HashSet::new();
        for program in &self.allowed_programs {
            validate_program(program)?;
            ensure!(seen.insert(program), "duplicate allowlist entry");
        }
        let mut launchers = HashSet::new();
        for launcher in &self.agent_launchers {
            validate_program(&launcher.executable)?;
            ensure!(
                launcher.executable != "*",
                "launcher must name an executable"
            );
            ensure!(
                agent_launcher(std::slice::from_ref(&launcher.executable)).is_none(),
                "custom launcher cannot replace a canonical agent name"
            );
            ensure!(
                launchers.insert(&launcher.executable),
                "duplicate agent launcher"
            );
            ensure!(
                !launcher.match_env.is_empty(),
                "launcher requires environment selectors"
            );
            for (key, value) in &launcher.match_env {
                ensure!(
                    !key.is_empty()
                        && key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
                        && !key.as_bytes()[0].is_ascii_digit(),
                    "invalid launcher environment key"
                );
                validate_text(value)?;
                ensure!(!value.is_empty(), "launcher selector cannot be empty");
            }
        }
        for (agent, args) in &self.agent_extra_args {
            ensure!(
                ["claude", "codex", "gemini", "copilot", "cursor"].contains(&agent.as_str()),
                "unknown agent configuration"
            );
            let kind = match agent.as_str() {
                "claude" => AgentKind::Claude,
                "codex" => AgentKind::Codex,
                "gemini" => AgentKind::Gemini,
                "copilot" => AgentKind::Copilot,
                "cursor" => AgentKind::Cursor,
                _ => unreachable!(),
            };
            let mut index = 0;
            while index < args.len() {
                validate_text(&args[index])?;
                if agent_option_takes_value(kind, &args[index]) {
                    let value = args
                        .get(index + 1)
                        .ok_or_else(|| anyhow::anyhow!("agent option requires a value"))?;
                    validate_text(value)?;
                    ensure!(
                        !value.starts_with('-'),
                        "agent option value cannot be another option"
                    );
                    index += 2;
                } else {
                    ensure!(
                        agent_boolean_option(kind, &args[index]),
                        "unsupported agent extra option; session selectors and prompts are forbidden"
                    );
                    index += 1;
                }
            }
        }
        Ok(())
    }
    pub fn allows(&self, executable: &str) -> bool {
        self.allowed_programs.iter().any(|allowed| {
            allowed == "*"
                || allowed == executable
                || (self.match_program_basename && {
                    let basename = std::path::Path::new(executable)
                        .file_name()
                        .and_then(|s| s.to_str());
                    basename.is_some_and(|name| name.eq_ignore_ascii_case(allowed))
                })
        })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Snapshot {
    pub tool: String,
    pub schema: u32,
    pub session: String,
    pub created_ms: u64,
    pub scope: Scope,
    pub panes: Vec<SavedPane>,
    #[serde(default)]
    pub layout: Vec<crate::layout::WorkspaceLayout>,
    #[serde(default)]
    pub focused_workspace_id: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Scope {
    Session,
    Space { workspace_id: String },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SavedPane {
    pub workspace_id: String,
    pub tab_id: String,
    pub pane_id: String,
    pub cwd: String,
    pub command: Option<CommandSpec>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CommandSpec {
    Program {
        argv: Vec<String>,
    },
    ProgramNullStdio {
        argv: Vec<String>,
        null_stdio: [bool; 3],
    },
    Agent {
        executable: String,
        agent: AgentKind,
        session_id: String,
        #[serde(default, skip_serializing_if = "AgentSessionMode::is_resume")]
        session_mode: AgentSessionMode,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        claude_config_dir: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        claude_home: Option<String>,
        #[serde(default, skip_serializing_if = "null_stdio_is_empty")]
        null_stdio: [bool; 3],
    },
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentSessionMode {
    #[default]
    #[serde(alias = "legacy")]
    Resume,
    Create,
}

impl AgentSessionMode {
    fn is_resume(&self) -> bool {
        *self == Self::Resume
    }
}

fn null_stdio_is_empty(null_stdio: &[bool; 3]) -> bool {
    !null_stdio.iter().any(|redirect| *redirect)
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentKind {
    Claude,
    Codex,
    Gemini,
    Copilot,
    Cursor,
}

impl AgentKind {
    pub fn name(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Gemini => "gemini",
            Self::Copilot => "copilot",
            Self::Cursor => "cursor",
        }
    }
    pub fn executable(self) -> &'static str {
        if self == Self::Cursor {
            "cursor-agent"
        } else {
            self.name()
        }
    }
}

pub fn agent_option_takes_value(agent: AgentKind, arg: &str) -> bool {
    if matches!(arg, "--model" | "-m") {
        return true;
    }
    match agent {
        AgentKind::Claude => matches!(
            arg,
            "--permission-mode"
                | "--settings"
                | "--add-dir"
                | "--allowedTools"
                | "--disallowedTools"
        ),
        AgentKind::Codex => matches!(
            arg,
            "--config" | "-c" | "--profile" | "--sandbox" | "--ask-for-approval"
        ),
        AgentKind::Gemini => matches!(arg, "--approval-mode" | "--include-directories"),
        AgentKind::Copilot => matches!(arg, "--allow-tool" | "--deny-tool" | "--add-dir"),
        AgentKind::Cursor => matches!(arg, "--workspace"),
    }
}

pub fn agent_boolean_option(agent: AgentKind, arg: &str) -> bool {
    match agent {
        AgentKind::Claude => arg == "--dangerously-skip-permissions",
        AgentKind::Codex => matches!(
            arg,
            "--full-auto" | "--dangerously-bypass-approvals-and-sandbox"
        ),
        AgentKind::Gemini => arg == "--yolo",
        AgentKind::Copilot => matches!(arg, "--allow-all-tools" | "--allow-all-paths"),
        AgentKind::Cursor => matches!(arg, "--force" | "-f"),
    }
}

impl CommandSpec {
    pub fn argv(&self) -> Result<Vec<String>> {
        let argv = match self {
            Self::Program { argv } | Self::ProgramNullStdio { argv, .. } => {
                validate_argv(argv)?;
                ensure!(
                    agent_launcher(argv).is_none(),
                    "known agent launchers require an exact session reference"
                );
                argv.clone()
            }
            Self::Agent {
                executable,
                agent,
                session_id,
                session_mode,
                claude_config_dir,
                claude_home,
                ..
            } => {
                validate_program(executable)?;
                if let Some(known) = agent_launcher(std::slice::from_ref(executable)) {
                    ensure!(known == *agent, "agent executable mismatch");
                }
                validate_session_id(session_id)?;
                ensure!(
                    *session_mode != AgentSessionMode::Create || *agent == AgentKind::Claude,
                    "new-session launch is only supported for Claude"
                );
                ensure!(
                    (*session_mode == AgentSessionMode::Create) == claude_config_dir.is_some(),
                    "Claude create mode requires its configuration profile"
                );
                if let Some(config_dir) = claude_config_dir {
                    validate_text(config_dir)?;
                    ensure!(
                        Path::new(config_dir).is_absolute(),
                        "Claude configuration directory must be absolute"
                    );
                }
                if let Some(home) = claude_home {
                    ensure!(
                        *session_mode == AgentSessionMode::Create,
                        "Claude home requires create mode"
                    );
                    validate_text(home)?;
                    ensure!(
                        Path::new(home).is_absolute(),
                        "Claude home must be absolute"
                    );
                }
                vec![
                    executable.clone(),
                    match (agent, session_mode) {
                        (AgentKind::Claude, AgentSessionMode::Create) => "--session-id",
                        (AgentKind::Codex, _) => "resume",
                        _ => "--resume",
                    }
                    .into(),
                    session_id.clone(),
                ]
            }
        };
        validate_argv(&argv)?;
        if let Some(null_stdio) = self.null_stdio() {
            ensure!(!null_stdio[0], "redirected stdin is unsupported");
            if matches!(self, Self::ProgramNullStdio { .. }) {
                ensure!(
                    !is_ssh(&argv[0]),
                    "SSH output redirection cannot be restored; repair or recapture the snapshot"
                );
            }
            ensure!(
                null_stdio.iter().any(|value| *value),
                "empty null-stdio specification"
            );
        }
        Ok(argv)
    }
    pub fn allowed_argv(&self, config: &Config) -> Result<Vec<String>> {
        let mut argv = self.argv()?;
        let mut create_profile = None;
        ensure!(
            config.allows(&argv[0]),
            "program is not in current allowlist"
        );
        if let Self::Agent {
            executable,
            agent,
            session_id,
            session_mode,
            claude_config_dir,
            claude_home,
            ..
        } = self
        {
            ensure!(
                agent_launcher(std::slice::from_ref(&argv[0])) == Some(*agent)
                    || config.agent_launchers.iter().any(|launcher| {
                        launcher.agent == *agent && launcher.executable == argv[0]
                    }),
                "custom agent launcher is not configured for this agent"
            );
            ensure!(
                config.resume_agents,
                "agent restoration disabled by current policy"
            );
            if *agent == AgentKind::Claude && *session_mode == AgentSessionMode::Create {
                let (current, current_home) = configured_claude_profile(config, executable)?;
                let captured = Path::new(
                    claude_config_dir
                        .as_deref()
                        .context("Claude create profile is unavailable")?,
                );
                ensure!(
                    current == captured
                        && current_home.as_deref() == claude_home.as_deref().map(Path::new),
                    "Claude configuration profile changed or was not captured exactly; recapture before restoring"
                );
                let exists = claude_transcript_exists(captured, session_id)?;
                if exists {
                    // The first save can race Claude's first transcript write.
                    // Never create over a conversation that became resumable.
                    argv[1] = "--resume".into();
                }
                create_profile = Some((captured, claude_home.as_deref()));
            }
            if let Some(extra) = config.agent_extra_args.get(agent.name()) {
                argv.extend(extra.iter().cloned());
            }
        } else {
            ensure!(
                !config.agent_launchers.iter().any(|launcher| {
                    std::path::Path::new(&launcher.executable).file_name()
                        == std::path::Path::new(&argv[0]).file_name()
                }),
                "configured agent launchers require an exact session reference"
            );
        }
        if let Some((profile, home)) = create_profile {
            let mut launch = vec!["/usr/bin/env".into()];
            if let Some(home) = home {
                launch.extend([
                    "-u".into(),
                    "CLAUDE_CONFIG_DIR".into(),
                    format!("HOME={home}"),
                ]);
            } else {
                launch.push(format!("CLAUDE_CONFIG_DIR={}", profile.display()));
            }
            launch.extend(argv);
            argv = launch;
        }
        validate_argv(&argv)?;
        if let Some(null_stdio) = self.null_stdio() {
            let mut script = String::from("exec \"$@\"");
            for (fd, redirect) in null_stdio.iter().enumerate() {
                if *redirect {
                    script.push_str(&format!(
                        " {fd}{} /dev/null",
                        if fd == 0 { "<" } else { ">" }
                    ));
                }
            }
            let mut launch = vec!["/bin/sh".into(), "-c".into(), script, "herdr-revive".into()];
            launch.extend(argv);
            validate_argv(&launch)?;
            return Ok(launch);
        }
        Ok(argv)
    }

    fn null_stdio(&self) -> Option<&[bool; 3]> {
        match self {
            Self::ProgramNullStdio { null_stdio, .. } => Some(null_stdio),
            Self::Agent { null_stdio, .. } if null_stdio.iter().any(|redirect| *redirect) => {
                Some(null_stdio)
            }
            _ => None,
        }
    }
}

pub(crate) fn configured_claude_profile(
    config: &Config,
    executable: &str,
) -> Result<(PathBuf, Option<PathBuf>)> {
    let configured = config
        .agent_launchers
        .iter()
        .find(|launcher| launcher.agent == AgentKind::Claude && launcher.executable == executable)
        .and_then(|launcher| launcher.match_env.get("CLAUDE_CONFIG_DIR"))
        .map(PathBuf::from);
    let configured =
        configured.or_else(|| std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from));
    let (path, home) = if let Some(path) = configured {
        (path, None)
    } else {
        let home = PathBuf::from(std::env::var_os("HOME").context("Claude HOME is unavailable")?);
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

pub fn claude_transcript_exists(config_dir: &Path, session_id: &str) -> Result<bool> {
    validate_session_id(session_id)?;
    ensure!(
        config_dir.is_absolute(),
        "Claude configuration directory must be absolute"
    );
    let projects = config_dir.join("projects");
    let entries = match std::fs::read_dir(&projects) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error).context("Claude project store is inaccessible"),
    };
    let name = format!("{session_id}.jsonl");
    let mut count = 0usize;
    for entry in entries {
        count += 1;
        ensure!(count <= 4096, "too many Claude project directories");
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        match std::fs::metadata(entry.path().join(&name)) {
            Ok(metadata) => {
                ensure!(
                    metadata.is_file(),
                    "Claude transcript is not a regular file"
                );
                if metadata.len() > 0 {
                    return Ok(true);
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("Claude transcript is inaccessible"),
        }
    }
    Ok(false)
}

/// Checks the canonical project directory without scanning every Claude project.
///
/// Claude replaces each non-ASCII-alphanumeric byte in an ordinary working
/// directory with `-`. Longer and non-ASCII paths need Claude's collision and
/// Unicode handling, so callers receive `None` and can use the exhaustive path.
pub fn claude_project_transcript_exists(
    config_dir: &Path,
    cwd: &Path,
    session_id: &str,
) -> Result<Option<bool>> {
    validate_session_id(session_id)?;
    ensure!(
        config_dir.is_absolute(),
        "Claude configuration directory must be absolute"
    );
    ensure!(
        cwd.is_absolute(),
        "Claude working directory must be absolute"
    );
    let cwd = match cwd.to_str() {
        Some(cwd) if cwd.is_ascii() => cwd,
        _ => return Ok(None),
    };
    let project: String = cwd
        .bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() {
                char::from(byte)
            } else {
                '-'
            }
        })
        .collect();
    if project.len() > 200 {
        return Ok(None);
    }
    let path = config_dir
        .join("projects")
        .join(project)
        .join(format!("{session_id}.jsonl"));
    match std::fs::metadata(path) {
        Ok(metadata) => {
            ensure!(
                metadata.is_file(),
                "Claude transcript is not a regular file"
            );
            Ok(Some(metadata.len() > 0))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Some(false)),
        Err(error) => Err(error).context("Claude transcript is inaccessible"),
    }
}

pub fn is_ssh(executable: &str) -> bool {
    std::path::Path::new(executable)
        .file_name()
        .is_some_and(|name| name == "ssh")
}

pub fn is_launcher(executable: &str) -> bool {
    matches!(
        std::path::Path::new(executable)
            .file_name()
            .and_then(|s| s.to_str()),
        Some(
            "node"
                | "nodejs"
                | "bun"
                | "deno"
                | "npx"
                | "npm"
                | "pnpm"
                | "yarn"
                | "env"
                | "sh"
                | "bash"
                | "dash"
                | "zsh"
                | "fish"
                | "python"
                | "python3"
                | "perl"
                | "ruby"
                | "pwsh"
                | "powershell"
                | "cmd"
        )
    )
}

pub fn agent_launcher(argv: &[String]) -> Option<AgentKind> {
    let executable = argv.first()?;
    let base = std::path::Path::new(executable).file_name()?.to_str()?;
    match base {
        "claude" => Some(AgentKind::Claude),
        "codex" => Some(AgentKind::Codex),
        "gemini" => Some(AgentKind::Gemini),
        "copilot" => Some(AgentKind::Copilot),
        "cursor-agent" => Some(AgentKind::Cursor),
        "node" | "nodejs" => {
            let script = std::path::Path::new(argv.get(1)?);
            if script.ends_with("@openai/codex/bin/codex.js") {
                Some(AgentKind::Codex)
            } else if script.ends_with("@anthropic-ai/claude-code/cli.js") {
                Some(AgentKind::Claude)
            } else if script.ends_with("@google/gemini-cli/dist/index.js") {
                Some(AgentKind::Gemini)
            } else if script.ends_with("@github/copilot/index.js") {
                Some(AgentKind::Copilot)
            } else {
                None
            }
        }
        _ => None,
    }
}

pub fn validate_session_id(id: &str) -> Result<()> {
    ensure!(
        id.len() == 36
            && id.bytes().enumerate().all(|(i, b)| {
                if [8, 13, 18, 23].contains(&i) {
                    b == b'-'
                } else {
                    b.is_ascii_hexdigit()
                }
            }),
        "agent session ID must be an exact UUID"
    );
    Ok(())
}

pub fn validate_text(text: &str) -> Result<()> {
    ensure!(text.len() <= 64 * 1024, "argument exceeds size limit");
    // PTY input is interpreted by the line editor before shell parsing.
    ensure!(
        !text.chars().any(char::is_control),
        "terminal control characters are not restorable"
    );
    Ok(())
}

pub fn validate_program(program: &str) -> Result<()> {
    validate_text(program)?;
    ensure!(
        !program.is_empty() && !program.starts_with('-'),
        "invalid executable"
    );
    ensure!(
        !program.contains('/') || std::path::Path::new(program).is_absolute(),
        "relative executable paths are not supported"
    );
    Ok(())
}

pub fn validate_argv(argv: &[String]) -> Result<()> {
    ensure!(
        !argv.is_empty() && argv.len() <= 4096,
        "invalid argument count"
    );
    validate_program(&argv[0])?;
    for arg in argv {
        validate_text(arg)?;
    }
    ensure!(
        argv.iter().map(String::len).sum::<usize>() <= 128 * 1024,
        "command exceeds size limit"
    );
    Ok(())
}

pub fn validate_id(id: &str) -> Result<()> {
    ensure!(
        !id.is_empty()
            && id.len() <= 128
            && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b':'),
        "invalid Herdr ID"
    );
    Ok(())
}

impl Snapshot {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            native_tool(&self.tool) && self.schema == SCHEMA,
            "unsupported snapshot identity or schema"
        );
        ensure!(
            self.session.len() == 64 && self.session.bytes().all(|b| b.is_ascii_hexdigit()),
            "invalid session identity"
        );
        ensure!(self.panes.len() <= 10_000, "too many panes");
        let mut seen = HashSet::new();
        if let Scope::Space { workspace_id } = &self.scope {
            validate_id(workspace_id)?;
        }
        for pane in &self.panes {
            validate_id(&pane.workspace_id)?;
            validate_id(&pane.tab_id)?;
            validate_id(&pane.pane_id)?;
            ensure!(
                pane.tab_id.starts_with(&format!("{}:t", pane.workspace_id))
                    && pane
                        .pane_id
                        .starts_with(&format!("{}:p", pane.workspace_id)),
                "inconsistent pane ancestry"
            );
            ensure!(seen.insert(&pane.pane_id), "duplicate pane ID");
            validate_text(&pane.cwd)?;
            ensure!(
                std::path::Path::new(&pane.cwd).is_absolute(),
                "pane cwd must be absolute"
            );
            if let Scope::Space { workspace_id } = &self.scope {
                ensure!(
                    &pane.workspace_id == workspace_id,
                    "pane outside named space"
                );
            }
            if let Some(command) = &pane.command {
                command.argv()?;
            }
        }
        crate::layout::validate(self)?;
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct LiveSnapshot {
    pub focused_pane_id: Option<String>,
    pub version: String,
    pub protocol: u32,
    pub workspaces: Vec<Workspace>,
    pub tabs: Vec<Tab>,
    pub panes: Vec<LivePane>,
    pub focused_workspace_id: Option<String>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Workspace {
    pub workspace_id: String,
    #[serde(default)]
    pub label: String,
    pub active_tab_id: Option<String>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Tab {
    pub workspace_id: String,
    pub tab_id: String,
    #[serde(default)]
    pub label: String,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct LivePane {
    pub workspace_id: String,
    pub tab_id: String,
    pub pane_id: String,
    pub terminal_id: String,
    pub cwd: Option<String>,
    pub agent: Option<String>,
    pub agent_session: Option<AgentSession>,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct AgentSession {
    pub source: String,
    pub agent: String,
    pub kind: String,
    pub value: String,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ProcessInfo {
    pub pane_id: String,
    pub shell_pid: Option<u32>,
    pub foreground_process_group_id: Option<u32>,
    #[serde(default)]
    pub foreground_processes: Vec<ForegroundProcess>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ForegroundProcess {
    pub pid: u32,
}

pub fn validate_protocol(version: &str, protocol: u32) -> Result<()> {
    ensure!(
        version == "0.9.1" && protocol == PROTOCOL,
        "unsupported Herdr version/protocol (validated: 0.9.1/22)"
    );
    Ok(())
}
