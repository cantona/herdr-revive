# Matched real-host benchmark

Measured 2026-09-18T03:36:08Z: unchanged herdr-resurrect versus the reviewed
herdr-revive 0.1.0 binary on the same disposable real Herdr sessions. At 100
panes, direct Rust saved in **40.69 ms versus 363.82 ms (8.9× faster)**
and previewed in **7.43 ms versus 161.65 ms (21.8× faster)**.
Median maximum process RSS for saving was **5.28 MiB versus 72.57 MiB**.
These compare whole implementations; they do not isolate programming language.

## Results

Wall time in milliseconds: median (p95). Each cell represents 30 fresh process
launches. Direct is the new plugin's default transport.

| Panes / operation | JavaScript CLI | Rust CLI | Rust direct |
| --- | ---: | ---: | ---: |
| 1 save | 162.20 (167.43) | 17.00 (17.38) | 13.01 (13.60) |
| 1 preview | 120.78 (125.97) | 3.79 (3.99) | 1.75 (2.19) |
| 10 save | 183.30 (188.58) | 32.68 (38.62) | 15.59 (16.54) |
| 10 preview | 122.13 (128.29) | 3.96 (4.35) | 2.23 (2.84) |
| 50 save | 265.26 (275.79) | 102.77 (125.99) | 25.97 (28.32) |
| 50 preview | 141.35 (146.93) | 10.06 (11.25) | 4.40 (5.29) |
| 100 save | 363.82 (388.00) | 183.82 (225.45) | 40.69 (44.91) |
| 100 preview | 161.65 (167.01) | 10.86 (12.15) | 7.43 (8.83) |
| Debounced event | 95.25 (100.14) | 1.46 (1.65) | 1.42 (1.67) |
| Completed-boot event | 96.72 (100.13) | 1.44 (1.65) | 1.38 (1.64) |

Events use the one-pane fixture with autosave enabled and a recent save. The
debounced case disables automatic restore; the completed-boot case enables it
with an already completed boot claim, then also reaches save debounce. Neither
executes a restore. Total: **900 measured launches across 30 cases**.

| 100-pane operation / variant | Median max RSS MiB | Herdr children | Process-scan children | Socket connections |
| --- | ---: | ---: | ---: | ---: |
| Save / JavaScript CLI | 72.57 | 101 | 1 | 202 |
| Save / Rust CLI | 8.85 | 101 | 0 | 214 |
| Save / Rust direct | 5.28 | 0 | 0 | 114 |
| Preview / JavaScript CLI | 68.10 | 16 | 0 | 32 |
| Preview / Rust CLI | 8.82 | 1 | 0 | 2 |
| Preview / Rust direct | 5.54 | 0 | 0 | 2 |

Socket counts include protocol and peer-identity connections, not just logical
requests. Rust's CLI mode still exports layouts through direct IPC, so its save
path is a hybrid. Both Rust no-op event paths spawn no children and make one
peer-identity connection; JavaScript makes no children or socket connections.

## Workload and measurement

Four isolated Herdr 0.9.1/protocol 22 servers remain running throughout the
measurement. All variants use the same live panes for a given size, with separate
private configuration and state. All 161 panes coexist, so global process scans
see the complete fixture population.

| Panes | Workspaces | Tabs | Programs | Idle shells | Agents |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 1 | 1 | 1 | 1 | 0 | 0 |
| 10 | 1 | 1 | 4 | 1 | 5 |
| 50 | 3 | 5 | 20 | 5 | 25 |
| 100 | 5 | 10 | 40 | 10 | 50 |

Program fixtures include ordinary processes and harmless stand-ins for SSH,
minicom and Node. All five supported agent names carry explicit UUIDs. These
executables only wait for signals; no actual agents or network commands run.
Arguments exercise spaces, quotes, shell metacharacters and Unicode. Every saved
pane ID, cwd, program argv and agent reference, and every preview candidate ID,
is checked against the expected fixture outside the timed interval.

