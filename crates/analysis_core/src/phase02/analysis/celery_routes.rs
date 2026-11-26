//! Resolve Celery task routers through ordered, source-owned settings.

use super::*;

pub(super) const ROUTER: &str = "celery_task_router";
pub(super) const ROUTER_PARAMETER: &str = "celery_router_parameter";

pub(super) enum RouteOperation {
    Create(Option<Value>),
    Configure(Value),
    OpaqueUpdate,
    ReplaceConfiguration { plain_object: bool },
    Load,
}

struct RouteEvent {
    scope: ScopeId,
    at: Location,
    conditional: bool,
    operation: RouteOperation,
}

#[derive(Default)]
pub(super) struct Routes(BTreeMap<Object, Vec<RouteEvent>>);

impl Routes {
    pub(super) fn has_application(&self) -> bool {
        self.0
            .values()
            .flatten()
            .any(|event| matches!(event.operation, RouteOperation::Create(_)))
    }
}

impl Graph<'_> {
    pub(super) fn celery_route_setting(
        &mut self,
        scope: ScopeId,
        value: &Expression,
        at: Location,
    ) -> Value {
        if matches!(value.kind, ExpressionKind::None) {
            Value::None
        } else {
            self.identity(scope, value, at, 0)
        }
    }

    pub(super) fn record_celery_route_load(
        &mut self,
        object: Object,
        scope: ScopeId,
        at: Location,
    ) {
        self.record_celery_route_event(object, scope, at, RouteOperation::Load);
    }

    pub(super) fn record_celery_route_event(
        &mut self,
        object: Object,
        scope: ScopeId,
        at: Location,
        operation: RouteOperation,
    ) {
        self.celery_routes
            .0
            .entry(object)
            .or_default()
            .push(RouteEvent {
                scope,
                at,
                conditional: self.branch_depth != 0,
                operation,
            });
    }

    fn celery_configuration_replacement(
        &self,
        object: Object,
        access: (ScopeId, Location),
    ) -> Option<&RouteEvent> {
        self.celery_routes
            .0
            .get(&object)?
            .iter()
            .rev()
            .find(|event| {
                event.scope == access.0
                    && position(event.at) < position(access.1)
                    && matches!(event.operation, RouteOperation::ReplaceConfiguration { .. })
            })
    }

    pub(super) fn celery_config_member(
        &self,
        object: Object,
        access: (ScopeId, Location),
    ) -> Value {
        if self
            .celery_configuration_replacement(object, access)
            .is_some()
        {
            Value::Unknown
        } else {
            Value::CeleryConfig(object)
        }
    }

    pub(super) fn celery_plain_object(
        &mut self,
        scope: ScopeId,
        value: &Expression,
        at: Location,
    ) -> bool {
        matches!(&value.kind, ExpressionKind::Call { function, arguments, keywords }
            if arguments.is_empty() && keywords.is_empty()
            && matches!(self.probe_identity(scope, function, at),
                Value::Provider(name) if name == "builtins.object"))
    }

    pub(super) fn terminal_celery_router_failure(
        &mut self,
        scope: ScopeId,
        statement: &Statement,
    ) -> bool {
        let at = statement.at;
        let reads = std::mem::take(&mut self.read);
        let failing = match &statement.kind {
            StatementKind::Evaluate(Expression {
                kind:
                    ExpressionKind::Call {
                        function,
                        arguments,
                        keywords,
                    },
                ..
            }) if keywords.is_empty()
                && matches!(arguments.as_slice(), [argument] if self.unbound_module_name(scope, argument, at))
                && matches!(self.probe_identity(scope, function, at), Value::Method(_, ObjectKind::CeleryApp, method) if method == "conf.update") =>
            {
                Some((None, Some(function.as_ref())))
            }
            StatementKind::Assign {
                names,
                value: Some(value),
                annotation: None,
            } if self.unbound_module_name(scope, value, at)
                && matches!(names.as_slice(), [name] if self.local(scope, &name.text, at).is_some_and(|binding| {
                    matches!(self.binding(binding, at, 0), Value::Provider(provider) if provider == "celery.Celery")
                })) =>
            {
                Some((None, None))
            }
            StatementKind::WriteCeleryConfigEntry {
                mapping,
                value: Some(value),
                ..
            } if self.unbound_module_name(scope, value, at)
                && matches!(
                    self.probe_identity(scope, mapping, at),
                    Value::CeleryConfig(_)
                )
                && self.closed_celery_config_prefix(scope, statement) =>
            {
                Some((None, None))
            }
            StatementKind::WriteAttribute {
                receiver,
                member,
                value: Some(value),
            } if member == "task_routes" => match &receiver.kind {
                ExpressionKind::Member(app, member)
                    if member == "conf"
                        && self.celery_invalid_route_configuration(scope, app, at) =>
                {
                    Some((Some(value), Some(receiver)))
                }
                _ => None,
            },
            _ => None,
        };
        self.read = reads;
        if let Some((value, receiver)) = failing {
            // Python evaluates the RHS before an assignment receiver.
            for expression in value.into_iter().chain(receiver) {
                self.walk(scope, expression, at);
            }
            return true;
        }
        false
    }

    fn celery_invalid_route_configuration(
        &mut self,
        scope: ScopeId,
        app: &Expression,
        at: Location,
    ) -> bool {
        if let Value::Object(object, ObjectKind::CeleryApp) = self.probe_identity(scope, app, at) {
            return self
                .celery_configuration_replacement(object, (scope, at))
                .is_some_and(|event| {
                    !event.conditional
                        && matches!(
                            event.operation,
                            RouteOperation::ReplaceConfiguration { plain_object: true }
                        )
                });
        }
        let ExpressionKind::Name(name) = &app.kind else {
            return false;
        };
        let Some(binding) = self.local(scope, name, at) else {
            return false;
        };
        if self.binding_depends_on_branch(binding, at) {
            return false;
        }
        if let Binding::Variable(Some(value), before) = self.symbols[binding].binding
            && self.celery_plain_object(scope, value, before)
            && let Some(previous) = self.local(scope, name, before)
        {
            return matches!(
                self.binding(previous, before, 0),
                Value::Object(_, ObjectKind::CeleryApp)
            );
        }
        false
    }

    fn closed_celery_config_prefix(&mut self, scope: ScopeId, failing: &Statement) -> bool {
        let body = self.scopes[scope].body;
        let Some(index) = body.iter().position(|item| std::ptr::eq(item, failing)) else {
            return false;
        };
        body[..index].iter().all(|statement| match &statement.kind {
            StatementKind::Import(import) => import.level == 0 && import.module == "celery"
                && !self.modules.contains_key("celery"),
            StatementKind::Assign {
                value: Some(Expression {kind: ExpressionKind::Call { function, arguments, keywords }, ..}),
                annotation: None, ..
            } => matches!(arguments.as_slice(), [arg] if matches!(arg.kind, ExpressionKind::Text(_)))
                && keywords.is_empty()
                && matches!(self.probe_identity(scope, function, statement.at),
                    Value::Provider(provider) if provider == "celery.Celery"),
            StatementKind::WriteAttribute { receiver, member, value: Some(value) }
                if matches!(member.as_str(), "task_routes" | "result_backend") =>
            {
                matches!(self.probe_identity(scope, receiver, statement.at), Value::CeleryConfig(_))
                    && (matches!(&value.kind, ExpressionKind::Text(_) | ExpressionKind::None)
                        || matches!(&value.kind, ExpressionKind::Sequence { items, .. }
                            if items.iter().all(|item| matches!(item.kind, ExpressionKind::Text(_)))))
            }
            StatementKind::Evaluate(Expression { kind: ExpressionKind::Text(_), .. }) => true,
            _ => false,
        })
    }

    pub(super) fn collect_celery_routes(&mut self) {
        // Keep the records available to identity walks while resolving textual
        // router references and exported app bindings.
        let objects: Vec<_> = self.celery_routes.0.keys().copied().collect();
        let mut exports = BTreeMap::new();
        for object in objects {
            let events = self.celery_routes.0.remove(&object).unwrap_or_default();
            if let Some(creation) = events
                .iter()
                .find(|event| matches!(event.operation, RouteOperation::Create(_)))
            {
                let scope = creation.scope;
                let mut current = Vec::new();
                let mut opaque = false;
                let mut configuration_replaced = false;
                for event in &events {
                    if event.scope != scope {
                        self.celery_route_issue(
                            event.scope,
                            event.at,
                            "Celery task routing crosses an unresolved execution boundary",
                        );
                        if let RouteOperation::Configure(routes) = &event.operation {
                            self.retain_celery_routes(event.scope, event.at, routes);
                        }
                        continue;
                    }
                    match &event.operation {
                        RouteOperation::Create(_) | RouteOperation::Configure(_) => {
                            if configuration_replaced {
                                // A captured settings alias may still refer to the retired mapping.
                                continue;
                            }
                            if !event.conditional {
                                current.clear();
                                opaque = false;
                            }
                            if let RouteOperation::Create(Some(routes))
                            | RouteOperation::Configure(routes) = &event.operation
                            {
                                current.push(routes);
                            }
                        }
                        RouteOperation::OpaqueUpdate => opaque = true,
                        RouteOperation::ReplaceConfiguration { .. } => {
                            if !event.conditional {
                                current.clear();
                            }
                            opaque = true;
                            configuration_replaced = true;
                        }
                        RouteOperation::Load => {
                            if opaque {
                                self.celery_route_issue(
                                    scope,
                                    event.at,
                                    "Celery task routing may use an unmodeled configuration",
                                );
                            }
                            for routes in &current {
                                self.retain_celery_routes(scope, event.at, routes);
                            }
                        }
                    }
                }
                if !self.failed_terminal_modules.contains(&scope) {
                    if scope != self.scopes[scope].file {
                        if opaque || !current.is_empty() {
                            self.celery_route_issue(scope, creation.at, "Celery task routes on a local app may escape their configuration scope");
                        }
                    } else if (opaque || !current.is_empty())
                        && exports
                            .entry(scope)
                            .or_insert_with(|| self.celery_module_apps(scope))
                            .contains(&object)
                    {
                        if opaque {
                            self.celery_route_issue(
                                scope,
                                creation.at,
                                "Celery task routing may use an unmodeled configuration",
                            );
                        }
                        for routes in current {
                            self.retain_celery_routes(scope, END, routes);
                        }
                    }
                }
            }
            self.celery_routes.0.insert(object, events);
        }
    }

    fn celery_module_apps(&mut self, scope: ScopeId) -> BTreeSet<Object> {
        let names: Vec<_> = self.scopes[scope].names.keys().copied().collect();
        let read = std::mem::take(&mut self.read);
        let previous = std::mem::replace(&mut self.probing_identity, true);
        let mut exports = BTreeSet::new();
        for name in names {
            let value = self.module_member_at(
                self.files[scope].module.as_str(),
                name,
                END,
                (scope, END),
                ModuleMemberAccess::Attribute,
                0,
            );
            if let Value::Alternatives(values) = value {
                exports.extend(values.into_iter().filter_map(celery_app_identity));
            } else {
                exports.extend(celery_app_identity(value));
            }
        }
        self.probing_identity = previous;
        self.read = read;
        exports
    }

    fn retain_celery_routes(&mut self, scope: ScopeId, at: Location, value: &Value) {
        match value {
            Value::Sequence(routes) => {
                for route in routes {
                    self.retain_celery_router(scope, at, route);
                }
            }
            Value::Alternatives(routes) => {
                for route in routes {
                    self.retain_celery_routes(scope, at, route);
                }
            }
            _ => self.retain_celery_router(scope, at, value),
        }
    }

    fn retain_celery_router(&mut self, scope: ScopeId, at: Location, value: &Value) {
        let mut resolved = None;
        let value = match value {
            Value::None => return,
            // Routing maps contain task selectors and destinations. A key
            // resembling a Python import path does not register a callable.
            Value::Mapping(items)
                if items.as_chunks::<2>().0.iter().all(|[key, destination]| {
                    matches!(key, Value::Text(_))
                        && matches!(destination, Value::Text(_) | Value::Mapping(_))
                }) =>
            {
                return;
            }
            Value::Text(name) => {
                if let Some((module, member)) =
                    name.rsplit_once(':').or_else(|| name.rsplit_once('.'))
                    && let Some(&file) = self.modules.get(module)
                {
                    resolved = Some(file);
                    let before = if file == self.scopes[scope].file {
                        at
                    } else {
                        END
                    };
                    self.module_member_at(
                        module,
                        member,
                        before,
                        (scope, at),
                        ModuleMemberAccess::FromImport,
                        0,
                    )
                } else {
                    Value::Unknown
                }
            }
            value => value.clone(),
        };
        if let Value::Callable(invocation) = value
            && let Binding::Definition(definition, body) = self.symbols[invocation.target].binding
            && definition.kind == DefinitionKind::Function
            && !definition.is_generator
            && !invocation.verified
        {
            if let Some(file) = resolved
                && file != self.scopes[scope].file
            {
                self.scopes[scope].imports.push(file);
            }
            let supplied = 4 + usize::from(matches!(invocation.receiver, Receiver::Bound(_)));
            if let Some(parameters) = router_parameters(definition, supplied) {
                self.invoke(scope, invocation, ROUTER, "possible", at);
                for parameter in parameters {
                    let parameter = &definition.parameters[parameter];
                    let target = self.by_location[&(body, parameter.at.line, parameter.at.column)];
                    self.edge(scope, target, ROUTER_PARAMETER, "possible");
                }
            }
            return;
        }
        self.celery_route_issue(
            scope,
            at,
            "Celery task routing could not resolve the configured router",
        );
    }

    fn celery_route_issue(&mut self, scope: ScopeId, at: Location, message: &str) {
        let at = if at.line == END.line {
            Location { line: 1, column: 1 }
        } else {
            at
        };
        self.scope_issue(scope, at, "unresolved_registration", message);
    }
}

