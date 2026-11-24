use std::collections::{BTreeMap, BTreeSet};
use std::fs;

use serde::{Deserialize, Serialize};
use skarmory_python::Location;
use tree_sitter::{Node, Parser};

use crate::Issue;
use crate::preview_source::{Project, Stamp, read_snapshot};

use super::{Analysis, Category, Evidence, Finding};

const MAX_AST_DEPTH: usize = 128;
const MAX_GRAPH_EDGES: usize = 262_144;

#[derive(Debug, Default, Deserialize, Serialize)]
pub(super) struct Facts {
    package: String,
    imports: Vec<Import>,
    definitions: Vec<Definition>,
    initializers: Vec<Reference>,
    declared_types: BTreeMap<String, Receiver>,
    variables: BTreeMap<String, Receiver>,
    linknames: BTreeSet<String>,
}

#[derive(Debug, Deserialize, Serialize)]
struct Import {
    alias: Option<String>,
    path: String,
    line: usize,
}

#[derive(Debug, Deserialize, Serialize)]
struct Definition {
    name: String,
    receiver: Option<String>,
    variable: bool,
    location: Site,
    root: bool,
    references: Vec<Reference>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
struct Site {
    line: usize,
    column: usize,
}

impl Site {
    fn of(node: Node<'_>) -> Self {
        let position = node.start_position();
        Self {
            line: position.row + 1,
            column: position.column + 1,
        }
    }

