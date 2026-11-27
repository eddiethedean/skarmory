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
    scan_with_excludes(project, settings, cache, &[], code)
}

fn scan_with_excludes(
    project: &Path,
    settings: Option<&str>,
    cache: Option<&Path>,
    excludes: &[&str],
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
        .args(excludes.iter().flat_map(|pattern| ["--exclude", *pattern]))
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

fn evidence_certainties<'a>(report: &'a Value, symbol: &str, kind: &str) -> Vec<&'a str> {
    report["dead_code_evidence"]["symbols"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["qualified_name"] == symbol)
        .flat_map(|row| row["evidence"].as_array().unwrap())
        .filter(|edge| edge["kind"] == kind)
        .map(|edge| edge["certainty"].as_str().unwrap())
        .collect()
}

#[test]
fn exported_factory_retains_only_the_public_methods_of_returned_instances() {
    let project = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    fs::write(
        project.path().join("neighbors.py"),
        "def used(): return 1\ndef unused(): return 2\n",
    )
    .unwrap();
    let source = r#"from neighbors import used, unused

class Service:
    def run(self):
        return self._dispatch()

    def _dispatch(self):
        return used()

    def _unexposed(self):
        return 0

class Decoy:
    def run(self):
        return unused()

def make_service(flag):
    Decoy()
    return Service() if flag else None

make_service(False)
"#;
    let app = project.path().join("app.py");
    for (exports, exported) in [
        ("__all__ = ['make_service']", true),
        ("__all__ = []", false),
        ("__all__ = ['make_service']", true),
    ] {
        fs::write(&app, format!("{source}\n{exports}\n")).unwrap();
        let serial = scan(project.path());
        let unused = findings(&serial, "unused_functions");
        assert!(unused.contains(&"app.Decoy.run"), "{serial}");
        assert!(unused.contains(&"app.Service._unexposed"), "{serial}");
        assert!(unused.contains(&"neighbors.unused"), "{serial}");
        for name in ["app.Service.run", "app.Service._dispatch", "neighbors.used"] {
            assert_eq!(unused.contains(&name), !exported, "{name}: {serial}");
        }
        assert_eq!(
            evidence_certainties(&serial, "app.Service.run", "public_export"),
            if exported {
                vec!["possible"]
            } else {
                Vec::new()
            },
            "{serial}"
        );
        for _ in 0..2 {
            let mut parallel = scan_with_settings(project.path(), None, Some(cache.path()));
            parallel.as_object_mut().unwrap().remove("cache").unwrap();
            assert_eq!(parallel, serial);
        }
    }
}

#[test]
fn closure_safety_retains_bindings_from_straight_line_finally() {
    let project = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let path = project.path().join("app.py");
    let source = r#"def live(): return 2
def dormant(): return 3
def launch():
    try:
        offset = 1
    finally:
        def callback():
            return live() + offset
    return callback()
launch()
"#;
    fs::write(&path, source).unwrap();
    let report = scan(project.path());
    assert_eq!(findings(&report, "unused_functions"), ["app.dormant"]);
    for _ in 0..2 {
        let mut cached = scan_with_settings(project.path(), None, Some(cache.path()));
        cached.as_object_mut().unwrap().remove("cache");
        assert_eq!(cached, report);
    }

    fs::write(&path, source.replace("offset = 1", "offset = live()")).unwrap();
    let report = scan_with_exit(project.path(), None, Some(cache.path()), 2);
    assert_eq!(report["coverage"]["status"], "incomplete");
}

#[test]
fn closure_safety_finally_requires_fresh_local_bindings() {
    let project = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let path = project.path().join("app.py");
    let definitions = r#"def release(): print("released")
class Resource:
    def __del__(self): release()
"#;
    let close = "    try:\n        resource = None\n    finally:\n        return resource\n";
    let local = "def close():\n    resource = Resource()\n";
    for (prefix, body, call) in [
        (local, close, "close()"),
        ("def close(resource):\n", close, "close(Resource())"),
        (
            "def close():\n    if __debug__:\n        resource = Resource()\n",
            close,
            "close()",
        ),
        (
            local,
            "    try:\n        unbound = 1\n    finally:\n        resource = None\n",
            "close()",
        ),
        (
            local,
            "    try:\n        unbound = 1\n    finally:\n        def resource(): return 2\n",
            "close()",
        ),
    ] {
        fs::write(&path, format!("{definitions}{prefix}{body}{call}\n")).unwrap();
        let report = scan_with_exit(project.path(), None, None, 2);
        assert_eq!(report["coverage"]["status"], "incomplete");
        assert!(findings(&report, "unused_functions").is_empty());
        let mut cached = scan_with_exit(project.path(), None, Some(cache.path()), 2);
        cached.as_object_mut().unwrap().remove("cache");
        assert_eq!(cached, report);
    }
}

#[test]
fn closure_safety_rebinding_keeps_unresolved_finalizers_visible() {
    let project = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let path = project.path().join("app.py");
    let definitions = r#"def release(): print("released")
class First:
    def read(self): return 1
class Second:
    def read(self): return 2
"#;
    let run = r#"def run():
    selected = First()
    def switch():
        nonlocal selected
        selected = Second()
    def read():
        return selected.read()
    switch()
    return read()
run()
"#;
    let ordinary_init = definitions.replace(
        "class First:\n",
        "class First:\n    def __init__(self): pass\n",
    );
    fs::write(&path, format!("{ordinary_init}{run}")).unwrap();
    let report = scan(project.path());
    assert!(findings(&report, "unused_functions").contains(&"app.First.read"));
    assert!(!findings(&report, "unused_functions").contains(&"app.Second.read"));

    let provider = project.path().join("provider.py");
    fs::write(&provider, "def replacement(): return 2\n").unwrap();
    fs::write(
        &path,
        r#"from provider import replacement
def earlier(): return 1
def run():
    selected = earlier
    def switch():
        nonlocal selected
        selected = replacement
    def read():
        return selected()
    switch()
    return read()
run()
"#,
    )
    .unwrap();
    let report = scan(project.path());
    assert_eq!(findings(&report, "unused_functions"), ["app.earlier"]);
    fs::remove_file(provider).unwrap();

    for class in ["First", "Second"] {
        let definitions = definitions.replace(
            &format!("class {class}:\n"),
            &format!("class {class}:\n    def __del__(self): release()\n"),
        );
        for run in [
            run.to_owned(),
            run.replace(
                "    def switch():",
                "    replacement = Second()\n    def switch():",
            )
            .replace(
                "        selected = Second()",
                "        selected = replacement",
            ),
        ] {
            fs::write(&path, format!("{definitions}{run}")).unwrap();
            let report = scan_with_exit(project.path(), None, None, 2);
            assert_eq!(report["coverage"]["status"], "incomplete");
            assert!(findings(&report, "unused_functions").is_empty());
            assert!(
                report["coverage"]["issues"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|issue| {
                        issue["message"] == "nonlocal assignment has no bounded closure call"
                    }),
                "{report}"
            );
            let mut cached = scan_with_exit(project.path(), None, Some(cache.path()), 2);
            cached.as_object_mut().unwrap().remove("cache");
            assert_eq!(cached, report);
        }
    }
}

#[test]
fn closure_safety_comprehension_dispatch_follows_iteration_and_consumption() {
    let project = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let path = project.path().join("app.py");
    let source = r#"def shadowed(): return 1
def eagerly_called(): return 2
def called_on_resume(): return 3
def dormant_yield(): return 4
def eager():
    def callback(): return shadowed()
    return [callback() for callback in [eagerly_called]]
def generator():
    def callback(): return shadowed()
    return (callback() for callback in [called_on_resume])
def unused_generator():
    return (callback() for callback in [dormant_yield])
eager()
list(generator())
unused_generator()
"#;
    fs::write(&path, source).unwrap();
    let report = scan(project.path());
    let unused = findings(&report, "unused_functions");
    for live in ["app.eagerly_called", "app.called_on_resume"] {
        assert!(!unused.contains(&live), "{unused:?}");
    }
    for dead in ["app.shadowed", "app.dormant_yield"] {
        assert!(unused.contains(&dead), "{unused:?}");
    }
    for _ in 0..2 {
        let mut cached = scan_with_settings(project.path(), None, Some(cache.path()));
        cached.as_object_mut().unwrap().remove("cache");
        assert_eq!(cached, report);
    }

    for consumer in [
        "set(generator())",
        "[item for item in generator()]",
        "list(generator() if __debug__ else unused_generator())",
        "dict([generator()])",
        "__all__ = ['generator']",
        "stream = generator()\n__all__ = ['stream']",
    ] {
        fs::write(&path, source.replace("list(generator())", consumer)).unwrap();
        let report = scan_with_exit(project.path(), None, Some(cache.path()), 2);
        assert!(
            report["coverage"]["issues"]
                .as_array()
                .unwrap()
                .iter()
                .any(|issue| { issue["kind"] == "unresolved_generator_execution" }),
            "{report}"
        );
    }
}

#[test]
fn deferred_generator_results_from_forwarding_lambdas_keep_coverage_incomplete() {
    let project = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let path = project.path().join("app.py");
    let forwarded = r#"def make():
    return (callback() for callback in [leaf])
def launch():
    forward = lambda callback: callback()
"#;
    let captured = r#"def launch():
    def make():
        return (callback() for callback in [leaf])
    forward = lambda: make()
"#;
    for (source, call) in [(forwarded, "forward(make)"), (captured, "forward()")] {
        let project_source = format!("def leaf(): return 2\n{source}");
        fs::write(
            &path,
            format!("{project_source}    return {call}\nlist(launch())\n"),
        )
        .unwrap();
        let report = scan_with_exit(project.path(), None, None, 2);
        assert!(
            report["coverage"]["issues"]
                .as_array()
                .unwrap()
                .iter()
                .any(|issue| { issue["kind"] == "unresolved_generator_execution" }),
            "{report}"
        );
        assert!(
            !findings(&report, "unused_functions").contains(&"app.leaf"),
            "{report}"
        );
        for _ in 0..2 {
            let mut cached = scan_with_exit(project.path(), None, Some(cache.path()), 2);
            cached.as_object_mut().unwrap().remove("cache");
            assert_eq!(cached, report);
        }

        fs::write(&path, format!("{project_source}    {call}\nlaunch()\n")).unwrap();
        let report = scan(project.path());
        assert_eq!(
            findings(&report, "unused_functions"),
            ["app.leaf"],
            "{report}"
        );
    }
}

#[test]
fn deferred_generator_arguments_to_executors_keep_coverage_incomplete() {
    let project = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let path = project.path().join("app.py");
    let prelude = r#"from concurrent.futures import ThreadPoolExecutor

def leaf(): return 42
def make(): return (callback() for callback in [leaf])
def collect(items):
    print(list(items))

pool = ThreadPoolExecutor()
"#;
    for argument in ["make()", "items=make()"] {
        fs::write(
            &path,
            format!("{prelude}pool.submit(collect, {argument})\n"),
        )
        .unwrap();
        let report = scan_with_exit(project.path(), None, None, 2);
        assert!(
            report["coverage"]["issues"]
                .as_array()
                .unwrap()
                .iter()
                .any(|issue| { issue["kind"] == "unresolved_generator_execution" }),
            "{report}"
        );
        assert!(
            !findings(&report, "unused_functions").contains(&"app.leaf"),
            "{report}"
        );
        assert_eq!(
            evidence_certainties(&report, "app.collect", "executor_submission"),
            ["possible"]
        );
        for _ in 0..2 {
            let mut cached = scan_with_exit(project.path(), None, Some(cache.path()), 2);
            cached.as_object_mut().unwrap().remove("cache");
            assert_eq!(cached, report);
        }
    }

    fs::write(&path, format!("{prelude}pool.submit(collect, [42])\n")).unwrap();
    let report = scan(project.path());
    assert_eq!(
        findings(&report, "unused_functions"),
        ["app.leaf", "app.make"],
        "{report}"
    );
}

#[test]
fn direct_lambda_calls_resolve_private_captures_and_parameter_shadowing() {
    let project = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let path = project.path().join("app.py");
    let source = r#"def captured(): return 'captured'
def passed(): return 'passed'
def shadowed(): return 'shadowed'

class _Capture:
    def run(self):
        callback = lambda: _Capture__step()
        def __step():
            return captured()
        return callback()

class Shadow:
    def run(self):
        def __step():
            return shadowed()
        callback = lambda __step: _Shadow__step()
        return callback(passed)

_Capture().run()
Shadow().run()
"#;
    fs::write(&path, source).unwrap();
    let report = scan(project.path());
    assert_eq!(
        findings(&report, "unused_functions"),
        ["app.Shadow.run.__step", "app.shadowed"]
    );
    for _ in 0..2 {
        let mut cached = scan_with_settings(project.path(), None, Some(cache.path()));
        cached.as_object_mut().unwrap().remove("cache");
        assert_eq!(cached, report);
    }

    fs::write(
        &path,
        source.replace("return callback(passed)", "return callback"),
    )
    .unwrap();
    let escaped = scan_with_exit(project.path(), None, Some(cache.path()), 2);
    assert!(
        escaped["coverage"]["issues"]
            .as_array()
            .unwrap()
            .iter()
            .any(|issue| { issue["kind"] == "unresolved_lambda_execution" }),
        "{escaped}"
    );

    let mutated = source.replace(
        "        return callback()",
        "        def replace():\n            nonlocal __step\n            __step = passed\n        replace()\n        return callback()",
    );
    fs::write(&path, mutated).unwrap();
    let report = scan_with_exit(project.path(), None, Some(cache.path()), 2);
    assert!(
        report["coverage"]["issues"]
            .as_array()
            .unwrap()
            .iter()
            .any(|issue| { issue["kind"] == "unresolved_lambda_execution" }),
        "{report}"
    );
}

#[test]
fn definition_headers_update_a_captured_receiver_before_invocation() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    let cache = tempfile::tempdir().unwrap();
    let code = root.join("app.py");
    let prelude = r#"def stale(): return "old"
def active(): return "new"

class Earlier:
    def read(self): return stale()

class Later:
    def read(self): return active()

def preserve(item): return item
"#;
    let lambda = r#"def run():
    current = Earlier()
    def get(): return current.read()
    initialize = lambda default=(current := Later()): default
    return get(), initialize
run()
"#;
    let function_decorator = r#"def run():
    current = Earlier()
    @((current := Later()) and preserve)
    def get(): return current.read()
    return get()
run()
"#;
    let class_decorator = r#"def run():
    current = Earlier()
    @((current := Later()) and preserve)
    class Local: pass
    def get(): return current.read()
    return get(), Local
run()
"#;
    for source in [lambda, function_decorator, class_decorator] {
        fs::write(&code, format!("{prelude}\n{source}")).unwrap();
        let report = scan(root);
        assert_eq!(
            evidence_certainties(&report, "app.Later.read", "direct_call"),
            ["certain"]
        );
        let unused = findings(&report, "unused_functions");
        assert!(unused.contains(&"app.Earlier.read"), "{report}");
        assert!(!unused.contains(&"app.active"), "{report}");
        for _ in 0..2 {
            let mut cached = scan_with_settings(root, None, Some(cache.path()));
            cached.as_object_mut().unwrap().remove("cache");
            assert_eq!(cached, report);
        }
    }

    let unknown_guard = prelude.replace(
        "class Later:\n",
        "class Later:\n    def __bool__(self): return False\n",
    );
    fs::write(&code, format!("{unknown_guard}\n{function_decorator}")).unwrap();
    let report = scan_with_exit(root, None, None, 2);
    assert!(
        report["analysis_errors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|issue| issue["kind"] == "unresolved_decorator")
    );
}

#[test]
fn private_attribute_lookup_uses_the_lexical_class_and_runtime_key() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    let cache = tempfile::tempdir().unwrap();
    let path = root.join("app.py");
    let source = r#"def base_helper(): return "base"
def child_helper(): return "child"
def earlier_helper(): return "earlier"
def explicit_helper(): return "explicit"

class Base:
    def __step(self):
        return base_helper()

    def run(self):
        return self.__step()

class Child(Base):
    def __step(self):
        return child_helper()

class _Owner:
    def __step(self):
        return earlier_helper()

    def _Owner__step(self):
        return explicit_helper()

Child().run()
_Owner()._Owner__step()
"#;
    fs::write(&path, source).unwrap();
    let report = scan(root);
    let unused = findings(&report, "unused_functions");
    for live in ["app.Base.__step", "app.base_helper", "app.explicit_helper"] {
        assert!(!unused.contains(&live), "{live}: {unused:?}");
    }
    for dead in ["app.Child.__step", "app.child_helper", "app.earlier_helper"] {
        assert!(unused.contains(&dead), "{dead}: {unused:?}");
    }
    for _ in 0..2 {
        let mut cached = scan_with_settings(root, None, Some(cache.path()));
        cached.as_object_mut().unwrap().remove("cache");
        assert_eq!(cached, report);
    }

    let conditional = source
        .replace("class _Owner:", "def choose(): return __debug__\n\nclass _Owner:")
        .replace(
            "    def _Owner__step(self):\n        return explicit_helper()",
            "    if choose():\n        def _Owner__step(self):\n            return explicit_helper()",
        );
    fs::write(&path, conditional).unwrap();
    let report = scan_with_exit(root, None, Some(cache.path()), 2);
    assert!(
        report["coverage"]["issues"].as_array().unwrap().iter().any(|issue| {
            issue["kind"] == "ambiguous_binding"
                && issue["message"] == "a private attribute has conditional declarations under different spellings"
        }),
        "{report}"
    );
}

#[test]
fn private_class_alias_captures_the_active_binding_at_assignment() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    let cache = tempfile::tempdir().unwrap();
    let path = root.join("app.py");
    let source = r#"def original(): return "original"
def installed(self): return "installed"
def later(self): return "later"

class Processor:
    def __step(self):
        return original()
    _Processor__step = installed
    __callback = __step
    _Processor__step = later

Processor()._Processor__callback()
"#;
    fs::write(&path, source).unwrap();
    let report = scan(root);
    let unused = findings(&report, "unused_functions");
    assert!(!unused.contains(&"app.installed"), "{unused:?}");
    assert!(unused.contains(&"app.original"), "{unused:?}");
    assert!(unused.contains(&"app.later"), "{unused:?}");
    for _ in 0..2 {
        let mut cached = scan_with_settings(root, None, Some(cache.path()));
        cached.as_object_mut().unwrap().remove("cache");
        assert_eq!(cached, report);
    }

    let conditional = source
        .replace(
            "class Processor:",
            "def choose(): return __debug__\n\nclass Processor:",
        )
        .replace(
            "    _Processor__step = installed",
            "    if choose():\n        _Processor__step = installed",
        );
    fs::write(&path, conditional).unwrap();
    let report = scan_with_exit(root, None, Some(cache.path()), 2);
    assert!(
        report["coverage"]["issues"]
            .as_array()
            .unwrap()
            .iter()
            .any(|issue| { issue["kind"] == "ambiguous_binding" }),
        "{report}"
    );
    assert!(findings(&report, "unused_functions").is_empty());
}

#[test]
fn private_class_alias_uses_module_fallback_before_the_class_binding() {
    let project = tempfile::tempdir().unwrap();
    fs::write(
        project.path().join("app.py"),
        r#"def _Handler__step(self): return "module"
def alternate(self): return "outer"

def setup():
    _Handler__step = alternate
    class Handler:
        __callback = __step
        def __step(self): return "later"
    return Handler()._Handler__callback()

setup()
"#,
    )
    .unwrap();
    let report = scan(project.path());
    let unused = findings(&report, "unused_functions");
    assert!(!unused.contains(&"app._Handler__step"), "{unused:?}");
    assert!(unused.contains(&"app.alternate"), "{unused:?}");
}

#[test]
fn traitlets_hooks_require_genuine_text_trait_declarations() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    let cache = tempfile::tempdir().unwrap();
    let path = root.join("app.py");
    let source = r#"from traitlets import Unicode, default, observe, validate
from traitlets.config import LoggingConfigurable

def handler_body():
    return "set"

class Base(LoggingConfigurable):
    pass

class Settings(Base):
    root = Unicode()
    alternate = Unicode()

    @default("alternate")
    def modern(self):
        return handler_body()

    def _root_default(self):
        return handler_body()

    @validate("root")
    def checked(self, proposal):
        return proposal["value"]

    @observe("root")
    def changed(self, change):
        return handler_body()

    def _missing_default(self):
        return None

SETTINGS = Settings()
"#;
    fs::write(&path, source).unwrap();
    let report = scan(root);
    let unused = findings(&report, "unused_functions");
    assert!(unused.contains(&"app.Settings._missing_default"));
    assert!(unused.contains(&"app.handler_body"));
    for name in ["modern", "_root_default", "checked", "changed"] {
        let qualified = format!("app.Settings.{name}");
        assert_eq!(
            evidence_certainties(&report, &qualified, "framework_class_attribute_declaration"),
            ["possible"],
        );
        assert!(evidence_certainties(&report, &qualified, "direct_call").is_empty());
    }
    for _ in 0..2 {
        let mut cached = scan_with_settings(root, None, Some(cache.path()));
        cached.as_object_mut().unwrap().remove("cache");
        assert_eq!(cached, report);
    }
    for construction in [
        "SETTINGS = Settings(root='configured')",
        "class Derived(Settings):\n    def __init__(self):\n        pass\n\nSETTINGS = Derived()",
        "class Derived(Settings):\n    def setup_instance(self):\n        pass\n\nSETTINGS = Derived()",
        "class Derived(Settings):\n    def _config_default(self):\n        return None\n\nSETTINGS = Derived()",
        "class Derived(Settings):\n    def __getattribute__(self, name):\n        pass\n\nSETTINGS = Derived()",
        "Settings.setup_instance = handler_body\nSETTINGS = Settings()",
    ] {
        fs::write(&path, source.replace("SETTINGS = Settings()", construction)).unwrap();
        let incomplete = scan_with_exit(root, None, Some(cache.path()), 2);
        assert!(
            incomplete["coverage"]["issues"]
                .as_array()
                .unwrap()
                .iter()
                .any(|issue| issue["kind"] == "unresolved_constructor"),
            "{construction}\n{incomplete}",
        );
    }
    fs::write(
        &path,
        source.replace("@default(\"alternate\")", "@default(\"root\")"),
    )
    .unwrap();
    let conflict = scan_with_exit(root, None, Some(cache.path()), 2);
    assert!(
        evidence_certainties(
            &conflict,
            "app.Settings.modern",
            "framework_class_attribute_declaration"
        )
        .is_empty()
    );
}

#[test]
fn missing_child_of_an_inert_package_selects_only_the_plain_optional_base() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    let cache = tempfile::tempdir().unwrap();
    let package = root.join("lookalike");
    fs::create_dir(&package).unwrap();
    fs::write(package.join("__init__.py"), "").unwrap();
    fs::write(
        root.join("app.py"),
        r#"try:
    from lookalike.backends.redis import RedisBackend
except ImportError:
    class RedisBackend:
        pass

class Backend(RedisBackend):
    ResultConsumer = object
    orphan = 1

ENTRY = Backend()
"#,
    )
    .unwrap();
    let report = scan(root);
    let unused = findings(&report, "unused_variables");
    assert!(unused.contains(&"app.Backend.ResultConsumer"));
    assert!(unused.contains(&"app.Backend.orphan"));
    for _ in 0..2 {
        let mut cached = scan_with_settings(root, None, Some(cache.path()));
        cached.as_object_mut().unwrap().remove("cache");
        assert_eq!(cached, report);
    }
    fs::create_dir(package.join("backends")).unwrap();
    fs::write(package.join("backends/__init__.py"), "").unwrap();
    fs::write(
        package.join("backends/redis.py"),
        "class RedisBackend: pass\n",
    )
    .unwrap();
    let incomplete = scan_with_exit(root, None, Some(cache.path()), 2);
    assert!(
        incomplete["coverage"]["issues"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| { row["kind"] == "unresolved_optional_import" })
    );
}

#[test]
fn stdlib_override_declarations_keep_hooks_without_opening_their_bodies() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    let path = root.join("app.py");
    let source = r#"import ast
import asyncio
import ctypes

def dormant_body():
    return None

class BaseVisitor(ast.NodeVisitor):
    pass

class Visitor(BaseVisitor):
    def generic_visit(self, node):
        return dormant_body()
    def unused_helper(self):
        return None

class Protocol(asyncio.Protocol):
    def eof_received(self):
        return dormant_body()

class Payload(ctypes.Structure):
    _fields_ = [('size', ctypes.c_int)]
    unused_field = 1
"#;
    fs::write(&path, source).unwrap();
    let report = scan(root);
    assert_eq!(
        findings(&report, "unused_functions"),
        ["app.Visitor.unused_helper", "app.dormant_body"]
    );
    assert!(findings(&report, "unused_variables").contains(&"app.Payload.unused_field"));
    for name in [
        "app.Visitor.generic_visit",
        "app.Protocol.eof_received",
        "app.Payload._fields_",
    ] {
        assert_eq!(
            evidence_certainties(&report, name, "framework_class_attribute_declaration"),
            ["possible"]
        );
    }
    for source in [
        source.replace("[('size', ctypes.c_int)]", "external_layout"),
        source.replace("    unused_field = 1", "    _pack_ = '8'"),
    ] {
        fs::write(&path, source).unwrap();
        let incomplete = scan_with_exit(root, None, None, 2);
        assert!(
            incomplete["coverage"]["issues"]
                .as_array()
                .unwrap()
                .iter()
                .any(|row| { row["kind"] == "unresolved_subclass_hook" })
        );
    }
}

#[test]
fn drf_and_guardian_retain_only_proven_class_options() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    let path = root.join("app.py");
    let source = r#"from guardian.admin import GuardedModelAdmin
from rest_framework import serializers
from rest_framework.authtoken.views import ObtainAuthToken
from rest_framework.throttling import ScopedRateThrottle
from rest_framework.viewsets import ModelViewSet

class Field(serializers.Field):
    default_error_messages = {'invalid': 'bad'}
    unused_option = 1

class Token(ObtainAuthToken):
    throttle_classes = [ScopedRateThrottle]
    throttle_scope = 'login'
    unused_option = 1

class ViewSet(ModelViewSet):
    http_method_names = ['get', 'post']
    unused_option = 1

class Admin(GuardedModelAdmin):
    radio_fields = {}
    unused_option = 1
"#;
    fs::write(&path, source).unwrap();
    let shown = scan(root);
    for name in [
        "app.Field.default_error_messages",
        "app.Token.throttle_scope",
        "app.ViewSet.http_method_names",
        "app.Admin.radio_fields",
    ] {
        assert_eq!(
            evidence_certainties(&shown, name, "framework_class_attribute_declaration"),
            ["possible"]
        );
    }
    for name in ["app.Field", "app.Token", "app.ViewSet", "app.Admin"] {
        assert!(
            findings(&shown, "unused_variables")
                .contains(&format!("{name}.unused_option").as_str())
        );
    }
    fs::write(&path, source.replace("[ScopedRateThrottle]", "[]")).unwrap();
    let dormant = scan(root);
    assert!(findings(&dormant, "unused_variables").contains(&"app.Token.throttle_scope"));
}

#[test]
fn builtin_dictionary_overrides_keep_only_declared_members() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    let cache = tempfile::tempdir().unwrap();
    let path = root.join("app.py");
    let body = "class Store(Base):\n    def get(self, *args): return None\n    def keys(self, *args): return None\n    def unused_neighbor(self): return None\n";
    fs::write(&path, format!("Base = dict\n{body}\nSTORE = Store()\n")).unwrap();
    let report = scan(root);
    assert_eq!(
        findings(&report, "unused_functions"),
        ["app.Store.unused_neighbor"]
    );
    for name in ["app.Store.get", "app.Store.keys"] {
        assert_eq!(
            evidence_certainties(&report, name, "builtin_override_declaration"),
            ["possible"]
        );
        assert!(evidence_certainties(&report, name, "direct_call").is_empty());
    }
    for _ in 0..2 {
        let mut cached = scan_with_settings(root, None, Some(cache.path()));
        cached.as_object_mut().unwrap().remove("cache");
        assert_eq!(cached, report);
    }
    fs::write(
        &path,
        format!("class Base: pass\n{body}\nSTORE = Store()\n"),
    )
    .unwrap();
    let shadowed = scan_with_settings(root, None, Some(cache.path()));
    assert_eq!(
        findings(&shadowed, "unused_functions"),
        [
            "app.Store.get",
            "app.Store.keys",
            "app.Store.unused_neighbor"
        ]
    );
    fs::write(
        &path,
        format!("Base = dict\n{body}\nSTORE = Store(external)\n"),
    )
    .unwrap();
    let incomplete = scan_with_exit(root, None, Some(cache.path()), 2);
    assert!(
        incomplete["coverage"]["issues"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| { row["kind"] == "unresolved_constructor" })
    );
}

#[test]
fn selected_initializer_can_store_a_fresh_source_owned_hook_instance() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    let cache = tempfile::tempdir().unwrap();
    fs::write(
        root.join("base.py"),
        "class FrameworkishBase:\n    def __init__(self):\n        self.hook = self.Hook()\n",
    )
    .unwrap();
    fs::write(root.join("impl.py"), "class Impl: pass\n").unwrap();
    let path = root.join("plugin.py");
    fs::write(
        &path,
        "from base import FrameworkishBase\nfrom impl import Impl\nclass Plugin(FrameworkishBase):\n    Hook = Impl\n    orphan = 1\nRESULT = Plugin()\n",
    )
    .unwrap();
    let report = scan(root);
    let unused = findings(&report, "unused_variables");
    assert!(unused.contains(&"plugin.Plugin.orphan"));
    assert!(!unused.contains(&"plugin.Plugin.Hook"));
    for _ in 0..2 {
        let mut cached = scan_with_settings(root, None, Some(cache.path()));
        cached.as_object_mut().unwrap().remove("cache");
        assert_eq!(cached, report);
    }

    fs::write(
        root.join("impl.py"),
        "class Impl:\n    def __init__(self): return None\n",
    )
    .unwrap();
    let incomplete = scan_with_exit(root, None, Some(cache.path()), 2);
    assert!(
        incomplete["coverage"]["issues"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| { row["kind"] == "unresolved_attribute_write" })
    );
}

#[test]
fn framework_class_attributes_require_proven_provider_declarations() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    fs::write(
        root.join("providers.py"),
        "from celery.backends import redis as backend\n\
         class Intermediate(backend.RedisBackend): pass\n",
    )
    .unwrap();
    let source = r#"from providers import Intermediate
from django.contrib.admin import SimpleListFilter
from storages.backends.s3 import S3Storage

def body_only():
    return 1

class Queue(Intermediate):
    def ResultConsumer(self, *args):
        return body_only()
    orphan = 1

class Choice(SimpleListFilter):
    parameter_name = "choice"
    title = "Choice"
    orphan = 1

class Media(S3Storage):
    bucket_name = "media"
    location = "uploads"
    orphan = 1

class RedisBackend:
    pass

class Local(RedisBackend):
    ResultConsumer = object
    orphan = 1
"#;
    let app = root.join("app.py");
    fs::write(&app, source).unwrap();
    let report = scan(root);
    let declared = "framework_class_attribute_declaration";
    let variables = findings(&report, "unused_variables");
    let functions = findings(&report, "unused_functions");
    assert!(functions.contains(&"app.body_only"));
    assert!(!functions.contains(&"app.Queue.ResultConsumer"));
    assert!(variables.contains(&"app.Local.ResultConsumer"));
    for name in ["app.Queue.orphan", "app.Choice.orphan", "app.Media.orphan"] {
        assert!(variables.contains(&name), "{name}");
    }
    for name in [
        "app.Queue.ResultConsumer",
        "app.Choice.parameter_name",
        "app.Choice.title",
        "app.Media.bucket_name",
        "app.Media.location",
    ] {
        assert!(!variables.contains(&name), "{name}");
        assert_eq!(
            evidence_certainties(&report, name, declared),
            ["possible"],
            "{name}"
        );
    }
    assert!(evidence_certainties(&report, "app.Local.ResultConsumer", declared).is_empty());

    let cache = tempfile::tempdir().unwrap();
    for _ in 0..2 {
        let mut cached = scan_with_settings(root, None, Some(cache.path()));
        cached.as_object_mut().unwrap().remove("cache");
        assert_eq!(cached, report);
    }
    fs::write(&app, format!("{source}\nChoice()\n")).unwrap();
    let incomplete = scan_with_exit(root, None, Some(cache.path()), 2);
    assert_eq!(incomplete["coverage"]["status"], "incomplete");
    assert!(
        incomplete["analysis_errors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| { row["kind"] == "unresolved_constructor" })
    );
    assert_eq!(
        evidence_certainties(&incomplete, "app.Choice.parameter_name", declared),
        ["possible"]
    );
}

#[test]
fn django_model_construction_retains_the_explicit_save_and_reports_dormant_siblings() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    let cache = tempfile::tempdir().unwrap();
    let app = root.join("app.py");
    fs::write(
        root.join("settings.py"),
        "INSTALLED_APPS = []\nSECRET_KEY = 'fixture'\n",
    )
    .unwrap();
    let source = concat!(
        "import django, os\n",
        "os.environ['DJANGO_SETTINGS_MODULE'] = 'settings'\n",
        "django.setup()\n",
        "from django.db import models\n",
        "class Worker(models.Model):\n",
        "    class Meta: app_label = 'fixture'\n",
        "    def save(self): return 1\n",
        "    def get_queryset(self): return 2\n",
        "Worker().save()\n",
    );
    fs::write(&app, source).unwrap();
    let report = scan(root);
    assert_eq!(
        findings(&report, "unused_functions"),
        ["app.Worker.get_queryset"]
    );
    assert_eq!(
        evidence_certainties(&report, "app.Worker.save", "direct_call"),
        ["certain"],
    );
    for _ in 0..2 {
        let mut cached = scan_with_settings(root, None, Some(cache.path()));
        cached.as_object_mut().unwrap().remove("cache");
        assert_eq!(cached, report);
    }

    fs::write(&app, source.replace("Worker()", "Worker(id=1)")).unwrap();
    let incomplete = scan_with_exit(root, None, None, 2);
    assert!(
        incomplete["coverage"]["issues"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["kind"] == "unresolved_constructor")
    );
}

