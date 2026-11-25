//! Django URL construction and terminal registration failures.

use super::*;

pub(super) fn is_django_url_factory(value: &Value) -> bool {
    matches!(value, Value::Provider(provider) if matches!(provider.as_str(),
        "django.urls.path" | "django.urls.re_path" | "django.urls.include"))
}

impl Graph<'_> {
    pub(super) fn django_view_has_missing_option(
        &self,
        scope: ScopeId,
        before: Location,
        arguments: &[Expression],
        keywords: &[(String, Expression)],
    ) -> bool {
        scope == self.scopes[scope].file
            && arguments.is_empty()
            && matches!(keywords, [(_, value)] if self.unbound_module_name(scope, value, before))
    }

    pub(super) fn django_path<'expr>(
        &mut self,
        positional: &'expr [Expression],
        keywords: &'expr [(String, Expression)],
        evaluate: impl Fn(&mut Self, &'expr Expression) -> Value,
    ) -> Value {
        // Django exports a partial. An explicit Pattern can replace the
        // default pattern factory, whose behavior we do not model.
        if keywords.iter().any(|(name, _)| name == "Pattern") {
            return Value::DjangoPattern(Box::new(Value::Unknown));
        }
        let Some([Some(route), Some(view), kwargs, _]) =
            registration_arguments(positional, keywords, ["route", "view", "kwargs", "name"])
        else {
            return Value::DjangoInvalidRegistration;
        };
        if let Some(kwargs) = kwargs
            && !matches!(kwargs.kind, ExpressionKind::None)
        {
            match evaluate(self, kwargs) {
                Value::Text(_) | Value::Bool(_) | Value::Integer(_) | Value::Sequence(_) => {
                    return Value::DjangoInvalidRegistration;
                }
                Value::Scalar if matches!(kwargs.kind, ExpressionKind::Scalar(_)) => {
                    return Value::DjangoInvalidRegistration;
                }
                Value::Mapping(_) => {}
                _ => return Value::DjangoPattern(Box::new(Value::Unknown)),
            }
        }
        let view = evaluate(self, view);
        match view {
            Value::DjangoInvalidRegistration => Value::DjangoInvalidRegistration,
            _ if matches!(evaluate(self, route), Value::Text(_)) => {
                Value::DjangoPattern(Box::new(view))
            }
            _ => Value::DjangoPattern(Box::new(Value::Unknown)),
        }
    }

    pub(super) fn django_include<'expr>(
        &mut self,
        call: CallArguments<'expr>,
        evaluate: impl Fn(&mut Self, &'expr Expression) -> Value,
    ) -> Value {
        let Some([Some(module), namespace]) =
            registration_arguments(call.positional, call.keywords, ["arg", "namespace"])
        else {
            return Value::DjangoInvalidRegistration;
        };
        let Value::Text(module) = evaluate(self, module) else {
            return Value::Unknown;
        };
        let Some(&file) = self.modules.get(module.as_str()) else {
            return Value::Unknown;
        };
        if self.failed_terminal_modules.contains(&file) {
            return Value::Unknown;
        }
        if let Some(namespace) = namespace
            && !matches!(namespace.kind, ExpressionKind::None)
        {
            match evaluate(self, namespace) {
                Value::Bool(false) => {}
                Value::Text(name) if name.is_empty() => {}
                Value::Text(_) => {
                    if self.scopes[file].open_wildcard
                        || self.local(file, "__getattr__", END).is_some()
                        || self
                            .modules
                            .contains_key(format!("{module}.app_name").as_str())
                        || self
                            .module_member_write(&module, "app_name", (call.scope, call.before))
                            .is_some()
                    {
                        return Value::Unknown;
                    }
                    match self.local(file, "app_name", END) {
                        Some(binding) if !self.binding_depends_on_branch(binding, END) => {
                            match self.binding(binding, END, 0) {
                                Value::Text(name) if !name.is_empty() => {}
                                Value::Text(_) | Value::Bool(false)
                                    if self.simple_django_url_module(file) =>
                                {
                                    return Value::DjangoInvalidRegistration;
                                }
                                _ => return Value::Unknown,
                            }
                        }
                        None if self.simple_django_url_module(file) => {
                            return Value::DjangoInvalidRegistration;
                        }
                        _ => return Value::Unknown,
                    }
                }
                _ => return Value::Unknown,
            }
        }
        Value::DjangoInclude(framework_module(file))
    }

    fn simple_django_url_module(&mut self, file: usize) -> bool {
        // Restrict absence proofs to ordinary URL modules whose local
        // initializer does not install an application namespace dynamically.
        self.scopes[file].body.iter().all(|statement| {
            let (name, value) = match &statement.kind {
                StatementKind::Import(_) => return true,
                StatementKind::Assign { names, value: Some(value), annotation: None } => {
                    let [name] = names.as_slice() else { return false };
                    (name.text.as_str(), value)
                }
                _ => return false,
            };
            if name == "app_name" {
                return matches!(value.kind, ExpressionKind::Text(_) | ExpressionKind::Bool(_));
            }
            let ExpressionKind::Sequence { items, .. } = &value.kind else {
                return false;
            };
            name == "urlpatterns" && items.iter().all(|item| {
                let ExpressionKind::Call { function, arguments, keywords } = &item.kind else {
                    return false;
                };
                let [route, view] = arguments.as_slice() else { return false };
                keywords.is_empty()
                    && matches!(route.kind, ExpressionKind::Text(_))
                    && matches!(view.kind, ExpressionKind::Name(_))
                    && matches!(self.probe_identity(file, function, statement.at),
                        Value::Provider(name) if name == "django.urls.path" || name == "django.urls.re_path")
            })
        })
    }

    pub(super) fn django_effective_url_patterns(
        &mut self,
        file: ScopeId,
        binding: SymbolId,
    ) -> Value {
        let initial = self.binding(binding, END, 0);
        let Binding::Variable(Some(table), assigned_at) = self.symbols[binding].binding else {
            return initial;
        };
        let ExpressionKind::Sequence { list: true, items } = &table.kind else {
            return initial;
        };
        let body = self.scopes[file].body;
        let Some(index) = body
            .iter()
            .position(|statement| position(statement.at) == position(assigned_at))
        else {
            return initial;
        };
        let tail = &body[index + 1..];
        if !tail
            .iter()
            .any(|statement| django_url_edit(statement).is_some())
        {
            return initial;
        }
        if self.binding_depends_on_branch(binding, END)
            || self
                .scopes
                .iter()
                .any(|scope| scope.file != file && scope.imports.contains(&file))
            || items
                .iter()
                .any(|item| !self.simple_url_pattern(file, item, assigned_at))
        {
            return Value::Unknown;
        }
        let Value::Sequence(mut rows) = initial else {
            return Value::Unknown;
        };
        if !rows.iter().all(|item| matches!(item, Value::DjangoPattern(callback) if self.plain_url_callback(callback))) {
            return Value::Unknown;
        }

        let mut proved = BTreeSet::new();
        for statement in tail {
            let Some(edit) = django_url_edit(statement) else {
                return Value::Unknown;
            };
            match edit {
                DjangoUrlEdit::Clear(at) => {
                    rows.clear();
                    proved.insert((position(at), "unresolved_call"));
                }
                DjangoUrlEdit::Callback {
                    at,
                    index,
                    receiver,
                    value,
                } => {
                    let replacement = self.probe_identity(file, value, at);
                    if !self.plain_url_callback(&replacement) {
                        return Value::Unknown;
                    }
                    let Some(Value::DjangoPattern(callback)) = rows.get_mut(index) else {
                        return Value::Unknown;
                    };
                    **callback = replacement;
                    proved.insert((position(at), "unresolved_attribute_write"));
                    proved.insert((position(receiver), "unresolved_subscript"));
                }
            }
        }
        self.scope_issues.retain(|(scope, issue)| {
            *scope != file
                || !issue
                    .line
                    .zip(issue.column)
                    .is_some_and(|(line, column)| proved.contains(&([line, column], issue.kind)))
        });
        Value::Sequence(rows)
    }

    fn simple_url_pattern(&mut self, file: ScopeId, item: &Expression, before: Location) -> bool {
        let ExpressionKind::Call {
            function,
            arguments,
            keywords,
        } = &item.kind
        else {
            return false;
        };
        matches!(arguments.as_slice(), [Expression { kind: ExpressionKind::Text(route), .. }, _]
                if !route.contains('<'))
            && keywords.is_empty()
            && matches!(self.probe_identity(file, function, before),
                Value::Provider(provider) if provider == "django.urls.path")
    }

    fn plain_url_callback(&self, value: &Value) -> bool {
        match value {
            Value::Callable(call) => !call.verified && !self.is_class(call.target),
            Value::DjangoView(_) | Value::ConstantFunction | Value::NullaryConstantFunction => true,
            _ => false,
        }
    }

    pub(super) fn terminal_django_url_assignment(
        &mut self,
        file: usize,
        statement: &Statement,
    ) -> bool {
        let StatementKind::Assign {
            names,
            value:
                Some(Expression {
                    kind: ExpressionKind::Sequence { items, .. },
                    ..
                }),
            annotation: None,
        } = &statement.kind
        else {
            return false;
        };
        if !matches!(names.as_slice(), [name] if name.text == "urlpatterns") {
            return false;
        }
        let previous_reads = std::mem::take(&mut self.read);
        let failed = items.iter().position(|item| {
            matches!(
                item.kind,
                ExpressionKind::Call { .. } | ExpressionKind::RepeatedKeywordCall { .. }
            ) && matches!(
                self.probe_identity(file, item, statement.at),
                Value::DjangoInvalidRegistration
            )
        });
        self.read = previous_reads;
        let Some(index) = failed else {
            return false;
        };
        // Construction of the table stops at this element, so no pattern in
        // this module can be served. Preserve effects of evaluated arguments.
        for item in &items[..=index] {
            self.walk(file, item, statement.at);
        }
        let item = &items[index];
        let marker = match &item.kind {
            ExpressionKind::RepeatedKeywordCall { .. } => Some((item.at, "unsupported_construct")),
            ExpressionKind::Call {
                arguments,
                keywords,
                ..
            } => {
                let view = registration_arguments(
                    arguments,
                    keywords,
                    ["route", "view", "kwargs", "name"],
                )
                .and_then(|[_, view, _, _]| view);
                view.and_then(|view| {
                    let ExpressionKind::Call {
                        function,
                        arguments,
                        keywords,
                    } = &view.kind
                    else {
                        return None;
                    };
                    if !self.django_view_has_missing_option(file, statement.at, arguments, keywords)
                        || !matches!(
                            self.probe_identity(file, function, statement.at),
                            Value::DjangoViewFactory(_)
                        )
                    {
                        return None;
                    }
                    Some((view.at, "unresolved_call"))
                })
            }
            _ => None,
        };
        if let Some((at, kind)) = marker {
            let [line, column] = position(at);
            self.scope_issues.retain(|(scope, issue)| {
                *scope != file
                    || issue.kind != kind
                    || (issue.line, issue.column) != (Some(line), Some(column))
            });
        }
        true
    }
}

