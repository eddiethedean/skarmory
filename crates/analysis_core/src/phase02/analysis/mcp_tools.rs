//! Declaration-time FastMCP tool registration through bounded decorators.

use super::*;

pub(super) const DECORATOR_APPLICATION: &str = "bounded_decorator_application";
const REGISTRATION: &str = "mcp_tool_registration";

pub(super) fn is_server_provider(name: &str) -> bool {
    matches!(
        name,
        "mcp.server.fastmcp.FastMCP" | "mcp.server.fastmcp.server.FastMCP"
    )
}

#[derive(Default)]
struct Decoration {
    helpers: Vec<SymbolId>,
    registration: Option<(Object, String)>,
    wraps_result: bool,
}

struct Context<'a> {
    module: ScopeId,
    before: Location,
    callback: &'a str,
    callback_name: &'a str,
    label: Option<(&'a str, &'a str)>,
}

pub(super) struct Registration {
    scope: ScopeId,
    at: Location,
    target: SymbolId,
    conditional: bool,
}

impl Graph<'_> {
    pub(super) fn mcp_server(
        &mut self,
        scope: ScopeId,
        at: Location,
        arguments: &[Expression],
        keywords: &[(String, Expression)],
    ) -> Value {
        let Some([Some(name)]) = registration_arguments(arguments, keywords, ["name"]) else {
            return Value::Unknown;
        };
        if !matches!(&name.kind, ExpressionKind::Text(name) if !name.is_empty()) {
            return Value::Unknown;
        }
        Value::McpServer((self.scopes[scope].file, at.line, at.column))
    }

    pub(super) fn apply_bounded_tool_decorator(
        &mut self,
        scope: ScopeId,
        decorator: &Expression,
        target: SymbolId,
        before: Location,
    ) -> bool {
        let Some(decoration) = self.bounded_tool_decoration(scope, decorator, target, before, 0)
        else {
            return false;
        };
        for helper in decoration.helpers {
            self.edge(scope, helper, DECORATOR_APPLICATION, "certain");
        }
        if let Some(key) = decoration.registration {
            self.mcp_registrations
                .entry(key)
                .or_default()
                .push(Registration {
                    scope,
                    at: decorator.at,
                    target,
                    conditional: self.branch_depth != 0,
                });
        }
        true
    }

    pub(super) fn collect_mcp_registrations(&mut self) {
        for registrations in std::mem::take(&mut self.mcp_registrations).into_values() {
            let Some(first) = registrations
                .iter()
                .min_by_key(|registration| position(registration.at))
            else {
                continue;
            };
            if registrations.len() == 1
                || registrations.iter().all(|registration| {
                    registration.scope == first.scope
                        && registration.scope == self.scopes[registration.scope].file
                        && !registration.conditional
                })
            {
                // FastMCP keeps the first tool registered under a duplicate name.
                self.edge(first.scope, first.target, REGISTRATION, "possible");
                continue;
            }
            for registration in registrations {
                self.scope_issue(
                    registration.scope,
                    registration.at,
                    "unresolved_registration",
                    "FastMCP duplicate tool registration order is outside the bounded model",
                );
                self.edge(
                    registration.scope,
                    registration.target,
                    REGISTRATION,
                    "possible",
                );
            }
        }
    }

    pub(super) fn bounded_decorator_replaces_function(
        &mut self,
        scope: ScopeId,
        decorator: &Expression,
        target: SymbolId,
        before: Location,
        depth: usize,
    ) -> bool {
        self.bounded_tool_decoration(scope, decorator, target, before, depth)
            .is_some_and(|decoration| decoration.wraps_result)
    }

    fn bounded_tool_decoration(
        &mut self,
        scope: ScopeId,
        decorator: &Expression,
        target: SymbolId,
        before: Location,
        depth: usize,
    ) -> Option<Decoration> {
        if depth >= MAX_BINDING_CHAIN || scope != self.scopes[scope].file {
            return None;
        }
        let Binding::Definition(decorated, _) = self.symbols[target].binding else {
            return None;
        };
        if decorated.decorators.len() != 1 || decorated.kind == DefinitionKind::Class {
            return None;
        }
        let context = Context {
            module: scope,
            before,
            callback: "",
            callback_name: &decorated.name.text,
            label: None,
        };
        if let Some(registration) = self.mcp_tool_factory(decorator, &context, depth + 1) {
            return Some(Decoration {
                registration: Some(registration),
                ..Decoration::default()
            });
        }
        if self.cache_factory(decorator, &context, depth + 1) {
            return Some(Decoration {
                wraps_result: true,
                ..Decoration::default()
            });
        }
        let (function, label) = match &decorator.kind {
            ExpressionKind::Call {
                function,
                arguments,
                keywords,
            } if keywords.is_empty() => {
                let [argument] = arguments.as_slice() else {
                    return None;
                };
                let ExpressionKind::Text(label) = &argument.kind else {
                    return None;
                };
                (function.as_ref(), Some(label.as_str()))
            }
            ExpressionKind::Name(_) | ExpressionKind::Member(..) => (decorator, None),
            _ => return None,
        };
        let Value::Callable(invocation) = self.identity(scope, function, before, depth + 1) else {
            return None;
        };
        if invocation.verified || !matches!(invocation.receiver, Receiver::Ordinary) {
            return None;
        }
        let symbol = &self.symbols[invocation.target];
        let module = symbol.scope;
        if module != self.scopes[module].file || self.failed_terminal_modules.contains(&module) {
            return None;
        }
        let Binding::Definition(definition, body) = symbol.binding else {
            return None;
        };
        let parameter = one_argument(definition)?;
        let mut helpers = vec![invocation.target];
        let before = if module == scope { before } else { END };
        let (definition, body, label) = if let Some(label) = label {
            let (inner, _) = returned_wrapper(definition)?;
            let target = self.by_location[&(body, inner.name.at.line, inner.name.at.column)];
            let Binding::Definition(_, inner_body) = self.symbols[target].binding else {
                return None;
            };
            helpers.push(target);
            (inner, inner_body, Some((parameter, label)))
        } else {
            (definition, body, None)
        };
        let context = Context {
            module,
            before,
            callback: one_argument(definition)?,
            callback_name: &decorated.name.text,
            label,
        };
        let mut decoration = self.mcp_wrapper_effect(definition, body, &context, depth + 1)?;
        helpers.append(&mut decoration.helpers);
        decoration.helpers = helpers;
        Some(decoration)
    }

    fn mcp_wrapper_effect(
        &mut self,
        definition: &Definition,
        body: ScopeId,
        context: &Context<'_>,
        depth: usize,
    ) -> Option<Decoration> {
        let statements = body_without_docstring(definition);
        let callback = context.callback;
        if let [
            Statement {
                kind: StatementKind::Return(Some(value)),
                ..
            },
        ] = statements
            && let Some(factory) = applies_to(value, callback)
        {
            let registration = self.mcp_tool_factory(factory, context, depth);
            let cached = self.cache_factory(factory, context, depth);
            return (registration.is_some() || cached).then_some(Decoration {
                registration,
                wraps_result: cached,
                ..Decoration::default()
            });
        }
        if let [
            Statement {
                kind: StatementKind::Evaluate(value),
                ..
            },
            Statement {
                kind: StatementKind::Return(Some(result)),
                ..
            },
        ] = statements
            && name_is(result, callback)
            && let Some(factory) = applies_to(value, callback)
            && let Some(registration) = self.mcp_tool_factory(factory, context, depth)
        {
            return Some(Decoration {
                registration: Some(registration),
                ..Decoration::default()
            });
        }
        let [
            Statement {
                kind: StatementKind::Define(wrapper),
                ..
            },
            Statement {
                kind: StatementKind::Return(Some(value)),
                ..
            },
        ] = statements
        else {
            return None;
        };
        if wrapper.kind != DefinitionKind::Function
            || wrapper.is_generator
            || !wrapper.annotations.is_empty()
            || !wrapper.defaults.is_empty()
            || !wrapper.has_variadic_positional_parameter
            || !wrapper.has_variadic_keyword_parameter
        {
            return None;
        }
        let decorators = wrapper.decorators.as_slice();
        let registration = if let Some(factory) = applies_to(value, &wrapper.name.text)
            && matches!(decorators, [wraps] if self.wraps_callback(wraps, context, depth))
        {
            self.mcp_tool_factory(factory, context, depth)
        } else if name_is(value, &wrapper.name.text) {
            match decorators {
                [] => None,
                [wraps] if self.wraps_callback(wraps, context, depth) => None,
                [factory, wraps] if self.wraps_callback(wraps, context, depth) => {
                    Some(self.mcp_tool_factory(factory, context, depth)?)
                }
                _ => return None,
            }
        } else {
            return None;
        };
        let registers = registration.is_some();
        if registers && !self.proven_forwarder(wrapper, context, depth)
            || !registers
                && (!name_is(value, &wrapper.name.text) || !has_passive_forwarder(wrapper))
        {
            return None;
        }
        let wrapper_id = self.by_location[&(body, wrapper.name.at.line, wrapper.name.at.column)];
        Some(Decoration {
            helpers: registers.then_some(wrapper_id).into_iter().collect(),
            registration,
            wraps_result: true,
        })
    }

    fn mcp_tool_factory(
        &mut self,
        expression: &Expression,
        context: &Context<'_>,
        depth: usize,
    ) -> Option<(Object, String)> {
        let ExpressionKind::Call {
            function,
            arguments,
            keywords,
        } = &expression.kind
        else {
            return None;
        };
        let ExpressionKind::Member(receiver, member) = &function.kind else {
            return None;
        };
        if member != "tool" || !matches!(receiver.kind, ExpressionKind::Name(_)) {
            return None;
        }
        let Value::McpServer(server) =
            self.identity(context.module, receiver, context.before, depth)
        else {
            return None;
        };
        if !self.mcp_tool_available(server, (context.module, context.before)) {
            return None;
        }
        let [name, description] =
            registration_arguments(arguments, keywords, ["name", "description"])?;
        if !description.is_none_or(|value| {
            matches!(value.kind, ExpressionKind::Text(_) | ExpressionKind::None)
        }) {
            return None;
        }
        let name = match name.map(|name| &name.kind) {
            None | Some(ExpressionKind::None) => context.callback_name,
            Some(ExpressionKind::Text(name)) => name,
            Some(ExpressionKind::Name(name)) => {
                let (parameter, label) = context.label?;
                if name != parameter {
                    return None;
                }
                label
            }
            Some(ExpressionKind::Member(receiver, member))
                if member == "__name__" && name_is(receiver, context.callback) =>
            {
                context.callback_name
            }
            _ => return None,
        };
        let name = if name.is_empty() {
            context.callback_name
        } else {
            name
        };
        Some((server, name.to_owned()))
    }

    fn wraps_callback(
        &mut self,
        expression: &Expression,
        context: &Context<'_>,
        depth: usize,
    ) -> bool {
        matches!(&expression.kind, ExpressionKind::Call { function, arguments, keywords }
            if keywords.is_empty()
            && matches!(arguments.as_slice(), [argument] if name_is(argument, context.callback))
            && matches!(self.identity(context.module, function, context.before, depth),
                Value::Provider(name) if name == "functools.wraps"))
    }

    fn cache_factory(
        &mut self,
        expression: &Expression,
        context: &Context<'_>,
        depth: usize,
    ) -> bool {
        let ExpressionKind::Call {
            function,
            arguments,
            keywords,
        } = &expression.kind
        else {
            return false;
        };
        matches!(self.identity(context.module, function, context.before, depth),
            Value::Provider(name) if name == "functools.lru_cache")
            && registration_arguments(arguments, keywords, ["maxsize", "typed"]).is_some_and(
                |[maxsize, typed]| {
                    maxsize.is_none_or(|value| {
                        matches!(
                            value.kind,
                            ExpressionKind::None | ExpressionKind::Scalar(Some(_))
                        )
                    }) && typed.is_none_or(|value| matches!(value.kind, ExpressionKind::Bool(_)))
                },
            )
    }

    fn proven_forwarder(
        &mut self,
        wrapper: &Definition,
        context: &Context<'_>,
        depth: usize,
    ) -> bool {
        let [args, kwargs] = wrapper.parameters.as_slice() else {
            return false;
        };
        if !wrapper.has_variadic_positional_parameter || !wrapper.has_variadic_keyword_parameter {
            return false;
        }
        let [
            Statement {
                kind: StatementKind::Return(Some(value)),
                ..
            },
        ] = body_without_docstring(wrapper)
        else {
            return false;
        };
        let value = match &value.kind {
            ExpressionKind::Call {
                function,
                arguments,
                keywords,
            } if keywords.is_empty()
                && matches!(self.identity(context.module, function, context.before, depth),
                        Value::Provider(name) if name == "builtins.str") =>
            {
                let [argument] = arguments.as_slice() else {
                    return false;
                };
                argument
            }
            _ => value,
        };
        matches!(&value.kind, ExpressionKind::ForwardingCall { function, positional, keywords }
            if name_is(function, context.callback)
            && name_is(positional, &args.text)
            && name_is(keywords, &kwargs.text))
    }
}

