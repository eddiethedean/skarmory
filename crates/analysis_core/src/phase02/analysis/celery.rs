//! Resolve Celery result backends through ordered, source-owned settings.

use super::*;

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Setting {
    Builtin,
    Named(String),
    Unknown,
}

enum Operation {
    Create(Setting),
    Configure(Setting),
    ReplaceConfiguration,
    Load,
}

struct Event {
    scope: ScopeId,
    at: Location,
    conditional: bool,
    operation: Operation,
}

#[derive(Default)]
pub(super) struct Backends(BTreeMap<Object, Vec<Event>>);

impl Graph<'_> {
    pub(super) fn record_celery_backend_creation(
        &mut self,
        object: Object,
        call: CallArguments<'_>,
    ) {
        let kwarg = |key| {
            call.keywords
                .iter()
                .find_map(|(name, value)| (name == key).then_some(value))
        };
        // The third positional constructor argument is the backend. An
        // explicit result_backend setting takes precedence over that default.
        let backend = kwarg("result_backend")
            .or_else(|| kwarg("backend"))
            .or_else(|| call.positional.get(2));
        let setting = if kwarg("config_source").is_some()
            || kwarg("changes").is_some()
            || call.positional.len() > 11
        {
            Setting::Unknown
        } else {
            backend.map_or(Setting::Builtin, |value| {
                self.celery_setting(call.scope, value, call.before)
            })
        };
        self.record_celery_backend_event(object, call.scope, call.at, Operation::Create(setting));
    }

    pub(super) fn record_celery_backend_config(
        &mut self,
        object: Object,
        method: &str,
        call: CallArguments<'_>,
    ) {
        let setting = match method {
            "conf.update" => {
                if let Some((_, value)) = call
                    .keywords
                    .iter()
                    .find(|(key, _)| key == "result_backend")
                {
                    Some(self.celery_setting(call.scope, value, call.before))
                } else {
                    match call.positional {
                        [] => None,
                        [mapping] => {
                            configured_mapping(self.identity(call.scope, mapping, call.before, 0))
                        }
                        _ => Some(Setting::Unknown),
                    }
                }
            }
            "config_from_object" => Some(match (call.positional, call.keywords) {
                ([config], []) => {
                    let module = match self.identity(call.scope, config, call.before, 0) {
                        Value::Module(file) => Some(file),
                        Value::Text(name) => self.modules.get(name.as_str()).copied(),
                        _ => None,
                    };
                    if let Some(file) = module {
                        self.scopes[call.scope].imports.push(file);
                        match self.local(file, "result_backend", END) {
                            Some(binding) => setting_value(self.binding(binding, END, 0)),
                            None => Setting::Builtin,
                        }
                    } else {
                        Setting::Unknown
                    }
                }
                _ => Setting::Unknown,
            }),
            _ => unreachable!("only recognized Celery settings methods are recorded"),
        };
        if let Some(setting) = setting {
            self.record_celery_backend_event(
                object,
                call.scope,
                call.at,
                Operation::Configure(setting),
            );
        }
    }

    pub(super) fn record_celery_backend_write(
        &mut self,
        scope: ScopeId,
        at: Location,
        statement: &StatementKind,
    ) -> bool {
        let (receiver, member, value) = match statement {
            StatementKind::WriteAttribute {
                receiver,
                member,
                value,
            } => (receiver, member, value),
            StatementKind::WriteModuleEntry {
                mapping,
                module,
                value,
            } => (mapping, module, value),
            _ => return false,
        };
        let (object, setting) = match self.identity(scope, receiver, at, 0) {
            Value::CeleryConfig(object) => (
                object,
                (member == "result_backend").then(|| {
                    value.as_ref().map_or(Setting::Builtin, |value| {
                        self.celery_setting(scope, value, at)
                    })
                }),
            ),
            Value::Object(object, ObjectKind::CeleryApp) if member == "conf" => {
                self.record_celery_backend_event(
                    object,
                    scope,
                    at,
                    Operation::ReplaceConfiguration,
                );
                return true;
            }
            _ => return false,
        };
        if let Some(setting) = setting {
            self.record_celery_backend_event(object, scope, at, Operation::Configure(setting));
        }
        true
    }

    pub(super) fn record_celery_backend_read(
        &mut self,
        object: Object,
        scope: ScopeId,
        at: Location,
    ) {
        self.record_celery_backend_event(object, scope, at, Operation::Load);
    }

    fn celery_setting(&mut self, scope: ScopeId, value: &Expression, at: Location) -> Setting {
        if matches!(value.kind, ExpressionKind::None) {
            return Setting::Builtin;
        }
        setting_value(self.identity(scope, value, at, 0))
    }

    fn record_celery_backend_event(
        &mut self,
        object: Object,
        scope: ScopeId,
        at: Location,
        operation: Operation,
    ) {
        self.celery_backends
            .0
            .entry(object)
            .or_default()
            .push(Event {
                scope,
                at,
                conditional: self.branch_depth != 0,
                operation,
            });
    }

    pub(super) fn collect_celery_backends(&mut self) {
        for events in std::mem::take(&mut self.celery_backends.0).into_values() {
            let Some(creation) = events
                .iter()
                .find(|event| matches!(event.operation, Operation::Create(_)))
            else {
                continue;
            };
            let scope = creation.scope;
            let conditional_creation = creation.conditional;
            let mut ordered = Vec::new();
            let mut external = false;
            for event in events {
                if event.scope == scope {
                    ordered.push(event);
                } else {
                    external = true;
                    self.scope_issue(
                        event.scope,
                        event.at,
                        "unresolved_registration",
                        "Celery backend settings cross an unresolved execution boundary",
                    );
                    if let Operation::Configure(setting) = &event.operation {
                        self.retain_celery_backend(event.scope, event.at, setting, false);
                    }
                }
            }
            ordered.sort_by_key(|event| position(event.at));
            let mut current = BTreeSet::new();
            let mut possible_load = external || conditional_creation;
            let mut opaque_config = false;
            let mut last = None;
            for event in ordered {
                last = Some(event.at);
                match event.operation {
                    Operation::Create(setting) | Operation::Configure(setting) => {
                        if !event.conditional && !opaque_config {
                            current.clear();
                        }
                        current.insert(setting);
                    }
                    Operation::ReplaceConfiguration => {
                        // Existing aliases can still point to the old settings
                        // object when the app's conf reference is replaced.
                        opaque_config = true;
                        current.insert(Setting::Unknown);
                    }
                    Operation::Load => {
                        self.retain_celery_backends(
                            scope,
                            event.at,
                            &current,
                            possible_load || event.conditional,
                        );
                        if !event.conditional {
                            last = None;
                            break;
                        }
                        possible_load = true;
                    }
                }
            }
            if let Some(at) = last {
                self.retain_celery_backends(scope, at, &current, possible_load);
            }
        }
    }

    fn retain_celery_backends(
        &mut self,
        scope: ScopeId,
        at: Location,
        candidates: &BTreeSet<Setting>,
        possible: bool,
    ) {
        for setting in candidates {
            self.retain_celery_backend(scope, at, setting, !possible && candidates.len() == 1);
        }
    }

    fn retain_celery_backend(
        &mut self,
        scope: ScopeId,
        at: Location,
        setting: &Setting,
        certain: bool,
    ) {
        match setting {
            Setting::Builtin => return,
            Setting::Named(name) => {
                let parts = name.rsplit_once(':').or_else(|| name.rsplit_once('.'));
                if let Some((module, member)) = parts
                    && let Some(&file) = self.modules.get(module)
                    && let Value::Callable(invocation) = self.module_member_at(
                        module,
                        member,
                        END,
                        (scope, at),
                        ModuleMemberAccess::FromImport,
                        0,
                    )
                    && !self.is_coroutine(invocation.target)
                {
                    if file != self.scopes[scope].file {
                        self.scopes[scope].imports.push(file);
                    }
                    let certainty = if certain { "certain" } else { "possible" };
                    self.invoke(scope, invocation, "celery_backend", certainty, at);
                    if self.is_class(invocation.target) {
                        self.constructor(scope, invocation.target, "celery_backend", certainty, at);
                    }
                    return;
                }
            }
            Setting::Unknown => {}
        }
        self.scope_issue(
            scope,
            at,
            "unresolved_registration",
            "Celery result backend cannot be resolved to a bounded provider",
        );
    }
}

fn configured_mapping(value: Value) -> Option<Setting> {
    let Value::Mapping(items) = value else {
        return Some(Setting::Unknown);
    };
    for [key, value] in items.as_chunks::<2>().0.iter().rev() {
        match key {
            Value::Text(key) if key == "result_backend" => {
                return Some(setting_value(value.clone()));
            }
            Value::Text(_) => {}
            _ => return Some(Setting::Unknown),
        }
    }
    None
}

fn setting_value(value: Value) -> Setting {
    match value {
        Value::Text(name) => match name
            .split_once("://")
            .map_or(name.as_str(), |(scheme, _)| scheme)
        {
            "rpc" | "redis" | "rediss" | "cache" | "db" | "disabled" => Setting::Builtin,
            _ => Setting::Named(name),
        },
        _ => Setting::Unknown,
    }
}
