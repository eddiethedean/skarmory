mod expression;
mod imports;

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::rc::Rc;
use std::time::Instant;

use skarmory_python::Location;
use skarmory_python::preview::{
    Constant, Function, Module, ParameterKind, Statement, StatementKind,
};

use super::{Evidence, PreviewFinding, PreviewReport, ScanOptions, ScanPolicy};
use crate::preview_source::Project;
use crate::{Issue, Timings};

const MAX_STEPS: usize = 250_000;
const MAX_DEPTH: usize = 40;
const MAX_PATHS: usize = 128;

type FrameId = usize;
type Names = BTreeMap<String, Value>;
type Outcomes<T> = Vec<(State, T)>;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct Definition {
    file: usize,
    line: usize,
    column: usize,
}

impl Definition {
    fn new(file: usize, location: Location) -> Self {
        Self {
            file,
            line: location.line,
            column: location.column,
        }
    }
}

struct FunctionInfo<'a> {
    node: &'a Function,
    scope: String,
    id: String,
}

#[derive(Clone, Debug)]
enum Value {
    Constant(Constant),
    Sequence(Rc<[Value]>, bool),
    Mapping(Rc<Names>),
    Module(String),
    Function(Rc<Closure>),
    UnknownBool,
    Unknown,
}

impl Value {
    fn none() -> Self {
        Self::Constant(Constant::None)
    }
}

#[derive(Debug)]
struct Closure {
    definition: Definition,
    enclosing: FrameId,
    defaults: Vec<Option<Value>>,
}

#[derive(Clone)]
struct Frame {
    parent: Option<FrameId>,
    module: FrameId,
    file: usize,
    locals: Names,
    definition: Option<Definition>,
    package: Option<String>,
}

#[derive(Clone, Copy)]
struct Loaded {
    frame: FrameId,
    loading: bool,
}

#[derive(Clone, Default)]
struct State {
    frames: Rc<Vec<Rc<Frame>>>,
    modules: Rc<BTreeMap<String, Loaded>>,
    possible: bool,
    revision: usize,
}

impl State {
    fn push(&mut self, frame: Frame) -> FrameId {
        let id = self.frames.len();
        Rc::make_mut(&mut self.frames).push(Rc::new(frame));
        id
    }

    fn frame(&self, id: FrameId) -> &Frame {
        &self.frames[id]
    }

    fn bind(&mut self, frame: FrameId, name: String, value: Value) {
        Rc::make_mut(&mut Rc::make_mut(&mut self.frames)[frame])
            .locals
            .insert(name, value);
    }

    fn lookup(&self, mut frame: FrameId, name: &str, engine: &Engine<'_>) -> Option<Value> {
        loop {
            let scope = self.frame(frame);
            if let Some(value) = scope.locals.get(name) {
                return Some(value.clone());
            }
            if let Some(definition) = scope.definition
                && engine.definitions[&definition].node.locals.contains(name)
            {
                return None;
            }
            frame = scope.parent?;
        }
    }
}

#[derive(Clone, Copy)]
struct Site {
    file: usize,
    location: Location,
}

impl Site {
    fn at(file: usize, location: Location) -> Self {
        Self { file, location }
    }

    fn entry(file: usize) -> Self {
        Self::at(file, Location { line: 1, column: 1 })
    }
}

enum Control {
    Next,
    Return(Value),
}

pub(super) struct Engine<'a> {
    project: &'a Project,
    modules: &'a [Module],
    definitions: BTreeMap<Definition, FunctionInfo<'a>>,
    evidence: BTreeSet<Evidence>,
    reached: BTreeSet<Definition>,
    uncertainties: BTreeSet<Issue>,
    issues: BTreeSet<Issue>,
    steps: usize,
    depth: usize,
    limited: bool,
    calls: Vec<(Rc<Closure>, Names, usize)>,
    escaping: Vec<Rc<Closure>>,
}

impl<'a> Engine<'a> {
    pub(super) fn new(project: &'a Project, modules: &'a [Module]) -> Self {
        let mut definitions = BTreeMap::new();
        for (file, module) in modules.iter().enumerate() {
            catalog(
                &mut definitions,
                file,
                &project.files[file].label,
                "",
                "",
                &module.body,
            );
        }
        Self {
            project,
            modules,
            definitions,
            evidence: BTreeSet::new(),
            reached: BTreeSet::new(),
            uncertainties: BTreeSet::new(),
            issues: BTreeSet::new(),
            steps: 0,
            depth: 0,
            limited: false,
            calls: Vec::new(),
            escaping: Vec::new(),
        }
    }

