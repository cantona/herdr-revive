#!/usr/bin/env python3
"""Warm costs of the herdr-core host against tests/core_integration.py's fake
core, served from this process. Never starts, stops or contacts a real hc: every
run gets a temporary HOME, XDG directories, socket and HC_SESSION. Linux only:
it reads /proc and runs setsid and GNU time -f."""
import argparse
import errno
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import pty
import random
import select
import signal
import statistics
import subprocess
import sys
import time

ROOT = Path(__file__).resolve().parents[1]
os.environ["REVIVE_BINARY"] = str(
    Path(os.environ.get("REVIVE_BINARY", ROOT / "target/release/herdr-revive")).resolve())
sys.path.insert(0, str(ROOT / "tests"))
from core_integration import BASH, BINARY, PROMPT, Fixture

TIME = "/usr/bin/time"
TIME_FORMAT = '{"rss_kib":%M,"user_s":%U,"system_s":%S}'
SESSION = f"revive-bench-{os.getpid()}"
SEED = 20261107
SAVE_PANES = (1, 10, 100)

# The rc file a benchmark shell reads: it waits until its pid is registered
# with the fake core, then times the same command substitution the installed
# rc line evaluates. `exec` keeps the pane's shell as init's parent.
INIT_RC = r"""read -r __gate < "$BENCH_GATE"
__t0=$EPOCHREALTIME
__out=$(exec @PROGRAM@ 2>/dev/null)
__t1=$EPOCHREALTIME
printf '%s\n%s\n%s' "$__t0" "$__t1" "$__out" > "$BENCH_OUT"
exit 0
"""
INIT_PROGRAM = "herdr-revive --host core init bash"


def version(argv):
    try:
        return subprocess.run(argv, capture_output=True, text=True, timeout=10,
                              check=True).stdout.strip()
    except (OSError, subprocess.SubprocessError) as error:
        return f"unavailable: {error}"


def cpu_model():
    try:
        for line in Path("/proc/cpuinfo").read_text().splitlines():
            if line.startswith("model name"):
                return line.split(":", 1)[1].strip()
    except OSError:
        pass
    return platform.processor() or None


class InitCase:
    """Starts one interactive bash per sample on a fresh pty, as core does
    for a new pane, and reads the time and output its rc file recorded."""

    def __init__(self, fx, name, program, expect):
        self.fx = fx
        self.name = name
        self.expect = expect
        self.rc = fx.root / f"{name}.rc"
        self.rc.write_text(INIT_RC.replace("@PROGRAM@", program))
        self.gate = fx.root / f"{name}.gate"
        os.mkfifo(self.gate)
        self.out = fx.root / f"{name}.out"
        self.timing = fx.root / f"{name}.time"
        self.argv = [TIME, "-f", TIME_FORMAT, "-o", str(self.timing),
                     "setsid", "-c", BASH, "--rcfile", str(self.rc), "-i"]

    def prepare(self):
        for path in self.fx.boots():
            path.unlink()
        for path in (self.out, self.timing):
            path.unlink(missing_ok=True)

    def sample(self):
        self.prepare()
        address = "t1"
        master, slave = pty.openpty()
        env = self.fx.env(HC_TERMINAL_ID=address, HC_SESSION=SESSION, LC_ALL="C",
                          BENCH_GATE=str(self.gate), BENCH_OUT=str(self.out))
        try:
            proc = subprocess.Popen(self.argv, stdin=slave, stdout=slave, stderr=slave,
                                    cwd=self.fx.work, env=env)
        finally:
            os.close(slave)
        shell = None
        try:
            shell = self.wait_shell(proc)
            self.fx.core.attach(address, self.fx.record(address, shell), master)
            self.open_gate(proc)
            self.drain(master, proc)
        except BaseException:
            try:
                self.cleanup(proc, shell, address, master)
            except Exception as error:
                print(f"{self.name}: cleanup after a failure also failed: {error!r}",
                      file=sys.stderr)
            raise
        self.cleanup(proc, shell, address, master)
        if proc.returncode:
            raise AssertionError(f"{self.name}: shell exited {proc.returncode}\n" + self.fx.describe())
        start, end, output = self.out.read_text().split("\n", 2)
        self.expect(output, self.fx)
        measurement = json.loads(self.timing.read_text())
        measurement["wall_ms"] = (float(end) - float(start)) * 1000
        return measurement

    def cleanup(self, proc, shell, address, master):
        try:
            self.fx.core.detach(address)
            if proc.poll() is None:
                if shell is not None:
                    try:
                        os.killpg(shell, signal.SIGKILL)
                    except ProcessLookupError:
                        pass
                proc.kill()
                proc.wait(timeout=5)
        finally:
            os.close(master)

    def wait_shell(self, proc):
        """The pid of bash once setsid, started by time, has executed it."""
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            for pid in pgrep_children(proc.pid):
                try:
                    if Path(f"/proc/{pid}/comm").read_text().strip() == "bash":
                        return pid
                except OSError:
                    continue
            if proc.poll() is not None:
                break
            time.sleep(0.002)
        raise AssertionError(f"{self.name}: no bash under {proc.pid}")

    def open_gate(self, proc):
        deadline = time.monotonic() + 5
        while True:
            try:
                gate = os.open(self.gate, os.O_WRONLY | os.O_NONBLOCK)
            except OSError as error:
                if error.errno != errno.ENXIO or proc.poll() is not None \
                        or time.monotonic() > deadline:
                    raise AssertionError(f"{self.name}: shell never opened its gate") from error
                time.sleep(0.002)
                continue
            try:
                os.write(gate, b"go\n")
            finally:
                os.close(gate)
            return

    def drain(self, master, proc):
        deadline = time.monotonic() + 15
        while proc.poll() is None:
            if time.monotonic() > deadline:
                raise AssertionError(f"{self.name}: shell did not exit\n" + self.fx.describe())
            try:
                if select.select([master], [], [], 0.05)[0]:
                    os.read(master, 65536)
            except OSError:
                pass
        proc.wait(timeout=5)


