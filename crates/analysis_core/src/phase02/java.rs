//! Bounded syntax-derived Java reachability and framework entry point facts.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::path::PathBuf;

use roxmltree::{Document, ParsingOptions};
use serde::{Deserialize, Serialize};
use skarmory_python::Location;
use tree_sitter::{Node, Parser};

use super::{Analysis, Category, Evidence, Finding};
use crate::Issue;
use crate::preview_source::{self, Project, Stamp};

const SOURCE_NODES: usize = 24_576;
const RESOURCE_NODES: u32 = 16_384;
const CALLS_PER_METHOD: usize = 2_048;
const MAX_CALL_RESOLUTION_STEPS: usize = 131_072;
const MAX_JAVA_RESOURCE_BYTES: usize = 2 * 1024 * 1024;
const MAX_JAVA_RESOURCES: usize = 512;
const FXML_NAMESPACE: &str = "http://javafx.com/fxml";
const CONSTRUCTOR: &str = "<init>";

#[derive(Debug, Deserialize, Serialize)]
pub(super) struct Facts {
    package: String,
    imports: BTreeMap<String, String>,
    wildcard_imports: Vec<String>,
    classes: Vec<Class>,
    providers: Vec<String>,
}

#[derive(Debug, Deserialize, Serialize)]
struct Class {
    name: String,
    line: usize,
    column: usize,
    public: bool,
    annotations: Vec<String>,
    parents: Vec<String>,
    methods: Vec<Method>,
    refs: Refs,
}

#[derive(Debug, Deserialize, Serialize)]
struct Method {
    name: String,
    line: usize,
    column: usize,
    entry: bool,
    public: bool,
    static_method: bool,
    abstract_method: bool,
    parameter_types: Vec<String>,
    annotations: Vec<String>,
    refs: Refs,
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct Refs {
    calls: Vec<Call>,
    types: BTreeSet<String>,
    reflections: Vec<Reflection>,
    fxml: Vec<FxmlUse>,
    unknown: Option<Site>,
}

#[derive(Debug, Deserialize, Serialize)]
struct FxmlUse {
    loader_type: String,
    path: Option<String>,
    site: Site,
}

#[derive(Debug, Deserialize, Serialize)]
struct Call {
    owner: Receiver,
    name: String,
    site: Site,
}

#[derive(Debug, Deserialize, Serialize)]
enum Receiver {
    This,
    Super,
    Type(String),
    External,
    Unknown,
}

#[derive(Debug, Deserialize, Serialize)]
struct Reflection {
    owner: String,
    name: Option<String>,
    site: Site,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
struct Site {
    line: usize,
    column: usize,
}

impl Site {
    fn new(node: Node<'_>) -> Self {
        let point = node.start_position();
        Self {
            line: point.row + 1,
            column: point.column + 1,
        }
    }

