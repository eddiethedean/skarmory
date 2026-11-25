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

fn unreachable_functions(report: &Value) -> Vec<&str> {
    let mut names = findings(report, "unused_functions");
    names.extend(
        report["suppressed"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|row| row["reason"] == "covered by the unused class finding")
            .map(|row| row["full_name"].as_str().unwrap()),
    );
    names.sort_unstable();
    names
}

#[test]
fn sanic_worker_listener_requires_resolved_registration_and_app_selection() {
    let project = tempfile::tempdir().unwrap();
    fs::write(
        project.path().join("app.py"),
        r#"from sanic import Sanic

def helper(): return 1
async def active(application, loop): return helper()
async def dormant(application): return None
async def unregistered(application): return None

app = Sanic('serving')
unused_app = Sanic('unused')
app.register_listener(active, 'before_server_start')
unused_app.register_listener(dormant, 'before_server_start')
"#,
    )
    .unwrap();
    fs::write(
        project.path().join("worker.py"),
        r#"from sanic.app import Sanic as SanicApp

async def explicit(application): return None
worker = SanicApp(name='worker')
worker.register_listener(listener=explicit, event='after_server_stop')
worker.run()
"#,
    )
    .unwrap();
    let report = scan(project.path());
    assert_eq!(
        findings(&report, "unused_functions"),
        ["app.dormant", "app.unregistered"]
    );
    let unused_parameters = findings(&report, "unused_parameters");
    assert!(!unused_parameters.contains(&"app.active.application"));
    assert!(!unused_parameters.contains(&"app.active.loop"));
    assert!(!unused_parameters.contains(&"worker.explicit.application"));
    let symbols = report["dead_code_evidence"]["symbols"].as_array().unwrap();
    for name in ["app.active", "worker.explicit"] {
        assert!(symbols.iter().any(|row| row["qualified_name"] == name
            && row["evidence"].as_array().unwrap().iter().any(|edge| {
                edge["kind"] == "sanic_listener" && edge["certainty"] == "possible"
            })));
    }
}

#[test]
fn pydantic_computed_fields_follow_dump_and_property_access() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    fs::write(
        root.join("models.py"),
        concat!(
            "from pydantic import BaseModel, computed_field\n",
            "class Dumped(BaseModel):\n    prefix: str\n",
            "    @computed_field\n    @property\n",
            "    def name(self) -> str: return f'{self.prefix} {dump_name()}'\n",
            "class Accessed(BaseModel):\n",
            "    @computed_field\n    def name(self) -> str: return accessed_name()\n",
            "class Constructed(BaseModel):\n",
            "    @computed_field\n    @property\n",
            "    def name(self) -> str: return dormant_name()\n",
            "def dump_name(): return 'dumped'\n",
            "def accessed_name(): return 'accessed'\n",
            "def dormant_name(): return 'dormant'\n",
            "Dumped(prefix='model').model_dump()\n",
            "Accessed().name\n",
            "Constructed()\n",
        ),
    )
    .unwrap();
    let report = scan(root);
    let unused = findings(&report, "unused_functions");
    for live in [
        "models.Dumped.name",
        "models.dump_name",
        "models.Accessed.name",
        "models.accessed_name",
    ] {
        assert!(!unused.contains(&live), "{live}: {report}");
    }
    for dormant in ["models.Constructed.name", "models.dormant_name"] {
        assert!(unused.contains(&dormant), "{dormant}: {report}");
    }
}

#[test]
fn formatted_string_retains_known_special_methods_and_reports_unresolved_dispatch() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    let path = root.join("formatting.py");
    let source = concat!(
        "class Formatter:\n",
        "    def __format__(self, spec): return render()\n",
        "def render(): return 'value'\n",
        "def dormant(): return 'dormant'\n",
        "def never_called(): return f'{dormant()}'\n",
        "print(f'{Formatter()} formatted')\n",
    );
    fs::write(&path, source).unwrap();
    let report = scan(root);
    assert_eq!(
        findings(&report, "unused_functions"),
        ["formatting.dormant", "formatting.never_called"],
        "{report}"
    );
    let cache = tempfile::tempdir().unwrap();
    for _ in 0..2 {
        let mut cached = scan_with_settings(root, None, Some(cache.path()));
        cached.as_object_mut().unwrap().remove("cache");
        assert_eq!(cached, report);
    }

    fs::write(
        path,
        format!("{source}from config import opaque\nprint(f'{{opaque}}')\n"),
    )
    .unwrap();
    let incomplete = scan_with_exit(root, None, Some(cache.path()), 2);
    assert!(
        incomplete["coverage"]["issues"]
            .as_array()
            .unwrap()
            .iter()
            .any(|issue| issue["kind"] == "unresolved_formatting"),
        "{incomplete}"
    );
}

#[test]
fn attrs_field_declarations_require_a_resolved_class_transform() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    fs::write(
        root.join("models.py"),
        r#"from attrs import define as declare

@declare
class User:
    user_id: int
    label: str = 'user'

def define(cls): return cls

@define
class Draft:
    label: str = 'draft'
"#,
    )
    .unwrap();

    let report = scan(root);
    assert_eq!(
        findings(&report, "unused_variables"),
        ["models.Draft.label"],
        "{report}"
    );
    assert!(
        report["dead_code_evidence"]["symbols"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| {
                row["qualified_name"] == "models.User.label"
                    && row["evidence"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|edge| edge["kind"] == "attrs_field" && edge["certainty"] == "certain")
            }),
        "{report}"
    );
}

#[test]
fn pluggy_marker_retains_metadata_without_executing_an_unregistered_hook() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    let path = root.join("hooks.py");
    let source = r#"import pluggy

marker = pluggy.HookimplMarker('demo')

@marker
def hook(config): return handle(config)

def handle(config): return config

def hookimpl(callback): return callback

@hookimpl
def lookalike(config): return config
"#;
    fs::write(&path, source).unwrap();
    let report = scan(root);
    assert_eq!(
        findings(&report, "unused_functions"),
        ["hooks.handle", "hooks.lookalike"],
        "{report}"
    );
    assert!(
        report["dead_code_evidence"]["symbols"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| {
                row["qualified_name"] == "hooks.hook"
                    && row["evidence"].as_array().unwrap().iter().any(|edge| {
                        edge["kind"] == "pluggy_hook_declaration" && edge["certainty"] == "certain"
                    })
            }),
        "{report}"
    );

    let cache = tempfile::tempdir().unwrap();
    for _ in 0..2 {
        let mut cached = scan_with_settings(root, None, Some(cache.path()));
        cached.as_object_mut().unwrap().remove("cache");
        assert_eq!(cached, report);
    }

    fs::write(path, format!("{source}\nhook('selected')\n")).unwrap();
    let report = scan(root);
    assert_eq!(
        findings(&report, "unused_functions"),
        ["hooks.lookalike"],
        "{report}"
    );
    let mut cached = scan_with_settings(root, None, Some(cache.path()));
    cached.as_object_mut().unwrap().remove("cache");
    assert_eq!(cached, report);
}

#[test]
fn marshmallow_schema_creation_records_registry_and_hook_metadata() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    let path = root.join("schemas.py");
    let source = r#"from marshmallow import Schema as BaseSchema, post_load, validates_schema

class Active(BaseSchema):
    @post_load
    def make_record(self, data, **kwargs): return convert(data)

    @validates_schema()
    def validate_record(self, data, **kwargs): return None

class Private(BaseSchema):
    class Meta:
        register = False

    @post_load
    def private_load(self, data, **kwargs): return data

class Lookalike:
    @post_load
    def make_record(self, data, **kwargs): return data

def convert(data): return data
"#;
    fs::write(&path, source).unwrap();
    let report = scan(root);
    assert_eq!(
        findings(&report, "unused_classes"),
        ["schemas.Lookalike", "schemas.Private"],
        "{report}"
    );
    assert_eq!(
        findings(&report, "unused_functions"),
        ["schemas.Lookalike.make_record", "schemas.convert"],
        "{report}"
    );
    for (name, kind) in [
        ("schemas.Active", "marshmallow_schema_registry"),
        ("schemas.Active.make_record", "marshmallow_hook_declaration"),
        (
            "schemas.Active.validate_record",
            "marshmallow_hook_declaration",
        ),
        (
            "schemas.Private.private_load",
            "marshmallow_hook_declaration",
        ),
    ] {
        assert!(
            report["dead_code_evidence"]["symbols"]
                .as_array()
                .unwrap()
                .iter()
                .any(|row| {
                    row["qualified_name"] == name
                        && row["evidence"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .any(|edge| edge["kind"] == kind && edge["certainty"] == "certain")
                }),
            "{report}"
        );
    }
    let cache = tempfile::tempdir().unwrap();
    for _ in 0..2 {
        let mut cached = scan_with_settings(root, None, Some(cache.path()));
        cached.as_object_mut().unwrap().remove("cache");
        assert_eq!(cached, report);
    }

    fs::write(
        path,
        source.replace(
            "register = False",
            "from config import enabled\n        register = enabled",
        ),
    )
    .unwrap();
    let incomplete = scan_with_exit(root, None, Some(cache.path()), 2);
    assert!(
        incomplete["coverage"]["issues"]
            .as_array()
            .unwrap()
            .iter()
            .any(|issue| issue["kind"] == "unresolved_registration"),
        "{incomplete}"
    );
}

#[test]
fn sqlalchemy_hybrid_descriptor_retains_the_getter_without_calling_it() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    fs::write(
        root.join("model.py"),
        concat!(
            "from sqlalchemy.ext.hybrid import hybrid_property\n",
            "class User:\n",
            "    @hybrid_property\n",
            "    def full_name(self): return make_name()\n",
            "def make_name(): return 'Ada'\n",
            "class Profile:\n",
            "    def render_name(self): return 'Ada'\n",
            "Profile()\n",
        ),
    )
    .unwrap();
    let report = scan(root);
    let unused = findings(&report, "unused_functions");
    assert!(!unused.contains(&"model.User.full_name"));
    assert!(unused.contains(&"model.make_name"));
    assert!(unused.contains(&"model.Profile.render_name"));
    assert!(
        report["dead_code_evidence"]["symbols"]
            .as_array()
            .unwrap()
            .iter()
            .any(|symbol| symbol["qualified_name"] == "model.User.full_name"
                && symbol["evidence"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|row| row["kind"] == "sqlalchemy_hybrid_descriptor"))
    );
}

#[test]
fn flask_login_callbacks_require_the_real_manager_and_a_reached_registration() {
    let project = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let root = project.path();
    fs::write(
        root.join("providers.py"),
        concat!(
            "from flask_login import LoginManager as Manager\n",
            "login = Manager()\n",
            "login.login_view = 'auth.login'\n",
        ),
    )
    .unwrap();
    fs::write(
        root.join("web.py"),
        concat!(
            "from providers import login\n",
            "@login.user_loader\ndef load_user(user_id): return lookup(user_id)\n",
            "def lookup(user_id): return user_id\n",
            "def other_user(user_id): return user_id\n",
            "def dormant():\n",
            "    login.user_loader(other_user)\n",
            "def user_loader(function): return function\n",
            "@user_loader\ndef lookalike(user_id): return user_id\n",
        ),
    )
    .unwrap();
    let report = scan(root);
    let unused = findings(&report, "unused_functions");
    assert!(!unused.contains(&"web.load_user"));
    assert!(!unused.contains(&"web.lookup"));
    assert!(unused.contains(&"web.other_user"));
    assert!(unused.contains(&"web.lookalike"));
    for _ in 0..2 {
        let mut cached = scan_with_settings(root, None, Some(cache.path()));
        cached.as_object_mut().unwrap().remove("cache");
        assert_eq!(report, cached);
    }

    fs::write(
        root.join("web.py"),
        concat!(
            "from providers import login\n",
            "def passthrough(function): return function\n",
            "login.user_loader = passthrough\n",
            "@login.user_loader\ndef replaced(user_id): return user_id\n",
        ),
    )
    .unwrap();
    let edited = scan(root);
    assert!(findings(&edited, "unused_functions").contains(&"web.replaced"));
    let mut cached = scan_with_settings(root, None, Some(cache.path()));
    cached.as_object_mut().unwrap().remove("cache");
    assert_eq!(edited, cached);
}

