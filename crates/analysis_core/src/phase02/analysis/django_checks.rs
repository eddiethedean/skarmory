use super::*;

pub(super) const CALLBACK: &str = "django_system_check";

#[derive(Default)]
pub(super) struct Checks {
    registrations: BTreeMap<ScopeId, Vec<Registration>>,
    runs: Vec<Run>,
}

struct Registration {
    at: Location,
    target: SymbolId,
    tags: BTreeSet<String>,
}

struct Run {
    scope: ScopeId,
    at: Location,
    tags: Option<BTreeSet<String>>,
}

pub(super) fn is_register(provider: &str) -> bool {
    matches!(
        provider,
        "django.core.checks.register" | "django.core.checks.registry.register"
    )
}

pub(super) fn is_run_checks(provider: &str) -> bool {
    matches!(
        provider,
        "django.core.checks.run_checks" | "django.core.checks.registry.run_checks"
    )
}

impl Graph<'_> {
    pub(super) fn record_django_system_check_decorator(
        &mut self,
        scope: ScopeId,
        target: SymbolId,
        decorator: &Expression,
        before: Location,
    ) -> bool {
        let tags = match &decorator.kind {
            ExpressionKind::Call {
                arguments,
                keywords,
                ..
            } if keywords.is_empty() => self.django_check_tags(scope, before, arguments),
            ExpressionKind::Name(_) | ExpressionKind::Member(..) => Some(BTreeSet::new()),
            _ => None,
        };
        self.add_django_system_check(scope, decorator.at, target, tags, 1)
    }

    pub(super) fn record_django_system_check_registration(&mut self, call: CallArguments<'_>) {
        if let [callback, tags @ ..] = call.positional
            && call.keywords.is_empty()
            && let Value::Callable(invocation) = self.identity(call.scope, callback, call.before, 0)
            && !invocation.verified
            && matches!(invocation.receiver, Receiver::Ordinary)
            && !invocation.implicit_descriptor_receiver
        {
            let tags = self.django_check_tags(call.scope, call.before, tags);
            if self.add_django_system_check(call.scope, call.at, invocation.target, tags, 0) {
                return;
            }
        }
        self.scope_issue(
            call.scope,
            call.at,
            "unresolved_registration",
            "Django system check registration is outside the bounded model",
        );
    }

    fn add_django_system_check(
        &mut self,
        scope: ScopeId,
        at: Location,
        target: SymbolId,
        tags: Option<BTreeSet<String>>,
        decorator_count: usize,
    ) -> bool {
        let Binding::Definition(definition, _) = self.symbols[target].binding else {
            return false;
        };
        if self.branch_depth != 0
            || definition.kind != DefinitionKind::Function
            || definition.is_generator
            || definition.decorators.len() != decorator_count
            || definition.positional_only_parameter_count != 0
            || definition.positional_parameter_count != 1
            || definition.parameters.len() != 2
            || definition.parameters[0].text != "app_configs"
            || !definition.has_variadic_keyword_parameter
            || self.symbols[target].scope != self.scopes[self.symbols[target].scope].file
        {
            return false;
        }
        let Some(tags) = tags else {
            return false;
        };
        self.django_checks
            .registrations
            .entry(scope)
            .or_default()
            .push(Registration { at, target, tags });
        true
    }

    pub(super) fn record_django_system_check_run(&mut self, call: CallArguments<'_>) {
        let tags = match call.keywords {
            [] => Some(None),
            [(keyword, value)] if keyword == "tags" => {
                if let ExpressionKind::Sequence { items, .. } = &value.kind {
                    self.django_check_tags(call.scope, call.before, items)
                        .map(Some)
                } else {
                    None
                }
            }
            _ => None,
        };
        if call.positional.is_empty()
            && self.branch_depth == 0
            && let Some(tags) = tags
        {
            self.django_checks.runs.push(Run {
                scope: call.scope,
                at: call.at,
                tags,
            });
        } else {
            self.scope_issue(
                call.scope,
                call.at,
                "unresolved_registration",
                "Django system check dispatch is outside the bounded model",
            );
        }
    }

    fn django_check_tags(
        &mut self,
        scope: ScopeId,
        before: Location,
        items: &[Expression],
    ) -> Option<BTreeSet<String>> {
        items
            .iter()
            .map(|item| match self.identity(scope, item, before, 0) {
                Value::Text(tag) => Some(tag),
                Value::Provider(provider) => {
                    let name = provider
                        .strip_prefix("django.core.checks.Tags.")
                        .or_else(|| provider.strip_prefix("django.core.checks.registry.Tags."))?;
                    matches!(
                        name,
                        "admin"
                            | "async_support"
                            | "caches"
                            | "commands"
                            | "compatibility"
                            | "database"
                            | "files"
                            | "models"
                            | "security"
                            | "signals"
                            | "sites"
                            | "staticfiles"
                            | "templates"
                            | "translation"
                            | "urls"
                    )
                    .then(|| name.to_owned())
                }
                _ => None,
            })
            .collect()
    }

    /// Prove that registration ran earlier in this scope or in a module it
    /// directly initialized. A shared process registry alone provides no order.
    fn django_check_registration_order(
        &self,
        run: &Run,
        registration_scope: ScopeId,
        registration: &Registration,
    ) -> Option<Option<SymbolId>> {
        if run.scope == registration_scope {
            return (position(registration.at) < position(run.at)).then_some(None);
        }
        if registration_scope != self.scopes[registration_scope].file
            || self.failed_terminal_modules.contains(&registration_scope)
        {
            return None;
        }
        self.scopes[run.scope]
            .initializer_imports
            .iter()
            .find_map(|&(import, module)| {
                (module == registration_scope
                    && position(self.symbols[import].name.at) < position(run.at))
                .then_some(Some(import))
            })
    }

    pub(super) fn collect_django_system_check_edges(&mut self) {
        let mut edges = BTreeSet::new();
        let mut imports = BTreeSet::new();
        for run in &self.django_checks.runs {
            let mut effective = BTreeMap::new();
            for (&scope, registrations) in &self.django_checks.registrations {
                for registration in registrations {
                    let Some(import) =
                        self.django_check_registration_order(run, scope, registration)
                    else {
                        continue;
                    };
                    // Django stores one function object and replaces its tags
                    // on every registration.
                    let entered = import.map_or(registration.at, |id| self.symbols[id].name.at);
                    let order = (position(entered), position(registration.at));
                    if effective
                        .get(&registration.target)
                        .is_none_or(|(previous, _, _)| *previous < order)
                    {
                        effective.insert(registration.target, (order, registration, import));
                    }
                }
            }
            for (_, registration, import) in effective.into_values() {
                if run.selects(registration) {
                    edges.insert((run.scope, registration.target));
                    imports.extend(import);
                }
            }
        }
        self.read.extend(imports);
        for (scope, target) in edges {
            self.edge(scope, target, CALLBACK, "possible");
        }
    }

    pub(super) fn report_django_system_check_order(&mut self, reach: &Reachability) {
        let mut unresolved = Vec::new();
        for run in &self.django_checks.runs {
            if !reach.scopes.contains(&run.scope) {
                continue;
            }
            if self
                .django_checks
                .registrations
                .iter()
                .any(|(&scope, registrations)| {
                    scope != run.scope
                        && reach.scopes.contains(&scope)
                        && registrations.iter().any(|registration| {
                            run.selects(registration)
                                && self
                                    .django_check_registration_order(run, scope, registration)
                                    .is_none()
                        })
                })
            {
                unresolved.push((run.scope, run.at));
            }
        }
        for (scope, at) in unresolved {
            self.scope_issue(
                scope,
                at,
                "unresolved_registration",
                "Django system check registration order could not be established",
            );
        }
    }
}

impl Run {
    fn selects(&self, registration: &Registration) -> bool {
        self.tags
            .as_ref()
            .is_none_or(|tags| !tags.is_disjoint(&registration.tags))
    }
}
