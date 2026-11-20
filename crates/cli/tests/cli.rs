use std::process::Command;

#[test]
fn a_scan_cannot_be_mistaken_for_a_clean_result() {
    let output = Command::new(env!("CARGO_BIN_EXE_skarmory"))
        .arg(".")
        .output()
        .expect("the CLI should start");

    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("not implemented"));
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
