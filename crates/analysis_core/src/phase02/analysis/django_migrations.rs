//! Terminal migration initializers with proven callable and receiver identity.

use super::*;

impl Graph<'_> {
    pub(super) fn terminal_django_runpython(
        &mut self,
        scope: ScopeId,
        statement: &Statement,
    ) -> bool {
        let read = std::mem::take(&mut self.read);
        let failure = self.prove_runpython_failure(scope, statement);
        self.read = read;
        let Some(reads) = failure else {
            return false;
        };
        for (scope, expression, before) in reads {
            self.walk(scope, expression, before);
        }
        true
    }

    fn prove_runpython_failure<'b>(
        &mut self,
        scope: ScopeId,
        statement: &'b Statement,
    ) -> Option<Vec<(ScopeId, &'b Expression, Location)>> {
        let before = statement.at;
        let body = self.scopes[scope].body;
        let failure = if let Some((function, [argument], [])) = initializer_call(statement)
            && matches!(function.kind, ExpressionKind::Name(_))
            && matches!(argument.kind, ExpressionKind::Name(_))
            && let Value::Callable(invocation) = self.probe_identity(scope, function, before)
            && self.symbols[invocation.target].scope == scope
            && matches!(self.symbols[invocation.target].binding, Binding::Definition(class, _)
                if class.name.text == "RunPython" && terminal::plain_class(class))
        {
            // Python reads the argument before the empty class rejects it.
            // Reading a callable here cannot make its body reachable.
            vec![(scope, function, before), (scope, argument, before)]
        } else if let StatementKind::Define(definition) = &statement.kind {
            self.runpython_class_local_failure(scope, definition)?
        } else if let StatementKind::Assign {
            names,
            value: Some(value),
            annotation: None,
        } = &statement.kind
            && let [name] = names.as_slice()
            && self.unbound_module_name(scope, value, before)
            && let Some(prior) = self.local(scope, &name.text, before)
            && matches!(self.symbols[prior].binding, Binding::Definition(definition, _)
                if definition.kind == DefinitionKind::Function)
        {
            let index = body.iter().position(|item| std::ptr::eq(item, statement))?;
            let next = body.get(index + 1)?;
            let (function, arguments, keywords) = initializer_call(next)?;
            let [code, reverse, ..] = registration_arguments(
                arguments,
                keywords,
                ["code", "reverse_code", "atomic", "hints", "elidable"],
            )?;
            if ![code, reverse].into_iter().flatten().any(|argument| {
                matches!(&argument.kind, ExpressionKind::Name(argument) if argument == &name.text)
            }) || !matches!(self.probe_identity(scope, function, next.at),
                Value::Provider(provider) if is_django_runpython(&provider))
            {
                return None;
            }
            // The failed right-hand side never assigns the callback name.
            return self
                .closed_runpython_prefix(scope, &body[..index])
                .then(Vec::new);
        } else {
            return None;
        };
        let index = body.iter().position(|item| std::ptr::eq(item, statement))?;
        self.closed_runpython_prefix(scope, &body[..index])
            .then_some(failure)
    }

    fn runpython_class_local_failure<'b>(
        &mut self,
        scope: ScopeId,
        definition: &'b Definition,
    ) -> Option<Vec<(ScopeId, &'b Expression, Location)>> {
        if definition.kind != DefinitionKind::Class
            || !definition.bases.is_empty()
            || !definition.decorators.is_empty()
            || definition.table_keyword.is_some()
        {
            return None;
        }
        let [assignment, operation] = body_without_docstring(definition) else {
            return None;
        };
        let StatementKind::Assign {
            names,
            value: Some(value),
            annotation: None,
        } = &assignment.kind
        else {
            return None;
        };
        let (
            [name],
            ExpressionKind::Call {
                function: constructor,
                arguments,
                keywords,
            },
        ) = (names.as_slice(), &value.kind)
        else {
            return None;
        };
        let (function, _, _) = initializer_call(operation)?;
        let ExpressionKind::Member(receiver, method) = &function.kind else {
            return None;
        };
        if method != "RunPython"
            || !matches!(&receiver.kind, ExpressionKind::Name(receiver) if receiver == &name.text)
            || !arguments.is_empty()
            || !keywords.is_empty()
        {
            return None;
        }
        let symbol = self.by_location[&(scope, definition.name.at.line, definition.name.at.column)];
        let Binding::Definition(_, class) = self.symbols[symbol].binding else {
            return None;
        };
        if !matches!(self.probe_identity(class, constructor, assignment.at),
            Value::Provider(provider) if provider == "builtins.object")
        {
            return None;
        }
        // The fresh object has no attributes. Its lookup fails in the class
        // suite before evaluating call arguments or binding the class name.
        Some(vec![
            (class, value, assignment.at),
            (class, receiver, operation.at),
        ])
    }

    fn closed_runpython_prefix(&self, scope: ScopeId, prefix: &[Statement]) -> bool {
        !self.scopes[scope].open_wildcard
            && prefix.iter().all(|statement| match &statement.kind {
                StatementKind::Define(definition) => {
                    terminal::plain_class(definition)
                        || definition.kind == DefinitionKind::Function
                            && definition.decorators.is_empty()
                            && definition.annotations.is_empty()
                            && definition.defaults.is_empty()
                }
                StatementKind::Import(import) => {
                    import.level == 0
                        && !self.modules.contains_key("django")
                        && matches!(import.module.as_str(), "django.db" | "django.db.migrations")
                }
                StatementKind::Evaluate(Expression {
                    kind: ExpressionKind::Text(_),
                    ..
                }) => true,
                _ => false,
            })
    }
}

type InitializerCall<'a> = (&'a Expression, &'a [Expression], &'a [(String, Expression)]);

fn initializer_call(statement: &Statement) -> Option<InitializerCall<'_>> {
    let expression = match &statement.kind {
        StatementKind::Assign {
            value: Some(value),
            annotation: None,
            ..
        }
        | StatementKind::Evaluate(value) => value,
        _ => return None,
    };
    let ExpressionKind::Call {
        function,
        arguments,
        keywords,
    } = &expression.kind
    else {
        return None;
    };
    Some((function, arguments, keywords))
}
