use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::rc::Rc;

use skarmory_python::preview::{BooleanOperator, Comparison, Expression, ExpressionKind};

use super::{
    Closure, Constant, Engine, FrameId, Names, Outcomes, ParameterKind, Site, State, Value,
};

impl Value {
    pub(super) fn truth(&self) -> Option<bool> {
        Some(match self {
            Self::Constant(Constant::None) => false,
            Self::Constant(Constant::Bool(value)) => *value,
            Self::Constant(Constant::Int(value)) => *value != 0,
            Self::Constant(Constant::String(value)) => !value.is_empty(),
            Self::Sequence(values, ..) => !values.is_empty(),
            Self::Mapping(values, _) => !values.is_empty(),
            Self::Module(_) | Self::Function(_) => true,
            Self::Unknown | Self::UnknownBool => return None,
        })
    }

    pub(super) fn same(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Constant(left), Self::Constant(right)) => left == right,
            (Self::Sequence(left, left_tuple, _), Self::Sequence(right, right_tuple, _)) => {
                left_tuple == right_tuple && Rc::ptr_eq(left, right)
            }
            (Self::Mapping(left, _), Self::Mapping(right, _)) => Rc::ptr_eq(left, right),
            (Self::Module(left), Self::Module(right)) => left == right,
            (Self::Function(left), Self::Function(right)) => Rc::ptr_eq(left, right),
            (Self::Unknown, Self::Unknown) | (Self::UnknownBool, Self::UnknownBool) => true,
            _ => false,
        }
    }

    fn identity(&self, other: &Self) -> Option<bool> {
        match (self, other) {
            (Self::Unknown, _) | (_, Self::Unknown) => None,
            (Self::UnknownBool, value) | (value, Self::UnknownBool) => {
                (!matches!(value, Self::UnknownBool | Self::Constant(Constant::Bool(_))))
                    .then_some(false)
            }
            (Self::Constant(Constant::Int(left)), Self::Constant(Constant::Int(right))) => {
                (left != right).then_some(false)
            }
            (Self::Constant(Constant::String(left)), Self::Constant(Constant::String(right))) => {
                (left != right).then_some(false)
            }
            (Self::Sequence(left, true, _), Self::Sequence(right, true, _)) => {
                Rc::ptr_eq(left, right).then_some(true)
            }
            _ => Some(self.same(other)),
        }
    }

    fn integer(&self) -> Option<i64> {
        match self {
            Self::Constant(Constant::Int(number)) => Some(*number),
            Self::Constant(Constant::Bool(value)) => Some(i64::from(*value)),
            _ => None,
        }
    }
}

impl<'a> Engine<'a> {
    pub(super) fn eval(
        &mut self,
        state: State,
        frame: FrameId,
        expression: &'a Expression,
    ) -> Outcomes<Value> {
        let site = Site::at(state.frame(frame).file, expression.location);
        if !self.step(site) {
            return Vec::new();
        }
        self.with_evaluation(site, |engine| {
            engine.eval_expression(state, frame, expression, site)
        })
        .unwrap_or_default()
    }

    fn eval_expression(
        &mut self,
        state: State,
        frame: FrameId,
        expression: &'a Expression,
        site: Site,
    ) -> Outcomes<Value> {
        match &expression.kind {
            ExpressionKind::Name(name) => {
                if let Some(value) = state.lookup(frame, name, self).cloned() {
                    vec![(state, value)]
                } else {
                    self.issue(
                        site,
                        "unbound_name",
                        format!("cannot resolve name {name:?}"),
                    );
                    Vec::new()
                }
            }
            ExpressionKind::Attribute { value, name } => self
                .eval(state, frame, value)
                .into_iter()
                .filter_map(|(state, base)| {
                    self.attribute(&state, base, name, site)
                        .map(|value| (state, value))
                })
                .collect(),
            ExpressionKind::Call {
                function,
                positional,
                keywords,
            } => self.call(state, frame, function, positional, keywords, site),
            ExpressionKind::Constant(constant) => vec![(state, Value::Constant(constant.clone()))],
            ExpressionKind::List(items) | ExpressionKind::Tuple(items) => {
                let tuple = matches!(expression.kind, ExpressionKind::Tuple(_));
                self.values(state, frame, items, site)
                    .into_iter()
                    .filter_map(|(state, values)| {
                        let value = Value::sequence(values, tuple);
                        self.check_value_depth(value.depth(), site)
                            .then_some((state, value))
                    })
                    .collect()
            }
            ExpressionKind::Compare { left, comparisons } => {
                self.comparison_chain(state, frame, left, comparisons, site)
            }
            ExpressionKind::Boolean { op, values } => self.boolean(state, frame, *op, values, site),
            ExpressionKind::Not(value) => self
                .eval(state, frame, value)
                .into_iter()
                .map(|(state, value)| {
                    let result = value
                        .truth()
                        .map_or(Value::UnknownBool, |value| boolean(!value));
                    (state, result)
                })
                .collect(),
        }
    }

