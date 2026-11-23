use std::{fs, path::Path};

use serde_json::Value;
use skarmory_core::phase02::{Options, Profile, scan};

fn compare(project: &Path, cache: &Path) -> (Value, Value) {
    let clean = scan(project, &Options::default()).unwrap();
    let cached = scan(
        project,
        &Options {
            workers: 4,
            cache_dir: Some(cache.to_owned()),
            ..Options::default()
        },
    )
    .unwrap();
    let clean: Value = serde_json::from_str(&clean.to_json().unwrap()).unwrap();
    let mut cached: Value = serde_json::from_str(&cached.to_json().unwrap()).unwrap();
    let stats = cached.as_object_mut().unwrap().remove("cache").unwrap();
    assert_eq!(cached, clean);
    (cached, stats)
}

fn finds(report: &Value, name: &str) -> bool {
    report["unused_functions"]
        .as_array()
        .unwrap()
        .iter()
        .any(|entry| entry["name"] == name)
}

#[test]
fn edits_rebuild_roots_and_recursive_reachability() {
    let project = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let config = project.path().join(".skylos.toml");
    let main = project.path().join("main.py");
    fs::write(&config, "root = true\n").unwrap();
    fs::write(
        project.path().join("helpers.py"),
        concat!(
            "def first(value):\n",
            "    if value <= 0:\n        return 1\n",
            "    return second(value - 1)\n\n",
            "def second(value):\n",
            "    if 0 >= value:\n        return 2\n",
            "    return first(value - 1)\n",
            "\nclass Label:\n    def __eq__(self, other):\n        return True\n",
        ),
    )
    .unwrap();
    fs::write(&main, "from helpers import first\nfirst(2)\n").unwrap();
    let (initial, cold) = compare(project.path(), cache.path());
    assert_eq!(initial["coverage"]["status"], "complete");
    assert_eq!(cold["misses"], 2);
    assert!(!finds(&initial, "first"));
    assert!(!finds(&initial, "second"));
    let (_, warm) = compare(project.path(), cache.path());
    assert_eq!(warm["hits"], 2);

    fs::write(&main, "pass\n").unwrap();
    let (unrooted, edited) = compare(project.path(), cache.path());
    assert_eq!(unrooted["coverage"]["status"], "complete");
    assert_eq!(edited["hits"], 1);
    assert!(finds(&unrooted, "first"));
    assert!(finds(&unrooted, "second"));

    fs::write(
        &config,
        "root = true\n[[dead_code.entrypoints]]\nname = 'first'\nmodule = 'helpers'\n",
    )
    .unwrap();
    let (rooted, reconfigured) = compare(project.path(), cache.path());
    assert_eq!(rooted["coverage"]["status"], "complete");
    assert_eq!(reconfigured["hits"], 2);
    assert!(!finds(&rooted, "first"));
    assert!(!finds(&rooted, "second"));
}

#[test]
fn function_namespace_reads_keep_local_bindings_live_across_cache_edits() {
    let project = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let pkg = project.path().join("pkg");
    fs::create_dir(&pkg).unwrap();
    let initializer = pkg.join("__init__.py");
    fs::write(pkg.join("neighbor.py"), "unread_global = 1\n").unwrap();

    let serial = Options {
        profile: Profile::SkylosPhase03,
        ..Options::default()
    };
    let parallel = Options {
        cache_dir: Some(cache.path().to_owned()),
        workers: 4,
        ..serial.clone()
    };
    let run = |options: &Options| -> Value {
        serde_json::from_str(&scan(project.path(), options).unwrap().to_json().unwrap()).unwrap()
    };

    // The first two expressions exercise the pinned Skylos 5.7.0 package
    // namespace positives. Root the function and add an independent unused control.
    for (imports, expression, expected_hit) in [
        ("", "locals()", 0),
        ("", "vars()", 1),
        ("from builtins import locals as collect", "collect()", 1),
        ("import builtins as core", "core.vars()", 1),
        ("", "0", 1),
    ] {
        fs::write(
            &initializer,
            format!(
                r#"{imports}
__all__ = ["evaluate", "control"]

def evaluate(source):
    retained_value = object()
    return {expression}

def control():
    unread_local = object()
    return 0
"#
            ),
        )
        .unwrap();

        let clean = run(&serial);
        assert_eq!(
            clean["coverage"]["status"], "complete",
            "{expression}: {clean}"
        );
        let variables = clean["unused_variables"].as_array().unwrap();
        let flagged = |name| variables.iter().any(|item| item["full_name"] == name);
        assert_eq!(flagged("pkg.evaluate.retained_value"), expression == "0");
        assert!(flagged("pkg.control.unread_local"));
        assert!(flagged("pkg.neighbor.unread_global"));

        for hits in [expected_hit, 2] {
            let mut cached = run(&parallel);
            let stats = cached.as_object_mut().unwrap().remove("cache").unwrap();
            assert_eq!(stats["hits"], hits);
            assert_eq!(cached, clean);
        }
    }
}

