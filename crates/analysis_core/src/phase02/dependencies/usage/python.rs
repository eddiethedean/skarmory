use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use skarmory_python::phase02::{
    DefinitionKind, Expression, ExpressionKind as Expr, Statement, StatementKind as Stmt,
};

use super::{Usage, file_name, module_reference};
use crate::phase02::AnalyzedFile;
use crate::phase02::dependencies::Reason;

pub(super) fn collect(
    usage: &mut Usage,
    root: &Path,
    files: &[&AnalyzedFile],
    locals: &BTreeSet<&str>,
) {
    let fixtures: Vec<_> = files
        .iter()
        .filter(|file| Path::new(&file.path).ends_with("conftest.py"))
        .map(|file| {
            (
                Path::new(&file.path).parent().unwrap_or(Path::new("")),
                local_fixtures(&file.facts.body),
            )
        })
        .collect();
    for file in files {
        let path = root.join(&file.path);
        let local = Path::new(&file.path);
        let mut supplied = local_fixtures(&file.facts.body);
        for (parent, names) in &fixtures {
            if local.starts_with(parent) {
                supplied.extend(names);
            }
        }
        Python {
            usage,
            path: &path,
            supplied,
            locals,
        }
        .suite(
            &file.facts.body,
            &BTreeMap::from([("__import__", Some("__import__".into()))]),
        );
    }
}

struct Python<'a> {
    usage: &'a mut Usage,
    path: &'a Path,
    supplied: BTreeSet<&'a str>,
    locals: &'a BTreeSet<&'a str>,
}

type Bindings<'a> = BTreeMap<&'a str, Option<String>>;

