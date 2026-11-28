//! Calls through a fresh instance returned by a selected source factory.

use super::*;

#[derive(Clone)]
pub(super) struct FactoryMethodCall {
    scope: ScopeId,
    before: Location,
    at: Location,
    factory: SymbolId,
    member: String,
    conditional: bool,
}

impl<'a> Graph<'a> {
    pub(super) fn defer_factory_method_call(
        &mut self,
        function: &Expression,
        call: CallArguments<'_>,
    ) -> bool {
        let ExpressionKind::Member(receiver, member) = &function.kind else {
            return false;
        };
        let ExpressionKind::Call {
            function,
            arguments,
            keywords,
        } = &receiver.kind
        else {
            return false;
        };
        if self.probing_identity
            || member.starts_with("__")
            || !call.positional.is_empty()
            || !call.keywords.is_empty()
            || !arguments.is_empty()
            || !keywords.is_empty()
        {
            return false;
        }
        let Some(factory) = self.stable_factory_binding(call.scope, function, call.before) else {
            return false;
        };
        let Binding::Definition(definition, _) = self.symbols[factory].binding else {
            return false;
        };
        if !field_dispatch::plain_method(definition, 0)
            || !matches!(
                body_without_docstring(definition),
                [Statement {
                    kind: StatementKind::Return(Some(Expression {
                        kind: ExpressionKind::Call { .. },
                        ..
                    })),
                    ..
                }]
            )
        {
            return false;
        }
        self.source_factory_method_calls.push(FactoryMethodCall {
            scope: call.scope,
            before: call.before,
            at: call.at,
            factory,
            member: member.clone(),
            conditional: self.branch_depth != 0,
        });
        true
    }

    fn stable_factory_binding(
        &mut self,
        scope: ScopeId,
        expression: &Expression,
        before: Location,
    ) -> Option<SymbolId> {
        let ExpressionKind::Name(name) = &expression.kind else {
            return None;
        };
        let LexicalBinding::Found(id) = self.lexical_binding(scope, name, before, true) else {
            return None;
        };
        let binding_scope = self.symbols[id].scope;
        let file = self.scopes[binding_scope].file;
        if self.scopes[binding_scope].names.get(name.as_str())?.len() != 1
            || self.conditional.contains_key(&id)
            || binding_scope == file && self.has_module_binding_write(file, name)
        {
            return None;
        }
        let Value::Callable(invocation) = self.probe_identity(scope, expression, before) else {
            return None;
        };
        let target_scope = self.symbols[invocation.target].scope;
        let target_name = self.symbols[invocation.target].name.text.as_str();
        (!invocation.verified
            && !invocation.implicit_descriptor_receiver
            && matches!(invocation.receiver, Receiver::Ordinary)
            && target_scope == self.scopes[target_scope].file
            && self.scopes[target_scope].names.get(target_name)?.len() == 1
            && !self.conditional.contains_key(&invocation.target)
            && !self.has_module_binding_write(target_scope, target_name)
            && self.source_provider_definition_precedes(invocation.target, (scope, before)))
        .then_some(invocation.target)
    }

    fn resolve_factory_method_call(
        &mut self,
        call: &FactoryMethodCall,
        invalidated: &BTreeSet<SymbolId>,
    ) -> Option<(SymbolId, SymbolId)> {
        if invalidated.contains(&call.factory) {
            return None;
        }
        let Binding::Definition(factory, scope) = self.symbols[call.factory].binding else {
            return None;
        };
        let [
            Statement {
                kind: StatementKind::Return(Some(returned)),
                at,
            },
        ] = body_without_docstring(factory)
        else {
            return None;
        };
        let ExpressionKind::Call {
            function,
            arguments,
            keywords,
        } = &returned.kind
        else {
            return None;
        };
        if !arguments.is_empty() || !keywords.is_empty() {
            return None;
        }
        let class = self.stable_factory_binding(scope, function, *at)?;
        let access = (call.scope, call.before);
        if !self.plain_default_provider(class, access, invalidated)
            || self
                .class_member_at(class, "__del__", 0, Some(access))
                .is_some()
        {
            return None;
        }
        let Value::Callable(method) = self.class_member_at(class, &call.member, 0, Some(access))?
        else {
            return None;
        };
        let Binding::Definition(definition, _) = self.symbols[method.target].binding else {
            return None;
        };
        let bound = method.bind_receiver(Receiver::Bound(class));
        (!method.verified
            && matches!(method.receiver, Receiver::Ordinary)
            && field_dispatch::plain_method(definition, 1)
            && !self.receiver_conflicts(bound)
            && self.source_provider_returns_scalar(method.target, invalidated, 0))
        .then_some((method.target, class))
    }

    pub(super) fn activate_factory_method_calls(
        &mut self,
        reach: &mut Reachability,
        pending: &mut VecDeque<Edge>,
        possible: bool,
        invalidated: &BTreeSet<SymbolId>,
    ) {
        for index in 0..self.source_factory_method_calls.len() {
            let call = self.source_factory_method_calls[index].clone();
            if !reach.scopes.contains(&call.scope)
                || reach.resolved_factory_method_calls.contains(&index)
            {
                continue;
            }
            let body = self.scopes[call.scope].body;
            if body.iter().take_while(|statement| position(statement.at) < position(call.before))
                .any(|statement| matches!(&statement.kind, StatementKind::Evaluate(expression)
                    if self.plain_call_always_raises(call.scope, expression, statement.at, invalidated)))
            {
                reach.resolved_factory_method_calls.insert(index);
                continue;
            }
            let Some((target, class)) = self.resolve_factory_method_call(&call, invalidated) else {
                continue;
            };
            reach.resolved_factory_method_calls.insert(index);
            reach
                .deferred_method_receivers
                .entry(target)
                .or_default()
                .insert(class);
            self.direct.insert(target);
            let certainty = if possible || call.conditional {
                "possible"
            } else {
                "certain"
            };
            pending.extend(["direct_call", SOURCE_FACTORY_METHOD].map(|kind| Edge {
                target,
                kind,
                certainty,
            }));
        }
    }

    pub(super) fn report_unresolved_factory_method_calls(&mut self, reach: &Reachability) {
        for (index, call) in self.source_factory_method_calls.iter().enumerate() {
            if reach.scopes.contains(&call.scope)
                && !reach.resolved_factory_method_calls.contains(&index)
            {
                self.issues.push(
                    Issue::new(
                        "unresolved_call",
                        &self.files[self.scopes[call.scope].file].path,
                        "factory method dispatch requires an unmodified fresh receiver",
                    )
                    .at(call.at),
                );
            }
        }
    }

    fn plain_call_always_raises(
        &mut self,
        scope: ScopeId,
        expression: &Expression,
        before: Location,
        invalidated: &BTreeSet<SymbolId>,
    ) -> bool {
        let ExpressionKind::Call {
            function,
            arguments,
            keywords,
        } = &expression.kind
        else {
            return false;
        };
        if scope == self.scopes[scope].file || !arguments.is_empty() || !keywords.is_empty() {
            return false;
        }
        let Some(target) = self.stable_factory_binding(scope, function, before) else {
            return false;
        };
        !invalidated.contains(&target)
            && matches!(self.symbols[target].binding, Binding::Definition(definition, _)
            if field_dispatch::plain_method(definition, 0)
                && matches!(body_without_docstring(definition), [Statement { kind: StatementKind::Raise(_), .. }]))
    }
}
