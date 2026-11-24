//! File reachability for a single package with a declared local entry point.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path, PathBuf};

use oxc_allocator::Allocator;
use oxc_ast::ast::{
    Argument, CallExpression, Expression, IdentifierReference, ImportMeta, MemberExpression,
    TSExternalModuleReference, TSImportType,
};
use oxc_ast_visit::Visit;
use oxc_ast_visit::walk::{walk_call_expression, walk_member_expression};
use oxc_parser::Parser;
use oxc_semantic::{Scoping, SemanticBuilder};
use oxc_span::{SourceType, Span};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use skarmory_python::{FrontendError, Location};

use super::{Analysis, Category, Evidence, Finding};
use crate::Issue;
use crate::fact_cache::FactCache;
use crate::preview_source::{self, Project, Stamp};

pub(super) const RULE: &str = "SKY-E003";
pub(super) const UNIMPORTED: &str = "unimported_file";
pub(super) const DEAD_IMPORTERS: &str = "imported_only_by_unused_files";
const EXTENSIONS: &[&str] = &["js", "jsx", "mjs", "ts", "tsx", "mts"];
const MANIFEST: &str = "package.json";
// Oxc can recurse in both parsing and AST traversal. Bound the amount of
// input that can create frames and reserve a stack independent of the caller.
const MAX_PARSE_BYTES: usize = 16 * 1024;
const PARSER_STACK_BYTES: usize = 64 * 1024 * 1024;
const UNSUPPORTED_GLOBAL_LOADERS: &[&str] = &[
    "require",
    "module",
    "Worker",
    "SharedWorker",
    "importScripts",
    "eval",
];

#[derive(Deserialize, Serialize)]
pub(super) struct Module {
    requests: Vec<Request>,
    pub(super) ignore_file: bool,
}

#[derive(Deserialize, Serialize)]
struct Request {
    specifier: String,
    line: usize,
    column: usize,
}

pub(super) fn extract(
    file: &str,
    source: &str,
    extension: &str,
    cache: Option<&FactCache>,
) -> Result<Module, FrontendError> {
    let source_type = SourceType::from_path(file).map_err(|error| FrontendError {
        kind: "unsupported_js_source",
        message: error.to_string(),
        location: Location { line: 1, column: 1 },
    })?;
    if !EXTENSIONS.contains(&extension) || source_type.is_typescript_definition() {
        return Err(FrontendError {
            kind: "unsupported_js_source",
            message: "CommonJS entry formats and TypeScript declaration files are outside the supported file graph".into(),
            location: Location { line: 1, column: 1 },
        });
    }
    if source.len() > MAX_PARSE_BYTES {
        return Err(FrontendError {
            kind: "analysis_limit",
            message: "JavaScript or TypeScript source exceeds the 16 KiB parser input budget"
                .into(),
            location: Location { line: 1, column: 1 },
        });
    }
    let parse = |source: &str| {
        stacker::maybe_grow(PARSER_STACK_BYTES, PARSER_STACK_BYTES, || {
            parse(source, source_type)
        })
    };
    match cache {
        Some(cache) => cache.get_or_extract_variant(
            &format!("phase03-js-v4/oxc-0.159.0/{extension}"),
            source,
            parse,
        ),
        None => parse(source),
    }
}

