use std::collections::BTreeMap;
use std::rc::Rc;

use skarmory_python::phase02::{
    Expression, ExpressionKind, Import, Name, Statement, StatementKind,
};

use super::{AnalyzedFile, Issue, preview_source::Project};

const MAX_DEPTH: usize = 16;
const MAX_NODES: usize = 4096;
const MAX_EXPORTS: usize = 128;

type Bindings<'a> = BTreeMap<&'a str, Pure<'a>>;
type Replacements = BTreeMap<[usize; 2], Vec<Statement>>;
pub(super) type Guards = BTreeMap<String, BTreeMap<[usize; 2], bool>>;

#[derive(Default)]
pub(super) struct Resolution {
    pub(super) issues: Vec<Issue>,
    pub(super) guards: Guards,
}

#[derive(Clone)]
enum Pure<'a> {
    Bool(bool),
    Hashable,
    Sequence { list: bool },
    Other,
    Module(Rc<Bindings<'a>>),
}

struct Resolver<'a> {
    project: &'a Project,
    files: &'a [AnalyzedFile],
    modules: BTreeMap<&'a str, Option<usize>>,
}

#[derive(Default)]
struct Expansion {
    replacements: Replacements,
    guards: BTreeMap<[usize; 2], bool>,
    issues: Vec<Issue>,
}

pub(super) fn resolve(project: &Project, files: &mut [AnalyzedFile]) -> Resolution {
    let mut modules = BTreeMap::new();
    for (index, file) in files.iter().enumerate() {
        modules
            .entry(file.module.as_str())
            .and_modify(|slot| *slot = None)
            .or_insert(Some(index));
    }
    let resolver = Resolver {
        project,
        files,
        modules,
    };
    let expansions: Vec<_> = resolver
        .files
        .iter()
        .map(|file| resolver.expand(file))
        .collect();
    let mut resolution = Resolution::default();
    for (
        file,
        Expansion {
            mut replacements,
            guards,
            issues: errors,
        },
    ) in files.iter_mut().zip(expansions)
    {
        if !replacements.is_empty() {
            apply(&mut file.facts.body, &mut replacements);
            if !guards.is_empty() {
                resolution.guards.insert(file.path.clone(), guards);
            }
        }
        resolution.issues.extend(errors);
    }
    resolution
}

impl<'a> Resolver<'a> {
    fn expand(&self, file: &'a AnalyzedFile) -> Expansion {
        let mut expansion = Expansion::default();
        if !contains_import(&file.facts.body) {
            return expansion;
        }
        if let Some(package) = package(file).filter(|package| self.closed_package(file, package))
            && !file.facts.postponed_annotations
        {
            let mut bindings = Bindings::new();
            let mut remaining = MAX_NODES;
            self.expand_suite(
                file,
                &file.facts.body,
                package,
                &mut bindings,
                &mut remaining,
                &mut expansion,
            );
        }
        for statement in &file.facts.body {
            collect_imports(statement, &mut |star| {
                if !expansion.replacements.contains_key(&key(star)) {
                    expansion.issues.push(unresolved(file, star));
                }
            });
        }
        expansion
    }

    fn expand_suite(
        &self,
        file: &'a AnalyzedFile,
        body: &'a [Statement],
        package: &str,
        bindings: &mut Bindings<'a>,
        remaining: &mut usize,
        expansion: &mut Expansion,
    ) -> Option<()> {
        for statement in body {
            if !consume(remaining) {
                return None;
            }
            match &statement.kind {
                StatementKind::ProjectWildcardImport { .. } => {
                    let imports = self.star(file, package, statement, bindings, remaining)?;
                    expansion.replacements.insert(key(statement), imports);
                }
                StatementKind::If {
                    test,
                    body,
                    otherwise,
                } if file.is_package => {
                    let guard = boolean(test, bindings, 0, remaining)?;
                    expansion.guards.insert(key(statement), guard);
                    let (selected, skipped) = if guard {
                        (body, otherwise)
                    } else {
                        (otherwise, body)
                    };
                    for child in skipped {
                        collect_imports(child, &mut |star| {
                            expansion.replacements.insert(key(star), Vec::new());
                        });
                    }
                    self.expand_suite(file, selected, package, bindings, remaining, expansion)?;
                }
                StatementKind::Import(import)
                    if file.is_package
                        && import.level == 0
                        && import.module == "sys"
                        && import.member.is_none()
                        && import.local.text == "sys"
                        && !self.modules.contains_key("sys") =>
                {
                    // The built-in sys import executes no provider initializer.
                    bindings.insert("sys", Pure::Other);
                }
                _ => assignment(statement, bindings, remaining)?,
            }
        }
        Some(())
    }

