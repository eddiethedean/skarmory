//! Statically ordered writes that affect framework identity and discovery.

use super::*;

#[derive(Clone, Ord, PartialOrd, Eq, PartialEq)]
enum ReceiverKey {
    Class(SymbolId),
    Module(String),
    Signal(Object),
    FlaskLoginManager(Object),
    TyperApp(Object),
    FlaskApplication(Object),
    FlaskBase(ObjectKind),
    Fastapi(Object),
    FastapiBase,
    McpServer(Object),
    McpServerBase,
}

#[derive(Clone)]
pub(super) enum MemberWriteValue {
    Replacement(Value),
    Deleted,
    Ambiguous,
}

struct Write {
    file: ScopeId,
    at: Location,
    value: MemberWriteValue,
}

struct FlaskLoginEffectCall {
    callee: SymbolId,
    forwarded: Option<Invocation>,
}

#[derive(Default)]
pub(super) struct Mutations {
    members: BTreeMap<ReceiverKey, BTreeMap<String, Vec<Write>>>,
    modules: BTreeMap<String, Vec<Write>>,
    command_namespaces: BTreeSet<ScopeId>,
    module_globals: BTreeMap<ScopeId, BTreeMap<String, Vec<Write>>>,
    flask_global_setters: BTreeSet<ScopeId>,
    command_code: BTreeMap<SymbolId, Vec<Write>>,
    django_command_installers: BTreeSet<(ScopeId, [usize; 2])>,
    flask_login_code: BTreeMap<String, Vec<Write>>,
    flask_login_writers: BTreeMap<SymbolId, Location>,
    flask_login_effect_calls: BTreeMap<(ScopeId, [usize; 2]), FlaskLoginEffectCall>,
    form_table_fields: BTreeMap<SymbolId, BTreeMap<String, Vec<Write>>>,
    unmodeled_class_writes: BTreeSet<SymbolId>,
    framework_escapes: BTreeMap<ReceiverKey, Vec<Write>>,
    modeled: BTreeSet<(ScopeId, [usize; 2])>,
}

impl<'a> Graph<'a> {
    pub(super) fn index_framework_mutations(&mut self) {
        let track_framework_escapes = self.symbols.iter().any(|symbol| {
            matches!(symbol.binding, Binding::Import(import) if import.level == 0
                && matches!(import.module.split('.').next(), Some("flask" | "flask_login" | "mcp")))
        });
        self.index_command_namespaces();
        for file in 0..self.files.len() {
            self.index_module_mutations(
                file,
                self.scopes[file].body,
                false,
                track_framework_escapes,
            );
        }
    }

    fn index_command_namespaces(&mut self) {
        let reads = std::mem::take(&mut self.read);
        for target in 0..self.symbols.len() {
            if self.is_class(target)
                && self.framework_class_role(target, &mut BTreeSet::new(), 0)
                    == Some(FrameworkClassRole::DjangoCommand)
            {
                self.mutations
                    .command_namespaces
                    .insert(self.scopes[self.symbols[target].scope].file);
            }
        }
        for file in 0..self.files.len() {
            if let Some(command) = self.local(file, "Command", END)
                && let Some(binding) = self.probe_callable_binding(command)
                && self.is_class(binding.target)
                && self.framework_class_role(binding.target, &mut BTreeSet::new(), 0)
                    == Some(FrameworkClassRole::DjangoCommand)
            {
                self.mutations.command_namespaces.insert(file);
            }
        }
        self.read = reads;
    }

    pub(super) fn has_command_namespace(&self, scope: ScopeId) -> bool {
        self.mutations.command_namespaces.contains(&scope)
    }

    pub(super) fn is_current_command_module(
        &self,
        scope: ScopeId,
        mapping: &Value,
        key: &Expression,
        before: Location,
    ) -> bool {
        self.has_command_namespace(scope)
            && matches!(mapping, Value::Provider(provider) if provider == "sys.modules")
            && matches!(&key.kind, ExpressionKind::Name(name) if name == "__name__")
            && matches!(
                self.lexical_binding(scope, "__name__", before, true),
                LexicalBinding::Missing
            )
    }

    pub(super) fn module_global_value(
        &mut self,
        scope: ScopeId,
        at: Location,
        write: MemberWriteValue,
    ) -> Value {
        match write {
            MemberWriteValue::Replacement(value) => value,
            MemberWriteValue::Deleted => Value::MissingFrameworkMember,
            MemberWriteValue::Ambiguous => {
                self.scope_issue(
                    scope,
                    at,
                    "unresolved_attribute_write",
                    "module lookup depends on an unresolved namespace write",
                );
                Value::Unknown
            }
        }
    }

    fn index_module_mutations(
        &mut self,
        file: ScopeId,
        body: &[Statement],
        conditional: bool,
        track_framework_escapes: bool,
    ) {
        for statement in body {
            let at = statement.at;
            match &statement.kind {
                StatementKind::WriteAttribute {
                    receiver,
                    member,
                    value,
                } => {
                    self.index_member_write(file, at, receiver, member, value.as_ref(), conditional)
                }
                StatementKind::WriteModuleEntry {
                    mapping,
                    module: name,
                    value: Some(value),
                } if self.has_command_namespace(file)
                    && matches!(self.probe_identity(file, mapping, at), Value::CommandNamespace(owner) if owner == file) =>
                {
                    self.index_command_global(file, at, name, value, conditional);
                }
                StatementKind::WriteModuleEntry {
                    mapping:
                        Expression {
                            kind: ExpressionKind::Member(class, table),
                            ..
                        },
                    module: field,
                    value: Some(value),
                } if matches!(table.as_str(), "base_fields" | "declared_fields") => {
                    self.index_django_form_table_field(file, at, class, field, value, conditional);
                }
                StatementKind::WriteModuleEntry {
                    mapping:
                        Expression {
                            kind: ExpressionKind::Member(receiver, table),
                            ..
                        },
                    module: member,
                    value: Some(value),
                } if table == "__dict__" && flask_login_callback(member) => {
                    let kind = ObjectKind::FlaskLoginManager;
                    if let Value::Object(site, ObjectKind::FlaskLoginManager) =
                        self.probe_identity(file, receiver, at)
                        && matches!(
                            self.flask_member(site, kind, member, (file, at)),
                            Value::Method(..)
                        )
                    {
                        // An unmodified LoginManager callback is a non-data
                        // descriptor. A literal instance slot shadows it.
                        self.index_member_write(
                            file,
                            at,
                            receiver,
                            member,
                            Some(value),
                            conditional,
                        );
                    }
                }
                StatementKind::WriteModuleEntry {
                    mapping,
                    module,
                    value: Some(value),
                } if provider_module(module) => {
                    if matches!(self.probe_identity(file, mapping, at),
                        Value::Provider(provider) if provider == "sys.modules")
                        && let ExpressionKind::Call {
                            function,
                            arguments,
                            keywords,
                        } = &value.kind
                        && arguments.is_empty()
                        && keywords.is_empty()
                        && matches!(self.probe_identity(file, function, at),
                            Value::Provider(provider) if provider == "builtins.object")
                    {
                        self.mutations
                            .modules
                            .entry(module.clone())
                            .or_default()
                            .push(Write {
                                file,
                                at,
                                value: if conditional {
                                    MemberWriteValue::Ambiguous
                                } else {
                                    MemberWriteValue::Replacement(Value::MissingFrameworkMember)
                                },
                            });
                        if !conditional {
                            self.mutations.modeled.insert((file, position(at)));
                        }
                    }
                }
                StatementKind::Evaluate(expression)
                    if track_framework_escapes
                        && (self.index_flask_login_mock_start(
                            file,
                            at,
                            expression,
                            conditional,
                        ) || !conditional
                            && (self.index_flask_global_setter(file, at, expression)
                                || self.index_flask_login_effect_call(file, at, expression))) => {}
                StatementKind::Evaluate(
                    expression @ Expression {
                        kind:
                            ExpressionKind::Call {
                                function,
                                arguments,
                                keywords,
                            },
                        ..
                    },
                ) if !arguments.is_empty() || !keywords.is_empty() => {
                    if self.has_command_namespace(file) {
                        if self.index_django_command_installer(
                            file,
                            at,
                            function,
                            arguments,
                            keywords,
                            conditional,
                        ) {
                            continue;
                        }
                        self.index_opaque_command_escape(file, at, function, arguments, keywords);
                    }
                    if track_framework_escapes
                        && self.index_flask_globals_update(file, at, expression, conditional)
                    {
                        continue;
                    }
                    if keywords.is_empty()
                        && let [receiver, name, rest @ ..] = arguments.as_slice()
                        && let ExpressionKind::Text(member) = &name.kind
                        && let Value::Provider(provider) = self.probe_identity(file, function, at)
                        && matches!(
                            (provider.as_str(), rest.len()),
                            ("builtins.setattr", 1) | ("builtins.delattr", 0)
                        )
                    {
                        self.index_member_write(
                            file,
                            at,
                            receiver,
                            member,
                            rest.first(),
                            conditional,
                        );
                    } else if track_framework_escapes {
                        match self.probe_identity(file, function, at) {
                            Value::Unknown => {
                                self.index_framework_escape(file, at, arguments, keywords)
                            }
                            Value::Provider(provider) if !is_known_call(&provider, true) => {
                                self.index_framework_escape(file, at, arguments, keywords);
                            }
                            _ => {}
                        }
                    }
                }
                StatementKind::If {
                    test,
                    body,
                    otherwise,
                } => match self.indexed_bool(file, test, at, 0) {
                    Some(true) => self.index_module_mutations(
                        file,
                        body,
                        conditional,
                        track_framework_escapes,
                    ),
                    Some(false) => self.index_module_mutations(
                        file,
                        otherwise,
                        conditional,
                        track_framework_escapes,
                    ),
                    None => {
                        self.index_module_mutations(file, body, true, track_framework_escapes);
                        self.index_module_mutations(file, otherwise, true, track_framework_escapes);
                    }
                },
                StatementKind::Return(_) | StatementKind::Raise(_) => break,
                _ => {}
            }
        }
    }

