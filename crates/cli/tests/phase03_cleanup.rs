use std::fs;
use std::path::Path;
use std::process::Command;

use serde_json::Value;

fn project(source: &str) -> tempfile::TempDir {
    let project = tempfile::tempdir().unwrap();
    fs::write(project.path().join("app.py"), source).unwrap();
    project
}

fn skarmory(root: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_skarmory"));
    command
        .current_dir(root)
        .env_remove("SKYLOS_CONFIG_FILE")
        .env_remove("SKYLOS_POLICY_BASE")
        .env_remove("SKYLOS_SYNCED_POLICY_FILE")
        .env_remove("DJANGO_SETTINGS_MODULE")
        .env_remove("SKARMORY_TEST_CLEANUP_ACTION")
        .env_remove("SKARMORY_TEST_CLEANUP_BINARY");
    command
}

fn cleanup(root: &Path, args: &[&str], expected_exit: i32) -> Value {
    cleanup_with_check(root, args, expected_exit, None)
}

fn cleanup_with_check(
    root: &Path,
    args: &[&str],
    expected_exit: i32,
    check_action: Option<&str>,
) -> Value {
    let mut command = skarmory(root);
    command
        .args(["cleanup", ".", "--format", "json"])
        .args(args);
    if let Some(action) = check_action {
        command.env("SKARMORY_TEST_CLEANUP_ACTION", action).env(
            "SKARMORY_TEST_CLEANUP_BINARY",
            std::env::current_exe().unwrap(),
        );
    }
    let output = command.output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(expected_exit),
        "{args:?}\n{stdout}\n{stderr}"
    );
    if output.stdout.is_empty() {
        return Value::Null;
    }
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| panic!("{error}: {stdout}"))
}

fn task<'a>(report: &'a Value, rule: &str, name: &str) -> &'a Value {
    report["tasks"]
        .as_array()
        .unwrap_or_else(|| panic!("missing cleanup queue: {report}"))
        .iter()
        .find(|task| task["rule_id"] == rule && task["name"] == name)
        .unwrap_or_else(|| panic!("missing task {rule} {name}: {report}"))
}

fn field<'a>(value: &'a Value, key: &str) -> &'a str {
    value[key]
        .as_str()
        .unwrap_or_else(|| panic!("missing string {key}: {value}"))
}

#[test]
fn queue_and_preview_remove_one_import_without_writing() {
    let original = "import math, json\r\nprint('ready')\r\n";
    let project = project(original);
    let path = project.path().join("app.py");
    let queue = cleanup(project.path(), &[], 0);
    let finding = task(&queue, "SKY-U002", "math");
    assert_eq!(finding["file"], "app.py");
    assert_eq!(finding["line"], 1);
    assert_eq!(finding["eligible"], true);
    assert!(!finding["source_fingerprint"].is_null());
    let preview = cleanup(project.path(), &["--task", field(finding, "id")], 0);
    let edit = &preview["edit"];
    let patch = field(edit, "patch");
    assert!(patch.contains("-import math, json"), "{patch}");
    assert!(patch.contains("+import json"), "{patch}");
    assert!(!field(edit, "proof").is_empty());
    assert_eq!(fs::read(path).unwrap(), original.as_bytes());
    assert!(
        edit.as_object()
            .unwrap()
            .keys()
            .all(|key| !key.starts_with('_'))
    );
}

#[test]
fn a_transitive_standard_library_shadow_blocks_cleanup() {
    for (import, provider) in [("pathlib", "fnmatch.py"), ("json", "copyreg.py")] {
        let original = format!("import {import}\nprint('ready')\n");
        let project = project(&original);
        let queue = cleanup(project.path(), &[], 0);
        let finding = task(&queue, "SKY-U002", import);
        let id = field(finding, "id");
        assert_eq!(finding["eligible"], true, "{finding}");
        cleanup(project.path(), &["--task", id], 0);

        fs::write(project.path().join(provider), "print('local provider')\n").unwrap();
        let queue = cleanup(project.path(), &[], 0);
        let finding = task(&queue, "SKY-U002", import);
        assert_eq!(finding["eligible"], false, "{finding}");
        assert!(field(finding, "reason").contains(provider), "{finding}");
        cleanup(project.path(), &["--task", id], 2);
        #[cfg(unix)]
        cleanup(project.path(), &["--task", id, "--apply"], 2);
        assert_eq!(
            fs::read_to_string(project.path().join("app.py")).unwrap(),
            original
        );
    }
}

