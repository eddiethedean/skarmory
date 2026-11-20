"""Skarmory: native Python code analysis for the documented preview scope."""

from ._version import __version__
from .api import AnalysisUnavailableError, analyze

__all__ = ["AnalysisUnavailableError", "__version__", "analyze"]