#[test]
fn reachable_builtin_evaluation_reports_incomplete_coverage() {
    let project = tempfile::tempdir().unwrap();
    let pkg = project.path().join("pkg");
    fs::create_dir(&pkg).unwrap();
    fs::write(pkg.join("neighbor.py"), "unread_global = 1\n").unwrap();
    let initializer = pkg.join("__init__.py");

    for profile in [Profile::SkylosPhase02, Profile::SkylosPhase03] {
        let cache = tempfile::tempdir().unwrap();
        let serial = Options {
            profile,
            ..Options::default()
        };
        let parallel = Options {
            cache_dir: Some(cache.path().to_owned()),
            workers: 4,
            ..serial.clone()
        };
        let run = |options: &Options| -> Value {
            serde_json::from_str(&scan(project.path(), options).unwrap().to_json().unwrap())
                .unwrap()
        };

        for (edit, (imports, target, shadow, export, diagnostic)) in [
            (
                "from builtins import eval as evaluate",
                "evaluate",
                "",
                "run",
                Some("eval"),
            ),
            (
                "from builtins import exec as evaluate",
                "evaluate",
                "",
                "run",
                Some("exec"),
            ),
            (
                "import builtins as runtime",
                "runtime.eval",
                "",
                "run",
                Some("eval"),
            ),
            (
                "from builtins import eval as evaluate",
                "evaluate",
                "    def evaluate(value):\n        return value\n",
                "run",
                None,
            ),
            (
                "from builtins import exec as evaluate",
                "evaluate",
                "",
                "control",
                None,
            ),
        ]
        .into_iter()
        .enumerate()
        {
            fs::write(&initializer, format!(
                "{imports}\n__all__ = [\"{export}\"]\n\ndef run(source):\n{shadow}    retained = 7\n    return {target}(source)\n\ndef control():\n    return 0\n"
            )).unwrap();

            let clean = run(&serial);
            let issues = clean["coverage"]["issues"].as_array().unwrap();
            let variables = clean["unused_variables"].as_array().unwrap();
            if let Some(name) = diagnostic {
                assert_eq!(clean["coverage"]["status"], "incomplete", "{clean}");
                assert_eq!(issues.len(), 1, "{clean}");
                assert_eq!(issues[0]["kind"], "unresolved_call");
                assert_eq!(issues[0]["path"], "pkg/__init__.py");
                assert_eq!(issues[0]["line"], 6);
                assert!(issues[0]["message"].as_str().unwrap().contains(name));
                assert!(variables.is_empty(), "{clean}");
            } else {
                assert_eq!(clean["coverage"]["status"], "complete", "{clean}");
                assert!(
                    variables
                        .iter()
                        .any(|entry| entry["full_name"] == "pkg.neighbor.unread_global"),
                    "{clean}"
                );
                assert_eq!(finds(&clean, "run"), export == "control", "{clean}");
                if export == "run" {
                    assert!(
                        variables
                            .iter()
                            .any(|entry| entry["full_name"] == "pkg.run.retained"),
                        "{clean}"
                    );
                }
            }

            for hits in [usize::from(edit > 0), 2] {
                let mut cached = run(&parallel);
                let stats = cached.as_object_mut().unwrap().remove("cache").unwrap();
                assert_eq!(stats["hits"], hits);
                assert_eq!(cached, clean);
            }
        }
    }
}

#[test]
fn pytest_lazy_exports_require_a_positional_lookup_parameter() {
    let project = tempfile::tempdir().unwrap();
    fs::write(
        project.path().join("test_example.py"),
        "from support import example as test_case\n",
    )
    .unwrap();
    let support = project.path().join("support.py");

    for profile in [Profile::SkylosPhase02, Profile::SkylosPhase03] {
        let cache = tempfile::tempdir().unwrap();
        let serial = Options {
            profile,
            ..Options::default()
        };
        let parallel = Options {
            cache_dir: Some(cache.path().to_owned()),
            workers: 4,
            ..serial.clone()
        };
        let run = |options: &Options| -> Value {
            serde_json::from_str(&scan(project.path(), options).unwrap().to_json().unwrap())
                .unwrap()
        };

        for (edit, (signature, valid)) in [
            ("*, name", false),
            ("**name", false),
            ("*name", false),
            ("name", true),
            ("name, /", true),
        ]
        .into_iter()
        .enumerate()
        {
            fs::write(
                &support,
                format!(
                    r#"def sample():
    pass

def __getattr__({signature}):
    if name == "example":
        return sample
    raise AttributeError(name)
"#
                ),
            )
            .unwrap();

            let clean = run(&serial);
            let status = if valid { "complete" } else { "incomplete" };
            assert_eq!(clean["coverage"]["status"], status, "{signature}: {clean}");
            let issues = clean["coverage"]["issues"].as_array().unwrap();
            if !valid {
                assert!(
                    issues
                        .iter()
                        .any(|issue| issue["kind"] == "unresolved_lazy_export"
                            && issue["path"] == "test_example.py"),
                    "{signature}: {clean}"
                );
            }
            let evidence = &clean["dead_code_evidence"];
            let pytest_root = match profile {
                Profile::SkylosPhase02 => evidence.as_array().unwrap().iter().any(|entry| {
                    entry["full_name"] == "support.sample" && entry["kind"] == "pytest_root"
                }),
                Profile::SkylosPhase03 => {
                    evidence["symbols"].as_array().unwrap().iter().any(|entry| {
                        entry["qualified_name"] == "support.sample"
                            && entry["evidence"]
                                .as_array()
                                .unwrap()
                                .iter()
                                .any(|event| event["kind"] == "pytest_root")
                    })
                }
            };
            assert_eq!(pytest_root, valid, "{signature}: {clean}");

            for hits in [usize::from(edit > 0), 2] {
                let mut cached = run(&parallel);
                let stats = cached.as_object_mut().unwrap().remove("cache").unwrap();
                assert_eq!(stats["hits"], hits);
                assert_eq!(cached, clean);
            }
        }
    }
}

