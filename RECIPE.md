# Herdr Revive

**Reboot it. Crash it. Quit it. It all comes back.** After a reboot, a
crash, or Herdr and herdr-core closing, the programs on your allowlist start
again in their own folders and every agent picks up its exact conversation,
before the pane's first prompt. In a matched benchmark it saved 8.9x faster
than herdr-resurrect (100 panes, median), with no daemon running. New in
0.3.0: move a whole session between the herdr TUI and the desktop app, either
way, on this machine or another reached with `hc`.

A dialog, opened with `ctrl-alt-r` (`cmd-alt-r` on macOS), shows what
herdr-revive last saved from your herdr-core terminals, which of those
programs the next core start would relaunch, and whether the shell hook and
config are in place; three buttons save now, relaunch idle panes now, or read
the facts again. Its Migrate section copies the TUI session into the app or
the app's spaces into the TUI: it shows the plan first, then copies tabs,
splits, folders and commands, and agents resume their exact session.

![Herdr Revive](preview.png)

## Install

```sh
cargo install --locked --git https://github.com/cantona/herdr-revive
herdr-revive install             # shows the plan, asks y/N
```

`install` writes `main.js` and `mod.json` (built into the binary) to
`~/.config/herdr-app/mods/herdr-revive`, adds the two bindings to
`~/.config/herdr-app/keys.json` and the shell hook to the rc file.
`herdr-revive uninstall` reverses it. `keys.json` must exist already (open
the app once); once it is edited, herdr-app stops adding new default
shortcuts to it, and install rewrites its formatting with 2-space
indentation.

## What you see

One dialog, 560px wide, drawn on the theme's `popover` color with 18px
padding and 8px corners. Text is 13px.

- **Title row:** "Herdr Revive" in 15px semibold `foreground`, with a dim
  11px "Esc" hint on the right.
- **Body** (scrolls when long), in sections headed by an 11.5px `dim` label:
  - **Last snapshot:** the snapshot's local date and time on the left; on the
    right "N of M panes with a command" in `muted`. Without a snapshot:
    "No snapshot yet. Run an allowed program in a pane, or press Save now."
  - **Next core start:** one row per saved pane: terminal id (or pane id),
    program (or "shell") and folder on the left, truncated; the decision on
    the right: "will relaunch" (`done`), "not in allowed_programs"
    (`blocked`), "pane is gone" and "moved to another tab" (`dim`), "nothing
    to relaunch" (`muted`). Hidden when there is no snapshot or it has no
    panes.
  - **Recovery:** only when a relaunch stopped part way: a `blocked` line
    "A relaunch stopped in state <state>; saves and relaunches wait until it
    is acknowledged." and an "Acknowledge without retry" button.
  - **Migrate:** between Recovery and Setup. A dim "Migrate" label, then one chip per machine ("This
    computer" first; temporary machines left out; the chosen one filled with
    `solid`), then two buttons, "Copy TUI session here" and "Copy app spaces
    to TUI". A button runs `herdr-revive migrate --to app|tui --json
    --dry-run` (plus `--machine` for another machine) and the section turns
    into the plan: a heading, then one row per source with its tab, pane and
    agent counts and `create`, `unchanged, copy <id>` or `skip: reason` on the right,
    notes below a row in `blocked` (agents still live in the TUI, a copy that
    stopped part way, the report's `notes`: an earlier copy under a reused
    TUI workspace id, a run that may have left a space, a gone folder, the
    copied agents still running in the app) or `dim` (commands that open a shell, mod panes left
    out), a `dim` line that split sizes are not copied when copying into the
    app, then "Cancel" and "Copy N" ("Nothing to copy" when there is nothing
    to create). "Copy N" runs the same command with `--yes`, limited with
    `--only` to the sources the plan showed, and toasts "Copied N of M."; if it stops, the toast says how many were copied before, unless
    that is unknown (`applied` is `null`). When the app stops a copy at
    the 600 s limit, a `warning` toast says, for a copy into M's TUI, "The
    copy may still finish on M; open the plan again to see what was
    copied.", and for a copy into the app, "The copy stopped part way.
    Close what it left in the app before copying again, then open the plan
    again." The report's `warnings` follow in
    a `warning` toast either way, and the plan lists its own below the rows
    (`blocked`, "Warning: ...").
    While the dry run runs the section reads "Planning..." and while `--yes`
    runs "Copying..." (both `muted`). A failed plan read or copy posts an
    error toast ("herdr-revive migrate failed" or "migrate stopped"); a click
    on "Nothing to copy" posts the info toast "Nothing to copy."
  - **Setup:** "Shell hook in <rc file>" with "installed" (`done`),
    "missing" or "update needed" (`blocked`, followed by the command
    `herdr-revive install`), or "unreadable" (`blocked`,
    followed by the error text). Then "Config <dir>/config.toml" with
    "present" (`done`) or "defaults" (`muted`); "auto_save / auto_restore"
    with "on" / "off" each; "allowed_programs" as a comma list or "none".
- **Button row**, right-aligned, 8px apart: "Refresh", "Save now" (both
  `raised` with an `input` border) and "Relaunch idle panes" (`solid` with
  `solid_foreground` text). Buttons are 30px tall, 12px side padding, 8px
  corners, 12.5px medium text. "Relaunch idle panes" asks first: a `blocked`
  line says how many panes will relaunch and that each command is typed into
  its pane's prompt, after anything typed there and not entered; "Cancel"
  and "Relaunch" then replace it.

Other bodies replace the sections: "Reading..." while status runs; "Another
herdr-revive operation is running. Refresh in a moment." while another run
holds the session lock; or one of these error bodies:

- "herdr-revive could not be started." (`blocked`), then "Install the
  current version, then open this dialog again:",
  `cargo install --locked --git https://github.com/cantona/herdr-revive`
  and the raw start error in small `dim` text.
