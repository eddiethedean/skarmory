//! Source-backed protocols named by effective Django settings.

use super::*;

mod backend_writes;
mod collections;

const PROTOCOL_SETTINGS: [&str; 5] = [
    "AUTHENTICATION_BACKENDS",
    "STORAGES",
    "LOGGING",
    "MIDDLEWARE",
    "TEMPLATES",
];

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
    Filter { default_constructor: bool },
    Formatter { default_constructor: bool },
    Middleware,
    ContextProcessor,
}

impl Protocol {
    fn methods(self) -> &'static [&'static str] {
        match self {
            Self::Auth => AUTH_METHODS,
            Self::Storage => STORAGE_METHODS,
            Self::Filter { .. } => &["filter"],
            Self::Formatter { .. } => &[
                "format",
                "formatTime",
                "formatException",
                "formatMessage",
                "formatStack",
            ],
            Self::Middleware | Self::ContextProcessor => &[],
        }
    }

    fn evidence(self) -> &'static str {
        match self {
            Self::Auth => "django_auth_backend",
            Self::Storage => "django_storage_backend",
            Self::Filter { .. } | Self::Formatter { .. } => "django_logging_factory",
            Self::Middleware => "django_middleware",
            Self::ContextProcessor => "django_setting_reference",
        }
    }
}

#[derive(Default)]
struct SettingsWrites {
    classes: BTreeMap<SymbolId, BTreeSet<String>>,
    callback_bodies: BTreeMap<SymbolId, (ScopeId, Location)>,
    modules: BTreeSet<ScopeId>,
}