impl<'a> Python<'a> {
    fn suite(&mut self, body: &'a [Statement], inherited: &Bindings<'a>) -> Bindings<'a> {
        let mut bindings = inherited.clone();
        for statement in body {
            match &statement.kind {
                Stmt::Import(import) => {
                    let qualified = (import.level == 0 && !is_local(self.locals, &import.module))
                        .then(|| {
                            import.member.as_ref().map_or_else(
                                || {
                                    if import.exposes_top_level {
                                        import.local.text.clone()
                                    } else {
                                        import.module.clone()
                                    }
                                },
                                |member| format!("{}.{}", import.module, member),
                            )
                        });
                    if import.level == 0 {
                        let reference = match import.member.as_deref() {
                            Some(member) if member != "*" => format!("{}.{member}", import.module),
                            _ => import.module.clone(),
                        };
                        self.module(statement.at.line, &reference, "python_import");
                    }
                    bindings.insert(&import.local.text, qualified);
                }
                Stmt::Assign {
                    names,
                    value,
                    annotation,
                } => {
                    if let Some(annotation) = annotation {
                        self.expression(annotation, &bindings);
                    }
                    if let Some(value) = value {
                        self.expression(value, &bindings);
                        if names
                            .iter()
                            .any(|name| python_configuration(self.path, &name.text))
                        {
                            self.configuration(value, &bindings);
                        }
                    }
                    let origin = value.as_ref().and_then(|value| qualified(value, &bindings));
                    for name in names {
                        bindings.insert(&name.text, origin.clone());
                    }
                }
                Stmt::Define(definition) => {
                    for expression in definition
                        .decorators
                        .iter()
                        .chain(&definition.defaults)
                        .chain(&definition.annotations)
                        .chain(&definition.bases)
                    {
                        self.expression(expression, &bindings);
                    }
                    if is_test_file(self.path)
                        && definition.name.text.starts_with("test_")
                        && definition.kind != DefinitionKind::Class
                    {
                        for parameter in &definition.parameters {
                            self.fixture(&parameter.text, parameter.at.line);
                        }
                    }
                    bindings.insert(&definition.name.text, None);
                }
                Stmt::If {
                    test,
                    body,
                    otherwise,
                } => {
                    self.expression(test, &bindings);
                    let then_bindings = self.suite(body, &bindings);
                    let else_bindings = self.suite(otherwise, &bindings);
                    bindings = merge_bindings(then_bindings, else_bindings);
                }
                Stmt::With { items, body } => {
                    let mut inside = bindings.clone();
                    for (value, name) in items {
                        self.expression(value, &inside);
                        if let Some(name) = name {
                            inside.insert(&name.text, None);
                            bindings.insert(&name.text, None);
                        }
                    }
                    let after = self.suite(body, &inside);
                    bindings = merge_bindings(after, bindings);
                }
                Stmt::Return(value) | Stmt::Raise(value) => {
                    if let Some(value) = value {
                        self.expression(value, &bindings);
                    }
                }
                Stmt::Assert(value) | Stmt::Evaluate(value) => self.expression(value, &bindings),
            }
        }
        for statement in body {
            if let Stmt::Define(definition) = &statement.kind {
                let mut inner = bindings.clone();
                for parameter in &definition.parameters {
                    inner.insert(&parameter.text, None);
                }
                shadow(&definition.body, &mut inner);
                self.suite(&definition.body, &inner);
            }
        }
        bindings
    }

    fn fixture(&mut self, name: &str, line: usize) {
        if self.supplied.contains(name) {
            return;
        }
        let distribution = match name {
            "cli_runner" | "isolated_cli_runner" => "pytest-click",
            "subtests" => "pytest-subtests",
            _ => return,
        };
        self.usage
            .plugin(self.path, line, distribution, "pytest_fixture");
    }

    fn configuration(&mut self, value: &Expression, bindings: &Bindings<'_>) {
        match &value.kind {
            Expr::Text(text) => self.module(value.at.line, text, "python_configuration"),
            Expr::Sequence(values) => {
                for value in values {
                    self.configuration(value, bindings);
                }
            }
            Expr::Dictionary(values) => {
                for [_, value] in values.as_chunks::<2>().0 {
                    self.configuration(value, bindings);
                }
            }
            Expr::Bool(_) | Expr::Scalar => {}
            _ if qualified(value, bindings).is_some_and(|name| name.starts_with("os.environ"))
                && !self.locals.contains("os") => {}
            _ => self.usage.reasons.push(Reason::new(
                "unresolved_dependency_configuration",
                self.path,
                Some(value.at.line),
                "module references in configuration are dynamic",
            )),
        }
    }

    fn module(&mut self, line: usize, name: &str, kind: &'static str) {
        if !module_reference(name).is_some_and(|name| is_local(self.locals, name)) {
            self.usage.module(self.path, line, name, kind);
        }
    }

    fn expression(&mut self, expression: &Expression, bindings: &Bindings<'_>) {
        match &expression.kind {
            Expr::Call {
                function,
                arguments,
                keywords,
            } => {
                let origin = qualified(function, bindings);
                let origin = origin.as_deref().unwrap_or_default();
                if is_import_call(origin) {
                    let target = arguments.first().or_else(|| {
                        keywords
                            .iter()
                            .find(|(name, _)| {
                                matches!(
                                    name.as_str(),
                                    "name" | "module_name" | "dotted_path" | "import_name"
                                )
                            })
                            .map(|(_, value)| value)
                    });
                    match target.map(|value| &value.kind) {
                        Some(Expr::Text(name)) => {
                            self.module(expression.at.line, name, "literal_dynamic_import")
                        }
                        _ => self.usage.reasons.push(Reason::new(
                            "unresolved_dynamic_import",
                            self.path,
                            Some(expression.at.line),
                            "a dynamic import target could not be resolved",
                        )),
                    }
                }
                if origin == "pkg_resources.load_entry_point" {
                    if let Some(Expr::Text(spec)) = arguments.first().map(|value| &value.kind)
                        && let Ok(requirement) = spec.parse::<pep508_rs::Requirement>()
                    {
                        self.usage.plugin(
                            self.path,
                            expression.at.line,
                            requirement.name.as_ref(),
                            "literal_entry_point",
                        );
                    } else {
                        self.usage.reasons.push(Reason::new(
                            "unresolved_dynamic_import",
                            self.path,
                            Some(expression.at.line),
                            "a dynamic package entry-point target could not be resolved",
                        ));
                    }
                }
                if origin == "importlib.util.find_spec"
                    && let Some(Expr::Text(name)) = arguments.first().map(|value| &value.kind)
                {
                    self.module(expression.at.line, name, "module_probe");
                }
                if origin == "pytest.mark.usefixtures" {
                    for argument in arguments {
                        if let Expr::Text(name) = &argument.kind
                            && is_test_file(self.path)
                        {
                            self.fixture(name, argument.at.line);
                        }
                    }
                }
                self.expression(function, bindings);
                for argument in arguments
                    .iter()
                    .chain(keywords.iter().map(|(_, expression)| expression))
                {
                    self.expression(argument, bindings);
                }
            }
            Expr::Member(value, _) => {
                let origin = qualified(expression, bindings);
                let plugin = match origin.as_deref() {
                    Some("pytest.mark.timeout") => Some("pytest-timeout"),
                    Some("pytest.mark.flaky") => Some("pytest-rerunfailures"),
                    _ => None,
                };
                if let Some(plugin) = plugin {
                    self.usage
                        .plugin(self.path, expression.at.line, plugin, "pytest_marker");
                }
                self.expression(value, bindings);
            }
            Expr::Not(value) | Expr::Await(value) => self.expression(value, bindings),
            Expr::Sequence(values)
            | Expr::Dictionary(values)
            | Expr::Boolean { values, .. }
            | Expr::Addition(values)
            | Expr::Operands(values) => {
                for item in values {
                    self.expression(item, bindings);
                }
            }
            Expr::Subscript { value, index }
            | Expr::Membership {
                element: value,
                container: index,
                ..
            }
            | Expr::Compare {
                left: value,
                right: index,
                ..
            }
            | Expr::OrderingComparison {
                left: value,
                right: index,
            } => {
                self.expression(value, bindings);
                self.expression(index, bindings);
            }
            Expr::Conditional {
                test,
                body,
                otherwise,
            } => {
                for value in [test, body, otherwise] {
                    self.expression(value, bindings);
                }
            }
            _ => {}
        }
    }
}

