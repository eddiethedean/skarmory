//! Git comparisons, versioned dead-code baselines and coverage-aware gates.

mod baseline;
mod git;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::ops::Range;
use std::path::Path;

use serde::Serialize;
use serde_json::{Value, json};
use similar::{Algorithm, DiffTag, capture_diff_slices};
use time::OffsetDateTime;

use super::super::Issue;
use super::{
    Categories, ConfigError, EvidenceInput, Options, Profile, Report, config, preview_source, scan,
    scan_with_context,
};
pub use baseline::Input as BaselineInput;
pub(super) use baseline::Summary as BaselineSummary;

const DEAD_CODE_RULES: &[&str] = &[
    "SKY-U001", "SKY-U002", "SKY-U003", "SKY-U004", "SKY-U006", "SKY-E002",
];
const MAX_DIFF_WORK: usize = 16_000_000;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DiffMode {
    #[default]
    Lines,
    Files,
    Impact,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Low,
    Medium,
    High,
    Critical,
}

impl Severity {
    pub fn parse(value: &str) -> Result<Self, ConfigError> {
        match value.trim().to_ascii_lowercase().as_str() {
            "low" => Ok(Self::Low),
            "medium" => Ok(Self::Medium),
            "high" => Ok(Self::High),
            "critical" => Ok(Self::Critical),
            _ => Err(ConfigError::new(format!(
                "unsupported minimum severity: {value}"
            ))),
        }
    }
}

#[derive(Default)]
pub struct Settings {
    pub diff: Option<String>,
    pub diff_mode: DiffMode,
    pub baseline: Option<BaselineInput>,
    pub write_baseline: Option<std::path::PathBuf>,
    pub rules: Vec<String>,
    pub severity: Option<Severity>,
    pub gate: bool,
    pub strict: bool,
    pub require_complete: bool,
    pub force: bool,
}

pub struct Outcome {
    pub report: Report,
    pub exit_code: u8,
}