fn parse(source: &str, source_type: SourceType) -> Result<Module, FrontendError> {
    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, source, source_type).parse();
    let mut lines = vec![0];
    lines.extend(source.match_indices('\n').map(|(offset, _)| offset + 1));
    let locate = |offset: usize| {
        let line = lines.partition_point(|start| *start <= offset);
        Location {
            line,
            column: offset - lines[line - 1] + 1,
        }
    };
    if let Some(error) = parsed.diagnostics.errors().next() {
        return Err(FrontendError {
            kind: "parse_error",
            message: error.to_string(),
            location: locate(
                error
                    .labels
                    .first()
                    .map_or(0, |label| label.offset() as usize),
            ),
        });
    }
    let unsupported = |span: Span| FrontendError {
        kind: "unsupported_js_module_loading",
        message: "only static ECMAScript import and re-export declarations are supported".into(),
        location: locate(span.start as usize),
    };
    if let Some(item) = parsed.module_record.dynamic_imports.first() {
        return Err(unsupported(item.span));
    }
    let semantic = SemanticBuilder::new().build(&parsed.program).semantic;
    let mut loaders = OtherLoaders {
        scoping: semantic.scoping(),
        first: None,
    };
    loaders.visit_program(&parsed.program);
    if let Some(span) = loaders.first {
        return Err(unsupported(span));
    }
    let mut ignore_file = false;
    for comment in &parsed.program.comments {
        let span = comment.content_span();
        let text = source[span.start as usize..span.end as usize].trim();
        if text
            .strip_prefix('/')
            .is_some_and(|text| text.trim_start().starts_with("<reference"))
            || text.contains("@import")
            || text.contains("import(")
        {
            return Err(unsupported(span));
        }
        if locate(comment.span.start as usize).line == 1 {
            let marker = text.to_ascii_lowercase();
            ignore_file |= marker.strip_prefix("skylos:").is_some_and(|directive| {
                let directive = directive.trim();
                directive == "ignore"
                    || directive.strip_prefix("ignore").is_some_and(|rules| {
                        let rules = rules.trim_start();
                        rules.strip_prefix(['[', ':']).is_some_and(|rules| {
                            rules
                                .split(|ch: char| ch.is_whitespace() || matches!(ch, ',' | ']'))
                                .any(|rule| rule.eq_ignore_ascii_case(RULE))
                        })
                    })
            });
        }
    }
    let mut requests: Vec<_> = parsed
        .module_record
        .requested_modules
        .iter()
        .filter_map(|(specifier, entries)| {
            let offset = entries.iter().map(|entry| entry.span.start).min()?;
            let location = locate(offset as usize);
            Some(Request {
                specifier: specifier.to_string(),
                line: location.line,
                column: location.column,
            })
        })
        .collect();
    requests.sort_unstable_by_key(|request| (request.line, request.column));
    Ok(Module {
        requests,
        ignore_file,
    })
}

struct OtherLoaders<'s> {
    scoping: &'s Scoping,
    first: Option<Span>,
}

impl OtherLoaders<'_> {
    fn is_ambient(&self, item: &IdentifierReference<'_>) -> bool {
        item.reference_id
            .get()
            .and_then(|id| self.scoping.get_reference(id).symbol_id())
            .is_none_or(|id| self.scoping.symbol_flags(id).is_ambient())
    }

    fn is_global_object(&self, object: &Expression<'_>) -> bool {
        matches!(
            object.get_inner_expression(),
            Expression::Identifier(item)
                if matches!(item.name.as_str(), "globalThis" | "self" | "window" | "global")
                    && self.is_ambient(item)
        )
    }

    fn is_node_process(&self, object: &Expression<'_>) -> bool {
        match object.get_inner_expression() {
            Expression::Identifier(item) => item.name == "process" && self.is_ambient(item),
            member => member.as_member_expression().is_some_and(|member| {
                member.static_property_name().and_then(|name| name.as_str()) == Some("process")
                    && self.is_global_object(member.object())
            }),
        }
    }

    fn is_node_main_module(&self, object: &Expression<'_>) -> bool {
        object
            .get_inner_expression()
            .as_member_expression()
            .is_some_and(|member| {
                member.static_property_name().and_then(|name| name.as_str()) == Some("mainModule")
                    && self.is_node_process(member.object())
            })
    }
}

