"""The declared offline Python dead-code profile for Skylos 5.7.0."""

from collections.abc import Sequence
from os import fspath

from ..api import SourcePath, _load_native


def analyze(
    path: SourcePath,
    conf: int = 60,
    exclude_folders: Sequence[str] | None = None,
    *,
    config_file: SourcePath | None = None,
    skip_generated: bool = False,
    generated_keep_paths: Sequence[str] | None = None,
    all_advice: bool = True,
    grep_verify: bool = False,
    grep_cache: bool = False,
    enable_dependency_hallucinations: bool = False,
    workers: int = 1,
    cache_dir: SourcePath | None = None,
) -> str:
    """Return the declared Skylos-compatible result as a JSON string.

    The offline profile defaults to disabled grep verification, grep caching,
    and dependency analysis. Incomplete scans retain their coverage diagnostics
    in the returned JSON. Invalid options or configuration raise ValueError.
    """
    if grep_verify or grep_cache or enable_dependency_hallucinations:
        raise ValueError(
            "The Skylos profile requires disabled grep verification, grep caching, "
            "and dependency analysis"
        )
    if not isinstance(conf, int):
        raise TypeError("conf must be an integer")
    if not 0 <= conf <= 100:
        raise ValueError("conf must be between 0 and 100")
    for name, values in (
        ("exclude_folders", exclude_folders),
        ("generated_keep_paths", generated_keep_paths),
    ):
        if isinstance(values, (str, bytes)):
            raise TypeError(f"{name} must be a sequence, such as [{values!r}]")

    native = _load_native()
    return native.scan_skylos(
        fspath(path),
        {
            "config_file": fspath(config_file) if config_file is not None else None,
            "exclude_folders": exclude_folders if exclude_folders is not None else (),
            "skip_generated": skip_generated,
            "generated_keep_paths": (
                generated_keep_paths if generated_keep_paths is not None else ()
            ),
            "confidence": conf,
            "all_advice": all_advice,
            "workers": workers,
            "cache_dir": fspath(cache_dir) if cache_dir is not None else None,
        },
    )
