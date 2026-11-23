//! Bounded native Python dead-code analysis and Skylos 5.7.0 reporting.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Serialize;
use serde_json::{Value, json};
use skarmory_python::phase02::{ModuleFacts, extract};

use crate::fact_cache::{CacheStats, FactCache};
use crate::grep_verification::{self, Evidence as VerificationEvidence};
use crate::preview_source::{self, Exclusion, FileEntry};
use crate::{Coverage, Issue};

mod analysis;
pub(crate) mod config;
mod package_entrypoints;
mod runtime_evidence;
pub use runtime_evidence::Input as EvidenceInput;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Profile {
    #[default]
    SkylosPhase02,
    SkylosPhase03,
}

#[derive(Clone, Debug)]
pub struct Options {
    pub profile: Profile,
    pub grep_verify: Option<bool>,
    pub trace_file: EvidenceInput,
    pub coverage_file: EvidenceInput,
    pub config_file: Option<PathBuf>,
    pub excludes: Vec<String>,
    pub includes: Vec<String>,
    pub no_default_excludes: bool,
    pub include_generated: Option<bool>,
    pub generated_keep_paths: Vec<String>,
    pub confidence: Option<u8>,
    pub category: Option<String>,
    pub all_advice: bool,
    pub workers: usize,
    pub cache_dir: Option<PathBuf>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            profile: Profile::default(),
            grep_verify: None,
            trace_file: EvidenceInput::Default,
            coverage_file: EvidenceInput::Default,
            config_file: None,
            excludes: Vec::new(),
            includes: Vec::new(),
            no_default_excludes: false,
            include_generated: None,
            generated_keep_paths: Vec::new(),
            confidence: None,
            category: None,
            all_advice: true,
            workers: 1,
            cache_dir: None,
        }
    }
}

#[derive(Debug)]
pub struct ConfigError(String);

impl ConfigError {
    fn new(message: impl ToString) -> Self {
        Self(message.to_string())
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, output: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(output)
    }
}

impl std::error::Error for ConfigError {}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum Category {
    Function,
    Class,
    Import,
    Variable,
    Parameter,
    File,
}

impl Category {
    fn as_str(self) -> &'static str {
        match self {
            Self::Function => "function",
            Self::Class => "class",
            Self::Import => "import",
            Self::Variable => "variable",
            Self::Parameter => "parameter",
            Self::File => "file",
        }
    }
}

#[derive(Clone, Debug)]
struct Finding {
    category: Category,
    name: String,
    full_name: String,
    file: String,
    line: usize,
    column: usize,
    confidence: u8,
    rule_id: &'static str,
    reason: &'static str,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
struct Evidence {
    file: String,
    line: usize,
    full_name: String,
    kind: &'static str,
    certainty: &'static str,
}

#[derive(Default)]
struct Analysis {
    findings: Vec<Finding>,
    evidence: Vec<Evidence>,
    issues: Vec<Issue>,
    uncertainties: Vec<Issue>,
    verification: Vec<VerificationEvidence>,
    rescued_count: usize,
    runtime_evidence: Vec<runtime_evidence::Evidence>,
    runtime_evidence_issues: Vec<Issue>,
}

struct AnalyzedFile {
    path: String,
    module: String,
    is_package: bool,
    facts: ModuleFacts,
}

#[derive(Debug, Serialize)]
struct Diagnostic {
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    simple_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    full_name: Option<String>,
    #[serde(rename = "type")]
    kind: &'static str,
    file: String,
    basename: String,
    line: usize,
    column: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    confidence: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    severity: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    category: Option<&'static str>,
    rule_id: &'static str,
    reason: String,
    dead_code_disposition: &'static str,
    dead_code_evidence: Vec<DecisionEvidence>,
    #[serde(skip)]
    sarif_column: Option<usize>,
}

#[derive(Debug, Serialize)]
struct DecisionEvidence {
    kind: &'static str,
    role: &'static str,
    reason: &'static str,
    details: Value,
}

impl Diagnostic {
    fn new(
        finding: Finding,
        reason: String,
        disposition: &'static str,
        source: Option<&str>,
    ) -> Self {
        let empty_file = finding.category == Category::File;
        Self {
            simple_name: (!empty_file).then(|| finding.name.clone()),
            name: (!empty_file).then_some(finding.name),
            full_name: (!empty_file).then_some(finding.full_name),
            kind: finding.category.as_str(),
            basename: finding.file.rsplit('/').next().unwrap_or("").into(),
            file: finding.file,
            line: finding.line,
            column: finding.column,
            confidence: (!empty_file).then_some(finding.confidence),
            severity: empty_file.then_some("LOW"),
            message: empty_file.then_some("Empty Python file (no code, or docstring-only)"),
            category: empty_file.then_some("DEAD_CODE"),
            rule_id: finding.rule_id,
            reason,
            dead_code_disposition: disposition,
            dead_code_evidence: vec![DecisionEvidence {
                kind: finding.reason,
                role: "supports_dead",
                reason: "no use found in the supported analysis scope",
                details: json!({ "confidence": finding.confidence }),
            }],
            sarif_column: source
                .and_then(|source| sarif_column(source, finding.line, finding.column)),
        }
    }

