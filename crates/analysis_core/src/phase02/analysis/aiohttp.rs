//! Bounded aiohttp HTTP route table declarations.

use super::*;

pub(super) const DECLARATION: &str = "aiohttp_route_declaration";

pub(super) fn is_route_table(name: &str) -> bool {
    matches!(
        name,
        "aiohttp.web.RouteTableDef" | "aiohttp.web_routedef.RouteTableDef"
    )
}

pub(super) fn is_http_decorator(name: &str) -> bool {
    matches!(
        name,
        "get" | "post" | "put" | "patch" | "delete" | "head" | "options"
    )
}

impl Graph<'_> {
    pub(super) fn aiohttp_route_declaration(
        &mut self,
        scope: ScopeId,
        target: SymbolId,
        decorator: &Expression,
        before: Location,
    ) -> bool {
        if let ExpressionKind::Call {
            arguments,
            keywords,
            ..
        } = &decorator.kind
            && let Some([Some(path)]) = registration_arguments(arguments, keywords, ["path"])
            && matches!(self.identity(scope, path, before, 0), Value::Text(path) if path.starts_with('/'))
            && matches!(
                self.symbols[target].binding,
                Binding::Definition(definition, _)
                    if definition.kind == DefinitionKind::Coroutine
                        && definition.simple_positional_parameters
                        && definition.parameters.len() == 1
                        && definition.decorators.len() == 1
                        && !definition.is_generator
            )
        {
            // The decorator stores the handler in the route table. Serving
            // requests also requires attaching the table to a running app.
            self.edge(scope, target, DECLARATION, "certain");
            true
        } else {
            false
        }
    }
}
