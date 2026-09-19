# Validation and remaining gates

Current macOS coverage and native performance measurements are recorded in
[MACOS.md](MACOS.md). The results below describe the original Linux prerelease.

Measured and tested on 2026-09-18 HKT (2026-09-17/18 UTC), Linux x86_64,
Rust 1.97.1, Herdr 0.9.1/protocol 22. These are local test results for the
initial Linux prerelease, not certification of every host, filesystem or interactive program.

## Completed checks

- Release build with Cargo.lock; formatting; Clippy on all targets with warnings
  denied; no application unsafe code.
- 29 Rust contract tests: strict schema/config, exact session and pane identity,
  policy changes, UUID/native agent references and Node wrappers, deterministic
  1/10/50/100-pane planning, real shell argument round trips, invalid encodings,
  process identity/PID reuse, pipelines, storage permissions/retention/symlinks,
  permission failures, corrupted state, crash evidence, partial failure, replay
  suppression, and bounded partial/stalled replies.
- 21 release integration scenarios: both transports with literal fake SSH and
  minicom argv; 100 simultaneous startup/event/autosave processes; lost mutation
  acknowledgement; busy foreground/background descendants; execution policy;
  capture/preview request counts; native-space import; full cross-session refusal;
  pipeline/redirection refusal; socket chmod replay regression; explicit
  reproductions of check/run and shell-readiness limitations. Expanded cases
  verify all five exact agent argv forms through fake executable recorders,
  global named spaces across sessions, repeatable manual restore, save-only
  forced autosave and ambiguous workspace-creation recovery.
- Twelve isolated real-host scenarios: repeated named-space opening with nested
  geometry/labels/tab order/cwd/native argv; explicit full recreation; default
  missing-workspace recreation versus rehydrate-only; 25-pane/depth-25 layout
  reconstruction with final-tab process environment and recorded destination
  identities; focus restoration; current-policy denial; actual server restart
  restoring a non-agent without an agent event; periodic and final timer saves.
- The first live migration extension misclassified OpenSSH's internally nulled
  fd 1 as a launch redirection. A user restore exposed blank SSH output; the fake
  SSH fixtures had not reproduced OpenSSH's internal descriptor handling. Fixed
  capture to inspect the verified system client's terminal-backed session output
  and reject ambiguous/genuinely redirected SSH output. Old null-output SSH
  snapshot commands now fail validation instead of replaying the bad wrapper.
- Four real OpenSSH regressions use disposable localhost sshd/client keys:
  interactive and noninteractive capture/restore/recapture with visible output,
  and actual null/file output redirection refusals preserving the prior snapshot.
  Generic output-device capture and native reconstruction remain tested;
  redirected stdin/files stay refused.
- Actual Herdr parser, ten actions, successful startup hook, runtime working
  directory, popup launcher exit, private link/enable/disable/relink/uninstall,
  preserved config and state, and refusal of a manifest requiring a future host.
  These lifecycle tests used disposable directories and a disposable server.
- Actual GitHub installation of the reviewed source: missing Cargo and a failed
  build refused registration; a failed reinstall preserved registration;
  successful install/reinstall/uninstall preserved external configuration and
  state. The installed binary and manager ran from the managed checkout.
  All paths were under temporary roots; normal registration was unchanged.
- Dependency metadata/license inventory for all 57 Linux dependency packages,
  with upstream license/notice texts preserved. The lock contains additional
  target-specific packages not validated for a non-Linux release.
- Matched real-host JavaScript/Rust benchmark: 900 launches across 30 cases,
  randomized/interleaved with matching 1/10/50/100-pane mixed workloads and
  per-sample capture/preview equivalence checks. Three benchmark trace-parser
  regression tests pass; two review findings were fixed before the final run.
- Focused reviews covered restore evidence, OpenSSH descriptors, custom-launcher
  policy, native startup collisions and performance changes. The whole-repository
  adversarial review returned NO FINDINGS: zero confirmed, disputed or unverified
  findings. All 66 runtime tests and three benchmark parser tests passed again
  at publication. Documentation review corrected one comparison claim; no
  findings remain. The annotator
  `review_changes` MCP was unavailable; the background reviewer fallback was used.

## Matched real-host performance

