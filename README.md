# Skarmory

**Fast Python code analysis, powered by Rust.**

Skarmory is building a native Rust analysis engine with a Python API and a
native command-line interface. The initial compatibility target is Skylos
5.7.0's Python analysis behavior: useful findings, framework awareness,
explainable confidence, and explicit analysis limits.

## Status

**This repository is an initial development scaffold. Analysis is not
implemented.** The native CLI supports help and version output; a scan request
exits with status 2. The Python API raises `AnalysisUnavailableError`. Neither
entry point reports an empty successful scan.

The repository contains a Rust workspace, a PyO3-backed Python package, build
and smoke-check CI, and the implementation plans. It has no implemented parser,
analysis rules, Skylos compatibility guarantee, or measured speedup yet.

## Plans

| Document | Contents |
| --- | --- |
| [Architecture](docs/architecture.md) | Shared Rust engine, Python semantics, project graph, caching, evidence, and integration boundaries |
| [Roadmap](docs/roadmap.md) | Phases, dependencies, acceptance gates, and initial engineering tasks |
| [Compatibility](docs/compatibility.md) | Pinned Skylos target, promised surfaces, exclusions, and discrepancy policy |
| [Architecture decision](docs/adr/0001-native-engine.md) | Why the CLI and Python SDK share a native engine |
| [Conformance strategy](tests/conformance/README.md) | Differential compatibility and independently reviewed correctness cases |
| [Benchmark strategy](benchmarks/README.md) | Matched workloads, cache states, accuracy gates, and unmeasured performance goals |

The first implementation milestone is a vertical slice from Python source
discovery through scoped symbols and project reachability to evidence-bearing
dead-code findings.

## Repository layout

| Path | Role |
| --- | --- |
| `crates/analysis_core` | `skarmory_core`: shared native engine boundary |
| `crates/python_frontend` | `skarmory_python`: future parser adapter and semantic extraction |
| `crates/cli` | `skarmory_cli`: native `skarmory` executable |
| `crates/python_bindings` | `skarmory_bindings`: PyO3 extension installed as `skarmory._native` |
| `python/skarmory` | Python API, typing information, and development version |
| `tests/python` | Source and installed-wheel smoke tests |
| `tests/conformance` | Compatibility plan and pinned reference metadata |
| `benchmarks` | Performance and accuracy evaluation plan |
| `docs` | Design documents and delivery roadmap |

## Development

Use Python 3.11 or newer and a Rust toolchain with `rustfmt` and `clippy`.
The workspace declares Rust 1.85 as its intended minimum; minimum-version
qualification remains a Phase 0 task. The toolchain file selects stable Rust.

### Native CLI

```bash
cargo run -p skarmory_cli -- --help
cargo run -p skarmory_cli -- --version
```

`cargo run -p skarmory_cli -- .` currently reports that analysis is not
implemented and exits with status 2.

### Python SDK

In a virtual environment, build and install the local package:

```bash
python -m pip install .
python -c "import skarmory; print(skarmory.__version__)"
```

The package builds the native extension with Maturin. A compiler and Rust are
required for this source installation. A request such as `skarmory.analyze('.')`
currently raises `skarmory.AnalysisUnavailableError`.

**The SDK wheel does not yet install the native CLI.** The CLI is built
separately through Cargo. A single-install distribution containing both remains
an explicit Phase 0 packaging experiment.

### Checks

```bash
python scripts/check_structure.py
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
python -m unittest discover -s tests/python -v
```

The Python test command uses the installed SDK. Without a native build, the
source-only smoke test can be run on POSIX with:

```bash
PYTHONPATH=python python -m unittest discover -s tests/python -v
```

CI sets `SKARMORY_REQUIRE_NATIVE=1` for installed-wheel tests, so a missing
extension cannot be hidden by the source-only test mode. CI also builds the
source distribution and tests the native CLI on Linux, macOS, and Windows.

## Compatibility and performance

The reference is [Skylos 5.7.0](https://github.com/duriantaco/skylos/releases/tag/v5.7.0),
commit `87978e69adf91b3811e97b00c40aa39d7937cd94`. The machine-readable baseline is
in [tests/conformance/baselines/skylos-5.7.0.toml](tests/conformance/baselines/skylos-5.7.0.toml).

Ruff is an architectural and responsiveness reference. Whole-project
reachability and security analysis have different workloads from local linting;
all speed targets in this repository are provisional and unmeasured. A faster
run with missing rules, fewer analyzed files, or incomplete coverage is not a
matched performance result.

Skarmory is an independent project. See [CONTRIBUTING.md](CONTRIBUTING.md) for
the development workflow and rules for preserving compatibility evidence.