    fn index_framework_escape(
        &mut self,
        file: ScopeId,
        at: Location,
        arguments: &[Expression],
        keywords: &[(String, Expression)],
    ) {
        let mut escaped = BTreeSet::new();
        let mut visited = BTreeSet::new();
        for expression in arguments
            .iter()
            .chain(keywords.iter().map(|(_, value)| value))
        {
            let value = self.probe_identity(file, expression, at);
            let read = std::mem::take(&mut self.read);
            self.framework_escape_keys(value, (file, at), &mut escaped, &mut visited, 0);
            self.read = read;
        }
        for receiver in escaped {
            self.mutations
                .framework_escapes
                .entry(receiver)
                .or_default()
                .push(Write {
                    file,
                    at,
                    value: MemberWriteValue::Ambiguous,
                });
        }
    }

    fn framework_escape_keys(
        &mut self,
        value: Value,
        access: (ScopeId, Location),
        escaped: &mut BTreeSet<ReceiverKey>,
        visited: &mut BTreeSet<SymbolId>,
        depth: usize,
    ) {
        match value {
            Value::Object(site, ObjectKind::FlaskLoginManager)
            | Value::Method(site, ObjectKind::FlaskLoginManager, _) => {
                escaped.insert(ReceiverKey::FlaskLoginManager(site));
            }
            Value::Object(site, ObjectKind::FlaskApp | ObjectKind::FlaskBlueprint)
            | Value::Method(site, ObjectKind::FlaskApp | ObjectKind::FlaskBlueprint, _) => {
                escaped.insert(ReceiverKey::FlaskApplication(site));
            }
            Value::McpServer(site) => {
                escaped.insert(ReceiverKey::McpServer(site));
            }
            Value::Provider(name) if mcp_tools::is_server_provider(&name) => {
                escaped.insert(ReceiverKey::McpServerBase);
            }
            Value::Provider(name) => {
                escaped.extend(flask::provider_kind(&name).map(ReceiverKey::FlaskBase));
            }
            Value::Sequence(values) | Value::Mapping(values) | Value::Alternatives(values) => {
                for value in values {
                    self.framework_escape_keys(value, access, escaped, visited, depth);
                }
            }
            Value::Callable(Invocation {
                target,
                receiver: Receiver::Ordinary,
                verified: false,
                ..
            }) if depth < MAX_BINDING_CHAIN && visited.insert(target) => {
                let file = self.symbols[target].scope;
                if file != self.scopes[file].file {
                    return;
                }
                let Binding::Definition(definition, _) = self.symbols[target].binding else {
                    return;
                };
                if definition.kind == DefinitionKind::Class {
                    return;
                }
                // A module function resolves its free names when invoked by the
                // opaque caller. Function defaults are evaluated at declaration.
                let (caller, at) = access;
                let before = if caller == file { at } else { END };
                let postponed = self.files[file].facts.postponed_annotations;
                for name in namespace_free_names(definition, postponed) {
                    let expression = Expression {
                        kind: ExpressionKind::Name(name.into()),
                        at: before,
                    };
                    let value = self.probe_identity(file, &expression, before);
                    self.framework_escape_keys(value, access, escaped, visited, depth + 1);
                }
                for default in &definition.defaults {
                    let value = self.probe_identity(file, default, definition.name.at);
                    self.framework_escape_keys(value, access, escaped, visited, depth + 1);
                }
            }
            _ => {}
        }
    }

    fn index_member_write(
        &mut self,
        file: ScopeId,
        at: Location,
        receiver: &Expression,
        member: &str,
        value: Option<&Expression>,
        conditional: bool,
    ) {
        if member == "__code__"
            && let Some(value) = value
            && self.index_flask_login_code(file, at, receiver, value, conditional)
        {
            return;
        }
        let receiver = self.probe_identity(file, receiver, at);
        if member == "__code__"
            && let Value::Callable(invocation) = receiver
            && !matches!(invocation.receiver, Receiver::Bound(_))
            && !invocation.implicit_descriptor_receiver
            && self.index_django_command_code(file, at, invocation.target, value, conditional)
        {
            return;
        }
        if matches!(receiver, Value::Module(owner) if owner == file)
            && self.has_command_namespace(file)
            && let Some(value) = value
            && self.index_command_global(file, at, member, value, conditional)
        {
            return;
        }
        let Some(receiver) = self.mutation_receiver(&receiver) else {
            return;
        };
        if !self.modeled_mutation_member(&receiver, member) {
            if let ReceiverKey::Class(class) = receiver {
                self.mutations.unmodeled_class_writes.insert(class);
            }
            return;
        }
        let deletes_inherited_method = value.is_none()
            && matches!(
                receiver,
                ReceiverKey::Fastapi(_)
                    | ReceiverKey::FlaskApplication(_)
                    | ReceiverKey::FlaskLoginManager(_)
            )
            && !matches!(
                self.member_write(&receiver, member, (file, at)),
                Some(MemberWriteValue::Replacement(_))
            );
        let login_replacement = match (&receiver, value) {
            (ReceiverKey::FlaskBase(ObjectKind::FlaskLoginManager), Some(value))
                if !conditional && flask_login_callback(member) =>
            {
                self.flask_login_identity_replacement(file, at, value)
            }
            _ => None,
        };
        let value = if let Some(replacement) = login_replacement {
            MemberWriteValue::Replacement(replacement)
        } else if conditional
            || deletes_inherited_method
            || matches!(
                receiver,
                ReceiverKey::FastapiBase | ReceiverKey::FlaskBase(_) | ReceiverKey::McpServerBase
            )
        {
            MemberWriteValue::Ambiguous
        } else if let Some(value) = value {
            let value = if matches!(value.kind, ExpressionKind::IdentityLambda)
                && matches!(
                    receiver,
                    ReceiverKey::FlaskApplication(_)
                        | ReceiverKey::FlaskLoginManager(_)
                        | ReceiverKey::Fastapi(_)
                ) {
                // A direct write to an instance stores the function without
                // descriptor binding to the instance on later reads.
                Value::IdentityFunction
            } else {
                self.probe_identity(file, value, at)
            };
            if value.contains_unknown() {
                MemberWriteValue::Ambiguous
            } else {
                MemberWriteValue::Replacement(value)
            }
        } else {
            MemberWriteValue::Deleted
        };
        // An ordered write to this resolved module slot has a bounded
        // destination even when its replacement value is opaque. Diagnose
        // later reads of that value without invalidating earlier captures.
        let ordered_receiver_write = !conditional
            && matches!(&receiver, ReceiverKey::Module(module)
                if is_django_receiver(&format!("{module}.{member}")));
        if ordered_receiver_write || !matches!(value, MemberWriteValue::Ambiguous) {
            self.mutations.modeled.insert((file, position(at)));
        }
        self.mutations
            .members
            .entry(receiver)
            .or_default()
            .entry(member.into())
            .or_default()
            .push(Write { file, at, value });
    }

