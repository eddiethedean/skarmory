//! Django's finite literal choices and their immediate model-field consumer.

use super::*;

const MAX_CHOICE_MEMBERS: usize = 128;

pub(super) fn is_declaration_provider(provider: &str) -> bool {
    matches!(
        provider,
        "django.db.models.IntegerChoices"
            | "django.db.models.TextChoices"
            | "django.db.models.enums.IntegerChoices"
            | "django.db.models.enums.TextChoices"
            | "django.db.models.enums.Choices"
            | "django.db.models.enums.ChoicesMeta"
            | "django.db.models.enums.ChoicesType"
            | "django.db.models.enums"
            | "django.db.models.base.ModelBase"
    )
}

impl Graph<'_> {
    pub(super) fn record_django_choice_declaration(&mut self, class: SymbolId) -> bool {
        let Some(members) = self.django_choice_members(class) else {
            return false;
        };
        let scope = self.symbols[class].scope;
        for member in members {
            self.edge(scope, member, "django_choice_member", "certain");
        }
        true
    }

    pub(super) fn django_direct_field_choices(
        &mut self,
        call: CallArguments<'_>,
        expression: &Expression,
    ) -> Option<()> {
        let ExpressionKind::Member(receiver, name) = &expression.kind else {
            return None;
        };
        if name != "choices" {
            return None;
        }
        let Value::Callable(choice) = self.probe_identity(call.scope, receiver, call.before) else {
            return None;
        };
        let model = self.scopes[call.scope].owner?;
        let Binding::Definition(definition, _) = self.symbols[model].binding else {
            return None;
        };
        let [base] = definition.bases.as_slice() else {
            return None;
        };
        if definition.has_class_keywords()
            || !definition.decorators.is_empty()
            || choice.verified
            || self.symbols[choice.target].scope != call.scope
            || self.has_unmodeled_class_write(choice.target)
            || !self
                .class_discovery_writes(choice.target, call.scope)
                .is_empty()
            || !matches!(
                self.probe_identity(self.symbols[model].scope, base, definition.name.at),
                Value::Provider(provider)
                    if matches!(provider.as_str(), "django.db.models.Model" | "django.db.models.base.Model")
            )
        {
            return None;
        }
        // Adjacent statements keep the enum local and unchanged before the field reads it.
        if !definition.body.windows(2).any(|pair| {
            matches!(&pair[0].kind, StatementKind::Define(choice_definition)
                if position(choice_definition.name.at) == position(self.symbols[choice.target].name.at))
                && matches!(&pair[1].kind, StatementKind::Assign { value: Some(value), .. }
                    if position(value.at) == position(call.at))
        }) {
            return None;
        }
        self.django_choice_members(choice.target).map(|_| ())
    }

    fn django_choice_members(&mut self, class: SymbolId) -> Option<Vec<SymbolId>> {
        let symbol = &self.symbols[class];
        let Binding::Definition(definition, body_scope) = symbol.binding else {
            return None;
        };
        let [base] = definition.bases.as_slice() else {
            return None;
        };
        let body = body_without_docstring(definition);
        if self.modules.contains_key("django")
            || definition.kind != DefinitionKind::Class
            || definition.has_class_keywords()
            || !definition.decorators.is_empty()
            || !definition.body_coverage_lines.is_empty()
            || body.is_empty()
            || body.len() > MAX_CHOICE_MEMBERS
        {
            return None;
        }
        let Value::Provider(provider) = self.probe_identity(symbol.scope, base, definition.name.at)
        else {
            return None;
        };
        let integers = match provider.as_str() {
            "django.db.models.IntegerChoices" | "django.db.models.enums.IntegerChoices" => true,
            "django.db.models.TextChoices" | "django.db.models.enums.TextChoices" => false,
            _ => return None,
        };
        let mut members = Vec::new();
        let mut names = BTreeSet::new();
        let mut values = BTreeSet::new();
        for statement in body {
            let StatementKind::Assign {
                names: targets,
                value:
                    Some(Expression {
                        kind: ExpressionKind::Sequence { items, list: false },
                        ..
                    }),
                annotation: None,
            } = &statement.kind
            else {
                return None;
            };
            let [name] = targets.as_slice() else {
                return None;
            };
            let [value, label] = items.as_slice() else {
                return None;
            };
            let key = match (&value.kind, integers) {
                (ExpressionKind::Scalar(Some(value)), true) => (Some(*value), None),
                (ExpressionKind::Text(value), false) => (None, Some(value.as_str())),
                _ => return None,
            };
            if !matches!(label.kind, ExpressionKind::Text(_))
                || !name.text.is_ascii()
                || !name
                    .text
                    .as_bytes()
                    .first()
                    .is_some_and(u8::is_ascii_uppercase)
                || !names.insert(name.text.as_str())
                || !values.insert(key)
            {
                return None;
            }
            members.push(
                *self
                    .by_location
                    .get(&(body_scope, name.at.line, name.at.column))?,
            );
        }
        Some(members)
    }
}
