mod python;

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde::Serialize;

use super::manifests::logical_lines;
use super::{Inputs, Reason, configured_strings, display, ini_entries, module_reference};
use crate::phase02::package_entrypoints::{SetupValue, setup_declarations};
use crate::phase02::{AnalyzedFile, config};
use crate::preview_source::Project;

const MAX_COMMAND_DEPTH: usize = 3;
const MAX_COMMAND_FILES: usize = 64;

#[derive(Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub(in crate::phase02) struct Evidence {
    distribution: String,
    kind: &'static str,
    file: String,
    line: usize,
    reference: String,
}

#[derive(Clone)]
struct Reference {
    kind: &'static str,
    file: String,
    line: usize,
    name: String,
}

impl Reference {
    fn evidence(&self, distribution: &str) -> Evidence {
        Evidence {
            distribution: distribution.into(),
            kind: self.kind,
            file: self.file.clone(),
            line: self.line,
            reference: self.name.clone(),
        }
    }

    fn uncertainty(&self, kind: &'static str, message: &str) -> Reason {
        Reason::new(kind, Path::new(&self.file), Some(self.line), message)
    }
}

#[derive(Default)]
pub(super) struct Usage {
    modules: BTreeMap<String, Vec<Reference>>,
    commands: BTreeMap<String, Vec<Reference>>,
    plugins: BTreeMap<String, Vec<Reference>>,
    ambiguous: BTreeMap<String, BTreeSet<String>>,
    pub(super) reasons: Vec<Reason>,
}

#[derive(Default)]
pub(super) struct Decision {
    pub(super) evidence: Vec<Evidence>,
    pub(super) reasons: Vec<Reason>,
}

impl Usage {
    pub(super) fn read(
        &mut self,
        project: &Project,
        files: &[AnalyzedFile],
        selection: &config::Selection,
        inputs: &mut Inputs,
    ) -> usize {
        let mut additional = Vec::new();
        let mut command_files = Vec::new();
        let root = project.base();
        for directory in project.verification_directories() {
            let label = project.label(directory);
            if selection.exclusion(&label).is_some() && !selection.may_include_children(&label) {
                continue;
            }
            let in_command_scope = directory
                .strip_prefix(root)
                .is_ok_and(|path| path.components().count() <= MAX_COMMAND_DEPTH);
            for path in inputs.list(root, directory) {
                let label = project.label(&path);
                if selection.exclusion(&label).is_some() {
                    continue;
                }
                if in_command_scope && is_command_file(&path) {
                    command_files.push(path);
                } else if path
                    .extension()
                    .is_some_and(|extension| extension == "pyi" || extension == "pyw")
                    && let Some(file) = supplemental_file(root, &path, label, inputs)
                {
                    additional.push(file);
                }
            }
        }
        let files: Vec<_> = files.iter().chain(&additional).collect();
        let locals: BTreeSet<_> = files.iter().map(|file| file.module.as_str()).collect();
        python::collect(self, root, &files, &locals);
        if let Some(setup) = files.iter().find(|file| file.path == "setup.py") {
            self.setup_py(&root.join("setup.py"), setup, &locals);
        }

        let manifest = root.join("pyproject.toml");
        if let Some(source) = inputs.read(root, &manifest, false) {
            self.pyproject(&manifest, &source, inputs);
        }
        for name in [
            "setup.cfg",
            "mypy.ini",
            ".mypy.ini",
            ".flake8",
            "tox.ini",
            "pytest.ini",
            ".pytest.ini",
        ] {
            let path = root.join(name);
            if selection.exclusion(name).is_none()
                && let Some(source) = inputs.read(root, &path, false)
            {
                self.ini(&path, &source);
            }
        }

        if command_files.len() > MAX_COMMAND_FILES {
            inputs.problem(
                "dependency_input_limit",
                root,
                None,
                "deployment command file budget exceeded",
            );
        }
        for file in command_files.into_iter().take(MAX_COMMAND_FILES) {
            if let Some(source) = inputs.read(root, &file, true) {
                self.command_file(&file, &source);
            }
        }
        self.modules
            .retain(|module, _| !python::is_local(&locals, module));
        files.len()
    }