#[test]
fn lazy_exports_report_unresolved_values_inside_containers() {
    let project = tempfile::tempdir().unwrap();
    fs::write(
        project.path().join("main.py"),
        "from support import example\nprint(example)\n",
    )
    .unwrap();
    let support = project.path().join("support.py");

    for profile in [Profile::SkylosPhase02, Profile::SkylosPhase03] {
        let cache = tempfile::tempdir().unwrap();
        let serial = Options {
            profile,
            ..Options::default()
        };
        let parallel = Options {
            cache_dir: Some(cache.path().to_owned()),
            workers: 4,
            ..serial.clone()
        };
        let run = |options: &Options| -> Value {
            serde_json::from_str(&scan(project.path(), options).unwrap().to_json().unwrap())
                .unwrap()
        };

        for (edit, (expr, incomplete)) in [
            ("{\"name\": name}", false),
            ("(name,)", false),
            ("(name != \"example\" and missing,)", false),
            ("(name, missing)", true),
            ("(missing,)", true),
            ("[{\"key\": (missing,)}]", true),
            ("{missing: 1}", true),
            ("missing", true),
            ("(load(),)", true),
            ("[{\"key\": (1,)}]", false),
        ]
        .into_iter()
        .enumerate()
        {
            fs::write(
                &support,
                format!(
                    r#"def __getattr__(name):
    if name == "example":
        return {expr}
    if name == "unselected":
        return [other_missing]
    raise AttributeError(name)
"#
                ),
            )
            .unwrap();

            let clean = run(&serial);
            let status = if incomplete { "incomplete" } else { "complete" };
            assert_eq!(clean["coverage"]["status"], status, "{expr}: {clean}");
            let issues = clean["coverage"]["issues"].as_array().unwrap();
            assert_eq!(issues.len(), usize::from(incomplete), "{expr}: {clean}");
            if let Some(issue) = issues.first() {
                assert_eq!(issue["kind"], "unresolved_lazy_export");
                assert_eq!(issue["path"], "support.py");
                assert_eq!(issue["line"], 3);
            }

            for hits in [usize::from(edit > 0), 2] {
                let mut cached = run(&parallel);
                let stats = cached.as_object_mut().unwrap().remove("cache").unwrap();
                assert_eq!(stats["hits"], hits);
                assert_eq!(cached, clean);
            }
        }
    }
}

#[test]
fn exported_callable_instances_retain_effective_call_methods() {
    let project = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    fs::write(project.path().join(".skylos.toml"), "root = true\n").unwrap();
    let pkg = project.path().join("pkg");
    fs::create_dir(&pkg).unwrap();
    fs::write(
        pkg.join("helpers.py"),
        concat!(
            "def called():\n    pass\n",
            "def inherited():\n    pass\n",
            "def public_method():\n    pass\n",
            "def decorated():\n    pass\n",
            "def unused():\n    pass\n",
        ),
    )
    .unwrap();
    let source = r#"from .helpers import called, decorated, inherited, public_method

class Client:
    def __call__(self):
        self._dispatch()

    def _dispatch(self):
        called()

    def run(self):
        public_method()

class Base:
    def __call__(self):
        inherited()

class Inherited(Base):
    pass

class Disabled(Base):
    __call__ = None

class StaticClient:
    @staticmethod
    def __call__():
        decorated()

client = Client()
child = Inherited()
disabled = Disabled()
static_client = StaticClient()
"#;
    let initializer = pkg.join("__init__.py");
    fs::write(
        &initializer,
        format!("{source}\n__all__ = ['client', 'child', 'static_client']\n"),
    )
    .unwrap();
    let (report, _) = compare(project.path(), cache.path());
    assert_eq!(report["coverage"]["status"], "complete", "{report}");
    for live in ["called", "decorated", "inherited", "public_method"] {
        assert!(!finds(&report, live), "{live}: {report}");
    }
    assert!(finds(&report, "unused"), "{report}");
    for call in [
        "pkg.Client.__call__",
        "pkg.Base.__call__",
        "pkg.StaticClient.__call__",
    ] {
        assert!(
            report["dead_code_evidence"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["full_name"] == call
                    && item["kind"] == "public_export"
                    && item["certainty"] == "possible"),
            "{call}: {report}"
        );
    }
    let (warm, stats) = compare(project.path(), cache.path());
    assert_eq!(stats["hits"], 2);
    assert_eq!(warm, report);

    fs::write(&initializer, format!("{source}\n__all__ = ['disabled']\n")).unwrap();
    let (unrooted, stats) = compare(project.path(), cache.path());
    assert_eq!(stats["hits"], 1);
    assert_eq!(unrooted["coverage"]["status"], "complete", "{unrooted}");
    for unused in [
        "called",
        "decorated",
        "inherited",
        "public_method",
        "unused",
    ] {
        assert!(finds(&unrooted, unused), "{unused}: {unrooted}");
    }
}

