#!/usr/bin/env python3
"""migrate between the fake TUI host of tests/integration.py, the fake core of
tests/core_integration.py and a stand-in herdr-app that records every call.
Nothing touches the real home, app, core or TUI."""
import fcntl
import json
import os
from pathlib import Path
import re
import shlex
import subprocess
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parent))
import core_integration  # noqa: E402
import integration  # noqa: E402

BINARY = integration.BINARY
CD_SHELL = 'cd -- "$0" || { echo "herdr-revive: $0 is gone" >&2; cd || cd /; }; exec "${SHELL:-sh}" -l'
TUI_READS = {"ping", "session.snapshot", "layout.export", "pane.process_info", "pane.get"}

# Answers the way the live app did: space.new and tab.new report no pane, only
# pane.split does, and ls lists every terminal made so far with its pane, in
# the space and tab it was made in. Call n makes terminal t<n> in pane p<n>.
FAKE_APP = """
import json, os, sys
state = os.environ["FAKE_APP_STATE"]
calls = os.path.join(state, "calls.jsonl")
with open(calls, "a") as f:
    f.write(json.dumps(sys.argv[1:]) + "\\n")
with open(calls) as f:
    n = len(f.read().splitlines())
spaces_path = os.path.join(state, "spaces.json")
spaces = []
if os.path.exists(spaces_path):
    with open(spaces_path) as f:
        spaces = json.load(f)
if sys.argv[1] == "ls":
    print(json.dumps({"spaces": spaces, "recent": []}))
    sys.exit(0)
if sys.argv[1] != "do":
    sys.exit(2)
command, args = sys.argv[2], json.loads(sys.argv[3])
terminal = {"terminal": "t%d" % n, "pane": "p%d" % n}
if command == "space.new":
    if os.environ.get("FAKE_APP_SLOW"):
        import time
        time.sleep(float(os.environ["FAKE_APP_SLOW"]))
        with open(spaces_path) if os.path.exists(spaces_path) else open(os.devnull) as f:
            text = f.read()
        spaces = json.loads(text) if text else []
    tab = {"id": "tb%d" % n, "name": args.get("tab", ""), "terminals": [terminal]}
    spaces.append({"id": "s%d" % n, "name": args.get("name", ""),
                   "folder": args["folder"], "tabs": [tab]})
    made = {"space": "s%d" % n, "tab": tab["id"], "terminal": terminal["terminal"]}
elif command == "tab.new":
    space = next(s for s in spaces if s["id"] == args["space"])
    space["tabs"].append({"id": "tb%d" % n, "name": args.get("name", ""),
                          "terminals": [terminal]})
    made = {"space": space["id"], "tab": "tb%d" % n, "terminal": terminal["terminal"]}
elif command == "pane.split":
    if os.environ.get("FAKE_APP_HANG_SPLIT"):
        import time
        time.sleep(60)
    tab = next(t for s in spaces for t in s["tabs"]
               if any(x["pane"] == args["pane"] for x in t["terminals"]))
    tab["terminals"].append(terminal)
    made = dict(terminal)
else:
    sys.exit(2)
with open(spaces_path, "w") as f:
    json.dump(spaces, f)
print(json.dumps(made))
"""


class FakeApp:
    def __init__(self, root):
        self.bin = root / "app-bin"
        self.state = root / "app-state"
        self.bin.mkdir()
        self.state.mkdir()
        script = self.bin / "herdr-app"
        script.write_text(f"#!{sys.executable} -I\n{FAKE_APP}")
        script.chmod(0o755)

    def calls(self):
        path = self.state / "calls.jsonl"
        if not path.exists():
            return []
        calls = []
        for line in path.read_text().splitlines():
            call = json.loads(line)
            if call[0] == "do":
                call[2] = json.loads(call[2])
            calls.append(call)
        return calls

    def forget_calls(self):
        (self.state / "calls.jsonl").unlink(missing_ok=True)


def clean_environment(**values):
    """This suite may run inside Herdr or herdr-core: nothing the binary sees
    may name the real session, server, core or terminal."""
    env = {key: value for key, value in os.environ.items()
           if not key.startswith(("HERDR_", "HC_"))}
    env.update(values)
    return env


