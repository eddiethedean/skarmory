//! Static package metadata for Phase 0.3 compatibility scans.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use skarmory_python::phase02::{
    Expression, ExpressionKind, Import, ModuleFacts, Statement, StatementKind, extract,
};
use skarmory_python::simple_name;

use super::AnalyzedFile;
use crate::Issue;
use crate::preview_source::{Stamp, read_snapshot};

#[derive(Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(super) struct Entrypoint {
    pub(super) module: String,
    pub(super) attributes: Vec<String>,
    pub(super) manifest: &'static str,
    pub(super) line: Option<usize>,
}

#[derive(Default)]
pub(super) struct Snapshot {
    metadata: Vec<(PathBuf, &'static str, Option<Stamp>)>,
    pub(super) entries: BTreeSet<Entrypoint>,
    pub(super) issues: Vec<Issue>,
    pub(super) file_only: bool,
}

impl Snapshot {
    pub(super) fn read(path: &Path, files: &[AnalyzedFile]) -> Self {
        let file_only = path.is_file();
        let root = if file_only {
            path.parent().expect("file path has a parent")
        } else {
            path
        };
        let mut snapshot = Self {
            file_only,
            ..Self::default()
        };
        if let Some(source) = snapshot.read_metadata(root, "pyproject.toml") {
            snapshot.read_pyproject(&source);
        }
        if let Some(source) = snapshot.read_metadata(root, "setup.cfg") {
            snapshot.read_setup_cfg(&source);
        }
        let local_setuptools = files.iter().any(|file| file.module == "setuptools");
        let local_distutils = files
            .iter()
            .any(|file| matches!(file.module.as_str(), "distutils" | "distutils.core"));
        if let Some(setup) = files.iter().find(|file| file.path == "setup.py") {
            // This input is already subject to the source snapshot verification.
            snapshot.read_setup_py(&setup.facts, local_setuptools, local_distutils);
        } else if let Some(source) = snapshot.read_metadata(root, "setup.py") {
            match extract(&source.replace("\r\n", "\n").replace('\r', "\n")) {
                Ok(facts) => snapshot.read_setup_py(&facts, local_setuptools, local_distutils),
                Err(error) => snapshot
                    .issues
                    .push(Issue::new(error.kind, "setup.py", error.message).at(error.location)),
            }
        }
        snapshot
    }

    fn read_metadata(&mut self, root: &Path, label: &'static str) -> Option<String> {
        let path = root.join(label);
        let observed = match fs::symlink_metadata(&path) {
            Ok(metadata) => Some(Stamp::new(&metadata)),
            Err(error) if error.kind() == ErrorKind::NotFound => None,
            Err(error) => {
                self.issues.push(Issue::new("read_error", label, error));
                return None;
            }
        };
        let source = observed.as_ref().map(|stamp| {
            let (bytes, _) = read_snapshot(&path, label, Some(stamp))?;
            String::from_utf8(bytes).map_err(|error| Issue::new("decode_error", label, error))
        });
        self.metadata.push((path, label, observed));
        match source {
            Some(Ok(source)) => Some(source),
            Some(Err(error)) => {
                self.issues.push(error);
                None
            }
            None => None,
        }
    }

    pub(super) fn verify(&self) -> impl Iterator<Item = Issue> + '_ {
        self.metadata.iter().filter_map(|(path, label, seen)| {
            let unchanged = match fs::symlink_metadata(path) {
                Ok(metadata) => seen.as_ref() == Some(&Stamp::new(&metadata)),
                Err(error) => seen.is_none() && error.kind() == ErrorKind::NotFound,
            };
            (!unchanged).then(|| {
                Issue::new(
                    "configuration_changed",
                    label,
                    "package entry-point metadata changed during analysis",
                )
            })
        })
    }

