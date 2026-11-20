use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::Value;
use skarmory_core::{ScanOptions, ScanPolicy, scan};

static NEXT_PROJECT: AtomicUsize = AtomicUsize::new(0);

struct Project(PathBuf);

impl Project {
    fn new(files: &[(&str, &str)]) -> Self {
        let root = loop {
            let path = std::env::temp_dir().join(format!(
                "skarmory-preview-expressions-{}-{}",
                std::process::id(),
                NEXT_PROJECT.fetch_add(1, Ordering::Relaxed)
            ));
            match fs::create_dir(&path) {
                Ok(()) => break path,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => panic!("create fixture directory: {error}"),
            }
        };
        let project = Self(root);
        for (name, source) in files {
            let path = project.0.join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, source).unwrap();
        }
        project
    }

    fn run(&self, options: &ScanOptions) -> Value {
        serde_json::from_str(&scan(&self.0, options).0.to_json().unwrap()).unwrap()
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn findings(report: &Value) -> Vec<&str> {
    report["findings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|finding| finding["symbol"].as_str().unwrap())
        .collect()
}

#[test]
fn call_arguments_defaults_and_short_circuit_keep_only_reached_callees() {
    let project = Project::new(&[(
        "main.py",
        "def live(): pass
def chosen(): pass
def unstored(): pass
def unused_default(): pass
def setup(): return unused_default
def dormant(): pass
def middle(): return 1
def run(target, /, *args, choice=setup(), **options):
    if args and 'target' in options:
        choice()
    target()
run(live, None, choice=chosen, target=unstored)
if False and dormant(): dormant()
if 3 < middle() < dormant(): dormant()
",
    )]);
    let report = project.run(&ScanOptions::default());
    assert_eq!(report["coverage"]["status"], "complete", "{report}");
    assert_eq!(findings(&report), ["unstored", "unused_default", "dormant"]);
}

#[test]
fn returned_closures_observe_late_bindings_and_recursion_preserves_identity() {
    let project = Project::new(&[(
        "main.py",
        "original = []
def old(): pass
def live(): pass
def factory():
    callback = old
    def inner():
        callback()
    callback = live
    return inner
def recursive(value):
    if value is original:
        recursive([])
    else:
        factory()()
recursive(original)
",
    )]);
    let report = project.run(&ScanOptions::default());
    assert_eq!(report["coverage"]["status"], "complete", "{report}");
    assert_eq!(findings(&report), ["old"]);
}

#[test]
fn boolean_results_are_bounded_and_comparison_hooks_keep_passed_callables() {
    let project = Project::new(&[(
        "main.py",
        "def target(): pass
def is_none(value): return value is None
def negate(value): return not value
def compare(value):
    if value == target:
        pass
",
    )]);
    let mut options = ScanOptions {
        policy: ScanPolicy::Library,
        public_callables: vec!["main:is_none".into(), "main:negate".into()],
        ..ScanOptions::default()
    };
    let report = project.run(&options);
    assert_eq!(report["coverage"]["status"], "complete", "{report}");
    assert_eq!(findings(&report), ["target", "compare"]);

    options.public_callables = vec!["main:compare".into()];
    let report = project.run(&options);
    assert_eq!(report["coverage"]["status"], "incomplete", "{report}");
    assert!(findings(&report).is_empty());
    assert!(
        report["evidence"].as_array().unwrap().iter().any(|event| {
            event["kind"] == "argument_escape"
                && event["certainty"] == "possible"
                && event["symbol_id"].as_str().unwrap().contains("::target@")
        }),
        "{report}"
    );
}

#[test]
fn dynamic_module_attribute_prevents_a_complete_negative_conclusion() {
    let project = Project::new(&[
        ("from_import.py", "from pkg import task\n"),
        ("attribute.py", "import pkg\npkg.task\n"),
        (
            "pkg/__init__.py",
            "def live(): pass
def __getattr__(name):
    live()
    return live
",
        ),
        ("pkg/task.py", "def shadowed(): pass\n"),
    ]);
    for entry in ["from_import", "attribute"] {
        let report = project.run(&ScanOptions {
            entry_modules: vec![entry.into()],
            ..ScanOptions::default()
        });
        assert_eq!(report["coverage"]["status"], "incomplete", "{report}");
        assert!(findings(&report).is_empty());
        assert!(
            report["coverage"]["issues"]
                .as_array()
                .unwrap()
                .iter()
                .any(|issue| issue["kind"] == "unsupported_dynamic_export"),
            "{report}"
        );
    }
}

#[test]
fn aliased_dotted_import_reads_the_rebound_package_attribute() {
    let project = Project::new(&[
        (
            "main.py",
            "import pkg.task as alias
from pkg import task
def live(): pass
if alias is task:
    live()
",
        ),
        (
            "pkg/__init__.py",
            "from . import task
def override(): pass
task = override
",
        ),
        ("pkg/task.py", "def dormant(): pass\n"),
    ]);
    let report = project.run(&ScanOptions::default());
    assert_eq!(report["coverage"]["status"], "complete", "{report}");
    assert_eq!(findings(&report), ["override", "dormant"]);
}

#[test]
fn a_local_file_cannot_supply_a_builtin_module() {
    let project = Project::new(&[
        (
            "main.py",
            "from sys import getdefaultencoding
def live(): pass
if getdefaultencoding() == 'utf-8':
    live()
",
        ),
        ("sys.py", "def getdefaultencoding(): return 'other'\n"),
    ]);
    let report = project.run(&ScanOptions::default());
    assert_eq!(report["coverage"]["status"], "incomplete", "{report}");
    assert!(findings(&report).is_empty());
    assert!(
        report["coverage"]["issues"]
            .as_array()
            .unwrap()
            .iter()
            .any(|issue| issue["kind"] == "unresolved_import"),
        "{report}"
    );
}

#[test]
fn a_rebound_package_path_prevents_static_child_selection() {
    let project = Project::new(&[
        (
            "main.py",
            "from pkg import task
def live(): pass
if task.flag():
    live()
",
        ),
        ("pkg/__init__.py", "__path__ = ['alternate']\n"),
        ("pkg/task.py", "def flag(): return False\n"),
        ("alternate/__init__.py", ""),
        ("alternate/task.py", "def flag(): return True\n"),
    ]);
    for options in [
        ScanOptions::default(),
        ScanOptions {
            entry_modules: vec!["pkg.task".into()],
            ..ScanOptions::default()
        },
    ] {
        let report = project.run(&options);
        assert_eq!(report["coverage"]["status"], "incomplete", "{report}");
        assert!(findings(&report).is_empty());
        assert!(
            report["coverage"]["issues"]
                .as_array()
                .unwrap()
                .iter()
                .any(|issue| issue["kind"] == "unsupported_import_path"),
            "{report}"
        );
    }
}

#[test]
fn named_top_level_module_and_direct_script_use_distinct_package_values() {
    let project = Project::new(&[(
        "main.py",
        "def named(): pass
def direct(): pass
if __package__ == '':
    named()
else:
    direct()
",
    )]);
    let script = project.run(&ScanOptions::default());
    assert_eq!(script["coverage"]["status"], "complete", "{script}");
    assert_eq!(findings(&script), ["named"]);

    let named = project.run(&ScanOptions {
        entry_modules: vec!["main".into()],
        ..ScanOptions::default()
    });
    assert_eq!(named["coverage"]["status"], "complete", "{named}");
    assert_eq!(findings(&named), ["direct"]);
}
