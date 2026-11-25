//! FastAPI registration effects and the router snapshots copied by include_router.

use super::*;

const MAX_SNAPSHOTS: usize = 16_384;

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
    startup_or_shutdown: bool,
    iterated: bool,
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
}

#[derive(Default)]
struct Snapshots {
    visited: BTreeMap<(Object, Option<Inclusion>), bool>,
    unordered_mounts: BTreeSet<usize>,
    version_sensitive_mounts: BTreeSet<usize>,
    limited: bool,
}

impl Graph<'_> {
    fn fastapi_effect_scope(&mut self, site: Site) -> ScopeId {
        self.push_scope(self.scopes[site.scope].file, None, None, &[])
    }

    pub(super) fn fastapi_app(&mut self, object: Object, scope: ScopeId) {
        self.fastapi.apps.insert(object, scope);
    }

    pub(super) fn fastapi_lifespan(&mut self, object: Object, site: Site, expression: &Expression) {
        let effects = self.fastapi_effect_scope(site);
        match self.identity(site.scope, expression, site.before, 0) {
            Value::Callable(invocation) if self.is_fastapi_lifespan(invocation.target) => {
                self.edge(effects, invocation.target, "fastapi_lifespan", "certain");
            }
            _ => self.scope_issue(
                effects,
                expression.at,
                "unresolved_registration",
                "FastAPI lifespan must resolve to a supported async context manager",
            ),
        }
        self.fastapi.lifespans.insert(object, (site, effects));
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
        self.fastapi_callable_target(value, false)
    }

    pub(super) fn fastapi_dependency_target(&mut self, value: Value) -> Option<SymbolId> {
        self.fastapi_callable_target(value, true)
    }

    fn fastapi_callable_target(&mut self, value: Value, dependency: bool) -> Option<SymbolId> {
        let callable = match value {
            Value::Instance(class) => self.class_member(class, "__call__", 0)?,
            value => value,
        };
        let Value::Callable(invocation) = callable else {
            return None;
        };
        let symbol = &self.symbols[invocation.target];
        let Binding::Definition(definition, _) = symbol.binding else {
            return None;
        };
        let scope = symbol.scope;
        ((dependency || definition.kind != DefinitionKind::Class)
            && (dependency || !definition.is_generator)
            && definition.decorators.iter().all(|decorator| {
                let value = self.probe_identity(scope, decorator, definition.name.at);
                self.local_identity_decorator(value, decorator)
            }))
        .then_some(invocation.target)
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
        } else {
            self.unresolved_fastapi_middleware(site);
        }
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
        let slots = match kind {
            "fastapi_lifespan" => 1,
            "fastapi_exception_handler" | "fastapi_middleware" => 2,
            "fastapi_asgi_application" => 3,
            _ => return false,
        };
        let Binding::Definition(definition, scope) = self.symbols[target].binding else {
            return false;
        };
        if !definition.simple_positional_parameters {
            return false;
        }
        let receiver = usize::from(
            self.scopes[self.symbols[target].scope]
                .owner
                .is_some_and(|id| self.is_class(id)),
        );
        let parameters = definition.parameters.iter().take(slots + receiver);
        self.read.extend(
            parameters.map(|parameter| {
                self.by_location[&(scope, parameter.at.line, parameter.at.column)]
            }),
        );
        true
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

    pub(super) fn fastapi_route(
        &mut self,
        object: Object,
        target: SymbolId,
        call: CallArguments<'_>,
        method: &str,
    ) {
        self.record_fastapi_route(object, target, call, method, false);
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
            callback.target,
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
        target: SymbolId,
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
        self.edge(effects, target, kind, certainty);
        let mut event = Event::new(site, effects);
        event.iterated = iterated;
        if kind == "fastapi_registration" {
            event.signature_targets = self.fastapi_options(effects, site, keywords);
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
            self.fastapi
                .signatures
                .entry(target)
                .or_default()
                .entry(scope)
                .and_modify(|bounded| *bounded &= plain_path)
                .or_insert(plain_path);
        }
        event.startup_or_shutdown = method == "on_event";
        self.fastapi_event(object, event);
    }

    pub(super) fn fastapi_mount(
        &mut self,
        parent: Object,
        child: Object,
        site: Site,
        keywords: &[(String, Expression)],
    ) {
        let effects = self.fastapi_effect_scope(site);
        let mut event = Event::new(site, effects);
        event.included_router = Some(child);
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
                if !self.fastapi_event_precedes(id, inclusion.cutoff) {
                    continue;
                }
                if evaluations >= MAX_SNAPSHOTS {
                    break;
                }
                if self
                    .prepare_fastapi_signatures(id, SignatureSite::AppMount(inclusion.outer_mount))
                {
                    self.prepare_fastapi_signatures(id, SignatureSite::ModuleEnd);
                    evaluations += 1;
                }
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

    pub(super) fn activate_fastapi(&self, reach: &mut Reachability, pending: &mut VecDeque<Edge>) {
        let mut snapshots = Snapshots::default();
        for (&object, &scope) in &self.fastapi.apps {
            if reach.scopes.contains(&scope) {
                self.fastapi_snapshot(object, None, reach, pending, &mut snapshots, 0);
            }
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
        snapshots: &mut Snapshots,
        depth: usize,
    ) -> bool {
        let key = (object, inclusion);
        if !self.fastapi.observable_objects.contains(&object) {
            return false;
        }
        if let Some(&has_routes) = snapshots.visited.get(&key) {
            return has_routes;
        }
        if depth >= MAX_BINDING_CHAIN || snapshots.visited.len() >= MAX_SNAPSHOTS {
            snapshots.limited = true;
            return false;
        }
        if let Some(&(site, effects)) = self.fastapi.lifespans.get(&object)
            && reach.scopes.contains(&site.scope)
        {
            self.open_fastapi_effects(effects, reach, pending);
        }
        let mut has_routes = false;
        for &id in self.fastapi.by_object.get(&object).into_iter().flatten() {
            let event = &self.fastapi.events[id];
            if !reach.scopes.contains(&event.site.scope) {
                continue;
            }
            if event
                .included_router
                .is_some_and(|child| !self.fastapi.observable_objects.contains(&child))
            {
                continue;
            }
            if inclusion.is_none()
                && event.startup_or_shutdown
                && self.fastapi.lifespans.contains_key(&object)
            {
                continue;
            }
            if let Some(inclusion) = inclusion
                && !self.fastapi_event_precedes(id, inclusion.cutoff)
            {
                if event.site.scope != self.fastapi.events[inclusion.cutoff].site.scope {
                    snapshots.unordered_mounts.insert(inclusion.cutoff);
                } else {
                    snapshots.version_sensitive_mounts.insert(inclusion.cutoff);
                }
                continue;
            }
            // include_router copies only the routes present at this call. Its
            // dependencies apply once at least one route is copied.
            if event.included_router.is_none_or(|child| {
                let outer_mount = inclusion.map_or(id, |outer| outer.outer_mount);
                self.fastapi_snapshot(
                    child,
                    Some(Inclusion {
                        cutoff: id,
                        outer_mount,
                    }),
                    reach,
                    pending,
                    snapshots,
                    depth + 1,
                )
            }) {
                has_routes |= event.carries_routes();
                self.open_fastapi_effects(event.effects, reach, pending);
                self.open_fastapi_signature(event, inclusion, reach, pending, snapshots);
            }
        }
        if has_routes && let Some(&scope) = self.fastapi.defaults.get(&object) {
            self.open_fastapi_effects(scope, reach, pending);
        }
        snapshots.visited.insert(key, has_routes);
        has_routes
    }

    fn open_fastapi_signature(
        &self,
        event: &Event,
        inclusion: Option<Inclusion>,
        reach: &mut Reachability,
        pending: &mut VecDeque<Edge>,
        snapshots: &mut Snapshots,
    ) {
        let timing = inclusion.map_or(SignatureSite::Original, |outer| {
            SignatureSite::AppMount(outer.outer_mount)
        });
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
            let later = event.signatures[&SignatureSite::ModuleEnd];
            reach.scopes.insert(later);
            // FastAPI 0.137+ can build the included route context lazily.
            // Avoid asserting a dependency when copies and live routers differ.
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

    pub(super) fn report_fastapi_coverage(&mut self, reach: &Reachability) {
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
}

impl Event {
    fn new(site: Site, effects: ScopeId) -> Self {
        Self {
            site,
            effects,
            included_router: None,
            startup_or_shutdown: false,
            iterated: false,
            signature_targets: Vec::new(),
            signatures: BTreeMap::new(),
            prior_scopes: BTreeMap::new(),
        }
    }

    fn carries_routes(&self) -> bool {
        self.included_router.is_some() || !self.signature_targets.is_empty()
    }
}
