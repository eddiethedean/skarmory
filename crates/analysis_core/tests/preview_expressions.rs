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
fn combined_call_and_expression_depth_is_bounded_on_a_small_stack() {
    use std::fmt::Write as _;
    use std::thread;

    for (functions, operators, expected) in [(5, 6, "complete"), (38, 60, "incomplete")] {
        let mut source = String::from("def dead(): pass\n");
        for index in 0..functions - 1 {
            writeln!(
                source,
                "def f{index}():\n    return {}f{}()",
                "not ".repeat(operators),
                index + 1
            )
            .unwrap();
        }
        writeln!(source, "def f{}(): return True\nf0()", functions - 1).unwrap();
        let project = Project::new(&[("main.py", &source)]);
        let regular = project.run(&ScanOptions::default());
        let small_stack = thread::Builder::new()
            .stack_size(1024 * 1024)
            .spawn(move || project.run(&ScanOptions::default()))
            .unwrap()
            .join()
            .unwrap();
        assert_eq!(small_stack, regular);
        assert_eq!(small_stack["coverage"]["status"], expected, "{small_stack}");
        if expected == "complete" {
            assert_eq!(findings(&small_stack), ["dead"]);
        } else {
            assert_eq!(
                small_stack["coverage"]["issues"][0]["kind"],
                "analysis_limit"
            );
            assert!(
                small_stack["coverage"]["issues"][0]["message"]
                    .as_str()
                    .unwrap()
                    .contains("evaluation depth"),
                "{small_stack}"
            );
            assert!(findings(&small_stack).is_empty());
        }
    }
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
fn escaped_closure_can_run_after_a_later_public_call_loads_a_module() {
    let project = Project::new(&[
        (
            "api.py",
            "import switches
from switches import mode as original
def hidden(): return 197
def factory():
    if switches.mode is original:
        def callback():
            if switches.mode is not original:
                return hidden()
        return callback
def arm():
    import switches.mode
__all__ = ['factory', 'arm']
",
        ),
        ("switches/__init__.py", "def mode(): pass\n"),
        ("switches/mode.py", "pass\n"),
    ]);
    let report = project.run(&ScanOptions {
        policy: ScanPolicy::Library,
        public_modules: vec!["api".into()],
        ..ScanOptions::default()
    });
    assert_eq!(report["coverage"]["status"], "complete", "{report}");
    assert_eq!(findings(&report), ["mode"], "{report}");
    let uses: Vec<_> = report["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| event["symbol_id"].as_str().unwrap().contains("::hidden@"))
        .collect();
    assert!(!uses.is_empty(), "{report}");
    assert!(uses.iter().all(|event| event["certainty"] == "possible"));
}

#[test]
fn independent_factories_complete_without_branching_on_their_order() {
    use std::fmt::Write as _;

    let mut source = String::from("def dead(): pass\n");
    let mut options = ScanOptions {
        policy: ScanPolicy::Library,
        ..ScanOptions::default()
    };
    for index in 0..6 {
        writeln!(
            source,
            "def factory{index}():\n    def saved(): pass\n    return saved"
        )
        .unwrap();
        options.public_callables.push(format!("api:factory{index}"));
    }
    let project = Project::new(&[("api.py", &source)]);
    let report = project.run(&options);
    assert_eq!(report["coverage"]["status"], "complete", "{report}");
    assert_eq!(findings(&report), ["dead"]);
    assert_eq!(
        report["evidence"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|event| event["certainty"] == "possible" && event["kind"] == "escape")
            .count(),
        6,
        "{report}"
    );
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
fn rich_comparison_results_can_reach_a_library_helper() {
    for comparison in [
        "value == 0",
        "value != 0",
        "[value] < [0]",
        "value == 0 == 1",
        "0 == 0 == value",
    ] {
        let source = format!(
            "def live(): return 17
def dead(): return 23
def entry(value):
    if ([value] == [0]) is None:
        return dead()
    if ({comparison}) is None:
        return live()
__all__ = ['entry']
",
        );
        let project = Project::new(&[("api.py", &source)]);
        let report = project.run(&ScanOptions {
            policy: ScanPolicy::Library,
            public_modules: vec!["api".into()],
            ..ScanOptions::default()
        });
        assert_eq!(
            report["coverage"]["status"], "complete",
            "{comparison}: {report}"
        );
        assert_eq!(findings(&report), ["dead"], "{comparison}: {report}");
        assert!(
            report["evidence"].as_array().unwrap().iter().any(|event| {
                event["kind"] == "call"
                    && event["certainty"] == "possible"
                    && event["symbol_id"].as_str().unwrap().contains("::live@")
            }),
            "{comparison}: {report}"
        );
    }
}

#[test]
fn externally_mutable_values_cannot_prove_a_helper_unreachable() {
    for (source, reason) in [
        (
            "flags = [True]
def hidden(): pass
def entry():
    if not flags:
        hidden()
__all__ = ['flags', 'entry']
",
            "export",
        ),
        (
            "flags = [True]
def hidden(): pass
def getter(): return flags
def entry():
    if not flags:
        hidden()
__all__ = ['getter', 'entry']
",
            "returned",
        ),
        (
            "def hidden(): pass
def entry(value):
    flags = [True]
    value == flags
    if not flags:
        hidden()
__all__ = ['entry']
",
            "comparison",
        ),
        (
            "def hidden(): pass
def factory():
    flags = [True]
    def callback():
        if not flags:
            hidden()
    return (flags, callback)
__all__ = ['factory']
",
            "returned",
        ),
        (
            "def hidden(): pass
def entry(value):
    flags = [True]
    value in [flags]
    if not flags:
        hidden()
__all__ = ['entry']
",
            "comparison",
        ),
    ] {
        let project = Project::new(&[("api.py", source)]);
        let report = project.run(&ScanOptions {
            policy: ScanPolicy::Library,
            public_modules: vec!["api".into()],
            ..ScanOptions::default()
        });
        assert_eq!(report["coverage"]["status"], "incomplete", "{report}");
        assert!(findings(&report).is_empty(), "{report}");
        assert!(
            report["coverage"]["issues"]
                .as_array()
                .unwrap()
                .iter()
                .any(|issue| {
                    issue["kind"] == "unbounded_use"
                        && issue["message"].as_str().is_some_and(|message| {
                            message.contains("mutable") && message.contains(reason)
                        })
                }),
            "{reason}: {report}"
        );
    }
}

#[test]
fn returning_a_fresh_container_keeps_coverage_complete() {
    let project = Project::new(&[(
        "api.py",
        "private = [True]
metadata = (1, 2)
def dead(): pass
def getter():
    fresh = [True]
    return (fresh,)
__all__ = ['getter', 'metadata']
",
    )]);
    let report = project.run(&ScanOptions {
        policy: ScanPolicy::Library,
        public_modules: vec!["api".into()],
        ..ScanOptions::default()
    });
    assert_eq!(report["coverage"]["status"], "complete", "{report}");
    assert_eq!(findings(&report), ["dead"], "{report}");
}

#[test]
fn public_tuple_retains_nested_callables_and_their_callees() {
    let project = Project::new(&[(
        "api.py",
        "def helper(): pass
def callback(): helper()
def direct(): pass
def private(): pass
callbacks = ((callback,),)
metadata = (1, 'ready')
__all__ = ['callbacks', 'direct', 'metadata']
",
    )]);
    let report = project.run(&ScanOptions {
        policy: ScanPolicy::Library,
        public_modules: vec!["api".into()],
        ..ScanOptions::default()
    });
    assert_eq!(report["coverage"]["status"], "complete", "{report}");
    assert_eq!(findings(&report), ["private"], "{report}");
    let evidence = report["evidence"].as_array().unwrap();
    for (symbol, kind, certainty) in [
        ("::callback@", "escape", "possible"),
        ("::helper@", "call", "possible"),
        ("::direct@", "public_export", "established"),
    ] {
        assert!(
            evidence.iter().any(|event| {
                event["kind"] == kind
                    && event["certainty"] == certainty
                    && event["symbol_id"].as_str().unwrap().contains(symbol)
            }),
            "{symbol}: {report}"
        );
    }
}

#[test]
fn exported_modules_require_incomplete_coverage() {
    for binding in ["exported = helper", "exported = (helper,)"] {
        let source = format!("import helper\n{binding}\n__all__ = ['exported']\n");
        let project = Project::new(&[
            ("api.py", &source),
            ("helper.py", "def called_by_client(): pass\n"),
        ]);
        let report = project.run(&ScanOptions {
            policy: ScanPolicy::Library,
            public_modules: vec!["api".into()],
            ..ScanOptions::default()
        });
        assert_eq!(report["coverage"]["status"], "incomplete", "{report}");
        assert!(findings(&report).is_empty(), "{report}");
        assert!(
            report["coverage"]["issues"]
                .as_array()
                .unwrap()
                .iter()
                .any(|issue| {
                    issue["kind"] == "unbounded_use"
                        && issue["message"].as_str().unwrap().contains("exported")
                }),
            "{report}"
        );
    }
}

#[test]
fn a_public_callable_does_not_select_its_modules_all_exports() {
    let project = Project::new(&[
        (
            "api.py",
            "import helper
def chosen(): helper.reachable()
def other(): pass
__all__ = ['helper', 'other']
",
        ),
        ("helper.py", "def reachable(): pass\ndef unused(): pass\n"),
    ]);
    let mut options = ScanOptions {
        policy: ScanPolicy::Library,
        public_callables: vec!["api:chosen".into()],
        ..ScanOptions::default()
    };
    let callable = project.run(&options);
    assert_eq!(callable["coverage"]["status"], "complete", "{callable}");
    assert_eq!(findings(&callable), ["other", "unused"]);

    options.public_modules.push("api".into());
    let combined = project.run(&options);
    assert_eq!(combined["coverage"]["status"], "incomplete", "{combined}");
    assert!(findings(&combined).is_empty(), "{combined}");
    assert_eq!(combined["coverage"]["issues"][0]["kind"], "unbounded_use");
    let evidence = combined["evidence"].as_array().unwrap();
    for (symbol, kind) in [
        ("::chosen@", "public_callable"),
        ("::other@", "public_export"),
    ] {
        assert!(
            evidence.iter().any(|event| {
                event["kind"] == kind && event["symbol_id"].as_str().unwrap().contains(symbol)
            }),
            "{symbol}: {combined}"
        );
    }
}

#[test]
fn returned_callback_can_share_a_container_with_its_caller() {
    let project = Project::new(&[(
        "main.py",
        "def hidden(): pass
def factory():
    flags = [True]
    def callback():
        if not flags:
            hidden()
        return flags
    return callback
",
    )]);
    let report = project.run(&ScanOptions {
        entry_callables: vec!["main:factory".into()],
        ..ScanOptions::default()
    });
    assert_eq!(report["coverage"]["status"], "incomplete", "{report}");
    assert!(findings(&report).is_empty(), "{report}");
    assert_eq!(report["coverage"]["issues"][0]["kind"], "unbounded_use");
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
fn running_main_module_cannot_be_selected_as_a_named_entry() {
    let project = Project::new(&[
        ("__main__.py", "def unused(): pass\n"),
        ("pkg/__init__.py", ""),
        ("pkg/__main__.py", "def run(): pass\nrun()\n"),
    ]);
    let named = project.run(&ScanOptions {
        entry_modules: vec!["__main__".into()],
        ..ScanOptions::default()
    });
    assert_eq!(named["coverage"]["status"], "incomplete", "{named}");
    assert_eq!(named["coverage"]["issues"][0]["kind"], "invalid_entry");
    assert!(findings(&named).is_empty());

    let package = project.run(&ScanOptions {
        entry_modules: vec!["pkg.__main__".into()],
        ..ScanOptions::default()
    });
    assert_eq!(package["coverage"]["status"], "complete", "{package}");
    assert_eq!(findings(&package), ["unused"]);
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