class Tui(integration.Fixture):
    """w1 "[1] work": tab w1:t1 "build" = w1:p1 | w1:p2 (right, 0.6) and tab
    w1:t2 "2" = w1:p3. Answers workspace.create and layout.apply the way the
    TUI does, numbering what it makes; a made workspace shows in snapshots
    with one tab holding one shell pane, labelled as it was created.
    `restart(labels)` stands for a TUI server restart: every terminal id
    changes, and the workspaces named in `labels` get those labels (another
    workspace that took the id, or the same one restored)."""

    def __init__(self):
        self.made = []
        self.made_labels = {}
        self.fail_apply = False
        self.label = "[1] work"
        self.terminal_suffix = ""
        super().__init__(panes=3)

    def restart(self, labels):
        self.label = labels.pop("w1", self.label)
        self.made_labels.update(labels)
        self.terminal_suffix += "_restarted"

    def pane(self, i=1):
        pane = super().pane(i)
        pane["terminal_id"] += self.terminal_suffix
        if i == 3:
            pane["tab_id"] = "w1:t2"
        return pane

    def made_pane(self, workspace):
        pane = super().pane(1)
        pane.update(workspace_id=workspace, tab_id=f"{workspace}:t1",
                    pane_id=f"{workspace}:p1",
                    terminal_id=f"term_{workspace}{self.terminal_suffix}")
        return pane

    def result(self, request):
        method, params = request["method"], request.get("params") or {}
        if method == "session.snapshot":
            self.requests.append(method)
            return dict(type="session_snapshot", snapshot=dict(
                version=self.host_version, protocol=self.host_protocol,
                workspaces=[dict(workspace_id="w1", label=self.label, active_tab_id="w1:t1")]
                + [dict(workspace_id=w, label=self.made_labels[w], active_tab_id=f"{w}:t1")
                   for w in self.made],
                tabs=[dict(workspace_id="w1", tab_id="w1:t1", label="build"),
                      dict(workspace_id="w1", tab_id="w1:t2", label="2")]
                + [dict(workspace_id=w, tab_id=f"{w}:t1", label="1") for w in self.made],
                panes=[self.pane(i) for i in (1, 2, 3)] + [self.made_pane(w) for w in self.made]))
        if method == "layout.export":
            self.requests.append(method)
            workspace = params["tab_id"].split(":")[0]
            if workspace in self.made:
                return dict(type="layout_export", layout=dict(
                    workspace_id=workspace, tab_id=params["tab_id"],
                    focused_pane_id=f"{workspace}:p1",
                    root=dict(type="pane", pane_id=f"{workspace}:p1")))
            if params["tab_id"] == "w1:t1":
                root = dict(type="split", direction="right", ratio=0.6,
                            first=dict(type="pane", pane_id="w1:p1"),
                            second=dict(type="pane", pane_id="w1:p2"))
                focused = "w1:p1"
            else:
                root, focused = dict(type="pane", pane_id="w1:p3"), "w1:p3"
            return dict(type="layout_export", layout=dict(
                workspace_id="w1", tab_id=params["tab_id"], focused_pane_id=focused, root=root))
        if method == "workspace.create":
            self.requests.append(method)
            self.mutations.append(request)
            workspace = f"w{len(self.made) + 2}"
            self.made.append(workspace)
            self.made_labels[workspace] = f"[{len(self.made) + 1}] {params['label']}"
            return dict(type="workspace_created", workspace=dict(workspace_id=workspace),
                        tab=dict(tab_id=f"{workspace}:t1"))
        if method == "layout.apply":
            self.requests.append(method)
            self.mutations.append(request)
            if self.fail_apply:
                return dict(type="not_a_layout")
            applies = sum(1 for m in self.mutations if m["method"] == "layout.apply")
            workspace = params.get("workspace_id") or params["tab_id"].split(":")[0]
            tab = params.get("tab_id") or f"{workspace}:t{applies}"
            count = [0]

            def numbered(node):
                if node["type"] == "pane":
                    count[0] += 1
                    return dict(type="pane", pane_id=f"{workspace}:p{applies}n{count[0]}")
                return dict(type="split", direction=node["direction"], ratio=node["ratio"],
                            first=numbered(node["first"]), second=numbered(node["second"]))
            return dict(type="layout_apply", layout=dict(
                workspace_id=workspace, tab_id=tab, root=numbered(params["root"])))
        return super().result(request)


class TuiWithBrokenWorkspace(Tui):
    """Tui plus workspace w9, whose only pane reports no shell, so capture
    fails on it."""

    def broken(self):
        pane = self.pane(1)
        pane.update(workspace_id="w9", tab_id="w9:t1", pane_id="w9:p1",
                    terminal_id="term_fixture_9")
        return pane

    def result(self, request):
        method, params = request["method"], request.get("params") or {}
        if method == "session.snapshot":
            answer = super().result(request)
            snapshot = answer["snapshot"]
            snapshot["workspaces"].append(dict(workspace_id="w9", label="broken",
                                               active_tab_id="w9:t1"))
            snapshot["tabs"].append(dict(workspace_id="w9", tab_id="w9:t1", label="1"))
            snapshot["panes"].append(self.broken())
            return answer
        if method == "layout.export" and params["tab_id"] == "w9:t1":
            self.requests.append(method)
            return dict(type="layout_export", layout=dict(
                workspace_id="w9", tab_id="w9:t1", focused_pane_id="w9:p1",
                root=dict(type="pane", pane_id="w9:p1")))
        if method == "pane.process_info" and params["pane_id"] == "w9:p1":
            self.requests.append(method)
            return dict(type="pane_process_info", process_info=dict(
                pane_id="w9:p1", shell_pid=None, foreground_process_group_id=None,
                foreground_processes=[]))
        return super().result(request)


class DocCore(core_integration.FakeCore):
    """The fake core plus a read-only `doc.get` for the app's layout document."""
    doc = {"spaces": []}

    def answer(self, request):
        if request["method"] == "doc.get":
            with self.lock:
                self.requests.append(request)
            return dict(result=dict(value=self.doc, version=1))
        return super().answer(request)


