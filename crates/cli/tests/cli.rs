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
fn skylos_retains_autouse_fixtures_imported_by_an_ancestor_conftest() {
    let project = skylos_fixture();
    fs::remove_file(project.path().join("main.py")).unwrap();
    let tests = project.path().join("tests");
    fs::create_dir(&tests).unwrap();
    fs::write(
        project.path().join("support.py"),
        "from pytest import fixture\n\n@fixture(autouse=True)\ndef automatic(seed):\n    assert seed == 7\n",
    )
    .unwrap();
    fs::write(
        tests.join("conftest.py"),
        "from support import automatic as active\n",
    )
    .unwrap();
    fs::write(
        tests.join("test_sample.py"),
        "from pytest import fixture\n\n@fixture\ndef seed():\n    return 7\n\ndef test_resource():\n    pass\n",
    )
    .unwrap();
    let output = skylos(project.path(), &["--json", "--strict"]);
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(output.status.code(), Some(0), "{report}");
    assert_eq!(report["coverage"]["status"], "complete");
    assert_eq!(report["unused_imports"], serde_json::json!([]));
    assert_eq!(report["unused_functions"], serde_json::json!([]));

    fs::rename(
        tests.join("test_sample.py"),
        project.path().join("test_sample.py"),
    )
    .unwrap();
    let output = skylos(project.path(), &["--json"]);
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(output.status.code(), Some(0), "{report}");
    assert_eq!(
        report["unused_imports"][0]["full_name"],
        "support.automatic"
    );
    assert_eq!(
        report["unused_functions"][0]["full_name"],
        "support.automatic"
    );
}

#[test]
fn skylos_selects_the_closest_pytest_fixture_and_can_request_its_parent() {
    let project = skylos_fixture();
    fs::remove_file(project.path().join("main.py")).unwrap();
    let tests = project.path().join("tests");
    fs::create_dir(&tests).unwrap();
    fs::write(tests.join("conftest.py"), "from support import resource\n").unwrap();
    for (decorator, parameters) in [("fixture", "resource"), ("fixture(autouse=True)", "")] {
        fs::write(
            project.path().join("support.py"),
            format!("from pytest import fixture\nfrom uuid import uuid4\n\n@{decorator}\ndef resource():\n    return uuid4()\n"),
        )
        .unwrap();
        fs::write(
            tests.join("test_sample.py"),
            format!("from pytest import fixture\n\n@fixture\ndef resource():\n    return 7\n\ndef test_resource({parameters}):\n    pass\n"),
        )
        .unwrap();
        let output = skylos(project.path(), &["--json"]);
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(output.status.code(), Some(0), "{report}");
        assert_eq!(report["coverage"]["status"], "complete");
        assert_eq!(
            report["unused_functions"].as_array().unwrap().len(),
            1,
            "{report}"
        );
        assert_eq!(
            report["unused_functions"][0]["full_name"],
            "support.resource"
        );
        assert_eq!(report["unused_imports"][0]["full_name"], "support.resource");
    }

    fs::write(
        project.path().join("support.py"),
        "from pytest import fixture\n\n@fixture\ndef resource():\n    return 3\n",
    )
    .unwrap();
    fs::write(
        tests.join("test_sample.py"),
        "from pytest import fixture\n\n@fixture\ndef resource(resource):\n    return resource + 4\n\ndef test_resource(resource):\n    assert resource == 7\n",
    )
    .unwrap();
    let output = skylos(project.path(), &["--json", "--strict"]);
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(output.status.code(), Some(0), "{report}");
    assert_eq!(report["unused_functions"], serde_json::json!([]));
    assert_eq!(report["unused_imports"], serde_json::json!([]));
}

#[test]
fn skylos_retains_an_indirectly_requested_ancestor_fixture() {
    let project = skylos_fixture();
    fs::remove_file(project.path().join("main.py")).unwrap();
    fs::write(
        project.path().join("conftest.py"),
        "from pytest import fixture\n\ndef build():\n    return 3\n\n@fixture\ndef resource():\n    return build()\n",
    )
    .unwrap();
    fs::write(
        project.path().join("test_sample.py"),
        "from pytest import fixture\n\n@fixture\ndef wrapper(resource):\n    return resource + 4\n\n@fixture\ndef resource(wrapper):\n    return wrapper\n\ndef test_resource(resource):\n    assert resource == 7\n",
    )
    .unwrap();
    let output = skylos(project.path(), &["--json", "--strict"]);
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(output.status.code(), Some(0), "{report}");
    assert_eq!(report["coverage"]["status"], "complete");
    assert_eq!(report["unused_functions"], serde_json::json!([]));
}

#[test]
fn skylos_retains_ancestors_behind_conditional_fixture_overrides() {
    let project = skylos_fixture();
    fs::remove_file(project.path().join("main.py")).unwrap();
    fs::write(
        project.path().join("conftest.py"),
        "from pytest import fixture\n\ndef build():\n    return 3\n\n@fixture\ndef resource():\n    return build()\n",
    )
    .unwrap();
    fs::write(
        project.path().join("test_sample.py"),
        "from os import name\nfrom pytest import fixture\n\nif name == 'nt':\n    @fixture\n    def resource():\n        return 3\n\ndef test_resource(resource):\n    assert resource == 3\n",
    )
    .unwrap();
    let output = skylos(project.path(), &["--json", "--strict"]);
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(output.status.code(), Some(0), "{report}");
    assert_eq!(report["coverage"]["status"], "complete");
    assert_eq!(report["unused_functions"], serde_json::json!([]));
    for name in ["conftest.resource", "test_sample.resource"] {
        assert!(
            report["dead_code_evidence"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["full_name"] == name && item["certainty"] == "possible"),
            "{report}"
        );
    }
}