enum DjangoUrlEdit<'a> {
    Clear(Location),
    Callback {
        at: Location,
        index: usize,
        receiver: Location,
        value: &'a Expression,
    },
}

fn django_url_edit(statement: &Statement) -> Option<DjangoUrlEdit<'_>> {
    match &statement.kind {
        StatementKind::Evaluate(Expression {
            at,
            kind:
                ExpressionKind::Call {
                    function,
                    arguments,
                    keywords,
                },
        }) if arguments.is_empty()
            && keywords.is_empty()
            && matches!(&function.kind, ExpressionKind::Member(base, member)
                if member == "clear" && is_urlpatterns(base)) =>
        {
            Some(DjangoUrlEdit::Clear(*at))
        }
        StatementKind::WriteAttribute {
            receiver,
            member,
            value: Some(value),
        } if member == "callback"
            && let ExpressionKind::Subscript { value: base, index } = &receiver.kind
            && is_urlpatterns(base)
            && let ExpressionKind::Scalar(Some(index)) = index.kind =>
        {
            Some(DjangoUrlEdit::Callback {
                at: statement.at,
                index: usize::try_from(index).ok()?,
                receiver: receiver.at,
                value,
            })
        }
        _ => None,
    }
}

fn is_urlpatterns(expression: &Expression) -> bool {
    matches!(&expression.kind, ExpressionKind::Name(name) if name == "urlpatterns")
}