#[test]
fn django_manager_all_dispatches_the_effective_queryset_override() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    let app = root.join("app.py");
    let declaration = concat!(
        "from django.db.models import Manager\n",
        "def load(): return 1\n",
        "class Worker(Manager):\n",
        "    def get_queryset(self): return load()\n",
        "    def abandoned(self): return 2\n",
    );
    fs::write(&app, format!("{declaration}Worker()\n")).unwrap();
    let dormant = scan(root);
    assert_eq!(
        findings(&dormant, "unused_functions"),
        [
            "app.Worker.abandoned",
            "app.Worker.get_queryset",
            "app.load"
        ],
    );
    fs::write(&app, format!("{declaration}Worker().all()\n")).unwrap();
    let active = scan(root);
    assert_eq!(
        findings(&active, "unused_functions"),
        ["app.Worker.abandoned"]
    );
    assert_eq!(
        evidence_certainties(
            &active,
            "app.Worker.get_queryset",
            "django_manager_queryset"
        ),
        ["certain"],
    );
    fs::write(&app, format!("{declaration}Worker().all(1)\n")).unwrap();
    let incomplete = scan_with_exit(root, None, None, 2);
    assert!(
        evidence_certainties(
            &incomplete,
            "app.Worker.get_queryset",
            "django_manager_queryset",
        )
        .is_empty(),
    );
}

#[test]
fn generic_django_storage_constructor_uses_empty_defaults() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    let app = root.join("app.py");
    let provider = "from storages.base import BaseStorage\n";
    let declaration = "class Media(BaseStorage):\n    bucket_name = 'unused'\n    orphan = 1\n";
    fs::write(&app, format!("{provider}{declaration}\nMedia()\n")).unwrap();
    let report = scan(root);
    let variables = findings(&report, "unused_variables");
    assert!(variables.contains(&"app.Media.bucket_name"));
    assert!(variables.contains(&"app.Media.orphan"));

    for source in [
        format!("{provider}{declaration}\nMedia(bucket_name='uploads')\n"),
        format!(
            "{provider}{declaration}    def get_default_settings(self): return {{'bucket_name': 'configured'}}\n\nMedia()\n"
        ),
    ] {
        fs::write(&app, source).unwrap();
        let report = scan_with_exit(root, None, None, 2);
        assert!(
            report["coverage"]["issues"]
                .as_array()
                .unwrap()
                .iter()
                .any(|row| { row["kind"] == "unresolved_constructor" })
        );
    }
}

#[test]
fn explicitly_selected_django_settings_keep_declarations_without_a_launcher() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    let cache = tempfile::tempdir().unwrap();
    fs::create_dir(root.join("conf")).unwrap();
    for (name, source) in [
        ("conf/__init__.py", ""),
        (
            "conf/base.py",
            "INSTALLED_APPS = []\nROOT_URLCONF = 'conf.urls'\nCUSTOM_FLAG = True\n",
        ),
        (
            "conf/settings_production.py",
            "from conf.base import *\nPRODUCTION_ONLY = CUSTOM_FLAG\n",
        ),
        (
            "conf/urls.py",
            "from django.urls import path\ndef handler(request): pass\nurlpatterns = []\n",
        ),
        ("conf/ordinary.py", "STALE_CONSTANT = True\n"),
    ] {
        fs::write(root.join(name), source).unwrap();
    }
    let option = "conf.settings_production.PRODUCTION_ONLY";
    let report = scan_with_settings(root, Some("conf.settings_production"), None);
    assert_eq!(
        evidence_certainties(&report, option, "django_setting"),
        ["possible"]
    );
    assert!(!findings(&report, "unused_variables").contains(&option));
    assert!(findings(&report, "unused_variables").contains(&"conf.ordinary.STALE_CONSTANT"));
    assert!(evidence_certainties(&report, "conf.urls.handler", "django_registration").is_empty());
    assert!(findings(&scan(root), "unused_variables").contains(&option));
    for _ in 0..2 {
        let mut cached =
            scan_with_settings(root, Some("conf.settings_production"), Some(cache.path()));
        cached.as_object_mut().unwrap().remove("cache");
        assert_eq!(cached, report);
    }
}

#[test]
fn wagtail_snippet_options_require_registration_with_the_real_viewset_and_model() {
    let project = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let root = project.path();
    let sources = r#"from django.db import models
from wagtail.snippets.models import register_snippet
from wagtail.snippets.views.snippets import SnippetViewSet

class Person(models.Model):
    name = models.CharField(max_length=20)
    sort_order = models.IntegerField(default=0)

class PersonViewSet(SnippetViewSet):
    list_display = ("name",)
    sort_order_field = "sort_order"
    adjacent_option = 1
"#;
    let register = "register_snippet(Person, viewset=PersonViewSet)\n";
    for (suffix, registered) in [
        (register, true),
        ("", false),
        (
            "def register_snippet(model, viewset): pass\nregister_snippet(Person, viewset=PersonViewSet)\n",
            false,
        ),
    ] {
        fs::write(root.join("app.py"), format!("{sources}{suffix}")).unwrap();
        let scan_result = scan(root);
        for option in ["list_display", "sort_order_field"] {
            let name = format!("app.PersonViewSet.{option}");
            assert_eq!(
                findings(&scan_result, "unused_variables").contains(&name.as_str()),
                !registered
            );
            assert_eq!(
                evidence_certainties(&scan_result, &name, "wagtail_snippet_option"),
                if registered { vec!["possible"] } else { vec![] }
            );
        }
        assert!(
            findings(&scan_result, "unused_variables")
                .contains(&"app.PersonViewSet.adjacent_option")
        );
        for _ in 0..2 {
            let mut cached = scan_with_settings(root, None, Some(cache.path()));
            cached.as_object_mut().unwrap().remove("cache");
            assert_eq!(cached, scan_result);
        }
    }
    fs::write(
        root.join("app.py"),
        format!("{sources}PersonViewSet.on_register = 1\n{register}"),
    )
    .unwrap();
    assert_eq!(
        scan_with_exit(root, None, None, 2)["coverage"]["status"],
        "incomplete"
    );
}

#[test]
fn django_ready_import_requires_an_active_hook_and_a_proven_signal_registration() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    fs::create_dir(root.join("shop")).unwrap();
    fs::write(root.join("shop/__init__.py"), "").unwrap();
    fs::write(root.join("shop/helpers.py"), "def helper(): pass\n").unwrap();
    fs::write(
        root.join("shop/apps.py"),
        r#"from django.apps import AppConfig
class Config(AppConfig):
    name = 'shop'
    def initialize(self):
        from . import helpers, signals as hooks
    ready = initialize
    def dormant(self):
        from . import signals as unused_hooks
"#,
    )
    .unwrap();
    fs::write(
        root.join("main.py"),
        "import django, os\nos.environ['DJANGO_SETTINGS_MODULE'] = 'settings'\ndjango.setup()\n",
    )
    .unwrap();
    let registration = "from django.dispatch import receiver\nfrom django.db.models.signals import post_save\n\
                        @receiver(post_save)\ndef register(sender, **kwargs): pass\n";
    let cache = tempfile::tempdir().unwrap();
    for (installed_apps, signals, retains_signal_import) in [
        ("['shop.apps.Config']", registration, true),
        ("[]", registration, false),
        ("['shop.apps.Config']", "def lookalike(): pass\n", false),
    ] {
        fs::write(
            root.join("settings.py"),
            format!("INSTALLED_APPS = {installed_apps}\nSECRET_KEY = 'fixture'\n"),
        )
        .unwrap();
        fs::write(root.join("shop/signals.py"), signals).unwrap();
        let report = scan(root);
        let imports = report["unused_imports"].as_array().unwrap();
        assert!(imports.iter().any(|row| row["full_name"] == "shop.helpers"));
        assert!(
            imports
                .iter()
                .any(|row| row["full_name"] == "shop.signals" && row["line"] == 8),
            "{report}"
        );
        assert_eq!(
            imports
                .iter()
                .any(|row| row["full_name"] == "shop.signals" && row["line"] == 5),
            !retains_signal_import,
            "{report}"
        );
        assert_eq!(
            evidence_certainties(
                &report,
                "shop.apps.Config.initialize.hooks",
                "django_ready_signal_import"
            ),
            if retains_signal_import {
                vec!["certain"]
            } else {
                vec![]
            },
            "{report}"
        );
        for _ in 0..2 {
            let mut cached = scan_with_settings(root, None, Some(cache.path()));
            cached.as_object_mut().unwrap().remove("cache");
            assert_eq!(cached, report);
        }
    }
}

#[test]
fn gin_bindings_keep_only_selected_registrations_and_leave_wrappers_unused() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    fs::create_dir(root.join("app")).unwrap();
    fs::write(root.join("app/__init__.py"), "").unwrap();
    fs::write(
        root.join("app/components.py"),
        r#"from gin import configurable as register
from functools import lru_cache
from pydantic import validate_call
from tenacity import retry, stop_after_attempt

def helper():
    return 1

@register("optimizer")
def optimizer(rate=0.1):
    return helper()

@register("network", denylist=["inputs"])
def network(inputs, layers=2):
    return layers

@register("retired")
def retired(value=0):
    return value

@retry(stop=stop_after_attempt(3))
def retry_wrapper():
    return 1

@lru_cache(maxsize=4)
def cache_wrapper():
    return 1

@validate_call()
def validate_wrapper():
    return 1
"#,
    )
    .unwrap();
    fs::write(
        root.join("main.py"),
        "import gin as registry\nfrom app import components\nregistry.parse_config_file('config.gin')\n",
    )
    .unwrap();
    fs::write(
        root.join("dormant.py"),
        "def launch():\n    import gin\n    gin.parse_config_file('missing.gin')\n",
    )
    .unwrap();
    let config = root.join("config.gin");
    fs::write(
        &config,
        "optimizer.rate = 0.05 # comment\nnetwork.layers = 3\n",
    )
    .unwrap();

    let cache = tempfile::tempdir().unwrap();
    let compare = || {
        let report = scan(root);
        let mut warm = scan_with_settings(root, None, Some(cache.path()));
        warm.as_object_mut().unwrap().remove("cache");
        assert_eq!(warm, report);
        report
    };

    let report = compare();
    assert_eq!(
        findings(&report, "unused_functions"),
        [
            "app.components.cache_wrapper",
            "app.components.helper",
            "app.components.retired",
            "app.components.retry_wrapper",
            "app.components.validate_wrapper",
            "dormant.launch",
        ],
        "{report}"
    );
    for symbol in ["app.components.optimizer", "app.components.network"] {
        assert_eq!(
            evidence_certainties(&report, symbol, "gin_config_binding"),
            ["possible"]
        );
    }

    fs::write(&config, "retired.value = 4\n").unwrap();
    let report = compare();
    assert_eq!(
        evidence_certainties(&report, "app.components.retired", "gin_config_binding"),
        ["possible"]
    );
    assert!(findings(&report, "unused_functions").contains(&"app.components.optimizer"));
    assert!(findings(&report, "unused_functions").contains(&"app.components.network"));

    for text in [
        "network.inputs = 'denied'\n",
        "include 'external.gin'\n",
        "optimizer.rate = @unknown()\n",
        "missing.param = 2\n",
    ] {
        fs::write(&config, text).unwrap();
        let report = scan_with_exit(root, None, Some(cache.path()), 2);
        assert_eq!(report["coverage"]["status"], "incomplete");
    }
    fs::remove_file(&config).unwrap();
    let report = scan_with_exit(root, None, Some(cache.path()), 2);
    assert_eq!(report["coverage"]["status"], "incomplete");
}

#[test]
fn gin_registry_and_wrapper_lookalikes_need_resolved_providers() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    fs::write(
        root.join("gin.py"),
        "def configurable(name):\n    return lambda function: function\ndef parse_config_file(path): return None\n",
    )
    .unwrap();
    fs::write(
        root.join("app.py"),
        "from gin import configurable, parse_config_file\n@configurable('optimizer')\ndef optimizer(rate=1): return rate\nparse_config_file('config.gin')\n",
    )
    .unwrap();
    fs::write(root.join("config.gin"), "optimizer.rate = 2\n").unwrap();
    let report = scan(root);
    assert!(evidence_certainties(&report, "app.optimizer", "gin_config_binding").is_empty());
    assert!(findings(&report, "unused_functions").contains(&"app.optimizer"));
    fs::remove_file(root.join("gin.py")).unwrap();

    fs::write(
        root.join("app.py"),
        "import gin\ngin.parse_config_file('config.gin')\n@gin.configurable('optimizer')\ndef optimizer(rate=1): return rate\n",
    )
    .unwrap();
    let report = scan_with_exit(root, None, None, 2);
    assert!(evidence_certainties(&report, "app.optimizer", "gin_config_binding").is_empty());

    fs::write(
        root.join("app.py"),
        "from tenacity import retry\n@retry(stop=unresolved_stop)\ndef wrapped(): return 1\n",
    )
    .unwrap();
    let report = scan_with_exit(root, None, None, 2);
    assert!(
        report["analysis_errors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|issue| issue["kind"] == "unresolved_decorator")
    );
}

#[test]
fn dictionary_type_alias_allows_a_bounded_logging_config_write() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    let cache = tempfile::tempdir().unwrap();
    fs::create_dir(root.join("pkg")).unwrap();
    fs::write(root.join("pkg/__init__.py"), "").unwrap();
    fs::write(
        root.join("pkg/formatters.py"),
        "class Active: pass\nclass Retired: pass\n",
    )
    .unwrap();
    let lookalike = root.join("path_policy.py");
    fs::write(&lookalike, "PathLike = dict\n").unwrap();
    let source = "import logging.config\nimport path_policy as os\ndef unknown(config): config['formatters'] = {}\nCONFIG = {'version': 1, 'formatters': {'active': {'()': 'pkg.formatters.Active'}}}\nif isinstance(CONFIG, os.PathLike):\n    unknown(CONFIG)\nlogging.config.dictConfig(CONFIG)\n";
    let app = root.join("app.py");
    fs::write(&app, source).unwrap();
    let report = scan(root);
    assert_eq!(
        findings(&report, "unused_classes"),
        ["pkg.formatters.Active", "pkg.formatters.Retired"],
    );
    for _ in 0..2 {
        let mut cached = scan_with_settings(root, None, Some(cache.path()));
        cached.as_object_mut().unwrap().remove("cache");
        assert_eq!(cached, report);
    }

    fs::remove_file(&lookalike).unwrap();
    let os_shadow = root.join("os.py");
    fs::write(&os_shadow, "class PathLike: pass\n").unwrap();
    let frozen_os_source = source
        .replace("import path_policy as os", "import os")
        .replace("def unknown(config): config['formatters'] = {}\n", "");
    fs::write(&app, frozen_os_source).unwrap();
    let stdlib = scan_with_settings(root, None, Some(cache.path()));
    assert_eq!(
        findings(&stdlib, "unused_classes"),
        ["os.PathLike", "pkg.formatters.Retired"]
    );

    fs::remove_file(os_shadow).unwrap();
    fs::write(&lookalike, "PathLike = dict\n").unwrap();
    fs::write(
        &app,
        source.replace("config['formatters'] = {}", "mutate_unknown(config)"),
    )
    .unwrap();
    let incomplete = scan_with_exit(root, None, Some(cache.path()), 2);
    assert!(
        incomplete["analysis_errors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|issue| issue["kind"] == "unresolved_call"),
        "{incomplete}"
    );
}

#[test]
fn reached_stdlib_logging_binds_os_before_a_local_pathlike_stub() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    let app = root.join("app.py");
    let cache = tempfile::tempdir().unwrap();
    let configure = r#"CONFIG = {'version': 1, 'formatters': {'main': {'()': 'formatters.Active'}}}
if isinstance(CONFIG, os.PathLike):
    unknown(CONFIG)
logging.config.dictConfig(CONFIG)
"#;
    fs::write(root.join("os.py"), "class PathLike: pass\n").unwrap();
    fs::write(
        root.join("formatters.py"),
        "class Active: pass\nclass Retired: pass\n",
    )
    .unwrap();
    let joined = format!("import logging.config\nimport os\n{configure}");
    fs::write(&app, &joined).unwrap();

    let factory = |report: &Value| {
        report["dead_code_evidence"]["symbols"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| {
                entry["qualified_name"] == "formatters.Active"
                    && entry["evidence"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|edge| edge["kind"] == "logging_factory")
            })
    };
    let report = scan(root);
    assert!(factory(&report), "{report}");
    let unused = findings(&report, "unused_classes");
    assert!(unused.contains(&"formatters.Retired"), "{report}");
    assert!(unused.contains(&"os.PathLike"), "{report}");
    for _ in 0..2 {
        let mut cached = scan_with_settings(root, None, Some(cache.path()));
        cached.as_object_mut().unwrap().remove("cache");
        assert_eq!(cached, report);
    }

    fs::write(
        &app,
        format!("import os\nimport logging.config\n{configure}"),
    )
    .unwrap();
    let report = scan_with_exit(root, None, Some(cache.path()), 2);
    assert!(!factory(&report), "{report}");

    fs::write(&app, joined).unwrap();
    let socket = root.join("socket.py");
    fs::write(&socket, "").unwrap();
    let report = scan_with_exit(root, None, None, 2);
    assert!(!factory(&report), "{report}");
    fs::remove_file(socket).unwrap();

    fs::create_dir(root.join("logging")).unwrap();
    fs::write(root.join("logging/__init__.py"), "").unwrap();
    fs::write(
        root.join("logging/config.py"),
        "def dictConfig(config): return config\n",
    )
    .unwrap();
    let report = scan_with_exit(root, None, None, 2);
    assert!(!factory(&report), "{report}");
    let report = scan_with_excludes(root, None, None, &["logging"], 2);
    assert!(!factory(&report), "{report}");
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
fn gunicorn_config_retains_effective_declarations_with_possible_evidence() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    let source = "workers = 2\nworkers = 4\nworker_hint = 3\n\n\
                  def worker_exit(server, worker):\n    server.log.info(worker.pid)\n\n\
                  def on_message(server, message):\n    return message\n";
    fs::write(root.join("gunicorn_config.py"), source).unwrap();
    fs::write(
        root.join("other.py"),
        "workers = 2\ndef worker_exit(server, worker): return None\n",
    )
    .unwrap();
    let report = scan(root);
    assert_eq!(
        findings(&report, "unused_functions"),
        ["gunicorn_config.on_message", "other.worker_exit"]
    );
    assert_eq!(
        findings(&report, "unused_variables"),
        [
            "gunicorn_config.worker_hint",
            "gunicorn_config.workers",
            "other.workers"
        ]
    );
    for name in ["gunicorn_config.worker_exit", "gunicorn_config.workers"] {
        assert!(
            report["dead_code_evidence"]["symbols"]
                .as_array()
                .unwrap()
                .iter()
                .any(|symbol| symbol["qualified_name"] == name
                    && symbol["evidence"].as_array().unwrap().iter().any(|edge| {
                        edge["kind"] == "gunicorn_config_declaration"
                            && edge["certainty"] == "possible"
                    })),
            "{report}"
        );
    }
    let cache = tempfile::tempdir().unwrap();
    for _ in 0..2 {
        let mut cached = scan_with_settings(root, None, Some(cache.path()));
        cached.as_object_mut().unwrap().remove("cache");
        assert_eq!(cached, report);
    }
}

#[test]
fn fastmcp_registration_follows_applied_wrappers_without_running_dormant_bodies() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    let providers = root.join("providers.py");
    let handlers = root.join("handlers.py");
    let source = r#"from functools import wraps
from mcp.server.fastmcp import FastMCP

server = FastMCP("demo")

def expose(fn):
    @server.tool(name=fn.__name__)
    @wraps(fn)
    def wrapper(*args, **kwargs):
        return str(fn(*args, **kwargs))
    return wrapper

def register_as(label):
    def decorate(fn):
        return server.tool(name=label)(fn)
    return decorate

def deferred(fn):
    def wrapper(*args, **kwargs):
        return server.tool(name="late")(fn)(*args, **kwargs)
    return wrapper
"#;
    fs::write(&providers, source).unwrap();
    let client = r#"from providers import expose, register_as, deferred, server

@expose
def live(value): return helper(value)

@register_as("configured")
def alternate(): return 1

@server.tool(name="direct")
def direct(): return 2

@server.tool(name="direct")
def duplicate(): return 9

@deferred
def inactive(): return 3

def helper(value): return value
def retired(): return 4
"#;
    fs::write(&handlers, client).unwrap();
    let report = scan(root);
    let unused = findings(&report, "unused_functions");
    assert!(
        [
            "handlers.duplicate",
            "handlers.inactive",
            "handlers.retired"
        ]
        .iter()
        .all(|name| unused.contains(name)),
        "{report}"
    );
    let evidence = report["dead_code_evidence"]["symbols"].as_array().unwrap();
    for target in ["handlers.live", "handlers.alternate", "handlers.direct"] {
        assert!(
            evidence
                .iter()
                .any(|symbol| symbol["qualified_name"] == target
                    && symbol["evidence"].as_array().unwrap().iter().any(|edge| {
                        edge["kind"] == "mcp_tool_registration" && edge["certainty"] == "possible"
                    })),
            "{target}\n{report}"
        );
    }
    assert!(!unused.contains(&"handlers.helper"), "{report}");
    let cache = tempfile::tempdir().unwrap();
    let mut cached = scan_with_settings(root, None, Some(cache.path()));
    cached.as_object_mut().unwrap().remove("cache");
    assert_eq!(report, cached);

    fs::write(&handlers, format!("{client}\ninactive()\n")).unwrap();
    let report = scan_with_exit(root, None, None, 2);
    assert!(
        report["analysis_errors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|error| { error["kind"] == "unresolved_call" && error["file"] == "handlers.py" }),
        "{report}"
    );

    fs::write(&handlers, client).unwrap();
    fs::write(
        &providers,
        format!("{source}\nfrom extension import customize\ncustomize(server)\n"),
    )
    .unwrap();
    let report = scan_with_exit(root, None, None, 2);
    assert!(
        !report["dead_code_evidence"]["symbols"]
            .as_array()
            .unwrap()
            .iter()
            .any(|symbol| {
                symbol["evidence"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|edge| edge["kind"] == "mcp_tool_registration")
            }),
        "{report}"
    );
}

#[test]
fn opaque_imported_decorator_retains_possible_use_through_an_identity_facade() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    let app = root.join("app.py");
    let source = "from fastapi import FastAPI\nclass Local:\n    def get(self, path): return lambda function: function\nfake = Local()\nfrom plugin import register_entry\n@register_entry\n@fake.get('/')\ndef endpoint(): return helper()\ndef helper(): return 1\n";
    fs::write(&app, source).unwrap();
    let report = scan_with_exit(root, None, None, 2);
    let errors = report["analysis_errors"].as_array().unwrap();
    assert_eq!(errors.len(), 1, "{report}");
    assert_eq!(errors[0]["kind"], "unresolved_decorator", "{report}");
    let evidence = report["dead_code_evidence"]["symbols"].as_array().unwrap();
    assert!(
        evidence
            .iter()
            .any(|symbol| symbol["qualified_name"] == "app.endpoint"
                && symbol["evidence"].as_array().unwrap().iter().any(|edge| {
                    edge["kind"] == "opaque_decorator_argument" && edge["certainty"] == "possible"
                })),
        "{report}"
    );
    assert!(
        evidence
            .iter()
            .any(|symbol| symbol["qualified_name"] == "app.helper"
                && symbol["evidence"].as_array().unwrap().iter().any(|edge| {
                    edge["kind"] == "direct_call" && edge["certainty"] == "possible"
                })),
        "{report}"
    );
    assert!(
        !evidence.iter().any(|symbol| {
            symbol["evidence"]
                .as_array()
                .unwrap()
                .iter()
                .any(|edge| edge["kind"] == "fastapi_registration")
        }),
        "{report}"
    );

    fs::write(
        &app,
        source.replace("lambda function: function", "lambda function: None"),
    )
    .unwrap();
    let report = scan_with_exit(root, None, None, 2);
    assert!(
        !report["dead_code_evidence"]["symbols"]
            .as_array()
            .unwrap()
            .iter()
            .any(|symbol| {
                symbol["evidence"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|edge| edge["kind"] == "opaque_decorator_argument")
            }),
        "{report}"
    );

    fs::write(&app, source).unwrap();
    fs::write(
        root.join("plugin.py"),
        "def register_entry(function): return function\n",
    )
    .unwrap();
    let report = scan(root);
    assert_eq!(
        findings(&report, "unused_functions"),
        ["app.endpoint", "app.helper"],
        "{report}"
    );
}

#[test]
fn drf_declarations_keep_only_proven_framework_protocol_members() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    let web = root.join("api.py");
    fs::write(
        &web,
        "from rest_framework import serializers as fields\nfrom rest_framework.viewsets import ViewSet as Views\n\nclass Users(Views):\n    def list(self, request): return external_service(request)\n    def internal(self): return None\n\nclass Account(fields.Serializer):\n    contact = fields.EmailField()\n    internal_id = fields.IntegerField(read_only=True)\n    def validate_contact(self, value): return opaque_validation(value)\n    def validate_internal_id(self, value): return value\n    def validate_missing(self, value): return value\n\nclass ViewSet: pass\nclass Fake(ViewSet):\n    def list(self, request): return None\n",
    )
    .unwrap();
    let report = scan(root);
    assert_eq!(
        findings(&report, "unused_classes"),
        ["api.Fake"],
        "{report}"
    );
    assert_eq!(
        findings(&report, "unused_functions"),
        [
            "api.Account.validate_internal_id",
            "api.Account.validate_missing",
            "api.Users.internal"
        ],
        "{report}"
    );
    assert!(findings(&report, "unused_variables").is_empty(), "{report}");
    let symbols = report["dead_code_evidence"]["symbols"].as_array().unwrap();
    for name in [
        "api.Users",
        "api.Users.list",
        "api.Account",
        "api.Account.validate_contact",
    ] {
        assert!(
            symbols.iter().any(|symbol| symbol["qualified_name"] == name
                && symbol["evidence"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|evidence| {
                        evidence["kind"] == "drf_protocol_declaration"
                            && evidence["certainty"] == "possible"
                    })),
            "{name}\n{report}"
        );
    }
    let cache = tempfile::tempdir().unwrap();
    let mut cached = scan_with_settings(root, None, Some(cache.path()));
    cached.as_object_mut().unwrap().remove("cache");
    assert_eq!(report, cached);

    fs::write(
        &web,
        "from rest_framework.viewsets import ViewSet\nclass Users(ViewSet):\n    def list(self, request): return None\ndef replacement(self, request): return request\nUsers.list = replacement\n",
    )
    .unwrap();
    let report = scan(root);
    assert_eq!(
        findings(&report, "unused_functions"),
        ["api.Users.list"],
        "{report}"
    );

    fs::write(
        &web,
        "from rest_framework import serializers\nfrom extension import options\nclass Account(serializers.Serializer):\n    email = serializers.EmailField(read_only=options.read_only)\n",
    )
    .unwrap();
    let report = scan_with_exit(root, None, None, 2);
    assert!(
        report["analysis_errors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|issue| { issue["kind"] == "unresolved_constructor" }),
        "{report}"
    );
}

#[test]
fn drf_method_fields_use_declared_serializer_fields_and_inherited_getters() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    fs::write(
        root.join("api.py"),
        "from rest_framework import serializers
class Base(serializers.Serializer):
    title = serializers.SerializerMethodField(method_name='render_title')
    invalid = serializers.SerializerMethodField()
class Mixin:
    extra = serializers.SerializerMethodField()
    def get_total(self, obj): return unreachable_effect(obj)
    def render_title(self, obj): return obj
    def get_extra(self, obj): return obj
    def get_invalid(self, obj, required): return obj
    def get_orphan(self, obj): return obj
class Report(Mixin, Base):
    total = serializers.SerializerMethodField()
",
    )
    .unwrap();
    let report = scan(root);
    let reported = findings(&report, "unused_functions");
    assert_eq!(
        reported,
        [
            "api.Mixin.get_extra",
            "api.Mixin.get_invalid",
            "api.Mixin.get_orphan"
        ],
        "{report}"
    );
    let symbols = report["dead_code_evidence"]["symbols"].as_array().unwrap();
    for name in ["api.Mixin.get_total", "api.Mixin.render_title"] {
        assert!(
            symbols.iter().any(|symbol| symbol["qualified_name"] == name
                && symbol["evidence"].as_array().unwrap().iter().any(|edge| {
                    edge["kind"] == "drf_protocol_declaration" && edge["certainty"] == "possible"
                })),
            "{name}\n{report}"
        );
    }
}

#[test]
fn phase03_named_conditions_bind_before_testing_and_keep_phase02_coverage() {
    let project = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let root = project.path();
    fs::write(
        root.join("entry.py"),
        "def called(): return 1\ndef skipped(): return 2\nif (first := False):\n    skipped()\nelif (callback := called):\n    callback()\n",
    )
    .unwrap();
    let report = scan(root);
    assert_eq!(findings(&report, "unused_functions"), ["entry.skipped"]);
    for _ in 0..2 {
        let mut cached = scan_with_settings(root, None, Some(cache.path()));
        cached.as_object_mut().unwrap().remove("cache");
        assert_eq!(cached, report);
    }
    let phase02 = Command::new(env!("CARGO_BIN_EXE_skarmory"))
        .current_dir(root)
        .env_remove("SKYLOS_CONFIG_FILE")
        .args(["--profile", "skylos", ".", "--json"])
        .output()
        .unwrap();
    let phase02_report: Value = serde_json::from_slice(&phase02.stdout).unwrap();
    assert_eq!(phase02.status.code(), Some(2), "{phase02_report}");
    assert!(
        phase02_report["analysis_errors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|error| { error["kind"] == "unsupported_construct" && error["line"] == 3 }),
        "{phase02_report}"
    );
}

#[test]
fn click_command_declarations_and_reference_safe_helpers_have_distinct_dispositions() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    fs::write(
        root.join("commands.py"),
        "from click import Command as Base\nclass Report(Base):\n    def invoke(self, context): return opaque_handler(context)\n    def retired(self): return None\nclass Command: pass\nclass Draft(Command):\n    def invoke(self, context): return None\ndef format_arguments(): return []\ndef compose_name(): return ''\ndef dormant_name(): return ''\ndef unused(): return dormant_name()\n",
    )
    .unwrap();
    let report = scan(root);
    assert_eq!(
        findings(&report, "unused_classes"),
        ["commands.Draft"],
        "{report}"
    );
    assert_eq!(
        findings(&report, "unused_functions"),
        [
            "commands.Report.retired",
            "commands.dormant_name",
            "commands.unused"
        ],
        "{report}"
    );
    assert_eq!(
        report["dead_code_abstentions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| {
                (
                    entry["full_name"].as_str().unwrap(),
                    entry["message"].as_str().unwrap(),
                )
            })
            .collect::<Vec<_>>(),
        [
            ("commands.format_arguments", "weak helper function"),
            ("commands.compose_name", "weak helper function")
        ],
        "{report}"
    );
    let evidence = report["dead_code_evidence"]["symbols"].as_array().unwrap();
    assert!(
        evidence
            .iter()
            .any(|entry| entry["qualified_name"] == "commands.Report.invoke"
                && entry["evidence"].as_array().unwrap().iter().any(|edge| {
                    edge["kind"] == "click_command_declaration" && edge["certainty"] == "possible"
                })),
        "{report}"
    );
}

#[test]
fn click_command_declarations_do_not_execute_a_short_circuited_callback() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    let source = "from click import command\n@command()\ndef entry(): return helper()\ndef helper(): return 1\ndef unused_neighbor(): return 2\nflag = True\nflag = not flag\nflag and entry()\n";
    fs::write(root.join("app.py"), source).unwrap();
    let report = scan(root);
    assert_eq!(
        findings(&report, "unused_functions"),
        ["app.helper", "app.unused_neighbor"],
        "{report}"
    );
    assert_eq!(
        evidence_certainties(&report, "app.entry", "click_command_declaration"),
        ["possible"]
    );
    let cache = tempfile::tempdir().unwrap();
    for _ in 0..2 {
        let mut cached = scan_with_settings(root, None, Some(cache.path()));
        cached.as_object_mut().unwrap().remove("cache");
        assert_eq!(cached, report);
    }

    fs::write(
        root.join("app.py"),
        source.replace(
            "from click import command",
            "def command(): return lambda callback: callback",
        ),
    )
    .unwrap();
    let shadowed = scan(root);
    assert!(
        findings(&shadowed, "unused_functions").contains(&"app.entry"),
        "{shadowed}"
    );
    assert!(evidence_certainties(&shadowed, "app.entry", "click_command_declaration").is_empty());

    fs::write(
        root.join("app.py"),
        source
            .replace(
                "from click import command",
                "import click\ndef local_factory(): return lambda callback: callback\nclick.command = local_factory",
            )
            .replace("@command()", "@click.command()"),
    )
    .unwrap();
    let rebound = scan(root);
    assert!(
        findings(&rebound, "unused_functions").contains(&"app.entry"),
        "{rebound}"
    );
    assert!(evidence_certainties(&rebound, "app.entry", "click_command_declaration").is_empty());

    fs::write(
        root.join("app.py"),
        source.replace("flag and entry()", "entry()"),
    )
    .unwrap();
    let invocation = scan_with_exit(root, None, None, 2);
    assert_eq!(
        invocation["coverage"]["status"], "incomplete",
        "{invocation}"
    );
    assert!(
        evidence_certainties(&invocation, "app.entry", "direct_call").is_empty(),
        "{invocation}"
    );
}

#[test]
fn starlette_plain_text_responses_require_bounded_content() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    let path = root.join("app.py");
    let source = r#"from starlette.applications import Starlette
from starlette.responses import PlainTextResponse as TextResponse

app = Starlette()

async def homepage(request):
    return TextResponse(content="home")

app.add_route("/", route=homepage, methods=["GET"])