#[test]
fn flask_cli_factory_reaches_bound_setup_and_context_hooks() {
    let project = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let root = project.path();
    fs::create_dir(root.join("shop")).unwrap();
    fs::create_dir(root.join("fake")).unwrap();
    fs::write(
        root.join("shop/__init__.py"),
        r#"from flask import Flask

def create_app(config=None):
    app = Flask(__name__)
    from . import db, auth
    db.init_app(app)
    app.register_blueprint(auth.bp)
    return app
"#,
    )
    .unwrap();
    fs::write(
        root.join("fake/__init__.py"),
        "def create_app(): return object()\n",
    )
    .unwrap();
    let auth = r#"from flask import Blueprint, g
bp = Blueprint('auth', __name__)

@bp.before_app_request
def load_user():
    g.user = None
"#;
    fs::write(root.join("shop/auth.py"), auth).unwrap();
    fs::write(
        root.join("shop/db.py"),
        r#"from flask import g

def close_db(exc=None):
    connection = g.pop('db', None)
    if connection is not None:
        connection.close()

def init_app(app):
    app.teardown_appcontext(close_db)

def unused(required): return 1
"#,
    )
    .unwrap();
    let report = scan(root);
    assert_eq!(
        findings(&report, "unused_functions"),
        ["fake.create_app", "shop.db.unused"]
    );
    assert!(!findings(&report, "unused_parameters").contains(&"shop.db.close_db.exc"));
    let symbols = report["dead_code_evidence"]["symbols"].as_array().unwrap();
    for callback in ["shop.create_app", "shop.auth.load_user", "shop.db.close_db"] {
        assert!(
            symbols.iter().any(|row| row["qualified_name"] == callback
                && row["evidence"].as_array().unwrap().iter().any(|edge| {
                    edge["kind"].as_str().unwrap().starts_with("flask_")
                        && edge["certainty"] == "possible"
                })),
            "{report}"
        );
    }
    for _ in 0..2 {
        let cached = scan_with_settings(root, None, Some(cache.path()));
        for key in [
            "unused_functions",
            "unused_parameters",
            "dead_code_evidence",
            "coverage",
        ] {
            assert_eq!(report[key], cached[key], "{key}");
        }
    }

    // An additional context writer in a different file voids the absence proof.
    fs::write(
        root.join("shop/auth.py"),
        format!("{auth}    g.db = object()\n"),
    )
    .unwrap();
    let changed = scan_with_exit(root, None, Some(cache.path()), 2);
    assert!(
        changed["analysis_errors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| { row["file"] == "shop/db.py" && row["kind"] == "unresolved_call" }),
        "{changed}"
    );
}

#[test]
fn flask_blueprint_child_reads_the_initialized_package_prefix() {
    let project = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let root = project.path();
    fs::create_dir_all(root.join("app/views")).unwrap();
    fs::write(
        root.join("app/views/__init__.py"),
        "from flask import Blueprint as Factory\n\
         bp = Factory('views', __name__)\n\
         from app.views import routes\n",
    )
    .unwrap();
    fs::write(
        root.join("app/views/routes.py"),
        "from app.views import bp\n\
         @bp.before_app_request\n\
         def prepare(): return helper()\n\
         def helper(): return 1\n\
         def retired(): return 2\n",
    )
    .unwrap();
    let factory = "from flask import Flask\n\
                   def create_app():\n    app = Flask(__name__)\n    from app.views import bp\n";
    for (mount, unused) in [
        (
            "    app.register_blueprint(bp)\n",
            vec!["app.views.routes.retired"],
        ),
        (
            "",
            vec![
                "app.views.routes.helper",
                "app.views.routes.prepare",
                "app.views.routes.retired",
            ],
        ),
    ] {
        fs::write(
            root.join("app/__init__.py"),
            format!("{factory}{mount}    return app\n"),
        )
        .unwrap();
        let report = scan(root);
        assert_eq!(findings(&report, "unused_functions"), unused);
        let mut cached = scan_with_settings(root, None, Some(cache.path()));
        cached.as_object_mut().unwrap().remove("cache");
        assert_eq!(report, cached);
    }
    // If the child runs before the export exists, no finite prefix can prove
    // this cycle safe.
    fs::write(
        root.join("app/views/__init__.py"),
        "from flask import Blueprint\n\
         from app.views import routes\n\
         bp = Blueprint('views', __name__)\n",
    )
    .unwrap();
    let report = scan_with_exit(root, None, Some(cache.path()), 2);
    assert!(
        report["analysis_errors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|error| { error["kind"] == "cyclic_imports" })
    );
}

#[test]
fn flask_registration_observes_ordered_writes_through_instance_aliases() {
    let project = tempfile::tempdir().unwrap();
    let file = project.path().join("web.py");
    let source = r#"from flask import Blueprint, Flask
from flask_login import LoginManager

app = Flask(__name__)
blueprint = Blueprint('shop', __name__)
login = LoginManager()

@app.shell_context_processor
def original(): return {}

app_alias = app
blueprint_alias = blueprint
login_alias = login
app_alias.shell_context_processor = lambda callback: callback
setattr(blueprint_alias, 'before_app_request', lambda callback: callback)
login_alias.user_loader = lambda callback: callback

@app.shell_context_processor
def replaced(): return {}

@blueprint.before_app_request
def detached(): return None

@login.user_loader
def detached_login(user_id): return None

app.register_blueprint(blueprint)
"#;
    fs::write(&file, source).unwrap();
    let report = scan(project.path());
    assert_eq!(
        findings(&report, "unused_functions"),
        ["web.detached", "web.detached_login", "web.replaced"]
    );

    fs::write(
        &file,
        source.replace(
            "app_alias.shell_context_processor = lambda callback: callback",
            "app_alias.shell_context_processor = opaque",
        ),
    )
    .unwrap();
    let report = scan_with_exit(project.path(), None, None, 2);
    assert!(
        report["analysis_errors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|error| { error["kind"] == "unresolved_attribute_write" })
    );
    let symbols = report["dead_code_evidence"]["symbols"].as_array().unwrap();
    assert!(
        symbols
            .iter()
            .any(|row| row["qualified_name"] == "web.original")
    );
    assert!(!symbols.iter().any(|row| {
        row["qualified_name"] == "web.replaced"
            && row["evidence"]
                .as_array()
                .unwrap()
                .iter()
                .any(|edge| edge["kind"] == "flask_registration")
    }));
}

#[test]
fn fastapi_startup_keeps_callbacks_with_proven_identity_decorators() {
    let project = tempfile::tempdir().unwrap();
    fs::write(
        project.path().join("web.py"),
        r#"from fastapi import FastAPI

def label(callback):
    return callback

def initialize(): return 1
def abandoned(): return 2

@label
def startup(): return initialize()

app = FastAPI(on_startup=[startup])
"#,
    )
    .unwrap();
    let report = scan(project.path());
    assert_eq!(findings(&report, "unused_functions"), ["web.abandoned"]);
}

#[test]
fn fastapi_environment_guard_keeps_possible_handler_evidence() {
    let project = tempfile::tempdir().unwrap();
    fs::write(
        project.path().join("web.py"),
        r#"import os
from fastapi import FastAPI

def handle_error(request, exc): return helper()
def helper(): return 1
def retired(): return 2

app = FastAPI()
if os.getenv('USE_HANDLER'):
    app.add_exception_handler(Exception, handle_error)
"#,
    )
    .unwrap();
    let report = scan(project.path());
    assert_eq!(findings(&report, "unused_functions"), ["web.retired"]);
    assert!(findings(&report, "unused_parameters").is_empty());
    let symbols = report["dead_code_evidence"]["symbols"].as_array().unwrap();
    assert!(symbols.iter().any(|row| {
        row["qualified_name"] == "web.handle_error"
            && row["evidence"].as_array().unwrap().iter().any(|edge| {
                edge["kind"] == "fastapi_exception_handler" && edge["certainty"] == "possible"
            })
    }));
}

#[test]
fn fastapi_conditional_factory_keeps_handler_and_signature_as_possible() {
    let project = tempfile::tempdir().unwrap();
    let file = project.path().join("web.py");
    for (setup, returned) in [("", "FastAPI()"), ("    shared = FastAPI()\n", "shared")] {
        fs::write(
            &file,
            format!(
                r#"import os
from fastapi import FastAPI

def helper(): return 1
def handle_error(request, exc): return helper()

def create_app(flag):
{setup}    if flag:
        return {returned}
    else:
        return {returned}

app = create_app(os.getenv('APP_CHOICE'))
app.add_exception_handler(Exception, handle_error)
"#
            ),
        )
        .unwrap();
        let report = scan(project.path());
        assert!(findings(&report, "unused_functions").is_empty());
        assert!(findings(&report, "unused_parameters").is_empty());
        let symbols = report["dead_code_evidence"]["symbols"].as_array().unwrap();
        let handler = symbols
            .iter()
            .find(|entry| entry["qualified_name"] == "web.handle_error")
            .unwrap();
        let effects = handler["evidence"].as_array().unwrap();
        assert!(
            effects
                .iter()
                .any(|edge| edge["kind"] == "fastapi_exception_handler")
        );
        assert!(
            effects
                .iter()
                .filter(|edge| edge["kind"] == "fastapi_exception_handler")
                .all(|edge| edge["certainty"] == "possible")
        );
    }

    fs::write(
        project.path().join("fake.py"),
        r#"class Sanic:
    def register_listener(self, listener, event): pass

async def unregistered(application): return None

fake = Sanic()
fake.register_listener(unregistered, 'before_server_start')
"#,
    )
    .unwrap();
    let report = scan(project.path());
    let symbols = report["dead_code_evidence"]["symbols"].as_array().unwrap();
    assert!(!symbols.iter().any(|row| {
        row["qualified_name"] == "fake.unregistered"
            && row["evidence"]
                .as_array()
                .unwrap()
                .iter()
                .any(|edge| edge["kind"] == "sanic_listener")
    }));
}

#[test]
fn fastapi_registration_tracks_ordered_method_replacement() {
    let project = tempfile::tempdir().unwrap();
    fs::write(
        project.path().join("web.py"),
        r#"from fastapi import FastAPI

app = FastAPI()
@app.get('/live')
def live(): return 1

app.get = lambda path: lambda callback: callback
@app.get('/dormant')
def dormant(): return 2

app.add_exception_handler = lambda *args: None
def unused_handler(request, exc): return 3
app.add_exception_handler(Exception, unused_handler)

app.middleware = lambda category: lambda callback: callback
@app.middleware('http')
def unused_middleware(request, call_next): return 4
"#,
    )
    .unwrap();
    let report = scan(project.path());
    assert_eq!(
        findings(&report, "unused_functions"),
        ["web.dormant", "web.unused_handler", "web.unused_middleware"]
    );
}

#[test]
fn fastapi_lookalike_method_returning_identity_lambda_cannot_register() {
    let project = tempfile::tempdir().unwrap();
    for setup in [
        "from fastapi import FastAPI\napp = Fake()\n",
        "from fastapi import FastAPI\nFastAPI = Fake\napp = FastAPI()\n",
        "from fastapi import FastAPI\napp = FastAPI()\napp = Fake()\n",
    ] {
        let source = format!(
            "class Fake:\n    def get(self, path):\n        return lambda fn: fn\n\
             {setup}\
             @app.get('/')\n\
             def endpoint(): return helper()\n\
             def helper(): return 1\n"
        );
        fs::write(project.path().join("web.py"), source).unwrap();
        let report = scan(project.path());
        assert_eq!(
            findings(&report, "unused_functions"),
            ["web.endpoint", "web.helper"]
        );
    }
}

#[test]
fn fastapi_mounted_asgi_callback_does_not_activate_api_dependencies() {
    let project = tempfile::tempdir().unwrap();
    fs::write(
        project.path().join("web.py"),
        r#"from fastapi import Depends, FastAPI

def api_only(): return 1
def side_effect(): return 2
def dormant_effect(): return 3

async def active(scope, receive, send): return side_effect()
async def dormant(scope, receive, send): return dormant_effect()

app = FastAPI(dependencies=[Depends(api_only)])
app.mount('/active', app=active)

def unused_setup():
    app.mount('/dormant', dormant)
"#,
    )
    .unwrap();
    let report = scan(project.path());
    assert_eq!(
        findings(&report, "unused_functions"),
        [
            "web.api_only",
            "web.dormant",
            "web.dormant_effect",
            "web.unused_setup"
        ]
    );
    assert!(
        findings(&report, "unused_parameters")
            .iter()
            .all(|name| !name.starts_with("web.active."))
    );
    let symbols = report["dead_code_evidence"]["symbols"].as_array().unwrap();
    assert!(symbols.iter().any(|row| {
        row["qualified_name"] == "web.active"
            && row["evidence"].as_array().unwrap().iter().any(|edge| {
                edge["kind"] == "fastapi_asgi_application" && edge["certainty"] == "certain"
            })
    }));
}

#[test]
fn fastapi_dependency_uses_the_effective_replacement_binding() {
    let project = tempfile::tempdir().unwrap();
    fs::write(
        project.path().join("web.py"),
        r#"from fastapi import Depends, FastAPI

def replace(callback):
    def replacement(): return 1
    return replacement

def original_helper(): return 1

@replace
def dependency(): return original_helper()

app = FastAPI()
@app.get('/')
def endpoint(value=Depends(dependency)): return 1
"#,
    )
    .unwrap();
    let report = scan(project.path());
    assert_eq!(
        findings(&report, "unused_functions"),
        ["web.dependency", "web.original_helper"]
    );
    assert!(!findings(&report, "unused_parameters").contains(&"web.endpoint.value"));
    let symbols = report["dead_code_evidence"]["symbols"].as_array().unwrap();
    assert!(symbols.iter().any(|row| {
        row["qualified_name"] == "web.replace.replacement"
            && row["evidence"]
                .as_array()
                .unwrap()
                .iter()
                .any(|edge| edge["kind"] == "fastapi_dependency" && edge["certainty"] == "certain")
    }));
}

#[test]
fn fastapi_lifecycle_accepts_proven_noop_replacements() {
    let project = tempfile::tempdir().unwrap();
    fs::write(
        project.path().join("web.py"),
        r#"from fastapi import APIRouter, FastAPI

def erase(callback): return lambda: None
def helper(): return 1

@erase
def startup(): return helper()

router = APIRouter(on_shutdown=[startup])
app = FastAPI(on_startup=[startup])
app.include_router(router)
"#,
    )
    .unwrap();
    let report = scan(project.path());
    assert_eq!(
        findings(&report, "unused_functions"),
        ["web.helper", "web.startup"]
    );
}

#[test]
fn fastapi_stacked_route_decorators_register_the_effective_callback() {
    let project = tempfile::tempdir().unwrap();
    fs::write(
        project.path().join("web.py"),
        r#"from fastapi import Depends, FastAPI

def erase(callback): return lambda: None
def keep(): return 1
def discard(): return 2
def route_dependency(): return 3

outer_app = FastAPI()
inner_app = FastAPI(dependencies=[Depends(route_dependency)])

@erase
@outer_app.get('/outer')
def outer(): return keep()

@inner_app.get('/inner')
@erase
def inner(): return discard()

inner()
"#,
    )
    .unwrap();
    let report = scan(project.path());
    assert_eq!(
        findings(&report, "unused_functions"),
        ["web.discard", "web.inner"]
    );
    let symbols = report["dead_code_evidence"]["symbols"].as_array().unwrap();
    for (name, kind) in [
        ("web.outer", "fastapi_registration"),
        ("web.route_dependency", "fastapi_dependency"),
    ] {
        assert!(
            symbols.iter().any(|row| {
                row["qualified_name"] == name
                    && row["evidence"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|edge| edge["kind"] == kind && edge["certainty"] == "certain")
            }),
            "{name} {report}"
        );
    }
}

#[test]
fn fastapi_literal_registration_wrapper_follows_reached_router_argument() {
    let project = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let file = project.path().join("web.py");
    let source = r#"from fastapi import APIRouter, FastAPI
from external_config import unknown_router

def helper(): return 1
def retired(): return 2

def attach(target):
    @target.get('/')
    def endpoint(): return helper()

router = APIRouter()
app = FastAPI()
"#;
    let mounted = "app.include_router(router)\n";
    fs::write(&file, format!("{source}{mounted}")).unwrap();
    let report = scan(project.path());
    assert_eq!(
        findings(&report, "unused_functions"),
        [
            "web.attach",
            "web.attach.endpoint",
            "web.helper",
            "web.retired"
        ]
    );

    fs::write(&file, format!("{source}attach(router)\n{mounted}")).unwrap();
    let report = scan(project.path());
    assert_eq!(findings(&report, "unused_functions"), ["web.retired"]);
    let mut cached = scan_with_settings(project.path(), None, Some(cache.path()));
    cached.as_object_mut().unwrap().remove("cache");
    assert_eq!(report, cached);

    fs::write(&file, format!("{source}attach(unknown_router)\n{mounted}")).unwrap();
    let report = scan_with_exit(project.path(), None, Some(cache.path()), 2);
    assert!(
        report["analysis_errors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|error| { error["kind"] == "unresolved_registration" && error["line"] == 8 })
    );
}

#[test]
fn fastapi_wildcard_imports_require_known_external_provider_identity() {
    let project = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    for (provider, source) in [
        (
            "fastapi",
            "from fastapi import *\n\
             app = FastAPI()\n\
             def dependency(): return 1\n\
             @app.get('/')\n\
             def endpoint(value=Depends(dependency)): return 1\n",
        ),
        (
            "typing",
            "from typing import *\n\
             from fastapi import Depends, FastAPI\n\
             app = FastAPI()\n\
             def dependency(): return 1\n\
             @app.get('/')\n\
             def endpoint(value: Annotated[int, Depends(dependency)]): return 1\n",
        ),
    ] {
        // This source sorts before the local provider in source discovery.
        fs::write(project.path().join("app.py"), source).unwrap();
        let report = scan(project.path());
        assert!(findings(&report, "unused_imports").is_empty());
        assert!(!findings(&report, "unused_functions").contains(&"app.dependency"));
        assert!(!findings(&report, "unused_parameters").contains(&"app.endpoint.value"));
        let mut cached = scan_with_settings(project.path(), None, Some(cache.path()));
        cached.as_object_mut().unwrap().remove("cache");
        assert_eq!(report, cached);

        let lookalike = project.path().join(format!("{provider}.py"));
        fs::write(&lookalike, "def private(): return 1\n").unwrap();
        let report = scan_with_exit(project.path(), None, Some(cache.path()), 2);
        assert!(
            report["analysis_errors"]
                .as_array()
                .unwrap()
                .iter()
                .any(|error| { error["kind"] == "unsupported_construct" && error["line"] == 1 })
        );
        fs::remove_file(lookalike).unwrap();
    }

    fs::write(
        project.path().join("app.py"),
        "from fastapi import *\napp = FastAPI()\nmetadata = routing\n",
    )
    .unwrap();
    let report = scan_with_exit(project.path(), None, Some(cache.path()), 2);
    assert!(
        report["analysis_errors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|error| { error["kind"] == "unresolved_import" && error["line"] == 3 })
    );
}

#[test]
fn fastapi_literal_route_loops_retain_only_reached_registrations() {
    let project = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    fs::write(
        project.path().join("web.py"),
        r#"from fastapi import Depends, FastAPI

def active_dependency(): return 1
def dormant_dependency(): return 2
def endpoint(session=Depends(active_dependency)): return 1
def dormant(session=Depends(dormant_dependency)): return 2

app = FastAPI()
for path in ('/a', '/b'):
    app.add_api_route(path, endpoint)

def unused_setup():
    for path in ['/c', '/d']:
        app.add_api_route(path, dormant)
"#,
    )
    .unwrap();
    let report = scan(project.path());
    assert_eq!(
        findings(&report, "unused_functions"),
        ["web.dormant", "web.dormant_dependency", "web.unused_setup",]
    );
    assert!(!findings(&report, "unused_parameters").contains(&"web.endpoint.session"));
    let symbols = report["dead_code_evidence"]["symbols"].as_array().unwrap();
    let dependency = symbols
        .iter()
        .find(|row| row["qualified_name"] == "web.active_dependency")
        .unwrap();
    let edges = dependency["evidence"].as_array().unwrap();
    assert!(
        edges
            .iter()
            .any(|edge| edge["kind"] == "fastapi_dependency")
    );
    assert!(
        edges
            .iter()
            .filter(|edge| edge["kind"] == "fastapi_dependency")
            .all(|edge| edge["certainty"] == "possible")
    );
    for _ in 0..2 {
        let cached = scan_with_settings(project.path(), None, Some(cache.path()));
        for key in [
            "unused_functions",
            "unused_parameters",
            "dead_code_evidence",
            "coverage",
        ] {
            assert_eq!(report[key], cached[key], "{key}");
        }
    }
}

#[test]
fn fastapi_route_in_empty_assigned_comprehension_is_dormant() {
    let project = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let file = project.path().join("web.py");
    let source = r#"from fastapi import FastAPI

def endpoint(): return helper()
def helper(): return 1

app = FastAPI()
registrations = [app.add_api_route('/', endpoint) for item in [] for extra in opaque()]
"#;
    fs::write(&file, source).unwrap();
    let report = scan(project.path());
    assert_eq!(
        findings(&report, "unused_functions"),
        ["web.endpoint", "web.helper"]
    );
    let mut cached = scan_with_settings(project.path(), None, Some(cache.path()));
    cached.as_object_mut().unwrap().remove("cache");
    assert_eq!(report, cached);

    fs::write(&file, source.replace("in []", "in [1]")).unwrap();
    let report = scan_with_exit(project.path(), None, Some(cache.path()), 2);
    assert_eq!(report["analysis_errors"][0]["kind"], "unresolved_call");
    assert_eq!(report["analysis_errors"][0]["file"], "web.py");
    assert_eq!(report["analysis_errors"][0]["line"], 7);
}

#[test]
fn conventional_pytest_hooks_have_possible_use_without_discovery() {
    let project = tempfile::tempdir().unwrap();
    for (path, source) in [
        (
            "plugin.py",
            "import pytest as pt\n\
             def pytest_assertrepr_compare(config, op, left, right): return explanation()\n\
             def explanation(): return []\n\
             def pytest_unknown(config): return None\n\
             def pytest_cmdline_main(not_config): return 0\n\
             class FixturesPlugin:\n    def pytest_collection_finish(self, session): return None\n\
             class Ordinary:\n    def pytest_collection_finish(self, session): return None\n",
        ),
        (
            "ordinary.py",
            "import pytest\ndef pytest_addhooks(pluginmanager): return None\n",
        ),
        (
            "active_plugin.py",
            "import pytest\ndef pytest_addhooks(pluginmanager): return None\n",
        ),
        ("test_driver.py", "pytest_plugins = ['active_plugin']\n"),
    ] {
        fs::write(project.path().join(path), source).unwrap();
    }

    let report = scan(project.path());
    let unused = unreachable_functions(&report);
    for expected in [
        "ordinary.pytest_addhooks",
        "plugin.Ordinary.pytest_collection_finish",
        "plugin.pytest_cmdline_main",
        "plugin.pytest_unknown",
    ] {
        assert!(unused.contains(&expected), "{unused:?}");
    }
    let parameters = findings(&report, "unused_parameters");
    let symbols = report["dead_code_evidence"]["symbols"].as_array().unwrap();
    for (name, kind, certainty, arguments) in [
        (
            "plugin.pytest_assertrepr_compare",
            "pytest_hook_candidate",
            "possible",
            &[
                "plugin.pytest_assertrepr_compare.config",
                "plugin.pytest_assertrepr_compare.op",
                "plugin.pytest_assertrepr_compare.left",
                "plugin.pytest_assertrepr_compare.right",
            ][..],
        ),
        (
            "plugin.FixturesPlugin.pytest_collection_finish",
            "pytest_hook_candidate",
            "possible",
            &["plugin.FixturesPlugin.pytest_collection_finish.session"][..],
        ),
        ("plugin.explanation", "direct_call", "possible", &[][..]),
        (
            "active_plugin.pytest_addhooks",
            "pytest_hook",
            "certain",
            &["active_plugin.pytest_addhooks.pluginmanager"][..],
        ),
    ] {
        assert!(!unused.contains(&name), "{unused:?}");
        assert!(
            arguments
                .iter()
                .all(|argument| !parameters.contains(argument))
        );
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

    fs::write(project.path().join("pytest.py"), "").unwrap();
    let report = scan(project.path());
    assert!(unreachable_functions(&report).contains(&"plugin.pytest_assertrepr_compare"));
}

#[test]
fn django_admin_callbacks_need_an_effective_option_and_a_loaded_module() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    let cache = tempfile::tempdir().unwrap();
    fs::create_dir(root.join("shop")).unwrap();
    for (path, source) in [
        ("shop/__init__.py", ""),
        (
            "shop/models.py",
            "from django.db.models import Model\nclass Product(Model):\n    def display_name(self): return 'product'\n    def old_label(self): return ''\n    def __str__(self): return model_label()\ndef model_label(): return 'model'\n",
        ),
        (
            "shop/views.py",
            "from django.views.generic import DetailView\nclass ProductView(DetailView):\n    def setup(self, request): return 1\n",
        ),
        (
            "manage.py",
            "import os\nfrom django.core.management import execute_from_command_line\nif __name__ == '__main__':\n    os.environ.setdefault('DJANGO_SETTINGS_MODULE', 'settings')\n    execute_from_command_line([])\n",
        ),
        (
            "shop/admin.py",
            r#"from django.contrib import admin
from .models import Product

@admin.register(Product)
class ProductAdmin(admin.ModelAdmin):
    list_display = ("old_display",)
    list_display = ("tax_display", "display_name", "__str__")

    @admin.display(description="Tax")
    def tax_display(self, obj):
        return format_tax()

    @admin.display(description="Old tax")
    def old_display(self, obj):
        return 2

    def __str__(self):
        return admin_label()

class OtherAdmin(admin.ModelAdmin):
    list_display = ("tax_display",)
    def tax_display(self, obj):
        return 3

def format_tax():
    return 1

def admin_label():
    return "admin"
"#,
        ),
    ] {
        fs::write(root.join(path), source).unwrap();
    }
    for (app, import, active) in [
        ("'django.contrib.admin',", "", true),
        ("'django.contrib.admin.apps.SimpleAdminConfig',", "", false),
        ("", "import shop.admin\n", true),
        ("", "", false),
    ] {
        fs::write(
            root.join("settings.py"),
            format!("INSTALLED_APPS = ['shop', {app}]\nROOT_URLCONF = 'urls'\n"),
        )
        .unwrap();
        fs::write(root.join("urls.py"), format!("{import}from django.urls import path\nfrom shop.views import ProductView\nurlpatterns = [path('', ProductView.as_view())]\n")).unwrap();
        let serial = scan(root);
        let parallel = scan_with_settings(root, None, Some(cache.path()));
        let cached = scan_with_settings(root, None, Some(cache.path()));
        for key in ["coverage", "dead_code_evidence", "unused_functions"] {
            assert_eq!(serial[key], parallel[key]);
            assert_eq!(serial[key], cached[key]);
        }
        let unused = unreachable_functions(&serial);
        assert_eq!(
            unused.contains(&"shop.admin.ProductAdmin.tax_display"),
            !active
        );
        assert_eq!(unused.contains(&"shop.admin.format_tax"), !active);
        assert!(unused.contains(&"shop.admin.ProductAdmin.old_display"));
        assert!(unused.contains(&"shop.admin.OtherAdmin.tax_display"));
        assert_eq!(
            unused.contains(&"shop.models.Product.display_name"),
            !active
        );
        assert!(unused.contains(&"shop.models.Product.old_label"));
        assert_eq!(unused.contains(&"shop.models.model_label"), !active);
        assert!(unused.contains(&"shop.admin.admin_label"));
        assert!(!unused.contains(&"shop.views.ProductView.setup"));
    }

    let admin_path = root.join("shop/admin.py");
    let source = fs::read_to_string(&admin_path).unwrap();
    fs::write(
        root.join("settings.py"),
        "INSTALLED_APPS = ['shop', 'django.contrib.admin']\nROOT_URLCONF = 'urls'\n",
    )
    .unwrap();
    let imperative = source.replace("@admin.register(Product)\n", "")
        + "\nadmin.site.register(Product, ProductAdmin)\n";
    fs::write(&admin_path, &imperative).unwrap();
    let report = scan(root);
    assert!(!unreachable_functions(&report).contains(&"shop.admin.ProductAdmin.tax_display"));

    let getter = imperative.replace(
        "    def __str__(self):\n        return admin_label()",
        "    def get_list_display(self, request):\n        return ('old_display',)\n\n    def __str__(self):\n        return admin_label()",
    );
    fs::write(&admin_path, getter).unwrap();
    let report = scan(root);
    let unused = unreachable_functions(&report);
    assert!(unused.contains(&"shop.admin.ProductAdmin.tax_display"));
    assert!(!unused.contains(&"shop.admin.ProductAdmin.old_display"));

    fs::write(
        &admin_path,
        imperative + "admin.site.register(Product, OtherAdmin)\n",
    )
    .unwrap();
    let duplicate = scan_with_exit(root, None, None, 2);
    assert!(
        duplicate["analysis_errors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|issue| {
                issue["message"]
                    .as_str()
                    .unwrap()
                    .contains("already registered on the default admin site")
            })
    );
    assert!(
        !duplicate["dead_code_evidence"]["symbols"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| { row["qualified_name"] == "shop.admin.OtherAdmin.tax_display" })
    );
}

#[test]
fn flask_context_and_imperative_hooks_follow_their_registration_owners() {
    let project = tempfile::tempdir().unwrap();
    fs::write(
        project.path().join("providers.py"),
        "from flask import Flask, Blueprint\napp = Flask(__name__)\nactive = Blueprint('active', __name__)\ndormant = Blueprint('dormant', __name__)\n",
    )
    .unwrap();
    fs::write(
        project.path().join("web.py"),
        r#"from providers import app, active, dormant

@app.shell_context_processor
def shell_context(): return {"value": helper()}

def helper(): return 1
def close_db(error): return None
def handle_error(error): return None
def prepare(): return None
def dormant_hook(): return None
def retired(): return 2

class Resource:
    def close(self, error): return self.release()

class ActiveResource(Resource):
    def release(self): return 1

class DormantResource(Resource):
    def release(self): return 2

app.teardown_appcontext(f=close_db)
app.teardown_appcontext(ActiveResource().close)
app.register_error_handler(Exception, handle_error)
active.before_app_request(prepare)
dormant.before_app_request(dormant_hook)
app.register_blueprint(active)

def dormant_setup():
    app.teardown_appcontext(DormantResource().close)
"#,
    )
    .unwrap();

    let cache = tempfile::tempdir().unwrap();
    let report = scan(project.path());
    assert_eq!(
        unreachable_functions(&report),
        [
            "web.DormantResource.release",
            "web.dormant_hook",
            "web.dormant_setup",
            "web.retired"
        ]
    );
    let symbols = report["dead_code_evidence"]["symbols"].as_array().unwrap();
    for callback in [
        "web.shell_context",
        "web.close_db",
        "web.handle_error",
        "web.prepare",
    ] {
        assert!(
            symbols.iter().any(|symbol| {
                symbol["qualified_name"] == callback
                    && symbol["evidence"].as_array().unwrap().iter().any(|entry| {
                        entry["kind"] == "flask_registration" && entry["certainty"] == "certain"
                    })
            }),
            "missing registration evidence for {callback}: {report}"
        );
    }
    assert!(!findings(&report, "unused_parameters").contains(&"web.close_db.error"));
    let cached = scan_with_settings(project.path(), None, Some(cache.path()));
    let warm = scan_with_settings(project.path(), None, Some(cache.path()));
    for key in [
        "unused_functions",
        "unused_parameters",
        "dead_code_evidence",
        "coverage",
    ] {
        assert_eq!(report[key], cached[key]);
        assert_eq!(report[key], warm[key]);
    }
}

#[test]
fn fastapi_lifespan_executes_only_for_active_owners() {
    let project = tempfile::tempdir().unwrap();
    fs::write(
        project.path().join("web.py"),
        r#"from contextlib import asynccontextmanager as manager
from fastapi import APIRouter, FastAPI

def open_resources(): return 1
def close_resources(): return 1
def replaced(): return 1
def legacy(): return 1

@manager
async def main_lifespan(app, obsolete=None):
    open_resources()
    yield
    close_resources()

@manager
async def mounted_lifespan(app):
    yield

@manager
async def dormant_lifespan(app):
    yield

app = FastAPI(lifespan=main_lifespan, on_startup=[replaced])
legacy_app = FastAPI(lifespan=None, on_shutdown=[legacy])
active = APIRouter(lifespan=mounted_lifespan)
dormant = APIRouter(lifespan=dormant_lifespan)
app.include_router(active)

@app.on_event('startup')
def replaced_event(): return 1
"#,
    )
    .unwrap();
    let report = scan(project.path());
    assert_eq!(
        findings(&report, "unused_functions"),
        ["web.dormant_lifespan", "web.replaced", "web.replaced_event"]
    );
    let parameters = findings(&report, "unused_parameters");
    assert!(parameters.contains(&"web.main_lifespan.obsolete"));
    assert!(!parameters.contains(&"web.main_lifespan.app"));
    assert!(!parameters.contains(&"web.mounted_lifespan.app"));
    let symbols = report["dead_code_evidence"]["symbols"].as_array().unwrap();
    for callback in ["web.main_lifespan", "web.mounted_lifespan"] {
        assert!(
            symbols.iter().any(|symbol| {
                symbol["qualified_name"] == callback
                    && symbol["evidence"].as_array().unwrap().iter().any(|entry| {
                        entry["kind"] == "fastapi_lifespan" && entry["certainty"] == "certain"
                    })
            }),
            "missing lifespan evidence for {callback}: {report}"
        );
    }
}

#[test]
fn fastapi_router_constructor_hooks_follow_inclusion_and_merged_lifespans() {
    let project = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let file = project.path().join("web.py");
    for app_custom in [false, true] {
        for router_custom in [false, true] {
            for inclusion in [
                "",
                "app.include_router(router)\n",
                "if os.getenv('MOUNT'):\n    app.include_router(router)\n",
            ] {
                let app_lifespan = if app_custom { "custom" } else { "None" };
                let router_lifespan = if router_custom { "custom" } else { "None" };
                fs::write(
                    &file,
                    format!(
                        r#"import os
from contextlib import asynccontextmanager
from fastapi import APIRouter, FastAPI

def setup(): return 1
def cleanup(): return 2
def start(): return setup()
async def stop(): return cleanup()
starts = (start,)

@asynccontextmanager
async def custom(app):
    yield

router = APIRouter(lifespan={router_lifespan}, on_startup=starts, on_shutdown=[stop])
app = FastAPI(lifespan={app_lifespan})
{inclusion}"#
                    ),
                )
                .unwrap();
                let report = scan(project.path());
                let mut cached = scan_with_settings(project.path(), None, Some(cache.path()));
                cached.as_object_mut().unwrap().remove("cache");
                assert_eq!(cached, report);
                let live = !inclusion.is_empty() && !(app_custom && router_custom);
                let certainty = if inclusion.starts_with("if") {
                    "possible"
                } else {
                    "certain"
                };
                for name in ["web.start", "web.stop", "web.setup", "web.cleanup"] {
                    assert_eq!(
                        !findings(&report, "unused_functions").contains(&name),
                        live,
                        "{name} {report}"
                    );
                }
                for name in ["web.start", "web.stop"] {
                    let symbols = report["dead_code_evidence"]["symbols"].as_array().unwrap();
                    let lifecycle = symbols
                        .iter()
                        .find(|entry| entry["qualified_name"] == name)
                        .into_iter()
                        .flat_map(|entry| entry["evidence"].as_array().unwrap())
                        .find(|entry| entry["kind"] == "fastapi_lifecycle");
                    assert_eq!(lifecycle.is_some(), live, "{name} {report}");
                    if let Some(lifecycle) = lifecycle {
                        assert_eq!(lifecycle["certainty"], certainty);
                    }
                }
            }
        }
    }
}

#[test]
fn fastapi_unresolved_router_constructor_hooks_require_reached_inclusion() {
    let project = tempfile::tempdir().unwrap();
    let file = project.path().join("web.py");
    let source = "from external_config import hooks\n\
                  from fastapi import APIRouter, FastAPI\n\
                  router = APIRouter(on_startup=hooks)\n\
                  app = FastAPI()\n";
    fs::write(&file, source).unwrap();
    scan(project.path());

    fs::write(&file, format!("{source}app.include_router(router)\n")).unwrap();
    let report = scan_with_exit(project.path(), None, None, 2);
    assert!(
        report["analysis_errors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|error| { error["kind"] == "unresolved_registration" && error["line"] == 3 })
    );
}

#[test]
fn fastapi_app_dependencies_need_an_api_route_instead_of_lifecycle_hooks() {
    let project = tempfile::tempdir().unwrap();
    let file = project.path().join("web.py");
    let base = r#"from fastapi import APIRouter, Depends, FastAPI

def guard(): return 1
def startup(): return None
def handle_error(request, error): return None

app = FastAPI(dependencies=[Depends(guard)], on_startup=[startup])
app.add_exception_handler(Exception, handle_error)
router = APIRouter()
app.include_router(router)
"#;
    fs::write(&file, base).unwrap();
    let inactive = scan(project.path());
    assert_eq!(findings(&inactive, "unused_functions"), ["web.guard"]);
    let mounted = base.replace(
        "app.include_router(router)",
        "@router.get('/')\ndef index(): return 1\n\napp.include_router(router)",
    );
    fs::write(file, mounted).unwrap();
    let active = scan(project.path());
    assert!(findings(&active, "unused_functions").is_empty());
}

#[test]
fn fastapi_middleware_preserves_its_supplied_parameters() {
    let project = tempfile::tempdir().unwrap();
    fs::write(
        project.path().join("web.py"),
        r#"from fastapi import FastAPI
from fastapi.middleware import Middleware
from starlette.middleware.base import BaseHTTPMiddleware

def configured(request, call_next, obsolete=None): return 1
def imperative(request, call_next): return 1
app = FastAPI(middleware=[Middleware(BaseHTTPMiddleware, dispatch=configured)])
app.add_middleware(BaseHTTPMiddleware, dispatch=imperative)

@app.middleware('http')
async def decorated(request, call_next): return 1
"#,
    )
    .unwrap();
    let report = scan(project.path());
    assert!(findings(&report, "unused_functions").is_empty());
    assert_eq!(
        findings(&report, "unused_parameters"),
        ["web.configured.obsolete"]
    );
}

#[test]
fn resolved_sqlmodel_and_declarative_base_consume_model_fields() {
    let project = tempfile::tempdir().unwrap();
    fs::write(
        project.path().join("base.py"),
        "from sqlalchemy.orm import declarative_base as make_base\nBase = make_base()\n",
    )
    .unwrap();
    fs::write(
        project.path().join("models.py"),
        r#"from base import Base
from sqlmodel import Field, Relationship, SQLModel
from sqlalchemy import Column, Integer
from sqlalchemy.orm import relationship

class CommonFields(SQLModel):
    name: str = Field(min_length=1)

class Item(CommonFields, table=True):
    owner: "Owner" = Relationship(back_populates='items')

class Owner(SQLModel, table=True):
    items: list[Item] = Relationship(back_populates='owner')

class Ordinary:
    unused = 1

class MetadataOnly(SQLModel, table=True):
    id: int = Field(primary_key=True)

class Payload(SQLModel, table=False):
    pass

class TableSubclass(MetadataOnly, table=True):
    pass

def dormant_setup():
    class Dormant(SQLModel, table=True):
        pass

class Linked(Base):
    id = Column(Integer, primary_key=True)
    children = relationship('Child')

print(Item, Owner, Ordinary, Linked)
"#,
    )
    .unwrap();
    let report = scan(project.path());
    assert_eq!(
        findings(&report, "unused_variables"),
        ["models.Ordinary.unused"]
    );
    assert_eq!(
        findings(&report, "unused_classes"),
        [
            "models.Payload",
            "models.TableSubclass",
            "models.dormant_setup.Dormant",
        ]
    );
}

#[test]
fn celery_backends_follow_effective_settings_and_the_first_load() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    fs::write(
        root.join("providers.py"),
        "class Selected: pass\nclass Replacement: pass\nclass Dormant: pass\nclass Substitute: pass\nAlias = Selected\nSelected = Substitute\n",
    )
    .unwrap();
    let app = root.join("jobs.py");
    let first = r#"from celery import Celery

app = Celery('jobs', backend='providers:Alias')
config = app.conf
config.update({'task_serializer': 'json'})
app.backend
config['result_backend'] = 'providers:Replacement'

other = Celery('other')
other.conf.result_backend = 'providers:Dormant'
del other.conf['result_backend']
"#;
    fs::write(&app, first).unwrap();
    let report = scan(root);
    assert_eq!(
        findings(&report, "unused_classes"),
        ["providers.Dormant", "providers.Replacement"],
        "{report}"
    );
    let assert_backend = |report: &Value, expected: &str| {
        assert!(
            report["dead_code_evidence"]["symbols"]
                .as_array()
                .unwrap()
                .iter()
                .any(|row| row["qualified_name"] == expected
                    && row["evidence"].as_array().unwrap().iter().any(|edge| {
                        edge["kind"] == "celery_backend" && edge["certainty"] == "certain"
                    })),
            "{report}"
        );
    };
    assert_backend(&report, "providers.Selected");
    let cache = tempfile::tempdir().unwrap();
    for _ in 0..2 {
        let mut cached = scan_with_settings(root, None, Some(cache.path()));
        cached.as_object_mut().unwrap().remove("cache");
        assert_eq!(cached, report);
    }

    fs::write(
        app,
        first.replace("app.backend\n", "False and app.backend\n"),
    )
    .unwrap();
    let changed = scan(root);
    assert_eq!(
        findings(&changed, "unused_classes"),
        ["providers.Dormant"],
        "{changed}"
    );
    assert_backend(&changed, "providers.Replacement");
    assert!(
        changed["dead_code_evidence"]["symbols"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|row| row["qualified_name"] == "providers.Selected")
            .flat_map(|row| row["evidence"].as_array().unwrap())
            .all(|edge| edge["kind"] != "celery_backend"),
        "{changed}"
    );
    let mut cached = scan_with_settings(root, None, Some(cache.path()));
    cached.as_object_mut().unwrap().remove("cache");
    assert_eq!(cached, changed);
}

#[test]
fn celery_dormant_lambda_does_not_load_the_configured_backend() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    fs::write(root.join("providers.py"), "class Backend: pass\n").unwrap();
    let path = root.join("jobs.py");
    let source = "from celery import Celery\napp = Celery('jobs', backend='providers:Backend')\npending = lambda: app.backend\napp.conf.result_backend = 'rpc://'\n";
    fs::write(&path, source).unwrap();
    let baseline = scan(root);
    assert_eq!(
        findings(&baseline, "unused_classes"),
        ["providers.Backend"],
        "{baseline}"
    );
    let cache = tempfile::tempdir().unwrap();
    for _ in 0..2 {
        let mut cached = scan_with_settings(root, None, Some(cache.path()));
        cached.as_object_mut().unwrap().remove("cache");
        assert_eq!(cached, baseline);
    }

    fs::write(
        path,
        source.replace(
            "app.conf.result_backend",
            "pending()\napp.conf.result_backend",
        ),
    )
    .unwrap();
    let incomplete = scan_with_exit(root, None, Some(cache.path()), 2);
    assert!(
        incomplete["coverage"]["issues"]
            .as_array()
            .unwrap()
            .iter()
            .any(|issue| issue["kind"] == "unresolved_lambda_execution"),
        "{incomplete}"
    );
}

#[test]
fn celery_backend_lookup_respects_deleted_exports_and_captured_aliases() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    let providers = root.join("providers.py");
    let jobs = root.join("jobs.py");
    fs::write(&providers, "class Backend: pass\ndel Backend\n").unwrap();
    fs::write(
        &jobs,
        "from celery import Celery\napp = Celery('jobs', backend='providers:Backend')\n",
    )
    .unwrap();
    let report = scan(root);
    assert_eq!(
        findings(&report, "unused_classes"),
        ["providers.Backend"],
        "{report}"
    );

    fs::write(
        &providers,
        "class Backend: pass\nSelected = Backend\ndel Backend\n",
    )
    .unwrap();
    fs::write(
        jobs,
        "from celery import Celery\napp = Celery('jobs', backend='providers:Selected')\n",
    )
    .unwrap();
    let report = scan(root);
    assert!(
        report["dead_code_evidence"]["symbols"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| {
                row["qualified_name"] == "providers.Backend"
                    && row["evidence"].as_array().unwrap().iter().any(|edge| {
                        edge["kind"] == "celery_backend" && edge["certainty"] == "certain"
                    })
            }),
        "{report}"
    );

    fs::write(
        providers,
        "class Backend: pass\nSelected = Backend\ndel Backend\nBackend()\n",
    )
    .unwrap();
    let incomplete = scan_with_exit(root, None, None, 2);
    assert!(
        incomplete["coverage"]["issues"]
            .as_array()
            .unwrap()
            .iter()
            .any(|issue| issue["kind"] == "unbound_local"),
        "{incomplete}"
    );
}

#[test]
fn celery_backend_reads_follow_comprehension_execution() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    fs::write(
        root.join("providers.py"),
        "class Prior: pass\nclass Current: pass\n",
    )
    .unwrap();
    let path = root.join("jobs.py");
    let write = |expression| {
        fs::write(&path, format!(
            "from celery import Celery\nfrom settings import enabled\nfrom types import SimpleNamespace\napp = Celery('jobs', backend='providers:Prior')\npending = {expression}\napp.conf.result_backend = 'providers:Current'\n"
        )).unwrap();
    };
    let evidence = |report: &Value, name: &str, kind: &str, certainty: &str| {
        report["dead_code_evidence"]["symbols"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|row| row["qualified_name"] == name)
            .flat_map(|row| row["evidence"].as_array().unwrap())
            .any(|edge| edge["kind"] == kind && edge["certainty"] == certainty)
    };

    for (expression, active) in [
        (
            "{key: app.backend for key in ['worker'] if not False}",
            "Prior",
        ),
        ("(key for key in [app.backend])", "Prior"),
        ("(app.backend for key in ['worker'])", "Current"),
        ("[app.backend for key in [1] if []]", "Current"),
        (
            "[key for app in [SimpleNamespace(backend=1)] for key in [app.backend]]",
            "Current",
        ),
    ] {
        write(expression);
        let report = scan(root);
        let inactive = if active == "Prior" {
            "Current"
        } else {
            "Prior"
        };
        assert!(
            evidence(
                &report,
                &format!("providers.{active}"),
                "celery_backend",
                "certain"
            ),
            "{report}"
        );
        assert_eq!(
            findings(&report, "unused_classes"),
            [format!("providers.{inactive}")]
        );
    }

    write("[app.backend for key in [1] if enabled]");
    let report = scan(root);
    assert!(
        evidence(&report, "providers.Prior", "celery_backend", "possible"),
        "{report}"
    );
    assert!(
        !evidence(&report, "providers.Prior", "celery_backend", "certain"),
        "{report}"
    );
    assert!(
        evidence(
            &report,
            "providers.Current",
            "celery_backend_default",
            "certain"
        ),
        "{report}"
    );
    assert!(
        evidence(&report, "providers.Current", "celery_backend", "possible"),
        "{report}"
    );
    assert!(
        !evidence(&report, "providers.Current", "celery_backend", "certain"),
        "{report}"
    );
    let cache = tempfile::tempdir().unwrap();
    for _ in 0..2 {
        let mut cached = scan_with_settings(root, None, Some(cache.path()));
        cached.as_object_mut().unwrap().remove("cache");
        assert_eq!(cached, report);
    }

    write("(app.backend for key in [1])");
    let mut source = fs::read_to_string(&path).unwrap();
    source.push_str("list(pending)\n");
    fs::write(&path, source).unwrap();
    let incomplete = scan_with_exit(root, None, None, 2);
    assert!(
        incomplete["coverage"]["issues"]
            .as_array()
            .unwrap()
            .iter()
            .any(|issue| issue["kind"] == "unresolved_generator_execution"),
        "{incomplete}"
    );

    write("[key for key in [1] if app.backend for app in [SimpleNamespace(backend=1)]]");
    let incomplete = scan_with_exit(root, None, None, 2);
    assert!(
        incomplete["coverage"]["issues"]
            .as_array()
            .unwrap()
            .iter()
            .any(|issue| issue["kind"] == "unbound_local"),
        "{incomplete}"
    );
    assert!(!evidence(
        &incomplete,
        "providers.Prior",
        "celery_backend",
        "certain"
    ));
}

#[test]
fn celery_task_class_needs_a_reached_registration() {
    let project = tempfile::tempdir().unwrap();
    fs::write(
        project.path().join("jobs.py"),
        r#"from celery import Celery, Task

app = Celery('worker')

class Reusable(Task):
    def run(self): return self.perform()

class Registered(Reusable):
    def perform(self): return helper()

class Dormant(Reusable):
    def perform(self): return dormant_helper()

def helper(): return 1
def dormant_helper(): return 2
def dormant_setup(): return app.register_task(Dormant())

app.register_task(Registered())
"#,
    )
    .unwrap();
    let report = scan(project.path());
    assert_eq!(
        unreachable_functions(&report),
        [
            "jobs.Dormant.perform",
            "jobs.dormant_helper",
            "jobs.dormant_setup"
        ]
    );
    let symbols = report["dead_code_evidence"]["symbols"].as_array().unwrap();
    assert!(symbols.iter().any(|symbol| {
        symbol["qualified_name"] == "jobs.Reusable.run"
            && symbol["evidence"]
                .as_array()
                .unwrap()
                .iter()
                .any(|entry| entry["kind"] == "celery_task" && entry["certainty"] == "certain")
    }));
}

#[test]
fn fastapi_annotation_aliases_consume_only_the_selected_dependency_parameters() {
    let project = tempfile::tempdir().unwrap();
    fs::write(
        project.path().join("deps.py"),
        r#"from typing import Annotated
from fastapi import Depends

def authenticate(): return 1
def retired(): return 2
Credential = Annotated[int, Depends(authenticate)]
UnusedType = Annotated[int, Depends(retired)]
"#,
    )
    .unwrap();
    fs::write(
        project.path().join("web.py"),
        r#"from deps import Credential as User, retired
from fastapi import APIRouter, Depends, FastAPI, Query
from typing import Annotated

app = FastAPI()
router = APIRouter()

@app.get('/active')
def endpoint(*, credential: User, plain: bool = False): return 1

@app.get('/query')
def precedence(credential: Annotated[int, Depends(retired), Query()]): return 2

@router.get('/unmounted')
def dormant(credential: User, plain: bool = False): return 3
"#,
    )
    .unwrap();

    let report = scan(project.path());
    assert_eq!(
        findings(&report, "unused_functions"),
        ["deps.retired", "web.dormant"]
    );
    assert_eq!(
        findings(&report, "unused_parameters"),
        [
            "web.dormant.plain",
            "web.endpoint.plain",
            "web.precedence.credential",
        ]
    );
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

def lookup():
    return 3

class Inferred:
    def __init__(self, credential: Annotated[int, Depends(lookup)]):
        pass

class Draft:
    def __init__(self):
        unused_prepare()

app = FastAPI()
dormant = APIRouter()

@app.get("/")
def endpoint(service: Annotated[Service, Depends(Service)]):
    return 1

@app.get("/inferred")
def inferred(service: Annotated[Inferred, Depends()]):
    return 3

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
    let parameters = findings(&report, "unused_parameters");
    assert!(!parameters.contains(&"web.inferred.service"));
    assert!(!parameters.contains(&"web.Inferred.__init__.credential"));
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
fn django_reads_the_explicit_settings_module_and_application_export() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    fs::write(
        root.join("wsgi.py"),
        "import os as environment\nfrom django.core.wsgi import get_wsgi_application as factory\nenvironment.environ['DJANGO_SETTINGS_MODULE'] = 'production'\napplication = factory()\n",
    )
    .unwrap();
    for module in ["production", "development"] {
        fs::write(
            root.join(format!("{module}.py")),
            "DEBUG = True\nhelper = 1\n",
        )
        .unwrap();
    }
    let report = scan_with_settings(root, Some("development"), None);
    assert_eq!(
        findings(&report, "unused_variables"),
        [
            "development.DEBUG",
            "development.helper",
            "production.helper"
        ],
        "{report}"
    );

    fs::write(
        root.join("wsgi.py"),
        "import os as environment\nfrom django.core.wsgi import get_wsgi_application as factory\nenvironment.environ['DJANGO_SETTINGS_MODULE'] = 'production'\napplication = factory()\napplication = None\n",
    )
    .unwrap();
    let report = scan(root);
    assert!(findings(&report, "unused_variables").contains(&"wsgi.application"));
}

#[test]
fn django_error_handlers_follow_only_the_selected_urlconf() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    for (name, source) in [
        (
            "wsgi.py",
            "import django, os\nos.environ['DJANGO_SETTINGS_MODULE'] = 'settings'\ndjango.setup()\n",
        ),
        ("settings.py", "ROOT_URLCONF = 'active_urls'\n"),
        (
            "active_urls.py",
            "from views import forbidden\nurlpatterns = []\nhandler403 = forbidden\nhandler404 = 'views.not_found'\n",
        ),
        (
            "inactive_urls.py",
            "urlpatterns = []\nhandler500 = 'views.server_error'\n",
        ),
        (
            "views.py",
            "def forbidden(request, exception): return None\ndef not_found(request, exception): return None\ndef server_error(request): return None\n",
        ),
    ] {
        fs::write(root.join(name), source).unwrap();
    }
    let report = scan(root);
    assert_eq!(
        findings(&report, "unused_functions"),
        ["views.server_error"],
        "{report}"
    );
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
            unreachable_functions(&report),
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
    assert!(unreachable_functions(&report).is_empty(), "{report}");
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
fn django_autoreload_requires_a_reached_genuine_provider() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    fs::write(
        root.join("startup.py"),
        r#"from django.utils import autoreload

def start():
    return helper()

def helper():
    return 1

def dormant():
    return 2

def unreached():
    autoreload.run_with_reloader(dormant)

def replaced():
    return 3

def noop(callback):
    pass

reload = autoreload.run_with_reloader
autoreload.run_with_reloader = noop
autoreload.run_with_reloader(replaced)
reload(main_func=start)
"#,
    )
    .unwrap();
    let report = scan(root);
    let callbacks = report["dead_code_evidence"]["symbols"].as_array().unwrap();
    let roots: Vec<_> = callbacks
        .iter()
        .filter(|row| {
            row["evidence"].as_array().unwrap().iter().any(|event| {
                event["kind"] == "django_autoreload" && event["certainty"] == "certain"
            })
        })
        .map(|row| row["qualified_name"].as_str().unwrap())
        .collect();
    assert_eq!(roots, ["startup.start"]);
    let unused = findings(&report, "unused_functions");
    assert!(unused.contains(&"startup.dormant"));
    assert!(!unused.contains(&"startup.helper"));
}