def pgrep_children(pid):
    result = subprocess.run(["pgrep", "-P", str(pid)], capture_output=True, text=True, timeout=5)
    if result.returncode not in (0, 1):
        raise AssertionError(f"pgrep -P {pid} exited {result.returncode}: {result.stderr}")
    return [int(p) for p in result.stdout.split()]


class CommandCase:
    """One herdr-revive invocation under GNU time, as scripts/benchmark.py
    measures the TUI host."""

    def __init__(self, fx, name, args, panes):
        self.fx = fx
        self.name = name
        self.panes = panes
        self.timing = fx.root / f"{name}.time"
        self.argv = [TIME, "-f", TIME_FORMAT, "-o", str(self.timing),
                     str(BINARY), "--host", "core", *args]

    def sample(self):
        start = time.perf_counter_ns()
        result = subprocess.run(self.argv, env=self.fx.env(HC_SESSION=SESSION), cwd=self.fx.work,
                                capture_output=True, text=True, timeout=30)
        elapsed = (time.perf_counter_ns() - start) / 1e6
        if result.returncode:
            raise AssertionError(f"{self.name} exited {result.returncode}: {result.stderr}")
        output = json.loads(result.stdout)
        if output["result"].get("status") != "saved":
            raise AssertionError(f"{self.name}: {output}")
        measurement = json.loads(self.timing.read_text())
        measurement.update(wall_ms=elapsed, requests=output.get("requests"))
        return measurement


def expect_hook_only(output, fx):
    if "__revive_preexec" not in output or output.startswith("__revive_relaunch() {") \
            or "__revive_relaunch_cmd=" in output:
        raise AssertionError(f"init without a relaunch printed: {output!r}")


def expect_relaunch(output, fx):
    if "__revive_preexec" not in output or not output.startswith("__revive_relaunch() {") \
            or f"\n cd -- '{fx.work}' && command 'sleep' '600'\n" not in output:
        raise AssertionError(f"init with a relaunch printed: {output!r}\n" + fx.describe())
    boots = fx.boots()
    entries = json.loads(boots[0].read_text())["entries"] if len(boots) == 1 else None
    if not entries or [e["outcome"] for e in entries] != ["applied"]:
        raise AssertionError(f"relaunch journal: {entries}")


def expect_empty(output, fx):
    if output:
        raise AssertionError(f"control printed: {output!r}")


def seed_snapshot(fx):
    """Saves a pane running `sleep 600` through the installed hook, then ends
    that shell like a lost pane."""
    shell = fx.start_shell(extra_env={"HC_SESSION": SESSION})
    shell.wait_output(PROMPT)
    shell.type("sleep 600")
    fx.wait_latest(lambda s: s["panes"][0]["command"] is not None)
    shell.stop()


def save_fixture(panes, fixtures):
    """One real shell at its prompt; the other records reuse its pid."""
    fx = Fixture()
    fixtures.append(fx)
    shell = fx.start_shell(extra_env={"HC_SESSION": SESSION})
    shell.wait_output(PROMPT)
    for index in range(2, panes + 1):
        address = f"t{index}"
        fx.core.attach(address, fx.record(address, shell.pid), shell.master)
    return fx


def summarize(samples):
    walls = sorted(s["wall_ms"] for s in samples)
    summary = dict(
        samples=len(walls),
        p50_ms=round(statistics.median(walls), 3),
        p95_ms=round(walls[math.ceil(0.95 * len(walls)) - 1], 3),
        min_ms=round(walls[0], 3),
        max_ms=round(walls[-1], 3),
        rss_kib=statistics.median(s["rss_kib"] for s in samples),
    )
    if samples[0].get("requests") is not None:
        summary["requests"] = samples[0]["requests"]
    return summary


