//! Project binding resolution and reachability for the Skylos profile.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::ops::RangeInclusive;

use skarmory_python::Location;
use skarmory_python::phase02::{
    Definition, DefinitionKind, Expression, ExpressionKind, Import, Name, Statement, StatementKind,
};

use super::config::Entrypoint;
use super::{Analysis, AnalyzedFile, Category, Evidence, Finding};
use crate::Issue;

const MAX_BINDING_CHAIN: usize = 64;
const MAX_FRAMEWORK_PASSES: usize = 64;
const END: Location = Location {
    line: usize::MAX,
    column: usize::MAX,
};

type SymbolId = usize;
type ScopeId = usize;
type Object = (usize, usize, usize);

#[derive(Clone)]
enum Value {
    Callable(SymbolId),
    Instance(SymbolId),
    Coroutine(SymbolId),
    Module(usize),
    Provider(String),
    Object(Object, ObjectKind),
    Method(Object, String),
    Executor,
    Submit,
    Process(Box<Value>),
    Start(Box<Value>),
    Text(String),
    Bool(bool),
    Sequence(Vec<Value>),
    Mapping(Vec<Value>),
    Scalar,
    Unknown,
}

#[derive(Clone, Copy)]
enum ObjectKind {
    App,
    Router,
}

struct Scope<'a> {
    file: usize,
    parent: Option<ScopeId>,
    owner: Option<SymbolId>,
    body: &'a [Statement],
    names: BTreeMap<&'a str, Vec<SymbolId>>,
    edges: Vec<Edge>,
    activated_apps: Vec<Object>,
    mounts: Vec<(Object, Object)>,
    routes: Vec<(Object, SymbolId)>,
}

enum Binding<'a> {
    Definition(&'a Definition, ScopeId),
    Import(&'a Import),
    Variable(Option<&'a Expression>, Location),
    Parameter,
}

struct Symbol<'a> {
    scope: ScopeId,
    name: &'a Name,
    binding: Binding<'a>,
}

#[derive(Clone, Copy)]
struct Edge {
    target: SymbolId,
    kind: &'static str,
    certainty: &'static str,
}

struct DirectArguments {
    caller: ScopeId,
    bounded_callbacks: BTreeSet<String>,
}

struct ParameterCall {
    scope: ScopeId,
    parameter: SymbolId,
    at: Location,
}

pub(super) fn analyze(files: &[AnalyzedFile], entrypoints: &[Entrypoint]) -> Analysis {
    let mut graph = Graph {
        files,
        modules: BTreeMap::new(),
        scopes: Vec::new(),
        symbols: Vec::new(),
        by_location: BTreeMap::new(),
        read: BTreeSet::new(),
        direct: BTreeSet::new(),
        conditional: BTreeMap::new(),
        issues: Vec::new(),
        scope_issues: Vec::new(),
        uncertainties: Vec::new(),
        fixtures: Vec::new(),
        called_tests: Vec::new(),
        direct_arguments: BTreeMap::new(),
        parameter_calls: Vec::new(),
    };
    for (file, source) in files.iter().enumerate() {
        graph.push_scope(file, None, None, &source.facts.body);
    }
    for (file, source) in files.iter().enumerate() {
        if graph.modules.insert(source.module.as_str(), file).is_some() {
            graph.issue(
                file,
                Location { line: 1, column: 1 },
                "ambiguous_module",
                "multiple selected files have the same module name",
            );
        }
        graph.index(file, &source.facts.body);
    }
    for scope in 0..graph.scopes.len() {
        let body = graph.scopes[scope].body;
        graph.suite(scope, body);
    }
    let mut roots = graph.external_roots(entrypoints);
    roots.extend(graph.test_edges());
    graph.finish(roots)
}

struct Graph<'a> {
    files: &'a [AnalyzedFile],
    modules: BTreeMap<&'a str, usize>,
    scopes: Vec<Scope<'a>>,
    symbols: Vec<Symbol<'a>>,
    by_location: BTreeMap<(ScopeId, usize, usize), SymbolId>,
    read: BTreeSet<SymbolId>,
    direct: BTreeSet<SymbolId>,
    conditional: BTreeMap<SymbolId, Vec<RangeInclusive<[usize; 2]>>>,
    issues: Vec<Issue>,
    scope_issues: Vec<(ScopeId, Issue)>,
    uncertainties: Vec<Issue>,
    fixtures: Vec<SymbolId>,
    called_tests: Vec<SymbolId>,
    direct_arguments: BTreeMap<SymbolId, Vec<DirectArguments>>,
    parameter_calls: Vec<ParameterCall>,
}

impl<'a> Graph<'a> {
    fn push_scope(
        &mut self,
        file: usize,
        parent: Option<ScopeId>,
        owner: Option<SymbolId>,
        body: &'a [Statement],
    ) -> ScopeId {
        let index = self.scopes.len();
        self.scopes.push(Scope {
            file,
            parent,
            owner,
            body,
            names: BTreeMap::new(),
            edges: Vec::new(),
            activated_apps: Vec::new(),
            mounts: Vec::new(),
            routes: Vec::new(),
        });
        index
    }

    fn bind(&mut self, scope: ScopeId, name: &'a Name, binding: Binding<'a>) -> SymbolId {
        let id = self.symbols.len();
        self.symbols.push(Symbol {
            scope,
            name,
            binding,
        });
        self.scopes[scope]
            .names
            .entry(&name.text)
            .or_default()
            .push(id);
        self.by_location
            .insert((scope, name.at.line, name.at.column), id);
        id
    }

    fn index(&mut self, scope: ScopeId, body: &'a [Statement]) {
        for statement in body {
            match &statement.kind {
                StatementKind::Define(definition) => {
                    let file = self.scopes[scope].file;
                    let id = self.symbols.len();
                    let child = self.push_scope(file, Some(scope), Some(id), &definition.body);
                    self.bind(
                        scope,
                        &definition.name,
                        Binding::Definition(definition, child),
                    );
                    for parameter in &definition.parameters {
                        self.bind(child, parameter, Binding::Parameter);
                    }
                    self.index(child, &definition.body);
                }
                StatementKind::Import(import) => {
                    self.bind(scope, &import.local, Binding::Import(import));
                }
                StatementKind::Assign { names, value, .. } => {
                    for name in names {
                        self.bind(scope, name, Binding::Variable(value.as_ref(), statement.at));
                    }
                }
                StatementKind::With { items, body } => {
                    for (expression, name) in items {
                        if let Some(name) = name {
                            self.bind(
                                scope,
                                name,
                                Binding::Variable(Some(expression), statement.at),
                            );
                        }
                    }
                    self.index(scope, body);
                }
                StatementKind::If {
                    test,
                    body,
                    otherwise,
                } => {
                    let first_symbol = self.symbols.len();
                    match static_bool(test) {
                        Some(true) => self.index(scope, body),
                        Some(false) => self.index(scope, otherwise),
                        None => {
                            self.index(scope, body);
                            self.mark_conditional(scope, first_symbol, body);
                            let first_symbol = self.symbols.len();
                            self.index(scope, otherwise);
                            self.mark_conditional(scope, first_symbol, otherwise);
                        }
                    }
                }
                _ => {}
            }
        }
    }