fn qualified(expression: &Expression, bindings: &Bindings<'_>) -> Option<String> {
    match &expression.kind {
        Expr::Name(name) => bindings
            .get(name.as_str())
            .cloned()
            .unwrap_or_else(|| (name == "__import__").then(|| name.clone())),
        Expr::Member(value, name) => {
            qualified(value, bindings).map(|base| format!("{base}.{name}"))
        }
        _ => None,
    }
}

fn merge_bindings<'a>(mut left: Bindings<'a>, mut right: Bindings<'a>) -> Bindings<'a> {
    for (name, value) in &mut left {
        let other = right.remove(name).unwrap_or_default();
        if *value != other && !value.as_deref().is_some_and(runtime_loader) {
            *value = other.filter(|origin| runtime_loader(origin));
        }
    }
    left.extend(
        right
            .into_iter()
            .map(|(name, value)| (name, value.filter(|origin| runtime_loader(origin)))),
    );
    left
}

fn runtime_loader(origin: &str) -> bool {
    matches!(
        origin,
        "__import__"
            | "builtins"
            | "importlib"
            | "pkg_resources"
            | "django"
            | "django.utils"
            | "django.utils.module_loading"
            | "werkzeug"
            | "werkzeug.utils"
    ) || is_import_call(origin)
        || origin == "pkg_resources.load_entry_point"
}

fn is_import_call(origin: &str) -> bool {
    matches!(
        origin,
        "__import__"
            | "builtins.__import__"
            | "importlib.import_module"
            | "django.utils.module_loading.import_string"
            | "werkzeug.utils.import_string"
    )
}

pub(super) fn is_local(locals: &BTreeSet<&str>, module: &str) -> bool {
    locals.contains(module)
        || module
            .match_indices('.')
            .any(|(split, _)| locals.contains(&module[..split]))
}

fn shadow<'a>(body: &'a [Statement], bindings: &mut Bindings<'a>) {
    for statement in body {
        match &statement.kind {
            Stmt::Assign { names, .. } => {
                for name in names {
                    bindings.insert(&name.text, None);
                }
            }
            Stmt::Define(definition) => {
                bindings.insert(&definition.name.text, None);
            }
            Stmt::Import(import) => {
                bindings.insert(&import.local.text, None);
            }
            Stmt::If {
                body, otherwise, ..
            } => {
                shadow(body, bindings);
                shadow(otherwise, bindings);
            }
            Stmt::With { items, body } => {
                for (_, name) in items {
                    if let Some(name) = name {
                        bindings.insert(&name.text, None);
                    }
                }
                shadow(body, bindings);
            }
            _ => {}
        }
    }
}

fn local_fixtures(body: &[Statement]) -> BTreeSet<&str> {
    let mut aliases = Bindings::new();
    let mut names = BTreeSet::new();
    for statement in body {
        match &statement.kind {
            Stmt::Import(import) if import.level == 0 && import.module == "pytest" => {
                aliases.insert(
                    &import.local.text,
                    Some(
                        import
                            .member
                            .as_ref()
                            .map_or_else(|| "pytest".into(), |name| format!("pytest.{name}")),
                    ),
                );
            }
            Stmt::Define(definition) => {
                for decorator in &definition.decorators {
                    let (function, keywords) = if let Expr::Call {
                        function, keywords, ..
                    } = &decorator.kind
                    {
                        (function.as_ref(), keywords.as_slice())
                    } else {
                        (decorator, &[][..])
                    };
                    if qualified(function, &aliases).as_deref() == Some("pytest.fixture") {
                        let name = keywords
                            .iter()
                            .find_map(|(key, value)| match &value.kind {
                                Expr::Text(text) if key == "name" => Some(text.as_str()),
                                _ => None,
                            })
                            .unwrap_or(&definition.name.text);
                        names.insert(name);
                    }
                }
            }
            _ => {}
        }
    }
    names
}

fn python_configuration(path: &Path, name: &str) -> bool {
    matches!(
        name,
        "pytest_plugins"
            | "INSTALLED_APPS"
            | "MIDDLEWARE"
            | "MIDDLEWARE_CLASSES"
            | "CACHES"
            | "STORAGES"
            | "TEMPLATES"
            | "REST_FRAMEWORK"
            | "AUTHENTICATION_BACKENDS"
            | "DATABASES"
            | "ROOT_URLCONF"
            | "WSGI_APPLICATION"
            | "ASGI_APPLICATION"
    ) || name == "extensions" && file_name(path) == "conf.py"
}

fn is_test_file(path: &Path) -> bool {
    let name = file_name(path);
    name.starts_with("test_") || name.ends_with("_test.py")
}
