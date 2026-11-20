//! Coarse Python/native boundary. The shared core owns analysis work.

use std::path::PathBuf;

use pyo3::exceptions::{PyRuntimeError, PyValueError};
use pyo3::prelude::*;
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
    let options = ScanOptions {
        source_roots: options.source_roots,
        policy,
        entry_file: options.entry_file,
        entry_modules: options.entry_modules,
        entry_callables: options.entry_callables,
        public_modules: options.public_modules,
        public_callables: options.public_callables,
        workers: options.workers,
    };
    py.detach(move || skarmory_core::scan(&path, &options).0.to_json())
        .map_err(|error| PyRuntimeError::new_err(error.to_string()))
}

#[pymodule]
fn _native(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add("__version__", skarmory_core::VERSION)?;
    module.add_function(wrap_pyfunction!(analyze, module)?)?;
    module.add_function(wrap_pyfunction!(scan, module)?)?;
    Ok(())
}
