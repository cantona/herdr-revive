#!/usr/bin/env python3
"""Replacement behavior against a private real Herdr server and harmless programs."""
import json
import os
from pathlib import Path
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import time
import unittest

from process_inspect import argv as process_argv, environment, executable_name, fd_target

ROOT = Path(__file__).resolve().parents[1]
BIN = Path(os.environ.get("REVIVE_BINARY", ROOT / "target/release/herdr-revive"))
HERDR = shutil.which("herdr")


class RealHost(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="herdr-revive-host-", dir="/tmp")
        self.root = Path(self.temp.name).resolve()
        self.env = {k:v for k,v in os.environ.items() if not k.startswith("HERDR_")}
        self.env.update(XDG_CONFIG_HOME=str(self.root/"config"), XDG_STATE_HOME=str(self.root/"state"),
            HERDR_CONFIG_PATH=str(self.root/"config/herdr/config.toml"),
            HERDR_SOCKET_PATH=str(self.root/"host.sock"), HERDR_BIN_PATH=HERDR,
            HERDR_PLUGIN_CONFIG_DIR=str(self.root/"plugin-config"),
            HERDR_PLUGIN_STATE_DIR=str(self.root/"plugin-state"), HERDR_PLUGIN_ID="cantona.herdr-revive",
            TERM="xterm-256color")
        shell = self.root/"fixture-shell"
        shell.write_text('#!/bin/sh\nexec /bin/bash --noprofile --norc "$@"\n')
        shell.chmod(0o700)
        self.env["SHELL"] = str(shell)
        self.config = self.root/"plugin-config/config.toml"
        self.config.parent.mkdir()
        self.config.write_text('settle_ms = 0\nallowed_programs = ["sleep", "/bin/sleep"]\n')
        self.log = open(self.root/"server.log", "w")
        self.server = subprocess.Popen([HERDR,"server"],env=self.env,cwd=self.root,stdout=self.log,stderr=self.log)
        deadline = time.monotonic()+10
        while not Path(self.env["HERDR_SOCKET_PATH"]).exists():
            if self.server.poll() is not None or time.monotonic()>deadline:
                self.fail((self.root/"server.log").read_text())
            time.sleep(0.05)

    def tearDown(self):
        self.server.terminate()
        try:
            self.server.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self.server.kill()
            self.server.wait(timeout=5)
        self.log.close()
        self.temp.cleanup()

    def api(self, method, params=None):
        with socket.socket(socket.AF_UNIX) as connection:
            connection.settimeout(3)
            connection.connect(self.env["HERDR_SOCKET_PATH"])
            connection.sendall(json.dumps(dict(id="fixture",method=method,params=params or {})).encode()+b"\n")
            reply = json.loads(connection.makefile("rb").readline())
            if "error" in reply:
                self.fail(reply)
            return reply["result"]

    def run_cli(self,*args,ok=True):
        result = subprocess.run([str(BIN),*args],env=self.env,capture_output=True,text=True,timeout=15)
        self.assertEqual(result.returncode==0,ok,result.stderr)
        return json.loads(result.stdout) if ok else result

    def source(self):
        created = self.api("workspace.create",dict(label="Saved workspace",cwd=str(self.root),focus=False))
        workspace = created["workspace"]["workspace_id"]
        def leaf(label):
            return dict(type="pane",label=label,cwd=str(self.root),command=["/bin/sleep","60"])
        root = dict(type="split",direction="right",ratio=0.65,first=leaf("left"),second=dict(
            type="split",direction="down",ratio=0.3,first=leaf("top"),second=leaf("bottom")))
        self.api("layout.apply",dict(tab_id=created["tab"]["tab_id"],tab_label="Nested",focus=False,root=root))
        self.api("layout.apply",dict(workspace_id=workspace,tab_label="Second",focus=False,root=leaf("only")))
        time.sleep(0.1)
        return workspace

    def test_monitor_and_log_commands_rehydrate_and_recapture(self):
        self.config.write_text('settle_ms = 100\nmatch_program_basename = true\nallowed_programs = ["top", "htop", "journalctl", "tail"]\n')
        logfile = self.root / "fixture.log"
        logfile.write_text("revive monitor fixture\n")
        commands = [["tail", "-f", str(logfile)]]
        if sys.platform != "darwin":
            commands += [["top"], ["htop"], ["journalctl", "-f", "-n", "0"]]
        for argv in commands:
            with self.subTest(program=argv[0]):
                created = self.api("workspace.create", dict(label=argv[0], cwd=str(self.root), focus=False))
                pane = created["root_pane"]["pane_id"]

                def wait_foreground(program):
                    deadline = time.monotonic() + 5
                    while time.monotonic() < deadline:
                        info = self.api("pane.process_info", dict(pane_id=pane))["process_info"]
                        pid = info.get("foreground_process_group_id")
                        try:
                            executable = executable_name(pid)
                            if executable == program:
                                return pid
                        except (FileNotFoundError, subprocess.CalledProcessError):
                            pass
                        time.sleep(0.05)
                    self.fail(f"{program} did not become foreground: {info}")

                import shlex
                self.api("pane.send_input", dict(pane_id=pane, text=shlex.join(argv), keys=["Enter"]))
                first = wait_foreground(argv[0])
                saved_path = Path(self.run_cli("save")["result"]["path"])
                saved = json.loads(saved_path.read_text())
                command = next(p["command"] for p in saved["panes"] if p["pane_id"] == pane)
                self.assertEqual(command["argv"], argv)
                self.assertIn(command["kind"], ["program", "program_null_stdio"])
                os.kill(first, signal.SIGTERM)
                wait_foreground("bash")
                result = self.run_cli("restore", "--rehydrate")
                entry = next(e for e in result["result"]["journal"]["entries"] if e["pane_id"] == pane)
                self.assertEqual(entry["outcome"], "applied")
                restored = wait_foreground(argv[0])
                self.assertNotEqual(first, restored)
                time.sleep(0.2)
                self.assertEqual(wait_foreground(argv[0]), restored)
                recaptured = json.loads(Path(self.run_cli("save")["result"]["path"]).read_text())
                self.assertEqual(next(p["command"] for p in recaptured["panes"] if p["pane_id"] == pane), command)
                self.api("workspace.close", dict(workspace_id=created["workspace"]["workspace_id"]))

    @unittest.skipUnless(sys.platform == "darwin", "macOS protected-process restriction")
    def test_protected_process_capture_preserves_previous_snapshot(self):
        created = self.api("workspace.create", dict(label="Protected", cwd=str(self.root), focus=False))
        pane = created["root_pane"]["pane_id"]
        time.sleep(0.1)
        path = Path(self.run_cli("save")["result"]["path"])
        before = path.read_bytes()
        # macOS top is setuid root; Herdr cannot report its foreground group.
        self.api("pane.send_input", dict(pane_id=pane, text="/usr/bin/top", keys=["Enter"]))
        time.sleep(0.2)
        self.run_cli("save", ok=False)
        self.assertEqual(path.read_bytes(), before)

    def test_git_internal_pager_restores_but_external_pipe_is_refused(self):
        self.config.write_text('settle_ms = 100\nmatch_program_basename = true\nallowed_programs = ["git"]\n')
        subprocess.run(["git", "init", "-q", str(self.root)], check=True)
        subprocess.run(["git", "-C", str(self.root), "-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid",
                        "commit", "-q", "--allow-empty", "-m", "revive pager fixture"], check=True)
        created = self.api("workspace.create", dict(label="Git pager", cwd=str(self.root), focus=False))
        pane = created["root_pane"]["pane_id"]
        # A pager command with arguments makes Git use a system-shell wrapper.
        argv = ["git", "log", "--oneline"]
        if sys.platform == "darwin":
            argv[0] = subprocess.check_output(["/usr/bin/xcrun", "--find", "git"], text=True).strip()
        import shlex
        self.api("pane.send_input", dict(
            pane_id=pane, text="GIT_PAGER='less -R' LESS=-R "+shlex.join(argv), keys=["Enter"]))

        def wait_process(name):
            deadline = time.monotonic() + 5
            while time.monotonic() < deadline:
                info = self.api("pane.process_info", dict(pane_id=pane))["process_info"]
                if any(p.get("name") == name for p in info["foreground_processes"]):
                    return info
                time.sleep(0.05)
            self.fail(f"{name} did not start: {info}")

        wait_process("less")
        if sys.platform != "darwin":
            wait_process("sh")
        path = Path(self.run_cli("save")["result"]["path"])
        command = json.loads(path.read_text())["panes"][0]["command"]
        self.assertEqual(command, dict(kind="program", argv=argv))
        self.api("pane.send_input", dict(pane_id=pane, text="q"))
        wait_process("bash")
        self.api("pane.send_input", dict(
            pane_id=pane, text="GIT_PAGER='less -R; true' LESS=-R "+shlex.join(argv), keys=["Enter"]))
        wait_process("less")
        wait_process("bash" if sys.platform == "darwin" else "sh")
        before = path.read_bytes()
        self.run_cli("save", ok=False)
        self.assertEqual(path.read_bytes(), before)
        self.api("pane.send_input", dict(pane_id=pane, text="q"))
        wait_process("bash")
        # LESS belongs to the user environment and must be present at relaunch too.
        self.api("pane.send_input", dict(pane_id=pane, text="export GIT_PAGER=less LESS=", keys=["Enter"]))
        time.sleep(0.05)
        self.run_cli("restore", "--rehydrate")
        wait_process("less")
        self.assertEqual(json.loads(Path(self.run_cli("save")["result"]["path"]).read_text())["panes"][0]["command"], command)
        self.api("pane.send_input", dict(pane_id=pane, text="q"))
        wait_process("bash")
        large = self.root / "large.txt"
        large.write_text("pager output " * 10 + "\n")
        large.write_text(large.read_text() * 20000)
        active = [argv[0], "diff", "--no-index", "/dev/null", str(large)]
        self.api("pane.send_input", dict(pane_id=pane, text=shlex.join(active), keys=["Enter"]))
        info = wait_process("less")
        git_pid = next(p["pid"] for p in info["foreground_processes"] if p["name"] == "git")
        self.assertTrue(fd_target(git_pid, 1).startswith("pipe:"))
        active_path = Path(self.run_cli("save")["result"]["path"])
        self.assertEqual(json.loads(active_path.read_text())["panes"][0]["command"], dict(kind="program", argv=active))
        self.api("pane.send_input", dict(pane_id=pane, text="q"))
        wait_process("bash")
        self.api("pane.send_input", dict(pane_id=pane, text="git log --oneline | less", keys=["Enter"]))
        wait_process("less")
        before = path.read_bytes()
        self.run_cli("save", ok=False)
        self.assertEqual(path.read_bytes(), before)
        self.api("pane.send_input", dict(pane_id=pane, text="q"))
        wait_process("bash")
        self.api("pane.send_input", dict(pane_id=pane, text="git log --oneline 2> redirected-error", keys=["Enter"]))
        wait_process("less")
        self.run_cli("save", ok=False)
        self.assertEqual(path.read_bytes(), before)

    def normalized(self,workspace):
        snapshot = self.api("session.snapshot")["snapshot"]
        tabs = [t for t in snapshot["tabs"] if t["workspace_id"]==workspace]
        def tree(node):
            if node["type"]=="pane":
                return dict(type="pane",label=node.get("label"),cwd=node.get("cwd"))
            return dict(type="split",direction=node["direction"],ratio=round(node["ratio"],4),
                        first=tree(node["first"]),second=tree(node["second"]))
        return [(t["label"],tree(self.api("layout.export",dict(tab_id=t["tab_id"]))["layout"]["root"])) for t in tabs]

    def test_named_space_opens_twice_with_nested_layout_order_labels_and_commands(self):
        source = self.source()
        saved = self.run_cli("space","save","reusable","--workspace",source)
        snapshot = json.loads(Path(saved["result"]["path"]).read_text())
        self.assertEqual(len(snapshot["panes"]),4)
        self.assertTrue(all(p["command"] is not None for p in snapshot["panes"]))
        expected = self.normalized(source)
        before = len(self.api("session.snapshot")["snapshot"]["workspaces"])
        preview = self.run_cli("space","open","reusable","--dry-run")
        self.assertEqual(preview["result"]["mode"],"recreate")
        self.assertEqual(len(self.api("session.snapshot")["snapshot"]["workspaces"]),before)
        copies=[]
        for _ in range(2):
            result = self.run_cli("space","open","reusable","--no-focus")
            workspace = result["result"]["workspaces"][0]
            copies.append(workspace)
            self.assertEqual(self.normalized(workspace),expected)
            panes=[p for p in self.api("session.snapshot")["snapshot"]["panes"] if p["workspace_id"]==workspace]
            for pane in panes:
                process=self.api("pane.process_info",dict(pane_id=pane["pane_id"]))["process_info"]
                argv=process["foreground_processes"][0]["argv"]
                self.assertEqual(argv,["/bin/sleep","60"])
        self.assertNotEqual(copies[0],copies[1])
        self.assertNotIn(source,copies)

    def test_null_streams_survive_native_layout_reconstruction_and_recapture(self):
        created = self.api("workspace.create", dict(label="Null stdout", cwd=str(self.root), focus=False))
        pane = created["root_pane"]["pane_id"]
        self.api("pane.send_input", dict(pane_id=pane, text="/bin/sleep 60 >/dev/null 2>/dev/null", keys=["Enter"]))
        time.sleep(0.2)
        self.run_cli("space", "save", "null-output", "--workspace", created["workspace"]["workspace_id"])
        result = self.run_cli("space", "open", "null-output", "--no-focus")
        workspace = result["result"]["workspaces"][0]
        restored = next(p for p in self.api("session.snapshot")["snapshot"]["panes"] if p["workspace_id"] == workspace)
        info = self.api("pane.process_info", dict(pane_id=restored["pane_id"]))["process_info"]
        pid = info["foreground_process_group_id"]
        self.assertEqual(process_argv(pid), [b"/bin/sleep", b"60"])
        self.assertEqual([fd_target(pid, fd) for fd in (1, 2)], ["/dev/null"] * 2)
        path = self.run_cli("space", "save", "recaptured", "--workspace", workspace)["result"]["path"]
        command = json.loads(Path(path).read_text())["panes"][0]["command"]
        self.assertEqual(command, dict(kind="program_null_stdio", argv=["/bin/sleep", "60"], null_stdio=[False, True, True]))

    def test_explicit_recreate_uses_new_workspace_and_preserves_source(self):
        source=self.source()
        self.run_cli("save")
        before=self.normalized(source)
        preview=self.run_cli("restore","--recreate","--dry-run")
        self.assertTrue(preview["result"]["creates_new_workspaces"])
        result=self.run_cli("restore","--recreate")
        self.assertTrue(result["result"]["workspaces"])
        self.assertEqual(self.normalized(source),before)

    def test_default_restore_recreates_missing_workspace_but_rehydrate_does_not(self):
        source = self.source()
        expected = self.normalized(source)
        saved = self.run_cli("save")["result"]["path"]
        self.api("workspace.close", dict(workspace_id=source))
        self.assertIsNotNone(self.run_cli("preview")["result"]["reconstruction"])
        result = self.run_cli("restore", "--rehydrate", "--file", saved)
        self.assertNotIn("reconstruction", result["result"])
        result = self.run_cli("restore", "--file", saved)
        target = result["result"]["reconstruction"]["workspaces"][0]
        self.assertEqual(self.normalized(target), expected)

    def test_large_deep_layout_reconstructs_beyond_bulk_api_limits(self):
        created = self.api("workspace.create",dict(label="Large",cwd=str(self.root),focus=False))
        workspace = created["workspace"]["workspace_id"]
        pane = created["root_pane"]["pane_id"]
        for _ in range(24):
            result = self.api("pane.split",dict(target_pane_id=pane,direction="right",ratio=0.6,cwd=str(self.root),focus=False))
            pane = result["pane"]["pane_id"]
        time.sleep(0.15)
        expected = self.normalized(workspace)
        saved = self.run_cli("space","save","large","--workspace",workspace)
        path = Path(saved["result"]["path"])
        data = json.loads(path.read_text())
        data["panes"][-1]["command"] = dict(kind="program",argv=["/bin/sleep","60"])
        path.write_text(json.dumps(data))
        focus = self.api("session.snapshot")["snapshot"]["focused_pane_id"]
        result = self.run_cli("space","open","large","--no-focus")
        target = result["result"]["workspaces"][0]
        self.assertEqual(self.normalized(target),expected)
        live = self.api("session.snapshot")["snapshot"]
        self.assertEqual(live["focused_pane_id"],focus)
        found = False
        for pane in [p for p in live["panes"] if p["workspace_id"] == target]:
            info = self.api("pane.process_info",dict(pane_id=pane["pane_id"]))["process_info"]
            for process in info["foreground_processes"]:
                if process.get("argv") == ["/bin/sleep","60"]:
                    # SIP hides system sleep's environment on macOS. The
                    # custom-agent test below checks readable native env there.
                    if sys.platform != "darwin":
                        env = environment(process['pid'])
                        self.assertIn(f"HERDR_TAB_ID={pane['tab_id']}".encode(),env)
                    found = True
        self.assertTrue(found)
        journal_path = next((self.root/"plugin-state").glob(f"*/operations/{result['result']['operation']}.json"))
        journal = json.loads(journal_path.read_text())
        splits = [step for step in journal["steps"] if step["method"] == "pane.split"]
        self.assertTrue(all(step["created_pane_id"] and step["tab_id"] for step in splits))
        sent = [step for step in journal["steps"] if step["method"] == "pane.send_input"]
        self.assertEqual(len(sent),1)
        self.assertTrue(sent[0]["target_pane_id"])
        self.assertTrue(sent[0]["target_terminal_id"])

    def test_saved_active_tab_and_focused_pane_are_restored(self):
        workspace = self.source()
        source = self.api("session.snapshot")["snapshot"]
        pane = next(p for p in source["panes"] if p["workspace_id"] == workspace and p.get("label") == "top")
        self.api("pane.focus",dict(pane_id=pane["pane_id"]))
        self.run_cli("space","save","focused","--workspace",workspace)
        result = self.run_cli("space","open","focused")
        target = result["result"]["workspaces"][0]
        live = self.api("session.snapshot")["snapshot"]
        focused = next(p for p in live["panes"] if p["pane_id"] == live["focused_pane_id"])
        self.assertEqual(focused["workspace_id"],target)
        self.assertEqual(focused["label"],"top")
        self.assertEqual(next(t["label"] for t in live["tabs"] if t["tab_id"] == focused["tab_id"]),"Nested")

    def test_policy_change_rebuilds_shells_without_denied_programs(self):
        workspace = self.source()
        self.run_cli("space","save","denied","--workspace",workspace)
        self.config.write_text('allowed_programs = []\nsettle_ms = 0\n')
        result = self.run_cli("space","open","denied","--no-focus")
        target = result["result"]["workspaces"][0]
        live = self.api("session.snapshot")["snapshot"]
        time.sleep(0.1)
        for pane in [p for p in live["panes"] if p["workspace_id"] == target]:
            info = self.api("pane.process_info",dict(pane_id=pane["pane_id"]))["process_info"]
            self.assertEqual(info["shell_pid"], info["foreground_process_group_id"])
            self.assertTrue(all("sleep" not in p.get("argv",[]) for p in info["foreground_processes"]))

    def test_restart_restores_non_agent_without_agent_event(self):
        created = self.api("workspace.create",dict(label="Restart",cwd=str(self.root),focus=False))
        pane = created["root_pane"]["pane_id"]
        time.sleep(0.1)
        self.api("pane.send_input",dict(pane_id=pane,text="/bin/sleep 60",keys=["Enter"]))
        time.sleep(0.1)
        self.run_cli("save")
        self.config.write_text('auto_restore = true\nsettle_ms = 100\nallowed_programs = ["/bin/sleep"]\n')
        time.sleep(0.3)
        self.server.terminate()
        self.server.wait(timeout=5)
        self.server = subprocess.Popen([HERDR,"server"],env=self.env,cwd=self.root,stdout=self.log,stderr=self.log)
        deadline = time.monotonic()+5
        while True:
            try:
                with socket.socket(socket.AF_UNIX) as connection:
                    connection.connect(self.env["HERDR_SOCKET_PATH"])
                break
            except OSError:
                self.assertLess(time.monotonic(),deadline)
                time.sleep(0.05)
        result = self.run_cli("event")
        self.assertEqual(result["result"]["journal"]["entries"][0]["outcome"],"applied")
        time.sleep(0.1)
        info = self.api("pane.process_info",dict(pane_id=pane))["process_info"]
        self.assertIn(["/bin/sleep","60"],[p["argv"] for p in info["foreground_processes"]])
        self.assertEqual(self.run_cli("event")["result"]["status"],"boot_done")

    def test_restart_preserves_custom_agent_launcher_without_native_resume(self):
        source = self.root / "agent.c"
        source.write_text(
            '#include <fcntl.h>\n#include <unistd.h>\n'
            'int main(void) { int fd = open("/dev/null", O_WRONLY); '
            'if (fd < 0 || dup2(fd, 2) < 0) return 1; if (fd != 2) close(fd); '
            'for (;;) pause(); }\n')
        bindir = self.root / "bin"
        bindir.mkdir()
        subprocess.run(["cc", str(source), "-o", str(bindir / "claude")], check=True)
        profile = self.root / "local-profile"
        wrapper = bindir / "claude-local"
        wrapper.write_text(f'#!/bin/sh\nexport CLAUDE_CONFIG_DIR="{profile}"\nexec "{bindir / "claude"}" "$@"\n')
        wrapper.chmod(0o700)
        self.config.write_text('auto_restore = true\nsettle_ms = 100\nmatch_program_basename = true\n'
            'allowed_programs = ["claude", "claude-local"]\n'
            '[[agent_launchers]]\nagent = "claude"\n'
            f'executable = "{wrapper}"\nmatch_env = {{ CLAUDE_CONFIG_DIR = "{profile}" }}\n')
        session = "01234567-89ab-cdef-0123-456789abcdef"
        created = self.api("workspace.create", dict(label="Custom agent", cwd=str(self.root), focus=False))
        pane = created["root_pane"]["pane_id"]
        self.api("pane.send_input", dict(pane_id=pane, text=f"{wrapper} --resume {session}", keys=["Enter"]))
        deadline = time.monotonic() + 5
        while True:
            info = self.api("pane.process_info", dict(pane_id=pane))["process_info"]
            processes = info["foreground_processes"]
            if processes and fd_target(processes[0]["pid"], 2) == "/dev/null":
                break
            self.assertLess(time.monotonic(), deadline, info)
            time.sleep(0.05)
        self.api("pane.report_agent_session", dict(pane_id=pane, source="herdr:claude", agent="claude", agent_session_id=session))
        saved = Path(self.run_cli("save")["result"]["path"])
        command = json.loads(saved.read_text())["panes"][0]["command"]
        self.assertEqual(command["executable"], str(wrapper))
        self.assertEqual(command["null_stdio"], [False, False, True])
        self.env["PATH"] = str(bindir) + ":" + self.env["PATH"]
        host_config = Path(self.env["HERDR_CONFIG_PATH"])
        host_config.write_text('[session]\nresume_agents_on_restore = true\n')
        time.sleep(0.3)
        self.server.terminate()
        self.server.wait(timeout=5)
        self.server = subprocess.Popen([HERDR,"server"], env=self.env, cwd=self.root, stdout=self.log, stderr=self.log)
        deadline = time.monotonic() + 10
        while True:
            try:
                info = self.api("pane.process_info", dict(pane_id=pane))["process_info"]
                canonical = [p for p in info["foreground_processes"] if p.get("argv") == ["claude", "--resume", session]]
                if canonical:
                    env = environment(canonical[0]['pid'])
                    self.assertFalse(any(value.startswith(b"CLAUDE_CONFIG_DIR=") for value in env))
                    break
            except (OSError, KeyError):
                pass
            except AssertionError as error:
                if "'code': 'pane_not_found'" not in str(error):
                    raise
            self.assertLess(time.monotonic(), deadline)
            time.sleep(0.05)
        host_config.write_text('[session]\nresume_agents_on_restore = false\n')
        time.sleep(0.3)
        self.server.terminate()
        self.server.wait(timeout=5)
        self.server = subprocess.Popen([HERDR,"server"], env=self.env, cwd=self.root, stdout=self.log, stderr=self.log)
        deadline = time.monotonic() + 5
        while True:
            try:
                with socket.socket(socket.AF_UNIX) as connection:
                    connection.connect(self.env["HERDR_SOCKET_PATH"])
                break
            except OSError:
                self.assertLess(time.monotonic(), deadline)
                time.sleep(0.05)
        result = self.run_cli("event")
        self.assertEqual(result["result"]["journal"]["entries"][0]["outcome"], "applied")
        deadline = time.monotonic() + 5
        while True:
            info = self.api("pane.process_info", dict(pane_id=pane))["process_info"]
            matches = [p for p in info["foreground_processes"] if p.get("argv") == [str(bindir / "claude"), "--resume", session]]
            if matches:
                pid = matches[0]["pid"]
                self.assertIn(f"CLAUDE_CONFIG_DIR={profile}".encode(), environment(pid))
                break
            self.assertLess(time.monotonic(), deadline, info)
            time.sleep(0.05)
        recaptured = Path(self.run_cli("save")["result"]["path"])
        command = json.loads(recaptured.read_text())["panes"][0]["command"]
        self.assertEqual(command["executable"], str(wrapper))
        self.assertEqual(command["null_stdio"], [False, False, True])
        self.assertEqual(self.run_cli("event")["result"]["status"], "boot_done")

    def test_timer_periodic_and_final_save(self):
        self.source()
        process=subprocess.Popen([str(BIN),"timer","--interval-seconds","1"],env=self.env,stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True)
        try:
            time.sleep(1.3)
            process.terminate()
            output,error=process.communicate(timeout=5)
            self.assertEqual(process.returncode,0,error)
            self.assertIn("timer_stopped",output)
            self.assertGreaterEqual(len(list((self.root/"plugin-state").glob("*/snapshots/*.json"))),3)
        finally:
            if process.poll() is None:
                process.kill();process.wait()


if __name__=="__main__":
    unittest.main(verbosity=2)
