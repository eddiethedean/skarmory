//! Bounded Starlette HTTP route registration.

use super::*;

pub(super) fn is_constructor(name: &str) -> bool {
    name == "starlette.applications.Starlette"
}

impl Graph<'_> {
    pub(super) fn register_starlette_route(&mut self, object: Object, call: CallArguments<'_>) {
        let CallArguments {
            scope,
            before,
            at,
            positional,
            keywords,
        } = call;
        if let Some([Some(path), Some(handler), methods, name, include_in_schema]) =
            registration_arguments(
                positional,
                keywords,
                ["path", "route", "methods", "name", "include_in_schema"],
            )
            && matches!(self.identity(scope, path, before, 0), Value::Text(path) if path.starts_with('/'))
            && methods.is_none_or(|methods| {
                matches!(methods.kind, ExpressionKind::None)
                    || matches!(self.identity(scope, methods, before, 0), Value::Sequence(items)
                        if !items.is_empty() && items.iter().all(|item|
                            matches!(item, Value::Text(method) if !method.is_empty())))
            })
            && name.is_none_or(|name| {
                matches!(name.kind, ExpressionKind::None)
                    || matches!(self.identity(scope, name, before, 0), Value::Text(_))
            })
            && include_in_schema.is_none_or(|include| {
                matches!(self.identity(scope, include, before, 0), Value::Bool(_))
            })
            && let Value::Callable(invocation) = self.identity(scope, handler, before, 0)
            && !invocation.verified
            && matches!(invocation.receiver, Receiver::Ordinary)
            && matches!(
                self.symbols[invocation.target].binding,
                Binding::Definition(definition, _)
                    if matches!(definition.kind, DefinitionKind::Function | DefinitionKind::Coroutine)
                        && definition.simple_positional_parameters
                        && definition.parameters.len() == 1
                        && definition.decorators.is_empty()
                        && !definition.is_generator
            )
        {
            self.scopes[scope]
                .routes
                .push((object, invocation.target, "starlette_route"));
        } else {
            self.scope_issue(
                scope,
                at,
                "unresolved_registration",
                "Starlette add_route requires a supported route callable and path",
            );
        }
    }
}
