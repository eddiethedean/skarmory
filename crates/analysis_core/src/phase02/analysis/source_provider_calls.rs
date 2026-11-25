//! Method dispatch on a fresh instance supplied to a source-owned wrapper.

use super::*;

const MAX_PROVIDER_CALLS: usize = 32;

#[derive(Clone, Copy)]
enum ProviderReceiver<'a> {
    Argument,
    Factory {
        expression: &'a Expression,
        before: Location,
        selector: &'a str,
    },
}

#[derive(Clone, Copy)]
pub(super) struct SourceProviderCall<'a> {
    scope: ScopeId,
    at: Location,
    receiver: ProviderReceiver<'a>,
    member: &'a str,
    supplied: usize,
}

impl<'a> Graph<'a> {
    pub(super) fn defer_source_provider_call(&mut self, scope: ScopeId, call: &Expression) -> bool {
        let row = &self.scopes[scope];
        let Some(owner) = row.owner else {
            return false;
        };
        let Binding::Definition(wrapper, _) = self.symbols[owner].binding else {
            return false;
        };
        let Some((returned_at, returned, initializer)) = wrapper_return(wrapper) else {
            return false;
        };
        if row.parent != Some(row.file) || !std::ptr::eq(returned, call) {
            return false;
        }
        let Some((name, member, supplied)) = provider_member(returned) else {
            return false;
        };
        if member.starts_with("__")
            || !supplied.iter().all(|argument| {
                matches!(
                    argument.kind,
                    ExpressionKind::Text(_)
                        | ExpressionKind::Scalar(_)
                        | ExpressionKind::Bool(_)
                        | ExpressionKind::None
                ) || self
                    .parameter_binding(scope, argument, returned_at)
                    .is_some()
            })
        {
            return false;
        }
        let receiver = if let Some(initializer) = initializer {
            let StatementKind::Assign {
                names,
                value: Some(expression),
                annotation: None,
            } = &initializer.kind
            else {
                return false;
            };
            let [assigned] = names.as_slice() else {
                return false;
            };
            let ExpressionKind::Call { arguments, .. } = &expression.kind else {
                return false;
            };
            let [selector] = arguments.as_slice() else {
                return false;
            };
            if assigned.text != name
                || self
                    .parameter_binding(scope, selector, initializer.at)
                    .is_none()
            {
                return false;
            }
            let ExpressionKind::Name(selector) = &selector.kind else {
                unreachable!("a parameter binding is a name")
            };
            ProviderReceiver::Factory {
                expression,
                before: initializer.at,
                selector,
            }
        } else {
            if !wrapper
                .parameters
                .iter()
                .any(|parameter| parameter.text == name)
            {
                return false;
            }
            ProviderReceiver::Argument
        };
        self.source_provider_calls.insert(
            owner,
            SourceProviderCall {
                scope,
                at: returned.at,
                receiver,
                member,
                supplied: supplied.len(),
            },
        );
        true
    }

    pub(super) fn fresh_provider_argument(
        &mut self,
        wrapper: &Definition,
        call: CallArguments<'_>,
        values: &BTreeMap<String, Value>,
    ) -> Option<SymbolId> {
        let (_, returned, None) = wrapper_return(wrapper)? else {
            return None;
        };
        let (receiver, _, _) = provider_member(returned)?;
        let parameter = wrapper
            .parameters
            .iter()
            .position(|parameter| parameter.text == receiver)?;
        let argument = call.positional.get(parameter).or_else(|| {
            call.keywords
                .iter()
                .find(|(name, _)| name == receiver)
                .map(|(_, value)| value)
        })?;
        let ExpressionKind::Call {
            function,
            arguments,
            keywords,
        } = &argument.kind
        else {
            return None;
        };
        if !arguments.is_empty() || !keywords.is_empty() {
            return None;
        }
        let (Some(Value::Instance(class)), Value::Callable(constructor)) = (
            values.get(receiver),
            self.probe_identity(call.scope, function, call.before),
        ) else {
            return None;
        };
        (!constructor.verified && constructor.target == *class).then_some(*class)
    }

