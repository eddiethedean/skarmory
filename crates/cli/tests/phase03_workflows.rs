use std::fs;
use std::path::Path;
use std::process::Command;

use serde_json::Value;

fn git(root: &Path, args: &[&str]) {
    let result = Command::new("git")
        .current_dir(root)
        .args(args)
        .output()
        .unwrap();
    assert!(result.status.success(), "git {args:?}: {result:?}");
}

fn project(files: &[(&str, &str)]) -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    git(root.path(), &["init", "-q"]);
    git(root.path(), &["config", "user.name", "Fixture"]);
    git(
        root.path(),
        &["config", "user.email", "fixture@example.com"],
    );
    git(root.path(), &["config", "commit.gpgsign", "false"]);
    git(root.path(), &["config", "core.autocrlf", "false"]);
    for (file, source) in files {
        let path = root.path().join(file);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, source).unwrap();
    }
    commit(root.path());
    root
}

fn commit(root: &Path) {
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "Fixture"]);
}

fn scan(root: &Path, args: &[&str], code: i32) -> Value {
    let result = Command::new(env!("CARGO_BIN_EXE_skarmory"))
        .current_dir(root)
        .env_remove("SKYLOS_CONFIG_FILE")
        .env_remove("SKYLOS_POLICY_BASE")
        .env_remove("SKYLOS_SYNCED_POLICY_FILE")
        .args([".", "--profile", "skylos-0.3", "--no-grep-verify"])
        .args(args)
        .output()
        .unwrap();
    assert_eq!(
        result.status.code(),
        Some(code),
        "{args:?}: {:?}\n{}",
        result,
        String::from_utf8_lossy(&result.stdout)
    );
    if result.stdout.is_empty() {
        return Value::Null;
    }
    serde_json::from_slice(&result.stdout).unwrap()
}

fn items<'a>(report: &'a Value, section: &str) -> &'a [Value] {
    report[section].as_array().unwrap()
}

fn names<'a>(report: &'a Value, section: &str) -> Vec<&'a str> {
    items(report, section)
        .iter()
        .map(|item| item["name"].as_str().unwrap())
        .collect()
}

#[test]
fn impact_flags_removed_last_caller_outside_changed_lines() {
    let root = project(&[
        (
            "app.py",
            "def legacy():\n    return 7\n\ndef already_dead():\n    return 8\n",
        ),
        ("caller.py", "from app import legacy\nlegacy()\n"),
    ]);
    fs::write(root.path().join("caller.py"), "print('done')\n").unwrap();
    let lines = scan(root.path(), &["--diff", "HEAD", "--strict"], 0);
    assert!(items(&lines, "unused_functions").is_empty());
    assert_eq!(
        lines["selected_paths"],
        serde_json::json!(["app.py", "caller.py"])
    );
    let impact = scan(
        root.path(),
        &["--diff", "HEAD", "--diff-mode", "impact", "--strict"],
        1,
    );
    assert_eq!(names(&impact, "unused_functions"), ["legacy"]);
    let item = &impact["unused_functions"][0];
    assert_eq!(item["report_scope"], "dependency_impact");
    assert_eq!(item["change_status"], "introduced");
    assert!(
        item["dead_code_evidence"]
            .as_array()
            .is_some_and(|events| !events.is_empty())
    );
    let receipt = &impact["analysis_summary"]["pr_impact"];
    assert_eq!(receipt["status"], "complete");
    assert_eq!(receipt["introduced_count"], 1);

    let cache = tempfile::tempdir().unwrap();
    for workers in ["1", "4", "4"] {
        let mut cached = scan(
            root.path(),
            &[
                "--diff",
                "HEAD",
                "--diff-mode",
                "impact",
                "--strict",
                "--cache-dir",
                cache.path().to_str().unwrap(),
                "--workers",
                workers,
            ],
            1,
        );
        cached.as_object_mut().unwrap().remove("cache");
        assert_eq!(cached, impact);
    }
}

