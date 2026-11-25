use std::collections::{BTreeMap, BTreeSet};

use skarmory_python::phase02::{
    Expression, ExpressionKind, Import, Name, Statement, StatementKind,
};

use super::{AnalyzedFile, Issue, preview_source::Project};

pub(super) fn resolve(project: &Project, files: &mut [AnalyzedFile]) -> Vec<Issue> {
    let mut modules = BTreeMap::new();
    for (index, file) in files.iter().enumerate() {
        modules
            .entry(file.module.as_str())
            .and_modify(|slot| *slot = None)
            .or_insert(Some(index));
    }
    let mut replacements = Vec::new();
    let mut issues = Vec::new();
    for (file_index, file) in files.iter().enumerate() {
        for (position, statement) in file.facts.body.iter().enumerate() {
            let StatementKind::ProjectWildcardImport(module) = &statement.kind else {
                continue;
            };
            let Some(exports) = exports(project, files, &modules, file, position, module) else {
                issues.push(
                    Issue::new(
                        "unsupported_construct",
                        &file.path,
                        "star imports cannot resolve finite bindings",
                    )
                    .at(statement.at),
                );
                continue;
            };
            let imports: Vec<_> = exports
                .into_iter()
                .map(|name| Statement {
                    at: statement.at,
                    kind: StatementKind::Import(Import {
                        local: Name {
                            text: name.to_owned(),
                            at: statement.at,
                        },
                        level: 0,
                        module: module.clone(),
                        member: Some(name.to_owned()),
                        declares_export: false,
                        exposes_top_level: false,
                        wildcard_member: true,
                    }),
                })
                .collect();
            replacements.push((file_index, position, imports));
        }
    }
    for (file, position, imports) in replacements.into_iter().rev() {
        files[file].facts.body.splice(position..=position, imports);
    }
    issues
}

fn exports<'a>(
    project: &Project,
    files: &'a [AnalyzedFile],
    modules: &BTreeMap<&str, Option<usize>>,
    importer: &AnalyzedFile,
    position: usize,
    module: &str,
) -> Option<BTreeSet<&'a str>> {
    let (package, member) = module.split_once('.')?;
    if member.contains('.')
        || importer.is_package
        || importer.facts.postponed_annotations
        || importer.module.rsplit_once('.')?.0 != package
        || module == importer.module
        || !importer.facts.body[..position].iter().all(inert_expression)
    {
        return None;
    }
    let package_file = &files[(*modules.get(package)?)?];
    let provider = &files[(*modules.get(module)?)?];
    if !package_file.is_package
        || provider.is_package
        || package_file.facts.postponed_annotations
        || provider.facts.postponed_annotations
        || !package_file.facts.body.iter().all(inert_expression)
        || !project.root_package_source_unambiguous(
            &package_file.path,
            &provider.path,
            package,
            member,
        )
    {
        return None;
    }
    let mut exports = BTreeSet::new();
    let mut remaining_nodes = 4096;
    for statement in &provider.facts.body {
        if inert_expression(statement) {
            continue;
        }
        let StatementKind::Assign {
            names,
            value: Some(value),
            annotation: None,
        } = &statement.kind
        else {
            return None;
        };
        let [name] = names.as_slice() else {
            return None;
        };
        if name.text.starts_with("__") || !literal(value, 0, &mut remaining_nodes) {
            return None;
        }
        if !name.text.starts_with('_') {
            exports.insert(name.text.as_str());
        }
    }
    (!exports.is_empty() && exports.len() <= 128).then_some(exports)
}

fn inert_expression(statement: &Statement) -> bool {
    matches!(
        &statement.kind,
        StatementKind::Evaluate(Expression {
            kind: ExpressionKind::Text(_),
            ..
        })
    )
}

fn literal(expression: &Expression, depth: usize, remaining: &mut usize) -> bool {
    if depth >= 16 || *remaining == 0 {
        return false;
    }
    *remaining -= 1;
    match &expression.kind {
        ExpressionKind::Text(_)
        | ExpressionKind::Bool(_)
        | ExpressionKind::None
        | ExpressionKind::Scalar(_) => true,
        ExpressionKind::Sequence { items, .. } => {
            items.iter().all(|item| literal(item, depth + 1, remaining))
        }
        _ => false,
    }
}
