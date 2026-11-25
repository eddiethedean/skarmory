use std::fs;
use std::path::Path;
use std::process::Command;

use serde_json::Value;

fn scan_with_status(project: &Path, cached: Option<&Path>, code: i32) -> Value {
    let mut command = Command::new(env!("CARGO_BIN_EXE_skarmory"));
    command
        .current_dir(project)
        .env_remove("SKYLOS_CONFIG_FILE")
        .env_remove("DJANGO_SETTINGS_MODULE")
        .args([
            "--profile",
            "skylos-0.3",
            ".",
            "--json",
            "--no-grep-verify",
            "--confidence",
            "0",
            "--workers",
            if cached.is_some() { "4" } else { "1" },
        ]);
    if let Some(cache) = cached {
        command.arg("--cache-dir").arg(cache);
    }
    let output = command.output().unwrap();
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(output.status.code(), Some(code), "{report}");
    report
}

fn scan(project: &Path, cached: Option<&Path>) -> Value {
    let report = scan_with_status(project, cached, 0);
    assert_eq!(report["coverage"]["status"], "complete", "{report}");
    report
}

fn discovery(report: &Value, name: &str) -> bool {
    report["dead_code_evidence"]["symbols"]
        .as_array()
        .unwrap()
        .iter()
        .any(|symbol| {
            symbol["qualified_name"] == name
                && symbol["evidence"].as_array().unwrap().iter().any(|edge| {
                    edge["kind"] == "unittest_discovery" && edge["certainty"] == "certain"
                })
        })
}

fn unused(report: &Value, bucket: &str, name: &str) -> bool {
    report[bucket]
        .as_array()
        .unwrap()
        .iter()
        .any(|row| row["full_name"] == name)
}

#[test]
fn runner_flavors_preserve_callable_lifecycle_and_ignore_unexecuted_bodies() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    fs::write(root.join(".skylos.toml"), "root = true\n").unwrap();
    fs::write(
        root.join("tests.py"),
        r#"from django.test import TestCase, SimpleTestCase
from unittest import TestCase as PlainCase, IsolatedAsyncioTestCase

def used():
    return 1

def generator_body():
    return 2

def property_body():
    return 3

def overwritten_body():
    return 4

class DatabaseCase(TestCase):
    async def testAsync(self): return used()
    testAsync: object

    @classmethod
    def setUpTestData(cls): return used()

class OtherDjangoCase(SimpleTestCase):
    def testSync(self): self.assertEqual(used(), 1)
    def setUpTestData(self): return overwritten_body()

class AsyncCase(IsolatedAsyncioTestCase):
    async def asyncSetUp(self): return used()
    async def setUp(self): return overwritten_body()
    def testGenerator(self): yield generator_body()

class OrdinaryCase(PlainCase):
    async def testAsync(self): return overwritten_body()

class NonCallableCase(PlainCase):
    @property
    def testProperty(self): return property_body()

    def testReplaced(self): return overwritten_body()
    testReplaced = None
"#,
    )
    .unwrap();
    let report = scan(root, None);
    for name in [
        "DatabaseCase.testAsync",
        "DatabaseCase.setUpTestData",
        "OtherDjangoCase.testSync",
        "AsyncCase.asyncSetUp",
        "NonCallableCase",
    ] {
        assert!(discovery(&report, &format!("tests.{name}")), "{report}");
    }
    for name in [
        "OtherDjangoCase.setUpTestData",
        "AsyncCase.setUp",
        "AsyncCase.testGenerator",
        "OrdinaryCase.testAsync",
        "NonCallableCase.testProperty",
        "NonCallableCase.testReplaced",
    ] {
        assert!(!discovery(&report, &format!("tests.{name}")), "{report}");
    }
    for name in ["generator_body", "property_body", "overwritten_body"] {
        assert!(
            unused(&report, "unused_functions", &format!("tests.{name}")),
            "{report}"
        );
    }

    let cache = tempfile::tempdir().unwrap();
    scan(root, Some(cache.path()));
    let warm = scan(root, Some(cache.path()));
    for key in [
        "coverage",
        "dead_code_evidence",
        "unused_functions",
        "unused_classes",
    ] {
        assert_eq!(report[key], warm[key], "{key}");
    }
}

#[test]
fn writes_change_future_bindings_without_replacing_captured_case_bases() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    fs::write(root.join(".skylos.toml"), "root = true\n").unwrap();
    fs::write(
        root.join("tests.py"),
        r#"from unittest import TestCase as Captured
import unittest

setattr(unittest, "TestCase", object)
from unittest import TestCase as Reimported

def retired():
    return 1

def replacement():
    return 2

class ActiveCase(Captured):
    def testLive(self): return 1
    def testRetired(self): return retired()
    def testRemoved(self): return retired()

class InactiveCase(Reimported):
    def testRejected(self): return retired()

ActiveCase.testRetired = replacement
del ActiveCase.testRemoved
"#,
    )
    .unwrap();
    let report = scan(root, None);
    assert!(discovery(&report, "tests.ActiveCase.testLive"), "{report}");
    assert!(discovery(&report, "tests.replacement"), "{report}");
    for name in [
        "ActiveCase.testRetired",
        "ActiveCase.testRemoved",
        "InactiveCase",
    ] {
        assert!(!discovery(&report, &format!("tests.{name}")), "{report}");
    }
    assert!(
        unused(&report, "unused_functions", "tests.retired"),
        "{report}"
    );
    assert!(
        unused(&report, "unused_classes", "tests.InactiveCase"),
        "{report}"
    );
}

#[test]
fn unrelated_attributes_and_dynamic_assertion_operands_stay_incomplete() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    fs::write(root.join(".skylos.toml"), "root = true\n").unwrap();
    fs::write(root.join("settings.py"), "values = []\n").unwrap();
    fs::write(
        root.join("tests.py"),
        r#"import settings
from unittest import TestCase

settings.values = [1]

class Dynamic:
    def __eq__(self, other): return True

class Example(TestCase):
    def test_value(self):
        self.assertEqual(Dynamic(), 1)
"#,
    )
    .unwrap();
    let report = scan_with_status(root, None, 2);
    assert_eq!(report["coverage"]["status"], "incomplete", "{report}");
    for kind in ["unresolved_attribute_write", "unresolved_comparison"] {
        assert!(
            report["analysis_errors"]
                .as_array()
                .unwrap()
                .iter()
                .any(|row| row["kind"] == kind),
            "{report}"
        );
    }
}

#[test]
fn competing_initializers_do_not_prove_the_local_unittest_replacement() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    fs::write(root.join(".skylos.toml"), "root = true\n").unwrap();
    for (path, source) in [
        (
            "fixtures.py",
            "from unittest import TestCase\nclass Fixture(TestCase):\n    def test_original(self): return None\n",
        ),
        (
            "patch.py",
            "from fixtures import Fixture\ndef replacement(self): return replacement_helper()\ndef replacement_helper(): return 1\nFixture.test_original = replacement\n",
        ),
        (
            "test_order.py",
            "from fixtures import Fixture\ndef local(self): return 2\nFixture.test_original = local\nimport patch\n",
        ),
    ] {
        fs::write(root.join(path), source).unwrap();
    }
    let report = scan_with_status(root, None, 2);
    assert_eq!(report["coverage"]["status"], "incomplete");
    assert!(!discovery(&report, "test_order.local"));
    assert!(!unused(
        &report,
        "unused_functions",
        "patch.replacement_helper"
    ));
}
