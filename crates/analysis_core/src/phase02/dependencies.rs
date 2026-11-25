//! Static inventory and usage evidence for Skylos's Python dependency rule.

mod manifests;
mod usage;
pub(super) use usage::{Evidence, is_command_file};

use std::collections::BTreeMap;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use serde::Serialize;

use super::AnalyzedFile;
use crate::Issue;
use crate::preview_source::{Project, Stamp, read_snapshot, reject_symlink_components};

pub(super) const RULE: &str = "SKY-U005";
const MAX_INPUTS: usize = 256;
const MAX_DIRECTORIES: usize = 512;
const MAX_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Serialize)]
pub(super) struct Finding {
    rule_id: &'static str,
    kind: &'static str,
    pub(super) severity: &'static str,
    #[serde(rename = "type")]
    finding_type: &'static str,
    pub(super) name: String,
    simple_name: String,
    value: &'static str,
    threshold: u8,
    pub(super) message: String,
    pub(super) file: String,
    #[serde(skip)]
    pub(super) relative_file: Option<String>,
    basename: String,
    pub(super) line: usize,
    col: usize,
}

impl Finding {
    fn new(name: String, declaration: &Declaration, root: &Path) -> Self {
        let located = declaration.path.starts_with(root);
        let (path, line) = if located {
            (declaration.path.as_path(), declaration.line)
        } else {
            (root, 0)
        };
        Self {
            rule_id: RULE,
            kind: "quality",
            severity: "MEDIUM",
            finding_type: "dependency",
            simple_name: name.clone(),
            message: format!(
                "Declared dependency '{name}' appears unused. No matching import found in any Python file."
            ),
            name,
            value: "unused",
            threshold: 0,
            file: display(path),
            relative_file: located
                .then(|| display(path.strip_prefix(root).expect("project declaration"))),
            basename: if located {
                path.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned()
            } else {
                String::new()
            },
            line,
            col: 0,
        }
    }
}

#[derive(Debug, Serialize)]
pub(super) struct Receipt {
    rule_id: &'static str,
    status: &'static str,
    complete: bool,
    reasons: Vec<Reason>,
    abstained_count: usize,
    abstained_dependencies: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    declarations: Option<DeclaredDependencies>,
}

#[derive(Debug, Serialize)]
struct DeclaredDependencies {
    dependencies: Vec<String>,
    project_name: Option<String>,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
struct Reason {
    kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    line: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
}

impl Reason {
    fn new(kind: &'static str, file: &Path, line: Option<usize>, message: impl ToString) -> Self {
        Self {
            kind,
            file: Some(display(file)),
            line,
            message: Some(message.to_string()),
        }
    }

    fn issue(&self) -> Issue {
        let mut issue = Issue::new(
            self.kind,
            self.file.as_deref().unwrap_or("."),
            self.message
                .as_deref()
                .unwrap_or("Python dependency analysis did not complete"),
        );
        issue.line = self.line;
        issue
    }
}

pub(super) struct Result {
    pub(super) findings: Vec<Finding>,
    pub(super) receipt: Receipt,
    pub(super) evidence: Vec<usage::Evidence>,
}

impl Result {
    pub(super) fn not_run(kind: &'static str) -> Self {
        Self {
            findings: Vec::new(),
            receipt: Receipt {
                rule_id: RULE,
                status: "not_run",
                complete: false,
                reasons: vec![Reason {
                    kind,
                    file: None,
                    line: None,
                    message: None,
                }],
                abstained_count: 0,
                abstained_dependencies: Vec::new(),
                declarations: None,
            },
            evidence: Vec::new(),
        }
    }

    pub(super) fn issues(&self) -> impl Iterator<Item = Issue> + '_ {
        let requested =
            !matches!(self.receipt.reasons.as_slice(), [reason] if reason.kind == "rule_ignored");
        self.receipt
            .reasons
            .iter()
            .filter(move |_| requested)
            .map(Reason::issue)
    }
}

struct Declaration {
    path: PathBuf,
    line: usize,
}

