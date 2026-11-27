//! Matching a type-only import to the source module's bounded lazy getter.

use super::*;

pub(super) fn literal_import_temporary<'a>(
    prefix: &'a [Statement],
    result: &Expression,
) -> Option<&'a Name> {
    let [
        Statement {
            kind:
                StatementKind::Assign {
                    names,
                    value: Some(initializer),
                    annotation: None,
                },
            ..
        },
    ] = prefix
    else {
        return None;
    };
    let [temporary] = names.as_slice() else {
        return None;
    };
    let ExpressionKind::Call {
        arguments,
        keywords,
        ..
    } = &initializer.kind
    else {
        return None;
    };
    if !keywords.is_empty()
        || !matches!(
            arguments.as_slice(),
            [Expression {
                kind: ExpressionKind::Text(_),
                ..
            }]
        )
    {
        return None;
    }
    let ExpressionKind::Call {
        function,
        arguments,
        keywords,
    } = &result.kind
    else {
        return None;
    };
    (keywords.is_empty()
        && matches!(&function.kind, ExpressionKind::Name(name) if name == "getattr")
        && matches!(arguments.as_slice(), [Expression { kind: ExpressionKind::Name(name), .. }, _] if name == &temporary.text))
        .then_some(temporary)
}

impl Graph<'_> {
    pub(super) fn index_lazy_type_imports(&mut self) {
        for file in 0..self.files.len() {
            if !self.files[file].is_package || self.scopes[file].open_wildcard {
                continue;
            }
            let Some(hook) = self.local(file, "__getattr__", END) else {
                continue;
            };
            let Binding::Definition(definition, _) = self.symbols[hook].binding else {
                continue;
            };
            if !definition.decorators.is_empty()
                || definition.is_generator
                || !definition.simple_positional_parameters
                || self.scopes[file].names["__getattr__"].len() != 1
            {
                continue;
            }
            for statement in self.scopes[file].body {
                let StatementKind::If {
                    test,
                    body,
                    otherwise,
                } = &statement.kind
                else {
                    continue;
                };
                if !otherwise.is_empty()
                    || !self.lazy_provider_member(
                        file,
                        test,
                        statement.at,
                        "typing",
                        "TYPE_CHECKING",
                    )
                {
                    continue;
                }
                for statement in body {
                    let StatementKind::Import(import) = &statement.kind else {
                        continue;
                    };
                    let Some(&[binding]) = self.scopes[file]
                        .names
                        .get(import.local.text.as_str())
                        .map(Vec::as_slice)
                    else {
                        continue;
                    };
                    if position(self.symbols[binding].name.at) != position(import.local.at)
                        || !self.lazy_import_matches(file, hook, import)
                    {
                        continue;
                    }
                    self.lazy_type_imports.insert(binding);
                    self.read.insert(binding);
                }
            }
        }
    }

    fn lazy_import_matches(&self, file: ScopeId, hook: SymbolId, import: &Import) -> bool {
        let export = import.local.text.as_str();
        let Some(member) = import.member.as_deref() else {
            return false;
        };
        let LazyMember::Branch(branch) = self.lazy_lookup(hook, export, END) else {
            return false;
        };
        let ExpressionKind::Call {
            function,
            arguments,
            keywords,
        } = &branch.value.kind
        else {
            return false;
        };
        let [base, name] = arguments.as_slice() else {
            return false;
        };
        if !branch.imports.is_empty()
            || !keywords.is_empty()
            || !matches!(&function.kind, ExpressionKind::Name(name) if name == "getattr")
            || !matches!(
                self.lexical_binding(branch.scope, "getattr", branch.at, true),
                LexicalBinding::Missing
            )
            || self
                .module_member_write("builtins", "getattr", (branch.scope, branch.at))
                .is_some()
        {
            return false;
        }
        let attribute_matches = match &name.kind {
            ExpressionKind::Name(name) => branch.argument.is_some_and(|(id, attribute)| {
                attribute == member && self.lexical(branch.scope, name, branch.at) == Some(id)
            }),
            ExpressionKind::Text(attribute) => attribute == member,
            _ => false,
        };
        if !attribute_matches {
            return false;
        }
        let mut loader_before = branch.at;
        let loader = if let Some(temporary) = branch.temporary {
            let Some(binding) = self.local(branch.scope, &temporary.text, branch.at) else {
                return false;
            };
            let Binding::Variable(Some(value), at) = self.symbols[binding].binding else {
                return false;
            };
            loader_before = at;
            value
        } else {
            base
        };
        let ExpressionKind::Call {
            function,
            arguments,
            keywords,
        } = &loader.kind
        else {
            return false;
        };
        let [
            Expression {
                kind: ExpressionKind::Text(target),
                ..
            },
        ] = arguments.as_slice()
        else {
            return false;
        };
        let Some(&target_file) = self.modules.get(target.as_str()) else {
            return false;
        };
        keywords.is_empty()
            && target_file != file
            && self.lazy_provider_member(
                branch.scope,
                function,
                loader_before,
                "importlib",
                "import_module",
            )
            && self
                .import_module(file, import)
                .is_some_and(|source| source == *target)
            && target
                .match_indices('.')
                .all(|(end, _)| self.lazy_import_parent(target, end).is_some())
            && self.local(target_file, member, END).is_some_and(|id| {
                !self.binding_depends_on_branch(id, END)
                    && !matches!(
                        self.symbols[id].binding,
                        Binding::Deleted | Binding::Variable(None, _)
                    )
            })
    }

    fn lazy_provider_member(
        &self,
        scope: ScopeId,
        expression: &Expression,
        before: Location,
        module: &str,
        member: &str,
    ) -> bool {
        let (name, expected_member) = match &expression.kind {
            ExpressionKind::Name(name) => (name, Some(member)),
            ExpressionKind::Member(base, attribute) if attribute == member => {
                let ExpressionKind::Name(name) = &base.kind else {
                    return false;
                };
                (name, None)
            }
            _ => return false,
        };
        let LexicalBinding::Found(id) = self.lexical_binding(scope, name, before, true) else {
            return false;
        };
        let symbol = &self.symbols[id];
        let Binding::Import(import) = symbol.binding else {
            return false;
        };
        let at = if symbol.scope == scope { before } else { END };
        !self.binding_depends_on_branch(id, at)
            && self.scopes[symbol.scope].names[name.as_str()].len() == 1
            && import.level == 0
            && !import.wildcard_member
            && import.member.as_deref() == expected_member
            && self
                .import_module(self.scopes[symbol.scope].file, import)
                .is_some_and(|resolved| resolved == module)
            && !self.modules.contains_key(module)
            && self
                .imported_module_write(module, (scope, before))
                .is_none()
            && self
                .module_member_write(module, member, (scope, before))
                .is_none()
    }
}
