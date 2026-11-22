# Phase 0.3 release gate

The [release workflow](../.github/workflows/release.yml) requires the
dead-code gate for versions 0.3.0 and later. The [initial inventory](../tests/conformance/phase03-inventory.toml)
is a reviewed subset of the pinned Skylos 5.7.0 suite. The complete case audit,
reference runs, and Skarmory ports remain release requirements. The inventory
records four blocked TypeScript cases whose fixtures are missing from the
pinned checkout. [Phase 0.3 acceptance criteria](phase-03.md#required-upstream-test-gate) ·
[Recorded seed reference outcomes](phase-03-reference.md)

## Inventory and receipts

Add one case per upstream pytest node, native subtest, or deterministic
benchmark case. Record its source, assertions, prerequisites, fixture
provenance, required variants, and Skarmory port. For mixed cases, record which
assertions were omitted and why they are independent. Each variant identifies
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

For a local check, use the same artifact file that the runner tested and
provide its receipt. Repeat `--artifact` and `--ledger` for additional
variant evidence.

```sh
python scripts/check_dead_code_gate.py \
  --inventory tests/conformance/phase03-inventory.toml \
  --candidate-revision "$(git rev-parse HEAD)" \
  --artifact "sdk=/path/to/candidate.whl" \
  --ledger /path/to/sdk-receipt.json \
  --report gate.json
```

The report includes artifact hashes, receipt contents, and each required case
and variant's decision. The checker exits 1 if a required result is missing,
blocked, skipped, duplicated, stale, or unsuccessful, or if the inventory is
incomplete. The existing Phase 0.2 conformance fixtures and independent
controls continue to run through the packaging checks.

The release workflow produces the Linux CPython 3.11 SDK receipt by installing
the same-run wheel into a clean environment and running the
[seed adapter](../tests/conformance/run_phase03_seed.py). The adapter executes
the original upstream pytest cases with their markers in the frozen Skylos
environment. For the candidate run it selects `skylos-0.3` in the installed
Skarmory SDK, forwards the upstream scan options, and disables upstream
expected-failure markers. It checks that the wheel's files match the installed
package and that the original assertion lines execute. The seed SDK runner
currently covers cases that explicitly disable tracing.

Run the checker regression tests with the following command.

```sh
python -m unittest discover -s tests/conformance -v
```
