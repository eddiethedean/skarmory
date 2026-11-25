use std::os::unix::fs::PermissionsExt;

use super::*;

const TARGET: &str = "app.py";
const ORIGINAL: &str = "import math\nprint('ready')\n";
const REPLACEMENT: &str = "print('ready')\n";

pub(super) fn project() -> (tempfile::TempDir, Options, Scan, Candidate) {
    let project = tempfile::tempdir().unwrap();
    fs::write(project.path().join(TARGET), ORIGINAL).unwrap();
    let (options, before, candidate) = project_scan(project.path());
    (project, options, before, candidate)
}

fn project_scan(root: &Path) -> (Options, Scan, Candidate) {
    let options = Options {
        profile: Profile::SkylosPhase03,
        trace_file: EvidenceInput::Disabled,
        coverage_file: EvidenceInput::Disabled,
        ..Options::default()
    };
    let before = Scan::read(root, &options).unwrap();
    assert!(before.report.is_complete());
    let candidate = before
        .candidates(root)
        .into_iter()
        .find(|candidate| {
            candidate.task.file == TARGET
                && candidate.task.rule_id == "SKY-U002"
                && candidate.task.name == "math"
        })
        .unwrap();
    (options, before, candidate)
}

pub(super) fn apply_and_check<'a>(
    root: &Path,
    candidate: &'a Candidate,
) -> (Edit<'a>, CheckResult) {
    let target = Path::new(TARGET);
    let edit = candidate
        .edit
        .as_ref()
        .unwrap()
        .load(root, &candidate.task)
        .unwrap();
    assert_eq!(edit.replacement, REPLACEMENT);
    execution::replace(
        root,
        target,
        edit.source.as_bytes(),
        edit.replacement.as_bytes(),
    )
    .unwrap();

    let tools = tempfile::tempdir().unwrap();
    let launcher = tools.path().join("pytest");
    fs::write(
        &launcher,
        r#"#!/bin/sh
[ "$1" = '--junitxml' ] || exit 91
[ "$(cat app.py)" = "print('ready')" ] || exit 92
cat > "$2" <<'XML'
<?xml version="1.0" encoding="utf-8"?>
<testsuites name="pytest tests"><testsuite name="pytest" errors="0" failures="0" skipped="0" tests="1"><testcase name="patched_source" /></testsuite></testsuites>
XML
"#,
    )
    .unwrap();
    fs::set_permissions(&launcher, fs::Permissions::from_mode(0o700)).unwrap();
    let command = shlex::try_quote(launcher.to_str().unwrap()).unwrap();
    let tests = Check::parse(&command).unwrap().run(root);
    assert_eq!(tests.status, "passed", "{:?}", tests.error);
    assert!(tests.observed);
    assert_eq!(tests.evidence, "pytest_junit");
    (edit, tests)
}

#[test]
fn original_source_restored_during_rescan_is_a_failed_validation() {
    for restored in [ORIGINAL, "import math\nprint('user change', math.pi)\n"] {
        let (project, options, before, candidate) = project();
        let root = project.path();
        let (edit, tests) = apply_and_check(root, &candidate);

        fs::write(root.join(TARGET), restored).unwrap();
        let rescan = compare_rescan(root, &before, &candidate, &edit, Scan::read(root, &options));
        let result = finish_execution(root, &candidate, &edit, tests, rescan);
        assert_eq!(result.status, "failed");
        assert_eq!(result.tests.status, "passed");
        assert_eq!(result.rescan.status, "failed");
        assert_eq!(result.rescan.original_present, Some(true));
        assert!(!result.rolled_back);
        assert_eq!(fs::read(root.join(TARGET)).unwrap(), restored.as_bytes());
    }
}

#[test]
fn source_changed_after_comparison_preserves_passed_rescan_and_user_changes() {
    for new_source in [None, Some("print('user change')\n")] {
        let (project, options, before, candidate) = project();
        let root = project.path();
        let (edit, tests) = apply_and_check(root, &candidate);
        let rescan = compare_rescan(root, &before, &candidate, &edit, Scan::read(root, &options));
        assert_eq!(rescan.status, "passed", "{:?}", rescan.error);
        assert_eq!(rescan.original_present, Some(false));

        let path = root.join(TARGET);
        if let Some(source) = new_source {
            fs::write(&path, source).unwrap();
        } else {
            fs::remove_file(&path).unwrap();
        }
        let result = finish_execution(root, &candidate, &edit, tests, rescan);
        assert_eq!(result.status, "incomplete");
        assert_eq!(result.tests.status, "passed");
        assert_eq!(result.rescan.status, "passed");
        assert!(!result.rolled_back);
        assert!(
            result
                .limitations
                .last()
                .is_some_and(|message| message.contains("source changed")),
            "{:?}",
            result.limitations
        );
        if let Some(source) = new_source {
            assert_eq!(fs::read(path).unwrap(), source.as_bytes());
        } else {
            assert_eq!(
                fs::symlink_metadata(path).unwrap_err().kind(),
                std::io::ErrorKind::NotFound
            );
        }
    }
}

#[test]
#[ignore = "requires the pinned Skylos cleanup fixture and pytest environment"]
fn pinned_source_removed_after_comparison_preserves_receipts() {
    let root = PathBuf::from(
        std::env::var_os("SKARMORY_PHASE03_CLEANUP_FIXTURE").expect("pinned cleanup fixture"),
    );
    let check = std::env::var("SKARMORY_PHASE03_CLEANUP_CHECK").expect("pinned pytest command");
    let target = root.join(TARGET);
    assert_eq!(fs::read(&target).unwrap(), b"import math\nanswer = 42\n");
    assert_eq!(
        fs::read(root.join("test_app.py")).unwrap(),
        b"import app\n\ndef test_answer():\n    assert app.answer == 42\n"
    );

    let (options, before, candidate) = project_scan(&root);
    let edit = candidate
        .edit
        .as_ref()
        .unwrap()
        .load(&root, &candidate.task)
        .unwrap();
    assert_eq!(edit.replacement, "answer = 42\n");
    execution::replace(
        &root,
        Path::new(TARGET),
        edit.source.as_bytes(),
        edit.replacement.as_bytes(),
    )
    .unwrap();
    let tests = Check::parse(&check).unwrap().run(&root);
    assert_eq!(tests.status, "passed", "{:?}", tests.error);
    assert!(tests.observed);
    assert_eq!(tests.evidence, "pytest_junit");
    assert_eq!(serde_json::to_value(&tests).unwrap()["count"], 1);
    let rescan = compare_rescan(
        &root,
        &before,
        &candidate,
        &edit,
        Scan::read(&root, &options),
    );
    assert_eq!(rescan.status, "passed", "{:?}", rescan.error);

    // Match the reference mutation boundary: remove the source after comparing
    // the rescan with the baseline, then finalize the execution receipt.
    fs::remove_file(&target).unwrap();
    let receipt = finish_execution(&root, &candidate, &edit, tests, rescan);
    assert_eq!(receipt.status, "incomplete");
    assert_eq!(receipt.tests.status, "passed");
    assert_eq!(receipt.rescan.status, "passed");
    assert!(!receipt.rolled_back);
    assert_eq!(
        fs::symlink_metadata(target).unwrap_err().kind(),
        std::io::ErrorKind::NotFound
    );
    assert!(
        receipt
            .limitations
            .last()
            .unwrap()
            .contains("source changed")
    );
}
