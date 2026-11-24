"""The declared offline dead-code profiles for Skylos 5.7.0."""

from collections.abc import Sequence
from os import fspath
from typing import Literal

from ..api import SourcePath, _load_native


def analyze(
    path: SourcePath,
    conf: int = 60,
    exclude_folders: Sequence[str] | None = None,
    *,
    profile: Literal["skylos", "skylos-0.3"] = "skylos",
    config_file: SourcePath | None = None,
    skip_generated: bool = False,
    generated_keep_paths: Sequence[str] | None = None,
    all_advice: bool = True,
    enable_quality: bool = False,
    experimental_javascript: bool = False,
    grep_verify: bool | None = None,
    grep_cache: bool = False,
    trace_file: SourcePath | Literal[False] | None = None,
    coverage_file: SourcePath | Literal[False] | None = None,
    enable_dependency_hallucinations: bool = False,
    workers: int = 1,
    cache_dir: SourcePath | None = None,
) -> str:
    """Return the declared Skylos-compatible result as a JSON string.

    The default Phase 0.2 profile disables grep verification. Select
    `profile="skylos-0.3"` to enable it when omitted, or pass
    `grep_verify=False` to disable it explicitly. Phase 0.3 accepts the pinned
    `grep_cache` argument and rebuilds grep evidence from the current snapshot.
    Pass `cache_dir` to reuse validated file-local facts across scans.
    Phase 0.3 reads project-root .skylos_trace and .coverage files by
    default. Pass a path relative to the project root or False to disable
    the corresponding input. The report keeps runtime provenance and
    rejected evidence separate from static coverage.
    Phase 0.3 scans Python by default. Pass `experimental_javascript=True`
    to also check unused JavaScript and TypeScript files in a bounded package
    graph rooted at the package.json main entry.
    Pass `enable_quality=True` in Phase 0.3 to check unused Python dependencies.
    """
    if profile not in ("skylos", "skylos-0.3"):
        raise ValueError("profile must be skylos or skylos-0.3")
    phase03 = profile == "skylos-0.3"
    if not phase03 and experimental_javascript:
        raise TypeError("experimental JavaScript analysis requires profile='skylos-0.3'")
    if not phase03 and enable_quality:
        raise TypeError("quality scanning requires profile='skylos-0.3'")
    if not phase03 and (grep_verify or grep_cache):
        raise ValueError(
            "The Skylos Phase 0.2 profile requires disabled grep verification and caching"
        )
    if enable_dependency_hallucinations:
        raise ValueError("The Skylos profile requires disabled dependency analysis")
    if not phase03 and (trace_file is not None or coverage_file is not None):
        raise TypeError("runtime evidence options require profile='skylos-0.3'")
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
            "phase03": phase03,
            "experimental_javascript": experimental_javascript,
            "enable_quality": enable_quality,
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
            "grep_verify": grep_verify,
            "trace_file": None if trace_file is None or trace_file is False else fspath(trace_file),
            "trace_disabled": trace_file is False,
            "coverage_file": (
                None if coverage_file is None or coverage_file is False else fspath(coverage_file)
            ),
            "coverage_disabled": coverage_file is False,
        },
    )