#[test]
fn django_autoreload_wrapper_activates_only_bound_callbacks() {
    let project = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let root = project.path();
    fs::write(
        root.join("reloads.py"),
        "from django.utils import autoreload\ndef run(target):\n    autoreload.run_with_reloader(target)\n",
    )
    .unwrap();
    let caller = r#"from reloads import run
from external import enabled, opaque

def selected():
    return helper()

def helper():
    return 1

def optional():
    return 2

def dormant():
    return 3

def abandoned():
    run(dormant)

run(selected)
if enabled:
    run(target=optional)
"#;
    let path = root.join("startup.py");
    fs::write(&path, caller).unwrap();
    let report = scan(root);
    let symbols = report["dead_code_evidence"]["symbols"].as_array().unwrap();
    let callbacks: Vec<_> = symbols
        .iter()
        .flat_map(|symbol| {
            symbol["evidence"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|entry| entry["kind"] == "django_autoreload")
                .map(|entry| {
                    (
                        symbol["qualified_name"].as_str().unwrap(),
                        entry["certainty"].as_str().unwrap(),
                    )
                })
        })
        .collect();
    assert_eq!(
        callbacks,
        [
            ("startup.optional", "possible"),
            ("startup.selected", "certain"),
        ]
    );
    assert!(findings(&report, "unused_functions").contains(&"startup.dormant"));
    assert!(!findings(&report, "unused_functions").contains(&"startup.helper"));
    for _ in 0..2 {
        let cached = scan_with_settings(root, None, Some(cache.path()));
        for field in ["coverage", "dead_code_evidence", "unused_functions"] {
            assert_eq!(report[field], cached[field], "{field}");
        }
    }

    for unresolved in ["run(opaque)", "run(dormant, invalid=True)"] {
        fs::write(&path, format!("{caller}\n{unresolved}\n")).unwrap();
        let report = scan_with_exit(root, None, Some(cache.path()), 2);
        assert!(
            report["coverage"]["issues"]
                .as_array()
                .unwrap()
                .iter()
                .any(|issue| issue["kind"] == "unresolved_registration"
                    && issue["path"] == "reloads.py")
        );
        assert!(
            report["dead_code_evidence"]["symbols"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|symbol| symbol["qualified_name"] == "startup.dormant")
                .all(|symbol| symbol["evidence"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|entry| entry["kind"] != "django_autoreload"))
        );
    }
}

