use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::Command;

use serde_json::Value;

fn scan(project: &Path) -> Value {
    scan_with_settings(project, None, None)
}

fn scan_with_settings(project: &Path, settings: Option<&str>, cache: Option<&Path>) -> Value {
    let report = scan_with_exit(project, settings, cache, 0);
    assert_eq!(report["coverage"]["status"], "complete", "{report}");
    report
}

fn scan_with_exit(
    project: &Path,
    settings: Option<&str>,
    cache: Option<&Path>,
    code: i32,
) -> Value {
    fs::write(project.join(".skylos.toml"), "root = true\n").unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_skarmory"));
    command.env_remove("DJANGO_SETTINGS_MODULE");
    if let Some(settings) = settings {
        command.env("DJANGO_SETTINGS_MODULE", settings);
    }
    if let Some(cache) = cache {
        command.args(["--workers", "4", "--cache-dir"]).arg(cache);
    }
    let output = command
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
        Some(code),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
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
fn fastapi_annotation_and_imperative_route_dependencies_are_retained() {
    let project = tempfile::tempdir().unwrap();
    fs::write(
        project.path().join("web.py"),
        r#"from fastapi import APIRouter, Depends, FastAPI
from typing import Annotated as Metadata

app = FastAPI()
dormant = APIRouter()

def db_session():
    return make_session()

def make_session():
    return 1

def access_check():
    return 2

def inactive_dependency():
    return 3

def return_metadata():
    return 0

@app.get("/")
def index(db: Metadata[int, Depends(db_session)]):
    return db

@app.get("/response")
def response() -> Metadata[int, Depends(return_metadata)]:
    return 1

def other():
    return 2

app.add_api_route("/other", other, dependencies=[Depends(access_check)])
dormant.add_api_route("/other", other, dependencies=[Depends(inactive_dependency)])
"#,
    )
    .unwrap();
    let report = scan(project.path());
    assert_eq!(
        findings(&report, "unused_functions"),
        ["web.inactive_dependency", "web.return_metadata"],
        "{report}"
    );
}

#[test]
fn fastapi_empty_registration_options_preserve_findings() {
    let project = tempfile::tempdir().unwrap();
    fs::write(
        project.path().join("web.py"),
        r#"from fastapi import FastAPI
app = FastAPI(dependencies=[], on_startup=[], exception_handlers={})

@app.get("/", dependencies=[])
def index():
    return 1

def unused():
    return 2
"#,
    )
    .unwrap();
    let report = scan(project.path());
    assert_eq!(findings(&report, "unused_functions"), ["web.unused"]);
}

#[test]
fn fastapi_mount_uses_the_registered_router_snapshot() {
    let project = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let root = project.path();
    fs::write(
        root.join("routes.py"),
        r#"from fastapi import APIRouter, Depends

def router_guard():
    return 1

def dormant_guard():
    return 2

router = APIRouter(dependencies=[Depends(router_guard)])
dormant = APIRouter(dependencies=[Depends(dormant_guard)])

@router.get("/early")
def early():
    return 1

@dormant.get("/")
def parked():
    return 2
"#,
    )
    .unwrap();
    fs::write(
        root.join("web.py"),
        r#"from fastapi import APIRouter, Depends, FastAPI
from routes import router

def nested_guard():
    return 1

def mount_guard():
    return 1

def empty_guard():
    return 1

app = FastAPI()
parent = APIRouter()
parent.include_router(router, dependencies=[Depends(nested_guard)])
app.include_router(parent, dependencies=[Depends(mount_guard)])
empty = APIRouter()
app.include_router(empty, dependencies=[Depends(empty_guard)])
"#,
    )
    .unwrap();
    let report = scan(root);
    assert_eq!(
        findings(&report, "unused_functions"),
        ["routes.dormant_guard", "routes.parked", "web.empty_guard",],
        "{report}"
    );
    for hits in [0, 2] {
        let mut cached = scan_with_settings(root, None, Some(cache.path()));
        let stats = cached.as_object_mut().unwrap().remove("cache").unwrap();
        assert_eq!(stats["hits"], hits);
        assert_eq!(cached, report);
    }

    let mut web = fs::OpenOptions::new()
        .append(true)
        .open(root.join("web.py"))
        .unwrap();
    web.write_all(
        br#"
def late_dependency():
    return 1

def late():
    return 2

router.add_api_route("/late", late, dependencies=[Depends(late_dependency)])
"#,
    )
    .unwrap();
    let report = scan_with_exit(root, None, None, 2);
    assert_eq!(report["coverage"]["status"], "incomplete", "{report}");
    assert!(findings(&report, "unused_functions").is_empty(), "{report}");
    assert!(
        report["coverage"]["issues"]
            .as_array()
            .unwrap()
            .iter()
            .any(|issue| {
                issue["kind"] == "unresolved_registration"
                    && issue["message"]
                        .as_str()
                        .unwrap()
                        .contains("framework versions")
            }),
        "{report}"
    );
    for hits in [1, 2] {
        let mut cached = scan_with_exit(root, None, Some(cache.path()), 2);
        let stats = cached.as_object_mut().unwrap().remove("cache").unwrap();
        assert_eq!(stats["hits"], hits);
        assert_eq!(cached, report);
    }
}

#[test]
fn fastapi_factory_can_mount_an_imported_router() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    fs::write(
        root.join("routes.py"),
        r#"from fastapi import APIRouter
router = APIRouter()

@router.get("/")
def endpoint():
    return 1

def abandoned():
    return 2
"#,
    )
    .unwrap();
    for (global, local) in [
        ("from routes import router\n", ""),
        ("", "    from routes import router\n"),
    ] {
        fs::write(root.join("web.py"), format!(
            "from fastapi import FastAPI\n{global}\ndef create():\n{local}    app = FastAPI()\n    app.include_router(router)\n    return app\n\napplication = create()\n",
        )).unwrap();
        let report = scan(root);
        assert_eq!(
            findings(&report, "unused_functions"),
            ["routes.abandoned"],
            "{report}"
        );
    }
}

