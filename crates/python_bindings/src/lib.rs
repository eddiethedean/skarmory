//! Coarse Python/native boundary. The shared core owns analysis work.

use std::path::PathBuf;

use pyo3::exceptions::{PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use skarmory_core::phase02::{self, workflow};
use skarmory_core::{ScanOptions, ScanPolicy};

#[derive(FromPyObject)]
#[pyo3(from_item_all)]
struct Options {
    source_roots: Vec<PathBuf>,
    policy: String,
    entry_file: Option<PathBuf>,
    entry_modules: Vec<String>,
    entry_callables: Vec<String>,
    public_modules: Vec<String>,
    public_callables: Vec<String>,
    workers: usize,
    cache_dir: Option<PathBuf>,
    grep_verify: bool,
}

#[derive(FromPyObject)]
#[pyo3(from_item_all)]
struct SkylosOptions {
    phase03: bool,
    #[pyo3(default)]
    experimental_javascript: bool,
    #[pyo3(default)]
    enable_quality: bool,
    #[pyo3(default)]
    dependency_search_paths: Vec<PathBuf>,
    config_file: Option<PathBuf>,
    exclude_folders: Vec<String>,
    skip_generated: bool,
    generated_keep_paths: Vec<String>,
    confidence: u8,
    all_advice: bool,
    workers: usize,
    cache_dir: Option<PathBuf>,
    grep_verify: Option<bool>,
    trace_file: Option<PathBuf>,
    trace_disabled: bool,
    coverage_file: Option<PathBuf>,
    coverage_disabled: bool,
    #[pyo3(default)]
    workflow: Option<WorkflowOptions>,
}

#[derive(FromPyObject)]
#[pyo3(from_item_all)]
struct WorkflowOptions {
    #[pyo3(default)]
    changed_files: Option<Vec<PathBuf>>,
    diff: Option<String>,
    diff_mode: String,
    baseline: Option<PathBuf>,
    baseline_ref: Option<String>,
    write_baseline: Option<PathBuf>,
    rules: Vec<String>,
    severity: Option<String>,
    gate: bool,
    strict: bool,
    require_complete: bool,
    force: bool,
    compact: bool,
}

impl WorkflowOptions {
    fn settings(self) -> PyResult<workflow::Settings> {
        let diff_mode = match self.diff_mode.as_str() {
            "lines" => workflow::DiffMode::Lines,
            "files" => workflow::DiffMode::Files,
            "impact" => workflow::DiffMode::Impact,
            _ => {
                return Err(PyValueError::new_err(
                    "diff_mode must be lines, files or impact",
                ));
            }
        };
        if self.baseline.is_some() && self.baseline_ref.is_some() {
            return Err(PyValueError::new_err(
                "baseline and baseline_ref are mutually exclusive",
            ));
        }
        let baseline = self
            .baseline
            .map(workflow::BaselineInput::File)
            .or_else(|| self.baseline_ref.map(workflow::BaselineInput::Revision));
        let severity = self
            .severity
            .as_deref()
            .map(workflow::Severity::parse)
            .transpose()
            .map_err(|error| PyValueError::new_err(error.to_string()))?;
        Ok(workflow::Settings {
            changed_files: self.changed_files,
            diff: self.diff,
            diff_mode,
            baseline,
            write_baseline: self.write_baseline,
            rules: self.rules,
            severity,
            gate: self.gate,
            strict: self.strict,
            require_complete: self.require_complete,
            force: self.force,
        })
    }
}

#[pyfunction]
fn analyze(py: Python<'_>, path: PathBuf) -> PyResult<String> {
    py.detach(move || skarmory_core::analyze(&path).to_json())
        .map_err(|error| PyRuntimeError::new_err(error.to_string()))
}

#[pyfunction]
fn scan(py: Python<'_>, path: PathBuf, options: Options) -> PyResult<String> {
    let policy = match options.policy.as_str() {
        "application" => ScanPolicy::Application,
        "library" => ScanPolicy::Library,
        _ => {
            return Err(PyValueError::new_err(
                "policy must be application or library",
            ));
        }
    };
    let cache_dir = options.cache_dir;
    let options = ScanOptions {
        source_roots: options.source_roots,
        policy,
        entry_file: options.entry_file,
        entry_modules: options.entry_modules,
        entry_callables: options.entry_callables,
        public_modules: options.public_modules,
        public_callables: options.public_callables,
        workers: options.workers,
        grep_verify: options.grep_verify,
    };
    py.detach(move || {
        if let Some(directory) = cache_dir {
            skarmory_core::scan_cached(&path, &options, &directory).0
        } else {
            skarmory_core::scan(&path, &options).0
        }
        .to_json()
    })
    .map_err(|error| PyRuntimeError::new_err(error.to_string()))
}

fn evidence_input(path: Option<PathBuf>, disabled: bool) -> phase02::EvidenceInput {
    if disabled {
        phase02::EvidenceInput::Disabled
    } else {
        path.map_or(
            phase02::EvidenceInput::Default,
            phase02::EvidenceInput::Path,
        )
    }
}

#[pyfunction]
fn scan_skylos(py: Python<'_>, path: PathBuf, options: SkylosOptions) -> PyResult<String> {
    let phase03 = options.phase03;
    let compact = options
        .workflow
        .as_ref()
        .is_some_and(|workflow| workflow.compact);
    let settings = options
        .workflow
        .map(WorkflowOptions::settings)
        .transpose()?;
    let options = phase02::Options {
        profile: if phase03 {
            phase02::Profile::SkylosPhase03
        } else {
            phase02::Profile::SkylosPhase02
        },
        experimental_javascript: options.experimental_javascript,
        config_file: options.config_file,
        dependency_search_paths: options.dependency_search_paths,
        excludes: options.exclude_folders,
        include_generated: Some(!options.skip_generated),
        generated_keep_paths: options.generated_keep_paths,
        confidence: Some(options.confidence),
        all_advice: options.all_advice,
        enable_quality: Some(options.enable_quality),
        workers: options.workers,
        cache_dir: options.cache_dir,
        grep_verify: options.grep_verify,
        trace_file: evidence_input(options.trace_file, options.trace_disabled),
        coverage_file: evidence_input(options.coverage_file, options.coverage_disabled),
        ..phase02::Options::default()
    };
    py.detach(move || {
        let mut report = if phase03 {
            workflow::run(&path, &options, &settings.unwrap_or_default())
                .map(|outcome| outcome.report)
        } else {
            phase02::scan(&path, &options)
        }
        .map_err(|error| PyValueError::new_err(error.to_string()))?;
        let json = if compact {
            report.to_json_ci()
        } else {
            report.to_json()
        };
        json.map_err(|error| PyRuntimeError::new_err(error.to_string()))
    })
}

#[pymodule]
fn _native(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add("__version__", skarmory_core::VERSION)?;
    module.add_function(wrap_pyfunction!(analyze, module)?)?;
    module.add_function(wrap_pyfunction!(scan, module)?)?;
    module.add_function(wrap_pyfunction!(scan_skylos, module)?)?;
    Ok(())
}