pub fn run(path: &Path, options: &Options, settings: &Settings) -> Result<Outcome, ConfigError> {
    if options.profile != Profile::SkylosPhase03 {
        return Err(ConfigError::new(
            "dead-code workflows require the skylos-0.3 profile",
        ));
    }
    if settings.diff.is_none() && settings.diff_mode != DiffMode::Lines {
        return Err(ConfigError::new(
            "the requested diff mode requires a Git diff base",
        ));
    }
    if settings.force && !settings.gate {
        return Err(ConfigError::new("force requires a configured gate"));
    }
    if settings.write_baseline.is_some()
        && (settings.baseline.is_some()
            || settings.diff.is_some()
            || settings.severity.is_some()
            || !settings.rules.is_empty())
    {
        return Err(ConfigError::new(
            "baseline creation requires an unfiltered scan",
        ));
    }

    let mut options = options.clone();
    let rules = parse_rules(&settings.rules)?;
    if !rules.is_empty() {
        options.quality_only = rules.len() == 1 && rules.contains(super::dependencies::RULE);
        options.enable_quality = Some(rules.contains(super::dependencies::RULE));
    }
    if options.quality_only
        && (matches!(options.trace_file, EvidenceInput::Path(_))
            || matches!(options.coverage_file, EvidenceInput::Path(_)))
    {
        return Err(ConfigError::new(
            "runtime evidence requires a dead-code analysis rule",
        ));
    }

    let (mut report, changes) = if let Some(reference) = &settings.diff {
        scan_diff(
            path,
            &options,
            settings.diff_mode,
            reference,
            settings.gate || settings.require_complete,
        )?
    } else {
        (scan(path, &options)?, None)
    };
    if let Some(impact) = &mut report.analysis_summary.pr_impact {
        impact.introduced_count = report
            .categories
            .iter()
            .filter(|item| {
                item.change_status.is_some() && (rules.is_empty() || rules.contains(item.rule_id))
            })
            .count();
    }
    if let Some(input) = &settings.baseline {
        let directory = path.canonicalize().map_err(ConfigError::new)?;
        let project = if directory.is_file() {
            directory.parent().expect("file has parent")
        } else {
            &directory
        };
        match baseline::read(input, project) {
            Ok(mut known) => {
                let original = known.values().sum::<usize>();
                // Count stale entries against the full selected report. Diff
                // and severity may hide a finding which still exists.
                let mut current = baseline::inventory(&report);
                let stale = report.is_complete().then(|| {
                    known
                        .iter()
                        .map(|(identity, count)| {
                            count.saturating_sub(current.remove(identity).unwrap_or(0))
                        })
                        .sum()
                });
                report
                    .categories
                    .retain(|item| !baseline::take(&mut known, item));
                report.analysis_summary.baseline = Some(BaselineSummary {
                    status: "applied",
                    matched_count: original - known.values().sum::<usize>(),
                    stale_count: stale,
                    message: None,
                });
            }
            Err(error) => {
                report.analysis_summary.baseline = Some(BaselineSummary {
                    status: "unavailable",
                    matched_count: 0,
                    stale_count: None,
                    message: Some(error.to_string()),
                });
            }
        }
    }
    if !rules.is_empty() {
        report
            .categories
            .retain(|item| rules.contains(item.rule_id));
    }
    if let Some(output) = &settings.write_baseline {
        report.analysis_summary.baseline = Some(baseline::write(output, &report)?);
    }
    if let Some(changes) = &changes {
        report.categories.retain(|item| {
            item.change_status.is_some()
                || changes.includes(&item.file, item.line, settings.diff_mode)
        });
        if let Some(quality) = &mut report.quality {
            quality.retain(|item| {
                item.relative_file
                    .as_deref()
                    .is_some_and(|path| changes.includes(path, item.line, settings.diff_mode))
            });
        }
    }

    let strict = settings.strict || (settings.gate && report.gate_policy.strict);
    let require_complete =
        strict || settings.require_complete || report.gate_policy.require_complete;
    let coverage = Assessment::new(&report, changes.as_ref(), settings.diff_mode);
    let baseline_failure = report
        .analysis_summary
        .baseline
        .as_ref()
        .filter(|baseline| baseline.status == "unavailable")
        .and_then(|baseline| baseline.message.clone());
    let exit_code = if settings.gate || strict || settings.require_complete {
        evaluate_gate(
            &mut report,
            &coverage,
            strict,
            require_complete,
            settings.force,
            baseline_failure,
        )
    } else {
        u8::from(!report.is_complete() || baseline_failure.is_some()) * 2
    };
    report.analysis_coverage = Some(coverage);

    // Severity is a presentation filter in the pinned CLI.
    if let Some(minimum) = settings.severity {
        report
            .categories
            .retain(|item| item.severity.map_or(Severity::Low, severity) >= minimum);
        report
            .quality
            .iter_mut()
            .for_each(|quality| quality.retain(|item| severity(item.severity) >= minimum));
    }
    Ok(Outcome { report, exit_code })
}

fn severity(value: &str) -> Severity {
    Severity::parse(value).expect("report finding has a known severity")
}

fn parse_rules(tokens: &[String]) -> Result<BTreeSet<String>, ConfigError> {
    tokens
        .iter()
        .flat_map(|token| token.split(','))
        .map(|rule| {
            let rule = rule.trim().to_ascii_uppercase();
            if DEAD_CODE_RULES.contains(&rule.as_str()) || rule == super::dependencies::RULE {
                Ok(rule)
            } else {
                Err(ConfigError::new(format!(
                    "unsupported dead-code rule: {rule:?}"
                )))
            }
        })
        .collect()
}

#[derive(Serialize, Debug)]
pub(super) struct EvidenceSummary {
    symbol_count: usize,
    candidate_decisions: EvidenceCounts,
}

#[derive(Serialize, Debug)]
struct EvidenceCounts {
    reported: usize,
    rescued: usize,
    abstained: usize,
}

impl EvidenceSummary {
    pub(super) fn new(
        symbol_count: usize,
        rescued: usize,
        findings: &Categories,
        abstentions: &[Issue],
    ) -> Self {
        let reported = findings.iter().count();
        Self {
            symbol_count,
            candidate_decisions: EvidenceCounts {
                reported,
                rescued,
                abstained: abstentions.len(),
            },
        }
    }
}

#[derive(Debug, Serialize)]
pub(super) struct Impact {
    mode: &'static str,
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<&'static str>,
    base_commit: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    omitted: Option<BTreeMap<&'static str, usize>>,
    base: SnapshotCoverage,
    head: SnapshotCoverage,
    introduced_count: usize,
}