impl Graph<'_> {
    pub(in crate::phase02::analysis) fn django_template_setting(
        &mut self,
        settings: ScopeId,
        reach: &Reachability,
    ) -> Option<Value> {
        let binding = self.local(settings, "TEMPLATES", END)?;
        let effective = self.django_collection_values(settings, "TEMPLATES", reach);
        if !self
            .django_settings_initializer_writes(settings)?
            .modules
            .is_empty()
        {
            return None;
        }
        Some(effective.unwrap_or_else(|| self.binding(binding, END, 0)))
    }

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
        let mut replaced_bodies = BTreeSet::new();
        for settings in selected {
            let mut effective_collections = BTreeMap::new();
            for name in PROTOCOL_SETTINGS {
                if self.local(settings, name, END).is_some()
                    && let Some(value) = self.django_collection_values(settings, name, reach)
                {
                    effective_collections.insert(name, value);
                }
            }
            // An unresolved side effect can change the effective settings.
            let Some(writes) = self.django_settings_initializer_writes(settings) else {
                continue;
            };
            // An imported module write can change a named setting before its
            // export is captured, even if the source binding appears stable.
            if !writes.modules.is_empty() {
                continue;
            }
            for name in PROTOCOL_SETTINGS {
                let Some(binding) = self.local(settings, name, END) else {
                    continue;
                };
                if name == "LOGGING"
                    && (self.modules.contains_key("logging")
                        || self.modules.contains_key("logging.config")
                        || self
                            .local(settings, "LOGGING_CONFIG", END)
                            .is_some_and(|config| {
                                !matches!(self.binding(config, END, 0), Value::Text(path)
                                if path == "logging.config.dictConfig")
                            }))
                {
                    continue;
                }
                let value = effective_collections.remove(name).unwrap_or_else(|| {
                    let value = self.binding(binding, END, 0);
                    self.exact_settings_collection_version(name, binding, value)
                });
                let mut paths = Vec::new();
                if name == "LOGGING" && opaque_logging_version(&value) {
                    self.scope_issue(
                        settings,
                        self.symbols[binding].name.at,
                        "unresolved_registration",
                        "Django logging configuration version could not be resolved",
                    );
                }
                collect_settings_paths(name, &value, &mut paths);
                for (path, protocol) in paths {
                    self.django_settings_protocol(
                        settings,
                        path,
                        protocol,
                        &writes,
                        &mut roots,
                        &mut replaced_bodies,
                    );
                }
            }
        }
        for (scope, line, column) in replaced_bodies {
            self.scope_issue(
                scope,
                Location { line, column },
                "unresolved_registration",
                "configured Django callback body was replaced",
            );
        }
        roots
    }

    fn exact_settings_collection_version(
        &self,
        name: &str,
        binding: SymbolId,
        mut value: Value,
    ) -> Value {
        if name == "LOGGING"
            && let Value::Mapping(items) = &mut value
            && let Some(pair) = items
                .as_chunks_mut::<2>()
                .0
                .iter_mut()
                .rev()
                .find(|[key, _]| matches!(key, Value::Text(key) if key == "version"))
            && matches!(pair[1], Value::Scalar)
            && let Some(version) = self.literal_logging_version(binding, 0)
        {
            pair[1] = Value::Integer(version);
        }
        value
    }

    fn literal_logging_version(&self, binding: SymbolId, depth: usize) -> Option<i64> {
        if depth >= MAX_BINDING_CHAIN || self.binding_depends_on_branch(binding, END) {
            return None;
        }
        let row = &self.symbols[binding];
        match row.binding {
            Binding::Variable(Some(value), before) => match &value.kind {
                ExpressionKind::Scalar(Some(version)) => Some(*version),
                ExpressionKind::Name(name) => {
                    self.literal_logging_version(self.lexical(row.scope, name, before)?, depth + 1)
                }
                ExpressionKind::Dictionary(items) => {
                    let [_, version] = items.as_chunks::<2>().0.iter().rev().find(
                        |[key, _]| matches!(&key.kind, ExpressionKind::Text(key) if key == "version"),
                    )?;
                    match &version.kind {
                        ExpressionKind::Scalar(Some(version)) => Some(*version),
                        ExpressionKind::Name(name) => self.literal_logging_version(
                            self.lexical(row.scope, name, before)?,
                            depth + 1,
                        ),
                        _ => None,
                    }
                }
                _ => None,
            },
            Binding::Import(import) => {
                let module = self.import_module(self.scopes[row.scope].file, import)?;
                let file = *self.modules.get(module.as_str())?;
                let member = import.member.as_deref()?;
                let access = (row.scope, import.local.at);
                let before = self
                    .import_member_position(row.scope, &module, import)
                    .unwrap_or(END);
                if self.failed_terminal_modules.contains(&file)
                    || self.module_member_write(&module, member, access).is_some()
                {
                    return None;
                }
                let ModuleMember::Binding(source, _) = self.selected_module_member(
                    file,
                    member,
                    before,
                    access,
                    ModuleMemberAccess::FromImport,
                ) else {
                    return None;
                };
                self.literal_logging_version(source, depth + 1)
            }
            _ => None,
        }
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
                Value::Callable(invocation) if name == "__code__" => {
                    writes
                        .callback_bodies
                        .insert(invocation.target, (scope, at));
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
        replaced_bodies: &mut BTreeSet<(ScopeId, usize, usize)>,
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
        let mut class = target.target;
        if matches!(protocol, Protocol::ContextProcessor) {
            if !self.is_class(class)
                && !self.is_coroutine(class)
                && !self.is_generator(class)
                && self.bounded_method_descriptor(class) == Some(ConverterDescriptor::Instance)
                && !modified_callback_body(class, &writes, replaced_bodies)
            {
                self.read.extend(target.bindings);
                roots.push(possible_edge(class, protocol.evidence()));
            }
            return;
        }
        if !self.is_class(class) {
            return;
        }
        let symbol = &self.symbols[class];
        if let Binding::Definition(definition, _) = symbol.binding
            && let [decorator] = definition.decorators.as_slice()
            && !matches!(decorator.kind, ExpressionKind::Call { .. })
            && let Value::Callable(invocation) =
                self.probe_identity(symbol.scope, decorator, definition.name.at)
            && let Some(replacement) = self.known_definition_replacement(class, invocation)
        {
            let Value::Callable(replacement) = replacement else {
                // A function returned by the decorator has no class protocol.
                return;
            };
            class = replacement.target;
        }
        if matches!(protocol, Protocol::Middleware) {
            self.read.extend(target.bindings);
            roots.push(possible_edge(class, "django_setting_reference"));
            match self.prove_django_middleware(class, settings) {
                Some(site)
                    if !writes
                        .iter()
                        .any(|writes| writes.classes.contains_key(&class))
                        && !modified_callback_body(site.initializer, &writes, replaced_bodies)
                        && !modified_callback_body(site.handler, &writes, replaced_bodies) =>
                {
                    for callback in [site.initializer, site.handler] {
                        roots.push(possible_edge(callback, protocol.evidence()));
                    }
                    self.django_middleware_sites.push(site);
                }
                _ => self.scope_issue(
                    file,
                    self.symbols[binding].name.at,
                    "unresolved_registration",
                    "configured Django middleware dispatch is outside the bounded model",
                ),
            }
            return;
        }
        let allow_logging_base = matches!(
            protocol,
            Protocol::Filter {
                default_constructor: true
            } | Protocol::Formatter {
                default_constructor: true
            }
        );
        let Some(mro) =
            self.source_protocol_mro(class, &mut BTreeMap::new(), 0, allow_logging_base)
        else {
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
            if modified_callback_body(target.target, &writes, replaced_bodies) {
                continue;
            }
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
            if *name == "format"
                && matches!(protocol, Protocol::Formatter { .. })
                && descriptor == ConverterDescriptor::Instance
                && mro.contains(&ConverterBase::Logging(
                    logging_config::LoggingBase::Formatter,
                ))
            {
                self.logging_formatter_methods.insert(target.target);
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
            let class = match base {
                ConverterBase::Source(class) => class,
                ConverterBase::Object => continue,
                ConverterBase::Logging(logging) if logging.defines(name) => return Some(None),
                ConverterBase::Logging(_) => continue,
                ConverterBase::String => return None,
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
            // Bare class annotations update __annotations__, leaving any
            // earlier member or inherited descriptor in place.
            if let Some(binding) = self.scopes[body]
                .names
                .get(name)
                .into_iter()
                .flatten()
                .rev()
                .copied()
                .find(|&id| !matches!(self.symbols[id].binding, Binding::Variable(None, _)))
            {
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

fn modified_callback_body(
    target: SymbolId,
    writes: &[&SettingsWrites],
    bodies: &mut BTreeSet<(ScopeId, usize, usize)>,
) -> bool {
    let mut modified = false;
    for &(scope, at) in writes
        .iter()
        .filter_map(|writes| writes.callback_bodies.get(&target))
    {
        bodies.insert((scope, at.line, at.column));
        modified = true;
    }
    modified
}

fn possible_edge(target: SymbolId, kind: &'static str) -> Edge {
    Edge {
        target,
        kind,
        certainty: "possible",
    }
}

fn opaque_logging_version(value: &Value) -> bool {
    match value {
        Value::Alternatives(values) => values.iter().any(opaque_logging_version),
        _ => matches!(get(value, "version"), Some(Value::Scalar | Value::Unknown)),
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
        ("AUTHENTICATION_BACKENDS" | "MIDDLEWARE", Value::Sequence(items)) => {
            paths.extend(items.iter().filter_map(|value| {
                let Value::Text(path) = value else {
                    return None;
                };
                Some((
                    path.as_str(),
                    if name == "MIDDLEWARE" {
                        Protocol::Middleware
                    } else {
                        Protocol::Auth
                    },
                ))
            }));
        }
        ("STORAGES", _) => {
            for entry in mapping(value).into_values() {
                if let Some(Value::Text(path)) = get(entry, "BACKEND") {
                    paths.push((path, Protocol::Storage));
                }
            }
        }
        // dictConfig rejects an omitted or unsupported version before
        // resolving any formatter or filter factories.
        ("LOGGING", _)
            if matches!(
                get(value, "version"),
                Some(Value::Integer(1) | Value::Bool(true))
            ) =>
        {
            for section in ["filters", "formatters"] {
                if let Some(entries) = get(value, section) {
                    for entry in mapping(entries).into_values() {
                        if let Some(Value::Text(path)) = get(entry, "()") {
                            let default_constructor = mapping(entry).len() == 1;
                            let protocol = if section == "filters" {
                                Protocol::Filter {
                                    default_constructor,
                                }
                            } else {
                                Protocol::Formatter {
                                    default_constructor,
                                }
                            };
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
