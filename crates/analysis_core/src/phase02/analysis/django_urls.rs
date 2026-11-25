//! Django URL construction and terminal registration failures.

use super::*;

impl Graph<'_> {
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
            matches!(item.kind, ExpressionKind::Call { .. })
                && matches!(
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
        true
    }
}
