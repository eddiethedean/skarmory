# Skarmory

[![PyPI version](https://img.shields.io/pypi/v/skarmory?logo=pypi&logoColor=white)](https://pypi.org/project/skarmory/)
[![CI](https://github.com/eddiethedean/skarmory/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/eddiethedean/skarmory/actions/workflows/ci.yml)
[![Python SDK supports CPython 3.11+](https://img.shields.io/badge/CPython-3.11%2B-3776ab?logo=python&logoColor=white)](https://pypi.org/project/skarmory/)
[![CLI builds with Rust 1.98+](https://img.shields.io/badge/Rust-1.98%2B-orange?logo=rust&logoColor=white)](https://crates.io/crates/skarmory_cli)
[![MIT license](https://img.shields.io/badge/license-MIT-green)](https://github.com/eddiethedean/skarmory/blob/main/LICENSE)

**Find dead code in Python projects with a native Rust analyzer.**

The native CLI and Python SDK share a Rust engine that reports findings,
reachability evidence, uncertainty, and analysis coverage.

Skarmory 0.2.0 adds a Skylos 5.7.0 compatibility profile for a defined subset
of Python 3.11 dead-code analysis. It detects unreachable functions, unused
classes, methods, imports, variables, and parameters, and empty Python files.
It models selected pytest and FastAPI patterns. [0.2.0 release notes](CHANGELOG.md#020)

## Install

Install the [Python SDK](https://pypi.org/project/skarmory/) in a virtual
environment. Wheels support standard CPython 3.11+ on x86-64 and ARM64 for Linux
(glibc and musl), macOS, and Windows. A source distribution is also available.

```bash
python -m pip install "skarmory==0.2.0"
```

Install the [native CLI](https://crates.io/crates/skarmory_cli) with Rust 1.98
or newer. The installed executable runs independently of Python.

```bash
cargo install --locked --version 0.2.0 skarmory_cli
skarmory --version
```

## Quick start

With the installed native CLI, run the Skylos compatibility profile from the
root of your Python project.

```sh
skarmory --profile skylos . --json
```

With the installed Python SDK, call the same compatibility profile. The API
returns a JSON string.

```python
import json
from skarmory.compat.skylos import analyze

report = json.loads(analyze("."))
if report["coverage"]["status"] == "complete":
    print(report["unused_functions"])
else:
    print(report["analysis_errors"])
```

The CLI exits `0` on a complete scan by default. With `--strict`, it exits `1`
when a complete scan reports findings. Exit `2` indicates incomplete analysis,
an invalid invocation, or an output error. Use `--sarif results.sarif.json` to
also write SARIF and `--cache-dir ../analysis-cache` to reuse file-local facts.
Each scan rebuilds project resolution from the current sources and configuration.

## Scan with explicit entry points

Use the default native profile to find unreachable functions from declared
entry points or library exports. Run this from the root of a project with an
entry at `src/example/main.py`.

```sh
skarmory . --source-root src --entry-module example.main
```

The Python SDK accepts the same source root and entry.

```python
import skarmory

report = skarmory.analyze(
    ".",
    source_roots=["src"],
    entry_modules=["example.main"],
)

coverage = report["coverage"]
if coverage["status"] == "complete":
    for finding in report["findings"]:
        print(finding["path"], finding["line"], finding["symbol"])
else:
    print(coverage["issues"])
```

For a library, declare the callables that external users can reach. You can
also declare public modules with resolvable literal `__all__` lists.

```sh
skarmory . --source-root src --policy library --public-callable example:open_file
```

## Read the native report

- **Findings** identify functions with no reachable path from the declared
  entries or exports in a complete scan.
- **Evidence and uncertainty** explain why definitions were retained and flag
  functions that may be used.
- **Coverage** records analyzed sources and reports reasons the scan could not
  finish, including required imports outside the selected sources, unsupported
  syntax, and exceeded analysis limits.

The CLI writes JSON to stdout. It exits `0` for a complete scan, including
scans with findings. Exit `2` signals incomplete analysis or an invocation or
output error. The SDK exposes completeness through `coverage.status`.

The native profile implements `unreachable_function` within a closed set of
Python 3.11 sources. Supported syntax, import resolution, and analysis limits
are defined in the [native scan contract](docs/phase-01.md).

## Features in version 0.2.0

The released Skylos compatibility profile supports these Python dead-code
checks within its declared analysis scope.

| Feature | Supported behavior |
| --- | --- |
| Functions | Unreachable functions and recursive groups |
| Classes and methods | Module-level classes and selected method dispatch |
| Imports, variables, parameters | Supported lexical bindings, reads, and aliases |
| Empty Python files | Empty, comment-only, and docstring-only files, with entry-file exemptions |
| Public API | Explicit re-exports, literal `__all__`, and selected literal lazy exports |
| Frameworks | Selected pytest discovery, fixtures, and FastAPI routing models |
| Finding controls | Confidence thresholds, whitelists, and inline ignores |
| Reports and automation | JSON, SARIF, strict exit status, and opt-in file-fact caching |

See the [0.2.0 release contract](https://github.com/eddiethedean/skarmory/blob/v0.2.0/docs/phase-02.md)
for the supported syntax, interfaces, limitations, and evidence. The
[reference comparison](docs/phase-02-reference.md) records measured
compatibility with Skylos 5.7.0.

Opaque lazy exports, dynamic plugin loading, custom import hooks, and
unresolved receiver dispatch can make a scan incomplete when they affect
reachability.

## Development preview

The `main` branch contains work toward Phase 0.3. These features require a
source build, and the Phase 0.3 compatibility gate is still pending. The
published 0.2.0 packages do not expose the `skylos-0.3` selector.

For example, build and run the current CLI from the Skarmory repository root
to check unused Python runtime dependencies in a target project.

```sh
cargo run --locked -p skarmory_cli -- --profile skylos-0.3 --quality /path/to/project --json
```

See the [Phase 0.3 plan and current preview behavior](docs/phase-03.md) and
the [supported dependency declarations](docs/phase-03.md#unused-python-dependencies).

## Roadmap

Phase 0.3 expands Python dead-code analysis and workflows. Its release gate
requires every in-scope pinned Python dead-code case and required variant to
pass. Python quality, security, and developer workflows follow in 0.4–0.6.

Starting in Phase 0.7, each additional language frontend has a dedicated Rust
crate and release milestone. JavaScript and TypeScript share one frontend.
See the [roadmap](docs/roadmap.md) and [language milestone plan](docs/language-frontends.md).

## Development

Use Python 3.11 or newer. The repository pins Rust 1.98.1 with `rustfmt`
and `clippy`. Run the CLI from source and install the development SDK in a
virtual environment.

```bash
cargo run --locked -p skarmory_cli -- tests/conformance/fixtures/reachable
python -m pip install .
```

### Checks

```bash
python scripts/check_structure.py
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace
cargo build --locked -p skarmory_cli
python tests/conformance/check_phase0.py target/debug/skarmory
python tests/conformance/check_phase01.py target/debug/skarmory
python tests/conformance/check_phase02.py target/debug/skarmory
python -m unittest discover -s tests/python -v
```

The Phase 0.2 command runs the Skarmory fixtures. The pinned Skylos comparison
requires a separate reference environment. [Reproduction instructions](docs/phase-02-reference.md#reproduce)

Run the Python tests after installing the SDK. On POSIX, verify the
source-only error path separately.

```bash
PYTHONPATH=python python -m unittest discover -s tests/python -v
```

### Project documentation

- [Phase 0.2](docs/phase-02.md) describes the Skylos profile, historical feature
  comparison, and changes after release.
- [Phase 0.3](docs/phase-03.md) plans expanded dead-code coverage, verification,
  and workflows. The development profile includes
  [Git diff, impact, baselines, and gates](docs/phase-03.md#dead-code-diff-baselines-and-gates).
- [Contributing](CONTRIBUTING.md) covers development, testing, and publishing.
- [Architecture](docs/architecture.md) describes the Rust engine and Python SDK.
- [Roadmap](docs/roadmap.md) lists milestones and release gates.
- [Language frontends](docs/language-frontends.md) describes the planned crates
  and independent language release phases.
- [Compatibility](docs/compatibility.md) defines the Skylos comparison policy.
- [Conformance](tests/conformance/README.md) and [benchmarks](benchmarks/README.md)
  document how correctness, coverage, and performance are measured.
