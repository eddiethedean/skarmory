use std::fs;
use std::path::Path;
use std::process::Command;

use serde_json::{Value, json};

fn put(root: &Path, path: &str, text: &str) {
    let path = root.join(path);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

fn scan(root: &Path, args: &[&str], code: i32) -> Value {
    let output = Command::new(env!("CARGO_BIN_EXE_skarmory"))
        .current_dir(root)
        .env_remove("SKYLOS_CONFIG_FILE")
        .env_remove("DJANGO_SETTINGS_MODULE")
        .args(["--profile", "skylos-0.3"])
        .args(args)
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(code),
        "{}\n{}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn findings(report: &Value) -> Vec<(&str, u64)> {
    report["unused_files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| {
            assert_eq!(row["rule_id"], "SKY-E003");
            assert_eq!(row["line"], 1);
            (
                row["file"].as_str().unwrap(),
                row["confidence"].as_u64().unwrap(),
            )
        })
        .collect()
}

#[test]
fn reaches_reexport_and_type_providers_but_reports_a_disconnected_cycle() {
    let tree = tempfile::tempdir().unwrap();
    let root = tree.path();
    put(
        root,
        "package.json",
        r#"{"main":"src/main.ts","type":"module"}"#,
    );
    put(
        root,
        "src/main.ts",
        "import { run } from './barrel';\nimport type { Shape } from './types.ts';\nrun({size: 1} satisfies Shape);\n",
    );
    put(
        root,
        "src/barrel/index.ts",
        "export { run } from '../impl';\n",
    );
    put(
        root,
        "src/impl.jsx",
        "export function run(x) { console.log(x); }\n",
    );
    put(
        root,
        "src/types.ts",
        "export interface Shape { size: number }\n",
    );
    put(root, "src/dead-a.ts", "export * from './dead-b';\n");
    put(root, "src/dead-b.ts", "export * from './dead-a';\n");
    put(root, "src/lonely.js", "console.log('unused');\n");
    put(root, "helper.py", "print('mixed project')\n");

    let report = scan(root, &[".", "--strict", "--sarif", "report.sarif"], 1);
    assert_eq!(report["coverage"]["status"], "complete");
    assert_eq!(report["coverage"]["selected_files"], 8);
    assert_eq!(report["coverage"]["extracted_files"], 8);
    assert_eq!(
        report["analysis_summary"]["languages"],
        json!({"Python": 1, "TypeScript": 5, "JavaScript": 2})
    );
    assert_eq!(
        report["analysis_summary"]["javascript_file_analysis"]["rule_id"],
        "SKY-E003"
    );
    assert_eq!(
        findings(&report),
        [
            ("src/dead-a.ts", 65),
            ("src/dead-b.ts", 65),
            ("src/lonely.js", 80)
        ]
    );
    let evidence = report["dead_code_evidence"]["symbols"].as_array().unwrap();
    for (file, kind) in [
        ("src/main.ts", "js_package_entrypoint"),
        ("src/barrel/index.ts", "js_reachable_module"),
        ("src/impl.jsx", "js_reachable_module"),
        ("src/types.ts", "js_reachable_module"),
    ] {
        assert!(evidence.iter().any(|event| {
            event["file"] == file
                && event["evidence"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|fact| fact["kind"] == kind)
        }));
    }
    let sarif: Value =
        serde_json::from_slice(&fs::read(root.join("report.sarif")).unwrap()).unwrap();
    assert_eq!(sarif["runs"][0]["results"][0]["ruleId"], "SKY-E003");
}

