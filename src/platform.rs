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

fn plain_shell_command_matches(script: &str, argv: &[String]) -> bool {
    !script.is_empty()
        && script.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b' ' | b'\t'
                        | b'/'
                        | b'.'
                        | b'_'
                        | b'-'
                        | b'+'
                        | b'='
                        | b','
                        | b':'
                        | b'%'
                        | b'@'
                )
        })
        && script
            .split_ascii_whitespace()
            .eq(argv.iter().map(String::as_str))
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
    Ok(Some(interactive_shell_name(&shell)?))
}

fn interactive_shell_name(shell: &Process) -> Result<String> {
    let (argv, _) = argv_and_cwd(shell)?;
    let exe = executable(shell.pid)?;
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
    Ok(name.into())
}

/// The shell evaluating revive's own `init` output: this process's parent,
/// an interactive shell holding its terminal's foreground group, and itself a
/// member of that group. Other members of the shell's own group are ignored:
/// while bash reads its startup files it has no job control, so a `cmd &`
/// there joins the shell's group, and no foreground command can exist yet.
/// A foreign foreground group means a command is already running there and
/// the relaunch is skipped.
pub fn startup_shell(info: &ProcessInfo, parent: u32) -> Result<Option<String>> {
    let pid = info.shell_pid.context("shell PID unavailable")?;
    if pid != parent || !is_shell_process(pid)? {
        return Ok(None);
    }
    let shell = process(pid)?;
    if info.foreground_process_group_id != Some(shell.group)
        || !info.foreground_processes.iter().any(|p| p.pid == pid)
    {
        return Ok(None);
    }
    Ok(Some(interactive_shell_name(&shell)?))
}

/// Linux's mark on a process cwd whose folder was removed.
pub const DELETED: &str = " (deleted)";