fn celery_app_identity(value: Value) -> Option<Object> {
    match value {
        Value::Object(object, ObjectKind::CeleryApp) => Some(object),
        _ => None,
    }
}

fn router_parameters(definition: &Definition, supplied: usize) -> Option<Vec<usize>> {
    let positional = definition.positional_parameter_count;
    let variadic_positional = definition.has_variadic_positional_parameter;
    let keyword_only_start = positional + usize::from(variadic_positional);
    let keyword_only_stop =
        definition.parameters.len() - usize::from(definition.has_variadic_keyword_parameter);
    let task = (definition.positional_only_parameter_count..positional)
        .chain(keyword_only_start..keyword_only_stop)
        .find(|&index| definition.parameters[index].text == "task");
    if supplied > positional && !variadic_positional
        || task.is_none() && !definition.has_variadic_keyword_parameter
        || task.is_some_and(|index| index < positional.min(supplied))
        || (supplied..definition.required_positional_parameter_count)
            .any(|index| Some(index) != task)
        || definition
            .required_keyword_only_parameters
            .iter()
            .any(|&index| Some(keyword_only_start + index) != task)
    {
        return None;
    }
    let mut slots: Vec<_> = (0..positional.min(supplied)).collect();
    if supplied > positional {
        slots.push(positional);
    }
    slots.extend(task);
    if definition.has_variadic_keyword_parameter {
        slots.push(keyword_only_stop);
    }
    Some(slots)
}
