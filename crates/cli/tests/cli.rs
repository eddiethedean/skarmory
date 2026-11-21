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

#[test]
fn skylos_retains_a_fixture_by_its_registered_name() {
    let project = skylos_fixture();
    fs::remove_file(project.path().join("main.py")).unwrap();
    fs::write(
        project.path().join("test_sample.py"),
        "from pytest import fixture\n\ndef helper():\n    return 1\n\n@fixture(name='resource')\ndef setup_resource():\n    return helper()\n\ndef test_resource(resource):\n    assert resource == 1\n",
    )
    .unwrap();

    let output = skylos(project.path(), &["--json", "--strict"]);
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(output.status.code(), Some(0), "{report}");
    assert_eq!(report["coverage"]["status"], "complete");
    assert_eq!(report["unused_functions"], serde_json::json!([]));
    assert!(
        report["dead_code_evidence"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| {
                item["full_name"] == "test_sample.setup_resource" && item["kind"] == "pytest_root"
            })
    );
}

#[test]
fn skylos_retains_an_imported_fixture_and_its_import() {
    let project = skylos_fixture();
    fs::remove_file(project.path().join("main.py")).unwrap();
    for (decorator, parameter) in [
        ("@fixture", "supplied"),
        ("@fixture(name='named')", "named"),
    ] {
        fs::write(
            project.path().join("support.py"),
            format!("from pytest import fixture\n\ndef helper(seed):\n    return seed\n\n{decorator}\ndef resource(seed):\n    return helper(seed)\n"),
        )
        .unwrap();
        for (filename, seed) in [("test_sample.py", 1), ("test_other.py", 2)] {
            fs::write(
                project.path().join(filename),
                format!("from pytest import fixture\nfrom support import resource as supplied\n\n@fixture\ndef seed():\n    return {seed}\n\ndef test_resource({parameter}):\n    assert {parameter} == {seed}\n"),
            )
            .unwrap();
        }
        let output = skylos(project.path(), &["--json", "--strict"]);
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(output.status.code(), Some(0), "{report}");
        assert_eq!(report["coverage"]["status"], "complete");
        assert_eq!(report["unused_imports"], serde_json::json!([]));
        assert_eq!(report["unused_functions"], serde_json::json!([]));
    }
}

#[test]
fn skylos_retains_an_imported_autouse_fixture_in_the_collecting_module() {
    let project = skylos_fixture();
    fs::remove_file(project.path().join("main.py")).unwrap();
    fs::write(
        project.path().join("support.py"),
        "from pytest import fixture\n\n@fixture(autouse=True)\ndef automatic(seed):\n    assert seed == 7\n",
    )
    .unwrap();
    fs::write(
        project.path().join("test_sample.py"),
        "from pytest import fixture\nfrom support import automatic as auto\n\n@fixture\ndef seed():\n    return 7\n\ndef test_resource():\n    pass\n",
    )
    .unwrap();
    let output = skylos(project.path(), &["--json", "--strict"]);
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(output.status.code(), Some(0), "{report}");
    assert_eq!(report["coverage"]["status"], "complete");
    assert_eq!(report["unused_imports"], serde_json::json!([]));
    assert_eq!(report["unused_functions"], serde_json::json!([]));

    fs::write(
        project.path().join("test_sample.py"),
        "def test_resource():\n    pass\n",
    )
    .unwrap();
    let output = skylos(project.path(), &["--json"]);
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(output.status.code(), Some(0), "{report}");
    assert_eq!(report["coverage"]["status"], "complete");
    assert_eq!(
        report["unused_functions"][0]["full_name"],
        "support.automatic"
    );
}

#[test]
fn skylos_retains_a_test_collected_through_an_import_alias() {
    let project = skylos_fixture();
    fs::remove_file(project.path().join("main.py")).unwrap();
    fs::write(
        project.path().join("support.py"),
        "def helper(seed):\n    return seed\n\ndef exported(seed):\n    assert helper(seed) == 7\n",
    )
    .unwrap();
    fs::write(
        project.path().join("test_sample.py"),
        "from pytest import fixture\nfrom support import exported as test_example\n\n@fixture\ndef seed():\n    return 7\n",
    )
    .unwrap();
    let output = skylos(project.path(), &["--json", "--strict"]);
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(output.status.code(), Some(0), "{report}");
    assert_eq!(report["coverage"]["status"], "complete");
    assert_eq!(report["unused_imports"], serde_json::json!([]));
    assert_eq!(report["unused_functions"], serde_json::json!([]));
}

#[test]
fn skylos_retains_a_collected_test_class_and_its_fixture() {
    let project = skylos_fixture();
    fs::remove_file(project.path().join("main.py")).unwrap();
    fs::write(
        project.path().join("test_sample.py"),
        "from pytest import fixture\n\ndef helper():\n    return 1\n\n@fixture\ndef resource():\n    return helper()\n\nclass TestExample:\n    def test_resource(self, resource):\n        assert resource == 1\n",
    )
    .unwrap();
    let output = skylos(project.path(), &["--json", "--strict"]);
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(output.status.code(), Some(0), "{report}");
    assert_eq!(report["coverage"]["status"], "complete");
    assert_eq!(report["unused_classes"], serde_json::json!([]));
    assert_eq!(report["unused_functions"], serde_json::json!([]));
}