    fn subject(&self) -> &str {
        self.full_name.as_deref().unwrap_or(&self.file)
    }
}

#[derive(Default, Debug, Serialize)]
struct Categories {
    unused_functions: Vec<Diagnostic>,
    unused_classes: Vec<Diagnostic>,
    unused_imports: Vec<Diagnostic>,
    unused_variables: Vec<Diagnostic>,
    unused_parameters: Vec<Diagnostic>,
    unused_files: Vec<Diagnostic>,
}

impl Categories {
    fn get_mut(&mut self, category: Category) -> &mut Vec<Diagnostic> {
        match category {
            Category::Function => &mut self.unused_functions,
            Category::Class => &mut self.unused_classes,
            Category::Import => &mut self.unused_imports,
            Category::Variable => &mut self.unused_variables,
            Category::Parameter => &mut self.unused_parameters,
            Category::File => &mut self.unused_files,
        }
    }

    fn iter(&self) -> impl Iterator<Item = &Diagnostic> {
        [
            &self.unused_functions,
            &self.unused_classes,
            &self.unused_imports,
            &self.unused_variables,
            &self.unused_parameters,
            &self.unused_files,
        ]
        .into_iter()
        .flatten()
    }
}

#[derive(Debug, Serialize)]
struct Summary {
    total_files: usize,
    analysis_error_count: usize,
    coverage_status: &'static str,
    languages: BTreeMap<&'static str, usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    grep_verify: Option<VerificationSummary>,
}

#[derive(Debug, Serialize)]
struct VerificationSummary {
    enabled: bool,
    rescued_count: usize,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum RetainedEvidence {
    Flat(Vec<Evidence>),
    Grouped { symbols: Vec<SymbolEvidence> },
}

#[derive(Debug, Serialize)]
struct SymbolEvidence {
    qualified_name: String,
    file: String,
    line: usize,
    evidence: Vec<SymbolEvent>,
}

#[derive(Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
struct SymbolEvent {
    kind: &'static str,
    certainty: &'static str,
}

impl RetainedEvidence {
    fn for_profile(
        profile: Profile,
        evidence: Vec<Evidence>,
        verification: &[VerificationEvidence],
    ) -> Self {
        if profile == Profile::SkylosPhase02 {
            return Self::Flat(evidence);
        }
        let mut symbols = BTreeMap::<_, BTreeSet<_>>::new();
        for event in evidence {
            symbols
                .entry((event.full_name, event.file, event.line))
                .or_default()
                .insert(SymbolEvent {
                    kind: match event.kind {
                        "runtime_trace" => "trace_hit",
                        "runtime_coverage" | runtime_evidence::COVERAGE_DECLARATION => {
                            "coverage_hit"
                        }
                        other => other,
                    },
                    certainty: event.certainty,
                });
        }
        for event in verification {
            symbols
                .entry((
                    event.qualified_name.clone(),
                    event.definition_path.clone(),
                    event.definition_line,
                ))
                .or_default()
                .insert(SymbolEvent {
                    kind: "grep_rescue",
                    certainty: event.certainty,
                });
        }
        Self::Grouped {
            symbols: symbols
                .into_iter()
                .map(|((qualified_name, file, line), evidence)| SymbolEvidence {
                    qualified_name,
                    file,
                    line,
                    evidence: evidence.into_iter().collect(),
                })
                .collect(),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct Report {
    schema_version: u8,
    profile: &'static str,
    selected_paths: Vec<String>,
    exclusions: Vec<Exclusion>,
    #[serde(skip_serializing_if = "BTreeSet::is_empty")]
    generated_sources: BTreeSet<String>,
    config_sources: Vec<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    file_config_sources: BTreeMap<String, Vec<String>>,
    config_warnings: Vec<String>,
    confidence_threshold: u8,
    all_advice: bool,
    coverage: Coverage,
    analysis_errors: Vec<Value>,
    analysis_summary: Summary,
    dead_code_evidence: RetainedEvidence,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    grep_verification_evidence: Vec<VerificationEvidence>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    runtime_evidence: Vec<runtime_evidence::Evidence>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    runtime_evidence_issues: Vec<Issue>,
    dead_code_abstentions: Vec<Issue>,
    suppressed: Vec<Diagnostic>,
    whitelisted: Vec<Diagnostic>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache: Option<CacheStats>,
    #[serde(flatten)]
    categories: Categories,
}

impl Report {
    pub fn is_complete(&self) -> bool {
        self.coverage.status == "complete"
    }

    pub fn has_findings(&self) -> bool {
        self.categories.iter().next().is_some()
    }

    pub fn to_json(&self) -> serde_json::Result<String> {
        serde_json::to_string(self)
    }

    pub fn to_json_strict(&self) -> serde_json::Result<String> {
        let mut result = serde_json::to_value(self)?;
        let issues = self.categories.iter().count();
        let (status, verdict, exit_code, reasons) = if !self.is_complete() {
            (
                "incomplete",
                "incomplete",
                2,
                vec!["Analysis incomplete".to_owned()],
            )
        } else if issues > 0 {
            (
                "evaluated",
                "failed",
                1,
                vec![format!(
                    "Strict mode: {issues} {} found",
                    if issues == 1 { "issue" } else { "issues" }
                )],
            )
        } else {
            ("evaluated", "passed", 0, Vec::new())
        };
        result["gate"] = json!({
            "passed": exit_code == 0,
            "reasons": reasons,
            "scope": "local_strict_policy",
            "status": status,
            "forced": false,
            "verdict": verdict,
            "exit_code": exit_code,
            "strict": true,
            "headline": match exit_code {
                0 => "Quality Gate: PASSED",
                1 => "Quality Gate: FAILED",
                _ => "Quality Gate: INCOMPLETE",
            },
            "hint": null,
            "reason_details": reasons.iter().map(|message| json!({
                "message": message,
                "details": self.categories.iter().map(|finding| format!(
                    "{}:{}  unused {} {}", finding.file, finding.line, finding.kind,
                    finding.name.as_deref().unwrap_or(&finding.basename)
                )).collect::<Vec<_>>(),
                "hint": null,
            })).collect::<Vec<_>>(),
            "coverage_status": self.coverage.status,
            "coverage_warnings": self.coverage.issues.iter().map(|issue| &issue.message).collect::<Vec<_>>(),
            "coverage_warning_details": self.coverage.issues,
        });
        serde_json::to_string(&result)
    }

    pub fn to_sarif(&self) -> serde_json::Result<String> {
        let results: Vec<_> = self.categories.iter().map(|item| {
            let mut region = json!({"startLine": item.line});
            if let Some(column) = item.sarif_column {
                region["startColumn"] = json!(column);
            }
            json!({
                "ruleId": item.rule_id,
                "level": "note",
                "message": {"text": format!("Unused {} {}", item.kind, item.subject())},
                "locations": [{"physicalLocation": {
                    "artifactLocation": {"uri": artifact_uri(&item.file)},
                    "region": region,
                }}],
                "properties": {"confidence": item.confidence, "disposition": item.dead_code_disposition},
            })
        }).collect();
        let rule_ids: BTreeSet<_> = self.categories.iter().map(|item| item.rule_id).collect();
        let notifications: Vec<_> = self
            .coverage
            .issues
            .iter()
            .map(|issue| {
                json!({
                    "level": "error",
                    "descriptor": {"id": "SKY-ANALYSIS-INCOMPLETE"},
                    "message": {"text": format!("{}: {}", issue.path, issue.message)},
                })
            })
            .collect();
        serde_json::to_string(&json!({
            "$schema": "https://json.schemastore.org/sarif-2.1.0.json",
            "version": "2.1.0",
            "runs": [{
                "tool": {"driver": {
                    "name": "Skarmory",
                    "version": crate::VERSION,
                    "informationUri": "https://github.com/eddiethedean/skarmory",
                    "rules": rule_ids.iter().map(|id| json!({"id": id})).collect::<Vec<_>>(),
                }},
                "columnKind": "utf16CodeUnits",
                "invocations": [{
                    "executionSuccessful": self.is_complete(),
                    "toolExecutionNotifications": notifications,
                }],
                "results": results,
                "properties": {"coverage": self.coverage},
            }],
        }))
    }
}

fn sarif_column(source: &str, line: usize, byte_col: usize) -> Option<usize> {
    let line = source.lines().nth(line.checked_sub(1)?)?;
    Some(line.get(..byte_col.checked_sub(1)?)?.encode_utf16().count() + 1)
}

fn artifact_uri(path: &str) -> String {
    use std::fmt::Write;
    let mut uri = String::new();
    if path.starts_with('/') {
        uri.push_str("file://");
    } else if path.as_bytes().get(1) == Some(&b':') {
        uri.push_str("file:///");
    }
    for byte in path.bytes() {
        if byte.is_ascii_alphanumeric()
            || matches!(byte, b'/' | b'-' | b'.' | b'_' | b'~')
            || (byte == b':' && uri.starts_with("file:///"))
        {
            uri.push(char::from(byte));
        } else {
            let _ = write!(uri, "%{byte:02X}");
        }
    }
    uri
}

fn validate_category(category: Option<&str>) -> Result<(), ConfigError> {
    let Some(value) = category else { return Ok(()) };
    let categories = value.split(',').map(str::trim);
    for category in categories {
        if !matches!(
            category.to_ascii_lowercase().as_str(),
            "dead_code" | "dead-code" | "deadcode" | "dead code"
        ) {
            return Err(ConfigError::new(format!(
                "unsupported category: {category}"
            )));
        }
    }
    Ok(())
}

struct Snapshot {
    facts: Result<ModuleFacts, skarmory_python::FrontendError>,
    source: String,
    hide_findings: bool,
}

pub fn scan(path: &Path, options: &Options) -> Result<Report, ConfigError> {
    validate_category(options.category.as_deref())?;
    let config = config::load(path, options)?;
    let phase03 = options.profile == Profile::SkylosPhase03;
    let grep_verify = options.grep_verify.unwrap_or(phase03);
    if !phase03
        && (!matches!(options.trace_file, EvidenceInput::Default)
            || !matches!(options.coverage_file, EvidenceInput::Default))
    {
        return Err(ConfigError::new(
            "runtime evidence options require the skylos-0.3 profile",
        ));
    }
    let roots = if path.is_dir() && path.join("src").is_dir() {
        vec![PathBuf::from("src"), PathBuf::from(".")]
    } else {
        Vec::new()
    };
    let cache = options.cache_dir.as_ref().map(|directory| {
        FactCache::for_project(
            directory,
            "phase02-module-v9/ruff-0.0.12/py311/utf8-universal",
            path,
            &roots,
        )
    });
    let mut project =
        preview_source::discover_filtered(path, &roots, FileEntry::None, Some(&config.selection));
    let mut issues = std::mem::take(&mut project.issues);
    let mut selected_paths = Vec::new();
    let mut generated_sources = BTreeSet::new();
    let mut sources = BTreeMap::new();
    let mut files = Vec::new();
    let mut stamps = Vec::new();
    let mut file_configs = BTreeMap::new();
    let mut directory_configs = BTreeMap::new();
    let mut local_configs = Vec::new();
    match project.map_file_sources(options.workers, |file, source| {
        let facts = match &cache {
            Some(cache) => cache.get_or_extract(source, extract),
            None => extract(source),
        };
        Snapshot {
            facts,
            source: source.to_owned(),
            hide_findings: !path.is_file()
                && config
                    .selection
                    .suppress_generated_findings(&file.label, source),
        }
    }) {
        Ok(work) => {
            for (file, work) in project.files.iter().zip(work) {
                stamps.push(work.stamp);
                selected_paths.push(file.label.clone());
                match work.result {
                    Ok(Snapshot {
                        facts: Ok(facts),
                        source,
                        hide_findings,
                    }) => {
                        if hide_findings {
                            generated_sources.insert(file.label.clone());
                        }
                        let directory = file.directory();
                        let config_index = *directory_configs
                            .entry(directory.to_owned())
                            .or_insert_with(|| match config.for_path(directory, options) {
                                Ok(local) if config.same_layers(&local) => None,
                                Ok(local) => {
                                    let index = local_configs.len();
                                    local_configs.push(local);
                                    Some(index)
                                }
                                Err(error) => {
                                    issues.push(Issue::new(
                                        "configuration_error",
                                        &file.label,
                                        error,
                                    ));
                                    None
                                }
                            });
                        if let Some(index) = config_index {
                            file_configs.insert(file.label.clone(), index);
                        }
                        sources.insert(file.label.clone(), source);
                        files.push(AnalyzedFile {
                            path: file.label.clone(),
                            module: file.name.clone(),
                            is_package: Path::new(&file.label).file_name()
                                == Some(OsStr::new("__init__.py")),
                            facts,
                        });
                    }
                    Ok(Snapshot {
                        facts: Err(error), ..
                    }) => issues.push(
                        Issue::new(error.kind, &file.label, error.message).at(error.location),
                    ),
                    Err(issue) => issues.push(issue),
                }
            }
        }
        Err(issue) => {
            selected_paths.extend(project.files.iter().map(|file| file.label.clone()));
            issues.push(issue);
        }
    }

    let package_entrypoints =
        (phase03 && issues.is_empty()).then(|| package_entrypoints::Snapshot::read(path, &files));
    let available_package_entrypoints = package_entrypoints
        .as_ref()
        .filter(|metadata| metadata.issues.is_empty());
    let runtime = (phase03 && issues.is_empty()).then(|| {
        runtime_evidence::Snapshot::read(
            path,
            &project,
            &sources,
            &stamps,
            &options.trace_file,
            &options.coverage_file,
        )
    });
    let verification = (grep_verify && issues.is_empty())
        .then(|| grep_verification::Snapshot::read(&project, Some(&config.selection)));
    let mut analysis = if issues.is_empty() {
        if local_configs.is_empty() {
            analysis::analyze_with_verification(
                &files,
                &config.entrypoints,
                verification.as_ref(),
                available_package_entrypoints,
                runtime.as_ref(),
            )
        } else {
            let mut groups = vec![BTreeSet::new(); local_configs.len()];
            let mut root_paths = BTreeSet::new();
            for file in &files {
                if let Some(index) = file_configs.get(&file.path) {
                    groups[*index].insert(file.path.clone());
                } else {
                    root_paths.insert(file.path.clone());
                }
            }
            let mut entrypoints = Vec::new();
            for (config, paths) in
                std::iter::once((&config, root_paths)).chain(local_configs.iter().zip(groups))
            {
                let paths = Arc::new(paths);
                entrypoints.extend(config.entrypoints.iter().map(|item| item.scoped(&paths)));
            }
            analysis::analyze_with_verification(
                &files,
                &entrypoints,
                verification.as_ref(),
                available_package_entrypoints,
                runtime.as_ref(),
            )
        }
    } else {
        Analysis::default()
    };
    if let Some(mut runtime) = runtime {
        analysis.runtime_evidence_issues.append(&mut runtime.issues);
        analysis.runtime_evidence_issues.sort();
        analysis.runtime_evidence_issues.dedup();
    }
    issues.append(&mut analysis.issues);
    if let Some(verification) = verification {
        issues.extend(verification.finish(&project, Some(&config.selection)));
    }
    if let Some(mut metadata) = package_entrypoints {
        issues.append(&mut metadata.issues);
        issues.extend(metadata.verify());
    }
    issues.extend(project.verify(stamps));
    if !config.unchanged(path, options)
        || directory_configs.iter().any(|(directory, index)| {
            let effective = index.map_or(&config, |index| &local_configs[index]);
            !effective.unchanged(directory, options)
        })
    {
        issues.push(Issue::new(
            "configuration_changed",
            ".",
            "configuration changed during analysis",
        ));
    }
    issues.sort();
    issues.dedup();
    analysis.uncertainties.sort();
    analysis.uncertainties.dedup();
    analysis.evidence.sort();
    analysis.evidence.dedup();
    analysis.verification.sort();
    analysis.verification.dedup();
    project
        .exclusions
        .sort_by(|left, right| left.path.cmp(&right.path));
    let status = if issues.is_empty() {
        "complete"
    } else {
        "incomplete"
    };

    let mut categories = Categories::default();
    let mut suppressed = Vec::new();
    let mut whitelisted = Vec::new();
    analysis.findings.sort_by(|left, right| {
        (
            &left.file,
            left.line,
            left.column,
            &left.full_name,
            left.category,
        )
            .cmp(&(
                &right.file,
                right.line,
                right.column,
                &right.full_name,
                right.category,
            ))
    });
    let file_facts: BTreeMap<_, _> = files
        .iter()
        .map(|file| (file.path.as_str(), &file.facts))
        .collect();
    if status == "complete" {
        for mut finding in analysis.findings {
            if generated_sources.contains(&finding.file) {
                continue;
            }
            let source = sources.get(&finding.file).map(String::as_str);
            let inline = file_facts.get(finding.file.as_str()).is_some_and(|facts| {
                facts.inline_ignores.iter().any(|entry| {
                    entry.line == finding.line
                        && (entry.rules.is_empty()
                            || entry
                                .rules
                                .iter()
                                .any(|rule| rule.eq_ignore_ascii_case(finding.rule_id)))
                })
            });
            if inline {
                suppressed.push(Diagnostic::new(
                    finding,
                    "inline ignore comment".into(),
                    "suppressed",
                    source,
                ));
                continue;
            }
            let effective = file_configs
                .get(&finding.file)
                .map_or(&config, |index| &local_configs[*index]);
            match effective.decision(&finding) {
                config::Decision::Suppress(reason) => {
                    suppressed.push(Diagnostic::new(finding, reason, "suppressed", source));
                    continue;
                }
                config::Decision::Whitelist(reason) => {
                    whitelisted.push(Diagnostic::new(finding, reason, "whitelisted", source));
                    continue;
                }
                config::Decision::Lower(amount) => {
                    finding.confidence = finding.confidence.saturating_sub(amount)
                }
                config::Decision::Keep => {}
            }
            // SKY-E002 is a file-level policy rather than a scored binding.
            if finding.category == Category::File || finding.confidence >= config.confidence {
                categories.get_mut(finding.category).push(Diagnostic::new(
                    finding,
                    "no use in the declared source scope".into(),
                    "reported",
                    source,
                ));
            }
        }
    }

    let analysis_errors = issues
        .iter()
        .map(|issue| {
            json!({
                "rule_id": "SKY-ANALYSIS-INCOMPLETE",
                "file": issue.path,
                "line": issue.line,
                "column": issue.column,
                "kind": issue.kind,
                "message": issue.message,
            })
        })
        .collect();
    let file_config_sources = file_configs
        .into_iter()
        .map(|(path, index)| (path, local_configs[index].sources.clone()))
        .collect();
    let config_warnings = std::iter::once(&config)
        .chain(&local_configs)
        .flat_map(|effective| effective.warnings.iter().cloned())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    Ok(Report {
        schema_version: if phase03 { 2 } else { 1 },
        profile: if phase03 {
            "skylos-5.7.0-python-dead-code-phase03"
        } else {
            "skylos-5.7.0-python-dead-code"
        },
        analysis_summary: Summary {
            total_files: selected_paths.len(),
            analysis_error_count: issues.len(),
            coverage_status: status,
            languages: BTreeMap::from([("Python", selected_paths.len())]),
            grep_verify: (phase03 || grep_verify).then_some(VerificationSummary {
                enabled: grep_verify,
                rescued_count: analysis.rescued_count,
            }),
        },
        coverage: Coverage {
            status,
            selected_files: selected_paths.len(),
            extracted_files: files.len(),
            issues,
        },
        selected_paths,
        exclusions: project.exclusions,
        generated_sources,
        config_sources: config.sources,
        file_config_sources,
        config_warnings,
        confidence_threshold: config.confidence,
        all_advice: options.all_advice,
        analysis_errors,
        dead_code_evidence: RetainedEvidence::for_profile(
            options.profile,
            analysis.evidence,
            &analysis.verification,
        ),
        grep_verification_evidence: analysis.verification,
        runtime_evidence: analysis.runtime_evidence,
        runtime_evidence_issues: analysis.runtime_evidence_issues,
        dead_code_abstentions: analysis.uncertainties,
        suppressed,
        whitelisted,
        cache: cache.as_ref().map(FactCache::stats),
        categories,
    })
}
