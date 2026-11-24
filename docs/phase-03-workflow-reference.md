# Phase 0.3 workflow assertion mapping

The reference is Skylos 5.7.0 at
`87978e69adf91b3811e97b00c40aa39d7937cd94`. Git and Python analyzer assertions
use the real native CLI and committed fixture sources. Rule, severity, gate
and JSON-CI policy assertions exercise the same report and policy code used by
the CLI and SDK. The mapping supplies ports for the
[#41 release inventory](https://github.com/eddiethedean/skarmory/issues/41).

| Pinned cases | Skarmory path under test | Regression coverage |
| --- | --- | --- |
| `test_pr_impact.py::test_removed_caller_is_introduced_outside_changed_lines`, `test_line_shift_does_not_reintroduce_existing_dead_code`, `test_subdirectory_qualified_identity_is_stable`, `test_new_scan_directory_has_an_empty_baseline` | `workflow::scan_impact`, scoped identities, and changed-line projection over native findings | `impact_flags_removed_last_caller_outside_changed_lines`, `line_shift_and_base_policy_are_stable_and_operator_config_applies_to_both`, `impact_of_a_new_project_directory_has_an_empty_base` |
| `test_pr_impact.py::test_incomplete_base_blocks_instead_of_claiming_all_findings_new`, `test_incomplete_head_caller_does_not_claim_dependency_impact` | Real parser, snapshot coverage and impact receipt | `impact_identifies_incomplete_base_head_and_both` |
| `test_pr_impact.py::test_explicit_operator_config_applies_to_baseline_file_processing`, `test_nongated_impact_uses_the_same_base_policy_for_head_and_baseline` | Shared base configuration lookup and explicit operator snapshot | `line_shift_and_base_policy_are_stable_and_operator_config_applies_to_both` |
| `test_pr_impact.py::test_snapshot_reads_raw_blobs_without_filters_or_target_execution`, `test_impact_rejects_missing_diff_or_partial_head` | Raw Git materialization and CLI option validation | `snapshots_use_raw_blobs_without_invoking_clean_or_smudge_filters`, `impact_requires_the_full_head_comparison_mode` |
| `test_pr_impact.py::test_oversized_non_source_blob_is_left_out_of_the_baseline` | Omission receipt for oversized binary assets in the Git base | `impact_omits_oversized_assets_outside_analysis_scope`, with incomplete-coverage controls for oversized analysis inputs |
| `test_diff_working_tree.py::test_working_tree_ranges_cover_committed_staged_unstaged_and_untracked`, `test_diff_head_sees_only_uncommitted_work` | Git merge base, current source bytes and Git-visible paths | `changed_lines_use_full_project_and_working_tree_sources` |
| `test_coverage_gaps.py::test_gate_is_decided_on_what_was_analysed`, `test_gate_with_strict_or_required_coverage_is_incomplete`, `test_check_gate_passes_on_partial_coverage_and_blocks_with_strict_config`, `test_gate_record_states_coverage` | `workflow::Assessment` and `evaluate_gate`, including thresholds, unchecked scope, warnings and verdict | `gate_coverage_is_independent_of_the_verdict_and_display_filters`. The critical security-finding assertion in the mixed upstream case requires the security rules. |
| `test_cli.py::test_apply_rule_selection_uses_public_ids_for_dead_code_without_rule_ids`, `test_apply_display_filters_severity_filters_without_crashing` | Real report rule IDs and display severity. Severity projection occurs after the gate. | `json_ci_preserves_findings_evidence_coverage_and_sarif` and `gate_coverage_is_independent_of_the_verdict_and_display_filters`. The mixed upstream severity case also tests synthetic quality findings at different severities. |
| `test_cli_json_ci_format.py::test_omits_top_level_bulk`, `test_preserves_summary_evidence_counts`, `test_preserves_findings`, `test_preserves_per_finding_evidence`, `test_does_not_mutate_input`, `test_preserves_sibling_summary_counts`, and the three real-scan cases | `Report::to_json_ci`, full and compact representations of the same report, and separate SARIF output | `json_ci_preserves_findings_evidence_coverage_and_sarif` and the installed SDK/CLI parity control |

[Impact cases](https://github.com/duriantaco/skylos/blob/87978e69adf91b3811e97b00c40aa39d7937cd94/test/test_pr_impact.py) · [Working-tree diff cases](https://github.com/duriantaco/skylos/blob/87978e69adf91b3811e97b00c40aa39d7937cd94/test/test_diff_working_tree.py) · [Coverage cases](https://github.com/duriantaco/skylos/blob/87978e69adf91b3811e97b00c40aa39d7937cd94/test/test_coverage_gaps.py) · [Filter cases](https://github.com/duriantaco/skylos/blob/87978e69adf91b3811e97b00c40aa39d7937cd94/test/test_cli.py) · [JSON-CI cases](https://github.com/duriantaco/skylos/blob/87978e69adf91b3811e97b00c40aa39d7937cd94/test/test_cli_json_ci_format.py)

The Rust ports run with `cargo test --locked -p skarmory_cli --test phase03_workflows`.
The SDK test runs against a built wheel in `tests/python/test_sdk.py`.
Skarmory's versioned baseline uses scoped identities, while the pinned
reference's legacy baseline matches several dead-code categories by simple
name. Skarmory's conservative parser-gap policy withholds graph-dependent
dead-code findings while reporting the unchecked project-wide reachability
scope. The compatibility gate can still evaluate available findings.
Unsupported languages, non-dead-code rule families, other CI providers'
automatic ref selection, and the full release inventory remain governed by
their respective Phase 0.3 workstreams.