def status_page(): return "status"
"#;
    fs::write(&path, source).unwrap();
    let report = scan(root);
    assert_eq!(findings(&report, "unused_functions"), ["app.status_page"]);

    let cache = tempfile::tempdir().unwrap();
    for _ in 0..2 {
        let mut cached = scan_with_settings(root, None, Some(cache.path()));
        cached.as_object_mut().unwrap().remove("cache");
        assert_eq!(cached, report);
    }

    fs::write(
        &path,
        source.replace(r#"content="home""#, "content=request"),
    )
    .unwrap();
    let incomplete = scan_with_exit(root, None, Some(cache.path()), 2);
    assert!(
        incomplete["coverage"]["issues"]
            .as_array()
            .unwrap()
            .iter()
            .any(|issue| issue["kind"] == "unresolved_call" && issue["line"] == 7)
    );

    fs::write(&path, source).unwrap();
    let mut cached = scan_with_settings(root, None, Some(cache.path()));
    cached.as_object_mut().unwrap().remove("cache");
    assert_eq!(cached, report);
}

#[test]
fn starlette_routes_require_a_supported_call_and_application() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    let path = root.join("app.py");
    let source = r#"from starlette.applications import Starlette as Application

app = Application()
dormant = Application()
async def homepage(request): return None
async def inactive(request): return None
app.add_route("/", route=homepage, methods=["GET"])
dormant.add_route("/", inactive)
"#;
    fs::write(&path, source).unwrap();
    let report = scan(root);
    assert_eq!(findings(&report, "unused_functions"), ["app.inactive"]);
    assert!(
        report["dead_code_evidence"]["symbols"]
            .as_array()
            .unwrap()
            .iter()
            .any(|symbol| symbol["qualified_name"] == "app.homepage"
                && symbol["evidence"].as_array().unwrap().iter().any(|edge| {
                    edge["kind"] == "starlette_route" && edge["certainty"] == "possible"
                })),
        "{report}"
    );

    let cache = tempfile::tempdir().unwrap();
    for _ in 0..2 {
        let mut cached = scan_with_settings(root, None, Some(cache.path()));
        cached.as_object_mut().unwrap().remove("cache");
        assert_eq!(cached, report);
    }

    fs::write(&path, source.replace("route=homepage", "endpoint=homepage")).unwrap();
    let invalid = scan_with_exit(root, None, Some(cache.path()), 2);
    assert!(
        invalid["coverage"]["issues"]
            .as_array()
            .unwrap()
            .iter()
            .any(|issue| {
                issue["kind"] == "unresolved_registration"
                    && issue["path"] == "app.py"
                    && issue["line"] == 7
            }),
        "{invalid}"
    );
    // endpoint is accepted by Router.add_route. Starlette.add_route rejects
    // it before registering the application callback.
    assert!(
        !invalid["dead_code_evidence"]["symbols"]
            .as_array()
            .unwrap()
            .iter()
            .any(|symbol| {
                symbol["qualified_name"] == "app.homepage"
                    && symbol["evidence"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|edge| edge["kind"] == "starlette_route")
            }),
        "{invalid}"
    );

    fs::write(
        &path,
        source.replace(
            "app.add_route(\"/\"",
            "def replacement(path, route, methods): return None\napp.add_route = replacement\napp.add_route(\"/\"",
        ),
    )
    .unwrap();
    let replaced = scan_with_settings(root, None, Some(cache.path()));
    assert!(
        !replaced["dead_code_evidence"]["symbols"]
            .as_array()
            .unwrap()
            .iter()
            .any(|symbol| symbol["qualified_name"] == "app.homepage"
                && symbol["evidence"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|edge| { edge["kind"] == "starlette_route" })),
        "{replaced}"
    );
}

#[test]
fn aiohttp_route_table_captures_handler_without_running_it() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    let source = r#"from aiohttp import web

routes = web.RouteTableDef()

def helper(): return "ok"

@routes.get("/")
async def endpoint(request): return helper()
"#;
    let path = root.join("routes.py");
    fs::write(&path, source).unwrap();
    let report = scan(root);
    assert_eq!(findings(&report, "unused_functions"), ["routes.helper"]);
    let symbols = report["dead_code_evidence"]["symbols"].as_array().unwrap();
    assert!(symbols.iter().any(|symbol| {
        symbol["qualified_name"] == "routes.endpoint"
            && symbol["evidence"]
                .as_array()
                .unwrap()
                .iter()
                .any(|edge| edge["kind"] == "aiohttp_route_declaration")
    }));

    let cache = tempfile::tempdir().unwrap();
    for _ in 0..2 {
        let mut cached = scan_with_settings(root, None, Some(cache.path()));
        cached.as_object_mut().unwrap().remove("cache");
        assert_eq!(cached, report);
    }
    fs::write(
        &path,
        source.replace(
            "@routes.get",
            "routes.get = lambda path: lambda callback: callback\n@routes.get",
        ),
    )
    .unwrap();
    let replaced = scan_with_settings(root, None, Some(cache.path()));
    assert_eq!(
        findings(&replaced, "unused_functions"),
        ["routes.endpoint", "routes.helper"]
    );
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
        unreachable_functions(&report),
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
fn flask_factory_calls_preserve_independent_app_identity_through_alias_writes() {
    let project = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    fs::write(
        project.path().join("provider.py"),
        "from flask import Flask as Application\n\
         def create_app():\n    app = Application(__name__)\n    return app\n\
         def create_direct():\n    return Application(__name__)\n",
    )
    .unwrap();
    fs::write(
        project.path().join("web.py"),
        r#"from provider import create_app, create_direct

first = create_app()
second = create_app()
direct = create_direct()

@first.shell_context_processor
def existing(): return {}

alias = first
alias.shell_context_processor = lambda callback: callback

@first.shell_context_processor
def detached(): return {}

@second.shell_context_processor
def independent(): return {}

@direct.shell_context_processor
def direct_hook(): return {}
"#,
    )
    .unwrap();
    let report = scan(project.path());
    assert_eq!(findings(&report, "unused_functions"), ["web.detached"]);
    let mut cached = scan_with_settings(project.path(), None, Some(cache.path()));
    cached.as_object_mut().unwrap().remove("cache");
    assert_eq!(report, cached);
}

#[test]
fn flask_factory_preserves_aliases_and_no_argument_defaults() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    let cache = tempfile::tempdir().unwrap();
    fs::write(
        root.join("provider.py"),
        "from flask import Flask
def create_app(config=None):
    source = Flask(__name__)
    alias = source
    final = alias
    return final
",
    )
    .unwrap();
    fs::write(
        root.join("web.py"),
        "from provider import create_app
first = create_app()
second = create_app()
alias = first
alias.shell_context_processor = lambda callback: callback
@first.shell_context_processor
def dormant(): return {}
@second.shell_context_processor
def served(): return {}
",
    )
    .unwrap();
    let report = scan(root);
    assert_eq!(
        findings(&report, "unused_functions"),
        ["web.dormant"],
        "{report}"
    );
    let mut cached = scan_with_settings(root, None, Some(cache.path()));
    cached.as_object_mut().unwrap().remove("cache");
    assert_eq!(report, cached);
}

#[test]
fn flask_config_and_deferred_factory_results_cannot_register_callbacks() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    let cache = tempfile::tempdir().unwrap();
    let file = root.join("web.py");
    for (setup, decorator) in [
        (
            "from flask import Flask
app = Flask(__name__)
config = app.config
",
            "config.shell_context_processor",
        ),
        (
            "from flask import Flask
async def create(): return Flask(__name__)
config = create()
",
            "config.shell_context_processor",
        ),
        (
            "from flask_login import LoginManager
login = LoginManager()
",
            "login.config.user_loader",
        ),
    ] {
        fs::write(
            &file,
            format!(
                "{setup}def helper(): return 1
@{decorator}
def dead(): return helper()
"
            ),
        )
        .unwrap();
        let report = scan(root);
        let unused = findings(&report, "unused_functions");
        for name in ["web.dead", "web.helper"] {
            assert!(unused.contains(&name), "{report}");
        }
        let mut cached = scan_with_settings(root, None, Some(cache.path()));
        cached.as_object_mut().unwrap().remove("cache");
        assert_eq!(report, cached);
    }
    fs::write(
        root.join("provider.py"),
        "from flask import Flask
def create():
    app = Flask(__name__)
    return app.config
",
    )
    .unwrap();
    fs::write(
        &file,
        "from provider import create
config = create()
@config.shell_context_processor
def dead(): return {}
",
    )
    .unwrap();
    let report = scan(root);
    assert_eq!(
        findings(&report, "unused_functions"),
        ["web.dead"],
        "{report}"
    );
    // An unresolved effect could supply a method on the configuration object.
    fs::write(
        &file,
        "from flask import Flask
app = Flask(__name__)
config = app.config
opaque(config)
@config.shell_context_processor
def callback(): return {}
",
    )
    .unwrap();
    let report = scan_with_exit(root, None, None, 2);
    assert!(
        report["analysis_errors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|issue| issue["kind"] == "unresolved_decorator"),
        "{report}"
    );
    fs::write(
        &file,
        "from flask import Flask\napp = Flask(__name__)\nhandler_owner = Flask(__name__)\napp.config = handler_owner\n@app.config.shell_context_processor\ndef active(): return {}\n",
    ).unwrap();
    let report = scan(root);
    assert_eq!(
        findings(&report, "unused_functions"),
        ["provider.create"],
        "{report}"
    );
    assert!(
        report["dead_code_evidence"]["symbols"]
            .as_array()
            .unwrap()
            .iter()
            .any(|symbol| {
                symbol["qualified_name"] == "web.active"
                    && symbol["evidence"].as_array().unwrap().iter().any(|edge| {
                        edge["kind"] == "flask_registration" && edge["certainty"] == "certain"
                    })
            }),
        "{report}"
    );
}

#[test]
fn flask_login_consumer_fails_before_exposing_the_manager_to_an_unbound_name() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    let web = root.join("web.py");
    fs::write(
        root.join("providers.py"),
        "from flask_login import LoginManager\nlogin = LoginManager()\n",
    )
    .unwrap();
    let setup = "from providers import login\nvalues = {'manager': login}; opaque(values)\n";
    let callback = "@login.user_loader\ndef callback(user_id): return user_id\n";
    fs::write(&web, format!("{setup}{callback}")).unwrap();
    let report = scan(root);
    assert_eq!(findings(&report, "unused_functions"), ["web.callback"]);

    let importer = root.join("consumer.py");
    fs::write(&importer, "from web import callback\n").unwrap();
    let report = scan_with_exit(root, None, None, 2);
    assert!(
        report["analysis_errors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|error| {
                error["file"] == "consumer.py" && error["kind"] == "unresolved_import"
            }),
        "{report}"
    );
    fs::remove_file(importer).unwrap();

    fs::write(&web, format!("def opaque(values): pass\n{setup}{callback}")).unwrap();
    let report = scan(root);
    assert!(findings(&report, "unused_functions").is_empty(), "{report}");
}

#[test]
fn flask_global_initializer_stops_only_when_its_replacement_is_unbound_at_call_time() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    let web = root.join("web.py");
    let cache = tempfile::tempdir().unwrap();
    fs::write(
        root.join("providers.py"),
        "from flask_login import LoginManager\nlogin = LoginManager()\n",
    )
    .unwrap();
    for (setup, global, callback) in [
        (
            "from providers import login\n",
            "login",
            "@login.user_loader\ndef callback(user_id): return user_id\n",
        ),
        (
            "from providers import login\nalias = login\n",
            "alias",
            "@alias.user_loader\ndef callback(user_id): return user_id\n",
        ),
        (
            "from flask_login import LoginManager\n",
            "LoginManager",
            "login = LoginManager()\n@login.user_loader\ndef callback(user_id): return user_id\n",
        ),
    ] {
        let helper = format!("def replace():\n    global {global}\n    {global} = replacement\n");
        fs::write(&web, format!("{setup}{helper}replace()\n{callback}")).unwrap();
        let report = scan(root);
        assert_eq!(
            findings(&report, "unused_functions"),
            ["web.callback"],
            "{report}"
        );
        let mut cached = scan_with_settings(root, None, Some(cache.path()));
        cached.as_object_mut().unwrap().remove("cache");
        assert_eq!(report, cached);
    }

    let importer = root.join("consumer.py");
    fs::write(&importer, "from web import callback\n").unwrap();
    let report = scan_with_exit(root, None, None, 2);
    assert!(
        report["analysis_errors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|error| {
                error["file"] == "consumer.py" && error["kind"] == "unresolved_import"
            }),
        "{report}"
    );
    fs::remove_file(importer).unwrap();

    fs::write(
        &web,
        "from providers import login\ndef replace():\n    global login\n    login = replacement\nreplacement = login\nreplace()\n@login.user_loader\ndef callback(user_id): return user_id\n",
    )
    .unwrap();
    let report = scan_with_exit(root, None, None, 2);
    assert!(
        report["analysis_errors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|error| { error["file"] == "web.py" && error["kind"] == "unsupported_construct" }),
        "{report}"
    );
}

