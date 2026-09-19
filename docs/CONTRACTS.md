# Independent contracts

This is a new implementation of documented behavior, not a translated source
tree or imported repository history. The predecessor's source was inspected
during the preceding study; this is not a clean-room provenance claim. This
implementation did not copy predecessor code, tests, wrappers or documentation.
The baseline remains a separate, unchanged repository. Herdr's public CLI/API
schema supplied the integration contract; no Herdr application code is linked.

## Storage and identity

The canonical socket path is its canonicalized parent plus unchanged filename.
Relative paths and non-UTF-8 path encodings are refused. SHA-256 over
`cantona.herde-revive\0session-v1\0<canonical socket path>` selects a session
directory below the injected plugin state root. Sharing a cwd has no effect.
The schema-1 hash namespace keeps the original spelling so existing boot claims
remain valid after the rename. Snapshot and journal readers accept both
`herde-revive` and `herdr-revive`; new records use `herdr-revive`.

Linux boot generation is SHA-256 over
`linux-server-v1:<OS boot UUID>:<SO_PEERCRED PID>:<process start ticks>`.
The Unix socket must be a real socket, and the server peer must have the same
UID as the plugin. Socket chmod/chown timestamps cannot create a new claim.
PID reuse across either a process restart or OS reboot changes identity.
Peer-identity connections send no API request; metrics count them separately.

macOS generation uses SHA-256 over
`macos-server-v1:<kern.bootsessionuuid>:<LOCAL_PEERPID>:<process start microseconds>`.
`LOCAL_PEERCRED` verifies the same-user peer. The Linux generation format is unchanged.

```
HERDR_PLUGIN_STATE_DIR/<session hash>/
  operation.lock
  latest.json
  last-save.json
  snapshots/<timestamp>-<content-hash-prefix>.json
  pending.json                 # only while unresolved
  boots/<claim>.json           # automatic generation or explicit manual claim
  rebuild-pending.json         # incomplete reconstruction
  operations/<id>.json         # retained reconstruction journals
  operations/<id>-snapshot.json

HERDR_PLUGIN_STATE_DIR/spaces/
  library.lock
  <validated-name>.json
```

The OS releases the exclusive file lock on process exit; no stale-PID-lock
heuristic or timeout-based lock stealing exists. Hook contention exits as
`operation_in_progress`; explicit operations report an error. Saves, restores,
imports and recovery acknowledgements hold the same session lock. Named-space
operations also acquire a plugin-wide library lock, after the session lock.

Directories are owner-only, with newly created ancestor links synchronized.
Files are written through owner-only same-directory temporary files, flushed,
synced, atomically replaced, then followed by a directory sync. Replacement
failures are visible. A directory-sync failure after replacement is reported
as uncertain durability. Symlink/non-regular input and oversized data are
refused. These guarantees depend on the filesystem honoring sync and rename;
power-cut and disk-full qualification remain release gates.

## Snapshot schema 1

```json
{
  "tool":"herdr-revive", "schema":1,
  "session":"<64 lowercase SHA-256 hex digits>", "created_ms":0,
  "scope":{"kind":"session"},
  "panes":[{
    "workspace_id":"w1", "tab_id":"w1:t1", "pane_id":"w1:p1",
    "cwd":"/fixture",
    "command":{"kind":"program", "argv":["ssh","fixture.invalid"]}
  }]
}
```

A space scope is `{"kind":"space","workspace_id":"w1"}`. A command may
be null for an idle shell. Agent commands instead use
`{"kind":"agent","executable":"codex","agent":"codex","session_id":"UUID"}`.
Program stdout/stderr may target `/dev/null` using
`{"kind":"program_null_stdio","argv":["sleep","60"],"null_stdio":[false,true,false]}`.
The mask is stdin/stdout/stderr; stdin must remain false and at least one output
must be true. Capture checks the null character-device identity (Linux 1:3;
macOS vnode device/inode/rdev matched against the native `/dev/null`).
Restoration checks the original executable's allowlist entry, then passes its
argv as positional arguments to a fixed `/bin/sh` exec/redirection wrapper.
No saved argument is interpolated into that wrapper's shell source.
SSH cannot use this variant: older incorrectly captured SSH null-output entries
are rejected before restoration and require an explicit repair or fresh capture.
Unknown native fields and schema versions are errors. Config uses strict TOML;
API responses tolerate additive host fields while validating required fields.
Maximum file/reply size is 8 MiB, maximum pane count 10,000, maximum argument
count 4,096, argument length 64 KiB, and total command length 128 KiB.