The separate [matched benchmark](MATCHED_BENCHMARK.md) compares unchanged
herdr-resurrect against both Rust transports on the same live private fixtures.
At 100 panes, median save time was 363.82 ms for JavaScript CLI,
183.82 ms for Rust CLI and 40.69 ms for Rust direct. Median preview
was 161.65 / 10.86 / 7.43 ms. Median maximum process RSS during save
was 72.57 / 8.85 / 5.28 MiB.
The report includes p95, provenance, workload, subprocess counts, correctness
checks and limits; [raw samples](benchmarks/linux-matched-2026-09-18.json)
are retained. This compares whole implementations, not language alone.

## Size and maintenance

The stripped release binary is 2,333,400 bytes
(2.23 MiB). Application Rust source is 3,723 physical lines,
including blanks and comments. There are nine direct dependencies and 57 packages
in the Linux build graph excluding this project, including build-time packages.
This does not establish a source-size reduction against the study's JavaScript
baseline. Platform adapters, protocol qualification, shell behavior and packaging
remain maintenance costs.

Binary SHA-256: `0caa8acb3077253c08fe31576a2806be1309f12bf010889cc6a739fd5170c3d3`.
Cargo.lock SHA-256: `d1fd4d301b3a99458630fc803588320bbf22dc3297d984ea5d3f5c66cde28c1d`.
The publication benchmark uses this release binary; its hash is also recorded
in the raw measurements.

## Remaining qualification

- Controlled-load and independently labeled cold-start measurements; aggregate
  burst memory. The unchanged-baseline mixed-workload comparison is complete;
  equivalent optimized I/O/caching and safety would be needed for language-only
  attribution.
  Real end-to-end readiness and restore time must be measured separately from
  startup and the configured settle delay.
- Actual disk-full/partial-write fault injection, power-cut filesystem tests,
  and comprehensive interrupted-save qualification. Current tests cover
  permission failure, oversized data, leftover partial temporaries and restore
  interruption; those are not substitutes for power-loss qualification.
- Full-session restoration and rollback observation beyond disposable fixtures.
  Local use exposed the OpenSSH issue corrected above; isolated restart tests
  and fake SSH/minicom argument tests do not prove remote/device connectivity.
- Actual older-host testing. The installed parser's future-minimum rejection is
  tested; running an older Herdr binary is not.
- Public marketplace indexing is external to this repository and does not imply
  a host review or endorsement.
- Windows implementation and Intel macOS runtime qualification remain planned.
  The current manifest advertises Linux and macOS; Apple Silicon results are
  recorded separately above.

There is no legacy importer or automatic migration. Native formats and explicit
migration instructions are described in the README.

## Launchers and interactive programs

The public package and plugin ID are `herdr-revive` and `cantona.herdr-revive`.
Schema-1 readers retain the early spelling and session hash namespace so existing
boot and pending evidence survive a rename. A copied-state regression checks
that this does not deliver another automatic restore.

A nonsecret environment selector can choose `claude-local` or another configured
exec wrapper. Exact UUIDs are required and process environment values are not
copied to snapshots. The PTY test checks delivery through the selected wrapper.
Older snapshots containing only `claude` require recapture with a launcher rule.

Real private Herdr tests cover top, htop, journalctl and tail foreground restarts
and recapture. Git tests cover live and completed internal pager output, restore
through the original Git argv, and preservation of the prior snapshot when an
external pipeline or redirected pager error stream is encountered. The tested
pager implementation corresponds to [Git pager.c](https://github.com/git/git/blob/master/pager.c).

The current speed changes buffer bounded reads and explicit output flushes, and
avoid whole-process-table scans for panes already proven busy. Candidate idle
shells retain the complete background-child and fresh identity checks. No fsync
or restore claim was removed. The annotator MCP remains unavailable.

## Native agent restore collision — 2026-09-18

A real restart exposed a gap in the wrapper tests: Herdr's own
`session.resume_agents_on_restore` defaults true. It launched canonical
`claude --resume UUID` before Revive could restore the correctly saved
`claude-local` entry. The conversation existed only under the local profile;
the canonical launch reported no conversation found. Earlier wrapper tests
covered Revive delivery, not this competing host startup path.

A new disposable real-host regression reports an exact native session, captures
the custom wrapper, and restarts twice. With native restore enabled, it observes
the wrong canonical argv and absent profile; with native restore disabled,
Revive restores the wrapper, exact ID and selected environment, recaptures it,
and refuses automatic replay in the same boot. The test uses a harmless fixture
executable and never connects to an agent service.

The required configuration handoff is documented in the README: disable Herdr's
native agent resume while keeping Revive's automatic restore and agent resume
on. Existing sessions need their own snapshots before relying on this handoff.
