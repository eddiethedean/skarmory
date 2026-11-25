//! Source-backed protocols named by effective Django settings.

use super::*;

const AUTH_METHODS: &[&str] = &[
    "authenticate",
    "aauthenticate",
    "get_user",
    "aget_user",
    "get_user_permissions",
    "aget_user_permissions",
    "get_group_permissions",
    "aget_group_permissions",
    "get_all_permissions",
    "aget_all_permissions",
    "has_perm",
    "ahas_perm",
    "has_module_perms",
    "ahas_module_perms",
    "with_perm",
];

const STORAGE_METHODS: &[&str] = &[
    "save",
    "delete",
    "exists",
    "open",
    "listdir",
    "path",
    "size",
    "url",
    "generate_filename",
    "get_valid_name",
    "get_available_name",
    "get_accessed_time",
    "get_created_time",
    "get_modified_time",
];

#[derive(Clone, Copy)]
enum Protocol {
    Auth,
    Storage,
    Filter,
    Formatter,
    ContextProcessor,
}

impl Protocol {
    fn methods(self) -> &'static [&'static str] {
        match self {
            Self::Auth => AUTH_METHODS,
            Self::Storage => STORAGE_METHODS,
            Self::Filter => &["filter"],
            Self::Formatter => &["format", "formatTime", "formatException", "formatStack"],
            Self::ContextProcessor => &[],
        }
    }

    fn evidence(self) -> &'static str {
        match self {
            Self::Auth => "django_auth_backend",
            Self::Storage => "django_storage_backend",
            Self::Filter | Self::Formatter => "django_logging_factory",
            Self::ContextProcessor => "django_setting_reference",
        }
    }
}

#[derive(Default)]
struct SettingsWrites {
    classes: BTreeMap<SymbolId, BTreeSet<String>>,
    modules: BTreeSet<ScopeId>,
}

