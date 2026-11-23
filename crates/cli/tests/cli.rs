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
fn grep_verification_honors_profile_defaults_and_explicit_flags() {
    let project = skylos_fixture();
    fs::write(
        project.path().join("workflow.py"),
        "def queue_task():\n    return 1\n",
    )
    .unwrap();
    fs::write(
        project.path().join("jobs.yaml"),
        "handler_module: workflow.py\n",
    )
    .unwrap();

    for (args, findings, name_field, name, retained) in [
        (&[][..], "findings", "symbol", "queue_task", false),
        (&["--grep-verify"], "findings", "symbol", "queue_task", true),
        (
            &["--profile", "skylos"],
            "unused_functions",
            "full_name",
            "workflow.queue_task",
            false,
        ),
        (
            &["--profile", "skylos-0.3"],
            "unused_functions",
            "full_name",
            "workflow.queue_task",
            true,
        ),
        (
            &["--profile", "skylos-0.3", "--no-grep-verify"],
            "unused_functions",
            "full_name",
            "workflow.queue_task",
            false,
        ),
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_skarmory"))
            .current_dir(project.path())
            .env_remove("SKYLOS_CONFIG_FILE")
            .arg(".")
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{args:?}: {} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(report["coverage"]["status"], "complete");
        assert_eq!(
            report[findings]
                .as_array()
                .unwrap()
                .iter()
                .any(|finding| finding[name_field] == name),
            !retained,
            "{args:?}: {report}"
        );
    }
}

#[test]
fn grep_verification_strict_gate_respects_yaml_scalar_context_and_file_identity() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    fs::write(root.join(".skylos.toml"), "root = true\n").unwrap();
    fs::write(root.join("workflow.py"), "def run(): return 1\n").unwrap();
    fs::create_dir(root.join("workflow")).unwrap();
    fs::write(root.join("workflow/py.py"), "def run(): return 2\n").unwrap();

    for (config, unused, matched) in [
        (
            "\"documentation\": |\n  handler: workflow.py:run\n",
            &["workflow.py", "workflow/py.py"][..],
            &[][..],
        ),
        (
            "'handler': workflow.py:run\n",
            &["workflow/py.py"][..],
            &["workflow.py"][..],
        ),
    ] {
        fs::write(root.join("jobs.yaml"), config).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_skarmory"))
            .current_dir(root)
            .env_remove("SKYLOS_CONFIG_FILE")
            .args([".", "--profile", "skylos-0.3", "--strict"])
            .output()
            .unwrap();
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(output.status.code(), Some(1), "{config}: {report}");
        assert_eq!(report["coverage"]["status"], "complete");
        assert_eq!(report["gate"]["passed"], false);
        let unused_files: Vec<_> = report["unused_functions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|finding| finding["file"].as_str().unwrap())
            .collect();
        assert_eq!(unused_files, unused, "{config}: {report}");
        let matched_files: Vec<_> = report["grep_verification_evidence"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|evidence| evidence["definition_path"].as_str().unwrap())
            .collect();
        assert_eq!(matched_files, matched, "{config}: {report}");
    }
}

