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

def analyze(path: str) -> str: ...
def scan(path: str, options: _ScanOptions) -> str: ...
