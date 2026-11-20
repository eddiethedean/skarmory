//! Coarse Python/native boundary. The shared core owns analysis work.

use std::path::Path;

use pyo3::exceptions::PyNotImplementedError;
use pyo3::prelude::*;

#[pyfunction]
fn analyze(py: Python<'_>, path: String) -> PyResult<()> {
    py.detach(move || skarmory_core::analyze(Path::new(&path)))
        .map_err(|error| PyNotImplementedError::new_err(error.to_string()))
}

#[pymodule]
fn _native(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add("__version__", skarmory_core::VERSION)?;
    module.add_function(wrap_pyfunction!(analyze, module)?)?;
    Ok(())
}