    fn mark_conditional(&mut self, scope: ScopeId, first_symbol: usize, body: &[Statement]) {
        let Some(first) = body.first() else { return };
        let end = suite_end(body);
        for id in first_symbol..self.symbols.len() {
            if self.symbols[id].scope == scope {
                self.conditional
                    .entry(id)
                    .or_default()
                    .push(position(first.at)..=end);
            }
        }
    }

    fn issue(&mut self, file: usize, at: Location, kind: &'static str, message: impl ToString) {
        self.issues
            .push(Issue::new(kind, &self.files[file].path, message).at(at));
    }

    fn scope_issue(&mut self, scope: ScopeId, at: Location, kind: &'static str, message: &str) {
        let file = &self.files[self.scopes[scope].file];
        self.scope_issues
            .push((scope, Issue::new(kind, &file.path, message).at(at)));
    }

    fn uncertain(&mut self, scope: ScopeId, at: Location, message: &str) {
        self.uncertainties.push(
            Issue::new(
                "possible_use",
                &self.files[self.scopes[scope].file].path,
                message,
            )
            .at(at),
        );
    }

    fn local(&self, scope: ScopeId, name: &str, before: Location) -> Option<SymbolId> {
        self.scopes[scope]
            .names
            .get(name)?
            .iter()
            .copied()
            .rev()
            .find(|&id| {
                let at = self.symbols[id].name.at;
                (at.line, at.column) < (before.line, before.column)
            })
    }

    fn lexical(&self, scope: ScopeId, name: &str, before: Location) -> Option<SymbolId> {
        let mut scope = scope;
        let mut before = before;
        let mut child_is_function = false;
        loop {
            let row = &self.scopes[scope];
            let class = row.owner.is_some_and(|id| self.is_class(id));
            if !(class && child_is_function) {
                if let Some(id) = self.local(scope, name, before) {
                    return Some(id);
                }
                if row.owner.is_some() && !class && row.names.contains_key(name) {
                    // A local binding shadows the outer name even before assignment.
                    return None;
                }
            }
            child_is_function = row.owner.is_some() && !class;
            scope = row.parent?;
            before = END;
        }
    }

    fn is_class(&self, id: SymbolId) -> bool {
        matches!(self.symbols[id].binding, Binding::Definition(def, _) if def.kind == DefinitionKind::Class)
    }

    fn full_name(&self, id: SymbolId) -> String {
        let symbol = &self.symbols[id];
        let mut names = vec![symbol.name.text.as_str()];
        let mut scope = symbol.scope;
        while let Some(owner) = self.scopes[scope].owner {
            let entry = &self.symbols[owner];
            names.push(&entry.name.text);
            scope = entry.scope;
        }
        names.push(&self.files[self.scopes[scope].file].module);
        names.reverse();
        names.join(".")
    }

    fn relative_module(&self, file: usize, import: &Import) -> Option<String> {
        if import.level == 0 {
            return Some(import.module.clone());
        }
        let source = &self.files[file];
        let mut parts: Vec<_> = source.module.split('.').collect();
        if !source.is_package {
            parts.pop();
        }
        if import.level as usize > parts.len() {
            return None;
        }
        parts.truncate(parts.len() + 1 - import.level as usize);
        if !import.module.is_empty() {
            parts.push(&import.module);
        }
        Some(parts.join("."))
    }

    fn binding(&mut self, id: SymbolId, before: Location, depth: usize) -> Value {
        let symbol = &self.symbols[id];
        if depth >= MAX_BINDING_CHAIN {
            self.issue(
                self.scopes[symbol.scope].file,
                symbol.name.at,
                "analysis_limit",
                "binding resolution limit exceeded",
            );
            return Value::Unknown;
        }
        self.read.insert(id);
        let scope = symbol.scope;
        let file = self.scopes[scope].file;
        if self.conditional.get(&id).is_some_and(|guards| {
            let position = position(before);
            guards.iter().any(|span| !span.contains(&position))
        }) {
            self.scope_issue(
                scope,
                symbol.name.at,
                "ambiguous_binding",
                "a required binding depends on an unresolved branch",
            );
            return Value::Unknown;
        }
        match symbol.binding {
            Binding::Definition(_, _) => Value::Callable(id),
            Binding::Variable(Some(value), before) => {
                self.identity(scope, value, before, depth + 1)
            }
            Binding::Variable(None, _) => Value::Unknown,
            Binding::Parameter => {
                if symbol.name.text == "self"
                    && let Some(parent) = self.scopes[scope]
                        .parent
                        .and_then(|parent| self.scopes[parent].owner)
                    && self.is_class(parent)
                {
                    return Value::Instance(parent);
                }
                Value::Unknown
            }
            Binding::Import(import) => {
                let Some(module) = self.relative_module(file, import) else {
                    self.issue(
                        file,
                        import.local.at,
                        "unresolved_import",
                        "relative import escapes its selected package",
                    );
                    return Value::Unknown;
                };
                if let Some(member) = import.member.as_deref() {
                    self.module_member(&module, member, depth + 1)
                } else {
                    let visible_module = if import.exposes_top_level {
                        module.split('.').next().unwrap_or(&module)
                    } else {
                        &module
                    };
                    self.modules.get(visible_module).map_or_else(
                        || Value::Provider(visible_module.into()),
                        |file| Value::Module(*file),
                    )
                }
            }
        }
    }

    fn module_member(&mut self, module: &str, member: &str, depth: usize) -> Value {
        let submodule = format!("{module}.{member}");
        if depth >= MAX_BINDING_CHAIN {
            return Value::Unknown;
        }
        if let Some(&file) = self.modules.get(module) {
            if let Some(id) = self.local(file, member, END) {
                return self.binding(id, END, depth + 1);
            }
            if let Some(&file) = self.modules.get(submodule.as_str()) {
                return Value::Module(file);
            }
            if let Some(id) = self.local(file, "__getattr__", END) {
                return self.lazy_member(id, member, depth + 1);
            }
            return Value::Unknown;
        }
        self.modules
            .get(submodule.as_str())
            .map_or(Value::Provider(submodule), |file| Value::Module(*file))
    }