    fn read_pyproject(&mut self, source: &str) {
        let manifest = "pyproject.toml";
        let table: toml::Table = match toml::from_str(source) {
            Ok(table) => table,
            Err(error) => {
                self.issues
                    .push(Issue::new("invalid_package_entrypoints", manifest, error));
                return;
            }
        };
        if let Some(project) = table.get("project").and_then(toml::Value::as_table) {
            if let Some(dynamic) = project.get("dynamic").and_then(toml::Value::as_array) {
                for field in dynamic.iter().filter_map(toml::Value::as_str) {
                    if matches!(field, "scripts" | "gui-scripts" | "entry-points") {
                        self.issues.push(Issue::new(
                            "unresolved_package_entrypoint",
                            manifest,
                            format!(
                                "dynamic package metadata for {field:?} is outside the static model"
                            ),
                        ));
                    }
                }
            }
            for name in ["scripts", "gui-scripts"] {
                if let Some(value) = project.get(name) {
                    self.read_toml_entries(value);
                }
            }
            if let Some(groups) = project.get("entry-points") {
                self.read_toml_groups(groups);
            }
        }
        if let Some(poetry) = table
            .get("tool")
            .and_then(|tool| tool.get("poetry"))
            .and_then(toml::Value::as_table)
        {
            if let Some(value) = poetry.get("scripts") {
                self.read_toml_entries(value);
            }
            if let Some(groups) = poetry.get("plugins") {
                self.read_toml_groups(groups);
            }
        }
    }

    fn read_toml_groups(&mut self, groups: &toml::Value) {
        if let Some(table) = groups.as_table() {
            for value in table.values() {
                self.read_toml_entries(value);
            }
        } else {
            self.invalid("pyproject.toml", None);
        }
    }

    fn read_toml_entries(&mut self, entries: &toml::Value) {
        let Some(table) = entries.as_table() else {
            self.invalid("pyproject.toml", None);
            return;
        };
        for value in table.values() {
            if value.get("type").and_then(toml::Value::as_str) == Some("file") {
                continue;
            }
            let reference = value
                .get("callable")
                .or_else(|| value.get("reference"))
                .unwrap_or(value);
            if let Some(reference) = reference.as_str() {
                self.add(reference, "pyproject.toml", None);
            } else {
                self.invalid("pyproject.toml", None);
            }
        }
    }

    fn read_setup_cfg(&mut self, source: &str) {
        let mut entry_points = false;
        let mut current_group = None;
        let mut groups = BTreeMap::<_, Vec<_>>::new();
        for (index, raw) in source.lines().enumerate() {
            let line = raw.trim();
            if let Some(header) = line.strip_prefix('[') {
                entry_points = header
                    .rsplit_once(']')
                    .is_some_and(|(name, _)| name == "options.entry_points");
                current_group = None;
            }
            if !entry_points || line.is_empty() || line.starts_with(['#', ';', '[']) {
                continue;
            }
            let indent = raw.len() - raw.trim_start().len();
            if current_group.is_none_or(|(_, previous)| indent <= previous) {
                let Some((name, value)) = line.split_once(['=', ':']) else {
                    self.invalid("setup.cfg", Some(index + 1));
                    continue;
                };
                let name = name.trim();
                current_group = Some((name, indent));
                groups.insert(name, vec![(value, index + 1)]);
            } else if let Some((name, _)) = current_group {
                groups
                    .get_mut(name)
                    .expect("current group was inserted")
                    .push((line, index + 1));
            }
        }
        for (line, number) in groups.into_values().flatten() {
            self.read_assignments(line, "setup.cfg", number);
        }
    }

