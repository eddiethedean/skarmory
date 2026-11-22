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
        "def first():\n    second()\n\ndef second():\n    first()\n",
    )
    .unwrap();
    fs::write(&main, "from helpers import first\nfirst()\n").unwrap();
    let (initial, cold) = compare(project.path(), cache.path());
    assert_eq!(initial["coverage"]["status"], "complete");
    assert_eq!(cold["misses"], 2);
    assert!(!finds(&initial, "first"));
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
    assert_eq!(reconfigured["hits"], 2);
    assert!(!finds(&rooted, "first"));
    assert!(!finds(&rooted, "second"));
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
