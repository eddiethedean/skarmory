"""Python interface to the Phase 0 native prototype."""

import json
from os import PathLike, fspath


class AnalysisUnavailableError(RuntimeError):
    """The native engine could not be loaded."""


def analyze(path: str | PathLike[str]) -> dict[str, object]:
    """Return a prototype report. Check coverage.status before using findings."""
    try:
        from . import _native
    except ImportError as error:
        raise AnalysisUnavailableError(
            "Skarmory's native extension is not built. "
            "Build the development package with `python -m pip install .`."
        ) from error

    return json.loads(_native.analyze(fspath(path)))