#[derive(Debug, Serialize)]
struct SnapshotCoverage {
    status: &'static str,
    selected_files: usize,
    extracted_files: usize,
    issues: Vec<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

impl SnapshotCoverage {
    fn from_report(report: &Report) -> Self {
        Self {
            status: report.coverage.status,
            selected_files: report.coverage.selected_files,
            extracted_files: report.coverage.extracted_files,
            issues: report.analysis_errors.clone(),
            error: None,
        }
    }

    fn unavailable(error: ConfigError) -> Self {
        Self {
            status: "incomplete",
            selected_files: 0,
            extracted_files: 0,
            issues: Vec::new(),
            error: Some(error.to_string()),
        }
    }
}

fn scan_diff(
    path: &Path,
    options: &Options,
    mode: DiffMode,
    reference: &str,
    lock_policy: bool,
) -> Result<(Report, Option<Changes>), ConfigError> {
    if !path.is_dir() {
        return Err(ConfigError::new(
            "Git diff workflows require a project directory",
        ));
    }
    let head = path.canonicalize().map_err(ConfigError::new)?;
    let repository = git::Repository::open(&head)?;
    // Leave source discovery's symlink diagnostics intact for an explicitly
    // symlinked project directory. Comparisons require a physical input root.
    if fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_symlink()) {
        return Err(ConfigError::new(
            "Git diff workflows require a physical project directory",
        ));
    }
    let scope = head
        .strip_prefix(&repository.root)
        .map_err(ConfigError::new)?;
    let base = repository.base(reference)?;
    let visible = repository.visible_paths()?;
    let mut sources = BTreeMap::new();
    let mut report = if mode == DiffMode::Impact {
        scan_impact(&repository, &base, scope, &head, options, &mut sources)?
    } else if lock_policy {
        scan_with_base_gate_policy(&repository, &base, scope, &head, options, &mut sources)?
    } else {
        scan_with_context(path, options, None, Some(&mut sources))?
    };
    let scope = if scope.as_os_str().is_empty() {
        String::new()
    } else {
        format!("{}/", preview_source::display(scope))
    };
    report.categories.retain(|item| {
        if item.change_status.is_some() && !visible.contains(&format!("{scope}{}", item.file)) {
            item.change_status = None;
            item.report_scope = None;
        }
        true
    });
    match Changes::read(
        &repository,
        &base,
        &scope,
        &sources,
        &report,
        mode,
        &visible,
    ) {
        Ok(changes) => Ok((report, Some(changes))),
        Err(error) => {
            report
                .categories
                .retain(|item| item.change_status.is_some());
            report.quality.iter_mut().for_each(Vec::clear);
            add_issue(&mut report, Issue::new("diff_scope_incomplete", ".", error));
            Ok((report, None))
        }
    }
}

fn scan_with_base_gate_policy(
    repository: &git::Repository,
    base: &git::Base,
    scope: &Path,
    head: &Path,
    options: &Options,
    sources: &mut BTreeMap<String, String>,
) -> Result<Report, ConfigError> {
    let snapshot = tempfile::tempdir().map_err(ConfigError::new)?;
    let root = snapshot.path().canonicalize().map_err(ConfigError::new)?;
    let explicit = config::explicit_policy(options)?;
    if explicit.is_none()
        && let Err(error) = repository.materialize_policy(base, &root, scope)
    {
        let mut report = scan_with_context(head, options, None, Some(sources))?;
        add_issue(
            &mut report,
            Issue::new("diff_policy_unavailable", ".", error),
        );
        return Ok(report);
    }
    let policy = config::Policy {
        scan_root: &repository.root,
        head_root: &repository.root,
        base_root: &root,
        date: OffsetDateTime::now_utc().date(),
        explicit: explicit.as_ref(),
    };
    scan_with_context(head, options, Some(&policy), Some(sources))
}

