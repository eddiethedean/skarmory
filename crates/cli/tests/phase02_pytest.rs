use std::fs;
use std::path::Path;
use std::process::Command;

use serde_json::{Value, json};

fn scan_with_status(project: &Path, exit_code: i32, coverage: &str) -> Value {
    fs::write(project.join(".skylos.toml"), "root = true\n").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_skarmory"))
        .current_dir(project)
        .env_remove("SKYLOS_CONFIG_FILE")
        .args(["--profile", "skylos", ".", "--json", "--no-grep-verify"])
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(exit_code),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["coverage"]["status"], coverage, "{report}");
    report
}

fn scan(project: &Path) -> Value {
    scan_with_status(project, 0, "complete")
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

#[test]
fn pytest_collects_module_qualified_test_and_fixture_aliases() {
    let project = tempfile::tempdir().unwrap();
    let package = project.path().join("pkg");
    fs::create_dir(&package).unwrap();
    fs::write(package.join("__init__.py"), "").unwrap();
    fs::write(
        package.join("support.py"),
        r#"from pytest import fixture

def worker():
    return 1

@fixture
def supply():
    return worker()

def check(data):
    assert data == 1
"#,
    )
    .unwrap();
    for (import, module) in [
        ("import pkg.support as support", "support"),
        ("import pkg.support", "pkg.support"),
        ("from pkg import support", "support"),
    ] {
        fs::write(
            project.path().join("test_example.py"),
            format!("{import}\ndata = {module}.supply\ntest_result = {module}.check\n"),
        )
        .unwrap();
        let report = scan(project.path());
        assert_eq!(report["unused_functions"], json!([]), "{report}");
        assert_eq!(report["unused_variables"], json!([]), "{report}");
        assert_eq!(report["unused_imports"], json!([]), "{report}");
        for name in ["pkg.support.check", "pkg.support.supply"] {
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
}

#[test]
fn pytest_reports_unresolved_collection_for_multiple_bases() {
    let project = tempfile::tempdir().unwrap();
    fs::write(
        project.path().join("test_example.py"),
        r#"def worker():
    return 1

class Base:
    def test_result(self):
        assert worker() == 1

class Mixin:
    pass

class TestSuite(Base, Mixin):
    pass
"#,
    )
    .unwrap();
    let report = scan_with_status(project.path(), 2, "incomplete");
    assert!(
        report["analysis_errors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|issue| {
                issue["kind"] == "unresolved_pytest_class" && issue["file"] == "test_example.py"
            }),
        "{report}"
    );
    assert_eq!(report["unused_functions"], json!([]), "{report}");
}
