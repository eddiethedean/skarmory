# Skarmory

[![PyPI version](https://img.shields.io/pypi/v/skarmory?logo=pypi&logoColor=white)](https://pypi.org/project/skarmory/)
[![CI](https://github.com/eddiethedean/skarmory/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/eddiethedean/skarmory/actions/workflows/ci.yml)
[![Python SDK supports CPython 3.11+](https://img.shields.io/badge/CPython-3.11%2B-3776ab?logo=python&logoColor=white)](https://pypi.org/project/skarmory/)
[![CLI builds with Rust 1.98+](https://img.shields.io/badge/Rust-1.98%2B-orange?logo=rust&logoColor=white)](https://crates.io/crates/skarmory_cli)
[![MIT license](https://img.shields.io/badge/license-MIT-green)](https://github.com/eddiethedean/skarmory/blob/main/LICENSE)

**Find unreachable functions in Python projects with a native Rust analyzer.**

Skarmory 0.1.0 ships a dead-code preview for packaged Python 3.11 projects.
Choose the source roots and entry points of an application or the public API of
a library. The native CLI and Python SDK use the same engine and return the
same versioned report, with findings, reachability evidence, uncertainty, and
coverage. See the [0.1.0 release notes](https://github.com/eddiethedean/skarmory/blob/main/CHANGELOG.md#010).

## Install

Install the [Python SDK](https://pypi.org/project/skarmory/) in a virtual
environment. Wheels support standard CPython 3.11+ on x86-64 and ARM64 for Linux
(glibc and musl), macOS, and Windows. A source distribution is also available.

```bash
python -m pip install "skarmory==0.1.0"
```

Install the [native CLI](https://crates.io/crates/skarmory_cli) with Rust 1.98
or newer. The installed executable runs independently of Python.

```bash
cargo install --locked --version 0.1.0 skarmory_cli
skarmory --version
```

## Quick start

Run this from the root of a project with a package entry at
`src/example/main.py`.

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

## Read the report

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

The 0.1.0 preview implements one rule, `unreachable_function`, within a closed
set of Python 3.11 sources. See the [scan contract](https://github.com/eddiethedean/skarmory/blob/main/docs/phase-01.md)
for supported syntax, import resolution, and scan limits. The [Skylos 5.7.0
comparison](https://github.com/eddiethedean/skarmory/blob/main/docs/phase-01-reference.md)
records the two fixtures checked against the pinned baseline.

## Dead-code features

The table compares released Skarmory 0.1.0 and its in-development Phase 0.2
profile with the pinned Skylos 5.7.0 baseline. A dash means the feature is
unavailable in that version or profile.

| Feature | Skarmory 0.1.0 | Skarmory Phase 0.2 (in development) | Skylos 5.7.0 |
| --- | --- | --- | --- |
| Unreachable functions | Bounded reachability from declared roots | Functions and unreachable groups within the supported call graph | Unused functions and unreachable groups |
| Unused methods and classes | — | Module-level classes and selected method dispatch patterns | Unused classes and methods |
| Unused imports | — | Supported import bindings and aliases | Yes |
| Unused variables | — | Supported lexical bindings | Yes |
| Unused parameters | — | Supported function signatures and reads | Yes |
| Unused declared dependencies | — | — | Python dependency check (`SKY-U005`) with quality scanning |
| Empty Python files | — | Empty, comment-only, and docstring-only files, with entry-file exemptions | Yes |
| Library exports | Declared public callables and resolvable literal `__all__` | Explicit re-exports, literal `__all__`, and selected literal lazy exports | Public API and export detection |
| pytest | — | Statically collected tests and selected fixture patterns | Test and fixture awareness |
| FastAPI | — | Selected routes and router registration | Framework-aware analysis |
| Django and Flask | — | — | Framework-aware analysis |
| Finding controls | — | Confidence thresholds, whitelists, and inline ignores | Confidence thresholds, whitelists, and inline ignores |
| Grep verification and runtime evidence | — | — | Text-based verification and optional tracing or coverage |
| Reports | Native JSON | Compatibility JSON, strict CLI exit status, and SARIF | JSON, strict CLI exit status, and SARIF |
| Git diff and baseline workflows | — | — | Changed-file scans, baselines, and configurable gates |
| Cleanup workflow | — | — | Review queue and patch previews for supported findings |
| JavaScript and TypeScript dead code | — | — | Unused files (`SKY-E003`) and unnecessary exports (`SKY-E004`) |

Phase 0.2 also has limits within its supported Python features. Opaque lazy
exports, dynamic plugin loading, custom import hooks, and unresolved receiver
dispatch can make a scan incomplete when they affect reachability.

See the [Phase 0.2 scope](docs/phase-02.md),
[tested matches and differences](docs/phase-02-reference.md), and
[Skylos 5.7.0 reference inventory](docs/skylos-5.7.0-contract.md) for details.
The [Phase 0.3 plan](docs/phase-03.md) schedules the missing dead-code features.
Its release gate requires all pinned Skylos dead-code tests to pass against
Skarmory.

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
python -m unittest discover -s tests/python -v
```

Run the Python tests after installing the SDK. On POSIX, verify the
source-only error path separately.

```bash
PYTHONPATH=python python -m unittest discover -s tests/python -v
```

### Project documentation

- [Phase 0.2](docs/phase-02.md) describes the development Skylos profile and
  opt-in module-fact cache.
- [Phase 0.3](docs/phase-03.md) plans expanded dead-code coverage, verification,
  and workflows.
- [Contributing](CONTRIBUTING.md) covers development, testing, and publishing.
- [Architecture](docs/architecture.md) describes the Rust engine and Python SDK.
- [Roadmap](docs/roadmap.md) lists milestones and release gates.
- [Compatibility](docs/compatibility.md) defines the Skylos comparison policy.
- [Conformance](tests/conformance/README.md) and [benchmarks](benchmarks/README.md)
  document how correctness, coverage, and performance are measured.