#[test]
fn grep_verification_limit_blocks_the_strict_gate() {
    let project = skylos_fixture();
    fs::write(
        project.path().join("jobs.yaml"),
        vec![b'#'; 1024 * 1024 + 1],
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_skarmory"))
        .current_dir(project.path())
        .env_remove("SKYLOS_CONFIG_FILE")
        .args([".", "--profile", "skylos-0.3", "--strict"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["coverage"]["status"], "incomplete");
    assert_eq!(
        report["coverage"]["issues"][0]["kind"],
        "grep_verification_limit"
    );
    assert_eq!(report["coverage"]["issues"][0]["path"], "jobs.yaml");
    assert_eq!(report["unused_functions"], serde_json::json!([]));
    assert_eq!(report["gate"]["exit_code"], 2);
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
fn skylos_rejects_misplaced_future_imports() {
    let project = skylos_fixture();
    let main = project.path().join("main.py");
    for (source, line) in [
        ("x = 1\nfrom __future__ import annotations\nprint(x)\n", 2),
        (
            "def f():\n    from __future__ import annotations\n    print('x')\nf()\n",
            2,
        ),
        ("if True:\n    from __future__ import annotations\n", 2),
        ("pass\nfrom __future__ import annotations\n", 2),
    ] {
        fs::write(&main, source).unwrap();
        let output = skylos(project.path(), &["--json", "--strict"]);
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(output.status.code(), Some(2), "{source}\n{report}");
        assert_eq!(report["coverage"]["status"], "incomplete", "{report}");
        assert_eq!(
            report["analysis_errors"][0]["kind"], "parse_error",
            "{report}"
        );
        assert_eq!(report["analysis_errors"][0]["line"], line, "{report}");
    }

    fs::write(
        &main,
        "'documentation'\nfrom __future__ import annotations\nfrom __future__ import annotations\nprint('ok')\n",
    )
    .unwrap();
    let output = skylos(project.path(), &["--json", "--strict"]);
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(output.status.code(), Some(0), "{report}");
    assert_eq!(report["coverage"]["status"], "complete", "{report}");
}

#[test]
fn skylos_rejects_bindings_to_debug_and_accepts_reads() {
    let project = skylos_fixture();
    let main = project.path().join("main.py");
    for profile in ["skylos", "skylos-0.3"] {
        let run = |source: &str| {
            fs::write(&main, source).unwrap();
            let output = Command::new(env!("CARGO_BIN_EXE_skarmory"))
                .current_dir(project.path())
                .env_remove("SKYLOS_CONFIG_FILE")
                .args(["--profile", profile, "--json", "--strict", "."])
                .output()
                .unwrap();
            let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
            (output.status.code(), report)
        };
        for (source, line, column) in [
            ("__debug__ = True\nprint(__debug__)\n", 1, 1),
            (
                "\ndef run(__debug__):\n    print(__debug__)\nrun(True)\n",
                2,
                9,
            ),
            ("__debug__: bool\n", 1, 1),
            ("__ｄｅｂｕｇ__ = True\n", 1, 1),
            ("def run(*, __debug__): pass\n", 1, 12),
            ("with manager as __debug__:\n    pass\n", 1, 17),
            ("def __debug__(): pass\n", 1, 5),
            ("class __debug__: pass\n", 1, 7),
            ("import __debug__.submodule\n", 1, 8),
            ("from module import value as __debug__\n", 1, 29),
        ] {
            let (exit, report) = run(source);
            assert_eq!(exit, Some(2), "{profile}: {source}\n{report}");
            assert_eq!(report["gate"]["exit_code"], 2, "{report}");
            assert_eq!(report["coverage"]["status"], "incomplete", "{report}");
            let error = &report["coverage"]["issues"][0];
            assert_eq!(error["kind"], "parse_error", "{report}");
            assert_eq!(error["line"], line, "{report}");
            assert_eq!(error["column"], column, "{report}");
        }

        let (exit, report) = run("print(__debug__)\n");
        assert_eq!(exit, Some(0), "{profile}: {report}");
        assert_eq!(report["coverage"]["status"], "complete", "{report}");
    }
}

#[test]
fn skylos_rejects_repeated_call_keywords() {
    let project = skylos_fixture();
    let main = project.path().join("main.py");
    fs::write(&main, "print('test', end='a', sep='b', end='c')\n").unwrap();
    let output = skylos(project.path(), &["--json", "--strict"]);
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(output.status.code(), Some(2), "{report}");
    assert_eq!(report["coverage"]["status"], "incomplete", "{report}");
    assert_eq!(
        report["analysis_errors"][0]["kind"], "parse_error",
        "{report}"
    );
    assert_eq!(report["analysis_errors"][0]["line"], 1, "{report}");
    assert_eq!(report["analysis_errors"][0]["column"], 33, "{report}");

    fs::write(&main, "print(print('nested', end=''), end='', sep='b')\n").unwrap();
    let output = skylos(project.path(), &["--json", "--strict"]);
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(output.status.code(), Some(0), "{report}");
    assert_eq!(report["coverage"]["status"], "complete", "{report}");
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
    for (definition, body) in [
        (
            "def callback():\n    from a import used\n    used()",
            "    fn()\n",
        ),
        (
            "def callback():\n    from a import used\n    used()",
            "    def nested():\n        fn()\n    nested()\n",
        ),
        (
            "class callback:\n    def __init__(self):\n        from a import used\n        used()",
            "    fn()\n",
        ),
    ] {
        fs::write(
            project.path().join("a.py"),
            format!("def used():\n    return 1\n\n{definition}\n\nfrom b import invoke\ninvoke(callback)\n\ndef used():\n    return 2\n"),
        )
        .unwrap();
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
fn skylos_relative_exclusions_only_apply_to_directory_scans() {
    let project = skylos_fixture();
    fs::write(project.path().join("main.py"), "print(1)\n").unwrap();
    fs::write(project.path().join("bad.py"), "def broken(\n").unwrap();

    for (config, flags) in [
        ("root = true\n", &["--exclude", "bad.py"][..]),
        ("root = true\nexclude = [\"**/bad.py\"]\n", &[][..]),
    ] {
        fs::write(project.path().join(".skylos.toml"), config).unwrap();
        for profile in ["skylos", "skylos-0.3"] {
            let run = |target| {
                let output = Command::new(env!("CARGO_BIN_EXE_skarmory"))
                    .current_dir(project.path())
                    .env_remove("SKYLOS_CONFIG_FILE")
                    .args(["--profile", profile, "--json", "--strict", target])
                    .args(flags)
                    .output()
                    .unwrap();
                let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
                (output.status.code(), report)
            };
            let (exit, report) = run("bad.py");
            assert_eq!(exit, Some(2), "{profile}: {report}");
            assert_eq!(report["gate"]["exit_code"], 2);
            assert_eq!(report["selected_paths"], serde_json::json!(["bad.py"]));
            assert_eq!(report["exclusions"], serde_json::json!([]));
            assert_eq!(report["coverage"]["status"], "incomplete");
            assert_eq!(report["coverage"]["issues"][0]["kind"], "parse_error");
            assert_eq!(report["coverage"]["issues"][0]["path"], "bad.py");

            let (exit, report) = run(".");
            assert_eq!(exit, Some(0), "{profile}: {report}");
            assert_eq!(report["gate"]["exit_code"], 0);
            assert_eq!(report["selected_paths"], serde_json::json!(["main.py"]));
            assert_eq!(report["coverage"]["status"], "complete");
            assert_eq!(
                report["exclusions"],
                serde_json::json!([{"path": "bad.py", "reason": "configured_exclusion"}])
            );
        }
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
    for (flag, classes) in [
        ("False", subclass),
        ("False", nested_subclass),
        ("True", subclass),
    ] {
        fs::write(
            project.path().join("main.py"),
            format!("{base}ENABLED = {flag}\n{classes}\nBase()\n"),
        )
        .unwrap();
        let output = skylos(project.path(), &["--json", "--strict"]);
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
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
fn skylos_retains_classes_used_for_member_lookup() {
    let project = skylos_fixture();
    fs::write(
        project.path().join("main.py"),
        r#"class Registry:
    @staticmethod
    def run():
        print("live")

class Dormant:
    @staticmethod
    def run():
        pass

def unused():
    Dormant.run()

Registry.run()
from support import lazy
lazy()
"#,
    )
    .unwrap();
    fs::write(
        project.path().join("support.py"),
        r#"class LazyRegistry:
    @staticmethod
    def run():
        print("lazy")

def __getattr__(name):
    if name == "lazy":
        return LazyRegistry.run
    raise AttributeError(name)
"#,
    )
    .unwrap();
    let output = skylos(project.path(), &["--json", "--strict"]);
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(output.status.code(), Some(1), "{report}");
    assert_eq!(report["coverage"]["status"], "complete");
    let classes = report["unused_classes"].as_array().unwrap();
    assert_eq!(classes.len(), 1, "{report}");
    assert_eq!(classes[0]["full_name"], "main.Dormant");
    let evidence = report["dead_code_evidence"].as_array().unwrap();
    for class in ["main.Registry", "support.LazyRegistry"] {
        assert!(
            evidence
                .iter()
                .any(|item| item["full_name"] == class && item["kind"] == "class_reference"),
            "{report}"
        );
    }
}

#[test]
fn skylos_retains_classes_read_as_values() {
    let project = skylos_fixture();
    fs::write(
        project.path().join("main.py"),
        r#"import support
from support import Imported

class Model:
    pass

class Dormant:
    pass

class Replaced:
    pass

Replaced = None

def unused():
    print(Dormant)

alias = Model
print(alias, Imported, support.Qualified, Replaced)
"#,
    )
    .unwrap();
    fs::write(
        project.path().join("support.py"),
        "class Imported:\n    pass\n\nclass Qualified:\n    pass\n",
    )
    .unwrap();
    let output = skylos(project.path(), &["--json", "--strict"]);
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(output.status.code(), Some(1), "{report}");
    assert_eq!(report["coverage"]["status"], "complete", "{report}");
    let classes: Vec<_> = report["unused_classes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["full_name"].as_str().unwrap())
        .collect();
    assert_eq!(classes, ["main.Dormant", "main.Replaced"], "{report}");
    let evidence = report["dead_code_evidence"].as_array().unwrap();
    for class in ["main.Model", "support.Imported", "support.Qualified"] {
        assert!(
            evidence
                .iter()
                .any(|item| item["full_name"] == class && item["kind"] == "class_reference"),
            "{report}"
        );
    }
}

#[test]
fn skylos_skips_enclosing_class_names_in_nested_class_scopes() {
    let project = skylos_fixture();
    fs::write(
        project.path().join("main.py"),
        r#"def for_body():
    print("module helper for class body")

def for_method():
    print("module helper for method body")

class Outer:
    def for_body():
        pass

    def for_method():
        pass

    class Inner:
        def local():
            print("class-local helper")

        local()
        for_body()

        @staticmethod
        def run():
            for_method()

Outer.Inner.run()
"#,
    )
    .unwrap();
    let output = skylos(project.path(), &["--json", "--strict"]);
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(output.status.code(), Some(1), "{report}");
    assert_eq!(report["coverage"]["status"], "complete");
    let findings = report["unused_functions"].as_array().unwrap();
    let names: Vec<_> = findings
        .iter()
        .map(|row| row["full_name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["main.Outer.for_body", "main.Outer.for_method"]);
}

#[test]
fn skylos_reports_unmodeled_comparison_dispatch_only_when_evaluated() {
    let project = skylos_fixture();
    let source = r#"def observed():
    print("comparison called")

class Value:
    def __eq__(self, other):
        observed()
        return False

value = Value()
"#;
    for (comparison, exit) in [
        ("Value() == Value()", 2),
        ("[Value()] == [Value()]", 2),
        ("{\"key\": Value()} == {\"key\": Value()}", 2),
        ("([[value]],) != ([[Value()]],)", 2),
        ("0 != value", 2),
        ("value is Value()", 1),
        ("[value] is [Value()]", 1),
        ("[value] == []", 1),
        ("[value] == 0", 1),
        ("if False:\n    value == Value()", 1),
        ("class Plain:\n    pass\n\nPlain() == Plain()", 1),
    ] {
        fs::write(
            project.path().join("main.py"),
            format!("{source}{comparison}\n"),
        )
        .unwrap();
        let output = skylos(project.path(), &["--json", "--strict"]);
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(output.status.code(), Some(exit), "{comparison}\n{report}");
        if exit == 2 {
            assert_eq!(report["coverage"]["status"], "incomplete");
            assert_eq!(report["unused_functions"], serde_json::json!([]));
            assert_eq!(
                report["analysis_errors"][0]["kind"],
                "unresolved_comparison"
            );
            assert_eq!(report["analysis_errors"][0]["line"], 10);
        } else {
            assert_eq!(report["coverage"]["status"], "complete");
            assert_eq!(report["unused_functions"][0]["full_name"], "main.observed");
        }
    }
}

#[test]
fn skylos_roots_only_effective_explicit_reexports() {
    let project = skylos_fixture();
    let pkg = project.path().join("pkg");
    fs::create_dir(&pkg).unwrap();
    fs::write(pkg.join("impl.py"), "def old():\n    pass\n").unwrap();
    fs::write(project.path().join("main.py"), "import pkg\nprint(pkg)\n").unwrap();
    for (rebind, exit) in [
        ("old = None\n", 1),
        ("", 0),
        ("old = old\n", 0),
        ("alias = old\nold = alias\n", 0),
        ("def replacement():\n    pass\nold = replacement\n", 1),
        ("if int('0'):\n    old = None\n", 2),
    ] {
        fs::write(
            pkg.join("__init__.py"),
            format!("from .impl import old as old\n{rebind}"),
        )
        .unwrap();
        let output = skylos(project.path(), &["--json", "--strict"]);
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(output.status.code(), Some(exit), "{rebind}\n{report}");
        if exit == 1 {
            assert_eq!(report["coverage"]["status"], "complete");
            let findings = report["unused_functions"].as_array().unwrap();
            assert_eq!(findings.len(), 1, "{report}");
            assert_eq!(findings[0]["full_name"], "pkg.impl.old");
        } else if exit == 0 {
            assert_eq!(report["coverage"]["status"], "complete");
            assert!(
                report["dead_code_evidence"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|item| item["full_name"] == "pkg.impl.old"
                        && item["kind"] == "explicit_re_export"),
                "{report}"
            );
        } else {
            assert_eq!(report["coverage"]["status"], "incomplete");
            assert_eq!(report["analysis_errors"][0]["kind"], "ambiguous_binding");
        }
    }
}

#[test]
fn skylos_resolves_explicit_reexports_after_submodule_attachment() {
    let project = skylos_fixture();
    let root = project.path();
    let pkg = root.join("pkg");
    fs::create_dir(&pkg).unwrap();
    fs::write(pkg.join("impl.py"), "def public():\n    pass\n").unwrap();
    fs::write(pkg.join("public.py"), "print('loaded')\n").unwrap();
    fs::write(root.join("main.py"), "import pkg\nprint(pkg)\n").unwrap();

    let reexport = "from .impl import public as public";
    let attach = "import pkg.public";
    for (initializer, replaced) in [
        (format!("{reexport}\n{attach}\n"), true),
        (format!("{attach}\n{reexport}\n"), false),
    ] {
        fs::write(pkg.join("__init__.py"), &initializer).unwrap();
        let output = skylos(root, &["--json", "--strict"]);
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(
            output.status.code(),
            Some(i32::from(replaced)),
            "{initializer}\n{report}"
        );
        assert_eq!(report["coverage"]["status"], "complete", "{report}");
        let expected: Vec<&str> = if replaced {
            vec!["pkg.impl.public"]
        } else {
            vec![]
        };
        for category in ["unused_functions", "unused_imports"] {
            let names: Vec<_> = report[category]
                .as_array()
                .unwrap()
                .iter()
                .map(|row| row["full_name"].as_str().unwrap())
                .collect();
            assert_eq!(names, expected, "{category}\n{report}");
        }
        assert_eq!(
            report["dead_code_evidence"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["full_name"] == "pkg.impl.public"
                    && item["kind"] == "explicit_re_export"),
            !replaced,
            "{report}"
        );
    }

    fs::write(
        pkg.join("__init__.py"),
        format!("{reexport}\nif int('0'):\n    {attach}\n"),
    )
    .unwrap();
    let output = skylos(root, &["--json", "--strict"]);
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(output.status.code(), Some(2), "{report}");
    assert_eq!(report["coverage"]["status"], "incomplete", "{report}");
    assert_eq!(report["analysis_errors"][0]["kind"], "ambiguous_binding");
    assert_eq!(report["analysis_errors"][0]["file"], "pkg/__init__.py");
    assert_eq!(report["analysis_errors"][0]["line"], 1);
}

#[test]
fn skylos_retains_explicit_reexport_chains_in_ordinary_modules() {
    let project = skylos_fixture();
    let root = project.path();
    fs::remove_file(root.join("main.py")).unwrap();
    fs::write(root.join("api.py"), "from bridge import launch as launch\n").unwrap();
    fs::write(
        root.join("bridge.py"),
        "from implementation import launch as launch, dormant\n",
    )
    .unwrap();
    fs::write(
        root.join("implementation.py"),
        "def launch():\n    pass\n\ndef dormant():\n    pass\n",
    )
    .unwrap();

    let output = skylos(root, &["--json", "--strict"]);
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(output.status.code(), Some(1), "{report}");
    assert_eq!(report["coverage"]["status"], "complete", "{report}");
    for category in ["unused_functions", "unused_imports"] {
        let names: Vec<_> = report[category]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["full_name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["implementation.dormant"], "{category}\n{report}");
    }
    assert!(
        report["dead_code_evidence"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["full_name"] == "implementation.launch"
                && item["kind"] == "explicit_re_export"),
        "{report}"
    );
}

#[test]
fn skylos_ignores_unexecuted_attachments_when_resolving_exports() {
    let project = skylos_fixture();
    let root = project.path();
    let pkg = root.join("pkg");
    fs::create_dir(&pkg).unwrap();
    fs::write(pkg.join("impl.py"), "def public():\n    pass\n").unwrap();
    fs::write(pkg.join("public.py"), "print('loaded')\n").unwrap();
    let initializer = "from .impl import public as public\n";
    fs::write(
        root.join("main.py"),
        "import pkg\nprint(pkg)\n\ndef _unused():\n    import pkg.public\n",
    )
    .unwrap();

    for (source, kind) in [
        (initializer, "explicit_re_export"),
        (
            "from .impl import public\n__all__ = ['public']\n",
            "public_export",
        ),
    ] {
        fs::write(pkg.join("__init__.py"), source).unwrap();
        let output = skylos(root, &["--json", "--strict"]);
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(output.status.code(), Some(1), "{report}");
        assert_eq!(report["coverage"]["status"], "complete", "{report}");
        let unused = report["unused_functions"].as_array().unwrap();
        assert_eq!(unused.len(), 1, "{report}");
        assert_eq!(unused[0]["full_name"], "main._unused");
        assert!(
            report["dead_code_evidence"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["full_name"] == "pkg.impl.public" && item["kind"] == kind),
            "{report}"
        );
    }

    fs::write(pkg.join("__init__.py"), initializer).unwrap();
    fs::write(
        root.join("main.py"),
        "import pkg\nprint(pkg)\nimport pkg.public\nprint(pkg.public)\n",
    )
    .unwrap();
    let output = skylos(root, &["--json", "--strict"]);
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(output.status.code(), Some(0), "{report}");
    assert_eq!(report["coverage"]["status"], "complete", "{report}");

    // A synchronous initializer call makes the submodule import possible.
    fs::write(root.join("main.py"), "import pkg\nprint(pkg)\n").unwrap();
    fs::write(
        pkg.join("__init__.py"),
        format!("{initializer}\ndef _load():\n    import pkg.public\n\n_load()\n"),
    )
    .unwrap();
    let output = skylos(root, &["--json", "--strict"]);
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(output.status.code(), Some(2), "{report}");
    assert_eq!(report["coverage"]["status"], "incomplete", "{report}");
    assert_eq!(report["analysis_errors"][0]["kind"], "ambiguous_binding");

    // The module import executed by a lazy hook belongs to the initializer
    // that selects the hook, even though the hook itself is never called directly.
    fs::write(
        pkg.join("__init__.py"),
        format!("{initializer}from .provider import load\nprint(load)\n"),
    )
    .unwrap();
    fs::write(
        pkg.join("provider.py"),
        "def __getattr__(name):\n    if name == 'load':\n        import pkg.public\n        return 1\n    raise AttributeError(name)\n",
    )
    .unwrap();
    let output = skylos(root, &["--json", "--strict"]);
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(output.status.code(), Some(2), "{report}");
    assert_eq!(report["coverage"]["status"], "incomplete", "{report}");
    assert_eq!(report["analysis_errors"][0]["kind"], "ambiguous_binding");
}

#[test]
fn skylos_retains_constructor_and_public_method_dependencies_of_exported_classes() {
    let project = skylos_fixture();
    let root = project.path();
    fs::remove_file(root.join("main.py")).unwrap();
    let pkg = root.join("pkg");
    fs::create_dir(&pkg).unwrap();
    let source = r#"def prepare():
    print("prepared")

def _render():
    print("rendered")

def unused():
    pass

class Base:
    def render(self):
        _render()

class Client(Base):
    def __init__(self):
        prepare()

    def _internal(self):
        pass
"#;
    fs::write(pkg.join("impl.py"), source).unwrap();

    for (ordinary, initializer, implementation, expected) in [
        (
            "from pkg.impl import Client as Client\n",
            "",
            "",
            vec!["pkg.impl.Client._internal", "pkg.impl.unused"],
        ),
        (
            "",
            "from .impl import Client as Client\n",
            "",
            vec!["pkg.impl.Client._internal", "pkg.impl.unused"],
        ),
        (
            "",
            "",
            "__all__ = ['Client']\n",
            vec!["pkg.impl.Client._internal", "pkg.impl.unused"],
        ),
        (
            "",
            "",
            "",
            vec![
                "pkg.impl.Base.render",
                "pkg.impl.Client._internal",
                "pkg.impl._render",
                "pkg.impl.prepare",
                "pkg.impl.unused",
            ],
        ),
    ] {
        fs::write(root.join("api.py"), format!("print('api')\n{ordinary}")).unwrap();
        fs::write(pkg.join("__init__.py"), initializer).unwrap();
        fs::write(pkg.join("impl.py"), format!("{source}\n{implementation}")).unwrap();
        let output = skylos(root, &["--json", "--strict"]);
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(output.status.code(), Some(1), "{report}");
        assert_eq!(report["coverage"]["status"], "complete", "{report}");
        let mut unused: Vec<_> = report["unused_functions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["full_name"].as_str().unwrap())
            .collect();
        unused.sort_unstable();
        assert_eq!(
            unused, expected,
            "{ordinary}{initializer}{implementation}\n{report}"
        );
        if expected.len() == 2 {
            assert!(
                report["dead_code_evidence"].as_array().unwrap().iter().any(
                    |item| item["full_name"] == "pkg.impl.Client.__init__"
                        && item["kind"] == "constructor"
                        && item["certainty"] == "possible"
                ),
                "{report}"
            );
            assert!(
                report["dead_code_evidence"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|item| item["full_name"] == "pkg.impl.Base.render"
                        && item["certainty"] == "possible"),
                "{report}"
            );
        }
    }
}

#[test]
fn skylos_retains_members_of_exported_modules_and_literal_containers() {
    let project = skylos_fixture();
    let root = project.path();
    fs::remove_file(root.join("main.py")).unwrap();
    let pkg = root.join("pkg");
    fs::create_dir(&pkg).unwrap();
    let api = pkg.join("api");
    fs::create_dir(&api).unwrap();
    fs::write(
        api.join("__init__.py"),
        r#"import pkg.api.attached

def _used():
    pass

def run():
    _used()

def _unused():
    pass

def __getattr__(name):
    if name == 'attached':
        return _unused
    raise AttributeError(name)
"#,
    )
    .unwrap();
    fs::write(api.join("attached.py"), "def public():\n    pass\n").unwrap();
    fs::write(api.join("dormant.py"), "def unused():\n    pass\n").unwrap();

    for initializer in [
        "from . import api as api\n",
        "from . import api\nexports = [api]\n__all__ = ['exports']\n",
    ] {
        fs::write(pkg.join("__init__.py"), initializer).unwrap();
        let output = skylos(root, &["--json", "--strict"]);
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(output.status.code(), Some(1), "{report}");
        assert_eq!(report["coverage"]["status"], "complete", "{report}");
        let unused: Vec<_> = report["unused_functions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["full_name"].as_str().unwrap())
            .collect();
        assert_eq!(
            unused,
            ["pkg.api._unused", "pkg.api.dormant.unused"],
            "{initializer}\n{report}"
        );
        assert!(
            report["dead_code_evidence"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["full_name"] == "pkg.api.run" && item["certainty"] == "possible"),
            "{report}"
        );
    }
}

#[test]
fn skylos_retains_public_methods_of_exported_instances() {
    let project = skylos_fixture();
    let root = project.path();
    fs::remove_file(root.join("main.py")).unwrap();
    let pkg = root.join("pkg");
    fs::create_dir(&pkg).unwrap();
    let source = r#"def _live():
    pass

class Client:
    def run(self):
        _live()

    def _internal(self):
        pass

client = Client()
"#;

    for (exports, expected) in [
        ("__all__ = ['client']", vec!["pkg.Client._internal"]),
        (
            "",
            vec!["pkg.Client._internal", "pkg.Client.run", "pkg._live"],
        ),
    ] {
        fs::write(pkg.join("__init__.py"), format!("{source}\n{exports}\n")).unwrap();
        let output = skylos(root, &["--json", "--strict"]);
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(output.status.code(), Some(1), "{report}");
        assert_eq!(report["coverage"]["status"], "complete", "{report}");
        let mut unused: Vec<_> = report["unused_functions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["full_name"].as_str().unwrap())
            .collect();
        unused.sort_unstable();
        assert_eq!(unused, expected, "{report}");
        assert_eq!(
            report["dead_code_evidence"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["full_name"] == "pkg.Client.run"
                    && item["kind"] == "public_export"
                    && item["certainty"] == "possible"),
            !exports.is_empty(),
            "{report}"
        );
    }
}

#[test]
fn skylos_retains_effective_lazy_members_of_exported_modules() {
    let project = skylos_fixture();
    let root = project.path();
    fs::remove_file(root.join("main.py")).unwrap();
    let pkg = root.join("pkg");
    fs::create_dir(&pkg).unwrap();
    let source = r#"def _live():
    pass

def _shadowed():
    pass

ready = True

def __getattr__(name):
    if name == 'run':
        return _live
    if name == 'ready':
        return _shadowed
"#;

    for (initializer, expected) in [
        ("from . import api as api\n", vec!["pkg.api._shadowed"]),
        ("", vec!["pkg.api._live", "pkg.api._shadowed"]),
    ] {
        fs::write(pkg.join("__init__.py"), initializer).unwrap();
        fs::write(
            pkg.join("api.py"),
            format!("{source}    raise AttributeError(name)\n"),
        )
        .unwrap();
        let output = skylos(root, &["--json", "--strict"]);
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(output.status.code(), Some(1), "{report}");
        assert_eq!(report["coverage"]["status"], "complete", "{report}");
        let mut unused: Vec<_> = report["unused_functions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["full_name"].as_str().unwrap())
            .collect();
        unused.sort_unstable();
        assert_eq!(unused, expected, "{report}");
        assert_eq!(
            report["dead_code_evidence"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["full_name"] == "pkg.api._live"
                    && item["kind"] == "explicit_re_export"
                    && item["certainty"] == "possible"),
            !initializer.is_empty(),
            "{report}"
        );
    }

    fs::write(pkg.join("__init__.py"), "from . import api as api\n").unwrap();
    fs::write(pkg.join("api.py"), format!("{source}    return _live\n")).unwrap();
    let output = skylos(root, &["--json", "--strict"]);
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(output.status.code(), Some(2), "{report}");
    assert_eq!(report["coverage"]["status"], "incomplete", "{report}");
    assert!(report["analysis_errors"].as_array().unwrap().iter().any(
        |issue| issue["kind"] == "unresolved_lazy_export" && issue["file"] == "pkg/api.py"
    ), "{report}");
}

#[test]
fn skylos_retains_initializers_for_invoked_class_callbacks_and_configured_classes() {
    let project = skylos_fixture();
    let root = project.path();
    let class =
        "def _prepare():\n    pass\n\nclass Worker:\n    def __init__(self):\n        _prepare()\n";
    for (activation, configured, constructed) in [
        (
            "def invoke(callback):\n    callback()\ninvoke(Worker)\n",
            false,
            true,
        ),
        (
            "def read(callback):\n    print(callback)\nread(Worker)\n",
            false,
            false,
        ),
        (
            "def invoke(callback):\n    if False:\n        callback()\ninvoke(Worker)\n",
            false,
            false,
        ),
        (
            "def invoke(callback):\n    def deferred():\n        callback()\ninvoke(Worker)\n",
            false,
            false,
        ),
        (
            "def invoke(callback):\n    def nested():\n        callback()\n    nested()\ninvoke(Worker)\n",
            false,
            true,
        ),
        ("", true, true),
    ] {
        fs::write(root.join("main.py"), format!("{class}\n{activation}")).unwrap();
        fs::write(
            root.join(".skylos.toml"),
            if configured {
                "root = true\n[[dead_code.entrypoints]]\nname = 'Worker'\nmodule = 'main'\n"
            } else {
                "root = true\n"
            },
        )
        .unwrap();
        let output = skylos(root, &["--json", "--strict"]);
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(report["coverage"]["status"], "complete", "{report}");
        assert_eq!(
            report["unused_functions"]
                .as_array()
                .unwrap()
                .iter()
                .any(|row| row["full_name"] == "main._prepare"),
            !constructed,
            "{activation}\n{report}"
        );
        assert_eq!(
            report["dead_code_evidence"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["full_name"] == "main.Worker.__init__"
                    && item["kind"] == "constructor"
                    && item["certainty"] == "possible"),
            constructed,
            "{activation}\n{report}"
        );
    }
}

#[test]
fn skylos_retains_initializers_for_started_class_callbacks() {
    let project = skylos_fixture();
    let root = project.path();
    let class =
        "def _prepare():\n    pass\n\nclass Worker:\n    def __init__(self):\n        _prepare()\n";
    for (source, started) in [
        (
            "from concurrent.futures import ThreadPoolExecutor\nexecutor = ThreadPoolExecutor()\nexecutor.submit(Worker)\n",
            true,
        ),
        (
            "from multiprocessing import Process\nprocess = Process(target=Worker)\nprocess.start()\n",
            true,
        ),
        (
            "from multiprocessing import Process\nprocess = Process(target=Worker)\nprint(process)\n",
            false,
        ),
    ] {
        fs::write(root.join("main.py"), format!("{class}\n{source}")).unwrap();
        let output = skylos(root, &["--json", "--strict"]);
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(report["coverage"]["status"], "complete", "{report}");
        assert_eq!(
            report["unused_functions"]
                .as_array()
                .unwrap()
                .iter()
                .any(|row| row["full_name"] == "main._prepare"),
            !started,
            "{source}\n{report}"
        );
        assert_eq!(
            report["dead_code_evidence"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["full_name"] == "main.Worker.__init__"
                    && item["kind"] == "callback_constructor"
                    && item["certainty"] == "possible"),
            started,
            "{source}\n{report}"
        );
    }
}

#[test]
fn skylos_awaits_tasks_that_were_already_scheduled() {
    let project = skylos_fixture();
    fs::write(
        project.path().join("main.py"),
        r#"from asyncio import create_task as schedule

def observed():
    print("scheduled")

async def work():
    observed()

async def inline():
    print("awaited")

async def entry():
    await schedule(work())
    task = schedule(work())
    await task
    await inline()

async def unused():
    pass

__all__ = ["entry"]
"#,
    )
    .unwrap();
    let output = skylos(project.path(), &["--json", "--strict"]);
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(output.status.code(), Some(1), "{report}");
    assert_eq!(report["coverage"]["status"], "complete");
    let findings = report["unused_functions"].as_array().unwrap();
    assert_eq!(findings.len(), 1, "{report}");
    assert_eq!(findings[0]["full_name"], "main.unused");
    let evidence = report["dead_code_evidence"].as_array().unwrap();
    assert!(
        evidence
            .iter()
            .any(|item| item["full_name"] == "main.work" && item["kind"] == "scheduled_coroutine"),
        "{report}"
    );
    let awaited: Vec<_> = evidence
        .iter()
        .filter(|item| item["kind"] == "awaited_coroutine")
        .map(|item| item["full_name"].as_str().unwrap())
        .collect();
    assert_eq!(awaited, ["main.inline"], "{report}");
}

#[test]
fn skylos_resolves_bounded_custom_allocation_and_abstains_on_opaque_results() {
    let project = skylos_fixture();
    let source = r#"def observed():
    print("receiver method ran")

class Receiver:
    def run(self):
        observed()

def allocate():
    return Receiver()

class Facade:
    def __init__(self):
        pass

    def run(self):
        pass

"#;
    let bounded = "    def __new__(cls):\n        return Receiver()\n\n";
    let opaque = "    def __new__(cls):\n        return allocate()\n\n";
    for (allocator, incomplete) in [(bounded, false), (opaque, true), ("", false)] {
        fs::write(
            project.path().join("main.py"),
            format!("{source}{allocator}Facade().run()\n"),
        )
        .unwrap();
        let output = skylos(project.path(), &["--json", "--strict"]);
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(
            output.status.code(),
            Some(if incomplete { 2 } else { 1 }),
            "{report}"
        );
        let evidence = report["dead_code_evidence"].as_array().unwrap();
        let has_edge = |target: &str, kind: &str| {
            evidence
                .iter()
                .any(|edge| edge["full_name"] == target && edge["kind"] == kind)
        };
        if incomplete {
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
            if allocator == bounded {
                assert!(!has_edge("main.Facade.__init__", "constructor"));
                assert!(has_edge("main.Receiver.run", "direct_call"));
                assert!(!has_edge("main.Facade.run", "direct_call"));
            } else {
                assert!(has_edge("main.Facade.__init__", "constructor"));
                assert!(has_edge("main.Facade.run", "direct_call"));
            }
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
