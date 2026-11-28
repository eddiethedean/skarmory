//! Bounded evaluation of direct calls to local closures.

use super::*;

pub(super) struct ClosureCall<'a> {
    pub(super) at: Location,
    pub(super) arguments: &'a [Expression],
}

#[derive(Clone, Copy)]
pub(super) struct ClosureCellWrite<'a> {
    setter: ScopeId,
    pub(super) value: &'a Expression,
    pub(super) value_scope: ScopeId,
    pub(super) value_before: Location,
    slot: ScopeId,
    source: SymbolId,
    key: &'a Name,
    pub(super) possible: bool,
}

struct TerminalRead<'a> {
    scope: ScopeId,
    target: &'a Expression,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum RegistryKind {
    List,
    Dictionary,
}

#[derive(Default)]
pub(super) struct RegistryUse {
    reads: BTreeSet<(ScopeId, [usize; 2])>,
    writes: BTreeSet<(ScopeId, [usize; 2])>,
}

impl<'a> Graph<'a> {
    pub(super) fn walk_direct_lambda_call(
        &mut self,
        function: &Expression,
        call: CallArguments<'_>,
    ) -> bool {
        let CallArguments {
            scope,
            before,
            at,
            positional,
            keywords,
        } = call;
        if !self.in_function(scope) || !keywords.is_empty() {
            return false;
        }
        let (lambda, local) = if let ExpressionKind::Name(name) = &function.kind {
            let Some(local) = self.lexical(scope, name, before) else {
                return false;
            };
            let Binding::Variable(Some(value), _) = self.symbols[local].binding else {
                return false;
            };
            if self.symbols[local].scope != scope || self.binding_depends_on_branch(local, before) {
                return false;
            }
            (value, Some(local))
        } else {
            (function, None)
        };
        let target = match (&lambda.kind, positional) {
            (ExpressionKind::DeferredLambda(body), []) => {
                let ExpressionKind::Call {
                    function,
                    arguments,
                    keywords,
                } = &body.kind
                else {
                    return false;
                };
                let ExpressionKind::Name(name) = &function.kind else {
                    return false;
                };
                if !arguments.is_empty()
                    || !keywords.is_empty()
                    || !self.lexical(scope, name, before).is_some_and(|id| {
                        self.symbols[id].scope == scope
                            && matches!(self.symbols[id].binding, Binding::Definition(_, _))
                    })
                {
                    return false;
                }
                // Captured locals are read when the lambda runs, including
                // functions defined after the lambda expression was evaluated.
                function
            }
            (
                ExpressionKind::DeferredCallbackLambda,
                [
                    target @ Expression {
                        kind: ExpressionKind::Name(_),
                        ..
                    },
                ],
            ) => target,
            _ => return false,
        };
        // Only a direct call in a straight-line function suite proves that
        // no intervening execution can replace the lambda or a captured cell.
        let suite = self.scopes[scope].body;
        let Some(index) = suite
            .iter()
            .position(|statement| position(statement.at) == position(before))
        else {
            return false;
        };
        if !matches!(&suite[index].kind,
            StatementKind::Return(Some(value)) | StatementKind::Evaluate(value)
                | StatementKind::Assign { value: Some(value), annotation: None, .. }
                if position(value.at) == position(at))
        {
            return false;
        }
        for statement in &suite[..index] {
            let inert = match &statement.kind {
                StatementKind::Define(def) => {
                    def.kind != DefinitionKind::Class
                        && def.decorators.is_empty()
                        && def.defaults.is_empty()
                        && def.annotations.is_empty()
                }
                StatementKind::Assign {
                    value: Some(value),
                    annotation: None,
                    ..
                } => matches!(
                    value.kind,
                    ExpressionKind::Name(_)
                        | ExpressionKind::DeferredLambda(_)
                        | ExpressionKind::DeferredCallbackLambda
                ),
                StatementKind::Evaluate(value) => matches!(value.kind, ExpressionKind::Text(_)),
                _ => false,
            };
            if !inert {
                return false;
            }
        }
        let Value::Callable(invocation) = self.probe_identity(scope, target, before) else {
            return false;
        };
        if !matches!(invocation.receiver, Receiver::Ordinary)
            || !matches!(self.symbols[invocation.target].binding, Binding::Definition(def, _)
                if def.kind == DefinitionKind::Function && !def.is_generator
                    && def.simple_positional_parameters && def.required_positional_parameter_count == 0)
        {
            return false;
        }
        // Invocation records a call edge, but a used lambda result can carry
        // the deferred generator outside that edge's modeled scope.
        if !matches!(suite[index].kind, StatementKind::Evaluate(_))
            && self.callback_generator_return(invocation, 0).is_some()
        {
            self.report_unmodeled_deferred_value(scope, at, &Value::DeferredComprehension);
        }
        self.walk(scope, target, before);
        self.read.extend(local);
        self.call_invocation(
            invocation,
            CallArguments {
                scope,
                before,
                at,
                positional: &[],
                keywords: &[],
            },
            "certain",
        );
        true
    }