    fn issue(self, kind: &'static str, path: &str, message: impl ToString) -> Issue {
        Issue::new(kind, path, message).at(Location {
            line: self.line,
            column: self.column,
        })
    }
}

fn children(node: Node<'_>) -> impl Iterator<Item = Node<'_>> {
    (0..node.named_child_count()).filter_map(move |i| node.named_child(i as u32))
}

fn nodes(node: Node<'_>) -> impl Iterator<Item = Node<'_>> {
    let mut remaining = vec![node];
    std::iter::from_fn(move || {
        let current = remaining.pop()?;
        for i in (0..current.named_child_count()).rev() {
            if let Some(child) = current.named_child(i as u32) {
                remaining.push(child);
            }
        }
        Some(current)
    })
}

fn text<'s>(node: Node<'_>, source: &'s str) -> &'s str {
    &source[node.byte_range()]
}

fn name<'s>(node: Node<'_>, source: &'s str) -> Option<&'s str> {
    Some(text(node.child_by_field_name("name")?, source))
}

fn qualification(value: &str) -> String {
    value.chars().filter(|c| !c.is_whitespace()).collect()
}

fn token(node: Node<'_>, token: &str) -> bool {
    (0..node.child_count()).any(|i| node.child(i).is_some_and(|part| part.kind() == token))
}

fn annotation_names(node: Node<'_>, source: &str) -> Vec<String> {
    children(node)
        .filter(|part| part.kind() == "modifiers")
        .flat_map(children)
        .filter(|part| matches!(part.kind(), "annotation" | "marker_annotation"))
        .filter_map(|part| name(part, source).map(qualification))
        .collect()
}

fn modifier(node: Node<'_>, kind: &str) -> bool {
    children(node)
        .find(|n| n.kind() == "modifiers")
        .is_some_and(|n| token(n, kind))
}

fn kind_is_class(kind: &str) -> bool {
    matches!(
        kind,
        "class_declaration" | "interface_declaration" | "enum_declaration" | "record_declaration"
    )
}

fn bare_type(node: Node<'_>, source: &str) -> String {
    match node.kind() {
        "generic_type" | "array_type" | "annotated_type" => {
            if let Some(inner) = node.child_by_field_name("element").or_else(|| {
                children(node).find(|n| !matches!(n.kind(), "annotation" | "marker_annotation"))
            }) {
                return qualification(
                    text(inner, source)
                        .split_once('<')
                        .map_or(text(inner, source), |(t, _)| t),
                );
            }
            qualification(text(node, source))
        }
        _ => qualification(text(node, source)),
    }
}

fn type_names(node: Node<'_>, source: &str) -> Vec<String> {
    let mut output = Vec::new();
    let mut stack = vec![node];
    while let Some(node) = stack.pop() {
        if matches!(node.kind(), "type_identifier" | "scoped_type_identifier") {
            output.push(qualification(text(node, source)));
        } else if !matches!(node.kind(), "annotation" | "marker_annotation") {
            stack.extend(children(node));
        }
    }
    output
}

fn literal_string<'s>(node: Node<'_>, source: &'s str) -> Option<&'s str> {
    if node.kind() != "string_literal" {
        return None;
    }
    let literal = text(node, source).strip_prefix('"')?.strip_suffix('"')?;
    (!literal.contains(['\n', '\r', '\\', '"'])).then_some(literal)
}

fn args(node: Node<'_>) -> impl Iterator<Item = Node<'_>> {
    node.child_by_field_name("arguments")
        .into_iter()
        .flat_map(children)
        .filter(|child| !matches!(child.kind(), "line_comment" | "block_comment"))
}

fn plain_java_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|first| first.is_alphabetic() || matches!(first, '_' | '$'))
        && chars.all(|ch| ch.is_alphanumeric() || matches!(ch, '_' | '$'))
}

fn dotted_java_name(name: &str) -> bool {
    !name.is_empty() && name.split('.').all(plain_java_identifier)
}

pub(super) fn extract(path: &str, source: &str) -> Result<Facts, Issue> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_java::LANGUAGE.into())
        .map_err(|err| Issue::new("unsupported_java_grammar", path, err))?;
    let tree = parser.parse(source, None).ok_or_else(|| {
        Issue::new(
            "java_parse_error",
            path,
            "Java parser produced no syntax tree",
        )
    })?;
    let root = tree.root_node();
    if root.has_error() {
        let bad = nodes(root)
            .take(SOURCE_NODES)
            .find(|node| node.is_error() || node.is_missing())
            .unwrap_or(root);
        return Err(Site::new(bad).issue("parse_error", path, "invalid Java syntax"));
    }
    if nodes(root).take(SOURCE_NODES + 1).count() > SOURCE_NODES {
        return Err(Issue::new(
            "analysis_limit",
            path,
            format!("Java syntax exceeds {SOURCE_NODES} named nodes"),
        ));
    }
    let mut facts = Facts {
        package: String::new(),
        imports: BTreeMap::new(),
        wildcard_imports: Vec::new(),
        classes: Vec::new(),
        providers: Vec::new(),
    };
    for item in children(root) {
        match item.kind() {
            "package_declaration" => {
                facts.package = children(item)
                    .find(|node| matches!(node.kind(), "identifier" | "scoped_identifier"))
                    .map_or_else(String::new, |node| qualification(text(node, source)));
            }
            "import_declaration" => {
                if token(item, "static") {
                    return Err(Site::new(item).issue(
                        "unsupported_java_import",
                        path,
                        "Java static imports require member resolution outside the supported scope",
                    ));
                }
                let Some(qualified) = children(item)
                    .find(|node| matches!(node.kind(), "identifier" | "scoped_identifier"))
                    .map(|node| qualification(text(node, source)))
                else {
                    continue;
                };
                if children(item).any(|node| node.kind() == "asterisk") {
                    facts.wildcard_imports.push(qualified);
                } else if let Some((_, simple)) = qualified.rsplit_once('.')
                    && facts
                        .imports
                        .insert(simple.into(), qualified.clone())
                        .is_some_and(|prior| prior != qualified)
                {
                    return Err(Site::new(item).issue(
                        "ambiguous_java_import",
                        path,
                        format!("multiple Java imports have the simple name {simple}"),
                    ));
                }
            }
            "module_declaration" => {
                for directive in
                    nodes(item).filter(|node| node.kind() == "provides_module_directive")
                {
                    facts.providers.extend(
                        children(directive)
                            .filter(|node| {
                                matches!(node.kind(), "identifier" | "scoped_identifier")
                            })
                            .skip(1)
                            .map(|node| qualification(text(node, source))),
                    );
                }
            }
            kind if kind_is_class(kind) => facts.classes.push(extract_class(item, path, source)?),
            "line_comment" | "block_comment" => {}
            _ => {
                return Err(Site::new(item).issue(
                    "unsupported_java_declaration",
                    path,
                    format!("unsupported Java top-level declaration {}", item.kind()),
                ));
            }
        }
    }
    Ok(facts)
}

fn extract_class(node: Node<'_>, path: &str, source: &str) -> Result<Class, Issue> {
    let identifier = node.child_by_field_name("name").ok_or_else(|| {
        Site::new(node).issue(
            "unsupported_java_declaration",
            path,
            "Java type has no explicit name",
        )
    })?;
    let body = node.child_by_field_name("body").ok_or_else(|| {
        Site::new(node).issue(
            "unsupported_java_declaration",
            path,
            "Java type has no explicit body",
        )
    })?;
    let mut items: Vec<_> = children(body).collect();
    if let Some(enum_members) = items
        .iter()
        .find(|item| item.kind() == "enum_body_declarations")
    {
        items.extend(children(*enum_members));
    }
    let mut fields = BTreeMap::new();
    for field in items
        .iter()
        .filter(|node| node.kind() == "field_declaration")
    {
        if let Some(ty) = field.child_by_field_name("type") {
            for variable in children(*field).filter(|node| node.kind() == "variable_declarator") {
                if let Some(field_name) = name(variable, source) {
                    fields.insert(field_name, bare_type(ty, source));
                }
            }
        }
    }
    let mut refs = Refs::default();
    let mut methods = Vec::new();
    for item in items {
        match item.kind() {
            "method_declaration" | "constructor_declaration" => {
                methods.push(extract_method(
                    item,
                    path,
                    source,
                    &fields,
                    node.kind() == "interface_declaration",
                )?);
            }
            "field_declaration" | "static_initializer" | "block" | "enum_constant" => {
                let extra = extract_refs(item, path, source, &fields, None)?;
                refs.calls.extend(extra.calls);
                refs.types.extend(extra.types);
                refs.reflections.extend(extra.reflections);
                refs.fxml.extend(extra.fxml);
                refs.unknown = refs.unknown.or(extra.unknown);
            }
            "line_comment" | "block_comment" | "enum_body_declarations" => {}
            _ => {
                return Err(Site::new(item).issue(
                    "unsupported_java_declaration",
                    path,
                    format!("unsupported Java type member {}", item.kind()),
                ));
            }
        }
    }
    let parents = children(node)
        .filter(|n| {
            matches!(
                n.kind(),
                "superclass" | "super_interfaces" | "extends_interfaces"
            )
        })
        .flat_map(|n| type_names(n, source))
        .collect();
    let site = Site::new(identifier);
    Ok(Class {
        name: text(identifier, source).into(),
        line: site.line,
        column: site.column,
        public: modifier(node, "public") || modifier(node, "protected"),
        annotations: annotation_names(node, source),
        parents,
        methods,
        refs,
    })
}

fn extract_method(
    node: Node<'_>,
    path: &str,
    source: &str,
    fields: &BTreeMap<&str, String>,
    interface: bool,
) -> Result<Method, Issue> {
    let identifier = node.child_by_field_name("name").ok_or_else(|| {
        Site::new(node).issue(
            "unsupported_java_declaration",
            path,
            "Java method has no explicit name",
        )
    })?;
    let parameters = node.child_by_field_name("parameters");
    let params: Vec<_> = parameters.into_iter().flat_map(children).collect();
    let parameter_types: Vec<_> = params
        .iter()
        .filter_map(|n| parameter(*n, source).map(|(_, ty)| ty))
        .collect();
    let body = node.child_by_field_name("body");
    let mut refs = if let Some(body) = body {
        extract_refs(body, path, source, fields, parameters)?
    } else {
        Refs::default()
    };
    if let Some(ty) = node.child_by_field_name("type") {
        refs.types.extend(type_names(ty, source));
    }
    for parameter in params.iter().filter_map(|n| n.child_by_field_name("type")) {
        refs.types.extend(type_names(parameter, source));
    }
    for thrown in children(node).filter(|n| n.kind() == "throws") {
        refs.types.extend(type_names(thrown, source));
    }
    let static_method = modifier(node, "static");
    let public = interface || modifier(node, "public") || modifier(node, "protected");
    let name = text(identifier, source);
    let entry = name == "main"
        && modifier(node, "public")
        && static_method
        && node
            .child_by_field_name("type")
            .is_some_and(|ty| ty.kind() == "void_type")
        && parameter_types.len() == 1
        && matches!(parameter_types[0].as_str(), "String" | "java.lang.String")
        && params.first().is_some_and(|p| {
            p.kind() == "spread_parameter"
                || p.child_by_field_name("type")
                    .is_some_and(|ty| ty.kind() == "array_type")
                || p.child_by_field_name("dimensions").is_some()
        });
    let site = Site::new(identifier);
    Ok(Method {
        name: if node.kind() == "constructor_declaration" {
            CONSTRUCTOR
        } else {
            name
        }
        .into(),
        line: site.line,
        column: site.column,
        entry,
        public,
        static_method,
        abstract_method: body.is_none() && !modifier(node, "native"),
        parameter_types,
        annotations: annotation_names(node, source),
        refs,
    })
}

fn parameter<'s>(node: Node<'_>, source: &'s str) -> Option<(&'s str, String)> {
    if node.kind() == "spread_parameter" {
        let variable = children(node).find(|item| item.kind() == "variable_declarator")?;
        let ty = children(node).find(|item| {
            !matches!(
                item.kind(),
                "variable_declarator" | "modifiers" | "annotation" | "marker_annotation"
            )
        })?;
        return Some((name(variable, source)?, bare_type(ty, source)));
    }
    Some((
        name(node, source)?,
        bare_type(node.child_by_field_name("type")?, source),
    ))
}