#[test]
fn skylos_retains_a_fixture_behind_a_dormant_conditional_rebinding() {
    let project = skylos_fixture();
    fs::remove_file(project.path().join("main.py")).unwrap();
    fs::write(
        project.path().join("test_sample.py"),
        "from pytest import fixture\n\nENABLED = False\n\n@fixture\ndef resource():\n    return 7\n\nif ENABLED:\n    def resource():\n        return 2\n\ndef test_resource(resource):\n    assert resource == 7\n",
    )
    .unwrap();
    let output = skylos(project.path(), &["--json"]);
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(output.status.code(), Some(0), "{report}");
    assert_eq!(report["coverage"]["status"], "complete");
    assert_eq!(
        report["unused_functions"].as_array().unwrap().len(),
        1,
        "{report}"
    );
    assert_eq!(report["unused_functions"][0]["line"], 10);
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
fn skylos_retains_an_imported_test_class_and_its_fixtures() {
    let project = skylos_fixture();
    fs::remove_file(project.path().join("main.py")).unwrap();
    fs::write(
        project.path().join("support.py"),
        "from pytest import fixture\n\ndef helper():\n    return 7\n\nclass Plain:\n    @fixture\n    def local(self):\n        return helper()\n\n    def test_resource(self, local, seed):\n        assert local == seed\n",
    )
    .unwrap();
    fs::write(
        project.path().join("test_sample.py"),
        "from pytest import fixture\nfrom support import Plain as TestImported\n\n@fixture\ndef seed():\n    return 7\n",
    )
    .unwrap();
    let output = skylos(project.path(), &["--json", "--strict"]);
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(output.status.code(), Some(0), "{report}");
    assert_eq!(report["coverage"]["status"], "complete");
    assert_eq!(report["unused_classes"], serde_json::json!([]));
    assert_eq!(report["unused_functions"], serde_json::json!([]));
    assert_eq!(report["unused_imports"], serde_json::json!([]));
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
fn skylos_reports_unavailable_names_in_a_literal_lazy_export() {
    let project = skylos_fixture();
    let package = project.path().join("pkg");
    fs::create_dir(&package).unwrap();
    fs::write(package.join("__init__.py"), "def found():\n    return 1\n\ndef __getattr__(name):\n    if name == 'available':\n        return found\n    raise AttributeError(name)\n").unwrap();
    fs::write(project.path().join("main.py"), "from pkg import missing\n").unwrap();
    let output = skylos(project.path(), &["--json", "--strict"]);
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(output.status.code(), Some(2), "{report}");
    assert_eq!(report["coverage"]["issues"][0]["kind"], "unresolved_import");
    assert_eq!(report["unused_imports"], serde_json::json!([]));

    fs::write(
        project.path().join("main.py"),
        "from pkg import available\navailable()\n",
    )
    .unwrap();
    let output = skylos(project.path(), &["--json", "--strict"]);
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(output.status.code(), Some(0), "{report}");
    assert_eq!(report["coverage"]["status"], "complete");
    assert_eq!(report["unused_functions"], serde_json::json!([]));
}

#[test]
fn skylos_accounts_for_imports_executed_by_the_selected_lazy_export_branch() {
    let project = skylos_fixture();
    fs::remove_file(project.path().join("main.py")).unwrap();
    let package = project.path().join("pkg");
    fs::create_dir(&package).unwrap();
    fs::write(package.join("__init__.py"), "def __getattr__(name):\n    if name == 'get':\n        from a import used\n        return used\n    if name == 'safe':\n        from .helper import used\n        return used\n    raise AttributeError(name)\n").unwrap();
    fs::write(package.join("helper.py"), "def used():\n    return 1\n").unwrap();
    for call in ["from pkg import get\nget()", "import pkg\npkg.get()"] {
        fs::write(
            project.path().join("a.py"),
            format!("def used():\n    return 1\n\n{call}\n\ndef used():\n    return 2\n"),
        )
        .unwrap();
        let output = skylos(project.path(), &["--json", "--strict"]);
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(output.status.code(), Some(2), "{report}");
        assert_eq!(report["coverage"]["issues"][0]["kind"], "cyclic_imports");
        assert_eq!(report["unused_functions"], serde_json::json!([]));
    }

    fs::write(
        project.path().join("a.py"),
        "from pkg import safe\nsafe()\n",
    )
    .unwrap();
    let output = skylos(project.path(), &["--json", "--strict"]);
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(output.status.code(), Some(0), "{report}");
    assert_eq!(report["coverage"]["status"], "complete");
    assert_eq!(report["unused_functions"], serde_json::json!([]));
}

#[test]
fn skylos_reports_missing_submodules_of_selected_packages() {
    let project = skylos_fixture();
    let package = project.path().join("pkg");
    fs::create_dir(&package).unwrap();
    fs::write(package.join("__init__.py"), "").unwrap();
    fs::write(
        project.path().join("main.py"),
        "import pkg.missing\nfrom pkg.another import some_name\nimport urllib.parse\n",
    )
    .unwrap();
    let output = skylos(project.path(), &["--json", "--strict"]);
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(output.status.code(), Some(2), "{report}");
    assert_eq!(report["coverage"]["status"], "incomplete");
    let issues = report["coverage"]["issues"].as_array().unwrap();
    let mut lines: Vec<_> = issues
        .iter()
        .map(|issue| {
            assert_eq!(issue["kind"], "unresolved_import");
            issue["line"].as_u64().unwrap()
        })
        .collect();
    lines.sort_unstable();
    assert_eq!(lines, [1, 2], "{report}");
    assert_eq!(report["unused_imports"], serde_json::json!([]));

    fs::write(package.join("missing.py"), "pass\n").unwrap();
    fs::write(package.join("another.py"), "some_name = 1\n").unwrap();
    let output = skylos(project.path(), &["--json"]);
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(output.status.code(), Some(0), "{report}");
    assert_eq!(report["coverage"]["status"], "complete");
    assert_eq!(report["unused_imports"].as_array().unwrap().len(), 3);
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

#[test]
fn skylos_prefers_a_lazy_export_until_the_same_named_child_is_attached() {
    let project = skylos_fixture();
    let package = project.path().join("pkg");
    fs::create_dir(&package).unwrap();
    fs::write(
        package.join("__init__.py"),
        "def __getattr__(name):\n    if name == 'plugin':\n        from . import actual\n        return actual\n    raise AttributeError(name)\n",
    )
    .unwrap();
    fs::write(package.join("actual.py"), "def run():\n    return 'lazy'\n").unwrap();
    fs::write(package.join("plugin.py"), "def run():\n    return 'disk'\n").unwrap();
    fs::write(
        project.path().join("loader.py"),
        "ATTACH = False\nif ATTACH:\n    import pkg.plugin\n",
    )
    .unwrap();
    for (setup, lookup, expected_unused) in [
        ("", "from pkg import plugin\nplugin.run()", "pkg.plugin.run"),
        ("", "import pkg\npkg.plugin.run()", "pkg.plugin.run"),
        (
            "",
            "from pkg import plugin\nplugin.run()\nimport pkg.plugin",
            "pkg.plugin.run",
        ),
        (
            "import pkg.plugin\n",
            "from pkg import plugin\nplugin.run()",
            "pkg.actual.run",
        ),
        ("import pkg.plugin\n", "pkg.plugin.run()", "pkg.actual.run"),
    ] {
        fs::write(project.path().join("main.py"), format!("{setup}{lookup}\n")).unwrap();
        let output = skylos(project.path(), &["--json"]);
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(output.status.code(), Some(0), "{report}");
        assert_eq!(report["coverage"]["status"], "complete", "{report}");
        let findings = report["unused_functions"].as_array().unwrap();
        assert_eq!(findings.len(), 1, "{report}");
        assert_eq!(findings[0]["full_name"], expected_unused, "{report}");
    }

    fs::write(project.path().join("loader.py"), "import pkg.plugin\n").unwrap();
    fs::write(
        project.path().join("main.py"),
        "from pkg import plugin\nplugin.run()\n",
    )
    .unwrap();
    let output = skylos(project.path(), &["--json"]);
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(output.status.code(), Some(2), "{report}");
    assert_eq!(report["coverage"]["status"], "incomplete");
    assert!(
        report["coverage"]["issues"]
            .as_array()
            .unwrap()
            .iter()
            .any(|issue| issue["kind"] == "ambiguous_binding")
    );
    assert_eq!(report["unused_functions"], serde_json::json!([]));
}

#[test]
fn skylos_uses_known_local_boolean_guards_for_imported_bindings() {
    let project = skylos_fixture();
    fs::write(
        project.path().join("main.py"),
        "from support import selected\n\ndef orphan():\n    return 1\n",
    )
    .unwrap();
    for enabled in ["False", "True"] {
        fs::write(
            project.path().join("support.py"),
            format!("USE_OPTIONAL = {enabled}\nselected = 1\nif USE_OPTIONAL:\n    selected = 2\n"),
        )
        .unwrap();
        let output = skylos(project.path(), &["--json", "--strict"]);
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(output.status.code(), Some(1), "{report}");
        assert_eq!(report["coverage"]["status"], "complete");
        assert_eq!(report["analysis_errors"], serde_json::json!([]));
        assert_eq!(report["unused_functions"][0]["full_name"], "main.orphan");
    }
}

#[test]
fn skylos_accounts_for_imports_in_chained_lazy_exports() {
    let project = skylos_fixture();
    let package = project.path().join("pkg");
    fs::create_dir(&package).unwrap();
    fs::write(package.join("__init__.py"), "").unwrap();
    fs::write(
        package.join("outer.py"),
        "from . import inner\n\ndef __getattr__(name):\n    if name == 'outer':\n        return inner.exported\n    raise AttributeError(name)\n",
    )
    .unwrap();
    fs::write(
        package.join("inner.py"),
        "def __getattr__(name):\n    if name == 'exported':\n        from .target import candidate\n        return candidate\n    raise AttributeError(name)\n",
    )
    .unwrap();
    let main = project.path().join("main.py");
    let reads = [
        "from pkg.outer import outer\nprint(outer)\n",
        "from pkg import outer\nprint(outer.outer)\n",
    ];
    for source in reads {
        fs::write(&main, source).unwrap();
        let output = skylos(project.path(), &["--json", "--strict"]);
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(output.status.code(), Some(2), "{report}");
        assert_eq!(report["coverage"]["status"], "incomplete");
        assert!(
            report["analysis_errors"]
                .as_array()
                .unwrap()
                .iter()
                .any(|issue| {
                    issue["kind"] == "unresolved_import"
                        && issue["file"] == "pkg/inner.py"
                        && issue["line"] == 3
                }),
            "{report}"
        );
    }

    fs::write(&main, "import pkg.outer\n").unwrap();
    let output = skylos(project.path(), &["--json"]);
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(output.status.code(), Some(0), "{report}");
    assert_eq!(report["coverage"]["status"], "complete");

    fs::write(
        package.join("target.py"),
        "def candidate():\n    return 1\n",
    )
    .unwrap();
    fs::write(&main, "from pkg.outer import outer\nouter()\n").unwrap();
    let output = skylos(project.path(), &["--json", "--strict"]);
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(output.status.code(), Some(0), "{report}");
    assert_eq!(report["coverage"]["status"], "complete");
    assert_eq!(report["unused_functions"], serde_json::json!([]));
}

#[test]
fn skylos_resolves_base_calls_when_a_subclass_definition_is_dormant() {
    let project = skylos_fixture();
    let base = "def observed():\n    print('base method ran')\n\nclass Base:\n    def __init__(self):\n        self.setup()\n\n    def setup(self):\n        observed()\n\n";
    let subclass =
        "if ENABLED:\n    class Extension(Base):\n        def setup(self):\n            pass\n";
    let nested_subclass = "if ENABLED:\n    class Container:\n        class Extension(Base):\n            def setup(self):\n                pass\n";
    for (flag, classes, complete) in [
        ("False", subclass, true),
        ("False", nested_subclass, true),
        ("True", subclass, false),
    ] {
        fs::write(
            project.path().join("main.py"),
            format!("{base}ENABLED = {flag}\n{classes}\nBase()\n"),
        )
        .unwrap();
        let output = skylos(project.path(), &["--json", "--strict"]);
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        if complete {
            assert_eq!(output.status.code(), Some(1), "{report}");
            assert_eq!(report["coverage"]["status"], "complete");
            assert_eq!(report["analysis_errors"], serde_json::json!([]));
            assert!(
                report["unused_functions"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|finding| finding["full_name"] != "main.observed")
            );
            assert!(
                report["unused_classes"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|finding| finding["name"] == "Extension")
            );
            assert!(report["dead_code_evidence"].as_array().unwrap().iter().any(
                |edge| edge["full_name"] == "main.Base.setup" && edge["kind"] == "direct_call"
            ));
        } else {
            assert_eq!(output.status.code(), Some(2), "{report}");
            assert_eq!(report["coverage"]["status"], "incomplete");
            assert!(
                report["analysis_errors"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|issue| issue["kind"] == "unresolved_call")
            );
        }
    }
}

#[test]
fn skylos_treats_static_and_non_receiver_self_as_ordinary_parameters() {
    let project = skylos_fixture();
    let source = r#"from builtins import staticmethod as as_static

def observed():
    print("receiver method ran")

class Receiver:
    def run(self):
        observed()

class Facade:
    def run(self):
        pass

"#;
    for (definition, call) in [
        (
            "    @as_static\n    def dispatch(self):",
            "Facade.dispatch(Receiver())",
        ),
        (
            "    def dispatch(instance, self):",
            "Facade().dispatch(Receiver())",
        ),
    ] {
        fs::write(
            project.path().join("main.py"),
            format!("{source}{definition}\n        self.run()\n\n{call}\n"),
        )
        .unwrap();
        let output = skylos(project.path(), &["--json", "--strict"]);
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(output.status.code(), Some(2), "{report}");
        assert_eq!(report["coverage"]["status"], "incomplete");
        assert_eq!(report["unused_functions"], serde_json::json!([]));
        assert!(
            report["analysis_errors"]
                .as_array()
                .unwrap()
                .iter()
                .any(|issue| issue["kind"] == "unresolved_call")
        );
        assert!(!report["dead_code_evidence"].as_array().unwrap().iter().any(
            |edge| edge["full_name"] == "main.Facade.run" && edge["kind"] == "direct_call"
        ));
    }
}

#[test]
fn skylos_preserves_a_positional_only_instance_receiver() {
    let project = skylos_fixture();
    fs::write(
        project.path().join("main.py"),
        r#"def identity(function):
    return function

def observed():
    print("receiver method ran")

class Receiver:
    def run(self):
        observed()

    @identity
    def dispatch(self, /):
        self.run()

Receiver().dispatch()
"#,
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
            .any(|edge| edge["full_name"] == "main.Receiver.run" && edge["kind"] == "direct_call")
    );
}

#[test]
fn skylos_checks_explicit_receivers_on_calls_through_the_class() {
    let project = skylos_fixture();
    let source = r#"def observed():
    print("receiver method ran")

class Receiver:
    def run(self):
        observed()

class Facade:
    def run(self):
        pass

    def dispatch(self):
        self.run()

"#;
    let coroutine_source = source.replace("def dispatch", "async def dispatch");
    for (source, call, complete) in [
        (source, "Facade.dispatch(Receiver())", false),
        (
            source,
            "dispatch = Facade.dispatch\ndispatch(Receiver())",
            false,
        ),
        (source, "Facade.dispatch(self=Receiver())", false),
        (source, "Facade.dispatch(Facade())", true),
        (source, "Facade.dispatch(self=Facade())", true),
        (source, "Facade().dispatch()", true),
        (source, "dispatch = Facade().dispatch\ndispatch()", true),
        (
            &coroutine_source,
            "from asyncio import create_task\ncreate_task(Facade.dispatch(Receiver()))",
            false,
        ),
        (
            &coroutine_source,
            "from asyncio import create_task\ncreate_task(Facade.dispatch(Facade()))",
            true,
        ),
    ] {
        fs::write(project.path().join("main.py"), format!("{source}{call}\n")).unwrap();
        let output = skylos(project.path(), &["--json", "--strict"]);
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        let has_facade_run = report["dead_code_evidence"]
            .as_array()
            .unwrap()
            .iter()
            .any(|edge| edge["full_name"] == "main.Facade.run" && edge["kind"] == "direct_call");
        assert_eq!(has_facade_run, complete, "{call}\n{report}");
        if complete {
            assert_eq!(output.status.code(), Some(1), "{report}");
            assert_eq!(report["coverage"]["status"], "complete");
            let unused: Vec<_> = report["unused_functions"]
                .as_array()
                .unwrap()
                .iter()
                .map(|row| row["full_name"].as_str().unwrap())
                .collect();
            assert_eq!(unused, ["main.observed", "main.Receiver.run"]);
        } else {
            assert_eq!(output.status.code(), Some(2), "{report}");
            assert_eq!(report["coverage"]["status"], "incomplete");
            assert_eq!(report["unused_functions"], serde_json::json!([]));
            assert!(
                report["analysis_errors"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|issue| issue["kind"] == "unresolved_call")
            );
        }
    }
}

#[test]
fn skylos_constructor_does_not_assume_the_result_of_custom_allocation() {
    let project = skylos_fixture();
    let source = r#"def observed():
    print("receiver method ran")

class Receiver:
    def run(self):
        observed()

class Facade:
    def __init__(self):
        pass

    def dispatch(self):
        self.run()

    def run(self):
        pass

"#;
    let custom = "    def __new__(cls):\n        return Receiver()\n\n";
    for (allocator, expected_exit) in [(custom, 2), ("", 1)] {
        fs::write(
            project.path().join("main.py"),
            format!("{source}{allocator}Facade.dispatch(Facade())\n"),
        )
        .unwrap();
        let output = skylos(project.path(), &["--json", "--strict"]);
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(output.status.code(), Some(expected_exit), "{report}");
        let evidence = report["dead_code_evidence"].as_array().unwrap();
        let has_edge = |target: &str, kind: &str| {
            evidence
                .iter()
                .any(|edge| edge["full_name"] == target && edge["kind"] == kind)
        };
        if expected_exit == 2 {
            assert_eq!(report["coverage"]["status"], "incomplete");
            assert_eq!(report["unused_functions"], serde_json::json!([]));
            assert!(
                report["analysis_errors"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|issue| issue["kind"] == "unresolved_constructor")
            );
            assert!(has_edge("main.Facade.__new__", "constructor"));
            assert!(!has_edge("main.Facade.__init__", "constructor"));
            assert!(!has_edge("main.Facade.run", "direct_call"));
        } else {
            assert_eq!(report["coverage"]["status"], "complete");
            assert!(has_edge("main.Facade.__init__", "constructor"));
            assert!(has_edge("main.Facade.run", "direct_call"));
        }
    }
}

#[test]
fn skylos_constructor_validates_the_receiver_of_an_aliased_initializer() {
    let project = skylos_fixture();
    let source = r#"def observed():
    print("receiver method ran")

class Facade:
    def dispatch(self):
        self.run()

    def run(self):
        pass

class Receiver:
    def run(self):
        observed()

"#;
    for (initializer, expected_exit) in [
        ("    __init__ = Facade.dispatch\n", 2),
        (
            "    def setup(self):\n        self.run()\n\n    __init__ = setup\n",
            1,
        ),
    ] {
        fs::write(
            project.path().join("main.py"),
            format!("{source}{initializer}\nReceiver()\n"),
        )
        .unwrap();
        let output = skylos(project.path(), &["--json", "--strict"]);
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(output.status.code(), Some(expected_exit), "{report}");
        let evidence = report["dead_code_evidence"].as_array().unwrap();
        assert!(!evidence.iter().any(
            |edge| edge["full_name"] == "main.Facade.run" && edge["kind"] == "direct_call"
        ));
        if expected_exit == 2 {
            assert_eq!(report["coverage"]["status"], "incomplete");
            assert_eq!(report["unused_functions"], serde_json::json!([]));
            assert!(
                report["analysis_errors"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|issue| issue["kind"] == "unresolved_call")
            );
        } else {
            assert_eq!(report["coverage"]["status"], "complete");
            assert!(
                evidence
                    .iter()
                    .any(|edge| edge["full_name"] == "main.Receiver.setup"
                        && edge["kind"] == "constructor")
            );
            assert!(
                evidence
                    .iter()
                    .any(|edge| edge["full_name"] == "main.Receiver.run"
                        && edge["kind"] == "direct_call")
            );
        }
    }
}

#[test]
fn skylos_evaluates_a_lazy_base_only_for_an_executed_class_definition() {
    let project = skylos_fixture();
    fs::write(
        project.path().join("support.py"),
        "def make_base():\n    return object\n\ndef __getattr__(name):\n    if name == 'Base':\n        return make_base()\n    raise AttributeError(name)\n",
    )
    .unwrap();
    let main = project.path().join("main.py");
    for (enabled, selector, exit, coverage) in [
        ("False", "", 1, "complete"),
        (
            "False",
            "\n[[dead_code.entrypoints]]\nname = 'unmatched'\nmodule = 'main'\n",
            1,
            "complete",
        ),
        ("True", "", 2, "incomplete"),
    ] {
        fs::write(
            project.path().join(".skylos.toml"),
            format!("root = true\n{selector}"),
        )
        .unwrap();
        fs::write(
            &main,
            format!(
                "import support\nENABLED = {enabled}\nif ENABLED:\n    class Unused(support.Base):\n        pass\n"
            ),
        )
        .unwrap();
        let output = skylos(project.path(), &["--json", "--strict"]);
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(output.status.code(), Some(exit), "{report}");
        assert_eq!(report["coverage"]["status"], coverage);
        if enabled == "False" {
            assert_eq!(report["analysis_errors"], serde_json::json!([]), "{report}");
            assert_eq!(report["unused_classes"][0]["full_name"], "main.Unused");
            assert_eq!(
                report["unused_functions"][0]["full_name"],
                "support.make_base"
            );
        } else {
            assert!(
                report["analysis_errors"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|issue| {
                        issue["kind"] == "unresolved_lazy_export" && issue["file"] == "support.py"
                    }),
                "{report}"
            );
            assert_eq!(report["unused_classes"], serde_json::json!([]));
            assert_eq!(report["unused_functions"], serde_json::json!([]));
        }
    }
}

#[test]
fn skylos_reports_unmodeled_lazy_return_evaluation_only_when_accessed() {
    let project = skylos_fixture();
    fs::write(
        project.path().join("support.py"),
        "def make():\n    return 1\n\ndef __getattr__(name):\n    if name == 'example':\n        return make()\n    if name == 'invalid':\n        return missing\n    if name == 'inert':\n        return False and make()\n    raise AttributeError(name)\n",
    )
    .unwrap();
    let main = project.path().join("main.py");
    for (code, line) in [
        ("from support import example\n", 6),
        ("import support\nprint(support.example)\n", 6),
        ("from support import invalid\n", 8),
    ] {
        fs::write(&main, code).unwrap();
        let output = skylos(project.path(), &["--json", "--strict"]);
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(output.status.code(), Some(2), "{report}");
        assert_eq!(report["coverage"]["status"], "incomplete");
        assert!(
            report["analysis_errors"]
                .as_array()
                .unwrap()
                .iter()
                .any(|issue| {
                    issue["kind"] == "unresolved_lazy_export"
                        && issue["file"] == "support.py"
                        && issue["line"] == line
                }),
            "{report}"
        );
        assert_eq!(report["unused_functions"], serde_json::json!([]));
    }

    for code in [
        "import support\n",
        "from support import inert\n",
        "from __future__ import annotations\nimport support\nvalue: support.example\n",
    ] {
        fs::write(&main, code).unwrap();
        let output = skylos(project.path(), &["--json", "--strict"]);
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(output.status.code(), Some(1), "{report}");
        assert_eq!(report["coverage"]["status"], "complete");
        assert_eq!(report["unused_functions"][0]["full_name"], "support.make");
    }
}

#[test]
fn skylos_reports_a_failed_lazy_import_in_an_evaluated_decorator() {
    let project = skylos_fixture();
    let package = project.path().join("pkg");
    fs::create_dir(&package).unwrap();
    fs::write(
        package.join("__init__.py"),
        "def identity(func):\n    return func\n\ndef __getattr__(name):\n    if name == 'deco':\n        from .absent import _\n        return identity\n    raise AttributeError(name)\n",
    )
    .unwrap();
    let main = project.path().join("main.py");
    let active = "import pkg\n\n@pkg.deco\ndef called():\n    pass\n\ncalled()\n";
    fs::write(&main, active).unwrap();
    let output = skylos(project.path(), &["--json", "--strict"]);
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(output.status.code(), Some(2), "{report}");
    assert_eq!(report["coverage"]["status"], "incomplete");
    assert!(
        report["analysis_errors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|issue| {
                issue["kind"] == "unresolved_import"
                    && issue["file"] == "pkg/__init__.py"
                    && issue["line"] == 6
            }),
        "{report}"
    );

    fs::write(
        &main,
        "import pkg\nENABLED = False\nif ENABLED:\n    @pkg.deco\n    def dormant():\n        pass\n",
    )
    .unwrap();
    let output = skylos(project.path(), &["--json", "--strict"]);
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(output.status.code(), Some(1), "{report}");
    assert_eq!(report["coverage"]["status"], "complete");
    assert_eq!(report["analysis_errors"], serde_json::json!([]));

    fs::write(package.join("absent.py"), "_ = 1\n").unwrap();
    fs::write(main, active).unwrap();
    let output = skylos(project.path(), &["--json", "--strict"]);
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(output.status.code(), Some(0), "{report}");
    assert_eq!(report["coverage"]["status"], "complete");
    assert_eq!(report["unused_functions"], serde_json::json!([]));
}

#[test]
fn skylos_reports_a_failed_lazy_import_in_an_eager_annotation() {
    let project = skylos_fixture();
    let package = project.path().join("pkg");
    fs::create_dir(&package).unwrap();
    fs::write(
        package.join("__init__.py"),
        "def __getattr__(name):\n    if name == 'Type':\n        from .absent import Type\n        return Type\n    raise AttributeError(name)\n",
    )
    .unwrap();
    for (prefix, status) in [("", 2), ("from __future__ import annotations\n", 1)] {
        fs::write(
            project.path().join("main.py"),
            format!("{prefix}import pkg\ndef called(arg: pkg.Type):\n    return arg\nassert called(1) == 1\n\ndef orphan():\n    pass\n"),
        )
        .unwrap();
        let output = skylos(project.path(), &["--json", "--strict"]);
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(output.status.code(), Some(status), "{report}");
        if status == 2 {
            assert_eq!(report["coverage"]["status"], "incomplete");
            assert!(
                report["analysis_errors"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|issue| {
                        issue["kind"] == "unresolved_import" && issue["file"] == "pkg/__init__.py"
                    }),
                "{report}"
            );
        } else {
            assert_eq!(report["coverage"]["status"], "complete");
            assert_eq!(report["analysis_errors"], serde_json::json!([]));
            assert_eq!(report["unused_functions"][0]["full_name"], "main.orphan");
        }
    }
}

#[test]
fn skylos_distinguishes_deferred_annotations_from_runtime_reads() {
    let project = skylos_fixture();
    let package = project.path().join("pkg");
    fs::create_dir(&package).unwrap();
    fs::write(package.join("__init__.py"), "class Present:\n    pass\n").unwrap();

    for (code, evaluated) in [
        (
            "from __future__ import annotations\nimport pkg\ndef used(arg: pkg.Missing) -> pkg.Present:\n    return arg\nassert used(1) == 1\n",
            false,
        ),
        (
            "import pkg\ndef used():\n    known: pkg.Present\n    result: pkg.Missing = 1\n    return result\nassert used() == 1\n",
            false,
        ),
        (
            "import pkg\ndef used(arg: pkg.Missing) -> pkg.Present:\n    return arg\nassert used(1) == 1\n",
            true,
        ),
    ] {
        fs::write(
            project.path().join("main.py"),
            format!("{code}\ndef orphan():\n    return 2\n"),
        )
        .unwrap();
        let output = skylos(project.path(), &["--json", "--strict"]);
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        if evaluated {
            assert_eq!(output.status.code(), Some(2), "{report}");
            assert_eq!(report["coverage"]["status"], "incomplete");
            assert!(
                report["analysis_errors"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|issue| {
                        issue["kind"] == "unresolved_attribute" && issue["file"] == "main.py"
                    }),
                "{report}"
            );
        } else {
            assert_eq!(output.status.code(), Some(1), "{report}");
            assert_eq!(report["coverage"]["status"], "complete");
            assert_eq!(report["analysis_errors"], serde_json::json!([]));
            let findings = report["unused_functions"].as_array().unwrap();
            assert_eq!(findings.len(), 1, "{report}");
            assert_eq!(findings[0]["full_name"], "main.orphan");
            assert!(
                report["dead_code_evidence"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|item| {
                        item["kind"] == "type_reference" && item["full_name"] == "pkg.Present"
                    }),
                "{report}"
            );
        }
    }
}

#[test]
fn skylos_distinguishes_dotted_attributes_from_submodule_imports() {
    let project = skylos_fixture();
    let package = project.path().join("pkg");
    fs::create_dir(&package).unwrap();
    fs::write(package.join("plugin.py"), "def run():\n    return 1\n").unwrap();

    for initializer in [
        "pass\n",
        "def __getattr__(name):\n    raise AttributeError(name)\n",
    ] {
        fs::write(package.join("__init__.py"), initializer).unwrap();
        for access in ["pkg.plugin", "pkg.plugin.run()", "pkg.missing"] {
            fs::write(
                project.path().join("main.py"),
                format!("import pkg\n{access}\n"),
            )
            .unwrap();
            let output = skylos(project.path(), &["--json", "--strict"]);
            let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(output.status.code(), Some(2), "{report}");
            assert_eq!(report["coverage"]["status"], "incomplete");
            assert!(
                report["coverage"]["issues"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|issue| {
                        issue["kind"] == "unresolved_attribute"
                            && issue["path"] == "main.py"
                            && issue["line"] == 2
                    }),
                "{report}"
            );
            assert_eq!(report["unused_functions"], serde_json::json!([]));
        }

        for main in [
            "from pkg import plugin\nplugin.run()\n",
            "import pkg.plugin\npkg.plugin.run()\n",
            "import pkg\nfrom pkg import plugin as attached\npkg.plugin.run()\n",
        ] {
            fs::write(project.path().join("main.py"), main).unwrap();
            let output = skylos(project.path(), &["--json", "--strict"]);
            let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(output.status.code(), Some(0), "{main}\n{report}");
            assert_eq!(report["coverage"]["status"], "complete");
            assert_eq!(report["unused_functions"], serde_json::json!([]));
            assert_eq!(report["unused_imports"], serde_json::json!([]));
        }
    }
}

#[test]
fn skylos_tracks_submodule_attachment_through_an_earlier_import() {
    let project = skylos_fixture();
    let root = project.path();
    fs::create_dir(root.join("pkg")).unwrap();
    fs::write(root.join("pkg/__init__.py"), "").unwrap();
    fs::write(root.join("pkg/child.py"), "def run():\n    return 1\n").unwrap();
    fs::write(root.join("bootstrap.py"), "import pkg.child\n").unwrap();
    fs::write(
        root.join("setup.py"),
        "import bootstrap\n\ndef ready():\n    return True\n",
    )
    .unwrap();

    for earlier in ["import setup", "from setup import ready\nready()"] {
        fs::write(
            root.join("main.py"),
            format!("{earlier}\nimport pkg\npkg.child.run()\n\ndef orphan():\n    return 2\n"),
        )
        .unwrap();
        let output = skylos(root, &["--json", "--strict"]);
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(output.status.code(), Some(1), "{report}");
        assert_eq!(report["coverage"]["status"], "complete");
        let findings = report["unused_functions"].as_array().unwrap();
        assert!(
            findings
                .iter()
                .any(|finding| finding["full_name"] == "main.orphan")
        );
        assert!(
            findings
                .iter()
                .all(|finding| finding["full_name"] != "pkg.child.run")
        );
        assert_eq!(report["unused_imports"], serde_json::json!([]), "{report}");
    }

    for (main, bootstrap) in [
        (
            "import pkg\npkg.child.run()\nimport setup\n",
            "import pkg.child\n",
        ),
        (
            "import setup\nimport pkg\npkg.child.run()\n",
            "if __name__ == '__main__':\n    import pkg.child\n",
        ),
    ] {
        fs::write(root.join("main.py"), main).unwrap();
        fs::write(root.join("bootstrap.py"), bootstrap).unwrap();
        let output = skylos(root, &["--json", "--strict"]);
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(output.status.code(), Some(2), "{report}");
        assert_eq!(report["coverage"]["status"], "incomplete");
    }
}

#[test]
fn skylos_tracks_attachments_from_imported_child_and_parent_initializers() {
    let project = skylos_fixture();
    let root = project.path();
    fs::create_dir(root.join("pkg")).unwrap();
    fs::write(root.join("pkg/__init__.py"), "").unwrap();
    fs::write(root.join("pkg/child.py"), "def run():\n    return 1\n").unwrap();
    fs::create_dir(root.join("bootstrap")).unwrap();

    for (earlier, initializer, loader) in [
        ("from bootstrap import loader", "", "import pkg.child\n"),
        ("import bootstrap.loader", "import pkg.child\n", ""),
    ] {
        fs::write(root.join("bootstrap/__init__.py"), initializer).unwrap();
        fs::write(root.join("bootstrap/loader.py"), loader).unwrap();
        fs::write(
            root.join("main.py"),
            format!("{earlier}\nimport pkg\npkg.child.run()\n\ndef orphan():\n    return 2\n"),
        )
        .unwrap();
        let output = skylos(root, &["--json", "--strict"]);
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(output.status.code(), Some(1), "{earlier}\n{report}");
        assert_eq!(report["coverage"]["status"], "complete", "{report}");
        let findings = report["unused_functions"].as_array().unwrap();
        assert_eq!(findings.len(), 1, "{report}");
        assert_eq!(findings[0]["full_name"], "main.orphan", "{report}");
        assert_eq!(report["unused_imports"], serde_json::json!([]), "{report}");
    }

    fs::write(root.join("bootstrap/__init__.py"), "loader = 1\n").unwrap();
    fs::write(root.join("bootstrap/loader.py"), "import pkg.child\n").unwrap();
    fs::write(
        root.join("main.py"),
        "from bootstrap import loader\nimport pkg\npkg.child.run()\n",
    )
    .unwrap();
    let output = skylos(root, &["--json", "--strict"]);
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(output.status.code(), Some(2), "{report}");
    assert_eq!(report["coverage"]["status"], "incomplete", "{report}");
    assert!(
        report["coverage"]["issues"]
            .as_array()
            .unwrap()
            .iter()
            .any(|issue| issue["kind"] == "ambiguous_binding")
    );
}

#[test]
fn skylos_resolves_the_package_binding_at_the_time_of_attachment() {
    let project = skylos_fixture();
    let package = project.path().join("pkg");
    fs::create_dir(&package).unwrap();
    fs::write(
        package.join("actual.py"),
        "def run():\n    return 'initial'\n",
    )
    .unwrap();
    fs::write(
        package.join("plugin.py"),
        "def run():\n    return 'attached'\n",
    )
    .unwrap();

    for (initializer, main, expected_unused, required_import_file) in [
        (
            "from . import actual as plugin\n",
            "import pkg.plugin\nfrom pkg import plugin\nassert plugin.run() == 'attached'\n",
            "pkg.actual.run",
            "main.py",
        ),
        (
            "from . import actual as plugin\n",
            "import pkg.plugin\nassert pkg.plugin.run() == 'attached'\n",
            "pkg.actual.run",
            "main.py",
        ),
        (
            "from . import actual as plugin\n",
            "from pkg import plugin\nimport pkg.plugin\nassert plugin.run() == 'initial'\n",
            "pkg.plugin.run",
            "",
        ),
        (
            "from . import plugin\nfrom . import actual as plugin\n",
            "import pkg.plugin\nfrom pkg import plugin\nassert plugin.run() == 'initial'\n",
            "pkg.plugin.run",
            "pkg/__init__.py",
        ),
    ] {
        fs::write(package.join("__init__.py"), initializer).unwrap();
        fs::write(project.path().join("main.py"), main).unwrap();
        let output = skylos(project.path(), &["--json"]);
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(output.status.code(), Some(0), "{report}");
        assert_eq!(report["coverage"]["status"], "complete", "{report}");
        let findings = report["unused_functions"].as_array().unwrap();
        assert_eq!(findings.len(), 1, "{report}");
        assert_eq!(
            findings[0]["full_name"], expected_unused,
            "{main}\n{report}"
        );
        assert!(
            report["unused_imports"]
                .as_array()
                .unwrap()
                .iter()
                .all(|finding| {
                    finding["file"] != required_import_file || finding["full_name"] != "pkg.plugin"
                }),
            "{main}\n{report}"
        );
    }
}

#[test]
fn skylos_reports_a_missing_relative_import_only_in_the_selected_lazy_branch() {
    let project = skylos_fixture();
    let package = project.path().join("pkg");
    fs::create_dir(&package).unwrap();
    fs::write(
        package.join("__init__.py"),
        "def use():\n    return 1\n\ndef __getattr__(name):\n    if name == 'plugin':\n        from .missing import plugin\n        return plugin\n    if name == 'available':\n        return use\n    raise AttributeError(name)\n",
    )
    .unwrap();
    fs::write(
        project.path().join("main.py"),
        "from pkg import available\navailable()\n",
    )
    .unwrap();
    let output = skylos(project.path(), &["--json", "--strict"]);
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(output.status.code(), Some(0), "{report}");
    assert_eq!(report["coverage"]["status"], "complete");
    assert_eq!(report["unused_functions"], serde_json::json!([]));

    fs::write(project.path().join("main.py"), "from pkg import plugin\n").unwrap();
    let output = skylos(project.path(), &["--json", "--strict"]);
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(output.status.code(), Some(2), "{report}");
    assert_eq!(report["coverage"]["status"], "incomplete");
    let issues = report["coverage"]["issues"].as_array().unwrap();
    assert_eq!(issues.len(), 1, "{report}");
    assert_eq!(issues[0]["kind"], "unresolved_import");
    assert_eq!(issues[0]["path"], "pkg/__init__.py");
    assert_eq!(issues[0]["line"], 6);
    assert_eq!(report["unused_imports"], serde_json::json!([]));
}