# Fixture.setup builds its core from this module global; the swap stays in
# this process, so the other suites keep the plain fake core.
core_integration.FakeCore = DocCore


class Core(core_integration.Fixture):
    TABS = {"t1": "tb1", "t2": "tb1", "t3": "tb2"}

    def record(self, address, pid):
        record = super().record(address, pid)
        record["labels"]["herdr.tab"] = self.TABS[address]
        return record


def run(env, *args):
    return subprocess.run([str(BINARY), "migrate", *args], env=env,
                          capture_output=True, text=True, timeout=60)


def start(env, *args, **streams):
    return subprocess.Popen([str(BINARY), "migrate", *args], env=env, text=True,
                            **{"stdout": subprocess.PIPE, "stderr": subprocess.PIPE,
                               **streams})


def kill_during_split(test, env, state):
    """Starts `--to app --yes`, kills it while the stand-in app hangs in
    pane.split (after space.new made the space), as the dialog's time limit,
    Ctrl-C or a crash would, and returns the space the ledger names for it."""
    import signal
    import time
    killed = start(dict(env, FAKE_APP_HANG_SPLIT="1"), "--to", "app", "--yes",
                   start_new_session=True)
    end = time.monotonic() + 30
    while time.monotonic() < end:
        path = Path(env["FAKE_APP_STATE"]) / "calls.jsonl"
        if path.exists() and '"pane.split"' in path.read_text():
            break
        time.sleep(0.05)
    os.killpg(killed.pid, signal.SIGKILL)
    killed.communicate(timeout=30)
    ledger = json.loads((state / "migrated.json").read_text())
    test.assertEqual(ledger["entries"], [])
    test.assertEqual(len(ledger["partial"]), 1, ledger)
    created = ledger["partial"][0]["created"]
    test.assertTrue(created["space"] and created["unknown"], created)
    test.assertEqual(len(created["terminals"]), 1, "space.new's terminal")
    return created["space"]


def overlapping_or_refused(test, runs):
    """Each of two overlapping runs either copies, finds the copy the other
    made (`nothing to copy`), or is told that the other one is running."""
    for done in runs:
        out, err = done.communicate(timeout=60)
        test.assertTrue(done.returncode == 0 or "another migration is running" in err,
                        f"{done.returncode}: {out}{err}")