#[test]
fn previews_an_unused_function_and_preserves_its_used_neighbor() {
    let original = "def unused():\n    return 1\n\ndef used():\n    return 42\n\nprint(used())\n";
    let project = project(original);
    let queue = cleanup(project.path(), &[], 0);
    let finding = task(&queue, "SKY-U001", "unused");
    assert_eq!(finding["eligible"], true);
    let preview = cleanup(project.path(), &["--task", field(finding, "id")], 0);
    let patch = field(&preview["edit"], "patch");
    assert!(patch.contains("-def unused():"), "{patch}");
    assert!(!patch.contains("-def used():"), "{patch}");
    assert_eq!(
        fs::read(project.path().join("app.py")).unwrap(),
        original.as_bytes()
    );
}

#[test]
fn task_identity_survives_line_shifts_but_proofs_follow_source_bytes() {
    let original = "import math\nprint('ready')\n";
    let project = project(original);
    let first = cleanup(project.path(), &[], 0);
    let first = task(&first, "SKY-U002", "math");
    let id = field(first, "id");
    let before = cleanup(project.path(), &["--task", id], 0);

    fs::write(project.path().join("app.py"), format!("\n\n{original}")).unwrap();
    let shifted = cleanup(project.path(), &[], 0);
    let shifted = task(&shifted, "SKY-U002", "math");
    assert_eq!(shifted["id"], id);
    assert_eq!(shifted["line"], 3);
    assert_ne!(shifted["source_fingerprint"], first["source_fingerprint"]);
    let after = cleanup(project.path(), &["--task", id], 0);
    assert_ne!(after["edit"]["proof"], before["edit"]["proof"]);
}

#[test]
fn cleanup_queue_and_preview_match_across_workers_and_cache_states() {
    let project = project("import math\nprint('ready')\n");
    fs::write(
        project.path().join("helper.py"),
        "def spare():\n    return 7\n",
    )
    .unwrap();
    let baseline = cleanup(project.path(), &[], 0);
    let id = field(task(&baseline, "SKY-U002", "math"), "id");
    let preview = cleanup(project.path(), &["--task", id], 0);

    for (args, expected) in [([].as_slice(), &baseline), (&["--task", id], &preview)] {
        let cache = tempfile::tempdir().unwrap();
        let cache_path = cache.path().to_str().unwrap();
        let cached = [&["--workers", "4", "--cache-dir", cache_path], args].concat();
        assert_eq!(&cleanup(project.path(), &cached, 0), expected);
        assert_eq!(&cleanup(project.path(), &cached, 0), expected);
    }
}

#[test]
fn definition_effects_and_project_imports_remain_review_tasks() {
    let original = concat!(
        "import local_plugin\nimport math\n\n",
        "def factory():\n    print(1)\n    return int\n\n",
        "def with_default(value=factory()):\n    pass\n\n",
        "def annotated(value: factory()):\n    pass\n\n",
        "print(42)\n",
    );
    let project = project(original);
    fs::write(project.path().join("local_plugin.py"), "print(7)\n").unwrap();
    fs::write(project.path().join("math.py"), "print(8)\n").unwrap();
    let queue = cleanup(project.path(), &[], 0);
    for (rule, name) in [
        ("SKY-U002", "local_plugin"),
        ("SKY-U002", "math"),
        ("SKY-U001", "with_default"),
        ("SKY-U001", "annotated"),
    ] {
        let finding = task(&queue, rule, name);
        assert_eq!(finding["eligible"], false, "{finding}");
        assert!(!field(finding, "reason").is_empty());
        cleanup(
            project.path(),
            &["--task", field(finding, "id"), "--apply"],
            2,
        );
    }
    assert_eq!(
        fs::read(project.path().join("app.py")).unwrap(),
        original.as_bytes()
    );
}

