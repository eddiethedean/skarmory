//! Flask factory discovery and teardown registration through local setup helpers.

use super::*;

enum FailedProvider<'a> {
    Expressions(Vec<&'a Expression>),
    GlobalWrite(&'a Expression, SymbolId),
}

type FlaskImporters = BTreeMap<ScopeId, BTreeMap<ScopeId, usize>>;

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
    pub(super) fn call_inert_flask_blueprint(
        &mut self,
        class: SymbolId,
        scope: ScopeId,
        before: Location,
        at: Location,
    ) -> bool {
        let access = (scope, before);
        if !self.plain_default_provider(class, access, &BTreeSet::new()) {
            return false;
        }
        let Value::Callable(method) = self.member(Value::Instance(class), "register", access, 0)
        else {
            return false;
        };
        let Binding::Definition(definition, _) = self.symbols[method.target].binding else {
            return false;
        };
        if method.verified
            || !matches!(method.receiver, Receiver::Bound(receiver) if receiver == class)
            || !field_dispatch::plain_method(definition, 3)
            || !body_without_docstring(definition).is_empty()
        {
            return false;
        }
        // Flask delegates to blueprint.register(app, options). A local
        // no-op method executes without mounting a framework Blueprint.
        self.invoke(scope, method, "direct_call", "certain", at);
        true
    }

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
        if !self.flask_app_allocation(scope, constructor, before, depth + 1) {
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

    fn flask_app_allocation(
        &mut self,
        scope: ScopeId,
        expression: &Expression,
        before: Location,
        depth: usize,
    ) -> bool {
        let ExpressionKind::Call {
            function,
            arguments,
            keywords,
        } = &expression.kind
        else {
            return false;
        };
        keywords.is_empty()
            && matches!(arguments.as_slice(), [name] if self.flask_literal_import_name(scope, name, before))
            && matches!(
                function.kind,
                ExpressionKind::Name(_) | ExpressionKind::Member(_, _)
            )
            && matches!(self.identity(scope, function, before, depth),
                Value::Provider(name) if provider_kind(&name) == Some(ObjectKind::FlaskApp))
            && !self.flask_constructor_changed(ObjectKind::FlaskApp, (scope, before))
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
        let Some(base) = flask_receiver_decorator(definition) else {
            return false;
        };
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
        let at = statement.at;
        let read = std::mem::take(&mut self.read);
        let failure = self.flask_failure_reads(scope, statement);
        self.read = read;
        let Some(reads) = failure else {
            return false;
        };
        let body = self.scopes[scope].body;
        let Some(index) = body.iter().position(|item| std::ptr::eq(item, statement)) else {
            return false;
        };
        let prefix = &body[..index];
        let local_initializer = matches!(&statement.kind, StatementKind::Assign { names, .. } if names.len() == 1)
            && prefix
                .iter()
                .any(|item| self.flask_constructor_import(scope, item));
        let read = std::mem::take(&mut self.read);
        let followed_by_callback = body
            .get(index + 1)
            .and_then(initializer_flask_receiver)
            .is_some_and(|base| self.flask_provider_expression(scope, base, at));
        self.read = read;
        if (!local_initializer && !followed_by_callback)
            || !self.closed_flask_initializers(scope, prefix)
        {
            return false;
        }
        for expression in reads {
            self.walk(scope, expression, at);
        }
        true
    }

    pub(super) fn terminal_flask_factory_initializer(
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
        let [name] = names.as_slice() else {
            return false;
        };
        let ExpressionKind::Call {
            function,
            arguments,
            keywords,
        } = &value.kind
        else {
            return false;
        };
        if !matches!(function.kind, ExpressionKind::Name(_))
            || !arguments.is_empty()
            || !keywords.is_empty()
        {
            return false;
        }
        let body = self.scopes[scope].body;
        let Some(index) = body.iter().position(|item| std::ptr::eq(item, statement)) else {
            return false;
        };
        let Some(next) = body.get(index + 1) else {
            return false;
        };
        if !matches!(initializer_flask_receiver(next), Some(Expression { kind: ExpressionKind::Name(receiver), .. }) if receiver == &name.text)
        {
            return false;
        }
        let at = statement.at;
        let read = std::mem::take(&mut self.read);
        let found = self.probe_identity(scope, function, at);
        self.read = read;
        let Value::Callable(invocation) = found else {
            return false;
        };
        let Binding::Definition(factory, factory_scope) = self.symbols[invocation.target].binding
        else {
            return false;
        };
        let provider = self.scopes[factory_scope].file;
        let prefix = &body[..index];
        let provider_body = if provider == scope {
            prefix
        } else {
            self.scopes[provider].body
        };
        if invocation.verified
            || !matches!(invocation.receiver, Receiver::Ordinary)
            || self.symbols[invocation.target].scope != provider
            || factory.kind == DefinitionKind::Class
            || !factory.decorators.is_empty()
            || !factory.simple_positional_parameters
            || !provider_body
                .iter()
                .any(|item| self.flask_constructor_import(provider, item))
            || !self.closed_flask_initializers(scope, prefix)
        {
            return false;
        }
        let missing_arguments = factory.parameters.len() > factory.defaults.len();
        let read = std::mem::take(&mut self.read);
        let fails_in_body = factory.kind == DefinitionKind::Function
            && !factory.is_generator
            && factory.parameters.is_empty()
            && self.closed_flask_factory_body_failure(factory, factory_scope);
        self.read = read;
        if !missing_arguments && !factory.is_generator && !fails_in_body {
            return false;
        }
        self.walk(scope, function, at);
        if !missing_arguments {
            // Calling a generator only creates its value. Its first use is
            // the unsupported attribute lookup, so nothing can consume it.
            let kind = if factory.is_generator {
                self.read
                    .insert(self.by_location[&(scope, name.at.line, name.at.column)]);
                "generator_creation"
            } else {
                "flask_factory_failed_call"
            };
            self.edge(scope, invocation.target, kind, "certain");
        }
        true
    }

    pub(super) fn terminal_flask_global_initializer(
        &mut self,
        scope: ScopeId,
        statement: &Statement,
    ) -> bool {
        let Some((function, target)) = self.flask_global_initializer_failure(scope, statement)
        else {
            return false;
        };
        let body = self.scopes[scope].body;
        let Some(index) = body.iter().position(|item| std::ptr::eq(item, statement)) else {
            return false;
        };
        if !self.closed_flask_initializers(scope, &body[..index]) {
            return false;
        }
        self.walk(scope, function, statement.at);
        self.edge(scope, target, "flask_initializer_failed_call", "certain");
        true
    }

    fn flask_global_initializer_failure<'a>(
        &mut self,
        scope: ScopeId,
        statement: &'a Statement,
    ) -> Option<(&'a Expression, SymbolId)> {
        let StatementKind::Evaluate(Expression {
            kind:
                ExpressionKind::Call {
                    function,
                    arguments,
                    keywords,
                },
            ..
        }) = &statement.kind
        else {
            return None;
        };
        if !matches!(function.kind, ExpressionKind::Name(_))
            || !arguments.is_empty()
            || !keywords.is_empty()
        {
            return None;
        }
        let at = statement.at;
        let read = std::mem::take(&mut self.read);
        let found = self.probe_identity(scope, function, at);
        self.read = read;
        let Value::Callable(Invocation {
            target,
            receiver: Receiver::Ordinary,
            implicit_descriptor_receiver: false,
            verified: false,
        }) = found
        else {
            return None;
        };
        let Binding::Definition(definition, _) = self.symbols[target].binding else {
            return None;
        };
        if self.symbols[target].scope != scope
            || definition.kind != DefinitionKind::Function
            || definition.is_generator
            || !definition.parameters.is_empty()
            || !definition.decorators.is_empty()
        {
            return None;
        }
        let [
            Statement {
                kind: StatementKind::SetGlobalName { name, value },
                ..
            },
        ] = body_without_docstring(definition)
        else {
            return None;
        };
        // The helper reads its global at the call position, before changing the
        // receiver. A declaration earlier in the module cannot establish absence.
        if !self.unbound_module_name(scope, value, at) {
            return None;
        }
        let receiver = Expression {
            kind: ExpressionKind::Name(name.text.clone()),
            at,
        };
        let read = std::mem::take(&mut self.read);
        let receiver = self.probe_identity(scope, &receiver, at);
        self.read = read;
        let flask_receiver = match receiver {
            Value::Object(
                _,
                ObjectKind::FlaskApp | ObjectKind::FlaskBlueprint | ObjectKind::FlaskLoginManager,
            ) => true,
            Value::Provider(name) => provider_kind(&name).is_some(),
            _ => false,
        };
        flask_receiver.then_some((function, target))
    }

    fn flask_constructor_import(&self, scope: ScopeId, statement: &Statement) -> bool {
        matches!(&statement.kind, StatementKind::Import(import) if import.level == 0
            && matches!(import.module.as_str(),
                "flask" | "flask.app" | "flask.blueprints" | "flask_login" | "flask_login.login_manager")
            && self.import_module(scope, import)
                .is_some_and(|module| !self.modules.contains_key(module.as_str()))
            && !self.scopes[scope].open_wildcard)
    }

    pub(super) fn index_failed_flask_provider_initializers(&mut self) {
        if !self.terminal_proofs {
            return;
        }
        let has_flask_provider = (0..self.files.len()).any(|file| {
            self.scopes[file]
                .body
                .iter()
                .any(|item| self.flask_constructor_import(file, item))
        });
        let mut imports = FlaskImporters::new();
        for file in 0..self.files.len() {
            for (index, statement) in self.scopes[file].body.iter().enumerate() {
                if let StatementKind::Import(import) = &statement.kind
                    && let Some(module) = self.import_module(file, import)
                    && let Some(&provider) = self.modules.get(module.as_str())
                    && provider != file
                    && self.import_member_position(file, &module, import).is_none()
                {
                    imports
                        .entry(provider)
                        .or_default()
                        .entry(file)
                        .or_insert(index);
                }
            }
        }
        let read = std::mem::take(&mut self.read);
        for (&provider, direct_importers) in &imports {
            if self.is_pytest_file(provider)
                || self.files[provider].path.rsplit('/').next() == Some("conftest.py")
            {
                continue;
            }
            if has_flask_provider
                && let Some(failure) =
                    self.closed_flask_provider_failure(provider, direct_importers)
            {
                let stopped = self.closed_flask_importer_prefixes(provider, &imports);
                if self.closed_flask_project_prefixes(&stopped, Some((provider, failure))) {
                    self.failed_flask_provider_initializers
                        .insert(provider, failure);
                    self.record_failed_flask_imports(stopped);
                }
            }
            if let Some(failure) = self.flask_reexport_cycle(provider, direct_importers) {
                let mut stopped = self.closed_flask_importer_prefixes(provider, &imports);
                stopped.insert(provider, failure);
                if self.closed_flask_project_prefixes(&stopped, None) {
                    self.record_failed_flask_imports(stopped);
                }
            }
        }
        self.read = read;
    }

    fn closed_flask_project_prefixes(
        &mut self,
        stopped: &BTreeMap<ScopeId, usize>,
        checked_provider: Option<(ScopeId, Location)>,
    ) -> bool {
        // A statement after the proposed failure cannot install a framework
        // mutation. Test the remaining prefixes with only their reached
        // writes, and discard the cutoffs if those prefixes are opaque.
        self.flask_initializer_probe_cutoffs = stopped
            .iter()
            .map(|(&file, &index)| (file, self.scopes[file].body[index].at))
            .chain(checked_provider)
            .collect();
        let closed = (0..self.files.len())
            .filter(|&file| !checked_provider.is_some_and(|(provider, _)| file == provider))
            .all(|file| {
                let body = self.scopes[file].body;
                let prefix = stopped.get(&file).map_or(body, |&index| &body[..index]);
                self.closed_flask_decorator_prefix(file, prefix)
            });
        self.flask_initializer_probe_cutoffs.clear();
        closed
    }

    pub(super) fn before_flask_initializer_failure(&self, file: ScopeId, at: Location) -> bool {
        [
            &self.flask_initializer_probe_cutoffs,
            &self.failed_flask_provider_initializers,
            &self.failed_flask_import_initializers,
        ]
        .into_iter()
        .all(|cutoffs| {
            cutoffs
                .get(&file)
                .is_none_or(|&failure| position(at) < position(failure))
        })
    }

    fn record_failed_flask_imports(&mut self, stopped: BTreeMap<ScopeId, usize>) {
        for (file, index) in stopped {
            let at = self.scopes[file].body[index].at;
            self.failed_flask_import_initializers
                .entry(file)
                .and_modify(|earlier| {
                    if position(at) < position(*earlier) {
                        *earlier = at;
                    }
                })
                .or_insert(at);
        }
    }

    fn flask_reexport_cycle(
        &self,
        provider: ScopeId,
        importers: &BTreeMap<ScopeId, usize>,
    ) -> Option<usize> {
        let (initial, import) = self.initial_flask_reexport(provider)?;
        let name = import.local.text.as_str();
        if !importers.iter().any(|(&file, &index)| {
            self.reads_flask_receiver_decorator(file, index, name, ObjectKind::FlaskLoginManager)
        }) {
            return None;
        }
        let mut visited = BTreeSet::new();
        let mut file = provider;
        let mut expected = name;
        for _ in 0..MAX_BINDING_CHAIN {
            if !visited.insert((file, expected)) {
                // The named import must finish before any participant binds
                // the requested export, whichever member Python enters first.
                return Some(initial);
            }
            let (_, import) = self.initial_flask_reexport(file)?;
            if import.local.text != expected {
                return None;
            }
            expected = import.member.as_deref()?;
            let module = self.import_module(file, import)?;
            file = *self.modules.get(module.as_str())?;
        }
        None
    }

    fn initial_flask_reexport(&self, file: ScopeId) -> Option<(usize, &Import)> {
        let row = &self.files[file];
        if row.is_package
            || row.module.contains('.')
            || self.is_pytest_file(file)
            || row.path.rsplit('/').next() == Some("conftest.py")
        {
            return None;
        }
        let (index, statement) =
            self.scopes[file]
                .body
                .iter()
                .enumerate()
                .find(|(_, statement)| {
                    !matches!(
                        statement.kind,
                        StatementKind::Evaluate(Expression {
                            kind: ExpressionKind::Text(_),
                            ..
                        })
                    )
                })?;
        let StatementKind::Import(import) = &statement.kind else {
            return None;
        };
        (import.level == 0
            && import
                .member
                .as_deref()
                .is_some_and(|member| !member.starts_with('_')))
        .then_some((index, import))
    }

    fn closed_flask_importer_prefixes(
        &self,
        provider: ScopeId,
        imports: &FlaskImporters,
    ) -> BTreeMap<ScopeId, usize> {
        let mut stopped = BTreeMap::new();
        let mut pending = vec![provider];
        while let Some(target) = pending.pop() {
            let Some(importers) = imports.get(&target) else {
                continue;
            };
            for (&file, &index) in importers {
                // Without an earlier selected import, this initializer cannot
                // be re-entered through a partially initialized source module.
                if file == provider
                    || self.is_pytest_file(file)
                    || self.files[file].path.rsplit('/').next() == Some("conftest.py")
                    || self.scopes[file].body[..index]
                        .iter()
                        .any(|item| self.flask_local_import(file, item))
                {
                    continue;
                }
                if let Entry::Vacant(entry) = stopped.entry(file) {
                    entry.insert(index);
                    pending.push(file);
                }
            }
        }
        stopped
    }

    fn closed_flask_provider_failure(
        &mut self,
        provider: ScopeId,
        importers: &BTreeMap<ScopeId, usize>,
    ) -> Option<Location> {
        let body = self.scopes[provider].body;
        let Some(start) = body
            .iter()
            .position(|stmt| self.flask_constructor_import(provider, stmt))
        else {
            return self.closed_blueprint_lookalike_failure(provider, importers);
        };
        if !self.closed_flask_decorator_prefix(provider, &body[..=start])
            || body[..start]
                .iter()
                .any(|statement| self.flask_local_import(provider, statement))
        {
            return None;
        }
        for statement in &body[start + 1..] {
            if self.flask_provider_failure(provider, statement).is_some() {
                return Some(statement.at);
            }
            if self.flask_local_import(provider, statement)
                || !self.closed_flask_decorator_prefix(provider, std::slice::from_ref(statement))
            {
                break;
            }
        }
        None
    }

    fn flask_local_import(&self, file: ScopeId, statement: &Statement) -> bool {
        matches!(&statement.kind, StatementKind::Import(import)
            if self.import_module(file, import)
                .is_some_and(|module| self.modules.contains_key(module.as_str())))
    }

    fn closed_blueprint_lookalike_failure(
        &mut self,
        provider: ScopeId,
        importers: &BTreeMap<ScopeId, usize>,
    ) -> Option<Location> {
        let body = self.scopes[provider].body;
        for (index, statement) in body.iter().enumerate() {
            let Some((export, _, class_file)) = self.blueprint_lookalike_call(provider, statement)
            else {
                continue;
            };
            if !importers.iter().any(|(&file, &index)| {
                self.reads_flask_receiver_decorator(
                    file,
                    index,
                    &export.text,
                    ObjectKind::FlaskBlueprint,
                )
            }) {
                continue;
            }
            let prefix = &body[..index];
            if !self.closed_flask_decorator_prefix(provider, prefix)
                || prefix.iter().any(|item| {
                    if let StatementKind::Import(import) = &item.kind
                        && let Some(module) = self.import_module(provider, import)
                        && let Some(&source) = self.modules.get(module.as_str())
                    {
                        return source != class_file || class_file == provider;
                    }
                    false
                })
            {
                continue;
            }
            if class_file != provider {
                let body = self.scopes[class_file].body;
                if !self.closed_flask_decorator_prefix(class_file, body)
                    || body
                        .iter()
                        .any(|item| self.flask_local_import(class_file, item))
                {
                    continue;
                }
            }
            return Some(statement.at);
        }
        None
    }

    fn reads_flask_receiver_decorator(
        &self,
        file: ScopeId,
        index: usize,
        export: &str,
        kind: ObjectKind,
    ) -> bool {
        let body = self.scopes[file].body;
        let StatementKind::Import(import) = &body[index].kind else {
            return false;
        };
        if import.member.as_deref() != Some(export) {
            return false;
        }
        body[index + 1..].iter().any(|statement| {
            let StatementKind::Define(definition) = &statement.kind else {
                return false;
            };
            definition.decorators.iter().any(|decorator| {
                let lookup = match &decorator.kind {
                    ExpressionKind::Call { function, .. } => function.as_ref(),
                    _ => decorator,
                };
                matches!(&lookup.kind, ExpressionKind::Member(base, member)
                    if (is_route(kind, member, true) || kind == ObjectKind::FlaskLoginManager && flask_login_callback(member))
                    && matches!(&base.kind, ExpressionKind::Name(name) if name == &import.local.text)
                    && self.local(file, &import.local.text, statement.at)
                        .is_some_and(|id| matches!(self.symbols[id].binding, Binding::Import(active)
                            if std::ptr::eq(active, import))))
            })
        })
    }

    fn blueprint_lookalike_call<'a>(
        &mut self,
        provider: ScopeId,
        statement: &'a Statement,
    ) -> Option<(&'a Name, &'a Expression, ScopeId)> {
        let StatementKind::Assign {
            names,
            value:
                Some(Expression {
                    kind:
                        ExpressionKind::Call {
                            function,
                            arguments,
                            keywords,
                        },
                    ..
                }),
            annotation: None,
        } = &statement.kind
        else {
            return None;
        };
        let [export] = names.as_slice() else {
            return None;
        };
        let [
            Expression {
                kind: ExpressionKind::Text(_),
                ..
            },
            import_name,
        ] = arguments.as_slice()
        else {
            return None;
        };
        if !matches!(function.kind, ExpressionKind::Name(_))
            || !keywords.is_empty()
            || !self.flask_literal_import_name(provider, import_name, statement.at)
        {
            return None;
        }
        let Value::Callable(call) = self.probe_identity(provider, function, statement.at) else {
            return None;
        };
        let symbol = &self.symbols[call.target];
        let Binding::Definition(class, _) = symbol.binding else {
            return None;
        };
        // object.__new__ and object.__init__ together reject these arguments
        // on an undecorated empty class, before any blueprint is available.
        (symbol.scope == self.scopes[symbol.scope].file
            && symbol.name.text == "Blueprint"
            && !call.verified
            && matches!(call.receiver, Receiver::Ordinary)
            && super::terminal::plain_class(class))
        .then_some((export, function.as_ref(), symbol.scope))
    }

    pub(super) fn terminal_failed_flask_provider(
        &mut self,
        scope: ScopeId,
        statement: &Statement,
    ) -> bool {
        if !self
            .failed_flask_provider_initializers
            .get(&scope)
            .is_some_and(|&at| position(at) == position(statement.at))
        {
            return false;
        }
        let read = std::mem::take(&mut self.read);
        let failure = self.flask_provider_failure(scope, statement).or_else(|| {
            self.blueprint_lookalike_call(scope, statement)
                .map(|(_, function, _)| FailedProvider::Expressions(vec![function]))
        });
        self.read = read;
        match failure {
            Some(FailedProvider::Expressions(expressions)) => {
                for expression in expressions {
                    self.walk(scope, expression, statement.at);
                }
                true
            }
            Some(FailedProvider::GlobalWrite(function, target)) => {
                self.walk(scope, function, statement.at);
                self.edge(scope, target, "flask_initializer_failed_call", "certain");
                true
            }
            None => false,
        }
    }

    pub(super) fn stop_at_failed_flask_provider_import(
        &mut self,
        scope: ScopeId,
        module: Option<&str>,
        import: &Import,
    ) -> bool {
        let file = self.scopes[scope].file;
        if self.branch_depth != 0
            || self.is_pytest_file(file)
            || self.files[file].path.rsplit('/').next() == Some("conftest.py")
        {
            return false;
        }
        let Some(module) = module else { return false };
        let Some(&provider) = self.modules.get(module) else {
            return false;
        };
        if !(self
            .failed_flask_provider_initializers
            .contains_key(&provider)
            || self
                .failed_flask_import_initializers
                .contains_key(&provider))
            || self.import_member_position(scope, module, import).is_some()
        {
            return false;
        }
        if scope == file {
            if !self.failed_flask_import_initializers.get(&file).is_some_and(|&failure| {
                let row = self.scopes[file].body.iter().find(|statement| {
                    matches!(&statement.kind, StatementKind::Import(item) if std::ptr::eq(item, import))
                });
                row.is_some_and(|statement| position(statement.at) == position(failure))
            }) {
                return false;
            }
        } else {
            let body = self.scopes[scope].body;
            let Some(index) = body.iter().position(|statement| {
                matches!(&statement.kind, StatementKind::Import(item) if std::ptr::eq(item, import))
            }) else {
                return false;
            };
            let read = std::mem::take(&mut self.read);
            let closed = self.in_function(scope)
                && self.closed_flask_decorator_prefix(scope, &body[..index]);
            self.read = read;
            if !closed {
                return false;
            }
        }
        self.import_issue(
            scope,
            file,
            import,
            format!("selected module {module:?} cannot finish initialization"),
        );
        if scope == file {
            self.failed_terminal_modules.insert(scope);
        }
        true
    }

    fn flask_provider_failure<'a>(
        &mut self,
        scope: ScopeId,
        statement: &'a Statement,
    ) -> Option<FailedProvider<'a>> {
        if let Some(reads) = self.flask_failure_reads(scope, statement) {
            return Some(FailedProvider::Expressions(reads));
        }
        if let Some((function, target)) = self.flask_global_initializer_failure(scope, statement) {
            return Some(FailedProvider::GlobalWrite(function, target));
        }
        let at = statement.at;
        let unbound = |graph: &Self, expression| graph.unbound_module_name(scope, expression, at);
        match &statement.kind {
            StatementKind::WriteModuleEntry {
                value: Some(value), ..
            } if unbound(self, value) => Some(FailedProvider::Expressions(Vec::new())),
            StatementKind::WriteAttribute {
                value:
                    Some(Expression {
                        kind: ExpressionKind::Member(base, _),
                        ..
                    }),
                ..
            } if unbound(self, base) => Some(FailedProvider::Expressions(Vec::new())),
            StatementKind::Define(definition)
                if definition
                    .decorators
                    .first()
                    .is_some_and(|value| unbound(self, value)) =>
            {
                Some(FailedProvider::Expressions(Vec::new()))
            }
            StatementKind::Evaluate(Expression {
                kind:
                    ExpressionKind::Call {
                        function,
                        arguments,
                        keywords,
                    },
                ..
            }) if keywords.is_empty() => self
                .flask_failed_patch_inputs(scope, function, arguments, at)
                .map(FailedProvider::Expressions),
            _ => None,
        }
    }

    fn flask_failed_patch_inputs<'a>(
        &mut self,
        scope: ScopeId,
        function: &'a Expression,
        arguments: &'a [Expression],
        at: Location,
    ) -> Option<Vec<&'a Expression>> {
        let ExpressionKind::Member(receiver, member) = &function.kind else {
            return None;
        };
        let ExpressionKind::Call {
            function: inner,
            arguments: inputs,
            keywords,
        } = &receiver.kind
        else {
            return None;
        };
        if !keywords.is_empty() {
            return None;
        }
        let provider = match self.probe_identity(scope, inner, at) {
            Value::Provider(provider) => provider,
            Value::Unknown
                if matches!(&inner.kind, ExpressionKind::Name(name) if name == "globals")
                    && matches!(
                        self.lexical_binding(scope, "globals", at, true),
                        LexicalBinding::Missing
                    ) =>
            {
                "builtins.globals".into()
            }
            _ => return None,
        };
        let replacement = match (
            member.as_str(),
            provider.as_str(),
            arguments,
            inputs.as_slice(),
        ) {
            (
                "update",
                "builtins.globals",
                [
                    Expression {
                        kind: ExpressionKind::Dictionary(items),
                        ..
                    },
                ],
                [],
            ) if matches!(
                items.as_slice(),
                [
                    Expression {
                        kind: ExpressionKind::Text(_),
                        ..
                    },
                    _
                ]
            ) =>
            {
                &items[1]
            }
            (
                "start",
                "unittest.mock.patch",
                [],
                [
                    Expression {
                        kind: ExpressionKind::Text(_),
                        ..
                    },
                    value,
                ],
            ) => value,
            (
                "start",
                "unittest.mock.patch.object",
                [],
                [
                    object,
                    Expression {
                        kind: ExpressionKind::Text(_),
                        ..
                    },
                    value,
                ],
            ) if matches!(self.probe_identity(scope, object, at), Value::Provider(name) if provider_kind(&name).is_some()) =>
            {
                return self
                    .unbound_module_name(scope, value, at)
                    .then_some(vec![inner, object]);
            }
            _ => return None,
        };
        self.unbound_module_name(scope, replacement, at)
            .then_some(vec![inner])
    }

    fn closed_flask_factory_body_failure(&mut self, factory: &Definition, scope: ScopeId) -> bool {
        let mut aliases = BTreeSet::new();
        let mut allocated = false;
        for statement in body_without_docstring(factory) {
            if self.flask_failure_reads(scope, statement).is_some() {
                return true;
            }
            let StatementKind::Assign {
                names,
                value: Some(value),
                annotation: None,
            } = &statement.kind
            else {
                return false;
            };
            let [name] = names.as_slice() else {
                return false;
            };
            match &value.kind {
                ExpressionKind::Name(alias) if aliases.contains(alias.as_str()) => {}
                ExpressionKind::Call { .. }
                    if !allocated && self.flask_app_allocation(scope, value, statement.at, 0) =>
                {
                    allocated = true;
                }
                ExpressionKind::DeferredLambda(_) if allocated => {}
                _ => return false,
            }
            aliases.insert(name.text.as_str());
        }
        false
    }

    fn flask_failure_reads<'a>(
        &mut self,
        scope: ScopeId,
        statement: &'a Statement,
    ) -> Option<Vec<&'a Expression>> {
        let at = statement.at;
        match &statement.kind {
            StatementKind::Assign {
                value: Some(value),
                annotation: None,
                ..
            }
            | StatementKind::Return(Some(value))
            | StatementKind::If { test: value, .. }
                if self.unbound_module_name(scope, value, at) =>
            {
                Some(vec![])
            }
            StatementKind::Assign {
                value:
                    Some(Expression {
                        kind:
                            ExpressionKind::Call {
                                function,
                                arguments,
                                keywords,
                            },
                        ..
                    }),
                annotation: None,
                ..
            } if keywords.is_empty()
                && matches!(&function.kind, ExpressionKind::Name(_))
                && matches!(arguments.as_slice(), [argument] if self.unbound_module_name(scope, argument, at)) =>
            {
                // Python reads the callable before its arguments.
                Some(vec![function])
            }
            StatementKind::WriteAttribute {
                value: Some(value), ..
            } if self.unbound_module_name(scope, value, at) => Some(vec![]),
            StatementKind::WriteAttribute {
                receiver,
                value: Some(value),
                ..
            } if self.unbound_module_name(scope, receiver, at)
                && self.flask_provider_expression(scope, value, at) =>
            {
                Some(vec![value])
            }
            StatementKind::Evaluate(Expression {
                kind:
                    ExpressionKind::Call {
                        function,
                        arguments,
                        keywords,
                    },
                ..
            }) => {
                if self.unbound_module_name(scope, function, at) {
                    return Some(vec![]);
                }
                if let [receiver, member, value] = arguments.as_slice()
                    && keywords.is_empty()
                    && matches!(&member.kind, ExpressionKind::Text(_))
                    && matches!(&function.kind, ExpressionKind::Name(name) if name == "setattr")
                    && matches!(self.probe_identity(scope, function, at), Value::Provider(name) if name == "builtins.setattr")
                    && self.unbound_module_name(scope, value, at)
                    && self.flask_provider_expression(scope, receiver, at)
                {
                    return Some(vec![function, receiver]);
                }
                None
            }
            _ => None,
        }
    }

    fn flask_provider_expression(
        &mut self,
        scope: ScopeId,
        expression: &Expression,
        before: Location,
    ) -> bool {
        matches!(expression.kind, ExpressionKind::Name(_))
            && matches!(
                self.probe_identity(scope, expression, before),
                Value::Object(_, ObjectKind::FlaskApp | ObjectKind::FlaskLoginManager)
            )
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
                    || matches!(module.as_str(), "flask" | "flask.app" | "flask.blueprints" | "flask_login" | "flask_login.login_manager" | "unittest.mock")
            }),
            StatementKind::Define(definition) => (definition.kind != DefinitionKind::Class
                || (definition.bases.is_empty() && !definition.has_class_keywords()
                    && body_without_docstring(definition).is_empty()))
                && !definition.name.text.starts_with("__")
                && (definition.decorators.is_empty()
                    || self.closed_flask_callback_declaration(file, definition, statement.at))
                && definition.annotations.is_empty()
                && definition.defaults.iter().all(|value| safe_flask_literal(&value.kind)),
            StatementKind::Evaluate(Expression { kind: ExpressionKind::Text(_), .. }) => true,
            StatementKind::Assign { names, value: Some(value), annotation: None }
                if matches!(names.as_slice(), [name] if !name.text.starts_with("__")) => {
                match &value.kind {
                    ExpressionKind::Name(_) => true,
                    ExpressionKind::Dictionary(items) => items.as_chunks::<2>().0.iter().all(|[key, value]| {
                        matches!(key.kind, ExpressionKind::Text(_))
                            && (safe_flask_literal(&value.kind) || matches!(value.kind, ExpressionKind::Name(_)))
                    }),
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

    fn closed_flask_callback_declaration(
        &mut self,
        scope: ScopeId,
        definition: &Definition,
        before: Location,
    ) -> bool {
        let [decorator] = definition.decorators.as_slice() else {
            return false;
        };
        // These resolved Flask descriptors only store the callback.
        // Its body does not execute during module initialization.
        definition.kind != DefinitionKind::Class
            && matches!(&decorator.kind, ExpressionKind::Member(..))
            && matches!(
                self.probe_identity(scope, decorator, before),
                Value::Method(_, kind, name)
                    if kind == ObjectKind::FlaskLoginManager && flask_login_callback(&name)
                        || flask_callback_method(kind, &name)
            )
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

        // A package can publish its Blueprint or a closed lookalike before
        // importing the module that decorates its callbacks. That child
        // observes the initialized prefix, including when it is the entry.
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
                StatementKind::Import(import)
                    if !assigned && self.flask_lookalike_constructor(parent, import) =>
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

    fn flask_lookalike_constructor(&self, scope: ScopeId, import: &Import) -> bool {
        let Some((name, provider)) = import
            .member
            .as_deref()
            .zip(self.import_module(scope, import))
        else {
            return false;
        };
        let Some(&file) = self.modules.get(provider.as_str()) else {
            return false;
        };
        !self.files[file].is_package
            && !provider.contains('.')
            && self
                .local(file, name, END)
                .is_some_and(|class| matches!(
                    self.symbols[class].binding,
                    Binding::Definition(definition, _) if inert_blueprint_lookalike(definition)
                ))
            && self.scopes[file].body.iter().all(|statement| match &statement.kind {
                StatementKind::Evaluate(Expression { kind: ExpressionKind::Text(_), .. }) => true,
                StatementKind::Define(definition)
                    if definition.kind == DefinitionKind::Class
                        && definition.bases.is_empty() && definition.decorators.is_empty()
                        && !definition.has_class_keywords() =>
                {
                    body_without_docstring(definition).iter().all(|statement| {
                        matches!(&statement.kind, StatementKind::Define(method)
                            if method.kind == DefinitionKind::Function && method.decorators.is_empty()
                                && method.annotations.is_empty() && method.defaults.is_empty())
                    })
                }
                _ => false,
            })
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

fn inert_blueprint_lookalike(definition: &Definition) -> bool {
    if definition.kind != DefinitionKind::Class
        || !definition.bases.is_empty()
        || !definition.decorators.is_empty()
        || definition.has_class_keywords()
    {
        return false;
    }
    let body = body_without_docstring(definition);
    body.len() == 3
        && ["__init__", "before_app_request", "register"]
            .into_iter()
            .all(|name| {
                body.iter().any(|statement| {
                    let StatementKind::Define(method) = &statement.kind else {
                        return false;
                    };
                    method.name.text == name
                        && method.annotations.is_empty()
                        && if name == "before_app_request" {
                            field_dispatch::plain_method(method, 2)
                                && returns_parameter(method, &method.parameters[1])
                        } else {
                            field_dispatch::plain_method(method, 3)
                                && body_without_docstring(method).is_empty()
                        }
                })
            })
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
            | ExpressionKind::Scalar(_)
    )
}

fn flask_receiver_decorator(definition: &Definition) -> Option<&Expression> {
    let [decorator] = definition.decorators.as_slice() else {
        return None;
    };
    let ExpressionKind::Member(base, method) = &decorator.kind else {
        return None;
    };
    (definition.kind != DefinitionKind::Class
        && definition.annotations.is_empty()
        && definition.defaults.is_empty()
        && (flask_callback_method(ObjectKind::FlaskApp, method) || flask_login_callback(method)))
    .then_some(base.as_ref())
}

fn initializer_flask_receiver(statement: &Statement) -> Option<&Expression> {
    match &statement.kind {
        StatementKind::Define(definition) => flask_receiver_decorator(definition),
        _ => None,
    }
}
