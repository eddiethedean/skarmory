//! Conservative reachability for a single Rust root with ordinary free functions.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde::{Deserialize, Serialize};
use skarmory_python::Location;
use tree_sitter::{Node, Parser};

use super::{Analysis, Category, Evidence, Finding};
use crate::Issue;
use crate::preview_source::Project;

const MAX_SOURCE_BYTES: usize = 64 * 1024;

#[derive(Serialize, Deserialize)]
pub(super) struct Facts {
    functions: Vec<Function>,
}

#[derive(Serialize, Deserialize)]
struct Function {
    name: String,
    line: usize,
    column: usize,
    public: bool,
    bindings: BTreeSet<String>,
    references: BTreeSet<String>,
    calls: BTreeSet<String>,
}

pub(super) fn extract(path: &str, source: &str) -> Result<Facts, Issue> {
    if source.len() > MAX_SOURCE_BYTES {
        return Err(Issue::new(
            "analysis_limit",
            path,
            "Rust source exceeds 64 KiB",
        ));
    }
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_rust::LANGUAGE.into())
        .map_err(|error| Issue::new("rust_parser_unavailable", path, error))?;
    let tree = parser.parse(source, None).ok_or_else(|| {
        Issue::new(
            "rust_parse_error",
            path,
            "Rust parser did not return a tree",
        )
    })?;
    let root = tree.root_node();
    if root.has_error() {
        let mut nodes = vec![root];
        while let Some(node) = nodes.pop() {
            if node.is_error() || node.is_missing() {
                return Err(at(
                    "rust_parse_error",
                    path,
                    node,
                    "invalid or incomplete Rust syntax",
                ));
            }
            let mut cursor = node.walk();
            let children: Vec<_> = node.children(&mut cursor).collect();
            nodes.extend(children.into_iter().rev().filter(|child| child.has_error()));
        }
        return Err(at(
            "rust_parse_error",
            path,
            root,
            "invalid or incomplete Rust syntax",
        ));
    }
    let mut functions = Vec::new();
    let mut cursor = root.walk();
    for item in root.named_children(&mut cursor) {
        match item.kind() {
            "line_comment" | "block_comment" => {}
            "function_item" => functions.push(function(path, source, item)?),
            _ => {
                return Err(at(
                    "unsupported_rust_item",
                    path,
                    item,
                    "Rust reachability covers top-level free functions without module declarations or attributes",
                ));
            }
        }
    }
    Ok(Facts { functions })
}

fn function(path: &str, source: &str, item: Node<'_>) -> Result<Function, Issue> {
    let name = item
        .child_by_field_name("name")
        .expect("valid Rust function name");
    let body = item.child_by_field_name("body").ok_or_else(|| {
        at(
            "unsupported_rust_item",
            path,
            item,
            "Rust reachability requires a function body",
        )
    })?;
    let parameters = item
        .child_by_field_name("parameters")
        .expect("valid Rust function parameters");
    let mut cursor = parameters.walk();
    if parameters
        .named_children(&mut cursor)
        .any(|node| !matches!(node.kind(), "line_comment" | "block_comment"))
        || item.child_by_field_name("type_parameters").is_some()
    {
        return Err(at(
            "unsupported_rust_binding",
            path,
            item,
            "Rust reachability currently covers parameterless functions",
        ));
    }
    let position = name.start_position();
    let mut facts = Function {
        name: source[name.byte_range()].to_owned(),
        line: position.row + 1,
        column: position.column + 1,
        public: item
            .named_children(&mut item.walk())
            .any(|child| child.kind() == "visibility_modifier"),
        bindings: BTreeSet::new(),
        references: BTreeSet::new(),
        calls: BTreeSet::new(),
    };

    let mut nodes = vec![body];
    while let Some(node) = nodes.pop() {
        match node.kind() {
            "let_declaration" => {
                let pattern = node
                    .child_by_field_name("pattern")
                    .expect("valid let pattern");
                if pattern.kind() != "identifier" {
                    return Err(at(
                        "unsupported_rust_binding",
                        path,
                        pattern,
                        "complex let pattern",
                    ));
                }
                facts
                    .bindings
                    .insert(source[pattern.byte_range()].to_owned());
                nodes.extend(node.child_by_field_name("value"));
                continue;
            }
            "identifier" => {
                facts
                    .references
                    .insert(source[node.byte_range()].to_owned());
            }
            "call_expression" => {
                if let Some(callee) = node.child_by_field_name("function")
                    && callee.kind() == "identifier"
                {
                    facts.calls.insert(source[callee.byte_range()].to_owned());
                } else {
                    return Err(at(
                        "unsupported_rust_call",
                        path,
                        node,
                        "unresolved Rust call form",
                    ));
                }
            }
            "attribute_item"
            | "inner_attribute_item"
            | "macro_invocation"
            | "macro_definition"
            | "scoped_identifier"
            | "generic_function"
            | "closure_expression"
            | "match_expression"
            | "for_expression"
            | "let_condition"
            | "let_chain"
            | "struct_expression"
            | "function_item"
            | "use_declaration"
            | "mod_item"
            | "extern_crate_declaration"
            | "static_item"
            | "const_item"
            | "foreign_mod_item"
            | "impl_item"
            | "trait_item"
            | "type_item"
            | "struct_item"
            | "enum_item"
            | "union_item" => {
                return Err(at(
                    "unsupported_rust_binding",
                    path,
                    node,
                    format!("Rust reachability cannot resolve {}", node.kind()),
                ));
            }
            _ => {}
        }
        let mut cursor = node.walk();
        nodes.extend(node.named_children(&mut cursor));
    }
    Ok(facts)
}

