//! Bounded source-order callback rebinding during Django signal registration.

use super::*;

const MAX_EFFECTS: usize = 4096;

#[derive(Default)]
pub(super) struct Globals {
    slots: BTreeSet<SymbolId>,
    writers: BTreeMap<ScopeId, SymbolId>,
    events: BTreeMap<ScopeId, Vec<Event>>,
    reads: BTreeSet<(ScopeId, SymbolId, [usize; 2])>,
    registration_reads: BTreeSet<(ScopeId, SymbolId, [usize; 2])>,
    resolving_callback_read: Option<bool>,
    covered: BTreeSet<(ScopeId, SymbolId)>,
    proved_scopes: BTreeMap<SymbolId, BTreeSet<ScopeId>>,
}

#[derive(Clone, Copy)]
struct Event {
    at: Location,
    conditional: bool,
    kind: EventKind,
}

#[derive(Clone, Copy)]
enum EventKind {
    Call {
        body: ScopeId,
        target: SymbolId,
    },
    Set {
        slot: SymbolId,
        value: Option<Invocation>,
    },
    Initialize {
        slot: SymbolId,
        value: Option<Invocation>,
    },
    Connect(SymbolId),
}

struct Replay {
    value: Option<Invocation>,
    budget: usize,
    callbacks: Vec<(Location, Invocation)>,
    covered: BTreeSet<(ScopeId, SymbolId)>,
}

