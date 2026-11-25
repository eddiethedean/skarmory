//! Source-owned dispatch through a field of a freshly constructed receiver.

use super::*;

pub(super) const DIRECT: &str = "fresh_field_call";

pub(super) struct FreshFieldInvocation {
    caller: ScopeId,
    at: Location,
    method: Invocation,
    provider_method: Invocation,
    dependencies: [SymbolId; 3],
}

impl Graph<'_> {
    pub(super) fn invoke_fresh_field_method(
        &mut self,
        invocation: Invocation,
        function: &Expression,
        call: CallArguments<'_>,
    ) -> bool {
        if invocation.verified || !call.positional.is_empty() || !call.keywords.is_empty() {
            return false;
        }
        let Receiver::Bound(class) = invocation.receiver else {
            return false;
        };
        let ExpressionKind::Member(receiver, _) = &function.kind else {
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
        let [stored] = arguments.as_slice() else {
            return false;
        };
        if !keywords.is_empty()
            || !matches!(self.probe_identity(call.scope, function, call.before),
                Value::Callable(constructor) if constructor.target == class && !constructor.verified)
        {
            return false;
        }
        let Binding::Definition(method, _) = self.symbols[invocation.target].binding else {
            return false;
        };
        let Some((_, field, member, supplied)) = field_method(method) else {
            return false;
        };
        if self.scopes[self.symbols[invocation.target].scope].owner != Some(class)
            || self.fresh_constructor_field(class) != Some(field)
            || !self.class_discovery_writes(class, call.scope).is_empty()
        {
            return false;
        }
        let Some((provider, factory)) =
            self.literal_factory_instance(call.scope, stored, call.before)
        else {
            return false;
        };
        let access = Some((call.scope, call.before));
        let Some(Value::Callable(selected)) = self.class_member_at(provider, member, 0, access)
        else {
            return false;
        };
        let Binding::Definition(selected_definition, _) = self.symbols[selected.target].binding
        else {
            return false;
        };
        if selected.verified || !plain_method(selected_definition, supplied + 1) {
            return false;
        }
        self.fresh_field_invocations.push(FreshFieldInvocation {
            caller: call.scope,
            at: call.at,
            method: invocation,
            provider_method: selected.bind_receiver(Receiver::Bound(provider)),
            dependencies: [class, provider, factory],
        });
        true
    }

    pub(super) fn resolve_fresh_field_invocations(&mut self) {
        if self.fresh_field_invocations.is_empty() {
            return;
        }
        // The body walker also records callable escapes. Check them after
        // every module has been visited so proof does not depend on file order.
        let escaped: BTreeSet<_> = self
            .scopes
            .iter()
            .flat_map(|scope| {
                scope.edges.iter().filter_map(|edge| {
                    matches!(edge.kind, "passed_callable" | "returned_callable")
                        .then_some(edge.target)
                })
            })
            .collect();
        let mut replaced = BTreeSet::new();
        for scope in 0..self.scopes.len() {
            self.collect_field_callback_replacements(scope, self.scopes[scope].body, &mut replaced);
        }
        for proof in std::mem::take(&mut self.fresh_field_invocations) {
            self.direct.insert(proof.method.target);
            if proof
                .dependencies
                .iter()
                .chain([&proof.method.target, &proof.provider_method.target])
                .any(|dependency| escaped.contains(dependency) || replaced.contains(dependency))
            {
                self.invoke(
                    proof.caller,
                    proof.method,
                    "direct_call",
                    "certain",
                    proof.at,
                );
                continue;
            }
            // The provider target stays attached to its specific caller.
            self.direct.insert(proof.provider_method.target);
            self.invoke(proof.caller, proof.method, DIRECT, "certain", proof.at);
            self.invoke(
                proof.caller,
                proof.provider_method,
                "direct_call",
                "certain",
                proof.at,
            );
        }
    }

    fn collect_field_callback_replacements(
        &mut self,
        scope: ScopeId,
        body: &[Statement],
        replaced: &mut BTreeSet<SymbolId>,
    ) {
        for statement in body {
            let at = statement.at;
            let receiver = match &statement.kind {
                StatementKind::WriteAttribute {
                    receiver, member, ..
                } if member == "__code__" => receiver,
                StatementKind::Evaluate(Expression {
                    kind:
                        ExpressionKind::Call {
                            function,
                            arguments,
                            keywords,
                        },
                    ..
                }) if keywords.is_empty() => {
                    let [receiver, name, _] = arguments.as_slice() else {
                        continue;
                    };
                    if !matches!(&name.kind, ExpressionKind::Text(name) if name == "__code__")
                        || !matches!(self.probe_identity(scope, function, at),
                            Value::Provider(name) if name == "builtins.setattr")
                    {
                        continue;
                    }
                    receiver
                }
                StatementKind::If {
                    test,
                    body,
                    otherwise,
                } => {
                    let known = self.indexed_bool(scope, test, at, 0);
                    if known != Some(false) {
                        self.collect_field_callback_replacements(scope, body, replaced);
                    }
                    if known != Some(true) {
                        self.collect_field_callback_replacements(scope, otherwise, replaced);
                    }
                    continue;
                }
                StatementKind::With { body, .. } => {
                    self.collect_field_callback_replacements(scope, body, replaced);
                    continue;
                }
                StatementKind::Return(_) | StatementKind::Raise(_) => break,
                _ => continue,
            };
            if let Value::Callable(callback) = self.probe_identity(scope, receiver, at) {
                replaced.insert(callback.target);
            }
        }
    }

    pub(super) fn is_fresh_field_initializer(&self, scope: ScopeId, statement: &Statement) -> bool {
        let Some(initializer) = self.scopes[scope].owner else {
            return false;
        };
        let Some(class) = self.scopes[self.symbols[initializer].scope].owner else {
            return false;
        };
        self.symbols[initializer].name.text == "__init__"
            && self.fresh_constructor_field(class).is_some()
            && self.class_discovery_writes(class, scope).is_empty()
            && self
                .receiver_parameter(initializer)
                .is_some_and(|parameter| self.instance_receivers.get(&parameter) == Some(&class))
            && self.scopes[scope]
                .body
                .iter()
                .any(|entry| std::ptr::eq(entry, statement))
    }

    pub(super) fn defer_fresh_field_call(&mut self, scope: ScopeId, call: &Expression) -> bool {
        let Some(owner) = self.scopes[scope].owner else {
            return false;
        };
        let Binding::Definition(method, _) = self.symbols[owner].binding else {
            return false;
        };
        let Some((returned, _, _, _)) = field_method(method) else {
            return false;
        };
        if !std::ptr::eq(returned, call) || self.receiver_parameter(owner).is_none() {
            return false;
        }
        self.fresh_field_calls.insert(owner, call.at);
        true
    }

    pub(super) fn report_fresh_field_calls(&mut self, reach: &Reachability) {
        for (&method, &at) in &self.fresh_field_calls {
            let Binding::Definition(_, scope) = self.symbols[method].binding else {
                unreachable!()
            };
            if reach.scopes.contains(&scope)
                && (!reach.has_evidence(method, DIRECT)
                    || reach
                        .evidence
                        .range((method, "", "")..)
                        .take_while(|(target, _, _)| *target == method)
                        .any(|(_, kind, _)| *kind != DIRECT))
            {
                self.issues.push(
                    Issue::new(
                        "unresolved_call",
                        &self.files[self.scopes[scope].file].path,
                        "field method dispatch requires a bounded fresh instance",
                    )
                    .at(at),
                );
            }
        }
    }

    fn fresh_constructor_field(&self, class: SymbolId) -> Option<&str> {
        let Binding::Definition(definition, _) = self.symbols[class].binding else {
            return None;
        };
        if definition.kind != DefinitionKind::Class
            || !definition.bases.is_empty()
            || definition.table_keyword.is_some()
            || !definition.decorators.is_empty()
            || self.subclassed.contains(&class)
        {
            return None;
        }
        let mut field = None;
        for statement in body_without_docstring(definition) {
            let StatementKind::Define(method) = &statement.kind else {
                return None;
            };
            if !method.decorators.is_empty() {
                return None;
            }
            if method.name.text == "__init__" {
                if !plain_method(method, 2) || field.is_some() {
                    return None;
                }
                let [
                    Statement {
                        kind:
                            StatementKind::WriteAttribute {
                                receiver,
                                member,
                                value: Some(value),
                            },
                        ..
                    },
                ] = body_without_docstring(method)
                else {
                    return None;
                };
                if !matches!((&receiver.kind, &value.kind), (ExpressionKind::Name(receiver), ExpressionKind::Name(argument))
                    if receiver == &method.parameters[0].text && argument == &method.parameters[1].text)
                    || member.starts_with("__")
                {
                    return None;
                }
                field = Some(member.as_str());
            } else if method.name.text.starts_with("__") {
                return None;
            }
        }
        field
    }

    fn literal_factory_instance(
        &mut self,
        scope: ScopeId,
        expression: &Expression,
        before: Location,
    ) -> Option<(SymbolId, SymbolId)> {
        let ExpressionKind::Call {
            function,
            arguments,
            keywords,
        } = &expression.kind
        else {
            return None;
        };
        let [argument] = arguments.as_slice() else {
            return None;
        };
        if !keywords.is_empty() {
            return None;
        }
        let Value::Text(actual) = self.probe_identity(scope, argument, before) else {
            return None;
        };
        let Value::Callable(factory) = self.probe_identity(scope, function, before) else {
            return None;
        };
        let Binding::Definition(definition, body) = self.symbols[factory.target].binding else {
            return None;
        };
        if factory.verified
            || !matches!(factory.receiver, Receiver::Ordinary)
            || self.scopes[self.symbols[factory.target].scope]
                .parent
                .is_some()
            || !plain_method(definition, 1)
        {
            return None;
        }
        let [
            Statement {
                kind:
                    StatementKind::If {
                        test,
                        body: matched,
                        otherwise,
                    },
                ..
            },
            fallback,
        ] = body_without_docstring(definition)
        else {
            return None;
        };
        if !otherwise.is_empty() {
            return None;
        }
        let ExpressionKind::Compare {
            left,
            right,
            equal: true,
            identity: false,
        } = &test.kind
        else {
            return None;
        };
        let (ExpressionKind::Name(parameter), ExpressionKind::Text(expected)) =
            (&left.kind, &right.kind)
        else {
            return None;
        };
        if parameter != &definition.parameters[0].text {
            return None;
        }
        let [on_match] = matched.as_slice() else {
            return None;
        };
        let selected = if &actual == expected {
            on_match
        } else {
            fallback
        };
        let Statement {
            kind: StatementKind::Return(Some(expression)),
            at,
        } = selected
        else {
            return None;
        };
        let ExpressionKind::Call {
            function,
            arguments,
            keywords,
        } = &expression.kind
        else {
            return None;
        };
        if !arguments.is_empty() || !keywords.is_empty() {
            return None;
        }
        let Value::Callable(constructor) = self.probe_identity(body, function, *at) else {
            return None;
        };
        let class = constructor.target;
        let access = Some((scope, before));
        (!constructor.verified
            && self.plain_provider_class(class, scope, 0)
            && [
                "__new__",
                "__init__",
                "__init_subclass__",
                "__getattribute__",
                "__getattr__",
            ]
            .iter()
            .all(|name| self.class_member_at(class, name, 0, access).is_none()))
        .then_some((class, factory.target))
    }

    fn plain_provider_class(&mut self, class: SymbolId, caller: ScopeId, depth: usize) -> bool {
        let symbol = &self.symbols[class];
        let Binding::Definition(definition, _) = symbol.binding else {
            return false;
        };
        if depth >= MAX_BINDING_CHAIN
            || definition.kind != DefinitionKind::Class
            || definition.table_keyword.is_some()
            || !definition.decorators.is_empty()
            || !self.class_discovery_writes(class, caller).is_empty()
        {
            return false;
        }
        match definition.bases.as_slice() {
            [] => true,
            [base] => match self.probe_identity(symbol.scope, base, definition.name.at) {
                Value::Provider(name) => name == "builtins.object",
                Value::Callable(parent) if !parent.verified => {
                    self.plain_provider_class(parent.target, caller, depth + 1)
                }
                _ => false,
            },
            _ => false,
        }
    }
}

fn plain_method(definition: &Definition, parameters: usize) -> bool {
    definition.kind == DefinitionKind::Function
        && !definition.is_generator
        && definition.decorators.is_empty()
        && definition.simple_positional_parameters
        && definition.defaults.is_empty()
        && definition.parameters.len() == parameters
}

fn field_method(definition: &Definition) -> Option<(&Expression, &str, &str, usize)> {
    if !plain_method(definition, 1) {
        return None;
    }
    let [
        Statement {
            kind: StatementKind::Return(Some(returned)),
            ..
        },
    ] = body_without_docstring(definition)
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
    if !keywords.is_empty()
        || !arguments.iter().all(|argument| {
            matches!(
                argument.kind,
                ExpressionKind::Text(_)
                    | ExpressionKind::Scalar(_)
                    | ExpressionKind::Bool(_)
                    | ExpressionKind::None
            )
        })
    {
        return None;
    }
    let ExpressionKind::Member(field, method) = &function.kind else {
        return None;
    };
    let ExpressionKind::Member(receiver, field) = &field.kind else {
        return None;
    };
    if !matches!(&receiver.kind, ExpressionKind::Name(name) if name == &definition.parameters[0].text)
    {
        return None;
    }
    Some((returned, field, method, arguments.len()))
}
