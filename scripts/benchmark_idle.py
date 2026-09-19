#!/usr/bin/env python3
"""Measure restore's idle-shell check without host I/O or command startup."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import random
import statistics
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "tests"))
from integration import Fixture


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--baseline-binary", type=Path)
    parser.add_argument("--samples", type=int, default=30)
    parser.add_argument("--checks", type=int, default=100)
    parser.add_argument("--output", type=Path, default=ROOT / "benchmark-results/idle.json")
    args = parser.parse_args()
    if args.samples < 30 or not 1 <= args.checks <= 10000:
        parser.error("30+ samples and 1..10000 checks required")
    binaries = {"candidate": ROOT / "target/release/examples/benchmark_idle"}
    if args.baseline_binary:
        binaries["baseline"] = args.baseline_binary.resolve(strict=True)
    hashes = {name: hashlib.sha256(path.read_bytes()).hexdigest() for name, path in binaries.items()}
    fixture = Fixture()
    try:
        samples = {name: [] for name in binaries}
        schedule = list(binaries) * args.samples
        random.Random(20260919).shuffle(schedule)
        load_start = os.getloadavg()
        for name in schedule:
            result = subprocess.run([str(binaries[name]), str(fixture.pid), str(args.checks)],
                                    capture_output=True, text=True, timeout=60)
            if result.returncode:
                raise RuntimeError(f"{name} idle check failed: {result.stderr.strip()}")
            sample = json.loads(result.stdout)
            assert sample["checks"] == args.checks
            samples[name].append(sample["elapsed_ms"])
        assert all(hashlib.sha256(path.read_bytes()).hexdigest() == hashes[name]
                   for name, path in binaries.items())
        machine = platform.uname()._asdict()
        machine.pop("node", None)
        result = dict(machine=machine, checks_per_sample=args.checks, binary_sha256=hashes,
                      load_start=load_start, load_end=os.getloadavg(), samples_ms=samples,
                      summary={name: dict(median_ms=statistics.median(rows), min_ms=min(rows),
                                          max_ms=max(rows)) for name, rows in samples.items()},
                      method="randomized fresh helper launches; elapsed time inside the idle-shell API loop",
                      limitations=["Repeated checks of one real idle shell; no host API or command startup",
                                   "Not end-to-end restore latency; host process count/load uncontrolled"])
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(result, indent=2) + "\n")
        print(json.dumps(result["summary"], indent=2))
    finally:
        fixture.close()


if __name__ == "__main__":
    main()