struct Binding<'s> {
    name: &'s str,
    ty: String,
    start: usize,
    end: usize,
}

fn local_bindings<'s>(
    body: Node<'_>,
    parameters: Option<Node<'_>>,
    source: &'s str,
) -> Vec<Binding<'s>> {
    let mut bindings: Vec<_> = parameters
        .into_iter()
        .flat_map(children)
        .filter_map(|p| parameter(p, source))
        .map(|(name, ty)| Binding {
            name,
            ty,
            start: body.start_byte(),
            end: body.end_byte(),
        })
        .collect();
    for declaration in nodes(body).filter(|item| item.kind() == "local_variable_declaration") {
        let Some(ty) = declaration.child_by_field_name("type") else {
            continue;
        };
        let mut scope = declaration.parent();
        while let Some(parent) = scope
            && !matches!(
                parent.kind(),
                "block" | "constructor_body" | "for_statement" | "try_with_resources_statement"
            )
        {
            scope = parent.parent();
        }
        let end = scope.map_or(body.end_byte(), |n| n.end_byte());
        let ty = bare_type(ty, source);
        for variable in children(declaration).filter(|item| item.kind() == "variable_declarator") {
            let Some(name) = name(variable, source) else {
                continue;
            };
            let ty = if ty == "var" {
                variable
                    .child_by_field_name("value")
                    .filter(|value| value.kind() == "object_creation_expression")
                    .and_then(|value| value.child_by_field_name("type"))
                    .map_or_else(|| ty.clone(), |ty| bare_type(ty, source))
            } else {
                ty.clone()
            };
            bindings.push(Binding {
                name,
                ty,
                start: variable.end_byte(),
                end,
            });
        }
    }
    bindings
}

