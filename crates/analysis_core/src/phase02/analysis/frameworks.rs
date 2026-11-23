//! Project-wide framework and test discovery for the Phase 0.3 profile.

use super::*;

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
                FrameworkClassRole::DjangoModel => (None, &[][..], false),
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
        if self
            .scopes
            .iter()
            .all(|scope| scope.starts_django.is_empty())
        {
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
            for scope in 0..self.scopes.len() {
                if let Some((_, target, _)) = self.scopes[scope]
                    .routes
                    .iter()
                    .find(|(_, _, kind)| *kind == "django_signal")
                {
                    self.scope_issue(
                        scope,
                        self.symbols[*target].name.at,
                        "unresolved_registration",
                        "Django signal registration has no selected startup",
                    );
                }
            }
            return;
        }
        for scope in 0..self.scopes.len() {
            for (at, settings) in std::mem::take(&mut self.scopes[scope].starts_django) {
                let root = settings
                    .and_then(|module| self.modules.get(module.as_str()).copied())
                    .and_then(|file| {
                        let row = &mut self.scopes[scope];
                        row.activated_apps
                            .extend([django_loaded_module(file), django_loaded_module(row.file)]);
                        self.local(file, "ROOT_URLCONF", END)
                    })
                    .and_then(|binding| match self.binding(binding, END, 0) {
                        Value::Text(module) => self.modules.get(module.as_str()).copied(),
                        _ => None,
                    });
                if let Some(file) = root {
                    let object = framework_module(file);
                    self.scopes[scope].activated_apps.push(object);
                    self.django_roots.insert(object);
                } else {
                    self.scope_issue(
                        scope,
                        at,
                        "unresolved_registration",
                        "Django settings and ROOT_URLCONF could not be resolved",
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

    pub(super) fn record_django_signal(&mut self, scope: ScopeId, target: SymbolId) {
        let file = self.scopes[scope].file;
        self.scopes[scope]
            .routes
            .push((django_loaded_module(file), target, "django_signal"));
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
                        .push((parent, class, "django_registration"))
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