#[test]
fn django_converters_use_proven_regex_and_method_descriptors() {
    let project = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let root = project.path();
    fs::write(
        root.join("converters.py"),
        r#"import builtins as native
from builtins import staticmethod as static
from django.urls import register_converter
from django.urls.converters import StringConverter as Base

class Static(Base):
    @static
    def to_python(value):
        return value.lower()

class Classed(Base):
    @native.classmethod
    def to_python(cls, value):
        return value.lower()

class StringConverter:
    pass

class Lookalike(StringConverter):
    def to_python(self, value):
        return value

register_converter(Static, 'static')
register_converter(Classed, 'classed')
register_converter(Lookalike, 'lookalike')
"#,
    )
    .unwrap();
    let report = scan(root);
    assert_eq!(
        findings(&report, "unused_functions"),
        ["converters.Lookalike.to_python"]
    );
    let registrations = report["dead_code_evidence"]["symbols"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|symbol| {
            let name = symbol["qualified_name"].as_str().unwrap();
            (name.ends_with(".to_python")
                && symbol["evidence"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|event| event["kind"] == "django_path_converter"))
            .then_some(name)
        })
        .collect::<Vec<_>>();
    assert_eq!(
        registrations,
        [
            "converters.Classed.to_python",
            "converters.Static.to_python"
        ]
    );
    for _ in 0..2 {
        let cached = scan_with_settings(root, None, Some(cache.path()));
        for field in ["unused_functions", "coverage", "dead_code_evidence"] {
            assert_eq!(report[field], cached[field], "{field}");
        }
    }
}