    fn lazy_member(&mut self, id: SymbolId, member: &str, depth: usize) -> Value {
        let Binding::Definition(definition, scope) = self.symbols[id].binding else {
            return Value::Unknown;
        };
        let [parameter] = definition.parameters.as_slice() else {
            return Value::Unknown;
        };
        // Prove literal dispatch only, after ordinary module lookup failed.
        for statement in &definition.body {
            let StatementKind::If {
                test,
                body,
                otherwise,
            } = &statement.kind
            else {
                break;
            };
            if !otherwise.is_empty() {
                break;
            }
            let ExpressionKind::Compare {
                left,
                right,
                equal: true,
                identity: false,
            } = &test.kind
            else {
                break;
            };
            let (ExpressionKind::Name(name), ExpressionKind::Text(text)) =
                (&left.kind, &right.kind)
            else {
                break;
            };
            if name != &parameter.text {
                break;
            }
            if text == member
                && let Some(Statement {
                    kind: StatementKind::Return(Some(value)),
                    at,
                }) = body.last()
                && body[..body.len() - 1]
                    .iter()
                    .all(|stmt| matches!(stmt.kind, StatementKind::Import(_)))
            {
                self.read.insert(id);
                return self.identity(scope, value, *at, depth + 1);
            }
        }
        Value::Unknown
    }

    fn member(&mut self, base: Value, name: &str, depth: usize) -> Value {
        match base {
            Value::Module(file) => self.module_member(&self.files[file].module, name, depth),
            Value::Provider(module) => self.module_member(&module, name, depth),
            Value::Instance(id) | Value::Callable(id) if self.is_class(id) => {
                let Binding::Definition(_, class_scope) = self.symbols[id].binding else {
                    unreachable!()
                };
                self.local(class_scope, name, END)
                    .map_or(Value::Unknown, |id| self.binding(id, END, depth))
            }
            Value::Object(object, _) => Value::Method(object, name.into()),
            Value::Executor if name == "submit" => Value::Submit,
            Value::Process(target) if name == "start" => Value::Start(target),
            _ => Value::Unknown,
        }
    }

    fn identity(
        &mut self,
        scope: ScopeId,
        expression: &Expression,
        before: Location,
        depth: usize,
    ) -> Value {
        if depth >= MAX_BINDING_CHAIN {
            self.issue(
                self.scopes[scope].file,
                expression.at,
                "analysis_limit",
                "expression identity limit exceeded",
            );
            return Value::Unknown;
        }
        let recurse =
            |this: &mut Self, expression| this.identity(scope, expression, before, depth + 1);
        match &expression.kind {
            ExpressionKind::Name(name) => {
                if let Some(id) = self.lexical(scope, name, before) {
                    let before = if self.symbols[id].scope == scope {
                        before
                    } else {
                        END
                    };
                    return self.binding(id, before, depth + 1);
                }
                if name == "__name__" {
                    return Value::Unknown;
                }
                if matches!(
                    name.as_str(),
                    "print"
                        | "len"
                        | "range"
                        | "str"
                        | "int"
                        | "bool"
                        | "object"
                        | "list"
                        | "tuple"
                        | "dict"
                        | "set"
                        | "float"
                        | "bytes"
                        | "ValueError"
                        | "RuntimeError"
                        | "AttributeError"
                        | "TypeError"
                        | "Exception"
                        | "staticmethod"
                        | "classmethod"
                        | "property"
                ) && !self.scopes[scope].names.contains_key(name.as_str())
                {
                    return Value::Provider(format!("builtins.{name}"));
                }
                Value::Unknown
            }
            ExpressionKind::Member(base, name) => {
                let base = recurse(self, base);
                self.member(base, name, depth + 1)
            }
            ExpressionKind::Call {
                function, keywords, ..
            } => match recurse(self, function) {
                Value::Callable(id) if self.is_class(id) => Value::Instance(id),
                Value::Callable(id) if matches!(self.symbols[id].binding, Binding::Definition(def, _) if def.kind == DefinitionKind::Coroutine) => {
                    Value::Coroutine(id)
                }
                Value::Provider(name)
                    if matches!(
                        name.as_str(),
                        "fastapi.FastAPI" | "fastapi.applications.FastAPI"
                    ) =>
                {
                    Value::Object(
                        (
                            self.scopes[scope].file,
                            expression.at.line,
                            expression.at.column,
                        ),
                        ObjectKind::App,
                    )
                }
                Value::Provider(name)
                    if matches!(
                        name.as_str(),
                        "fastapi.APIRouter" | "fastapi.routing.APIRouter"
                    ) =>
                {
                    Value::Object(
                        (
                            self.scopes[scope].file,
                            expression.at.line,
                            expression.at.column,
                        ),
                        ObjectKind::Router,
                    )
                }
                Value::Provider(name)
                    if matches!(
                        name.as_str(),
                        "concurrent.futures.ThreadPoolExecutor"
                            | "concurrent.futures.ProcessPoolExecutor"
                    ) =>
                {
                    Value::Executor
                }
                Value::Provider(name) if name == "multiprocessing.Process" => {
                    let target = keywords
                        .iter()
                        .find(|(name, _)| name == "target")
                        .map_or(Value::Unknown, |(_, target)| recurse(self, target));
                    Value::Process(Box::new(target))
                }
                _ => Value::Unknown,
            },
            ExpressionKind::Text(text) => Value::Text(text.clone()),
            ExpressionKind::Bool(value) => Value::Bool(*value),
            ExpressionKind::Scalar | ExpressionKind::Operands(_) => Value::Scalar,
            ExpressionKind::Sequence(items) => {
                Value::Sequence(items.iter().map(|expr| recurse(self, expr)).collect())
            }
            ExpressionKind::Dictionary(items) => {
                Value::Mapping(items.iter().map(|expr| recurse(self, expr)).collect())
            }
            ExpressionKind::Compare {
                left,
                right,
                equal,
                identity,
            } => {
                let left = recurse(self, left);
                let right = recurse(self, right);
                let same = match (left, right) {
                    (Value::Text(left), Value::Text(right)) if !identity => left == right,
                    (Value::Bool(left), Value::Bool(right)) => left == right,
                    _ => return Value::Unknown,
                };
                Value::Bool(same == *equal)
            }
            ExpressionKind::Not(value) => match recurse(self, value) {
                Value::Bool(value) => Value::Bool(!value),
                _ => Value::Unknown,
            },
            ExpressionKind::Boolean { and, values } => {
                for value in values {
                    match recurse(self, value) {
                        Value::Bool(value) if value != *and => return Value::Bool(value),
                        Value::Bool(_) => {}
                        _ => return Value::Unknown,
                    }
                }
                Value::Bool(*and)
            }
            ExpressionKind::Await(_) => Value::Unknown,
        }
    }

    fn edge(
        &mut self,
        scope: ScopeId,
        target: SymbolId,
        kind: &'static str,
        certainty: &'static str,
    ) {
        self.scopes[scope].edges.push(Edge {
            target,
            kind,
            certainty,
        });
    }

