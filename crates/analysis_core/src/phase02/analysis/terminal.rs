//! Closed terminal paths whose expressions cannot execute a registration.

use super::*;

impl Graph<'_> {
    pub(super) fn prove_terminal_module_failure(
        &mut self,
        scope: ScopeId,
        statement: &Statement,
    ) -> bool {
        if !self.terminal_proofs || scope != self.scopes[scope].file || self.branch_depth != 0 {
            return false;
        }
        if self.is_pytest_file(scope)
            || self.files[scope].path.rsplit('/').next() == Some("conftest.py")
        {
            return false;
        }
        if self.terminal_failed_flask_provider(scope, statement)
            || self.terminal_fastapi_shadowed_import(scope, statement)
            || self.terminal_django_url_assignment(scope, statement)
            || self.terminal_django_url_initializer(scope, statement)
            || self.terminal_unbound_flask_initializer(scope, statement)
            || self.terminal_flask_factory_initializer(scope, statement)
            || self.terminal_flask_global_initializer(scope, statement)
            || self.phase03
                && (self.terminal_missing_django_reloader(scope, statement)
                    || self.terminal_django_auth_initializer(scope, statement)
                    || self.terminal_django_backend_replacement(scope, statement)
                    || self.terminal_django_runpython(scope, statement)
                    || self.terminal_celery_router_failure(scope, statement)
                    || self.terminal_logging_config_failure(scope, statement))
        {
            return true;
        }
        let Some((last, earlier)) = self.scopes[scope].body.split_last() else {
            return false;
        };
        if !std::ptr::eq(last, statement) {
            return false;
        }
        let before = statement.at;
        match &statement.kind {
            StatementKind::Define(definition) => {
                self.terminal_flask_decorator(scope, definition, before, earlier)
            }
            StatementKind::Evaluate(Expression {
                kind: ExpressionKind::Call { function, .. },
                ..
            }) => earlier.split_last().is_some_and(|(previous, prefix)| {
                plain_module_prefix(prefix)
                    && self.terminal_missing_plain_member(scope, function, before, previous)
            }),
            StatementKind::Evaluate(Expression {
                kind: ExpressionKind::KeywordMappingCall { function, mapping },
                ..
            }) if self.unbound_module_name(scope, mapping, before)
                && matches!(self.probe_identity(scope, function, before),
                    Value::Method(_, ObjectKind::CeleryApp, method) if method == "conf.update") =>
            {
                // The callable expression is evaluated before its arguments.
                self.walk(scope, function, before);
                true
            }
            StatementKind::WriteAttribute {
                receiver,
                member,
                value: Some(value),
            } if member == "conf"
                && self.unbound_module_name(scope, value, before)
                && self.is_celery_assignment_receiver(scope, receiver, before) =>
            {
                // An assignment evaluates its right hand side before reading
                // the assignment receiver or changing the settings object.
                true
            }
            _ => false,
        }
    }

    fn terminal_missing_django_reloader(&mut self, scope: ScopeId, statement: &Statement) -> bool {
        let access = match &statement.kind {
            StatementKind::Assign {
                value: Some(value),
                annotation: None,
                ..
            } => value,
            StatementKind::Evaluate(Expression {
                kind: ExpressionKind::Call { function, .. },
                ..
            }) => function,
            _ => return false,
        };
        let ExpressionKind::Member(receiver, member) = &access.kind else {
            return false;
        };
        if member != "run_with_reloader" {
            return false;
        }
        let at = statement.at;
        let reads = std::mem::take(&mut self.read);
        let missing = match self.probe_identity(scope, receiver, at) {
            Value::Provider(provider) if provider == "django.utils.autoreload" => matches!(
                self.probe_identity(scope, access, at),
                Value::MissingFrameworkMember
            ),
            // A source-owned Django lookalike can export a literal or a
            // fresh built-in object. No exact built-in scalar has this slot.
            value => value.is_builtin_scalar() || matches!(value, Value::None | Value::Integer(_)),
        };
        self.read = reads;
        if missing {
            // The receiver lookup runs, but fails before binding the alias
            // or evaluating any arguments of an attempted call.
            self.walk(scope, receiver, at);
        }
        missing
    }

    fn is_celery_assignment_receiver(
        &mut self,
        scope: ScopeId,
        receiver: &Expression,
        before: Location,
    ) -> bool {
        // This lookup proves the shape of the unreachable left hand side.
        // Its variables are not read by the Python statement.
        let read = std::mem::take(&mut self.read);
        let receiver = self.probe_identity(scope, receiver, before);
        self.read = read;
        matches!(receiver, Value::Object(_, ObjectKind::CeleryApp))
    }

    fn terminal_missing_plain_member(
        &mut self,
        scope: ScopeId,
        mut function: &Expression,
        before: Location,
        previous: &Statement,
    ) -> bool {
        // Resolve the first receiver lookup, even when the final callee is
        // deeper in the chain. A failed lookup also prevents argument effects.
        while let ExpressionKind::Member(receiver, member) = &function.kind {
            let ExpressionKind::Name(receiver_name) = &receiver.kind else {
                function = receiver;
                continue;
            };
            let StatementKind::Assign {
                names,
                value: Some(value),
                annotation: None,
            } = &previous.kind
            else {
                return false;
            };
            let [name] = names.as_slice() else {
                return false;
            };
            if &name.text != receiver_name || member.starts_with("__") {
                return false;
            }
            let ExpressionKind::Call {
                function,
                arguments,
                keywords,
            } = &value.kind
            else {
                return false;
            };
            if !arguments.is_empty() || !keywords.is_empty() {
                return false;
            }
            let Value::Callable(class) = self.probe_identity(scope, function, previous.at) else {
                return false;
            };
            if self.symbols[class.target].scope != scope
                || !matches!(self.symbols[class.target].binding, Binding::Definition(definition, _)
                    if plain_class(definition))
            {
                return false;
            }
            // No user constructor, descriptor, metaclass or intervening
            // statement can have installed a member on this fresh instance.
            self.walk(scope, receiver, before);
            return true;
        }
        false
    }

    pub(super) fn unbound_module_name(
        &self,
        scope: ScopeId,
        expression: &Expression,
        before: Location,
    ) -> bool {
        let ExpressionKind::Name(name) = &expression.kind else {
            return false;
        };
        // Restrict the proof to ASCII identifiers with an alphabetical start.
        // Allow CPython builtins in that spelling space, including site names.
        const BUILTINS: &str = "abs aiter all anext any ascii bin bool breakpoint bytearray bytes \
            callable chr classmethod compile complex copyright credits delattr dict dir divmod \
            enumerate eval exec exit filter float format frozenset getattr globals hasattr hash \
            help hex id input int isinstance issubclass iter len license list locals map max \
            memoryview min next object oct open ord pow print property quit range repr reversed \
            round set setattr slice sorted staticmethod str sum super tuple type vars zip \
            ArithmeticError AssertionError AttributeError BaseException BaseExceptionGroup \
            BlockingIOError BrokenPipeError BufferError BytesWarning ChildProcessError \
            ConnectionAbortedError ConnectionError ConnectionRefusedError ConnectionResetError \
            DeprecationWarning EOFError Ellipsis EncodingWarning EnvironmentError Exception \
            ExceptionGroup False FileExistsError FileNotFoundError FloatingPointError FutureWarning \
            GeneratorExit IOError ImportError ImportWarning IndentationError IndexError \
            InterruptedError IsADirectoryError KeyError KeyboardInterrupt LookupError MemoryError \
            ModuleNotFoundError NameError None NotADirectoryError NotImplemented \
            NotImplementedError OSError OverflowError PendingDeprecationWarning PermissionError \
            ProcessLookupError PythonFinalizationError RecursionError ReferenceError ResourceWarning \
            RuntimeError RuntimeWarning StopAsyncIteration StopIteration SyntaxError SyntaxWarning \
            SystemError SystemExit TabError TimeoutError True TypeError UnboundLocalError \
            UnicodeDecodeError UnicodeEncodeError UnicodeError UnicodeTranslateError UnicodeWarning \
            UserWarning ValueError Warning ZeroDivisionError";
        name.as_bytes().first().is_some_and(u8::is_ascii_alphabetic)
            && name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
            && !self.scopes[scope].open_wildcard
            && !BUILTINS
                .split_ascii_whitespace()
                .any(|builtin| builtin == name)
            && matches!(
                self.lexical_binding(scope, name, before, true),
                LexicalBinding::Missing
            )
    }

    pub(super) fn report_failed_terminal_module_consumers(&mut self) {
        if self.failed_terminal_modules.is_empty() {
            return;
        }
        let consumers: Vec<_> = self
            .scopes
            .iter()
            .enumerate()
            .flat_map(|(scope, row)| {
                row.imports
                    .iter()
                    .copied()
                    .filter(|file| self.failed_terminal_modules.contains(file))
                    .map(move |file| (scope, file))
            })
            .collect();
        for (scope, module) in consumers {
            self.scope_issue(
                scope,
                Location { line: 1, column: 1 },
                "unresolved_import",
                format!(
                    "selected module {:?} cannot finish initialization",
                    self.files[module].module
                ),
            );
        }
    }
}

pub(super) fn plain_class(definition: &Definition) -> bool {
    definition.kind == DefinitionKind::Class
        && definition.bases.is_empty()
        && definition.decorators.is_empty()
        && definition.table_keyword.is_none()
        && body_without_docstring(definition).is_empty()
}

fn plain_module_prefix(prefix: &[Statement]) -> bool {
    let mut defined_class = false;
    for statement in prefix {
        match &statement.kind {
            StatementKind::Import(_) if !defined_class => {}
            StatementKind::Evaluate(Expression {
                kind: ExpressionKind::Text(_),
                ..
            }) => {}
            StatementKind::Define(definition) if plain_class(definition) => {
                defined_class = true;
            }
            StatementKind::Assign {
                value:
                    Some(Expression {
                        kind: ExpressionKind::Name(_),
                        ..
                    }),
                annotation: None,
                ..
            } => {}
            _ => return false,
        }
    }
    defined_class
}
