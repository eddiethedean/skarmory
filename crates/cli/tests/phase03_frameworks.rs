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
    } else {
        command.args(["--workers", "1"]);
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
fn django_management_settings_option_selects_the_installed_commands() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    for (settings, app) in [("production", "primary"), ("staging", "alternate")] {
        fs::write(
            root.join(format!("{settings}.py")),
            format!("INSTALLED_APPS = ['{app}']\n"),
        )
        .unwrap();
        let app_dir = root.join(app);
        let commands = app_dir.join("management/commands");
        fs::create_dir_all(&commands).unwrap();
        fs::write(app_dir.join("__init__.py"), "").unwrap();
        fs::write(commands.join("record.py"), format!(
            "from django.core.management.base import BaseCommand\nclass Command(BaseCommand):\n    def handle(self, *args, **options):\n        return record_message()\ndef record_message():\n    return '{settings}'\n",
        )).unwrap();
    }
    let launcher = |argv: &str| {
        fs::write(
            root.join("manage.py"),
            format!(
                "import os\nfrom os import name\nfrom django.core.management import execute_from_command_line\nos.environ.setdefault('DJANGO_SETTINGS_MODULE', 'production')\nexecute_from_command_line(argv=['manage.py', 'record', {argv}])\n"
            ),
        )
        .unwrap();
    };

    for (options, unused_app) in [
        ("'--settings=staging'", "primary"),
        ("'--settings', 'staging'", "primary"),
        ("'--settings=production', '--settings=staging'", "primary"),
        ("'--settings=staging', '--settings='", "alternate"),
        ("'--', '--settings=staging'", "alternate"),
    ] {
        launcher(options);
        let report = scan_with_settings(root, Some("production"), None);
        let module = format!("{unused_app}.management.commands.record");
        assert_eq!(
            findings(&report, "unused_functions"),
            [
                format!("{module}.Command.handle"),
                format!("{module}.record_message"),
            ],
            "{options}: {report}"
        );
    }

    launcher("'--settings', name");
    let report = scan_with_exit(root, Some("production"), None, 2);
    assert_eq!(report["coverage"]["status"], "incomplete", "{report}");
    assert!(findings(&report, "unused_functions").is_empty(), "{report}");
    assert!(
        report["coverage"]["issues"]
            .as_array()
            .unwrap()
            .iter()
            .any(|issue| {
                issue["kind"] == "unresolved_registration"
                    && issue["message"] == "Django startup settings could not be resolved"
            }),
        "{report}"
    );
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
fn django_registered_views_reach_inherited_setup() {
    let project = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let root = project.path();
    fs::write(root.join("settings.py"), "ROOT_URLCONF = 'urls'\n").unwrap();
    fs::write(
        root.join("wsgi.py"),
        "import os\nfrom django.core.wsgi import get_wsgi_application\nos.environ.setdefault('DJANGO_SETTINGS_MODULE', 'settings')\napplication = get_wsgi_application()\n",
    )
    .unwrap();
    let write_views = |setup_body: &str| {
        fs::write(
            root.join("urls.py"),
            format!(
                r#"from django.http import HttpResponse
from django.urls import path
from django.views import View

class Audited(View):
    def setup(self, request):
        audit_setup()
{setup_body}
    def get(self, request):
        return HttpResponse("ok")

class Home(Audited):
    pass

class Dormant(View):
    def setup(self, request):
        dormant_setup()
        raise RuntimeError("dormant")

def audit_setup():
    return "audit"

def dormant_setup():
    return "dormant"

dormant_view = Dormant.as_view()
urlpatterns = [path("", Home.as_view())]
"#,
            ),
        )
        .unwrap();
    };
    let assert_setup_evidence = |report: &Value| {
        let symbols = report["dead_code_evidence"]["symbols"].as_array().unwrap();
        for (name, kind) in [
            ("urls.Audited.setup", "framework_lifecycle"),
            ("urls.audit_setup", "direct_call"),
        ] {
            assert!(
                symbols.iter().any(|row| {
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
    };

    write_views("        raise RuntimeError('request rejected')");
    let report = scan(root);
    assert_eq!(
        findings(&report, "unused_functions"),
        ["urls.Dormant.setup", "urls.dormant_setup"],
        "{report}"
    );
    assert_setup_evidence(&report);
    for hits in [0, 3] {
        let mut cached = scan_with_settings(root, None, Some(cache.path()));
        let stats = cached.as_object_mut().unwrap().remove("cache").unwrap();
        assert_eq!(stats["hits"], hits);
        assert_eq!(cached, report);
    }

    write_views("        super().setup(request)");
    let incomplete = scan_with_exit(root, None, None, 2);
    assert_eq!(incomplete["coverage"]["status"], "incomplete");
    assert!(
        incomplete["coverage"]["issues"]
            .as_array()
            .unwrap()
            .iter()
            .any(|issue| {
                issue["path"] == "urls.py"
                    && issue["line"] == 8
                    && issue["kind"] == "unresolved_call"
            }),
        "{incomplete}"
    );
    assert_setup_evidence(&incomplete);
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

#[test]
fn pydantic_builds_an_eager_schema_when_the_class_is_defined() {
    let project = tempfile::tempdir().unwrap();
    fs::write(
        project.path().join("models.py"),
        r#"from os import name
from pydantic import BaseModel

def eager_schema():
    return {"type": "any"}

def conditional_schema():
    return {"type": "any"}

def deferred_schema():
    return {"type": "any"}

def dormant_schema():
    return {"type": "any"}

def post_init_only():
    return None

class Eager(BaseModel):
    @classmethod
    def __get_pydantic_core_schema__(cls, source, handler):
        return eager_schema()

    def model_post_init(self, context):
        post_init_only()

class Deferred(BaseModel):
    model_config = {"defer_build": True}

    @classmethod
    def __get_pydantic_core_schema__(cls, source, handler):
        return deferred_schema()

class Conditional(BaseModel):
    model_config = {"defer_build": name == "posix"}

    @classmethod
    def __get_pydantic_core_schema__(cls, source, handler):
        return conditional_schema()

def build_dormant_model():
    class Dormant(BaseModel):
        @classmethod
        def __get_pydantic_core_schema__(cls, source, handler):
            return dormant_schema()
"#,
    )
    .unwrap();

    let report = scan(project.path());
    let unused = findings(&report, "unused_functions");
    for name in ["models.eager_schema", "models.conditional_schema"] {
        assert!(!unused.contains(&name), "{report}");
    }
    for name in [
        "models.deferred_schema",
        "models.dormant_schema",
        "models.post_init_only",
    ] {
        assert!(unused.contains(&name), "{report}");
    }
    assert!(
        findings(&report, "unused_classes").contains(&"models.Eager"),
        "{report}"
    );
    let evidence = report["dead_code_evidence"]["symbols"].as_array().unwrap();
    for (model, certainty) in [("Eager", "certain"), ("Conditional", "possible")] {
        let name = format!("models.{model}.__get_pydantic_core_schema__");
        let schema_evidence: Vec<_> = evidence
            .iter()
            .filter(|row| row["qualified_name"] == name)
            .flat_map(|row| row["evidence"].as_array().unwrap())
            .filter(|entry| entry["kind"] == "pydantic_schema_build")
            .map(|entry| entry["certainty"].as_str().unwrap())
            .collect();
        assert_eq!(schema_evidence, [certainty], "{report}");
    }
}

fn django_signal_project(project: &Path, registrations: &str) {
    fs::create_dir(project.join("inventory")).unwrap();
    for (path, source) in [
        (
            "manage.py",
            "import os\nfrom django.core.management import execute_from_command_line\nos.environ.setdefault('DJANGO_SETTINGS_MODULE', 'settings')\nexecute_from_command_line(['manage.py', 'check'])\n",
        ),
        (
            "settings.py",
            "INSTALLED_APPS = ['inventory.apps.InventoryConfig']\n",
        ),
        ("inventory/__init__.py", ""),
        (
            "inventory/apps.py",
            "from django.apps import AppConfig\nclass InventoryConfig(AppConfig):\n    name = 'inventory'\n    def ready(self):\n        from . import signals\n",
        ),
        ("inventory/signals.py", registrations),
    ] {
        fs::write(project.join(path), source).unwrap();
    }
}

fn django_signal_evidence(report: &Value) -> Vec<(&str, &str)> {
    let mut registrations = Vec::new();
    for row in report["dead_code_evidence"]["symbols"].as_array().unwrap() {
        for event in row["evidence"].as_array().unwrap() {
            if event["kind"] == "django_signal" {
                registrations.push((
                    row["qualified_name"].as_str().unwrap(),
                    event["certainty"].as_str().unwrap(),
                ));
            }
        }
    }
    registrations.sort_unstable();
    registrations
}

#[test]
fn django_resolves_loaded_signal_forms_and_leaves_dormant_receivers_unused() {
    let project = tempfile::tempdir().unwrap();
    django_signal_project(
        project.path(),
        r#"from django.db.models.signals import post_save
from django.core.signals import request_finished
from django.dispatch import Signal as Event, receiver as listen
from sys import platform

changed = Event()
signals = [post_save, request_finished]

def update_inventory():
    return 42

@listen(signal=signals)
def refresh(sender, **kwargs):
    return update_inventory()

register = listen(request_finished)
@register
def report(sender, **kwargs):
    return 1

@listen(changed)
def on_change(sender, **kwargs):
    return 2

def after_change(sender, **kwargs):
    return 3

connect = changed.connect
connect(receiver=after_change)

class InventoryEvent(Event):
    pass

@listen(InventoryEvent())
def on_inventory(sender, **kwargs):
    return 6

def conditional_receiver(sender, **kwargs):
    return 4

if platform == "darwin":
    post_save.connect(conditional_receiver)

def dormant_receiver(sender, **kwargs):
    return 5

def dormant_registration():
    changed.connect(dormant_receiver)

class Lookalike:
    def connect(self, receiver):
        return None

Lookalike().connect(None)

def receiver(callback):
    return callback

@receiver
def looks_registered():
    return 8

@listen([])
def empty_registration(sender, **kwargs):
    return 9
"#,
    );
    fs::write(
        project.path().join("other.py"),
        "from django.dispatch import receiver\nfrom django.db.models.signals import post_save\n@receiver(post_save)\ndef unloaded(sender, **kwargs):\n    return 7\n",
    )
    .unwrap();

    let report = scan(project.path());
    let unused = findings(&report, "unused_functions");
    for name in [
        "inventory.signals.dormant_receiver",
        "inventory.signals.dormant_registration",
        "inventory.signals.looks_registered",
        "inventory.signals.empty_registration",
        "other.unloaded",
    ] {
        assert!(unused.contains(&name), "{unused:?}");
    }
    assert!(
        !unused.contains(&"inventory.signals.update_inventory"),
        "{unused:?}"
    );
    assert_eq!(
        django_signal_evidence(&report),
        [
            ("inventory.signals.after_change", "possible"),
            ("inventory.signals.conditional_receiver", "possible"),
            ("inventory.signals.on_change", "possible"),
            ("inventory.signals.on_inventory", "possible"),
            ("inventory.signals.refresh", "certain"),
            ("inventory.signals.report", "certain"),
        ]
    );

    let cache = tempfile::tempdir().unwrap();
    let cold = scan_with_settings(project.path(), None, Some(cache.path()));
    let warm = scan_with_settings(project.path(), None, Some(cache.path()));
    for key in [
        "unused_functions",
        "unused_parameters",
        "dead_code_evidence",
        "coverage",
    ] {
        assert_eq!(report[key], cold[key], "{key}");
        assert_eq!(cold[key], warm[key], "{key}");
    }
    fs::write(project.path().join("settings.py"), "INSTALLED_APPS = []\n").unwrap();
    let inactive = scan_with_settings(project.path(), None, Some(cache.path()));
    assert!(django_signal_evidence(&inactive).is_empty());
    assert!(findings(&inactive, "unused_functions").contains(&"inventory.signals.refresh"));
}

#[test]
fn django_reports_opaque_or_malformed_signal_registrations_only_when_loaded() {
    let project = tempfile::tempdir().unwrap();
    django_signal_project(
        project.path(),
        r#"from django.db.models.signals import post_save
from django.dispatch import receiver
from external import custom

@receiver(custom)
def opaque(sender, **kwargs):
    return 1

@receiver(post_save, signal=post_save)
def malformed(sender, **kwargs):
    return 2
"#,
    );

    let report = scan_with_exit(project.path(), None, None, 2);
    let issues = report["analysis_errors"].as_array().unwrap();
    assert_eq!(issues.len(), 2, "{report}");
    assert!(issues.iter().all(|issue| {
        issue["file"] == "inventory/signals.py" && issue["kind"] == "unresolved_registration"
    }));
    assert_eq!(
        django_signal_evidence(&report),
        [("inventory.signals.opaque", "possible")],
    );

    fs::write(project.path().join("settings.py"), "INSTALLED_APPS = []\n").unwrap();
    let inactive = scan(project.path());
    let unused = findings(&inactive, "unused_functions");
    for name in ["inventory.signals.opaque", "inventory.signals.malformed"] {
        assert!(unused.contains(&name), "{unused:?}");
    }
}

#[test]
fn django_entry_reaching_setup_checks_its_own_registrations() {
    let project = tempfile::tempdir().unwrap();
    for (path, source) in [
        ("settings.py", "INSTALLED_APPS = []\n"),
        (
            "bootstrap.py",
            "import os\nfrom django import setup\nos.environ.setdefault('DJANGO_SETTINGS_MODULE', 'settings')\nsetup()\n",
        ),
        ("entry.py", "import bootstrap\nimport receivers\n"),
        (
            "receivers.py",
            r#"from django.dispatch import receiver
from integration.signals import changed

@receiver(changed)
def sync_record(sender, **kwargs):
    return sync_helper()

def sync_helper():
    return 1
"#,
        ),
        (
            "unrelated.py",
            "import settings\nfrom django.db.models.signals import post_save\nfrom django.dispatch import receiver\n@receiver(post_save)\ndef dormant(sender, **kwargs):\n    return 2\n",
        ),
    ] {
        fs::write(project.path().join(path), source).unwrap();
    }

    let report = scan_with_exit(project.path(), None, None, 2);
    let issues = report["analysis_errors"].as_array().unwrap();
    assert_eq!(issues.len(), 1, "{report}");
    assert_eq!(issues[0]["file"], "receivers.py");
    assert_eq!(issues[0]["kind"], "unresolved_registration");
    assert_eq!(
        django_signal_evidence(&report),
        [("receivers.sync_record", "possible")],
    );

    fs::write(
        project.path().join("bootstrap.py"),
        "import os\nfrom django import setup\ndef initialize():\n    os.environ.setdefault('DJANGO_SETTINGS_MODULE', 'settings')\n    setup()\n",
    )
    .unwrap();
    fs::write(
        project.path().join("entry.py"),
        "from bootstrap import initialize\nfrom receivers import subscribe\ninitialize()\nsubscribe()\n",
    )
    .unwrap();
    fs::write(
        project.path().join("receivers.py"),
        r#"from django.db.models.signals import post_save as changed
from integration import on_save

def subscribe():
    changed.connect(on_save)
"#,
    )
    .unwrap();

    let report = scan_with_exit(project.path(), None, None, 2);
    let issues = report["analysis_errors"].as_array().unwrap();
    assert_eq!(issues.len(), 1, "{report}");
    assert_eq!(issues[0]["file"], "receivers.py");
    assert_eq!(issues[0]["kind"], "unresolved_registration");

    fs::write(
        project.path().join("receivers.py"),
        r#"from django.db.models.signals import post_save as changed

def on_save(sender, **kwargs):
    return 1

def subscribe():
    changed.connect(on_save)
"#,
    )
    .unwrap();
    let report = scan(project.path());
    assert!(findings(&report, "unused_functions").contains(&"unrelated.dormant"));
    assert_eq!(
        django_signal_evidence(&report),
        [("receivers.on_save", "possible")],
    );
    let cache = tempfile::tempdir().unwrap();
    for _ in 0..2 {
        let cached = scan_with_settings(project.path(), None, Some(cache.path()));
        for key in ["coverage", "dead_code_evidence", "unused_functions"] {
            assert_eq!(cached[key], report[key], "{key}");
        }
    }
}

#[test]
fn django_dormant_signal_receivers_do_not_change_dispatch() {
    let project = tempfile::tempdir().unwrap();
    let active = r#"from django.db.models.signals import post_save

class Handler:
    def on_save(self, sender, **kwargs):
        return self.process()

class Installed(Handler):
    def process(self):
        return 1

installed = Installed()
post_save.connect(installed.on_save)
"#;
    let dormant = r#"
def install_optional():
    instance = Optional()
    post_save.connect(instance.on_save)
"#;
    let callable = format!(
        "{active}\nclass Optional(Handler):\n    def process(self):\n        return 2\n{dormant}installed.on_save(None)\n",
    );
    django_signal_project(project.path(), &callable);
    let report = scan(project.path());
    let unused = findings(&report, "unused_functions");
    assert!(
        unused.contains(&"inventory.signals.Optional.process"),
        "{report}"
    );
    assert!(
        !unused.contains(&"inventory.signals.Installed.process"),
        "{report}"
    );

    let non_callable = format!("{active}\nclass Optional(Handler):\n    process = None\n{dormant}");
    let signals = project.path().join("inventory/signals.py");
    fs::write(&signals, &non_callable).unwrap();
    let report = scan(project.path());
    assert_eq!(
        django_signal_evidence(&report),
        [("inventory.signals.Handler.on_save", "certain")],
    );
    let unused = findings(&report, "unused_functions");
    assert!(
        unused.contains(&"inventory.signals.install_optional"),
        "{report}"
    );
    assert!(
        !unused.contains(&"inventory.signals.Installed.process"),
        "{report}"
    );

    fs::write(&signals, format!("{non_callable}\ninstall_optional()\n")).unwrap();
    let report = scan_with_exit(project.path(), None, None, 2);
    let issues = report["analysis_errors"].as_array().unwrap();
    assert_eq!(issues.len(), 1, "{report}");
    assert_eq!(issues[0]["file"], "inventory/signals.py");
    assert_eq!(issues[0]["kind"], "unresolved_call");
}

#[test]
fn atexit_registrations_retain_only_reached_callbacks_and_transitive_helpers() {
    let project = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let path = project.path().join("shutdown.py");
    fs::write(
        &path,
        r#"from atexit import register as on_exit
from os import name

def leaf():
    return 1

def cleanup(code=0):
    return leaf() + code

def platform_cleanup(code=0):
    return code

callback = cleanup if name == "posix" else platform_cleanup
on_exit(callback, code=1)

@on_exit
def decorated():
    return 2

def unused():
    return 3

def dormant():
    on_exit(unused)
"#,
    )
    .unwrap();
    let report = scan(project.path());
    assert_eq!(
        findings(&report, "unused_functions"),
        ["shutdown.dormant", "shutdown.unused"],
        "{report}"
    );
    let symbols = report["dead_code_evidence"]["symbols"].as_array().unwrap();
    for (name, kind) in [
        ("shutdown.cleanup", "atexit_registration"),
        ("shutdown.platform_cleanup", "atexit_registration"),
        ("shutdown.decorated", "atexit_registration"),
        ("shutdown.leaf", "direct_call"),
    ] {
        assert!(
            symbols.iter().any(|row| {
                row["qualified_name"] == name
                    && row["evidence"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|entry| entry["kind"] == kind && entry["certainty"] == "possible")
            }),
            "missing possible {kind} evidence for {name}: {report}"
        );
    }
    for hits in [0, 1] {
        let mut cached = scan_with_settings(project.path(), None, Some(cache.path()));
        let stats = cached.as_object_mut().unwrap().remove("cache").unwrap();
        assert_eq!(stats["hits"], hits);
        assert_eq!(cached, report);
    }

    // An independent direct call establishes reachability through the same body.
    let contents = fs::read_to_string(&path).unwrap();
    fs::write(&path, contents + "\ncleanup()\n").unwrap();
    let updated = scan(project.path());
    assert!(
        updated["dead_code_evidence"]["symbols"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| {
                row["qualified_name"] == "shutdown.leaf"
                    && row["evidence"].as_array().unwrap().iter().any(|entry| {
                        entry["kind"] == "direct_call" && entry["certainty"] == "certain"
                    })
            })
    );
    let mut cached = scan_with_settings(project.path(), None, Some(cache.path()));
    cached.as_object_mut().unwrap().remove("cache");
    assert_eq!(cached, updated);
}

#[test]
fn atexit_registration_checks_provider_identity_and_unresolved_targets() {
    let project = tempfile::tempdir().unwrap();
    let path = project.path().join("shutdown.py");
    fs::write(
        &path,
        r#"from atexit import register
from sys import argv

def cleanup():
    return 1

register(argv[0])
"#,
    )
    .unwrap();
    let report = scan_with_exit(project.path(), None, None, 2);
    assert_eq!(report["coverage"]["status"], "incomplete");
    assert!(
        report["coverage"]["issues"]
            .as_array()
            .unwrap()
            .iter()
            .any(|issue| issue["kind"] == "unresolved_callback" && issue["line"] == 7)
    );
    assert!(findings(&report, "unused_functions").is_empty());

    fs::write(
        &path,
        r#"from atexit import register

def fake_register(value):
    return 1

register = fake_register

def cleanup():
    return 1

register(1)
"#,
    )
    .unwrap();
    let report = scan(project.path());
    assert_eq!(findings(&report, "unused_functions"), ["shutdown.cleanup"]);
    let symbols = report["dead_code_evidence"]["symbols"].as_array().unwrap();
    assert!(symbols.iter().all(|row| {
        row["evidence"]
            .as_array()
            .unwrap()
            .iter()
            .all(|entry| entry["kind"] != "atexit_registration")
    }));
}

#[test]
fn atexit_callback_reads_respect_dormant_lexical_bindings() {
    let project = tempfile::tempdir().unwrap();
    for (import, dormant, call) in [
        (
            "from atexit import register as schedule",
            "schedule",
            "schedule(cleanup)",
        ),
        (
            "import atexit as shutdown",
            "shutdown",
            "shutdown.register(cleanup)",
        ),
        (
            "from atexit import register",
            "cleanup",
            "register(cleanup)",
        ),
    ] {
        fs::write(
            project.path().join("shutdown.py"),
            format!(
                "{import}\n\ndef cleanup():\n    return 1\n\ndef run():\n    if False:\n        {dormant} = print\n    {call}\n\nrun()\n"
            ),
        )
        .unwrap();
        let report = scan_with_exit(project.path(), None, None, 2);
        assert_eq!(report["coverage"]["status"], "incomplete");
        assert!(
            report["coverage"]["issues"]
                .as_array()
                .unwrap()
                .iter()
                .any(|issue| issue["kind"] == "unbound_local" && issue["line"] == 9)
        );
        assert!(findings(&report, "unused_functions").is_empty());
    }
}

#[test]
fn atexit_dormant_bound_callbacks_do_not_change_live_receiver_dispatch() {
    let project = tempfile::tempdir().unwrap();
    let path = project.path().join("shutdown.py");
    for (method, has_finish) in [
        ("    pass\n", false),
        (
            "    def finish(self):\n        return dormant_helper()\n",
            true,
        ),
    ] {
        fs::write(
            &path,
            format!(
                r#"from atexit import register

class Handler:
    def cleanup(self):
        return self.finish()

class Active(Handler):
    def finish(self):
        return 1

def dormant_helper():
    return 2

class Dormant(Handler):
{method}
def install_dormant():
    resource = Dormant()
    register(resource.cleanup)

Active().cleanup()
"#
            ),
        )
        .unwrap();
        let report = scan(project.path());
        let unused = findings(&report, "unused_functions");
        assert!(unused.contains(&"shutdown.dormant_helper"), "{report}");
        assert!(unused.contains(&"shutdown.install_dormant"), "{report}");
        assert_eq!(
            unused.contains(&"shutdown.Dormant.finish"),
            has_finish,
            "{report}"
        );
        assert!(!unused.contains(&"shutdown.Active.finish"), "{report}");
    }
}

#[test]
fn atexit_reached_bound_callback_retains_its_receiver_with_possible_evidence() {
    let project = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let path = project.path().join("shutdown.py");
    fs::write(
        &path,
        r#"from atexit import register
from os import name
from helpers import live_helper, shutdown_helper

class Handler:
    def cleanup(self):
        return self.finish()

class Active(Handler):
    def finish(self):
        return live_helper()

class AtExit(Handler):
    def finish(self):
        return self.flush()

    def flush(self):
        return shutdown_helper()

def install_shutdown():
    resource = AtExit()
    register(resource.cleanup)

Active().cleanup()
if name == "posix":
    install_shutdown()
"#,
    )
    .unwrap();
    fs::write(
        project.path().join("helpers.py"),
        "def live_helper():\n    return 1\n\ndef shutdown_helper():\n    return 2\n",
    )
    .unwrap();
    let report = scan(project.path());
    assert!(findings(&report, "unused_functions").is_empty(), "{report}");
    let symbols = report["dead_code_evidence"]["symbols"].as_array().unwrap();
    for (name, kind, certainty) in [
        (
            "shutdown.Handler.cleanup",
            "atexit_registration",
            "possible",
        ),
        ("shutdown.Active.finish", "direct_call", "certain"),
        ("helpers.live_helper", "direct_call", "certain"),
        ("shutdown.AtExit.finish", "direct_call", "possible"),
        ("shutdown.AtExit.flush", "direct_call", "possible"),
        ("helpers.shutdown_helper", "direct_call", "possible"),
    ] {
        assert!(
            symbols.iter().any(|row| {
                row["qualified_name"] == name
                    && row["evidence"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|entry| entry["kind"] == kind && entry["certainty"] == certainty)
            }),
            "missing {certainty} {kind} evidence for {name}: {report}"
        );
    }
    for row in symbols.iter().filter(|row| {
        row["qualified_name"] == "shutdown.AtExit.finish"
            || row["qualified_name"] == "shutdown.AtExit.flush"
            || row["qualified_name"] == "helpers.shutdown_helper"
    }) {
        assert!(
            row["evidence"]
                .as_array()
                .unwrap()
                .iter()
                .all(|entry| entry["certainty"] != "certain"),
            "unexpected certain shutdown-only evidence: {row}"
        );
    }
    for _ in 0..2 {
        let mut cached = scan_with_settings(project.path(), None, Some(cache.path()));
        cached.as_object_mut().unwrap().remove("cache");
        assert_eq!(cached, report);
    }

    let updated = fs::read_to_string(&path)
        .unwrap()
        .replace("if name == \"posix\":\n    install_shutdown()\n", "");
    fs::write(path, updated).unwrap();
    let report = scan(project.path());
    let unused = findings(&report, "unused_functions");
    assert!(unused.contains(&"shutdown.AtExit.finish"), "{report}");
    assert!(unused.contains(&"helpers.shutdown_helper"), "{report}");
    let mut cached = scan_with_settings(project.path(), None, Some(cache.path()));
    cached.as_object_mut().unwrap().remove("cache");
    assert_eq!(cached, report);
}