#[test]
fn flask_opaque_hook_can_change_a_receiver_referenced_by_an_escaped_function() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    let cache = tempfile::tempdir().unwrap();
    fs::write(
        root.join("providers.py"),
        "from flask_login import LoginManager\nlogin = LoginManager()\nother = LoginManager()\n",
    )
    .unwrap();
    let app = root.join("app.py");
    let registered = |report: &Value| {
        report["dead_code_evidence"]["symbols"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|symbol| {
                symbol["evidence"].as_array().unwrap().iter().any(|edge| {
                    edge["kind"] == "flask_login_registration" && edge["certainty"] == "certain"
                })
            })
            .map(|symbol| symbol["qualified_name"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>()
    };
    let callbacks = "@login.user_loader\ndef after(user_id): return user_id\n@other.user_loader\ndef independent(user_id): return user_id\n";
    let patch = "def replacement(callback): return callback\ndef replace():\n    target = login\n    target.user_loader = replacement\n";
    let imports = "from providers import login, other\nfrom extension import customize\n";
    let before = "@login.user_loader\ndef before(user_id): return user_id\n";
    fs::write(
        &app,
        format!("{imports}{before}{patch}customize(callbacks={{'patch': replace}})\n{callbacks}"),
    )
    .unwrap();
    let report = scan_with_exit(root, None, None, 2);
    assert_eq!(
        registered(&report),
        ["app.before", "app.independent"],
        "{report}"
    );
    assert!(
        report["analysis_errors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|error| { error["file"] == "app.py" && error["kind"] == "unresolved_call" }),
        "{report}"
    );
    let mut cached = scan_with_exit(root, None, Some(cache.path()), 2);
    cached.as_object_mut().unwrap().remove("cache");
    assert_eq!(report, cached);

    fs::write(
        root.join("patches.py"),
        format!("from providers import login\n{patch}"),
    )
    .unwrap();
    fs::write(
        &app,
        format!("{imports}from patches import replace\n{before}customize(replace)\n{callbacks}"),
    )
    .unwrap();
    let report = scan_with_exit(root, None, None, 2);
    assert_eq!(
        registered(&report),
        ["app.before", "app.independent"],
        "{report}"
    );

    fs::write(
        &app,
        format!("{imports}{before}def replace(login): pass\ncustomize(replace)\n{callbacks}"),
    )
    .unwrap();
    let report = scan_with_exit(root, None, None, 2);
    assert_eq!(
        registered(&report),
        ["app.after", "app.before", "app.independent"],
        "{report}"
    );

    for (parameter, target, expected) in [
        ("", "selected", ["app.after", "app.before"]),
        (
            "target=selected",
            "target",
            ["app.before", "app.independent"],
        ),
    ] {
        fs::write(
            &app,
            format!(
                "{imports}{before}selected = login\ndef replace({parameter}):\n    {target}.user_loader = lambda callback: callback\nselected = other\ncustomize(replace)\n{callbacks}"
            ),
        )
        .unwrap();
        let report = scan_with_exit(root, None, None, 2);
        assert_eq!(registered(&report), expected, "{report}");
    }
}

#[test]
fn flask_failed_patch_leaves_an_independent_shell_context_callback_registered() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    let setup = "from flask import Flask\napp = Flask(__name__)\n";
    fs::write(
        root.join("providers.py"),
        format!("{setup}Flask.shell_context_processor = replacement\n"),
    )
    .unwrap();
    fs::write(
        root.join("consumer.py"),
        "from providers import app\n@app.shell_context_processor\ndef deferred(): return {}\n",
    )
    .unwrap();
    fs::write(
        root.join("active.py"),
        format!("{setup}@app.shell_context_processor\ndef shell(): return {{}}\n"),
    )
    .unwrap();
    let report = scan_with_exit(root, None, None, 2);
    let errors = report["analysis_errors"].as_array().unwrap();
    assert_eq!(errors.len(), 1, "{report}");
    assert_eq!(errors[0]["file"], "consumer.py", "{report}");
    assert_eq!(errors[0]["kind"], "unresolved_import", "{report}");
    let symbols = report["dead_code_evidence"]["symbols"].as_array().unwrap();
    assert!(
        symbols
            .iter()
            .any(|symbol| symbol["qualified_name"] == "active.shell"
                && symbol["evidence"].as_array().unwrap().iter().any(|edge| {
                    edge["kind"] == "flask_registration" && edge["certainty"] == "certain"
                })),
        "{report}"
    );
    assert!(
        !symbols
            .iter()
            .any(|symbol| symbol["qualified_name"] == "consumer.deferred"),
        "{report}"
    );
}

#[test]
fn flask_failed_class_patches_leave_independent_receivers_unchanged() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    let cache = tempfile::tempdir().unwrap();
    let providers = root.join("providers.py");
    let manager = "from flask_login import LoginManager\nlogin = LoginManager()\n";
    fs::write(
        root.join("active.py"),
        format!("{manager}@login.user_loader\ndef active(user_id): return user_id\n"),
    )
    .unwrap();
    fs::write(
        root.join("consumer.py"),
        "from providers import login\n@login.user_loader\ndef deferred(user_id): return user_id\n",
    )
    .unwrap();

    for failed in [
        "LoginManager.user_loader = replacement",
        "LoginManager.__new__ = replacement",
        "stop = replacement\nLoginManager.user_loader = lambda self: self",
    ] {
        fs::write(&providers, format!("{manager}{failed}\n")).unwrap();
        let report = scan_with_exit(root, None, None, 2);
        let errors = report["analysis_errors"].as_array().unwrap();
        assert_eq!(errors.len(), 1, "{failed}\n{report}");
        assert_eq!(errors[0]["file"], "consumer.py", "{failed}\n{report}");
        assert_eq!(errors[0]["kind"], "unresolved_import", "{failed}\n{report}");
        let symbols = report["dead_code_evidence"]["symbols"].as_array().unwrap();
        assert!(
            symbols
                .iter()
                .any(|symbol| symbol["qualified_name"] == "active.active"
                    && symbol["evidence"].as_array().unwrap().iter().any(|edge| {
                        edge["kind"] == "flask_login_registration" && edge["certainty"] == "certain"
                    })),
            "{failed}\n{report}"
        );
        assert!(
            !symbols
                .iter()
                .any(|symbol| symbol["qualified_name"] == "consumer.deferred"),
            "{failed}\n{report}"
        );
        let mut cached = scan_with_exit(root, None, Some(cache.path()), 2);
        cached.as_object_mut().unwrap().remove("cache");
        assert_eq!(report, cached);
    }

    fs::write(
        &providers,
        format!("{manager}def replacement(self, callback): return callback\nLoginManager.user_loader = replacement\n"),
    ).unwrap();
    let report = scan_with_exit(root, None, None, 2);
    assert!(
        !report["analysis_errors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|error| { error["kind"] == "unresolved_import" }),
        "{report}"
    );
    assert!(
        !report["dead_code_evidence"]["symbols"]
            .as_array()
            .unwrap()
            .iter()
            .any(|symbol| {
                symbol["qualified_name"] == "active.active"
                    && symbol["evidence"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|edge| edge["kind"] == "flask_login_registration")
            }),
        "{report}"
    );
}

#[test]
fn flask_login_allocator_replacement_resolves_the_class_at_call_time() {
    let project = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let root = project.path();
    let provider = root.join("providers.py");
    let setup = r#"from flask_login import LoginManager
control = LoginManager()
def allocate(cls): return Selected()
LoginManager.__new__ = allocate
class Selected:
    @staticmethod
    def user_loader(callback): return callback
"#;
    fs::write(&provider, format!("{setup}login = LoginManager()\n")).unwrap();
    fs::write(
        root.join("app.py"),
        "from providers import login, control\n@login.user_loader\ndef replaced(value): return value\n@control.user_loader\ndef active(user_id): return user_id\n",
    )
    .unwrap();
    let report = scan(root);
    assert_eq!(
        evidence_certainties(&report, "providers.allocate", "flask_login_allocator"),
        ["certain"]
    );
    assert_eq!(
        evidence_certainties(&report, "providers.Selected.user_loader", "decorator"),
        ["certain"]
    );
    assert_eq!(
        evidence_certainties(&report, "app.active", "flask_login_registration"),
        ["certain"]
    );
    assert!(findings(&report, "unused_functions").contains(&"app.replaced"));
    let mut cached = scan_with_settings(root, None, Some(cache.path()));
    cached.as_object_mut().unwrap().remove("cache");
    assert_eq!(report, cached);

    fs::write(&provider, format!("{setup}login = LoginManager(None)\n")).unwrap();
    let report = scan_with_exit(root, None, Some(cache.path()), 2);
    assert!(
        report["analysis_errors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|issue| issue["kind"] == "unresolved_constructor")
    );
    assert!(evidence_certainties(&report, "app.replaced", "flask_login_registration").is_empty());
}

#[test]
fn flask_login_class_change_uses_the_replacement_layout_and_leaves_siblings_active() {
    let project = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let root = project.path();
    let provider = root.join("providers.py");
    let setup =
        "from flask_login import LoginManager\nlogin = LoginManager()\ncontrol = LoginManager()\n";
    let members = "    @staticmethod\n    def user_loader(callback): return callback\n";
    let mutation = "login.__class__ = Replacement\n";
    fs::write(
        &provider,
        format!("{setup}class Replacement:\n{members}{mutation}"),
    )
    .unwrap();
    fs::write(
        root.join("app.py"),
        "from providers import login, control\n@login.user_loader\ndef replaced(value): return value\n@control.user_loader\ndef active(user_id): return user_id\n",
    )
    .unwrap();
    let report = scan(root);
    assert_eq!(
        evidence_certainties(&report, "app.active", "flask_login_registration"),
        ["certain"]
    );
    assert!(findings(&report, "unused_functions").contains(&"app.replaced"));
    let mut cached = scan_with_settings(root, None, Some(cache.path()));
    cached.as_object_mut().unwrap().remove("cache");
    assert_eq!(report, cached);

    fs::write(
        &provider,
        format!("{setup}class Replacement:\n    __slots__ = ()\n{members}{mutation}"),
    )
    .unwrap();
    let report = scan_with_exit(root, None, Some(cache.path()), 2);
    assert_eq!(
        evidence_certainties(&report, "app.active", "flask_login_registration"),
        ["certain"]
    );
    assert!(
        report["analysis_errors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|issue| issue["kind"] == "unresolved_decorator")
    );
}

#[test]
fn flask_login_code_replacement_affects_captured_methods_only_after_the_write() {
    let project = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let root = project.path();
    let provider = root.join("providers.py");
    let setup = r#"from flask_login import LoginManager
login = LoginManager()
saved = login.user_loader
@saved
def before(user_id): return user_id
"#;
    let write = "LoginManager.user_loader.__code__ = replacement.__code__\n";
    fs::write(
        &provider,
        format!("{setup}def replacement(self, callback): return callback\n{write}"),
    )
    .unwrap();
    fs::write(
        root.join("app.py"),
        "from providers import login, saved\n@login.user_loader\ndef replaced(user_id): return user_id\n@saved\ndef captured(user_id): return user_id\n@login.request_loader\ndef active(request): return request\n",
    )
    .unwrap();
    let report = scan(root);
    for active in ["providers.before", "app.active"] {
        assert_eq!(
            evidence_certainties(&report, active, "flask_login_registration"),
            ["certain"]
        );
    }
    for unused in ["app.captured", "app.replaced"] {
        assert!(findings(&report, "unused_functions").contains(&unused));
    }
    assert_eq!(
        evidence_certainties(&report, "providers.replacement", "code_reference"),
        ["certain"]
    );
    assert!(evidence_certainties(&report, "providers.replacement", "direct_call").is_empty());
    assert!(!findings(&report, "unused_functions").contains(&"providers.replacement"));
    for _ in 0..2 {
        let mut cached = scan_with_settings(root, None, Some(cache.path()));
        cached.as_object_mut().unwrap().remove("cache");
        assert_eq!(report, cached);
    }

    fs::write(
        &provider,
        format!("{setup}def replacement(callback): return callback\n{write}"),
    )
    .unwrap();
    let report = scan_with_exit(root, None, Some(cache.path()), 2);
    assert!(
        report["analysis_errors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|issue| issue["kind"] == "unresolved_attribute_write")
    );
}

#[test]
fn flask_login_literal_mock_start_replaces_a_resolved_callback_decorator() {
    let project = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let root = project.path();
    let provider = root.join("providers.py");
    let setup = "from flask_login import LoginManager\nfrom unittest.mock import patch\nlogin = LoginManager()\n";
    fs::write(
        root.join("app.py"),
        "from providers import login\n@login.user_loader\ndef replaced(value): return value\n@login.request_loader\ndef active(request): return request\n",
    )
    .unwrap();
    for start in [
        "def replacement(callback): return callback\npatch('providers.login.user_loader', replacement).start()\n",
        "def replacement(self, callback): return callback\npatch.object(LoginManager, 'user_loader', replacement).start()\n",
    ] {
        fs::write(&provider, format!("{setup}{start}")).unwrap();
        let report = scan(root);
        assert_eq!(
            evidence_certainties(&report, "providers.replacement", "decorator"),
            ["certain"]
        );
        assert_eq!(
            evidence_certainties(&report, "app.active", "flask_login_registration"),
            ["certain"]
        );
        assert!(
            evidence_certainties(&report, "app.replaced", "flask_login_registration").is_empty()
        );
        assert!(findings(&report, "unused_functions").contains(&"app.replaced"));
        let mut cached = scan_with_settings(root, None, Some(cache.path()));
        cached.as_object_mut().unwrap().remove("cache");
        assert_eq!(cached, report);

        fs::write(&provider, format!("{setup}{start}patch.stopall()\n")).unwrap();
        let report = scan_with_exit(root, None, Some(cache.path()), 2);
        assert!(
            report["analysis_errors"]
                .as_array()
                .unwrap()
                .iter()
                .any(|issue| issue["kind"] == "unresolved_call")
        );
    }
}

#[test]
fn flask_login_instance_dictionary_replaces_only_the_selected_callback_decorator() {
    let project = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let root = project.path();
    let provider = root.join("providers.py");
    let setup = "from flask_login import LoginManager\nlogin = LoginManager()\n";
    let write =
        "def identity(callback): return callback\nlogin.__dict__['user_loader'] = identity\n";
    fs::write(&provider, format!("{setup}{write}")).unwrap();
    fs::write(
        root.join("app.py"),
        "from providers import login\n@login.user_loader\ndef replaced(value): return value\n@login.request_loader\ndef active(request): return request\n",
    )
    .unwrap();

    let report = scan(root);
    assert_eq!(
        evidence_certainties(&report, "providers.identity", "decorator"),
        ["certain"]
    );
    assert_eq!(
        evidence_certainties(&report, "app.active", "flask_login_registration"),
        ["certain"]
    );
    assert!(evidence_certainties(&report, "app.replaced", "flask_login_registration").is_empty());
    assert!(findings(&report, "unused_functions").contains(&"app.replaced"));
    for _ in 0..2 {
        let mut cached = scan_with_settings(root, None, Some(cache.path()));
        cached.as_object_mut().unwrap().remove("cache");
        assert_eq!(cached, report);
    }

    fs::write(
        &provider,
        "class LoginManager: pass\nlogin = LoginManager()\ndef identity(callback): return callback\nlogin.__dict__['user_loader'] = identity\n",
    )
    .unwrap();
    let mut report = scan_with_exit(root, None, Some(cache.path()), 2);
    report.as_object_mut().unwrap().remove("cache");
    assert_eq!(report, scan_with_exit(root, None, None, 2));
    assert!(
        report["analysis_errors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|issue| issue["kind"] == "unresolved_attribute_write")
    );
}

#[test]
fn flask_login_class_replacement_binds_the_local_identity_decorator() {
    let project = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let root = project.path();
    let provider = root.join("providers.py");
    fs::write(
        root.join("app.py"),
        "from providers import login\n@login.user_loader\ndef callback(value): return value\n",
    )
    .unwrap();
    let setup = "from flask_login import LoginManager\nlogin = LoginManager()\n";
    let patch = "LoginManager.user_loader = prepare\n";
    fs::write(
        &provider,
        format!("{setup}def prepare(self, callback): return callback\n{patch}"),
    )
    .unwrap();

    let report = scan(root);
    assert_eq!(
        evidence_certainties(&report, "providers.prepare", "decorator"),
        ["certain"]
    );
    assert!(evidence_certainties(&report, "app.callback", "flask_login_registration").is_empty());
    assert!(findings(&report, "unused_functions").contains(&"app.callback"));
    for _ in 0..2 {
        let mut cached = scan_with_settings(root, None, Some(cache.path()));
        cached.as_object_mut().unwrap().remove("cache");
        assert_eq!(cached, report);
    }

    fs::write(
        &provider,
        format!("{setup}def prepare(self, callback): return callback(1)\n{patch}"),
    )
    .unwrap();
    let report = scan_with_exit(root, None, Some(cache.path()), 2);
    assert!(
        report["analysis_errors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|issue| issue["kind"] == "unresolved_attribute_write")
    );
    assert!(evidence_certainties(&report, "app.callback", "flask_login_registration").is_empty());

    fs::write(&provider, setup).unwrap();
    let report = scan_with_settings(root, None, Some(cache.path()));
    assert_eq!(
        evidence_certainties(&report, "app.callback", "flask_login_registration"),
        ["certain"]
    );
}

#[test]
fn flask_failed_provider_import_cannot_supply_a_callback() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    let importer = root.join("app.py");
    let providers = root.join("providers.py");
    let cache = tempfile::tempdir().unwrap();
    let setup = "from flask_login import LoginManager\nlogin = LoginManager()\n";
    let callback =
        "from providers import login\n@login.user_loader\ndef callback(user_id): return user_id\n";
    fs::write(&importer, callback).unwrap();

    for failure in [
        "login.__dict__['user_loader'] = replacement\n",
        "LoginManager.user_loader.__code__ = replacement.__code__\n",
        "from unittest.mock import patch\npatch('providers.login.user_loader', replacement).start()\n",
        "from unittest.mock import patch\npatch.object(LoginManager, 'user_loader', replacement).start()\n",
        "globals().update({'login': replacement})\n",
        "def replace():\n    global login\n    login = replacement\nreplace()\n",
    ] {
        fs::write(&providers, format!("{setup}{failure}")).unwrap();
        let report = scan_with_exit(root, None, None, 2);
        assert!(
            !report["dead_code_evidence"]["symbols"]
                .as_array()
                .unwrap()
                .iter()
                .any(|symbol| {
                    symbol["qualified_name"] == "app.callback"
                        && symbol["evidence"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .any(|edge| edge["kind"] == "flask_login_registration")
                }),
            "{failure}\n{report}"
        );
        assert!(
            report["analysis_errors"]
                .as_array()
                .unwrap()
                .iter()
                .any(|error| { error["file"] == "app.py" && error["kind"] == "unresolved_import" }),
            "{failure}\n{report}"
        );
        let mut cached = scan_with_exit(root, None, Some(cache.path()), 2);
        cached.as_object_mut().unwrap().remove("cache");
        assert_eq!(report, cached);
    }

    fs::write(
        &providers,
        format!("{setup}from extension import replacement\nlogin.__dict__['user_loader'] = replacement\n"),
    )
    .unwrap();
    let report = scan_with_exit(root, None, None, 2);
    assert!(
        !report["analysis_errors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|error| { error["file"] == "app.py" && error["kind"] == "unresolved_import" }),
        "{report}"
    );

    fs::write(&providers, setup).unwrap();
    let report = scan(root);
    assert!(
        report["dead_code_evidence"]["symbols"]
            .as_array()
            .unwrap()
            .iter()
            .any(|symbol| {
                symbol["qualified_name"] == "app.callback"
                    && symbol["evidence"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|edge| edge["kind"] == "flask_login_registration")
            }),
        "{report}"
    );
}

#[test]
fn flask_failed_provider_imports_stop_an_ordered_chain_of_reexports() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    let cache = tempfile::tempdir().unwrap();
    let provider = root.join("provider.py");
    let wrapper = root.join("wrapper.py");
    let bridge = root.join("bridge.py");
    let app = root.join("app.py");
    let setup = "from flask_login import LoginManager\nlogin = LoginManager()\n";
    fs::write(
        &provider,
        format!("{setup}login.__dict__['user_loader'] = replacement\n"),
    )
    .unwrap();
    fs::write(&wrapper, "from provider import login\n").unwrap();
    fs::write(&bridge, "from wrapper import login\n").unwrap();
    let callback = "@bridge.login.user_loader\ndef callback(user_id): return user_id\n";
    fs::write(&app, format!("import bridge\n{callback}")).unwrap();

    let report = scan_with_exit(root, None, None, 2);
    let imports: Vec<_> = report["analysis_errors"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|error| error["kind"] == "unresolved_import")
        .map(|error| error["file"].as_str().unwrap())
        .collect();
    assert_eq!(imports, ["app.py", "bridge.py", "wrapper.py"], "{report}");
    assert!(
        !report["dead_code_evidence"]["symbols"]
            .as_array()
            .unwrap()
            .iter()
            .any(|symbol| { symbol["qualified_name"] == "app.callback" }),
        "{report}"
    );
    let mut cached = scan_with_exit(root, None, Some(cache.path()), 2);
    cached.as_object_mut().unwrap().remove("cache");
    assert_eq!(report, cached);

    fs::write(
        &wrapper,
        "from extension import prepare_imports\nprepare_imports()\nfrom provider import login\n",
    )
    .unwrap();
    let report = scan_with_exit(root, None, None, 2);
    let errors = report["analysis_errors"].as_array().unwrap();
    assert!(
        errors
            .iter()
            .any(|error| { error["file"] == "wrapper.py" && error["kind"] == "unresolved_call" }),
        "{report}"
    );
    assert!(
        !errors
            .iter()
            .any(|error| error["kind"] == "unresolved_import"),
        "{report}"
    );

    fs::write(&wrapper, "from provider import login\n").unwrap();
    fs::write(&provider, setup).unwrap();
    let report = scan(root);
    assert!(findings(&report, "unused_functions").is_empty(), "{report}");
    assert!(
        report["dead_code_evidence"]["symbols"]
            .as_array()
            .unwrap()
            .iter()
            .any(|symbol| {
                symbol["qualified_name"] == "app.callback"
                    && symbol["evidence"].as_array().unwrap().iter().any(|edge| {
                        edge["kind"] == "flask_login_registration" && edge["certainty"] == "certain"
                    })
            }),
        "{report}"
    );
}

#[test]
fn flask_circular_named_exports_report_failed_imports_without_resolving_final_bindings() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    fs::write(root.join("providers.py"), "from other import login\n").unwrap();
    let other = root.join("other.py");
    fs::write(&other, "from providers import login\n").unwrap();
    fs::write(
        root.join("app.py"),
        "from providers import login\n@login.user_loader\ndef callback(user_id): return user_id\n",
    )
    .unwrap();
    let report = scan_with_exit(root, None, None, 2);
    let errors = report["analysis_errors"].as_array().unwrap();
    assert!(
        errors
            .iter()
            .all(|error| error["kind"] == "unresolved_import"),
        "{report}"
    );
    assert_eq!(
        errors
            .iter()
            .map(|error| error["file"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["app.py", "other.py", "providers.py"],
        "{report}"
    );
    assert!(
        !report["dead_code_evidence"]["symbols"]
            .as_array()
            .unwrap()
            .iter()
            .any(|symbol| { symbol["qualified_name"] == "app.callback" }),
        "{report}"
    );

    fs::write(
        &other,
        "from flask_login import LoginManager\nlogin = LoginManager()\n",
    )
    .unwrap();
    let report = scan(root);
    assert!(
        report["dead_code_evidence"]["symbols"]
            .as_array()
            .unwrap()
            .iter()
            .any(|symbol| {
                symbol["qualified_name"] == "app.callback"
                    && symbol["evidence"].as_array().unwrap().iter().any(|edge| {
                        edge["kind"] == "flask_login_registration" && edge["certainty"] == "certain"
                    })
            }),
        "{report}"
    );
}

#[test]
fn circular_named_exports_reuse_the_same_published_binding_in_either_order() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    let cache = tempfile::tempdir().unwrap();
    fs::write(
        root.join("origin.py"),
        "class Replacement:\n    @staticmethod\n    def user_loader(callback): return callback\nlogin = Replacement()\n",
    )
    .unwrap();
    fs::write(
        root.join("providers.py"),
        "from origin import login\nfrom other import login\n",
    )
    .unwrap();
    let peer = root.join("other.py");
    fs::write(
        &peer,
        "from origin import login\nfrom providers import login\n",
    )
    .unwrap();
    let app = root.join("app.py");
    fs::write(
        &app,
        "from providers import login\n@login.user_loader\ndef callback(): return 1\n",
    )
    .unwrap();
    let report = scan(root);
    assert_eq!(findings(&report, "unused_functions"), ["app.callback"]);
    for _ in 0..2 {
        let mut cached = scan_with_settings(root, None, Some(cache.path()));
        cached.as_object_mut().unwrap().remove("cache");
        assert_eq!(cached, report);
    }
    fs::write(
        &app,
        "from other import login\n@login.user_loader\ndef callback(): return 1\n",
    )
    .unwrap();
    let entered_through_peer = scan_with_settings(root, None, Some(cache.path()));
    assert_eq!(
        findings(&entered_through_peer, "unused_functions"),
        ["app.callback"]
    );
    fs::write(&peer, "from providers import login\n").unwrap();
    let incomplete = scan_with_exit(root, None, Some(cache.path()), 2);
    assert!(
        incomplete["analysis_errors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|issue| {
                matches!(
                    issue["kind"].as_str(),
                    Some("cyclic_imports" | "unresolved_import")
                )
            }),
        "{incomplete}"
    );
}

#[test]
fn flask_factory_stops_at_the_failed_blueprint_export() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    fs::create_dir_all(root.join("app/views")).unwrap();
    fs::write(
        root.join("app/__init__.py"),
        "from flask import Flask\ndef create_app():\n    app = Flask(__name__)\n    from app.views import blueprint\n    app.register_blueprint(blueprint)\n    return app\n",
    )
    .unwrap();
    let views = root.join("app/views/__init__.py");
    let setup = "from flask import Blueprint\nblueprint = Blueprint('main', __name__)\n";
    fs::write(
        root.join("app/views/routes.py"),
        "from app.views import blueprint\n@blueprint.before_app_request\ndef prepare(): return helper()\ndef helper(): return 1\ndef retired(): return 2\n",
    )
    .unwrap();
    fs::write(
        &views,
        format!("{setup}blueprint = replacement\nfrom app.views import routes\n"),
    )
    .unwrap();
    let report = scan_with_exit(root, None, None, 2);
    let errors = report["analysis_errors"].as_array().unwrap();
    assert!(
        errors.iter().any(|error| {
            error["file"] == "app/views/routes.py" && error["kind"] == "unresolved_import"
        }),
        "{report}"
    );
    assert!(
        errors
            .iter()
            .all(|error| error["kind"] == "unresolved_import"),
        "{report}"
    );
    assert!(
        !report["dead_code_evidence"]["symbols"]
            .as_array()
            .unwrap()
            .iter()
            .any(|symbol| { symbol["qualified_name"] == "app.views.routes.prepare" }),
        "{report}"
    );

    fs::write(root.join("lookalike.py"), "class Blueprint: pass\n").unwrap();
    fs::write(
        &views,
        "from lookalike import Blueprint\nblueprint = Blueprint('main', __name__)\nfrom app.views import routes\n",
    ).unwrap();
    let report = scan_with_exit(root, None, None, 2);
    let errors = report["analysis_errors"].as_array().unwrap();
    assert!(
        errors.iter().any(|error| {
            error["file"] == "app/views/routes.py" && error["kind"] == "unresolved_import"
        }),
        "{report}"
    );
    assert!(
        errors
            .iter()
            .all(|error| error["kind"] == "unresolved_import"),
        "{report}"
    );

    fs::write(
        root.join("lookalike.py"),
        "class Blueprint:\n    def __init__(self, name, import_name): pass\n    def before_app_request(self, callback): return callback\n",
    ).unwrap();
    let report = scan_with_exit(root, None, None, 2);
    assert!(
        !report["analysis_errors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|error| { error["kind"] == "unresolved_import" }),
        "{report}"
    );

    fs::write(&views, format!("{setup}from app.views import routes\n")).unwrap();
    let report = scan(root);
    assert_eq!(
        findings(&report, "unused_functions"),
        ["app.views.routes.retired"],
        "{report}"
    );
}

#[test]
fn flask_factory_calls_an_inert_replacement_after_initializing_blueprint_routes() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    let cache = tempfile::tempdir().unwrap();
    fs::create_dir_all(root.join("app/views")).unwrap();
    fs::write(
        root.join("app/__init__.py"),
        "from flask import Flask\ndef create_app():\n    app = Flask(__name__)\n    from app.views import bp\n    app.register_blueprint(bp)\n    return app\n",
    )
    .unwrap();
    fs::write(
        root.join("app/views/routes.py"),
        "from app.views import bp\n@bp.before_app_request\ndef prepare(): return helper()\ndef helper(): return 1\ndef retired(): return 2\n",
    )
    .unwrap();
    let views = root.join("app/views/__init__.py");
    let setup = "from flask import Blueprint\nbp = Blueprint('main', __name__)\nfrom app.views import routes\n";
    fs::write(
        &views,
        format!(
            "{setup}class Replacement:\n    def register(self, app, options): pass\nbp = Replacement()\n"
        ),
    )
    .unwrap();
    let report = scan(root);
    assert_eq!(
        findings(&report, "unused_functions"),
        [
            "app.views.routes.helper",
            "app.views.routes.prepare",
            "app.views.routes.retired",
        ],
    );
    assert!(
        evidence_certainties(&report, "app.views.routes.prepare", "flask_registration").is_empty()
    );
    for _ in 0..2 {
        let mut cached_report = scan_with_settings(root, None, Some(cache.path()));
        cached_report.as_object_mut().unwrap().remove("cache");
        assert_eq!(cached_report, report);
    }

    fs::write(
        &views,
        format!("{setup}class Replacement:\n    def register(self, app, options): unknown(app)\nbp = Replacement()\n"),
    )
    .unwrap();
    let incomplete = scan_with_exit(root, None, Some(cache.path()), 2);
    assert!(
        incomplete["analysis_errors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|error| error["kind"] == "unresolved_registration"),
        "{incomplete}"
    );

    fs::write(&views, setup).unwrap();
    let mounted = scan_with_settings(root, None, Some(cache.path()));
    assert_eq!(
        findings(&mounted, "unused_functions"),
        ["app.views.routes.retired"],
    );
}

#[test]
fn flask_factory_resolves_an_inert_lookalike_published_before_its_child_import() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    let cache = tempfile::tempdir().unwrap();
    fs::create_dir_all(root.join("app/views")).unwrap();
    fs::write(
        root.join("app/__init__.py"),
        "from flask import Flask\ndef create_app():\n    app = Flask(__name__)\n    from app.views import bp\n    app.register_blueprint(bp)\n    return app\n",
    )
    .unwrap();
    fs::write(
        root.join("app/views/routes.py"),
        "from app.views import bp\n@bp.before_app_request\ndef prepare(): return helper()\ndef helper(): return 1\ndef retired(): return 2\n",
    )
    .unwrap();
    fs::write(
        root.join("lookalike.py"),
        "class Blueprint:\n    def __init__(self, name, import_name): pass\n    def before_app_request(self, callback): return callback\n    def register(self, app, options): pass\n",
    )
    .unwrap();
    let initializer = root.join("app/views/__init__.py");
    fs::write(
        &initializer,
        "from lookalike import Blueprint\nbp = Blueprint('main', __name__)\nfrom app.views import routes\n",
    )
    .unwrap();
    let report = scan(root);
    assert_eq!(
        findings(&report, "unused_functions"),
        [
            "app.views.routes.helper",
            "app.views.routes.prepare",
            "app.views.routes.retired",
        ],
    );
    for _ in 0..2 {
        let mut cached = scan_with_settings(root, None, Some(cache.path()));
        cached.as_object_mut().unwrap().remove("cache");
        assert_eq!(cached, report);
    }

    fs::write(
        &initializer,
        "from lookalike import Blueprint\nfrom app.views import routes\nbp = Blueprint('main', __name__)\n",
    )
    .unwrap();
    let late = scan_with_exit(root, None, Some(cache.path()), 2);
    assert!(
        late["analysis_errors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|issue| {
                matches!(
                    issue["kind"].as_str(),
                    Some("cyclic_imports" | "unresolved_import")
                )
            }),
        "{late}"
    );
}

#[test]
fn flask_unbound_initializer_stops_before_a_decorator_or_factory_body() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    let cache = tempfile::tempdir().unwrap();
    for (setup, bootstrap) in [
        (
            "app = Flask(__name__)\napp = missing",
            "@app.shell_context_processor\ndef dead(): return {}",
        ),
        (
            "def factory(value):\n    app = Flask(__name__)\n    @app.shell_context_processor\n    def dead(): return {}\n    return app",
            "app = factory(missing)",
        ),
    ] {
        fs::write(
            root.join("web.py"),
            format!("from flask import Flask\n{setup}\n{bootstrap}\n"),
        )
        .unwrap();
        let report = scan(root);
        assert!(
            findings(&report, "unused_functions")
                .iter()
                .any(|name| name.ends_with(".dead")),
            "{report}"
        );
        let mut cached = scan_with_settings(root, None, Some(cache.path()));
        cached.as_object_mut().unwrap().remove("cache");
        assert_eq!(report, cached);
    }
    fs::write(root.join("consumer.py"), "from web import app\n").unwrap();
    let report = scan_with_exit(root, None, None, 2);
    assert!(
        report["analysis_errors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|error| {
                error["file"] == "consumer.py" && error["kind"] == "unresolved_import"
            }),
        "{report}"
    );
    // Imported module hooks can initialize otherwise absent globals.
    fs::write(
        root.join("consumer.py"),
        "def __getattr__(name): return missing
",
    )
    .unwrap();
    fs::write(
        root.join("web.py"),
        "from flask import Flask\nfrom consumer import setup\napp = Flask(__name__)\napp = missing\n@app.shell_context_processor\ndef callback(): return {}\n",
    ).unwrap();
    let report = scan_with_exit(root, None, None, 2);
    assert!(
        report["analysis_errors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|error| { error["file"] == "web.py" && error["kind"] == "unresolved_decorator" }),
        "{report}"
    );
}

#[test]
fn flask_login_empty_provider_lookalike_has_no_registration_method() {
    let project = tempfile::tempdir().unwrap();
    fs::write(
        project.path().join("provider.py"),
        "class LoginManager: pass\nlogin = LoginManager()\n",
    )
    .unwrap();
    fs::write(
        project.path().join("app.py"),
        "from provider import login\n@login.user_loader\ndef handler(user_id): return 1\n",
    )
    .unwrap();
    let report = scan(project.path());
    assert_eq!(findings(&report, "unused_functions"), ["app.handler"]);
}

#[test]
fn flask_factory_decorator_keeps_its_closed_named_class_replacement() {
    let project = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let root = project.path();
    let provider = root.join("providers.py");
    let setup = r#"from flask import Flask
class Replacement:
    @staticmethod
    def shell_context_processor(callback): return callback
def replace(factory): return Replacement
"#;
    let factory = "@replace\ndef create_app(): return Flask(__name__)\n";
    fs::write(&provider, format!("{setup}{factory}")).unwrap();
    fs::write(
        root.join("app.py"),
        "from providers import create_app\napp = create_app()\n@app.shell_context_processor\ndef context(): return {}\n",
    )
    .unwrap();
    let report = scan(root);
    assert_eq!(
        evidence_certainties(&report, "providers.replace", "decorator"),
        ["certain"]
    );
    assert_eq!(
        evidence_certainties(&report, "providers.Replacement", "direct_call"),
        ["certain"]
    );
    assert!(evidence_certainties(&report, "app.context", "flask_registration").is_empty());
    assert!(findings(&report, "unused_functions").contains(&"app.context"));
    for _ in 0..2 {
        let mut cached = scan_with_settings(root, None, Some(cache.path()));
        cached.as_object_mut().unwrap().remove("cache");
        assert_eq!(cached, report);
    }

    fs::write(
        &provider,
        format!(
            "{setup}def overwrite():\n    global Replacement\n    Replacement = Flask\noverwrite()\n{factory}"
        ),
    )
    .unwrap();
    let mut report = scan_with_exit(root, None, Some(cache.path()), 2);
    report.as_object_mut().unwrap().remove("cache");
    assert_eq!(report, scan_with_exit(root, None, None, 2));
    assert!(
        report["analysis_errors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|issue| issue["kind"] == "unresolved_decorator")
    );
}

#[test]
fn flask_factory_consumer_stops_at_proven_initialization_failures() {
    let project = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let root = project.path();
    let provider = root.join("providers.py");
    let web = root.join("web.py");
    let callback = "@app.shell_context_processor\ndef callback(): return {}\n";
    let factory = "from flask import Flask\ndef create_app():\n    app = Flask(__name__)\n";

    for (source, mutation) in [
        (
            format!("{factory}    return app\n"),
            "app.shell_context_processor = missing\n",
        ),
        (
            format!("{factory}    return app\n"),
            "alias = app\nalias.shell_context_processor = missing\n",
        ),
        (
            format!("{factory}    return app\n"),
            "setattr(app, 'shell_context_processor', missing)\n",
        ),
        (
            format!("{factory}    return app\n"),
            "if missing:\n    app = replacement\n",
        ),
        (format!("{factory}    yield 1\n    return app\n"), ""),
        (format!("{factory}    yield from []\n    return app\n"), ""),
        (
            "from flask import Flask\ndef create_app(Flask): return Flask(__name__)\n".into(),
            "",
        ),
        (
            "from flask import Flask\ndef create_app(): return missing\n".into(),
            "",
        ),
        (
            format!("{factory}    app.shell_context_processor = missing\n    return app\n"),
            "",
        ),
        (
            format!(
                "{factory}    setattr(app, 'shell_context_processor', missing)\n    return app\n"
            ),
            "",
        ),
        (
            format!("{factory}    alias = app\n    opaque(alias)\n    return app\n"),
            "",
        ),
        (
            format!("{factory}    holder.value = app\n    return app\n"),
            "",
        ),
    ] {
        fs::write(&provider, &source).unwrap();
        fs::write(
            &web,
            format!("from providers import create_app\napp = create_app()\n{mutation}{callback}"),
        )
        .unwrap();
        let report = scan(root);
        assert!(
            findings(&report, "unused_functions").contains(&"web.callback"),
            "{source}{mutation}\n{report}"
        );
        let mut cached = scan_with_settings(root, None, Some(cache.path()));
        cached.as_object_mut().unwrap().remove("cache");
        assert_eq!(report, cached);
    }

    fs::write(&provider, format!("{factory}    return app\n")).unwrap();
    fs::write(
        &web,
        format!("from providers import create_app\napp = create_app()\n{callback}"),
    )
    .unwrap();
    let report = scan(root);
    assert!(
        !findings(&report, "unused_functions").contains(&"web.callback"),
        "{report}"
    );

    // A resolved callee before the factory returns may actually change the application.
    fs::write(&provider, format!("def opaque(app): app.shell_context_processor = lambda fn: fn\n{factory}    opaque(app)\n    return app\n")).unwrap();
    let report = scan_with_exit(root, None, Some(cache.path()), 2);
    assert!(
        report["analysis_errors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|error| error["kind"] == "unresolved_decorator"),
        "{report}"
    );

    // An escaped generator may be consumed before attempting the decoration.
    fs::write(&provider, format!("{factory}    yield app\n")).unwrap();
    fs::write(
        &web,
        format!("from providers import create_app\napp = create_app()\nopaque(app)\n{callback}"),
    )
    .unwrap();
    let report = scan_with_exit(root, None, Some(cache.path()), 2);
    assert!(
        report["analysis_errors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|error| error["kind"] == "unresolved_generator_execution"),
        "{report}"
    );
}

#[test]
fn flask_class_writes_do_not_supply_stale_framework_evidence() {
    let project = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let root = project.path();
    fs::write(
        root.join("app.py"),
        r#"from flask import Flask
from flask_login import LoginManager as Manager

app = Flask(__name__)
login = Manager()

@app.shell_context_processor
def app_before(): return {}

@login.user_loader
def login_before(user_id): return user_id

def passthrough(self, callback): return callback
setattr(Manager, 'user_loader', passthrough)

@login.user_loader
def login_after(user_id): return user_id

@login.request_loader
def independent(request): return request

def replace_allocation(cls, import_name): return object()
Flask.__new__ = replace_allocation
replaced = Flask(__name__)

@replaced.shell_context_processor
def app_after(): return {}
"#,
    )
    .unwrap();
    let report = scan_with_exit(root, None, None, 2);
    assert_eq!(report["coverage"]["status"], "incomplete");
    let evidence = report["dead_code_evidence"]["symbols"].as_array().unwrap();
    let framework = |symbol: &str| {
        evidence.iter().any(|row| {
            row["qualified_name"] == symbol
                && row["evidence"].as_array().unwrap().iter().any(|edge| {
                    matches!(
                        edge["kind"].as_str().unwrap(),
                        "flask_registration" | "flask_login_registration"
                    )
                })
        })
    };
    for name in ["app.app_before", "app.login_before", "app.independent"] {
        assert!(framework(name), "{report}");
    }
    for name in ["app.app_after", "app.login_after"] {
        assert!(!framework(name), "{report}");
    }
    for _ in 0..2 {
        let mut cached = scan_with_exit(root, None, Some(cache.path()), 2);
        cached.as_object_mut().unwrap().remove("cache");
        assert_eq!(report, cached);
    }
}

#[test]
fn flask_opaque_consumers_invalidate_only_exposed_registration_providers() {
    let project = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let root = project.path();
    fs::write(
        root.join("provider.py"),
        "from flask_login import LoginManager\nlogin = LoginManager()\n",
    )
    .unwrap();
    for argument in [
        "login",
        "{'manager': login}",
        "manager=login",
        "login.user_loader",
    ] {
        fs::write(
            root.join("app.py"),
            format!(
                r#"from extension import customize
from flask_login import LoginManager
from provider import login

other = LoginManager()

@login.user_loader
def before(user_id): return user_id

customize({argument})

@login.user_loader
def after(user_id): return user_id

@other.user_loader
def independent(user_id): return user_id
"#,
            ),
        )
        .unwrap();
        let report = scan_with_exit(root, None, None, 2);
        let registrations: Vec<_> = report["dead_code_evidence"]["symbols"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|symbol| {
                symbol["evidence"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|edge| edge["kind"] == "flask_login_registration")
            })
            .map(|symbol| symbol["qualified_name"].as_str().unwrap())
            .collect();
        assert_eq!(registrations, ["app.before", "app.independent"], "{report}");
        let mut cached = scan_with_exit(root, None, Some(cache.path()), 2);
        cached.as_object_mut().unwrap().remove("cache");
        assert_eq!(report, cached);
    }
}

#[test]
fn flask_constructor_rebinding_does_not_change_the_earlier_import_capture() {
    let project = tempfile::tempdir().unwrap();
    fs::write(
        project.path().join("app.py"),
        r#"import flask as framework
from flask import Flask as Original

class Replacement: pass
framework.Flask = Replacement
from flask import Flask as Updated

app = Original(__name__)
replaced = Updated()

@app.shell_context_processor
def live(): return {}

@replaced.shell_context_processor
def detached(): return {}
"#,
    )
    .unwrap();
    let report = scan(project.path());
    assert_eq!(findings(&report, "unused_functions"), ["app.detached"]);
}

#[test]
fn fastapi_long_boolean_aliases_preserve_route_activation() {
    use std::fmt::Write as _;

    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    let cache = tempfile::tempdir().unwrap();
    let app = root.join("app.py");

    for (length, initial) in [(1100, false), (1101, false), (1101, true)] {
        let initial_literal = if initial { "True" } else { "False" };
        let mut source = format!(
            "from fastapi import FastAPI\napp = FastAPI()\n\
             def live(): return 1\ndef dormant(): return 2\n\
             flag_0 = {initial_literal}\n"
        );
        for index in 1..=length {
            writeln!(source, "flag_{index} = not flag_{}", index - 1).unwrap();
        }
        // Updating the first binding cannot affect the Boolean already captured by the last.
        let next_literal = if initial { "False" } else { "True" };
        writeln!(source, "flag_0 = {next_literal}").unwrap();
        let (active, inactive) = if initial != (length % 2 == 1) {
            ("and", "or")
        } else {
            ("or", "and")
        };
        writeln!(
            source,
            "flag_{length} {active} app.add_api_route('/live', live)"
        )
        .unwrap();
        writeln!(
            source,
            "flag_{length} {inactive} app.add_api_route('/dormant', dormant)"
        )
        .unwrap();
        fs::write(&app, source).unwrap();

        let report = scan(root);
        assert_eq!(findings(&report, "unused_functions"), ["app.dormant"]);
        assert_eq!(
            evidence_certainties(&report, "app.live", "fastapi_registration"),
            ["certain"]
        );
        assert!(evidence_certainties(&report, "app.dormant", "fastapi_registration").is_empty());
        for _ in 0..2 {
            let mut cached = scan_with_settings(root, None, Some(cache.path()));
            cached.as_object_mut().unwrap().remove("cache");
            assert_eq!(cached, report);
        }
    }
}

#[test]
fn fastapi_boolean_aliases_preserve_conditional_route_evidence() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    fs::write(
        root.join("app.py"),
        r#"from fastapi import FastAPI
import os
app = FastAPI()
def endpoint(): return 1
flag = False
if os.getenv('ENABLE'):
    flag = True
alias = not flag
alias and app.add_api_route('/', endpoint)
"#,
    )
    .unwrap();
    let report = scan(root);
    assert_eq!(
        evidence_certainties(&report, "app.endpoint", "fastapi_registration"),
        ["possible"]
    );
}

#[test]
fn fastapi_router_cycle_preserves_snapshot_order_and_live_uncertainty() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    fs::write(
        root.join("app.py"),
        r#"from fastapi import APIRouter, FastAPI
app = FastAPI()
first = APIRouter()
second = APIRouter()
orphan = APIRouter()

@second.get('/live')
def endpoint(): return 1

@orphan.get('/orphan')
def unreachable(): return 2

def uncalled_factory():
    app.include_router(orphan)

first.include_router(second)
second.include_router(first)
app.include_router(first)
"#,
    )
    .unwrap();
    for (mode, code) in [("snapshot", 0), ("live", 2)] {
        fs::write(
            root.join(".skylos.toml"),
            format!("root = true\n[dead_code]\nfastapi_router_mode = '{mode}'\n"),
        )
        .unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_skarmory"))
            .current_dir(root)
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
        let report: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(output.status.code(), Some(code), "{report}");
        if mode == "snapshot" {
            assert_eq!(
                findings(&report, "unused_functions"),
                ["app.uncalled_factory", "app.unreachable"]
            );
        } else {
            assert!(
                report["analysis_errors"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|error| { error["kind"] == "analysis_limit" }),
                "{report}"
            );
        }
    }
}

#[test]
fn fastapi_router_mode_selects_late_direct_and_nested_routes() {
    let project = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let root = project.path();
    let config = root.join(".skylos.toml");
    fs::write(
        root.join("app.py"),
        r#"from fastapi import APIRouter, FastAPI

app = FastAPI()
outer = APIRouter()
inner = APIRouter()
dormant = APIRouter()

@outer.on_event('startup')
def early_startup():
    return 5

app.include_router(outer)

@outer.get('/direct')
def direct():
    return 1

@inner.get('/nested')
def nested():
    return 2

@dormant.get('/dormant')
def orphan():
    return 3

@inner.on_event('startup')
def nested_startup():
    return 6

outer.include_router(inner)

@outer.on_event('startup')
def startup():
    return 4
"#,
    )
    .unwrap();
    let scan = |code, cached| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_skarmory"));
        command
            .current_dir(root)
            .env_remove("SKYLOS_CONFIG_FILE")
            .args([
                "--profile",
                "skylos-0.3",
                ".",
                "--json",
                "--no-grep-verify",
                "--confidence",
                "0",
            ]);
        if cached {
            command
                .args(["--workers", "4", "--cache-dir"])
                .arg(cache.path());
        }
        let output = command.output().unwrap();
        assert_eq!(
            output.status.code(),
            Some(code),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let mut report: Value = serde_json::from_slice(&output.stdout).unwrap();
        report.as_object_mut().unwrap().remove("cache");
        report
    };
    fs::write(&config, "root = true\n").unwrap();
    let unresolved = scan(2, false);
    assert_eq!(unresolved["coverage"]["status"], "incomplete");
    assert!(
        unresolved["analysis_errors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|error| {
                error["message"]
                    .as_str()
                    .unwrap()
                    .contains("across framework versions")
            })
    );

    fs::write(
        &config,
        "root = true\n[dead_code]\nfastapi_router_mode = 'live'\n",
    )
    .unwrap();
    let live = scan(0, false);
    assert_eq!(
        findings(&live, "unused_functions"),
        ["app.nested_startup", "app.orphan", "app.startup"],
        "{live}"
    );
    assert_eq!(live, scan(0, true));
    assert_eq!(live, scan(0, true));

    fs::write(
        &config,
        "root = true\n[dead_code]\nfastapi_router_mode = 'snapshot'\n",
    )
    .unwrap();
    let copied = scan(0, false);
    assert_eq!(
        findings(&copied, "unused_functions"),
        [
            "app.direct",
            "app.nested",
            "app.nested_startup",
            "app.orphan",
            "app.startup"
        ],
        "{copied}"
    );
    assert_eq!(copied, scan(0, true));
}

#[test]
fn fastapi_deployment_factory_preserves_possible_routes() {
    let project = tempfile::tempdir().unwrap();
    let path = project.path().join("app.py");
    let cache = tempfile::tempdir().unwrap();
    let source = r#"from fastapi import FastAPI

def helper(): return 1
def retired(): return 2

def create_app():
    application = FastAPI()
    @application.get('/')
    def handler():
        return helper()
    return application
"#;
    for (suffix, certainty) in [("", "possible"), ("app = create_app()\n", "certain")] {
        fs::write(&path, format!("{source}{suffix}")).unwrap();
        let report = scan(project.path());
        assert_eq!(
            findings(&report, "unused_functions"),
            ["app.retired"],
            "{report}"
        );
        let mut cached = scan_with_settings(project.path(), None, Some(cache.path()));
        cached.as_object_mut().unwrap().remove("cache");
        assert_eq!(cached, report);
        let symbols = report["dead_code_evidence"]["symbols"].as_array().unwrap();
        assert!(
            symbols.iter().any(|symbol| {
                symbol["qualified_name"] == "app.create_app.handler"
                    && symbol["evidence"].as_array().unwrap().iter().any(|edge| {
                        edge["kind"] == "fastapi_registration" && edge["certainty"] == certainty
                    })
            }),
            "{report}"
        );
    }
    for changed in [
        source.replace("return application", "return None"),
        format!("{source}create_app = None\n"),
        source.replace(
            "from fastapi import FastAPI",
            "class FastAPI:\n    def get(self, path): return lambda callback: callback",
        ),
    ] {
        fs::write(&path, changed).unwrap();
        let report = scan(project.path());
        assert!(
            findings(&report, "unused_functions").contains(&"app.helper"),
            "{report}"
        );
    }
}

#[test]
fn fastapi_plain_async_lifespan_does_not_open_its_body() {
    let project = tempfile::tempdir().unwrap();
    fs::write(
        project.path().join("app.py"),
        "from fastapi import FastAPI\nasync def lifespan(app): return helper()\ndef helper(): return 1\napp = FastAPI(lifespan=lifespan)\n",
    )
    .unwrap();
    let report = scan(project.path());
    assert!(
        findings(&report, "unused_functions").contains(&"app.helper"),
        "{report}"
    );
    let symbols = report["dead_code_evidence"]["symbols"].as_array().unwrap();
    assert!(
        !symbols.iter().any(|symbol| {
            symbol["qualified_name"] == "app.lifespan"
                && symbol["evidence"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|edge| edge["kind"] == "fastapi_lifespan")
        }),
        "{report}"
    );
}

#[test]
fn fastapi_source_middleware_does_not_consume_an_ignored_dispatch_option() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    fs::create_dir_all(root.join("starlette/middleware")).unwrap();
    for name in ["starlette/__init__.py", "starlette/middleware/__init__.py"] {
        fs::write(root.join(name), "").unwrap();
    }
    fs::write(
        root.join("app.py"),
        "from fastapi import FastAPI\nfrom starlette.middleware.base import BaseHTTPMiddleware\ndef helper(): return 1\ndef dispatch(request, call_next): return helper()\napplication = FastAPI()\napplication.add_middleware(BaseHTTPMiddleware, dispatch=dispatch)\n",
    ).unwrap();
    let path = root.join("starlette/middleware/base.py");
    for provider in [
        "class BaseHTTPMiddleware:\n    def __init__(self, app, **kwargs): pass\n    async def __call__(self, scope, receive, send): pass\n",
        "def BaseHTTPMiddleware(app, **kwargs):\n    async def middleware(scope, receive, send): pass\n    return middleware\n",
    ] {
        fs::write(&path, provider).unwrap();
        let report = scan(root);
        assert!(
            findings(&report, "unused_functions").contains(&"app.helper"),
            "{report}"
        );
        let symbols = report["dead_code_evidence"]["symbols"].as_array().unwrap();
        assert!(
            !symbols
                .iter()
                .any(|symbol| symbol["qualified_name"] == "app.dispatch"),
            "{report}"
        );
    }
    fs::write(
        &path,
        "def BaseHTTPMiddleware(app, **kwargs): return unknown(app, kwargs)\n",
    )
    .unwrap();
    let report = scan_with_exit(root, None, None, 2);
    assert!(
        report["analysis_errors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|error| { error["kind"] == "unresolved_registration" })
    );
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
    for guard in [
        "os.getenv('USE_HANDLER')",
        "os.environ.get('USE_HANDLER')",
        "os.environ.get('USE_HANDLER', None)",
    ] {
        fs::write(
            project.path().join("web.py"),
            format!(
                r#"import os
from fastapi import FastAPI

def handle_error(request, exc): return helper()
def helper(): return 1
def retired(): return 2

app = FastAPI()
if {guard}:
    app.add_exception_handler(Exception, handle_error)
"#
            ),
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
fn project_wildcard_reads_finite_settings_exports_from_a_selected_sibling() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    let cache = tempfile::tempdir().unwrap();
    let conf = root.join("conf");
    fs::create_dir(&conf).unwrap();
    fs::write(conf.join("__init__.py"), "").unwrap();
    let base = conf.join("settings.py");
    let provider = "INSTALLED_APPS = ['django.contrib.auth']\n\
                    CUSTOM_FLAG = True\n\
                    _PRIVATE = False\n";
    fs::write(&base, provider).unwrap();
    fs::write(
        conf.join("settings_production.py"),
        "from conf.settings import *\nPRODUCTION_ONLY = True\n",
    )
    .unwrap();
    fs::write(conf.join("constants.py"), "STALE_CONSTANT = 1\n").unwrap();
    fs::write(
        conf.join("consumer.py"),
        "from conf.settings_production import CUSTOM_FLAG\n\
         if not CUSTOM_FLAG:\n    raise RuntimeError()\n",
    )
    .unwrap();

    let report = scan(root);
    let unused = findings(&report, "unused_variables");
    assert!(
        unused.contains(&"conf.constants.STALE_CONSTANT"),
        "{report}"
    );
    assert!(!unused.contains(&"conf.settings.CUSTOM_FLAG"), "{report}");
    assert!(findings(&report, "unused_imports").is_empty(), "{report}");
    for _ in 0..2 {
        let mut cached = scan_with_settings(root, None, Some(cache.path()));
        cached.as_object_mut().unwrap().remove("cache");
        assert_eq!(cached, report);
    }

    let unresolved = |report: &Value| {
        assert_eq!(report["coverage"]["status"], "incomplete", "{report}");
        assert!(
            report["analysis_errors"]
                .as_array()
                .unwrap()
                .iter()
                .any(|error| {
                    error["kind"] == "unsupported_construct"
                        && error["file"] == "conf/settings_production.py"
                        && error["line"] == 1
                }),
            "{report}"
        );
    };
    fs::write(&base, format!("{provider}__all__ = []\n")).unwrap();
    unresolved(&scan_with_exit(root, None, Some(cache.path()), 2));
    fs::write(
        &base,
        format!("from __future__ import annotations\n{provider}"),
    )
    .unwrap();
    unresolved(&scan_with_exit(root, None, Some(cache.path()), 2));
    fs::write(&base, provider).unwrap();
    fs::create_dir(conf.join("settings")).unwrap();
    fs::write(conf.join("settings/__init__.py"), "CUSTOM_FLAG = False\n").unwrap();
    unresolved(&scan_with_excludes(
        root,
        None,
        Some(cache.path()),
        &["conf/settings/"],
        2,
    ));
}

#[test]
fn project_wildcards_follow_the_selected_nested_settings_branch() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    let cache = tempfile::tempdir().unwrap();
    let settings = root.join("project/settings");
    fs::create_dir_all(&settings).unwrap();
    fs::write(root.join("project/__init__.py"), "").unwrap();
    fs::write(
        settings.join("__init__.py"),
        "import sys\nfrom .base import *\ntry:\n    from .local import *\nexcept ImportError:\n    pass\nif DEV:\n    from .dev import *\n",
    ).unwrap();
    fs::write(
        settings.join("dev.py"),
        "import project.settings.base as base\nAPPS = base.APPS + ('development',)\nCONFIG = {'flag': True}\n",
    ).unwrap();
    for dev in [true, false] {
        fs::write(
            settings.join("base.py"),
            format!(
                "DEV = {}\nAPPS = ('shop',)\nif DEV:\n    SESSION = True\nDORMANT = 'keep'\n",
                if dev { "True" } else { "False" },
            ),
        )
        .unwrap();
        fs::write(
            root.join("consumer.py"),
            if dev {
                "from project.settings import APPS, CONFIG, SESSION\nassert APPS\nassert CONFIG\nassert SESSION\n"
            } else {
                "from project.settings import APPS\nassert APPS\n"
            },
        ).unwrap();
        let report = scan(root);
        let used = findings(&report, "unused_variables");
        assert!(used.contains(&"project.settings.base.DORMANT"), "{report}");
        assert_eq!(
            used.contains(&"project.settings.dev.CONFIG"),
            !dev,
            "{report}"
        );
        assert_eq!(
            used.contains(&"project.settings.base.SESSION"),
            !dev,
            "{report}"
        );
        for _ in 0..2 {
            let mut cached = scan_with_settings(root, None, Some(cache.path()));
            cached.as_object_mut().unwrap().remove("cache");
            assert_eq!(cached, report);
        }
    }
}

#[test]
fn django_settings_replay_resolves_wildcard_collection_by_binding() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    let conf = root.join("conf");
    fs::create_dir(&conf).unwrap();
    fs::write(conf.join("__init__.py"), "").unwrap();
    fs::write(
        conf.join("base.py"),
        "INSTALLED_APPS = []\nROOT_URLCONF = 'conf.urls'\nMIDDLEWARE = ['handler.Middleware']\nSECRET_KEY = 'fixture'\n",
    ).unwrap();
    fs::write(conf.join("settings.py"), "from .base import *\n").unwrap();
    fs::write(conf.join("urls.py"), "urlpatterns = []\n").unwrap();
    fs::write(
        root.join("wsgi.py"),
        "import os\nfrom django.core.wsgi import get_wsgi_application\nos.environ['DJANGO_SETTINGS_MODULE'] = 'conf.settings'\napplication = get_wsgi_application()\n",
    ).unwrap();
    fs::write(
        root.join("handler.py"),
        "class Middleware:\n    def __init__(self, get_response): self.get_response = get_response\n    def __call__(self, request): return self.get_response(request)\n",
    ).unwrap();

    let report = scan(root);
    assert_eq!(
        evidence_certainties(&report, "handler.Middleware", "django_setting_reference"),
        ["possible"]
    );
}

#[test]
fn django_builtin_startup_app_support_requires_default_toolbar_panels() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    fs::write(
        root.join("wsgi.py"),
        "import os\nfrom django.core.wsgi import get_wsgi_application\nos.environ['DJANGO_SETTINGS_MODULE'] = 'settings'\napplication = get_wsgi_application()\n",
    ).unwrap();
    fs::write(root.join("urls.py"), "urlpatterns = []\n").unwrap();
    let source = "INSTALLED_APPS = ['django.contrib.staticfiles.apps.StaticFilesConfig', 'debug_toolbar.apps.DebugToolbarConfig']\nROOT_URLCONF = 'urls'\nSTATIC_ROOT = 'staticfiles'\nSECRET_KEY = 'fixture'\n";
    let settings = root.join("settings.py");
    fs::write(&settings, source).unwrap();
    scan(root);

    let unresolved = |report: Value| {
        assert!(
            report["analysis_errors"]
                .as_array()
                .unwrap()
                .iter()
                .any(|error| {
                    error["kind"] == "unresolved_registration"
                        && error["file"] == "settings.py"
                        && error["line"] == 1
                }),
            "{report}"
        );
    };
    fs::write(
        &settings,
        format!("{source}DEBUG_TOOLBAR_PANELS = ['custom.Panel']\n"),
    )
    .unwrap();
    unresolved(scan_with_exit(root, None, None, 2));
    fs::write(&settings, source).unwrap();
    fs::create_dir(root.join("debug_toolbar")).unwrap();
    fs::write(root.join("debug_toolbar/__init__.py"), "").unwrap();
    unresolved(scan_with_excludes(root, None, None, &["debug_toolbar/"], 2));
}

#[test]
fn django_startup_replays_a_wildcard_selected_app_extension() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    let settings = root.join("conf/settings");
    fs::create_dir_all(&settings).unwrap();
    fs::write(root.join("conf/__init__.py"), "").unwrap();
    fs::write(root.join("conf/urls.py"), "urlpatterns = []\n").unwrap();
    fs::write(
        root.join("wsgi.py"),
        "import os\nfrom django.core.wsgi import get_wsgi_application\nos.environ['DJANGO_SETTINGS_MODULE'] = 'conf.settings'\napplication = get_wsgi_application()\n",
    ).unwrap();
    fs::write(
        settings.join("__init__.py"),
        "from .base import *\nif DEV:\n    from .dev import *\n",
    )
    .unwrap();

    for (base, additional) in [
        ("('django.contrib.staticfiles',)", "('debug_toolbar',)"),
        ("['django.contrib.staticfiles']", "['debug_toolbar']"),
    ] {
        fs::write(settings.join("base.py"), format!("DEV = True\nINSTALLED_APPS = {base}\nROOT_URLCONF = 'conf.urls'\nSTATIC_ROOT = 'static'\nSECRET_KEY = 'fixture'\n")).unwrap();
        fs::write(settings.join("dev.py"), format!("import conf.settings.base as base\nINSTALLED_APPS = base.INSTALLED_APPS + {additional}\n")).unwrap();
        scan(root);
    }
    fs::write(
        settings.join("dev.py"),
        "import conf.settings.base as base\nINSTALLED_APPS = base.INSTALLED_APPS + ('debug_toolbar',)\n",
    ).unwrap();
    let report = scan_with_exit(root, None, None, 2);
    assert!(
        report["analysis_errors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|error| {
                error["kind"] == "unsupported_construct"
                    && error["file"] == "conf/settings/__init__.py"
            }),
        "{report}"
    );
}