/// A folder as it stands on this machine, or `None` when it is gone. Linux
/// names a removed cwd `<path> (deleted)`, and a folder can be made again at
/// that path (`rm -rf build && mkdir build`), so the path without the mark
/// is what counts; a folder whose own name ends in the mark is itself.
pub fn existing_folder(cwd: &str) -> Option<&str> {
    let is_dir = |path: &str| Path::new(path).is_dir();
    if is_dir(cwd) {
        return Some(cwd);
    }
    let path = cwd.strip_suffix(DELETED)?;
    is_dir(path).then_some(path)
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

/// Runs `"$@"` through the user's login shell, which replaces itself with
/// the program, so the program is found on the PATH the login profile sets:
/// herdr-core, and a TUI server started outside a login session, never read
/// `~/.profile`. No shell stays; the caller keeps one. fish has no `"$@"`
/// and keeps its PATH in `config.fish`; any other shell gets `sh`'s login
/// profile. Every value travels as a positional parameter.
pub const LOGIN_RUN: &str = concat!(
    r#"case ${SHELL##*/} in "#,
    r#"fish) exec "$SHELL" -lc 'exec $argv' "$@" ;; "#,
    r#"sh|bash|zsh|dash|ksh|mksh) exec "$SHELL" -lc 'exec "$@"' "$SHELL" "$@" ;; "#,
    r#"*) exec sh -lc 'exec "$@"' sh "$@" ;; esac"#,
);

/// `LOGIN_RUN` for a pane nothing else keeps open: it changes into `$0`,
/// runs `"$@"` from an interactive login shell, and a login shell stays once
/// the program ends, Ctrl-C included. Interactive so the program is its own
/// foreground job, and a later capture sees the program rather than this
/// wrapper (fish has to be asked for that). An interactive shell drops the
/// rest of its command line when its job dies of SIGINT, so the POSIX shells
/// `exec` the shell that stays from an INT trap too (a caught trap is reset
/// in the program, which still gets the signal); fish runs no trap there,
/// so it runs the program in a child fish and this `sh` starts the one that
/// stays. The fallback's `-l` stands alone because tcsh takes no other flag
/// beside it. A folder that is gone (removed after the plan read it) leaves
/// the shell in the home folder without the program, which would run in the
/// wrong place, rather than ending the pane.
pub const LOGIN_PANE: &str = concat!(
    r#"cd -- "$0" || { echo "herdr-revive: $0 is gone, so $1 was not started" >&2; cd || cd /; "#,
    r#"case ${SHELL##*/} in sh|bash|zsh|dash|ksh|mksh|fish) exec "$SHELL" -li ;; "#,
    r#"*) exec "${SHELL:-sh}" -l ;; esac; }; case ${SHELL##*/} in "#,
    r#"fish) "$SHELL" -lic 'status job-control full; command $argv' "$@"; exec "$SHELL" -li ;; "#,
    r#"sh|bash|zsh|dash|ksh|mksh) exec "$SHELL" -lic 'trap '\''exec "$0" -li'\'' INT; "$@"; exec "$0" -li' "$SHELL" "$@" ;; "#,
    r#"*) exec sh -lic 'trap '\''exec "${SHELL:-sh}" -l'\'' INT; "$@"; exec "${SHELL:-sh}" -l' sh "$@" ;; esac"#,
);

/// `LOGIN_PANE` for a pane whose own shell another program keeps: herdr-app
/// runs a `space.new`/`tab.new` argv as a job of its `$SHELL -ic` (`-lic` on
/// macOS and on another machine), which `exec`s an interactive shell once
/// that job ends. Only that shell is the pane's own process, the one the rc
/// hook's `init` and capture accept, so nothing here stays: the program runs
/// from an interactive login shell, as its own foreground job, and that
/// shell exits when it ends. It has to exit rather than die of SIGINT, or
/// herdr-app's shell drops its `exec` too and the pane closes: the POSIX
/// shells trap INT, and fish, which runs no trap there, runs as a child of
/// this `sh`, which exits with fish's status. A folder that is gone ends the
/// job without the program; herdr-app's shell stays in the terminal's own
/// folder.
pub const LOGIN_JOB: &str = concat!(
    r#"cd -- "$0" || { echo "herdr-revive: $0 is gone, so $1 was not started" >&2; exit 1; }; "#,
    r#"case ${SHELL##*/} in "#,
    r#"fish) "$SHELL" -lic 'status job-control full; command $argv' "$@" ;; "#,
    r#"sh|bash|zsh|dash|ksh|mksh) exec "$SHELL" -lic 'trap "exit 130" INT; "$@"' "$SHELL" "$@" ;; "#,
    r#"*) exec sh -lic 'trap "exit 130" INT; "$@"' sh "$@" ;; esac"#,
);

/// `argv` through the login shell (`LOGIN_RUN`), in the caller's cwd.
pub fn login_command(argv: &[String]) -> Vec<String> {
    let mut wrapped = vec!["sh".into(), "-c".into(), LOGIN_RUN.into(), "sh".into()];
    wrapped.extend(argv.iter().cloned());
    wrapped
}

/// `argv` in `cwd` through the login shell, which stays (`LOGIN_PANE`).
pub fn login_pane(cwd: &str, argv: &[String]) -> Vec<String> {
    let mut wrapped = vec!["sh".into(), "-c".into(), LOGIN_PANE.into(), cwd.into()];
    wrapped.extend(argv.iter().cloned());
    wrapped
}

/// `argv` in `cwd` through the login shell, as a job of a shell that the
/// caller's program keeps (`LOGIN_JOB`).
pub fn login_job(cwd: &str, argv: &[String]) -> Vec<String> {
    let mut wrapped = vec!["sh".into(), "-c".into(), LOGIN_JOB.into(), cwd.into()];
    wrapped.extend(argv.iter().cloned());
    wrapped
}

static IDENTITY_CONNECTIONS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

pub fn identity_connections() -> usize {
    IDENTITY_CONNECTIONS.load(std::sync::atomic::Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::{login_command, login_job, login_pane, plain_shell_command_matches, shell_command};
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};

    #[test]
    fn a_folder_counts_by_its_path_without_the_removed_mark() {
        let root = tempfile::tempdir().unwrap();
        let build = root.path().join("build");
        let marked = root.path().join("odd (deleted)");
        std::fs::create_dir(&build).unwrap();
        std::fs::create_dir(&marked).unwrap();
        let text = |path: &std::path::Path| path.to_str().unwrap().to_string();
        let (build, marked) = (text(&build), text(&marked));
        assert_eq!(super::existing_folder(&build), Some(build.as_str()));
        let recreated = format!("{build} (deleted)");
        assert_eq!(
            super::existing_folder(&recreated),
            Some(build.as_str()),
            "removed and made again at the same path"
        );
        assert_eq!(
            super::existing_folder(&marked),
            Some(marked.as_str()),
            "a folder whose own name ends in the mark"
        );
        let gone = text(&root.path().join("gone"));
        assert_eq!(super::existing_folder(&format!("{gone} (deleted)")), None);
        assert_eq!(super::existing_folder(&gone), None);
    }

    #[test]
    fn plain_shell_command_rejects_expansion_and_operators() {
        let pager = vec!["less".into(), "-FRX".into()];
        assert!(plain_shell_command_matches("less -FRX", &pager));
        assert!(!plain_shell_command_matches("less -FRX; true", &pager));
        assert!(!plain_shell_command_matches("less $PAGER_ARGS", &pager));
        assert!(!plain_shell_command_matches("less -FRX | more", &pager));
    }

    const ODD_ARGS: [&str; 6] = ["a b", "$HOME", "'q\"", "", "*", "-l"];

    /// A HOME whose login profiles (POSIX and fish) put `stubbin` on PATH
    /// and count each read in `sourced`, a folder whose name needs every
    /// kind of quoting, and the stub program only that PATH can find.
    fn login_home() -> (tempfile::TempDir, PathBuf) {
        let home = tempfile::tempdir().unwrap();
        let bin = home.path().join("stubbin");
        std::fs::create_dir(&bin).unwrap();
        let stub = bin.join("revive-login-stub");
        std::fs::write(
            &stub,
            "#!/bin/sh\npwd -P\nfor a in \"$@\"; do printf '[%s]\\n' \"$a\"; done\n",
        )
        .unwrap();
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(
            home.path().join(".profile"),
            "PATH=\"$HOME/stubbin:$PATH\"\necho x >> \"$HOME/sourced\"\n",
        )
        .unwrap();
        let fish = home.path().join(".config/fish");
        std::fs::create_dir_all(&fish).unwrap();
        std::fs::write(
            fish.join("config.fish"),
            "set -gx PATH $HOME/stubbin $PATH\n",
        )
        .unwrap();
        let dir = home.path().join("x y$z'q\"");
        std::fs::create_dir(&dir).unwrap();
        let dir = dir.canonicalize().unwrap();
        (home, dir)
    }

    /// The shells to try: none set, `sh`, and bash and fish where installed.
    fn shells() -> Vec<Option<&'static str>> {
        let mut shells = vec![None, Some("/bin/sh")];
        for shell in ["/bin/bash", "/usr/bin/fish"] {
            if Path::new(shell).exists() {
                shells.push(Some(shell));
            }
        }
        shells
    }

    /// `argv` under `env -i` with only HOME, a PATH without the stub and
    /// SHELL, stdin closed so a shell that stays exits at once.
    fn run_clean(home: &Path, shell: Option<&str>, cwd: &Path, argv: &[String]) -> String {
        let mut command = Command::new("env");
        command
            .arg("-i")
            .arg(format!("HOME={}", home.display()))
            .arg("PATH=/usr/bin:/bin");
        if let Some(shell) = shell {
            command.arg(format!("SHELL={shell}"));
        }
        let output = command
            .args(argv)
            .current_dir(cwd)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{shell:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }

    fn expected(dir: &Path) -> String {
        let mut lines = vec![dir.to_str().unwrap().to_string()];
        lines.extend(ODD_ARGS.iter().map(|arg| format!("[{arg}]")));
        lines.join("\n") + "\n"
    }

    fn odd_argv() -> Vec<String> {
        let mut argv = vec!["revive-login-stub".to_string()];
        argv.extend(ODD_ARGS.iter().map(|arg| arg.to_string()));
        argv
    }

    fn reads(home: &Path) -> usize {
        std::fs::read_to_string(home.join("sourced"))
            .unwrap_or_default()
            .lines()
            .count()
    }

    #[test]
    fn a_login_pane_finds_the_program_on_the_login_path_and_keeps_a_login_shell() {
        for shell in shells() {
            let (home, dir) = login_home();
            let argv = login_pane(dir.to_str().unwrap(), &odd_argv());
            let stdout = run_clean(home.path(), shell, Path::new("/"), &argv);
            assert_eq!(stdout, expected(&dir), "{shell:?}");
            if shell != Some("/usr/bin/fish") {
                assert_eq!(
                    reads(home.path()),
                    2,
                    "{shell:?}: the program's login shell, then the one that stays"
                );
            }
        }
    }

    #[test]
    fn a_login_job_finds_the_program_on_the_login_path_and_keeps_nothing() {
        for shell in shells() {
            let (home, dir) = login_home();
            let argv = login_job(dir.to_str().unwrap(), &odd_argv());
            let stdout = run_clean(home.path(), shell, Path::new("/"), &argv);
            assert_eq!(stdout, expected(&dir), "{shell:?}");
            if shell != Some("/usr/bin/fish") {
                assert_eq!(reads(home.path()), 1, "{shell:?}: only the program's shell");
            }
        }
    }

    #[test]
    fn a_login_command_finds_the_program_on_the_login_path_and_keeps_nothing() {
        for shell in shells() {
            let (home, dir) = login_home();
            let stdout = run_clean(home.path(), shell, &dir, &login_command(&odd_argv()));
            assert_eq!(stdout, expected(&dir), "{shell:?}");
            if shell != Some("/usr/bin/fish") {
                assert_eq!(reads(home.path()), 1, "{shell:?}");
            }
        }
    }

    #[test]
    fn a_login_command_survives_being_typed_into_a_shell() {
        let (home, dir) = login_home();
        let line =
            shell_command("dash", dir.to_str().unwrap(), &login_command(&odd_argv())).unwrap();
        let argv = ["sh".to_string(), "-c".to_string(), line];
        let stdout = run_clean(home.path(), Some("/bin/sh"), Path::new("/"), &argv);
        assert_eq!(stdout, expected(&dir));
    }

    /// Runs argv (JSON in argv[1]) with env (JSON in argv[2]) on a pty as the
    /// pane's own process, waits until `sleep` leads the foreground group
    /// (not in mode `gone`, whose folder is missing), then, in mode `int`,
    /// sends Ctrl-C. Prints whether the program led,
    /// whether the pane's own process leads once it is gone and its name,
    /// whether that shell ran a typed command, and whether one typed `exit`
    /// ended the pane. Answers the terminal queries fish waits on.
    const PANE_PROBE: &str = r#"
import json, os, pty, select, subprocess, sys, time
argv, env, mode = json.loads(sys.argv[1]), json.loads(sys.argv[2]), sys.argv[3]
pid, fd = pty.fork()
if pid == 0:
    os.chdir("/")
    os.execvpe(argv[0], argv, env)
out = bytearray()
def pump(wait):
    if select.select([fd], [], [], wait)[0]:
        try:
            data = os.read(fd, 65536)
        except OSError:
            return False
        if not data:
            return False
        out.extend(data)
        if b"\x1b[c" in data or b"\x1b[0c" in data:
            os.write(fd, b"\x1b[?62;22c")
        if b"\x1b[6n" in data:
            os.write(fd, b"\x1b[1;1R")
    return True
def group():
    try:
        return os.tcgetpgrp(fd)
    except OSError:
        return None
def name(pgrp):
    if pgrp is None:
        return None
    if sys.platform != "darwin":
        try:
            with open("/proc/%d/comm" % pgrp) as f:
                return f.read().strip()
        except OSError:
            return None
    ps = subprocess.run(["/bin/ps", "-o", "comm=", "-p", str(pgrp)],
                        capture_output=True, text=True).stdout.strip()
    return os.path.basename(ps).lstrip("-") or None
def folder(pid):
    if sys.platform != "darwin":
        try:
            return os.readlink("/proc/%d/cwd" % pid)
        except OSError:
            return None
    lsof = subprocess.run(["/usr/sbin/lsof", "-a", "-p", str(pid), "-d", "cwd", "-Fn"],
                          capture_output=True, text=True).stdout
    return next((line[1:] for line in lsof.splitlines() if line.startswith("n")), None)
def until(test, seconds=10):
    end = time.monotonic() + seconds
    while time.monotonic() < end:
        if test():
            return True
        if not pump(0.05):
            break
    return test()
led = mode != "gone" and until(lambda: name(group()) == "sleep")
if mode == "int":
    os.write(fd, b"\x03")
own = until(lambda: group() == pid)
start = len(out)
os.write(fd, b"echo AL''IVE\r")
answered = until(lambda: b"ALIVE" in bytes(out[start:]).replace(b"AL''IVE", b""))
# Named only once it answers: the split pane's fish case leads with the
# wrapper's `sh` until that `exec`s the fish that stays.
after = name(group()) if group() == pid else None
cwd = folder(pid)
os.write(fd, b"exit\r")
closed = False
end = time.monotonic() + 10
while time.monotonic() < end:
    if os.waitpid(pid, os.WNOHANG)[0] == pid:
        closed = True
        break
    if not pump(0.05):
        time.sleep(0.05)
if not closed:
    os.kill(pid, 9)
    os.waitpid(pid, 0)
said = b"was not started" in bytes(out)
print(json.dumps({"led": led, "own": own, "after": after, "answered": answered, "closed": closed,
                  "cwd": cwd, "said": said}))
"#;

    fn python() -> bool {
        Command::new("python3")
            .args(["-I", "-c", ""])
            .status()
            .is_ok_and(|status| status.success())
    }

    fn in_pty(home: &Path, shell: Option<&str>, argv: &[String], mode: &str) -> serde_json::Value {
        let mut env = serde_json::json!({
            "HOME": home, "PATH": "/usr/bin:/bin", "TERM": "xterm"
        });
        if let Some(shell) = shell {
            env["SHELL"] = shell.into();
        }
        let output = Command::new("python3")
            .args(["-I", "-c", PANE_PROBE])
            .arg(serde_json::to_string(argv).unwrap())
            .arg(env.to_string())
            .arg(mode)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{shell:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }

    /// The shell `${SHELL:-sh}` starts with SHELL unset: dash leaves it
    /// unset, but bash as `sh` (macOS) sets it from the passwd entry.
    fn shell_when_unset() -> String {
        let output = Command::new("env")
            .args([
                "-i",
                "PATH=/usr/bin:/bin",
                "sh",
                "-c",
                "echo \"${SHELL:-sh}\"",
            ])
            .output()
            .unwrap();
        let path = String::from_utf8(output.stdout).unwrap();
        let name = Path::new(path.trim()).file_name().unwrap();
        name.to_str().unwrap().to_string()
    }

    /// The shells installed here, with the name the one that stays runs as.
    fn installed_shells() -> Vec<(Option<&'static str>, &'static str)> {
        let mut shells = vec![(None, "sh")];
        for (path, name) in [
            ("/bin/sh", "sh"),
            ("/bin/bash", "bash"),
            ("/bin/dash", "dash"),
            ("/usr/bin/zsh", "zsh"),
            ("/usr/bin/ksh", "ksh"),
            ("/usr/bin/mksh", "mksh"),
            ("/usr/bin/fish", "fish"),
        ] {
            if Path::new(path).exists() {
                shells.push((Some(path), name));
            } else {
                eprintln!("{path} is not installed; its pane cases are skipped");
            }
        }
        shells
    }

    /// herdr-app's `remote_shell::launch` script, which runs a `space.new`
    /// or `tab.new` argv on another machine.
    const APP_REMOTE: &str = r#"shell=${SHELL:-/bin/sh}
case "${shell##*/}" in
  fish) exec "$shell" -lic 'command $argv[2..]; exec $argv[1] -li' "$shell" "$@" ;;
  sh|bash|zsh|dash|ksh) exec "$shell" -lic '"$@"; exec "$0" -li' "$shell" "$@" ;;
  *) exec sh -c '"$@"; exec "${SHELL:-sh}" -i' sh "$@" ;;
