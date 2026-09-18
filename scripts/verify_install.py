#!/usr/bin/env python3
"""Validate the real manifest and lifecycle with a disposable Herdr server."""
import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import time

ROOT = Path(__file__).resolve().parents[1]
HERDR = shutil.which("herdr")
if not HERDR:
    raise SystemExit("Herdr 0.9.1 must be installed")
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--github-ref", help="also test GitHub build failures and managed reinstall")
args = parser.parse_args()
with tempfile.TemporaryDirectory(prefix="herdr-revive-install-") as tmp:
    root = Path(tmp)
    env = {key: value for key, value in os.environ.items() if not key.startswith("HERDR_")}
    env.update(XDG_CONFIG_HOME=str(root / "config"), XDG_STATE_HOME=str(root / "state"),
               XDG_DATA_HOME=str(root / "data"), XDG_CACHE_HOME=str(root / "cache"),
               HERDR_CONFIG_PATH=str(root / "config/herdr/config.toml"),
               HERDR_SOCKET_PATH=str(root / "nonexistent.sock"))
    env.pop("HERDR_SESSION", None)
    for key in ["HERDR_WORKSPACE_ID", "HERDR_TAB_ID", "HERDR_PANE_ID"]:
        env.pop(key, None)
    shell = root / "fixture-shell"
    shell.write_text('#!/bin/sh\nexec /bin/bash --noprofile --norc "$@"\n')
    shell.chmod(0o700)
    env["SHELL"] = str(shell)

    def run(*command, success=True, run_env=None):
        result = subprocess.run([HERDR, "plugin", *command], env=run_env or env,
                                capture_output=True, text=True, timeout=300)
        if bool(result.returncode == 0) != success:
            raise AssertionError(f"{command}: {result.stdout}\n{result.stderr}")
        return result

    if args.github_ref:
        install = ("install", "cantona/herdr-revive", "--ref", args.github_ref, "--yes")
        missing_bin = root / "without-cargo"
        missing_bin.mkdir()
        (missing_bin / "git").symlink_to(shutil.which("git"))
        missing = run(*install, success=False, run_env=dict(env, PATH=str(missing_bin)))
        assert "cargo" in missing.stderr and "No such file" in missing.stderr, missing.stderr
        assert "cantona.herdr-revive" not in run("list", "--json").stdout
        failing_cargo = missing_bin / "cargo"
        failing_cargo.write_text('#!/bin/sh\necho deliberate-cargo-failure >&2\nexit 42\n')
        failing_cargo.chmod(0o700)
        failed = run(*install, success=False, run_env=dict(env, PATH=str(missing_bin)))
        assert "deliberate-cargo-failure" in failed.stderr, failed.stderr
        assert "cantona.herdr-revive" not in run("list", "--json").stdout
        run(*install)
        config_dir = Path(run("config-dir", "cantona.herdr-revive").stdout.strip())
        assert config_dir.is_relative_to(root), config_dir
        config_marker = config_dir / "preserve-me"
        state_marker = root / "state/herdr/plugins/cantona.herdr-revive/preserve-me"
        config_marker.write_text("durable user configuration")
        state_marker.write_text("durable user state")
        run(*install, success=False, run_env=dict(env, PATH=str(missing_bin)))
        assert "cantona.herdr-revive" in run("list", "--json").stdout
        run(*install)
        assert config_marker.read_text() == "durable user configuration"
        assert state_marker.read_text() == "durable user state"
        installed = json.loads(run("list", "--json").stdout)["result"]["plugins"]
        managed = next(plugin for plugin in installed if plugin["plugin_id"] == "cantona.herdr-revive")
        managed_root = Path(managed["plugin_root"])
        assert managed_root.is_relative_to(root)
        env.update(HERDR_PLUGIN_ROOT=str(managed_root), HERDR_PLUGIN_CONFIG_DIR=str(config_dir),
                   HERDR_PLUGIN_STATE_DIR=str(state_marker.parent), HERDR_PLUGIN_ID="cantona.herdr-revive")
        subprocess.run(["target/release/herdr-revive", "event"], cwd=managed_root, env=env,
                       check=True, capture_output=True)
        subprocess.run(["sh", "scripts/manage.sh"], input="0\n", text=True, cwd=managed_root,
                       env=env, check=True, capture_output=True)
        run("uninstall", "cantona.herdr-revive")
        assert config_marker.exists() and state_marker.exists()
        assert not managed_root.exists()
        for key in ["HERDR_PLUGIN_ROOT", "HERDR_PLUGIN_CONFIG_DIR", "HERDR_PLUGIN_STATE_DIR", "HERDR_PLUGIN_ID"]:
            env.pop(key)
        print(json.dumps(dict(github_ref=args.github_ref, missing_cargo="refused",
            failed_build="refused", failed_reinstall="preserved registration", managed_reinstall="passed",
            managed_runtime_cwd="verified", managed_uninstall="passed", config_and_state_preserved=True)))

    run("link", str(ROOT), "--enabled")
    listing = run("list", "--json").stdout
    assert "cantona.herdr-revive" in listing, listing
    with open(root / "server.log", "w") as log:
        server = subprocess.Popen([HERDR, "server"], env=env, cwd=root, stdout=log, stderr=log)
        try:
            deadline = time.monotonic() + 10
            while not Path(env["HERDR_SOCKET_PATH"]).exists():
                if server.poll() is not None or time.monotonic() >= deadline:
                    raise AssertionError(root.joinpath("server.log").read_text())
                time.sleep(0.05)
            run("enable", "cantona.herdr-revive")
            actions = run("action", "list", "--plugin", "cantona.herdr-revive").stdout
            for action in ["save", "restore", "preview", "autosave", "manage", "list", "save-space", "open-space", "delete-space", "timer"]:
                assert action in actions, actions
            deadline = time.monotonic() + 5
            while True:
                logs = json.loads(run("log", "list", "--plugin", "cantona.herdr-revive").stdout)["result"]["logs"]
                startup = [entry for entry in logs if entry.get("event") == "startup" and entry["status"] != "running"]
                if startup:
                    assert startup[0]["status"] == "succeeded", startup
                    break
                if time.monotonic() >= deadline:
                    raise AssertionError("startup hook did not complete")
                time.sleep(0.05)
            config_dir = Path(run("config-dir", "cantona.herdr-revive").stdout.strip())
            assert config_dir.is_relative_to(root), config_dir
            marker = config_dir / "preserve-me"
            marker.write_text("durable user configuration")
            state_marker = root / "state/herdr/plugins/cantona.herdr-revive/preserve-me"
            state_marker.write_text("durable user state")
            run("disable", "cantona.herdr-revive")
            run("enable", "cantona.herdr-revive")
            run("unlink", "cantona.herdr-revive")
            assert marker.exists()
            assert state_marker.exists()
            run("link", str(ROOT), "--disabled")
            run("uninstall", "cantona.herdr-revive")
            assert marker.exists()
            assert state_marker.exists()
        finally:
            server.terminate()
            try:
                server.wait(timeout=5)
            except subprocess.TimeoutExpired:
                server.kill()
                server.wait(timeout=5)
    incompatible = root / "incompatible"
    incompatible.mkdir()
    incompatible.joinpath("herdr-plugin.toml").write_text(
        ROOT.joinpath("herdr-plugin.toml").read_text().replace('min_herdr_version = "0.9.1"', 'min_herdr_version = "999.0.0"'))
    run("link", str(incompatible), "--disabled", success=False)
    env.update(HERDR_PLUGIN_ROOT=str(ROOT), HERDR_PLUGIN_CONFIG_DIR=str(config_dir),
               HERDR_PLUGIN_STATE_DIR=str(root / "runtime-state"), HERDR_PLUGIN_ID="cantona.herdr-revive")
    subprocess.run(["target/release/herdr-revive", "event"], cwd=ROOT, env=env, check=True, capture_output=True)
    subprocess.run(["sh", "scripts/manage.sh"], input="0\n", text=True, cwd=ROOT, env=env, check=True, capture_output=True)
    print(json.dumps(dict(manifest="accepted by actual Herdr parser", actions=10,
        isolated_link_enable_disable_relink_uninstall="passed", config_and_state_preserved=True,
        incompatible_minimum_refused=True, startup_hook="passed", runtime_cwd="verified", live_registration_changed=False), indent=2))
