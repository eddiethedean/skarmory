//! Resolve terminal writes to source-owned authentication backend bindings.

use super::*;

enum FailingWrite<'a> {
    BeforeReceiver,
    LocalCall(&'a Expression, SymbolId),
}

impl Graph<'_> {
    pub(in crate::phase02::analysis) fn terminal_django_backend_replacement(
        &mut self,
        scope: ScopeId,
        statement: &Statement,
    ) -> bool {
        let reads = std::mem::take(&mut self.read);
        let failure = self.prove_django_backend_replacement_failure(scope, statement);
        self.read = reads;
        match failure {
            Some(FailingWrite::BeforeReceiver) => true,
            Some(FailingWrite::LocalCall(function, target)) => {
                self.walk(scope, function, statement.at);
                self.edge(
                    scope,
                    target,
                    "django_settings_failed_initializer_call",
                    "certain",
                );
                true
            }
            None => false,
        }
    }

    fn prove_django_backend_replacement_failure<'a>(
        &mut self,
        scope: ScopeId,
        statement: &'a Statement,
    ) -> Option<FailingWrite<'a>> {
        let before = statement.at;
        let failure = match &statement.kind {
            StatementKind::WriteAttribute {
                value: Some(value), ..
            } if self.unbound_module_name(scope, value, before)
                && self.is_auth_backend_write(scope, statement, before) =>
            {
                FailingWrite::BeforeReceiver
            }
            StatementKind::If { test, body, .. }
                if self.unbound_module_name(scope, test, before)
                    && matches!(body.as_slice(), [write] if self.is_auth_backend_write(scope, write, before)) =>
            {
                FailingWrite::BeforeReceiver
            }
            StatementKind::Evaluate(Expression {
                kind:
                    ExpressionKind::Call {
                        function,
                        arguments,
                        keywords,
                    },
                ..
            }) if matches!(function.kind, ExpressionKind::Name(_))
                && arguments.is_empty()
                && keywords.is_empty() =>
            {
                let Value::Callable(Invocation {
                    target,
                    receiver: Receiver::Ordinary,
                    verified: false,
                    ..
                }) = self.probe_identity(scope, function, before)
                else {
                    return None;
                };
                let symbol = &self.symbols[target];
                let Binding::Definition(definition, _) = symbol.binding else {
                    return None;
                };
                if symbol.scope != scope
                    || !plain_backend_function(definition)
                    || !definition.parameters.is_empty()
                    || definition.is_generator
                {
                    return None;
                }
                let [write] = body_without_docstring(definition) else {
                    return None;
                };
                let StatementKind::WriteAttribute {
                    value: Some(value), ..
                } = &write.kind
                else {
                    return None;
                };
                if !self.unbound_module_name(scope, value, before)
                    || !self.is_auth_backend_write(scope, write, before)
                {
                    return None;
                }
                FailingWrite::LocalCall(function, target)
            }
            _ => return None,
        };
        let body = self.scopes[scope].body;
        let index = body.iter().position(|item| std::ptr::eq(item, statement))?;
        body[..index]
            .iter()
            .all(|item| {
                plain_backend_initializer(item)
                    || matches!(&item.kind, StatementKind::Import(import)
                if self.closed_backend_import(scope, import))
            })
            .then_some(failure)
    }

    fn is_auth_backend_write(
        &mut self,
        scope: ScopeId,
        write: &Statement,
        before: Location,
    ) -> bool {
        let StatementKind::WriteAttribute {
            receiver, member, ..
        } = &write.kind
        else {
            return false;
        };
        if !matches!(&receiver.kind, ExpressionKind::Name(_)) {
            return false;
        }
        let path = match self.probe_identity(scope, receiver, before) {
            Value::Callable(invocation)
                if AUTH_METHODS.contains(&member.as_str())
                    && self.is_class(invocation.target)
                    && self.symbols[invocation.target].scope == scope =>
            {
                self.full_name(invocation.target)
            }
            Value::Module(file)
                if self
                    .local(file, member, END)
                    .is_some_and(|id| self.is_class(id)) =>
            {
                format!("{}.{member}", self.files[file].module)
            }
            _ => return false,
        };
        self.scopes[..self.files.len()].iter().any(|row| {
            row.names
                .get("AUTHENTICATION_BACKENDS")
                .is_some_and(|bindings| {
                    bindings.iter().any(|&id| {
                        matches!(self.symbols[id].binding,
                        Binding::Variable(Some(Expression {
                            kind: ExpressionKind::Sequence { items, .. }, ..
                        }), _) if items.iter().any(|item|
                            matches!(&item.kind, ExpressionKind::Text(name) if name == &path)))
                    })
                })
        })
    }

    fn closed_backend_import(&self, scope: ScopeId, import: &Import) -> bool {
        let Some(module) = self.import_module(scope, import) else {
            return false;
        };
        let closed = |name: &str| {
            self.modules.get(name).is_some_and(|&file| {
                file != scope
                    && !self.scopes[file].open_wildcard
                    && self.scopes[file].body.iter().all(plain_backend_initializer)
            })
        };
        if !closed(module.as_str())
            || !module
                .match_indices('.')
                .all(|(end, _)| closed(&module[..end]))
        {
            return false;
        }
        import.member.as_ref().is_none_or(|member| {
            let child = format!("{module}.{member}");
            if self.modules.contains_key(child.as_str()) {
                closed(child.as_str())
            } else {
                self.modules
                    .get(module.as_str())
                    .is_some_and(|&file| self.local(file, member, END).is_some())
            }
        })
    }
}

fn plain_backend_initializer(statement: &Statement) -> bool {
    match &statement.kind {
        StatementKind::Assign { value: Some(value), annotation: None, names }
            if names.iter().all(|name| !name.text.starts_with("__")) =>
        {
            collections::literal(value).is_some()
        }
        StatementKind::Define(definition) if !definition.name.text.starts_with("__") => {
            plain_backend_function(definition)
                || definition.kind == DefinitionKind::Class
                    && definition.bases.is_empty()
                    && definition.decorators.is_empty()
                    && definition.table_keyword.is_none()
                    && body_without_docstring(definition).iter().all(|item|
                        matches!(&item.kind, StatementKind::Define(method) if plain_backend_function(method)))
        }
        StatementKind::Evaluate(Expression { kind: ExpressionKind::Text(_), .. }) => true,
        _ => false,
    }
}

fn plain_backend_function(definition: &Definition) -> bool {
    definition.kind == DefinitionKind::Function
        && definition.annotations.is_empty()
        && definition.decorators.is_empty()
        && definition.defaults.is_empty()
}