pub(super) fn scan(
    path: &Path,
    project: &Project,
    files: &[AnalyzedFile],
    selection: &super::config::Selection,
    static_complete: bool,
    ignored: bool,
    diff_sources: Option<&mut BTreeMap<String, String>>,
) -> Result {
    if ignored {
        return Result::not_run("rule_ignored");
    }
    if path.is_file() {
        return Result::not_run("file_scoped_scan");
    }
    if !static_complete {
        return Result::not_run("incomplete_python_sources");
    }
    let root = project.base();
    let mut inputs = Inputs::default();
    let declared = manifests::Inventory::read(root, files, &mut inputs);
    let mut usage = usage::Usage::default();
    if files.is_empty() || !declared.dependencies.is_empty() {
        let source_count = usage.read(project, files, selection, &mut inputs);
        if source_count == 0 && inputs.reasons.is_empty() {
            return Result::not_run("no_project_python_sources");
        }
        usage.resolve_providers(declared.dependencies.keys());
    }
    inputs.verify();
    if let Some(diff_sources) = diff_sources {
        diff_sources.extend(inputs.files.iter().filter_map(|(path, (_, source))| {
            let relative = path.strip_prefix(root).ok()?;
            Some((display(relative), source.as_deref()?.to_owned()))
        }));
    }
    let mut reasons = inputs.reasons;
    reasons.append(&mut usage.reasons);
    let globally_incomplete = !reasons.is_empty();
    let mut findings = Vec::new();
    let mut evidence = Vec::new();
    let mut abstained = Vec::new();
    let dependencies = declared.dependencies.keys().cloned().collect();

    for (name, declaration) in declared.dependencies {
        if declared.project_name.as_deref() == Some(&name) {
            continue;
        }
        let decision = usage.for_distribution(&name);
        if !decision.evidence.is_empty() {
            evidence.extend(decision.evidence);
        } else if usage::exempt(&name) {
            continue;
        } else if globally_incomplete || !decision.reasons.is_empty() {
            abstained.push(name);
            reasons.extend(decision.reasons);
        } else {
            findings.push(Finding::new(name, &declaration, root));
        }
    }
    reasons.sort();
    reasons.dedup();
    evidence.sort();
    evidence.dedup();
    let complete = reasons.is_empty();
    Result {
        findings,
        receipt: Receipt {
            rule_id: RULE,
            status: if complete { "complete" } else { "incomplete" },
            complete,
            reasons,
            abstained_count: abstained.len(),
            abstained_dependencies: abstained,
            declarations: Some(DeclaredDependencies {
                dependencies,
                project_name: declared.project_name,
            }),
        },
        evidence,
    }
}

#[derive(Default)]
struct Inputs {
    files: BTreeMap<PathBuf, (Option<Stamp>, Option<Rc<str>>)>,
    directories: BTreeMap<PathBuf, Vec<PathBuf>>,
    reasons: Vec<Reason>,
    bytes: usize,
}

impl Inputs {
    fn read(&mut self, boundary: &Path, path: &Path, required: bool) -> Option<Rc<str>> {
        if !self.files.contains_key(path) {
            if self.files.len() >= MAX_INPUTS || self.bytes >= MAX_BYTES {
                self.problem(
                    "dependency_input_limit",
                    boundary,
                    None,
                    "dependency input budget exceeded",
                );
                return None;
            }
            if let Err(error) = reject_symlink_components(boundary, path) {
                self.problem("unresolved_dependency_input", path, None, error);
                return None;
            }
            let (stamp, content) = match fs::symlink_metadata(path) {
                Ok(metadata) => {
                    let stamp = Stamp::new(&metadata);
                    let content = read_snapshot(path, &display(path), Some(&stamp))
                        .map_err(|issue| issue.message)
                        .and_then(|(bytes, _)| {
                            self.bytes += bytes.len();
                            if self.bytes > MAX_BYTES {
                                return Err("dependency input byte budget exceeded".into());
                            }
                            String::from_utf8(bytes).map_err(|error| error.to_string())
                        });
                    match content {
                        Ok(source) => (Some(stamp), Some(Rc::from(source))),
                        Err(error) => {
                            self.problem("unresolved_dependency_input", path, None, error);
                            return None;
                        }
                    }
                }
                Err(error) if error.kind() == ErrorKind::NotFound => (None, None),
                Err(error) => {
                    self.problem("unresolved_dependency_input", path, None, error);
                    return None;
                }
            };
            self.files.insert(path.to_owned(), (stamp, content));
        }

        let content = self.files[path].1.clone();
        if required && content.is_none() {
            self.problem(
                "unresolved_dependency_input",
                path,
                None,
                "required dependency input is missing",
            );
        }
        content
    }

    fn list(&mut self, boundary: &Path, path: &Path) -> Vec<PathBuf> {
        if let Some(paths) = self.directories.get(path) {
            return paths.clone();
        }
        if self.directories.len() >= MAX_DIRECTORIES {
            self.problem(
                "dependency_input_limit",
                boundary,
                None,
                "dependency directory budget exceeded",
            );
            return Vec::new();
        }
        if let Err(error) = reject_symlink_components(boundary, path) {
            self.problem("unresolved_dependency_input", path, None, error);
            return Vec::new();
        }
        match list(path) {
            Ok(entries) => {
                self.directories.insert(path.to_owned(), entries.clone());
                entries
            }
            Err(error) => {
                self.problem("unresolved_dependency_input", path, None, error);
                Vec::new()
            }
        }
    }

