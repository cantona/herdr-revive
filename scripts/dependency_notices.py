#!/usr/bin/env python3
"""Regenerate license notices from the locked Linux dependency source packages."""
import json
from pathlib import Path
import subprocess

ROOT = Path(__file__).resolve().parents[1]
metadata = json.loads(subprocess.check_output(["cargo", "metadata", "--format-version", "1",
    "--locked", "--offline", "--filter-platform", "x86_64-unknown-linux-gnu"], cwd=ROOT))
parts = ["# Third-party notices\n\nGenerated from Cargo.lock for the Linux build, including build-time dependencies. "
         "Original herdr-revive code is MIT. Dependency licenses remain with their authors. "
         "The table records full upstream SPDX expressions; license texts are preserved below. "
         "Review again for any additional release target.\n\n| Package | Version | Upstream license |\n| --- | --- | --- |\n"]
packages = sorted((p for p in metadata["packages"] if p["source"]), key=lambda p: p["name"])
for package in packages:
    parts.append(f"| {package['name']} | {package['version']} | {package['license']} |\n")
for package in packages:
    root = Path(package["manifest_path"]).parent
    licenses = sorted(p for p in root.iterdir() if p.is_file() and (
        p.name.upper().startswith(("LICENSE", "COPYING", "NOTICE"))))
    if not package.get("license") or not licenses:
        raise SystemExit(f"Manual license review required for {package['name']}")
    parts.append(f"\n## {package['name']} {package['version']}\n")
    for path in licenses:
        parts.append(f"\n### {path.name}\n\n```text\n{path.read_text().rstrip()}\n```\n")
ROOT.joinpath("THIRD_PARTY_NOTICES.md").write_text("".join(parts))
print(f"Preserved license texts for {len(packages)} dependency packages")
