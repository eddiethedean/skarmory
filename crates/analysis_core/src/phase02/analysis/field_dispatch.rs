//! Source-owned dispatch through a field of a freshly constructed receiver.

use super::*;

pub(super) const DIRECT: &str = "fresh_field_call";
const MAX_INITIALIZER_FIELDS: usize = 32;

pub(super) struct FreshFieldInvocation {
    caller: ScopeId,
    at: Location,
    pub(super) method: Invocation,
    provider_method: Invocation,
    dependencies: [SymbolId; 3],
}

pub(super) struct ConstructorHookStore {
    scope: ScopeId,
    initializer: SymbolId,
    at: Location,
    field: String,
    factory: String,
}

enum ConstructorField<'a> {
    FromArgument(&'a str),
    Literal,
}

impl Graph<'_> {
    pub(super) fn invoke_fresh_field_method(
        &mut self,
        invocation: Invocation,
        function: &Expression,
        call: CallArguments<'_>,
    ) -> bool {
        let Some(proof) = self.prove_fresh_field_method(invocation, function, call, None) else {
            return false;
        };
        self.fresh_field_invocations.push(proof);
        true
    }

    pub(super) fn prove_selected_field_call(
        &mut self,
        scope: ScopeId,
        expression: &Expression,
        before: Location,
        parameter: &str,
        text: &str,
    ) -> Option<FreshFieldInvocation> {
        let ExpressionKind::Call {
            function,
            arguments,
            keywords,
        } = &expression.kind
        else {
            return None;
        };
        let Value::Callable(invocation) = self.probe_identity(scope, function, before) else {
            return None;
        };
        self.prove_fresh_field_method(
            invocation,
            function,
            CallArguments {
                scope,
                before,
                at: expression.at,
                positional: arguments,
                keywords,
            },
            Some((parameter, text)),
        )
    }

    fn prove_fresh_field_method(
        &mut self,
        invocation: Invocation,
        function: &Expression,
        call: CallArguments<'_>,
        selector: Option<(&str, &str)>,
    ) -> Option<FreshFieldInvocation> {
        if invocation.verified || !call.positional.is_empty() || !call.keywords.is_empty() {
            return None;
        }
        let Receiver::Bound(class) = invocation.receiver else {
            return None;
        };
        let ExpressionKind::Member(receiver, _) = &function.kind else {
            return None;
        };
        let ExpressionKind::Call {
            function,
            arguments,
            keywords,
        } = &receiver.kind
        else {
            return None;
        };
        let [stored] = arguments.as_slice() else {
            return None;
        };
        if !keywords.is_empty()
            || !matches!(self.probe_identity(call.scope, function, call.before),
                Value::Callable(constructor) if constructor.target == class && !constructor.verified)
        {
            return None;
        }
        let Binding::Definition(method, _) = self.symbols[invocation.target].binding else {
            return None;
        };
        let (_, field, member, supplied) = field_method(method)?;
        if self.scopes[self.symbols[invocation.target].scope].owner != Some(class)
            || !matches!(self.fresh_constructor_field(class), Some(ConstructorField::FromArgument(member)) if member == field)
            || !self.class_discovery_writes(class, call.scope).is_empty()
        {
            return None;
        }
        let (provider, factory) =
            self.literal_factory_instance(call.scope, stored, call.before, selector)?;
        let access = Some((call.scope, call.before));
        let Some(Value::Callable(selected)) = self.class_member_at(provider, member, 0, access)
        else {
            return None;
        };
        let Binding::Definition(selected_definition, _) = self.symbols[selected.target].binding
        else {
            return None;
        };
        if selected.verified || !plain_method(selected_definition, supplied + 1) {
            return None;
        }
        Some(FreshFieldInvocation {
            caller: call.scope,
            at: call.at,
            method: invocation,
            provider_method: selected.bind_receiver(Receiver::Bound(provider)),
            dependencies: [class, provider, factory],
        })
    }

    pub(super) fn resolve_fresh_field_invocations(&mut self) {
        if self.fresh_field_invocations.is_empty() {
            return;
        }
        // The body walker also records callable escapes. Check them after
        // every module has been visited so proof does not depend on file order.
        let escaped = self.escaped_field_callbacks();
        let replaced = self.field_callback_replacements();
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

    pub(super) fn escaped_field_callbacks(&self) -> BTreeSet<SymbolId> {
        self.scopes
            .iter()
            .flat_map(|scope| {
                scope.edges.iter().filter_map(|edge| {
                    matches!(edge.kind, "passed_callable" | "returned_callable")
                        .then_some(edge.target)
                })
            })
            .collect()
    }

    pub(super) fn field_callback_replacements(&mut self) -> BTreeSet<SymbolId> {
        let mut replaced = BTreeSet::new();
        let reads = std::mem::take(&mut self.read);
        for scope in 0..self.scopes.len() {
            self.collect_field_callback_replacements(scope, self.scopes[scope].body, &mut replaced);
        }
        self.read = reads;
        replaced
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
            && (self.fresh_constructor_field(class).is_some()
                || self.plain_argument_initializer(class, initializer))
            && self.class_discovery_writes(class, scope).is_empty()
            && self
                .receiver_parameter(initializer)
                .is_some_and(|parameter| self.instance_receivers.get(&parameter) == Some(&class))
            && self.scopes[scope]
                .body
                .iter()
                .any(|entry| std::ptr::eq(entry, statement))
    }

    pub(super) fn defer_constructor_hook_store(
        &mut self,
        scope: ScopeId,
        statement: &Statement,
    ) -> bool {
        let Some(initializer) = self.scopes[scope].owner else {
            return false;
        };
        let symbol = &self.symbols[initializer];
        let Binding::Definition(definition, _) = symbol.binding else {
            return false;
        };
        let [only] = body_without_docstring(definition) else {
            return false;
        };
        if symbol.name.text != "__init__"
            || !self.scopes[symbol.scope]
                .owner
                .is_some_and(|class| self.is_class(class))
            || !plain_method(definition, 1)
            || !std::ptr::eq(only, statement)
        {
            return false;
        }
        let StatementKind::WriteAttribute {
            receiver,
            member,
            value: Some(value),
        } = &statement.kind
        else {
            return false;
        };
        let ExpressionKind::Call {
            function,
            arguments,
            keywords,
        } = &value.kind
        else {
            return false;
        };
        let ExpressionKind::Member(factory_receiver, factory) = &function.kind else {
            return false;
        };
        let parameter = &definition.parameters[0].text;
        if member.starts_with("__")
            || factory.starts_with("__")
            || member == factory
            || !arguments.is_empty()
            || !keywords.is_empty()
            || !matches!(&receiver.kind, ExpressionKind::Name(name) if name == parameter)
            || !matches!(&factory_receiver.kind, ExpressionKind::Name(name) if name == parameter)
        {
            return false;
        }
        self.constructor_hook_stores.push(ConstructorHookStore {
            scope,
            initializer,
            at: statement.at,
            field: member.clone(),
            factory: factory.clone(),
        });
        true
    }

    pub(super) fn report_constructor_hook_stores(&mut self, reach: &Reachability) {
        for store in std::mem::take(&mut self.constructor_hook_stores) {
            if !reach.scopes.contains(&store.scope) {
                continue;
            }
            let receivers: BTreeSet<_> = self
                .method_receivers
                .get(&store.initializer)
                .into_iter()
                .chain(reach.deferred_method_receivers.get(&store.initializer))
                .flatten()
                .copied()
                .collect();
            if receivers.is_empty()
                || !receivers
                    .into_iter()
                    .all(|class| self.prove_constructor_hook_store(class, &store))
            {
                self.issues.push(
                    Issue::new(
                        "unresolved_attribute_write",
                        &self.files[self.scopes[store.scope].file].path,
                        "constructor hook storage requires ordinary source-owned allocation and attribute lookup",
                    )
                    .at(store.at),
                );
            }
        }
    }

    fn prove_constructor_hook_store(
        &mut self,
        class: SymbolId,
        store: &ConstructorHookStore,
    ) -> bool {
        let (lineage, complete) = self.class_lineage(class);
        if !complete
            || !lineage.into_iter().all(|body| {
                let owner = self.scopes[body].owner.expect("class body has an owner");
                let Binding::Definition(class, _) = self.symbols[owner].binding else {
                    unreachable!();
                };
                !class.has_class_keywords()
                    && class.decorators.is_empty()
                    && !self.has_unmodeled_class_write(owner)
                    && self.class_discovery_writes(owner, store.scope).is_empty()
                    && [
                        "__new__",
                        "__getattribute__",
                        "__getattr__",
                        "__setattr__",
                        "__slots__",
                        &store.field,
                    ]
                    .into_iter()
                    .all(|name| self.local(body, name, END).is_none())
            })
        {
            return false;
        }
        let Some(Value::Callable(factory)) =
            self.class_member_at(class, &store.factory, 0, Some((store.scope, store.at)))
        else {
            return false;
        };
        !factory.verified
            && matches!(factory.receiver, Receiver::Ordinary)
            && !self.has_unmodeled_class_write(factory.target)
            && self
                .class_discovery_writes(factory.target, store.scope)
                .is_empty()
            && matches!(self.symbols[factory.target].binding, Binding::Definition(definition, _)
                if definition.kind == DefinitionKind::Class
                    && definition.decorators.is_empty() && definition.bases.is_empty()
                    && !definition.has_class_keywords() && body_without_docstring(definition).is_empty())
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

    fn fresh_field_class(&self, class: SymbolId) -> Option<&Definition> {
        let Binding::Definition(definition, _) = self.symbols[class].binding else {
            return None;
        };
        (definition.kind == DefinitionKind::Class
            && definition.bases.is_empty()
            && !definition.has_class_keywords()
            && definition.decorators.is_empty()
            && !self.subclassed.contains(&class))
        .then_some(definition)
    }

    pub(super) fn plain_argument_initializer(
        &self,
        class: SymbolId,
        initializer: SymbolId,
    ) -> bool {
        let Some(class) = self.fresh_field_class(class) else {
            return false;
        };
        let Binding::Definition(method, _) = self.symbols[initializer].binding else {
            return false;
        };
        let Some((receiver, parameters)) = method.parameters.split_first() else {
            return false;
        };
        let writes = body_without_docstring(method);
        if !plain_method(method, method.parameters.len())
            || parameters.len() > MAX_INITIALIZER_FIELDS
            || writes.is_empty()
            || writes.len() > MAX_INITIALIZER_FIELDS
        {
            return false;
        }
        let mut fields = BTreeSet::new();
        for statement in writes {
            let StatementKind::WriteAttribute {
                receiver: target,
                member,
                value: Some(value),
            } = &statement.kind
            else {
                return false;
            };
            if member.starts_with("__")
                || !matches!(&target.kind, ExpressionKind::Name(name) if name == &receiver.text)
                || !matches!(&value.kind, ExpressionKind::Name(name)
                    if parameters.iter().any(|parameter| name == &parameter.text))
                || !fields.insert(member.as_str())
            {
                return false;
            }
        }
        // Plain functions elsewhere in the class cannot intercept instance
        // stores. Exclude data descriptors and special attribute machinery.
        body_without_docstring(class).iter().all(|statement| {
            let StatementKind::Define(definition) = &statement.kind else {
                return false;
            };
            let name = definition.name.text.as_str();
            std::ptr::eq(definition, method)
                || (definition.kind == DefinitionKind::Function
                    && definition.decorators.is_empty()
                    && (name == "__str__" || !name.starts_with("__"))
                    && !fields.contains(name))
        })
    }

    fn fresh_constructor_field(&self, class: SymbolId) -> Option<ConstructorField<'_>> {
        let definition = self.fresh_field_class(class)?;
        let mut field = None;
        for statement in body_without_docstring(definition) {
            let StatementKind::Define(method) = &statement.kind else {
                return None;
            };
            if !method.decorators.is_empty() {
                return None;
            }
            if method.name.text == "__init__" {
                if field.is_some() {
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
                if member.starts_with("__")
                    || !matches!(&receiver.kind,
                    ExpressionKind::Name(name) if method.parameters.first().is_some_and(|parameter| parameter.text == *name))
                {
                    return None;
                }
                field = match &value.kind {
                    ExpressionKind::Name(argument)
                        if plain_method(method, 2) && argument == &method.parameters[1].text =>
                    {
                        Some(ConstructorField::FromArgument(member))
                    }
                    ExpressionKind::Sequence { items, .. }
                        if plain_method(method, 1)
                            && items.iter().all(|item| {
                                matches!(
                                    item.kind,
                                    ExpressionKind::Text(_)
                                        | ExpressionKind::Scalar(_)
                                        | ExpressionKind::Bool(_)
                                        | ExpressionKind::None
                                )
                            }) =>
                    {
                        Some(ConstructorField::Literal)
                    }
                    _ => return None,
                };
            } else if method.name.text.starts_with("__") {
                return None;
            }
        }
        field
    }

    pub(super) fn literal_factory_instance(
        &mut self,
        scope: ScopeId,
        expression: &Expression,
        before: Location,
        selector: Option<(&str, &str)>,
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
        let actual = if let Some((parameter, value)) = selector {
            if !matches!(&argument.kind, ExpressionKind::Name(argument) if argument == parameter) {
                return None;
            }
            value.to_owned()
        } else if let Value::Text(value) = self.probe_identity(scope, argument, before) {
            value
        } else {
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
        (!constructor.verified
            && self.plain_default_provider(class, (scope, before), &BTreeSet::new()))
        .then_some((class, factory.target))
    }

    pub(super) fn plain_default_provider(
        &mut self,
        class: SymbolId,
        access: (ScopeId, Location),
        invalidated: &BTreeSet<SymbolId>,
    ) -> bool {
        self.plain_provider_class(class, access.0, invalidated, 0)
            && [
                "__new__",
                "__init__",
                "__init_subclass__",
                "__getattribute__",
                "__getattr__",
            ]
            .iter()
            .all(|name| self.class_member_at(class, name, 0, Some(access)).is_none())
    }

    fn plain_provider_class(
        &mut self,
        class: SymbolId,
        caller: ScopeId,
        invalidated: &BTreeSet<SymbolId>,
        depth: usize,
    ) -> bool {
        let symbol = &self.symbols[class];
        let Binding::Definition(definition, _) = symbol.binding else {
            return false;
        };
        if depth >= MAX_BINDING_CHAIN
            || definition.kind != DefinitionKind::Class
            || definition.has_class_keywords()
            || !definition.decorators.is_empty()
            || invalidated.contains(&class)
            || self.has_unmodeled_class_write(class)
            || !self.class_discovery_writes(class, caller).is_empty()
        {
            return false;
        }
        match definition.bases.as_slice() {
            [] => true,
            [base] => match self.probe_identity(symbol.scope, base, definition.name.at) {
                Value::Provider(name) => name == "builtins.object",
                Value::Callable(parent) if !parent.verified => {
                    self.plain_provider_class(parent.target, caller, invalidated, depth + 1)
                }
                _ => false,
            },
            _ => false,
        }
    }
}

pub(super) fn plain_method(definition: &Definition, parameters: usize) -> bool {
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
