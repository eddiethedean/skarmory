//! FastAPI registration effects and the router snapshots copied by include_router.

use super::*;

const MAX_SNAPSHOTS: usize = 16_384;
const MAX_ROUTER_GRAPH_STEPS: usize = 8 * MAX_SNAPSHOTS;
pub(super) const MAX_INITIALIZER_CALL_EVENTS: usize = 20_000;

pub(super) fn is_app_constructor(provider: &str) -> bool {
    matches!(provider, "fastapi.FastAPI" | "fastapi.applications.FastAPI")
}

pub(super) fn is_supported_app_method(method: &str) -> bool {
    is_route(ObjectKind::App, method, true)
        || is_imperative_registration(ObjectKind::App, method)
        || matches!(
            method,
            "include_router" | "mount" | "middleware" | "add_middleware"
        )
}

#[derive(Default)]
pub(super) struct Fastapi {
    apps: BTreeMap<Object, ScopeId>,
    events: Vec<Event>,
    by_object: BTreeMap<Object, Vec<usize>>,
    defaults: BTreeMap<Object, ScopeId>,
    default_dependencies: BTreeMap<Object, Vec<SymbolId>>,
    constructor_effects: BTreeMap<SymbolId, ScopeId>,
    parameter_dependencies: BTreeMap<SymbolId, Vec<SymbolId>>,
    injected_parameters: BTreeMap<SymbolId, BTreeSet<SymbolId>>,
    signatures: BTreeMap<SymbolId, BTreeMap<ScopeId, bool>>,
    lifespans: BTreeMap<Object, (Site, ScopeId)>,
    observable_objects: BTreeSet<Object>,
    limited_initializers: BTreeSet<ScopeId>,
    #[cfg(test)]
    initializer_call_events: usize,
    plain_query_owners: BTreeSet<Object>,
    parameter_routes: Vec<ParameterRoute>,
    pub(super) route_declarations: BTreeSet<SymbolId>,
}

#[derive(Debug)]
struct RouterGraphLimit;

struct ActiveRouterGraph {
    roots: Vec<Object>,
    events: BTreeMap<Object, Vec<usize>>,
}

impl Fastapi {
    fn active_router_graph(
        &self,
        reached_scopes: &BTreeSet<ScopeId>,
        limit: usize,
    ) -> Result<ActiveRouterGraph, RouterGraphLimit> {
        let roots: Vec<_> = self
            .apps
            .iter()
            .filter_map(|(&object, scope)| {
                (reached_scopes.contains(scope)
                    && self.observable_objects.contains(&object)
                    && !self.limited_initializers.contains(scope))
                .then_some(object)
            })
            .collect();
        let mut pending = roots.clone();
        let mut discovered: BTreeSet<_> = roots.iter().copied().collect();
        let mut events = BTreeMap::new();
        let mut remaining = limit;
        let mut charge = || {
            remaining = remaining.checked_sub(1).ok_or(RouterGraphLimit)?;
            Ok(())
        };

        while let Some(object) = pending.pop() {
            charge()?;
            let mut eligible = Vec::new();
            for &id in self.by_object.get(&object).into_iter().flatten() {
                charge()?;
                let event = &self.events[id];
                if !reached_scopes.contains(&event.site.scope) {
                    continue;
                }
                if let Some(child) = event.included_router {
                    if !self.observable_objects.contains(&child) {
                        continue;
                    }
                    if discovered.insert(child) {
                        pending.push(child);
                    }
                }
                eligible.push(id);
            }
            events.insert(object, eligible);
        }
        Ok(ActiveRouterGraph { roots, events })
    }
}

struct ParameterRoute {
    owner: SymbolId,
    scope: ScopeId,
    parameter: String,
    target: SymbolId,
    path: Expression,
    method: String,
    at: Location,
}

#[derive(Clone, Copy)]
pub(super) struct Site {
    pub(super) scope: ScopeId,
    pub(super) at: Location,
    pub(super) before: Location,
}

struct Event {
    site: Site,
    effects: ScopeId,
    included_router: Option<Object>,
    conditional_include: bool,
    startup_or_shutdown: bool,
    iterated: bool,
    api_route: bool,
    websocket_query_target: Option<SymbolId>,
    plain_query_binding: bool,
    signature_targets: Vec<SymbolId>,
    signatures: BTreeMap<SignatureSite, ScopeId>,
    // Proven execution prefixes before this inclusion. Completed imports use END.
    prior_scopes: BTreeMap<ScopeId, Location>,
}

#[derive(Clone, Copy, Eq, Ord, PartialEq, PartialOrd)]
enum SignatureSite {
    Original,
    AppMount(usize),
    ModuleEnd,
}

#[derive(Clone, Copy, Eq, Ord, PartialEq, PartialOrd)]
struct Inclusion {
    cutoff: usize,
    outer_mount: usize,
    legacy_default_reached: bool,
    lifecycle_reached: bool,
    conditional: bool,
    plain_query_binding: bool,
    mode: FastapiRouterMode,
}

struct Snapshots<'a> {
    events: &'a BTreeMap<Object, Vec<usize>>,
    visited: BTreeMap<(Object, Option<Inclusion>), Option<bool>>,
    remaining: usize,
    unordered_mounts: BTreeSet<usize>,
    version_sensitive_mounts: BTreeSet<usize>,
    limited: bool,
}

impl<'a> Snapshots<'a> {
    fn new(events: &'a BTreeMap<Object, Vec<usize>>) -> Self {
        Self {
            events,
            visited: BTreeMap::new(),
            remaining: MAX_ROUTER_GRAPH_STEPS,
            unordered_mounts: BTreeSet::new(),
            version_sensitive_mounts: BTreeSet::new(),
            limited: false,
        }
    }
}

enum DecoratorLayer<'a> {
    Route {
        function: &'a Expression,
        path: &'a Expression,
        at: Location,
        object: Object,
        method: String,
    },
    Replace {
        expression: &'a Expression,
        invocation: Invocation,
        replacement: Value,
    },
}

impl<'a> Graph<'a> {
    pub(super) fn bound_fastapi_initializers(&mut self, limit: usize) {
        let initializers: BTreeSet<_> = self
            .fastapi
            .apps
            .iter()
            .filter_map(|(object, &scope)| {
                self.fastapi
                    .observable_objects
                    .contains(object)
                    .then_some(scope)
            })
            .collect();
        if initializers.is_empty() {
            return;
        }
        let mut calls = BTreeMap::<ScopeId, Vec<ScopeId>>::new();
        for (&target, callers) in &self.direct_arguments {
            let Binding::Definition(definition, body) = self.symbols[target].binding else {
                continue;
            };
            if definition.kind != DefinitionKind::Function || definition.is_generator {
                continue;
            }
            for caller in callers
                .iter()
                .filter(|caller| caller.arguments_bound && !caller.verified && !caller.conditional)
            {
                calls.entry(caller.caller).or_default().push(body);
            }
        }
        let mut remaining = limit;
        for scope in initializers {
            if !initializer_call_events(&calls, scope, &mut remaining) {
                self.fastapi.limited_initializers.insert(scope);
            }
        }
        #[cfg(test)]
        {
            self.fastapi.initializer_call_events = limit - remaining;
        }
    }

    pub(super) fn terminal_fastapi_shadowed_import(
        &mut self,
        scope: ScopeId,
        statement: &Statement,
    ) -> bool {
        let StatementKind::Import(import) = &statement.kind else {
            return false;
        };
        if import.level != 0 || !self.modules.contains_key(import.module.as_str()) {
            return false;
        }
        let Cow::Owned(selected) = self.nested_packages.resolve(scope, &import.module) else {
            return false;
        };
        let Some((parent, child)) = selected.rsplit_once('.') else {
            return false;
        };
        let Some(&package) = self.modules.get(parent) else {
            return false;
        };
        if !self.files[package].is_package
            || self.files[scope]
                .module
                .rsplit_once('.')
                .map(|(name, _)| name)
                != Some(parent)
            || self.modules.contains_key(selected.as_str())
            || !self.scopes[package].body.iter().all(|row| {
                matches!(
                    row.kind,
                    StatementKind::Evaluate(Expression {
                        kind: ExpressionKind::Text(_),
                        ..
                    })
                )
            })
            || self.discovery.is_none_or(|project| {
                !project.direct_submodule_absent(&self.files[package].path, child, "fastapi")
            })
        {
            return false;
        }
        let [provider, initializer, importing, ..] = self.scopes[scope].body else {
            return false;
        };
        if !std::ptr::eq(importing, statement)
            || !matches!(&provider.kind, StatementKind::Import(import)
                if import.level == 0 && matches!(import.module.as_str(), "fastapi" | "fastapi.applications"))
        {
            return false;
        }
        let StatementKind::Assign {
            names,
            value:
                Some(Expression {
                    kind:
                        ExpressionKind::Call {
                            function,
                            arguments,
                            keywords,
                        },
                    ..
                }),
            annotation: None,
        } = &initializer.kind
        else {
            return false;
        };
        // This prefix cannot extend package search or publish the missing
        // submodule before the ordinary import reaches the selected package.
        names.len() == 1
            && arguments.is_empty()
            && keywords.is_empty()
            && matches!(self.probe_identity(scope, function, initializer.at),
                Value::Provider(provider) if is_app_constructor(&provider))
    }

    fn fastapi_effect_scope(&mut self, site: Site) -> ScopeId {
        self.push_scope(self.scopes[site.scope].file, None, None, &[])
    }

    pub(super) fn fastapi_app(&mut self, object: Object, scope: ScopeId) {
        self.fastapi.apps.insert(object, scope);
    }

