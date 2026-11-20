//! Shared engine boundary for Skarmory.
//!
//! This scaffold deliberately refuses analysis until a real implementation can
//! return findings together with explicit coverage and evidence.

use std::error::Error;
use std::fmt;
use std::path::Path;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Analysis is not implemented in the initial project scaffold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AnalysisUnavailable;

impl fmt::Display for AnalysisUnavailable {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(
            "analysis is not implemented yet; this is the Skarmory development scaffold",
        )
    }
}

impl Error for AnalysisUnavailable {}

/// Reserved engine entry point; it never reports a successful scan.
///
/// The return type will become a structured report when Phase 0 introduces the
/// analysis model. This development interface is not a stable API.
pub fn analyze(_path: &Path) -> Result<(), AnalysisUnavailable> {
    Err(AnalysisUnavailable)
}
