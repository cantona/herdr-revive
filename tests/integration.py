#!/usr/bin/env python3
"""Isolated Linux PTY/API tests. No real Herdr session is used."""
import concurrent.futures
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
import threading
import time
import unittest

ROOT = Path(__file__).resolve().parents[1]
BINARY = Path(os.environ.get("REVIVE_BINARY", ROOT / "target/debug/herdr-revive"))


class Fixture:
    def __init__(self, transport="direct", panes=1):
        self.temp = tempfile.TemporaryDirectory(prefix="herdr-revive-test-")
        self.root = Path(self.temp.name)
        self.config = self.root / "config"
        self.config.mkdir()
        self.state = self.root / "state"
        self.socket_path = self.root / "host.sock"
        self.transport = transport
        self.panes = panes
        self.mutations = []
        self.requests = []
        self.drop_mutation_reply = False
        self.before_run = None
        self.bin_dir = self.root / "bin"
        self.bin_dir.mkdir()
        for program in ("ssh", "minicom", "claude", "codex", "gemini", "copilot", "cursor-agent"):
            path = self.bin_dir / program
            path.write_text("#!/usr/bin/python3\nimport json, os, sys\n"
                            "with open(os.environ['REVIVE_TEST_OUTPUT'], 'w') as f:\n"
                            " json.dump(sys.argv, f)\n")
            path.chmod(0o700)
        self.master, slave = pty.openpty()
        env = os.environ.copy()
        env.update(PATH=str(self.bin_dir) + ":/usr/bin:/bin",
                   REVIVE_TEST_OUTPUT=str(self.root / "argv.json"), PS1="FIXTURE_READY> ")
        self.shell = subprocess.Popen([sys.executable, "-c",
            "import fcntl, termios, os; fcntl.ioctl(0, termios.TIOCSCTTY, 0); "
            "os.execv('/bin/bash', ['bash', '--noprofile', '--norc', '-i'])"],
            stdin=slave, stdout=slave, stderr=slave, start_new_session=True,
            cwd=self.root, env=env)
        self.pid = self.shell.pid
        os.close(slave)
        self.wait_output(b"FIXTURE_READY>")
        self.listener = socket.socket(socket.AF_UNIX)
        self.listener.bind(str(self.socket_path))
        self.listener.listen(128)
        self.listener.settimeout(0.1)
        self.stopping = threading.Event()
        self.errors = []
        self.thread = threading.Thread(target=self.serve, daemon=True)
        self.thread.start()
        self.write_config()

    def write_config(self, **values):
        config = dict(auto_restore=False, auto_save=False, settle_ms=0,
                      timeout_ms=1000, debounce_ms=3600000, transport=self.transport,
                      allowed_programs=["ssh", "minicom", "claude", "codex", "gemini", "copilot", "cursor-agent"])
        config.update(values)
        self.config.joinpath("config.toml").write_text("\n".join(
            f"{key} = {json.dumps(value)}" for key, value in config.items()))

    def wait_output(self, needle, timeout=3):
        output = bytearray()
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if select.select([self.master], [], [], 0.05)[0]:
                output.extend(os.read(self.master, 65536))
                if needle in output:
                    return bytes(output)
        raise AssertionError(f"PTY did not produce {needle!r}: {output!r}")

    def pane(self, i=1):
        return dict(workspace_id="w1", tab_id="w1:t1", pane_id=f"w1:p{i}",
                    terminal_id=f"term_fixture_{i}", cwd=str(self.root),
                    agent=None, agent_status="unknown")

    def result(self, request):
        method = request["method"]
        self.requests.append(method)
        if method == "ping":
            return dict(type="pong", version="0.9.1", protocol=22)
        if method == "session.snapshot":
            return dict(type="session_snapshot", snapshot=dict(version="0.9.1", protocol=22,
                        workspaces=[dict(workspace_id="w1")],
                        tabs=[dict(workspace_id="w1", tab_id="w1:t1")],
                        panes=[self.pane(i) for i in range(1, self.panes + 1)]))
        if method == "pane.get":
            return dict(type="pane_info", pane=self.pane())
        if method == "layout.export":
            def tree(ids):
                if len(ids) == 1:
                    return dict(type="pane", pane_id=ids[0], cwd=str(self.root))
                middle = len(ids)//2
                return dict(type="split", direction="right", ratio=0.5,
                            first=tree(ids[:middle]), second=tree(ids[middle:]))
            return dict(type="layout_export", layout=dict(workspace_id="w1", tab_id="w1:t1",
                focused_pane_id="w1:p1", root=tree([f"w1:p{i}" for i in range(1,self.panes+1)])))
        if method == "pane.process_info":
            return dict(type="pane_process_info", process_info=dict(
                pane_id=request["params"]["pane_id"], shell_pid=self.pid,
                foreground_process_group_id=os.tcgetpgrp(self.master),
                foreground_processes=[dict(pid=os.tcgetpgrp(self.master))]))
        if method == "workspace.create":
            self.mutations.append(request)
            return dict(type="workspace_created", workspace=dict(workspace_id="w2"),tab=dict(tab_id="w2:t1"))
        if method == "pane.send_input":
            self.mutations.append(request)
            if self.before_run:
                self.before_run()
            os.write(self.master, (request["params"]["text"] + "\n").encode())
            return dict(type="ok")
        raise AssertionError(f"Unexpected API method {method}")

    def serve(self):
        while not self.stopping.is_set():
            try:
                stream, _ = self.listener.accept()
            except socket.timeout:
                continue
            with stream:
                try:
                    stream.settimeout(2)
                    data = b""
                    while not data.endswith(b"\n"):
                        chunk = stream.recv(65536)
                        if not chunk:
                            break
                        data += chunk
                    if not data:
                        continue
                    request = json.loads(data)
                    result = self.result(request)
                    if self.drop_mutation_reply and request["method"] in ("pane.send_input", "workspace.create"):
                        continue
                    stream.sendall(json.dumps(dict(id=request["id"], result=result)).encode() + b"\n")
                except Exception as error:
                    self.errors.append(error)

    def env(self):
        env = os.environ.copy()
        env.update(HERDR_PLUGIN_CONFIG_DIR=str(self.config),
                   HERDR_PLUGIN_STATE_DIR=str(self.state),
                   HERDR_PLUGIN_ID="cantona.herdr-revive",
                   HERDR_SOCKET_PATH=str(self.socket_path),
                   HERDR_BIN_PATH=shutil.which("herdr") or "/missing/herdr")
        env.pop("HERDR_SESSION", None)
        return env

    def run(self, *args, ok=True):
        result = subprocess.run([str(BINARY), *args], env=self.env(),
                                capture_output=True, text=True, timeout=10)
        if ok and result.returncode:
            raise AssertionError(result.stderr)
        if not ok and not result.returncode:
            raise AssertionError("expected failure")
        return json.loads(result.stdout) if ok else result

    def seed(self, argv=None):
        result = self.run("save")
        path = Path(result["result"]["path"])
        snapshot = json.loads(path.read_text())
        snapshot["panes"][0]["command"] = dict(kind="program", argv=argv or ["ssh", "fixture.invalid"])
        path.write_text(json.dumps(snapshot))
        return path

    def wait_argv(self):
        deadline = time.monotonic() + 3
        while time.monotonic() < deadline:
            path = self.root / "argv.json"
            if path.exists() and path.stat().st_size:
                return json.loads(path.read_text())
            time.sleep(0.01)
        raise AssertionError("fixture executable did not run")

    def close(self):
        self.stopping.set()
        self.thread.join(timeout=3)
        self.listener.close()
        os.close(self.master)
        try:
            os.kill(self.pid, signal.SIGHUP)
        except ProcessLookupError:
            pass
        self.shell.wait(timeout=3)
        self.temp.cleanup()
        if self.errors:
            raise AssertionError(self.errors)