    pub(super) fn collect_fastapi_deployment_factory(&mut self) {
        for file in 0..self.files.len() {
            let module = &self.files[file].module;
            if !matches!(module.as_str(), "app" | "main" | "asgi")
                || self.failed_terminal_modules.contains(&file)
                || self
                    .module_member_write(module, "create_app", (file, END))
                    .is_some()
            {
                continue;
            }
            let Some(factory) = self.local(file, "create_app", END) else {
                continue;
            };
            let Binding::Definition(definition, body) = self.symbols[factory].binding else {
                continue;
            };
            if self.conditional.contains_key(&factory)
                || definition.kind != DefinitionKind::Function
                || definition.is_generator
                || !definition.decorators.is_empty()
                || !definition.parameters.is_empty()
            {
                continue;
            }
            let [
                Statement {
                    kind: StatementKind::Assign { names, .. },
                    ..
                },
                callbacks @ ..,
                Statement {
                    at,
                    kind: StatementKind::Return(Some(value)),
                    ..
                },
            ] = body_without_docstring(definition)
            else {
                continue;
            };
            let [application] = names.as_slice() else {
                continue;
            };
            if !matches!(&value.kind, ExpressionKind::Name(name) if name == &application.text)
                || !callbacks.iter().all(|statement| {
                    matches!(&statement.kind, StatementKind::Define(callback)
                        if callback.kind != DefinitionKind::Class)
                })
            {
                continue;
            }
            let Value::Object(object, ObjectKind::App) = self.probe_identity(body, value, *at)
            else {
                continue;
            };
            // The deployment may name the factory. Only the returned app can
            // become active through that path.
            let mut apps = self
                .fastapi
                .apps
                .iter()
                .filter(|(_, scope)| **scope == body);
            if matches!((apps.next(), apps.next()), (Some((&app, _)), None) if app == object) {
                self.framework_roots.push(Edge {
                    target: factory,
                    kind: "fastapi_deployment_factory",
                    certainty: "possible",
                });
            }
        }
    }

    pub(super) fn fastapi_direct_subclass_scope(
        &mut self,
        class: SymbolId,
        depth: usize,
    ) -> Option<ScopeId> {
        let symbol = &self.symbols[class];
        let Binding::Definition(definition, scope) = symbol.binding else {
            return None;
        };
        let [base] = definition.bases.as_slice() else {
            return None;
        };
        (depth < MAX_BINDING_CHAIN
            && definition.kind == DefinitionKind::Class
            && definition.decorators.is_empty()
            && !definition.has_class_keywords()
            && matches!(self.class_base(symbol.scope, base, definition.name.at, depth + 1),
                Value::Provider(provider) if is_app_constructor(&provider)))
        .then_some(scope)
    }

    pub(super) fn fastapi_class_factory(
        &mut self,
        class: SymbolId,
        method: SymbolId,
        depth: usize,
    ) -> bool {
        let Some(class_scope) = self.fastapi_direct_subclass_scope(class, depth + 1) else {
            return false;
        };
        let Binding::Definition(definition, _) = self.symbols[method].binding else {
            return false;
        };
        let [decorator] = definition.decorators.as_slice() else {
            return false;
        };
        self.symbols[method].scope == class_scope
            && definition.kind == DefinitionKind::Function
            && !definition.is_generator
            && definition.simple_positional_parameters
            && definition.parameters.len() == 1
            && definition.defaults.is_empty()
            && matches!(body_without_docstring(definition), [Statement {
                kind: StatementKind::Return(Some(Expression {
                    kind: ExpressionKind::Call { function, arguments, keywords }, ..
                })), ..
            }] if arguments.is_empty() && keywords.is_empty()
                && matches!(&function.kind, ExpressionKind::Name(name) if name == &definition.parameters[0].text))
            && matches!(self.probe_identity(class_scope, decorator, definition.name.at),
                Value::Provider(name) if name == "builtins.classmethod")
    }

    pub(super) fn fastapi_class_factory_allocation(
        &mut self,
        scope: ScopeId,
        function: &Expression,
    ) -> bool {
        let (ExpressionKind::Name(name), Some(method)) = (&function.kind, self.scopes[scope].owner)
        else {
            return false;
        };
        let symbol = &self.symbols[method];
        let Some(class) = self.scopes[symbol.scope].owner else {
            return false;
        };
        let Binding::Definition(definition, _) = symbol.binding else {
            return false;
        };
        definition
            .parameters
            .first()
            .is_some_and(|parameter| &parameter.text == name)
            && self.fastapi_class_factory(class, method, 0)
            && self
                .fastapi_default_subclass_scope(class, (scope, function.at), 0)
                .is_some()
    }

    pub(super) fn fastapi_subclass_app(
        &mut self,
        invocation: Invocation,
        call: CallArguments<'_>,
        depth: usize,
    ) -> Option<Value> {
        let CallArguments {
            scope,
            before,
            at,
            positional,
            keywords,
        } = call;
        if invocation.verified
            || self.scopes[scope].owner.is_some()
            || !positional.is_empty()
            || !keywords.is_empty()
        {
            return None;
        }
        let class = match invocation.receiver {
            Receiver::Ordinary if self.is_class(invocation.target) => invocation.target,
            Receiver::Bound(class)
                if self.fastapi_class_factory(class, invocation.target, depth) =>
            {
                class
            }
            _ => return None,
        };
        self.fastapi_default_subclass_scope(class, (scope, before), depth)?;
        Some(Value::FastapiSubclassApp(
            (self.scopes[scope].file, at.line, at.column),
            class,
        ))
    }

    fn fastapi_default_subclass_scope(
        &mut self,
        class: SymbolId,
        access: (ScopeId, Location),
        depth: usize,
    ) -> Option<ScopeId> {
        let class_scope = self.fastapi_direct_subclass_scope(class, depth)?;
        if [
            "__new__",
            "__init__",
            "__getattr__",
            "__getattribute__",
            "__slots__",
        ]
        .into_iter()
        .any(|name| {
            self.local(class_scope, name, END).is_some()
                || self.class_member_write(class, name, access).is_some()
                || self.fastapi_base_member_changed(name, access)
        }) {
            return None;
        }
        Some(class_scope)
    }

    pub(super) fn fastapi_subclass_member(
        &mut self,
        object: Object,
        class: SymbolId,
        name: &str,
        access: (ScopeId, Location),
        depth: usize,
    ) -> Value {
        if ["__getattr__", "__getattribute__"]
            .into_iter()
            .any(|attribute| {
                self.class_member_write(class, attribute, access).is_some()
                    || self.fastapi_base_member_changed(attribute, access)
            })
        {
            self.scope_issue(
                access.0,
                access.1,
                "unresolved_attribute_write",
                "FastAPI subclass attribute lookup depends on a replaced accessor",
            );
            return Value::Unknown;
        }
        match self.fastapi_member_write(object, name, access) {
            Some(mutations::MemberWriteValue::Replacement(value)) => return value,
            Some(mutations::MemberWriteValue::Ambiguous) => {
                self.scope_issue(
                    access.0,
                    access.1,
                    "unresolved_attribute_write",
                    "FastAPI registration method depends on an unresolved write",
                );
                return Value::Unknown;
            }
            Some(mutations::MemberWriteValue::Deleted) | None => {}
        }
        match self.class_member_at(class, name, depth + 1, Some(access)) {
            Some(Value::FastapiInheritedMethod) => {
                Value::Method(object, ObjectKind::App, name.into())
            }
            Some(Value::Callable(invocation))
                if matches!(self.symbols[invocation.target].binding,
                Binding::Definition(definition, _) if definition.decorators.is_empty()
                    && self.scopes[self.symbols[invocation.target].scope].owner == Some(class)
                    && self.receiver_parameter(invocation.target).is_some()) =>
            {
                Value::Callable(invocation.bind_receiver(Receiver::Bound(class)))
            }
            _ => Value::Unknown,
        }
    }

    pub(super) fn fastapi_lifespan(
        &mut self,
        object: Object,
        site: Site,
        expression: &Expression,
    ) -> bool {
        let lifespan = self.identity(site.scope, expression, site.before, 0);
        if matches!(lifespan, Value::None) {
            return false;
        }
        let effects = self.fastapi_effect_scope(site);
        match lifespan {
            Value::Callable(invocation) if self.is_fastapi_lifespan(invocation.target) => {
                self.edge(effects, invocation.target, "fastapi_lifespan", "certain");
            }
            Value::Callable(invocation)
                if matches!(self.symbols[invocation.target].binding,
                    Binding::Definition(definition, _) if definition.kind == DefinitionKind::Coroutine
                        && !definition.is_generator && definition.decorators.is_empty()) =>
            {
                // Calling a plain async function only creates a coroutine.
                // The lifespan protocol requires an async context manager.
            }
            _ => self.scope_issue(
                effects,
                expression.at,
                "unresolved_registration",
                "FastAPI lifespan must resolve to a supported async context manager",
            ),
        }
        self.fastapi.lifespans.insert(object, (site, effects));
        true
    }

    pub(super) fn fastapi_router_lifecycle(
        &mut self,
        object: Object,
        site: Site,
        expression: &Expression,
    ) {
        if matches!(expression.kind, ExpressionKind::None) {
            return;
        }
        let callbacks = self.identity(site.scope, expression, site.before, 0);
        if matches!(&callbacks, Value::Sequence(values) if values.is_empty()) {
            return;
        }
        let effects = self.fastapi_effect_scope(site);
        let mut resolved = false;
        if let Value::Sequence(callbacks) = callbacks {
            resolved = true;
            for callback in callbacks {
                if matches!(callback, Value::NullaryConstantFunction) {
                    continue;
                }
                if let Some(invocation) = self.fastapi_callable(callback, false) {
                    let target = invocation.target;
                    let Binding::Definition(definition, _) = self.symbols[target].binding else {
                        unreachable!("FastAPI callable resolution only returns definitions");
                    };
                    let supplied = usize::from(
                        invocation.implicit_descriptor_receiver
                            || matches!(invocation.receiver, Receiver::Bound(_)),
                    );
                    if definition.required_positional_parameter_count > supplied
                        || definition.positional_parameter_count < supplied
                            && !definition.has_variadic_positional_parameter
                        || !definition.required_keyword_only_parameters.is_empty()
                    {
                        self.scope_issue(
                            effects,
                            expression.at,
                            "unresolved_registration",
                            "FastAPI router lifecycle callback must accept a call with zero arguments",
                        );
                        continue;
                    }
                    let certainty = if self.branch_depth == 0 {
                        "certain"
                    } else {
                        "possible"
                    };
                    self.edge(effects, target, "fastapi_lifecycle", certainty);
                } else {
                    resolved = false;
                }
            }
        }
        if !resolved {
            self.scope_issue(
                effects,
                expression.at,
                "unresolved_registration",
                "FastAPI router lifecycle callbacks require a bounded sequence",
            );
        }
        let mut event = Event::new(site, effects);
        event.startup_or_shutdown = true;
        self.fastapi_event(object, event);
    }