class TuiToApp(unittest.TestCase):
    TUI = Tui

    def setUp(self):
        self.tui = self.TUI()
        self.addCleanup(self.tui.close)
        self.app = FakeApp(self.tui.root)
        root = self.tui.root
        home = root / "home"
        home.mkdir()
        # HERDR_REVIVE_HOST=core is set on purpose: migrate ignores it.
        self.env = clean_environment(
            HOME=str(home),
            XDG_STATE_HOME=str(root / "xdg-state"),
            XDG_CONFIG_HOME=str(root / "xdg-config"),
            XDG_RUNTIME_DIR=str(root / "runtime"),
            HC_SOCKET=str(root / "no-core.sock"),
            HERDR_SOCKET_PATH=str(self.tui.socket_path),
            HERDR_PLUGIN_CONFIG_DIR=str(self.tui.config),
            HERDR_PLUGIN_STATE_DIR=str(self.tui.state),
            HERDR_BIN_PATH="/missing/herdr",
            HERDR_REVIVE_HOST="core",
            CLAUDE_CONFIG_DIR=str(root / "claude-config"),
            FAKE_APP_STATE=str(self.app.state),
            PATH=f"{self.app.bin}:{os.environ['PATH']}")

    def assert_tui_only_read(self):
        self.assertEqual(self.tui.mutations, [], "the TUI source is never changed")
        self.assertLessEqual(set(self.tui.requests), TUI_READS)

    def test_the_workspace_becomes_one_space_in_this_order_and_then_unchanged(self):
        result = run(self.env, "--to", "app", "--yes")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue(result.stdout.endswith("done\n"), result.stdout)
        self.assertIn("create     w1 -> \"work\", 2 tabs, 3 panes, 0 agents", result.stdout)
        root = str(self.tui.root)
        # ls whether the window is open, ls again under the ledger lock for
        # the plan, then the copy.
        self.assertEqual(self.app.calls(), [
            ["ls"],
            ["ls"],
            ["do", "space.new", {"folder": root, "name": "work", "tab": "build"}],
            ["ls"],
            ["do", "pane.split", {"direction": "right", "pane": "p3",
                                  "argv": ["sh", "-c", CD_SHELL, root]}],
            ["do", "tab.new", {"folder": root, "space": "s3"}],
            ["ls"],
        ])
        self.assert_tui_only_read()
        ledger = json.loads((self.tui.root / "xdg-state/herdr-revive/migrated.json").read_text())
        self.assertEqual(ledger["entries"][0]["created"],
                         {"space": "s3", "terminals": ["t3", "t5", "t6"]})
        self.assertEqual(ledger["partial"], [])
        # Another machine reading this one learns of the copy from its export.
        export = json.loads(run(self.env, "--export", "tui").stdout)
        self.assertEqual([(c["workspace_id"], c["space"]) for c in export["app_copies"]],
                         [("w1", "s3")])

        self.app.forget_calls()
        again = run(self.env, "--to", "app", "--yes")
        self.assertEqual(again.returncode, 0, again.stderr)
        self.assertIn("unchanged  w1", again.stdout)
        self.assertIn("copy s3", again.stdout)
        self.assertTrue(again.stdout.endswith("nothing to copy\n"), again.stdout)
        self.assertEqual(self.app.calls(), [["ls"], ["ls"]])
        self.assert_tui_only_read()

    def test_a_workspace_that_took_a_copied_workspaces_id_is_not_unchanged(self):
        done = run(self.env, "--to", "app", "--yes")
        self.assertEqual(done.returncode, 0, done.stderr)
        ledger = json.loads((self.tui.root / "xdg-state/herdr-revive/migrated.json").read_text())
        self.assertEqual(ledger["entries"][0]["source"]["terminal_ids"],
                         ["term_fixture_1", "term_fixture_2", "term_fixture_3"])
        space = ledger["entries"][0]["created"]["space"]
        self.assertEqual(ledger["entries"][0]["source"]["label"], "work")

        # Renamed without a restart: the same terminals.
        self.tui.label = "[1] renamed"
        renamed = run(self.env, "--to", "app", "--dry-run")
        self.assertIn("unchanged  w1 -> \"renamed\"", renamed.stdout)
        self.assertNotIn("only by its label", renamed.stdout)
        self.tui.label = "[1] work"

        # The TUI restarted and restored w1: new terminals, same label.
        self.tui.restart({"w1": "[1] work"})
        restored = run(self.env, "--to", "app", "--dry-run")
        self.assertEqual(restored.returncode, 0, restored.stderr)
        self.assertIn(f"unchanged  w1 -> \"work\"", restored.stdout)
        self.assertIn(f"copy {space}", restored.stdout)
        self.assertIn(f"copied a TUI workspace w1 to space {space}, and this one matches it "
                      f"only by its label: if it is another workspace, close {space} or "
                      "rename this one to copy it", restored.stdout)
        self.assertTrue(restored.stdout.endswith("nothing to copy\n"), restored.stdout)

        # The copied w1 was closed and the TUI restarted; "frontend" is w1 now.
        self.tui.restart({"w1": "[1] frontend"})
        plan = run(self.env, "--to", "app", "--dry-run")
        self.assertEqual(plan.returncode, 0, plan.stderr)
        self.assertIn("create     w1 -> \"frontend\", 2 tabs, 3 panes, 0 agents", plan.stdout)
        self.assertIn(f"copied a TUI workspace w1 to space {space}; this may be another "
                      f"workspace that took its id: if it is the same, close {space} first",
                      plan.stdout)
        self.assertNotIn("unchanged", plan.stdout)
        self.assertNotIn("nothing to copy", plan.stdout)
        report = json.loads(run(self.env, "--to", "app", "--dry-run", "--json").stdout)
        self.assertEqual(report["lines"][0]["status"], "create")
        self.assertEqual(len(report["lines"][0]["notes"]), 1)
        self.assert_tui_only_read()

    def test_two_overlapping_runs_copy_a_workspace_once(self):
        # A is still in space.new when B starts; B must not plan from a
        # listing taken before A recorded its copy.
        env = dict(self.env, FAKE_APP_SLOW="1")
        first = start(env, "--to", "app", "--yes")
        import time
        time.sleep(0.5)
        second = start(env, "--to", "app", "--yes")
        overlapping_or_refused(self, [first, second])
        made = [call for call in self.app.calls() if call[:2] == ["do", "space.new"]]
        self.assertEqual(len(made), 1, self.app.calls())
        ledger = json.loads((self.tui.root / "xdg-state/herdr-revive/migrated.json").read_text())
        self.assertEqual(len(ledger["entries"]), 1, ledger)

    def test_a_copy_killed_part_way_leaves_a_record_the_next_plan_names(self):
        space = kill_during_split(self, self.env, self.tui.root / "xdg-state/herdr-revive")
        plan = json.loads(run(self.env, "--to", "app", "--dry-run", "--json").stdout)
        line = plan["lines"][0]
        self.assertEqual((line["status"], line["reason"]),
                         ("skip", f"partial copy {space} still open: close it first"))

    def test_export_prints_the_capture_contract(self):
        result = run(self.env, "--export", "tui")
        self.assertEqual(result.returncode, 0, result.stderr)
        export = json.loads(result.stdout)
        self.assertEqual(list(export), ["tool", "schema", "session", "snapshot", "launches",
                                        "live_agents", "denied", "tui_copies", "terminal_ids",
                                        "missing_folders", "app_copies", "partial_copies",
                                        "uncaptured", "warnings", "live_workspaces"])
        self.assertEqual(export["tui_copies"], [])
        self.assertEqual(export["app_copies"], [])
        self.assertEqual(export["missing_folders"], {})
        self.assertEqual(export["terminal_ids"],
                         {"w1": ["term_fixture_1", "term_fixture_2", "term_fixture_3"]})
        self.assertEqual(sorted(export["launches"]), ["w1:p1", "w1:p2", "w1:p3"])
        self.assertEqual(export["launches"]["w1:p1"], {"cwd": str(self.tui.root), "argv": None})
        self.assertEqual(self.app.calls(), [])
        self.assert_tui_only_read()

    def test_a_closed_app_is_named_while_another_migration_holds_the_lock(self):
        (self.app.bin / "herdr-app").write_text("#!/bin/sh\necho closed >&2\nexit 1\n")
        state = self.tui.root / "xdg-state/herdr-revive"
        state.mkdir(parents=True, exist_ok=True)
        with open(state / "migrated.lock", "w") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            result = run(self.env, "--to", "app", "--dry-run")
        self.assertEqual(result.returncode, 1)
        self.assertIn("open the Herdr app first", result.stderr)
        self.assertNotIn("another migration is running", result.stderr)

    def test_a_closed_app_stops_before_the_tui_is_read(self):
        (self.app.bin / "herdr-app").write_text("#!/bin/sh\necho closed >&2\nexit 1\n")
        result = run(self.env, "--to", "app", "--dry-run")
        self.assertEqual(result.returncode, 1)
        self.assertIn("open the Herdr app first", result.stderr)
        self.assertNotIn("session.snapshot", self.tui.requests)


