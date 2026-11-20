"""Python interface to the native dead-code analyzer."""

from collections.abc import Sequence
import json
from os import PathLike, fspath
from typing import Literal, cast

from .report import NativeReport


SourcePath = str | PathLike[str]


class AnalysisUnavailableError(RuntimeError):
    """The native engine could not be loaded."""


def _load_native():
    try:
        from . import _native
    except ImportError as error:
        raise AnalysisUnavailableError(
            "Skarmory's native extension is not built. "
            "Build the development package with `python -m pip install .`."
        ) from error
    return _native


def analyze(
    path: SourcePath,
    *,
    source_roots: Sequence[SourcePath] = (),
    policy: Literal["application", "library"] = "application",
    entry_file: SourcePath | None = None,
    entry_modules: Sequence[str] = (),
    entry_callables: Sequence[str] = (),
    public_modules: Sequence[str] = (),
    public_callables: Sequence[str] = (),
    workers: int = 1,
    cache_dir: SourcePath | None = None,
    phase0: bool = False,
) -> NativeReport:
    """Analyze selected Python sources and return the native report.

    Declare source roots in import search order. Specify callable roots as
    `"module:function"`. Check `coverage.status` before using findings.
    Pass `cache_dir` to reuse validated file-local facts.
    Set `phase0=True` to use the flat-project prototype with its original report.
    """
    native = _load_native()

    if phase0:
        if (
            source_roots
            or policy != "application"
            or entry_file is not None
            or entry_modules
            or entry_callables
            or public_modules
            or public_callables
            or workers != 1
            or cache_dir is not None
        ):
            raise ValueError("Phase 0 does not accept preview scan options")
        return cast(NativeReport, json.loads(native.analyze(fspath(path))))

    for name, values in (
        ("source_roots", source_roots),
        ("entry_modules", entry_modules),
        ("entry_callables", entry_callables),
        ("public_modules", public_modules),
        ("public_callables", public_callables),
    ):
        if isinstance(values, (str, bytes)):
            raise TypeError(f"{name} must be a sequence, such as [{values!r}]")
    serialized = native.scan(
        fspath(path),
        {
            "source_roots": [fspath(root) for root in source_roots],
            "policy": policy,
            "entry_file": fspath(entry_file) if entry_file is not None else None,
            "entry_modules": entry_modules,
            "entry_callables": entry_callables,
            "public_modules": public_modules,
            "public_callables": public_callables,
            "workers": workers,
            "cache_dir": fspath(cache_dir) if cache_dir is not None else None,
        },
    )
    return cast(NativeReport, json.loads(serialized))
