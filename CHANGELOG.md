# Changelog

## 0.3.0 — 2026-11-10 (prerelease)

- `herdr-revive migrate --to app` copies a herdr TUI session into Herdr
  desktop app spaces, and `--to tui` copies the app's spaces into a TUI
  session: workspaces or spaces and tabs with their names, the split
  layout, each pane's cwd and command, and agents resumed. The source is
  never changed. It prints a plan like `install` (`create`, `skip` with a
  reason, `unchanged` when an earlier copy still exists) and asks first.
  Runs that overlap on one machine are safe, `--machine` runs started
  there included (a run holds the migration ledger's lock from before it
  lists the destination); two machines must not migrate to or from the
  same machine at the same time.
  `--yes` applies, `--dry-run` only prints, `--json` (with one of those)
  prints a report. Into the app, split sizes are not kept (the app's
  commands take none); into the TUI, the layout is rebuilt with `restore
  --recreate`'s journal and mod panes are left out. `--session` picks the
  TUI session, `--only` the sources, and `--machine` copies between another
  machine's TUI and its app spaces, which needs herdr-revive 0.3.0 there
  and refuses `--socket`, `--config-dir`, `--state-dir` and `--herdr-bin`.
  `--host` is refused. Each copy is recorded in
  `~/.local/state/herdr-revive/migrated.json`, and a source that is itself
  a copy from the other side is skipped while its original exists. A copy
  into M's app made with `--machine M` is recorded on M too, so a migrate
  started on M, or from a third machine, knows it: as a partial copy as
  soon as the app names its space, then whole (or as the partial copy it
  stopped at) when the build ends. A run killed in the moment between the
  app naming the space and M recording it leaves M without the copy, and
  one killed after this machine records the whole copy but before M does
  leaves M with the partial copy only; this machine knows the copy either
  way. When M cannot record it, the copy stands and a warning names the
  space and the command to run on M. The `--json` report lists
  the run's warnings (`warnings`), and the dialog shows them. After the
  TUI numbers its workspaces again (`restore --recreate` after a restart),
  earlier copies still open are found by a terminal they share or by a
  label only one workspace has, so no second copy is made; the plan says
  which workspace id the copy had and, for a match by the label alone,
  how to copy the workspace anyway. With
  `--only`, only the named sources are read, and a workspace or space
  that cannot be captured is a `skip` line ("cannot capture: ...") while
  the others are copied. A copy into the app that fails
  part way stays recorded until its space and terminals are closed, and
  until then the plan skips that source as "partial copy S still open:
  close it first". One into the TUI leaves
  `rebuild-pending.json`, which pauses the TUI plugin's saves and restores
  until it is acknowledged; the error prints the exact `recovery inspect`
  and `recovery acknowledge` commands with the `--host herdr`,
  `--config-dir`, `--state-dir` and `--socket` they need (to run on M for
  `--machine M`); one into the app that timed out, or was stopped (Ctrl-C,
  killed, the dialog's time limit), is a partial copy of what it had made,
  recorded as each step was made, or, stopped before the app named its
  space, is named with its time as one that may have left a space. A TUI workspace is recorded with its
  terminal ids and label, so a workspace that later takes its id with
  another label is planned as a copy that names the earlier one, not as
  `unchanged`, while one restored after a TUI restart stays `unchanged`
  with a note that it matches only by its label. Every copied command
  starts through the user's login shell, so it finds programs on the login
  PATH that herdr-core lacks, and the pane keeps its own shell when the
  command ends or is stopped with Ctrl-C. A pane whose folder no longer
  exists opens a shell in the home folder without its command, and the plan
  names the folder. A space copied into two TUI sessions is recorded for
  each. An unnamed app space takes the
  window's name for it, else its folder's name.
- `--host core save` and the core relaunch now record an idle shell pane's
  cwd as the folder its shell is in, which herdr-core reads from the
  foreground process, instead of the folder the terminal started in. Where
  core cannot read it, or the folder it names is gone, the start folder is
  used as before; a folder removed and made again at the same path counts
  as there.
- The app dialog gets a Migrate section: a machine picker and "Copy TUI
  session here" / "Copy app spaces to TUI", each showing the plan before it
  copies; "Copy N" copies only the sources that plan showed. herdr-revive
  started from the dialog now also finds `herdr`, `hc` and `herdr-app` in
  `~/.local/bin` and `~/.cargo/bin`.

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
