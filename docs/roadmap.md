# Skarmory roadmap

[Phase 0](phase-0.md) shipped in [0.0.1](../CHANGELOG.md#001).
The [Phase 0.1 preview](phase-01.md) is included in [0.1.0](../CHANGELOG.md#010).
[Phase 0.2 Python dead-code compatibility](phase-02.md) is included in
[0.2.0](../CHANGELOG.md#020).
Subsequent version labels describe planned milestones.

The default distribution uses a PyO3 SDK wheel and a native Cargo CLI. The
combined wheel is an experimental installed layout.

## Product goal

Build a Python-first code analyzer with a shared native Rust engine, a native CLI,
and a thin Python API. Expand into a multi-language analyzer through dedicated
Rust frontend crates and independently qualified language milestones. Preserve the useful analysis behavior and framework awareness
of Skylos while making frequent local scans responsive.

The first reference implementation is
[Skylos 5.7.0](https://github.com/duriantaco/skylos/releases/tag/v5.7.0), released
November 19, 2026, at source revision
[`87978e69adf91b3811e97b00c40aa39d7937cd94`](https://github.com/duriantaco/skylos/tree/87978e69adf91b3811e97b00c40aa39d7937cd94).
Compatibility is versioned and limited to explicitly declared features. The
[compatibility plan](compatibility.md) defines that boundary.

## Delivery principles

- Correctness and analysis completeness are release gates. Faster incomplete
  analysis is not an equivalent scan.
- Separate conformance to Skylos from independently reviewed correctness.
- Make performance measurable from the first vertical slice. All speed figures
  remain targets until supported by reproducible matched runs.
- Share the native engine between the CLI and SDK. Keep Python integration out of
  per-node analysis work.
- Add file-summary caching before complex incremental graph maintenance; compare
  every cached result with a clean rebuild in correctness tests.
- Expose unsupported syntax, missing sources, and exhausted analysis budgets.
  An empty finding list does not mean that analysis completed.
- Ship reporting before source modification. Edit eligibility requires separate
  reasoning about effects, current source identity, and the proposed transformation.

## Milestone overview

| Milestone | Main deliverable | Depends on | Release gate |
| --- | --- | --- | --- |
| Phase 0 | Compatibility contract, benchmark baseline, native vertical slice | Initial scaffold | Reproducible baseline and measured native prototype |
| 0.1 | Native unreachable-function preview for packaged Python 3.11 projects | Phase 0 | Reviewed reachability, coverage, and worker-equivalence checks pass |
| 0.2 | [Declared Python dead-code compatibility and persistent module facts](phase-02.md) | 0.1 | Reviewed reference differences, independent controls, and cached/clean equivalence |
| 0.3 | [Expanded Python dead-code coverage and workflows](phase-03.md) | 0.2 | Every in-scope pinned Python dead-code case and variant passes against Skarmory |
| 0.4 | Quality and secrets | 0.3 | Rule-level conformance and independent controls |
| 0.5 | Security and data flow | 0.4 and semantic infrastructure | Accuracy, completeness, and resource budgets verified |
| 0.6 | Developer workflows | Phase 0.3 workflow contracts and relevant stable rule families | Integration and narrow edit contracts verified |
| 0.7 | [JavaScript / TypeScript frontend](language-frontends.md#phase-07) (`skarmory_javascript`) | 0.6 | Scoped language conformance and earlier-phase regressions |
| 0.8 | [Rust frontend](language-frontends.md#phase-08) (`skarmory_rust`) | 0.7 | Scoped language conformance and earlier-phase regressions |
| 0.9 | [Go frontend](language-frontends.md#phase-09) (`skarmory_go`) | 0.8 | Scoped language conformance and earlier-phase regressions |
| 0.10 | [Java frontend](language-frontends.md#phase-010) (`skarmory_java`) | 0.9 | Scoped language conformance and earlier-phase regressions |
| 0.11 | [C++ frontend](language-frontends.md#phase-011) (`skarmory_cpp`) | 0.10 | Scoped language conformance and earlier-phase regressions |
| 0.12 | [C# frontend](language-frontends.md#phase-012) (`skarmory_csharp`) | 0.11 | Scoped language conformance and earlier-phase regressions |
| 0.13 | [Swift frontend](language-frontends.md#phase-013) (`skarmory_swift`) | 0.12 | Scoped language conformance and earlier-phase regressions |
| 0.14 | [Kotlin frontend](language-frontends.md#phase-014) (`skarmory_kotlin`) | 0.13 | Scoped language conformance and earlier-phase regressions |
| 0.15 | [Dart frontend](language-frontends.md#phase-015) (`skarmory_dart`) | 0.14 | Scoped language conformance and earlier-phase regressions |
| 0.16 | [PHP frontend](language-frontends.md#phase-016) (`skarmory_php`) | 0.15 | Scoped language conformance and earlier-phase regressions |
| 0.17 | [Ruby frontend](language-frontends.md#phase-017) (`skarmory_ruby`) | 0.16 | Scoped language conformance and earlier-phase regressions |
| 0.18 | [Robot Framework frontend](language-frontends.md#phase-018) (`skarmory_robot`) | 0.17 | Scoped language conformance and earlier-phase regressions |
| 1.0 | Stable declared analysis contracts | Earlier gates | Published feature matrix, APIs, wheels, and evidence |

## Planned Rust dependencies

The workspace uses the pinned Ruff parser and AST crates behind the Python
frontend, Serde for reports, and PyO3 at the SDK boundary. Adopt the candidates
below as their features land. Verify each addition on Rust 1.98 and the
supported distribution targets. Follow the locked dependency and notice
workflow in [CONTRIBUTING.md](../CONTRIBUTING.md#dependencies-and-packaging).

| Milestone | Candidates | Adoption criteria |
| --- | --- | --- |
| 0.1 | [walkdir](https://docs.rs/walkdir/latest/walkdir/), [Rayon](https://docs.rs/rayon/latest/rayon/), [clap](https://docs.rs/clap/latest/clap/) | Use walkdir for recursive discovery with explicit root and nested symlink behavior, sorted paths, and coverage diagnostics for traversal errors. Add bounded Rayon workers after the serial fixtures pass. Use clap for the preview flags while preserving native CLI output and exit behavior. |
| 0.2 | [toml](https://docs.rs/toml/latest/toml/), [globset](https://docs.rs/globset/latest/globset/), [ignore](https://docs.rs/ignore/latest/ignore/), [BLAKE3](https://docs.rs/blake3/latest/blake3/), [tempfile](https://docs.rs/tempfile/latest/tempfile/), [serde-sarif](https://docs.rs/serde-sarif/latest/serde_sarif/) | Match TOML syntax, path rules, and config precedence against pinned Skylos fixtures. Evaluate ignore for declared Git-style filtering. Use BLAKE3 content fingerprints and versioned Serde records, with tempfile for atomic cache writes from the same directory. Evaluate serde-sarif for the declared report format and verify exported reports against the compatibility fixtures. |
| 0.3 | Python requirement parsing, evidence search, Git integration, and patch previews. See the [Phase 0.3 crate shortlist](phase-03.md#rust-crate-candidates). | Qualify candidates against the pinned test inventory, supported build targets, and packaging requirements. Preserve interpreter-independent static CLI operation. |
| 0.4 | [regex](https://docs.rs/regex/latest/regex/), [aho-corasick](https://docs.rs/aho-corasick/latest/aho_corasick/) | Use regex for reviewed text and byte patterns. Check Python lookaround and backreferences when porting rules. Evaluate the already transitive aho-corasick crate for a shared literal search pass. Preserve reference clone scores when optimizing candidate selection. |
| 0.5 | [petgraph](https://docs.rs/petgraph/latest/petgraph/), [fixedbitset](https://docs.rs/fixedbitset/latest/fixedbitset/) | Evaluate iterative strongly connected component analysis for recursive call groups and dense bitsets for flow facts. Keep propagation, budgets, and evidence paths in the analysis engine. |
| 0.6 | [notify](https://docs.rs/notify/latest/notify/), [lsp-server](https://docs.rs/lsp-server/latest/lsp_server/), [lsp-types](https://docs.rs/lsp-types/latest/lsp_types/) | Use notify for watch events and rescan when events may have been lost. Use the LSP pair for editor transport and messages, with position conversion at the adapter boundary. |
| Through 1.0 | [proptest](https://proptest-rs.github.io/proptest/proptest/getting-started.html), [cargo-deny](https://embarkstudios.github.io/cargo-deny/checks/index.html) | Use proptest in development for worker-order, cache-edit, and Unicode-position invariants. Evaluate cargo-deny in CI for advisories, licenses, and dependency sources. |

In the cache dependency trial, tempfile brought in `r-efi` 6.0.0. Its
[AUTHORS file](https://github.com/r-efi/r-efi/blob/v6.0.0/AUTHORS) contains
the license text that the current notice generator missed. Account for the
license file of the version resolved when adding the cache dependencies.

## Phase 0: freeze the contract and measure the foundation

Goal: establish whether the proposed architecture can preserve the intended
behavior while substantially reducing the cost of equivalent analysis.

- [x] Record the Skylos source revision, distribution artifact, Python version,
  dependencies, OS, configuration, and native-accelerator state in a baseline
  manifest.
- [x] Inventory CLI flags, configuration resolution, Python API options, result
  schemas, rule families, suppressions, exit behavior, and incomplete analysis.
- [x] Define the exact first Python dead-code subset and its unsupported surfaces.
- [x] Capture an authentic Cargo dependency lockfile and select the Ruff-compatible
  Rust minimum.
- [x] Qualify Rust 1.96 and the CPython 3.11–3.14 development matrix across
  Linux, macOS, and Windows.
- [x] Select and document the distribution license before the first release;
  record provenance for any imported upstream code or fixtures.
- [x] Capture small reviewed fixtures before optimizing against their output.
- [x] Evaluate a pinned, matching Ruff parser/AST crate set behind a project-owned
  adapter. Record supported syntax and parser error behavior.
- [x] Prove separate native CLI and PyO3 SDK distributions on Linux, macOS, and
  Windows; record installation behavior, linkage, startup cost, and artifact size.
- [x] Evaluate a combined native-CLI/SDK wheel as a separate packaging spike.
  Preserve the working separate distributions until the installed layout is
  verified; do not infer support merely from a build-target configuration example.
- [x] Build source discovery, parsing, scoped symbols, basic imports, roots,
  reachability, and machine-readable findings as one native vertical slice.
- [x] Benchmark discovery, read/decode, parse/extract, resolution, graph analysis,
  rules, and output separately as well as end to end.
- [x] Compare against Skylos with its existing accelerator both disabled and
  enabled, where that extension can be built reproducibly.

Evidence includes [runnable fixtures](../tests/conformance/phase0.toml), a
[versioned contract inventory](skylos-5.7.0-contract.md), and
[reproduction instructions and raw measurements](../benchmarks/phase0.md).

## 0.1: Python dead-code preview

Expand the [0.0.1 flat-project prototype](phase-0.md) into a native preview
for multi-file applications and explicitly bounded libraries. The preview
reports one rule, `unreachable_function`, under the declared source and entry
assumptions.

### Preview contract

| Area | 0.1 behavior |
| --- | --- |
| Source selection | Keep explicit file scans and add recursive discovery from declared source roots, including `src` layouts and ordinary packages with `__init__.py`. Use the requested directory as the default source root. Report roots, effective import search order, selected paths, and documented exclusions. Ambiguous module identities and selected namespace packages surface as coverage issues. |
| Syntax and symbols | Keep the Python 3.11 target and UTF-8 source policy. Model module and synchronous function scopes, including directly called nested functions, parameter and local shadowing, simple assignments and returns, and conditional branches with conservative reachability. Identify functions by module, owning scope, and definition. |
| Imports and declarations | Resolve absolute and relative imports, aliases, and explicit local re-exports within the selected source universe. An import required by the graph that cannot resolve within that universe produces incomplete coverage. Reachable imports activate the target module and its package initializers. Keep module execution, declaration creation, function calls, and value references distinct. A supported default expression can call a helper when its function is defined. |
| Application roots | Support direct-file, named-module, and callable entries. Keep the flat `main.py` and explicit-file defaults in direct-file mode. A missing or unresolved entry prevents a complete application scan. |
| Library roots | Activate declared public modules on import. Use explicit public callables and callable names in the effective literal `__all__` lists after initialization as externally callable roots. Resolve roots through supported re-exports and preserve code that a declared export can reach. A missing or unresolved public boundary prevents a complete library scan. |
| Uncertainty | Record possible use and the evidence for it. When a function value may escape, retain the known candidate and its potentially reachable callees without asserting that they were called. Withhold project-wide unreachable findings when the affected scope cannot be bounded. |
| Coverage and output | Record failed reads and parses, unsupported selected constructs, unresolvable graph inputs, detected source changes, and exhausted limits as incomplete coverage. Keep coverage, uncertainty, and findings separate. Use a versioned native report with deterministic paths, symbol identities, locations, and evidence. CLI stdout carries JSON. Exit 0 means completion, including runs with findings. Exit 2 covers incomplete scans and invocation or output failures. The SDK returns the same report. |

#### Entry and import resolution

- A direct-file entry models `python path/to/entry.py`. Its initial
  `__name__` is `__main__` and it has no implicit package context. An
  explicit file scan still selects that file alone by default. A reachable
  relative import from a direct-file entry with no package context produces
  incomplete coverage even when its target is among the selected files.
- A named-module entry models `python -m pkg.entry`. It runs the parent package
  initializers before the entry body, supports relative imports in that package,
  and starts the entry body with `__name__` set to `__main__`. For a package
  entry, the declaration names `pkg.__main__` explicitly. Keep entry execution
  and ordinary import of the same source in distinct execution contexts.
- A callable entry imports its owning module and package initializers before
  calling the selected function. Ordinary imports initialize `__name__` to the
  module's qualified name. Evaluate a main guard using the binding that reaches
  the condition, accounting for rebinding and lexical shadowing. If the
  condition can be both true and false, preserve possible call edges from both
  branches.
- For absolute imports, named-module, callable, and library entries use the
  declared source roots in declaration order, with the requested directory as
  the default. Direct-file entries search the script's directory first, then
  any explicitly declared roots in order. Resolve the top-level name in the
  first matching root within the selected sources, then resolve child names
  under the selected package. Within a root, a regular package takes
  precedence over a same-name module file. Record incomplete coverage if the
  lookup rules leave an ambiguous identity.
- For `from pkg import name`, resolve the package binding at that import site
  after package initialization. Search for and load `pkg.name` as a submodule
  only when the binding is absent. Loading a submodule binds it on the parent
  package and can change subsequent lookups. Earlier `from` aliases retain the
  values they captured. Preserve the possible targets when supported branches
  leave a finite set of bindings. Mark coverage incomplete if the target cannot
  be bounded.
- The preview uses a closed set of selected sources. If an import needed for
  reachability has no target in that set, record its location and reason, mark
  coverage incomplete, and withhold project-wide unreachable findings. This
  applies equally to an absent project module and to standard-library or
  third-party modules outside the selection. Analysis resolves imports
  statically from selected files.

#### Library export resolution

Resolve the effective `__all__` binding and each listed name in the module
namespace after reachable initialization. Assignments on paths that cannot
execute have no effect. If multiple reachable outcomes have different literal
lists or callable bindings, retain every possible callable and record possible
export evidence for targets that vary by outcome. Resolved noncallable values
add no callable root. A missing listed name or an export boundary the analyzer
cannot determine produces incomplete coverage. When `__all__` is absent, the
module needs an explicit public callable boundary.

The [preview interface](phase-01.md) specifies source roots, application or
library policy, entry and export declarations, worker budgets, and report fields.
Keep Phase 0's explicit diagnostics for symlinked scan targets and source files.
Recursive discovery treats symlinked directories as exclusions and records them.
Decorators, star imports, import cycles the preview cannot resolve, framework
models, general callback inference, dynamic exports, and Skylos-compatible
configuration and output remain later work. Persistent caching starts in 0.2.

### Implementation order

1. Establish the source snapshot and package identities. Read each selected file
   into one snapshot and derive both facts and locations from it. Test source
   selection and detected changes before adding graph features.
2. Extract scoped facts and resolve supported imports and exports in the serial
   path. Record shadowing, declaration effects, and unresolved relationships
   before making reachability decisions.
3. Add application and library roots, traverse resolved call and possible-use
   edges, and produce findings for unreachable functions and recursive groups.
   Exercise the same report through the native CLI and installed SDK.
4. Add bounded workers for per-file work once the serial fixtures pass. Merge
   facts deterministically and compare findings, evidence, uncertainties, and
   coverage across worker counts.

### Release criteria

| Reviewed cases | Required result |
| --- | --- |
| Source snapshot | A conventional package in a declared `src` root resolves relative imports and reports its exact file set. A module identity that remains ambiguous after lookup, an unreadable selected file, or a detected source change produces incomplete coverage with a reason. |
| Scoped identity | Same-name functions in different modules or scopes get distinct identities. A local binding that shadows an import prevents a call from being attributed to that import. |
| Entry modes | With the same selected package sources, a named-module entry resolves a relative import and retains helpers called from the parent initializer and under the main guard. A direct-file entry into that same file reports incomplete coverage at the relative import. An ordinary import or callable entry leaves an unmodified main guard dormant. |
| Absolute import order | With both `helper.py` and `pkg/helper.py` selected under a declared project root, the same `from helper import run` in `pkg/entry.py` reaches the sibling helper in direct-file mode and the root helper in named-module mode. Reversing two declared roots that both supply a module changes the resolved target in named-module mode. |
| Package imports | A package's re-exported `task` function takes precedence over a selected `pkg/task.py` in `from pkg import task`. The submodule stays dormant until an explicit import loads it and updates the parent binding. The earlier alias still calls the function, and a later `from` import binds the submodule. With no package binding, the `from` import itself loads the submodule. |
| Main guard bindings | Importing a module that first sets `__name__ = "__main__"` retains the helper called under its guard. When a parameter shadows `__name__` and its value is unknown, both possible branches contribute reachability evidence. |
| Recursion | A recursive group is reported when isolated and retained when reached from an entry. |
| Definition effects and exports | A helper called by a default expression survives even if the declared function's body is unreachable. A library re-export preserves its target and reachable helpers. A private sibling with no supported callers remains reportable. |
| Effective library exports | A dormant branch that reassigns `__all__` does not change the visible export. When two reachable outcomes export different callables, both are retained with possible export evidence. A listed name that remains unbound after initialization produces incomplete coverage. |
| Import coverage | A reachable import of a missing local module and a reachable `import pathlib` outside the selected sources each produce an import-site diagnostic, incomplete coverage, and no project-wide unreachable findings. A corresponding resolved local import permits complete coverage. |
| Uncertainty and failure | Possible-use evidence retains affected functions. Invalid syntax, an unbounded dynamic target, and exhausted limits produce explicit diagnostics and no project-wide non-use findings. A supported clean fixture reports complete coverage. |
| Interface and workers | CLI and installed SDK reports agree. Single-worker, multi-worker, and repeated runs match after removing documented timing and temporary-path fields. Vary file discovery and worker completion order. Compare coverage, uncertainty, evidence, finding identities, and locations. |

Publish the preview matrix and runnable fixtures, keeping the Phase 0 controls
as regressions. Record differences against [Skylos 5.7.0](skylos-5.7.0-contract.md)
for overlapping cases and preserve independently reviewed expectations. Capture
phase timings and coverage on a representative packaged workload using the
[benchmark protocol](../benchmarks/README.md). The 0.1 gate is correctness and
explicit coverage. Compatibility and speed claims require separate evidence.

## 0.2: declared Python dead-code compatibility

The [Phase 0.2 scope](phase-02.md) defines a Python 3.11 subset of the pinned
Skylos 5.7.0 dead-code contract alongside the native interfaces.

- [x] Freeze the supported configuration, semantic, and output fixtures
  against the pinned reference and independently reviewed expectations.
- [x] Implement the declared finding categories, export and callback cases,
  and bounded pytest and FastAPI models in the native engine.
- [x] Deliver an explicit Skylos profile for the supported CLI and SDK
  invocations, including confidence, suppressions, JSON, and SARIF.
- [x] Cache per-file facts and rebuild the graph from current inputs on every
  scan. Validate clean, cached, edited, serial, and parallel equivalence.
- [x] Record the compatibility matrix, reviewed discrepancies, and matched
  cold, warm, and edited benchmark receipts.

See the [pinned reference results](phase-02-reference.md),
[carry-forward holdout](phase-02-holdout.md), and
[cache benchmark](../benchmarks/phase02.md).

## 0.3: expanded Python dead-code coverage and workflows

Phase 0.3 requires every in-scope Python case and required variant in the pinned
Skylos dead-code suite to pass against Skarmory before release. Non-Python and
inseparable cross-language cases belong to their respective later milestones. The
[Phase 0.3 plan](phase-03.md) defines the workstream acceptance checks and
[upstream test gate](phase-03.md#required-upstream-test-gate). See the
[workstream issues](phase-03.md#scope-and-acceptance) to track implementation.

- [ ] Freeze the case and variant inventory, record pinned reference results,
  and map each assertion to Skarmory.
- [ ] Expand Python reachability, framework and test discovery models, and
  unused declared dependency checks.
- [ ] Add grep verification and positive runtime use evidence with versioned
  compatibility defaults and provenance.
- [ ] Add dead-code diff reports, complete base and head comparisons, baselines,
  coverage-aware gates, and cleanup behaviors exercised by the pinned suite.
- [ ] Require the reconciled upstream results before publication. Verify
  independent controls, CLI and SDK agreement, and cache and worker equivalence.

## 0.4: quality and secrets

Goal: reuse the native front end for declared local rule families.

- [ ] Inventory and port the selected quality rules with exact boundary fixtures.
- [ ] Define source/text discovery for secret rules independently of Python AST
  parsing; document which non-Python files those rules inspect.
- [ ] Implement suppressions, confidence/severity policy, and evidence consistently.
- [ ] Mask sensitive finding values appropriately in reports and logs.
- [ ] Preserve clone-detection semantics when introducing candidate filtering or
  faster similarity implementations.
- [ ] Separate rule-selection costs from shared parsing costs in benchmarks.

Exit evidence: reviewed positive and negative controls for each advertised rule,
conformance results, and no silent reduction in the analyzed file set.

## 0.5: security and data flow

Goal: deliver a declared, bounded security-analysis contract.

- [ ] Introduce context-specific source, sink, and sanitizer models.
- [ ] Implement selected local dangerous-operation checks.
- [ ] Build flow-sensitive function analysis with explicit unsupported operations.
- [ ] Add function summaries and supported propagation across calls and files.
- [ ] Define bounded fixed-point iteration, resource budgets, cancellation, and
  incomplete-result behavior.
- [ ] Preserve uncertainty at unknown library boundaries and dynamic dispatch.
- [ ] Produce evidence paths that explain the reported flow and its assumptions.
- [ ] Test identity, mutation, aliasing, and sanitizer-context negative controls.
- [ ] Establish independent security accuracy evidence and a separate performance
  budget; do not borrow the dead-code budget or claim complete program verification.

Exit evidence: rule and flow coverage matrices, independent reviewed cases, and
documented limits. Dependency advisories, registry queries, and LLM review have
separate integration and timing contracts.

## 0.6: developer workflows

Goal: use the engine reliably in CI and frequent edit loops.

- [ ] Extend the Phase 0.3 dead-code diff, baseline, and gate contracts to
  qualified rule families and CI integrations with versioned diagnostics.
- [ ] Preserve full project context and the separate changed-line and impact
  modes when integrating diff reports into CI and editor workflows.
- [ ] Add watch or resident sessions with consistent source generations and
  cancellation of superseded requests.
- [ ] Reuse the Phase 0.3 trace provenance and freshness checks in workflows
  that accept recorded runtime evidence.
- [ ] Add editor integration using the supported position-encoding contract.
- [ ] Extend the tested cleanup apply and rollback contract to additional
  transformations with source-fingerprint revalidation and applicability checks.
- [ ] Define any Python custom-visitor adapter and measure it separately from the
  native default path.

Exit evidence: end-to-end integration tests, cached/resident/clean equivalence,
and transformation-specific safety controls.

## 0.7–0.18: dedicated language frontends

Introduce the shared `skarmory_engine` and language-neutral `skarmory_core` boundary
in 0.7. Each later milestone adds a dedicated Rust frontend crate and passes its
own pinned dead-code cases, supported build variants, independent controls, and
earlier-phase regressions. See [language milestone details](language-frontends.md)
and [tracking issue #46](https://github.com/eddiethedean/skarmory/issues/46).

## 1.0: stable declared analysis

- [ ] Publish supported language syntax, operating systems, interpreter/wheel matrix,
  and analyzer feature matrix.
- [ ] Stabilize the native public configuration, typed Python API, diagnostic schema,
  error model, and documented CLI contracts.
- [ ] Publish exact Skylos compatibility scope and all reviewed differences.
- [ ] Publish reproducible speed results together with completeness and accuracy
  evidence, including unfavorable workloads.
- [ ] Verify wheel and source-distribution installation from clean environments.
- [ ] Document parser/model upgrades, cache invalidation, schema evolution, and
  policy changes.

Each language frontend has its own published compatibility scope and release
gate. Language backends beyond 0.18, external services, and cloud/LLM workflows
require their own milestones.

## Performance goals

The initial aspiration is a 5–10× reduction in wall time for matched Python
dead-code workloads. Responsiveness goals include approximately one second for
a specified 100K-line cold-cache workload, approximately 100 ms for its warm-cache
run, and sub-100 ms feedback for typical leaf edits in a resident session. These
figures are unmeasured hypotheses, not release claims or universal guarantees.
Phase 0 must specify hardware, file counts, dependency shape, rule coverage,
completeness, output format, and cache state before evaluating or revising them.

See the [benchmark plan](../benchmarks/README.md) and
[conformance plan](../tests/conformance/README.md) for evidence requirements.

## Sources and related decisions

- [Architecture](architecture.md)
- [ADR 0001: native engine](adr/0001-native-engine.md)
- [Skylos native accelerator scope](https://github.com/duriantaco/skylos/blob/87978e69adf91b3811e97b00c40aa39d7937cd94/rust/README.md)
- [Ruff internal-crate versioning](https://docs.astral.sh/ruff/versioning/)
- [Maturin bindings guide](https://www.maturin.rs/bindings)