    fn location(self) -> Location {
        Location {
            line: self.line,
            column: self.column,
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
enum Reference {
    Name(String),
    Selector {
        receiver: Receiver,
        name: String,
        location: Site,
    },
    InterfaceMethod(String),
}

#[derive(Debug, Clone, Deserialize, Serialize)]
enum Receiver {
    LocalType(String),
    Unbound(String),
    Interface,
    Unknown,
}

pub(super) fn extract(path: &str, source: &str) -> Result<Facts, Issue> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_go::LANGUAGE.into())
        .map_err(|error| Issue::new("go_grammar", path, error))?;
    let tree = parser
        .parse(source, None)
        .ok_or_else(|| Issue::new("go_parse_error", path, "the parser returned no tree"))?;
    let root = tree.root_node();
    check_tree(path, root)?;
    let package = children(root)
        .find(|node| node.kind() == "package_clause")
        .and_then(|node| node.named_child(0))
        .map(|node| text(node, source))
        .ok_or_else(|| Issue::new("go_parse_error", path, "missing Go package clause"))?;
    let mut facts = Facts {
        package: package.to_owned(),
        ..Facts::default()
    };
    let prefix = source
        .get(..source.floor_char_boundary(4096))
        .unwrap_or(source);
    let prefix = prefix.to_ascii_lowercase();
    let generated = path.ends_with(".pb.go")
        || (prefix.contains("code generated") && prefix.contains("do not edit"));
    let mut external_exports = BTreeSet::new();
    let mut declarations = Vec::new();
    for node in children(root) {
        match node.kind() {
            "comment" => {
                let mut words = text(node, source).split_whitespace();
                match (words.next(), words.next()) {
                    (Some("//go:linkname"), Some(name)) => {
                        facts.linknames.insert(name.to_owned());
                    }
                    (Some("//export"), Some(name)) => {
                        external_exports.insert(name);
                    }
                    _ => {}
                }
            }
            "package_clause" => {}
            "import_declaration" => {
                for spec in specs(node, "import_spec_list") {
                    let literal = field(spec, "path", path)?;
                    let literal = text(literal, source);
                    let Some(import_path) = literal
                        .strip_prefix('"')
                        .and_then(|name| name.strip_suffix('"'))
                        .or_else(|| {
                            literal
                                .strip_prefix('`')
                                .and_then(|name| name.strip_suffix('`'))
                        })
                        .filter(|name| !name.contains(['\\', '\n', '\r']))
                    else {
                        return Err(unsupported(
                            path,
                            spec,
                            "escaped or multiline Go import path",
                        ));
                    };
                    facts.imports.push(Import {
                        alias: spec
                            .child_by_field_name("name")
                            .map(|name| text(name, source).to_owned()),
                        path: import_path.to_owned(),
                        line: Site::of(spec).line,
                    });
                }
            }
            _ => declarations.push(node),
        }
    }

    for node in declarations {
        let mut collector = Collector::new(path, source);
        match node.kind() {
            "function_declaration" | "method_declaration" => {
                let name = field(node, "name", path)?;
                let name_text = text(name, source);
                if name_text == "_" {
                    continue;
                }
                let body = node.child_by_field_name("body").ok_or_else(|| {
                    unsupported(path, node, "Go function has an external implementation")
                })?;
                let mut receiver = None;
                if let Some(parameters) = node.child_by_field_name("receiver") {
                    let parameter = parameters.named_child(0).ok_or_else(|| {
                        unsupported(path, parameters, "Go method has an unsupported receiver")
                    })?;
                    let receiver_node = field(parameter, "type", path)?;
                    let Receiver::LocalType(owner) = type_receiver(receiver_node, source) else {
                        return Err(unsupported(
                            path,
                            receiver_node,
                            "Go method has an unresolved receiver",
                        ));
                    };
                    receiver = Some(owner);
                    collector.bind_parameters(parameters);
                }
                collector.callable_signature(node)?;
                collector.walk_children(body)?;
                let root = generated
                    || (receiver.is_none()
                        && (name_text == "init" || (package == "main" && name_text == "main")))
                    || (package != "main" && is_exported(name_text))
                    || (path.ends_with("_test.go")
                        && receiver.is_none()
                        && is_test_entry(name_text))
                    || (receiver.is_none() && external_exports.contains(name_text));
                facts.definitions.push(Definition {
                    name: name_text.to_owned(),
                    receiver,
                    variable: false,
                    location: Site::of(name),
                    root,
                    references: collector.references,
                });
            }
            "type_declaration" => {
                for spec in specs(node, "type_spec_list") {
                    if spec.kind() == "type_alias" {
                        return Err(unsupported(
                            path,
                            spec,
                            "Go type alias resolution is unsupported",
                        ));
                    }
                    let name = text(field(spec, "name", path)?, source);
                    let typ = field(spec, "type", path)?;
                    if typ.kind() == "struct_type" && has_embedded_field(typ) {
                        return Err(unsupported(
                            path,
                            typ,
                            "Go embedded fields can promote methods",
                        ));
                    }
                    let receiver = if typ.kind() == "interface_type" {
                        Receiver::Interface
                    } else {
                        Receiver::LocalType(name.to_owned())
                    };
                    if name != "_"
                        && facts
                            .declared_types
                            .insert(name.to_owned(), receiver)
                            .is_some()
                    {
                        return Err(unsupported(path, spec, "duplicate Go type declaration"));
                    }
                }
                collector.walk(node)?;
                facts.initializers.extend(collector.references);
            }
            "const_declaration" | "var_declaration" => {
                for spec in specs(node, "var_spec_list") {
                    if spec.kind() != "var_spec" && spec.kind() != "const_spec" {
                        continue;
                    }
                    let names = named_fields(spec, "name");
                    for ((name, receiver), site) in
                        collector.variable_bindings(spec).into_iter().zip(names)
                    {
                        if name == "_" {
                            continue;
                        }
                        if facts.variables.insert(name.clone(), receiver).is_some() {
                            return Err(unsupported(path, spec, "duplicate Go value declaration"));
                        }
                        if spec.kind() == "var_spec" {
                            let root = generated || (package != "main" && is_exported(&name));
                            facts.definitions.push(Definition {
                                name,
                                receiver: None,
                                variable: true,
                                location: Site::of(site),
                                root,
                                references: Vec::new(),
                            });
                        }
                    }
                    // Every package initializer runs, including the initializer
                    // of a variable that no reachable source reads.
                    for part in ["type", "value"] {
                        if let Some(value) = spec.child_by_field_name(part) {
                            collector.walk(value)?;
                        }
                    }
                }
                facts.initializers.extend(collector.references);
            }
            _ => return Err(unsupported(path, node, "unexpected Go top-level syntax")),
        }
    }
    Ok(facts)
}

fn check_tree(path: &str, root: Node<'_>) -> Result<(), Issue> {
    let mut pending = vec![(root, 0)];
    while let Some((node, depth)) = pending.pop() {
        if node.is_error() || node.is_missing() {
            return Err(Issue::new("go_parse_error", path, "invalid Go syntax")
                .at(Site::of(node).location()));
        }
        if depth == MAX_AST_DEPTH {
            return Err(Issue::new(
                "analysis_limit",
                path,
                "Go syntax nesting exceeded the analysis limit",
            )
            .at(Site::of(node).location()));
        }
        let mut cursor = node.walk();
        pending.extend(node.children(&mut cursor).map(|child| (child, depth + 1)));
    }
    Ok(())
}

fn children(node: Node<'_>) -> impl Iterator<Item = Node<'_>> {
    // Indexed access also permits taking an iterator across scoped visitor calls.
    (0..node.named_child_count()).filter_map(move |index| node.named_child(index as u32))
}

fn specs<'t>(node: Node<'t>, list: &str) -> Vec<Node<'t>> {
    children(node)
        .filter(|child| child.kind() != "comment")
        .flat_map(|child| {
            if child.kind() == list {
                children(child)
                    .filter(|item| item.kind() != "comment")
                    .collect()
            } else {
                vec![child]
            }
        })
        .collect()
}

fn text<'s>(node: Node<'_>, source: &'s str) -> &'s str {
    &source[node.byte_range()]
}

fn field<'t>(node: Node<'t>, name: &str, path: &str) -> Result<Node<'t>, Issue> {
    node.child_by_field_name(name)
        .ok_or_else(|| unsupported(path, node, &format!("missing Go syntax field {name}")))
}