#[test]
fn django_converter_wrappers_resolve_reached_arguments_only() {
    let project = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let root = project.path();
    fs::write(
        root.join("configure.py"),
        "from django.urls import register_converter\ndef install(converter):\n    register_converter(converter=converter, type_name='custom')\n",
    )
    .unwrap();
    let src = r#"from configure import install
from external import opaque

class Active:
    regex = r'[a-z]+'
    def to_python(self, value):
        return value.lower()
    def to_url(self, value):
        return value

class Inactive:
    regex = r'[a-z]+'
    def to_python(self, value):
        return value
    def to_url(self, value):
        return value

def dormant():
    install(Inactive)

install(Active)
"#;
    let source = root.join("routes.py");
    fs::write(&source, src).unwrap();
    let report = scan(root);
    assert_eq!(
        unreachable_functions(&report),
        [
            "routes.Inactive.to_python",
            "routes.Inactive.to_url",
            "routes.dormant"
        ]
    );
    assert_eq!(
        findings(&report, "unused_variables"),
        ["routes.Inactive.regex"]
    );
    for _ in 0..2 {
        let cached = scan_with_settings(root, None, Some(cache.path()));
        for field in [
            "coverage",
            "dead_code_evidence",
            "unused_functions",
            "unused_variables",
            "suppressed",
        ] {
            assert_eq!(report[field], cached[field], "{field}");
        }
    }
    fs::write(source, format!("{src}\ninstall(opaque)\n")).unwrap();
    let report = scan_with_exit(root, None, Some(cache.path()), 2);
    assert!(
        report["coverage"]["issues"]
            .as_array()
            .unwrap()
            .iter()
            .any(|issue| {
                issue["kind"] == "unresolved_registration" && issue["path"] == "configure.py"
            })
    );
}