#[test]
fn changed_lines_use_full_project_and_working_tree_sources() {
    let root = project(&[
        (
            "app.py",
            "def live():\n    return 1\n\ndef old():\n    return 2\n",
        ),
        ("caller.py", "from app import live\nlive()\n"),
    ]);
    git(root.path(), &["tag", "base"]);
    fs::write(
        root.path().join("committed.py"),
        "def committed():\n    return 7\n",
    )
    .unwrap();
    commit(root.path());
    fs::write(root.path().join("app.py"),
        "def live():  # modified definition still has a caller\n    return 1\n\ndef old():\n    return 2\n\ndef staged():\n    return 3\n").unwrap();
    git(root.path(), &["add", "app.py"]);
    use std::io::Write;
    writeln!(
        fs::OpenOptions::new()
            .append(true)
            .open(root.path().join("app.py"))
            .unwrap(),
        "\ndef unstaged():\n    return 4"
    )
    .unwrap();
    fs::write(
        root.path().join("new file.py"),
        "def untracked():\n    return 5\n",
    )
    .unwrap();
    fs::write(root.path().join(".gitignore"), "ignored.py\n").unwrap();
    fs::write(
        root.path().join("ignored.py"),
        "def ignored():\n    return 6\n",
    )
    .unwrap();
    let report = scan(
        root.path(),
        &["--diff", "HEAD", "--strict", "--select", "SKY-U001"],
        1,
    );
    assert_eq!(
        names(&report, "unused_functions"),
        ["staged", "unstaged", "untracked"]
    );
    let since_base = scan(root.path(), &["--diff", "base", "--select", "SKY-U001"], 0);
    assert_eq!(
        names(&since_base, "unused_functions"),
        ["staged", "unstaged", "committed", "untracked"]
    );
    let files = scan(
        root.path(),
        &["--diff-base", "HEAD", "--select", "SKY-U001"],
        0,
    );
    assert_eq!(
        names(&files, "unused_functions"),
        ["old", "staged", "unstaged", "untracked"]
    );
}

#[test]
fn line_shift_and_base_policy_are_stable_and_operator_config_applies_to_both() {
    let root = project(&[
        ("service/app.py", "def existing_dead():\n    return 1\n"),
        (
            "pyproject.toml",
            "[tool.skylos]\nwhitelist = [\"existing_dead\"]\n",
        ),
        (
            "service/nested/skylos.toml",
            "whitelist = [\"nested_dead\"]\n",
        ),
        (
            "service/nested/mod.py",
            "def nested_dead():\n    return 2\n",
        ),
    ]);
    let subdir = root.path().join("service");
    fs::write(
        subdir.join("app.py"),
        "# new header\n\ndef existing_dead():\n    return 1\n",
    )
    .unwrap();
    fs::write(
        root.path().join("pyproject.toml"),
        "[tool.skylos]\nwhitelist = []\n",
    )
    .unwrap();
    fs::remove_file(subdir.join("nested/skylos.toml")).unwrap();
    let args = ["--diff", "HEAD", "--diff-mode", "impact", "--strict"];
    let inherited = scan(&subdir, &args, 0);
    assert!(items(&inherited, "unused_functions").is_empty());
    assert_eq!(items(&inherited, "whitelisted").len(), 2);
    let operator = tempfile::tempdir().unwrap();
    let config = operator.path().join("operator.toml");
    fs::write(&config, "[tool.skylos]\n").unwrap();
    let explicit = scan(
        &subdir,
        &[
            args.as_slice(),
            &["--config-file", config.to_str().unwrap()],
        ]
        .concat(),
        0,
    );
    assert!(items(&explicit, "unused_functions").is_empty());
    assert!(items(&explicit, "whitelisted").is_empty());
    assert_eq!(
        explicit["analysis_summary"]["pr_impact"]["introduced_count"],
        0
    );
    assert_eq!(
        explicit["analysis_summary"]["pr_impact"]["status"],
        "complete"
    );
}

