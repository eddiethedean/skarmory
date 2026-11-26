//! Bounded native dead-code analysis and Skylos 5.7.0 reporting.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Serialize;
use serde_json::{Value, json};
use skarmory_python::Location;
use skarmory_python::phase02::{ModuleFacts, extract, extract_with_project_wildcards};

use crate::fact_cache::{CacheStats, FactCache};
use crate::grep_verification::{self, Evidence as VerificationEvidence};
use crate::preview_source::{self, Exclusion, FileEntry};
use crate::{Coverage, Issue};

mod analysis;
pub mod cleanup;
pub(crate) mod config;
mod dependencies;
mod external_classes;
mod javascript;
mod package_entrypoints;
mod runtime_evidence;
pub use runtime_evidence::Input as EvidenceInput;
mod wildcards;
pub mod workflow;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Profile {
    #[default]
    SkylosPhase02,
    SkylosPhase03,
}

#[derive(Clone, Debug)]
pub struct Options {
    pub profile: Profile,
    pub experimental_javascript: bool,
    pub grep_verify: Option<bool>,
    /// Ordered external dependency source paths for the Phase 0.3 model.
    pub dependency_search_paths: Vec<PathBuf>,
    /// None uses the Phase 0.3 compatibility configuration.
    pub enable_quality: Option<bool>,
    pub quality_only: bool,
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
            experimental_javascript: false,
            grep_verify: None,
            dependency_search_paths: Vec::new(),
            enable_quality: Some(false),
            quality_only: false,
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
    identity_name: String,
    file: String,
    line: usize,
    column: usize,
    method_owner: Option<[usize; 2]>,
    declaration_whitelist: Option<&'static str>,
    confidence: u8,
    rule_id: &'static str,
    reason: &'static str,
}

const WEAK_HELPER_FUNCTION: &str = "weak_helper_function";

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
    definitions: BTreeMap<String, DefinitionContext>,
    findings: Vec<Finding>,
    evidence: Vec<Evidence>,
    fastapi_route_declarations: Vec<FastapiRouteDeclaration>,
    #[cfg(test)]
    fastapi_initializer_evidence: FastapiInitializerEvidence,
    issues: Vec<Issue>,
    uncertainties: Vec<Issue>,
    verification: Vec<VerificationEvidence>,
    rescued_count: usize,
    runtime_evidence: Vec<runtime_evidence::Evidence>,
    runtime_evidence_issues: Vec<Issue>,
}

#[cfg(test)]
#[derive(Default)]
struct FastapiInitializerEvidence {
    call_events: usize,
    limited: bool,
    callback_parameters: usize,
    injected_parameters: usize,
}

#[derive(Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
struct FastapiRouteDeclaration {
    file: String,
    line: usize,
    qualified_name: String,
}

#[derive(Debug, Serialize)]
struct DefinitionContext {
    name: String,
    #[serde(rename = "type")]
    kind: &'static str,
    file: String,
    line: usize,
    column: usize,
}