    pub(super) fn is_fastapi_lifespan(&mut self, target: SymbolId) -> bool {
        let symbol = &self.symbols[target];
        let Binding::Definition(definition, _) = symbol.binding else {
            return false;
        };
        definition.kind == DefinitionKind::Coroutine
            && definition.is_generator
            && matches!(definition.decorators.as_slice(), [decorator]
                if matches!(self.probe_identity(symbol.scope, decorator, definition.name.at),
                    Value::Provider(name) if name == "contextlib.asynccontextmanager"))
    }

    pub(super) fn fastapi_hook_target(&mut self, value: Value) -> Option<SymbolId> {
        self.fastapi_callable(value, false).map(|call| call.target)
    }

    pub(super) fn fastapi_dependency_target(&mut self, value: Value) -> Option<SymbolId> {
        self.fastapi_callable(value, true).map(|call| call.target)
    }

    fn fastapi_callable(&mut self, value: Value, dependency: bool) -> Option<Invocation> {
        let (callable, instance) = match value {
            Value::Instance(class) => (self.class_member(class, "__call__", 0)?, Some(class)),
            value => (value, None),
        };
        let Value::Callable(invocation) = callable else {
            return None;
        };
        let symbol = &self.symbols[invocation.target];
        let Binding::Definition(definition, _) = symbol.binding else {
            return None;
        };
        let scope = symbol.scope;
        let invocation = instance.map_or(invocation, |class| {
            invocation.bind_receiver(Receiver::Bound(class))
        });
        ((dependency || definition.kind != DefinitionKind::Class)
            && (dependency || !definition.is_generator)
            && definition.decorators.iter().all(|decorator| {
                let value = self.probe_identity(scope, decorator, definition.name.at);
                self.local_identity_decorator(value, decorator)
            }))
        .then_some(invocation)
    }

    pub(super) fn fastapi_middleware(
        &mut self,
        object: Object,
        site: Site,
        arguments: &[Expression],
        keywords: &[(String, Expression)],
    ) {
        if let Some(target) = self.fastapi_middleware_callback(
            site.scope,
            site.before,
            arguments,
            keywords,
            "middleware_class",
        ) {
            self.fastapi_route(
                object,
                target,
                CallArguments {
                    scope: site.scope,
                    before: site.before,
                    at: site.at,
                    positional: arguments,
                    keywords,
                },
                "add_middleware",
            );
        } else if let Some(targets) = self.fastapi_noop_middleware(site, arguments, keywords) {
            let effects = self.fastapi_effect_scope(site);
            for target in targets {
                self.edge(effects, target, "fastapi_source_middleware", "possible");
            }
            self.fastapi_event(object, Event::new(site, effects));
        } else {
            self.unresolved_fastapi_middleware(site);
        }
    }

    fn fastapi_noop_middleware(
        &mut self,
        site: Site,
        arguments: &[Expression],
        keywords: &[(String, Expression)],
    ) -> Option<Vec<SymbolId>> {
        let [Some(provider), Some(_)] =
            registration_arguments(arguments, keywords, ["middleware_class", "dispatch"])?
        else {
            return None;
        };
        let Value::Callable(invocation) = self.probe_identity(site.scope, provider, site.before)
        else {
            return None;
        };
        let Binding::Definition(definition, scope) = self.symbols[invocation.target].binding else {
            return None;
        };
        if invocation.verified
            || !matches!(invocation.receiver, Receiver::Ordinary)
            || definition.is_generator
            || !definition.decorators.is_empty()
            || !definition.annotations.is_empty()
            || !definition.defaults.is_empty()
        {
            return None;
        }
        let [first, second] = body_without_docstring(definition) else {
            return None;
        };
        // Starlette calls the supplied provider with the wrapped ASGI app and
        // middleware options. These source bodies cannot consume the dispatch option.
        let mut targets = vec![invocation.target];
        match (&first.kind, &second.kind) {
            (StatementKind::Define(init), StatementKind::Define(call))
                if self.is_class(invocation.target)
                    && definition.bases.is_empty()
                    && !definition.has_class_keywords()
                    && init.name.text == "__init__"
                    && call.name.text == "__call__"
                    && init.kind == DefinitionKind::Function
                    && call.kind == DefinitionKind::Coroutine
                    && plain_empty_middleware_body(init)
                    && plain_empty_middleware_body(call)
                    && self
                        .class_discovery_writes(invocation.target, site.scope)
                        .is_empty() =>
            {
                targets.push(self.local(scope, "__init__", END)?);
                targets.push(self.local(scope, "__call__", END)?);
            }
            (
                StatementKind::Define(call),
                StatementKind::Return(Some(Expression {
                    kind: ExpressionKind::Name(name),
                    ..
                })),
            ) if definition.kind == DefinitionKind::Function
                && &call.name.text == name
                && call.kind == DefinitionKind::Coroutine
                && plain_empty_middleware_body(call) =>
            {
                targets.push(self.local(scope, name, END)?);
            }
            _ => return None,
        }
        Some(targets)
    }

    pub(super) fn fastapi_middleware_callback(
        &mut self,
        scope: ScopeId,
        before: Location,
        arguments: &[Expression],
        keywords: &[(String, Expression)],
        class_name: &str,
    ) -> Option<SymbolId> {
        let [Some(provider), Some(dispatch)] =
            registration_arguments(arguments, keywords, [class_name, "dispatch"])?
        else {
            return None;
        };
        if !matches!(self.identity(scope, provider, before, 0),
            Value::Provider(name) if name == "starlette.middleware.base.BaseHTTPMiddleware")
        {
            return None;
        }
        let callback = self.identity(scope, dispatch, before, 0);
        self.fastapi_hook_target(callback)
    }

    pub(super) fn fastapi_middleware_list(
        &mut self,
        object: Object,
        site: Site,
        expr: &Expression,
    ) {
        let Value::Sequence(entries) = self.identity(site.scope, expr, site.before, 0) else {
            self.unresolved_fastapi_middleware(site);
            return;
        };
        for entry in entries {
            if let Value::FastapiMiddleware(Some(target)) = entry {
                self.fastapi_route(
                    object,
                    target,
                    CallArguments {
                        scope: site.scope,
                        before: site.before,
                        at: site.at,
                        positional: &[],
                        keywords: &[],
                    },
                    "add_middleware",
                );
            } else {
                self.unresolved_fastapi_middleware(site);
            }
        }
    }

    fn unresolved_fastapi_middleware(&mut self, site: Site) {
        self.scope_issue(
            site.scope,
            site.at,
            "unresolved_registration",
            "FastAPI middleware provider or dispatch callback could not be resolved",
        );
    }

    pub(super) fn retain_fastapi_callback_parameters(
        &mut self,
        target: SymbolId,
        kind: &str,
    ) -> bool {
        let Some((definition, scope, slots)) = self.fastapi_callback_parameter_slots(target, kind)
        else {
            return false;
        };
        self.read.extend(
            definition.parameters.iter().take(slots).map(|parameter| {
                self.by_location[&(scope, parameter.at.line, parameter.at.column)]
            }),
        );
        true
    }

    fn fastapi_callback_parameter_slots(
        &self,
        target: SymbolId,
        kind: &str,
    ) -> Option<(&'a Definition, ScopeId, usize)> {
        let slots = match kind {
            "fastapi_lifespan" => 1,
            "fastapi_exception_handler" | "fastapi_middleware" => 2,
            "fastapi_asgi_application" => 3,
            _ => return None,
        };
        let Binding::Definition(definition, scope) = self.symbols[target].binding else {
            return None;
        };
        if !definition.simple_positional_parameters {
            return None;
        }
        let receiver = usize::from(
            self.scopes[self.symbols[target].scope]
                .owner
                .is_some_and(|id| self.is_class(id)),
        );
        Some((definition, scope, slots + receiver))
    }

    #[cfg(test)]
    pub(super) fn fastapi_initializer_evidence(
        &self,
        reach: &Reachability,
    ) -> super::super::FastapiInitializerEvidence {
        let mut callbacks = BTreeSet::new();
        for &(target, kind, _) in &reach.evidence {
            if let Some((definition, scope, slots)) =
                self.fastapi_callback_parameter_slots(target, kind)
            {
                callbacks.extend(definition.parameters.iter().take(slots).map(|parameter| {
                    self.by_location[&(scope, parameter.at.line, parameter.at.column)]
                }));
            }
        }
        let injected_parameters = self
            .fastapi
            .injected_parameters
            .iter()
            .filter(|(owner, _)| {
                reach.fastapi_callbacks.contains(owner)
                    || self.fastapi.signatures.get(owner).is_some_and(|scopes| {
                        scopes.keys().any(|scope| reach.scopes.contains(scope))
                    })
            })
            .map(|(_, parameters)| parameters.len())
            .sum();
        super::super::FastapiInitializerEvidence {
            call_events: self.fastapi.initializer_call_events,
            limited: self
                .fastapi
                .limited_initializers
                .iter()
                .any(|scope| reach.scopes.contains(scope)),
            callback_parameters: callbacks.len(),
            injected_parameters,
        }
    }

