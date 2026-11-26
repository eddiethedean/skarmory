//! Bounded activation of Pydantic serialization callbacks.

use super::*;

const SERIALIZER: &str = "pydantic_serializer";

#[derive(Clone, Copy)]
enum Callback {
    Field,
    Computed,
    Model,
    Wrap { delegate: Option<SymbolId> },
    Opaque,
}

#[derive(Default)]
pub(super) struct Serialization {
    callbacks: BTreeMap<SymbolId, Callback>,
    dumps: Vec<(SymbolId, ScopeId, Location, bool)>,
    reads: Vec<(Invocation, ScopeId, Location)>,
}

impl Graph<'_> {
    pub(super) fn record_pydantic_dump(
        &mut self,
        class: SymbolId,
        scope: ScopeId,
        at: Location,
        default_options: bool,
    ) {
        self.pydantic_serialization
            .dumps
            .push((class, scope, at, default_options));
    }

    pub(super) fn record_pydantic_serializer(
        &mut self,
        scope: ScopeId,
        target: SymbolId,
        provider: &str,
        args: &[Expression],
        keywords: &[(String, Expression)],
    ) {
        let Some(class) = self.scopes[scope].owner else {
            return;
        };
        if !self.is_class(class)
            || self.framework_class_role(class, &mut BTreeSet::new(), 0)
                != Some(FrameworkClassRole::PydanticModel)
        {
            return;
        }
        let Binding::Definition(definition, body) = self.symbols[target].binding else {
            unreachable!("serializer decorators apply to definitions");
        };
        let computed = provider == "pydantic.computed_field";
        let supported_decorators = match definition.decorators.as_slice() {
            [_] => true,
            [_, property] if computed => {
                matches!(self.identity(scope, property, definition.name.at, 0),
                    Value::Provider(name) if name == "builtins.property")
            }
            _ => false,
        };
        let mode = match keywords {
            [] => Some("plain"),
            [(key, mode)] if key == "mode" => match &mode.kind {
                ExpressionKind::Text(value) => Some(value.as_str()),
                _ => None,
            },
            _ => None,
        };
        let callback = if definition.kind != DefinitionKind::Function
            || !supported_decorators
            || !definition.simple_positional_parameters
            || definition.is_generator
        {
            Callback::Opaque
        } else {
            match (provider, mode, args, definition.parameters.as_slice()) {
                ("pydantic.computed_field", Some("plain"), [], [_]) if keywords.is_empty() => {
                    Callback::Computed
                }
                ("pydantic.model_serializer", Some("plain"), [], [_, ..]) => Callback::Model,
                ("pydantic.model_serializer", Some("wrap"), [], [receiver, handler, ..]) => {
                    Callback::Wrap {
                        delegate: delegates_to_handler(definition, receiver, handler)
                            .then(|| self.local(body, &handler.text, END))
                            .flatten(),
                    }
                }
                ("pydantic.field_serializer", Some("plain"), [_, ..], [_, _, ..])
                    if args
                        .iter()
                        .all(|arg| matches!(&arg.kind, ExpressionKind::Text(_))) =>
                {
                    Callback::Field
                }
                _ => Callback::Opaque,
            }
        };
        self.pydantic_serialization
            .callbacks
            .insert(target, callback);
    }

    pub(super) fn read_pydantic_computed_field(
        &mut self,
        scope: ScopeId,
        value: &Value,
        at: Location,
    ) {
        if self.phase03
            && let Value::Callable(invocation) = value
            && let Receiver::Bound(class) = invocation.receiver
            && self.framework_class_role(class, &mut BTreeSet::new(), 0)
                == Some(FrameworkClassRole::PydanticModel)
        {
            self.pydantic_serialization
                .reads
                .push((*invocation, scope, at));
        }
    }

    pub(super) fn collect_pydantic_serialization(&mut self) {
        for (invocation, scope, at) in std::mem::take(&mut self.pydantic_serialization.reads) {
            if matches!(
                self.pydantic_serialization
                    .callbacks
                    .get(&invocation.target),
                Some(Callback::Computed)
            ) {
                self.invoke(scope, invocation, SERIALIZER, "certain", at);
            }
        }
        for (class, scope, at, default_options) in
            std::mem::take(&mut self.pydantic_serialization.dumps)
        {
            let (lineage, _) = self.class_lineage(class);
            let mut seen_names = BTreeSet::new();
            let mut fields = Vec::new();
            let mut models = Vec::new();
            let mut unresolved = Vec::new();
            let mut opaque = false;
            for parent in lineage {
                for (&name, bindings) in &self.scopes[parent].names {
                    let shadowed = !seen_names.insert(name);
                    let candidates = self.effective_bindings(bindings);
                    for &target in candidates {
                        let Some(callback) = self.pydantic_serialization.callbacks.get(&target)
                        else {
                            continue;
                        };
                        if shadowed {
                            opaque = true;
                            continue;
                        }
                        match callback {
                            Callback::Field | Callback::Computed => fields.push(target),
                            Callback::Model | Callback::Wrap { .. } => models.push(target),
                            Callback::Opaque => unresolved.push(target),
                        }
                        opaque |= candidates.len() != 1 || self.conditional.contains_key(&target);
                    }
                }
            }
            if fields.is_empty() && models.is_empty() && unresolved.is_empty() && !opaque {
                continue;
            }
            opaque |= models.len() > 1 || !default_options || !unresolved.is_empty();
            let fields_certainty = match (opaque, models.as_slice()) {
                (false, []) => Some("certain"),
                (false, [model]) => match self.pydantic_serialization.callbacks[model] {
                    Callback::Model => None,
                    Callback::Wrap { delegate: Some(_) } => Some("certain"),
                    _ => Some("possible"),
                },
                _ => Some("possible"),
            };
            if opaque {
                self.scope_issue(
                    scope,
                    at,
                    "unresolved_registration",
                    "Pydantic serializer selection is outside the bounded model",
                );
            }
            for target in models.into_iter().chain(unresolved) {
                self.invoke(
                    scope,
                    Invocation::ordinary(target).bind_receiver(Receiver::Bound(class)),
                    SERIALIZER,
                    if opaque { "possible" } else { "certain" },
                    at,
                );
            }
            if let Some(certainty) = fields_certainty {
                for target in fields {
                    self.invoke(
                        scope,
                        Invocation::ordinary(target).bind_receiver(Receiver::Bound(class)),
                        SERIALIZER,
                        certainty,
                        at,
                    );
                }
            }
        }
    }

    pub(super) fn pydantic_resolves_handler(
        &self,
        call: &ParameterCall,
        owner: SymbolId,
        evidence: &BTreeSet<(SymbolId, &'static str, &'static str)>,
        reachable_scopes: &BTreeSet<ScopeId>,
    ) -> bool {
        if !matches!(self.pydantic_serialization.callbacks.get(&owner),
            Some(Callback::Wrap { delegate: Some(parameter) }) if *parameter == call.parameter)
            || !evidence.contains(&(owner, SERIALIZER, "certain"))
        {
            return false;
        }
        // A separately reached explicit call must still supply a known handler.
        let parameter = &self.symbols[call.parameter].name.text;
        self.direct_arguments
            .get(&owner)
            .into_iter()
            .flatten()
            .filter(|site| reachable_scopes.contains(&site.caller))
            .all(|site| matches!(site.values.get(parameter), Some(Value::Callable(_))))
            && evidence
                .iter()
                .filter(|(target, _, _)| *target == owner)
                .all(|(_, kind, _)| matches!(*kind, SERIALIZER | "direct_call" | "type_reference"))
    }
}

fn delegates_to_handler(definition: &Definition, receiver: &Name, handler: &Name) -> bool {
    matches!(definition.body.as_slice(), [statement] if matches!(&statement.kind,
        StatementKind::Return(Some(Expression {
            kind: ExpressionKind::Call { function, arguments, keywords }, ..
        })) if keywords.is_empty()
            && matches!(&function.kind, ExpressionKind::Name(name) if *name == handler.text)
            && matches!(arguments.as_slice(), [argument] if matches!(&argument.kind,
                ExpressionKind::Name(name) if *name == receiver.text))))
}
