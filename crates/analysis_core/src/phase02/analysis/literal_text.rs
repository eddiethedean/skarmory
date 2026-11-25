//! Bounded literal string operations and string-enum iteration.

use super::*;

const MAX_ENUM_MEMBERS: usize = 128;
const MAX_TEXT_BYTES: usize = 65_536;

pub(super) struct TextCall<'a> {
    pub(super) value: String,
    enum_projection: Option<EnumProjection<'a>>,
}

struct EnumProjection<'a> {
    iterable: &'a Expression,
    members: Vec<(SymbolId, &'a str)>,
}

impl<'a> Graph<'a> {
    pub(super) fn literal_string_enum(&mut self, class: SymbolId) -> bool {
        self.string_enum_members(class, 0).is_some()
    }

    fn string_enum_members(
        &mut self,
        class: SymbolId,
        depth: usize,
    ) -> Option<Vec<(SymbolId, &'a str)>> {
        let symbol = &self.symbols[class];
        let declaration_scope = symbol.scope;
        let Binding::Definition(definition, class_scope) = symbol.binding else {
            return None;
        };
        let [text_base, enum_base] = definition.bases.as_slice() else {
            return None;
        };
        let body = body_without_docstring(definition);
        if definition.kind != DefinitionKind::Class
            || definition.table_keyword.is_some()
            || !definition.decorators.is_empty()
            || body.is_empty()
            || body.len() > MAX_ENUM_MEMBERS
        {
            return None;
        }
        let mut names = BTreeSet::new();
        let mut values = BTreeSet::new();
        let mut members = Vec::new();
        for statement in body {
            let StatementKind::Assign {
                names: targets,
                value:
                    Some(Expression {
                        kind: ExpressionKind::Text(value),
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
            if !name
                .text
                .as_bytes()
                .first()
                .is_some_and(u8::is_ascii_uppercase)
                || !names.insert(name.text.as_str())
            {
                return None;
            }
            // Enum iteration omits aliases and preserves first declaration order.
            if values.insert(value.as_str()) {
                members.push((
                    *self
                        .by_location
                        .get(&(class_scope, name.at.line, name.at.column))?,
                    value.as_str(),
                ));
            }
        }
        let at = definition.name.at;
        if self.scopes[self.scopes[declaration_scope].file].open_wildcard
            || self
                .module_member_write("builtins", "str", (declaration_scope, at))
                .is_some()
            || !matches!(self.identity_with_dormant_declarations(declaration_scope, text_base, at, depth + 1),
                Value::Provider(provider) if provider == "builtins.str")
            || !matches!(self.identity_with_dormant_declarations(declaration_scope, enum_base, at, depth + 1),
                Value::Provider(provider) if provider == "enum.Enum")
        {
            return None;
        }
        Some(members)
    }

    fn enum_projection<'b>(
        &mut self,
        call: CallArguments<'b>,
        argument: &'b Expression,
        depth: usize,
    ) -> Option<EnumProjection<'b>>
    where
        'a: 'b,
    {
        let ExpressionKind::Comprehension {
            form: ComprehensionForm::Generator,
            iterators,
            elements,
        } = &argument.kind
        else {
            return None;
        };
        let [iterator] = iterators.as_slice() else {
            return None;
        };
        let [
            Expression {
                kind: ExpressionKind::Member(base, member),
                ..
            },
        ] = elements.as_slice()
        else {
            return None;
        };
        if !iterator.filters.is_empty()
            || member != "value"
            || !matches!(base.kind, ExpressionKind::IterationVariable(at)
                if position(at) == position(iterator.target))
        {
            return None;
        }
        let Value::Callable(class) =
            self.identity(call.scope, &iterator.iterable, call.before, depth + 1)
        else {
            return None;
        };
        if class.verified
            || !self
                .class_discovery_writes(class.target, call.scope)
                .is_empty()
        {
            return None;
        }
        Some(EnumProjection {
            iterable: &iterator.iterable,
            members: self.string_enum_members(class.target, depth + 1)?,
        })
    }

    pub(super) fn literal_text_call<'b>(
        &mut self,
        function: &Expression,
        call: CallArguments<'b>,
        depth: usize,
    ) -> Option<TextCall<'b>>
    where
        'a: 'b,
    {
        let ExpressionKind::Member(base, method) = &function.kind else {
            return None;
        };
        if !call.keywords.is_empty() || !matches!(method.as_str(), "format" | "join") {
            return None;
        }
        let [argument] = call.positional else {
            return None;
        };
        let Value::Text(receiver) = self.identity(call.scope, base, call.before, depth + 1) else {
            return None;
        };
        if method == "join" {
            let projection = self.enum_projection(call, argument, depth + 1)?;
            let separator_count = projection.members.len().saturating_sub(1);
            let total = projection
                .members
                .iter()
                .map(|(_, value)| value.len())
                .sum::<usize>();
            if total.saturating_add(receiver.len().saturating_mul(separator_count)) > MAX_TEXT_BYTES
            {
                return None;
            }
            let members: Vec<_> = projection.members.iter().map(|(_, value)| *value).collect();
            return Some(TextCall {
                value: members.join(&receiver),
                enum_projection: Some(projection),
            });
        }

        let (prefix, suffix) = receiver.split_once("{}")?;
        if prefix.contains(['{', '}']) || suffix.contains(['{', '}']) {
            return None;
        }
        let Value::Text(argument) = self.identity(call.scope, argument, call.before, depth + 1)
        else {
            return None;
        };
        if receiver.len().saturating_add(argument.len()) > MAX_TEXT_BYTES {
            return None;
        }
        Some(TextCall {
            value: [prefix, &argument, suffix].concat(),
            enum_projection: None,
        })
    }

    pub(super) fn walk_literal_text_call(
        &mut self,
        function: &Expression,
        call: CallArguments<'_>,
    ) -> bool {
        let Some(result) = self.literal_text_call(function, call, 0) else {
            return false;
        };
        self.walk_expression(call.scope, function, call.before, true);
        if let Some(projection) = result.enum_projection {
            self.walk(call.scope, projection.iterable, call.before);
            self.read
                .extend(projection.members.into_iter().map(|(symbol, _)| symbol));
        } else {
            self.walk(call.scope, &call.positional[0], call.before);
        }
        true
    }
}
