# Replacement feature assessment

Baseline: [cantona/herdr-resurrect at c05b43d](https://github.com/cantona/herdr-resurrect/tree/c05b43dda3ff968cb21653b7f4188245015a15be),
assessed on 2026-09-18 from its manifest, CLI/configuration and observable
feature contracts.
Implementation and tests here were written independently. The baseline was not
modified or imported into this repository.

| Baseline capability | herdr-revive coverage |
| --- | --- |
| Full session snapshot, retained history and list | Native validated snapshots, configurable retention, explicit selected-file restore |
| Restore preview/dry run | Pure exact-ID plan plus missing-workspace/recreation plan; no mutation |
| Restore existing panes | Repeated manual rehydration with fresh identity, current policy and busy checks |
| Recreate workspaces/layout | Default manual missing-workspace rebuild; explicit full `--recreate` |
| Split geometry | Exact exported BSP tree, ratios, nested orientation; tested beyond bulk API limits |
| Workspace/tab/pane names and cwd | Preserved, along with workspace/tab order and default focus restoration |
| General programs/development tools | Structured argv, including configured node/npm/interpreters and absolute paths |
| Program allowlist/wildcard/basename behavior | Exact default, optional case-insensitive basename matching, explicit `*` |
| Claude/Codex/Gemini/Copilot/Cursor resume | All five; exact native or explicit UUID, current executable allowlist |
| Agent resume customization/toggle | `resume_agents`, reviewed per-agent extra argv; no shell templates |
| Named space save/open/delete/list | Global library; repeatable opens create fresh workspaces; native import/export files |
| Interactive space management | Dedicated save/open/delete popup actions, optional fzf picker, confirmation, manager |
| Periodic autosave and exit save | Foreground timer pane; configurable interval; final orderly-signal save |
| Lifecycle autosave | All baseline events plus tab creation/closure/rename, workspace rename and agent status |
| Startup restore without agent event | Startup hook shares the same persistent automatic boot claim |
| Concurrent hooks and restart safety | Session file lock; peer PID/start-time boot identity; durable no-retry evidence |
| Platform targets | Linux and macOS; Windows remains planned |

Additional capabilities include native argv launch during reconstruction, bounded
version-checked direct IPC, strict snapshot/config schemas, redacted previews,
private atomic storage, mutation journals, explicit recovery acknowledgement,
previewable policy denial, a no-focus named-space option, and native-space imports.

## Deliberate safety and migration differences

Replacement capability does not mean old namespace/config/snapshot compatibility.
The new identity is `cantona.herdr-revive`; old installation and state stay intact.
No automatic migration, inherited bindings, or optional legacy importer is present.

- Automation and command permission start disabled. General launchers remain
  configurable; policy does not claim to sandbox allowed executables.
- Automatic restore only targets exact workspace/tab/pane IDs and never creates
  missing panes. Manual mode also avoids positional matching. Missing members of
  an existing workspace require explicit full recreation into a new workspace.
- Agent resume refuses latest/cwd/continue guessing and arbitrary shell templates.
  Exact native IDs survive independently of original command flags; reviewed
  extra argv supplies supported options without replacing session selection.
- Reconstruction launches argv directly and preserves source workspaces. Idle
  existing-pane restoration is limited to tested Bash/dash/zsh encoding.
- Program stdout/stderr to `/dev/null` are captured and restored explicitly.
  SSH uses verified terminal-backed session descriptors and does not replay its
  internally replaced fd 1; redirected or ambiguous SSH output is refused.
  Pipelines, other redirected stdio, non-UTF-8 and terminal-control data fail visibly
  instead of replaying an altered command. Background jobs, terminal scrollback,
  editor buffers and arbitrary environment variables are not session snapshots.
- `--no-focus` preserves current focus rather than assigning saved focus in new
  tabs. Default reconstruction restores saved active/focused selections.
- Interrupted/ambiguous mutations require inspection and acknowledgement. No
  automatic retry is performed; explicit subsequent manual requests are distinct.

These differences implement the plan's safety contract. Native Linux replacement
workflows are covered by the automated tests described in [VALIDATION.md](VALIDATION.md).
Full-session rollback, additional operating systems and remaining qualification
are tracked separately from feature coverage. See [release notes](RELEASE.md).


## Action migration

Bindings must explicitly use the new namespace; no aliases impersonate the old
plugin. The action mapping is:

| Baseline local ID | New fully qualified ID |
| --- | --- |
| `save` | `cantona.herdr-revive.save` |
| `restore` | `cantona.herdr-revive.restore` |
| `restore-preview` | `cantona.herdr-revive.preview` |
| `snapshots` | `cantona.herdr-revive.list` |
| `save-space` | `cantona.herdr-revive.save-space` |
| `open-space` | `cantona.herdr-revive.open-space` |
| `delete-space` | `cantona.herdr-revive.delete-space` |

Open the new timer through action `cantona.herdr-revive.timer` or plugin pane
`autosave`. Save existing bindings/configuration before changing them; rollback
restores those backups. All listed actions are available through Herdr.
