//! Evaluate the portions of bounded comprehensions that run immediately.

use super::*;

const MAX_COMPREHENSION_STEPS: usize = 128;

struct Evaluation<'a> {
    scope: ScopeId,
    before: Location,
    at: Location,
    form: ComprehensionForm,
    elements: &'a [Expression],
    remaining: usize,
}

impl Graph<'_> {
    pub(super) fn scalar_callback_comprehension(
        &mut self,
        scope: ScopeId,
        expression: &Expression,
        before: Location,
        depth: usize,
    ) -> Option<Vec<Invocation>> {
        let ExpressionKind::Comprehension {
            form: ComprehensionForm::List | ComprehensionForm::Generator,
            iterators,
            elements,
        } = &expression.kind
        else {
            return None;
        };
        if self.scopes[scope]
            .owner
            .is_some_and(|owner| self.is_class(owner))
        {
            return None;
        }
        let [iterator] = iterators.as_slice() else {
            return None;
        };
        let ExpressionKind::Sequence { items, .. } = &iterator.iterable.kind else {
            return None;
        };
        let [
            Expression {
                kind:
                    ExpressionKind::Call {
                        function,
                        arguments,
                        keywords,
                    },
                ..
            },
        ] = elements.as_slice()
        else {
            return None;
        };
        if items.len() > MAX_COMPREHENSION_STEPS
            || !iterator.filters.is_empty()
            || !arguments.is_empty()
            || !keywords.is_empty()
            || !matches!(function.kind, ExpressionKind::IterationVariable(binding)
                if position(binding) == position(iterator.target))
        {
            return None;
        }
        items
            .iter()
            .map(|item| {
                if !matches!(item.kind, ExpressionKind::Name(_)) {
                    return None;
                }
                let Value::Callable(invocation) = self.identity(scope, item, before, depth + 1)
                else {
                    return None;
                };
                let Binding::Definition(definition, _) = self.symbols[invocation.target].binding
                else {
                    return None;
                };
                (matches!(invocation.receiver, Receiver::Ordinary)
                    && definition.kind == DefinitionKind::Function
                    && !definition.is_generator
                    && definition.parameters.is_empty()
                    && definition.decorators.is_empty()
                    && matches!(
                        body_without_docstring(definition),
                        [Statement {
                            kind: StatementKind::Return(
                                None | Some(Expression {
                                    kind: ExpressionKind::None
                                        | ExpressionKind::Bool(_)
                                        | ExpressionKind::Scalar(_)
                                        | ExpressionKind::Text(_),
                                    ..
                                })
                            ),
                            ..
                        }]
                    ))
                .then_some(invocation)
            })
            .collect()
    }

    pub(super) fn callback_generator_return(
        &mut self,
        invocation: Invocation,
        depth: usize,
    ) -> Option<Vec<Invocation>> {
        let Binding::Definition(factory, body) = self.symbols[invocation.target].binding else {
            return None;
        };
        if !matches!(invocation.receiver, Receiver::Ordinary)
            || factory.kind != DefinitionKind::Function
            || factory.is_generator
            || !factory.parameters.is_empty()
            || !factory.decorators.is_empty()
        {
            return None;
        }
        let [
            prefix @ ..,
            Statement {
                at,
                kind: StatementKind::Return(Some(expression)),
            },
        ] = body_without_docstring(factory)
        else {
            return None;
        };
        if !matches!(
            expression.kind,
            ExpressionKind::Comprehension {
                form: ComprehensionForm::Generator,
                ..
            }
        ) || !prefix.iter().all(|statement| {
            matches!(&statement.kind,
                StatementKind::Define(definition)
                    if definition.kind == DefinitionKind::Function
                        && definition.decorators.is_empty()
                        && definition.defaults.is_empty()
                        && definition.annotations.is_empty()
            )
        }) {
            return None;
        }
        self.scalar_callback_comprehension(body, expression, *at, depth)
    }

    pub(super) fn walk_comprehension(
        &mut self,
        scope: ScopeId,
        before: Location,
        at: Location,
        form: ComprehensionForm,
        iterators: &[ComprehensionIterator],
        elements: &[Expression],
    ) {
        if !self.phase03 {
            self.scope_issue(
                scope,
                at,
                "unsupported_construct",
                "comprehensions are outside this compatibility profile",
            );
            return;
        }
        let mut evaluation = Evaluation {
            scope,
            before,
            at,
            form,
            elements,
            remaining: MAX_COMPREHENSION_STEPS,
        };
        if matches!(form, ComprehensionForm::Generator) {
            // Python evaluates the first iterable and creates its iterator
            // while constructing a generator expression. Its body is deferred.
            if let Some(first) = iterators.first() {
                self.comprehension_iterable(&evaluation, &first.iterable);
            }
        } else if self.scopes[scope]
            .owner
            .is_some_and(|owner| self.is_class(owner))
        {
            if let Some(first) = iterators.first() {
                self.comprehension_iterable(&evaluation, &first.iterable);
            }
            self.scope_issue(
                scope,
                at,
                "unsupported_construct",
                "class-body comprehension scope is outside the bounded model",
            );
        } else {
            self.walk_comprehension_iterators(&mut evaluation, iterators);
        }
    }

    pub(super) fn report_unmodeled_deferred_value(
        &mut self,
        scope: ScopeId,
        at: Location,
        value: &Value,
    ) {
        let (kind, message) = match value {
            Value::DeferredComprehension | Value::CallbackGenerator(_) => (
                "unresolved_generator_execution",
                "resuming a generator expression is outside the bounded model",
            ),
            Value::ComprehensionResult => (
                "unsupported_construct",
                "use of a comprehension result is outside the bounded model",
            ),
            Value::DeferredLambda => (
                "unresolved_lambda_execution",
                "executing an anonymous callable is outside the bounded model",
            ),
            _ => return,
        };
        self.scope_issue(scope, at, kind, message);
    }

    fn comprehension_iterable(
        &mut self,
        evaluation: &Evaluation<'_>,
        iterable: &Expression,
    ) -> Value {
        self.walk(evaluation.scope, iterable, evaluation.before);
        let value = self.identity(evaluation.scope, iterable, evaluation.before, 0);
        if value.contains_callback_generator() {
            self.report_unmodeled_deferred_value(
                evaluation.scope,
                iterable.at,
                &Value::DeferredComprehension,
            );
        } else if matches!(
            value,
            Value::DeferredComprehension | Value::ComprehensionResult
        ) {
            self.report_unmodeled_deferred_value(evaluation.scope, iterable.at, &value);
        } else if !matches!(
            value,
            Value::Sequence(_) | Value::Text(_) | Value::Range | Value::BuiltinIterable
        ) && (self.has_iteration_hooks || matches!(value, Value::Instance(_)))
        {
            self.scope_issue(
                evaluation.scope,
                iterable.at,
                "unresolved_iteration",
                "comprehension iterator dispatch is outside the bounded model",
            );
        }
        value
    }

    fn walk_comprehension_iterators(
        &mut self,
        evaluation: &mut Evaluation<'_>,
        iterators: &[ComprehensionIterator],
    ) {
        let Some((iterator, rest)) = iterators.split_first() else {
            self.walk_comprehension_elements(evaluation);
            return;
        };
        let iterable = self.comprehension_iterable(evaluation, &iterator.iterable);
        let items = match iterable {
            Value::Sequence(items)
                if matches!(iterator.iterable.kind, ExpressionKind::Sequence { .. }) =>
            {
                items
            }
            Value::Text(text) => text
                .chars()
                .take(evaluation.remaining + 1)
                .map(|item| Value::Text(item.to_string()))
                .collect(),
            _ => {
                self.branch_depth += 1;
                self.walk_comprehension_iteration(evaluation, iterator, rest, Value::Unknown);
                self.branch_depth -= 1;
                return;
            }
        };
        for item in items {
            if evaluation.remaining == 0 {
                self.scope_issue(
                    evaluation.scope,
                    evaluation.at,
                    "analysis_limit",
                    "comprehension evaluation limit exceeded",
                );
                break;
            }
            self.walk_comprehension_iteration(evaluation, iterator, rest, item);
        }
    }

    fn walk_comprehension_iteration(
        &mut self,
        evaluation: &mut Evaluation<'_>,
        iterator: &ComprehensionIterator,
        rest: &[ComprehensionIterator],
        item: Value,
    ) {
        if evaluation.remaining == 0 {
            self.scope_issue(
                evaluation.scope,
                evaluation.at,
                "analysis_limit",
                "comprehension evaluation limit exceeded",
            );
            return;
        }
        evaluation.remaining -= 1;
        let before_filters = self.branch_depth;
        self.iteration_locals
            .push((position(iterator.target), item));
        let mut reached = true;
        for filter in &iterator.filters {
            let value = self.walk_if_test(evaluation.scope, filter, evaluation.before);
            let value = value.or_else(|| {
                match self.identity(evaluation.scope, filter, evaluation.before, 0) {
                    Value::Sequence(items)
                        if matches!(filter.kind, ExpressionKind::Sequence { .. }) =>
                    {
                        Some(!items.is_empty())
                    }
                    Value::Text(text) => Some(!text.is_empty()),
                    _ => None,
                }
            });
            match value {
                Some(false) => {
                    reached = false;
                    break;
                }
                Some(true) => {}
                None => self.branch_depth += 1,
            }
        }
        if reached {
            self.walk_comprehension_iterators(evaluation, rest);
        }
        self.iteration_locals.pop();
        self.branch_depth = before_filters;
    }

    fn walk_comprehension_elements(&mut self, evaluation: &Evaluation<'_>) {
        for element in evaluation.elements {
            self.walk(evaluation.scope, element, evaluation.before);
        }
        if matches!(
            evaluation.form,
            ComprehensionForm::Set | ComprehensionForm::Dictionary
        ) && (self.has_hash_hooks || self.has_equality_hooks)
            && let Some(key) = evaluation.elements.first()
            && !self
                .identity(evaluation.scope, key, evaluation.before, 0)
                .is_builtin_scalar()
        {
            self.scope_issue(
                evaluation.scope,
                key.at,
                "unresolved_comparison",
                "comprehension key hashing or equality is outside the bounded model",
            );
        }
    }
}
