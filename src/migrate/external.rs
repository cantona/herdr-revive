//! The programs a migration drives, each by its bare name from PATH:
//! `herdr-app` (the window), `hc` (other machines) and `herdr` (the TUI's
//! session list). Every call has a deadline.

use crate::model::MAX_BYTES;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::Read;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

pub struct Ran {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl Ran {
    pub fn stderr_text(&self) -> String {
        String::from_utf8_lossy(&self.stderr).trim().to_string()
    }
}

/// A program that did not finish in time: what it did is unknown.
#[derive(Debug)]
pub struct TimedOut {
    program: String,
    timeout: Duration,
}

impl TimedOut {
    pub fn new(program: &str, timeout: Duration) -> Self {
        Self {
            program: program.into(),
            timeout,
        }
    }
}

impl std::fmt::Display for TimedOut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} did not finish within {} ms",
            self.program,
            self.timeout.as_millis()
        )
    }
}

impl std::error::Error for TimedOut {}

pub fn timed_out(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| cause.is::<TimedOut>())
}

fn read_capped(pipe: impl Read) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    pipe.take(MAX_BYTES as u64 + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= MAX_BYTES, "output exceeds size limit");
    Ok(bytes)
}

/// The pipes drain on their own threads so a chatty program cannot block on
/// a full pipe while this waits for it to exit.
pub fn run(program: &str, args: &[String], timeout: Duration) -> Result<Ran> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("cannot run {program}"))?;
    let stdout = child.stdout.take().context("stdout missing")?;
    let stderr = child.stderr.take().context("stderr missing")?;
    let stdout = drain(stdout);
    let stderr = drain(stderr);
    let deadline = Instant::now() + timeout;
    let too_slow = || anyhow::Error::new(TimedOut::new(program, timeout));
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            child.kill().ok();
            child.wait().ok();
            return Err(too_slow());
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    // A descendant that inherited a pipe keeps it open after the program
    // exits, so the readers get only what is left of the deadline.
    let collect = |reader: &Receiver<Result<Vec<u8>>>, stream: &str| -> Result<Vec<u8>> {
        reader
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .map_err(|_| too_slow())?
            .with_context(|| format!("{program} {stream}"))
    };
    let stdout = collect(&stdout, "stdout")?;
    let stderr = collect(&stderr, "stderr")?;
    Ok(Ran {
        status,
        stdout,
        stderr,
    })
}

fn drain(pipe: impl Read + Send + 'static) -> Receiver<Result<Vec<u8>>> {
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || sender.send(read_capped(pipe)).ok());
    receiver
}

pub fn run_json(program: &str, args: &[String], timeout: Duration) -> Result<Value> {
    let ran = run(program, args, timeout)?;
    let what = args.first().map(String::as_str).unwrap_or_default();
    ensure!(
        ran.status.success(),
        "{program} {what} failed ({}): {}",
        ran.status,
        ran.stderr_text()
    );
    serde_json::from_slice(&ran.stdout).with_context(|| format!("{program} {what} printed no JSON"))
}