fn scan_impact(
    repository: &git::Repository,
    base: &git::Base,
    scope: &Path,
    head: &Path,
    options: &Options,
    sources: &mut BTreeMap<String, String>,
) -> Result<Report, ConfigError> {
    if matches!(options.trace_file, EvidenceInput::Path(_))
        || matches!(options.coverage_file, EvidenceInput::Path(_))
    {
        return Err(ConfigError::new(
            "impact mode compares static analysis and does not accept runtime artifacts",
        ));
    }
    let mut options = options.clone();
    options.trace_file = EvidenceInput::Disabled;
    options.coverage_file = EvidenceInput::Disabled;
    let snapshot = tempfile::tempdir().map_err(ConfigError::new)?;
    let root = snapshot.path().canonicalize().map_err(ConfigError::new)?;
    let base_path = root.join(scope);
    let date = OffsetDateTime::now_utc().date();
    let explicit = config::explicit_policy(&options)?;

    let mut omitted = 0;
    let compare = (|| {
        omitted = repository.materialize(base, &root)?;
        preview_source::reject_symlink_components(&root, &base_path).map_err(ConfigError::new)?;
        fs::create_dir_all(&base_path).map_err(ConfigError::new)?;
        let policy = config::Policy {
            scan_root: &root,
            head_root: &repository.root,
            base_root: &root,
            date,
            explicit: explicit.as_ref(),
        };
        let mut base_options = options.clone();
        // Cache initialization is governed by the live project boundary.
        base_options.cache_dir = base_options.cache_dir.filter(|path| path.is_dir());
        scan_with_context(&base_path, &base_options, Some(&policy), None)
    })();
    let policy = config::Policy {
        scan_root: &repository.root,
        head_root: &repository.root,
        base_root: &root,
        date,
        explicit: explicit.as_ref(),
    };
    let mut report = match &compare {
        Ok(_) => scan_with_context(head, &options, Some(&policy), Some(sources))?,
        Err(_) => scan_with_context(head, &options, None, Some(sources))?,
    };
    let mut impact = Impact {
        mode: "impact",
        status: "incomplete",
        reason: None,
        base_commit: base.commit.clone(),
        omitted: (omitted > 0).then(|| BTreeMap::from([("oversize_non_source", omitted)])),
        base: match &compare {
            Ok(base_report) => SnapshotCoverage::from_report(base_report),
            Err(error) => SnapshotCoverage::unavailable(ConfigError::new(error)),
        },
        head: SnapshotCoverage::from_report(&report),
        introduced_count: 0,
    };
    impact.reason = match (&compare, report.is_complete()) {
        (Ok(base_report), true) if base_report.is_complete() => {
            mark_introduced(&mut report, base_report, &mut impact);
            None
        }
        (Ok(base_report), false) if base_report.is_complete() => Some("head_analysis_incomplete"),
        (_, true) => Some("base_analysis_incomplete"),
        (_, false) => Some("base_and_head_analysis_incomplete"),
    };
    if let Some(reason) = impact.reason {
        add_issue(&mut report, Issue::new("pr_impact_incomplete", ".", reason));
    }
    report.analysis_summary.pr_impact = Some(impact);
    Ok(report)
}

fn mark_introduced(report: &mut Report, base_report: &Report, impact: &mut Impact) {
    let mut known = baseline::inventory(base_report);
    report.categories.retain(|item| {
        if !baseline::take(&mut known, item) {
            item.change_status = Some("introduced");
            item.report_scope = Some("dependency_impact");
        }
        true
    });
    impact.status = "complete";
}

fn add_issue(report: &mut Report, issue: Issue) {
    report.analysis_errors.push(json!({
        "rule_id": "SKY-ANALYSIS-INCOMPLETE",
        "file": issue.path,
        "kind": issue.kind,
        "message": issue.message,
    }));
    report.coverage.issues.push(issue);
    report.coverage.status = "incomplete";
    report.analysis_summary.coverage_status = "incomplete";
    report.analysis_summary.analysis_error_count = report.analysis_errors.len();
}

#[derive(Default)]
struct Changes {
    changed: BTreeMap<String, Vec<Range<usize>>>,
}