    fn closed_package(&self, file: &AnalyzedFile, mut package: &str) -> bool {
        loop {
            let Some(initializer) = self.file(package) else {
                return false;
            };
            if !initializer.is_package
                || initializer.path != format!("{}/__init__.py", package.replace('.', "/"))
                || initializer.facts.postponed_annotations
                || (initializer.module != file.module
                    && !initializer.facts.body.iter().all(inert_expression))
            {
                return false;
            }
            let Some((parent, _)) = package.rsplit_once('.') else {
                return true;
            };
            package = parent;
        }
    }

    fn file(&self, module: &str) -> Option<&'a AnalyzedFile> {
        Some(&self.files[(*self.modules.get(module)?)?])
    }

    fn star(
        &self,
        importer: &AnalyzedFile,
        package: &str,
        statement: &Statement,
        bindings: &mut Bindings<'a>,
        remaining: &mut usize,
    ) -> Option<Vec<Statement>> {
        let StatementKind::ProjectWildcardImport {
            module,
            level,
            optional,
        } = &statement.kind
        else {
            return None;
        };
        let (member, absolute) = match level {
            0 => (
                module.strip_prefix(package)?.strip_prefix('.')?,
                module.to_owned(),
            ),
            1 => (module.as_str(), format!("{package}.{module}")),
            _ => return None,
        };
        if !skarmory_python::simple_name(member) || absolute == importer.module {
            return None;
        }
        if *optional {
            return (importer.is_package
                && *level == 1
                && !bindings.contains_key("ImportError")
                && self
                    .project
                    .package_source_unambiguous(package, member, None))
            .then(Vec::new);
        }

        let provider = self.source(package, member, &absolute)?;
        let environment = self.provider(provider, importer, package, remaining, true)?;
        let exports: Vec<_> = environment
            .into_iter()
            .filter(|(name, _)| !name.starts_with('_'))
            .collect();
        if exports.is_empty() || exports.len() > MAX_EXPORTS {
            return None;
        }
        Some(
            exports
                .into_iter()
                .map(|(name, value)| {
                    bindings.insert(name, value);
                    Statement {
                        at: statement.at,
                        kind: StatementKind::Import(Import {
                            local: Name {
                                text: name.to_owned(),
                                at: statement.at,
                            },
                            level: 0,
                            module: absolute.clone(),
                            member: Some(name.to_owned()),
                            declares_export: false,
                            exposes_top_level: false,
                            wildcard_member: true,
                        }),
                    }
                })
                .collect(),
        )
    }

    fn source(&self, package: &str, member: &str, absolute: &str) -> Option<&'a AnalyzedFile> {
        let provider = self.file(absolute)?;
        (!provider.is_package
            && !provider.facts.postponed_annotations
            && self
                .project
                .package_source_unambiguous(package, member, Some(&provider.path)))
        .then_some(provider)
    }

    fn provider(
        &self,
        file: &'a AnalyzedFile,
        importer: &AnalyzedFile,
        package: &str,
        remaining: &mut usize,
        allow_sibling: bool,
    ) -> Option<Bindings<'a>> {
        let mut bindings = Bindings::new();
        for statement in &file.facts.body {
            if !consume(remaining) {
                return None;
            }
            if let StatementKind::Import(import) = &statement.kind {
                if !allow_sibling || import.local.text.starts_with("__") {
                    return None;
                }
                if !importer.is_package
                    || import.level != 0
                    || import.member.is_some()
                    || import.exposes_top_level
                {
                    return None;
                }
                let member = import.module.strip_prefix(package)?.strip_prefix('.')?;
                if !skarmory_python::simple_name(member) {
                    return None;
                }
                let absolute = format!("{package}.{member}");
                if absolute == file.module || absolute == importer.module {
                    return None;
                }
                let sibling = self.source(package, member, &absolute)?;
                let namespace = self.provider(sibling, importer, package, remaining, false)?;
                bindings.insert(&import.local.text, Pure::Module(Rc::new(namespace)));
            } else {
                declarations(statement, &mut bindings, 0, remaining)?;
            }
        }
        Some(bindings)
    }
}

fn declarations<'a>(
    statement: &'a Statement,
    bindings: &mut Bindings<'a>,
    depth: usize,
    remaining: &mut usize,
) -> Option<()> {
    if depth >= MAX_DEPTH || !consume(remaining) {
        return None;
    }
    if let StatementKind::If {
        test,
        body,
        otherwise,
    } = &statement.kind
    {
        let selected = if boolean(test, bindings, 0, remaining)? {
            body
        } else {
            otherwise
        };
        for child in selected {
            declarations(child, bindings, depth + 1, remaining)?;
        }
        Some(())
    } else {
        assignment(statement, bindings, remaining)
    }
}