pub(super) fn analyze(project: &Project, files: &BTreeMap<String, Facts>, analysis: &mut Analysis) {
    let Some((path, facts)) = files.first_key_value() else {
        return;
    };
    if files.len() != 1
        || !matches!(
            Path::new(path).file_name().and_then(|name| name.to_str()),
            Some("main.rs" | "lib.rs")
        )
    {
        analysis.issues.push(Issue::new(
            "unsupported_rust_project",
            &project.label(project.base()),
            "Rust reachability requires a single main.rs or lib.rs crate root",
        ));
        return;
    }

    let functions: BTreeMap<_, _> = facts
        .functions
        .iter()
        .map(|item| (item.name.as_str(), item))
        .collect();
    if functions.len() != facts.functions.len() {
        analysis.issues.push(Issue::new(
            "ambiguous_rust_definition",
            path,
            "duplicate Rust function",
        ));
        return;
    }
    let application = Path::new(path).ends_with("main.rs");
    if application && !functions.contains_key("main") {
        analysis.issues.push(Issue::new(
            "missing_rust_entrypoint",
            path,
            "main.rs has no main function",
        ));
        return;
    }
    for function in functions.values() {
        let issue = if function
            .bindings
            .iter()
            .any(|binding| functions.contains_key(binding.as_str()))
        {
            Some((
                "ambiguous_rust_binding",
                "a local binding shadows a declared function".to_owned(),
            ))
        } else {
            function
                .calls
                .iter()
                .find(|call| {
                    !functions.contains_key(call.as_str())
                        && !function.bindings.contains(call.as_str())
                })
                .map(|call| {
                    (
                        "unresolved_rust_call",
                        format!("cannot resolve Rust call {call:?}"),
                    )
                })
        };
        if let Some((kind, message)) = issue {
            analysis
                .issues
                .push(Issue::new(kind, path, message).at(Location {
                    line: function.line,
                    column: function.column,
                }));
        }
    }
    if !analysis.issues.is_empty() {
        return;
    }

    let mut pending: Vec<_> = functions
        .values()
        .filter(|item| item.public || (application && item.name == "main"))
        .map(|item| item.name.as_str())
        .collect();
    let mut reached = BTreeSet::new();
    while let Some(name) = pending.pop() {
        if reached.insert(name) {
            pending.extend(
                functions[name]
                    .references
                    .iter()
                    .map(String::as_str)
                    .filter(|reference| functions.contains_key(reference)),
            );
        }
    }
    for function in &facts.functions {
        if reached.contains(function.name.as_str()) {
            analysis.evidence.push(Evidence {
                file: path.clone(),
                line: function.line,
                full_name: function.name.clone(),
                kind: "rust_reachable_function",
                certainty: "definite",
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
                reason: "unreachable_rust_function",
            });
        }
    }
}

fn at(kind: &'static str, path: &str, node: Node<'_>, message: impl ToString) -> Issue {
    let location = node.start_position();
    Issue::new(kind, path, message).at(Location {
        line: location.row + 1,
        column: location.column + 1,
    })
}