#[test]
fn django_converter_chooses_methods_in_c3_order() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    fs::write(
        root.join("converters.py"),
        r#"from django.urls import register_converter

class Base:
    regex = '[a-z]+'
    def to_python(self, value):
        return old(value)
    def to_url(self, value):
        return value

class Left(Base):
    pass

class Right(Base):
    def to_python(self, value):
        return value.lower()

class Diamond(Left, Right):
    pass

def old(value):
    return value

register_converter(Diamond, 'diamond')
"#,
    )
    .unwrap();
    let report = scan(root);
    assert_eq!(
        findings(&report, "unused_functions"),
        ["converters.Base.to_python", "converters.old"]
    );
    assert!(!findings(&report, "unused_variables").contains(&"converters.Base.regex"));
}

#[test]
fn django_converter_protocol_requires_a_reached_registration() {
    let project = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let root = project.path();
    let src = r#"from django.urls import register_converter

class Converter:
    regex = '[a-z]+'
    def to_python(self, value):
        return parse(value)
    def to_url(self, value):
        return value

class Child(Converter):
    pass

def parse(value):
    return value.lower()

def install():
    register_converter(Child, 'custom')

class Inactive:
    regex = '[0-9]+'
    def to_python(self, value):
        return value
    def to_url(self, value):
        return value

def dormant_install():
    register_converter(Inactive, 'inactive')

instance = Child()
inactive_instance = Inactive()
"#;
    let path = root.join("converters.py");
    fs::write(&path, src).unwrap();
    let dormant = scan_with_settings(root, None, Some(cache.path()));
    assert_eq!(
        findings(&dormant, "unused_functions"),
        [
            "converters.Converter.to_python",
            "converters.Converter.to_url",
            "converters.Inactive.to_python",
            "converters.Inactive.to_url",
            "converters.dormant_install",
            "converters.install",
            "converters.parse",
        ]
    );
    assert!(findings(&dormant, "unused_variables").contains(&"converters.Converter.regex"));

    fs::write(&path, format!("{src}\ninstall()\n")).unwrap();
    let active = scan_with_settings(root, None, Some(cache.path()));
    assert_eq!(
        findings(&active, "unused_functions"),
        [
            "converters.Inactive.to_python",
            "converters.Inactive.to_url",
            "converters.dormant_install",
        ]
    );
    let variables = findings(&active, "unused_variables");
    assert!(!variables.contains(&"converters.Converter.regex"));
    assert!(variables.contains(&"converters.Inactive.regex"));
    let clean = scan(root);
    for field in [
        "coverage",
        "dead_code_evidence",
        "unused_functions",
        "unused_variables",
    ] {
        assert_eq!(clean[field], active[field], "{field}");
    }

    for unknown_caller in ["parse(123)", "instance.to_python(123)"] {
        fs::write(&path, format!("{src}\ninstall()\n{unknown_caller}\n")).unwrap();
        let report = scan_with_exit(root, None, Some(cache.path()), 2);
        assert_eq!(report["coverage"]["status"], "incomplete");
        assert!(
            report["coverage"]["issues"]
                .as_array()
                .unwrap()
                .iter()
                .any(|error| {
                    error["kind"] == "unresolved_call"
                        && error["path"] == "converters.py"
                        && error["line"] == 14
                }),
            "{report}"
        );
    }
}