#[test]
fn fastapi_uses_registration_time_for_postponed_but_not_eager_annotations() {
    for (postponed, nested, app_default) in [
        (false, false, false),
        (true, false, false),
        (true, true, false),
        (true, false, true),
    ] {
        let project = tempfile::tempdir().unwrap();
        let future = if postponed {
            "from __future__ import annotations\n"
        } else {
            ""
        };
        let wrapper = if app_default {
            "def wrapper():\n    return 0\n"
        } else if nested {
            "def wrapper(value=Depends(endpoint)):\n    return value\n"
        } else {
            "wrapper = endpoint\n"
        };
        let settings = if app_default {
            "dependencies=[Depends(endpoint)]"
        } else {
            ""
        };
        fs::write(
            project.path().join("web.py"),
            format!(
                r#"{future}from fastapi import Depends, FastAPI
from typing import Annotated

def earlier():
    return 1

dependency = earlier

def endpoint(value: Annotated[int, Depends(dependency)]):
    return value

def configured():
    return 2

app = FastAPI({settings})
dependency = configured
{wrapper}app.add_api_route("/", wrapper)
"#
            ),
        )
        .unwrap();
        let report = scan(project.path());
        let unused = if postponed {
            "web.earlier"
        } else {
            "web.configured"
        };
        assert_eq!(findings(&report, "unused_functions"), [unused], "{report}");
    }
}

#[test]
fn fastapi_runs_constructor_of_the_selected_dependency() {
    let project = tempfile::tempdir().unwrap();
    fs::write(
        project.path().join("web.py"),
        r#"from fastapi import APIRouter, Depends, FastAPI
from typing import Annotated

def prepare():
    return 1

def unused_prepare():
    return 2

class Service:
    def __init__(self):
        prepare()

class Draft:
    def __init__(self):
        unused_prepare()

app = FastAPI()
dormant = APIRouter()

@app.get("/")
def endpoint(service: Annotated[Service, Depends(Service)]):
    return 1

@dormant.get("/")
def parked(service: Annotated[Draft, Depends(Draft)]):
    return 2
"#,
    )
    .unwrap();
    let report = scan(project.path());
    assert_eq!(
        findings(&report, "unused_functions"),
        ["web.parked", "web.unused_prepare"],
        "{report}"
    );
}

