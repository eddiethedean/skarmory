use std::fs;
use std::path::Path;
use std::process::Command;

use serde_json::Value;

fn put(root: &Path, path: &str, contents: &str) {
    let path = root.join(path);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}

fn scan(root: &Path, args: &[&str], code: i32) -> Value {
    let output = Command::new(env!("CARGO_BIN_EXE_skarmory"))
        .current_dir(root)
        .env_remove("SKYLOS_CONFIG_FILE")
        .env_remove("DJANGO_SETTINGS_MODULE")
        .args(["--profile", "skylos-0.3", "--no-grep-verify"])
        .args(args)
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(code),
        "{}\n{}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn findings(report: &Value) -> Vec<(&str, usize)> {
    report["quality"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| {
            (
                row["name"].as_str().unwrap(),
                row["line"].as_u64().unwrap() as usize,
            )
        })
        .collect()
}

#[test]
fn quality_is_opt_in_and_counts_for_strict_and_sarif() {
    let tree = tempfile::tempdir().unwrap();
    let root = tree.path();
    put(root, "source.py", "print('hello')\n");
    put(
        root,
        "pyproject.toml",
        r#"[build-system]
requires = ["build-only"]
[project]
name = "example"
dependencies = [
    # This dependency's name is on the next line.
    "not_used[extras]>=1; python_version >= '3.11'",
]
[project.optional-dependencies]
docs = ["docs-only"]
[dependency-groups]
dev = ["dev-only"]
"#,
    );
    assert!(scan(root, &["."], 0).get("quality").is_none());
    let selected = scan(root, &[".", "--select", "sky-u005"], 0);
    assert_eq!(findings(&selected), [("not-used", 7)]);
    let report = scan(
        root,
        &[".", "--quality", "--strict", "--sarif", "report.sarif"],
        1,
    );
    assert_eq!(findings(&report), [("not-used", 7)]);
    let row = &report["quality"][0];
    assert_eq!(row["type"], "dependency");
    assert_eq!(row["rule_id"], "SKY-U005");
    assert_eq!(row["severity"], "MEDIUM");
    assert_eq!(row["basename"], "pyproject.toml");
    assert_eq!(report["gate"]["exit_code"], 1);
    assert_eq!(
        report["analysis_summary"]["unused_dependency_analysis"]["complete"],
        true
    );
    let sarif: Value =
        serde_json::from_slice(&fs::read(root.join("report.sarif")).unwrap()).unwrap();
    assert_eq!(
        sarif["runs"][0]["results"][0]["locations"][0]["physicalLocation"]["region"]["startLine"],
        7
    );
}

#[test]
fn quality_selection_in_cli_configuration_enables_the_rule() {
    let tree = tempfile::tempdir().unwrap();
    let root = tree.path();
    put(
        root,
        ".skylos.toml",
        "root = true\nquality_enabled = true\n",
    );
    put(root, "requirements.txt", "unused\n");
    put(root, "app.py", "print('hello')\n");
    assert_eq!(findings(&scan(root, &["."], 0)), [("unused", 1)]);
}

#[test]
fn static_setuptools_declarations_use_value_lines_without_executing_setup() {
    let tree = tempfile::tempdir().unwrap();
    let root = tree.path();
    put(
        root,
        "setup.cfg",
        "[metadata]\nname = app\n[options]\ninstall_requires =\n    requests>=2\n    cfg-unused\nsetup_requires = build-option\n[options.extras_require]\nfeature = optional\n",
    );
    put(
        root,
        "setup.py",
        "import setuptools as tools\nrequirements = [\n    'pyyaml',\n    'setup-unused',\n]\nopen('executed', 'w').close()\ntools.setup(name='app', install_requires=requirements, setup_requires=['build-helper'])\n",
    );
    put(
        root,
        "application.py",
        "import requests\nimport yaml\nprint(yaml)\n",
    );
    let report = scan(root, &[".", "--quality"], 0);
    assert_eq!(findings(&report), [("cfg-unused", 6), ("setup-unused", 4)]);
    assert_eq!(report["quality"][0]["basename"], "setup.cfg");
    assert_eq!(report["quality"][1]["basename"], "setup.py");
    assert!(!root.join("executed").exists());
}

#[test]
fn statically_resolves_guarded_setup_and_concatenated_requirements() {
    let tree = tempfile::tempdir().unwrap();
    let root = tree.path();
    put(
        root,
        "setup.py",
        "from setuptools import setup\nbase = ['requests']\nextras = ['unused-in-concat']\nif __name__ == '__main__':\n    setup(install_requires=base + extras)\n",
    );
    put(root, "application.py", "import requests\n");
    let report = scan(root, &[".", "--quality"], 0);
    assert_eq!(findings(&report), [("unused-in-concat", 3)]);
}