    fn expose(&mut self, scope: ScopeId, value: Value, kind: &'static str, at: Location) {
        match value {
            Value::Callable(id) | Value::Coroutine(id) => {
                self.edge(scope, id, kind, "possible");
                self.uncertain(
                    scope,
                    at,
                    "a known target may be invoked through an external or asynchronous consumer",
                );
            }
            Value::Sequence(values) | Value::Mapping(values) => {
                for value in values {
                    self.expose(scope, value, kind, at);
                }
            }
            _ => {}
        }
    }

    fn walk(&mut self, scope: ScopeId, expr: &Expression, before: Location) {
        match &expr.kind {
            ExpressionKind::Call {
                function,
                arguments,
                keywords,
            } => {
                self.walk(scope, function, before);
                for expr in arguments
                    .iter()
                    .chain(keywords.iter().map(|(_, value)| value))
                {
                    self.walk(scope, expr, before);
                }
                let callee = self.identity(scope, function, before, 0);
                match callee {
                    Value::Callable(id) => {
                        self.direct.insert(id);
                        self.record_direct_arguments(scope, id, before, arguments, keywords);
                        if !matches!(self.symbols[id].binding, Binding::Definition(def, _) if def.kind == DefinitionKind::Coroutine)
                        {
                            self.edge(scope, id, "direct_call", "certain");
                        }
                        if self.is_class(id)
                            && let Value::Callable(initializer) =
                                self.member(Value::Instance(id), "__init__", 0)
                        {
                            self.edge(scope, initializer, "constructor", "certain");
                        }
                        // Callback parameter substitution is bounded by retaining the
                        // finite supplied values even if the callee dispatches dynamically.
                        for argument in arguments
                            .iter()
                            .chain(keywords.iter().map(|(_, value)| value))
                        {
                            let value = self.identity(scope, argument, before, 0);
                            self.expose(scope, value, "passed_callable", argument.at);
                        }
                    }
                    Value::Provider(name) if name == "asyncio.create_task" => {
                        self.activate_argument(
                            scope,
                            arguments.first(),
                            before,
                            "scheduled_coroutine",
                            expr.at,
                        );
                    }
                    Value::Submit => self.activate_argument(
                        scope,
                        arguments.first(),
                        before,
                        "executor_submission",
                        expr.at,
                    ),
                    Value::Start(value) => match *value {
                        Value::Callable(_) => self.expose(scope, *value, "process_target", expr.at),
                        _ => self.scope_issue(
                            scope,
                            expr.at,
                            "unresolved_callback",
                            "process target could not be bounded",
                        ),
                    },
                    Value::Method(parent, method) if method == "include_router" => {
                        let target = arguments.first().or_else(|| {
                            keywords
                                .iter()
                                .find(|(name, _)| name == "router")
                                .map(|(_, value)| value)
                        });
                        if let Some(target) = target
                            && let Value::Object(child, ObjectKind::Router) =
                                self.identity(scope, target, before, 0)
                        {
                            self.scopes[scope].mounts.push((parent, child));
                        } else {
                            self.scope_issue(
                                scope,
                                expr.at,
                                "unresolved_registration",
                                "router identity could not be resolved",
                            );
                        }
                    }
                    Value::Provider(name) if is_known_call(&name) => {
                        if let Value::Object(object, ObjectKind::App) =
                            self.identity(scope, expr, before, 0)
                        {
                            self.scopes[scope].activated_apps.push(object);
                        }
                    }
                    _ => {
                        let parameter = match &function.kind {
                            ExpressionKind::Name(name) => self.lexical(scope, name, before),
                            _ => None,
                        }
                        .filter(|&id| matches!(self.symbols[id].binding, Binding::Parameter));
                        if let Some(parameter) = parameter {
                            self.parameter_calls.push(ParameterCall {
                                scope,
                                parameter,
                                at: expr.at,
                            });
                        } else {
                            self.scope_issue(
                                scope,
                                expr.at,
                                "unresolved_call",
                                "call target is outside the bounded model",
                            );
                        }
                    }
                }
            }
            ExpressionKind::Await(value) => {
                self.walk(scope, value, before);
                self.activate_argument(scope, Some(value), before, "awaited_coroutine", expr.at);
            }
            ExpressionKind::Member(base, _) | ExpressionKind::Not(base) => {
                self.walk(scope, base, before)
            }
            ExpressionKind::Compare { left, right, .. } => {
                self.walk(scope, left, before);
                self.walk(scope, right, before);
            }
            ExpressionKind::Sequence(values)
            | ExpressionKind::Dictionary(values)
            | ExpressionKind::Operands(values) => {
                for item in values {
                    self.walk(scope, item, before);
                }
            }
            ExpressionKind::Boolean { and, values } => {
                for value in values {
                    self.walk(scope, value, before);
                    if matches!(self.identity(scope, value, before, 0), Value::Bool(result) if result != *and)
                    {
                        break;
                    }
                }
            }
            ExpressionKind::Name(_) => {
                self.identity(scope, expr, before, 0);
            }
            _ => {}
        }
    }

    fn record_direct_arguments(
        &mut self,
        caller: ScopeId,
        callee: SymbolId,
        before: Location,
        args: &[Expression],
        keywords: &[(String, Expression)],
    ) {
        let Binding::Definition(definition, _) = self.symbols[callee].binding else {
            return;
        };
        if definition.kind != DefinitionKind::Function
            || !definition.simple_positional_parameters
            || self.scopes[self.symbols[callee].scope]
                .owner
                .is_some_and(|owner| self.is_class(owner))
        {
            return;
        }
        let mut bounded_callbacks = BTreeSet::new();
        for (parameter, argument) in definition.parameters.iter().zip(args) {
            if matches!(
                self.identity(caller, argument, before, 0),
                Value::Callable(_)
            ) {
                bounded_callbacks.insert(parameter.text.clone());
            }
        }
        for (parameter, argument) in keywords {
            if matches!(
                self.identity(caller, argument, before, 0),
                Value::Callable(_)
            ) {
                bounded_callbacks.insert(parameter.clone());
            }
        }
        self.direct_arguments
            .entry(callee)
            .or_default()
            .push(DirectArguments {
                caller,
                bounded_callbacks,
            });
    }

    fn activate_argument(
        &mut self,
        scope: ScopeId,
        arg: Option<&Expression>,
        before: Location,
        kind: &'static str,
        at: Location,
    ) {
        let target = arg.map_or(Value::Unknown, |expr| self.identity(scope, expr, before, 0));
        if matches!(target, Value::Callable(_) | Value::Coroutine(_)) {
            self.expose(scope, target, kind, at);
        } else {
            self.scope_issue(
                scope,
                at,
                "unresolved_callback",
                "callback target could not be bounded",
            );
        }
    }

