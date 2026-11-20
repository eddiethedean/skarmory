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
const MAX_EVALUATION_DEPTH: usize = 128;
const MAX_VALUE_DEPTH: usize = 64;
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
    Sequence(Rc<[Value]>, bool, usize),
    Mapping(Rc<Names>, usize),
    Module(String),
    Function(Rc<Closure>),
    UnknownBool,
    Unknown,
}

impl Value {
    fn none() -> Self {
        Self::Constant(Constant::None)
    }

    fn sequence(values: Vec<Value>, tuple: bool) -> Self {
        let depth = Self::nested_depth(values.iter());
        Self::Sequence(values.into(), tuple, depth)
    }

    fn mapping(values: Names) -> Self {
        let depth = Self::nested_depth(values.values());
        Self::Mapping(Rc::new(values), depth)
    }

    fn nested_depth<'a>(values: impl Iterator<Item = &'a Self>) -> usize {
        1 + values.map(Self::depth).max().unwrap_or(0)
    }

    fn depth(&self) -> usize {
        match self {
            Self::Sequence(_, _, depth) | Self::Mapping(_, depth) => *depth,
            Self::Function(closure) => closure.depth,
            _ => 0,
        }
    }
}

#[derive(Debug)]
struct Closure {
    definition: Definition,
    enclosing: FrameId,
    defaults: Vec<Option<Value>>,
    depth: usize,
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
    library_calls: Option<Rc<LibraryCalls>>,
}