fn receiver(
    node: Node<'_>,
    source: &str,
    bindings: &[Binding<'_>],
    fields: &BTreeMap<&str, String>,
    position: usize,
    depth: usize,
) -> Receiver {
    if depth == 16 {
        return Receiver::Unknown;
    }
    match node.kind() {
        "this" => Receiver::This,
        "super" => Receiver::Super,
        "identifier" => {
            let id = text(node, source);
            let local = bindings.iter().rev().find(|binding| {
                binding.name == id && binding.start <= position && position < binding.end
            });
            if let Some(ty) = local.map(|binding| &binding.ty).or_else(|| fields.get(id)) {
                if ty == "var" {
                    Receiver::Unknown
                } else {
                    Receiver::Type(ty.clone())
                }
            } else if id.chars().next().is_some_and(char::is_uppercase) {
                Receiver::Type(id.into())
            } else {
                Receiver::Unknown
            }
        }
        "object_creation_expression" | "cast_expression" => node
            .child_by_field_name("type")
            .map_or(Receiver::Unknown, |ty| {
                Receiver::Type(bare_type(ty, source))
            }),
        "parenthesized_expression" => children(node).next().map_or(Receiver::Unknown, |inner| {
            receiver(inner, source, bindings, fields, position, depth + 1)
        }),
        "field_access" => {
            let Some(object) = node.child_by_field_name("object") else {
                return Receiver::Unknown;
            };
            let Some(field) = node.child_by_field_name("field") else {
                return Receiver::Unknown;
            };
            match receiver(object, source, bindings, fields, position, depth + 1) {
                Receiver::This => fields
                    .get(text(field, source))
                    .map_or(Receiver::Unknown, |ty| Receiver::Type(ty.clone())),
                Receiver::External => Receiver::External,
                Receiver::Type(owner)
                    if owner == "System" && matches!(text(field, source), "out" | "err" | "in") =>
                {
                    Receiver::External
                }
                _ if dotted_java_name(text(node, source))
                    && text(field, source)
                        .chars()
                        .next()
                        .is_some_and(char::is_uppercase) =>
                {
                    Receiver::Type(qualification(text(node, source)))
                }
                _ => Receiver::Unknown,
            }
        }
        "method_invocation" => {
            if name(node, source) == Some("getClass")
                && args(node).next().is_none()
                && node
                    .child_by_field_name("object")
                    .is_none_or(|owner| owner.kind() == "this")
            {
                return Receiver::External;
            }
            let Some(owner) = node.child_by_field_name("object") else {
                return Receiver::Unknown;
            };
            match receiver(owner, source, bindings, fields, position, depth + 1) {
                Receiver::External => Receiver::External,
                Receiver::Type(ty) if builtin(&ty) => Receiver::External,
                _ => Receiver::Unknown,
            }
        }
        "string_literal" | "class_literal" | "array_creation_expression" => Receiver::External,
        _ => Receiver::Unknown,
    }
}

fn extract_refs(
    body: Node<'_>,
    path: &str,
    source: &str,
    fields: &BTreeMap<&str, String>,
    parameters: Option<Node<'_>>,
) -> Result<Refs, Issue> {
    let bindings = local_bindings(body, parameters, source);
    let mut refs = Refs::default();
    // Keep the resource association scoped to its source declaration. Project
    // analysis later resolves the resource against the selected snapshot.
    let mut loaders = BTreeMap::new();
    let binding_at = |node: Node<'_>| {
        if node.kind() != "identifier" {
            return None;
        }
        bindings.iter().rev().find(|binding| {
            binding.name == text(node, source)
                && binding.start <= node.start_byte()
                && node.start_byte() < binding.end
        })
    };
    for node in nodes(body) {
        match node.kind() {
            "variable_declarator" => {
                let Some(created) = node
                    .child_by_field_name("value")
                    .filter(|v| v.kind() == "object_creation_expression")
                else {
                    continue;
                };
                let Some(ty) = created.child_by_field_name("type") else {
                    continue;
                };
                if bare_type(ty, source).rsplit('.').next() == Some("FXMLLoader")
                    && let Some(resource) = args(created)
                        .next()
                        .filter(|arg| name(*arg, source) == Some("getResource"))
                    && let Some(path) = args(resource)
                        .next()
                        .and_then(|arg| literal_string(arg, source))
                {
                    loaders.insert(node.end_byte(), path);
                }
            }
            "assignment_expression" => {
                if let Some(binding) = node.child_by_field_name("left").and_then(binding_at) {
                    loaders.remove(&binding.start);
                }
            }
            "method_invocation" => {
                let Some(member) = name(node, source) else {
                    continue;
                };
                let object = node.child_by_field_name("object");
                let site = Site::new(node);
                let owner = object.map_or(Receiver::This, |object| {
                    receiver(object, source, &bindings, fields, node.start_byte(), 0)
                });
                if member == "forName"
                    && let Receiver::Type(owner_type) = &owner
                    && matches!(owner_type.as_str(), "Class" | "java.lang.Class")
                {
                    refs.reflections.push(Reflection {
                        owner: owner_type.clone(),
                        name: args(node)
                            .next()
                            .and_then(|arg| literal_string(arg, source))
                            .map(str::to_owned),
                        site,
                    });
                }
                if member == "setLocation" {
                    if let Some(binding) = object.and_then(binding_at) {
                        loaders.remove(&binding.start);
                    }
                } else if member == "setController"
                    && let Receiver::Type(loader_type) = &owner
                    && matches!(
                        loader_type.as_str(),
                        "FXMLLoader" | "javafx.fxml.FXMLLoader"
                    )
                {
                    let resource = args(node)
                        .next()
                        .filter(|child| child.kind() == "this")
                        .and_then(|_| object.and_then(binding_at))
                        .and_then(|binding| loaders.get(&binding.start))
                        .map(|path| (*path).to_owned());
                    refs.fxml.push(FxmlUse {
                        loader_type: loader_type.clone(),
                        path: resource,
                        site,
                    });
                }
                refs.calls.push(Call {
                    owner,
                    name: member.into(),
                    site,
                });
            }
            "object_creation_expression" => {
                if children(node).any(|child| child.kind() == "class_body") {
                    return Err(Site::new(node).issue(
                        "unsupported_java_declaration",
                        path,
                        "anonymous Java types require additional dispatch analysis",
                    ));
                }
                if let Some(ty) = node.child_by_field_name("type") {
                    refs.calls.push(Call {
                        owner: Receiver::Type(bare_type(ty, source)),
                        name: CONSTRUCTOR.into(),
                        site: Site::new(node),
                    });
                }
            }
            "type_identifier" | "scoped_type_identifier" => {
                if node.parent().is_none_or(|parent| {
                    !matches!(parent.kind(), "scoped_type_identifier" | "type_parameter")
                }) {
                    refs.types.insert(qualification(text(node, source)));
                }
            }
            "method_reference" => {
                // Method references can hide overload and receiver identity.
                refs.unknown.get_or_insert(Site::new(node));
            }
            kind if kind_is_class(kind) => {
                return Err(Site::new(node).issue(
                    "unsupported_java_declaration",
                    path,
                    "local Java types require additional dispatch analysis",
                ));
            }
            _ => {}
        }
        if refs.calls.len() > CALLS_PER_METHOD {
            return Err(Issue::new(
                "analysis_limit",
                path,
                format!("Java body exceeds {CALLS_PER_METHOD} calls"),
            ));
        }
    }
    Ok(refs)
}

fn implicit_object_call(name: &str) -> bool {
    matches!(
        name,
        "getClass"
            | "toString"
            | "equals"
            | "clone"
            | "hashCode"
            | "finalize"
            | "wait"
            | "notify"
            | "notifyAll"
    )
}

