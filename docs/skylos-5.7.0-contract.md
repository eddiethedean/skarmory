# Skylos 5.7.0 contract inventory

Reference revision `87978e69adf91b3811e97b00c40aa39d7937cd94`. This inventory
freezes the surfaces for future conformance work. Only function reachability
and incomplete-input controls in the [Phase 0 scope](phase-0.md) were exercised.

| Surface | Pinned reference behavior and inputs |
| --- | --- |
| CLI discovery and scope | The source-scan entry point accepts paths, `--config-file`, `--exclude`, `--include-folder`, `--no-default-excludes`, `--include-generated`, `--diff`, `--diff-base`, and `--diff-mode`. The source universe also depends on supported suffixes, exclusion rules, symlinks, and the native discovery path. |
| CLI rule selection | Default scan includes dead code. `--confidence`, `--category`, `--severity`, `--select`, `--no-grep-verify`, `--all-advice`, and `--expand-advice` affect analysis or listing. Opt-in families include `--quality`, `--danger`, `--secrets`, `--ai-defects`, and `--sca`. `-a` enables the broader suite. |
| CLI presentation | `--json`, `--format`, `--sarif`, `--output`, `--summary`, `--file-filter`, and `--limit` cover machine output and filtering. Capture exact stdout, stderr, file contents, and process status per mode. JSON, JSON-CI, concise, and SARIF have distinct contracts. |
| CLI workflows | `--gate`, `--strict`, `--force`, `--baseline`, and `--baseline-ref` affect gate or exit policy. `--trace`, `--coverage`, `--allow-coverage-execution`, and `--pytest-fixtures` control runtime evidence. Profiling, cache controls, configured type checking, cleanup, cloud, and verification are distinct workflows. |
| Configuration resolution | For a file scan start at its parent. Per directory choose `.skylos.toml`, then `skylos.toml`, then `pyproject.toml` containing `[tool.skylos]`. Stop ordinary discovery at `.git`, filesystem root, or `root = true`. Merge from ancestors to descendants. `--config-file` / the API argument takes precedence over `SKYLOS_CONFIG_FILE` and replaces the discovered user layers. Defaults, synced config, operator-provided policy and a policy-base revision can also apply. Ordinary lists replace, while designated sections such as `dead_code`, `gate`, and `masking` have section-level merge rules. |
| Suppressions | Configurable names and path excludes, whitelist names and documented/temporary entries, lower-confidence entries, path overrides, inline `noqa` and `skylos: ignore` directives, rule-level policy, and custom dead-code entry-point selectors. Expiring entries require a fixed clock in conformance. Discovery exclusions, suppression, and confidence/listing decisions need separate assertions. |
| Python API | Public `analyze` forwards to the analyzer. The analyzer accepts `path`, `conf=60`, `exclude_folders`, `enable_secrets`, `enable_danger`, `enable_quality`, `enable_ai_defects`, `enable_dependency_hallucinations`, `extra_visitors`, `progress_callback`, `custom_rules_data`, `changed_files`, `grep_verify`, `enable_sca`, `trace_file`, `config_file`, `project_config_overrides`, `required_config_rules`, `grep_cache`, `dependency_bump_diff_base`, `include_review_proofs`, `include_review_context`, `skip_generated`, `generated_keep_paths`, `all_advice`, `changed_file_exclude_folders`, and `config_exclude_folders`. The compatibility result is a JSON string. |
| Rules and semantics | Dead-code lists include unused functions, classes, parameters, imports, variables, and files. The broader reference includes local quality rules, clones and coupling, risky operations and data flows, secret detection, dependency checks, and AI-defect checks. Framework roots, tests, exports, uncertain calls, and confidence thresholds affect eligibility. Freezing these semantics requires category-specific fixtures. |
| Report data | Inspect `unused_*`, `definitions`, `dead_code_evidence`, `dead_code_rescues`, `dead_code_abstentions`, `suppressed`, `whitelisted`, `analysis_errors`, `analysis_summary`, `assessment`, workspace data, and family-specific fields. Function entries include `full_name`, file, line, confidence, evidence, and disposition. Listing and gating can have separate policy. |
| Exit and coverage | The Python API can return `analysis_errors` to a successful caller. For the reachable fixture, the CLI returned 0 in JSON report mode and 1 in concise and JSON strict modes. Gate mode and gate plus force returned 0 under the frozen settings. An unknown flag returned 2. The invalid fixture returned 2 in JSON report and gate modes, each with parseable JSON containing `SKY-ANALYSIS-INCOMPLETE`. |

The Phase 0 comparison uses local function findings projected to file, fully
qualified name, and line. Confidence, evidence shape, error locations, exit
behavior, and coverage are reviewed separately. A future explicitly selected
Skylos profile will require those semantics. Phase 0 exposes only the native
profile, so there is no legacy profile selector yet.

Primary pinned sources are the [CLI](https://github.com/duriantaco/skylos/blob/87978e69adf91b3811e97b00c40aa39d7937cd94/skylos/cli.py),
[configuration loader](https://github.com/duriantaco/skylos/blob/87978e69adf91b3811e97b00c40aa39d7937cd94/skylos/config.py),
and [analyzer](https://github.com/duriantaco/skylos/blob/87978e69adf91b3811e97b00c40aa39d7937cd94/skylos/analyzer.py).
