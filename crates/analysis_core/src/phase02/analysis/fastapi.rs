//! FastAPI registration effects and the router snapshots copied by include_router.

use super::*;

const MAX_SNAPSHOTS: usize = 16_384;

#[derive(Default)]
pub(super) struct Fastapi {
    apps: BTreeMap<Object, ScopeId>,
    events: Vec<Event>,
    by_object: BTreeMap<Object, Vec<usize>>,
    router_defaults: BTreeMap<Object, ScopeId>,
    default_dependencies: BTreeMap<Object, Vec<SymbolId>>,
    constructor_effects: BTreeMap<SymbolId, ScopeId>,
    parameter_dependencies: BTreeMap<SymbolId, Vec<SymbolId>>,
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

    pub(super) fn fastapi_app(&mut self, object: Object, scope: ScopeId, defaults: Vec<SymbolId>) {
        self.fastapi.apps.insert(object, scope);
        if !defaults.is_empty() {
            self.fastapi.default_dependencies.insert(object, defaults);
        }
    }

    pub(super) fn fastapi_router_defaults(
        &mut self,
        object: Object,
        site: Site,
        keywords: &[(String, Expression)],
    ) {
        if self.fastapi.router_defaults.contains_key(&object)
            || !keywords.iter().any(|(name, _)| name == "dependencies")
        {
            return;
        }
        let effects = self.fastapi_effect_scope(site);
        self.fastapi.router_defaults.insert(object, effects);
        let defaults = self.fastapi_options(effects, site, keywords);
        if !defaults.is_empty() {
            self.fastapi.default_dependencies.insert(object, defaults);
        }
    }

    pub(super) fn fastapi_route(
        &mut self,
        object: Object,
        site: Site,
        target: SymbolId,
        keywords: &[(String, Expression)],
    ) {
        let effects = self.fastapi_effect_scope(site);
        self.edge(effects, target, "fastapi_registration", "certain");
        let mut targets = self.fastapi_options(effects, site, keywords);
        targets.push(target);
        self.fastapi_event(object, site, effects, None, targets);
    }

    pub(super) fn fastapi_mount(
        &mut self,
        parent: Object,
        child: Object,
        site: Site,
        keywords: &[(String, Expression)],
    ) {
        let effects = self.fastapi_effect_scope(site);
        let targets = self.fastapi_options(effects, site, keywords);
        self.fastapi_event(parent, site, effects, Some(child), targets);
    }

    fn fastapi_event(
        &mut self,
        object: Object,
        site: Site,
        effects: ScopeId,
        included_router: Option<Object>,
        signature_targets: Vec<SymbolId>,
    ) {
        let id = self.fastapi.events.len();
        self.fastapi.events.push(Event {
            site,
            effects,
            included_router,
            signature_targets,
            signatures: BTreeMap::new(),
            prior_scopes: BTreeMap::new(),
        });
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
            for meta in self.fastapi_annotation_metadata(module, before, annotation) {
                match self.probe_identity(module, meta, before) {
                    Value::FastapiDependency(Some(dependency)) if ordered => {
                        self.fastapi_dependency_edge(effects, dependency);
                        self.fastapi_deferred_parameters(effects, site, dependency, visited);
                    }
                    Value::FastapiDependency(_) => self.scope_issue(
                        effects,
                        site.at,
                        "unresolved_registration",
                        "FastAPI postponed dependency could not be resolved at registration",
                    ),
                    _ => {}
                }
            }
        }
    }

    pub(super) fn index_fastapi_mounts(&mut self) {
        for (object, defaults) in &self.fastapi.default_dependencies {
            for &id in self.fastapi.by_object.get(object).into_iter().flatten() {
                self.fastapi.events[id].signature_targets.extend(defaults);
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
        let effects = self.fastapi_effect_scope(site);
        let mut visited = BTreeSet::new();
        for target in targets {
            self.fastapi_deferred_parameters(effects, site, target, &mut visited);
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
        if let Some(&has_routes) = snapshots.visited.get(&key) {
            return has_routes;
        }
        if depth >= MAX_BINDING_CHAIN || snapshots.visited.len() >= MAX_SNAPSHOTS {
            snapshots.limited = true;
            return false;
        }
        let mut has_routes = false;
        for &id in self.fastapi.by_object.get(&object).into_iter().flatten() {
            let event = &self.fastapi.events[id];
            if !reach.scopes.contains(&event.site.scope) {
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
                has_routes = true;
                self.open_fastapi_effects(event.effects, reach, pending);
                self.open_fastapi_signature(event, inclusion, reach, pending, snapshots);
            }
        }
        if has_routes && let Some(&scope) = self.fastapi.router_defaults.get(&object) {
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