#[test]
fn django_settings_override_and_first_default_select_the_urlconf() {
    let project = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let root = project.path();
    fs::write(
        root.join("wsgi.py"),
        r#"import os
from django.core.wsgi import get_wsgi_application
os.environ.setdefault("DJANGO_SETTINGS_MODULE", "production")
os.environ.setdefault("DJANGO_SETTINGS_MODULE", "development")
application = get_wsgi_application()
"#,
    )
    .unwrap();
    for name in ["production", "development"] {
        fs::write(
            root.join(format!("{name}.py")),
            format!("ROOT_URLCONF = '{name}_urls'\n"),
        )
        .unwrap();
        fs::write(
            root.join(format!("{name}_urls.py")),
            format!("# {name} URLconf\nfrom django.urls import path\ndef home(request):\n    return 1\nurlpatterns = [path('', home)]\n"),
        ).unwrap();
    }
    for (override_, unused, hits) in [
        (None, "development_urls.home", 0),
        (Some("development"), "production_urls.home", 5),
    ] {
        let report = scan_with_settings(root, override_, None);
        assert_eq!(findings(&report, "unused_functions"), [unused], "{report}");
        let mut cached = scan_with_settings(root, override_, Some(cache.path()));
        let stats = cached.as_object_mut().unwrap().remove("cache").unwrap();
        assert_eq!(stats["hits"], hits);
        assert_eq!(cached, report);
    }
}

#[test]
fn unittest_cases_in_pytest_suffix_modules_are_collected() {
    let project = tempfile::tempdir().unwrap();
    fs::write(
        project.path().join("checks_test.py"),
        r#"import unittest

class Example(unittest.TestCase):
    def test_value(self):
        assert helper() == 1

def helper():
    return 1

def unused():
    return 2
"#,
    )
    .unwrap();
    let report = scan(project.path());
    assert_eq!(
        findings(&report, "unused_functions"),
        ["checks_test.unused"],
        "{report}"
    );
}

#[test]
fn unittest_async_hooks_belong_to_the_isolated_runner() {
    let project = tempfile::tempdir().unwrap();
    fs::write(
        project.path().join("test_setup.py"),
        r#"from unittest import IsolatedAsyncioTestCase, TestCase

class Example(IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        assert prepare() == 1

    async def test_value(self):
        assert True

    async def asyncTearDown(self):
        assert finish() == 1

class SyncExample(TestCase):
    def setUp(self):
        assert sync_setup() == 1

    async def test_unawaited(self):
        return 0

    async def asyncSetUp(self):
        return 0

def prepare():
    return 1

def finish():
    return 1

def sync_setup():
    return 1
"#,
    )
    .unwrap();
    let report = scan(project.path());
    assert_eq!(
        findings(&report, "unused_functions"),
        [
            "test_setup.SyncExample.asyncSetUp",
            "test_setup.SyncExample.test_unawaited",
        ],
        "{report}"
    );
}

#[test]
fn pytest_plugin_fixtures_are_visible_below_local_fixtures() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    fs::write(
        root.join("plugin.py"),
        r#"from pytest import fixture

@fixture
def resource():
    return helper()

@fixture
def choice():
    return 2

def helper():
    return 1
"#,
    )
    .unwrap();
    fs::write(root.join("conftest.py"), "pytest_plugins = ['plugin']\n").unwrap();
    fs::write(
        root.join("test_value.py"),
        r#"from pytest import fixture

@fixture
def choice():
    return 1

def test_value(resource, choice):
    assert resource == choice
"#,
    )
    .unwrap();
    let report = scan(root);
    assert_eq!(
        findings(&report, "unused_functions"),
        ["plugin.choice"],
        "{report}"
    );
}

#[test]
fn pytest_applies_autouse_plugin_fixtures_to_unittest_cases() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    fs::write(root.join("conftest.py"), "pytest_plugins = ['plugin']\n").unwrap();
    fs::write(
        root.join("plugin.py"),
        r#"from pytest import fixture

@fixture(autouse=True)
def setup_environment():
    assert prepare() == 1

@fixture
def explicit():
    return 2

def prepare():
    return 1
"#,
    )
    .unwrap();
    fs::write(
        root.join("checks_test.py"),
        r#"import unittest

class WebChecks(unittest.TestCase):
    def test_index(self, explicit=None):
        assert explicit is None
"#,
    )
    .unwrap();
    let report = scan(root);
    assert_eq!(
        findings(&report, "unused_functions"),
        ["plugin.explicit"],
        "{report}"
    );
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
