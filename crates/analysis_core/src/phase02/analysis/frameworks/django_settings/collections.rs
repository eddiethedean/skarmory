//! Bounded operations on the built-in collections used by Django settings.

use super::*;

const MAX_STATES: usize = 64;
const MAX_ITEMS: usize = 4096;
const AUTH_BACKENDS: &str = "AUTHENTICATION_BACKENDS";

#[derive(Clone)]
enum Collection {
    List(Vec<Value>),
    Dict(BTreeMap<String, Value>),
    Immutable(Value),
}

impl Collection {
    fn from_expression(expression: &Expression) -> Option<Self> {
        match (&expression.kind, literal(expression)?) {
            (ExpressionKind::Sequence { list: true, .. }, Value::Sequence(items)) => {
                Some(Self::List(items))
            }
            (ExpressionKind::Dictionary(_), Value::Mapping(items)) => {
                Some(Self::Dict(text_dictionary(items)?))
            }
            (_, value) => Some(Self::Immutable(value)),
        }
    }

    fn apply(&mut self, method: &str, arguments: &[Expression]) -> Option<()> {
        match (self, method, arguments) {
            (Self::List(items), "append", [item]) => {
                items.push(literal(item)?);
                (items.len() <= MAX_ITEMS).then_some(())
            }
            (Self::List(items), "extend", [sequence]) => {
                match literal(sequence)? {
                    Value::Sequence(values) => items.extend(values),
                    Value::Text(text) => {
                        items.extend(text.chars().map(|c| Value::Text(c.to_string())));
                    }
                    _ => return None,
                }
                (items.len() <= MAX_ITEMS).then_some(())
            }
            (Self::List(items), "insert", [index, item]) => {
                let ExpressionKind::Scalar(Some(index)) = index.kind else {
                    return None;
                };
                let index = usize::try_from(index).ok()?.min(items.len());
                items.insert(index, literal(item)?);
                (items.len() <= MAX_ITEMS).then_some(())
            }
            (Self::List(items), "remove", [item]) => {
                let ExpressionKind::Text(needle) = &item.kind else {
                    return None;
                };
                let mut found = None;
                for (index, value) in items.iter().enumerate() {
                    let Value::Text(value) = value else {
                        return None;
                    };
                    if needle == value {
                        found = Some(index);
                        break;
                    }
                }
                items.remove(found?);
                Some(())
            }
            (Self::List(items), "pop", []) => items.pop().map(|_| ()),
            (Self::List(items), "clear", []) => {
                items.clear();
                Some(())
            }
            (Self::Dict(items), "update", [mapping]) => {
                let Value::Mapping(mapping) = literal(mapping)? else {
                    return None;
                };
                items.extend(text_dictionary(mapping)?);
                (items.len() <= MAX_ITEMS).then_some(())
            }
            (Self::Dict(items), "clear", []) => {
                items.clear();
                Some(())
            }
            _ => None,
        }
    }

    fn into_value(self) -> Value {
        match self {
            Self::List(items) => Value::Sequence(items),
            Self::Immutable(value) => value,
            Self::Dict(items) => Value::Mapping(
                items
                    .into_iter()
                    .flat_map(|(key, value)| [Value::Text(key), value])
                    .collect(),
            ),
        }
    }

    fn edit_backend_list(&mut self, edit: &DjangoBackendListEdit) -> Option<()> {
        let Self::List(items) = self else {
            return None;
        };
        match edit {
            DjangoBackendListEdit::Extend(value) => {
                self.apply("extend", std::slice::from_ref(value))
            }
            DjangoBackendListEdit::Repeat(times) => {
                if *times <= 0 || items.is_empty() {
                    items.clear();
                    return Some(());
                }
                let times = usize::try_from(*times).ok()?;
                let length = items.len();
                if length.checked_mul(times)? > MAX_ITEMS {
                    return None;
                }
                for _ in 1..times {
                    items.extend_from_within(..length);
                }
                Some(())
            }
            DjangoBackendListEdit::Item { index, value } => {
                let index = if *index < 0 {
                    index.checked_add(i64::try_from(items.len()).ok()?)?
                } else {
                    *index
                };
                let index = usize::try_from(index).ok()?;
                items.get(index)?;
                if let Some(value) = value {
                    items[index] = literal(value)?;
                } else {
                    items.remove(index);
                }
                Some(())
            }
        }
    }
}

