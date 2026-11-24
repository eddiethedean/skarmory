//! Project-wide framework and test discovery for the Phase 0.3 profile.

use super::*;

pub(super) struct DjangoStartup {
    pub(super) at: Location,
    pub(super) settings: Option<String>,
    pub(super) requires_urlconf: bool,
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

struct DjangoSignalCallback {
    edge: Edge,
    receiver: Option<SymbolId>,
}

impl Graph<'_> {
    pub(super) fn collect_phase03_frameworks(&mut self) {
        self.collect_django_urlconfs();

        let classes: Vec<_> = (0..self.symbols.len())
            .filter(|&id| self.is_class(id) && !self.dormant_classes.contains(&id))
            .collect();
        for class in classes {
            let Some(role) = self.framework_class_role(class, &mut BTreeSet::new(), 0) else {
                continue;
            };
            let (root, lifecycle, fields) = match role {
                FrameworkClassRole::Unittest | FrameworkClassRole::UnittestAsync => {
                    (None, &[][..], false)
                }
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
                FrameworkClassRole::DjangoModel | FrameworkClassRole::DjangoSignal => {
                    (None, &[][..], false)
                }
                // A ModelAdmin subclass is live only after an actual admin.register
                // registration. Lifecycle entries are therefore owner-gated.
                FrameworkClassRole::DjangoAdmin => (
                    None,
                    &["get_queryset", "save_model", "delete_model"][..],
                    true,
                ),
                FrameworkClassRole::CeleryTask => (Some("celery_task"), &["run"][..], false),
                FrameworkClassRole::SqlAlchemyModel => (
                    Some("sqlalchemy_model"),
                    &["__declare_first__", "__declare_last__"][..],
                    true,
                ),
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
            if matches!(
                role,
                FrameworkClassRole::Unittest | FrameworkClassRole::UnittestAsync
            ) {
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
            let mut has_cases = false;
            for (class, certain) in self.pytest_registrations(&self.scopes[file].names, "") {
                let Some(role) = self.unittest_classes.get(&class.target) else {
                    continue;
                };
                let async_runner = *role == FrameworkClassRole::UnittestAsync;
                let (lineage, _) = self.class_lineage(class.target);
                let mut names = BTreeMap::new();
                for scope in lineage {
                    for (&name, bindings) in &self.scopes[scope].names {
                        names.entry(name).or_insert_with(|| bindings.clone());
                    }
                }
                let mut methods = self.pytest_registrations(&names, "test");
                if methods.is_empty()
                    && let Some(bindings) = names.get("runTest")
                {
                    methods.extend(self.pytest_registrations(
                        &BTreeMap::from([("runTest", bindings.clone())]),
                        "runTest",
                    ));
                }
                methods.retain(|(method, _)| self.is_pytest_function(method.target));
                if methods.is_empty() {
                    continue;
                }
                let certain_case = certain && methods.iter().any(|(_, selected)| *selected);
                has_cases = true;
                self.read.extend(class.bindings);
                self.framework_root(class.target, "unittest_discovery", certain);
                let lifecycle = [
                    ("setUp", false),
                    ("tearDown", false),
                    ("setUpClass", false),
                    ("tearDownClass", false),
                    ("asyncSetUp", true),
                    ("asyncTearDown", true),
                ];
                for (hook, asynchronous) in lifecycle {
                    if asynchronous && !async_runner {
                        continue;
                    }
                    if let Some(bindings) = names.get(hook) {
                        methods.extend(
                            self.pytest_registrations(
                                &BTreeMap::from([(hook, bindings.clone())]),
                                hook,
                            )
                            .into_iter()
                            .filter(|(method, _)| self.is_coroutine(method.target) == asynchronous),
                        );
                    }
                }
                if self.is_pytest_file(file) {
                    self.pytest_unittest_cases
                        .entry(file)
                        .or_default()
                        .push((names, certain_case));
                }
                for (method, selected) in methods {
                    if self.is_pytest_function(method.target)
                        && (async_runner || !self.is_coroutine(method.target))
                    {
                        self.read.extend(method.bindings);
                        self.method_receivers
                            .entry(method.target)
                            .or_default()
                            .insert(class.target);
                        self.framework_root(
                            method.target,
                            "unittest_discovery",
                            certain && selected,
                        );
                    }
                }
            }
            if has_cases {
                for name in ["setUpModule", "tearDownModule"] {
                    if let Some(target) = self.local(file, name, END)
                        && self.is_pytest_function(target)
                        && !self.is_coroutine(target)
                    {
                        self.framework_root(
                            target,
                            "unittest_discovery",
                            !self.conditional.contains_key(&target),
                        );
                    }
                }
            }
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
        for file in plugins {
            for (hook, certain) in self.pytest_registrations(&self.scopes[file].names, "pytest_") {
                if self.is_pytest_function(hook.target) {
                    self.read.extend(hook.bindings);
                    self.framework_root(hook.target, "pytest_hook", certain);
                }
            }
        }
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

    pub(super) fn register_protocol_class(
        &mut self,
        scope: ScopeId,
        value: Value,
        methods: &[&str],
        kind: &'static str,
        at: Location,
    ) {
        let Value::Callable(Invocation { target: class, .. }) = value else {
            self.scope_issue(
                scope,
                at,
                "unresolved_registration",
                "framework protocol class identity could not be resolved",
            );
            return;
        };
        if !self.is_class(class) {
            self.scope_issue(
                scope,
                at,
                "unresolved_registration",
                "framework protocol registration did not select a class",
            );
            return;
        }
        self.edge(scope, class, kind, "certain");
        let Binding::Definition(_, class_scope) = self.symbols[class].binding else {
            return;
        };
        for name in methods {
            if let Some(target) = self.local(class_scope, name, END)
                && matches!(self.symbols[target].binding, Binding::Definition(_, _))
            {
                self.framework_lifecycle.push((class, target, kind));
            }
        }
        for bindings in self.scopes[class_scope].names.values() {
            for &target in self.effective_bindings(bindings) {
                if self.symbols[target].name.text == "regex"
                    && matches!(self.symbols[target].binding, Binding::Variable(_, _))
                {
                    self.framework_fields.insert(target);
                }
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
                let row = &mut self.scopes[scope];
                row.activated_apps.extend([
                    django_loaded_module(settings),
                    django_loaded_module(row.file),
                ]);
                self.collect_django_apps(scope, settings, &startup.commands);
                let binding = self.local(settings, "ROOT_URLCONF", END);
                let root = binding.and_then(|binding| match self.binding(binding, END, 0) {
                    Value::Text(module) => self.modules.get(module.as_str()).copied(),
                    _ => None,
                });
                if let Some(file) = root {
                    let object = framework_module(file);
                    self.scopes[scope].activated_apps.push(object);
                    self.django_roots.insert(object);
                } else if startup.requires_urlconf || binding.is_some() {
                    self.scope_issue(
                        scope,
                        startup.at,
                        "unresolved_registration",
                        "Django ROOT_URLCONF could not be resolved",
                    );
                }
            }
        }

        let mut pending: Vec<_> = self.django_roots.iter().copied().collect();
        let mut visited = BTreeSet::new();
        while let Some(object) = pending.pop() {
            if !visited.insert(object) {
                continue;
            }
            let file = object.0;
            if let Some(patterns) = self.local(file, "urlpatterns", END) {
                let value = self.binding(patterns, END, 0);
                self.collect_django_patterns(file, object, value, self.symbols[patterns].name.at);
                pending.extend(
                    self.scopes[file]
                        .mounts
                        .iter()
                        .filter(|(parent, _)| *parent == object)
                        .map(|(_, child)| *child),
                );
            } else {
                self.django_registration_issue(
                    object,
                    Location { line: 1, column: 1 },
                    "selected Django URL module has no resolved urlpatterns",
                );
            }
        }

        for object in self.django_roots.clone() {
            let file = object.0;
            for name in ["handler400", "handler403", "handler404", "handler500"] {
                if let Some(binding) = self.lexical(file, name, END) {
                    let value = self.binding(binding, END, 0);
                    if let Value::Callable(invocation) = value {
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
            for registration in &row.django_signals {
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
            if loaded {
                reach
                    .active_objects
                    .extend(row.imports.iter().copied().map(django_loaded_module));
            } else if reach.django_signal_modules.contains(&row.file) {
                reach
                    .django_signal_modules
                    .extend(row.imports.iter().copied());
            } else {
                continue;
            }
            for callback in row
                .django_signals
                .iter()
                .flat_map(|registration| &registration.callbacks)
            {
                let mut edge = callback.edge;
                // A receiver can change method dispatch only after its
                // registration scope and module are both reached.
                if let Some(class) = callback.receiver {
                    reach
                        .deferred_method_receivers
                        .entry(edge.target)
                        .or_default()
                        .insert(class);
                }
                if possible || !loaded {
                    edge.certainty = "possible";
                }
                if !reach
                    .evidence
                    .contains(&(edge.target, edge.kind, edge.certainty))
                {
                    pending.push_back(edge);
                }
            }
        }
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
        let mut registered_commands = BTreeMap::new();
        for app in apps {
            let resolved = if let Value::Text(entry) = app {
                self.django_app_config(scope, object, &entry, at)
            } else {
                None
            };
            let Some(app) = resolved else {
                self.django_registration_issue(
                    object,
                    at,
                    "Django INSTALLED_APPS entry could not be resolved in selected sources",
                );
                continue;
            };
            let prefix = format!("{}.", self.files[app].module);
            for module in [
                Some(app),
                self.modules
                    .get(format!("{prefix}models").as_str())
                    .copied(),
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
            let command = self.local(file, "Command", END).and_then(|binding| {
                if self.conditional.contains_key(&binding) {
                    return None;
                }
                self.django_class_binding(binding, FrameworkClassRole::DjangoCommand)
            });
            if let Some(command) = command {
                self.read.extend(command.bindings);
                self.scopes[scope]
                    .activated_apps
                    .push(django_loaded_module(file));
                self.activate_django_class(
                    scope,
                    object,
                    command.target,
                    &["__init__", "add_arguments", "handle"],
                    "django_command",
                    at,
                );
            } else {
                self.django_registration_issue(
                    object,
                    at,
                    "Django command module has no resolved BaseCommand subclass named Command",
                );
            }
        }
    }

    fn django_app_config(
        &mut self,
        scope: ScopeId,
        object: Object,
        entry: &str,
        at: Location,
    ) -> Option<usize> {
        let (file, config) = if let Some(&app) = self.modules.get(entry) {
            let Some(&file) = self.modules.get(format!("{entry}.apps").as_str()) else {
                return Some(app);
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
                let default = match self.django_class_attribute(config.target, "default") {
                    None => false,
                    Some(binding) => match self.binding(binding, END, 0) {
                        Value::Bool(false) => continue,
                        Value::Bool(true) => true,
                        _ => return None,
                    },
                };
                configs.push((config, default));
            }
            let config = if configs.len() == 1 {
                configs.pop()?.0
            } else {
                let mut defaults = configs.into_iter().filter(|(_, default)| *default);
                match (defaults.next(), defaults.next()) {
                    (Some((config, _)), None) => config,
                    (None, _) => return Some(app),
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
        let binding = self.django_class_attribute(class, "name")?;
        let Value::Text(name) = self.binding(binding, END, 0) else {
            return None;
        };
        let app = *self.modules.get(name.as_str())?;
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
        Some(app)
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

    fn django_class_attribute(&mut self, class: SymbolId, name: &str) -> Option<SymbolId> {
        let (lineage, _) = self.class_lineage(class);
        lineage
            .into_iter()
            .find_map(|scope| self.local(scope, name, END))
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
        let Some(binding) = self.django_class_attribute(class, name) else {
            return;
        };
        if let Value::Callable(invocation) = self.binding(binding, END, 0) {
            self.scopes[scope]
                .routes
                .push((object, invocation.target, kind));
            self.method_receivers
                .entry(invocation.target)
                .or_default()
                .insert(class);
        } else {
            self.django_registration_issue(
                object,
                at,
                "Django lifecycle callback could not be resolved",
            );
        }
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
        self.django_signal_sources(&value)
    }

    fn django_signal_sources(&mut self, value: &Value) -> DjangoReceiver {
        match value {
            Value::Sequence(values) if values.is_empty() => DjangoReceiver::Empty,
            Value::Sequence(values) => {
                let mut source = DjangoReceiver::Certain;
                for value in values {
                    match self.django_signal_source(value) {
                        DjangoReceiver::Certain => {}
                        DjangoReceiver::Possible => source = DjangoReceiver::Possible,
                        _ => return DjangoReceiver::Unresolved,
                    }
                }
                source
            }
            Value::Alternatives(values) => {
                let mut sources = values.iter().map(|value| self.django_signal_sources(value));
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
            _ => self.django_signal_source(value),
        }
    }

    fn django_signal_source(&mut self, value: &Value) -> DjangoReceiver {
        match value {
            Value::Provider(provider) if is_django_signal(provider) => DjangoReceiver::Certain,
            Value::DjangoSignal => DjangoReceiver::Possible,
            Value::Instance(class)
                if matches!(
                    self.class_member(*class, "connect", 0),
                    Some(Value::DjangoSignalConnect)
                ) =>
            {
                DjangoReceiver::Possible
            }
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
        builtin: bool,
    ) {
        let callback = registration_arguments(
            args,
            keywords,
            ["receiver", "sender", "weak", "dispatch_uid"],
        )
        .and_then(|[callback, ..]| callback)
        .map(|callback| self.identity(scope, callback, before, 0));
        let signal = if builtin {
            DjangoReceiver::Certain
        } else {
            DjangoReceiver::Possible
        };
        self.record_django_signal(scope, signal, callback, at);
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
                Value::Callable(invocation) if !self.is_class(invocation.target) => {
                    self.scopes[file].routes.push((
                        parent,
                        invocation.target,
                        "django_registration",
                    ));
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
