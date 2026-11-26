//! Bounded trait declarations and their callbacks on source-owned classes.

use super::*;

fn traitlets_member(provider: &str) -> Option<&str> {
    provider
        .strip_prefix("traitlets.traitlets.")
        .or_else(|| provider.strip_prefix("traitlets."))
}

pub(super) fn tracks_provider_member(provider: &str) -> bool {
    matches!(
        traitlets_member(provider),
        Some("Unicode" | "default" | "validate" | "observe")
    )
}

pub(super) fn is_text_trait(provider: &str) -> bool {
    traitlets_member(provider) == Some("Unicode")
}

impl Graph<'_> {
    fn is_traitlets_class(&mut self, class: SymbolId) -> bool {
        self.framework_class_role(class, &mut BTreeSet::new(), 0)
            == Some(FrameworkClassRole::TraitletsConfigurable)
    }

    pub(super) fn traitlets_text_declaration(&mut self, call: CallArguments<'_>) -> bool {
        call.positional.is_empty()
            && call.keywords.is_empty()
            && self.scopes[call.scope]
                .owner
                .is_some_and(|class| self.is_traitlets_class(class))
            && self.scopes[call.scope].body.iter().any(|statement| {
                matches!(&statement.kind, StatementKind::Assign { value: Some(value), .. }
                    if position(value.at) == position(call.at))
            })
    }

    fn declared_text_trait(
        &mut self,
        class: SymbolId,
        name: &str,
        scope: ScopeId,
    ) -> Option<SymbolId> {
        for body in self.class_lineage(class).0 {
            let owner = self.scopes[body].owner?;
            if self.has_unmodeled_class_write(owner)
                || self.class_discovery_write(owner, name, scope).is_some()
            {
                return None;
            }
            let Some(target) = self.local(body, name, END) else {
                continue;
            };
            if self.binding_depends_on_branch(target, END) {
                return None;
            }
            let Binding::Variable(Some(value), before) = self.symbols[target].binding else {
                return None;
            };
            let ExpressionKind::Call {
                function,
                arguments,
                keywords,
            } = &value.kind
            else {
                return None;
            };
            return (arguments.is_empty()
                && keywords.is_empty()
                && matches!(self.probe_identity(body, function, before), Value::Provider(provider)
                    if is_text_trait(&provider)))
            .then_some(target);
        }
        None
    }

    fn traitlets_callback_field(
        &mut self,
        scope: ScopeId,
        decorator: &Expression,
        target: SymbolId,
        provider: &str,
    ) -> Option<SymbolId> {
        let hook = traitlets_member(provider)?;
        let parameters = match hook {
            "default" => 1,
            "validate" | "observe" => 2,
            _ => return None,
        };
        let ExpressionKind::Call {
            arguments,
            keywords,
            ..
        } = &decorator.kind
        else {
            return None;
        };
        let [
            Expression {
                kind: ExpressionKind::Text(name),
                ..
            },
        ] = arguments.as_slice()
        else {
            return None;
        };
        let Binding::Definition(definition, _) = self.symbols[target].binding else {
            return None;
        };
        if !keywords.is_empty()
            || definition.decorators.len() != 1
            || definition.kind != DefinitionKind::Function
            || definition.is_generator
            || !definition.simple_positional_parameters
            || !definition.defaults.is_empty()
            || definition.parameters.len() != parameters
        {
            return None;
        }
        let class = self.scopes[scope].owner?;
        if !self.is_traitlets_class(class) {
            return None;
        }
        if hook == "default" {
            let legacy = format!("_{name}_default");
            if self
                .class_lineage(class)
                .0
                .into_iter()
                .any(|body| self.local(body, &legacy, END).is_some())
            {
                return None;
            }
        }
        self.declared_text_trait(class, name, scope)
    }

    pub(super) fn record_traitlets_decorator(
        &mut self,
        scope: ScopeId,
        decorator: &Expression,
        target: SymbolId,
        provider: &str,
    ) -> bool {
        let Some(field) = self.traitlets_callback_field(scope, decorator, target, provider) else {
            return false;
        };
        for declaration in [target, field] {
            self.edge(
                scope,
                declaration,
                frameworks::CLASS_ATTRIBUTE_DECLARATION,
                "possible",
            );
        }
        true
    }

    pub(super) fn traitlets_replaces_function(
        &mut self,
        scope: ScopeId,
        decorator: &Expression,
        target: SymbolId,
        before: Location,
        depth: usize,
    ) -> bool {
        let ExpressionKind::Call { function, .. } = &decorator.kind else {
            return false;
        };
        matches!(self.identity(scope, function, before, depth), Value::Provider(provider)
            if self.traitlets_callback_field(scope, decorator, target, &provider).is_some())
    }

    pub(super) fn collect_traitlets_legacy_defaults(&mut self, class: SymbolId, body: ScopeId) {
        let bindings: Vec<_> = self.scopes[body]
            .names
            .values()
            .filter_map(|ids| ids.last().copied())
            .collect();
        for target in bindings {
            let symbol = &self.symbols[target];
            let Some(name) = symbol
                .name
                .text
                .strip_prefix('_')
                .and_then(|name| name.strip_suffix("_default"))
            else {
                continue;
            };
            if !matches!(symbol.binding, Binding::Definition(definition, _)
                if field_dispatch::plain_method(definition, 1))
                || self.binding_depends_on_branch(target, END)
            {
                continue;
            }
            let name = name.to_owned();
            if let Some(field) = self.declared_text_trait(class, &name, body) {
                for declaration in [target, field] {
                    self.edge(
                        body,
                        declaration,
                        frameworks::CLASS_ATTRIBUTE_DECLARATION,
                        "possible",
                    );
                }
            }
        }
    }
}