#[test]
fn exported_instances_with_opaque_call_methods_are_incomplete() {
    let project = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    fs::write(project.path().join(".skylos.toml"), "root = true\n").unwrap();
    fs::write(
        project.path().join("main.py"),
        concat!(
            "def choose_callback():\n    return print\n",
            "class Client:\n    __call__ = choose_callback()\n",
            "client = Client()\n__all__ = ['client']\n",
        ),
    )
    .unwrap();
    let (report, _) = compare(project.path(), cache.path());
    assert_eq!(report["coverage"]["status"], "incomplete", "{report}");
    assert!(
        report["coverage"]["issues"]
            .as_array()
            .unwrap()
            .iter()
            .any(|issue| issue["kind"] == "unresolved_export"),
        "{report}"
    );
}

#[test]
fn conditional_truth_tests_follow_instance_hooks() {
    let project = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    fs::write(project.path().join(".skylos.toml"), "root = true\n").unwrap();
    fs::write(
        project.path().join("flags.py"),
        concat!(
            "def bool_helper():\n    return True\n\n",
            "def shadowed_length():\n    return 0\n\n",
            "def length_helper():\n    return 1\n\n",
            "class Base:\n    def __bool__(self):\n        return bool_helper()\n",
            "    def __len__(self):\n        return shadowed_length()\n\n",
            "class Flag(Base):\n    pass\n\n",
            "class Sized:\n    def __len__(self):\n        return self.measure()\n",
            "    def measure(self):\n        return length_helper()\n",
        ),
    )
    .unwrap();
    let main = project.path().join("main.py");
    fs::write(
        &main,
        concat!(
            "from flags import Flag, Sized\n",
            "flag = Flag()\n",
            "if flag:\n    pass\n",
            "if not (True and Sized()):\n    pass\n",
        ),
    )
    .unwrap();
    let (active, cold) = compare(project.path(), cache.path());
    assert_eq!(cold["misses"], 2);
    assert_eq!(active["coverage"]["status"], "complete", "{active}");
    assert!(!finds(&active, "bool_helper"), "{active}");
    assert!(!finds(&active, "length_helper"), "{active}");
    assert!(finds(&active, "shadowed_length"));
    let evidence = active["dead_code_evidence"].as_array().unwrap();
    for method in ["flags.Base.__bool__", "flags.Sized.__len__"] {
        assert!(
            evidence
                .iter()
                .any(|item| { item["full_name"] == method && item["kind"] == "truth_test" }),
            "{evidence:?}"
        );
    }
    let (again, warm) = compare(project.path(), cache.path());
    assert_eq!(warm["hits"], 2);
    assert_eq!(again, active);

    fs::write(
        &main,
        concat!(
            "from flags import Flag\nFlag()\n",
            "if False and Flag():\n    pass\n",
            "disabled = False\n",
            "if (len([1]) and disabled) and Flag():\n    pass\n",
            "enabled = True\n",
            "if (len([]) or enabled) or Flag():\n    pass\n",
        ),
    )
    .unwrap();
    let (dormant, edited) = compare(project.path(), cache.path());
    assert_eq!(edited["hits"], 1);
    assert_eq!(dormant["coverage"]["status"], "complete", "{dormant}");
    assert!(finds(&dormant, "bool_helper"));
    assert!(finds(&dormant, "length_helper"));
}

#[test]
fn builtin_values_do_not_dispatch_unrelated_truth_hooks() {
    let project = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    fs::write(project.path().join(".skylos.toml"), "root = true\n").unwrap();
    fs::write(
        project.path().join("flags.py"),
        "class Flag:\n    def __bool__(self):\n        return True\n",
    )
    .unwrap();
    fs::write(
        project.path().join("main.py"),
        concat!(
            "settings = {'active': True}\n",
            "if dict(settings):\n    pass\n",
            "if list([1]):\n    pass\n",
            "if tuple([1]):\n    pass\n",
            "if range(3):\n    pass\n",
            "count = int(1)\nif count:\n    pass\n",
            "same = 1 == 1\nif same:\n    pass\n",
            "def unused():\n    pass\n",
        ),
    )
    .unwrap();

    let (report, _) = compare(project.path(), cache.path());
    assert_eq!(report["coverage"]["status"], "complete", "{report}");
    assert!(finds(&report, "unused"));
    let (warm, stats) = compare(project.path(), cache.path());
    assert_eq!(stats["hits"], 2);
    assert_eq!(warm, report);
}