    pub(super) fn fastapi_defaults(
        &mut self,
        object: Object,
        site: Site,
        keywords: &[(String, Expression)],
    ) {
        if self.fastapi.defaults.contains_key(&object)
            || !keywords.iter().any(|(name, _)| name == "dependencies")
        {
            return;
        }
        let effects = self.fastapi_effect_scope(site);
        self.fastapi.defaults.insert(object, effects);
        let defaults = self.fastapi_options(effects, site, keywords);
        if !defaults.is_empty() {
            self.fastapi.default_dependencies.insert(object, defaults);
        }
    }

    pub(super) fn record_fastapi_query_owner(
        &mut self,
        object: Object,
        site: Site,
        arguments: &[Expression],
        keywords: &[(String, Expression)],
        router: bool,
    ) {
        if arguments.is_empty()
            && (keywords.is_empty()
                || router
                    && matches!(keywords, [(name, prefix)] if name == "prefix"
                        && self.fastapi_plain_prefix(site, Some(prefix))))
        {
            self.fastapi.plain_query_owners.insert(object);
        }
    }

    fn fastapi_plain_prefix(&mut self, site: Site, prefix: Option<&Expression>) -> bool {
        prefix.is_none_or(|prefix| {
            matches!(self.probe_identity(site.scope, prefix, site.before),
                Value::Text(prefix) if prefix.is_empty()
                    || prefix.starts_with('/') && !prefix.ends_with('/') && !prefix.contains(['{', '}']))
        })
    }

    pub(super) fn fastapi_route(
        &mut self,
        object: Object,
        target: SymbolId,
        call: CallArguments<'_>,
        method: &str,
    ) {
        self.record_fastapi_route(object, Some(target), call, method, false);
    }

    pub(super) fn fastapi_conditional_app_result(
        &mut self,
        invocation: Invocation,
        arguments: &[Expression],
        keywords: &[(String, Expression)],
        depth: usize,
    ) -> Value {
        let Binding::Definition(definition, scope) = self.symbols[invocation.target].binding else {
            return Value::Unknown;
        };
        if depth >= MAX_BINDING_CHAIN
            || !matches!(invocation.receiver, Receiver::Ordinary)
            || definition.kind != DefinitionKind::Function
            || !definition.decorators.is_empty()
            || !definition.simple_positional_parameters
            || definition.parameters.len() != 1
            || arguments.len() != 1
            || !keywords.is_empty()
        {
            return Value::Unknown;
        }
        // Bound the alternatives to a single branch with two returns and, at
        // most, one preceding assignment for a shared application instance.
        let body = match body_without_docstring(definition) {
            [branch] => branch,
            [
                Statement {
                    kind: StatementKind::Assign { names, .. },
                    ..
                },
                branch,
            ] if names.len() == 1 => branch,
            _ => return Value::Unknown,
        };
        let StatementKind::If {
            test,
            body,
            otherwise,
        } = &body.kind
        else {
            return Value::Unknown;
        };
        if !matches!(&test.kind, ExpressionKind::Name(name) if *name == definition.parameters[0].text)
        {
            return Value::Unknown;
        }
        let mut values = Vec::new();
        for branch in [body, otherwise] {
            let [
                Statement {
                    kind: StatementKind::Return(Some(result)),
                    at,
                },
            ] = branch.as_slice()
            else {
                return Value::Unknown;
            };
            let value = self.identity(scope, result, *at, depth + 1);
            if !matches!(value, Value::Object(_, ObjectKind::App)) {
                return Value::Unknown;
            }
            values.push(value);
        }
        Value::alternatives(values)
    }

    pub(super) fn fastapi_exception_handlers(
        &mut self,
        objects: impl IntoIterator<Item = Object>,
        call: CallArguments<'_>,
        possible: bool,
    ) {
        let CallArguments {
            scope,
            before,
            at,
            positional,
            keywords,
        } = call;
        let target = callback_argument(
            ObjectKind::App,
            "add_exception_handler",
            positional,
            keywords,
        )
        .map(|expression| self.identity(scope, expression, before, 0))
        .and_then(|value| self.fastapi_hook_target(value));
        if let Some(target) = target {
            for object in objects {
                self.record_fastapi_route(
                    object,
                    Some(target),
                    CallArguments {
                        scope,
                        before,
                        at,
                        positional,
                        keywords,
                    },
                    "add_exception_handler",
                    possible,
                );
            }
        } else {
            self.scope_issue(
                scope,
                at,
                "unresolved_registration",
                "FastAPI exception handler could not be resolved",
            );
        }
    }

    pub(super) fn record_fastapi_parameter_route(
        &mut self,
        scope: ScopeId,
        decorator: &Expression,
        target: SymbolId,
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
        let ExpressionKind::Member(receiver, method) = &function.kind else {
            return false;
        };
        let [
            path @ Expression {
                kind: ExpressionKind::Text(text),
                ..
            },
        ] = arguments.as_slice()
        else {
            return false;
        };
        if !keywords.is_empty() || !text.starts_with('/') || !is_literal_api_decorator(method) {
            return false;
        }
        let Some(parameter) = self.parameter_binding(scope, receiver, before) else {
            return false;
        };
        let Some(owner) = self.scopes[scope].owner else {
            return false;
        };
        let Binding::Definition(wrapper, _) = self.symbols[owner].binding else {
            return false;
        };
        let Binding::Definition(endpoint, _) = self.symbols[target].binding else {
            return false;
        };
        // Only a direct definition runs at the wrapper call site. Defaults,
        // annotations and other decorators can execute code before mounting.
        if self.scopes[scope].parent != Some(self.scopes[scope].file)
            || wrapper.kind != DefinitionKind::Function
            || !wrapper.decorators.is_empty()
            || !wrapper.simple_positional_parameters
            || wrapper.parameters.len() != 1
            || !wrapper.defaults.is_empty()
            || !matches!(body_without_docstring(wrapper), [Statement { kind: StatementKind::Define(child), .. }]
                if position(child.name.at) == position(endpoint.name.at))
            || endpoint.kind == DefinitionKind::Class
            || endpoint.decorators.len() != 1
            || !endpoint.defaults.is_empty()
            || !endpoint.annotations.is_empty()
        {
            return false;
        }
        self.fastapi.parameter_routes.push(ParameterRoute {
            owner,
            scope,
            parameter: self.symbols[parameter].name.text.clone(),
            target,
            path: Expression {
                at: path.at,
                kind: ExpressionKind::Text(text.clone()),
            },
            method: method.clone(),
            at: decorator.at,
        });
        true
    }

    fn fastapi_parameter_receiver(
        &self,
        route: &ParameterRoute,
        call: &DirectArguments,
    ) -> Option<Object> {
        let Value::Object(object, ObjectKind::App | ObjectKind::Router) =
            call.values.get(&route.parameter)?
        else {
            return None;
        };
        (call.arguments_bound
            && matches!(
                self.fastapi_member_write(*object, &route.method, (call.caller, call.at)),
                None | Some(mutations::MemberWriteValue::Deleted)
            ))
        .then_some(*object)
    }

    pub(super) fn index_fastapi_parameter_routes(&mut self) {
        let routes = std::mem::take(&mut self.fastapi.parameter_routes);
        for route in &routes {
            let calls: Vec<_> = self
                .direct_arguments
                .get(&route.owner)
                .into_iter()
                .flatten()
                .filter_map(|call| {
                    self.fastapi_parameter_receiver(route, call)
                        .map(|object| (object, call.caller, call.at, call.conditional))
                })
                .collect();
            for (object, scope, at, conditional) in calls {
                self.record_fastapi_route(
                    object,
                    Some(route.target),
                    CallArguments {
                        scope,
                        before: at,
                        at,
                        positional: std::slice::from_ref(&route.path),
                        keywords: &[],
                    },
                    &route.method,
                    conditional,
                );
            }
        }
        self.fastapi.parameter_routes = routes;
    }

    pub(super) fn fastapi_decorated_value(
        &mut self,
        target: SymbolId,
        depth: usize,
    ) -> Option<Value> {
        self.fastapi_replacement_stack(target, depth)?
            .into_iter()
            .find_map(|layer| match layer {
                DecoratorLayer::Replace { replacement, .. } => Some(replacement),
                DecoratorLayer::Route { .. } => None,
            })
    }

    pub(super) fn apply_fastapi_replacement_stack(&mut self, target: SymbolId) -> bool {
        let Some(layers) = self.fastapi_replacement_stack(target, 0) else {
            return false;
        };
        let scope = self.symbols[target].scope;
        let before = self.symbols[target].name.at;
        // Python evaluates decorator expressions from top to bottom before
        // applying them in reverse. The bounded route factories have one
        // literal argument and produce a decorator without registering yet.
        for layer in &layers {
            match layer {
                DecoratorLayer::Route { function, path, .. } => {
                    self.walk(scope, function, before);
                    self.walk(scope, path, before);
                }
                DecoratorLayer::Replace { expression, .. } => {
                    self.walk(scope, expression, before);
                }
            }
        }
        let mut callback = Some(target);
        for layer in layers.into_iter().rev() {
            match layer {
                DecoratorLayer::Route {
                    path,
                    at,
                    object,
                    method,
                    ..
                } => {
                    self.record_fastapi_route(
                        object,
                        callback,
                        CallArguments {
                            scope,
                            before,
                            at,
                            positional: std::slice::from_ref(path),
                            keywords: &[],
                        },
                        &method,
                        false,
                    );
                }
                DecoratorLayer::Replace {
                    expression,
                    invocation,
                    replacement,
                } => {
                    self.invoke(scope, invocation, "decorator", "certain", expression.at);
                    callback = match replacement {
                        Value::Callable(invocation) => Some(invocation.target),
                        Value::NullaryConstantFunction => None,
                        _ => unreachable!("replacement proof supplies a supported callable"),
                    };
                }
            }
        }
        true
    }