impl Changes {
    fn read(
        repository: &git::Repository,
        base: &git::Base,
        prefix: &str,
        sources: &BTreeMap<String, String>,
        report: &Report,
        mode: DiffMode,
        visible: &BTreeSet<String>,
    ) -> Result<Self, ConfigError> {
        let mut result = Self::default();
        let mut compare = BTreeMap::new();
        let mut paths: BTreeSet<_> = report
            .categories
            .iter()
            .map(|item| item.file.clone())
            .collect();
        paths.extend(
            report
                .quality
                .iter()
                .flatten()
                .filter_map(|item| item.relative_file.clone()),
        );
        paths.extend(
            report
                .coverage
                .issues
                .iter()
                .filter(|issue| file_gap(issue))
                .map(|issue| issue.path.clone()),
        );
        for path in paths {
            if !git::safe_path(&path) {
                return Err(ConfigError::new(format!(
                    "invalid path in Git comparison: {path:?}"
                )));
            }
            let full = format!("{prefix}{path}");
            if !visible.contains(&full) {
                continue;
            }
            match (base.entries.get(&full), sources.get(&path)) {
                (Some(entry), Some(source)) if entry.kind == git::Kind::File => {
                    compare.insert(full, (path, entry, source));
                }
                _ => {
                    // A newly added file, type change, or unreadable file has
                    // no trustworthy old/new line relation.
                    result
                        .changed
                        .insert(path, std::iter::once(1..usize::MAX).collect());
                }
            }
        }
        repository.blobs(
            compare
                .iter()
                .map(|(name, (_, entry, _))| (name.as_str(), *entry)),
            |name, _, bytes| {
                let (path, _, new) = &compare[name];
                let old = if let Ok(old) = String::from_utf8(bytes) {
                    old
                } else {
                    result
                        .changed
                        .insert(path.clone(), std::iter::once(1..usize::MAX).collect());
                    return Ok(());
                };
                let old = if path.ends_with(".py") || path.ends_with(".pyi") {
                    old.strip_prefix('\u{feff}')
                        .unwrap_or(&old)
                        .replace("\r\n", "\n")
                        .replace('\r', "\n")
                } else {
                    old
                };
                if &old != *new {
                    let ranges = if mode == DiffMode::Files {
                        Vec::new()
                    } else {
                        changed_lines(&old, new)?
                    };
                    result.changed.insert(path.clone(), ranges);
                }
                Ok(())
            },
        )?;
        Ok(result)
    }

    fn includes(&self, path: &str, line: usize, mode: DiffMode) -> bool {
        self.changed.get(path).is_some_and(|ranges| {
            mode == DiffMode::Files || ranges.iter().any(|range| range.contains(&line))
        })
    }
}

