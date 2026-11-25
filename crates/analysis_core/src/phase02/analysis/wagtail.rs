//! Declaration-time Wagtail registry and block metadata reads.

use super::*;

pub(super) const FIELD_DECLARATION: &str = "wagtail_field_declaration";
pub(super) const BLOCK_METADATA: &str = "wagtail_block_metadata";
pub(super) const MODEL_OPTION: &str = "wagtail_model_option";
pub(super) const CALLBACK_PARAMETER: &str = "wagtail_callback_parameter";
const SETTING_REGISTRATION: &str = "wagtail_setting_registration";
const SNIPPET_VIEWSET: &str = "wagtail.snippets.views.snippets.SnippetViewSet";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Declaration {
    Setting,
    StructBlock,
    StreamBlock,
    Page,
    Orderable,
    Task,
}

impl Declaration {
    fn is_block(self) -> bool {
        matches!(self, Self::StructBlock | Self::StreamBlock)
    }
}

fn declaration(provider: &str) -> Option<Declaration> {
    match provider {
        "wagtail.contrib.settings.models.BaseSiteSetting"
        | "wagtail.contrib.settings.models.BaseGenericSetting" => Some(Declaration::Setting),
        "wagtail.blocks.StructBlock" | "wagtail.blocks.struct_block.StructBlock" => {
            Some(Declaration::StructBlock)
        }
        "wagtail.blocks.StreamBlock" | "wagtail.blocks.stream_block.StreamBlock" => {
            Some(Declaration::StreamBlock)
        }
        "wagtail.models.Page" | "wagtail.models.pages.Page" => Some(Declaration::Page),
        "wagtail.models.Orderable" => Some(Declaration::Orderable),
        "wagtail.models.Task" | "wagtail.models.workflows.Task" => Some(Declaration::Task),
        _ => None,
    }
}

pub(super) fn is_setting_registrar(provider: &str) -> bool {
    matches!(
        provider,
        "wagtail.contrib.settings.models.register_setting"
            | "wagtail.contrib.settings.registry.register_setting"
    )
}

