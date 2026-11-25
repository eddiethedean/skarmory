//! Reached stdlib logging configuration and bounded dictionary mutations.

use super::*;

mod instances;

use instances::{Instance, Method};

pub(super) const DICT_CONFIG: &str = "logging.config.dictConfig";
const MAX_ITEMS: usize = 256;
const MAX_REPLAY_DEPTH: usize = 16;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum LoggingBase {
    Formatter,
    Filter,
}

impl LoggingBase {
    pub(super) fn provider(name: &str) -> Option<Self> {
        match name {
            "logging.Formatter" => Some(Self::Formatter),
            "logging.Filter" => Some(Self::Filter),
            _ => None,
        }
    }

    pub(super) fn defines(self, name: &str) -> bool {
        name == "__init__"
            || match self {
                Self::Formatter => matches!(
                    name,
                    "format" | "formatTime" | "formatException" | "formatMessage" | "formatStack"
                ),
                Self::Filter => name == "filter",
            }
    }
}

#[derive(Clone)]
enum Data {
    Dictionary(usize),
    Class(SymbolId),
    Instance(usize),
    Text(String),
    Integer(i64),
    Bool(bool),
    None,
}

type Dictionary = Vec<(String, Data)>;
type Site = (ScopeId, [usize; 2]);

#[derive(Default)]
struct Replay<'a> {
    names: BTreeMap<&'a str, Data>,
    dictionaries: Vec<Dictionary>,
    classes: BTreeMap<SymbolId, BTreeMap<&'a str, Method<'a>>>,
    instances: Vec<Instance<'a>>,
    mutations: BTreeSet<Site>,
    helpers: BTreeMap<SymbolId, BTreeSet<[usize; 2]>>,
    call: Option<(Location, Vec<Factory>)>,
    calls: BTreeSet<(ScopeId, [usize; 2], SymbolId)>,
    method_scopes: BTreeSet<ScopeId>,
    depth: usize,
    returned: bool,
    stopped: bool,
    dictionary_is_pathlike: bool,
    steps: usize,
}

enum Dispatch {
    Continues,
    Rejected,
    Unknown,
}

struct Factory {
    path: String,
    // Bare formatter classes receive positional arguments. Custom factories
    // receive the configuration's keyword arguments.
    default_formatter: bool,
    has_options: bool,
}

impl<'a> Replay<'a> {
    fn evaluate(&mut self, expression: &'a Expression, depth: usize) -> Option<Data> {
        if depth >= MAX_REPLAY_DEPTH || self.steps >= MAX_ITEMS * MAX_REPLAY_DEPTH {
            return None;
        }
        self.steps += 1;
        Some(match &expression.kind {
            ExpressionKind::Text(value)
                if !value.starts_with("ext://") && !value.starts_with("cfg://") =>
            {
                Data::Text(value.clone())
            }
            ExpressionKind::Scalar(Some(value)) => Data::Integer(*value),
            ExpressionKind::Bool(value) => Data::Bool(*value),
            ExpressionKind::None => Data::None,
            ExpressionKind::Name(name) => self.names.get(name.as_str())?.clone(),
            ExpressionKind::Member(owner, name) => {
                let Data::Instance(id) = self.evaluate(owner, depth + 1)? else {
                    return None;
                };
                self.instances[id].fields.get(name.as_str())?.clone()
            }
            ExpressionKind::Subscript { value, index } => {
                let Data::Dictionary(id) = self.evaluate(value, depth + 1)? else {
                    return None;
                };
                let Data::Text(key) = self.evaluate(index, depth + 1)? else {
                    return None;
                };
                get(&self.dictionaries[id], &key)?.clone()
            }
            ExpressionKind::Dictionary(items)
                if self.dictionaries.len() < MAX_ITEMS && items.len() <= MAX_ITEMS =>
            {
                let mut mapping = Vec::new();
                let (pairs, remainder) = items.as_chunks::<2>();
                if !remainder.is_empty() {
                    return None;
                }
                for [key, value] in pairs {
                    let Data::Text(key) = self.evaluate(key, depth + 1)? else {
                        return None;
                    };
                    put(&mut mapping, &key, self.evaluate(value, depth + 1)?);
                }
                let id = self.dictionaries.len();
                self.dictionaries.push(mapping);
                Data::Dictionary(id)
            }
            _ => return None,
        })
    }

