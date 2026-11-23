"""Opt-in collection of current-thread Python call observations."""

from collections import Counter
from collections.abc import Iterator
from contextlib import contextmanager
import inspect
import json
from pathlib import Path
import sys
from tempfile import NamedTemporaryFile
from types import FrameType

from .api import SourcePath


@contextmanager
def collect_trace(
    project: SourcePath,
    output: SourcePath = ".skylos_trace",
) -> Iterator[None]:
    """Save calls made by the caller's code as an unverified v1 trace.

    Only Python functions from the named project directory and the current
    thread are recorded. A relative output is resolved from that directory.
    The caller selects and runs the workload inside the context.
    """
    root = Path(project).resolve(strict=True)
    if not root.is_dir():
        raise ValueError("project must be a directory")
    if sys.getprofile() is not None:
        raise RuntimeError("trace collection requires an inactive Python profiler")
    path = root / output
    calls: Counter[tuple[str, str, int]] = Counter()
    exceeded_limit = False

    def record(frame: FrameType, event: str, _argument: object) -> None:
        nonlocal exceeded_limit
        code = frame.f_code
        if event == "call" and code.co_flags & inspect.CO_NEWLOCALS:
            filename = Path(code.co_filename)
            if not filename.is_absolute():
                filename = Path.cwd() / filename
            if filename.is_relative_to(root):
                key = (
                    filename.relative_to(root).as_posix(),
                    code.co_name,
                    code.co_firstlineno,
                )
                if key in calls or len(calls) < 32_768:
                    calls[key] += 1
                else:
                    exceeded_limit = True

    try:
        sys.setprofile(record)
        yield
    finally:
        sys.setprofile(None)
    payload = json.dumps(
        {
            "version": 1,
            "calls": [
                {"file": file, "function": function, "line": line, "count": count}
                for (file, function, line), count in sorted(calls.items())
            ],
        },
        ensure_ascii=True,
    )
    if exceeded_limit or len(payload) > 1024 * 1024:
        raise ValueError("runtime trace exceeds the supported artifact limits")
    with NamedTemporaryFile(mode="w", encoding="utf-8", dir=path.parent, delete=False) as stream:
        temporary = Path(stream.name)
        try:
            stream.write(payload)
            stream.close()
            temporary.replace(path)
        finally:
            stream.close()
            temporary.unlink(missing_ok=True)
