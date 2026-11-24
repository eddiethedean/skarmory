//! Source-preserving edits for the bounded Python dead-code cleanup workflow.

use std::collections::BTreeSet;
use std::ops::Range;

use ruff_python_ast::{Alias, ModModule, Stmt};
use ruff_text_size::Ranged;

use crate::{Lines, parse, parse_source};

pub const SAFE_IMPORT_ROOTS: &[&str] = &[
    "collections",
    "functools",
    "itertools",
    "json",
    "math",
    "operator",
    "os",
    "pathlib",
    "re",
    "sys",
    "typing",
];

pub struct EditPlan {
    /// Byte range to remove from the original source.
    pub removal: Range<usize>,
    pub import_roots: Vec<String>,
    pub anchor: String,
}

pub struct Source<'a> {
    text: &'a str,
    module: ModModule,
    lines: Lines,
}

struct Target {
    index: usize,
    partial: Option<Range<usize>>,
    import_roots: Vec<String>,
}

pub fn validate_replacement(replacement: &str) -> Result<(), String> {
    parse_source(replacement)
        .map(|_| ())
        .map_err(|error| error.message)
}

impl<'a> Source<'a> {
    /// Parse a source once to plan edits for multiple findings.
    pub fn parse(source: &'a str) -> Result<Self, String> {
        if source.starts_with('\u{feff}')
            || source
                .match_indices('\r')
                .any(|(index, _)| source.as_bytes().get(index + 1) != Some(&b'\n'))
        {
            return Err("the source encoding or newline convention requires manual review".into());
        }
        let (module, lines) = parse(source).map_err(|error| error.message)?;
        Ok(Self {
            text: source,
            module,
            lines,
        })
    }

    /// Plan a removal at the exact location of a finding. Check import roots
    /// for project shadowing and validate the materialized replacement before use.
    pub fn plan(
        &self,
        line: usize,
        column: usize,
        name: &str,
        rule: &str,
    ) -> Result<EditPlan, String> {
        let source = self.text;
        let module = &self.module;
        let lines = &self.lines;

        let at_finding = |node: &dyn Ranged| {
            let location = lines.locate(node.start());
            location.line == line && location.column == column
        };
        let Target {
            index,
            partial,
            import_roots,
        } = match rule {
            "SKY-U001" => {
                let mut selected = None;
                for (index, statement) in module.body.iter().enumerate() {
                    let Stmt::FunctionDef(node) = statement else {
                        continue;
                    };
                    if node.name.as_str() != name {
                        continue;
                    }
                    if selected.replace((index, node)).is_some() {
                        return Err("function source anchor is ambiguous".into());
                    }
                }
                let Some((index, node)) = selected.filter(|(_, node)| at_finding(&node.name))
                else {
                    return Err("the finding does not identify a top-level function".into());
                };
                if !node.decorator_list.is_empty()
                    || node
                        .parameters
                        .iter_non_variadic_params()
                        .any(|parameter| parameter.default.is_some())
                {
                    return Err(
                        "function has definition-time behavior and requires manual review".into(),
                    );
                }
                if node.returns.is_some()
                    || node.type_params.is_some()
                    || node
                        .parameters
                        .iter()
                        .any(|parameter| parameter.annotation().is_some())
                {
                    return Err("function annotations require manual review before deletion".into());
                }
                Target {
                    index,
                    partial: None,
                    import_roots: Vec::new(),
                }
            }
            "SKY-U002" => import_edit(source, &module.body, lines, line, column, name)?,
            _ => return Err("no deterministic cleanup edit supports this rule".into()),
        };

        let statement = &module.body[index];
        let anchor = source[range(statement)].to_owned();
        let removal = match partial {
            Some(partial) => partial,
            None => {
                if index == 0
                    && matches!(module.body.get(1), Some(Stmt::Expr(node)) if node.value.is_string_literal_expr())
                {
                    return Err("removing the statement would change the module docstring".into());
                }
                whole_lines(source, range(statement))?
            }
        };
        Ok(EditPlan {
            removal,
            import_roots,
            anchor,
        })
    }
}

fn import_edit(
    source: &str,
    statements: &[Stmt],
    lines: &Lines,
    line: usize,
    column: usize,
    name: &str,
) -> Result<Target, String> {
    let selected = statements
        .iter()
        .enumerate()
        .find_map(|(index, statement)| {
            let Stmt::Import(node) = statement else {
                return None;
            };
            node.names
                .iter()
                .enumerate()
                .find_map(|(alias_index, alias)| {
                    let binding = alias.asname.as_ref().unwrap_or(&alias.name);
                    let location = lines.locate(binding.start());
                    // Compatibility reports can use the imported name in place of
                    // the local alias. The binding position disambiguates both forms.
                    let imported = alias.name.as_str();
                    (location.line == line
                        && location.column == column
                        && (binding.as_str() == name
                            || imported == name
                            || imported.rsplit('.').next() == Some(name)))
                    .then_some((index, node, alias_index))
                })
        });
    let Some((index, node, alias_index)) = selected else {
        return Err("import side effects require manual review outside a top-level import".into());
    };
    let roots = node.names.iter().map(import_root).collect::<BTreeSet<_>>();
    if node.is_lazy
        || node
            .names
            .iter()
            .any(|alias| alias.name.as_str().contains('.'))
        || roots.iter().any(|root| !SAFE_IMPORT_ROOTS.contains(root))
    {
        return Err("import side effects require manual review for this module".into());
    }
    let target = &node.names[alias_index];
    if statements
        .iter()
        .filter_map(|statement| match statement {
            Stmt::Import(import) => Some(&import.names),
            _ => None,
        })
        .flatten()
        .filter(|alias| import_binding(alias) == import_binding(target))
        .take(2)
        .count()
        != 1
    {
        return Err("import source anchor is ambiguous".into());
    }

    let partial = if node.names.len() == 1 {
        None
    } else if let Some(next) = node.names.get(alias_index + 1) {
        let separator = usize::from(target.end())..usize::from(next.start());
        confirm_separator(&source[separator.clone()])?;
        Some(usize::from(target.start())..separator.end)
    } else {
        let previous = &node.names[alias_index - 1];
        let separator = usize::from(previous.end())..usize::from(target.start());
        confirm_separator(&source[separator.clone()])?;
        Some(separator.start..usize::from(target.end()))
    };
    Ok(Target {
        index,
        partial,
        import_roots: roots.into_iter().map(str::to_owned).collect(),
    })
}