    fn statement(
        &mut self,
        graph: &mut Graph<'a>,
        scope: ScopeId,
        statement: &'a Statement,
    ) -> Option<()> {
        // Factory construction may raise or call user code. Only an empty
        // return may follow a configuration call in the replayed initializer.
        if self.call.is_some() && !matches!(statement.kind, StatementKind::Return(_)) {
            return None;
        }
        let at = statement.at;
        match &statement.kind {
            StatementKind::Import(import) => {
                // A selected local initializer may mutate the logging API and
                // can import the configuration that is now being replayed.
                if !graph.logging_safe_import(scope, import) {
                    return None;
                }
                self.names.remove(import.local.text.as_str());
            }
            StatementKind::Assign {
                names,
                value: Some(value),
                annotation: None,
            } => {
                let [name] = names.as_slice() else {
                    return None;
                };
                if let Some(value) = self.evaluate(value, 0) {
                    self.names.insert(&name.text, value);
                } else if self.is_constructor(value) {
                    let instance = self.construct(graph, scope, value)?;
                    self.names.insert(&name.text, Data::Instance(instance));
                } else if provider_alias(graph, scope, value, at) {
                    self.names.remove(name.text.as_str());
                } else {
                    return None;
                }
            }
            StatementKind::WriteModuleEntry {
                mapping,
                module: key,
                value,
            } => {
                let value = match value {
                    Some(value) => Some(self.evaluate(value, 0)?),
                    None => None,
                };
                let Data::Dictionary(id) = self.evaluate(mapping, 0)? else {
                    return None;
                };
                let dictionary = &mut self.dictionaries[id];
                if let Some(value) = value {
                    if dictionary.len() >= MAX_ITEMS && get(dictionary, key).is_none() {
                        return None;
                    }
                    put(dictionary, key, value);
                } else {
                    dictionary.remove(dictionary.iter().position(|(name, _)| name == key)?);
                }
                self.mutations.insert((scope, position(at)));
            }
            StatementKind::WriteAttribute {
                receiver,
                member,
                value,
            } if let Some(Data::Instance(id)) = self.evaluate(receiver, 0) => {
                let value = value.as_ref().map(|value| self.evaluate(value, 0));
                let instance = &mut self.instances[id];
                if instance.has_method(member, &self.classes) || member.starts_with("__") {
                    return None;
                }
                if let Some(value) = value {
                    instance.fields.insert(member, value?);
                } else {
                    instance.fields.remove(member.as_str())?;
                }
                self.mutations.insert((scope, position(at)));
            }
            StatementKind::WriteAttribute {
                receiver,
                member,
                value: Some(value),
            } if member == "dictConfig"
                && graph.modeled_framework_write(scope, at)
                && matches!(graph.probe_identity(scope, receiver, at),
                        Value::Provider(name) if name == "logging.config")
                && matches!(
                    value.kind,
                    ExpressionKind::Name(_)
                        | ExpressionKind::ConstantLambda
                        | ExpressionKind::NullaryConstantLambda
                        | ExpressionKind::IdentityLambda
                ) => {}
            StatementKind::Define(definition) if plain_function(definition) => {
                self.names.remove(definition.name.text.as_str());
            }
            StatementKind::Define(definition) if definition.kind == DefinitionKind::Class => {
                self.define_class(graph, scope, definition)?;
            }
            StatementKind::Evaluate(Expression {
                kind: ExpressionKind::Text(_),
                ..
            }) => {}
            StatementKind::Evaluate(expression) => {
                self.evaluate_call(graph, scope, expression, at)?
            }
            StatementKind::If {
                test,
                body,
                otherwise,
            } => {
                let branch = if self.condition(graph, scope, test, at, 0)? {
                    body
                } else {
                    otherwise
                };
                for item in branch {
                    self.statement(graph, scope, item)?;
                    if self.returned || self.stopped {
                        break;
                    }
                }
            }
            StatementKind::Return(value)
                if self.depth > 0
                    && value
                        .as_ref()
                        .is_none_or(|value| matches!(value.kind, ExpressionKind::None)) =>
            {
                self.returned = true;
            }
            _ => return None,
        }
        Some(())
    }

