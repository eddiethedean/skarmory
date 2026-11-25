"""Run pinned report-facing framework assertions against Skylos or an installed SDK."""

import argparse
import json
import os
from pathlib import Path
import subprocess
import sys
from tempfile import TemporaryDirectory
import tomllib

from phase03_framework import framework_definitions, framework_report


BASELINE = Path(__file__).with_name("baselines") / "skylos-5.7.0.toml"
SIGNALS = "test/test_django_signal_liveness.py"
UNITTEST = "test/test_unittest_discovery_liveness.py"
WEB = "test/test_web_framework_liveness.py"
CASES = [
    SIGNALS,
    UNITTEST,
    WEB,
    "test/test_django_app_repro_liveness.py::test_repro_14_unittest_testcase_is_live_helper_is_not",
    "test/test_analyzer.py::TestIgnorePragmas::test_analyze_suppresses_pytest_plugin_hook_methods",
    "test/test_analyzer.py::TestIgnorePragmas::test_analyze_suppresses_additional_pytest_plugin_hooks",
    "test/test_analyzer.py::TestIgnorePragmas::test_analyze_suppresses_pytest_hook_parameters",
]
PRIVATE_SIGNAL_PROBES = (
    "test_projects_without_signal_candidates_do_not_construct_signal_registry",
    "test_receiver_spelling_does_not_supply_framework_root_before_project_proof",
    "test_signal_proof_is_not_cut_off_by_many_unrelated_called_functions",
    "test_repeated_registration_wrapper_graph_is_memoized",
)
SDK_SCAN = """
import json
from importlib.machinery import EXTENSION_SUFFIXES
import sys
from pathlib import Path
from skarmory import _native
from skarmory.compat.skylos import analyze

native = Path(_native.__file__).resolve()
assert native.is_relative_to(Path(sys.prefix).resolve()), native
assert native.name.endswith(tuple(EXTENSION_SUFFIXES)), native
path, options = json.load(sys.stdin)
print(analyze(path, profile="skylos-0.3", **options))
"""


def environment():
    env = {
        key: value
        for key, value in os.environ.items()
        if not key.startswith(("SKYLOS_", "PYTEST_", "PYTHON"))
        and key != "DJANGO_SETTINGS_MODULE"
    }
    env["PYTEST_DISABLE_PLUGIN_AUTOLOAD"] = "1"
    return env


def check_checkout(upstream):
    baseline = tomllib.loads(BASELINE.read_text(encoding="utf-8"))["reference"]
    revision = subprocess.check_output(
        ["git", "rev-parse", "HEAD"], cwd=upstream, text=True
    ).strip()
    if revision != baseline["git_revision"]:
        raise ValueError(
            f"Expected pinned Skylos revision {baseline['git_revision']}, got {revision}"
        )
    modified = subprocess.check_output(
        [
            "git",
            "status",
            "--porcelain",
            "--untracked-files=all",
            "--",
            "test",
            "skylos",
        ],
        cwd=upstream,
        text=True,
    )
    if modified:
        raise ValueError(
            f"Pinned test or analyzer source has local changes:\n{modified}"
        )