fn unsupported(path: &str, node: Node<'_>, reason: &str) -> Issue {
    Issue::new("unsupported_go_syntax", path, reason).at(Site::of(node).location())
}

fn named_fields<'t>(node: Node<'t>, name: &str) -> Vec<Node<'t>> {
    let mut cursor = node.walk();
    node.children_by_field_name(name, &mut cursor).collect()
}

fn is_exported(name: &str) -> bool {
    name.chars().next().is_some_and(char::is_uppercase)
}

fn is_test_entry(name: &str) -> bool {
    ["Test", "Benchmark", "Fuzz", "Example"]
        .iter()
        .filter_map(|prefix| name.strip_prefix(prefix))
        .any(|suffix| suffix.chars().next().is_none_or(|ch| !ch.is_lowercase()))
}

fn has_embedded_field(node: Node<'_>) -> bool {
    children(node).flat_map(children).any(|field| {
        field.kind() == "field_declaration" && field.child_by_field_name("name").is_none()
    })
}

fn type_receiver(node: Node<'_>, source: &str) -> Receiver {
    match node.kind() {
        "type_identifier" => Receiver::LocalType(text(node, source).to_owned()),
        "generic_type" => node
            .child_by_field_name("type")
            .map_or(Receiver::Unknown, |inner| type_receiver(inner, source)),
        "pointer_type" | "parenthesized_type" => node
            .named_child(0)
            .map_or(Receiver::Unknown, |inner| type_receiver(inner, source)),
        "interface_type" => Receiver::Interface,
        _ => Receiver::Unknown,
    }
}

struct Collector<'s> {
    path: &'s str,
    source: &'s str,
    scopes: Vec<BTreeMap<String, Receiver>>,
    references: Vec<Reference>,
}

impl<'s> Collector<'s> {
    fn new(path: &'s str, source: &'s str) -> Self {
        Self {
            path,
            source,
            scopes: vec![BTreeMap::new()],
            references: Vec::new(),
        }
    }

    fn bind(&mut self, name: String, receiver: Receiver) {
        if name != "_" {
            self.scopes
                .last_mut()
                .expect("lexical scope")
                .entry(name)
                .or_insert(receiver);
        }
    }

    fn binding(&self, name: &str) -> Option<&Receiver> {
        self.scopes.iter().rev().find_map(|scope| scope.get(name))
    }

    fn bind_parameters(&mut self, parameters: Node<'_>) {
        for parameter in children(parameters) {
            let receiver = parameter
                .child_by_field_name("type")
                .map_or(Receiver::Unknown, |typ| type_receiver(typ, self.source));
            let receiver = if parameter.kind() == "variadic_parameter_declaration" {
                Receiver::Unknown
            } else {
                receiver
            };
            for name in named_fields(parameter, "name") {
                self.bind(text(name, self.source).to_owned(), receiver.clone());
            }
        }
    }