#[test]
fn nested_requirements_follow_lock_inputs_and_leave_constraints_and_pins_out() {
    let tree = tempfile::tempdir().unwrap();
    let root = tree.path();
    put(
        root,
        "requirements.txt",
        "# runtime\n-r service/current.txt\n-c service/constraints.txt\n",
    );
    put(
        root,
        "service/current.txt",
        "# This file was autogenerated by uv via the following command:\nused==1 \\\n  --hash=sha256:1111\n  # via -r install.in\ntransitive-only==2\n  # via used\n",
    );
    put(
        root,
        "service/install.in",
        "--requirement=more.in\nleftover\n",
    );
    put(root, "service/more.in", "used\n");
    put(
        root,
        "service/constraints.txt",
        "constraints-only==2\n-r pinned.txt\n",
    );
    put(root, "service/pinned.txt", "also-constraint-only==3\n");
    put(root, "main.py", "import used\n");
    let report = scan(root, &[".", "--quality"], 0);
    assert_eq!(findings(&report), [("leftover", 2)]);
    assert!(
        report["quality"][0]["file"]
            .as_str()
            .unwrap()
            .ends_with("/service/install.in")
    );
}

#[test]
fn uses_the_report_directory_for_an_enclosing_manifest_and_skips_explicit_files() {
    let tree = tempfile::tempdir().unwrap();
    let root = tree.path();
    put(root, "requirements.txt", "requests\npyserial\n");
    put(root, "apps/web/app.py", "import requests\n");
    let report = scan(
        root,
        &["apps/web", "--quality", "--sarif", "fallback.sarif"],
        0,
    );
    assert_eq!(findings(&report), [("pyserial", 0)]);
    let row = &report["quality"][0];
    assert_eq!(row["basename"], "");
    assert!(row["file"].as_str().unwrap().ends_with("/apps/web"));
    let sarif: Value =
        serde_json::from_slice(&fs::read(root.join("fallback.sarif")).unwrap()).unwrap();
    assert!(
        sarif["runs"][0]["results"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["ruleId"] == "SKY-U005" && item.get("locations").is_none())
    );

    let report = scan(root, &["apps/web/app.py", "--quality", "--strict"], 2);
    assert!(findings(&report).is_empty());
    let receipt = &report["analysis_summary"]["unused_dependency_analysis"];
    assert_eq!(receipt["status"], "not_run");
    assert_eq!(receipt["reasons"][0]["kind"], "file_scoped_scan");
}

#[test]
fn recognized_usage_and_install_only_control_agree_across_cache_states() {
    let tree = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let root = tree.path();
    put(
        root,
        "requirements.txt",
        "django-csp\ndjango-bmemcached\nnewrelic\nzensical\nmy-console-tool\ndjango-stubs\ntypes-pyyaml\nmypkg-plugin\npypydispatcher\npython-consul2\nsphinx-click\nflake8-docstrings\npytest-rerunfailures\npytest-subtests\npytest-click\npyyaml\nrequests\npsycopg2\n",
    );
    put(
        root,
        "app/settings.py",
        r#"from typing import TYPE_CHECKING
from pydispatch import dispatcher
import consul
if TYPE_CHECKING:
    import requests
MIDDLEWARE = ["csp.middleware.CSPMiddleware"]
CACHES = {"default": {"BACKEND": "django_bmemcached.memcached.BMemcached"}}
"#,
    );
    put(
        root,
        "tests/test_cli.py",
        "import yaml\ndef test_cli(cli_runner, subtests):\n    assert cli_runner is not None\n    assert subtests is not None\n",
    );
    put(
        root,
        "Procfile",
        "web: newrelic-admin run-program gunicorn app.wsgi\n",
    );
    put(
        root,
        "docker/Dockerfile",
        "RUN uv pip install -r requirements.txt my-console-tool && zensical build\nRUN echo my-console-tool\n",
    );
    put(
        root,
        "mypy.ini",
        "[mypy]\nplugins =\n    mypy_django_plugin.main\n",
    );
    put(root, "docs/conf.py", "extensions = ['sphinx_click']\n");
    put(
        root,
        "tox.ini",
        "[testenv]\ncommands = pytest --reruns 3 tests\n",
    );
    put(
        root,
        "setup.cfg",
        "[options.entry_points]\nconsole_scripts =\n    serve = mypkg_plugin.cli:main\n[flake8]\nextend-ignore = D102, D104\n",
    );
    // The package-entry-point graph cannot resolve this external callable.
    // The dependency receipt still records the positive declaration evidence.
    let clean = scan(root, &[".", "--quality"], 2);
    assert_eq!(
        clean["analysis_summary"]["unused_dependency_analysis"]["status"],
        "complete"
    );
    assert_eq!(findings(&clean), [("my-console-tool", 5)], "{clean}");
    let kinds: Vec<_> = clean["dependency_usage_evidence"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|item| item["kind"].as_str())
        .collect();
    for kind in [
        "python_import",
        "python_configuration",
        "tool_configuration",
        "package_entrypoint",
        "console_command",
        "pytest_fixture",
        "pytest_configuration",
    ] {
        assert!(kinds.contains(&kind), "missing {kind} in {clean}");
    }
    let cached = [
        ".",
        "--quality",
        "--workers",
        "4",
        "--cache-dir",
        cache.path().to_str().unwrap(),
    ];
    for _ in 0..2 {
        let mut report = scan(root, &cached, 2);
        report.as_object_mut().unwrap().remove("cache");
        assert_eq!(report, clean);
    }
    put(
        root,
        "docker/Dockerfile",
        "RUN zensical build && my-console-tool check\n",
    );
    let mut edited = scan(root, &cached, 2);
    edited.as_object_mut().unwrap().remove("cache");
    assert_eq!(edited, scan(root, &[".", "--quality", "--workers", "2"], 2));
    assert!(findings(&edited).is_empty());
}