#[test]
fn skylos_resolves_a_bound_pytest_autouse_flag() {
    let project = skylos_fixture();
    fs::remove_file(project.path().join("main.py")).unwrap();
    for (flag, expected) in [
        ("True", serde_json::json!([])),
        (
            "False",
            serde_json::json!(["test_sample.helper", "test_sample.resource"]),
        ),
    ] {
        fs::write(
            project.path().join("test_sample.py"),
            format!("from pytest import fixture\n\ndef helper():\n    return 1\n\nENABLED = {flag}\n\n@fixture(autouse=ENABLED)\ndef resource():\n    return helper()\n\ndef test_resource():\n    pass\n"),
        )
        .unwrap();
        let output = skylos(project.path(), &["--json"]);
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(output.status.code(), Some(0), "{report}");
        assert_eq!(report["coverage"]["status"], "complete");
        let names: Vec<_> = report["unused_functions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| &item["full_name"])
            .collect();
        assert_eq!(serde_json::json!(names), expected, "{report}");
    }
}

#[test]
fn skylos_reports_cycles_through_imported_initialization_helpers() {
    let project = skylos_fixture();
    fs::remove_file(project.path().join("main.py")).unwrap();
    fs::write(
        project.path().join("a.py"),
        "def used():\n    return 1\n\nfrom b import load\nload()\n\ndef used():\n    return 2\n",
    )
    .unwrap();
    fs::write(
        project.path().join("b.py"),
        "def load():\n    from a import used\n    used()\n",
    )
    .unwrap();
    let output = skylos(project.path(), &["--json", "--strict"]);
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(output.status.code(), Some(2), "{report}");
    assert_eq!(report["coverage"]["issues"][0]["kind"], "cyclic_imports");
    assert_eq!(report["unused_functions"], serde_json::json!([]));

    fs::write(
        project.path().join("b.py"),
        "def load():\n    from c import used\n    used()\n",
    )
    .unwrap();
    fs::write(project.path().join("c.py"), "def used():\n    return 1\n").unwrap();
    let output = skylos(project.path(), &["--json"]);
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(output.status.code(), Some(0), "{report}");
    assert_eq!(report["coverage"]["status"], "complete");
    assert!(
        report["unused_functions"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item["file"] == "a.py")
    );
}

#[test]
fn skylos_reports_cycles_through_synchronous_initialization_callbacks() {
    let project = skylos_fixture();
    fs::remove_file(project.path().join("main.py")).unwrap();
    fs::write(
        project.path().join("a.py"),
        "def used():\n    return 1\n\ndef callback():\n    from a import used\n    used()\n\nfrom b import invoke\ninvoke(callback)\n\ndef used():\n    return 2\n",
    )
    .unwrap();
    for body in [
        "    fn()\n",
        "    def nested():\n        fn()\n    nested()\n",
    ] {
        fs::write(
            project.path().join("b.py"),
            format!("def invoke(fn):\n{body}"),
        )
        .unwrap();
        let output = skylos(project.path(), &["--json", "--strict"]);
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(output.status.code(), Some(2), "{report}");
        assert_eq!(report["coverage"]["issues"][0]["kind"], "cyclic_imports");
        assert_eq!(report["unused_functions"], serde_json::json!([]));
    }

    fs::write(
        project.path().join("b.py"),
        "def invoke(fn):\n    def dormant():\n        fn()\n",
    )
    .unwrap();
    let output = skylos(project.path(), &["--json"]);
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(output.status.code(), Some(0), "{report}");
    assert_eq!(report["coverage"]["status"], "complete");
}

#[test]
fn skylos_resolves_relative_self_imports_at_initialization_time() {
    let project = skylos_fixture();
    fs::remove_file(project.path().join("main.py")).unwrap();
    let package = project.path().join("pkg");
    fs::create_dir(&package).unwrap();
    fs::write(
        package.join("__init__.py"),
        "def used():\n    return 1\n\nfrom . import used as previous\nprevious()\n\ndef used():\n    return 2\n",
    )
    .unwrap();
    let output = skylos(project.path(), &["--json"]);
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(output.status.code(), Some(0), "{report}");
    assert_eq!(report["coverage"]["status"], "complete");
    let findings = report["unused_functions"].as_array().unwrap();
    assert_eq!(findings.len(), 1, "{report}");
    assert_eq!(findings[0]["line"], 7, "{report}");
}

#[test]
fn skylos_reports_a_missing_member_even_when_the_import_binding_is_unread() {
    let project = skylos_fixture();
    fs::remove_file(project.path().join("main.py")).unwrap();
    let package = project.path().join("pkg");
    fs::create_dir(&package).unwrap();
    fs::write(package.join("__init__.py"), "").unwrap();
    fs::write(package.join("main.py"), "from . import unavailable\n").unwrap();

    let output = skylos(project.path(), &["--json", "--strict"]);
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(output.status.code(), Some(2), "{report}");
    assert_eq!(report["coverage"]["issues"][0]["kind"], "unresolved_import");
    assert_eq!(report["unused_imports"], serde_json::json!([]));

    fs::write(package.join("unavailable.py"), "pass\n").unwrap();
    fs::write(
        package.join("main.py"),
        "from . import unavailable, __path__ as imported_path\n",
    )
    .unwrap();
    let output = skylos(project.path(), &["--json"]);
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(output.status.code(), Some(0), "{report}");
    assert_eq!(report["coverage"]["status"], "complete");
    let names: Vec<_> = report["unused_imports"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["full_name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["pkg.unavailable", "pkg.__path__"], "{report}");
}

#[test]
fn skylos_reports_import_cycles_as_incomplete() {
    let project = skylos_fixture();
    fs::remove_file(project.path().join("main.py")).unwrap();
    fs::write(project.path().join("b.py"), "from a import used\nused()\n").unwrap();
    for import in ["import b", "def load():\n    import b\n\nload()"] {
        fs::write(
            project.path().join("a.py"),
            format!("def used():\n    return 1\n\n{import}\n\ndef used():\n    return 2\n"),
        )
        .unwrap();
        let output = skylos(project.path(), &["--json", "--strict"]);
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(output.status.code(), Some(2), "{report}");
        assert_eq!(report["coverage"]["status"], "incomplete");
        assert_eq!(report["coverage"]["issues"][0]["kind"], "cyclic_imports");
        assert_eq!(report["unused_functions"], serde_json::json!([]));
    }

    fs::write(
        project.path().join("a.py"),
        "def used():\n    return 1\n\ndef used():\n    return 2\n",
    )
    .unwrap();
    let output = skylos(project.path(), &["--json"]);
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(output.status.code(), Some(0), "{report}");
    assert_eq!(report["coverage"]["status"], "complete");
    let findings = report["unused_functions"].as_array().unwrap();
    assert_eq!(findings.len(), 1, "{report}");
    assert_eq!(findings[0]["line"], 1);

    fs::remove_file(project.path().join("a.py")).unwrap();
    fs::remove_file(project.path().join("b.py")).unwrap();
    let package = project.path().join("pkg");
    fs::create_dir(&package).unwrap();
    fs::write(
        package.join("__init__.py"),
        "from .child import used as used\n",
    )
    .unwrap();
    fs::write(
        package.join("child.py"),
        "from . import helper as dependency\n\ndef used():\n    return dependency.value()\n",
    )
    .unwrap();
    fs::write(package.join("helper.py"), "def value():\n    return 1\n").unwrap();
    let output = skylos(project.path(), &["--json"]);
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(output.status.code(), Some(0), "{report}");
    assert_eq!(report["coverage"]["status"], "complete");
    assert_eq!(report["unused_functions"], serde_json::json!([]));
}

#[test]
fn skylos_distinguishes_generated_markers_from_ordinary_comments() {
    let project = skylos_fixture();
    fs::write(
        project.path().join("main.py"),
        "# This file processes generated values.\nfrom generated import helper\nhelper()\n\ndef orphan():\n    pass\n",
    )
    .unwrap();
    fs::write(
        project.path().join("generated.py"),
        "# This file was automatically generated.\ndef helper():\n    pass\n\ndef spare():\n    pass\n",
    )
    .unwrap();
    for (args, expected) in [
        (&["--json"][..], vec!["main.orphan"]),
        (
            &["--json", "--include-generated"][..],
            vec!["generated.spare", "main.orphan"],
        ),
    ] {
        let output = skylos(project.path(), args);
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(output.status.code(), Some(0), "{report}");
        assert_eq!(report["coverage"]["status"], "complete");
        assert_eq!(
            report["selected_paths"],
            serde_json::json!(["generated.py", "main.py"])
        );
        let names: Vec<_> = report["unused_functions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|finding| finding["full_name"].as_str().unwrap())
            .collect();
        assert_eq!(names, expected);
    }
}

#[test]
fn skylos_keeps_descendants_excluded_when_an_unrelated_include_is_present() {
    let project = skylos_fixture();
    let excluded = project.path().join("generated/sub");
    fs::create_dir_all(&excluded).unwrap();
    fs::write(excluded.join("omitted.py"), "def omitted():\n    pass\n").unwrap();
    fs::write(excluded.join("retained.py"), "def retained():\n    pass\n").unwrap();
    for (include, paths) in [
        ("unrelated", serde_json::json!(["main.py"])),
        (
            "generated/sub/retained.py",
            serde_json::json!(["generated/sub/retained.py", "main.py"]),
        ),
    ] {
        let output = skylos(
            project.path(),
            &[
                "--json",
                "--exclude",
                "generated/sub",
                "--include-folder",
                include,
            ],
        );
        assert_eq!(output.status.code(), Some(0));
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(report["selected_paths"], paths, "{report}");
    }
}
