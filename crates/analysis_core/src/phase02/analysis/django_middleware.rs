//! A source middleware instance forwarding Django's adapter to its request.

use super::*;

pub(super) struct MiddlewareSite {
    class: SymbolId,
    pub(super) initializer: SymbolId,
    pub(super) handler: SymbolId,
    write: (ScopeId, Location),
    forward: (ScopeId, Location),
}

impl Graph<'_> {
    pub(super) fn prove_django_middleware(
        &self,
        class: SymbolId,
        settings: ScopeId,
    ) -> Option<MiddlewareSite> {
        let Binding::Definition(definition, body) = self.symbols[class].binding else {
            return None;
        };
        if !definition.bases.is_empty()
            || definition.table_keyword.is_some()
            || !definition.decorators.is_empty()
            || !self.class_discovery_writes(class, settings).is_empty()
            || body_without_docstring(definition).iter().any(|statement| {
                !matches!(&statement.kind, StatementKind::Define(method)
                    if (!method.name.text.starts_with("__")
                        || matches!(method.name.text.as_str(), "__init__" | "__call__"))
                        && method.decorators.is_empty() && method.annotations.is_empty()
                        && method.defaults.is_empty())
            })
        {
            return None;
        }
        let initializer = self.local(body, "__init__", END)?;
        let handler = self.local(body, "__call__", END)?;
        let (init, init_scope) = self.middleware_method(initializer)?;
        let (call, call_scope) = self.middleware_method(handler)?;
        let ([instance, response], [receiver, request]) =
            (init.parameters.as_slice(), call.parameters.as_slice())
        else {
            return None;
        };
        let [
            Statement {
                kind:
                    StatementKind::WriteAttribute {
                        receiver: target,
                        member,
                        value: Some(value),
                    },
                at: write,
            },
        ] = body_without_docstring(init)
        else {
            return None;
        };
        if !local_name(target, &instance.text)
            || !local_name(value, &response.text)
            || member.starts_with("__")
            || self.local(body, member, END).is_some()
        {
            return None;
        }
        let [
            Statement {
                kind: StatementKind::Return(Some(value)),
                ..
            },
        ] = body_without_docstring(call)
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
        let ([argument], ExpressionKind::Member(owner, field)) =
            (arguments.as_slice(), &function.kind)
        else {
            return None;
        };
        if !keywords.is_empty()
            || !local_name(argument, &request.text)
            || !local_name(owner, &receiver.text)
            || field != member
        {
            return None;
        }
        Some(MiddlewareSite {
            class,
            initializer,
            handler,
            write: (init_scope, *write),
            forward: (call_scope, value.at),
        })
    }

    fn middleware_method(&self, target: SymbolId) -> Option<(&Definition, ScopeId)> {
        let Binding::Definition(definition, body) = self.symbols[target].binding else {
            return None;
        };
        (definition.kind == DefinitionKind::Function
            && definition.simple_positional_parameters
            && !definition.is_generator)
            .then_some((definition, body))
    }

    pub(super) fn report_django_middleware_calls(&mut self, reach: &Reachability) {
        let mut resolved = BTreeSet::new();
        for site in &self.django_middleware_sites {
            if !reach.has_evidence(site.handler, "django_middleware") {
                continue;
            }
            resolved.insert((
                site.write.0,
                position(site.write.1),
                "unresolved_attribute_write",
            ));
            let scoped = |callback| {
                reach
                    .evidence
                    .range((callback, "", "")..)
                    .take_while(|(target, _, _)| *target == callback)
                    .all(|(_, kind, _)| matches!(*kind, "django_middleware" | "type_reference"))
            };
            let class_closed = reach
                .evidence
                .range((site.class, "", "")..)
                .take_while(|(target, _, _)| *target == site.class)
                .all(|(_, kind, _)| {
                    matches!(
                        *kind,
                        "django_setting_reference" | "class_reference" | "type_reference"
                    )
                });
            if class_closed && scoped(site.initializer) && scoped(site.handler) {
                resolved.insert((site.forward.0, position(site.forward.1), "unresolved_call"));
            }
        }
        self.scope_issues.retain(|(scope, issue)| {
            !issue.line.zip(issue.column).is_some_and(|(line, column)| {
                resolved.contains(&(*scope, [line, column], issue.kind))
            })
        });
    }
}

fn local_name(expression: &Expression, expected: &str) -> bool {
    matches!(&expression.kind, ExpressionKind::Name(name) if name == expected)
}
