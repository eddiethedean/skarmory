# ADR 0001: Shared native analysis engine

- Status: accepted architectural direction
- Date: 2026-11-20
- Scope: engine boundaries, frontends, and compatibility strategy
- Implementation status: initial scaffold; analyzer and packaging milestones remain open

## Context

Skarmory should provide the useful Python analysis behavior of Skylos with the
responsiveness associated with native developer tools such as Ruff. Its main
analysis pipeline must support project-wide resolution, framework awareness,
reachability, explainable uncertainty, and later data-flow rules.

The initial reference is Skylos 5.7.0 at commit
`87978e69adf91b3811e97b00c40aa39d7937cd94`. Skylos already uses Python process
parallelism and a run-scoped AST cache, and includes an experimental Rust
extension for selected helpers. A new implementation must measure equivalent
work and cannot assume that changing the language alone guarantees a particular
speedup.

Sources:

- [Skylos parallel analysis](https://github.com/duriantaco/skylos/blob/87978e69adf91b3811e97b00c40aa39d7937cd94/skylos/scale/parallel_static.py)
- [Skylos AST cache](https://github.com/duriantaco/skylos/blob/87978e69adf91b3811e97b00c40aa39d7937cd94/skylos/analysis/ast_cache.py)
- [Skylos native extension and documented differences](https://github.com/duriantaco/skylos/blob/87978e69adf91b3811e97b00c40aa39d7937cd94/rust/README.md)

## Decision

Implement one Rust engine shared by a native CLI and a thin PyO3 Python API.
Keep source discovery, parsing, resolution, framework models, graphs, analysis,
caching, and diagnostic generation in Rust. Keep interpreter-specific
conversion and integrations in the Python boundary.

Use a versioned Skylos adapter for the declared compatibility surface. Preserve
legacy behavior through translation and conformance tests rather than a second
analysis implementation. Offer a separate typed Skarmory API. An unsupported
feature must produce an explicit limitation or error rather than silently
reducing analysis coverage.

Evaluate Ruff's parser and AST crates first. Pin the tested dependency set and
isolate it behind a project-owned adapter because those internal interfaces have
no stability guarantee. Persist our own versioned semantic facts rather than
Ruff AST objects.
[Ruff crate versioning](https://docs.astral.sh/ruff/versioning/#crate-versioning)

Use bounded native CPU parallelism, initially evaluating Rayon. Detach from the
Python interpreter for analysis. Do not require Tokio or per-node Python
callbacks for the default scanner.
[PyO3 parallelism](https://pyo3.rs/main/parallelism)

Start caching at the file-fact boundary and recompute project reachability from
cached summaries. Add more granular dependency-aware invalidation only when
profiling demonstrates its value and clean/cached equivalence is maintained.

The initial scaffold uses a standard PyO3 SDK wheel with a `skarmory._native`
extension and an independent native CLI built through Cargo. The SDK wheel does
not bundle the executable. A single-install distribution is a longer-term goal
and an unresolved Phase 0 packaging spike. Maturin's target-selection settings
do not establish automatic combined binary-and-extension packaging support.
[Maturin binary/library packaging discussion](https://github.com/PyO3/maturin/issues/368)

## Alternatives considered

| Alternative | Assessment |
| --- | --- |
| Accelerate individual Skylos helpers | Useful upstream work, but leaves the rest of the Python pipeline on the critical path and does not establish the standalone product architecture |
| Run the CLI through a Python entry point calling Rust | Simpler packaging, but includes interpreter startup in frequent command execution; acceptable fallback if measured tradeoffs justify it |
| Separate Python and Rust analysis implementations | Increases semantic drift and maintenance cost; rejected for the main engine |
| Reuse an entire linter implementation | Does not supply the intended Skylos compatibility contract or project-wide analysis policies; reuse focused frontend components instead |
| Build a Python parser from scratch immediately | Expands the language-compatibility burden before validating the product; defer unless the parser spike identifies a concrete blocker |
| Introduce a daemon and fully incremental graph engine first | Adds lifetime and invalidation complexity before measuring the simpler design; persistent sessions may follow |

## Consequences

### Benefits

- CLI and SDK use the same analysis and evidence model.
- Native execution can reduce interpreter and process-serialization overhead.
- Compact native facts and shared indexes can reduce memory and repeated work.
- Compatibility can be reviewed independently from native API evolution.
- The Rust core can eventually be reused by other tools, including an upstream
  integration, without requiring that integration for the standalone project.

These are architectural expectations. Benchmarks must establish their effect.

### Costs and obligations

- Python semantics and framework behavior still require substantial modeling.
- Unstable parser dependencies require deliberate upgrades and conformance work.
- A native CLI plus extension may increase wheel size and release complexity.
- Python visitor compatibility is an optional integration with an explicit cost.
- Cross-file findings require semantic invalidation, not just file-result caches.
- Native concurrency requires deterministic merges, cancellation, bounded
  resources, and thread-safe session ownership.

### Correctness boundaries

An empty finding set is not a complete analysis result. Reports must preserve
coverage and uncertainty. Library APIs, callbacks, dynamic dispatch, and
framework registration cannot be reduced to simple reference counts. Finding a
symbol apparently unused does not itself establish that removing its declaration
has no effects.

The scaffold must report unavailable analysis honestly. Compatibility claims
and speed claims begin only after the relevant tests and measurements exist.

## Validation and reconsideration

The first vertical slice must produce native Python dead-code results from
discovery through project reachability and machine output. Compare it with the
pinned reference on a declared feature set and review every difference.

Validate cold scans, warm scans, leaf edits, and root/export changes separately.
Require equivalent normalized findings, evidence, and coverage across caches
and worker counts. Test packaging on each advertised platform before release.

Revisit this decision if the parser cannot support the required Python syntax,
the shared-core boundary cannot preserve the agreed semantics, packaging costs
prevent reasonable distribution, or measured performance fails to justify the
maintenance burden. Record such changes in a new ADR rather than silently
altering the compatibility contract.

The detailed design is in [the architecture document](../architecture.md).
