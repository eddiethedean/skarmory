//! Reachable `type.__subclasses__` lookups with explicit initialization order.

use super::*;

#[derive(Clone, Copy)]
pub(super) struct Lookup {
    scope: ScopeId,
    before: Location,
    at: Location,
    base: SymbolId,
}

impl Graph<'_> {
    pub(super) fn record_subclass_lookup(
        &mut self,
        function: &Expression,
        call: CallArguments<'_>,
    ) -> bool {
        if !self.phase03
            || !self.terminal_proofs
            || !call.positional.is_empty()
            || !call.keywords.is_empty()
        {
            return false;
        }
        let ExpressionKind::Member(receiver, name) = &function.kind else {
            return false;
        };
        if name != "__subclasses__" {
            return false;
        }
        let ExpressionKind::Name(name) = &receiver.kind else {
            return false;
        };
        let LexicalBinding::Found(id) = self.lexical_binding(call.scope, name, call.before, true)
        else {
            return false;
        };
        if !self.stable_subclass_binding(id) {
            return false;
        }
        let Value::Callable(invocation) = self.probe_identity(call.scope, receiver, call.before)
        else {
            return false;
        };
        let base = invocation.target;
        if invocation.verified
            || !self.stable_subclass_binding(base)
            || !self.source_provider_definition_precedes(base, (call.scope, call.before))
            || !self.plain_subclass_base(base)
            || self
                .class_member_at(base, name, 0, Some((call.scope, call.before)))
                .is_some()
        {
            return false;
        }
        self.subclass_lookups.push(Lookup {
            scope: call.scope,
            before: call.before,
            at: call.at,
            base,
        });
        true
    }

    fn stable_subclass_binding(&self, id: SymbolId) -> bool {
        let binding = &self.symbols[id];
        let name = binding.name.text.as_str();
        let owner = &self.scopes[binding.scope];
        owner
            .names
            .get(name)
            .is_some_and(|names| names.as_slice() == [id])
            && !self.conditional.contains_key(&id)
            && (binding.scope != owner.file || !self.has_module_binding_write(owner.file, name))
    }

    fn plain_subclass_base(&self, id: SymbolId) -> bool {
        matches!(self.symbols[id].binding, Binding::Definition(definition, _)
            if definition.kind == DefinitionKind::Class
                && closures::passive_definition(definition))
            && !self.has_unmodeled_class_write(id)
    }

    pub(super) fn retain_discovered_subclasses(&mut self, reach: &mut Reachability) {
        for lookup in std::mem::take(&mut self.subclass_lookups) {
            if !reach.scopes.contains(&lookup.scope) {
                continue;
            }
            let targets = self.subclasses_at_lookup(lookup, reach);
            if let Some(targets) = targets {
                for target in targets {
                    reach.symbols.insert(target);
                    reach
                        .evidence
                        .insert((target, "subclass_discovery", "possible"));
                }
            } else {
                self.issues.push(
                    Issue::new(
                        "unresolved_call",
                        &self.files[self.scopes[lookup.scope].file].path,
                        "subclass lookup requires a known receiver and module initialization order",
                    )
                    .at(lookup.at),
                );
            }
        }
    }

    fn subclasses_at_lookup(
        &mut self,
        lookup: Lookup,
        reach: &Reachability,
    ) -> Option<Vec<SymbolId>> {
        let file = self.scopes[lookup.scope].file;
        if !self.plain_subclass_base(lookup.base)
            || !self
                .class_discovery_writes(lookup.base, lookup.scope)
                .is_empty()
            || self.scopes.iter().enumerate().any(|(scope, row)| {
                self.scopes[scope].file != file
                    && reach.scopes.contains(&scope)
                    && row.imports.contains(&file)
            })
        {
            return None;
        }

        self.subclass_call_boundary(lookup.scope, lookup.at)?;
        let caller_positions = if lookup.scope == file {
            vec![lookup.at]
        } else {
            let owner = self.scopes[lookup.scope].owner?;
            let Binding::Definition(definition, _) = self.symbols[owner].binding else {
                return None;
            };
            if self.symbols[owner].scope != file
                || !field_dispatch::plain_method(definition, 0)
                || reach
                    .evidence
                    .iter()
                    .any(|&(target, kind, _)| target == owner && kind != "direct_call")
            {
                return None;
            }
            let calls = self.direct_arguments.get(&owner)?;
            let callers: Vec<_> = calls
                .iter()
                .filter(|call| reach.scopes.contains(&call.caller))
                .collect();
            if callers.is_empty()
                || callers.iter().any(|call| {
                    call.caller != file
                        || call.verified
                        || call.implicit_receiver
                        || !call.arguments_bound
                })
            {
                return None;
            }
            callers.into_iter().map(|call| call.at).collect()
        };

        let mut targets = BTreeSet::new();
        for before in caller_positions {
            let boundary = self.subclass_call_boundary(file, before)?;
            let mut pending = Vec::new();
            if !self.subclass_prefix_imports(file, self.scopes[file].body, boundary, &mut pending)
                || (lookup.scope != file
                    && !self.subclass_prefix_imports(
                        lookup.scope,
                        self.scopes[lookup.scope].body,
                        lookup.before,
                        &mut pending,
                    ))
            {
                return None;
            }
            let mut loaded = BTreeMap::from([(file, boundary)]);
            while let Some(module) = pending.pop() {
                if let std::collections::btree_map::Entry::Vacant(entry) = loaded.entry(module) {
                    entry.insert(END);
                    if !self.subclass_prefix_imports(
                        module,
                        self.scopes[module].body,
                        END,
                        &mut pending,
                    ) {
                        return None;
                    }
                }
            }

            for (&module, &before) in &loaded {
                let declarations: Vec<_> = self.scopes[module]
                    .names
                    .values()
                    .flatten()
                    .copied()
                    .filter(|&id| self.is_class(id))
                    .collect();
                for id in declarations {
                    let symbol = &self.symbols[id];
                    if position(symbol.name.at) >= position(before)
                        || self.conditional.contains_key(&id)
                    {
                        continue;
                    }
                    let Binding::Definition(definition, body) = symbol.binding else {
                        continue;
                    };
                    if definition.kind != DefinitionKind::Class || !reach.scopes.contains(&body) {
                        continue;
                    }
                    let scope = symbol.scope;
                    if definition.bases.iter().any(|base| {
                    matches!(self.probe_identity(scope, base, definition.name.at),
                        Value::Callable(invocation) if !invocation.verified && invocation.target == lookup.base)
                }) {
                    targets.insert(id);
                }
                }
            }
        }
        Some(targets.into_iter().collect())
    }

    fn subclass_call_boundary(&self, scope: ScopeId, at: Location) -> Option<Location> {
        let statement = self.scopes[scope]
            .body
            .iter()
            .rev()
            .find(|statement| position(statement.at) <= position(at))?;
        let value = match &statement.kind {
            StatementKind::Evaluate(value)
            | StatementKind::Return(Some(value))
            | StatementKind::Assign {
                value: Some(value),
                annotation: None,
                ..
            } => value,
            _ => return None,
        };
        let ExpressionKind::Call {
            function,
            arguments,
            keywords,
        } = &value.kind
        else {
            return None;
        };
        (position(value.at) == position(at)
            // A bare callable lookup cannot run before its only argument.
            || keywords.is_empty()
                && matches!(function.kind, ExpressionKind::Name(_))
                && matches!(arguments.as_slice(), [argument]
                    if matches!(argument.kind, ExpressionKind::Call { .. })
                        && position(argument.at) == position(at)))
        .then_some(statement.at)
    }

    fn subclass_prefix_imports(
        &mut self,
        scope: ScopeId,
        body: &[Statement],
        before: Location,
        pending: &mut Vec<ScopeId>,
    ) -> bool {
        body.iter()
            .take_while(|statement| position(statement.at) < position(before))
            .all(|statement| match &statement.kind {
                StatementKind::Import(import) => {
                    let Some(&id) = self.by_location.get(&(
                        scope,
                        import.local.at.line,
                        import.local.at.column,
                    )) else {
                        return false;
                    };
                    let mut imports = self.scopes[scope]
                        .initializer_imports
                        .iter()
                        .filter(|&&(binding, _)| binding == id)
                        .map(|&(_, target)| target)
                        .peekable();
                    if imports.peek().is_none() {
                        return false;
                    }
                    pending.extend(imports);
                    true
                }
                StatementKind::Define(definition) => {
                    closures::passive_definition(definition)
                        || definition.kind == DefinitionKind::Class
                            && definition.bases.iter().all(|base| {
                                matches!(base.kind, ExpressionKind::Name(_))
                                    && matches!(self.probe_identity(scope, base, definition.name.at),
                                        Value::Callable(invocation)
                                            if !invocation.verified && self.plain_subclass_base(invocation.target)
                                                && self.class_discovery_writes(invocation.target, scope).is_empty())
                            })
                            && definition.decorators.is_empty()
                            && !definition.has_class_keywords()
                            && body_without_docstring(definition).is_empty()
                }
                StatementKind::Evaluate(value) => closures::inert_default(value),
                StatementKind::If {
                    test,
                    body,
                    otherwise,
                } => {
                    let ExpressionKind::Bool(value) = test.kind else {
                        return false;
                    };
                    self.subclass_prefix_imports(
                        scope,
                        if value { body } else { otherwise },
                        before,
                        pending,
                    )
                }
                _ => false,
            })
    }
}