#[test]
fn impact_identifies_incomplete_base_head_and_both() {
    for (base, head, reason) in [
        (
            "def broken(\n",
            "def new_dead():\n    return 1\n",
            "base_analysis_incomplete",
        ),
        (
            "def helper():\n    return 1\n",
            "from app import helper\nhelper(\n",
            "head_analysis_incomplete",
        ),
        (
            "def broken(\n",
            "also_broken(\n",
            "base_and_head_analysis_incomplete",
        ),
    ] {
        let root = project(&[("app.py", base)]);
        fs::write(root.path().join("app.py"), head).unwrap();
        let report = scan(
            root.path(),
            &["--diff", "HEAD", "--diff-mode", "impact", "--gate"],
            2,
        );
        let impact = &report["analysis_summary"]["pr_impact"];
        assert_eq!(impact["status"], "incomplete");
        assert_eq!(impact["reason"], reason);
        assert_eq!(
            impact["base"]["status"],
            if base.starts_with("def broken") {
                "incomplete"
            } else {
                "complete"
            }
        );
        assert_eq!(
            impact["head"]["status"],
            if head.starts_with("def new") {
                "complete"
            } else {
                "incomplete"
            }
        );
        assert!(
            items(&report, "analysis_errors")
                .iter()
                .any(|item| item["kind"] == "pr_impact_incomplete")
        );
        assert!(
            items(&report, "unused_functions")
                .iter()
                .all(|item| item.get("change_status").is_none())
        );
    }
}

#[test]
fn impact_of_a_new_project_directory_has_an_empty_base() {
    let root = project(&[("app.py", "print('existing')\n")]);
    let service = root.path().join("service");
    fs::create_dir(&service).unwrap();
    fs::write(service.join("app.py"), "def new_dead():\n    return 1\n").unwrap();
    let report = scan(
        &service,
        &["--diff", "HEAD", "--diff-mode", "impact", "--strict"],
        1,
    );
    assert_eq!(names(&report, "unused_functions"), ["new_dead"]);
    assert_eq!(report["unused_functions"][0]["change_status"], "introduced");
    assert_eq!(
        report["analysis_summary"]["pr_impact"]["base"]["selected_files"],
        0
    );
}

#[test]
fn baseline_tracks_shifted_and_stale_findings_and_can_be_read_from_git() {
    let root = project(&[(
        "app.py",
        "def existing():\n    return 1\n\ndef fixed():\n    return 2\n",
    )]);
    let path = ".skylos/baseline.json";
    let written = scan(root.path(), &["--write-baseline", path], 0);
    assert_eq!(written["analysis_summary"]["baseline"]["status"], "written");
    commit(root.path());
    let baseline = fs::read(root.path().join(path)).unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&baseline).unwrap()["schema_version"],
        1
    );

    fs::write(root.path().join("app.py"),
        "# shifted\n\ndef existing():\n    return 1\n\ndef fixed():\n    return 2\n\nfixed()\n\ndef new():\n    return 3\n").unwrap();
    let report = scan(root.path(), &["--baseline", "--strict"], 1);
    assert_eq!(names(&report, "unused_functions"), ["new"]);
    assert_eq!(report["analysis_summary"]["baseline"]["matched_count"], 1);
    assert_eq!(report["analysis_summary"]["baseline"]["stale_count"], 1);

    fs::write(
        root.path().join(path),
        "{\"schema_version\":99,\"findings\":[]}",
    )
    .unwrap();
    assert_eq!(
        scan(root.path(), &["--baseline-ref", "HEAD", "--strict"], 1),
        report
    );
    let unavailable = scan(root.path(), &["--baseline", "--gate"], 2);
    assert_eq!(unavailable["gate"]["verdict"], "incomplete");
    assert_eq!(unavailable["coverage"]["status"], "complete");
    assert_eq!(
        unavailable["analysis_summary"]["baseline"]["status"],
        "unavailable"
    );
    assert_eq!(names(&unavailable, "unused_functions"), ["existing", "new"]);

    fs::write(root.path().join("app.py"), "def broken(\n").unwrap();
    let incomplete = scan(root.path(), &["--write-baseline", path], 2);
    assert_eq!(
        incomplete["analysis_summary"]["baseline"]["status"],
        "incomplete"
    );
    assert_eq!(
        fs::read_to_string(root.path().join(path)).unwrap(),
        "{\"schema_version\":99,\"findings\":[]}"
    );
}

