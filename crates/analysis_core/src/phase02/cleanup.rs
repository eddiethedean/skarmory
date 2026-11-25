//! Live Python cleanup planning, validation, and execution.

mod comparison;
mod execution;
#[cfg(all(test, unix))]
mod tests;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::ops::Range;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::Value;
use similar::TextDiff;
use skarmory_python::cleanup;
use walkdir::WalkDir;

use super::{ConfigError, EvidenceInput, Options, Profile, Report, scan_selected};
use execution::{Check, CheckResult};

#[derive(Default)]
pub struct Settings {
    pub task: Option<String>,
    pub apply: bool,
    pub proof: Option<String>,
    pub test_command: Option<String>,
    pub config_file: Option<PathBuf>,
    pub workers: usize,
    pub cache_dir: Option<PathBuf>,
}

#[derive(Default)]
pub(super) struct Inputs {
    pub(super) sources: BTreeMap<String, String>,
    pub(super) policy: BTreeMap<String, blake3::Hash>,
}

impl Inputs {
    fn fingerprint(&self, skip_source: Option<&str>) -> blake3::Hash {
        let mut digest = blake3::Hasher::new();
        for (path, hash) in &self.policy {
            hash_field(&mut digest, b"policy");
            hash_field(&mut digest, path.as_bytes());
            hash_field(&mut digest, hash.as_bytes());
        }
        for (path, source) in &self.sources {
            if Some(path.as_str()) != skip_source {
                hash_field(&mut digest, b"source");
                hash_field(&mut digest, path.as_bytes());
                hash_field(&mut digest, source.as_bytes());
            }
        }
        digest.finalize()
    }
}

fn hash_field(digest: &mut blake3::Hasher, bytes: &[u8]) {
    digest.update(&(bytes.len() as u64).to_le_bytes());
    digest.update(bytes);
}

#[derive(Serialize)]
struct Task {
    id: String,
    file: String,
    line: usize,
    name: String,
    rule_id: &'static str,
    eligible: bool,
    source_fingerprint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
}

#[derive(Serialize)]
struct Preview {
    patch: String,
    proof: String,
    source_fingerprint: String,
    replacement_fingerprint: String,
}

struct PlannedEdit {
    removal: Range<usize>,
    statement: Range<usize>,
    proof: String,
    import_roots: Vec<String>,
}

struct Edit<'a> {
    source: String,
    replacement: String,
    plan: &'a PlannedEdit,
}

impl PlannedEdit {
    fn load<'a>(&'a self, root: &Path, task: &Task) -> Result<Edit<'a>, ConfigError> {
        let bytes = execution::read(root, Path::new(&task.file))?;
        if task.source_fingerprint.as_deref() != Some(&fingerprint(&bytes)) {
            return Err(ConfigError::new(
                "source changed while preparing cleanup preview",
            ));
        }
        let source = String::from_utf8(bytes).map_err(ConfigError::new)?;
        let mut replacement = source.clone();
        replacement.replace_range(self.removal.clone(), "");
        cleanup::validate_replacement(&replacement).map_err(ConfigError::new)?;
        Ok(Edit {
            source,
            replacement,
            plan: self,
        })
    }
}

impl Edit<'_> {
    fn preview(&self, file: &str) -> Preview {
        Preview {
            patch: TextDiff::from_lines(&self.source, &self.replacement)
                .unified_diff()
                .header(&format!("a/{file}"), &format!("b/{file}"))
                .to_string(),
            proof: self.plan.proof.clone(),
            source_fingerprint: fingerprint(self.source.as_bytes()),
            replacement_fingerprint: fingerprint(self.replacement.as_bytes()),
        }
    }

    fn is_deletion_only(&self) -> bool {
        let source = self.source.as_bytes();
        let removal = &self.plan.removal;
        (removal.start == 0 || source[removal.start - 1] == b'\n')
            && (removal.end == source.len() || source[removal.end - 1] == b'\n')
    }
}

#[derive(Serialize)]
struct Rescan {
    status: &'static str,
    original_present: Option<bool>,
    new_findings: Vec<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip)]
    rollback_proof_current: bool,
}

impl Rescan {
    fn compared(original_present: bool, new_findings: Vec<Value>) -> Self {
        Self {
            status: if original_present || !new_findings.is_empty() {
                "failed"
            } else {
                "passed"
            },
            original_present: Some(original_present),
            new_findings,
            error: None,
            rollback_proof_current: true,
        }
    }
}