    pub(super) fn resolve_providers(&mut self, names: impl Iterator<Item = impl AsRef<str>>) {
        let mut providers = BTreeMap::<String, Vec<String>>::new();
        for name in names {
            let name = name.as_ref();
            for module in import_paths(name) {
                providers.entry(module).or_default().push(name.to_owned());
            }
        }
        for (module, names) in providers {
            if names.len() > 1 && prefix_matches(&self.modules, &module).next().is_some() {
                for name in names {
                    self.ambiguous
                        .entry(name)
                        .or_default()
                        .insert(module.clone());
                }
            }
        }
    }

    pub(super) fn for_distribution(&self, name: &str) -> Decision {
        let mut decision = Decision::default();
        let ambiguous = self.ambiguous.get(name);
        for module in import_paths(name) {
            for reference in prefix_matches(&self.modules, &module) {
                let contested = ambiguous.is_some_and(|paths| {
                    paths
                        .iter()
                        .any(|path| includes_module(path, &reference.name))
                });
                if contested {
                    decision.reasons.push(reference.uncertainty(
                        "ambiguous_distribution_mapping",
                        "more than one declared distribution provides this import path",
                    ));
                } else {
                    decision.evidence.push(reference.evidence(name));
                }
            }

            for (parent, _) in module.match_indices('.') {
                if let Some(references) = self.modules.get(&module[..parent]) {
                    decision.reasons.extend(references.iter().map(|reference| {
                        reference.uncertainty(
                            "namespace_parent_only",
                            "only the parent namespace is referenced",
                        )
                    }));
                }
            }
        }
        for command in std::iter::once(name).chain(command_names(name).iter().copied()) {
            if let Some(references) = self.commands.get(command) {
                decision
                    .evidence
                    .extend(references.iter().map(|reference| reference.evidence(name)));
            }
        }
        if let Some(references) = self.plugins.get(name) {
            decision
                .evidence
                .extend(references.iter().map(|reference| reference.evidence(name)));
        }
        decision
    }

    fn module(&mut self, path: &Path, line: usize, text: &str, kind: &'static str) {
        if let Some(name) = module_reference(text) {
            Self::reference(&mut self.modules, name, path, line, kind);
        }
    }

    fn plugin(&mut self, path: &Path, line: usize, name: &str, kind: &'static str) {
        Self::reference(&mut self.plugins, name, path, line, kind);
    }

    fn reference(
        map: &mut BTreeMap<String, Vec<Reference>>,
        name: &str,
        path: &Path,
        line: usize,
        kind: &'static str,
    ) {
        map.entry(name.into()).or_default().push(Reference {
            kind,
            file: display(path),
            line,
            name: name.into(),
        });
    }

    fn pyproject(&mut self, path: &Path, source: &str, inputs: &mut Inputs) {
        let parsed: toml::Table = match toml::from_str(source) {
            Ok(value) => value,
            Err(error) => {
                inputs.problem("invalid_dependency_manifest", path, None, error);
                return;
            }
        };
        let lookup = |keys: &[&str]| {
            let (first, rest) = keys.split_first().unwrap();
            rest.iter()
                .try_fold(parsed.get(*first)?, |value, key| value.get(*key))
        };
        for keys in [
            &["project", "scripts"][..],
            &["project", "gui-scripts"],
            &["project", "entry-points"],
            &["tool", "poetry", "scripts"],
            &["tool", "poetry", "plugins"],
            &["tool", "mypy", "plugins"],
        ] {
            if let Some(value) = lookup(keys) {
                configured_strings(value, &mut |value| {
                    self.module(path, 0, value, "tool_configuration")
                });
            }
        }
        for keys in [&["tool", "pytest", "ini_options"][..], &["tool", "pytest"]] {
            if let Some(options) = lookup(keys).and_then(toml::Value::as_table) {
                for (key, value) in options {
                    self.pytest_option(path, 0, key);
                    if key == "addopts" {
                        configured_strings(value, &mut |value| {
                            if let Some(words) = shlex::split(value) {
                                self.pytest_args(path, 0, &words);
                            }
                        });
                    }
                }
            }
        }
    }