fn text_dictionary(items: Vec<Value>) -> Option<BTreeMap<String, Value>> {
    let mut result = BTreeMap::new();
    let mut items = items.into_iter();
    while let Some(key) = items.next() {
        let Value::Text(key) = key else {
            return None;
        };
        result.insert(key, items.next()?);
    }
    Some(result)
}

fn literal(expression: &Expression) -> Option<Value> {
    match &expression.kind {
        ExpressionKind::Text(value) if value.len() <= MAX_ITEMS => Some(Value::Text(value.clone())),
        ExpressionKind::Bool(value) => Some(Value::Bool(*value)),
        ExpressionKind::Scalar(Some(value)) => Some(Value::Integer(*value)),
        ExpressionKind::Scalar(None) => Some(Value::Scalar),
        ExpressionKind::None => Some(Value::None),
        ExpressionKind::Sequence { items, .. } if items.len() <= MAX_ITEMS => Some(
            Value::Sequence(items.iter().map(literal).collect::<Option<_>>()?),
        ),
        ExpressionKind::Dictionary(items) if items.len() <= MAX_ITEMS => {
            let (pairs, remainder) = items.as_chunks::<2>();
            if !remainder.is_empty()
                || pairs
                    .iter()
                    .any(|[key, _]| !matches!(key.kind, ExpressionKind::Text(_)))
            {
                return None;
            }
            Some(Value::Mapping(
                items.iter().map(literal).collect::<Option<_>>()?,
            ))
        }
        _ => None,
    }
}

#[derive(Clone, Default)]
struct State {
    module: Option<Collection>,
    // None represents a global read. Some(None) is a shadowing local whose
    // value has not been bound to a known container.
    local: Option<Option<Collection>>,
    stopped: bool,
}

struct Replay<'g, 'a> {
    graph: &'g mut Graph<'a>,
    module: ScopeId,
    name: &'static str,
    mutations: BTreeSet<(ScopeId, [usize; 2])>,
    helpers: BTreeMap<SymbolId, BTreeSet<[usize; 2]>>,
    selected_import: bool,
}

