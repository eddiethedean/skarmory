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

#[test]
fn root_prefixed_include_restores_excluded_sources() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    let app = root.join("services/app");
    for directory in ["generated", "generated_extra"] {
        let directory = app.join(directory);
        fs::create_dir_all(&directory).unwrap();
        fs::write(directory.join("bad.py"), "def invalid(\n").unwrap();
    }
    fs::write(app.join("main.py"), "pass\n").unwrap();
    fs::write(root.join(".skylos.toml"), "root = true\n").unwrap();
    let options = Options {
        excludes: vec!["app/generated".into(), "app/generated_extra".into()],
        ..Options::default()
    };
    assert_eq!(selected_paths(&app, &options), json!(["main.py"]));

    let options = Options {
        includes: vec!["app/generated".into()],
        ..options
    };
    let report = scan(&app, &options).unwrap();
    assert!(!report.is_complete());
    let data: Value = serde_json::from_str(&report.to_json().unwrap()).unwrap();
    assert_eq!(
        data["selected_paths"],
        json!(["generated/bad.py", "main.py"])
    );
    assert_eq!(data["coverage"]["issues"][0]["kind"], "parse_error");
    assert_eq!(data["coverage"]["issues"][0]["path"], "generated/bad.py");
}

#[test]
fn configuration_discovery_uses_the_resolved_scan_directory() {
    let project = tempfile::tempdir().unwrap();
    let parent = project.path();
    let root = parent.join("root");
    let child = root.join("child");
    fs::create_dir_all(&child).unwrap();
    fs::write(parent.join(".skylos.toml"), "root = true\n").unwrap();
    fs::write(
        child.join(".skylos.toml"),
        "root = true\nexclude = [\"orphan.py\"]\n",
    )
    .unwrap();
    fs::write(root.join("orphan.py"), "def unused():\n    pass\n").unwrap();

    let scan_json = |path: &Path| {
        let report = scan(path, &Options::default()).unwrap();
        assert!(report.is_complete());
        serde_json::from_str::<Value>(&report.to_json().unwrap()).unwrap()
    };
    let direct = scan_json(&root);
    let aliased = scan_json(&child.join(".."));
    assert_eq!(direct["selected_paths"], json!(["orphan.py"]));
    assert_eq!(direct["unused_functions"][0]["name"], "unused");
    for key in ["selected_paths", "unused_functions", "config_sources"] {
        assert_eq!(direct[key], aliased[key], "different {key}");
    }
}

#[cfg(unix)]
#[test]
fn unsupported_symlink_target_keeps_the_requested_configuration_ancestry() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path().join("root");
    let external = project.path().join("external");
    let target = external.join("project");
    fs::create_dir(&root).unwrap();
    fs::create_dir_all(&target).unwrap();
    fs::write(root.join(".skylos.toml"), "root = true\n").unwrap();
    fs::write(external.join(".skylos.toml"), "malformed [\n").unwrap();
    let link = root.join("link");
    std::os::unix::fs::symlink(&target, &link).unwrap();

    let report = scan(&link, &Options::default()).unwrap();
    assert!(!report.is_complete());
    let data: Value = serde_json::from_str(&report.to_json().unwrap()).unwrap();
    assert_eq!(data["coverage"]["issues"][0]["kind"], "discovery_error");
    assert_eq!(
        data["coverage"]["issues"][0]["message"],
        "symlinked scan targets are unsupported"
    );
}
