use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use tempfile::TempDir;

#[test]
fn a_parse_failure_cannot_be_mistaken_for_a_clean_result() {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/invalid.py");
    let output = Command::new(env!("CARGO_BIN_EXE_skarmory"))
        .arg(fixture)
        .output()
        .expect("the CLI should start");

    assert_eq!(output.status.code(), Some(2));
    assert!(output.stderr.is_empty());
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["coverage"]["status"], "incomplete");
    assert_eq!(report["coverage"]["issues"][0]["kind"], "parse_error");
    assert_eq!(report["findings"], serde_json::json!([]));
}

#[test]
fn the_native_executable_exposes_its_version() {
    let output = Command::new(env!("CARGO_BIN_EXE_skarmory"))
        .arg("--version")
        .output()
        .expect("the CLI should start");

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        format!("skarmory {}", skarmory_core::VERSION),
    );
}

#[test]
fn phase0_report_and_timings_use_separate_streams() {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/invalid.py");
    let output = Command::new(env!("CARGO_BIN_EXE_skarmory"))
        .args(["--phase0", "--timings"])
        .arg(fixture)
        .output()
        .expect("the CLI should start");

    assert_eq!(output.status.code(), Some(2));
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["profile"], "phase0-python311-flat-application");
    let timings: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
    assert!(timings["analysis"]["discovery_ns"].is_u64());
}

#[test]
fn phase0_rejects_preview_options() {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/invalid.py");
    let output = Command::new(env!("CARGO_BIN_EXE_skarmory"))
        .args(["--phase0", "--workers", "2"])
        .arg(fixture)
        .output()
        .expect("the CLI should start");

    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("--workers"));
}

fn skylos_fixture() -> TempDir {
    let project = tempfile::tempdir().unwrap();
    fs::write(project.path().join(".skylos.toml"), "root = true\n").unwrap();
    fs::write(
        project.path().join("main.py"),
        "def kept():\n    pass\nkept()\n\ndef unused():\n    pass\n",
    )
    .unwrap();
    project
}

fn skylos(project: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_skarmory"))
        .current_dir(project)
        .env_remove("SKYLOS_CONFIG_FILE")
        .args(["--profile", "skylos", ".", "--no-grep-verify"])
        .args(args)
        .output()
        .expect("the CLI should start")
}

#[test]
fn skylos_report_exit_status_tracks_output_mode_and_completion() {
    let project = skylos_fixture();
    let listing = skylos(project.path(), &["--json"]);
    assert_eq!(listing.status.code(), Some(0));
    assert!(listing.stderr.is_empty());
    let listing: serde_json::Value = serde_json::from_slice(&listing.stdout).unwrap();
    assert!(!listing["unused_functions"].as_array().unwrap().is_empty());

    let strict = skylos(project.path(), &["--format", "json", "--strict"]);
    assert_eq!(strict.status.code(), Some(1));
    assert!(strict.stderr.is_empty());
    let strict: serde_json::Value = serde_json::from_slice(&strict.stdout).unwrap();
    assert_eq!(strict["unused_functions"], listing["unused_functions"]);
    assert_eq!(strict["gate"]["exit_code"], 1);
    assert_eq!(strict["gate"]["passed"], false);

    let redirected = skylos(
        project.path(),
        &["--json", "--strict", "--output", "report.json"],
    );
    assert_eq!(redirected.status.code(), Some(1));
    assert!(redirected.stdout.is_empty());
    let verdict = String::from_utf8(redirected.stderr).unwrap();
    assert_eq!(
        verdict,
        "Quality Gate: FAILED\n   • Strict mode: 1 issue found\n       main.py:5  unused function unused\n"
    );
    let written = fs::read(project.path().join("report.json")).unwrap();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&written).unwrap(),
        strict
    );

    fs::write(project.path().join("main.py"), "pass\n").unwrap();
    let clean = skylos(project.path(), &["--json", "--strict"]);
    assert_eq!(clean.status.code(), Some(0));
    let report: serde_json::Value = serde_json::from_slice(&clean.stdout).unwrap();
    assert_eq!(report["gate"]["passed"], true);

    fs::write(project.path().join("main.py"), "def broken(\n").unwrap();
    let incomplete = skylos(project.path(), &["--json", "--strict"]);
    assert_eq!(incomplete.status.code(), Some(2));
    let report: serde_json::Value = serde_json::from_slice(&incomplete.stdout).unwrap();
    assert!(!report["analysis_errors"].as_array().unwrap().is_empty());
    assert_eq!(report["gate"]["exit_code"], 2);
}

#[test]
fn skylos_writes_sarif_and_json_to_distinct_files() {
    let project = skylos_fixture();
    for _ in 0..2 {
        let output = skylos(
            project.path(),
            &["--json", "--output", "report.json", "--sarif"],
        );
        assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
        assert!(output.stdout.is_empty());
        let json: serde_json::Value =
            serde_json::from_slice(&fs::read(project.path().join("report.json")).unwrap()).unwrap();
        let sarif: serde_json::Value =
            serde_json::from_slice(&fs::read(project.path().join("skylos.sarif.json")).unwrap())
                .unwrap();
        assert_eq!(sarif["version"], "2.1.0");
        assert!(!sarif["runs"][0]["results"].as_array().unwrap().is_empty());
        assert!(!json["unused_functions"].as_array().unwrap().is_empty());
    }
}

#[test]
fn invalid_profile_options_and_report_paths_fail_before_emitting_json() {
    let project = skylos_fixture();
    for (args, message) in [
        (&["--source-root", "src"][..], "require the native profile"),
        (
            &["--output", "missing/report.json"][..],
            "failed to write report",
        ),
        (
            &["--output", "report.json", "--sarif", "./report.json"][..],
            "different output paths",
        ),
    ] {
        let output = skylos(project.path(), args);
        assert_eq!(output.status.code(), Some(2));
        assert!(output.stdout.is_empty());
        assert!(String::from_utf8_lossy(&output.stderr).contains(message));
    }
    let mixed = Command::new(env!("CARGO_BIN_EXE_skarmory"))
        .args(["--profile", "native", "--strict", "."])
        .output()
        .unwrap();
    assert_eq!(mixed.status.code(), Some(2));
    assert!(mixed.stdout.is_empty());
}
