mod common;

use common::fake_core::{FakeCore, record};
use common::shell::Shell;
use herdr_revive::core::paths;
use herdr_revive::layout::{Node, TabLayout, WorkspaceLayout};
use herdr_revive::model::*;
use herdr_revive::store::Store;
use serde_json::Value;
use std::path::Path;
use std::process::Command;

fn revive() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_herdr-revive"));
    command.env_clear();
    command.env("PATH", std::env::var("PATH").unwrap_or_default());
    command.env("HOME", "/tmp");
    command
}

fn run(command: &mut Command) -> (bool, Value, String) {
    let output = command.output().unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();
    let value = serde_json::from_str(&stdout).unwrap_or(Value::Null);
    (output.status.success(), value, stderr)
}

struct Dirs {
    root: tempfile::TempDir,
}

impl Dirs {
    fn new() -> Self {
        let root = tempfile::tempdir_in("/tmp").unwrap();
        std::fs::create_dir(root.path().join("config")).unwrap();
        Self { root }
    }
    fn config(&self) -> std::path::PathBuf {
        self.root.path().join("config")
    }
    fn state(&self) -> std::path::PathBuf {
        self.root.path().join("state")
    }
    fn write_config(&self, text: &str) {
        std::fs::write(self.config().join("config.toml"), text).unwrap();
    }
    fn core(&self, socket: &Path) -> Command {
        let mut command = revive();
        command
            .arg("--host")
            .arg("core")
            .arg("--config-dir")
            .arg(self.config())
            .arg("--state-dir")
            .arg(self.state())
            .arg("--socket")
            .arg(socket);
        command
    }
    fn store(&self, socket: &Path) -> Store {
        let (_, session) = paths::session_identity(socket).unwrap();
        Store::new(&self.state(), session).unwrap()
    }
    fn seed(&self, socket: &Path, argv: &[&str]) {
        self.store(socket)
            .save(&self.snapshot(socket, argv), None, 10)
            .unwrap();
    }
    fn snapshot(&self, socket: &Path, argv: &[&str]) -> Snapshot {
        let (_, session) = paths::session_identity(socket).unwrap();
        Snapshot {
            tool: TOOL.into(),
            schema: SCHEMA,
            session,
            created_ms: 1,
            scope: Scope::Session,
            layout: vec![],
            focused_workspace_id: None,
            panes: vec![SavedPane {
                workspace_id: "s1".into(),
                tab_id: "s1:ttb1".into(),
                pane_id: "s1:pt7".into(),
                cwd: "/tmp".into(),
                command: Some(CommandSpec::Program {
                    argv: argv.iter().map(|s| (*s).into()).collect(),
                }),
            }],
        }
    }
}

#[test]
fn the_host_flag_is_required_outside_a_herdr_plugin_environment() {
    let (ok, _, stderr) = run(revive().arg("list"));
    assert!(!ok);
    assert!(stderr.contains("--host herdr|core is required"), "{stderr}");
    let (ok, _, stderr) = run(revive().env("HERDR_REVIVE_HOST", "both").arg("list"));
    assert!(!ok);
    assert!(
        stderr.contains("HERDR_REVIVE_HOST must be herdr or core"),
        "{stderr}"
    );
    let (ok, _, stderr) = run(revive().env("HERDR_PLUGIN_STATE_DIR", "/tmp/x").arg("list"));
    assert!(!ok);
    assert!(
        stderr.contains("HERDR_PLUGIN_CONFIG_DIR is required"),
        "{stderr}"
    );
}

#[test]
fn named_spaces_and_recreate_are_refused_on_core() {
    let dirs = Dirs::new();
    for args in [vec!["space", "list"], vec!["restore", "--recreate"]] {
        let (ok, _, stderr) = run(revive()
            .env("HERDR_REVIVE_HOST", "core")
            .arg("--config-dir")
            .arg(dirs.config())
            .args(&args));
        assert!(!ok, "{args:?}");
        assert!(
            stderr.contains("not supported by the herdr-core host"),
            "{stderr}"
        );
    }
}

#[test]
fn core_commands_use_explicit_dirs_and_the_fake_socket() {
    let core = FakeCore::start(vec![record("t7", "s1", "tb1", 4242, "/tmp")]);
    let dirs = Dirs::new();
    let (ok, value, stderr) = run(dirs.core(&core.socket).arg("list"));
    assert!(ok, "{stderr}");
    assert_eq!(value["result"]["snapshots"], serde_json::json!([]));
    assert_eq!(value["requests"], 1);
    assert!(dirs.state().join("spaces").is_dir());

    dirs.write_config("allowed_programs = [\"sleep\"]\n");
    dirs.seed(&core.socket, &["sleep", "600"]);
    let (ok, value, stderr) = run(dirs.core(&core.socket).arg("preview"));
    assert!(ok, "{stderr}");
    assert_eq!(
        value["result"]["plan"]["entries"][0]["decision"],
        "candidate"
    );
    assert_eq!(value["result"]["plan"]["entries"][0]["terminal_id"], "t7");
    assert!(value["result"]["reconstruction"].is_null());
}

#[test]
fn event_on_core_never_restores_and_saves_only_with_auto_save() {
    for auto_save in [false, true] {
        let shell = Shell::start();
        let core = FakeCore::start(vec![record("t7", "s1", "tb1", shell.pid, "/tmp")]);
        let dirs = Dirs::new();
        dirs.write_config(&format!(
            "auto_save = {auto_save}\nauto_restore = true\nallowed_programs = [\"sleep\"]\n"
        ));
        dirs.seed(&core.socket, &["sleep", "600"]);
        let store = dirs.store(&core.socket);
        let before = std::fs::read(store.snapshot_path(None).unwrap()).unwrap();
        let (ok, value, stderr) = run(dirs.core(&core.socket).arg("event"));
        assert!(ok, "{stderr}");
        let after = std::fs::read(store.snapshot_path(None).unwrap()).unwrap();
        if auto_save {
            assert_eq!(value["result"]["status"], "saved", "{value}");
            assert_eq!(value["result"]["panes"], 1, "{value}");
            assert_ne!(before, after);
            assert_eq!(store.list("snapshots").unwrap().len(), 2);
        } else {
            assert_eq!(value["result"]["status"], "boot_done");
            assert_eq!(before, after);
            assert_eq!(store.list("snapshots").unwrap().len(), 1);
        }
        assert!(store.list("boots").unwrap().is_empty());
        assert!(core.sent.lock().unwrap().is_empty());
        assert!(
            !core
                .requests
                .lock()
                .unwrap()
                .iter()
                .any(|r| r["method"] == "terminal.send")
        );
    }
}

