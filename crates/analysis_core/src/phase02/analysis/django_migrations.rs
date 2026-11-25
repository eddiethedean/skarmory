//! Migration declarations and terminal initializers with proven provider identity.

use super::*;

pub(super) const DECLARATION: &str = "django_migration_declaration";

pub(super) fn is_migration_base(provider: &str) -> bool {
    matches!(
        provider,
        "django.db.migrations.Migration" | "django.db.migrations.migration.Migration"
    )
}

impl Graph<'_> {
    pub(super) fn collect_django_migration_declarations(&mut self) {
        // Configured custom migration packages need their own loader proof.
        if self
            .django_settings_sources
            .iter()
            .any(|&(_, settings)| self.local(settings, "MIGRATION_MODULES", END).is_some())
        {
            return;
        }
        for file in 0..self.files.len() {
            let source = &self.files[file];
            let Some((package, name)) = source.module.rsplit_once('.') else {
                continue;
            };
            let Some(app) = package.strip_suffix(".migrations") else {
                continue;
            };
            if source.is_package
                || name.starts_with(['_', '~'])
                || ![app, package].into_iter().all(|name| {
                    self.modules
                        .get(name)
                        .is_some_and(|&file| self.files[file].is_package)
                })
                || self.failed_terminal_modules.contains(&file)
                || self.scopes[file].open_wildcard
                || self.scopes[file]
                    .body
                    .iter()
                    .any(|statement| matches!(statement.kind, StatementKind::Raise(_)))
                || self
                    .module_member_write(&source.module, "Migration", (file, END))
                    .is_some()
            {
                continue;
            }
            let Some(export) = self.local(file, "Migration", END) else {
                continue;
            };
            let Some(binding) = self.probe_callable_binding(export) else {
                continue;
            };
            if self.binding_depends_on_branch(export, END)
                || !self.is_class(binding.target)
                || self.conditional.contains_key(&binding.target)
                || self.dormant_classes.contains(&binding.target)
            {
                continue;
            }
            let (lineage, _) = self.class_lineage(binding.target);
            let Some(&last) = lineage.last() else {
                continue;
            };
            let owner = self.scopes[last].owner.expect("class body has an owner");
            let symbol = &self.symbols[owner];
            let Binding::Definition(definition, _) = symbol.binding else {
                continue;
            };
            let [base] = definition.bases.as_slice() else {
                continue;
            };
            if !matches!(self.class_base(symbol.scope, base, definition.name.at, 0),
                Value::Provider(provider) if is_migration_base(&provider))
            {
                continue;
            }
            // Conventional discovery establishes a possible protocol use.
            // The declaration edge does not execute the migration or its callbacks.
            self.read.extend(binding.bindings);
            self.edge(file, binding.target, DECLARATION, "possible");
            for name in [
                "initial",
                "dependencies",
                "operations",
                "run_before",
                "replaces",
                "atomic",
            ] {
                for &body in &lineage {
                    let owner = self.scopes[body].owner.expect("class body has an owner");
                    if self.class_discovery_write(owner, name, file).is_some() {
                        break;
                    }
                    if let Some(field) = self.local(body, name, END) {
                        if !self.binding_depends_on_branch(field, END)
                            && matches!(self.symbols[field].binding, Binding::Variable(_, _))
                        {
                            self.edge(file, field, DECLARATION, "possible");
                        }
                        break;
                    }
                }
            }
        }
    }

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