    fn read_setup_py(
        &mut self,
        facts: &ModuleFacts,
        local_setuptools: bool,
        local_distutils: bool,
    ) {
        let setup_import = |import: &Import| {
            let module = import.module.as_str();
            let local_provider = match module {
                "setuptools" => local_setuptools,
                "distutils" | "distutils.core" => local_distutils,
                _ => false,
            };
            match (import.level, module, import.member.as_deref()) {
                _ if local_provider => SetupValue::Unknown,
                (0, "setuptools" | "distutils.core", Some("setup")) => SetupValue::Setup,
                (0, "setuptools" | "distutils.core", None) if !import.exposes_top_level => {
                    SetupValue::SetupModule
                }
                (0, "distutils" | "distutils.core", None) => SetupValue::Distutils,
                _ => SetupValue::Unknown,
            }
        };
        let mut bindings = BTreeMap::new();
        let mut unmodeled_effect = None;
        let mut declared_entries = Vec::new();
        for statement in &facts.body {
            match &statement.kind {
                StatementKind::Import(import) => {
                    bindings.insert(import.local.text.as_str(), setup_import(import));
                }
                StatementKind::Assign {
                    names,
                    value,
                    annotation,
                } => {
                    if value
                        .iter()
                        .chain(annotation)
                        .any(|value| !setup_expression_is_static(value, &bindings))
                    {
                        unmodeled_effect.get_or_insert(statement.at.line);
                    }
                    let value = if names.len() == 1 {
                        value
                            .as_ref()
                            .map_or(SetupValue::Unknown, |value| setup_value(value, &bindings))
                    } else {
                        SetupValue::Unknown
                    };
                    for name in names {
                        bindings.insert(name.text.as_str(), value.clone());
                    }
                }
                StatementKind::Define(definition) => {
                    if !definition.decorators.is_empty()
                        || !definition.defaults.is_empty()
                        || !definition.annotations.is_empty()
                        || !definition.bases.is_empty()
                    {
                        unmodeled_effect.get_or_insert(statement.at.line);
                    }
                    bindings.insert(definition.name.text.as_str(), SetupValue::Unknown);
                }
                StatementKind::Evaluate(Expression {
                    kind:
                        ExpressionKind::Call {
                            function,
                            arguments,
                            keywords,
                        },
                    ..
                }) if matches!(setup_value(function, &bindings), SetupValue::Setup) => {
                    if arguments
                        .iter()
                        .chain(keywords.iter().map(|(_, value)| value))
                        .any(|value| !setup_expression_is_static(value, &bindings))
                    {
                        unmodeled_effect.get_or_insert(statement.at.line);
                    }
                    if let Some((_, value)) =
                        keywords.iter().find(|(name, _)| name == "entry_points")
                    {
                        declared_entries.push((setup_value(value, &bindings), statement.at.line));
                    }
                }
                StatementKind::Evaluate(Expression {
                    kind:
                        ExpressionKind::Call {
                            function,
                            arguments,
                            keywords,
                        },
                    ..
                }) if arguments.is_empty()
                    && keywords.is_empty()
                    && matches!(&function.kind,
                        ExpressionKind::Member(open, name)
                            if name == "close"
                                && matches!(&open.kind,
                                    ExpressionKind::Call { function, .. }
                                        if matches!(&function.kind,
                                            ExpressionKind::Name(name)
                                                if name == "open" && !bindings.contains_key("open")))) =>
                    {}
                StatementKind::Evaluate(value) if setup_expression_is_static(value, &bindings) => {}
                _ => {
                    unmodeled_effect.get_or_insert(statement.at.line);
                }
            }
        }
        if let Some(line) = unmodeled_effect
            && facts.body.iter().any(mentions_setup_entry_points)
            && has_setup_import(&facts.body, &|import| {
                !matches!(setup_import(import), SetupValue::Unknown)
            })
        {
            self.invalid("setup.py", Some(line));
            return;
        }
        for (value, line) in declared_entries {
            match value {
                SetupValue::Mapping(groups) => {
                    for value in groups.values().cloned() {
                        self.read_setup_values(value, line);
                    }
                }
                value => self.read_setup_values(value, line),
            }
        }
    }

    fn read_setup_values(&mut self, value: SetupValue<'_>, call_line: usize) {
        match value {
            SetupValue::Text(text, line) => self.read_assignments(text, "setup.py", line),
            SetupValue::Sequence(values) => {
                for value in values.iter() {
                    match value {
                        SetupValue::Text(text, line) => {
                            self.read_assignments(text, "setup.py", *line)
                        }
                        _ => self.invalid("setup.py", Some(call_line)),
                    }
                }
            }
            _ => self.invalid("setup.py", Some(call_line)),
        }
    }

    fn read_assignments(&mut self, text: &str, manifest: &'static str, line: usize) {
        for reference in text.lines().map(str::trim) {
            if reference.is_empty() || reference.starts_with(['#', ';', '[']) {
                continue;
            }
            if let Some((name, value)) = reference.split_once('=')
                && !name.trim().is_empty()
            {
                self.add(value, manifest, Some(line));
            } else {
                self.invalid(manifest, Some(line));
            }
        }
    }