impl Graph<'_> {
    pub(super) fn record_django_global_initializer(
        &mut self,
        scope: ScopeId,
        names: &[Name],
        value: &Expression,
        at: Location,
    ) {
        if self.django_globals.slots.is_empty() || scope != self.scopes[scope].file {
            return;
        }
        let [name] = names else {
            return;
        };
        let slot = self.by_location[&(scope, name.at.line, name.at.column)];
        if self.django_globals.slots.contains(&slot) {
            let value = self.identity_with_dormant_declarations(scope, value, at, 0);
            let value = self.django_global_callback(value);
            self.django_global_event(scope, at, EventKind::Initialize { slot, value }, false);
        }
    }

    pub(super) fn index_django_global_writes(&mut self) {
        for scope in self.files.len()..self.scopes.len() {
            let row = &self.scopes[scope];
            let [
                Statement {
                    kind: StatementKind::SetGlobalName { name, .. },
                    ..
                },
            ] = row.body
            else {
                continue;
            };
            if row.parent != Some(row.file) {
                continue;
            }
            let Some(bindings) = self.scopes[row.file].names.get(name.text.as_str()) else {
                continue;
            };
            if let [slot] = bindings.as_slice()
                && matches!(self.symbols[*slot].binding, Binding::Variable(Some(_), _))
                && !self.binding_depends_on_branch(*slot, END)
            {
                self.django_globals.slots.insert(*slot);
                self.django_globals.writers.insert(scope, *slot);
            }
        }
    }

    pub(super) fn record_django_global_write(
        &mut self,
        scope: ScopeId,
        value: &Expression,
        at: Location,
    ) {
        if !self.phase03 {
            self.issue(
                self.scopes[scope].file,
                at,
                "unsupported_construct",
                "global write is outside this compatibility profile",
            );
            return;
        }
        self.walk(scope, value, at);
        let Some(&slot) = self.django_globals.writers.get(&scope) else {
            self.scope_issue(
                scope,
                at,
                "unsupported_construct",
                "global write is outside the bounded Django callback model",
            );
            return;
        };
        let value = self.identity_with_dormant_declarations(scope, value, at, 0);
        let value = self.django_global_callback(value);
        self.django_global_event(scope, at, EventKind::Set { slot, value }, false);
    }

    pub(super) fn record_django_global_call(
        &mut self,
        invocation: Invocation,
        call: CallArguments<'_>,
        certainty: &'static str,
    ) {
        if self.django_globals.slots.is_empty() {
            return;
        }
        let Binding::Definition(definition, body) = self.symbols[invocation.target].binding else {
            return;
        };
        if definition.kind != DefinitionKind::Function || definition.is_generator {
            return;
        }
        let unproved = invocation.verified
            || !matches!(invocation.receiver, Receiver::Ordinary)
            || certainty != "certain"
            || !definition.decorators.is_empty()
            || !definition.parameters.is_empty()
            || !call.positional.is_empty()
            || !call.keywords.is_empty();
        self.django_global_event(
            call.scope,
            call.at,
            EventKind::Call {
                body,
                target: invocation.target,
            },
            unproved,
        );
    }

    pub(super) fn record_django_global_read(
        &mut self,
        scope: ScopeId,
        slot: SymbolId,
        at: Location,
    ) {
        if !self.probing_identity && self.django_globals.slots.contains(&slot) {
            if let Some(observed) = &mut self.django_globals.resolving_callback_read {
                *observed = true;
            }
            self.django_globals
                .reads
                .insert((scope, slot, position(at)));
        }
    }

    pub(super) fn resolve_untracked_django_callback(
        &mut self,
        scope: ScopeId,
        argument: &Expression,
        before: Location,
    ) -> Option<Value> {
        let (value, reads_rebound_global) =
            self.track_rebound_global_reads(|graph| graph.identity(scope, argument, before, 0));
        // Module attributes and imported aliases can read a tracked slot,
        // but this lookup cannot prove when they capture its value.
        (!reads_rebound_global).then_some(value)
    }

    pub(super) fn track_rebound_global_reads(
        &mut self,
        resolve: impl FnOnce(&mut Self) -> Value,
    ) -> (Value, bool) {
        let outer = self.django_globals.resolving_callback_read.replace(false);
        let value = resolve(self);
        let observed = self.django_globals.resolving_callback_read == Some(true);
        self.django_globals.resolving_callback_read = outer.map(|prior| prior || observed);
        (value, observed)
    }

    pub(super) fn record_django_global_connect(
        &mut self,
        scope: ScopeId,
        argument: &Expression,
        before: Location,
        at: Location,
        sole_argument: bool,
    ) -> bool {
        let ExpressionKind::Name(name) = &argument.kind else {
            return false;
        };
        let LexicalBinding::Found(slot) = self.lexical_binding(scope, name, before, true) else {
            return false;
        };
        if !self.django_globals.slots.contains(&slot) {
            return false;
        }
        self.django_globals
            .registration_reads
            .insert((scope, slot, position(argument.at)));
        // Additional arguments may run after the callback has been read.
        self.django_global_event(scope, at, EventKind::Connect(slot), !sole_argument);
        true
    }

    fn django_global_callback(&self, value: Value) -> Option<Invocation> {
        let Value::Callable(invocation) = value else {
            return None;
        };
        let Binding::Definition(definition, _) = self.symbols[invocation.target].binding else {
            return None;
        };
        (matches!(invocation.receiver, Receiver::Ordinary)
            && !invocation.verified
            && definition.kind != DefinitionKind::Class
            && !definition.is_generator
            && definition.decorators.is_empty())
        .then_some(invocation)
    }

    fn django_global_event(
        &mut self,
        scope: ScopeId,
        at: Location,
        kind: EventKind,
        unproved: bool,
    ) {
        let conditional = unproved
            || self.branch_depth != 0
            || self.scopes[scope]
                .owner
                .is_some_and(|owner| self.conditional.contains_key(&owner));
        self.django_globals
            .events
            .entry(scope)
            .or_default()
            .push(Event {
                at,
                conditional,
                kind,
            });
    }

    pub(super) fn resolve_django_global_signals(
        &mut self,
        roots: &[Edge],
        possible_roots: &[Edge],
    ) {
        let slots: Vec<_> = self.django_globals.slots.iter().copied().collect();
        for slot in slots {
            let relevant = self.django_globals.relevant_scopes(slot);
            let Some(module) = self.django_global_root(&relevant, roots, possible_roots) else {
                continue;
            };
            // An ordinary read of this slot could depend on any reached write.
            // Keep those uses outside this callback-only model.
            if self.django_globals.reads.iter().any(|read| {
                read.1 == slot
                    && (read.0 < self.files.len() || relevant.contains(&read.0))
                    && !self.django_globals.registration_reads.contains(read)
            }) {
                continue;
            }
            let owner_module = self.symbols[slot].scope;
            let Some(initial) = self.django_globals.initializer(owner_module, slot) else {
                continue;
            };
            if self
                .scope_issues
                .iter()
                .any(|(scope, _)| *scope == owner_module)
            {
                continue;
            }
            let mut replay = Replay {
                value: (module != owner_module).then_some(initial),
                budget: MAX_EFFECTS,
                callbacks: Vec::new(),
                covered: BTreeSet::new(),
            };
            if !self
                .django_globals
                .replay(module, slot, &relevant, None, 0, &mut replay)
                || replay.callbacks.is_empty()
            {
                continue;
            }
            for (at, value) in replay.callbacks {
                self.record_django_signal(
                    module,
                    frameworks::DjangoReceiver::Certain,
                    Some(Value::Callable(value)),
                    at,
                );
            }
            self.django_globals.proved_scopes.insert(slot, relevant);
            self.django_globals.covered.extend(replay.covered);
        }
    }

    fn django_global_root(
        &self,
        relevant: &BTreeSet<ScopeId>,
        roots: &[Edge],
        possible_roots: &[Edge],
    ) -> Option<ScopeId> {
        let mut modules = relevant
            .iter()
            .copied()
            .filter(|&scope| scope < self.files.len());
        let module = modules.next()?;
        if modules.next().is_some()
            || self
                .scope_issues
                .iter()
                .any(|(scope, _)| relevant.contains(scope))
        {
            return None;
        }
        let relevant_target = |target: SymbolId| matches!(self.symbols[target].binding, Binding::Definition(_, body) if relevant.contains(&body));
        if roots
            .iter()
            .chain(possible_roots)
            .any(|edge| relevant_target(edge.target))
            || self.scopes.iter().enumerate().any(|(scope, row)| {
                row.edges.iter().any(|edge| {
                    relevant_target(edge.target)
                        && !(edge.kind == "direct_call"
                            && self.django_globals.has_call(scope, edge.target))
                        && edge.kind != "type_reference"
                }) || row
                    .routes
                    .iter()
                    .chain(&row.optional_flask_routes)
                    .any(|&(_, target, _)| relevant_target(target))
            })
        {
            return None;
        }
        Some(module)
    }

    pub(super) fn report_django_global_signals(&mut self, reach: &Reachability) {
        let active_writes: BTreeSet<_> = self
            .django_globals
            .writers
            .iter()
            .filter_map(|(scope, slot)| reach.scopes.contains(scope).then_some(*slot))
            .collect();
        for &(scope, slot, [line, column]) in self
            .django_globals
            .reads
            .difference(&self.django_globals.registration_reads)
        {
            if reach.scopes.contains(&scope) && active_writes.contains(&slot) {
                self.issues.push(
                    Issue::new(
                        "unresolved_global_binding",
                        &self.files[self.scopes[scope].file].path,
                        "read of a rebound Django callback is outside the supported registration",
                    )
                    .at(Location { line, column }),
                );
            }
        }
        for (&scope, events) in &self.django_globals.events {
            if !reach.scopes.contains(&scope) {
                continue;
            }
            for event in events {
                let slot = match event.kind {
                    EventKind::Set { slot, .. } | EventKind::Connect(slot) => slot,
                    EventKind::Call { .. } | EventKind::Initialize { .. } => continue,
                };
                let proved = self.django_globals.covered.contains(&(scope, slot));
                let independently_reached = self.django_globals.proved_scopes.get(&slot).is_some_and(|scopes| {
                    reach.evidence.iter().any(|&(target, kind, _)| {
                        !matches!(kind, "direct_call" | "type_reference")
                            && matches!(self.symbols[target].binding, Binding::Definition(_, body) if scopes.contains(&body))
                    })
                });
                if !proved || independently_reached {
                    self.issues.push(Issue::new("unresolved_global_binding", &self.files[self.scopes[scope].file].path,
                        "Django signal callback depends on a global write outside the bounded call order").at(event.at));
                }
            }
        }
    }
}