    fn flask_login_identity_replacement(
        &mut self,
        file: ScopeId,
        at: Location,
        value: &Expression,
    ) -> Option<Value> {
        if !matches!(value.kind, ExpressionKind::Name(_)) {
            return None;
        }
        let Value::Callable(call) = self.probe_identity(file, value, at) else {
            return None;
        };
        let symbol = &self.symbols[call.target];
        let Binding::Definition(definition, _) = symbol.binding else {
            return None;
        };
        // Assigning a plain function to LoginManager's class dictionary
        // binds its first parameter when read through a manager instance.
        (!call.verified
            && matches!(call.receiver, Receiver::Ordinary)
            && !call.implicit_descriptor_receiver
            && symbol.scope == file
            && field_dispatch::plain_method(definition, 2)
            && returns_parameter(definition, &definition.parameters[1]))
        .then_some(Value::Callable(Invocation {
            implicit_descriptor_receiver: true,
            ..call
        }))
    }

    fn index_command_global(
        &mut self,
        file: ScopeId,
        at: Location,
        name: &str,
        value: &Expression,
        conditional: bool,
    ) -> bool {
        let Some(binding) = self.local(file, name, at) else {
            return false;
        };
        if self.probe_callable_binding(binding).is_none() || name.starts_with("__") {
            return false;
        }
        let replacement = self.probe_identity(file, value, at);
        self.index_module_global_value(file, at, name, replacement, conditional);
        true
    }

    fn index_module_global_value(
        &mut self,
        file: ScopeId,
        at: Location,
        name: &str,
        replacement: Value,
        conditional: bool,
    ) {
        let value = if conditional || replacement.contains_unknown() {
            MemberWriteValue::Ambiguous
        } else {
            self.mutations.modeled.insert((file, position(at)));
            MemberWriteValue::Replacement(replacement)
        };
        self.mutations
            .module_globals
            .entry(file)
            .or_default()
            .entry(name.into())
            .or_default()
            .push(Write { file, at, value });
    }

    fn index_flask_globals_update(
        &mut self,
        file: ScopeId,
        at: Location,
        expression: &Expression,
        conditional: bool,
    ) -> bool {
        let Some((globals, name, replacement)) = flask_globals_update(expression) else {
            return false;
        };
        let current = Expression {
            kind: ExpressionKind::Name(name.to_owned()),
            at,
        };
        if name.starts_with("__")
            || !matches!(self.probe_identity(file, globals, at), Value::Provider(provider)
                if provider == "builtins.globals")
            || !matches!(
                self.probe_identity(file, &current, at),
                Value::Object(_, ObjectKind::FlaskLoginManager)
            )
        {
            return false;
        }
        let Value::Instance(class) = self.probe_identity(file, replacement, at) else {
            return false;
        };
        if !self.plain_default_provider(class, (file, at), &BTreeSet::new()) {
            return false;
        }
        self.index_module_global_value(file, at, name, Value::Instance(class), conditional);
        true
    }

    fn index_flask_global_setter(
        &mut self,
        file: ScopeId,
        at: Location,
        expression: &Expression,
    ) -> bool {
        let ExpressionKind::Call {
            function,
            arguments,
            keywords,
        } = &expression.kind
        else {
            return false;
        };
        if !arguments.is_empty()
            || !keywords.is_empty()
            || !matches!(function.kind, ExpressionKind::Name(_))
        {
            return false;
        }
        let Value::Callable(invocation) = self.probe_identity(file, function, at) else {
            return false;
        };
        let symbol = &self.symbols[invocation.target];
        let Binding::Definition(setter, scope) = symbol.binding else {
            return false;
        };
        let [
            Statement {
                kind: StatementKind::SetGlobalName { name, value },
                ..
            },
        ] = setter.body.as_slice()
        else {
            return false;
        };
        let ExpressionKind::Name(replacement) = &value.kind else {
            return false;
        };
        if invocation.verified
            || !matches!(invocation.receiver, Receiver::Ordinary)
            || symbol.scope != file
            || setter.kind != DefinitionKind::Function
            || !setter.decorators.is_empty()
            || !setter.parameters.is_empty()
            || !setter.defaults.is_empty()
            || !setter.annotations.is_empty()
            || !self.scopes[file].names.get(replacement.as_str()).is_some_and(|ids| ids.len() == 1)
            || !self.scopes[file].body.windows(2).any(|pair| {
                matches!(&pair[0].kind, StatementKind::Define(definition)
                    if std::ptr::eq(definition, setter))
                    && position(pair[1].at) == position(at)
            })
            || self.scopes.iter().enumerate().any(|(other, row)| {
                other != scope && row.file == file
                    && matches!(row.body, [Statement { kind: StatementKind::SetGlobalName { name: other, .. }, .. }]
                        if other.text == *replacement || other.text == name.text)
            })
        {
            return false;
        }

        // The immediately invoked helper can only assign the same bounded
        // instance when called again. Earlier lexical reads see the old slot.
        let current = Expression {
            kind: ExpressionKind::Name(name.text.clone()),
            at,
        };
        if !matches!(self.probe_identity(file, &current, at),
            Value::Object(site, ObjectKind::FlaskLoginManager) if site.0 == file)
        {
            return false;
        }
        let Value::Instance(class) = self.probe_identity(file, value, at) else {
            return false;
        };
        if !self.plain_default_provider(class, (file, at), &BTreeSet::new()) {
            return false;
        }
        self.index_module_global_value(file, at, &name.text, Value::Instance(class), false);
        self.mutations.flask_global_setters.insert(scope);
        true
    }

    pub(super) fn modeled_flask_global_setter(&self, scope: ScopeId) -> bool {
        self.mutations.flask_global_setters.contains(&scope)
    }

    fn index_flask_login_effect_call(
        &mut self,
        file: ScopeId,
        at: Location,
        expression: &Expression,
    ) -> bool {
        let ExpressionKind::Call {
            function,
            arguments,
            keywords,
        } = &expression.kind
        else {
            return false;
        };
        if !keywords.is_empty() {
            return false;
        }
        let Some((callee, call_scope, function)) =
            self.flask_login_effect_function(file, at, function)
        else {
            return false;
        };
        let (writer, scope, definition, arguments) = if let (
            [parameter],
            [argument],
            [
                Statement {
                    kind:
                        StatementKind::Evaluate(Expression {
                            kind:
                                ExpressionKind::Call {
                                    function: forwarded,
                                    arguments: forwarded_args,
                                    keywords: forwarded_keywords,
                                },
                            ..
                        }),
                    ..
                },
            ],
        ) = (
            function.parameters.as_slice(),
            arguments.as_slice(),
            body_without_docstring(function),
        ) && matches!(&forwarded.kind, ExpressionKind::Name(name) if *name == parameter.text)
            && forwarded_args.is_empty()
            && forwarded_keywords.is_empty()
        {
            let Some((writer, scope, definition)) =
                self.flask_login_effect_function(file, at, argument)
            else {
                return false;
            };
            (writer, scope, definition, &[][..])
        } else {
            (callee, call_scope, function, arguments.as_slice())
        };
        let [
            Statement {
                at: write_at,
                kind:
                    StatementKind::WriteAttribute {
                        receiver,
                        member,
                        value: Some(value),
                    },
            },
        ] = body_without_docstring(definition)
        else {
            return false;
        };
        let ExpressionKind::Name(name) = &receiver.kind else {
            return false;
        };
        let receiver = match (definition.parameters.as_slice(), arguments) {
            ([], []) => receiver,
            ([parameter], [argument]) if parameter.text == *name => argument,
            _ => return false,
        };
        if !flask_login_callback(member)
            || !matches!(&value.kind, ExpressionKind::Name(replacement)
                if definition.parameters.iter().all(|parameter| parameter.text != *replacement))
        {
            return false;
        }
        let resolved = self.probe_identity(file, receiver, at);
        if !match resolved {
            Value::Provider(provider) if is_flask_login_constructor(&provider) => self
                .flask_login_identity_replacement(file, at, value)
                .is_some(),
            Value::Object(_, ObjectKind::FlaskLoginManager) => matches!(
                self.flask_login_effect_function(file, at, value),
                Some((_, _, replacement)) if replacement.parameters.len() == 1
                    && returns_parameter(replacement, &replacement.parameters[0])
            ),
            _ => false,
        } {
            return false;
        }

        self.index_member_write(file, at, receiver, member, Some(value), false);
        if !self.modeled_framework_write(file, at) {
            return false;
        }
        self.mutations.modeled.insert((scope, position(*write_at)));
        self.mutations
            .flask_login_writers
            .insert(writer.target, *write_at);
        self.mutations.flask_login_effect_calls.insert(
            (file, position(at)),
            FlaskLoginEffectCall {
                callee: callee.target,
                forwarded: (writer.target != callee.target).then_some(writer),
            },
        );
        true
    }