    fn annotation(&mut self, scope: ScopeId, expr: &Expression, before: Location, evaluated: bool) {
        match &expr.kind {
            ExpressionKind::Operands(values) | ExpressionKind::Sequence(values) => {
                for reference in values {
                    self.annotation(scope, reference, before, evaluated);
                }
            }
            ExpressionKind::Call { .. }
                if evaluated
                    && !self.files[self.scopes[scope].file]
                        .facts
                        .postponed_annotations =>
            {
                self.walk(scope, expr, before);
            }
            _ => {
                if let Value::Callable(id) = self.identity(scope, expr, before, 0) {
                    self.edge(scope, id, "type_reference", "possible");
                }
            }
        }
    }

    fn suite(&mut self, scope: ScopeId, body: &[Statement]) -> bool {
        for statement in body {
            let at = statement.at;
            match &statement.kind {
                StatementKind::Define(definition) => {
                    let target = self.by_location
                        [&(scope, definition.name.at.line, definition.name.at.column)];
                    for annotation in &definition.annotations {
                        self.annotation(scope, annotation, at, true);
                    }
                    for expression in &definition.defaults {
                        self.walk(scope, expression, at);
                    }
                    for base in &definition.bases {
                        self.walk(scope, base, at);
                        if let Value::Callable(id) = self.identity(scope, base, at, 0) {
                            self.edge(scope, id, "base_class", "certain");
                        }
                    }
                    for decorator in &definition.decorators {
                        self.decorator(scope, decorator, target, at);
                    }
                }
                StatementKind::Assign {
                    value, annotation, ..
                } => {
                    if let Some(value) = value {
                        self.walk(scope, value, at);
                    }
                    if let Some(annotation) = annotation {
                        let evaluated = self.scopes[scope]
                            .owner
                            .is_none_or(|owner| self.is_class(owner));
                        self.annotation(scope, annotation, at, evaluated);
                    }
                }
                StatementKind::Return(value) => {
                    if let Some(value) = value {
                        self.walk(scope, value, at);
                        let result = self.identity(scope, value, at, 0);
                        self.expose(scope, result, "returned_callable", value.at);
                    }
                    return true;
                }
                StatementKind::Raise(value) => {
                    if let Some(value) = value {
                        self.walk(scope, value, at);
                    }
                    return true;
                }
                StatementKind::Evaluate(value) => self.walk(scope, value, at),
                StatementKind::If {
                    test,
                    body,
                    otherwise,
                } => {
                    self.walk(scope, test, at);
                    let test = self.identity(scope, test, at, 0);
                    let stop = match test {
                        Value::Bool(true) => self.suite(scope, body),
                        Value::Bool(false) => self.suite(scope, otherwise),
                        _ => self.suite(scope, body) & self.suite(scope, otherwise),
                    };
                    if stop {
                        return true;
                    }
                }
                StatementKind::With { items, body } => {
                    for (expression, _) in items {
                        self.walk(scope, expression, at);
                        if !matches!(self.identity(scope, expression, at, 0), Value::Executor) {
                            self.scope_issue(
                                scope,
                                expression.at,
                                "unresolved_context_manager",
                                "context manager is outside the bounded model",
                            );
                        }
                    }
                    if self.suite(scope, body) {
                        return true;
                    }
                }
                StatementKind::Import(import) if import.level > 0 => {
                    let resolved = self
                        .relative_module(self.scopes[scope].file, import)
                        .is_some_and(|module| {
                            self.modules.contains_key(module.as_str())
                                || import.member.as_ref().is_some_and(|name| {
                                    self.modules
                                        .contains_key(format!("{module}.{name}").as_str())
                                })
                        });
                    if !resolved {
                        self.issue(
                            self.scopes[scope].file,
                            at,
                            "unresolved_import",
                            "relative import has no selected module",
                        );
                    }
                }
                _ => {}
            }
        }
        false
    }

    fn decorator(
        &mut self,
        scope: ScopeId,
        decorator: &Expression,
        target: SymbolId,
        before: Location,
    ) {
        let (function, args, keywords) = match &decorator.kind {
            ExpressionKind::Call {
                function,
                arguments,
                keywords,
            } => (function.as_ref(), arguments.as_slice(), keywords.as_slice()),
            _ => (decorator, [].as_slice(), [].as_slice()),
        };
        for value in args.iter().chain(keywords.iter().map(|(_, value)| value)) {
            self.walk(scope, value, before);
        }
        match self.identity(scope, function, before, 0) {
            Value::Method(object, name) if is_route(&name) => {
                self.scopes[scope].routes.push((object, target))
            }
            Value::Provider(name) if name == "pytest.fixture" => {
                self.fixtures.push(target);
                if keywords
                    .iter()
                    .any(|(name, value)| name == "autouse" && static_bool(value) == Some(true))
                {
                    self.called_tests.push(target);
                }
            }
            Value::Callable(id) => {
                self.edge(scope, id, "decorator", "certain");
                let Binding::Definition(definition, _) = self.symbols[id].binding else {
                    unreachable!()
                };
                if !is_identity_decorator(definition) {
                    self.scope_issue(
                        scope,
                        decorator.at,
                        "unresolved_decorator",
                        "local decorator may replace or invoke the decorated definition",
                    );
                }
            }
            Value::Provider(name)
                if matches!(
                    name.as_str(),
                    "builtins.staticmethod" | "builtins.classmethod" | "builtins.property"
                ) => {}
            _ => self.scope_issue(
                scope,
                decorator.at,
                "unresolved_decorator",
                "decorator identity is outside the bounded model",
            ),
        };
    }

    fn test_edges(&mut self) -> Vec<Edge> {
        let mut test_roots = self.called_tests.clone();
        for (id, symbol) in self.symbols.iter().enumerate() {
            if symbol.scope >= self.files.len()
                || !matches!(symbol.binding, Binding::Definition(def, _) if def.kind != DefinitionKind::Class)
            {
                continue;
            }
            let filename = self.files[symbol.scope]
                .path
                .rsplit('/')
                .next()
                .unwrap_or_default();
            if (filename.starts_with("test_") || filename.ends_with("_test.py"))
                && symbol.name.text.starts_with("test_")
                && !self.fixtures.contains(&id)
            {
                test_roots.push(id);
            }
        }
        let mut queue = VecDeque::from(test_roots);
        let mut roots = BTreeSet::new();
        while let Some(id) = queue.pop_front() {
            if !roots.insert(id) {
                continue;
            }
            let Binding::Definition(definition, _) = self.symbols[id].binding else {
                continue;
            };
            let file = &self.files[self.scopes[self.symbols[id].scope].file].path;
            let candidates = self.fixtures.iter().copied().filter(|&id| {
                let fixture = &self.files[self.scopes[self.symbols[id].scope].file].path;
                fixture == file
                    || fixture
                        .strip_suffix("conftest.py")
                        .is_some_and(|parent| file.starts_with(parent))
            });
            for fixture in candidates {
                if definition
                    .parameters
                    .iter()
                    .any(|param| param.text == self.symbols[fixture].name.text)
                {
                    queue.push_back(fixture);
                }
            }
        }
        roots
            .into_iter()
            .map(|target| Edge {
                target,
                kind: "pytest_root",
                certainty: "certain",
            })
            .collect()
    }