fn builtin(name: &str) -> bool {
    matches!(
        name,
        "boolean"
            | "byte"
            | "short"
            | "int"
            | "long"
            | "float"
            | "double"
            | "char"
            | "void"
            | "Object"
            | "System"
            | "String"
            | "StringBuilder"
            | "StringBuffer"
            | "Boolean"
            | "Byte"
            | "Short"
            | "Integer"
            | "Long"
            | "Float"
            | "Double"
            | "Character"
            | "Class"
            | "ClassLoader"
            | "Math"
            | "Number"
            | "Throwable"
            | "Error"
            | "Exception"
            | "RuntimeException"
            | "IllegalArgumentException"
            | "IllegalStateException"
            | "NullPointerException"
            | "Thread"
            | "Enum"
            | "Record"
            | "Comparable"
            | "Iterable"
            | "AutoCloseable"
            | "Runnable"
    )
}

fn external_type(name: &str) -> bool {
    name.starts_with("java.") || name.starts_with("javax.") || name.starts_with("javafx.")
}

fn qualified(package: &str, simple: &str) -> String {
    if package.is_empty() {
        simple.to_owned()
    } else {
        format!("{package}.{simple}")
    }
}

#[derive(Clone, Copy)]
struct IndexedClass<'a> {
    path: &'a str,
    facts: &'a Facts,
    class: &'a Class,
}

struct Graph<'a> {
    classes: Vec<IndexedClass<'a>>,
    names: BTreeMap<String, usize>,
    parents: Vec<Vec<usize>>,
    children: Vec<Vec<usize>>,
}

#[derive(Clone, Copy)]
enum Resolution {
    Local(usize),
    External,
    Missing,
    Ambiguous,
}

impl<'a> Graph<'a> {
    fn new(files: &'a BTreeMap<String, Facts>, issues: &mut Vec<Issue>) -> Self {
        let mut classes = Vec::new();
        let mut names = BTreeMap::new();
        for (path, facts) in files {
            for class in &facts.classes {
                let identity = qualified(&facts.package, &class.name);
                if names.insert(identity.clone(), classes.len()).is_some() {
                    issues.push(Issue::new(
                        "ambiguous_java_type",
                        path,
                        format!("multiple selected Java declarations define {identity}"),
                    ));
                }
                classes.push(IndexedClass { path, facts, class });
            }
        }
        let mut graph = Self {
            parents: vec![Vec::new(); classes.len()],
            children: vec![Vec::new(); classes.len()],
            classes,
            names,
        };
        for (index, item) in graph.classes.iter().enumerate() {
            for parent in &item.class.parents {
                match graph.resolve(item.facts, parent) {
                    Resolution::Local(parent) => {
                        graph.parents[index].push(parent);
                        graph.children[parent].push(index);
                    }
                    Resolution::External => {}
                    Resolution::Missing | Resolution::Ambiguous => issues.push(Issue::new(
                        "unresolved_java_supertype",
                        item.path,
                        format!("cannot resolve Java supertype {parent}"),
                    )),
                }
            }
        }
        graph
    }

    fn resolve(&self, facts: &Facts, name: &str) -> Resolution {
        if name.contains('.') {
            return self.names.get(name).map_or_else(
                || {
                    if external_type(name) {
                        Resolution::External
                    } else {
                        Resolution::Missing
                    }
                },
                |index| Resolution::Local(*index),
            );
        }
        if let Some(import) = facts.imports.get(name) {
            return self.resolve(facts, import);
        }
        if let Some(index) = self.names.get(&qualified(&facts.package, name)) {
            return Resolution::Local(*index);
        }
        let mut choices = facts
            .wildcard_imports
            .iter()
            .filter_map(|package| self.names.get(&qualified(package, name)).copied());
        match (choices.next(), choices.next()) {
            (_, Some(_)) => Resolution::Ambiguous,
            (Some(index), _) => Resolution::Local(index),
            _ if builtin(name) => Resolution::External,
            _ if facts.wildcard_imports.iter().any(|name| {
                name.starts_with("java.")
                    || name.starts_with("javax.")
                    || name.starts_with("javafx.")
            }) =>
            {
                Resolution::External
            }
            _ => Resolution::Missing,
        }
    }

    fn resource_type(&self, path: &str, name: &str, issues: &mut Vec<Issue>) -> Option<usize> {
        if let Some(&class) = self.names.get(name) {
            return Some(class);
        }
        let message = format!("Java resource names an unavailable selected type {name}");
        issues.push(Issue::new("unresolved_java_resource_type", path, message));
        None
    }

