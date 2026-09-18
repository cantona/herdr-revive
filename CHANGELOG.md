# Changelog

## 0.1.0 — 2026-09-18 (prerelease)

- Documented and tested the required handoff from Herdr's native agent restore
  to Revive. Native restore generates canonical `claude` commands and cannot
  preserve `claude-local`, even when the Revive snapshot is correct.

- Buffered process reads and JSON output; busy restore skips redundant whole-host
  process scans while keeping candidate idle-shell checks.
- Corrected package, executable and plugin identity to `herdr-revive`, preserving
  existing snapshot reads and boot claims across the local rename.
- Environment-selected custom agent launchers preserve exec-style wrappers such
  as `claude-local` with exact session IDs and current configuration checks.
- Real `top`, `htop`, `journalctl` and `tail` restoration coverage; Git's internal
  terminal pager is captured as the original Git command.

- Independent MIT-licensed Rust implementation and plugin identity.
- Linux process/argv capture, versioned native snapshots, retention and explicit
  reusable global named spaces and optional mapping imports.
- Explicit `/dev/null` stdout/stderr capture and restoration for programs,
  with original executable policy enforcement. OpenSSH uses its actual session
  output descriptor; its internally nulled fd 1 is never replayed as a redirection.
- CLI and direct local JSON transports with bounded I/O and protocol checks.
- Pure exact-ID planning, execution-time policy/busy checks and POSIX encoding.
- Shared startup/event/autosave claims, durable failure evidence and explicit
  recovery acknowledgement without replay.
- Full layout reconstruction, repeated named-space opens, preserved labels,
  nested splits, cwd, tab order and focus; safe handling of large/deep layouts.
- Repeatable manual restores, missing-workspace recreation, and explicit
  rehydrate/recreate modes with selected-history snapshots.
- Five-agent exact resume, configurable program matching and reviewed extra argv.
- Opt-in event automation, foreground periodic/final autosave, ten actions,
  dedicated space popups, optional fzf selection and a snapshot manager.
- Linux fixtures, real-shell argument tests, disposable host lifecycle checks,
  dependency notices and warm benchmark harness.

This Linux prerelease includes automated runtime and installation checks.
Power-loss qualification, full-session rollback observation, macOS and Windows
remain follow-up work; see [validation](docs/VALIDATION.md).