    fn fastapi_replacement_stack(
        &mut self,
        target: SymbolId,
        depth: usize,
    ) -> Option<Vec<DecoratorLayer<'a>>> {
        let symbol = &self.symbols[target];
        let Binding::Definition(definition, _) = symbol.binding else {
            return None;
        };
        let scope = symbol.scope;
        let before = definition.name.at;
        if depth >= MAX_BINDING_CHAIN
            || self.scopes[scope].owner.is_some()
            || definition.decorators.len() < 2
            || !matches!(
                definition.kind,
                DefinitionKind::Function | DefinitionKind::Coroutine
            )
        {
            return None;
        }
        let mut stack = Vec::new();
        for decorator in &definition.decorators {
            match &decorator.kind {
                ExpressionKind::Name(name) => {
                    let binding = self.local(scope, name, before)?;
                    if self.binding_depends_on_branch(binding, before) {
                        return None;
                    }
                    let invocation = Invocation::ordinary(binding);
                    let replacement = self.local_replacement_decorator(invocation)?;
                    stack.push(DecoratorLayer::Replace {
                        expression: decorator,
                        invocation,
                        replacement,
                    });
                }
                ExpressionKind::Call {
                    function,
                    arguments,
                    keywords,
                } if keywords.is_empty()
                    && matches!(&function.kind, ExpressionKind::Member(base, _)
                            if matches!(base.kind, ExpressionKind::Name(_))) =>
                {
                    let [
                        path @ Expression {
                            kind: ExpressionKind::Text(text),
                            ..
                        },
                    ] = arguments.as_slice()
                    else {
                        return None;
                    };
                    if !text.starts_with('/') {
                        return None;
                    }
                    let previous = std::mem::replace(&mut self.probing_identity, true);
                    let callee = self.identity(scope, function, before, depth + 1);
                    self.probing_identity = previous;
                    let Value::Method(object, ObjectKind::App | ObjectKind::Router, method) =
                        callee
                    else {
                        return None;
                    };
                    if !is_literal_api_decorator(&method) {
                        return None;
                    }
                    stack.push(DecoratorLayer::Route {
                        function,
                        path,
                        object,
                        method,
                        at: decorator.at,
                    });
                }
                _ => return None,
            }
        }
        (stack
            .iter()
            .any(|layer| matches!(layer, DecoratorLayer::Route { .. }))
            && stack
                .iter()
                .any(|layer| matches!(layer, DecoratorLayer::Replace { .. })))
        .then_some(stack)
    }

    pub(super) fn fastapi_asgi_mount(&mut self, object: Object, call: CallArguments<'_>) {
        let CallArguments {
            scope,
            before,
            at,
            positional,
            keywords,
        } = call;
        if let Some([Some(path), Some(app), _]) =
            registration_arguments(positional, keywords, ["path", "app", "name"])
            && matches!(self.probe_identity(scope, path, before), Value::Text(path) if path.is_empty() || path.starts_with('/'))
        {
            let value = self.identity(scope, app, before, 0);
            if let Some(target) = self.fastapi_hook_target(value)
                && matches!(self.symbols[target].binding, Binding::Definition(definition, _)
                    if definition.kind == DefinitionKind::Coroutine
                        && definition.simple_positional_parameters
                        && definition.parameters.len() == 3)
                && self.scopes[self.symbols[target].scope]
                    .owner
                    .is_none_or(|owner| !self.is_class(owner))
            {
                self.fastapi_route(object, target, call, "mount");
                return;
            }
        }
        self.scope_issue(
            scope,
            at,
            "unresolved_registration",
            "FastAPI mount requires a supported ASGI application and literal path",
        );
    }

    pub(super) fn fastapi_immediate_route_lambda(
        &mut self,
        scope: ScopeId,
        body: &Expression,
        before: Location,
    ) -> bool {
        let ExpressionKind::Call {
            function,
            arguments,
            keywords,
        } = &body.kind
        else {
            return false;
        };
        // A nullary lambda inherits these simple lexical references outside a
        // class body and evaluates them at the point of its immediate call.
        if self.scopes[scope]
            .owner
            .is_some_and(|owner| self.is_class(owner))
            || !keywords.is_empty()
            || !matches!(&function.kind, ExpressionKind::Member(receiver, _)
                if matches!(receiver.kind, ExpressionKind::Name(_)))
            || !matches!(arguments.as_slice(), [Expression { kind: ExpressionKind::Text(path), .. },
                Expression { kind: ExpressionKind::Name(_), .. }] if path.starts_with('/'))
            || !matches!(self.probe_identity(scope, function, before),
                Value::Method(_, ObjectKind::App | ObjectKind::Router, method) if method == "add_api_route")
        {
            return false;
        }
        self.walk(scope, body, before);
        true
    }

    pub(super) fn fastapi_literal_route_loop(
        &mut self,
        scope: ScopeId,
        registration: &Expression,
    ) -> bool {
        let ExpressionKind::Call {
            function,
            arguments,
            keywords,
        } = &registration.kind
        else {
            return false;
        };
        let [_, endpoint] = arguments.as_slice() else {
            return false;
        };
        let at = registration.at;
        let Value::Method(object, ObjectKind::App | ObjectKind::Router, method) =
            self.call_target_identity(scope, function, at, 0)
        else {
            return false;
        };
        if method != "add_api_route" {
            return false;
        }
        let Value::Callable(callback) = self.identity(scope, endpoint, at, 0) else {
            return false;
        };
        if self.is_class(callback.target) {
            return false;
        }
        self.walk(scope, function, at);
        for argument in arguments {
            self.walk(scope, argument, at);
        }
        self.record_fastapi_route(
            object,
            Some(callback.target),
            CallArguments {
                scope,
                before: at,
                at,
                positional: arguments,
                keywords,
            },
            &method,
            true,
        );
        true
    }

    fn record_fastapi_route(
        &mut self,
        object: Object,
        target: Option<SymbolId>,
        call: CallArguments<'_>,
        method: &str,
        iterated: bool,
    ) {
        let CallArguments {
            scope,
            before,
            at,
            positional,
            keywords,
        } = call;
        let site = Site { scope, at, before };
        let effects = self.fastapi_effect_scope(site);
        let kind = match method {
            "on_event" => "fastapi_lifecycle",
            "mount" => "fastapi_asgi_application",
            "exception_handler" | "add_exception_handler" => "fastapi_exception_handler",
            "middleware" | "add_middleware" => "fastapi_middleware",
            _ => "fastapi_registration",
        };
        let certainty = if self.branch_depth == 0 && !iterated {
            "certain"
        } else {
            "possible"
        };
        if let Some(target) = target {
            self.edge(effects, target, kind, certainty);
        }
        let mut event = Event::new(site, effects);
        event.iterated = iterated;
        if kind == "fastapi_registration" {
            event.api_route = true;
            event.signature_targets = self.fastapi_options(effects, site, keywords);
            if let Some(target) = target {
                self.fastapi.route_declarations.insert(target);
                event.signature_targets.push(target);
                let path = positional.first().or_else(|| {
                    keywords
                        .iter()
                        .find_map(|(name, expr)| (name == "path").then_some(expr))
                });
                let plain_path = path.is_some_and(|expr| {
                    matches!(self.probe_identity(scope, expr, before),
                        Value::Text(path) if !path.contains('{'))
                });
                if self.fastapi_query_accept_sites(target).is_some() {
                    // Any other route registered for this callable can supply
                    // different inputs, even if its body looks the same.
                    event.websocket_query_target = Some(target);
                    event.plain_query_binding = method == "websocket"
                        && certainty == "certain"
                        && matches!(positional, [path] if keywords.is_empty()
                            && matches!(self.probe_identity(scope, path, before),
                                Value::Text(path) if path.starts_with('/') && !path.contains(['{', '}'])));
                }
                self.fastapi
                    .signatures
                    .entry(target)
                    .or_default()
                    .entry(scope)
                    .and_modify(|bounded| *bounded &= plain_path)
                    .or_insert(plain_path);
            }
        }
        event.startup_or_shutdown = method == "on_event";
        self.fastapi_event(object, event);
    }

    pub(super) fn fastapi_mount(
        &mut self,
        parent: Object,
        child: Object,
        site: Site,
        arguments: &[Expression],
        keywords: &[(String, Expression)],
    ) {
        let effects = self.fastapi_effect_scope(site);
        let mut event = Event::new(site, effects);
        event.included_router = Some(child);
        event.conditional_include = self.branch_depth != 0;
        event.plain_query_binding = arguments.len() <= 1
            && matches!(registration_arguments(arguments, keywords, ["router", "prefix"]),
                Some([Some(_), prefix]) if self.fastapi_plain_prefix(site, prefix));
        event.signature_targets = self.fastapi_options(effects, site, keywords);
        self.fastapi_event(parent, event);
    }

    fn fastapi_event(&mut self, object: Object, event: Event) {
        let id = self.fastapi.events.len();
        self.fastapi.events.push(event);
        self.fastapi.by_object.entry(object).or_default().push(id);
    }

    fn fastapi_options(
        &mut self,
        effects: ScopeId,
        site: Site,
        keywords: &[(String, Expression)],
    ) -> Vec<SymbolId> {
        let mut targets = Vec::new();
        if let Some((_, expr)) = keywords.iter().find(|(name, _)| name == "dependencies") {
            let mut resolved = true;
            if let Value::Sequence(values) = self.identity(site.scope, expr, site.before, 0) {
                for value in values {
                    if let Value::FastapiDependency(Some(target)) = value {
                        self.fastapi_dependency_edge(effects, target);
                        targets.push(target);
                    } else {
                        resolved = false;
                    }
                }
            } else {
                resolved = false;
            }
            if !resolved {
                self.scope_issue(
                    effects,
                    expr.at,
                    "unresolved_registration",
                    "FastAPI registration dependencies could not be resolved",
                );
            }
        }
        targets
    }

    pub(super) fn fastapi_dependency_edge(&mut self, effects: ScopeId, target: SymbolId) {
        self.prepare_fastapi_constructor(target);
        self.edge(effects, target, "fastapi_dependency", "certain");
    }

    pub(super) fn fastapi_parameter_dependency(&mut self, owner: SymbolId, target: SymbolId) {
        self.prepare_fastapi_constructor(target);
        self.fastapi
            .parameter_dependencies
            .entry(owner)
            .or_default()
            .push(target);
    }

    pub(super) fn fastapi_parameter_dependencies(&self, owner: SymbolId) -> &[SymbolId] {
        self.fastapi
            .parameter_dependencies
            .get(&owner)
            .map_or(&[], Vec::as_slice)
    }

    pub(super) fn fastapi_parameter_binding(
        &self,
        owner: SymbolId,
        at: Location,
    ) -> Option<SymbolId> {
        let Binding::Definition(definition, scope) = self.symbols[owner].binding else {
            return None;
        };
        let parameter = definition
            .parameters
            .iter()
            .rev()
            .find(|parameter| position(parameter.at) < position(at))?;
        Some(self.by_location[&(scope, parameter.at.line, parameter.at.column)])
    }

    pub(super) fn record_fastapi_injected_parameter(&mut self, owner: SymbolId, at: Location) {
        if let Some(parameter) = self.fastapi_parameter_binding(owner, at) {
            self.fastapi
                .injected_parameters
                .entry(owner)
                .or_default()
                .insert(parameter);
        }
    }

    pub(super) fn report_fastapi_signatures(&mut self, reach: &Reachability) -> BTreeSet<SymbolId> {
        let mut bounded = BTreeSet::new();
        for (&target, sites) in &self.fastapi.signatures {
            let mut reachable = sites
                .iter()
                .filter(|(scope, _)| reach.scopes.contains(scope))
                .peekable();
            if reachable.peek().is_some() {
                if reachable.all(|(_, bounded)| *bounded) {
                    bounded.insert(target);
                }
                self.read.extend(
                    self.fastapi
                        .injected_parameters
                        .get(&target)
                        .into_iter()
                        .flatten(),
                );
            }
        }
        for target in &reach.fastapi_callbacks {
            self.read.extend(
                self.fastapi
                    .injected_parameters
                    .get(target)
                    .into_iter()
                    .flatten(),
            );
        }
        bounded
    }

    pub(super) fn prepare_fastapi_constructor(&mut self, target: SymbolId) {
        if self.is_class(target) && !self.fastapi.constructor_effects.contains_key(&target) {
            let symbol = &self.symbols[target];
            let site = Site {
                scope: symbol.scope,
                at: symbol.name.at,
                before: symbol.name.at,
            };
            let effects = self.fastapi_effect_scope(site);
            self.fastapi.constructor_effects.insert(target, effects);
            self.constructor(effects, target, "fastapi_dependency", "certain", site.at);
        }
    }

    pub(super) fn fastapi_constructor_effects(&self, target: SymbolId) -> Option<ScopeId> {
        self.fastapi.constructor_effects.get(&target).copied()
    }

    fn fastapi_deferred_parameters(
        &mut self,
        effects: ScopeId,
        site: Site,
        target: SymbolId,
        visited: &mut BTreeSet<SymbolId>,
    ) {
        if visited.contains(&target) {
            return;
        }
        if visited.len() >= MAX_BINDING_CHAIN {
            self.scope_issue(
                effects,
                site.at,
                "analysis_limit",
                "FastAPI deferred dependency limit exceeded",
            );
            return;
        }
        visited.insert(target);
        if let Some(dependencies) = self.fastapi.parameter_dependencies.get(&target).cloned() {
            for dependency in dependencies {
                self.fastapi_deferred_parameters(effects, site, dependency, visited);
            }
        }
        let symbol = &self.symbols[target];
        let Binding::Definition(definition, _) = symbol.binding else {
            return;
        };
        let module = self.scopes[symbol.scope].file;
        if !self.files[module].facts.postponed_annotations {
            return;
        }
        // FastAPI evaluates a postponed signature in the callable's globals.
        let ordered = site.scope == module || self.scopes[site.scope].file != module;
        let before = if site.scope == module {
            site.before
        } else {
            END
        };
        for annotation in definition
            .annotations
            .iter()
            .take(definition.parameter_annotation_count)
        {
            match self.fastapi_annotation_metadata(module, before, annotation) {
                Value::FastapiAnnotated(Some(dependency)) if ordered => {
                    if let Some(parameter) = self.fastapi_parameter_binding(target, annotation.at) {
                        self.edge(effects, parameter, "fastapi_parameter", "certain");
                    }
                    self.fastapi_dependency_edge(effects, dependency);
                    self.fastapi_deferred_parameters(effects, site, dependency, visited);
                }
                Value::FastapiAnnotated(_) => self.scope_issue(
                    effects,
                    site.at,
                    "unresolved_registration",
                    "FastAPI postponed dependency could not be resolved at registration",
                ),
                _ => {}
            }
        }
    }

    pub(super) fn index_fastapi_mounts(&mut self) {
        let mut parents: BTreeMap<Object, Vec<Object>> = BTreeMap::new();
        let mut pending: Vec<_> = self.fastapi.lifespans.keys().copied().collect();
        for (&object, ids) in &self.fastapi.by_object {
            for &id in ids {
                if let Some(router) = self.fastapi.events[id].included_router {
                    parents.entry(router).or_default().push(object);
                } else {
                    pending.push(object);
                }
            }
        }
        while let Some(object) = pending.pop() {
            if self.fastapi.observable_objects.insert(object) {
                pending.extend(parents.remove(&object).into_iter().flatten());
            }
        }
        for (object, defaults) in &self.fastapi.default_dependencies {
            for &id in self.fastapi.by_object.get(object).into_iter().flatten() {
                let event = &mut self.fastapi.events[id];
                if event.carries_routes() {
                    event.signature_targets.extend(defaults);
                }
            }
        }
        for event in &mut self.fastapi.events {
            if event.included_router.is_none() {
                continue;
            }
            let mut prefix = Some((event.site.scope, event.site.at));
            let mut pending = Vec::new();
            while let Some((scope, before)) = prefix {
                pending.push((scope, before));
                let row = &self.scopes[scope];
                prefix = row
                    .parent
                    .zip(row.owner.map(|owner| self.symbols[owner].name.at));
            }
            while let Some((scope, before)) = pending.pop() {
                if event
                    .prior_scopes
                    .get(&scope)
                    .is_some_and(|&seen| position(seen) >= position(before))
                {
                    continue;
                }
                event.prior_scopes.insert(scope, before);
                for &(binding, module) in &self.scopes[scope].initializer_imports {
                    if position(self.symbols[binding].name.at) < position(before)
                        && !self.conditional.contains_key(&binding)
                    {
                        pending.push((module, END));
                    }
                }
            }
        }
        self.index_fastapi_signatures();
    }

    fn index_fastapi_signatures(&mut self) {
        for id in 0..self.fastapi.events.len() {
            self.prepare_fastapi_signatures(id, SignatureSite::Original);
        }

        // A copied route is reconstructed at each inclusion. Evaluate the
        // callable's postponed signature at the final app inclusion site,
        // using the intermediate inclusions only to determine route order.
        let mut pending = Vec::new();
        for object in self.fastapi.apps.keys() {
            for &mount in self.fastapi.by_object.get(object).into_iter().flatten() {
                if let Some(router) = self.fastapi.events[mount].included_router {
                    pending.push((
                        router,
                        Inclusion {
                            cutoff: mount,
                            outer_mount: mount,
                            legacy_default_reached: false,
                            lifecycle_reached: true,
                            conditional: false,
                            plain_query_binding: false,
                            mode: self.files[object.0].fastapi_router_mode,
                        },
                    ));
                }
            }
        }
        let mut visited = BTreeSet::new();
        let mut evaluations = 0;
        while let Some((object, inclusion)) = pending.pop() {
            if !self.fastapi.observable_objects.contains(&object) {
                continue;
            }
            if visited.len() >= MAX_SNAPSHOTS || evaluations >= MAX_SNAPSHOTS {
                break;
            }
            if !visited.insert((object, inclusion)) {
                continue;
            }
            let Some(events) = self.fastapi.by_object.get(&object).cloned() else {
                continue;
            };
            for id in events {
                if !(self.fastapi_event_precedes(id, inclusion.cutoff)
                    || inclusion.mode == FastapiRouterMode::Live
                        && self.fastapi_event_follows_in_initializer(id, inclusion.cutoff)
                        && self.fastapi.events[id].carries_routes())
                {
                    continue;
                }
                if evaluations >= MAX_SNAPSHOTS {
                    break;
                }
                let mut prepared = false;
                if inclusion.mode != FastapiRouterMode::Live {
                    prepared |= self.prepare_fastapi_signatures(
                        id,
                        SignatureSite::AppMount(inclusion.outer_mount),
                    );
                }
                if inclusion.mode != FastapiRouterMode::Snapshot {
                    prepared |= self.prepare_fastapi_signatures(id, SignatureSite::ModuleEnd);
                }
                evaluations += usize::from(prepared);
                if let Some(router) = self.fastapi.events[id].included_router {
                    pending.push((
                        router,
                        Inclusion {
                            cutoff: id,
                            ..inclusion
                        },
                    ));
                }
            }
        }
    }

    fn prepare_fastapi_signatures(&mut self, id: usize, timing: SignatureSite) -> bool {
        let event = &self.fastapi.events[id];
        if event.signature_targets.is_empty() || event.signatures.contains_key(&timing) {
            return false;
        }
        let site = match timing {
            SignatureSite::Original => event.site,
            SignatureSite::AppMount(mount) => self.fastapi.events[mount].site,
            SignatureSite::ModuleEnd => Site {
                before: END,
                ..event.site
            },
        };
        let targets = event.signature_targets.clone();
        let iterated = event.iterated;
        let effects = self.fastapi_effect_scope(site);
        let mut visited = BTreeSet::new();
        for target in targets {
            self.fastapi_deferred_parameters(effects, site, target, &mut visited);
        }
        if iterated {
            for edge in &mut self.scopes[effects].edges {
                edge.certainty = "possible";
            }
        }
        self.fastapi.events[id].signatures.insert(timing, effects);
        true
    }

    fn fastapi_event_precedes(&self, id: usize, mount: usize) -> bool {
        let event = &self.fastapi.events[id];
        self.fastapi.events[mount]
            .prior_scopes
            .get(&event.site.scope)
            .is_some_and(|&before| position(event.site.at) < position(before))
    }

    fn fastapi_event_follows_in_initializer(&self, id: usize, mount: usize) -> bool {
        let site = self.fastapi.events[id].site;
        let inclusion = self.fastapi.events[mount].site;
        site.scope == inclusion.scope
            && site.scope == self.scopes[site.scope].file
            && position(site.at) > position(inclusion.at)
    }

    pub(super) fn activate_fastapi(&self, reach: &mut Reachability, pending: &mut VecDeque<Edge>) {
        let Ok(graph) = self
            .fastapi
            .active_router_graph(&reach.scopes, MAX_ROUTER_GRAPH_STEPS)
        else {
            reach.fastapi_router_graph_limited = true;
            return;
        };
        let mut snapshots = Snapshots::new(&graph.events);
        for object in graph.roots {
            self.fastapi_snapshot(object, None, reach, pending, &mut snapshots, 0);
        }
        reach
            .unordered_fastapi_mounts
            .extend(snapshots.unordered_mounts);
        reach
            .version_sensitive_fastapi_mounts
            .extend(snapshots.version_sensitive_mounts);
        reach.fastapi_limited |= snapshots.limited;
    }

    fn fastapi_snapshot(
        &self,
        object: Object,
        inclusion: Option<Inclusion>,
        reach: &mut Reachability,
        pending: &mut VecDeque<Edge>,
        snapshots: &mut Snapshots<'_>,
        depth: usize,
    ) -> bool {
        let key = (object, inclusion);
        if let Some(&has_routes) = snapshots.visited.get(&key) {
            // A live router may lead back to an in-progress snapshot. Keep
            // that order unresolved and allow other eligible routes to run.
            snapshots.limited |= has_routes.is_none();
            return has_routes.unwrap_or(false);
        }
        if depth >= MAX_BINDING_CHAIN || snapshots.visited.len() >= MAX_SNAPSHOTS {
            snapshots.limited = true;
            return false;
        }
        snapshots.visited.insert(key, None);
        let lifecycle_reached = inclusion.is_none_or(|inclusion| inclusion.lifecycle_reached);
        let conditional = inclusion.is_some_and(|inclusion| inclusion.conditional);
        if lifecycle_reached
            && let Some(&(site, effects)) = self.fastapi.lifespans.get(&object)
            && reach.scopes.contains(&site.scope)
        {
            self.open_fastapi_lifecycle_effects(effects, conditional, reach, pending);
        }
        // The lifecycle lists and context are merged at inclusion, even when
        // dispatch consults the child router later.
        let legacy_default_reached = lifecycle_reached
            && (!self.fastapi.lifespans.contains_key(&object)
                || inclusion.is_some_and(|inclusion| inclusion.legacy_default_reached));
        let mode = inclusion.map_or(self.files[object.0].fastapi_router_mode, |item| item.mode);
        let plain_query_binding = self.fastapi.plain_query_owners.contains(&object)
            && inclusion.is_none_or(|inclusion| inclusion.plain_query_binding);
        let mut has_routes = false;
        for &id in snapshots.events.get(&object).into_iter().flatten() {
            let Some(remaining) = snapshots.remaining.checked_sub(1) else {
                snapshots.limited = true;
                break;
            };
            snapshots.remaining = remaining;
            let event = &self.fastapi.events[id];
            if event.startup_or_shutdown && !legacy_default_reached {
                continue;
            }
            let mut child_lifecycle = lifecycle_reached;
            if let Some(inclusion) = inclusion
                && !self.fastapi_event_precedes(id, inclusion.cutoff)
            {
                if self.fastapi_event_follows_in_initializer(id, inclusion.cutoff) {
                    if mode == FastapiRouterMode::Snapshot
                        || mode == FastapiRouterMode::Live && event.startup_or_shutdown
                    {
                        continue;
                    }
                    if mode == FastapiRouterMode::Live
                        && !conditional
                        && !event.conditional_include
                        && event.carries_routes()
                    {
                        child_lifecycle = false;
                    } else {
                        snapshots.version_sensitive_mounts.insert(inclusion.cutoff);
                        continue;
                    }
                } else {
                    if event.site.scope != self.fastapi.events[inclusion.cutoff].site.scope {
                        snapshots.unordered_mounts.insert(inclusion.cutoff);
                    } else {
                        snapshots.version_sensitive_mounts.insert(inclusion.cutoff);
                    }
                    continue;
                }
            }
            if event.included_router.is_none_or(|child| {
                let outer_mount = inclusion.map_or(id, |outer| outer.outer_mount);
                self.fastapi_snapshot(
                    child,
                    Some(Inclusion {
                        cutoff: id,
                        outer_mount,
                        legacy_default_reached,
                        lifecycle_reached: child_lifecycle,
                        conditional: conditional || event.conditional_include,
                        plain_query_binding: plain_query_binding && event.plain_query_binding,
                        mode,
                    }),
                    reach,
                    pending,
                    snapshots,
                    depth + 1,
                )
            }) {
                has_routes |= event.carries_routes();
                if let Some(target) = event.websocket_query_target {
                    let ordinary_query =
                        plain_query_binding && event.plain_query_binding && !conditional;
                    reach
                        .fastapi_websocket_query_bindings
                        .entry(target)
                        .and_modify(|bounded| *bounded &= ordinary_query)
                        .or_insert(ordinary_query);
                }
                if event.startup_or_shutdown {
                    self.open_fastapi_lifecycle_effects(event.effects, conditional, reach, pending);
                } else {
                    self.open_fastapi_effects(event.effects, reach, pending);
                }
                self.open_fastapi_signature(event, inclusion, reach, pending, snapshots);
            }
        }
        if has_routes && let Some(&scope) = self.fastapi.defaults.get(&object) {
            self.open_fastapi_effects(scope, reach, pending);
        }
        snapshots.visited.insert(key, Some(has_routes));
        has_routes
    }

    fn open_fastapi_signature(
        &self,
        event: &Event,
        inclusion: Option<Inclusion>,
        reach: &mut Reachability,
        pending: &mut VecDeque<Edge>,
        snapshots: &mut Snapshots<'_>,
    ) {
        let timing = match inclusion {
            Some(item) if item.mode == FastapiRouterMode::Live => SignatureSite::ModuleEnd,
            Some(item) => SignatureSite::AppMount(item.outer_mount),
            None => SignatureSite::Original,
        };
        let Some(&scope) = event.signatures.get(&timing) else {
            snapshots.limited |= !event.signature_targets.is_empty();
            return;
        };
        if let Some(inclusion) = inclusion {
            // Include the original signature's diagnostics. Its dependency
            // targets only apply to the original router.
            reach
                .scopes
                .insert(event.signatures[&SignatureSite::Original]);
            if inclusion.mode == FastapiRouterMode::Auto {
                let later = event.signatures[&SignatureSite::ModuleEnd];
                reach.scopes.insert(later);
                // Both models must agree when the framework mode is unknown.
                let targets = |scope: ScopeId| {
                    self.scopes[scope]
                        .edges
                        .iter()
                        .map(|edge| edge.target)
                        .collect::<BTreeSet<_>>()
                };
                if targets(scope) != targets(later) {
                    reach.scopes.insert(scope);
                    snapshots
                        .version_sensitive_mounts
                        .insert(inclusion.outer_mount);
                    return;
                }
            }
        }
        self.open_fastapi_effects(scope, reach, pending);
    }

    pub(super) fn open_fastapi_effects(
        &self,
        scope: ScopeId,
        reach: &mut Reachability,
        pending: &mut VecDeque<Edge>,
    ) {
        if reach.scopes.insert(scope) {
            pending.extend(&self.scopes[scope].edges);
        }
    }

    fn open_fastapi_lifecycle_effects(
        &self,
        scope: ScopeId,
        conditional: bool,
        reach: &mut Reachability,
        pending: &mut VecDeque<Edge>,
    ) {
        if reach.fastapi_lifecycle_effects.insert((scope, conditional)) {
            reach.scopes.insert(scope);
            pending.extend(self.scopes[scope].edges.iter().map(|edge| Edge {
                certainty: if conditional {
                    "possible"
                } else {
                    edge.certainty
                },
                ..*edge
            }));
        }
    }

    pub(super) fn report_fastapi_coverage(&mut self, reach: &Reachability) {
        for route in &self.fastapi.parameter_routes {
            if !reach.scopes.contains(&route.scope) {
                continue;
            }
            let mut calls = self
                .direct_arguments
                .get(&route.owner)
                .into_iter()
                .flatten()
                .filter(|call| reach.scopes.contains(&call.caller))
                .peekable();
            if calls.peek().is_none()
                || calls.any(|call| self.fastapi_parameter_receiver(route, call).is_none())
                || reach
                    .evidence
                    .iter()
                    .any(|(target, kind, _)| *target == route.owner && *kind != "direct_call")
            {
                self.issues.push(
                    Issue::new(
                        "unresolved_registration",
                        &self.files[self.scopes[route.scope].file].path,
                        "FastAPI registration parameter is unresolved for a reached wrapper call",
                    )
                    .at(route.at),
                );
            }
        }
        if reach.fastapi_router_graph_limited {
            self.issues.push(Issue::new(
                "analysis_limit",
                ".",
                "FastAPI router inclusion graph step budget exceeded",
            ));
        }
        for &scope in &self.fastapi.limited_initializers {
            if reach.scopes.contains(&scope) {
                self.issues.push(Issue::new(
                    "analysis_limit",
                    &self.files[self.scopes[scope].file].path,
                    "FastAPI initializer call-event budget exceeded",
                ));
            }
        }
        if reach.fastapi_limited {
            self.issues.push(Issue::new(
                "analysis_limit",
                ".",
                "FastAPI router snapshot limit exceeded",
            ));
        }
        for &id in &reach.unordered_fastapi_mounts {
            let site = self.fastapi.events[id].site;
            self.issue(
                self.scopes[site.scope].file,
                site.at,
                "unresolved_registration",
                "could not prove which FastAPI routes existed when the router was included",
            );
        }
        for &id in &reach.version_sensitive_fastapi_mounts {
            let site = self.fastapi.events[id].site;
            self.issue(
                self.scopes[site.scope].file,
                site.at,
                "unresolved_registration",
                "FastAPI router inclusion can select different routes or dependencies across framework versions",
            );
        }
    }

    pub(super) fn report_fastapi_websocket_query_failures(&mut self, reach: &Reachability) {
        if !self.issues.is_empty() {
            return;
        }
        let sites: BTreeMap<_, _> = reach
            .fastapi_websocket_query_bindings
            .iter()
            .filter_map(|(&target, &plain_query)| {
                (plain_query
                    && reach
                        .evidence
                        .contains(&(target, "fastapi_registration", "certain"))
                    && reach
                        .evidence
                        .range((target, "", "")..)
                        .take_while(|&&(id, _, _)| id == target)
                        .all(|&(_, kind, _)| {
                            matches!(kind, "fastapi_registration" | "type_reference")
                        }))
                .then(|| self.fastapi_query_accept_sites(target))
                .flatten()
            })
            .collect();
        if sites.is_empty() {
            return;
        }
        let known_failure = |scope, issue: &Issue| {
            sites.get(&scope).is_some_and(|&(call, awaited)| {
                let at = (issue.line, issue.column);
                match (issue.kind, issue.message.as_str()) {
                    ("unresolved_call", "call target is outside the bounded model") => {
                        at == (Some(call.line), Some(call.column))
                    }
                    ("unresolved_callback", "callback target could not be bounded") => {
                        at == (Some(awaited.line), Some(awaited.column))
                    }
                    _ => false,
                }
            })
        };
        // Unknown statements or mutations in the reached project can change
        // the callable's effective signature or how the app supplies its inputs.
        if self
            .scope_issues
            .iter()
            .any(|(scope, issue)| reach.scopes.contains(scope) && !known_failure(*scope, issue))
        {
            return;
        }
        self.scope_issues
            .retain(|(scope, issue)| !known_failure(*scope, issue));
    }

    fn fastapi_query_accept_sites(
        &self,
        target: SymbolId,
    ) -> Option<(ScopeId, (Location, Location))> {
        let symbol = &self.symbols[target];
        let Binding::Definition(definition, body) = symbol.binding else {
            return None;
        };
        let [parameter] = definition.parameters.as_slice() else {
            return None;
        };
        if symbol.scope != self.scopes[symbol.scope].file
            || definition.kind != DefinitionKind::Coroutine
            || definition.is_generator
            || !definition.simple_positional_parameters
            || !definition.defaults.is_empty()
            || !definition.annotations.is_empty()
            || definition.decorators.len() != 1
        {
            return None;
        }
        let [
            Statement {
                kind: StatementKind::Evaluate(awaited),
                ..
            },
        ] = body_without_docstring(definition)
        else {
            return None;
        };
        let ExpressionKind::Await(call) = &awaited.kind else {
            return None;
        };
        let ExpressionKind::Call {
            function,
            arguments,
            keywords,
        } = &call.kind
        else {
            return None;
        };
        let ExpressionKind::Member(receiver, member) = &function.kind else {
            return None;
        };
        // The required, unannotated input is the raw query string whenever
        // FastAPI enters the callback. str has no accept member. Evaluating
        // the call raises before the await can consume any other coroutine.
        (member == "accept"
            && arguments.is_empty()
            && keywords.is_empty()
            && self.parameter_binding(body, receiver, awaited.at)
                == Some(self.by_location[&(body, parameter.at.line, parameter.at.column)]))
        .then_some((body, (call.at, awaited.at)))
    }
}