#[test]
fn builtin_container_lengths_ignore_unrelated_length_hooks() {
    let project = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    fs::write(project.path().join(".skylos.toml"), "root = true\n").unwrap();
    fs::write(
        project.path().join("inventory.py"),
        "class Inventory:\n    def __len__(self):\n        return 1\n",
    )
    .unwrap();
    fs::write(
        project.path().join("main.py"),
        concat!(
            "items = [1, 2, 2]\n",
            "print(len(set(items)))\n",
            "print(len(dict(active=True)))\n",
            "print(len(list(range(3))))\n",
            "indices = range(3)\nprint(len(list(indices)))\n",
            "def unused():\n    pass\n",
        ),
    )
    .unwrap();

    let (report, cold) = compare(project.path(), cache.path());
    assert_eq!(cold["misses"], 2);
    assert_eq!(report["coverage"]["status"], "complete", "{report}");
    assert!(finds(&report, "unused"));
    let (warm, stats) = compare(project.path(), cache.path());
    assert_eq!(stats["hits"], 2);
    assert_eq!(warm, report);
}

#[test]
fn builtin_container_lengths_preserve_unresolved_constructor_effects() {
    let project = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    fs::write(project.path().join(".skylos.toml"), "root = true\n").unwrap();
    fs::write(
        project.path().join("inventory.py"),
        concat!(
            "class Inventory:\n",
            "    def __len__(self):\n        return 1\n",
            "    def __iter__(self):\n        return iter([1])\n",
            "    def __hash__(self):\n        return 1\n",
            "    def __eq__(self, other):\n        return True\n",
        ),
    )
    .unwrap();
    let main = project.path().join("main.py");
    for (source, kind) in [
        ("print(len(dict(active=missing())))\n", "unresolved_call"),
        (
            "from inventory import Inventory\nprint(len(list(Inventory())))\n",
            "unresolved_length_call",
        ),
        (
            "from inventory import Inventory\nprint(len(set([Inventory()])))\n",
            "unresolved_length_call",
        ),
        (
            "from inventory import Inventory\nif list(range(1)) == [Inventory()]:\n    pass\n",
            "unresolved_comparison",
        ),
    ] {
        fs::write(&main, source).unwrap();
        let (report, _) = compare(project.path(), cache.path());
        assert_eq!(report["coverage"]["status"], "incomplete", "{report}");
        assert!(
            report["coverage"]["issues"]
                .as_array()
                .unwrap()
                .iter()
                .any(|issue| issue["kind"] == kind),
            "{report}"
        );
    }
}

#[test]
fn set_length_does_not_trust_mutable_container_contents() {
    let project = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    fs::write(project.path().join(".skylos.toml"), "root = true\n").unwrap();
    fs::write(
        project.path().join("inventory.py"),
        concat!(
            "def record_hash():\n    return 7\n",
            "class Inventory:\n",
            "    def __len__(self):\n        return 1\n",
            "    def __hash__(self):\n        return record_hash()\n",
        ),
    )
    .unwrap();
    let main = project.path().join("main.py");
    for initial in ["[1]", "list(range(1))"] {
        fs::write(
            &main,
            format!(
                "from inventory import Inventory\n\
                 items = {initial}\n\
                 alias = items\n\
                 extend = list.extend\n\
                 extend(alias, [Inventory()])\n\
                 print(len(set(items)))\n"
            ),
        )
        .unwrap();
        let (report, _) = compare(project.path(), cache.path());
        assert_eq!(report["coverage"]["status"], "incomplete", "{report}");
        assert!(
            report["coverage"]["issues"]
                .as_array()
                .unwrap()
                .iter()
                .any(|issue| issue["kind"] == "unresolved_length_call"),
            "{report}"
        );
        assert!(!finds(&report, "record_hash"), "{report}");
        let (warm, stats) = compare(project.path(), cache.path());
        assert_eq!(stats["hits"], 2);
        assert_eq!(warm, report);
    }

    fs::write(
        &main,
        concat!(
            "print(len(set([1, 2, 2])))\n",
            "indices = range(3)\nprint(len(set(indices)))\n",
            "def unused():\n    pass\n",
        ),
    )
    .unwrap();
    let (bounded, _) = compare(project.path(), cache.path());
    assert_eq!(bounded["coverage"]["status"], "complete", "{bounded}");
    assert!(finds(&bounded, "unused"), "{bounded}");
}

#[test]
fn ranges_do_not_dispatch_container_element_equality() {
    let project = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    fs::write(project.path().join(".skylos.toml"), "root = true\n").unwrap();
    fs::write(
        project.path().join("main.py"),
        concat!(
            "def compare_entry():\n    return True\n",
            "class Entry:\n    def __eq__(self, other):\n        return compare_entry()\n",
            "indices = range(1)\n",
            "print(indices == [Entry()])\n",
            "print([Entry()] == indices)\n",
            "print(indices != {'key': Entry()})\n",
        ),
    )
    .unwrap();

    let (report, _) = compare(project.path(), cache.path());
    assert_eq!(report["coverage"]["status"], "complete", "{report}");
    assert!(finds(&report, "compare_entry"), "{report}");
    let (warm, stats) = compare(project.path(), cache.path());
    assert_eq!(stats["hits"], 1);
    assert_eq!(warm, report);
}

