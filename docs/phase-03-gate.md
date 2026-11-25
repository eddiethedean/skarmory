# Phase 0.3 release gate

The [release workflow](../.github/workflows/release.yml) requires the
dead-code gate for versions 0.3.0 and later. The [initial inventory](../tests/conformance/phase03-inventory.toml)
is a reviewed subset of the pinned Skylos 5.7.0 suite. The complete case audit,
reference runs, and Skarmory ports remain release requirements. The reviewed
inventory assigns 1,353 cases to Phase 0.3 and defers six to later language
milestones. Of the required cases, 1,235 have port mappings. The reported release
decision requires receipts from the same candidate and complete inventory.
[Phase 0.3 acceptance criteria](phase-03.md#required-upstream-test-gate) ·
[Recorded seed reference outcomes](phase-03-reference.md)

## Inventory and receipts

Add one case per upstream pytest node, native subtest, or deterministic
benchmark case. Record its source, assertions, prerequisites, fixture
provenance, required variants, Skarmory port, languages, and owning milestone.
Record independently separable Python assertions in Phase 0.3. Assign
assertions that require other frontends to the first release with all
required languages. Each variant identifies
its platform, backend, test layer, environment, and candidate artifact role.
Keep `inventory_complete = false` until the entire pinned suite has been
audited and every required case and variant is represented.

The checker uses evidence from the release workflow's own candidate jobs.
Each JSON receipt covers one variant and identifies the pinned Skylos
revision, candidate revision, exact inventory, environment, and SHA-256 of the
artifact actually tested. A receipt records the reference and Skarmory
commands and their separate case outcomes. A Skarmory pass must account for
the listed assertions and verify every listed prerequisite. A reference
pass must account for the listed assertions. For a reviewed `xfail`, record
the expected failing assertion as `reference_xfail` in the inventory. The
receipt records the failure location in `reference_failed_assertion` and
the preceding completed assertions in `reference_assertions`. The candidate
must pass the original assertions with the upstream marker disabled.

Use `--fixture-root` to check declared fixture paths for required cases.
The gate checks fixture provenance, license, and SHA-256, and requires
the receipt to identify the same content. Deferred cases retain their
fixture paths and prerequisites in `deferred_cases`. The four TypeScript
benchmarks are deferred to 0.7 and still need the original sources for that
milestone. [Fixture issue #45](https://github.com/eddiethedean/skarmory/issues/45)

For a local check, use the same artifact file that the runner tested and
provide its receipt. Repeat `--artifact` and `--ledger` for additional
variant evidence.

```sh
python scripts/check_dead_code_gate.py \
  --inventory tests/conformance/phase03-inventory.toml \
  --candidate-revision "$(git rev-parse HEAD)" \
  --artifact "sdk=/path/to/candidate.whl" \
  --fixture-root /path/to/pinned-skylos \
  --ledger /path/to/sdk-receipt.json \
  --report gate.json
```

The report includes artifact hashes, receipt contents, and each required case
and variant's decision. Pass and blocker counts cover required Python cases.
The checker exits 1 if a required result is missing,
blocked, skipped, duplicated, stale, or unsuccessful, or if the inventory is
incomplete. The existing Phase 0.2 conformance fixtures and independent
controls continue to run through the packaging checks.

The release workflow installs the same-run Linux wheel in a clean SDK
environment and builds the native CLI and Rust library test executable from
the same candidate checkout. The [seed adapter](../tests/conformance/run_phase03_seed.py)
records separate SDK, CLI, and engine receipts. It runs the original upstream
assertions with their markers in the frozen Skylos environment. Candidate
pytest runs select `skylos-0.3` and disable upstream expected-failure markers.

The SDK shard runs the original framework corpus scorer against the selected
fixtures and records each original assertion, fixture hash, and outcome.
The framework corpus maps 29 of 42 cases, including DRF declarations and
FastMCP registration through project wrappers.
The framework audit now includes the FastAPI dependency-parameter tests and
all 40 nested-project route variants. Thirty-two route variants have SDK
ports. The remaining eight lack a resolved router. The audit also records
eight import-scope cases where a project-local package has no such submodule.
Those cases now replay the original declaration, activation and unused-route
assertions through the installed SDK. The import-root alias engine port stages
the original nested project and verifies both negative controls through the
native index. Two router graph ports run the original budget and cycle
assertions against the native traversal. The receipts preserve the original
edges, the uncalled-owner control, and the fixture-specific work limit.

The eight version-sensitive FastAPI variants use a separate live-router
receipt. The runner verifies the FastAPI 0.141.1 runtime wheels against a
dedicated lock and serves the original fixture before crediting the
installed SDK scan with the explicit live mode. The original pytest
assertions and PyProject remain unchanged. In the runtime cross-check,
FastAPI 0.136.0 returns HTTP 404 for both late routes, while 0.141.1 returns
HTTP 200. [FastAPI router inclusion release notes](https://fastapi.tiangolo.com/release-notes/#01370-2026-06-14)

The mapped Django SDK cases also verify assertions in the pinned shared
fixture helper. The configuration replacement suite has thirty ports exercising effective
Django settings protocols and ordered collection mutations.

| Django family | Mapped | Blocked |
| --- | ---: | ---: |
| URL function callbacks and view providers | 43 | 8 |
| Path converter registration and protocol | 59 | 1 |
| Authentication backend selection and hooks | 69 | 2 |
| Named settings imports and overrides | 26 | 2 |
| Model, command, and form hook provenance | 37 | 10 |
| Management command reachability | 24 | 12 |
| Python template liveness | 48 | 1 |
| Form hook evidence after source edits | 2 | 0 |
| Autoreload callback selection | 29 | 3 |
| Admin stub context | 5 | 2 |

The Django hook cache ports reuse a cache across the original edits. The separate Django
converter probes for Skylos's private parser and index hooks exercise
implementation call counts outside the native report contract. The broader
framework inventory remains under review.

The logging configuration suite maps 42 of 43 cases. The Flask callback
provider suite maps 38 of 58 cases. Proven provider initialization failures
stop later Flask registration, while the selected importers still report
incomplete coverage.

The SDK shard also executes the pinned basic dead-code benchmark with
the original scorer and staged fixtures. Both score reports are included in
the receipt. Credit requires complete analysis, the labeled detections and live
controls, the original timing limit, and verified candidate scan options.

For SDK cases, the adapter forwards upstream scan options, checks the
candidate interpreter version and architecture, and compares the installed
package with the wheel. Runtime-evidence cases verify import, explicit
disablement, or rejection as appropriate. For the recursive CLI case, the
adapter records the binary hash and verifies complete scans of both fixture
states with the default grep verification. The CLI shard also runs the
landed Git impact and real-scan JSON-CI cases using the original fixtures
and options. It reads the native CLI's stdout and report files or verifies
the CLI error for invalid impact invocations. The adapters require the
original assertion lines to execute.

The cleanup CLI ports perform live previews, edits, and rescans. They run
authorized checks in subprocesses, including the pinned configuration and
source mutations. The engine receipt covers the late-removal case, which
injects its mutation after report comparison. It stages the pinned fixture,
runs real pytest and Skarmory analysis, and requires exactly the named
Rust test to pass. The receipt identifies the test executable and the
fixture bytes.

For versions below 0.3, matching pull requests and manual workflow runs
require the mapped SDK, CLI, and engine cases to pass and upload the full gate decision.
The full decision remains blocked while required evidence is missing.
Publication of 0.3 and later requires that full decision to pass.

Run the checker regression tests with the following command.

```sh
python -m unittest discover -s tests/conformance -v
```
