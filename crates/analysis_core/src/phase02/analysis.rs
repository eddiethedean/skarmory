//! Project binding resolution and reachability for the Skylos profile.

use std::cell::OnceCell;
use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::ops::RangeInclusive;

use skarmory_python::Location;
use skarmory_python::phase02::{
    Definition, DefinitionKind, Expression, ExpressionKind, Import, Name, Statement, StatementKind,
};

use super::config::Entrypoint;
use super::{Analysis, AnalyzedFile, Category, Evidence, Finding};
use super::{package_entrypoints, runtime_evidence};
use crate::Issue;
use crate::grep_verification::{self, Evidence as VerificationEvidence};

const MAX_BINDING_CHAIN: usize = 64;
const MAX_FRAMEWORK_PASSES: usize = 64;
const MAX_PYTEST_REQUEST_STATES: usize = 16_384;
const MAX_CONDITIONAL_STATES: usize = 4_096;
const END: Location = Location {
    line: usize::MAX,
    column: usize::MAX,
};

type SymbolId = usize;
type ScopeId = usize;
type Object = (usize, usize, usize);

#[derive(Clone)]
enum Value {
    Callable(Invocation),
    Instance(SymbolId),
    Alternatives(Vec<Value>),
    ReceiverParameter(SymbolId),
    ReceiverMethod(SymbolId, String),
    Coroutine(Invocation),
    ScheduledTask,
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
    // Python 3.11 can refresh a previously returned dictionary on a later read.
    LocalNamespace(ScopeId, [usize; 2]),
    NamespaceCopy(Box<NamespaceCopy>),
    NamespaceParameter(SymbolId),
    OpaqueNamespace,
    Range,
    /// An exact built-in container with scalar elements and untracked length and order.
    BuiltinIterable,
    Scalar,
    OperatorResult,
    Unknown,
}

impl Value {
    fn alternatives(values: Vec<Self>) -> Self {
        fn flatten(value: Value, flattened: &mut Vec<Value>) {
            match value {
                Value::Alternatives(values) => {
                    for value in values {
                        flatten(value, flattened);
                    }
                }
                value => flattened.push(value),
            }
        }

        let mut flattened = Vec::new();
        for value in values {
            flatten(value, &mut flattened);
        }
        Self::Alternatives(flattened)
    }

    fn contains_unknown(&self) -> bool {
        match self {
            Self::Unknown => true,
            Self::Sequence(values) | Self::Mapping(values) => {
                values.iter().any(Self::contains_unknown)
            }
            Self::Alternatives(values) => values.iter().any(Self::contains_unknown),
            _ => false,
        }
    }

    fn may_contain_namespace(&self) -> bool {
        match self {
            Self::LocalNamespace(..) | Self::NamespaceCopy(_) | Self::OpaqueNamespace => true,
            Self::Sequence(values) | Self::Mapping(values) => {
                values.iter().any(Self::may_contain_namespace)
            }
            Self::Alternatives(values) => values.iter().any(Self::may_contain_namespace),
            _ => false,
        }
    }

    fn is_builtin_scalar(&self) -> bool {
        matches!(self, Self::Scalar | Self::Bool(_) | Self::Text(_))
    }

    fn is_builtin_scalar_or_iterable(&self) -> bool {
        self.is_builtin_scalar() || matches!(self, Self::Range | Self::BuiltinIterable)
    }
}

#[derive(Clone)]
struct NamespaceCopy {
    owner: ScopeId,
    first: [usize; 2],
    copied_at: (ScopeId, [usize; 2]),
    site: (ScopeId, [usize; 2]),
    keywords: BTreeMap<String, Value>,
}

#[derive(Clone, Copy)]
struct Invocation {
    target: SymbolId,
    receiver: Receiver,
    verified: bool,
}

impl Invocation {
    fn ordinary(target: SymbolId) -> Self {
        Self {
            target,
            receiver: Receiver::Ordinary,
            verified: false,
        }
    }

    fn bind_receiver(self, receiver: Receiver) -> Self {
        if matches!(self.receiver, Receiver::Bound(_)) {
            self
        } else {
            Self { receiver, ..self }
        }
    }
}

#[derive(Clone, Copy)]
enum Receiver {
    Ordinary,
    Bound(SymbolId),
    Explicit(Option<SymbolId>),
}

impl Receiver {
    fn supplied(self) -> Option<SymbolId> {
        match self {
            Self::Ordinary => None,
            Self::Bound(class) => Some(class),
            Self::Explicit(class) => class,
        }
    }
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
    namespace_reads: BTreeMap<[usize; 2], NamespaceRead>,
    namespace_copy_cutoffs: BTreeMap<[usize; 2], [usize; 2]>,
    namespace_captures: OnceCell<Vec<SymbolId>>,
    namespace_exclusions: Vec<RangeInclusive<[usize; 2]>>,
    namespace_escape_exclusions: BTreeMap<[usize; 2], Vec<RangeInclusive<[usize; 2]>>>,
    edges: Vec<Edge>,
    imports: Vec<usize>,
    evaluated_imports: Vec<SymbolId>,
    initializer_imports: Vec<(SymbolId, usize)>,
    class_bodies: Vec<ScopeId>,
    activated_apps: Vec<Object>,
    mounts: Vec<(Object, Object)>,
    routes: Vec<(Object, SymbolId)>,
}

struct NamespaceRead {
    bindings: Vec<SymbolId>,
    exclusions: Vec<RangeInclusive<[usize; 2]>>,
}

