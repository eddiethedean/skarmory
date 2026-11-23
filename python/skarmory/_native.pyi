"""Typing boundary for the native extension."""

from collections.abc import Sequence
from typing import Literal, TypedDict

__version__: str

class _ScanOptions(TypedDict):
    source_roots: Sequence[str]
    policy: Literal["application", "library"]
    entry_file: str | None
    entry_modules: Sequence[str]
    entry_callables: Sequence[str]
    public_modules: Sequence[str]
    public_callables: Sequence[str]
    workers: int
    cache_dir: str | None
    grep_verify: bool

class _SkylosOptions(TypedDict):
    phase03: bool
    config_file: str | None
    exclude_folders: Sequence[str]
    skip_generated: bool
    generated_keep_paths: Sequence[str]
    confidence: int
    all_advice: bool
    workers: int
    cache_dir: str | None
    grep_verify: bool | None
    trace_file: str | None
    trace_disabled: bool
    coverage_file: str | None
    coverage_disabled: bool

def analyze(path: str) -> str: ...
def scan(path: str, options: _ScanOptions) -> str: ...
def scan_skylos(path: str, options: _SkylosOptions) -> str: ...