#[derive(Clone, Default)]
struct LibraryCalls {
    escaped: Vec<(Rc<Closure>, Site)>,
    explored: Vec<(Rc<Closure>, &'static str, usize)>,
}

impl State {
    fn library_calls_mut(&mut self) -> &mut LibraryCalls {
        Rc::make_mut(self.library_calls.as_mut().expect("library scan state"))
    }

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
    evaluation_depth: usize,
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
            evaluation_depth: 0,
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
            if name == "__main__" {
                self.issue(
                    site,
                    "invalid_entry",
                    "the running __main__ module cannot be selected as a named entry",
                );
                continue;
            }
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
            library_calls: Some(Rc::default()),
            ..State::default()
        }]);
        'states: while let Some(state) = queue.pop_front() {
            if self.limited || !self.step(Site::entry(0)) {
                break;
            }
            let mut outgoing = Vec::new();
            for (&module, names) in &modules {
                let loaded = state.modules.get(module).copied();
                let Some(Loaded {
                    frame,
                    loading: false,
                }) = loaded
                else {
                    for (mut next, _) in self.load(state.clone(), module, Site::entry(0)) {
                        if outgoing.len() == MAX_PATHS {
                            self.limit(Site::entry(0), "too many library initialization paths");
                            break;
                        }
                        next.library_calls_mut().explored.clear();
                        outgoing.push(next);
                    }
                    continue;
                };
                let site = Site::entry(state.frame(frame).file);
                for (function, kind) in self.public_functions(&state, frame, names, site) {
                    if let Some(enriched) =
                        self.schedule_library_call(&mut outgoing, &state, function, site, kind)
                    {
                        queue.push_front(enriched);
                        continue 'states;
                    }
                }
            }
            for (function, site) in &state.library_calls.as_ref().unwrap().escaped {
                if let Some(enriched) = self.schedule_library_call(
                    &mut outgoing,
                    &state,
                    function.clone(),
                    *site,
                    "escape",
                ) {
                    queue.push_front(enriched);
                    continue 'states;
                }
            }
            if !self.check_paths(queue.len() + outgoing.len(), Site::entry(0)) {
                break;
            }
            queue.extend(outgoing);
        }
    }

    fn schedule_library_call(
        &mut self,
        outgoing: &mut Vec<State>,
        state: &State,
        function: Rc<Closure>,
        site: Site,
        kind: &'static str,
    ) -> Option<State> {
        let calls = state.library_calls.as_ref().expect("library scan state");
        let mut input = state.clone();
        input.possible |= kind == "escape";
        let outcomes = self.invoke_external(input, function.clone(), site, kind);
        for (index, mut next) in outcomes.into_iter().enumerate() {
            let world_changed = next.revision != state.revision;
            let new_escape =
                next.library_calls.as_ref().unwrap().escaped.len() > calls.escaped.len();
            if world_changed {
                if outgoing.len() == MAX_PATHS {
                    self.limit(site, "too many library call paths");
                    return None;
                }
                // Revisit public calls and saved callbacks after imports change
                // the module bindings they observe.
                next.library_calls_mut().explored.clear();
                next.possible = true;
                outgoing.push(next);
            } else if new_escape
                && !calls
                    .explored
                    .iter()
                    .any(|(previous, previous_kind, outcome)| {
                        *previous_kind == kind
                            && *outcome == index
                            && Rc::ptr_eq(previous, &function)
                    })
            {
                // With unchanged module bindings, this outcome only adds
                // optional calls. Keep them before exploring other actions.
                // Remember the outcome so rerunning the factory in this same
                // world cannot create unbounded copies of the same closure.
                next.library_calls_mut()
                    .explored
                    .push((function, kind, index));
                next.possible = state.possible;
                return Some(next);
            }
        }
        None
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
            Some(Value::Sequence(names, false, _)) => {
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

    fn escape(&mut self, state: State, value: Value, site: Site) -> Vec<State> {
        if !self.step(site) {
            return Vec::new();
        }
        self.with_evaluation(site, |engine| engine.escape_value(state, value, site))
            .unwrap_or_default()
    }

    fn escape_value(&mut self, mut state: State, value: Value, site: Site) -> Vec<State> {
        match value {
            Value::Function(function) => {
                self.uncertainty(
                    site,
                    "possible_use",
                    "a function returned to an external caller may be invoked",
                );
                if state.library_calls.is_some() {
                    let calls = state.library_calls_mut();
                    if !calls
                        .escaped
                        .iter()
                        .any(|(saved, _)| Rc::ptr_eq(saved, &function))
                    {
                        if calls.escaped.len() == MAX_PATHS {
                            self.limit(site, "too many saved library callbacks");
                            return Vec::new();
                        }
                        calls.escaped.push((function, site));
                    }
                    return vec![state];
                }
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
            Value::Sequence(items, ..) => self.escape_items(state, items.iter(), site),
            Value::Mapping(items, _) => self.escape_items(state, items.values(), site),
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
        self.leave();
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
        let Some(first) = statements.first() else {
            return vec![(state, Control::Next)];
        };
        let file = state.frame(frame).file;
        self.with_evaluation(Site::at(file, first.location), |engine| {
            let mut paths = vec![(state, Control::Next)];
            for statement in statements {
                let mut next = Vec::new();
                for (state, control) in paths {
                    let site = Site::at(file, statement.location);
                    match control {
                        Control::Next => {
                            next.extend(engine.statement(state, frame, statement, site))
                        }
                        Control::Return(_) => next.push((state, control)),
                    }
                    if !engine.check_paths(next.len(), site) {
                        return Vec::new();
                    }
                }
                paths = next;
            }
            paths
        })
        .unwrap_or_default()
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
                    .filter_map(|(mut state, defaults)| {
                        let depth = Value::nested_depth(defaults.iter().flatten());
                        if !self.check_value_depth(depth, site) {
                            return None;
                        }
                        state.bind(
                            frame,
                            node.name.clone(),
                            Value::Function(Rc::new(Closure {
                                definition,
                                enclosing: frame,
                                defaults,
                                depth,
                            })),
                        );
                        Some((state, Control::Next))
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
        if !self.enter_evaluation(site) {
            return false;
        }
        self.depth += 1;
        true
    }

    fn leave(&mut self) {
        self.depth -= 1;
        self.evaluation_depth -= 1;
    }

    fn enter_evaluation(&mut self, site: Site) -> bool {
        if self.evaluation_depth >= MAX_EVALUATION_DEPTH {
            self.limit(site, "maximum combined evaluation depth exceeded");
            return false;
        }
        if self.limited {
            return false;
        }
        self.evaluation_depth += 1;
        true
    }

    fn with_evaluation<T>(&mut self, site: Site, run: impl FnOnce(&mut Self) -> T) -> Option<T> {
        if !self.enter_evaluation(site) {
            return None;
        }
        let result = run(self);
        self.evaluation_depth -= 1;
        Some(result)
    }

    fn check_paths(&mut self, count: usize, site: Site) -> bool {
        if count > MAX_PATHS {
            self.limit(site, "maximum simultaneous analysis paths exceeded");
            return false;
        }
        !self.limited
    }

    fn check_value_depth(&mut self, depth: usize, site: Site) -> bool {
        if depth > MAX_VALUE_DEPTH {
            self.limit(site, "maximum abstract value nesting exceeded");
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