impl Graph<'_> {
    pub(super) fn wagtail_declaration_base(&self, provider: &str) -> bool {
        !self.modules.contains_key("wagtail")
            && !self.modules.contains_key("django")
            && declaration(provider).is_some()
    }

    pub(super) fn wagtail_plain_snippet_base(&self, provider: &str, subclass: &Definition) -> bool {
        // Wagtail 7.1 and 7.2 use type and object.__init_subclass__ for
        // SnippetViewSet. Registration consumes options on a viewset instance.
        provider == SNIPPET_VIEWSET
            && !self.modules.contains_key("wagtail")
            && !self.modules.contains_key("django")
            && subclass.bases.len() == 1
            && !subclass.has_class_keywords()
            && subclass.decorators.is_empty()
            && subclass.body_coverage_lines.is_empty()
            && body_without_docstring(subclass).iter().all(|statement| {
                let StatementKind::Assign {
                    names,
                    value: Some(value),
                    annotation: None,
                } = &statement.kind
                else {
                    return false;
                };
                names.iter().all(|name| !name.text.starts_with("__"))
                    && (is_snippet_literal(value)
                        || matches!(&value.kind, ExpressionKind::Sequence { items, .. }
                            if items.iter().all(is_snippet_literal)))
            })
    }

    pub(super) fn wagtail_snippet_provider_write(
        &mut self,
        scope: ScopeId,
        receiver: &Expression,
        before: Location,
    ) -> bool {
        matches!(self.probe_identity(scope, receiver, before), Value::Provider(provider)
            if matches!(provider.as_str(),
                SNIPPET_VIEWSET
                    | "wagtail.admin.viewsets.model.ModelViewSet"
                    | "wagtail.admin.viewsets.base.ViewSet"
                    | "wagtail.admin.menu.WagtailMenuRegisterable"
                    | "wagtail.snippets.views.snippets"))
    }

    fn wagtail_declaration(&mut self, class: SymbolId) -> Option<(Declaration, ScopeId)> {
        let Binding::Definition(definition, body) = self.symbols[class].binding else {
            return None;
        };
        let [base] = definition.bases.as_slice() else {
            return None;
        };
        if definition.kind != DefinitionKind::Class || definition.has_class_keywords() {
            return None;
        }
        let Value::Provider(provider) =
            self.probe_identity(self.symbols[class].scope, base, definition.name.at)
        else {
            return None;
        };
        if !self.wagtail_declaration_base(&provider) {
            return None;
        }
        Some((declaration(&provider)?, body))
    }

    pub(super) fn register_wagtail_setting(
        &mut self,
        scope: ScopeId,
        decorator: &Expression,
        class: SymbolId,
        arguments: &[Expression],
        keywords: &[(String, Expression)],
    ) {
        let options_are_bounded = match keywords {
            [] => true,
            [(name, icon)] => name == "icon" && matches!(icon.kind, ExpressionKind::Text(_)),
            _ => false,
        };
        let bounded_class = self.wagtail_declaration(class).is_some_and(|(role, body)| {
            role == Declaration::Setting
                && ![
                    "get_permission_policy",
                    "_meta",
                    "__getattribute__",
                    "__getattr__",
                ]
                .iter()
                .any(|member| self.scopes[body].names.contains_key(member))
                && self.bounded_wagtail_model_meta(body)
                && matches!(self.symbols[class].binding, Binding::Definition(definition, _)
                        if definition.decorators.len() == 1)
        });
        if arguments.is_empty() && options_are_bounded && bounded_class {
            // The decorator adds this class to the settings registry and
            // returns it unchanged. Admin menu and permission hooks run later.
            self.edge(scope, class, SETTING_REGISTRATION, "possible");
        } else {
            self.scope_issue(
                scope,
                decorator.at,
                "unresolved_registration",
                "Wagtail setting model or registration options could not be resolved",
            );
        }
    }

    fn bounded_wagtail_model_meta(&self, body: ScopeId) -> bool {
        self.local(body, "Meta", END).is_none_or(|meta| {
            matches!(self.symbols[meta].binding, Binding::Definition(definition, options)
                if definition.kind == DefinitionKind::Class
                    && !definition.has_class_keywords()
                    && definition.bases.is_empty()
                    && definition.decorators.is_empty()
                    && !self.binding_depends_on_branch(meta, END)
                    && !self.scopes[options].names.contains_key("abstract"))
        })
    }

    pub(super) fn collect_wagtail_declaration(&mut self, class: SymbolId) {
        let Some((role, body)) = self.wagtail_declaration(class) else {
            return;
        };
        let bindings: Vec<_> = self.scopes[body]
            .names
            .values()
            .filter_map(|bindings| self.effective_bindings(bindings).last().copied())
            .collect();
        for binding in bindings {
            if self.binding_depends_on_branch(binding, END) {
                continue;
            }
            if let Binding::Variable(Some(value), _) = self.symbols[binding].binding
                && self.wagtail_field_call(body, value, self.symbols[binding].name.at, role, 0)
            {
                self.edge(body, binding, FIELD_DECLARATION, "certain");
            }
        }
        if !role.is_block() {
            if role != Declaration::Setting && self.bounded_wagtail_model_meta(body) {
                self.collect_wagtail_model(class, body, role);
            }
            return;
        }
        let Some(meta) = self.local(body, "Meta", END) else {
            return;
        };
        let Binding::Definition(definition, meta_body) = self.symbols[meta].binding else {
            return;
        };
        if self.binding_depends_on_branch(meta, END)
            || definition.kind != DefinitionKind::Class
            || !definition.decorators.is_empty()
            || !definition.bases.is_empty()
            || definition.has_class_keywords()
        {
            return;
        }
        self.edge(body, meta, BLOCK_METADATA, "certain");
        for name in ["icon", "label", "label_format", "template"] {
            if let Some(member) = self.local(meta_body, name, END)
                && !self.binding_depends_on_branch(member, END)
                && matches!(self.symbols[member].binding, Binding::Variable(Some(value), _)
                    if matches!(value.kind, ExpressionKind::Text(_)))
            {
                self.edge(body, member, BLOCK_METADATA, "possible");
            }
        }
    }

    fn collect_wagtail_model(&mut self, class: SymbolId, body: ScopeId, role: Declaration) {
        let scope = self.symbols[class].scope;
        let Binding::Definition(definition, _) = self.symbols[class].binding else {
            return;
        };
        if !definition.decorators.is_empty()
            || self.local(scope, &definition.name.text, END) != Some(class)
            || self.binding_depends_on_branch(class, END)
            || self.scopes[body].names.contains_key("_meta")
        {
            return;
        }
        // Django's model metaclass records concrete models when the class is created.
        self.edge(body, class, "wagtail_model_declaration", "possible");
        if let Some(meta) = self.local(body, "Meta", END)
            && let Binding::Definition(_, options) = self.symbols[meta].binding
        {
            self.edge(body, meta, MODEL_OPTION, "certain");
            for name in ["verbose_name", "verbose_name_plural"] {
                if let Some(member) = self.local(options, name, END)
                    && !self.binding_depends_on_branch(member, END)
                    && matches!(self.symbols[member].binding, Binding::Variable(Some(value), _)
                        if matches!(value.kind, ExpressionKind::Text(_)))
                {
                    self.edge(body, member, MODEL_OPTION, "certain");
                }
            }
        }
        for name in match role {
            Declaration::Page => &[
                "content_panels",
                "max_count",
                "max_count_per_parent",
                "parent_page_types",
                "subpage_types",
            ][..],
            Declaration::Task => &["task_state_class"],
            _ => &[],
        } {
            let Some(member) = self.local(body, name, END) else {
                continue;
            };
            if let Binding::Variable(Some(value), _) = self.symbols[member].binding
                && !self.binding_depends_on_branch(member, END)
                && self.wagtail_model_option(body, name, value, self.symbols[member].name.at)
            {
                self.edge(body, member, MODEL_OPTION, "possible");
            }
        }
        match role {
            Declaration::Page
                if self.local(body, "serve_preview", END).is_none()
                    && self
                        .class_discovery_write(class, "serve_preview", scope)
                        .is_none() =>
            {
                self.collect_wagtail_callback(class, body, "get_preview_template");
            }
            Declaration::Task => {
                self.collect_wagtail_callback(class, body, "user_can_access_editor")
            }
            _ => {}
        }
    }

    fn collect_wagtail_callback(&mut self, class: SymbolId, body: ScopeId, name: &str) {
        let Some(method) = self.local(body, name, END) else {
            return;
        };
        let Binding::Definition(definition, method_scope) = self.symbols[method].binding else {
            return;
        };
        if definition.kind != DefinitionKind::Function
            || definition.is_generator
            || !definition.simple_positional_parameters
            || !definition.decorators.is_empty()
            || definition.parameters.len() != 3
            || self.binding_depends_on_branch(method, END)
            || self
                .class_discovery_write(class, name, self.symbols[class].scope)
                .is_some()
        {
            return;
        }
        self.edge(body, method, "wagtail_model_callback", "possible");
        for parameter in &definition.parameters[1..] {
            let binding = self.by_location[&(method_scope, parameter.at.line, parameter.at.column)];
            self.edge(body, binding, CALLBACK_PARAMETER, "possible");
        }
    }

    fn wagtail_model_option(
        &mut self,
        scope: ScopeId,
        name: &str,
        value: &Expression,
        before: Location,
    ) -> bool {
        match (name, &value.kind) {
            ("max_count" | "max_count_per_parent", ExpressionKind::Scalar(Some(value))) => *value >= 0,
            ("max_count" | "max_count_per_parent" | "task_state_class", ExpressionKind::None) => true,
            ("parent_page_types" | "subpage_types", ExpressionKind::Sequence { items, .. }) =>
                items.iter().all(|entry| matches!(entry.kind, ExpressionKind::Text(_))),
            ("content_panels", ExpressionKind::Addition(operands)) => {
                let [base, panels] = operands.as_slice() else {
                    return false;
                };
                matches!(self.probe_identity(scope, base, before), Value::Provider(provider)
                    if matches!(provider.as_str(), "wagtail.models.Page.content_panels" | "wagtail.models.pages.Page.content_panels"))
                    && self.wagtail_model_option(scope, name, panels, before)
            }
            ("content_panels", ExpressionKind::Sequence { items, list: true }) =>
                items.iter().all(|entry| {
                    let ExpressionKind::Call { function, arguments, keywords } = &entry.kind else {
                        return false;
                    };
                    keywords.is_empty()
                        && matches!(arguments.as_slice(), [field] if matches!(field.kind, ExpressionKind::Text(_)))
                        && matches!(self.probe_identity(scope, function, before),
                            Value::Provider(provider) if field_panel(&provider))
                }),
            _ => false,
        }
    }

    pub(super) fn wagtail_field_constructor(
        &mut self,
        call: CallArguments<'_>,
        provider: &str,
    ) -> bool {
        if !provider.starts_with("wagtail.blocks.")
            && !provider.starts_with("django.db.models.")
            && !provider.starts_with("wagtail.fields.")
            && provider != "modelcluster.fields.ParentalKey"
            && !field_panel(provider)
        {
            return false;
        }
        let Some(class) = self.scopes[call.scope].owner else {
            return false;
        };
        let Some((role, _)) = self.wagtail_declaration(class) else {
            return false;
        };
        if role == Declaration::Page && field_panel(provider) {
            return call.keywords.is_empty()
                && matches!(call.positional, [field] if matches!(field.kind, ExpressionKind::Text(_)));
        }
        self.wagtail_field_arguments(provider, call, role, 0)
    }

    fn wagtail_field_call(
        &mut self,
        scope: ScopeId,
        value: &Expression,
        before: Location,
        role: Declaration,
        depth: usize,
    ) -> bool {
        if depth >= MAX_BINDING_CHAIN {
            return false;
        }
        let ExpressionKind::Call {
            function,
            arguments,
            keywords,
        } = &value.kind
        else {
            return false;
        };
        let Value::Provider(provider) = self.probe_identity(scope, function, before) else {
            return false;
        };
        self.wagtail_field_arguments(
            &provider,
            CallArguments {
                scope,
                before,
                at: value.at,
                positional: arguments,
                keywords,
            },
            role,
            depth,
        )
    }

    fn wagtail_field_arguments(
        &mut self,
        provider: &str,
        call: CallArguments<'_>,
        role: Declaration,
        depth: usize,
    ) -> bool {
        match (role, provider, call.positional, call.keywords) {
            (
                Declaration::Setting
                | Declaration::Page
                | Declaration::Task
                | Declaration::Orderable,
                "django.db.models.CharField" | "django.db.models.fields.CharField",
                [],
                [(name, length)],
            ) => {
                name == "max_length"
                    && matches!(length.kind, ExpressionKind::Scalar(Some(value)) if value > 0)
            }
            (Declaration::Page, "wagtail.fields.RichTextField", [], [(name, blank)]) => {
                name == "blank" && matches!(blank.kind, ExpressionKind::Bool(_))
            }
            (
                Declaration::Page,
                "django.db.models.ForeignKey" | "django.db.models.fields.related.ForeignKey",
                [related],
                _,
            ) => {
                matches!(related.kind, ExpressionKind::Text(_))
                    && matches!(
                        registration_arguments(&[], call.keywords, ["null", "on_delete"]),
                        Some([Some(null), Some(delete)])
                        if matches!(null.kind, ExpressionKind::Bool(true))
                            && matches!(self.probe_identity(call.scope, delete, call.before),
                                Value::Provider(provider) if matches!(provider.as_str(),
                                    "django.db.models.SET_NULL" | "django.db.models.deletion.SET_NULL"))
                    )
            }
            (
                Declaration::Orderable,
                "modelcluster.fields.ParentalKey",
                [related],
                [(name, reverse)],
            ) => {
                !self.modules.contains_key("modelcluster")
                    && matches!(related.kind, ExpressionKind::Text(_))
                    && name == "related_name"
                    && matches!(&reverse.kind, ExpressionKind::Text(value) if !value.is_empty() && !value.ends_with('+'))
            }
            (
                Declaration::StructBlock | Declaration::StreamBlock,
                "wagtail.blocks.CharBlock" | "wagtail.blocks.field_block.CharBlock",
                [],
                [],
            ) => true,
            (
                Declaration::StructBlock | Declaration::StreamBlock,
                "wagtail.blocks.ListBlock" | "wagtail.blocks.list_block.ListBlock",
                [child],
                [],
            ) => self.wagtail_field_call(call.scope, child, call.before, role, depth + 1),
            _ => false,
        }
    }
}

fn is_snippet_literal(value: &Expression) -> bool {
    matches!(
        value.kind,
        ExpressionKind::Text(_)
            | ExpressionKind::Bool(_)
            | ExpressionKind::None
            | ExpressionKind::Scalar(_)
    )
}

fn field_panel(provider: &str) -> bool {
    matches!(
        provider,
        "wagtail.admin.panels.FieldPanel" | "wagtail.admin.panels.field_panel.FieldPanel"
    )
}
