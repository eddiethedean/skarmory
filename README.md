# Skarmory

**Fast Python code analysis, powered by Rust.**

Skarmory is building a native Rust analysis engine with a Python API and a
native command-line interface. The initial compatibility target is Skylos
5.7.0's Python analysis behavior: useful findings, framework awareness,
explainable confidence, and explicit analysis limits.

## Status

Skarmory 0.0.1 is the first Phase 0 release. The native CLI and Python SDK
report unreachable functions with evidence and explicit coverage for a flat,
closed-world Python 3.11 application. See the [prototype contract](docs/phase-0.md)
for the supported source subset and the [benchmark report](benchmarks/phase0.md)
for comparisons with the pinned Skylos reference.

## Install 0.0.1

Install the Python SDK from PyPI in a virtual environment.

```bash
python -m pip install "skarmory==0.0.1"
```

Install the native CLI from crates.io with Rust 1.96 or newer.

```bash
cargo install --locked --version 0.0.1 skarmory_cli
skarmory --version
```

The SDK ships eight wheels for standard CPython 3.11+ and a source
distribution. The [0.0.1 release notes](CHANGELOG.md#001) list the
wheel tags and all four published crates.

## Plans

| Document | Contents |
| --- | --- |
| [Architecture](docs/architecture.md) | Shared Rust engine, Python semantics, project graph, caching, evidence, and integration boundaries |
| [Roadmap](docs/roadmap.md) | Phases, dependencies, acceptance gates, and initial engineering tasks |
| [Planned Rust dependencies](docs/roadmap.md#planned-rust-dependencies) | Proposed crates and adoption checks by milestone |
| [Compatibility](docs/compatibility.md) | Pinned Skylos target, promised surfaces, exclusions, and discrepancy policy |
| [Architecture decision](docs/adr/0001-native-engine.md) | Why the CLI and Python SDK share a native engine |
| [Conformance strategy](tests/conformance/README.md) | Differential compatibility and independently reviewed correctness cases |
| [Benchmark strategy](benchmarks/README.md) | Matched workloads, cache states, accuracy gates, and unmeasured performance goals |

The next milestone expands the prototype into the Python dead-code preview.

## Repository layout

| Path | Role |
| --- | --- |
| `crates/analysis_core` | `skarmory_core`: shared native engine boundary |
| `crates/python_frontend` | `skarmory_python`: Ruff parser adapter and prototype fact extraction |
| `crates/cli` | `skarmory_cli`: native `skarmory` executable |
| `crates/python_bindings` | `skarmory_bindings`: PyO3 extension installed as `skarmory._native` |
| `python/skarmory` | Python API, typing information, and development version |
| `tests/python` | Source and installed-wheel tests |
| `tests/conformance` | Reviewed fixtures and pinned reference metadata |
| `benchmarks` | Measurement harness, results, and evaluation plan |
| `docs` | Design documents and delivery roadmap |

## Development

Use Python 3.11 or newer and Rust 1.98 or newer. The local toolchain file
pins Rust 1.98.1 with `rustfmt` and `clippy`. CI checks and release builds use
the same Rust patch release.

### Native CLI

```bash
cargo run --locked -p skarmory_cli -- --help
cargo run --locked -p skarmory_cli -- tests/conformance/fixtures/reachable
```

The CLI emits one JSON report per scan. A complete report exits 0. Incomplete
coverage is explicit and exits 2.

### Python SDK

In a virtual environment, build and install the local package:

```bash
python -m pip install .
python -c "import skarmory; print(skarmory.analyze('tests/conformance/fixtures/reachable'))"
```

Maturin builds the native extension from source. The SDK returns a report
dictionary with a `coverage.status` field. Install the native CLI separately with
`cargo install --locked --path crates/cli`. The combined-wheel layout is an
experiment documented in the [Phase 0 evidence](docs/phase-0.md#distributions).

### Checks

```bash
python scripts/check_structure.py
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace
cargo build --locked -p skarmory_cli
python tests/conformance/check_phase0.py target/debug/skarmory
python -m unittest discover -s tests/python -v
```

The Python test command uses the installed SDK. Run the source-only failure
check on POSIX with:

```bash
PYTHONPATH=python python -m unittest discover -s tests/python -v
```

CI sets `SKARMORY_REQUIRE_NATIVE=1` for installed-wheel tests, so a missing
extension cannot be hidden by the source-only test mode. The packaging matrix covers Linux, macOS, and Windows and includes a source
distribution rebuild.

## Compatibility and performance

The reference is [Skylos 5.7.0](https://github.com/duriantaco/skylos/releases/tag/v5.7.0),
commit `87978e69adf91b3811e97b00c40aa39d7937cd94`. The machine-readable baseline is
in [tests/conformance/baselines/skylos-5.7.0.toml](tests/conformance/baselines/skylos-5.7.0.toml).

Ruff is an architectural and responsiveness reference. Whole-project
reachability and security analysis have different workloads from local linting;
all speed targets in this repository are provisional and unmeasured. A faster
run with missing rules, fewer analyzed files, or incomplete coverage is not a
matched performance result.

Skarmory is an independent project distributed under the [MIT license](LICENSE). See [CONTRIBUTING.md](CONTRIBUTING.md) for
the development workflow and rules for preserving compatibility evidence.
