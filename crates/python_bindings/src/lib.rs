//! Coarse Python/native boundary. The shared core owns analysis work.

use std::path::PathBuf;

use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;

#[pyfunction]
fn analyze(py: Python<'_>, path: PathBuf) -> PyResult<String> {
    py.detach(move || skarmory_core::analyze(&path).to_json())
        .map_err(|error| PyRuntimeError::new_err(error.to_string()))
}

#[pymodule]
fn _native(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add("__version__", skarmory_core::VERSION)?;
    module.add_function(wrap_pyfunction!(analyze, module)?)?;
    Ok(())
}
