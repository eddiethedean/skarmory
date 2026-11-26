//! Source-owned instances in a closed logging configuration initializer.

use super::*;

#[derive(Clone)]
pub(super) struct Method<'a> {
    target: SymbolId,
    definition: &'a Definition,
    body: ScopeId,
    defaults: Vec<Data>,
}

pub(super) struct Instance<'a> {
    class: SymbolId,
    pub(super) fields: BTreeMap<&'a str, Data>,
}

impl Instance<'_> {
    pub(super) fn has_method(
        &self,
        name: &str,
        classes: &BTreeMap<SymbolId, BTreeMap<&str, Method<'_>>>,
    ) -> bool {
        classes[&self.class].contains_key(name)
    }
}

impl<'a> Replay<'a> {
    pub(super) fn define_class(
        &mut self,
        graph: &Graph<'a>,
        scope: ScopeId,
        definition: &'a Definition,
    ) -> Option<()> {
        if self.depth > 0
            || !definition.bases.is_empty()
            || !definition.decorators.is_empty()
            || definition.has_class_keywords()
            || self.classes.len() >= MAX_ITEMS
        {
            return None;
        }
        let target =
            graph.by_location[&(scope, definition.name.at.line, definition.name.at.column)];
        let Binding::Definition(_, class_scope) = graph.symbols[target].binding else {
            return None;
        };
        if graph.dormant_classes.contains(&target) || graph.subclassed.contains(&target) {
            return None;
        }
        let mut methods = BTreeMap::new();
        for statement in body_without_docstring(definition) {
            let StatementKind::Define(method) = &statement.kind else {
                return None;
            };
            let name = method.name.text.as_str();
            if name.starts_with("__") && name != "__init__"
                || method.kind != DefinitionKind::Function
                || !method.simple_positional_parameters
                || method.is_generator
                || !method.decorators.is_empty()
                || !method.annotations.is_empty()
                || method.parameters.len() <= method.defaults.len()
            {
                return None;
            }
            let defaults = method
                .defaults
                .iter()
                .map(|value| {
                    // A class default evaluates in the class namespace. Only
                    // earlier plain methods can shadow module names here.
                    match &value.kind {
                        ExpressionKind::Name(name) if !methods.contains_key(name.as_str()) => {
                            self.evaluate(value, 0)
                        }
                        ExpressionKind::Text(_)
                        | ExpressionKind::Scalar(Some(_))
                        | ExpressionKind::Bool(_)
                        | ExpressionKind::None => self.evaluate(value, 0),
                        _ => None,
                    }
                })
                .collect::<Option<Vec<_>>>()?;
            let id = graph.by_location[&(class_scope, method.name.at.line, method.name.at.column)];
            let Binding::Definition(_, body) = graph.symbols[id].binding else {
                return None;
            };
            methods.insert(
                name,
                Method {
                    target: id,
                    definition: method,
                    body,
                    defaults,
                },
            );
        }
        self.classes.insert(target, methods);
        self.names
            .insert(&definition.name.text, Data::Class(target));
        Some(())
    }

    pub(super) fn is_constructor(&self, expression: &Expression) -> bool {
        matches!(&expression.kind,
            ExpressionKind::Call { function, .. } if matches!(&function.kind,
                ExpressionKind::Name(name) if matches!(self.names.get(name.as_str()), Some(Data::Class(_)))))
    }

    pub(super) fn construct(
        &mut self,
        graph: &mut Graph<'a>,
        scope: ScopeId,
        expression: &'a Expression,
    ) -> Option<usize> {
        if self.instances.len() >= MAX_ITEMS {
            return None;
        }
        let ExpressionKind::Call {
            function,
            arguments,
            keywords,
        } = &expression.kind
        else {
            return None;
        };
        let Data::Class(class) = self.evaluate(function, 0)? else {
            return None;
        };
        self.calls.insert((scope, position(expression.at), class));
        let id = self.instances.len();
        self.instances.push(Instance {
            class,
            fields: BTreeMap::new(),
        });
        if self.instances[id].has_method("__init__", &self.classes) {
            self.invoke_method(graph, scope, id, "__init__", expression)?;
        } else if !arguments.is_empty() || !keywords.is_empty() {
            return None;
        }
        Some(id)
    }