- "herdr-revive is too old for this dialog." when the binary's output says
  `unexpected argument '--host'` or `unrecognized subcommand` (a release from
  before the core host), then the same install lines and the error line in
  small `dim` text.
- Any other nonzero exit, in `blocked`: the output's last line starting
  with `herdr-revive:` that is not `herdr-revive: warning:`, else its line
  starting with `error:`, else its first three non-empty lines, else
  "herdr-revive exited with code N".
- "unreadable status output: <parse error>" when the output is not JSON.
- A timeout or other `exec` rejection, shown as its message (for example
  "herdr-revive took longer than 30 s and was stopped").

## How it behaves

- `ctrl-alt-r` (`cmd-alt-r` on macOS, where Option types characters instead of
  acting as Alt) opens the dialog; pressing it again, or Esc, closes it.
- On open it runs status once. Nothing refreshes on a timer.
- A button shows "..." while its command runs, and all buttons dim to 60%
  while a command or a status read runs; clicks on any button are ignored
  until both the command and the status read after it finish. A command
  that exits nonzero posts an error toast titled "herdr-revive save failed"
  (or restore, ack) with its last `herdr-revive:` line that is not a
  warning, else its `error:` line, else its first three non-empty lines,
  else "exited with code N".
- "Refresh" shows "Reading..." and runs status again; it too is ignored
  while a read or command runs.
- Migrate never changes the source. The plan is read fresh on each click;
  "Copy N" applies what the plan showed, after the binary plans again.
- Buttons fade slightly on hover. The mod shows no agent statuses (working,
  blocked, done, idle); it only reports what herdr-revive returns.

## Built from

- `mods/herdr-revive/mod.json`:
  `{ "place": "none", "title": "herdr-revive", "overlay": { "kind": "dialog", "width": 560 } }`.
  No layout block; `mod.open` shows it as a dialog.
- `keys.json`: `{ "global": { "ctrl-alt-r": "mod.open:herdr-revive", "cmd-alt-r": "mod.open:herdr-revive" } }`,
  merged into the person's own `keys.json` by `herdr-revive install`, which
  never changes a key bound to another action, in `global` or in a
  `terminal` or `layout:<name>` section that takes precedence.
- `mods/herdr-revive/main.js`, one `View`:
  - `herdr-revive --host core status` in `cx.spawn`, run through
    `exec("sh", ...)` so it is found as described under Watch out, and
    parsed as JSON. The dialog draws `result.latest`, `result.panes`,
    `result.recovery.pending`, `result.config` and `result.install`; a
    top-level `{"status":"operation_in_progress"}` is the lock message.
  - The buttons run `herdr-revive --host core save`,
    `herdr-revive --host core restore --rehydrate` and
    `herdr-revive --host core recovery acknowledge <pending.generation>`.
  - `notify` for failures, `closeOverlay(props().overlay)` on Esc, `theme()`
    for every color, `cx.focus_handle()` with `track_focus` and
    `on_key_down` for Esc.
- The mod runs one program, `herdr-revive`, and reads nothing else: no files,
  no network, no `hc`. No art.

## Watch out

- The dialog reads `~/.config/herdr-revive/config.toml`. To share the TUI
  plugin's config, link the directory:
  `ln -s ~/.config/herdr/plugins/config/cantona.herdr-revive ~/.config/herdr-revive`.
  A symlinked `config.toml` itself is refused.
- The dialog runs `herdr-revive` from the app's PATH, else from
  `~/.local/bin` or `~/.cargo/bin`, as the rc line does: an app started
  from the desktop often has a bare PATH. Installed anywhere else, link it
  into `~/.local/bin`.
- Status is read once per open and after each button, never on a timer, so
  the dialog does not follow saves made elsewhere until Refresh.
- `exec` inherits the app's `HC_SESSION`. A core started with another session
  name is not the one the dialog reads or acts on.
- A pending recovery makes Save and Relaunch fail until it is acknowledged;
  the dialog shows the Recovery section and the toast says why.
- The relaunch button passes `--rehydrate` to state that it only relaunches
  programs in panes that still exist and never rebuilds closed spaces. On
  the core host a plain `restore` does the same, because core snapshots
  store no layout to rebuild from.
- On herdr-app 0.1.13 `exec` has a fixed 30 s limit; the `timeout` option
  the mod passes is honored only by later app versions. A relaunch that
  takes longer is stopped and reported as failed.
- The status output must stay JSON with `result` at the top level; the lock
  message is the one exception.

## Done when

- `herdr-app check mods/herdr-revive` passes.
- Opening with `ctrl-alt-r` (`cmd-alt-r` on macOS) shows the last snapshot's time.
- "Save now" changes that time.
- With `herdr-revive` in none of those places, the dialog shows the
  install command.
- Esc closes the dialog.