def run(upstream, sdk_python, selected, backend, diagnostics_path=None):
    import pytest

    sys.dont_write_bytecode = True
    sys.path.insert(0, str(upstream))
    if backend == "python":
        sys.modules["skylos_fast"] = None
    from skylos.core.fast import FAST_AVAILABLE

    if FAST_AVAILABLE != (backend == "skylos_fast"):
        raise RuntimeError(f"Pinned reference backend {backend} is unavailable")
    env = environment()
    os.environ.clear()
    os.environ.update(env)
    os.chdir(upstream)

    class Replay:
        def __init__(self):
            self.skipped = []
            self.diagnostics = {}

        @pytest.hookimpl(wrapper=True)
        def pytest_runtest_call(self, item):
            if sdk_python is None:
                return (yield)

            from skylos.analyzer import Skylos
            from test import web_framework_fixtures

            scans = self.diagnostics[item.nodeid] = []

            def analyze(path, **options):
                candidate_path = Path(path).absolute()
                command = subprocess.run(
                    [sdk_python, "-I", "-c", SDK_SCAN],
                    env=os.environ.copy(),
                    text=True,
                    cwd=candidate_path
                    if candidate_path.is_dir()
                    else candidate_path.parent,
                    input=json.dumps([str(candidate_path), options], default=os.fspath),
                    capture_output=True,
                    check=False,
                    timeout=60,
                )
                if command.returncode:
                    pytest.fail(
                        f"Installed SDK failed to analyze {path}:\n{command.stderr}"
                    )
                report = json.loads(command.stdout)
                scans.append({
                    "coverage": report["coverage"],
                    "analysis_errors": report["analysis_errors"],
                })
                return json.dumps(framework_report(report))

            def analyze_instance(analyzer, path, thr=60, exclude_folders=None, **options):
                options.setdefault("grep_verify", True)
                options.setdefault("grep_cache", True)
                result = analyze(path, conf=thr, exclude_folders=exclude_folders, **options)
                analyzer.defs = framework_definitions(json.loads(result))
                return result

            with pytest.MonkeyPatch.context() as patch:
                patch.setattr(Skylos, "analyze", analyze_instance)
                if hasattr(item.module, "_scan"):
                    patch.setattr(web_framework_fixtures, "analyze", analyze)
                if hasattr(item.module, "analyze"):
                    patch.setattr(item.module, "analyze", analyze)
                try:
                    result = yield
                finally:
                    if scans:
                        item.add_report_section(
                            "call", "installed SDK coverage", json.dumps(scans, indent=2)
                        )
                if not scans:
                    pytest.fail("Pinned assertion did not call the installed SDK")
                if any(scan["coverage"]["status"] != "complete" for scan in scans):
                    pytest.fail("The pinned case ran with incomplete installed SDK coverage")
                return result

        def pytest_runtest_logreport(self, report):
            if report.skipped:
                self.skipped.append(report.nodeid)

    args = ["-q", "-p", "no:cacheprovider"]
    if sdk_python:
        args.append("--runxfail")
    if not selected:
        args.extend(f"--deselect={SIGNALS}::{name}" for name in PRIVATE_SIGNAL_PROBES)
    replay = Replay()
    with TemporaryDirectory(prefix="skarmory-phase03-framework-") as base:
        code = pytest.main(
            [
                *args,
                "--basetemp",
                str(Path(base).resolve() / "fixtures"),
                *(selected or CASES),
            ],
            plugins=[replay],
        )
    if diagnostics_path is not None:
        diagnostics_path.write_text(json.dumps(replay.diagnostics, indent=2) + "\n", encoding="utf-8")
    if sdk_python and replay.skipped:
        print(f"Candidate cases skipped: {', '.join(replay.skipped)}", file=sys.stderr)
        return 1
    return int(code)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("upstream", type=Path)
    parser.add_argument(
        "--sdk-python",
        type=Path,
        help="Python in an isolated installed-wheel environment",
    )
    parser.add_argument(
        "--backend", choices=("python", "skylos_fast"), default="python"
    )
    parser.add_argument(
        "--case",
        action="append",
        help="Override the starting report-facing case selection",
    )
    parser.add_argument(
        "--diagnostics", type=Path, help="Write per-case SDK coverage and analysis errors"
    )
    args = parser.parse_args()
    if args.diagnostics and not args.sdk_python:
        parser.error("--diagnostics requires --sdk-python")
    upstream = args.upstream.resolve()
    check_checkout(upstream)
    return run(
        upstream,
        args.sdk_python.absolute() if args.sdk_python else None,
        args.case,
        args.backend,
        args.diagnostics.absolute() if args.diagnostics else None,
    )


if __name__ == "__main__":
    raise SystemExit(main())