class Integration(unittest.TestCase):
    def fixture(self, **kwargs):
        fixture = Fixture(**kwargs)
        self.addCleanup(fixture.close)
        return fixture

    def test_literal_ssh_and_minicom_through_both_transports(self):
        for transport in ("direct", "cli"):
            for program in ("ssh", "minicom"):
                with self.subTest(transport=transport, program=program):
                    f = self.fixture(transport=transport)
                    args = [program, "", "space here", "'\"$;|`", "中文", "$(touch WRONG)"]
                    f.seed(args)
                    preview = f.run("preview")
                    self.assertEqual(preview["result"]["plan"]["entries"][0]["decision"], "candidate")
                    self.assertEqual(f.mutations, [])
                    restored = f.run("restore")
                    self.assertEqual(restored["result"]["journal"]["entries"][0]["outcome"], "applied")
                    self.assertEqual(f.wait_argv()[1:], args[1:])
                    self.assertFalse(f.root.joinpath("WRONG").exists())
                    f.write_config(auto_restore=True)
                    self.assertEqual(f.run("event")["result"]["status"], "boot_done")
                    self.assertEqual(len(f.mutations), 1)

    def test_100_simultaneous_startup_event_autosave_claims(self):
        f = self.fixture()
        f.seed()
        f.write_config(auto_restore=True, auto_save=True)
        with concurrent.futures.ThreadPoolExecutor(max_workers=20) as pool:
            results = list(pool.map(lambda i: f.run("event" if i % 2 else "autosave"), range(100)))
        self.assertEqual(len(results), 100)
        self.assertEqual(len(f.mutations), 1)

    def test_ambiguous_delivery_requires_acknowledgement_without_retry(self):
        f = self.fixture()
        saved = f.seed()
        before = saved.read_bytes()
        f.drop_mutation_reply = True
        f.run("restore", ok=False)
        self.assertEqual(f.wait_argv()[1:], ["fixture.invalid"])
        f.run("restore", ok=False)
        f.run("save", ok=False)
        self.assertEqual(saved.read_bytes(), before)
        evidence = f.run("recovery", "inspect")["result"]["pending"]
        self.assertEqual(evidence["state"], "failed")
        f.run("recovery", "acknowledge", evidence["generation"])
        f.write_config(auto_restore=True)
        self.assertEqual(f.run("event")["result"]["status"], "boot_done")
        self.assertEqual(len(f.mutations), 1)

    def test_ambiguous_workspace_creation_blocks_retries_and_preserves_evidence(self):
        f = self.fixture()
        f.run("space", "save", "template", "--workspace", "w1")
        f.drop_mutation_reply = True
        f.run("space", "open", "template", ok=False)
        self.assertEqual(len(f.mutations), 1)
        f.run("space", "open", "template", ok=False)
        f.run("save", ok=False)
        pending = f.run("recovery", "inspect")["result"]["rebuild"]
        self.assertEqual(pending["steps"][0]["outcome"], "sending")
        f.run("recovery", "acknowledge", pending["operation"])
        self.assertEqual(len(f.mutations), 1)
        self.assertIsNone(f.run("recovery", "inspect")["result"]["rebuild"])

    def test_manual_restore_is_repeatable_but_forced_autosave_never_restores(self):
        f = self.fixture()
        f.seed()
        for _ in range(2):
            f.run("restore", "--rehydrate")
            f.wait_output(b"FIXTURE_READY>")
        self.assertEqual(len(f.mutations), 2)
        other = self.fixture()
        other.seed()
        other.write_config(auto_restore=True)
        self.assertEqual(other.run("autosave", "--force")["result"]["status"], "saved")
        self.assertEqual(other.mutations, [])

    def test_busy_foreground_and_background_children_are_skipped(self):
        for command in ("sleep 5", "sleep 5 &"):
            with self.subTest(command=command):
                f = self.fixture()
                f.seed()
                os.write(f.master, (command + "\n").encode())
                time.sleep(0.1)
                result = f.run("restore")
                self.assertEqual(result["result"]["journal"]["entries"][0]["outcome"], "skipped")
                self.assertEqual(f.mutations, [])

    def test_allowlist_is_rechecked_after_snapshot_write(self):
        f = self.fixture()
        f.seed()
        f.write_config(allowed_programs=[])
        self.assertEqual(f.run("preview")["result"]["plan"]["entries"][0]["decision"], "denied_by_policy")
        f.run("restore")
        self.assertEqual(f.mutations, [])

    def test_layout_preview_and_capture_request_counts(self):
        for count in (1, 10, 50, 100):
            with self.subTest(panes=count):
                f = self.fixture(panes=count)
                saved = f.run("save")
                self.assertEqual(saved["requests"], count + 3)
                self.assertEqual(saved["herdr_children"], 0)
                f.requests.clear()
                preview = f.run("preview")
                self.assertEqual(preview["requests"], 2)
                self.assertEqual(f.requests, ["ping", "session.snapshot"])

    def test_native_named_space_import_never_executes(self):
        source = self.fixture()
        result = source.run("space", "save", "work", "--workspace", "w1")
        path = Path(result["result"]["path"])
        target = self.fixture()
        mapping = target.root / "mapping.json"
        mapping.write_text(json.dumps([dict(source_pane_id="w1:p1", workspace_id="w1", tab_id="w1:t1", pane_id="w1:p1")]))
        target.run("space", "import", "copied", str(path), "--mapping", str(mapping))
        self.assertEqual(target.mutations, [])
        target.run("space", "preview", "copied")
        target.run("space", "delete", "copied")
        self.assertEqual(target.run("space", "list")["result"]["spaces"], [])

    def test_named_library_is_shared_across_session_ids(self):
        source = self.fixture()
        source.run("space","save","shared","--workspace","w1")
        target = self.fixture()
        target.state = source.state
        preview = target.run("space","preview","shared")
        self.assertEqual(preview["result"]["mode"],"recreate")
        self.assertEqual(len(target.run("space","list")["result"]["spaces"]),1)
        target.run("space","delete","shared")
        self.assertEqual(source.run("space","list")["result"]["spaces"],[])

    def test_five_exact_agent_resume_vectors_reach_fixture_executables(self):
        session = "01234567-89ab-cdef-0123-456789abcdef"
        for agent, executable in [("claude","claude"),("codex","codex"),("gemini","gemini"),("copilot","copilot"),("cursor","cursor-agent")]:
            with self.subTest(agent=agent):
                f = self.fixture()
                path = f.seed()
                data = json.loads(path.read_text())
                data["panes"][0]["command"] = dict(kind="agent",agent=agent,executable=executable,session_id=session)
                path.write_text(json.dumps(data))
                f.run("restore","--rehydrate")
                self.assertEqual(f.wait_argv()[1:],["resume" if agent=="codex" else "--resume",session])

    def test_wrong_session_full_snapshot_is_rejected(self):
        source = self.fixture()
        path = source.seed()
        target = self.fixture()
        target.run("restore", "--snapshot", str(path), ok=False)
        self.assertEqual(target.mutations, [])

    def test_exec_wrapper_profile_survives_capture_and_restore(self):
        f = self.fixture()
        session = "01234567-89ab-cdef-0123-456789abcdef"
        original_pane = f.pane
        def agent_pane(i=1):
            pane = original_pane(i)
            if os.tcgetpgrp(f.master) != f.pid:
                pane.update(agent="claude", agent_session=dict(source="herdr:claude", agent="claude", kind="id", value=session))
            return pane
        f.pane = agent_pane
        shutil.copy2(f.bin_dir / "claude", f.bin_dir / "claude-local")
        f.write_config(allowed_programs=["claude", "claude-local"])
        with (f.config / "config.toml").open("a") as config:
            config.write('\n[[agent_launchers]]\nagent = "claude"\nexecutable = "claude-local"\nmatch_env = { CLAUDE_CONFIG_DIR = "/fixture/local" }\n')
        os.write(f.master, b"CLAUDE_CONFIG_DIR=/fixture/local SECRET=never-save bash -c 'exec -a claude /usr/bin/sleep 60'\n")
        deadline = time.monotonic() + 3
        while os.tcgetpgrp(f.master) == f.pid and time.monotonic() < deadline:
            time.sleep(0.01)
        time.sleep(0.05)
        path = Path(f.run("save")["result"]["path"])
        content = path.read_text()
        command = json.loads(content)["panes"][0]["command"]
        self.assertEqual(command, dict(kind="agent", agent="claude", executable="claude-local", session_id=session))
        self.assertNotIn("never-save", content)
        self.assertNotIn("/fixture/local", content)
        os.write(f.master, b"\x03")
        f.wait_output(b"FIXTURE_READY>")
        f.run("restore", "--rehydrate")
        self.assertEqual(f.wait_argv(), [str(f.bin_dir / "claude-local"), "--resume", session])

    def test_renamed_installation_keeps_completed_boot_claim(self):
        f = self.fixture()
        f.seed()
        f.run("restore")
        f.wait_argv()
        for path in f.state.rglob("*.json"):
            data = json.loads(path.read_text())
            if "tool" in data:
                data["tool"] = "herde-revive"
                path.write_text(json.dumps(data))
        copied = f.root / "renamed-state"
        shutil.copytree(f.state, copied)
        f.state = copied
        f.write_config(auto_restore=True)
        self.assertEqual(f.run("event")["result"]["status"], "boot_done")
        self.assertEqual(len(f.mutations), 1)

    def test_socket_metadata_change_does_not_replay_same_server_boot(self):
        f = self.fixture()
        f.seed()
        f.run("restore")
        f.wait_argv()
        f.socket_path.chmod(0o600)
        f.write_config(auto_restore=True)
        self.assertEqual(f.run("event")["result"]["status"], "boot_done")
        self.assertEqual(len(f.mutations), 1)

    def test_idle_check_run_gap_is_observable(self):
        f = self.fixture()
        f.seed()
        def make_busy():
            os.write(f.master, b"sleep 0.2\n")
            time.sleep(0.05)
        f.before_run = make_busy
        f.run("restore")
        self.assertEqual(len(f.mutations), 1)
        self.assertNotEqual(os.tcgetpgrp(f.master), f.pid)
        # The request is still accepted: Herdr 0.9.1 has no conditional-run API.

    def test_shell_readiness_cannot_be_inferred_from_process_idleness(self):
        f = self.fixture()
        f.seed()
        os.write(f.master, b"read -r value\n")
        time.sleep(0.05)
        result = f.run("restore")
        self.assertEqual(result["result"]["journal"]["entries"][0]["outcome"], "applied")
        self.assertFalse(f.root.joinpath("argv.json").exists())
        # Applied means input accepted, not command completion or prompt readiness.

    def test_pipeline_capture_preserves_previous_snapshot(self):
        f = self.fixture()
        path = f.seed()
        before = path.read_bytes()
        os.write(f.master, b"sleep 5 | cat\n")
        time.sleep(0.1)
        f.run("save", ok=False)
        self.assertEqual(path.read_bytes(), before)

    def test_redirected_command_capture_preserves_previous_snapshot(self):
        f = self.fixture()
        path = f.seed()
        before = path.read_bytes()
        os.write(f.master, b"sleep 5 > redirected-output\n")
        time.sleep(0.1)
        f.run("save", ok=False)
        self.assertEqual(path.read_bytes(), before)

    def test_null_streams_round_trip_without_changing_program_policy(self):
        f = self.fixture()
        f.write_config(allowed_programs=["sleep"])
        os.write(f.master, b"sleep 30 >/dev/null 2>/dev/null\n")
        time.sleep(0.1)
        path = Path(f.run("save")["result"]["path"])
        command = json.loads(path.read_text())["panes"][0]["command"]
        self.assertEqual(command, dict(kind="program_null_stdio", argv=["sleep", "30"],
                                       null_stdio=[False, True, True]))
        os.write(f.master, b"\x03")
        f.wait_output(b"FIXTURE_READY>")
        f.write_config(allowed_programs=["/bin/sh"])
        f.run("restore")
        self.assertEqual(len(f.mutations), 0)
        f.write_config(allowed_programs=["sleep"])
        f.run("restore")
        deadline = time.monotonic() + 3
        while time.monotonic() < deadline:
            pid = os.tcgetpgrp(f.master)
            if pid != f.pid and Path(f"/proc/{pid}/comm").read_text().strip() == "sleep":
                break
            time.sleep(0.02)
        self.assertNotEqual(pid, f.pid)
        self.assertEqual([os.readlink(f"/proc/{pid}/fd/{fd}") for fd in (1, 2)], ["/dev/null"] * 2)
        self.assertEqual(os.readlink(f"/proc/{pid}/fd/0"), os.readlink(f"/proc/{f.pid}/fd/0"))

    def test_null_stdin_capture_preserves_previous_snapshot(self):
        f = self.fixture()
        path = f.seed()
        before = path.read_bytes()
        os.write(f.master, b"sleep 5 </dev/null\n")
        time.sleep(0.1)
        f.run("save", ok=False)
        self.assertEqual(path.read_bytes(), before)


if __name__ == "__main__":
    unittest.main(verbosity=2)