    fn callable_signature(&mut self, node: Node<'_>) -> Result<(), Issue> {
        for field in ["parameters", "result", "type_parameters"] {
            if let Some(part) = node.child_by_field_name(field) {
                if matches!(part.kind(), "parameter_list" | "type_parameter_list") {
                    self.bind_parameters(part);
                }
                self.walk(part)?;
            }
        }
        Ok(())
    }

    fn expr_receiver(&self, node: Node<'_>) -> Receiver {
        match node.kind() {
            "composite_literal" => {
                node.child_by_field_name("type")
                    .map_or(Receiver::Unknown, |typ| {
                        let receiver = type_receiver(typ, self.source);
                        if let Receiver::LocalType(name) = &receiver
                            && self.binding(name).is_some()
                        {
                            return Receiver::Unknown;
                        }
                        receiver
                    })
            }
            "identifier" => {
                let name = text(node, self.source);
                self.binding(name)
                    .cloned()
                    .unwrap_or_else(|| Receiver::Unbound(name.to_owned()))
            }
            "parenthesized_expression" => node
                .named_child(0)
                .map_or(Receiver::Unknown, |inner| self.expr_receiver(inner)),
            "unary_expression" => node
                .child_by_field_name("operator")
                .filter(|op| matches!(op.kind(), "&" | "*"))
                .and_then(|_| node.child_by_field_name("operand"))
                .map_or(Receiver::Unknown, |inner| self.expr_receiver(inner)),
            "type_assertion_expression" => node
                .child_by_field_name("type")
                .map_or(Receiver::Unknown, |typ| type_receiver(typ, self.source)),
            _ => Receiver::Unknown,
        }
    }

    fn variable_bindings(&self, node: Node<'_>) -> Vec<(String, Receiver)> {
        let typ = node.child_by_field_name("type");
        let values = node.child_by_field_name("value");
        let names = named_fields(node, "name");
        let single_value_per_name =
            values.is_some_and(|list| list.named_child_count() == names.len());
        names
            .into_iter()
            .enumerate()
            .map(|(index, name)| {
                let receiver = if let Some(typ) = typ {
                    type_receiver(typ, self.source)
                } else if single_value_per_name {
                    values
                        .and_then(|list| list.named_child(index as u32))
                        .map_or(Receiver::Unknown, |value| self.expr_receiver(value))
                } else {
                    Receiver::Unknown
                };
                (text(name, self.source).to_owned(), receiver)
            })
            .collect()
    }

    fn scoped(&mut self, walk: impl FnOnce(&mut Self) -> Result<(), Issue>) -> Result<(), Issue> {
        self.scopes.push(BTreeMap::new());
        let result = walk(self);
        self.scopes.pop();
        result
    }

    fn walk_children(&mut self, node: Node<'_>) -> Result<(), Issue> {
        for child in children(node) {
            self.walk(child)?;
        }
        Ok(())
    }