impl<'a> Visit<'a> for OtherLoaders<'_> {
    fn visit_identifier_reference(&mut self, item: &IdentifierReference<'a>) {
        if UNSUPPORTED_GLOBAL_LOADERS.contains(&item.name.as_str()) && self.is_ambient(item) {
            self.first.get_or_insert(item.span);
        }
    }

    fn visit_member_expression(&mut self, item: &MemberExpression<'a>) {
        if let Some((span, property)) = item.static_property_info()
            && let Some(property) = property.as_str()
        {
            let object = item.object();
            // The entry guard reads a boolean and cannot introduce a module edge.
            if property == "main"
                && matches!(object.without_parentheses(), Expression::ImportMeta(_))
            {
                return;
            }
            if (UNSUPPORTED_GLOBAL_LOADERS.contains(&property) && self.is_global_object(object))
                || (property == "require" && self.is_node_main_module(object))
            {
                self.first.get_or_insert(span);
            }
        }
        walk_member_expression(self, item);
    }

    fn visit_call_expression(&mut self, item: &CallExpression<'a>) {
        if let Some(callee) = item.callee.get_inner_expression().as_member_expression()
            && let Some((span, property)) = callee.static_property_info()
            && property.as_str() == Some("getBuiltinModule")
            && self.is_node_process(callee.object())
            && !matches!(
                item.arguments.first(),
                Some(Argument::StringLiteral(module))
                    if module.value.as_str().is_some_and(|name| !matches!(name, "module" | "node:module"))
            )
        {
            self.first.get_or_insert(span);
        }
        walk_call_expression(self, item);
    }

    fn visit_import_meta(&mut self, item: &ImportMeta) {
        self.first.get_or_insert(item.span);
    }

    fn visit_ts_import_type(&mut self, item: &TSImportType<'a>) {
        self.first.get_or_insert(item.span);
    }

    fn visit_ts_external_module_reference(&mut self, item: &TSExternalModuleReference<'a>) {
        self.first.get_or_insert(item.span);
    }
}

pub(super) fn analyze(
    project: &Project,
    modules: &BTreeMap<String, Module>,
    analysis: &mut Analysis,
) {
    if modules.is_empty() {
        return;
    }
    let manifest = project.base().join(MANIFEST);
    for path in project.javascript_context() {
        if path != manifest {
            analysis.issues.push(Issue::new(
                "unsupported_js_project_context",
                &project.label(&path),
                "file reachability supports a single package without additional entry-point or resolver configuration",
            ));
        }
    }
    let (main, stamp) = match read_main(&manifest) {
        Ok(value) => value,
        Err(issue) => {
            analysis.issues.push(issue);
            return;
        }
    };
    let mut paths: BTreeMap<_, _> = project
        .javascript_sources()
        .filter_map(|path| Some((path.strip_prefix(project.base()).ok()?.to_owned(), None)))
        .collect();
    for path in modules.keys() {
        paths.insert(PathBuf::from(path), Some(path.as_str()));
    }
    let excluded: BTreeSet<_> = project
        .exclusions
        .iter()
        .map(|item| Path::new(&item.path))
        .collect();
    let entry = resolve(&paths, &excluded, Path::new(""), &main, false);
    if entry.is_none() {
        analysis.issues.push(Issue::new(
            "unresolved_js_entrypoint",
            MANIFEST,
            format!("main {main:?} does not resolve to a unique selected source file"),
        ));
    }
    let mut edges = BTreeMap::new();
    let mut imported = BTreeSet::new();
    for (file, module) in modules {
        let importer = Path::new(file);
        let parent = importer.parent().expect("selected file has a parent");
        let typescript = matches!(
            preview_source::javascript_extension(importer.as_os_str()),
            Some("ts" | "tsx" | "mts")
        );
        let mut outgoing = Vec::new();
        for request in &module.requests {
            let specifier = &request.specifier;
            let target = if specifier.starts_with("./") || specifier.starts_with("../") {
                resolve(&paths, &excluded, parent, specifier, typescript)
            } else {
                None
            };
            if let Some(target) = target {
                outgoing.push(target);
                imported.insert(target);
            } else {
                analysis.issues.push(
                    Issue::new(
                        "unresolved_js_import",
                        file,
                        format!("{specifier:?} does not resolve to a unique selected source file"),
                    )
                    .at(Location {
                        line: request.line,
                        column: request.column,
                    }),
                );
            }
        }
        edges.insert(file.as_str(), outgoing);
    }
    if !fs::symlink_metadata(manifest)
        .is_ok_and(|metadata| metadata.is_file() && Stamp::new(&metadata) == stamp)
    {
        analysis.issues.push(Issue::new(
            "source_changed",
            MANIFEST,
            "package manifest changed after its snapshot was read",
        ));
    }
    if !analysis.issues.is_empty() {
        return;
    }

    let mut queue: Vec<_> = entry.into_iter().collect();
    let mut reached = BTreeSet::new();
    while let Some(file) = queue.pop() {
        if reached.insert(file) {
            queue.extend(&edges[file]);
        }
    }
    for file in modules.keys() {
        if reached.contains(file.as_str()) {
            analysis.evidence.push(Evidence {
                file: file.clone(),
                line: 1,
                full_name: file.clone(),
                kind: if entry == Some(file) {
                    "js_package_entrypoint"
                } else {
                    "js_reachable_module"
                },
                certainty: "definite",
            });
        } else {
            let has_importers = imported.contains(file.as_str());
            analysis.findings.push(Finding {
                category: Category::File,
                identity_name: String::new(),
                name: file.clone(),
                full_name: file.clone(),
                file: file.clone(),
                line: 1,
                column: 1,
                confidence: if has_importers { 65 } else { 80 },
                rule_id: RULE,
                reason: if has_importers {
                    DEAD_IMPORTERS
                } else {
                    UNIMPORTED
                },
            });
        }
    }
}