class TuiWithGoneFolder(Tui):
    """Tui whose pane w1:p3 (the first pane of tab 2) is in a folder that was
    removed, and whose pane w1:p2 is in one that was removed and made again,
    both as Linux names them."""

    def pane(self, i=1):
        pane = super().pane(i)
        if i == 3:
            pane["cwd"] = f"{self.root}/gone (deleted)"
        if i == 2:
            pane["cwd"] = f"{self.root} (deleted)"
        return pane


class GoneFolder(unittest.TestCase):
    TUI = TuiWithGoneFolder
    setUp = TuiToApp.setUp
    assert_tui_only_read = TuiToApp.assert_tui_only_read

    def test_a_pane_whose_folder_is_gone_opens_a_shell_at_home_and_the_plan_says_so(self):
        gone = f"{self.tui.root}/gone"
        plan = run(self.env, "--to", "app", "--dry-run")
        self.assertEqual(plan.returncode, 0, plan.stderr)
        self.assertIn(f"the folder of 1 pane no longer exists ({gone}): it opens a shell "
                      "in the home folder, without its command", plan.stdout)
        export = json.loads(run(self.env, "--export", "tui").stdout)
        self.assertEqual(export["missing_folders"], {"w1:p3": gone})
        self.assertEqual(export["launches"]["w1:p3"], {"cwd": self.env["HOME"], "argv": None})
        self.assertEqual(export["launches"]["w1:p2"]["cwd"], str(self.tui.root),
                         "a folder made again at its path is where the pane was")
        result = run(self.env, "--to", "app", "--yes")
        self.assertEqual(result.returncode, 0, result.stderr)
        root, home = str(self.tui.root), self.env["HOME"]
        tab = [call[2] for call in self.app.calls() if call[:2] == ["do", "tab.new"]]
        self.assertEqual([(args["folder"], "argv" in args) for args in tab], [(home, False)])
        self.assertIn(["do", "space.new", {"folder": root, "name": "work", "tab": "build"}],
                      self.app.calls())
        splits = [call[2] for call in self.app.calls() if call[:2] == ["do", "pane.split"]]
        self.assertEqual([args["argv"] for args in splits], [["sh", "-c", CD_SHELL, root]])
        self.assert_tui_only_read()


class BrokenWorkspace(unittest.TestCase):
    TUI = TuiWithBrokenWorkspace
    setUp = TuiToApp.setUp
    assert_tui_only_read = TuiToApp.assert_tui_only_read

    def test_a_workspace_that_cannot_be_captured_is_skipped_and_the_rest_copied(self):
        whole = run(self.env, "--to", "app", "--dry-run", "--json")
        self.assertEqual(whole.returncode, 0, whole.stderr)
        lines = {l["source"]: l for l in json.loads(whole.stdout)["lines"]}
        self.assertEqual(lines["w1"]["status"], "create")
        self.assertEqual((lines["w9"]["status"], lines["w9"]["name"]), ("skip", "broken"))
        self.assertTrue(lines["w9"]["reason"].startswith("cannot capture: "), lines["w9"])
        text = run(self.env, "--to", "app", "--dry-run")
        self.assertIn("skip       w9 -> \"broken\": cannot capture: ", text.stdout)
        done = run(self.env, "--to", "app", "--yes")
        self.assertEqual(done.returncode, 0, done.stderr)
        made = [call for call in self.app.calls() if call[:2] == ["do", "space.new"]]
        self.assertEqual([call[2]["name"] for call in made], ["work"], "only w1 is copied")

        only = run(self.env, "--to", "app", "--only", "w1", "--dry-run", "--json")
        self.assertEqual(only.returncode, 0, only.stderr)
        lines = json.loads(only.stdout)["lines"]
        self.assertEqual([(l["source"], l["status"]) for l in lines], [("w1", "unchanged")],
                         "--only reads w1 alone; it was copied above")
        self.assert_tui_only_read()


