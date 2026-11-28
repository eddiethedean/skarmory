//! Source-owned builtin properties used immediately as method receivers.

use super::*;

pub(super) struct SourcePropertyInvocation {
    caller: ScopeId,
    at: Location,
    getter: Invocation,
    method: Invocation,
    classes: [SymbolId; 2],
}

impl<'a> Graph<'a> {
    pub(super) fn source_property_value(&self, value: Value) -> Value {
        match value {
            Value::Callable(invocation)
                if self.source_property_getter(invocation.target).is_some() =>
            {
                Value::SourceProperty(invocation)
            }
            Value::Alternatives(values) => Value::alternatives(
                values
                    .into_iter()
                    .map(|value| self.source_property_value(value))
                    .collect(),
            ),
            value => value,
        }
    }

    pub(super) fn source_property_getter(&self, target: SymbolId) -> Option<&'a Expression> {
        let symbol = &self.symbols[target];
        let Binding::Definition(definition, _) = symbol.binding else {
            return None;
        };
        let scope = symbol.scope;
        let [decorator] = definition.decorators.as_slice() else {
            return None;
        };
        let [parameter] = definition.parameters.as_slice() else {
            return None;
        };
        let file = self.scopes[scope].file;
        if !matches!(&decorator.kind, ExpressionKind::Name(name) if name == "property")
            || !matches!(
                self.lexical_binding(scope, "property", definition.name.at, true),
                LexicalBinding::Missing
            )
            || self.scopes[scope].names.contains_key("property")
            || self.scopes[file].open_wildcard
            || self
                .module_member_write("builtins", "property", (scope, definition.name.at))
                .is_some()
            || !self.scopes[scope].owner.is_some_and(|id| self.is_class(id))
            || definition.kind != DefinitionKind::Function
            || definition.is_generator
            || !definition.simple_positional_parameters
            || !definition.defaults.is_empty()
        {
            return None;
        }
        let [
            Statement {
                kind: StatementKind::Return(Some(value)),
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
        } = &value.kind
        else {
            return None;
        };
        if !arguments.is_empty()
            || !keywords.is_empty()
            || !matches!(&function.kind, ExpressionKind::Name(name) if name != &parameter.text)
        {
            return None;
        }
        Some(value)
    }

    pub(super) fn walk_source_property_call(
        &mut self,
        scope: ScopeId,
        function: &Expression,
        before: Location,
        at: Location,
    ) -> bool {
        let ExpressionKind::Member(property_access, method_name) = &function.kind else {
            return false;
        };
        let ExpressionKind::Member(receiver, property_name) = &property_access.kind else {
            return false;
        };
        // The getter's constructor name is read in its module at execution.
        // Limit the proof to top-level callers in that same module.
        if scope != self.scopes[scope].file {
            return false;
        }
        let access = (scope, before);
        let Some(owner) = self.plain_provider_constructor(scope, receiver, before) else {
            return false;
        };
        if self.symbols[owner].scope != scope {
            return false;
        }
        let Some(Value::SourceProperty(getter)) =
            self.class_member_at(owner, property_name, 0, Some(access))
        else {
            return false;
        };
        if getter.verified || self.scopes[self.symbols[getter.target].scope].owner != Some(owner) {
            return false;
        }
        let Some(result) = self.source_property_getter(getter.target) else {
            return false;
        };
        let Some(payload) = self.plain_provider_constructor(scope, result, before) else {
            return false;
        };
        if self.symbols[payload].scope != scope
            || self.plain_provider_constructor(scope, result, END) != Some(payload)
        {
            return false;
        }
        let Some(Value::Callable(method)) =
            self.class_member_at(payload, method_name, 0, Some(access))
        else {
            return false;
        };
        let Binding::Definition(definition, _) = self.symbols[method.target].binding else {
            return false;
        };
        if method.verified
            || !field_dispatch::plain_method(definition, 1)
            || self.receiver_parameter(method.target).is_none()
        {
            return false;
        }
        self.walk(scope, receiver, before);
        self.source_property_invocations
            .push(SourcePropertyInvocation {
                caller: scope,
                at,
                getter: getter.bind_receiver(Receiver::Bound(owner)),
                method: method.bind_receiver(Receiver::Bound(payload)),
                classes: [owner, payload],
            });
        true
    }

    pub(super) fn resolve_source_property_invocations(&mut self) {
        if self.source_property_invocations.is_empty() {
            return;
        }
        let mut invalidated = self.field_callback_replacements();
        invalidated.extend(self.escaped_field_callbacks());
        invalidated.extend(&self.source_provider_opaque_arguments);
        for proof in std::mem::take(&mut self.source_property_invocations) {
            let access = (proof.caller, proof.at);
            if invalidated.contains(&proof.getter.target)
                || invalidated.contains(&proof.method.target)
                || self.receiver_conflicts(proof.method)
                || !proof.classes.into_iter().all(|class| {
                    self.plain_default_provider(class, access, &invalidated)
                        && self
                            .class_member_at(class, "__del__", 0, Some(access))
                            .is_none()
                })
            {
                self.scope_issue(
                    proof.caller,
                    proof.at,
                    "unresolved_descriptor",
                    "property dispatch requires stable getters and plain temporary receivers",
                );
                continue;
            }
            if !self.source_provider_returns_scalar(proof.method.target, &invalidated, 0) {
                self.scope_issue(
                    proof.caller,
                    proof.at,
                    "unresolved_finalizer",
                    "the property result's method may return a value with an unmodeled finalizer",
                );
            }
            for invocation in [proof.getter, proof.method] {
                self.direct.insert(invocation.target);
                self.invoke(proof.caller, invocation, "direct_call", "certain", proof.at);
            }
        }
    }
}
