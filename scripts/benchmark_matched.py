#!/usr/bin/env python3
"""Compare unchanged JavaScript and Rust plugins on shared disposable real hosts."""
import argparse
from collections import Counter
from contextlib import ExitStack
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import random
import re
import shlex
import shutil
import socket
import statistics
import subprocess
import tempfile
import time

ROOT = Path(__file__).resolve().parents[1]
BINARY = ROOT / "target/release/herdr-revive"
NODE = shutil.which("node")
VARIANTS = ("javascript_cli", "rust_cli", "rust_direct")
OPERATIONS = ("save", "preview", "event_debounced", "event_boot_done")


def checked(command, **kwargs):
    try:
        return subprocess.run(command, check=True, capture_output=True, text=True,
                              timeout=60, **kwargs)
    except subprocess.CalledProcessError as error:
        raise RuntimeError(f"{Path(command[0]).name} failed: {error.stderr[-2000:]}") from error


def stop_server(server):
    server.terminate()
    try:
        server.wait(timeout=5)
    except subprocess.TimeoutExpired:
        server.kill()
        server.wait(timeout=5)


def build_programs(root):
    source = root / "wait.c"
    source.write_text("#include <unistd.h>\n\nint main(void)\n{\n\tfor (;;)\n\t\tpause();\n}\n")
    programs = root / "bin"
    programs.mkdir()
    checked(["cc", "-O2", "-Wall", "-Wextra", str(source), "-o", str(programs / "fixture-job")])
    for name in ("ssh", "minicom", "node", "claude", "codex", "gemini", "copilot", "cursor-agent"):
        shutil.copy2(programs / "fixture-job", programs / name)
    shell = root / "fixture-shell"
    shell.write_text('#!/bin/sh\nexec /bin/bash --noprofile --norc "$@"\n')
    shell.chmod(0o700)
    return programs, shell