#[test]
fn duplicate_source_anchors_stay_in_manual_review() {
    for (source, rule, name) in [
        (
            "import math\nimport math\nprint('ready')\n",
            "SKY-U002",
            "math",
        ),
        (
            "def unused():\n    return 1\n\ndef unused():\n    return 2\n\nprint('ready')\n",
            "SKY-U001",
            "unused",
        ),
    ] {
        let project = project(source);
        let queue = cleanup(project.path(), &[], 0);
        let matches: Vec<_> = queue["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|task| task["rule_id"] == rule && task["name"] == name)
            .collect();
        assert_eq!(matches.len(), 2, "{queue}");
        for finding in matches {
            assert_eq!(finding["eligible"], false, "{finding}");
            assert!(field(finding, "reason").contains("ambiguous"), "{finding}");
            let id = field(finding, "id");
            cleanup(project.path(), &["--task", id], 2);
            cleanup(project.path(), &["--task", id, "--apply"], 2);
        }
        assert_eq!(
            fs::read(project.path().join("app.py")).unwrap(),
            source.as_bytes()
        );
    }
}

#[test]
fn rejects_an_incomplete_scan_before_editing() {
    let original = "import math\nprint('ready')\n";
    let project = project(original);
    let queue = cleanup(project.path(), &[], 0);
    let id = field(task(&queue, "SKY-U002", "math"), "id");
    fs::write(project.path().join("broken.py"), "def broken(:\n").unwrap();
    assert_eq!(cleanup(project.path(), &[], 2)["status"], "incomplete");
    cleanup(project.path(), &["--task", id, "--apply"], 2);
    assert_eq!(
        fs::read(project.path().join("app.py")).unwrap(),
        original.as_bytes()
    );
}

#[cfg(unix)]
#[test]
fn apply_runs_a_real_rescan_and_reports_skipped_tests() {
    let project = project("import math\nprint('ready')\n");
    let queue = cleanup(project.path(), &[], 0);
    let id = field(task(&queue, "SKY-U002", "math"), "id");
    let report = cleanup(project.path(), &["--task", id, "--apply"], 0);
    let execution = &report["execution"];
    assert_eq!(execution["status"], "applied_unverified");
    assert_eq!(execution["applied"], true);
    assert_eq!(execution["rolled_back"], false);
    assert_eq!(execution["tests"]["status"], "skipped");
    assert_eq!(execution["rescan"]["status"], "passed");
    assert_eq!(
        fs::read(project.path().join("app.py")).unwrap(),
        b"print('ready')\n"
    );
    let after = cleanup(project.path(), &[], 0);
    assert!(after["tasks"].as_array().unwrap().is_empty(), "{after}");
}

#[cfg(unix)]
#[test]
fn stale_preview_proof_cannot_overwrite_an_edited_source() {
    let original = b"import math\nprint('ready')\n";
    let project = project(std::str::from_utf8(original).unwrap());
    let queue = cleanup(project.path(), &[], 0);
    let id = field(task(&queue, "SKY-U002", "math"), "id");
    let preview = cleanup(project.path(), &["--task", id], 0);
    let proof = field(&preview["edit"], "proof");
    let path = project.path().join("app.py");
    cleanup(
        project.path(),
        &["--task", id, "--apply", "--proof", &"0".repeat(64)],
        2,
    );
    assert_eq!(fs::read(&path).unwrap(), original);

    let modified = b"import math\nprint('edited')\n";
    fs::write(&path, modified).unwrap();
    cleanup(
        project.path(),
        &["--task", id, "--apply", "--proof", proof],
        2,
    );
    assert_eq!(fs::read(path).unwrap(), modified);
}

#[cfg(unix)]
#[test]
fn stale_policy_and_unsafe_check_commands_are_rejected_before_apply() {
    let original = "import math\nprint('ready')\n";
    let project = project(original);
    let queue = cleanup(project.path(), &[], 0);
    let id = field(task(&queue, "SKY-U002", "math"), "id");
    let preview = cleanup(project.path(), &["--task", id], 0);
    let proof = field(&preview["edit"], "proof");
    cleanup(
        project.path(),
        &["--task", id, "--apply", "--test-cmd", "pytest; echo unsafe"],
        2,
    );
    assert_eq!(
        fs::read(project.path().join("app.py")).unwrap(),
        original.as_bytes()
    );

    fs::write(
        project.path().join("pyproject.toml"),
        "[tool.skylos]\nconfidence = 55\n",
    )
    .unwrap();
    cleanup(
        project.path(),
        &["--task", id, "--apply", "--proof", proof],
        2,
    );
    assert_eq!(
        fs::read(project.path().join("app.py")).unwrap(),
        original.as_bytes()
    );
}

#[cfg(unix)]
#[test]
fn preview_and_apply_preserve_linked_targets() {
    use std::os::unix::fs::symlink;

    let original = "import math\nprint('ready')\n";
    for kind in ["hardlink", "symlink"] {
        let project = project(original);
        let outside = tempfile::tempdir().unwrap();
        let alternate = outside.path().join("external.py");
        let source = project.path().join("app.py");
        let queue = cleanup(project.path(), &[], 0);
        let id = field(task(&queue, "SKY-U002", "math"), "id");
        match kind {
            "hardlink" => fs::hard_link(&source, &alternate).unwrap(),
            "symlink" => {
                fs::rename(&source, &alternate).unwrap();
                symlink(&alternate, &source).unwrap();
            }
            _ => unreachable!(),
        }
        cleanup(project.path(), &["--task", id], 2);
        cleanup(project.path(), &["--task", id, "--apply"], 2);
        assert_eq!(fs::read(&source).unwrap(), original.as_bytes());
        assert_eq!(fs::read(&alternate).unwrap(), original.as_bytes());
    }
}

#[cfg(unix)]
#[test]
fn ignored_local_provider_invalidates_preapproved_import_preview() {
    let original = "import math\nprint('ready')\n";
    let project = project(original);
    let path = project.path().join("app.py");
    fs::write(
        project.path().join("pyproject.toml"),
        "[tool.skylos]\nexclude = [\"private\"]\n",
    )
    .unwrap();
    let queue = cleanup(project.path(), &[], 0);
    let id = field(task(&queue, "SKY-U002", "math"), "id");
    let preview = cleanup(project.path(), &["--task", id], 0);
    let proof = field(&preview["edit"], "proof");

    let providers = project.path().join("private/providers");
    fs::create_dir_all(&providers).unwrap();
    fs::write(providers.join("math.py"), "print('local provider')\n").unwrap();
    let current = cleanup(project.path(), &[], 0);
    let finding = task(&current, "SKY-U002", "math");
    assert_eq!(finding["eligible"], false, "{finding}");
    assert!(field(finding, "reason").contains("shadow"), "{finding}");
    cleanup(
        project.path(),
        &["--task", id, "--apply", "--proof", proof],
        2,
    );
    assert_eq!(fs::read(path).unwrap(), original.as_bytes());
}

#[cfg(unix)]
fn check_command() -> String {
    let executable = std::env::current_exe().unwrap();
    format!(
        "\"{}\" --exact explicit_cleanup_check_fixture",
        executable.display()
    )
}

#[cfg(unix)]
fn junit_check_fixture() -> (tempfile::TempDir, String) {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::tempdir().unwrap();
    let launcher = directory.path().join("pytest");
    fs::write(
        &launcher,
        r#"#!/bin/sh
[ "$1" = '--junitxml' ] || exit 92
SKARMORY_TEST_CLEANUP_JUNIT="$2" "$SKARMORY_TEST_CLEANUP_BINARY" --exact explicit_cleanup_check_fixture || exit "$?"
cat > "$2" <<'XML'
<?xml version="1.0" encoding="utf-8"?>
<testsuites name="pytest tests"><testsuite name="pytest" errors="0" failures="0" skipped="0" tests="1"><testcase classname="cleanup" name="explicit_cleanup_check_fixture"/></testsuite></testsuites>
XML
"#,
    )
    .unwrap();
    fs::set_permissions(&launcher, fs::Permissions::from_mode(0o700)).unwrap();
    let command = format!("\"{}\"", launcher.display());
    (directory, command)
}

#[cfg(unix)]
#[test]
fn explicit_cleanup_check_fixture() {
    let Ok(action) = std::env::var("SKARMORY_TEST_CLEANUP_ACTION") else {
        return;
    };
    let expected = match action.as_str() {
        "partial" => "import json\r\nprint('ready')\r\n",
        "new-dependency-review" => "\r\ndef existing():\r\n    return 7\r\n\r\nprint('ready')\r\n",
        _ => "print('ready')\r\n",
    };
    assert_eq!(fs::read("app.py").unwrap(), expected.as_bytes());
    match action.as_str() {
        "fail" => std::process::exit(42),
        "edit-and-fail" => {
            fs::write("app.py", b"print('user change')\r\n").unwrap();
            std::process::exit(42);
        }
        "edit-neighbor-and-fail" => {
            fs::write("other.py", "VALUE = 2\n").unwrap();
        }
        "large-junit-failure" => {
            let row = "<testcase classname=\"test_many\" name=\"test_smoke\" time=\"0.000\" />";
            let cases = row.repeat(20_000);
            let receipt = format!(
                r#"<testsuites name="pytest tests"><testsuite name="pytest" errors="0" failures="1" skipped="0" tests="20001">{cases}<testcase name="test_failure"><failure message="assert False">assert False</failure></testcase></testsuite></testsuites>"#
            );
            assert!(receipt.len() > 1024 * 1024);
            fs::write(
                std::env::var_os("SKARMORY_TEST_CLEANUP_JUNIT").unwrap(),
                receipt,
            )
            .unwrap();
            std::process::exit(1);
        }
        "restore-source" => {
            fs::write("app.py", b"import math\r\nprint('ready')\r\n").unwrap();
        }
        "edit-source" => {
            fs::write("app.py", b"print('user change')\r\n").unwrap();
        }
        "change-newlines" => fs::write("app.py", b"print('ready')\n").unwrap(),
        "remove-source" => fs::remove_file("app.py").unwrap(),
        "change-policy" | "change-policy-and-fail" => {
            fs::write("pyproject.toml", "[tool.skylos]\nignore = [\"SKY-U002\"]\n").unwrap();
        }
        "add-provider" | "add-provider-and-fail" => {
            fs::write("math.py", "print('local provider')\n").unwrap();
        }
        "add-transitive-provider" => {
            fs::write("fnmatch.py", "print('local provider')\n").unwrap();
        }
        "change-dependencies" | "change-dependencies-and-fail" => {
            fs::write("requirements.txt", "flask\nrequests\n").unwrap();
        }
        "pass" | "partial" | "new-dependency-review" => {}
        _ => panic!("unknown fixture action {action}"),
    }
    if action.ends_with("-and-fail") {
        std::process::exit(42);
    }
}

#[cfg(unix)]
#[test]
fn a_returned_original_finding_fails_rescan_without_overwriting_the_source() {
    let original = "import math\r\nprint('ready')\r\n";
    let project = project(original);
    let queue = cleanup(project.path(), &[], 0);
    let id = field(task(&queue, "SKY-U002", "math"), "id");
    let report = cleanup_with_check(
        project.path(),
        &["--task", id, "--apply", "--test-cmd", &check_command()],
        1,
        Some("restore-source"),
    );
    let execution = &report["execution"];
    assert_eq!(execution["status"], "failed", "{execution}");
    assert_eq!(execution["tests"]["status"], "passed", "{execution}");
    assert_eq!(execution["rescan"]["status"], "failed", "{execution}");
    assert_eq!(execution["rescan"]["original_present"], true, "{execution}");
    assert_eq!(execution["rolled_back"], false, "{execution}");
    assert_eq!(
        fs::read(project.path().join("app.py")).unwrap(),
        original.as_bytes()
    );
}

#[cfg(unix)]
#[test]
fn a_passing_check_that_edits_or_removes_the_target_preserves_the_user_change() {
    let (_fixture, command) = junit_check_fixture();
    for (action, expected) in [
        ("edit-source", Some(b"print('user change')\r\n".as_slice())),
        ("remove-source", None),
    ] {
        let project = project("import math\r\nprint('ready')\r\n");
        fs::write(project.path().join("helper.py"), "print('untouched')\n").unwrap();
        let queue = cleanup(project.path(), &[], 0);
        let id = field(task(&queue, "SKY-U002", "math"), "id");
        let report = cleanup_with_check(
            project.path(),
            &["--task", id, "--apply", "--test-cmd", &command],
            2,
            Some(action),
        );
        let execution = &report["execution"];
        assert_eq!(execution["status"], "incomplete", "{execution}");
        assert_eq!(execution["tests"]["status"], "passed", "{execution}");
        assert_eq!(execution["tests"]["observed"], true, "{execution}");
        assert_eq!(execution["rescan"]["status"], "incomplete", "{execution}");
        assert_eq!(execution["rolled_back"], false, "{execution}");
        match expected {
            Some(bytes) => assert_eq!(fs::read(project.path().join("app.py")).unwrap(), bytes),
            None => assert!(!project.path().join("app.py").exists()),
        }
    }
}

#[cfg(unix)]
#[test]
fn a_passing_rescan_cannot_verify_changed_raw_source_bytes() {
    let project = project("import math\r\nprint('ready')\r\n");
    let queue = cleanup(project.path(), &[], 0);
    let id = field(task(&queue, "SKY-U002", "math"), "id");
    let (_fixture, command) = junit_check_fixture();
    let report = cleanup_with_check(
        project.path(),
        &["--task", id, "--apply", "--test-cmd", &command],
        2,
        Some("change-newlines"),
    );
    let execution = &report["execution"];
    assert_eq!(execution["status"], "incomplete", "{execution}");
    assert_eq!(execution["tests"]["status"], "passed", "{execution}");
    assert_eq!(execution["tests"]["observed"], true, "{execution}");
    assert_eq!(execution["rescan"]["status"], "passed", "{execution}");
    assert_eq!(execution["rescan"]["original_present"], false);
    assert_eq!(execution["rolled_back"], false, "{execution}");
    assert!(
        execution["limitations"]
            .as_array()
            .unwrap()
            .iter()
            .any(|line| {
                line.as_str()
                    .is_some_and(|message| message.to_lowercase().contains("source changed"))
            }),
        "{execution}"
    );
    assert_eq!(
        fs::read(project.path().join("app.py")).unwrap(),
        b"print('ready')\n"
    );
}

#[cfg(unix)]
#[test]
fn a_changed_dependency_manifest_invalidates_validation_and_rollback_proof() {
    let (_fixture, passing_command) = junit_check_fixture();
    let failing_command = check_command();
    for (action, command, exit, status, test_status) in [
        (
            "change-dependencies",
            &passing_command,
            2,
            "incomplete",
            "passed",
        ),
        (
            "change-dependencies-and-fail",
            &failing_command,
            1,
            "failed",
            "failed",
        ),
    ] {
        let project = project("import math\r\nprint('ready')\r\n");
        fs::write(
            project.path().join("pyproject.toml"),
            "[tool.skylos]\nquality_enabled = true\n",
        )
        .unwrap();
        let manifest = project.path().join("requirements.txt");
        fs::write(&manifest, "flask\n").unwrap();
        let queue = cleanup(project.path(), &[], 0);
        let id = field(task(&queue, "SKY-U002", "math"), "id");
        let report = cleanup_with_check(
            project.path(),
            &["--task", id, "--apply", "--test-cmd", command],
            exit,
            Some(action),
        );
        let execution = &report["execution"];
        assert_eq!(execution["status"], status, "{execution}");
        assert_eq!(execution["tests"]["status"], test_status, "{execution}");
        assert_eq!(execution["rescan"]["status"], "incomplete", "{execution}");
        assert_eq!(execution["rolled_back"], false, "{execution}");
        assert_eq!(
            fs::read(project.path().join("app.py")).unwrap(),
            b"print('ready')\r\n"
        );
        assert_eq!(fs::read(manifest).unwrap(), b"flask\nrequests\n");
    }
}

#[cfg(unix)]
#[test]
fn medium_review_findings_and_shifted_dead_code_do_not_trigger_rollback() {
    let project = project(concat!(
        "def obsolete():\r\n    import requests\r\n    return requests.__version__\r\n",
        "\r\ndef existing():\r\n    return 7\r\n\r\nprint('ready')\r\n",
    ));
    fs::write(
        project.path().join("pyproject.toml"),
        concat!(
            "[project]\nname = 'app'\nversion = '1.0.0'\n",
            "dependencies = ['flask', 'requests']\n[tool.skylos]\nquality_enabled = true\n",
        ),
    )
    .unwrap();
    let dependency_reviews = || -> Vec<String> {
        let output = skarmory(project.path())
            .args(["--profile", "skylos-0.3", "."])
            .output()
            .unwrap();
        assert!(output.status.success(), "{:?}", output);
        let report: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(report["coverage"]["status"], "complete", "{report}");
        report["quality"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| {
                assert_eq!(row["rule_id"], "SKY-U005", "{row}");
                assert_eq!(row["severity"], "MEDIUM", "{row}");
                field(row, "name").to_owned()
            })
            .collect()
    };
    assert_eq!(dependency_reviews(), ["flask"]);
    let queue = cleanup(project.path(), &[], 0);
    let id = field(task(&queue, "SKY-U001", "obsolete"), "id");
    let existing = task(&queue, "SKY-U001", "existing");
    let (_fixture, command) = junit_check_fixture();
    let report = cleanup_with_check(
        project.path(),
        &["--task", id, "--apply", "--test-cmd", &command],
        0,
        Some("new-dependency-review"),
    );
    let execution = &report["execution"];
    assert_eq!(execution["status"], "verified", "{execution}");
    assert_eq!(execution["rescan"]["status"], "passed", "{execution}");
    assert_eq!(execution["rescan"]["new_findings"], serde_json::json!([]));
    assert_eq!(execution["rolled_back"], false, "{execution}");
    assert_eq!(dependency_reviews(), ["flask", "requests"]);
    let after_queue = cleanup(project.path(), &[], 0);
    let shifted = task(&after_queue, "SKY-U001", "existing");
    assert_eq!(shifted["id"], existing["id"]);
    assert_ne!(shifted["line"], existing["line"]);
}

#[cfg(unix)]
#[test]
fn failed_explicit_check_rolls_back_exact_bytes_without_overwriting_user_edits() {
    use std::os::unix::fs::PermissionsExt;

    let original = "import math\r\nprint('ready')\r\n";
    for (action, rolled_back, expected) in [
        ("fail", true, original),
        ("edit-and-fail", false, "print('user change')\r\n"),
    ] {
        let project = project(original);
        let path = project.path().join("app.py");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
        let queue = cleanup(project.path(), &[], 0);
        let id = field(task(&queue, "SKY-U002", "math"), "id");
        let report = cleanup_with_check(
            project.path(),
            &["--task", id, "--apply", "--test-cmd", &check_command()],
            1,
            Some(action),
        );
        let execution = &report["execution"];
        assert_eq!(execution["status"], "failed");
        assert_eq!(execution["tests"]["status"], "failed");
        assert_eq!(execution["rolled_back"], rolled_back);
        assert_eq!(fs::read(&path).unwrap(), expected.as_bytes());
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o640
        );
    }
}

