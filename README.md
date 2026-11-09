# herdr-revive

Bring your commands back after a Herdr restart. Save the programs, working
directories, layouts and exact agent conversations in your panes, then restore
them from retained snapshots or reusable named workspaces.

An independent MIT-licensed Rust plugin by Su Kang Yin.
Package/executable: `herdr-revive`; plugin: `cantona.herdr-revive`.
**Supports Linux and macOS.** Windows remains planned.
It also runs against the Herdr desktop app; see [Herdr desktop app](#herdr-desktop-app).

## Why this plugin exists

Herdr already restores workspace layout and working directories. It also resumes
supported agents when an official integration reports a valid session reference.
Ordinary programs return as fresh shells: your SSH connection, `top`,
`journalctl -f` or development server still needs to be started again.
[Herdr's restore documentation](https://herdr.dev/docs/session-state/) describes
this boundary. Detaching while the server stays alive needs no restore plugin.

Revive adds allowlisted command relaunch, selectable snapshot history, preview,
reusable named spaces, event saves and a periodic autosave pane. It also preserves
configured agent wrappers such as `claude-local`, so an exact conversation ID
reopens through the correct profile. Commands start again; their process memory,
network connections and unsaved editor buffers are not checkpointed.

## Built-in vs herdr-resurrect vs tmux plugins

The Herdr plugin comparison uses
[cantona/herdr-resurrect at c05b43d](https://github.com/cantona/herdr-resurrect/tree/c05b43dda3ff968cb21653b7f4188245015a15be),
including that fork's exact-ID and session-isolation fixes. Some older sections
of its README describe fallback behavior its current code has already removed.
The tmux column combines [tmux-resurrect](https://github.com/tmux-plugins/tmux-resurrect)
and [tmux-continuum](https://github.com/tmux-plugins/tmux-continuum); these operate
inside tmux, so they are alternatives at the terminal-multiplexer level.

| Capability | Herdr built-in | herdr-resurrect fork | herdr-revive | tmux-resurrect + continuum |
| --- | --- | --- | --- | --- |
| Layout, cwd and focus after restart | Yes | Yes | Yes | Yes, in tmux |
| Relaunch ordinary pane programs | No | Allowlist | Allowlist | Configurable program list |
| Agent conversation restore | Official native integrations; broader agent coverage | Five agents, exact references | Five agents, exact references | General command/custom strategies; no Herdr reference integration |
| Custom agent launchers | Canonical agent commands | Resume-template overrides; canonical executable | Environment-selected wrappers and reviewed extra arguments | Custom command mappings/strategies |
| Snapshot history and manual selection | Current shape; recovery backups | Yes | Yes | Yes |
| Automatic saving | Native session state | Events and timer pane | Events and timer pane | Continuum interval via tmux status line |
| Automatic command restore | Native agents | Opt-in | Opt-in | Continuum opt-in |
| Terminal contents | Optional native pane history | Uses host capability | Uses host capability | Optional pane contents |
| Vim/Neovim session integration | No editor-session strategy | Relaunch command | Relaunch command | Optional session-file strategy |
| Plugin runtime | Included in Herdr | Node.js | Native binary; shell for popups | Bash and tmux |
| Platform scope | Herdr's supported platforms | Manifest lists Linux/macOS/Windows | Linux and macOS | Upstream reports Linux/macOS/Cygwin |

See the [detailed replacement matrix](docs/PARITY.md) for named spaces, actions,
layout reconstruction and migration differences. Revive does not import the
predecessor's snapshots or configuration automatically.

## Where Revive improves the Herdr workflow

- **Lower measured save and hook overhead.** Direct socket requests avoid a
  Node startup and a Herdr CLI process for each pane. The matched measurements
  below compare equivalent work, including command and layout checks.
- **Explicit recovery after an interrupted restore.** Synced mutation journals
  preserve uncertain delivery and block further save/restore until acknowledged.
  A lost response is not automatically retried. The fork also prevents automatic
  replay after a failed restore; Revive extends this with persistent per-mutation
  evidence and a shared operation lock.
- **Correct agent profile selection.** Configured environment selectors retain
  an exec wrapper such as `claude-local` alongside the exact session UUID, without
  saving the process environment. Current policy is checked again before launch.
- **Inspect the intended change.** Preview command eligibility, rehydrate exact
  existing pane IDs, or explicitly rebuild a saved layout as a new workspace.
  Both this plugin and the compared fork support exact-ID automatic restoration.

Use the built-in restore if layouts and its supported agents cover your needs.
Use Revive for command restoration and reusable workspaces in Herdr with the
recovery controls above. If you work in tmux or need Vim/Neovim session-file and
pane-content restoration, the tmux plugins provide features Revive does not.
No performance comparison against Herdr built-in or the tmux plugins is claimed.

### Measured against herdr-resurrect

Matched Linux run with the reviewed 0.1.0 binary: 100 mixed panes, 30 launches per case, identical
private Herdr sessions. Revive uses its default direct transport.

| Operation / metric | herdr-resurrect | herdr-revive | Measured difference |
| --- | ---: | ---: | ---: |
| Save, median | 363.82 ms | 40.69 ms | 8.9× faster |
| Preview, median | 161.65 ms | 7.43 ms | 21.8× faster |
| Save, median maximum process RSS | 72.57 MiB | 5.28 MiB | 92.7% lower |
| Debounced event, median | 95.25 ms | 1.42 ms | 67.1× faster |

The [benchmark report and raw samples](docs/MATCHED_BENCHMARK.md) give p95,
input hashes, all pane counts and reproduction commands. The measurements cover
warm local operations, not end-to-end restore readiness, and compare complete
implementations rather than Rust versus JavaScript in isolation.

Existing-pane restoration still has a host check/send race, and an idle process
does not prove an empty shell prompt. See [recovery limits](#recovery-and-safety-limits)
and [validation](docs/VALIDATION.md) before enabling automatic restoration.

## Installation

Requires Linux with readable `/proc` or macOS, Rust/Cargo **1.97.1+**, a C linker, and
Herdr **0.9.1 or newer**, declared as `min_herdr_version` and enforced by Herdr at
install and link time. A host method revive calls that a newer Herdr no longer
offers fails the operation with an error (Herdr's details are redacted). A
running agent whose session can be identified neither from Herdr's metadata nor
from a resume ID on its command line fails the save and keeps the previous
snapshot.
No Node or Python runtime is required. The popup uses POSIX `sh`, with optional
`fzf` for saved-space selection. Python 3 is used only by development scripts.
On macOS, install Xcode Command Line Tools (`xcode-select --install`) for the linker.

Install the tagged source release; Herdr previews the manifest and builds it:

```sh
herdr plugin install cantona/herdr-revive --ref v0.2.1
herdr plugin config-dir cantona.herdr-revive
```

Create `config.toml` in the printed directory using the example below. Automatic
behavior starts off, and commands require an explicit allowlist. For updates,
repeat `plugin install` with the desired tag; managed reinstalls preserve external
configuration and state. A locally linked installation must be unlinked first.

For local development:

```sh
cargo build --release --locked
target/release/herdr-revive --help
target/release/herdr-revive config
```

The manifest builds with `cargo build --release --locked`. Herdr does not install
missing toolchains, and `plugin link` does not build. A local registration can be made with:

```sh
herdr plugin link /absolute/path/to/herdr-revive --disabled
herdr plugin config-dir cantona.herdr-revive
```

Registration is shared across sessions. Development tests use temporary
config/state roots and their own disposable server. Do not enable both
implementations' automatic restore hooks during cutover.

## Configuration

Copy [config.example.toml](config.example.toml) to `config.toml` in the injected
plugin config directory. Missing configuration uses defaults; invalid values,
unknown keys, or unreadable configuration fail explicitly.

```toml
auto_restore = false
auto_save = false
settle_ms = 2500
debounce_ms = 5000
interval_seconds = 900
retention = 10
transport = "direct"
allowed_programs = ["ssh", "minicom", "top", "htop", "journalctl", "tail", "git", "less", "claude", "claude-local", "codex", "gemini", "copilot", "cursor-agent"]
match_program_basename = true
resume_agents = true
```

Both automatic features default off. An empty allowlist denies all commands.
When enabling Revive's automatic agent restoration, also put this in **Herdr's
main `config.toml`**, separate from the plugin configuration:

```toml
[session]
resume_agents_on_restore = false
```

Herdr's native agent restore defaults on and launches canonical agent names.
It cannot preserve a wrapper such as `claude-local`; running both restore paths
can launch ordinary `claude` before Revive gets to the pane. Keep Revive's
`auto_restore = true` and `resume_agents = true` to let it restore the saved
launcher and exact session. The host setting applies to every session using
that config, so those sessions need their own Revive snapshots. Re-enable the
host setting if migrating back to native agent restoration.

By default entries match `argv[0]` exactly, including absolute paths; optional
basename matching is case-insensitive. `"*"` explicitly trusts all executables.
General programs and development tools, including interpreters, are supported;
the allowlist is not a sandbox. Arguments to an allowed program remain trusted.
Policy is checked again immediately before execution. Denied commands become
fresh shells when rebuilding a workspace and are skipped during rehydration.

`agent_extra_args` accepts reviewed per-agent option/value pairs and boolean
flags. Session selectors, prompts and unknown flags are rejected. See
[contracts](docs/CONTRACTS.md) for the supported options.

An exec-style wrapper disappears from process argv after launching its agent.
To restore through a custom wrapper, configure a nonsecret environment selector
that distinguishes it from ordinary agent sessions:

```toml
[[agent_launchers]]
agent = "claude"
executable = "claude-local"
match_env = { CLAUDE_CONFIG_DIR = "/home/you/.claude-local" }
```

Use your actual absolute config path; TOML values do not expand `~` or `$HOME`.
The wrapper must also be in `allowed_programs`. Capture saves the launcher and
exact session ID, without copying the process environment. Multiple matches or
duplicate selector keys fail capture. Removing the rule denies old snapshots
that name the custom launcher. Historical snapshots captured without a rule
need recapture; the original wrapper cannot be inferred from `claude` alone.
For an untouched bare Claude session, capture also requires the live
`CLAUDE_CONFIG_DIR` to match the profile Revive can derive for restoration. Add
that path to the launcher selector when a wrapper uses a nondefault profile;
Revive refuses the save rather than risk creating the UUID in the wrong profile.
The snapshot records the resolved profile path and whether it was explicitly
selected. Standard Claude restores with its captured `HOME` and
`CLAUDE_CONFIG_DIR` unset, preserving its normal `~/.claude.json` lookup.
Explicit profiles, including configured `claude-local` launchers, keep their
selected `CLAUDE_CONFIG_DIR`.

## Commands

Herdr supplies `HERDR_PLUGIN_ROOT`, `HERDR_PLUGIN_CONFIG_DIR`,
`HERDR_PLUGIN_STATE_DIR`, `HERDR_SOCKET_PATH`, and `HERDR_BIN_PATH`.
Explicit CLI overrides are `--config-dir`, `--state-dir`, `--socket`, and
`--herdr-bin`; paths must be absolute. No fallback to another session occurs.
Outside the plugin environment pass `--host herdr` (or set
`HERDR_REVIVE_HOST=herdr`), or name the paths with `--config-dir`,
`--state-dir` or `--socket`, which imply the herdr host; the plugin's own
actions and events already pass `--host herdr`.

| Command | Behavior |
| --- | --- |
| `save` | Save the session's layout, cwd and commands, with retained history |
| `preview [--snapshot FILE]` | Show exact-ID decisions and missing-workspace reconstruction without mutation |
| `restore [--snapshot FILE] [--dry-run]` | Rehydrate eligible existing panes and rebuild missing workspaces |
| `restore --rehydrate` | Only rehydrate exact existing pane IDs |
| `restore --recreate` | Rebuild every saved workspace as a new workspace; preserve existing workspaces |
| `list` | List retained session snapshot paths; `--file` aliases `--snapshot` on restore/preview |
| `event` | Once-per-boot automatic restore, then configured debounced autosave |
| `autosave [--force]` | Configured save; `--force` bypasses debounce and performs save only |
| `timer [--interval-seconds N]` | Foreground periodic save, including a final save on SIGINT/SIGTERM/SIGHUP (not on the core host) |
| `space save NAME [--workspace ID]` | Save a reusable workspace; injected workspace ID is the default |
| `space preview NAME` | Show reconstruction and command policy decisions |
| `space open NAME [--dry-run] [--no-focus]` | Open a new copy every time; `restore` is an alias for `open` |
| `space list` / `space names` / `space delete NAME` | Manage the shared named-space library |
| `space import NAME FILE [--mapping MAP.json]` | Import a native named snapshot without executing commands |
| `recovery inspect` | Inspect pending restore/reconstruction evidence |
| `recovery acknowledge ID` | Preserve evidence and unblock operations without retrying commands |

Manual restores are repeatable requests. Automatic startup/events share one
claim per server boot and never recreate missing IDs. Missing panes/tabs inside
an existing workspace are shown as missing; use explicit `--recreate` to make
a complete new copy. Preview cannot establish shell readiness.

Automatic saves retain the last exact agent session when the same pane, tab,
workspace and directory temporarily return to a shell. This also applies to
the timer and `autosave --force`. To intentionally forget that session, use
manual **Save** while the pane is at its shell prompt. A different captured
command or directory replaces the retained session; closed panes are not kept.
Agent detection and changed session IDs bypass debounce. Missing native IDs
receive a bounded retry; failure leaves the previous snapshot intact.

Actions under `cantona.herdr-revive` are `save`, `preview`, `restore`, `list`,
`autosave`, `timer`, `manage`, `save-space`, `open-space`, and `delete-space`.
The manager offers session history selection, named spaces, and confirmation
before restore/delete. Saved-space pickers use `fzf` when available, otherwise
a name prompt. The optional `autosave` pane runs the timer; it is not a daemon.
The timer's explicit saves are independent of `auto_save` and never initiate
automatic restore. Event autosave uses leading-edge debounce, not a trailing queue.

## Herdr desktop app

The same binary runs against the desktop app's terminals (herdr-core, `hc`)
with `--host core`, or `HERDR_REVIVE_HOST=core` in its environment. Capture,
policy, snapshots, the journal and recovery are the code the TUI plugin runs;
only the host and the triggers differ. On the app a save happens when an
allowlisted program starts or ends in a pane (a bash hook), and a relaunch
happens per pane, in the fresh shell core gives a lost pane, once its whole rc
file has run and before its first prompt is drawn, as long as the rc file
does not replace `PROMPT_COMMAND` below the herdr-revive line (see below).
Each pane is relaunched at most once per core start; the journal records it.
With `auto_restore` on, until a pane's own shell has recorded its relaunch (or
that there was none to do) in the journal, automatic saves keep that pane's
entry from the snapshot the recovery started with while core still lists its
terminal and the pane is either not live yet or its shell is still reading
its rc file (the shell's own process group in the foreground with rc commands
in it). This keeps a save made while the shell is starting from replacing the
command. An idle prompt or any program in the pane, a shell started from the
prompt included, ends this for that pane at once, and so does a manual `save`
for every pane.

Setup (bash only):

```sh
cargo install --locked --git https://github.com/cantona/herdr-revive
herdr-revive install             # shows the plan, asks y/N
```

`install` prints its plan, one line per item with `create`, `update`,
`unchanged` or `skip` (followed by the reason), and asks before it changes
anything. The
items are the dialog mod (`main.js` and `mod.json`, built into the binary)
in `~/.config/herdr-app/mods/herdr-revive`, the `ctrl-alt-r` and
`cmd-alt-r` bindings in the `global` section of `~/.config/herdr-app/keys.json`
(`HERDR_APP_HOME`, or `herdr-app` under `XDG_CONFIG_HOME`, replaces
`~/.config/herdr-app`), and the rc hook described
below. `--dry-run` prints the plan only; `--yes` applies it without asking.
The prompt goes to stderr and is shown only when stdin and stderr are both
terminals. Otherwise, without `--yes`, it prints the plan, then
`not a terminal: pass --yes to apply`, and exits 2. `uninstall` takes the same
flags and removes the two mod files, the `gpui-kit.d.ts` and `jsconfig.json`
the app writes into the folder, the folder unless something else is left in
it (the plan names what), the bindings that still open this mod, and the rc
hook. `keys.json` is the
app's whole keymap, so `install` never creates it: open the app once first.
Once `install` has edited `keys.json`, herdr-app no longer adds new default
shortcuts to it on upgrade, and the file is rewritten with 2-space
indentation. A key already bound to another action, in
`global` or in a `terminal` or `layout:<name>` section that takes precedence,
is never changed, whatever its spelling (`alt-ctrl-r`, `option`, `super`);
other keys keep their order. A `keys.json` that is not valid JSON or cannot
be written skips the two keys and leaves the file alone; the mod and the rc
hook still go ahead. An rc file or mod file that `install` could not write
(owned by another user, or a backup left from a failed edit) stops the plan
before anything is written, and running it as root with
your HOME (`sudo`) is refused. A linked mod directory or mod file is left
alone. Running `install` again finishes an install that stopped part way. 0.2.0's
`install --write` and `install --remove` still work as `install --yes` and
`uninstall --yes`.

For the config:

```sh
mkdir -p ~/.config/herdr-revive
herdr-revive config > ~/.config/herdr-revive/config.toml
```

Releases before v0.2.0 have no `--host`; install v0.2.0 or newer for the core
host, and v0.2.1 or newer for `install` to set up the mod and keys.

Set `auto_save = true`, `auto_restore = true` and `allowed_programs` in the
config, then open a new pane. The config and state directories follow
`XDG_CONFIG_HOME` and `XDG_STATE_HOME` (`~/.config/herdr-revive`,
`~/.local/state/herdr-revive`); `--config-dir` and `--state-dir` override
them. If you also use the TUI plugin, give both one config by linking the
directory: `ln -s ~/.config/herdr/plugins/config/cantona.herdr-revive
~/.config/herdr-revive`. Link the directory, not `config.toml`: the file
itself is opened without following symlinks and a linked one is refused.
Snapshots stay per host. The core socket is `--socket`, else `HC_SOCKET`, else the `socket` in
herdr-core's own config, else core's default path for `HC_SESSION`.

The rc hook is one line between `# >>> herdr-revive >>>` and
`# <<< herdr-revive <<<` markers:

```sh
case $- in *i*) [ -n "${BASH_VERSION-}" ] && for __revive_found in "$(command -v herdr-revive 2>/dev/null)" "$HOME/.local/bin/herdr-revive" "$HOME/.cargo/bin/herdr-revive"; do [ -x "$__revive_found" ] && { eval "$(exec "$__revive_found" --host core init bash 2>/dev/null)"; break; }; done; unset __revive_found ;; esac
```

On macOS's stock bash 3.2 any DEBUG trap, this hook's included, leaves
`PIPESTATUS` holding the trap's status, so `${PIPESTATUS[*]}` read after a
pipeline shows `0`; bash 4 and later restore it. On Linux it goes into
`~/.bashrc`, which core's interactive bash reads. Where core starts a login
bash (by default on macOS; anywhere when herdr-core's `shell` has `-l`), it goes
into the first of `~/.bash_profile`, `~/.bash_login` and `~/.profile` that
exists, else a new `~/.bash_profile`; on Linux a login file that already
sources `~/.bashrc` keeps it in `~/.bashrc`. A block left in another of
these files (core's `shell` changed since) is reported by `status` and
removed by `install` or `uninstall`. A symlinked rc file is edited at
its target and keeps its permissions; a hard-linked one is rewritten in
place, so every link sees the block. A file that does not end in a newline
gets one before the block, so the marker starts its own line; removing the
block leaves that newline. Before the first change the file is
copied once to `<file>.revive-backup` beside it (for a symlink, `<file>` is
the target); there is no backup when the file was missing or empty. Every
mode refuses an rc file with an unmatched marker, and a change to one owned
by another user is refused.
`install` replaces an outdated block in place (its plan line says
`update`); `uninstall` takes the block out.

`init bash` prints the hook when `auto_save` is on and, when `auto_restore`
is on and this pane has a saved command to relaunch, a one-time
`PROMPT_COMMAND` entry that runs that command at the first prompt: after the
rc lines below the marker block (`PATH` changes, nvm, conda), and with job
control on, so the command leads its own process group. An rc line below the
marker block that replaces `PROMPT_COMMAND` instead of extending it (for
example `PROMPT_COMMAND='history -a'`) removes that entry: the command does
not run, the journal still records it as applied, and, when `auto_save` is
on, the hook prints a dim notice naming the command when the first prompt
starts. It acts only in a
pane's own shell (`HC_TERMINAL_ID` set, and its parent the shell core started
there), and it never breaks a shell: it always exits 0, prints nothing when it
fails before the hook is built (no `HC_TERMINAL_ID`, an unreadable config) or
when its parent is not the pane's shell, and still prints the hook when core
cannot be reached or the relaunch fails. `REVIVE_AUTO=0` in a pane's
environment skips its relaunch, with a dim notice when there was something to
relaunch. When another operation holds the session lock for longer than
`timeout_ms` + `settle_ms` (at most 60 s), the relaunch is skipped with a dim
notice naming `herdr-revive --host core restore --rehydrate`. Pending
recovery evidence skips the relaunch of every app pane, with a dim notice
naming `recovery acknowledge`, until it is acknowledged. `REVIVE_DEBUG=1`
prints the reason for a failure or for a candidate's skipped relaunch on
stderr, which the installed line discards (drop its `2>/dev/null` to see it).

The hook registers with bash-preexec when it is loaded, and otherwise chains
any existing `DEBUG` trap (which still sees the command's `$?` and `$_`, and
whose return status is passed on, so `extdebug` command skipping still works)
and adds `__revive_precmd` at the end of `PROMPT_COMMAND`, so the entries
before it still see the command's `PIPESTATUS` and `$_`; its entry is
`${__revive_precmd_run:-:} && :`, so the entries after it still see `$?`
without a failure tripping `set -e` or the ERR trap, and a child bash that
inherits an exported `PROMPT_COMMAND` runs `:` there. When another file
runs the rc file (`~/.profile` running `~/.bashrc`), the trap goes in at
the first prompt, because bash hides an existing `DEBUG` trap from a sourced
file; a later line that replaces `PROMPT_COMMAND` then leaves the hook off.
A tool loaded after the hook that wraps the `DEBUG` trap and calls it
without the status (starship's bash init) hides the command's status from
the hook; the shutdown guard then has only the status the prompt hook sees.
The trap leaves `$?` and `$_` as they were, so
`mkdir -p build && cd "$_"` still works. Evaluating it again registers
nothing twice. When a command names an allowlisted program,
it runs `herdr-revive --host core autosave --force` detached after 0.5 s, so
the shell never waits. These saves are limited to two per second (the second
one deferred to 1.5 s); a builtin such as `cd` takes none, and a save takes
the hint of whatever the shell runs when it fires, so a third program in
the same second is covered by the deferred one. The next prompt after such a command saves once
more, unless the command died of SIGHUP, SIGKILL or SIGTERM (exit status
129, 137 or 143): at shutdown every pane's program dies that way, and that
save would record every pane idle. With the `DEBUG` trap the status is read
when the next command starts, before any `PROMPT_COMMAND` entry runs,
so entries the rc file puts in front of the hook's do not hide it, and such
a status holds off every save until the prompt (for that second and the
next), so a `PROMPT_COMMAND` entry the hook takes for a command cannot save
a shutdown either.
With bash-preexec it is the status bash-preexec hands its precmd functions.
With bash-preexec the whole command line is matched; with the `DEBUG` trap
each simple command is matched on its own (`cd x && ssh h` is checked as
`cd x`, then as `ssh h`). `"*"` in `allowed_programs` matches every command,
so every command line gets the limited saves and the prompt after it one more.
A program started inside a shell function does not trigger a save (the
`DEBUG` trap does not run inside functions unless `set -T` is on, and
bash-preexec sees only the function's name), nor, with bash-preexec, does one
started through an alias, whose line is matched as typed; such a program is
captured only by the next save from elsewhere: the timer, the dialog's Save
button, or another pane's hook. The relaunch also runs its command from a
function, so it reports that command to the hook itself. A job brought back
with `fg` or `%1` saves as a start does. A shell that reads its rc file
again (`source ~/.bashrc`, `exec bash`) relaunches nothing: the saved program
was running in this core run, so the user has stopped it since.

The dialog mod shows what `herdr-revive --host core status` reports: the last
snapshot, which panes the next core start would relaunch, any pending
recovery, and whether the config and rc line are in place. Its buttons run
`save`, `restore --rehydrate` and `recovery acknowledge`. `herdr-revive
install` puts it in `~/.config/herdr-app/mods/herdr-revive` and binds it to
`ctrl-alt-r` (`cmd-alt-r` on macOS), as in [keys.json](keys.json). The app runs
`herdr-revive` by name from its own `PATH`, which may lack `~/.cargo/bin`.
herdr-app 0.1.13 stops a command after 30 s. [RECIPE.md](RECIPE.md)
describes the dialog in full.

`status`, `save`, `preview`, `restore [--rehydrate]`, `list`, `recovery`,
`autosave` and `timer` work on the app; a manual `restore` types each command
into its pane through core when the pane's shell is idle. `event` only saves.
Not on the app: named spaces, `restore --recreate`, fish and zsh. The
optional periodic timer runs in a core terminal the app does not show, and
stops without a final save, since a logout signals every terminal at once:
`hc run --keep --bg --no-parent --name revive-timer -- herdr-revive --host core timer`.
It keeps running across `hc server restart` and `hc update`. When its program
is lost (`hc server stop`, a reboot), core keeps the terminal and its name but
puts a shell in it and never re-runs the timer: restart it there with
`hc send revive-timer 'herdr-revive --host core timer'`, or `hc kill
revive-timer` and run the line above again. The hook and the dialog's Save
button do not need the timer. The rc line looks for `herdr-revive` on the
shell's `PATH`, then in `~/.local/bin` and `~/.cargo/bin`, because a login
bash reads `~/.bashrc` before `~/.profile` adds those, and a core started
from the desktop hands its shells a bare `PATH`. The timer's `hc run` still
finds `herdr-revive` by name on core's own `PATH`.

Measured on Linux (Intel Core i7-10700) with `scripts/benchmark_core.py`
(Linux only: it reads `/proc` and runs `setsid` and GNU `time -f`) against
the fake core from `tests/core_integration.py`, not a real `hc`; 30
interleaved warm samples per case. The init rows time the rc line's
`$(exec herdr-revive --host core init bash)` in a fresh interactive bash, fork
and exec included. Raw samples, versions and commands are in
[docs/benchmarks/linux-core-2026-11-07.json](docs/benchmarks/linux-core-2026-11-07.json).
A save makes one core request per pane but usually reads the whole process
table only twice, not once per pane, so 100 panes cost little more than one.

| Operation | p50 | p95 |
| --- | ---: | ---: |
| `init bash`, no relaunch | 1.62 ms | 1.73 ms |
| `init bash`, one relaunch | 10.27 ms | 10.76 ms |
| `autosave --force`, 1 pane | 17.70 ms | 18.53 ms |
| `save`, 100 panes | 22.61 ms | 23.83 ms |

## Layouts and command capture

Snapshots preserve workspace/tab order and labels, nested split directions and
ratios, pane labels, cwd, active tabs, focused panes, and structured commands.
Named spaces live in a plugin-wide library and can open in another session.
Full session snapshots remain strictly bound to their original socket identity.
Default reconstruction restores focus; `--no-focus` preserves current focus and
leaves new tabs' focus at host defaults.

New workspaces use Herdr's native argv launch API. Large/deep tabs are assembled
using splits in their final tab, avoiding the host's 24-pane/16-level bulk-layout
limit and preserving launch-time tab identity. These split-created panes and
existing panes use tested Bash/dash/zsh shell encoding.
No existing workspace is deleted by reconstruction.

Linux capture reads `/proc`; macOS uses native `libproc` and `sysctl` calls for
argv/cwd with PID/start-time checks. Display command lines are never parsed,
and neither adapter launches process-inspection utilities. Program stdout/stderr redirected to `/dev/null`
device are preserved explicitly. Non-UTF-8 arguments, terminal controls, inaccessible
processes, pipelines and other redirected standard streams fail capture and preserve
the last good snapshot. Background jobs are not reconstructed.
For OpenSSH, capture checks the session output descriptors rather than inferring
a launch redirection from its internally replaced stdout. Genuine or ambiguous
SSH output redirections are refused.
Git's internal system `less`/`more` pager is recognized by its child relationship
and stream identities, and restores by rerunning Git. External shell pipelines
and redirected pager output remain unsupported.

macOS uses authenticated Unix-socket peer credentials and a boot UUID to identify
server restarts, and `kqueue` for autosave shutdown signals. Restore checks shell
children directly instead of scanning every process for each idle pane. The
[macOS validation and performance results](docs/MACOS.md) cover Apple Silicon;
Intel macOS is compile-checked. A protected/setuid foreground program
(including Apple's `top`) does not expose its arguments, working directory or
standard streams. Its pane is saved with its shell's directory, where the
program started, and with a command only when something proves the program runs
without redirection: the command the bash hook saw start, when it ran with the
shell's streams on the terminal (no redirection of a group or loop around it,
no `exec >file`), was not a function or alias, was (with bash-preexec, whose
line comes from history) the command about to run, is plain words naming the
program and, where Linux reads the exact command line, equals it; or the
program's streams could be read and all point at the terminal. A hint counts
only until the shell runs anything else or shows its prompt, so in `a && b`, `a
| b` or `a; f` (a function) the next program never gets `a`'s line. A `( ... )`
subshell (also as an `if` or `while` condition) or a `coproc` runs no hook in
the shell, so `top -h; ( top > log )` can still lend `top -h` to the second
`top`. Nor does a pipeline whose last part is a compound command (`a | while
...`, `a | { ...; }`): if that part ends first, `a` can be saved without its
pipe. A later save that cannot prove it again keeps the command and directory
proven before (`protected.json` in the state directory) while that same process
runs there in the same core generation, never by an equal command line, which
says nothing about a new run's redirections. Otherwise the pane is saved
without a command, as is a pane whose program Herdr reports as an agent. A
warning says which, and the other panes still save. A process that disappears
mid-capture or missing configured launcher-environment data still fails the
capture and preserves the previous snapshot.

Claude, Codex, Gemini, Copilot and Cursor restoration requires an exact UUID from
a matching native reference or explicit resume arguments. An untouched bare
Claude prompt already has a UUID but no resumable transcript; Revive relaunches
that UUID with `--session-id`, then uses exact `--resume` after Claude writes the
transcript. This transition is checked in Claude's active profile and does not
guess a latest session. Recognized Node package wrappers become canonical agent
executables on PATH. Original launch flags are not inherited; use reviewed
`agent_extra_args`. Latest-session, continuation and cwd guesses are refused.
Unknown detected-agent wrappers fail capture.

## Recovery and safety limits

Plugins run as your user. Their directories and command allowlists are not a
sandbox. Private snapshots contain command arguments and must stay out of logs
and source control.

Herdr 0.9.1 has no atomic conditional-run or mutation idempotency token. Existing
pane restoration has a check/send race, and an idle shell process does not prove
an empty prompt: `read`, heredocs or partially typed text can consume input.
Restore into known fresh prompts. Tests reproduce these limits. `applied` means
request acknowledgement, not command completion or readiness.

Before every mutation, a synced journal records `sending`. A lost reply, crash
or persistence failure leaves evidence and blocks further save/restore, including
future boots. Reconstruction can leave partially created workspaces; inspect
them before acknowledgement. Mutating requests are never automatically retried.

```sh
herdr-revive recovery inspect
herdr-revive recovery acknowledge ID_FROM_INSPECT
```

Outside a Herdr plugin environment add `--host herdr`, or `--host core` for
the desktop app (`herdr-revive --host core recovery inspect`).

Acknowledgement neither retries nor rolls back. Automatic replay remains
suppressed for an acknowledged boot; a later explicit manual restore is a new
user request. Recovery records and reconstruction source snapshots are retained
indefinitely and are separate from normal snapshot retention.

## Import and migration

No automatic migration or adoption of old config, locks or action IDs occurs.
The optional legacy importer is not implemented. Native named snapshots can be
imported directly with `space import NAME FILE` and then previewed/opened as new
workspaces. Optional mappings explicitly associate source panes with current
workspace/tab/pane IDs before importing; see [contracts](docs/CONTRACTS.md).
The source file remains unchanged. Full cross-session snapshots are rejected.

## Verification

```sh
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo build --release --locked
REVIVE_BINARY="$PWD/target/release/herdr-revive" python3 tests/integration.py
REVIVE_BINARY="$PWD/target/release/herdr-revive" python3 tests/core_integration.py
python3 tests/real_host.py
REVIVE_BINARY="$PWD/target/release/herdr-revive" python3 tests/openssh.py
python3 scripts/verify_install.py
python3 scripts/verify_install.py --github-ref v0.2.1
python3 scripts/benchmark.py --include-cli
python3 scripts/benchmark_core.py --output docs/benchmarks/linux-core-$(date +%F).json  # Linux only
python3 tests/benchmark_contracts.py
python3 scripts/benchmark_matched.py --baseline ../herdr-resurrect
python3 scripts/dependency_notices.py
```

Tests use private sockets, clean shells and harmless fixtures. Fake SSH/minicom
executables only record arguments; no devices or remote hosts are contacted.
The OpenSSH regression suite requires `sshd` and connects only to a temporary
localhost server using disposable keys and configuration.
See [validation](docs/VALIDATION.md), [contracts](docs/CONTRACTS.md),
[release/rollback](docs/RELEASE.md) and [dependency notices](THIRD_PARTY_NOTICES.md).
The [matched benchmark](docs/MATCHED_BENCHMARK.md) includes real-host results
against the unchanged JavaScript predecessor and reproduction requirements.
