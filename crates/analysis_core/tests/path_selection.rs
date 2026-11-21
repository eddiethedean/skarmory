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
fn recursive_exclusion_matches_files_at_the_root_and_below_it() {
    let project = tempfile::tempdir().unwrap();
    fs::create_dir_all(project.path().join("nested/deeper")).unwrap();
    for path in ["ignored.py", "nested/deeper/ignored.py"] {
        fs::write(project.path().join(path), "def invalid(\n").unwrap();
    }
    for path in ["main.py", "nested/not_ignored.py"] {
        fs::write(project.path().join(path), "pass\n").unwrap();
    }
    let options = Options {
        excludes: vec!["**/ignored.py".into()],
        ..Options::default()
    };
    assert_eq!(
        selected_paths(project.path(), &options),
        json!(["main.py", "nested/not_ignored.py"])
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

#[test]
fn inherited_exclusion_uses_the_scan_subdirectory_as_an_alternate_base() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    for directory in ["app/nested", "app_extra", "services/app"] {
        fs::create_dir_all(root.join(directory)).unwrap();
    }
    for path in [
        "app/ignored.py",
        "app/nested/ignored.py",
        "app/not_ignored.py",
        "app_extra/ignored.py",
    ] {
        fs::write(root.join(path), "pass\n").unwrap();
    }
    let config = root.join(".skylos.toml");
    fs::write(&config, "root = true\nexclude = [\"app/ignored.py\"]\n").unwrap();
    let options = Options::default();
    assert_eq!(
        selected_paths(root, &options),
        json!([
            "app/nested/ignored.py",
            "app/not_ignored.py",
            "app_extra/ignored.py"
        ])
    );
    assert_eq!(
        selected_paths(&root.join("app"), &options),
        json!(["not_ignored.py"])
    );
    for path in ["app/nested", "app_extra", "app/ignored.py"] {
        assert_eq!(
            selected_paths(&root.join(path), &options),
            json!(["ignored.py"])
        );
    }

    let nested = root.join("services/app");
    fs::write(nested.join("ignored.py"), "def invalid(\n").unwrap();
    fs::write(nested.join("main.py"), "pass\n").unwrap();
    assert_eq!(selected_paths(&nested, &options), json!(["main.py"]));

    fs::write(config, "root = true\n").unwrap();
    let flags = Options {
        excludes: vec!["app/ignored.py".into()],
        ..options
    };
    assert_eq!(
        selected_paths(&root.join("app"), &flags),
        json!(["not_ignored.py"])
    );
    assert_eq!(selected_paths(&nested, &flags), json!(["main.py"]));
}
