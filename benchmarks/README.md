# Benchmark plan

Status: methodology for the initial scaffold. No benchmark runner, corpus, timing
result, speedup, or general accuracy claim is delivered by this file. Implementing
the harness and recording the first results is Phase 0 work.

## Questions to answer

1. Does Skarmory complete the same declared analysis substantially faster than
   the pinned Skylos reference?
2. Does that improvement preserve required findings, retain live code, and keep
   uncertainty and incomplete analysis visible?
3. Which phases dominate cold, warm, and edited-project workloads?
4. How do latency, CPU use, and memory change with source size, graph shape,
   worker budget, and enabled rule families?

Ruff is a responsiveness and architecture reference. Its normal lint workload is
not equivalent to whole-project dead-code or interprocedural security analysis.
Its documented parallel file pipeline is useful design context, not proof of a
Skarmory speed target.
[Source](https://docs.astral.sh/ruff/contributing/#compilation-pipeline).

## Baselines

| Baseline | Purpose |
| --- | --- |
| Skylos 5.7.0, Python fallback backend | Reproducible reference behavior and cost |
| Same Skylos build with its optional native extension | Compare against existing acceleration |
| Skarmory release-mode binary | End-to-end native CLI latency |
| Skarmory Python API | SDK conversion and call overhead plus engine cost |
| Skarmory resident session, when implemented | Incremental feedback without process startup |
| Pinned Ruff release, separately labeled | Local lint responsiveness reference |

Pin Skylos source revision
`87978e69adf91b3811e97b00c40aa39d7937cd94`. Record whether its native extension is
installed and which backend actually ran. The extension documentation describes
limited accelerated operations and possible finding differences; those must not
be attributed only to speed.
[Source](https://github.com/duriantaco/skylos/blob/87978e69adf91b3811e97b00c40aa39d7937cd94/rust/README.md).

## Match the work

Each comparison must specify:

- Identical input revisions and effective source-file sets.
- Supported language, syntax version, source roots, environment, and dependencies.
- Equivalent rule families, thresholds, framework models, ignores, and policies.
- Comparable output mode, evidence detail, and output destination.
- Completion status and any skipped, failed, capped, or unavailable work.
- Native-extension state and worker budget.

Separate local static work from registry/advisory lookups, LLM calls, test
execution, and runtime tracing. Those integrations may have their own benchmarks,
including offline cache freshness and failures, but their latency is not a native
CPU-engine result.

Do not compare a dead-code-only Skarmory preview with Skylos `-a`, or treat fewer
findings as a performance optimization without explaining the semantic change.

## Workload matrix

| Scenario | Preparation | What it reveals |
| --- | --- | --- |
| Cold analysis cache | Fresh process; remove only the tool's analysis cache | Discovery, parse, resolution, and graph cost |
| Warm unchanged | Fresh process; valid analysis cache | Startup, verification, cache read, graph, and output cost |
| Leaf-body edit | Change a function body with limited dependencies | Local recomputation and invalidation |
| Caller/root removal | Remove the last reachable edge or entry point | Correct nonlocal liveness changes |
| Export/import edit | Change re-exports, imports, or an API surface | Dependency propagation |
| Module added/deleted/renamed | Change the file universe | Resolution and identity invalidation |
| Configuration/model change | Change roots, ignores, targets, or model version | Semantic cache invalidation |
| Resident edit | Same supported session; apply a defined edit | Interactive engine latency |
| Large connected component | Freeze a synthetic cyclic/callback graph | Graph and fixed-point scaling |
| Dense clone candidates | Freeze repetitive source inputs | Similarity candidate and memory costs |

A cold tool cache is not necessarily a cold OS page cache. Record OS cache state
and storage conditions rather than implying that deleting the analysis cache
flushes disk caches. Resident-session results must never be substituted for fresh
CLI timings.

## Corpus

Build a versioned corpus with three distinct roles:

1. **Semantic regression fixtures:** small, reviewed positive/negative cases that
   prevent known bugs from returning.
2. **Real repository workloads:** pinned applications and libraries with different
   framework, import, callback, and concurrency patterns.
3. **Independent holdout:** source-labeled cases frozen before detector tuning and
   excluded from development decisions until the evaluation checkpoint.

Use the user's package projects as adoption workloads alongside external projects.
Do not treat one project's coding conventions as the entire ecosystem. Record
repository licenses and attribution before distributing source fixtures.

Skylos's benchmark documentation explicitly distinguishes regression tests from
independent accuracy evidence. Preserve that separation in Skarmory: passing
fixtures is a regression result, not a population accuracy estimate.
[Source](https://github.com/duriantaco/skylos/blob/87978e69adf91b3811e97b00c40aa39d7937cd94/BENCHMARK.md).

## Measurements

Capture end-to-end wall time, CPU time, peak memory, worker count, and repeated-run
distribution. Record separately:

- CLI startup and configuration.
- Discovery and source read/decode.
- Parsing and fact extraction.
- Import/binding resolution.
- Graph construction and reachability.
- Selected rule-family work.
- Cache validation, reads, and writes.
- Evidence generation and serialization.

Use release-mode Rust builds. Declare compiler version, build profile, allocator
if changed, target architecture, and any profile-guided optimization. Record
Python version and installed dependency versions for the reference and SDK.
Avoid adding overlapping cumulative phase/function timings together.

Report medians and useful tail/distribution information, with raw observations and
sample counts. Control competing work and thermal conditions; vary run order where
appropriate. Document outlier treatment before looking at the result. Include
failures, cancellations, and resource exhaustion in the report, not only completed
fast runs.

## Reproducibility manifest

The future harness must emit a machine-readable manifest containing:

| Area | Required data |
| --- | --- |
| Identity | Run ID, timestamp, tool source revision, artifact hash, schema version |
| Host | OS, CPU model/architecture, cores available, memory, storage, resource limits |
| Toolchain | Rust/Python versions, release flags, resolved dependencies |
| Input | Corpus ID/revision, source hashes, files/bytes/lines, relevant graph metrics |
| Policy | Full effective settings, selected rules, target Python, model versions |
| Execution | Exact commands or API calls, working directory, thread count, output mode |
| Cache | Analysis-cache preparation and OS-cache conditions |
| Result | Raw timings, memory/CPU, exit code, coverage, finding fingerprint, errors |
| Review | Known discrepancies, adjudicated labels, unresolved labels, reviewer notes |

No benchmark artifact should contain credentials or unnecessary source snippets.
Keep a manifest and compact results in Git; use an explicit distribution strategy
for large reproducible corpora instead of checking build artifacts into the repo.

## Accuracy and completeness accompany speed

Report required detections, false positives on reviewed live examples, missed
reviewed positives, and unknown labels. Do not count an unreviewed diagnostic as a
true positive merely because either tool emits it. Unsupported and incomplete
cases stay visible and unscored for accuracy where truth is not established.

A speed comparison is equivalent only when the intended contract and completion
conditions match. A reviewed improvement that changes findings may be valuable,
but it must be presented as a behavior change alongside its timing.

## Provisional targets

The roadmap proposes 5–10× lower wall time for matched Python dead-code scans,
approximately one second for a specified 100K-line cold-cache workload,
approximately 100 ms for its warm run, and sub-100 ms typical resident leaf edits.
None has been measured. Phase 0 must select a reference host and corpus, establish
variance and completeness, and revise unrealistic budgets openly.

CI performance thresholds should be introduced only after variance is understood.
Correctness and cache-equivalence gates apply immediately; noisy latency numbers
should not become arbitrary merge blockers.

## First harness tasks

- [ ] Freeze environment and corpus manifests.
- [ ] Capture matched Skylos Python and native-extension baselines.
- [ ] Implement fresh-process cold/warm runs and deterministic edits.
- [ ] Add completion and finding-equivalence checks before calculating speedups.
- [ ] Add phase timings without including profiler overhead in headline runs.
- [ ] Publish raw measurements and a small report showing limitations.

Related: [roadmap](../docs/roadmap.md),
[compatibility](../docs/compatibility.md), and
[conformance methodology](../tests/conformance/README.md).
