//! Bounded Django form declarations, field tables, and validation callbacks.

use super::*;

pub(super) const VALIDATION_HOOK: &str = "django_form_validation";

#[derive(Clone, Copy)]
pub(super) enum Field {
    Form,
    Model { editable: bool },
}

pub(super) fn provider_field_role(provider: &str) -> Option<FrameworkClassRole> {
    if provider
        .strip_prefix("django.forms.fields.")
        .or_else(|| provider.strip_prefix("django.forms."))
        .is_some_and(is_form_field)
    {
        Some(FrameworkClassRole::DjangoFormField)
    } else {
        None
    }
}

fn is_form_field(name: &str) -> bool {
    matches!(
        name,
        "Field"
            | "BooleanField"
            | "CharField"
            | "ChoiceField"
            | "DateField"
            | "DateTimeField"
            | "DecimalField"
            | "DurationField"
            | "EmailField"
            | "FloatField"
            | "GenericIPAddressField"
            | "IntegerField"
            | "JSONField"
            | "NullBooleanField"
            | "RegexField"
            | "SlugField"
            | "TimeField"
            | "TypedChoiceField"
            | "URLField"
            | "UUIDField"
    )
}

fn is_model_field(provider: &str) -> bool {
    provider
        .strip_prefix("django.db.models.fields.")
        .or_else(|| provider.strip_prefix("django.db.models."))
        .is_some_and(|name| {
            matches!(
                name,
                "BigIntegerField"
                    | "BooleanField"
                    | "CharField"
                    | "DateField"
                    | "DateTimeField"
                    | "DecimalField"
                    | "DurationField"
                    | "EmailField"
                    | "FloatField"
                    | "GenericIPAddressField"
                    | "IntegerField"
                    | "PositiveBigIntegerField"
                    | "PositiveIntegerField"
                    | "PositiveSmallIntegerField"
                    | "SlugField"
                    | "SmallIntegerField"
                    | "TextField"
                    | "TimeField"
                    | "URLField"
                    | "UUIDField"
            )
        })
}

pub(super) fn is_field_provider(provider: &str) -> bool {
    provider_field_role(provider).is_some() || is_model_field(provider)
}

fn field_option(provider: &str, option: &str, form: bool) -> bool {
    let field = provider.rsplit('.').next().unwrap_or_default();
    if form {
        matches!(
            option,
            "disabled"
                | "help_text"
                | "initial"
                | "label"
                | "label_suffix"
                | "required"
                | "show_hidden_initial"
        ) || matches!(
            (field, option),
            (
                "CharField" | "EmailField" | "SlugField" | "RegexField",
                "max_length" | "min_length"
            ) | ("CharField", "strip" | "empty_value")
                | (
                    "IntegerField" | "FloatField" | "DecimalField",
                    "max_value" | "min_value" | "step_size"
                )
                | ("DecimalField", "max_digits" | "decimal_places")
                | ("SlugField", "allow_unicode")
                | ("GenericIPAddressField", "protocol" | "unpack_ipv4")
                | ("URLField", "assume_scheme")
        )
    } else {
        matches!(
            option,
            "blank"
                | "db_column"
                | "db_index"
                | "default"
                | "editable"
                | "help_text"
                | "max_length"
                | "null"
                | "primary_key"
                | "serialize"
                | "unique"
                | "verbose_name"
        ) || matches!(
            (field, option),
            (
                "DateField" | "DateTimeField" | "TimeField",
                "auto_now" | "auto_now_add"
            ) | ("DecimalField", "max_digits" | "decimal_places")
                | ("SlugField", "allow_unicode")
                | ("GenericIPAddressField", "protocol" | "unpack_ipv4")
        )
    }
}

#[derive(Default)]
struct Lineage {
    seen: BTreeSet<SymbolId>,
    role: Option<FrameworkClassRole>,
    mros: BTreeMap<SymbolId, Vec<SymbolId>>,
}