#[test]
fn explicit_length_calls_follow_resolved_instances() {
    let project = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    fs::write(project.path().join(".skylos.toml"), "root = true\n").unwrap();
    fs::write(
        project.path().join("inventory.py"),
        concat!(
            "def load_count():\n    return 1\n\n",
            "def check_availability():\n    return True\n\n",
            "class Collection:\n",
            "    def __len__(self):\n        return load_count()\n",
            "    def __bool__(self):\n        return check_availability()\n\n",
            "class Inventory(Collection):\n    pass\n",
        ),
    )
    .unwrap();
    let main = project.path().join("main.py");
    fs::write(
        &main,
        "from inventory import Inventory\nsize = len(Inventory())\nprint(size)\n",
    )
    .unwrap();
    let (direct, cold) = compare(project.path(), cache.path());
    assert_eq!(cold["misses"], 2);
    assert_eq!(direct["coverage"]["status"], "complete", "{direct}");
    assert!(!finds(&direct, "load_count"), "{direct}");
    assert!(finds(&direct, "check_availability"));
    let evidence = direct["dead_code_evidence"].as_array().unwrap();
    assert!(
        evidence.iter().any(|item| {
            item["full_name"] == "inventory.Collection.__len__" && item["kind"] == "length_call"
        }),
        "{evidence:?}"
    );
    let (warm, stats) = compare(project.path(), cache.path());
    assert_eq!(stats["hits"], 2);
    assert_eq!(warm, direct);

    fs::write(
        &main,
        concat!(
            "from builtins import len as count\n",
            "from inventory import Inventory\n",
            "def len(value):\n    print(value)\n    return 0\n",
            "print(len(Inventory()))\n",
            "print(count(Inventory()))\n",
        ),
    )
    .unwrap();
    let (alias, edited) = compare(project.path(), cache.path());
    assert_eq!(edited["hits"], 1);
    assert_eq!(alias["coverage"]["status"], "complete", "{alias}");
    assert!(!finds(&alias, "load_count"), "{alias}");

    fs::write(
        &main,
        concat!(
            "from inventory import Inventory\n",
            "def len(value):\n    print(value)\n    return 0\n",
            "print(len(Inventory()))\n",
        ),
    )
    .unwrap();
    let (shadowed, _) = compare(project.path(), cache.path());
    assert_eq!(shadowed["coverage"]["status"], "complete", "{shadowed}");
    assert!(finds(&shadowed, "load_count"));
}

#[test]
fn unresolved_length_calls_keep_ordering_guards_incomplete() {
    let project = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    fs::write(project.path().join(".skylos.toml"), "root = true\n").unwrap();
    let main = project.path().join("main.py");
    let source = concat!(
        "def load_count():\n    return 1\n\n",
        "class RankedItems:\n",
        "    def __len__(self):\n        return load_count()\n",
        "    def __lt__(self, other):\n        return len(self) < len(other)\n",
    );
    fs::write(
        &main,
        format!("{source}\nif len(RankedItems()) <= 0:\n    pass\n"),
    )
    .unwrap();
    let (custom, _) = compare(project.path(), cache.path());
    assert_eq!(custom["coverage"]["status"], "complete", "{custom}");
    assert!(!finds(&custom, "load_count"), "{custom}");

    fs::write(
        &main,
        format!(
            "{source}\ndef make():\n    return RankedItems()\nif len(make()) <= 0:\n    pass\n"
        ),
    )
    .unwrap();
    let (opaque, _) = compare(project.path(), cache.path());
    assert_eq!(opaque["coverage"]["status"], "incomplete", "{opaque}");
    let issues = opaque["coverage"]["issues"].as_array().unwrap();
    assert_eq!(issues.len(), 1, "{opaque}");
    assert_eq!(issues[0]["kind"], "unresolved_length_call");
    assert_eq!(opaque["unused_functions"], serde_json::json!([]));

    fs::write(&main, format!("{source}\nif len([1]) <= 0:\n    pass\n")).unwrap();
    let (builtin, _) = compare(project.path(), cache.path());
    assert_eq!(builtin["coverage"]["status"], "complete", "{builtin}");
    assert!(finds(&builtin, "load_count"));
}