    fn closure_registry_kind(&self, id: SymbolId) -> Option<RegistryKind> {
        // The source must own the unaliased built-in container. Cross-module
        // imports and attribute reads can observe mutations outside this model.
        if !self.phase03 || self.files.len() != 1 || self.symbols[id].scope != 0 {
            return None;
        }
        let Binding::Variable(Some(value), _) = self.symbols[id].binding else {
            return None;
        };
        let kind = match &value.kind {
            ExpressionKind::Sequence { items, list: true } if items.is_empty() => {
                RegistryKind::List
            }
            ExpressionKind::Dictionary(items) if items.is_empty() => RegistryKind::Dictionary,
            _ => return None,
        };
        let name = self.symbols[id].name.text.as_str();
        (self.scopes[0]
            .names
            .get(name)
            .is_some_and(|bindings| bindings.len() == 1)
            && !self.binding_depends_on_branch(id, END))
        .then_some(kind)
    }

    pub(super) fn record_closure_registry_read(
        &mut self,
        scope: ScopeId,
        id: SymbolId,
        at: Location,
    ) {
        if !self.probing_identity && self.closure_registry_kind(id).is_some() {
            self.closure_registries
                .entry(id)
                .or_default()
                .reads
                .insert((scope, position(at)));
        }
    }

    pub(super) fn retain_registry_callback(
        &mut self,
        scope: ScopeId,
        receiver: &Expression,
        callback: &Expression,
        before: Location,
        at: Location,
        kind: RegistryKind,
    ) -> bool {
        let ExpressionKind::Name(name) = &receiver.kind else {
            return false;
        };
        let Some(id) = self.lexical(scope, name, before) else {
            return false;
        };
        if self.probing_identity || self.closure_registry_kind(id) != Some(kind) {
            return false;
        }
        let Value::Callable(invocation) = self.identity(scope, callback, before, 0) else {
            return false;
        };
        if self.symbols[invocation.target].scope != scope || self.is_class(invocation.target) {
            return false;
        }
        self.closure_registries
            .entry(id)
            .or_default()
            .writes
            .insert((scope, position(receiver.at)));
        self.expose(scope, Value::Callable(invocation), "callback_registry", at);
        true
    }

    pub(super) fn report_unmodeled_registry_reads(&mut self, reach: &Reachability) {
        let mut unresolved = BTreeSet::new();
        for usage in self.closure_registries.values() {
            if usage
                .writes
                .iter()
                .any(|(scope, _)| reach.scopes.contains(scope))
            {
                unresolved.extend(
                    usage
                        .reads
                        .difference(&usage.writes)
                        .copied()
                        .filter(|(scope, _)| reach.scopes.contains(scope)),
                );
                // Reflective namespace reads can observe registered callbacks
                // without a direct reference to the container name.
                for &scope in &reach.scopes {
                    unresolved.extend(
                        self.scopes[scope]
                            .namespace_reads
                            .keys()
                            .map(|&at| (scope, at)),
                    );
                }
            }
        }
        for (scope, [line, column]) in unresolved {
            self.scope_issue(
                scope,
                Location { line, column },
                "unresolved_registry_read",
                "reading a mutated callback registry requires tracking its contents",
            );
        }
    }