    fn external_roots(&mut self, entrypoints: &[Entrypoint]) -> Vec<Edge> {
        let mut roots = Vec::new();
        for file in 0..self.files.len() {
            if let Some(all) = self.local(file, "__all__", END) {
                let value = self.binding(all, END, 0);
                if let Value::Sequence(exports) = value {
                    for export in exports {
                        let Value::Text(export) = export else {
                            self.issue(
                                file,
                                self.symbols[all].name.at,
                                "opaque_exports",
                                "public exports must be literal names",
                            );
                            continue;
                        };
                        let value = self.module_member(&self.files[file].module, &export, 0);
                        match value {
                            Value::Callable(target) => roots.push(Edge {
                                target,
                                kind: "public_export",
                                certainty: "certain",
                            }),
                            Value::Unknown => self.issue(
                                file,
                                self.symbols[all].name.at,
                                "unresolved_export",
                                format!("public export {export} could not be resolved"),
                            ),
                            _ => {}
                        }
                    }
                } else {
                    self.issue(
                        file,
                        self.symbols[all].name.at,
                        "opaque_exports",
                        "public exports must be a literal sequence",
                    );
                }
            }
            let aliases: Vec<_> = self.scopes[file].names.values().flatten().copied().filter(|&id| matches!(
                self.symbols[id].binding, Binding::Import(import) if import.declares_export && self.files[file].is_package
            )).collect();
            for alias in aliases {
                if let Value::Callable(target) = self.binding(alias, END, 0) {
                    roots.push(Edge {
                        target,
                        kind: "explicit_re_export",
                        certainty: "certain",
                    });
                }
            }
        }
        for id in 0..self.symbols.len() {
            let symbol = &self.symbols[id];
            let Binding::Definition(definition, _) = symbol.binding else {
                continue;
            };
            if entrypoints.is_empty() {
                continue;
            }
            let scope = symbol.scope;
            let file = &self.files[self.scopes[scope].file];
            let full_name = self.full_name(id);
            let mut qualifiers = vec![(
                "type".into(),
                if self.is_class(id) {
                    "class"
                } else {
                    "function"
                }
                .into(),
            )];
            if let Some(parent) = self.scopes[scope].owner {
                qualifiers.push(("parent_name".into(), self.symbols[parent].name.text.clone()));
                qualifiers.push(("parent_full_name".into(), self.full_name(parent)));
                qualifiers.push(("parent_module".into(), file.module.clone()));
                qualifiers.push(("parent_path".into(), file.path.clone()));
                if let Binding::Definition(parent_definition, _) = self.symbols[parent].binding {
                    for base in &parent_definition.bases {
                        match self.identity(
                            self.symbols[parent].scope,
                            base,
                            parent_definition.name.at,
                            0,
                        ) {
                            Value::Provider(name) => {
                                qualifiers.push(("parent_base_class".into(), name))
                            }
                            Value::Callable(id) => {
                                qualifiers.push(("parent_base_class".into(), self.full_name(id)))
                            }
                            _ => {}
                        }
                    }
                }
            }
            for (key, values) in [
                ("decorator", &definition.decorators),
                ("base_class", &definition.bases),
            ] {
                for expression in values {
                    let identity = match &expression.kind {
                        ExpressionKind::Call { function, .. } => function,
                        _ => expression,
                    };
                    match self.identity(scope, identity, definition.name.at, 0) {
                        Value::Provider(name) => qualifiers.push((key.into(), name)),
                        Value::Callable(id) => qualifiers.push((key.into(), self.full_name(id))),
                        _ => {}
                    }
                }
            }
            if entrypoints.iter().any(|selector| {
                selector.matches(
                    &file.path,
                    &file.module,
                    &definition.name.text,
                    &full_name,
                    &qualifiers,
                )
            }) {
                roots.push(Edge {
                    target: id,
                    kind: "configured_entrypoint",
                    certainty: "certain",
                });
            }
        }
        roots
    }