    fn setup_py(&mut self, path: &Path, file: &AnalyzedFile, locals: &BTreeSet<&str>) {
        let declarations = setup_declarations(
            &file.facts,
            locals.contains("setuptools"),
            locals.contains("distutils") || locals.contains("distutils.core"),
            true,
        );
        for call in declarations.calls {
            if let Some(value) = call.values.get("entry_points") {
                self.setup_entrypoints(path, call.line, value);
            }
        }
    }

    fn setup_entrypoints(&mut self, path: &Path, line: usize, value: &SetupValue<'_>) {
        match value {
            SetupValue::Mapping(values) => {
                for value in values.values() {
                    self.setup_entrypoints(path, line, value);
                }
            }
            SetupValue::Sequence(values) => {
                for value in values.iter() {
                    self.setup_entrypoints(path, line, value);
                }
            }
            SetupValue::Text(value, line) => {
                for text in value.lines() {
                    if let Some((_, target)) = text.split_once('=') {
                        self.module(path, *line, target, "package_entrypoint");
                    }
                }
            }
            _ => self.reasons.push(Reason::new(
                "unresolved_dependency_configuration",
                path,
                Some(line),
                "package entry points are dynamic",
            )),
        }
    }

    fn ini(&mut self, path: &Path, source: &str) {
        for (section, key, value, line, continuation) in ini_entries(source) {
            match (section, key) {
                ("options.entry_points", _) => {
                    if let Some((_, target)) = value.split_once('=') {
                        self.module(path, line, target, "package_entrypoint");
                    }
                }
                ("mypy", "plugins") => {
                    for value in value.split(|ch: char| ch == ',' || ch.is_whitespace()) {
                        self.module(path, line, value, "tool_configuration");
                    }
                }
                ("flake8", "docstring-convention") => {
                    self.plugin(path, line, "flake8-docstrings", "tool_configuration")
                }
                (
                    "flake8",
                    "select" | "extend-select" | "ignore" | "extend-ignore" | "per-file-ignores",
                ) => {
                    if value
                        .split(|ch: char| !ch.is_ascii_alphanumeric())
                        .any(|word| {
                            word.strip_prefix('D')
                                .is_some_and(|rest| rest.bytes().all(|byte| byte.is_ascii_digit()))
                        })
                    {
                        self.plugin(path, line, "flake8-docstrings", "tool_configuration");
                    }
                }
                ("pytest" | "tool:pytest", _) => {
                    if !continuation {
                        self.pytest_option(path, line, key);
                    }
                    if key == "addopts"
                        && let Some(words) = shlex::split(value)
                    {
                        self.pytest_args(path, line, &words);
                    }
                }
                (section, "commands" | "commands_pre" | "commands_post")
                    if section.starts_with("testenv") =>
                {
                    self.command(path, line, value);
                }
                _ => {}
            }
        }
    }

    fn pytest_option(&mut self, path: &Path, line: usize, name: &str) {
        let plugin = match name {
            "timeout" | "timeout_method" | "timeout_func_only" | "timeout_disable_debugger" => {
                "pytest-timeout"
            }
            "reruns" | "reruns_delay" | "only_rerun" | "rerun_except" => "pytest-rerunfailures",
            _ => return,
        };
        self.plugin(path, line, plugin, "pytest_configuration");
    }

    fn pytest_args(&mut self, path: &Path, line: usize, words: &[String]) {
        for (index, word) in words.iter().enumerate() {
            let key = word.split('=').next().unwrap_or(word);
            let plugin = match key {
                "--timeout" | "--timeout-method" => Some("pytest-timeout"),
                "--reruns" | "--reruns-delay" | "--only-rerun" | "--rerun-except" => {
                    Some("pytest-rerunfailures")
                }
                "-p" => words.get(index + 1).and_then(|name| match name.as_str() {
                    "timeout" | "pytest_timeout" => Some("pytest-timeout"),
                    "rerunfailures" | "pytest_rerunfailures" => Some("pytest-rerunfailures"),
                    "subtests" | "pytest_subtests" => Some("pytest-subtests"),
                    "pytest_click" => Some("pytest-click"),
                    _ => {
                        self.module(path, line, name, "pytest_configuration");
                        None
                    }
                }),
                _ => None,
            };
            if let Some(plugin) = plugin {
                self.plugin(path, line, plugin, "pytest_configuration");
            }
        }
    }

