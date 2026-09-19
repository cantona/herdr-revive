# macOS support and validation

Version `v0.1.1` adds native macOS support; the original `v0.1.0` tag remains
Linux-only. Install with `herdr plugin install cantona/herdr-revive --ref v0.1.1`,
or build and link a local checkout using the README's development instructions.
Xcode Command Line Tools, Rust 1.97.1+, and Herdr 0.9.1 / protocol 22 are required.

## Implementation and limits

`src/platform.rs` contains shared validation and shell encoding. Linux `/proc`
inspection lives in `src/platform/linux.rs`; Darwin inspection lives in
`src/platform/macos.rs`. Native libproc and sysctl calls retain exact argument
boundaries, working directories, PID/start identities and process ancestry.
Other-user processes remain visible to ancestry checks, even when their full
metadata cannot be read. Display command lines are never parsed.

The macOS adapter validates terminal and `/dev/null` device identities, OpenSSH
session output descriptors, and system Git's internal less/more pipe topology.
Apple's Git launcher shim is handled through installed developer-tool paths,
including the xcode-select link, without launching a helper process.

Kernel interfaces require FFI: `unsafe_code = "deny"` remains the default, with
allowances on individual calls/initialized-result reads, each with a safety
comment. Buffer lengths and complete native structure sizes are checked. No
new dependency packages were added; the existing nix dependency enables its
macOS event feature.

macOS can hide environment data for SIP-protected binaries. If configured agent
launcher matching needs that data, capture refuses rather than silently choosing
a different profile. Setuid programs such as Apple's `top` can also hide required
foreground metadata from Herdr. Such capture failures leave prior snapshots
intact. Ordinary native agent executables and exec-style profile wrappers are
covered by tests; no SIP changes or root access are needed.

## Performance

Direct transport launches no Herdr or process-inspection subprocesses. Save
enumerates the process table once; preview performs no process inspection.
macOS restore checks direct shell children using `proc_listchildpids`, with
fresh identity checks, instead of repeating a whole-host scan per candidate.
Linux uses per-thread child lists with a complete-table fallback on unsupported
or racing reads; a native test checks children forked by another thread.
Periodic autosave sleeps in kqueue until a deadline or shutdown signal.

Reproduce the native benchmark with:

```sh
cargo build --release --locked
python3 scripts/benchmark_native.py --output benchmark-results/native.json
```

The script reuses the matched benchmark's private real Herdr fixtures at
1/10/50/100 panes, containing ordinary programs, idle shells and all five agent
types. It verifies every saved argv/cwd/session ID and every preview candidate,
then measures 30 fresh launches per case in randomized order. Raw results are
in [macos-native-2026-09-19.json](benchmarks/macos-native-2026-09-19.json).

| Measurement | Median | p95 |
| --- | ---: | ---: |
| macOS, 100-pane save | 62.01 ms | 69.00 ms |
| macOS, 100-pane preview | 15.37 ms | 16.32 ms |
| macOS, debounced event | 10.00 ms | 10.74 ms |

The separate [idle-check measurement](benchmarks/macos-idle-2026-09-19.json)
is 1.85 ms for 100 checks of a real idle shell, excluding host I/O and command startup.

## Linux regression comparison

The original `bdbd663` source and the candidate were built with Rust 1.98.1 on
the same Linux x86_64 host. Both binaries ran against the same real mixed-pane
fixtures, randomly interleaved with 30 fresh launches per case. The recorded
binary hashes identify the inputs; the baseline received only the identical
standalone benchmark example for the idle-check measurement.

| Measurement | Original | Candidate |
| --- | ---: | ---: |
| 100-pane save median | 39.63 ms | 39.51 ms |
| 100-pane preview median | 6.61 ms | 6.67 ms |
| 100 idle-shell checks median | 652.09 ms | 3.07 ms |

Save/preview differences are within the overlapping bootstrap intervals;
the measured idle-check operation is about 212 times faster. That ratio is
not an end-to-end restore speedup. The debounced-event medians were 1.25 ms
and 1.34 ms with overlapping intervals; no meaningful latency regression was
established by this run. See the [native comparison](benchmarks/linux-regression-2026-09-19.json)
and [idle comparison](benchmarks/linux-idle-2026-09-19.json) for all samples.

```sh
python3 scripts/benchmark_native.py --baseline-binary /path/to/original/herdr-revive
cargo build --release --example benchmark_idle --locked
python3 scripts/benchmark_idle.py --baseline-binary /path/to/original/benchmark_idle
```

Compile the same `examples/benchmark_idle.rs` against the original library for
the second command's baseline. It exercises the shared idle-shell API directly.

Wall time includes process launch and BSD time; RSS excludes the Herdr server.
These are warm local measurements, not end-to-end agent-resume latency, a
cross-hardware Linux comparison, or proof of a universal fastest claim.

## Validation

Tested on 2026-09-19, Darwin 25.6.0 arm64, Rust 1.98.1 and Herdr 0.9.1/protocol 22.
Native tests cover argv/environment boundaries, PID identity and ABI sizes;
shared contracts cover policy, persistence, recovery and transport deadlines.
Runtime suites exercise literal argv, both transports, background jobs,
redirection refusal, real OpenSSH, named layouts, host restarts, custom agent
profiles, protected-process refusal and timer shutdown. Installation/lifecycle
checks use a disposable registration and do not change the user's live registry.

Completed on Apple Silicon: 32 Rust tests (one additional ignored test is a
subprocess fixture), 23 isolated PTY integration cases, 13 real-host cases,
four real OpenSSH cases, lifecycle checks, formatting and Clippy. The headless
zsh PTY fixture disables ZLE because it has no terminal emulator.

Completed on Linux x86_64 / kernel 7.0.0: 30 Rust tests including the new
cross-thread child check, 22 integration cases, 12 real-host cases, four OpenSSH
cases, lifecycle checks and Clippy. Linux skips the unavailable zsh fixture and
the macOS-only protected-process case. The faster idle check exposed a new-pane
startup window with empty argv; reconstruction now retries observation until
validation succeeds or the existing deadline expires, and the large-layout
regression passes on both platforms.

Intel macOS is compile-checked with `cargo check --tests`; Intel runtime
qualification remains outstanding. Historical Linux qualification is in
[VALIDATION.md](VALIDATION.md). The macOS dependency graph adds no packages
beyond the existing Linux third-party license inventory (56 vs 57 packages).
