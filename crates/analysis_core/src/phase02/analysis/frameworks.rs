//! Project-wide framework and test discovery for the Phase 0.3 profile.

use super::*;

mod django_settings;

pub(super) struct DjangoStartup {
    pub(super) at: Location,
    pub(super) settings: Option<String>,
    pub(super) creates_application: bool,
    pub(super) commands: DjangoCommands,
}

pub(super) enum DjangoCommands {
    None,
    Available,
    Selected(String),
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum DjangoReceiver {
    Certain,
    Possible,
    Empty,
    Unresolved,
    Invalid,
}

pub(super) struct DjangoSignalRegistration {
    at: Location,
    callbacks: Vec<DjangoSignalCallback>,
    issue: Option<&'static str>,
}

impl DjangoSignalRegistration {
    fn precedes(&self, first_failure: Option<[usize; 2]>) -> bool {
        first_failure.is_none_or(|at| position(self.at) < at)
    }
}

struct DjangoSignalCallback {
    edge: Edge,
    receiver: Option<SymbolId>,
}

#[derive(Default)]
pub(super) struct DjangoConverter {
    inputs: BTreeMap<SymbolId, Vec<(ScopeId, Option<SymbolId>)>>,
    pub(super) string_calls: Vec<ParameterCall>,
    wrappers: Vec<DjangoParameterRegistration>,
    noop_wrappers: Vec<(ScopeId, Location)>,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum ConverterDescriptor {
    Instance,
    Static,
    Class,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum ConverterBase {
    Source(SymbolId),
    String,
    Object,
}

enum ConverterMember {
    Source(SymbolId),
    Builtin,
}

#[derive(Clone, Copy)]
pub(super) struct DjangoParameterRegistration {
    site: ScopeId,
    parameter: SymbolId,
    owner: SymbolId,
    at: Location,
    conditional: bool,
}

pub(super) fn is_django_signal_edge(kind: &str) -> bool {
    matches!(kind, "django_signal" | "django_signal_unknown_loading")
}

impl Graph<'_> {
    pub(super) fn register_attrs_fields(&mut self, class: SymbolId) -> bool {
        let Binding::Definition(definition, class_scope) = self.symbols[class].binding else {
            return false;
        };
        if definition.kind != DefinitionKind::Class
            || definition.decorators.len() != 1
            || !definition.bases.is_empty()
        {
            return false;
        }
        let mut fields = BTreeSet::new();
        for statement in body_without_docstring(definition) {
            let StatementKind::Assign {
                names,
                value,
                annotation: Some(annotation),
            } = &statement.kind
            else {
                return false;
            };
            let [name] = names.as_slice() else {
                return false;
            };
            if !matches!(self.probe_identity(class_scope, annotation, statement.at),
                Value::Provider(provider) if matches!(provider.as_str(), "builtins.str" | "builtins.int" | "builtins.bool"))
                || value.as_ref().is_some_and(|value| {
                    !self
                        .probe_identity(class_scope, value, statement.at)
                        .is_builtin_scalar()
                })
            {
                return false;
            }
            let field = self.by_location[&(class_scope, name.at.line, name.at.column)];
            if self.local(class_scope, &name.text, END) != Some(field) {
                return false;
            }
            fields.insert(field);
        }
        for field in fields {
            self.edge(class_scope, field, "attrs_field", "certain");
        }
        true
    }

    fn retain_django_configured_class_declarations(&mut self) {
        for file in 0..self.files.len() {
            let Some(package) = self.files[file].module.strip_suffix(".settings") else {
                continue;
            };
            if !self
                .modules
                .get(package)
                .is_some_and(|&package| self.files[package].is_package)
                || !self
                    .local(file, "INSTALLED_APPS", END)
                    .is_some_and(|binding| {
                        matches!(self.binding(binding, END, 0), Value::Sequence(_))
                    })
                || !self
                    .local(file, "SECRET_KEY", END)
                    .is_some_and(|binding| matches!(self.binding(binding, END, 0), Value::Text(_)))
            {
                continue;
            }
            for statement in self.scopes[file].body {
                let StatementKind::Assign {
                    names,
                    value: Some(value),
                    ..
                } = &statement.kind
                else {
                    continue;
                };
                let [setting] = names.as_slice() else {
                    continue;
                };
                let entries: Vec<_> = match setting.text.as_str() {
                    "LOGGING" => literal_settings_entries(value)
                        .filter(|(name, _)| matches!(*name, "filters" | "formatters" | "handlers"))
                        .flat_map(|(_, group)| literal_settings_entries(group))
                        .map(|(_, entry)| (entry, "()"))
                        .collect(),
                    "STORAGES" => literal_settings_entries(value)
                        .map(|(_, entry)| (entry, "BACKEND"))
                        .collect(),
                    _ => continue,
                };
                for (entry, key) in entries {
                    if let Some((_, provider)) =
                        literal_settings_entries(entry).find(|(name, _)| *name == key)
                        && let ExpressionKind::Text(target) = &provider.kind
                        && let Some((module, name)) = target.rsplit_once('.')
                        && let Some(&source) = self.modules.get(module)
                        && let Some(binding) = self.local(source, name, END)
                        && self.is_class(binding)
                    {
                        // A settings declaration can name a class even when a later
                        // assignment replaces the setting. Do not run protocol methods.
                        self.edge(
                            file,
                            binding,
                            "django_configured_class_declaration",
                            "possible",
                        );
                    }
                }
            }
        }
    }

    pub(super) fn collect_phase03_frameworks(&mut self) {
        self.collect_django_urlconfs();
        self.retain_django_configured_class_declarations();
        self.collect_flask_cli_factory();

        let classes: Vec<_> = (0..self.symbols.len())
            .filter(|&id| self.is_class(id) && !self.dormant_classes.contains(&id))
            .collect();
        for class in classes {
            let Some(role) = self.framework_class_role(class, &mut BTreeSet::new(), 0) else {
                continue;
            };
            if role == FrameworkClassRole::DjangoAdmin {
                self.framework_class_whitelists
                    .insert(class, "Django framework class");
            }
            let (root, lifecycle, fields) = match role {
                FrameworkClassRole::Unittest
                | FrameworkClassRole::UnittestAsync
                | FrameworkClassRole::DjangoTestCase
                | FrameworkClassRole::DjangoDatabaseTestCase => (None, &[][..], false),
                FrameworkClassRole::DjangoAppConfig | FrameworkClassRole::DjangoCommand => {
                    (None, &[][..], false)
                }
                FrameworkClassRole::DjangoView => (
                    None,
                    &[
                        "get", "post", "put", "patch", "delete", "head", "options", "dispatch",
                    ][..],
                    false,
                ),
                FrameworkClassRole::DjangoModel
                | FrameworkClassRole::DjangoForm
                | FrameworkClassRole::DjangoModelForm
                | FrameworkClassRole::DjangoFormField
                | FrameworkClassRole::DjangoSignal => (None, &[][..], false),
                FrameworkClassRole::DjangoAdmin => (None, &[][..], true),
                FrameworkClassRole::CeleryTask | FrameworkClassRole::MarshmallowSchema => {
                    (None, &[][..], false)
                }
                FrameworkClassRole::SqlAlchemyModel => (
                    Some("sqlalchemy_model"),
                    &["__declare_first__", "__declare_last__"][..],
                    true,
                ),
                FrameworkClassRole::SqlModel => (None, &[][..], true),
                FrameworkClassRole::PydanticModel => (
                    None,
                    &[
                        "model_post_init",
                        "__get_pydantic_core_schema__",
                        "__get_pydantic_json_schema__",
                    ][..],
                    true,
                ),
            };
            if role.is_unittest() {
                self.unittest_classes.insert(class, role);
            }
            if let Some(kind) = root {
                self.framework_roots.push(Edge {
                    target: class,
                    kind,
                    certainty: "certain",
                });
            }
            let Binding::Definition(_, class_scope) = self.symbols[class].binding else {
                continue;
            };
            if role == FrameworkClassRole::PydanticModel {
                self.collect_pydantic_schema_build(class, class_scope);
            }
            if role == FrameworkClassRole::MarshmallowSchema {
                self.collect_marshmallow_registration(class, class_scope);
            }
            for name in lifecycle {
                if let Some(target) = self.local(class_scope, name, END)
                    && matches!(self.symbols[target].binding, Binding::Definition(_, _))
                {
                    self.framework_lifecycle
                        .push((class, target, "framework_lifecycle"));
                }
            }
            if fields {
                for bindings in self.scopes[class_scope].names.values() {
                    for &target in self.effective_bindings(bindings) {
                        if matches!(self.symbols[target].binding, Binding::Variable(_, _)) {
                            self.framework_fields.insert(target);
                        }
                    }
                }
            }
        }

        self.collect_test_frameworks();
    }

    fn collect_marshmallow_registration(&mut self, class: SymbolId, class_scope: ScopeId) {
        let (lineage, _) = self.class_lineage(class);
        match self.marshmallow_register_option(&lineage, class_scope) {
            Some(true) => self.edge(class_scope, class, "marshmallow_schema_registry", "certain"),
            Some(false) => {}
            None => self.scope_issue(
                class_scope,
                self.symbols[class].name.at,
                "unresolved_registration",
                "Marshmallow SchemaMeta registration options could not be resolved",
            ),
        }
    }

    fn marshmallow_register_option(
        &mut self,
        lineage: &[ScopeId],
        class_scope: ScopeId,
    ) -> Option<bool> {
        // A custom OPTIONS_CLASS can determine registration independently of Meta.
        if lineage
            .iter()
            .any(|&scope| self.local(scope, "OPTIONS_CLASS", END).is_some())
        {
            return None;
        }
        let Some(meta) = lineage
            .iter()
            .find_map(|&scope| self.local(scope, "Meta", END))
        else {
            return Some(true);
        };
        let Value::Callable(meta) = self.binding(meta, END, 0) else {
            return None;
        };
        if !self.is_class(meta.target) {
            return None;
        }
        self.edge(
            class_scope,
            meta.target,
            "marshmallow_schema_options",
            "certain",
        );
        let (meta_lineage, complete) = self.class_lineage(meta.target);
        for &scope in &meta_lineage {
            if let Some(option) = self.local(scope, "register", END) {
                if self.binding_depends_on_branch(option, END) {
                    return None;
                }
                self.edge(class_scope, option, "marshmallow_schema_options", "certain");
                return match self.binding_value(option, 0) {
                    Value::Bool(register) => Some(register),
                    _ => None,
                };
            }
        }
        complete.then_some(true)
    }

    pub(super) fn record_marshmallow_hook(&mut self, scope: ScopeId, target: SymbolId) -> bool {
        let Binding::Definition(definition, _) = self.symbols[target].binding else {
            return false;
        };
        if definition.kind != DefinitionKind::Function || definition.decorators.len() != 1 {
            return false;
        }
        if self.scopes[scope].owner.is_some_and(|class| {
            self.is_class(class)
                && self.framework_class_role(class, &mut BTreeSet::new(), 0)
                    == Some(FrameworkClassRole::MarshmallowSchema)
        }) && self.local(scope, &definition.name.text, END) == Some(target)
        {
            if self.binding_depends_on_branch(target, END) {
                self.scope_issue(
                    scope,
                    definition.name.at,
                    "unresolved_registration",
                    "Marshmallow schema hook depends on an unresolved branch",
                );
            } else {
                // SchemaMeta stores the marked method in its hooks table without
                // executing it. Load/dump dispatch is a separate operation.
                self.edge(scope, target, "marshmallow_hook_declaration", "certain");
            }
        }
        true
    }

    fn collect_pydantic_schema_build(&mut self, class: SymbolId, class_scope: ScopeId) {
        let (lineage, _) = self.class_lineage(class);
        let Some(hook) = lineage
            .iter()
            .find_map(|&scope| self.local(scope, "__get_pydantic_core_schema__", END))
        else {
            return;
        };
        let deferred = self.pydantic_defer_build(&lineage);
        if deferred == Some(true) {
            return;
        }
        let certainty = if deferred == Some(false)
            && !self.conditional.contains_key(&class)
            && !self.conditional.contains_key(&hook)
        {
            "certain"
        } else {
            "possible"
        };
        let hooks = match self.binding(hook, END, 0) {
            Value::Alternatives(values) => values,
            value => vec![value],
        };
        for hook in hooks {
            if let Value::Callable(invocation) = hook {
                // Schema generation runs as part of class creation, even if
                // no later code references or instantiates the model.
                self.edge(
                    class_scope,
                    invocation.target,
                    "pydantic_schema_build",
                    certainty,
                );
            } else {
                self.scope_issue(
                    class_scope,
                    self.symbols[class].name.at,
                    "unresolved_registration",
                    "Pydantic core schema hook could not be resolved",
                );
            }
        }
    }

    fn pydantic_defer_build(&mut self, lineage: &[ScopeId]) -> Option<bool> {
        // Subclasses merge model_config by key, with nearer classes winning.
        for &scope in lineage {
            if let Some(config) = self.local(scope, "model_config", END) {
                let Value::Mapping(items) = self.binding(config, END, 0) else {
                    return None;
                };
                for [key, value] in items.as_chunks::<2>().0.iter().rev() {
                    match (key, value) {
                        (Value::Text(key), Value::Bool(value)) if key == "defer_build" => {
                            return Some(*value);
                        }
                        (Value::Text(key), _) if key != "defer_build" => {}
                        _ => return None,
                    }
                }
            } else if self.local(scope, "Config", END).is_some() {
                return None;
            }
        }
        Some(false)
    }

    fn collect_test_frameworks(&mut self) {
        let mut plugins = BTreeSet::new();
        let mut declarations = Vec::new();
        for file in 0..self.files.len() {
            let path = &self.files[file].path;
            if let Some(prefix) = pytest_conftest_prefix(path)
                && self.files.iter().enumerate().any(|(test, source)| {
                    source.path.starts_with(prefix) && self.is_pytest_file(test)
                })
            {
                plugins.insert(file);
                declarations.push(file);
            } else if self.is_pytest_file(file) {
                declarations.push(file);
            }
            if !self.is_pytest_file(file)
                && !path
                    .rsplit('/')
                    .next()
                    .is_some_and(|name| name.starts_with("test"))
            {
                continue;
            }
            self.collect_unittest_module(file);
        }
        while let Some(file) = declarations.pop() {
            let Some(binding) = self.local(file, "pytest_plugins", END) else {
                continue;
            };
            match self.binding(binding, END, 0) {
                Value::Text(module) => self.collect_pytest_plugin(
                    file,
                    binding,
                    &module,
                    &mut plugins,
                    &mut declarations,
                ),
                Value::Sequence(values) => {
                    for value in values {
                        if let Value::Text(module) = value {
                            self.collect_pytest_plugin(
                                file,
                                binding,
                                &module,
                                &mut plugins,
                                &mut declarations,
                            );
                        } else {
                            self.unresolved_pytest_plugins(file, binding);
                        }
                    }
                }
                _ => self.unresolved_pytest_plugins(file, binding),
            }
        }
        for file in 0..self.files.len() {
            if !plugins.contains(&file) {
                self.collect_possible_pytest_hooks(file);
            }
        }
        for file in plugins {
            for (hook, certain) in self.pytest_registrations(&self.scopes[file].names, "pytest_") {
                if self.is_pytest_function(hook.target) {
                    self.read.extend(hook.bindings);
                    self.framework_root(hook.target, "pytest_hook", certain);
                }
            }
        }
    }

    fn collect_unittest_module(&mut self, file: ScopeId) {
        let mut runnable = None;
        for (class, certain) in self.pytest_registrations(&self.scopes[file].names, "") {
            let Some(&role) = self.unittest_classes.get(&class.target) else {
                continue;
            };
            self.read.extend(class.bindings);
            self.framework_root(class.target, "unittest_discovery", certain);
            let (lineage, _) = self.class_lineage(class.target);
            let mut names = BTreeMap::new();
            let mut overrides = BTreeMap::new();
            for scope in lineage {
                let owner = self.scopes[scope].owner.expect("class scope has an owner");
                let writes = self.class_discovery_writes(owner, file);
                for (name, write) in &writes {
                    if !names.contains_key(name.as_str())
                        && !matches!(write, mutations::MemberWriteValue::Deleted)
                    {
                        overrides.entry(name.clone()).or_insert(write.clone());
                    }
                }
                for (&name, bindings) in &self.scopes[scope].names {
                    if writes.contains_key(name) || overrides.contains_key(name) {
                        continue;
                    }
                    // A bare annotation changes __annotations__, not the
                    // actual class attribute consulted by discovery.
                    let bindings: Vec<_> = bindings
                        .iter()
                        .copied()
                        .filter(|&id| {
                            !matches!(self.symbols[id].binding, Binding::Variable(None, _))
                        })
                        .collect();
                    if !bindings.is_empty() {
                        names.entry(name).or_insert(bindings);
                    }
                }
            }
            let mut tests = self.pytest_registrations(&names, "test");
            tests.retain(|(method, _)| self.unittest_callable(method.target));
            let mut opaque_test = false;
            for (name, write) in &overrides {
                if name.starts_with("test") {
                    tests.extend(self.unittest_write(write));
                    opaque_test |= matches!(
                        write,
                        mutations::MemberWriteValue::Replacement(Value::ConstantFunction)
                    );
                }
                if matches!(write, mutations::MemberWriteValue::Ambiguous) {
                    let symbol = &self.symbols[class.target];
                    self.scope_issue(
                        file,
                        symbol.name.at,
                        "unresolved_unittest_member",
                        "unittest member changed by a write with unresolved ordering or value",
                    );
                }
            }
            if tests.is_empty() && !opaque_test {
                tests = self.unittest_member(&names, &overrides, "runTest");
                opaque_test = matches!(
                    overrides.get("runTest"),
                    Some(mutations::MemberWriteValue::Replacement(
                        Value::ConstantFunction
                    ))
                );
            }
            if tests.is_empty() && !opaque_test {
                continue;
            }
            let certain_case =
                certain && (opaque_test || tests.iter().any(|(_, selected)| *selected));
            runnable = Some(runnable.unwrap_or(false) || certain_case);
            if self.is_pytest_file(file) {
                self.pytest_unittest_cases
                    .entry(file)
                    .or_default()
                    .push((names.clone(), certain_case));
            }
            let asynchronous = role == FrameworkClassRole::UnittestAsync;
            let django = matches!(
                role,
                FrameworkClassRole::DjangoTestCase | FrameworkClassRole::DjangoDatabaseTestCase
            );
            for (method, selected) in tests {
                if asynchronous || django || !self.is_coroutine(method.target) {
                    self.unittest_method(class.target, method, certain && selected);
                }
            }
            let mut lifecycle = vec!["setUp", "tearDown", "setUpClass", "tearDownClass"];
            if asynchronous {
                lifecycle.extend(["asyncSetUp", "asyncTearDown"]);
            }
            if django {
                lifecycle.extend(["_pre_setup", "_post_teardown"]);
            }
            if role == FrameworkClassRole::DjangoDatabaseTestCase {
                lifecycle.push("setUpTestData");
            }
            for name in lifecycle {
                for (method, selected) in self.unittest_member(&names, &overrides, name) {
                    if self.is_coroutine(method.target) == name.starts_with("async") {
                        self.unittest_method(class.target, method, certain_case && selected);
                    }
                }
            }
        }
        if let Some(certain) = runnable {
            for name in ["setUpModule", "tearDownModule"] {
                let Some(bindings) = self.scopes[file].names.get(name) else {
                    continue;
                };
                let bindings: Vec<_> = bindings
                    .iter()
                    .copied()
                    .filter(|&id| !matches!(self.symbols[id].binding, Binding::Variable(None, _)))
                    .collect();
                for (method, selected) in self.callable_candidates(&bindings) {
                    let target = method.target;
                    if !self.unittest_callable(target) || self.is_coroutine(target) {
                        continue;
                    }
                    self.read.extend(method.bindings);
                    self.framework_root(
                        target,
                        if self.is_generator(target) {
                            "generator_creation"
                        } else {
                            "unittest_discovery"
                        },
                        certain && selected,
                    );
                }
            }
        }
    }

    fn unittest_member(
        &mut self,
        names: &PytestNames<'_>,
        overrides: &BTreeMap<String, mutations::MemberWriteValue>,
        name: &str,
    ) -> Vec<(CallableBinding, bool)> {
        if let Some(write) = overrides.get(name) {
            return self.unittest_write(write).into_iter().collect();
        }
        let Some(bindings) = names.get(name) else {
            return Vec::new();
        };
        self.callable_candidates(bindings)
            .into_iter()
            .filter(|(method, _)| self.unittest_callable(method.target))
            .collect()
    }

    fn unittest_write(
        &mut self,
        write: &mutations::MemberWriteValue,
    ) -> Option<(CallableBinding, bool)> {
        let mutations::MemberWriteValue::Replacement(Value::Callable(invocation)) = write else {
            return None;
        };
        self.unittest_callable(invocation.target).then_some((
            CallableBinding {
                target: invocation.target,
                bindings: Vec::new(),
            },
            true,
        ))
    }

    fn unittest_callable(&mut self, target: SymbolId) -> bool {
        if !self.is_pytest_function(target) {
            return false;
        }
        let symbol = &self.symbols[target];
        let Binding::Definition(definition, _) = symbol.binding else {
            unreachable!()
        };
        let scope = symbol.scope;
        definition.decorators.iter().all(|decorator| {
            let value = self.probe_identity(scope, decorator, definition.name.at);
            matches!(
                &value,
                Value::Provider(provider)
                    if matches!(provider.as_str(), "builtins.classmethod" | "builtins.staticmethod")
            ) || self.local_identity_decorator(value, decorator)
        })
    }

    fn unittest_method(&mut self, class: SymbolId, method: CallableBinding, certain: bool) {
        self.read.extend(method.bindings);
        if self.is_generator(method.target) {
            self.framework_root(method.target, "generator_creation", certain);
            return;
        }
        self.method_receivers
            .entry(method.target)
            .or_default()
            .insert(class);
        self.framework_root(method.target, "unittest_discovery", certain);
    }

    fn collect_possible_pytest_hooks(&mut self, file: ScopeId) {
        let basename = self.files[file].path.rsplit('/').next().unwrap_or_default();
        if !(basename == "plugin.py"
            || basename.starts_with("pytest_")
            || basename.ends_with("_plugin.py"))
            || self.modules.contains_key("pytest")
            || !self.scopes[file].names.values().any(|bindings| {
                let &[id] = self.effective_bindings(bindings) else {
                    return false;
                };
                !self.conditional.contains_key(&id)
                    && matches!(self.symbols[id].binding, Binding::Import(import)
                        if import.level == 0 && import.module == "pytest" && import.member.is_none())
            })
        {
            return;
        }

        // A conventionally named plugin can be loaded by an external runner.
        // Without a registration in the project, its hooks have possible use.
        self.retain_possible_pytest_hooks(file, false);
        let classes: Vec<_> = self.scopes[file]
            .names
            .iter()
            .filter_map(|(name, bindings)| {
                let &[owner] = self.effective_bindings(bindings) else {
                    return None;
                };
                let Binding::Definition(definition, scope) = self.symbols[owner].binding else {
                    return None;
                };
                (name.ends_with("Plugin")
                    && definition.kind == DefinitionKind::Class
                    && definition.bases.is_empty()
                    && definition.decorators.is_empty()
                    && !self.conditional.contains_key(&owner)
                    && !self.dormant_classes.contains(&owner))
                .then_some((owner, scope))
            })
            .collect();
        for (owner, scope) in classes {
            if self.retain_possible_pytest_hooks(scope, true) {
                self.framework_root(owner, "pytest_hook_candidate", false);
            }
        }
    }

    fn retain_possible_pytest_hooks(&mut self, scope: ScopeId, method: bool) -> bool {
        const HOOKS: &[(&str, &[&str])] = &[
            ("pytest_addhooks", &["pluginmanager"]),
            (
                "pytest_assertrepr_compare",
                &["config", "op", "left", "right"],
            ),
            ("pytest_cmdline_main", &["config"]),
            ("pytest_collection_finish", &["session"]),
            ("pytest_fixture_setup", &["fixturedef", "request"]),
            ("pytest_sessionfinish", &["session", "exitstatus"]),
        ];
        let mut found = false;
        for (name, arguments) in HOOKS {
            let Some(bindings) = self.scopes[scope].names.get(name) else {
                continue;
            };
            let &[target] = self.effective_bindings(bindings) else {
                continue;
            };
            let Binding::Definition(definition, _) = self.symbols[target].binding else {
                continue;
            };
            let expected = method
                .then_some("self")
                .into_iter()
                .chain(arguments.iter().copied());
            if definition.kind == DefinitionKind::Function
                && definition.decorators.is_empty()
                && definition.simple_positional_parameters
                && definition
                    .parameters
                    .iter()
                    .map(|name| name.text.as_str())
                    .eq(expected)
                && !self.conditional.contains_key(&target)
            {
                self.framework_root(target, "pytest_hook_candidate", false);
                found = true;
            }
        }
        found
    }

    fn collect_pytest_plugin(
        &mut self,
        file: usize,
        binding: SymbolId,
        module: &str,
        plugins: &mut BTreeSet<usize>,
        declarations: &mut Vec<usize>,
    ) {
        if let Some(&plugin) = self.modules.get(module) {
            if plugins.insert(plugin) {
                declarations.push(plugin);
                self.pytest_plugins.push(plugin);
            }
        } else {
            self.unresolved_pytest_plugins(file, binding);
        }
    }

    fn unresolved_pytest_plugins(&mut self, file: usize, binding: SymbolId) {
        self.issue(
            file,
            self.symbols[binding].name.at,
            "unresolved_registration",
            "pytest_plugins contains a module outside the bounded discovery model",
        );
    }

    fn framework_root(&mut self, target: SymbolId, kind: &'static str, certain: bool) {
        self.framework_roots.push(Edge {
            target,
            kind,
            certainty: if certain { "certain" } else { "possible" },
        });
    }

    pub(super) fn django_admin_stub_packages(&self) -> BTreeSet<&Path> {
        if !self.phase03
            || self.modules.contains_key("django")
            || self.files.iter().any(|source| {
                let mut parts = source.path.split('/');
                let first = parts.next();
                let top = match first {
                    Some("src" | "lib" | "python") => parts.next(),
                    first => first,
                };
                matches!(top, Some("django" | "django.py"))
            })
            || !self.symbols.iter().any(|entry| {
                matches!(entry.binding, Binding::Import(import)
                    if import.level == 0
                        && import.module.split('.').next() == Some("django")
                        && self.import_module(self.scopes[entry.scope].file, import).as_deref()
                            == Some(import.module.as_str()))
            })
        {
            return BTreeSet::new();
        }
        self.files
            .iter()
            .filter(|source| source.is_package)
            .filter_map(|source| Path::new(&source.path).parent())
            .collect()
    }

    pub(super) fn record_django_converter(&mut self, call: CallArguments<'_>) {
        let CallArguments {
            scope,
            before,
            at,
            positional,
            keywords,
        } = call;
        if let Some([Some(target), Some(_)]) =
            registration_arguments(positional, keywords, ["converter", "type_name"])
        {
            let conditional = self.branch_depth != 0
                || self.scopes[scope]
                    .owner
                    .is_some_and(|owner| self.conditional.contains_key(&owner));
            let value = self.identity_with_dormant_declarations(scope, target, before, 0);
            if let Some(class) = self.django_converter_class(&value) {
                self.register_django_converter(scope, class, at, conditional);
                return;
            }
            if let Some(wrapper) =
                self.django_parameter_registration(scope, target, before, at, conditional)
                && self.bounded_framework_function(wrapper.owner)
            {
                self.django_converter.wrappers.push(wrapper);
                return;
            }
        }
        self.scope_issue(
            scope,
            at,
            "unresolved_registration",
            "Django converter registration requires a resolved class and a converter name",
        );
    }

    pub(super) fn django_converter_noop_rebinding(
        &mut self,
        scope: ScopeId,
        function: &Expression,
        at: Location,
        before: Location,
    ) -> Option<Invocation> {
        let ExpressionKind::Name(name) = &function.kind else {
            return None;
        };
        let module = self.scopes[scope].file;
        let owner = self.scopes[scope].owner?;
        let node = &self.symbols[owner];
        let Binding::Definition(wrapper, _) = node.binding else {
            return None;
        };
        if node.scope != module
            || wrapper.kind != DefinitionKind::Function
            || wrapper.is_generator
            || !wrapper.decorators.is_empty()
            || !wrapper.parameters.is_empty()
            || !matches!(wrapper.body.as_slice(), [Statement { kind: StatementKind::Evaluate(call), .. }]
                if position(call.at) == position(at))
            || !matches!(self.lexical_binding(scope, name, before, true), LexicalBinding::Found(id)
                if self.symbols[id].scope == module)
        {
            return None;
        }
        let history = self.scopes[module].names.get(name.as_str())?.clone();
        if history.len() < 2
            || !history.into_iter().any(|binding| {
                matches!(self.symbols[binding].binding, Binding::Import(_))
                    && matches!(self.binding_value(binding, 0), Value::Provider(provider)
                        if matches!(provider.as_str(), "django.urls.register_converter"
                            | "django.urls.converters.register_converter"))
            })
        {
            return None;
        }

        // Module suites have already been visited. Resolve the global name
        // at every witnessed direct call, not at function definition time.
        let callers = self.direct_arguments.get(&owner)?;
        let bindings = callers
            .iter()
            .map(|call| {
                if call.caller != module
                    || !call.arguments_bound
                    || call.conditional
                    || self
                        .module_member_write(&self.files[module].module, name, (module, call.at))
                        .is_some()
                {
                    return None;
                }
                self.local(module, name, call.at)
            })
            .collect::<Option<BTreeSet<_>>>()?;
        let mut bindings = bindings.into_iter();
        let binding = bindings.next()?;
        if bindings.next().is_some() || self.binding_depends_on_branch(binding, END) {
            return None;
        }
        let Value::Callable(invocation) = self.binding_value(binding, 0) else {
            return None;
        };
        let replacement = &self.symbols[invocation.target];
        let Binding::Definition(definition, _) = replacement.binding else {
            return None;
        };
        if invocation.verified
            || !matches!(invocation.receiver, Receiver::Ordinary)
            || replacement.scope != module
            || definition.kind != DefinitionKind::Function
            || definition.is_generator
            || !definition.decorators.is_empty()
            || !definition.simple_positional_parameters
            || definition.parameters.len() != 2
            || !matches!(
                definition.body.as_slice(),
                [Statement { kind: StatementKind::Return(Some(value)), .. }]
                    if matches!(value.kind, ExpressionKind::None)
            )
        {
            return None;
        }
        self.read.insert(binding);
        self.django_converter.noop_wrappers.push((scope, at));
        Some(invocation)
    }

    fn django_converter_class(&self, value: &Value) -> Option<SymbolId> {
        let Value::Callable(invocation) = value else {
            return None;
        };
        (!invocation.verified && self.is_class(invocation.target)).then_some(invocation.target)
    }

    fn django_parameter_registration(
        &self,
        site: ScopeId,
        target: &Expression,
        before: Location,
        at: Location,
        conditional: bool,
    ) -> Option<DjangoParameterRegistration> {
        let parameter = self.parameter_binding(site, target, before)?;
        (self.symbols[parameter].scope == site).then_some(())?;
        let owner = self.scopes[site].owner?;
        Some(DjangoParameterRegistration {
            site,
            parameter,
            owner,
            at,
            conditional,
        })
    }

    fn django_converter_wrapper_class(
        &self,
        wrapper: DjangoParameterRegistration,
        call: &DirectArguments,
    ) -> Option<SymbolId> {
        call.arguments_bound.then_some(())?;
        let value = call
            .values
            .get(&self.symbols[wrapper.parameter].name.text)?;
        self.django_converter_class(value)
    }

    pub(super) fn collect_django_converter_wrappers(&mut self) {
        for index in 0..self.django_converter.wrappers.len() {
            let wrapper = self.django_converter.wrappers[index];
            let registrations: Vec<_> = self
                .direct_arguments
                .get(&wrapper.owner)
                .into_iter()
                .flatten()
                .filter_map(|call| {
                    let class = self.django_converter_wrapper_class(wrapper, call)?;
                    let conditional = wrapper.conditional
                        || call.conditional
                        || self.scopes[call.caller].owner.is_some();
                    Some((call.caller, class, call.at, conditional))
                })
                .collect();
            for (caller, class, at, conditional) in registrations {
                // The call-site scope gates the effects even if another
                // caller makes the wrapper body independently reachable.
                self.register_django_converter(caller, class, at, conditional);
            }
        }
    }

    pub(super) fn report_django_converter_wrappers(&mut self, reach: &Reachability) {
        for wrapper in &self.django_converter.wrappers {
            if !reach.scopes.contains(&wrapper.site) {
                continue;
            }
            let mut callers = self
                .direct_arguments
                .get(&wrapper.owner)
                .into_iter()
                .flatten()
                .filter(|call| reach.scopes.contains(&call.caller))
                .peekable();
            let independent = reach
                .evidence
                .range((wrapper.owner, "", "")..)
                .take_while(|&&(owner, _, _)| owner == wrapper.owner)
                .any(|&(_, kind, _)| !matches!(kind, "direct_call" | "type_reference"));
            if independent
                || callers.peek().is_none()
                || !callers.all(|call| {
                    self.django_converter_wrapper_class(*wrapper, call)
                        .is_some()
                })
            {
                self.issues.push(
                    Issue::new(
                        "unresolved_registration",
                        &self.files[self.scopes[wrapper.site].file].path,
                        "Django converter wrapper has an unresolved class argument",
                    )
                    .at(wrapper.at),
                );
            }
        }
        for &(scope, at) in &self.django_converter.noop_wrappers {
            let row = &self.scopes[scope];
            let owner = row.owner.expect("converter wrapper is a function body");
            // Later-discovered nested callers and independent entry points
            // cannot be represented by the module's direct call order.
            let has_unordered_call = self.direct_arguments[&owner]
                .iter()
                .any(|call| call.caller != row.file && reach.scopes.contains(&call.caller));
            let independently_reached = reach
                .evidence
                .range((owner, "", "")..)
                .take_while(|&&(target, _, _)| target == owner)
                .any(|&(_, kind, _)| !matches!(kind, "direct_call" | "type_reference"));
            if has_unordered_call || independently_reached {
                self.issues.push(
                    Issue::new(
                        "ambiguous_binding",
                        &self.files[row.file].path,
                        "Django converter binding may differ for a caller outside the bounded module order",
                    ).at(at),
                );
            }
        }
    }

    fn register_django_converter(
        &mut self,
        scope: ScopeId,
        class: SymbolId,
        at: Location,
        conditional: bool,
    ) {
        let kind = "django_path_converter";
        let certainty = if conditional { "possible" } else { "certain" };
        self.edge(scope, class, kind, certainty);
        let Some(mro) = self.converter_mro(class, &mut BTreeMap::new(), 0) else {
            self.scope_issue(
                scope,
                at,
                "unresolved_registration",
                "Django converter class hierarchy could not be resolved",
            );
            return;
        };
        match self.converter_member(&mro, "regex") {
            Some(ConverterMember::Source(target))
                if !self.binding_depends_on_branch(target, END)
                    && matches!(self.binding_value(target, 0), Value::Text(_)) =>
            {
                // Probing a dormant registration must not mark the field as read.
                self.edge(scope, target, "django_converter_regex", certainty);
            }
            Some(ConverterMember::Builtin) => {}
            // Registration can store the instance without compiling a URL
            // pattern. A class without regex cannot supply its callbacks.
            None => return,
            Some(ConverterMember::Source(_)) => {
                self.scope_issue(
                    scope,
                    at,
                    "unresolved_registration",
                    "Django converter regex could not be resolved",
                );
                return;
            }
        }
        for name in ["to_python", "to_url"] {
            let Some(ConverterMember::Source(binding)) = self.converter_member(&mro, name) else {
                // The built-in protocol does not introduce a source callback.
                continue;
            };
            match self.binding(binding, END, 0) {
                Value::Callable(invocation)
                    if let Some(descriptor) = self.converter_descriptor(invocation.target) =>
                {
                    let descriptor_arguments =
                        usize::from(descriptor != ConverterDescriptor::Static);
                    let input = if name == "to_python"
                        && let Binding::Definition(definition, body) =
                            self.symbols[invocation.target].binding
                        && definition.simple_positional_parameters
                        && definition.parameters.len() == descriptor_arguments + 1
                    {
                        let value = &definition.parameters[descriptor_arguments];
                        self.by_location
                            .get(&(body, value.at.line, value.at.column))
                            .copied()
                    } else {
                        None
                    };
                    self.django_converter
                        .inputs
                        .entry(invocation.target)
                        .or_default()
                        .push((scope, input));
                    let invocation = if descriptor == ConverterDescriptor::Instance {
                        invocation.bind_receiver(Receiver::Bound(class))
                    } else {
                        invocation
                    };
                    self.invoke(scope, invocation, kind, certainty, at);
                }
                Value::ConstantFunction => {}
                _ => self.scope_issue(
                    scope,
                    at,
                    "unresolved_registration",
                    "Django converter method could not be resolved",
                ),
            }
        }
    }

    fn converter_member(&self, mro: &[ConverterBase], name: &str) -> Option<ConverterMember> {
        for entry in mro {
            match *entry {
                ConverterBase::Source(class) => {
                    let Binding::Definition(_, body) = self.symbols[class].binding else {
                        unreachable!()
                    };
                    if let Some(binding) = self.local(body, name, END) {
                        return Some(ConverterMember::Source(binding));
                    }
                }
                ConverterBase::String => return Some(ConverterMember::Builtin),
                ConverterBase::Object => {}
            }
        }
        None
    }

    fn converter_mro(
        &mut self,
        class: SymbolId,
        memo: &mut BTreeMap<SymbolId, Vec<ConverterBase>>,
        depth: usize,
    ) -> Option<Vec<ConverterBase>> {
        if let Some(lineage) = memo.get(&class) {
            return Some(lineage.clone());
        }
        let symbol = &self.symbols[class];
        let Binding::Definition(definition, _) = symbol.binding else {
            return None;
        };
        let scope = symbol.scope;
        if depth >= MAX_BINDING_CHAIN
            || definition.bases.len() >= MAX_BINDING_CHAIN
            || !definition.decorators.iter().all(|decorator| {
                let value = self.probe_identity(scope, decorator, definition.name.at);
                self.local_identity_decorator(value, decorator)
            })
        {
            return None;
        }
        let mut lineage = vec![ConverterBase::Source(class)];
        if definition.bases.is_empty() {
            lineage.push(ConverterBase::Object);
        } else {
            let mut bases = VecDeque::new();
            let mut orders = Vec::new();
            for base in &definition.bases {
                let order = match self.probe_identity(scope, base, definition.name.at) {
                    Value::Callable(invocation) if self.is_class(invocation.target) => {
                        self.converter_mro(invocation.target, memo, depth + 1)?
                    }
                    Value::Provider(provider) if provider == "builtins.object" => {
                        vec![ConverterBase::Object]
                    }
                    Value::Provider(provider) if is_django_string_converter(&provider) => {
                        vec![ConverterBase::String, ConverterBase::Object]
                    }
                    _ => return None,
                };
                let direct = order[0];
                if bases.contains(&direct) {
                    return None;
                }
                bases.push_back(direct);
                orders.push(VecDeque::from(order));
            }
            orders.push(bases);
            // Python's C3 merge selects the first head that does not follow
            // another class in any remaining base order.
            while orders.iter().any(|order| !order.is_empty()) {
                if lineage.len() >= MAX_BINDING_CHAIN {
                    return None;
                }
                let next = orders
                    .iter()
                    .filter_map(|order| order.front().copied())
                    .find(|head| {
                        orders
                            .iter()
                            .all(|order| !order.iter().skip(1).any(|entry| entry == head))
                    })?;
                lineage.push(next);
                for order in &mut orders {
                    if order.front() == Some(&next) {
                        order.pop_front();
                    }
                }
            }
        }
        memo.insert(class, lineage.clone());
        Some(lineage)
    }

    fn converter_descriptor(&mut self, target: SymbolId) -> Option<ConverterDescriptor> {
        if self.is_coroutine(target) {
            return None;
        }
        self.bounded_method_descriptor(target)
    }

    fn bounded_method_descriptor(&mut self, target: SymbolId) -> Option<ConverterDescriptor> {
        let symbol = &self.symbols[target];
        let Binding::Definition(definition, _) = symbol.binding else {
            return None;
        };
        if definition.kind == DefinitionKind::Class || definition.is_generator {
            return None;
        }
        let scope = symbol.scope;
        let mut descriptor = ConverterDescriptor::Instance;
        for decorator in &definition.decorators {
            let value = self.probe_identity(scope, decorator, definition.name.at);
            match value {
                Value::Provider(provider)
                    if provider == "builtins.staticmethod"
                        && descriptor == ConverterDescriptor::Instance =>
                {
                    descriptor = ConverterDescriptor::Static;
                }
                Value::Provider(provider)
                    if provider == "builtins.classmethod"
                        && descriptor == ConverterDescriptor::Instance =>
                {
                    descriptor = ConverterDescriptor::Class;
                }
                value => {
                    if !self.local_identity_decorator(value, decorator) {
                        return None;
                    }
                }
            }
        }
        Some(descriptor)
    }

    pub(super) fn report_converter_string_calls(&mut self, reach: &Reachability) {
        let mut proved = BTreeMap::new();
        for call in &self.django_converter.string_calls {
            if reach.scopes.contains(&call.scope)
                && !self.converter_string_parameter(call.parameter, reach, &mut proved, 0)
            {
                self.issues.push(
                    Issue::new(
                        "unresolved_call",
                        &self.files[self.scopes[call.scope].file].path,
                        "call target is outside the bounded model",
                    )
                    .at(call.at),
                );
            }
        }
    }

    fn converter_string_parameter(
        &self,
        parameter: SymbolId,
        reach: &Reachability,
        proved: &mut BTreeMap<SymbolId, bool>,
        depth: usize,
    ) -> bool {
        if depth >= MAX_BINDING_CHAIN {
            return false;
        }
        if let Some(&result) = proved.get(&parameter) {
            return result;
        }
        // Recursion with no established input is outside the bounded proof.
        proved.insert(parameter, false);
        let symbol = &self.symbols[parameter];
        let owner = self.scopes[symbol.scope]
            .owner
            .expect("parameters belong to a definition");
        let kinds = reach
            .evidence
            .range((owner, "", "")..)
            .take_while(|&&(target, _, _)| target == owner)
            .map(|&(_, kind, _)| kind)
            .filter(|kind| !matches!(*kind, "type_reference" | "generator_creation"))
            .collect::<BTreeSet<_>>();
        let result = !kinds.is_empty()
            && kinds.into_iter().all(|kind| match kind {
                "django_path_converter" => {
                    let mut candidates = self
                        .django_converter
                        .inputs
                        .get(&owner)
                        .into_iter()
                        .flatten()
                        .filter(|(scope, _)| reach.scopes.contains(scope))
                        .peekable();
                    // URL matching supplies a regex capture as a Python str.
                    // The same body can also be registered for to_url.
                    candidates.peek().is_some()
                        && candidates.all(|(_, input)| *input == Some(parameter))
                }
                "direct_call" => {
                    let mut callers = self
                        .direct_arguments
                        .get(&owner)
                        .into_iter()
                        .flatten()
                        .filter(|call| reach.scopes.contains(&call.caller))
                        .peekable();
                    callers.peek().is_some()
                        && callers.all(|call| {
                            call.forwarded
                                .get(&symbol.name.text)
                                .is_some_and(|&source| {
                                    self.converter_string_parameter(
                                        source,
                                        reach,
                                        proved,
                                        depth + 1,
                                    )
                                })
                        })
                }
                _ => false,
            });
        proved.insert(parameter, result);
        result
    }

    fn bounded_framework_function(&mut self, target: SymbolId) -> bool {
        let symbol = &self.symbols[target];
        let Binding::Definition(definition, _) = symbol.binding else {
            return false;
        };
        let scope = symbol.scope;
        definition.kind == DefinitionKind::Function
            && !definition.is_generator
            && definition.decorators.iter().all(|decorator| {
                let value = self.probe_identity(scope, decorator, definition.name.at);
                self.local_identity_decorator(value, decorator)
            })
    }

    pub(super) fn register_django_autoreload(&mut self, call: CallArguments<'_>) {
        let CallArguments {
            scope,
            before,
            at,
            positional,
            keywords,
        } = call;
        if let Some([Some(callback)]) = registration_arguments(positional, keywords, ["main_func"])
        {
            let conditional = self.branch_depth != 0
                || self.scopes[scope]
                    .owner
                    .is_some_and(|owner| self.conditional.contains_key(&owner));
            let value = self.identity_with_dormant_declarations(scope, callback, before, 0);
            if let Some(invocation) = self.django_autoreload_invocation(value) {
                let certainty = if conditional { "possible" } else { "certain" };
                self.invoke(scope, invocation, "django_autoreload", certainty, at);
                return;
            }
            if let Some(wrapper) =
                self.django_parameter_registration(scope, callback, before, at, conditional)
            {
                self.django_autoreload_wrappers.push(wrapper);
                return;
            }
        }
        self.scope_issue(
            scope,
            at,
            "unresolved_registration",
            "Django autoreload requires a bounded synchronous callback",
        );
    }

    fn django_autoreload_invocation(&mut self, value: Value) -> Option<Invocation> {
        let Value::Callable(invocation) = value else {
            return None;
        };
        (!invocation.verified
            && !matches!(invocation.receiver, Receiver::Explicit(_))
            && self.bounded_framework_function(invocation.target))
        .then_some(invocation)
    }

    fn django_autoreload_wrapper_targets(
        &mut self,
        hook: DjangoParameterRegistration,
        reach: &Reachability,
    ) -> (Vec<(Invocation, bool)>, bool) {
        let parameter = &self.symbols[hook.parameter].name.text;
        let candidates: Vec<_> = self
            .direct_arguments
            .get(&hook.owner)
            .into_iter()
            .flatten()
            .filter(|call| reach.scopes.contains(&call.caller))
            .map(|call| {
                (
                    call.arguments_bound
                        .then(|| call.values.get(parameter).cloned())
                        .flatten()
                        .unwrap_or(Value::Unknown),
                    call.conditional || self.scopes[call.caller].owner.is_some(),
                )
            })
            .collect();
        let independent = reach
            .evidence
            .range((hook.owner, "", "")..)
            .take_while(|&&(owner, _, _)| owner == hook.owner)
            .any(|&(_, kind, _)| !matches!(kind, "direct_call" | "type_reference"));
        let mut complete = !independent && !candidates.is_empty();
        let mut targets = Vec::new();
        for (value, conditional_call) in candidates {
            if let Some(invocation) = self.django_autoreload_invocation(value) {
                targets.push((invocation, hook.conditional || conditional_call));
            } else {
                complete = false;
            }
        }
        (targets, complete)
    }

    pub(super) fn activate_django_autoreload_wrappers(
        &mut self,
        reach: &mut Reachability,
        pending: &mut VecDeque<Edge>,
        possible: bool,
    ) {
        for index in 0..self.django_autoreload_wrappers.len() {
            let hook = self.django_autoreload_wrappers[index];
            if !reach.scopes.contains(&hook.site) {
                continue;
            }
            for (invocation, conditional) in self.django_autoreload_wrapper_targets(hook, reach).0 {
                let certainty = if possible || conditional {
                    "possible"
                } else {
                    "certain"
                };
                let kind = "django_autoreload";
                let target = invocation.target;
                if let Some(class) = invocation.receiver.supplied() {
                    reach
                        .deferred_method_receivers
                        .entry(target)
                        .or_default()
                        .insert(class);
                }
                if !reach.evidence.contains(&(target, kind, certainty)) {
                    pending.push_back(Edge {
                        target,
                        kind,
                        certainty,
                    });
                }
            }
        }
    }

    pub(super) fn report_django_autoreload_wrappers(&mut self, reach: &Reachability) {
        for index in 0..self.django_autoreload_wrappers.len() {
            let hook = self.django_autoreload_wrappers[index];
            if reach.scopes.contains(&hook.site)
                && !self.django_autoreload_wrapper_targets(hook, reach).1
            {
                self.issue(
                    self.scopes[hook.site].file,
                    hook.at,
                    "unresolved_registration",
                    "Django autoreload wrapper has an unresolved callback argument",
                );
            }
        }
    }

    pub(super) fn resolve_django_reloader_parameter_receivers(
        &mut self,
        reach: &mut Reachability,
        pending: &mut VecDeque<Edge>,
        possible: bool,
    ) {
        for index in 0..self.receiver_calls.len() {
            let call = &self.receiver_calls[index];
            if call.member != "run_with_reloader"
                || !reach.scopes.contains(&call.scope)
                || self.scopes[call.scope].parent.is_some_and(|parent| {
                    self.scopes[parent]
                        .owner
                        .is_some_and(|owner| self.is_class(owner))
                })
            {
                continue;
            }
            let Some(owner) = self.scopes[self.symbols[call.parameter].scope].owner else {
                continue;
            };
            reach.resolved_django_reloader_parameters.remove(&index);
            if reach
                .evidence
                .range((owner, "", "")..)
                .take_while(|&&(target, _, _)| target == owner)
                .any(|&(_, kind, _)| !matches!(kind, "direct_call" | "type_reference"))
            {
                continue;
            }
            let name = &self.symbols[call.parameter].name.text;
            let candidates: Option<Vec<_>> = self
                .direct_arguments
                .get(&owner)
                .into_iter()
                .flatten()
                .filter(|invocation| reach.scopes.contains(&invocation.caller))
                .map(|invocation| match invocation.values.get(name) {
                    // The recorded explicit arguments omit the implicit receiver
                    // of a module function installed as a class descriptor.
                    Some(Value::Instance(class))
                        if invocation.arguments_bound && !invocation.implicit_receiver =>
                    {
                        Some((*class, invocation.conditional))
                    }
                    _ => None,
                })
                .collect();
            let Some(candidates) = candidates else {
                continue;
            };
            let call = call.clone();
            let mut resolved = Vec::new();
            for (class, conditional) in candidates {
                let access = Some((call.scope, call.at));
                if ["__new__", "__init__", "__getattribute__", "__getattr__"]
                    .into_iter()
                    .any(|name| self.class_member_at(class, name, 0, access).is_some())
                {
                    resolved.clear();
                    break;
                }
                let target = match self.class_member_at(class, &call.member, 0, access) {
                    Some(Value::ConstantFunction) => None,
                    Some(Value::Callable(invocation))
                        if self.bounded_framework_function(invocation.target) =>
                    {
                        Some(invocation.target)
                    }
                    _ => {
                        resolved.clear();
                        break;
                    }
                };
                resolved.push((class, conditional, target));
            }
            if !resolved.is_empty() {
                reach.resolved_django_reloader_parameters.insert(index);
            }
            for (class, conditional, target) in resolved {
                let Some(target) = target else {
                    continue;
                };
                let certainty = if possible || conditional {
                    "possible"
                } else {
                    "certain"
                };
                if !reach.evidence.contains(&(target, "direct_call", certainty)) {
                    pending.push_back(Edge {
                        target,
                        kind: "direct_call",
                        certainty,
                    });
                }
                reach
                    .deferred_method_receivers
                    .entry(target)
                    .or_default()
                    .insert(class);
            }
        }
    }

    pub(super) fn framework_class_role(
        &mut self,
        class: SymbolId,
        seen: &mut BTreeSet<SymbolId>,
        depth: usize,
    ) -> Option<FrameworkClassRole> {
        if depth >= MAX_BINDING_CHAIN || !seen.insert(class) {
            return None;
        }
        let symbol = &self.symbols[class];
        let Binding::Definition(definition, _) = symbol.binding else {
            return None;
        };
        let scope = symbol.scope;
        let at = definition.name.at;
        let [base] = definition.bases.as_slice() else {
            return None;
        };
        match self.probe_identity(scope, base, at) {
            Value::Provider(provider) => framework_class_provider(&provider),
            Value::Callable(Invocation { target, .. }) if self.is_class(target) => {
                self.framework_class_role(target, seen, depth + 1)
            }
            _ => None,
        }
    }

    pub(super) fn register_celery_task(
        &mut self,
        scope: ScopeId,
        object: Object,
        value: Value,
        at: Location,
    ) {
        let task = match value {
            Value::Instance(class) => Some(class),
            Value::Callable(invocation) if self.is_class(invocation.target) => {
                self.constructor(scope, invocation.target, "constructor", "certain", at);
                Some(invocation.target)
            }
            _ => None,
        };
        if let Some(task) = task
            && self.framework_class_role(task, &mut BTreeSet::new(), 0)
                == Some(FrameworkClassRole::CeleryTask)
        {
            self.scopes[scope]
                .routes
                .push((object, task, "celery_task"));
            // Celery dispatches the task's effective run method, including an inherited one.
            if let Some(Value::Callable(run)) = self.class_member(task, "run", 0) {
                self.scopes[scope]
                    .routes
                    .push((object, run.target, "celery_task"));
                self.framework_route_receivers
                    .entry((scope, object, run.target))
                    .or_default()
                    .insert(task);
                return;
            }
        }
        self.scope_issue(
            scope,
            at,
            "unresolved_registration",
            "Celery task registration could not resolve an effective run method",
        );
    }

    pub(super) fn register_sqlmodel_table(&mut self, scope: ScopeId, model: SymbolId) {
        let (lineage, _) = self.class_lineage(model);
        let mut certain = self.branch_depth == 0;
        for (index, &class_scope) in lineage.iter().enumerate() {
            let owner = self.scopes[class_scope]
                .owner
                .expect("class scope has an owner");
            let Binding::Definition(definition, _) = self.symbols[owner].binding else {
                unreachable!()
            };
            // A subclass of an existing table does not create another table
            // through SQLModel's metaclass.
            if index > 0 && definition.table_keyword == Some(true) {
                return;
            }
            if self.local(class_scope, "model_config", END).is_some()
                || self.local(class_scope, "__abstract__", END).is_some()
            {
                certain = false;
            }
        }
        self.edge(
            scope,
            model,
            "sqlmodel_table",
            if certain { "certain" } else { "possible" },
        );
    }

    fn collect_django_urlconfs(&mut self) {
        for scope in 0..self.scopes.len() {
            let startups = std::mem::take(&mut self.scopes[scope].starts_django);
            if !startups.is_empty() {
                self.django_startup_scopes.insert(scope);
            }
            for startup in startups {
                let Some(settings) = startup
                    .settings
                    .as_deref()
                    .and_then(|module| self.modules.get(module).copied())
                else {
                    self.scope_issue(
                        scope,
                        startup.at,
                        "unresolved_registration",
                        "Django startup settings could not be resolved",
                    );
                    continue;
                };
                self.activate_django_settings(scope, settings, &startup.commands);
                if startup.creates_application
                    && scope == self.scopes[scope].file
                    && matches!(
                        self.files[scope].module.rsplit('.').next(),
                        Some("wsgi" | "asgi")
                    )
                    && let Some(application) = self.local(scope, "application", END)
                    && !self.conditional.contains_key(&application)
                    && matches!(self.symbols[application].binding,
                        Binding::Variable(Some(value), _) if position(value.at) == position(startup.at))
                {
                    self.edge(scope, application, "django_application", "certain");
                }
            }
        }

        if self.django_startup_scopes.is_empty() {
            self.select_configured_django_settings();
        }
        self.collect_django_root_patterns();
    }

    pub(super) fn activate_configured_django_after_dormant_startup(
        &mut self,
        reach: &Reachability,
    ) -> bool {
        if self.django_startup_scopes.is_empty()
            || !self.django_startup_scopes.is_disjoint(&reach.scopes)
            || !self.select_configured_django_settings()
        {
            return false;
        }
        self.collect_django_root_patterns();
        true
    }

    fn select_configured_django_settings(&mut self) -> bool {
        // A deployment can choose settings outside the checked-in launchers.
        // Prefer any reachable startup or environment selection over convention.
        if self.initial_django_settings != DjangoSettings::Unset {
            return false;
        }
        let Some(settings) = self.configured_django_settings() else {
            return false;
        };
        self.django_configured_settings = Some(settings);
        self.django_startup_scopes.insert(settings);
        self.activate_django_settings(settings, settings, &DjangoCommands::None);
        true
    }

    fn collect_django_root_patterns(&mut self) {
        let mut pending: Vec<_> = self.django_roots.iter().copied().collect();
        let mut visited = BTreeSet::new();
        while let Some(object) = pending.pop() {
            if !visited.insert(object) {
                continue;
            }
            let file = object.0;
            if self.failed_terminal_modules.contains(&file) {
                continue;
            }
            if let Some(patterns) = self.local(file, "urlpatterns", END) {
                let value = self.django_effective_url_patterns(file, patterns);
                self.collect_django_patterns(file, object, value, self.symbols[patterns].name.at);
                pending.extend(
                    self.scopes[file]
                        .mounts
                        .iter()
                        .filter(|(parent, _)| *parent == object)
                        .map(|(_, child)| *child),
                );
            } else if !self.django_closed_url_table_missing(file) {
                self.django_registration_issue(
                    object,
                    Location { line: 1, column: 1 },
                    "selected Django URL module has no resolved urlpatterns",
                );
            }
        }

        for object in self.django_roots.clone() {
            let file = object.0;
            if self.failed_terminal_modules.contains(&file) {
                continue;
            }
            for name in ["handler400", "handler403", "handler404", "handler500"] {
                if let Some(binding) = self.lexical(file, name, END) {
                    let value = match self.binding(binding, END, 0) {
                        Value::Text(path) => path
                            .rsplit_once('.')
                            .and_then(|(module, name)| {
                                self.modules.get(module).map(|file| (*file, name))
                            })
                            .and_then(|(file, name)| self.local(file, name, END))
                            .map_or(Value::Unknown, |id| self.binding(id, END, 0)),
                        value => value,
                    };
                    if let Value::Callable(invocation) = value
                        && !self.is_class(invocation.target)
                    {
                        self.scopes[file].routes.push((
                            object,
                            invocation.target,
                            "django_registration",
                        ));
                    } else {
                        self.django_registration_issue(
                            object,
                            self.symbols[binding].name.at,
                            "Django error handler callback could not be resolved",
                        );
                    }
                }
            }
        }
    }

    fn configured_django_settings(&mut self) -> Option<usize> {
        if self.modules.contains_key("django") || self.modules.contains_key("django.urls") {
            return None;
        }
        let mut selected = None;
        for file in 0..self.files.len() {
            let Some((package, "settings")) = self.files[file].module.rsplit_once('.') else {
                continue;
            };
            let [Some(apps), Some(root)] =
                ["INSTALLED_APPS", "ROOT_URLCONF"].map(|name| self.local(file, name, END))
            else {
                continue;
            };
            if [apps, root]
                .into_iter()
                .any(|binding| self.conditional.contains_key(&binding))
                || !matches!(self.binding_value(apps, 0), Value::Sequence(_))
                || !matches!(self.binding_value(root, 0), Value::Text(module)
                    if module == format!("{package}.urls"))
            {
                continue;
            }
            let Some(&urls) = self.modules.get(format!("{package}.urls").as_str()) else {
                continue;
            };
            let Some(patterns) = self.local(urls, "urlpatterns", END) else {
                continue;
            };
            if self.conditional.contains_key(&patterns)
                || !matches!(self.binding_value(patterns, 0), Value::Sequence(_))
                || !self.scopes[urls].names.values()
                    .flat_map(|bindings| self.effective_bindings(bindings))
                    .any(|&bound| {
                        !self.conditional.contains_key(&bound)
                            && matches!(self.symbols[bound].binding, Binding::Import(import)
                                if import.level == 0 && import.module == "django.urls"
                                    && matches!(import.member.as_deref(), Some("path" | "re_path" | "include")))
                    })
            {
                continue;
            }
            if selected.replace(file).is_some() {
                return None;
            }
        }
        selected
    }

    fn activate_django_settings(
        &mut self,
        scope: ScopeId,
        settings: usize,
        commands: &DjangoCommands,
    ) {
        self.django_settings_sources.insert((scope, settings));
        let row = &mut self.scopes[scope];
        row.activated_apps.extend([
            django_loaded_module(settings),
            django_loaded_module(row.file),
        ]);
        let settings_bindings: Vec<_> = self.scopes[settings]
            .names
            .iter()
            .filter(|(name, _)| {
                name.chars().any(char::is_uppercase) && !name.chars().any(char::is_lowercase)
            })
            .flat_map(|(_, bindings)| self.effective_bindings(bindings).iter().copied())
            .filter(|&binding| matches!(self.symbols[binding].binding, Binding::Variable(..)))
            .collect();
        for binding in settings_bindings {
            self.edge(scope, binding, "django_setting", "certain");
        }
        self.collect_django_apps(scope, settings, commands);
        if let Some(binding) = self.local(settings, "ROOT_URLCONF", END) {
            if let Value::Text(module) = self.binding(binding, END, 0)
                && let Some(&file) = self.modules.get(module.as_str())
            {
                let object = framework_module(file);
                self.scopes[scope].activated_apps.push(object);
                self.django_roots.insert(object);
            } else {
                self.django_registration_issue(
                    django_loaded_module(settings),
                    self.symbols[binding].name.at,
                    "Django ROOT_URLCONF could not be resolved",
                );
            }
        }
    }

    pub(super) fn report_django_startup_coverage(&mut self, reach: &Reachability) {
        let no_startup = self.django_startup_scopes.is_disjoint(&reach.scopes);
        if no_startup {
            for file in 0..self.files.len() {
                if let Some(patterns) = self.local(file, "urlpatterns", END)
                    && has_django_pattern(&self.binding_value(patterns, 0))
                {
                    self.issue(
                        file,
                        self.symbols[patterns].name.at,
                        "unresolved_registration",
                        "Django URL patterns have no selected startup and ROOT_URLCONF",
                    );
                }
            }
        }
        for &scope in &reach.scopes {
            let row = &self.scopes[scope];
            let loaded = reach
                .active_objects
                .contains(&django_loaded_module(row.file))
                || reach.django_signal_modules.contains(&row.file);
            let first_failure = self.first_terminal_signal_import(scope);
            for registration in row
                .django_signals
                .iter()
                .filter(|registration| registration.precedes(first_failure))
            {
                let issue = if no_startup {
                    Some("Django signal registration has no selected startup")
                } else if loaded {
                    registration.issue
                } else {
                    None
                };
                if let Some(message) = issue {
                    self.issues.push(
                        Issue::new(
                            "unresolved_registration",
                            &self.files[row.file].path,
                            message,
                        )
                        .at(registration.at),
                    );
                }
            }
        }
    }

    fn first_terminal_signal_import(&self, scope: ScopeId) -> Option<[usize; 2]> {
        if self.failed_terminal_modules.is_empty() {
            return None;
        }
        // These import edges carry source locations and are unconditional.
        // The initialization cycle check has ruled out partial reentry.
        self.scopes[scope]
            .initializer_imports
            .iter()
            .filter(|(_, target)| self.failed_terminal_modules.contains(target))
            .map(|(import, _)| position(self.symbols[*import].name.at))
            .min()
    }

    pub(super) fn activate_django_signals(
        &self,
        reach: &mut Reachability,
        pending: &mut VecDeque<Edge>,
        verification: bool,
        possible: bool,
    ) {
        if self.django_startup_scopes.is_disjoint(&reach.scopes) {
            return;
        }
        for &scope in &reach.scopes {
            let row = &self.scopes[scope];
            let loaded_module = django_loaded_module(row.file);
            if reach.active_objects.contains(&framework_module(row.file)) {
                reach.active_objects.insert(loaded_module);
            }
            let starts_django = self.django_startup_scopes.contains(&scope)
                && self.django_configured_settings != Some(scope)
                && row.activated_apps.contains(&loaded_module);
            // An entry point may execute startup in an imported module or a
            // helper. Its other imports can then register receivers too.
            if starts_django
                || row
                    .imports
                    .iter()
                    .copied()
                    .chain(self.synchronous_calls(scope, verification))
                    .chain(row.class_bodies.iter().copied())
                    .any(|target| reach.django_setup_scopes.contains(&target))
            {
                reach.django_setup_scopes.insert(scope);
                if !starts_django {
                    reach.django_signal_modules.insert(row.file);
                }
            }

            let loaded = reach.active_objects.contains(&loaded_module);
            let startup_import = reach.django_signal_modules.contains(&row.file);
            if loaded {
                reach
                    .active_objects
                    .extend(row.imports.iter().copied().map(django_loaded_module));
            } else if startup_import {
                reach
                    .django_signal_modules
                    .extend(row.imports.iter().copied());
            } else if !possible || self.django_configured_settings.is_none() {
                continue;
            }
            let first_failure = self.first_terminal_signal_import(scope);
            for callback in row
                .django_signals
                .iter()
                .filter(|registration| registration.precedes(first_failure))
                .flat_map(|registration| &registration.callbacks)
            {
                let mut edge = callback.edge;
                let certain = loaded && edge.certainty == "certain";
                if !possible && !certain {
                    continue;
                }
                if possible {
                    edge.certainty = "possible";
                    if !loaded && !startup_import {
                        edge.kind = "django_signal_unknown_loading";
                    }
                }
                // A receiver can change method dispatch only after its
                // registration scope and module are both reached.
                if let Some(class) = callback.receiver {
                    reach
                        .deferred_method_receivers
                        .entry(edge.target)
                        .or_default()
                        .insert(class);
                }
                if !reach
                    .evidence
                    .contains(&(edge.target, edge.kind, edge.certainty))
                    && !(possible
                        && reach
                            .evidence
                            .contains(&(edge.target, "django_signal", "certain")))
                {
                    pending.push_back(edge);
                }
            }
        }
    }

    pub(super) fn has_reachable_django_signals(&self, reach: &Reachability) -> bool {
        !self.django_startup_scopes.is_disjoint(&reach.scopes)
            && reach.scopes.iter().any(|&scope| {
                let first_failure = self.first_terminal_signal_import(scope);
                self.scopes[scope]
                    .django_signals
                    .iter()
                    .any(|registration| {
                        registration.precedes(first_failure) && !registration.callbacks.is_empty()
                    })
            })
    }

    pub(super) fn resolve_deferred_receiver_calls(
        &mut self,
        reach: &mut Reachability,
        pending: &mut VecDeque<Edge>,
    ) {
        if reach.deferred_method_receivers.is_empty() {
            return;
        }
        for index in 0..self.receiver_calls.len() {
            let call = &self.receiver_calls[index];
            if !reach.scopes.contains(&call.scope) {
                continue;
            }
            let Some(owner) = self.scopes[self.symbols[call.parameter].scope].owner else {
                continue;
            };
            let Some(classes) = reach.deferred_method_receivers.get(&owner).cloned() else {
                continue;
            };
            let call = call.clone();
            for class in classes {
                if self.resolved_receiver_calls.contains(&(index, class))
                    || !reach
                        .resolved_deferred_receiver_calls
                        .insert((index, class))
                {
                    continue;
                }
                if let Some(Value::Callable(invocation)) = self.class_member(class, &call.member, 0)
                    && !self.receiver_conflicts(invocation.bind_receiver(Receiver::Bound(class)))
                {
                    if self.receiver_parameter(invocation.target).is_some() {
                        reach
                            .deferred_method_receivers
                            .entry(invocation.target)
                            .or_default()
                            .insert(class);
                    }
                    pending.push_back(Edge {
                        target: invocation.target,
                        kind: "direct_call",
                        certainty: "possible",
                    });
                } else {
                    self.scope_issue(
                        call.scope,
                        call.at,
                        "unresolved_call",
                        "receiver method dispatch is outside the bounded class hierarchy",
                    );
                }
            }
        }
    }

    fn collect_django_apps(&mut self, scope: ScopeId, settings: usize, commands: &DjangoCommands) {
        let Some(binding) = self.local(settings, "INSTALLED_APPS", END) else {
            return;
        };
        let at = self.symbols[binding].name.at;
        let object = django_loaded_module(settings);
        let Value::Sequence(apps) = self.binding(binding, END, 0) else {
            self.django_registration_issue(
                object,
                at,
                "Django INSTALLED_APPS is not a resolved sequence",
            );
            return;
        };
        let external_django = !self.modules.contains_key("django");
        let builtin_admin = |entry: &str| {
            matches!(
                entry,
                "django.contrib.admin"
                    | "django.contrib.admin.apps.AdminConfig"
                    | "django.contrib.admin.apps.SimpleAdminConfig"
            ) && external_django
        };
        let autodiscover_admin = apps.iter().any(|app| {
            matches!(app, Value::Text(entry) if builtin_admin(entry)
                && entry != "django.contrib.admin.apps.SimpleAdminConfig")
        });
        let mut registered_commands = BTreeMap::new();
        for app in apps {
            let resolved = if let Value::Text(entry) = app {
                if builtin_admin(&entry) {
                    continue;
                }
                self.django_app_config(scope, object, &entry, at)
            } else {
                None
            };
            let Some((app, label)) = resolved else {
                self.django_registration_issue(
                    object,
                    at,
                    "Django INSTALLED_APPS entry could not be resolved in selected sources",
                );
                continue;
            };
            self.django_template_apps
                .entry(settings)
                .or_default()
                .insert(app, label);
            let prefix = format!("{}.", self.files[app].module);
            for module in [
                Some(app),
                self.modules
                    .get(format!("{prefix}models").as_str())
                    .copied(),
                if autodiscover_admin {
                    self.modules.get(format!("{prefix}admin").as_str()).copied()
                } else {
                    None
                },
            ]
            .into_iter()
            .flatten()
            {
                self.scopes[scope]
                    .activated_apps
                    .push(django_loaded_module(module));
            }
            if matches!(commands, DjangoCommands::None) {
                continue;
            }
            let prefix = format!("{prefix}management.commands.");
            for (module, &file) in self.modules.range(prefix.as_str()..) {
                let Some(name) = module.strip_prefix(&prefix) else {
                    break;
                };
                if !name.contains('.') && !name.starts_with('_') && !self.files[file].is_package {
                    // Earlier installed apps override later commands with the same name.
                    registered_commands.entry(name.to_owned()).or_insert(file);
                }
            }
        }
        for (name, file) in registered_commands {
            if matches!(commands, DjangoCommands::Selected(command) if *command != name) {
                continue;
            }
            let module = &self.files[file].module;
            let command = match self.module_member_write(module, "Command", (file, END)) {
                Some(mutations::MemberWriteValue::Replacement(Value::Callable(invocation)))
                    if self.is_class(invocation.target)
                        && self.framework_class_role(
                            invocation.target,
                            &mut BTreeSet::new(),
                            0,
                        ) == Some(FrameworkClassRole::DjangoCommand) =>
                {
                    Some(CallableBinding {
                        target: invocation.target,
                        bindings: Vec::new(),
                    })
                }
                Some(_) => None,
                None => self.local(file, "Command", END).and_then(|binding| {
                    if self.conditional.contains_key(&binding) {
                        return None;
                    }
                    self.django_class_binding(binding, FrameworkClassRole::DjangoCommand)
                }),
            };
            self.scopes[scope]
                .activated_apps
                .push(django_loaded_module(file));
            if self.failed_terminal_modules.contains(&file) {
                self.django_registration_issue(
                    object,
                    at,
                    "Django command module cannot finish initialization",
                );
                continue;
            }
            if let Some(command) = command {
                self.read.extend(command.bindings);
                self.activate_django_command(scope, file, object, command.target, at);
            } else {
                self.django_registration_issue(
                    object,
                    at,
                    "Django command module has no resolved BaseCommand subclass named Command",
                );
            }
        }
    }

    fn activate_django_command(
        &mut self,
        launcher: ScopeId,
        module: ScopeId,
        object: Object,
        class: SymbolId,
        at: Location,
    ) {
        let kind = "django_command";
        self.scopes[launcher].routes.push((object, class, kind));
        // Django constructs the exported class before dispatch. The default
        // run_from_argv path creates the parser before invoking execute.
        for (name, positional) in [
            ("__new__", Some(1)),
            ("__init__", Some(1)),
            ("__getattribute__", Some(2)),
            ("run_from_argv", Some(2)),
            ("create_parser", Some(3)),
            ("add_arguments", Some(2)),
            ("execute", None),
            ("handle", None),
        ] {
            let Some(value) = self.django_class_value(class, name, module) else {
                continue;
            };
            let unknown_dispatch = matches!(
                name,
                "__new__" | "__getattribute__" | "run_from_argv" | "create_parser" | "execute"
            );
            if let Value::Callable(invocation) = value
                && self.bounded_framework_function(invocation.target)
                && let Binding::Definition(definition, _) = self.symbols[invocation.target].binding
            {
                if positional.is_some_and(|count| {
                    definition.required_positional_parameter_count > count
                        || definition.positional_parameter_count < count
                            && !definition.has_variadic_positional_parameter
                        || !definition.required_keyword_only_parameters.is_empty()
                }) {
                    break;
                }
                if name == "__new__" {
                    self.scopes[launcher]
                        .routes
                        .push((object, invocation.target, kind));
                } else {
                    self.record_django_hook(launcher, object, class, invocation, kind);
                }
                let body = body_without_docstring(definition);
                let stop = body
                    .iter()
                    .find(|stmt| {
                        matches!(
                            stmt.kind,
                            StatementKind::Raise(_)
                                | StatementKind::Return(_)
                                | StatementKind::If { .. }
                                | StatementKind::With { .. }
                        )
                    })
                    .is_some_and(|stmt| matches!(stmt.kind, StatementKind::Raise(_)));
                if stop {
                    break;
                }
                if unknown_dispatch {
                    // A closed no-op cannot delegate to the next stage.
                    if !body.is_empty() {
                        self.django_registration_issue(
                            object,
                            at,
                            "Django command override may change subsequent dispatch",
                        );
                    }
                    break;
                }
            } else if matches!(
                value,
                Value::ConstantFunction | Value::NullaryConstantFunction
            ) {
                if matches!(name, "__init__" | "add_arguments")
                    && matches!(value, Value::ConstantFunction)
                {
                    self.django_registration_issue(
                        object,
                        at,
                        "Django command stage has an unresolved lambda signature",
                    );
                }
                break;
            } else {
                if !value.is_builtin_scalar()
                    && !matches!(
                        value,
                        Value::None | Value::Integer(_) | Value::MissingFrameworkMember
                    )
                {
                    self.django_registration_issue(
                        object,
                        at,
                        "Django command callback could not be resolved",
                    );
                }
                break;
            }
        }
    }

    pub(super) fn django_admin_site_register(
        &mut self,
        scope: ScopeId,
        arguments: &[Expression],
        keywords: &[(String, Expression)],
        before: Location,
        at: Location,
    ) {
        let options =
            registration_arguments(arguments, keywords, ["model_or_iterable", "admin_class"]);
        let object = django_loaded_module(self.scopes[scope].file);
        match options {
            Some([Some(models), Some(admin)]) => {
                let models = self.identity(scope, models, before, 0);
                let mut local_models = BTreeSet::new();
                if !self.django_admin_models(&models, &mut local_models) {
                    self.django_registration_issue(
                        object,
                        at,
                        "Django admin model could not be resolved",
                    );
                    return;
                }
                if let Value::Callable(admin) = self.identity(scope, admin, before, 0) {
                    self.register_django_admin(scope, admin.target, &local_models, at);
                } else {
                    self.django_registration_issue(
                        object,
                        at,
                        "Django ModelAdmin could not be resolved",
                    );
                }
            }
            Some([Some(_), None]) => {}
            _ => self.django_registration_issue(
                object,
                at,
                "Django admin registration arguments could not be resolved",
            ),
        }
    }

    pub(super) fn django_admin_decorator(
        &mut self,
        scope: ScopeId,
        target: SymbolId,
        arguments: &[Expression],
        keywords: &[(String, Expression)],
        before: Location,
        at: Location,
    ) {
        let default_site = match keywords {
            [] => true,
            [(name, value)] if name == "site" => {
                matches!(self.identity(scope, value, before, 0), Value::Provider(provider) if provider == "django.contrib.admin.site")
            }
            _ => false,
        };
        if arguments.is_empty() || !default_site {
            self.django_registration_issue(
                django_loaded_module(self.scopes[scope].file),
                at,
                "Django admin decorator arguments could not be resolved",
            );
            return;
        }
        // The decorator consumes the class even without a known admin loader.
        self.edge(scope, target, "reference", "certain");
        let mut local_models = BTreeSet::new();
        if !arguments.iter().all(|model| {
            let model = self.identity(scope, model, before, 0);
            !matches!(model, Value::Sequence(_))
                && self.django_admin_models(&model, &mut local_models)
        }) || matches!(self.symbols[target].binding, Binding::Definition(definition, _) if definition.decorators.len() > 1)
        {
            self.django_registration_issue(
                django_loaded_module(self.scopes[scope].file),
                at,
                "Django admin models or decorated class could not be resolved",
            );
            return;
        }
        self.register_django_admin(scope, target, &local_models, at);
    }

    fn django_admin_models(&mut self, models: &Value, local: &mut BTreeSet<SymbolId>) -> bool {
        match models {
            Value::Callable(model)
                if self.is_class(model.target)
                    && self.framework_class_role(model.target, &mut BTreeSet::new(), 0)
                        == Some(FrameworkClassRole::DjangoModel) =>
            {
                local.insert(model.target)
            }
            Value::Sequence(models) if !models.is_empty() => models.iter().all(|model| {
                !matches!(model, Value::Sequence(_)) && self.django_admin_models(model, local)
            }),
            Value::Provider(name) => matches!(
                name.as_str(),
                "django.contrib.auth.models.User" | "django.contrib.auth.models.Group"
            ),
            _ => false,
        }
    }

    fn register_django_admin(
        &mut self,
        scope: ScopeId,
        target: SymbolId,
        local_models: &BTreeSet<SymbolId>,
        at: Location,
    ) {
        let object = django_loaded_module(self.scopes[scope].file);
        if self.branch_depth > 0 {
            self.django_registration_issue(
                object,
                at,
                "Django ModelAdmin registration is conditional",
            );
            return;
        }
        if !self.is_class(target)
            || self.framework_class_role(target, &mut BTreeSet::new(), 0)
                != Some(FrameworkClassRole::DjangoAdmin)
        {
            self.django_registration_issue(object, at, "Django ModelAdmin could not be resolved");
            return;
        }
        if local_models.iter().any(|&model| {
            self.registered_django_admin_models
                .contains(&(scope, model))
        }) {
            self.django_registration_issue(
                object,
                at,
                "A Django model is already registered on the default admin site in this scope",
            );
            return;
        }
        self.registered_django_admin_models
            .extend(local_models.iter().map(|&model| (scope, model)));

        self.scopes[scope]
            .routes
            .push((object, target, "django_admin_registration"));
        for name in ["get_queryset", "save_model", "delete_model"] {
            self.django_admin_method(scope, object, target, name, at);
        }
        for (name, getter) in [
            ("list_display", "get_list_display"),
            ("readonly_fields", "get_readonly_fields"),
            ("actions", "get_actions"),
        ] {
            let option = if let Some((override_value, getter_at)) =
                self.django_admin_option(scope, target, getter, at)
            {
                self.django_admin_method(scope, object, target, getter, at);
                let choices = if let Value::Callable(getter) = override_value
                    && let Binding::Definition(definition, body) =
                        self.symbols[getter.target].binding
                    && definition.kind == DefinitionKind::Function
                    && !definition.is_generator
                    && definition.decorators.is_empty()
                    && let [
                        Statement {
                            kind: StatementKind::Return(Some(expression)),
                            at,
                        },
                    ] = body_without_docstring(definition)
                    && matches!(expression.kind, ExpressionKind::Sequence { .. })
                    && name != "actions"
                {
                    self.identity(body, expression, *at, 0)
                } else {
                    Value::Unknown
                };
                Some((choices, getter_at))
            } else {
                self.django_admin_option(scope, target, name, at)
            };
            let Some((choices, option_at)) = option else {
                continue;
            };
            let Value::Sequence(choices) = choices else {
                self.django_registration_issue(
                    object,
                    option_at,
                    "Django ModelAdmin callback options could not be resolved",
                );
                continue;
            };
            for choice in choices {
                match choice {
                    Value::Text(callback) => {
                        if name != "actions" && callback == "id" {
                            // An automatic or custom primary key can determine whether Django
                            // reads a field or falls back to a callable with this spelling.
                            if self
                                .django_admin_option(scope, target, &callback, at)
                                .is_some()
                                || local_models.iter().any(|&model| {
                                    matches!(
                                        self.django_admin_option(scope, model, &callback, at),
                                        Some((Value::Callable(_), _))
                                    )
                                })
                            {
                                self.django_registration_issue(object, at, "Django admin id field and callback precedence could not be resolved");
                            }
                            continue;
                        }
                        let model_only = name != "actions" && callback == "__str__";
                        let admin_attribute = !model_only
                            && self.django_admin_method(scope, object, target, &callback, at);
                        if !admin_attribute && name != "actions" {
                            for &model in local_models {
                                self.django_admin_method(scope, object, model, &callback, at);
                            }
                        }
                    }
                    Value::Callable(callback) => self.scopes[scope].routes.push((
                        object,
                        callback.target,
                        "django_admin_registration",
                    )),
                    _ => self.django_registration_issue(
                        object,
                        at,
                        "Django ModelAdmin callback option is outside the bounded model",
                    ),
                }
            }
        }
    }

    fn django_admin_method(
        &mut self,
        scope: ScopeId,
        object: Object,
        class: SymbolId,
        name: &str,
        at: Location,
    ) -> bool {
        let Some((attribute, option_at)) = self.django_admin_option(scope, class, name, at) else {
            return false;
        };
        match attribute {
            Value::Callable(callback) if !self.is_class(callback.target) => {
                self.scopes[scope].routes.push((
                    object,
                    callback.target,
                    "django_admin_registration",
                ));
                self.framework_route_receivers
                    .entry((scope, object, callback.target))
                    .or_default()
                    .insert(class);
            }
            Value::Unknown | Value::Alternatives(_) => self.django_registration_issue(
                object,
                option_at,
                "Django ModelAdmin method could not be resolved",
            ),
            _ => {}
        }
        true
    }

    fn django_admin_option(
        &mut self,
        scope: ScopeId,
        class: SymbolId,
        name: &str,
        at: Location,
    ) -> Option<(Value, Location)> {
        use mutations::MemberWriteValue;

        let (lineage, _) = self.class_lineage(class);
        for owner in lineage {
            if let Some(class) = self.scopes[owner].owner {
                match self.class_member_write(class, name, (scope, END)) {
                    Some(MemberWriteValue::Replacement(value)) => return Some((value, at)),
                    Some(MemberWriteValue::Deleted) => continue,
                    Some(MemberWriteValue::Ambiguous) => return Some((Value::Unknown, at)),
                    None => {}
                }
            }
            if let Some(binding) = self.local(owner, name, END) {
                self.read.insert(binding);
                return Some((self.binding(binding, END, 0), self.symbols[binding].name.at));
            }
        }
        None
    }

    fn django_app_config(
        &mut self,
        scope: ScopeId,
        object: Object,
        entry: &str,
        at: Location,
    ) -> Option<(usize, Option<String>)> {
        let (file, config) = if let Some(&app) = self.modules.get(entry) {
            let Some(&file) = self.modules.get(format!("{entry}.apps").as_str()) else {
                return Some((app, Some(django_default_app_label(entry).to_owned())));
            };
            self.scopes[scope]
                .activated_apps
                .push(django_loaded_module(file));
            let candidates: Vec<_> = self.scopes[file]
                .names
                .values()
                .flat_map(|bindings| self.effective_bindings(bindings).iter().copied())
                .collect();
            let mut configs = Vec::new();
            for binding in candidates {
                let Some(config) =
                    self.django_class_binding(binding, FrameworkClassRole::DjangoAppConfig)
                else {
                    continue;
                };
                if self.conditional.contains_key(&binding) {
                    return None;
                }
                let default = match self.django_class_value(config.target, "default", scope) {
                    None => false,
                    Some(Value::Bool(false)) => continue,
                    Some(Value::Bool(true)) => true,
                    Some(_) => return None,
                };
                configs.push((config, default));
            }
            let config = if configs.len() == 1 {
                configs.pop()?.0
            } else {
                let mut defaults = configs.into_iter().filter(|(_, default)| *default);
                match (defaults.next(), defaults.next()) {
                    (Some((config, _)), None) => config,
                    (None, _) => {
                        return Some((app, Some(django_default_app_label(entry).to_owned())));
                    }
                    _ => return None,
                }
            };
            (file, config)
        } else {
            let (module, name) = entry.rsplit_once('.')?;
            let file = *self.modules.get(module)?;
            let binding = self.local(file, name, END)?;
            if self.conditional.contains_key(&binding) {
                return None;
            }
            (
                file,
                self.django_class_binding(binding, FrameworkClassRole::DjangoAppConfig)?,
            )
        };
        let class = config.target;
        let Value::Text(name) = self.django_class_value(class, "name", scope)? else {
            return None;
        };
        let app = *self.modules.get(name.as_str())?;
        let default_path = [
            "path",
            "__init__",
            "__new__",
            "__getattribute__",
            "__getattr__",
        ]
        .iter()
        .all(|name| self.django_class_value(class, name, scope).is_none());
        let label = if default_path {
            match self.django_class_value(class, "label", scope) {
                None => Some(django_default_app_label(&name).to_owned()),
                Some(Value::Text(label)) if skarmory_python::simple_name(&label) => Some(label),
                _ => None,
            }
        } else {
            None
        };
        self.read.extend(config.bindings);
        self.scopes[scope]
            .activated_apps
            .push(django_loaded_module(file));
        self.activate_django_class(
            scope,
            object,
            class,
            &["__init__", "ready"],
            "django_app_config",
            at,
        );
        Some((app, label))
    }

    fn django_class_binding(
        &mut self,
        binding: SymbolId,
        role: FrameworkClassRole,
    ) -> Option<CallableBinding> {
        let config = self.probe_callable_binding(binding)?;
        let target = config.target;
        (self.is_class(target)
            && self.framework_class_role(target, &mut BTreeSet::new(), 0) == Some(role))
        .then_some(config)
    }

    fn django_class_value(&mut self, class: SymbolId, name: &str, scope: ScopeId) -> Option<Value> {
        let (lineage, _) = self.class_lineage(class);
        let reader = self.scopes[scope].file;
        for body in lineage {
            let owner = self.scopes[body].owner.expect("class body has an owner");
            match self.class_discovery_write(owner, name, reader) {
                Some(mutations::MemberWriteValue::Replacement(value)) => return Some(value),
                Some(mutations::MemberWriteValue::Ambiguous) => return Some(Value::Unknown),
                Some(mutations::MemberWriteValue::Deleted) => continue,
                None => {}
            }
            if let Some(binding) = self.local(body, name, END) {
                return Some(self.binding(binding, END, 0));
            }
        }
        None
    }

    fn activate_django_class(
        &mut self,
        scope: ScopeId,
        object: Object,
        class: SymbolId,
        hooks: &[&str],
        kind: &'static str,
        at: Location,
    ) {
        self.scopes[scope].routes.push((object, class, kind));
        for name in hooks {
            self.activate_django_hook(scope, object, class, name, kind, at);
        }
    }

    fn activate_django_hook(
        &mut self,
        scope: ScopeId,
        object: Object,
        class: SymbolId,
        name: &str,
        kind: &'static str,
        at: Location,
    ) {
        let Some(value) = self.django_class_value(class, name, scope) else {
            return;
        };
        if let Value::Callable(invocation) = value {
            self.record_django_hook(scope, object, class, invocation, kind);
        } else if !matches!(value, Value::ConstantFunction) {
            self.django_registration_issue(
                object,
                at,
                "Django lifecycle callback could not be resolved",
            );
        }
    }

    fn record_django_hook(
        &mut self,
        scope: ScopeId,
        object: Object,
        class: SymbolId,
        invocation: Invocation,
        kind: &'static str,
    ) {
        self.scopes[scope]
            .routes
            .push((object, invocation.target, kind));
        self.method_receivers
            .entry(invocation.target)
            .or_default()
            .insert(class);
    }

    pub(super) fn django_receiver(
        &mut self,
        scope: ScopeId,
        args: &[Expression],
        keywords: &[(String, Expression)],
        before: Location,
        depth: usize,
    ) -> DjangoReceiver {
        // receiver(signal, **kwargs) forwards these keyword arguments to connect().
        let Some([Some(signal), ..]) =
            registration_arguments(args, keywords, ["signal", "sender", "weak", "dispatch_uid"])
                .filter(|_| args.len() <= 1)
        else {
            return DjangoReceiver::Invalid;
        };
        let value = self.identity(scope, signal, before, depth + 1);
        self.django_signal_sources(&value, (scope, before))
    }

    pub(super) fn django_receiver_rejects_definition(
        &mut self,
        scope: ScopeId,
        decorator: &Expression,
        before: Location,
    ) -> bool {
        let ExpressionKind::Call {
            function,
            arguments,
            keywords,
        } = &decorator.kind
        else {
            return false;
        };
        let [signal] = arguments.as_slice() else {
            return false;
        };
        if !matches!(self.probe_identity(scope, function, before),
            Value::Provider(provider) if is_django_receiver(&provider))
            || keywords.iter().any(|(name, _)| name.is_empty())
        {
            return false;
        }
        // A duplicate signal fails at the receiver call itself. Other
        // arguments are forwarded to connect, which only has a known signature
        // when the actual signal member resolves to Django's implementation.
        if keywords.iter().any(|(name, _)| name == "signal") {
            return true;
        }
        if !keywords
            .iter()
            .any(|(name, _)| !matches!(name.as_str(), "sender" | "weak" | "dispatch_uid" | "apps"))
        {
            return false;
        }
        let value = self.probe_identity(scope, signal, before);
        self.django_signal_source(&value, (scope, before)) == DjangoReceiver::Certain
    }

    fn django_signal_sources(
        &mut self,
        value: &Value,
        access: (ScopeId, Location),
    ) -> DjangoReceiver {
        match value {
            Value::Sequence(values) if values.is_empty() => DjangoReceiver::Empty,
            Value::Sequence(values) => {
                let mut source = DjangoReceiver::Certain;
                let mut registers = false;
                for value in values {
                    match self.django_signal_source(value, access) {
                        DjangoReceiver::Empty => {}
                        DjangoReceiver::Certain => registers = true,
                        DjangoReceiver::Possible => {
                            registers = true;
                            source = DjangoReceiver::Possible;
                        }
                        _ => return DjangoReceiver::Unresolved,
                    }
                }
                if registers {
                    source
                } else {
                    DjangoReceiver::Empty
                }
            }
            Value::Alternatives(values) => {
                let mut sources = values
                    .iter()
                    .map(|value| self.django_signal_sources(value, access));
                let Some(first) = sources
                    .next()
                    .filter(|first| *first != DjangoReceiver::Unresolved)
                else {
                    return DjangoReceiver::Unresolved;
                };
                let mut source = first;
                for other in sources {
                    if other == DjangoReceiver::Unresolved {
                        return other;
                    }
                    if other != first {
                        source = DjangoReceiver::Possible;
                    }
                }
                source
            }
            _ => self.django_signal_source(value, access),
        }
    }

    fn django_signal_source(
        &mut self,
        value: &Value,
        access: (ScopeId, Location),
    ) -> DjangoReceiver {
        if !matches!(value, Value::DjangoSignal(_) | Value::Instance(_))
            && !matches!(value, Value::Provider(provider) if is_django_signal(provider))
        {
            return DjangoReceiver::Unresolved;
        }
        match self.member(value.clone(), "connect", access, 0) {
            Value::DjangoSignalConnect => DjangoReceiver::Certain,
            Value::Provider(provider)
                if provider
                    .strip_suffix(".connect")
                    .is_some_and(is_django_signal) =>
            {
                DjangoReceiver::Certain
            }
            Value::ConstantFunction => DjangoReceiver::Empty,
            _ => DjangoReceiver::Unresolved,
        }
    }

    pub(super) fn connect_django_signal(
        &mut self,
        scope: ScopeId,
        args: &[Expression],
        keywords: &[(String, Expression)],
        before: Location,
        at: Location,
    ) {
        let callback = registration_arguments(
            args,
            keywords,
            ["receiver", "sender", "weak", "dispatch_uid"],
        )
        .and_then(|[callback, ..]| callback);
        if let Some(callback) = callback
            && self.record_django_global_connect(
                scope,
                callback,
                before,
                at,
                args.len() + keywords.len() == 1,
            )
        {
            return;
        }
        let callback = callback
            .and_then(|callback| self.resolve_untracked_django_callback(scope, callback, before));
        self.record_django_signal(scope, DjangoReceiver::Certain, callback, at);
    }

    pub(super) fn record_django_signal(
        &mut self,
        scope: ScopeId,
        signal: DjangoReceiver,
        callback: Option<Value>,
        at: Location,
    ) {
        if signal == DjangoReceiver::Empty {
            return;
        }
        let conditional_scope = self.branch_depth != 0
            || self.scopes[scope]
                .owner
                .is_some_and(|owner| self.conditional.contains_key(&owner));
        let (candidates, conditional) = match callback {
            Some(Value::Alternatives(values)) => (values, true),
            Some(value) => (vec![value], conditional_scope),
            None => (Vec::new(), false),
        };
        let mut issue = match signal {
            DjangoReceiver::Invalid => {
                Some("Django receiver decorator arguments could not be resolved")
            }
            DjangoReceiver::Unresolved => Some("Django signal identity could not be resolved"),
            _ => None,
        };
        let mut callbacks = Vec::new();
        if signal != DjangoReceiver::Invalid {
            if candidates.is_empty() {
                issue = Some("Django signal callback arguments could not be resolved");
            }
            for value in candidates {
                let Value::Callable(invocation) = value else {
                    issue = Some("Django signal callback could not be resolved");
                    continue;
                };
                if self.is_class(invocation.target)
                    || matches!(invocation.receiver, Receiver::Explicit(_))
                {
                    issue = Some("Django signal callback could not be resolved");
                    continue;
                }
                let certainty = if signal == DjangoReceiver::Certain
                    && !conditional
                    && !self.conditional.contains_key(&invocation.target)
                    && !invocation.verified
                {
                    "certain"
                } else {
                    "possible"
                };
                callbacks.push(DjangoSignalCallback {
                    edge: Edge {
                        target: invocation.target,
                        kind: "django_signal",
                        certainty,
                    },
                    receiver: invocation.receiver.supplied(),
                });
            }
        }
        self.scopes[scope]
            .django_signals
            .push(DjangoSignalRegistration {
                at,
                callbacks,
                issue,
            });
    }

    fn collect_django_patterns(&mut self, file: usize, parent: Object, value: Value, at: Location) {
        match value {
            Value::Sequence(values) | Value::Alternatives(values) => {
                for value in values {
                    self.collect_django_patterns(file, parent, value, at);
                }
            }
            Value::DjangoPattern(callback) => match *callback {
                Value::Callable(invocation)
                    if self.django_constant_callback_code(file, invocation.target) => {}
                Value::Callable(invocation) if !self.is_class(invocation.target) => {
                    let kind = if self.is_generator(invocation.target) {
                        // Django receives the generator as the view result.
                        // Calling the registered function does not consume it.
                        "generator_creation"
                    } else {
                        "django_registration"
                    };
                    self.scopes[file]
                        .routes
                        .push((parent, invocation.target, kind));
                }
                Value::DjangoView(class) => {
                    self.scopes[file]
                        .routes
                        .push((parent, class, "django_registration"));
                    self.activate_django_hook(
                        file,
                        parent,
                        class,
                        "setup",
                        "framework_lifecycle",
                        at,
                    );
                }
                Value::DjangoInclude(child) => self.scopes[file].mounts.push((parent, child)),
                Value::ConstantFunction | Value::NullaryConstantFunction => {}
                _ => self.django_registration_issue(
                    parent,
                    at,
                    "Django URL callback could not be resolved",
                ),
            },
            _ => self.django_registration_issue(
                parent,
                at,
                "Django urlpatterns value is outside the bounded registration model",
            ),
        }
    }

    fn django_registration_issue(&mut self, object: Object, at: Location, message: &'static str) {
        self.django_registration_issues.push((
            object,
            Issue::new(
                "unresolved_registration",
                &self.files[object.0].path,
                message,
            )
            .at(at),
        ));
    }

    pub(super) fn phase03_framework_edges(&mut self) -> Vec<Edge> {
        std::mem::take(&mut self.framework_roots)
    }
}

fn has_django_pattern(value: &Value) -> bool {
    match value {
        Value::DjangoPattern(_) => true,
        Value::Sequence(values) | Value::Alternatives(values) => {
            values.iter().any(has_django_pattern)
        }
        _ => false,
    }
}

fn django_default_app_label(name: &str) -> &str {
    name.rsplit('.').next().expect("application module name")
}

fn literal_settings_entries(value: &Expression) -> impl Iterator<Item = (&str, &Expression)> {
    let items = match &value.kind {
        ExpressionKind::Dictionary(items) => items.as_slice(),
        _ => &[],
    };
    items
        .as_chunks::<2>()
        .0
        .iter()
        .filter_map(|[key, value]| match &key.kind {
            ExpressionKind::Text(name) => Some((name.as_str(), value)),
            _ => None,
        })
}