const APP_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AppLs {
    pub spaces: Vec<AppSpace>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AppSpace {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub folder: String,
    #[serde(default)]
    pub machine: Option<String>,
    #[serde(default)]
    pub tabs: Vec<AppTab>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AppTab {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub terminals: Vec<AppTerminal>,
}

/// `pane` is optional so that one odd entry cannot fail a whole listing;
/// only a lookup that needs it fails.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AppTerminal {
    pub terminal: String,
    #[serde(default)]
    pub pane: Option<String>,
}

/// `herdr-app ls` answers only while the window is open.
pub fn app_ls() -> Result<AppLs> {
    let value = run_json("herdr-app", &["ls".to_string()], APP_TIMEOUT)
        .context("open the Herdr app first (herdr-app ls failed)")?;
    serde_json::from_value(value).context("herdr-app ls printed an unexpected shape")
}

/// The ids `herdr-app do` reports for what it made.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
pub struct Made {
    pub space: Option<String>,
    pub tab: Option<String>,
    pub pane: Option<String>,
    pub terminal: Option<String>,
}

pub trait AppDo {
    fn run(&mut self, command: &str, args: Value) -> Result<Made>;
    /// `space.new` and `tab.new` report no pane, so a copy finds the pane
    /// of the terminal they made here.
    fn ls(&mut self) -> Result<AppLs>;
}

pub struct AppCli;

impl AppDo for AppCli {
    fn run(&mut self, command: &str, args: Value) -> Result<Made> {
        let value = run_json(
            "herdr-app",
            &["do".to_string(), command.to_string(), args.to_string()],
            APP_TIMEOUT,
        )
        .with_context(|| format!("herdr-app do {command}"))?;
        serde_json::from_value(value)
            .with_context(|| format!("herdr-app do {command} printed an unexpected shape"))
    }

    fn ls(&mut self) -> Result<AppLs> {
        app_ls()
    }
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct Machine {
    pub name: String,
    pub state: String,
    #[serde(default)]
    pub temporary: bool,
}

pub fn machines() -> Result<Vec<Machine>> {
    let value = run_json(
        "hc",
        &[
            "machine".to_string(),
            "ls".to_string(),
            "--json".to_string(),
        ],
        Duration::from_secs(15),
    )?;
    serde_json::from_value(value["machines"].clone())
        .context("hc machine ls printed an unexpected shape")
}

/// The dialog's lookup (`mods/herdr-revive/main.js`): herdr-core's PATH
/// usually lacks `~/.local/bin` and `~/.cargo/bin`, where these are installed.
/// The two are appended to PATH as well, so a program run this way finds
/// `herdr` and the rest by bare name the same way the lookup found it.
pub const LOOKUP: &str = "PATH=\"${PATH:+$PATH:}$HOME/.local/bin:$HOME/.cargo/bin\"; export PATH; for f in \"$(command -v \"$0\" 2>/dev/null)\" \"$HOME/.local/bin/$0\" \"$HOME/.cargo/bin/$0\"; do [ -x \"$f\" ] && exec \"$f\" \"$@\"; done; echo \"cannot run $0: not on PATH, in ~/.local/bin or in ~/.cargo/bin\" >&2; exit 127";

/// `hc exec` passes the program's stdout, stderr and exit code through.
pub fn hc_exec(machine: &str, program: &str, args: &[String], timeout_secs: u64) -> Result<Ran> {
    let timeout = timeout_secs.to_string();
    let mut argv: Vec<String> = [
        "exec",
        "--on",
        machine,
        "--timeout",
        timeout.as_str(),
        "--",
        "sh",
        "-c",
        LOOKUP,
        program,
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    argv.extend(args.iter().cloned());
    run("hc", &argv, Duration::from_secs(timeout_secs + 10))
}

/// `name 1.2.3[-pre]` -> `(1, 2, 3)`.
pub fn parse_version(text: &str) -> Option<(u32, u32, u32)> {
    let word = text.split_whitespace().nth(1)?;
    let release = word.split(['-', '+']).next()?;
    let mut parts = release.split('.').map(|part| part.parse::<u32>().ok());
    Some((parts.next()??, parts.next()??, parts.next()??))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn strings(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn run_keeps_stdout_stderr_and_the_exit_code() {
        let ran = run(
            "sh",
            &strings(&["-c", "echo out; echo err >&2; exit 3"]),
            Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(ran.status.code(), Some(3));
        assert_eq!(ran.stdout, b"out\n");
        assert_eq!(ran.stderr_text(), "err");
    }

    #[test]
    fn run_stops_a_program_at_the_deadline() {
        let started = Instant::now();
        let error = run("sleep", &strings(&["5"]), Duration::from_millis(200))
            .err()
            .unwrap();
        assert!(error.to_string().contains("did not finish"));
        assert!(timed_out(&error));
        assert!(timed_out(&error.context("herdr-app do space.new")));
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[test]
    fn run_json_names_the_failure() {
        let error = run_json(
            "sh",
            &strings(&["-c", "echo nope >&2; exit 1"]),
            Duration::from_secs(5),
        )
        .err()
        .unwrap();
        assert!(format!("{error:#}").contains("nope"));
        assert!(!timed_out(&error));
        let error = run_json(
            "sh",
            &strings(&["-c", "echo not json"]),
            Duration::from_secs(5),
        )
        .err()
        .unwrap();
        assert!(format!("{error:#}").contains("printed no JSON"));
    }

    fn probe(dir: &std::path::Path, name: &str, tag: &str) {
        std::fs::create_dir_all(dir).unwrap();
        let tool = dir.join(name);
        std::fs::write(
            &tool,
            format!("#!/bin/sh\necho {tag}\nprintf '%s\\n' \"$@\"\n"),
        )
        .unwrap();
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn lookup(home: &std::path::Path, path: &str, args: &[&str]) -> std::process::Output {
        std::process::Command::new("sh")
            .args(["-c", LOOKUP, "revive-lookup-probe"])
            .args(args)
            .env("HOME", home)
            .env("PATH", path)
            .output()
            .unwrap()
    }

    #[test]
    fn the_lookup_tries_path_then_local_bin_then_cargo_bin() {
        let home = tempfile::tempdir().unwrap();
        let on_path = tempfile::tempdir().unwrap();
        let path = format!("{}:/usr/bin:/bin", on_path.path().display());
        probe(
            &home.path().join(".cargo/bin"),
            "revive-lookup-probe",
            "cargo",
        );
        let out = lookup(home.path(), "/usr/bin:/bin", &["a b"]);
        assert_eq!(String::from_utf8(out.stdout).unwrap(), "cargo\na b\n");
        probe(
            &home.path().join(".local/bin"),
            "revive-lookup-probe",
            "local",
        );
        let out = lookup(home.path(), "/usr/bin:/bin", &["a", "b"]);
        assert_eq!(String::from_utf8(out.stdout).unwrap(), "local\na\nb\n");
        probe(on_path.path(), "revive-lookup-probe", "path");
        let out = lookup(home.path(), &path, &[]);
        assert_eq!(String::from_utf8(out.stdout).unwrap(), "path\n\n");
        let missing = std::process::Command::new("sh")
            .args(["-c", LOOKUP, "revive-missing-probe"])
            .env("HOME", home.path())
            .env("PATH", "/usr/bin:/bin")
            .output()
            .unwrap();
        assert_eq!(missing.status.code(), Some(127));
        assert!(
            String::from_utf8(missing.stderr)
                .unwrap()
                .contains("cannot run revive-missing-probe")
        );
    }

    #[test]
    fn the_lookup_lets_the_program_find_its_own_tools_the_same_way() {
        let home = tempfile::tempdir().unwrap();
        let local = home.path().join(".local/bin");
        let cargo = home.path().join(".cargo/bin");
        std::fs::create_dir_all(&local).unwrap();
        probe(&cargo, "revive-lookup-child", "child");
        let parent = local.join("revive-lookup-probe");
        std::fs::write(
            &parent,
            "#!/bin/sh\nprintf '%s\\n' \"$PATH\"\nexec revive-lookup-child x\n",
        )
        .unwrap();
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o755)).unwrap();
        let out = lookup(home.path(), "/usr/bin:/bin", &[]);
        assert_eq!(
            String::from_utf8(out.stdout).unwrap(),
            format!(
                "/usr/bin:/bin:{}:{}\nchild\nx\n",
                local.display(),
                cargo.display()
            )
        );
    }

    #[test]
    fn run_gives_up_on_a_descendant_holding_the_pipe() {
        let started = Instant::now();
        let error = run(
            "sh",
            &strings(&["-c", "sleep 5 & echo hi"]),
            Duration::from_millis(200),
        )
        .err()
        .unwrap();
        assert!(error.to_string().contains("did not finish"));
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[test]
    fn app_tabs_keep_the_pane_of_each_terminal() {
        let tab: AppTab = serde_json::from_value(serde_json::json!({"id": "tb1", "name": "1",
            "terminals": [{"terminal": "t1", "pane": "p1", "status": null, "sub_agents": []}]}))
        .unwrap();
        assert_eq!(tab.terminals[0].terminal, "t1");
        assert_eq!(tab.terminals[0].pane.as_deref(), Some("p1"));
        let bare: AppTab = serde_json::from_value(serde_json::json!({"id": "tb2"})).unwrap();
        assert!(bare.terminals.is_empty());
        let odd: AppTab = serde_json::from_value(serde_json::json!({"id": "tb3",
            "terminals": [{"terminal": "t2"}, {"terminal": "t3", "pane": null}]}))
        .unwrap();
        assert_eq!(odd.terminals[0].pane, None);
        assert_eq!(odd.terminals[1].pane, None);
    }

    #[test]
    fn versions_parse_from_the_version_line() {
        assert_eq!(parse_version("herdr-revive 0.3.0\n"), Some((0, 3, 0)));
        assert_eq!(parse_version("herdr 0.9.1-preview.2"), Some((0, 9, 1)));
        assert_eq!(parse_version("herdr-revive"), None);
        assert_eq!(parse_version("herdr-revive x.y"), None);
        assert!(parse_version("herdr-revive 0.10.0").unwrap() >= (0, 3, 0));
        assert!(parse_version("herdr-revive 0.2.9").unwrap() < (0, 3, 0));
    }

    #[test]
    fn app_listings_parse_with_unknown_fields() {
        let value = serde_json::json!({"spaces": [{"id": "s1", "name": "api", "folder": "/srv",
            "machine": null, "active": true, "tabs": [{"id": "tb1", "name": "1", "active": true,
            "settled": false, "terminals": [{"terminal": "t1", "pane": "p1", "status": null,
            "app": null, "name": "bash", "sub_agents": []}]}]}]});
        let ls: AppLs = serde_json::from_value(value).unwrap();
        assert_eq!(ls.spaces[0].id, "s1");
        assert_eq!(ls.spaces[0].tabs[0].id, "tb1");
        let made: Made =
            serde_json::from_value(serde_json::json!({"pane": "p9", "terminal": "t9"})).unwrap();
        assert_eq!(made.pane.as_deref(), Some("p9"));
        assert_eq!(made.space, None);
    }
}
