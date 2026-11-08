use std::process::Command;
use std::time::{Duration, Instant};

/// A process that owns a pty, standing in for a terminal's shell: capture
/// reads the shell's foreground group from its controlling terminal, so
/// `start` returns only once the child leads that group. `start` runs
/// `sleep 30`, a real program in the pane.
pub struct Shell {
    keeper: std::process::Child,
    pub pid: u32,
}

impl Shell {
    pub fn start() -> Self {
        Self::start_with(&["sleep", "30"])
    }

    /// Runs `argv` as the pty's session leader.
    pub fn start_with(argv: &[&str]) -> Self {
        Self::launch(argv, |pid, group| group == pid)
    }

    /// Runs `argv` as the pty's session leader and returns once another
    /// process group holds the terminal, as a job started under `set -m`.
    pub fn start_job(argv: &[&str]) -> Self {
        Self::launch(argv, |pid, group| group != pid && group > 0)
    }

    fn launch(argv: &[&str], settled: impl Fn(u32, u32) -> bool) -> Self {
        let script = format!(
            "import os, pty, sys\n\
             pid, fd = pty.fork()\n\
             if pid == 0:\n    os.execvp({program:?}, {argv:?})\n\
             print(pid, flush=True)\n\
             sys.stdin.read()\n\
             os.killpg(pid, 9)\n\
             os.waitpid(pid, 0)\n",
            program = argv[0],
        );
        let mut keeper = Command::new("python3")
            .arg("-I")
            .arg("-c")
            .arg(&script)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut line = String::new();
        std::io::BufRead::read_line(
            &mut std::io::BufReader::new(keeper.stdout.as_mut().unwrap()),
            &mut line,
        )
        .unwrap();
        let pid = line.trim().parse().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !herdr_revive::platform::terminal_foreground_group(pid)
            .is_ok_and(|group| settled(pid, group))
        {
            assert!(
                Instant::now() < deadline,
                "pty child {pid} never settled its terminal"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        Self { keeper, pid }
    }
}

impl Drop for Shell {
    fn drop(&mut self) {
        drop(self.keeper.stdin.take());
        let _ = self.keeper.wait();
    }
}
