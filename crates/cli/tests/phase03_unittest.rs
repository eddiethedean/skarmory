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
    certain_evidence(report, name, "unittest_discovery")
}

fn certain_evidence(report: &Value, name: &str, kind: &str) -> bool {
    report["dead_code_evidence"]["symbols"]
        .as_array()
        .unwrap()
        .iter()
        .any(|symbol| {
            symbol["qualified_name"] == name
                && symbol["evidence"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|edge| edge["kind"] == kind && edge["certainty"] == "certain")
        })
}

fn unused(report: &Value, bucket: &str, name: &str) -> bool {
    report[bucket]
        .as_array()
        .unwrap()
        .iter()
        .any(|row| row["full_name"] == name)
}

fn test_only_whitelisted(report: &Value, file: &str, name: &str) -> bool {
    report["whitelisted"].as_array().unwrap().iter().any(|row| {
        row["file"] == file
            && row["full_name"] == name
            && row["reason"] == "test-only path"
            && row["suppression_code"] == "non_library_path"
            && row["folder_role"] == "test"
            && row["dead_code_evidence"]
                .as_array()
                .unwrap()
                .iter()
                .any(|event| event["role"] == "supports_dead")
    })
}

#[test]
fn test_path_whitelists_dead_bindings_without_opening_their_bodies() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    fs::write(root.join(".skylos.toml"), "root = true\n").unwrap();
    fs::write(
        root.join("test_service.py"),
        r#"from unittest import TestCase
from support import publish_record

class ServiceTests(TestCase):
    def test_service(self): pass

def setUpModule(): pass

def prepare_record():
    publish_record()
"#,
    )
    .unwrap();
    fs::write(root.join("support.py"), "def publish_record(): pass\n").unwrap();
    fs::create_dir(root.join("tests")).unwrap();
    let support = "def prepare_record(): pass\n";
    fs::write(root.join("tests/helpers.py"), support).unwrap();

    let report = scan(root, None);
    assert!(discovery(&report, "test_service.setUpModule"));
    assert!(unused(
        &report,
        "unused_functions",
        "support.publish_record"
    ));
    for (file, name) in [
        ("test_service.py", "test_service.prepare_record"),
        ("tests/helpers.py", "tests.helpers.prepare_record"),
    ] {
        assert!(!unused(&report, "unused_functions", name));
        assert!(test_only_whitelisted(&report, file, name), "{report}");
    }
    let cache = tempfile::tempdir().unwrap();
    for _ in 0..2 {
        let mut cached = scan(root, Some(cache.path()));
        cached.as_object_mut().unwrap().remove("cache");
        assert_eq!(cached, report);
    }

    fs::rename(root.join("tests/helpers.py"), root.join("helpers.py")).unwrap();
    let moved = scan(root, None);
    assert!(unused(&moved, "unused_functions", "helpers.prepare_record"));
    let mut cached = scan(root, Some(cache.path()));
    cached.as_object_mut().unwrap().remove("cache");
    assert_eq!(cached, moved);
}

#[test]
fn unittest_keeps_tests_with_proven_identity_decorators() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    fs::write(root.join(".skylos.toml"), "root = true\n").unwrap();
    fs::write(
        root.join("test_service.py"),
        r#"from unittest import TestCase

def label(test):
    return test

def exercise_service(): return 1
def abandoned(): return 2

class ServiceTests(TestCase):
    @label
    def test_service(self):
        return exercise_service()
"#,
    )
    .unwrap();
    let report = scan(root, None);
    assert!(discovery(&report, "test_service.ServiceTests.test_service"));
    assert!(!unused(
        &report,
        "unused_functions",
        "test_service.exercise_service"
    ));
    assert!(test_only_whitelisted(
        &report,
        "test_service.py",
        "test_service.abandoned"
    ));
}

#[test]
fn unittest_lifecycle_hooks_require_a_discovered_test() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    fs::write(root.join(".skylos.toml"), "root = true\n").unwrap();
    fs::write(
        root.join("test_service.py"),
        r#"from unittest import TestCase

def create_database(): return seed_rows()
def seed_rows(): return 1
def unused_cleanup(): return 2

setUpModule = create_database
setUpModule: object
create_database = None
tearDownModule = unused_cleanup
tearDownModule = None

class ServiceTests(TestCase):
    def test_service(self): return 1
"#,
    )
    .unwrap();
    let empty_path = root.join("test_empty.py");
    let empty = r#"from unittest import TestCase

def configure(): return 1
setUpModule = configure
def tearDownModule(): return 2
def configure_class(): return 3
def finish_class(): return 4

class Empty(TestCase):
    @classmethod
    def setUpClass(cls): return configure_class()

    @classmethod
    def tearDownClass(cls): return finish_class()
