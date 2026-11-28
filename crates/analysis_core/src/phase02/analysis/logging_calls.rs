//! Default stdlib logging in a closed, selected Python project.

use super::*;

#[derive(Default)]
pub(super) struct Calls {
    enabled: bool,
    operations: Vec<(ScopeId, Location)>,
    open_scopes: BTreeSet<ScopeId>,
}

impl Graph<'_> {
    pub(super) fn index_logging_calls(&mut self) {
        self.logging_calls.enabled = self.bounded_deprecation
            && self.terminal_proofs
            && self.symbols.iter().any(|symbol| {
                matches!(symbol.binding, Binding::Import(import)
                    if import.level == 0 && import.module == "logging")
            });
    }

    pub(super) fn bounded_logger_name(
        &self,
        scope: ScopeId,
        at: Location,
        arguments: &[Expression],
        keywords: &[(String, Expression)],
    ) -> bool {
        self.logging_calls.enabled
            && matches!(registration_arguments(arguments, keywords, ["name"]), Some([Some(name)])
                if matches!(&name.kind, ExpressionKind::Text(_))
                    || matches!(&name.kind, ExpressionKind::Name(name) if name == "__name__"
                        && matches!(self.lexical_binding(scope, name, at, true), LexicalBinding::Missing)
                        && !self.has_module_binding_write(self.scopes[scope].file, name)))
    }

    pub(super) fn record_logging_call(&mut self, callee: &Value, call: CallArguments<'_>) -> bool {
        if !self.logging_calls.enabled {
            return false;
        }
        let operation = match callee {
            Value::Provider(name) if name == "logging.getLogger" => {
                self.bounded_logger_name(call.scope, call.before, call.positional, call.keywords)
            }
            Value::StdlibLoggerInfo => literal_message(call),
            Value::Provider(name) if name == "logging.info" => literal_message(call),
            _ => false,
        };
        if operation {
            self.logging_calls.operations.push((call.scope, call.at));
        } else if !self.preserves_default_logging(callee, call) {
            self.logging_calls.open_scopes.insert(call.scope);
        }
        operation
    }

    fn preserves_default_logging(&mut self, callee: &Value, call: CallArguments<'_>) -> bool {
        let print = matches!(callee, Value::Provider(name) if name == "builtins.print" && call.keywords.is_empty());
        if !print && !matches!(callee, Value::Callable(_)) {
            return false;
        }
        // Argument evaluation is handled by the ordinary walk. Here, reject
        // calls that can hand control of logging state to another operation.
        let reads = std::mem::take(&mut self.read);
        let preserves = call
            .positional
            .iter()
            .chain(call.keywords.iter().map(|(_, value)| value))
            .all(|value| {
                let value = self.probe_identity(call.scope, value, call.before);
                if print {
                    value.is_builtin_scalar() || matches!(value, Value::Integer(_))
                } else {
                    !can_change_logging(value)
                }
            });
        self.read = reads;
        preserves
    }

    pub(super) fn report_stdlib_logging_calls(&mut self, reach: &Reachability) {
        if self.logging_calls.operations.is_empty() {
            return;
        }
        let closed = self.logging_calls.open_scopes.is_disjoint(&reach.scopes)
            && !self
                .scope_issues
                .iter()
                .any(|(scope, _)| reach.scopes.contains(scope))
            && self.issues.is_empty()
            && self.modules.keys().all(|module| {
                let root = module.split('.').next().unwrap_or(module);
                !logging_config::LOGGING_IMPORT_NAMES
                    .split_ascii_whitespace()
                    .any(|name| name == root)
                    && root != "os"
            })
            && self.symbols.iter().all(|symbol| {
                let Binding::Import(import) = symbol.binding else {
                    return true;
                };
                !reach.scopes.contains(&symbol.scope)
                    || (import.level == 0
                        && import.module == "logging"
                        && matches!(import.member.as_deref(), None | Some("getLogger" | "info")))
                    || self
                        .import_module(self.scopes[symbol.scope].file, import)
                        .is_some_and(|module| self.modules.contains_key(module.as_str()))
            });
        if !closed {
            self.issues.extend(
                self.logging_calls
                    .operations
                    .iter()
                    .filter(|(scope, _)| reach.scopes.contains(scope))
                    .map(|&(scope, at)| {
                        Issue::new(
                            "unresolved_call",
                            &self.files[self.scopes[scope].file].path,
                            "logging call may depend on a configured or replaced provider",
                        )
                        .at(at)
                    }),
            );
        }
    }
}

fn can_change_logging(value: Value) -> bool {
    match value {
        Value::StdlibLogger | Value::StdlibLoggerInfo => true,
        Value::Provider(name) => name == "logging" || name.starts_with("logging."),
        Value::Alternatives(values) | Value::Sequence(values) | Value::Mapping(values) => {
            values.into_iter().any(can_change_logging)
        }
        _ => false,
    }
}

fn literal_message(call: CallArguments<'_>) -> bool {
    call.keywords.is_empty()
        && matches!(call.positional, [message] if matches!(message.kind, ExpressionKind::Text(_)))
}
