//! Declaration-time Wagtail registry and block metadata reads.

use super::*;

pub(super) const FIELD_DECLARATION: &str = "wagtail_field_declaration";
pub(super) const BLOCK_METADATA: &str = "wagtail_block_metadata";
const SETTING_REGISTRATION: &str = "wagtail_setting_registration";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Declaration {
    Setting,
    StructBlock,
    StreamBlock,
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

    fn wagtail_declaration(&mut self, class: SymbolId) -> Option<(Declaration, ScopeId)> {
        let Binding::Definition(definition, body) = self.symbols[class].binding else {
            return None;
        };
        let [base] = definition.bases.as_slice() else {
            return None;
        };
        if definition.kind != DefinitionKind::Class || definition.table_keyword.is_some() {
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
                && self.bounded_wagtail_settings_meta(body)
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

    fn bounded_wagtail_settings_meta(&self, body: ScopeId) -> bool {
        self.local(body, "Meta", END).is_none_or(|meta| {
            matches!(self.symbols[meta].binding, Binding::Definition(definition, options)
                if definition.kind == DefinitionKind::Class
                    && definition.table_keyword.is_none()
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
        if role == Declaration::Setting {
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
            || definition.table_keyword.is_some()
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

    pub(super) fn wagtail_field_constructor(
        &mut self,
        call: CallArguments<'_>,
        provider: &str,
    ) -> bool {
        if !provider.starts_with("wagtail.blocks.") && !provider.starts_with("django.db.models.") {
            return false;
        }
        let Some(class) = self.scopes[call.scope].owner else {
            return false;
        };
        let Some((role, _)) = self.wagtail_declaration(class) else {
            return false;
        };
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
                Declaration::Setting,
                "django.db.models.CharField" | "django.db.models.fields.CharField",
                [],
                [(name, length)],
            ) => {
                name == "max_length"
                    && matches!(length.kind, ExpressionKind::Scalar(Some(value)) if value > 0)
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
