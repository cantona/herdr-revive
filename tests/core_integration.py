#!/usr/bin/env python3
"""PTY tests for the herdr-core host: a real interactive bash reads an rc file
written by `install --yes`, and a fake core answers revive's JSON-RPC calls.
Nothing touches the real home, config, state or herdr-core socket."""
import json
import os
from pathlib import Path
import pty
import select
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import termios
import threading
import time
import unittest

ROOT = Path(__file__).resolve().parents[1]
BINARY = Path(os.environ.get("REVIVE_BINARY", ROOT / "target/debug/herdr-revive"))
BASH = shutil.which("bash")
PROMPT = b"FIXTURE_READY>"
SPACE, TAB = "s1", "tb1"


class FakeCore:
    """herdr-core's client socket: one thread per connection, one JSON-RPC
    line per request, answered from `records` (keyed by terminal address)."""

    def __init__(self, path):
        self.path = path
        self.records = {}
        self.masters = {}
        self.requests = []
        self.sent = []
        self.errors = []
        self.lock = threading.Lock()
        self.stopping = threading.Event()
        self.handlers = []
        self.listener = socket.socket(socket.AF_UNIX)
        self.listener.bind(str(path))
        self.listener.listen(64)
        self.listener.settimeout(0.1)
        self.acceptor = threading.Thread(target=self.accept, daemon=True)
        self.acceptor.start()

    def attach(self, address, record, master):
        with self.lock:
            self.records[address] = record
            self.masters[address] = master

    def detach(self, address):
        with self.lock:
            self.records.pop(address, None)
            self.masters.pop(address, None)

    def methods(self):
        with self.lock:
            return [request["method"] for request in self.requests]

    def accept(self):
        while not self.stopping.is_set():
            try:
                conn, _ = self.listener.accept()
            except socket.timeout:
                continue
            except OSError:
                return
            thread = threading.Thread(target=self.handle, args=(conn,), daemon=True)
            with self.lock:
                self.handlers.append(thread)
            thread.start()

    def handle(self, conn):
        with conn:
            conn.settimeout(0.1)
            pending = b""
            while not self.stopping.is_set():
                try:
                    chunk = conn.recv(65536)
                except socket.timeout:
                    continue
                except OSError:
                    return
                if not chunk:
                    return
                pending += chunk
                while b"\n" in pending:
                    line, pending = pending.split(b"\n", 1)
                    try:
                        request = json.loads(line)
                        reply = dict(jsonrpc="2.0", id=request.get("id"), **self.answer(request))
                        conn.sendall(json.dumps(reply).encode() + b"\n")
                    except Exception as error:
                        self.errors.append(error)
                        return

    def answer(self, request):
        def error(kind):
            return dict(error=dict(code=-32000, message="refused", data=dict(kind=kind)))
        method = request["method"]
        params = request.get("params") or {}
        with self.lock:
            self.requests.append(request)
            if method == "hello":
                return dict(result=dict(
                    generation=1, version="0.1.18", core=dict(id="c0ffee", name="fixture"),
                    framed=False, methods=["hello", "terminal.list", "terminal.get", "terminal.send"],
                    features=[]))
            if method == "terminal.list":
                return dict(result=dict(terminals=list(self.records.values()),
                                        hidden_finished=0, machines=[]))
            if method == "terminal.get":
                record = self.records.get(params.get("terminal"))
                return dict(result=record) if record else error("not_found")
            if method == "terminal.send":
                master = self.masters.get(params.get("terminal"))
                if master is None:
                    return error("not_found")
                self.sent.append(params)
                os.write(master, (params["text"] + "\n").encode())
                return dict(result=dict(turn=1, status=None, started=None, wait=None))
        return error("unknown_method")

    def close(self):
        self.stopping.set()
        self.listener.close()
        self.acceptor.join(timeout=3)
        with self.lock:
            handlers = list(self.handlers)
        for thread in handlers:
            thread.join(timeout=3)
        stuck = [t for t in [self.acceptor, *handlers] if t.is_alive()]
        if stuck:
            self.errors.append(RuntimeError(f"{len(stuck)} fake core threads did not stop"))


