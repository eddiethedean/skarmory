# Phase 0.3 dead-code coverage expansion

**Planned.** Phase 0.3 starts from the [Phase 0.2 profile](phase-02.md) and ships
before [quality and secrets](roadmap.md#04-quality-and-secrets). Every dead-code
test at the pinned [Skylos 5.7.0 revision](skylos-5.7.0-contract.md) must pass
against Skarmory for Phase 0.3 to release. The test inventory determines the
minimum scope across languages, framework models, and workflows.

## Required upstream test gate

Track release conformance in issue [#41](https://github.com/eddiethedean/skarmory/issues/41).

The [release gate implementation](phase-03-gate.md) checks a candidate against
the [initial inventory](../tests/conformance/phase03-inventory.toml). The
inventory remains incomplete while the case audit and ports are in progress.
The gate blocks publication until the inventory and required evidence pass.

### Discover and review cases

Audit the entire pinned checkout. Classify at the case level by what the
assertion exercises. Include positive and negative dead-code assertions,
analysis facts that determine liveness, and workflow assertions that depend
on a dead-code result or its coverage. A fixture's language or a test's filename
is a discovery hint. For example, review individual cases in
`test/test_requirements_includes_and_unused_deps.py` for `SKY-U005` versus SCA,
and in `test/test_kotlin.py` for symbol and entry-point behavior versus
unrelated rules. For each mixed case, record which assertions are independently
separable. Preserve the setup, expected results, and positive and negative
controls that contribute to the dead-code contract. If a combined assertion
depends on another rule or workflow, keep that dependency in the mapped case.
Count tests embedded in fixture projects when they independently assert analysis
or workflow behavior. Otherwise, preserve them as inputs to the analysis case.

The initial source audit found these collection paths. The commands are
starting points for discovery and reference runs at the pinned checkout.
[Pinned Skylos source](https://github.com/duriantaco/skylos/tree/87978e69adf91b3811e97b00c40aa39d7937cd94)

| Source | Discovery and environment |
| --- | --- |
| Python tests in `test/` | Start with `python -m pytest --collect-only -q test/`. Review collection against tracked test sources, then collect in each environment needed by the cases. Include the Dart grammar extra, the source-built Go engine and host standard library, and the Python and `skylos_fast` reference paths where asserted. |
| Go engine native tests | From `skylos/engines/go`, start with `go list ./...` and `go test -list . ./...`. Use `go test -json` execution records to enumerate table-driven subtests, including those in `internal/symbols/reachability_test.go`. Review package scope across symbols, source selection, and output. |
| Corpus and deterministic benchmarks | Review `corpus/manifest.json` with `python scripts/corpus_ci.py --manifest corpus/manifest.json --json`. Use `python scripts/dead_code_benchmark.py --manifest PATH --json` for the default and adversarial dead-code manifests. Review deterministic assertions in the other benchmark runners separately. Record each case's original assertion mode and any external fixture revision it depends on. |
| Tests outside default collection | Search tracked paths, runner configurations, and CI for additional suites. Review `rust/` with `cargo test --locked --manifest-path rust/Cargo.toml -- --list` and report-contract assertions under `editors/vscode/test/` using `npm test --prefix editors/vscode`. Record the runner and applicable cases, or the reason for excluding a reviewed suite. |

Freeze a versioned manifest containing these fields before using pass counts
to track implementation:

- **Case ID** uses the pytest node ID with its parameter ID, the Go module,
  package, and full test or subtest name, or the benchmark manifest path and
  case ID. Record the pinned source path and fixture provenance.
- **Scope** records the assertion being preserved, its required workstream,
  and the reason for excluding any reviewed case outside the gate.
- **Required variants** describe each platform, toolchain, optional dependency,
  or backend that the original assertion requires, along with any fixture
  capabilities the runner must verify. Use the upstream preconditions to define
  this matrix before recording results.
- **Port mapping** identifies the Skarmory test or adapter and the fields that
  carry the asserted behavior. Use the engine for semantic facts and the native
  CLI or installed SDK for public interface assertions.

### Execute and account for every required variant

1. Record pinned Skylos results separately with upstream result markers intact.
   Run the original assertions against Skarmory without upstream expected-failure
   markers. Preserve the original fixture, controls, and expected results when
   porting. Thin adapters may translate entry points and fields losslessly.
   Ports must exercise the corresponding Skarmory behavior. A report-consumer
   test may use an upstream synthetic report as input to the real Skarmory
   consumer. When a case asserts analyzer or rescan behavior, run the real
   Skarmory analysis and inject fixture events at the relevant boundary. Record
   a divergent reference result under the
   [difference policy](compatibility.md#difference-policy).
   [Pinned expected-failure example](https://github.com/duriantaco/skylos/blob/87978e69adf91b3811e97b00c40aa39d7937cd94/test/test_package_local_exports.py#L60-L81)
2. Give pass credit when a required case and variant executes against Skarmory
   and all mapped assertions run and pass. Check fixture prerequisites
   explicitly and record setup failures or early returns as blocking outcomes.
   For example, verify symlink creation before running the legacy trace symlink
   control. A missing prerequisite, collection error, skip, deselection, xfail,
   xpass, or absent implementation blocks that case. An upstream-only run,
   mocked analysis result, or reviewed difference earns zero credit.
   [Pinned symlink control](https://github.com/duriantaco/skylos/blob/87978e69adf91b3811e97b00c40aa39d7937cd94/test/test_trace_file_analyzer.py#L90-L103)
3. Require the aggregated suite to pass before publication in the
   [release workflow](../.github/workflows/release.yml). Join expected and
   observed results by case ID and variant. Verify each result's release
   candidate, test inventory, and applicable artifact identity before giving
   credit. Fail on missing, unexpected, duplicate, mismatched, or nonpassing
   results and on runner errors.
   Publish the inventory, port mapping, exact commands, locked environments,
   revision and artifact identities, and per-case results. Exercise rejection
   of a missing shard, skipped case, missing backend, silent fixture setup
   failure, and result from another release candidate.

### Missing TypeScript fixtures

The pinned checkout omits both fixtures. A focused reference run skipped these
four cases in `test/test_typescript_expanded.py`.

| Missing path | Blocked case IDs within the test module |
| --- | --- |
| `manual/mixed_repo/hard_benchmark.ts` | `TestHardBenchmark::test_all_dead_detected`, `TestHardBenchmark::test_no_false_positives` |
| `manual/mixed_repo/realistic_benchmark.ts` | `TestRealisticBenchmark::test_all_dead_detected`, `TestRealisticBenchmark::test_no_false_positives` |

A search of public Skylos branch and tag history, the 5.7.0 release assets,
and the published 5.7.0 source distribution and wheels found neither fixture.
Keep all four cases blocked until the original sources are recovered and
their provenance, content hashes, and license are verified.
[Pinned TypeScript tests](https://github.com/duriantaco/skylos/blob/87978e69adf91b3811e97b00c40aa39d7937cd94/test/test_typescript_expanded.py#L2211-L2324) ·
[Fixture tracking issue #45](https://github.com/eddiethedean/skarmory/issues/45)

The source audit identifies where to start collection. The frozen case
inventory and Skarmory-backed execution results are Phase 0.3 deliverables.

## Scope and acceptance

| Workstream | Deliverable | Required controls | Issue |
| --- | --- | --- | --- |
| Python resolution | Extend bounded receiver and constructor dispatch, literal lazy exports, and statically declared plugin entry points. Resolve only targets supported by the selected sources and configuration. | Retain live definitions reached through aliases, constructors, exports, and registration. Report unused controls beside them. Retain finite possible targets with evidence. Mark coverage incomplete when custom import hooks, opaque exports, or dynamic loading leave an unbounded target set that affects non-use. | [#42](https://github.com/eddiethedean/skarmory/issues/42) |
| Python frameworks and test discovery | Extend pytest and FastAPI coverage from Phase 0.2. Model Django URL configurations and view registration, Flask routes and blueprints, and the additional framework contracts required by the inventory. The corpus already includes Django signals and management commands, Celery tasks, SQLAlchemy models and listeners, Pydantic lifecycle hooks, and unittest discovery. | Test active registrations, inactive modules and blueprints, and unrelated decorators with the same names. Surface unresolved registrations that could affect a non-use finding. | [#43](https://github.com/eddiethedean/skarmory/issues/43) |
| Unused Python dependencies | Implement `SKY-U005` for direct runtime declarations in `pyproject.toml`, requirements files, `setup.cfg`, and statically readable `setup.py`. Follow bounded requirements includes and recover declarations from supported source manifests referenced by compiled locks. Resolve usage through imports, framework and tool configuration, and statically identifiable command invocations. | Test nested includes, generated locks, and exact setuptools declaration lines. Cover import names that differ from distribution names, production, test and type-checking use, and configuration-only and command-only use. Keep build requirements, extras, constraints, and transitive pins outside the pinned runtime findings. An install command alone supplies no usage evidence. Abstain when unresolved inputs or dynamic uses prevent a reliable finding. | [#44](https://github.com/eddiethedean/skarmory/issues/44) |
| JavaScript and TypeScript | Build a native module graph for JS/TS imports, re-exports, and entry points required by the suite. Cover unused symbols, files (`SKY-E003`), unnecessary exports (`SKY-E004`), unused enum members (`SKY-U007`), and unused declared npm dependencies (`SKY-U005`). Account for component and asset references where asserted, including browser scripts and named handlers referenced by Vue templates. | Cover live public exports, unreachable file groups, re-export chains, type-only imports, and inline and project-level enum suppressions. Specify supported package and `tsconfig.json` resolution before implementation. Report missing entry points or unresolved imports that affect reachability. Verify npm imports and package-script use, unused controls, and incomplete coverage for file-scoped scans. | [#45](https://github.com/eddiethedean/skarmory/issues/45) |
| Other upstream-tested frontends | Implement the dead-code behavior required by the full inventory. The pinned sources include cases for Go, Rust, Swift, PHP, C#, Java, C++, Kotlin, Dart, Ruby, and Robot Framework. Record required build metadata, package boundaries, and cross-language references for each frontend. | Verify each frontend's live and unused controls, uncertain resolutions, and mixed-language cases. Include Java service and JavaFX/FXML entry points and C++ same-name symbol identity, call chains, and address-taken callbacks. Derive the final frontend and toolchain list from the upstream inventory. | [#46](https://github.com/eddiethedean/skarmory/issues/46) |
| Grep verification | Search selected sources and supported configuration with a bounded verifier. Record textual matches as possible-use evidence with their locations and resolved symbol owners. Apply the profile defaults below. | Cover lookalike names, comments, strings, and supported dynamic references. Verify that a retained root reached from configuration keeps its reachable callees, including with a warm verifier cache. An empty search or an exhausted budget cannot establish non-use. | [#47](https://github.com/eddiethedean/skarmory/issues/47) |
| Runtime evidence | Import trace and coverage artifacts. Verify revision, source fingerprints, and observed locations for native evidence, and handle legacy trace inputs under the compatibility policy below. Offer explicitly requested Python collection through a separate integration. | Cover fresh, stale, malformed, mismatched, and legacy inputs. Unobserved execution cannot establish non-use. Static CLI scans remain independent of Python and execute no target code. | [#48](https://github.com/eddiethedean/skarmory/issues/48) |
| Git diff, baselines, and gates | Build findings from the full selected head snapshot before changed-line filtering. Compare complete base and head analyses for impact mode, including changes to unchanged code. Add versioned baselines and a configurable dead-code gate. | Remove a caller while leaving its callee unchanged. Check stable identity across line shifts, already-dead symbols, incomplete base and head snapshots, and stale baselines. Test strict and non-strict verdicts independently of scan coverage. | [#49](https://github.com/eddiethedean/skarmory/issues/49) |
| Dead-code cleanup | Produce a review queue and read-only patch previews for eligible findings. Implement explicitly requested edit application and rollback where required by upstream tests. Define effects and applicability per transformation and attach a source fingerprint to each preview. | Cover a safe preview, an import or definition with observable effects, and source changes after the scan. Revalidate the fingerprint before applying a patch. Exercise the pinned TypeScript preview, apply, and rollback tests. | [#50](https://github.com/eddiethedean/skarmory/issues/50) |

The pinned Skylos release exposes Python and npm `SKY-U005` through quality
scanning. Preserve that behavior and the asserted coverage and locations in the
compatibility adapter. Python findings point to the original declaration when
the manifest is inside the report scope. Match the pinned project-directory
and line-zero fallback for a declaration outside that scope. Parse
`setup.py` and command evidence statically. Treat optional Python dependency
groups as a planned native opt-in with its own selection and evidence contract.
[Pinned Python declaration tests](https://github.com/duriantaco/skylos/blob/87978e69adf91b3811e97b00c40aa39d7937cd94/test/test_setuptools_declarations.py#L378-L421)
· [Pinned usage evidence test](https://github.com/duriantaco/skylos/blob/87978e69adf91b3811e97b00c40aa39d7937cd94/test/test_requirements_includes_and_unused_deps.py#L167-L223)
· [Pinned location fallback test](https://github.com/duriantaco/skylos/blob/87978e69adf91b3811e97b00c40aa39d7937cd94/test/test_vibe_rules.py#L786-L806)

Record how the mixed npm integration assertions for `SKY-Q408` and `SKY-Q409`
are handled under the [upstream test gate](#required-upstream-test-gate).
General quality rules, security analysis, and reusable developer integrations
have their own milestones.
[Pinned npm and enum integration tests](https://github.com/duriantaco/skylos/blob/87978e69adf91b3811e97b00c40aa39d7937cd94/test/test_project_gap_integration.py#L33-L63)

## Shared analysis and interface contracts

- Feed language-specific facts into the shared Rust analysis pipeline. Keep
  source discovery, declaration identity, reference resolution, and reachability
  explicit. When a selected frontend is unavailable or an unbounded reference
  can affect non-use, report incomplete coverage with the affected scope and
  cause. Retain finite possible targets with uncertainty and evidence.
- Store versioned file-local facts. Rebuild project resolution from the selected
  snapshot, effective configuration, and supplied evidence. Check frontend,
  grammar, and fact schema identity before reusing cached facts. Exercise an
  unchanged consumer after a provider, export, project manifest, or entry point
  changes.
- Keep native CLI and SDK options equivalent for shared analysis. Before adding a
  public option, document its default, input scope, output fields, and operational
  errors. Preserve Phase 0.2 interfaces or introduce a versioned schema or
  explicit opt-in where additional behavior requires a contract change. Keep
  stronger native semantics and reviewed compatibility differences visible.
- Preserve the source snapshot through analysis, filtering, and patch preview.
  Native gates require complete analysis of their declared scope. Filtering,
  suppressions, baselines, and runtime evidence must preserve coverage failures.
  Revalidate edit inputs and proof dependencies before applying or rolling back
  an explicitly requested change.

### Bounded Python ordering guards

The Skylos profiles analyze both branches of a single `<`, `<=`, `>`, or `>=`
guard when its operands have bounded scalar behavior. Comparisons that could
invoke a selected class hook report incomplete coverage when reached. This
supports the pinned recursive-group and caller-edit cases. [#52](https://github.com/eddiethedean/skarmory/issues/52)

### Bounded Python receivers and lazy imports

Ordinary constructors and instance methods use the first positional
parameter as the receiver regardless of its name. An explicit
keyword receiver is resolved when the supported signature proves that the name
accepts a keyword. Unknown receiver types and custom allocation can still
make analysis incomplete.

A literal equality branch in a module's `__getattr__` can return a selected
member through `getattr(importlib.import_module("pkg.provider"), name)`.
The analyzer resolves the loader and member through their bindings and tracks
the selected import's initialization. The bounded model requires selected
package initializers and abstains when attaching a child could replace a
package member. Shadowed loaders and other unmodeled call effects also
leave coverage incomplete.

### Static package entry points

The `skylos-0.3` profile reads package entry-point declarations from the scan
root. It supports PEP 621 script and group tables, Poetry scripts and plugin
groups, `[options.entry_points]` in `setup.cfg`, and literal entry points
passed to a statically identified setuptools or distutils `setup()` call.
Source modules and dotted members resolve against the selected Python files.
A class target retains its public surface. A dotted class member retains the
named member and the class needed to look it up.

Metadata reads use the current snapshot even when Python facts come from the
cache. Invalid or unresolved declarations produce coverage diagnostics.
`setup.py` metadata is interpreted statically. When the script is itself
selected for dead-code analysis, its Python statements follow the ordinary
coverage rules.

### Bounded Python function namespace reads

In both Skylos profiles, zero-argument calls to the built-in `locals()` and
`vars()` within a function retain the bindings that can be present at the
call site, including captured cells. A namespace that escapes can retain
possible callable targets with evidence, including coroutines and values
supplied by selected callers and executor submissions. Dictionary copies keep
the entries present after argument evaluation and apply explicit keyword
overrides. Unresolved values and escaping results of unsupported namespace
expressions, such as a dictionary union, report incomplete coverage. Length
checks and provable truth tests can read the namespace without exposing its
values. Shadowed names use ordinary call resolution.
Object-argument `vars` and module and class namespace reads remain outside this
model. [#42](https://github.com/eddiethedean/skarmory/issues/42)
Reachable calls resolved to `builtins.eval` or `builtins.exec`, including
imported aliases, report incomplete coverage and withhold unused findings.
The analyzer does not evaluate their arguments as Python code. [#78](https://github.com/eddiethedean/skarmory/issues/78)

### Grep verification preview

The native CLI enables grep verification with `--grep-verify`. In the native
SDK, pass `grep_verify=True` to `skarmory.analyze`. The Phase 0.2 `skylos`
profile keeps verification disabled by default. Select `--profile skylos-0.3`
in the CLI or `profile="skylos-0.3"` in
`skarmory.compat.skylos.analyze` to enable verification when omitted. Disable
it with `--no-grep-verify` in the CLI or `grep_verify=False` in the SDK. The
Phase 0.3 selector is a preview for this workstream.

The verifier considers selected Python sources and UTF-8 `.yaml` and `.yml`
files in the scanned directories. It reads plain or quoted single-line block
mapping values and sequence items. A project-relative path such as
`workflow.py` can retain module-level functions. A qualified target such as
`workflow:queue_task` or `workflow.py:queue_task` names a specific function.
In Python, a literal member in a built-in `getattr` call can supply evidence
when the receiver and member resolve to a selected definition. Comments and
ordinary Python strings supply no evidence. This bounded subset covers the
pinned YAML rescue case. Skylos also searches other configuration formats and
uses additional text patterns. [Pinned configuration rescue test](https://github.com/duriantaco/skylos/blob/87978e69adf91b3811e97b00c40aa39d7937cd94/test/test_reachability_integration_qa.py#L259-L286)

Matches in `grep_verification_evidence` carry a symbol identity, definition
location, original reference location, and `possible` certainty. Reachability
propagates from the retained definition in the same scan. The Phase 0.3
compatibility report groups evidence by symbol and counts direct rescues under
`analysis_summary.grep_verify`. An empty search contributes no evidence of
non-use. The verifier bounds directory entries, configuration files and bytes,
and the number of references and evidence records. A verification limit
produces a coverage issue and an incomplete scan.

The Phase 0.3 SDK accepts the pinned `grep_cache` flag. Skarmory uses the
explicit `cache_dir` option for persistent file-local facts, and rebuilds
verification and reachability from the current scan inputs even with a warm
cache. The pinned SDK flag creates no verification cache files.

### Runtime evidence

Select the Phase 0.3 profile to import runtime observations. By default it
looks for project-root `.skylos_trace` and `.coverage` files. In the CLI,
`--trace-file PATH` and `--coverage-file PATH` select explicit artifacts.
`--no-trace` and `--no-coverage` disable the respective inputs. The Python SDK
accepts `trace_file` and `coverage_file` with the same path behavior, or
`False` to disable either input. Relative paths start at the scan root
(or the containing directory for a file scan).

The reader accepts these formats:

- Legacy Skylos v1 JSON calls with `file`, `function`, and `line`. A call
  must match the selected file, simple function name, and definition or first
  decorator line. A positive `count` is optional.
- A standalone coverage.py SQLite database with `file` and `line_bits`
  tables. The analyzer retains a callable when a reported line matches a
  statement or expression location in its body. Coverage source paths must
  be absolute. The database does not record the base for relative paths, so
  those observations are reported as rejected. Set `relative_files = False`
  in coverage.py's `[run]` configuration to store absolute paths. Bare legacy
  bitmaps without a `context_id` column can also retain a declaration to match
  the pinned compatibility behavior. These unverified hints do not establish
  body execution or retain callees through that body.
- Version 2 JSON with a `revision`, a `fingerprints` map, and `calls` and/or
  `lines` arrays. Line observations contain `file` and a one-based `line`.
  Fingerprints are lowercase BLAKE3 hex digests of the selected file's UTF-8
  content after CRLF and bare CR normalization to LF. Keys are selected
  project-relative paths with forward slashes.

The version 2 revision is the lowercase BLAKE3 hex digest of the source
snapshot. Start the hash with the bytes `skarmory-source-snapshot-v1\0`
(including the terminating NUL). In lexicographic path order, append each
selected file's path length as an unsigned 64-bit little-endian integer,
the UTF-8 path bytes, and the file's 32 raw BLAKE3 fingerprint bytes.
Use the selected files for the same effective scan scope. Both the revision
and the observed file's fingerprint must match to earn verified provenance.

The `runtime_evidence` report field identifies the matched callable,
observed location, artifact, and provenance. It reports declaration-only
matches as `runtime_coverage_declaration`. The grouped
`dead_code_evidence` uses the compatibility event kinds `trace_hit` and
`coverage_hit`. Legacy artifacts are unverified
and contribute possible use. An observed nested callable supplies possible-use
evidence for the scopes that created it. Evidence binds to selected files and
unambiguous locations. The reader rejects symlink artifacts, caps inputs at
1 MiB and 32,768 observations per artifact, and rejects WAL-mode SQLite
databases. A source newer than a legacy artifact cannot supply a match.
Rejections appear in `runtime_evidence_issues`, independently of static scan
coverage. Absence from any artifact supplies no evidence of non-use. Each
scan reads current artifacts even when it reuses cached Python facts.

To collect an unverified call trace explicitly, use
`skarmory.runtime.collect_trace(project)` as a Python context manager around
the workload you choose to run. It records calls on the current thread and
writes `.skylos_trace` when the context exits successfully. Static scans read
artifacts through Rust and run independently of that collection step.

[Pinned trace inputs](https://github.com/duriantaco/skylos/blob/87978e69adf91b3811e97b00c40aa39d7937cd94/test/test_trace_file_analyzer.py) · [Pinned coverage database cases](https://github.com/duriantaco/skylos/blob/87978e69adf91b3811e97b00c40aa39d7937cd94/test/test_implicit_refs.py#L181-L232)

### Other planned compatibility policies

Explicitly select the Phase 0.3 Skylos contract in the CLI and SDK to apply the
policies below. Keep existing Phase 0.2 invocations and defaults valid. The
conformance harness selects the contract at the entry-point boundary and
preserves each upstream case's inputs and omitted options.

- **Coverage and verdict.** In impact mode, evaluate base and head under the
  same effective comparison policy, including the pinned use of base policy
  and explicit operator configuration. Return an incomplete comparison when
  either analysis is incomplete and omit introduced status. Under the
  compatibility gate, a non-strict verdict may pass when the pinned policy
  allows a partial scan. Report partial coverage separately, with its warnings
  and unchecked scope. Required completeness and strict mode block on missing
  coverage. Preserve native incomplete-analysis reporting and exit behavior.
  [Pinned impact tests](https://github.com/duriantaco/skylos/blob/87978e69adf91b3811e97b00c40aa39d7937cd94/test/test_pr_impact.py#L50-L98) ·
  [Pinned coverage tests](https://github.com/duriantaco/skylos/blob/87978e69adf91b3811e97b00c40aa39d7937cd94/test/test_coverage_gaps.py#L277-L325)
- **Dead-code reporting.** Track rule selection, severity and filtering policy,
  and JSON-CI preservation of findings, coverage, and per-finding evidence with
  the diff and gate workstream. Port tests at the layer that computes each
  policy decision or report field. [#49](https://github.com/eddiethedean/skarmory/issues/49) ·
  [Pinned JSON-CI tests](https://github.com/duriantaco/skylos/blob/87978e69adf91b3811e97b00c40aa39d7937cd94/test/test_cli_json_ci_format.py#L86-L113)

## Rust crate candidates

These versions record the research baseline. Select and lock dependencies after
the pinned test inventory establishes which capabilities each frontend needs.

| Workstream | Candidate | What to verify before adoption |
| --- | --- | --- |
| JavaScript and TypeScript | Use `oxc_parser`, `oxc_semantic`, `oxc_allocator`, and `oxc_span` at `0.159.0` for parsing and binding. Use `oxc_resolver` `11.24.3` for Node resolution and `tsconfig` paths. | Check package exports, re-exports, type-only imports, and enum member identity against pinned fixtures. Keep package-script evidence, project-wide reachability, and coverage reporting in Skarmory. [Parser](https://oxc.rs/docs/guide/usage/parser.html), [semantic analysis](https://docs.rs/oxc_semantic/0.159.0/oxc_semantic/), [resolver](https://docs.rs/oxc_resolver/11.24.3/oxc_resolver/) |
| Other frontends | Start with `tree-sitter` `0.27.2` and individually pinned, compiled-in grammars. The smoke test covered `tree-sitter-go` `0.25.0`, `tree-sitter-c-sharp` `0.24.0`, and `tree-sitter-dart-orchard` `0.7.1`. Check grammar candidates for Rust, Swift, PHP, Java, C++, Ruby, Kotlin, and Robot Framework where the inventory requires them. | Verify grammar compatibility, syntax error handling, and source spans on pinned fixtures. Qualify grammar and native scanner licensing and packaging. Establish a build and distribution path for the Robot grammar. Skarmory owns language-specific symbol extraction and reachability. [Tree-sitter](https://docs.rs/tree-sitter/0.27.2/tree_sitter/), [Dart grammar](https://docs.rs/crate/tree-sitter-dart-orchard/0.7.1), [Robot grammar candidate](https://github.com/Hubro/tree-sitter-robot) |
| Go and Rust semantics | Start with syntax-derived facts. Compare a narrowly scoped `go/types` fact adapter and an isolated `ra_ap_hir` `0.0.363` pilot if the pinned tests require richer facts. | Run pinned Go tests that exercise type identity and generics, and Rust fixtures that require semantic resolution. Measure what each approach adds to build time and binary size. Qualify how any adapter would ship and keep static scans free of target-code execution. [Pinned Go tests](https://github.com/duriantaco/skylos/blob/87978e69adf91b3811e97b00c40aa39d7937cd94/skylos/engines/go/internal/symbols/reachability_test.go), [`go/types`](https://pkg.go.dev/go/types), [`ra_ap_hir`](https://docs.rs/ra_ap_hir/0.0.363/ra_ap_hir/) |
| Python dependencies | Keep the Ruff AST and existing `toml` dependency. Use `uv-pep508` `0.0.101` for requirement specifiers, extras, and markers. Evaluate `uv-requirements-txt` `0.0.101` if pinned fixtures warrant a fuller requirements parser. | Keep static setuptools and command interpretation, bounded requirements traversal, declaration provenance, and evidence-to-distribution mapping in Skarmory. Compare the fuller parser's dependency footprint and offline behavior. [`uv-pep508`](https://docs.rs/uv-pep508/0.0.101/uv_pep508/), [`uv-requirements-txt`](https://docs.rs/crate/uv-requirements-txt/0.0.101) |
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

| Step | Exit evidence |
| --- | --- |
| 1. Inventory and runner | Reviewed case and variant manifest, pinned reference results, and an initial Skarmory ledger that shows gaps. Prove a port at each relevant layer before scaling the harness. Freeze independent holdouts before using them to evaluate the implementations. |
| 2. Python and frontend foundations | Port the Python resolution and framework cases, then the dependency checks. Qualify Go and Rust semantic approaches early and build the JS/TS and other required frontends as separate slices. For each frontend, demonstrate an entry point, a retained symbol or file, an unused counterpart, an incomplete control, and equivalence after an edit to a dependency. |
| 3. Verification and reporting | Pass grep and runtime evidence cases with distinct provenance and retention decisions. Verify changed-line filtering, snapshot comparisons, baselines, and gates once finding identity is stable across the edits the contracts support. Show that deleting the final caller exposes the unchanged callee in impact mode only after both scans complete. |
| 4. Cleanup | Pass read-only previews and the pinned apply and rollback cases after finding identity, edit eligibility, and source fingerprints are verified. Test a changed source or proof dependency and a failed explicit validation check. |
| 5. Release | Publish the reconciled upstream ledger, independent controls, reviewed differences, feature matrix, and install evidence for the native CLI, SDK wheels, and SDK rebuilt from the source distribution. |

## Release gates

- Pass every case in the [required upstream test gate](#required-upstream-test-gate).
- Run independent live, unused, and incomplete controls for every advertised
  feature. Preserve required detections and must-retain cases from the
  [Phase 0.2 comparator](../tests/conformance/check_phase02.py).
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
  languages and after filtering. Check freshness for verified runtime evidence
  and explicit provenance for legacy retention in the compatibility profile.
  Verify that cleanup previews leave the analyzed sources intact.
- Record timing and memory for matched, completed workloads using the
  [benchmark protocol](../benchmarks/README.md). Capture optional verification
  and workflow costs separately. Publish evidence before making performance
  or compatibility claims.
