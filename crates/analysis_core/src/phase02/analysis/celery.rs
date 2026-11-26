//! Resolve Celery result backends through ordered, source-owned settings.

use super::{celery_routes::RouteOperation, *};

pub(super) const LOGGING_CONNECT: &str = "celery.signals.setup_logging.connect";
pub(super) const LOGGING_CALLBACK: &str = "celery_setup_logging";
pub(super) const LOGGING_PARAMETER: &str = "celery_setup_logging_parameter";

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Setting {
    Builtin,
    Named(String),
    Unknown,
}

enum Operation {
    Create(Setting),
    Configure(Setting),
    ReplaceConfiguration,
    Load,
}

struct Event {
    scope: ScopeId,
    at: Location,
    conditional: bool,
    operation: Operation,
}

#[derive(Default)]
pub(super) struct Backends(BTreeMap<Object, Vec<Event>>);

impl Graph<'_> {
    pub(super) fn register_celery_logging(
        &mut self,
        scope: ScopeId,
        target: SymbolId,
        decorator: &Expression,
    ) -> bool {
        let Binding::Definition(definition, body) = self.symbols[target].binding else {
            return false;
        };
        if self.branch_depth != 0
            || scope != self.scopes[scope].file
            || matches!(decorator.kind, ExpressionKind::Call { .. })
            || definition.kind != DefinitionKind::Function
            || definition.is_generator
            || definition.decorators.len() != 1
            || !definition.has_variadic_keyword_parameter
            || definition.required_positional_parameter_count != 0
            || !definition.required_keyword_only_parameters.is_empty()
        {
            return false;
        }
        let kwargs = definition
            .parameters
            .last()
            .expect("keyword collector is last");
        let parameter = self.by_location[&(body, kwargs.at.line, kwargs.at.column)];
        self.scopes[scope]
            .celery_logging_receivers
            .push((target, parameter));
        true
    }

    pub(super) fn dispatch_celery_logging(&mut self, object: Object, call: CallArguments<'_>) {
        if self.branch_depth != 0
            || call.scope != self.scopes[call.scope].file
            || call.scope != object.0
            || !self.stock_celery_logging.contains(&object)
            || !call.positional.is_empty()
            || !call.keywords.is_empty()
            || self
                .module_member_write("celery.signals", "setup_logging", (call.scope, call.before))
                .is_some()
        {
            self.scope_issue(
                call.scope,
                call.at,
                "unresolved_call",
                "Celery logging requires a same-module stock application and a zero-argument setup",
            );
            return;
        }

        // The stock Logging class suppresses subsequent setup calls in
        // this process. Only receivers already registered here can be
        // proved in source order. Earlier external setup is still possible.
        let row = &mut self.scopes[call.scope];
        if row.celery_logging_setup.is_none() {
            row.celery_logging_setup = Some(call.at);
            for &(target, parameter) in &row.celery_logging_receivers {
                row.edges.extend([
                    Edge {
                        target,
                        kind: LOGGING_CALLBACK,
                        certainty: "possible",
                    },
                    Edge {
                        target: parameter,
                        kind: LOGGING_PARAMETER,
                        certainty: "possible",
                    },
                ]);
            }
        }
    }

    pub(super) fn report_celery_logging_imports(&mut self, reach: &Reachability) {
        for &scope in &reach.scopes {
            let Some(at) = self.scopes[scope].celery_logging_setup else {
                continue;
            };
            let mut imported = self.celery_logging_imports_before(scope, at);
            let mut visited = BTreeSet::from([scope]);
            while let Some(module) = imported.pop() {
                if !reach.scopes.contains(&module) || !visited.insert(module) {
                    continue;
                }
                let row = &self.scopes[module];
                if !row.celery_logging_receivers.is_empty() {
                    self.scope_issue(
                        scope,
                        at,
                        "unresolved_registration",
                        "Celery logging dispatch may reach receivers registered in an imported module",
                    );
                    break;
                }
                imported.extend(&row.imports);
            }
        }
    }

    fn celery_logging_imports_before(&self, scope: ScopeId, at: Location) -> Vec<ScopeId> {
        let row = &self.scopes[scope];
        let mut imported: Vec<_> = row
            .initializer_imports
            .iter()
            .filter_map(|&(id, module)| {
                (position(self.symbols[id].name.at) < position(at)).then_some(module)
            })
            .collect();
        // Conditional imports can publish receivers before the first setup.
        // Include only modules selected by an evaluated import in this scope.
        for &id in &row.evaluated_imports {
            let symbol = &self.symbols[id];
            if symbol.scope != scope
                || position(symbol.name.at) >= position(at)
                || !self.conditional.contains_key(&id)
            {
                continue;
            }
            let Binding::Import(import) = symbol.binding else {
                continue;
            };
            let Some(module) = self.import_module(row.file, import) else {
                continue;
            };
            let child = import
                .member
                .as_ref()
                .map(|name| format!("{module}.{name}"));
            imported.extend(row.imports.iter().copied().filter(|&file| {
                let name = &self.files[file].module;
                name == &module || Some(name) == child.as_ref() || within_package(&module, name)
            }));
        }
        imported
    }

    pub(super) fn record_celery_expanded_config(
        &mut self,
        object: Object,
        scope: ScopeId,
        mapping: &Expression,
        before: Location,
        at: Location,
    ) {
        let value = self.identity(scope, mapping, before, 0);
        if matches!(&value, Value::Mapping(items) if items.as_chunks::<2>().0.iter().all(|[key, _]| matches!(key, Value::Text(_))))
        {
            if let Some(routes) = configured_mapping_member(&value, "task_routes") {
                self.record_celery_route_event(
                    object,
                    scope,
                    at,
                    RouteOperation::Configure(routes),
                );
            }
            if let Some(setting) = configured_mapping(&value) {
                self.record_celery_backend_event(object, scope, at, Operation::Configure(setting));
            }
        } else {
            // An unbounded mapping may update result_backend or leave the
            // previous setting in force. Keep both possibilities visible.
            self.scope_issue(
                scope,
                at,
                "unresolved_registration",
                "Celery keyword expansion requires a bounded mapping with string keys",
            );
            self.branch_depth += 1;
            self.record_celery_backend_event(
                object,
                scope,
                at,
                Operation::Configure(Setting::Unknown),
            );
            self.branch_depth -= 1;
            self.record_celery_route_event(object, scope, at, RouteOperation::OpaqueUpdate);
        }
    }

    pub(super) fn record_celery_backend_creation(
        &mut self,
        object: Object,
        call: CallArguments<'_>,
    ) {
        if self.branch_depth == 0
            && call.scope == object.0
            && call.keywords.is_empty()
            && matches!(
                call.positional,
                [] | [Expression {
                    kind: ExpressionKind::Text(_),
                    ..
                }]
            )
        {
            self.stock_celery_logging.insert(object);
        }

        let kwarg = |key| {
            call.keywords
                .iter()
                .find_map(|(name, value)| (name == key).then_some(value))
        };
        // The third positional constructor argument is the backend. An
        // explicit result_backend setting takes precedence over that default.
        let backend = kwarg("result_backend")
            .or_else(|| kwarg("backend"))
            .or_else(|| call.positional.get(2));
        let setting = if kwarg("config_source").is_some()
            || kwarg("changes").is_some()
            || call.positional.len() > 11
        {
            Setting::Unknown
        } else {
            backend.map_or(Setting::Builtin, |value| {
                self.celery_setting(call.scope, value, call.before)
            })
        };
        self.record_celery_backend_event(object, call.scope, call.at, Operation::Create(setting));
        let routes = match kwarg("task_routes") {
            Some(value) => Some(self.celery_route_setting(call.scope, value, call.before)),
            None if kwarg("config_source").is_some()
                || kwarg("changes").is_some()
                || call.positional.len() > 11 =>
            {
                Some(Value::Unknown)
            }
            None => None,
        };
        self.record_celery_route_event(object, call.scope, call.at, RouteOperation::Create(routes));
    }

    pub(super) fn record_celery_backend_config(
        &mut self,
        object: Object,
        method: &str,
        call: CallArguments<'_>,
    ) {
        if method == "conf.update" {
            if let Some((_, value)) = call.keywords.iter().find(|(key, _)| key == "task_routes") {
                let routes = self.celery_route_setting(call.scope, value, call.before);
                self.record_celery_route_event(
                    object,
                    call.scope,
                    call.at,
                    RouteOperation::Configure(routes),
                );
            } else if let [mapping] = call.positional {
                let mapping = self.identity(call.scope, mapping, call.before, 0);
                if matches!(&mapping, Value::Mapping(items) if items.as_chunks::<2>().0.iter().all(|[key, _]| matches!(key, Value::Text(_))))
                {
                    if let Some(routes) = configured_mapping_member(&mapping, "task_routes") {
                        self.record_celery_route_event(
                            object,
                            call.scope,
                            call.at,
                            RouteOperation::Configure(routes),
                        );
                    }
                } else {
                    self.record_celery_route_event(
                        object,
                        call.scope,
                        call.at,
                        RouteOperation::OpaqueUpdate,
                    );
                }
            } else if !call.positional.is_empty() {
                self.record_celery_route_event(
                    object,
                    call.scope,
                    call.at,
                    RouteOperation::OpaqueUpdate,
                );
            }
        }
        let setting = match method {
            "conf.update" => {
                if let Some((_, value)) = call
                    .keywords
                    .iter()
                    .find(|(key, _)| key == "result_backend")
                {
                    Some(self.celery_setting(call.scope, value, call.before))
                } else {
                    match call.positional {
                        [] => None,
                        [mapping] => {
                            configured_mapping(&self.identity(call.scope, mapping, call.before, 0))
                        }
                        _ => Some(Setting::Unknown),
                    }
                }
            }
            "config_from_object" => Some(match (call.positional, call.keywords) {
                ([config], []) => {
                    let module = match self.identity(call.scope, config, call.before, 0) {
                        Value::Module(file) => Some(file),
                        Value::Text(name) => self.modules.get(name.as_str()).copied(),
                        _ => None,
                    };
                    if let Some(file) = module {
                        self.scopes[call.scope].imports.push(file);
                        let routes = self
                            .local(file, "task_routes", END)
                            .map_or(Value::None, |binding| self.binding(binding, END, 0));
                        self.record_celery_route_event(
                            object,
                            call.scope,
                            call.at,
                            RouteOperation::Configure(routes),
                        );
                        match self.local(file, "result_backend", END) {
                            Some(binding) => setting_value(self.binding(binding, END, 0)),
                            None => Setting::Builtin,
                        }
                    } else {
                        self.record_celery_route_event(
                            object,
                            call.scope,
                            call.at,
                            RouteOperation::OpaqueUpdate,
                        );
                        Setting::Unknown
                    }
                }
                _ => {
                    self.record_celery_route_event(
                        object,
                        call.scope,
                        call.at,
                        RouteOperation::OpaqueUpdate,
                    );
                    Setting::Unknown
                }
            }),
            _ => unreachable!("only recognized Celery settings methods are recorded"),
        };
        if let Some(setting) = setting {
            self.record_celery_backend_event(
                object,
                call.scope,
                call.at,
                Operation::Configure(setting),
            );
        }
    }

    pub(super) fn record_celery_backend_write(
        &mut self,
        scope: ScopeId,
        at: Location,
        statement: &StatementKind,
    ) -> bool {
        let (receiver, member, value) = match statement {
            StatementKind::WriteAttribute {
                receiver,
                member,
                value,
            } => (receiver, member, value),
            StatementKind::WriteModuleEntry {
                mapping,
                module,
                value,
            } => (mapping, module, value),
            _ => return false,
        };
        let receiver = self.identity(scope, receiver, at, 0);
        match receiver {
            Value::Object(object, ObjectKind::CeleryApp) if member == "log" => {
                self.stock_celery_logging.remove(&object);
            }
            Value::CeleryLogging(object) if member == "setup_logging_subsystem" => {
                self.stock_celery_logging.remove(&object);
            }
            _ => {}
        }
        self.record_celery_config_entry(scope, at, receiver, member, value.as_ref())
    }

    pub(super) fn record_celery_config_key(
        &mut self,
        scope: ScopeId,
        at: Location,
        mapping: &Expression,
        key: &Expression,
        value: &Option<Expression>,
    ) -> bool {
        let Value::CeleryConfig(object) = self.identity(scope, mapping, at, 0) else {
            return false;
        };
        let receiver = Value::CeleryConfig(object);
        let value = value.as_ref();
        if let Value::Text(key) = self.identity(scope, key, at, 0) {
            return self.record_celery_config_entry(scope, at, receiver, &key, value);
        }
        // The unknown key can leave both settings unchanged or replace one.
        self.branch_depth += 1;
        for name in ["task_routes", "result_backend"] {
            self.record_celery_config_entry(scope, at, receiver.clone(), name, value);
        }
        self.branch_depth -= 1;
        self.record_celery_route_event(object, scope, at, RouteOperation::OpaqueUpdate);
        self.scope_issue(
            scope,
            at,
            "unresolved_registration",
            "Celery configuration write has an unresolved key",
        );
        true
    }

    fn record_celery_config_entry(
        &mut self,
        scope: ScopeId,
        at: Location,
        receiver: Value,
        member: &str,
        value: Option<&Expression>,
    ) -> bool {
        let (object, setting) = match receiver {
            Value::CeleryConfig(object) => {
                if member == "task_routes" {
                    let routes = value.map_or(Value::None, |value| {
                        self.celery_route_setting(scope, value, at)
                    });
                    self.record_celery_route_event(
                        object,
                        scope,
                        at,
                        RouteOperation::Configure(routes),
                    );
                } else if member == "update" {
                    self.record_celery_route_event(object, scope, at, RouteOperation::OpaqueUpdate);
                    return false;
                }
                (
                    object,
                    (member == "result_backend").then(|| {
                        value.map_or(Setting::Builtin, |value| {
                            self.celery_setting(scope, value, at)
                        })
                    }),
                )
            }
            Value::Object(object, ObjectKind::CeleryApp) if member == "conf" => {
                let plain_object =
                    value.is_some_and(|value| self.celery_plain_object(scope, value, at));
                self.record_celery_route_event(
                    object,
                    scope,
                    at,
                    RouteOperation::ReplaceConfiguration { plain_object },
                );
                self.record_celery_backend_event(
                    object,
                    scope,
                    at,
                    Operation::ReplaceConfiguration,
                );
                return true;
            }
            _ => return false,
        };
        if let Some(setting) = setting {
            self.record_celery_backend_event(object, scope, at, Operation::Configure(setting));
        }
        true
    }

    pub(super) fn record_celery_backend_read(
        &mut self,
        object: Object,
        scope: ScopeId,
        at: Location,
    ) {
        self.record_celery_backend_event(object, scope, at, Operation::Load);
    }

    fn celery_setting(&mut self, scope: ScopeId, value: &Expression, at: Location) -> Setting {
        if matches!(value.kind, ExpressionKind::None) {
            return Setting::Builtin;
        }
        setting_value(self.identity(scope, value, at, 0))
    }

    fn record_celery_backend_event(
        &mut self,
        object: Object,
        scope: ScopeId,
        at: Location,
        operation: Operation,
    ) {
        self.celery_backends
            .0
            .entry(object)
            .or_default()
            .push(Event {
                scope,
                at,
                conditional: self.branch_depth != 0,
                operation,
            });
    }

    pub(super) fn collect_celery_backends(&mut self) {
        for events in std::mem::take(&mut self.celery_backends.0).into_values() {
            let Some(creation) = events
                .iter()
                .find(|event| matches!(event.operation, Operation::Create(_)))
            else {
                continue;
            };
            let scope = creation.scope;
            let conditional_creation = creation.conditional;
            if events
                .iter()
                .filter(|event| matches!(event.operation, Operation::Create(_)))
                .count()
                > 1
            {
                self.scope_issue(
                    scope,
                    creation.at,
                    "unresolved_registration",
                    "Celery backend analysis cannot distinguish repeated allocations at one source location",
                );
                for event in events {
                    if let Operation::Create(setting) | Operation::Configure(setting) =
                        event.operation
                    {
                        self.retain_celery_backend(event.scope, event.at, &setting, false, false);
                    }
                }
                continue;
            }
            let mut ordered = Vec::new();
            let mut external = false;
            for event in events {
                if event.scope == scope {
                    ordered.push(event);
                } else {
                    external = true;
                    self.scope_issue(
                        event.scope,
                        event.at,
                        "unresolved_registration",
                        "Celery backend settings cross an unresolved execution boundary",
                    );
                    if let Operation::Configure(setting) = &event.operation {
                        self.retain_celery_backend(event.scope, event.at, setting, false, false);
                    }
                }
            }
            let mut current = BTreeSet::new();
            let mut possible_load = external || conditional_creation;
            let mut opaque_config = false;
            let mut last = None;
            for event in ordered {
                last = Some(event.at);
                match event.operation {
                    Operation::Create(setting) | Operation::Configure(setting) => {
                        if !event.conditional && !opaque_config {
                            current.clear();
                        }
                        current.insert(setting);
                    }
                    Operation::ReplaceConfiguration => {
                        // Existing aliases can still point to the old settings
                        // object when the app's conf reference is replaced.
                        opaque_config = true;
                        current.insert(Setting::Unknown);
                    }
                    Operation::Load => {
                        self.retain_celery_backends(
                            scope,
                            event.at,
                            &current,
                            possible_load || event.conditional,
                            false,
                        );
                        if !event.conditional {
                            last = None;
                            break;
                        }
                        possible_load = true;
                    }
                }
            }
            if let Some(at) = last
                && !self.failed_terminal_modules.contains(&scope)
            {
                let exact_default =
                    possible_load && !external && !conditional_creation && !opaque_config;
                self.retain_celery_backends(scope, at, &current, possible_load, exact_default);
            }
        }
    }

    fn retain_celery_backends(
        &mut self,
        scope: ScopeId,
        at: Location,
        candidates: &BTreeSet<Setting>,
        possible: bool,
        exact_default: bool,
    ) {
        for setting in candidates {
            self.retain_celery_backend(
                scope,
                at,
                setting,
                !possible && candidates.len() == 1,
                exact_default && candidates.len() == 1,
            );
        }
    }

    fn retain_celery_backend(
        &mut self,
        scope: ScopeId,
        at: Location,
        setting: &Setting,
        certain: bool,
        exact_default: bool,
    ) {
        match setting {
            Setting::Builtin => return,
            Setting::Named(name) => {
                let parts = name.rsplit_once(':').or_else(|| name.rsplit_once('.'));
                if let Some((module, member)) = parts
                    && let Some(&file) = self.modules.get(module)
                {
                    let provider = self.module_member_at(
                        module,
                        member,
                        END,
                        (scope, at),
                        ModuleMemberAccess::FromImport,
                        0,
                    );
                    if matches!(provider, Value::DeletedModuleBinding) {
                        if file != self.scopes[scope].file {
                            self.scopes[scope].imports.push(file);
                        }
                        return;
                    }
                    if let Value::Callable(invocation) = provider
                        && !self.is_coroutine(invocation.target)
                    {
                        if file != self.scopes[scope].file {
                            self.scopes[scope].imports.push(file);
                        }
                        if exact_default {
                            // Configuration proves the default for an uncached
                            // lookup. An earlier possible load can still win.
                            self.edge(
                                scope,
                                invocation.target,
                                "celery_backend_default",
                                "certain",
                            );
                        }
                        let certainty = if certain { "certain" } else { "possible" };
                        self.invoke(scope, invocation, "celery_backend", certainty, at);
                        if self.is_class(invocation.target) {
                            self.constructor(
                                scope,
                                invocation.target,
                                "celery_backend",
                                certainty,
                                at,
                            );
                        }
                        return;
                    }
                }
            }
            Setting::Unknown => {}
        }
        self.scope_issue(
            scope,
            at,
            "unresolved_registration",
            "Celery result backend cannot be resolved to a bounded provider",
        );
    }
}

fn configured_mapping(value: &Value) -> Option<Setting> {
    configured_mapping_member(value, "result_backend").map(setting_value)
}

fn configured_mapping_member(value: &Value, name: &str) -> Option<Value> {
    let Value::Mapping(items) = value else {
        return Some(Value::Unknown);
    };
    for [key, value] in items.as_chunks::<2>().0.iter().rev() {
        match key {
            Value::Text(key) if key == name => return Some(value.clone()),
            Value::Text(_) => {}
            _ => return Some(Value::Unknown),
        }
    }
    None
}

fn setting_value(value: Value) -> Setting {
    match value {
        Value::Text(name) => match name
            .split_once("://")
            .map_or(name.as_str(), |(scheme, _)| scheme)
        {
            "rpc" | "redis" | "rediss" | "cache" | "db" | "disabled" => Setting::Builtin,
            _ => Setting::Named(name),
        },
        _ => Setting::Unknown,
    }
}