    fn walk(&mut self, node: Node<'_>) -> Result<(), Issue> {
        match node.kind() {
            "identifier" => {
                let name = text(node, self.source);
                if name != "_" && self.binding(name).is_none() {
                    self.references.push(Reference::Name(name.to_owned()));
                }
            }
            "selector_expression" => {
                let operand = field(node, "operand", self.path)?;
                let name = field(node, "field", self.path)?;
                self.references.push(Reference::Selector {
                    receiver: self.expr_receiver(operand),
                    name: text(name, self.source).to_owned(),
                    location: Site::of(name),
                });
                self.walk(operand)?;
            }
            "block"
            | "if_statement"
            | "for_statement"
            | "expression_switch_statement"
            | "expression_case"
            | "default_case"
            | "communication_case" => {
                self.scoped(|collector| collector.walk_children(node))?;
            }
            "type_switch_statement" => {
                return Err(unsupported(
                    self.path,
                    node,
                    "Go type-switch binding and dispatch are unsupported",
                ));
            }
            "func_literal" => {
                self.scoped(|collector| {
                    collector.callable_signature(node)?;
                    collector.walk_children(field(node, "body", collector.path)?)
                })?;
            }
            "short_var_declaration" | "range_clause" | "receive_statement" => {
                let right = field(node, "right", self.path)?;
                let declares = node.kind() == "short_var_declaration" || {
                    let mut cursor = node.walk();
                    node.children(&mut cursor).any(|child| child.kind() == ":=")
                };
                let left = node.child_by_field_name("left");
                self.walk(right)?;
                if let Some(left) = left {
                    if declares {
                        let values = (node.kind() == "short_var_declaration"
                            && right.named_child_count() == left.named_child_count())
                        .then_some(right);
                        let new_bindings: Vec<_> = children(left)
                            .enumerate()
                            .map(|(index, name)| {
                                let receiver = values
                                    .and_then(|list| list.named_child(index as u32))
                                    .map_or(Receiver::Unknown, |value| self.expr_receiver(value));
                                (text(name, self.source).to_owned(), receiver)
                            })
                            .collect();
                        for (name, receiver) in new_bindings {
                            self.bind(name, receiver);
                        }
                    } else {
                        self.walk(left)?;
                    }
                }
            }
            "var_spec" | "const_spec" => {
                let bindings = self.variable_bindings(node);
                for field in ["type", "value"] {
                    if let Some(part) = node.child_by_field_name(field) {
                        self.walk(part)?;
                    }
                }
                for (name, receiver) in bindings {
                    self.bind(name, receiver);
                }
            }
            "type_spec" | "type_alias" => {
                let name = text(field(node, "name", self.path)?, self.source);
                let typ = field(node, "type", self.path)?;
                let receiver = if typ.kind() == "interface_type" {
                    Receiver::Interface
                } else {
                    Receiver::Unknown
                };
                self.bind(name.to_owned(), receiver);
                self.walk(typ)?;
            }
            "method_elem" => {
                if let Some(method) = node.child_by_field_name("name") {
                    self.references.push(Reference::InterfaceMethod(
                        text(method, self.source).to_owned(),
                    ));
                }
                self.walk_children(node)?;
            }
            "parameter_declaration"
            | "variadic_parameter_declaration"
            | "type_parameter_declaration"
            | "field_declaration"
            | "qualified_type" => {
                // Parameter and field names are declarations, not identifier uses.
                if let Some(typ) = node.child_by_field_name("type") {
                    self.walk(typ)?;
                }
            }
            "composite_literal" => {
                let typ = field(node, "type", self.path)?;
                self.walk(typ)?;
                let body = field(node, "body", self.path)?;
                self.walk_literal(body, is_named_struct_candidate(typ))?;
            }
            "literal_value" => self.walk_literal(node, false)?,
            "comment" | "interpreted_string_literal" | "raw_string_literal" => {}
            _ => self.walk_children(node)?,
        }
        Ok(())
    }

    fn walk_literal(&mut self, node: Node<'_>, named_keys: bool) -> Result<(), Issue> {
        for child in children(node) {
            if child.kind() == "keyed_element" && named_keys {
                // A struct literal key is a field declaration lookup.
                self.walk(field(child, "value", self.path)?)?;
            } else {
                self.walk(child)?;
            }
        }
        Ok(())
    }
}

fn is_named_struct_candidate(node: Node<'_>) -> bool {
    matches!(
        node.kind(),
        "struct_type" | "type_identifier" | "qualified_type" | "generic_type"
    )
}

#[derive(Default)]
struct Package<'f> {
    name: &'f str,
    values: BTreeMap<&'f str, usize>,
    methods: BTreeMap<(&'f str, &'f str), usize>,
    methods_by_name: BTreeMap<&'f str, Vec<usize>>,
    types: BTreeMap<&'f str, &'f Receiver>,
    variables: BTreeMap<&'f str, &'f Receiver>,
    names: BTreeSet<&'f str>,
}

struct Entry<'f> {
    file: &'f str,
    dir: &'f str,
    definition: &'f Definition,
}

#[derive(Clone, Copy)]
enum Target<'p> {
    Local(&'p str),
    Standard,
}

