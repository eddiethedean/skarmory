use super::*;

pub(super) const WARNING_CATEGORIES: &[&str] = &[
    "DeprecationWarning",
    "PendingDeprecationWarning",
    "FutureWarning",
];

impl Graph<'_> {
    fn deprecation_import(&self, scope: ScopeId, name: &str, at: Location) -> Option<&Import> {
        let LexicalBinding::Found(id) = self.lexical_binding(scope, name, at, true) else {
            return None;
        };
        let binding = &self.symbols[id];
        let Binding::Import(import) = binding.binding else {
            return None;
        };
        let owner = &self.scopes[binding.scope];
        if import.level != 0
            || import.wildcard_member
            || owner.names.get(name)?.len() != 1
            || self.conditional.contains_key(&id)
            || binding.scope == owner.file && self.has_module_binding_write(owner.file, name)
        {
            return None;
        }
        // For a deferred top-level function, require the imported global to
        // have been established before the definition could escape.
        if binding.scope != scope
            && !self.scopes[scope].owner.is_some_and(|owner| {
                self.symbols[owner].scope == binding.scope
                    && position(binding.name.at) < position(self.symbols[owner].name.at)
            })
        {
            return None;
        }
        Some(import)
    }

    fn deprecation_provider(
        &self,
        scope: ScopeId,
        expression: &Expression,
        at: Location,
        module: &str,
        member: &str,
    ) -> bool {
        let file = self.scopes[scope].file;
        if !self.bounded_deprecation
            || self.scopes[scope].open_wildcard
            || self.scopes[file].open_wildcard
            || self.modules.contains_key(module)
            || self.nested_packages.resolve(file, module) != module
            || self
                .module_member_write(module, member, (scope, at))
                .is_some()
        {
            return false;
        }
        let (import, from_member) = match &expression.kind {
            ExpressionKind::Name(name) => (self.deprecation_import(scope, name, at), Some(member)),
            ExpressionKind::Member(base, name) if name == member => {
                let ExpressionKind::Name(name) = &base.kind else {
                    return false;
                };
                (self.deprecation_import(scope, name, at), None)
            }
            _ => return false,
        };
        import.is_some_and(|import| {
            import.module == module && import.member.as_deref() == from_member
        })
    }

    pub(super) fn deprecation_decorator(
        &self,
        scope: ScopeId,
        definition: &Definition,
        decorator: &Expression,
        at: Location,
    ) -> bool {
        // A preceding decorator may have installed a descriptor instead of
        // the function, and class decoration can change allocation behavior.
        definition.kind == DefinitionKind::Function
            && !definition.is_generator
            && matches!(definition.decorators.as_slice(), [only] if std::ptr::eq(only, decorator))
            && matches!(&decorator.kind, ExpressionKind::Call { function, arguments, keywords }
            if keywords.is_empty() && matches!(arguments.as_slice(), [Expression { kind: ExpressionKind::Text(_), .. }])
                && self.deprecation_provider(scope, function, at, "typing_extensions", "deprecated"))
    }

    fn deprecation_warning(&self, scope: ScopeId, expression: &Expression, at: Location) -> bool {
        let ExpressionKind::Call {
            function,
            arguments,
            keywords,
        } = &expression.kind
        else {
            return false;
        };
        let Some([Some(message), Some(category), stacklevel]) =
            registration_arguments(arguments, keywords, ["message", "category", "stacklevel"])
        else {
            return false;
        };
        if !matches!(message.kind, ExpressionKind::Text(_))
            || stacklevel.is_some_and(
                |value| !matches!(value.kind, ExpressionKind::Scalar(Some(level)) if level > 0),
            )
            || !self.deprecation_provider(scope, function, at, "warnings", "warn")
            || self
                .module_member_write("warnings", "showwarning", (scope, at))
                .is_some()
        {
            return false;
        }
        let file = self.scopes[scope].file;
        WARNING_CATEGORIES.iter().any(|name| {
            (matches!(&category.kind, ExpressionKind::Name(spelling) if spelling == name)
                && matches!(
                    self.lexical_binding(scope, name, at, true),
                    LexicalBinding::Missing
                )
                && !self.scopes[file].names.contains_key("__builtins__")
                && self
                    .module_member_write("builtins", name, (scope, at))
                    .is_none())
                || self.deprecation_provider(scope, category, at, "builtins", name)
        })
    }

    pub(super) fn leading_deprecation_warning(&self, scope: ScopeId) -> Option<Location> {
        let body = self.scopes[scope].body;
        let module = scope == self.scopes[scope].file;
        for (index, statement) in body.iter().enumerate() {
            match &statement.kind {
                StatementKind::Import(_) => {}
                StatementKind::Evaluate(Expression {
                    kind: ExpressionKind::Text(_),
                    ..
                }) if index == 0 => {}
                StatementKind::Assign {
                    value: Some(value),
                    annotation: None,
                    ..
                } if module && passive_metadata(value) => {}
                StatementKind::Evaluate(expression)
                    if self.deprecation_warning(scope, expression, statement.at) =>
                {
                    return Some(statement.at);
                }
                _ => return None,
            }
        }
        None
    }

    pub(super) fn deprecated_definition(
        &self,
        owner: ScopeId,
        body: ScopeId,
        definition: &Definition,
    ) -> bool {
        matches!(definition.body.first().map(|statement| &statement.kind),
            Some(StatementKind::Evaluate(Expression { kind: ExpressionKind::Text(text), .. }))
                if deprecated_docstring(text))
            || definition.kind != DefinitionKind::Class
                && self.leading_deprecation_warning(body).is_some()
            || matches!(definition.decorators.as_slice(), [decorator]
                if self.deprecation_decorator(owner, definition, decorator, definition.name.at))
    }
}