impl Graph<'_> {
    pub(in crate::phase02::analysis) fn django_settings_protocol_roots(
        &mut self,
        reach: &Reachability,
    ) -> Vec<Edge> {
        let mut selected: BTreeSet<_> = self
            .django_settings_sources
            .iter()
            .filter_map(|&(scope, settings)| reach.scopes.contains(&scope).then_some(settings))
            .collect();
        if selected.is_empty()
            && self.django_startup_scopes.is_disjoint(&reach.scopes)
            && self.initial_django_settings == DjangoSettings::Unset
            && let Some(settings) = self.conventional_protocol_settings()
        {
            selected.insert(settings);
        }

        let mut roots = Vec::new();
        for settings in selected {
            // An unresolved side effect can change the effective settings.
            let Some(writes) = self.django_settings_initializer_writes(settings) else {
                continue;
            };
            // An imported module write can change a named setting before its
            // export is captured, even if the source binding appears stable.
            if !writes.modules.is_empty() {
                continue;
            }
            for name in [
                "AUTHENTICATION_BACKENDS",
                "STORAGES",
                "LOGGING",
                "TEMPLATES",
            ] {
                let Some(binding) = self.local(settings, name, END) else {
                    continue;
                };
                if name == "LOGGING"
                    && let Some(config) = self.local(settings, "LOGGING_CONFIG", END)
                    && !matches!(self.binding(config, END, 0), Value::Text(path)
                        if path == "logging.config.dictConfig")
                {
                    continue;
                }
                let value = self.binding(binding, END, 0);
                let mut paths = Vec::new();
                collect_settings_paths(name, &value, &mut paths);
                for (path, protocol) in paths {
                    self.django_settings_protocol(settings, path, protocol, &writes, &mut roots);
                }
            }
        }
        roots
    }

    fn django_settings_initializer_writes(&mut self, start: ScopeId) -> Option<SettingsWrites> {
        let mut reachable = BTreeSet::new();
        let mut pending = vec![start];
        while let Some(scope) = pending.pop() {
            if reachable.insert(scope) {
                pending.extend(&self.scopes[scope].imports);
                pending.extend(&self.scopes[scope].class_bodies);
                pending.extend(self.synchronous_calls(scope, false));
            }
        }
        if !self.failed_terminal_modules.is_disjoint(&reachable)
            || self
                .scope_issues
                .iter()
                .any(|(scope, _)| reachable.contains(scope))
        {
            return None;
        }
        let mut writes = SettingsWrites::default();
        for scope in reachable {
            self.django_settings_writes_in(scope, self.scopes[scope].body, &mut writes);
        }
        Some(writes)
    }

    fn django_settings_writes_in(
        &mut self,
        scope: ScopeId,
        body: &[Statement],
        writes: &mut SettingsWrites,
    ) {
        for statement in body {
            let at = statement.at;
            let (receiver, name) = match &statement.kind {
                StatementKind::WriteAttribute {
                    receiver, member, ..
                } => (receiver, member.as_str()),
                StatementKind::Evaluate(Expression {
                    kind:
                        ExpressionKind::Call {
                            function,
                            arguments,
                            keywords,
                        },
                    ..
                }) if keywords.is_empty() => {
                    let [receiver, name, rest @ ..] = arguments.as_slice() else {
                        continue;
                    };
                    let ExpressionKind::Text(name) = &name.kind else {
                        continue;
                    };
                    if !matches!(self.probe_identity(scope, function, at),
                        Value::Provider(provider) if matches!((provider.as_str(), rest.len()),
                            ("builtins.setattr", 1) | ("builtins.delattr", 0)))
                    {
                        continue;
                    }
                    (receiver, name.as_str())
                }
                StatementKind::If {
                    test,
                    body,
                    otherwise,
                } => {
                    let outcome = self.indexed_bool(scope, test, at, 0);
                    if outcome != Some(false) {
                        self.django_settings_writes_in(scope, body, writes);
                    }
                    if outcome != Some(true) {
                        self.django_settings_writes_in(scope, otherwise, writes);
                    }
                    continue;
                }
                StatementKind::With { body, .. } => {
                    self.django_settings_writes_in(scope, body, writes);
                    continue;
                }
                StatementKind::Return(_) | StatementKind::Raise(_) => break,
                _ => continue,
            };
            match self.probe_identity(scope, receiver, at) {
                Value::Callable(invocation) if self.is_class(invocation.target) => {
                    writes
                        .classes
                        .entry(invocation.target)
                        .or_default()
                        .insert(name.to_owned());
                }
                Value::Module(module) => {
                    writes.modules.insert(module);
                }
                _ => {}
            }
        }
    }

    fn conventional_protocol_settings(&mut self) -> Option<ScopeId> {
        if self.modules.contains_key("django") || self.modules.contains_key("django.urls") {
            return None;
        }
        let mut selected = None;
        for file in 0..self.files.len() {
            let Some(package) = self.files[file].module.strip_suffix(".settings") else {
                continue;
            };
            if !self
                .modules
                .get(package)
                .is_some_and(|&module| self.files[module].is_package)
            {
                continue;
            }
            let [Some(apps), Some(key)] =
                ["INSTALLED_APPS", "SECRET_KEY"].map(|name| self.local(file, name, END))
            else {
                continue;
            };
            if self.binding_depends_on_branch(apps, END)
                || self.binding_depends_on_branch(key, END)
                || !matches!(self.binding_value(apps, 0), Value::Sequence(list)
                    if list.iter().all(|entry| matches!(entry, Value::Text(_))))
                || !matches!(self.binding_value(key, 0), Value::Text(_))
            {
                continue;
            }
            if selected.replace(file).is_some() {
                return None;
            }
        }
        selected
    }

    fn django_settings_protocol(
        &mut self,
        settings: ScopeId,
        path: &str,
        protocol: Protocol,
        settings_writes: &SettingsWrites,
        roots: &mut Vec<Edge>,
    ) {
        let Some((module, name)) = path.rsplit_once('.') else {
            return;
        };
        let Some(&file) = self.modules.get(module) else {
            return;
        };
        let Some(binding) = self.local(file, name, END) else {
            return;
        };
        if self.binding_depends_on_branch(binding, END)
            || self
                .module_member_write(module, name, (settings, END))
                .is_some()
            || self.failed_terminal_modules.contains(&file)
        {
            return;
        }
        let Some(target) = self.probe_callable_binding(binding) else {
            return;
        };
        let Some(provider_writes) = self.django_settings_initializer_writes(file) else {
            return;
        };
        if provider_writes.modules.contains(&file) {
            return;
        }
        let writes = [settings_writes, &provider_writes];
        let class = target.target;
        if matches!(protocol, Protocol::ContextProcessor) {
            if !self.is_class(class)
                && !self.is_coroutine(class)
                && !self.is_generator(class)
                && self.bounded_method_descriptor(class) == Some(ConverterDescriptor::Instance)
            {
                self.read.extend(target.bindings);
                roots.push(possible_edge(class, protocol.evidence()));
            }
            return;
        }
        if !self.is_class(class) {
            return;
        }
        let Some(mro) = self.converter_mro(class, &mut BTreeMap::new(), 0) else {
            return;
        };
        // Creation or instance lookup can replace any method on the object.
        if [
            "__new__",
            "__init__",
            "__getattr__",
            "__getattribute__",
            "__bases__",
            "__class__",
            "__dict__",
        ]
        .iter()
        .any(|name| {
            !matches!(
                self.django_protocol_member(&mro, name, settings, &writes),
                Some(None)
            )
        }) {
            return;
        }

        self.read.extend(target.bindings);
        roots.push(possible_edge(class, "django_setting_reference"));
        for name in protocol.methods() {
            let Some(Some(method)) = self.django_protocol_member(&mro, name, settings, &writes)
            else {
                continue;
            };
            let Some(target) = self.probe_callable_binding(method) else {
                continue;
            };
            if self.is_coroutine(target.target)
                && !(matches!(protocol, Protocol::Auth)
                    && name.starts_with('a')
                    && *name != "authenticate")
            {
                continue;
            }
            let Some(descriptor) = self.bounded_method_descriptor(target.target) else {
                continue;
            };
            if matches!(descriptor, ConverterDescriptor::Instance)
                && self.receiver_parameter(target.target).is_some()
            {
                self.method_receivers
                    .entry(target.target)
                    .or_default()
                    .insert(class);
            }
            self.read.extend(target.bindings);
            roots.push(possible_edge(target.target, protocol.evidence()));
        }
    }

    // The inner option denotes an absent member. The outer option denotes a
    // lookup we could resolve without opaque replacements or unknown bases.
    fn django_protocol_member(
        &self,
        mro: &[ConverterBase],
        name: &str,
        settings: ScopeId,
        writes: &[&SettingsWrites],
    ) -> Option<Option<SymbolId>> {
        for &base in mro {
            let ConverterBase::Source(class) = base else {
                if base == ConverterBase::Object {
                    continue;
                }
                return None;
            };
            if writes.iter().any(|writes| {
                writes
                    .classes
                    .get(&class)
                    .is_some_and(|members| members.contains(name))
            }) {
                return None;
            }
            let Binding::Definition(_, body) = self.symbols[class].binding else {
                unreachable!()
            };
            match self.class_discovery_write(class, name, settings) {
                Some(mutations::MemberWriteValue::Deleted) => continue,
                Some(_) => return None,
                None => {}
            }
            if let Some(binding) = self.local(body, name, END) {
                if self.binding_depends_on_branch(binding, END) {
                    return None;
                }
                if !matches!(self.symbols[binding].binding, Binding::Deleted) {
                    return Some(Some(binding));
                }
            }
        }
        Some(None)
    }
}