#[test]
fn reachable_opaque_truth_tests_are_incomplete() {
    let project = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    fs::write(project.path().join(".skylos.toml"), "root = true\n").unwrap();
    let main = project.path().join("main.py");
    let source = concat!(
        "def observed():\n    return True\n\n",
        "class Flag:\n    def __bool__(self):\n        return observed()\n\n",
        "def dict():\n    return Flag()\n\n",
        "def check():\n    if dict():\n        pass\n\n",
        "if __name__ == '__main__':\n    print('ready')\n",
        "if len([1]):\n    pass\n",
        "def builtin_items():\n    return [1]\nprint(len(builtin_items()))\n",
        "ordered = 0 < 1\nif ordered:\n    pass\n",
        "token = object()\nis_empty = token is None\nif is_empty:\n    pass\n",
    );
    fs::write(&main, source).unwrap();
    let (dormant, _) = compare(project.path(), cache.path());
    assert_eq!(dormant["coverage"]["status"], "complete", "{dormant}");
    assert!(finds(&dormant, "observed"));

    fs::write(&main, format!("{source}check()\n")).unwrap();
    let (active, _) = compare(project.path(), cache.path());
    assert_eq!(active["coverage"]["status"], "incomplete", "{active}");
    let issues = active["coverage"]["issues"].as_array().unwrap();
    assert_eq!(issues.len(), 1, "{active}");
    assert_eq!(issues[0]["kind"], "unresolved_truth_test");
    assert_eq!(issues[0]["line"], 12);
    assert_eq!(active["unused_functions"], serde_json::json!([]));
}

#[test]
fn reachable_ordering_dispatch_stays_incomplete() {
    let project = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    fs::write(project.path().join(".skylos.toml"), "root = true\n").unwrap();
    fs::write(
        project.path().join("ranks.py"),
        concat!(
            "def hook_helper():\n    return True\n\n",
            "class Rank:\n    def __ge__(self, other):\n        return hook_helper()\n",
            "    def __neg__(self):\n        return self\n\n",
            "def check(value):\n    if 0 <= value:\n        return 1\n    return 0\n",
        ),
    )
    .unwrap();
    let main = project.path().join("main.py");
    fs::write(
        &main,
        "threshold = 0\nif threshold < 1:\n    print('ready')\n",
    )
    .unwrap();
    let (dormant, _) = compare(project.path(), cache.path());
    assert_eq!(dormant["coverage"]["status"], "complete");
    assert!(finds(&dormant, "check"));

    fs::write(&main, "from ranks import Rank, check\ncheck(Rank())\n").unwrap();
    let (live, warm) = compare(project.path(), cache.path());
    assert_eq!(warm["hits"], 1);
    assert_eq!(live["coverage"]["status"], "incomplete");
    let issues = live["coverage"]["issues"].as_array().unwrap();
    assert_eq!(issues.len(), 1, "{live}");
    assert_eq!(issues[0]["kind"], "unresolved_comparison");
    assert_eq!(issues[0]["path"], "ranks.py");
    assert_eq!(issues[0]["line"], 11);
    assert_eq!(live["unused_functions"], serde_json::json!([]));

    fs::write(
        &main,
        "from ranks import Rank\nif 0 <= -Rank():\n    print('ready')\n",
    )
    .unwrap();
    let (overloaded, _) = compare(project.path(), cache.path());
    assert_eq!(overloaded["coverage"]["status"], "incomplete");
    let issues = overloaded["coverage"]["issues"].as_array().unwrap();
    assert_eq!(issues.len(), 1, "{overloaded}");
    assert_eq!(issues[0]["kind"], "unresolved_comparison");
    assert_eq!(issues[0]["path"], "main.py");
    assert_eq!(issues[0]["line"], 2);
}

#[test]
fn indirect_sequence_ordering_can_dispatch_equality() {
    let project = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    fs::write(project.path().join(".skylos.toml"), "root = true\n").unwrap();
    fs::write(
        project.path().join("main.py"),
        concat!(
            "def hook_helper():\n    return False\n\n",
            "class Label:\n    def __eq__(self, other):\n        return hook_helper()\n\n",
            "def labels():\n    return [Label()]\n\n",
            "if labels() < labels():\n    print('before')\n",
        ),
    )
    .unwrap();
    let (report, _) = compare(project.path(), cache.path());
    assert_eq!(report["coverage"]["status"], "incomplete");
    assert_eq!(
        report["coverage"]["issues"][0]["kind"],
        "unresolved_comparison"
    );
    assert_eq!(report["unused_functions"], serde_json::json!([]));
}

#[test]
fn read_and_parse_proofs_follow_the_current_file_universe() {
    let project = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    fs::write(project.path().join(".skylos.toml"), "root = true\n").unwrap();
    fs::write(
        project.path().join("main.py"),
        "from helper import target\ntarget()\n",
    )
    .unwrap();
    let helper = project.path().join("helper.py");
    let moved = project.path().join("moved.py");
    let source = "def target():\n    pass\n";
    fs::write(&helper, source).unwrap();
    let (initial, _) = compare(project.path(), cache.path());
    assert_eq!(initial["coverage"]["status"], "complete");

    fs::rename(&helper, &moved).unwrap();
    let (renamed, warm) = compare(project.path(), cache.path());
    assert_eq!(warm["hits"], 2);
    assert_eq!(renamed["coverage"]["status"], "incomplete");

    fs::write(&helper, "def target(\n").unwrap();
    let (invalid, failed) = compare(project.path(), cache.path());
    assert_eq!(invalid["coverage"]["status"], "incomplete");
    assert_eq!(failed["misses"], 1);
    assert_eq!(failed["writes"], 0);

    fs::remove_file(moved).unwrap();
    fs::write(&helper, source).unwrap();
    let (restored, warm) = compare(project.path(), cache.path());
    assert_eq!(warm["hits"], 2);
    assert_eq!(restored, initial);
}