    fn evaluate_call(
        &mut self,
        graph: &mut Graph<'a>,
        scope: ScopeId,
        expression: &'a Expression,
        before: Location,
    ) -> Option<()> {
        if self.is_constructor(expression) {
            self.construct(graph, scope, expression)?;
            return Some(());
        }
        if self.mutate(scope, expression).is_some() {
            return Some(());
        }
        let ExpressionKind::Call {
            function,
            arguments,
            keywords,
        } = &expression.kind
        else {
            return None;
        };
        if let ExpressionKind::Member(receiver, name) = &function.kind
            && let Some(Data::Instance(id)) = self.evaluate(receiver, 0)
        {
            return self.invoke_method(graph, scope, id, name, expression);
        }
        let provider = graph.probe_identity(scope, function, before);
        if let Value::Callable(invocation) = provider {
            return self.invoke_mutator(graph, invocation, expression.at, arguments, keywords);
        }
        if matches!(&provider, Value::Provider(name) if name == "os.PathLike.register")
            && let ([ty], []) = (arguments.as_slice(), keywords.as_slice())
            && matches!(graph.probe_identity(scope, ty, before), Value::Provider(name) if name == "builtins.dict")
        {
            self.dictionary_is_pathlike = true;
            self.mutations.insert((scope, position(expression.at)));
            return Some(());
        }
        if !matches!(provider, Value::Provider(provider) if provider == DICT_CONFIG)
            || graph
                .module_member_write("logging.config", "dictConfigClass", (scope, before))
                .is_some()
        {
            return None;
        }
        let Some([Some(argument)]) = registration_arguments(arguments, keywords, ["config"]) else {
            return None;
        };
        if self.call.is_some() {
            return None;
        }
        let factories = match self.evaluate(argument, 0) {
            Some(Data::Dictionary(id)) => self.factories(id)?,
            None if self.missing_instance_field(argument) => {
                self.stopped = true;
                Vec::new()
            }
            _ => return None,
        };
        self.mutations.insert((scope, position(expression.at)));
        self.call = Some((expression.at, factories));
        Some(())
    }

    fn mutate(&mut self, scope: ScopeId, expression: &'a Expression) -> Option<()> {
        let ExpressionKind::Call {
            function,
            arguments,
            keywords,
        } = &expression.kind
        else {
            return None;
        };
        if let ExpressionKind::Member(owner, name) = &function.kind
            && let Some(Data::Dictionary(id)) = self.evaluate(owner, 0)
        {
            match (name.as_str(), arguments.as_slice(), keywords.as_slice()) {
                ("clear", [], []) => self.dictionaries[id].clear(),
                ("update", [mapping], []) => {
                    let Data::Dictionary(source) = self.evaluate(mapping, 0)? else {
                        return None;
                    };
                    let items = self.dictionaries[source].clone();
                    if self.dictionaries[id].len() + items.len() > MAX_ITEMS {
                        return None;
                    }
                    for (name, value) in items {
                        put(&mut self.dictionaries[id], &name, value);
                    }
                }
                _ => return None,
            }
            self.mutations.insert((scope, position(expression.at)));
            return Some(());
        }
        None
    }

    fn invoke_mutator(
        &mut self,
        graph: &mut Graph<'a>,
        invocation: Invocation,
        at: Location,
        arguments: &'a [Expression],
        keywords: &'a [(String, Expression)],
    ) -> Option<()> {
        let Binding::Definition(definition, body) = graph.symbols[invocation.target].binding else {
            return None;
        };
        if invocation.verified
            || !matches!(invocation.receiver, Receiver::Ordinary)
            || !plain_function(definition)
            || !definition.simple_positional_parameters
            || definition.is_generator
            || !keywords.is_empty()
        {
            return None;
        }
        let [parameter] = definition.parameters.as_slice() else {
            return None;
        };
        let [argument] = arguments else {
            return None;
        };
        let Data::Dictionary(id) = self.evaluate(argument, 0)? else {
            return None;
        };
        // The helper may mutate only its one explicit argument. Its other
        // symbols and control flow remain outside this replay.
        let previous = std::mem::replace(
            &mut self.names,
            BTreeMap::from([(parameter.text.as_str(), Data::Dictionary(id))]),
        );
        let result = body_without_docstring(definition)
            .iter()
            .try_for_each(|statement| {
                let StatementKind::Evaluate(expression) = &statement.kind else {
                    return None;
                };
                self.mutate(body, expression)
            });
        self.names = previous;
        result?;
        self.helpers
            .entry(invocation.target)
            .or_default()
            .insert(position(at));
        Some(())
    }

