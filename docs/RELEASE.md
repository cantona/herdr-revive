# Release, migration and rollback

Version 0.1.3 is a Linux and macOS prerelease of `cantona/herdr-revive`, licensed
under MIT. It is built from source during installation. The manifest advertises
both platforms. See [macOS and Linux regression validation](MACOS.md). Windows
remains planned.

## Build and installation

Requires Rust/Cargo 1.97.1+, a C linker, Git, and Herdr 0.9.1 / protocol 22.
The runtime currently refuses other host version/protocol pairs.

```sh
herdr plugin install cantona/herdr-revive --ref v0.1.3
herdr plugin config-dir cantona.herdr-revive
```

The manifest runs `cargo build --release --locked` before registration. Missing
toolchains and build failures abort installation; Herdr does not install Cargo.
Local `plugin link` does not build. Reinstall the desired GitHub tag to update a
managed installation. Keep config and state in the host-provided directories,
not the replaceable plugin checkout. A locally linked plugin must be unlinked
before installing a managed copy.

Run the README verification commands from a fresh tree. Record the compiler,
target triple, lockfile digest and release binary digest. The lockfile pins
dependency resolution; byte-identical binaries across compilers, build paths
and linkers are not claimed. The release source includes third-party license
texts for the 57 Linux dependency packages, including build-time dependencies.
Regenerate them with `scripts/dependency_notices.py` when dependencies change;
review additional target graphs separately.

## Qualification

[Validation](VALIDATION.md) records automated runtime coverage, actual OpenSSH
and host restart regressions, installation checks and remaining work. The
[matched benchmark](MATCHED_BENCHMARK.md) compares the predecessor on equivalent
private fixtures.
These measurements do not establish remote command readiness or power-loss
behavior. Full-session rollback observation, actual disk-full/power-cut tests,
cold-start measurements and other platforms remain follow-up qualification.

The whole-repository adversarial source review returned NO FINDINGS. Formatting,
Clippy, the locked build, runtime suites and benchmark parser checks passed.
The annotator MCP was unavailable for the separate interactive diff checkpoint.
The actual GitHub installer passed successful build, missing-Cargo, failed-build,
reinstall and uninstall checks in temporary roots. External config/state survived
reinstall and uninstall; a failed reinstall preserved the existing registration.

Public discovery follows the [official marketplace rules](https://herdr.dev/docs/marketplace/):
a non-fork, non-archived public GitHub repository, a parseable root manifest, and
the `herdr-plugin` topic. Indexing is automatic, normally every 30 minutes, and
is not a Herdr review or endorsement. Check the indexed owner, ID, version,
platforms and default-branch commit after publication.

## Migration and canary

First test with a disposable Herdr session, private plugin config/state and
harmless commands. Inspect capture/preview output, restore a known fresh prompt,
and exercise recovery acknowledgement. Before switching a normal session, back
up the predecessor's config/state and existing action bindings.

Registration and enabled state are shared across sessions. Disable the
predecessor's automatic restore hooks before enabling Revive. Create an explicit
program allowlist; automatic save/restore are off by default. Capture a native
Revive snapshot for every session that will use its startup restoration.
Legacy snapshots and configuration are not automatically imported.

For agent startup, disable Herdr's native path in its **main** configuration:

```toml
[session]
resume_agents_on_restore = false
```

Keep Revive's `auto_restore` and `resume_agents` enabled in the plugin config.
The native path uses canonical agent names and loses custom launchers such as
`claude-local`. The host setting applies to all sessions sharing that config;
a session without a Revive snapshot will not gain one through this setting.
Configure custom launcher selectors before capture, as shown in the README.

Update save/restore bindings to the IDs in [the feature matrix](PARITY.md).
Verify save, preview and the intended restore in one session before relying on
other sessions. Stop on unresolved recovery evidence and inspect the affected
panes; acknowledgement does not undo or repeat commands.

## Rollback

Disable `cantona.herdr-revive`. Restore the predecessor's configuration and
action bindings from backup, merging later edits, and re-enable it as needed.
Restore Herdr's native agent-resume setting if returning to native restoration.
Keep both plugins' state and any pending evidence. Disabling a plugin does not
stop commands it already launched.

`plugin unlink cantona.herdr-revive` removes registration and leaves the local
checkout. `plugin uninstall cantona.herdr-revive` also removes a managed checkout;
external config/state are retained. Do not delete old snapshots as part of
cutover. A full rollback of long-running normal sessions has not been qualified.
