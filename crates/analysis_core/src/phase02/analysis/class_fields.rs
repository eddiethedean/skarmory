//! Declaration reads for plain source class values.

use super::*;

#[derive(Default)]
pub(super) struct Uses {
    parameter_reads: BTreeSet<(ScopeId, ScopeId, SymbolId, String)>,
    printed_classes: BTreeSet<(ScopeId, SymbolId)>,
}

impl Graph<'_> {
    pub(super) fn record_fresh_class_field_read(
        &mut self,
        callee: SymbolId,
        definition: &Definition,
        call: CallArguments<'_>,
    ) {
        if definition.is_generator || !definition.decorators.is_empty() {
            return;
        }
        let [parameter] = definition.parameters.as_slice() else {
            return;
        };
        let [statement] = body_without_docstring(definition) else {
            return;
        };
        let StatementKind::Return(Some(Expression {
            kind: ExpressionKind::Member(receiver, member),
            ..
        })) = &statement.kind
        else {
            return;
        };
        if member.starts_with('_')
            || !matches!(&receiver.kind, ExpressionKind::Name(name) if name == &parameter.text)
        {
            return;
        }
        let (argument, keyword) = match (call.positional, call.keywords) {
            ([argument], []) => (argument, None),
            ([], [(keyword, argument)]) => (argument, Some(keyword)),
            _ => return,
        };
        if keyword.is_some_and(|name| name != &parameter.text) {
            return;
        }
        // A named instance can shadow the field. A proven class value
        // supplies the class attribute directly.
        let class = match self.plain_provider_constructor(call.scope, argument, call.before) {
            Some(class) => class,
            None if field_dispatch::plain_method(definition, 1) => {
                let Value::Callable(value) = self.probe_identity(call.scope, argument, call.before)
                else {
                    return;
                };
                if !self.is_returned_literal_class(value, call.scope) {
                    return;
                }
                value.target
            }
            None => return,
        };
        let Binding::Definition(_, body) = self.symbols[callee].binding else {
            return;
        };
        self.class_field_uses
            .parameter_reads
            .insert((call.scope, body, class, member.clone()));
    }

    pub(super) fn returned_literal_class(
        &self,
        factory: SymbolId,
        caller: ScopeId,
    ) -> Option<SymbolId> {
        let Binding::Definition(definition, scope) = self.symbols[factory].binding else {
            return None;
        };
        if !field_dispatch::plain_method(definition, 0) {
            return None;
        }
        let [
            Statement {
                kind: StatementKind::Define(class),
                ..
            },
            Statement {
                kind:
                    StatementKind::Return(Some(Expression {
                        kind: ExpressionKind::Name(name),
                        ..
                    })),
                ..
            },
        ] = body_without_docstring(definition)
        else {
            return None;
        };
        if class.kind != DefinitionKind::Class || *name != class.name.text {
            return None;
        }
        let id = self.local(scope, name, END)?;
        self.literal_class_body(id, caller)?;
        Some(id)
    }

    fn is_returned_literal_class(&self, value: Invocation, scope: ScopeId) -> bool {
        let class = value.target;
        !value.verified
            && self.is_class(class)
            && self.scopes[self.symbols[class].scope]
                .owner
                .is_some_and(|factory| self.returned_literal_class(factory, scope) == Some(class))
    }

    pub(super) fn record_printed_returned_class(&mut self, scope: ScopeId, value: Invocation) {
        if self.is_returned_literal_class(value, scope) {
            // The pinned profile retains fields when print directly consumes
            // the factory call. A field lookup uses ordinary class resolution.
            self.class_field_uses
                .printed_classes
                .insert((scope, value.target));
        }
    }

    pub(super) fn retain_source_class_fields(&mut self, reach: &mut Reachability) {
        let uses = std::mem::take(&mut self.class_field_uses);
        for (caller, body, class, member) in uses.parameter_reads {
            if reach.scopes.contains(&caller)
                && reach.scopes.contains(&body)
                && let Some(class_body) = self.literal_class_body(class, caller)
                && self.scopes[self.symbols[class].scope]
                    .class_bodies
                    .contains(&class_body)
                && let Some(field) = self.local(class_body, &member, END)
            {
                self.read.insert(field);
                reach
                    .evidence
                    .insert((field, "class_attribute_read", "possible"));
            }
        }

        for (scope, class) in uses.printed_classes {
            if !reach.scopes.contains(&scope) {
                continue;
            }
            let Some(body) = self.literal_class_body(class, scope) else {
                continue;
            };
            for (name, bindings) in &self.scopes[body].names {
                if !name.starts_with('_')
                    && let Some(&field) = bindings.last()
                    && matches!(self.symbols[field].binding, Binding::Variable(Some(_), _))
                {
                    self.read.insert(field);
                    reach
                        .evidence
                        .insert((field, "returned_class_attribute", "possible"));
                }
            }
        }
    }

    fn literal_class_body(&self, class: SymbolId, consumer: ScopeId) -> Option<ScopeId> {
        let declaration = &self.symbols[class];
        let Binding::Definition(definition, body) = declaration.binding else {
            return None;
        };
        (definition.kind == DefinitionKind::Class
            && definition.bases.is_empty()
            && definition.decorators.is_empty()
            && !definition.has_class_keywords()
            && !self.has_unmodeled_class_write(class)
            && self.class_discovery_writes(class, consumer).is_empty()
            && body_without_docstring(definition).iter().all(|statement| {
                matches!(&statement.kind, StatementKind::Assign { names, value: Some(value), annotation: None }
                    if names.iter().all(|name| !name.text.starts_with("__")) && literal_json_value(value, 0))
            })).then_some(body)
    }
}