struct AnalyzedFile {
    path: String,
    module: String,
    is_package: bool,
    fastapi_router_mode: config::FastapiRouterMode,
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
    confidence_factors: Option<[&'static str; 1]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    category: Option<&'static str>,
    rule_id: &'static str,
    reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    suppression_code: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    folder_role: Option<&'static str>,
    dead_code_disposition: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    dead_code_classification: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    change_status: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    report_scope: Option<&'static str>,
    #[serde(skip)]
    method_owner: Option<[usize; 2]>,
    #[serde(skip)]
    identity_name: String,
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
        let file = finding.category == Category::File;
        let javascript_file = finding.rule_id == javascript::RULE;
        let message = match (finding.rule_id, finding.reason) {
            (javascript::RULE, javascript::DEAD_IMPORTERS) => Some(
                "Unused TypeScript/JavaScript file (only imported by files that are themselves unused)",
            ),
            (javascript::RULE, _) => {
                Some("Unused TypeScript/JavaScript file (not imported by any other file)")
            }
            ("SKY-E002", _) => Some("Empty Python file (no code, or docstring-only)"),
            _ => None,
        };
        Self {
            simple_name: (!file).then(|| finding.name.clone()),
            name: (!file).then_some(finding.name),
            full_name: (!file).then_some(finding.full_name),
            kind: finding.category.as_str(),
            basename: finding.file.rsplit('/').next().unwrap_or("").into(),
            file: finding.file,
            line: finding.line,
            column: finding.column,
            confidence: (!file || javascript_file).then_some(finding.confidence),
            severity: file.then_some("LOW"),
            confidence_factors: javascript_file.then_some([finding.reason]),
            message,
            category: file.then_some("DEAD_CODE"),
            rule_id: finding.rule_id,
            reason,
            suppression_code: None,
            folder_role: None,
            dead_code_disposition: disposition,
            dead_code_classification: None,
            change_status: None,
            report_scope: None,
            identity_name: finding.identity_name,
            method_owner: finding.method_owner,
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

    fn group_unused_class_methods(&mut self, suppressed: &mut Vec<Diagnostic>) -> usize {
        let class_locations: BTreeMap<_, _> = self
            .unused_classes
            .iter()
            .map(|class| {
                (
                    (class.file.as_str(), [class.line, class.column]),
                    class.change_status,
                )
            })
            .collect();
        let before = suppressed.len();
        for mut method in std::mem::take(&mut self.unused_functions) {
            if method.method_owner.is_some_and(|owner| {
                class_locations
                    .get(&(method.file.as_str(), owner))
                    .is_some_and(|status| {
                        method.change_status.is_none() || *status == method.change_status
                    })
            }) {
                method.reason = "covered by the unused class finding".into();
                method.dead_code_disposition = "suppressed";
                method.dead_code_classification = Some("likely_dead");
                suppressed.push(method);
            } else {
                self.unused_functions.push(method);
            }
        }
        suppressed.len() - before
    }

    fn retain(&mut self, mut keep: impl FnMut(&mut Diagnostic) -> bool) {
        for group in [
            &mut self.unused_functions,
            &mut self.unused_classes,
            &mut self.unused_imports,
            &mut self.unused_variables,
            &mut self.unused_parameters,
            &mut self.unused_files,
        ] {
            group.retain_mut(&mut keep);
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
    #[serde(skip_serializing_if = "Vec::is_empty")]
    fastapi_route_declarations: Vec<FastapiRouteDeclaration>,
    #[serde(skip_serializing_if = "Option::is_none")]
    javascript_file_analysis: Option<JavaScriptFileAnalysis>,
    #[serde(skip_serializing_if = "Option::is_none")]
    grep_verify: Option<VerificationSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    dead_code_liveness: Option<LivenessSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    unused_dependency_analysis: Option<dependencies::Receipt>,
    #[serde(skip_serializing_if = "Option::is_none")]
    dead_code_evidence: Option<workflow::EvidenceSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pr_impact: Option<workflow::Impact>,
    #[serde(skip_serializing_if = "Option::is_none")]
    baseline: Option<workflow::BaselineSummary>,
}

#[derive(Debug, Serialize)]
struct JavaScriptFileAnalysis {
    rule_id: &'static str,
    status: &'static str,
    scope: &'static str,
}

#[derive(Debug, Serialize)]
struct VerificationSummary {
    enabled: bool,
    rescued_count: usize,
}

#[derive(Debug, Serialize)]
struct LivenessSummary {
    rescued: Vec<LivenessRescue>,
}

#[derive(Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
struct LivenessRescue {
    full_name: String,
    reason: &'static str,
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
    #[serde(skip_serializing_if = "Vec::is_empty")]
    external_dependency_roots: Vec<String>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    dead_code_evidence: Option<RetainedEvidence>,
    #[serde(skip_serializing_if = "Option::is_none")]
    definitions: Option<BTreeMap<String, DefinitionContext>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    grep_verification_evidence: Vec<VerificationEvidence>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    runtime_evidence: Vec<runtime_evidence::Evidence>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    runtime_evidence_issues: Vec<Issue>,
    dead_code_abstentions: Vec<Issue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    quality: Option<Vec<dependencies::Finding>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    dependency_usage_evidence: Vec<dependencies::Evidence>,
    suppressed: Vec<Diagnostic>,
    whitelisted: Vec<Diagnostic>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache: Option<CacheStats>,
    #[serde(skip_serializing_if = "Option::is_none")]
    gate: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    analysis_coverage: Option<workflow::Assessment>,
    #[serde(skip)]
    gate_policy: config::Gate,
    #[serde(flatten)]
    categories: Categories,
}

impl Report {
    fn group_unused_class_methods(&mut self) {
        if let Some(evidence) = &mut self.analysis_summary.dead_code_evidence {
            let grouped = self
                .categories
                .group_unused_class_methods(&mut self.suppressed);
            evidence.group_reports(grouped);
        }
    }

    pub fn is_complete(&self) -> bool {
        self.coverage.status == "complete"
    }

    pub fn has_findings(&self) -> bool {
        self.categories.iter().next().is_some()
            || self
                .quality
                .as_ref()
                .is_some_and(|quality| !quality.is_empty())
    }

    pub fn to_json(&self) -> serde_json::Result<String> {
        serde_json::to_string(self)
    }

    pub fn to_json_ci(&mut self) -> serde_json::Result<String> {
        let evidence = self.dead_code_evidence.take();
        let definitions = self.definitions.take();
        let result = self.to_json();
        self.dead_code_evidence = evidence;
        self.definitions = definitions;
        result
    }

    pub fn to_json_strict(&self) -> serde_json::Result<String> {
        let mut result = serde_json::to_value(self)?;
        let issues = self.categories.iter().count() + self.quality.iter().flatten().count();
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
                )).chain(self.quality.iter().flatten().map(|finding| format!(
                    "{}:{}  unused dependency {}", finding.file, finding.line, finding.name
                ))).collect::<Vec<_>>(),
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
            let mut properties = json!({"confidence": item.confidence, "disposition": item.dead_code_disposition});
            if let Some(classification) = item.dead_code_classification {
                properties["dead_code_classification"] = json!(classification);
                properties["dead_code_evidence"] = json!(item.dead_code_evidence);
            }
            if let Some(status) = item.change_status {
                properties["change_status"] = json!(status);
                properties["report_scope"] = json!(item.report_scope);
            }
            json!({
                "ruleId": item.rule_id,
                "level": "note",
                "message": {"text": format!("Unused {} {}", item.kind, item.subject())},
                "locations": [{"physicalLocation": {
                    "artifactLocation": {"uri": artifact_uri(&item.file)},
                    "region": region,
                }}],
                "properties": properties,
            })
        }).chain(self.quality.iter().flatten().map(|finding| {
            let mut result = json!({
                "ruleId": dependencies::RULE,
                "level": "warning",
                "message": {"text": finding.message},
            });
            if finding.line != 0 {
                result["locations"] = json!([{"physicalLocation": {
                    "artifactLocation": {"uri": artifact_uri(&finding.file)},
                    "region": {"startLine": finding.line},
                }}]);
            }
            result
        })).collect();
        let mut rule_ids: BTreeSet<_> = self.categories.iter().map(|item| item.rule_id).collect();
        if self
            .quality
            .as_ref()
            .is_some_and(|quality| !quality.is_empty())
        {
            rule_ids.insert(dependencies::RULE);
        }
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
        let mut properties = json!({"coverage": self.coverage});
        if let Some(coverage) = &self.analysis_coverage {
            properties["analysis_coverage"] = json!(coverage);
        }
        if let Some(impact) = &self.analysis_summary.pr_impact {
            properties["pr_impact"] = json!(impact);
        }
        if let Some(gate) = &self.gate {
            properties["gate"] = gate.clone();
        }
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
                "properties": properties,
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

fn is_test_only_path(file: &str) -> bool {
    let path = file.to_ascii_lowercase();
    let (directories, basename) = path.rsplit_once('/').unwrap_or(("", &path));
    let mut parent = "";
    for directory in directories.split('/') {
        // The reference uses the first non-library directory role.
        match directory {
            "test" | "tests" | "__tests__" => return true,
            "example" if matches!(parent, "com" | "org" | "net") => {}
            "example" | "examples" | "doc_src" | "docs_src" | "bench" | "benchmark"
            | "benchmarks" => return false,
            _ if ["_example", "_examples", "_benchmark", "_benchmarks"]
                .iter()
                .any(|suffix| directory.ends_with(suffix)) =>
            {
                return false;
            }
            _ => {}
        }
        parent = directory;
    }
    basename == "conftest.py"
        || basename.starts_with("test_")
        || basename.ends_with("_test.py")
        || basename.contains(".test.")
        || basename.contains(".spec.")
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
    facts: Result<FileFacts, Issue>,
    source: String,
    hide_findings: bool,
}

enum FileFacts {
    Python(ModuleFacts),
    JavaScript(javascript::Module),
}

pub fn scan(path: &Path, options: &Options) -> Result<Report, ConfigError> {
    let mut report = scan_with_context(path, options, None, None)?;
    report.group_unused_class_methods();
    Ok(report)
}

fn scan_with_context(
    path: &Path,
    options: &Options,
    policy: Option<&config::Policy>,
    diff_sources: Option<&mut BTreeMap<String, String>>,
) -> Result<Report, ConfigError> {
    scan_selected(path, options, policy, diff_sources, None)
}

fn scan_selected(
    path: &Path,
    options: &Options,
    policy: Option<&config::Policy>,
    mut diff_sources: Option<&mut BTreeMap<String, String>>,
    mut cleanup_inputs: Option<&mut cleanup::Inputs>,
) -> Result<Report, ConfigError> {
    validate_category(options.category.as_deref())?;
    let config = config::load_for_scan(path, options, policy)?;
    let phase03 = options.profile == Profile::SkylosPhase03;
    let enable_quality =
        options.enable_quality.unwrap_or(config.quality_enabled) || options.quality_only;
    let include_dead_code = !options.quality_only;
    let project_wildcards = phase03 && include_dead_code;
    let grep_verify = include_dead_code && options.grep_verify.unwrap_or(phase03);
    if !phase03 && !options.dependency_search_paths.is_empty() {
        return Err(ConfigError::new(
            "dependency source paths require the skylos-0.3 profile",
        ));
    }
    if options.experimental_javascript && !(phase03 && include_dead_code) {
        return Err(ConfigError::new(
            "experimental JavaScript and TypeScript analysis requires a skylos-0.3 dead-code scan",
        ));
    }
    if enable_quality && !phase03 {
        return Err(ConfigError::new(
            "quality scanning requires the skylos-0.3 profile",
        ));
    }
    if !phase03
        && (!matches!(options.trace_file, EvidenceInput::Default)
            || !matches!(options.coverage_file, EvidenceInput::Default))
    {
        return Err(ConfigError::new(
            "runtime evidence options require the skylos-0.3 profile",
        ));
    }
    let src_is_package = phase03 && path.join("src/__init__.py").is_file();
    let roots = if path.is_dir() && path.join("src").is_dir() && !src_is_package {
        vec![PathBuf::from("src"), PathBuf::from(".")]
    } else {
        Vec::new()
    };
    let cache = options.cache_dir.as_ref().map(|directory| {
        FactCache::for_project(
            directory,
            if project_wildcards {
                "phase03-module-v42/ruff-0.0.12/py311/utf8-universal"
            } else {
                "phase02-module-v39/ruff-0.0.12/py311/utf8-universal"
            },
            path,
            &roots,
        )
    });
    let mut project = preview_source::discover_sources(
        path,
        &roots,
        FileEntry::None,
        Some(&config.selection),
        options.experimental_javascript && cleanup_inputs.is_none(),
    );
    let mut languages: BTreeMap<_, usize> = if phase03 && !project.files.is_empty() {
        BTreeMap::new()
    } else {
        BTreeMap::from([("Python", 0)])
    };
    for file in &project.files {
        let language = match preview_source::javascript_extension(OsStr::new(&file.label)) {
            Some("ts" | "tsx" | "mts" | "cts") => "TypeScript",
            Some(_) => "JavaScript",
            None => "Python",
        };
        *languages.entry(language).or_default() += 1;
    }
    let has_javascript =
        languages.contains_key("JavaScript") || languages.contains_key("TypeScript");
    let mut issues = std::mem::take(&mut project.issues);
    let mut selected_paths = Vec::new();
    let mut generated_sources = BTreeSet::new();
    let mut sources = BTreeMap::new();
    let mut files = Vec::new();
    let mut js_modules = BTreeMap::new();
    let mut javascript_analysis = Analysis::default();
    let mut stamps = Vec::new();
    let mut file_configs = BTreeMap::new();
    let mut directory_configs = BTreeMap::new();
    let mut local_configs = Vec::new();
    let extract_python = if project_wildcards {
        extract_with_project_wildcards
    } else {
        extract
    };
    match project.map_file_sources(options.workers, |file, source| {
        let facts = if let Some(extension) =
            preview_source::javascript_extension(OsStr::new(&file.label))
        {
            javascript::extract(&file.label, source, extension, cache.as_ref())
                .map(FileFacts::JavaScript)
        } else {
            match &cache {
                Some(cache) => cache.get_or_extract(source, extract_python),
                None => extract_python(source),
            }
            .map(FileFacts::Python)
        }
        .map_err(|error| Issue::new(error.kind, &file.label, error.message).at(error.location));
        Ok(Snapshot {
            facts,
            source: source.to_owned(),
            hide_findings: !path.is_file()
                && config
                    .selection
                    .suppress_generated_findings(&file.label, source),
        })
    }) {
        Ok(work) => {
            javascript_analysis
                .issues
                .extend(work.javascript_budget_issue);
            for (file, work) in project.files.iter().zip(work.files) {
                selected_paths.push(file.label.clone());
                let Some(work) = work else {
                    stamps.push(None);
                    continue;
                };
                stamps.push(work.stamp);
                let snapshot = work.result.and_then(|snapshot| snapshot).and_then(
                    |Snapshot {
                         facts,
                         source,
                         hide_findings,
                     }| match facts {
                        Ok(facts) => Ok((facts, source, hide_findings)),
                        Err(issue) => {
                            if let Some(diff_sources) = diff_sources.as_deref_mut() {
                                diff_sources.insert(file.label.clone(), source);
                            }
                            Err(issue)
                        }
                    },
                );
                match snapshot {
                    Ok((facts, source, hide_findings)) => {
                        if hide_findings {
                            generated_sources.insert(file.label.clone());
                        }
                        let directory = file.directory();
                        let config_index = *directory_configs
                            .entry(directory.to_owned())
                            .or_insert_with(|| match config.for_path(directory, options, policy) {
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
                        match facts {
                            FileFacts::Python(facts) => {
                                if !phase03 && let Some(at) = facts.first_named_condition {
                                    issues.push(Issue::new(
                                        "unsupported_construct",
                                        &file.label,
                                        "assignment expressions are outside the compatibility subset",
                                    ).at(at));
                                    continue;
                                }
                                let is_package = Path::new(&file.label).file_name()
                                    == Some(OsStr::new("__init__.py"));
                                // The loose-file identity includes the initializer's
                                // stem when a package sits beneath a project folder.
                                let module = if phase03 && is_package && file.loose_name {
                                    file.name.strip_suffix(".__init__").unwrap_or(&file.name)
                                } else {
                                    &file.name
                                };
                                files.push(AnalyzedFile {
                                    path: file.label.clone(),
                                    module: module.to_owned(),
                                    is_package,
                                    fastapi_router_mode: config_index
                                        .map_or(&config, |index| &local_configs[index])
                                        .fastapi_router_mode,
                                    facts,
                                });
                            }
                            FileFacts::JavaScript(module) => {
                                js_modules.insert(file.label.clone(), module);
                            }
                        }
                    }
                    Err(issue)
                        if preview_source::javascript_extension(OsStr::new(&file.label))
                            .is_some() =>
                    {
                        javascript_analysis.issues.push(issue);
                    }
                    Err(issue) => issues.push(issue),
                }
            }
        }
        Err(issue) => {
            selected_paths.extend(project.files.iter().map(|file| file.label.clone()));
            issues.push(issue);
        }
    }

    let wildcard_resolution = if project_wildcards {
        wildcards::resolve(&project, &mut files)
    } else {
        wildcards::Resolution::default()
    };
    let wildcard_issues = wildcard_resolution.issues;
    let dependency_sources_complete = issues.is_empty() && wildcard_issues.is_empty();
    let package_entrypoints = (include_dead_code && phase03 && dependency_sources_complete)
        .then(|| package_entrypoints::Snapshot::read(path, &files));
    let available_package_entrypoints = package_entrypoints
        .as_ref()
        .filter(|metadata| metadata.issues.is_empty());
    let runtime = (include_dead_code && phase03 && dependency_sources_complete).then(|| {
        runtime_evidence::Snapshot::read(
            path,
            &project,
            &sources,
            &stamps,
            &options.trace_file,
            &options.coverage_file,
        )
    });
    let verification = (grep_verify && dependency_sources_complete)
        .then(|| grep_verification::Snapshot::read(&project, Some(&config.selection)));
    let django_settings = (include_dead_code && phase03)
        .then(|| std::env::var_os("DJANGO_SETTINGS_MODULE"))
        .flatten();
    let django_templates = (include_dead_code && phase03 && path.is_dir())
        .then(|| analysis::TemplateProject::new(&project, &config.selection));
    let gin = (include_dead_code && phase03 && path.is_dir())
        .then(|| analysis::GinProject::new(&project, &config.selection));
    let external_classes = (phase03 && include_dead_code && path.is_dir()).then(|| {
        external_classes::ExternalClassIndex::new(external_classes::search_roots(
            path,
            &options.dependency_search_paths,
        ))
    });
    let framework = analysis::FrameworkInputs {
        external_classes: external_classes.as_ref(),
        settings: django_settings.as_deref(),
        templates: django_templates.as_ref(),
        gin: gin.as_ref(),
        discovery: (phase03 && path.is_dir()).then_some(&project),
        unresolved_project_wildcards: !wildcard_issues.is_empty(),
        wildcard_guards: Some(&wildcard_resolution.guards),
        #[cfg(test)]
        fastapi_call_event_limit: None,
    };
    let mut analysis = if include_dead_code && issues.is_empty() {
        if local_configs.is_empty() {
            analysis::analyze_with_verification(
                &files,
                &config.entrypoints,
                verification.as_ref(),
                available_package_entrypoints,
                runtime.as_ref(),
                phase03,
                framework,
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
                phase03,
                framework,
            )
        }
    } else {
        Analysis::default()
    };
    analysis.issues.extend(wildcard_issues);
    if include_dead_code
        && has_javascript
        && issues.is_empty()
        && analysis.issues.is_empty()
        && javascript_analysis.issues.is_empty()
    {
        if path.is_file() {
            javascript_analysis.issues.push(Issue::new(
                "unsupported_js_input_scope",
                &selected_paths[0],
                "JavaScript and TypeScript file reachability requires scanning the package directory",
            ));
        } else {
            javascript::analyze(&project, &js_modules, &mut javascript_analysis);
        }
    }
    if let Some(mut runtime) = runtime {
        analysis.runtime_evidence_issues.append(&mut runtime.issues);
        analysis.runtime_evidence_issues.sort();
        analysis.runtime_evidence_issues.dedup();
    }
    if let Some(inputs) = cleanup_inputs.as_deref_mut() {
        if let Some(templates) = &django_templates {
            inputs.policy.extend(templates.input_digests());
        }
        if let Some(gin) = &gin {
            inputs.policy.extend(gin.input_digests());
        }
        for config in std::iter::once(&config).chain(&local_configs) {
            inputs.policy.extend(
                config
                    .input_digests()
                    .map(|(path, hash)| (path.clone(), *hash)),
            );
        }
        if let Some(settings) = &django_settings {
            inputs.policy.insert(
                "DJANGO_SETTINGS_MODULE".into(),
                blake3::hash(settings.as_encoded_bytes()),
            );
        }
        let metadata = package_entrypoints
            .iter()
            .flat_map(|snapshot| snapshot.input_stamps());
        let grep = verification
            .iter()
            .flat_map(|snapshot| snapshot.input_stamps());
        for (path, stamp) in metadata.chain(grep) {
            let label = preview_source::display(path);
            match preview_source::read_snapshot(path, &label, Some(stamp)) {
                Ok((bytes, _)) => {
                    inputs.policy.insert(label, blake3::hash(&bytes));
                }
                Err(issue) => issues.push(issue),
            }
        }
    }
    let external_dependency_roots = external_classes.as_ref().map_or_else(Vec::new, |index| {
        issues.extend(index.changed_sources().iter().map(|path| {
            Issue::new(
                "source_changed",
                &preview_source::display(path),
                "dependency source changed during analysis",
            )
        }));
        if let Some(inputs) = cleanup_inputs.as_deref_mut() {
            for (path, stamp) in index.input_stamps().iter() {
                let label = preview_source::display(path);
                match preview_source::read_snapshot(path, &label, Some(stamp)) {
                    Ok((bytes, _)) => {
                        inputs.policy.insert(label, blake3::hash(&bytes));
                    }
                    Err(issue) => issues.push(issue),
                }
            }
        }
        index
            .roots_used()
            .iter()
            .map(|path| preview_source::display(path))
            .collect()
    });
    issues.append(&mut analysis.issues);
    if let Some(verification) = verification {
        issues.extend(verification.finish(&project, Some(&config.selection)));
    }
    if let Some(mut metadata) = package_entrypoints {
        issues.append(&mut metadata.issues);
        issues.extend(metadata.verify());
    }
    let mut cleanup_dependency_sources = BTreeMap::new();
    let mut dependency_result = enable_quality.then(|| {
        dependencies::scan(
            path,
            &project,
            &files,
            &config.selection,
            dependency_sources_complete,
            config.ignores_rule(dependencies::RULE),
            diff_sources.as_deref_mut().or(cleanup_inputs
                .is_some()
                .then_some(&mut cleanup_dependency_sources)),
        )
    });
    if let Some(inputs) = cleanup_inputs.as_deref_mut() {
        inputs.policy.extend(
            cleanup_dependency_sources
                .into_iter()
                .map(|(file, source)| {
                    (
                        preview_source::display(&path.join(file)),
                        blake3::hash(source.as_bytes()),
                    )
                }),
        );
    }
    let mut snapshot_issues = project.verify(stamps);
    if let Some(templates) = django_templates {
        snapshot_issues.extend(templates.verify());
    }
    if let Some(gin) = gin {
        snapshot_issues.extend(gin.verify());
    }
    if !snapshot_issues.is_empty()
        && dependency_result.is_some()
        && !config.ignores_rule(dependencies::RULE)
    {
        dependency_result = Some(dependencies::Result::not_run("incomplete_python_sources"));
    }
    issues.extend(snapshot_issues);
    if !config.unchanged(path, options, policy)
        || directory_configs.iter().any(|(directory, index)| {
            let effective = index.map_or(&config, |index| &local_configs[index]);
            !effective.unchanged(directory, options, policy)
        })
    {
        issues.push(Issue::new(
            "configuration_changed",
            ".",
            "configuration changed during analysis",
        ));
        if dependency_result.is_some() {
            dependency_result = Some(dependencies::Result::not_run(
                "dependency_selection_changed",
            ));
        }
    }
    let python_complete = issues.is_empty();
    let javascript_complete = python_complete && javascript_analysis.issues.is_empty();
    if javascript_complete {
        analysis.findings.append(&mut javascript_analysis.findings);
        analysis.evidence.append(&mut javascript_analysis.evidence);
    }
    issues.append(&mut javascript_analysis.issues);
    if has_javascript && enable_quality && !config.ignores_rule(dependencies::RULE) {
        issues.push(Issue::new(
            "unsupported_npm_dependency_analysis",
            ".",
            "the unused-dependency rule currently covers Python dependencies only",
        ));
    }
    if let Some(result) = &dependency_result {
        issues.extend(result.issues());
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
    let test_only_paths: BTreeSet<_> = file_facts
        .keys()
        .copied()
        .filter(|file| phase03 && is_test_only_path(file))
        .collect();
    if python_complete {
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
            let inline = inline
                || (finding.rule_id == javascript::RULE
                    && js_modules
                        .get(&finding.file)
                        .is_some_and(|module| module.ignore_file));
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
            if finding.category != Category::File && test_only_paths.contains(finding.file.as_str())
            {
                // Keep non-use evidence while presenting a test-path policy decision.
                let mut diagnostic =
                    Diagnostic::new(finding, "test-only path".into(), "whitelisted", source);
                diagnostic.suppression_code = Some("non_library_path");
                diagnostic.folder_role = Some("test");
                whitelisted.push(diagnostic);
                continue;
            }
            if let Some(reason) = finding.declaration_whitelist {
                whitelisted.push(Diagnostic::new(
                    finding,
                    reason.into(),
                    "whitelisted",
                    source,
                ));
                continue;
            }
            if finding.reason == WEAK_HELPER_FUNCTION {
                // The pinned reference treats these unreferenced helper names
                // as abstentions. Keep them separate from usage and coverage.
                analysis.uncertainties.push(
                    Issue::new(WEAK_HELPER_FUNCTION, &finding.file, "weak helper function")
                        .at(Location {
                            line: finding.line,
                            column: finding.column,
                        })
                        .with_symbol(finding.name, finding.full_name),
                );
                continue;
            }
            // Skylos applies the confidence threshold to bindings and keeps file findings.
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

    if let Some(diff_sources) = diff_sources {
        diff_sources.extend(sources);
    } else if let Some(inputs) = cleanup_inputs {
        inputs.sources = sources;
    }
    let dead_code_evidence = phase03.then(|| {
        workflow::EvidenceSummary::new(
            analysis.definitions.len(),
            analysis.rescued_count,
            &categories,
            &analysis.uncertainties,
        )
    });
    if phase03 {
        categories.retain(|item| {
            item.dead_code_classification = Some("likely_dead");
            true
        });
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
    let dead_code_liveness = phase03.then(|| LivenessSummary {
        rescued: analysis
            .evidence
            .iter()
            .filter(|event| event.kind == "package_entrypoint")
            .map(|event| LivenessRescue {
                full_name: event.full_name.clone(),
                reason: event.kind,
            })
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect(),
    });
    let retained_evidence =
        RetainedEvidence::for_profile(options.profile, analysis.evidence, &analysis.verification);
    let (quality, unused_dependency_analysis, dependency_usage_evidence) =
        if let Some(result) = dependency_result {
            (Some(result.findings), Some(result.receipt), result.evidence)
        } else {
            (None, None, Vec::new())
        };
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
            languages,
            fastapi_route_declarations: analysis.fastapi_route_declarations,
            javascript_file_analysis: has_javascript.then_some(JavaScriptFileAnalysis {
                rule_id: javascript::RULE,
                status: if javascript_complete {
                    "complete"
                } else {
                    "incomplete"
                },
                scope: "local_static_imports_from_package_main",
            }),
            grep_verify: (phase03 || grep_verify).then_some(VerificationSummary {
                enabled: grep_verify,
                rescued_count: analysis.rescued_count,
            }),
            dead_code_liveness,
            unused_dependency_analysis,
            dead_code_evidence,
            pr_impact: None,
            baseline: None,
        },
        coverage: Coverage {
            status,
            selected_files: selected_paths.len(),
            extracted_files: files.len() + js_modules.len(),
            issues,
        },
        selected_paths,
        external_dependency_roots,
        exclusions: project.exclusions,
        generated_sources,
        config_sources: config.sources,
        file_config_sources,
        config_warnings,
        confidence_threshold: config.confidence,
        all_advice: options.all_advice,
        analysis_errors,
        dead_code_evidence: Some(retained_evidence),
        definitions: phase03.then_some(analysis.definitions),
        grep_verification_evidence: analysis.verification,
        runtime_evidence: analysis.runtime_evidence,
        runtime_evidence_issues: analysis.runtime_evidence_issues,
        dead_code_abstentions: analysis.uncertainties,
        quality,
        dependency_usage_evidence,
        suppressed,
        whitelisted,
        cache: cache.as_ref().map(FactCache::stats),
        categories,
        gate: None,
        analysis_coverage: None,
        gate_policy: config.gate,
    })
}