impl<'a> Replay<'_, 'a> {
    fn suite(
        &mut self,
        scope: ScopeId,
        statements: &'a [Statement],
        mut states: Vec<State>,
    ) -> Option<Vec<State>> {
        for statement in statements {
            if states.iter().all(|s| s.stopped) {
                break;
            }
            match &statement.kind {
                StatementKind::Assign {
                    names,
                    value,
                    annotation: None,
                } if names.len() == 1 => {
                    let value = value.as_ref()?;
                    let name = &names[0].text;
                    if name == self.name {
                        let binding = Some(Collection::from_expression(value)?);
                        for state in states.iter_mut().filter(|s| !s.stopped) {
                            if let Some(local) = &mut state.local {
                                *local = binding.clone();
                            } else if scope == self.module {
                                state.module = binding.clone();
                            } else {
                                return None;
                            }
                        }
                    } else if literal(value).is_none()
                        && !matches!(value.kind, ExpressionKind::Name(ref name) if name != self.name
                            && matches!(self.graph.probe_identity(scope, value, statement.at), Value::Callable(_)))
                    {
                        // Do not carry a mutable value past an untracked alias
                        // or an assignment whose evaluation can escape.
                        return None;
                    }
                }
                StatementKind::DeleteName(name)
                    if name.text == self.name && scope == self.module =>
                {
                    for state in states.iter_mut().filter(|s| !s.stopped) {
                        state.module = None;
                    }
                }
                StatementKind::Import(import)
                    if scope == self.module && import.local.text != self.name => {}
                StatementKind::Import(import) if scope == self.module => {
                    let module = self.graph.import_module(scope, import)?;
                    let source = *self.graph.modules.get(module.as_str())?;
                    if source == self.module || import.member.is_none() {
                        return None;
                    }
                    let binding = self.graph.by_location
                        [&(scope, import.local.at.line, import.local.at.column)];
                    // This branch has reached the import. Resolve the source
                    // member directly and keep the other branches' prior values.
                    let value = self.graph.binding_value(binding, 0);
                    if value.contains_unknown() {
                        return None;
                    }
                    self.graph.read.insert(binding);
                    self.selected_import = true;
                    for state in states.iter_mut().filter(|state| !state.stopped) {
                        // Imported containers can be shared with other modules.
                        // Mutating one still requires tracking the source identity.
                        state.module = Some(Collection::Immutable(value.clone()));
                    }
                }
                StatementKind::Define(definition)
                    if definition.kind != DefinitionKind::Class
                        && definition.name.text != self.name
                        && definition.decorators.is_empty()
                        && definition.annotations.is_empty()
                        && definition.defaults.is_empty() => {}
                StatementKind::Evaluate(value) if matches!(value.kind, ExpressionKind::Text(_)) => {
                }
                StatementKind::Evaluate(value) | StatementKind::DjangoSettingsGlobalCall(value) => {
                    self.call(scope, value, statement.at, &mut states)?;
                }
                StatementKind::EditDjangoBackendList { edit, .. }
                    if scope == self.module && self.name == "AUTHENTICATION_BACKENDS" =>
                {
                    if self
                        .graph
                        .scopes
                        .iter()
                        .any(|row| row.file != self.module && row.imports.contains(&self.module))
                    {
                        // Other consumers would still observe the original
                        // list identity in the general binding graph.
                        return None;
                    }
                    for state in states.iter_mut().filter(|s| !s.stopped) {
                        state.module.as_mut()?.edit_backend_list(edit)?;
                    }
                    self.mutations.insert((scope, position(statement.at)));
                }
                StatementKind::If {
                    test,
                    body,
                    otherwise,
                } => match self.graph.indexed_bool(scope, test, statement.at, 0) {
                    Some(true) => states = self.suite(scope, body, states)?,
                    Some(false) => states = self.suite(scope, otherwise, states)?,
                    None if matches!(&test.kind, ExpressionKind::Name(name) if name != self.name
                            && matches!(self.graph.lexical_binding(scope, name, statement.at, true), LexicalBinding::Missing)) =>
                    {
                        if states.len() > MAX_STATES / 2 {
                            return None;
                        }
                        let other = self.suite(scope, otherwise, states.clone())?;
                        states = self.suite(scope, body, states)?;
                        states.extend(other);
                        if states.len() > MAX_STATES {
                            return None;
                        }
                    }
                    _ => return None,
                },
                StatementKind::Return(value)
                    if scope != self.module
                        && value
                            .as_ref()
                            .is_none_or(|value| matches!(value.kind, ExpressionKind::None)) =>
                {
                    for state in &mut states {
                        state.stopped = true;
                    }
                }
                _ => return None,
            }
        }
        Some(states)
    }

    fn call(
        &mut self,
        scope: ScopeId,
        value: &'a Expression,
        before: Location,
        states: &mut Vec<State>,
    ) -> Option<()> {
        let ExpressionKind::Call {
            function,
            arguments,
            keywords,
        } = &value.kind
        else {
            return None;
        };
        if !keywords.is_empty() {
            return None;
        }
        if let ExpressionKind::Member(receiver, method) = &function.kind
            && matches!(&receiver.kind, ExpressionKind::Name(name) if name == self.name)
        {
            for state in states.iter_mut().filter(|s| !s.stopped) {
                state
                    .local
                    .as_mut()
                    .unwrap_or(&mut state.module)
                    .as_mut()?
                    .apply(method, arguments)?;
            }
            self.mutations.insert((scope, position(value.at)));
            return Some(());
        }
        if scope != self.module || !matches!(function.kind, ExpressionKind::Name(_)) {
            return None;
        }
        let Value::Callable(invocation) = self.graph.probe_identity(scope, function, before) else {
            return None;
        };
        if invocation.verified || !matches!(invocation.receiver, Receiver::Ordinary) {
            return None;
        }
        let Binding::Definition(definition, body) = self.graph.symbols[invocation.target].binding
        else {
            return None;
        };
        if self.graph.symbols[invocation.target].scope != self.module
            || definition.kind != DefinitionKind::Function
            || definition.is_generator
            || !definition.simple_positional_parameters
            || !definition.decorators.is_empty()
            || !definition.defaults.is_empty()
            || !definition.annotations.is_empty()
        {
            return None;
        }
        let local = match (definition.parameters.as_slice(), arguments.as_slice()) {
            ([], []) => suite_binds_name(&definition.body, self.name).then_some(None),
            ([parameter], [argument]) if parameter.text == self.name => {
                Some(Some(Collection::from_expression(argument)?))
            }
            _ => return None,
        };
        for state in states.iter_mut() {
            state.local = local.clone();
        }
        *states = self.suite(body, &definition.body, std::mem::take(states))?;
        for state in states.iter_mut() {
            state.local = None;
            state.stopped = false;
        }
        self.helpers
            .entry(invocation.target)
            .or_default()
            .insert(position(value.at));
        Some(())
    }

    fn accounts_for_helpers(&self, reach: &Reachability) -> bool {
        self.helpers.iter().all(|(&target, sites)| {
            !reach
                .evidence
                .iter()
                .any(|&(id, kind, _)| id == target && kind != "direct_call")
                && !self.graph.scopes.iter().any(|scope| {
                    scope
                        .edges
                        .iter()
                        .any(|edge| edge.target == target && edge.kind != "direct_call")
                })
                && self
                    .graph
                    .direct_arguments
                    .get(&target)
                    .is_some_and(|calls| {
                        calls.iter().all(|call| {
                            call.caller == self.module
                                && call.arguments_bound
                                && sites.contains(&position(call.at))
                        })
                    })
        })
    }
}

