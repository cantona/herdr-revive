#!/usr/bin/env python3
"""Benchmark native save/preview/events on private real Herdr mixed-pane hosts.

Uses the matched benchmark's fixtures and exact saved-command assertions,
without requiring the JavaScript baseline or Linux strace.
"""
import argparse
from contextlib import ExitStack
import hashlib
import json
import os
from pathlib import Path
import platform
import random
import re
import shutil
import statistics
import subprocess
import sys
import tempfile
import time

from benchmark_matched import BINARY, ROOT, Fixture, build_programs, summarize


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--samples", type=int, default=30)
    parser.add_argument("--panes", type=int, nargs="+", default=[1, 10, 50, 100])
    parser.add_argument("--output", type=Path, default=ROOT / "benchmark-results/native.json")
    parser.add_argument("--baseline-binary", type=Path,
                        help="interleave an existing baseline binary on the same fixtures")
    parser.add_argument("--include-autosave", action="store_true",
                        help="also measure forced autosave with existing snapshots")
    parser.add_argument("--events-only", action="store_true",
                        help="measure event launches without interleaved save/preview work")
    args = parser.parse_args()
    if args.samples < 30 or any(n < 1 or n > 100 for n in args.panes):
        parser.error("at least 30 samples and pane counts within 1..100 are required")
    herdr = shutil.which("herdr")
    if not herdr or not BINARY.is_file():
        parser.error("installed Herdr and a built release binary are required")
    binary_hash = hashlib.sha256(BINARY.read_bytes()).hexdigest()
    binaries = {"candidate": BINARY}
    if args.baseline_binary:
        binaries["baseline"] = args.baseline_binary.resolve(strict=True)
    hashes = {name: hashlib.sha256(path.read_bytes()).hexdigest() for name, path in binaries.items()}
    with ExitStack() as stack:
        root = Path(stack.enter_context(tempfile.TemporaryDirectory(prefix="revive-bench-", dir="/tmp"))).resolve()
        programs, shell = build_programs(root)
        fixtures = []
        for count in args.panes:
            print(f"Preparing {count} real mixed panes", flush=True)
            fixtures.append(Fixture(stack, root / str(count), count, root, programs, shell,
                                    herdr, variants=tuple(binaries)))
        # Each binary owns its state directory. Strict newer snapshots may add
        # fields an older comparison binary intentionally rejects; sharing one
        # latest.json would benchmark cross-version parsing, not runtime cost.
        for fixture in fixtures:
            for variant, binary in binaries.items():
                fixture.configure(variant, "save")
                command = fixture.command(variant, "save")
                command[0] = str(binary)
                subprocess.run(command, env=fixture.environment(variant), cwd=ROOT,
                               check=True, capture_output=True, text=True, timeout=60)
                fixture.verify_save(variant)
        operations = ("save", "preview", "autosave") if args.include_autosave else ("save", "preview")
        cases = [] if args.events_only else [(f"{f.count}_{op}", f, op) for f in fixtures for op in operations]
        cases += [(op, fixtures[0], op) for op in ("event_debounced", "event_boot_done")]
        cases = [(f"{variant}_{name}" if args.baseline_binary else name, f, op, variant)
                 for name, f, op in cases for variant in binaries]
        rows = {name: [] for name, *_ in cases}
        schedule = cases * args.samples
        random.Random(20260919).shuffle(schedule)
        load_start = os.getloadavg()
        for name, fixture, operation, variant in schedule:
            fixture.configure(variant, operation)
            command = fixture.command(variant, operation)
            command[0] = str(binaries[variant])
            if operation == "autosave":
                command[1:] = ["autosave", "--force"]
            timing = root / "time.json"
            if sys.platform == "darwin":
                command = ["/usr/bin/time", "-l", *command]
            else:
                command = ["/usr/bin/time", "-f", '{"rss_kib":%M,"user_s":%U,"system_s":%S}',
                           "-o", str(timing), *command]
            start = time.perf_counter_ns()
            result = subprocess.run(command, env=fixture.environment(variant), cwd=ROOT,
                                    check=True, capture_output=True, text=True, timeout=60)
            elapsed = (time.perf_counter_ns() - start) / 1e6
            if sys.platform == "darwin":
                cpu = re.search(r"([\d.]+) real\s+([\d.]+) user\s+([\d.]+) sys", result.stderr)
                rss = re.search(r"(\d+)\s+maximum resident set size", result.stderr)
                if not cpu or not rss:
                    raise AssertionError("unexpected BSD time output")
                measurement = dict(rss_kib=int(rss[1]) / 1024, user_s=float(cpu[2]), system_s=float(cpu[3]))
            else:
                measurement = json.loads(timing.read_text())
            output = json.loads(result.stdout)
            measurement.update(wall_ms=elapsed, requests=output["requests"],
                               herdr_children=output["herdr_children"],
                               identity_connections=output["identity_connections"])
            assert measurement["herdr_children"] == 0
            rows[name].append(measurement)
            if operation in ("save", "autosave"):
                fixture.verify_save(variant)
            elif operation == "preview":
                fixture.verify_preview(variant, result.stdout)
            else:
                assert output["result"]["status"] == "debounced"
        assert all(hashlib.sha256(path.read_bytes()).hexdigest() == hashes[name]
                   for name, path in binaries.items()), "binary changed during timing"
        machine = platform.uname()._asdict()
        machine.pop("node", None)
        rng = random.Random(20260919)
        result = dict(machine=machine, date_utc=time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
                      binary_sha256=binary_hash, binary_bytes=BINARY.stat().st_size,
                      compared_binary_sha256=hashes,
                      load_start=load_start, load_end=os.getloadavg(),
                      host=fixtures[0].api("ping"), fixtures=[f.shape for f in fixtures],
                      summary={name: summarize(samples, rng) for name, samples in rows.items()}, samples=rows,
                      method="30+ fresh launches per case; warm cache; randomized schedule; real mixed panes; exact argv/cwd/session assertions; direct transport",
                      limitations=["No comparative fastest claim or Linux/macOS hardware comparison",
                                   "Wall time includes system time utility and process launch; host load uncontrolled",
                                   "RSS excludes the Herdr server; no cold-cache or real-agent readiness measurement",
                                   "Save/preview/event overhead only; not end-to-end resume latency"])
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(result, indent=2) + "\n")
        print(json.dumps(dict(output=str(args.output), summary=result["summary"]), indent=2))


if __name__ == "__main__":
    main()