Planner output is sorted by pane ID. It never contains executable shell text.
Full snapshots must match the current session. Named spaces are reusable across
sessions through the global library or explicit import. Optional ID mappings
must be one-to-one and reference one existing destination workspace. A mapping
entry has `source_pane_id`, `workspace_id`, `tab_id`, and `pane_id`.

The `layout` array preserves workspace/tab order and labels, active/focused IDs,
and a tagged recursive BSP root (`pane` or `split` with direction/ratio/children).
Every leaf must correspond to exactly one saved pane with matching ancestry;
IDs are unique. The parser has bounded size/recursion and trees are validated
to depth 64. Empty layout is accepted for command-only native fixtures, but
explicit reconstruction refuses it. Focused workspace is optional.

Reconstruction creates new workspaces. Each tab uses native argv `layout.apply`
when it fits the host's 24-pane, depth-16 bulk API limits. Larger/deeper trees
start with one native-argv anchor, then create splits directly inside that tab.
Split-created commands use idle checks and tested shell encoding after a single
settle delay per tab. This keeps managed `HERDR_TAB_ID` correct; moving already
running processes from temporary tabs would retain stale environment values.
Every mutating call checks peer boot identity and persists a sending record.
Default reconstruction applies saved focus; no-focus mode makes no focus calls.

## Restore state transitions

An absent boot record is Unclaimed. Restore writes `pending.json` with
`restoring`, a snapshot digest and per-pane outcomes. Before revalidation and
any potentially mutating request, the current pane becomes `sending`. Outcomes
become `applied`, `skipped`, or `failed`; failures stop subsequent panes.
After completion the Done record is synced under `boots/`, then pending evidence
is removed and that directory synced. If interruption leaves both records,
pending evidence takes precedence and requires acknowledgement.

Any pending record blocks saving and new restores, even across boots. A
successful acknowledgement marks the record Done and acknowledged without
altering recorded outcomes or retrying input. Reading recovery evidence and
previewing snapshots remain available. A completed boot prevents later automatic
restores for that server generation. Manual rehydration after a completed boot
uses a fresh explicit claim. Named-space opens and full reconstruction use
independent operation journals, preserving their source snapshot and created
workspace/tab IDs. They can be requested repeatedly; interrupted operations
require acknowledgement before another attempt.

Default manual restore first rehydrates exact existing IDs, then reconstructs
missing workspaces. Missing panes inside an existing workspace are never guessed
by position. Automatic hooks do not reconstruct anything. Forced autosave and
the optional foreground timer are save-only paths and cannot initiate restore.

## Transport

The direct adapter makes one connection per newline-delimited JSON request.
It validates ping version/protocol once per operation, response IDs, result
types, error envelopes, frame size, and a total deadline covering both write
and read. It never retries mutations. Unix sockets are nonblocking, including
connection establishment; unavailable or saturated endpoints fail closed.

CLI mode uses exactly the injected `HERDR_BIN_PATH` with argv and an explicit
socket environment. Stdout is bounded/nonblocking, errors are redacted, and a
deadline kills/reaps the child. Herdr's own CLI does an additional protocol
handshake per invocation; `requests` counts logical adapter calls rather than
these internal checks. `pane run` success is its zero exit status, as specified
by the host, while read operations validate JSON envelopes. Layout/reconstruction
operations without CLI equivalents use direct API even in CLI mode.

Direct save uses ping + one snapshot + one process-info request per pane +
one layout export per tab,
and one native process-table scan, with no Herdr or process-enumeration subprocesses.
Preview uses ping + one snapshot and no process queries. Execution deliberately
adds fresh identity and busy checks. There is no persistent connection,
unsupported batching or host library dependency. macOS uses bounded native FFI
calls with per-statement `unsafe` allowances and ABI size checks; other code
denies `unsafe`. Idle-shell checks on macOS query direct children and recheck
process identity rather than repeating the whole process-table scan. Linux checks
each shell thread's child list and falls back to the original process-table scan
if those lists are unavailable. Newly created split panes retry unavailable shell
metadata within the readiness deadline; no command is sent on an invalid read. The timer
waits on kqueue signals without polling during its interval.

