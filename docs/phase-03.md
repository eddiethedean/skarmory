# Phase 0.3 dead-code coverage expansion

**Planned.** Phase 0.3 starts from the [Phase 0.2 profile](phase-02.md) and ships
before [quality and secrets](roadmap.md#04-quality-and-secrets). Every dead-code
test at the pinned [Skylos 5.7.0 revision](skylos-5.7.0-contract.md) must pass
against Skarmory for Phase 0.3 to release. The test inventory determines the
minimum scope across languages, framework models, and workflows.

## Required upstream test gate

1. Audit the entire pinned checkout for dead-code assertions. Record each
   relevant test runner, suite root, and discovery command. Include pytest's
   `test/` directory and native suites such as the Go engine's reachability
   tests in `skylos/engines/go/internal/symbols/`. Freeze a reviewed inventory
   with stable case IDs for each parametrization or native subtest. Cover
   mixed modules, benchmark correctness cases, and optional frontends and
   backends. Document the basis for classifying mixed-module cases.
2. Run the inventoried tests against Skarmory's engine, native CLI, or installed
   SDK. Use thin adapters for entry points and lossless report-field mappings.
   Port tests of upstream internals to the corresponding Skarmory layer while
   preserving the fixture, semantic assertions, expected results, and controls.
   Run pinned Skylos separately to record the reference result.
3. Run platform- and backend-dependent cases where their original preconditions
   are met. Every inventoried case and required variant must execute and pass.
   A missing prerequisite, collection error, skipped or deselected case,
   expected failure, or missing Skarmory implementation blocks the gate.
   Preserve upstream assertions and expected results. Upstream-only runs,
   mocked analysis results, and documented differences receive zero pass credit.
4. Make the complete-suite check a required release job. Publish the pinned
   revision, inventory and port mapping, exact commands, locked dependencies,
   applicable environments and backends, and per-case results. Verify that
   every inventoried runner contributed results and that its expected and
   passing case sets match, with no unresolved cases.

## Scope and acceptance

| Workstream | Deliverable | Required controls |
| --- | --- | --- |
| Python resolution | Extend bounded receiver and constructor dispatch, literal lazy exports, and statically declared plugin entry points. Resolve only targets supported by the selected sources and configuration. | Retain live definitions reached through aliases, constructors, exports, and registration. Report unused controls beside them. Preserve possible-use evidence or mark coverage incomplete when custom import hooks, opaque exports, or dynamic loading leave the target set unbounded. |
| Django and Flask | Recognize statically resolvable Django URL configurations and view registration, along with Flask routes and registered blueprints. Preserve the Phase 0.2 pytest and FastAPI behavior. | Test active registrations, inactive modules and blueprints, and unrelated decorators with the same names. Surface unresolved registrations that could affect a non-use finding. |
| Unused Python dependencies | Add opt-in `SKY-U005` analysis for project dependencies and optional groups in `pyproject.toml` and direct declarations in requirements files. Follow nested requirements includes within the project root, track constraints and transitive lock pins separately, and map generated lock entries back to their source manifests where supported. Use explicit or evidence-backed import-to-distribution mappings. | Test an unused dependency in a nested include, a direct declaration behind a generated lock, and transitive pins and constraints that stay out of the findings. Report the original declaration path and line. Cover distribution names that differ from import names and production, optional, test, and type-checking use. Abstain when ambiguous mappings, unresolved inputs, or dynamic imports prevent a reliable finding. |
| JavaScript and TypeScript | Build a native module graph for JS/TS imports, re-exports, and entry points required by the suite. Add unused-file findings (`SKY-E003`) and unnecessary-export findings (`SKY-E004`). | Cover live public exports, unreachable file groups, re-export chains, type-only imports, and ambiguous resolution. Specify supported package and `tsconfig.json` resolution before implementation. A missing entry point or unresolved import that affects reachability must be visible in coverage. |
| Other upstream-tested frontends | Implement the dead-code behavior required by the full inventory, including cases for Go, Rust, PHP, C#, Kotlin, Dart, Ruby, and Robot Framework. Route formats and cross-language references through the shared coverage contract. | Verify each frontend's live and unused controls, uncertain resolutions, and mixed-language cases. Derive the final frontend and toolchain list from the upstream inventory. |
| Grep verification | Add an opt-in, bounded identifier search over selected sources. Record textual matches as possible-use evidence with their locations. | Review cases for comments, strings, same-named symbols, and real dynamic references. An empty search or an exhausted search budget cannot establish non-use. |
| Runtime evidence | Import trace and coverage artifacts with project revision, source fingerprints, and observed locations. Offer explicitly requested Python trace collection through a separate integration. | Retain definitions with valid observed-use evidence. Reject stale, mismatched, or malformed evidence. Unobserved execution cannot establish non-use. Static CLI scans remain independent of Python and execute no target code. |
| Git diff, baselines, and gates | Build dead-code findings from the full selected project, then filter by changed lines or report impact on unchanged code as a separate mode. Add versioned baselines and a configurable dead-code gate. | Test removing a caller in a changed file while leaving the now-unused callee unchanged. Exercise stable finding identity, stale baselines, existing and new findings, and strict distinction between a failed gate and incomplete analysis. |
| Dead-code cleanup | Produce a review queue and read-only patch previews for eligible findings. Implement explicitly requested edit application and rollback where required by upstream tests. Define effects and applicability per transformation and attach a source fingerprint to each preview. | Cover a safe preview, an import or definition with observable effects, and source changes after the scan. Revalidate the fingerprint before applying a patch. Exercise the pinned TypeScript preview, apply, and rollback tests. |

The pinned Skylos release exposes `SKY-U005` through quality scanning. Preserve
that behavior in the compatibility adapter and document any separate native
opt-in. General quality rules, security analysis, and reusable developer
integrations have their own milestones.

## Rust crate candidates

These versions record the research baseline. Select and lock dependencies after
the pinned test inventory establishes which capabilities each frontend needs.

| Workstream | Candidate | What to verify before adoption |
| --- | --- | --- |
| JavaScript and TypeScript | Use `oxc_parser`, `oxc_semantic`, `oxc_allocator`, and `oxc_span` at `0.159.0` for parsing and binding. Use `oxc_resolver` `11.24.3` for Node resolution and `tsconfig` paths. | Check package exports, re-exports, and type-only imports against pinned fixtures. Build project-wide reachability and coverage reporting in Skarmory. [Parser](https://oxc.rs/docs/guide/usage/parser.html), [semantic analysis](https://docs.rs/oxc_semantic/0.159.0/oxc_semantic/), [resolver](https://docs.rs/oxc_resolver/11.24.3/oxc_resolver/) |
| Other frontends | Start with `tree-sitter` `0.27.2` and individually pinned, compiled-in grammars. The smoke test covered `tree-sitter-go` `0.25.0`, `tree-sitter-c-sharp` `0.24.0`, and `tree-sitter-dart-orchard` `0.7.1`. Check grammar candidates for Rust, Swift, PHP, Ruby, Kotlin, and Robot Framework where the inventory requires them. | Verify grammar compatibility, syntax error handling, and source spans on pinned fixtures. Qualify grammar and native scanner licensing and packaging. Establish a build and distribution path for the Robot grammar. Skarmory owns language-specific symbol extraction and reachability. [Tree-sitter](https://docs.rs/tree-sitter/0.27.2/tree_sitter/), [Dart grammar](https://docs.rs/crate/tree-sitter-dart-orchard/0.7.1), [Robot grammar candidate](https://github.com/Hubro/tree-sitter-robot) |
| Go and Rust semantics | Start with syntax-derived facts. Compare a narrowly scoped `go/types` fact adapter and an isolated `ra_ap_hir` `0.0.363` pilot if the pinned tests require richer facts. | Run pinned Go tests that exercise type identity and generics, and Rust fixtures that require semantic resolution. Measure what each approach adds to build time and binary size. Qualify how any adapter would ship and keep static scans free of target-code execution. [Pinned Go tests](https://github.com/duriantaco/skylos/blob/87978e69adf91b3811e97b00c40aa39d7937cd94/skylos/engines/go/internal/symbols/reachability_test.go), [`go/types`](https://pkg.go.dev/go/types), [`ra_ap_hir`](https://docs.rs/ra_ap_hir/0.0.363/ra_ap_hir/) |
| Python dependencies | Keep the Ruff AST and existing `toml` dependency. Use `uv-pep508` `0.0.101` for requirement specifiers, extras, and markers. Evaluate `uv-requirements-txt` `0.0.101` if pinned fixtures warrant a fuller requirements parser. | Keep bounded include traversal, declaration provenance, constraints, transitive pins, and import-to-distribution mapping in Skarmory. Compare the fuller parser's dependency footprint and offline behavior. [`uv-pep508`](https://docs.rs/uv-pep508/0.0.101/uv_pep508/), [`uv-requirements-txt`](https://docs.rs/crate/uv-requirements-txt/0.0.101) |
| Grep verification | Evaluate `aho-corasick` `1.1.5`, already in the lockfile as a transitive dependency, for bounded multi-literal matching. | Verify identifier boundaries and original source offsets. Apply symbol ownership, search budgets, and possible-use classification in Skarmory. [`aho-corasick`](https://docs.rs/aho-corasick/1.1.5/aho_corasick/) |
| Git diff and gates | Evaluate `gix` `0.90.0` with default features disabled and `sha1`, `status`, `blob-diff`, and `revision` enabled. | Check merge bases, staged and unstaged changes, rename behavior, and changed-line filtering against fixtures. The smoke test only checked repository opening and `HEAD` lookup. [`gix` features](https://docs.rs/crate/gix/0.90.0/features) |
| Cleanup | Use `similar` `3.2.1` for unified diff previews. Reuse `blake3` for source fingerprints and `tempfile` for staging. | Keep edit eligibility, source revalidation, and apply and rollback behavior in Skarmory. Cover read-only previews and stale patches with the pinned fixtures. [`similar`](https://docs.rs/similar/3.2.1/similar/) |
| Runtime evidence | Start with existing `serde_json` for artifact ingestion and the Python integration for explicit collection and export. Evaluate `rusqlite` if the pinned tests require direct native reads of `.coverage` files. | Check artifact provenance and stale evidence. If SQLite is required, qualify the supported coverage database schema versions and distribution impact. [Coverage database schema](https://coverage.readthedocs.io/en/latest/dbschema.html), [`rusqlite`](https://github.com/rusqlite/rusqlite) |

### Smoke-test results and adoption checks

An isolated Cargo project passed five tests with Rust `1.98.1` on
`aarch64-apple-darwin` using an offline build and a locked dependency graph. It
parsed and bound TypeScript, resolved a `tsconfig` path alias, loaded the Go,
C#, and Dart grammars on one runtime, checked Git `HEAD` and parsed a PEP 508
requirement, and verified literal match offsets and unified diff output. The
initial Git build failed with `status` and `blob-diff` alone. The passing build
enabled `sha1` and `revision` as well.

Before adoption, run the relevant pinned fixtures, verify the minimum Rust
version and builds across supported targets, and measure the native CLI and
Python package size impact. Review licenses, including those of native grammar
sources, and update third-party notices when adding locked dependencies. Verify
installed native CLI and SDK entry points, and keep static scans independent of
the Python interpreter and grammar downloads. The smoke tests provide local
integration evidence. Phase 0.3 compatibility requires the complete upstream gate.

## Implementation order

1. Freeze the complete upstream test inventory, map each case to its required
   component and environment, and record the initial Skarmory-backed results.
   Freeze independent holdouts for representative supported workflows.
2. Extend Python resolution and framework models, then add dependency analysis.
   Run the Go and Rust semantic pilots early in frontend development. Build the
   required frontends and module graphs alongside the Python work. Reuse the
   reporting, coverage, and cache contracts at language boundaries.
3. Add grep and runtime evidence with distinct evidence kinds and provenance.
   Verify that each opt-in can change retention only within its declared scope.
4. Add diff reporting, baselines, and dead-code gates over the full project
   result. Add cleanup previews and test-required edit application after finding
   identity and source fingerprints have passed the edited-source controls.
5. Verify package installs, run the release gates, and publish the supported
   feature matrix and reviewed differences.

## Release gates

- Pass every case in the [required upstream test gate](#required-upstream-test-gate).
- Run independent live, unused, and incomplete
  controls for every advertised feature. Preserve required detections and
  retain cases from the [Phase 0.2 comparator](../tests/conformance/check_phase02.py).
  Record differences under the [compatibility policy](compatibility.md#difference-policy).
- Run the native CLI and installed SDK with equivalent supported options. Keep
  existing Phase 0, 0.1, and 0.2 invocations and report contracts valid. Version
  any incompatible schema or policy and verify that the CLI still runs without
  a Python interpreter for static scans.
- Compare single- and multi-worker scans with caching disabled and with cold,
  warm, and edited caches. Check findings, locations, evidence, uncertainty,
  and coverage. Edits must include source changes, graph root changes, and
  changes to configuration or supplied runtime evidence.
- Check that incomplete analysis remains visible in reports and gates across
  languages and after filtering. Require fresh evidence for retention and verify
  that cleanup previews leave the analyzed sources intact.
- Record timing and memory for matched, completed workloads using the
  [benchmark protocol](../benchmarks/README.md). Capture optional verification
  and workflow costs separately. Publish evidence before making performance
  or compatibility claims.
