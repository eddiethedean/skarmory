use std::path::Path;
use std::process::Command;

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
