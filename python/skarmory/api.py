"""Initial SDK boundary; the planned report API is documented, not implemented."""

from os import PathLike, fspath
from typing import NoReturn


class AnalysisUnavailableError(RuntimeError):
    """The native engine is unavailable or cannot yet perform analysis."""


def analyze(path: str | PathLike[str]) -> NoReturn:
    """Request analysis, currently always raising AnalysisUnavailableError.

    A missing native extension and an unimplemented scanner are explicit
    failures. Neither returns an empty list or a successful scan report.
    """
    try:
        from . import _native
    except ImportError as error:
        raise AnalysisUnavailableError(
            "Skarmory's native extension is not built. "
            "Build the development package with `python -m pip install .`. "
            "Analysis is not implemented in this scaffold."
        ) from error

    try:
        _native.analyze(fspath(path))
    except NotImplementedError as error:
        raise AnalysisUnavailableError(str(error)) from error

    raise AnalysisUnavailableError("The native engine did not produce an analysis report.")
