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
  test/test_trace_file_analyzer.py \
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
| `test_analyzer_trace_file_loads_only_explicit_path` | Passed |
| `test_analyzer_trace_file_false_ignores_project_root_trace` | Passed |
| `test_analyzer_default_trace_file_preserves_legacy_root_trace` | Passed |
| `test_analyzer_default_trace_file_rejects_symlink` | Passed with symlink creation available. |
| `TestHardBenchmark::test_all_dead_detected` | Skipped in setup because `hard_benchmark.ts` was absent |
| `TestHardBenchmark::test_no_false_positives` | Skipped in setup because `hard_benchmark.ts` was absent |
| `TestRealisticBenchmark::test_all_dead_detected` | Skipped in setup because `realistic_benchmark.ts` was absent |
| `TestRealisticBenchmark::test_no_false_positives` | Skipped in setup because `realistic_benchmark.ts` was absent |
| `test_npm_usage_reports_findings_and_rule_coverage` | Passed, including the combined `SKY-U005`, `SKY-Q408`, and `SKY-Q409` assertion |
| `TestFunctionReachabilityUsesObjectsAndLiveOwners/unrooted_recursion_and_mutual_calls` | Go JSON event `pass` at the subtest level |
| `benchmarks/dead_code/manifest.json::basic-unused-symbols` | Complete and scored. Five unused detections and three live controls matched. Default label mode, zero failures. |

The four missing-fixture cases and the npm case belong to Phase 0.7.
The npm case uses a TypeScript-only fixture and retains all three of its
assertions. The Go subtest belongs to Phase 0.9.
[Phase 0.7 issue #45](https://github.com/eddiethedean/skarmory/issues/45) ·
[Phase 0.9 issue #131](https://github.com/eddiethedean/skarmory/issues/131)
See the [upstream test gate](phase-03.md#required-upstream-test-gate) for the
remaining inventory and release evidence requirements.

## Python entry point and export results

The added SDK cases ran on macOS 26.6.2 (25G83), ARM64, with CPython
3.11.14, uv 0.11.3, and pytest 9.0.3 for the reference. The reference
uses the same pinned revision and lock digest above. The candidate was
installed from the locally built macOS ARM64 wheel under CPython 3.11.9.
The macOS run exercises the ports locally. The inventory requires the
Linux x86-64 release wheel for gate credit.

From the pinned checkout, using the frozen reference environment above:

```sh
PYTEST_DISABLE_PLUGIN_AUTOLOAD=1 "$reference_python" -m pytest -p no:cacheprovider -q -rA \
  test/test_reachability_integration_qa.py::test_external_entry_and_callback_roots_keep_transitive_helpers_live \
  test/test_dead_code_liveness.py::test_plugin_entry_point_groups_are_live \
  test/test_dead_code_liveness.py::test_setup_py_and_setup_cfg_entry_points_are_read_without_running_setup \
  test/test_analyzer.py::TestAnalyze::test_pyproject_gui_script_entrypoint_is_not_reported_dead
```

| Original SDK case | Pinned Skylos | Local Skarmory |
| --- | --- | --- |
| `test_external_entry_and_callback_roots_keep_transitive_helpers_live[callback-False]` | Passed | Passed |
| `test_external_entry_and_callback_roots_keep_transitive_helpers_live[callback-True]` | Passed | Passed |
| `test_external_entry_and_callback_roots_keep_transitive_helpers_live[export-False]` | Passed | Passed |
| `test_external_entry_and_callback_roots_keep_transitive_helpers_live[export-True]` | Passed | Passed |
| `test_external_entry_and_callback_roots_keep_transitive_helpers_live[entrypoint-False]` | Passed | Passed |
| `test_external_entry_and_callback_roots_keep_transitive_helpers_live[entrypoint-True]` | Passed | Passed |
| `test_plugin_entry_point_groups_are_live` | Passed | Passed |
| `test_setup_py_and_setup_cfg_entry_points_are_read_without_running_setup` | Passed | Passed |
| `TestAnalyze::test_pyproject_gui_script_entrypoint_is_not_reported_dead` | Passed | Passed |

The port preserves the original fixture creation and assertions, including the
marker check that verifies `setup.py` does not execute. It verifies the
effective grep setting in each case and checks trace disablement where the
original test explicitly requests it. The dormant `eval(source)` case and both
shutdown callback cases pass against the installed candidate SDK. [Callback coverage issue #118](https://github.com/eddiethedean/skarmory/issues/118)


## Python benchmark candidate

The installed-SDK shard runs the pinned `basic-unused-symbols` manifest case
through Skylos's original scorer with default label mode. It checks the staged
fixture bytes against the tracked tree, forwards confidence zero and enabled
grep verification, and records the original score report from each analyzer.
Skarmory completes the scan and matches all five unused and three live labels
within the benchmark's five-second limit. The scorer records one additional
unlabeled function finding from Skarmory, which default label mode permits.
A `math.sqrt` call that can invoke a custom numeric conversion still reports
incomplete coverage.

## Python workflow CLI cases

The CLI shard includes ten pinned Python impact invocations and the three
real-scan JSON-CI cases. The original pytest assertions exercise the native
CLI, including output files and SARIF, incomplete base and head comparisons,
and rejected impact options. The adapter removes the upstream upload and
provenance switches for those local runs. The case IDs and original assertion
locations are in the [inventory](../tests/conformance/phase03-inventory.toml).

The pinned reference and candidate passed all 14 CLI cases in a local macOS
ARM64 CPython 3.11 run, including the original recursive reachability case.
The release workflow requires its separate Linux receipts. The inventory audit
and remaining ports still govern the full publication gate.
