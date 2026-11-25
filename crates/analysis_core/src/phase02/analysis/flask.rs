//! Flask factory discovery and teardown registration through local setup helpers.

use super::*;

pub(super) fn provider_kind(name: &str) -> Option<ObjectKind> {
    match name {
        "flask.Flask" | "flask.app.Flask" => Some(ObjectKind::FlaskApp),
        "flask.Blueprint" | "flask.blueprints.Blueprint" => Some(ObjectKind::FlaskBlueprint),
        _ if is_flask_login_constructor(name) => Some(ObjectKind::FlaskLoginManager),
        _ => None,
    }
}

pub(super) struct ParameterHook {
    owner: SymbolId,
    scope: ScopeId,
    parameter: SymbolId,
    target: SymbolId,
    at: Location,
}

impl Graph<'_> {
    pub(super) fn flask_instance(
        &self,
        scope: ScopeId,
        at: Location,
        before: Location,
        kind: ObjectKind,
    ) -> Value {
        if self.flask_constructor_changed(kind, (scope, before)) {
            return Value::Unknown;
        }
        Value::Object((self.scopes[scope].file, at.line, at.column), kind)
    }

    pub(super) fn flask_factory_result(
        &mut self,
        invocation: Invocation,
        caller: ScopeId,
        at: Location,
        depth: usize,
    ) -> Option<Value> {
        let symbol = &self.symbols[invocation.target];
        let Binding::Definition(definition, scope) = symbol.binding else {
            return None;
        };
        if depth >= MAX_BINDING_CHAIN
            || invocation.verified
            || !matches!(invocation.receiver, Receiver::Ordinary)
            || self.scopes[symbol.scope].owner.is_some()
            || self.scopes[caller].owner.is_some()
            || definition.kind != DefinitionKind::Function
            || definition.is_generator
            || !definition.decorators.is_empty()
            || !definition.simple_positional_parameters
            || definition.parameters.len() != definition.defaults.len()
        {
            return None;
        }
        let (returned, setup) = body_without_docstring(definition).split_last()?;
        let StatementKind::Return(Some(value)) = &returned.kind else {
            return None;
        };
        let configuration =
            matches!(&value.kind, ExpressionKind::Member(_, name) if name == "config");
        let returned_object = match &value.kind {
            ExpressionKind::Member(base, name) if name == "config" => base.as_ref(),
            _ => value,
        };
        // Follow local aliases only through a single allocation and plain
        // assignments that cannot mutate or expose the returned application.
        let (constructor, before) = if setup.is_empty() {
            (returned_object, returned.at)
        } else {
            let ExpressionKind::Name(returned) = &returned_object.kind else {
                return None;
            };
            let mut aliases = BTreeSet::new();
            let mut constructor = None;
            for statement in setup {
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
                match &value.kind {
                    ExpressionKind::Name(alias) if aliases.contains(alias.as_str()) => {}
                    ExpressionKind::Call { .. } if constructor.is_none() => {
                        constructor = Some((value, statement.at));
                    }
                    _ => return None,
                }
                aliases.insert(name.text.as_str());
            }
            if !aliases.contains(returned.as_str()) {
                return None;
            }
            constructor?
        };
        let ExpressionKind::Call {
            function,
            arguments,
            keywords,
        } = &constructor.kind
        else {
            return None;
        };
        let [name] = arguments.as_slice() else {
            return None;
        };
        if !keywords.is_empty()
            || !self.flask_literal_import_name(scope, name, before)
            || !matches!(
                function.kind,
                ExpressionKind::Name(_) | ExpressionKind::Member(_, _)
            )
            || !matches!(self.identity(scope, function, before, depth + 1),
                Value::Provider(name) if provider_kind(&name) == Some(ObjectKind::FlaskApp))
            || self.flask_constructor_changed(ObjectKind::FlaskApp, (scope, before))
        {
            return None;
        }
        // The caller can create multiple independent instances from the same
        // constructor. Associate member writes with the specific outer call.
        if configuration {
            return matches!(
                self.identity(scope, value, returned.at, depth + 1),
                Value::FlaskConfig
            )
            .then_some(Value::FlaskConfig);
        }
        Some(Value::Object(
            (self.scopes[caller].file, at.line, at.column),
            ObjectKind::FlaskApp,
        ))
    }

    fn flask_literal_import_name(
        &self,
        scope: ScopeId,
        expression: &Expression,
        before: Location,
    ) -> bool {
        match &expression.kind {
            ExpressionKind::Text(_) => true,
            ExpressionKind::Name(name) if name == "__name__" => {
                matches!(
                    self.lexical_binding(scope, name, before, true),
                    LexicalBinding::Missing
                ) && !self.scopes[self.scopes[scope].file].open_wildcard
            }
            _ => false,
        }
    }

    pub(super) fn terminal_flask_decorator(
        &mut self,
        scope: ScopeId,
        definition: &Definition,
        at: Location,
        prefix: &[Statement],
    ) -> bool {
        let [decorator] = definition.decorators.as_slice() else {
            return false;
        };
        let ExpressionKind::Member(base, method) = &decorator.kind else {
            return false;
        };
        if definition.kind == DefinitionKind::Class
            || !definition.annotations.is_empty()
            || !definition.defaults.is_empty()
            || !(flask_callback_method(ObjectKind::FlaskApp, method)
                || flask_login_callback(method))
        {
            return false;
        }
        let read = std::mem::take(&mut self.read);
        let wrong_receiver = match self.probe_identity(scope, base, at) {
            Value::FlaskConfig | Value::Coroutine(_) => true,
            _ => {
                if let ExpressionKind::Member(receiver, attribute) = &base.kind
                    && attribute == "config"
                    && let Value::Object(site, ObjectKind::FlaskLoginManager) =
                        self.probe_identity(scope, receiver, at)
                {
                    matches!(
                        self.flask_member(
                            site,
                            ObjectKind::FlaskLoginManager,
                            attribute,
                            (scope, at)
                        ),
                        Value::Method(..)
                    )
                } else {
                    false
                }
            }
        };
        self.read = read;
        if !wrong_receiver || !self.closed_flask_initializers(scope, prefix) {
            return false;
        }
        self.walk(scope, base, at);
        true
    }

    pub(super) fn terminal_unbound_flask_initializer(
        &mut self,
        scope: ScopeId,
        statement: &Statement,
    ) -> bool {
        let StatementKind::Assign {
            names,
            value: Some(value),
            annotation: None,
        } = &statement.kind
        else {
            return false;
        };
        if names.len() != 1 {
            return false;
        }
        let at = statement.at;
        let callable = match &value.kind {
            ExpressionKind::Name(_) if self.unbound_module_name(scope, value, at) => None,
            ExpressionKind::Call {
                function,
                arguments,
                keywords,
            } if keywords.is_empty()
                && matches!(&function.kind, ExpressionKind::Name(_))
                && matches!(arguments.as_slice(), [argument]
                        if self.unbound_module_name(scope, argument, at)) =>
            {
                Some(function.as_ref())
            }
            _ => return false,
        };
        let body = self.scopes[scope].body;
        let Some(index) = body.iter().position(|item| std::ptr::eq(item, statement)) else {
            return false;
        };
        let prefix = &body[..index];
        if !prefix.iter().any(|item| {
            matches!(&item.kind, StatementKind::Import(import) if import.level == 0
                && matches!(import.module.as_str(), "flask" | "flask_login")
                && !self.modules.contains_key(import.module.as_str()))
        }) || !self.closed_flask_initializers(scope, prefix)
        {
            return false;
        }
        // Reading the callable name precedes evaluation of its arguments.
        // The missing argument raises before the factory body can run.
        if let Some(callable) = callable {
            self.walk(scope, callable, at);
        }
        true
    }

    fn closed_flask_initializers(&mut self, scope: ScopeId, prefix: &[Statement]) -> bool {
        // The absence proof applies to closed initialization. In particular,
        // an arbitrary call can attach an attribute to a Flask Config object.
        let read = std::mem::take(&mut self.read);
        let closed = (0..self.files.len()).all(|file| {
            let statements = if file == scope {
                prefix
            } else {
                self.scopes[file].body
            };
            self.closed_flask_decorator_prefix(file, statements)
        });
        self.read = read;
        closed
    }

    fn closed_flask_decorator_prefix(&mut self, file: ScopeId, body: &[Statement]) -> bool {
        body.iter().all(|statement| match &statement.kind {
            StatementKind::Import(import) if !import.local.text.starts_with("__") => self.import_module(file, import).is_some_and(|module| {
                self.modules.contains_key(module.as_str())
                    || matches!(module.as_str(), "flask" | "flask.app" | "flask_login" | "flask_login.login_manager")
            }),
            StatementKind::Define(definition) => (definition.kind != DefinitionKind::Class
                || (definition.bases.is_empty() && definition.table_keyword.is_none()
                    && body_without_docstring(definition).is_empty()))
                && !definition.name.text.starts_with("__")
                && definition.decorators.is_empty()
                && definition.annotations.is_empty()
                && definition.defaults.iter().all(|value| safe_flask_literal(&value.kind)),
            StatementKind::Evaluate(Expression { kind: ExpressionKind::Text(_), .. }) => true,
            StatementKind::Assign { names, value: Some(value), annotation: None }
                if matches!(names.as_slice(), [name] if !name.text.starts_with("__")) => {
                match &value.kind {
                    ExpressionKind::Name(_) => true,
                    ExpressionKind::Member(_, member) if member == "config" =>
                        matches!(self.probe_identity(file, value, statement.at), Value::FlaskConfig),
                    ExpressionKind::Call { function, arguments, keywords } if keywords.is_empty() => {
                        let no_arguments = arguments.is_empty();
                        let at = statement.at;
                        match self.probe_identity(file, function, at) {
                            Value::Provider(provider) => provider_kind(&provider).is_some_and(|kind| {
                                if self.flask_constructor_changed(kind, (file, at)) {
                                    return false;
                                }
                                match kind {
                                    ObjectKind::FlaskApp => matches!(arguments.as_slice(), [import_name]
                                        if self.flask_literal_import_name(file, import_name, at)),
                                    ObjectKind::FlaskBlueprint => matches!(arguments.as_slice(), [
                                        Expression { kind: ExpressionKind::Text(name), .. }, import_name,
                                    ] if !name.is_empty() && !name.contains('.')
                                        && self.flask_literal_import_name(file, import_name, at)),
                                    ObjectKind::FlaskLoginManager => no_arguments,
                                    _ => false,
                                }
                            }),
                            Value::Callable(invocation) if no_arguments => {
                                self.is_coroutine(invocation.target)
                                    || matches!(
                                        self.flask_factory_result(invocation, file, value.at, 0),
                                        Some(Value::FlaskConfig | Value::Object(_, ObjectKind::FlaskApp))
                                    )
                            }
                            _ => false,
                        }
                    }
                    kind => safe_flask_literal(kind),
                }
            }
            _ => false,
        })
    }

    pub(super) fn flask_blueprint_import_position(
        &self,
        child: usize,
        module: &str,
        member: &str,
    ) -> Option<Location> {
        let (package, child_name) = self.files[child].module.rsplit_once('.')?;
        let &parent = self.modules.get(package)?;
        if package != module || !self.files[parent].is_package || child_name == member {
            return None;
        }

        // A package can publish its Blueprint before importing the module that
        // decorates its callbacks. That child observes the initialized prefix,
        // even when Python originally entered through the child module name.
        let mut constructor = None;
        let mut assigned = false;
        for statement in self.scopes[parent].body {
            match &statement.kind {
                StatementKind::Evaluate(Expression {
                    kind: ExpressionKind::Text(_),
                    ..
                }) => {}
                StatementKind::Import(import)
                    if !assigned
                        && import.level == 0
                        && import.module == "flask"
                        && import.member.as_deref() == Some("Blueprint")
                        && !self.modules.contains_key("flask") =>
                {
                    constructor = Some(import.local.text.as_str());
                }
                StatementKind::Assign {
                    names,
                    value: Some(value),
                    annotation: None,
                } if !assigned
                    && matches!(names.as_slice(), [name] if name.text == member)
                    && matches!(&value.kind, ExpressionKind::Call { function, arguments, keywords }
                            if matches!(&function.kind, ExpressionKind::Name(name)
                                if Some(name.as_str()) == constructor)
                            && keywords.is_empty()
                            && matches!(arguments.as_slice(), [
                                Expression { kind: ExpressionKind::Text(_), .. },
                                Expression { kind: ExpressionKind::Name(name), .. },
                            ] if name == "__name__")) =>
                {
                    assigned = true;
                }
                StatementKind::Import(import)
                    if assigned
                        && import.member.as_deref() == Some(child_name)
                        && self.import_module(parent, import).as_deref() == Some(package) =>
                {
                    return Some(statement.at);
                }
                _ => return None,
            }
        }
        None
    }

    pub(super) fn collect_flask_cli_factory(&mut self) {
        let mut selected = None;
        for file in 0..self.files.len() {
            let source = &self.files[file];
            let cli_module = matches!(source.module.as_str(), "app" | "wsgi");
            let top_package = source.is_package && !source.module.contains('.');
            if !cli_module && !top_package {
                continue;
            }
            // Flask prefers an exported application to a factory.
            if ["app", "application"].iter().any(|name| {
                self.local(file, name, END).is_some_and(|binding| {
                    matches!(
                        self.binding_value(binding, 0),
                        Value::Object(_, ObjectKind::FlaskApp)
                    )
                })
            }) {
                continue;
            }
            let Some(factory) = ["create_app", "make_app"]
                .iter()
                .find_map(|name| self.local(file, name, END))
            else {
                continue;
            };
            let Binding::Definition(definition, body) = self.symbols[factory].binding else {
                continue;
            };
            if self.conditional.contains_key(&factory)
                || definition.kind != DefinitionKind::Function
                || definition.is_generator
                || !definition.decorators.is_empty()
                || !definition.simple_positional_parameters
                || definition.parameters.len() != definition.defaults.len()
            {
                continue;
            }
            let Some(Statement {
                at,
                kind: StatementKind::Return(Some(value)),
            }) = definition.body.last()
            else {
                continue;
            };
            if matches!(
                self.probe_identity(body, value, *at),
                Value::Object(_, ObjectKind::FlaskApp)
            ) && selected.replace(factory).is_some()
            {
                return;
            }
        }
        // The CLI can select the qualifying package from outside the scan.
        if let Some(target) = selected {
            self.framework_roots.push(Edge {
                target,
                kind: "flask_cli_factory",
                certainty: "possible",
            });
        }
    }

    pub(super) fn record_flask_parameter_hook(
        &mut self,
        scope: ScopeId,
        function: &Expression,
        arguments: &[Expression],
        keywords: &[(String, Expression)],
        before: Location,
        at: Location,
    ) -> bool {
        let ExpressionKind::Member(base, method) = &function.kind else {
            return false;
        };
        if method != "teardown_appcontext" {
            return false;
        }
        let ExpressionKind::Name(name) = &base.kind else {
            return false;
        };
        let Some(parameter) = self.local(scope, name, before) else {
            return false;
        };
        let Some(owner) = self.scopes[scope].owner else {
            return false;
        };
        if !matches!(self.symbols[parameter].binding, Binding::Parameter) {
            return false;
        }
        let Some(argument) = callback_argument(ObjectKind::FlaskApp, method, arguments, keywords)
        else {
            return false;
        };
        let Value::Callable(callback) = self.identity(scope, argument, before, 0) else {
            return false;
        };
        if !matches!(callback.receiver, Receiver::Ordinary) || self.is_class(callback.target) {
            return false;
        }
        self.flask_hooks.push(ParameterHook {
            owner,
            scope,
            parameter,
            target: callback.target,
            at,
        });
        true
    }

    fn flask_hook_callers<'r>(
        &'r self,
        hook: &'r ParameterHook,
        reach: &'r Reachability,
    ) -> impl Iterator<Item = &'r Value> {
        let name = self.symbols[hook.parameter].name.text.as_str();
        self.direct_arguments
            .get(&hook.owner)
            .into_iter()
            .flatten()
            .filter(|call| reach.scopes.contains(&call.caller))
            .map(move |call| call.values.get(name).unwrap_or(&Value::Unknown))
    }

    pub(super) fn activate_flask_parameter_hooks(
        &self,
        reach: &Reachability,
        pending: &mut VecDeque<Edge>,
    ) {
        for hook in &self.flask_hooks {
            if reach.scopes.contains(&hook.scope)
                && !reach.has_evidence(hook.target, "flask_registration")
                && self.flask_hook_callers(hook, reach).any(|value| {
                    matches!(value, Value::Object(app, ObjectKind::FlaskApp) if reach.active_objects.contains(app))
                })
            {
                pending.push_back(Edge { target: hook.target, kind: "flask_registration", certainty: "possible" });
            }
        }
    }

    pub(super) fn report_flask_parameter_hooks(&mut self, reach: &Reachability) {
        for hook in &self.flask_hooks {
            if !reach.scopes.contains(&hook.scope) {
                continue;
            }
            let finite = {
                let mut callers = self.flask_hook_callers(hook, reach).peekable();
                callers.peek().is_some()
                    && callers.all(|value| matches!(value, Value::Object(_, ObjectKind::FlaskApp)))
                    && reach
                        .evidence
                        .iter()
                        .filter(|(id, _, _)| *id == hook.owner)
                        .all(|(_, kind, _)| *kind == "direct_call")
            };
            if !finite {
                let file = self.scopes[hook.scope].file;
                self.issues.push(Issue::new(
                    "unresolved_registration", &self.files[file].path,
                    "Flask setup parameter does not resolve to a Flask app for all reached callers",
                ).at(hook.at));
            }
        }
    }
}