class Fixture:
    def __init__(self, stack, root, count, baseline, programs, shell, herdr, empty_arg_probe=False):
        self.root, self.count, self.baseline = root, count, baseline
        self.empty_arg_probe = empty_arg_probe
        self.probe_results = {}
        root.mkdir()
        self.socket = root / "host.sock"
        self.env = {k: v for k, v in os.environ.items()
                    if not k.startswith("HERDR_") and k not in ("NODE_OPTIONS", "NODE_PATH")}
        self.env.update(XDG_CONFIG_HOME=str(root / "config"), XDG_STATE_HOME=str(root / "state"),
                        HERDR_CONFIG_PATH=str(root / "config/herdr/config.toml"),
                        HERDR_SOCKET_PATH=str(self.socket), HERDR_BIN_PATH=herdr,
                        SHELL=str(shell), PATH=str(programs) + ":" + self.env["PATH"])
        log = stack.enter_context(open(root / "server.log", "w"))
        self.server = subprocess.Popen([herdr, "server"], env=self.env, cwd=root, stdout=log, stderr=log)
        stack.callback(stop_server, self.server)
        deadline = time.monotonic() + 10
        while True:
            try:
                self.api("ping")
                break
            except (OSError, ValueError):
                if self.server.poll() is not None or time.monotonic() > deadline:
                    raise RuntimeError("private Herdr server failed to become ready")
                time.sleep(0.05)
        self.expected = {}
        self.populate(programs)
        for variant in VARIANTS:
            directory = root / variant
            (directory / "config").mkdir(parents=True)
            (directory / "state").mkdir()
            if variant == "javascript_cli":
                (directory / "config/allowlist.txt").write_text("*\n")
            self.configure(variant, "event_boot_done")
            if variant == "javascript_cli":
                checked([NODE, "-e", "const b=require(process.env.BENCH_BASELINE+'/lib/boot'); "
                         "const t=b.token(); if(!t) throw Error('no boot'); "
                         "if(b.claim(t)!=='claimed') throw Error('already claimed'); b.markDone(t);"],
                        env=self.environment(variant), cwd=baseline)
            else:
                result = checked([str(BINARY), "event"], env=self.environment(variant), cwd=ROOT)
                if json.loads(result.stdout)["result"]["status"] != "no_snapshot":
                    raise AssertionError("Rust boot setup must not restore anything")
        self.latest = {}
        for variant in VARIANTS:
            self.invoke(variant, "save")
            pattern = "sessions/*/last.json" if variant == "javascript_cli" else "*/latest.json"
            paths = list((root / variant / "state").glob(pattern))
            if len(paths) != 1:
                raise AssertionError("expected exactly one private snapshot")
            self.latest[variant] = paths[0]
            self.verify_save(variant)
            self.verify_preview(variant, self.invoke(variant, "preview").stdout)
            self.invoke(variant, "event_debounced")
            self.invoke(variant, "event_boot_done")
        snap = self.api("session.snapshot")["snapshot"]
        self.shape = dict(panes=len(snap["panes"]), tabs=len(snap["tabs"]),
                          workspaces=len(snap["workspaces"]),
                          kinds=dict(Counter(v[1] for v in self.expected.values())))

    def api(self, method, params=None):
        with socket.socket(socket.AF_UNIX) as connection:
            connection.settimeout(10)
            connection.connect(str(self.socket))
            connection.sendall(json.dumps(dict(id="benchmark", method=method, params=params or {})).encode() + b"\n")
            with connection.makefile("rb") as stream:
                reply = json.loads(stream.readline(8 * 1024 * 1024))
        if "error" in reply:
            raise RuntimeError(f"private API {method}: {reply['error']}")
        return reply["result"]

    def populate(self, programs):
        directories = [self.root / f"cwd-{i}" for i in range(3)]
        for directory in directories:
            directory.mkdir()

        def tree(indices, depth=0):
            if len(indices) == 1:
                index = indices[0]
                return dict(type="pane", label=f"fixture-{index}", cwd=str(directories[index % 3]))
            middle = len(indices) // 2
            return dict(type="split", direction="right" if depth % 2 else "down", ratio=0.5,
                        first=tree(indices[:middle], depth + 1), second=tree(indices[middle:], depth + 1))

        for start in range(0, self.count, 20):
            created = self.api("workspace.create", dict(label=f"Fixture {start // 20}", cwd=str(directories[0]), focus=False))
            for tab_start in range(start, min(start + 20, self.count), 10):
                params = dict(root=tree(list(range(tab_start, min(tab_start + 10, self.count)))),
                              tab_label=f"Tab {tab_start // 10}", focus=False)
                params["tab_id" if tab_start == start else "workspace_id"] = (
                    created["tab"]["tab_id"] if tab_start == start else created["workspace"]["workspace_id"])
                self.api("layout.apply", params)
        time.sleep(0.15)
        for pane in self.api("session.snapshot")["snapshot"]["panes"]:
            index = int(pane["label"].split("-")[1])
            slot = index % 10
            cwd = str(directories[index % 3])
            if slot == 1:
                value = (cwd, "idle", None)
            elif slot < 5:
                name = {0: "fixture-job", 2: "ssh", 3: "minicom", 4: "node"}[slot]
                argv = [str(programs / name), "fixture.invalid", "space here", "'\"$;|", "中文"]
                if self.empty_arg_probe:
                    argv.append("")
                value = (cwd, "program", argv)
            else:
                agent = ("claude", "codex", "gemini", "copilot", "cursor")[slot - 5]
                executable = "cursor-agent" if agent == "cursor" else agent
                session = f"00000000-0000-4000-8000-{self.count * 1000 + index:012x}"
                argv = [str(programs / executable), "resume" if agent == "codex" else "--resume", session]
                value = (cwd, "agent", [agent, session])
            self.expected[pane["pane_id"]] = value
            if slot != 1:
                self.api("pane.send_input", dict(pane_id=pane["pane_id"], text=shlex.join(argv), keys=["Enter"]))
        deadline = time.monotonic() + 10
        expected_agents = sum(v[1] == "agent" for v in self.expected.values())
        while True:
            snap = self.api("session.snapshot")["snapshot"]
            if sum(bool(p.get("agent")) for p in snap["panes"]) == expected_agents:
                break
            if time.monotonic() > deadline:
                raise AssertionError("host did not detect the expected harmless agent fixtures")
            time.sleep(0.05)
        time.sleep(0.1)

    def configure(self, variant, operation):
        auto = operation == "event_boot_done"
        directory = self.root / variant / "config"
        if variant == "javascript_cli":
            text = json.dumps(dict(autoRestore=auto, autoRestoreSettleMs=0, agentResume=True))
            (directory / "settings.json").write_text(text)
        else:
            (directory / "config.toml").write_text(
                f'auto_restore = {str(auto).lower()}\nauto_save = true\nsettle_ms = 0\n'
                'debounce_ms = 86400000\nretention = 20\ntimeout_ms = 10000\n'
                f'transport = "{"cli" if variant == "rust_cli" else "direct"}"\nallowed_programs = ["*"]\n')

    def environment(self, variant):
        env = self.env.copy()
        env.update(HERDR_PLUGIN_CONFIG_DIR=str(self.root / variant / "config"),
                   HERDR_PLUGIN_STATE_DIR=str(self.root / variant / "state"),
                   HERDR_PLUGIN_ROOT=str(self.baseline if variant == "javascript_cli" else ROOT),
                   HERDR_PLUGIN_ID="ntindle.herdr-resurrect" if variant == "javascript_cli" else "cantona.herdr-revive",
                   HERDR_RESURRECT_DEBOUNCE="86400000", HERDR_RESURRECT_KEEP="20",
                   BENCH_BASELINE=str(self.baseline))
        return env

    def command(self, variant, operation):
        if variant == "javascript_cli":
            script = {"save": "save.js", "preview": "restore.js"}.get(operation, "on-event.js")
            return [NODE, str(self.baseline / "bin" / script)] + (["--dry-run"] if operation == "preview" else [])
        return [str(BINARY), operation if operation in ("save", "preview") else "event"]

    def invoke(self, variant, operation):
        self.configure(variant, operation)
        return checked(self.command(variant, operation), env=self.environment(variant), cwd=ROOT)

    def verify_save(self, variant):
        saved = json.loads(self.latest[variant].read_text())
        observed = {}
        if variant == "javascript_cli":
            for workspace in saved["workspaces"]:
                for tab in workspace["tabs"]:
                    for pane in tab["panes"]:
                        if pane.get("agent"):
                            agent = pane["agent"]
                            value = (pane["cwd"], "agent", [agent["name"], agent["session"]["value"]])
                        elif pane.get("command"):
                            if not pane["command"]["restorable"]:
                                raise AssertionError("baseline policy unexpectedly denies a fixture")
                            value = (pane["cwd"], "program", pane["command"]["argv"])
                        else:
                            value = (pane["cwd"], "idle", None)
                        observed[pane["pane_id"]] = value
        else:
            for pane in saved["panes"]:
                command = pane["command"]
                if command is None:
                    value = (pane["cwd"], "idle", None)
                elif command["kind"] == "agent":
                    value = (pane["cwd"], "agent", [command["agent"], command["session_id"]])
                else:
                    value = (pane["cwd"], "program", command["argv"])
                observed[pane["pane_id"]] = value
        if self.empty_arg_probe:
            if set(observed) != set(self.expected):
                raise AssertionError("probe lost panes")
            self.probe_results[variant] = dict(exact_argv_preserved=observed == self.expected,
                expected_argc=[len(v[2]) for v in self.expected.values()], captured_argc=[len(v[2]) for v in observed.values()])
        elif observed != self.expected:
            differences = [pane for pane in set(observed) | set(self.expected) if observed.get(pane) != self.expected.get(pane)]
            raise AssertionError(f"{variant} captured different fixture commands/cwds/IDs: {differences}")

    def verify_preview(self, variant, output):
        expected = {pane for pane, value in self.expected.items() if value[1] != "idle"}
        if variant == "javascript_cli":
            candidates = set(re.findall(r"^\s+run\s+(\S+)\s+<-", output, re.MULTILINE))
        else:
            candidates = {p["pane_id"] for p in json.loads(output)["result"]["plan"]["entries"] if p["decision"] == "candidate"}
        if candidates != expected:
            raise AssertionError(f"{variant} preview candidate IDs differ from the fixture")


