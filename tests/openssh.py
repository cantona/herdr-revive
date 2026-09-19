#!/usr/bin/env python3
"""Real OpenSSH round trips using disposable localhost keys, server and PTYs."""
import getpass
import json
import os
from pathlib import Path
import shlex
import shutil
import signal
import socket
import subprocess
import time
import unittest

from integration import Fixture
from process_inspect import fd_target


@unittest.skipUnless(Path("/usr/bin/ssh").exists() and shutil.which("sshd"), "OpenSSH client/server required")
class OpenSsh(unittest.TestCase):
    def setUp(self):
        self.fixture = Fixture()
        self.addCleanup(self.fixture.close)
        root = self.fixture.root
        for name in ("hostkey", "clientkey"):
            subprocess.run(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-f", str(root / name)], check=True)
        with socket.socket() as listener:
            listener.bind(("127.0.0.1", 0))
            port = listener.getsockname()[1]
        (root / "known_hosts").write_text(f"[127.0.0.1]:{port} " + (root / "hostkey.pub").read_text())
        (root / "sshd.conf").write_text(
            f"Port {port}\nListenAddress 127.0.0.1\nHostKey {root}/hostkey\n"
            f"AuthorizedKeysFile {root}/clientkey.pub\nPidFile {root}/sshd.pid\n"
            "StrictModes no\nUsePAM no\nPasswordAuthentication no\nKbdInteractiveAuthentication no\n")
        server = subprocess.Popen([shutil.which("sshd"), "-D", "-f", str(root / "sshd.conf"), "-E", str(root / "sshd.log")])
        self.addCleanup(self.stop_server, server)
        deadline = time.monotonic() + 5
        while True:
            try:
                with socket.create_connection(("127.0.0.1", port), timeout=0.2):
                    break
            except OSError:
                if server.poll() is not None or time.monotonic() > deadline:
                    self.fail((root / "sshd.log").read_text())
                time.sleep(0.02)
        self.argv = ["/usr/bin/ssh", "-F", "/dev/null", "-i", str(root / "clientkey"), "-p", str(port),
                     "-o", "IdentitiesOnly=yes", "-o", "UserKnownHostsFile=" + str(root / "known_hosts"),
                     "-o", "StrictHostKeyChecking=yes", "-o", "ControlMaster=no", "-o", "ControlPath=none",
                     "-o", "BatchMode=yes", "-T", getpass.getuser() + "@127.0.0.1",
                     "printf 'SSH_OUTPUT_READY\\n'; printf 'SSH_ERROR_READY\\n' >&2; sleep 30"]
        self.fixture.write_config(allowed_programs=["/usr/bin/ssh"])

    def stop_server(self, server):
        server.terminate()
        server.wait(timeout=5)

    def launch(self, suffix=""):
        f = self.fixture
        os.write(f.master, (shlex.join(self.argv) + suffix + "\n").encode())
        f.wait_output(b"SSH_ERROR_READY\r\n")
        pid = os.tcgetpgrp(f.master)
        self.assertEqual(fd_target(pid, 1), "/dev/null")
        return pid

    def test_session_stdout_is_recaptured_and_restored_to_terminal(self):
        f = self.fixture
        pid = self.launch()
        path = Path(f.run("save")["result"]["path"])
        self.assertEqual(json.loads(path.read_text())["panes"][0]["command"], dict(kind="program", argv=self.argv))
        os.kill(pid, signal.SIGTERM)
        f.wait_output(b"FIXTURE_READY>")
        f.run("restore")
        f.wait_output(b"SSH_OUTPUT_READY\r\n")
        path = Path(f.run("save")["result"]["path"])
        self.assertEqual(json.loads(path.read_text())["panes"][0]["command"], dict(kind="program", argv=self.argv))

    def test_interactive_ssh_stdout_survives_restore(self):
        self.argv[self.argv.index("-T")] = "-tt"
        self.test_session_stdout_is_recaptured_and_restored_to_terminal()

    def test_real_null_session_stdout_refuses_capture_and_preserves_snapshot(self):
        f = self.fixture
        path = Path(f.run("save")["result"]["path"])
        before = path.read_bytes()
        self.launch(" >/dev/null")
        result = f.run("save", ok=False)
        self.assertIn("SSH session output is redirected", result.stderr)
        self.assertEqual(path.read_bytes(), before)

    def test_real_file_session_stdout_refuses_capture_and_preserves_snapshot(self):
        f = self.fixture
        path = Path(f.run("save")["result"]["path"])
        before = path.read_bytes()
        self.launch(" >redirected-output")
        result = f.run("save", ok=False)
        self.assertIn("SSH session output is redirected", result.stderr)
        self.assertEqual(path.read_bytes(), before)


if __name__ == "__main__":
    unittest.main(verbosity=2)