    fn flask_login_effect_function(
        &mut self,
        file: ScopeId,
        at: Location,
        expression: &Expression,
    ) -> Option<(Invocation, ScopeId, &'a Definition)> {
        if !matches!(expression.kind, ExpressionKind::Name(_)) {
            return None;
        }
        let Value::Callable(invocation) = self.probe_identity(file, expression, at) else {
            return None;
        };
        let symbol = &self.symbols[invocation.target];
        let Binding::Definition(function, scope) = symbol.binding else {
            return None;
        };
        (symbol.scope == file
            && !invocation.verified
            && matches!(invocation.receiver, Receiver::Ordinary)
            && !invocation.implicit_descriptor_receiver
            && function.annotations.is_empty()
            && field_dispatch::plain_method(function, function.parameters.len()))
        .then_some((invocation, scope, function))
    }

    pub(super) fn flask_login_mutator_dispatch(
        &mut self,
        callee: SymbolId,
        call: CallArguments<'_>,
    ) -> Option<Invocation> {
        let modeled = self
            .mutations
            .flask_login_effect_calls
            .get(&(call.scope, position(call.at)))
            .filter(|effect| effect.callee == callee);
        let forwarded = modeled.and_then(|effect| effect.forwarded);
        if modeled.is_none() && self.mutations.flask_login_writers.contains_key(&callee) {
            self.scope_issue(
                call.scope,
                call.at,
                "unresolved_attribute_write",
                "Flask callback mutation is outside the bounded initializer",
            );
        }
        forwarded
    }

    pub(super) fn report_escaped_flask_login_mutators(&mut self, reach: &Reachability) {
        self.issues.extend(
            self.mutations
                .flask_login_writers
                .iter()
                .filter(|&(&target, _)| {
                    reach
                        .evidence
                        .range((target, "", "")..)
                        .take_while(|(id, _, _)| *id == target)
                        .any(|(_, kind, _)| !matches!(*kind, "direct_call" | "type_reference"))
                })
                .map(|(&target, &at)| {
                    Issue::new(
                        "unresolved_attribute_write",
                        &self.files[self.symbols[target].scope].path,
                        "Flask callback mutation can escape the bounded initializer",
                    )
                    .at(at)
                }),
        );
    }

    pub(super) fn walk_flask_globals_update(
        &mut self,
        scope: ScopeId,
        expression: &Expression,
        at: Location,
    ) -> bool {
        if self.modeled_framework_write(scope, at)
            && let Some((_, _, replacement)) = flask_globals_update(expression)
        {
            self.walk(scope, replacement, at);
            return true;
        }
        false
    }

    fn index_flask_login_mock_start(
        &mut self,
        file: ScopeId,
        at: Location,
        expression: &Expression,
        conditional: bool,
    ) -> bool {
        let Some((constructor, arguments)) = mock_start(expression) else {
            return false;
        };
        let Value::Provider(provider) = self.probe_identity(file, constructor, at) else {
            return false;
        };
        let dotted_receiver;
        let (receiver, member, replacement) = match (provider.as_str(), arguments) {
            (
                "unittest.mock.patch",
                [
                    Expression {
                        kind: ExpressionKind::Text(target),
                        ..
                    },
                    value,
                ],
            ) => {
                let Some(rest) = target
                    .strip_prefix(self.files[file].module.as_str())
                    .and_then(|rest| rest.strip_prefix('.'))
                else {
                    return false;
                };
                let Some((name, member)) = rest.split_once('.') else {
                    return false;
                };
                dotted_receiver = Expression {
                    kind: ExpressionKind::Name(name.to_owned()),
                    at,
                };
                (&dotted_receiver, member, value)
            }
            (
                "unittest.mock.patch.object",
                [
                    receiver,
                    Expression {
                        kind: ExpressionKind::Text(member),
                        ..
                    },
                    value,
                ],
            ) => (receiver, member.as_str(), value),
            _ => return false,
        };
        if !flask_login_callback(member) {
            return false;
        }
        let kind = ObjectKind::FlaskLoginManager;
        let known_target = match self.probe_identity(file, receiver, at) {
            Value::Object(site, ObjectKind::FlaskLoginManager) => matches!(
                self.flask_member(site, kind, member, (file, at)),
                Value::Method(..)
            ),
            Value::Provider(name)
                if provider == "unittest.mock.patch.object"
                    && is_flask_login_constructor(&name) =>
            {
                let class = ReceiverKey::FlaskBase(kind);
                !self.framework_receiver_has_escaped(&class, None, (file, at))
                    && self.member_write(&class, member, (file, at)).is_none()
            }
            _ => false,
        };
        if known_target {
            self.index_member_write(file, at, receiver, member, Some(replacement), conditional);
        }
        known_target
    }

    pub(super) fn walk_flask_login_mock_start(
        &mut self,
        scope: ScopeId,
        expression: &Expression,
        at: Location,
    ) -> bool {
        if self.modeled_framework_write(scope, at)
            && let Some((constructor, arguments)) = mock_start(expression)
        {
            self.walk(scope, constructor, at);
            for argument in arguments {
                self.walk(scope, argument, at);
            }
            return true;
        }
        false
    }

    fn index_opaque_command_escape(
        &mut self,
        file: ScopeId,
        at: Location,
        function: &Expression,
        arguments: &[Expression],
        keywords: &[(String, Expression)],
    ) {
        let opaque = match self.probe_identity(file, function, at) {
            Value::Unknown => true,
            Value::Provider(provider) => !is_known_call(&provider, true),
            _ => false,
        };
        if !opaque {
            return;
        }
        for argument in arguments
            .iter()
            .chain(keywords.iter().map(|(_, value)| value))
        {
            let Value::Callable(target) = self.probe_identity(file, argument, at) else {
                continue;
            };
            if target.verified {
                continue;
            }
            let write = Write {
                file,
                at,
                value: MemberWriteValue::Ambiguous,
            };
            if self.is_django_command_class(target.target) {
                self.mutations
                    .members
                    .entry(ReceiverKey::Class(target.target))
                    .or_default()
                    .entry("handle".into())
                    .or_default()
                    .push(write);
            } else if self.symbols[target.target].name.text == "handle"
                && let Some(owner) = self.scopes[self.symbols[target.target].scope].owner
                && self.is_django_command_class(owner)
            {
                self.mutations
                    .command_code
                    .entry(target.target)
                    .or_default()
                    .push(write);
            }
        }
    }

    fn index_django_command_installer(
        &mut self,
        file: ScopeId,
        at: Location,
        function: &Expression,
        arguments: &[Expression],
        keywords: &[(String, Expression)],
        conditional: bool,
    ) -> bool {
        let ([supplied], []) = (arguments, keywords) else {
            return false;
        };
        let Value::Callable(callee) = self.probe_identity(file, function, at) else {
            return false;
        };
        let Binding::Definition(definition, body) = self.symbols[callee.target].binding else {
            return false;
        };
        if callee.verified
            || !matches!(callee.receiver, Receiver::Ordinary)
            || callee.implicit_descriptor_receiver
            || !field_dispatch::plain_method(definition, 1)
            || !definition.annotations.is_empty()
            || self.symbols[callee.target].scope != self.scopes[body].file
        {
            return false;
        }
        let [statement] = body_without_docstring(definition) else {
            return false;
        };
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
            return false;
        };
        let ([receiver, field, value], []) = (arguments.as_slice(), keywords.as_slice()) else {
            return false;
        };
        let ExpressionKind::Text(field) = &field.kind else {
            return false;
        };
        if !matches!(&receiver.kind, ExpressionKind::Name(parameter) if parameter == &definition.parameters[0].text)
            || !matches!(self.probe_identity(body, function, statement.at),
                Value::Provider(provider) if provider == "builtins.setattr")
        {
            return false;
        }

