#!/usr/bin/env python3
"""Warm synthetic fixtures; does not access live Herdr sessions."""
import argparse
import concurrent.futures
import json
import hashlib
import math
import os
from pathlib import Path
import platform
import random
import statistics
import subprocess
import sys
import time

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "tests"))
os.environ["REVIVE_BINARY"] = str(ROOT / "target/release/herdr-revive")
from integration import BINARY, Fixture


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--samples", type=int, default=30)
    parser.add_argument("--output", type=Path, default=ROOT / "benchmark-results/results.json")
    parser.add_argument("--include-cli", action="store_true")
    args = parser.parse_args()
    if args.samples < 30:
        parser.error("at least 30 independent samples are required")
    fixtures = []
    cases = []
    try:
        for transport in (["direct", "cli"] if args.include_cli else ["direct"]):
            for panes in [1, 10, 50, 100]:
                fixture = Fixture(transport=transport, panes=panes)
                fixtures.append(fixture)
                fixture.run("save")
                fixture.run("preview")
                for operation in ["save", "preview"]:
                    cases.append((f"{transport}_{panes}_{operation}", fixture, operation))
        for mode in ["disabled", "debounced", "boot_done"]:
            fixture = Fixture()
            fixtures.append(fixture)
            fixture.run("save")
            fixture.write_config(auto_save=(mode != "disabled"), auto_restore=(mode == "boot_done"))
            fixture.run("event")
            cases.append((f"event_{mode}", fixture, "event"))
        schedule = cases * args.samples
        random.Random(20260918).shuffle(schedule)
        rows = {name: [] for name, _, _ in cases}
        load_start = os.getloadavg()
        for name, fixture, operation in schedule:
            timing = fixture.root / "time.json"
            start = time.perf_counter_ns()
            result = subprocess.run(["/usr/bin/time", "-f",
                '{"rss_kib":%M,"user_s":%U,"system_s":%S}', "-o", str(timing),
                str(BINARY), operation], env=fixture.env(), capture_output=True, text=True, check=True)
            elapsed = (time.perf_counter_ns() - start) / 1e6
            measurement = json.loads(timing.read_text())
            output = json.loads(result.stdout)
            measurement.update(wall_ms=elapsed, logical_requests=output.get("requests", 0),
                herdr_children=output.get("herdr_children", 0), identity_connections=output.get("identity_connections", 0))
            rows[name].append(measurement)
        summaries = {}
        for name, samples in rows.items():
            walls = sorted(s["wall_ms"] for s in samples)
            summaries[name] = dict(samples=len(samples), median_ms=statistics.median(walls),
                p95_ms=walls[math.ceil(0.95*len(walls))-1], min_ms=walls[0], max_ms=walls[-1],
                median_rss_mib=statistics.median(s["rss_kib"] for s in samples)/1024,
                logical_requests=samples[0]["logical_requests"], herdr_children=samples[0]["herdr_children"],
                identity_connections=samples[0]["identity_connections"])
        fixture = fixtures[-1]
        start = time.perf_counter_ns()
        with concurrent.futures.ThreadPoolExecutor(max_workers=20) as pool:
            list(pool.map(lambda _: fixture.run("event"), range(100)))
        burst_ms = (time.perf_counter_ns() - start)/1e6
        machine = platform.uname()._asdict()
        machine.pop("node", None)
        result = dict(machine=machine, load_start=load_start, load_end=os.getloadavg(),
            binary_bytes=BINARY.stat().st_size, binary_sha256=hashlib.sha256(BINARY.read_bytes()).hexdigest(), rustc=subprocess.check_output(["rustc", "--version"], text=True).strip(),
            cache="warm; save and preview independently warmed", seed=20260918,
            wall_includes="GNU time and process launch; fixture server time included",
            rss="GNU time maximum process RSS; includes waited children for CLI; not aggregate memory",
            limitations=["synthetic idle panes share one PTY", "no matched JavaScript comparison", "no cold-cache claim",
                         "CPU is coarsely rounded", "host load uncontrolled", "burst aggregate memory not measured"],
            event_burst_100_wall_ms=burst_ms, summary=summaries, samples=rows)
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(result, indent=2) + "\n")
        print(json.dumps(dict(output=str(args.output), summary=summaries), indent=2))
    finally:
        for fixture in fixtures:
            fixture.close()


if __name__ == "__main__":
    main()
