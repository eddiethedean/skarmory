# Phase 0.3 Python cleanup assertion mapping

The reference is Skylos 5.7.0 at
`87978e69adf91b3811e97b00c40aa39d7937cd94`. These ports exercise the native
Skarmory CLI against fresh Python projects. The queue and comparable rescan
use real Skarmory analysis. Explicit validation commands are real
subprocesses and put fixture mutations at the validation boundary.

| Pinned assertions | Skarmory regression |
| --- | --- |
| `test_cleanup_plan.py::test_dead_code_is_review_work_never_an_automatic_delete`, `test_cleanup_execution.py::test_import_preview_reuses_codemod_and_writes_nothing`, `test_multi_import_preview_targets_one_bound_name` | `queue_and_preview_remove_one_import_without_writing` checks that the queue and preview leave original bytes intact, the unused import edit leaves the other import, and the preview exposes a proof without private payloads. |
| `test_cleanup_execution.py::test_function_preview_preserves_surrounding_code`, `test_definition_time_effects_require_manual_review`, `test_unknown_import_effects_are_not_automatically_removed`, `test_stdlib_lookalike_in_project_is_not_trusted` | `previews_an_unused_function_and_preserves_its_used_neighbor` and `definition_effects_and_project_imports_remain_review_tasks` cover the paired eligible and manual cases using real source and findings. |
| `test_cleanup_execution.py::test_stale_hash_and_forged_anchor_cannot_plan`, `test_stale_and_tampered_edits_cannot_overwrite_changes`, `test_unsafe_test_commands_are_rejected_before_writing` | `stale_preview_proof_cannot_overwrite_an_edited_source` and `stale_policy_and_unsafe_check_commands_are_rejected_before_apply` exercise changed source and configuration, a mismatching preview proof, and rejection of an unsafe check command before any source write. `ignored_local_provider_invalidates_preapproved_import_preview` adds a Python provider after preview but before apply, under a path already excluded from analysis. |
| `test_cleanup_execution.py::test_source_symlinks_parent_symlinks_hardlinks_and_escape_are_rejected` | `preview_and_apply_preserve_linked_targets` exercises refusal of direct symlinks and hardlinks after a live queue has identified a regular source. |
| `test_cleanup_execution.py::test_duplicate_source_anchors_require_explicit_manual_review` | `duplicate_source_anchors_stay_in_manual_review` checks that duplicates in real import and function findings remain visible and reject both preview and apply. |
| `test_cleanup_workflow.py::test_task_identity_survives_blank_lines_but_apply_source_hash_changes` | `task_identity_survives_line_shifts_but_proofs_follow_source_bytes` checks task identity remains stable across leading blank lines while the source fingerprint and approval proof change. |
| `test_cleanup_workflow.py::test_actual_analyzer_patch_roundtrip`, `test_preview_then_apply_rescans_and_skipped_tests_stay_unverified` | `apply_runs_a_real_rescan_and_reports_skipped_tests` checks the applied bytes, the real rescan receipt and the subsequently clear queue. |
| `test_cleanup_execution.py::test_failed_explicit_test_rolls_back_exact_applied_bytes`, `test_failed_test_preserves_a_user_change_instead_of_rollback`, `test_cleanup_workflow.py::test_failing_authorized_check_rolls_back_and_returns_failure` | `failed_explicit_check_rolls_back_exact_bytes_without_overwriting_user_edits` runs a failing child process with and without a concurrent source edit and checks the outcome, bytes and mode. `a_failed_check_does_not_roll_back_across_changed_dependencies` changes configuration or adds a provider before failing. |
| `test_cleanup_workflow.py::test_policy_changed_by_authorized_check_cannot_verify_a_fix` | `policy_or_provider_mutations_during_a_check_leave_the_rescan_incomplete` changes project configuration or adds a local import provider in the explicit validation command. The check produces fresh observed evidence and the rescan stays incomplete while preserving the changed input. |
| `test_cleanup_execution.py::test_complete_rescan_and_authorized_tests_are_required_for_verified`, `test_zero_exit_without_observed_tests_cannot_verify_a_cleanup`, `test_cleanup_workflow.py::test_authorized_passing_tests_and_same_scope_rescan_produce_verified_receipt` | `observed_checks_and_comparable_rescan_verify_the_edit` runs an explicit command that checks the applied source and emits a fresh JUnit receipt. `an_opaque_zero_exit_check_does_not_verify_the_edit` exercises the same rescan with successful execution and no observed test evidence. |
| `test_cleanup_fstring_verification.py::test_deletion_only_verification_explains_its_coverage_limit`, `test_custom_junit_without_replacement_coverage_remains_unverified` | `observed_checks_and_comparable_rescan_verify_the_edit` checks the absence of edit coverage evidence is explicit and the deletion-only verified receipt explains its limit. `observed_tests_do_not_verify_a_same_line_import_rewrite` exercises a separate replacement fixture with a passing check and no coverage evidence for the changed line. |
| `test_cleanup_execution.py::test_test_command_unavailable_is_incomplete_never_passed` | `an_unavailable_explicit_check_leaves_verification_incomplete` requests an unavailable executable and checks the incomplete test receipt and the successfully rescanned edit. |
| `test_cleanup_execution.py::test_original_anchor_returning_during_rescan_is_not_verified` | `a_returned_original_finding_fails_rescan_without_overwriting_the_source` restores the selected statement before a real rescan. Skarmory reports the returned statement and preserves the restored source. |
| `test_cleanup_execution.py::test_source_removed_after_rescan_preserves_the_incomplete_receipt` | `source_changed_after_comparison_preserves_passed_rescan_and_user_changes` removes or rewrites the file after a real rescan and comparison. Both receipts still pass, the overall execution is incomplete, and the intervening change is preserved. `a_passing_rescan_cannot_verify_changed_raw_source_bytes` exercises the final check with a real subprocess that changes the file's line endings. |
| `test_cleanup_execution.py::test_new_high_risk_finding_rolls_back_but_existing_shift_does_not`, `test_comparator_ignores_advice_as_risk_but_preserves_identity_variants`, `test_medium_defects_and_evidence_changes_are_regressions`, `test_source_anchors_detect_new_risk_at_different_statement_even_if_totals_match` | Core comparator tests supply synthetic defect and review rows to the production report consumer. `medium_review_findings_and_shifted_dead_code_do_not_trigger_rollback` checks that native medium-severity dependency findings remain review items. Malformed external reports are outside this interface because cleanup consumes typed reports from the scanner. |
| `test_cleanup_execution.py::test_incomplete_or_changed_scope_never_verifies` | The real configuration, provider, manifest, and source mutation tests exercise comparability. `a_changed_dependency_manifest_invalidates_validation_and_rollback_proof` additionally verifies that changing requirements during a passing or failing check preserves the manifest and prevents verification or rollback. |