fn name_is(expression: &Expression, expected: &str) -> bool {
    matches!(&expression.kind, ExpressionKind::Name(name) if name == expected)
}

fn has_passive_forwarder(wrapper: &Definition) -> bool {
    let [args, kwargs] = wrapper.parameters.as_slice() else {
        return false;
    };
    matches!(body_without_docstring(wrapper).last(),
        Some(Statement {
            kind: StatementKind::Return(Some(Expression {
                kind: ExpressionKind::ForwardingCall { positional, keywords, .. },
                ..
            })),
            ..
        }) if name_is(positional, &args.text) && name_is(keywords, &kwargs.text))
}

fn one_argument(definition: &Definition) -> Option<&str> {
    let [parameter] = definition.parameters.as_slice() else {
        return None;
    };
    (definition.kind == DefinitionKind::Function
        && definition.simple_positional_parameters
        && !definition.is_generator
        && definition.decorators.is_empty()
        && definition.annotations.is_empty()
        && definition.defaults.is_empty())
    .then_some(parameter.text.as_str())
}

fn returned_wrapper(definition: &Definition) -> Option<(&Definition, &Expression)> {
    let [
        Statement {
            kind: StatementKind::Define(wrapper),
            ..
        },
        Statement {
            kind: StatementKind::Return(Some(value)),
            ..
        },
    ] = body_without_docstring(definition)
    else {
        return None;
    };
    name_is(value, &wrapper.name.text).then_some((wrapper, value))
}

fn applies_to<'a>(expression: &'a Expression, name: &str) -> Option<&'a Expression> {
    let ExpressionKind::Call {
        function,
        arguments,
        keywords,
    } = &expression.kind
    else {
        return None;
    };
    (keywords.is_empty() && matches!(arguments.as_slice(), [value] if name_is(value, name)))
        .then_some(function.as_ref())
}