def completed_syscalls(lines):
    pending = {}
    for line in lines:
        prefix = re.match(r"^(\d+)\s+(.*)$", line)
        pid, body = prefix.groups() if prefix else ("single", line)
        resumed = re.match(r"<\.\.\. (\w+) resumed>(.*)", body)
        if resumed:
            key = (pid, resumed[1])
            if key not in pending:
                raise AssertionError("strace resumed a syscall without its entry record")
            body = pending.pop(key) + resumed[2]
        elif body.endswith("<unfinished ...>"):
            name = body.split("(", 1)[0]
            pending[(pid, name)] = body.removesuffix("<unfinished ...>")
            continue
        yield body


def audit(fixture, variant, operation):
    fixture.configure(variant, operation)
    trace = fixture.root / f"trace-{variant}-{operation}.log"
    checked(["strace", "-f", "-qq", "-e", "trace=execve,connect", "-o", str(trace),
             *fixture.command(variant, operation)], env=fixture.environment(variant), cwd=ROOT)
    raw = trace.read_bytes()
    lines = list(completed_syscalls(raw.decode().splitlines()))
    programs = [m.group(1) for line in lines if re.search(r"= 0$", line)
                for m in [re.search(r'execve\("([^\"]+)"', line)] if m]
    counts = Counter(Path(p).name for p in programs)
    result = dict(herdr_children=counts[Path(fixture.env["HERDR_BIN_PATH"]).name],
                  process_scan_children=counts["ps"],
                  socket_connects=sum("connect(" in line and str(fixture.socket) in line for line in lines),
                  successful_execs=len(programs), trace_sha256=hashlib.sha256(raw).hexdigest())
    expected = 0
    if operation == "save" and variant != "rust_direct":
        expected = fixture.count + 1
    elif operation == "preview":
        expected = 1 + fixture.shape["workspaces"] + fixture.shape["tabs"] if variant == "javascript_cli" else int(variant == "rust_cli")
    if result["herdr_children"] != expected:
        raise AssertionError(f"unexpected subprocess count for {variant}/{operation}: {result}")
    if result["process_scan_children"] != int(variant == "javascript_cli" and operation == "save"):
        raise AssertionError("unexpected process-enumeration subprocess count")
    return result


