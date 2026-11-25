//! Native engines for the historical prototype and the packaged-project preview.

mod fact_cache;
mod graph;
mod grep_verification;
pub mod phase02;
mod preview;
mod preview_source;
mod source;

pub use fact_cache::CacheStats;
pub use preview::{PreviewReport, ScanOptions, ScanPolicy, scan, scan_cached};

use std::path::Path;
use std::time::Instant;

use serde::Serialize;
use skarmory_python::{FrontendError, Location, Module};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Serialize)]
pub struct Report {
    schema_version: u32,
    profile: &'static str,
    entry: Option<String>,
    coverage: Coverage,
    uncertainties: Vec<Issue>,
    findings: Vec<Finding>,
}

#[derive(Debug, Serialize)]
struct Coverage {
    status: &'static str,
    selected_files: usize,
    extracted_files: usize,
    issues: Vec<Issue>,
}

#[derive(Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
struct Issue {
    kind: &'static str,
    path: String,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    line: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    column: Option<usize>,
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    symbol: Option<Box<IssueSymbol>>,
}

#[derive(Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
struct IssueSymbol {
    name: String,
    full_name: String,
}

impl Issue {
    fn new(kind: &'static str, path: &str, message: impl ToString) -> Self {
        Self {
            kind,
            path: path.into(),
            message: message.to_string(),
            line: None,
            column: None,
            symbol: None,
        }
    }

    fn at(mut self, location: Location) -> Self {
        self.line = Some(location.line);
        self.column = Some(location.column);
        self
    }

    fn with_symbol(mut self, name: String, full_name: String) -> Self {
        self.symbol = Some(Box::new(IssueSymbol { name, full_name }));
        self
    }
}

#[derive(Debug, Serialize)]
struct Finding {
    rule: &'static str,
    module: String,
    symbol: String,
    path: String,
    line: usize,
    column: usize,
    evidence: &'static str,
}

#[derive(Debug, Default, Serialize)]
pub struct Timings {
    discovery_ns: u128,
    read_decode_ns: u128,
    parse_extract_ns: u128,
    resolution_ns: Option<u128>,
    reachability_ns: Option<u128>,
    rule_ns: Option<u128>,
}

struct ModuleFile {
    path: String,
    name: String,
    facts: Module,
}

impl Report {
    pub fn is_complete(&self) -> bool {
        self.coverage.status == "complete"
    }

    pub fn to_json(&self) -> serde_json::Result<String> {
        serde_json::to_string(self)
    }
}

pub fn analyze(path: &Path) -> Report {
    analyze_with_timings(path).0
}

pub fn analyze_with_timings(path: &Path) -> (Report, Timings) {
    let start = Instant::now();
    let discovery = source::discover(path);
    let mut timings = Timings {
        discovery_ns: start.elapsed().as_nanos(),
        ..Timings::default()
    };
    let mut report = Report {
        schema_version: 1,
        profile: "phase0-python311-flat-application",
        entry: discovery.entry,
        coverage: Coverage {
            status: "incomplete",
            selected_files: discovery.files.len(),
            extracted_files: 0,
            issues: discovery.issues,
        },
        uncertainties: Vec::new(),
        findings: Vec::new(),
    };
    let mut modules = Vec::new();
    for file in discovery.files {
        let start = Instant::now();
        let source = file.read();
        timings.read_decode_ns += start.elapsed().as_nanos();
        let source = match source {
            Ok(source) => source,
            Err(issue) => {
                report.coverage.issues.push(issue);
                continue;
            }
        };
        let start = Instant::now();
        let facts = skarmory_python::extract(&source);
        timings.parse_extract_ns += start.elapsed().as_nanos();
        match facts {
            Ok(facts) => modules.push(ModuleFile {
                name: file.name,
                path: file.label,
                facts,
            }),
            Err(FrontendError {
                kind,
                location,
                message,
            }) => report
                .coverage
                .issues
                .push(Issue::new(kind, &file.label, message).at(location)),
        }
    }
    report.coverage.extracted_files = modules.len();
    if report.coverage.issues.is_empty()
        && let Some(entry) = &report.entry
    {
        let start = Instant::now();
        let graph = graph::resolve(&modules);
        timings.resolution_ns = Some(start.elapsed().as_nanos());
        match graph {
            Ok(graph) => {
                let start = Instant::now();
                let reachable = graph.reachable(entry);
                timings.reachability_ns = Some(start.elapsed().as_nanos());
                let start = Instant::now();
                report.findings = graph.findings(&modules, &reachable);
                timings.rule_ns = Some(start.elapsed().as_nanos());
                report.coverage.status = "complete";
            }
            Err(graph::Failure::Unsupported(issue)) => report.coverage.issues.push(issue),
            Err(graph::Failure::Unresolved(issue)) => report.uncertainties.push(issue),
        }
    }
    (report, timings)
}
