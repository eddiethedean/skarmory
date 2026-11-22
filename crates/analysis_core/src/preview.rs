use std::path::{Path, PathBuf};
use std::time::Instant;

use serde::Serialize;
use skarmory_python::preview;

use crate::fact_cache::{CacheStats, FactCache};
use crate::grep_verification::{self, Evidence as VerificationEvidence};
use crate::preview_source::{self, Exclusion, FileEntry};
use crate::{Coverage, Issue, Timings};

mod engine;

const MAX_WORKERS: usize = 32;
// Bump the facts epoch when the extractor, parser, or location semantics change.
const FACTS_NAMESPACE: &str = "phase01-module-v1/ruff-0.0.12/py311/utf8-universal";

#[derive(Clone, Copy, Debug, Default, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ScanPolicy {
    #[default]
    Application,
    Library,
}

#[derive(Clone, Debug)]
pub struct ScanOptions {
    pub source_roots: Vec<PathBuf>,
    pub policy: ScanPolicy,
    pub entry_file: Option<PathBuf>,
    pub entry_modules: Vec<String>,
    pub entry_callables: Vec<String>,
    pub public_modules: Vec<String>,
    pub public_callables: Vec<String>,
    pub workers: usize,
    pub grep_verify: bool,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            source_roots: Vec::new(),
            policy: ScanPolicy::Application,
            entry_file: None,
            entry_modules: Vec::new(),
            entry_callables: Vec::new(),
            public_modules: Vec::new(),
            public_callables: Vec::new(),
            workers: 1,
            grep_verify: false,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct PreviewReport {
    schema_version: u32,
    profile: &'static str,
    policy: ScanPolicy,
    entry: Option<String>,
    roots: Vec<Root>,
    source_roots: Vec<String>,
    import_roots: Vec<String>,
    selected_paths: Vec<String>,
    exclusions: Vec<Exclusion>,
    coverage: Coverage,
    uncertainties: Vec<Issue>,
    evidence: Vec<Evidence>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    grep_verification_evidence: Vec<VerificationEvidence>,
    findings: Vec<PreviewFinding>,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
struct Root {
    kind: &'static str,
    target: String,
}

#[derive(Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
struct Evidence {
    symbol_id: String,
    certainty: &'static str,
    kind: &'static str,
    path: String,
    line: usize,
    column: usize,
}

#[derive(Debug, Serialize)]
struct PreviewFinding {
    rule: &'static str,
    symbol_id: String,
    module: String,
    symbol: String,
    scope: String,
    path: String,
    line: usize,
    column: usize,
    evidence: &'static str,
}

impl PreviewReport {
    pub fn is_complete(&self) -> bool {
        self.coverage.status == "complete"
    }

    pub fn to_json(&self) -> serde_json::Result<String> {
        serde_json::to_string(self)
    }
}

impl ScanOptions {
    fn roots(&self, script: Option<&str>) -> Vec<Root> {
        let mut roots = Vec::new();
        if let Some(target) = script {
            roots.push(Root {
                kind: "direct_file",
                target: target.into(),
            });
        }
        for (kind, targets) in [
            ("named_module", &self.entry_modules),
            ("callable_entry", &self.entry_callables),
            ("public_module", &self.public_modules),
            ("public_callable", &self.public_callables),
        ] {
            roots.extend(targets.iter().cloned().map(|target| Root { kind, target }));
        }
        roots.sort();
        roots.dedup();
        roots
    }

    fn file_entry(&self) -> Result<FileEntry<'_>, Issue> {
        let invalid = |message| Issue::new("invalid_options", ".", message);
        if !(1..=MAX_WORKERS).contains(&self.workers) {
            return Err(invalid("workers must be between 1 and 32"));
        }
        let application_modes = usize::from(self.entry_file.is_some())
            + usize::from(!self.entry_modules.is_empty())
            + usize::from(!self.entry_callables.is_empty());
        let public_boundary = !self.public_modules.is_empty() || !self.public_callables.is_empty();
        match self.policy {
            ScanPolicy::Application if public_boundary || application_modes > 1 => Err(invalid(
                "application scans accept one entry mode and no library declarations",
            )),
            ScanPolicy::Library if application_modes != 0 || !public_boundary => Err(invalid(
                "library scans require public modules or callables and no application entries",
            )),
            ScanPolicy::Application
                if self.entry_modules.is_empty() && self.entry_callables.is_empty() =>
            {
                Ok(self
                    .entry_file
                    .as_deref()
                    .map_or(FileEntry::Default, FileEntry::Explicit))
            }
            _ => Ok(FileEntry::None),
        }
    }
}

pub fn scan(path: &Path, options: &ScanOptions) -> (PreviewReport, Timings) {
    scan_with_cache(path, options, None)
}

/// Reuse file-local facts while deriving all project results from current inputs.
pub fn scan_cached(
    path: &Path,
    options: &ScanOptions,
    directory: &Path,
) -> (PreviewReport, Timings, CacheStats) {
    let cache = FactCache::for_project(directory, FACTS_NAMESPACE, path, &options.source_roots);
    let (report, timings) = scan_with_cache(path, options, Some(&cache));
    (report, timings, cache.stats())
}

fn scan_with_cache(
    path: &Path,
    options: &ScanOptions,
    cache: Option<&FactCache>,
) -> (PreviewReport, Timings) {
    let mut timings = Timings::default();
    let mut report = PreviewReport {
        schema_version: 2,
        profile: "phase01-python311-dead-code-preview",
        policy: options.policy,
        entry: None,
        roots: Vec::new(),
        source_roots: Vec::new(),
        import_roots: Vec::new(),
        selected_paths: Vec::new(),
        exclusions: Vec::new(),
        coverage: Coverage {
            status: "incomplete",
            selected_files: 0,
            extracted_files: 0,
            issues: Vec::new(),
        },
        uncertainties: Vec::new(),
        evidence: Vec::new(),
        grep_verification_evidence: Vec::new(),
        findings: Vec::new(),
    };
    let entry = match options.file_entry() {
        Ok(entry) => entry,
        Err(issue) => {
            report.coverage.issues.push(issue);
            return (report, timings);
        }
    };
    let default_path = match &entry {
        FileEntry::Default if path.extension().is_some_and(|ext| ext == "py") => {
            Some(path.file_name().unwrap_or_default().to_string_lossy())
        }
        FileEntry::Default => Some("main.py".into()),
        FileEntry::Explicit(path) => Some(path.to_string_lossy()),
        FileEntry::None => None,
    };
    let start = Instant::now();
    let mut project = preview_source::discover(path, &options.source_roots, entry);
    timings.discovery_ns = start.elapsed().as_nanos();
    report.source_roots.clone_from(&project.roots);
    report.import_roots.clone_from(&project.import_roots);
    report.selected_paths = project
        .files
        .iter()
        .map(|file| file.label.clone())
        .collect();
    report.coverage.selected_files = project.files.len();
    report.coverage.issues.append(&mut project.issues);
    let script = project
        .script
        .map(|index| project.files[index].label.as_str());
    report.roots = options.roots(script.or(default_path.as_deref()));
    if let Some(index) = project.script {
        report.entry = Some(project.files[index].name.clone());
    } else if let Some(name) = options
        .entry_modules
        .first()
        .or(options.entry_callables.first())
    {
        report.entry = Some(name.clone());
    }

    let work = match project.map_files(options.workers, |source| match cache {
        Some(cache) => cache.get_or_extract(source, preview::extract),
        None => preview::extract(source),
    }) {
        Ok(work) => work,
        Err(issue) => {
            report.coverage.issues.push(issue);
            report.exclusions = project.exclusions;
            return (report, timings);
        }
    };
    let mut stamps = Vec::with_capacity(work.len());
    let mut modules = Vec::with_capacity(work.len());
    for (file, work) in project.files.iter().zip(work) {
        stamps.push(work.stamp);
        timings.read_decode_ns += work.read_ns;
        timings.parse_extract_ns += work.extract_ns;
        match work.result {
            Ok(Ok(module)) => modules.push(module),
            Ok(Err(error)) => report
                .coverage
                .issues
                .push(Issue::new(error.kind, &file.label, error.message).at(error.location)),
            Err(issue) => report.coverage.issues.push(issue),
        }
    }
    report.coverage.extracted_files = modules.len();
    if report.coverage.issues.is_empty() {
        let mut engine = engine::Engine::new(&project, &modules);
        let start = Instant::now();
        engine.run(options);
        if options.grep_verify {
            let verification = grep_verification::Snapshot::read(&project, None);
            engine.apply_verification(&verification);
            report
                .coverage
                .issues
                .extend(verification.finish(&project, None));
        }
        timings.resolution_ns = Some(start.elapsed().as_nanos());
        engine.finish(&mut report, &mut timings);
    }

    report.coverage.issues.extend(project.verify(stamps));
    report.exclusions = project.exclusions;
    report.coverage.issues.sort();
    report.coverage.issues.dedup();
    if report.coverage.issues.is_empty() {
        report.coverage.status = "complete";
    } else {
        report.findings.clear();
    }
    (report, timings)
}