def tracked_files(baseline):
    return [Path(p) for p in checked(["git", "-C", str(baseline), "ls-files", "-z"]).stdout.split("\0") if p]


def source_digest(baseline, files):
    digest = hashlib.sha256()
    for path in sorted(files):
        digest.update(str(path).encode() + b"\0")
        digest.update((baseline / path).read_bytes())
    return digest.hexdigest()


def summarize(samples, rng):
    walls = sorted(s["wall_ms"] for s in samples)
    bootstrap = sorted(statistics.median(rng.choices(walls, k=len(walls))) for _ in range(2000))
    return dict(samples=len(samples), median_ms=statistics.median(walls),
                p95_ms=walls[math.ceil(0.95 * len(walls)) - 1], min_ms=walls[0], max_ms=walls[-1],
                median_bootstrap_95_ms=[bootstrap[49], bootstrap[1949]],
                median_rss_mib=statistics.median(s["rss_kib"] for s in samples) / 1024,
                median_cpu_ms=statistics.median((s["user_s"] + s["system_s"]) * 1000 for s in samples))


def main():
    global BINARY
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--baseline", type=Path, default=ROOT.parent / "herdr-resurrect")
    parser.add_argument("--output", type=Path, default=ROOT / "benchmark-results/matched.json")
    parser.add_argument("--samples", type=int, default=30)
    parser.add_argument("--panes", type=int, nargs="+", default=[1, 10, 50, 100])
    args = parser.parse_args()
    if args.samples < 30 or any(n < 1 or n > 100 for n in args.panes):
        parser.error("at least 30 samples and pane counts within 1..100 are required")
    baseline = args.baseline.resolve(strict=True)
    if checked(["git", "-C", str(baseline), "status", "--porcelain"]).stdout:
        parser.error("baseline must be clean for reproducible provenance")
    herdr = shutil.which("herdr")
    if not herdr or not NODE or not BINARY.is_file():
        parser.error("installed Herdr and built release binary are required")
    baseline_source = baseline
    baseline_commit = checked(["git", "-C", str(baseline), "rev-parse", "HEAD"]).stdout.strip()
    files = tracked_files(baseline)
    baseline_digest = source_digest(baseline, files)
    binary_hash = hashlib.sha256(BINARY.read_bytes()).hexdigest()
    with ExitStack() as stack:
        root = Path(stack.enter_context(tempfile.TemporaryDirectory(prefix="herdr-matched-")))
        pinned_binary = root / "herdr-revive"
        shutil.copy2(BINARY, pinned_binary)
        BINARY = pinned_binary
        baseline = root / "baseline"
        baseline.mkdir()
        for path in files:
            (baseline / path).parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(baseline_source / path, baseline / path)
        if source_digest(baseline, files) != baseline_digest or hashlib.sha256(BINARY.read_bytes()).hexdigest() != binary_hash:
            raise AssertionError("pinned input does not match starting provenance")
        programs, shell = build_programs(root)
        fixtures = []
        for count in args.panes:
            print(f"Preparing real {count}-pane mixed fixture", flush=True)
            fixtures.append(Fixture(stack, root / f"panes-{count}", count, baseline, programs, shell, herdr))
        cases = [(f"{f.count}_{v}_{op}", f, v, op) for f in fixtures for v in VARIANTS
                 for op in OPERATIONS if op in ("save", "preview") or f == fixtures[0]]
        rows = {name: [] for name, *_ in cases}
        audits = {}
        for name, fixture, variant, operation in cases:
            audits[name] = audit(fixture, variant, operation)
        schedule = cases * args.samples
        random.Random(20260918).shuffle(schedule)
        load_start = os.getloadavg()
        started = time.monotonic()
        for index, (name, fixture, variant, operation) in enumerate(schedule):
            fixture.configure(variant, operation)
            timing = root / "time.json"
            start = time.perf_counter_ns()
            result = checked(["/usr/bin/time", "-f", '{"rss_kib":%M,"user_s":%U,"system_s":%S}',
                              "-o", str(timing), *fixture.command(variant, operation)],
                             env=fixture.environment(variant), cwd=ROOT)
            elapsed = (time.perf_counter_ns() - start) / 1e6
            measurement = json.loads(timing.read_text())
            measurement.update(wall_ms=elapsed, order=index)
            rows[name].append(measurement)
            if operation == "save":
                fixture.verify_save(variant)
            elif operation == "preview":
                fixture.verify_preview(variant, result.stdout)
            elif variant != "javascript_cli" and json.loads(result.stdout)["result"]["status"] != "debounced":
                raise AssertionError("event benchmark left the completed-boot/debounce path")
            if (index + 1) % 100 == 0:
                print(f"Measured {index + 1}/{len(schedule)} launches ({time.monotonic()-started:.1f}s)", flush=True)
        load_end = os.getloadavg()
        probe = Fixture(stack, root / "empty-argument-probe", 1, baseline, programs, shell, herdr, empty_arg_probe=True)
        rng = random.Random(20260918)
        machine = platform.uname()._asdict()
        machine.pop("node", None)
        if (source_digest(baseline, files) != baseline_digest
                or source_digest(baseline_source, files) != baseline_digest
                or checked(["git", "-C", str(baseline_source), "rev-parse", "HEAD"]).stdout.strip() != baseline_commit
                or checked(["git", "-C", str(baseline_source), "status", "--porcelain"]).stdout
                or hashlib.sha256(BINARY.read_bytes()).hexdigest() != binary_hash):
            raise AssertionError("benchmark inputs changed during measurement")
        output = dict(schema=1, date_utc=time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
                      machine=machine, load_start=load_start, load_end=load_end, seed=20260918,
                      baseline_commit=baseline_commit, baseline_source_sha256=baseline_digest,
                      binary_sha256=hashlib.sha256(BINARY.read_bytes()).hexdigest(), binary_bytes=BINARY.stat().st_size,
                      node=checked([NODE, "--version"]).stdout.strip(), rustc=checked(["rustc", "--version"]).stdout.strip(),
                      host=fixtures[0].api("ping"), fixtures=[f.shape for f in fixtures],
                      empty_argument_probe=probe.probe_results,
                      samples=rows, summary={name: dict(**summarize(samples, rng), **audits[name]) for name, samples in rows.items()},
                      method=dict(host="same private real Herdr server per pane count; all size fixtures coexist during timing",
                                  provenance="private pinned copies; source/binary hashes verified before and after", cache="warm; each operation warmed independently",
                                  schedule="all variants, operations and sizes randomly interleaved", samples_per_case=args.samples,
                                  timings="fresh process; GNU time plus process launch; no strace during timing",
                                  rss="GNU time max RSS of command/waited children; not aggregate/PSS; server excluded",
                                  audit="one separate strace invocation per case; child counts and socket connects",
                                  verification="every saved argv/cwd/agent ID and preview candidate ID checked against expected fixture",
                                  config="wildcard fixture policy; retention20; debounce1day; settle0; precompleted boot claims"),
                      limitations=["unchanged implementations differ in durability, validation and layout capture; not a language-only comparison",
                                   "CPU values rounded by GNU time; host work excluded", "host load observed, not controlled; bootstrap intervals reflect within-run variation",
                                   "no cold-cache, real-agent readiness, restoration latency or aggregate-memory claim"])
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(output, indent=2) + "\n")
        print(json.dumps(dict(output=str(args.output), summary=output["summary"]), indent=2))


if __name__ == "__main__":
    main()