def close_fixtures(fixtures):
    errors = []
    for fx in fixtures:
        try:
            fx.close()
        except Exception as error:
            errors.append(error)
    return errors


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--samples", type=int, default=30)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    if not sys.platform.startswith("linux"):
        parser.error(f"Linux only (this is {sys.platform}): the benchmark reads /proc and "
                     "needs setsid and GNU time -f")
    if args.samples < 30:
        parser.error("at least 30 independent samples are required")
    if not Path(TIME).is_file():
        parser.error(f"{TIME} (GNU time) is required")
    if not BINARY.is_file():
        parser.error(f"{BINARY} does not exist; run cargo build --release --locked")
    fixtures = []
    try:
        measure(args, fixtures)
    except BaseException:
        for error in close_fixtures(fixtures):
            print(f"fixture cleanup after a failure also failed: {error!r}", file=sys.stderr)
        raise
    errors = close_fixtures(fixtures)
    if errors:
        raise AssertionError(f"fixture cleanup errors: {errors!r}")


def measure(args, fixtures):
    hook_fx = Fixture(auto_save=True, auto_restore=True)
    fixtures.append(hook_fx)
    relaunch_fx = Fixture(auto_save=True, auto_restore=True)
    fixtures.append(relaunch_fx)
    seed_snapshot(relaunch_fx)
    cases = [
        InitCase(hook_fx, "shell_control", "true", expect_empty),
        InitCase(hook_fx, "init_no_relaunch", INIT_PROGRAM, expect_hook_only),
        InitCase(relaunch_fx, "init_relaunch", INIT_PROGRAM, expect_relaunch),
    ]
    for panes in SAVE_PANES:
        fx = save_fixture(panes, fixtures)
        if panes == 1:
            cases.append(CommandCase(fx, "autosave_force_1", ["autosave", "--force"], panes))
        cases.append(CommandCase(fx, f"save_{panes}", ["save"], panes))
    for case in cases:
        case.sample()
        if isinstance(case, CommandCase):
            saved = len(case.fx.latest()["panes"])
            if saved != case.panes:
                raise AssertionError(f"{case.name}: saved {saved} panes, expected {case.panes}")
    schedule = cases * args.samples
    random.Random(SEED).shuffle(schedule)
    rows = {case.name: [] for case in cases}
    load_start = os.getloadavg()
    for case in schedule:
        rows[case.name].append(case.sample())
    load_end = os.getloadavg()
    summaries = {name: summarize(samples) for name, samples in rows.items()}
    machine = platform.uname()._asdict()
    machine.pop("node", None)
    machine.update(cpu=cpu_model(), cpus=os.cpu_count())
    result = dict(
        os=sys.platform,
        date=time.strftime("%Y-%m-%d"),
        samples=args.samples,
        seed=SEED,
        machine=machine,
        versions=dict(
            herdr_revive=version([str(BINARY), "--version"]),
            hc=version(["hc", "--version"]),
            herdr_app=version(["herdr-app", "--version"]),
            rustc=version(["rustc", "--version"]),
            bash=version([BASH, "--version"]).splitlines()[0],
            python=sys.version.split()[0],
        ),
        binary=dict(path=str(BINARY), bytes=BINARY.stat().st_size,
                    sha256=hashlib.sha256(BINARY.read_bytes()).hexdigest()),
        commands=dict(
            run="cargo build --release --locked && REVIVE_BINARY=target/release/herdr-revive "
                "python3 -I scripts/benchmark_core.py --output " + str(args.output),
            init_rc=INIT_RC.replace("@PROGRAM@", INIT_PROGRAM),
            **{case.name: " ".join(case.argv) for case in cases},
        ),
        core="fake: FakeCore from tests/core_integration.py, a Python thread per connection in "
             "the benchmark process; no hc server is started or contacted",
        method=[
            "warm: every case runs once before measuring; samples are interleaved in a "
            "seeded shuffle",
            "init cases: a fresh `bash -i` on a new pty per sample, made session leader "
            "with `setsid -c` and registered with the fake core before it reads its rc file; "
            "the rc file times `$(exec herdr-revive --host core init bash)` with "
            "EPOCHREALTIME, so wall time includes the fork and exec the installed rc line pays",
            "init_relaunch: the snapshot was saved by the hook from a shell running "
            "`sleep 600`; the boot journal is deleted before each sample so the same pane "
            "relaunches again; each sample checks the printed command and an applied "
            "journal entry",
            "init_no_relaunch: auto_save and auto_restore on, no snapshot; prints the hook only",
            "shell_control: the same shell and rc file running `true` instead of herdr-revive",
            "save and autosave cases: herdr-revive invoked directly under GNU time; wall time "
            "is measured around the time process, as scripts/benchmark.py does",
            "save_N: one live bash at its prompt plus N-1 more fake core records reusing "
            "that shell's pid",
        ],
        rss="GNU time maximum RSS. Init cases measure the whole shell tree (setsid, bash, "
            "its subshell and herdr-revive), so compare them with shell_control; command "
            "cases measure herdr-revive alone.",
        limitations=[
            "fake core, not hc: real core latency is not included",
            "extra save records share one shell process",
            "no cold-cache claim",
            "host load uncontrolled",
        ],
        load_start=load_start,
        load_end=load_end,
        cases=summaries,
        samples_ms={name: [round(s["wall_ms"], 3) for s in samples]
                    for name, samples in rows.items()},
    )
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps(dict(output=str(args.output), cases=summaries), indent=2))


if __name__ == "__main__":
    main()