fn import_root(alias: &Alias) -> &str {
    alias.name.as_str().split('.').next().expect("import root")
}

fn import_binding(alias: &Alias) -> &str {
    alias
        .asname
        .as_ref()
        .map_or_else(|| import_root(alias), |name| name.as_str())
}

fn confirm_separator(separator: &str) -> Result<(), String> {
    if separator.trim_matches([' ', '\t']) != "," {
        return Err("the import list separator requires manual review".into());
    }
    Ok(())
}

fn range(node: &impl Ranged) -> Range<usize> {
    usize::from(node.start())..usize::from(node.end())
}

fn whole_lines(source: &str, statement: Range<usize>) -> Result<Range<usize>, String> {
    let start = source[..statement.start]
        .rfind('\n')
        .map_or(0, |index| index + 1);
    let rest = &source[statement.end..];
    let newline = rest.find('\n').unwrap_or(rest.len());
    let tail = rest[..newline].trim();
    if !source[start..statement.start].trim().is_empty()
        || (!tail.is_empty() && !tail.starts_with('#'))
    {
        return Err("the statement shares a source line and requires manual review".into());
    }
    let end = statement.end + newline + usize::from(newline < rest.len());
    Ok(start..end)
}

#[cfg(test)]
mod tests {
    use std::ops::Range;

    use super::{Source, validate_replacement};

    fn reconstruct(source: &str, removal: Range<usize>) -> String {
        let mut replacement = source.to_owned();
        replacement.replace_range(removal, "");
        validate_replacement(&replacement).unwrap();
        replacement
    }

    #[test]
    fn removes_only_the_selected_function_and_keeps_crlf() {
        let source = "# Header\r\nasync def unused():\r\n    return 'hi'\r\n\r\nanswer = 42\r\n";
        let edit = Source::parse(source)
            .unwrap()
            .plan(2, 11, "unused", "SKY-U001")
            .unwrap();
        assert_eq!(
            &source[edit.removal.clone()],
            "async def unused():\r\n    return 'hi'\r\n"
        );
        assert_eq!(
            reconstruct(source, edit.removal),
            "# Header\r\n\r\nanswer = 42\r\n"
        );
        assert!(edit.import_roots.is_empty());
        let moved = Source::parse(&format!("\r\n{source}"))
            .unwrap()
            .plan(3, 11, "unused", "SKY-U001")
            .unwrap();
        assert_eq!(edit.anchor, moved.anchor);
    }

    #[test]
    fn keeps_other_aliases_and_comments() {
        let source = "import math, json as codec, os # comment\nanswer = 42\n";
        let parsed = Source::parse(source).unwrap();
        let cases = [
            (8, "math", "import json as codec, os # comment\n"),
            (22, "json", "import math, os # comment\n"),
            (29, "os", "import math, json as codec # comment\n"),
        ];
        for (column, name, expected) in cases {
            let edit = parsed.plan(1, column, name, "SKY-U002").unwrap();
            assert_eq!(
                reconstruct(source, edit.removal),
                format!("{expected}answer = 42\n")
            );
            assert_eq!(edit.import_roots, ["json", "math", "os"]);
        }
        assert!(validate_replacement("import math,\n").is_err());
    }

    #[test]
    fn holds_effectful_or_ambiguous_statements_for_review() {
        for (source, line, column) in [
            ("@register\ndef unused():\n    pass\n", 2, 5),
            ("def unused(value=register()):\n    pass\n", 1, 5),
            ("def unused(*values: register()):\n    pass\n", 1, 5),
            ("def unused():\n    pass\n'docstring'\n", 1, 5),
            ("def unused(): pass\ndef unused(): pass\n", 1, 5),
        ] {
            assert!(
                Source::parse(source)
                    .unwrap()
                    .plan(line, column, "unused", "SKY-U001")
                    .is_err(),
                "{source}"
            );
        }
        for source in [
            "import math, plugin\n",
            "import math\nimport math\n",
            "import math; print('keep')\n",
        ] {
            assert!(
                Source::parse(source)
                    .unwrap()
                    .plan(1, 8, "math", "SKY-U002")
                    .is_err(),
                "{source}"
            );
        }
    }
}