    fn builtin_type_test(
        &mut self,
        graph: &mut Graph<'a>,
        scope: ScopeId,
        expression: &'a Expression,
        before: Location,
    ) -> Option<bool> {
        let ExpressionKind::Call {
            function,
            arguments,
            keywords,
        } = &expression.kind
        else {
            return None;
        };
        let [value, ty] = arguments.as_slice() else {
            return None;
        };
        if !keywords.is_empty()
            || !matches!(&function.kind, ExpressionKind::Name(name) if name == "isinstance"
                && matches!(graph.lexical_binding(scope, name, before, true), LexicalBinding::Missing))
        {
            return None;
        }
        let Value::Provider(provider) = graph.probe_identity(scope, ty, before) else {
            return None;
        };
        let value = self.evaluate(value, 0)?;
        let result = match provider.as_str() {
            "builtins.str" => matches!(value, Data::Text(_)),
            "builtins.dict" => matches!(value, Data::Dictionary(_)),
            "os.PathLike" if !matches!(value, Data::Instance(_) | Data::Class(_)) => {
                self.dictionary_is_pathlike && matches!(value, Data::Dictionary(_))
            }
            _ => return None,
        };
        self.mutations.insert((scope, position(expression.at)));
        Some(result)
    }

    fn condition(
        &mut self,
        graph: &mut Graph<'a>,
        scope: ScopeId,
        expression: &'a Expression,
        before: Location,
        depth: usize,
    ) -> Option<bool> {
        if depth >= MAX_REPLAY_DEPTH {
            return None;
        }
        if let ExpressionKind::Boolean { and, values } = &expression.kind {
            for value in values {
                let truth = self.condition(graph, scope, value, before, depth + 1)?;
                if truth != *and {
                    return Some(truth);
                }
            }
            return Some(*and);
        }
        match self.evaluate(expression, 0) {
            Some(Data::Bool(value)) => Some(value),
            _ => self.builtin_type_test(graph, scope, expression, before),
        }
    }

    fn accounts_for_helpers(&self, graph: &Graph<'_>, file: ScopeId) -> bool {
        self.helpers.iter().all(|(&target, sites)| {
            !graph.scopes.iter().any(|scope| {
                scope
                    .edges
                    .iter()
                    .any(|edge| edge.target == target && edge.kind != "direct_call")
            }) && graph.direct_arguments.get(&target).is_some_and(|calls| {
                calls.iter().all(|call| {
                    call.caller == file
                        && call.arguments_bound
                        && sites.contains(&position(call.at))
                })
            })
        })
    }

    fn factories(&self, root: usize) -> Option<Vec<Factory>> {
        let config = &self.dictionaries[root];
        match get(config, "version") {
            Some(Data::Integer(1) | Data::Bool(true)) => {}
            // dictConfig rejects an unsupported version before factory lookup.
            None | Some(Data::Integer(_) | Data::Bool(false) | Data::None | Data::Text(_)) => {
                return Some(Vec::new());
            }
            _ => return None,
        }
        if !matches!(get(config, "incremental"), None | Some(Data::Bool(false))) {
            return None;
        }
        for name in ["handlers", "loggers", "root"] {
            match get(config, name) {
                None => {}
                Some(Data::Dictionary(id)) if self.dictionaries[*id].is_empty() => {}
                _ => return None,
            }
        }
        let mut factories = Vec::new();
        for name in ["formatters", "filters"] {
            let Some(entries) = get(config, name) else {
                continue;
            };
            let Data::Dictionary(entries) = entries else {
                return None;
            };
            for (_, entry) in &self.dictionaries[*entries] {
                let Data::Dictionary(entry) = entry else {
                    return None;
                };
                let entry = &self.dictionaries[*entry];
                let (target, default_formatter) = match get(entry, "()") {
                    Some(target) => (Some(target), false),
                    None if name == "formatters" => (get(entry, "class"), true),
                    None => (None, false),
                };
                let Some(target) = target else {
                    if !entry.is_empty() {
                        return None;
                    }
                    continue;
                };
                let Data::Text(path) = target else {
                    return None;
                };
                if get(entry, ".").is_some() {
                    return None;
                }
                factories.push(Factory {
                    path: path.clone(),
                    default_formatter,
                    has_options: entry.len() > 1,
                });
            }
        }
        Some(factories)
    }
}