#[test]
fn the_host_flag_wins_over_the_environment() {
    let core = FakeCore::start(vec![record("t7", "s1", "tb1", 4242, "/tmp")]);
    let dirs = Dirs::new();
    let mut command = revive();
    command
        .env("HERDR_REVIVE_HOST", "core")
        .arg("--host")
        .arg("herdr")
        .arg("--config-dir")
        .arg(dirs.config())
        .arg("--state-dir")
        .arg(dirs.state())
        .arg("--socket")
        .arg(&core.socket)
        .arg("list");
    let (ok, value, stderr) = run(&mut command);
    assert!(ok, "{stderr}");
    assert_eq!(value["requests"], 0);
    assert!(core.requests.lock().unwrap().is_empty());
    let (_, herdr_session) = herdr_revive::store::session_identity(&core.socket).unwrap();
    assert!(dirs.state().join(herdr_session).is_dir());

    let (ok, value, stderr) = run(revive()
        .env("HERDR_REVIVE_HOST", "core")
        .arg("--config-dir")
        .arg(dirs.config())
        .arg("--state-dir")
        .arg(dirs.state())
        .arg("--socket")
        .arg(&core.socket)
        .arg("list"));
    assert!(ok, "{stderr}");
    assert_eq!(value["requests"], 1);
    assert_eq!(core.requests.lock().unwrap()[0]["method"], "hello");
}

#[test]
fn the_environment_overrides_the_plugin_default() {
    let core = FakeCore::start(vec![record("t7", "s1", "tb1", 4242, "/tmp")]);
    let dirs = Dirs::new();
    let plugin_state = dirs.root.path().join("plugin-state");
    let (ok, value, stderr) = run(revive()
        .env("HERDR_PLUGIN_STATE_DIR", &plugin_state)
        .env("HERDR_REVIVE_HOST", "core")
        .arg("--config-dir")
        .arg(dirs.config())
        .arg("--state-dir")
        .arg(dirs.state())
        .arg("--socket")
        .arg(&core.socket)
        .arg("list"));
    assert!(ok, "{stderr}");
    assert_eq!(value["requests"], 1);
    assert_eq!(core.requests.lock().unwrap()[0]["method"], "hello");
    assert!(dirs.store(&core.socket).root.is_dir());
    assert!(!plugin_state.exists());
}

#[test]
fn install_refuses_the_herdr_host() {
    let explicit = revive()
        .arg("--host")
        .arg("herdr")
        .arg("install")
        .output()
        .unwrap();
    let implied = revive()
        .env("HERDR_PLUGIN_STATE_DIR", "/tmp/x")
        .arg("install")
        .output()
        .unwrap();
    for output in [explicit, implied] {
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(!output.status.success());
        assert!(
            stderr.contains("install is only for the herdr-core host"),
            "{stderr}"
        );
    }
}

#[test]
fn restore_on_core_refuses_a_layout_rebuild_before_any_send() {
    let core = FakeCore::start(vec![record("t7", "s1", "tb1", 4242, "/tmp")]);
    let dirs = Dirs::new();
    dirs.write_config("allowed_programs = [\"sleep\"]\n");
    let mut snapshot = dirs.snapshot(&core.socket, &["sleep", "600"]);
    snapshot.panes.push(SavedPane {
        workspace_id: "s2".into(),
        tab_id: "s2:t1".into(),
        pane_id: "s2:p9".into(),
        cwd: "/tmp".into(),
        command: None,
    });
    let tab = |workspace: &str, tab: &str, pane: &str| WorkspaceLayout {
        workspace_id: workspace.into(),
        label: workspace.into(),
        active_tab_id: Some(tab.into()),
        tabs: vec![TabLayout {
            tab_id: tab.into(),
            label: "tab".into(),
            focused_pane_id: pane.into(),
            root: Node::Pane {
                pane_id: pane.into(),
                label: None,
            },
        }],
    };
    snapshot.layout = vec![tab("s1", "s1:ttb1", "s1:pt7"), tab("s2", "s2:t1", "s2:p9")];
    dirs.store(&core.socket).save(&snapshot, None, 10).unwrap();

    let (ok, value, stderr) = run(dirs.core(&core.socket).arg("preview"));
    assert!(ok, "{stderr}");
    assert!(!value["result"]["reconstruction"].is_null(), "{value}");

    let (ok, _, stderr) = run(dirs.core(&core.socket).arg("restore"));
    assert!(!ok);
    assert!(
        stderr.contains("not supported by the herdr-core host"),
        "{stderr}"
    );
    assert!(core.sent.lock().unwrap().is_empty());
    assert!(dirs.store(&core.socket).list("boots").unwrap().is_empty());
}

fn init_outcome(output: std::process::Output) -> (bool, String, String) {
    (
        output.status.success(),
        String::from_utf8(output.stdout).unwrap(),
        String::from_utf8(output.stderr).unwrap(),
    )
}

#[test]
fn init_prints_nothing_without_a_terminal_id_and_explains_only_under_debug() {
    let core = FakeCore::start(vec![record("t7", "s1", "tb1", 4242, "/tmp")]);
    let dirs = Dirs::new();
    dirs.write_config("auto_save = true\nallowed_programs = [\"ssh\"]\n");
    for debug in [None, Some("0"), Some("yes")] {
        let mut command = dirs.core(&core.socket);
        command.arg("init").arg("bash");
        if let Some(value) = debug {
            command.env("REVIVE_DEBUG", value);
        }
        let (ok, stdout, stderr) = init_outcome(command.output().unwrap());
        assert!(ok, "{debug:?}");
        assert_eq!(stdout, "", "{debug:?}");
        assert_eq!(stderr, "", "{debug:?}");
    }
    let (ok, stdout, stderr) = init_outcome(
        dirs.core(&core.socket)
            .env("REVIVE_DEBUG", "1")
            .arg("init")
            .arg("bash")
            .output()
            .unwrap(),
    );
    assert!(ok);
    assert_eq!(stdout, "");
    assert!(stderr.contains("HC_TERMINAL_ID is not set"), "{stderr}");
}

#[test]
fn init_prints_nothing_when_another_process_owns_the_terminal() {
    let core = FakeCore::start(vec![record("t7", "s1", "tb1", 4242, "/tmp")]);
    let dirs = Dirs::new();
    dirs.write_config("auto_save = true\nallowed_programs = [\"ssh\"]\n");
    for debug in ["", "1"] {
        let (ok, stdout, stderr) = init_outcome(
            dirs.core(&core.socket)
                .env("HC_TERMINAL_ID", "t7")
                .env("REVIVE_DEBUG", debug)
                .arg("init")
                .arg("bash")
                .output()
                .unwrap(),
        );
        assert!(ok);
        assert_eq!(stdout, "");
        if debug == "1" {
            assert!(
                stderr.contains("init was not started by the pane's shell"),
                "{stderr}"
            );
        } else {
            assert_eq!(stderr, "");
        }
    }
}

