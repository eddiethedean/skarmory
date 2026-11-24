use std::{fs, path::Path, process::Command};

use serde_json::Value;

fn write(root: &Path, name: &str, source: &str) {
    let path = root.join(name);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, source).unwrap();
}

fn scan(root: &Path, expected: i32, args: &[&str]) -> Value {
    let output = Command::new(env!("CARGO_BIN_EXE_skarmory"))
        .current_dir(root)
        .env_remove("SKYLOS_CONFIG_FILE")
        .env_remove("DJANGO_SETTINGS_MODULE")
        .args([
            ".",
            "--profile",
            "skylos-0.3",
            "--confidence",
            "0",
            "--no-grep-verify",
        ])
        .args(args)
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(expected),
        "{}\n{}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout),
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn functions(report: &Value) -> Vec<&str> {
    report["unused_functions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|function| function["name"].as_str().unwrap())
        .collect()
}

#[test]
fn rust_reachability_uses_only_reached_owners() {
    let tree = tempfile::tempdir().unwrap();
    let root = tree.path();
    let cycle = "fn alpha() { beta(); }\nfn beta() { alpha(); }\n";
    write(root, "src/main.rs", &format!("{cycle}fn main() {{}}\n"));
    let report = scan(root, 0, &[]);
    assert_eq!(report["analysis_summary"]["languages"]["Rust"], 1);
    assert_eq!(
        report["analysis_summary"]["frontend_analysis"]["Rust"],
        "complete"
    );
    assert_eq!(functions(&report), ["alpha", "beta"]);

    write(
        root,
        "src/main.rs",
        &format!("{cycle}fn main() {{ let run = alpha; run(); }}\n"),
    );
    let report = scan(root, 0, &[]);
    assert!(functions(&report).is_empty());

    write(
        root,
        "src/main.rs",
        &format!("external_macro!();\n{cycle}fn main() {{}}\n"),
    );
    let report = scan(root, 2, &[]);
    assert_eq!(
        report["analysis_summary"]["frontend_analysis"]["Rust"],
        "incomplete"
    );
    assert_eq!(
        report["coverage"]["issues"][0]["kind"],
        "unsupported_rust_item"
    );
    assert!(functions(&report).is_empty());
}

#[test]
fn rust_parse_failure_does_not_erase_independent_python_findings() {
    let tree = tempfile::tempdir().unwrap();
    let root = tree.path();
    write(root, "app.py", "def orphan():\n    return 1\n");
    write(root, "main.rs", "fn main() {\n    let value = ;\n}\n");
    let report = scan(root, 2, &[]);
    assert_eq!(report["coverage"]["issues"][0]["kind"], "rust_parse_error");
    assert_eq!(report["coverage"]["issues"][0]["line"], 2);
    assert_eq!(functions(&report), ["orphan"]);
}

#[test]
fn unsupported_frontends_have_independent_coverage() {
    let tree = tempfile::tempdir().unwrap();
    let root = tree.path();
    write(root, "main.rs", "fn main() {}\nfn orphan() {}\n");
    write(root, "sample.dart", "void main() {}\n");
    let report = scan(root, 2, &[]);
    assert_eq!(
        report["coverage"]["issues"][0]["kind"],
        "unsupported_frontend"
    );
    assert_eq!(report["coverage"]["issues"][0]["path"], "sample.dart");
    assert_eq!(
        report["analysis_summary"]["frontend_analysis"]["Dart"],
        "incomplete"
    );
    assert_eq!(
        report["analysis_summary"]["frontend_analysis"]["Rust"],
        "complete"
    );
    assert_eq!(functions(&report), ["orphan"]);

    let cache = tempfile::tempdir().unwrap();
    let args = [
        "--cache-dir",
        cache.path().to_str().unwrap(),
        "--workers",
        "4",
    ];
    let _ = scan(root, 2, &args);
    let mut cached = scan(root, 2, &args);
    assert_eq!(cached["cache"]["hits"], 1);
    cached.as_object_mut().unwrap().remove("cache");
    assert_eq!(cached, report);
}

#[test]
fn unresolved_robot_libraries_suspend_python_non_use_proofs() {
    let tree = tempfile::tempdir().unwrap();
    let root = tree.path();
    write(root, "library.py", "def exposed():\n    return 1\n");
    write(
        root,
        "suite.robot",
        "*** Settings ***\nLibrary    library.py\n",
    );
    let report = scan(root, 2, &[]);
    let issues = report["coverage"]["issues"].as_array().unwrap();
    assert!(
        issues
            .iter()
            .any(|issue| issue["kind"] == "unsupported_robot_python_usage")
    );
    assert!(functions(&report).is_empty());
}