Run the ports with
`cargo test --locked -p skarmory_cli --test phase03_cleanup`. Guarded source
editing tests run on Unix. Platforms without guarded editing exercise
preview and verify that apply leaves source intact. The same CLI tests also
compare the queue and preview with one worker and no cache against four
workers and cold and warm caches.

Four original workflow cases also passed through a temporary adapter that
replaced only the command entry point with the native CLI and returned its
exit code and JSON. The candidate used real Skarmory analysis even where an
original fixture supplied a stub. The assertions and filesystem inputs
were unchanged. The cases were `test_actual_analyzer_patch_roundtrip`,
`test_preview_then_apply_rescans_and_skipped_tests_stay_unverified`,
`test_failing_authorized_check_rolls_back_and_returns_failure`, and
`test_authorized_passing_tests_and_same_scope_rescan_produce_verified_receipt`.
The last case invoked real pytest through the explicit check boundary.

The Phase 0.3 scanner supplies Python dead-code findings and optional
dependency review findings. The synthetic comparator inputs exercise the
pinned risk distinctions without claiming native coverage for later rule
families. Full test inventory and release receipts remain part of the
release gate.

This mapping records scoped translations of assertions and boundaries.
The pinned tests themselves remain in the reference checkout. The release
inventory and receipt collection are tracked separately under
[#41](https://github.com/eddiethedean/skarmory/issues/41). The TypeScript
transformations have their own scope under
[#45](https://github.com/eddiethedean/skarmory/issues/45).

[Pinned planner assertions](https://github.com/duriantaco/skylos/blob/87978e69adf91b3811e97b00c40aa39d7937cd94/test/test_cleanup_plan.py)
· [Pinned execution assertions](https://github.com/duriantaco/skylos/blob/87978e69adf91b3811e97b00c40aa39d7937cd94/test/test_cleanup_execution.py)
· [Pinned workflow assertions](https://github.com/duriantaco/skylos/blob/87978e69adf91b3811e97b00c40aa39d7937cd94/test/test_cleanup_workflow.py)
· [Pinned coverage assertions](https://github.com/duriantaco/skylos/blob/87978e69adf91b3811e97b00c40aa39d7937cd94/test/test_cleanup_fstring_verification.py)