#[derive(Serialize)]
struct Execution {
    status: &'static str,
    applied: bool,
    rolled_back: bool,
    tests: CheckResult,
    rescan: Rescan,
    limitations: Vec<String>,
}

#[derive(Serialize)]
pub struct CleanupReport {
    schema_version: u8,
    status: &'static str,
    analysis_errors: Vec<Value>,
    tasks: Vec<Task>,
    #[serde(skip_serializing_if = "Option::is_none")]
    edit: Option<Preview>,
    #[serde(skip_serializing_if = "Option::is_none")]
    execution: Option<Execution>,
}

pub struct Outcome {
    pub report: CleanupReport,
    pub exit_code: u8,
}

struct Candidate {
    task: Task,
    edit: Option<PlannedEdit>,
}

struct Scan {
    report: Report,
    inputs: Inputs,
}

struct RawSource {
    source: String,
    hash: blake3::Hash,
}

impl Scan {
    fn read(root: &Path, options: &Options) -> Result<Self, ConfigError> {
        let mut inputs = Inputs::default();
        let mut report = scan_selected(root, options, None, None, Some(&mut inputs))?;
        report.group_unused_class_methods();
        Ok(Self { report, inputs })
    }

    fn candidates(&self, root: &Path) -> Vec<Candidate> {
        let context = self.inputs.fingerprint(None);
        let mut providers = None;
        let rows = self
            .report
            .categories
            .iter()
            .filter(|row| matches!(row.rule_id, "SKY-U001" | "SKY-U002"))
            .collect::<Vec<_>>();
        let mut raw: BTreeMap<_, Result<RawSource, String>> = BTreeMap::new();
        for row in &rows {
            raw.entry(&row.file).or_insert_with(|| {
                let bytes = execution::read(root, Path::new(&row.file))
                    .map_err(|error| error.to_string())?;
                let hash = blake3::hash(&bytes);
                let source = String::from_utf8(bytes).map_err(|error| error.to_string())?;
                if self.inputs.sources.get(&row.file) != Some(&normalize(&source)) {
                    return Err(
                        "source changed after analysis or uses an unsupported byte encoding".into(),
                    );
                }
                Ok(RawSource { source, hash })
            });
        }
        let parsed = raw
            .iter()
            .map(|(&path, raw)| {
                let source = raw
                    .as_ref()
                    .map_err(Clone::clone)
                    .and_then(|raw| cleanup::Source::parse(&raw.source));
                (path, source)
            })
            .collect::<BTreeMap<_, _>>();
        rows.into_iter()
            .map(|row| {
                let current = &raw[&row.file];
                let mut key = blake3::Hasher::new();
                for field in [&row.file, row.rule_id, &row.identity_name] {
                    hash_field(&mut key, field.as_bytes());
                }
                let mut task = Task {
                    id: String::new(),
                    file: row.file.clone(),
                    line: row.line,
                    name: row.name.clone().unwrap_or_else(|| row.file.clone()),
                    rule_id: row.rule_id,
                    eligible: false,
                    source_fingerprint: current
                        .as_ref()
                        .ok()
                        .map(|source| format!("blake3:{}", source.hash)),
                    reason: None,
                };
                let edit = (|| {
                    let raw = current.as_ref().map_err(Clone::clone)?;
                    let source = parsed[&row.file].as_ref().map_err(Clone::clone)?;
                    let planned = source.plan(row.line, row.column, &task.name, row.rule_id)?;
                    hash_field(&mut key, planned.anchor.as_bytes());
                    if !planned.import_roots.is_empty() {
                        providers
                            .get_or_insert_with(|| Providers::read(root))
                            .as_ref()
                            .map_err(Clone::clone)?
                            .check(&planned.import_roots)?;
                    }
                    let mut proof = key.clone();
                    hash_field(&mut proof, root.as_os_str().as_encoded_bytes());
                    hash_field(&mut proof, context.as_bytes());
                    hash_field(&mut proof, raw.hash.as_bytes());
                    hash_field(&mut proof, &(planned.removal.start as u64).to_le_bytes());
                    hash_field(&mut proof, &(planned.removal.end as u64).to_le_bytes());
                    Ok::<_, String>(PlannedEdit {
                        removal: planned.removal,
                        statement: planned.statement,
                        proof: proof.finalize().to_hex().to_string(),
                        import_roots: planned.import_roots,
                    })
                })();
                task.id = key.finalize().to_hex().to_string();
                let edit = match edit {
                    Ok(edit) => {
                        task.eligible = true;
                        Some(edit)
                    }
                    Err(reason) => {
                        task.reason = Some(reason);
                        None
                    }
                };
                Candidate { task, edit }
            })
            .collect()
    }
}