    fn command_file(&mut self, path: &Path, source: &str) {
        let name = file_name(path);
        let docker = dockerfile(name);
        let procfile = name.starts_with("Procfile");
        let make = name == "Makefile";
        if !docker && !procfile && !make {
            self.compose_commands(path, source);
            return;
        }
        for (line, raw) in logical_lines(source) {
            let text = raw.trim();
            if text.is_empty() || text.starts_with('#') {
                continue;
            }
            let command = if docker {
                text.split_once(char::is_whitespace)
                    .and_then(|(verb, rest)| {
                        matches!(
                            verb.to_ascii_uppercase().as_str(),
                            "RUN" | "CMD" | "ENTRYPOINT"
                        )
                        .then_some(rest.trim())
                    })
            } else if procfile {
                text.split_once(':').map(|(_, rest)| rest.trim())
            } else {
                raw.starts_with('\t')
                    .then_some(text.trim_start_matches(['@', '-', '+']))
            };
            if let Some(command) = command {
                self.command(path, line, command);
            }
        }
    }

    fn compose_commands(&mut self, path: &Path, source: &str) {
        let mut lines = source.lines().enumerate().peekable();
        while let Some((index, raw)) = lines.next() {
            let text = raw.trim();
            let indent = raw.len() - raw.trim_start().len();
            let Some((command, value)) = text.split_once(':') else {
                continue;
            };
            if !matches!(command, "entrypoint" | "command") {
                continue;
            }
            let line = index + 1;
            let value = value.trim();
            if let Some(flow) = value
                .strip_prefix('[')
                .and_then(|value| value.strip_suffix(']'))
            {
                let argv: Vec<_> = segments(flow, &[','])
                    .into_iter()
                    .map(yaml_scalar)
                    .collect();
                self.command_words(path, line, &argv, 0);
            } else if value.is_empty() || matches!(value, "|" | ">" | "|-" | ">-") {
                let mut values = Vec::new();
                while let Some((_, next)) = lines.peek() {
                    let item = next.trim();
                    let sequence = item.strip_prefix("- ");
                    if item.is_empty() || item.starts_with('#') {
                        lines.next();
                        continue;
                    }
                    if next.len() - next.trim_start().len() <= indent && sequence.is_none() {
                        break;
                    }
                    if value.is_empty() {
                        let Some(item) = sequence else { break };
                        values.push(yaml_scalar(item));
                    } else {
                        values.push(item.to_owned());
                    }
                    lines.next();
                }
                if value.is_empty() {
                    self.command_words(path, line, &values, 0);
                } else {
                    self.command(path, line, &values.join(" "));
                }
            } else if value != "null" {
                self.command(path, line, &yaml_scalar(value));
            }
        }
    }

    fn command(&mut self, path: &Path, line: usize, text: &str) {
        if text.starts_with('[')
            && let Ok(words) = serde_json::from_str::<Vec<String>>(text)
        {
            self.command_words(path, line, &words, 0);
            return;
        }
        for segment in segments(text, &['&', '|', ';']) {
            if let Some(words) = shlex::split(segment) {
                self.command_words(path, line, &words, 0);
            } else {
                self.reasons.push(Reason::new(
                    "unresolved_dependency_command",
                    path,
                    Some(line),
                    "command arguments could not be parsed",
                ));
            }
        }
    }