fn get<'a>(mapping: &'a Dictionary, key: &str) -> Option<&'a Data> {
    mapping
        .iter()
        .find(|(name, _)| name == key)
        .map(|(_, value)| value)
}

fn put(mapping: &mut Dictionary, key: &str, value: Data) {
    if let Some((_, previous)) = mapping.iter_mut().find(|(name, _)| name == key) {
        *previous = value;
    } else {
        mapping.push((key.into(), value));
    }
}

fn provider_alias(graph: &mut Graph<'_>, scope: ScopeId, expr: &Expression, at: Location) -> bool {
    matches!(
        expr.kind,
        ExpressionKind::Name(_) | ExpressionKind::Member(_, _)
    ) && matches!(graph.probe_identity(scope, expr, at), Value::Provider(name)
        if matches!(name.as_str(), "logging" | "logging.config" | DICT_CONFIG
            | "os" | "os.PathLike" | "os.PathLike.register"))
}

fn plain_function(definition: &Definition) -> bool {
    definition.kind == DefinitionKind::Function
        && definition.annotations.is_empty()
        && definition.decorators.is_empty()
        && definition.defaults.is_empty()
}

fn imports_logging(statement: &Statement) -> bool {
    matches!(&statement.kind, StatementKind::Import(import)
        if import.level == 0 && matches!(import.module.as_str(), "logging" | "logging.config"))
}

impl<'a> Graph<'a> {
    pub(super) fn defer_log_record_message(
        &mut self,
        scope: ScopeId,
        callee: &Expression,
        before: Location,
        at: Location,
    ) -> bool {
        let Some(parameter) = self.log_record_member(scope, callee, before, &["getMessage"]) else {
            return false;
        };
        self.logging_record_calls.push(ParameterCall {
            scope,
            parameter,
            at,
        });
        true
    }

    pub(super) fn defer_log_record_json(
        &mut self,
        scope: ScopeId,
        value: &Expression,
        before: Location,
        at: Location,
    ) -> bool {
        let ExpressionKind::Dictionary(items) = &value.kind else {
            return false;
        };
        let (items, remainder) = items.as_chunks::<2>();
        if !remainder.is_empty() {
            return false;
        }
        let mut record = None;
        for [key, value] in items {
            if !matches!(key.kind, ExpressionKind::Text(_)) {
                return false;
            }
            let parameter = match &value.kind {
                ExpressionKind::Text(_)
                | ExpressionKind::Scalar(Some(_))
                | ExpressionKind::Bool(_)
                | ExpressionKind::None => continue,
                ExpressionKind::Member(_, _) => {
                    self.log_record_member(scope, value, before, &["levelname", "name"])
                }
                ExpressionKind::Call {
                    function,
                    arguments,
                    keywords,
                } if arguments.is_empty() && keywords.is_empty() => {
                    self.log_record_member(scope, function, before, &["getMessage"])
                }
                _ => None,
            };
            let Some(parameter) = parameter else {
                return false;
            };
            record = Some(parameter);
        }
        let Some(parameter) = record else {
            return false;
        };
        self.logging_record_calls.push(ParameterCall {
            scope,
            parameter,
            at,
        });
        true
    }

    fn log_record_member(
        &self,
        scope: ScopeId,
        expression: &Expression,
        before: Location,
        members: &[&str],
    ) -> Option<SymbolId> {
        let ExpressionKind::Member(receiver, member) = &expression.kind else {
            return None;
        };
        if !members.contains(&member.as_str()) {
            return None;
        }
        let owner = self.scopes[scope].owner?;
        let Binding::Definition(definition, _) = self.symbols[owner].binding else {
            return None;
        };
        let [_, record] = definition.parameters.as_slice() else {
            return None;
        };
        if definition.name.text != "format"
            || !definition.simple_positional_parameters
            || !definition.decorators.is_empty()
            || !matches!(&receiver.kind, ExpressionKind::Name(name) if name == &record.text)
        {
            return None;
        }
        self.parameter_binding(scope, receiver, before)
    }