#[cfg(unix)]
#[test]
fn failed_check_rolls_back_only_the_target_after_an_unrelated_save() {
    let original = "import math\r\nprint('ready')\r\n";
    let project = project(original);
    let other = project.path().join("other.py");
    fs::write(&other, "VALUE = 1\n").unwrap();
    let queue = cleanup(project.path(), &[], 0);
    let id = field(task(&queue, "SKY-U002", "math"), "id");
    let report = cleanup_with_check(
        project.path(),
        &["--task", id, "--apply", "--test-cmd", &check_command()],
        1,
        Some("edit-neighbor-and-fail"),
    );
    let execution = &report["execution"];
    assert_eq!(execution["tests"]["status"], "failed", "{execution}");
    assert_eq!(execution["rescan"]["status"], "incomplete", "{execution}");
    assert_eq!(execution["rolled_back"], true, "{execution}");
    assert_eq!(
        fs::read(project.path().join("app.py")).unwrap(),
        original.as_bytes()
    );
    assert_eq!(fs::read_to_string(other).unwrap(), "VALUE = 2\n");
}

#[cfg(unix)]
#[test]
fn failed_check_with_large_pytest_junit_report_rolls_back() {
    let original = "import math\r\nprint('ready')\r\n";
    let project = project(original);
    let (_launcher, command) = junit_check_fixture();
    let queue = cleanup(project.path(), &[], 0);
    let id = field(task(&queue, "SKY-U002", "math"), "id");
    let report = cleanup_with_check(
        project.path(),
        &["--task", id, "--apply", "--test-cmd", &command],
        1,
        Some("large-junit-failure"),
    );
    let execution = &report["execution"];
    assert_eq!(execution["tests"]["status"], "failed", "{execution}");
    assert_eq!(execution["tests"]["count"], 20_001, "{execution}");
    assert_eq!(execution["rolled_back"], true, "{execution}");
    assert_eq!(
        fs::read(project.path().join("app.py")).unwrap(),
        original.as_bytes()
    );
}

