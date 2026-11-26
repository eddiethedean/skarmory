use super::*;

struct BoundCapture<'a> {
    member: &'a str,
    capture: Location,
    replacement: Location,
}

type DiagnosticSite = ([usize; 2], &'static str);

pub(in crate::phase02::analysis) struct DjangoCommandDispatch {
    launcher: ScopeId,
    hook: SymbolId,
    sites: Option<Vec<DiagnosticSite>>,
}

pub(super) struct CommandOptionReplay {
    parser: SymbolId,
    handle: SymbolId,
    callback: SymbolId,
    parser_sites: Vec<DiagnosticSite>,
    handle_sites: Vec<DiagnosticSite>,
}

impl Graph<'_> {
    fn django_bound_capture(&mut self, scope: ScopeId) -> Option<BoundCapture<'_>> {
        let hook = self.scopes[scope].owner?;
        let Binding::Definition(definition, _) = self.symbols[hook].binding else {
            return None;
        };
        if definition.name.text != "handle" || !self.bounded_framework_function(hook) {
            return None;
        }
        let receiver = self.receiver_parameter(hook)?;
        let class = *self.instance_receivers.get(&receiver)?;
        let [first, second, call] = body_without_docstring(definition) else {
            return None;
        };
        let (capture, replacement) = if matches!(first.kind, StatementKind::Assign { .. }) {
            (first, second)
        } else {
            (second, first)
        };
        let StatementKind::Assign {
            names,
            value:
                Some(Expression {
                    kind: ExpressionKind::Member(instance, member),
                    ..
                }),
            annotation: None,
        } = &capture.kind
        else {
            return None;
        };
        let [alias] = names.as_slice() else {
            return None;
        };
        let StatementKind::WriteAttribute {
            receiver: assigned,
            member: slot,
            value:
                Some(Expression {
                    kind: ExpressionKind::ConstantLambda,
                    ..
                }),
        } = &replacement.kind
        else {
            return None;
        };
        let StatementKind::Evaluate(Expression {
            kind:
                ExpressionKind::Call {
                    function,
                    arguments,
                    keywords,
                },
            ..
        }) = &call.kind
        else {
            return None;
        };
        if alias.text == self.symbols[receiver].name.text
            || self.parameter_binding(scope, instance, capture.at) != Some(receiver)
            || self.parameter_binding(scope, assigned, replacement.at) != Some(receiver)
            || member != slot
            || member.starts_with("__")
            || arguments.len() != 1
            || !keywords.is_empty()
            || !matches!(&arguments[0].kind, ExpressionKind::Name(_))
            || !matches!(&function.kind, ExpressionKind::Name(name) if *name == alias.text)
            || !self.is_django_command_class(class)
        {
            return None;
        }
        let file = self.scopes[scope].file;
        if self.class_lineage(class).0.into_iter().any(|body| {
            self.scopes[body].owner.is_none_or(|owner| {
                self.has_unmodeled_class_write(owner)
                    || !self.class_discovery_writes(owner, file).is_empty()
            })
        }) || [
            "__new__",
            "__init__",
            "__getattribute__",
            "__getattr__",
            "__setattr__",
            "__slots__",
            "__dict__",
        ]
        .into_iter()
        .any(|name| self.django_class_value(class, name, scope).is_some())
        {
            return None;
        }
        let Value::Callable(method) = self.django_class_value(class, member, scope)? else {
            return None;
        };
        if method.verified || !self.bounded_framework_function(method.target) {
            return None;
        }
        Some(BoundCapture {
            member,
            capture: capture.at,
            replacement: replacement.at,
        })
    }

    pub(in crate::phase02::analysis) fn django_command_captured_member(
        &mut self,
        scope: ScopeId,
        member: &str,
        before: Location,
    ) -> Option<Value> {
        let capture = self.django_bound_capture(scope)?;
        (capture.member == member
            && position(capture.capture) == position(before)
            && position(capture.replacement) < position(before))
        .then_some(Value::ConstantFunction)
    }

    pub(in crate::phase02::analysis) fn modeled_django_command_instance_write(
        &mut self,
        scope: ScopeId,
        at: Location,
    ) -> bool {
        self.django_bound_capture(scope)
            .is_some_and(|capture| position(capture.replacement) == position(at))
    }

    pub(super) fn django_command_option_replay(
        &mut self,
        class: SymbolId,
        commands: &DjangoCommands,
    ) -> Option<CommandOptionReplay> {
        let DjangoCommands::Selected {
            literal_args: Some(argv),
            ..
        } = commands
        else {
            return None;
        };
        let scope = self.symbols[class].scope;
        let file = self.scopes[scope].file;
        let Binding::Definition(definition, _) = self.symbols[class].binding else {
            return None;
        };
        let [base] = definition.bases.as_slice() else {
            return None;
        };
        if definition.has_class_keywords()
            || !definition.decorators.is_empty()
            || self.has_unmodeled_class_write(class)
            || !self.class_discovery_writes(class, file).is_empty()
            || !matches!(self.probe_identity(scope, base, definition.name.at),
                Value::Provider(provider) if provider == "django.core.management.base.BaseCommand")
        {
            return None;
        }
        // These are the stock parser and dispatch stages. An override of
        // another class member could change whether Django reaches them.
        if !body_without_docstring(definition)
            .iter()
            .all(|statement| match &statement.kind {
                StatementKind::Define(method) => {
                    matches!(method.name.text.as_str(), "add_arguments" | "handle")
                }
                StatementKind::Assign {
                    names,
                    value:
                        Some(Expression {
                            kind: ExpressionKind::Text(_),
                            ..
                        }),
                    annotation: None,
                } => matches!(names.as_slice(), [name] if name.text == "help"),
                _ => false,
            })
        {
            return None;
        }
        let Value::Callable(parser) = self.django_class_value(class, "add_arguments", scope)?
        else {
            return None;
        };
        let Value::Callable(handle) = self.django_class_value(class, "handle", scope)? else {
            return None;
        };
        if parser.verified
            || handle.verified
            || !self.bounded_framework_function(parser.target)
            || !self.bounded_framework_function(handle.target)
        {
            return None;
        }
        let Binding::Definition(parser_definition, _) = self.symbols[parser.target].binding else {
            return None;
        };
        let (aliases, parser_sites) = declared_options(parser_definition)?;
        let present = supplied_options(argv, &aliases)?;
        let Binding::Definition(handle_definition, handle_scope) =
            self.symbols[handle.target].binding
        else {
            return None;
        };
        let (call, options, mut handle_sites) = reloader_option_call(handle_definition)?;
        let ExpressionKind::Call {
            function,
            arguments,
            ..
        } = &call.kind
        else {
            return None;
        };
        if !matches!(self.probe_identity(handle_scope, function, call.at),
            Value::Provider(provider) if provider == "django.utils.autoreload.run_with_reloader")
        {
            return None;
        }
        let (callback, forwarded) = arguments.split_first()?;
        for argument in forwarded {
            let ExpressionKind::Subscript { value, index } = &argument.kind else {
                return None;
            };
            if !matches!(&value.kind, ExpressionKind::Name(name) if name == options)
                || !matches!(&index.kind, ExpressionKind::Text(key) if present.contains(key.as_str()))
            {
                return None;
            }
            handle_sites.push((position(argument.at), "unresolved_subscript"));
        }
        let Value::Callable(callback) = self.probe_identity(handle_scope, callback, call.at) else {
            return None;
        };
        let Binding::Definition(callback_definition, _) = self.symbols[callback.target].binding
        else {
            return None;
        };
        if callback.verified
            || !matches!(callback.receiver, Receiver::Ordinary)
            || callback.implicit_descriptor_receiver
            || !self.bounded_framework_function(callback.target)
            || !callback_definition.simple_positional_parameters
            || callback_definition.required_positional_parameter_count > forwarded.len()
            || callback_definition.positional_parameter_count < forwarded.len()
        {
            return None;
        }
        Some(CommandOptionReplay {
            parser: parser.target,
            handle: handle.target,
            callback: callback.target,
            parser_sites,
            handle_sites,
        })
    }

    pub(super) fn record_django_command_dispatch(
        &mut self,
        launcher: ScopeId,
        object: Object,
        name: &str,
        hook: SymbolId,
        replay: Option<&CommandOptionReplay>,
    ) {
        let sites = replay.and_then(|replay| match name {
            "add_arguments" if replay.parser == hook => Some(replay.parser_sites.clone()),
            "handle" if replay.handle == hook => {
                self.scopes[launcher]
                    .routes
                    .push((object, replay.callback, "django_autoreload"));
                Some(replay.handle_sites.clone())
            }
            _ => None,
        });
        self.django_command_dispatches.push(DjangoCommandDispatch {
            launcher,
            hook,
            sites,
        });
    }

    pub(in crate::phase02::analysis) fn remove_proven_django_command_option_diagnostics(
        &mut self,
        reach: &Reachability,
    ) {
        let mut open = BTreeSet::new();
        let mut proven = BTreeMap::new();
        for dispatch in &self.django_command_dispatches {
            if !reach.scopes.contains(&dispatch.launcher) {
                continue;
            }
            if let Some(sites) = &dispatch.sites {
                proven.insert(dispatch.hook, sites);
            } else {
                open.insert(dispatch.hook);
            }
        }
        let mut covered = BTreeMap::new();
        for (hook, sites) in proven {
            if open.contains(&hook)
                || reach
                    .evidence
                    .range((hook, "", "")..)
                    .take_while(|&&(target, _, _)| target == hook)
                    .any(|&(_, kind, _)| !matches!(kind, "django_command" | "type_reference"))
            {
                continue;
            }
            if let Binding::Definition(_, scope) = self.symbols[hook].binding {
                covered.insert(scope, sites);
            }
        }
        self.scope_issues.retain(|(scope, issue)| {
            !covered.get(scope).is_some_and(|sites| {
                sites.iter().any(|(at, kind)| {
                    issue.line == Some(at[0]) && issue.column == Some(at[1]) && issue.kind == *kind
                })
            })
        });
    }
}