        let Value::Callable(target) = self.probe_identity(file, supplied, at) else {
            return false;
        };
        if target.verified || target.implicit_descriptor_receiver {
            return false;
        }
        if field == "__code__" {
            let ExpressionKind::Member(code, attribute) = &value.kind else {
                return false;
            };
            let symbol = &self.symbols[target.target];
            if symbol.name.text != "handle"
                || matches!(target.receiver, Receiver::Bound(_))
                || attribute != "__code__"
            {
                return false;
            }
            let Some(owner) = self.scopes[symbol.scope].owner else {
                return false;
            };
            if !self.plain_installer_command(owner) {
                return false;
            }
            let Value::Callable(replacement) = self.probe_identity(body, code, statement.at) else {
                return false;
            };
            if replacement.verified
                || !matches!(replacement.receiver, Receiver::Ordinary)
                || replacement.implicit_descriptor_receiver
                || !self.closed_django_command_code(
                    target.target,
                    replacement.target,
                    self.scopes[body].file,
                )
            {
                return false;
            }
            // The code retains the old function's globals and needs no
            // calling edge in the installer.
            self.edge(body, replacement.target, "code_reference", "possible");
            self.mutations
                .command_code
                .entry(target.target)
                .or_default()
                .push(Write {
                    file,
                    at,
                    value: if conditional {
                        MemberWriteValue::Ambiguous
                    } else {
                        MemberWriteValue::Replacement(Value::FrameworkCode(replacement.target))
                    },
                });
            self.mutations
                .django_command_installers
                .insert((file, position(at)));
            return true;
        }
        if field != "handle"
            || !matches!(target.receiver, Receiver::Ordinary)
            || !self.plain_installer_command(target.target)
        {
            return false;
        }
        let Value::Callable(replacement) = self.probe_identity(body, value, statement.at) else {
            return false;
        };
        if replacement.verified
            || !matches!(replacement.receiver, Receiver::Ordinary)
            || replacement.implicit_descriptor_receiver
            || !self.bounded_framework_function(replacement.target)
        {
            return false;
        }
        self.mutations
            .members
            .entry(ReceiverKey::Class(target.target))
            .or_default()
            .entry(field.clone())
            .or_default()
            .push(Write {
                file,
                at,
                value: if conditional {
                    MemberWriteValue::Ambiguous
                } else {
                    MemberWriteValue::Replacement(Value::Callable(replacement))
                },
            });
        self.mutations
            .django_command_installers
            .insert((file, position(at)));
        true
    }

    fn plain_installer_command(&mut self, class: SymbolId) -> bool {
        let scope = self.symbols[class].scope;
        let Binding::Definition(definition, _) = self.symbols[class].binding else {
            return false;
        };
        let [base] = definition.bases.as_slice() else {
            return false;
        };
        self.is_django_command_class(class)
            && !definition.has_class_keywords()
            && definition.decorators.is_empty()
            && matches!(self.probe_identity(scope, base, definition.name.at), Value::Provider(provider)
                if framework_class_provider(&provider) == Some(FrameworkClassRole::DjangoCommand))
    }

    pub(super) fn modeled_django_command_installer(&self, scope: ScopeId, at: Location) -> bool {
        self.mutations
            .django_command_installers
            .contains(&(scope, position(at)))
    }

    fn index_django_command_code(
        &mut self,
        file: ScopeId,
        at: Location,
        target: SymbolId,
        value: Option<&Expression>,
        conditional: bool,
    ) -> bool {
        let symbol = &self.symbols[target];
        let Some(class) = self.scopes[symbol.scope].owner else {
            return false;
        };
        if symbol.name.text != "handle"
            || !self.is_class(class)
            || self.framework_class_role(class, &mut BTreeSet::new(), 0)
                != Some(FrameworkClassRole::DjangoCommand)
        {
            return false;
        }
        let replacement = value.and_then(|value| {
            let Value::FrameworkCode(replacement) = self.probe_identity(file, value, at) else {
                return None;
            };
            self.closed_django_command_code(target, replacement, file)
                .then_some(replacement)
        });
        let value = match replacement {
            Some(replacement) if !conditional => {
                self.mutations.modeled.insert((file, position(at)));
                MemberWriteValue::Replacement(Value::FrameworkCode(replacement))
            }
            _ => MemberWriteValue::Ambiguous,
        };
        self.mutations
            .command_code
            .entry(target)
            .or_default()
            .push(Write { file, at, value });
        true
    }

    fn closed_django_command_code(
        &self,
        target: SymbolId,
        replacement: SymbolId,
        file: ScopeId,
    ) -> bool {
        let Binding::Definition(original, _) = self.symbols[target].binding else {
            return false;
        };
        let replacement = &self.symbols[replacement];
        let Binding::Definition(code, _) = replacement.binding else {
            return false;
        };
        let original_class = self.scopes[self.symbols[target].scope]
            .owner
            .expect("command method has a class");
        let original_file = self.scopes[self.symbols[original_class].scope].file;
        // A module function's inert code has no free variables. A method in
        // a module class can still close over Python's implicit __class__ cell.
        replacement.scope == file
            && self.symbols[original_class].scope == original_file
            && original.kind == DefinitionKind::Function
            && code.kind == DefinitionKind::Function
            && !original.is_generator
            && !code.is_generator
            && original.decorators.is_empty()
            && code.decorators.is_empty()
            && !namespace_free_names(
                original,
                self.files[original_file].facts.postponed_annotations,
            )
            .iter()
            .any(|name| matches!(*name, "__class__" | "super"))
            && original
                .parameters
                .iter()
                .map(|p| &p.text)
                .eq(code.parameters.iter().map(|p| &p.text))
            && original.positional_parameter_count == code.positional_parameter_count
            && original.positional_only_parameter_count == code.positional_only_parameter_count
            && original.has_variadic_positional_parameter == code.has_variadic_positional_parameter
            && original.has_variadic_keyword_parameter == code.has_variadic_keyword_parameter
            && matches!(
                body_without_docstring(code),
                [Statement {
                    kind: StatementKind::Return(None)
                        | StatementKind::Return(Some(Expression {
                            kind: ExpressionKind::None,
                            ..
                        })),
                    ..
                }]
            )
    }

    pub(super) fn django_command_code_write(
        &self,
        target: SymbolId,
        access: (ScopeId, Location),
    ) -> Option<MemberWriteValue> {
        let original = self.scopes[self.symbols[target].scope].file;
        self.write_value(
            self.mutations.command_code.get(&target)?,
            Some(original),
            access,
        )
    }

    pub(super) fn django_command_has_code_write(&self, target: SymbolId) -> bool {
        self.mutations.command_code.contains_key(&target)
    }

    fn index_django_form_table_field(
        &mut self,
        file: ScopeId,
        at: Location,
        class: &Expression,
        field: &str,
        value: &Expression,
        conditional: bool,
    ) {
        let Value::Callable(invocation) = self.probe_identity(file, class, at) else {
            return;
        };
        let class = invocation.target;
        if !self.is_class(class) || !self.django_plain_form_tables(class, (file, at)) {
            return;
        }
        let replacement = self.probe_identity(file, value, at);
        let value = if conditional
            || !matches!(
                replacement,
                Value::DjangoField(django_forms::Field::Form { widget: None })
            ) {
            MemberWriteValue::Ambiguous
        } else {
            self.mutations.modeled.insert((file, position(at)));
            MemberWriteValue::Replacement(replacement)
        };
        self.mutations
            .form_table_fields
            .entry(class)
            .or_default()
            .entry(field.into())
            .or_default()
            .push(Write { file, at, value });
    }

    pub(super) fn has_django_form_table_edits(&self, class: SymbolId) -> bool {
        self.mutations.form_table_fields.contains_key(&class)
    }

    pub(super) fn django_form_table_fields_at(
        &self,
        class: SymbolId,
        access: (ScopeId, Location),
    ) -> Option<Vec<String>> {
        let original = self.scopes[self.symbols[class].scope].file;
        let mut fields = Vec::new();
        if let Some(table) = self.mutations.form_table_fields.get(&class) {
            for (name, writes) in table {
                match self.write_value(writes, Some(original), access) {
                    Some(MemberWriteValue::Replacement(_)) => fields.push(name.clone()),
                    None => {}
                    _ => return None,
                }
            }
        }
        Some(fields)
    }

    pub(super) fn module_global_write(
        &self,
        file: ScopeId,
        name: &str,
        before: Location,
        access: (ScopeId, Location),
    ) -> Option<MemberWriteValue> {
        let writes = self.mutations.module_globals.get(&file)?.get(name)?;
        let before = if access.0 == file && position(access.1) < position(before) {
            access.1
        } else {
            before
        };
        let end = writes.partition_point(|write| position(write.at) < position(before));
        let binding = self.local(file, name, before)?;
        let start = writes
            .partition_point(|write| position(write.at) < position(self.symbols[binding].name.at));
        let writes = writes.get(start..end)?;
        if !writes.is_empty() && self.binding_depends_on_branch(binding, before) {
            return Some(MemberWriteValue::Ambiguous);
        }
        self.write_value(writes, Some(file), access)
    }

    fn modeled_mutation_member(&mut self, receiver: &ReceiverKey, member: &str) -> bool {
        match receiver {
            ReceiverKey::Class(class) => {
                if self.fastapi_direct_subclass_scope(*class, 0).is_some() {
                    return fastapi::is_supported_app_method(member)
                        || matches!(
                            member,
                            "__new__" | "__init__" | "__getattr__" | "__getattribute__"
                        );
                }
                let role = self.framework_class_role(*class, &mut BTreeSet::new(), 0);
                role.is_some_and(|role| {
                    role.is_unittest()
                        && (member.starts_with("test")
                            || matches!(
                                member,
                                "runTest"
                                    | "setUp"
                                    | "tearDown"
                                    | "setUpClass"
                                    | "tearDownClass"
                                    | "asyncSetUp"
                                    | "asyncTearDown"
                                    | "setUpTestData"
                                    | "_pre_setup"
                                    | "_post_teardown"
                            ))
                        || role == FrameworkClassRole::DjangoAppConfig && member == "ready"
                        || role == FrameworkClassRole::DjangoView
                            && (matches!(
                                member,
                                "__init__"
                                    | "__new__"
                                    | "__getattribute__"
                                    | "__getattr__"
                                    | "setup"
                                    | "dispatch"
                                    | "http_method_names"
                                    | "http_method_not_allowed"
                                    | "request"
                            ) || self
                                .class_lineage(*class)
                                .0
                                .into_iter()
                                .any(|body| self.local(body, member, END).is_some()))
                        || role == FrameworkClassRole::DjangoCommand
                            && (matches!(
                                member,
                                "__new__"
                                    | "__init__"
                                    | "__getattribute__"
                                    | "run_from_argv"
                                    | "print_help"
                                    | "create_parser"
                                    | "add_arguments"
                                    | "execute"
                                    | "handle"
                            ) || self
                                .class_lineage(*class)
                                .0
                                .into_iter()
                                .any(|body| self.local(body, member, END).is_some()))
                        || role == FrameworkClassRole::DjangoSignal && member == "connect"
                        || role == FrameworkClassRole::ClickCommand && member == "invoke"
                        || role == FrameworkClassRole::DrfViewSet && drf::is_viewset_action(member)
                        || role == FrameworkClassRole::DrfSerializer
                            && drf::is_serializer_mutation(member)
                        || matches!(
                            role,
                            FrameworkClassRole::DjangoForm | FrameworkClassRole::DjangoModelForm
                        ) && matches!(member, "base_fields" | "declared_fields")
                })
            }
            ReceiverKey::Module(module) => {
                if let Some(&file) = self.modules.get(module.as_str()) {
                    return self
                        .local(file, member, END)
                        .and_then(|binding| self.probe_callable_binding(binding))
                        .filter(|binding| self.is_class(binding.target))
                        .is_some_and(|binding| {
                            self.framework_class_role(binding.target, &mut BTreeSet::new(), 0)
                                .is_some_and(|role| {
                                    role.is_unittest()
                                        || role == FrameworkClassRole::DjangoAppConfig
                                        || role == FrameworkClassRole::DjangoCommand
                                            && member == "Command"
                                })
                        });
                }
                let qualified = format!("{module}.{member}");
                framework_class_provider(&qualified).is_some_and(|role| {
                    role.is_unittest()
                        || matches!(
                            role,
                            FrameworkClassRole::DjangoForm
                                | FrameworkClassRole::DjangoModelForm
                                | FrameworkClassRole::DjangoFormField
                        )
                }) || matches!(
                    qualified.as_str(),
                    "flask.Flask"
                        | "flask.app.Flask"
                        | "flask.Blueprint"
                        | "flask.blueprints.Blueprint"
                        | "celery.Celery"
                        | "celery.signals.setup_logging"
                        | celery::LOGGING_CONNECT
                ) || is_flask_login_constructor(&qualified)
                    || gin::tracks_provider_member(&qualified)
                    || click_command_provider(&qualified)
                    || mcp_tools::is_server_provider(&qualified)
                    || is_typer_constructor(&qualified)
                    || is_django_receiver(&qualified)
                    || is_django_signal_constructor(&qualified)
                    || is_django_runpython(&qualified)
                    || django_template_paths::is_pathlib_member(module, member)
                    || matches!(
                        qualified.as_str(),
                        logging_config::DICT_CONFIG | "logging.config.dictConfigClass"
                    )
                    || matches!(
                        qualified.as_str(),
                        "django.urls" | "django.utils.autoreload.run_with_reloader"
                    )
                    || member == "connect" && is_django_signal(module)
            }
            ReceiverKey::Signal(_) => member == "connect",
            ReceiverKey::McpServer(_) => {
                matches!(member, "tool" | "add_tool" | "_tool_manager" | "__class__")
            }
            ReceiverKey::McpServerBase => matches!(
                member,
                "tool" | "add_tool" | "__new__" | "__init__" | "__getattr__" | "__getattribute__"
            ),
            ReceiverKey::TyperApp(_) => matches!(member, "callback" | "__class__"),
            ReceiverKey::FlaskLoginManager(_) => {
                flask_login_callback(member)
                    || matches!(member, "__class__" | "config" | "login_view")
            }
            ReceiverKey::FlaskApplication(_) => {
                matches!(member, "__class__" | "config" | "register_blueprint")
                    || [ObjectKind::FlaskApp, ObjectKind::FlaskBlueprint]
                        .into_iter()
                        .any(|kind| {
                            is_route(kind, member, true) || is_imperative_registration(kind, member)
                        })
            }
            ReceiverKey::FlaskBase(kind) => {
                is_route(*kind, member, true)
                    || is_imperative_registration(*kind, member)
                    || matches!(
                        member,
                        "__new__" | "__init__" | "__getattr__" | "__getattribute__"
                    )
                    || matches!(kind, ObjectKind::FlaskApp | ObjectKind::FlaskBlueprint)
                        && member == "register_blueprint"
                    || *kind == ObjectKind::FlaskApp
                        && matches!(member, "config" | "config_class" | "make_config")
                    || *kind == ObjectKind::FlaskLoginManager && member == "config"
            }
            ReceiverKey::Fastapi(_) => {
                is_route(ObjectKind::App, member, true)
                    || is_imperative_registration(ObjectKind::App, member)
                    || matches!(
                        member,
                        "include_router" | "mount" | "middleware" | "add_middleware"
                    )
            }
            ReceiverKey::FastapiBase => {
                fastapi::is_supported_app_method(member)
                    || matches!(
                        member,
                        "__new__" | "__init__" | "__getattr__" | "__getattribute__"
                    )
            }
        }
    }

    fn mutation_receiver(&self, receiver: &Value) -> Option<ReceiverKey> {
        match receiver {
            Value::Callable(invocation) if self.is_class(invocation.target) => {
                Some(ReceiverKey::Class(invocation.target))
            }
            Value::Module(file) => Some(ReceiverKey::Module(self.files[*file].module.clone())),
            Value::Provider(provider) if fastapi::is_app_constructor(provider) => {
                Some(ReceiverKey::FastapiBase)
            }
            Value::McpServer(site) => Some(ReceiverKey::McpServer(*site)),
            Value::Provider(provider) if mcp_tools::is_server_provider(provider) => {
                Some(ReceiverKey::McpServerBase)
            }
            Value::Provider(provider) if flask::provider_kind(provider).is_some() => {
                flask::provider_kind(provider).map(ReceiverKey::FlaskBase)
            }
            Value::Provider(module)
                if provider_module(module)
                    || is_django_signal(module)
                    || module == "celery.signals.setup_logging" =>
            {
                Some(ReceiverKey::Module(module.clone()))
            }
            Value::DjangoSignal(site) => Some(ReceiverKey::Signal(*site)),
            Value::Object(site, ObjectKind::TyperApp) => Some(ReceiverKey::TyperApp(*site)),
            Value::Object(site, ObjectKind::FlaskLoginManager) => {
                Some(ReceiverKey::FlaskLoginManager(*site))
            }
            Value::Object(site, ObjectKind::FlaskApp | ObjectKind::FlaskBlueprint) => {
                Some(ReceiverKey::FlaskApplication(*site))
            }
            Value::Object(
                site,
                ObjectKind::App
                | ObjectKind::Router
                | ObjectKind::StarletteApp
                | ObjectKind::AiohttpRouteTable,
            )
            | Value::FastapiSubclassApp(site, _) => Some(ReceiverKey::Fastapi(*site)),
            _ => None,
        }
    }

    pub(super) fn modeled_framework_write(&self, scope: ScopeId, at: Location) -> bool {
        self.mutations.modeled.contains(&(scope, position(at)))
    }

    pub(super) fn record_unmodeled_class_write(
        &mut self,
        scope: ScopeId,
        receiver: &Expression,
        before: Location,
    ) {
        if let Value::Callable(invocation) = self.probe_identity(scope, receiver, before)
            && self.is_class(invocation.target)
        {
            self.mutations
                .unmodeled_class_writes
                .insert(invocation.target);
        }
    }

    pub(super) fn has_unmodeled_class_write(&self, class: SymbolId) -> bool {
        self.mutations.unmodeled_class_writes.contains(&class)
    }

    pub(super) fn class_discovery_write(
        &self,
        class: SymbolId,
        name: &str,
        reader: ScopeId,
    ) -> Option<MemberWriteValue> {
        self.member_write(&ReceiverKey::Class(class), name, (reader, END))
    }

    pub(super) fn class_discovery_writes(
        &self,
        class: SymbolId,
        reader: ScopeId,
    ) -> BTreeMap<String, MemberWriteValue> {
        let receiver = ReceiverKey::Class(class);
        self.mutations
            .members
            .get(&receiver)
            .into_iter()
            .flat_map(|members| members.keys())
            .filter_map(|name| {
                self.class_discovery_write(class, name, reader)
                    .map(|write| (name.clone(), write))
            })
            .collect()
    }

    pub(super) fn class_member_write(
        &self,
        class: SymbolId,
        name: &str,
        access: (ScopeId, Location),
    ) -> Option<MemberWriteValue> {
        self.member_write(&ReceiverKey::Class(class), name, access)
    }

    pub(super) fn module_member_write(
        &self,
        module: &str,
        name: &str,
        access: (ScopeId, Location),
    ) -> Option<MemberWriteValue> {
        self.member_write(&ReceiverKey::Module(module.into()), name, access)
    }

    pub(super) fn typer_callback_member(&self, site: Object, access: (ScopeId, Location)) -> Value {
        let receiver = ReceiverKey::TyperApp(site);
        if self.member_write(&receiver, "__class__", access).is_some() {
            return Value::Unknown;
        }
        match self.member_write(&receiver, "callback", access) {
            Some(MemberWriteValue::Replacement(value)) => value,
            Some(MemberWriteValue::Ambiguous) => Value::Unknown,
            Some(MemberWriteValue::Deleted) | None => {
                Value::Method(site, ObjectKind::TyperApp, "callback".into())
            }
        }
    }

    pub(super) fn flask_member(
        &mut self,
        site: Object,
        kind: ObjectKind,
        name: &str,
        access: (ScopeId, Location),
    ) -> Value {
        let receiver = if matches!(kind, ObjectKind::FlaskLoginManager) {
            ReceiverKey::FlaskLoginManager(site)
        } else {
            ReceiverKey::FlaskApplication(site)
        };
        let class = ReceiverKey::FlaskBase(kind);
        if self.framework_receiver_has_escaped(&receiver, Some(site.0), access)
            || self.framework_receiver_has_escaped(&class, None, access)
            || ["__getattr__", "__getattribute__"]
                .iter()
                .any(|member| self.member_write(&class, member, access).is_some())
        {
            return Value::Unknown;
        }
        if let Some(class_write) = self.member_write(&receiver, "__class__", access) {
            if matches!(kind, ObjectKind::FlaskLoginManager)
                && flask_login_callback(name)
                && self.member_write(&receiver, name, access).is_none()
                && let MemberWriteValue::Replacement(Value::Callable(replacement)) = class_write
                && !replacement.verified
                && matches!(replacement.receiver, Receiver::Ordinary)
                && !replacement.implicit_descriptor_receiver
                && self.closed_flask_login_layout(replacement.target, access)
            {
                return self.member(Value::Instance(replacement.target), name, access, 0);
            }
            return Value::Unknown;
        }
        match self.member_write(&receiver, name, access) {
            Some(MemberWriteValue::Replacement(value)) => value,
            Some(MemberWriteValue::Ambiguous) => Value::Unknown,
            Some(MemberWriteValue::Deleted) | None => {
                // An imported manager can only expose this class method after
                // the module that created the instance has initialized.
                let class_write = if matches!(kind, ObjectKind::FlaskLoginManager)
                    && flask_login_callback(name)
                {
                    self.mutations
                        .members
                        .get(&class)
                        .and_then(|members| members.get(name))
                        .and_then(|writes| self.write_value(writes, Some(site.0), access))
                } else {
                    self.member_write(&class, name, access)
                };
                match class_write {
                    Some(MemberWriteValue::Replacement(value)) => value,
                    Some(_) => Value::Unknown,
                    None if kind == ObjectKind::FlaskApp
                        && name == "config"
                        && ["config_class", "make_config"].iter().all(|name| {
                            self.member_write(&class, name, access).is_none()
                                && self.member_write(&receiver, name, access).is_none()
                        }) =>
                    {
                        Value::FlaskConfig
                    }
                    None => Value::Method(site, kind, name.into()),
                }
            }
        }
    }

    fn closed_flask_login_layout(&mut self, class: SymbolId, access: (ScopeId, Location)) -> bool {
        // Flask-Login's plain Python manager has the ordinary object layout.
        // Only a class with that same layout can replace it here.
        matches!(self.symbols[class].binding, Binding::Definition(definition, _)
            if definition.kind == DefinitionKind::Class && definition.bases.is_empty())
            && self.plain_default_provider(class, access, &BTreeSet::new())
            && ["__slots__", "__dict__", "__weakref__"]
                .iter()
                .all(|member| {
                    self.class_member_at(class, member, 0, Some(access))
                        .is_none()
                })
    }

    pub(super) fn flask_login_allocation(
        &mut self,
        scope: ScopeId,
        before: Location,
    ) -> Option<(Invocation, SymbolId)> {
        let kind = ObjectKind::FlaskLoginManager;
        let class = ReceiverKey::FlaskBase(kind);
        let access = (scope, before);
        if scope != self.scopes[scope].file
            || self.framework_receiver_has_escaped(&class, None, access)
            || self.member_write(&class, "__init__", access).is_some()
        {
            return None;
        }
        let MemberWriteValue::Replacement(Value::Callable(allocator)) =
            self.member_write(&class, "__new__", access)?
        else {
            return None;
        };
        if self.symbols[allocator.target].scope != scope {
            return None;
        }
        // The allocator reads this global when called, not when it is
        // assigned to __new__. Resolve the class at that execution point.
        let name = self.flask_login_allocator_class(allocator)?.to_owned();
        if self.scopes[scope].names.get(name.as_str())?.len() != 1 {
            return None;
        }
        let expression = Expression {
            kind: ExpressionKind::Name(name),
            at: before,
        };
        let Value::Callable(class) = self.probe_identity(scope, &expression, before) else {
            return None;
        };
        (!class.verified
            && matches!(class.receiver, Receiver::Ordinary)
            && !class.implicit_descriptor_receiver
            && self.closed_flask_login_layout(class.target, access))
        .then_some((allocator, class.target))
    }

    pub(super) fn flask_constructor_changed(
        &self,
        kind: ObjectKind,
        access: (ScopeId, Location),
    ) -> bool {
        let class = ReceiverKey::FlaskBase(kind);
        self.framework_receiver_has_escaped(&class, None, access)
            || ["__new__", "__init__"]
                .iter()
                .any(|member| self.member_write(&class, member, access).is_some())
    }

    fn framework_receiver_has_escaped(
        &self,
        receiver: &ReceiverKey,
        origin: Option<ScopeId>,
        access: (ScopeId, Location),
    ) -> bool {
        self.mutations
            .framework_escapes
            .get(receiver)
            .is_some_and(|writes| self.write_value(writes, origin, access).is_some())
    }

    pub(super) fn mcp_tool_available(&self, site: Object, access: (ScopeId, Location)) -> bool {
        let receiver = ReceiverKey::McpServer(site);
        let base = ReceiverKey::McpServerBase;
        !self.framework_receiver_has_escaped(&receiver, Some(site.0), access)
            && !self.framework_receiver_has_escaped(&base, None, access)
            && ["tool", "add_tool", "_tool_manager", "__class__"]
                .iter()
                .all(|member| self.member_write(&receiver, member, access).is_none())
            && [
                "tool",
                "add_tool",
                "__new__",
                "__init__",
                "__getattr__",
                "__getattribute__",
            ]
            .iter()
            .all(|member| self.member_write(&base, member, access).is_none())
    }

    pub(super) fn signal_member_write(
        &self,
        site: Object,
        name: &str,
        access: (ScopeId, Location),
    ) -> Option<MemberWriteValue> {
        self.member_write(&ReceiverKey::Signal(site), name, access)
    }

    pub(super) fn fastapi_member_write(
        &self,
        site: Object,
        name: &str,
        access: (ScopeId, Location),
    ) -> Option<MemberWriteValue> {
        self.member_write(&ReceiverKey::Fastapi(site), name, access)
    }

    pub(super) fn fastapi_base_member_changed(
        &self,
        name: &str,
        access: (ScopeId, Location),
    ) -> bool {
        self.member_write(&ReceiverKey::FastapiBase, name, access)
            .is_some()
    }

    pub(super) fn imported_module_write(
        &self,
        module: &str,
        access: (ScopeId, Location),
    ) -> Option<Value> {
        match self.write_value(self.mutations.modules.get(module)?, None, access)? {
            MemberWriteValue::Replacement(value) => Some(value),
            MemberWriteValue::Deleted => Some(Value::MissingFrameworkMember),
            MemberWriteValue::Ambiguous => Some(Value::Unknown),
        }
    }

    fn member_write(
        &self,
        receiver: &ReceiverKey,
        name: &str,
        access: (ScopeId, Location),
    ) -> Option<MemberWriteValue> {
        let original = match receiver {
            ReceiverKey::Class(class) => Some(self.scopes[self.symbols[*class].scope].file),
            ReceiverKey::Module(module) => self.modules.get(module.as_str()).copied(),
            ReceiverKey::FastapiBase | ReceiverKey::FlaskBase(_) | ReceiverKey::McpServerBase => {
                None
            }
            ReceiverKey::Signal(site)
            | ReceiverKey::FlaskLoginManager(site)
            | ReceiverKey::FlaskApplication(site)
            | ReceiverKey::Fastapi(site)
            | ReceiverKey::McpServer(site)
            | ReceiverKey::TyperApp(site) => Some(site.0),
        };
        self.write_value(
            self.mutations.members.get(receiver)?.get(name)?,
            original,
            access,
        )
    }

    fn write_value(
        &self,
        writes: &[Write],
        original: Option<ScopeId>,
        (scope, at): (ScopeId, Location),
    ) -> Option<MemberWriteValue> {
        let reached = || {
            writes
                .iter()
                .filter(|write| self.before_flask_initializer_failure(write.file, write.at))
        };
        let reader = self.scopes[scope].file;
        let mut before = at;
        let mut deferred = false;
        let mut current = scope;
        while let Some(parent) = self.scopes[current].parent {
            if let Some(owner) = self.scopes[current].owner {
                before = self.symbols[owner].name.at;
                deferred |= !self.is_class(owner);
            }
            current = parent;
        }
        // A function can run before or after a later write in its module.
        // Its enclosing module-level declaration provides a lower bound.
        let before = if deferred { before } else { at };
        if deferred
            && reached().any(|write| write.file == reader && position(write.at) >= position(before))
        {
            return Some(MemberWriteValue::Ambiguous);
        }
        let local = reached()
            .rev()
            .find(|write| write.file == reader && position(write.at) < position(before));
        let unordered = reached().any(|write| write.file != reader && Some(write.file) != original);
        if unordered {
            return Some(MemberWriteValue::Ambiguous);
        }
        local
            .or_else(|| {
                reached()
                    .rev()
                    .find(|write| Some(write.file) == original && write.file != reader)
            })
            .map(|write| write.value.clone())
    }
}