impl Globals {
    fn initializer(&self, module: ScopeId, slot: SymbolId) -> Option<Invocation> {
        self.events
            .get(&module)?
            .iter()
            .find_map(|event| match event.kind {
                EventKind::Initialize {
                    slot: candidate,
                    value,
                } if candidate == slot && !event.conditional => value,
                _ => None,
            })
    }

    fn has_call(&self, scope: ScopeId, target: SymbolId) -> bool {
        self.events.get(&scope).into_iter().flatten().any(|event| {
            matches!(event.kind, EventKind::Call { target: selected, .. } if selected == target)
        })
    }

    fn relevant_scopes(&self, slot: SymbolId) -> BTreeSet<ScopeId> {
        let mut callers = BTreeMap::<_, Vec<_>>::new();
        let mut pending = Vec::new();
        for (&scope, events) in &self.events {
            for event in events {
                match event.kind {
                    EventKind::Set {
                        slot: candidate, ..
                    }
                    | EventKind::Connect(candidate)
                        if candidate == slot =>
                    {
                        pending.push(scope)
                    }
                    EventKind::Call { body, .. } => callers.entry(body).or_default().push(scope),
                    _ => {}
                }
            }
        }
        let mut found = BTreeSet::new();
        while let Some(scope) = pending.pop() {
            if found.insert(scope) {
                pending.extend(callers.get(&scope).into_iter().flatten());
            }
        }
        found
    }

    fn replay(
        &self,
        scope: ScopeId,
        slot: SymbolId,
        relevant: &BTreeSet<ScopeId>,
        origin: Option<Location>,
        depth: usize,
        replay: &mut Replay,
    ) -> bool {
        if depth >= MAX_BINDING_CHAIN {
            return false;
        }
        for event in self.events.get(&scope).into_iter().flatten() {
            let selected = match event.kind {
                EventKind::Set {
                    slot: candidate, ..
                }
                | EventKind::Initialize {
                    slot: candidate, ..
                }
                | EventKind::Connect(candidate) => candidate == slot,
                EventKind::Call { body, .. } => relevant.contains(&body),
            };
            if !selected {
                continue;
            }
            if event.conditional || replay.budget == 0 {
                return false;
            }
            replay.budget -= 1;
            let at = origin.unwrap_or(event.at);
            match event.kind {
                EventKind::Call { body, .. } => {
                    if !self.replay(body, slot, relevant, Some(at), depth + 1, replay) {
                        return false;
                    }
                }
                EventKind::Set {
                    value: Some(value), ..
                } => {
                    replay.value = Some(value);
                    replay.covered.insert((scope, slot));
                }
                EventKind::Initialize { value, .. } => replay.value = value,
                EventKind::Set { value: None, .. } => return false,
                EventKind::Connect(_) => {
                    let Some(value) = replay.value else {
                        return false;
                    };
                    replay.callbacks.push((at, value));
                    replay.covered.insert((scope, slot));
                }
            }
        }
        true
    }
}
