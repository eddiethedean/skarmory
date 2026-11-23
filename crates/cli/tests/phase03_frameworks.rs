use std::fs;
use std::path::Path;
use std::process::Command;

use serde_json::Value;

fn scan(project: &Path) -> Value {
    fs::write(project.join(".skylos.toml"), "root = true\n").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_skarmory"))
        .current_dir(project)
        .env_remove("SKYLOS_CONFIG_FILE")
        .args([
            "--profile",
            "skylos-0.3",
            ".",
            "--json",
            "--no-grep-verify",
            "--confidence",
            "0",
        ])
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["coverage"]["status"], "complete", "{report}");
    report
}

fn findings<'a>(report: &'a Value, category: &str) -> Vec<&'a str> {
    let mut names: Vec<_> = report[category]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["full_name"].as_str().unwrap())
        .collect();
    names.sort_unstable();
    names
}

#[test]
fn phase03_framework_registrations_preserve_positive_and_negative_controls() {
    let project = tempfile::tempdir().unwrap();
    fs::write(
        project.path().join("web.py"),
        r#"from flask import Blueprint, Flask

active = Blueprint("active", __name__)
dormant = Blueprint("dormant", __name__)

@active.route("/")
def served():
    return helper()

@dormant.route("/")
def parked():
    return 0

def helper():
    return 1

app = Flask(__name__)
app.register_blueprint(active)
"#,
    )
    .unwrap();
    fs::write(
        project.path().join("callbacks.py"),
        r#"from celery import Celery
from sqlalchemy import event

worker = Celery("jobs")

@worker.task
def queued():
    return 1

def abandoned_task():
    return 0

class Engine:
    pass

@event.listens_for(Engine, "connect")
def connected(dbapi, record):
    return None

def disconnected(dbapi, record):
    return None
"#,
    )
    .unwrap();
    fs::write(
        project.path().join("models.py"),
        r#"from pydantic import BaseModel

class User(BaseModel):
    name: str
    def model_post_init(self, context):
        return None

class Draft(BaseModel):
    name: str

User(name="Ada")
"#,
    )
    .unwrap();

    let report = scan(project.path());
    assert_eq!(
        findings(&report, "unused_functions"),
        [
            "callbacks.abandoned_task",
            "callbacks.disconnected",
            "web.parked",
        ],
        "{report}"
    );
    assert_eq!(
        findings(&report, "unused_classes"),
        ["models.Draft"],
        "{report}"
    );
    let evidence = report["dead_code_evidence"]["symbols"].as_array().unwrap();
    for (name, kind) in [
        ("web.served", "flask_registration"),
        ("callbacks.queued", "celery_task"),
        ("callbacks.connected", "sqlalchemy_listener"),
        ("models.User.model_post_init", "framework_lifecycle"),
    ] {
        assert!(
            evidence.iter().any(|row| {
                row["qualified_name"] == name
                    && row["evidence"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|entry| entry["kind"] == kind)
            }),
            "missing {kind} evidence for {name}: {report}"
        );
    }
}
