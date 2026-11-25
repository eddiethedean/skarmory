//! Bounded Django REST Framework class and field declarations.

use super::*;

pub(super) const PROTOCOL_DECLARATION: &str = "drf_protocol_declaration";
pub(super) const FIELD_DECLARATION: &str = "drf_field_declaration";
const FIELD_LOOKUP: &[&str] = &[
    "_declared_fields",
    "fields",
    "get_fields",
    "_writable_fields",
    "to_internal_value",
];

pub(super) fn is_viewset_action(name: &str) -> bool {
    matches!(
        name,
        "list" | "retrieve" | "create" | "update" | "partial_update" | "destroy"
    )
}

pub(super) fn is_validator(name: &str) -> bool {
    name == "validate" || name.starts_with("validate_")
}

pub(super) fn is_field(provider: &str) -> bool {
    let Some((module, name)) = provider.rsplit_once('.') else {
        return false;
    };
    matches!(
        module,
        "rest_framework.fields" | "rest_framework.serializers"
    ) && matches!(
        name,
        "BooleanField"
            | "CharField"
            | "DateField"
            | "DateTimeField"
            | "EmailField"
            | "FloatField"
            | "IntegerField"
            | "TimeField"
            | "URLField"
            | "UUIDField"
    )
}

impl Graph<'_> {
    pub(super) fn drf_field_writable(
        &mut self,
        scope: ScopeId,
        before: Location,
        arguments: &[Expression],
        keywords: &[(String, Expression)],
    ) -> Option<bool> {
        registration_arguments(
            arguments,
            keywords,
            ["read_only", "write_only", "required", "allow_null"],
        )?;
        if !arguments.is_empty() {
            return None;
        }
        let mut flags = BTreeMap::new();
        for (name, expression) in keywords {
            let Value::Bool(value) = self.probe_identity(scope, expression, before) else {
                return None;
            };
            flags.insert(name.as_str(), value);
        }
        let flag = |name| flags.get(name).copied().unwrap_or(false);
        let read_only = flag("read_only");
        if read_only && (flag("write_only") || flag("required")) {
            return None;
        }
        // DRF accepts an omitted required flag on a read-only field.
        Some(!read_only)
    }

    pub(super) fn collect_drf_declaration(
        &mut self,
        class: SymbolId,
        body: ScopeId,
        role: FrameworkClassRole,
    ) {
        let symbol = &self.symbols[class];
        let scope = symbol.scope;
        let name = &symbol.name.text;
        if scope != self.scopes[scope].file
            || name.starts_with('_')
            || self.local(scope, name, END) != Some(class)
            || self.binding_depends_on_branch(class, END)
        {
            return;
        }
        let Binding::Definition(definition, _) = symbol.binding else {
            return;
        };
        if !definition.decorators.is_empty() {
            return;
        }
        self.edge(scope, class, PROTOCOL_DECLARATION, "possible");

        // Fields are collected by SerializerMetaclass at class creation.
        // Potential external dispatch retains the matching methods separately.
        let mut writable = BTreeSet::new();
        if role == FrameworkClassRole::DrfSerializer {
            let (lineage, _) = self.class_lineage(class);
            let bounded_lookup = FIELD_LOOKUP.iter().all(|name| {
                self.class_discovery_write(class, name, scope).is_none()
                    && lineage
                        .iter()
                        .all(|&body| self.local(body, name, END).is_none())
            });
            let mut names = BTreeSet::new();
            for ancestor in lineage {
                let bindings = self.drf_class_bindings(ancestor);
                for field in bindings {
                    let name = self.symbols[field].name.text.as_str();
                    if !names.insert(name) {
                        continue;
                    }
                    if self.binding_depends_on_branch(field, END) {
                        continue;
                    }
                    if let Some(can_write) = self.drf_field_binding(field) {
                        self.edge(scope, field, FIELD_DECLARATION, "certain");
                        if can_write && bounded_lookup {
                            writable.insert(name);
                        }
                    }
                }
            }
        }

        for method in self.drf_class_bindings(body) {
            let name = self.symbols[method].name.text.as_str();
            if self.binding_depends_on_branch(method, END) {
                continue;
            }
            let method = match self.class_discovery_write(class, name, scope) {
                None => method,
                Some(mutations::MemberWriteValue::Replacement(Value::Callable(invocation)))
                    if !invocation.verified
                        && matches!(invocation.receiver, Receiver::Ordinary) =>
                {
                    invocation.target
                }
                Some(_) => continue,
            };
            let is_hook = match role {
                FrameworkClassRole::DrfViewSet => is_viewset_action(name),
                FrameworkClassRole::DrfSerializer => {
                    name == "validate"
                        || name
                            .strip_prefix("validate_")
                            .is_some_and(|field| writable.contains(field))
                }
                _ => false,
            };
            if is_hook
                && matches!(
                    self.symbols[method].binding,
                    Binding::Definition(definition, _)
                        if definition.kind == DefinitionKind::Function
                            && !definition.is_generator
                            && definition.decorators.is_empty()
                            && definition.parameters.len() >= 2
                )
            {
                self.edge(scope, method, PROTOCOL_DECLARATION, "possible");
            }
        }
    }

    fn drf_class_bindings(&self, body: ScopeId) -> Vec<SymbolId> {
        self.scopes[body]
            .names
            .values()
            .filter_map(|bindings| bindings.last().copied())
            .collect()
    }

    fn drf_field_binding(&mut self, field: SymbolId) -> Option<bool> {
        let symbol = &self.symbols[field];
        let Binding::Variable(Some(expression), before) = symbol.binding else {
            return None;
        };
        let ExpressionKind::Call {
            function,
            arguments,
            keywords,
        } = &expression.kind
        else {
            return None;
        };
        let scope = symbol.scope;
        if !matches!(self.probe_identity(scope, function, before), Value::Provider(name) if is_field(&name))
        {
            return None;
        }
        self.drf_field_writable(scope, before, arguments, keywords)
    }
}

pub(super) fn is_serializer_mutation(name: &str) -> bool {
    is_validator(name) || FIELD_LOOKUP.contains(&name)
}
