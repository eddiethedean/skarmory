//! Reached stdlib logging configuration and bounded dictionary mutations.

use super::*;

pub(super) const DICT_CONFIG: &str = "logging.config.dictConfig";
const MAX_ITEMS: usize = 256;
const MAX_REPLAY_DEPTH: usize = 16;

#[derive(Clone)]
enum Data {
    Dictionary(usize),
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
    mutations: BTreeSet<Site>,
    helpers: BTreeMap<SymbolId, BTreeSet<[usize; 2]>>,
    call: Option<(Location, Vec<Factory>)>,
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
                let branch = match self.evaluate(test, 0).or_else(|| {
                    self.builtin_type_test(graph, scope, test, at)
                        .map(Data::Bool)
                })? {
                    Data::Bool(true) => body,
                    Data::Bool(false) => otherwise,
                    _ => return None,
                };
                for statement in branch {
                    self.statement(graph, scope, statement)?;
                }
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
        let provider = graph.probe_identity(scope, function, before);
        if let Value::Callable(invocation) = provider {
            return self.invoke_mutator(graph, invocation, expression.at, arguments, keywords);
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
        let Data::Dictionary(id) = self.evaluate(argument, 0)? else {
            return None;
        };
        if self.call.is_some() {
            return None;
        }
        self.call = Some((expression.at, self.factories(id)?));
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
            _ => return None,
        };
        self.mutations.insert((scope, position(expression.at)));
        Some(result)
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
    ) && matches!(graph.probe_identity(scope, expr, at),
            Value::Provider(name) if matches!(name.as_str(), "logging" | "logging.config" | DICT_CONFIG))
}

fn plain_function(definition: &Definition) -> bool {
    definition.kind == DefinitionKind::Function
        && definition.annotations.is_empty()
        && definition.decorators.is_empty()
        && definition.defaults.is_empty()
}

impl<'a> Graph<'a> {
    pub(super) fn collect_logging_configs(&mut self) {
        let mut modeled = BTreeSet::new();
        for file in 0..self.files.len() {
            if self.failed_terminal_modules.contains(&file)
                || self.scopes[file].open_wildcard
                || !self.scopes[file].body.iter().any(|statement| {
                    matches!(&statement.kind, StatementKind::Import(import)
                        if matches!(import.module.as_str(), "logging" | "logging.config"))
                })
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
            {
                continue;
            }
            let Some((at, factories)) = replay.call else {
                continue;
            };
            for factory in &factories {
                match self.record_logging_factory(file, at, factory) {
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
            modeled.insert((file, position(at)));
        }
        self.scope_issues.retain(|(scope, issue)| {
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