#[test]
fn grep_roots_and_literal_dispatch_follow_current_configuration_with_a_warm_cache() {
    let project = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    fs::write(project.path().join(".skylos.toml"), "root = true\n").unwrap();
    let workflow = project.path().join("workflow.py");
    let workflow_source = concat!(
        "import plugin as handler\n",
        "# getattr(handler, 'lookalike')()\n",
        "example = \"getattr(handler, 'lookalike')()\"\n",
        "def queue_task():\n",
        "    getattr(handler, 'dispatch')(handler.done)\n",
        "    return getattr(handler, 'process')()\n",
    );
    fs::write(&workflow, workflow_source).unwrap();
    fs::write(
        project.path().join("plugin.py"),
        concat!(
            "def process():\n    return helper(terminal)\n",
            "def helper(callback):\n    return callback()\n",
            "def terminal():\n    return 42\n",
            "def dispatch(callback):\n    return callback()\n",
            "def done():\n    return 1\n",
            "def lookalike():\n    return 0\n",
        ),
    )
    .unwrap();
    fs::write(
        project.path().join("other.py"),
        "def process():\n    pass\n",
    )
    .unwrap();
    let jobs = project.path().join("jobs.yaml");
    fs::write(
        &jobs,
        "# ignored: other:process\nhandler_module: workflow.py\n",
    )
    .unwrap();

    let options = Options {
        profile: skarmory_core::phase02::Profile::SkylosPhase03,
        ..Options::default()
    };
    let run = |options: &Options| -> Value {
        let json = scan(project.path(), options).unwrap().to_json().unwrap();
        serde_json::from_str(&json).unwrap()
    };
    let parallel = Options {
        cache_dir: Some(cache.path().to_owned()),
        workers: 4,
        ..options.clone()
    };
    let normalized = |report: &mut Value| report.as_object_mut().unwrap().remove("cache").unwrap();
    let initial = run(&options);
    for metric in ["misses", "hits"] {
        let mut cached = run(&parallel);
        assert_eq!(normalized(&mut cached)[metric], 3);
        assert_eq!(cached, initial);
    }
    assert_eq!(initial["coverage"]["status"], "complete");
    let unused = |report: &Value| {
        report["unused_functions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["full_name"].as_str().unwrap().to_owned())
            .collect::<std::collections::BTreeSet<_>>()
    };
    assert_eq!(
        unused(&initial),
        ["other.process".into(), "plugin.lookalike".into()].into()
    );
    let evidence = initial["grep_verification_evidence"].as_array().unwrap();
    assert_eq!(evidence.len(), 3);
    for (qualified, kind, path, line, column) in [
        ("plugin.dispatch", "dynamic_member", "workflow.py", 5, 22),
        ("plugin.process", "dynamic_member", "workflow.py", 6, 29),
        (
            "workflow.queue_task",
            "configuration_module",
            "jobs.yaml",
            2,
            17,
        ),
    ] {
        assert!(
            evidence.iter().any(|item| {
                item["qualified_name"] == qualified
                    && item["kind"] == kind
                    && item["path"] == path
                    && item["line"] == line
                    && item["column"] == column
                    && item["certainty"] == "possible"
            }),
            "{evidence:?}"
        );
    }
    let helper = initial["dead_code_evidence"]["symbols"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["qualified_name"] == "plugin.helper")
        .unwrap();
    assert!(
        helper["evidence"].as_array().unwrap().iter().any(|item| {
            item["kind"] == "reachable_from_root" && item["certainty"] == "possible"
        })
    );

    for use_of_dispatch in [
        "getattr(handler, 'dispatch')\n    getattr(handler, 'dispatch')(handler.done)",
        "getattr(handler, 'dispatch')(None)",
    ] {
        fs::write(
            &workflow,
            workflow_source.replace(
                "getattr(handler, 'dispatch')(handler.done)",
                use_of_dispatch,
            ),
        )
        .unwrap();
        let report = run(&options);
        assert_eq!(report["coverage"]["status"], "incomplete");
        assert!(
            report["coverage"]["issues"]
                .as_array()
                .unwrap()
                .iter()
                .any(|issue| {
                    issue["kind"] == "unresolved_call"
                        && issue["path"] == "plugin.py"
                        && issue["line"] == 8
                })
        );
    }
    fs::write(&workflow, workflow_source).unwrap();

    fs::remove_file(jobs).unwrap();
    let mut edited = run(&parallel);
    assert_eq!(normalized(&mut edited)["hits"], 3);
    assert_eq!(edited, run(&options));
    assert_eq!(edited["coverage"]["status"], "complete");
    assert_eq!(
        edited["analysis_summary"]["grep_verify"]["rescued_count"],
        0
    );
    assert!(edited.get("grep_verification_evidence").is_none());
    assert_eq!(unused(&edited).len(), 8);
}