impl Graph<'_> {
    pub(super) fn django_field_value(
        &mut self,
        provider: &str,
        call: CallArguments<'_>,
    ) -> Option<Field> {
        let mut editable = true;
        let mut auto_updated = false;
        let mut names = BTreeSet::new();
        let form = provider_field_role(provider).is_some();
        if !call.positional.is_empty() {
            return None;
        }
        for (name, expression) in call.keywords {
            if !names.insert(name) || !field_option(provider, name, form) {
                return None;
            }
            let value = self.probe_identity(call.scope, expression, call.before);
            if !value.is_builtin_scalar() {
                return None;
            }
            if name == "editable" {
                let Value::Bool(value) = value else {
                    return None;
                };
                editable = value;
            }
            if name == "auto_now" || name == "auto_now_add" {
                let Value::Bool(value) = value else {
                    return None;
                };
                auto_updated |= value;
            }
        }
        if form {
            Some(Field::Form)
        } else if is_model_field(provider) {
            Some(Field::Model {
                editable: editable && !auto_updated,
            })
        } else {
            None
        }
    }

    pub(super) fn check_django_form_initializer(
        &mut self,
        class: SymbolId,
        call: CallArguments<'_>,
    ) {
        if call.positional.is_empty() && call.keywords.is_empty()
            || self
                .django_table_lineage(
                    class,
                    &[
                        FrameworkClassRole::DjangoForm,
                        FrameworkClassRole::DjangoModelForm,
                    ],
                )
                .is_none()
            || self.class_member(class, "__init__", 0).is_some()
        {
            return;
        }
        let positional = ["data", "files"];
        let mut seen = BTreeSet::new();
        if call.positional.len() > positional.len()
            || !positional
                .into_iter()
                .zip(call.positional)
                .chain(
                    call.keywords
                        .iter()
                        .map(|(name, value)| (name.as_str(), value)),
                )
                .all(|(name, value)| {
                    seen.insert(name) && self.django_form_initializer_argument(name, value, call)
                })
        {
            self.scope_issue(
                call.scope,
                call.at,
                "unresolved_constructor",
                "Django form arguments may dispatch into unmodeled data or initialization hooks",
            );
        }
    }

    fn django_form_initializer_argument(
        &mut self,
        name: &str,
        expression: &Expression,
        call: CallArguments<'_>,
    ) -> bool {
        let value = self.probe_identity(call.scope, expression, call.before);
        match name {
            "data" | "files" | "initial" => {
                matches!(expression.kind, ExpressionKind::None)
                    || matches!(value, Value::Mapping(_)) && inert_submission(&value)
            }
            "auto_id"
            | "empty_permitted"
            | "label_suffix"
            | "prefix"
            | "use_required_attribute" => {
                value.is_builtin_scalar() || matches!(value, Value::Integer(_) | Value::None)
            }
            _ => false,
        }
    }

    fn django_table_lineage(
        &mut self,
        class: SymbolId,
        roles: &[FrameworkClassRole],
    ) -> Option<Lineage> {
        let mut lineage = Lineage::default();
        self.django_class_mro(class, roles, &mut lineage)?;
        Some(lineage)
    }

    fn django_class_mro(
        &mut self,
        class: SymbolId,
        roles: &[FrameworkClassRole],
        lineage: &mut Lineage,
    ) -> Option<()> {
        if lineage.mros.contains_key(&class) {
            return Some(());
        }
        if lineage.seen.len() >= MAX_BINDING_CHAIN || !lineage.seen.insert(class) {
            return None;
        }
        let symbol = &self.symbols[class];
        let Binding::Definition(definition, _) = symbol.binding else {
            return None;
        };
        if !definition.decorators.is_empty() || definition.bases.is_empty() {
            return None;
        }
        let module = symbol.scope;
        let mut parents = Vec::new();
        for base in &definition.bases {
            match self.class_base(module, base, definition.name.at, 0) {
                Value::Callable(Invocation { target: parent, .. }) if self.is_class(parent) => {
                    self.django_class_mro(parent, roles, lineage)?;
                    parents.push(parent);
                }
                Value::Provider(provider) if definition.bases.len() == 1 => {
                    let role = framework_class_provider(&provider)?;
                    if !roles.contains(&role) || lineage.role.is_some_and(|prior| prior != role) {
                        return None;
                    }
                    lineage.role = Some(role);
                }
                _ => return None,
            }
        }
        if parents.len() > 1
            && !matches!(
                lineage.role,
                Some(FrameworkClassRole::DjangoForm | FrameworkClassRole::DjangoModelForm)
            )
        {
            return None;
        }
        let mut lists: Vec<_> = parents
            .iter()
            .map(|parent| lineage.mros[parent].as_slice())
            .chain(std::iter::once(parents.as_slice()))
            .collect();
        let mut mro = vec![class];
        while lists.iter().any(|list| !list.is_empty()) {
            let &next = lists
                .iter()
                .filter_map(|list| list.first())
                .find(|candidate| {
                    !lists
                        .iter()
                        .any(|list| list.get(1..).is_some_and(|tail| tail.contains(candidate)))
                })?;
            mro.push(next);
            for list in &mut lists {
                if list.first() == Some(&next) {
                    *list = &list[1..];
                }
            }
        }
        lineage.mros.insert(class, mro);
        Some(())
    }

    pub(super) fn django_form_multiple_member(
        &mut self,
        class: SymbolId,
        name: &str,
        depth: usize,
        access: Option<(ScopeId, Location)>,
    ) -> Option<Option<Value>> {
        let lineage = self.django_table_lineage(
            class,
            &[
                FrameworkClassRole::DjangoForm,
                FrameworkClassRole::DjangoModelForm,
            ],
        )?;
        for &parent in &lineage.mros[&class][1..] {
            match access.and_then(|site| self.class_member_write(parent, name, site)) {
                Some(mutations::MemberWriteValue::Replacement(value)) => return Some(Some(value)),
                Some(mutations::MemberWriteValue::Ambiguous) => return Some(Some(Value::Unknown)),
                Some(mutations::MemberWriteValue::Deleted) => {}
                None => {
                    if let Some(id) = self.local(self.django_class_body(parent), name, END) {
                        return Some(Some(self.binding(id, END, depth + 1)));
                    }
                }
            }
        }
        Some(match name {
            "__init__" | "__new__" | "__init_subclass__" => None,
            "is_valid" | "full_clean" | "non_field_errors" => {
                Some(Value::DjangoFormValidation(None))
            }
            _ => Some(Value::Unknown),
        })
    }

    fn django_class_body(&self, class: SymbolId) -> ScopeId {
        let Binding::Definition(_, body) = self.symbols[class].binding else {
            unreachable!("Django lineage contains only classes");
        };
        body
    }

    pub(super) fn django_literal_none(&self, id: SymbolId, depth: usize) -> Option<bool> {
        if depth >= MAX_BINDING_CHAIN || self.binding_depends_on_branch(id, END) {
            return None;
        }
        let symbol = &self.symbols[id];
        let Binding::Variable(Some(value), before) = symbol.binding else {
            return None;
        };
        match &value.kind {
            ExpressionKind::None => Some(true),
            ExpressionKind::Scalar(_) | ExpressionKind::Text(_) | ExpressionKind::Bool(_) => {
                Some(false)
            }
            ExpressionKind::Name(name) => self
                .lexical(symbol.scope, name, before)
                .and_then(|source| self.django_literal_none(source, depth + 1)),
            _ => None,
        }
    }

    fn django_model_meta_option(
        &mut self,
        meta: SymbolId,
        name: &str,
        access: (ScopeId, Location),
    ) -> Option<Value> {
        let value = self.class_member_at(meta, name, 0, Some(access))?;
        if matches!(value, Value::Scalar)
            && self.class_member_write(meta, name, access).is_none()
            && self
                .local(self.django_class_body(meta), name, END)
                .is_some_and(|id| self.django_literal_none(id, 0) == Some(true))
        {
            return Some(Value::None);
        }
        Some(value)
    }

    fn django_declared_fields(
        &mut self,
        class: SymbolId,
        lineage: &Lineage,
        model: bool,
        access: (ScopeId, Location),
    ) -> Option<BTreeSet<String>> {
        let mut tables: BTreeMap<_, BTreeSet<String>> = BTreeMap::new();
        let mut removals: BTreeMap<_, BTreeSet<String>> = BTreeMap::new();
        for &owner in lineage.mros[&class].iter().rev() {
            let body = self.django_class_body(owner);
            if ["base_fields", "declared_fields", "_meta"]
                .iter()
                .any(|name| {
                    self.local(body, name, END).is_some()
                        || self.class_member_write(owner, name, access).is_some()
                })
            {
                return None;
            }
            let mut fields = BTreeSet::new();
            // Django's form metaclass updates from each ancestor's effective
            // declared_fields and then applies that ancestor's None masks.
            for parent in lineage.mros[&owner][1..].iter().rev() {
                fields.extend(tables[parent].iter().cloned());
                for name in &removals[parent] {
                    fields.remove(name);
                }
            }
            let mut removed = BTreeSet::new();
            let bindings: Vec<_> = self.scopes[body]
                .names
                .iter()
                .filter_map(|(&name, _)| self.local(body, name, END).map(|id| (name, id)))
                .collect();
            for (name, id) in bindings {
                if matches!(
                    self.symbols[id].binding,
                    Binding::Definition(..) | Binding::Deleted
                ) {
                    continue;
                }
                let was_read = self.read.contains(&id);
                let value = self.binding(id, END, 0);
                let value = if matches!(value, Value::Scalar) && !model {
                    match self.django_literal_none(id, 0) {
                        Some(true) => Value::None,
                        None if fields.contains(name) => return None,
                        _ => value,
                    }
                } else {
                    value
                };
                let used_field = match value {
                    Value::DjangoField(Field::Form) if !model => {
                        fields.insert(name.to_owned());
                        true
                    }
                    Value::DjangoField(Field::Model { editable }) if model => {
                        if editable {
                            fields.insert(name.to_owned());
                        }
                        true
                    }
                    Value::Instance(field_class)
                        if !model
                            && self.framework_class_role(field_class, &mut BTreeSet::new(), 0)
                                == Some(FrameworkClassRole::DjangoFormField) =>
                    {
                        if !self.django_inert_form_field(field_class) {
                            return None;
                        }
                        fields.insert(name.to_owned());
                        true
                    }
                    Value::None if !model => {
                        fields.remove(name);
                        removed.insert(name.to_owned());
                        false
                    }
                    Value::Alternatives(_) | Value::Unknown => return None,
                    _ => false,
                };
                if !was_read && !used_field {
                    self.read.remove(&id);
                }
            }
            tables.insert(owner, fields);
            removals.insert(owner, removed);
        }
        tables.remove(&class)
    }

    fn django_inert_form_field(&mut self, class: SymbolId) -> bool {
        self.django_table_lineage(class, &[FrameworkClassRole::DjangoFormField])
            .is_some_and(|lineage| {
                lineage.mros[&class]
                    .iter()
                    .all(|&owner| self.scopes[self.django_class_body(owner)].names.is_empty())
            })
    }

    fn django_model_form_fields(
        &mut self,
        class: SymbolId,
        declared: &BTreeSet<String>,
    ) -> Option<BTreeSet<String>> {
        // ModelFormMetaclass snapshots Meta into _meta when the class is declared.
        let symbol = &self.symbols[class];
        let access = (symbol.scope, symbol.name.at);
        let Some(Value::Callable(meta)) = self.class_member_at(class, "Meta", 0, Some(access))
        else {
            return None;
        };
        if !self.is_class(meta.target) {
            return None;
        }
        if ["formfield_callback", "field_classes"].iter().any(|name| {
            self.class_member_at(meta.target, name, 0, Some(access))
                .is_some()
        }) {
            return None;
        }
        let Some(Value::Callable(model)) =
            self.class_member_at(meta.target, "model", 0, Some(access))
        else {
            return None;
        };
        let lineage =
            self.django_table_lineage(model.target, &[FrameworkClassRole::DjangoModel])?;
        if lineage.mros[&model.target].iter().any(|&class| {
            let body = self.django_class_body(class);
            [
                "__init__",
                "clean",
                "clean_fields",
                "full_clean",
                "validate_constraints",
                "validate_unique",
            ]
            .iter()
            .any(|name| {
                self.local(body, name, END).is_some()
                    || self.class_member_write(class, name, access).is_some()
            })
        }) {
            return None;
        }
        let mut fields = self.django_declared_fields(model.target, &lineage, true, access)?;
        let excluded = match self.django_model_meta_option(meta.target, "exclude", access) {
            None | Some(Value::None) => None,
            Some(Value::Sequence(exclude)) => Some(string_set(exclude)?),
            _ => return None,
        };
        match self.django_model_meta_option(meta.target, "fields", access) {
            Some(Value::Text(all)) if all == "__all__" => {}
            Some(Value::Sequence(selected)) => {
                let selected = string_set(selected)?;
                if selected.iter().any(|name| {
                    !fields.contains(name)
                        && !declared.contains(name)
                        && !excluded
                            .as_ref()
                            .is_some_and(|exclude| exclude.contains(name))
                }) {
                    return None;
                }
                fields.retain(|name| selected.contains(name));
            }
            None | Some(Value::None) if excluded.is_some() => {}
            _ => return None,
        }
        if let Some(excluded) = excluded {
            fields.retain(|name| !excluded.contains(name));
        }
        Some(fields)
    }

    pub(super) fn record_django_form_validation(
        &mut self,
        class: SymbolId,
        call: CallArguments<'_>,
    ) {
        if call.positional.is_empty()
            && call.keywords.is_empty()
            && self.django_form_validation(class, call).is_some()
        {
            return;
        }
        self.scope_issue(
            call.scope,
            call.at,
            "unresolved_registration",
            "Django form validation has an unresolved field table or dispatch",
        );
    }

    fn django_form_validation(&mut self, class: SymbolId, call: CallArguments<'_>) -> Option<()> {
        let lineage = self.django_table_lineage(
            class,
            &[
                FrameworkClassRole::DjangoForm,
                FrameworkClassRole::DjangoModelForm,
            ],
        )?;
        let access = (call.scope, call.before);
        let mro = &lineage.mros[&class];
        if mro.iter().any(|&class| {
            let body = self.django_class_body(class);
            [
                "__init__",
                "__new__",
                "__getattribute__",
                "__getattr__",
                "_clean_fields",
                "_clean_form",
                "_post_clean",
                "full_clean",
                "errors",
            ]
            .iter()
            .any(|name| {
                self.local(body, name, END).is_some()
                    || self.class_member_write(class, name, access).is_some()
            })
        }) {
            return None;
        }
        let mut fields = self.django_declared_fields(class, &lineage, false, access)?;
        if lineage.role == Some(FrameworkClassRole::DjangoModelForm) {
            let generated = self.django_model_form_fields(class, &fields)?;
            fields.extend(generated);
        }
        let hooks = std::iter::once("clean".to_owned())
            .chain(fields.into_iter().map(|field| format!("clean_{field}")));
        for name in hooks {
            let Some(member) = self.class_member_at(class, &name, 0, Some(access)) else {
                continue;
            };
            let invocation = match member {
                Value::Callable(invocation) if !self.is_class(invocation.target) => invocation,
                Value::Unknown
                    if !mro.iter().any(|&owner| {
                        self.local(self.django_class_body(owner), &name, END)
                            .is_some()
                            || self.class_member_write(owner, &name, access).is_some()
                    }) =>
                {
                    // No source hook on the selected lineage.
                    continue;
                }
                _ => return None,
            };
            let Binding::Definition(definition, _) = self.symbols[invocation.target].binding else {
                return None;
            };
            if definition.kind != DefinitionKind::Function
                || definition.is_generator
                || !definition.decorators.is_empty()
            {
                return None;
            }
            // Retain a selected callback as possible at a validation site.
            // This records the configured contract, not a claim that this
            // particular form is bound or that it passes field validation.
            self.invoke(
                call.scope,
                invocation.bind_receiver(Receiver::Bound(class)),
                VALIDATION_HOOK,
                "possible",
                call.at,
            );
        }
        Some(())
    }
}

fn inert_submission(value: &Value) -> bool {
    match value {
        Value::Mapping(items) | Value::Sequence(items) => items.iter().all(inert_submission),
        Value::Integer(_) | Value::None => true,
        _ => value.is_builtin_scalar(),
    }
}

fn string_set(values: Vec<Value>) -> Option<BTreeSet<String>> {
    values
        .into_iter()
        .map(|value| match value {
            Value::Text(text) => Some(text),
            _ => None,
        })
        .collect()
}