    fn references(&self, owner: usize, refs: &Refs, state: &mut State) {
        let item = self.classes[owner];
        if let Some(site) = refs.unknown {
            state.issues.push(site.issue(
                "unsupported_java_reference",
                item.path,
                "a reachable Java reference requires reflection or method-reference resolution",
            ));
        }
        for ty in &refs.types {
            match self.resolve(item.facts, ty) {
                Resolution::Local(class) => state.class(class, "java_type_reference"),
                Resolution::Ambiguous => state.issues.push(Issue::new(
                    "ambiguous_java_type",
                    item.path,
                    format!("multiple imports can supply Java type {ty}"),
                )),
                Resolution::External | Resolution::Missing => {}
            }
        }
        for reflection in &refs.reflections {
            if !matches!(
                self.resolve(item.facts, &reflection.owner),
                Resolution::External
            ) || item
                .facts
                .imports
                .get(&reflection.owner)
                .is_some_and(|name| name != "java.lang.Class")
            {
                continue;
            }
            let Some(name) = &reflection.name else {
                state.issues.push(reflection.site.issue(
                    "unsupported_java_reference",
                    item.path,
                    "Java Class.forName requires a literal class name",
                ));
                continue;
            };
            if let Resolution::Local(class) = self.resolve(item.facts, name) {
                state.construct(class, self, "java_literal_reflection");
            } else if let Some(class) = name
                .rsplit('.')
                .next()
                .and_then(|simple| self.names.get(simple).copied())
                .filter(|class| self.classes[*class].path == item.path)
            {
                // Skylos retains a same-file default-package class from a
                // qualified literal. The fallback supplies possible use.
                state.construct(class, self, "java_literal_reflection");
            } else if !external_type(name) {
                state.issues.push(reflection.site.issue(
                    "unresolved_java_reflection",
                    item.path,
                    format!("cannot resolve selected Java reflected type {name}"),
                ));
            }
        }
        for call in &refs.calls {
            let targets = match &call.owner {
                Receiver::This => vec![owner],
                Receiver::Super => self.parents[owner].clone(),
                Receiver::Type(name) => match self.resolve(item.facts, name) {
                    Resolution::Local(class) => vec![class],
                    Resolution::External => {
                        if matches!(name.as_str(), "System" | "java.lang.System")
                            && matches!(call.name.as_str(), "load" | "loadLibrary")
                        {
                            state.issues.push(call.site.issue(
                                "unsupported_java_reference",
                                item.path,
                                "Java native library loading can reference selected code",
                            ));
                        }
                        continue;
                    }
                    Resolution::Missing | Resolution::Ambiguous => {
                        state.issues.push(call.site.issue(
                            "unresolved_java_call",
                            item.path,
                            format!("cannot resolve Java receiver {name} for {}", call.name),
                        ));
                        continue;
                    }
                },
                Receiver::External => continue,
                Receiver::Unknown => {
                    state.issues.push(call.site.issue(
                        "unresolved_java_call",
                        item.path,
                        format!("cannot prove the receiver of Java call {}", call.name),
                    ));
                    continue;
                }
            };
            let mut resolved = false;
            for target in targets {
                state.class(target, "java_call");
                if call.name == CONSTRUCTOR {
                    state.construct(target, self, "java_constructor_call");
                    resolved = true;
                    continue;
                }
                let mut seen = BTreeSet::new();
                let mut hierarchy = vec![target];
                while let Some(class) = hierarchy.pop() {
                    if !seen.insert(class) {
                        continue;
                    }
                    state.call_resolution_steps += 1 + self.classes[class].class.methods.len();
                    if state.call_resolution_steps > MAX_CALL_RESOLUTION_STEPS {
                        state.issues.push(Issue::new(
                            "analysis_limit",
                            item.path,
                            "Java receiver resolution exhausted its work budget",
                        ));
                        return;
                    }
                    for (method, definition) in self.classes[class].class.methods.iter().enumerate()
                    {
                        if definition.name == call.name {
                            resolved = true;
                            state.method(class, method, "java_call");
                        }
                    }
                    hierarchy.extend(&self.parents[class]);
                    if !matches!(call.owner, Receiver::Super) {
                        hierarchy.extend(&self.children[class]);
                    }
                }
            }
            if !resolved && !implicit_object_call(&call.name) {
                state.issues.push(call.site.issue(
                    "unresolved_java_call",
                    item.path,
                    format!("cannot resolve reachable Java member {}", call.name),
                ));
            }
        }
    }
}

#[derive(Default)]
struct State {
    classes: BTreeMap<usize, &'static str>,
    methods: BTreeMap<(usize, usize), &'static str>,
    pending: VecDeque<(usize, Option<usize>)>,
    issues: Vec<Issue>,
    call_resolution_steps: usize,
}

impl State {
    fn class(&mut self, class: usize, reason: &'static str) {
        if let std::collections::btree_map::Entry::Vacant(entry) = self.classes.entry(class) {
            entry.insert(reason);
            self.pending.push_back((class, None));
        }
    }

    fn method(&mut self, class: usize, method: usize, reason: &'static str) {
        self.class(class, reason);
        if let std::collections::btree_map::Entry::Vacant(entry) =
            self.methods.entry((class, method))
        {
            entry.insert(reason);
            self.pending.push_back((class, Some(method)));
        }
    }

    fn construct(&mut self, class: usize, graph: &Graph<'_>, reason: &'static str) {
        self.class(class, reason);
        for (method, definition) in graph.classes[class].class.methods.iter().enumerate() {
            if definition.name == CONSTRUCTOR {
                self.method(class, method, reason);
            }
        }
    }
}

fn framework_class_annotation(name: &str) -> bool {
    matches!(
        name,
        "org.springframework.stereotype.Component"
            | "org.springframework.stereotype.Service"
            | "org.springframework.stereotype.Repository"
            | "org.springframework.stereotype.Controller"
            | "org.springframework.context.annotation.Configuration"
            | "org.springframework.web.bind.annotation.RestController"
            | "org.springframework.boot.autoconfigure.SpringBootApplication"
            | "jakarta.persistence.Entity"
            | "javax.persistence.Entity"
            | "jakarta.persistence.Embeddable"
            | "javax.persistence.Embeddable"
            | "jakarta.ws.rs.Path"
            | "javax.ws.rs.Path"
            | "jakarta.servlet.annotation.WebServlet"
            | "javax.servlet.annotation.WebServlet"
            | "jakarta.servlet.annotation.WebFilter"
            | "javax.servlet.annotation.WebFilter"
            | "org.junit.jupiter.api.extension.ExtendWith"
            | "org.junit.runner.RunWith"
    )
}

fn framework_method_annotation(name: &str) -> bool {
    let (package, member) = name.rsplit_once('.').unwrap_or(("", name));
    match package {
        "org.junit" | "org.junit.jupiter.api" | "org.junit.jupiter.params" => matches!(
            member,
            "Test"
                | "Before"
                | "After"
                | "BeforeClass"
                | "AfterClass"
                | "BeforeAll"
                | "AfterAll"
                | "BeforeEach"
                | "AfterEach"
                | "ParameterizedTest"
                | "RepeatedTest"
                | "TestFactory"
                | "TestTemplate"
        ),
        "org.springframework.web.bind.annotation" => matches!(
            member,
            "RequestMapping"
                | "GetMapping"
                | "PostMapping"
                | "PutMapping"
                | "DeleteMapping"
                | "PatchMapping"
                | "ExceptionHandler"
        ),
        "jakarta.ws.rs" | "javax.ws.rs" => {
            matches!(member, "GET" | "POST" | "PUT" | "DELETE" | "PATCH" | "Path")
        }
        "jakarta.annotation" | "javax.annotation" => {
            matches!(member, "PostConstruct" | "PreDestroy")
        }
        "org.springframework.context.annotation" => member == "Bean",
        "org.springframework.context.event" => member == "EventListener",
        "org.springframework.scheduling.annotation" => member == "Scheduled",
        _ => false,
    }
}

fn matches_annotation(facts: &Facts, name: &str, supported: impl Fn(&str) -> bool) -> bool {
    if name.contains('.') {
        return supported(name);
    }
    if let Some(import) = facts.imports.get(name) {
        return supported(import);
    }
    facts
        .wildcard_imports
        .iter()
        .any(|package| supported(&qualified(package, name)))
}

