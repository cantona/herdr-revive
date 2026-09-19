use crate::model::{ProcessInfo, validate_argv, validate_text};
use anyhow::{Context, Result, ensure};
use std::collections::HashMap;
use std::path::Path;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::*;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::*;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Process {
    pub pid: u32,
    pub parent: u32,
    pub group: u32,
    pub start: u64,
}

pub fn validate_foreground_tree(table: &HashMap<u32, Process>, leader: &Process) -> Result<()> {
    for process in table
        .values()
        .filter(|p| p.group == leader.group && p.pid != leader.pid)
    {
        let mut parent = process.parent;
        let mut visited = std::collections::HashSet::new();
        while parent != leader.pid {
            ensure!(visited.insert(parent), "process ancestry contains a cycle");
            parent = table
                .get(&parent)
                .context("pipeline or ambiguous foreground process group refused")?
                .parent;
        }
    }
    Ok(())
}

pub fn is_shell_process(pid: u32) -> Result<bool> {
    let exe = executable(pid)?;
    Ok(exe
        .file_name()
        .and_then(|s| s.to_str())
        .is_some_and(|name| {
            [
                "bash", "dash", "zsh", "fish", "ksh", "mksh", "tcsh", "csh", "nu", "pwsh",
            ]
            .contains(&name)
        }))
}

pub fn decode_argv(bytes: &[u8]) -> Result<Vec<String>> {
    ensure!(
        bytes.last() == Some(&0),
        "missing or truncated process argv"
    );
    let argv = bytes[..bytes.len() - 1]
        .split(|b| *b == 0)
        .map(|arg| String::from_utf8(arg.to_vec()).context("non-UTF-8 process arguments refused"))
        .collect::<Result<Vec<_>>>()?;
    ensure!(
        !argv.is_empty() && argv.len() <= 4096,
        "invalid process argument count"
    );
    for arg in &argv {
        validate_text(arg)?;
    }
    Ok(argv)
}

pub fn idle_shell(info: &ProcessInfo) -> Result<Option<String>> {
    let pid = info.shell_pid.context("shell PID unavailable")?;
    if !is_shell_process(pid)? {
        return Ok(None);
    }
    let shell = process(pid)?;
    if info.foreground_process_group_id != Some(shell.group)
        || info.foreground_processes.len() != 1
        || info.foreground_processes[0].pid != pid
    {
        return Ok(None);
    }
    // A direct child implies a descendant, including background jobs.
    if has_children(&shell)? {
        return Ok(None);
    }
    let (argv, _) = argv_and_cwd(&shell)?;
    let exe = executable(pid)?;
    let name = exe
        .file_name()
        .and_then(|s| s.to_str())
        .context("shell executable unavailable")?;
    ensure!(
        matches!(name, "bash" | "dash" | "zsh"),
        "unsupported shell; only bash, dash and zsh are encoded"
    );
    let arg0 = Path::new(argv[0].trim_start_matches('-'))
        .file_name()
        .and_then(|s| s.to_str());
    ensure!(
        matches!(arg0, Some("bash" | "dash" | "sh" | "zsh")),
        "shell argv does not identify a supported shell"
    );
    ensure!(
        !argv
            .iter()
            .skip(1)
            .any(|a| a == "-c" || (a.starts_with('-') && !a.starts_with("--") && a.contains('c'))),
        "noninteractive shell refused"
    );
    Ok(Some(name.into()))
}

pub fn shell_command(shell: &str, cwd: &str, argv: &[String]) -> Result<String> {
    ensure!(
        matches!(shell, "bash" | "dash" | "zsh"),
        "unsupported shell encoder"
    );
    validate_argv(argv)?;
    validate_text(cwd)?;
    ensure!(Path::new(cwd).is_absolute(), "cwd must be absolute");
    let quote = |s: &str| format!("'{}'", s.replace('\'', "'\\''"));
    Ok(format!(
        " cd -- {} && command {}",
        quote(cwd),
        argv.iter().map(|a| quote(a)).collect::<Vec<_>>().join(" ")
    ))
}

static IDENTITY_CONNECTIONS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

pub fn identity_connections() -> usize {
    IDENTITY_CONNECTIONS.load(std::sync::atomic::Ordering::Relaxed)
}