fn initializer_call_events(
    calls: &BTreeMap<ScopeId, Vec<ScopeId>>,
    initializer: ScopeId,
    remaining: &mut usize,
) -> bool {
    let callees = |scope| {
        calls
            .get(&scope)
            .map_or([].iter(), |children| children.iter())
    };
    let mut active = BTreeSet::from([initializer]);
    let mut pending = vec![(initializer, callees(initializer))];
    while let Some((caller, children)) = pending.last_mut() {
        if let Some(&callee) = children.next() {
            if *remaining == 0 {
                return false;
            }
            *remaining -= 1;
            // Count repeated acyclic call paths, while a recursive back-edge
            // is already represented by its active ancestor in the graph.
            if active.insert(callee) {
                pending.push((callee, callees(callee)));
            }
        } else {
            active.remove(caller);
            pending.pop();
        }
    }
    true
}

fn plain_empty_middleware_body(definition: &Definition) -> bool {
    definition.decorators.is_empty()
        && definition.annotations.is_empty()
        && definition.defaults.is_empty()
        && body_without_docstring(definition).is_empty()
}

fn is_literal_api_decorator(method: &str) -> bool {
    matches!(
        method,
        "get" | "post" | "put" | "delete" | "patch" | "head" | "options" | "trace" | "api_route"
    )
}