fn assignment<'a>(
    statement: &'a Statement,
    bindings: &mut Bindings<'a>,
    remaining: &mut usize,
) -> Option<()> {
    if inert_expression(statement) {
        return Some(());
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
    if name.text.starts_with("__") {
        return None;
    }
    let value = pure(value, bindings, 0, remaining)?;
    bindings.insert(&name.text, value);
    Some(())
}

fn pure<'a>(
    expression: &Expression,
    bindings: &Bindings<'a>,
    depth: usize,
    remaining: &mut usize,
) -> Option<Pure<'a>> {
    if depth >= MAX_DEPTH || !consume(remaining) {
        return None;
    }
    match &expression.kind {
        ExpressionKind::Name(name) => bindings.get(name.as_str()).cloned(),
        ExpressionKind::Member(receiver, member) => {
            let ExpressionKind::Name(receiver) = &receiver.kind else {
                return None;
            };
            let Pure::Module(namespace) = bindings.get(receiver.as_str())? else {
                return None;
            };
            namespace.get(member.as_str()).cloned()
        }
        ExpressionKind::Text(_) | ExpressionKind::None | ExpressionKind::Scalar(_) => {
            Some(Pure::Hashable)
        }
        ExpressionKind::Bool(value) => Some(Pure::Bool(*value)),
        ExpressionKind::ConstantLambda
        | ExpressionKind::NullaryConstantLambda
        | ExpressionKind::DeferredBoolLambda => Some(Pure::Other),
        ExpressionKind::Sequence { items, list } => {
            for item in items {
                pure(item, bindings, depth + 1, remaining)?;
            }
            Some(Pure::Sequence { list: *list })
        }
        ExpressionKind::Addition(items) => {
            let [left, right] = items.as_slice() else {
                return None;
            };
            let Pure::Sequence { list } = pure(left, bindings, depth + 1, remaining)? else {
                return None;
            };
            matches!(pure(right, bindings, depth + 1, remaining)?, Pure::Sequence { list: other } if list == other)
                .then_some(Pure::Sequence { list })
        }
        ExpressionKind::Dictionary(items) => {
            let (pairs, []) = items.as_chunks::<2>() else {
                return None;
            };
            for [key, value] in pairs {
                if !matches!(
                    pure(key, bindings, depth + 1, remaining)?,
                    Pure::Hashable | Pure::Bool(_)
                ) {
                    return None;
                }
                pure(value, bindings, depth + 1, remaining)?;
            }
            Some(Pure::Other)
        }
        _ => None,
    }
}

fn boolean(
    expression: &Expression,
    bindings: &Bindings<'_>,
    depth: usize,
    remaining: &mut usize,
) -> Option<bool> {
    if depth >= MAX_DEPTH || !consume(remaining) {
        return None;
    }
    match &expression.kind {
        ExpressionKind::Bool(value) => Some(*value),
        ExpressionKind::Name(name) => match bindings.get(name.as_str())? {
            Pure::Bool(value) => Some(*value),
            _ => None,
        },
        ExpressionKind::Not(value) => Some(!boolean(value, bindings, depth + 1, remaining)?),
        ExpressionKind::Boolean { and, values } => {
            for value in values {
                if boolean(value, bindings, depth + 1, remaining)? != *and {
                    return Some(!and);
                }
            }
            Some(*and)
        }
        _ => None,
    }
}

fn consume(remaining: &mut usize) -> bool {
    if *remaining == 0 {
        return false;
    }
    *remaining -= 1;
    true
}

fn collect_imports(statement: &Statement, visit: &mut impl FnMut(&Statement)) {
    match &statement.kind {
        StatementKind::If {
            body, otherwise, ..
        } => {
            for child in body.iter().chain(otherwise) {
                collect_imports(child, visit);
            }
        }
        StatementKind::With { body, .. } => {
            for child in body {
                collect_imports(child, visit);
            }
        }
        StatementKind::ProjectWildcardImport { .. } => visit(statement),
        _ => {}
    }
}

fn apply(body: &mut Vec<Statement>, replacements: &mut Replacements) {
    for position in (0..body.len()).rev() {
        let statement = &mut body[position];
        match &mut statement.kind {
            StatementKind::If {
                body, otherwise, ..
            } => {
                apply(body, replacements);
                apply(otherwise, replacements);
            }
            StatementKind::With { body, .. } => apply(body, replacements),
            StatementKind::ProjectWildcardImport { .. } => {
                if let Some(imports) = replacements.remove(&key(statement)) {
                    body.splice(position..=position, imports);
                }
            }
            _ => {}
        }
    }
}

fn contains_import(body: &[Statement]) -> bool {
    let mut found = false;
    for statement in body {
        collect_imports(statement, &mut |_| found = true);
        if found {
            return true;
        }
    }
    false
}

fn key(statement: &Statement) -> [usize; 2] {
    [statement.at.line, statement.at.column]
}

fn unresolved(file: &AnalyzedFile, statement: &Statement) -> Issue {
    Issue::new(
        "unsupported_construct",
        &file.path,
        "star imports cannot resolve finite bindings",
    )
    .at(statement.at)
}

fn package(file: &AnalyzedFile) -> Option<&str> {
    if file.is_package {
        Some(&file.module)
    } else {
        file.module.rsplit_once('.').map(|(package, _)| package)
    }
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