fn fingerprint(bytes: &[u8]) -> String {
    format!("blake3:{}", blake3::hash(bytes))
}

fn normalize(source: &str) -> String {
    source.replace("\r\n", "\n").replace('\r', "\n")
}

struct Providers(BTreeMap<String, PathBuf>);

// Names reachable during initialization of the allowlisted CPython modules.
// Include platform-dependent providers even when cleanup runs on another OS.
fn shadow_names(root: &str) -> impl Iterator<Item = &str> {
    let dependencies = match root {
        "collections" => {
            "_abc _collections _collections_abc _operator abc itertools keyword operator reprlib"
        }
        "functools" => {
            "_abc _collections _collections_abc _functools _operator _types abc collections itertools keyword operator reprlib types"
        }
        "json" => {
            "_abc _collections _collections_abc _functools _json _operator _sre _types abc collections copyreg enum functools itertools keyword operator re reprlib types"
        }
        "math" => "_math_integer",
        "operator" => "_operator",
        "os" => {
            "_abc _collections_abc _stat _winapi abc errno genericpath nt ntpath posix posixpath stat warnings"
        }
        "pathlib" => {
            "_abc _collections _collections_abc _functools _math_integer _operator _sre _stat _types _winapi abc collections contextlib copyreg enum errno fcntl fnmatch functools genericpath glob grp io ipaddress itertools keyword math msvcrt nt ntpath operator os posix posixpath pwd re reprlib stat types urllib warnings"
        }
        "re" => {
            "_abc _collections _collections_abc _functools _operator _sre _types abc collections copyreg enum functools itertools keyword operator reprlib types"
        }
        "typing" => {
            "_abc _collections _collections_abc _functools _operator _sre _stat _types _typing _winapi abc collections contextlib copyreg enum errno functools genericpath itertools keyword nt ntpath operator os posix posixpath re reprlib stat types warnings"
        }
        _ => "",
    };
    std::iter::once(root).chain(dependencies.split_ascii_whitespace())
}

impl Providers {
    fn read(root: &Path) -> Result<Self, String> {
        // Python's import paths can include directories outside analyzer
        // selection. Check the bounded project tree independently.
        let guarded: BTreeSet<_> = cleanup::SAFE_IMPORT_ROOTS
            .iter()
            .flat_map(|name| shadow_names(name))
            .collect();
        let mut found = BTreeMap::<String, PathBuf>::new();
        for (index, entry) in WalkDir::new(root).min_depth(1).into_iter().enumerate() {
            if index >= 65_536 {
                return Err("import shadow check exceeded the directory entry limit".into());
            }
            let entry = entry.map_err(|error| error.to_string())?;
            let Some(name) = entry.file_name().to_str() else {
                continue;
            };
            let name = name.to_ascii_lowercase();
            let provider = match (name.split_once('.'), name.rsplit_once('.')) {
                (Some((module, _)), Some((_, "py" | "pyc" | "so" | "pyd"))) => module,
                _ => &name,
            };
            if guarded.contains(provider)
                && found
                    .get(provider)
                    .is_none_or(|path| entry.path() < path.as_path())
            {
                found.insert(provider.into(), entry.into_path());
            }
        }
        Ok(Self(found))
    }

    fn check(&self, names: &[String]) -> Result<(), String> {
        for root in names {
            if let Some((name, path)) =
                shadow_names(root).find_map(|name| self.0.get(name).map(|path| (name, path)))
            {
                return Err(format!(
                    "a project path may shadow {name} when importing {root}: {}",
                    path.display()
                ));
            }
        }
        Ok(())
    }
}