#[test]
fn django_command_uses_the_effective_handle_after_module_writes() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    let commands = root.join("shop/management/commands");
    fs::create_dir_all(&commands).unwrap();
    for path in ["shop", "shop/management", "shop/management/commands"] {
        fs::write(root.join(path).join("__init__.py"), "").unwrap();
    }
    fs::write(root.join("settings.py"), "INSTALLED_APPS = ['shop']\n").unwrap();
    fs::write(
        root.join("manage.py"),
        "from django.core.management import execute_from_command_line\nexecute_from_command_line(['manage.py', 'audit', '--settings=settings'])\n",
    )
    .unwrap();
    let command = r#"from django.core.management.base import BaseCommand

class Command(BaseCommand):
    def handle(self, *args, **options):
        return retired()

def retired():
    return 1

def current(self, *args, **options):
    return 2
"#;
    for write in [
        "Command.handle = current",
        "setattr(Command, 'handle', current)",
    ] {
        fs::write(commands.join("audit.py"), format!("{command}\n{write}\n")).unwrap();
        let report = scan(root);
        let unused = findings(&report, "unused_functions");
        assert_eq!(
            unused,
            [
                "shop.management.commands.audit.Command.handle",
                "shop.management.commands.audit.retired",
            ]
        );
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
fn pydantic_dump_activates_selected_serializers_and_bounds_the_wrap_delegate() {
    let project = tempfile::tempdir().unwrap();
    fs::write(
        project.path().join("models.py"),
        r#"from pydantic import BaseModel, field_serializer, model_serializer

class User(BaseModel):
    name: str

    @field_serializer('name')
    def serialize_name(self, value):
        return value

    @model_serializer(mode='wrap')
    def serialize_model(self, handler):
        return handler(self)

class Preview(BaseModel):
    name: str

    @field_serializer('name')
    def unused_serializer(self, value):
        return value

class SerializationDraft: pass

def unrelated(): return 1

serialize = User(name='Ada').model_dump
serialize()
Preview(name='Ada')
"#,
    )
    .unwrap();

    let report = scan(project.path());
    assert_eq!(
        findings(&report, "unused_functions"),
        ["models.Preview.unused_serializer", "models.unrelated"],
        "{report}"
    );
    assert_eq!(
        findings(&report, "unused_classes"),
        ["models.SerializationDraft"],
        "{report}"
    );
    for name in ["models.User.serialize_name", "models.User.serialize_model"] {
        assert!(
            report["dead_code_evidence"]["symbols"]
                .as_array()
                .unwrap()
                .iter()
                .any(|row| row["qualified_name"] == name
                    && row["evidence"].as_array().unwrap().iter().any(|edge| {
                        edge["kind"] == "pydantic_serializer" && edge["certainty"] == "certain"
                    })),
            "{report}"
        );
    }
    let cache = tempfile::tempdir().unwrap();
    for _ in 0..2 {
        let mut cached = scan_with_settings(project.path(), None, Some(cache.path()));
        cached.as_object_mut().unwrap().remove("cache");
        assert_eq!(cached, report);
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
fn django_class_receiver_uses_the_binding_at_class_creation() {
    let project = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let root = project.path();
    django_signal_project(
        root,
        r#"from django.db.models.signals import post_save
from django.dispatch import receiver

class Registered:
    @staticmethod
    @receiver(post_save)
    def run(sender, instance, **kwargs):
        return live_helper()

def receiver(signal):
    def decorate(callback):
        return callback
    return decorate

class Inert:
    @staticmethod
    @receiver(post_save)
    def run(sender, instance, **kwargs):
        return unused_helper()

def live_helper():
    return 1

def unused_helper():
    return 2

Registered()
Inert()
"#,
    );
    let report = scan(root);
    assert_eq!(
        django_signal_evidence(&report),
        [("inventory.signals.Registered.run", "certain")]
    );
    assert!(findings(&report, "unused_functions").contains(&"inventory.signals.unused_helper"));
    assert!(!findings(&report, "unused_functions").contains(&"inventory.signals.live_helper"));
    assert!(
        findings(&report, "unused_parameters").contains(&"inventory.signals.Inert.run.instance")
    );
    assert!(
        !findings(&report, "unused_parameters")
            .contains(&"inventory.signals.Registered.run.instance")
    );
    for _ in 0..2 {
        let cached = scan_with_settings(root, None, Some(cache.path()));
        for field in [
            "unused_functions",
            "unused_parameters",
            "dead_code_evidence",
            "coverage",
            "definitions",
        ] {
            assert_eq!(report[field], cached[field], "{field}");
        }
    }
}

#[test]
fn django_captured_receiver_survives_an_opaque_provider_assignment() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    let src = r#"import django.dispatch as dispatch
from django.dispatch import receiver
from django.db.models.signals import post_save
captured = receiver
dispatch.receiver = replacement
import os

@captured(post_save)
def registered(sender, instance, **kwargs):
    return helper()

def helper():
    return 1
"#;
    django_signal_project(root, src);
    let report = scan(root);
    assert_eq!(
        django_signal_evidence(&report),
        [("inventory.signals.registered", "certain")]
    );
    assert!(!findings(&report, "unused_functions").contains(&"inventory.signals.helper"));
    let second = r#"
@dispatch.receiver(post_save)
def opaque(sender, instance, **kwargs):
    pass
"#;
    fs::write(root.join("inventory/signals.py"), format!("{src}{second}")).unwrap();
    let report = scan_with_exit(root, None, None, 2);
    assert_eq!(report["coverage"]["status"], "incomplete");
    assert_eq!(
        django_signal_evidence(&report),
        [("inventory.signals.registered", "certain")]
    );
}

#[test]
fn django_short_circuit_guards_preserve_signal_certainty() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    django_signal_project(
        root,
        r#"from django.db.models.signals import post_save
from external import enabled

def certain(sender, instance, **kwargs):
    return 1

def optional_and(sender, instance, **kwargs):
    return 2

def optional_or(sender, instance, **kwargs):
    return 3

def inactive(sender, instance, **kwargs):
    return 4

True and post_save.connect(certain)
enabled and post_save.connect(optional_and)
enabled or post_save.connect(optional_or)
False and post_save.connect(inactive)
"#,
    );
    let report = scan(root);
    assert_eq!(
        django_signal_evidence(&report),
        [
            ("inventory.signals.certain", "certain"),
            ("inventory.signals.optional_and", "possible"),
            ("inventory.signals.optional_or", "possible"),
        ]
    );
    assert_eq!(
        findings(&report, "unused_functions"),
        ["inventory.signals.inactive"]
    );
}

#[test]
fn django_signals_keep_coverage_for_proven_object_base_factories() {
    let project = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let root = project.path();
    let src = r#"from django.dispatch import receiver
from django.db.models.signals import post_save
from external import dynamic_base

def base_factory():
    return object

class Other(base_factory()):
    pass

instance = Other()

@receiver(post_save)
def registered(sender, instance, **kwargs):
    return helper()

def helper():
    return 1
"#;
    django_signal_project(root, src);
    let report = scan(root);
    assert_eq!(
        django_signal_evidence(&report),
        [("inventory.signals.registered", "certain")]
    );
    assert!(!findings(&report, "unused_functions").contains(&"inventory.signals.helper"));
    for _ in 0..2 {
        let cached = scan_with_settings(root, None, Some(cache.path()));
        for field in ["coverage", "dead_code_evidence", "unused_functions"] {
            assert_eq!(report[field], cached[field], "{field}");
        }
    }

    fs::write(
        root.join("inventory/signals.py"),
        src.replace("    return object", "    return dynamic_base"),
    )
    .unwrap();
    let report = scan_with_exit(root, None, Some(cache.path()), 2);
    assert_eq!(report["coverage"]["status"], "incomplete");
    assert_eq!(
        django_signal_evidence(&report),
        [("inventory.signals.registered", "certain")]
    );
}

#[test]
fn django_receiver_decorator_factory_can_be_a_local_identity() {
    let project = tempfile::tempdir().unwrap();
    django_signal_project(
        project.path(),
        r#"from django.db.models.signals import post_save
from django.dispatch import receiver as listen

receiver = lambda signal: (lambda callback: callback)

@receiver(post_save)
def unused(sender, instance, **kwargs):
    return unused_helper()

@listen(post_save)
def active(sender, instance, **kwargs):
    return 1

def unused_helper():
    return 2
"#,
    );
    let report = scan(project.path());
    assert_eq!(
        django_signal_evidence(&report),
        [("inventory.signals.active", "certain")]
    );
    assert_eq!(
        findings(&report, "unused_functions"),
        [
            "inventory.signals.unused",
            "inventory.signals.unused_helper"
        ],
    );
}

#[test]
fn django_configured_project_proves_loaded_signals_without_a_launcher() {
    let project = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let root = project.path();
    django_signal_project(
        root,
        "from django.db.models.signals import post_save\nfrom django.dispatch import receiver\n@receiver(post_save)\ndef refresh(sender, instance, **kwargs):\n    return helper()\ndef helper():\n    return 1\ndef stale():\n    return 2\n",
    );
    fs::remove_file(root.join("manage.py")).unwrap();
    fs::remove_file(root.join("settings.py")).unwrap();
    fs::create_dir(root.join("project")).unwrap();
    for (path, source) in [
        ("project/__init__.py", ""),
        (
            "project/settings.py",
            "INSTALLED_APPS = ['inventory.apps.InventoryConfig']\nROOT_URLCONF = 'project.urls'\n",
        ),
        (
            "project/urls.py",
            "from django.urls import path\nurlpatterns = []\n",
        ),
    ] {
        fs::write(root.join(path), source).unwrap();
    }
    let report = scan(root);
    assert_eq!(
        django_signal_evidence(&report),
        [("inventory.signals.refresh", "certain")],
        "{report}"
    );
    assert_eq!(unreachable_functions(&report), ["inventory.signals.stale"]);

    let launcher_path = root.join("deployment.py");
    let launcher = "import os\nfrom django import setup\ndef boot():\n    os.environ['DJANGO_SETTINGS_MODULE'] = 'other_settings'\n    setup()\n";
    fs::write(root.join("other_settings.py"), "INSTALLED_APPS = []\n").unwrap();
    fs::write(&launcher_path, launcher).unwrap();
    let dormant_launcher = scan(root);
    assert_eq!(
        django_signal_evidence(&dormant_launcher),
        django_signal_evidence(&report)
    );

    fs::write(&launcher_path, format!("{launcher}boot()\n")).unwrap();
    let explicit_launcher = scan(root);
    assert!(
        django_signal_evidence(&explicit_launcher).is_empty(),
        "{explicit_launcher}"
    );
    assert!(
        findings(&explicit_launcher, "unused_functions").contains(&"inventory.signals.refresh")
    );
    fs::remove_file(launcher_path).unwrap();
    fs::remove_file(root.join("other_settings.py")).unwrap();

    for _ in 0..2 {
        let cached = scan_with_settings(root, None, Some(cache.path()));
        for key in [
            "unused_functions",
            "unused_parameters",
            "dead_code_evidence",
            "coverage",
        ] {
            assert_eq!(report[key], cached[key], "{key}");
        }
    }

    let config_path = root.join("inventory/apps.py");
    let config = fs::read_to_string(&config_path).unwrap();
    for replacement in [
        "InventoryConfig.ready = lambda self: None\n",
        "setattr(InventoryConfig, 'ready', lambda self: None)\n",
        "del InventoryConfig.ready\n",
    ] {
        fs::write(&config_path, format!("{config}{replacement}")).unwrap();
        let report = scan(root);
        assert!(django_signal_evidence(&report).is_empty(), "{report}");
        assert!(!unreachable_functions(&report).contains(&"inventory.signals.helper"));
    }
    fs::write(config_path, config).unwrap();

    fs::write(
        root.join("project/settings.py"),
        "INSTALLED_APPS = []\nROOT_URLCONF = 'project.urls'\n",
    )
    .unwrap();
    let report = scan(root);
    assert!(django_signal_evidence(&report).is_empty());
    assert_eq!(
        unreachable_functions(&report),
        [
            "inventory.apps.InventoryConfig.ready",
            "inventory.signals.stale"
        ]
    );
    assert!(
        report["dead_code_evidence"]["symbols"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| {
                row["qualified_name"] == "inventory.signals.refresh"
                    && row["evidence"].as_array().unwrap().iter().any(|event| {
                        event["kind"] == "django_signal_unknown_loading"
                            && event["certainty"] == "possible"
                    })
            }),
        "{report}"
    );

    fs::write(
        root.join("project/urls.py"),
        "def path(route, view): return view\nurlpatterns = []\n",
    )
    .unwrap();
    let lookalike = scan_with_exit(root, None, None, 2);
    assert!(django_signal_evidence(&lookalike).is_empty());
    assert_eq!(
        lookalike["analysis_errors"][0]["message"],
        "Django signal registration has no selected startup"
    );
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

replaced = Event()
replaced.connect = lambda callback: None

@listen(replaced)
def replaced_receiver(sender, **kwargs):
    return 10

def replaced_connection(sender, **kwargs):
    return 11

replaced.connect(replaced_connection)

def never_iterated(sender, **kwargs):
    return 12

(post_save.connect(never_iterated) for value in [1])
[post_save.connect(never_iterated) for value in []]

def deferred_install():
    post_save.connect(never_iterated)
    yield None

deferred_install()
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
        "inventory.signals.replaced_receiver",
        "inventory.signals.replaced_connection",
        "inventory.signals.never_iterated",
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
            ("inventory.signals.after_change", "certain"),
            ("inventory.signals.conditional_receiver", "possible"),
            ("inventory.signals.on_change", "certain"),
            ("inventory.signals.on_inventory", "certain"),
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
fn django_preserves_opaque_signal_diagnostics_when_loaded() {
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
    assert_eq!(issues.len(), 1, "{report}");
    assert_eq!(issues[0]["file"], "inventory/signals.py");
    assert_eq!(issues[0]["kind"], "unresolved_registration");
    assert_eq!(issues[0]["line"], 5);
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
fn django_known_receiver_rejection_stops_following_registration() {
    for keyword in ["signal=post_save", "invalid=True"] {
        let project = tempfile::tempdir().unwrap();
        django_signal_project(
            project.path(),
            &format!(
                "from django.db.models.signals import post_save\nfrom django.dispatch import receiver\n\n@receiver(post_save)\ndef before(sender, **kwargs):\n    return 1\n\n@receiver(post_save, {keyword})\ndef rejected(sender, **kwargs):\n    return 2\n\n@receiver(post_save)\ndef after(sender, **kwargs):\n    return 3\n"
            ),
        );
        let report = scan(project.path());
        assert_eq!(
            django_signal_evidence(&report),
            [("inventory.signals.before", "certain")],
            "{keyword}: {report}"
        );
        assert_eq!(
            findings(&report, "unused_functions"),
            ["inventory.signals.after", "inventory.signals.rejected"]
        );
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
    let unused = unreachable_functions(&report);
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
    let unused = unreachable_functions(&report);
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
        let unused = unreachable_functions(&report);
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
    assert!(unreachable_functions(&report).is_empty(), "{report}");
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
    let unused = unreachable_functions(&report);
    assert!(unused.contains(&"shutdown.AtExit.finish"), "{report}");
    assert!(unused.contains(&"helpers.shutdown_helper"), "{report}");
    let mut cached = scan_with_settings(project.path(), None, Some(cache.path()));
    cached.as_object_mut().unwrap().remove("cache");
    assert_eq!(cached, report);
}

#[test]
fn phase03_reports_the_dead_class_without_duplicate_method_findings() {
    let project = tempfile::tempdir().unwrap();
    fs::write(
        project.path().join("model.py"),
        concat!(
            "class Dormant:\n",
            "    def clean(self): return 1\n",
            "class Used:\n",
            "    def clean(self): return 2\n",
            "Used()\n",
        ),
    )
    .unwrap();

    let report = scan(project.path());
    assert_eq!(findings(&report, "unused_classes"), ["model.Dormant"]);
    assert_eq!(findings(&report, "unused_functions"), ["model.Used.clean"]);
    assert!(report["suppressed"].as_array().unwrap().iter().any(|row| {
        row["full_name"] == "model.Dormant.clean"
            && row["reason"] == "covered by the unused class finding"
    }));
}

#[test]
fn typer_callback_discovery_requires_the_real_reached_owner() {
    let project = tempfile::tempdir().unwrap();
    fs::write(
        project.path().join("commands.py"),
        concat!(
            "from typer import Typer as CommandLine\n",
            "app = CommandLine()\n",
            "@app.callback()\ndef main(verbose=False): return prepare(verbose)\n",
            "def prepare(value): return value\n",
            "def independent(): return 0\n",
            "def dormant():\n",
            "    local = CommandLine()\n",
            "    @local.callback()\n",
            "    def nested(): return dormant_helper()\n",
            "def dormant_helper(): return 0\n",
            "def identity_factory():\n",
            "    def identity(function): return function\n",
            "    return identity\n",
            "alias = app\n",
            "alias.callback = identity_factory\n",
            "@app.callback()\ndef replaced(): return 0\n",
        ),
    )
    .unwrap();

    let report = scan(project.path());
    let unused = findings(&report, "unused_functions");
    for name in ["commands.main", "commands.prepare"] {
        assert!(!unused.contains(&name), "{report}");
    }
    for name in [
        "commands.independent",
        "commands.dormant",
        "commands.dormant_helper",
        "commands.replaced",
    ] {
        assert!(unused.contains(&name), "{report}");
    }
    let symbols = report["dead_code_evidence"]["symbols"].as_array().unwrap();
    assert!(symbols.iter().any(|symbol| {
        symbol["qualified_name"] == "commands.main"
            && symbol["evidence"]
                .as_array()
                .unwrap()
                .iter()
                .any(|event| event["kind"] == "typer_callback" && event["certainty"] == "certain")
    }));
    assert!(
        !symbols
            .iter()
            .any(|symbol| symbol["qualified_name"] == "commands.dormant.nested")
    );
}