#[test]
fn relative_settings_wildcards_keep_unproven_optional_and_nested_imports_incomplete() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    let cache = tempfile::tempdir().unwrap();
    let conf = root.join("conf");
    fs::create_dir(&conf).unwrap();
    let initializer = conf.join("__init__.py");
    fs::write(conf.join("base.py"), "DEV = False\nCUSTOM_FLAG = True\n").unwrap();
    fs::write(
        conf.join("consumer.py"),
        "from conf import CUSTOM_FLAG\nassert CUSTOM_FLAG\n",
    )
    .unwrap();
    let source = "from .base import *\n";
    fs::write(&initializer, source).unwrap();
    let report = scan(root);
    assert!(!findings(&report, "unused_variables").contains(&"conf.base.CUSTOM_FLAG"));

    fs::write(conf.join("local.py"), "CUSTOM_FLAG = False\n").unwrap();
    for guard in ["True", "False", "unknown"] {
        let body = format!(
            "{source}try:\n    from .local import *\nexcept ImportError:\n    pass\n\
             if {guard}:\n    from .dev import *\nRETAINED = True\n"
        );
        fs::write(&initializer, body).unwrap();
        let uncached = scan_with_excludes(root, None, None, &["conf/local.py"], 2);
        assert_eq!(uncached["coverage"]["status"], "incomplete", "{uncached}");
        assert_eq!(uncached["coverage"]["extracted_files"], 3, "{uncached}");
        assert!(
            uncached["definitions"]
                .as_object()
                .unwrap()
                .values()
                .any(|definition| definition["name"] == "conf.RETAINED"),
            "{uncached}"
        );
        assert!(
            findings(&uncached, "unused_variables").is_empty(),
            "{uncached}"
        );
        assert!(
            uncached["dead_code_evidence"]["symbols"]
                .as_array()
                .unwrap()
                .is_empty(),
            "{uncached}"
        );
        let errors: Vec<_> = uncached["analysis_errors"]
            .as_array()
            .unwrap()
            .iter()
            .map(|error| (&error["file"], &error["line"], &error["kind"]))
            .collect();
        assert_eq!(
            errors,
            vec![
                (
                    &Value::from("conf/__init__.py"),
                    &Value::from(2),
                    &Value::from("unsupported_construct")
                ),
                (
                    &Value::from("conf/__init__.py"),
                    &Value::from(7),
                    &Value::from("unsupported_construct")
                ),
            ],
            "{uncached}"
        );
        for _ in 0..2 {
            let mut cached =
                scan_with_excludes(root, None, Some(cache.path()), &["conf/local.py"], 2);
            cached.as_object_mut().unwrap().remove("cache");
            assert_eq!(cached, uncached);
        }
    }
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
fn fastapi_immediate_route_lambda_executes_while_unconsumed_lambda_stays_dormant() {
    let project = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let file = project.path().join("web.py");
    let source = r#"from fastapi import APIRouter, FastAPI

def active_helper(): return 1
def dormant_helper(): return 2
def active(): return active_helper()
def dormant(): return dormant_helper()

app = FastAPI()
router = APIRouter()
(lambda: router.add_api_route('/active', active))()
stored = lambda: router.add_api_route('/dormant', dormant)
app.include_router(router)
"#;
    fs::write(&file, source).unwrap();
    let report = scan(project.path());
    assert_eq!(
        findings(&report, "unused_functions"),
        ["web.dormant", "web.dormant_helper"]
    );
    let symbols = report["dead_code_evidence"]["symbols"].as_array().unwrap();
    assert!(
        symbols
            .iter()
            .any(|row| row["qualified_name"] == "web.active"
                && row["evidence"].as_array().unwrap().iter().any(|edge| {
                    edge["kind"] == "fastapi_registration" && edge["certainty"] == "certain"
                }))
    );
    let mut cached = scan_with_settings(project.path(), None, Some(cache.path()));
    cached.as_object_mut().unwrap().remove("cache");
    assert_eq!(cached, report);

    fs::write(&file, format!("{source}stored()\n")).unwrap();
    let report = scan_with_exit(project.path(), None, Some(cache.path()), 2);
    assert!(
        report["analysis_errors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|issue| issue["kind"] == "unresolved_lambda_execution")
    );

    fs::write(
        &file,
        "from fastapi import FastAPI\n\
         def endpoint(): return 1\n\
         class Container:\n    app = FastAPI()\n    (lambda: app.add_api_route('/', endpoint))()\n",
    )
    .unwrap();
    let report = scan_with_exit(project.path(), None, Some(cache.path()), 2);
    assert!(
        report["analysis_errors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|issue| issue["kind"] == "unresolved_lambda_execution")
    );
    assert!(
        !report["dead_code_evidence"]["symbols"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["qualified_name"] == "web.endpoint")
    );
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
fn django_model_form_metaclass_reads_only_its_effective_help_texts() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    let cache = tempfile::tempdir().unwrap();
    fs::write(
        root.join("forms.py"),
        concat!(
            "from django.db.models import Model, CharField\n",
            "from django.forms import ModelForm\n",
            "class Product(Model):\n",
            "    name = CharField(max_length=20)\n",
            "class ProductForm(ModelForm):\n",
            "    class Meta:\n",
            "        model = Product\n",
            "        fields = ['name']\n",
            "        help_texts = {'name': 'Product name'}\n",
            "        stale_option = True\n",
            "class Lookalike:\n",
            "    class Meta:\n",
            "        help_texts = {'name': 'Unused'}\n",
        ),
    )
    .unwrap();
    let report = scan(root);
    let selected = "forms.ProductForm.Meta.help_texts";
    assert_eq!(
        evidence_certainties(&report, selected, "django_model_form_help_texts"),
        ["certain"]
    );
    assert!(!findings(&report, "unused_variables").contains(&selected));
    assert!(
        evidence_certainties(
            &report,
            "forms.Lookalike.Meta.help_texts",
            "django_model_form_help_texts"
        )
        .is_empty()
    );
    assert!(findings(&report, "unused_variables").contains(&"forms.ProductForm.Meta.stale_option"));
    for _ in 0..2 {
        let mut cached = scan_with_settings(root, None, Some(cache.path()));
        cached.as_object_mut().unwrap().remove("cache");
        assert_eq!(cached, report);
    }
}

#[test]
fn django_date_format_uses_only_the_selected_source_locale() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    let cache = tempfile::tempdir().unwrap();
    let imports = "import os\nfrom django.core.wsgi import get_wsgi_application\nfrom django.utils.formats import get_format\n";
    let setup =
        "os.environ['DJANGO_SETTINGS_MODULE'] = 'settings'\napplication = get_wsgi_application()\n";
    let lookup = "print(get_format('DATE_FORMAT', lang='fr'))\n";
    for (path, source) in [
        ("locale_settings/__init__.py", ""),
        ("locale_settings/fr/__init__.py", ""),
        (
            "locale_settings/fr/formats.py",
            "DATE_FORMAT = 'j F Y'\nTIME_FORMAT = 'H:i'\n",
        ),
        ("locale_settings/en/__init__.py", ""),
        ("locale_settings/en/formats.py", "DATE_FORMAT = 'N j, Y'\n"),
        (
            "settings.py",
            "INSTALLED_APPS = []\nSECRET_KEY = 'fixture'\nFORMAT_MODULE_PATH = 'locale_settings'\n",
        ),
    ] {
        let file = root.join(path);
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(file, source).unwrap();
    }
    let wsgi = root.join("wsgi.py");
    fs::write(&wsgi, format!("{imports}{setup}{lookup}")).unwrap();
    let report = scan(root);
    let selected = "locale_settings.fr.formats.DATE_FORMAT";
    assert_eq!(
        evidence_certainties(&report, selected, "django_format_setting"),
        ["possible"]
    );
    let unused = findings(&report, "unused_variables");
    assert!(!unused.contains(&selected));
    assert!(unused.contains(&"locale_settings.fr.formats.TIME_FORMAT"));
    assert!(unused.contains(&"locale_settings.en.formats.DATE_FORMAT"));
    for _ in 0..2 {
        let mut cached = scan_with_settings(root, None, Some(cache.path()));
        cached.as_object_mut().unwrap().remove("cache");
        assert_eq!(cached, report);
    }

    fs::write(&wsgi, format!("{imports}{lookup}{setup}")).unwrap();
    let early = scan_with_exit(root, None, None, 2);
    assert!(evidence_certainties(&early, selected, "django_format_setting").is_empty());

    fs::write(&wsgi, format!("{imports}{setup}{lookup}")).unwrap();
    fs::write(
        root.join("locale_settings/fr/__init__.py"),
        "raise ImportError\n",
    )
    .unwrap();
    let failed_parent = scan_with_exit(root, None, None, 2);
    assert!(evidence_certainties(&failed_parent, selected, "django_format_setting").is_empty());

    fs::write(root.join("locale_settings/fr/__init__.py"), "").unwrap();
    let source = root.join("locale_settings/fr/formats.py");
    fs::write(&source, "DATE_FORMAT = 'j F Y'\nraise ImportError\n").unwrap();
    let failed_module = scan_with_exit(root, None, None, 2);
    assert!(evidence_certainties(&failed_module, selected, "django_format_setting").is_empty());

    fs::write(&source, "DATE_FORMAT = 'j F Y'\n").unwrap();
    let edit = "import settings\nsettings.FORMAT_MODULE_PATH = 'alternate'\n";
    fs::write(&wsgi, format!("{imports}{edit}{setup}{lookup}")).unwrap();
    let edited = scan_with_exit(root, None, None, 2);
    assert!(evidence_certainties(&edited, selected, "django_format_setting").is_empty());
}

#[test]
fn celery_logging_with_previously_imported_receivers_keeps_coverage_incomplete() {
    let project = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let root = project.path();
    let worker = root.join("worker.py");
    let setup =
        "from celery import Celery\napp = Celery('shop')\napp.log.setup_logging_subsystem()\n";
    fs::write(
        root.join("logging_hooks.py"),
        "from celery.signals import setup_logging\n@setup_logging.connect\ndef configure_logging(**kwargs): return None\n",
    )
    .unwrap();
    fs::write(root.join("hooks.py"), "import logging_hooks\n").unwrap();

    for import in [
        "import logging_hooks\n",
        "import hooks\n",
        "from os import getenv\nif getenv('LOAD_LOGGING'):\n    import logging_hooks\n",
    ] {
        fs::write(&worker, format!("{import}{setup}")).unwrap();
        let report = scan_with_exit(root, None, None, 2);
        assert!(report["coverage"]["issues"].as_array().unwrap().iter().any(|issue| {
            issue["path"] == "worker.py"
                && issue["message"] == "Celery logging dispatch may reach receivers registered in an imported module"
        }), "{report}");
        assert!(!unreachable_functions(&report).contains(&"logging_hooks.configure_logging"));
        for _ in 0..2 {
            let mut cached = scan_with_exit(root, None, Some(cache.path()), 2);
            cached.as_object_mut().unwrap().remove("cache");
            assert_eq!(cached, report);
        }
    }

    for suffix in ["", "import logging_hooks\n"] {
        fs::write(&worker, format!("{setup}{suffix}")).unwrap();
        let report = scan(root);
        assert!(unreachable_functions(&report).contains(&"logging_hooks.configure_logging"));
    }
}

#[test]
fn celery_logging_dispatch_uses_only_receivers_registered_before_stock_setup() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    let cache = tempfile::tempdir().unwrap();
    let provider = "from celery import Celery\nfrom celery.signals import setup_logging\n";
    let register = "@setup_logging.connect\ndef configure_logging(*args, **kwargs):\n    return formatter()\ndef formatter(): return 'formatted'\n";
    let setup = "app = Celery('shop')\napp.log.setup_logging_subsystem()\n";
    let after = "@setup_logging.connect\ndef too_late(**kwargs):\n    return 2\n";
    let launcher = root.join("entry.py");
    fs::write(
        &launcher,
        format!("{provider}{register}{setup}{after}app.log.setup_logging_subsystem()\n"),
    )
    .unwrap();

    let report = scan(root);
    assert_eq!(
        evidence_certainties(&report, "entry.configure_logging", "celery_setup_logging"),
        ["possible"]
    );
    assert!(!unreachable_functions(&report).contains(&"entry.formatter"));
    assert!(unreachable_functions(&report).contains(&"entry.too_late"));
    let parameters = findings(&report, "unused_parameters");
    assert!(!parameters.contains(&"entry.configure_logging.kwargs"));
    for _ in 0..2 {
        let mut cached = scan_with_settings(root, None, Some(cache.path()));
        cached.as_object_mut().unwrap().remove("cache");
        assert_eq!(cached, report);
    }

    fs::write(&launcher, format!("{provider}{register}app = Celery('shop', log='custom')\napp.log.setup_logging_subsystem()\n")).unwrap();
    let custom = scan_with_exit(root, None, None, 2);
    assert!(
        evidence_certainties(&custom, "entry.configure_logging", "celery_setup_logging").is_empty()
    );

    fs::write(
        &launcher,
        format!("{provider}{register}app = Celery('shop')\napp.log = object()\napp.log.setup_logging_subsystem()\n"),
    )
    .unwrap();
    let replaced = scan_with_exit(root, None, None, 2);
    assert!(
        evidence_certainties(&replaced, "entry.configure_logging", "celery_setup_logging")
            .is_empty()
    );

    fs::write(&launcher, format!("{provider}{register}")).unwrap();
    assert!(unreachable_functions(&scan(root)).contains(&"entry.configure_logging"));
}

#[test]
fn django_standard_contrib_apps_allow_source_startup() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    let cache = tempfile::tempdir().unwrap();
    fs::create_dir(root.join("shop")).unwrap();
    for (path, source) in [
        ("shop/__init__.py", ""),
        (
            "shop/apps.py",
            "from django.apps import AppConfig\nclass ShopConfig(AppConfig):\n    name = 'shop'\n    def ready(self):\n        from . import signals\n",
        ),
        (
            "shop/signals.py",
            "from django.db.models.signals import post_save\nfrom django.dispatch import receiver\n@receiver(post_save)\ndef update(sender, **kwargs): pass\n",
        ),
        (
            "wsgi.py",
            "import os\nfrom django.core.wsgi import get_wsgi_application\nos.environ['DJANGO_SETTINGS_MODULE'] = 'settings'\napplication = get_wsgi_application()\n",
        ),
    ] {
        fs::write(root.join(path), source).unwrap();
    }
    for apps in [
        "'django.contrib.auth', 'django.contrib.contenttypes', 'django.contrib.sessions'",
        "'django.contrib.auth.apps.AuthConfig', 'django.contrib.contenttypes.apps.ContentTypesConfig', 'django.contrib.sessions.apps.SessionsConfig'",
    ] {
        fs::write(
            root.join("settings.py"),
            format!("INSTALLED_APPS = [{apps}, 'shop.apps.ShopConfig']\nSECRET_KEY = 'fixture'\n"),
        )
        .unwrap();
        let report = scan(root);
        assert!(!unreachable_functions(&report).contains(&"shop.signals.update"));
        for _ in 0..2 {
            let mut cached = scan_with_settings(root, None, Some(cache.path()));
            cached.as_object_mut().unwrap().remove("cache");
            assert_eq!(cached, report);
        }
    }

    fs::write(
        root.join("settings.py"),
        "INSTALLED_APPS = ['django.contrib.auth.apps.LookalikeConfig', 'shop.apps.ShopConfig']\nSECRET_KEY = 'fixture'\n",
    )
    .unwrap();
    let opaque = scan_with_exit(root, None, None, 2);
    assert!(
        opaque["coverage"]["issues"]
            .as_array()
            .unwrap()
            .iter()
            .any(|issue| issue["kind"] == "unresolved_registration"),
        "{opaque}"
    );
    assert!(
        !evidence_certainties(&opaque, "shop.signals.update", "django_signal_registration")
            .contains(&"certain")
    );
}

#[test]
fn django_field_callbacks_follow_effective_model_and_form_fields() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    let cache = tempfile::tempdir().unwrap();
    fs::write(
        root.join("fields.py"),
        r#"from django import forms
from django.db import models

class Converted(models.CharField):
    def from_db_value(self, value, expression, connection):
        return value

class DormantConverter(models.CharField):
    def from_db_value(self, value, expression, connection):
        return value

class Item(models.Model):
    value = Converted(max_length=15)

class Picker(forms.Widget):
    def value_from_datadict(self, data, files, name):
        return data.get(name)

class MaskedPicker(forms.Widget):
    def value_from_datadict(self, data, files, name):
        return data.get(name)

class Selection(forms.Form):
    value = forms.CharField(widget=Picker)

class Maskable(forms.Form):
    value = forms.CharField(widget=MaskedPicker)

class Masked(Maskable):
    value = None

class Disabled(forms.Form):
    value = forms.CharField(widget=MaskedPicker, disabled=True)

Disabled({'value': 'choose'}).is_valid()
Selection({'value': 'choose'}).is_valid()
Masked({'value': 'choose'}).is_valid()
"#,
    )
    .unwrap();
    let report = scan(root);
    assert_eq!(
        evidence_certainties(
            &report,
            "fields.Converted.from_db_value",
            "django_model_field_converter_declaration",
        ),
        ["possible"]
    );
    assert_eq!(
        evidence_certainties(
            &report,
            "fields.Picker.value_from_datadict",
            "django_widget_validation"
        ),
        ["possible"]
    );
    let unused = findings(&report, "unused_parameters");
    for parameter in [
        "fields.Converted.from_db_value.expression",
        "fields.Converted.from_db_value.connection",
        "fields.Picker.value_from_datadict.files",
    ] {
        assert!(!unused.contains(&parameter), "{parameter}: {report}");
        assert_eq!(
            evidence_certainties(&report, parameter, "django_field_parameter"),
            ["possible"]
        );
    }
    for parameter in [
        "fields.DormantConverter.from_db_value.expression",
        "fields.MaskedPicker.value_from_datadict.files",
    ] {
        assert!(unused.contains(&parameter), "{parameter}: {report}");
    }
    for _ in 0..2 {
        let mut cached = scan_with_settings(root, None, Some(cache.path()));
        cached.as_object_mut().unwrap().remove("cache");
        assert_eq!(cached, report);
    }

    fs::OpenOptions::new()
        .append(true)
        .open(root.join("fields.py"))
        .unwrap()
        .write_all(b"payload = {'value': 'choose'}\nSelection(payload).is_valid()\n")
        .unwrap();
    let opaque = scan_with_exit(root, None, None, 2);
    assert!(
        opaque["coverage"]["issues"]
            .as_array()
            .unwrap()
            .iter()
            .any(|issue| {
                issue["kind"] == "unresolved_call"
                    && issue["message"]
                        == "Django widget data lookup requires a bounded form submission"
            }),
        "{opaque}"
    );
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

    def get_list_select_related(self, current_request):
        return select_related()

    def __str__(self):
        return admin_label()

def select_related():
    return ()

class OtherAdmin(admin.ModelAdmin):
    list_display = ("tax_display",)
    def tax_display(self, obj):
        return 3

    def get_list_select_related(self, current_request):
        return ()

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
        assert_eq!(unused.contains(&"shop.admin.select_related"), !active);
        assert_eq!(
            evidence_certainties(
                &serial,
                "shop.admin.ProductAdmin.get_list_select_related.current_request",
                "django_admin_parameter",
            ),
            if active { vec!["possible"] } else { vec![] }
        );
        assert!(
            findings(&serial, "unused_parameters")
                .contains(&"shop.admin.OtherAdmin.get_list_select_related.current_request")
        );
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
fn flask_conditional_imperative_teardown_retains_possible_callbacks() {
    let project = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    fs::write(
        project.path().join("web.py"),
        r#"from flask import Flask
from os import name

def release(error): return cleanup()
def cleanup(): return 1
def always(error): return 2

app = Flask(__name__)
if name == "nt":
    app.teardown_appcontext(release)
app.teardown_appcontext(always)
"#,
    )
    .unwrap();
    let report = scan(project.path());
    assert_eq!(
        evidence_certainties(&report, "web.release", "flask_registration"),
        ["possible"]
    );
    assert_eq!(
        evidence_certainties(&report, "web.cleanup", "direct_call"),
        ["possible"]
    );
    assert_eq!(
        evidence_certainties(&report, "web.always", "flask_registration"),
        ["certain"]
    );
    let mut cached = scan_with_settings(project.path(), None, Some(cache.path()));
    cached.as_object_mut().unwrap().remove("cache");
    assert_eq!(cached, report);
}