#[test]
fn suppresses_file_findings_with_real_comments_and_project_rules() {
    let tree = tempfile::tempdir().unwrap();
    let root = tree.path();
    put(root, "package.json", r#"{"main":"main.js"}"#);
    put(root, "main.js", "console.log('main');\n");
    put(
        root,
        "ignored.js",
        "// skylos: ignore[SKY-E003] retained script\nconsole.log('ignored');\n",
    );
    put(
        root,
        "dead.js",
        "console.log('// skylos: ignore[SKY-E003]');\n",
    );

    let report = scan(root, &["."], 0);
    assert_eq!(findings(&report), [("dead.js", 80)]);
    assert_eq!(report["suppressed"][0]["file"], "ignored.js");
    assert_eq!(report["suppressed"][0]["reason"], "inline ignore comment");

    put(
        root,
        "pyproject.toml",
        "[tool.skylos]\nignore = ['SKY-E003']\n",
    );
    let report = scan(root, &["."], 0);
    assert_eq!(report["coverage"]["status"], "complete");
    assert!(findings(&report).is_empty());
    assert_eq!(report["suppressed"][0]["file"], "dead.js");
    assert_eq!(report["suppressed"][0]["reason"], "rule ignored (SKY-E003)");
}

#[test]
fn marks_missing_ambiguous_or_unmodeled_inputs_incomplete() {
    let tree = tempfile::tempdir().unwrap();
    let root = tree.path();
    put(root, "unused.ts", "export const unused = 1;\n");
    put(root, "main.ts", "import './resolved';\n");
    put(root, "resolved.ts", "export const reached = 1;\n");
    let report = scan(root, &["."], 2);
    assert_eq!(
        report["coverage"]["issues"][0]["kind"],
        "missing_js_entrypoint"
    );

    put(root, "package.json", r#"{"main":"main.ts"}"#);
    assert_eq!(
        scan(root, &["main.ts"], 2)["coverage"]["issues"][0]["kind"],
        "unsupported_js_input_scope"
    );
    assert_eq!(
        scan(root, &[".", "--exclude", "resolved.ts"], 2)["coverage"]["issues"][0]["kind"],
        "unresolved_js_import"
    );
    put(root, "resolved.js", "export const reached = 1;\n");
    assert_eq!(
        scan(root, &[".", "--exclude", "resolved.ts"], 2)["coverage"]["issues"][0]["kind"],
        "unresolved_js_import"
    );
    let report = scan(root, &["."], 2);
    assert_eq!(report["coverage"]["status"], "incomplete");
    assert_eq!(
        report["coverage"]["issues"][0]["kind"],
        "unresolved_js_import"
    );
    assert_eq!(report["coverage"]["issues"][0]["path"], "main.ts");
    assert_eq!(report["coverage"]["issues"][0]["line"], 1);
    assert!(findings(&report).is_empty());

    put(root, "main.ts", "import('./resolved.ts');\n");
    assert_eq!(
        scan(root, &["."], 2)["coverage"]["issues"][0]["kind"],
        "unsupported_js_module_loading"
    );
    put(root, "main.ts", "export const broken = ;\n");
    assert_eq!(
        scan(root, &["."], 2)["coverage"]["issues"][0]["kind"],
        "parse_error"
    );
    put(root, "main.ts", "import './resolved.ts';\n");
    put(root, "tsconfig.json", "{}");
    assert_eq!(
        scan(root, &["."], 2)["coverage"]["issues"][0]["kind"],
        "unsupported_js_project_context"
    );
    fs::remove_file(root.join("tsconfig.json")).unwrap();
    let report = scan(root, &[".", "--select", "SKY-U005"], 2);
    assert_eq!(
        report["coverage"]["issues"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|issue| issue["kind"] == "unsupported_npm_dependency_analysis")
            .count(),
        1
    );
    assert_eq!(
        report["analysis_summary"]["javascript_file_analysis"]["status"],
        "not_run"
    );
}

#[test]
fn does_not_report_modules_reached_through_unsupported_loaders_as_unused() {
    let tree = tempfile::tempdir().unwrap();
    let root = tree.path();
    put(root, "package.json", r#"{"main":"main.js"}"#);
    put(root, "live.js", "console.log('live');\n");

    for source in [
        "module.require('./live.js');\n",
        "self.importScripts('./live.js');\n",
        "new globalThis['Worker']('./live.js');\n",
    ] {
        put(root, "main.js", source);
        let report = scan(root, &["."], 2);
        assert_eq!(
            report["coverage"]["issues"][0]["kind"],
            "unsupported_js_module_loading"
        );
        assert!(findings(&report).is_empty());
    }

    put(
        root,
        "main.js",
        "const local = { importScripts() {} }; local.importScripts();\n",
    );
    let report = scan(root, &["."], 0);
    assert_eq!(findings(&report), [("live.js", 80)]);
}

#[test]
fn parser_bounds_large_expressions_with_multiple_workers() {
    let tree = tempfile::tempdir().unwrap();
    let root = tree.path();
    put(root, "package.json", r#"{"main":"main.js"}"#);
    put(root, "spare.js", "export const spare = 1;\n");

    let nested = format!(
        "export const value = {}1{};",
        "(".repeat(2_000),
        ")".repeat(2_000)
    );
    put(root, "main.js", &nested);
    let report = scan(root, &[".", "--workers", "4"], 0);
    assert_eq!(findings(&report), [("spare.js", 80)]);

    let oversized = format!("export const value = {}1;", "1+".repeat(20_000));
    put(root, "main.js", &oversized);
    let report = scan(root, &[".", "--workers", "4"], 2);
    assert_eq!(report["coverage"]["issues"][0]["kind"], "analysis_limit");
    assert_eq!(report["coverage"]["issues"][0]["path"], "main.js");
    assert!(findings(&report).is_empty());
}

#[test]
fn cache_reuses_file_facts_and_refreshes_graph_after_source_and_manifest_edits() {
    let tree = tempfile::tempdir().unwrap();
    let root = tree.path();
    let cache = tempfile::tempdir().unwrap();
    let cache = cache.path().to_str().unwrap();
    put(root, "package.json", r#"{"main":"main.ts"}"#);
    put(root, "main.ts", "import './live.ts';\n");
    put(root, "live.ts", "export const value = 1;\n");
    put(root, "dead.js", "export const value = 1;\n");
    let compare = |expected: &[(&str, u64)], hits| {
        let fresh = scan(root, &["."], 0);
        let mut parallel = scan(root, &[".", "--workers", "4", "--cache-dir", cache], 0);
        assert_eq!(
            parallel.as_object_mut().unwrap().remove("cache").unwrap()["hits"],
            hits
        );
        assert_eq!(parallel, fresh);
        assert_eq!(findings(&fresh), expected);
    };

    compare(&[("dead.js", 80)], 0);
    compare(&[("dead.js", 80)], 3);
    put(root, "main.ts", "import './dead.js';\n");
    compare(&[("live.ts", 80)], 2);
    put(root, "package.json", r#"{"main":"live.ts"}"#);
    compare(&[("dead.js", 65), ("main.ts", 80)], 3);
}

#[test]
fn incomplete_javascript_keeps_completed_python_findings() {
    let tree = tempfile::tempdir().unwrap();
    let root = tree.path();
    put(root, "app.py", "def definitely_unused():\n    return 42\n");

    for (source, kind) in [
        ("console.log('example');\n", "missing_js_entrypoint"),
        ("export const broken = ;\n", "parse_error"),
    ] {
        put(root, "notes/demo.js", source);
        let report = scan(root, &[".", "--confidence", "0", "--strict"], 2);
        assert_eq!(report["coverage"]["status"], "incomplete");
        assert_eq!(report["coverage"]["issues"][0]["kind"], kind);
        assert_eq!(
            report["analysis_summary"]["javascript_file_analysis"]["status"],
            "incomplete"
        );
        assert!(findings(&report).is_empty());
        let python = report["unused_functions"].as_array().unwrap();
        assert_eq!(python.len(), 1);
        assert_eq!(python[0]["name"], "definitely_unused");
    }
}