/// A child process killed and reaped when the test is done with it, also
/// when an assertion panics first.
struct Keeper(std::process::Child);

impl Drop for Keeper {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

const INIT_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// The script both init helpers run with the `init bash` argv as `"$@"`: it
/// waits for a line on stdin, so the caller can register its pid as terminal
/// `t7`'s shell first, then runs init the way the rc line does,
/// `$(exec herdr-revive ...)`, and records init's text and exit status.
/// `REVIVE_PRELUDE` stands in for the rest of an rc file evaluated first.
const INIT_SCRIPT: &str = r#"read -r _
eval "${REVIVE_PRELUDE-}"
out=$(exec "$@" 2>"$REVIVE_ERR")
status=$?
printf '%s\n__status=%s\n' "$out" "$status" >"$REVIVE_OUT"
"#;

/// Runs `keeper` with only `env` and the fixture's variables; it prints the
/// pid of the bash running `INIT_SCRIPT` on its first stdout line. Every wait
/// is bounded by `INIT_WAIT`. Returns whether init exited 0, init's stdout and
/// init's stderr.
fn run_init(
    dirs: &Dirs,
    core: &FakeCore,
    mut keeper: Command,
    env: &[(&str, &str)],
) -> (bool, String, String) {
    use std::io::{BufRead, Write};
    let out = dirs.root.path().join("init.out");
    let err = dirs.root.path().join("init.err");
    let _ = std::fs::remove_file(&out);
    let script = dirs.root.path().join("init.sh");
    std::fs::write(&script, INIT_SCRIPT).unwrap();
    keeper
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", "/tmp")
        .env("HC_TERMINAL_ID", "t7")
        .env("REVIVE_OUT", &out)
        .env("REVIVE_ERR", &err)
        .envs(env.iter().copied())
        .arg(&script)
        .arg(env!("CARGO_BIN_EXE_herdr-revive"))
        .args(["--host", "core", "--config-dir"])
        .arg(dirs.config())
        .arg("--state-dir")
        .arg(dirs.state())
        .arg("--socket")
        .arg(&core.socket)
        .args(["init", "bash"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped());
    let mut keeper = Keeper(keeper.spawn().unwrap());
    let stdout = keeper.0.stdout.take().unwrap();
    let (lines, received) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in std::io::BufReader::new(stdout).lines() {
            if lines.send(line).is_err() {
                break;
            }
        }
    });
    let pid: u32 = received
        .recv_timeout(INIT_WAIT)
        .expect("init keeper printed no pid")
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    *core.records().lock().unwrap() = vec![record("t7", "s1", "tb1", pid, "/tmp")];
    writeln!(keeper.0.stdin.take().unwrap()).unwrap();
    let deadline = std::time::Instant::now() + INIT_WAIT;
    let status = loop {
        if let Some(status) = keeper.0.try_wait().unwrap() {
            break status;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "init keeper did not exit within {INIT_WAIT:?}"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    };
    assert!(status.success(), "init keeper failed: {status:?}");
    let recorded = std::fs::read_to_string(&out).unwrap();
    let (text, init_status) = recorded
        .strip_suffix('\n')
        .and_then(|r| r.rsplit_once("\n__status="))
        .unwrap_or_else(|| panic!("no init status recorded: {recorded:?}"));
    (
        init_status == "0",
        text.to_string(),
        std::fs::read_to_string(&err).unwrap(),
    )
}

/// `init bash` from a bash with no terminal, so no relaunch can pass the
/// foreground checks.
fn init_from_bash(dirs: &Dirs, core: &FakeCore, env: &[(&str, &str)]) -> (bool, String, String) {
    let mut keeper = Command::new("bash");
    keeper.args(["-c", r#"echo $$; . "$0""#]);
    run_init(dirs, core, keeper, env)
}

/// `init bash` from a bash that leads a pty session, as a pane's shell does,
/// so a candidate relaunch passes the foreground checks. The bash runs the
/// script as a file: init refuses a shell started with `-c`.
fn init_on_pty(dirs: &Dirs, core: &FakeCore, env: &[(&str, &str)]) -> (bool, String, String) {
    let mut keeper = Command::new("python3");
    keeper.args([
        "-I",
        "-c",
        r#"import os, pty, sys
pid, fd = pty.fork()
if pid == 0:
    os.execvp("bash", ["bash", *sys.argv[1:]])
print(pid, flush=True)
sys.stdin.readline()
os.write(fd, b"\n")
while True:
    try:
        if not os.read(fd, 65536):
            break
    except OSError:
        break
sys.exit(os.waitstatus_to_exitcode(os.waitpid(pid, 0)[1]))
"#,
    ]);
    let mut env = env.to_vec();
    env.push(("REVIVE_DEBUG", "1"));
    run_init(dirs, core, keeper, &env)
}

#[test]
fn init_prints_the_hook_for_the_panes_own_shell() {
    let core = FakeCore::start(vec![]);
    let dirs = Dirs::new();
    dirs.write_config("auto_save = true\nallowed_programs = [\"ssh\"]\n");
    let (ok, stdout, stderr) = init_from_bash(&dirs, &core, &[("REVIVE_DEBUG", "1")]);
    assert!(ok, "{stderr}");
    assert_eq!(stderr, "");
    assert!(stdout.contains("__revive_preexec"), "{stdout}");
    assert!(
        !stdout.contains("__revive_relaunch() {"),
        "hook-only output: {stdout}"
    );
    let syntax = Command::new("bash")
        .arg("-nc")
        .arg(&stdout)
        .output()
        .unwrap();
    assert!(syntax.status.success(), "{syntax:?}");
    let shell = Command::new("bash")
        .env_clear()
        .arg("-c")
        .arg(
            r#"ssh() { :; }
PROMPT_COMMAND='echo pc;'
eval "$1"
eval "$1"
__revive_save() { echo "save $1"; }
ssh host
eval "$PROMPT_COMMAND"
printf '%s|' "$PROMPT_COMMAND""#,
        )
        .arg("_")
        .arg(&stdout)
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&shell.stderr),
        "",
        "{:?}",
        shell.status
    );
    assert_eq!(
        String::from_utf8(shell.stdout).unwrap(),
        "save 0.5\npc\nsave 0.5\necho pc;\n__revive_precmd|"
    );

    dirs.write_config("auto_save = false\nallowed_programs = [\"ssh\"]\n");
    let (ok, stdout, stderr) = init_from_bash(&dirs, &core, &[("REVIVE_DEBUG", "1")]);
    assert!(ok, "{stderr}");
    assert_eq!((stdout.as_str(), stderr.as_str()), ("", ""));
}

#[test]
fn init_does_not_wait_for_the_lock_when_the_pane_has_nothing_to_relaunch() {
    let core = FakeCore::start(vec![]);
    let dirs = Dirs::new();
    dirs.write_config(
        "auto_save = true\nauto_restore = true\ntimeout_ms = 3000\nsettle_ms = 0\nallowed_programs = [\"sleep\"]\n",
    );
    let held = dirs.store(&core.socket).try_lock().unwrap().unwrap();
    let started = std::time::Instant::now();
    let (ok, stdout, stderr) = init_from_bash(&dirs, &core, &[("REVIVE_DEBUG", "1")]);
    let took = started.elapsed();
    drop(held);
    assert!(ok, "{stderr}");
    assert!(stdout.contains("__revive_preexec"), "{stdout}");
    assert!(!stdout.contains("relaunch skipped"), "{stdout}");
    assert!(took < std::time::Duration::from_millis(2500), "{took:?}");
}

#[test]
fn init_keeps_the_hook_and_names_the_skip_when_the_session_lock_is_held() {
    let core = FakeCore::start(vec![]);
    let dirs = Dirs::new();
    dirs.write_config(
        "auto_save = true\nauto_restore = true\ntimeout_ms = 10\nsettle_ms = 0\nallowed_programs = [\"sleep\"]\n",
    );
    dirs.seed(&core.socket, &["sleep", "600"]);
    let held = dirs.store(&core.socket).try_lock().unwrap().unwrap();
    let (ok, stdout, stderr) = init_from_bash(&dirs, &core, &[("REVIVE_DEBUG", "1")]);
    drop(held);
    assert!(ok, "{stderr}");
    let (hook, notice) = stdout.rsplit_once('\n').unwrap();
    assert!(
        hook.contains("__revive_preexec") && hook.ends_with("\nfi"),
        "{stdout}"
    );
    assert!(!stdout.contains("__revive_relaunch() {"), "{stdout}");
    assert!(notice.starts_with("printf '\\033[2m"), "{notice}");
    assert!(
        notice.contains("automatic relaunch skipped: another operation held the session lock")
            && notice.contains("herdr-revive --host core restore --rehydrate"),
        "{notice}"
    );
    assert!(
        stderr.contains("another operation holds the session lock"),
        "{stderr}"
    );
    let shell = Command::new("bash")
        .arg("-c")
        .arg(&stdout)
        .output()
        .unwrap();
    assert!(shell.status.success(), "{shell:?}");
    assert!(
        String::from_utf8_lossy(&shell.stderr).contains("restore --rehydrate"),
        "{shell:?}"
    );
    assert!(core.sent.lock().unwrap().is_empty());
    assert!(dirs.store(&core.socket).list("boots").unwrap().is_empty());
}

#[test]
fn init_notes_a_skipped_relaunch_only_when_the_pane_has_a_candidate() {
    let core = FakeCore::start(vec![]);
    let dirs = Dirs::new();
    dirs.write_config("auto_restore = true\nallowed_programs = [\"sleep\"]\n");
    let skip = [("REVIVE_AUTO", "0"), ("REVIVE_DEBUG", "1")];
    let (ok, stdout, stderr) = init_from_bash(&dirs, &core, &skip);
    assert!(ok, "{stderr}");
    assert_eq!((stdout.as_str(), stderr.as_str()), ("", ""));

    dirs.seed(&core.socket, &["sleep", "600"]);
    let (ok, stdout, stderr) = init_from_bash(&dirs, &core, &skip);
    assert!(ok, "{stderr}");
    assert_eq!(stderr, "");
    assert_eq!(stdout.lines().count(), 1, "{stdout}");
    assert!(
        stdout.contains("automatic relaunch skipped (REVIVE_AUTO=0)"),
        "{stdout}"
    );

    dirs.write_config("auto_restore = true\nallowed_programs = [\"ssh\"]\n");
    let (ok, stdout, stderr) = init_from_bash(&dirs, &core, &skip);
    assert!(ok, "{stderr}");
    assert_eq!((stdout.as_str(), stderr.as_str()), ("", ""));
    assert!(core.sent.lock().unwrap().is_empty());
    assert!(dirs.store(&core.socket).list("boots").unwrap().is_empty());
}

#[test]
fn init_defers_the_relaunch_command_to_the_first_prompt() {
    let core = FakeCore::start(vec![]);
    let dirs = Dirs::new();
    dirs.write_config(
        "auto_save = true\nauto_restore = true\nsettle_ms = 0\nallowed_programs = [\"sleep\"]\n",
    );
    dirs.seed(&core.socket, &["sleep", "600"]);
    let (ok, stdout, stderr) = init_on_pty(&dirs, &core, &[]);
    assert!(ok, "{stderr}");
    assert_eq!(stderr, "");
    let (relaunch, hook) = stdout.split_once("__revive_bin=").unwrap();
    assert!(
        hook.contains("__revive_preexec") && hook.ends_with("\nfi"),
        "{stdout}"
    );
    assert!(
        relaunch.starts_with(
            "__revive_relaunch() {\n  __revive_relaunch() { return; }\n  unset __revive_relaunch_cmd\n"
        ),
        "{stdout}"
    );
    assert!(
        relaunch.contains("\n cd -- '/tmp' && command 'sleep' '600'\n  __revive_rc=$?\n"),
        "{stdout}"
    );
    assert!(
        relaunch.ends_with(concat!(
            "  *) PROMPT_COMMAND=\"__revive_relaunch${PROMPT_COMMAND:+\n$PROMPT_COMMAND}\" ;;\nesac\n",
            r#"__revive_relaunch_cmd=' cd -- '\''/tmp'\'' && command '\''sleep'\'' '\''600'\'''"#,
            "\n"
        )),
        "{stdout}"
    );
    assert!(!stdout.contains("set -m"), "{stdout}");
    let order = Command::new("bash")
        .arg("-c")
        .arg("PROMPT_COMMAND='echo pc'\neval \"$1\"\nprintf '%s|' \"$PROMPT_COMMAND\"")
        .arg("_")
        .arg(&stdout)
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&order.stdout),
        "__revive_relaunch\necho pc\n__revive_precmd|",
        "{order:?}"
    );
    let syntax = Command::new("bash")
        .arg("-nc")
        .arg(&stdout)
        .output()
        .unwrap();
    assert!(syntax.status.success(), "{syntax:?}");
    assert!(core.sent.lock().unwrap().is_empty());
    assert_eq!(dirs.store(&core.socket).list("boots").unwrap().len(), 1);
}