fn passive_metadata(value: &Expression) -> bool {
    fn allowed(value: &Expression, budget: &mut usize) -> bool {
        let Some(remaining) = budget.checked_sub(1) else {
            return false;
        };
        *budget = remaining;
        match &value.kind {
            ExpressionKind::None
            | ExpressionKind::Bool(_)
            | ExpressionKind::Scalar(_)
            | ExpressionKind::Text(_) => true,
            ExpressionKind::Sequence { items, .. } => {
                items.iter().all(|item| allowed(item, budget))
            }
            _ => false,
        }
    }
    allowed(value, &mut 128)
}

fn deprecated_docstring(text: &str) -> bool {
    let text = text.to_ascii_lowercase();
    let starts_word = |text: &str, prefix| {
        text.strip_prefix(prefix).is_some_and(|rest| {
            rest.chars()
                .next()
                .is_none_or(|ch| !ch.is_alphanumeric() && ch != '_')
        })
    };
    let headline = text.trim_start();
    if [
        "deprecated",
        "soft deprecated",
        "soft-deprecated",
        "softdeprecated",
    ]
    .into_iter()
    .any(|prefix| starts_word(headline, prefix))
        || text.lines().any(|line| {
            line.trim_start()
                .strip_prefix("..")
                .is_some_and(|rest| rest.trim_start().starts_with("deprecated::"))
        })
    {
        return true;
    }
    let words: Vec<_> = text.split_whitespace().collect();
    if words.windows(2).enumerate().any(|(i, words_at)| {
        let [qualifier, signal] = words_at else {
            return false;
        };
        if !matches!(*qualifier, "kept" | "retained" | "provided") {
            return false;
        }
        let for_index = i + if *signal == "only" { 2 } else { 1 };
        let dialect = words.get(for_index + 1).copied().unwrap_or("");
        words.get(for_index) == Some(&"for")
            && (matches!(dialect, "backward" | "backwards")
                && words
                    .get(for_index + 2)
                    .is_some_and(|word| word.starts_with("compatib"))
                || dialect.starts_with("backward-compatib")
                || dialect.starts_with("backwards-compatib"))
    }) {
        return true;
    }
    let Some(kind) = headline
        .strip_prefix("old ")
        .or_else(|| headline.strip_prefix("legacy "))
    else {
        return false;
    };
    if !["function", "method", "class", "api", "entry point"]
        .into_iter()
        .any(|prefix| starts_word(kind, prefix))
    {
        return false;
    }
    [
        "backward compatib",
        "backwards compatib",
        "backward-compatib",
        "backwards-compatib",
    ]
    .into_iter()
    .any(|phrase| {
        kind.match_indices(phrase).any(|(index, phrase)| {
            index <= 240
                && kind.get(index + phrase.len()..).is_some_and(|tail| {
                    tail.match_indices("do not use").any(|(offset, _)| {
                        offset <= 240
                            && starts_word(&tail[offset..], "do not use")
                            && tail[..offset]
                                .chars()
                                .next_back()
                                .is_none_or(|ch| !ch.is_alphanumeric() && ch != '_')
                    }) || tail
                        .split(['.', '\n'])
                        .scan(0, |offset, sentence| {
                            let start = *offset;
                            *offset += sentence.len() + 1;
                            Some((
                                start + sentence.len() - sentence.trim_start().len(),
                                sentence.trim(),
                            ))
                        })
                        .any(|(offset, sentence)| {
                            offset <= 240
                                && sentence
                                    .strip_prefix("use ")
                                    .and_then(|text| text.strip_suffix(" instead"))
                                    .is_some_and(|target| (1..=160).contains(&target.trim().len()))
                        })
                })
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::phase02::{Options, Profile, scan};

    #[test]
    #[ignore = "requires the original Skylos deprecation proof fixture"]
    fn pinned_deprecation_facts() {
        let project =
            PathBuf::from(std::env::var_os("SKARMORY_PHASE03_DEPRECATION_FIXTURE").unwrap());
        let report_path = std::env::var_os("SKARMORY_PHASE03_DEPRECATION_REPORT").unwrap();
        let options = Options {
            profile: Profile::SkylosPhase03,
            confidence: Some(0),
            grep_verify: Some(false),
            cache_dir: Some(project.with_file_name("facts")),
            ..Default::default()
        };
        let report = scan(&project, &options).unwrap();
        assert!(report.is_complete(), "{}", report.to_json().unwrap());
        let rows: Vec<_> = report
            .categories
            .unused_functions
            .iter()
            .filter(|row| row.file == "app.py")
            .collect();
        let [candidate] = rows.as_slice() else {
            panic!("expected the original unused app.candidate, got {rows:?}");
        };
        assert_eq!(candidate.full_name.as_deref(), Some("app.candidate"));
        let deprecated = candidate.confidence_factors == Some(["deprecated_api"]);
        assert_eq!(candidate.confidence, Some(if deprecated { 40 } else { 90 }));
        let functions = deprecated.then_some((candidate.line, "candidate"));
        std::fs::write(
            report_path,
            serde_json::to_vec(&serde_json::json!({
                "functions": functions.into_iter().collect::<Vec<_>>(),
                "cache": report.cache.unwrap(),
            }))
            .unwrap(),
        )
        .unwrap();
    }
}