fn changed_lines(old: &str, new: &str) -> Result<Vec<Range<usize>>, ConfigError> {
    let old: Vec<_> = old.split_inclusive('\n').collect();
    let new: Vec<_> = new.split_inclusive('\n').collect();
    let prefix = old.iter().zip(&new).take_while(|(a, b)| a == b).count();
    let (old, new) = (&old[prefix..], &new[prefix..]);
    let suffix = old
        .iter()
        .rev()
        .zip(new.iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    let (old, new) = (&old[..old.len() - suffix], &new[..new.len() - suffix]);
    if old.len().saturating_mul(new.len()) > MAX_DIFF_WORK {
        return Err(ConfigError::new(
            "changed-line comparison exceeds its work limit",
        ));
    }
    Ok(capture_diff_slices(Algorithm::Myers, old, new)
        .into_iter()
        .filter(|op| matches!(op.tag(), DiffTag::Insert | DiffTag::Replace))
        .map(|op| {
            let range = op.new_range();
            (range.start + prefix + 1)..(range.end + prefix + 1)
        })
        .collect())
}

fn file_gap(issue: &Issue) -> bool {
    matches!(issue.kind, "parse_error" | "read_error")
}

#[derive(Debug, Serialize)]
pub(super) struct Assessment {
    status: &'static str,
    source_file_count: usize,
    unchecked_scope: Vec<&'static str>,
    unchecked_files: Vec<String>,
    warnings: Vec<String>,
}

impl Assessment {
    fn new(report: &Report, changes: Option<&Changes>, mode: DiffMode) -> Self {
        let file_count = report.coverage.selected_files;
        let selected: BTreeSet<_> = report.selected_paths.iter().collect();
        let files: BTreeSet<_> = report
            .coverage
            .issues
            .iter()
            .filter(|issue| file_gap(issue) && selected.contains(&issue.path))
            .map(|issue| issue.path.clone())
            .collect();
        let mut coverage = Self {
            status: "complete",
            source_file_count: file_count,
            unchecked_files: files.iter().cloned().collect(),
            unchecked_scope: Vec::new(),
            warnings: Vec::new(),
        };
        if report.is_complete() {
            return coverage;
        }
        let only_gaps = report.coverage.issues.iter().all(|issue| {
            (file_gap(issue)
                && files.contains(&issue.path)
                && changes.is_none_or(|changes| !changes.changed.contains_key(&issue.path)))
                || issue.kind == "grep_verification_limit"
        });
        coverage.status = if only_gaps && files.len() <= file_count / 2 {
            "partial"
        } else {
            "incomplete"
        };
        if !files.is_empty() {
            coverage.warnings.push(format!(
                "Incomplete coverage: {} of {file_count} source files could not be analysed",
                files.len()
            ));
            coverage
                .unchecked_scope
                .push("project_dead_code_reachability");
        }
        if report
            .coverage
            .issues
            .iter()
            .any(|issue| issue.kind == "grep_verification_limit")
        {
            coverage.unchecked_scope.push("grep_verification");
            coverage.unchecked_scope.push("project_dead_code_findings");
        }
        if mode == DiffMode::Impact
            && report
                .analysis_summary
                .pr_impact
                .as_ref()
                .is_some_and(|impact| impact.status != "complete")
        {
            coverage.unchecked_scope.push("pr_impact");
        }
        if coverage.warnings.is_empty() || coverage.status == "incomplete" {
            coverage.warnings.extend(
                report
                    .coverage
                    .issues
                    .iter()
                    .filter(|issue| !file_gap(issue))
                    .map(|issue| format!("{}: {}", issue.path, issue.message)),
            );
        }
        coverage
    }
}

fn evaluate_gate(
    report: &mut Report,
    coverage: &Assessment,
    strict: bool,
    require_complete: bool,
    force: bool,
    baseline_failure: Option<String>,
) -> u8 {
    let dead = report.categories.iter().count();
    let quality = report.quality.iter().flatten().count();
    let mut reasons = Vec::new();
    if strict && dead + quality > 0 {
        reasons.push(format!("Strict mode: {} issue(s) found", dead + quality));
    } else if !strict {
        for (label, count, limit) in [
            ("dead code", dead, report.gate_policy.max_dead_code),
            ("quality", quality, report.gate_policy.max_quality),
        ] {
            if let Some(limit) = limit
                && count > limit
            {
                reasons.push(format!(
                    "{count} {label} issue(s) exceed the limit of {limit}"
                ));
            }
        }
    }
    let (verdict, status, code) = if let Some(message) = baseline_failure {
        reasons = vec![format!("Baseline unavailable: {message}")];
        ("incomplete", "incomplete", 2)
    } else if coverage.status == "incomplete" || (require_complete && coverage.status != "complete")
    {
        reasons = coverage.warnings.clone();
        ("incomplete", "incomplete", 2)
    } else if reasons.is_empty() {
        ("passed", "evaluated", 0)
    } else {
        ("failed", "evaluated", 1)
    };
    let details: Vec<_> = if code == 2 {
        report
            .coverage
            .issues
            .iter()
            .map(|issue| format!("{}: {}", issue.path, issue.message))
            .collect()
    } else {
        report
            .categories
            .iter()
            .map(|item| {
                format!(
                    "{}:{}  unused {} {}",
                    item.file,
                    item.line,
                    item.kind,
                    item.subject()
                )
            })
            .chain(
                report
                    .quality
                    .iter()
                    .flatten()
                    .map(|item| format!("{}:{}  {}", item.file, item.line, item.message)),
            )
            .collect()
    };
    let forced = force && code == 1;
    let exit_code = if forced { 0 } else { code };
    report.gate = Some(json!({
        "passed": code == 0,
        "reasons": reasons,
        "scope": if strict {"local_strict_policy"} else {"local_configured_policy"},
        "status": status,
        "forced": forced,
        "verdict": verdict,
        "exit_code": exit_code,
        "strict": strict,
        "headline": format!("Quality Gate: {}", verdict.to_ascii_uppercase()),
        "reason_details": reasons.iter().map(|message| json!({"message":message,"details":details,"hint":null})).collect::<Vec<_>>(),
        "coverage_status": coverage.status,
        "coverage_warnings": coverage.warnings,
        "coverage_warning_details": coverage.warnings.iter().map(|message| json!({
            "message": message,
            "details": report.coverage.issues.iter().map(|issue| format!("{}: {}", issue.path, issue.message)).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
    }));
    exit_code
}
