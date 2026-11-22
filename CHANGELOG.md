# Changelog

## 0.2.0

- Adds the Skylos profile to the native CLI and Python SDK for a declared
  subset of Skylos 5.7.0 Python dead-code analysis. It reports unreachable
  functions and unused classes, methods, imports, variables, parameters, and
  empty files in supported Python 3.11 projects.
- Models scoped bindings, explicit re-exports, literal `__all__`, and selected
  lazy exports, pytest fixtures, FastAPI routes, and callback patterns.
- Supports Skylos config discovery, confidence filtering, suppressions, JSON
  reports, and SARIF output. The CLI's `--strict` mode exits `1` for findings in
  a complete scan. Incomplete scans retain coverage diagnostics and exit `2`.
- Adds opt-in caching for file-local facts with fresh project resolution on
  every scan.

See the [Phase 0.2 scope and limits](docs/phase-02.md), the
[pinned reference results](docs/phase-02-reference.md), and the
[cache benchmark](benchmarks/phase02.md).

## 0.1.0

- Adds a packaged-project preview for the `unreachable_function` rule with
  explicit application entries and bounded library exports.
- Reports the analyzed source set, reachability evidence, uncertainty, and
  coverage through the native CLI and Python SDK.
- Supports bounded file discovery and parallel source processing with
  deterministic reports.
- Resolves ordinary imports of a local `__future__` module and keeps Boolean
  identity comparisons distinct from equality.
- Preserves uncertainty for speculative recursion and complete coverage for
  bounded escapes of containers with unrelated callback values.
- Raises the minimum Rust version to 1.98.

The [Phase 0.1 contract](docs/phase-01.md) documents the supported subset and
the historical prototype mode. The [reference comparison](docs/phase-01-reference.md)
records the two fixtures checked against pinned Skylos 5.7.0.

## 0.0.1

Released November 20, 2026.

- Introduces the native CLI and Python SDK for detecting unreachable
  functions in a flat, closed-world Python 3.11 application.
- Reports findings, uncertainty, and incomplete coverage in the same versioned
  JSON format across both interfaces.

See the [Phase 0 contract](docs/phase-0.md) for supported syntax and scan limits.

### Published artifacts

- PyPI has eight `cp311-abi3` wheels for standard CPython 3.11+ and one source
  distribution. [PyPI files](https://pypi.org/project/skarmory/0.0.1/#files)
- crates.io has four workspace crates at version 0.0.1. Install `skarmory_cli`
  for the native `skarmory` command. [`skarmory_cli`](https://crates.io/crates/skarmory_cli/0.0.1) ·
  [`skarmory_core`](https://crates.io/crates/skarmory_core/0.0.1) ·
  [`skarmory_python`](https://crates.io/crates/skarmory_python/0.0.1) ·
  [`skarmory_bindings`](https://crates.io/crates/skarmory_bindings/0.0.1)

| Platform | x86-64 wheel tag | ARM64 wheel tag |
| --- | --- | --- |
| Linux (glibc) | `manylinux_2_17_x86_64` | `manylinux_2_28_aarch64` |
| Linux (musl) | `musllinux_1_2_x86_64` | `musllinux_1_2_aarch64` |
| macOS | `macosx_10_12_x86_64` | `macosx_11_0_arm64` |
| Windows | `win_amd64` | `win_arm64` |

All 19 release jobs passed on commit `602aec4`, including all eight wheel
builds, installation checks, and publication to both registries.
[`v0.0.1` source tag](https://github.com/eddiethedean/skarmory/tree/v0.0.1) ·
[Release CI](https://github.com/eddiethedean/skarmory/actions/runs/44782566713)