    pub(super) fn report_log_record_calls(&mut self, reach: &Reachability) {
        if self.logging_record_calls.is_empty() {
            return;
        }
        // LogRecord.getMessage can format a supplied object. Keep coverage
        // open when selected code can supply a custom formatting operation
        // or an opaque action can alter the record contract.
        let closed = !self.has_format_hooks
            && !self.has_iteration_hooks
            && !self
                .symbols
                .iter()
                .any(|symbol| matches!(symbol.name.text.as_str(), "__mod__" | "__rmod__"))
            && !self
                .scope_issues
                .iter()
                .any(|(scope, _)| reach.scopes.contains(scope));
        for call in &self.logging_record_calls {
            if !reach.scopes.contains(&call.scope) {
                continue;
            }
            let owner = self.scopes[self.symbols[call.parameter].scope]
                .owner
                .expect("parameters belong to a definition");
            if closed
                && self.logging_formatter_methods.contains(&owner)
                && reach.has_evidence(owner, "django_logging_factory")
                && reach
                    .evidence
                    .range((owner, "", "")..)
                    .take_while(|(target, _, _)| *target == owner)
                    .all(|(_, kind, _)| {
                        matches!(*kind, "django_logging_factory" | "type_reference")
                    })
            {
                continue;
            }
            self.issues.push(
                Issue::new(
                    "unresolved_call",
                    &self.files[self.scopes[call.scope].file].path,
                    "call target is outside the bounded model",
                )
                .at(call.at),
            );
        }
    }

    pub(super) fn terminal_logging_config_failure(
        &mut self,
        scope: ScopeId,
        statement: &Statement,
    ) -> bool {
        let at = statement.at;
        let missing = match &statement.kind {
            StatementKind::WriteAttribute {
                value: Some(value), ..
            } => value,
            StatementKind::Evaluate(Expression {
                kind:
                    ExpressionKind::Call {
                        function,
                        arguments,
                        keywords,
                    },
                ..
            }) if keywords.is_empty() => match arguments.as_slice() {
                [_] if self.unbound_module_name(scope, function, at) => function,
                [operand] | [_, _, operand] => operand,
                _ => return false,
            },
            _ => return false,
        };
        if !self.unbound_module_name(scope, missing, at) {
            return false;
        }
        let body = self.scopes[scope].body;
        let Some(index) = body.iter().position(|item| std::ptr::eq(item, statement)) else {
            return false;
        };
        let (prefix, remaining) = body.split_at(index);
        if !prefix.iter().any(imports_logging) {
            return false;
        }
        let statement = &remaining[0];
        let (receiver, argument) = match &statement.kind {
            StatementKind::WriteAttribute {
                receiver,
                member,
                value: Some(value),
            } if self.unbound_module_name(scope, value, at)
                && matches!(
                    member.as_str(),
                    "dictConfig" | "PathLike" | "__subclasshook__"
                )
                && self.logging_member(scope, receiver, member, at) =>
            {
                // The failed right hand side is evaluated before the assignment receiver.
                (None, None)
            }
            StatementKind::Evaluate(Expression {
                kind:
                    ExpressionKind::Call {
                        function,
                        arguments,
                        keywords,
                    },
                ..
            }) if keywords.is_empty() => match arguments.as_slice() {
                [argument] if self.unbound_module_name(scope, function, at) => {
                    (None, Some(argument))
                }
                [argument]
                    if self.unbound_module_name(scope, argument, at)
                        && matches!(&function.kind, ExpressionKind::Name(_))
                        && self.logging_wrapper(scope, function, at) =>
                {
                    (Some(function.as_ref()), None)
                }
                [
                    receiver,
                    Expression {
                        kind: ExpressionKind::Text(member),
                        ..
                    },
                    value,
                ] if self.unbound_module_name(scope, value, at)
                    && matches!(&function.kind, ExpressionKind::Name(_))
                    && matches!(self.probe_identity(scope, function, at),
                            Value::Provider(name) if name == "builtins.setattr")
                    && self.logging_member(scope, receiver, member, at) =>
                {
                    (Some(receiver), None)
                }
                _ => return false,
            },
            _ => return false,
        };
        let mut replay = Replay::default();
        for item in prefix {
            if replay.statement(self, scope, item).is_none()
                || replay.call.is_some()
                || !replay.instances.is_empty()
            {
                return false;
            }
        }
        if let Some(argument) = argument
            && !matches!(self.probe_identity(scope, argument, at),
                Value::Provider(name) if name == "os.PathLike" || name == DICT_CONFIG)
            && !matches!(replay.evaluate(argument, 0), Some(Data::Dictionary(id))
                if replay.factories(id).is_some_and(|factories| !factories.is_empty()))
        {
            return false;
        }
        if let Some(receiver) = receiver {
            self.walk(scope, receiver, at);
        }
        true
    }

