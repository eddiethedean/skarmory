//! Selected class creation effects from closed source metaclasses.

use super::*;

#[derive(Clone)]
pub(super) enum ClassCreation {
    Redirect {
        constructor: SymbolId,
        replacement: SymbolId,
    },
    Rewrite {
        metaclass: SymbolId,
        constructor: SymbolId,
        member: String,
        replacement: SymbolId,
    },
    Unknown,
}

impl Graph<'_> {
    pub(super) fn index_metaclasses(&mut self) {
        let classes: Vec<_> = (0..self.symbols.len())
            .filter(|&id| self.is_class(id))
            .collect();
        if !classes.iter().any(|&id| {
            matches!(self.symbols[id].binding,
                Binding::Definition(definition, _) if definition.metaclass.is_some())
        }) {
            return;
        }
        let mut resolved = BTreeSet::new();
        for id in classes {
            self.index_class_creation(id, &mut resolved, 0);
        }
    }

    fn index_class_creation(
        &mut self,
        id: SymbolId,
        resolved: &mut BTreeSet<SymbolId>,
        depth: usize,
    ) {
        if depth >= MAX_BINDING_CHAIN || !resolved.insert(id) {
            return;
        }
        let symbol = &self.symbols[id];
        let Binding::Definition(definition, _) = symbol.binding else {
            return;
        };
        let scope = symbol.scope;
        let at = definition.name.at;
        let ancestors: Vec<_> = definition
            .bases
            .iter()
            .filter_map(|base| {
                let Value::Callable(invocation) =
                    self.pytest_expression(scope, base, at, &mut Vec::new(), 0)?
                else {
                    return None;
                };
                self.is_class(invocation.target)
                    .then_some(invocation.target)
            })
            .collect();
        for &ancestor in &ancestors {
            self.index_class_creation(ancestor, resolved, depth + 1);
        }
        let select_inherited = ancestors
            .iter()
            .find_map(|id| match self.metaclasses.get(id) {
                Some(ClassCreation::Redirect { .. }) | None => None,
                Some(creation) => Some(creation.clone()),
            });
        let explicit = definition.metaclass.as_ref();
        if explicit.is_none() && select_inherited.is_none() {
            return;
        }

        let creation = if scope == self.scopes[scope].file
            && !self.binding_depends_on_branch(id, END)
            && closed_namespace(definition)
            && self.closed_class_prefix(scope, at)
        {
            self.source_class_creation(scope, definition, &ancestors, select_inherited)
                .unwrap_or(ClassCreation::Unknown)
        } else {
            ClassCreation::Unknown
        };
        self.metaclasses.insert(id, creation);
    }

    fn source_class_creation(
        &self,
        scope: ScopeId,
        definition: &Definition,
        ancestors: &[SymbolId],
        inherited: Option<ClassCreation>,
    ) -> Option<ClassCreation> {
        let at = definition.name.at;
        let metaclass = if let Some(explicit) = &definition.metaclass {
            if !definition.bases.is_empty() {
                return None;
            }
            let ExpressionKind::Name(name) = &explicit.kind else {
                return None;
            };
            self.local(scope, name, at)?
        } else {
            let [ancestor] = ancestors else {
                return None;
            };
            let [base] = definition.bases.as_slice() else {
                return None;
            };
            let ExpressionKind::Name(name) = &base.kind else {
                return None;
            };
            if self.local(scope, name, at) != Some(*ancestor)
                || !matches!(self.symbols[*ancestor].binding, Binding::Definition(_, body)
                    if self.local(body, "__init_subclass__", END).is_none())
            {
                return None;
            }
            let ClassCreation::Rewrite { metaclass, .. } = inherited? else {
                return None;
            };
            metaclass
        };
        let Binding::Definition(meta, body) = self.symbols[metaclass].binding else {
            return None;
        };
        let [base] = meta.bases.as_slice() else {
            return None;
        };
        if !self.is_class(metaclass)
            || self.symbols[metaclass].scope != scope
            || meta.metaclass.is_some()
            || !closed_namespace(meta)
            || !self.genuine_type_name(scope, meta.name.at, base)
        {
            return None;
        }
        let [
            Statement {
                kind: StatementKind::Define(method),
                ..
            },
        ] = body_without_docstring(meta)
        else {
            return None;
        };
        let [class, name, bases, namespace] = method.parameters.as_slice() else {
            return None;
        };
        if method.name.text != "__new__"
            || method.is_generator
            || !method.simple_positional_parameters
        {
            return None;
        }
        let constructor = self.local(body, "__new__", END)?;
        match body_without_docstring(method) {
            [
                Statement {
                    kind: StatementKind::Return(Some(value)),
                    ..
                },
            ] if definition.bases.is_empty()
                && !namespace_free_names(
                    definition,
                    self.files[scope].facts.postponed_annotations,
                )
                .iter()
                .any(|name| matches!(*name, "__class__" | "super")) =>
            {
                let ExpressionKind::Name(value) = &value.kind else {
                    return None;
                };
                if method.parameters.iter().any(|param| param.text == *value) {
                    return None;
                }
                let replacement = self.local(scope, value, at)?;
                matches!(self.symbols[replacement].binding,
                    Binding::Definition(replacement, _)
                        if replacement.kind == DefinitionKind::Class
                            && replacement.bases.is_empty()
                            && replacement.metaclass.is_none()
                            && closed_namespace(replacement))
                .then_some(ClassCreation::Redirect {
                    constructor,
                    replacement,
                })
            }
            [
                Statement {
                    kind:
                        StatementKind::WriteModuleEntry {
                            mapping,
                            module: member,
                            value: Some(value),
                        },
                    ..
                },
                Statement {
                    kind: StatementKind::Return(Some(result)),
                    ..
                },
            ] if named(mapping, namespace)
                && !member.starts_with("__")
                && forwards_type_new(result, [class, name, bases, namespace])
                && matches!(
                    self.lexical_binding(scope, "super", at, true),
                    LexicalBinding::Missing
                )
                && method.parameters.iter().all(|param| param.text != "super") =>
            {
                let ExpressionKind::Name(value) = &value.kind else {
                    return None;
                };
                if method.parameters.iter().any(|param| param.text == *value) {
                    return None;
                }
                let replacement = self.local(scope, value, at)?;
                matches!(self.symbols[replacement].binding,
                    Binding::Definition(replacement, _) if closed_function(replacement))
                .then_some(ClassCreation::Rewrite {
                    metaclass,
                    constructor,
                    member: member.clone(),
                    replacement,
                })
            }
            _ => None,
        }
    }

    fn closed_class_prefix(&self, scope: ScopeId, before: Location) -> bool {
        // A preceding declaration must be unable to mutate the module
        // namespace or replace the built-in metaclass machinery.
        !self.scopes[scope].open_wildcard
            && self.scopes[scope]
                .body
                .iter()
                .take_while(|stmt| stmt.at.line < before.line)
                .all(|stmt| match &stmt.kind {
                    StatementKind::Evaluate(value) => {
                        matches!(value.kind, ExpressionKind::Text(_))
                    }
                    StatementKind::Define(definition)
                        if definition.kind == DefinitionKind::Class =>
                    {
                        let plain_bases = match definition.bases.as_slice() {
                            [] => true,
                            [base] => self.genuine_type_name(scope, definition.name.at, base),
                            _ => false,
                        };
                        closed_namespace(definition)
                            && (definition.metaclass.is_none() && plain_bases
                                || self
                                    .by_location
                                    .get(&(
                                        scope,
                                        definition.name.at.line,
                                        definition.name.at.column,
                                    ))
                                    .and_then(|id| self.metaclasses.get(id))
                                    .is_some_and(|creation| {
                                        !matches!(creation, ClassCreation::Unknown)
                                    }))
                    }
                    StatementKind::Define(definition) => closed_function(definition),
                    _ => false,
                })
    }

    fn genuine_type_name(&self, scope: ScopeId, at: Location, expression: &Expression) -> bool {
        matches!(&expression.kind, ExpressionKind::Name(name) if name == "type")
            && matches!(
                self.lexical_binding(scope, "type", at, true),
                LexicalBinding::Missing
            )
    }

    pub(super) fn metaclass_creation(&self, id: SymbolId) -> Option<ClassCreation> {
        let creation = self.metaclasses.get(&id)?;
        let site = (self.symbols[id].scope, self.symbols[id].name.at);
        if self.module_member_write("builtins", "type", site).is_some()
            || matches!(creation, ClassCreation::Rewrite { .. })
                && self
                    .module_member_write("builtins", "super", site)
                    .is_some()
        {
            return Some(ClassCreation::Unknown);
        }
        Some(creation.clone())
    }

    pub(super) fn metaclass_member(&self, class: SymbolId, name: &str) -> Option<SymbolId> {
        let ClassCreation::Rewrite {
            member,
            replacement,
            ..
        } = self.metaclass_creation(class)?
        else {
            return None;
        };
        (member == name).then_some(replacement)
    }

    pub(super) fn apply_metaclass(&mut self, scope: ScopeId, class: SymbolId, at: Location) {
        match self.metaclass_creation(class) {
            Some(ClassCreation::Rewrite {
                constructor,
                replacement,
                ..
            })
            | Some(ClassCreation::Redirect {
                constructor,
                replacement,
            }) => {
                // This particular call is evaluated at class creation. A separate
                // ordinary call to __new__ still checks its generic body.
                self.edge(scope, constructor, "metaclass_creation", "certain");
                self.read.insert(replacement);
                if self.is_class(replacement) {
                    self.edge(scope, replacement, "metaclass_result", "certain");
                }
            }
            Some(ClassCreation::Unknown) => self.scope_issue(
                scope,
                at,
                "unresolved_metaclass",
                "class creation depends on a metaclass outside the bounded model",
            ),
            None => {}
        }
    }
}