fn possible_edge(target: SymbolId, kind: &'static str) -> Edge {
    Edge {
        target,
        kind,
        certainty: "possible",
    }
}

fn collect_settings_paths<'a>(name: &str, value: &'a Value, paths: &mut Vec<(&'a str, Protocol)>) {
    if let Value::Alternatives(values) = value {
        for value in values {
            collect_settings_paths(name, value, paths);
        }
        return;
    }
    match (name, value) {
        ("AUTHENTICATION_BACKENDS", Value::Sequence(items)) => {
            paths.extend(items.iter().filter_map(|value| {
                let Value::Text(path) = value else {
                    return None;
                };
                Some((path.as_str(), Protocol::Auth))
            }));
        }
        ("STORAGES", _) => {
            for entry in mapping(value).into_values() {
                if let Some(Value::Text(path)) = get(entry, "BACKEND") {
                    paths.push((path, Protocol::Storage));
                }
            }
        }
        // Native values distinguish definitely nonnumeric versions but
        // do not retain the value of number literals.
        ("LOGGING", _)
            if matches!(
                get(value, "version"),
                Some(Value::Scalar | Value::Bool(true))
            ) =>
        {
            for (section, protocol) in [
                ("filters", Protocol::Filter),
                ("formatters", Protocol::Formatter),
            ] {
                if let Some(entries) = get(value, section) {
                    for entry in mapping(entries).into_values() {
                        if let Some(Value::Text(path)) = get(entry, "()") {
                            paths.push((path, protocol));
                        }
                    }
                }
            }
        }
        ("TEMPLATES", Value::Sequence(engines)) => {
            for engine in engines {
                if !matches!(get(engine, "BACKEND"), Some(Value::Text(path))
                    if path == "django.template.backends.django.DjangoTemplates")
                {
                    continue;
                }
                if let Some(Value::Sequence(items)) =
                    get(engine, "OPTIONS").and_then(|options| get(options, "context_processors"))
                {
                    paths.extend(items.iter().filter_map(|value| {
                        let Value::Text(path) = value else {
                            return None;
                        };
                        Some((path.as_str(), Protocol::ContextProcessor))
                    }));
                }
            }
        }
        _ => {}
    }
}

fn mapping(value: &Value) -> BTreeMap<&str, &Value> {
    let Value::Mapping(items) = value else {
        return BTreeMap::new();
    };
    let (pairs, remainder) = items.as_chunks::<2>();
    if !remainder.is_empty() {
        return BTreeMap::new();
    }
    pairs
        .iter()
        .map(|[key, value]| match key {
            Value::Text(name) => Some((name.as_str(), value)),
            _ => None,
        })
        .collect::<Option<_>>()
        .unwrap_or_default()
}

fn get<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    mapping(value).get(key).copied()
}