    fn logging_member(
        &mut self,
        scope: ScopeId,
        receiver: &Expression,
        member: &str,
        at: Location,
    ) -> bool {
        matches!(
            &receiver.kind,
            ExpressionKind::Name(_) | ExpressionKind::Member(_, _)
        ) && matches!(self.probe_identity(scope, receiver, at), Value::Provider(name)
                if matches!((name.as_str(), member), ("logging.config", "dictConfig")
                    | ("os", "PathLike") | ("os.PathLike", "__subclasshook__")))
    }

    fn logging_wrapper(&mut self, scope: ScopeId, function: &Expression, at: Location) -> bool {
        let Value::Callable(invocation) = self.probe_identity(scope, function, at) else {
            return false;
        };
        let symbol = &self.symbols[invocation.target];
        symbol.scope == scope
            && matches!(symbol.binding, Binding::Definition(definition, _)
            if plain_function(definition) && definition.body.iter().any(|statement|
                matches!(&statement.kind, StatementKind::Evaluate(Expression {
                    kind: ExpressionKind::Call { function, .. }, .. })
                    if matches!(&function.kind, ExpressionKind::Member(_, member)
                        if member == "dictConfig"))))
    }

    pub(super) fn collect_logging_configs(&mut self) {
        let mut modeled = BTreeSet::new();
        let mut method_scopes = BTreeSet::new();
        for file in 0..self.files.len() {
            if self.failed_terminal_modules.contains(&file)
                || self.scopes[file].open_wildcard
                || !self.scopes[file].body.iter().any(imports_logging)
            {
                continue;
            }
            let mut replay = Replay::default();
            let Some((last, preceding)) = self.scopes[file].body.split_last() else {
                continue;
            };
            let replayed = preceding
                .iter()
                .try_for_each(|statement| replay.statement(self, file, statement));
            if replayed.is_none()
                || replay.call.is_some()
                || replay.statement(self, file, last).is_none()
                || !replay.accounts_for_helpers(self, file)
                || !replay.accounts_for_methods(self, file)
            {
                continue;
            }
            let Some((at, factories)) = replay.call else {
                continue;
            };
            for &(scope, _, target) in &replay.calls {
                self.edge(scope, target, "direct_call", "certain");
                self.direct.insert(target);
            }
            for factory in &factories {
                match self.record_logging_factory(file, last.at, factory) {
                    Dispatch::Continues => {}
                    Dispatch::Rejected => break,
                    Dispatch::Unknown => {
                        self.scope_issue(
                            file,
                            at,
                            "unresolved_registration",
                            "logging factory dispatch is outside the bounded configuration model",
                        );
                        break;
                    }
                }
            }
            modeled.extend(replay.mutations);
            method_scopes.extend(replay.method_scopes);
        }
        self.receiver_calls
            .retain(|call| !method_scopes.contains(&call.scope));
        self.scope_issues.retain(|(scope, issue)| {
            if method_scopes.contains(scope)
                && matches!(
                    issue.kind,
                    "unresolved_call" | "unresolved_attribute_write" | "unresolved_subscript"
                )
            {
                return false;
            }
            !matches!(issue.kind, "unresolved_call" | "unresolved_attribute_write")
                || !issue
                    .line
                    .zip(issue.column)
                    .is_some_and(|(line, column)| modeled.contains(&(*scope, [line, column])))
        });
    }

