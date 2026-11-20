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