#[test]
fn python_baseline_round_trips_in_a_mixed_repository() {
    let root = project(&[
        ("app.py", "def old():\n    return 1\n"),
        ("package.json", r#"{"main":"main.js"}"#),
        ("main.js", "console.log('entry');\n"),
        ("unused.js", "console.log('unused');\n"),
    ]);
    let path = ".skylos/baseline.json";
    let saved = scan(root.path(), &["--write-baseline", path], 0);
    assert_eq!(names(&saved, "unused_functions"), ["old"]);
    let baseline: Value =
        serde_json::from_slice(&fs::read(root.path().join(path)).unwrap()).unwrap();
    assert_eq!(
        baseline["findings"],
        serde_json::json!([{"rule": "SKY-U001", "path": "app.py", "symbol": "app.old"}])
    );

    let applied = scan(root.path(), &["--baseline", path], 0);
    assert_eq!(applied["analysis_summary"]["baseline"]["matched_count"], 1);
    assert!(items(&applied, "unused_functions").is_empty());
    assert_eq!(applied["unused_files"], saved["unused_files"]);
}

#[test]
fn gate_coverage_is_independent_of_the_verdict_and_display_filters() {
    let root = project(&[
        ("a.py", "print(1)\n"),
        ("b.py", "print(2)\n"),
        ("c.py", "def broken(\n"),
    ]);
    let report = scan(root.path(), &["--gate"], 0);
    assert_eq!(report["coverage"]["status"], "incomplete");
    assert_eq!(report["gate"]["verdict"], "passed");
    assert_eq!(report["gate"]["coverage_status"], "partial");
    assert_eq!(
        report["gate"]["coverage_warnings"][0],
        "Incomplete coverage: 1 of 3 source files could not be analysed"
    );
    assert_eq!(report["analysis_coverage"]["unchecked_files"][0], "c.py");
    assert_eq!(
        report["analysis_coverage"]["unchecked_scope"][0],
        "project_dead_code_reachability"
    );
    assert!(
        !report["gate"]["coverage_warning_details"][0]["details"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let unchanged = scan(root.path(), &["--gate", "--diff", "HEAD"], 0);
    assert_eq!(unchanged["gate"]["coverage_status"], "partial");

    for flag in ["--strict", "--require-complete"] {
        let strict = scan(root.path(), &["--gate", flag], 2);
        assert_eq!(strict["gate"]["verdict"], "incomplete");
        assert_eq!(strict["gate"]["coverage_status"], "partial");
    }
    fs::write(root.path().join("b.py"), "also_broken(\n").unwrap();
    let majority = scan(root.path(), &["--gate"], 2);
    assert_eq!(majority["gate"]["coverage_status"], "incomplete");
    fs::write(root.path().join("b.py"), "print(2)\n").unwrap();

    let changed = root.path().join("c.py");
    fs::write(&changed, "def STILL_BROKEN(\n").unwrap();
    let in_diff = scan(root.path(), &["--gate", "--diff", "HEAD"], 2);
    assert_eq!(in_diff["gate"]["coverage_status"], "incomplete");

    let config = root.path().join(".skylos.toml");
    fs::write(&config, "[gate.coverage]\nrequire_complete = true\n").unwrap();
    let required = scan(root.path(), &["--gate", "--force"], 2);
    assert_eq!(required["gate"]["verdict"], "incomplete");
    assert_eq!(required["gate"]["forced"], false);
    fs::write(&config, "[gate]\nstrict = true\n").unwrap();
    assert_eq!(
        scan(root.path(), &["--gate"], 2)["gate"]["verdict"],
        "incomplete"
    );

    fs::write(changed, "def old():\n    return 1\n").unwrap();
    fs::write(&config, "[gate]\nmax_dead_code = 1\n").unwrap();
    assert_eq!(
        scan(root.path(), &["--gate"], 0)["gate"]["coverage_status"],
        "complete"
    );
    fs::write(&config, "[gate]\nmax_dead_code = 0\n").unwrap();
    let failed = scan(root.path(), &["--gate", "--severity", "medium"], 1);
    let forced = scan(root.path(), &["--gate", "--force"], 0);
    assert_eq!(forced["gate"]["verdict"], "failed");
    assert_eq!(forced["gate"]["forced"], true);
    assert_eq!(failed["gate"]["verdict"], "failed");
    assert!(items(&failed, "unused_functions").is_empty());
    assert!(
        failed["gate"]["reason_details"][0]["details"][0]
            .as_str()
            .unwrap()
            .contains("c.py:1")
    );
}

#[test]
fn json_ci_preserves_findings_evidence_coverage_and_sarif() {
    let root = project(&[(
        "app.py",
        "import sys\n\ndef used():\n    return 1\n\ndef unused():\n    return 2\n\nprint(used())\n",
    )]);
    let mut full = scan(
        root.path(),
        &["--format", "json", "--sarif", "full.sarif"],
        0,
    );
    let compact = scan(
        root.path(),
        &["--format", "json-ci", "--sarif", "ci.sarif"],
        0,
    );
    assert!(
        full.as_object_mut()
            .unwrap()
            .remove("dead_code_evidence")
            .is_some()
    );
    let definitions = full.as_object_mut().unwrap().remove("definitions").unwrap();
    assert!(
        definitions
            .as_object()
            .unwrap()
            .values()
            .any(|item| item["name"] == "app.used")
    );
    assert_eq!(compact, full);
    assert_eq!(
        compact["analysis_summary"]["dead_code_evidence"]["candidate_decisions"]["reported"],
        2
    );
    assert_eq!(
        compact["unused_functions"][0]["dead_code_classification"],
        "likely_dead"
    );
    assert!(
        !compact["unused_functions"][0]["dead_code_evidence"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        fs::read(root.path().join("full.sarif")).unwrap(),
        fs::read(root.path().join("ci.sarif")).unwrap()
    );

    let selected = scan(
        root.path(),
        &["--select", "sky-u001", "--severity", "low"],
        0,
    );
    assert_eq!(selected["unused_functions"], compact["unused_functions"]);
    assert!(items(&selected, "unused_imports").is_empty());
    let repeated = scan(
        root.path(),
        &["--select", "SKY-E002,SKY-U002", "--select", "SKY-U001"],
        0,
    );
    assert_eq!(repeated["unused_imports"], compact["unused_imports"]);
    assert_eq!(repeated["unused_functions"], compact["unused_functions"]);

    fs::write(root.path().join("app.py"), "broken(\n").unwrap();
    let mut full = scan(
        root.path(),
        &["--format", "json", "--select", "SKY-U001"],
        2,
    );
    full.as_object_mut().unwrap().remove("dead_code_evidence");
    full.as_object_mut().unwrap().remove("definitions");
    let compact = scan(
        root.path(),
        &["--format", "json-ci", "--select", "SKY-U001"],
        2,
    );
    assert_eq!(compact, full);
    assert!(!items(&compact, "analysis_errors").is_empty());
}

#[test]
fn snapshots_use_raw_blobs_without_invoking_clean_or_smudge_filters() {
    let root = project(&[
        (
            "app.py",
            "def unused():\n    return 1\n\nraise RuntimeError('DO_NOT_EXECUTE_TARGET')\n",
        ),
        (".gitattributes", "*.py filter=fixture\n"),
    ]);
    git(root.path(), &["config", "filter.fixture.clean", "false"]);
    git(root.path(), &["config", "filter.fixture.smudge", "false"]);
    git(root.path(), &["config", "filter.fixture.required", "true"]);
    let report = scan(
        root.path(),
        &["--diff", "HEAD", "--diff-mode", "impact", "--strict"],
        0,
    );
    assert_eq!(
        report["analysis_summary"]["pr_impact"]["status"],
        "complete"
    );
    assert!(items(&report, "unused_functions").is_empty());
}

#[test]
fn impact_omits_oversized_assets_outside_analysis_scope() {
    let root = project(&[
        (
            "app.py",
            "def legacy():\n    return 7\n\ndef already_dead():\n    return 8\n",
        ),
        ("caller.py", "from app import legacy\nlegacy()\n"),
    ]);
    let assets = root.path().join("docs/media");
    fs::create_dir_all(&assets).unwrap();
    let oversized = vec![0; 2 * 1024 * 1024 + 1];
    fs::write(assets.join("logo.PNG"), &oversized).unwrap();
    fs::write(assets.join("fixture.bin"), &oversized).unwrap();
    commit(root.path());
    fs::write(root.path().join("caller.py"), "print('done')\n").unwrap();

    let report = scan(
        root.path(),
        &["--diff", "HEAD", "--diff-mode", "impact", "--strict"],
        1,
    );
    assert_eq!(names(&report, "unused_functions"), ["legacy"]);
    assert_eq!(report["unused_functions"][0]["change_status"], "introduced");
    let receipt = &report["analysis_summary"]["pr_impact"];
    assert_eq!(receipt["status"], "complete");
    assert_eq!(receipt["base"]["status"], "complete");
    assert_eq!(receipt["head"]["status"], "complete");
    assert_eq!(
        receipt["omitted"],
        serde_json::json!({"oversize_non_source": 2})
    );
}

#[test]
fn impact_preserves_the_blob_limit_for_analysis_inputs() {
    let oversized = vec![b' '; 2 * 1024 * 1024 + 1];
    for input in ["app.py", "routes.yaml", "vite.config.png", "Dockerfile.png"] {
        let root = project(&[("entry.py", "print('entry')\n")]);
        let input = root.path().join(input);
        fs::write(&input, &oversized).unwrap();
        commit(root.path());
        fs::remove_file(input).unwrap();

        let report = scan(
            root.path(),
            &["--diff", "HEAD", "--diff-mode", "impact", "--strict"],
            2,
        );
        let receipt = &report["analysis_summary"]["pr_impact"];
        assert_eq!(receipt["reason"], "base_analysis_incomplete");
        assert_eq!(receipt["base"]["status"], "incomplete");
        assert_eq!(receipt["head"]["status"], "complete");
        assert!(receipt.get("omitted").is_none());
    }
}

#[test]
fn an_omitted_asset_required_by_dependency_analysis_leaves_impact_incomplete() {
    let root = project(&[
        ("app.py", "def old():\n    return 1\n"),
        ("caller.py", "from app import old\nold()\n"),
        ("requirements.txt", "-r dependencies.bin\n"),
    ]);
    let dependency_input = root.path().join("dependencies.bin");
    fs::write(&dependency_input, vec![b' '; 2 * 1024 * 1024 + 1]).unwrap();
    commit(root.path());
    fs::remove_file(dependency_input).unwrap();
    fs::write(root.path().join("requirements.txt"), "requests\n").unwrap();
    fs::write(root.path().join("caller.py"), "print('done')\n").unwrap();

    let report = scan(
        root.path(),
        &[
            "--diff",
            "HEAD",
            "--diff-mode",
            "impact",
            "--quality",
            "--strict",
        ],
        2,
    );
    let receipt = &report["analysis_summary"]["pr_impact"];
    assert_eq!(receipt["reason"], "base_analysis_incomplete");
    assert_eq!(receipt["base"]["status"], "incomplete");
    assert_eq!(receipt["head"]["status"], "complete");
    assert_eq!(receipt["introduced_count"], 0);
    assert!(items(&report, "unused_functions").is_empty());
}

#[test]
fn impact_requires_the_full_head_comparison_mode() {
    let root = project(&[("app.py", "print(1)\n")]);
    assert!(scan(root.path(), &["--diff-mode", "impact"], 2).is_null());
    assert!(
        scan(
            root.path(),
            &[
                "--diff",
                "HEAD",
                "--diff-base",
                "HEAD",
                "--diff-mode",
                "impact"
            ],
            2
        )
        .is_null()
    );
}

#[test]
fn dependency_findings_use_manifest_lines_in_the_diff() {
    let root = project(&[
        ("app.py", "print(1)\n"),
        ("requirements.txt", "old-unused==1\n"),
    ]);
    fs::write(
        root.path().join("requirements.txt"),
        "old-unused==1\nnew-unused==1\n",
    )
    .unwrap();
    let report = scan(
        root.path(),
        &["--diff", "HEAD", "--select", "SKY-U005", "--strict"],
        1,
    );
    assert_eq!(names(&report, "quality"), ["new-unused"]);
    assert_eq!(report["quality"][0]["line"], 2);
}

#[test]
fn diff_gate_uses_the_base_configuration() {
    let root = project(&[
        ("app.py", "print(1)\n"),
        (".skylos.toml", "[gate]\nmax_dead_code = 0\n"),
    ]);
    fs::write(root.path().join("app.py"), "def dead():\n    return 1\n").unwrap();
    fs::write(
        root.path().join(".skylos.toml"),
        "[gate]\nmax_dead_code = 10\n",
    )
    .unwrap();
    let local = scan(root.path(), &["--gate"], 0);
    assert_eq!(local["gate"]["verdict"], "passed");
    let compared = scan(root.path(), &["--diff", "HEAD", "--gate"], 1);
    assert_eq!(compared["gate"]["verdict"], "failed");
    assert_eq!(names(&compared, "unused_functions"), ["dead"]);
}
