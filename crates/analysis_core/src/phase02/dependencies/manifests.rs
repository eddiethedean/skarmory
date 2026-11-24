use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

use pep508_rs::Requirement;
use serde::Deserialize;
use skarmory_python::phase02::extract;
use toml::Spanned;

use super::{Declaration, Inputs, ini_entries, normalize, source_line};
use crate::phase02::AnalyzedFile;
use crate::phase02::package_entrypoints::{SetupValue, declares_setup_keyword, setup_declarations};

const MAX_ANCESTORS: usize = 5;
const MAX_INCLUDE_DEPTH: usize = 16;

#[derive(Default)]
pub(super) struct Inventory {
    pub(super) dependencies: BTreeMap<String, Declaration>,
    pub(super) project_name: Option<String>,
}

#[derive(Deserialize)]
struct Pyproject {
    project: Option<Pep621>,
    tool: Option<Setuptools>,
}

#[derive(Deserialize)]
struct Pep621 {
    name: Option<String>,
    dependencies: Option<Vec<Spanned<String>>>,
    #[serde(default)]
    dynamic: Vec<String>,
}

#[derive(Deserialize)]
struct Setuptools {
    setuptools: Option<SetuptoolsSettings>,
}

#[derive(Deserialize)]
struct SetuptoolsSettings {
    dynamic: Option<SetuptoolsDynamic>,
}

#[derive(Deserialize)]
struct SetuptoolsDynamic {
    dependencies: Option<Files>,
}

#[derive(Deserialize)]
struct Files {
    file: Vec<String>,
}

impl Inventory {
    pub(super) fn read(root: &Path, files: &[AnalyzedFile], inputs: &mut Inputs) -> Self {
        let mut inventory = Self::default();
        for directory in root.ancestors().take(MAX_ANCESTORS) {
            let mut requirements = Requirements::new(directory, inputs, &mut inventory);
            let path = directory.join("requirements.txt");
            if requirements.inputs.read(directory, &path, false).is_some() {
                requirements.read(&path, 0);
            } else {
                let path = directory.join("requirements.in");
                if requirements.inputs.read(directory, &path, false).is_some() {
                    requirements.read(&path, 0);
                } else {
                    requirements.read_directory(&directory.join("requirements"));
                }
            }
            let pyproject = directory.join("pyproject.toml");
            let has_pyproject = if let Some(source) = inputs.read(directory, &pyproject, false) {
                inventory.pyproject(directory, &pyproject, &source, inputs);
                true
            } else {
                false
            };
            let setup_cfg = directory.join("setup.cfg");
            if let Some(source) = inputs.read(directory, &setup_cfg, false) {
                inventory.setup_cfg(directory, &setup_cfg, &source, inputs);
            }
            inventory.setup_py(root, directory, files, inputs);
            if has_pyproject
                || !inventory.dependencies.is_empty()
                || !inputs.reasons.is_empty()
                || directory.join(".git").exists()
            {
                break;
            }
        }
        inventory
    }

    fn add(&mut self, text: &str, path: &Path, line: usize, inputs: &mut Inputs) {
        let text = text.trim();
        let spec = strip_comment(text);
        match spec.parse::<Requirement>() {
            Ok(requirement) => {
                self.dependencies
                    .entry(requirement.name.to_string())
                    .or_insert_with(|| Declaration {
                        path: path.to_owned(),
                        line,
                    });
            }
            Err(_) => inputs.problem(
                "invalid_dependency_declaration",
                path,
                Some(line),
                "unsupported or invalid Python requirement",
            ),
        }
    }

    fn pyproject(&mut self, root: &Path, path: &Path, source: &str, inputs: &mut Inputs) {
        let parsed = match toml::from_str::<Pyproject>(source) {
            Ok(value) => value,
            Err(error) => {
                inputs.problem("invalid_dependency_manifest", path, None, error);
                return;
            }
        };
        let Some(project) = parsed.project else {
            return;
        };
        self.project_name = project
            .name
            .as_deref()
            .map(normalize)
            .or(self.project_name.take());
        for dependency in project.dependencies.into_iter().flatten() {
            self.add(
                dependency.get_ref(),
                path,
                source_line(source, dependency.span().start),
                inputs,
            );
        }
        if project.dynamic.iter().any(|key| key == "dependencies") {
            let paths = parsed
                .tool
                .and_then(|tool| tool.setuptools)
                .and_then(|settings| settings.dynamic)
                .and_then(|dynamic| dynamic.dependencies)
                .map(|files| files.file);
            self.declared_files(
                root,
                path,
                None,
                paths.as_deref().unwrap_or_default(),
                inputs,
            );
        }
    }

