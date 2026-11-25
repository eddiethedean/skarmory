//! Selected function views forwarding a literal selector into a source dispatcher.

use super::*;

pub(super) type ViewArguments = BTreeMap<String, String>;
pub(super) type ViewRouteArguments = BTreeMap<(Object, SymbolId), Vec<Option<ViewArguments>>>;

struct ViewForward {
    view: SymbolId,
    caller: ScopeId,
    at: Location,
    selectors: BTreeSet<String>,
}

impl Graph<'_> {
    pub(super) fn django_view_provider_failed(&self, target: SymbolId) -> bool {
        let file = self.scopes[self.symbols[target].scope].file;
        self.failed_terminal_modules.contains(&file)
            || self.scopes[file]
                .body
                .iter()
                .any(|statement| matches!(statement.kind, StatementKind::Raise(_)))
    }

    pub(super) fn resolve_django_view_dispatch(&mut self, reach: &Reachability) -> bool {
        let candidates: BTreeSet<_> = self
            .django_view_arguments
            .iter()
            .filter_map(|(&(object, target), options)| {
                (options.iter().any(Option::is_some) && reach.active_objects.contains(&object))
                    .then_some(target)
            })
            .collect();
        let mut dispatchers = BTreeMap::<SymbolId, Vec<ViewForward>>::new();
        for candidate in candidates {
            if let Some((dispatcher, forward)) = self.closed_django_view_forward(candidate, reach) {
                dispatchers.entry(dispatcher).or_default().push(forward);
            }
        }
        if dispatchers.is_empty() {
            return false;
        }
        let replaced = self.field_callback_replacements();
        let mut changed = false;
        for (dispatcher, forwards) in dispatchers {
            if replaced.contains(&dispatcher)
                || forwards.iter().any(|entry| replaced.contains(&entry.view))
            {
                continue;
            }
            let Some((scope, proofs)) = self.prove_view_dispatcher(dispatcher, &forwards, reach)
            else {
                continue;
            };
            // The sole dispatcher expression was initially walked without
            // arguments. Its ordinary method edge is superseded by the proof.
            let method = proofs[0].method.target;
            self.scopes[scope]
                .edges
                .retain(|edge| edge.target != method || edge.kind != "direct_call");
            self.fresh_field_invocations.extend(proofs);
            changed = true;
        }
        if changed {
            self.resolve_fresh_field_invocations();
        }
        changed
    }

    fn closed_django_view_forward(
        &mut self,
        view: SymbolId,
        reach: &Reachability,
    ) -> Option<(SymbolId, ViewForward)> {
        if !reach.has_evidence(view, "django_registration")
            || reach
                .evidence
                .range((view, "", "")..)
                .take_while(|(target, _, _)| *target == view)
                .any(|(_, kind, _)| *kind != "django_registration")
        {
            return None;
        }
        let Binding::Definition(definition, caller) = self.symbols[view].binding else {
            return None;
        };
        if self
            .scope_issues
            .iter()
            .any(|(scope, _)| *scope == self.scopes[caller].file)
        {
            return None;
        }
        let [_, selector] = definition.parameters.as_slice() else {
            return None;
        };
        let (returned, before) = plain_return(definition)?;
        let ExpressionKind::Call {
            function,
            arguments,
            keywords,
        } = &returned.kind
        else {
            return None;
        };
        if !keywords.is_empty()
            || !matches!(arguments.as_slice(), [arg] if matches!(&arg.kind,
                ExpressionKind::Name(name) if name == &selector.text))
            || !matches!(function.kind, ExpressionKind::Name(_))
            || !self.only_direct_callers(view, false)
        {
            return None;
        }
        let Value::Callable(invocation) = self.probe_identity(caller, function, before) else {
            return None;
        };
        if invocation.verified
            || !matches!(invocation.receiver, Receiver::Ordinary)
            || invocation.implicit_descriptor_receiver
            || self.django_view_provider_failed(view)
            || self.django_view_provider_failed(invocation.target)
        {
            return None;
        }
        let default = match definition.defaults.as_slice() {
            [
                Expression {
                    kind: ExpressionKind::Text(text),
                    ..
                },
            ] => Some(text),
            [] => None,
            _ => return None,
        };
        let mut selectors = BTreeSet::new();
        for (&(object, _), options) in self
            .django_view_arguments
            .iter()
            .filter(|(key, _)| key.1 == view)
        {
            // Resolver captures and default arguments on an enclosing include
            // can override this leaf's arguments. Keep that boundary open.
            if !self.django_roots.contains(&object)
                || self
                    .scope_issues
                    .iter()
                    .any(|(scope, _)| *scope == object.0)
                || self
                    .django_registration_issues
                    .iter()
                    .any(|(parent, _)| *parent == object)
                || self
                    .scopes
                    .iter()
                    .any(|row| row.mounts.iter().any(|&(_, child)| child == object))
            {
                return None;
            }
            for option in options {
                let arguments = option.as_ref()?;
                let value = if arguments.is_empty() {
                    default?
                } else if arguments.len() == 1 {
                    arguments.get(&selector.text)?
                } else {
                    return None;
                };
                selectors.insert(value.clone());
            }
        }
        (!selectors.is_empty()).then_some((
            invocation.target,
            ViewForward {
                view,
                caller,
                at: returned.at,
                selectors,
            },
        ))
    }

    fn prove_view_dispatcher(
        &mut self,
        dispatcher: SymbolId,
        forwards: &[ViewForward],
        reach: &Reachability,
    ) -> Option<(ScopeId, Vec<field_dispatch::FreshFieldInvocation>)> {
        let callers = self.direct_arguments.get(&dispatcher)?;
        if !self.only_direct_callers(dispatcher, true)
            || reach
                .evidence
                .range((dispatcher, "", "")..)
                .take_while(|(target, _, _)| *target == dispatcher)
                .any(|(_, kind, _)| !matches!(*kind, "direct_call" | "type_reference"))
            || callers.len() != forwards.len()
            || callers.iter().any(|call| {
                !call.arguments_bound
                    || !forwards.iter().any(|known| {
                        (call.caller, position(call.at)) == (known.caller, position(known.at))
                    })
            })
        {
            return None;
        }
        let Binding::Definition(definition, body) = self.symbols[dispatcher].binding else {
            return None;
        };
        let [parameter] = definition.parameters.as_slice() else {
            return None;
        };
        if !definition.defaults.is_empty() {
            return None;
        }
        let (returned, before) = plain_return(definition)?;
        let arguments: BTreeSet<_> = forwards.iter().flat_map(|known| &known.selectors).collect();
        let proofs: Vec<_> = arguments
            .into_iter()
            .map(|selector| {
                self.prove_selected_field_call(body, returned, before, &parameter.text, selector)
            })
            .collect::<Option<_>>()?;
        (!proofs.is_empty()).then_some((body, proofs))
    }

    fn only_direct_callers(&self, target: SymbolId, allow_direct: bool) -> bool {
        self.scopes.iter().all(|row| {
            row.edges.iter().all(|edge| {
                edge.target != target
                    || edge.kind == "type_reference"
                    || allow_direct && edge.kind == "direct_call"
            })
        })
    }
}

fn plain_return(definition: &Definition) -> Option<(&Expression, Location)> {
    if definition.kind != DefinitionKind::Function
        || definition.is_generator
        || !definition.simple_positional_parameters
        || !definition.decorators.is_empty()
    {
        return None;
    }
    let [
        Statement {
            kind: StatementKind::Return(Some(value)),
            at,
        },
    ] = body_without_docstring(definition)
    else {
        return None;
    };
    Some((value, *at))
}