fn declared_options(
    definition: &Definition,
) -> Option<(BTreeMap<&str, String>, Vec<DiagnosticSite>)> {
    let [_, parser] = definition.parameters.as_slice() else {
        return None;
    };
    if !definition.simple_positional_parameters || !definition.defaults.is_empty() {
        return None;
    }
    let mut aliases: BTreeMap<&str, String> = BTreeMap::new();
    let mut destinations = BTreeSet::new();
    let mut sites = Vec::new();
    for statement in body_without_docstring(definition) {
        let StatementKind::Evaluate(call) = &statement.kind else {
            return None;
        };
        let ExpressionKind::Call {
            function,
            arguments,
            keywords,
        } = &call.kind
        else {
            return None;
        };
        if !matches!(&function.kind, ExpressionKind::Member(receiver, member)
            if member == "add_argument"
                && matches!(&receiver.kind, ExpressionKind::Name(name) if *name == parser.text))
            || !matches!(keywords.as_slice(), [(key, value)]
                if key == "required" && matches!(value.kind, ExpressionKind::Bool(true)))
        {
            return None;
        }
        let [short @ .., long] = arguments.as_slice() else {
            return None;
        };
        let ExpressionKind::Text(long) = &long.kind else {
            return None;
        };
        let name = long.strip_prefix("--")?;
        let destination = name.replace('-', "_");
        if name.is_empty()
            || !name
                .bytes()
                .all(|char| char.is_ascii_alphanumeric() || matches!(char, b'-' | b'_'))
            || matches!(
                destination.as_str(),
                "help"
                    | "version"
                    | "verbosity"
                    | "settings"
                    | "pythonpath"
                    | "traceback"
                    | "no_color"
                    | "force_color"
                    | "skip_checks"
                    | "args"
                    | "stdout"
                    | "stderr"
            )
        {
            return None;
        }
        if !destinations.insert(destination.clone())
            || aliases.insert(long, destination.clone()).is_some()
        {
            return None;
        }
        match short {
            [] => {}
            [
                Expression {
                    kind: ExpressionKind::Text(short),
                    ..
                },
            ] if matches!(short.as_bytes(), [b'-', key] if key.is_ascii_alphabetic() && !matches!(key, b'h' | b'v')) => {
                if aliases.insert(short, destination).is_some() {
                    return None;
                }
            }
            _ => return None,
        }
        sites.push((position(call.at), "unresolved_call"));
    }
    (!aliases.is_empty()).then_some((aliases, sites))
}

