//! Invocation-specific proofs for straight-line class argument consumers.

use super::*;

const MAX_CALLS: usize = 32;
const MAX_STEPS: usize = 96;

type Site = (ScopeId, [usize; 2], &'static str);
type Arguments = BTreeMap<String, LocalValue>;

#[derive(Clone)]
enum LocalValue {
    Core(Value),
    EnumText(String),
    FormattedText,
    FreshInstance { class: SymbolId, fields: Arguments },
}

impl LocalValue {
    fn plain_scalar(&self) -> bool {
        matches!(
            self,
            Self::FormattedText
                | Self::Core(
                    Value::Text(_)
                        | Value::Integer(_)
                        | Value::Bool(_)
                        | Value::None
                        | Value::Scalar
                )
        )
    }
}

struct Proof {
    steps: usize,
    sites: BTreeSet<Site>,
    calls: BTreeSet<(SymbolId, &'static str)>,
    reads: BTreeSet<SymbolId>,
}

impl Default for Proof {
    fn default() -> Self {
        Self {
            steps: MAX_STEPS,
            sites: BTreeSet::new(),
            calls: BTreeSet::new(),
            reads: BTreeSet::new(),
        }
    }
}

impl Proof {
    fn step(&mut self) -> Option<()> {
        self.steps = self.steps.checked_sub(1)?;
        Some(())
    }

    fn site(&mut self, scope: ScopeId, at: Location, kind: &'static str) {
        self.sites.insert((scope, position(at), kind));
    }
}

impl<'a> Graph<'a> {
    pub(super) fn activate_closed_class_calls(
        &mut self,
        reach: &mut Reachability,
        pending: &mut VecDeque<Edge>,
    ) {
        reach.closed_class_call_sites.clear();
        let wrappers: BTreeSet<_> = self
            .static_parameter_calls
            .iter()
            .filter(|site| reach.scopes.contains(&site.scope))
            .map(|site| site.scope)
            .collect();
        if wrappers.is_empty() {
            return;
        }
        let replaced = self
            .closed_class_replacements
            .take()
            .unwrap_or_else(|| self.field_callback_replacements());
        for scope in wrappers {
            self.resolve_closed_wrapper(scope, reach, pending, &replaced);
        }
        self.closed_class_replacements = Some(replaced);
    }

    fn resolve_closed_wrapper(
        &mut self,
        scope: ScopeId,
        reach: &mut Reachability,
        pending: &mut VecDeque<Edge>,
        replaced: &BTreeSet<SymbolId>,
    ) {
        let Some(owner) = self.scopes[scope].owner else {
            return;
        };
        let Binding::Definition(definition, _) = self.symbols[owner].binding else {
            return;
        };
        if replaced.contains(&owner) {
            return;
        }
        let direct: Vec<_> = self
            .direct_arguments
            .get(&owner)
            .into_iter()
            .flatten()
            .filter(|call| reach.scopes.contains(&call.caller))
            .take(MAX_CALLS + 1)
            .map(|call| {
                (
                    call.arguments_bound && !call.implicit_receiver,
                    call.values.clone(),
                )
            })
            .collect();
        if direct.is_empty() || direct.len() > MAX_CALLS {
            return;
        }
        let mut proved: Option<BTreeSet<Site>> = None;
        let mut complete = !self.reached_independently(reach, owner, "direct_call");
        for (bound, arguments) in direct {
            let Some(proof) =
                self.prove_closed_wrapper(owner, definition, replaced, bound, arguments)
            else {
                complete = false;
                continue;
            };
            let Proof {
                mut sites,
                calls,
                reads,
                ..
            } = proof;
            let external_scopes: BTreeSet<_> = calls
                .iter()
                .filter_map(|&(target, kind)| {
                    let Binding::Definition(_, body) = self.symbols[target].binding else {
                        return None;
                    };
                    let other_call = reach.scopes.iter().any(|&caller| {
                        self.scopes[caller].edges.iter().any(|edge| {
                            edge.target == target
                                && !matches!(edge.kind, "type_reference" | "class_reference")
                        })
                    });
                    (other_call || self.reached_independently(reach, target, kind)).then_some(body)
                })
                .collect();
            sites.retain(|(site_scope, _, _)| !external_scopes.contains(site_scope));
            self.read.extend(reads);

            for (target, kind) in calls {
                if !reach.has_evidence(target, kind) {
                    pending.push_back(Edge {
                        target,
                        kind,
                        certainty: "possible",
                    });
                }
            }
            if let Some(proved) = proved.as_mut() {
                proved.retain(|site| sites.contains(site));
            } else {
                proved = Some(sites);
            }
        }
        if complete {
            reach
                .closed_class_call_sites
                .extend(proved.into_iter().flatten());
        }
    }

    fn prove_closed_wrapper(
        &mut self,
        owner: SymbolId,
        definition: &'a Definition,
        replaced: &BTreeSet<SymbolId>,
        bound: bool,
        arguments: BTreeMap<String, Value>,
    ) -> Option<Proof> {
        if !bound || arguments.len() != definition.parameters.len() {
            return None;
        }
        let mut class_seen = false;
        for value in arguments.values() {
            match value {
                Value::Callable(class)
                    if !class.verified
                        && matches!(class.receiver, Receiver::Ordinary)
                        && self.is_class(class.target)
                        && self
                            .class_argument_consumers
                            .get(&class.target)
                            .is_some_and(|consumers| {
                                consumers.iter().all(|(_, callee)| *callee == owner)
                            }) =>
                {
                    class_seen = true;
                }
                Value::Text(_)
                | Value::Bool(_)
                | Value::Scalar
                | Value::None
                | Value::Integer(_) => {}
                _ => return None,
            }
        }
        if !class_seen {
            return None;
        }
        let locals = arguments
            .into_iter()
            .map(|(name, value)| (name, LocalValue::Core(value)))
            .collect();
        let Binding::Definition(_, scope) = self.symbols[owner].binding else {
            return None;
        };
        let previous = std::mem::replace(&mut self.probing_identity, true);
        let mut execution = ClosedExecution {
            graph: self,
            replaced,
            proof: Proof::default(),
        };
        let result = execution.body(scope, definition, locals);
        execution.graph.probing_identity = previous;
        result.map(|_| execution.proof)
    }

    fn reached_independently(&self, reach: &Reachability, target: SymbolId, modeled: &str) -> bool {
        reach
            .evidence
            .range((target, "", "")..)
            .take_while(|&&(symbol, _, _)| symbol == target)
            .any(|&(_, kind, _)| {
                kind != modeled && !matches!(kind, "type_reference" | "class_reference")
            })
    }
}

struct ClosedExecution<'g, 'a> {
    graph: &'g mut Graph<'a>,
    replaced: &'g BTreeSet<SymbolId>,
    proof: Proof,
}

impl<'a> ClosedExecution<'_, 'a> {
    fn body(
        &mut self,
        scope: ScopeId,
        definition: &'a Definition,
        mut locals: Arguments,
    ) -> Option<LocalValue> {
        for statement in body_without_docstring(definition) {
            self.proof.step()?;
            let at = statement.at;
            match &statement.kind {
                StatementKind::Assign {
                    names,
                    value: Some(value),
                    annotation: None,
                } => {
                    let [name] = names.as_slice() else {
                        return None;
                    };
                    let value = self.expression(scope, &locals, value, at)?;
                    locals.insert(name.text.clone(), value);
                }
                StatementKind::AssignUnpack { names, value } => {
                    let LocalValue::Core(Value::Sequence(values)) =
                        self.expression(scope, &locals, value, at)?
                    else {
                        return None;
                    };
                    if names.len() != values.len() {
                        return None;
                    }
                    for (name, value) in names.iter().zip(values) {
                        locals.insert(name.text.clone(), LocalValue::Core(value));
                    }
                    self.proof.site(scope, at, "unresolved_unpack");
                }
                StatementKind::Evaluate(value) => {
                    self.expression(scope, &locals, value, at)?;
                }
                StatementKind::Return(value) => {
                    return value
                        .as_ref()
                        .map_or(Some(LocalValue::Core(Value::None)), |value| {
                            self.expression(scope, &locals, value, at)
                        });
                }
                _ => return None,
            }
        }
        Some(LocalValue::Core(Value::None))
    }

    fn expression(
        &mut self,
        scope: ScopeId,
        locals: &Arguments,
        expression: &Expression,
        before: Location,
    ) -> Option<LocalValue> {
        self.proof.step()?;
        match &expression.kind {
            ExpressionKind::Name(name) => {
                if let Some(value) = locals.get(name) {
                    return Some(value.clone());
                }
                if matches!(
                    self.graph.lexical_binding(scope, name, before, true),
                    LexicalBinding::UnboundLocal
                ) {
                    return None;
                }
                let value = self.graph.probe_identity(scope, expression, before);
                match &value {
                    Value::Callable(invocation)
                        if !invocation.verified
                            && matches!(invocation.receiver, Receiver::Ordinary)
                            && self.graph.is_class(invocation.target)
                            && self.definition_precedes_method(scope, invocation.target) =>
                    {
                        Some(LocalValue::Core(value))
                    }
                    Value::Provider(provider)
                        if matches!(provider.as_str(), "builtins.str" | "builtins.int")
                            && self
                                .graph
                                .module_member_write("builtins", name, (scope, before))
                                .is_none() =>
                    {
                        Some(LocalValue::Core(value))
                    }
                    _ => None,
                }
            }
            ExpressionKind::Text(text) => Some(LocalValue::Core(Value::Text(text.clone()))),
            ExpressionKind::Scalar(Some(number)) => Some(LocalValue::Core(Value::Integer(*number))),
            ExpressionKind::Bool(value) => Some(LocalValue::Core(Value::Bool(*value))),
            ExpressionKind::None => Some(LocalValue::Core(Value::None)),
            ExpressionKind::Member(base, name) => {
                match self.expression(scope, locals, base, before)? {
                    LocalValue::FreshInstance { fields, .. } => fields.get(name).cloned(),
                    LocalValue::EnumText(text) if name == "value" => {
                        Some(LocalValue::Core(Value::Text(text)))
                    }
                    LocalValue::Core(Value::Callable(invocation)) => {
                        self.graph
                            .static_parameter_class(invocation.target, scope)?;
                        let value = LocalValue::Core(self.graph.class_member_at(
                            invocation.target,
                            name,
                            0,
                            Some((scope, before)),
                        )?);
                        value.plain_scalar().then_some(value)
                    }
                    _ => None,
                }
            }
            ExpressionKind::Call {
                function,
                arguments,
                keywords,
            } if keywords.is_empty() => {
                self.call(scope, locals, function, arguments, (before, expression.at))
            }
            ExpressionKind::FormattedString(expressions) => {
                for value in expressions {
                    if !self
                        .expression(scope, locals, value, before)?
                        .plain_scalar()
                    {
                        return None;
                    }
                    self.proof.site(scope, value.at, "unresolved_formatting");
                }
                // Only the result type is needed, since the literal fragments
                // of formatted strings are not retained by the frontend.
                Some(LocalValue::FormattedText)
            }
            _ => None,
        }
    }

    fn call(
        &mut self,
        scope: ScopeId,
        locals: &Arguments,
        function: &Expression,
        operands: &[Expression],
        (before, at): (Location, Location),
    ) -> Option<LocalValue> {
        let (receiver, target) = if let ExpressionKind::Member(base, method) = &function.kind {
            (
                Some(self.expression(scope, locals, base, before)?),
                method.as_str(),
            )
        } else {
            (Some(self.expression(scope, locals, function, before)?), "")
        };
        let arguments = operands
            .iter()
            .map(|value| self.expression(scope, locals, value, before))
            .collect::<Option<Vec<_>>>()?;
        let result = match (receiver?, target, arguments.as_slice()) {
            (
                LocalValue::Core(Value::Text(text)),
                "split",
                [LocalValue::Core(Value::Text(separator))],
            ) => {
                let items = literal_text::split_text(&text, separator)?
                    .into_iter()
                    .map(Value::Text)
                    .collect();
                LocalValue::Core(Value::Sequence(items))
            }
            (
                LocalValue::Core(Value::Provider(name)),
                "",
                [LocalValue::Core(Value::Text(text))],
            ) if name == "builtins.int" => LocalValue::Core(Value::Integer(text.parse().ok()?)),
            (LocalValue::Core(Value::Provider(name)), "", [value]) if name == "builtins.str" => {
                if let LocalValue::FreshInstance { class, .. } = value {
                    let Value::Callable(invocation) =
                        self.graph
                            .class_member_at(*class, "__str__", 0, Some((scope, before)))?
                    else {
                        return None;
                    };
                    let value = self.invoke(
                        invocation.target,
                        std::slice::from_ref(value),
                        "formatting_hook",
                    )?;
                    if !matches!(
                        value,
                        LocalValue::FormattedText | LocalValue::Core(Value::Text(_))
                    ) {
                        return None;
                    }
                    value
                } else {
                    return None;
                }
            }
            (LocalValue::Core(Value::Callable(invocation)), "", _) => {
                self.allocate(scope, invocation.target, &arguments, before, at)?
            }
            (LocalValue::Core(Value::Callable(invocation)), member, _) => {
                let class = invocation.target;
                let class_scope = self.graph.static_parameter_class(class, scope)?;
                let target = self.graph.static_parameter_method(
                    class,
                    class_scope,
                    member,
                    arguments.len(),
                    (scope, before),
                )?;
                self.invoke(target, &arguments, "direct_call")?
            }
            _ => return None,
        };
        self.proof.site(scope, at, "unresolved_call");
        Some(result)
    }

    fn allocate(
        &mut self,
        scope: ScopeId,
        class: SymbolId,
        arguments: &[LocalValue],
        before: Location,
        at: Location,
    ) -> Option<LocalValue> {
        if self.graph.has_unmodeled_class_write(class)
            || !self.graph.class_discovery_writes(class, scope).is_empty()
        {
            return None;
        }
        if let [LocalValue::Core(Value::Text(lookup))] = arguments
            && let Some(members) = self.graph.string_enum_members(class, 0)
        {
            if self
                .graph
                .module_member_write("enum", "Enum", (scope, before))
                .is_some()
            {
                return None;
            }
            let (member, value) = members.into_iter().find(|(_, value)| value == lookup)?;
            self.proof.reads.insert(member);
            self.proof.site(scope, at, "unresolved_constructor");
            return Some(LocalValue::EnumText(value.to_owned()));
        }
        let Value::Callable(initializer) =
            self.graph
                .class_member_at(class, "__init__", 0, Some((scope, before)))?
        else {
            return None;
        };
        if self.replaced.contains(&initializer.target)
            || !self
                .graph
                .plain_argument_initializer(class, initializer.target)
        {
            return None;
        }
        let Binding::Definition(definition, _) = self.graph.symbols[initializer.target].binding
        else {
            return None;
        };
        if definition.parameters.len() != arguments.len() + 1 {
            return None;
        }
        let mut fields = Arguments::new();
        for statement in body_without_docstring(definition) {
            let StatementKind::WriteAttribute {
                member,
                value: Some(value),
                ..
            } = &statement.kind
            else {
                return None;
            };
            let ExpressionKind::Name(name) = &value.kind else {
                return None;
            };
            let argument = definition.parameters[1..]
                .iter()
                .position(|parameter| &parameter.text == name)?;
            fields.insert(member.clone(), arguments[argument].clone());
        }
        self.proof.calls.insert((initializer.target, "constructor"));
        Some(LocalValue::FreshInstance { class, fields })
    }

    fn definition_precedes_method(&self, scope: ScopeId, target: SymbolId) -> bool {
        let Some(owner) = self.graph.scopes[scope].owner else {
            return false;
        };
        let Some(class) = self.graph.scopes[self.graph.symbols[owner].scope].owner else {
            return false;
        };
        self.graph.symbols[target].scope == self.graph.symbols[class].scope
            && position(self.graph.symbols[target].name.at)
                < position(self.graph.symbols[class].name.at)
    }

    fn invoke(
        &mut self,
        target: SymbolId,
        args: &[LocalValue],
        kind: &'static str,
    ) -> Option<LocalValue> {
        let Binding::Definition(definition, scope) = self.graph.symbols[target].binding else {
            return None;
        };
        if self.replaced.contains(&target)
            || definition.parameters.len() != args.len()
            || !definition.simple_positional_parameters
            || definition.is_generator
            || definition.kind != DefinitionKind::Function
        {
            return None;
        }
        let arguments = definition
            .parameters
            .iter()
            .zip(args)
            .map(|(parameter, value)| (parameter.text.clone(), value.clone()))
            .collect();
        let value = self.body(scope, definition, arguments)?;
        self.proof.calls.insert((target, kind));
        Some(value)
    }
}