fn mock_start(expression: &Expression) -> Option<(&Expression, &[Expression])> {
    let ExpressionKind::Call {
        function,
        arguments,
        keywords,
    } = &expression.kind
    else {
        return None;
    };
    let ExpressionKind::Member(patch, method) = &function.kind else {
        return None;
    };
    let ExpressionKind::Call {
        function: constructor,
        arguments: target,
        keywords: patch_keywords,
    } = &patch.kind
    else {
        return None;
    };
    (method == "start" && arguments.is_empty() && keywords.is_empty() && patch_keywords.is_empty())
        .then_some((constructor, target))
}

fn flask_globals_update(expression: &Expression) -> Option<(&Expression, &str, &Expression)> {
    let ExpressionKind::Call {
        function,
        arguments,
        keywords,
    } = &expression.kind
    else {
        return None;
    };
    let ExpressionKind::Member(namespace, update) = &function.kind else {
        return None;
    };
    let [
        Expression {
            kind: ExpressionKind::Dictionary(items),
            ..
        },
    ] = arguments.as_slice()
    else {
        return None;
    };
    let [
        Expression {
            kind: ExpressionKind::Text(name),
            ..
        },
        value,
    ] = items.as_slice()
    else {
        return None;
    };
    let ExpressionKind::Call {
        function: globals,
        arguments: namespace_arguments,
        keywords: namespace_keywords,
    } = &namespace.kind
    else {
        return None;
    };
    (update == "update"
        && keywords.is_empty()
        && namespace_arguments.is_empty()
        && namespace_keywords.is_empty()
        && matches!(&globals.kind, ExpressionKind::Name(name) if name == "globals"))
    .then_some((globals, name, value))
}

