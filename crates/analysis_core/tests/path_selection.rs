use std::fs;
use std::path::Path;

use serde_json::{Value, json};
use skarmory_core::phase02::{Options, scan};

fn selected_paths(path: &Path, options: &Options) -> Value {
    let report = scan(path, options).unwrap();
    assert!(report.is_complete());
    serde_json::from_str::<Value>(&report.to_json().unwrap()).unwrap()["selected_paths"].clone()
}

#[test]
fn literal_exclusion_matches_whole_path_components() {
    let project = tempfile::tempdir().unwrap();
    for path in ["ignored.py", "not_ignored.py"] {
        fs::write(project.path().join(path), "pass\n").unwrap();
    }
    let options = Options {
        excludes: vec!["ignored.py".into()],
        ..Options::default()
    };
    assert_eq!(
        selected_paths(project.path(), &options),
        json!(["not_ignored.py"])
    );
}

#[test]
fn including_an_excluded_folder_restores_its_descendants() {
    let project = tempfile::tempdir().unwrap();
    for dir in [
        "generated/sub",
        "generated/sub_backup",
        "hidden/prefix/generated/sub",
        "hidden/prefix/not_generated/sub",
    ] {
        let dir = project.path().join(dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("code.py"), "pass\n").unwrap();
    }
    let options = Options {
        excludes: vec![
            "generated/sub".into(),
            "generated/sub_backup".into(),
            "hidden".into(),
        ],
        includes: vec!["generated/sub".into()],
        ..Options::default()
    };
    assert_eq!(
        selected_paths(project.path(), &options),
        json!([
            "generated/sub/code.py",
            "hidden/prefix/generated/sub/code.py"
        ])
    );
}

#[test]
fn absolute_exclusion_is_anchored_to_the_target_path() {
    let project = tempfile::tempdir().unwrap();
    fs::create_dir(project.path().join("nested")).unwrap();
    for path in ["ignored.py", "not_ignored.py", "nested/ignored.py"] {
        fs::write(project.path().join(path), "pass\n").unwrap();
    }
    let excluded = project.path().join("ignored.py");
    let options = Options {
        excludes: vec![excluded.to_string_lossy().into_owned()],
        ..Options::default()
    };
    assert_eq!(
        selected_paths(project.path(), &options),
        json!(["nested/ignored.py", "not_ignored.py"])
    );
    assert_eq!(selected_paths(&excluded, &options), json!([]));
}