fn find_candidate<'a>(candidates: &'a [Candidate], id: &str) -> Result<&'a Candidate, ConfigError> {
    let mut matches = candidates
        .iter()
        .filter(|row| !id.is_empty() && row.task.id.starts_with(id));
    let first = matches
        .next()
        .ok_or_else(|| ConfigError::new("cleanup task is absent or stale"))?;
    if matches.next().is_some() {
        return Err(ConfigError::new("cleanup task prefix is ambiguous"));
    }
    Ok(first)
}

pub fn run(path: &Path, settings: &Settings) -> Result<Outcome, ConfigError> {
    if settings.apply && settings.task.is_none()
        || !settings.apply && (settings.proof.is_some() || settings.test_command.is_some())
    {
        return Err(ConfigError::new(
            "applying requires a task, and proof and test commands require explicit apply",
        ));
    }
    let check = settings
        .test_command
        .as_deref()
        .map(Check::parse)
        .transpose()?;
    if settings.apply && !cfg!(unix) {
        return Err(ConfigError::new("guarded cleanup writes require Unix"));
    }
    if !fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_dir()) {
        return Err(ConfigError::new(
            "cleanup requires a regular project directory",
        ));
    }
    let root = path.canonicalize().map_err(ConfigError::new)?;
    let options = Options {
        profile: Profile::SkylosPhase03,
        enable_quality: None,
        config_file: settings.config_file.clone(),
        workers: settings.workers.max(1),
        cache_dir: settings.cache_dir.clone(),
        trace_file: EvidenceInput::Disabled,
        coverage_file: EvidenceInput::Disabled,
        ..Options::default()
    };
    let scan = Scan::read(&root, &options)?;
    if !scan.report.is_complete() {
        return Ok(Outcome {
            exit_code: 2,
            report: CleanupReport {
                schema_version: 1,
                status: "incomplete",
                analysis_errors: scan.report.analysis_errors,
                tasks: Vec::new(),
                edit: None,
                execution: None,
            },
        });
    }
    let candidates = scan.candidates(&root);
    let mut report = CleanupReport {
        schema_version: 1,
        status: if candidates.is_empty() {
            "clear"
        } else {
            "review"
        },
        analysis_errors: Vec::new(),
        tasks: Vec::new(),
        edit: None,
        execution: None,
    };
    let mut exit_code = 0;
    if let Some(id) = &settings.task {
        let candidate = find_candidate(&candidates, id)?;
        let plan = candidate.edit.as_ref().ok_or_else(|| {
            ConfigError::new(
                candidate
                    .task
                    .reason
                    .as_deref()
                    .unwrap_or("task requires manual review"),
            )
        })?;
        let edit = plan.load(&root, &candidate.task)?;
        report.edit = Some(edit.preview(&candidate.task.file));
        if settings.apply {
            if settings
                .proof
                .as_ref()
                .is_some_and(|proof| *proof != plan.proof)
            {
                return Err(ConfigError::new("cleanup proof is stale"));
            }
            let fresh = Scan::read(&root, &options)?;
            if !fresh.report.is_complete() {
                return Err(ConfigError::new("cleanup preflight analysis is incomplete"));
            }
            let fresh_candidates = fresh.candidates(&root);
            let fresh_candidate = find_candidate(&fresh_candidates, &candidate.task.id)?;
            if !fresh_candidate
                .edit
                .as_ref()
                .is_some_and(|current| current.proof == plan.proof)
            {
                return Err(ConfigError::new(
                    "source or proof dependencies changed before apply",
                ));
            }
            let execution = apply(&root, &options, &fresh, candidate, &edit, check.as_ref())?;
            exit_code = match execution.status {
                "failed" => 1,
                "incomplete" => 2,
                _ => 0,
            };
            report.status = execution.status;
            report.execution = Some(execution);
        } else {
            report.status = "preview";
        }
    }
    report.tasks = candidates
        .into_iter()
        .map(|candidate| candidate.task)
        .collect();
    Ok(Outcome { report, exit_code })
}

fn apply(
    root: &Path,
    options: &Options,
    before: &Scan,
    candidate: &Candidate,
    edit: &Edit<'_>,
    check: Option<&Check>,
) -> Result<Execution, ConfigError> {
    let target = Path::new(&candidate.task.file);
    execution::replace(
        root,
        target,
        edit.source.as_bytes(),
        edit.replacement.as_bytes(),
    )?;
    let tests = check.map_or_else(CheckResult::skipped, |check| check.run(root));
    let rescan = compare_rescan(root, before, candidate, edit, Scan::read(root, options));
    Ok(finish_execution(root, candidate, edit, tests, rescan))
}