#[test]
fn flask_login_bounded_namespace_writes_change_only_later_receiver_reads() {
    let project = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let root = project.path();
    let provider = root.join("providers.py");
    let setup = r#"from flask_login import LoginManager
login = LoginManager()
class Replacement:
    @staticmethod
    def user_loader(callback): return callback
replacement = Replacement()
@login.user_loader
def before(user_id): return user_id
"#;
    let update = "globals().update({'login': replacement})\n";
    let setter = "def replace():\n    global login\n    login = replacement\nreplace()\n";
    fs::write(
        root.join("app.py"),
        "from providers import login\n@login.user_loader\ndef after(user_id): return user_id\n",
    )
    .unwrap();
    for operation in [update, setter] {
        fs::write(&provider, format!("{setup}{operation}")).unwrap();
        let report = scan(root);
        assert_eq!(
            evidence_certainties(&report, "providers.before", "flask_login_registration"),
            ["certain"]
        );
        assert!(evidence_certainties(&report, "app.after", "flask_login_registration").is_empty());
        assert!(findings(&report, "unused_functions").contains(&"app.after"));
        for _ in 0..2 {
            let mut cached = scan_with_settings(root, None, Some(cache.path()));
            cached.as_object_mut().unwrap().remove("cache");
            assert_eq!(report, cached);
        }
    }

    fs::write(
        &provider,
        format!("{setup}def globals(): return {{}}\n{update}"),
    )
    .unwrap();
    let report = scan_with_exit(root, None, Some(cache.path()), 2);
    assert_eq!(
        evidence_certainties(&report, "app.after", "flask_login_registration"),
        ["certain"]
    );

    fs::write(
        &provider,
        format!("{setup}{setter}def later():\n    global replacement\n    replacement = login\n"),
    )
    .unwrap();
    let report = scan_with_exit(root, None, Some(cache.path()), 2);
    assert!(
        report["analysis_errors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|issue| issue["kind"] == "unresolved_global_binding")
    );
}

#[test]
fn flask_login_closed_helpers_apply_writes_at_their_initializer_call_sites() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    let cache = tempfile::tempdir().unwrap();
    let providers = root.join("providers.py");
    fs::write(
        root.join("app.py"),
        "from providers import login\n@login.user_loader\ndef callback(): return 1\n",
    )
    .unwrap();
    let setup = "from flask_login import LoginManager\nlogin = LoginManager()\n@login.user_loader\ndef before(): return 1\n";
    let cases = [
        "def replacement(self, callback): return callback\ndef mutate(target): target.user_loader = replacement\nmutate(LoginManager)\n",
        "def replacement(callback): return callback\ndef mutate(target): target.user_loader = replacement\nmutate(login)\n",
        "def replacement(callback): return callback\ndef replace(): login.user_loader = replacement\ndef mutate(apply): apply()\nmutate(replace)\n",
    ];
    for effect in cases {
        fs::write(&providers, format!("{setup}{effect}")).unwrap();
        let report = scan(root);
        assert_eq!(findings(&report, "unused_functions"), ["app.callback"]);
        assert_eq!(
            evidence_certainties(&report, "providers.before", "flask_login_registration"),
            ["certain"],
        );
        assert!(
            evidence_certainties(&report, "app.callback", "flask_login_registration").is_empty()
        );
        for _ in 0..2 {
            let mut cached = scan_with_settings(root, None, Some(cache.path()));
            cached.as_object_mut().unwrap().remove("cache");
            assert_eq!(cached, report);
        }
    }

    fs::write(&providers, format!("{setup}{}mutate(object())\n", cases[1])).unwrap();
    let incomplete = scan_with_exit(root, None, Some(cache.path()), 2);
    assert!(
        incomplete["analysis_errors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|issue| issue["kind"] == "unresolved_attribute_write"),
        "{incomplete}"
    );
}

#[test]
fn flask_login_global_rebind_does_not_prove_registration() {
    let project = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    fs::write(
        project.path().join("providers.py"),
        "from flask_login import LoginManager\nlogin = LoginManager()\n",
    )
    .unwrap();
    fs::write(
        project.path().join("app.py"),
        r#"from providers import login

class DisabledLogin:
    def user_loader(self, callback):
        return callback

manager = login
replacement = DisabledLogin()

def configure():
    global manager
    manager = replacement

configure()

@manager.user_loader
def load(user_id): return user_id

@login.request_loader
def request_user(request): return None
"#,
    )
    .unwrap();
    let report = scan_with_exit(project.path(), None, None, 2);
    assert!(
        report["coverage"]["issues"]
            .as_array()
            .unwrap()
            .iter()
            .any(|issue| issue["kind"] == "unresolved_global_binding"),
        "{report}"
    );
    assert_eq!(
        evidence_certainties(&report, "app.load", "flask_login_registration"),
        ["possible"]
    );
    assert_eq!(
        evidence_certainties(&report, "app.request_user", "flask_login_registration"),
        ["certain"]
    );
    let mut cached = scan_with_exit(project.path(), None, Some(cache.path()), 2);
    cached.as_object_mut().unwrap().remove("cache");
    assert_eq!(cached, report);
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
fn fastapi_router_lifespan_inherits_conditional_inclusion() {
    let project = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let source = r#"import os
from contextlib import asynccontextmanager
from fastapi import APIRouter, FastAPI

def initialize(): return 1

@asynccontextmanager
async def child_lifespan(app):
    initialize()
    yield

router = APIRouter(lifespan=child_lifespan)
app = FastAPI()
"#;
    for (mount, expected) in [
        (
            "if os.getenv('ENABLE_ROUTER'):\n    app.include_router(router)\n",
            "possible",
        ),
        ("app.include_router(router)\n", "certain"),
    ] {
        fs::write(project.path().join("web.py"), format!("{source}{mount}")).unwrap();
        let report = scan(project.path());
        assert_eq!(
            evidence_certainties(&report, "web.child_lifespan", "fastapi_lifespan"),
            [expected]
        );
        assert_eq!(
            evidence_certainties(&report, "web.initialize", "direct_call"),
            [expected]
        );
        for _ in 0..2 {
            let mut cached = scan_with_settings(project.path(), None, Some(cache.path()));
            cached.as_object_mut().unwrap().remove("cache");
            assert_eq!(cached, report);
        }
    }
}

#[test]
fn fastapi_none_lifespan_alias_uses_legacy_startup() {
    let project = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let config = project.path().join("config.py");
    fs::write(
        project.path().join("web.py"),
        r#"from fastapi import FastAPI
from config import LIFESPAN

def initialize(): return 1
def startup(): return initialize()

app = FastAPI(lifespan=LIFESPAN, on_startup=[startup])
"#,
    )
    .unwrap();
    for (value, expected) in [
        ("LIFESPAN = None\n", true),
        (
            "from contextlib import asynccontextmanager\n@asynccontextmanager\nasync def LIFESPAN(app):\n    yield\n",
            false,
        ),
    ] {
        fs::write(&config, value).unwrap();
        let report = scan(project.path());
        let certainty = if expected { &["certain"][..] } else { &[] };
        assert_eq!(
            evidence_certainties(&report, "web.startup", "fastapi_lifecycle"),
            certainty
        );
        assert_eq!(
            evidence_certainties(&report, "web.initialize", "direct_call"),
            certainty
        );
        let mut cached = scan_with_settings(project.path(), None, Some(cache.path()));
        cached.as_object_mut().unwrap().remove("cache");
        assert_eq!(cached, report);
    }
}

#[test]
fn fastapi_router_constructor_hooks_require_zero_argument_binding() {
    let project = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    for (definition, callback, symbol, valid) in [
        (
            "def startup(settings): return initialize()",
            "startup",
            "web.startup",
            false,
        ),
        (
            "async def startup(*, settings): return initialize()",
            "startup",
            "web.startup",
            false,
        ),
        (
            "def startup(settings=None): return initialize()",
            "startup",
            "web.startup",
            true,
        ),
        (
            "class Startup:\n    def __call__(self): return initialize()",
            "Startup()",
            "web.Startup.__call__",
            true,
        ),
        (
            "class Startup:\n    def __call__(self, settings): return initialize()",
            "Startup()",
            "web.Startup.__call__",
            false,
        ),
    ] {
        fs::write(
            project.path().join("web.py"),
            format!(
                r#"from fastapi import APIRouter, FastAPI

def initialize(): return 1
{definition}

router = APIRouter(on_startup=[{callback}])
app = FastAPI()
app.include_router(router)
"#
            ),
        )
        .unwrap();
        let code = if valid { 0 } else { 2 };
        let report = scan_with_exit(project.path(), None, None, code);
        let certainty = if valid { &["certain"][..] } else { &[] };
        assert_eq!(
            evidence_certainties(&report, symbol, "fastapi_lifecycle"),
            certainty
        );
        assert_eq!(
            evidence_certainties(&report, "web.initialize", "direct_call"),
            certainty
        );
        if !valid {
            assert!(
                report["coverage"]["issues"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|issue| issue["kind"] == "unresolved_registration"
                        && issue["message"]
                            .as_str()
                            .unwrap()
                            .contains("zero arguments")),
                "{report}"
            );
        }
        let mut cached = scan_with_exit(project.path(), None, Some(cache.path()), code);
        cached.as_object_mut().unwrap().remove("cache");
        assert_eq!(cached, report);
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
fn celery_terminal_failures_exclude_unreached_backend_calls() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    fs::write(root.join("providers.py"), "class Candidate: pass\n").unwrap();
    let app = root.join("app.py");
    let configured = "from celery import Celery\napp = Celery('worker')\napp.conf.result_backend = 'providers:Candidate'\n";
    for source in [
        "class Local: pass\napp = Local()\napp.conf.update(result_backend='providers:Candidate')\n".into(),
        "from celery import Celery\nclass Local: pass\nCelery = Local\napp = Celery()\napp.conf.update(result_backend='providers:Candidate')\n".into(),
        format!("{configured}app.conf.update(**replacement)\n"),
        format!("{configured}app.conf = replacement\n"),
        format!("{configured}replacement = {{'result_backend': 'rpc://'}}\napp.conf.update(**replacement)\n"),
    ] {
        fs::write(&app, source).unwrap();
        let report = scan(root);
        assert!(findings(&report, "unused_classes").contains(&"providers.Candidate"), "{report}");
    }

    let source = format!("{configured}app.backend\napp.conf.update(**replacement)\n");
    fs::write(&app, &source).unwrap();
    let report = scan(root);
    assert!(
        !findings(&report, "unused_classes").contains(&"providers.Candidate"),
        "{report}"
    );
    let cache = tempfile::tempdir().unwrap();
    for _ in 0..2 {
        let mut cached = scan_with_settings(root, None, Some(cache.path()));
        cached.as_object_mut().unwrap().remove("cache");
        assert_eq!(cached, report);
    }

    let consumer = root.join("consumer.py");
    fs::write(&consumer, "import app\n").unwrap();
    let incomplete = scan_with_exit(root, None, Some(cache.path()), 2);
    assert!(
        incomplete["coverage"]["issues"]
            .as_array()
            .unwrap()
            .iter()
            .any(|issue| issue["kind"] == "unresolved_import" && issue["path"] == "consumer.py"),
        "{incomplete}"
    );

    fs::remove_file(consumer).unwrap();
    fs::write(
        app,
        format!(
            "{configured}from configuration import replacement\napp.conf.update(**replacement)\n"
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
fn public_celery_task_declarations_keep_execution_separate() {
    let project = tempfile::tempdir().unwrap();
    let evidence = |report: &Value, name: &str, kind: &str, certainty: &str| {
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
                        .any(|entry| entry["kind"] == kind && entry["certainty"] == certainty)
            })
    };
    let jobs = project.path().join("jobs.py");
    let source = "from celery import Task\nclass PublicTask(Task):\n    def run(self): return helper()\ndef helper(): return 1\n";
    fs::write(&jobs, source).unwrap();
    let library = scan(project.path());
    assert!(
        evidence(
            &library,
            "jobs.PublicTask.run",
            "celery_task_declaration",
            "possible"
        ),
        "{library}"
    );
    assert_eq!(findings(&library, "unused_functions"), ["jobs.helper"]);
    assert!(!evidence(
        &library,
        "jobs.PublicTask.run",
        "celery_task",
        "certain"
    ));

    fs::write(
        &jobs,
        format!("from celery import Celery\napp = Celery('worker')\n{source}"),
    )
    .unwrap();
    let configured = scan(project.path());
    assert_eq!(
        unreachable_functions(&configured),
        ["jobs.PublicTask.run", "jobs.helper"]
    );
    assert!(!evidence(
        &configured,
        "jobs.PublicTask.run",
        "celery_task_declaration",
        "possible"
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
    let unused = findings(&report, "unused_functions");
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
    assert!(findings(&report, "unused_functions").contains(&"plugin.pytest_assertrepr_compare"));
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
fn fastapi_unannotated_websocket_query_accept_failure_preserves_unused_findings() {
    let project = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let root = project.path();
    let app = root.join("app.py");
    let project_source = r#"from fastapi import APIRouter, FastAPI
router = APIRouter(prefix="/accounts")
inactive = APIRouter()
@router.websocket("/ws")
async def connected(socket):
    """Call the socket protocol."""
    await socket.accept()
@inactive.websocket("/ws")
async def unmounted(socket):
    await socket.accept()
def unused():
    return 1
app = FastAPI()
app.include_router(router, prefix="/api")
"#;
    fs::write(&app, project_source).unwrap();
    let report = scan(root);
    assert_eq!(
        findings(&report, "unused_functions"),
        ["app.unmounted", "app.unused"]
    );
    assert!(
        report["dead_code_evidence"]["symbols"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| {
                row["qualified_name"] == "app.connected"
                    && row["evidence"].as_array().unwrap().iter().any(|edge| {
                        edge["kind"] == "fastapi_registration" && edge["certainty"] == "certain"
                    })
            }),
        "{report}"
    );
    for _ in 0..2 {
        let mut cached = scan_with_settings(root, None, Some(cache.path()));
        cached.as_object_mut().unwrap().remove("cache");
        assert_eq!(cached, report);
    }

    for changed in [
        project_source.replace("/accounts", "/{socket:custom}"),
        project_source.replace("/api", "/{socket:custom}"),
        project_source.replace(r#"websocket("/ws")"#, r#"websocket("/{socket:custom}")"#),
        project_source
            .replace(
                "from fastapi import APIRouter, FastAPI",
                "from fastapi import APIRouter, FastAPI, WebSocket",
            )
            .replace("connected(socket)", "connected(socket: WebSocket)"),
        project_source.replace("connected(socket)", "connected(socket=None)"),
        project_source.replace("router.websocket(", "router.websocket_route("),
        format!("{project_source}app.include_router(router, prefix='/{{socket:custom}}')\n"),
        project_source.replace(
            "app = FastAPI()",
            "from fastapi import WebSocket\nconnected.__annotations__ = {'socket': WebSocket}\napp = FastAPI()",
        ),
        format!("{project_source}app.add_api_route('/also-http', connected)\n"),
        format!("{project_source}from external import preserve\npreserve(connected)\n"),
        format!(
            "{project_source}class LocalSocket:\n    async def accept(self): return None\nasync def launch(): await connected(LocalSocket())\nimport asyncio\nasyncio.run(launch())\n"
        ),
        project_source.replace(
            "await socket.accept()",
            "await socket.accept()\n    return 1",
        ),
    ] {
        fs::write(&app, &changed).unwrap();
        let report = scan_with_exit(root, None, None, 2);
        assert!(
            report["analysis_errors"]
                .as_array()
                .unwrap()
                .iter()
                .any(|row| row["kind"] == "unresolved_callback"
                    && row["line"] == 7
                    && row["column"] == 5),
            "{changed}\n{report}"
        );
    }

    fs::write(
        &app,
        r#"class FakeRouter:
    def websocket(self, path):
        return lambda fn: fn
router = FakeRouter()
@router.websocket("/ws")
async def connected(socket):
    await socket.accept()
"#,
    )
    .unwrap();
    assert!(findings(&scan(root), "unused_functions").contains(&"app.connected"));
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
fn flat_django_shadow_does_not_whitelist_empty_admin_stubs() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    fs::create_dir(root.join("shop")).unwrap();
    fs::write(root.join("shop/__init__.py"), "").unwrap();
    fs::write(root.join("shop/admin.py"), "# Register models here.\n").unwrap();
    fs::write(
        root.join("shop/placeholder.py"),
        "# An ordinary empty file.\n",
    )
    .unwrap();
    fs::write(
        root.join("settings.py"),
        "from django.apps import AppConfig\n",
    )
    .unwrap();
    let empty_files = |report: &Value| {
        report["unused_files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["file"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>()
    };
    let genuine = scan(root);
    assert_eq!(empty_files(&genuine), ["shop/placeholder.py"]);

    fs::write(root.join("django.py"), "value = 1\n").unwrap();
    let shadowed = scan(root);
    assert_eq!(
        empty_files(&shadowed),
        ["shop/admin.py", "shop/placeholder.py"]
    );
    fs::write(root.join("consumer.py"), "import settings\n").unwrap();
    let attempted = scan_with_exit(root, None, None, 2);
    assert!(
        attempted["coverage"]["issues"]
            .as_array()
            .unwrap()
            .iter()
            .any(|issue| {
                issue["kind"] == "unresolved_import" && issue["path"] == "consumer.py"
            }),
        "{attempted}"
    );
}

#[test]
fn inert_flat_django_provider_keeps_competing_imports_unresolved() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    fs::write(root.join("django.py"), "class BaseCommand: pass\n").unwrap();
    fs::write(
        root.join("commands.py"),
        "from django.core.management.base import BaseCommand\nclass Command(BaseCommand): pass\n",
    )
    .unwrap();
    scan(root);

    fs::write(root.join("django.abi3.so"), "").unwrap();
    let opaque = scan_with_exit(root, None, None, 2);
    assert!(
        opaque["coverage"]["issues"]
            .as_array()
            .unwrap()
            .iter()
            .any(|issue| { issue["kind"] == "unresolved_import" }),
        "{opaque}"
    );
    fs::remove_file(root.join("django.abi3.so")).unwrap();

    fs::create_dir(root.join("django")).unwrap();
    fs::write(root.join("django/__init__.py"), "").unwrap();
    let excluded = scan_with_excludes(root, None, None, &["django"], 2);
    assert!(
        excluded["coverage"]["issues"]
            .as_array()
            .unwrap()
            .iter()
            .any(|issue| { issue["kind"] == "unresolved_import" }),
        "{excluded}"
    );
}

#[test]
fn inert_root_framework_packages_do_not_register_unavailable_providers() {
    for (provider, hidden, source) in [
        (
            "django",
            "django/db.py",
            "from django.db import migrations\ndef forwards(apps, editor): pass\noperation = migrations.RunPython(forwards)\n",
        ),
        (
            "celery",
            "celery/Celery.py",
            "from celery import Celery\napp = Celery('shop', task_routes=('routers.route_task',))\n",
        ),
    ] {
        let project = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let root = project.path();
        fs::create_dir(root.join(provider)).unwrap();
        fs::write(root.join(provider).join("__init__.py"), "").unwrap();
        fs::write(root.join("app.py"), source).unwrap();
        fs::write(
            root.join("routers.py"),
            "def route_task(name, args, kwargs, options): return None\n",
        )
        .unwrap();
        let missing = scan(root);
        let unused = findings(&missing, "unused_parameters");
        assert!(unused.contains(&"routers.route_task.args"));
        if provider == "django" {
            assert!(unused.contains(&"app.forwards.apps"));
        }
        let mut warm = scan_with_settings(root, None, Some(cache.path()));
        warm.as_object_mut().unwrap().remove("cache");
        assert_eq!(warm, missing);

        fs::write(root.join(hidden), "").unwrap();
        let hidden_report = scan_with_excludes(root, None, Some(cache.path()), &[hidden], 2);
        assert!(
            hidden_report["coverage"]["issues"]
                .as_array()
                .unwrap()
                .iter()
                .any(|issue| {
                    matches!(
                        issue["kind"].as_str(),
                        Some("unresolved_import" | "unresolved_call")
                    )
                }),
            "{hidden_report}"
        );
        fs::remove_file(root.join(hidden)).unwrap();

        fs::write(root.join("consumer.py"), "import app\n").unwrap();
        let consumer = scan_with_exit(root, None, None, 2);
        assert!(
            consumer["coverage"]["issues"]
                .as_array()
                .unwrap()
                .iter()
                .any(|issue| {
                    issue["kind"] == "unresolved_import" && issue["path"] == "consumer.py"
                }),
            "{consumer}"
        );
    }
}

#[test]
fn fastapi_shadows_an_unavailable_submodule_in_a_closed_regular_package() {
    let project = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let root = project.path();
    for package in ["app", "backend/app"] {
        fs::create_dir_all(root.join(package)).unwrap();
        fs::write(root.join(package).join("__init__.py"), "").unwrap();
    }
    let route = "from fastapi import APIRouter\nrouter = APIRouter()\n\
                 @router.get('/')\ndef endpoint(): return 1\n";
    fs::write(root.join("app/routes.py"), route).unwrap();
    let main = root.join("backend/app/main.py");
    for mount in [
        "from app.routes import router\napp.include_router(router)\n",
        "import app.routes as routes\napp.include_router(routes.router)\n",
    ] {
        fs::write(
            &main,
            format!("from fastapi import FastAPI\napp = FastAPI()\n{mount}"),
        )
        .unwrap();
        let report = scan(root);
        assert_eq!(
            findings(&report, "unused_functions"),
            ["app.routes.endpoint"]
        );
        assert!(
            report["analysis_summary"]["fastapi_route_declarations"]
                .as_array()
                .unwrap()
                .iter()
                .any(|row| {
                    row["qualified_name"] == "app.routes.endpoint" && row["file"] == "app/routes.py"
                })
        );
        for _ in 0..2 {
            let mut cached = scan_with_settings(root, None, Some(cache.path()));
            cached.as_object_mut().unwrap().remove("cache");
            assert_eq!(cached, report);
        }
    }

    for artifact in [
        "backend/app/routes.cpython-311.so",
        "backend/app/__init__.abi3.so",
        "backend/__init__.abi3.so",
        "backend/app/routes.pyc",
        "backend/app/routes.pyw",
    ] {
        let native_candidate = root.join(artifact);
        fs::write(&native_candidate, "").unwrap();
        let unavailable = scan_with_exit(root, None, Some(cache.path()), 2);
        assert!(
            unavailable["coverage"]["issues"]
                .as_array()
                .unwrap()
                .iter()
                .any(|issue| issue["kind"] == "unresolved_import")
        );
        fs::remove_file(native_candidate).unwrap();
    }

    for hidden in [
        "backend/__init__.py",
        "backend/app/routes.py",
        "backend/fastapi.py",
        "fastapi.py",
    ] {
        let source = root.join(hidden);
        fs::write(&source, "").unwrap();
        let unavailable = scan_with_excludes(root, None, Some(cache.path()), &[hidden], 2);
        assert!(
            unavailable["selected_paths"]
                .as_array()
                .unwrap()
                .iter()
                .all(|path| path != hidden)
        );
        assert!(
            unavailable["coverage"]["issues"]
                .as_array()
                .unwrap()
                .iter()
                .any(|issue| issue["kind"] == "unresolved_import")
        );
        fs::remove_file(source).unwrap();
    }

    let consumer = root.join("consumer.py");
    fs::write(&consumer, "from backend.app.main import app\n").unwrap();
    let consumer_failure = scan_with_exit(root, None, None, 2);
    assert!(
        consumer_failure["coverage"]["issues"]
            .as_array()
            .unwrap()
            .iter()
            .any(|issue| issue["kind"] == "unresolved_import" && issue["path"] == "consumer.py")
    );
    fs::remove_file(consumer).unwrap();

    fs::write(root.join("backend/app/routes.py"), route).unwrap();
    let resolved = scan_with_settings(root, None, Some(cache.path()));
    assert_eq!(
        findings(&resolved, "unused_functions"),
        ["app.routes.endpoint"]
    );
    assert!(
        resolved["dead_code_evidence"]["symbols"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| {
                row["qualified_name"] == "backend.app.routes.endpoint"
                    && row["evidence"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|edge| edge["kind"] == "fastapi_registration")
            })
    );
}

#[test]
fn fastapi_tool_config_leaves_namespace_route_selection_to_the_scan_root() {
    let project = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let root = project.path();
    let backend = root.join("backend");
    let namespace = backend.join("app");
    fs::create_dir_all(&namespace).unwrap();
    for directory in [root, backend.as_path()] {
        fs::write(
            directory.join("pyproject.toml"),
            "[tool.ruff]\nline-length = 100\n",
        )
        .unwrap();
    }
    fs::write(
        namespace.join("main.py"),
        "from fastapi import FastAPI\nfrom app.routes import router\n\napp = FastAPI()\napp.include_router(router)\n",
    )
    .unwrap();
    fs::write(
        namespace.join("routes.py"),
        "from fastapi import APIRouter\nrouter = APIRouter()\n\n@router.get('/namespace_route')\ndef namespace_route(): return 1\ndef dormant(): return 2\n",
    )
    .unwrap();

    let unresolved = scan_with_exit(root, None, None, 2);
    assert_eq!(
        unresolved["coverage"]["status"], "incomplete",
        "{unresolved}"
    );
    assert!(
        unresolved["analysis_errors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|issue| {
                issue["kind"] == "unresolved_registration"
                    && issue["file"] == "backend/app/main.py"
                    && issue["line"] == 5
            }),
        "{unresolved}"
    );
    assert!(
        unresolved["analysis_summary"]["fastapi_route_declarations"]
            .as_array()
            .unwrap()
            .iter()
            .any(|route| route["qualified_name"] == "backend.app.routes.namespace_route"),
        "{unresolved}"
    );
    assert!(
        !unresolved["dead_code_evidence"]["symbols"]
            .as_array()
            .unwrap()
            .iter()
            .any(|symbol| {
                symbol["qualified_name"] == "backend.app.routes.namespace_route"
                    && symbol["evidence"].as_array().unwrap().iter().any(|edge| {
                        edge["kind"] == "fastapi_registration" && edge["certainty"] == "certain"
                    })
            }),
        "{unresolved}"
    );

    let resolved = scan(&backend);
    assert_eq!(
        findings(&resolved, "unused_functions"),
        ["app.routes.dormant"],
        "{resolved}"
    );
    assert!(
        resolved["dead_code_evidence"]["symbols"]
            .as_array()
            .unwrap()
            .iter()
            .any(|symbol| {
                symbol["qualified_name"] == "app.routes.namespace_route"
                    && symbol["evidence"].as_array().unwrap().iter().any(|edge| {
                        edge["kind"] == "fastapi_registration" && edge["certainty"] == "certain"
                    })
            }),
        "{resolved}"
    );
    for _ in 0..2 {
        let mut cached = scan_with_settings(&backend, None, Some(cache.path()));
        cached.as_object_mut().unwrap().remove("cache");
        assert_eq!(cached, resolved);
    }
}

#[test]
fn fastapi_resolves_regular_packages_from_the_importing_service() {
    let project = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let root = project.path();
    for (directory, route) in [
        ("app", "root_endpoint"),
        ("services/one/app", "first_endpoint"),
        ("services/two/src/app", "second_endpoint"),
    ] {
        let package = root.join(directory);
        fs::create_dir_all(&package).unwrap();
        fs::write(package.join("__init__.py"), "").unwrap();
        fs::write(
            package.join("routes.py"),
            format!(
                "from fastapi import APIRouter\nrouter = APIRouter()\n@router.get('/')\ndef {route}(): return 1\n"
            ),
        )
        .unwrap();
    }
    fs::write(
        root.join("services/one/app/main.py"),
        "from fastapi import FastAPI\nimport app.routes\napp_server = FastAPI()\napp_server.include_router(app.routes.router)\n",
    )
    .unwrap();
    fs::write(
        root.join("services/two/src/app/main.py"),
        "from fastapi import FastAPI\nfrom app.routes import router\napp = FastAPI()\n",
    )
    .unwrap();

    let check = |report: &Value, expected: &[&str]| {
        assert_eq!(findings(report, "unused_functions"), expected, "{report}");
    };
    let before = scan(root);
    check(
        &before,
        &[
            "app.routes.root_endpoint",
            "services.two.src.app.routes.second_endpoint",
        ],
    );
    for _ in 0..2 {
        let cached = scan_with_settings(root, None, Some(cache.path()));
        assert_eq!(cached["unused_functions"], before["unused_functions"]);
        assert_eq!(cached["dead_code_evidence"], before["dead_code_evidence"]);
        assert_eq!(cached["coverage"], before["coverage"]);
        assert_eq!(
            cached["dead_code_abstentions"],
            before["dead_code_abstentions"]
        );
    }

    fs::write(
        root.join("services/one/app/main.py"),
        "from fastapi import FastAPI\nfrom app import routes\napp = FastAPI()\napp.include_router(routes.router)\n",
    )
    .unwrap();
    let edited = scan(root);
    check(
        &edited,
        &[
            "app.routes.root_endpoint",
            "services.two.src.app.routes.second_endpoint",
        ],
    );
    let cached = scan_with_settings(root, None, Some(cache.path()));
    assert_eq!(cached["unused_functions"], edited["unused_functions"]);
    assert_eq!(cached["dead_code_evidence"], edited["dead_code_evidence"]);
    assert_eq!(cached["coverage"], edited["coverage"]);
    assert_eq!(
        cached["dead_code_abstentions"],
        edited["dead_code_abstentions"]
    );

    fs::remove_file(root.join("services/one/app/routes.py")).unwrap();
    let missing = scan_with_exit(root, None, None, 2);
    assert_eq!(missing["coverage"]["status"], "incomplete", "{missing}");
    assert!(
        missing["analysis_errors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|error| {
                error["kind"] == "unresolved_import" && error["file"] == "services/one/app/main.py"
            })
    );
    assert!(
        missing["dead_code_evidence"]["symbols"]
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row["qualified_name"] != "app.routes.root_endpoint")
    );
    let cached = scan_with_exit(root, None, Some(cache.path()), 2);
    assert_eq!(cached["coverage"], missing["coverage"]);
    assert_eq!(cached["dead_code_evidence"], missing["dead_code_evidence"]);
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
fn fastapi_subclasses_preserve_inherited_and_overridden_route_methods() {
    let project = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    fs::write(
        project.path().join("web.py"),
        r#"from fastapi import FastAPI as App

class WorkerApp(App):
    @classmethod
    def build(cls):
        return cls()

class Overridden(App):
    def get(self, path):
        return lambda callback: callback

primary = WorkerApp.build()
overridden = Overridden()

@primary.get('/live')
def endpoint(): return helper()

@overridden.get('/unused')
def detached(): return abandoned()

def helper(): return 1
def abandoned(): return 2
"#,
    )
    .unwrap();
    let report = scan(project.path());
    assert_eq!(
        findings(&report, "unused_functions"),
        ["web.abandoned", "web.detached"]
    );
    let symbols = report["dead_code_evidence"]["symbols"].as_array().unwrap();
    assert!(
        symbols
            .iter()
            .any(|row| row["qualified_name"] == "web.endpoint"
                && row["evidence"].as_array().unwrap().iter().any(|edge| {
                    edge["kind"] == "fastapi_registration" && edge["certainty"] == "certain"
                }))
    );
    let mut cached = scan_with_settings(project.path(), None, Some(cache.path()));
    cached.as_object_mut().unwrap().remove("cache");
    assert_eq!(report, cached);

    fs::write(
        project.path().join("web.py"),
        r#"from fastapi import FastAPI

class Custom(FastAPI):
    def __init__(self):
        pass

    @classmethod
    def build(cls):
        return cls()

app = Custom.build()
@app.get('/')
def endpoint(): return 1
"#,
    )
    .unwrap();
    let report = scan_with_exit(project.path(), None, Some(cache.path()), 2);
    assert!(
        report["analysis_errors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|issue| {
                issue["kind"] == "unresolved_call" || issue["kind"] == "unresolved_decorator"
            })
    );
    assert!(
        !report["dead_code_evidence"]["symbols"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| {
                row["qualified_name"] == "web.endpoint"
                    && row["evidence"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|edge| edge["kind"] == "fastapi_registration")
            })
    );
}

#[test]
fn fastapi_subclass_inheritance_abstains_after_a_base_class_method_write() {
    let project = tempfile::tempdir().unwrap();
    fs::write(
        project.path().join("web.py"),
        r#"from fastapi import FastAPI
from fastapi.applications import FastAPI as Provider

class Application(FastAPI): pass
app = Application()

@app.get('/live')
def registered(): return 1

def no_registration(self, path):
    return lambda callback: callback

Provider.get = no_registration

@app.get('/phantom')
def phantom(): return 1
"#,
    )
    .unwrap();
    let report = scan_with_exit(project.path(), None, None, 2);
    let symbols = report["dead_code_evidence"]["symbols"].as_array().unwrap();
    for (name, live) in [("web.registered", true), ("web.phantom", false)] {
        assert_eq!(
            symbols.iter().any(|row| row["qualified_name"] == name
                && row["evidence"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|edge| edge["kind"] == "fastapi_registration")),
            live
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
    let dormant = scan(root);
    assert_eq!(
        unreachable_functions(&dormant),
        [
            "alternate.management.commands.record.Command.handle",
            "alternate.management.commands.record.record_message",
            "primary.management.commands.record.Command.handle",
            "primary.management.commands.record.record_message",
        ],
        "{dormant}"
    );
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

fn install_django_reloader_command(root: &Path, source: &str) {
    let commands = root.join("shop/management/commands");
    fs::create_dir_all(&commands).unwrap();
    for package in ["shop", "shop/management", "shop/management/commands"] {
        fs::write(root.join(package).join("__init__.py"), "").unwrap();
    }
    fs::write(root.join("settings.py"), "INSTALLED_APPS = ['shop']\n").unwrap();
    fs::write(commands.join("hello.py"), source).unwrap();
}

#[test]
fn django_selected_command_preserves_bound_autoreloader_capture_order() {
    let project = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let root = project.path();
    fs::write(
        root.join("manage.py"),
        "from django.core.management import execute_from_command_line\nexecute_from_command_line(['manage.py', 'hello', '--settings=settings'])\n",
    )
    .unwrap();
    let callback = "shop.management.commands.hello.callback";
    let capture = "        saved = self.start\n";
    let replace = "        self.start = lambda target: None\n";
    for (operations, expected) in [
        (format!("{capture}{replace}"), &["certain"][..]),
        (format!("{replace}{capture}"), &[][..]),
    ] {
        install_django_reloader_command(
            root,
            &format!(
                r#"from django.core.management.base import BaseCommand
from django.utils import autoreload

def callback():
    return 1

class Command(BaseCommand):
    def handle(self, **options):
{operations}        saved(callback)

    def start(self, target):
        autoreload.run_with_reloader(target)
"#
            ),
        );
        let report = scan(root);
        assert_eq!(
            evidence_certainties(&report, callback, "django_autoreload"),
            expected
        );
        for _ in 0..2 {
            let mut cached = scan_with_settings(root, None, Some(cache.path()));
            cached.as_object_mut().unwrap().remove("cache");
            assert_eq!(cached, report);
        }
    }
}

#[test]
fn django_selected_command_dispatches_reloader_only_with_bound_options() {
    let project = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let root = project.path();
    let source = r#"from django.core.management.base import BaseCommand
from django.utils import autoreload

def restart(name, queue):
    print(f"{name} on {queue}")

def dormant():
    return 1

class Command(BaseCommand):
    help = "Restart a worker"

    def add_arguments(self, parser):
        parser.add_argument("-Q", "--queue", required=True)
        parser.add_argument("-n", "--name", required=True)

    def handle(self, *args, **options):
        autoreload.run_with_reloader(restart, options["name"], options["queue"])
"#;
    install_django_reloader_command(root, source);
    let launcher = |args| {
        fs::write(
            root.join("manage.py"),
            format!(
                "from django.core.management import execute_from_command_line\nexecute_from_command_line(['manage.py', 'hello', '--settings=settings', {args}])\n"
            ),
        )
        .unwrap();
    };
    let callback = "shop.management.commands.hello.restart";
    launcher("'--name=night', '-Q', 'main'");
    let report = scan(root);
    assert_eq!(
        evidence_certainties(&report, callback, "django_autoreload"),
        ["certain"]
    );
    assert!(
        findings(&report, "unused_functions").contains(&"shop.management.commands.hello.dormant")
    );
    for _ in 0..2 {
        let mut cached = scan_with_settings(root, None, Some(cache.path()));
        cached.as_object_mut().unwrap().remove("cache");
        assert_eq!(cached, report);
    }
    for bad in [
        "'--name=night'",
        "'--name=night', '-Q', 'main', '--unknown'",
    ] {
        launcher(bad);
        let incomplete = scan_with_exit(root, None, Some(cache.path()), 2);
        assert!(evidence_certainties(&incomplete, callback, "django_autoreload").is_empty());
    }
    for reserved in ["args", "stdout", "stderr"] {
        let source = source
            .replace("\"--queue\"", &format!("\"--{reserved}\""))
            .replace("options[\"queue\"]", &format!("options[\"{reserved}\"]"));
        install_django_reloader_command(root, &source);
        launcher("'--name=night', '-Q', 'main'");
        let incomplete = scan_with_exit(root, None, Some(cache.path()), 2);
        assert!(evidence_certainties(&incomplete, callback, "django_autoreload").is_empty());
    }
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
fn flat_unpack_keeps_assignment_order_and_reports_active_unresolved_iteration() {
    let project = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let root = project.path();
    let source = root.join("entry.py");
    let code = r#"def first():
    return 'first'

def second():
    return 'second'

chosen, inactive = first, second
[inactive, chosen] = [chosen, inactive]
chosen()

def unpack_external(source):
    callback, label = source
    callback(label)
"#;
    fs::write(&source, code).unwrap();
    let report = scan(root);
    assert_eq!(
        unreachable_functions(&report),
        ["entry.first", "entry.unpack_external"]
    );
    for _ in 0..2 {
        let cached = scan_with_settings(root, None, Some(cache.path()));
        for field in [
            "coverage",
            "unused_functions",
            "unused_variables",
            "dead_code_evidence",
        ] {
            assert_eq!(report[field], cached[field], "{field}");
        }
    }

    fs::write(
        &source,
        format!("{code}\nfrom external import opaque\nunpack_external(opaque)\n"),
    )
    .unwrap();
    let report = scan_with_exit(root, None, Some(cache.path()), 2);
    assert!(
        report["coverage"]["issues"]
            .as_array()
            .unwrap()
            .iter()
            .any(|issue| {
                issue["kind"] == "unresolved_unpack"
                    && issue["path"] == "entry.py"
                    && issue["line"] == 12
            })
    );
}

#[test]
fn flat_unpack_evaluates_operands_before_rejecting_wrong_arity() {
    let project = tempfile::tempdir().unwrap();
    let source = project.path().join("entry.py");
    fs::write(&source, "def before():\n    return 1\n\ndef after():\n    return 2\n\nfirst, second = (before(),)\nafter()\n").unwrap();
    let report = scan_with_exit(project.path(), None, None, 2);
    assert!(
        report["coverage"]["issues"]
            .as_array()
            .unwrap()
            .iter()
            .any(|issue| {
                issue["kind"] == "invalid_unpack"
                    && issue["path"] == "entry.py"
                    && issue["line"] == 7
            })
    );
    let symbols = report["dead_code_evidence"]["symbols"].as_array().unwrap();
    let direct_call = |name: &str| {
        symbols.iter().any(|symbol| {
            symbol["qualified_name"] == name
                && symbol["evidence"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|edge| edge["kind"] == "direct_call" && edge["certainty"] == "certain")
        })
    };
    assert!(direct_call("entry.before"), "{report}");
    assert!(!direct_call("entry.after"), "{report}");
}

#[test]
fn string_enum_joins_use_only_the_reached_literal_members() {
    let project = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let root = project.path();
    let source = root.join("entry.py");
    let code = r#"from enum import Enum as Tags

class Data(str, Tags):
    FIRST = 'a'
    SECOND = 'b'
    ALIAS = 'a'

def expected():
    return 1

def unexpected():
    return 2

pattern = '^({})$'.format('|'.join(item.value for item in Data))
if pattern == '^(a|b)$':
    expected()
else:
    unexpected()
"#;
    fs::write(&source, code).unwrap();
    let report = scan(root);
    assert_eq!(unreachable_functions(&report), ["entry.unexpected"]);
    assert_eq!(findings(&report, "unused_variables"), ["entry.Data.ALIAS"]);
    for _ in 0..2 {
        let mut cached = scan_with_settings(root, None, Some(cache.path()));
        cached.as_object_mut().unwrap().remove("cache");
        assert_eq!(cached, report);
    }

    // A project-owned class has a distinct identity from the standard Enum.
    fs::write(root.join("enum.py"), "class Enum:\n    pass\n").unwrap();
    let report = scan_with_exit(root, None, Some(cache.path()), 2);
    assert!(
        report["coverage"]["issues"]
            .as_array()
            .unwrap()
            .iter()
            .any(|issue| {
                issue["kind"] == "unresolved_generator_execution" && issue["path"] == "entry.py"
            })
    );
}

#[test]
fn exact_string_split_supplies_values_and_arity_for_flat_unpacking() {
    let project = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let root = project.path();
    let source = root.join("entry.py");
    let code = r#"def selected(): return 1
def dormant(): return 2
text = 'active::'
separator = '::'
first, second = text.split(separator)
if first == 'active' and second == '':
    selected()
else:
    dormant()
"#;
    fs::write(&source, code).unwrap();
    let report = scan(root);
    assert_eq!(unreachable_functions(&report), ["entry.dormant"]);
    for _ in 0..2 {
        let mut cached = scan_with_settings(root, None, Some(cache.path()));
        cached.as_object_mut().unwrap().remove("cache");
        assert_eq!(cached, report);
    }

    fs::write(&source, code.replace("active::", "active::extra::")).unwrap();
    let report = scan_with_exit(root, None, Some(cache.path()), 2);
    assert!(
        report["coverage"]["issues"]
            .as_array()
            .unwrap()
            .iter()
            .any(|issue| { issue["kind"] == "invalid_unpack" }),
        "{report}"
    );

    fs::write(
        &source,
        code.replace("text = 'active::'", "text = factory()"),
    )
    .unwrap();
    let report = scan_with_exit(root, None, None, 2);
    assert!(
        report["coverage"]["issues"]
            .as_array()
            .unwrap()
            .iter()
            .any(|issue| { issue["kind"] == "unresolved_unpack" }),
        "{report}"
    );
}

#[test]
fn str_uses_the_instance_string_hook_with_repr_as_a_fallback() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    fs::write(
        root.join("entry.py"),
        r#"class Explicit:
    def __format__(self, specification): return formatted()
    def __str__(self): return selected()
    def __repr__(self): return bypassed()

class Fallback:
    def __repr__(self): return fallback()

def selected(): return 'explicit'
def fallback(): return 'fallback'
def formatted(): return 'format'
def bypassed(): return 'repr'

str(Explicit())
str(Fallback())
"#,
    )
    .unwrap();
    let report = scan(root);
    assert_eq!(
        unreachable_functions(&report),
        ["entry.bypassed", "entry.formatted"]
    );
}

#[test]
fn constructor_proves_plain_field_writes_without_bypassing_setattr() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    let source = root.join("entry.py");
    let code = r#"class Pair:
    def __init__(self, name: str, count: int):
        self.name = name
        self.count = count

    def __str__(self):
        return 'pair'

Pair('example', 2)
"#;
    fs::write(&source, code).unwrap();
    let report = scan(root);
    let symbols = report["dead_code_evidence"]["symbols"].as_array().unwrap();
    assert!(symbols.iter().any(|symbol| {
        symbol["qualified_name"] == "entry.Pair.__init__"
            && symbol["evidence"]
                .as_array()
                .unwrap()
                .iter()
                .any(|edge| edge["kind"] == "constructor" && edge["certainty"] == "certain")
    }));

    fs::write(
        &source,
        code.replace(
            "    def __str__",
            "    def __setattr__(self, field, value):\n        field(value)\n\n    def __str__",
        ),
    )
    .unwrap();
    let report = scan_with_exit(root, None, None, 2);
    assert!(
        report["coverage"]["issues"]
            .as_array()
            .unwrap()
            .iter()
            .any(|issue| {
                issue["kind"] == "unresolved_attribute_write" && issue["path"] == "entry.py"
            })
    );
}

#[test]
fn direct_class_parameters_open_only_their_proven_static_calls() {
    let project = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let root = project.path();
    let wrapper = root.join("wrapper.py");
    let caller = root.join("app.py");
    let wrapper_code = "def install(converter, label):\n    _ = label\n    _ = converter.regex\n    converter.convert('sample')\n";
    let caller_code = r#"from wrapper import install

def dependency(value):
    return value

class Active:
    regex = '[a-z]+'

    @staticmethod
    def convert(value):
        return dependency(value)

class Inactive:
    regex = '[a-z]+'

    @staticmethod
    def convert(value):
        return value

def dormant():
    install(Inactive, 'retired')

install(Active, 'current')
"#;
    fs::write(&wrapper, wrapper_code).unwrap();
    fs::write(&caller, caller_code).unwrap();
    let report = scan(root);
    assert_eq!(
        unreachable_functions(&report),
        ["app.Inactive.convert", "app.dormant"]
    );
    for _ in 0..2 {
        let mut cached = scan_with_settings(root, None, Some(cache.path()));
        cached.as_object_mut().unwrap().remove("cache");
        assert_eq!(cached, report);
    }

    let expect_unresolved_wrapper = |report: &Value| {
        assert!(
            report["coverage"]["issues"]
                .as_array()
                .unwrap()
                .iter()
                .any(|issue| {
                    issue["kind"] == "unresolved_call" && issue["path"] == "wrapper.py"
                }),
            "{report}"
        );
    };
    fs::write(
        &caller,
        format!("{caller_code}\nfrom external import opaque\ninstall(opaque, 'unknown')\n"),
    )
    .unwrap();
    expect_unresolved_wrapper(&scan_with_exit(root, None, Some(cache.path()), 2));

    fs::write(
        &caller,
        caller_code.replace("install(Active,", "Active.convert = print\ninstall(Active,"),
    )
    .unwrap();
    let patched = scan_with_exit(root, None, Some(cache.path()), 2);
    expect_unresolved_wrapper(&patched);
    assert!(
        !patched["dead_code_evidence"]["symbols"]
            .as_array()
            .unwrap()
            .iter()
            .any(|symbol| {
                symbol["qualified_name"] == "app.Active.convert"
                    && symbol["evidence"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|edge| edge["kind"] == "direct_call")
            }),
        "{patched}"
    );

    fs::write(&caller, caller_code.replace("install(Active,", "def patch(target):\n    setattr(target, 'convert', print)\n\npatch(Active)\ninstall(Active,")).unwrap();
    expect_unresolved_wrapper(&scan_with_exit(root, None, Some(cache.path()), 2));

    fs::write(&caller, caller_code).unwrap();
    for changed in [
        wrapper_code.replace(
            "    converter.convert('sample')",
            "    result = converter.convert('sample')\n    result()",
        ),
        wrapper_code.replace(
            "    converter.convert('sample')",
            "    setattr(converter, 'convert', print)\n    converter.convert('sample')",
        ),
    ] {
        fs::write(&wrapper, changed).unwrap();
        expect_unresolved_wrapper(&scan_with_exit(root, None, Some(cache.path()), 2));
    }
}

#[test]
fn nested_static_arguments_keep_value_flow_local_to_the_selected_calls() {
    let project = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let root = project.path();
    let wrapper = root.join("wrapper.py");
    let caller = root.join("app.py");
    fs::write(&wrapper, "def install(converter, sample):\n    _ = converter.regex\n    converter.parse('mg_1')\n    converter.render(converter.parse(sample))\n").unwrap();
    let definitions = r#"from enum import Enum
from wrapper import install

class Category(str, Enum):
    GAME = "mg"

class Key:
    def __init__(self, category, number):
        self.category = category
        self.number = number

    def __str__(self):
        return f"{self.category.value}_{self.number}"

class Converter:
    regex = '[a-z]+'

    @staticmethod
    def parse(text):
        category, number = text.split("_")
        return Key(Category(category), int(number))

    @staticmethod
    def render(key):
        return str(key)

def retired():
    return 1
"#;
    fs::write(&caller, definitions).unwrap();
    let dormant = scan(root);
    assert!(unreachable_functions(&dormant).contains(&"app.Converter.render"));

    let entry = format!("{definitions}\ninstall(Converter, 'mg_2')\n");
    fs::write(&caller, &entry).unwrap();
    let active = scan(root);
    let symbols = active["dead_code_evidence"]["symbols"].as_array().unwrap();
    for (name, kind) in [
        ("app.Converter.parse", "direct_call"),
        ("app.Converter.render", "direct_call"),
        ("app.Key.__str__", "formatting_hook"),
    ] {
        assert!(
            symbols.iter().any(|symbol| {
                symbol["qualified_name"] == name
                    && symbol["evidence"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|edge| edge["kind"] == kind)
            }),
            "{active}"
        );
    }
    assert_eq!(findings(&active, "unused_functions"), ["app.retired"]);
    for _ in 0..2 {
        let mut cached = scan_with_settings(root, None, Some(cache.path()));
        cached.as_object_mut().unwrap().remove("cache");
        assert_eq!(cached, active);
    }

    for bad_call in [
        "install(Converter, 'mg')",                             // Arity differs.
        "install(Converter, 'te_2')",                           // Enum value is absent.
        "install(Converter, 'mg_two')",                         // Integer conversion is unproved.
        "from external import opaque\nConverter.parse(opaque)", // A separate call can supply any value.
    ] {
        fs::write(&caller, format!("{entry}\n{bad_call}\n")).unwrap();
        let report = scan_with_exit(root, None, Some(cache.path()), 2);
        assert!(
            report["coverage"]["issues"]
                .as_array()
                .unwrap()
                .iter()
                .any(|issue| { issue["kind"] == "unresolved_unpack" }),
            "{report}"
        );
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
fn django_converter_wrapper_uses_the_module_binding_at_its_call() {
    let project = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let root = project.path();
    let src = r#"from django.urls import register_converter

class Converter:
    regex = '[a-z]+'
    def to_python(self, value):
        return value
    def to_url(self, value):
        return value

def configure():
    register_converter(Converter, 'custom')

def replacement(cls, name):
    return None

register_converter = replacement
configure()
"#;
    let path = root.join("routes.py");
    fs::write(&path, src).unwrap();
    let report = scan(root);
    assert_eq!(
        findings(&report, "unused_functions"),
        ["routes.Converter.to_python", "routes.Converter.to_url"]
    );
    assert!(findings(&report, "unused_variables").contains(&"routes.Converter.regex"));
    for _ in 0..2 {
        let cached = scan_with_settings(root, None, Some(cache.path()));
        for field in ["coverage", "dead_code_evidence", "unused_functions"] {
            assert_eq!(report[field], cached[field], "{field}");
        }
    }
    for mutation in [
        "from external import opaque\nregister_converter = opaque\nconfigure()\n",
        "def separately_called():\n    configure()\nseparately_called()\n",
    ] {
        fs::write(&path, format!("{src}\n{mutation}")).unwrap();
        let report = scan_with_exit(root, None, Some(cache.path()), 2);
        assert!(
            report["coverage"]["issues"]
                .as_array()
                .unwrap()
                .iter()
                .any(|issue| issue["kind"] == "ambiguous_binding"),
            "{mutation}: {report}"
        );
    }
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
    assert!(findings(&report, "unused_functions").is_empty());
    assert_eq!(
        findings(&report, "whitelisted"),
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
    assert!(findings(&report, "unused_functions").is_empty());
    assert_eq!(
        findings(&report, "whitelisted"),
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
        [
            "urls.Audited.get",
            "urls.Dormant.setup",
            "urls.dormant_setup"
        ],
        "{report}"
    );
    assert_setup_evidence(&report);
    for hits in [0, 3] {
        let mut cached = scan_with_settings(root, None, Some(cache.path()));
        let stats = cached.as_object_mut().unwrap().remove("cache").unwrap();
        assert_eq!(stats["hits"], hits);
        assert_eq!(cached, report);
    }

    write_views("        self.request = request");
    let report = scan(root);
    assert_setup_evidence(&report);
    assert_eq!(
        findings(&report, "unused_functions"),
        ["urls.Dormant.setup", "urls.dormant_setup"],
        "{report}"
    );

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
fn django_signal_registration_follows_cross_module_global_callback_writes() {
    let project = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let root = project.path();
    let source = r#"from . import callbacks

def wrapper():
    callbacks.install()

def dormant_reader():
    return callbacks.callback

wrapper()
callbacks.select()
wrapper()
"#;
    django_signal_project(root, source);
    fs::write(
        root.join("inventory/callbacks.py"),
        r#"from django.db.models.signals import post_save

def first(sender, **kwargs):
    return first_helper()

def first_helper():
    return 1

def second(sender, **kwargs):
    return second_helper()

def second_helper():
    return 2

callback = first

def install():
    post_save.connect(callback)

def select():
    global callback
    callback = second
"#,
    )
    .unwrap();
    let report = scan(root);
    assert_eq!(
        django_signal_evidence(&report),
        [
            ("inventory.callbacks.first", "certain"),
            ("inventory.callbacks.second", "certain")
        ]
    );
    for _ in 0..2 {
        let cached = scan_with_settings(root, None, Some(cache.path()));
        for field in ["unused_functions", "coverage", "dead_code_evidence"] {
            assert_eq!(report[field], cached[field], "{field}");
        }
    }
    let signals = root.join("inventory/signals.py");
    fs::write(
        &signals,
        source.replacen("wrapper()\ncallbacks.select()", "callbacks.select()", 1),
    )
    .unwrap();
    let edited = scan_with_settings(root, None, Some(cache.path()));
    assert_eq!(
        django_signal_evidence(&edited),
        [("inventory.callbacks.second", "certain")]
    );
    let clean = scan(root);
    for field in ["unused_functions", "coverage", "dead_code_evidence"] {
        assert_eq!(clean[field], edited[field], "{field}");
    }
    for unsupported in [
        source.replace(
            "callbacks.select()",
            "from external import enabled\nif enabled:\n    callbacks.select()",
        ),
        format!("{source}\nobserved = callbacks.callback\n"),
        "from . import callbacks\nfrom django.db.models.signals import post_save\ncallbacks.select()\npost_save.connect(callbacks.callback)\n".into(),
        "from . import callbacks\nfrom django.db.models.signals import post_save\ncallbacks.select()\nfrom .callbacks import callback\npost_save.connect(callback)\n".into(),
    ] {
        fs::write(&signals, unsupported).unwrap();
        let report = scan_with_exit(root, None, Some(cache.path()), 2);
        assert!(django_signal_evidence(&report).is_empty(), "{report}");
        assert!(
            report["coverage"]["issues"]
                .as_array()
                .unwrap()
                .iter()
                .any(|issue| issue["kind"] == "unresolved_global_binding")
        );
    }
}

#[test]
fn django_signal_registration_stops_at_a_proven_terminal_import() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    django_signal_project(
        root,
        r#"from django.db.models.signals import post_save

def before_failure(sender, **kwargs): return 1
def after_failure(sender, **kwargs): return 2

post_save.connect(before_failure)
from . import broken
post_save.connect(after_failure)
"#,
    );
    let broken_path = root.join("inventory/broken.py");
    let broken =
        "from celery import Celery\napp = Celery('worker')\napp.conf.update(**missing_settings)\n";
    fs::write(&broken_path, broken).unwrap();
    let report = scan_with_exit(root, None, None, 2);
    assert_eq!(
        django_signal_evidence(&report),
        [("inventory.signals.before_failure", "certain")],
        "{report}"
    );
    assert!(
        report["coverage"]["issues"]
            .as_array()
            .unwrap()
            .iter()
            .any(|issue| issue["path"] == "inventory/signals.py"
                && issue["kind"] == "unresolved_import"
                && issue["message"]
                    .as_str()
                    .is_some_and(|message| message.contains("inventory.broken"))),
        "{report}"
    );

    fs::write(broken_path, format!("from . import signals\n{broken}")).unwrap();
    let report = scan_with_exit(root, None, None, 2);
    assert!(
        report["coverage"]["issues"]
            .as_array()
            .unwrap()
            .iter()
            .any(|issue| issue["kind"] == "cyclic_imports"),
        "{report}"
    );
}

#[test]
fn django_signal_global_callback_write_respects_the_module_initializer() {
    let project = tempfile::tempdir().unwrap();
    django_signal_project(
        project.path(),
        r#"from django.db.models.signals import post_save

def first(sender, **kwargs): return 1
def second(sender, **kwargs): return 2

def select():
    global callback
    callback = second

select()
callback = first
post_save.connect(callback)
"#,
    );
    let report = scan(project.path());
    assert_eq!(
        django_signal_evidence(&report),
        [("inventory.signals.first", "certain")]
    );
    assert!(findings(&report, "unused_functions").contains(&"inventory.signals.second"));
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
fn django_signal_subclass_forwarding_preserves_runtime_condition_and_coverage() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    let cache = tempfile::tempdir().unwrap();
    let source = r#"import os
from django.dispatch import Signal, receiver
from django.db.models.signals import post_save

def connector(callback):
    if os.getenv('SIGNAL_ENABLED'):
        post_save.connect(callback)

class DomainSignal(Signal):
    def connect(self, callback):
        return connector(callback)

saved = DomainSignal()

@receiver(saved)
def on_save(sender, instance, **kwargs):
    return helper()

def helper(): return 1
def abandoned(): return 2
"#;
    django_signal_project(root, source);
    let report = scan(root);
    assert_eq!(
        django_signal_evidence(&report),
        [("inventory.signals.on_save", "possible")],
    );
    assert_eq!(
        findings(&report, "unused_functions"),
        ["inventory.signals.abandoned"]
    );
    for _ in 0..2 {
        let mut cached = scan_with_settings(root, None, Some(cache.path()));
        cached.as_object_mut().unwrap().remove("cache");
        assert_eq!(cached, report);
    }

    fs::write(
        root.join("inventory/signals.py"),
        source.replace("os.getenv('SIGNAL_ENABLED')", "unknown_provider()"),
    )
    .unwrap();
    let incomplete = scan_with_exit(root, None, Some(cache.path()), 2);
    assert!(
        incomplete["coverage"]["issues"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["kind"] == "unresolved_registration"),
    );
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
fn atexit_forwarding_decorator_preserves_the_callback_in_a_local_stack() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    fs::write(
        root.join("app.py"),
        "from plugin import register_entry\nclass Local:\n    def get(self, path): return lambda fn: fn\nfake = Local()\n@register_entry\n@fake.get('/')\ndef endpoint(): return helper()\ndef helper(): return 1\ndef unused(): return 2\n",
    )
    .unwrap();
    let plugin = root.join("plugin.py");
    let wrapper = "from atexit import register\ndef register_entry(callback):\n    register(callback)\n    return callback\n";
    fs::write(&plugin, wrapper).unwrap();
    let report = scan(root);
    assert_eq!(
        findings(&report, "unused_functions"),
        ["app.unused"],
        "{report}"
    );
    assert_eq!(
        evidence_certainties(&report, "app.endpoint", "atexit_registration"),
        ["possible"]
    );
    assert_eq!(
        evidence_certainties(&report, "app.helper", "direct_call"),
        ["possible"]
    );
    assert!(evidence_certainties(&report, "app.endpoint", "fastapi_registration").is_empty());
    let cache = tempfile::tempdir().unwrap();
    for _ in 0..2 {
        let mut cached = scan_with_settings(root, None, Some(cache.path()));
        cached.as_object_mut().unwrap().remove("cache");
        assert_eq!(cached, report);
    }

    fs::write(&plugin, wrapper.replace("register(callback)", "callback()")).unwrap();
    let immediate = scan_with_exit(root, None, Some(cache.path()), 2);
    assert_eq!(immediate["coverage"]["status"], "incomplete", "{immediate}");
    assert!(evidence_certainties(&immediate, "app.endpoint", "atexit_registration").is_empty());
}

#[test]
fn atexit_forwarding_decorator_checks_forwarded_literal_arguments() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    fs::write(
        root.join("app.py"),
        "from plugin import receiver\n@receiver\ndef saved(sender, instance, **kwargs): return helper()\ndef helper(): return 1\ndef abandoned(): return 2\n",
    )
    .unwrap();
    let plugin = root.join("plugin.py");
    let wrapper = "from atexit import register\ndef receiver(callback):\n    register(callback, None, None)\n    return callback\n";
    fs::write(&plugin, wrapper).unwrap();
    let report = scan(root);
    assert_eq!(findings(&report, "unused_functions"), ["app.abandoned"]);
    assert_eq!(
        evidence_certainties(&report, "app.saved", "atexit_registration"),
        ["possible"],
    );
    assert!(evidence_certainties(&report, "app.saved", "direct_call").is_empty());

    for register in [
        "register(callback, None)",
        "register(callback, opaque(), None)",
    ] {
        fs::write(
            &plugin,
            wrapper.replace("register(callback, None, None)", register),
        )
        .unwrap();
        let report = scan_with_exit(root, None, None, 2);
        assert!(evidence_certainties(&report, "app.saved", "atexit_registration").is_empty());
    }
}

#[test]
fn atexit_decorator_factory_retains_only_the_source_proven_returned_wrapper() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    fs::write(
        root.join("app.py"),
        "from plugin import select\nevent = 'saved'\n@select(event)\ndef saved(sender, instance, **kwargs): return helper()\ndef helper(): return 1\ndef abandoned(): return 2\n",
    )
    .unwrap();
    let plugin = root.join("plugin.py");
    let wrapper = "from atexit import register\ndef select(event):\n    return receiver\ndef receiver(callback):\n    register(callback, None, None)\n    return callback\n";
    fs::write(&plugin, wrapper).unwrap();
    let report = scan(root);
    assert_eq!(findings(&report, "unused_functions"), ["app.abandoned"]);
    assert_eq!(
        evidence_certainties(&report, "app.saved", "atexit_registration"),
        ["possible"],
    );
    assert!(evidence_certainties(&report, "app.saved", "direct_call").is_empty());

    fs::write(
        &plugin,
        wrapper.replace(
            "    return receiver",
            "    opaque(event)\n    return receiver",
        ),
    )
    .unwrap();
    let report = scan_with_exit(root, None, None, 2);
    assert!(evidence_certainties(&report, "app.saved", "atexit_registration").is_empty());
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

#[test]
fn django_setting_class_declaration_does_not_activate_replaced_protocol_callbacks() {
    let project = tempfile::tempdir().unwrap();
    let package = project.path().join("service");
    fs::create_dir(&package).unwrap();
    fs::write(package.join("__init__.py"), "").unwrap();
    fs::write(
        package.join("settings.py"),
        concat!(
            "INSTALLED_APPS = []\n",
            "SECRET_KEY = 'fixture'\n",
            "LOGGING = {'filters': {'selected': {'()': 'service.backend.Filter'}}}\n",
            "LOGGING = {}\n",
            "STORAGES = {'default': {'BACKEND': 'service.backend.Declared'}}\n",
            "NOTE = 'service.backend.Unrelated'\n",
        ),
    )
    .unwrap();
    fs::write(
        package.join("backend.py"),
        concat!(
            "from pydantic import BaseModel\n",
            "class Filter:\n",
            "    def filter(self, record): return helper()\n",
            "class Declared(BaseModel):\n",
            "    def model_post_init(self, context): return other_helper()\n",
            "class Unrelated: pass\n",
            "def helper(): return True\n",
            "def other_helper(): return True\n",
        ),
    )
    .unwrap();
    let report = scan(project.path());
    assert_eq!(
        findings(&report, "unused_classes"),
        ["service.backend.Unrelated"]
    );
    assert_eq!(
        findings(&report, "unused_functions"),
        [
            "service.backend.Declared.model_post_init",
            "service.backend.Filter.filter",
            "service.backend.helper",
            "service.backend.other_helper",
        ]
    );
    let symbols = report["dead_code_evidence"]["symbols"].as_array().unwrap();
    assert!(symbols.iter().any(|symbol| {
        symbol["qualified_name"] == "service.backend.Filter"
            && symbol["evidence"].as_array().unwrap().iter().any(|event| {
                event["kind"] == "django_configured_class_declaration"
                    && event["certainty"] == "possible"
            })
    }));
    assert!(
        !symbols
            .iter()
            .any(|symbol| symbol["qualified_name"] == "service.backend.Filter.filter")
    );
}

#[test]
fn django_effective_settings_keep_only_possible_protocol_callbacks() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    let package = root.join("service");
    fs::create_dir(&package).unwrap();
    for (name, source) in [
        ("__init__.py", ""),
        (
            "settings.py",
            "INSTALLED_APPS = ['service']\nSECRET_KEY = 'fixture'\nfrom .config import BACKENDS as AUTHENTICATION_BACKENDS, LOGGING, TEMPLATES\nif enabled:\n    LOGGING = {}\nSTORAGES = {'default': {'BACKEND': 'service.backends.Old'}, 'default': {'BACKEND': 'service.backends.Store'}}\n",
        ),
        (
            "config.py",
            "BACKENDS = ('service.backends.Configured',)\nLOGGING = {'version': 1, 'filters': {'active': {'()': 'service.backends.Filter'}}}\nTEMPLATES = [{'BACKEND': 'django.template.backends.django.DjangoTemplates', 'OPTIONS': {'context_processors': ['service.backends.context']}}]\n",
        ),
        (
            "backends.py",
            r#"class Base:
    def get_user(self, user_id): return False
class Left(Base):
    pass
class Right(Base):
    def get_user(self, user_id): return lookup(user_id)
    async def aget_user(self, user_id): return lookup(user_id)
class Configured(Left, Right):
    pass
class Filter:
    def filter(self, record): return valid(record)
    def unrelated(self): return True
class Store:
    def save(self, name, content): return name
class Old:
    def save(self, name, content): return name

def lookup(user_id): return user_id
def valid(record): return True
def context(request): return {}
"#,
        ),
    ] {
        fs::write(package.join(name), source).unwrap();
    }
    let report = scan(root);
    let symbols = report["dead_code_evidence"]["symbols"].as_array().unwrap();
    let edges = |name: &str, kind: &str| {
        symbols
            .iter()
            .filter(|item| item["qualified_name"] == name)
            .flat_map(|item| item["evidence"].as_array().unwrap())
            .filter(|edge| edge["kind"] == kind)
            .map(|edge| edge["certainty"].as_str().unwrap())
            .collect::<Vec<_>>()
    };
    for (name, kind) in [
        ("service.backends.Right.get_user", "django_auth_backend"),
        ("service.backends.Right.aget_user", "django_auth_backend"),
        ("service.backends.Filter.filter", "django_logging_factory"),
        ("service.backends.Store.save", "django_storage_backend"),
        ("service.backends.context", "django_setting_reference"),
        ("service.backends.lookup", "direct_call"),
    ] {
        assert_eq!(edges(name, kind), ["possible"], "{name}: {report}");
    }
    for (name, kind) in [
        ("service.backends.Base.get_user", "django_auth_backend"),
        ("service.backends.Old.save", "django_storage_backend"),
        (
            "service.backends.Filter.unrelated",
            "django_logging_factory",
        ),
    ] {
        assert!(edges(name, kind).is_empty(), "{name}: {report}");
    }
    let cache = tempfile::tempdir().unwrap();
    for _ in 0..2 {
        let mut cached = scan_with_settings(root, None, Some(cache.path()));
        cached.as_object_mut().unwrap().remove("cache");
        assert_eq!(cached, report);
    }
    fs::write(
        package.join("config.py"),
        "BACKENDS = []\nLOGGING = {}\nTEMPLATES = []\n",
    )
    .unwrap();
    let edited = scan(root);
    let mut cached = scan_with_settings(root, None, Some(cache.path()));
    cached.as_object_mut().unwrap().remove("cache");
    assert_eq!(cached, edited);
    assert!(
        findings(&edited, "unused_functions").contains(&"service.backends.lookup"),
        "{edited}"
    );
    let settings = fs::read_to_string(package.join("settings.py")).unwrap();
    fs::write(
        package.join("settings.py"),
        format!("{settings}from . import backends\nsetattr(backends.Right, 'get_user', None)\n"),
    )
    .unwrap();
    fs::write(
        package.join("config.py"),
        "BACKENDS = ['service.backends.Configured']\nLOGGING = {}\nTEMPLATES = []\n",
    )
    .unwrap();
    let replaced = scan(root);
    let symbols = replaced["dead_code_evidence"]["symbols"]
        .as_array()
        .unwrap();
    assert!(
        symbols
            .iter()
            .any(|row| row["qualified_name"] == "service.backends.Right.aget_user")
    );
    assert!(
        !symbols
            .iter()
            .any(|row| row["qualified_name"] == "service.backends.Right.get_user")
    );

    fs::write(
        package.join("settings.py"),
        format!(
            "{settings}from . import backends\ndef replacement(self, user_id): return None\nsetattr(backends.Right.get_user, '__code__', replacement.__code__)\n"
        ),
    )
    .unwrap();
    let changed_body = scan_with_exit(root, None, None, 2);
    assert!(
        changed_body["analysis_errors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|issue| { issue["message"] == "configured Django callback body was replaced" })
    );
    let symbols = changed_body["dead_code_evidence"]["symbols"]
        .as_array()
        .unwrap();
    assert!(
        !symbols
            .iter()
            .any(|row| row["qualified_name"] == "service.backends.Right.get_user")
    );
    assert!(
        symbols
            .iter()
            .any(|row| row["qualified_name"] == "service.backends.Right.aget_user")
    );
    fs::write(package.join("settings.py"), &settings).unwrap();

    fs::write(
        package.join("config.py"),
        concat!(
            "BACKENDS = ['service.backends.Configured']\n",
            "LOGGING = {'version': 1, 'filters': {'live': {'()': 'service.backends.Filter'}}}\n",
            "TEMPLATES = []\n",
            "def reset():\n    LOGGING.clear()\n",
            "reset()\n",
        ),
    )
    .unwrap();
    let mutated = scan_with_exit(root, None, None, 2);
    assert_eq!(mutated["coverage"]["status"], "incomplete");
    assert!(
        !mutated["dead_code_evidence"]["symbols"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["qualified_name"] == "service.backends.Filter.filter")
    );
}

#[test]
fn phase03_preserves_an_importable_package_named_src() {
    let project = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let root = project.path();
    fs::create_dir(root.join("src")).unwrap();
    fs::write(root.join("src/__init__.py"), "").unwrap();
    fs::write(
        root.join("src/routes.py"),
        concat!(
            "from fastapi import APIRouter\n",
            "router = APIRouter()\n",
            "@router.get('/')\n",
            "def home(): return 1\n",
            "def dormant(): return 2\n",
        ),
    )
    .unwrap();
    fs::write(
        root.join("src/main.py"),
        concat!(
            "from fastapi import FastAPI\n",
            "from src import routes\n",
            "app = FastAPI()\n",
            "app.include_router(routes.router)\n",
        ),
    )
    .unwrap();
    let report = scan(root);
    assert_eq!(
        findings(&report, "unused_functions"),
        ["src.routes.dormant"]
    );
    let cached = scan_with_settings(root, None, Some(cache.path()));
    assert_eq!(report["coverage"], cached["coverage"]);
    assert_eq!(report["unused_functions"], cached["unused_functions"]);
    assert_eq!(report["dead_code_evidence"], cached["dead_code_evidence"]);
}

#[test]
fn django_setting_collections_follow_initializer_call_and_reassignment_order() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    let package = root.join("service");
    fs::create_dir(&package).unwrap();
    fs::write(package.join("__init__.py"), "").unwrap();
    fs::write(
        package.join("backends.py"),
        "class Backend:\n    def get_user(self, user_id): return user(user_id)\n\
         class Store:\n    def save(self, name, content): return name\n\
         class Filter:\n    def filter(self, record): return True\n\
         def user(user_id): return user_id\n",
    )
    .unwrap();
    let settings = package.join("settings.py");
    let cache = tempfile::tempdir().unwrap();
    let roots = |report: &Value, name: &str| {
        report["dead_code_evidence"]["symbols"]
            .as_array()
            .unwrap()
            .iter()
            .any(|symbol| {
                symbol["qualified_name"] == name
                    && symbol["evidence"].as_array().unwrap().iter().any(|event| {
                        matches!(
                            event["kind"].as_str().unwrap(),
                            "django_auth_backend"
                                | "django_storage_backend"
                                | "django_logging_factory"
                        ) && event["certainty"] == "possible"
                    })
            })
    };
    for (tail, symbol, active) in [
        (
            "AUTHENTICATION_BACKENDS = []\n\
             def setup():\n    AUTHENTICATION_BACKENDS.append('service.backends.Backend')\n\
             AUTHENTICATION_BACKENDS = []\nactivate = setup\nactivate()\n",
            "service.backends.Backend.get_user",
            true,
        ),
        (
            "AUTHENTICATION_BACKENDS = []\n\
             def setup():\n    AUTHENTICATION_BACKENDS.append('service.backends.Backend')\n\
             setup()\nAUTHENTICATION_BACKENDS = []\n",
            "service.backends.Backend.get_user",
            false,
        ),
        (
            "AUTHENTICATION_BACKENDS = []\n\
             def setup(AUTHENTICATION_BACKENDS):\n    AUTHENTICATION_BACKENDS.append('service.backends.Backend')\n\
             setup([])\n",
            "service.backends.Backend.get_user",
            false,
        ),
        (
            "AUTHENTICATION_BACKENDS = []\n\
             if enabled:\n    AUTHENTICATION_BACKENDS.insert(0, 'service.backends.Backend')\n",
            "service.backends.Backend.get_user",
            true,
        ),
        (
            "AUTHENTICATION_BACKENDS = ['service.backends.Backend']\n\
             AUTHENTICATION_BACKENDS.remove('service.backends.Backend')\n",
            "service.backends.Backend.get_user",
            false,
        ),
        (
            "AUTHENTICATION_BACKENDS = []\n\
             AUTHENTICATION_BACKENDS.clear()\n\
             AUTHENTICATION_BACKENDS = ('service.backends.Backend',)\n",
            "service.backends.Backend.get_user",
            true,
        ),
        (
            "STORAGES = {}\n\
             def modify():\n    STORAGES.update({'default': {'BACKEND': 'service.backends.Store'}})\n\
             STORAGES = {}\nmodify()\n",
            "service.backends.Store.save",
            true,
        ),
    ] {
        fs::write(
            &settings,
            format!("INSTALLED_APPS = []\nSECRET_KEY = 'fixture'\n{tail}"),
        )
        .unwrap();
        let report = scan(root);
        assert_eq!(roots(&report, symbol), active, "{tail}: {report}");
        let mut cached = scan_with_settings(root, None, Some(cache.path()));
        cached.as_object_mut().unwrap().remove("cache");
        assert_eq!(cached, report);
    }
    for (version, active) in [("1", true), ("2", false), ("None", false)] {
        fs::write(
            &settings,
            format!(
                "INSTALLED_APPS = []\nSECRET_KEY = 'fixture'\nLOGGING = {{}}\n\
                 LOGGING.update({{'version': {version}, 'filters': {{'active': {{'()': 'service.backends.Filter'}}}}}})\n",
            ),
        )
        .unwrap();
        let report = scan(root);
        assert_eq!(
            roots(&report, "service.backends.Filter.filter"),
            active,
            "{report}"
        );
    }
    for tail in [
        "AUTHENTICATION_BACKENDS = ('service.backends.Backend',)\nAUTHENTICATION_BACKENDS.clear()\n",
        "AUTHENTICATION_BACKENDS = ['service.backends.Backend']\nalias = AUTHENTICATION_BACKENDS\nalias.pop()\n",
    ] {
        fs::write(
            &settings,
            format!("INSTALLED_APPS = []\nSECRET_KEY = 'fixture'\n{tail}"),
        )
        .unwrap();
        let report = scan_with_exit(root, None, None, 2);
        assert_eq!(report["coverage"]["status"], "incomplete");
        assert!(!roots(&report, "service.backends.Backend.get_user"));
    }
}

#[test]
fn django_url_callback_dispatches_only_the_fresh_instances_selected_provider() {
    let project = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let root = project.path();
    for (file, source) in [
        (
            "manage.py",
            "import os\nfrom django.core.management import execute_from_command_line\nos.environ.setdefault('DJANGO_SETTINGS_MODULE', 'settings')\nexecute_from_command_line([])\n",
        ),
        (
            "settings.py",
            "ROOT_URLCONF = 'urls'\nINSTALLED_APPS = []\n",
        ),
        (
            "providers.py",
            "class Base: pass\nclass First(Base):\n    def run(self, value): return value\nclass Second(Base):\n    def run(self, value): return value\ndef choose(name):\n    if name == 'first':\n        return First()\n    return Second()\n",
        ),
        (
            "pipeline.py",
            "from providers import choose\nclass Pipeline:\n    def __init__(self, provider):\n        self.provider = provider\n    def dispatch(self):\n        return self.provider.run(1)\ndef select():\n    return Pipeline(choose('first')).dispatch()\n",
        ),
        (
            "views.py",
            "from pipeline import select\ndef view(request): return select()\n",
        ),
    ] {
        fs::write(root.join(file), source).unwrap();
    }
    let urls = root.join("urls.py");
    let registered =
        "from django.urls import path\nfrom views import view\nurlpatterns = [path('', view)]\n";
    for (source, live, unused) in [
        (registered, true, &["providers.Second.run"][..]),
        (
            "urlpatterns = []\n",
            false,
            &["providers.First.run", "providers.Second.run"][..],
        ),
    ] {
        fs::write(&urls, source).unwrap();
        let report = scan(root);
        let symbols = report["dead_code_evidence"]["symbols"].as_array().unwrap();
        assert_eq!(
            symbols.iter().any(|row| {
                row["qualified_name"] == "providers.First.run"
                    && row["evidence"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|edge| edge["kind"] == "direct_call" && edge["certainty"] == "certain")
            }),
            live,
            "{report}"
        );
        let unused_functions = unreachable_functions(&report);
        assert!(
            unused.iter().all(|name| unused_functions.contains(name)),
            "{report}"
        );
        let mut cached = scan_with_settings(root, None, Some(cache.path()));
        cached.as_object_mut().unwrap().remove("cache");
        assert_eq!(report, cached);
    }

    fs::write(&urls, registered).unwrap();
    fs::write(
        root.join("views.py"),
        "from pipeline import Pipeline, select\nfrom providers import choose\ndef view(request):\n    select()\n    other = Pipeline(choose('second'))\n    return other.dispatch()\n",
    ).unwrap();
    let report = scan_with_exit(root, None, None, 2);
    assert!(
        report["analysis_errors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|error| { error["kind"] == "unresolved_call" && error["file"] == "pipeline.py" }),
        "{report}"
    );

    fs::write(
        root.join("views.py"),
        "from pipeline import select\ndef view(request): return select()\n",
    )
    .unwrap();
    let providers = root.join("providers.py");
    let mut source = fs::read_to_string(&providers).unwrap();
    source.push_str("def replacement(self, value): return 0\nsetattr(First.run, '__code__', replacement.__code__)\n");
    fs::write(&providers, source).unwrap();
    let edited = scan_with_exit(root, None, Some(cache.path()), 2);
    assert!(
        edited["analysis_errors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|error| { error["kind"] == "unresolved_call" && error["file"] == "pipeline.py" }),
        "{edited}"
    );
    assert!(
        !edited["dead_code_evidence"]["symbols"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| {
                row["qualified_name"] == "providers.First.run"
                    && row["evidence"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|edge| edge["certainty"] == "certain")
            }),
        "{edited}"
    );
}

#[test]
fn django_url_import_stops_at_a_proven_unbound_view_initializer() {
    let project = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let root = project.path();
    for (file, source) in [
        (
            "manage.py",
            "import os\nfrom django.core.management import execute_from_command_line\nos.environ.setdefault('DJANGO_SETTINGS_MODULE', 'settings')\nexecute_from_command_line([])\n",
        ),
        (
            "settings.py",
            "ROOT_URLCONF = 'urls'\nINSTALLED_APPS = []\n",
        ),
        (
            "urls.py",
            "from django.urls import path\nfrom views import view\nurlpatterns = [path('', view)]\n",
        ),
        (
            "helpers.py",
            "class Provider:\n    def render(self): return 'ok'\nprovider = Provider()\ndef render(): return provider.render()\n",
        ),
    ] {
        fs::write(root.join(file), source).unwrap();
    }
    let views = root.join("views.py");
    let body = "def view(request): return render()\n";
    for source in [
        format!("{body}install(view)\n"),
        format!("@install\n{body}"),
    ] {
        fs::write(&views, format!("from helpers import render\n{source}")).unwrap();
        let report = scan(root);
        assert!(
            unreachable_functions(&report).contains(&"helpers.render"),
            "{report}"
        );
        for _ in 0..2 {
            let mut cached = scan_with_settings(root, None, Some(cache.path()));
            cached.as_object_mut().unwrap().remove("cache");
            assert_eq!(report, cached);
        }

        // Binding the same name to an actual identity keeps the selected view.
        fs::write(
            &views,
            format!("from helpers import render\ndef install(value): return value\n{source}"),
        )
        .unwrap();
        assert!(!unreachable_functions(&scan(root)).contains(&"helpers.render"));
    }

    fs::write(
        &views,
        format!("from external import install\nfrom helpers import render\n@install\n{body}"),
    )
    .unwrap();
    let report = scan_with_exit(root, None, None, 2);
    assert!(
        report["analysis_errors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["kind"] == "unresolved_decorator"),
        "{report}"
    );

    fs::write(
        root.join("settings.py"),
        "ROOT_URLCONF = 'urls'\nINSTALLED_APPS = []\nimport builtins\ndef identity(view): return view\nbuiltins.install = identity\n",
    )
    .unwrap();
    fs::write(
        &views,
        format!("from helpers import render\n@install\n{body}"),
    )
    .unwrap();
    let report = scan_with_exit(root, None, None, 2);
    assert!(
        report["analysis_errors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["file"] == "views.py" && row["kind"] == "unresolved_decorator"),
        "{report}"
    );
}

#[test]
fn django_url_callbacks_enter_async_views_before_validating_the_response() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    for (file, source) in [
        (
            "manage.py",
            "import os\nfrom django.core.management import execute_from_command_line\nos.environ.setdefault('DJANGO_SETTINGS_MODULE', 'settings')\nexecute_from_command_line([])\n",
        ),
        (
            "settings.py",
            "ROOT_URLCONF = 'urls'\nINSTALLED_APPS = []\n",
        ),
        (
            "urls.py",
            "from django.urls import path\nfrom views import stream, ordinary\nurlpatterns = [path('stream/', stream), path('async/', ordinary)]\n",
        ),
        (
            "views.py",
            "async def ordinary(request): return async_helper()\ndef stream(request): yield generator_helper()\ndef async_helper(): return [2026]\ndef generator_helper(): return 2\n",
        ),
    ] {
        fs::write(root.join(file), source).unwrap();
    }
    let report = scan(root);
    assert_eq!(
        findings(&report, "unused_functions"),
        ["views.generator_helper"],
        "{report}"
    );
    let symbols = report["dead_code_evidence"]["symbols"].as_array().unwrap();
    for (name, kind) in [
        ("views.ordinary", "django_registration"),
        // Django reaches this call before rejecting the view's list response.
        ("views.async_helper", "direct_call"),
        ("views.stream", "generator_creation"),
    ] {
        assert!(
            symbols.iter().any(|row| {
                row["qualified_name"] == name
                    && row["evidence"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|edge| edge["kind"] == kind)
            }),
            "{report}"
        );
    }
}

#[test]
fn source_provider_dispatch_uses_the_fresh_argument_instead_of_its_annotation() {
    let project = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let root = project.path();
    let source = r#"class Provider:
    def schedule_dates(self, season): raise NotImplementedError
    def scoreboard(self, day): raise NotImplementedError

class ConcreteProvider(Provider):
    def schedule_dates(self, season): return [f"{season}-01-01"]
    def scoreboard(self, day): return {"day": day, "games": []}

def fetch_day(provider: Provider, day: str):
    return provider.scoreboard(day)

RESULT = fetch_day(ConcreteProvider(), "2026-10-23")
"#;
    let providers = root.join("providers.py");
    fs::write(&providers, source).unwrap();

    let report = scan(root);
    assert_provider_call(&report, "providers.ConcreteProvider.scoreboard", "certain");
    let unused = unreachable_functions(&report);
    assert!(unused.contains(&"providers.ConcreteProvider.schedule_dates"));
    assert!(unused.contains(&"providers.Provider.scoreboard"));
    assert_provider_cache_equivalence(root, cache.path(), &report, 0);

    fs::write(
        &providers,
        format!("{source}UNKNOWN = fetch_day(object(), '2026-10-24')\n"),
    )
    .unwrap();
    let report = scan_with_exit(root, None, None, 2);
    assert_provider_call(&report, "providers.ConcreteProvider.scoreboard", "certain");
    assert_provider_dispatch_unresolved(&report, "providers.py", 10);
    assert_provider_cache_equivalence(root, cache.path(), &report, 2);

    fs::write(
        providers,
        format!(
            "{source}def replacement(self, day): return {{}}\n\
             ConcreteProvider.scoreboard.__code__ = replacement.__code__\n"
        ),
    )
    .unwrap();
    let report = scan_with_exit(root, None, None, 2);
    assert_provider_dispatch_unresolved(&report, "providers.py", 10);
    assert!(!provider_call(
        &report,
        "providers.ConcreteProvider.scoreboard",
        "certain"
    ));
}

#[test]
fn source_provider_dispatch_follows_a_literal_through_a_local_factory() {
    let project = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let root = project.path();
    fs::write(
        root.join("providers.py"),
        r#"class Provider:
    def scoreboard(self, day): raise NotImplementedError

class NcaaProvider(Provider):
    def schedule_dates(self, season): return [f"{season}-01-01"]
    def scoreboard(self, day): return {"day": day, "games": []}

class NjcaaProvider(Provider):
    def schedule_dates(self, season): return [f"{season}-02-01"]
    def scoreboard(self, day): return {"day": day, "games": []}

def get_provider(slug):
    if slug == "ncaa":
        return NcaaProvider()
    return NjcaaProvider()
"#,
    )
    .unwrap();
    let pipeline = root.join("pipeline.py");
    let wrapper = r#"from providers import get_provider

def import_month(slug, season):
    client = get_provider(slug)
    return client.schedule_dates(season)
"#;

    for (input, selected, dormant) in [
        ("'ncaa'", "NcaaProvider", "NjcaaProvider"),
        ("'njcaa'", "NjcaaProvider", "NcaaProvider"),
    ] {
        fs::write(
            &pipeline,
            format!("{wrapper}\nRESULT = import_month({input}, 2026)\n"),
        )
        .unwrap();
        let report = scan(root);
        assert_provider_call(
            &report,
            &format!("providers.{selected}.schedule_dates"),
            "certain",
        );
        let unused = unreachable_functions(&report);
        assert!(unused.contains(&format!("providers.{dormant}.schedule_dates").as_str()));
        assert!(unused.contains(&format!("providers.{selected}.scoreboard").as_str()));
        assert_provider_cache_equivalence(root, cache.path(), &report, 0);
    }

    fs::write(
        pipeline,
        format!("{wrapper}\nRESULT = import_month(object(), 2026)\n"),
    )
    .unwrap();
    let report = scan_with_exit(root, None, None, 2);
    assert_provider_dispatch_unresolved(&report, "pipeline.py", 5);
    for name in [
        "providers.NcaaProvider.schedule_dates",
        "providers.NjcaaProvider.schedule_dates",
    ] {
        assert!(!provider_call(&report, name, "certain"), "{report}");
        assert!(!provider_call(&report, name, "possible"), "{report}");
    }
    assert_provider_cache_equivalence(root, cache.path(), &report, 2);
}

fn assert_provider_dispatch_unresolved(report: &Value, path: &str, line: u64) {
    assert!(
        report["analysis_errors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|issue| {
                issue["kind"] == "unresolved_call" && issue["file"] == path && issue["line"] == line
            }),
        "{report}"
    );
}

fn assert_provider_call(report: &Value, symbol: &str, certainty: &str) {
    assert!(provider_call(report, symbol, certainty), "{report}");
}

fn provider_call(report: &Value, symbol: &str, certainty: &str) -> bool {
    report["dead_code_evidence"]["symbols"]
        .as_array()
        .unwrap()
        .iter()
        .any(|row| {
            row["qualified_name"] == symbol
                && row["evidence"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|edge| edge["kind"] == "direct_call" && edge["certainty"] == certainty)
        })
}

fn assert_provider_cache_equivalence(root: &Path, cache: &Path, report: &Value, code: i32) {
    for _ in 0..2 {
        let mut cached = scan_with_exit(root, None, Some(cache), code);
        cached.as_object_mut().unwrap().remove("cache");
        assert_eq!(&cached, report);
    }
}

#[test]
fn source_provider_dispatch_does_not_invoke_a_factory_class_before_definition() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    let header = r#"def select(kind):
    if kind == "live":
        return Live()
    return Fallback()

def run(kind, value):
    provider = select(kind)
    return provider.execute(value)
"#;
    let classes = r#"class Live:
    def execute(self, value): return {"value": value}

class Fallback:
    def execute(self, value): return {"fallback": value}
"#;
    let call = "RESULT = run('live', 1)\n";
    let source = root.join("main.py");
    fs::write(&source, format!("{header}\n{call}\n{classes}")).unwrap();
    let report = scan_with_exit(root, None, None, 2);
    assert_provider_dispatch_unresolved(&report, "main.py", 8);
    assert!(!provider_call(&report, "main.Live.execute", "certain"));

    fs::write(source, format!("{header}\n{classes}\n{call}")).unwrap();
    let report = scan(root);
    assert_provider_call(&report, "main.Live.execute", "certain");
    assert!(unreachable_functions(&report).contains(&"main.Fallback.execute"));
}

#[test]
fn source_provider_dispatch_keeps_opaque_class_mutations_unresolved() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    fs::write(
        root.join("main.py"),
        r#"from unittest.mock import patch

class Provider:
    def execute(self, value): return {"value": value}

def run(provider, value):
    return provider.execute(value)

def replacement(self, value):
    return {"replacement": value}

with patch.object(Provider, 'execute', replacement):
    RESULT = run(Provider(), 1)
"#,
    )
    .unwrap();

    let report = scan_with_exit(root, None, None, 2);
    assert_provider_dispatch_unresolved(&report, "main.py", 7);
    assert!(!provider_call(&report, "main.Provider.execute", "certain"));
    assert!(!provider_call(&report, "main.Provider.execute", "possible"));
}

#[test]
fn source_owned_installers_replace_the_selected_django_command_handle() {
    let workspace = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let root = workspace.path();
    fs::create_dir_all(root.join("shop/management/commands")).unwrap();
    for path in [
        "shop/__init__.py",
        "shop/management/__init__.py",
        "shop/management/commands/__init__.py",
    ] {
        fs::write(root.join(path), "").unwrap();
    }
    fs::write(
        root.join("settings.py"),
        "INSTALLED_APPS = ['shop']\nSECRET_KEY = 'fixture'\n",
    )
    .unwrap();
    fs::write(
        root.join("manage.py"),
        "from django.core.management import execute_from_command_line\nexecute_from_command_line(['manage.py', 'audit', '--settings=settings'])\n",
    )
    .unwrap();
    let command = root.join("shop/management/commands/audit.py");
    let source = "from django.core.management.base import BaseCommand\nfrom external_plugin import install\n\ndef prepare(): return 'ready'\n\nclass Command(BaseCommand):\n    def handle(self, *args, **options): return prepare()\n";
    let handle = "shop.management.commands.audit.Command.handle";
    for (statement, replacement, kind) in [
        (
            "install(Command)\n",
            "setattr(target, 'handle', replacement)",
            "django_command",
        ),
        (
            "selected = Command.handle\ninstall(selected)\n",
            "setattr(target, '__code__', replacement.__code__)",
            "code_reference",
        ),
    ] {
        fs::write(
            root.join("external_plugin.py"),
            format!(
                "def replacement(self, *args, **options): return None\ndef install(target): {replacement}\n"
            ),
        )
        .unwrap();
        fs::write(&command, source).unwrap();
        assert_eq!(
            evidence_certainties(&scan(root), handle, "django_command"),
            ["certain"]
        );
        fs::write(&command, format!("{source}{statement}")).unwrap();
        let report = scan(root);
        assert!(evidence_certainties(&report, handle, "django_command").is_empty());
        assert!(
            !evidence_certainties(&report, "external_plugin.replacement", kind).is_empty(),
            "{report}",
        );
        if kind == "code_reference" {
            assert!(
                evidence_certainties(&report, "external_plugin.replacement", "direct_call")
                    .is_empty()
            );
        }
        for _ in 0..2 {
            let mut warm = scan_with_settings(root, None, Some(cache.path()));
            warm.as_object_mut().unwrap().remove("cache");
            assert_eq!(report, warm);
        }
    }
    fs::remove_file(root.join("external_plugin.py")).unwrap();
    let frozen = scan_with_exit(root, None, None, 2);
    assert!(
        frozen["coverage"]["issues"]
            .as_array()
            .unwrap()
            .iter()
            .any(|issue| issue["kind"] == "unresolved_call")
    );
    assert!(evidence_certainties(&frozen, handle, "django_command").is_empty());
}

#[test]
fn django_management_check_keeps_unproved_dispatch_coverage_incomplete() {
    let project = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let root = project.path();
    fs::create_dir(root.join("shop")).unwrap();
    for (name, source) in [
        ("shop/__init__.py", ""),
        (
            "shop/settings.py",
            "INSTALLED_APPS = ['shop.apps.ShopConfig']\nSECRET_KEY = 'fixture'\n",
        ),
        (
            "shop/apps.py",
            "from django.apps import AppConfig\nclass ShopConfig(AppConfig):\n    name = 'shop'\n    def ready(self):\n        from . import checks\n",
        ),
        (
            "shop/checks.py",
            "from django.core.checks import Tags, register\n@register(Tags.security)\ndef check_store(app_configs, **kwargs): return []\n",
        ),
    ] {
        fs::write(root.join(name), source).unwrap();
    }
    let launcher = root.join("manage.py");
    let prefix = "import os\nfrom django.core.management import execute_from_command_line\nos.environ['DJANGO_SETTINGS_MODULE'] = 'shop.settings'\n";
    for dispatch in [
        "execute_from_command_line(['manage.py', 'check', '--settings=shop.settings'])\n",
        "import sys\nexecute_from_command_line(sys.argv)\n",
    ] {
        fs::write(&launcher, format!("{prefix}{dispatch}")).unwrap();
        let report = scan_with_exit(root, None, None, 2);
        assert!(report["coverage"]["issues"].as_array().unwrap().iter().any(|issue| {
            issue["path"] == "manage.py"
                && issue["message"] == "Django management dispatch of project system checks is outside the bounded model"
        }), "{report}");
        assert!(!unreachable_functions(&report).contains(&"shop.checks.check_store"));
        for _ in 0..2 {
            let mut cached = scan_with_exit(root, None, Some(cache.path()), 2);
            cached.as_object_mut().unwrap().remove("cache");
            assert_eq!(cached, report);
        }
    }

    fs::write(
        &launcher,
        format!("{prefix}execute_from_command_line(['manage.py', 'check', '--help'])\n"),
    )
    .unwrap();
    let report = scan(root);
    assert!(unreachable_functions(&report).contains(&"shop.checks.check_store"));

    fs::write(
        root.join("shop/settings.py"),
        "INSTALLED_APPS = []\nSECRET_KEY = 'fixture'\n",
    )
    .unwrap();
    fs::write(
        &launcher,
        format!("{prefix}execute_from_command_line(['manage.py', 'check'])\n"),
    )
    .unwrap();
    let report = scan(root);
    assert!(unreachable_functions(&report).contains(&"shop.checks.check_store"));
}

#[test]
fn django_system_checks_require_ordered_selected_registration_and_matching_tags() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    let cache = tempfile::tempdir().unwrap();
    fs::create_dir(root.join("shop")).unwrap();
    for (name, source) in [
        ("shop/__init__.py", ""),
        (
            "manage.py",
            "import os\nimport django\nos.environ['DJANGO_SETTINGS_MODULE'] = 'settings'\ndjango.setup()\n",
        ),
        ("settings.py", "INSTALLED_APPS = ['shop.apps.Selected']\n"),
        (
            "shop/checks.py",
            "from django.core.checks import Tags, register\n@register(Tags.security)\ndef checked(app_configs, **kwargs): return witness()\n@register(Tags.database)\ndef other(app_configs, **kwargs): return []\ndef witness(): return []\n",
        ),
        (
            "shop/fields.py",
            "def callback(app_configs, **kwargs): return []\ndef dead(arg): return None\ndead(1)\n",
        ),
    ] {
        fs::write(root.join(name), source).unwrap();
    }
    let configure = |operations| {
        fs::write(
            root.join("shop/apps.py"),
            format!(
                "from django.apps import AppConfig\nclass Selected(AppConfig):\n    name = 'shop'\n    def ready(self):\n        from django.core.checks import Tags, register, run_checks\n{operations}"
            ),
        )
        .unwrap();
    };
    configure(
        "        from . import checks, fields\n        register(fields.callback, Tags.security)\n        run_checks(tags=[Tags.security])\n",
    );
    let report = scan(root);
    for expected in ["shop.checks.checked", "shop.fields.callback"] {
        assert_eq!(
            evidence_certainties(&report, expected, "django_system_check"),
            ["possible"]
        );
    }
    assert!(evidence_certainties(&report, "shop.checks.other", "django_system_check").is_empty());
    assert!(findings(&report, "unused_functions").contains(&"shop.checks.other"));
    let parameters = findings(&report, "unused_parameters");
    assert!(parameters.contains(&"shop.fields.dead.arg"));
    assert!(!parameters.contains(&"shop.checks.checked.app_configs"));
    assert!(!parameters.contains(&"shop.fields.callback.kwargs"));
    for _ in 0..2 {
        let mut cached = scan_with_settings(root, None, Some(cache.path()));
        cached.as_object_mut().unwrap().remove("cache");
        assert_eq!(cached, report);
    }

    configure(
        "        from . import checks, fields\n        register(fields.callback, Tags.security)\n        register(fields.callback, Tags.database)\n        run_checks(tags=[Tags.security])\n",
    );
    let retagged = scan(root);
    assert!(
        evidence_certainties(&retagged, "shop.fields.callback", "django_system_check").is_empty()
    );

    configure(
        "        run_checks(tags=[Tags.security])\n        from . import checks, fields\n        register(fields.callback, Tags.security)\n",
    );
    let unordered = scan_with_exit(root, None, Some(cache.path()), 2);
    for expected in ["shop.checks.checked", "shop.fields.callback"] {
        assert!(evidence_certainties(&unordered, expected, "django_system_check").is_empty());
    }
}

#[test]
fn pontoon_settings_bind_genuine_storage_and_pagination_and_exception_defaults() {
    let project = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let root = project.path();
    let settings = "SECRET_KEY = 'fixture'\nINSTALLED_APPS = []\nSTATIC_ROOT = 'static'\nSTATIC_URL = '/static/'\n";
    let providers = "STORAGES = {'staticfiles': {'BACKEND': 'components.SelectedStorage'}}\nREST_FRAMEWORK = {'DEFAULT_PAGINATION_CLASS': 'components.SelectedPagination'}\n";
    let launcher = "import os\nfrom django.core.wsgi import get_wsgi_application\nfrom components import ActiveError, ExplicitError\nos.environ['DJANGO_SETTINGS_MODULE'] = 'settings'\napplication = get_wsgi_application()\nActiveError()\nExplicitError(detail='overridden')\n";
    fs::write(root.join("settings.py"), format!("{settings}{providers}")).unwrap();
    fs::write(root.join("wsgi.py"), launcher).unwrap();
    fs::write(
        root.join("components.py"),
        r#"from django.contrib.staticfiles.storage import ManifestStaticFilesStorage
from rest_framework.exceptions import APIException
from rest_framework.pagination import PageNumberPagination

class SelectedStorage(ManifestStaticFilesStorage):
    pass

class DormantStorage(ManifestStaticFilesStorage):
    pass

class SelectedPagination(PageNumberPagination):
    page_size_query_param = 'limit'
    max_page_size = 1000
    unused_option = 15

class DormantPagination(PageNumberPagination):
    page_size_query_param = 'unselected'
    max_page_size = 22

class ActiveError(APIException):
    default_detail = 'used'
    unrelated = 'unread'

class ExplicitError(APIException):
    default_detail = 'bypassed'
"#,
    )
    .unwrap();
    let report = scan(root);
    for (target, evidence) in [
        ("components.SelectedStorage", "django_setting_reference"),
        ("components.SelectedPagination", "drf_pagination_setting"),
        (
            "components.SelectedPagination.page_size_query_param",
            "drf_pagination_option",
        ),
        (
            "components.SelectedPagination.max_page_size",
            "drf_pagination_option",
        ),
        (
            "components.ActiveError.default_detail",
            "drf_exception_default",
        ),
    ] {
        assert_eq!(
            evidence_certainties(&report, target, evidence),
            ["possible"]
        );
    }
    let classes = findings(&report, "unused_classes");
    assert!(classes.contains(&"components.DormantStorage"));
    assert!(classes.contains(&"components.DormantPagination"));
    let variables = findings(&report, "unused_variables");
    for target in [
        "components.ExplicitError.default_detail",
        "components.ActiveError.unrelated",
        "components.SelectedPagination.unused_option",
    ] {
        assert!(variables.contains(&target), "{variables:?}");
    }
    for _ in 0..2 {
        let mut cached = scan_with_settings(root, None, Some(cache.path()));
        cached.as_object_mut().unwrap().remove("cache");
        assert_eq!(cached, report);
    }

    fs::write(
        root.join("settings.py"),
        format!("{settings}{providers}REST_FRAMEWORK = {{}}\nSTORAGES = {{}}\n"),
    )
    .unwrap();
    let replaced = scan_with_settings(root, None, Some(cache.path()));
    assert!(
        evidence_certainties(
            &replaced,
            "components.SelectedStorage",
            "django_setting_reference"
        )
        .is_empty()
    );
    assert!(
        evidence_certainties(
            &replaced,
            "components.SelectedPagination",
            "drf_pagination_setting"
        )
        .is_empty()
    );
}

#[test]
fn django_settings_retain_only_the_selected_uppercase_aliases() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    let cache = tempfile::tempdir().unwrap();
    let settings = root.join("project/settings");
    fs::create_dir_all(&settings).unwrap();
    for (name, text) in [
        ("project/__init__.py", ""),
        ("project/urls.py", "urlpatterns = []\n"),
        ("project/unused.py", "UNSELECTED = True\n"),
        (
            "project/settings/__init__.py",
            "from .base import *\nif DEV:\n    from .dev import *\nfrom .base import _named_value as NAMED_SETTING\nREPLACED = 'chosen'\n",
        ),
        (
            "project/settings/dev.py",
            "INSTALLED_APPS = ['debug_toolbar']\nDEBUG_TOOLBAR_CONFIG = {'SHOW_TOOLBAR_CALLBACK': lambda request: True}\n",
        ),
        (
            "wsgi.py",
            "import os\nfrom django.core.wsgi import get_wsgi_application\nos.environ['DJANGO_SETTINGS_MODULE'] = 'project.settings'\napplication = get_wsgi_application()\n",
        ),
    ] {
        fs::write(root.join(name), text).unwrap();
    }
    for (dev, literal) in [(false, "False"), (true, "True")] {
        fs::write(
            settings.join("base.py"),
            format!(
                "DEV = {literal}\nINSTALLED_APPS = []\nSECRET_KEY = 'fixture'\nROOT_URLCONF = 'project.urls'\nSTATIC_ROOT = 'staticfiles'\nSTATIC_URL = '/static/'\nEMAIL_CONSENT_TITLE = 'Stay informed'\nREPLACED = 'shadowed'\n_named_value = 1\n_ignored = 2\nif DEV:\n    SESSION_COOKIE_SECURE = False\n"
            ),
        )
        .unwrap();
        let report = scan(root);
        for target in [
            "project.settings.base.EMAIL_CONSENT_TITLE",
            "project.settings.base._named_value",
            "project.settings.REPLACED",
        ] {
            assert_eq!(
                evidence_certainties(&report, target, "django_setting"),
                ["certain"]
            );
        }
        let variables = findings(&report, "unused_variables");
        for target in [
            "project.settings.base.REPLACED",
            "project.settings.base._ignored",
            "project.unused.UNSELECTED",
        ] {
            assert!(variables.contains(&target), "{variables:?}");
        }
        for (target, selected) in [
            ("project.settings.base.SESSION_COOKIE_SECURE", dev),
            ("project.settings.dev.DEBUG_TOOLBAR_CONFIG", dev),
        ] {
            let expected: &[&str] = if selected { &["certain"] } else { &[] };
            assert_eq!(
                evidence_certainties(&report, target, "django_setting"),
                expected
            );
            assert_eq!(variables.contains(&target), !selected, "{variables:?}");
        }
        for _ in 0..2 {
            let mut cached = scan_with_settings(root, None, Some(cache.path()));
            cached.as_object_mut().unwrap().remove("cache");
            assert_eq!(cached, report);
        }
    }
}

#[test]
fn django_settings_keep_launcher_free_backend_negatives_complete() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    fs::create_dir(root.join("shop")).unwrap();
    for (name, text) in [
        ("shop/__init__.py", ""),
        (
            "shop/settings.py",
            concat!(
                "INSTALLED_APPS = ['shop']\nSECRET_KEY = 'fixture'\nROOT_URLCONF = 'shop.urls'\n",
                "AUTHENTICATION_BACKENDS = ['shop.backends.Configured']\n",
                "AUTHENTICATION_BACKENDS.unknown_mutation()\n",
            ),
        ),
        (
            "shop/urls.py",
            "from django.urls import path\nurlpatterns = []\n",
        ),
        (
            "shop/backends.py",
            "class Configured:\n    def get_user(self, user_id): return None\n",
        ),
    ] {
        fs::write(root.join(name), text).unwrap();
    }
    let report = scan(root);
    assert!(
        evidence_certainties(
            &report,
            "shop.backends.Configured.get_user",
            "django_auth_backend",
        )
        .is_empty()
    );
}

#[test]
fn django_settings_cannot_read_aliases_from_a_failed_initializer() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    for (name, text) in [
        ("providers.py", "FEATURE = True\nraise ImportError\n"),
        (
            "settings.py",
            "from providers import FEATURE\nSECRET_KEY = 'fixture'\nINSTALLED_APPS = []\n",
        ),
        (
            "wsgi.py",
            "import os\nfrom django.core.wsgi import get_wsgi_application\nos.environ['DJANGO_SETTINGS_MODULE'] = 'settings'\napplication = get_wsgi_application()\n",
        ),
    ] {
        fs::write(root.join(name), text).unwrap();
    }
    let report = scan_with_exit(root, None, None, 2);
    assert!(
        report["coverage"]["issues"]
            .as_array()
            .unwrap()
            .iter()
            .any(|issue| issue["message"]
                == "Django settings initialization terminates before exposing its values")
    );
    for target in ["settings.FEATURE", "providers.FEATURE"] {
        assert!(evidence_certainties(&report, target, "django_setting").is_empty());
    }
    assert!(evidence_certainties(&report, "wsgi.application", "django_application").is_empty());
}