    pub(super) fn invoke_method(
        &mut self,
        graph: &mut Graph<'a>,
        caller: ScopeId,
        receiver: usize,
        name: &str,
        call: &'a Expression,
    ) -> Option<()> {
        let ExpressionKind::Call {
            arguments,
            keywords,
            ..
        } = &call.kind
        else {
            return None;
        };
        if self.depth >= MAX_REPLAY_DEPTH || self.instances[receiver].fields.contains_key(name) {
            return None;
        }
        let method = self.classes[&self.instances[receiver].class]
            .get(name)?
            .clone();
        let [first, parameters @ ..] = method.definition.parameters.as_slice() else {
            return None;
        };
        if arguments.len() > parameters.len() {
            return None;
        }
        let mut bindings = BTreeMap::from([(first.text.as_str(), Data::Instance(receiver))]);
        for (parameter, value) in parameters.iter().zip(arguments) {
            bindings.insert(&parameter.text, self.evaluate(value, 0)?);
        }
        for (name, value) in keywords {
            let parameter = parameters.iter().find(|param| param.text == *name)?;
            if bindings
                .insert(&parameter.text, self.evaluate(value, 0)?)
                .is_some()
            {
                return None;
            }
        }
        let required = parameters.len() - method.defaults.len();
        for (index, parameter) in parameters.iter().enumerate() {
            if bindings.contains_key(parameter.text.as_str()) {
                continue;
            }
            bindings.insert(
                &parameter.text,
                method.defaults.get(index.checked_sub(required)?)?.clone(),
            );
        }

        let previous = std::mem::replace(&mut self.names, bindings);
        self.depth += 1;
        self.calls
            .insert((caller, position(call.at), method.target));
        self.method_scopes.insert(method.body);
        for statement in body_without_docstring(method.definition) {
            self.statement(graph, method.body, statement)?;
            if self.returned || self.stopped {
                break;
            }
        }
        self.depth -= 1;
        self.returned = false;
        self.names = previous;
        Some(())
    }

    pub(super) fn missing_instance_field(&mut self, expression: &'a Expression) -> bool {
        let ExpressionKind::Member(owner, name) = &expression.kind else {
            return false;
        };
        let Some(Data::Instance(id)) = self.evaluate(owner, 0) else {
            return false;
        };
        let instance = &self.instances[id];
        !name.starts_with("__")
            && !instance.fields.contains_key(name.as_str())
            && !instance.has_method(name, &self.classes)
    }

    pub(super) fn accounts_for_methods(&self, graph: &Graph<'_>, file: ScopeId) -> bool {
        if self.classes.is_empty() {
            return true;
        }
        if !graph.terminal_proofs
            || graph.files[file].is_package
            || graph
                .scopes
                .iter()
                .any(|scope| scope.file != file && scope.imports.contains(&file))
        {
            return false;
        }

        let tracked: BTreeSet<_> = self
            .classes
            .iter()
            .flat_map(|(&class, methods)| {
                std::iter::once(class).chain(methods.values().map(|method| method.target))
            })
            .collect();
        let observed: BTreeSet<_> = self
            .calls
            .iter()
            .map(|&(scope, _, target)| (scope, target))
            .collect();
        let intended = |scope, edge: &Edge| {
            matches!(edge.kind, "direct_call" | "constructor")
                && observed.contains(&(scope, edge.target))
        };
        for (scope, row) in graph.scopes.iter().enumerate() {
            if self.method_scopes.contains(&scope)
                && (row.edges.iter().any(|edge| !intended(scope, edge))
                    || !row.imports.is_empty()
                    || !row.class_bodies.is_empty()
                    || !row.activated_apps.is_empty()
                    || !row.mounts.is_empty()
                    || !row.routes.is_empty()
                    || !row.optional_flask_routes.is_empty()
                    || !row.starts_django.is_empty()
                    || !row.django_signals.is_empty())
            {
                return false;
            }
            if row.edges.iter().any(|edge| {
                tracked.contains(&edge.target)
                    && !matches!(edge.kind, "class_reference")
                    && !intended(scope, edge)
            }) {
                return false;
            }
        }
        if graph.class_invocations.iter().any(|call| {
            (tracked.contains(&call.edge.target) || self.method_scopes.contains(&call.scope))
                && !intended(call.scope, &call.edge)
        }) {
            return false;
        }
        tracked.iter().all(|target| {
            graph.direct_arguments.get(target).is_none_or(|sites| {
                sites.iter().all(|call| {
                    self.calls
                        .contains(&(call.caller, position(call.at), *target))
                })
            })
        })
    }
}
