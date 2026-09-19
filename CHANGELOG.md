# Changelog

## 0.1.3 — 2026-09-19 (prerelease)

- Keep exact agent sessions across automatic shell-state saves, while manual
  saves can intentionally clear them. Retry delayed native metadata, refresh
  capture after waiting, and avoid dropping contending lifecycle events silently.
- Bypass debounce for agent detection and changed session references; preserve
  the ordinary debounced-event and exact-ID capture fast paths.
- Refuse stale native references that conflict with a newly detected explicit
  resume until matching metadata arrives; preserve in-process session switching.
- Preserve untouched bare Claude sessions with their assigned UUID by
  relaunching them with `--session-id`; switch to exact `--resume` as soon as
  Claude has written a nonempty transcript.
- Keep `CLAUDE_CONFIG_DIR` unset for standard Claude and preserve its captured
  `HOME`; retain explicitly selected profiles for launchers such as `claude-local`.
- Resolve the active Claude profile from the captured process environment and
  check the canonical project transcript directly on ordinary paths, with a
  bounded exhaustive fallback for continuation, explicit session-ID selection,
  and unusual path cases.
- Resume exact saved agent sessions when Herdr retains stale agent metadata
  after a restart. The live process tree remains authoritative, so panes with
  real foreground or background work are still skipped.
- Add empty-session transition, legacy-snapshot, automatic-startup, regression,
  and paired pre-fix v0.1.3 performance coverage on macOS and Linux.
- Omit redundant resume-mode fields and reuse encoded snapshot bytes for
  history/latest writes, preserving archive IDs and synchronization guarantees.

## 0.1.2 — 2026-09-19 (prerelease)

- Preserve exact bare-agent sessions when an agent internally redirects
  stdout or stderr to `/dev/null`, including Codex on macOS.
- Accept Git's single plain system-shell wrapper around system `less` or
  `more`, so an active pager in another pane cannot block atomic autosave.
  Shell expansions, operators, pipelines, queued commands, and redirected
  pager output remain refused.
- Add Linux and macOS live-session, restart, security, regression, and
  before/after performance validation for the affected paths.

## 0.1.1 — 2026-09-19 (prerelease)

- Native macOS process/argv/cwd and launcher-environment capture, terminal and
  null-stream validation, OpenSSH output recovery, and Apple Git pager support.
- Authenticated macOS server generation and kqueue autosave shutdown handling.
- Split Linux and macOS adapters into `src/platform/linux.rs` and `macos.rs`.
  Darwin FFI exceptions are limited to individual checked calls; other code
  continues to deny unsafe code.
- Preserve direct transport with no process-inspection subprocesses. Idle checks
  use direct child enumeration on macOS and per-thread child lists on Linux,
  with the original Linux scan retained as a fallback. Newly created panes
  retry transient shell metadata reads within their existing readiness deadline.
- Portable native runtime tests and real mixed-pane performance measurements.

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