    fn add(&mut self, raw: &str, manifest: &'static str, line: Option<usize>) {
        let reference = raw.split('[').next().unwrap_or_default().trim();
        let (module, attributes) = reference
            .split_once(':')
            .map_or((reference, None), |(module, value)| {
                (module.trim(), Some(value.trim()))
            });
        if !module.split('.').all(simple_name)
            || attributes.is_some_and(|names| !names.split('.').all(simple_name))
        {
            self.invalid(manifest, line);
            return;
        }
        self.entries.insert(Entrypoint {
            module: module.to_owned(),
            attributes: attributes
                .into_iter()
                .flat_map(|names| names.split('.'))
                .map(str::to_owned)
                .collect(),
            manifest,
            line,
        });
    }

    fn invalid(&mut self, manifest: &str, line: Option<usize>) {
        let mut issue = Issue::new(
            "invalid_package_entrypoints",
            manifest,
            "package entry-point declaration is outside the supported static formats",
        );
        issue.line = line;
        self.issues.push(issue);
    }
}

#[derive(Clone)]
enum SetupValue<'a> {
    Setup,
    SetupModule,
    Distutils,
    Text(&'a str, usize),
    Sequence(Rc<[SetupValue<'a>]>),
    Mapping(Rc<BTreeMap<&'a str, SetupValue<'a>>>),
    Unknown,
}

// Recognize providers in conditional setup code so it cannot silently
// bypass the diagnostic for registration outside the static model.
fn has_setup_import(body: &[Statement], is_setup_import: &impl Fn(&Import) -> bool) -> bool {
    body.iter().any(|statement| match &statement.kind {
        StatementKind::Import(import) => is_setup_import(import),
        StatementKind::If {
            test,
            body,
            otherwise,
        } => match test.kind {
            ExpressionKind::Bool(true) => has_setup_import(body, is_setup_import),
            ExpressionKind::Bool(false) => has_setup_import(otherwise, is_setup_import),
            _ => {
                has_setup_import(body, is_setup_import)
                    || has_setup_import(otherwise, is_setup_import)
            }
        },
        _ => false,
    })
}

fn mentions_setup_entry_points(statement: &Statement) -> bool {
    match &statement.kind {
        StatementKind::Evaluate(value)
        | StatementKind::Assign {
            value: Some(value), ..
        } => {
            matches!(&value.kind, ExpressionKind::Call { keywords, .. }
                if keywords.iter().any(|(name, _)| name == "entry_points"))
        }
        StatementKind::Define(definition) => {
            definition.body.iter().any(mentions_setup_entry_points)
        }
        StatementKind::If {
            body, otherwise, ..
        } => body
            .iter()
            .chain(otherwise.iter())
            .any(mentions_setup_entry_points),
        StatementKind::With { body, .. } => body.iter().any(mentions_setup_entry_points),
        _ => false,
    }
}

fn setup_expression_is_static(
    expression: &Expression,
    bindings: &BTreeMap<&str, SetupValue<'_>>,
) -> bool {
    match &expression.kind {
        ExpressionKind::Name(_)
        | ExpressionKind::Text(_)
        | ExpressionKind::Scalar
        | ExpressionKind::Bool(_) => true,
        ExpressionKind::Sequence(values) | ExpressionKind::Dictionary(values) => values
            .iter()
            .all(|value| setup_expression_is_static(value, bindings)),
        ExpressionKind::Call {
            function,
            arguments,
            keywords,
        } => {
            arguments.is_empty()
                && matches!(&function.kind, ExpressionKind::Name(name) if name == "dict" && !bindings.contains_key("dict"))
                && keywords
                    .iter()
                    .all(|(_, value)| setup_expression_is_static(value, bindings))
        }
        _ => !matches!(setup_value(expression, bindings), SetupValue::Unknown),
    }
}

fn setup_value<'a>(
    expression: &'a Expression,
    bindings: &BTreeMap<&str, SetupValue<'a>>,
) -> SetupValue<'a> {
    match &expression.kind {
        ExpressionKind::Name(name) => bindings
            .get(name.as_str())
            .cloned()
            .unwrap_or(SetupValue::Unknown),
        ExpressionKind::Member(base, name) => match (setup_value(base, bindings), name.as_str()) {
            (SetupValue::SetupModule, "setup") => SetupValue::Setup,
            (SetupValue::Distutils, "core") => SetupValue::SetupModule,
            _ => SetupValue::Unknown,
        },
        ExpressionKind::Text(text) => SetupValue::Text(text, expression.at.line),
        ExpressionKind::Sequence(values) => SetupValue::Sequence(
            values
                .iter()
                .map(|value| match setup_value(value, bindings) {
                    value @ SetupValue::Text(..) => value,
                    _ => SetupValue::Unknown,
                })
                .collect(),
        ),
        ExpressionKind::Dictionary(values) => {
            let mut groups = BTreeMap::new();
            for pair in values.as_chunks::<2>().0 {
                let SetupValue::Text(name, _) = setup_value(&pair[0], bindings) else {
                    return SetupValue::Unknown;
                };
                groups.insert(name, setup_group(&pair[1], bindings));
            }
            SetupValue::Mapping(Rc::new(groups))
        }
        ExpressionKind::Call {
            function,
            arguments,
            keywords,
        } if arguments.is_empty()
            && matches!(&function.kind, ExpressionKind::Name(name) if name == "dict" && !bindings.contains_key("dict")) =>
        {
            SetupValue::Mapping(Rc::new(
                keywords
                    .iter()
                    .map(|(key, value)| (key.as_str(), setup_group(value, bindings)))
                    .collect(),
            ))
        }
        _ => SetupValue::Unknown,
    }
}