#[test]
fn init_relaunches_past_a_background_job_the_rc_file_started() {
    let core = FakeCore::start(vec![]);
    let dirs = Dirs::new();
    dirs.write_config("auto_restore = true\nsettle_ms = 0\nallowed_programs = [\"sleep\"]\n");
    dirs.seed(&core.socket, &["sleep", "600"]);
    let prelude = [(
        "REVIVE_PRELUDE",
        "sleep 3 </dev/null >/dev/null 2>&1 &\n\
         [ \"$(ps -o pgid= -p $!)\" -eq $$ ] || echo 'job left the group' >&2",
    )];
    let (ok, stdout, stderr) = init_on_pty(&dirs, &core, &prelude);
    assert!(ok, "{stderr}");
    assert_eq!(stderr, "");
    assert!(
        stdout.contains("\n cd -- '/tmp' && command 'sleep' '600'\n  __revive_rc=$?\n"),
        "{stdout}"
    );
    let store = dirs.store(&core.socket);
    let boots = store.list("boots").unwrap();
    assert_eq!(boots.len(), 1);
    let journal: Value = serde_json::from_slice(&std::fs::read(&boots[0]).unwrap()).unwrap();
    assert_eq!(journal["entries"][0]["outcome"], "applied", "{journal}");
}

#[test]
fn init_journals_a_relaunch_skipped_when_core_stops_answering() {
    let core = FakeCore::start(vec![]);
    let dirs = Dirs::new();
    dirs.write_config(
        "auto_restore = true\ntimeout_ms = 300\nsettle_ms = 0\nallowed_programs = [\"sleep\"]\n",
    );
    dirs.seed(&core.socket, &["sleep", "600"]);
    *core.answered_gets.lock().unwrap() = Some(1);
    let (ok, stdout, stderr) = init_from_bash(&dirs, &core, &[("REVIVE_DEBUG", "1")]);
    assert!(ok, "{stderr}");
    assert_eq!(stdout, "");
    assert!(
        stderr.contains("relaunch skipped: terminal.get failed"),
        "{stderr}"
    );
    let store = dirs.store(&core.socket);
    assert!(!store.root.join("pending.json").exists());
    let boots = store.list("boots").unwrap();
    assert_eq!(boots.len(), 1);
    let journal: Value = serde_json::from_slice(&std::fs::read(&boots[0]).unwrap()).unwrap();
    assert_eq!(journal["state"], "done", "{journal}");
    assert_eq!(journal["entries"][0]["outcome"], "skipped", "{journal}");
    assert!(core.sent.lock().unwrap().is_empty());
}

