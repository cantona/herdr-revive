#![cfg(unix)]

use herdr_revive::core::{install, paths};
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use serde_json::{Value, json};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const SPACE: &str = "s1";
const TAB: &str = "tb1";
const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
const STEP_TIMEOUT: Duration = Duration::from_secs(20);

/// One isolated herdr-core: its own HOME, XDG dirs and HC_SESSION, so `hc`
/// resolves a socket under the temp dir and never reaches the user's core.
/// Dropping it ends every terminal, the core, and anything else that still
/// carries this HOME, even when the test panicked.
struct Core {
    env: Vec<(String, String)>,
    home: PathBuf,
    root: PathBuf,
    socket: PathBuf,
    core_pid: Option<i32>,
    addresses: Vec<String>,
    calls: usize,
    _dir: tempfile::TempDir,
}

impl Core {
    fn new() -> Self {
        let dir = tempfile::Builder::new()
            .prefix("rv")
            .tempdir_in("/tmp")
            .unwrap();
        let root = dir.path().canonicalize().unwrap();
        for sub in [
            "home",
            "config/herdr-revive",
            "config/herdr-core",
            "state",
            "run",
            "cache",
            "data",
            "bin",
            "work",
            "out",
        ] {
            std::fs::create_dir_all(root.join(sub)).unwrap();
        }
        let session = format!("rv{}", std::process::id());
        let path = format!(
            "{}:{}",
            root.join("bin").display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let dir_var =
            |name: &str, sub: &str| (name.to_string(), root.join(sub).display().to_string());
        let env = vec![
            dir_var("HOME", "home"),
            ("PATH".into(), path),
            dir_var("XDG_CONFIG_HOME", "config"),
            dir_var("XDG_STATE_HOME", "state"),
            dir_var("XDG_RUNTIME_DIR", "run"),
            dir_var("XDG_CACHE_HOME", "cache"),
            dir_var("XDG_DATA_HOME", "data"),
            ("HC_SESSION".into(), session.clone()),
            ("HERDR_REVIVE_HOST".into(), "core".into()),
            ("TERM".into(), "dumb".into()),
        ];
        Self {
            env,
            home: root.join("home"),
            socket: root.join("run/herdr-core").join(format!("{session}.sock")),
            root,
            core_pid: None,
            addresses: Vec::new(),
            calls: 0,
            _dir: dir,
        }
    }

    fn path(&self, sub: &str) -> PathBuf {
        self.root.join(sub)
    }

    /// Runs a program in the isolated environment with stdout and stderr in
    /// files, so a daemon that inherits them cannot hold the call open, and
    /// kills it at the deadline.
    fn try_exec(
        &mut self,
        program: &Path,
        args: &[&str],
    ) -> Result<(bool, String, String), String> {
        self.calls += 1;
        let out = self.path("out").join(format!("{}.out", self.calls));
        let err = self.path("out").join(format!("{}.err", self.calls));
        let text = |e: std::io::Error| e.to_string();
        let mut child = Command::new(program)
            .args(args)
            .env_clear()
            .envs(self.env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::null())
            .stdout(File::create(&out).map_err(text)?)
            .stderr(File::create(&err).map_err(text)?)
            .spawn()
            .map_err(text)?;
        let deadline = Instant::now() + COMMAND_TIMEOUT;
        let status = loop {
            if let Some(status) = child.try_wait().map_err(text)? {
                break status;
            }
            if Instant::now() > deadline {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("did not finish within {COMMAND_TIMEOUT:?}"));
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        Ok((
            status.success(),
            std::fs::read_to_string(out).map_err(text)?,
            std::fs::read_to_string(err).map_err(text)?,
        ))
    }

    fn exec(&mut self, program: &Path, args: &[&str]) -> (bool, String, String) {
        self.try_exec(program, args)
            .unwrap_or_else(|e| panic!("{} {args:?}: {e}", program.display()))
    }

    fn hc(&mut self, args: &[&str]) -> Value {
        let (ok, stdout, stderr) = self.exec(Path::new("hc"), args);
        assert!(ok, "hc {args:?}: {stderr}{stdout}");
        serde_json::from_str(&stdout).unwrap_or(Value::Null)
    }

    fn get(&mut self, address: &str) -> Value {
        self.hc(&["get", address, "--json"])
    }

    fn screen(&mut self, address: &str) -> String {
        self.exec(Path::new("hc"), &["read", address]).1
    }

    fn start(&mut self) {
        let started = self.hc(&["server", "start"]);
        self.core_pid = started["pid"]
            .as_i64()
            .and_then(|pid| i32::try_from(pid).ok());
        assert!(self.core_pid.is_some(), "server start: {started}");
        assert!(
            self.socket.exists(),
            "the core did not listen at {}",
            self.socket.display()
        );
        let status = self.hc(&["server", "status"]);
        assert_eq!(status["pid"], started["pid"], "{status}");
    }

    fn run_kept_shell(&mut self) -> String {
        let work = self.path("work").display().to_string();
        let space = format!("herdr.space={SPACE}");
        let tab = format!("herdr.tab={TAB}");
        let started = self.hc(&[
            "run",
            "--json",
            "--keep",
            "--bg",
            "--no-parent",
            "--label",
            &space,
            "--label",
            &tab,
            "--cwd",
            &work,
            "--",
            "bash",
        ]);
        let address = started["address"]
            .as_str()
            .unwrap_or_else(|| panic!("run: {started}"))
            .to_string();
        self.addresses.push(address.clone());
        address
    }

    fn store_dir(&self) -> PathBuf {
        let (_, session) = paths::session_identity(&self.socket).unwrap();
        self.path("state/herdr-revive").join(session)
    }

    fn latest(&self) -> Option<Value> {
        let text = std::fs::read_to_string(self.store_dir().join("latest.json")).ok()?;
        serde_json::from_str(&text).ok()
    }

    fn describe(&mut self, address: &str) -> String {
        let record = self.exec(Path::new("hc"), &["get", address, "--json"]).1;
        let screen = self.screen(address);
        format!(
            "record: {record}\nscreen: {screen:?}\nlatest: {}",
            self.latest().unwrap_or(Value::Null)
        )
    }

    fn wait(&mut self, address: &str, what: &str, mut check: impl FnMut(&mut Self) -> bool) {
        let deadline = Instant::now() + STEP_TIMEOUT;
        while !check(self) {
            if Instant::now() > deadline {
                let state = self.describe(address);
                panic!("timed out after {STEP_TIMEOUT:?} waiting for {what}\n{state}");
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// Every process that inherited this core's HOME: the core, its shells,
    /// their jobs and detached hook saves. The test runner itself does not
    /// carry it.
    #[cfg(target_os = "linux")]
    fn strays(&self) -> Vec<i32> {
        let marker = format!("HOME={}", self.home.display());
        let Ok(entries) = std::fs::read_dir("/proc") else {
            return Vec::new();
        };
        entries
            .flatten()
            .filter_map(|entry| entry.file_name().to_str()?.parse::<i32>().ok())
            .filter(|pid| *pid != std::process::id() as i32)
            .filter(|pid| {
                std::fs::read(format!("/proc/{pid}/environ")).is_ok_and(|environ| {
                    environ
                        .split(|b| *b == 0)
                        .any(|entry| entry == marker.as_bytes())
                })
            })
            .collect()
    }

    #[cfg(not(target_os = "linux"))]
    fn strays(&self) -> Vec<i32> {
        self.core_pid
            .into_iter()
            .filter(|pid| kill(Pid::from_raw(*pid), None).is_ok())
            .collect()
    }

    fn shut_down(&mut self) {
        for address in std::mem::take(&mut self.addresses) {
            let _ = self.try_exec(Path::new("hc"), &["kill", &address, "--json"]);
        }
        if self.core_pid.is_some() {
            let _ = self.try_exec(Path::new("hc"), &["server", "stop"]);
        }
    }

    fn wait_strays_gone(&self, timeout: Duration) -> Vec<i32> {
        let deadline = Instant::now() + timeout;
        loop {
            let strays = self.strays();
            if strays.is_empty() || Instant::now() > deadline {
                return strays;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

impl Drop for Core {
    fn drop(&mut self) {
        self.shut_down();
        for pid in self.wait_strays_gone(Duration::from_secs(5)) {
            let _ = kill(Pid::from_raw(pid), Signal::SIGKILL);
        }
        let left = self.wait_strays_gone(Duration::from_secs(5));
        if !left.is_empty() && !std::thread::panicking() {
            panic!("processes of the test core survived SIGKILL: {left:?}");
        }
    }
}

fn parent_of(pid: u64) -> Option<u64> {
    let output = Command::new("ps")
        .args(["-o", "ppid=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    String::from_utf8(output.stdout).ok()?.trim().parse().ok()
}

#[test]
fn kept_shell_relaunches_its_program_after_loss() {
    if Command::new("hc").arg("--version").output().is_err() {
        eprintln!("skipped: hc not on PATH");
        return;
    }
    let mut core = Core::new();
    std::os::unix::fs::symlink(
        env!("CARGO_BIN_EXE_herdr-revive"),
        core.path("bin/herdr-revive"),
    )
    .unwrap();
    std::fs::write(
        core.home.join(".bashrc"),
        format!("PS1='READY> '\n{}\n", install::RC_LINE),
    )
    .unwrap();
    std::fs::write(
        core.path("config/herdr-revive/config.toml"),
        "auto_save = true\nauto_restore = true\nallowed_programs = [\"sleep\"]\n\
         timeout_ms = 3000\nsettle_ms = 0\ndebounce_ms = 0\n",
    )
    .unwrap();
    std::fs::write(
        core.path("config/herdr-core/config.toml"),
        "shell = [\"bash\"]\nnotes = false\n",
    )
    .unwrap();
    let work = core.path("work").display().to_string();

    core.start();
    let address = core.run_kept_shell();
    let pane_id = format!("{SPACE}:p{address}");
    core.wait(&address, "the rc file's prompt", |c| {
        c.screen(&address).contains("READY>")
    });
    let first = core.get(&address);
    let first_shell = first["pid"].as_u64().unwrap();
    assert_eq!(
        first["labels"],
        json!({"herdr.space": SPACE, "herdr.tab": TAB}),
        "{first}"
    );

    core.hc(&["send", &address, "sleep 600"]);
    core.wait(&address, "a snapshot with the running sleep", |c| {
        c.latest()
            .is_some_and(|s| s["panes"][0]["command"]["argv"] == json!(["sleep", "600"]))
    });
    let saved = core.latest().unwrap();
    assert_eq!(saved["panes"].as_array().map(Vec::len), Some(1), "{saved}");
    assert_eq!(saved["panes"][0]["pane_id"], pane_id.as_str(), "{saved}");
    assert_eq!(
        saved["panes"][0]["tab_id"],
        format!("{SPACE}:t{TAB}"),
        "{saved}"
    );
    assert_eq!(saved["panes"][0]["cwd"], work.as_str(), "{saved}");
    let mut first_sleep = None;
    core.wait(&address, "core to see the first sleep", |c| {
        let record = c.get(&address);
        first_sleep = record["foreground"]["pid"].as_u64();
        record["foreground"]["argv"] == json!(["sleep", "600"])
    });

    kill(
        Pid::from_raw(i32::try_from(first_shell).unwrap()),
        Signal::SIGKILL,
    )
    .unwrap();
    let mut second_shell = 0;
    core.wait(&address, "the keep-recovery shell", |c| {
        let record = c.get(&address);
        second_shell = record["pid"].as_u64().unwrap_or(0);
        record["state"] == "live" && second_shell != 0 && second_shell != first_shell
    });
    let mut relaunched = Value::Null;
    core.wait(&address, "the relaunched sleep", |c| {
        relaunched = c.get(&address);
        relaunched["foreground"]["argv"] == json!(["sleep", "600"])
            && relaunched["foreground"]["cwd"] == work.as_str()
            && relaunched["pid"].as_u64() == Some(second_shell)
    });
    let sleep_pid = relaunched["foreground"]["pid"].as_u64().unwrap();
    assert_ne!(Some(sleep_pid), first_sleep, "{relaunched}");
    assert_eq!(parent_of(sleep_pid), Some(second_shell), "{relaunched}");
    assert_eq!(
        relaunched["labels"],
        json!({"herdr.space": SPACE, "herdr.tab": TAB}),
        "{relaunched}"
    );

    // The relaunched sleep's own hook save may still hold the session lock.
    let mut inspect = Value::Null;
    core.wait(
        &address,
        "the relaunch's hook save to release the lock",
        |c| {
            let revive = Path::new(env!("CARGO_BIN_EXE_herdr-revive"));
            let (ok, stdout, stderr) = c.exec(revive, &["recovery", "inspect"]);
            if ok {
                inspect = serde_json::from_str(&stdout).unwrap();
                return true;
            }
            assert!(
                stderr.contains("holds the session lock"),
                "herdr-revive recovery inspect: {stderr}"
            );
            false
        },
    );
    let boots = inspect["result"]["boots"].as_array().unwrap();
    assert_eq!(boots.len(), 1, "{inspect}");
    let boot_path = PathBuf::from(boots[0].as_str().unwrap());
    assert_eq!(
        boot_path.parent(),
        Some(core.store_dir().join("boots").as_path())
    );
    let boot: Value = serde_json::from_str(&std::fs::read_to_string(&boot_path).unwrap()).unwrap();
    assert_eq!(boot["state"], "done", "{boot}");
    assert_eq!(
        boot["entries"],
        json!([{"pane_id": pane_id, "outcome": "applied"}]),
        "{boot}"
    );

    core.hc(&["interrupt", &address, "--json"]);
    core.wait(&address, "a snapshot without the sleep", |c| {
        c.latest().is_some_and(|s| {
            s["panes"][0]["pane_id"] == pane_id.as_str() && s["panes"][0]["command"].is_null()
        })
    });

    core.shut_down();
    let strays = core.wait_strays_gone(Duration::from_secs(10));
    assert_eq!(strays, Vec::<i32>::new(), "processes outlived the core");
}
