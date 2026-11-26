//! Bounded trait declarations and their callbacks on source-owned classes.

use super::*;

// Names read or written while setting up a Configurable with its own empty
// config. The declaration filter below separately excludes custom descriptors.
const CONSTRUCTOR_MEMBERS: &[&str] = &[
    "_add_notifiers",
    "_all_trait_default_generators",
    "_config_changed",
    "_config_default",
    "_cross_validation_lock",
    "_descriptors",
    "_find_my_config",
    "_get_trait_default_generator",
    "_instance_inits",
    "_load_config",
    "_notify_observers",
    "_parent_default",
    "_register_validator",
    "_remove_notifiers",
    "_static_immutable_initial_values",
    "_trait_default_generators",
    "_trait_notifiers",
    "_trait_validators",
    "_trait_values",
    "_traits",
    "config",
    "has_trait",
    "hold_trait_notifications",
    "log",
    "mro",
    "notify_change",
    "observe",
    "parent",
    "section_names",
    "setup_class",
    "setup_instance",
    "trait_defaults",
    "traits",
    "unobserve",
];

fn constructor_member(name: &str) -> bool {
    name.starts_with("__") || CONSTRUCTOR_MEMBERS.contains(&name)
}

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

    pub(super) fn bounded_traitlets_constructor(
        &mut self,
        class: SymbolId,
        call: CallArguments<'_>,
    ) -> bool {
        if !call.positional.is_empty() || !call.keywords.is_empty() {
            return false;
        }
        // The framework role has already verified single inheritance ending at
        // Configurable. Check every source-owned class along that inheritance.
        for body in self.class_lineage(class).0 {
            let Some(owner) = self.scopes[body].owner else {
                return false;
            };
            let Binding::Definition(definition, _) = self.symbols[owner].binding else {
                return false;
            };
            if definition.has_class_keywords()
                || !definition.decorators.is_empty()
                || self.has_unmodeled_class_write(owner)
                || !self.class_discovery_writes(owner, call.scope).is_empty()
                || self.scopes[body].names.values().any(|ids| ids.len() != 1)
            {
                return false;
            }
            for statement in &definition.body {
                let supported = match &statement.kind {
                    StatementKind::Evaluate(Expression {
                        kind: ExpressionKind::Text(_),
                        ..
                    }) => true,
                    StatementKind::Assign { names, .. } => {
                        matches!(names.as_slice(), [name]
                            if !name.text.starts_with('_')
                                && !constructor_member(&name.text)
                                && self.local_text_trait(body, &name.text).is_some())
                    }
                    StatementKind::Define(method)
                        if method.kind == DefinitionKind::Function
                            && method.defaults.is_empty()
                            && method.annotations.is_empty()
                            && !constructor_member(&method.name.text) =>
                    {
                        match method.decorators.as_slice() {
                            [] => true,
                            [decorator] => {
                                self.local(body, &method.name.text, END)
                                    .is_some_and(|target| {
                                        let ExpressionKind::Call { function, .. } = &decorator.kind
                                        else {
                                            return false;
                                        };
                                        matches!(
                                            self.probe_identity(body, function, method.name.at),
                                            Value::Provider(provider)
                                                if self.traitlets_callback_field(
                                                    body, decorator, target, &provider,
                                                )
                                                .is_some()
                                        )
                                    })
                            }
                            _ => false,
                        }
                    }
                    _ => false,
                };
                if !supported {
                    return false;
                }
            }
        }
        // Provider setup registers hooks and initializes an empty config. None
        // of the declared Unicode traits is read or assigned by this call.
        true
    }

    fn local_text_trait(&mut self, body: ScopeId, name: &str) -> Option<SymbolId> {
        let target = self.local(body, name, END)?;
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
        (arguments.is_empty()
            && keywords.is_empty()
            && matches!(self.probe_identity(body, function, before), Value::Provider(provider)
                if is_text_trait(&provider)))
        .then_some(target)
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
            if self.local(body, name, END).is_some() {
                return self.local_text_trait(body, name);
            }
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
