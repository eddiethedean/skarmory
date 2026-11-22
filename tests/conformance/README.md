# Conformance and semantic validation plan

The [Phase 0 manifest](phase0.toml) declares independent expectations for the
prototype. Run `python tests/conformance/check_phase0.py target/debug/skarmory`
after building the CLI. The [reference harness](../../benchmarks/phase0.md)
records the pinned Skylos comparison alongside those expectations.

The [Phase 0.3 release gate](../../docs/phase-03.md#required-upstream-test-gate)
requires every pinned Skylos dead-code test to pass against Skarmory. Preserve
upstream expectations and publish per-case evidence alongside the independent
semantic controls described here.

## Two separate questions

**Compatibility:** does Skarmory reproduce the declared observable behavior of
the pinned Skylos version?

**Correctness:** does the result agree with independently reviewed source facts
under the stated assumptions?

Matching upstream output is useful evidence for the first question. It is not an
independent answer to the second. Keep reference snapshots and reviewed truth as
separate data.

## Fixture contract

Each future fixture should include:

- A stable ID, short purpose, and supported feature/rule contract.
- Minimal source files with explicit source roots and target Python version.
- Configuration and any required package/framework models.
- Expected findings and locations, required retained symbols, and expected
  uncertain or incomplete outcomes.
- Root and call/reference evidence supporting the reviewed expectation.
- A pinned reference result and environment identity, when testing compatibility.
- A reviewer explanation for each intentional upstream difference.

Use category/rule, file, symbol, and source range on the same assertion. Equal
finding counts alone are insufficient. A nominally clean fixture must also assert
the expected analysis coverage and successful completion.

The Phase 0 fixture schema covers module-scoped function findings and incomplete
coverage. The broader fixture families below are planned.

## Paired semantic cases

Each positive detector example should have a close negative or uncertainty control
where practical:

| Area | Required pair or family |
| --- | --- |
| Unreachable recursion | Isolated mutually recursive functions; same group reached from a real root |
| Symbol identity | Same simple name in different modules/scopes; alias referring to only one |
| Imports | Direct use, unused binding, re-export chain, star import, and shadowed module |
| Public APIs | Exported library function without local calls; internal application helper |
| Callbacks | Function handed to an active callback consumer; same registration in dead code |
| Async | Scheduled coroutine/callback, retained function value, and unreachable scheduling site |
| Multiprocessing | Process target and pool/executor worker; similarly named unrelated function |
| Framework routes | Active mounted route; inactive router; unrelated decorator with the same spelling |
| Models and validators | Real framework registration; ordinary method with a matching name |
| Tests | Discovered test/fixture/hook; ordinary matching name; test-only production helper |
| Types | Runtime use, type-only use, exported typing API, and runtime annotation evaluation |
| Dynamic lookup | Literal known target, finite target set, and opaque target |
| Plugins/config | Declared entry point or data-held callback; dormant/unloaded registration |
| Nested scopes | Captured binding, closure escape, nonlocal/global use, and shadowing |
| Definition effects | Unused callable with decorator/default effects; effect-free counterpart |
| Parsing | Valid supported syntax; invalid syntax; unsupported target; unreadable selected source |
| Limits | Completed analysis; bounded/cancelled work with explicit incomplete status |

Framework tests must exercise identity and activation, not just spellings.
Supported string/config references should have a defined interpretation; comments
and unrelated text must not silently become proven callers.

## Configuration and presentation cases

- Nested configuration discovery and inheritance boundaries.
- Competing config filenames and explicit config selection.
- Lists versus nested-table merging; path-relative selectors.
- Excludes, generated-file policy, inline ignores, rule ignores, and entry points.
- Documented and expiring whitelists using a deterministic clock.
- Confidence boundaries immediately below, at, and above the threshold.
- Category-specific threshold and reporting behavior.
- CLI report/gate/force modes, invalid invocations, operational failure, and
  interrupted work.
- Machine-readable stdout with diagnostics/progress on the documented channel.
- JSON/SARIF escaping, non-ASCII paths and identifiers, CRLF sources, location
  conventions, and meaningful missing/null values.
- Native typed API versus legacy JSON-string adapter behavior.
- Unsupported legacy options and Python visitor integrations failing explicitly.

Only normalize documented non-semantic fields such as temporary root directories
and timings. Do not normalize away evidence, locations, confidence, completion,
exit behavior, or finding differences merely to make snapshots agree.

## Cache and concurrency invariants

For the same source snapshot and configuration:

> Clean, cached, single-worker, and multi-worker runs must produce equivalent
> normalized findings, evidence, and coverage status.

Test that invariant after:

- A local body edit.
- Removing the last reachable call to an unchanged function.
- Removing a root to an entire recursive group.
- Changing an export, re-export, or `__all__` declaration.
- Adding a previously missing module or a module that shadows an external name.
- File deletion, renaming, package-root change, and supported symlink behavior.
- Changing excludes, source roots, entry points, target syntax, or model versions.
- Changing trace evidence or its source fingerprint.
- A failed parse becoming valid, and a valid source becoming unreadable/invalid.
- Concurrent scan requests and cancellation of a superseded resident generation.

Discovery order and worker completion order must not affect root selection,
confidence, stable finding identity, or output ordering. Tests must not rely on
wall-clock sleeps to create thread ordering.

Cache correctness includes the absence of stale proof. A scan with missing source
must not inherit a previous complete-clean verdict from a valid snapshot.

## Property and metamorphic checks

Introduce properties where they test semantics rather than repeat implementation:

- Adding a valid root makes its supported reachable component live.
- Removing the final live edge can expose an unreachable group.
- Harmless renaming of a private lexical binding preserves unrelated findings,
  provided the fixture has no name-sensitive registration or dynamic use.
- Reordering independent declarations preserves supported semantic conclusions.
- Changing worker count or traversal order preserves normalized results.
- Rebuilding from the edited source equals incrementally updating the old source.
- Parse failures, resource limits, and cancellation cannot produce a completed
  clean report for the affected analysis.

Fuzz parsing/adaptation, source decoding, config handling, and graph boundaries.
Resource limits are part of the contract; maliciously shaped or simply enormous
input must produce an explicit bounded outcome rather than uncontrolled growth.

## Real repositories and holdout review

Use pinned libraries and applications with different framework, export, plugin,
and concurrency patterns. Freeze reviewed source facts and settings before running
the holdout evaluation. Keep cases used to tune models separate from the holdout.

For every adjudicated symbol, record source evidence and analysis assumptions.
Unknown truth stays unknown. Do not classify every disagreement with Skylos as a
Skarmory error or every extra finding as a true positive.

Skylos's own benchmark methodology distinguishes regressions from independent
accuracy evidence. A passing regression suite should be reported with that scope.
[Source](https://github.com/duriantaco/skylos/blob/87978e69adf91b3811e97b00c40aa39d7937cd94/BENCHMARK.md).

## Difference review and release gates

For each mismatch, retain the fixture/source revision, reference environment,
effective settings, actual results, reviewed truth if known, and resolution.
Classify it as a Skarmory defect, upstream defect, intentional improvement,
unsupported capability, or unresolved source truth. Never bulk-refresh snapshots
without inspecting why behavior changed.

Before claiming a compatibility surface:

1. Every mandatory contract fixture passes or has a published intentional
   exception to exact parity.
2. No unexplained behavioral difference remains.
3. The must-retain set has no new false positives and required positive cases
   retain their detections.
4. Cache/concurrency invariants hold for the supported scope.
5. Coverage, limits, and unsupported cases are visible in user-facing results.
6. [Performance comparisons](../../benchmarks/README.md) use the same tested
   contract and completion conditions.

## Initial implementation tasks

- [ ] Define versioned fixture and reference-environment schemas.
- [ ] Add a small reviewed set covering symbols, roots, recursion, exports,
  callbacks, frameworks, uncertainty, and incomplete parsing.
- [ ] Implement a differential runner with explicit supported-scope selection.
- [ ] Implement a structural result comparator with narrow normalization rules.
- [ ] Add a reviewed discrepancy manifest.
- [ ] Add clean/cached/worker-count equivalence checks when those capabilities exist.
- [ ] Freeze the first external holdout separately from development fixtures.

Related: [roadmap](../../docs/roadmap.md) and
[compatibility contract](../../docs/compatibility.md).