    pub(super) fn run(&mut self, options: &ScanOptions) {
        let site = Site::entry(0);
        if self.modules.is_empty() {
            self.issues.insert(Issue::new(
                "missing_sources",
                ".",
                "no Python sources were selected",
            ));
            return;
        }
        if options.policy == ScanPolicy::Library {
            self.library(options);
            return;
        }
        if let Some(file) = self.project.script {
            self.initialize(State::default(), "__main__", file, None, Site::entry(file));
        }
        for name in &options.entry_modules {
            let Some(module) = self.resolve(name, site) else {
                continue;
            };
            if module.package {
                self.issue(
                    site,
                    "invalid_entry",
                    format!("name the package entry {name}.__main__ explicitly"),
                );
                continue;
            }
            let parent = name.rsplit_once('.').map(|(parent, _)| parent);
            for state in self.load_parent(State::default(), parent, site) {
                if !self.static_package_path(&state, parent, site) {
                    continue;
                }
                self.initialize(
                    state,
                    "__main__",
                    module.file,
                    Some(parent.unwrap_or("")),
                    Site::entry(module.file),
                );
            }
        }
        for target in &options.entry_callables {
            let Some((module, name)) = self.callable_name(target) else {
                continue;
            };
            for (state, frame) in self.load(State::default(), module, site) {
                let loc = Site::entry(state.frame(frame).file);
                if let Some(function) = self.callable_root(&state, frame, name, loc) {
                    self.invoke_external(state, function, loc, "callable_entry");
                }
            }
        }
    }

