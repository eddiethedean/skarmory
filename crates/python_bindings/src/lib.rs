//! Coarse Python/native boundary. The shared core owns analysis work.

use std::path::PathBuf;

use pyo3::exceptions::{PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use skarmory_core::phase02;
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
    let options = phase02::Options {
        profile: if options.phase03 {
            phase02::Profile::SkylosPhase03
        } else {
            phase02::Profile::SkylosPhase02
        },
        config_file: options.config_file,
        excludes: options.exclude_folders,
        include_generated: Some(!options.skip_generated),
        generated_keep_paths: options.generated_keep_paths,
        confidence: Some(options.confidence),
        all_advice: options.all_advice,
        workers: options.workers,
        cache_dir: options.cache_dir,
        grep_verify: options.grep_verify,
        trace_file: evidence_input(options.trace_file, options.trace_disabled),
        coverage_file: evidence_input(options.coverage_file, options.coverage_disabled),
        ..phase02::Options::default()
    };
    py.detach(move || {
        phase02::scan(&path, &options)
            .map_err(|error| PyValueError::new_err(error.to_string()))?
            .to_json()
            .map_err(|error| PyRuntimeError::new_err(error.to_string()))
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