class Shell:
    """An interactive bash on a pty, registered with the fake core before it
    reads its rc file: the child blocks on a pipe byte until its pid is known."""

    def __init__(self, fixture, address, env):
        self.fixture = fixture
        self.address = address
        self.output = bytearray()
        self.cursor = 0
        self.changed = threading.Condition()
        self.stopping = threading.Event()
        self.closed = False
        self.proc = None
        self.master, slave = pty.openpty()
        # pgrep -t takes the name without /dev/ on Linux (pts/N) and Darwin (ttysNNN).
        self.tty = os.ttyname(slave).removeprefix("/dev/")
        reader, writer = os.pipe()
        argv = [BASH, "--rcfile", str(fixture.rc), "-i"]
        try:
            try:
                self.proc = subprocess.Popen([sys.executable, "-I", "-c",
                    "import fcntl, os, termios; fcntl.ioctl(0, termios.TIOCSCTTY, 0)\n"
                    f"go = os.read({reader}, 1); os.close({reader})\n"
                    f"os.execv({BASH!r}, {argv!r}) if go == b'g' else os._exit(1)"],
                    stdin=slave, stdout=slave, stderr=slave, start_new_session=True,
                    cwd=fixture.work, env=env, pass_fds=(reader,))
            finally:
                os.close(slave)
                os.close(reader)
            self.pid = self.proc.pid
            fixture.core.attach(address, fixture.record(address, self.pid), self.master)
            os.write(writer, b"g")
        except BaseException:
            fixture.core.detach(address)
            if self.proc is not None:
                self.proc.kill()
                self.proc.wait(timeout=3)
            os.close(self.master)
            raise
        finally:
            os.close(writer)
        self.reader = threading.Thread(target=self.drain, daemon=True)
        self.reader.start()

    def drain(self):
        while not self.stopping.is_set():
            try:
                if not select.select([self.master], [], [], 0.05)[0]:
                    continue
                data = os.read(self.master, 65536)
            except OSError:
                return
            if not data:
                return
            with self.changed:
                self.output.extend(data)
                self.changed.notify_all()

    def transcript(self):
        with self.changed:
            return bytes(self.output)

    def wait_output(self, needle, timeout=5):
        """Waits for `needle` after the previous match and moves past it."""
        deadline = time.monotonic() + timeout
        with self.changed:
            while True:
                found = self.output.find(needle, self.cursor)
                if found >= 0:
                    self.cursor = found + len(needle)
                    return
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise AssertionError(
                        f"PTY of {self.address} did not produce {needle!r} within {timeout}s; "
                        f"output since the last match: {bytes(self.output[self.cursor:])!r}\n"
                        + self.fixture.describe())
                self.changed.wait(remaining)

    def type(self, text):
        os.write(self.master, (text + "\n").encode())

    def interrupt(self):
        os.write(self.master, termios.tcgetattr(self.master)[6][termios.VINTR])

    def foreground(self):
        return os.tcgetpgrp(self.master)

    def tpgid(self):
        """The terminal foreground group as the kernel reports it for the
        shell, which is what revive's core host reads."""
        if sys.platform == "darwin":
            return self.foreground()
        stat = Path(f"/proc/{self.pid}/stat").read_text()
        return int(stat[stat.rindex(")") + 2:].split()[5])

    def foreground_jobs(self):
        """The argv of every process in the terminal's foreground group except
        the shell. Each job, typed at the prompt or relaunched by init from
        the first prompt, leads its own group; while the shell itself is in
        the foreground this is empty."""
        jobs = []
        for pid in pgrep("-g", self.foreground()):
            if pid == self.pid:
                continue
            if sys.platform == "darwin":
                jobs.append(subprocess.run(["ps", "-o", "command=", "-p", str(pid)],
                            capture_output=True, text=True, timeout=5).stdout.split())
                continue
            try:
                raw = Path(f"/proc/{pid}/cmdline").read_bytes()
            except (FileNotFoundError, ProcessLookupError):
                continue
            jobs.append([part.decode() for part in raw.split(b"\0")[:-1]])
        return jobs

    def environment(self, pid):
        """The environment process `pid` started with, as the fixture's sleep
        wrapper recorded it."""
        text = self.fixture.env_dir.joinpath(str(pid)).read_text()
        return dict(line.split("=", 1) for line in text.splitlines() if "=" in line)

    def stop(self):
        """Ends the shell, its jobs and every hook save it left running. Safe
        to call again, and on a shell that has already exited."""
        if self.closed:
            return
        self.closed = True
        self.fixture.core.detach(self.address)
        try:
            for sig in (signal.SIGHUP, signal.SIGKILL):
                if self.proc.poll() is not None:
                    break
                self.signal_foreground(sig)
                self.proc.send_signal(sig)
                try:
                    self.proc.wait(timeout=3)
                except subprocess.TimeoutExpired:
                    continue
            if self.proc.poll() is None:
                raise AssertionError(f"shell {self.pid} survived SIGKILL")
            self.kill_session()
        finally:
            self.stopping.set()
            self.reader.join(timeout=3)
            os.close(self.master)

    def signal_foreground(self, sig):
        # Once the session leader is gone tcgetpgrp reports 0, and killpg(0)
        # would signal the test runner's own group.
        try:
            group = self.foreground()
        except OSError:
            return
        if group > 0 and group != os.getpgrp():
            try:
                os.killpg(group, sig)
            except ProcessLookupError:
                pass

    def kill_session(self):
        # Hook saves are detached into the background with a delay; one that
        # outlived the shell would write into the next shell's state, or into
        # a removed temp dir.
        deadline = time.monotonic() + 3
        while time.monotonic() < deadline:
            pids = pgrep("-t", self.tty)
            if not pids:
                return
            for pid in pids:
                try:
                    os.kill(pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
            time.sleep(0.02)
        raise AssertionError(f"processes on {self.tty} survived SIGKILL")


def pgrep(option, value):
    """The pids pgrep selects; any status but 0 (found) or 1 (none) is an
    error, so an unsupported option cannot pass as an empty result."""
    result = subprocess.run(["pgrep", option, str(value)],
                            capture_output=True, text=True, timeout=5)
    if option == "-t" and "No such tty" in result.stderr:
        # Darwin's pgrep refuses a pty that no process holds open any more.
        return []
    if result.returncode not in (0, 1):
        raise AssertionError(f"pgrep {option} {value} exited {result.returncode}: {result.stderr}")
    return sorted(int(pid) for pid in result.stdout.split())


class Fixture:
    def __init__(self, auto_save=False, auto_restore=False):
        # Darwin's default TMPDIR can exceed the Unix socket path limit.
        self.temp = tempfile.TemporaryDirectory(prefix="herdr-revive-core-test-", dir="/tmp")
        self.root = Path(self.temp.name).resolve()
        self.shells = []
        self.core = None
        try:
            self.setup(auto_save, auto_restore)
        except BaseException:
            self.close()
            raise

    def setup(self, auto_save, auto_restore):
        self.home = self.root / "home"
        self.work = self.root / "work"
        self.bin_dir = self.root / "bin"
        self.state = self.root / "state"
        self.config_home = self.root / "config"
        for path in (self.home, self.work, self.bin_dir, self.state, self.config_home):
            path.mkdir()
        self.bin_dir.joinpath("herdr-revive").symlink_to(BINARY.resolve())
        # Darwin hides the environment of platform binaries such as /bin/sleep
        # from ps, so the sleep the tests run records the environment it was
        # started with, then becomes /bin/sleep under its own name.
        self.env_dir = self.root / "env"
        self.env_dir.mkdir()
        sleep = self.bin_dir / "sleep"
        sleep.write_text("#!/bin/bash\n"
                         f"env > \"{self.env_dir}/$$\"\n"
                         "exec -a sleep /bin/sleep \"$@\"\n")
        sleep.chmod(0o755)
        self.core = FakeCore(self.root / "core.sock")
        config = self.config_home / "herdr-revive"
        config.mkdir()
        config.joinpath("config.toml").write_text("\n".join(
            f"{key} = {json.dumps(value)}" for key, value in dict(
                auto_save=auto_save, auto_restore=auto_restore, allowed_programs=["sleep"],
                timeout_ms=2000, settle_ms=0, debounce_ms=0).items()) + "\n")
        installed = subprocess.run([str(BINARY), "install", "--yes"], env=self.env(),
                                   capture_output=True, text=True, timeout=15)
        if installed.returncode or not installed.stdout.endswith("done\n"):
            raise AssertionError(f"install --yes: {installed.stdout}{installed.stderr}")
        hook = next(line for line in installed.stdout.splitlines() if "shell hook in " in line)
        self.rc = Path(hook.split("shell hook in ", 1)[1].split("; ")[0]).resolve()
        # PS1 goes first so the prompt is the fixture's only once the rc file,
        # including any relaunch it evaluates, has finished.
        self.rc.write_text("PS1='FIXTURE_READY> '\n" + self.rc.read_text())

    def env(self, **extra):
        env = dict(
            PATH=f"{self.bin_dir}:{os.environ['PATH']}",
            HOME=str(self.home),
            XDG_CONFIG_HOME=str(self.config_home),
            XDG_STATE_HOME=str(self.state),
            XDG_CACHE_HOME=str(self.root / "cache"),
            XDG_DATA_HOME=str(self.root / "data"),
            HC_SOCKET=str(self.core.path),
            HERDR_REVIVE_HOST="core",
            PS1="FIXTURE_READY> ",
            TERM="dumb",
        )
        env.update(extra)
        return env

    def record(self, address, pid):
        return dict(address=address, state="live", pid=pid, cwd=str(self.work), keep=True,
                    labels={"herdr.space": SPACE, "herdr.tab": TAB, "herdr.seen": "0"},
                    argv=["bash"], status=None, foreground=None)

    def start_shell(self, address="t1", extra_env=None):
        shell = Shell(self, address, self.env(HC_TERMINAL_ID=address, **(extra_env or {})))
        self.shells.append(shell)
        return shell

    def run(self, *args):
        result = subprocess.run([str(BINARY), "--host", "core", *args], env=self.env(),
                                capture_output=True, text=True, timeout=15)
        if result.returncode:
            raise AssertionError(f"{args} exited {result.returncode}: {result.stderr}\n"
                                 + self.describe())
        return json.loads(result.stdout)

    def session_files(self, pattern):
        return sorted(self.state.glob(f"herdr-revive/*/{pattern}"))

    def latest(self):
        paths = self.session_files("latest.json")
        if len(paths) > 1:
            raise AssertionError(f"more than one store session: {paths}")
        return json.loads(paths[0].read_text()) if paths else None

    def snapshots(self):
        return self.session_files("snapshots/*.json")

    def boots(self):
        return self.session_files("boots/*.json")

    def wait_until(self, predicate, timeout=10, what="condition"):
        deadline = time.monotonic() + timeout
        while True:
            try:
                value = predicate()
            except (LookupError, OSError):
                value = None
            if value:
                return value
            if time.monotonic() > deadline:
                raise AssertionError(f"{what} did not hold within {timeout}s\n" + self.describe())
            time.sleep(0.05)

    def wait_latest(self, predicate, timeout=10):
        def check():
            snapshot = self.latest()
            return snapshot if snapshot is not None and predicate(snapshot) else None
        return self.wait_until(check, timeout, "latest snapshot predicate")

    def describe(self):
        lines = []
        for shell in self.shells:
            lines.append(f"--- pty {shell.address} (pid {shell.pid}) ---")
            lines.append(repr(shell.transcript()))
        if self.core is not None:
            lines.append(f"--- core requests: {self.core.methods()}")
            lines.append(f"--- core sent: {self.core.sent}")
        try:
            lines.append(f"--- latest: {json.dumps(self.latest())}")
        except (OSError, ValueError, AssertionError) as error:
            lines.append(f"--- latest unreadable: {error}")
        return "\n".join(lines)

    def close(self):
        errors = []
        for shell in self.shells:
            try:
                shell.stop()
            except Exception as error:
                errors.append(error)
        if self.core is not None:
            self.core.close()
            errors.extend(self.core.errors)
        self.temp.cleanup()
        if errors:
            raise AssertionError(f"fixture cleanup errors: {errors!r}")


class CoreHost(unittest.TestCase):
    def fixture(self, **kwargs):
        fx = Fixture(**kwargs)
        self.addCleanup(fx.close)
        return fx

    def command(self, snapshot):
        return snapshot["panes"][0]["command"]

    def save_running_sleep(self, fx):
        """A first shell runs `sleep 600` until the hook has saved it, then dies
        like a closed pane would."""
        first = fx.start_shell()
        first.wait_output(PROMPT)
        first.type("sleep 600")
        saved = fx.wait_latest(self.command)
        self.assertEqual(self.command(saved)["argv"], ["sleep", "600"], fx.describe())
        first.stop()
        return saved

    def test_hook_saves_at_command_start_and_prompt_return(self):
        fx = self.fixture(auto_save=True)
        sh = fx.start_shell()
        sh.wait_output(PROMPT)
        sh.type("declare -F __revive_preexec")
        sh.wait_output(b"\r\n__revive_preexec\r\n")
        sh.type("sleep 2")
        running = fx.wait_latest(self.command)
        self.assertEqual(self.command(running), dict(kind="program", argv=["sleep", "2"]),
                         fx.describe())
        self.assertEqual(running["panes"][0]["pane_id"], f"{SPACE}:pt1")
        sh.wait_output(PROMPT, timeout=6)
        fx.wait_latest(lambda s: self.command(s) is None)
        self.assertIn("terminal.list", fx.core.methods())
        self.assertEqual(fx.core.sent, [])
        count = len(fx.snapshots())
        requests = len(fx.core.methods())
        sh.type("echo hello")
        sh.wait_output(b"\r\nhello\r\n")
        sh.wait_output(PROMPT)
        time.sleep(1.5)
        self.assertEqual((len(fx.snapshots()), len(fx.core.methods())), (count, requests),
                         fx.describe())

    def test_init_relaunches_the_saved_command_before_the_first_prompt(self):
        fx = self.fixture(auto_save=True, auto_restore=True)
        first_save = self.save_running_sleep(fx)
        # rc lines below the marker block (PATH, nvm, conda) must apply to the
        # relaunched program too.
        late = fx.root / "late"
        fx.rc.write_text(fx.rc.read_text()
                         + f"export REVIVE_AFTER=1\nexport PATH=\"$PATH:{late}\"\n")
        second = fx.start_shell()
        fx.wait_until(lambda: second.tpgid() != second.pid
                      and second.foreground_jobs() == [["sleep", "600"]],
                      what="relaunched sleep as its own foreground group")
        environment = second.environment(second.tpgid())
        self.assertEqual(environment.get("REVIVE_AFTER"), "1", fx.describe())
        self.assertTrue(environment.get("PATH", "").endswith(f":{late}"), environment.get("PATH"))
        with self.assertRaises(AssertionError):
            second.wait_output(PROMPT, timeout=1)
        self.assertEqual(fx.core.sent, [], "init must emit the command, not type it")
        boots = fx.boots()
        self.assertEqual(len(boots), 1, fx.describe())
        journal = json.loads(boots[0].read_text())
        self.assertEqual([(e["pane_id"], e["outcome"]) for e in journal["entries"]],
                         [(f"{SPACE}:pt1", "applied")], fx.describe())
        # Regression: a relaunched command that shared bash's group made every
        # save record bash instead.
        hook_save = fx.wait_latest(lambda s: s["created_ms"] > first_save["created_ms"])
        self.assertEqual(self.command(hook_save), dict(kind="program", argv=["sleep", "600"]),
                         fx.describe())
        explicit = fx.run("autosave", "--force")
        self.assertEqual(explicit["result"]["status"], "saved", explicit)
        self.assertEqual(self.command(fx.latest()), dict(kind="program", argv=["sleep", "600"]),
                         fx.describe())
        second.interrupt()
        second.wait_output(PROMPT)
        fx.wait_latest(lambda s: self.command(s) is None)

    def test_a_prompt_command_replaced_after_the_block_reports_the_lost_relaunch(self):
        fx = self.fixture(auto_save=True, auto_restore=True)
        self.save_running_sleep(fx)
        fx.rc.write_text(fx.rc.read_text() + "PROMPT_COMMAND='history -a'\n")
        lost = fx.start_shell()
        lost.wait_output(b"the relaunch did not run because PROMPT_COMMAND was replaced"
                         b" after the herdr-revive line")
        lost.wait_output(PROMPT)
        self.assertEqual(lost.foreground(), lost.pid, fx.describe())
        lost.type("echo typed")
        lost.wait_output(b"\r\ntyped\r\n")
        lost.wait_output(PROMPT)
        self.assertEqual(lost.transcript().count(b"the relaunch did not run"), 1, fx.describe())
        self.assertEqual(lost.foreground_jobs(), [], fx.describe())

    def test_rc_lines_that_extend_prompt_command_keep_working(self):
        fx = self.fixture(auto_save=True, auto_restore=True)
        self.save_running_sleep(fx)
        # Not `history -a`: bash 3.2 (macOS) never appends from PROMPT_COMMAND
        # on a pty, so the appended command leaves its own mark instead.
        prompts = fx.root / "prompts"
        fx.config_home.joinpath("herdr-revive", "config.toml").write_text(
            'auto_save = false\nauto_restore = true\nallowed_programs = ["sleep"]\n'
            "timeout_ms = 2000\nsettle_ms = 0\n")
        fx.rc.write_text(fx.rc.read_text()
                         + f'PROMPT_COMMAND="${{PROMPT_COMMAND:+$PROMPT_COMMAND; }}echo prompt >> {prompts}"\n')
        sh = fx.start_shell()
        fx.wait_until(lambda: sh.foreground_jobs() == [["sleep", "600"]],
                      what="relaunched sleep with an appended PROMPT_COMMAND")
        sh.interrupt()
        sh.wait_output(PROMPT)
        sh.type("echo first")
        sh.wait_output(PROMPT)
        sh.type("echo second")
        sh.wait_output(PROMPT)
        self.assertNotIn(b"syntax error", sh.transcript(), fx.describe())
        fx.wait_until(lambda: prompts.exists() and prompts.read_text().count("prompt") >= 2,
                      what="the appended PROMPT_COMMAND ran at both prompts")

    def test_a_prepended_prompt_command_keeps_the_relaunch(self):
        fx = self.fixture(auto_save=True, auto_restore=True)
        self.save_running_sleep(fx)
        fx.rc.write_text(fx.rc.read_text() + 'PROMPT_COMMAND="true; $PROMPT_COMMAND"\n')
        sh = fx.start_shell()
        fx.wait_until(lambda: sh.foreground_jobs() == [["sleep", "600"]],
                      what="relaunched sleep with a prepended PROMPT_COMMAND")
        self.assertNotIn(b"the relaunch did not run", sh.transcript())

    def test_stopping_a_shell_that_already_exited_spares_the_runner(self):
        fx = self.fixture()
        sh = fx.start_shell()
        sh.wait_output(PROMPT)
        sh.type("exit")
        fx.wait_until(lambda: sh.proc.poll() is not None, what="shell exit")
        received = []
        previous = signal.signal(signal.SIGHUP, lambda sig, frame: received.append(sig))
        try:
            sh.stop()
            sh.stop()
        finally:
            signal.signal(signal.SIGHUP, previous)
        self.assertEqual(received, [])
        self.assertEqual(pgrep("-t", sh.tty), [])

    def test_init_is_silent_when_it_cannot_act(self):
        fx = self.fixture(auto_save=True, auto_restore=True)
        env = fx.env()
        result = subprocess.run([str(BINARY), "init", "bash"], env=env,
                                capture_output=True, text=True, timeout=15)
        self.assertEqual((result.returncode, result.stdout, result.stderr), (0, "", ""))
        self.assertEqual(fx.core.methods(), [])
        env.update(HC_TERMINAL_ID="t1", REVIVE_DEBUG="1")
        fx.core.attach("t1", fx.record("t1", 1), None)
        result = subprocess.run([str(BINARY), "init", "bash"], env=env,
                                capture_output=True, text=True, timeout=15)
        self.assertEqual((result.returncode, result.stdout), (0, ""), result.stderr)
        self.assertIn("not started by the pane's shell", result.stderr)
        self.assertEqual(fx.core.methods(), ["hello", "terminal.get"])
        self.assertEqual(fx.core.requests[-1]["params"], dict(terminal="t1"))

    def test_revive_auto_zero_skips_with_a_notice(self):
        fx = self.fixture(auto_save=True, auto_restore=True)
        self.save_running_sleep(fx)
        second = fx.start_shell(extra_env={"REVIVE_AUTO": "0"})
        second.wait_output(b"automatic relaunch skipped (REVIVE_AUTO=0)")
        second.wait_output(PROMPT)
        self.assertEqual(second.foreground(), second.pid)
        self.assertEqual(fx.boots(), [])
        self.assertEqual(fx.core.sent, [])

    def test_restore_types_through_terminal_send(self):
        fx = self.fixture(auto_save=True)
        self.save_running_sleep(fx)
        second = fx.start_shell()
        second.wait_output(PROMPT)
        self.assertEqual(second.foreground(), second.pid, "init relaunched without auto_restore")
        result = fx.run("restore")
        self.assertEqual(result["result"]["status"], "restored", result)
        self.assertEqual(fx.core.sent, [dict(terminal="t1",
                         text=f" cd -- '{fx.work}' && command 'sleep' '600'")])
        fx.wait_until(lambda: second.foreground_jobs() == [["sleep", "600"]],
                      what="restored sleep in the foreground")


if __name__ == "__main__":
    unittest.main(verbosity=2)