pub(super) fn analyze(project: &Project, files: &BTreeMap<String, Facts>, analysis: &mut Analysis) {
    if files.is_empty() {
        return;
    }
    let modules = match module_context(project) {
        Ok(modules) => modules,
        Err(issue) => {
            analysis.issues.push(issue);
            return;
        }
    };
    if let Err(issue) = analyze_module(files, &modules.path, analysis) {
        analysis.issues.push(issue);
    }
    if !fs::symlink_metadata(project.base().join("go.mod"))
        .is_ok_and(|metadata| metadata.is_file() && Stamp::new(&metadata) == modules.stamp)
    {
        analysis.issues.push(Issue::new(
            "source_changed",
            "go.mod",
            "Go module manifest changed after its snapshot was read",
        ));
    }
    if !analysis.issues.is_empty() {
        analysis.findings.clear();
        analysis.evidence.clear();
        analysis.uncertainties.clear();
    }
}

struct ModuleContext {
    path: String,
    stamp: Stamp,
}

fn module_context(project: &Project) -> Result<ModuleContext, Issue> {
    let manifest = project.base().join("go.mod");
    for path in project.go_context() {
        if path != manifest {
            return Err(Issue::new(
                "unsupported_go_project_context",
                &project.label(&path),
                "Go workspaces, nested modules, and assembly sources require additional reachability analysis",
            ));
        }
    }
    if fs::symlink_metadata(&manifest)
        .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
    {
        return Err(Issue::new(
            "unsupported_go_project_context",
            "go.mod",
            "Go reachability requires a root go.mod",
        ));
    }
    let (bytes, stamp) = read_snapshot(&manifest, "go.mod", None)?;
    let source = std::str::from_utf8(&bytes)
        .map_err(|error| Issue::new("unsupported_go_project_context", "go.mod", error))?;
    let mut path = None;
    for line in source.lines() {
        let fields: Vec<_> = line
            .split_once("//")
            .map_or(line, |(line, _)| line)
            .split_whitespace()
            .collect();
        match fields.as_slice() {
            [] | ["go" | "toolchain", _] => {}
            ["module", module] if path.is_none() && !module.contains(['"', '`', '\\']) => {
                path = Some((*module).to_owned());
            }
            _ => {
                return Err(Issue::new(
                    "unsupported_go_project_context",
                    "go.mod",
                    "expected one Go module with no external requirements or replacement directives",
                ));
            }
        }
    }
    let path = path.ok_or_else(|| {
        Issue::new(
            "unsupported_go_project_context",
            "go.mod",
            "missing Go module path",
        )
    })?;
    Ok(ModuleContext { path, stamp })
}

fn directory(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(parent, _)| parent)
}

fn insert_name(package: &Package<'_>, name: &str, file: &str) -> Issue {
    Issue::new(
        "ambiguous_go_definition",
        file,
        format!(
            "Go package {:?} declares {name:?} more than once",
            package.name
        ),
    )
}