class AppToTui(unittest.TestCase):
    def setUp(self):
        self.core = Core()
        self.addCleanup(self.core.close)
        self.tui = Tui()
        self.addCleanup(self.tui.close)
        self.app = FakeApp(self.core.root)
        work = str(self.core.work)
        self.core.core.doc = {"spaces": [{"id": "s1", "name": "api", "folder": work, "tabs": [
            {"id": "tb1", "name": "main", "root": {"type": "row", "id": "r1", "children": [
                {"type": "block", "id": "b1", "content": {"terminal": "t1"}, "size": {"weight": 2}},
                {"type": "block", "id": "b2", "content": {"terminal": "t2"}, "size": {"weight": 1}},
                {"type": "block", "id": "b9", "content": {"mod": "files"}}]}},
            {"id": "tb2", "name": "", "root": {"type": "block", "id": "b3",
                                               "content": {"terminal": "t3"}}}]}]}
        for address in ("t1", "t2", "t3"):
            self.core.start_shell(address).wait_output(core_integration.PROMPT)
        core_env = {key: value for key, value in self.core.env().items()
                    if key not in ("PS1", "TERM")}
        self.env = clean_environment(**dict(
            core_env,
            XDG_RUNTIME_DIR=str(self.core.root / "runtime"),
            HERDR_SOCKET_PATH=str(self.tui.socket_path),
            HERDR_PLUGIN_CONFIG_DIR=str(self.tui.config),
            HERDR_PLUGIN_STATE_DIR=str(self.tui.state),
            HERDR_BIN_PATH="/missing/herdr",
            FAKE_APP_STATE=str(self.app.state),
            PATH=f"{self.app.bin}:{core_env['PATH']}"))

    def ledger(self):
        return json.loads((self.core.state / "herdr-revive/migrated.json").read_text())

    def test_two_overlapping_runs_copy_a_space_once(self):
        # A waits at its prompt with its plan made; B starts, then A is told
        # yes. B must not plan from a TUI listing taken before A's copy.
        import pty
        import time
        master, slave = pty.openpty()
        self.addCleanup(os.close, master)
        first = start(self.env, "--to", "tui", stdin=slave, stdout=slave, stderr=slave)
        os.close(slave)
        output = b""
        end = time.monotonic() + 30
        while b"[y/N]" not in output and time.monotonic() < end:
            output += os.read(master, 65536)
        self.assertIn(b"[y/N]", output)
        second = start(self.env, "--to", "tui", "--yes")
        time.sleep(0.3)
        os.write(master, b"y\n")
        first.wait(timeout=60)
        out, err = second.communicate(timeout=60)
        self.assertTrue(second.returncode == 0 or "another migration is running" in err,
                        f"{second.returncode}: {out}{err}")
        creates = [m for m in self.tui.mutations if m["method"] == "workspace.create"]
        self.assertEqual(len(creates), 1, self.tui.mutations)
        self.assertEqual(len(self.ledger()["entries"]), 1)

    def test_a_space_copied_from_the_tui_is_not_copied_back(self):
        to_app = run(self.env, "--to", "app", "--yes")
        self.assertEqual(to_app.returncode, 0, to_app.stderr)
        space = self.ledger()["entries"][0]["created"]["space"]
        doc = self.core.core.doc
        self.core.core.doc = {"spaces": doc["spaces"] + [
            {"id": space, "name": "work", "folder": str(self.core.work), "tabs": [
                {"id": "tb9", "name": "build", "root": {"type": "block", "id": "b7",
                                                        "content": {"terminal": "t1"}}}]}]}

        plan = run(self.env, "--to", "tui", "--dry-run", "--json")
        self.assertEqual(plan.returncode, 0, plan.stderr)
        lines = {l["source"]: l for l in json.loads(plan.stdout)["lines"]}
        self.assertEqual(lines["s1"]["status"], "create")
        self.assertEqual((lines[space]["status"], lines[space]["reason"]),
                         ("skip", "copy of TUI workspace w1"))

        result = run(self.env, "--to", "tui", "--yes")
        self.assertEqual(result.returncode, 0, result.stderr)
        creates = [m for m in self.tui.mutations if m["method"] == "workspace.create"]
        self.assertEqual([m["params"]["label"] for m in creates], ["api"],
                         "only the app's own space is copied")

    def test_a_space_left_by_a_killed_copy_is_not_copied_back(self):
        space = kill_during_split(self, self.env, self.core.state / "herdr-revive")
        doc = self.core.core.doc
        self.core.core.doc = {"spaces": doc["spaces"] + [
            {"id": space, "name": "work", "folder": str(self.core.work), "tabs": [
                {"id": "tb9", "name": "build", "root": {"type": "block", "id": "b7",
                                                        "content": {"terminal": "t1"}}}]}]}
        to_tui = run(self.env, "--to", "tui", "--dry-run", "--json")
        self.assertEqual(to_tui.returncode, 0, to_tui.stderr)
        lines = {l["source"]: l for l in json.loads(to_tui.stdout)["lines"]}
        self.assertEqual((lines[space]["status"], lines[space]["reason"]),
                         ("skip", "copy of TUI workspace w1"))
        to_app = json.loads(run(self.env, "--to", "app", "--dry-run", "--json").stdout)
        self.assertEqual(
            [(l["status"], l["reason"]) for l in to_app["lines"]],
            [("skip", f"partial copy {space} still open: close it first")])

    def test_a_workspace_copied_from_the_app_is_not_copied_back(self):
        to_tui = run(self.env, "--to", "tui", "--yes")
        self.assertEqual(to_tui.returncode, 0, to_tui.stderr)
        session = self.ledger()["entries"][0]["created"]["session"]

        # Another machine's --to app reads this machine's copies from the
        # export; reading the ledger for it changes nothing.
        ledger_path = self.core.state / "herdr-revive/migrated.json"
        before = ledger_path.read_bytes()
        export = run(self.env, "--export", "tui")
        self.assertEqual(export.returncode, 0, export.stderr)
        at = self.ledger()["entries"][0]["at"]
        copies = [{"space": "s1", "session": session, "workspace_id": "w2",
                   "terminal_ids": ["term_w2"], "label": "api", "at": at}]
        self.assertEqual(json.loads(export.stdout)["tui_copies"], copies)
        # A renumbered TUI may have given the copy any id, so an export of
        # w1 alone lists it too, with every live workspace to match it to.
        only_w1 = json.loads(run(self.env, "--export", "tui", "--only", "w1").stdout)
        self.assertEqual(only_w1["tui_copies"], copies)
        self.assertEqual(only_w1["live_workspaces"],
                         [{"workspace_id": "w1", "label": "[1] work",
                           "terminal_ids": ["term_fixture_1", "term_fixture_2",
                                            "term_fixture_3"]},
                          {"workspace_id": "w2", "label": "[2] api",
                           "terminal_ids": ["term_w2"]}])
        self.assertEqual(ledger_path.read_bytes(), before)
        plan = run(self.env, "--to", "tui", "--dry-run", "--json")
        self.assertEqual(plan.returncode, 0, plan.stderr)
        self.assertEqual(json.loads(plan.stdout)["tui_session"],
                         {"session": session, "workspaces": ["w1", "w2"],
                          "terminal_ids": {"w1": ["term_fixture_1", "term_fixture_2",
                                                  "term_fixture_3"],
                                           "w2": ["term_w2"]},
                          "labels": {"w1": "[1] work", "w2": "[2] api"},
                          "spaces": ["s1"]})

        # The app window lists the space the copy came from.
        (self.app.state / "spaces.json").write_text(json.dumps(
            [{"id": "s1", "name": "api", "folder": str(self.core.work), "tabs": []}]))

        plan = run(self.env, "--to", "app", "--dry-run", "--json")
        self.assertEqual(plan.returncode, 0, plan.stderr)
        lines = {l["source"]: l for l in json.loads(plan.stdout)["lines"]}
        self.assertEqual(lines["w1"]["status"], "create")
        self.assertEqual((lines["w2"]["status"], lines["w2"]["reason"]),
                         ("skip", "copy of app space s1"))

        (self.app.state / "spaces.json").write_text("[]")
        closed = run(self.env, "--to", "app", "--dry-run", "--json")
        self.assertEqual(closed.returncode, 0, closed.stderr)
        lines = {l["source"]: l for l in json.loads(closed.stdout)["lines"]}
        self.assertEqual(lines["w2"]["status"], "create", "its space was closed")

    def test_an_unnamed_space_without_a_window_takes_its_folder_name(self):
        self.core.core.doc["spaces"][0]["name"] = ""
        no_app = dict(self.env, PATH=self.env["PATH"].split(":", 1)[1])
        result = run(no_app, "--to", "tui", "--yes")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.app.calls(), [], "herdr-app is not installed here")
        creates = [m for m in self.tui.mutations if m["method"] == "workspace.create"]
        self.assertEqual([m["params"]["label"] for m in creates], [self.core.work.name])

    def test_the_tui_session_lock_is_taken_only_to_apply(self):
        first = run(self.env, "--to", "tui", "--dry-run")
        self.assertEqual(first.returncode, 0, first.stderr)
        (session,) = self.tui.state.glob("*/snapshots")
        lock_path = session.parent / "operation.lock"
        with open(lock_path, "a") as held:
            fcntl.flock(held, fcntl.LOCK_EX)
            plan = run(self.env, "--to", "tui", "--dry-run")
            self.assertEqual(plan.returncode, 0, "a plan does not need the lock")
            blocked = run(self.env, "--to", "tui", "--yes")
            self.assertEqual(blocked.returncode, 1, blocked.stdout)
            self.assertIn("cannot take the TUI session lock", blocked.stderr)
        self.assertEqual(self.tui.mutations, [])
        done = run(self.env, "--to", "tui", "--yes")
        self.assertEqual(done.returncode, 0, done.stderr)

    def test_a_failed_rebuild_names_the_exact_recovery_commands(self):
        self.tui.fail_apply = True
        failed = run(self.env, "--to", "tui", "--yes")
        self.assertEqual(failed.returncode, 1, failed.stdout)
        self.assertIn("copy of s1 stopped; TUI workspaces created for this space: w2",
                      failed.stderr)
        self.assertIn("TUI saves and restores are paused", failed.stderr)
        inspect, acknowledge = re.findall(r"`(herdr-revive [^`]*)`", failed.stderr)
        self.assertTrue(inspect.endswith(" recovery inspect"), inspect)

        blocked = run(self.env, "--to", "tui", "--dry-run")
        self.assertEqual(blocked.returncode, 1)
        self.assertIn(acknowledge, blocked.stderr, "the next run names the same commands")

        # From a plain shell: no plugin environment, no herdr session.
        shell = clean_environment(HOME=self.env["HOME"], PATH="/usr/bin:/bin")
        for command in (inspect, acknowledge):
            argv = [str(BINARY), *shlex.split(command)[1:]]
            done = subprocess.run(argv, env=shell, capture_output=True, text=True, timeout=60)
            self.assertEqual(done.returncode, 0, f"{command}\n{done.stderr}")
            if command == inspect:
                self.assertIsNotNone(json.loads(done.stdout)["result"]["rebuild"])
        self.assertEqual(list(self.tui.state.glob("*/rebuild-pending.json")), [])

        self.tui.fail_apply = False
        again = run(self.env, "--to", "tui", "--dry-run", "--json")
        self.assertEqual(again.returncode, 0, again.stderr)
        self.assertEqual(json.loads(again.stdout)["lines"][0]["status"], "create")

    def core_requests_since(self, start):
        with self.core.core.lock:
            return [r["method"] for r in self.core.core.requests[start:]]

    def test_the_space_becomes_one_workspace_through_the_rebuild_and_then_unchanged(self):
        start = len(self.core.core.requests)
        plan = run(self.env, "--to", "tui", "--dry-run", "--json")
        self.assertEqual(plan.returncode, 0, plan.stderr)
        line = json.loads(plan.stdout)["lines"][0]
        self.assertEqual((line["source"], line["name"], line["tabs"], line["panes"],
                          line["mod_blocks"], line["status"]), ("s1", "api", 2, 3, 1, "create"))
        self.assertEqual(self.tui.mutations, [])

        result = run(self.env, "--to", "tui", "--yes")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue(result.stdout.endswith("done\n"), result.stdout)
        work = str(self.core.work)
        methods = [m["method"] for m in self.tui.mutations]
        self.assertEqual(methods, ["workspace.create", "layout.apply", "layout.apply"])
        create, first, second = (m["params"] for m in self.tui.mutations)
        self.assertEqual(create, {"label": "api", "cwd": work, "focus": False})
        self.assertEqual(first["tab_id"], "w2:t1")
        self.assertEqual(first["tab_label"], "main")
        self.assertIs(first["focus"], False)
        self.assertEqual(first["root"]["type"], "split")
        self.assertEqual(first["root"]["direction"], "right")
        self.assertAlmostEqual(first["root"]["ratio"], 2 / 3)
        self.assertEqual(first["root"]["first"], {"type": "pane", "cwd": work})
        self.assertEqual(first["root"]["second"], {"type": "pane", "cwd": work})
        self.assertEqual(second["workspace_id"], "w2")
        self.assertNotIn("tab_id", second)
        self.assertEqual(second["tab_label"], "2")
        self.assertEqual(second["root"], {"type": "pane", "cwd": work})
        self.assertEqual([m for m in self.core_requests_since(start)
                          if m not in ("hello", "terminal.list", "terminal.get", "doc.get")],
                         [], "the core is only read")
        self.assertEqual(self.app.calls(), [], "every space is named, so ls is not needed")
        ledger = json.loads((self.core.state / "herdr-revive/migrated.json").read_text())
        self.assertEqual(ledger["entries"][0]["created"]["workspace_id"], "w2")
        self.assertEqual(list(self.tui.state.glob("*/rebuild-pending.json")), [])

        again = run(self.env, "--to", "tui", "--yes")
        self.assertEqual(again.returncode, 0, again.stderr)
        self.assertIn("unchanged  s1", again.stdout)
        self.assertIn("copy w2", again.stdout)
        self.assertTrue(again.stdout.endswith("nothing to copy\n"), again.stdout)
        self.assertEqual(len(self.tui.mutations), 3)
        self.assertEqual(ledger["entries"][0]["created"]["terminal_ids"], ["term_w2"])

        self.assertEqual(ledger["entries"][0]["created"]["label"], "api")

        # The TUI restarted and restored w2: new terminals, same label.
        self.tui.restart({"w2": "[2] api"})
        restored = run(self.env, "--to", "tui", "--dry-run")
        self.assertEqual(restored.returncode, 0, restored.stderr)
        self.assertIn("unchanged  s1", restored.stdout)
        self.assertIn("copied this space to TUI workspace w2, which matches the copy only by "
                      "its label: if w2 is another workspace, rename it to copy this space "
                      "again", restored.stdout)
        self.assertTrue(restored.stdout.endswith("nothing to copy\n"), restored.stdout)

        # The copy w2 was closed and the TUI restarted; another workspace is w2.
        self.tui.restart({"w2": "[2] notes"})
        reused = run(self.env, "--to", "tui", "--dry-run")
        self.assertEqual(reused.returncode, 0, reused.stderr)
        self.assertIn("create     s1 -> \"api\"", reused.stdout)
        self.assertIn("copied this space to TUI workspace w2; w2 may now be another "
                      "workspace that took its id: if it is the copy, close it first",
                      reused.stdout)


if __name__ == "__main__":
    unittest.main(verbosity=2)
