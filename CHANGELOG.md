# Changelog

## 0.2.1 — 2026-11-09 (prerelease)

- `herdr-revive install` sets up the Herdr desktop app in one step: it
  writes the dialog mod (`main.js`, `mod.json`, built into the binary) to
  `~/.config/herdr-app/mods/herdr-revive` (or under `HERDR_APP_HOME` or
  `XDG_CONFIG_HOME`), adds `ctrl-alt-r` and `cmd-alt-r` to the `global`
  section of `keys.json`, and
  adds the rc hook. It prints a plan (`create`, `update`, `unchanged`,
  `skip` with a reason) and asks first; `--yes` applies without asking,
  `--dry-run` only prints. A key bound to another action, in `global` or
  in a `terminal` or `layout:<name>` section and however it is spelled, is
  left as it is, key order is kept, and a missing `keys.json` is never
  created (it is the app's whole keymap). A `keys.json` that is invalid or
  cannot be written skips the keys and the rest still applies. An rc or mod
  file owned by another user or with a leftover backup stops the plan
  before anything is written, and running as root with a user's HOME is
  refused. `herdr-revive uninstall` removes what `install` added, plus the
  editor files the app writes into the mod folder, and nothing else. Both
  imply `--host core`.
- Bare `install` used to print the rc dry run as JSON; it now prints the
  plan and, without a terminal or `--yes`, exits 2 without changing
  anything. `install --write` and `install --remove` still apply, as
  `install --yes` and `uninstall --yes`, and print the plan as text instead
  of JSON. The dialog's Setup hint now reads `herdr-revive install`.

## 0.2.0 — 2026-11-09 (prerelease)

- A pane whose foreground program is protected from inspection (setuid, such
  as Apple's `top` or `sudo`) no longer fails the whole save. It is saved with
  the pane's cwd and the program's full command line on Linux; on macOS, the
  bare program name, or the line the bash hook saw the shell run when it is
  plain words. A warning names the pane.
- Run against the Herdr desktop app (herdr-core) with `--host core`: the
  same capture, policy, snapshots, journal and recovery over a second host.
  A bash hook saves when an allowlisted program starts or ends in a pane; a
  lost pane's fresh shell relaunches its program before the first prompt,
  journaled per pane per core start. `REVIVE_AUTO=0` in a pane's environment
  skips its relaunch. New commands `init bash`, `install [--write|--remove]`
  and `status`, and a dialog mod under `mods/herdr-revive` (`RECIPE.md`,
  `keys.json` binding `ctrl-alt-r`, `cmd-alt-r` on macOS). Named spaces and `restore --recreate`
  stay TUI-only; `event` on the app only saves.
- Direct invocations outside the Herdr plugin environment, except `config`,
  now need `--host herdr|core` (or `HERDR_REVIVE_HOST`); before, they assumed
  the TUI host. The plugin manifest and scripts pass `--host herdr`, so the plugin's
  actions and events are unchanged.

## 0.1.6 — 2026-10-15 (prerelease)

- Save Claude Code panes that reopened a background session as
  `claude attach <short id>`. Capture now looks the id up in
  `claude agents --json --all`, using the pane's `CLAUDE_CONFIG_DIR`, `HOME`
  and `PATH`, and saves the pane as `--resume <uuid>`; before, the save failed
  and the last snapshot went stale. The resolved session takes precedence over
  Herdr's record, which can name another pane's session because the daemon's
  hooks report to the pane that started it. An attach that cannot be resolved
  falls back to Herdr's record when there is one.

## 0.1.5 — 2026-10-07 (prerelease)

- Save panes of agents Revive cannot resume as ordinary programs instead of
  failing the save. A pane reporting an agent such as `annotator` (the
  herdr-file-annotator review pane), opencode or pi no longer blocks autosave;
  restore runs it only if allowlisted, freshly. Claude, Codex, Gemini, Copilot
  and Cursor behind an unrecognized launcher still fail capture.
- Keep status events of non-resumable agents debounced, so they no longer write
  a snapshot per status change and rotate retained snapshots out.

## 0.1.4 — 2026-10-07 (prerelease)

- Accept every Herdr release from `min_herdr_version` (0.9.1) onward. The
  runtime no longer refuses hosts other than 0.9.1 / protocol 22; on Herdr
  0.9.3 that check failed startup and lifecycle events, so autosave and the
  startup restore stopped without a visible error. Herdr enforces the minimum
  version at install and link time.

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