    fn finish(mut self, roots: Vec<Edge>) -> Analysis {
        let mut reachable_scopes: BTreeSet<_> = (0..self.files.len()).collect();
        let mut reachable_symbols = BTreeSet::new();
        let mut active_objects = BTreeSet::new();
        let mut evidence = BTreeSet::new();
        let mut pending = VecDeque::from(roots);
        for scope in 0..self.scopes.len() {
            // Classes declared in a module execute their bodies eagerly, even
            // when the class itself has no consumers. Nested classes become
            // active when their enclosing scope is reached.
            if self.scopes[scope].owner.is_some_and(|id| self.is_class(id))
                && self.scopes[scope]
                    .parent
                    .is_some_and(|parent| reachable_scopes.contains(&parent))
            {
                reachable_scopes.insert(scope);
            }
        }
        for &scope in &reachable_scopes {
            pending.extend(&self.scopes[scope].edges);
        }
        let mut converged = false;
        for _ in 0..MAX_FRAMEWORK_PASSES {
            let before = (
                reachable_scopes.len(),
                active_objects.len(),
                reachable_symbols.len(),
            );
            while let Some(edge) = pending.pop_front() {
                evidence.insert((edge.target, edge.kind, edge.certainty));
                reachable_symbols.insert(edge.target);
                if edge.kind == "type_reference" {
                    continue;
                }
                let Binding::Definition(_, scope) = self.symbols[edge.target].binding else {
                    continue;
                };
                if reachable_scopes.insert(scope) {
                    pending.extend(&self.scopes[scope].edges);
                }
            }
            for scope in 0..self.scopes.len() {
                if self.scopes[scope].owner.is_some_and(|id| self.is_class(id))
                    && self.scopes[scope]
                        .parent
                        .is_some_and(|parent| reachable_scopes.contains(&parent))
                    && reachable_scopes.insert(scope)
                {
                    pending.extend(&self.scopes[scope].edges);
                }
            }
            for &scope in &reachable_scopes {
                let scope = &self.scopes[scope];
                active_objects.extend(&scope.activated_apps);
                for &(parent, child) in &scope.mounts {
                    if active_objects.contains(&parent) {
                        active_objects.insert(child);
                    }
                }
                for &(object, target) in &scope.routes {
                    if active_objects.contains(&object) && !reachable_symbols.contains(&target) {
                        pending.push_back(Edge {
                            target,
                            kind: "fastapi_registration",
                            certainty: "certain",
                        });
                    }
                }
            }
            if pending.is_empty()
                && before
                    == (
                        reachable_scopes.len(),
                        active_objects.len(),
                        reachable_symbols.len(),
                    )
            {
                converged = true;
                break;
            }
        }
        if !converged {
            self.issues.push(Issue::new(
                "analysis_limit",
                ".",
                "framework activation limit exceeded",
            ));
        }
        let independently_retained: BTreeSet<_> = evidence
            .iter()
            .filter_map(|&(target, kind, _)| {
                (kind != "direct_call" && kind != "type_reference").then_some(target)
            })
            .collect();
        for call in self
            .parameter_calls
            .iter()
            .filter(|call| reachable_scopes.contains(&call.scope))
        {
            let parameter = &self.symbols[call.parameter];
            let owner = self.scopes[parameter.scope]
                .owner
                .expect("parameters belong to a definition");
            let mut callers = self
                .direct_arguments
                .get(&owner)
                .into_iter()
                .flatten()
                .filter(|callsite| reachable_scopes.contains(&callsite.caller))
                .peekable();
            let finite = !independently_retained.contains(&owner)
                && callers.peek().is_some()
                && callers
                    .all(|callsite| callsite.bounded_callbacks.contains(&parameter.name.text));
            if !finite {
                self.issues.push(
                    Issue::new(
                        "unresolved_call",
                        &self.files[self.scopes[call.scope].file].path,
                        "callable parameter is outside the bounded model",
                    )
                    .at(call.at),
                );
            }
        }

        let mut findings = Vec::new();
        let mut retained = Vec::new();
        for (id, kind, certainty) in evidence {
            let symbol = &self.symbols[id];
            retained.push(Evidence {
                file: self.files[self.scopes[symbol.scope].file].path.clone(),
                line: symbol.name.at.line,
                full_name: self.full_name(id),
                kind,
                certainty,
            });
        }
        for (id, symbol) in self.symbols.iter().enumerate() {
            let scope = &self.scopes[symbol.scope];
            let name = &symbol.name.text;
            if name == "_" || (name.starts_with("__") && name.ends_with("__")) {
                continue;
            }
            let public = !name.starts_with('_');
            let (category, rule_id, confidence, reason) = match symbol.binding {
                Binding::Definition(def, _)
                    if !reachable_symbols.contains(&id) && def.kind == DefinitionKind::Class =>
                {
                    (Category::Class, "SKY-U004", 90, "unreachable_class")
                }
                Binding::Definition(def, _)
                    if !reachable_symbols.contains(&id) && def.kind != DefinitionKind::Class =>
                {
                    (
                        Category::Function,
                        "SKY-U001",
                        if public && scope.owner.is_some_and(|id| self.is_class(id)) {
                            80
                        } else {
                            90
                        },
                        "no_call_path_from_root",
                    )
                }
                Binding::Import(_) if !self.read.contains(&id) => {
                    (Category::Import, "SKY-U002", 100, "binding_is_unread")
                }
                Binding::Variable(_, _) if !self.read.contains(&id) => {
                    let confidence = match scope.owner {
                        Some(owner) if self.is_class(owner) => 80,
                        None => 90,
                        _ if !public => 90,
                        _ => 100,
                    };
                    (
                        Category::Variable,
                        "SKY-U003",
                        confidence,
                        "binding_is_unread",
                    )
                }
                Binding::Parameter
                    if !self.read.contains(&id)
                        && !name.starts_with('_')
                        && !matches!(name.as_str(), "self" | "cls")
                        && !scope.owner.is_some_and(|owner| {
                            self.fixtures.contains(&owner)
                                || reachable_symbols.contains(&owner)
                                    && !self.direct.contains(&owner)
                        }) =>
                {
                    (
                        Category::Parameter,
                        "SKY-U006",
                        if scope.owner.is_some_and(|owner| {
                            self.symbols[owner].name.text.starts_with('_')
                                && self.direct.contains(&owner)
                        }) {
                            100
                        } else {
                            50
                        },
                        "parameter_is_unread",
                    )
                }
                _ => continue,
            };
            let (name, full_name) = if let Binding::Import(import) = &symbol.binding {
                let module = self
                    .relative_module(scope.file, import)
                    .unwrap_or_else(|| import.module.clone());
                let full_name = import
                    .member
                    .as_ref()
                    .map_or_else(|| module.clone(), |member| format!("{module}.{member}"));
                let name = full_name.rsplit('.').next().unwrap_or(name).to_owned();
                (name, full_name)
            } else {
                (name.clone(), self.full_name(id))
            };
            findings.push(Finding {
                category,
                name,
                full_name,
                file: self.files[scope.file].path.clone(),
                line: symbol.name.at.line,
                column: symbol.name.at.column,
                confidence,
                rule_id,
                reason,
            });
        }
        for file in self.files.iter().filter(|file| file.facts.empty) {
            if !matches!(
                file.path.rsplit('/').next(),
                Some("__init__.py" | "__main__.py" | "main.py")
            ) {
                findings.push(Finding {
                    category: Category::File,
                    name: file.path.clone(),
                    full_name: file.path.clone(),
                    file: file.path.clone(),
                    line: 1,
                    column: 1,
                    confidence: 100,
                    rule_id: "SKY-E002",
                    reason: "empty_or_docstring_only",
                });
            }
        }
        self.issues.extend(
            self.scope_issues
                .into_iter()
                .filter_map(|(scope, issue)| reachable_scopes.contains(&scope).then_some(issue)),
        );
        self.issues.sort();
        self.issues.dedup();
        self.uncertainties.sort();
        self.uncertainties.dedup();
        Analysis {
            findings,
            evidence: retained,
            issues: self.issues,
            uncertainties: self.uncertainties,
        }
    }
}

fn position(at: Location) -> [usize; 2] {
    [at.line, at.column]
}

fn suite_end(body: &[Statement]) -> [usize; 2] {
    let Some(last) = body.last() else {
        return [0, 0];
    };
    let mut end = position(last.at);
    match &last.kind {
        StatementKind::If {
            body, otherwise, ..
        } => {
            end = end.max(suite_end(body)).max(suite_end(otherwise));
        }
        StatementKind::With { body, .. } => {
            end = end.max(suite_end(body));
        }
        StatementKind::Define(definition) => {
            end = end.max(suite_end(&definition.body));
        }
        _ => {}
    }
    end
}

fn static_bool(expression: &Expression) -> Option<bool> {
    match &expression.kind {
        ExpressionKind::Bool(value) => Some(*value),
        ExpressionKind::Not(value) => static_bool(value).map(|it| !it),
        _ => None,
    }
}

fn is_identity_decorator(definition: &Definition) -> bool {
    let [parameter] = definition.parameters.as_slice() else {
        return false;
    };
    matches!(definition.body.as_slice(), [Statement { kind: StatementKind::Return(Some(Expression {
        kind: ExpressionKind::Name(name), ..
    })), .. }] if name == &parameter.text)
}