fn finish_execution(
    root: &Path,
    candidate: &Candidate,
    edit: &Edit<'_>,
    tests: CheckResult,
    rescan: Rescan,
) -> Execution {
    let target = Path::new(&candidate.task.file);
    let mut result = Execution {
        status: "applied_unverified",
        applied: true,
        rolled_back: false,
        tests,
        rescan,
        limitations: Vec::new(),
    };
    result.status = match (
        result.rescan.status,
        result.tests.status,
        result.tests.observed,
    ) {
        ("failed", _, _) | (_, "failed", _) => {
            if result.rescan.rollback_proof_current {
                rollback(root, target, edit, &mut result);
            } else if let Some(error) = &result.rescan.error {
                result.limitations.push(format!(
                    "Rollback was not applied: proof could not be revalidated: {error}"
                ));
            }
            "failed"
        }
        ("incomplete", _, _) | (_, "incomplete", _) => "incomplete",
        ("passed", _, _)
            if !execution::read(root, target)
                .is_ok_and(|bytes| bytes == edit.replacement.as_bytes()) =>
        {
            result
                .limitations
                .push("The source changed after the comparable rescan.".into());
            "incomplete"
        }
        ("passed", "passed", true) if edit.is_deletion_only() => {
            result
                .limitations
                .push("Deletion-only patch; changed-line execution was not measured.".into());
            "verified"
        }
        ("passed", "passed", true) => {
            result.limitations.push(
                "Patch replaces source lines; changed-line execution is required for verification."
                    .into(),
            );
            "applied_unverified"
        }
        _ => "applied_unverified",
    };
    result
}

fn compare_rescan(
    root: &Path,
    before: &Scan,
    candidate: &Candidate,
    edit: &Edit<'_>,
    after: Result<Scan, ConfigError>,
) -> Rescan {
    let incomplete = |error: String, rollback_proof_current| Rescan {
        status: "incomplete",
        original_present: None,
        new_findings: Vec::new(),
        error: Some(error),
        rollback_proof_current,
    };
    let after = match after {
        Ok(after) => after,
        Err(error) => return incomplete(error.to_string(), false),
    };
    if before.inputs.policy != after.inputs.policy {
        return incomplete(
            "configuration changed or another analysis input changed during cleanup validation"
                .into(),
            false,
        );
    }
    if !edit.plan.import_roots.is_empty()
        && let Err(error) =
            Providers::read(root).and_then(|providers| providers.check(&edit.plan.import_roots))
    {
        return incomplete(error, false);
    }
    if !after.report.is_complete() {
        return incomplete("post-edit analysis has incomplete coverage".into(), false);
    }
    // Other source changes prevent a comparable rescan. The inverse edit
    // still checks the exact applied target bytes before writing.
    let target = Some(candidate.task.file.as_str());
    if before.inputs.fingerprint(target) != after.inputs.fingerprint(target) {
        return incomplete(
            "other analyzed sources changed during cleanup validation".into(),
            true,
        );
    }
    let Some(source) = after.inputs.sources.get(&candidate.task.file) else {
        return incomplete("source is missing from the cleanup rescan".into(), true);
    };
    let original = &edit.source[edit.plan.statement.clone()];
    match cleanup::contains_statement(source, original) {
        Ok(true) => return Rescan::compared(true, Vec::new()),
        Ok(false) => {}
        Err(error) => return incomplete(error, true),
    }
    if source != &normalize(&edit.replacement) {
        return incomplete("source changed during cleanup validation".into(), true);
    }
    let new_findings = comparison::new_risks(
        &before.report,
        &after.report,
        &candidate.task.file,
        &edit.source,
        &edit.replacement,
        &edit.plan.removal,
        &edit.plan.statement,
    );
    Rescan::compared(false, new_findings)
}

fn rollback(root: &Path, target: &Path, edit: &Edit<'_>, result: &mut Execution) {
    match execution::replace(
        root,
        target,
        edit.replacement.as_bytes(),
        edit.source.as_bytes(),
    ) {
        Ok(()) => {
            result.rolled_back = true;
            result.applied = false;
        }
        Err(error) => result
            .limitations
            .push(format!("Rollback was not applied: {error}")),
    }
}