"#;
    fs::write(&empty_path, empty).unwrap();

    let report = scan(root, None);
    assert!(discovery(&report, "test_service.create_database"));
    assert!(!unused(
        &report,
        "unused_functions",
        "test_service.seed_rows"
    ));
    assert!(test_only_whitelisted(
        &report,
        "test_service.py",
        "test_service.unused_cleanup"
    ));
    let module_hooks = ["test_empty.configure", "test_empty.tearDownModule"];
    let class_hooks = [
        "test_empty.Empty.setUpClass",
        "test_empty.Empty.tearDownClass",
    ];
    let helpers = ["test_empty.configure_class", "test_empty.finish_class"];
    assert!(discovery(&report, "test_empty.Empty"));
    for hook in module_hooks.into_iter().chain(class_hooks) {
        assert!(!discovery(&report, hook), "{report}");
        assert!(
            test_only_whitelisted(&report, "test_empty.py", hook),
            "{report}"
        );
    }
    for name in helpers {
        assert!(
            test_only_whitelisted(&report, "test_empty.py", name),
            "{report}"
        );
    }
    let cache = tempfile::tempdir().unwrap();
    for _ in 0..2 {
        let mut cached = scan(root, Some(cache.path()));
        cached.as_object_mut().unwrap().remove("cache");
        assert_eq!(cached, report);
    }

    fs::write(
        &empty_path,
        format!("{empty}\n    def test_service(self): pass\n"),
    )
    .unwrap();
    let report = scan(root, None);
    assert!(discovery(&report, "test_empty.Empty.test_service"));
    for hook in module_hooks.into_iter().chain(class_hooks) {
        assert!(discovery(&report, hook), "{report}");
    }
    for helper in helpers {
        assert!(!unused(&report, "unused_functions", helper), "{report}");
        assert!(
            !test_only_whitelisted(&report, "test_empty.py", helper),
            "{report}"
        );
    }
    let mut cached = scan(root, Some(cache.path()));
    cached.as_object_mut().unwrap().remove("cache");
    assert_eq!(cached, report);
}

#[test]
fn unittest_module_hooks_follow_the_discovered_case_to_its_defining_module() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    fs::write(root.join(".skylos.toml"), "root = true\n").unwrap();
    fs::write(
        root.join("service_cases.py"),
        r#"from unittest import TestCase

def prepare(): return seed()
def seed(): return 1
setUpModule = prepare
def tearDownModule(): return 2

class ServiceCase(TestCase):
    def test_service(self): return 1
"#,
    )
    .unwrap();
    let loader = root.join("tests.py");
    let source = r#"from service_cases import ServiceCase as ImportedCase

def setUpModule(): return 3
def tearDownModule(): return 4
"#;
    fs::write(&loader, source).unwrap();

    let report = scan(root, None);
    for name in ["service_cases.prepare", "service_cases.tearDownModule"] {
        assert!(discovery(&report, name), "{name}");
    }
    assert!(!unused(&report, "unused_functions", "service_cases.seed"));
    for name in ["tests.setUpModule", "tests.tearDownModule"] {
        assert!(unused(&report, "unused_functions", name), "{name}");
    }

    let cache = tempfile::tempdir().unwrap();
    for _ in 0..2 {
        let mut cached = scan(root, Some(cache.path()));
        cached.as_object_mut().unwrap().remove("cache");
        assert_eq!(cached, report);
    }

    let pytest_loader = root.join("test_service.py");
    fs::rename(&loader, &pytest_loader).unwrap();
    let report = scan(root, None);
    assert!(discovery(&report, "service_cases.prepare"));
    for name in ["test_service.setUpModule", "test_service.tearDownModule"] {
        assert!(!discovery(&report, name), "{name}");
        assert!(
            certain_evidence(&report, name, "pytest_xunit_fixture"),
            "{name}"
        );
        assert!(!unused(&report, "unused_functions", name), "{name}");
    }

    fs::write(&pytest_loader, format!("{source}\nImportedCase = None\n")).unwrap();
    let report = scan(root, None);
    for name in [
        "service_cases.prepare",
        "service_cases.seed",
        "service_cases.tearDownModule",
        "test_service.setUpModule",
        "test_service.tearDownModule",
    ] {
        assert!(unused(&report, "unused_functions", name), "{name}");
    }
    let mut cached = scan(root, Some(cache.path()));
    cached.as_object_mut().unwrap().remove("cache");
    assert_eq!(cached, report);
}

#[test]
fn unittest_module_hooks_report_an_overridden_class_module() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    fs::write(root.join(".skylos.toml"), "root = true\n").unwrap();
    fs::write(
        root.join("tests.py"),
        r#"from unittest import TestCase

def setUpModule(): return 1

class ServiceCase(TestCase):
    __module__ = "other"
    def test_service(self): return 2
"#,
    )
    .unwrap();

    let report = scan_with_status(root, None, 2);
    assert_eq!(report["coverage"]["status"], "incomplete");
    assert!(discovery(&report, "tests.ServiceCase.test_service"));
    assert!(!discovery(&report, "tests.setUpModule"));
    assert!(
        report["analysis_errors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|error| error["kind"] == "unresolved_unittest_fixture_module")
    );
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
