//! Bounded Sanic worker-listener registration and application selection.

use super::*;

pub(super) fn is_constructor(name: &str) -> bool {
    matches!(name, "sanic.Sanic" | "sanic.app.Sanic")
}

impl Graph<'_> {
    pub(super) fn retain_sanic_listener_parameters(
        &mut self,
        target: SymbolId,
        kind: &str,
    ) -> bool {
        if kind != "sanic_listener" {
            return false;
        }
        if let Binding::Definition(definition, scope) = self.symbols[target].binding {
            // Only bounded one- or two-argument listeners reach this path.
            self.read
                .extend(definition.parameters.iter().map(|parameter| {
                    self.by_location[&(scope, parameter.at.line, parameter.at.column)]
                }));
        }
        true
    }

    pub(super) fn sanic_constructor(
        &mut self,
        scope: ScopeId,
        at: Location,
        before: Location,
        arguments: &[Expression],
        keywords: &[(String, Expression)],
    ) -> Value {
        if let Some([Some(name)]) = registration_arguments(arguments, keywords, ["name"])
            && let Value::Text(name) = self.identity(scope, name, before, 0)
            && name.starts_with(|character: char| character.is_ascii_alphabetic())
            && name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        {
            Value::Object(
                (self.scopes[scope].file, at.line, at.column),
                ObjectKind::SanicApp,
            )
        } else {
            Value::Unknown
        }
    }

    pub(super) fn collect_sanic_cli_app(&mut self) {
        let mut selected = None;
        for file in 0..self.files.len() {
            if matches!(self.files[file].module.as_str(), "app" | "server")
                && let Some(binding) = self.local(file, "app", END)
                && !self.binding_depends_on_branch(binding, END)
                && let Value::Object(object, ObjectKind::SanicApp) = self.binding_value(binding, 0)
                && selected.replace((file, object)).is_some()
            {
                return;
            }
        }
        if let Some((file, object)) = selected {
            self.scopes[file].activated_apps.push(object);
        }
    }

    pub(super) fn register_sanic_listener(&mut self, object: Object, call: CallArguments<'_>) {
        let CallArguments {
            scope,
            before,
            at,
            positional,
            keywords,
        } = call;
        if let Some([Some(listener), Some(event)]) =
            registration_arguments(positional, keywords, ["listener", "event"])
            && let Value::Text(event) = self.identity(scope, event, before, 0)
            && matches!(
                event.to_ascii_lowercase().as_str(),
                "before_server_start"
                    | "after_server_start"
                    | "before_server_stop"
                    | "after_server_stop"
            )
            && let Value::Callable(invocation) = self.identity(scope, listener, before, 0)
            && matches!(invocation.receiver, Receiver::Ordinary)
            && matches!(self.symbols[invocation.target].binding, Binding::Definition(definition, _)
                if definition.kind != DefinitionKind::Class && !definition.is_generator
                    && definition.decorators.is_empty() && definition.simple_positional_parameters
                    && matches!(definition.parameters.len(), 1 | 2))
        {
            self.scopes[scope]
                .routes
                .push((object, invocation.target, "sanic_listener"));
        } else {
            self.scope_issue(
                scope,
                at,
                "unresolved_registration",
                "Sanic requires a supported worker event and listener",
            );
        }
    }
}