    fn attribute(&mut self, state: &State, base: Value, name: &str, site: Site) -> Option<Value> {
        match base {
            Value::Module(module) => {
                let namespace = state
                    .modules
                    .get(&module)
                    .map(|loaded| &state.frame(loaded.frame).locals);
                let value = namespace.and_then(|namespace| namespace.get(name));
                if value.is_none()
                    && namespace.is_some_and(|namespace| namespace.contains_key("__getattr__"))
                {
                    self.issue(
                        site,
                        "unsupported_dynamic_export",
                        format!("resolving {module}.{name} requires a module __getattr__ hook"),
                    );
                } else if value.is_none() {
                    self.issue(
                        site,
                        "unresolved_attribute",
                        format!("cannot resolve {module}.{name}"),
                    );
                }
                value.cloned()
            }
            Value::Unknown => {
                self.unbounded(site, "cannot bound attribute lookup on an unknown value");
                Some(Value::Unknown)
            }
            _ => {
                self.issue(
                    site,
                    "unsupported_attribute",
                    "the preview supports attributes on selected modules",
                );
                None
            }
        }
    }

    fn values(
        &mut self,
        state: State,
        frame: FrameId,
        expressions: impl IntoIterator<Item = &'a Expression>,
        site: Site,
    ) -> Outcomes<Vec<Value>> {
        let mut paths = vec![(state, Vec::new())];
        for expression in expressions {
            let mut next = Vec::new();
            for (state, mut prefix) in paths {
                let mut outcomes = self.eval(state, frame, expression);
                if let Some((last_state, last_value)) = outcomes.pop() {
                    for (state, value) in outcomes {
                        let mut values = prefix.clone();
                        values.push(value);
                        next.push((state, values));
                    }
                    prefix.push(last_value);
                    next.push((last_state, prefix));
                }
                if !self.check_paths(next.len(), site) {
                    return Vec::new();
                }
            }
            paths = next;
        }
        paths
    }

    fn call(
        &mut self,
        state: State,
        frame: FrameId,
        callee: &'a Expression,
        positional: &'a [Expression],
        keywords: &'a [(String, Expression)],
        site: Site,
    ) -> Outcomes<Value> {
        let mut paths = Vec::new();
        for (state, target) in self.eval(state, frame, callee) {
            let args = positional
                .iter()
                .chain(keywords.iter().map(|(_, value)| value));
            for (state, mut values) in self.values(state, frame, args, site) {
                match &target {
                    Value::Function(function) => {
                        let named = values.split_off(positional.len());
                        let named = keywords.iter().map(|(name, _)| name).zip(named).collect();
                        match self.bind_arguments(function, values, named) {
                            Ok(args) => {
                                if !self.check_value_depth(
                                    args.values().map(Value::depth).max().unwrap_or(0),
                                    site,
                                ) {
                                    return Vec::new();
                                }
                                paths.extend(self.invoke(
                                    state,
                                    function.clone(),
                                    args,
                                    site,
                                    "call",
                                ));
                            }
                            Err(reason) => self.issue(site, "invalid_call", reason),
                        }
                    }
                    Value::Unknown => {
                        self.unbounded(site, "cannot bound the target of a call");
                        self.possible_argument_uses(state, &values, site);
                    }
                    _ => {
                        self.issue(site, "invalid_call", "call target is not callable");
                    }
                }
                if !self.check_paths(paths.len(), site) {
                    return Vec::new();
                }
            }
        }
        paths
    }

