use std::fs;
use std::path::Path;
use std::process::Command;

use serde_json::Value;

fn put(project: &Path, path: &str, source: &str) {
    let file = project.join(path);
    fs::create_dir_all(file.parent().expect("fixture path has parent")).unwrap();
    fs::write(file, source).unwrap();
}

fn scan(project: &Path, options: &[&str], exit: i32) -> Value {
    let output = Command::new(env!("CARGO_BIN_EXE_skarmory"))
        .current_dir(project)
        .env_remove("SKYLOS_CONFIG_FILE")
        .env_remove("DJANGO_SETTINGS_MODULE")
        .args([
            "--profile",
            "skylos-0.3",
            "--confidence",
            "0",
            "--no-grep-verify",
        ])
        .args(options)
        .arg(".")
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(exit),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn unused<'a>(report: &'a Value, category: &str) -> Vec<&'a str> {
    report[category]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["name"].as_str().unwrap())
        .collect()
}

#[test]
fn java_entrypoint_retains_typed_receiver_and_local_call_chain() {
    let project = tempfile::tempdir().unwrap();
    put(
        project.path(),
        "src/demo/Server.java",
        r#"
package demo;
public class Server {
    public static void main(String[] args) {
        Server server = new Server();
        server.process();
    }

    void process() { normalize(); }
    private void normalize() { System.out.println("live"); }
    public static void obsolete() {}
}
class UnusedMonitor {
    void probe() {}
}
"#,
    );
    let report = scan(project.path(), &[], 0);
    assert_eq!(report["analysis_summary"]["languages"]["Java"], 1);
    assert_eq!(report["coverage"]["status"], "complete");
    assert_eq!(unused(&report, "unused_classes"), ["UnusedMonitor"]);
    assert_eq!(
        unused(&report, "unused_functions"),
        ["Server.obsolete", "UnusedMonitor.probe"]
    );
}

#[test]
fn java_fxml_selection_rebuilds_graph_from_resource_edits_with_cached_facts() {
    let project = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    put(
        project.path(),
        "Controller.java",
        r#"
import javafx.fxml.FXML;
class Controller {
    @FXML private void launch() {}
    @FXML private void cancel() {}
    @FXML private void initialize() {}
    private void neverWired() {}
}
"#,
    );
    let fxml = |handler| {
        format!(
            r##"<Button xmlns:fx="http://javafx.com/fxml/1" fx:controller="Controller"
             onAction="#{handler}" id="#neverWired">
             <!-- <Button onAction="#neverWired" /> -->
           </Button>"##,
        )
    };
    put(project.path(), "view.fxml", &fxml("launch"));
    let params = [
        "--cache-dir",
        cache.path().to_str().unwrap(),
        "--workers",
        "4",
    ];
    let report = scan(project.path(), &params, 0);
    assert!(unused(&report, "unused_classes").is_empty());
    assert_eq!(
        unused(&report, "unused_functions"),
        ["Controller.cancel", "Controller.neverWired"]
    );

    put(project.path(), "view.fxml", &fxml("cancel"));
    let cold = scan(project.path(), &[], 0);
    let warm = scan(project.path(), &params, 0);
    assert_eq!(
        unused(&warm, "unused_functions"),
        ["Controller.launch", "Controller.neverWired"]
    );
    assert_eq!(warm["coverage"], cold["coverage"]);
    assert_eq!(warm["dead_code_evidence"], cold["dead_code_evidence"]);

    put(project.path(), "view.fxml", &fxml("missing"));
    let incomplete = scan(project.path(), &params, 2);
    assert_eq!(incomplete["coverage"]["status"], "incomplete");
    assert!(unused(&incomplete, "unused_functions").is_empty());
    assert!(
        incomplete["coverage"]["issues"]
            .as_array()
            .unwrap()
            .iter()
            .any(|issue| issue["kind"] == "unresolved_java_fxml_handler"
                && issue["path"] == "view.fxml")
    );
}

#[test]
fn java_service_descriptor_retains_provider_and_identifies_unavailable_provider() {
    let project = tempfile::tempdir().unwrap();
    put(
        project.path(),
        "src/demo/Lookup.java",
        "package demo; public interface Lookup { void read(); }",
    );
    put(
        project.path(),
        "src/demo/Indexer.java",
        r#"
package demo;
public class Indexer implements Lookup {
    public Indexer() { initialize(); }
    private void initialize() {}
    public void read() {}
    private void stale() {}
}
"#,
    );
    put(
        project.path(),
        "resources/META-INF/services/demo.Lookup",
        "demo.Indexer # provider\n",
    );
    let report = scan(project.path(), &[], 0);
    assert_eq!(unused(&report, "unused_functions"), ["Indexer.stale"]);
    let evidence = report["dead_code_evidence"].to_string();
    assert!(evidence.contains("java_service_provider"), "{evidence}");

    put(
        project.path(),
        "resources/META-INF/services/demo.Lookup",
        "demo.NotSelected\n",
    );
    let incomplete = scan(project.path(), &[], 2);
    assert!(unused(&incomplete, "unused_functions").is_empty());
    assert!(
        incomplete["coverage"]["issues"]
            .as_array()
            .unwrap()
            .iter()
            .any(|issue| issue["kind"] == "unresolved_java_resource_type")
    );
}

#[test]
fn malformed_java_is_incomplete_and_retains_completed_python_findings() {
    let project = tempfile::tempdir().unwrap();
    put(project.path(), "Broken.java", "class Broken { void open( }");
    put(
        project.path(),
        "main.py",
        "def unrelated():\n    return 0\n",
    );
    let report = scan(project.path(), &[], 2);
    assert_eq!(report["analysis_summary"]["languages"]["Java"], 1);
    assert_eq!(unused(&report, "unused_functions"), ["unrelated"]);
    assert!(
        report["coverage"]["issues"]
            .as_array()
            .unwrap()
            .iter()
            .any(|issue| issue["kind"] == "parse_error" && issue["path"] == "Broken.java")
    );
}

#[test]
fn java_dynamic_fxml_binding_reports_incomplete_coverage() {
    let project = tempfile::tempdir().unwrap();
    put(
        project.path(),
        "Controller.java",
        r#"import javafx.fxml.FXMLLoader;
class Controller {
    Controller(String path) {
        FXMLLoader loader = new FXMLLoader(getClass().getResource(path));
        loader.setController(this);
    }
    private void action() {}
}"#,
    );
    put(
        project.path(),
        "view.fxml",
        r##"<Button xmlns:fx="http://javafx.com/fxml/1" onAction="#action" />"##,
    );
    let report = scan(project.path(), &[], 2);
    assert!(unused(&report, "unused_functions").is_empty());
    assert!(
        report["coverage"]["issues"]
            .as_array()
            .unwrap()
            .iter()
            .any(|issue| issue["kind"] == "unsupported_java_fxml"
                && issue["path"] == "Controller.java")
    );
}

#[test]
fn local_class_for_name_is_a_static_method_call() {
    let project = tempfile::tempdir().unwrap();
    put(
        project.path(),
        "App.java",
        r#"public class App {
    public static void main(String[] args) {
        Class.forName("not.a.literal.reflection");
    }
}
class Class {
    static void forName(String value) {}
    static void stale() {}
}
"#,
    );
    let report = scan(project.path(), &[], 0);
    assert!(unused(&report, "unused_classes").is_empty());
    assert_eq!(unused(&report, "unused_functions"), ["Class.stale"]);
}
