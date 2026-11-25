//! Discarded static method calls on a class supplied to a local function.

use super::*;

mod closed;

const MAX_WRAPPER_PREFIX: usize = 16;

#[derive(Clone)]
pub(super) struct StaticParameterCall {
    scope: ScopeId,
    at: Location,
    parameter: SymbolId,
    member: String,
    prefix_reads: Vec<String>,
    arguments: usize,
}

impl Graph<'_> {
    pub(super) fn record_class_argument_consumer(
        &mut self,
        scope: ScopeId,
        callee: SymbolId,
        value: &Value,
    ) {
        match value {
            Value::Callable(invocation) if self.is_class(invocation.target) => {
                self.class_argument_consumers
                    .entry(invocation.target)
                    .or_default()
                    .insert((scope, callee));
            }
            Value::Sequence(items) | Value::Mapping(items) | Value::Alternatives(items) => {
                for item in items {
                    self.record_class_argument_consumer(scope, callee, item);
                }
            }
            _ => {}
        }
    }

    pub(super) fn defer_static_parameter_call(
        &mut self,
        function: &Expression,
        expression: &Expression,
        call: CallArguments<'_>,
    ) -> bool {
        let row = &self.scopes[call.scope];
        let ExpressionKind::Member(receiver, member) = &function.kind else {
            return false;
        };
        let (Some(owner), Some(parameter)) = (
            row.owner,
            self.parameter_binding(call.scope, receiver, call.before),
        ) else {
            return false;
        };
        let Binding::Definition(wrapper, _) = self.symbols[owner].binding else {
            return false;
        };
        if row.parent != Some(row.file)
            || self.symbols[parameter].scope != call.scope
            || wrapper.kind != DefinitionKind::Function
            || wrapper.is_generator
            || !wrapper.simple_positional_parameters
            || !wrapper.decorators.is_empty()
            || member.starts_with("__")
            || !call.keywords.is_empty()
            || !call.positional.iter().all(literal_argument)
        {
            return false;
        }

        let mut prefix_reads = Vec::new();
        for statement in row.body.iter().take(MAX_WRAPPER_PREFIX + 1) {
            match &statement.kind {
                StatementKind::Evaluate(value) if std::ptr::eq(value, expression) => {
                    self.static_parameter_calls.push(StaticParameterCall {
                        scope: call.scope,
                        at: call.at,
                        parameter,
                        member: member.clone(),
                        prefix_reads,
                        arguments: call.positional.len(),
                    });
                    return true;
                }
                StatementKind::Evaluate(value) if literal_argument(value) => {}
                StatementKind::Assign {
                    names,
                    value: Some(value),
                    annotation: None,
                } if names
                    .iter()
                    .all(|name| name.text != self.symbols[parameter].name.text) =>
                {
                    match &value.kind {
                        ExpressionKind::Member(base, name)
                            if !name.starts_with("__")
                                && self.parameter_binding(call.scope, base, statement.at)
                                    == Some(parameter) =>
                        {
                            prefix_reads.push(name.clone());
                        }
                        _ if literal_argument(value)
                            || self
                                .parameter_binding(call.scope, value, statement.at)
                                .is_some() => {}
                        _ => return false,
                    }
                }
                _ => return false,
            }
        }
        false
    }

    pub(super) fn activate_static_parameter_calls(
        &mut self,
        reach: &mut Reachability,
        pending: &mut VecDeque<Edge>,
    ) {
        self.activate_closed_class_calls(reach, pending);
        for index in 0..self.static_parameter_calls.len() {
            let site = &self.static_parameter_calls[index];
            if !reach.scopes.contains(&site.scope) {
                continue;
            }
            let owner = self.scopes[site.scope].owner.expect("function scope");
            let independently_reached = reach
                .evidence
                .range((owner, "", "")..)
                .take_while(|&&(target, _, _)| target == owner)
                .any(|&(_, kind, _)| !matches!(kind, "direct_call" | "type_reference"));
            let name = &self.symbols[site.parameter].name.text;
            let candidates: BTreeSet<_> = self
                .direct_arguments
                .get(&owner)
                .into_iter()
                .flatten()
                .filter(|call| reach.scopes.contains(&call.caller))
                .map(|call| match call.values.get(name) {
                    Some(Value::Callable(class))
                        if call.arguments_bound
                            && !call.implicit_receiver
                            && !class.verified
                            && matches!(class.receiver, Receiver::Ordinary) =>
                    {
                        Some(class.target)
                    }
                    _ => None,
                })
                .collect();
            let mut complete = !independently_reached && !candidates.is_empty();
            let site = site.clone();
            for class in candidates {
                if let Some(class) = class
                    && !self
                        .class_argument_consumers
                        .get(&class)
                        .is_some_and(|consumers| {
                            consumers.iter().any(|(scope, callee)| {
                                reach.scopes.contains(scope) && *callee != owner
                            })
                        })
                    && let Some(target) = self.static_parameter_target(class, &site)
                {
                    if !reach.has_evidence(target, "direct_call") {
                        pending.push_back(Edge {
                            target,
                            kind: "direct_call",
                            certainty: "possible",
                        });
                    }
                } else {
                    complete = false;
                }
            }
            if complete {
                reach.resolved_static_parameter_calls.insert(index);
            } else {
                reach.resolved_static_parameter_calls.remove(&index);
            }
        }
    }

    pub(super) fn remove_resolved_static_parameter_diagnostics(&mut self, reach: &Reachability) {
        let proved: BTreeSet<_> = reach
            .resolved_static_parameter_calls
            .iter()
            .map(|&index| {
                let site = &self.static_parameter_calls[index];
                (site.scope, Some(site.at.line), Some(site.at.column))
            })
            .collect();
        self.scope_issues.retain(|(scope, issue)| {
            let closed_reason = matches!(
                (issue.kind, issue.message.as_str()),
                (
                    "unresolved_call",
                    "call target is outside the bounded model"
                ) | (
                    "unresolved_constructor",
                    "allocation result is outside the bounded class model"
                ) | (
                    "unresolved_unpack",
                    "assignment unpacking requires a bounded list or tuple value"
                ) | (
                    "unresolved_formatting",
                    "formatted string dispatch is outside the bounded model"
                )
            );
            if closed_reason
                && let (Some(line), Some(column)) = (issue.line, issue.column)
                && reach
                    .closed_class_call_sites
                    .contains(&(*scope, [line, column], issue.kind))
            {
                return false;
            }
            issue.kind != "unresolved_call"
                || issue.message != "call target is outside the bounded model"
                || !proved.contains(&(*scope, issue.line, issue.column))
        });
    }

    fn static_parameter_target(
        &mut self,
        class: SymbolId,
        site: &StaticParameterCall,
    ) -> Option<SymbolId> {
        let class_scope = self.static_parameter_class(class, site.scope)?;
        let access = Some((site.scope, site.at));
        for read in &site.prefix_reads {
            if !self
                .class_member_at(class, read, 0, access)
                .is_some_and(|value| value.is_builtin_scalar())
            {
                return None;
            }
        }
        self.static_parameter_method(
            class,
            class_scope,
            &site.member,
            site.arguments,
            (site.scope, site.at),
        )
    }

    fn static_parameter_class(&mut self, class: SymbolId, caller: ScopeId) -> Option<ScopeId> {
        let symbol = &self.symbols[class];
        let Binding::Definition(definition, class_scope) = symbol.binding else {
            return None;
        };
        if definition.kind != DefinitionKind::Class
            || !definition.bases.is_empty()
            || definition.has_class_keywords()
            || !definition.decorators.is_empty()
            || !self.scopes[symbol.scope]
                .class_bodies
                .contains(&class_scope)
            || self
                .failed_terminal_modules
                .contains(&self.scopes[symbol.scope].file)
            || self.dormant_classes.contains(&class)
            || self.subclassed.contains(&class)
            || self.has_unmodeled_class_write(class)
            || !self.class_discovery_writes(class, caller).is_empty()
        {
            return None;
        }
        for statement in body_without_docstring(definition) {
            match &statement.kind {
                StatementKind::Define(method)
                    if method.kind == DefinitionKind::Function
                        && !method.name.text.starts_with("__")
                        && method.decorators.iter().all(|decorator| {
                            is_staticmethod(self, class_scope, decorator, statement.at)
                        }) => {}
                StatementKind::Assign {
                    value: Some(value),
                    annotation: None,
                    ..
                } if self
                    .probe_identity(class_scope, value, statement.at)
                    .is_builtin_scalar() => {}
                _ => return None,
            }
        }
        Some(class_scope)
    }

    fn static_parameter_method(
        &mut self,
        class: SymbolId,
        class_scope: ScopeId,
        member: &str,
        arguments: usize,
        access: (ScopeId, Location),
    ) -> Option<SymbolId> {
        let Value::Callable(invocation) = self.class_member_at(class, member, 0, Some(access))?
        else {
            return None;
        };
        let Binding::Definition(method, _) = self.symbols[invocation.target].binding else {
            return None;
        };
        let [decorator] = method.decorators.as_slice() else {
            return None;
        };
        (self.symbols[invocation.target].scope == class_scope
            && !invocation.verified
            && matches!(invocation.receiver, Receiver::Ordinary)
            && method.kind == DefinitionKind::Function
            && !method.is_generator
            && method.simple_positional_parameters
            && method.parameters.len() == arguments
            && is_staticmethod(self, class_scope, decorator, method.name.at))
        .then_some(invocation.target)
    }
}

fn literal_argument(value: &Expression) -> bool {
    matches!(
        value.kind,
        ExpressionKind::Text(_)
            | ExpressionKind::Scalar(_)
            | ExpressionKind::Bool(_)
            | ExpressionKind::None
    )
}

fn is_staticmethod(
    graph: &mut Graph<'_>,
    scope: ScopeId,
    expression: &Expression,
    before: Location,
) -> bool {
    graph
        .module_member_write("builtins", "staticmethod", (scope, before))
        .is_none()
        && matches!(graph.probe_identity(scope, expression, before), Value::Provider(name)
            if name == "builtins.staticmethod")
}