impl Graph<'_> {
    pub(in crate::phase02::analysis) fn terminal_django_auth_initializer(
        &mut self,
        scope: ScopeId,
        statement: &Statement,
    ) -> bool {
        let at = statement.at;
        let (receiver, item_index) = match &statement.kind {
            StatementKind::WriteModuleEntry {
                mapping:
                    Expression {
                        kind:
                            ExpressionKind::Subscript {
                                value: owner,
                                index,
                            },
                        ..
                    },
                value,
                ..
            } if backend_name(owner)
                && value.as_ref().is_none_or(|value| literal(value).is_some()) =>
            {
                let ExpressionKind::Scalar(Some(index)) = index.kind else {
                    return false;
                };
                (Some(owner.as_ref()), Some(index))
            }
            StatementKind::Evaluate(Expression {
                kind:
                    ExpressionKind::Call {
                        function,
                        arguments,
                        ..
                    },
                ..
            }) => match &function.kind {
                ExpressionKind::Member(owner, member)
                    if backend_name(owner) && missing_list_method(member) =>
                {
                    (Some(owner.as_ref()), None)
                }
                ExpressionKind::Name(_)
                    if matches!(arguments.as_slice(), [owner] if backend_name(owner))
                        && self.unbound_module_name(scope, function, at) =>
                {
                    (None, None)
                }
                _ => return false,
            },
            StatementKind::Assign {
                names,
                value: Some(value),
                annotation: None,
            } if self.unbound_module_name(scope, value, at) => {
                let [name] = names.as_slice() else {
                    return false;
                };
                let Some(old) = self.local(scope, &name.text, at) else {
                    return false;
                };
                let Binding::Definition(definition, _) = self.symbols[old].binding else {
                    return false;
                };
                if !body_without_docstring(definition)
                    .iter()
                    .any(backend_mutator_call)
                {
                    return false;
                }
                (None, None)
            }
            _ => return false,
        };
        let Some(items) = self.closed_auth_backend_list(scope, statement) else {
            return false;
        };
        if let Some(index) = item_index {
            let Ok(length) = i64::try_from(items.len()) else {
                return false;
            };
            if (-length..length).contains(&index) {
                return false;
            }
        }
        if let Some(receiver) = receiver {
            self.walk(scope, receiver, at);
        }
        true
    }

    fn closed_auth_backend_list(
        &self,
        scope: ScopeId,
        statement: &Statement,
    ) -> Option<&[Expression]> {
        let file = self.scopes[scope].body;
        let index = file.iter().position(|item| std::ptr::eq(item, statement))?;
        let prefix = &file[..index];
        if !prefix.iter().all(|item| match &item.kind {
            StatementKind::Assign {
                value: Some(value),
                annotation: None,
                ..
            } => literal(value).is_some(),
            StatementKind::Define(definition) => {
                definition.kind == DefinitionKind::Function
                    && definition.decorators.is_empty()
                    && definition.annotations.is_empty()
                    && definition.defaults.is_empty()
            }
            StatementKind::Evaluate(Expression {
                kind: ExpressionKind::Text(_),
                ..
            }) => true,
            _ => false,
        }) {
            return None;
        }
        let binding = self.local(scope, AUTH_BACKENDS, statement.at)?;
        let Binding::Variable(Some(value), _) = self.symbols[binding].binding else {
            return None;
        };
        let ExpressionKind::Sequence { list: true, items } = &value.kind else {
            return None;
        };
        Some(items)
    }

    pub(super) fn django_collection_values(
        &mut self,
        module: ScopeId,
        name: &'static str,
        reach: &Reachability,
    ) -> Option<Value> {
        let mut replay = Replay {
            graph: self,
            module,
            name,
            mutations: BTreeSet::new(),
            helpers: BTreeMap::new(),
            selected_import: false,
        };
        let statements = replay.graph.scopes[module].body;
        let states = replay.suite(module, statements, vec![State::default()])?;
        if (replay.mutations.is_empty() && !replay.selected_import)
            || !replay.accounts_for_helpers(reach)
        {
            return None;
        }
        // Preserve diagnostics from arguments and every statement the replay
        // did not establish. These exact calls operate on proven builtins.
        replay.graph.scope_issues.retain(|(scope, issue)| {
            !matches!(
                issue.kind,
                "unresolved_call" | "ambiguous_binding" | "unresolved_setting_mutation"
            ) || !issue
                .line
                .zip(issue.column)
                .is_some_and(|(line, column)| replay.mutations.contains(&(*scope, [line, column])))
        });
        let mut values = states
            .into_iter()
            .map(|state| state.module.map_or(Value::Unknown, Collection::into_value));
        if values.len() == 1 {
            values.next()
        } else {
            Some(Value::alternatives(values.collect()))
        }
    }
}

fn backend_name(expression: &Expression) -> bool {
    matches!(&expression.kind, ExpressionKind::Name(name) if name == AUTH_BACKENDS)
}

fn backend_mutator_call(statement: &Statement) -> bool {
    let expression = match &statement.kind {
        StatementKind::Evaluate(expression)
        | StatementKind::DjangoSettingsGlobalCall(expression) => expression,
        _ => return false,
    };
    matches!(&expression.kind, ExpressionKind::Call { function, .. }
        if matches!(&function.kind, ExpressionKind::Member(receiver, _) if backend_name(receiver)))
}

fn missing_list_method(member: &str) -> bool {
    !member.starts_with('_')
        && !matches!(
            member,
            "append"
                | "clear"
                | "copy"
                | "count"
                | "extend"
                | "index"
                | "insert"
                | "pop"
                | "remove"
                | "reverse"
                | "sort"
        )
}