impl Graph<'_> {
    pub(super) fn index_flask_context(&mut self) {
        let mut keys = BTreeSet::new();
        for scope in 0..self.scopes.len() {
            if !self.collect_flask_context_keys(scope, self.scopes[scope].body, &mut keys) {
                return;
            }
        }
        self.flask_context_writes = Some(keys);
    }

    // A Flask context starts with no application keys. Prove absence only
    // when every selected use is a direct write or a bounded pop. An escape,
    // dynamic mutation, or extension import prevents that proof.
    fn collect_flask_context_keys(
        &mut self,
        scope: ScopeId,
        body: &[Statement],
        keys: &mut BTreeSet<String>,
    ) -> bool {
        for statement in body {
            let at = statement.at;
            let mut expressions = Vec::new();
            match &statement.kind {
                StatementKind::Import(import) => {
                    let Some(module) = self.import_module(self.scopes[scope].file, import) else {
                        return false;
                    };
                    if !matches!(module.as_str(), "flask" | "flask.globals")
                        && !self.modules.contains_key(module.as_str())
                    {
                        return false;
                    }
                }
                StatementKind::WriteAttribute {
                    receiver,
                    member,
                    value: Some(value),
                } if self.is_flask_context_receiver(scope, receiver, member, at) => {
                    keys.insert(member.clone());
                    expressions.push(value);
                }
                StatementKind::Define(definition) => {
                    expressions.extend(&definition.decorators);
                    expressions.extend(&definition.defaults);
                    expressions.extend(&definition.annotations);
                    expressions.extend(&definition.bases);
                }
                StatementKind::If {
                    test,
                    body,
                    otherwise,
                } => {
                    if !self.collect_flask_context_keys(scope, body, keys)
                        || !self.collect_flask_context_keys(scope, otherwise, keys)
                    {
                        return false;
                    }
                    expressions.push(test);
                }
                StatementKind::Assign {
                    value, annotation, ..
                } => {
                    expressions.extend(value);
                    expressions.extend(annotation);
                }
                StatementKind::Evaluate(expr)
                | StatementKind::Assert(expr)
                | StatementKind::DiscardedComprehension(expr) => expressions.push(expr),
                StatementKind::Return(value) | StatementKind::Raise(value) => {
                    expressions.extend(value)
                }
                // Other effectful forms fall outside the context proof.
                _ => return false,
            }
            if !self.bounded_flask_context_expressions(scope, at, expressions) {
                return false;
            }
        }
        true
    }

    fn bounded_flask_context_expressions(
        &mut self,
        scope: ScopeId,
        at: Location,
        mut pending: Vec<&Expression>,
    ) -> bool {
        while let Some(expr) = pending.pop() {
            match &expr.kind {
                ExpressionKind::Call {
                    function,
                    arguments,
                    keywords,
                } => {
                    if matches!(self.probe_identity(scope, function, at), Value::Provider(name)
                        if flask_context_pop_key(&name, arguments, keywords).is_some())
                    {
                        continue;
                    }
                    pending.push(function);
                    pending.extend(arguments);
                    pending.extend(keywords.iter().map(|(_, expr)| expr));
                }
                ExpressionKind::Name(_) | ExpressionKind::Member(..) => {
                    if let Value::Provider(name) = self.probe_identity(scope, expr, at) {
                        match name.as_str() {
                            "flask" | "flask.globals" | "flask.g" | "flask.globals.g" => {
                                return false;
                            }
                            "flask.Flask" | "flask.Blueprint" => continue,
                            _ => {}
                        }
                    }
                    if let ExpressionKind::Member(base, _) = &expr.kind {
                        pending.push(base);
                    }
                }
                ExpressionKind::Compare { left, right, .. } => {
                    pending.extend([left.as_ref(), right.as_ref()]);
                }
                ExpressionKind::Text(_)
                | ExpressionKind::Bool(_)
                | ExpressionKind::None
                | ExpressionKind::Scalar(_) => {}
                _ => return false,
            }
        }
        true
    }

    fn is_flask_context_receiver(
        &mut self,
        scope: ScopeId,
        receiver: &Expression,
        member: &str,
        at: Location,
    ) -> bool {
        member != "pop"
            && !member.starts_with('_')
            && matches!(self.probe_identity(scope, receiver, at),
            Value::Provider(name) if matches!(name.as_str(), "flask.g" | "flask.globals.g"))
    }

    pub(super) fn modeled_flask_context_write(
        &mut self,
        scope: ScopeId,
        receiver: &Expression,
        member: &str,
        at: Location,
    ) -> bool {
        self.flask_context_writes.is_some()
            && self.is_flask_context_receiver(scope, receiver, member, at)
    }

    pub(super) fn empty_flask_context_pop(
        &self,
        name: &str,
        arguments: &[Expression],
        keywords: &[(String, Expression)],
    ) -> bool {
        self.flask_context_writes.as_ref().is_some_and(|writes| {
            flask_context_pop_key(name, arguments, keywords)
                .is_some_and(|key| !writes.contains(key))
        })
    }
}

fn flask_context_pop_key<'a>(
    name: &str,
    arguments: &'a [Expression],
    keywords: &[(String, Expression)],
) -> Option<&'a str> {
    if !matches!(name, "flask.g.pop" | "flask.globals.g.pop") || !keywords.is_empty() {
        return None;
    }
    let [key, default] = arguments else {
        return None;
    };
    match (&key.kind, &default.kind) {
        (ExpressionKind::Text(key), ExpressionKind::None) if !key.starts_with('_') => Some(key),
        _ => None,
    }
}

fn safe_flask_literal(kind: &ExpressionKind) -> bool {
    matches!(
        kind,
        ExpressionKind::None
            | ExpressionKind::Bool(_)
            | ExpressionKind::Text(_)
            | ExpressionKind::Scalar
    )
}
