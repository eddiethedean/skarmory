# Skylos compatibility plan

Status: specification. See the [Phase 0 inventory](skylos-5.7.0-contract.md) and
[prototype comparisons](../benchmarks/phase0.md) for recorded evidence.

## Frozen reference

| Field | Initial reference |
| --- | --- |
| Project | [Skylos](https://github.com/duriantaco/skylos) |
| Release | [5.7.0](https://github.com/duriantaco/skylos/releases/tag/v5.7.0) |
| Source revision | `87978e69adf91b3811e97b00c40aa39d7937cd94` |
| Release date | November 19, 2026 |
| Initial scope | Explicitly inventoried local static Python dead-code behavior |
| Later scope | Declared quality, secrets, security, and workflow contracts |

The source revision is the reference for reproducible tests. Website documentation
can describe newer or unreleased features; disagreements must be resolved through
the pinned implementation and reviewed fixtures. The [baseline manifest](../tests/conformance/baselines/skylos-5.7.0.toml)
records the artifact, interpreter, dependencies, platform, settings, and backends.

Skylos already has an experimental native extension. Its documented accelerated
operations are discovery, clone similarity, class coupling, and import cycles;
its AST visitor is not wired into the main analyzer, and some native results can
differ from Python results. Treat the backend as part of the baseline identity.
[Source](https://github.com/duriantaco/skylos/blob/87978e69adf91b3811e97b00c40aa39d7937cd94/rust/README.md).

## Meaning of compatibility

Compatibility is an observable contract for a named reference version, feature
set, configuration, source snapshot, and environment. It is not similarity of a
few screenshots or an equal count of findings.

The conformance matrix must include the following surfaces:

| Surface | Contract to capture | Planned milestone |
| --- | --- | --- |
| Source discovery | Paths, exclusions, generated files, package roots, encoding and failure behavior | Phase 0 / 0.2 |
| Dead-code findings | Categories, resolved symbol identity, locations, dispositions, evidence | 0.2 |
| Confidence | Scoring order, caps, thresholds, category-specific visibility | 0.2 |
| Framework use | Identity, registration, activation, lifecycle and callback contracts | 0.2 |
| Configuration | Discovery, inheritance, explicit configuration, environment inputs, precedence | 0.2 |
| Suppressions | Rule ignores, inline directives, whitelists, path overrides, custom entry points | 0.2 |
| Python API | Supported arguments, return type, validation, errors, callbacks | 0.2, with explicit gaps |
| Machine output | JSON field semantics, SARIF, absence/null distinctions, escaping and locations | 0.2 |
| CLI | Supported invocations, stdout/stderr separation, command-specific exit behavior | 0.2 |
| Quality and secrets | Declared rules, file universe, severity and evidence | 0.3 |
| Security | Declared rules, source/sink/sanitizer models, propagation and limits | 0.4 |
| Diff and CI gates | Reporting scope, baseline policy, gate outcome and operational errors | 0.5 |
| Runtime evidence and cleanup | Trace import, optional execution, edit applicability and failure reporting | 0.5 |
| Other languages and external services | Separate feature and environment contracts | Future scope |

Until a row has an implementation and evidence, its state is **planned**. A
feature matrix in a release must distinguish supported, experimental,
unsupported, and unavailable-in-this-run behavior.

## Compatibility adapters and native interfaces

Skarmory owns its stable native configuration, result objects, and analysis
representation. An adapter translates supported Skylos options and output shapes
without maintaining a second analysis implementation in Python.

The default SDK wheel and native CLI are separate artifacts. The prototype API
and CLI expose the same native report and describe incomplete work in the
coverage field. See the [Phase 0 contract](phase-0.md) for exit behavior and
packaging evidence.

The pinned `skylos.analyze(...)` API takes `conf=60` by default and returns a JSON
string. A compatibility adapter must preserve that string return contract; a
separate native API may return typed report objects.
[Source](https://github.com/duriantaco/skylos/blob/87978e69adf91b3811e97b00c40aa39d7937cd94/skylos/analyzer.py).

Required adapter rules:

- Preserve legacy rule identifiers where promised; do not relabel new rules as
  existing Skylos rules with different semantics.
- Preserve meaningful missing, empty, null, and false values.
- Translate confidence and severity independently.
- Keep findings, retained candidates, abstentions, suppressions, analysis errors,
  coverage, and gate outcomes distinct.
- Reject unsupported arguments and configuration keys clearly. Never accept an
  option that is silently ineffective.
- Treat arbitrary Python AST visitors as a separate integration contract. They
  are not native plugins and do not inherit the native latency target.
- Define progress callback frequency, exceptions, cancellation, and interpreter
  interaction before claiming callback compatibility.
- Keep user-facing package and command names `skarmory`; importing or installing
  Skarmory does not replace the `skylos` package or its executable.

Phase 0 exposes a single native profile. Selection syntax for a legacy profile
is deferred until it has an implemented contract.

## Configuration is observable behavior

Capture the full resolution algorithm, including ancestor traversal, stopping
boundaries, per-directory file precedence, list replacement, nested-table merge,
explicit config paths, environment overrides, and path-relative matching.
Configuration differences can alter the analyzed source universe and graph roots,
so they belong in both the compatibility corpus and cache fingerprints.

Exercise at least:

- `.skylos.toml`, `skylos.toml`, and `[tool.skylos]` in `pyproject.toml`.
- Multiple nested configurations and an explicit inheritance boundary.
- Explicit configuration and environment selection.
- Inline and rule-specific ignores versus broad name exclusions.
- Documented and expiring whitelist entries with a fixed test clock.
- Entry-point selectors combining symbol, path, decorator, base, and parent facts.
- Native and legacy config files present together, with deliberate selection
  rather than an undocumented merge.

The [Skylos configuration reference](https://docs.skylos.dev/configuration) is a
guide to inventory; tests must bind behavior to the frozen source revision.

## Semantic boundaries

### Roots and references

References are graph edges. A group of functions referencing one another is not
automatically reachable. Distinguish application roots, external library API,
framework entry points, test use, data/config references, and positive runtime
evidence. Keep symbol identity scoped through modules, lexical bindings, and
owners.

The source implementation already has conservative reachability and uncertain-use
handling. The Rust plan preserves those protections rather than reducing the
reference tool to a global name counter.
[Source](https://github.com/duriantaco/skylos/blob/87978e69adf91b3811e97b00c40aa39d7937cd94/skylos/deadcode/reachability.py).

### Framework activation and callbacks

Model known framework identities and activation. A decorator's spelling is
insufficient; a registering module or router can itself be inactive. Callback
values, closures, async scheduling, process targets, executor submissions, and
plugin exports can create use without a direct call at the definition site.

### Incomplete and uncertain analysis

Maintain separate facts for unsupported syntax, parse/read errors, missing
source, unresolved dynamic use, analysis limits, and cancellation. Preserve any
valid local findings, but do not turn incomplete project analysis into a negative
reachability proof or a complete clean result. In strict compatibility scope,
match the reference's abstention behavior. Improvements to limits or partial
proofs require an explicitly reviewed policy difference.

### Tests, typing, and runtime evidence

Retain edge kinds for production execution, tests, type references, exports, and
possible dynamic calls. Test-only use and type-only use are not interchangeable
with ordinary runtime reachability. Runtime observations establish observed use;
absence from a trace does not establish non-use. Trace provenance and freshness
must be represented before using recorded evidence to suppress a finding.

### Findings and edits

An unused binding may still have import, decorator, default-argument, class-body,
or registration effects. Finding compatibility does not establish safe deletion.
Cleanup needs its own versioned transformation contract and explicit failure
reporting.

## Output and exit behavior

Normalize only fields that are intentionally non-semantic, such as temporary
root paths and elapsed time. Compare rule/category, symbol identity, location,
confidence policy, severity, disposition, evidence, coverage, and exit outcome.
Maintain byte-level or structural format fixtures where exact serialization is
part of the declared interface.

Do not assign one universal exit rule to every Skylos command. Capture command
and mode behavior, including report-only modes, gates, forced success behavior,
invalid invocation, unavailable analysis, interrupted work, and output failure.
Machine output must remain parseable when progress or warnings are emitted.

Diff filtering is an output decision over sufficiently contextual analysis.
Changed-line reports and impact reports for unchanged affected code are distinct
products; the adapter must reproduce whichever contract is selected.

## Difference policy

Every disagreement receives a reviewed record containing the reference identity,
fixture or repository revision, settings, expected and actual evidence, source
truth where known, classification, owner, and intended resolution.

| Classification | Treatment |
| --- | --- |
| Skarmory defect | Fix before claiming the affected compatibility surface |
| Upstream defect | Preserve the repro and source evidence; make any divergence explicit |
| Intentional improvement | Document the native behavior and compatibility implications |
| Unsupported capability | Report the gap; exclude it from parity and equivalent-speed claims |
| Unknown ground truth | Leave accuracy unscored pending review; still record behavioral difference |

Do not blindly preserve upstream bugs in the native policy. Do not silently
change the contract of a strict compatibility profile. Keep a documented
distinction where those goals diverge.

## Acceptance gates

For each declared surface:

1. All mandatory conformance fixtures pass, or each intentional difference is
   approved and published as an exception to exact compatibility.
2. No unexplained behavioral differences remain.
3. Independent live/dead controls show no new false positives in the must-retain
   set and no loss of required detections.
4. Cached, clean, single-worker, and multi-worker runs agree on normalized output
   and completion status.
5. Benchmarks use matched supported work and report any incomplete run separately.
6. User documentation names the unsupported and experimental cases.

See [conformance methodology](../tests/conformance/README.md),
[benchmark methodology](../benchmarks/README.md), and the [roadmap](roadmap.md).