fn analyze_module(
    files: &BTreeMap<String, Facts>,
    module: &str,
    analysis: &mut Analysis,
) -> Result<(), Issue> {
    let mut packages = BTreeMap::<_, Package<'_>>::new();
    let mut entries = Vec::new();
    let mut roots = Vec::new();
    let mut methods = BTreeMap::<_, Vec<usize>>::new();
    for (path, file) in files {
        let dir = directory(path);
        let package = packages.entry(dir).or_insert_with(|| Package {
            name: &file.package,
            ..Package::default()
        });
        if package.name != file.package {
            return Err(Issue::new(
                "unsupported_go_project_context",
                path,
                "Go directory has several package names",
            ));
        }
        for (name, typ) in &file.declared_types {
            if !package.names.insert(name) {
                return Err(insert_name(package, name, path));
            }
            package.types.insert(name, typ);
        }
        for (name, typ) in &file.variables {
            if !package.names.insert(name) {
                return Err(insert_name(package, name, path));
            }
            package.variables.insert(name, typ);
        }
        for definition in &file.definitions {
            let index = entries.len();
            let name = definition.name.as_str();
            if let Some(receiver) = &definition.receiver {
                if package.methods.insert((receiver, name), index).is_some() {
                    return Err(insert_name(package, &format!("{receiver}.{name}"), path));
                }
                methods.entry(name).or_default().push(index);
                package.methods_by_name.entry(name).or_default().push(index);
            } else if definition.variable {
                package.values.insert(name, index);
            } else if name != "init" {
                if !package.names.insert(name) {
                    return Err(insert_name(package, name, path));
                }
                package.values.insert(name, index);
            }
            entries.push(Entry {
                file: path,
                dir,
                definition,
            });
            if definition.root {
                roots.push((index, true));
            }
        }
    }
    let mut edges = vec![Vec::new(); entries.len()];
    let mut edge_count = 0;
    let mut next_definition = 0;
    let mut packages_with_standard_roots = BTreeSet::new();
    for (path, facts) in files {
        let dir = directory(path);
        let package = &packages[dir];
        let aliases = import_targets(path, facts, &packages, module)?;
        for name in &facts.linknames {
            if let Some(index) = package.values.get(name.as_str()) {
                roots.push((*index, true));
            }
        }
        let resolver = Resolver {
            aliases: &aliases,
            packages: &packages,
            package,
            methods: &methods,
        };
        if aliases
            .values()
            .any(|target| matches!(target, Target::Standard))
            && packages_with_standard_roots.insert(dir)
        {
            // Standard-library callbacks can invoke exported methods through
            // interfaces even where there is no source-level method selector.
            roots.extend(
                package.methods.iter().filter_map(|((_, method), &index)| {
                    is_exported(method).then_some((index, false))
                }),
            );
        }
        resolver.resolve(&facts.initializers, &mut roots, &mut edge_count, path)?;
        for definition in &facts.definitions {
            if let Some(owner) = &definition.receiver
                && !package.types.contains_key(owner.as_str())
            {
                return Err(Issue::new(
                    "unresolved_go_receiver",
                    path,
                    format!("Go receiver type {owner:?} is outside the selected package"),
                )
                .at(definition.location.location()));
            }
            resolver.resolve(
                &definition.references,
                &mut edges[next_definition],
                &mut edge_count,
                path,
            )?;
            next_definition += 1;
        }
    }

    let mut reached = vec![0u8; entries.len()];
    while let Some((index, definite)) = roots.pop() {
        let strength = if definite { 2 } else { 1 };
        if reached[index] < strength {
            reached[index] = strength;
            roots.extend(
                edges[index]
                    .iter()
                    .map(|&(index, edge)| (index, definite && edge)),
            );
        }
    }
    for (index, entry) in entries.iter().enumerate() {
        let definition = entry.definition;
        let name = definition.receiver.as_ref().map_or_else(
            || definition.name.clone(),
            |owner| format!("{owner}.{}", definition.name),
        );
        let full_name = if entry.dir.is_empty() {
            name.clone()
        } else {
            format!("{}.{name}", entry.dir)
        };
        let line = definition.location.line;
        if reached[index] == 0 {
            let (category, rule_id, reason) = if definition.variable {
                (Category::Variable, "SKY-U003", "unreachable_go_variable")
            } else {
                (Category::Function, "SKY-U001", "unreachable_go_function")
            };
            analysis.findings.push(Finding {
                category,
                name: definition.name.clone(),
                full_name,
                file: entry.file.to_owned(),
                line,
                column: definition.location.column,
                confidence: 100,
                rule_id,
                reason,
            });
        } else {
            let definite = reached[index] == 2;
            analysis.evidence.push(Evidence {
                file: entry.file.to_owned(),
                line,
                full_name,
                kind: if definition.root {
                    "go_entrypoint"
                } else if definition.variable {
                    "go_reachable_variable"
                } else {
                    "go_reachable_function"
                },
                certainty: if definite { "definite" } else { "possible" },
            });
        }
    }
    Ok(())
}