    fn callable_name<'b>(&mut self, target: &'b str) -> Option<(&'b str, &'b str)> {
        if let Some((module, name)) = target.split_once(':')
            && !module.is_empty()
            && !name.is_empty()
            && !name.contains([':', '.'])
        {
            return Some((module, name));
        }
        self.issues.insert(Issue::new(
            "invalid_entry",
            ".",
            format!("expected MODULE:FUNCTION, got {target:?}"),
        ));
        None
    }

    fn callable_root(
        &mut self,
        state: &State,
        frame: FrameId,
        name: &str,
        site: Site,
    ) -> Option<Rc<Closure>> {
        if let Some(Value::Function(function)) = state.frame(frame).locals.get(name) {
            return Some(function.clone());
        }
        self.issue(
            site,
            "unresolved_callable",
            format!("no selected function is bound to {name:?}"),
        );
        None
    }

    fn library(&mut self, options: &ScanOptions) {
        let mut modules: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for name in &options.public_modules {
            modules.entry(name).or_default();
        }
        for target in &options.public_callables {
            if let Some((module, name)) = self.callable_name(target) {
                modules.entry(module).or_default().push(name);
            }
        }
        let mut queue = VecDeque::from([State {
            possible: modules.len() > 1,
            ..State::default()
        }]);
        while let Some(state) = queue.pop_front() {
            if self.limited || !self.step(Site::entry(0)) {
                break;
            }
            for (&module, names) in &modules {
                let loaded = state.modules.get(module).copied();
                let Some(Loaded {
                    frame,
                    loading: false,
                }) = loaded
                else {
                    for (next, _) in self.load(state.clone(), module, Site::entry(0)) {
                        if queue.len() == MAX_PATHS {
                            self.limit(Site::entry(0), "too many library initialization paths");
                            break;
                        }
                        queue.push_back(next);
                    }
                    continue;
                };
                let site = Site::entry(state.frame(frame).file);
                let revision = state.revision;
                for (function, kind) in self.public_functions(&state, frame, names, site) {
                    for mut next in self.invoke_external(state.clone(), function, site, kind) {
                        if next.revision != revision {
                            if queue.len() == MAX_PATHS {
                                self.limit(site, "too many library call paths");
                                break;
                            }
                            next.possible = true;
                            queue.push_back(next);
                        }
                    }
                }
            }
        }
    }

    fn public_functions(
        &mut self,
        state: &State,
        frame: FrameId,
        declared: &[&str],
        site: Site,
    ) -> Vec<(Rc<Closure>, &'static str)> {
        let mut functions = Vec::new();
        let namespace = &state.frame(frame).locals;
        match namespace.get("__all__") {
            Some(Value::Sequence(names, false)) => {
                for name in names.iter() {
                    let Value::Constant(Constant::String(name)) = name else {
                        self.issue(
                            site,
                            "invalid_exports",
                            "__all__ must contain literal names",
                        );
                        continue;
                    };
                    match namespace.get(name) {
                        Some(Value::Function(function)) => {
                            functions.push((function.clone(), "public_export"))
                        }
                        Some(Value::Unknown) | None => {
                            self.issue(
                                site,
                                "unresolved_export",
                                format!("cannot resolve __all__ name {name:?}"),
                            );
                        }
                        Some(_) => {}
                    }
                }
            }
            None if !declared.is_empty() => {}
            _ => self.issue(
                site,
                "unresolved_exports",
                "public module needs a literal __all__ list or explicit callables",
            ),
        }
        for name in declared {
            if let Some(function) = self.callable_root(state, frame, name, site) {
                functions.push((function, "public_callable"));
            }
        }
        functions
    }

    fn invoke_external(
        &mut self,
        state: State,
        function: Rc<Closure>,
        site: Site,
        kind: &'static str,
    ) -> Vec<State> {
        let node = self.definitions[&function.definition].node;
        let mut alternatives = vec![Names::new()];
        for (parameter, default) in node.parameters.iter().zip(&function.defaults) {
            if let Some(default) = default {
                if !self.check_paths(alternatives.len() * 2, site) {
                    return Vec::new();
                }
                let omitted: Vec<_> = alternatives
                    .iter()
                    .cloned()
                    .map(|mut args| {
                        args.insert(parameter.name.clone(), default.clone());
                        args
                    })
                    .collect();
                for args in &mut alternatives {
                    args.insert(parameter.name.clone(), Value::Unknown);
                }
                alternatives.extend(omitted);
            } else {
                for args in &mut alternatives {
                    args.insert(parameter.name.clone(), Value::Unknown);
                }
            }
        }
        let mut outcomes = Vec::new();
        for args in alternatives {
            for (next, result) in self.invoke(state.clone(), function.clone(), args, site, kind) {
                outcomes.extend(self.escape(next, result, site));
                if !self.check_paths(outcomes.len(), site) {
                    return Vec::new();
                }
            }
        }
        outcomes
    }

    fn escape(&mut self, mut state: State, value: Value, site: Site) -> Vec<State> {
        if !self.step(site) {
            return Vec::new();
        }
        match value {
            Value::Function(function) => {
                self.uncertainty(
                    site,
                    "possible_use",
                    "a function returned to an external caller may be invoked",
                );
                if self
                    .escaping
                    .iter()
                    .any(|active| Rc::ptr_eq(active, &function))
                {
                    return vec![state];
                }
                if self.escaping.len() == MAX_DEPTH {
                    self.limit(site, "maximum escaping function depth exceeded");
                    return Vec::new();
                }
                let original = state.clone();
                state.possible = true;
                self.escaping.push(function.clone());
                let mut results = self.invoke_external(state, function, site, "escape");
                self.escaping.pop();
                results.push(original);
                results
            }
            Value::Sequence(items, _) => self.escape_items(state, items.iter(), site),
            Value::Mapping(items) => self.escape_items(state, items.values(), site),
            Value::Module(_) | Value::Unknown => {
                self.unbounded(
                    site,
                    "cannot bound callable values returned to an external caller",
                );
                Vec::new()
            }
            Value::Constant(_) | Value::UnknownBool => vec![state],
        }
    }

    fn escape_items<'b>(
        &mut self,
        state: State,
        items: impl Iterator<Item = &'b Value>,
        site: Site,
    ) -> Vec<State> {
        let mut states = vec![state];
        for value in items {
            states = states
                .into_iter()
                .flat_map(|state| self.escape(state, value.clone(), site))
                .collect();
            if !self.check_paths(states.len(), site) {
                return Vec::new();
            }
        }
        states
    }

    fn invoke(
        &mut self,
        mut state: State,
        function: Rc<Closure>,
        args: Names,
        site: Site,
        kind: &'static str,
    ) -> Outcomes<Value> {
        if !self.step(site) {
            return Vec::new();
        }
        self.retain(&state, function.definition, site, kind);
        if self.calls.iter().any(|(active, bindings, revision)| {
            Rc::ptr_eq(active, &function)
                && *revision == state.revision
                && bindings.len() == args.len()
                && bindings
                    .iter()
                    .all(|(name, value)| args.get(name).is_some_and(|arg| value.same(arg)))
        }) {
            // An active call already explores the same abstract inputs.
            // Keep the recursive edge and conservatively allow a return.
            return vec![(state, Value::Unknown)];
        }
        if !self.enter(site) {
            return Vec::new();
        }
        let node = self.definitions[&function.definition].node;
        let frame = state.push(Frame {
            parent: Some(function.enclosing),
            module: state.frame(function.enclosing).module,
            file: function.definition.file,
            locals: args.clone(),
            definition: Some(function.definition),
            package: None,
        });
        self.calls.push((function, args, state.revision));
        let outcomes = self.block(state, frame, &node.body);
        self.calls.pop();
        self.depth -= 1;
        outcomes
            .into_iter()
            .map(|(state, control)| {
                let value = match control {
                    Control::Next => Value::none(),
                    Control::Return(value) => value,
                };
                (state, value)
            })
            .collect()
    }

    fn retain(&mut self, state: &State, definition: Definition, site: Site, kind: &'static str) {
        self.reached.insert(definition);
        self.evidence.insert(Evidence {
            symbol_id: self.definitions[&definition].id.clone(),
            certainty: if state.possible {
                "possible"
            } else {
                "established"
            },
            kind,
            path: self.project.files[site.file].label.clone(),
            line: site.location.line,
            column: site.location.column,
        });
    }

    fn block(
        &mut self,
        state: State,
        frame: FrameId,
        statements: &'a [Statement],
    ) -> Outcomes<Control> {
        let mut paths = vec![(state, Control::Next)];
        for statement in statements {
            let mut next = Vec::new();
            for (state, control) in paths {
                let site = Site::at(state.frame(frame).file, statement.location);
                match control {
                    Control::Next => next.extend(self.statement(state, frame, statement, site)),
                    Control::Return(_) => next.push((state, control)),
                }
                if !self.check_paths(next.len(), site) {
                    return Vec::new();
                }
            }
            paths = next;
        }
        paths
    }

    fn statement(
        &mut self,
        state: State,
        frame: FrameId,
        statement: &'a Statement,
        site: Site,
    ) -> Outcomes<Control> {
        if !self.step(site) {
            return Vec::new();
        }
        match &statement.kind {
            StatementKind::Function(node) => {
                let definition = Definition::new(site.file, node.location);
                let mut paths = vec![(state, Vec::new())];
                for parameter in &node.parameters {
                    let mut next = Vec::new();
                    for (state, mut defaults) in paths {
                        if let Some(value) = &parameter.default {
                            for (state, value) in self.eval(state, frame, value) {
                                let mut defaults = defaults.clone();
                                defaults.push(Some(value));
                                next.push((state, defaults));
                            }
                        } else {
                            defaults.push(None);
                            next.push((state, defaults));
                        }
                    }
                    if !self.check_paths(next.len(), site) {
                        return Vec::new();
                    }
                    paths = next;
                }
                paths
                    .into_iter()
                    .map(|(mut state, defaults)| {
                        state.bind(
                            frame,
                            node.name.clone(),
                            Value::Function(Rc::new(Closure {
                                definition,
                                enclosing: frame,
                                defaults,
                            })),
                        );
                        (state, Control::Next)
                    })
                    .collect()
            }
            StatementKind::Import(import) => self
                .import(state, frame, import, site)
                .into_iter()
                .map(|state| (state, Control::Next))
                .collect(),
            StatementKind::Assign { names, value } => self
                .eval(state, frame, value)
                .into_iter()
                .map(|(mut state, value)| {
                    for name in names {
                        state.bind(frame, name.clone(), value.clone());
                    }
                    (state, Control::Next)
                })
                .collect(),
            StatementKind::Return(Some(value)) => self
                .eval(state, frame, value)
                .into_iter()
                .map(|(state, value)| (state, Control::Return(value)))
                .collect(),
            StatementKind::Return(None) => vec![(state, Control::Return(Value::none()))],
            StatementKind::Expr(value) => self
                .eval(state, frame, value)
                .into_iter()
                .map(|(state, _)| (state, Control::Next))
                .collect(),
            StatementKind::If {
                test,
                body,
                otherwise,
            } => {
                let mut paths = Vec::new();
                for (mut state, test) in self.eval(state, frame, test) {
                    match test.truth() {
                        Some(true) => paths.extend(self.block(state, frame, body)),
                        Some(false) => paths.extend(self.block(state, frame, otherwise)),
                        None => {
                            state.possible = true;
                            paths.extend(self.block(state.clone(), frame, body));
                            paths.extend(self.block(state, frame, otherwise));
                        }
                    }
                    if !self.check_paths(paths.len(), site) {
                        return Vec::new();
                    }
                }
                paths
            }
        }
    }

    fn step(&mut self, site: Site) -> bool {
        if self.limited {
            return false;
        }
        if self.steps == MAX_STEPS {
            self.limit(site, "analysis operation budget exhausted");
            return false;
        }
        self.steps += 1;
        true
    }

    fn enter(&mut self, site: Site) -> bool {
        if self.depth == MAX_DEPTH {
            self.limit(site, "maximum analysis call and import depth exceeded");
            return false;
        }
        self.depth += 1;
        true
    }

    fn check_paths(&mut self, count: usize, site: Site) -> bool {
        if count > MAX_PATHS {
            self.limit(site, "maximum simultaneous analysis paths exceeded");
            return false;
        }
        !self.limited
    }

    fn limit(&mut self, site: Site, message: &str) {
        if !self.limited {
            self.issue(site, "analysis_limit", message);
            self.limited = true;
        }
    }

    fn issue(&mut self, site: Site, kind: &'static str, message: impl ToString) {
        self.issues.insert(
            Issue::new(kind, &self.project.files[site.file].label, message).at(site.location),
        );
    }

    fn uncertainty(&mut self, site: Site, kind: &'static str, message: impl ToString) {
        self.uncertainties.insert(
            Issue::new(kind, &self.project.files[site.file].label, message).at(site.location),
        );
    }

    fn unbounded(&mut self, site: Site, message: &str) {
        self.uncertainty(site, "unbounded_use", message);
        self.issue(site, "unbounded_use", message);
    }

    pub(super) fn finish(self, report: &mut PreviewReport, timings: &mut Timings) {
        let start = Instant::now();
        report.coverage.issues.extend(self.issues);
        report.uncertainties.extend(self.uncertainties);
        report.evidence.extend(self.evidence);
        let dead = self
            .definitions
            .into_iter()
            .filter(|(definition, _)| !self.reached.contains(definition));
        let dead: Vec<_> = dead.collect();
        timings.reachability_ns = Some(start.elapsed().as_nanos());
        let start = Instant::now();
        for (definition, info) in dead {
            let file = &self.project.files[definition.file];
            report.findings.push(PreviewFinding {
                rule: "unreachable_function",
                symbol_id: info.id,
                module: file.name.clone(),
                symbol: info.node.name.clone(),
                scope: info.scope,
                path: file.label.clone(),
                line: definition.line,
                column: definition.column,
                evidence: "no_call_path_from_entry",
            });
        }
        timings.rule_ns = Some(start.elapsed().as_nanos());
    }
}

fn catalog<'a>(
    definitions: &mut BTreeMap<Definition, FunctionInfo<'a>>,
    file: usize,
    path: &str,
    scope: &str,
    owners: &str,
    body: &'a [Statement],
) {
    for statement in body {
        match &statement.kind {
            StatementKind::Function(function) => {
                let definition = Definition::new(file, function.location);
                let qualified = if scope.is_empty() {
                    function.name.clone()
                } else {
                    format!("{scope}.{}", function.name)
                };
                let chain = format!(
                    "{owners}{}@{}:{}/",
                    function.name, definition.line, definition.column
                );
                definitions.insert(
                    definition,
                    FunctionInfo {
                        node: function,
                        scope: scope.into(),
                        id: format!("{path}::{chain}"),
                    },
                );
                catalog(definitions, file, path, &qualified, &chain, &function.body);
            }
            StatementKind::If {
                body, otherwise, ..
            } => {
                catalog(definitions, file, path, scope, owners, body);
                catalog(definitions, file, path, scope, owners, otherwise);
            }
            _ => {}
        }
    }
}
