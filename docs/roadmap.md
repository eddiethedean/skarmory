# Skarmory roadmap

Status: [Phase 0](phase-0.md) is complete. Version labels describe planned
milestones.

The default distribution uses a PyO3 SDK wheel and a native Cargo CLI. The
combined wheel is an experimental installed layout.

## Product goal

Build a Python code analyzer with a shared native Rust engine, a native CLI, and
a thin Python API. Preserve the useful analysis behavior and framework awareness
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
| 0.1 | Python dead-code preview | Phase 0 | Core semantic fixtures pass; limitations explicit |
| 0.2 | Declared Python dead-code compatibility | 0.1 | No unexplained conformance differences |
| 0.3 | Quality and secrets | 0.2 | Rule-level conformance and independent controls |
| 0.4 | Security and data flow | 0.3 and semantic infrastructure | Accuracy, completeness, and resource budgets verified |
| 0.5 | Developer workflows | Relevant stable rule families | Integration and narrow edit contracts verified |
| 1.0 | Stable declared Python analysis contract | Earlier gates | Published feature matrix, APIs, wheels, and evidence |

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

Goal: produce useful, explainable native findings for a deliberately bounded
Python subset.

- [ ] Implement source snapshots, package discovery, and explicit source-root
  configuration.
- [ ] Resolve lexical scopes, shadowing, import aliases, and supported re-exports.
- [ ] Give symbols identities beyond their unqualified names.
- [ ] Distinguish references, function execution, declaration effects, and roots.
- [ ] Find unreachable recursive groups from declared roots.
- [ ] Support initial application and library policies without inventing external
  callers or deleting public APIs by assumption.
- [ ] Preserve possible dynamic use and explain unsupported resolution.
- [ ] Report failures and analysis coverage separately from findings.
- [ ] Add stable normalized output, explicit errors, and documented preview CLI
  behavior.
- [ ] Establish deterministic single-worker and multi-worker equivalence.

Exit evidence: paired live/dead fixtures and a published preview matrix. This
milestone is not a complete replacement for Skylos.

## 0.2: declared Python dead-code compatibility

Goal: satisfy the frozen Python dead-code contract and make repeated scans fast.

- [ ] Port declared confidence thresholds, scoring order, evidence, and
  category-specific filtering behavior.
- [ ] Match configuration discovery and precedence, exclusions, ignores,
  whitelists, path overrides, and custom entry-point selectors.
- [ ] Resolve public API exposure, re-export chains, and supported lazy exports.
- [ ] Model the declared frameworks, including registration activation and
  imported identity rather than names alone.
- [ ] Cover callbacks, closures, async scheduling, executor submissions, and
  multiprocessing targets within the supported semantic model.
- [ ] Add the legacy JSON/API adapter alongside the new typed Python interface.
- [ ] Match the promised output formats and command-specific exit contracts.
- [ ] Cache module facts using content and semantic-version fingerprints; rebuild
  graph reachability conservatively before pursuing partial graph updates.
- [ ] Test edits to callers, exports, roots, configuration, and the file universe
  against clean rebuilds.
- [ ] Review every mismatch and publish any intentional differences.

Exit evidence: no unexplained differences for the declared contract, no regression
on the independently reviewed retain/detect controls, and matched cold/warm/edit
benchmarks. Arbitrary Python AST visitors remain an explicitly unsupported or
separately adapted interface until their contract is implemented.

## 0.3: quality and secrets

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

## 0.4: security and data flow

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

## 0.5: developer workflows

Goal: use the engine reliably in CI and frequent edit loops.

- [ ] Add baseline/gate policy and versioned diagnostics for CI consumers.
- [ ] Analyze sufficient project context before applying diff-report filters.
- [ ] Keep changed-line reporting distinct from reporting affected unchanged code.
- [ ] Add watch or resident sessions with consistent source generations and
  cancellation of superseded requests.
- [ ] Import positive trace/coverage evidence with provenance and freshness checks.
  Unobserved execution is not proof that code is unused.
- [ ] Add explicitly requested runtime tracing as a separate Python integration.
- [ ] Add editor integration using the supported position-encoding contract.
- [ ] Introduce narrowly reviewed cleanup transformations with dry-run output,
  source-fingerprint revalidation, and explicit applicability.
- [ ] Define any Python custom-visitor adapter and measure it separately from the
  native default path.

Exit evidence: end-to-end integration tests, cached/resident/clean equivalence,
and transformation-specific safety controls.

## 1.0: stable declared Python analysis

- [ ] Publish supported Python syntax, operating systems, interpreter/wheel matrix,
  and analyzer feature matrix.
- [ ] Stabilize the native public configuration, typed Python API, diagnostic schema,
  error model, and documented CLI contracts.
- [ ] Publish exact Skylos compatibility scope and all reviewed differences.
- [ ] Publish reproducible speed results together with completeness and accuracy
  evidence, including unfavorable workloads.
- [ ] Verify wheel and source-distribution installation from clean environments.
- [ ] Document parser/model upgrades, cache invalidation, schema evolution, and
  policy changes.

Other language backends, external services, and cloud/LLM workflows require their
own compatibility milestones. A stable Python release does not establish parity
with every feature of the broader Skylos product.

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