fn import_targets<'p>(
    path: &str,
    facts: &'p Facts,
    packages: &BTreeMap<&'p str, Package<'p>>,
    module: &str,
) -> Result<BTreeMap<&'p str, Target<'p>>, Issue> {
    let mut result = BTreeMap::new();
    for import in &facts.imports {
        let directory = if import.path == module {
            Some("")
        } else {
            import
                .path
                .strip_prefix(module)
                .and_then(|rest| rest.strip_prefix('/'))
        };
        let (target, default_name) = if let Some(directory) = directory {
            packages
                .get_key_value(directory)
                .map(|(&directory, package)| (Target::Local(directory), package.name))
        } else if matches!(
            import.path.as_str(),
            "fmt" | "net/http" | "testing" | "unsafe"
        ) {
            Some((
                Target::Standard,
                import.path.rsplit('/').next().expect("package basename"),
            ))
        } else {
            None
        }
        .ok_or_else(|| {
            Issue::new(
                "unresolved_go_import",
                path,
                format!("unsupported Go import {:?}", import.path),
            )
            .at(Location {
                line: import.line,
                column: 1,
            })
        })?;
        let alias = import.alias.as_deref().unwrap_or(default_name);
        if alias == "_" {
            continue;
        }
        if alias == "." || result.insert(alias, target).is_some() {
            return Err(Issue::new(
                "ambiguous_go_import",
                path,
                format!("Go import alias {alias:?} does not support unambiguous lookup"),
            )
            .at(Location {
                line: import.line,
                column: 1,
            }));
        }
    }
    Ok(result)
}

struct Resolver<'a, 'f> {
    aliases: &'a BTreeMap<&'f str, Target<'f>>,
    packages: &'a BTreeMap<&'f str, Package<'f>>,
    package: &'a Package<'f>,
    methods: &'a BTreeMap<&'f str, Vec<usize>>,
}

impl Resolver<'_, '_> {
    fn resolve(
        &self,
        references: &[Reference],
        selected: &mut Vec<(usize, bool)>,
        count: &mut usize,
        path: &str,
    ) -> Result<(), Issue> {
        for reference in references {
            let start = selected.len();
            match reference {
                Reference::Name(name) => selected.extend(
                    self.package
                        .values
                        .get(name.as_str())
                        .map(|&index| (index, true)),
                ),
                Reference::InterfaceMethod(name) => self.possible_methods(name, selected),
                Reference::Selector {
                    receiver,
                    name,
                    location,
                } => {
                    self.selector(receiver, name, *location, selected, path)?;
                }
            }
            *count += selected.len() - start;
            if *count > MAX_GRAPH_EDGES {
                return Err(Issue::new(
                    "analysis_limit",
                    path,
                    "Go reference graph exceeded the analysis limit",
                ));
            }
        }
        Ok(())
    }

    fn selector(
        &self,
        receiver: &Receiver,
        name: &str,
        location: Site,
        selected: &mut Vec<(usize, bool)>,
        path: &str,
    ) -> Result<(), Issue> {
        let receiver = match receiver {
            Receiver::Unbound(owner) => {
                if let Some(target) = self.aliases.get(owner.as_str()) {
                    if let Target::Local(directory) = target {
                        let package = &self.packages[directory];
                        if let Some(&index) = package.values.get(name) {
                            selected.push((index, true));
                        } else if !package.names.contains(name) {
                            return Err(Issue::new(
                                "unresolved_go_member",
                                path,
                                format!("Go package {owner:?} has no selected member {name:?}"),
                            )
                            .at(location.location()));
                        }
                    }
                    return Ok(());
                }
                self.package
                    .variables
                    .get(owner.as_str())
                    .or_else(|| self.package.types.get(owner.as_str()))
                    .copied()
                    .unwrap_or(&Receiver::Unknown)
            }
            receiver => receiver,
        };
        if let Receiver::LocalType(owner) = receiver {
            if matches!(
                self.package.types.get(owner.as_str()),
                Some(Receiver::Interface)
            ) {
                self.possible_methods(name, selected);
            } else if let Some(&index) = self.package.methods.get(&(owner.as_str(), name)) {
                selected.push((index, true));
            } else if !self.package.types.contains_key(owner.as_str()) {
                self.possible_methods(name, selected);
            }
        } else {
            self.possible_methods(name, selected);
        }
        Ok(())
    }

    fn possible_methods(&self, name: &str, selected: &mut Vec<(usize, bool)>) {
        let methods = if is_exported(name) {
            self.methods
        } else {
            &self.package.methods_by_name
        };
        selected.extend(
            methods
                .get(name)
                .into_iter()
                .flatten()
                .map(|&index| (index, false)),
        );
    }
}