    fn command_words(&mut self, path: &Path, line: usize, words: &[String], depth: usize) {
        if depth >= 8 {
            self.reasons.push(Reason::new(
                "dependency_command_limit",
                path,
                Some(line),
                "command wrapper depth limit",
            ));
            return;
        }
        let Some(index) = words.iter().position(|word| {
            !word.contains('=')
                && !matches!(
                    word.as_str(),
                    "exec" | "env" | "command" | "then" | "do" | "if" | "!"
                )
        }) else {
            return;
        };
        let command = words[index].rsplit('/').next().unwrap_or_default();
        let args = &words[index + 1..];
        let argument = args.first().map(String::as_str);
        if matches!(
            command,
            "pip"
                | "pip3"
                | "pipx"
                | "uv"
                | "poetry"
                | "pdm"
                | "conda"
                | "mamba"
                | "apt-get"
                | "apt"
                | "apk"
                | "yum"
                | "dnf"
                | "npm"
                | "yarn"
                | "pnpm"
        ) && args
            .iter()
            .any(|word| matches!(word.as_str(), "install" | "add" | "sync"))
        {
            return;
        }
        if is_python_command(command)
            && let Some((module, module_args)) = python_module_args(args)
        {
            if module == "pip" && module_args.first().is_some_and(|verb| verb == "install") {
                return;
            }
            self.module(path, line, module, "module_command");
            if module == "pytest" {
                self.pytest_args(path, line, module_args);
            }
            return;
        }
        if matches!(command, "sh" | "bash" | "zsh") && argument == Some("-c") {
            if let Some(script) = args.get(1) {
                for segment in segments(script, &['&', '|', ';']) {
                    if let Some(words) = shlex::split(segment) {
                        self.command_words(path, line, &words, depth + 1);
                    }
                }
            }
            return;
        }
        if !command.is_empty() {
            Self::reference(&mut self.commands, command, path, line, "console_command");
        }
        if command == "pytest" || command == "py.test" {
            self.pytest_args(path, line, args);
        }
        if matches!(
            (command, argument),
            ("uv" | "poetry" | "pdm" | "pipx", Some("run"))
                | ("newrelic-admin", Some("run-program"))
        ) {
            self.command_words(path, line, &args[1..], depth + 1);
        }
    }
}

fn supplemental_file(
    root: &Path,
    path: &Path,
    label: String,
    inputs: &mut Inputs,
) -> Option<AnalyzedFile> {
    let source = inputs.read(root, path, true)?;
    let facts = match skarmory_python::phase02::extract(
        &source.replace("\r\n", "\n").replace('\r', "\n"),
    ) {
        Ok(facts) => facts,
        Err(error) => {
            inputs.problem(
                "unparsed_dependency_source",
                path,
                Some(error.location.line),
                error.message,
            );
            return None;
        }
    };
    let stem = label
        .rsplit_once('.')
        .map_or(label.as_str(), |(stem, _)| stem);
    let stem = stem.strip_prefix("src/").unwrap_or(stem);
    let module = stem
        .strip_suffix("/__init__")
        .unwrap_or(stem)
        .replace('/', ".");
    Some(AnalyzedFile {
        path: label,
        module,
        facts,
        is_package: false,
        fastapi_router_mode: config::FastapiRouterMode::Auto,
    })
}

fn file_name(path: &Path) -> &str {
    path.file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
}

fn dockerfile(name: &str) -> bool {
    matches!(name, "Dockerfile" | "Containerfile")
        || [
            "Dockerfile.",
            "Containerfile.",
            "Dockerfile-",
            "Containerfile-",
        ]
        .iter()
        .any(|prefix| name.starts_with(prefix))
        || [".dockerfile", ".Dockerfile"]
            .iter()
            .any(|suffix| name.ends_with(suffix))
}

pub(in crate::phase02) fn is_command_file(path: &Path) -> bool {
    let name = file_name(path);
    dockerfile(name)
        || name == "Makefile"
        || name == "Procfile"
        || name.starts_with("Procfile.")
        || ((name.starts_with("compose.") || name.starts_with("docker-compose."))
            && (name.ends_with(".yml") || name.ends_with(".yaml")))
}

fn is_python_command(command: &str) -> bool {
    command
        .strip_suffix(".exe")
        .unwrap_or(command)
        .strip_prefix("python")
        .is_some_and(|version| {
            version
                .bytes()
                .all(|byte| byte.is_ascii_digit() || byte == b'.')
        })
}