#[test]
fn init_names_pending_recovery_evidence_and_keeps_the_hook() {
    let core = FakeCore::start(vec![]);
    let dirs = Dirs::new();
    dirs.write_config("auto_save = true\nauto_restore = true\nallowed_programs = [\"sleep\"]\n");
    dirs.seed(&core.socket, &["sleep", "600"]);
    let store = dirs.store(&core.socket);
    let mut journal = herdr_revive::app::Journal::new(&store.session, &"d".repeat(64));
    journal.state = herdr_revive::app::BootState::Failed;
    herdr_revive::store::atomic_json(&store.root.join("pending.json"), &journal).unwrap();
    let (ok, stdout, stderr) = init_from_bash(&dirs, &core, &[("REVIVE_DEBUG", "1")]);
    assert!(ok, "{stderr}");
    assert!(stdout.contains("__revive_preexec"), "{stdout}");
    let notice = stdout.lines().last().unwrap();
    assert!(notice.starts_with("printf '\\033[2m"), "{notice}");
    assert!(
        notice.contains("recovery evidence is pending")
            && notice.contains("herdr-revive --host core recovery acknowledge"),
        "{notice}"
    );
    assert!(
        stderr.contains("restore evidence requires inspection"),
        "{stderr}"
    );
    let shell = Command::new("bash")
        .arg("-c")
        .arg(&stdout)
        .output()
        .unwrap();
    assert!(shell.status.success(), "{shell:?}");
    assert!(
        String::from_utf8_lossy(&shell.stderr)
            .contains("herdr-revive --host core recovery acknowledge"),
        "{shell:?}"
    );
    assert!(core.sent.lock().unwrap().is_empty());
    assert!(store.list("boots").unwrap().is_empty());
}

#[test]
fn a_hook_save_from_a_replaced_shell_writes_nothing() {
    let shell = Shell::start();
    let core = FakeCore::start(vec![record("t7", "s1", "tb1", shell.pid, "/tmp")]);
    let dirs = Dirs::new();
    // A shell has the hook only while auto_save is on.
    dirs.write_config("auto_save = true\n");
    let store = dirs.store(&core.socket);
    let hook = |terminal: &str, pid: u32| {
        run(dirs
            .core(&core.socket)
            .env("HC_TERMINAL_ID", terminal)
            .env("REVIVE_SHELL_PID", pid.to_string())
            .args(["autosave", "--force"]))
    };
    for (terminal, pid) in [("t7", shell.pid + 1), ("t9", shell.pid)] {
        let (ok, value, stderr) = hook(terminal, pid);
        assert!(ok, "{stderr}");
        assert_eq!(
            value,
            serde_json::json!({"status": "skipped", "reason": "shell replaced"})
        );
        assert!(store.list("snapshots").unwrap().is_empty());
        assert!(!store.root.join("last-save.json").exists());
    }
    core.records().lock().unwrap()[0]["state"] = "finished".into();
    let (ok, value, _) = hook("t7", shell.pid);
    assert!(ok);
    assert_eq!(value["status"], "skipped", "{value}");
    core.records().lock().unwrap()[0]["state"] = "live".into();

    let (ok, value, stderr) = hook("t7", shell.pid);
    assert!(ok, "{stderr}");
    assert_eq!(value["result"]["status"], "saved", "{value}");
    assert_eq!(store.list("snapshots").unwrap().len(), 1);
    // Nothing changed: the next hook save archives nothing new.
    let (ok, _, stderr) = hook("t7", shell.pid);
    assert!(ok, "{stderr}");
    assert_eq!(store.list("snapshots").unwrap().len(), 1);

    let (ok, value, stderr) = run(dirs
        .core(&core.socket)
        .env("HC_TERMINAL_ID", "t9")
        .args(["autosave", "--force"]));
    assert!(ok, "{stderr}");
    assert_eq!(value["result"]["status"], "saved", "manual save: {value}");
}

