"""Skarmory: fast Python code analysis, powered by Rust.

This initial scaffold does not implement analysis. See the repository roadmap.
"""

from ._version import __version__
from .api import AnalysisUnavailableError, analyze

__all__ = ["AnalysisUnavailableError", "__version__", "analyze"]