fn serialization_hook(method: &Method, facts: &Facts) -> bool {
    match (method.name.as_str(), method.parameter_types.as_slice()) {
        ("readObjectNoData" | "readResolve" | "writeReplace", []) => true,
        ("readObject", [ty]) => {
            ty == "java.io.ObjectInputStream"
                || (ty == "ObjectInputStream"
                    && facts
                        .imports
                        .get(ty)
                        .is_some_and(|name| name == "java.io.ObjectInputStream"))
        }
        ("writeObject", [ty]) => {
            ty == "java.io.ObjectOutputStream"
                || (ty == "ObjectOutputStream"
                    && facts
                        .imports
                        .get(ty)
                        .is_some_and(|name| name == "java.io.ObjectOutputStream"))
        }
        _ => false,
    }
}

#[derive(Debug, Default)]
struct Fxml {
    controller: Option<String>,
    handlers: BTreeSet<String>,
    unsupported: bool,
}

fn is_fxml_namespace(namespace: Option<&str>) -> bool {
    namespace.is_some_and(|uri| {
        uri == FXML_NAMESPACE
            || uri
                .strip_prefix(FXML_NAMESPACE)
                .and_then(|suffix| suffix.strip_prefix('/'))
                .is_some_and(|version| {
                    !version.is_empty() && version.bytes().all(|b| b.is_ascii_digit())
                })
    })
}

fn parse_fxml(path: &str, bytes: &[u8]) -> Result<Fxml, Issue> {
    let xml = std::str::from_utf8(bytes)
        .map_err(|err| Issue::new("java_resource_parse_error", path, err))?;
    let document = Document::parse_with_options(
        xml.trim_start_matches(|c: char| c == '\u{feff}' || c.is_ascii_whitespace()),
        ParsingOptions {
            nodes_limit: RESOURCE_NODES,
            ..ParsingOptions::default()
        },
    )
    .map_err(|err| Issue::new("java_resource_parse_error", path, err))?;
    let mut fxml = Fxml::default();
    for element in document.descendants().filter(roxmltree::Node::is_element) {
        if is_fxml_namespace(element.tag_name().namespace())
            && matches!(element.tag_name().name(), "script" | "include")
        {
            fxml.unsupported = true;
        }
        for attribute in element.attributes() {
            let value = attribute.value();
            if attribute.name() == "controller" && is_fxml_namespace(attribute.namespace()) {
                if !dotted_java_name(value) || fxml.controller.replace(value.into()).is_some() {
                    fxml.unsupported = true;
                }
            } else if attribute.namespace().is_none()
                && attribute
                    .name()
                    .strip_prefix("on")
                    .is_some_and(|name| name.chars().next().is_some_and(char::is_uppercase))
            {
                if let Some(handler) = value
                    .strip_prefix('#')
                    .filter(|name| plain_java_identifier(name))
                {
                    fxml.handlers.insert(handler.into());
                } else {
                    fxml.unsupported = true;
                }
            }
        }
    }
    Ok(fxml)
}

fn bind_fxml(path: &str, resource: &Fxml, class: usize, graph: &Graph<'_>, state: &mut State) {
    if resource.unsupported {
        state.issues.push(Issue::new(
            "unsupported_java_fxml",
            path,
            "FXML contains a dynamic or unsupported controller, script, include, or handler",
        ));
        return;
    }
    state.construct(class, graph, "java_fxml_controller");
    let definition = graph.classes[class].class;
    for (index, method) in definition.methods.iter().enumerate() {
        if resource.handlers.contains(&method.name)
            || (method.name == "initialize" && method.parameter_types.is_empty())
        {
            state.method(class, index, "java_fxml_callback");
        }
    }
    for handler in &resource.handlers {
        if !definition
            .methods
            .iter()
            .any(|method| method.name == *handler)
        {
            state.issues.push(Issue::new(
                "unresolved_java_fxml_handler",
                path,
                format!("no selected controller member named {handler}"),
            ));
        }
    }
}

fn resources(project: &Project, graph: &Graph<'_>, state: &mut State) {
    let mut fxml = BTreeMap::new();
    let mut reads = Vec::new();
    let mut total_bytes = 0;
    for (index, path) in project.java_context().enumerate() {
        let label = project.label(&path);
        if index == MAX_JAVA_RESOURCES {
            state.issues.push(Issue::new(
                "analysis_limit",
                &label,
                format!("selected Java metadata exceeds {MAX_JAVA_RESOURCES} files"),
            ));
            break;
        }
        if let Err(err) = preview_source::reject_symlink_components(project.base(), &path) {
            state
                .issues
                .push(Issue::new("unsupported_java_resource", &label, err));
            continue;
        }
        let (bytes, stamp) = match preview_source::read_snapshot(&path, &label, None) {
            Ok(snapshot) => snapshot,
            Err(issue) => {
                state.issues.push(issue);
                continue;
            }
        };
        total_bytes += bytes.len();
        if total_bytes > MAX_JAVA_RESOURCE_BYTES {
            state.issues.push(Issue::new(
                "analysis_limit",
                &label,
                "selected Java metadata exceeds 2 MiB",
            ));
            break;
        }
        if path
            .extension()
            .is_some_and(|extension| extension == "fxml")
        {
            match parse_fxml(&label, &bytes) {
                Ok(resource) => {
                    fxml.insert(label.clone(), (path.clone(), resource));
                }
                Err(issue) => state.issues.push(issue),
            }
        } else {
            match std::str::from_utf8(&bytes) {
                Ok(descriptor) => {
                    for provider in descriptor
                        .lines()
                        .map(|line| line.split('#').next().unwrap_or_default().trim())
                        .filter(|line| !line.is_empty())
                    {
                        if let Some(class) =
                            graph.resource_type(&label, provider, &mut state.issues)
                        {
                            state.construct(class, graph, "java_service_provider");
                        }
                    }
                }
                Err(err) => state
                    .issues
                    .push(Issue::new("java_resource_parse_error", &label, err)),
            }
        }
        reads.push((path, label, stamp));
    }
    for (label, (_, resource)) in &fxml {
        if let Some(controller) = &resource.controller {
            if let Some(class) = graph.resource_type(label, controller, &mut state.issues) {
                bind_fxml(label, resource, class, graph, state);
            }
        } else if resource.unsupported {
            state.issues.push(Issue::new(
                "unsupported_java_fxml",
                label,
                "FXML contains a dynamic or unsupported controller, script, include, or handler",
            ));
        }
    }
    for (class, item) in graph.classes.iter().enumerate() {
        let uses = item
            .class
            .methods
            .iter()
            .flat_map(|method| &method.refs.fxml)
            .chain(&item.class.refs.fxml);
        for fxml_use in uses {
            if !matches_annotation(item.facts, &fxml_use.loader_type, |name| {
                name == "javafx.fxml.FXMLLoader"
            }) {
                continue;
            }
            let Some(resource) = &fxml_use.path else {
                state.issues.push(fxml_use.site.issue(
                    "unsupported_java_fxml",
                    item.path,
                    "FXMLLoader.setController requires a proven literal selected resource and controller",
                ));
                continue;
            };
            let relative = PathBuf::from(item.path).with_file_name(resource);
            let exact = project.base().join(relative);
            let mut candidates = fxml.iter().filter(|(_, (path, _))| {
                path == &exact
                    || resource
                        .strip_prefix('/')
                        .is_some_and(|name| path.ends_with(name))
            });
            let chosen = candidates.next();
            if let Some((label, (_, fxml))) = chosen
                && candidates.next().is_none()
                && fxml.controller.is_none()
            {
                bind_fxml(label, fxml, class, graph, state);
            } else {
                state.issues.push(Issue::new(
                    "unresolved_java_fxml_resource",
                    item.path,
                    format!("cannot identify selected programmatic FXML resource {resource}"),
                ));
            }
        }
    }
    for (path, label, snapshot) in reads {
        if !fs::symlink_metadata(path)
            .is_ok_and(|metadata| metadata.is_file() && Stamp::new(&metadata) == snapshot)
        {
            state.issues.push(Issue::new(
                "source_changed",
                &label,
                "Java metadata changed after its snapshot was read",
            ));
        }
    }
}

