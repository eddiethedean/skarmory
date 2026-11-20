# Phase 0.1 packaged workload

The generated application uses a `src` layout, ordinary packages, absolute and
relative imports, a package re-export, and a named-module entry guard. The
entry calls each worker. A nested function calls its helper, and an unused
function's default expression calls its setup function when the module loads.
Every worker also contains isolated functions with reviewed expected findings.
This balances module activation, definition-time effects, and unreachable
bodies in one reproducible workload.

## Reproduce

On a POSIX host with Python 3.11 or later, run from the repository root.

```bash
cargo build --locked --release -p skarmory_cli
python benchmarks/phase01.py target/release/skarmory \
  --modules 96 --dead-per-module 24 --samples 9 \
  --workers 1 4 > phase01-results.json
```

The harness generates the sources before the first sample. Each sample starts
a fresh process, and sample order rotates across worker budgets. The native
preview has no analysis cache. The OS page cache is uncontrolled. The harness
checks the exact selected paths, counts, expected finding identities, and
complete coverage. It compares a digest of the full report across all runs,
including evidence and uncertainty.

The harness hashes the binary separately from its build sources. The source
digest covers the Cargo workspace and crate manifests, the lockfile, the
toolchain file, and the Rust files under each crate's `src` directory. The
workload digest covers the generated source paths and bytes. The base revision
and build-source digest identify a measured working tree before it is committed.

## Recorded run

The [macOS ARM64 receipt](results/phase01-macos-arm64.json) records Rust 1.98.1,
CPython 3.11.14, 12 logical cores, 24 GiB memory, exact build and workload
fingerprints, and all 18 raw observations. All runs completed the same 100
files and 8,360 lines, reported the same 2,400 expected findings, and recorded
385 evidence entries with zero uncertainties or coverage issues.

| Median per fresh process | 1 worker | 4 workers |
| --- | ---: | ---: |
| Wall time | 13.28 ms | 9.29 ms |
| User and system CPU time | 9.23 ms | 10.41 ms |
| Peak RSS | 7.56 MiB | 7.67 MiB |
| Discovery | 0.83 ms | 0.81 ms |
| Read and decode | 5.35 ms | 5.47 ms |
| Parse and extract | 0.96 ms | 1.45 ms |
| Resolution and execution analysis | 0.97 ms | 0.95 ms |
| Reachability evidence | 0.08 ms | 0.09 ms |
| Finding generation | 0.12 ms | 0.11 ms |
| Report output | 0.50 ms | 0.50 ms |

File-local stages sum time spent across workers. The phase medians describe
individual stages, while wall time includes process startup and snapshot
verification. These timings describe a synthetic native workload on one host.
The overlapping Skylos fixture behavior is recorded separately. [Pinned
Skylos comparison](../docs/phase-01-reference.md)
