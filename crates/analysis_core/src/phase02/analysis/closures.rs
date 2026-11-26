//! Bounded evaluation of direct calls to local closures.

use super::*;

pub(super) struct ClosureCall<'a> {
    pub(super) at: Location,
    pub(super) arguments: &'a [Expression],
    pub(super) write: Option<ClosureCellWrite<'a>>,
}

pub(super) struct ClosureCellWrite<'a> {
    pub(super) setter: ScopeId,
    pub(super) name: &'a Name,
    pub(super) value: &'a Expression,
    pub(super) at: Location,
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
            || !closure.decorators.is_empty()
            || !closure.annotations.is_empty()
            || !closure.defaults.is_empty() && closure.default_binding.is_none()
            || !closure.simple_positional_parameters
        {
            return None;
        }
        let [
            prefix @ ..,
            Statement {
                kind:
                    StatementKind::Return(Some(Expression {
                        kind:
                            ExpressionKind::Call {
                                function,
                                arguments,
                                keywords,
                            },
                        at,
                    })),
                ..
            },
        ] = self.scopes[outer].body
        else {
            return None;
        };
        let ExpressionKind::Name(name) = &function.kind else {
            return None;
        };
        if arguments.len() < closure.required_positional_parameter_count
            || arguments.len() > closure.parameters.len()
            || !keywords.is_empty()
            || self.local(outer, name, *at) != Some(owner)
        {
            return None;
        }
        let closure_key = self.function_binding_key(outer, name);
        let mut cell_writes = prefix.iter().filter_map(|statement| match &statement.kind {
            StatementKind::Evaluate(call) => self.local_cell_setter(outer, call),
            _ => None,
        });
        let write = cell_writes.next();
        if let Some(write) = &write {
            // Track a single explicit write followed by a direct read of the
            // same cell. A callable with other effects or escaped state needs
            // evaluation across activation instances.
            let [
                Statement {
                    kind:
                        StatementKind::Return(Some(Expression {
                            kind:
                                ExpressionKind::Call {
                                    function,
                                    arguments,
                                    keywords,
                                },
                            ..
                        })),
                    ..
                },
            ] = closure.body.as_slice()
            else {
                return None;
            };
            if cell_writes.next().is_some()
                || !arguments.is_empty()
                || !keywords.is_empty()
                || !matches!(&function.kind, ExpressionKind::Name(name)
                    if self.function_binding_key(scope, name)
                        == self.function_binding_key(write.setter, &write.name.text))
                || self.local(outer, &write.name.text, write.at).is_none()
            {
                return None;
            }
        }
        let setter = write
            .as_ref()
            .and_then(|write| self.scopes[write.setter].owner);
        let setter_key =
            setter.map(|id| self.function_binding_key(outer, &self.symbols[id].name.text));
        let separate_name = |name: &str| {
            let key = self.function_binding_key(outer, name);
            key != closure_key && setter_key.as_ref() != Some(&key)
        };
        let separate_value = |value: &Expression| match &value.kind {
            ExpressionKind::Name(name) => separate_name(name),
            ExpressionKind::Call {
                function,
                arguments,
                keywords,
            } => {
                arguments.is_empty()
                    && keywords.is_empty()
                    && matches!(&function.kind, ExpressionKind::Name(name) if separate_name(name))
            }
            _ => false,
        };
        // No prior statement or argument can expose or invoke the closure.
        (arguments.iter().all(separate_value)
            && prefix.iter().all(|statement| match &statement.kind {
                StatementKind::Define(definition) => {
                    std::ptr::eq(definition, closure)
                        || setter.is_some_and(|id| {
                            matches!(self.symbols[id].binding, Binding::Definition(found, _)
                            if std::ptr::eq(found, definition))
                        })
                }
                StatementKind::Assign {
                    value: Some(value),
                    annotation: None,
                    ..
                } => separate_value(value),
                StatementKind::Evaluate(Expression {
                    kind: ExpressionKind::Text(_),
                    ..
                }) => true,
                StatementKind::Evaluate(call) => write
                    .as_ref()
                    .is_some_and(|write| position(call.at) == position(write.at)),
                _ => false,
            }))
        .then_some(ClosureCall {
            at: *at,
            arguments,
            write,
        })
    }

    fn local_cell_setter(
        &self,
        outer: ScopeId,
        call: &'a Expression,
    ) -> Option<ClosureCellWrite<'a>> {
        let ExpressionKind::Call {
            function,
            arguments,
            keywords,
        } = &call.kind
        else {
            return None;
        };
        let ExpressionKind::Name(name) = &function.kind else {
            return None;
        };
        if !arguments.is_empty() || !keywords.is_empty() {
            return None;
        }
        let id = self.local(outer, name, call.at)?;
        let Binding::Definition(definition, setter) = self.symbols[id].binding else {
            return None;
        };
        let [
            Statement {
                kind: StatementKind::SetNonlocalName { name, value },
                ..
            },
        ] = definition.body.as_slice()
        else {
            return None;
        };
        if !definition.annotations.is_empty() || self.binding_depends_on_branch(id, call.at) {
            return None;
        }
        Some(ClosureCellWrite {
            setter,
            name,
            value,
            at: call.at,
        })
    }

    pub(super) fn proved_closure_cell_write(&self, scope: ScopeId) -> Option<ClosureCellWrite<'a>> {
        let outer = self.scopes[scope].parent?;
        let StatementKind::Return(Some(Expression {
            kind: ExpressionKind::Call { function, .. },
            at,
        })) = &self.scopes[outer].body.last()?.kind
        else {
            return None;
        };
        let ExpressionKind::Name(name) = &function.kind else {
            return None;
        };
        let id = self.local(outer, name, *at)?;
        let Binding::Definition(_, closure) = self.symbols[id].binding else {
            return None;
        };
        self.final_direct_closure_call(closure, outer)?
            .write
            .filter(|write| write.setter == scope)
    }
}