#[test]
fn a_hook_save_honours_auto_save_off_and_a_manual_save_does_not_need_it() {
    let shell = Shell::start();
    let core = FakeCore::start(vec![record("t7", "s1", "tb1", shell.pid, "/tmp")]);
    let dirs = Dirs::new();
    dirs.write_config("auto_save = false\n");
    let store = dirs.store(&core.socket);
    let (ok, value, stderr) = run(dirs
        .core(&core.socket)
        .env("HC_TERMINAL_ID", "t7")
        .env("REVIVE_SHELL_PID", shell.pid.to_string())
        .args(["autosave", "--force"]));
    assert!(ok, "{stderr}");
    assert_eq!(value, serde_json::json!({"status": "disabled"}));
    assert!(store.list("snapshots").unwrap().is_empty());
    let (ok, value, stderr) = run(dirs
        .core(&core.socket)
        .env("HC_TERMINAL_ID", "t7")
        .args(["autosave", "--force"]));
    assert!(ok, "{stderr}");
    assert_eq!(value["result"]["status"], "saved", "{value}");
}

#[test]
fn a_core_hook_save_waits_for_the_session_lock() {
    let shell = Shell::start();
    let core = FakeCore::start(vec![record("t7", "s1", "tb1", shell.pid, "/tmp")]);
    let dirs = Dirs::new();
    let store = dirs.store(&core.socket);
    let held = store.try_lock().unwrap().unwrap();
    let release = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs(1));
        drop(held);
    });
    let started = std::time::Instant::now();
    let (ok, value, stderr) = run(dirs.core(&core.socket).args(["autosave", "--force"]));
    release.join().unwrap();
    assert!(ok, "{stderr}");
    assert_eq!(value["result"]["status"], "saved", "{value}");
    assert!(started.elapsed() >= std::time::Duration::from_millis(900));
    assert_eq!(store.list("snapshots").unwrap().len(), 1);
}

#[test]
fn install_prints_the_rc_line_and_writes_only_on_request() {
    let home = tempfile::tempdir_in("/tmp").unwrap();
    let (ok, value, stderr) = run(revive()
        .env("HOME", home.path())
        .args(["--host", "core", "install"]));
    assert!(ok, "{stderr}");
    assert_eq!(value["status"], "dry_run");
    assert!(value["rc_line"].as_str().unwrap().contains("init bash"));
    assert!(std::fs::read_dir(home.path()).unwrap().next().is_none());
    let (ok, value, _) = run(revive()
        .env("HOME", home.path())
        .args(["--host", "core", "install", "--write"]));
    assert!(ok);
    assert_eq!(value["status"], "written");
    let rc = std::path::PathBuf::from(value["target"].as_str().unwrap());
    assert!(
        std::fs::read_to_string(rc)
            .unwrap()
            .contains("# >>> herdr-revive >>>")
    );
    let (ok, _, stderr) = run(revive().args(["--host", "herdr", "install"]));
    assert!(!ok);
    assert!(stderr.contains("install is only for the herdr-core host"));
}

#[test]
fn install_refuses_an_rc_file_with_an_unmatched_marker() {
    let home = tempfile::tempdir_in("/tmp").unwrap();
    let (ok, value, stderr) = run(revive()
        .env("HOME", home.path())
        .args(["--host", "core", "install"]));
    assert!(ok, "{stderr}");
    let rc = std::path::PathBuf::from(value["target"].as_str().unwrap());
    let text = "export A=1\n# >>> herdr-revive >>>\nexport KEEP=1\n";
    std::fs::write(&rc, text).unwrap();
    for args in [
        &["install"][..],
        &["install", "--write"],
        &["install", "--remove"],
    ] {
        let (ok, value, stderr) = run(revive()
            .env("HOME", home.path())
            .args(["--host", "core"])
            .args(args));
        assert!(!ok, "{args:?}");
        assert!(value.is_null());
        assert!(
            stderr.contains("rc file has an unmatched herdr-revive marker at line 2"),
            "{stderr}"
        );
        assert_eq!(std::fs::read_to_string(&rc).unwrap(), text);
    }
    assert_eq!(std::fs::read_dir(home.path()).unwrap().count(), 1);
}

#[test]
fn status_composes_snapshot_plan_recovery_and_setup() {
    let core = FakeCore::start(vec![record("t7", "s1", "tb1", 4242, "/tmp")]);
    let dirs = Dirs::new();
    let home = tempfile::tempdir_in("/tmp").unwrap();
    let (ok, value, stderr) = run(dirs
        .core(&core.socket)
        .env("HOME", home.path())
        .arg("status"));
    assert!(ok, "{stderr}");
    let result = &value["result"];
    assert_eq!(result["host"], "core");
    assert_eq!(result["session"].as_str().unwrap().len(), 64);
    assert!(result["latest"].is_null());
    assert!(result["panes"].is_null());
    assert_eq!(result["snapshots"], serde_json::json!([]));
    assert!(result["recovery"]["pending"].is_null());
    assert_eq!(result["config"]["present"], false);
    assert_eq!(result["config"]["auto_save"], false);
    assert_eq!(result["install"]["rc_installed"], false);
    assert_eq!(result["install"]["rc_up_to_date"], false);
    assert!(result["install"]["rc_error"].is_null());
    assert!(
        result["install"]["rc_file"]
            .as_str()
            .unwrap()
            .starts_with(home.path().to_str().unwrap())
    );
    assert!(
        result["install"]["binary"]
            .as_str()
            .unwrap()
            .ends_with("herdr-revive")
    );

    dirs.write_config("allowed_programs = [\"sleep\"]\nauto_restore = true\n");
    dirs.seed(&core.socket, &["sleep", "600"]);
    std::fs::write(
        home.path().join(".bashrc"),
        herdr_revive::core::install::block(),
    )
    .unwrap();
    let (ok, value, stderr) = run(dirs
        .core(&core.socket)
        .env("HOME", home.path())
        .arg("status"));
    assert!(ok, "{stderr}");
    let result = &value["result"];
    assert_eq!(result["latest"]["panes"], 1);
    assert_eq!(result["latest"]["commands"], 1);
    assert_eq!(result["panes"][0]["decision"], "candidate");
    assert_eq!(result["panes"][0]["program"], "sleep");
    assert_eq!(result["panes"][0]["terminal_id"], "t7");
    assert_eq!(result["panes"][0]["cwd"], "/tmp");
    assert_eq!(result["config"]["present"], true);
    assert_eq!(result["config"]["auto_restore"], true);
    if cfg!(target_os = "linux") {
        assert_eq!(result["install"]["rc_installed"], true);
        assert_eq!(result["install"]["rc_up_to_date"], true);
        assert!(result["install"]["rc_error"].is_null());
    }
    assert_eq!(result["snapshots"].as_array().unwrap().len(), 1);
}