esac"#;

    #[derive(Clone, Copy, Debug, PartialEq)]
    enum App {
        Linux,
        Mac,
        Remote,
    }

    /// The argv herdr-app's `space.new`/`tab.new` runs a command in: a job
    /// of `$SHELL -ic` on Linux and `-lic` on macOS (`window.rs`
    /// `unix_agent_argv`), `APP_REMOTE` on another machine.
    fn app_wrapped(app: App, shell: Option<&str>, command: Vec<String>) -> Vec<String> {
        let sh = shell.unwrap_or("/bin/sh").to_string();
        let mut argv: Vec<String> = match app {
            App::Remote => vec!["sh".into(), "-c".into(), APP_REMOTE.into(), "sh".into()],
            App::Linux | App::Mac => {
                let (flags, stays) = if app == App::Mac {
                    ("-lic", "-li")
                } else {
                    ("-ic", "-i")
                };
                let script = if sh.ends_with("/fish") {
                    format!("command $argv[2..]; exec $argv[1] {stays}")
                } else {
                    format!("\"$@\"; exec \"$0\" {stays}")
                };
                vec![sh.clone(), flags.into(), script, sh]
            }
        };
        argv.extend(command);
        argv
    }

    #[test]
    fn a_pane_keeps_its_own_shell_when_the_program_ends_or_is_stopped_with_ctrl_c() {
        if !python() {
            eprintln!("python3 is not installed; the pty cases are skipped");
            return;
        }
        let mut cases = Vec::new();
        let mut homes = Vec::new();
        let unset = shell_when_unset();
        for (shell, name) in installed_shells() {
            // The split pane and the remote script stay in `${SHELL:-...}`.
            let from_shell = if shell.is_none() {
                unset.as_str()
            } else {
                name
            };
            for (mode, seconds) in [("int", "30"), ("end", "1"), ("gone", "30")] {
                let sleep = vec!["sleep".to_string(), seconds.to_string()];
                let (home, dir) = login_home();
                let dir = match mode {
                    "gone" => home.path().join("gone"),
                    _ => dir,
                };
                let dir = dir.to_str().unwrap().to_string();
                // A split pane's shell stays in its folder, or in the home
                // folder when that is gone; herdr-app's own shell stays where
                // its terminal started, which is `/` in the probe.
                let split_cwd = match mode {
                    "gone" => home.path().canonicalize().unwrap(),
                    _ => PathBuf::from(&dir).canonicalize().unwrap(),
                };
                let mut forms = vec![(
                    "split pane".to_string(),
                    login_pane(&dir, &sleep),
                    split_cwd,
                    from_shell,
                )];
                for app in [App::Linux, App::Mac, App::Remote] {
                    // herdr-app runs mksh's argv from a non-interactive `sh -c`
                    // on another machine, which Ctrl-C ends with the program.
                    if app == App::Remote && name == "mksh" {
                        continue;
                    }
                    let argv = app_wrapped(app, shell, login_job(&dir, &sleep));
                    forms.push((
                        format!("app's first pane ({app:?})"),
                        argv,
                        PathBuf::from("/"),
                        if app == App::Remote { from_shell } else { name },
                    ));
                }
                for (form, argv, cwd, after) in forms {
                    let expected = serde_json::json!({
                        "led": mode != "gone", "own": true, "after": after, "answered": true,
                        "closed": true, "cwd": cwd, "said": mode == "gone"
                    });
                    cases.push((shell, mode, form, argv, home.path().to_path_buf(), expected));
                }
                homes.push(home);
            }
        }
        let results: Vec<_> = std::thread::scope(|scope| {
            let runs: Vec<_> = cases
                .iter()
                .map(|(shell, mode, _, argv, home, _)| {
                    scope.spawn(move || in_pty(home, *shell, argv, mode))
                })
                .collect();
            runs.into_iter().map(|run| run.join().unwrap()).collect()
        });
        // Every case is checked before failing, so one run names them all. The
        // program leads the foreground group (it never starts when its folder
        // is gone, and the pane says so); once it is gone the pane's own
        // process does, under the shell's name, in the expected folder,
        // answers, and one exit ends it.
        let failed: Vec<String> = cases
            .iter()
            .zip(results)
            .filter(|((.., expected), result)| result != expected)
            .map(|((shell, mode, form, ..), result)| format!("{shell:?} {form} {mode}: {result}"))
            .collect();
        assert!(failed.is_empty(), "{}", failed.join("\n"));
    }

    #[test]
    fn without_the_wrapper_the_minimal_path_does_not_find_the_program() {
        let (home, dir) = login_home();
        let output = Command::new("env")
            .arg("-i")
            .arg(format!("HOME={}", home.path().display()))
            .arg("PATH=/usr/bin:/bin")
            .args(odd_argv())
            .current_dir(&dir)
            .output()
            .unwrap();
        assert!(
            !output.status.success(),
            "the stub must need the login PATH"
        );
    }
}