fn read_main(path: &Path) -> Result<(String, Stamp), Issue> {
    if fs::symlink_metadata(path).is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound) {
        return Err(Issue::new(
            "missing_js_entrypoint",
            MANIFEST,
            "file reachability requires a package.json main entry point",
        ));
    }
    let (bytes, stamp) = preview_source::read_snapshot(path, MANIFEST, None)?;
    let value: Value = serde_json::from_slice(&bytes)
        .map_err(|error| Issue::new("invalid_js_package_manifest", MANIFEST, error))?;
    for key in [
        "exports",
        "imports",
        "bin",
        "module",
        "browser",
        "types",
        "typings",
        "typesVersions",
        "scripts",
        "workspaces",
    ] {
        if value.get(key).is_some() {
            return Err(Issue::new(
                "unsupported_js_project_context",
                MANIFEST,
                format!("package.json field {key:?} is outside the supported file graph"),
            ));
        }
    }
    let main = value
        .get("main")
        .and_then(Value::as_str)
        .filter(|main| !main.trim().is_empty())
        .ok_or_else(|| {
            Issue::new(
                "missing_js_entrypoint",
                MANIFEST,
                "file reachability requires a package.json main entry point",
            )
        })?;
    Ok((main.to_owned(), stamp))
}

fn resolve<'a>(
    paths: &BTreeMap<PathBuf, Option<&'a str>>,
    excluded: &BTreeSet<&Path>,
    base: &Path,
    request: &str,
    typescript: bool,
) -> Option<&'a str> {
    if request.contains(['\\', ':', '?', '#']) {
        return None;
    }
    let mut path = base.to_owned();
    for part in Path::new(request).components() {
        match part {
            Component::Normal(part) => path.push(part),
            Component::CurDir => {}
            Component::ParentDir if path.pop() => {}
            _ => return None,
        }
    }
    if typescript {
        let source_extensions: &[&str] =
            match preview_source::javascript_extension(path.as_os_str()) {
                Some("js") => &["ts", "tsx", "d.ts"],
                Some("jsx") => &["tsx", "ts", "d.ts"],
                Some("mjs") => &["mts", "d.mts"],
                _ => &[],
            };
        for extension in source_extensions {
            if let Some(file) = paths.get(&path.with_extension(extension)) {
                // A known but excluded source cannot be replaced by its emitted file.
                return *file;
            }
        }
    }
    if let Some(file) = paths.get(path.as_path()) {
        return *file;
    }
    if path.extension().is_some() || excluded.contains(path.as_path()) {
        return None;
    }
    let index = path.join("index");
    let mut candidates = EXTENSIONS.iter().flat_map(|extension| {
        [
            paths.get(path.with_extension(extension).as_path()),
            paths.get(index.with_extension(extension).as_path()),
        ]
        .into_iter()
        .flatten()
    });
    let target = candidates.next()?;
    if candidates.next().is_none() {
        *target
    } else {
        None
    }
}