    fn setup_cfg(&mut self, root: &Path, path: &Path, source: &str, inputs: &mut Inputs) {
        let mut file_mode = false;
        let mut files = Vec::new();
        let mut file_line = None;
        for (section, key, value, line, continuation) in ini_entries(source) {
            let value = strip_comment(value);
            match (section, key) {
                ("metadata", "name") if !value.is_empty() && !value.contains(':') => {
                    self.project_name.get_or_insert_with(|| normalize(value));
                }
                ("options", "install_requires") => {
                    if !continuation {
                        file_mode = false;
                    }
                    if let Some(value) = value.strip_prefix("file:") {
                        file_mode = true;
                        file_line = Some(line);
                        files.extend(split_file_names(value));
                    } else if file_mode {
                        files.extend(split_file_names(value));
                    } else if !value.is_empty() {
                        for spec in split_setup_cfg_requirements(value) {
                            self.add(spec, path, line, inputs);
                        }
                    }
                }
                _ => {}
            }
        }
        if file_line.is_some() {
            self.declared_files(root, path, file_line, &files, inputs);
        }
    }

    fn declared_files(
        &mut self,
        root: &Path,
        path: &Path,
        line: Option<usize>,
        files: &[String],
        inputs: &mut Inputs,
    ) {
        if files.is_empty() {
            inputs.problem(
                "unresolved_dependency_input",
                path,
                line,
                "dynamic dependency files are unavailable",
            );
        }
        let mut requirements = Requirements::new(root, inputs, self);
        for file in files {
            if let Some(input) = requirements.resolve(path, line, file) {
                requirements.read(&input, 0);
            }
        }
    }

    fn setup_py(
        &mut self,
        report_root: &Path,
        directory: &Path,
        files: &[AnalyzedFile],
        inputs: &mut Inputs,
    ) {
        let path = directory.join("setup.py");
        let outside;
        let facts = if directory == report_root
            && let Some(setup) = files.iter().find(|file| file.path == "setup.py")
        {
            &setup.facts
        } else {
            let Some(source) = inputs.read(directory, &path, false) else {
                return;
            };
            outside = match extract(&source.replace("\r\n", "\n").replace('\r', "\n")) {
                Ok(facts) => facts,
                Err(error) => {
                    inputs.problem(
                        "unresolved_dependency_input",
                        &path,
                        Some(error.location.line),
                        error.message,
                    );
                    return;
                }
            };
            &outside
        };
        let local_setuptools = files.iter().any(|file| file.module == "setuptools");
        let local_distutils = files
            .iter()
            .any(|file| matches!(file.module.as_str(), "distutils" | "distutils.core"));
        let declarations = setup_declarations(facts, local_setuptools, local_distutils, true);
        if let Some(line) = declarations.opaque_line
            && declares_setup_keyword(&facts.body, "install_requires")
        {
            inputs.problem(
                "unresolved_dependency_input",
                &path,
                Some(line),
                "setup.py runtime declarations are dynamic",
            );
        }
        for call in declarations.calls {
            if let Some(SetupValue::Text(name, _)) = call.values.get("name") {
                self.project_name.get_or_insert_with(|| normalize(name));
            }
            let Some(value) = call.values.get("install_requires") else {
                continue;
            };
            match value {
                SetupValue::Sequence(values) => {
                    for value in values.iter() {
                        self.setup_requirement(value, &path, call.line, inputs);
                    }
                }
                value => self.setup_requirement(value, &path, call.line, inputs),
            }
        }
    }

    fn setup_requirement(
        &mut self,
        value: &SetupValue<'_>,
        path: &Path,
        call_line: usize,
        inputs: &mut Inputs,
    ) {
        match value {
            SetupValue::Text(text, line) => {
                for (offset, line_text) in text.lines().enumerate() {
                    let line_text = strip_comment(line_text.trim());
                    if !line_text.is_empty() {
                        self.add(line_text, path, line + offset, inputs);
                    }
                }
            }
            _ => inputs.problem(
                "unresolved_dependency_input",
                path,
                Some(call_line),
                "setup.py runtime declaration is not a literal requirement",
            ),
        }
    }
}

fn strip_comment(text: &str) -> &str {
    text.char_indices()
        .find(|&(index, ch)| {
            ch == '#' && (index == 0 || text.as_bytes()[index - 1].is_ascii_whitespace())
        })
        .map_or(text, |(index, _)| text[..index].trim_end())
}

fn split_file_names(value: &str) -> impl Iterator<Item = String> + '_ {
    value
        .split(',')
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .map(str::to_owned)
}

