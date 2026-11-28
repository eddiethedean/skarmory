use super::*;

impl Graph<'_> {
    pub(super) fn proven_guarded_project_import(
        &self,
        scope: ScopeId,
        at: Location,
        statement: &Statement,
        raises_import_error: bool,
    ) -> bool {
        let StatementKind::Import(import) = &statement.kind else {
            return false;
        };
        if scope != self.scopes[scope].file
            || !self.terminal_proofs
            || self.local(scope, "__builtins__", at).is_some()
            || raises_import_error
                && (import.local.text == "ImportError"
                    || self.local(scope, "ImportError", at).is_some())
        {
            return false;
        }
        let Some((module, member)) = self
            .import_module(scope, import)
            .zip(import.member.as_deref())
        else {
            return false;
        };
        let Some((package, child)) = module.rsplit_once('.') else {
            return false;
        };
        let Some(&source) = self.modules.get(module.as_str()) else {
            return false;
        };
        let selected = &self.files[source];
        source != scope
            && !selected.is_package
            && self.discovery.is_some_and(|project| {
                project.package_source_unambiguous(package, child, Some(&selected.path))
            })
            && module.match_indices('.').all(|(end, _)| {
                self.modules.get(&module[..end]).is_some_and(|&parent| self.files[parent].facts.body.is_empty())
            })
            && selected.facts.body.iter().all(passive_module_statement)
            && selected.facts.body.iter().filter(|statement| {
                matches!(&statement.kind, StatementKind::Define(def) if def.name.text == member)
            }).count() == 1
    }

    pub(super) fn settled_module_import_read(
        &mut self,
        scope: ScopeId,
        expression: &Expression,
        name: &str,
        replacement: SymbolId,
        depth: usize,
    ) -> Option<Value> {
        let file = self.scopes[scope].file;
        let reader = self.scopes[scope].owner?;
        let [initial, final_binding] = self.scopes[file].names.get(name)?.as_slice() else {
            return None;
        };
        if *final_binding != replacement
            || self.symbols[reader].scope != file
            || self.scopes[file].open_wildcard
            || self.has_module_binding_write(file, name)
        {
            return None;
        }
        if self.conditional_groups.contains_key(&replacement) {
            return self.settled_conditional_import_read(
                scope,
                reader,
                [*initial, replacement],
                depth,
            );
        }
        let Binding::Import(import) = self.symbols[*initial].binding else {
            return None;
        };
        let Binding::Definition(reader_def, _) = self.symbols[reader].binding else {
            return None;
        };
        if !self.quiet_callable_import(file, import) {
            return None;
        }
        let body: Vec<_> = self.initializer_statements(file).collect();
        let reader_index = definition_index(body.iter().copied(), reader_def)?;
        let import_index = body.iter().position(|statement| {
            matches!(&statement.kind, StatementKind::Import(actual) if std::ptr::eq(actual, import))
        })?;
        if let Binding::Variable(Some(value), at) = self.symbols[replacement].binding
            && matches!(&value.kind, ExpressionKind::Name(value) if value == name)
            && import_index < reader_index
            && body[import_index + 1..reader_index]
                .iter()
                .any(|statement| position(statement.at) == position(at))
        {
            // The right hand side of a self-copy reads the previous binding.
            return Some(self.binding(replacement, END, depth + 1));
        }
        let Binding::Definition(replacement_def, _) = self.symbols[replacement].binding else {
            return None;
        };
        let replacement_index = definition_index(body.iter().copied(), replacement_def)?;
        if import_index < replacement_index && replacement_index < reader_index {
            // A later direct definition cannot enter this reader before the
            // replacement is installed by the same module initializer.
            return Some(self.binding(replacement, END, depth + 1));
        }
        if !field_dispatch::plain_method(reader_def, 0) || !reader_def.annotations.is_empty() {
            return None;
        }
        let [
            Statement {
                kind: StatementKind::Return(Some(returned)),
                ..
            },
        ] = body_without_docstring(reader_def)
        else {
            return None;
        };
        let ExpressionKind::Call {
            function,
            arguments,
            keywords,
        } = &returned.kind
        else {
            return None;
        };
        if !std::ptr::eq(function.as_ref(), expression)
            || !keywords.is_empty()
            || !arguments.iter().all(closures::inert_default)
            || !field_dispatch::plain_method(replacement_def, arguments.len())
            || !replacement_def.annotations.is_empty()
        {
            return None;
        }

        let mut imports = 0;
        for statement in &body[..=reader_index.max(replacement_index)] {
            match &statement.kind {
                StatementKind::Import(row) if std::ptr::eq(row, import) => imports += 1,
                StatementKind::Define(definition)
                    if closures::passive_definition(definition)
                        && (std::ptr::eq(definition, replacement_def)
                            || (self.scopes[file].names[definition.name.text.as_str()].len()
                                == 1
                                && !self
                                    .has_module_binding_write(file, &definition.name.text))) => {}
                StatementKind::Evaluate(value) if closures::inert_default(value) => {}
                // Calls, decorators and overwrites can enter the reader
                // before the replacement, including through a finalizer.
                _ => return None,
            }
        }
        // No statement through both definitions can enter the reader, so
        // every execution of its body sees the replacement installed.
        (imports == 1).then(|| self.binding(replacement, END, depth + 1))
    }

    fn settled_conditional_import_read(
        &mut self,
        scope: ScopeId,
        reader: SymbolId,
        [first, last]: [SymbolId; 2],
        depth: usize,
    ) -> Option<Value> {
        let file = self.scopes[scope].file;
        let (import, fallback) = match (&self.symbols[first].binding, &self.symbols[last].binding) {
            (Binding::Import(import), Binding::Definition(fallback, _))
            | (Binding::Definition(fallback, _), Binding::Import(import)) => (*import, *fallback),
            _ => return None,
        };
        let Binding::Definition(reader_def, _) = self.symbols[reader].binding else {
            return None;
        };
        if fallback.kind != DefinitionKind::Function
            || !closures::passive_definition(fallback)
            || !self.quiet_callable_import(file, import)
        {
            return None;
        }
        let body = self.scopes[file].body;
        let branch_index = body.iter().position(|statement| {
            let StatementKind::If {
                test,
                body,
                otherwise,
            } = &statement.kind
            else {
                return false;
            };
            let ([then], [otherwise]) = (body.as_slice(), otherwise.as_slice()) else {
                return false;
            };
            let matching_arms = matches!((&then.kind, &otherwise.kind),
                (StatementKind::Import(actual), StatementKind::Define(definition))
                | (StatementKind::Define(definition), StatementKind::Import(actual))
                    if std::ptr::eq(actual, import) && std::ptr::eq(definition, fallback));
            matching_arms && self.stable_platform_comparison(file, test, statement.at)
        })?;
        let reader_index = definition_index(body.iter(), reader_def)?;
        for (index, statement) in body[..=branch_index.max(reader_index)].iter().enumerate() {
            if index == branch_index {
                continue;
            }
            match &statement.kind {
                StatementKind::Import(import)
                    if import.level == 0
                        && import.module == "sys"
                        && import.member.is_none()
                        && !import.wildcard_member
                        && self.scopes[file].names[import.local.text.as_str()].len() == 1
                        && !self.has_module_binding_write(file, &import.local.text)
                        && self.import_module(file, import).as_deref() == Some("sys")
                        && !self.modules.contains_key("sys")
                        && self
                            .imported_module_write("sys", (file, statement.at))
                            .is_none() => {}
                StatementKind::Define(definition)
                    if closures::passive_definition(definition)
                        && self.scopes[file].names[definition.name.text.as_str()].len() == 1
                        && !self.has_module_binding_write(file, &definition.name.text) => {}
                StatementKind::Evaluate(value) if closures::inert_default(value) => {}
                // The reader can escape or run as soon as its declaration is
                // evaluated. Require a quiet prefix through the whole branch.
                _ => return None,
            }
        }
        self.finite_conditional_value(last, END, depth + 1)
    }

    fn initializer_statements(&self, file: ScopeId) -> impl Iterator<Item = &Statement> + '_ {
        self.scopes[file].body.iter().flat_map(move |statement| {
            let (first, tail) = match &statement.kind {
                StatementKind::GuardedProjectImport {
                    import,
                    raises_import_error,
                    fallback,
                    otherwise,
                } if self.proven_guarded_project_import(
                    file,
                    statement.at,
                    import,
                    *raises_import_error,
                ) =>
                {
                    (
                        import.as_ref(),
                        if *raises_import_error {
                            fallback.as_slice()
                        } else {
                            otherwise.as_slice()
                        },
                    )
                }
                _ => (statement, &[][..]),
            };
            std::iter::once(first).chain(tail)
        })
    }

    fn stable_platform_comparison(&self, file: ScopeId, test: &Expression, at: Location) -> bool {
        let ExpressionKind::Compare {
            left,
            right,
            identity: false,
            ..
        } = &test.kind
        else {
            return false;
        };
        (matches!(right.kind, ExpressionKind::Text(_))
            && self.lazy_provider_member(file, left, at, "sys", "platform"))
            || (matches!(left.kind, ExpressionKind::Text(_))
                && self.lazy_provider_member(file, right, at, "sys", "platform"))
    }

    fn quiet_callable_import(&self, file: ScopeId, import: &Import) -> bool {
        let Some(module) = self.import_module(file, import) else {
            return false;
        };
        let Some(&source) = self.modules.get(module.as_str()) else {
            return false;
        };
        let Some(member) = import.member.as_deref() else {
            return false;
        };
        let Some(&[target]) = self.scopes[source].names.get(member).map(Vec::as_slice) else {
            return false;
        };
        source != file
            && !import.wildcard_member
            && !self.has_module_binding_write(source, member)
            && matches!(self.symbols[target].binding, Binding::Definition(definition, _)
                if definition.kind == DefinitionKind::Function)
            && self.scopes[source]
                .body
                .iter()
                .all(passive_module_statement)
            && module.match_indices('.').all(|(end, _)| {
                self.modules.get(&module[..end]).is_some_and(|&parent| {
                    self.scopes[parent]
                        .body
                        .iter()
                        .all(passive_module_statement)
                })
            })
    }
}

fn definition_index<'a>(
    body: impl IntoIterator<Item = &'a Statement>,
    expected: &Definition,
) -> Option<usize> {
    body.into_iter().position(|statement| {
        matches!(&statement.kind, StatementKind::Define(actual) if std::ptr::eq(actual, expected))
    })
}

fn passive_module_statement(statement: &Statement) -> bool {
    match &statement.kind {
        StatementKind::Define(definition) => closures::passive_definition(definition),
        StatementKind::Evaluate(value) => closures::inert_default(value),
        _ => false,
    }
}