    pub(super) fn returned_local_callable(&self, invocation: Invocation) -> Option<Value> {
        let Binding::Definition(factory, scope) = self.symbols[invocation.target].binding else {
            return None;
        };
        if !matches!(invocation.receiver, Receiver::Ordinary)
            || factory.kind != DefinitionKind::Function
            || factory.is_generator
            || !factory.decorators.is_empty()
            || !factory.parameters.is_empty()
        {
            return None;
        }
        // The supported body defines and returns a plain nested function.
        // Evaluating the factory result does not execute the returned body.
        let callback = returned_nested_function(factory)?;
        let target = self.local(scope, &callback.name.text, END)?;
        Some(Value::Callable(Invocation::ordinary(target)))
    }

    pub(super) fn final_direct_closure_call(
        &self,
        scope: ScopeId,
        outer: ScopeId,
    ) -> Option<ClosureCall<'a>> {
        if !self.phase03 || self.scopes[scope].parent != Some(outer) || !self.in_function(outer) {
            return None;
        }
        let owner = self.scopes[scope].owner?;
        let Binding::Definition(closure, _) = self.symbols[owner].binding else {
            return None;
        };
        if closure.kind != DefinitionKind::Function
            || closure.is_generator
            || !closure.annotations.is_empty()
            || !closure.defaults.is_empty() && closure.default_binding.is_none()
            || !closure.simple_positional_parameters
        {
            return None;
        }
        let (prefix, call, returned) = terminal_call(self.scopes[outer].body)?;
        let (name, arguments) = name_call(call)?;
        if arguments.len() < closure.required_positional_parameter_count
            || arguments.len() > closure.parameters.len()
            || self.local(outer, name, call.at) != Some(owner)
        {
            return None;
        }
        let key = self.lexical_binding_key(outer, name);
        let separate_name = |name: &str| self.lexical_binding_key(outer, name) != key;
        let separate_value = |value: &Expression| self.separate_closure_value(outer, value, &key);
        let import_before_closure = |statement: &Statement| {
            let StatementKind::Import(import) = &statement.kind else {
                return false;
            };
            if position(statement.at) >= position(closure.name.at) {
                return false;
            }
            // Replacing a local object during an import can run its finalizer.
            // Check earlier versions as a conditional import may be skipped.
            let mut before = statement.at;
            for _ in 0..MAX_BINDING_CHAIN {
                let Some(previous) = self.local(outer, &import.local.text, before) else {
                    return true;
                };
                let symbol = &self.symbols[previous];
                if !matches!(symbol.binding, Binding::Import(_)) {
                    return false;
                }
                before = symbol.name.at;
            }
            false
        };
        let separate_guarded_decorator = |definition: &Definition| {
            let at = definition.name.at;
            let [decorator] = definition.decorators.as_slice() else {
                return false;
            };
            let ExpressionKind::Boolean { and: true, values } = &decorator.kind else {
                return false;
            };
            self.by_location
                .get(&(outer, at.line, at.column))
                .is_some_and(|id| self.transparent_guarded_definitions.contains(id))
                && values
                    .iter()
                    .all(|value| plain_name(value).is_some_and(separate_name))
        };
        // Every observed invocation uses the bindings at the terminal call.
        // Earlier operations cannot call or expose that nested function.
        ((closure.decorators.is_empty() || separate_guarded_decorator(closure))
            && returned
                .iter()
                .all(|value| plain_name(value).is_some_and(separate_name))
            && arguments.iter().all(separate_value)
            && prefix.iter().all(|statement| match &statement.kind {
                StatementKind::Define(definition) => {
                    std::ptr::eq(definition, closure)
                        || passive_definition(definition)
                        || separate_guarded_decorator(definition)
                            && passive_definition_body(definition)
                }
                StatementKind::Assign {
                    value: Some(value),
                    annotation: None,
                    ..
                } => separate_value(value),
                StatementKind::If {
                    test,
                    body,
                    otherwise,
                } => {
                    separate_value(test)
                        && !body.is_empty()
                        && !otherwise.is_empty()
                        && body.iter().chain(otherwise).all(|statement| {
                            import_before_closure(statement)
                                || matches!(&statement.kind,
                                    StatementKind::Assign { value: Some(value), annotation: None, .. }
                                    if separate_value(value))
                        })
                }
                StatementKind::Evaluate(Expression {
                    kind: ExpressionKind::Text(_),
                    ..
                }) => true,
                _ => import_before_closure(statement),
            }))
        .then_some(ClosureCall {
            at: call.at,
            arguments,
        })
    }

    fn separate_closure_value(&self, frame: ScopeId, value: &Expression, own: &str) -> bool {
        let value = match &value.kind {
            ExpressionKind::IdentityDefaultLambda(default) => default,
            _ => value,
        };
        bounded_cell_source(value).is_some_and(|source| {
            self.lexical_binding_key(frame, source) != own
                && (plain_name(value).is_some()
                    || self
                        .lexical(frame, source, value.at)
                        .is_none_or(|id| self.symbols[id].scope != frame))
        })
    }

    pub(super) fn closure_reads_own_binding(&self, scope: ScopeId, closure: &Definition) -> bool {
        let key = self.lexical_binding_key(scope, &closure.name.text);
        let postponed = self.files[self.scopes[scope].file]
            .facts
            .postponed_annotations;
        namespace_free_names(closure, postponed)
            .into_iter()
            .any(|name| self.lexical_binding_key(scope, name) == key)
    }

    fn closure_cell_setter(&self, scope: ScopeId, depth: usize) -> Option<ClosureCellWrite<'a>> {
        let row = &self.scopes[scope];
        let owner = row.owner?;
        let Binding::Definition(definition, _) = self.symbols[owner].binding else {
            return None;
        };
        if depth >= MAX_BINDING_CHAIN || !plain_nullary_function(definition) {
            return None;
        }
        let (key, value, slot) = match row.body {
            [
                Statement {
                    kind: StatementKind::SetNonlocalName { name, value },
                    ..
                },
                tail @ ..,
            ] if tail.is_empty()
                || matches!(tail, [Statement { kind: StatementKind::Return(Some(value)), .. }]
                    if plain_name(value).is_some()) =>
            {
                let key = self.lexical_binding_key(scope, &name.text);
                let mut ancestor = row.parent?;
                loop {
                    if !self.in_function(ancestor) {
                        return None;
                    }
                    if self.local_by_key(ancestor, &key, END).is_some() {
                        break;
                    }
                    ancestor = self.scopes[ancestor].parent?;
                }
                (name, value, ancestor)
            }
            [
                Statement {
                    kind: StatementKind::SetGlobalName { name, value },
                    ..
                },
            ] => (name, value, row.file),
            [
                Statement {
                    kind: StatementKind::Define(inner),
                    ..
                },
                Statement {
                    kind: StatementKind::Evaluate(call),
                    ..
                },
            ] => {
                let (name, []) = name_call(call)? else {
                    return None;
                };
                let id = self.local(scope, name, call.at)?;
                let Binding::Definition(found, inner_scope) = self.symbols[id].binding else {
                    return None;
                };
                if !std::ptr::eq(inner, found) {
                    return None;
                }
                return self.closure_cell_setter(inner_scope, depth + 1);
            }
            _ => return None,
        };
        let source = bounded_cell_source(value)?;
        let source_id = self.lexical(scope, source, value.at)?;
        let source_scope = self.symbols[source_id].scope;
        let source_key = self.lexical_binding_key(scope, source);
        if self.local_by_key(source_scope, &source_key, END) != Some(source_id)
            || self.binding_depends_on_branch(source_id, END)
            || source_key == self.lexical_binding_key(scope, &key.text)
        {
            return None;
        }
        // Only ordinary nullary construction has a modeled result here.
        if plain_name(value).is_none() && !self.ordinary_cell_constructor(source_id) {
            return None;
        }
        Some(ClosureCellWrite {
            setter: scope,
            value,
            value_scope: scope,
            value_before: value.at,
            slot,
            source: source_id,
            key,
            possible: false,
        })
    }

    fn called_cell_setter(
        &self,
        frame: ScopeId,
        call: &Expression,
    ) -> Option<ClosureCellWrite<'a>> {
        let (name, arguments) = name_call(call)?;
        let mut write = match arguments {
            [] => {
                let id = self.local(frame, name, call.at)?;
                let Binding::Definition(_, setter) = self.symbols[id].binding else {
                    return None;
                };
                (!self.binding_depends_on_branch(id, call.at))
                    .then(|| self.closure_cell_setter(setter, 0))
                    .flatten()
            }
            [argument] => {
                let id = self.lexical(frame, name, call.at)?;
                let Binding::Import(import) = self.symbols[id].binding else {
                    return None;
                };
                let module = self.import_module(self.scopes[frame].file, import)?;
                let provider = *self.modules.get(module.as_str())?;
                let provider_before = self
                    .import_member_position(self.symbols[id].scope, &module, import)
                    .unwrap_or(END);
                let provider_id =
                    self.local(provider, import.member.as_deref()?, provider_before)?;
                let Binding::Definition(dispatch, _) = self.symbols[provider_id].binding else {
                    return None;
                };
                if !bounded_optional_callback(dispatch)
                    || self.binding_depends_on_branch(id, call.at)
                    || self.binding_depends_on_branch(provider_id, provider_before)
                    || self
                        .module_member_write(&module, import.member.as_deref()?, (frame, call.at))
                        .is_some()
                    || self.has_command_namespace(provider)
                {
                    return None;
                }
                let setter_id = self.local(frame, plain_name(argument)?, call.at)?;
                let Binding::Definition(_, setter) = self.symbols[setter_id].binding else {
                    return None;
                };
                if self.binding_depends_on_branch(setter_id, call.at) {
                    return None;
                }
                let mut write = self.closure_cell_setter(setter, 0)?;
                write.possible = true;
                Some(write)
            }
            _ => None,
        }?;
        let source = &self.symbols[write.source];
        if source.scope == frame && position(source.name.at) >= position(call.at) {
            return None;
        }
        let key = self.lexical_binding_key(source.scope, &source.name.text);
        let names = &self.scopes[source.scope].names;
        let explicit = names.get(key.as_ref());
        let private = self
            .private_binding_spelling(source.scope, &key)
            .and_then(|name| names.get(name));
        if write.slot == frame
            && source.scope == frame
            && self.scopes[write.setter].parent == Some(frame)
            && !write.possible
            && plain_name(write.value).is_some()
            && matches!(source.binding, Binding::Variable(Some(value), _) if plain_name(value).is_some())
        {
            // The immediate setter reads this caller's cell at the call.
            // Earlier values replaced by reassignment must be safe to drop.
            if self.local_by_key(frame, &key, call.at) != Some(write.source)
                || explicit
                    .into_iter()
                    .flatten()
                    .chain(private.into_iter().flatten())
                    .any(|&id| {
                        position(self.symbols[id].name.at) < position(source.name.at)
                            && !self.finalizer_free_cell_binding(id, 0)
                    })
            {
                return None;
            }
            write.value_scope = frame;
            write.value_before = call.at;
        } else if !matches!(source.binding, Binding::Definition(_, _))
            && explicit.map_or(0, Vec::len) + private.map_or(0, Vec::len) != 1
        {
            // For other frames, keep requiring a single stable source.
            return None;
        }
        Some(write)
    }

    fn terminal_cell_write(
        &self,
        frame: ScopeId,
    ) -> Option<(TerminalRead<'a>, ClosureCellWrite<'a>)> {
        if !self.phase03 || !self.in_function(frame) {
            return None;
        }
        let (prefix, call, []) = terminal_call(self.scopes[frame].body)? else {
            return None;
        };
        let (name, []) = name_call(call)? else {
            return None;
        };
        let frame_key = self.lexical_binding_key(frame, name);
        let mut write = None;
        for statement in prefix {
            let next = match &statement.kind {
                StatementKind::Define(definition) if passive_definition(definition) => None,
                StatementKind::Define(definition)
                    if definition.name.text == name
                        && definition.defaults.iter().all(inert_default)
                        && definition.decorators.is_empty()
                        && !self.files[self.scopes[frame].file]
                            .facts
                            .postponed_annotations
                        && let [annotation] = definition.annotations.as_slice() =>
                {
                    Some(self.called_cell_setter(frame, annotation)?)
                }
                StatementKind::Assign {
                    value: Some(value),
                    annotation: None,
                    ..
                } if write.is_none() && self.separate_closure_value(frame, value, &frame_key) =>
                {
                    None
                }
                StatementKind::Evaluate(Expression {
                    kind: ExpressionKind::Text(_),
                    ..
                }) => None,
                StatementKind::Evaluate(Expression { kind: ExpressionKind::Member(base, member), .. })
                    if member == "__annotations__"
                        && plain_name(base).is_some_and(|name| self.lexical_binding_key(frame, name) == frame_key)
                        && self.local_by_key(frame, &frame_key, statement.at).is_some_and(|id|
                            matches!(self.symbols[id].binding, Binding::Definition(definition, _)
                                if !definition.annotations.is_empty() && definition.decorators.is_empty())) => None,
                StatementKind::Evaluate(value) => Some(self.called_cell_setter(frame, value)?),
                _ => return None,
            };
            if let Some(next) = next
                && write.replace(next).is_some()
            {
                return None;
            }
        }
        let write = write?;
        let global = write.slot != frame;
        if global && !self.closed_global_cell_frame(frame, &write) {
            return None;
        }
        let key = self.lexical_binding_key(write.setter, &write.key.text);
        let previous = self.local_by_key(write.slot, &key, call.at)?;
        if self.binding_depends_on_branch(previous, call.at)
            || !self.finalizer_free_cell_binding(previous, 0)
            || !self.finalizer_free_cell_binding(write.source, 0)
        {
            return None;
        }
        let ExpressionKind::Call { function, .. } = &call.kind else {
            return None;
        };
        if frame_key == key && !global {
            return Some((
                TerminalRead {
                    scope: frame,
                    target: function,
                },
                write,
            ));
        }
        let id = self.local(frame, name, call.at)?;
        let Binding::Definition(definition, reader) = self.symbols[id].binding else {
            return None;
        };
        if definition.kind != DefinitionKind::Function
            || definition.is_generator
            || !definition.simple_positional_parameters
            || definition.required_positional_parameter_count != 0
            || !definition.required_keyword_only_parameters.is_empty()
            || !definition.defaults.iter().all(inert_default)
            || !definition.decorators.is_empty()
            || self.binding_depends_on_branch(id, call.at)
            || self.local_by_key(reader, &key, END).is_some()
        {
            return None;
        }
        let ([], call, tail) = terminal_call(definition.body.as_slice())? else {
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
        if !arguments.is_empty()
            || !keywords.is_empty()
            || !tail.iter().all(|value| {
                plain_name(value)
                    .is_some_and(|name| definition.parameters.iter().any(|p| p.text == name))
            })
        {
            return None;
        }
        let target = call_receiver(function);
        let target_name = plain_name(target)?;
        let receiver = self.lexical(reader, target_name, target.at)?;
        if global
            && self.scopes.iter().enumerate().any(|(scope, row)| {
                let Some(owner) = row.owner else { return false };
                let Binding::Definition(definition, _) = self.symbols[owner].binding else {
                    return false;
                };
                scope != frame
                    && scope != reader
                    && scope != write.setter
                    && (definition
                        .global_reads
                        .iter()
                        .any(|name| self.lexical_binding_key(scope, name) == key)
                        || namespace_free_names(
                            definition,
                            self.files[row.file].facts.postponed_annotations,
                        )
                        .into_iter()
                        .any(|name| {
                            matches!(name, "globals" | "eval" | "exec")
                                || self.lexical_binding_key(scope, name) == key
                        }))
            })
        {
            return None;
        }
        (self.lexical_binding_key(reader, target_name) == key
            && self.symbols[receiver].scope == write.slot)
            .then_some((
                TerminalRead {
                    scope: reader,
                    target,
                },
                write,
            ))
    }

    fn finalizer_free_cell_binding(&self, id: SymbolId, depth: usize) -> bool {
        if depth >= MAX_BINDING_CHAIN {
            return false;
        }
        let symbol = &self.symbols[id];
        match symbol.binding {
            Binding::Definition(definition, _) => {
                if definition.kind == DefinitionKind::Class {
                    self.finalizer_free_cell_instance(id)
                } else {
                    definition.decorators.is_empty()
                }
            }
            Binding::Variable(Some(value), _) if inert_default(value) => true,
            Binding::Variable(Some(value), before) => {
                let Some(source) = bounded_cell_source(value)
                    .and_then(|source| self.lexical(symbol.scope, source, before))
                else {
                    return false;
                };
                !self.binding_depends_on_branch(source, before)
                    && if plain_name(value).is_some() {
                        self.finalizer_free_cell_binding(source, depth + 1)
                    } else {
                        self.finalizer_free_cell_instance(source)
                    }
            }
            Binding::Import(import) => {
                let Some(member) = import.member.as_deref() else {
                    return false;
                };
                let Some(module) = self.import_module(self.scopes[symbol.scope].file, import)
                else {
                    return false;
                };
                let Some(&provider) = self.modules.get(module.as_str()) else {
                    return false;
                };
                let before = self
                    .import_member_position(symbol.scope, &module, import)
                    .unwrap_or(END);
                self.module_member_write(&module, member, (symbol.scope, import.local.at))
                    .is_none()
                    && self.local(provider, member, before).is_some_and(|source| {
                        !self.binding_depends_on_branch(source, before)
                            && self.finalizer_free_cell_binding(source, depth + 1)
                    })
            }
            _ => false,
        }
    }

    fn finalizer_free_cell_class(&self, source: SymbolId) -> Option<&'a Definition> {
        let Binding::Definition(class, _) = self.symbols[source].binding else {
            return None;
        };
        (class.kind == DefinitionKind::Class
            && class.decorators.is_empty()
            && class.bases.is_empty()
            && !class.has_class_keywords()
            && class.body.iter().all(|statement| {
                matches!(&statement.kind,
                StatementKind::Define(member)
                    if !matches!(member.name.text.as_str(), "__new__" | "__del__"))
            }))
        .then_some(class)
    }

    fn finalizer_free_cell_instance(&self, source: SymbolId) -> bool {
        let Some(class) = self.finalizer_free_cell_class(source) else {
            return false;
        };
        class.body.iter().all(|statement| {
            let StatementKind::Define(member) = &statement.kind else {
                return false;
            };
            member.name.text != "__init__"
                || (field_dispatch::plain_method(member, 1)
                    && matches!(
                        body_without_docstring(member),
                        [] | [Statement {
                            kind: StatementKind::Return(
                                None | Some(Expression {
                                    kind: ExpressionKind::None,
                                    ..
                                })
                            ),
                            ..
                        }]
                    ))
        })
    }

    fn ordinary_cell_constructor(&self, source: SymbolId) -> bool {
        self.finalizer_free_cell_class(source).is_some_and(|class| {
            class.body.iter().all(|statement| {
                matches!(&statement.kind, StatementKind::Define(member) if member.name.text != "__init__")
            })
        })
    }

    fn closed_global_cell_frame(&self, frame: ScopeId, write: &ClosureCellWrite<'a>) -> bool {
        let file = self.scopes[frame].file;
        if self.files.len() != 1 || write.slot != file || write.possible {
            return false;
        }
        let [
            prefix @ ..,
            Statement {
                kind: StatementKind::Evaluate(call),
                ..
            },
        ] = self.scopes[file].body
        else {
            return false;
        };
        let Some((name, [])) = name_call(call) else {
            return false;
        };
        let Some(owner) = self.local(file, name, call.at) else {
            return false;
        };
        if Some(owner) != self.scopes[frame].owner {
            return false;
        }
        // This is the only possible invocation of the frame. The module
        // prefix may define callables and construct plain instances, but
        // cannot run an arbitrary call or expose a callback before the write.
        prefix.iter().all(|statement| match &statement.kind {
            StatementKind::Define(definition) if definition.kind == DefinitionKind::Class =>
                definition.bases.is_empty()
                    && !definition.has_class_keywords()
                    && definition.decorators.is_empty()
                    && definition.body.iter().all(|statement| matches!(&statement.kind,
                        StatementKind::Define(method) if passive_definition(method))),
            StatementKind::Define(definition) => passive_definition(definition),
            StatementKind::Assign { value: Some(value), annotation: None, .. } =>
                inert_default(value)
                    || plain_name(value).is_some_and(|name| self.local(file, name, value.at) != Some(owner))
                    || matches!(name_call(value), Some((name, []))
                        if self.local(file, name, value.at).is_some_and(|id| self.ordinary_cell_constructor(id))),
            StatementKind::Evaluate(Expression { kind: ExpressionKind::Text(_), .. }) => true,
            _ => false,
        })
    }

    pub(super) fn closure_cell_read(
        &self,
        scope: ScopeId,
        expression: &Expression,
    ) -> Option<ClosureCellWrite<'a>> {
        if !self.phase03 {
            return None;
        }
        let (_, terminal, _) = terminal_call(self.scopes[scope].body)?;
        let ExpressionKind::Call { function, .. } = &terminal.kind else {
            return None;
        };
        if position(call_receiver(function).at) != position(expression.at) {
            return None;
        }
        [Some(scope), self.scopes[scope].parent]
            .into_iter()
            .flatten()
            .filter_map(|frame| self.terminal_cell_write(frame))
            .find_map(|(read, write)| {
                (read.scope == scope && position(read.target.at) == position(expression.at))
                    .then_some(write)
            })
    }

    pub(super) fn proved_closure_cell_write(&self, scope: ScopeId) -> Option<ClosureCellWrite<'a>> {
        let mut parent = self.scopes[scope].parent;
        for _ in 0..MAX_BINDING_CHAIN {
            let frame = parent?;
            if let Some((_, write)) = self.terminal_cell_write(frame)
                && write.setter == scope
            {
                return Some(write);
            }
            parent = self.scopes[frame].parent;
        }
        None
    }
}

