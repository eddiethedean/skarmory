use std::{fs, path::Path};

use serde_json::Value;
use skarmory_core::{CacheStats, ScanOptions, scan, scan_cached};

fn compare(project: &Path, cache: &Path, options: &ScanOptions) -> (Value, CacheStats) {
    let mut parallel = options.clone();
    parallel.workers = 4;
    let (cached, _, stats) = scan_cached(project, &parallel, cache);
    let uncached = scan(project, options).0;
    let json = cached.to_json().unwrap();
    assert_eq!(json, uncached.to_json().unwrap());
    (serde_json::from_str(&json).unwrap(), stats)
}

fn finds(report: &Value, name: &str) -> bool {
    report["findings"]
        .as_array()
        .unwrap()
        .iter()
        .any(|entry| entry["symbol"] == name)
}

#[test]
fn reuses_edited_project_facts_without_reusing_reachability() {
    let project = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let main = project.path().join("main.py");
    fs::write(&main, "from helper import first\nfirst()\n").unwrap();
    fs::write(
        project.path().join("helper.py"),
        "def first():\n    second()\ndef second():\n    first()\n",
    )
    .unwrap();
    let options = ScanOptions::default();
    let (initial, cold) = compare(project.path(), cache.path(), &options);
    assert_eq!(cold.misses, 2);
    assert_eq!(cold.writes, 2);
    assert_eq!(initial["coverage"]["status"], "complete");
    assert!(!finds(&initial, "first"));
    assert!(!finds(&initial, "second"));
    let (_, warm) = compare(project.path(), cache.path(), &options);
    assert_eq!(warm.hits, 2);

    fs::write(&main, "from helper import first\n").unwrap();
    let (edited, cold_edit) = compare(project.path(), cache.path(), &options);
    assert_eq!(cold_edit.hits, 1);
    assert_eq!(cold_edit.misses, 1);
    assert_eq!(edited["coverage"]["status"], "complete");
    assert!(finds(&edited, "first"));
    assert!(finds(&edited, "second"));
}

#[test]
fn refreshes_imports_and_parse_failures_on_every_scan() {
    let project = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let main = project.path().join("main.py");
    let helper = project.path().join("helper.py");
    let moved = project.path().join("moved.py");
    let module_text = "def target():\n    pass\n";
    fs::write(&main, "from helper import target\ntarget()\n").unwrap();
    let options = ScanOptions::default();
    let (missing, _) = compare(project.path(), cache.path(), &options);
    assert_eq!(missing["coverage"]["status"], "incomplete");

    fs::write(&helper, module_text).unwrap();
    let (complete, _) = compare(project.path(), cache.path(), &options);
    assert_eq!(complete["coverage"]["status"], "complete");
    assert!(!finds(&complete, "target"));

    fs::rename(&helper, &moved).unwrap();
    let (renamed, warm) = compare(project.path(), cache.path(), &options);
    assert_eq!(renamed["coverage"]["status"], "incomplete");
    assert_eq!(warm.hits, 2);

    fs::write(&helper, "def target(\n").unwrap();
    let (invalid, changed) = compare(project.path(), cache.path(), &options);
    assert_eq!(invalid["coverage"]["status"], "incomplete");
    assert_eq!(changed.misses, 1);
    assert_eq!(changed.writes, 0);
    assert_eq!(invalid["findings"], serde_json::json!([]));

    fs::remove_file(moved).unwrap();
    fs::write(&helper, module_text).unwrap();
    let (recovered, warm) = compare(project.path(), cache.path(), &options);
    assert_eq!(recovered, complete);
    assert_eq!(warm.hits, 2);
}
