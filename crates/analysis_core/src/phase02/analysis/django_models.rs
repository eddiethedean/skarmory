//! Bounded allocation and manager dispatch for source-owned Django ORM classes.

use super::*;

impl Graph<'_> {
    pub(super) fn bounded_django_model_constructor(
        &mut self,
        class: SymbolId,
        call: CallArguments<'_>,
    ) -> bool {
        if !call.positional.is_empty()
            || !call.keywords.is_empty()
            || !self.scopes[call.scope].starts_django.iter().any(|startup| {
                startup.definite_setup
                    && startup.settings.is_some()
                    && position(startup.at) < position(call.before)
            })
        {
            return false;
        }
        // Only the provider's implicit primary key is initialized. Explicit
        // fields can evaluate user defaults or run custom descriptors.
        let (lineage, _) = self.class_lineage(class);
        let [body] = lineage.as_slice() else {
            return false;
        };
        let body = *body;
        let Binding::Definition(definition, _) = self.symbols[class].binding else {
            return false;
        };
        if definition.has_class_keywords()
            || !definition.decorators.is_empty()
            || self.has_unmodeled_class_write(class)
            || !self.class_discovery_writes(class, call.scope).is_empty()
            || self.scopes[body].names.values().any(|ids| ids.len() != 1)
        {
            return false;
        }
        let mut explicit_app_label = false;
        for statement in body_without_docstring(definition) {
            let StatementKind::Define(member) = &statement.kind else {
                return false;
            };
            match member.kind {
                DefinitionKind::Class
                    if member.name.text == "Meta"
                        && member.bases.is_empty()
                        && member.decorators.is_empty()
                        && !member.has_class_keywords() =>
                {
                    explicit_app_label = matches!(
                        body_without_docstring(member),
                        [Statement {
                            kind: StatementKind::Assign {
                                names,
                                value: Some(Expression { kind: ExpressionKind::Text(value), .. }),
                                ..
                            },
                            ..
                        }] if names.len() == 1 && names[0].text == "app_label" && !value.is_empty()
                    );
                }
                DefinitionKind::Function
                    if member.decorators.is_empty()
                        && !member.name.text.starts_with('_')
                        && !matches!(member.name.text.as_str(), "id" | "pk" | "objects") => {}
                _ => return false,
            }
        }
        explicit_app_label
            && self.local(body, "Meta", END).is_some_and(|meta| {
                !self.has_unmodeled_class_write(meta)
                    && self.class_discovery_writes(meta, call.scope).is_empty()
            })
    }

    pub(super) fn record_django_manager_all(&mut self, class: SymbolId, call: CallArguments<'_>) {
        let access = (call.scope, call.before);
        if call.positional.is_empty()
            && call.keywords.is_empty()
            && self.class_lineage(class).0.into_iter().all(|body| {
                self.scopes[body].owner.is_some_and(|owner| {
                    !self.has_unmodeled_class_write(owner)
                        && ["__getattribute__", "__getattr__", "__setattr__"]
                            .into_iter()
                            .all(|name| {
                                self.local(body, name, END).is_none()
                                    && self.class_member_write(owner, name, access).is_none()
                            })
                })
            })
            && let Some(Value::Callable(method)) =
                self.class_member_at(class, "get_queryset", 0, Some(access))
            && matches!(self.symbols[method.target].binding, Binding::Definition(definition, _)
                if field_dispatch::plain_method(definition, 1))
        {
            let conditional = self.branch_depth != 0
                || self.scopes[call.scope]
                    .owner
                    .is_some_and(|owner| self.conditional.contains_key(&owner));
            self.invoke(
                call.scope,
                method.bind_receiver(Receiver::Bound(class)),
                "django_manager_queryset",
                if conditional { "possible" } else { "certain" },
                call.at,
            );
            return;
        }
        self.scope_issue(
            call.scope,
            call.at,
            "unresolved_call",
            "Django Manager.all requires a bounded get_queryset method",
        );
    }
}
