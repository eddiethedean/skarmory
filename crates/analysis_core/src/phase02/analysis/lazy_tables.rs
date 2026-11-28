//! Static dispatch through a private literal PEP 562 lookup table.

use super::*;

const MAX_LAZY_TABLE_ROWS: usize = 64;

#[derive(Clone, Copy)]
pub(super) struct LazyTableTarget<'a> {
    pub(super) module: &'a str,
    pub(super) attribute: &'a str,
    pub(super) table: SymbolId,
    pub(super) provider: Option<SymbolId>,
}

pub(super) struct LazyTable<'a> {
    pub(super) branches: Vec<(&'a str, LazyBranch<'a>)>,
    pub(super) fallback: LazyMember<'a>,
}

fn expression_name(expression: &Expression, expected: &str) -> bool {
    matches!(&expression.kind, ExpressionKind::Name(name) if name == expected)
}

impl<'a> Graph<'a> {
    pub(super) fn proven_lazy_table(
        &self,
        scope: ScopeId,
        hook: &'a Definition,
        parameter: &Name,
    ) -> Option<LazyTable<'a>> {
        let file = self.scopes[scope].file;
        if !hook.simple_positional_parameters
            || !hook.decorators.is_empty()
            || !self.lazy_table_annotations(file, hook)
            || !hook.defaults.is_empty()
            || hook.is_generator
            || self.scopes[scope].open_wildcard
            || self.scopes[file].open_wildcard
            || self.exposed_lazy_tables.contains(&file)
            // An explicit member or reflective string can acquire the table
            // through a module object outside the static import index.
            || self.explicit_lazy_table_access
        {
            return None;
        }
        let [
            lookup,
            Statement {
                kind: StatementKind::Return(Some(value)),
                at,
            },
        ] = hook.body.as_slice()
        else {
            return None;
        };
        let (names, lookup_value, fallback) = match &lookup.kind {
            StatementKind::AssignUnpack { names, value } => (names, value, LazyMember::Unknown),
            StatementKind::GuardedLazyUnpack {
                names,
                value,
                key_error_at,
                attribute_error_at,
            } if self.unshadowed_lazy_builtin(scope, "KeyError", *key_error_at)
                && self.unshadowed_lazy_builtin(scope, "AttributeError", *attribute_error_at) =>
            {
                (names, value, LazyMember::Missing)
            }
            _ => return None,
        };
        let [module, attribute] = names.as_slice() else {
            return None;
        };
        let ExpressionKind::Subscript {
            value: table,
            index: key,
        } = &lookup_value.kind
        else {
            return None;
        };
        if module.text == attribute.text
            || !expression_name(table, "_LAZY")
            || !expression_name(key, &parameter.text)
            || names
                .iter()
                .any(|name| matches!(name.text.as_str(), "_LAZY" | "__builtins__"))
        {
            return None;
        }
        let table = self.lexical(scope, "_LAZY", lookup.at)?;
        let Binding::Variable(
            Some(Expression {
                kind: ExpressionKind::Dictionary(items),
                ..
            }),
            assignment,
        ) = self.symbols[table].binding
        else {
            return None;
        };
        if self.symbols[table].scope != file
            || self.scopes[file].names["_LAZY"].len() != 1
            || position(assignment) >= position(hook.name.at)
            || !self.stable_lazy_table_owner(file, hook, assignment)
            || self
                .module_member_write(&self.files[file].module, "_LAZY", (file, END))
                .is_some()
        {
            return None;
        }
        let ExpressionKind::Call {
            function: getattr,
            arguments,
            keywords,
        } = &value.kind
        else {
            return None;
        };
        let [loaded, name] = arguments.as_slice() else {
            return None;
        };
        if !keywords.is_empty()
            || !expression_name(getattr, "getattr")
            || !self.unshadowed_lazy_builtin(scope, "getattr", *at)
            || !expression_name(name, &attribute.text)
        {
            return None;
        }
        let ExpressionKind::Call {
            function: loader,
            arguments,
            keywords,
        } = &loaded.kind
        else {
            return None;
        };
        let [target] = arguments.as_slice() else {
            return None;
        };
        if !expression_name(target, &module.text) {
            return None;
        }
        let provider = if expression_name(loader, "__import__") {
            let [(keyword, fromlist)] = keywords.as_slice() else {
                return None;
            };
            let ExpressionKind::Sequence { items, .. } = &fromlist.kind else {
                return None;
            };
            let [item] = items.as_slice() else {
                return None;
            };
            if keyword != "fromlist"
                || !expression_name(item, &attribute.text)
                || !self.unshadowed_lazy_builtin(scope, "__import__", *at)
            {
                return None;
            }
            None
        } else if keywords.is_empty()
            && self.lazy_provider_member(scope, loader, *at, "importlib", "import_module")
        {
            let provider = match &loader.kind {
                ExpressionKind::Name(name) => name,
                ExpressionKind::Member(base, _) => match &base.kind {
                    ExpressionKind::Name(name) => name,
                    _ => return None,
                },
                _ => return None,
            };
            Some(self.lexical(scope, provider, *at)?)
        } else {
            return None;
        };
        let (rows, remainder) = items.as_chunks::<2>();
        if !remainder.is_empty() || rows.len() > MAX_LAZY_TABLE_ROWS {
            return None;
        }
        let mut names = BTreeSet::new();
        let mut branches = Vec::with_capacity(rows.len());
        for [export, target] in rows {
            let (ExpressionKind::Text(export), ExpressionKind::Sequence { items, list: false }) =
                (&export.kind, &target.kind)
            else {
                return None;
            };
            let [module, attribute] = items.as_slice() else {
                return None;
            };
            let (ExpressionKind::Text(module), ExpressionKind::Text(attribute)) =
                (&module.kind, &attribute.kind)
            else {
                return None;
            };
            if !names.insert(export) {
                return None;
            }
            // Builtin __import__ probes __path__ before processing fromlist.
            // A leaf can still supply that attribute through its own hook.
            if provider.is_none()
                && !self.modules.get(module.as_str()).is_some_and(|&target| {
                    !self.files[target].is_package
                        && !self.scopes[target].names.contains_key("__path__")
                        && !self.scopes[target].names.contains_key("__getattr__")
                        && !self.has_module_binding_write(target, "__path__")
                        && !self.has_module_binding_write(target, "__getattr__")
                })
            {
                return None;
            }
            let branch = LazyBranch {
                scope,
                imports: &[],
                temporary: None,
                value,
                at: *at,
                argument: None,
                table_target: Some(LazyTableTarget {
                    module,
                    attribute,
                    table,
                    provider,
                }),
            };
            branches.push((export.as_str(), branch));
        }
        Some(LazyTable { branches, fallback })
    }

    fn lazy_table_annotations(&self, file: ScopeId, hook: &Definition) -> bool {
        self.files[file].facts.postponed_annotations
            || hook.annotations.iter().all(|annotation| {
                matches!(annotation.kind, ExpressionKind::Text(_))
                    || expression_name(annotation, "str")
                        && self.unshadowed_lazy_builtin(file, "str", hook.name.at)
                    || self.lazy_provider_member(file, annotation, hook.name.at, "typing", "Any")
            })
    }

    fn unshadowed_lazy_builtin(&self, scope: ScopeId, name: &str, at: Location) -> bool {
        let file = self.scopes[scope].file;
        matches!(
            self.lexical_binding(scope, name, at, true),
            LexicalBinding::Missing
        ) && !self.scopes[file].names.contains_key("__builtins__")
            && self
                .module_member_write("builtins", name, (scope, at))
                .is_none()
    }

    fn stable_lazy_table_owner(
        &self,
        file: ScopeId,
        hook: &Definition,
        assignment: Location,
    ) -> bool {
        self.scopes[file].body.iter().all(|statement| match &statement.kind {
            StatementKind::Import(import) => !import.wildcard_member,
            StatementKind::Assign { names, annotation: None, .. } if position(statement.at) == position(assignment) =>
                matches!(names.as_slice(), [name] if name.text == "_LAZY"),
            StatementKind::Assign {
                names,
                value: Some(Expression { kind: ExpressionKind::Sequence { items, .. }, .. }),
                annotation: None,
            } => matches!(names.as_slice(), [name] if name.text == "__all__")
                && items.iter().all(|item| {
                    matches!(&item.kind, ExpressionKind::Text(name) if name != "_LAZY")
                }),
            StatementKind::Define(definition) => std::ptr::eq(definition, hook),
            StatementKind::If { test, body, otherwise } => otherwise.is_empty()
                && self.lazy_provider_member(file, test, statement.at, "typing", "TYPE_CHECKING")
                && body.iter().all(|statement| {
                    matches!(&statement.kind, StatementKind::Import(import) if !import.wildcard_member)
                }),
            StatementKind::Evaluate(Expression { kind: ExpressionKind::Text(_), .. }) => true,
            _ => false,
        })
    }
}