#[cfg(unix)]
#[test]
fn policy_or_provider_mutations_during_a_check_leave_the_rescan_incomplete() {
    let (_fixture, command) = junit_check_fixture();
    for (action, changed_file, import) in [
        ("change-policy", "pyproject.toml", "math"),
        ("add-provider", "math.py", "math"),
        ("add-transitive-provider", "fnmatch.py", "pathlib"),
    ] {
        let project = project(&format!("import {import}\r\nprint('ready')\r\n"));
        let queue = cleanup(project.path(), &[], 0);
        let id = field(task(&queue, "SKY-U002", import), "id");
        let report = cleanup_with_check(
            project.path(),
            &["--task", id, "--apply", "--test-cmd", &command],
            2,
            Some(action),
        );
        let execution = &report["execution"];
        assert_eq!(execution["status"], "incomplete", "{execution}");
        assert_eq!(execution["tests"]["observed"], true, "{execution}");
        assert_eq!(execution["rescan"]["status"], "incomplete", "{execution}");
        assert_eq!(execution["rolled_back"], false, "{execution}");
        assert!(project.path().join(changed_file).exists());
    }
}

#[cfg(unix)]
#[test]
fn a_failed_check_does_not_roll_back_across_changed_dependencies() {
    for (action, changed_file, changed_contents) in [
        (
            "change-policy-and-fail",
            "pyproject.toml",
            "[tool.skylos]\nignore = [\"SKY-U002\"]\n",
        ),
        (
            "add-provider-and-fail",
            "math.py",
            "print('local provider')\n",
        ),
    ] {
        let project = project("import math\r\nprint('ready')\r\n");
        let queue = cleanup(project.path(), &[], 0);
        let id = field(task(&queue, "SKY-U002", "math"), "id");
        let report = cleanup_with_check(
            project.path(),
            &["--task", id, "--apply", "--test-cmd", &check_command()],
            1,
            Some(action),
        );
        let execution = &report["execution"];
        assert_eq!(execution["status"], "failed", "{execution}");
        assert_eq!(execution["tests"]["status"], "failed", "{execution}");
        assert_eq!(execution["applied"], true, "{execution}");
        assert_eq!(execution["rolled_back"], false, "{execution}");
        assert_eq!(
            fs::read(project.path().join("app.py")).unwrap(),
            b"print('ready')\r\n"
        );
        assert_eq!(
            fs::read(project.path().join(changed_file)).unwrap(),
            changed_contents.as_bytes()
        );
    }
}

