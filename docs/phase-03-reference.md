# Phase 0.3 seed reference results

These reference results cover the reviewed [seed inventory](../tests/conformance/phase03-inventory.toml).
They record original Skylos outcomes. The Skarmory release gate evaluates separate
candidate results against its declared variants. The inventory and reference
matrix are still incomplete.

## Pinned source and environment

- Commit `87978e69adf91b3811e97b00c40aa39d7937cd94` from the Skylos
  repository, using the unchanged test source.
- macOS 26.7.3 (25G342), ARM64, CPython 3.11.15, uv 0.11.3, pytest 9.0.3.
  Dependencies came from the upstream `uv.lock` with SHA-256
  `21dd281b15e9dc0cc872a5e7083bcf16acca0e8ae67e6a9bcb11c416efcbac41`.
  The Python backend ran without `skylos_fast`.
- Go 1.22.12 with the bundled standard library. The official
  `go1.22.12.darwin-arm64.tar.gz` SHA-256 is
  `416c35218edb9d20990b5d8fc87be655d8b39926f15524ea35c66ee70273050d`.

From the pinned Skylos checkout, run the Python commands below. The test and Dart
extras come from the lock. `--no-install-project` runs Skylos from this checkout.

```sh
export UV_PROJECT_ENVIRONMENT=/tmp/skarmory-issue41-reference-venv
uv sync --frozen --extra test --extra dart --python 3.11 --no-install-project
reference_python="$UV_PROJECT_ENVIRONMENT/bin/python"

"$reference_python" -m pytest --collect-only -q test/
"$reference_python" -m pytest -q -rA \
  'test/test_package_local_exports.py::test_package_dynamic_local_namespace_stays_live[locals()]' \
  'test/test_package_local_exports.py::test_package_dynamic_local_namespace_stays_live[vars()]' \
  'test/test_package_local_exports.py::test_package_dynamic_local_namespace_stays_live[eval(source)]' \
  test/test_package_local_exports.py::test_module_all_does_not_export_a_function_local_with_the_same_name \
  test/test_typescript_expanded.py::TestHardBenchmark \
  test/test_typescript_expanded.py::TestRealisticBenchmark

"$reference_python" -m pytest -q -rA \
  test/test_reachability_integration_qa.py::test_real_cli_reports_dead_cycle_and_keeps_live_variant \
  test/test_closure_reachability.py::test_creating_a_nested_function_does_not_execute_its_body

"$reference_python" -m pytest -q -rA \
  test/test_project_gap_integration.py::test_npm_usage_reports_findings_and_rule_coverage

"$reference_python" scripts/dead_code_benchmark.py \
  --manifest benchmarks/dead_code/manifest.json --case basic-unused-symbols --json

(cd skylos/engines/go && go test -json -count=1 ./internal/symbols \
  -run '^TestFunctionReachabilityUsesObjectsAndLiveOwners$/^unrooted_recursion_and_mutual_calls$')
```

Use CPython 3.11.15 and Go 1.22.12 to match the recorded run. Go 1.22.12
was on `PATH` for the CLI and closure cases. This seed reference matrix
uses the upstream lock and supported optional Dart grammar.
The `skylos_fast` backend and required platform variants need separate runs
where the original assertions require them.

## Observed upstream outcomes

The full `test/` collection found 41,082 items in this environment, including
cases outside dead code. That count does not classify cases and omits
suites outside that collection root.

| Pinned case | Original result |
| --- | --- |
| `test_creating_a_nested_function_does_not_execute_its_body[none-False]` | Passed |
| `test_creating_a_nested_function_does_not_execute_its_body[none-True]` | Passed |
| `test_creating_a_nested_function_does_not_execute_its_body[outer-False]` | Passed |
| `test_creating_a_nested_function_does_not_execute_its_body[outer-True]` | Passed |
| `test_creating_a_nested_function_does_not_execute_its_body[inner-False]` | Passed |
| `test_creating_a_nested_function_does_not_execute_its_body[inner-True]` | Passed |
| `test_real_cli_reports_dead_cycle_and_keeps_live_variant` | Passed |
| `test_package_dynamic_local_namespace_stays_live[locals()]` | Passed |
| `test_package_dynamic_local_namespace_stays_live[vars()]` | Passed |
| `test_package_dynamic_local_namespace_stays_live[eval(source)]` | Passed |
| `test_module_all_does_not_export_a_function_local_with_the_same_name` | Strict xfail. The upstream marker names explicit `__all__` local-name collisions. |
| `TestHardBenchmark::test_all_dead_detected` | Skipped in setup because `hard_benchmark.ts` was absent |
| `TestHardBenchmark::test_no_false_positives` | Skipped in setup because `hard_benchmark.ts` was absent |
| `TestRealisticBenchmark::test_all_dead_detected` | Skipped in setup because `realistic_benchmark.ts` was absent |
| `TestRealisticBenchmark::test_no_false_positives` | Skipped in setup because `realistic_benchmark.ts` was absent |
| `test_npm_usage_reports_findings_and_rule_coverage` | Passed, including the combined `SKY-U005`, `SKY-Q408`, and `SKY-Q409` assertion |
| `TestFunctionReachabilityUsesObjectsAndLiveOwners/unrooted_recursion_and_mutual_calls` | Go JSON event `pass` at the subtest level |
| `benchmarks/dead_code/manifest.json::basic-unused-symbols` | Complete and scored. Five unused detections and three live controls matched. Default label mode, zero failures. |

The missing-fixture cases remain blocked under [#45](https://github.com/eddiethedean/skarmory/issues/45).
The mixed npm case retains its combined assertion and both coverage checks.
See the [upstream test gate](phase-03.md#required-upstream-test-gate) for the
remaining inventory and release evidence requirements.
