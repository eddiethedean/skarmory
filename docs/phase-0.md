# Phase 0 prototype and evidence

The prototype exercises one native pipeline for a flat Python 3.11 application.
The CLI and Python SDK return the same versioned report. This is a development
interface ahead of the broader [0.1 preview](roadmap.md#01-python-dead-code-preview).

## Scan contract

- A directory selects regular direct entries ending in lowercase `.py`. Its
  entry module is `main.py`. A file argument selects that file alone and starts
  from its module body. Subdirectories and non-Python files are outside this
  source scope. The corpus assumes a closed application where each supported
  import resolves to its selected local file.
- Module filenames, bindings, and call targets use ASCII identifiers. Sources
  are UTF-8 up to 1 MiB per file, with an optional BOM and universal newlines.
  Encoding declarations are limited to `utf-8` and `utf_8`. Literal NUL bytes,
  symlinked scan targets or source entries, missing roots, and detected read
  races produce incomplete reports.
- Modules may have a leading docstring and `pass` statements. Absolute imports
  of local modules and their directly defined functions precede definitions and
  calls. Imports can use aliases. Import cycles, imports of the running entry
  module `__main__`, and duplicate bindings are unsupported.
- Functions are synchronous, top-level, zero-parameter definitions with no
  decorators, type parameters, annotations, or defaults. Bodies may contain a
  leading docstring, `pass`, and direct zero-argument call statements. A call
  names a function or a directly defined member of an imported module.
- Imports activate the target module body. Function declarations do not activate
  their bodies. The graph uses each module and definition as distinct nodes, so
  aliased or same-name functions resolve independently. Calls are conservatively
  treated as potential execution even when an earlier call might never return.
- The single rule, `unreachable_function`, reports selected functions with no
  call path from the entry under these assumptions. Findings identify the
  module, function, relative filename, and the identifier's one-based line and
  UTF-8 byte column. They carry the evidence tag `no_call_path_from_entry`.

Packages, arbitrary external or dynamic lookup, reassignment, shadowing,
nested scopes, branches, callbacks, classes, typing, library API roots,
framework activation, config files, suppressions, and non-dead-code rules are
outside the prototype. No project code is imported or executed.

`skarmory PATH` writes one JSON report to stdout. Exit 0 means the declared scope
completed, including scans with findings. Exit 2 signals incomplete coverage,
bad invocation, or output failure. The Python call `skarmory.analyze(path)`
returns the same report dictionary and raises `AnalysisUnavailableError` if the
extension cannot load. Check `coverage.status` before interpreting findings.

Reports identify the profile and entry, record selected and extracted file
counts, and list coverage issues separately from unresolved relationships and
findings. A coverage issue or unresolved relationship prevents a project-wide
non-use conclusion. Incomplete reports withhold findings. Extraction records
the first blocking diagnostic per file. Resolution stops at its first blocking
relationship or unsupported binding. The prototype runs serially without a
persistent cache.

## Parser and dependencies

The adapter pins Ruff parser, AST, and text range crates to `0.0.12` from
[Ruff revision `22f65a2`](https://github.com/astral-sh/ruff/tree/22f65a2ab5052990503985c7c794de37598d531e).
The Rust 1.85 scaffold built before adopting Ruff. The selected matching crate
set requires Rust 1.96, so the workspace minimum and CI are set to 1.96.
The Cargo lockfile records the resolved dependency graph. Parser AST and
diagnostic types stay in the adapter.

Ruff distinguishes malformed input from syntax that needs a later target
version. The adapter checks both before extracting facts. The invalid function
header and Python 3.12 type alias fixtures exercise the respective cases. A valid
assignment outside the subset is reported as an unsupported construct.

Skarmory uses the [MIT license](../LICENSE). The Ruff crates are MIT dependencies.
Skylos, under Apache-2.0, is used as an external reference, including its
source-built optional accelerator. The fixture sources and benchmark generator
were written for Skarmory. The repository vendors no upstream source or fixtures.

## Distributions

Version 0.0.1 ships the native CLI through `skarmory_cli` on crates.io and the
Python SDK through `skarmory` on PyPI. The [release notes](../CHANGELOG.md#001)
record the published wheel targets and the completed release checks.

Phase 0 qualified Rust 1.96 and a CPython 3.11–3.14 development matrix across
Linux, macOS, and Windows. CI installs both distributions into clean
destinations, verifies their reports against the reviewed cases, checks CLI
linkage and operation with no Python on PATH, and records startup samples,
sizes, and hashes. It also rebuilds and tests the SDK from its source
distribution.

The combined-wheel experiment stages the same native executable in the wheel's
scripts data directory and uses Maturin for the PyO3 extension. The install
check verifies that pip places the identical bytes in the environment's scripts
directory, then tests both entry points and linkage. The experiment retains the
separate default layout. See the [measurement instructions](../benchmarks/phase0.md)
and the CI artifacts for platform receipts.

## Reference comparison

The [versioned inventory](skylos-5.7.0-contract.md) identifies Skylos surfaces
and how the prototype relates to them. The [baseline manifest](../tests/conformance/baselines/skylos-5.7.0.toml)
pins the source, artifacts, environment, and configuration. Independent source
expectations live in the [fixture manifest](../tests/conformance/phase0.toml).
Raw timing and comparison results are linked in the [benchmark report](../benchmarks/phase0.md).
