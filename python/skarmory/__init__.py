"""Skarmory: native Python code analysis for the documented preview scope."""

from ._version import __version__
from .api import AnalysisUnavailableError, analyze
from .report import NativeReport, Phase0Report, PreviewReport

__all__ = [
    "AnalysisUnavailableError",
    "NativeReport",
    "Phase0Report",
    "PreviewReport",
    "__version__",
    "analyze",
]
