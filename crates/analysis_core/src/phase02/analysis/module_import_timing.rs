use super::*;

impl Graph<'_> {
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
        let Binding::Import(import) = self.symbols[*initial].binding else {
            return None;
        };
        let Binding::Definition(replacement_def, _) = self.symbols[replacement].binding else {
            return None;
        };
        let Binding::Definition(reader_def, _) = self.symbols[reader].binding else {
            return None;
        };
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
            || !self.quiet_callable_import(file, import)
        {
            return None;
        }

        let body = self.scopes[file].body;
        let reader_index = direct_definition_index(body, reader_def)?;
        let replacement_index = direct_definition_index(body, replacement_def)?;
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

fn direct_definition_index(body: &[Statement], expected: &Definition) -> Option<usize> {
    body.iter().position(|statement| {
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