#[test]
fn deployment_argv_counts_invoked_tools_and_modules_only() {
    let tree = tempfile::tempdir().unwrap();
    let root = tree.path();
    put(
        root,
        "requirements.txt",
        "pyyaml\nwaitress\nonly-argument\n",
    );
    put(root, "app.py", "print('hello')\n");
    put(
        root,
        "compose.yml",
        "services:\n  web:\n    entrypoint: [waitress-serve, 'only-argument']\n    command:\n      - python\n      - -m\n      - yaml\n",
    );
    assert_eq!(
        findings(&scan(root, &[".", "--quality"], 0)),
        [("only-argument", 3)]
    );
}

#[test]
fn conventional_requirements_directory_excludes_nested_constraints() {
    let tree = tempfile::tempdir().unwrap();
    let root = tree.path();
    put(
        root,
        "requirements/prod.txt",
        "-c constraints.txt\nused\nunused\n",
    );
    put(
        root,
        "requirements/constraints.txt",
        "-r constraints-extra.txt\nconstrained==1\n",
    );
    put(
        root,
        "requirements/constraints-extra.txt",
        "also-constrained==1\n",
    );
    put(root, "app.py", "import used\n");
    assert_eq!(
        findings(&scan(root, &[".", "--quality"], 0)),
        [("unused", 3)]
    );
}

#[test]
fn type_stub_sources_supply_dependency_use() {
    let tree = tempfile::tempdir().unwrap();
    let root = tree.path();
    put(root, "requirements.txt", "pyyaml\npyserial\n");
    put(root, "typings/schema.pyi", "from yaml import SafeLoader\n");
    let report = scan(root, &[".", "--quality"], 0);
    assert_eq!(findings(&report), [("pyserial", 2)]);
    assert_eq!(
        report["dependency_usage_evidence"][0]["distribution"],
        "pyyaml"
    );
}

#[test]
fn incomplete_input_and_dynamic_import_abstain_and_keep_known_use() {
    let tree = tempfile::tempdir().unwrap();
    let root = tree.path();
    put(
        root,
        "requirements.txt",
        "requests\npyserial\n-r missing.in\n",
    );
    put(
        root,
        "app.py",
        "from importlib import import_module as load\nimport requests\nload('requests.adapters')\nload(input())\n",
    );
    let report = scan(root, &[".", "--quality", "--strict"], 2);
    assert!(findings(&report).is_empty());
    let receipt = &report["analysis_summary"]["unused_dependency_analysis"];
    assert_eq!(receipt["status"], "incomplete");
    assert_eq!(
        receipt["abstained_dependencies"],
        serde_json::json!(["pyserial"])
    );
    let reasons: Vec<_> = receipt["reasons"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["kind"].as_str().unwrap())
        .collect();
    assert_eq!(
        reasons,
        ["unresolved_dependency_input", "unresolved_dynamic_import"]
    );
    assert_eq!(report["gate"]["verdict"], "incomplete");
    assert!(
        report["dependency_usage_evidence"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["kind"] == "literal_dynamic_import")
    );
}

#[test]
fn ambiguous_provider_abstention_does_not_erase_an_independent_finding() {
    let tree = tempfile::tempdir().unwrap();
    let root = tree.path();
    put(
        root,
        "requirements.txt",
        "opencv-python\nopencv-python-headless\nindependent\n",
    );
    put(root, "app.py", "import cv2\n");
    let report = scan(root, &[".", "--quality"], 2);
    assert_eq!(findings(&report), [("independent", 3)]);
    let receipt = &report["analysis_summary"]["unused_dependency_analysis"];
    assert_eq!(receipt["abstained_count"], 2);
    assert_eq!(
        receipt["reasons"][0]["kind"],
        "ambiguous_distribution_mapping"
    );
}

#[test]
fn ignored_quality_rule_does_not_access_dependency_inputs() {
    let tree = tempfile::tempdir().unwrap();
    let root = tree.path();
    put(root, "requirements.txt", "-r missing.txt\n");
    put(root, ".skylos.toml", "root = true\nignore = ['SKY-U005']\n");
    put(root, "app.py", "print('hello')\n");
    let report = scan(root, &[".", "--quality"], 0);
    assert!(findings(&report).is_empty());
    assert_eq!(
        report["analysis_summary"]["unused_dependency_analysis"]["reasons"][0]["kind"],
        "rule_ignored"
    );
}