    fn possible_argument_uses<'b>(
        &mut self,
        mut state: State,
        values: impl IntoIterator<Item = &'b Value>,
        site: Site,
    ) {
        state.possible = true;
        // This path is incomplete already. Collect evidence immediately since
        // there is no successor state in which to schedule saved callbacks.
        state.library_calls = None;
        let mut pending = values.into_iter().collect::<Vec<_>>();
        while let Some(value) = pending.pop() {
            if !self.step(site) {
                return;
            }
            match value {
                Value::Function(function) => {
                    self.uncertainty(
                        site,
                        "possible_use",
                        "an unknown call target may invoke a function passed as an argument",
                    );
                    self.invoke_external(state.clone(), function.clone(), site, "argument_escape");
                }
                Value::Sequence(items, ..) => pending.extend(items.iter()),
                Value::Mapping(items, _) => pending.extend(items.values()),
                _ => {}
            }
        }
    }

    fn bind_arguments(
        &self,
        function: &Closure,
        positional: Vec<Value>,
        mut keywords: BTreeMap<&String, Value>,
    ) -> Result<Names, String> {
        let parameters = &self.definitions[&function.definition].node.parameters;
        let mut positional = positional.into_iter();
        let mut bindings = Names::new();
        for (parameter, default) in parameters.iter().zip(&function.defaults) {
            let name = &parameter.name;
            let supplied = match parameter.kind {
                ParameterKind::PositionalOnly => positional.next(),
                ParameterKind::PositionalOrKeyword => {
                    match (positional.next(), keywords.remove(name)) {
                        (Some(_), Some(_)) => {
                            return Err(format!("multiple arguments for {name:?}"));
                        }
                        (position, keyword) => position.or(keyword),
                    }
                }
                ParameterKind::VariadicPositional => {
                    Some(Value::sequence(positional.by_ref().collect(), true))
                }
                ParameterKind::KeywordOnly => keywords.remove(name),
                ParameterKind::VariadicKeyword => {
                    let extras = std::mem::take(&mut keywords)
                        .into_iter()
                        .map(|(name, value)| (name.clone(), value))
                        .collect();
                    Some(Value::mapping(extras))
                }
            };
            let value = supplied
                .or_else(|| default.clone())
                .ok_or_else(|| format!("missing argument {name:?}"))?;
            bindings.insert(name.clone(), value);
        }
        if positional.next().is_some() {
            return Err("too many positional arguments".into());
        }
        if let Some((name, _)) = keywords.first_key_value() {
            return Err(format!("unexpected keyword argument {name:?}"));
        }
        Ok(bindings)
    }

    fn boolean(
        &mut self,
        state: State,
        frame: FrameId,
        op: BooleanOperator,
        values: &'a [Expression],
        site: Site,
    ) -> Outcomes<Value> {
        let mut completed = Vec::new();
        let mut pending = vec![state];
        let stop_value = op == BooleanOperator::Or;
        for (index, operand) in values.iter().enumerate() {
            let mut next = Vec::new();
            for state in pending {
                for (mut state, value) in self.eval(state, frame, operand) {
                    if index + 1 == values.len() {
                        completed.push((state, value));
                    } else {
                        match value.truth() {
                            Some(truth) if truth == stop_value => completed.push((state, value)),
                            Some(_) => next.push(state),
                            None => {
                                state.possible = true;
                                completed.push((state.clone(), value));
                                next.push(state);
                            }
                        }
                    }
                    if !self.check_paths(next.len() + completed.len(), site) {
                        return Vec::new();
                    }
                }
            }
            pending = next;
        }
        completed
    }

    fn comparison_chain(
        &mut self,
        state: State,
        frame: FrameId,
        left: &'a Expression,
        comparisons: &'a [(Comparison, Expression)],
        site: Site,
    ) -> Outcomes<Value> {
        let mut pending = self.eval(state, frame, left);
        let mut completed = Vec::new();
        for (index, (op, expression)) in comparisons.iter().enumerate() {
            let last = index + 1 == comparisons.len();
            let mut next = Vec::new();
            for (state, left) in pending {
                for (mut state, right) in self.eval(state, frame, expression) {
                    let (has_unknown, has_selected_object) =
                        if matches!(op, Comparison::Is | Comparison::IsNot) {
                            (false, false)
                        } else {
                            self.comparison_operands([&left, &right], site)
                        };
                    if has_unknown && has_selected_object {
                        self.unbounded(
                            site,
                            "an unknown comparison hook may receive selected callable values",
                        );
                        self.possible_argument_uses(state, [&left, &right], site);
                        continue;
                    }
                    if has_unknown && self.comparison_exposes_mutable(*op, &left, &right, site) {
                        self.unbounded(
                            site,
                            "an unknown comparison hook may mutate a selected mutable container",
                        );
                        continue;
                    }
                    // Sequence ordering can return an element's comparison result.
                    // Sequence equality always converts the element result to a bool.
                    let may_return_object = match op {
                        Comparison::Equal | Comparison::NotEqual => {
                            matches!(left, Value::Unknown) || matches!(right, Value::Unknown)
                        }
                        Comparison::Less
                        | Comparison::LessEqual
                        | Comparison::Greater
                        | Comparison::GreaterEqual => has_unknown,
                        _ => false,
                    };
                    match self.compare(*op, &left, &right, site) {
                        Ok(Some(true)) => next.push((state, right)),
                        Ok(Some(false)) => completed.push((state, boolean(false))),
                        Ok(None) if may_return_object => {
                            // A falsy comparison result can itself be the chain's value.
                            if !last {
                                state.possible = true;
                                next.push((state.clone(), right));
                            }
                            completed.push((state, Value::Unknown));
                        }
                        Ok(None) => {
                            state.possible = true;
                            completed.push((state.clone(), boolean(false)));
                            next.push((state, right));
                        }
                        Err(message) => self.issue(site, "unsupported_comparison", message),
                    }
                    if !self.check_paths(next.len() + completed.len(), site) {
                        return Vec::new();
                    }
                }
            }
            pending = next;
        }
        completed.extend(pending.into_iter().map(|(state, _)| (state, boolean(true))));
        completed
    }

    fn comparison_operands(&mut self, operands: [&Value; 2], site: Site) -> (bool, bool) {
        let mut pending = operands.to_vec();
        let mut unknown = false;
        let mut selected_object = false;
        while let Some(value) = pending.pop() {
            if !self.step(site) {
                break;
            }
            match value {
                Value::Unknown => unknown = true,
                Value::Function(_) | Value::Module(_) => selected_object = true,
                Value::Sequence(values, ..) => pending.extend(values.iter()),
                Value::Mapping(values, _) => pending.extend(values.values()),
                _ => {}
            }
            if unknown && selected_object {
                break;
            }
        }
        (unknown, selected_object)
    }

    fn comparison_exposes_mutable(
        &mut self,
        op: Comparison,
        left: &Value,
        right: &Value,
        site: Site,
    ) -> bool {
        // Built-in containers compare their items. Only the values handed to
        // an unknown hook, including objects they contain, become exposed.
        let mut pending = if matches!(op, Comparison::In | Comparison::NotIn) {
            match right {
                Value::Unknown => return self.contains_mutable(left, site),
                Value::Sequence(items, ..) => items.iter().map(|item| (item, left)).collect(),
                _ => return false,
            }
        } else {
            vec![(left, right)]
        };
        while let Some((left, right)) = pending.pop() {
            if !self.step(site) {
                return false;
            }
            match (left, right) {
                (Value::Unknown, value) | (value, Value::Unknown) => {
                    if self.contains_mutable(value, site) {
                        return true;
                    }
                }
                (Value::Sequence(left, left_tuple, _), Value::Sequence(right, right_tuple, _))
                    if left_tuple == right_tuple =>
                {
                    pending.extend(left.iter().zip(right.iter()));
                }
                (Value::Mapping(left, _), Value::Mapping(right, _))
                    if matches!(op, Comparison::Equal | Comparison::NotEqual) =>
                {
                    pending.extend(
                        left.iter()
                            .filter_map(|(key, value)| right.get(key).map(|other| (value, other))),
                    );
                }
                _ => {}
            }
        }
        false
    }

    fn compare(
        &mut self,
        op: Comparison,
        left: &Value,
        right: &Value,
        site: Site,
    ) -> Result<Option<bool>, &'static str> {
        let result = match op {
            Comparison::Is | Comparison::IsNot => left.identity(right),
            _ if uncertain_value(left) || uncertain_value(right) => return Ok(None),
            Comparison::Equal | Comparison::NotEqual => self.equals(left, right, site),
            Comparison::In | Comparison::NotIn => self.contains(right, left, site)?,
            Comparison::Less
            | Comparison::LessEqual
            | Comparison::Greater
            | Comparison::GreaterEqual => self.order(left, right, site)?.map(|order| match op {
                Comparison::Less => order.is_lt(),
                Comparison::LessEqual => !order.is_gt(),
                Comparison::Greater => order.is_gt(),
                _ => !order.is_lt(),
            }),
        };
        Ok(match op {
            Comparison::NotEqual | Comparison::IsNot | Comparison::NotIn => {
                result.map(|value| !value)
            }
            _ => result,
        })
    }

    fn equals(&mut self, left: &Value, right: &Value, site: Site) -> Option<bool> {
        if !self.step(site) {
            return None;
        }
        self.with_evaluation(site, |engine| engine.equal_values(left, right, site))
            .flatten()
    }

    fn equal_values(&mut self, left: &Value, right: &Value, site: Site) -> Option<bool> {
        if uncertain_value(left) || uncertain_value(right) {
            return None;
        }
        if let (Some(left), Some(right)) = (left.integer(), right.integer()) {
            return Some(left == right);
        }
        match (left, right) {
            (Value::Sequence(left, left_tuple, _), Value::Sequence(right, right_tuple, _)) => {
                if left_tuple != right_tuple || left.len() != right.len() {
                    return Some(false);
                }
                let mut uncertain = false;
                for (left, right) in left.iter().zip(right.iter()) {
                    match self.equals(left, right, site) {
                        Some(false) => return Some(false),
                        None if self.limited => return None,
                        None => uncertain = true,
                        Some(true) => {}
                    }
                }
                (!uncertain).then_some(true)
            }
            (Value::Mapping(left, _), Value::Mapping(right, _)) => {
                if left.len() != right.len() {
                    return Some(false);
                }
                let mut uncertain = false;
                for ((left_key, left), (right_key, right)) in left.iter().zip(right.iter()) {
                    if left_key != right_key {
                        return Some(false);
                    }
                    match self.equals(left, right, site) {
                        Some(false) => return Some(false),
                        None if self.limited => return None,
                        None => uncertain = true,
                        Some(true) => {}
                    }
                }
                (!uncertain).then_some(true)
            }
            _ => Some(left.same(right)),
        }
    }

    fn order(
        &mut self,
        left: &Value,
        right: &Value,
        site: Site,
    ) -> Result<Option<Ordering>, &'static str> {
        self.with_evaluation(site, |engine| engine.order_values(left, right, site))
            .unwrap_or(Ok(None))
    }

    fn order_values(
        &mut self,
        left: &Value,
        right: &Value,
        site: Site,
    ) -> Result<Option<Ordering>, &'static str> {
        if uncertain_value(left) || uncertain_value(right) {
            return Ok(None);
        }
        if let (Some(left), Some(right)) = (left.integer(), right.integer()) {
            return Ok(Some(left.cmp(&right)));
        }
        match (left, right) {
            (Value::Constant(Constant::String(left)), Value::Constant(Constant::String(right))) => {
                Ok(Some(left.cmp(right)))
            }
            (Value::Sequence(left, left_tuple, _), Value::Sequence(right, right_tuple, _))
                if left_tuple == right_tuple =>
            {
                for (left, right) in left.iter().zip(right.iter()) {
                    match self.equals(left, right, site) {
                        Some(true) => {}
                        Some(false) => return self.order(left, right, site),
                        None => return Ok(None),
                    }
                }
                Ok(Some(left.len().cmp(&right.len())))
            }
            _ => Err("cannot order these values in the preview subset"),
        }
    }

    fn contains(
        &mut self,
        haystack: &Value,
        needle: &Value,
        site: Site,
    ) -> Result<Option<bool>, &'static str> {
        match (haystack, needle) {
            (
                Value::Constant(Constant::String(haystack)),
                Value::Constant(Constant::String(needle)),
            ) => Ok(Some(haystack.contains(needle))),
            (Value::Sequence(values, ..), _) => {
                let mut uncertain = false;
                for value in values.iter() {
                    match self.equals(value, needle, site) {
                        Some(true) => return Ok(Some(true)),
                        None if self.limited => return Ok(None),
                        None => uncertain = true,
                        Some(false) => {}
                    }
                }
                Ok((!uncertain).then_some(false))
            }
            (Value::Mapping(values, _), Value::Constant(Constant::String(needle))) => {
                Ok(Some(values.contains_key(needle)))
            }
            (Value::Mapping(..), Value::Sequence(..) | Value::Mapping(..)) => {
                Err("dictionary membership requires a supported hashable key")
            }
            (Value::Mapping(..), _) => Ok(Some(false)),
            _ => Err("membership check is outside the preview subset for these values"),
        }
    }
}

fn boolean(value: bool) -> Value {
    Value::Constant(Constant::Bool(value))
}

fn uncertain_value(value: &Value) -> bool {
    matches!(value, Value::Unknown | Value::UnknownBool)
}