#[cfg(unix)]
#[test]
fn observed_checks_and_comparable_rescan_verify_the_edit() {
    let (_fixture, command) = junit_check_fixture();
    let project = project("import math\r\nprint('ready')\r\n");
    let queue = cleanup(project.path(), &[], 0);
    let id = field(task(&queue, "SKY-U002", "math"), "id");
    let report = cleanup_with_check(
        project.path(),
        &["--task", id, "--apply", "--test-cmd", &command],
        0,
        Some("pass"),
    );
    let execution = &report["execution"];
    assert_eq!(execution["status"], "verified");
    assert_eq!(execution["tests"]["status"], "passed");
    assert_eq!(execution["tests"]["observed"], true);
    assert_eq!(execution["tests"]["evidence"], "pytest_junit");
    assert_eq!(
        execution["tests"].get("exercised_change"),
        Some(&Value::Null)
    );
    assert_eq!(execution["rescan"]["status"], "passed");
    assert!(
        execution["limitations"]
            .as_array()
            .unwrap()
            .iter()
            .any(|line| {
                line.as_str()
                    .is_some_and(|message| message.to_lowercase().contains("delet"))
            }),
        "{execution}"
    );
}

#[cfg(unix)]
#[test]
fn observed_tests_do_not_verify_a_same_line_import_rewrite() {
    let (_fixture, command) = junit_check_fixture();
    let project = project("import math, json\r\nprint('ready')\r\n");
    let queue = cleanup(project.path(), &[], 0);
    let id = field(task(&queue, "SKY-U002", "math"), "id");
    let report = cleanup_with_check(
        project.path(),
        &["--task", id, "--apply", "--test-cmd", &command],
        0,
        Some("partial"),
    );
    let execution = &report["execution"];
    assert_eq!(execution["status"], "applied_unverified", "{execution}");
    assert_eq!(execution["tests"]["status"], "passed");
    assert_eq!(execution["tests"]["observed"], true);
    assert_eq!(execution["rescan"]["status"], "passed");
}

