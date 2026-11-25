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

#[derive(Default)]
pub(super) struct Mutations {
    members: BTreeMap<ReceiverKey, BTreeMap<String, Vec<Write>>>,
    modules: BTreeMap<String, Vec<Write>>,
    command_namespaces: BTreeSet<ScopeId>,
    command_globals: BTreeMap<ScopeId, BTreeMap<String, Vec<Write>>>,
    form_table_fields: BTreeMap<SymbolId, BTreeMap<String, Vec<Write>>>,
    flask_escapes: BTreeMap<ReceiverKey, Vec<Write>>,
    modeled: BTreeSet<(ScopeId, [usize; 2])>,
}

impl Graph<'_> {
    pub(super) fn index_framework_mutations(&mut self) {
        let track_flask_escapes = self.symbols.iter().any(|symbol| {
            matches!(symbol.binding, Binding::Import(import) if import.level == 0
                && matches!(import.module.split('.').next(), Some("flask" | "flask_login")))
        });
        self.index_command_namespaces();
        for file in 0..self.files.len() {
            self.index_module_mutations(file, self.scopes[file].body, false, track_flask_escapes);
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

    pub(super) fn command_global_value(
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
                    "Django command module lookup depends on an unresolved namespace write",
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
        track_flask_escapes: bool,
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
                StatementKind::Evaluate(Expression {
                    kind:
                        ExpressionKind::Call {
                            function,
                            arguments,
                            keywords,
                        },
                    ..
                }) if !arguments.is_empty() || !keywords.is_empty() => {
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
                    } else if track_flask_escapes {
                        match self.probe_identity(file, function, at) {
                            Value::Unknown => {
                                self.index_flask_escape(file, at, arguments, keywords)
                            }
                            Value::Provider(provider) if !is_known_call(&provider, true) => {
                                self.index_flask_escape(file, at, arguments, keywords);
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
                    Some(true) => {
                        self.index_module_mutations(file, body, conditional, track_flask_escapes)
                    }
                    Some(false) => self.index_module_mutations(
                        file,
                        otherwise,
                        conditional,
                        track_flask_escapes,
                    ),
                    None => {
                        self.index_module_mutations(file, body, true, track_flask_escapes);
                        self.index_module_mutations(file, otherwise, true, track_flask_escapes);
                    }
                },
                StatementKind::Return(_) | StatementKind::Raise(_) => break,
                _ => {}
            }
        }
    }

    fn index_flask_escape(
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
            self.flask_escape_keys(value, (file, at), &mut escaped, &mut visited, 0);
            self.read = read;
        }
        for receiver in escaped {
            self.mutations
                .flask_escapes
                .entry(receiver)
                .or_default()
                .push(Write {
                    file,
                    at,
                    value: MemberWriteValue::Ambiguous,
                });
        }
    }

    fn flask_escape_keys(
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
            Value::Provider(name) => {
                escaped.extend(flask::provider_kind(&name).map(ReceiverKey::FlaskBase));
            }
            Value::Sequence(values) | Value::Mapping(values) | Value::Alternatives(values) => {
                for value in values {
                    self.flask_escape_keys(value, access, escaped, visited, depth);
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
                    self.flask_escape_keys(value, access, escaped, visited, depth + 1);
                }
                for default in &definition.defaults {
                    let value = self.probe_identity(file, default, definition.name.at);
                    self.flask_escape_keys(value, access, escaped, visited, depth + 1);
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
        let receiver = self.probe_identity(file, receiver, at);
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
        let value = if conditional
            || deletes_inherited_method
            || matches!(
                receiver,
                ReceiverKey::FastapiBase | ReceiverKey::FlaskBase(_)
            ) {
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
        let value = if conditional || replacement.contains_unknown() {
            MemberWriteValue::Ambiguous
        } else {
            self.mutations.modeled.insert((file, position(at)));
            MemberWriteValue::Replacement(replacement)
        };
        self.mutations
            .command_globals
            .entry(file)
            .or_default()
            .entry(name.into())
            .or_default()
            .push(Write { file, at, value });
        true
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
            || !matches!(replacement, Value::DjangoField(django_forms::Field::Form))
        {
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

    pub(super) fn command_global_write(
        &self,
        file: ScopeId,
        name: &str,
        before: Location,
        access: (ScopeId, Location),
    ) -> Option<MemberWriteValue> {
        let writes = self.mutations.command_globals.get(&file)?.get(name)?;
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
                        || role == FrameworkClassRole::DjangoCommand
                            && (matches!(
                                member,
                                "__new__"
                                    | "__init__"
                                    | "__getattribute__"
                                    | "run_from_argv"
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
                ) || is_flask_login_constructor(&qualified)
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
            Value::Provider(provider) if flask::provider_kind(provider).is_some() => {
                flask::provider_kind(provider).map(ReceiverKey::FlaskBase)
            }
            Value::Provider(module) if provider_module(module) || is_django_signal(module) => {
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
        &self,
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
        if self.flask_has_escaped(&receiver, Some(site.0), access)
            || self.flask_has_escaped(&class, None, access)
            || self.member_write(&receiver, "__class__", access).is_some()
            || ["__getattr__", "__getattribute__"]
                .iter()
                .any(|member| self.member_write(&class, member, access).is_some())
        {
            return Value::Unknown;
        }
        match self.member_write(&receiver, name, access) {
            Some(MemberWriteValue::Replacement(value)) => value,
            Some(MemberWriteValue::Ambiguous) => Value::Unknown,
            Some(MemberWriteValue::Deleted) | None => {
                if self.member_write(&class, name, access).is_some() {
                    Value::Unknown
                } else {
                    if kind == ObjectKind::FlaskApp
                        && name == "config"
                        && ["config_class", "make_config"].iter().all(|name| {
                            self.member_write(&class, name, access).is_none()
                                && self.member_write(&receiver, name, access).is_none()
                        })
                    {
                        Value::FlaskConfig
                    } else {
                        Value::Method(site, kind, name.into())
                    }
                }
            }
        }
    }

    pub(super) fn flask_constructor_changed(
        &self,
        kind: ObjectKind,
        access: (ScopeId, Location),
    ) -> bool {
        let class = ReceiverKey::FlaskBase(kind);
        self.flask_has_escaped(&class, None, access)
            || ["__new__", "__init__"]
                .iter()
                .any(|member| self.member_write(&class, member, access).is_some())
    }

    fn flask_has_escaped(
        &self,
        receiver: &ReceiverKey,
        origin: Option<ScopeId>,
        access: (ScopeId, Location),
    ) -> bool {
        self.mutations
            .flask_escapes
            .get(receiver)
            .is_some_and(|writes| self.write_value(writes, origin, access).is_some())
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
            ReceiverKey::FastapiBase | ReceiverKey::FlaskBase(_) => None,
            ReceiverKey::Signal(site)
            | ReceiverKey::FlaskLoginManager(site)
            | ReceiverKey::FlaskApplication(site)
            | ReceiverKey::Fastapi(site)
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
            && writes
                .iter()
                .any(|write| write.file == reader && position(write.at) >= position(before))
        {
            return Some(MemberWriteValue::Ambiguous);
        }
        let local = writes
            .iter()
            .rev()
            .find(|write| write.file == reader && position(write.at) < position(before));
        let unordered = writes
            .iter()
            .any(|write| write.file != reader && Some(write.file) != original);
        if unordered {
            return Some(MemberWriteValue::Ambiguous);
        }
        local
            .or_else(|| {
                writes
                    .iter()
                    .rev()
                    .find(|write| Some(write.file) == original && write.file != reader)
            })
            .map(|write| write.value.clone())
    }
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
            | "celery"
            | "typer"
            | "typer.main"
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