pub(super) fn analyze(project: &Project, files: &BTreeMap<String, Facts>, analysis: &mut Analysis) {
    if files.is_empty() {
        return;
    }
    let mut state = State::default();
    let graph = Graph::new(files, &mut state.issues);
    resources(project, &graph, &mut state);
    for (path, facts) in files {
        for provider in &facts.providers {
            if let Some(class) = graph.resource_type(path, provider, &mut state.issues) {
                state.construct(class, &graph, "java_service_provider");
            }
        }
    }
    for (index, item) in graph.classes.iter().enumerate() {
        let class = item.class;
        let has_main = class.methods.iter().any(|method| method.entry);
        if class.annotations.iter().any(|annotation| {
            matches_annotation(item.facts, annotation, framework_class_annotation)
        }) {
            state.construct(index, &graph, "java_framework_entrypoint");
        } else if class.public {
            state.class(index, "java_public_api");
        }
        for (method, definition) in class.methods.iter().enumerate() {
            let reason = if definition.entry {
                Some("java_main_entrypoint")
            } else if definition.abstract_method {
                Some("java_abstract_contract")
            } else if serialization_hook(definition, item.facts)
                || definition.annotations.iter().any(|annotation| {
                    matches_annotation(item.facts, annotation, framework_method_annotation)
                })
            {
                Some("java_framework_entrypoint")
            } else if definition.public && !(has_main && definition.static_method) {
                Some("java_public_api")
            } else {
                None
            };
            if let Some(reason) = reason {
                state.method(index, method, reason);
            }
        }
    }
    let mut traversed = 0;
    while let Some((index, method)) = state.pending.pop_front() {
        traversed += 1;
        if traversed > 32_768 {
            state.issues.push(Issue::new(
                "analysis_limit",
                ".",
                "Java reachability exceeds 32768 definitions",
            ));
            break;
        }
        let item = graph.classes[index];
        let refs = method.map_or(&item.class.refs, |method| &item.class.methods[method].refs);
        if method.is_none() {
            for &parent in &graph.parents[index] {
                state.class(parent, "java_supertype");
            }
        }
        graph.references(index, refs, &mut state);
        if state.call_resolution_steps > MAX_CALL_RESOLUTION_STEPS {
            break;
        }
    }
    if !state.issues.is_empty() {
        analysis.issues.extend(state.issues);
        return;
    }
    for (index, item) in graph.classes.iter().enumerate() {
        let class = item.class;
        let full_name = qualified(&item.facts.package, &class.name);
        record(
            analysis,
            item.path,
            &class.name,
            &full_name,
            Site {
                line: class.line,
                column: class.column,
            },
            Category::Class,
            state.classes.get(&index).copied(),
        );
        let mut overloads = BTreeMap::new();
        for (method, definition) in class.methods.iter().enumerate() {
            let reason = state.methods.get(&(index, method)).copied();
            let group = overloads
                .entry(&definition.name)
                .or_insert((definition, None));
            group.1 = group.1.or(reason);
        }
        for (name, (definition, reason)) in overloads {
            if name == CONSTRUCTOR {
                continue;
            }
            record(
                analysis,
                item.path,
                &format!("{}.{name}", class.name),
                &format!("{full_name}.{name}"),
                Site {
                    line: definition.line,
                    column: definition.column,
                },
                Category::Function,
                reason,
            );
        }
    }
}

fn record(
    analysis: &mut Analysis,
    file: &str,
    name: &str,
    full_name: &str,
    site: Site,
    category: Category,
    reason: Option<&'static str>,
) {
    if let Some(kind) = reason {
        analysis.evidence.push(Evidence {
            file: file.into(),
            line: site.line,
            full_name: full_name.into(),
            kind,
            certainty: if kind == "java_literal_reflection" {
                "possible"
            } else {
                "definite"
            },
        });
    } else {
        let (rule_id, reason) = if category == Category::Class {
            ("SKY-U004", "java_unreachable_class")
        } else {
            ("SKY-U001", "java_unreachable_method")
        };
        analysis.findings.push(Finding {
            category,
            name: name.into(),
            full_name: full_name.into(),
            file: file.into(),
            line: site.line,
            column: site.column,
            confidence: 90,
            rule_id,
            reason,
        });
    }
}