#[cfg(unix)]
#[test]
fn an_opaque_zero_exit_check_does_not_verify_the_edit() {
    let project = project("import math\r\nprint('ready')\r\n");
    let queue = cleanup(project.path(), &[], 0);
    let id = field(task(&queue, "SKY-U002", "math"), "id");
    let report = cleanup_with_check(
        project.path(),
        &["--task", id, "--apply", "--test-cmd", &check_command()],
        0,
        Some("pass"),
    );
    let execution = &report["execution"];
    assert_eq!(execution["status"], "applied_unverified");
    assert_eq!(execution["rescan"]["status"], "passed");
    assert_eq!(execution["tests"]["observed"], false);
}

#[cfg(unix)]
#[test]
fn an_unavailable_explicit_check_leaves_verification_incomplete() {
    let project = project("import math\nprint('ready')\n");
    let queue = cleanup(project.path(), &[], 0);
    let id = field(task(&queue, "SKY-U002", "math"), "id");
    let nonexistent = project.path().join("missing-executable");
    let command = format!("\"{}\"", nonexistent.display());
    let report = cleanup(
        project.path(),
        &["--task", id, "--apply", "--test-cmd", &command],
        2,
    );
    let execution = &report["execution"];
    assert_eq!(execution["status"], "incomplete");
    assert_eq!(execution["tests"]["status"], "incomplete");
    assert_eq!(execution["tests"]["observed"], false);
    assert_eq!(execution["rescan"]["status"], "passed");
    assert_eq!(
        fs::read(project.path().join("app.py")).unwrap(),
        b"print('ready')\n"
    );
}

#[cfg(not(unix))]
#[test]
fn apply_reports_unavailable_guarded_writes_and_preserves_source() {
    let original = "import math\nprint('ready')\n";
    let project = project(original);
    let queue = cleanup(project.path(), &[], 0);
    let id = field(task(&queue, "SKY-U002", "math"), "id");
    cleanup(project.path(), &["--task", id, "--apply"], 2);
    assert_eq!(
        fs::read(project.path().join("app.py")).unwrap(),
        original.as_bytes()
    );
}