impl Event {
    fn new(site: Site, effects: ScopeId) -> Self {
        Self {
            site,
            effects,
            included_router: None,
            conditional_include: false,
            startup_or_shutdown: false,
            iterated: false,
            api_route: false,
            websocket_query_target: None,
            plain_query_binding: false,
            signature_targets: Vec::new(),
            signatures: BTreeMap::new(),
            prior_scopes: BTreeMap::new(),
        }
    }

    fn carries_routes(&self) -> bool {
        self.included_router.is_some() || self.api_route
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initializer_call_budget_counts_repeated_acyclic_paths_and_stops_at_the_limit() {
        let calls = BTreeMap::from([(0, vec![1]), (1, vec![2, 2]), (2, vec![3, 3])]);
        let mut remaining = 6;
        assert!(!initializer_call_events(&calls, 0, &mut remaining));
        assert_eq!(remaining, 0);
        let mut remaining = 7;
        assert!(initializer_call_events(&calls, 0, &mut remaining));
        assert_eq!(remaining, 0);
    }

    #[derive(serde::Deserialize)]
    struct IncludeGraphFixture {
        roots: Vec<String>,
        edges: Vec<IncludeEdge>,
        limit: Option<usize>,
    }

    #[derive(serde::Deserialize)]
    struct IncludeEdge {
        receiver: String,
        child: String,
        owner: Option<String>,
    }

    #[test]
    #[ignore = "requires the pinned FastAPI graph fixture"]
    fn pinned_fastapi_include_graph() {
        let fixture = std::env::var_os("SKARMORY_PHASE03_FASTAPI_INCLUDE_FIXTURE").unwrap();
        let report_path = std::env::var_os("SKARMORY_PHASE03_FASTAPI_INCLUDE_REPORT").unwrap();
        let fixture: IncludeGraphFixture =
            serde_json::from_slice(&std::fs::read(fixture).unwrap()).unwrap();

        let names: BTreeSet<_> = fixture
            .roots
            .iter()
            .chain(
                fixture
                    .edges
                    .iter()
                    .flat_map(|edge| [&edge.receiver, &edge.child]),
            )
            .collect();
        let objects: BTreeMap<_, _> = names
            .into_iter()
            .enumerate()
            .map(|(index, name)| (name, (0, index + 1, 0)))
            .collect();
        let scopes: BTreeMap<_, _> = fixture
            .edges
            .iter()
            .filter_map(|edge| edge.owner.as_deref())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .enumerate()
            .map(|(index, owner)| (owner, index + 1))
            .collect();
        let mut fastapi = Fastapi {
            apps: fixture
                .roots
                .iter()
                .map(|name| (objects[name], 0))
                .collect(),
            observable_objects: objects.values().copied().collect(),
            ..Fastapi::default()
        };
        for edge in &fixture.edges {
            let scope = edge.owner.as_deref().map_or(0, |owner| scopes[owner]);
            let mut registration = Event::new(
                Site {
                    scope,
                    at: END,
                    before: END,
                },
                scope,
            );
            registration.included_router = Some(objects[&edge.child]);
            fastapi
                .by_object
                .entry(objects[&edge.receiver])
                .or_default()
                .push(fastapi.events.len());
            fastapi.events.push(registration);
        }

        let result = fastapi.active_router_graph(
            &BTreeSet::from([0]),
            fixture.limit.unwrap_or(MAX_ROUTER_GRAPH_STEPS),
        );
        let report = match result {
            Ok(graph) => {
                let reached: Vec<_> = objects
                    .iter()
                    .filter_map(|(&name, object)| {
                        (graph.events.contains_key(object) && !graph.roots.contains(object))
                            .then_some(name)
                    })
                    .collect();
                serde_json::json!({"status": "complete", "reachable": reached})
            }
            Err(RouterGraphLimit) => serde_json::json!({
                "status": "incomplete",
                "reason": "abstract_step_budget",
            }),
        };
        std::fs::write(report_path, serde_json::to_vec(&report).unwrap()).unwrap();
    }
}