    fn logging_safe_import(&self, scope: ScopeId, import: &Import) -> bool {
        let Some(module) = self.import_module(self.scopes[scope].file, import) else {
            return false;
        };
        if matches!(module.as_str(), "logging" | "logging.config")
            && !self.modules.contains_key("logging")
            && !self.modules.contains_key("logging.config")
        {
            return import.level == 0;
        }
        if module == "os" && !self.modules.contains_key("os") {
            return import.level == 0;
        }
        if !self.modules.contains_key(module.as_str()) {
            return false;
        }
        for end in module
            .match_indices('.')
            .map(|(end, _)| end)
            .chain(std::iter::once(module.len()))
        {
            if let Some(&file) = self.modules.get(&module[..end])
                && (self.failed_terminal_modules.contains(&file)
                    || !self.scopes[file].body.iter().all(|statement| {
                        matches!(
                            &statement.kind,
                            StatementKind::Evaluate(Expression {
                                kind: ExpressionKind::Text(_),
                                ..
                            })
                        ) || matches!(&statement.kind, StatementKind::Define(definition)
                                if plain_function(definition))
                    }))
            {
                return false;
            }
        }
        true
    }

    fn record_logging_factory(
        &mut self,
        scope: ScopeId,
        at: Location,
        factory: &Factory,
    ) -> Dispatch {
        let Some((module, name)) = factory.path.rsplit_once('.') else {
            return Dispatch::Unknown;
        };
        let Some(&file) = self.modules.get(module) else {
            return Dispatch::Unknown;
        };
        let mut parents = Vec::new();
        for (end, _) in module.match_indices('.') {
            let prefix = &module[..end];
            let member = module[end + 1..].split('.').next().unwrap_or_default();
            let Some(&parent) = self.modules.get(prefix) else {
                return Dispatch::Unknown;
            };
            // The stdlib resolver reads each package attribute before trying
            // an import. A package can expose a different member under that name.
            if !self.files[parent].is_package
                || self
                    .module_member_write(prefix, member, (scope, at))
                    .is_some()
                || !self.scopes[parent].body.iter().all(|statement| {
                    matches!(
                        &statement.kind,
                        StatementKind::Evaluate(Expression {
                            kind: ExpressionKind::Text(_),
                            ..
                        })
                    )
                })
            {
                return Dispatch::Unknown;
            }
            if !within_package(&self.files[self.scopes[scope].file].module, prefix) {
                parents.push(parent);
            }
        }
        let before = if scope == file { at } else { END };
        let Some(binding) = self.local(file, name, before) else {
            return Dispatch::Unknown;
        };
        if self.failed_terminal_modules.contains(&file)
            || self.scopes[file].open_wildcard
            || self.binding_depends_on_branch(binding, before)
            || self
                .module_member_write(module, name, (scope, at))
                .is_some()
        {
            return Dispatch::Unknown;
        }
        let Some(target) = self.probe_callable_binding(binding) else {
            return Dispatch::Unknown;
        };
        let class = target.target;
        let Binding::Definition(definition, _) = self.symbols[class].binding else {
            return Dispatch::Unknown;
        };
        if definition.kind != DefinitionKind::Class
            || !definition.decorators.is_empty()
            || definition.table_keyword.is_some()
        {
            return Dispatch::Unknown;
        }
        self.read.extend(target.bindings);
        self.edge(scope, class, "logging_factory", "possible");
        if !self.scopes[scope].imports.contains(&file) && file != self.scopes[scope].file {
            self.scopes[scope].imports.push(file);
        }
        for parent in parents {
            if !self.scopes[scope].imports.contains(&parent) {
                self.scopes[scope].imports.push(parent);
            }
        }
        // The declaration is looked up even when a default formatter passes
        // positional arguments to a class that cannot consume them.
        if definition.bases.is_empty()
            && self
                .class_member_at(class, "__init__", 0, Some((scope, at)))
                .is_none()
            && self
                .class_member_at(class, "__new__", 0, Some((scope, at)))
                .is_none()
        {
            return if factory.default_formatter || factory.has_options {
                Dispatch::Rejected
            } else {
                Dispatch::Continues
            };
        }
        Dispatch::Unknown
    }
}
