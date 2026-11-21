use std::fs;
use std::path::Path;
use std::process::Command;

use serde_json::{Value, json};

fn scan(project: &Path) -> Value {
    fs::write(project.join(".skylos.toml"), "root = true\n").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_skarmory"))
        .current_dir(project)
        .env_remove("SKYLOS_CONFIG_FILE")
        .args(["--profile", "skylos", ".", "--json", "--no-grep-verify"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["coverage"]["status"], "complete", "{report}");
    report
}

#[test]
fn pytest_collects_effective_test_and_fixture_aliases() {
    let project = tempfile::tempdir().unwrap();
    fs::write(
        project.path().join("test_example.py"),
        r#"from pytest import fixture

def helper():
    return 7

@fixture
def supply():
    return helper()

data = supply

def check(data):
    assert data == 7

test_result = check

def test_rebound():
    pass

test_rebound = None
"#,
    )
    .unwrap();

    let report = scan(project.path());
    let findings: Vec<_> = report["unused_functions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["full_name"].as_str().unwrap())
        .collect();
    assert_eq!(findings, ["test_example.test_rebound"], "{report}");
    for name in ["test_example.check", "test_example.supply"] {
        assert!(
            report["dead_code_evidence"]
                .as_array()
                .unwrap()
                .iter()
                .any(|row| { row["full_name"] == name && row["kind"] == "pytest_root" }),
            "{report}"
        );
    }
    assert!(
        report["unused_variables"]
            .as_array()
            .unwrap()
            .iter()
            .all(|row| { row["name"] != "test_result" && row["name"] != "data" }),
        "{report}"
    );
}

#[test]
fn pytest_collects_inherited_tests_with_effective_class_fixtures() {
    let project = tempfile::tempdir().unwrap();
    fs::write(
        project.path().join("support.py"),
        r#"from pytest import fixture

def build():
    return 7

class Cases:
    @fixture
    def data(self):
        return build()

    def test_result(self, data):
        assert data == 7

    def test_shadowed(self):
        pass
"#,
    )
    .unwrap();
    fs::write(
        project.path().join("test_example.py"),
        r#"from support import Cases

class Intermediate(Cases):
    pass

class TestExample(Intermediate):
    test_shadowed = None
"#,
    )
    .unwrap();

    let report = scan(project.path());
    let findings: Vec<_> = report["unused_functions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["full_name"].as_str().unwrap())
        .collect();
    assert_eq!(findings, ["support.Cases.test_shadowed"], "{report}");
    assert_eq!(report["unused_classes"], json!([]), "{report}");
    for name in ["support.Cases.test_result", "support.Cases.data"] {
        assert!(
            report["dead_code_evidence"]
                .as_array()
                .unwrap()
                .iter()
                .any(|row| { row["full_name"] == name && row["kind"] == "pytest_root" }),
            "{report}"
        );
    }
}
