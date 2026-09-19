# herdr-revive

Bring your commands back after a Herdr restart. Save the programs, working
directories, layouts and exact agent conversations in your panes, then restore
them from retained snapshots or reusable named workspaces.

An independent MIT-licensed Rust plugin by Su Kang Yin.
Package/executable: `herdr-revive`; plugin: `cantona.herdr-revive`.
**Current source supports Linux and macOS.** The original `v0.1.0` tag is a
Linux prerelease; build and link this checkout for macOS support. Windows remains planned.

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
Herdr **0.9.1 / protocol 22**. The runtime currently accepts only that host pair.
No Node or Python runtime is required. The popup uses POSIX `sh`, with optional
`fzf` for saved-space selection. Python 3 is used only by development scripts.
On macOS, install Xcode Command Line Tools (`xcode-select --install`) for the linker.

Install the tagged source release; Herdr previews the manifest and builds it:

```sh
herdr plugin install cantona/herdr-revive --ref v0.1.0
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

## Commands

Herdr supplies `HERDR_PLUGIN_ROOT`, `HERDR_PLUGIN_CONFIG_DIR`,
`HERDR_PLUGIN_STATE_DIR`, `HERDR_SOCKET_PATH`, and `HERDR_BIN_PATH`.
Explicit CLI overrides are `--config-dir`, `--state-dir`, `--socket`, and
`--herdr-bin`; paths must be absolute. No fallback to another session occurs.

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
| `timer [--interval-seconds N]` | Foreground periodic save, including a final save on SIGINT/SIGTERM/SIGHUP |
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

Actions under `cantona.herdr-revive` are `save`, `preview`, `restore`, `list`,
`autosave`, `timer`, `manage`, `save-space`, `open-space`, and `delete-space`.
The manager offers session history selection, named spaces, and confirmation
before restore/delete. Saved-space pickers use `fzf` when available, otherwise
a name prompt. The optional `autosave` pane runs the timer; it is not a daemon.
The timer's explicit saves are independent of `auto_save` and never initiate
automatic restore. Event autosave uses leading-edge debounce, not a trailing queue.

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
Intel macOS is compile-checked. Protected/setuid processes (including Apple's
`top`) may not expose the required metadata. Missing process or configured
launcher-environment data causes capture to fail and preserves the previous snapshot.

Claude, Codex, Gemini, Copilot and Cursor resume require an exact UUID from a
matching native reference or explicit resume arguments. Recognized Node package
wrappers become canonical agent executables on PATH. Original launch flags are
not inherited; use reviewed `agent_extra_args`. Latest-session, continuation and
cwd guesses are refused. Unknown detected-agent wrappers fail capture.

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
python3 tests/real_host.py
REVIVE_BINARY="$PWD/target/release/herdr-revive" python3 tests/openssh.py
python3 scripts/verify_install.py
python3 scripts/verify_install.py --github-ref v0.1.0
python3 scripts/benchmark.py --include-cli
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