fn python_module_args(mut args: &[String]) -> Option<(&str, &[String])> {
    while let Some((argument, rest)) = args.split_first() {
        args = rest;
        if argument == "--check-hash-based-pycs" {
            args = rest.get(1..)?;
            continue;
        }
        let flags = argument
            .strip_prefix('-')
            .filter(|flags| !flags.is_empty())?;
        for (index, flag) in flags.char_indices() {
            match flag {
                'm' => {
                    let inline = &flags[index + 1..];
                    return if inline.is_empty() {
                        rest.split_first()
                            .map(|(module, args)| (module.as_str(), args))
                    } else {
                        Some((inline, rest))
                    };
                }
                'W' | 'X' => {
                    if index + 1 == flags.len() {
                        args = rest.get(1..)?;
                    }
                    break;
                }
                'b' | 'B' | 'd' | 'E' | 'i' | 'I' | 'O' | 'P' | 'q' | 'R' | 's' | 'S' | 'u'
                | 'v' | 'x' => {}
                // A script, stdin, -c, or -- ends interpreter option parsing.
                _ => return None,
            }
        }
    }
    None
}

fn yaml_scalar(value: &str) -> String {
    let value = value.trim();
    if let Ok(quoted) = serde_json::from_str::<String>(value) {
        quoted
    } else if let Some(quoted) = value
        .strip_prefix('\'')
        .and_then(|value| value.strip_suffix('\''))
    {
        quoted.replace("''", "'")
    } else {
        value.to_owned()
    }
}

fn segments<'a>(text: &'a str, separators: &[char]) -> Vec<&'a str> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut quote = None;
    let mut escape = false;
    for (index, ch) in text.char_indices() {
        if escape {
            escape = false;
        } else if ch == '\\' && quote != Some('\'') {
            escape = true;
        } else if quote == Some(ch) {
            quote = None;
        } else if quote.is_none() {
            match ch {
                '\'' | '"' | '`' => quote = Some(ch),
                '#' if index == start || text.as_bytes()[index - 1].is_ascii_whitespace() => {
                    parts.push(&text[start..index]);
                    return parts;
                }
                ch if separators.contains(&ch) => {
                    parts.push(&text[start..index]);
                    start = index + ch.len_utf8();
                }
                _ => {}
            }
        }
    }
    parts.push(&text[start..]);
    parts
}

fn prefix_matches<'a>(
    references: &'a BTreeMap<String, Vec<Reference>>,
    prefix: &str,
) -> impl Iterator<Item = &'a Reference> {
    let children = format!("{prefix}.");
    references.get(prefix).into_iter().flatten().chain(
        references
            .range(children.clone()..)
            .take_while(move |(name, _)| name.starts_with(&children))
            .flat_map(|(_, values)| values),
    )
}

fn includes_module(provider: &str, reference: &str) -> bool {
    reference == provider
        || reference
            .strip_prefix(provider)
            .is_some_and(|suffix| suffix.starts_with('.'))
}

fn import_paths(distribution: &str) -> BTreeSet<String> {
    let mut paths: BTreeSet<_> = import_names(distribution)
        .iter()
        .map(|name| (*name).to_owned())
        .collect();
    paths.insert(distribution.replace('-', "_"));
    if let Some((root, _)) = distribution.split_once('-')
        && matches!(
            root,
            "google" | "azure" | "zope" | "opentelemetry" | "sphinxcontrib" | "ruamel"
        )
    {
        paths.insert(distribution.replace('-', "."));
    }
    paths
}