fn split_setup_cfg_requirements(value: &str) -> impl Iterator<Item = &str> {
    // Setuptools accepts semicolon-separated requirements. A PEP 508 marker
    // also starts with a semicolon and must stay attached to its requirement.
    let mut rest = value;
    std::iter::from_fn(move || {
        if rest.is_empty() {
            return None;
        }
        if rest.parse::<Requirement>().is_ok() {
            return Some(std::mem::take(&mut rest));
        }
        if let Some((first, next)) = rest.split_once(';')
            && first.parse::<Requirement>().is_ok()
            && !starts_marker(next.trim_start())
        {
            rest = next.trim();
            return Some(first.trim());
        }
        Some(std::mem::take(&mut rest))
    })
}

fn starts_marker(value: &str) -> bool {
    let word = value
        .split(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_'))
        .next()
        .unwrap_or_default();
    value.starts_with(['\'', '"'])
        || matches!(
            word,
            "extra"
                | "implementation_name"
                | "implementation_version"
                | "os_name"
                | "platform_machine"
                | "platform_python_implementation"
                | "platform_release"
                | "platform_system"
                | "platform_version"
                | "python_full_version"
                | "python_version"
                | "sys_platform"
        )
}

struct Requirements<'a> {
    root: &'a Path,
    inputs: &'a mut Inputs,
    inventory: &'a mut Inventory,
    visited: BTreeSet<PathBuf>,
    active: BTreeSet<PathBuf>,
}

impl<'a> Requirements<'a> {
    fn new(root: &'a Path, inputs: &'a mut Inputs, inventory: &'a mut Inventory) -> Self {
        Self {
            root,
            inputs,
            inventory,
            visited: BTreeSet::new(),
            active: BTreeSet::new(),
        }
    }

    fn read_directory(&mut self, path: &Path) {
        let candidates: Vec<_> = self
            .inputs
            .list(self.root, path)
            .into_iter()
            .filter(|candidate| candidate.extension().is_some_and(|ext| ext == "txt"))
            .collect();
        // Unreferenced files in this conventional directory are fallback
        // roots. First remove files reached with constraint semantics.
        let mut constraints = BTreeSet::new();
        for candidate in &candidates {
            if let Some(source) = self.inputs.read(self.root, candidate, true) {
                self.find_constraints(candidate, &source, &mut constraints, false, 0);
            }
        }
        for candidate in candidates {
            if !constraints.contains(&candidate) {
                self.read(&candidate, 0);
            }
        }
    }

    fn find_constraints(
        &mut self,
        path: &Path,
        source: &str,
        constraints: &mut BTreeSet<PathBuf>,
        is_constraint: bool,
        depth: usize,
    ) {
        if depth >= MAX_INCLUDE_DEPTH {
            self.inputs.problem(
                "unresolved_dependency_input",
                path,
                None,
                "constraint include depth limit",
            );
            return;
        }
        for (line, text) in logical_lines(source) {
            let spec = strip_comment(text.trim());
            let argument = include(spec, "-c", "--constraint").or_else(|| {
                is_constraint
                    .then(|| include(spec, "-r", "--requirement"))
                    .flatten()
            });
            if let Some(argument) = argument
                && let Some(target) = self.resolve(path, Some(line), argument)
                && constraints.insert(target.clone())
                && let Some(source) = self.inputs.read(self.root, &target, false)
            {
                self.find_constraints(&target, &source, constraints, true, depth + 1);
            }
        }
    }

    fn read(&mut self, path: &Path, depth: usize) {
        if depth > MAX_INCLUDE_DEPTH || self.active.contains(path) {
            self.inputs.problem(
                "unresolved_dependency_input",
                path,
                None,
                "requirements include cycle or depth limit",
            );
            return;
        }
        if !self.visited.insert(path.to_owned()) {
            return;
        }
        let Some(source) = self.inputs.read(self.root, path, true) else {
            return;
        };
        self.active.insert(path.to_owned());
        if is_compiled(&source) {
            self.read_lock_sources(path, &source, depth);
        } else {
            for (line, text) in logical_lines(&source) {
                let spec = strip_comment(text.trim());
                if let Some(argument) = include(spec, "-r", "--requirement") {
                    if let Some(target) = self.resolve(path, Some(line), argument) {
                        self.read(&target, depth + 1);
                    }
                } else if spec.is_empty()
                    || include(spec, "-c", "--constraint").is_some()
                    || pip_option(spec)
                {
                    continue;
                } else {
                    let spec = spec.split_once(" --hash=").map_or(spec, |(spec, _)| spec);
                    self.inventory.add(spec, path, line, self.inputs);
                }
            }
        }
        self.active.remove(path);
    }

