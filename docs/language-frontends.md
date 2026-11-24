# Language frontend milestones

Phase 0.3 is Python-only. The Python quality, security and workflows milestones
remain 0.4–0.6. Beginning in Phase 0.7, each language has its own Rust workspace
crate and release gate. Related ECMAScript variants share one frontend. Each
release includes previously qualified languages and preserves earlier contracts.

## Shared architecture and release gates

- `skarmory_core` owns language-neutral facts, graphs, reachability algorithms,
  symbol identities, evidence, diagnostics and coverage. It has no frontend
  or parser dependencies. `skarmory_engine` owns discovery, immutable snapshots,
  frontend selection, resource budgets, cache orchestration, cross-language
  references and aggregate reporting. CLI and SDK depend on the engine.
- Each frontend crate owns its parser, grammar, language facts, build metadata,
  resolution, entry points, language-specific rules and tests. Move existing
  Python and JS/TS semantics into their appropriate frontend crates and isolate
  the draft Go, Rust, C++ and Java modules before shipping them.
- Optional Cargo features select frontend crates; default distributions enable
  the qualified languages. Explicitly requested unavailable languages report
  incomplete coverage. Package builds and notices must be validated for
  each frontend, with no target-code execution in the static CLI.
- Freeze each language's reviewed cases and required variants from pinned Skylos
  5.7.0. Preserve exact assertions, mixed-language dependencies, and provenance.
  A case requiring several languages belongs to the milestone where all required
  frontends are available. No future case earns pass credit or blocks an earlier
  release. Every in-scope case must pass normally. Retain prior regression suites,
  paired live/dead controls, uncertainty, cache/worker equivalence, CLI/SDK parity,
  default and minimal builds, locked environments, notices and artifact identities.
- Measure accuracy, supported coverage, runtime, memory and artifact size for
  each language; do not claim coverage beyond the verified inventory.

