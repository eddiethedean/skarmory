//! Syntax-derived reachability for C++ file-local free functions.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde::{Deserialize, Serialize};
use skarmory_python::Location;
use tree_sitter::{Node, Parser};

use super::{Analysis, Category, Evidence, Finding};
use crate::Issue;
use crate::preview_source::Project;

const MAX_PARSE_BYTES: usize = 256 * 1024;
const MAX_SYNTAX_NODES: usize = 200_000;

#[derive(Deserialize, Serialize)]
pub(super) struct Facts {
    functions: Vec<Function>,
    opaque: Option<OpaqueInput>,
}

#[derive(Deserialize, Serialize)]
struct Function {
    name: String,
    line: usize,
    column: usize,
    retention: Option<Retention>,
}

#[derive(Clone, Copy, Deserialize, Serialize)]
enum Retention {
    External,
    Ambiguous,
    Reference,
}

enum Linkage {
    Local,
    External,
    Ambiguous,
}

#[derive(Deserialize, Serialize)]
struct OpaqueInput {
    line: usize,
    column: usize,
    kind: OpaqueKind,
}

#[derive(Deserialize, Serialize)]
enum OpaqueKind {
    Include,
    Assembly,
}

pub(super) fn extract(path: &str, source: &str) -> Result<Facts, Issue> {
    if source.len() > MAX_PARSE_BYTES {
        return Err(Issue::new(
            "analysis_limit",
            path,
            "C++ source exceeds the 256 KiB parser input budget",
        ));
    }
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_cpp::LANGUAGE.into())
        .map_err(|error| Issue::new("cpp_parser_unavailable", path, error))?;
    let tree = parser.parse(source, None).ok_or_else(|| {
        Issue::new(
            "cpp_parse_error",
            path,
            "C++ parser returned no syntax tree",
        )
    })?;
    let root = tree.root_node();
    if root.has_error() {
        let fault = nodes(root)
            .take(MAX_SYNTAX_NODES)
            .find(|node| node.is_error() || node.is_missing())
            .unwrap_or(root);
        return Err(
            Issue::new("cpp_parse_error", path, "C++ source does not parse").at(location(fault)),
        );
    }

    let syntax: Vec<_> = nodes(root).take(MAX_SYNTAX_NODES + 1).collect();
    if syntax.len() > MAX_SYNTAX_NODES {
        return Err(Issue::new(
            "analysis_limit",
            path,
            "C++ syntax tree exceeds the supported node budget",
        ));
    }

    let mut declarations = BTreeMap::<_, usize>::new();
    let mut directive_names = BTreeSet::new();
    let mut token_pasting = false;
    let mut opaque = None;
    for &node in &syntax {
        if node.kind() == "function_declarator"
            && let Some(name) = node.child_by_field_name("declarator")
            && name.kind() == "identifier"
        {
            *declarations.entry(text(name, source)).or_default() += 1;
        }
        let opaque_kind = match node.kind() {
            "preproc_include" => Some(OpaqueKind::Include),
            "gnu_asm_expression" | "asm_statement" => Some(OpaqueKind::Assembly),
            _ => None,
        };
        if opaque.is_none()
            && let Some(kind) = opaque_kind
        {
            let position = location(node);
            opaque = Some(OpaqueInput {
                line: position.line,
                column: position.column,
                kind,
            });
        }
        if node.kind().starts_with("preproc_")
            && !node
                .parent()
                .is_some_and(|parent| parent.kind().starts_with("preproc_"))
        {
            let directive = text(node, source);
            directive_names
                .extend(directive.split(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_'));
            token_pasting |= directive.contains("##");
        }
    }

    let mut functions = Vec::new();
    let mut definition_names = BTreeSet::new();
    for &node in &syntax {
        if node.kind() != "function_definition" {
            continue;
        }
        let Some(function) = unwrap_declarator(node.child_by_field_name("declarator")) else {
            continue;
        };
        let Some(identifier) = function.child_by_field_name("declarator") else {
            continue;
        };
        if function.kind() != "function_declarator" || identifier.kind() != "identifier" {
            continue;
        }
        let Some(linkage) = linkage(node, function, source) else {
            continue;
        };
        let name = text(identifier, source);
        let retention = match linkage {
            Linkage::External => Some(Retention::External),
            Linkage::Local
                if name.is_ascii()
                    && !token_pasting
                    && declarations.get(name) == Some(&1)
                    && !directive_names.contains(name) =>
            {
                None
            }
            Linkage::Local | Linkage::Ambiguous => Some(Retention::Ambiguous),
        };
        let position = location(identifier);
        definition_names.insert(identifier.start_byte());
        functions.push(Function {
            name: name.into(),
            line: position.line,
            column: position.column,
            retention,
        });
    }

    let candidate_names: BTreeSet<_> = functions
        .iter()
        .filter(|function| function.retention.is_none())
        .map(|function| function.name.as_str())
        .collect();
    let mut binding_ranges = BTreeMap::<&str, Vec<(usize, usize)>>::new();
    let mut binding_names = BTreeSet::new();
    for &node in &syntax {
        let scope = match node.kind() {
            "declaration" => node
                .parent()
                .filter(|parent| parent.kind() == "compound_statement"),
            "parameter_declaration" | "optional_parameter_declaration" => parameter_body(node),
            _ => None,
        };
        let Some(scope) = scope else { continue };
        let mut cursor = node.walk();
        for declaration in node.children_by_field_name("declarator", &mut cursor) {
            let declaration = if declaration.kind() == "init_declarator" {
                declaration.child_by_field_name("declarator")
            } else {
                Some(declaration)
            };
            let Some(declaration) = declaration else {
                continue;
            };
            let Some(identifier) =
                unwrap_declarator(Some(declaration)).filter(|name| name.kind() == "identifier")
            else {
                continue;
            };
            let name = text(identifier, source);
            if candidate_names.contains(name) {
                binding_names.insert(identifier.start_byte());
                binding_ranges.entry(name).or_default().push((
                    declaration.end_byte().max(scope.start_byte()),
                    scope.end_byte(),
                ));
            }
        }
    }
    for ranges in binding_ranges.values_mut() {
        ranges.sort_unstable();
        let mut merged: Vec<(usize, usize)> = Vec::new();
        for (start, end) in ranges.drain(..) {
            if let Some(last) = merged.last_mut().filter(|last| last.1 >= start) {
                last.1 = last.1.max(end);
            } else {
                merged.push((start, end));
            }
        }
        *ranges = merged;
    }

    let mut references = BTreeSet::new();
    for node in syntax {
        if node.kind() != "identifier"
            || definition_names.contains(&node.start_byte())
            || binding_names.contains(&node.start_byte())
        {
            continue;
        }
        let name = text(node, source);
        if !candidate_names.contains(name) {
            continue;
        }
        let qualified = node
            .parent()
            .is_some_and(|parent| parent.kind() == "qualified_identifier");
        if !qualified
            && binding_ranges.get(name).is_some_and(|ranges| {
                let index = ranges.partition_point(|range| range.0 <= node.start_byte());
                index > 0 && node.start_byte() < ranges[index - 1].1
            })
        {
            continue;
        }
        references.insert(name);
    }
    for function in &mut functions {
        if function.retention.is_none() && references.contains(function.name.as_str()) {
            function.retention = Some(Retention::Reference);
        }
    }
    Ok(Facts { functions, opaque })
}

fn location(node: Node<'_>) -> Location {
    let point = node.start_position();
    Location {
        line: point.row + 1,
        column: point.column + 1,
    }
}

fn text<'a>(node: Node<'_>, source: &'a str) -> &'a str {
    source.get(node.byte_range()).unwrap_or_default()
}

fn nodes(root: Node<'_>) -> impl Iterator<Item = Node<'_>> {
    let mut cursor = root.walk();
    let mut next = Some(root);
    std::iter::from_fn(move || {
        let current = next.take()?;
        if !cursor.goto_first_child() {
            while !cursor.goto_next_sibling() {
                if !cursor.goto_parent() {
                    return Some(current);
                }
            }
        }
        next = Some(cursor.node());
        Some(current)
    })
}

fn unwrap_declarator(mut node: Option<Node<'_>>) -> Option<Node<'_>> {
    while let Some(value) = node {
        node = match value.kind() {
            "pointer_declarator" | "reference_declarator" => value
                .child_by_field_name("declarator")
                .or_else(|| value.named_child(0)),
            _ => return Some(value),
        };
    }
    None
}

fn linkage(function: Node<'_>, declarator: Node<'_>, source: &str) -> Option<Linkage> {
    let mut local_namespace = false;
    let mut ambiguous = false;
    let mut parent = function.parent();
    while let Some(node) = parent {
        match node.kind() {
            "class_specifier" | "struct_specifier" | "union_specifier" | "lambda_expression" => {
                return None;
            }
            "namespace_definition" if node.child_by_field_name("name").is_none() => {
                local_namespace = true;
            }
            kind if kind == "template_declaration" || kind.starts_with("preproc_") => {
                ambiguous = true;
            }
            _ => {}
        }
        parent = node.parent();
    }
    let mut cursor = function.walk();
    let mut static_linkage = false;
    for child in function.named_children(&mut cursor) {
        if child.kind() == "storage_class_specifier" {
            match text(child, source) {
                "static" => static_linkage = true,
                "extern" => return Some(Linkage::External),
                _ => {}
            }
        }
        ambiguous |= is_attribute(child);
    }
    // An attribute can register an internal function as a compiler entry point.
    // Retain such definitions until the build configuration is available.
    Some(if !static_linkage && !local_namespace {
        Linkage::External
    } else if ambiguous || nodes(declarator).any(is_attribute) {
        Linkage::Ambiguous
    } else {
        Linkage::Local
    })
}

fn is_attribute(node: Node<'_>) -> bool {
    matches!(
        node.kind(),
        "attribute_declaration" | "attribute_specifier" | "ms_declspec_modifier"
    )
}

fn parameter_body(parameter: Node<'_>) -> Option<Node<'_>> {
    let parameters = parameter.parent()?;
    let declarator = parameters.parent()?;
    if parameters.kind() != "parameter_list" || declarator.kind() != "function_declarator" {
        return None;
    }
    let mut owner = declarator.parent()?;
    while matches!(owner.kind(), "pointer_declarator" | "reference_declarator") {
        owner = owner.parent()?;
    }
    if owner.kind() != "function_definition" {
        return None;
    }
    owner.child_by_field_name("body")
}

pub(super) fn analyze(
    _project: &Project,
    files: &BTreeMap<String, Facts>,
    analysis: &mut Analysis,
) {
    for (path, facts) in files {
        if let Some(opaque) = &facts.opaque {
            let message = match opaque.kind {
                OpaqueKind::Include => "C++ includes require build-aware preprocessing",
                OpaqueKind::Assembly => "inline assembly may reference C++ symbols by name",
            };
            analysis
                .issues
                .push(
                    Issue::new("unsupported_cpp_context", path, message).at(Location {
                        line: opaque.line,
                        column: opaque.column,
                    }),
                );
            continue;
        }
        let source_file = Path::new(path)
            .extension()
            .and_then(|suffix| suffix.to_str())
            .is_some_and(|suffix| {
                ["cpp", "cc", "cxx"]
                    .iter()
                    .any(|extension| suffix.eq_ignore_ascii_case(extension))
            });
        for function in &facts.functions {
            let retention = if source_file {
                function.retention
            } else {
                Some(Retention::External)
            };
            if let Some(retention) = retention {
                analysis.evidence.push(Evidence {
                    file: path.clone(),
                    line: function.line,
                    full_name: function.name.clone(),
                    kind: match retention {
                        Retention::External => "cpp_external_or_header_function",
                        Retention::Ambiguous => "cpp_ambiguous_function_identity",
                        Retention::Reference => "cpp_file_local_reference",
                    },
                    certainty: "possible",
                });
            } else {
                analysis.findings.push(Finding {
                    category: Category::Function,
                    name: function.name.clone(),
                    full_name: function.name.clone(),
                    file: path.clone(),
                    line: function.line,
                    column: function.column,
                    confidence: 100,
                    rule_id: "SKY-U001",
                    reason: "cpp_unreferenced_file_local_function",
                });
            }
        }
    }
}