fn setup_group<'a>(
    expression: &'a Expression,
    bindings: &BTreeMap<&str, SetupValue<'a>>,
) -> SetupValue<'a> {
    match setup_value(expression, bindings) {
        value @ (SetupValue::Text(..) | SetupValue::Sequence(_)) => value,
        _ => SetupValue::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::phase02::{Options, Profile, scan};

    fn options() -> Options {
        Options {
            profile: Profile::SkylosPhase03,
            grep_verify: Some(false),
            confidence: Some(0),
            ..Options::default()
        }
    }

    fn diagnostic_names(report: &super::super::Report) -> BTreeSet<&str> {
        report
            .categories
            .iter()
            .filter_map(|diagnostic| diagnostic.full_name.as_deref())
            .collect()
    }

    fn write(root: &Path, file: &str, source: &str) {
        let path = root.join(file);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, source).unwrap();
    }

    #[test]
    fn package_declarations_resolve_selected_bindings_and_track_manifest_edits() {
        let project = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let root = project.path();
        let manifest = "[project.entry-points.\"demo.formatters\"]\n\
            json = \"demo:render [fast]\"\n\
            yaml = \"demo.formatters:Renderers.yaml\"\n\
            [project.entry-points.pytest11]\n\
            demo = \"demo.plugin\"\n\
            [tool.poetry.plugins.\"poetry.application.plugin\"]\n\
            demo = \"demo.checker:Checker\"\n";
        write(root, "pyproject.toml", manifest);
        write(
            root,
            "demo/__init__.py",
            "from .formatters import render_json as render\n",
        );
        write(
            root,
            "demo/formatters.py",
            "def render_json():\n    return helper()\n\
            def render_xml():\n    return 0\n\
            def helper():\n    return 1\n\
            class Renderers:\n    @staticmethod\n    def yaml():\n        return 1\n    @staticmethod\n    def toml():\n        return 2\n",
        );
        write(root, "demo/other.py", "def render_json():\n    return 2\n");
        write(
            root,
            "demo/plugin.py",
            "def plugin_helper():\n    return 0\n",
        );
        write(
            root,
            "demo/checker.py",
            "class Checker:\n    name = 'checker'\n    def run(self):\n        return []\n    def _unused(self):\n        return None\n",
        );
        let baseline = scan(root, &options()).unwrap();
        assert!(baseline.is_complete(), "{}", baseline.to_json().unwrap());
        let diagnostics = diagnostic_names(&baseline);
        for name in [
            "demo.formatters.render_json",
            "demo.formatters.helper",
            "demo.formatters.Renderers",
            "demo.formatters.Renderers.yaml",
            "demo.checker.Checker",
            "demo.checker.Checker.name",
            "demo.checker.Checker.run",
        ] {
            assert!(
                !diagnostics.contains(name),
                "reported retained definition {name}"
            );
        }
        for name in [
            "demo.formatters.render_xml",
            "demo.formatters.Renderers.toml",
            "demo.other.render_json",
            "demo.plugin.plugin_helper",
            "demo.checker.Checker._unused",
        ] {
            assert!(diagnostics.contains(name), "missing unused control {name}");
        }
        let before: serde_json::Value = serde_json::from_str(&baseline.to_json().unwrap()).unwrap();
        assert!(
            before["dead_code_evidence"]["symbols"]
                .as_array()
                .unwrap()
                .iter()
                .any(
                    |item| item["qualified_name"] == "demo.formatters.render_json"
                        && item["evidence"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .any(|event| event["kind"] == "package_entrypoint")
                )
        );
        let cached = Options {
            workers: 4,
            cache_dir: Some(cache.path().into()),
            ..options()
        };
        let without_cache = |report: super::super::Report| {
            let mut value: serde_json::Value =
                serde_json::from_str(&report.to_json().unwrap()).unwrap();
            value.as_object_mut().unwrap().remove("cache");
            value
        };
        assert_eq!(before, without_cache(scan(root, &cached).unwrap()));
        assert_eq!(before, without_cache(scan(root, &cached).unwrap()));
        write(
            root,
            "pyproject.toml",
            &manifest.replace("demo:render [fast]", "demo.formatters:render_xml"),
        );
        let edited = scan(root, &options()).unwrap();
        let diagnostics = diagnostic_names(&edited);
        assert!(diagnostics.contains("demo.formatters.render_json"));
        assert!(!diagnostics.contains("demo.formatters.render_xml"));
        assert_eq!(
            without_cache(edited),
            without_cache(scan(root, &cached).unwrap())
        );
        let phase02 = scan(
            root,
            &Options {
                profile: Profile::SkylosPhase02,
                ..options()
            },
        )
        .unwrap();
        assert!(diagnostic_names(&phase02).contains("demo.formatters.render_xml"));
    }

    #[test]
    fn setup_metadata_uses_static_values_and_does_not_execute_setup() {
        let project = tempfile::tempdir().unwrap();
        let root = project.path();
        let marker = root.join("setup-was-executed");
        write(
            root,
            "setup.py",
            &format!(
                "from setuptools import setup as register\n\
            open({:?}, 'w').close()\n\
            UNUSED = {{'console_scripts': ['old = tool.cli:retired']}}\n\
            CONSOLE = ['a = tool.cli:run_a']\n\
            register(name='tool', entry_points={{'console_scripts': CONSOLE, 'flake8.extension': 'checker = tool.cli:Checker'}})\n",
                marker.to_string_lossy()
            ),
        );
        write(
            root,
            "setup.cfg",
            "[options.entry_points]\nconsole_scripts =\n    b = tool.cli:run_b\n",
        );
        write(root, "tool/__init__.py", "");
        write(
            root,
            "tool/cli.py",
            "def run_a():\n    return 1\n\
            def run_b():\n    return 2\n\
            def retired():\n    return 3\n\
            class Checker:\n    pass\n",
        );
        let selected_setup = scan(root, &options()).unwrap();
        assert!(
            selected_setup.is_complete(),
            "{}",
            selected_setup.to_json().unwrap()
        );
        let selected_diagnostics = diagnostic_names(&selected_setup);
        for name in ["tool.cli.run_a", "tool.cli.run_b", "tool.cli.Checker"] {
            assert!(!selected_diagnostics.contains(name));
        }
        assert!(selected_diagnostics.contains("tool.cli.retired"));
        let selected_json: serde_json::Value =
            serde_json::from_str(&selected_setup.to_json().unwrap()).unwrap();
        assert!(
            selected_json["analysis_summary"]["dead_code_liveness"]["rescued"]
                .as_array()
                .unwrap()
                .iter()
                .any(|row| {
                    row["full_name"] == "tool.cli.run_a" && row["reason"] == "package_entrypoint"
                })
        );
        assert!(!marker.exists());
        let runtime_sources = Options {
            excludes: vec!["setup.py".into()],
            ..options()
        };
        let setup_source = fs::read_to_string(root.join("setup.py")).unwrap();
        let literal_setup = setup_source
            .lines()
            .filter(|line| !line.starts_with("open("))
            .collect::<Vec<_>>()
            .join("\n");
        write(root, "setup.py", &literal_setup);
        let report = scan(root, &runtime_sources).unwrap();
        assert!(report.is_complete(), "{}", report.to_json().unwrap());
        let diagnostics = diagnostic_names(&report);
        for name in ["tool.cli.run_a", "tool.cli.run_b", "tool.cli.Checker"] {
            assert!(
                !diagnostics.contains(name),
                "reported declared entry point {name}"
            );
        }
        assert!(diagnostics.contains("tool.cli.retired"));
        assert!(!marker.exists());
        write(
            root,
            "setup.cfg",
            "[options.entry_points]\nconsole_scripts =\n    b = tool.cli:missing\n",
        );
        let report = scan(root, &runtime_sources).unwrap();
        assert!(!report.is_complete());
        assert!(report.coverage.issues.iter().any(|issue| {
            issue.kind == "unresolved_package_entrypoint"
                && issue.path == "setup.cfg"
                && issue.line == Some(3)
        }));
        write(root, "setup.cfg", "");
        write(
            root,
            "setup.py",
            "from setuptools import setup\nCONSOLE = []\nALIAS = CONSOLE\nALIAS.append('a = tool.cli:run_a')\nsetup(entry_points={'console_scripts': CONSOLE})\n",
        );
        let report = scan(root, &runtime_sources).unwrap();
        assert!(report.coverage.issues.iter().any(|issue| {
            issue.kind == "invalid_package_entrypoints" && issue.path == "setup.py"
        }));
    }

    #[test]
    fn setup_cfg_recognizes_entry_points_after_a_section_header_comment() {
        let project = tempfile::tempdir().unwrap();
        let root = project.path();
        write(
            root,
            "app.py",
            "def main():\n    return 0\ndef unused():\n    return 1\n",
        );
        write(
            root,
            "setup.cfg",
            "[options.entry_points] # generated\nconsole_scripts =\n    demo = app:main\n",
        );
        let report = scan(root, &options()).unwrap();
        assert!(report.is_complete(), "{}", report.to_json().unwrap());
        assert_eq!(diagnostic_names(&report), BTreeSet::from(["app.unused"]));
    }

    #[test]
    fn guarded_setup_registration_reports_incomplete_metadata_coverage() {
        let project = tempfile::tempdir().unwrap();
        let root = project.path();
        write(root, "app.py", "def main():\n    return 0\n");
        write(
            root,
            "setup.py",
            "if __name__ == '__main__':\n    from setuptools import setup\n    setup(entry_points={'console_scripts': ['demo = app:main']})\n",
        );
        let report = scan(
            root,
            &Options {
                excludes: vec!["setup.py".into()],
                ..options()
            },
        )
        .unwrap();
        assert!(!report.is_complete(), "{}", report.to_json().unwrap());
        assert!(report.coverage.issues.iter().any(|issue| {
            issue.kind == "invalid_package_entrypoints"
                && issue.path == "setup.py"
                && issue.line == Some(1)
        }));
        assert!(!diagnostic_names(&report).contains("app.main"));
    }

    #[test]
    fn local_setuptools_lookalike_does_not_declare_package_roots() {
        let project = tempfile::tempdir().unwrap();
        let root = project.path();
        write(
            root,
            "setuptools.py",
            "def setup(entry_points):\n    return None\n",
        );
        write(
            root,
            "setup.py",
            "from setuptools import setup\nsetup(entry_points={'g': ['a = app:handler']})\n",
        );
        write(root, "app.py", "def handler():\n    return 1\n");
        let report = scan(root, &options()).unwrap();
        assert!(report.is_complete(), "{}", report.to_json().unwrap());
        assert!(diagnostic_names(&report).contains("app.handler"));
    }
}