fn closed_function(definition: &Definition) -> bool {
    definition.kind == DefinitionKind::Function
        && definition.annotations.is_empty()
        && definition.decorators.is_empty()
        && definition.defaults.is_empty()
}

fn closed_namespace(definition: &Definition) -> bool {
    definition.decorators.is_empty()
        && definition.table_keyword.is_none()
        && body_without_docstring(definition)
            .iter()
            .all(|stmt| match &stmt.kind {
                StatementKind::Define(definition) => closed_function(definition),
                StatementKind::Assign {
                    names,
                    value: Some(value),
                    annotation: None,
                } => {
                    names.iter().all(|name| !name.text.starts_with("__"))
                        && matches!(
                            value.kind,
                            ExpressionKind::Text(_)
                                | ExpressionKind::Bool(_)
                                | ExpressionKind::None
                                | ExpressionKind::Scalar(_)
                        )
                }
                _ => false,
            })
}

fn named(expression: &Expression, parameter: &Name) -> bool {
    matches!(&expression.kind, ExpressionKind::Name(name) if *name == parameter.text)
}

fn forwards_type_new(result: &Expression, parameters: [&Name; 4]) -> bool {
    matches!(&result.kind, ExpressionKind::Call { function, arguments, keywords }
        if keywords.is_empty() && arguments.len() == parameters.len()
            && arguments.iter().zip(parameters).all(|(arg, parameter)| named(arg, parameter))
            && matches!(&function.kind, ExpressionKind::Member(receiver, name)
                if name == "__new__"
                    && matches!(&receiver.kind, ExpressionKind::Call { function, arguments, keywords }
                        if matches!(&function.kind, ExpressionKind::Name(name) if name == "super")
                            && arguments.is_empty()
                            && keywords.is_empty())))
}
