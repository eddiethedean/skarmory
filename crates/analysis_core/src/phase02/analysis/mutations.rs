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
    flask_escapes: BTreeMap<ReceiverKey, Vec<Write>>,
    modeled: BTreeSet<(ScopeId, [usize; 2])>,
}

impl Graph<'_> {
    pub(super) fn index_framework_mutations(&mut self) {
        let track_flask_escapes = self.symbols.iter().any(|symbol| {
            matches!(symbol.binding, Binding::Import(import) if import.level == 0
                && matches!(import.module.split('.').next(), Some("flask" | "flask_login")))
        });
        for file in 0..self.files.len() {
            self.index_module_mutations(file, self.scopes[file].body, false, track_flask_escapes);
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
        for expression in arguments
            .iter()
            .chain(keywords.iter().map(|(_, value)| value))
        {
            flask_escape_keys(self.probe_identity(file, expression, at), &mut escaped);
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
                            && matches!(member, "__init__" | "add_arguments" | "handle")
                        || role == FrameworkClassRole::DjangoSignal && member == "connect"
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
                ) || is_flask_login_constructor(&qualified)
                    || is_typer_constructor(&qualified)
                    || is_django_receiver(&qualified)
                    || is_django_signal_constructor(&qualified)
                    || is_django_runpython(&qualified)
                    || django_template_paths::is_pathlib_member(module, member)
                    || qualified == "django.utils.autoreload.run_with_reloader"
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

// These values expose either the object itself or a bound method's receiver.
fn flask_escape_keys(value: Value, escaped: &mut BTreeSet<ReceiverKey>) {
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
                flask_escape_keys(value, escaped);
            }
        }
        _ => {}
    }
}

fn provider_module(name: &str) -> bool {
    matches!(
        name,
        "unittest"
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