// Distribution names and exposed import or console names are independent.
fn import_names(distribution: &str) -> &'static [&'static str] {
    match distribution {
        "attrs" => &["attr"],
        "beautifulsoup4" => &["bs4"],
        "django-allauth" => &["allauth"],
        "django-bmemcached" => &["django_bmemcached"],
        "django-debug-toolbar" => &["debug_toolbar"],
        "django-csp" => &["csp"],
        "django-storages" => &["storages"],
        "django-stubs" => &["mypy_django_plugin"],
        "djangorestframework" => &["rest_framework"],
        "dnspython" => &["dns"],
        "factory-boy" => &["factory"],
        "flake8-docstrings" => &["flake8_docstrings"],
        "gitpython" => &["git"],
        "google-api-core" => &["google.api_core"],
        "google-api-python-client" => &["apiclient", "googleapiclient"],
        "google-auth" => &["google.auth", "google.oauth2"],
        "google-cloud-core" => &["google.cloud.client", "google.cloud.exceptions"],
        "grpcio" => &["grpc"],
        "matplotlib" => &["mpl_toolkits", "pylab"],
        "opencv-python"
        | "opencv-python-headless"
        | "opencv-contrib-python"
        | "opencv-contrib-python-headless" => &["cv2"],
        "pillow" | "pillow-simd" => &["PIL"],
        "pydispatcher" | "pypydispatcher" => &["pydispatch"],
        "protobuf" => &["google.protobuf"],
        "pycryptodome" => &["Crypto"],
        "pygithub" => &["github"],
        "pyjwt" => &["jwt"],
        "pymupdf" => &["fitz"],
        "pyopenssl" => &["OpenSSL"],
        "pyserial" => &["serial"],
        "pytest-click" => &["pytest_click"],
        "pytest-subtests" => &["pytest_subtests"],
        "python-consul" | "python-consul2" => &["consul"],
        "python-dateutil" => &["dateutil"],
        "python-dotenv" => &["dotenv"],
        "python-jose" => &["jose"],
        "python-ldap" => &["ldap", "ldif"],
        "python-magic" => &["magic"],
        "python-multipart" => &["python_multipart", "multipart"],
        "pyyaml" => &["yaml"],
        "pyusb" => &["usb"],
        "pyzmq" => &["zmq"],
        "scikit-image" => &["skimage"],
        "scikit-learn" => &["sklearn"],
        "sphinx-click" => &["sphinx_click"],
        "websocket-client" => &["websocket"],
        _ => &[],
    }
}

fn command_names(distribution: &str) -> &'static [&'static str] {
    match distribution {
        "django" => &["django-admin"],
        "ddtrace" => &["ddtrace-run"],
        "newrelic" => &["newrelic-admin"],
        "sphinx" => &["sphinx-build", "sphinx-autobuild"],
        "supervisor" => &["supervisord", "supervisorctl"],
        "waitress" => &["waitress-serve"],
        _ => &[],
    }
}

// The pinned profile exempts these tool, plugin, driver and stub declarations
// even without a concrete usage reference.
pub(super) fn exempt(name: &str) -> bool {
    name.starts_with("types-")
        || name.ends_with("-stubs")
        || matches!(
            name,
            "black"
                | "ruff"
                | "mypy"
                | "pytest"
                | "flake8"
                | "pylint"
                | "isort"
                | "pre-commit"
                | "tox"
                | "nox"
                | "coverage"
                | "sphinx"
                | "mkdocs"
                | "twine"
                | "build"
                | "setuptools"
                | "wheel"
                | "pip"
                | "pipx"
                | "autopep8"
                | "bandit"
                | "pyflakes"
                | "pycodestyle"
                | "pydocstyle"
                | "pytest-cov"
                | "pytest-xdist"
                | "pytest-mock"
                | "pytest-asyncio"
                | "pytest-django"
                | "pytest-flask"
                | "pytest-celery"
                | "flask-cors"
                | "flask-login"
                | "flask-migrate"
                | "flask-sqlalchemy"
                | "django-cors-headers"
                | "django-filter"
                | "django-extensions"
                | "celery"
                | "gunicorn"
                | "uvicorn"
                | "psycopg"
                | "psycopg-binary"
                | "psycopg-c"
                | "psycopg2"
                | "psycopg2-binary"
                | "mysqlclient"
                | "pymysql"
                | "cx-oracle"
                | "oracledb"
                | "asyncpg"
                | "aiomysql"
                | "aiosqlite"
        )
}
