//! Recognize possible externally selected Gunicorn config declarations.

use std::path::Path;

use super::*;

pub(super) const DECLARATION: &str = "gunicorn_config_declaration";

const SETTINGS: &[&str] = &[
    "accesslog",
    "bind",
    "chdir",
    "control_socket_disable",
    "daemon",
    "errorlog",
    "forwarded_allow_ips",
    "graceful_timeout",
    "keepalive",
    "loglevel",
    "max_requests",
    "pidfile",
    "preload_app",
    "secure_scheme_headers",
    "threads",
    "timeout",
    "worker_class",
    "workers",
    "wsgi_app",
];

const HOOKS: &[(&str, RangeInclusive<usize>)] = &[
    ("on_starting", 1..=1),
    ("on_reload", 1..=1),
    ("when_ready", 1..=1),
    ("pre_fork", 2..=2),
    ("post_fork", 2..=2),
    ("post_worker_init", 1..=1),
    ("worker_int", 1..=1),
    ("worker_abort", 1..=1),
    ("pre_exec", 1..=1),
    ("pre_request", 2..=2),
    ("post_request", 2..=4),
    ("child_exit", 2..=2),
    ("worker_exit", 2..=2),
    ("nworkers_changed", 3..=3),
    ("on_exit", 1..=1),
    ("ssl_context", 2..=2),
];

impl Graph<'_> {
    pub(super) fn collect_gunicorn_config(&mut self) {
        for file in 0..self.files.len() {
            if !matches!(
                Path::new(&self.files[file].path)
                    .file_name()
                    .and_then(|name| name.to_str()),
                Some("gunicorn.conf.py" | "gunicorn_config.py" | "gunicorn_conf.py")
            ) {
                continue;
            }
            for name in SETTINGS {
                if let Some(binding) = self.gunicorn_config_binding(file, name)
                    && matches!(self.symbols[binding].binding, Binding::Variable(Some(_), _))
                {
                    self.edge(file, binding, DECLARATION, "possible");
                }
            }
            for (name, arities) in HOOKS {
                if let Some(binding) = self.gunicorn_config_binding(file, name)
                    && let Value::Callable(invocation) = self.binding_value(binding, 0)
                    && !invocation.verified
                    && matches!(invocation.receiver, Receiver::Ordinary)
                    && matches!(
                        self.symbols[invocation.target].binding,
                        Binding::Definition(definition, _)
                            if definition.kind == DefinitionKind::Function
                                && !definition.is_generator
                                && definition.decorators.is_empty()
                                && definition.simple_positional_parameters
                                && arities.contains(&definition.parameters.len())
                    )
                {
                    // The config can expose the hook to Gunicorn. The selected
                    // Python sources do not establish that its event fires.
                    self.edge(file, binding, DECLARATION, "possible");
                    if binding != invocation.target {
                        self.edge(file, invocation.target, DECLARATION, "possible");
                    }
                }
            }
        }
    }

    fn gunicorn_config_binding(&self, file: usize, name: &str) -> Option<SymbolId> {
        self.local(file, name, END)
            .filter(|&binding| !self.binding_depends_on_branch(binding, END))
    }
}