fn provider_module(name: &str) -> bool {
    matches!(
        name,
        "unittest"
            | "logging"
            | "logging.config"
            | "unittest.case"
            | "unittest.async_case"
            | "django"
            | "django.test"
            | "django.test.testcases"
            | "django.dispatch"
            | "django.forms"
            | "django.forms.fields"
            | "django.forms.forms"
            | "django.forms.models"
            | "django.dispatch.dispatcher"
            | "django.db.models.signals"
            | "django.db.migrations"
            | "django.db.migrations.operations"
            | "django.db.migrations.operations.special"
            | "flask"
            | "flask.app"
            | "flask.blueprints"
            | "flask_login"
            | "flask_login.login_manager"
            | "mcp.server.fastmcp"
            | "mcp.server.fastmcp.server"
            | "celery"
            | "gin"
            | "gin.config"
            | "pydantic"
            | "pydantic.validate_call_decorator"
            | "tenacity"
            | "tenacity.stop"
            | "typer"
            | "typer.main"
            | "click"
            | "click.decorators"
            | "django.utils.autoreload"
            | "pathlib"
            | "pathlib.Path"
            | "pathlib.PurePath"
            | "pathlib.PurePosixPath"
            | "pathlib.PureWindowsPath"
            | "pathlib.PosixPath"
            | "pathlib.WindowsPath"
    )
}