    pub(super) fn activate_source_provider_calls(
        &mut self,
        reach: &mut Reachability,
        pending: &mut VecDeque<Edge>,
        possible: bool,
    ) {
        let sites: Vec<_> = self
            .source_provider_calls
            .iter()
            .filter(|(_, site)| reach.scopes.contains(&site.scope))
            .map(|(&owner, &site)| (owner, site))
            .collect();
        if sites.is_empty() {
            return;
        }
        let invalidated = self.source_provider_invalidated.take().unwrap_or_else(|| {
            let mut invalidated = self.field_callback_replacements();
            invalidated.extend(self.escaped_field_callbacks());
            invalidated
        });
        for (owner, site) in sites {
            if invalidated.contains(&owner) {
                continue;
            }
            let calls: Vec<_> = self
                .direct_arguments
                .get(&owner)
                .into_iter()
                .flatten()
                .filter(|call| reach.scopes.contains(&call.caller))
                .take(MAX_PROVIDER_CALLS + 1)
                .map(|call| {
                    let receiver = match site.receiver {
                        ProviderReceiver::Argument => call.fresh_provider.map(ProviderInput::Class),
                        ProviderReceiver::Factory { selector, .. } => match call
                            .values
                            .get(selector)
                        {
                            Some(Value::Text(value)) => Some(ProviderInput::Text(value.clone())),
                            _ => None,
                        },
                    };
                    let bound = call.arguments_bound && !call.implicit_receiver && !call.verified;
                    (
                        call.caller,
                        call.at,
                        call.conditional,
                        receiver.filter(|_| bound),
                    )
                })
                .collect();
            if calls.is_empty() || calls.len() > MAX_PROVIDER_CALLS {
                reach.resolved_source_provider_calls.remove(&owner);
                continue;
            }
            let mut complete = !reach
                .evidence
                .range((owner, "", "")..)
                .take_while(|&&(target, _, _)| target == owner)
                .any(|&(_, kind, _)| !matches!(kind, "direct_call" | "type_reference"));
            for (caller, at, conditional, input) in calls {
                let Some((target, class)) = input.and_then(|input| {
                    self.source_provider_target(site, input, (caller, at), &invalidated)
                }) else {
                    complete = false;
                    continue;
                };
                self.direct.insert(target);
                let certainty = if possible || conditional {
                    "possible"
                } else {
                    "certain"
                };
                reach
                    .deferred_method_receivers
                    .entry(target)
                    .or_default()
                    .insert(class);
                if !reach.evidence.contains(&(target, "direct_call", certainty)) {
                    pending.push_back(Edge {
                        target,
                        kind: "direct_call",
                        certainty,
                    });
                }
            }
            if complete {
                reach.resolved_source_provider_calls.insert(owner);
            } else {
                reach.resolved_source_provider_calls.remove(&owner);
            }
        }
        self.source_provider_invalidated = Some(invalidated);
    }

    fn source_provider_target(
        &mut self,
        site: SourceProviderCall<'a>,
        input: ProviderInput,
        access: (ScopeId, Location),
        invalidated: &BTreeSet<SymbolId>,
    ) -> Option<(SymbolId, SymbolId)> {
        let class = match (site.receiver, input) {
            (ProviderReceiver::Argument, ProviderInput::Class(class)) => class,
            (
                ProviderReceiver::Factory {
                    expression,
                    before,
                    selector,
                },
                ProviderInput::Text(text),
            ) => {
                let (class, factory) = self.literal_factory_instance(
                    site.scope,
                    expression,
                    before,
                    Some((selector, &text)),
                )?;
                if invalidated.contains(&factory) {
                    return None;
                }
                class
            }
            _ => return None,
        };
        if !self.plain_default_provider(class, access, invalidated) {
            return None;
        }
        let Some(Value::Callable(invocation)) =
            self.class_member_at(class, site.member, 0, Some(access))
        else {
            return None;
        };
        let Binding::Definition(method, _) = self.symbols[invocation.target].binding else {
            return None;
        };
        (!invocation.verified
            && matches!(invocation.receiver, Receiver::Ordinary)
            && !invalidated.contains(&invocation.target)
            && !self.receiver_conflicts(invocation.bind_receiver(Receiver::Bound(class)))
            && field_dispatch::plain_method(method, site.supplied + 1))
        .then_some((invocation.target, class))
    }

    pub(super) fn report_source_provider_calls(&mut self, reach: &Reachability) {
        for (&owner, site) in &self.source_provider_calls {
            if reach.scopes.contains(&site.scope)
                && !reach.resolved_source_provider_calls.contains(&owner)
            {
                self.issues.push(
                    Issue::new(
                        "unresolved_call",
                        &self.files[self.scopes[site.scope].file].path,
                        "provider dispatch requires a bounded fresh instance",
                    )
                    .at(site.at),
                );
            }
        }
    }
}

enum ProviderInput {
    Class(SymbolId),
    Text(String),
}

fn wrapper_return(definition: &Definition) -> Option<(Location, &Expression, Option<&Statement>)> {
    if !field_dispatch::plain_method(definition, definition.parameters.len()) {
        return None;
    }
    let (returned, assignment) = match body_without_docstring(definition) {
        [returned] => (returned, None),
        [assignment, returned] => (returned, Some(assignment)),
        _ => return None,
    };
    let StatementKind::Return(Some(expression)) = &returned.kind else {
        return None;
    };
    Some((returned.at, expression, assignment))
}

fn provider_member(expression: &Expression) -> Option<(&str, &str, &[Expression])> {
    let ExpressionKind::Call {
        function,
        arguments,
        keywords,
    } = &expression.kind
    else {
        return None;
    };
    let ExpressionKind::Member(receiver, member) = &function.kind else {
        return None;
    };
    let ExpressionKind::Name(receiver) = &receiver.kind else {
        return None;
    };
    keywords.is_empty().then_some((receiver, member, arguments))
}
