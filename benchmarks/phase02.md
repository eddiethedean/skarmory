# Phase 0.2 cache benchmark

The harness generates a packaged application and a library with the same
worker modules. The application declares a module entry, and the library has
a literal public export through a re-export. Each workload has an unchanged
state and three separate edits. The leaf edit changes a function body, the root
edit switches the application's entry path or adds a library root, and the
re-export edit changes the target of a stable public name.

## Reproduce

Run from the repository root on a POSIX host with Python 3.11 or later.

```bash
cargo build --locked --release -p skarmory_cli
python benchmarks/phase02.py target/release/skarmory \
  --modules 32 --samples 5 --workers 1 4 > phase02-results.json
```

The default uses the Phase 0.2 Skylos report profile. Pass `--profile native`
to measure the native preview with the same cache. For each workload,
repetition, and worker budget the harness checks cache-disabled, cold, warm,
and edited-cache runs against a clean scan of the same state. Each measurement
starts a fresh CLI process. Cold runs start with an empty analysis cache.
Page-cache state is uncontrolled.

The receipt retains wall and CPU time, peak RSS, coverage, cache metrics,
output fingerprints, and errors. The harness reports success only when
compared runs select the same expected files, complete, and agree on the full
semantic report. Cache metrics and native phase timings are outside that
report comparison. The cold and warm runs must also demonstrate writes and
hits. Source and binary fingerprints identify the exact measured workload
and build.

These generated workloads measure cache behavior and equivalence within
Skarmory. The pinned Skylos differential corpus and independent holdout
review establish separate correctness and compatibility evidence.

## Recorded run

The [macOS ARM64 receipt](results/phase02-cache-macos-arm64.json) records
Skarmory 0.2.0 at source revision `bcc5ab3`, Rust 1.98.1, CPython 3.11.14,
an Apple M4 Pro with 12 logical cores and 24 GiB of memory, and 180
fresh-process observations.
All scans completed and matched the clean report of the same state. The
application selected 36 files and the library selected 35.

| Unchanged workload | Workers | Cache disabled | Cold cache | Warm cache |
| --- | ---: | ---: | ---: | ---: |
| Application | 1 | 6.27 ms | 10.58 ms | 20.00 ms |
| Application | 4 | 4.45 ms | 6.91 ms | 10.09 ms |
| Library | 1 | 5.68 ms | 10.27 ms | 18.80 ms |
| Library | 4 | 4.26 ms | 6.50 ms | 10.73 ms |

Values are median wall times from five samples per case. Warm scans hit
every module fact record. Cache use increased median time for these small
unchanged workloads on this host. The raw receipt includes cold and warm
activity and cached-versus-clean results after each edit.