Configuration matches retention (20), wildcard fixture policy, zero settle delay
and one-day debounce. Each operation is warmed independently. All variants,
operations and sizes are randomized together with seed 20260918. Timings include
fresh process launch and the same GNU time wrapper. Strace runs separately once
per case; its overhead is excluded from timing. Successful child counts are
asserted, including syscalls split into unfinished/resumed trace records.

RSS is GNU time's maximum process/waited-child measurement, excluding the Herdr
server. It is not summed concurrent memory or PSS. CPU values are coarsely rounded
and exclude server work. Raw data includes individual samples, order, min/max,
p95 and 2,000-resample bootstrap intervals for medians. These intervals describe
within-run variation, not repeated independent machines or load conditions.

## Provenance and limits

- Intel Core i7-10700 at 2.90 GHz, 16 logical CPUs; Linux 7.0.0-30-generic x86_64.
- Node v22.22.1; Rust 1.97.1 (8bab26f4f, 2026-07-14).
- Baseline: [cantona/herdr-resurrect at c05b43d](https://github.com/cantona/herdr-resurrect/tree/c05b43dda3ff968cb21653b7f4188245015a15be).
- Baseline tracked-source SHA-256: `1b98aaa8a1b6de71f2d0fc7733feb9a8d4299b2c6731bbdf00bfb465786ebe5a`.
- Rust binary: 2,333,400 bytes; SHA-256 `0caa8acb3077253c08fe31576a2806be1309f12bf010889cc6a739fd5170c3d3`.
- Load averages: start 1.16 / 1.06 / 1.69; end 1.34 / 1.14 / 1.67.

The harness uses private pinned copies of baseline sources and the Rust binary,
checking hashes before and after measurement. The original baseline remains
clean and unchanged. It never registers either plugin in the normal registry.

Durability, validation, current-policy checks and layout capture differ: Rust
syncs durable state and exports complete split trees; the baseline uses different
storage and geometry paths. No optimized JavaScript direct-IPC/caching variant
was tested. These results support choosing between these implementations on this
warm workload, not attributing the entire gain to Rust.

Load was observed, not controlled. An earlier development run showed noticeable
run-to-run variation, especially for CLI transport. The linked
[raw measurements](benchmarks/linux-matched-2026-09-18.json) contain the final
matched run; development samples are not pooled with it.

Cold-cache startup, controlled-load repetitions, aggregate burst memory and
end-to-end restore/readiness timing remain unmeasured by this benchmark. Feature
coverage is assessed separately in [PARITY.md](PARITY.md); remaining acceptance
and release gates are in [VALIDATION.md](VALIDATION.md).

## Separate correctness probe

An untimed one-pane probe adds an empty argument. JavaScript captured five of
six argv elements; both Rust transports preserved all six. The baseline consumes
Herdr's process-info argv, whose Linux parser filters empty NUL-separated parts
(`../herdr/src/platform/linux.rs`, `process_argv`). The timed common workload
excludes empty arguments so all implementations capture equivalent commands.
Neither Herdr nor the baseline was modified to conceal this difference.

## Reproduction and review

Requires the built Rust release, a clean sibling herdr-resurrect checkout, Herdr,
Node, Python 3, a C compiler, GNU time, strace, and permission to create private
Unix sockets and trace child processes.

```sh
cargo build --release --locked
python3 tests/benchmark_contracts.py
python3 scripts/benchmark_matched.py --baseline ../herdr-resurrect \
  --samples 30 --panes 1 10 50 100 --output benchmark-results/matched.json
```

Adversarial review identified two confirmed harness defects: incomplete parsing
of interleaved strace records and insufficient pinning of measured inputs. Both
were fixed before the final full run and re-reviewed. Three trace-parser
regression tests pass. The annotator `review_changes` MCP was unavailable, so
that separate checkpoint was skipped explicitly.

This publication run uses the reviewed release binary and current executable name.
