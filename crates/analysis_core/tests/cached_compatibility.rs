use std::{fs, path::Path};

use serde_json::Value;
use skarmory_core::phase02::{Options, scan};

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
fn custom_length_calls_do_not_make_an_ordering_guard_complete() {
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
    assert_eq!(custom["coverage"]["status"], "incomplete", "{custom}");
    let issues = custom["coverage"]["issues"].as_array().unwrap();
    assert_eq!(issues.len(), 1, "{custom}");
    assert_eq!(issues[0]["kind"], "unresolved_comparison");
    assert_eq!(custom["unused_functions"], serde_json::json!([]));

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
