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
    postponed_signatures: Vec<(ScopeId, Site, SymbolId)>,
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
    // Proven execution prefixes before this inclusion. Completed imports use END.
    prior_scopes: BTreeMap<ScopeId, Location>,
}

#[derive(Default)]
struct Snapshots {
    visited: BTreeMap<(Object, Option<usize>), bool>,
    unordered_mounts: BTreeSet<usize>,
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
        self.fastapi
            .postponed_signatures
            .push((effects, site, target));
        let targets = self.fastapi_options(effects, site, keywords);
        self.queue_fastapi_signatures(effects, site, targets);
        self.fastapi_event(object, site, effects, None);
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
        self.queue_fastapi_signatures(effects, site, targets);
        self.fastapi_event(parent, site, effects, Some(child));
    }

    fn fastapi_event(
        &mut self,
        object: Object,
        site: Site,
        effects: ScopeId,
        included_router: Option<Object>,
    ) {
        let id = self.fastapi.events.len();
        self.fastapi.events.push(Event {
            site,
            effects,
            included_router,
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

    fn queue_fastapi_signatures(&mut self, effects: ScopeId, site: Site, targets: Vec<SymbolId>) {
        self.fastapi
            .postponed_signatures
            .extend(targets.into_iter().map(|target| (effects, site, target)));
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
                let event = &self.fastapi.events[id];
                self.fastapi.postponed_signatures.extend(
                    defaults
                        .iter()
                        .map(|&target| (event.effects, event.site, target)),
                );
            }
        }
        let mut visited = BTreeSet::new();
        let mut previous_scope = None;
        for (effects, site, target) in std::mem::take(&mut self.fastapi.postponed_signatures) {
            if previous_scope != Some(effects) {
                visited.clear();
                previous_scope = Some(effects);
            }
            self.fastapi_deferred_parameters(effects, site, target, &mut visited);
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
        reach.fastapi_limited |= snapshots.limited;
    }

    fn fastapi_snapshot(
        &self,
        object: Object,
        mount: Option<usize>,
        reach: &mut Reachability,
        pending: &mut VecDeque<Edge>,
        snapshots: &mut Snapshots,
        depth: usize,
    ) -> bool {
        let key = (object, mount);
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
            if let Some(cutoff) = mount {
                let inclusion = &self.fastapi.events[cutoff];
                if !inclusion
                    .prior_scopes
                    .get(&event.site.scope)
                    .is_some_and(|&before| position(event.site.at) < position(before))
                {
                    if event.site.scope != inclusion.site.scope {
                        snapshots.unordered_mounts.insert(cutoff);
                    }
                    continue;
                }
            }
            // include_router copies only the routes present at this call. Its
            // dependencies apply once at least one route is copied.
            if event.included_router.is_none_or(|child| {
                self.fastapi_snapshot(child, Some(id), reach, pending, snapshots, depth + 1)
            }) {
                has_routes = true;
                self.open_fastapi_effects(event.effects, reach, pending);
            }
        }
        if has_routes && let Some(&scope) = self.fastapi.router_defaults.get(&object) {
            self.open_fastapi_effects(scope, reach, pending);
        }
        snapshots.visited.insert(key, has_routes);
        has_routes
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
    }
}