Tracking [#46](https://github.com/eddiethedean/skarmory/issues/46).

## Phase 0.7

**JavaScript / TypeScript** — `skarmory_javascript`. [Issue #45](https://github.com/eddiethedean/skarmory/issues/45).

Complete [shared engine/frontend infrastructure #142](https://github.com/eddiethedean/skarmory/issues/142)
before qualifying the JavaScript/TypeScript frontend. Introduce the engine,
language-neutral core, Python frontend migration, dependency boundaries and
feature registry. Qualify module resolution, package exports, tsconfig paths,
re-exports, unused files, exports, symbols, enum members, npm dependencies,
component references,
and TypeScript cleanup preview/apply/rollback. Resolve the missing original
fixtures below before awarding pass credit.

## Phase 0.8

**Rust** — `skarmory_rust`. [Issue #130](https://github.com/eddiethedean/skarmory/issues/130).

Qualify `Cargo.toml`, module paths, trait and inherent method ownership, macro expansion, and conditional compilation. Qualify semantic resolution for bindings that syntax cannot identify.

Pinned anchors: [Function reachability](https://github.com/duriantaco/skylos/blob/87978e69adf91b3811e97b00c40aa39d7937cd94/test/test_rust_function_reachability.py), [coverage](https://github.com/duriantaco/skylos/blob/87978e69adf91b3811e97b00c40aa39d7937cd94/test/test_rust_analysis_coverage.py).

## Phase 0.9

**Go** — `skarmory_go`. [Issue #131](https://github.com/eddiethedean/skarmory/issues/131).

Qualify `go.mod`, `go.work`, build selection, standard library, typed callbacks and selectors. The pinned native reference uses Go 1.22. Source-derived identity is insufficient for arbitrary interface dispatch and external packages.

Pinned anchors: [Native reachability group](https://github.com/duriantaco/skylos/blob/87978e69adf91b3811e97b00c40aa39d7937cd94/skylos/engines/go/internal/symbols/reachability_test.go), [workspace](https://github.com/duriantaco/skylos/blob/87978e69adf91b3811e97b00c40aa39d7937cd94/test/test_go_workspace_context.py).

## Phase 0.10

**Java** — `skarmory_java`. [Issue #132](https://github.com/eddiethedean/skarmory/issues/132).

Qualify Java package and class identities, service descriptors, FXML namespaces and resources, and receiver-specific dispatch.

Pinned anchors: [Service and FXML benchmark cases](https://github.com/duriantaco/skylos/blob/87978e69adf91b3811e97b00c40aa39d7937cd94/benchmarks/dead_code/manifest.json).

## Phase 0.11

**C++** — `skarmory_cpp`. [Issue #133](https://github.com/eddiethedean/skarmory/issues/133).

Qualify Per-file linkage, references and direct calls, preprocessing and includes, compiler or build metadata when needed.

Pinned anchors: [Discovery and local identity](https://github.com/duriantaco/skylos/blob/87978e69adf91b3811e97b00c40aa39d7937cd94/test/test_cpp_integration.py), [corpus](https://github.com/duriantaco/skylos/blob/87978e69adf91b3811e97b00c40aa39d7937cd94/test/test_cpp_corpus.py).

## Phase 0.12

**C#** — `skarmory_csharp`. [Issue #134](https://github.com/eddiethedean/skarmory/issues/134).

Qualify Project files, overload and type identity, application roots, and markup handlers.

Pinned anchors: [Application reachability](https://github.com/duriantaco/skylos/blob/87978e69adf91b3811e97b00c40aa39d7937cd94/test/test_csharp_app_reachability.py), [markup and overloads](https://github.com/duriantaco/skylos/blob/87978e69adf91b3811e97b00c40aa39d7937cd94/test/test_csharp_overload_and_markup_refs.py).

## Phase 0.13

**Swift** — `skarmory_swift`. [Issue #135](https://github.com/eddiethedean/skarmory/issues/135).

Qualify SwiftPM source selection, private file identity, access policy, and reflective entry points.

Pinned anchors: [Integration and private identity](https://github.com/duriantaco/skylos/blob/87978e69adf91b3811e97b00c40aa39d7937cd94/test/test_swift_integration.py), [reflection](https://github.com/duriantaco/skylos/blob/87978e69adf91b3811e97b00c40aa39d7937cd94/test/test_swift_project_reflection.py).

## Phase 0.14

**Kotlin** — `skarmory_kotlin`. [Issue #136](https://github.com/eddiethedean/skarmory/issues/136).

Qualify Gradle source sets, package and member ownership, and callback entry points.

Pinned anchors: [Kotlin cases](https://github.com/duriantaco/skylos/blob/87978e69adf91b3811e97b00c40aa39d7937cd94/test/test_kotlin.py).

## Phase 0.15

**Dart** — `skarmory_dart`. [Issue #137](https://github.com/eddiethedean/skarmory/issues/137).

Qualify Package imports, entry roots, owner identity, and the required grammar variant.

Pinned anchors: [Imported call identity](https://github.com/duriantaco/skylos/blob/87978e69adf91b3811e97b00c40aa39d7937cd94/test/test_dart_project_call_identity.py), [optional grammar](https://github.com/duriantaco/skylos/blob/87978e69adf91b3811e97b00c40aa39d7937cd94/test/test_dart_optional_extra.py).

## Phase 0.16

**PHP** — `skarmory_php`. [Issue #138](https://github.com/eddiethedean/skarmory/issues/138).

Qualify Composer inputs, namespace aliases, receiver and trait ownership, and framework registration.

Pinned anchors: [Callable identity](https://github.com/duriantaco/skylos/blob/87978e69adf91b3811e97b00c40aa39d7937cd94/test/test_php_dead_code_identity.py).

## Phase 0.17

**Ruby** — `skarmory_ruby`. [Issue #139](https://github.com/eddiethedean/skarmory/issues/139).

Qualify Gem/package source scope, reopening and inheritance, aliases, and callback lineage.

Pinned anchors: [Ruby scopes and callbacks](https://github.com/duriantaco/skylos/blob/87978e69adf91b3811e97b00c40aa39d7937cd94/test/test_ruby_scope_callbacks.py).

## Phase 0.18

**Robot Framework** — `skarmory_robot`. [Issue #140](https://github.com/eddiethedean/skarmory/issues/140).

Qualify Suites, resources, variable files, Python library exports, and keyword semantics.

Pinned anchors: [Python library liveness](https://github.com/duriantaco/skylos/blob/87978e69adf91b3811e97b00c40aa39d7937cd94/test/test_robot_framework_liveness.py), [dead blocks](https://github.com/duriantaco/skylos/blob/87978e69adf91b3811e97b00c40aa39d7937cd94/test/test_robot_framework_dead_blocks.py).

## Recorded implementation work and prerequisite research

The following sections preserve earlier prototype evidence and research. They do not qualify a new language release.

### Missing TypeScript fixtures (Phase 0.7)

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

The source audit identifies where to start collection. The frozen language case inventory and execution results belong to Phase 0.7.

### Existing JavaScript and TypeScript preview in main

The earlier experimental `skylos-0.3` implementation supports `SKY-E003` for a single package scanned
from its root directory. It reads the root `package.json` main entry and
follows local static ECMAScript imports and re-exports, including type-only
imports. It accepts exact source paths and unique extensionless file or
`index` matches among selected JavaScript and TypeScript sources. For a
TypeScript importer, a `.js`, `.jsx`, or `.mjs` specifier prefers a matching
TypeScript source before the emitted JavaScript file. Disconnected files,
including groups that only import each other, appear in `unused_files`.
The report records reached modules in `dead_code_evidence` and applies the
project rule ignore and first-line `// skylos: ignore[SKY-E003]` directive.

The CLI and installed Python SDK use the same Rust graph. Cached scans reuse
file-local parser facts and rebuild the graph from the current sources and
package manifest. The `javascript_file_analysis` summary identifies the rule,
its supported scope, and its coverage status.

Missing, excluded, or ambiguous import targets make the file analysis
incomplete. File-scoped scans, bare package imports, CommonJS and dynamic
loading, declaration files, and detected alternative project configuration
also produce incomplete coverage. JavaScript parsing and reachability gaps
keep the overall report incomplete and preserve findings from completed Python
analysis. The parser accepts up to 16 KiB of source per file in this slice.
Larger inputs produce an explicit analysis-limit diagnostic. If JavaScript and
TypeScript sources exceed the space left in the shared snapshot budget, the
file graph is incomplete and Python analysis can still use its admitted inputs.
Package exports, workspace and `tsconfig` resolution, package scripts, browser
and component assets, binding and export analysis, and npm dependency
analysis remain part of [#45](https://github.com/eddiethedean/skarmory/issues/45).


### Draft implementations (PR #129)

Draft PR #129 reports selected Go, Rust, C++, and Java sources through the shared
CLI and SDK. The `frontend_analysis` summary reports coverage independently
for each selected frontend. Versioned file facts are cached, and each scan
rebuilds name resolution and re-reads any supported project metadata.
Missing declarations, uncertain bindings, unsupported syntax, and exceeded
budgets produce coverage issues. The static scanner uses compiled-in parsers
and executes no project code.

| Frontend | Implemented boundary |
| --- | --- |
| Go | One module, ordinary package functions, direct references and simple receiver identities. The native graph starts from package entry points and keeps call cycles separate from the roots that can reach them. Workspaces, build-sensitive source selection, and unresolved semantic cases retain explicit gaps. |
| Rust | A single `main.rs` or `lib.rs` crate root with parameterless free functions, direct calls, and local named function values. Module graphs, macros, attributes, and unresolved bindings report incomplete coverage. |
| C++ | File-local free-function identity, ordinary direct references and addresses, and parse diagnostics for the six pinned source and header suffixes. Build-dependent inputs and unresolved forms report coverage gaps. |
| Java | Selected class and method reachability, standard entry points, and statically resolved service and JavaFX controller declarations. Unknown resource targets and unsupported or ambiguous bindings report coverage gaps. |

The table below records the initial pinned anchors for issue #46. The complete
case-level inventory, build variants, and original assertion mappings are
tracked under the owning language issues. A
frontend requires both semantic controls and a verified distribution before
claiming release compatibility. Selected sources for the seven frontends
marked pending are counted and produce `unsupported_frontend`. Selected
Robot Framework sources also suspend Python non-use conclusions because
library declarations can activate Python functions.

| Frontend | Pinned case anchors | Inputs and semantics to qualify |
| --- | --- | --- |
| Go | [Native reachability group](https://github.com/duriantaco/skylos/blob/87978e69adf91b3811e97b00c40aa39d7937cd94/skylos/engines/go/internal/symbols/reachability_test.go), [workspace](https://github.com/duriantaco/skylos/blob/87978e69adf91b3811e97b00c40aa39d7937cd94/test/test_go_workspace_context.py) | `go.mod`, `go.work`, build selection, standard library, typed callbacks and selectors. The pinned native reference uses Go 1.22. Source-derived identity is insufficient for arbitrary interface dispatch and external packages. |
| Rust | [Function reachability](https://github.com/duriantaco/skylos/blob/87978e69adf91b3811e97b00c40aa39d7937cd94/test/test_rust_function_reachability.py), [coverage](https://github.com/duriantaco/skylos/blob/87978e69adf91b3811e97b00c40aa39d7937cd94/test/test_rust_analysis_coverage.py) | `Cargo.toml`, module paths, trait and inherent method ownership, macro expansion, and conditional compilation. Qualify semantic resolution for bindings that syntax cannot identify. |
| C++ | [Discovery and local identity](https://github.com/duriantaco/skylos/blob/87978e69adf91b3811e97b00c40aa39d7937cd94/test/test_cpp_integration.py), [corpus](https://github.com/duriantaco/skylos/blob/87978e69adf91b3811e97b00c40aa39d7937cd94/test/test_cpp_corpus.py) | Per-file linkage, references and direct calls, preprocessing and includes, compiler or build metadata when needed. |
| Java | [Service and FXML benchmark cases](https://github.com/duriantaco/skylos/blob/87978e69adf91b3811e97b00c40aa39d7937cd94/benchmarks/dead_code/manifest.json) | Java package and class identities, service descriptors, FXML namespaces and resources, and receiver-specific dispatch. |
| Swift (pending) | [Integration and private identity](https://github.com/duriantaco/skylos/blob/87978e69adf91b3811e97b00c40aa39d7937cd94/test/test_swift_integration.py), [reflection](https://github.com/duriantaco/skylos/blob/87978e69adf91b3811e97b00c40aa39d7937cd94/test/test_swift_project_reflection.py) | SwiftPM source selection, private file identity, access policy, and reflective entry points. |
| PHP (pending) | [Callable identity](https://github.com/duriantaco/skylos/blob/87978e69adf91b3811e97b00c40aa39d7937cd94/test/test_php_dead_code_identity.py) | Composer inputs, namespace aliases, receiver and trait ownership, and framework registration. |
| C# (pending) | [Application reachability](https://github.com/duriantaco/skylos/blob/87978e69adf91b3811e97b00c40aa39d7937cd94/test/test_csharp_app_reachability.py), [markup and overloads](https://github.com/duriantaco/skylos/blob/87978e69adf91b3811e97b00c40aa39d7937cd94/test/test_csharp_overload_and_markup_refs.py) | Project files, overload and type identity, application roots, and markup handlers. |
| Kotlin (pending) | [Kotlin cases](https://github.com/duriantaco/skylos/blob/87978e69adf91b3811e97b00c40aa39d7937cd94/test/test_kotlin.py) | Gradle source sets, package and member ownership, and callback entry points. |
| Ruby (pending) | [Ruby scopes and callbacks](https://github.com/duriantaco/skylos/blob/87978e69adf91b3811e97b00c40aa39d7937cd94/test/test_ruby_scope_callbacks.py) | Gem/package source scope, reopening and inheritance, aliases, and callback lineage. |
| Dart (pending) | [Imported call identity](https://github.com/duriantaco/skylos/blob/87978e69adf91b3811e97b00c40aa39d7937cd94/test/test_dart_project_call_identity.py), [optional grammar](https://github.com/duriantaco/skylos/blob/87978e69adf91b3811e97b00c40aa39d7937cd94/test/test_dart_optional_extra.py) | Package imports, entry roots, owner identity, and the required grammar variant. |
| Robot Framework (pending) | [Python library liveness](https://github.com/duriantaco/skylos/blob/87978e69adf91b3811e97b00c40aa39d7937cd94/test/test_robot_framework_liveness.py), [dead blocks](https://github.com/duriantaco/skylos/blob/87978e69adf91b3811e97b00c40aa39d7937cd94/test/test_robot_framework_dead_blocks.py) | Suites, resources, variable files, Python library exports, and keyword semantics. |


## Parser and semantic research candidates

| Workstream | Candidate | What to verify before adoption |
| --- | --- | --- |
| JavaScript and TypeScript | Use `oxc_parser`, `oxc_semantic`, `oxc_allocator`, and `oxc_span` at `0.159.0` for parsing and binding. Use `oxc_resolver` `11.24.3` for Node resolution and `tsconfig` paths. | Check package exports, re-exports, type-only imports, and enum member identity against pinned fixtures. Keep package-script evidence, project-wide reachability, and coverage reporting in Skarmory. [Parser](https://oxc.rs/docs/guide/usage/parser.html), [semantic analysis](https://docs.rs/oxc_semantic/0.159.0/oxc_semantic/), [resolver](https://docs.rs/oxc_resolver/11.24.3/oxc_resolver/) |
| Other frontends | Start with `tree-sitter` `0.27.2` and individually pinned, compiled-in grammars. The smoke test covered `tree-sitter-go` `0.25.0`, `tree-sitter-c-sharp` `0.24.0`, and `tree-sitter-dart-orchard` `0.7.1`. Check grammar candidates for Rust, Swift, PHP, Java, C++, Ruby, Kotlin, and Robot Framework where the inventory requires them. | Verify grammar compatibility, syntax error handling, and source spans on pinned fixtures. Qualify grammar and native scanner licensing and packaging. Establish a build and distribution path for the Robot grammar. Skarmory owns language-specific symbol extraction and reachability. [Tree-sitter](https://docs.rs/tree-sitter/0.27.2/tree_sitter/), [Dart grammar](https://docs.rs/crate/tree-sitter-dart-orchard/0.7.1), [Robot grammar candidate](https://github.com/Hubro/tree-sitter-robot) |
| Go and Rust semantics | Start with syntax-derived facts. Compare a narrowly scoped `go/types` fact adapter and an isolated `ra_ap_hir` `0.0.363` pilot if the pinned tests require richer facts. | Run pinned Go tests that exercise type identity and generics, and Rust fixtures that require semantic resolution. Measure what each approach adds to build time and binary size. Qualify how any adapter would ship and keep static scans free of target-code execution. [Pinned Go tests](https://github.com/duriantaco/skylos/blob/87978e69adf91b3811e97b00c40aa39d7937cd94/skylos/engines/go/internal/symbols/reachability_test.go), [`go/types`](https://pkg.go.dev/go/types), [`ra_ap_hir`](https://docs.rs/ra_ap_hir/0.0.363/ra_ap_hir/) |

### Historical dependency smoke tests

An isolated Cargo project passed five tests with Rust `1.98.1` on
`aarch64-apple-darwin` using an offline build and a locked dependency graph. It
parsed and bound TypeScript, resolved a `tsconfig` path alias, loaded the Go,
C#, and Dart grammars on one runtime, checked Git `HEAD` and parsed a PEP 508
requirement, and verified literal match offsets and unified diff output. The
initial Git build failed with `status` and `blob-diff` alone. The passing build
enabled `sha1` and `revision` as well.