    fn verify(&mut self) {
        for (path, (stamp, _)) in &self.files {
            let unchanged = match fs::symlink_metadata(path) {
                Ok(metadata) => stamp.as_ref() == Some(&Stamp::new(&metadata)),
                Err(error) => stamp.is_none() && error.kind() == ErrorKind::NotFound,
            };
            if !unchanged {
                self.reasons.push(Reason::new(
                    "dependency_input_changed",
                    path,
                    None,
                    "dependency input changed during analysis",
                ));
            }
        }
        for (path, entries) in &self.directories {
            if !list(path).is_ok_and(|current| current == *entries) {
                self.reasons.push(Reason::new(
                    "dependency_input_changed",
                    path,
                    None,
                    "dependency input directory changed during analysis",
                ));
            }
        }
    }

    fn problem(
        &mut self,
        kind: &'static str,
        path: &Path,
        line: Option<usize>,
        message: impl ToString,
    ) {
        self.reasons.push(Reason::new(kind, path, line, message));
    }
}

fn list(path: &Path) -> std::io::Result<Vec<PathBuf>> {
    let entries = match fs::read_dir(path) {
        Ok(entries) => entries,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut paths = entries
        .take(MAX_INPUTS + 1)
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<std::io::Result<Vec<_>>>()?;
    if paths.len() > MAX_INPUTS {
        return Err(std::io::Error::other(
            "dependency directory entry budget exceeded",
        ));
    }
    paths.sort();
    Ok(paths)
}

fn source_line(source: &str, offset: usize) -> usize {
    source.as_bytes()[..offset]
        .iter()
        .filter(|&&b| b == b'\n')
        .count()
        + 1
}

fn display(path: &Path) -> String {
    let label = path
        .to_string_lossy()
        .replace(std::path::MAIN_SEPARATOR, "/");
    #[cfg(windows)]
    if let Some(std::path::Component::Prefix(prefix)) = path.components().next() {
        match prefix.kind() {
            std::path::Prefix::VerbatimDisk(_) => {
                if let Some(disk) = label.strip_prefix("//?/") {
                    return disk.to_owned();
                }
            }
            std::path::Prefix::VerbatimUNC(_, _) => {
                if let Some(network) = label.get("//?/UNC/".len()..) {
                    return format!("//{network}");
                }
            }
            _ => {}
        }
    }
    label
}

fn normalize(name: &str) -> String {
    let mut normalized = String::new();
    for ch in name.chars() {
        if matches!(ch, '-' | '_' | '.') {
            if !normalized.ends_with('-') {
                normalized.push('-');
            }
        } else {
            normalized.push(ch.to_ascii_lowercase());
        }
    }
    normalized
}

fn is_module(name: &str) -> bool {
    name.split('.').all(skarmory_python::simple_name)
}

fn module_reference(value: &str) -> Option<&str> {
    let value = value.trim().split([':', '[']).next()?.trim();
    is_module(value).then_some(value)
}

fn configured_strings(value: &toml::Value, visit: &mut impl FnMut(&str)) {
    match value {
        toml::Value::String(value) => visit(value),
        toml::Value::Array(values) => {
            for value in values {
                configured_strings(value, visit);
            }
        }
        toml::Value::Table(table) => {
            for value in table.values() {
                configured_strings(value, visit);
            }
        }
        _ => {}
    }
}

fn ini_entries(source: &str) -> impl Iterator<Item = (&str, &str, &str, usize, bool)> {
    let mut section = "";
    let mut option = None;
    source.lines().enumerate().filter_map(move |(index, raw)| {
        let line = raw.trim();
        if line.is_empty() || line.starts_with(['#', ';']) {
            return None;
        }
        if let Some(header) = line.strip_prefix('[').and_then(|line| line.split_once(']')) {
            section = header.0.trim();
            option = None;
            return None;
        }
        let indent = raw.len() - raw.trim_start().len();
        if let Some((name, previous)) = option
            && indent > previous
        {
            return Some((section, name, line, index + 1, true));
        }
        let (key, value) = line.split_once(['=', ':'])?;
        option = Some((key.trim(), indent));
        Some((section, key.trim(), value.trim(), index + 1, false))
    })
}