fn call_receiver(function: &Expression) -> &Expression {
    match &function.kind {
        ExpressionKind::Member(base, _) => base,
        _ => function,
    }
}

fn plain_name(value: &Expression) -> Option<&str> {
    let ExpressionKind::Name(name) = &value.kind else {
        return None;
    };
    Some(name)
}

fn name_call(value: &Expression) -> Option<(&str, &[Expression])> {
    let ExpressionKind::Call {
        function,
        arguments,
        keywords,
    } = &value.kind
    else {
        return None;
    };
    if !keywords.is_empty() {
        return None;
    }
    Some((plain_name(function)?, arguments.as_slice()))
}

fn bounded_cell_source(value: &Expression) -> Option<&str> {
    plain_name(value).or_else(|| match name_call(value) {
        Some((name, [])) => Some(name),
        _ => None,
    })
}

fn terminal_call(body: &[Statement]) -> Option<(&[Statement], &Expression, &[Expression])> {
    let [
        prefix @ ..,
        Statement {
            kind: StatementKind::Return(Some(result)),
            ..
        },
    ] = body
    else {
        return None;
    };
    match &result.kind {
        ExpressionKind::Sequence { items, list: false } => {
            let (call, rest) = items.split_first()?;
            Some((prefix, call, rest))
        }
        _ => Some((prefix, result, &[])),
    }
}