fn is_route(name: &str) -> bool {
    matches!(
        name,
        "get"
            | "post"
            | "put"
            | "delete"
            | "patch"
            | "head"
            | "options"
            | "trace"
            | "api_route"
            | "route"
            | "websocket"
            | "websocket_route"
    )
}

fn is_known_call(name: &str) -> bool {
    name.starts_with("builtins.")
        || matches!(
            name,
            "fastapi.FastAPI"
                | "fastapi.applications.FastAPI"
                | "fastapi.APIRouter"
                | "fastapi.routing.APIRouter"
                | "concurrent.futures.ThreadPoolExecutor"
                | "concurrent.futures.ProcessPoolExecutor"
                | "multiprocessing.Process"
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use skarmory_python::phase02::extract;

    fn module(path: &str, name: &str, text: &str) -> AnalyzedFile {
        AnalyzedFile {
            path: path.into(),
            module: name.into(),
            is_package: path.ends_with("__init__.py"),
            facts: extract(text).unwrap(),
        }
    }

    fn names(result: &Analysis, category: Category) -> Vec<&str> {
        let mut names: Vec<_> = result
            .findings
            .iter()
            .filter(|finding| finding.category == category)
            .map(|finding| finding.name.as_str())
            .collect();
        names.sort_unstable();
        names
    }

    #[test]
    fn reports_required_categories_and_retains_rooted_definitions() {
        let files = [
            module(
                "main.py",
                "main",
                "import os\nfrom helper import work as run\n\nclass Unused:\n    pass\nclass Used:\n    pass\n\ndef _entry(unread):\n    quiet = 1\n    value = 2\n    print(value)\n    run()\n    Used()\n\ndef dormant():\n    dormant()\n\n_entry(0)\n",
            ),
            module("helper.py", "helper", "def work():\n    return 3\n"),
            module("empty.py", "empty", "# deliberately empty\n"),
        ];
        let result = analyze(&files, &[]);
        assert!(result.issues.is_empty(), "{:?}", result.issues);
        assert_eq!(names(&result, Category::Function), ["dormant"]);
        assert_eq!(names(&result, Category::Class), ["Unused"]);
        assert_eq!(names(&result, Category::Import), ["os"]);
        assert_eq!(names(&result, Category::Variable), ["quiet"]);
        assert_eq!(names(&result, Category::Parameter), ["unread"]);
        assert_eq!(names(&result, Category::File), ["empty.py"]);
    }

    #[test]
    fn models_mounted_routes_scheduling_and_process_start() {
        let files = [module(
            "app.py",
            "app",
            "from fastapi import FastAPI, APIRouter\nfrom asyncio import create_task\nfrom concurrent.futures import ThreadPoolExecutor\nfrom multiprocessing import Process\n\napp = FastAPI()\nactive = APIRouter()\ndormant = APIRouter()\napp.include_router(active)\n\n@active.get('/')\ndef mounted():\n    return {'ready': True}\n@dormant.get('/')\ndef unmounted():\n    return False\n\nasync def scheduled():\n    return 1\nasync def unopened():\n    return 2\n\ndef submit_target():\n    return 3\ndef launched():\n    return 4\ndef parked():\n    return 5\n\ncreate_task(scheduled())\nunopened()\nwith ThreadPoolExecutor() as pool:\n    pool.submit(submit_target)\nready = Process(target=launched)\nunused = Process(target=parked)\nready.start()\n",
        )];
        let result = analyze(&files, &[]);
        assert!(result.issues.is_empty(), "{:?}", result.issues);
        assert_eq!(
            names(&result, Category::Function),
            ["parked", "unmounted", "unopened"]
        );
        assert!(
            result
                .evidence
                .iter()
                .any(|evidence| evidence.full_name == "app.mounted"
                    && evidence.kind == "fastapi_registration")
        );
        assert!(result.evidence.iter().any(
            |evidence| evidence.full_name == "app.launched" && evidence.certainty == "possible"
        ));
    }

    #[test]
    fn exports_lazy_target_and_keeps_other_scoped_names_separate() {
        let files = [
            module(
                "pkg/__init__.py",
                "pkg",
                "__all__ = ['work']\n\ndef __getattr__(name):\n    if name == 'work':\n        from .worker import work\n        return work\n    raise AttributeError(name)\n",
            ),
            module("pkg/worker.py", "pkg.worker", "def work():\n    return 1\n"),
            module("other.py", "other", "def work():\n    return 2\n"),
        ];
        let result = analyze(&files, &[]);
        assert!(result.issues.is_empty(), "{:?}", result.issues);
        assert_eq!(names(&result, Category::Function), ["work"]);
        assert!(
            result
                .evidence
                .iter()
                .any(|evidence| evidence.full_name == "pkg.worker.work"
                    && evidence.kind == "public_export")
        );
        assert!(
            result
                .findings
                .iter()
                .any(|found| found.full_name == "other.work")
        );
    }

    #[test]
    fn unknown_dispatch_blocks_reachable_but_not_dormant_code() {
        let source = "def go(callback):\n    callback()\n";
        let dormant = analyze(&[module("main.py", "main", source)], &[]);
        assert!(dormant.issues.is_empty(), "{:?}", dormant.issues);
        let active = analyze(
            &[module("main.py", "main", &format!("{source}\ngo(1)\n"))],
            &[],
        );
        assert!(
            active
                .issues
                .iter()
                .any(|issue| issue.kind == "unresolved_call" && issue.line == Some(2))
        );
    }

    #[test]
    fn direct_callback_calls_can_resolve_finite_parameter_targets() {
        let source =
            "def go(callback):\n    callback()\n\ndef local():\n    return 1\n\ngo(local)\n";
        let active = analyze(&[module("main.py", "main", source)], &[]);
        assert!(active.issues.is_empty(), "{:?}", active.issues);
        assert!(
            active
                .evidence
                .iter()
                .any(|edge| edge.full_name == "main.local" && edge.kind == "passed_callable")
        );

        let mixed = analyze(
            &[module("main.py", "main", &format!("{source}\ngo(42)\n"))],
            &[],
        );
        assert!(
            mixed
                .issues
                .iter()
                .any(|issue| issue.kind == "unresolved_call" && issue.line == Some(2))
        );
    }

    #[test]
    fn conditional_shadowing_cannot_prove_the_previous_target_unused() {
        let source = "def old():\n    pass\ndef new():\n    pass\nhandler = old\nif flag:\n    handler = new\nhandler()\n";
        let result = analyze(&[module("main.py", "main", source)], &[]);
        assert!(
            result
                .issues
                .iter()
                .any(|issue| issue.kind == "ambiguous_binding")
        );
    }
}
