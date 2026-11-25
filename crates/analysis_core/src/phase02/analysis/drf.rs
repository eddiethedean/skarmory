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

pub(super) fn is_method_field(provider: &str) -> bool {
    matches!(
        provider,
        "rest_framework.fields.SerializerMethodField"
            | "rest_framework.serializers.SerializerMethodField"
    )
}

pub(super) fn method_field_name<'a>(
    arguments: &'a [Expression],
    keywords: &'a [(String, Expression)],
) -> Option<Option<&'a str>> {
    match registration_arguments(arguments, keywords, ["method_name"])? {
        [
            None
            | Some(Expression {
                kind: ExpressionKind::None,
                ..
            }),
        ] => Some(None),
        [
            Some(Expression {
                kind: ExpressionKind::Text(name),
                ..
            }),
        ] if skarmory_python::simple_name(name) => Some(Some(name)),
        _ => None,
    }
}

enum DeclaredField {
    Plain { writable: bool },
    Method { callback: String },
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

    pub(super) fn drf_mixin_base(&mut self, class: SymbolId) -> Option<(ScopeId, SymbolId)> {
        let symbol = &self.symbols[class];
        let Binding::Definition(definition, _) = symbol.binding else {
            return None;
        };
        let [mixin, base] = definition.bases.as_slice() else {
            return None;
        };
        let (scope, before) = (symbol.scope, definition.name.at);
        let Value::Callable(mixin) = self.probe_identity(scope, mixin, before) else {
            return None;
        };
        let Binding::Definition(declaration, body) = self.symbols[mixin.target].binding else {
            return None;
        };
        if declaration.kind != DefinitionKind::Class
            || !declaration.bases.is_empty()
            || !declaration.decorators.is_empty()
            || declaration.table_keyword.is_some()
            || self.local(body, "__init_subclass__", END).is_some()
            || self.local(body, "_declared_fields", END).is_some()
        {
            return None;
        }
        let Value::Callable(base) = self.probe_identity(scope, base, before) else {
            return None;
        };
        if base.target == mixin.target || !self.is_class(base.target) {
            return None;
        }
        let (lineage, _) = self.class_lineage(base.target);
        lineage
            .iter()
            .all(|&scope| {
                self.scopes[scope].owner.is_some_and(|owner| {
                    matches!(self.symbols[owner].binding, Binding::Definition(definition, _) if definition.bases.len() <= 1)
                })
            })
            .then_some((body, base.target))
    }

    fn drf_declaration_lineage(&mut self, class: SymbolId) -> (Vec<ScopeId>, Option<ScopeId>) {
        let (mut lineage, _) = self.class_lineage(class);
        let mixin = lineage
            .last()
            .and_then(|&body| self.scopes[body].owner)
            .and_then(|owner| self.drf_mixin_base(owner));
        if let Some((mixin, base)) = mixin {
            lineage.push(mixin);
            lineage.extend(self.class_lineage(base).0);
            (lineage, Some(mixin))
        } else {
            (lineage, None)
        }
    }

    pub(super) fn collect_drf_declaration(
        &mut self,
        class: SymbolId,
        body: ScopeId,
        role: FrameworkClassRole,
    ) {
        let Some(scope) = self.public_protocol_module_scope(class) else {
            return;
        };
        self.edge(scope, class, PROTOCOL_DECLARATION, "possible");

        // Fields are collected by SerializerMetaclass at class creation.
        // Potential external dispatch retains the matching methods separately.
        let mut writable = BTreeSet::new();
        if role == FrameworkClassRole::DrfSerializer {
            let (lineage, mixin) = self.drf_declaration_lineage(class);
            let bounded_lookup = FIELD_LOOKUP.iter().all(|name| {
                self.class_discovery_write(class, name, scope).is_none()
                    && lineage
                        .iter()
                        .all(|&body| self.local(body, name, END).is_none())
            });
            let bounded_method_lookup = bounded_lookup
                && [
                    "_readable_fields",
                    "to_representation",
                    "__getattribute__",
                    "__getattr__",
                ]
                .iter()
                .all(|name| {
                    self.class_discovery_write(class, name, scope).is_none()
                        && lineage
                            .iter()
                            .all(|&body| self.local(body, name, END).is_none())
                });
            let mut names = BTreeSet::new();
            let mut method_callbacks = BTreeSet::new();
            for &ancestor in lineage.iter().filter(|&&body| Some(body) != mixin) {
                let bindings = self.drf_class_bindings(ancestor);
                for field in bindings {
                    let name = self.symbols[field].name.text.as_str();
                    if !names.insert(name) {
                        continue;
                    }
                    if self.binding_depends_on_branch(field, END) {
                        continue;
                    }
                    if let Some(declaration) = self.drf_field_binding(field) {
                        self.edge(scope, field, FIELD_DECLARATION, "certain");
                        match declaration {
                            DeclaredField::Plain { writable: true } if bounded_lookup => {
                                writable.insert(name);
                            }
                            DeclaredField::Method { callback } if bounded_method_lookup => {
                                method_callbacks.insert(callback);
                            }
                            _ => {}
                        }
                    }
                }
            }
            for callback in method_callbacks {
                if let Some(method) = lineage
                    .iter()
                    .find_map(|&body| self.local(body, &callback, END))
                    && self
                        .class_discovery_write(class, &callback, scope)
                        .is_none()
                    && self.drf_protocol_method(method).is_some_and(|definition| {
                        definition.positional_parameter_count >= 2
                            && definition.required_positional_parameter_count <= 2
                            && definition.required_keyword_only_parameters.is_empty()
                    })
                {
                    self.edge(scope, method, PROTOCOL_DECLARATION, "possible");
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
                && self
                    .drf_protocol_method(method)
                    .is_some_and(|definition| definition.parameters.len() >= 2)
            {
                self.edge(scope, method, PROTOCOL_DECLARATION, "possible");
            }
        }
    }

    fn drf_protocol_method(&self, method: SymbolId) -> Option<&Definition> {
        let Binding::Definition(definition, _) = self.symbols[method].binding else {
            return None;
        };
        (definition.kind == DefinitionKind::Function
            && !self.binding_depends_on_branch(method, END)
            && !definition.is_generator
            && definition.decorators.is_empty())
        .then_some(definition)
    }

    fn drf_class_bindings(&self, body: ScopeId) -> Vec<SymbolId> {
        self.scopes[body]
            .names
            .values()
            .filter_map(|bindings| bindings.last().copied())
            .collect()
    }

    fn drf_field_binding(&mut self, field: SymbolId) -> Option<DeclaredField> {
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
        let Value::Provider(provider) = self.probe_identity(scope, function, before) else {
            return None;
        };
        if is_method_field(&provider) {
            let method = method_field_name(arguments, keywords)?;
            Some(DeclaredField::Method {
                callback: method.map_or_else(
                    || format!("get_{}", self.symbols[field].name.text),
                    str::to_owned,
                ),
            })
        } else if is_field(&provider) {
            self.drf_field_writable(scope, before, arguments, keywords)
                .map(|writable| DeclaredField::Plain { writable })
        } else {
            None
        }
    }
}

pub(super) fn is_serializer_mutation(name: &str) -> bool {
    is_validator(name) || FIELD_LOOKUP.contains(&name)
}