pub(super) fn passive_definition(definition: &Definition) -> bool {
    definition.decorators.is_empty() && passive_definition_body(definition)
}

fn passive_definition_body(definition: &Definition) -> bool {
    definition.defaults.is_empty()
        && definition.annotations.is_empty()
        && if definition.kind == DefinitionKind::Class {
            definition.bases.is_empty()
                && !definition.has_class_keywords()
                && definition.body.is_empty()
        } else {
            true
        }
}

pub(super) fn inert_default(value: &Expression) -> bool {
    matches!(
        value.kind,
        ExpressionKind::None
            | ExpressionKind::Bool(_)
            | ExpressionKind::Scalar(_)
            | ExpressionKind::Text(_)
    )
}

fn plain_nullary_function(definition: &Definition) -> bool {
    definition.kind == DefinitionKind::Function
        && !definition.is_generator
        && definition.parameters.is_empty()
        && definition.simple_positional_parameters
        && definition.defaults.is_empty()
        && definition.decorators.is_empty()
}

fn bounded_optional_callback(definition: &Definition) -> bool {
    let [parameter] = definition.parameters.as_slice() else {
        return false;
    };
    if definition.kind != DefinitionKind::Function
        || definition.is_generator
        || !definition.simple_positional_parameters
        || !passive_definition(definition)
    {
        return false;
    }
    let [
        Statement {
            kind:
                StatementKind::If {
                    test,
                    body,
                    otherwise,
                },
            ..
        },
    ] = definition.body.as_slice()
    else {
        return false;
    };
    matches!(plain_name(test), Some("__debug__"))
        && otherwise.is_empty()
        && matches!(body.as_slice(), [Statement { kind: StatementKind::Evaluate(call), .. }] if
            name_call(call).is_some_and(|(name, args)| name == parameter.text && args.is_empty()))
}