#[test]
fn status_reports_rc_file_problems_without_failing() {
    if !cfg!(target_os = "linux") {
        return;
    }
    let core = FakeCore::start(vec![record("t7", "s1", "tb1", 4242, "/tmp")]);
    let dirs = Dirs::new();
    let home = tempfile::tempdir_in("/tmp").unwrap();
    let rc = home.path().join(".bashrc");
    let status = |dirs: &Dirs| {
        let (ok, value, stderr) = run(dirs
            .core(&core.socket)
            .env("HOME", home.path())
            .arg("status"));
        assert!(ok, "{stderr}");
        value["result"]["install"].clone()
    };

    std::fs::write(&rc, "# >>> herdr-revive >>>\nalias x=y\n").unwrap();
    let install = status(&dirs);
    assert!(install["rc_installed"].is_null());
    assert!(install["rc_up_to_date"].is_null());
    assert!(install["rc_error"].as_str().unwrap().contains("unmatched"));

    std::fs::write(
        &rc,
        "# >>> herdr-revive >>>\nold line\n# <<< herdr-revive <<<\n",
    )
    .unwrap();
    let install = status(&dirs);
    assert_eq!(install["rc_installed"], true);
    assert_eq!(install["rc_up_to_date"], false);
    assert!(install["rc_error"].is_null());

    std::fs::remove_file(&rc).unwrap();
    std::fs::write(&rc, [0xff, 0xfe]).unwrap();
    let install = status(&dirs);
    assert!(install["rc_installed"].is_null());
    assert!(install["rc_error"].as_str().unwrap().contains("UTF-8"));
}

#[test]
fn without_core_only_a_shell_started_by_the_holder_gets_the_hook() {
    let dirs = Dirs::new();
    dirs.write_config(
        "auto_save = true\nauto_restore = true\ntimeout_ms = 300\nallowed_programs = [\"ssh\"]\n",
    );
    let init = |socket: &Path| {
        init_outcome(
            dirs.core(socket)
                .env("HC_TERMINAL_ID", "t7")
                .env("REVIVE_DEBUG", "1")
                .args(["init", "bash"])
                .output()
                .unwrap(),
        )
    };
    // The test runner is not core's holder: no hook, unreachable or not.
    let (ok, stdout, stderr) = init(&dirs.root.path().join("missing.sock"));
    assert!(ok, "{stderr}");
    assert_eq!(stdout, "", "only a pane's shell gets a hook");
    assert!(stderr.contains("cannot reach herdr-core"), "{stderr}");

    let core = FakeCore::start(vec![]);
    let (ok, stdout, stderr) = init(&core.socket);
    assert!(ok, "{stderr}");
    assert_eq!(stdout, "");
    assert!(stderr.contains("init: "), "{stderr}");

    *core.records().lock().unwrap() = vec![record("t7", "s1", "tb1", 4242, "/tmp")];
    let (ok, stdout, stderr) = init(&core.socket);
    assert!(ok, "{stderr}");
    assert_eq!(stdout, "", "a nested shell gets no hook");
    assert!(
        stderr.contains("init was not started by the pane's shell"),
        "{stderr}"
    );

    // A shell whose parent's argv is `herdr-core hold` is a pane's shell:
    // bash under a link named herdr-core, running a script named hold, is
    // that holder.
    let holder = dirs.root.path().join("holder");
    std::fs::create_dir_all(&holder).unwrap();
    std::os::unix::fs::symlink("/bin/bash", holder.join("herdr-core")).unwrap();
    let missing = dirs.root.path().join("missing.sock");
    std::fs::write(
        holder.join("hold"),
        format!(
            // As in a pane: the holder runs a shell, which evaluates init in
            // a command substitution, so init's parent is that shell.
            "HC_TERMINAL_ID=t7 bash -c 'out=$(exec \"$0\" --host core --config-dir \"$1\" --state-dir \"$2\" --socket \"$3\" init bash); printf %s \"$out\"' '{}' '{}' '{}' '{}'\n",
            env!("CARGO_BIN_EXE_herdr-revive"),
            dirs.config().display(),
            dirs.state().display(),
            missing.display()
        ),
    )
    .unwrap();
    let output = Command::new("./herdr-core")
        .arg("hold")
        .current_dir(&holder)
        .output()
        .unwrap();
    let (ok, stdout, stderr) = init_outcome(output);
    assert!(ok, "{stderr}");
    assert!(stdout.contains("__revive_preexec"), "{stdout}");
    assert!(!stdout.contains("__revive_relaunch() {"), "{stdout}");
}

#[test]
fn a_terminal_with_an_odd_label_does_not_stop_saves_or_status() {
    let shell = Shell::start();
    let core = FakeCore::start(vec![
        record("t1", "my-space", "tb1", 1, "/tmp"),
        record("t7", "s1", "tb1", shell.pid, "/tmp"),
    ]);
    let dirs = Dirs::new();
    let (ok, value, stderr) = run(dirs.core(&core.socket).arg("save"));
    assert!(ok, "{stderr}");
    assert_eq!(value["result"]["panes"], 1, "{value}");
    let (ok, value, stderr) = run(dirs.core(&core.socket).arg("status"));
    assert!(ok, "{stderr}");
    assert_eq!(value["result"]["panes"][0]["terminal_id"], "t7", "{value}");
}

type PaneCommand = (String, Option<Vec<String>>);

/// The panes a core recovery left: `a` already relaunched by its init, `b`
/// whose shell is still reading its rc file, `c` whose terminal is gone.
struct Recovery {
    a: Shell,
    _b: Shell,
    core: FakeCore,
    dirs: Dirs,
    generation: String,
}

/// `b`'s shell is still running its rc file: bash leads the pane's
/// foreground group, with a child in it.
const STARTING: &[&str] = &["bash", "-c", "sleep 30; :"];
/// `b` sits at an idle prompt: bash alone in its foreground group.
const IDLE: &[&str] = &["bash", "-c", "read -r _"];
/// `b`'s user started another shell from the prompt: it leads its own group.
const NESTED: &[&str] = &["bash", "-c", "set -m; bash -c 'sleep 30; :'; :"];

impl Recovery {
    fn new() -> Self {
        Self::with(Shell::start_with(STARTING))
    }

