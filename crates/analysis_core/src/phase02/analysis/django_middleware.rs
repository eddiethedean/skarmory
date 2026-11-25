//! A source middleware instance forwarding Django's adapter to its request.

use super::*;

pub(super) struct MiddlewareSite {
    class: SymbolId,
    pub(super) initializer: SymbolId,
    pub(super) handler: SymbolId,
    write: (ScopeId, Location),
    forward: (ScopeId, Location),
    metadata: Option<MetadataSite>,
}

impl MiddlewareSite {
    pub(super) fn metadata_helper(&self) -> Option<SymbolId> {
        self.metadata.as_ref().map(|metadata| metadata.helper)
    }
}

struct MetadataSite {
    helper: SymbolId,
    called_at: Location,
    route_calls: (ScopeId, [Location; 3]),
    metadata_lookup: Location,
    writes: [Location; 2],
}

impl<'a> Graph<'a> {
    pub(super) fn prove_django_middleware(
        &mut self,
        class: SymbolId,
        settings: ScopeId,
        singleton: bool,
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
        let (value, metadata) = match body_without_docstring(call) {
            [
                Statement {
                    kind: StatementKind::Return(Some(value)),
                    ..
                },
            ] => (value, None),
            [first, helper, conditional, last]
                if singleton && self.closed_middleware_downstream(settings) =>
            {
                let (result, continuation) = assigned_expression(first)?;
                if !matches!(&last.kind, StatementKind::Return(Some(value))
                    if local_name(value, result))
                {
                    return None;
                }
                let metadata = self.middleware_request_metadata(
                    call_scope,
                    &request.text,
                    helper,
                    conditional,
                )?;
                (continuation, Some(metadata))
            }
            _ => return None,
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
            metadata,
        })
    }

    fn middleware_method(&self, target: SymbolId) -> Option<(&'a Definition, ScopeId)> {
        let Binding::Definition(definition, body) = self.symbols[target].binding else {
            return None;
        };
        (definition.kind == DefinitionKind::Function
            && definition.simple_positional_parameters
            && !definition.is_generator)
            .then_some((definition, body))
    }

    fn closed_middleware_downstream(&self, settings: ScopeId) -> bool {
        let setting_value = |name| {
            let binding = self.local(settings, name, END)?;
            if self.binding_depends_on_branch(binding, END) {
                return None;
            }
            match self.symbols[binding].binding {
                Binding::Variable(Some(value), _) => Some(&value.kind),
                _ => None,
            }
        };
        if ["TEMPLATES", "LOGGING", "LOGGING_CONFIG", "DEBUG"]
            .iter()
            .any(|name| self.local(settings, name, END).is_some())
        {
            return false;
        }
        let Some(ExpressionKind::Sequence { items: apps, .. }) = setting_value("INSTALLED_APPS")
        else {
            return false;
        };
        if !apps.iter().all(|app| {
            let ExpressionKind::Text(app) = &app.kind else {
                return false;
            };
            self.modules.get(app.as_str()).is_some_and(|&owner| {
                self.files[owner].is_package
                    && self.files[owner].facts.empty
                    && !self.modules.contains_key(format!("{app}.apps").as_str())
                    && !self.modules.contains_key(format!("{app}.models").as_str())
            })
        }) {
            return false;
        }
        let Some(ExpressionKind::Text(urlconf)) = setting_value("ROOT_URLCONF") else {
            return false;
        };
        let Some(&urls) = self.modules.get(urlconf.as_str()) else {
            return false;
        };
        let body = self.scopes[urls].body;
        let body = if body.first().is_some_and(|first| {
            matches!(&first.kind, StatementKind::Evaluate(value)
                if matches!(value.kind, ExpressionKind::Text(_)))
        }) {
            &body[1..]
        } else {
            body
        };
        matches!(body, [statement]
            if assigned_expression(statement).is_some_and(|(name, value)|
                name == "urlpatterns"
                    && matches!(&value.kind, ExpressionKind::Sequence { items, .. } if items.is_empty())))
            && !self.failed_terminal_modules.contains(&urls)
            && self
                .module_member_write(urlconf, "urlpatterns", (settings, END))
                .is_none()
            && ["handler400", "handler403", "handler404", "handler500"]
                .iter()
                .all(|name| {
                    self.module_member_write(urlconf, name, (settings, END))
                        .is_none()
                })
    }

    fn middleware_request_metadata(
        &mut self,
        scope: ScopeId,
        request: &str,
        helper: &Statement,
        conditional: &Statement,
    ) -> Option<MetadataSite> {
        let (route, call) = assigned_expression(helper)?;
        let ExpressionKind::Call {
            function,
            arguments,
            keywords,
        } = &call.kind
        else {
            return None;
        };
        if !keywords.is_empty()
            || !matches!(arguments.as_slice(), [argument] if local_name(argument, request))
        {
            return None;
        }
        let Value::Callable(target) = self.probe_identity(scope, function, helper.at) else {
            return None;
        };
        if target.verified
            || !matches!(target.receiver, Receiver::Ordinary)
            || self.symbols[target.target].scope != self.scopes[scope].file
            || self.binding_depends_on_branch(target.target, helper.at)
        {
            return None;
        }
        let StatementKind::If {
            test,
            body,
            otherwise,
        } = &conditional.kind
        else {
            return None;
        };
        if !otherwise.is_empty() || !local_name(test, route) {
            return None;
        }
        let [first, second] = body.as_slice() else {
            return None;
        };
        let StatementKind::WriteAttribute {
            receiver,
            member,
            value: Some(meta),
        } = &first.kind
        else {
            return None;
        };
        let StatementKind::WriteModuleEntry {
            mapping,
            module: key,
            value: Some(value),
        } = &second.kind
        else {
            return None;
        };
        if member != "META"
            || !local_name(receiver, request)
            || !matches!(&mapping.kind, ExpressionKind::Member(owner, member)
                if member == "META" && local_name(owner, request))
            || !key.split_once('.').is_some_and(|(namespace, field)| {
                !namespace.is_empty() && !field.is_empty()
                    && !matches!(namespace, "wsgi" | "wsgiorg" | "asgi")
                    && namespace.bytes().all(|byte| byte.is_ascii_lowercase())
            })
            || !local_name(value, route)
            || !self.request_attribute_with_default(scope, first.at, meta, request, "META",
                |value| matches!(&value.kind, ExpressionKind::Dictionary(items) if items.is_empty()))
        {
            return None;
        }
        Some(MetadataSite {
            helper: target.target,
            called_at: call.at,
            route_calls: self.middleware_route_string(target.target)?,
            metadata_lookup: meta.at,
            writes: [first.at, second.at],
        })
    }

    fn middleware_route_string(&mut self, target: SymbolId) -> Option<(ScopeId, [Location; 3])> {
        let (helper, scope) = self.middleware_method(target)?;
        let [request] = helper.parameters.as_slice() else {
            return None;
        };
        if !helper.decorators.is_empty() || !helper.defaults.is_empty() {
            return None;
        }
        let [first, middle, last] = body_without_docstring(helper) else {
            return None;
        };
        let (path, paths) = assigned_expression(first)?;
        let [preferred, fallback] = disjunction_pair(paths)?;
        if !self.request_attribute_with_default(
            scope,
            first.at,
            preferred,
            &request.text,
            "path_info",
            |value| matches!(value.kind, ExpressionKind::None),
        ) || !self.request_attribute_with_default(
            scope,
            first.at,
            fallback,
            &request.text,
            "path",
            |value| matches!(value.kind, ExpressionKind::Text(_)),
        ) {
            return None;
        }
        if !matches!(&middle.kind, StatementKind::If { test, body, otherwise }
            if otherwise.is_empty()
                && matches!(&test.kind, ExpressionKind::Not(value) if local_name(value, path))
                && matches!(body.as_slice(), [Statement {kind: StatementKind::Return(value), ..}]
                    if value.as_ref().is_none_or(|value| matches!(value.kind, ExpressionKind::None))))
        {
            return None;
        }
        let StatementKind::Return(Some(result)) = &last.kind else {
            return None;
        };
        let [strip, default_route] = disjunction_pair(result)?;
        let ExpressionKind::Call {
            function,
            arguments,
            keywords,
        } = &strip.kind
        else {
            return None;
        };
        (matches!(default_route.kind, ExpressionKind::Text(_))
            && keywords.is_empty()
            && matches!(arguments.as_slice(), [argument] if matches!(argument.kind, ExpressionKind::Text(_)))
            && matches!(&function.kind, ExpressionKind::Member(owner, name)
                if name == "rstrip" && local_name(owner, path)))
            .then_some((scope, [preferred.at, fallback.at, strip.at]))
    }

    fn request_attribute_with_default(
        &mut self,
        scope: ScopeId,
        before: Location,
        expression: &Expression,
        request: &str,
        expected: &str,
        default: impl FnOnce(&Expression) -> bool,
    ) -> bool {
        matches!(&expression.kind, ExpressionKind::Call { function, arguments, keywords }
            if keywords.is_empty()
                && matches!(arguments.as_slice(), [owner, name, fallback]
                    if local_name(owner, request)
                        && matches!(&name.kind, ExpressionKind::Text(name) if name == expected)
                        && default(fallback))
                && (matches!(&function.kind, ExpressionKind::Name(name) if name == "getattr"
                    && matches!(self.lexical_binding(scope, name, before, true), LexicalBinding::Missing))
                    || matches!(self.probe_identity(scope, function, before),
                        Value::Provider(provider) if provider == "builtins.getattr")))
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
                if let Some(metadata) = &site.metadata
                    && reach
                        .evidence
                        .range((metadata.helper, "", "")..)
                        .take_while(|(target, _, _)| *target == metadata.helper)
                        .all(|(_, kind, _)| matches!(*kind, "direct_call" | "type_reference"))
                    && self
                        .direct_arguments
                        .get(&metadata.helper)
                        .is_some_and(|calls| {
                            calls
                                .iter()
                                .filter(|call| reach.scopes.contains(&call.caller))
                                .all(|call| {
                                    call.caller == site.forward.0
                                        && position(call.at) == position(metadata.called_at)
                                })
                        })
                {
                    let (route_scope, route_calls) = metadata.route_calls;
                    for at in route_calls {
                        resolved.insert((route_scope, position(at), "unresolved_call"));
                    }
                    resolved.insert((
                        site.forward.0,
                        position(metadata.metadata_lookup),
                        "unresolved_call",
                    ));
                    for at in metadata.writes {
                        resolved.insert((
                            site.forward.0,
                            position(at),
                            "unresolved_attribute_write",
                        ));
                    }
                }
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

fn assigned_expression(statement: &Statement) -> Option<(&str, &Expression)> {
    let StatementKind::Assign {
        names,
        value: Some(value),
        annotation: None,
    } = &statement.kind
    else {
        return None;
    };
    let [name] = names.as_slice() else {
        return None;
    };
    Some((&name.text, value))
}

fn disjunction_pair(value: &Expression) -> Option<&[Expression; 2]> {
    let ExpressionKind::Boolean { and: false, values } = &value.kind else {
        return None;
    };
    values.as_slice().try_into().ok()
}