    fn read_lock_sources(&mut self, path: &Path, source: &str, depth: usize) {
        let mut named = BTreeSet::new();
        for (index, raw) in source.lines().enumerate() {
            let comment = raw.trim().strip_prefix('#').map(str::trim);
            if let Some(text) = comment {
                let text = text.strip_prefix("via ").unwrap_or(text);
                if let Some(name) = include(text, "-r", "--requirement") {
                    if let Some(target) = self.resolve(path, Some(index + 1), name) {
                        named.insert(target);
                    }
                } else if text.contains("pip-compile") || text.contains("uv pip compile") {
                    // Tool headers may name inputs even when no lock entry has a via comment.
                    for word in shlex::split(text).into_iter().flatten() {
                        if word.ends_with(".in")
                            && let Some(target) = self.resolve(path, Some(index + 1), &word)
                        {
                            named.insert(target);
                        }
                    }
                }
            }
        }
        if named.is_empty() && path.extension().is_none_or(|extension| extension != "in") {
            named.insert(path.with_extension("in"));
        }
        if named.is_empty() {
            self.inputs.problem(
                "unresolved_dependency_input",
                path,
                None,
                "compiled requirements input could not be identified",
            );
        }
        for target in named {
            self.read(&target, depth + 1);
        }
    }

    fn resolve(&mut self, origin: &Path, line: Option<usize>, raw: &str) -> Option<PathBuf> {
        let trimmed = raw.trim();
        let candidate = trimmed
            .strip_prefix('"')
            .and_then(|value| value.strip_suffix('"'))
            .or_else(|| {
                trimmed
                    .strip_prefix('\'')
                    .and_then(|value| value.strip_suffix('\''))
            })
            .unwrap_or(trimmed);
        if let Some(path) = contained(self.root, origin, candidate) {
            return Some(path);
        }
        self.inputs.problem(
            "unresolved_dependency_input",
            origin,
            line,
            "requirements input must be a literal path inside its manifest root",
        );
        None
    }
}

fn pip_option(text: &str) -> bool {
    let key = text.split(['=', ' ', '\t']).next().unwrap_or_default();
    matches!(
        key,
        "-i" | "-f"
            | "--index-url"
            | "--extra-index-url"
            | "--no-index"
            | "--find-links"
            | "--trusted-host"
            | "--require-hashes"
            | "--pre"
            | "--no-binary"
            | "--only-binary"
            | "--prefer-binary"
    )
}

fn is_compiled(text: &str) -> bool {
    text.lines()
        .filter_map(|line| line.trim().strip_prefix('#'))
        .any(|line| {
            let line = line.to_ascii_lowercase();
            line.contains("autogenerated by")
                && (line.contains("pip-compile") || line.contains("uv"))
                || line.contains("generated by pip-compile")
                || line.trim_start().starts_with("via -r ")
                || line.trim_start().starts_with("via -r=")
        })
}

fn include<'a>(text: &'a str, short: &str, long: &str) -> Option<&'a str> {
    for flag in [long, short] {
        if let Some(value) = text.strip_prefix(flag)
            && (flag == short || value.is_empty() || value.starts_with([' ', '\t', '=']))
        {
            return Some(value.trim_start_matches([' ', '\t', '=']));
        }
    }
    None
}

fn contained(root: &Path, origin: &Path, relative: &str) -> Option<PathBuf> {
    if relative.is_empty() || relative.contains(['$', '%', ':', '\0']) {
        return None;
    }
    let mut target = origin.parent()?.to_owned();
    for part in Path::new(relative).components() {
        match part {
            Component::Normal(name) => target.push(name),
            Component::ParentDir if target != root => {
                target.pop();
            }
            Component::CurDir => {}
            _ => return None,
        }
    }
    (target != root && target.starts_with(root)).then_some(target)
}

pub(super) fn logical_lines(source: &str) -> impl Iterator<Item = (usize, String)> {
    let mut lines = source.lines().enumerate();
    std::iter::from_fn(move || {
        let (index, first) = lines.next()?;
        let mut logical = String::new();
        let mut part = first.trim_end();
        while let Some(text) = part.strip_suffix('\\') {
            logical.push_str(text);
            logical.push(' ');
            let Some((_, continued)) = lines.next() else {
                logical.push('\\');
                return Some((index + 1, logical));
            };
            part = continued.trim();
        }
        logical.push_str(part);
        Some((index + 1, logical))
    })
}
