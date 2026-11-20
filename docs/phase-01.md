# Phase 0.1 dead-code preview

Version 0.1.0 implements one rule, `unreachable_function`, for a closed set of
Python 3.11 sources. The native CLI and Python SDK use the same Rust analyzer.
A complete report applies to the selected sources, entry modes, and supported
syntax below. The analyzer reads Python source statically.

## Scanning a project

```sh
skarmory . --source-root src --entry-module example.main --workers 4
skarmory . --source-root src --entry-callable example.cli:main
skarmory . --source-root src --policy library --public-module example
skarmory . --source-root src --policy library --public-callable example:open_file
```

Application scans select one entry mode. Repeat `--entry-module` or
`--entry-callable` to declare independent application entries of that mode.
Use `--entry-file` for a direct script entry within a directory scan. A
directory with no entry declaration starts at its flat `main.py`. An explicit
file scan without source roots selects just that file.

Source roots are relative to the requested directory, or to the parent of an
explicit input file. Repeat `--source-root` to set import search order. A
direct-file entry searches its own directory first. Relative imports from a
script without a package context produce incomplete coverage. The
[roadmap's entry and import rules](roadmap.md#entry-and-import-resolution)
define the named-module and callable behaviors.

Import lookup uses only the selected sources and declared search order.
Interpreter import hooks, installed packages, and the standard library are
outside the modeled environment. Known interpreter-provided module names
produce an unresolved import even if the selection contains a same-name file.
Dynamic export hooks and package search-path overrides produce incomplete
coverage when they affect a required lookup.

Library scans declare public modules, public callables, or both. A public
module needs a resolvable `__all__` list or explicit callable declarations.
The analyzer reads export bindings after supported module initialization.
Functions returned to external callers and their potentially reached callees
are retained as possible use.

```python
import skarmory

report = skarmory.analyze(
    ".",
    source_roots=["src"],
    entry_modules=["example.main"],
    workers=4,
)
if report["coverage"]["status"] == "complete":
    for finding in report["findings"]:
        print(finding["path"], finding["line"], finding["symbol"])
```

The SDK accepts the corresponding `entry_file`, `entry_callables`,
`public_modules`, and `public_callables` keyword arguments. Options are
supplied per scan. A CLI invocation and SDK call with the same options produce
the same report. Use `--phase0` or `phase0=True` to select the
[historical flat-project contract](phase-0.md) and schema 1.

## Supported sources

- Recursive discovery selects regular lowercase `.py` files in ordinary
  packages and source roots. Discovery records excluded directory symlinks,
  `.git`, `.venv`, and `__pycache__` subtrees. An explicitly selected
  symlink target or symlinked Python source produces a coverage issue.
- The frontend accepts UTF-8 with an optional BOM and universal newlines.
  Paths and module resolution follow the declared roots and source names.
  Package and module import components in this preview are ASCII identifiers.
- Analysis models module and synchronous function scopes, nested definitions,
  positional and keyword arguments, supported defaults, returns, simple name
  assignments, and conditional branches. Expressions include scalar literals,
  tuples, lists, names, module attributes, boolean operators, comparisons, and
  direct calls. Module imports, definition-time effects, and function calls
  have separate activation semantics.
- Decorators, function annotations, async functions, loops, star imports,
  unmodeled import cycles, dynamic attributes, namespace packages, framework
  callbacks, and imports required by the graph outside the selected sources
  produce incomplete coverage. A known but possibly used function is retained
  with evidence. An unbounded dynamic target prevents a project-wide result.

Worker budgets range from 1 to 32 and apply to file-local processing.
Concurrent parallel scans share a process-wide background-worker budget.
The engine bounds discovery to 16,384 files and aggregate source snapshot
size to 32 MiB, with up to 1 MiB per file. It also bounds execution depth,
abstract operations, and simultaneous paths. A limit that prevents coverage
is reported in the result. Each selected file is parsed from a single read
snapshot. Detected changes to files or the selected file universe invalidate
the scan result.

## Reading the report

A schema 2 report includes the declared `roots`, `source_roots`, effective
`import_roots`, `selected_paths`, and `exclusions`. Paths use forward
slashes and are relative to the scan directory where possible. Findings
identify a source definition by module, owning scope, location, and
`symbol_id`. Locations are one-based with UTF-8 byte columns.

- `coverage` records selected and extracted file counts and reasons a scan
  could not finish.
- `uncertainties` describes possible or unresolved use.
- `evidence` records why definitions were retained, including possible calls,
  callable roots, and returned functions that may escape.
- `findings` contains unreachable definitions only when coverage completes.

The CLI writes JSON to stdout. Exit 0 means the declared scan completed,
including scans with findings. Exit 2 covers incomplete analysis, invocation
errors, and output failures. `--timings` writes a separate JSON timing record
to stderr. File read and extraction timings sum per-file work across
workers. The resolution timing includes the execution and call model, and
the reachability and rule timings cover collecting evidence and findings.

See the [conformance instructions](../tests/conformance/README.md) for
independent expectations and worker equivalence. The
[Skylos comparison](phase-01-reference.md) records the overlapping cases
reviewed against the pinned baseline. The [packaged benchmark](../benchmarks/phase01.md)
records timings and coverage for the release-mode CLI. Declared Skylos
compatibility is a Phase 0.2 deliverable.