    fn with(b: Shell) -> Self {
        let a = Shell::start();
        let core = FakeCore::start(vec![
            record("ta", "s1", "tb1", a.pid, "/tmp"),
            record("tb", "s1", "tb1", b.pid, "/tmp"),
        ]);
        let dirs = Dirs::new();
        dirs.write_config(
            "auto_save = true\nauto_restore = true\nallowed_programs = [\"ssh\", \"sleep\"]\n",
        );
        let mut snapshot = dirs.snapshot(&core.socket, &["ssh", "x"]);
        snapshot.panes = ["a", "b", "c"]
            .iter()
            .map(|name| SavedPane {
                workspace_id: "s1".into(),
                tab_id: "s1:ttb1".into(),
                pane_id: format!("s1:pt{name}"),
                cwd: "/tmp".into(),
                command: Some(CommandSpec::Program {
                    argv: vec!["ssh".into(), (*name).into()],
                }),
            })
            .collect();
        dirs.store(&core.socket).save(&snapshot, None, 10).unwrap();
        let generation = herdr_revive::platform::generation(&core.socket).unwrap();
        let recovery = Self {
            a,
            _b: b,
            core,
            dirs,
            generation,
        };
        recovery.handled(&["s1:pta"]);
        recovery
    }

    fn handled(&self, panes: &[&str]) {
        let store = self.dirs.store(&self.core.socket);
        let mut journal = herdr_revive::app::Journal::new(&store.session, &self.generation);
        journal.state = herdr_revive::app::BootState::Done;
        journal.entries = panes
            .iter()
            .map(|pane| herdr_revive::app::JournalEntry {
                pane_id: (*pane).into(),
                outcome: herdr_revive::app::Outcome::Applied,
            })
            .collect();
        herdr_revive::app::finish(&store, &journal).unwrap();
    }

    /// The save `a`'s hook runs, and the latest snapshot's commands by pane.
    fn hook_save(&self) -> (Value, Vec<PaneCommand>) {
        let (ok, value, stderr) = run(self
            .dirs
            .core(&self.core.socket)
            .env("HC_TERMINAL_ID", "ta")
            .env("REVIVE_SHELL_PID", self.a.pid.to_string())
            .args(["autosave", "--force"]));
        assert!(ok, "{stderr}");
        assert_eq!(value["result"]["status"], "saved", "{value}");
        let store = self.dirs.store(&self.core.socket);
        let latest = store
            .read_snapshot(&store.snapshot_path(None).unwrap())
            .unwrap();
        let mut panes: Vec<_> = latest
            .panes
            .iter()
            .map(|pane| {
                (
                    pane.pane_id.clone(),
                    pane.command.as_ref().map(|c| c.argv().unwrap()),
                )
            })
            .collect();
        panes.sort();
        (value, panes)
    }
}

fn panes(list: &[(&str, &[&str])]) -> Vec<PaneCommand> {
    list.iter()
        .map(|(pane, argv)| {
            (
                (*pane).to_string(),
                (!argv.is_empty()).then(|| argv.iter().map(|s| (*s).to_string()).collect()),
            )
        })
        .collect()
}

#[test]
fn a_hook_save_during_recovery_keeps_the_commands_of_panes_not_yet_relaunched() {
    let recovery = Recovery::new();
    let expected = panes(&[("s1:pta", &["sleep", "30"]), ("s1:ptb", &["ssh", "b"])]);
    let (value, latest) = recovery.hook_save();
    assert_eq!(latest, expected, "{value}");
    assert_eq!(value["result"]["carried_panes"], 1, "{value}");
    let (value, latest) = recovery.hook_save();
    assert_eq!(latest, expected, "the next save carries it again: {value}");

    recovery.handled(&["s1:pta", "s1:ptb"]);
    let (value, latest) = recovery.hook_save();
    let live = panes(&[
        ("s1:pta", &["sleep", "30"]),
        ("s1:ptb", &["bash", "-c", "sleep 30; :"]),
    ]);
    assert_eq!(latest, live, "{value}");
    assert_eq!(value["result"]["carried_panes"], 0, "{value}");
    recovery.handled(&["s1:pta"]);
    let (_, latest) = recovery.hook_save();
    assert_eq!(latest, live, "a pane stops being carried for good");
}

#[test]
fn a_pane_not_live_yet_is_carried() {
    let recovery = Recovery::new();
    recovery.core.records().lock().unwrap()[1]["state"] = "starting".into();
    let (value, latest) = recovery.hook_save();
    assert_eq!(
        latest,
        panes(&[("s1:pta", &["sleep", "30"]), ("s1:ptb", &["ssh", "b"])]),
        "{value}"
    );
    assert_eq!(value["result"]["carried_panes"], 1, "{value}");
}

#[test]
fn an_idle_prompt_a_real_program_or_a_nested_shell_ends_the_carry() {
    let cases = [
        (Shell::start_with(IDLE), &[][..]),
        (Shell::start(), &["sleep", "30"][..]),
        (Shell::start_job(NESTED), &["bash", "-c", "sleep 30; :"][..]),
    ];
    for (b, captured) in cases {
        let recovery = Recovery::with(b);
        let expected = panes(&[("s1:pta", &["sleep", "30"]), ("s1:ptb", captured)]);
        let (value, latest) = recovery.hook_save();
        assert_eq!(latest, expected, "{value}");
        assert_eq!(value["result"]["carried_panes"], 0, "{value}");
        let (_, latest) = recovery.hook_save();
        assert_eq!(latest, expected, "{captured:?}");
    }
}

#[test]
fn a_pane_first_saved_in_this_generation_is_always_captured_live() {
    let recovery = Recovery::new();
    let store = recovery.dirs.store(&recovery.core.socket);
    herdr_revive::store::atomic_json(
        &store.root.join("last-save.json"),
        &serde_json::json!({"generation": recovery.generation, "time_ms": 1}),
    )
    .unwrap();
    let (value, latest) = recovery.hook_save();
    assert_eq!(
        latest,
        panes(&[
            ("s1:pta", &["sleep", "30"]),
            ("s1:ptb", &["bash", "-c", "sleep 30; :"])
        ]),
        "{value}"
    );
    assert_eq!(value["result"]["carried_panes"], 0, "{value}");
}

#[test]
fn only_automatic_core_saves_with_auto_restore_carry_panes() {
    let recovery = Recovery::new();
    let (ok, value, stderr) = run(recovery.dirs.core(&recovery.core.socket).arg("save"));
    assert!(ok, "{stderr}");
    assert_eq!(value["result"]["carried_panes"], 0, "manual save: {value}");

    let recovery = Recovery::new();
    recovery
        .dirs
        .write_config("auto_save = true\nallowed_programs = [\"ssh\", \"sleep\"]\n");
    let (value, latest) = recovery.hook_save();
    assert_eq!(latest[1].1.as_ref().unwrap()[0], "bash", "{value}");
    assert_eq!(value["result"]["carried_panes"], 0, "{value}");
}