fn supplied_options<'a>(
    argv: &[String],
    aliases: &'a BTreeMap<&str, String>,
) -> Option<BTreeSet<&'a str>> {
    let mut supplied = BTreeSet::new();
    let mut arguments = argv.iter();
    while let Some(argument) = arguments.next() {
        let (name, value) = match argument.split_once('=') {
            Some((name, value)) => (name, value),
            None => (argument.as_str(), arguments.next()?.as_str()),
        };
        if value.starts_with('-') && !argument.contains('=') {
            return None;
        }
        if name == "--settings" {
            if value.is_empty() {
                return None;
            }
            continue;
        }
        if !supplied.insert(aliases.get(name)?.as_str()) {
            return None;
        }
    }
    aliases
        .values()
        .all(|value| supplied.contains(value.as_str()))
        .then_some(supplied)
}

fn reloader_option_call(
    definition: &Definition,
) -> Option<(&Expression, &str, Vec<DiagnosticSite>)> {
    if definition.positional_parameter_count != 1
        || !definition.has_variadic_keyword_parameter
        || definition.parameters.len()
            != 2 + usize::from(definition.has_variadic_positional_parameter)
        || !definition.defaults.is_empty()
    {
        return None;
    }
    let [
        Statement {
            kind: StatementKind::Evaluate(call),
            ..
        },
    ] = body_without_docstring(definition)
    else {
        return None;
    };
    if !matches!(&call.kind, ExpressionKind::Call { arguments, keywords, .. }
        if arguments.len() > 1 && keywords.is_empty())
    {
        return None;
    }
    let options = definition.parameters.last()?.text.as_str();
    Some((
        call,
        options,
        vec![(position(call.at), "unresolved_registration")],
    ))
}