enum Binding<'a> {
    Definition(&'a Definition, ScopeId),
    Import(&'a Import),
    Variable(Option<&'a Expression>, Location),
    Parameter,
}

enum LexicalBinding {
    Found(SymbolId),
    UnboundLocal,
    Missing,
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

struct ClassInvocation {
    scope: ScopeId,
    at: Location,
    edge: Edge,
    receiver: Receiver,
}

#[derive(Clone)]
struct ReceiverCall {
    scope: ScopeId,
    parameter: SymbolId,
    member: String,
    at: Location,
}

struct DirectArguments {
    caller: ScopeId,
    at: Location,
    values: BTreeMap<String, Value>,
}

struct CallArguments<'a> {
    scope: ScopeId,
    before: Location,
    at: Location,
    positional: &'a [Expression],
    keywords: &'a [(String, Expression)],
}

struct Export {
    scope: ScopeId,
    at: Location,
    kind: &'static str,
    value: Value,
}

struct ParameterCall {
    scope: ScopeId,
    parameter: SymbolId,
    at: Location,
}

struct PytestFixture {
    name: Option<String>,
    autouse: bool,
}

struct PytestRegistration {
    target: SymbolId,
    bindings: Vec<SymbolId>,
}

type PytestNames<'a> = BTreeMap<&'a str, Vec<SymbolId>>;

struct PytestFixtureChoices {
    alternatives: Vec<PytestRegistration>,
    may_be_absent: bool,
}

struct LazyBranch<'a> {
    scope: ScopeId,
    imports: &'a [Statement],
    value: &'a Expression,
    at: Location,
    argument: Option<(SymbolId, &'a str)>,
}

#[derive(Clone, Copy)]
enum ValueEvaluation<'a> {
    Identity,
    Lazy {
        execution_scope: Option<ScopeId>,
        argument: Option<(SymbolId, &'a str)>,
    },
}

enum LazyMember<'a> {
    Branch(LazyBranch<'a>),
    Missing,
    Unknown,
    Conditional,
}

enum ModuleMember<'a> {
    Binding(SymbolId, Option<SymbolId>),
    Submodule(usize, Vec<SymbolId>),
    Lazy(SymbolId, LazyBranch<'a>),
    Missing,
    UnattachedSubmodule,
    Unknown,
    Ambiguous,
    ConditionalHook,
    OpaqueHook,
}

#[derive(Clone, Copy)]
enum ModuleMemberAccess<'a> {
    Attribute,
    FromImport,
    Export {
        at: Location,
        initialization_edges: &'a [Vec<ScopeId>],
        initialization_imports: &'a OnceCell<BTreeSet<SymbolId>>,
    },
}

enum SubmoduleAttachment {
    Attached(SymbolId),
    Rebound(SymbolId),
    Absent,
    Ambiguous,
}

#[cfg(test)]
fn analyze(files: &[AnalyzedFile], entrypoints: &[Entrypoint]) -> Analysis {
    analyze_with_verification(files, entrypoints, None, None, None)
}

pub(super) fn analyze_with_verification(
    files: &[AnalyzedFile],
    entrypoints: &[Entrypoint],
    verification: Option<&grep_verification::Snapshot>,
    package_entrypoints: Option<&package_entrypoints::Snapshot>,
    runtime: Option<&runtime_evidence::Snapshot>,
) -> Analysis {
    let mut graph = Graph {
        files,
        modules: BTreeMap::new(),
        scopes: Vec::new(),
        symbols: Vec::new(),
        direct_submodule_imports: BTreeMap::new(),
        by_location: BTreeMap::new(),
        read: BTreeSet::new(),
        namespace_exports: BTreeMap::new(),
        direct: BTreeSet::new(),
        conditional: BTreeMap::new(),
        conditional_groups: BTreeMap::new(),
        next_conditional_group: 0,
        dormant_classes: BTreeSet::new(),
        subclassed: BTreeSet::new(),
        instance_receivers: BTreeMap::new(),
        class_invocations: Vec::new(),
        method_receivers: BTreeMap::new(),
        receiver_calls: Vec::new(),
        resolved_receiver_calls: BTreeSet::new(),
        receiver_call_issues: BTreeSet::new(),
        issues: Vec::new(),
        scope_issues: Vec::new(),
        probing_identity: false,
        nested_lazy_imports: Vec::new(),
        uncertainties: Vec::new(),
        fixtures: BTreeMap::new(),
        direct_arguments: BTreeMap::new(),
        executor_arguments: BTreeMap::new(),
        parameter_calls: Vec::new(),
        has_ordering_hooks: false,
        has_equality_hooks: false,
        has_hash_hooks: false,
        has_bool_hooks: false,
        has_length_hooks: false,
        grep_verify: verification.is_some(),
        dynamic_references: BTreeSet::new(),
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
    for symbol in &graph.symbols {
        if graph.scopes[symbol.scope]
            .owner
            .is_some_and(|id| graph.is_class(id))
        {
            match symbol.name.text.as_str() {
                "__lt__" | "__le__" | "__gt__" | "__ge__" => graph.has_ordering_hooks = true,
                "__eq__" => graph.has_equality_hooks = true,
                "__hash__" => graph.has_hash_hooks = true,
                "__bool__" => graph.has_bool_hooks = true,
                "__len__" => graph.has_length_hooks = true,
                _ => {}
            }
        }
    }
    graph.index_submodule_imports();
    graph.index_base_classes();
    for scope in 0..graph.scopes.len() {
        let body = graph.scopes[scope].body;
        graph.suite(scope, body);
    }
    graph.resolve_receiver_calls(false);
    graph.resolve_class_invocations();
    let callbacks = graph.initialization_callbacks();
    graph.resolve_receiver_calls(true);
    graph.resolve_class_invocations();
    let Some(initialization_edges) = graph.acyclic_initialization_edges(&callbacks) else {
        graph.issues.push(Issue::new(
            "cyclic_imports",
            ".",
            "cyclic module initialization cannot use final module bindings to prove non-use",
        ));
        return Analysis {
            issues: graph.issues,
            ..Analysis::default()
        };
    };
    let mut roots = graph.external_roots(entrypoints, &initialization_edges);
    if let Some(package_entrypoints) = package_entrypoints {
        graph.retain_package_entrypoints(package_entrypoints, &initialization_edges);
    }
    roots.extend(graph.test_edges());
    // A dormant consumer must not consume another scope's export traversal.
    for exports in std::mem::take(&mut graph.namespace_exports).into_values() {
        graph.retain_exports(exports, &initialization_edges, "possible");
    }
    let mut possible_runtime_roots = Vec::new();
    let (observed, rejected) = runtime
        .filter(|snapshot| !snapshot.observations.is_empty())
        .map_or_else(Default::default, |snapshot| {
            graph.runtime_roots(snapshot, &mut roots, &mut possible_runtime_roots)
        });
    let mut analysis = graph.finish(roots, possible_runtime_roots, verification);
    analysis.runtime_evidence = observed;
    analysis.runtime_evidence_issues = rejected;
    analysis
}

struct Graph<'a> {
    files: &'a [AnalyzedFile],
    modules: BTreeMap<&'a str, usize>,
    scopes: Vec<Scope<'a>>,
    symbols: Vec<Symbol<'a>>,
    direct_submodule_imports: BTreeMap<usize, Vec<SymbolId>>,
    by_location: BTreeMap<(ScopeId, usize, usize), SymbolId>,
    read: BTreeSet<SymbolId>,
    namespace_exports: BTreeMap<ScopeId, Vec<Export>>,
    direct: BTreeSet<SymbolId>,
    conditional: BTreeMap<SymbolId, Vec<RangeInclusive<[usize; 2]>>>,
    conditional_groups: BTreeMap<SymbolId, Vec<(usize, bool)>>,
    next_conditional_group: usize,
    dormant_classes: BTreeSet<SymbolId>,
    subclassed: BTreeSet<SymbolId>,
    instance_receivers: BTreeMap<SymbolId, SymbolId>,
    class_invocations: Vec<ClassInvocation>,
    method_receivers: BTreeMap<SymbolId, BTreeSet<SymbolId>>,
    receiver_calls: Vec<ReceiverCall>,
    resolved_receiver_calls: BTreeSet<(usize, SymbolId)>,
    receiver_call_issues: BTreeSet<usize>,
    issues: Vec<Issue>,
    scope_issues: Vec<(ScopeId, Issue)>,
    probing_identity: bool,
    nested_lazy_imports: Vec<(ScopeId, &'a [Statement])>,
    uncertainties: Vec<Issue>,
    fixtures: BTreeMap<SymbolId, PytestFixture>,
    direct_arguments: BTreeMap<SymbolId, Vec<DirectArguments>>,
    executor_arguments: BTreeMap<SymbolId, Vec<DirectArguments>>,
    parameter_calls: Vec<ParameterCall>,
    has_ordering_hooks: bool,
    has_equality_hooks: bool,
    has_hash_hooks: bool,
    has_bool_hooks: bool,
    has_length_hooks: bool,
    grep_verify: bool,
    dynamic_references: BTreeSet<(ScopeId, SymbolId, usize, usize)>,
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
            namespace_reads: BTreeMap::new(),
            namespace_copy_cutoffs: BTreeMap::new(),
            namespace_captures: OnceCell::new(),
            namespace_exclusions: Vec::new(),
            namespace_escape_exclusions: BTreeMap::new(),
            edges: Vec::new(),
            imports: Vec::new(),
            evaluated_imports: Vec::new(),
            initializer_imports: Vec::new(),
            class_bodies: Vec::new(),
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
                    if let Some(enabled) = static_bool(test) {
                        self.index(scope, if enabled { body } else { otherwise });
                        continue;
                    }
                    let first_symbol = self.symbols.len();
                    match self.indexed_bool(scope, test, statement.at, 0) {
                        Some(true) => {
                            self.index(scope, body);
                            self.index_dormant(scope, otherwise);
                        }
                        Some(false) => {
                            self.index_dormant(scope, body);
                            self.index(scope, otherwise);
                        }
                        None => {
                            let group = self.next_conditional_group;
                            self.next_conditional_group += 1;
                            self.index(scope, body);
                            self.mark_conditional(scope, first_symbol, body, group, true);
                            let first_symbol = self.symbols.len();
                            self.index(scope, otherwise);
                            self.mark_conditional(scope, first_symbol, otherwise, group, false);
                        }
                    }
                }
                _ => {}
            }
        }
    }

    fn indexed_bool(
        &self,
        scope: ScopeId,
        expression: &Expression,
        before: Location,
        depth: usize,
    ) -> Option<bool> {
        if depth >= MAX_BINDING_CHAIN {
            return None;
        }
        match &expression.kind {
            ExpressionKind::Bool(value) => Some(*value),
            ExpressionKind::Not(value) => self
                .indexed_bool(scope, value, before, depth + 1)
                .map(|value| !value),
            // Module guards can use prior local bindings during indexing.
            // Nested scopes can depend on bindings that are still being indexed.
            ExpressionKind::Name(name) if self.scopes[scope].file == scope => {
                let id = self.local(scope, name, before)?;
                if self.binding_depends_on_branch(id, before) {
                    return None;
                }
                let Binding::Variable(Some(value), assigned) = self.symbols[id].binding else {
                    return None;
                };
                self.indexed_bool(scope, value, assigned, depth + 2)
            }
            _ => None,
        }
    }

    fn index_dormant(&mut self, scope: ScopeId, body: &'a [Statement]) {
        let first = self.symbols.len();
        self.index(scope, body);
        // These symbols are still reportable. They never change the module's
        // effective bindings at runtime.
        let names = &mut self.scopes[scope].names;
        for id in (first..self.symbols.len()).rev() {
            let symbol = &self.symbols[id];
            if matches!(symbol.binding, Binding::Definition(definition, _)
                if definition.kind == DefinitionKind::Class)
            {
                self.dormant_classes.insert(id);
            }
            if symbol.scope == scope
                && let Entry::Occupied(mut entry) = names.entry(symbol.name.text.as_str())
                && entry.get().last() == Some(&id)
            {
                entry.get_mut().pop();
                if entry.get().is_empty() {
                    entry.remove();
                }
            }
        }
    }

    fn mark_conditional(
        &mut self,
        scope: ScopeId,
        first_symbol: usize,
        body: &[Statement],
        group: usize,
        branch: bool,
    ) {
        let Some(first) = body.first() else { return };
        let end = suite_end(body);
        for id in first_symbol..self.symbols.len() {
            if self.symbols[id].scope == scope {
                self.conditional_groups
                    .entry(id)
                    .or_default()
                    .push((group, branch));
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

    fn scope_issue(
        &mut self,
        scope: ScopeId,
        at: Location,
        kind: &'static str,
        message: impl ToString,
    ) {
        // Speculative identity lookups can inform the graph. A runtime
        // diagnostic requires an evaluated read.
        if self.probing_identity {
            return;
        }
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
        match self.lexical_binding(scope, name, before, false) {
            LexicalBinding::Found(id) => Some(id),
            LexicalBinding::UnboundLocal | LexicalBinding::Missing => None,
        }
    }

    fn lexical_binding(
        &self,
        origin: ScopeId,
        name: &str,
        before: Location,
        check_dormant_declarations: bool,
    ) -> LexicalBinding {
        let mut scope = origin;
        let mut before = before;
        loop {
            let row = &self.scopes[scope];
            let class = row.owner.is_some_and(|id| self.is_class(id));
            // A class body sees its own namespace, but nested scopes do not
            // capture a class namespace as an enclosing lexical scope.
            if scope == origin || !class {
                if let Some(id) = self.local(scope, name, before) {
                    return LexicalBinding::Found(id);
                }
                if row.owner.is_some()
                    && !class
                    && (row.names.contains_key(name)
                        || (check_dormant_declarations || matches!(name, "locals" | "vars"))
                            && suite_binds_name(row.body, name))
                {
                    // A lexical declaration can shadow an outer binding even
                    // in a branch omitted from the runtime binding index.
                    return LexicalBinding::UnboundLocal;
                }
            }
            let Some(parent) = row.parent else {
                return LexicalBinding::Missing;
            };
            scope = parent;
            before = END;
        }
    }

    fn is_class(&self, id: SymbolId) -> bool {
        matches!(self.symbols[id].binding, Binding::Definition(def, _) if def.kind == DefinitionKind::Class)
    }

    fn index_base_classes(&mut self) {
        for id in 0..self.symbols.len() {
            let scope = self.symbols[id].scope;
            let Binding::Definition(definition, _) = self.symbols[id].binding else {
                continue;
            };
            if definition.kind == DefinitionKind::Class && !self.dormant_classes.contains(&id) {
                for base in &definition.bases {
                    if let Value::Callable(Invocation { target: base, .. }) =
                        self.probe_identity(scope, base, definition.name.at)
                        && self.is_class(base)
                    {
                        self.subclassed.insert(base);
                    }
                }
            }
        }
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

    fn import_member_position(
        &self,
        scope: ScopeId,
        module: &str,
        import: &Import,
    ) -> Option<Location> {
        let file = self.scopes[scope].file;
        (scope == file && import.level > 0 && self.files[file].module == module)
            .then_some(import.local.at)
    }

    fn index_submodule_imports(&mut self) {
        for (id, symbol) in self.symbols.iter().enumerate() {
            let Binding::Import(import) = symbol.binding else {
                continue;
            };
            if !self.scopes[symbol.scope]
                .names
                .get(symbol.name.text.as_str())
                .is_some_and(|ids| ids.binary_search(&id).is_ok())
            {
                continue;
            }
            let source = self.scopes[symbol.scope].file;
            let Some(module) = self.relative_module(source, import) else {
                continue;
            };
            let unconditional = symbol.scope == source && !self.conditional.contains_key(&id);
            let module_file = self.modules.get(module.as_str()).copied();
            let fallback =
                module_file
                    .zip(import.member.as_deref())
                    .and_then(|(package, member)| {
                        self.fallback_submodule(package, member, import, symbol.scope)
                    });
            let loaded_child = fallback
                .filter(|_| module_file.is_some_and(|package| self.files[package].is_package));
            let loads_module = unconditional
                && module_file.is_some_and(|file| {
                    loaded_child.is_some()
                        || import.member.as_ref().is_none_or(|member| {
                            self.local(file, member, END)
                                .is_some_and(|binding| !self.conditional.contains_key(&binding))
                        })
                });
            for end in module
                .match_indices('.')
                .map(|(end, _)| end)
                .chain(std::iter::once(module.len()))
            {
                let name = &module[..end];
                let Some(&target) = self.modules.get(name) else {
                    continue;
                };
                if name.contains('.') {
                    self.direct_submodule_imports
                        .entry(target)
                        .or_default()
                        .push(id);
                }
                // A successful dotted import also runs its parent packages.
                // The source and its own ancestors may still be initializing.
                if loads_module
                    && (end == module.len() || self.files[target].is_package)
                    && !within_package(&self.files[source].module, name)
                {
                    self.scopes[source].initializer_imports.push((id, target));
                }
            }
            if let Some(child) = fallback {
                self.direct_submodule_imports
                    .entry(child)
                    .or_default()
                    .push(id);
            }
            if unconditional
                && let Some(child) = loaded_child
                && child != source
            {
                self.scopes[source].initializer_imports.push((id, child));
            }
        }
    }

    fn fallback_submodule(
        &self,
        package: usize,
        member: &str,
        import: &Import,
        scope: ScopeId,
    ) -> Option<usize> {
        let file = &self.files[package];
        let child = self
            .modules
            .get(format!("{}.{member}", file.module).as_str())?;
        let before = self
            .import_member_position(scope, &file.module, import)
            .unwrap_or(END);
        // From-import can load a child when ordinary and lazy lookup fail.
        (!has_implicit_module_member(member, file.is_package)
            && self.local(package, member, before).is_none()
            && self
                .local(package, "__getattr__", before)
                .is_none_or(|hook| {
                    matches!(self.lazy_lookup(hook, member, before), LazyMember::Missing)
                }))
        .then_some(*child)
    }

    fn submodule_attachment(
        &self,
        package: usize,
        child: usize,
        binding: Option<SymbolId>,
        access: (ScopeId, Location),
        mode: ModuleMemberAccess<'_>,
    ) -> SubmoduleAttachment {
        let candidates = self.direct_submodule_imports.get(&child);
        let initialization_imports = match mode {
            ModuleMemberAccess::Export {
                initialization_edges,
                initialization_imports,
                ..
            } if candidates.is_some() => Some(
                initialization_imports
                    .get_or_init(|| self.initialization_imports(package, initialization_edges)),
            ),
            _ => None,
        };
        let (scope, before) = access;
        let mut attached: Option<SymbolId> = None;
        let mut possible_attachment = false;
        let mut possible_earlier_attachment = false;
        for &id in candidates.into_iter().flatten() {
            if initialization_imports.is_some_and(|imports| !imports.contains(&id)) {
                continue;
            }
            let symbol = &self.symbols[id];
            let earlier_in_scope =
                symbol.scope == scope && position(symbol.name.at) < position(before);
            let guaranteed = if earlier_in_scope {
                self.conditional
                    .get(&id)
                    .is_none_or(|guards| guards.iter().all(|span| span.contains(&position(before))))
            } else {
                symbol.scope == package
                    && self.scopes[scope].file != package
                    && !self.conditional.contains_key(&id)
            };
            // Other scopes may already have executed. A later statement in
            // this module's own initializer has not yet attached the child.
            let possible = symbol.scope != scope || scope >= self.files.len() || earlier_in_scope;
            if binding.is_some_and(|binding| {
                symbol.scope == package
                    && position(symbol.name.at) <= position(self.symbols[binding].name.at)
            }) {
                // The first successful child import sets the package attribute.
                // A later import from the module cache leaves a reassignment in place.
                if guaranteed {
                    return SubmoduleAttachment::Rebound(id);
                }
                possible_earlier_attachment |= possible;
            } else {
                if guaranteed
                    && attached.is_none_or(|first| {
                        symbol.scope == package && self.symbols[first].scope != package
                    })
                {
                    attached = Some(id);
                }
                possible_attachment |= possible;
            }
        }
        if let Some(import) = attached
            && !possible_earlier_attachment
        {
            SubmoduleAttachment::Attached(import)
        } else if possible_attachment {
            SubmoduleAttachment::Ambiguous
        } else {
            SubmoduleAttachment::Absent
        }
    }

    fn imported_submodule(
        &self,
        child: usize,
        access: (ScopeId, Location),
    ) -> Option<Vec<SymbolId>> {
        let (source, before) = access;
        if source >= self.files.len() {
            return None;
        }
        let attachments: BTreeMap<_, _> = self
            .direct_submodule_imports
            .get(&child)?
            .iter()
            .copied()
            .filter(|id| !self.conditional.contains_key(id))
            .map(|id| (self.symbols[id].scope, id))
            .collect();
        // Follow earlier imports through completed module initializers. Each
        // link matters because deleting it could remove the attachment.
        let mut pending = VecDeque::from([source]);
        let mut via = BTreeMap::new();
        while let Some(module) = pending.pop_front() {
            if module != source
                && let Some(&attachment) = attachments.get(&module)
            {
                let mut evidence = vec![attachment];
                let mut current = module;
                while let Some(&(caller, import)) = via.get(&current) {
                    evidence.push(import);
                    current = caller;
                }
                return Some(evidence);
            }
            for &(import, target) in &self.scopes[module].initializer_imports {
                if target != source
                    && (module != source
                        || position(self.symbols[import].name.at) < position(before))
                    && let std::collections::btree_map::Entry::Vacant(entry) = via.entry(target)
                {
                    entry.insert((module, import));
                    pending.push_back(target);
                }
            }
        }
        None
    }

    fn record_import(
        &mut self,
        scope: ScopeId,
        source_scope: ScopeId,
        module: Option<&str>,
        import: &Import,
    ) -> Option<LazyBranch<'a>> {
        let source = self.scopes[source_scope].file;
        if let Some(&id) =
            self.by_location
                .get(&(source_scope, import.local.at.line, import.local.at.column))
        {
            // A selected lazy branch evaluates its imports in the caller's
            // execution, even though their bindings belong to the hook.
            self.scopes[scope].evaluated_imports.push(id);
        }
        let Some(module) = module else {
            self.import_issue(
                scope,
                source,
                import,
                "relative import escapes its selected package".into(),
            );
            return None;
        };
        let own_module = &self.files[source].module;
        let module_file = self.modules.get(module).copied();
        let before = self.import_member_position(source_scope, module, import);
        let member = import.member.as_deref();
        let lookup = module_file.zip(member).map(|(file, member)| {
            self.selected_module_member(
                file,
                member,
                before.unwrap_or(END),
                (source_scope, import.local.at),
                ModuleMemberAccess::FromImport,
            )
        });
        let child = match &lookup {
            Some(ModuleMember::Submodule(child, _)) => Some(*child),
            None => member.and_then(|member| {
                self.modules
                    .get(format!("{module}.{member}").as_str())
                    .copied()
            }),
            _ => None,
        };
        if import.level > 0 && module_file.is_none() && child.is_none() {
            self.import_issue(
                scope,
                source,
                import,
                "relative import has no selected module".into(),
            );
            return None;
        }
        // Package self-imports can read the bindings already established at
        // this statement. Sibling submodule imports also need no final parent state.
        if let Some(target) = module_file
            && before.is_none()
            && !(child.is_some() && within_package(own_module, module))
        {
            self.scopes[scope].imports.push(target);
        }
        if let Some(child) = child {
            self.scopes[scope].imports.push(child);
        }
        // Importing a dotted module can also execute its package initializers.
        let mut has_selected_parent = false;
        for (end, _) in module.match_indices('.') {
            let parent = &module[..end];
            if let Some(&target) = self.modules.get(parent) {
                has_selected_parent = true;
                if !within_package(own_module, parent) {
                    self.scopes[scope].imports.push(target);
                }
            }
        }
        if import.level == 0 && module_file.is_none() && has_selected_parent {
            self.import_issue(
                scope,
                source,
                import,
                format!("no source selected for submodule {module:?}"),
            );
        }
        if let (Some(member), Some(lookup)) = (member, lookup) {
            match lookup {
                ModuleMember::Lazy(_, branch) => return Some(branch),
                ModuleMember::Missing => self.import_issue(
                    scope,
                    source,
                    import,
                    format!("{module:?} has no selected binding for {member:?}"),
                ),
                ModuleMember::Binding(id, _)
                    if self.binding_depends_on_branch(id, before.unwrap_or(END)) =>
                {
                    // A from-import reads the member even if the local alias
                    // is used only by an external collector such as pytest.
                    self.scope_issues.push((
                        scope,
                        Issue::new(
                            "ambiguous_binding",
                            &self.files[source].path,
                            format!("import of {module}.{member} depends on an unresolved branch"),
                        )
                        .at(import.local.at),
                    ));
                }
                ModuleMember::Ambiguous => self.scope_issues.push((
                    scope,
                    Issue::new("ambiguous_binding", &self.files[source].path,
                        format!("cannot establish whether submodule {module}.{member} was attached before this lookup"))
                        .at(import.local.at),
                )),
                ModuleMember::ConditionalHook => self.scope_issues.push((
                    scope,
                    Issue::new(
                        "ambiguous_binding",
                        &self.files[source].path,
                        format!("lazy import of {module}.{member} depends on a conditional __getattr__"),
                    )
                    .at(import.local.at),
                )),
                ModuleMember::OpaqueHook => self.scope_issues.push((
                    scope,
                    Issue::new(
                        "unresolved_lazy_export",
                        &self.files[source].path,
                        format!("lazy import of {module}.{member} is outside the bounded __getattr__ model"),
                    )
                    .at(import.local.at),
                )),
                _ => {}
            }
        }
        None
    }

    fn import_issue(&mut self, scope: ScopeId, file: usize, import: &Import, message: String) {
        self.scope_issues.push((
            scope,
            Issue::new("unresolved_import", &self.files[file].path, message).at(import.local.at),
        ));
    }

    fn execute_lazy_branch(&mut self, scope: ScopeId, branch: &LazyBranch<'a>) -> Value {
        // Keep imports discovered through return expressions on the same
        // worklist as nested from-imports. All execute for the original caller.
        let value = self.lazy_value(scope, branch, 0, Some(scope));
        let mut pending = vec![(branch.scope, branch.imports)];
        pending.append(&mut self.nested_lazy_imports);
        let mut visited = BTreeSet::new();
        while let Some((handler, imports)) = pending.pop() {
            let file = self.scopes[handler].file;
            for statement in imports {
                let StatementKind::Import(import) = &statement.kind else {
                    continue;
                };
                if !visited.insert((handler, position(import.local.at))) {
                    continue;
                }
                let module = self.relative_module(file, import);
                if let Some(branch) = self.record_import(scope, handler, module.as_deref(), import)
                {
                    self.lazy_value(scope, &branch, 0, Some(scope));
                    pending.push((branch.scope, branch.imports));
                    pending.append(&mut self.nested_lazy_imports);
                }
            }
        }
        value
    }

    fn synchronous_calls(&self, scope: ScopeId) -> impl Iterator<Item = ScopeId> {
        self.scopes[scope].edges.iter().filter_map(|edge| {
            if matches!(
                edge.kind,
                "direct_call"
                    | "constructor"
                    | "decorator"
                    | "subclass_hook"
                    | "truth_test"
                    | "length_call"
                    | "grep_dynamic_call"
            ) && let Binding::Definition(_, body) = self.symbols[edge.target].binding
            {
                Some(body)
            } else {
                None
            }
        })
    }

    fn initialization_callbacks(&mut self) -> Vec<Vec<ScopeId>> {
        let mut invocations = BTreeMap::<_, Vec<_>>::new();
        for call in &self.parameter_calls {
            let parameter = &self.symbols[call.parameter];
            let owner = self.scopes[parameter.scope]
                .owner
                .expect("parameters belong to a definition");
            if self.direct_arguments.contains_key(&owner) {
                invocations
                    .entry((owner, parameter.scope))
                    .or_default()
                    .push(call);
            }
        }
        let mut callbacks = vec![Vec::new(); self.scopes.len()];
        let mut constructors = Vec::new();
        for ((callee, body), invocations) in invocations {
            // A captured parameter call matters only in a nested body that
            // can execute during this invocation of the enclosing function.
            let mut reached = BTreeSet::new();
            if invocations.iter().any(|call| call.scope != body) {
                let mut pending = vec![body];
                while let Some(scope) = pending.pop() {
                    if reached.insert(scope) {
                        pending.extend(&self.scopes[scope].class_bodies);
                        pending.extend(self.synchronous_calls(scope));
                    }
                }
            }
            let parameters: BTreeSet<_> = invocations
                .into_iter()
                .filter(|call| call.scope == body || reached.contains(&call.scope))
                .map(|call| self.symbols[call.parameter].name.text.as_str())
                .collect();
            for call in &self.direct_arguments[&callee] {
                for parameter in &parameters {
                    if let Some(&Value::Callable(invocation)) = call.values.get(*parameter)
                        && !self.receiver_conflicts(invocation)
                        && let Binding::Definition(definition, body) =
                            self.symbols[invocation.target].binding
                    {
                        match definition.kind {
                            DefinitionKind::Function => callbacks[call.caller].push(body),
                            DefinitionKind::Class => {
                                constructors.push((call.caller, invocation.target, call.at));
                            }
                            DefinitionKind::Coroutine => {}
                        }
                    }
                }
            }
        }
        for (caller, class, at) in constructors {
            self.constructor(caller, class, "constructor", "possible", at);
        }
        callbacks
    }

    fn acyclic_initialization_edges(
        &self,
        callbacks: &[Vec<ScopeId>],
    ) -> Option<Vec<Vec<ScopeId>>> {
        let count = self.scopes.len();
        let mut outgoing = vec![Vec::new(); count];
        let mut incoming = vec![Vec::new(); count];
        let mut reached = vec![false; count];
        let mut finished = Vec::with_capacity(count);
        let mut pending: Vec<_> = (0..self.files.len()).map(|file| (file, false)).collect();
        while let Some((id, exiting)) = pending.pop() {
            if exiting {
                finished.push(id);
                continue;
            }
            if std::mem::replace(&mut reached[id], true) {
                continue;
            }
            pending.push((id, true));
            let scope = &self.scopes[id];
            for target in scope
                .imports
                .iter()
                .chain(&scope.class_bodies)
                .chain(&callbacks[id])
                .copied()
                .chain(self.synchronous_calls(id))
            {
                incoming[target].push(id);
                outgoing[id].push(target);
                if !reached[target] {
                    pending.push((target, false));
                }
            }
        }
        // Reverse finishing order gives the strongly connected components.
        // A recursive call alone is valid. A component with an import edge
        // can consult a module before its final bindings are established.
        let mut component = vec![usize::MAX; count];
        let mut pending = Vec::new();
        for &root in finished.iter().rev() {
            pending.push(root);
            while let Some(id) = pending.pop() {
                if component[id] == usize::MAX {
                    component[id] = root;
                    pending.extend(&incoming[id]);
                }
            }
        }
        if finished.iter().any(|&id| {
            self.scopes[id]
                .imports
                .iter()
                .any(|&target| component[id] == component[target])
        }) {
            return None;
        }
        Some(outgoing)
    }

    fn initialization_imports(
        &self,
        file: usize,
        initialization_edges: &[Vec<ScopeId>],
    ) -> BTreeSet<SymbolId> {
        let mut imports = BTreeSet::new();
        let mut reached = BTreeSet::new();
        let mut pending = vec![file];
        while let Some(scope) = pending.pop() {
            if reached.insert(scope) {
                imports.extend(&self.scopes[scope].evaluated_imports);
                pending.extend(&initialization_edges[scope]);
            }
        }
        imports
    }

    fn binding_depends_on_branch(&self, id: SymbolId, at: Location) -> bool {
        self.conditional
            .get(&id)
            .is_some_and(|guards| guards.iter().any(|span| !span.contains(&position(at))))
    }

    fn binding(&mut self, id: SymbolId, before: Location, depth: usize) -> Value {
        let scope = self.symbols[id].scope;
        let at = self.symbols[id].name.at;
        if depth >= MAX_BINDING_CHAIN {
            self.issue(
                self.scopes[scope].file,
                at,
                "analysis_limit",
                "binding resolution limit exceeded",
            );
            return Value::Unknown;
        }
        self.read.insert(id);
        if self.binding_depends_on_branch(id, before) {
            if let Some(value) = self.finite_conditional_value(id, before, depth + 1) {
                return value;
            }
            self.scope_issue(
                scope,
                at,
                "ambiguous_binding",
                "a required binding depends on an unresolved branch",
            );
            return Value::Unknown;
        }
        self.binding_value(id, depth)
    }

    fn finite_conditional_value(
        &mut self,
        id: SymbolId,
        before: Location,
        depth: usize,
    ) -> Option<Value> {
        let symbol = &self.symbols[id];
        let candidates = self.scopes[symbol.scope]
            .names
            .get(symbol.name.text.as_str())?;
        let mut eligible = Vec::new();
        for &candidate in candidates {
            if position(self.symbols[candidate].name.at) >= position(before) {
                break;
            }
            eligible.push(candidate);
        }
        if !self.conditional_groups.contains_key(&id) {
            return None;
        }
        if let Some(fallback) = eligible
            .iter()
            .rposition(|candidate| !self.conditional_groups.contains_key(candidate))
        {
            eligible.drain(..fallback);
        }
        let groups = eligible
            .iter()
            .filter_map(|candidate| self.conditional_groups.get(candidate))
            .flatten()
            .map(|(group, _)| *group)
            .collect::<BTreeSet<_>>();
        let group_bits = groups
            .into_iter()
            .enumerate()
            .map(|(bit, group)| (group, bit))
            .collect::<BTreeMap<_, _>>();
        let states = 1usize.checked_shl(group_bits.len() as u32)?;
        if states > MAX_CONDITIONAL_STATES {
            return None;
        }
        let mut selected = BTreeSet::new();
        for state in 0..states {
            let candidate = eligible.iter().rev().copied().find(|candidate| {
                self.conditional_groups
                    .get(candidate)
                    .is_none_or(|conditions| {
                        conditions.iter().all(|(group, branch)| {
                            let bit = group_bits[group];
                            ((state >> bit) & 1 == 1) == *branch
                        })
                    })
            })?;
            selected.insert(candidate);
        }
        let mut values = Vec::new();
        for candidate in selected {
            self.read.insert(candidate);
            let value = self.binding_value(candidate, depth + 1);
            if value.contains_unknown() {
                return None;
            }
            values.push(value);
        }
        Some(Value::alternatives(values))
    }

    fn binding_value(&mut self, id: SymbolId, depth: usize) -> Value {
        let symbol = &self.symbols[id];
        let scope = symbol.scope;
        let file = self.scopes[scope].file;
        match symbol.binding {
            Binding::Definition(_, _) => Value::Callable(Invocation::ordinary(id)),
            Binding::Variable(Some(value), before) => {
                self.identity(scope, value, before, depth + 1)
            }
            Binding::Variable(None, _) => Value::Unknown,
            Binding::Parameter => {
                if let Some(&class) = self.instance_receivers.get(&id) {
                    return if self.subclassed.contains(&class) {
                        Value::ReceiverParameter(id)
                    } else {
                        Value::Instance(class)
                    };
                }
                Value::Unknown
            }
            Binding::Import(import) => {
                let Some(module) = self.relative_module(file, import) else {
                    return Value::Unknown;
                };
                if let Some(member) = import.member.as_deref() {
                    let before = self
                        .import_member_position(scope, &module, import)
                        .unwrap_or(END);
                    self.module_member_at(
                        &module,
                        member,
                        before,
                        (scope, import.local.at),
                        ModuleMemberAccess::FromImport,
                        depth + 1,
                    )
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

    fn selected_module_member(
        &self,
        file: usize,
        member: &str,
        before: Location,
        access: (ScopeId, Location),
        mode: ModuleMemberAccess<'_>,
    ) -> ModuleMember<'a> {
        let submodule = format!("{}.{member}", self.files[file].module);
        let child = self.modules.get(submodule.as_str()).copied();
        if let Some(binding) = self.local(file, member, before) {
            if let Some(child) = child {
                return match self.submodule_attachment(file, child, Some(binding), access, mode) {
                    SubmoduleAttachment::Attached(import) => {
                        ModuleMember::Submodule(child, vec![import])
                    }
                    SubmoduleAttachment::Rebound(import) => {
                        ModuleMember::Binding(binding, Some(import))
                    }
                    SubmoduleAttachment::Ambiguous => ModuleMember::Ambiguous,
                    SubmoduleAttachment::Absent => ModuleMember::Binding(binding, None),
                };
            }
            return ModuleMember::Binding(binding, None);
        }
        if has_implicit_module_member(member, self.files[file].is_package) {
            return ModuleMember::Unknown;
        }
        let hook = self.local(file, "__getattr__", before);
        let lazy = hook.map_or(LazyMember::Missing, |hook| {
            self.lazy_lookup(hook, member, before)
        });
        if let Some(child) = child {
            match self.submodule_attachment(file, child, None, access, mode) {
                SubmoduleAttachment::Attached(import) => {
                    return ModuleMember::Submodule(child, vec![import]);
                }
                _ if matches!(
                    mode,
                    ModuleMemberAccess::FromImport | ModuleMemberAccess::Export { .. }
                ) && matches!(lazy, LazyMember::Missing) =>
                {
                    return ModuleMember::Submodule(child, Vec::new());
                }
                SubmoduleAttachment::Absent if matches!(lazy, LazyMember::Missing) => {
                    return ModuleMember::UnattachedSubmodule;
                }
                SubmoduleAttachment::Absent => {}
                SubmoduleAttachment::Ambiguous => {
                    return self
                        .imported_submodule(child, access)
                        .map_or(ModuleMember::Ambiguous, |imports| {
                            ModuleMember::Submodule(child, imports)
                        });
                }
                SubmoduleAttachment::Rebound(_) => return ModuleMember::Ambiguous,
            }
        }
        match (hook, lazy) {
            (Some(hook), LazyMember::Branch(branch)) => ModuleMember::Lazy(hook, branch),
            (_, LazyMember::Missing) => ModuleMember::Missing,
            (_, LazyMember::Conditional) => ModuleMember::ConditionalHook,
            (_, LazyMember::Unknown) => ModuleMember::OpaqueHook,
            _ => ModuleMember::Unknown,
        }
    }

    fn module_member_at(
        &mut self,
        module: &str,
        member: &str,
        before: Location,
        access: (ScopeId, Location),
        mode: ModuleMemberAccess<'_>,
        depth: usize,
    ) -> Value {
        let submodule = format!("{module}.{member}");
        if depth >= MAX_BINDING_CHAIN {
            return Value::Unknown;
        }
        let (scope, at) = match mode {
            ModuleMemberAccess::Export { at, .. } => (access.0, at),
            _ => access,
        };
        if let Some(&file) = self.modules.get(module) {
            return match self.selected_module_member(file, member, before, access, mode) {
                ModuleMember::Binding(id, import) => {
                    self.read.extend(import);
                    self.binding(id, before, depth + 1)
                }
                ModuleMember::Submodule(file, import) => {
                    self.read.extend(import);
                    Value::Module(file)
                }
                ModuleMember::Lazy(id, branch) => {
                    self.read.insert(id);
                    self.lazy_value(scope, &branch, depth + 2, None)
                }
                ModuleMember::Ambiguous => {
                    self.scope_issue(scope, at, "ambiguous_binding",
                        format!("cannot establish whether submodule {submodule} was attached before this lookup"));
                    Value::Unknown
                }
                ModuleMember::ConditionalHook => {
                    self.scope_issue(
                        scope,
                        at,
                        "ambiguous_binding",
                        format!("lazy lookup of {submodule} depends on a conditional __getattr__"),
                    );
                    Value::Unknown
                }
                ModuleMember::OpaqueHook => {
                    self.scope_issue(
                        scope,
                        at,
                        "unresolved_lazy_export",
                        format!(
                            "lazy lookup of {submodule} is outside the bounded __getattr__ model"
                        ),
                    );
                    Value::Unknown
                }
                ModuleMember::UnattachedSubmodule => {
                    self.scope_issue(
                        scope,
                        at,
                        "unresolved_attribute",
                        format!("submodule {submodule} is not attached to package {module:?}"),
                    );
                    Value::Unknown
                }
                ModuleMember::Missing if matches!(mode, ModuleMemberAccess::Attribute) => {
                    self.scope_issue(
                        scope,
                        at,
                        "unresolved_attribute",
                        format!("module {module:?} has no selected binding for {member:?}"),
                    );
                    Value::Unknown
                }
                ModuleMember::Missing | ModuleMember::Unknown => Value::Unknown,
            };
        }
        self.modules
            .get(submodule.as_str())
            .map_or(Value::Provider(submodule), |file| Value::Module(*file))
    }

    fn lazy_value(
        &mut self,
        caller: ScopeId,
        branch: &LazyBranch<'a>,
        depth: usize,
        execution_scope: Option<ScopeId>,
    ) -> Value {
        let first_issue = self.scope_issues.len();
        let evaluation = ValueEvaluation::Lazy {
            execution_scope,
            argument: branch.argument,
        };
        let value = self.expression_value(branch.scope, branch.value, branch.at, depth, evaluation);
        if value.contains_unknown() && self.scope_issues.len() == first_issue {
            self.scope_issue(
                branch.scope,
                branch.value.at,
                "unresolved_lazy_export",
                "lazy return value or its evaluation is outside the bounded model",
            );
        }
        // A member access executes only the selected lazy branch. Keep the
        // original diagnostic locations and use the caller for reachability.
        for (owner, _) in &mut self.scope_issues[first_issue..] {
            *owner = caller;
        }
        value
    }

    fn lazy_call(
        &mut self,
        scope: ScopeId,
        expression: &Expression,
        before: Location,
        depth: usize,
        evaluation: ValueEvaluation<'a>,
    ) -> Value {
        let ExpressionKind::Call {
            function,
            arguments,
            keywords,
        } = &expression.kind
        else {
            unreachable!()
        };
        let ValueEvaluation::Lazy {
            execution_scope, ..
        } = evaluation
        else {
            unreachable!()
        };
        let recurse = |this: &mut Self, expression| {
            this.expression_value(scope, expression, before, depth + 1, evaluation)
        };
        let plain_hook = self.scopes[scope].owner.is_some_and(|id| {
            matches!(self.symbols[id].binding, Binding::Definition(definition, _) if definition.decorators.is_empty())
        });
        if plain_hook && keywords.is_empty() {
            match (recurse(self, function), arguments.as_slice()) {
                (Value::Provider(provider), [base, name]) if provider == "builtins.getattr" => {
                    if let (base @ Value::Module(_), Value::Text(name)) =
                        (recurse(self, base), recurse(self, name))
                    {
                        return self.lazy_attribute(
                            base,
                            &name,
                            (scope, before),
                            depth + 1,
                            execution_scope,
                        );
                    }
                }
                (Value::Provider(provider), [name]) if provider == "importlib.import_module" => {
                    if let Value::Text(name) = recurse(self, name)
                        && let Some(&target) = self.modules.get(name.as_str())
                        && !within_package(&self.files[self.scopes[scope].file].module, &name)
                    {
                        // An omitted initializer can change the imported module or
                        // prevent it from loading. This path requires selected packages.
                        let parents: Option<Vec<_>> = name
                            .match_indices('.')
                            .map(|(end, _)| self.lazy_import_parent(&name, end))
                            .collect();
                        if let Some(parents) = parents {
                            if let Some(caller) = execution_scope {
                                let caller_module = &self.files[self.scopes[caller].file].module;
                                self.scopes[caller]
                                    .imports
                                    .extend(parents.into_iter().filter(|&file| {
                                        !within_package(caller_module, &self.files[file].module)
                                    }));
                                self.scopes[caller].imports.push(target);
                            }
                            return Value::Module(target);
                        }
                    }
                }
                _ => {}
            }
        }
        self.scope_issue(
            scope,
            expression.at,
            "unresolved_lazy_export",
            "effects of the lazy return expression are outside the bounded model",
        );
        Value::Unknown
    }

    fn lazy_import_parent(&self, name: &str, end: usize) -> Option<usize> {
        let &parent = self.modules.get(&name[..end])?;
        let child = name[end + 1..].split('.').next()?;
        // Loading a child can replace an existing attribute on its package.
        // The static import attachment index cannot order this lazy effect.
        let replaces_search_path =
            self.scopes[parent]
                .names
                .get("__path__")
                .is_some_and(|bindings| {
                    bindings
                        .iter()
                        .any(|&id| !matches!(self.symbols[id].binding, Binding::Variable(None, _)))
                });
        if !self.files[parent].is_package
            || replaces_search_path
            || has_implicit_module_member(child, true)
            || self.local(parent, child, END).is_some()
            || self.local(parent, "__getattr__", END).is_some_and(|hook| {
                !matches!(self.lazy_lookup(hook, child, END), LazyMember::Missing)
            })
        {
            return None;
        }
        Some(parent)
    }

    fn lazy_attribute(
        &mut self,
        base: Value,
        name: &str,
        access: (ScopeId, Location),
        depth: usize,
        execution_scope: Option<ScopeId>,
    ) -> Value {
        if let Some(caller) = execution_scope
            && let Value::Module(file) = base
            && let ModuleMember::Lazy(hook, branch) =
                self.selected_module_member(file, name, END, access, ModuleMemberAccess::Attribute)
        {
            self.read.insert(hook);
            self.nested_lazy_imports
                .push((branch.scope, branch.imports));
            return self.lazy_value(access.0, &branch, depth + 2, Some(caller));
        }
        let value = self.member(base, name, access, depth);
        if let Some(caller) = execution_scope {
            self.record_class_reference(caller, &value);
        }
        value
    }

    fn lazy_lookup(&self, id: SymbolId, member: &str, before: Location) -> LazyMember<'a> {
        self.lazy_dispatch(id, before, |name, scope, body, argument| {
            (name == member).then(|| Self::lazy_branch(scope, body, argument))
        })
    }

    fn lazy_dispatch(
        &self,
        id: SymbolId,
        before: Location,
        mut visit: impl FnMut(
            &'a str,
            ScopeId,
            &'a [Statement],
            Option<(SymbolId, &'a str)>,
        ) -> Option<LazyMember<'a>>,
    ) -> LazyMember<'a> {
        if self.binding_depends_on_branch(id, before) {
            return LazyMember::Conditional;
        }
        let Binding::Definition(definition, scope) = self.symbols[id].binding else {
            return LazyMember::Unknown;
        };
        let [parameter] = definition.parameters.as_slice() else {
            return LazyMember::Unknown;
        };
        // Python supplies the lookup name as one positional string argument.
        if definition.kind != DefinitionKind::Function
            || !(definition.has_positional_parameter
                || definition.has_variadic_positional_parameter)
        {
            return LazyMember::Unknown;
        }
        let parameter_id = self
            .by_location
            .get(&(scope, parameter.at.line, parameter.at.column))
            .copied();
        // Prove literal dispatch only, after ordinary module lookup failed.
        for statement in &definition.body {
            if let StatementKind::Raise(Some(exception)) = &statement.kind {
                let target = match &exception.kind {
                    ExpressionKind::Call {
                        function,
                        arguments,
                        keywords,
                    } if definition.has_positional_parameter
                        || (keywords.is_empty()
                            && arguments.iter().all(|arg| match &arg.kind {
                                ExpressionKind::Text(_)
                                | ExpressionKind::Bool(_)
                                | ExpressionKind::Scalar => true,
                                ExpressionKind::Name(name) => name == &parameter.text,
                                _ => false,
                            })) =>
                    {
                        function
                    }
                    _ => exception,
                };
                if matches!(&target.kind, ExpressionKind::Name(name) if name == "AttributeError")
                    && self
                        .lexical(scope, "AttributeError", statement.at)
                        .is_none()
                    && !self.scopes[scope].names.contains_key("AttributeError")
                {
                    return LazyMember::Missing;
                }
            }
            // A variadic hook receives a tuple, so only a proven missing
            // member can be resolved without matching a string guard.
            if !definition.has_positional_parameter {
                break;
            }
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
            if let ExpressionKind::Membership {
                element,
                container,
                contains: true,
            } = &test.kind
                && matches!(&element.kind, ExpressionKind::Name(name) if name == &parameter.text)
                && let ExpressionKind::Name(mapping) = &container.kind
                && let Some(table) = self.lexical(scope, mapping, statement.at)
                && !self.binding_depends_on_branch(table, statement.at)
                && let Binding::Variable(
                    Some(Expression {
                        kind: ExpressionKind::Dictionary(items),
                        ..
                    }),
                    _,
                ) = self.symbols[table].binding
            {
                for pair in items.as_chunks::<2>().0 {
                    let ExpressionKind::Text(text) = &pair[0].kind else {
                        return LazyMember::Unknown;
                    };
                    if let Some(result) = visit(
                        text,
                        scope,
                        body,
                        parameter_id.map(|id| (id, text.as_str())),
                    ) {
                        return result;
                    }
                }
                continue;
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
            if let Some(result) = visit(
                text,
                scope,
                body,
                parameter_id.map(|id| (id, text.as_str())),
            ) {
                return result;
            }
        }
        LazyMember::Unknown
    }

    fn lazy_branch(
        scope: ScopeId,
        body: &'a [Statement],
        argument: Option<(SymbolId, &'a str)>,
    ) -> LazyMember<'a> {
        if let Some((
            Statement {
                kind: StatementKind::Return(Some(value)),
                at,
            },
            imports,
        )) = body.split_last()
            && imports
                .iter()
                .all(|stmt| matches!(stmt.kind, StatementKind::Import(_)))
        {
            return LazyMember::Branch(LazyBranch {
                scope,
                imports,
                value,
                at: *at,
                argument,
            });
        }
        LazyMember::Unknown
    }

    fn member(
        &mut self,
        base: Value,
        name: &str,
        access: (ScopeId, Location),
        depth: usize,
    ) -> Value {
        match base {
            Value::Alternatives(values) => Value::alternatives(
                values
                    .into_iter()
                    .map(|value| self.member(value, name, access, depth + 1))
                    .collect(),
            ),
            Value::ReceiverParameter(parameter) => {
                Value::ReceiverMethod(parameter, name.to_owned())
            }
            Value::Module(file) => self.module_member_at(
                &self.files[file].module,
                name,
                END,
                access,
                ModuleMemberAccess::Attribute,
                depth,
            ),
            Value::Provider(module) => self.module_member_at(
                &module,
                name,
                END,
                access,
                ModuleMemberAccess::Attribute,
                depth,
            ),
            Value::Instance(class) | Value::Callable(Invocation { target: class, .. })
                if self.is_class(class) =>
            {
                let member = self
                    .class_member(class, name, depth)
                    .unwrap_or(Value::Unknown);
                if let Value::Callable(invocation) = member
                    && self.receiver_parameter(invocation.target).is_some()
                {
                    let receiver = match base {
                        Value::Callable(_) => Receiver::Explicit(None),
                        _ => Receiver::Bound(class),
                    };
                    Value::Callable(invocation.bind_receiver(receiver))
                } else {
                    member
                }
            }
            Value::Object(object, _) => Value::Method(object, name.into()),
            Value::Executor if name == "submit" => Value::Submit,
            Value::Process(target) if name == "start" => Value::Start(target),
            _ => Value::Unknown,
        }
    }

    fn record_class_reference(&mut self, scope: ScopeId, base: &Value) {
        match base {
            Value::Callable(invocation) if self.is_class(invocation.target) => {
                self.edge(scope, invocation.target, "class_reference", "certain");
            }
            Value::Alternatives(values) => {
                for value in values {
                    self.record_class_reference(scope, value);
                }
            }
            _ => {}
        }
    }

    fn comparison_may_call_method(&mut self, left: &Value, right: &Value, equal: bool) -> bool {
        if let Value::Alternatives(values) = left {
            return values
                .iter()
                .any(|value| self.comparison_may_call_method(value, right, equal));
        }
        if let Value::Alternatives(values) = right {
            return values
                .iter()
                .any(|value| self.comparison_may_call_method(left, value, equal));
        }
        // Sequence equality tests corresponding elements, and unequal
        // lengths short-circuit even when the outer operation is !=.
        if let (Value::Sequence(left), Value::Sequence(right)) = (left, right) {
            return left.len() == right.len()
                && left
                    .iter()
                    .zip(right)
                    .any(|(left, right)| self.comparison_may_call_method(left, right, true));
        }

        [(left, right), (right, left)]
            .into_iter()
            .any(|(value, other)| match value {
                Value::Instance(class) => {
                    self.class_member(*class, "__eq__", 0).is_some()
                        || !equal && self.class_member(*class, "__ne__", 0).is_some()
                }
                Value::LocalNamespace(scope, first)
                    if matches!(
                        other,
                        Value::Mapping(_)
                            | Value::LocalNamespace(..)
                            | Value::Unknown
                            | Value::OpaqueNamespace
                    ) =>
                {
                    self.has_equality_hooks
                        && (!self.namespace_has_only_scalars(*scope, *first)
                            || match other {
                                Value::Mapping(items) => {
                                    !items.iter().all(Value::is_builtin_scalar)
                                }
                                Value::LocalNamespace(scope, first) => {
                                    !self.namespace_has_only_scalars(*scope, *first)
                                }
                                _ => true,
                            })
                }
                Value::NamespaceCopy(_)
                    if matches!(
                        other,
                        Value::Mapping(_)
                            | Value::LocalNamespace(..)
                            | Value::NamespaceCopy(_)
                            | Value::BuiltinIterable
                            | Value::Unknown
                            | Value::OpaqueNamespace
                    ) =>
                {
                    self.has_equality_hooks
                }
                Value::OpaqueNamespace => self.has_equality_hooks,
                // A mapping can compare keys and values independent of the
                // order in which the literals list their entries.
                Value::Mapping(items)
                    if matches!(
                        other,
                        Value::Mapping(_) | Value::BuiltinIterable | Value::Unknown
                    ) =>
                {
                    items
                        .iter()
                        .any(|item| self.comparison_may_call_method(item, &Value::Unknown, true))
                }
                Value::Sequence(items)
                    if matches!(other, Value::BuiltinIterable | Value::Unknown) =>
                {
                    items
                        .iter()
                        .any(|item| self.comparison_may_call_method(item, &Value::Unknown, true))
                }
                _ => false,
            })
    }

    fn namespace_has_only_scalars(&mut self, scope: ScopeId, first: [usize; 2]) -> bool {
        let reads = &self.scopes[scope].namespace_reads;
        if !reads.contains_key(&first) {
            return false;
        }
        // Python 3.11 can refresh an earlier namespace through another read.
        let bindings: BTreeSet<_> = reads
            .range(first..)
            .flat_map(|(_, read)| read.bindings.iter().copied())
            .collect();
        bindings
            .into_iter()
            .all(|id| self.binding_value(id, 0).is_builtin_scalar())
    }

    fn ordering_may_call_method(&self, left: &Value, right: &Value) -> bool {
        let operands = [left, right];
        // Only two potential sequences can invoke element equality during ordering.
        let unknown_may_dispatch = self.has_ordering_hooks
            || (self.has_equality_hooks
                && !operands
                    .into_iter()
                    .any(|value| value.is_builtin_scalar() || matches!(value, Value::Range)));
        operands.into_iter().any(|operand| match operand {
            Value::Unknown | Value::OperatorResult | Value::OpaqueNamespace => unknown_may_dispatch,
            _ => !operand.is_builtin_scalar_or_iterable(),
        })
    }

    /// None means the attribute is absent in a fully resolved base chain.
    fn class_member(&mut self, id: SymbolId, name: &str, depth: usize) -> Option<Value> {
        if depth >= MAX_BINDING_CHAIN {
            return Some(Value::Unknown);
        }
        let symbol = &self.symbols[id];
        let Binding::Definition(definition, class_scope) = symbol.binding else {
            unreachable!()
        };
        if let Some(attribute) = self.local(class_scope, name, END) {
            return Some(self.binding(attribute, END, depth + 1));
        }
        match definition.bases.as_slice() {
            [] => None,
            [base] => match self.identity(symbol.scope, base, definition.name.at, depth + 1) {
                Value::Callable(Invocation { target: parent, .. }) if self.is_class(parent) => {
                    self.class_member(parent, name, depth + 1)
                }
                Value::Provider(name) if name == "builtins.object" => None,
                Value::Provider(base)
                    if name == "__init_subclass__" && builtin_noop_subclass_hook(&base) =>
                {
                    None
                }
                _ => Some(Value::Unknown),
            },
            _ => Some(Value::Unknown),
        }
    }

    fn subclass_hook(&mut self, scope: ScopeId, base: Value, at: Location, single_base: bool) {
        let hook = match base {
            Value::Callable(Invocation { target: id, .. }) if self.is_class(id) => {
                self.class_member(id, "__init_subclass__", 0)
            }
            Value::Provider(name) if builtin_noop_subclass_hook(&name) => None,
            _ => Some(Value::Unknown),
        };
        match hook {
            None => {}
            Some(Value::Callable(Invocation { target: id, .. }))
                if single_base
                    && matches!(self.symbols[id].binding, Binding::Definition(definition, _)
                        if definition.kind == DefinitionKind::Function) =>
            {
                self.edge(scope, id, "subclass_hook", "certain");
            }
            _ => self.scope_issue(
                scope,
                at,
                "unresolved_subclass_hook",
                "class creation hook dispatch is outside the bounded class hierarchy",
            ),
        }
    }

    fn probe_identity(
        &mut self,
        scope: ScopeId,
        expression: &Expression,
        before: Location,
    ) -> Value {
        let previous = std::mem::replace(&mut self.probing_identity, true);
        let value = self.identity(scope, expression, before, 0);
        self.probing_identity = previous;
        value
    }

    fn identity(
        &mut self,
        scope: ScopeId,
        expression: &Expression,
        before: Location,
        depth: usize,
    ) -> Value {
        self.expression_value(scope, expression, before, depth, ValueEvaluation::Identity)
    }

    fn expression_value(
        &mut self,
        scope: ScopeId,
        expression: &Expression,
        before: Location,
        depth: usize,
        evaluation: ValueEvaluation<'a>,
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
        if matches!(evaluation, ValueEvaluation::Lazy { .. })
            && matches!(expression.kind, ExpressionKind::Call { .. })
        {
            return self.lazy_call(scope, expression, before, depth, evaluation);
        }
        // Other effectful operations require additional execution.
        if matches!(evaluation, ValueEvaluation::Lazy { .. })
            && matches!(
                expression.kind,
                ExpressionKind::Await(_)
                    | ExpressionKind::OrderingComparison { .. }
                    | ExpressionKind::Operands(_)
            )
        {
            self.scope_issue(
                scope,
                expression.at,
                "unresolved_lazy_export",
                "effects of the lazy return expression are outside the bounded model",
            );
            return Value::Unknown;
        }
        let recurse = |this: &mut Self, expression| {
            this.expression_value(scope, expression, before, depth + 1, evaluation)
        };
        match &expression.kind {
            ExpressionKind::Name(name) => {
                let lazy = matches!(evaluation, ValueEvaluation::Lazy { .. });
                let binding = self.lexical_binding(scope, name, before, lazy);
                if lazy && matches!(binding, LexicalBinding::UnboundLocal) {
                    return Value::Unknown;
                }
                if let LexicalBinding::Found(id) = binding {
                    if let ValueEvaluation::Lazy {
                        argument: Some((parameter, text)),
                        ..
                    } = evaluation
                        && id == parameter
                    {
                        self.read.insert(id);
                        return Value::Text(text.to_owned());
                    }
                    let binding_scope = self.symbols[id].scope;
                    let before = if binding_scope == scope {
                        before
                    } else {
                        if self.scopes[binding_scope].names[name.as_str()].len() > 1 {
                            self.scope_issue(scope, expression.at, "ambiguous_binding",
                                "an enclosing name has multiple bindings and the active version is unresolved");
                            return Value::Unknown;
                        }
                        END
                    };
                    let value = self.binding(id, before, depth + 1);
                    if let ValueEvaluation::Lazy {
                        execution_scope: Some(caller),
                        ..
                    } = evaluation
                    {
                        self.record_class_reference(caller, &value);
                    }
                    return value;
                }
                if matches!(name.as_str(), "locals" | "vars") {
                    return match binding {
                        LexicalBinding::Missing => Value::Provider(format!("builtins.{name}")),
                        _ => Value::Unknown,
                    };
                }
                if name == "__name__" {
                    return Value::Unknown;
                }
                if ((self.grep_verify || lazy) && name == "getattr"
                    || matches!(
                        name.as_str(),
                        "print"
                            | "len"
                            | "range"
                            | "str"
                            | "int"
                            | "bool"
                            | "open"
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
                    ))
                    && !self.scopes[scope].names.contains_key(name.as_str())
                {
                    return Value::Provider(format!("builtins.{name}"));
                }
                Value::Unknown
            }
            ExpressionKind::Member(base, name) => {
                let base = recurse(self, base);
                if let ValueEvaluation::Lazy {
                    execution_scope, ..
                } = evaluation
                {
                    return self.lazy_attribute(
                        base,
                        name,
                        (scope, before),
                        depth + 1,
                        execution_scope,
                    );
                }
                self.member(base, name, (scope, before), depth + 1)
            }
            ExpressionKind::Call {
                function,
                arguments,
                keywords,
            } => match recurse(self, function) {
                Value::Callable(Invocation { target: id, .. }) if self.is_class(id) => self
                    .allocation_class(id, depth + 1)
                    .map_or(Value::Unknown, Value::Instance),
                Value::Callable(invocation) if matches!(self.symbols[invocation.target].binding, Binding::Definition(def, _) if def.kind == DefinitionKind::Coroutine) => {
                    Value::Coroutine(
                        self.with_explicit_receiver(invocation, arguments, keywords, recurse),
                    )
                }
                Value::Provider(name) if self.grep_verify && name == "builtins.getattr" => self
                    .verification_member(scope, arguments, keywords, before, depth)
                    .map_or(Value::Unknown, |(value, _)| value),
                Value::Provider(name) if name == "builtins.open" => {
                    Value::Provider("builtins.open.result".into())
                }
                Value::Provider(name) if name == "asyncio.create_task" => Value::ScheduledTask,
                Value::Provider(name)
                    if name == "builtins.object" && arguments.is_empty() && keywords.is_empty() =>
                {
                    Value::Scalar
                }
                Value::Provider(name)
                    if matches!(name.as_str(), "builtins.locals" | "builtins.vars")
                        && arguments.is_empty()
                        && keywords.is_empty()
                        && self.in_function(scope) =>
                {
                    Value::LocalNamespace(scope, position(expression.at))
                }
                Value::Provider(name)
                    if name == "builtins.len" && arguments.len() == 1 && keywords.is_empty() =>
                {
                    // The call walker handles __len__ dispatch independently of its integer result.
                    Value::Scalar
                }
                Value::Provider(name) if name == "builtins.bool" && keywords.is_empty() => {
                    match arguments.as_slice() {
                        [] => Value::Scalar,
                        [argument] => {
                            let value = recurse(self, argument);
                            // A scalar result alone cannot account for a custom truth test.
                            if value.is_builtin_scalar_or_iterable()
                                || matches!(
                                    value,
                                    Value::Sequence(_)
                                        | Value::Mapping(_)
                                        | Value::LocalNamespace(..)
                                        | Value::NamespaceCopy(_)
                                )
                            {
                                Value::Scalar
                            } else {
                                Value::Unknown
                            }
                        }
                        _ => Value::Unknown,
                    }
                }
                Value::Provider(name) if name == "builtins.dict" => {
                    let named: BTreeMap<_, _> = keywords
                        .iter()
                        .map(|(name, value)| (name.clone(), recurse(self, value)))
                        .collect();
                    let site = (scope, position(expression.at));
                    match arguments.as_slice() {
                        [] => Value::Mapping(mapping_entries(named).collect()),
                        [argument] => match recurse(self, argument) {
                            mapping @ Value::Mapping(_) if named.is_empty() => mapping,
                            Value::Mapping(entries)
                                if entries.iter().any(Value::may_contain_namespace)
                                    || named.values().any(Value::may_contain_namespace) =>
                            {
                                // Exact string keys overwritten by keywords
                                // no longer expose their old values.
                                let (pairs, _) = entries.as_chunks::<2>();
                                let mut result: Vec<_> = pairs
                                    .iter()
                                    .filter(|entry| !matches!(&entry[0], Value::Text(key) if named.contains_key(key)))
                                    .flatten()
                                    .cloned()
                                    .collect();
                                result.extend(mapping_entries(named));
                                Value::Mapping(result)
                            }
                            Value::LocalNamespace(owner, first) => {
                                let cutoff = self.scopes[scope]
                                    .namespace_copy_cutoffs
                                    .get(&site.1)
                                    .copied()
                                    .unwrap_or(site.1);
                                Value::NamespaceCopy(Box::new(NamespaceCopy {
                                    owner,
                                    first,
                                    copied_at: (scope, cutoff),
                                    site,
                                    keywords: named,
                                }))
                            }
                            Value::NamespaceCopy(mut copy) => {
                                copy.site = site;
                                copy.keywords.extend(named);
                                Value::NamespaceCopy(copy)
                            }
                            Value::OpaqueNamespace => Value::OpaqueNamespace,
                            _ if named.values().any(Value::may_contain_namespace) => {
                                Value::OpaqueNamespace
                            }
                            _ => Value::Unknown,
                        },
                        _ => Value::Unknown,
                    }
                }
                Value::Provider(name)
                    if keywords.is_empty()
                        && matches!(name.as_str(), "builtins.list" | "builtins.tuple") =>
                {
                    match arguments.as_slice() {
                        [] => Value::Sequence(Vec::new()),
                        [argument] => match recurse(self, argument) {
                            value @ (Value::Sequence(_) | Value::BuiltinIterable) => value,
                            Value::Range | Value::Text(_) => Value::BuiltinIterable,
                            _ => Value::Unknown,
                        },
                        _ => Value::Unknown,
                    }
                }
                Value::Provider(name) if name == "builtins.set" && keywords.is_empty() => {
                    // A mutable input can acquire elements through aliases. When
                    // selected hooks can matter, trust scalar elements only in a
                    // fresh literal or an immutable input.
                    let has_element_hooks = self.has_hash_hooks || self.has_equality_hooks;
                    match arguments.as_slice() {
                        [] => Value::BuiltinIterable,
                        [argument] => match recurse(self, argument) {
                            Value::Range | Value::Text(_) => Value::BuiltinIterable,
                            Value::BuiltinIterable if !has_element_hooks => Value::BuiltinIterable,
                            Value::Sequence(items)
                                if (!has_element_hooks
                                    || matches!(argument.kind, ExpressionKind::Sequence(_)))
                                    && items.iter().all(Value::is_builtin_scalar) =>
                            {
                                Value::BuiltinIterable
                            }
                            _ => Value::Unknown,
                        },
                        _ => Value::Unknown,
                    }
                }
                Value::Provider(name)
                    if name == "builtins.range"
                        && keywords.is_empty()
                        && arguments
                            .iter()
                            .all(|argument| recurse(self, argument).is_builtin_scalar()) =>
                {
                    Value::Range
                }
                Value::Provider(name)
                    if keywords.is_empty()
                        && matches!(
                            name.as_str(),
                            "builtins.int"
                                | "builtins.float"
                                | "builtins.str"
                                | "builtins.bytes"
                                | "builtins.print"
                        )
                        && arguments.iter().all(|argument| {
                            recurse(self, argument).is_builtin_scalar_or_iterable()
                        }) =>
                {
                    Value::Scalar
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
            ExpressionKind::Scalar => Value::Scalar,
            ExpressionKind::Operands(operands) => {
                if self.has_namespace_reads(scope)
                    && operands
                        .iter()
                        .any(|value| recurse(self, value).may_contain_namespace())
                {
                    Value::OpaqueNamespace
                } else {
                    Value::OperatorResult
                }
            }
            ExpressionKind::Sequence(items) => {
                Value::Sequence(items.iter().map(|expr| recurse(self, expr)).collect())
            }
            ExpressionKind::Dictionary(items) => {
                Value::Mapping(items.iter().map(|expr| recurse(self, expr)).collect())
            }
            ExpressionKind::Subscript { value, index } => {
                let value = recurse(self, value);
                let index = recurse(self, index);
                match (value, index) {
                    (Value::Mapping(items), Value::Text(index)) => items
                        .as_chunks::<2>()
                        .0
                        .iter()
                        .rev()
                        .find_map(|pair| match &pair[0] {
                            Value::Text(key) if key == &index => Some(pair[1].clone()),
                            _ => None,
                        })
                        .unwrap_or(Value::Unknown),
                    _ => Value::Unknown,
                }
            }
            ExpressionKind::Conditional {
                test,
                body,
                otherwise,
            } => match recurse(self, test) {
                Value::Bool(true) => recurse(self, body),
                Value::Bool(false) => recurse(self, otherwise),
                _ => {
                    let values = vec![recurse(self, body), recurse(self, otherwise)];
                    if values.iter().any(Value::contains_unknown) {
                        Value::Unknown
                    } else {
                        Value::alternatives(values)
                    }
                }
            },
            ExpressionKind::Membership {
                element,
                container,
                contains,
            } => {
                let element = recurse(self, element);
                let container = recurse(self, container);
                match (element, container) {
                    (Value::Text(element), Value::Mapping(items)) => {
                        let found =
                            items.as_chunks::<2>().0.iter().any(
                                |pair| matches!(&pair[0], Value::Text(key) if key == &element),
                            );
                        Value::Bool(found == *contains)
                    }
                    _ => Value::Unknown,
                }
            }
            ExpressionKind::Compare {
                left,
                right,
                equal,
                identity,
            } => {
                let left = recurse(self, left);
                let right = recurse(self, right);
                let same = match (&left, &right) {
                    (Value::Text(left), Value::Text(right)) if !identity => left == right,
                    (Value::Bool(left), Value::Bool(right)) => left == right,
                    _ if *identity
                        || (left.is_builtin_scalar_or_iterable()
                            && right.is_builtin_scalar_or_iterable()) =>
                    {
                        return Value::Scalar;
                    }
                    _ => return Value::Unknown,
                };
                Value::Bool(same == *equal)
            }
            ExpressionKind::Not(value) => match recurse(self, value) {
                Value::Bool(value) => Value::Bool(!value),
                _ => Value::Scalar,
            },
            ExpressionKind::Boolean { and, values } => {
                let mut result = Value::Bool(*and);
                for (index, value) in values.iter().enumerate() {
                    match recurse(self, value) {
                        Value::Bool(value) if value != *and => {
                            if matches!(result, Value::Bool(_)) {
                                result = Value::Bool(value);
                            }
                            break;
                        }
                        Value::Bool(_) => {}
                        Value::LocalNamespace(owner, first)
                            if *and
                                && index + 1 < values.len()
                                && self.namespace_is_nonempty(owner, first) => {}
                        value if value.may_contain_namespace() => {
                            return Value::OpaqueNamespace;
                        }
                        _ if !self.has_namespace_reads(scope) => return Value::Unknown,
                        _ => result = Value::Unknown,
                    }
                }
                result
            }
            ExpressionKind::OrderingComparison { left, right } => {
                let left = recurse(self, left);
                let right = recurse(self, right);
                if self.ordering_may_call_method(&left, &right) {
                    Value::Unknown
                } else {
                    Value::Scalar
                }
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

    fn invoke(
        &mut self,
        scope: ScopeId,
        invocation: Invocation,
        kind: &'static str,
        certainty: &'static str,
        at: Location,
    ) {
        let (kind, certainty) = match (invocation.verified, kind) {
            (true, "direct_call") => ("grep_dynamic_call", "possible"),
            (true, _) => ("grep_dynamic", "possible"),
            (false, _) => (kind, certainty),
        };
        let owner = self.scopes[self.symbols[invocation.target].scope].owner;
        if let Some(class) = invocation.receiver.supplied()
            && self.receiver_parameter(invocation.target).is_some()
        {
            self.method_receivers
                .entry(invocation.target)
                .or_default()
                .insert(class);
        }
        if matches!(invocation.receiver, Receiver::Ordinary)
            || matches!(invocation.receiver, Receiver::Bound(class) if Some(class) == owner)
        {
            self.edge(scope, invocation.target, kind, certainty);
        } else {
            self.class_invocations.push(ClassInvocation {
                scope,
                at,
                edge: Edge {
                    target: invocation.target,
                    kind,
                    certainty,
                },
                receiver: invocation.receiver,
            });
        }
    }

    fn receiver_conflicts(&self, invocation: Invocation) -> bool {
        if matches!(invocation.receiver, Receiver::Ordinary) {
            return false;
        }
        let inferred_class = self
            .receiver_parameter(invocation.target)
            .and_then(|parameter| {
                self.instance_receivers
                    .get(&parameter)
                    .copied()
                    .filter(|class| {
                        self.read.contains(&parameter) && !self.subclassed.contains(class)
                    })
            });
        inferred_class.is_some() && inferred_class != invocation.receiver.supplied()
    }

    fn resolve_class_invocations(&mut self) {
        // A module can read a method before analysis has visited its class suite
        // and determined whether decorators preserve instance binding.
        for call in std::mem::take(&mut self.class_invocations) {
            if self.receiver_conflicts(Invocation {
                target: call.edge.target,
                receiver: call.receiver,
                verified: false,
            }) {
                self.scope_issue(
                    call.scope,
                    call.at,
                    "unresolved_call",
                    "explicit method receiver is outside the bounded model",
                );
            } else {
                self.scopes[call.scope].edges.push(call.edge);
            }
        }
    }

    fn resolve_receiver_calls(&mut self, finalize: bool) {
        loop {
            let before = self.resolved_receiver_calls.len();
            for index in 0..self.receiver_calls.len() {
                let call = self.receiver_calls[index].clone();
                let Some(owner) = self.scopes[self.symbols[call.parameter].scope].owner else {
                    continue;
                };
                let receivers = self
                    .method_receivers
                    .get(&owner)
                    .cloned()
                    .unwrap_or_default();
                let certainty = if receivers.len() == 1 {
                    "certain"
                } else {
                    "possible"
                };
                for class in receivers {
                    if !self.resolved_receiver_calls.insert((index, class)) {
                        continue;
                    }
                    match self.class_member(class, &call.member, 0) {
                        Some(Value::Callable(invocation)) => self.invoke(
                            call.scope,
                            invocation.bind_receiver(Receiver::Bound(class)),
                            "direct_call",
                            certainty,
                            call.at,
                        ),
                        _ => {
                            if self.receiver_call_issues.insert(index) {
                                self.scope_issue(
                                    call.scope,
                                    call.at,
                                    "unresolved_call",
                                    "receiver method dispatch is outside the bounded class hierarchy",
                                );
                            }
                        }
                    }
                }
            }
            if self.resolved_receiver_calls.len() == before {
                break;
            }
        }
        if !finalize {
            return;
        }
        for index in 0..self.receiver_calls.len() {
            let call = self.receiver_calls[index].clone();
            let owner = self.scopes[self.symbols[call.parameter].scope].owner;
            if owner.is_none_or(|owner| {
                self.method_receivers
                    .get(&owner)
                    .is_none_or(BTreeSet::is_empty)
            }) && self.receiver_call_issues.insert(index)
            {
                self.scope_issue(
                    call.scope,
                    call.at,
                    "unresolved_call",
                    "receiver type is outside the bounded invocation model",
                );
            }
        }
    }

    fn expose(&mut self, scope: ScopeId, value: Value, kind: &'static str, at: Location) {
        let invocation = match value {
            Value::Callable(invocation) | Value::Coroutine(invocation) => invocation,
            Value::Sequence(values) | Value::Mapping(values) | Value::Alternatives(values) => {
                for value in values {
                    self.expose(scope, value, kind, at);
                }
                return;
            }
            Value::OpaqueNamespace => {
                self.scope_issue(
                    scope,
                    at,
                    "unresolved_export",
                    "a function namespace escapes through an expression outside the bounded model",
                );
                return;
            }
            Value::LocalNamespace(..) | Value::NamespaceCopy(_) => {
                let row = &mut self.scopes[scope];
                row.namespace_escape_exclusions
                    .entry(position(at))
                    .or_insert_with(|| row.namespace_exclusions.clone());
                self.namespace_exports
                    .entry(scope)
                    .or_default()
                    .push(Export {
                        scope,
                        at,
                        kind: "local_namespace",
                        value,
                    });
                return;
            }
            _ => return,
        };
        self.invoke(scope, invocation, kind, "possible", at);
        self.uncertain(
            scope,
            at,
            "a known target may be invoked through an external or asynchronous consumer",
        );
    }

    fn constructor(
        &mut self,
        scope: ScopeId,
        class: SymbolId,
        kind: &'static str,
        certainty: &'static str,
        at: Location,
    ) {
        if let Some(allocator) = self.class_member(class, "__new__", 0) {
            if let Value::Callable(invocation) = allocator {
                // An allocator receives the class itself. Its result determines
                // whether an instance initializer runs and which one is selected.
                self.invoke(
                    scope,
                    invocation.bind_receiver(Receiver::Explicit(None)),
                    kind,
                    certainty,
                    at,
                );
            }
            if self.allocation_class(class, 0).is_none() {
                self.scope_issue(
                    scope,
                    at,
                    "unresolved_constructor",
                    "allocation result is outside the bounded class model",
                );
            }
            return;
        }

        match self.class_member(class, "__init__", 0) {
            None => {}
            Some(Value::Callable(invocation)) => self.invoke(
                scope,
                invocation.bind_receiver(Receiver::Bound(class)),
                kind,
                certainty,
                at,
            ),
            _ => self.scope_issue(
                scope,
                at,
                "unresolved_constructor",
                "constructor dispatch is outside the bounded class hierarchy",
            ),
        }
    }

    fn allocation_class(&mut self, class: SymbolId, depth: usize) -> Option<SymbolId> {
        let Some(allocator) = self.class_member(class, "__new__", depth + 1) else {
            return Some(class);
        };
        let Value::Callable(invocation) = allocator else {
            return None;
        };
        let Binding::Definition(definition, body) = self.symbols[invocation.target].binding else {
            return None;
        };
        let [
            Statement {
                kind: StatementKind::Return(Some(value)),
                ..
            },
        ] = definition.body.as_slice()
        else {
            return None;
        };
        if definition.kind != DefinitionKind::Function || !definition.decorators.is_empty() {
            return None;
        }
        match self.identity(body, value, END, depth + 2) {
            Value::Instance(actual) => Some(actual),
            _ => None,
        }
    }

    fn verification_member(
        &mut self,
        scope: ScopeId,
        args: &[Expression],
        keywords: &[(String, Expression)],
        before: Location,
        depth: usize,
    ) -> Option<(Value, Location)> {
        let [receiver, member] = args else {
            return None;
        };
        let ExpressionKind::Text(name) = &member.kind else {
            return None;
        };
        if !keywords.is_empty() {
            return None;
        }
        let receiver = self.identity(scope, receiver, before, depth + 1);
        let known_receiver = matches!(receiver, Value::Module(_) | Value::Instance(_))
            || matches!(receiver, Value::Callable(invocation) if self.is_class(invocation.target));
        if !known_receiver {
            return None;
        }
        let value = self.member(receiver, name, (scope, before), depth + 1);
        let value = match value {
            Value::Unknown => return None,
            Value::Callable(invocation) => Value::Callable(Invocation {
                verified: true,
                ..invocation
            }),
            value => value,
        };
        Some((value, member.at))
    }

    fn walk(&mut self, scope: ScopeId, expr: &Expression, before: Location) {
        self.walk_expression(scope, expr, before, false);
    }

    fn is_module_getattr_parameter(
        &self,
        scope: ScopeId,
        expression: &Expression,
        before: Location,
    ) -> bool {
        let ExpressionKind::Name(name) = &expression.kind else {
            return false;
        };
        let Some(parameter) = self.lexical(scope, name, before) else {
            return false;
        };
        let Some(owner) = self.scopes[scope].owner else {
            return false;
        };
        matches!(self.symbols[parameter].binding, Binding::Parameter)
            && self.symbols[parameter].scope == scope
            && self.symbols[owner].name.text == "__getattr__"
            && self.scopes[self.symbols[owner].scope].owner.is_none()
    }

    fn has_literal_mapping_keys(value: &Value) -> bool {
        let Value::Mapping(items) = value else {
            return false;
        };
        items
            .as_chunks::<2>()
            .0
            .iter()
            .all(|pair| matches!(pair[0], Value::Text(_) | Value::Bool(_) | Value::Scalar))
    }

    fn walk_if_test(
        &mut self,
        scope: ScopeId,
        test: &Expression,
        before: Location,
    ) -> Option<bool> {
        match &test.kind {
            ExpressionKind::Not(operand) => self
                .walk_if_test(scope, operand, before)
                .map(|value| !value),
            ExpressionKind::Boolean { and, values } => {
                let mut known = true;
                for operand in values {
                    match self.walk_if_test(scope, operand, before) {
                        Some(value) if value != *and => return Some(value),
                        Some(_) => {}
                        None => known = false,
                    }
                }
                known.then_some(*and)
            }
            _ => {
                self.walk(scope, test, before);
                let value = self.identity(scope, test, before, 0);
                self.record_truth_test(scope, test, &value);
                match value {
                    Value::Bool(value) => Some(value),
                    _ => None,
                }
            }
        }
    }

    fn record_truth_test(&mut self, scope: ScopeId, test: &Expression, value: &Value) {
        match value {
            Value::Alternatives(values) => {
                for value in values {
                    self.record_truth_test(scope, test, value);
                }
                return;
            }
            Value::Instance(class) => {
                // Fall back to length only after proving that __bool__ is absent.
                let Some(hook) = self
                    .class_member(*class, "__bool__", 0)
                    .or_else(|| self.class_member(*class, "__len__", 0))
                else {
                    return;
                };
                if self.invoke_instance_hook(scope, *class, hook, "truth_test", test.at) {
                    return;
                }
            }
            // Comparison expressions check their own dispatch during the walk.
            Value::Unknown | Value::OperatorResult | Value::OpaqueNamespace
                if (self.has_bool_hooks || self.has_length_hooks)
                    && !matches!(
                        test.kind,
                        ExpressionKind::Compare { .. } | ExpressionKind::OrderingComparison { .. }
                    ) => {}
            _ => return,
        }
        self.scope_issue(
            scope,
            test.at,
            "unresolved_truth_test",
            "truth-value method dispatch is outside the bounded model",
        );
    }

    fn record_length_call(&mut self, scope: ScopeId, value: Value, at: Location) {
        match value {
            Value::Alternatives(values) => {
                for value in values {
                    self.record_length_call(scope, value, at);
                }
                return;
            }
            Value::Instance(class) => {
                if let Some(hook) = self.class_member(class, "__len__", 0)
                    && self.invoke_instance_hook(scope, class, hook, "length_call", at)
                {
                    return;
                }
            }
            Value::Unknown | Value::OperatorResult | Value::OpaqueNamespace
                if self.has_length_hooks => {}
            _ => return,
        }
        self.scope_issue(
            scope,
            at,
            "unresolved_length_call",
            "len() dispatch is outside the bounded model",
        );
    }

    fn invoke_instance_hook(
        &mut self,
        scope: ScopeId,
        class: SymbolId,
        hook: Value,
        kind: &'static str,
        at: Location,
    ) -> bool {
        let Value::Callable(invocation) = hook else {
            return false;
        };
        if invocation.verified
            || !matches!(self.symbols[invocation.target].binding, Binding::Definition(definition, _)
                if definition.kind == DefinitionKind::Function && definition.decorators.is_empty())
        {
            return false;
        }
        let invocation = if self.receiver_parameter(invocation.target).is_some() {
            invocation.bind_receiver(Receiver::Bound(class))
        } else {
            invocation
        };
        self.invoke(scope, invocation, kind, "certain", at);
        true
    }

    fn in_function(&self, scope: ScopeId) -> bool {
        self.scopes[scope]
            .owner
            .is_some_and(|owner| !self.is_class(owner))
    }

    fn namespace_is_nonempty(&self, owner: ScopeId, first: [usize; 2]) -> bool {
        let at = Location {
            line: first[0],
            column: first[1],
        };
        self.scopes[owner]
            .namespace_reads
            .get(&first)
            .is_some_and(|read| {
                read.bindings.iter().any(|&id| {
                    self.symbols[id].scope == owner && !self.binding_depends_on_branch(id, at)
                })
            })
    }

    fn has_namespace_reads(&self, mut scope: ScopeId) -> bool {
        loop {
            let row = &self.scopes[scope];
            if !row.namespace_reads.is_empty() {
                return true;
            }
            let Some(parent) = row.parent else {
                return false;
            };
            scope = parent;
        }
    }

    fn read_local_namespace(&mut self, scope: ScopeId, before: Location, at: Location) {
        let row = &self.scopes[scope];
        let visible = |id: SymbolId| {
            let symbol = &self.symbols[id];
            !matches!(symbol.binding, Binding::Variable(None, _))
                && !row
                    .namespace_exclusions
                    .iter()
                    .any(|span| span.contains(&position(symbol.name.at)))
        };
        let mut bindings = Vec::new();
        for names in row.names.values() {
            let end =
                names.partition_point(|&id| position(self.symbols[id].name.at) < position(before));
            // Walk the possible assignments back to the last definite value.
            for id in names[..end].iter().rev().copied().filter(|&id| visible(id)) {
                bindings.push(id);
                if !self.binding_depends_on_branch(id, before) {
                    break;
                }
            }
        }
        bindings.extend(
            row.namespace_captures
                .get_or_init(|| self.namespace_captures(scope)),
        );
        self.read.extend(bindings.iter().copied());
        let row = &mut self.scopes[scope];
        row.namespace_reads.insert(
            position(at),
            NamespaceRead {
                bindings,
                exclusions: row.namespace_exclusions.clone(),
            },
        );
    }

    fn namespace_captures(&self, scope: ScopeId) -> Vec<SymbolId> {
        let owner = self.scopes[scope]
            .owner
            .expect("namespace belongs to a function");
        let Binding::Definition(definition, _) = self.symbols[owner].binding else {
            unreachable!("function scope has a definition");
        };
        let postponed = self.files[self.scopes[scope].file]
            .facts
            .postponed_annotations;
        let free_names = namespace_free_names(definition, postponed);
        let mut captures = Vec::new();
        for name in free_names {
            let mut parent = self.scopes[scope].parent;
            while let Some(outer) = parent {
                let row = &self.scopes[outer];
                if row.owner.is_none() {
                    break;
                }
                if self.in_function(outer) {
                    if let Some(bindings) = row.names.get(name) {
                        // A closure can run before or after a later assignment.
                        captures.extend(bindings.iter().copied().filter(|&id| {
                            !matches!(self.symbols[id].binding, Binding::Variable(None, _))
                        }));
                        break;
                    }
                    if suite_binds_name(row.body, name) {
                        break;
                    }
                }
                parent = row.parent;
            }
        }
        captures
    }

    fn walk_expression(
        &mut self,
        scope: ScopeId,
        expr: &Expression,
        before: Location,
        as_callee: bool,
    ) {
        match &expr.kind {
            ExpressionKind::Call {
                function,
                arguments,
                keywords,
            } => {
                self.walk_expression(scope, function, before, true);
                for expr in arguments
                    .iter()
                    .chain(keywords.iter().map(|(_, value)| value))
                {
                    self.walk(scope, expr, before);
                }
                let callee = self.identity(scope, function, before, 0);
                match callee {
                    Value::Callable(invocation) => {
                        self.call_invocation(
                            invocation,
                            CallArguments {
                                scope,
                                before,
                                at: expr.at,
                                positional: arguments,
                                keywords,
                            },
                            "certain",
                        );
                    }
                    Value::Alternatives(values) => {
                        let mut resolved = false;
                        let mut unresolved = false;
                        for value in values {
                            if let Value::Callable(invocation) = value {
                                resolved = true;
                                self.call_invocation(
                                    invocation,
                                    CallArguments {
                                        scope,
                                        before,
                                        at: expr.at,
                                        positional: arguments,
                                        keywords,
                                    },
                                    "possible",
                                );
                            } else {
                                unresolved = true;
                            }
                        }
                        if !resolved || unresolved {
                            self.scope_issue(
                                scope,
                                expr.at,
                                "unresolved_call",
                                "one or more finite call targets are outside the bounded model",
                            );
                        }
                    }
                    Value::ReceiverMethod(parameter, member) => {
                        self.receiver_calls.push(ReceiverCall {
                            scope,
                            parameter,
                            member,
                            at: expr.at,
                        });
                    }
                    Value::Provider(name)
                        if matches!(name.as_str(), "builtins.locals" | "builtins.vars") =>
                    {
                        if arguments.is_empty() && keywords.is_empty() && self.in_function(scope) {
                            self.read_local_namespace(scope, before, expr.at);
                        } else {
                            self.scope_issue(
                                scope,
                                expr.at,
                                "unresolved_call",
                                "only zero-argument namespace reads in functions are bounded",
                            );
                        }
                    }
                    Value::Provider(name)
                        if matches!(name.as_str(), "builtins.eval" | "builtins.exec") =>
                    {
                        self.scope_issue(
                            scope,
                            expr.at,
                            "unresolved_call",
                            format!("{name} can execute code outside the bounded model"),
                        );
                    }
                    Value::Provider(name) if name == "builtins.len" => {
                        if let [argument] = arguments.as_slice()
                            && keywords.is_empty()
                        {
                            let value = self.identity(scope, argument, before, 0);
                            self.record_length_call(scope, value, expr.at);
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
                    Value::Submit => {
                        self.activate_submission(scope, before, expr.at, arguments, keywords);
                    }

                    Value::Start(value) => match *value {
                        Value::Callable(_) => {
                            self.activate_callback(scope, *value, "process_target", expr.at);
                        }
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
                    Value::Provider(name) if self.grep_verify && name == "builtins.getattr" => {
                        match self.verification_member(scope, arguments, keywords, before, 0) {
                            Some((Value::Callable(invocation), at)) => {
                                // An immediately called function gets its edge and
                                // arguments from the enclosing call. A standalone
                                // lookup still supplies possible external use.
                                if !as_callee
                                    || !matches!(self.symbols[invocation.target].binding,
                                        Binding::Definition(def, _) if def.kind == DefinitionKind::Function)
                                {
                                    self.invoke(scope, invocation, "grep_dynamic", "possible", at);
                                }
                                let reference = (scope, invocation.target, at.line, at.column);
                                if self.dynamic_references.len() < grep_verification::MAX_EVIDENCE {
                                    self.dynamic_references.insert(reference);
                                } else if !self.dynamic_references.contains(&reference) {
                                    self.issues.push(grep_verification::evidence_limit());
                                }
                            }
                            Some(_) => {}
                            None => self.scope_issue(
                                scope,
                                expr.at,
                                "unresolved_call",
                                "dynamic member is outside the bounded verification model",
                            ),
                        }
                    }
                    Value::Provider(name) if is_known_call(&name) => {
                        let row = &mut self.scopes[scope];
                        if name == "builtins.dict"
                            && let Some((&read, _)) = row.namespace_reads.last_key_value()
                            && read > position(expr.at)
                        {
                            // The call copies after all arguments have run,
                            // including any reads that refreshed its input.
                            row.namespace_copy_cutoffs.insert(position(expr.at), read);
                        }
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
                let target = self.identity(scope, value, before, 0);
                if !matches!(target, Value::ScheduledTask) {
                    self.activate_callback(scope, target, "awaited_coroutine", expr.at);
                }
            }
            ExpressionKind::Member(base, name) => {
                self.walk(scope, base, before);
                let base = self.identity(scope, base, before, 1);
                let value = if let Value::Module(file) = base
                    && let ModuleMember::Lazy(hook, branch) = self.selected_module_member(
                        file,
                        name,
                        END,
                        (scope, before),
                        ModuleMemberAccess::Attribute,
                    ) {
                    self.read.insert(hook);
                    self.execute_lazy_branch(scope, &branch)
                } else {
                    self.member(base, name, (scope, before), 1)
                };
                self.record_class_reference(scope, &value);
            }
            ExpressionKind::Not(base) => self.walk(scope, base, before),
            ExpressionKind::Compare {
                left,
                right,
                equal,
                identity,
            } => {
                self.walk(scope, left, before);
                self.walk(scope, right, before);
                if !identity {
                    let left = self.identity(scope, left, before, 0);
                    let right = self.identity(scope, right, before, 0);
                    if self.comparison_may_call_method(&left, &right, *equal) {
                        self.scope_issue(
                            scope,
                            expr.at,
                            "unresolved_comparison",
                            "comparison method dispatch is outside the bounded model",
                        );
                    }
                }
            }
            ExpressionKind::OrderingComparison { left, right } => {
                self.walk(scope, left, before);
                self.walk(scope, right, before);
                let left = self.identity(scope, left, before, 0);
                let right = self.identity(scope, right, before, 0);
                if self.ordering_may_call_method(&left, &right) {
                    self.scope_issue(
                        scope,
                        expr.at,
                        "unresolved_comparison",
                        "ordering comparison dispatch is outside the bounded model",
                    );
                }
            }
            ExpressionKind::Sequence(values)
            | ExpressionKind::Dictionary(values)
            | ExpressionKind::Operands(values) => {
                for item in values {
                    self.walk(scope, item, before);
                }
            }
            ExpressionKind::Subscript { value, index } => {
                self.walk(scope, value, before);
                self.walk(scope, index, before);
                let mapping = self.identity(scope, value, before, 0);
                let key = self.identity(scope, index, before, 0);
                if !Self::has_literal_mapping_keys(&mapping)
                    || !(matches!(key, Value::Text(_) | Value::Bool(_) | Value::Scalar)
                        || self.is_module_getattr_parameter(scope, index, before))
                {
                    self.scope_issue(
                        scope,
                        expr.at,
                        "unresolved_subscript",
                        "subscript dispatch is outside the bounded literal mapping model",
                    );
                }
            }
            ExpressionKind::Conditional {
                test,
                body,
                otherwise,
            } => match self.walk_if_test(scope, test, before) {
                Some(true) => self.walk(scope, body, before),
                Some(false) => self.walk(scope, otherwise, before),
                None => {
                    self.walk(scope, body, before);
                    self.walk(scope, otherwise, before);
                }
            },
            ExpressionKind::Membership {
                element, container, ..
            } => {
                self.walk(scope, element, before);
                self.walk(scope, container, before);
                let mapping = self.identity(scope, container, before, 0);
                let key = self.identity(scope, element, before, 0);
                if !Self::has_literal_mapping_keys(&mapping)
                    || !(matches!(key, Value::Text(_) | Value::Bool(_) | Value::Scalar)
                        || self.is_module_getattr_parameter(scope, element, before))
                {
                    self.scope_issue(
                        scope,
                        expr.at,
                        "unresolved_membership",
                        "membership dispatch is outside the bounded literal mapping model",
                    );
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
                let value = self.identity(scope, expr, before, 0);
                self.record_class_reference(scope, &value);
            }
            _ => {}
        }
    }

    fn call_invocation(
        &mut self,
        invocation: Invocation,
        call: CallArguments<'_>,
        certainty: &'static str,
    ) {
        let CallArguments {
            scope,
            before,
            at,
            positional,
            keywords,
        } = call;
        let id = invocation.target;
        if !invocation.verified {
            self.direct.insert(id);
        }
        self.record_direct_arguments(scope, id, before, at, positional, keywords);
        if !matches!(self.symbols[id].binding, Binding::Definition(def, _) if def.kind == DefinitionKind::Coroutine)
        {
            let invocation =
                self.with_explicit_receiver(invocation, positional, keywords, |graph, value| {
                    graph.identity(scope, value, before, 0)
                });
            self.invoke(scope, invocation, "direct_call", certainty, at);
        }
        if self.is_class(id) {
            let (kind, certainty) = if invocation.verified {
                ("grep_dynamic_call", "possible")
            } else {
                ("constructor", certainty)
            };
            self.constructor(scope, id, kind, certainty, at);
        }
        // Callback parameter substitution is bounded by retaining the finite
        // supplied values even if the callee dispatches dynamically.
        for argument in positional
            .iter()
            .chain(keywords.iter().map(|(_, value)| value))
        {
            let value = self.identity(scope, argument, before, 0);
            self.expose(scope, value, "passed_callable", argument.at);
        }
    }

    fn record_direct_arguments(
        &mut self,
        caller: ScopeId,
        callee: SymbolId,
        before: Location,
        at: Location,
        args: &[Expression],
        keywords: &[(String, Expression)],
    ) {
        if let Some(values) = self.call_argument_values(caller, callee, before, args, keywords) {
            self.direct_arguments
                .entry(callee)
                .or_default()
                .push(DirectArguments { caller, at, values });
        }
    }

    fn call_argument_values(
        &mut self,
        caller: ScopeId,
        callee: SymbolId,
        before: Location,
        args: &[Expression],
        keywords: &[(String, Expression)],
    ) -> Option<BTreeMap<String, Value>> {
        let Binding::Definition(definition, _) = self.symbols[callee].binding else {
            return None;
        };
        if definition.kind != DefinitionKind::Function
            || !definition.simple_positional_parameters
            || self.scopes[self.symbols[callee].scope]
                .owner
                .is_some_and(|owner| self.is_class(owner))
        {
            return None;
        }
        let mut values = BTreeMap::new();
        for (parameter, argument) in definition.parameters.iter().zip(args) {
            values.insert(
                parameter.text.clone(),
                self.identity(caller, argument, before, 0),
            );
        }
        for (parameter, argument) in keywords {
            values.insert(
                parameter.clone(),
                self.identity(caller, argument, before, 0),
            );
        }
        Some(values)
    }

    fn activate_submission(
        &mut self,
        scope: ScopeId,
        before: Location,
        at: Location,
        arguments: &[Expression],
        keywords: &[(String, Expression)],
    ) {
        let target = arguments.first().map_or(Value::Unknown, |argument| {
            self.identity(scope, argument, before, 0)
        });
        if let Value::Callable(invocation) = &target
            && let Binding::Definition(definition, _) = self.symbols[invocation.target].binding
        {
            let values = self
                .call_argument_values(scope, invocation.target, before, &arguments[1..], keywords)
                .unwrap_or_else(|| {
                    definition
                        .parameters
                        .iter()
                        .map(|p| (p.text.clone(), Value::Unknown))
                        .collect()
                });
            // Executor submissions can supply namespace values but cannot
            // establish a synchronous call during module initialization.
            self.executor_arguments
                .entry(invocation.target)
                .or_default()
                .push(DirectArguments {
                    caller: scope,
                    at,
                    values,
                });
        }
        self.activate_callback(scope, target, "executor_submission", at);
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
        self.activate_callback(scope, target, kind, at);
    }

    fn activate_callback(
        &mut self,
        scope: ScopeId,
        target: Value,
        kind: &'static str,
        at: Location,
    ) {
        if matches!(target, Value::Callable(_) | Value::Coroutine(_)) {
            if matches!(kind, "executor_submission" | "process_target")
                && let Value::Callable(invocation) = &target
                && self.is_class(invocation.target)
            {
                self.constructor(
                    scope,
                    invocation.target,
                    "callback_constructor",
                    "possible",
                    at,
                );
            }
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

    fn receiver_parameter(&self, target: SymbolId) -> Option<SymbolId> {
        let symbol = &self.symbols[target];
        let Binding::Definition(definition, body) = symbol.binding else {
            return None;
        };
        if !definition.has_positional_parameter
            || matches!(
                definition.name.text.as_str(),
                "__new__" | "__init_subclass__" | "__class_getitem__"
            )
            || !self.scopes[symbol.scope]
                .owner
                .is_some_and(|owner| self.is_class(owner))
        {
            return None;
        }
        let parameter = definition.parameters.first()?;
        Some(self.by_location[&(body, parameter.at.line, parameter.at.column)])
    }

    fn with_explicit_receiver<'expr>(
        &mut self,
        mut invocation: Invocation,
        arguments: &'expr [Expression],
        keywords: &'expr [(String, Expression)],
        mut evaluate: impl FnMut(&mut Self, &'expr Expression) -> Value,
    ) -> Invocation {
        // A plain call to a function from a class namespace has no
        // descriptor binding. Its receiver, if any, comes from the arguments.
        if matches!(invocation.receiver, Receiver::Explicit(_))
            || (matches!(invocation.receiver, Receiver::Ordinary)
                && self.receiver_parameter(invocation.target).is_some())
        {
            let argument = arguments.first().or_else(|| {
                let Binding::Definition(definition, _) = self.symbols[invocation.target].binding
                else {
                    return None;
                };
                // A simple positional signature also accepts keywords by name.
                if !definition.simple_positional_parameters {
                    return None;
                }
                let parameter = definition.parameters.first()?;
                keywords
                    .iter()
                    .find_map(|(name, value)| (name == &parameter.text).then_some(value))
            });
            let class = match argument.map(|argument| evaluate(self, argument)) {
                Some(Value::Instance(class)) => Some(class),
                _ => None,
            };
            invocation.receiver = Receiver::Explicit(class);
        }
        invocation
    }

    fn annotation(&mut self, scope: ScopeId, expr: &Expression, before: Location, evaluated: bool) {
        let evaluated = evaluated
            && !self.files[self.scopes[scope].file]
                .facts
                .postponed_annotations;
        match &expr.kind {
            ExpressionKind::Operands(values) | ExpressionKind::Sequence(values) => {
                for reference in values {
                    self.annotation(scope, reference, before, evaluated);
                }
            }
            ExpressionKind::Call { .. } if evaluated => {
                self.walk(scope, expr, before);
            }
            _ => {
                let value = if evaluated {
                    self.walk(scope, expr, before);
                    self.identity(scope, expr, before, 0)
                } else {
                    self.probe_identity(scope, expr, before)
                };
                if let Value::Callable(Invocation { target: id, .. }) = value {
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
                    if matches!(
                        definition.name.text.as_str(),
                        "__getattribute__" | "__getattr__"
                    ) && self.scopes[scope].owner.is_some_and(|id| self.is_class(id))
                    {
                        self.scope_issue(
                            scope,
                            at,
                            "unresolved_descriptor",
                            "dynamic instance attribute resolution is outside the bounded model",
                        );
                    }
                    let target = self.by_location
                        [&(scope, definition.name.at.line, definition.name.at.column)];
                    let Binding::Definition(_, definition_scope) = self.symbols[target].binding
                    else {
                        unreachable!()
                    };
                    if definition.kind == DefinitionKind::Class {
                        self.scopes[scope].class_bodies.push(definition_scope);
                    }
                    for annotation in &definition.annotations {
                        self.annotation(scope, annotation, at, true);
                    }
                    for expression in &definition.defaults {
                        self.walk(scope, expression, at);
                    }
                    for base in &definition.bases {
                        self.walk(scope, base, at);
                        let value = self.identity(scope, base, at, 0);
                        if let Value::Callable(Invocation { target: id, .. }) = value {
                            self.edge(scope, id, "base_class", "certain");
                        }
                        self.subclass_hook(scope, value, base.at, definition.bases.len() == 1);
                    }
                    let mut preserves_receiver = true;
                    for decorator in &definition.decorators {
                        preserves_receiver &= self.decorator(scope, decorator, target, at);
                    }
                    if preserves_receiver
                        && let Some(receiver) = self.receiver_parameter(target)
                        && let Some(class) = self.scopes[scope].owner
                    {
                        self.instance_receivers.insert(receiver, class);
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
                StatementKind::Assert(test) => {
                    self.walk(scope, test, at);
                    if matches!(self.identity(scope, test, at, 0), Value::Bool(false)) {
                        return true;
                    }
                }
                StatementKind::Evaluate(value) => self.walk(scope, value, at),
                StatementKind::If {
                    test,
                    body,
                    otherwise,
                } => {
                    let stop = match self.walk_if_test(scope, test, at) {
                        Some(true) => {
                            self.scopes[scope]
                                .namespace_exclusions
                                .extend(namespace_span(otherwise));
                            self.suite(scope, body)
                        }
                        Some(false) => {
                            self.scopes[scope]
                                .namespace_exclusions
                                .extend(namespace_span(body));
                            self.suite(scope, otherwise)
                        }
                        None => {
                            let body_stops = self.suite(scope, body);
                            let excluded_branch = self.scopes[scope].namespace_exclusions.len();
                            self.scopes[scope]
                                .namespace_exclusions
                                .extend(namespace_span(body));
                            let otherwise_stops = self.suite(scope, otherwise);
                            if !body_stops && !body.is_empty() {
                                self.scopes[scope]
                                    .namespace_exclusions
                                    .remove(excluded_branch);
                            }
                            if otherwise_stops {
                                self.scopes[scope]
                                    .namespace_exclusions
                                    .extend(namespace_span(otherwise));
                            }
                            body_stops && otherwise_stops
                        }
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
                StatementKind::Import(import) => {
                    let module = self.relative_module(self.scopes[scope].file, import);
                    if let Some(branch) =
                        self.record_import(scope, scope, module.as_deref(), import)
                    {
                        self.execute_lazy_branch(scope, &branch);
                    }
                }
            }
        }
        false
    }

    /// Returns whether the decorator preserves the function's instance binding.
    fn decorator(
        &mut self,
        scope: ScopeId,
        decorator: &Expression,
        target: SymbolId,
        before: Location,
    ) -> bool {
        let (function, args, keywords) = match &decorator.kind {
            ExpressionKind::Call {
                function,
                arguments,
                keywords,
            } => (function.as_ref(), arguments.as_slice(), keywords.as_slice()),
            _ => (decorator, [].as_slice(), [].as_slice()),
        };
        self.walk(scope, function, before);
        for value in args.iter().chain(keywords.iter().map(|(_, value)| value)) {
            self.walk(scope, value, before);
        }
        match self.identity(scope, function, before, 0) {
            Value::Method(object, name) if is_route(&name) => {
                self.scopes[scope].routes.push((object, target))
            }
            Value::Provider(name) if name == "pytest.fixture" => {
                let registered_name = if let Some((_, expression)) =
                    keywords.iter().find(|(name, _)| name == "name")
                {
                    if let Value::Text(name) = self.identity(scope, expression, before, 0) {
                        Some(name)
                    } else {
                        self.scope_issue(
                            scope,
                            expression.at,
                            "unresolved_fixture_name",
                            "pytest fixture name could not be resolved",
                        );
                        return false;
                    }
                } else {
                    None
                };
                let mut autouse = false;
                if let Some((_, expression)) = keywords.iter().find(|(name, _)| name == "autouse") {
                    match self.identity(scope, expression, before, 0) {
                        Value::Bool(value) => autouse = value,
                        _ => self.scope_issue(
                            scope,
                            expression.at,
                            "unresolved_fixture_autouse",
                            "pytest fixture autouse could not be resolved",
                        ),
                    }
                }
                self.fixtures.insert(
                    target,
                    PytestFixture {
                        name: registered_name,
                        autouse,
                    },
                );
            }
            Value::Callable(invocation) => {
                self.invoke(scope, invocation, "decorator", "certain", decorator.at);
                let Binding::Definition(definition, _) = self.symbols[invocation.target].binding
                else {
                    unreachable!()
                };
                if is_identity_decorator(definition) {
                    return definition.kind == DefinitionKind::Function
                        && definition.has_positional_parameter
                        && definition.decorators.is_empty()
                        && !matches!(decorator.kind, ExpressionKind::Call { .. });
                } else {
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
                    "builtins.staticmethod" | "builtins.classmethod"
                ) => {}
            _ => self.scope_issue(
                scope,
                decorator.at,
                "unresolved_decorator",
                "decorator identity is outside the bounded model",
            ),
        };
        false
    }

    fn class_lineage(&mut self, class: SymbolId) -> (Vec<ScopeId>, bool) {
        let mut lineage = Vec::new();
        let mut current = class;
        let mut resolved = false;
        for _ in 0..MAX_BINDING_CHAIN {
            let symbol = &self.symbols[current];
            let Binding::Definition(definition, scope) = symbol.binding else {
                unreachable!()
            };
            lineage.push(scope);
            let base = match definition.bases.as_slice() {
                [] => {
                    resolved = true;
                    break;
                }
                [base] => base,
                _ => break,
            };
            match self.identity(symbol.scope, base, definition.name.at, 0) {
                Value::Callable(Invocation { target: parent, .. }) if self.is_class(parent) => {
                    current = parent
                }
                Value::Provider(name) if name == "builtins.object" => {
                    resolved = true;
                    break;
                }
                _ => break,
            }
        }
        (lineage, resolved)
    }

    fn pytest_class_names(&mut self, class: SymbolId) -> Option<(PytestNames<'a>, bool)> {
        let symbol = &self.symbols[class];
        if symbol.scope != self.scopes[symbol.scope].file || !self.is_class(class) {
            return None;
        }
        let (lineage, resolved) = self.class_lineage(class);
        // Pytest sees the effective attributes of the collected class, so a
        // subclass can shadow both a test method and a fixture of its base.
        let mut names = PytestNames::new();
        for scope in lineage.into_iter().rev() {
            for (&name, bindings) in &self.scopes[scope].names {
                names.entry(name).or_default().extend(bindings);
            }
        }
        let mut certain = true;
        for name in ["__init__", "__new__", "__test__"] {
            let Some(bindings) = names.get(name) else {
                continue;
            };
            let candidates = self.effective_bindings(bindings);
            let disables_collection = |id: SymbolId| {
                name != "__test__"
                    || matches!(self.symbols[id].binding, Binding::Variable(Some(value), _) if static_bool(value) == Some(false))
            };
            if candidates.len() == 1
                && !self.conditional.contains_key(&candidates[0])
                && disables_collection(candidates[0])
            {
                return None;
            }
            certain &= !candidates.iter().copied().any(disables_collection);
        }
        if !resolved {
            // Unresolved bases can supply tests and fixtures even when the
            // collected class declares none of its own.
            let symbol = &self.symbols[class];
            self.scope_issue(
                symbol.scope,
                symbol.name.at,
                "unresolved_pytest_class",
                "pytest test collection through this class hierarchy could not be resolved",
            );
            return None;
        }
        Some((names, certain))
    }

    fn is_pytest_function(&self, id: SymbolId) -> bool {
        matches!(self.symbols[id].binding, Binding::Definition(def, _) if def.kind != DefinitionKind::Class)
            && !self.fixtures.contains_key(&id)
    }

    fn is_pytest_file(&self, file: usize) -> bool {
        let filename = self.files[file].path.rsplit('/').next().unwrap_or_default();
        filename.starts_with("test_") || filename.ends_with("_test.py")
    }

    fn effective_bindings<'b>(&self, bindings: &'b [SymbolId]) -> &'b [SymbolId] {
        // Conditional attributes may leave a previous binding in place.
        let first = bindings
            .iter()
            .rposition(|id| !self.conditional.contains_key(id))
            .unwrap_or(0);
        &bindings[first..]
    }

    fn pytest_target(&self, id: SymbolId) -> Option<PytestRegistration> {
        let mut bindings = Vec::new();
        let Value::Callable(Invocation { target, .. }) =
            self.pytest_binding(id, None, &mut bindings, 0)?
        else {
            return None;
        };
        Some(PytestRegistration { target, bindings })
    }

    // Collect identity evidence without marking unrelated namespace entries
    // as used. Only a test or fixture that is retained consumes this evidence.
    fn pytest_binding(
        &self,
        id: SymbolId,
        before: Option<Location>,
        bindings: &mut Vec<SymbolId>,
        depth: usize,
    ) -> Option<Value> {
        if depth >= MAX_BINDING_CHAIN
            || before.is_some_and(|at| self.binding_depends_on_branch(id, at))
        {
            return None;
        }
        let symbol = &self.symbols[id];
        match symbol.binding {
            Binding::Definition(_, _) => Some(Value::Callable(Invocation::ordinary(id))),
            Binding::Variable(Some(value), at) => {
                bindings.push(id);
                self.pytest_expression(symbol.scope, value, at, bindings, depth + 1)
            }
            Binding::Import(import) => {
                bindings.push(id);
                let source = self.scopes[symbol.scope].file;
                let module = self.relative_module(source, import)?;
                if let Some(member) = &import.member {
                    let file = *self.modules.get(module.as_str())?;
                    let before = self
                        .import_member_position(symbol.scope, &module, import)
                        .unwrap_or(END);
                    let selected = self.selected_module_member(
                        file,
                        member,
                        before,
                        (symbol.scope, import.local.at),
                        ModuleMemberAccess::FromImport,
                    );
                    self.pytest_member(selected, before, bindings, depth + 1)
                } else {
                    let visible = if import.exposes_top_level {
                        module.split('.').next().unwrap_or(&module)
                    } else {
                        &module
                    };
                    self.modules.get(visible).copied().map(Value::Module)
                }
            }
            _ => None,
        }
    }

    fn pytest_expression(
        &self,
        scope: ScopeId,
        expression: &Expression,
        before: Location,
        bindings: &mut Vec<SymbolId>,
        depth: usize,
    ) -> Option<Value> {
        if depth >= MAX_BINDING_CHAIN {
            return None;
        }
        match &expression.kind {
            ExpressionKind::Name(name) => {
                let id = self.lexical(scope, name, before)?;
                let before = if self.symbols[id].scope == scope {
                    before
                } else {
                    END
                };
                self.pytest_binding(id, Some(before), bindings, depth)
            }
            ExpressionKind::Member(base, member) => {
                let Value::Module(file) =
                    self.pytest_expression(scope, base, before, bindings, depth + 1)?
                else {
                    return None;
                };
                let selected = self.selected_module_member(
                    file,
                    member,
                    END,
                    (scope, before),
                    ModuleMemberAccess::Attribute,
                );
                self.pytest_member(selected, END, bindings, depth + 1)
            }
            _ => None,
        }
    }

    fn pytest_member(
        &self,
        member: ModuleMember<'a>,
        before: Location,
        bindings: &mut Vec<SymbolId>,
        depth: usize,
    ) -> Option<Value> {
        match member {
            ModuleMember::Binding(id, import) => {
                bindings.extend(import);
                self.pytest_binding(id, Some(before), bindings, depth)
            }
            ModuleMember::Submodule(file, imports) => {
                bindings.extend(imports);
                Some(Value::Module(file))
            }
            ModuleMember::Lazy(hook, branch) => {
                bindings.push(hook);
                self.pytest_expression(branch.scope, branch.value, branch.at, bindings, depth)
            }
            _ => None,
        }
    }

    fn pytest_registrations(
        &self,
        names: &PytestNames<'a>,
        prefix: &str,
    ) -> Vec<(PytestRegistration, bool)> {
        names
            .iter()
            .filter(|(name, _)| name.starts_with(prefix))
            .flat_map(|(_, bindings)| {
                let candidates = self.effective_bindings(bindings);
                candidates.iter().filter_map(move |id| {
                    let certain = candidates.len() == 1 && !self.conditional.contains_key(id);
                    self.pytest_target(*id).map(|target| (target, certain))
                })
            })
            .collect()
    }

    fn test_edges(&mut self) -> Vec<Edge> {
        let mut roots = BTreeMap::new();
        let conftests: BTreeMap<_, _> = self
            .files
            .iter()
            .enumerate()
            .filter_map(|(file, source)| {
                pytest_conftest_prefix(&source.path).map(|prefix| (prefix, file))
            })
            .collect();
        for file in 0..self.files.len() {
            if !self.is_pytest_file(file) {
                continue;
            }
            let path = &self.files[file].path;
            let prefixes =
                std::iter::once("").chain(path.match_indices('/').map(|(end, _)| &path[..=end]));
            let ancestors = prefixes.filter_map(|prefix| conftests.get(prefix).copied());
            let namespaces: Vec<_> = ancestors.chain(std::iter::once(file)).collect();
            let module_tests = self.pytest_registrations(&self.scopes[file].names, "test_");
            self.record_pytest_tests(file, &namespaces, None, module_tests, &mut roots);
            for (class, collected) in self.pytest_registrations(&self.scopes[file].names, "Test") {
                let Some((names, eligible)) = self.pytest_class_names(class.target) else {
                    continue;
                };
                let tests: Vec<_> = self
                    .pytest_registrations(&names, "test_")
                    .into_iter()
                    .filter(|(method, _)| self.is_pytest_function(method.target))
                    .map(|(method, certain)| (method, certain && collected && eligible))
                    .collect();
                if !tests.is_empty() {
                    *roots.entry(class.target).or_default() |=
                        tests.iter().any(|(_, certain)| *certain);
                    self.read.extend(class.bindings);
                    self.record_pytest_tests(file, &namespaces, Some(&names), tests, &mut roots);
                }
            }
        }
        roots
            .into_iter()
            .map(|(target, certain)| Edge {
                target,
                kind: "pytest_root",
                certainty: if certain { "certain" } else { "possible" },
            })
            .collect()
    }

    fn record_pytest_tests(
        &mut self,
        file: usize,
        namespaces: &[ScopeId],
        class: Option<&PytestNames<'a>>,
        tests: Vec<(PytestRegistration, bool)>,
        roots: &mut BTreeMap<SymbolId, bool>,
    ) {
        let tests: Vec<_> = tests
            .into_iter()
            .filter_map(|(test, certain)| {
                self.is_pytest_function(test.target)
                    .then_some((test, certain))
            })
            .collect();
        if tests.is_empty() {
            return;
        }
        for (test, certain) in &tests {
            *roots.entry(test.target).or_default() |= *certain;
            self.read.extend(&test.bindings);
        }
        if !self.fixtures.is_empty() {
            self.collect_pytest_fixtures(file, namespaces, class, &tests, roots);
        }
    }

    fn collect_pytest_fixtures(
        &mut self,
        file: usize,
        namespaces: &[ScopeId],
        class: Option<&PytestNames<'a>>,
        tests: &[(PytestRegistration, bool)],
        roots: &mut BTreeMap<SymbolId, bool>,
    ) {
        // Register from outermost to innermost. Pytest requests an autouse
        // fixture by name, so a closer override can satisfy that request too.
        let mut registrations = BTreeMap::<String, Vec<PytestFixtureChoices>>::new();
        for names in namespaces
            .iter()
            .map(|&scope| &self.scopes[scope].names)
            .chain(class)
        {
            for (&alias, bindings) in names {
                // Keep possible conditional bindings at the same precedence.
                let mut alternatives = BTreeMap::<&str, Vec<_>>::new();
                let candidates = self.effective_bindings(bindings);
                let has_unconditional_binding = !self.conditional.contains_key(&candidates[0]);
                for &binding in candidates.iter().rev() {
                    if let Some(resolved) = self.pytest_target(binding)
                        && let Some(fixture) = self.fixtures.get(&resolved.target)
                    {
                        let name = fixture.name.as_deref().unwrap_or(alias);
                        alternatives.entry(name).or_default().push(resolved);
                    }
                }
                for (name, alternatives) in alternatives {
                    let may_be_absent =
                        !has_unconditional_binding || alternatives.len() != candidates.len();
                    registrations
                        .entry(name.into())
                        .or_default()
                        .push(PytestFixtureChoices {
                            alternatives,
                            may_be_absent,
                        });
                }
            }
        }

        let mut pending = Vec::new();
        for (name, choices) in &registrations {
            let autouse = choices
                .iter()
                .flat_map(|choice| &choice.alternatives)
                .any(|registration| self.fixtures[&registration.target].autouse);
            if autouse {
                let certain = choices.iter().any(|choice| {
                    !choice.may_be_absent
                        && choice
                            .alternatives
                            .iter()
                            .all(|registration| self.fixtures[&registration.target].autouse)
                });
                pending.push((
                    name.as_str(),
                    BTreeMap::new(),
                    certain && tests.iter().any(|(_, certain)| *certain),
                ));
            }
        }
        for (test, certain) in tests {
            let Binding::Definition(definition, _) = self.symbols[test.target].binding else {
                continue;
            };
            for parameter in &definition.parameters {
                if let Some((name, _)) = registrations.get_key_value(&parameter.text) {
                    pending.push((name.as_str(), BTreeMap::new(), *certain));
                }
            }
        }
        let mut visited = BTreeSet::new();
        while let Some((name, active, mut certain)) = pending.pop() {
            if !visited.insert((name, active.clone(), certain)) {
                continue;
            }
            if visited.len() > MAX_PYTEST_REQUEST_STATES || active.len() > MAX_BINDING_CHAIN {
                self.issue(
                    file,
                    Location { line: 1, column: 1 },
                    "analysis_limit",
                    "pytest fixture request limit exceeded",
                );
                break;
            }
            let choices = &registrations[name];
            let upper_bound = active.get(name).copied().unwrap_or(choices.len());
            for (index, choice) in choices[..upper_bound].iter().enumerate().rev() {
                // The request chain can already include this fixture name,
                // even when the immediate requester has a different name.
                let mut nested = active.clone();
                if choices.len() > 1 {
                    nested.insert(name, index);
                }
                let selected_certain =
                    certain && !choice.may_be_absent && choice.alternatives.len() == 1;
                for registration in &choice.alternatives {
                    *roots.entry(registration.target).or_default() |= selected_certain;
                    self.read.extend(&registration.bindings);
                    let Binding::Definition(definition, _) =
                        self.symbols[registration.target].binding
                    else {
                        continue;
                    };
                    for parameter in &definition.parameters {
                        if let Some((name, _)) = registrations.get_key_value(&parameter.text) {
                            pending.push((name.as_str(), nested.clone(), selected_certain));
                        }
                    }
                }
                if !choice.may_be_absent {
                    break;
                }
                certain = false;
            }
        }
    }

    fn retain_exports(
        &mut self,
        exports: Vec<Export>,
        initialization_edges: &[Vec<ScopeId>],
        certainty: &'static str,
    ) {
        let mut pending: VecDeque<_> = exports
            .into_iter()
            .map(|export| (export, certainty))
            .collect();
        let mut modules = BTreeSet::new();
        let mut exported_constructors = BTreeSet::new();
        let mut visited_instances = BTreeSet::new();
        let mut visited_namespaces = BTreeSet::new();
        let mut visited_namespace_copies = BTreeSet::new();
        while let Some((
            Export {
                scope,
                at,
                kind,
                value,
            },
            certainty,
        )) = pending.pop_front()
        {
            let visit_scope = (kind == "local_namespace").then_some(scope);
            let members = match value {
                Value::Callable(invocation) => {
                    self.invoke(scope, invocation, kind, certainty, at);
                    if kind == "local_namespace" {
                        self.uncertain(
                            scope,
                            at,
                            "a callable may escape through a function namespace",
                        );
                    }
                    let class = invocation.target;
                    if self.is_class(class) && exported_constructors.insert((visit_scope, class)) {
                        self.constructor(scope, class, "constructor", "possible", at);
                        vec![Value::Instance(class)]
                    } else {
                        Vec::new()
                    }
                }
                Value::Coroutine(_) if kind == "local_namespace" => {
                    self.expose(scope, value, kind, at);
                    Vec::new()
                }
                Value::NamespaceParameter(parameter) => {
                    let default = self.namespace_parameter_default(parameter);
                    let symbol = &self.symbols[parameter];
                    let owner = self.scopes[symbol.scope]
                        .owner
                        .expect("parameter belongs to a definition");
                    let mut callers = self
                        .direct_arguments
                        .get(&owner)
                        .into_iter()
                        .chain(self.executor_arguments.get(&owner))
                        .flatten()
                        .peekable();
                    if callers.peek().is_some() {
                        pending.extend(callers.map(|call| {
                            // Use the call site's scope so that an inactive
                            // caller cannot make its argument reachable.
                            let value = call
                                .values
                                .get(&symbol.name.text)
                                .cloned()
                                .or_else(|| default.clone())
                                .unwrap_or(Value::Unknown);
                            (
                                Export {
                                    scope: call.caller,
                                    at: call.at,
                                    kind,
                                    value,
                                },
                                "possible",
                            )
                        }));
                    } else if self.direct.contains(&owner) {
                        // This call shape has no bounded argument mapping.
                        self.scope_issue(
                            scope,
                            at,
                            "unresolved_export",
                            "a project argument in an escaping namespace could not be resolved",
                        );
                    }
                    default.into_iter().collect()
                }
                Value::Instance(class) if visited_instances.insert((visit_scope, class)) => {
                    self.exported_class_members(class, scope, at)
                }
                Value::Module(file) if modules.insert((visit_scope, file)) => {
                    self.exported_module_members(file, initialization_edges)
                }
                Value::LocalNamespace(owner, first)
                    if visited_namespaces.insert((scope, owner, first, position(at))) =>
                {
                    self.namespace_export_values(owner, first, scope, at, None)
                }
                Value::NamespaceCopy(copy)
                    if visited_namespace_copies.insert((scope, position(at), copy.site)) =>
                {
                    let mut values = self.namespace_export_values(
                        copy.owner,
                        copy.first,
                        scope,
                        at,
                        Some(&copy),
                    );
                    values.extend(copy.keywords.into_values());
                    values
                }
                Value::Sequence(items) | Value::Mapping(items) | Value::Alternatives(items) => {
                    items
                }
                Value::Unknown | Value::OpaqueNamespace => {
                    self.scope_issue(
                        scope,
                        at,
                        "unresolved_export",
                        if kind == "local_namespace" {
                            "an escaping function namespace contains an unresolved value"
                        } else {
                            "a public member could not be resolved"
                        },
                    );
                    Vec::new()
                }
                _ => Vec::new(),
            };
            pending.extend(members.into_iter().map(|value| {
                (
                    Export {
                        scope,
                        at,
                        kind,
                        value,
                    },
                    "possible",
                )
            }));
        }
    }

    fn namespace_export_values(
        &mut self,
        owner: ScopeId,
        first: [usize; 2],
        consumer: ScopeId,
        at: Location,
        copy: Option<&NamespaceCopy>,
    ) -> Vec<Value> {
        let reads = &self.scopes[owner].namespace_reads;
        let Some(initial) = reads.get(&first) else {
            return Vec::new();
        };
        let escape_at = position(at);
        let escape_exclusions = (owner == consumer)
            .then(|| {
                self.scopes[consumer]
                    .namespace_escape_exclusions
                    .get(&escape_at)
            })
            .flatten()
            .map_or(&[][..], Vec::as_slice);
        let excludes = |spans: &[RangeInclusive<_>], at| spans.iter().any(|span| span.contains(at));
        let copied_at = copy
            .filter(|copy| copy.copied_at.0 == owner)
            // The namespace expression can be an argument of the copy call.
            .map(|copy| copy.copied_at.1.max(first));
        // A later same-frame call can refresh the original dictionary.
        // Independent copies keep their contents at the time of the copy.
        let bindings: BTreeSet<_> = reads
            .range(first..)
            .take_while(|(candidate, _)| copied_at.is_none_or(|at| **candidate <= at))
            .filter(|(candidate, read)| {
                !excludes(&initial.exclusions, candidate)
                    && !excludes(escape_exclusions, candidate)
                    && !excludes(&read.exclusions, &first)
                    && (owner != consumer || !excludes(&read.exclusions, &escape_at))
            })
            .flat_map(|(_, read)| read.bindings.iter().copied())
            .filter(|&id| {
                copy.is_none_or(|copy| !copy.keywords.contains_key(&self.symbols[id].name.text))
            })
            .collect();
        bindings
            .into_iter()
            .map(|id| {
                let value = self.binding_value(id, 0);
                if matches!(value, Value::Unknown)
                    && matches!(self.symbols[id].binding, Binding::Parameter)
                {
                    Value::NamespaceParameter(id)
                } else {
                    value
                }
            })
            .collect()
    }

    fn namespace_parameter_default(&mut self, parameter: SymbolId) -> Option<Value> {
        let symbol = &self.symbols[parameter];
        let owner = self.scopes[symbol.scope].owner?;
        let function = &self.symbols[owner];
        let Binding::Definition(definition, _) = function.binding else {
            return None;
        };
        if definition.defaults.is_empty() {
            return None;
        }
        if !definition.simple_positional_parameters {
            return Some(Value::Unknown);
        }
        // Simple positional-or-keyword defaults occupy the final parameters.
        let first_default = definition.parameters.len() - definition.defaults.len();
        let index = definition.parameters[first_default..]
            .iter()
            .position(|candidate| candidate.text == symbol.name.text)?;
        Some(self.identity(
            function.scope,
            &definition.defaults[index],
            definition.name.at,
            0,
        ))
    }

    fn exported_class_members(
        &mut self,
        class: SymbolId,
        scope: ScopeId,
        at: Location,
    ) -> Vec<Value> {
        let (lineage, resolved) = self.class_lineage(class);
        if !resolved {
            self.scope_issue(
                scope,
                at,
                "unresolved_export",
                "public methods of the exported class could not be bounded",
            );
        }
        let names: BTreeSet<_> = lineage
            .into_iter()
            .flat_map(|scope| self.scopes[scope].names.keys().copied())
            .filter(|name| *name == "__call__" || !name.starts_with('_'))
            .collect();
        names
            .into_iter()
            .filter_map(|name| {
                let value = self.member(Value::Instance(class), name, (scope, at), 0);
                if name != "__call__" {
                    return Some(value);
                }
                // Calling the instance invokes its effective __call__ member.
                // A noncallable override does not expose the members of its value.
                match value {
                    Value::Callable(invocation)
                        if !invocation.verified
                            && !self.is_class(invocation.target)
                            && !self.fixtures.contains_key(&invocation.target) =>
                    {
                        Some(value)
                    }
                    Value::Sequence(_)
                    | Value::Mapping(_)
                    | Value::LocalNamespace(..)
                    | Value::NamespaceCopy(_)
                    | Value::Module(_)
                    | Value::Coroutine(_)
                    | Value::ScheduledTask => None,
                    value if value.is_builtin_scalar_or_iterable() => None,
                    _ => Some(Value::Unknown),
                }
            })
            .collect()
    }

    fn exported_module_members(
        &mut self,
        file: usize,
        initialization_edges: &[Vec<ScopeId>],
    ) -> Vec<Value> {
        let hook = self.local(file, "__getattr__", END);
        let mut lazy_members = BTreeMap::new();
        if let Some(hook) = hook {
            let fallback = self.lazy_dispatch(hook, END, |name, scope, body, argument| {
                if !name.starts_with('_') && self.local(file, name, END).is_none() {
                    lazy_members
                        .entry(name)
                        .or_insert_with(|| Self::lazy_branch(scope, body, argument));
                }
                None
            });
            if !matches!(fallback, LazyMember::Missing) {
                self.scope_issue(
                    file,
                    self.symbols[hook].name.at,
                    "unresolved_lazy_export",
                    "public lazy module members could not be bounded",
                );
            }
        }
        let names: Vec<_> = self.scopes[file]
            .names
            .iter()
            .filter(|(name, _)| !name.starts_with('_'))
            .filter_map(|(&name, bindings)| {
                bindings.last().map(|&id| (name, self.symbols[id].name.at))
            })
            .collect();
        let initialization_imports = OnceCell::new();
        let mut members: Vec<_> = names
            .into_iter()
            .map(|(name, at)| {
                self.module_member_at(
                    &self.files[file].module,
                    name,
                    END,
                    (file, END),
                    ModuleMemberAccess::Export {
                        at,
                        initialization_edges,
                        initialization_imports: &initialization_imports,
                    },
                    0,
                )
            })
            .collect();
        if self.files[file].is_package {
            // Dotted imports can attach public children without a lexical
            // binding in the package. A child found only on disk is absent.
            let prefix = format!("{}.", self.files[file].module);
            let children: Vec<_> = self
                .modules
                .range(prefix.as_str()..)
                .map_while(|(&module, &child)| Some((module.strip_prefix(&prefix)?, child)))
                .filter(|(name, _)| {
                    !name.contains('.')
                        && !name.starts_with('_')
                        && self.local(file, name, END).is_none()
                })
                .collect();
            let at = Location { line: 1, column: 1 };
            let mode = ModuleMemberAccess::Export {
                at,
                initialization_edges,
                initialization_imports: &initialization_imports,
            };
            for (name, child) in children {
                match self.submodule_attachment(file, child, None, (file, END), mode) {
                    SubmoduleAttachment::Attached(import) => {
                        self.read.insert(import);
                        members.push(Value::Module(child));
                        lazy_members.remove(name);
                    }
                    SubmoduleAttachment::Ambiguous => {
                        self.scope_issue(
                            file,
                            at,
                            "ambiguous_binding",
                            format!(
                                "cannot establish whether public submodule {name} was attached"
                            ),
                        );
                        lazy_members.remove(name);
                    }
                    _ => {}
                }
            }
        }
        if let Some(hook) = hook {
            for (name, member) in lazy_members {
                if let LazyMember::Branch(branch) = member {
                    self.read.insert(hook);
                    let value = self.execute_lazy_branch(file, &branch);
                    if !matches!(value, Value::Unknown) {
                        members.push(value);
                    }
                } else {
                    self.scope_issue(
                        file,
                        self.symbols[hook].name.at,
                        "unresolved_lazy_export",
                        format!("public lazy module member {name:?} could not be bounded"),
                    );
                }
            }
        }
        members
    }

    fn retain_package_entrypoints(
        &mut self,
        metadata: &package_entrypoints::Snapshot,
        initialization_edges: &[Vec<ScopeId>],
    ) {
        let mut exports = Vec::new();
        let at = Location { line: 1, column: 1 };
        for entry in &metadata.entries {
            let unresolved = || {
                let reference = if entry.attributes.is_empty() {
                    entry.module.clone()
                } else {
                    format!("{}:{}", entry.module, entry.attributes.join("."))
                };
                let mut issue = Issue::new(
                    "unresolved_package_entrypoint",
                    entry.manifest,
                    format!(
                        "package entry point {reference:?} could not be resolved in the selected sources"
                    ),
                );
                issue.line = entry.line;
                issue
            };
            let Some(&file) = self.modules.get(entry.module.as_str()) else {
                if !metadata.file_only {
                    self.issues.push(unresolved());
                }
                continue;
            };
            // Importing a module-only entry executes its top-level code, which
            // is already part of the compatibility profile's initial roots.
            let mut value = Value::Module(file);
            for attribute in &entry.attributes {
                let access = (file, END);
                value = match value {
                    Value::Module(module) => {
                        let initialization_imports = OnceCell::new();
                        let mode = ModuleMemberAccess::Export {
                            at,
                            initialization_edges,
                            initialization_imports: &initialization_imports,
                        };
                        self.external_member(module, attribute, access, mode)
                    }
                    value => {
                        if let Value::Callable(invocation) = &value
                            && self.is_class(invocation.target)
                        {
                            // Looking up Class.member loads Class without
                            // exposing the rest of its public members.
                            self.edge(file, invocation.target, "package_entrypoint", "certain");
                        }
                        self.member(value, attribute, access, 0)
                    }
                };
            }
            match value {
                Value::Unknown
                | Value::OpaqueNamespace
                | Value::OperatorResult
                | Value::Provider(_) => self.issues.push(unresolved()),
                Value::Module(_) => {}
                value => exports.push(Export {
                    scope: file,
                    at,
                    kind: "package_entrypoint",
                    value,
                }),
            }
        }
        self.retain_exports(exports, initialization_edges, "certain");
    }

    fn external_member(
        &mut self,
        file: usize,
        name: &str,
        access: (ScopeId, Location),
        mode: ModuleMemberAccess<'_>,
    ) -> Value {
        if let ModuleMember::Lazy(hook, branch) =
            self.selected_module_member(file, name, END, access, mode)
        {
            self.read.insert(hook);
            self.execute_lazy_branch(file, &branch)
        } else {
            self.module_member_at(&self.files[file].module, name, END, access, mode, 0)
        }
    }

    fn external_roots(
        &mut self,
        entrypoints: &[Entrypoint],
        initialization_edges: &[Vec<ScopeId>],
    ) -> Vec<Edge> {
        let mut exports = Vec::new();
        for file in 0..self.files.len() {
            let initialization_imports = OnceCell::new();
            if let Some(all) = self.local(file, "__all__", END) {
                let value = self.binding(all, END, 0);
                if let Value::Sequence(export_names) = value {
                    for export in export_names {
                        let Value::Text(export) = export else {
                            self.issue(
                                file,
                                self.symbols[all].name.at,
                                "opaque_exports",
                                "public exports must be literal names",
                            );
                            continue;
                        };
                        let access = (file, END);
                        let mode = ModuleMemberAccess::Export {
                            at: self.symbols[all].name.at,
                            initialization_edges,
                            initialization_imports: &initialization_imports,
                        };
                        let value = self.external_member(file, &export, access, mode);
                        match value {
                            Value::Unknown => self.issue(
                                file,
                                self.symbols[all].name.at,
                                "unresolved_export",
                                format!("public export {export} could not be resolved"),
                            ),
                            value => exports.push(Export {
                                scope: file,
                                at: self.symbols[all].name.at,
                                kind: "public_export",
                                value,
                            }),
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
            let aliases: Vec<_> = self.scopes[file]
                .names
                .values()
                .filter_map(|bindings| {
                    let &latest = bindings.last()?;
                    // The explicit import names the public member. Later
                    // assignments can change the value behind that name.
                    bindings.iter().any(|&id| {
                        matches!(self.symbols[id].binding, Binding::Import(import) if import.declares_export)
                    }).then_some(latest)
                })
                .collect();
            for alias in aliases {
                let name = self.symbols[alias].name;
                let at = name.at;
                let previous_issues = self.scope_issues.len();
                let value = if self.files[file].is_package {
                    self.module_member_at(
                        &self.files[file].module,
                        &name.text,
                        END,
                        (file, END),
                        ModuleMemberAccess::Export {
                            at,
                            initialization_edges,
                            initialization_imports: &initialization_imports,
                        },
                        0,
                    )
                } else {
                    self.binding(alias, END, 0)
                };
                match value {
                    Value::Unknown if self.scope_issues.len() == previous_issues => self
                        .scope_issue(
                            file,
                            at,
                            "unresolved_export",
                            "explicit re-export could not be resolved after initialization",
                        ),
                    Value::Unknown => {}
                    value => exports.push(Export {
                        scope: file,
                        at,
                        kind: "explicit_re_export",
                        value,
                    }),
                }
            }
        }
        self.retain_exports(exports, initialization_edges, "certain");

        let mut roots = Vec::new();
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
                        match self.probe_identity(
                            self.symbols[parent].scope,
                            base,
                            parent_definition.name.at,
                        ) {
                            Value::Provider(name) => {
                                qualifiers.push(("parent_base_class".into(), name))
                            }
                            Value::Callable(Invocation { target: id, .. }) => {
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
                    match self.probe_identity(scope, identity, definition.name.at) {
                        Value::Provider(name) => qualifiers.push((key.into(), name)),
                        Value::Callable(Invocation { target: id, .. }) => {
                            qualifiers.push((key.into(), self.full_name(id)))
                        }
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
                if self.is_class(id) {
                    self.constructor(
                        self.scopes[scope].file,
                        id,
                        "constructor",
                        "possible",
                        definition.name.at,
                    );
                }
            }
        }
        roots
    }

    fn verification_evidence(
        &self,
        id: SymbolId,
        path: &str,
        at: Location,
        kind: &'static str,
    ) -> VerificationEvidence {
        let symbol = &self.symbols[id];
        let file = &self.files[self.scopes[symbol.scope].file];
        let qualified_name = self.full_name(id);
        let Location { line, column } = symbol.name.at;
        VerificationEvidence {
            symbol_id: format!("{}::{qualified_name}@{line}:{column}", file.path),
            qualified_name,
            definition_path: file.path.clone(),
            definition_line: line,
            definition_column: column,
            path: path.to_owned(),
            line: at.line,
            column: at.column,
            kind,
            certainty: "possible",
        }
    }

    fn configuration_roots(
        &self,
        snapshot: &grep_verification::Snapshot,
        derived: &mut Verification,
    ) -> bool {
        if !snapshot.has_configuration_references() {
            return true;
        }
        let effective: BTreeSet<_> = self
            .scopes
            .iter()
            .flat_map(|scope| scope.names.values())
            .flat_map(|bindings| self.effective_bindings(bindings))
            .copied()
            .filter(|&id| matches!(self.symbols[id].binding, Binding::Definition(..)))
            .collect();
        for &id in &effective {
            let symbol = &self.symbols[id];
            let Binding::Definition(definition, _) = symbol.binding else {
                continue;
            };
            let scope = &self.scopes[symbol.scope];
            // Only a module member or a member of an addressable class can
            // be selected by a qualified configuration reference.
            let mut owner = scope.owner;
            while let Some(id) = owner {
                if !self.is_class(id) || !effective.contains(&id) {
                    break;
                }
                owner = self.scopes[self.symbols[id].scope].owner;
            }
            if owner.is_some() {
                continue;
            }
            let file = &self.files[scope.file];
            let qualified = self.full_name(id);
            let member = &qualified[file.module.len() + 1..];
            let include_module_reference =
                scope.owner.is_none() && definition.kind != DefinitionKind::Class;
            for reference in
                snapshot.matches(&file.path, &file.module, member, include_module_reference)
            {
                if derived.evidence.len() + self.dynamic_references.len()
                    >= grep_verification::MAX_EVIDENCE
                {
                    return false;
                }
                derived.roots.insert(id);
                derived.evidence.push(self.verification_evidence(
                    id,
                    &reference.path,
                    reference.at,
                    reference.kind,
                ));
            }
        }
        true
    }

    fn runtime_roots(
        &self,
        snapshot: &runtime_evidence::Snapshot,
        roots: &mut Vec<Edge>,
        possible_roots: &mut Vec<Edge>,
    ) -> (Vec<runtime_evidence::Evidence>, Vec<Issue>) {
        type Observed<'a> = &'a runtime_evidence::Observation;
        let mut by_location = BTreeMap::<(&str, usize), Vec<Observed<'_>>>::new();
        for observation in &snapshot.observations {
            by_location
                .entry((&observation.file, observation.line))
                .or_default()
                .push(observation);
        }
        let mut matched = BTreeMap::<Observed<'_>, Option<SymbolId>>::new();
        let mut declarations = BTreeMap::<Observed<'_>, Option<SymbolId>>::new();
        for (id, symbol) in self.symbols.iter().enumerate() {
            let Binding::Definition(definition, body) = symbol.binding else {
                continue;
            };
            if definition.kind == DefinitionKind::Class {
                continue;
            }
            let file = self.files[self.scopes[body].file].path.as_str();
            let mut observe = |line, function, declaration| {
                if let Some(observations) = by_location.get(&(file, line)) {
                    for &observation in observations {
                        if observation.function.as_deref() != function {
                            continue;
                        }
                        let matches = if declaration {
                            if !observation.allow_declaration {
                                continue;
                            }
                            &mut declarations
                        } else {
                            &mut matched
                        };
                        matches
                            .entry(observation)
                            .and_modify(|previous| {
                                if *previous != Some(id) {
                                    *previous = None;
                                }
                            })
                            .or_insert(Some(id));
                    }
                }
            };
            let name = Some(definition.name.text.as_str());
            observe(definition.name.at.line, name, false);
            observe(definition.name.at.line, None, true);
            if let Some(decorator) = definition.decorators.first() {
                observe(decorator.at.line, name, false);
            }
            for &line in &definition.body_coverage_lines {
                if line != definition.name.at.line {
                    observe(line, None, false);
                }
            }
        }
        let mut evidence = Vec::new();
        let mut rejected = BTreeMap::<&str, usize>::new();
        for observation in &snapshot.observations {
            let (candidate, kind) = match matched.get(observation) {
                Some(target) => (*target, observation.kind()),
                None => (
                    declarations.get(observation).copied().flatten(),
                    runtime_evidence::COVERAGE_DECLARATION,
                ),
            };
            let Some(target) = candidate else {
                *rejected.entry(&observation.artifact).or_default() += 1;
                continue;
            };
            let (roots, certainty) = match observation.provenance {
                runtime_evidence::Provenance::Verified => (&mut *roots, "certain"),
                runtime_evidence::Provenance::Unverified => (&mut *possible_roots, "possible"),
            };
            roots.push(Edge {
                target,
                kind,
                certainty,
            });
            if kind != runtime_evidence::COVERAGE_DECLARATION {
                // A nested callable can only be observed after an enclosing
                // activation created it. Keep the dependencies of that activation.
                let mut owner = self.scopes[self.symbols[target].scope].owner;
                while let Some(target) = owner {
                    possible_roots.push(Edge {
                        target,
                        kind: "runtime_owner",
                        certainty: "possible",
                    });
                    owner = self.scopes[self.symbols[target].scope].owner;
                }
            }
            evidence.push(runtime_evidence::Evidence {
                artifact: observation.artifact.clone(),
                file: observation.file.clone(),
                line: observation.line,
                full_name: self.full_name(target),
                kind,
                provenance: observation.provenance,
            });
        }
        let issues = rejected
            .into_iter()
            .map(|(path, count)| {
                Issue::new(
                    "runtime_evidence_rejected",
                    path,
                    format!(
                        "{count} observations did not match a unique selected callable location"
                    ),
                )
            })
            .collect();
        (evidence, issues)
    }

    fn traverse(
        &self,
        reach: &mut Reachability,
        mut pending: VecDeque<Edge>,
        class_bodies: &BTreeSet<ScopeId>,
        verification: bool,
        possible: bool,
    ) -> bool {
        for _ in 0..MAX_FRAMEWORK_PASSES {
            let before = (
                reach.scopes.len(),
                reach.active_objects.len(),
                reach.symbols.len(),
            );
            while let Some(edge) = pending.pop_front() {
                if !verification && is_verification_edge(edge.kind) {
                    continue;
                }
                let certainty = if possible { "possible" } else { edge.certainty };
                reach.evidence.insert((edge.target, edge.kind, certainty));
                reach.symbols.insert(edge.target);
                if edge.kind == "type_reference" {
                    continue;
                }
                let Binding::Definition(_, scope) = self.symbols[edge.target].binding else {
                    continue;
                };
                if reach.scopes.insert(scope) {
                    pending.extend(&self.scopes[scope].edges);
                }
            }
            for &scope in class_bodies {
                if self.scopes[scope]
                    .parent
                    .is_some_and(|parent| reach.scopes.contains(&parent))
                    && reach.scopes.insert(scope)
                {
                    pending.extend(&self.scopes[scope].edges);
                }
            }
            for &scope in &reach.scopes {
                let scope = &self.scopes[scope];
                reach.active_objects.extend(&scope.activated_apps);
                for &(parent, child) in &scope.mounts {
                    if reach.active_objects.contains(&parent) {
                        reach.active_objects.insert(child);
                    }
                }
                for &(object, target) in &scope.routes {
                    if reach.active_objects.contains(&object) && !reach.symbols.contains(&target) {
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
                        reach.scopes.len(),
                        reach.active_objects.len(),
                        reach.symbols.len(),
                    )
            {
                return true;
            }
        }
        false
    }

    fn verification_edges(
        &self,
        reach: &Reachability,
        roots: &BTreeSet<SymbolId>,
    ) -> VecDeque<Edge> {
        let mut pending: VecDeque<_> = roots
            .iter()
            .map(|&target| Edge {
                target,
                kind: "grep_configuration",
                certainty: "possible",
            })
            .collect();
        pending.extend(reach.scopes.iter().flat_map(|&scope| {
            self.scopes[scope]
                .edges
                .iter()
                .copied()
                .filter(|edge| is_verification_edge(edge.kind))
        }));
        pending
    }

    fn finish(
        mut self,
        roots: Vec<Edge>,
        mut possible_runtime_roots: Vec<Edge>,
        snapshot: Option<&grep_verification::Snapshot>,
    ) -> Analysis {
        // Declaration hints retain only the definition in the compatibility
        // report. Leave them out of the execution graph and parameter analysis.
        let mut coverage_declarations = BTreeSet::new();
        possible_runtime_roots.retain(|edge| {
            if edge.kind == runtime_evidence::COVERAGE_DECLARATION {
                coverage_declarations.insert(edge.target);
                false
            } else {
                true
            }
        });
        self.resolve_class_invocations();
        let mut reach = Reachability {
            scopes: (0..self.files.len()).collect(),
            ..Reachability::default()
        };
        let runtime_targets: BTreeSet<_> = roots
            .iter()
            .chain(&possible_runtime_roots)
            .filter(|edge| is_runtime_execution_edge(edge.kind))
            .map(|edge| edge.target)
            .collect();
        let observed_callback = !runtime_targets.is_empty()
            && self.parameter_calls.iter().any(|call| {
                self.scopes[self.symbols[call.parameter].scope]
                    .owner
                    .is_some_and(|owner| runtime_targets.contains(&owner))
            });
        let mut pending = VecDeque::from(roots);
        // Only visited class declarations execute when their parent is reached.
        let class_bodies: BTreeSet<_> = self
            .scopes
            .iter()
            .flat_map(|scope| scope.class_bodies.iter().copied())
            .collect();
        for &scope in &class_bodies {
            if self.scopes[scope]
                .parent
                .is_some_and(|parent| reach.scopes.contains(&parent))
            {
                reach.scopes.insert(scope);
            }
        }
        for &scope in &reach.scopes {
            pending.extend(&self.scopes[scope].edges);
        }
        // Check whether a runtime observation of a callback owner has an
        // independent static witness. Calls reached only through the runtime
        // roots, such as a recursive call, cannot explain the initial call.
        let mut static_reach = None;
        if observed_callback {
            let mut static_only = Reachability {
                scopes: reach.scopes.clone(),
                ..Reachability::default()
            };
            let static_pending = pending
                .iter()
                .copied()
                .filter(|edge| !is_runtime_execution_edge(edge.kind))
                .collect();
            if self.traverse(
                &mut static_only,
                static_pending,
                &class_bodies,
                false,
                false,
            ) {
                static_reach = Some(static_only);
            }
        }
        let baseline_converged = self.traverse(&mut reach, pending, &class_bodies, false, false);
        if !possible_runtime_roots.is_empty()
            && !self.traverse(
                &mut reach,
                possible_runtime_roots.into(),
                &class_bodies,
                false,
                true,
            )
        {
            self.issues.push(framework_limit());
        }
        let mut verification = Verification::default();
        if let Some(snapshot) = snapshot {
            let baseline = reach.symbols.clone();
            if !self.configuration_roots(snapshot, &mut verification) {
                self.issues.push(grep_verification::evidence_limit());
            }
            if let Some(mut static_only) = static_reach.take() {
                let pending = self.verification_edges(&static_only, &verification.roots);
                if pending.is_empty()
                    || self.traverse(&mut static_only, pending, &class_bodies, true, true)
                {
                    static_reach = Some(static_only);
                }
            }
            let pending = self.verification_edges(&reach, &verification.roots);
            if !pending.is_empty() && !self.traverse(&mut reach, pending, &class_bodies, true, true)
            {
                self.issues.push(framework_limit());
            }
            for &(scope, target, line, column) in &self.dynamic_references {
                if reach.scopes.contains(&scope) && reach.symbols.contains(&target) {
                    if !baseline.contains(&target) {
                        verification.roots.insert(target);
                    }
                    verification.evidence.push(self.verification_evidence(
                        target,
                        &self.files[self.scopes[scope].file].path,
                        Location { line, column },
                        "dynamic_member",
                    ));
                }
            }
            verification.rescued_count = verification.roots.difference(&baseline).count();
            verification.transitive = reach
                .symbols
                .difference(&baseline)
                .filter(|target| !verification.roots.contains(target))
                .copied()
                .collect();
        }
        if !baseline_converged {
            self.issues.push(framework_limit());
        }
        let Reachability {
            scopes: reachable_scopes,
            symbols: reachable_symbols,
            evidence,
            ..
        } = reach;
        let independently_retained: BTreeSet<_> = evidence
            .iter()
            .filter_map(|&(target, kind, _)| {
                if matches!(kind, "direct_call" | "grep_dynamic_call" | "type_reference") {
                    return None;
                }
                if is_runtime_execution_edge(kind)
                    && let Binding::Definition(_, body) = self.symbols[target].binding
                    && static_reach
                        .as_ref()
                        .is_some_and(|reach| reach.scopes.contains(&body))
                {
                    return None;
                }
                Some(target)
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
                && callers.all(|callsite| {
                    matches!(
                        callsite.values.get(&parameter.name.text),
                        Some(Value::Callable(_))
                    )
                });
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
        let transitive = verification
            .transitive
            .iter()
            .map(|&target| (target, "reachable_from_root", "possible"));
        let declaration_evidence = coverage_declarations
            .iter()
            .map(|&target| (target, runtime_evidence::COVERAGE_DECLARATION, "possible"));
        for (id, kind, certainty) in evidence
            .into_iter()
            .chain(transitive)
            .chain(declaration_evidence)
        {
            if is_verification_edge(kind) {
                continue;
            }
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
                    if !reachable_symbols.contains(&id)
                        && !coverage_declarations.contains(&id)
                        && def.kind != DefinitionKind::Class =>
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
                            self.fixtures.contains_key(&owner)
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
            verification: verification.evidence,
            rescued_count: verification.rescued_count,
            ..Analysis::default()
        }
    }
}

#[derive(Default)]
struct Reachability {
    scopes: BTreeSet<ScopeId>,
    symbols: BTreeSet<SymbolId>,
    active_objects: BTreeSet<Object>,
    evidence: BTreeSet<(SymbolId, &'static str, &'static str)>,
}

#[derive(Default)]
struct Verification {
    roots: BTreeSet<SymbolId>,
    transitive: BTreeSet<SymbolId>,
    evidence: Vec<VerificationEvidence>,
    rescued_count: usize,
}

fn is_verification_edge(kind: &str) -> bool {
    matches!(
        kind,
        "grep_configuration" | "grep_dynamic" | "grep_dynamic_call"
    )
}

fn is_runtime_execution_edge(kind: &str) -> bool {
    matches!(kind, "runtime_trace" | "runtime_coverage" | "runtime_owner")
}

fn framework_limit() -> Issue {
    Issue::new("analysis_limit", ".", "framework activation limit exceeded")
}

fn mapping_entries(keywords: BTreeMap<String, Value>) -> impl Iterator<Item = Value> {
    keywords
        .into_iter()
        .flat_map(|(name, value)| [Value::Text(name), value])
}

fn position(at: Location) -> [usize; 2] {
    [at.line, at.column]
}

fn namespace_span(body: &[Statement]) -> Option<RangeInclusive<[usize; 2]>> {
    body.first()
        .map(|first| position(first.at)..=[suite_end(body)[0], usize::MAX])
}

// Python creates closure cells from lexical references even in skipped code.
fn namespace_free_names(definition: &Definition, postponed: bool) -> BTreeSet<&str> {
    let class = definition.kind == DefinitionKind::Class;
    let mut assigned: BTreeSet<_> = definition
        .parameters
        .iter()
        .map(|p| p.text.as_str())
        .collect();
    let mut nested_reads = BTreeSet::new();
    let mut expressions = Vec::new();
    let mut statements: Vec<_> = definition.body.iter().collect();
    while let Some(statement) = statements.pop() {
        match &statement.kind {
            StatementKind::Define(child) => {
                assigned.insert(child.name.text.as_str());
                nested_reads.extend(namespace_free_names(child, postponed));
                expressions.extend(&child.decorators);
                expressions.extend(&child.bases);
                expressions.extend(&child.defaults);
                if !postponed {
                    expressions.extend(&child.annotations);
                }
            }
            StatementKind::Import(import) => {
                assigned.insert(import.local.text.as_str());
            }
            StatementKind::Assign {
                names,
                value,
                annotation,
            } => {
                assigned.extend(names.iter().map(|name| name.text.as_str()));
                expressions.extend(value);
                if class && !postponed {
                    expressions.extend(annotation);
                }
            }
            StatementKind::If {
                test,
                body,
                otherwise,
            } => {
                expressions.push(test);
                statements.extend(body);
                statements.extend(otherwise);
            }
            StatementKind::With { items, body } => {
                for (value, name) in items {
                    expressions.push(value);
                    assigned.extend(name.iter().map(|name| name.text.as_str()));
                }
                statements.extend(body);
            }
            StatementKind::Return(value) | StatementKind::Raise(value) => expressions.extend(value),
            StatementKind::Assert(value) | StatementKind::Evaluate(value) => {
                expressions.push(value)
            }
        }
    }
    let mut reads = BTreeSet::new();
    while let Some(expression) = expressions.pop() {
        match &expression.kind {
            ExpressionKind::Name(name) => {
                reads.insert(name.as_str());
            }
            ExpressionKind::Member(value, _)
            | ExpressionKind::Not(value)
            | ExpressionKind::Await(value) => expressions.push(value),
            ExpressionKind::Call {
                function,
                arguments,
                keywords,
            } => {
                expressions.push(function);
                expressions.extend(arguments);
                expressions.extend(keywords.iter().map(|(_, value)| value));
            }
            ExpressionKind::Sequence(values)
            | ExpressionKind::Dictionary(values)
            | ExpressionKind::Boolean { values, .. }
            | ExpressionKind::Operands(values) => expressions.extend(values),
            ExpressionKind::Compare { left, right, .. }
            | ExpressionKind::OrderingComparison { left, right } => {
                expressions.extend([left.as_ref(), right.as_ref()]);
            }
            ExpressionKind::Subscript { value, index } => {
                expressions.extend([value.as_ref(), index.as_ref()]);
            }
            ExpressionKind::Conditional {
                test,
                body,
                otherwise,
            } => expressions.extend([test.as_ref(), body.as_ref(), otherwise.as_ref()]),
            ExpressionKind::Membership {
                element, container, ..
            } => expressions.extend([element.as_ref(), container.as_ref()]),
            ExpressionKind::Text(_) | ExpressionKind::Bool(_) | ExpressionKind::Scalar => {}
        }
    }
    reads.retain(|name| !assigned.contains(name));
    // Class-local names cannot supply cells to nested functions.
    if !class {
        nested_reads.retain(|name| !assigned.contains(name));
    }
    reads.extend(nested_reads);
    reads
}

// Function locals are declared at compile time, including bindings in
// branches that the runtime index can prove are never executed.
fn suite_binds_name(body: &[Statement], name: &str) -> bool {
    body.iter().any(|statement| match &statement.kind {
        StatementKind::Define(definition) => definition.name.text == name,
        StatementKind::Import(import) => import.local.text == name,
        StatementKind::Assign { names, .. } => names.iter().any(|target| target.text == name),
        StatementKind::If {
            body, otherwise, ..
        } => suite_binds_name(body, name) || suite_binds_name(otherwise, name),
        StatementKind::With { items, body } => {
            items
                .iter()
                .any(|(_, target)| target.as_ref().is_some_and(|target| target.text == name))
                || suite_binds_name(body, name)
        }
        _ => false,
    })
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

fn within_package(module: &str, package: &str) -> bool {
    module
        .strip_prefix(package)
        .is_some_and(|suffix| suffix.is_empty() || suffix.starts_with('.'))
}

fn pytest_conftest_prefix(path: &str) -> Option<&str> {
    path.strip_suffix("conftest.py")
        .filter(|prefix| prefix.is_empty() || prefix.ends_with('/'))
}

fn has_implicit_module_member(name: &str, is_package: bool) -> bool {
    // Source modules inherit attributes from ModuleType and its base object.
    matches!(
        name,
        "__name__"
            | "__doc__"
            | "__package__"
            | "__loader__"
            | "__spec__"
            | "__file__"
            | "__cached__"
            | "__builtins__"
            | "__annotations__"
            | "__class__"
            | "__delattr__"
            | "__dict__"
            | "__dir__"
            | "__eq__"
            | "__format__"
            | "__ge__"
            | "__getattribute__"
            | "__getstate__"
            | "__gt__"
            | "__hash__"
            | "__init__"
            | "__init_subclass__"
            | "__le__"
            | "__lt__"
            | "__ne__"
            | "__new__"
            | "__reduce__"
            | "__reduce_ex__"
            | "__repr__"
            | "__setattr__"
            | "__sizeof__"
            | "__str__"
            | "__subclasshook__"
    ) || (is_package && name == "__path__")
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
                | "setuptools.setup"
                | "distutils.core.setup"
        )
}

fn builtin_noop_subclass_hook(name: &str) -> bool {
    // These subclassable built-ins inherit object's no-op hook.
    matches!(
        name,
        "builtins.object"
            | "builtins.str"
            | "builtins.int"
            | "builtins.list"
            | "builtins.dict"
            | "builtins.tuple"
            | "builtins.set"
            | "builtins.float"
            | "builtins.bytes"
            | "builtins.ValueError"
            | "builtins.RuntimeError"
            | "builtins.AttributeError"
            | "builtins.TypeError"
            | "builtins.Exception"
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
    fn local_namespace_observes_possible_bindings_at_the_call() {
        let files = [module(
            "main.py",
            "main",
            r#"import os

def _entry(flag):
    visible = 0
    visible = 1
    if flag:
        visible = 2
    ignored: int
    def callback():
        return 0
    result = locals()
    later = 1
    return result

_entry(True)
"#,
        )];
        let result = analyze(&files, &[]);
        assert!(result.issues.is_empty(), "{:?}", result.issues);
        assert_eq!(
            names(&result, Category::Variable),
            ["ignored", "later", "visible"]
        );
        assert!(result.findings.iter().any(|finding| {
            finding.category == Category::Variable && finding.name == "visible" && finding.line == 4
        }));
        assert_eq!(names(&result, Category::Import), ["os"]);
        assert!(result.evidence.iter().any(|evidence| {
            evidence.full_name == "main._entry.callback"
                && evidence.kind == "local_namespace"
                && evidence.certainty == "possible"
        }));
    }

    #[test]
    fn escaping_local_namespace_retains_instance_calls_and_constructors() {
        let files = [module(
            "pkg/__init__.py",
            "pkg",
            r#"__all__ = ["build", "register"]

def handle():
    return 42

class Handler:
    def __call__(self):
        return handle()

def build():
    snapshot = locals()
    action = Handler()
    vars()
    return snapshot

def initialize():
    return 1

def register():
    class Plugin:
        def __init__(self):
            initialize()
    return dict(vars(), registered=True)

def unused():
    return 0
"#,
        )];
        let result = analyze(&files, &[]);
        assert!(result.issues.is_empty(), "{:?}", result.issues);
        assert_eq!(names(&result, Category::Function), ["unused"]);
        for name in ["pkg.Handler.__call__", "pkg.register.Plugin.__init__"] {
            assert!(
                result
                    .evidence
                    .iter()
                    .any(|entry| { entry.full_name == name && entry.certainty == "possible" }),
                "{name}: {:?}",
                result.evidence
            );
        }
    }

    #[test]
    fn escaping_composite_namespace_reports_unresolved_values() {
        for expression in [
            "debug is True and locals() or {}",
            "vars() | {'version': 1}",
            "dict(vars() | {'version': 1})",
            "dict({'snapshot': vars() | {'version': 1}}, status='ok')",
        ] {
            let source = format!(
                r#"__all__ = ["build"]

def build(debug):
    def action():
        return 42
    namespace = {expression}
    return {{"snapshot": namespace}}
"#
            );
            let result = analyze(&[module("pkg/__init__.py", "pkg", &source)], &[]);
            assert!(
                result
                    .issues
                    .iter()
                    .any(|issue| issue.kind == "unresolved_export" && issue.line == Some(7)),
                "{expression}: {:?}",
                result.issues
            );
        }
    }

    #[test]
    fn dictionary_keywords_preserve_nested_namespace_entries() {
        let files = [module(
            "pkg/__init__.py",
            "pkg",
            r#"__all__ = ["build"]

def handle():
    return 42

def build():
    def action():
        return handle()
    return dict({"namespace": locals()}, status="ok")
"#,
        )];
        let result = analyze(&files, &[]);
        assert!(result.issues.is_empty(), "{:?}", result.issues);
        assert!(
            names(&result, Category::Function).is_empty(),
            "{:?}",
            result.findings
        );
        assert!(result.evidence.iter().any(|entry| {
            entry.full_name == "pkg.build.action" && entry.kind == "local_namespace"
        }));
    }

    #[test]
    fn namespace_dictionary_copy_respects_overrides_and_refreshes() {
        let files = [module(
            "pkg/__init__.py",
            "pkg",
            r#"from builtins import locals as collect

__all__ = ["build", "snapshot"]

def build():
    def callback(consumer):
        return consumer()
    def retained():
        return 42
    return dict(collect(), callback=None)

def snapshot():
    copied = dict(collect())
    def after_copy():
        return 42
    collect()
    return copied
"#,
        )];
        let result = analyze(&files, &[]);
        assert!(result.issues.is_empty(), "{:?}", result.issues);
        assert_eq!(
            names(&result, Category::Function),
            ["after_copy", "callback"]
        );
    }

    #[test]
    fn namespace_copy_includes_refreshes_during_argument_evaluation() {
        let files = [module(
            "pkg/__init__.py",
            "pkg",
            r#"__all__ = ["build", "snapshot"]

def handle():
    return 42

def build():
    saved = locals()
    def action():
        return handle()
    return dict(saved, size=len(locals()))

def snapshot():
    saved = locals()
    def too_late():
        return 42
    return dict(saved), len(locals())
"#,
        )];
        let result = analyze(&files, &[]);
        assert!(result.issues.is_empty(), "{:?}", result.issues);
        assert_eq!(names(&result, Category::Function), ["too_late"]);
        assert!(result.evidence.iter().any(|entry| {
            entry.full_name == "pkg.build.action" && entry.kind == "local_namespace"
        }));
    }

    #[test]
    fn namespace_truth_test_does_not_export_a_proven_nonempty_namespace() {
        let files = [module(
            "pkg/__init__.py",
            "pkg",
            r#"from builtins import locals as collect

__all__ = ["flag"]

def flag(enabled):
    def unused(consumer):
        return consumer()
    return collect() and True
"#,
        )];
        let result = analyze(&files, &[]);
        assert!(result.issues.is_empty(), "{:?}", result.issues);
        assert_eq!(names(&result, Category::Function), ["unused"]);
    }

    #[test]
    fn escaping_namespace_retains_executor_arguments() {
        let files = [module(
            "pkg/__init__.py",
            "pkg",
            r#"from concurrent.futures import ThreadPoolExecutor

__all__ = ["build"]

def handle():
    return 42

class Action:
    def __call__(self):
        return handle()

def worker(action):
    return locals()

def build():
    with ThreadPoolExecutor() as executor:
        return executor.submit(worker, action=Action())

class Dormant:
    def __call__(self, consumer):
        return consumer()

def unused():
    with ThreadPoolExecutor() as executor:
        return executor.submit(worker, Dormant())
"#,
        )];
        let result = analyze(&files, &[]);
        assert!(result.issues.is_empty(), "{:?}", result.issues);
        assert_eq!(names(&result, Category::Function), ["unused"]);
        assert_eq!(names(&result, Category::Class), ["Dormant"]);
        assert!(result.evidence.iter().any(|entry| {
            entry.full_name == "pkg.Action.__call__"
                && entry.kind == "local_namespace"
                && entry.certainty == "possible"
        }));
    }

    #[test]
    fn escaping_local_namespace_retains_project_arguments() {
        let files = [module(
            "pkg/__init__.py",
            "pkg",
            r#"__all__ = ["build"]

def live():
    return 42

class Action:
    def __call__(self):
        return live()

def _namespace(action):
    return locals()

def build():
    return _namespace(action=Action())

class Dormant:
    def __call__(self, callback):
        return callback()

def unused():
    return _namespace(Dormant())
"#,
        )];
        let result = analyze(&files, &[]);
        assert!(result.issues.is_empty(), "{:?}", result.issues);
        assert_eq!(names(&result, Category::Function), ["unused"]);
        assert_eq!(names(&result, Category::Class), ["Dormant"]);
        assert!(result.evidence.iter().any(|entry| {
            entry.full_name == "pkg.Action.__call__"
                && entry.kind == "local_namespace"
                && entry.certainty == "possible"
        }));
    }

    #[test]
    fn escaping_local_namespace_retains_coroutine_targets() {
        let files = [module(
            "pkg/__init__.py",
            "pkg",
            r#"__all__ = ["build"]

def handle():
    return 42

async def process():
    return handle()

def build():
    task = process()
    return locals()

async def unused():
    return 0
"#,
        )];
        let result = analyze(&files, &[]);
        assert!(result.issues.is_empty(), "{:?}", result.issues);
        assert_eq!(names(&result, Category::Function), ["unused"]);
        assert!(result.evidence.iter().any(|entry| {
            entry.full_name == "pkg.process"
                && entry.kind == "local_namespace"
                && entry.certainty == "possible"
        }));
    }

    #[test]
    fn scalar_local_namespace_equality_rules_out_unrelated_hooks() {
        for (expression, expected_issues) in [
            ("collect() == {'enabled': True}", 0),
            ("collect() == {'enabled': Entry()}", 1),
        ] {
            let source = format!(
                r#"from builtins import locals as collect

__all__ = ["check"]

class Entry:
    def __eq__(self, other):
        return False

def check():
    enabled = True
    return {expression}
"#
            );
            let result = analyze(&[module("pkg/__init__.py", "pkg", &source)], &[]);
            assert_eq!(
                result.issues.len(),
                expected_issues,
                "{expression}: {:?}",
                result.issues
            );
            assert!(
                result
                    .issues
                    .iter()
                    .all(|issue| issue.kind == "unresolved_comparison")
            );
        }
    }

    #[test]
    fn escaping_local_namespace_reports_opaque_local_values() {
        let files = [module(
            "pkg/__init__.py",
            "pkg",
            r#"__all__ = ["build"]

class Handler:
    def run(self):
        return 42

def make():
    return Handler()

def build():
    action = make()
    return locals()
"#,
        )];
        let result = analyze(&files, &[]);
        assert!(
            result
                .issues
                .iter()
                .any(|issue| { issue.kind == "unresolved_export" && issue.line == Some(12) }),
            "{:?}",
            result.issues
        );
    }

    #[test]
    fn counting_local_namespace_entries_does_not_expose_callables() {
        let files = [module(
            "pkg/__init__.py",
            "pkg",
            r#"from builtins import locals as collect

__all__ = ["count"]

class Sized:
    def __len__(self):
        return 1

def count(preview):
    if preview is True:
        def unused(handler):
            return handler()
        namespace = collect()
        return len(namespace)
    return vars()
"#,
        )];
        let result = analyze(&files, &[]);
        assert!(result.issues.is_empty(), "{:?}", result.issues);
        assert_eq!(names(&result, Category::Function), ["unused"]);
    }

    #[test]
    fn local_namespace_excludes_assignments_from_exited_branches() {
        let files = [module(
            "pkg/__init__.py",
            "pkg",
            r#"__all__ = ["build"]

def build(enabled):
    if enabled:
        unused = 1
        return {}
    else:
        carried = 2
    return locals()
"#,
        )];
        let result = analyze(&files, &[]);
        assert!(result.issues.is_empty(), "{:?}", result.issues);
        assert_eq!(names(&result, Category::Variable), ["unused"]);
    }

    #[test]
    fn local_namespace_retains_compiler_captures_including_skipped_reads() {
        let files = [module(
            "pkg/__init__.py",
            "pkg",
            r#"__all__ = ["build"]
DEBUG = False

def build():
    context = object()
    captured_through_child = object()
    unrelated = object()
    def collect():
        if DEBUG:
            print(context)
        def child():
            if DEBUG:
                print(captured_through_child)
        return vars()
    return collect()
"#,
        )];
        let result = analyze(&files, &[]);
        assert!(result.issues.is_empty(), "{:?}", result.issues);
        assert_eq!(names(&result, Category::Variable), ["unrelated"]);
    }

    #[test]
    fn local_namespace_requires_a_proven_zero_argument_builtin() {
        let files = [module(
            "main.py",
            "main",
            r#"from builtins import vars as collect

def _entry():
    def locals():
        return 0
    unread = 1
    return locals()

def _opaque(value, source):
    candidate = 1
    collect(value)
    return eval(source)

def _unbound():
    if False:
        vars = object
    return vars()

_entry()
_opaque(object(), "pass")
_unbound()
"#,
        )];
        let result = analyze(&files, &[]);
        assert_eq!(names(&result, Category::Variable), ["candidate", "unread"]);
        assert_eq!(
            result
                .issues
                .iter()
                .map(|issue| (issue.kind, issue.line))
                .collect::<BTreeSet<_>>(),
            [
                ("unresolved_call", Some(11)),
                ("unresolved_call", Some(12)),
                ("unresolved_call", Some(17))
            ]
            .into()
        );
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
    fn resolves_literal_lazy_import_with_real_loader_identity() {
        let package = |loader: &str, name: &str| {
            module(
                "pkg/__init__.py",
                "pkg",
                &format!(
                    "{loader}\n__all__ = ['work']\ndef __getattr__(name):\n    if name == 'work':\n        return getattr(load({name}), name)\n    raise AttributeError(name)\n"
                ),
            )
        };
        let provider = || {
            module(
                "pkg/worker.py",
                "pkg.worker",
                "def work():\n    return 1\ndef unused():\n    return 2\n",
            )
        };
        let import = "from importlib import import_module as load";
        let result = analyze(&[package(import, "'pkg.worker'"), provider()], &[]);
        assert!(result.issues.is_empty(), "{:?}", result.issues);
        assert_eq!(names(&result, Category::Function), ["unused"]);
        assert!(result.evidence.iter().any(|evidence| {
            evidence.full_name == "pkg.worker.work" && evidence.kind == "public_export"
        }));

        let opaque = analyze(&[package(import, "name"), provider()], &[]);
        assert!(
            opaque
                .issues
                .iter()
                .any(|issue| issue.kind == "unresolved_lazy_export")
        );
        let unselected_parent = analyze(
            &[
                package(import, "'pkg.sub.worker'"),
                module(
                    "pkg/sub/worker.py",
                    "pkg.sub.worker",
                    "def work():\n    return 1\n",
                ),
            ],
            &[],
        );
        assert!(
            unselected_parent
                .issues
                .iter()
                .any(|issue| issue.kind == "unresolved_lazy_export")
        );

        for hook in [
            "def __getattr__(name):\n    if name == 'work':\n        return getattr(load('worker'), name)\n    if False:\n        getattr = None\n    raise AttributeError(name)\n",
            "@classmethod\ndef __getattr__(name):\n    if name == 'work':\n        return getattr(load('worker'), name)\n    raise AttributeError(name)\n",
        ] {
            let unresolved = analyze(
                &[
                    module(
                        "pkg/__init__.py",
                        "pkg",
                        &format!("{import}\n__all__ = ['work']\n{hook}"),
                    ),
                    module("worker.py", "worker", "def work():\n    return 1\n"),
                ],
                &[],
            );
            assert!(
                unresolved
                    .issues
                    .iter()
                    .any(|issue| issue.kind == "unresolved_lazy_export")
            );
        }

        let collision = analyze(
            &[
                package(
                    "from importlib import import_module as load\ndef worker():\n    return 3",
                    "'pkg.worker'",
                ),
                provider(),
                module(
                    "main.py",
                    "main",
                    "import pkg\nfrom pkg import work\nwork()\npkg.worker()\n",
                ),
            ],
            &[],
        );
        assert!(
            collision
                .issues
                .iter()
                .any(|issue| issue.kind == "unresolved_lazy_export")
        );

        let shadowed = analyze(
            &[
                package(import, "'pkg.worker'"),
                provider(),
                module(
                    "importlib.py",
                    "importlib",
                    "def import_module(name):\n    return None\n",
                ),
            ],
            &[],
        );
        assert!(
            shadowed
                .issues
                .iter()
                .any(|issue| issue.kind == "unresolved_lazy_export")
        );
    }

    #[test]
    fn resolves_literal_lazy_import_tables() {
        let files = [
            module(
                "pkg/__init__.py",
                "pkg",
                "import importlib\n_LAZY = {'work': 'pkg.worker'}\n__all__ = ['work']\ndef __getattr__(name):\n    if name in _LAZY:\n        return getattr(importlib.import_module(_LAZY[name]), name)\n    raise AttributeError(name)\n",
            ),
            module(
                "pkg/worker.py",
                "pkg.worker",
                "def work():\n    return helper()\ndef helper():\n    return 1\ndef unused():\n    return 2\n",
            ),
        ];
        let result = analyze(&files, &[]);
        assert!(result.issues.is_empty(), "{:?}", result.issues);
        assert_eq!(names(&result, Category::Function), ["unused"]);
        assert!(result.evidence.iter().any(|evidence| {
            evidence.full_name == "pkg.worker.work" && evidence.kind == "public_export"
        }));

        let duplicate = analyze(
            &[
                module(
                    "pkg/__init__.py",
                    "pkg",
                    "import importlib\n_LAZY = {'work': 'pkg.retired', 'work': 'pkg.worker'}\n__all__ = ['work']\ndef __getattr__(name):\n    if name in _LAZY:\n        return getattr(importlib.import_module(_LAZY[name]), name)\n    raise AttributeError(name)\n",
                ),
                module("pkg/worker.py", "pkg.worker", "def work():\n    return 1\n"),
                module(
                    "pkg/retired.py",
                    "pkg.retired",
                    "def work():\n    return 2\n",
                ),
            ],
            &[],
        );
        assert!(duplicate.issues.is_empty(), "{:?}", duplicate.issues);
        assert_eq!(names(&duplicate, Category::Function), ["work"]);
        assert!(duplicate.evidence.iter().any(|evidence| {
            evidence.full_name == "pkg.worker.work" && evidence.kind == "public_export"
        }));

        let opaque = analyze(
            &[
                module(
                    "pkg/__init__.py",
                    "pkg",
                    "import importlib\ndef build_table():\n    return {'work': 'pkg.worker'}\n_LAZY = build_table()\n__all__ = ['work']\ndef __getattr__(name):\n    if name in _LAZY:\n        return getattr(importlib.import_module(_LAZY[name]), name)\n    raise AttributeError(name)\n",
                ),
                module(
                    "pkg/worker.py",
                    "pkg.worker",
                    "def work():\n    return helper()\ndef helper():\n    return 1\ndef unused():\n    return 2\n",
                ),
            ],
            &[],
        );
        assert!(
            opaque
                .issues
                .iter()
                .any(|issue| issue.kind == "unresolved_lazy_export"),
            "{:?}",
            opaque.issues
        );
    }

    #[test]
    fn arbitrary_subscript_and_membership_dispatch_remain_incomplete() {
        let result = analyze(
            &[module(
                "main.py",
                "main",
                "def inspect(values, key):\n    return values[key], key in values\ninspect(object(), object())\n",
            )],
            &[],
        );
        for kind in ["unresolved_subscript", "unresolved_membership"] {
            assert!(
                result.issues.iter().any(|issue| issue.kind == kind),
                "missing {kind}: {:?}",
                result.issues
            );
        }
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
    fn conditional_shadowing_retains_previous_and_new_targets() {
        let source = "def old():\n    pass\ndef new():\n    pass\nhandler = old\nif flag:\n    handler = new\nhandler()\n";
        let result = analyze(&[module("main.py", "main", source)], &[]);
        assert!(result.issues.is_empty(), "{:?}", result.issues);
        assert!(names(&result, Category::Function).is_empty());
        for target in ["main.old", "main.new"] {
            assert!(result.evidence.iter().any(|evidence| {
                evidence.full_name == target
                    && evidence.kind == "direct_call"
                    && evidence.certainty == "possible"
            }));
        }
    }

    #[test]
    fn a_later_parent_rebinding_cannot_prove_the_earlier_target_unused() {
        let project = tempfile::tempdir().unwrap();
        let config = project.path().join(".skylos.toml");
        let source = project.path().join("main.py");
        std::fs::write(&config, "root = true\n").unwrap();
        let options = super::super::Options {
            config_file: Some(config),
            ..super::super::Options::default()
        };
        for (program, call_line) in [
            (
                "def helper():\n    return 1\ndef caller():\n    helper()\ncaller()\ndef helper():\n    return 2\n",
                4,
            ),
            (
                "def outer():\n    def early():\n        return 1\n    def late():\n        return 2\n    target = early\n    def caller():\n        target()\n    caller()\n    target = late\nouter()\n",
                8,
            ),
        ] {
            std::fs::write(&source, program).unwrap();
            let report = super::super::scan(project.path(), &options).unwrap();
            let output: serde_json::Value =
                serde_json::from_str(&report.to_json().unwrap()).unwrap();
            assert!(!report.is_complete(), "{output}");
            assert_eq!(output["unused_functions"], serde_json::json!([]));
            assert!(output["coverage"]["issues"]
                .as_array()
                .unwrap()
                .iter()
                .any(|issue| issue["kind"] == "ambiguous_binding" && issue["line"] == call_line));
        }
    }

    #[test]
    fn subclass_hooks_follow_executed_class_definitions() {
        let base = "def helper():\n    pass\nclass Base:\n    def __init_subclass__(cls):\n        helper()\n";
        for (child, active) in [
            ("class Child(Base):\n    pass\n", true),
            (
                "ENABLED = False\nif ENABLED:\n    class Outer:\n        class Child(Base):\n            pass\n",
                false,
            ),
        ] {
            let result = analyze(&[module("main.py", "main", &format!("{base}{child}"))], &[]);
            assert!(result.issues.is_empty(), "{child}\n{:?}", result.issues);
            assert_eq!(
                result.evidence.iter().any(|edge| {
                    edge.full_name == "main.Base.__init_subclass__"
                        && edge.kind == "subclass_hook"
                        && edge.certainty == "certain"
                }),
                active,
                "{child}"
            );
            let unused = names(&result, Category::Function);
            assert_eq!(unused.contains(&"helper"), !active, "{child}\n{unused:?}");
        }
    }

    #[test]
    fn unresolved_subclass_hooks_make_class_creation_incomplete() {
        for source in [
            "from foreign import Base\nclass Child(Base):\n    pass\n",
            "class First:\n    def __init_subclass__(cls):\n        pass\nclass Second:\n    pass\nclass Child(First, Second):\n    pass\n",
        ] {
            let result = analyze(&[module("main.py", "main", source)], &[]);
            assert!(
                result
                    .issues
                    .iter()
                    .any(|issue| issue.kind == "unresolved_subclass_hook"),
                "{source}\n{:?}",
                result.issues
            );
        }
    }

    #[test]
    fn instantiating_a_subclass_executes_the_resolved_inherited_constructor() {
        let files = [module(
            "main.py",
            "main",
            "def helper():\n    return 1\ndef runner():\n    return 2\nclass Base:\n    def __init__(self):\n        helper()\n    def run(self):\n        runner()\nclass Middle(Base):\n    pass\nclass Child(Middle):\n    pass\nChild().run()\n",
        )];
        let result = analyze(&files, &[]);
        assert!(result.issues.is_empty(), "{:?}", result.issues);
        assert!(names(&result, Category::Function).is_empty());
        assert!(
            result
                .evidence
                .iter()
                .any(|evidence| evidence.full_name == "main.Base.__init__"
                    && evidence.kind == "constructor")
        );
        assert!(result.evidence.iter().any(
            |evidence| evidence.full_name == "main.Base.run" && evidence.kind == "direct_call"
        ));
    }

    #[test]
    fn custom_allocator_can_return_a_different_selected_class() {
        let files = [module(
            "main.py",
            "main",
            "class Requested:\n    def __new__(cls):\n        return Actual()\n    def deliver(self):\n        return 0\nclass Actual:\n    def deliver(self):\n        return 1\nRequested().deliver()\n",
        )];
        let result = analyze(&files, &[]);
        assert!(result.issues.is_empty(), "{:?}", result.issues);
        assert_eq!(names(&result, Category::Function), ["deliver"]);
        assert!(result.evidence.iter().any(|evidence| {
            evidence.full_name == "main.Actual.deliver" && evidence.kind == "direct_call"
        }));

        let opaque = analyze(
            &[module(
                "main.py",
                "main",
                "from external import allocate\nclass Requested:\n    def __new__(cls):\n        return allocate()\nRequested()\n",
            )],
            &[],
        );
        assert!(
            opaque
                .issues
                .iter()
                .any(|issue| issue.kind == "unresolved_constructor"),
            "{:?}",
            opaque.issues
        );
    }

    #[test]
    fn inherited_method_dispatches_to_the_selected_child_override() {
        let files = [module(
            "main.py",
            "main",
            "def finish_child_work():\n    return 1\nclass Parent:\n    def run(self):\n        return self.step()\n    def step(self):\n        return 0\nclass Child(Parent):\n    def step(self):\n        return finish_child_work()\nChild().run()\n",
        )];
        let result = analyze(&files, &[]);
        assert!(result.issues.is_empty(), "{:?}", result.issues);
        assert_eq!(names(&result, Category::Function), ["step"]);
        for target in [
            "main.Parent.run",
            "main.Child.step",
            "main.finish_child_work",
        ] {
            assert!(
                result
                    .evidence
                    .iter()
                    .any(|evidence| evidence.full_name == target),
                "missing evidence for {target}"
            );
        }
    }

    #[test]
    fn finite_receiver_branch_join_retains_each_selected_method() {
        let classes = "class First:\n    def run(self):\n        return 1\nclass Second:\n    def run(self):\n        return 2\n";
        for usage in [
            "if choose:\n    receiver = First()\nelse:\n    receiver = Second()\nreceiver.run()\n",
            "receiver = First() if choose else Second()\nreceiver.run()\n",
        ] {
            let files = [module("main.py", "main", &format!("{classes}{usage}"))];
            let result = analyze(&files, &[]);
            assert!(result.issues.is_empty(), "{usage}\n{:?}", result.issues);
            assert!(names(&result, Category::Function).is_empty());
            for target in ["main.First.run", "main.Second.run"] {
                assert!(result.evidence.iter().any(|evidence| {
                    evidence.full_name == target
                        && evidence.kind == "direct_call"
                        && evidence.certainty == "possible"
                }));
            }
        }

        let known = analyze(
            &[module(
                "main.py",
                "main",
                &format!("{classes}receiver = First() if True else Second()\nreceiver.run()\n"),
            )],
            &[],
        );
        assert!(known.issues.is_empty(), "{:?}", known.issues);
        assert_eq!(names(&known, Category::Function), ["run"]);
        assert!(known.evidence.iter().any(|evidence| {
            evidence.full_name == "main.First.run" && evidence.kind == "direct_call"
        }));
    }

    #[test]
    fn nested_finite_receiver_paths_retain_every_selected_method() {
        let classes = "class First:\n    def run(self):\n        return 1\nclass Second:\n    def run(self):\n        return 2\nclass Third:\n    def run(self):\n        return 3\ndef unused():\n    return 4\n";
        for usage in [
            "receiver = Third()\nif outer:\n    if inner:\n        receiver = First()\nelse:\n    receiver = Second()\nreceiver.run()\n",
            "receiver = (First() if inner else Second()) if outer else Third()\nreceiver.run()\n",
        ] {
            let result = analyze(
                &[module("main.py", "main", &format!("{classes}{usage}"))],
                &[],
            );
            assert!(result.issues.is_empty(), "{usage}\n{:?}", result.issues);
            assert_eq!(names(&result, Category::Function), ["unused"]);
            for target in ["main.First.run", "main.Second.run", "main.Third.run"] {
                assert!(result.evidence.iter().any(|evidence| {
                    evidence.full_name == target
                        && evidence.kind == "direct_call"
                        && evidence.certainty == "possible"
                }));
            }
        }
    }

    #[test]
    fn conditional_state_limit_remains_incomplete() {
        let mut source =
            "class Target:\n    def run(self):\n        return 1\nreceiver = Target()\n".to_owned();
        for index in 0..13 {
            source.push_str(&format!("if choice_{index}:\n    receiver = Target()\n"));
        }
        source.push_str("receiver.run()\n");

        let result = analyze(&[module("main.py", "main", &source)], &[]);
        assert!(
            result
                .issues
                .iter()
                .any(|issue| issue.kind == "ambiguous_binding"),
            "{:?}",
            result.issues
        );
    }

    #[test]
    fn finite_conditional_export_retains_each_possible_target() {
        let result = analyze(
            &[module(
                "pkg/__init__.py",
                "pkg",
                "__all__ = ['exported']\ndef first():\n    return 1\ndef second():\n    return 2\ndef unused():\n    return 3\nexported = first if choose else second\n",
            )],
            &[],
        );
        assert!(result.issues.is_empty(), "{:?}", result.issues);
        assert_eq!(names(&result, Category::Function), ["unused"]);
        for target in ["pkg.first", "pkg.second"] {
            assert!(result.evidence.iter().any(|evidence| {
                evidence.full_name == target
                    && evidence.kind == "public_export"
                    && evidence.certainty == "possible"
            }));
        }
    }

    #[test]
    fn instance_dispatch_uses_the_first_positional_parameter() {
        let source = "\
def finish_setup():
    return 1

class Service:
    def __init__(instance):
        instance.prepare()
    def prepare(instance):
        return instance.finish()
    def finish(instance, /):
        return finish_setup()
    def unused(instance):
        return 0
";
        for caller in [
            "Service()\n",
            "Factory = Service\ncreated = Factory()\nService.prepare(instance=created)\n",
        ] {
            let result = analyze(
                &[module("main.py", "main", &format!("{source}{caller}"))],
                &[],
            );
            assert!(result.issues.is_empty(), "{caller}\n{:?}", result.issues);
            assert_eq!(names(&result, Category::Function), ["unused"]);
            for target in [
                "main.Service.prepare",
                "main.Service.finish",
                "main.finish_setup",
            ] {
                assert!(
                    result
                        .evidence
                        .iter()
                        .any(|edge| edge.full_name == target && edge.kind == "direct_call"),
                    "missing evidence for {target} after {caller}"
                );
            }
        }
    }

    #[test]
    fn bare_class_body_call_checks_its_supplied_receiver() {
        let files = [module(
            "main.py",
            "main",
            "\
def configured():
    return False

class Configuration:
    def __bool__(self):
        return configured()

class Status:
    def label(config):
        if config:
            return 'on'
        return 'off'
    default = label(Configuration())
print(Status.default)
",
        )];
        let result = analyze(&files, &[]);
        assert!(
            result
                .issues
                .iter()
                .any(|issue| { issue.kind == "unresolved_call" && issue.line == Some(13) }),
            "{:?}",
            result.issues
        );
    }

    #[test]
    fn active_unknown_mro_and_descriptor_dispatch_are_incomplete() {
        for (source, expected_issue) in [
            (
                "from external import Foreign\nclass Child(Foreign):\n    pass\nChild()\n",
                "unresolved_constructor",
            ),
            (
                "def getter():\n    return 1\nclass Base:\n    @property\n    def value(self):\n        return getter()\nclass Child(Base):\n    pass\nprint(Child().value)\n",
                "unresolved_decorator",
            ),
        ] {
            let result = analyze(&[module("main.py", "main", source)], &[]);
            assert!(
                result
                    .issues
                    .iter()
                    .any(|issue| issue.kind == expected_issue),
                "{source}\n{:?}",
                result.issues
            );
        }
    }

    #[test]
    fn pytest_assertion_retains_its_fixture() {
        let files = [module(
            "test_app.py",
            "test_app",
            "from pytest import fixture\n@fixture\ndef resource():\n    return 1\n\ndef test_app(resource):\n    assert resource\n",
        )];
        let result = analyze(&files, &[]);
        assert!(result.issues.is_empty(), "{:?}", result.issues);
        assert!(names(&result, Category::Function).is_empty());
        assert!(names(&result, Category::Parameter).is_empty());
        assert!(
            result
                .evidence
                .iter()
                .any(|evidence| evidence.full_name == "test_app.resource"
                    && evidence.kind == "pytest_root")
        );
    }
}