## Intentional restrictions

The allowlist is checked at planning and execution; persisted booleans never
authorize input. Supported Bash/dash/zsh encoding handles literal argument
boundaries and cwd. Terminal control characters are rejected before encoding.
General programs/interpreters are supported when allowed. Exact matching is the
default; optional basename matching and wildcard trust are explicit. Known
agent executables and recognized Node package wrappers must use typed exact
agent references, even with wildcard policy. Allowed binaries are not sandboxed. Agent resume never falls
back to `--continue`, latest-session lookup, or filesystem/cwd heuristics.

Process-group members must descend from one foreground leader. Pipelines,
redirected stdin and output redirections other than `/dev/null` are rejected.
Observed agent stdout/stderr redirections to `/dev/null` are preserved exactly.
Git with a system less/more pager,
either direct or through Git's single system-shell wrapper, may use its internal
pipe: terminal and pipe identities are checked, and the original Git argv is
saved. Once Git closes both output pipe writers,
capture also requires its retained terminal descriptors; redirected pager output
and external shell pipelines remain refused. Keeping stdin attached also
preserves the PTY lifetime during native reconstruction. Process identity checks
guard PID reuse, but capture is not an atomic
snapshot of all OS processes. Busy detection includes background descendants.
Shell-readiness and check/run races remain the host limitations described in
the README and reproduced in the integration suite.

OpenSSH session stdout is separate from fd 1: after duplicating its streams,
OpenSSH redirects fd 1 internally. Capture verifies the process executable
against `/usr/bin/ssh`, checks character-device identity against the controlling
terminal, and identifies a unique consecutive session-descriptor triple using
the input/error identities before validating output. Terminal-backed session
output becomes a plain argv command. Redirected, ambiguous or nonconsecutive
session descriptors are refused; unsupported layouts are not guessed.
See [OpenSSH 10.2p1](https://github.com/openssh/openssh-portable/blob/V_10_2_P1/ssh.c#L2120).


## Agent options

Claude, Codex, Gemini, Copilot and Cursor use exact UUID references; Cursor's
canonical executable is `cursor-agent`. Native sources must match
`herdr:<agent>` and kind `id`. Parsed argv must contain one explicit resume ID;
unknown options/prompts fail parsing. Known Node wrappers are canonicalized to
the corresponding executable on PATH. Original flags are not replayed.

Extra configuration supports `--model`/`-m` with a value for all five agents,
and these agent-specific options (all value-taking unless marked boolean):

| Agent | Additional options |
| --- | --- |
| Claude | `--permission-mode`, `--settings`, `--add-dir`, `--allowedTools`, `--disallowedTools`; boolean `--dangerously-skip-permissions` |
| Codex | `--config`/`-c`, `--profile`, `--sandbox`, `--ask-for-approval`; boolean `--full-auto`, `--dangerously-bypass-approvals-and-sandbox` |
| Gemini | `--approval-mode`, `--include-directories`; boolean `--yolo` |
| Copilot | `--allow-tool`, `--deny-tool`, `--add-dir`; boolean `--allow-all-tools`, `--allow-all-paths` |
| Cursor | `--workspace`; boolean `--force`/`-f` |

Values use separate argv elements and cannot start with `-`. Session-changing
short/long flags, attached resume forms, prompts and unknown options are errors.
These options grant whatever the chosen agent implements; they are trusted user
configuration, not a second agent sandbox. `resume_agents=false` denies agents
while still allowing configured ordinary programs.

## Custom agent launchers

`agent_launchers` is a list of `{agent, executable, match_env}` tables. It describes
exec-style wrappers by an explicit environment selector after the wrapper has
become a canonical agent process. All selector keys and values must match exactly.
Duplicate selector keys in the process environment and multiple matching rules
are errors. Selector values should be nonsecret; snapshots contain only the
chosen launcher, agent kind and exact session ID. PID identity is checked around
the bounded environment read.

Custom launcher execution requires the same agent-to-executable rule in current
configuration and an allowlist entry. Ordinary Program snapshots using a configured
launcher basename are refused, including absolute/bare-path variants. A removed
profile cannot silently become the canonical launcher. An ordinary canonical
agent with no matching profile keeps its own executable.
