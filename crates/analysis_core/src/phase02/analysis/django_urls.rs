//! Django URL construction and terminal registration failures.

use super::*;

pub(super) fn is_django_url_factory(value: &Value) -> bool {
    matches!(value, Value::Provider(provider) if matches!(provider.as_str(),
        "django.urls.path" | "django.urls.re_path" | "django.urls.include"))
}

impl Graph<'_> {
    pub(super) fn django_converter_name(
        &self,
        file: ScopeId,
        expression: &Expression,
        before: Location,
        depth: usize,
    ) -> Option<String> {
        if depth >= MAX_BINDING_CHAIN {
            return None;
        }
        if let ExpressionKind::Name(name) = &expression.kind {
            let binding = self.local(file, name, before)?;
            if self.binding_depends_on_branch(binding, before) {
                return None;
            }
            let Binding::Variable(Some(value), at) = self.symbols[binding].binding else {
                return None;
            };
            return self.django_converter_name(file, value, at, depth + 1);
        }
        literal_converter_text(expression)
    }

    pub(super) fn standalone_django_converter_roots(&mut self, reach: &Reachability) -> Vec<Edge> {
        if !self.terminal_proofs || !self.django_startup_scopes.is_disjoint(&reach.scopes) {
            return Vec::new();
        }
        let files: Vec<_> = self.django_converter.declarations.keys().copied().collect();
        let mut roots = Vec::new();
        for file in files {
            if let Some(callbacks) = self.standalone_converter_callbacks(file) {
                self.django_converter.bounded_url_modules.insert(file);
                roots.extend(callbacks.into_iter().map(|target| Edge {
                    target,
                    kind: "django_registration",
                    certainty: "possible",
                }));
            }
        }
        roots
    }

    fn standalone_converter_callbacks(&mut self, file: ScopeId) -> Option<Vec<SymbolId>> {
        if self.failed_terminal_modules.contains(&file)
            || self.scopes[file].open_wildcard
            || self
                .scopes
                .iter()
                .any(|scope| scope.file != file && scope.imports.contains(&file))
        {
            return None;
        }
        let (table, prefix) = self.scopes[file].body.split_last()?;
        let StatementKind::Assign {
            names,
            value:
                Some(Expression {
                    kind: ExpressionKind::Sequence { items, .. },
                    ..
                }),
            annotation: None,
        } = &table.kind
        else {
            return None;
        };
        if !matches!(names.as_slice(), [name] if name.text == "urlpatterns") || items.is_empty() {
            return None;
        }

        let declarations = &self.django_converter.declarations[&file];
        let mut names = BTreeSet::new();
        for statement in prefix {
            match &statement.kind {
                StatementKind::Import(_) => {}
                StatementKind::Define(definition) if plain_code_function(definition) => {}
                StatementKind::Assign {
                    names,
                    value: Some(value),
                    annotation: None,
                } if matches!(names.as_slice(), [name] if !name.text.starts_with("__"))
                    && literal_converter_text(value).is_some() => {}
                StatementKind::Evaluate(expression)
                    if matches!(expression.kind, ExpressionKind::Text(_)) => {}
                StatementKind::Evaluate(expression) => {
                    let name = declarations.get(&position(expression.at))?;
                    if !names.insert(name.clone()) {
                        return None;
                    }
                }
                _ => return None,
            }
        }
        items
            .iter()
            .map(|item| {
                let ExpressionKind::Call {
                    function,
                    arguments,
                    keywords,
                } = &item.kind
                else {
                    return None;
                };
                let [
                    Expression {
                        kind: ExpressionKind::Text(route),
                        ..
                    },
                    view,
                ] = arguments.as_slice()
                else {
                    return None;
                };
                if !keywords.is_empty()
                    || !matches!(view.kind, ExpressionKind::Name(_))
                    || !matches!(self.probe_identity(file, function, table.at),
                    Value::Provider(provider) if provider == "django.urls.path")
                {
                    return None;
                }
                let captures = bounded_converter_captures(route, &names)?;
                let Value::Callable(invocation) = self.probe_identity(file, view, table.at) else {
                    return None;
                };
                let Binding::Definition(definition, _) = self.symbols[invocation.target].binding
                else {
                    return None;
                };
                if invocation.verified
                    || !matches!(invocation.receiver, Receiver::Ordinary)
                    || invocation.implicit_descriptor_receiver
                    || !plain_code_function(definition)
                    || definition.parameters.len() != captures.len() + 1
                    || !definition.parameters[1..]
                        .iter()
                        .all(|parameter| captures.contains(parameter.text.as_str()))
                {
                    return None;
                }
                Some(invocation.target)
            })
            .collect()
    }

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
        simple_path: bool,
        evaluate: impl Fn(&mut Self, &'expr Expression) -> Value,
    ) -> Value {
        // Django exports a partial. An explicit Pattern can replace the
        // default pattern factory, whose behavior we do not model.
        if keywords.iter().any(|(name, _)| name == "Pattern") {
            return Value::DjangoPattern(Box::new(Value::Unknown), None);
        }
        let Some([Some(route), Some(view), kwargs, _]) =
            registration_arguments(positional, keywords, ["route", "view", "kwargs", "name"])
        else {
            return Value::DjangoInvalidRegistration;
        };
        let defaults = if let Some(kwargs) = kwargs
            && !matches!(kwargs.kind, ExpressionKind::None)
        {
            match evaluate(self, kwargs) {
                Value::Text(_) | Value::Bool(_) | Value::Integer(_) | Value::Sequence(_) => {
                    return Value::DjangoInvalidRegistration;
                }
                Value::Scalar if matches!(kwargs.kind, ExpressionKind::Scalar(_)) => {
                    return Value::DjangoInvalidRegistration;
                }
                Value::Mapping(_) => literal_view_keywords(kwargs),
                _ => return Value::DjangoPattern(Box::new(Value::Unknown), None),
            }
        } else {
            Some(BTreeMap::new())
        };
        let view = evaluate(self, view);
        match view {
            Value::DjangoInvalidRegistration => Value::DjangoInvalidRegistration,
            _ => match evaluate(self, route) {
                Value::Text(path) => Value::DjangoPattern(
                    Box::new(view),
                    defaults.filter(|_| {
                        simple_path
                            && matches!(route.kind, ExpressionKind::Text(_))
                            && !path.contains(['<', '>'])
                    }),
                ),
                _ => Value::DjangoPattern(Box::new(Value::Unknown), None),
            },
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
        if namespace.is_none() && self.django_closed_view_option_failure(file) {
            // include() imports the module before returning its URLConf tuple.
            return Value::DjangoInvalidRegistration;
        }
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

    pub(super) fn record_failed_django_include(&mut self, call: CallArguments<'_>) {
        if call.scope == self.scopes[call.scope].file
            && self.branch_depth == 0
            && let Some([Some(module), None]) =
                registration_arguments(call.positional, call.keywords, ["arg", "namespace"])
            && let Value::Text(module) = self.identity(call.scope, module, call.before, 0)
            && let Some(&file) = self.modules.get(module.as_str())
            && self.django_closed_view_option_failure(file)
        {
            self.scopes[call.scope]
                .activated_apps
                .push(django_loaded_module(file));
        }
    }

    fn django_closed_view_option_failure(&mut self, file: ScopeId) -> bool {
        if !self.terminal_proofs
            || self.is_pytest_file(file)
            || self.files[file].path.rsplit('/').next() == Some("conftest.py")
        {
            return false;
        }
        let Some((assignment, prefix)) = self.scopes[file].body.split_last() else {
            return false;
        };
        let StatementKind::Assign {
            names,
            value: Some(value),
            annotation: None,
        } = &assignment.kind
        else {
            return false;
        };
        if !matches!(names.as_slice(), [name] if name.text == "urlpatterns")
            || !prefix
                .iter()
                .all(|row| matches!(row.kind, StatementKind::Import(_)))
        {
            return false;
        }
        let ExpressionKind::Sequence { items, .. } = &value.kind else {
            return false;
        };
        let [
            Expression {
                kind:
                    ExpressionKind::Call {
                        function,
                        arguments,
                        keywords,
                    },
                ..
            },
        ] = items.as_slice()
        else {
            return false;
        };
        matches!(arguments.as_slice(), [Expression { kind: ExpressionKind::Text(_), .. }, view]
            if keywords.is_empty()
                && matches!(&function.kind, ExpressionKind::Name(_))
                && matches!(self.probe_identity(file, function, assignment.at), Value::Provider(name)
                    if matches!(name.as_str(), "django.urls.path" | "django.urls.re_path"))
                && self.django_view_option_failure(file, view, assignment.at))
    }

    fn django_view_option_failure(
        &mut self,
        file: ScopeId,
        view: &Expression,
        before: Location,
    ) -> bool {
        matches!(&view.kind, ExpressionKind::Call { function, arguments, keywords }
            if self.django_view_has_missing_option(file, before, arguments, keywords)
                && matches!(self.probe_identity(file, function, before), Value::DjangoViewFactory(_)))
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
        if !rows.iter().all(|item| matches!(item, Value::DjangoPattern(callback, _) if self.plain_url_callback(callback))) {
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
                    let Some(Value::DjangoPattern(callback, _)) = rows.get_mut(index) else {
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

    pub(super) fn terminal_django_url_initializer(
        &mut self,
        file: ScopeId,
        statement: &Statement,
    ) -> bool {
        let Some((last, prefix)) = self.scopes[file].body.split_last() else {
            return false;
        };
        let StatementKind::Evaluate(Expression {
            kind: ExpressionKind::Call { function, .. },
            ..
        }) = &statement.kind
        else {
            return false;
        };
        if !std::ptr::eq(last, statement) || !self.unbound_module_name(file, function, statement.at)
        {
            return false;
        }
        let Some((table, imports)) = prefix.split_last() else {
            return false;
        };
        imports
            .iter()
            .all(|row| matches!(row.kind, StatementKind::Import(_)))
            && matches!(&table.kind, StatementKind::Assign { names, value: Some(value), annotation: None }
                if matches!(names.as_slice(), [name] if name.text == "urlpatterns")
                    && self.closed_named_url_patterns(file, value, table.at))
    }

    pub(super) fn terminal_django_url_callback_import(
        &mut self,
        file: ScopeId,
        statement: &Statement,
    ) -> bool {
        let Some((provider, member)) = self.django_url_callback_provider(file, statement) else {
            return false;
        };
        if !self.django_failed_view_initializers.contains_key(&provider)
            && !self.django_raising_view_module(provider, member)
        {
            return false;
        }
        // The import runs the provider's prefix, but never binds its callback
        // or reaches any later imports and registrations in the URLConf.
        self.scopes[file]
            .activated_apps
            .push(django_loaded_module(provider));
        true
    }

    pub(super) fn index_failed_django_view_initializers(&mut self) {
        if !self.terminal_proofs {
            return;
        }
        for file in 0..self.files.len() {
            if self.is_pytest_file(file)
                || self.files[file].path.rsplit('/').next() == Some("conftest.py")
            {
                continue;
            }
            let Some((table, imports)) = self.scopes[file].body.split_last() else {
                continue;
            };
            if !matches!(&table.kind, StatementKind::Assign { names, .. }
                if matches!(names.as_slice(), [name] if name.text == "urlpatterns"))
            {
                continue;
            }
            for statement in imports {
                let Some((provider, member)) = self.django_url_callback_provider(file, statement)
                else {
                    continue;
                };
                if !self.is_pytest_file(provider)
                    && self.files[provider].path.rsplit('/').next() != Some("conftest.py")
                    && let Some(at) = self.django_unbound_view_initializer(provider, member)
                {
                    self.django_failed_view_initializers.insert(provider, at);
                }
            }
        }
    }

    fn django_url_callback_provider<'statement>(
        &mut self,
        file: ScopeId,
        statement: &'statement Statement,
    ) -> Option<(ScopeId, &'statement str)> {
        let StatementKind::Import(import) = &statement.kind else {
            return None;
        };
        let member = import.member.as_deref()?;
        let module = self.import_module(file, import)?;
        let &provider = self.modules.get(module.as_str())?;
        if provider == file || module.contains('.') || self.files[provider].is_package {
            return None;
        }
        let (table, imports) = self.scopes[file].body.split_last()?;
        let StatementKind::Assign {
            names,
            value: Some(value),
            annotation: None,
        } = &table.kind
        else {
            return None;
        };
        let ExpressionKind::Sequence { items, .. } = &value.kind else {
            return None;
        };
        (matches!(names.as_slice(), [name] if name.text == "urlpatterns")
            && imports.iter().all(|row| matches!(row.kind, StatementKind::Import(_)))
            && items.iter().any(|item| matches!(&item.kind,
                ExpressionKind::Call { arguments, .. }
                    if matches!(arguments.as_slice(), [_, Expression { kind: ExpressionKind::Name(name), .. }]
                        if name == &import.local.text)))
            && self.closed_named_url_patterns(file, value, table.at))
        .then_some((provider, member))
    }

    fn django_unbound_view_initializer(&self, file: ScopeId, member: &str) -> Option<Location> {
        let (terminal, prefix) = self.scopes[file].body.split_last()?;
        match &terminal.kind {
            StatementKind::Evaluate(Expression {
                kind: ExpressionKind::Call { function, .. },
                ..
            }) if self.unbound_module_name(file, function, terminal.at)
                && prefix.iter().any(|statement| {
                    matches!(&statement.kind, StatementKind::Define(definition)
                        if definition.name.text == member && plain_code_function(definition))
                }) => {}
            StatementKind::Define(definition)
                if definition.kind == DefinitionKind::Function
                    && definition.name.text == member
                    && definition.defaults.is_empty()
                    && definition.annotations.is_empty()
                    && matches!(definition.decorators.as_slice(), [decorator]
                        if self.unbound_module_name(file, decorator, terminal.at)) => {}
            _ => return None,
        }
        // An earlier imported initializer could supply the name dynamically.
        // Follow only flat local imports with passive bodies.
        self.passive_django_view_imports(file, prefix, &mut 128, 0)
            .then_some(terminal.at)
    }

    fn passive_django_view_imports(
        &self,
        file: ScopeId,
        body: &[Statement],
        remaining: &mut usize,
        depth: usize,
    ) -> bool {
        let Some(budget) = remaining.checked_sub(body.len()) else {
            return false;
        };
        if depth >= MAX_BINDING_CHAIN {
            return false;
        }
        *remaining = budget;
        let mut classes = BTreeMap::<&str, bool>::new();
        body.iter().all(|statement| match &statement.kind {
            StatementKind::Import(import) if !import.local.text.starts_with("__") => {
                classes.remove(import.local.text.as_str());
                self.import_module(file, import).is_some_and(|module| {
                    !module.contains('.')
                        && self.modules.get(module.as_str()).is_some_and(|&provider| {
                            !self.files[provider].is_package
                                && self.passive_django_view_imports(
                                    provider, self.scopes[provider].body, remaining, depth + 1,
                                )
                        })
                })
            }
            StatementKind::Define(definition) if !definition.name.text.starts_with("__") => {
                if plain_code_function(definition) {
                    classes.remove(definition.name.text.as_str());
                    return true;
                }
                if definition.kind != DefinitionKind::Class
                    || definition.has_class_keywords()
                    || !definition.decorators.is_empty()
                    || !definition.bases.iter().all(|base| {
                        matches!(&base.kind, ExpressionKind::Name(name)
                            if classes.contains_key(name.as_str()))
                    })
                {
                    return false;
                }
                let methods = body_without_docstring(definition);
                if !methods.iter().all(|row| {
                    matches!(&row.kind, StatementKind::Define(method)
                        if plain_code_function(method)
                            && (!method.name.text.starts_with("__") || method.name.text == "__init__"))
                }) {
                    return false;
                }
                let default_constructor = definition.bases.is_empty() && methods.iter().all(|row| {
                    matches!(&row.kind, StatementKind::Define(method) if method.name.text != "__init__")
                });
                classes.insert(&definition.name.text, default_constructor);
                true
            }
            StatementKind::Evaluate(Expression { kind: ExpressionKind::Text(_), .. }) => true,
            StatementKind::Assign {
                names,
                value: Some(value),
                annotation: None,
            } if matches!(names.as_slice(), [name] if !name.text.starts_with("__")) => {
                let passive = matches!(&value.kind,
                    ExpressionKind::Call { function, arguments, keywords }
                        if arguments.is_empty() && keywords.is_empty()
                            && matches!(&function.kind, ExpressionKind::Name(name)
                                if classes.get(name.as_str()) == Some(&true)));
                classes.remove(names[0].text.as_str());
                passive
            }
            _ => false,
        })
    }

    fn django_raising_view_module(&self, file: ScopeId, member: &str) -> bool {
        let Some((terminal, prefix)) = self.scopes[file].body.split_last() else {
            return false;
        };
        if !matches!(&terminal.kind,
            StatementKind::Raise(Some(Expression {
                kind: ExpressionKind::Call { function, arguments, keywords }, ..
            })) if matches!(&function.kind, ExpressionKind::Name(name) if name == "RuntimeError")
                && matches!(arguments.as_slice(), [] | [Expression { kind: ExpressionKind::Text(_), .. }])
                && keywords.is_empty()
                && matches!(self.lexical_binding(file, "RuntimeError", terminal.at, true), LexicalBinding::Missing))
        {
            return false;
        }
        prefix.iter().all(|statement| match &statement.kind {
            StatementKind::Import(_) => true,
            StatementKind::Define(definition) => plain_code_function(definition),
            _ => false,
        }) && prefix.iter().any(|statement| {
            matches!(&statement.kind,
            StatementKind::Define(definition) if definition.name.text == member)
        })
    }

    pub(super) fn django_closed_url_table_missing(&mut self, file: ScopeId) -> bool {
        if !self.terminal_proofs
            || self.scopes[file].open_wildcard
            || self.local(file, "__getattr__", END).is_some()
            || self
                .scopes
                .iter()
                .any(|scope| scope.file != file && scope.imports.contains(&file))
        {
            return false;
        }
        let Some((assignment, imports)) = self.scopes[file].body.split_last() else {
            return false;
        };
        let StatementKind::Assign {
            names,
            value: Some(value),
            annotation: None,
        } = &assignment.kind
        else {
            return false;
        };
        if !matches!(names.as_slice(), [name] if !name.text.starts_with("__"))
            || !imports
                .iter()
                .all(|row| matches!(row.kind, StatementKind::Import(_)))
        {
            return false;
        }
        // The URL resolver falls back to the module object when no table is
        // exported. This initializer cannot turn it into an iterable or
        // publish the local patterns under another name.
        let ExpressionKind::Sequence { items, .. } = &value.kind else {
            return false;
        };
        !items.is_empty()
            && items.iter().all(|item| {
                let ExpressionKind::Call { arguments, .. } = &item.kind else {
                    return false;
                };
                self.simple_url_pattern(file, item, assignment.at) && {
                    let callback = self.probe_identity(file, &arguments[1], assignment.at);
                    self.plain_url_callback(&callback)
                }
            })
    }

    fn closed_named_url_patterns(
        &mut self,
        file: ScopeId,
        value: &Expression,
        at: Location,
    ) -> bool {
        matches!(&value.kind, ExpressionKind::Sequence { items, .. }
        if items.iter().all(|item| {
            matches!(&item.kind, ExpressionKind::Call { arguments, .. }
                if matches!(arguments.as_slice(), [_, Expression {kind: ExpressionKind::Name(_), .. }]))
                && self.simple_url_pattern(file, item, at)
        }))
    }

    pub(super) fn django_constant_callback_code(
        &mut self,
        file: ScopeId,
        target: SymbolId,
    ) -> bool {
        let source = self.symbols[target].scope;
        if !self.terminal_proofs
            || source == file
            || source != self.scopes[source].file
            || self.scopes[source].open_wildcard
            || self.scopes[file].open_wildcard
        {
            return false;
        }
        let Binding::Definition(original, _) = self.symbols[target].binding else {
            return false;
        };
        let Some((write, prefix)) = self.scopes[source].body.split_last() else {
            return false;
        };
        let StatementKind::WriteAttribute {
            receiver,
            member,
            value: Some(value),
        } = &write.kind
        else {
            return false;
        };
        let ExpressionKind::Member(replacement, attribute) = &value.kind else {
            return false;
        };
        if member != "__code__"
            || attribute != "__code__"
            || !matches!(receiver.kind, ExpressionKind::Name(_))
            || !matches!(replacement.kind, ExpressionKind::Name(_))
            || !plain_code_function(original)
            || !matches!(self.probe_identity(source, receiver, write.at),
                Value::Callable(callback) if !callback.verified && callback.target == target)
            || !prefix
                .iter()
                .skip_while(|statement| matches!(statement.kind, StatementKind::Import(_)))
                .all(|statement| match &statement.kind {
                    StatementKind::Define(definition) => plain_code_function(definition),
                    StatementKind::Assign {
                        names,
                        value: Some(value),
                        annotation: None,
                    } => {
                        names.len() == 1
                            && matches!(value.kind, ExpressionKind::Name(_))
                            && matches!(self.probe_identity(source, value, statement.at),
                            Value::Callable(call) if self.symbols[call.target].scope == source)
                    }
                    _ => false,
                })
        {
            return false;
        }
        let Value::Callable(replacement) = self.probe_identity(source, replacement, write.at)
        else {
            return false;
        };
        let Binding::Definition(definition, _) = self.symbols[replacement.target].binding else {
            return false;
        };
        if replacement.verified
            || self.symbols[replacement.target].scope != source
            || !plain_code_function(definition)
            || definition.parameters.len() != original.parameters.len()
            || !matches!(
                body_without_docstring(definition),
                [Statement {
                    kind: StatementKind::Return(Some(Expression {
                        kind: ExpressionKind::None,
                        ..
                    })),
                    ..
                }]
            )
        {
            return false;
        }
        // A direct call or an escape outside this URL initializer can
        // observe the overwritten code, which needs general call modeling.
        if self.scopes.iter().any(|scope| {
            scope.file != file && scope.file != source && scope.imports.contains(&source)
        }) || !self.scopes[file]
            .body
            .iter()
            .all(|statement| match &statement.kind {
                StatementKind::Import(_) => true,
                StatementKind::Assign {
                    names,
                    value: Some(value),
                    annotation: None,
                } if matches!(names.as_slice(), [name] if name.text == "urlpatterns") => {
                    self.closed_named_url_patterns(file, value, statement.at)
                }
                _ => false,
            })
        {
            return false;
        }
        let [line, column] = position(write.at);
        self.scope_issues.retain(|(scope, issue)| {
            *scope != source
                || issue.kind != "unresolved_attribute_write"
                || (issue.line, issue.column) != (Some(line), Some(column))
        });
        true
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
                view.filter(|view| self.django_view_option_failure(file, view, statement.at))
                    .map(|view| (view.at, "unresolved_call"))
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

fn literal_view_keywords(expression: &Expression) -> Option<django_views::ViewArguments> {
    let ExpressionKind::Dictionary(items) = &expression.kind else {
        return None;
    };
    let (items, remainder) = items.as_chunks::<2>();
    if !remainder.is_empty() {
        return None;
    }
    items
        .iter()
        .map(|[key, value]| {
            let (ExpressionKind::Text(key), ExpressionKind::Text(value)) = (&key.kind, &value.kind)
            else {
                return None;
            };
            Some((key.clone(), value.clone()))
        })
        .collect()
}

fn plain_code_function(definition: &Definition) -> bool {
    definition.kind == DefinitionKind::Function
        && !definition.is_generator
        && definition.decorators.is_empty()
        && definition.defaults.is_empty()
        && definition.annotations.is_empty()
        && definition.simple_positional_parameters
}

fn bounded_converter_captures<'a>(
    mut route: &'a str,
    names: &BTreeSet<String>,
) -> Option<BTreeSet<&'a str>> {
    let mut captures = BTreeSet::new();
    while let Some((prefix, angle)) = route.split_once('<') {
        let (capture, rest) = angle.split_once('>')?;
        let (converter, parameter) = capture.split_once(':')?;
        if prefix.contains('>')
            || !names.contains(converter)
            || !skarmory_python::simple_name(parameter)
            || !captures.insert(parameter)
        {
            return None;
        }
        route = rest;
    }
    (!captures.is_empty() && !route.contains('>')).then_some(captures)
}

fn literal_converter_text(expression: &Expression) -> Option<String> {
    match &expression.kind {
        ExpressionKind::Text(text) => Some(text.clone()),
        ExpressionKind::Addition(values) => {
            let [left, right] = values.as_slice() else {
                return None;
            };
            let mut text = literal_converter_text(left)?;
            text.push_str(&literal_converter_text(right)?);
            Some(text)
        }
        _ => None,
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
