"""Run original pinned pytest cases against Skylos and candidate artifacts."""

import argparse
import ast
import hashlib
import json
import os
import platform
import subprocess
import sys
from pathlib import Path
from tempfile import TemporaryDirectory

from phase03_cli import CASE as CLI_CASE
from phase03_cli import RecursiveCaseCli

PORT = "tests/conformance/run_phase03_seed.py"
ROOT = Path(__file__).resolve().parents[2]
SCRIPT = Path(__file__).resolve()


# Run in an isolated interpreter so the candidate never imports the Skylos
# checkout or the Skarmory source tree. Check the installed package against the
# exact wheel before letting the original assertions exercise its SDK.
CHECK_WHEEL = """
from pathlib import Path, PurePosixPath
import platform
import sys
from zipfile import ZipFile

expected = sys.argv[2]
observed = f"{sys.platform}-{platform.machine()}-cp{sys.version_info.major}{sys.version_info.minor}"
implementation = platform.python_implementation()
if implementation != "CPython" or observed != expected:
    raise SystemExit(f"Required candidate SDK platform {expected}, found {implementation} {observed}")

import skarmory
from skarmory import _native

root = Path(skarmory.__file__).resolve().parent.parent
native = Path(_native.__file__).resolve().relative_to(root).as_posix()
with ZipFile(sys.argv[1]) as wheel:
    files = [
        info for info in wheel.infolist()
        if info.filename.startswith("skarmory/") and not info.is_dir()
    ]
    if native not in {info.filename for info in files}:
        raise SystemExit("Installed native extension is absent from the candidate wheel")
    for info in files:
        path = root.joinpath(*PurePosixPath(info.filename).parts)
        if path.read_bytes() != wheel.read(info):
            raise SystemExit(f"Installed package differs from the wheel: {info.filename}")
"""
SCAN = """
import json
import sys
from skarmory.compat.skylos import analyze
path, options = json.load(sys.stdin)
print(analyze(path, **options))
"""


def environment():
    env = {
        name: value for name, value in os.environ.items()
        if not name.startswith(("SKYLOS_", "PYTEST_", "PYTHON"))
    }
    env.update(PYTEST_DISABLE_PLUGIN_AUTOLOAD="1", PYTHONHASHSEED="0")
    return env


def check_candidate_wheel(sdk_python, artifact, expected_platform, cwd, env):
    result = subprocess.run(
        [str(sdk_python), "-I", "-c", CHECK_WHEEL, str(artifact), expected_platform],
        cwd=cwd, env=env, text=True, capture_output=True, timeout=60, check=False,
    )
    if result.returncode:
        raise ValueError(result.stderr.strip() or "candidate SDK wheel validation failed")


def scan_candidate(sdk_python, path, options):
    def json_value(value):
        if isinstance(value, os.PathLike):
            return os.fspath(value)
        if isinstance(value, dict):
            return {key: json_value(item) for key, item in value.items()}
        if isinstance(value, (list, tuple)):
            return [json_value(item) for item in value]
        return value

    return subprocess.run(
        [str(sdk_python), "-I", "-c", SCAN],
        input=json.dumps(json_value([path, {**options, "profile": "skylos-0.3"}])),
        cwd=path, env=environment(), text=True, capture_output=True, timeout=60,
        check=True,
    ).stdout


def port_cases(args):
    from check_dead_code_gate import read_inventory

    inventory, digest, variants, expected = read_inventory(args.inventory)
    if args.variant not in variants:
        raise ValueError(f"unknown inventory variant: {args.variant}")
    variant = variants[args.variant]
    layer = variant["layer"]
    if layer not in {"sdk", "cli"} or variant["backend"] != "python":
        raise ValueError("the seed runner requires a Python backend SDK or CLI variant")
    cases = {key: case for key, case in expected[args.variant].items() if case["port"] == PORT}
    if not cases:
        raise ValueError(f"no seed port mappings for {args.variant}")
    if layer == "cli" and set(cases) != {CLI_CASE}:
        raise ValueError("the CLI port only supports the original recursive control pair")
    if layer == "sdk" and args.sdk_python is None:
        raise ValueError("--sdk-python is required for the SDK variant")
    return inventory, digest, variant, cases


class AssertionTrace:
    """Record source lines only as they finish without raising an exception."""

    def __init__(self, source):
        self.source = str(source.resolve())
        self.pending = {}
        self.completed = set()

    def __call__(self, frame, event, _arg):
        if frame.f_code.co_filename != self.source:
            return None
        if event == "exception":
            self.pending.pop(frame, None)
        elif event in {"line", "return"}:
            if frame in self.pending:
                self.completed.add(self.pending.pop(frame))
            if event == "line":
                self.pending[frame] = frame.f_lineno
        return self


def pytest_worker(args):
    import pytest

    _, _, variant, cases = port_cases(args)
    sys.path.insert(0, str(args.upstream))
    sys.modules["skylos_fast"] = None
    from skylos.core.fast import FAST_AVAILABLE

    if FAST_AVAILABLE:
        raise RuntimeError("reference case requires the Python backend")
    for source in {case["source"] for case in cases.values()}:
        tree = ast.parse((args.upstream / source).read_text(encoding="utf-8"))
        source_assertions = {
            f"{source}:{node.lineno}" for node in ast.walk(tree) if isinstance(node, ast.Assert)
        }
        for case in cases.values():
            if case["source"] == source and not set(case["assertions"]) <= source_assertions:
                raise ValueError(f"{case['id']}: inventory points outside original assertions")

    class Capture:
        def __init__(self):
            self.outcomes = {}
            self.lines = {}
            self.failed_assertions = {}
            self.scans = set()
            self.grep_scans = set()
            self.grep_disables = set()
            self.trace_imports = set()
            self.trace_disables = set()
            self.trace_rejections = set()

        @pytest.hookimpl(wrapper=True)
        def pytest_runtest_call(self, item):
            key = item.nodeid
            case = cases[key]
            trace = AssertionTrace(args.upstream / case["source"])
            old_trace = sys.gettrace()

            def analyze(path, **options):
                scanned = scan_candidate(args.sdk_python, path, options)
                report = json.loads(scanned)
                if report["coverage"]["status"] != "complete":
                    pytest.fail(f"candidate scan incomplete: {report['coverage']}")
                self.scans.add(key)
                grep_enabled = report["analysis_summary"]["grep_verify"]["enabled"]
                if grep_enabled is True:
                    self.grep_scans.add(key)
                if options.get("grep_verify") is False and grep_enabled is False:
                    self.grep_disables.add(key)
                if any(
                    evidence["kind"] == "runtime_trace"
                    for evidence in report.get("runtime_evidence", [])
                ):
                    self.trace_imports.add(key)
                if (
                    options.get("trace_file") is False
                    and not report.get("runtime_evidence")
                    and not report.get("runtime_evidence_issues")
                ):
                    self.trace_disables.add(key)
                if any(
                    issue["kind"] == "runtime_evidence_rejected"
                    for issue in report.get("runtime_evidence_issues", [])
                ):
                    self.trace_rejections.add(key)
                return scanned

            cli = None
            with pytest.MonkeyPatch.context() as patch:
                if args.mode == "candidate":
                    if variant["layer"] == "cli":
                        cli = RecursiveCaseCli(args.artifact)
                        patch.setattr(item.module, "subprocess", cli)
                    else:
                        patch.setattr(item.module, "analyze", analyze)
                try:
                    sys.settrace(trace)
                    return (yield)
                finally:
                    sys.settrace(old_trace)
                    if cli is not None and cli.proved:
                        self.scans.add(key)
                    self.lines[key] = {f"{case['source']}:{line}" for line in trace.completed}

        def pytest_runtest_makereport(self, item, call):
            if (
                args.mode == "reference"
                and call.when == "call"
                and call.excinfo
                and call.excinfo.errisinstance(AssertionError)
            ):
                case = cases[item.nodeid]
                failure = call.excinfo.traceback[-1]
                if failure.path.resolve() == (args.upstream / case["source"]).resolve():
                    assertion = f"{case['source']}:{failure.lineno + 1}"
                    if assertion in case["assertions"]:
                        self.failed_assertions[item.nodeid] = assertion

        def pytest_runtest_logreport(self, report):
            key = report.nodeid
            if report.when != "call" and not report.passed:
                self.outcomes[key] = "error" if report.failed else (
                    "xfailed" if hasattr(report, "wasxfail") else "skipped"
                )
            elif report.skipped:
                self.outcomes[key] = "xfailed" if hasattr(report, "wasxfail") else "skipped"
            elif report.failed:
                self.outcomes[key] = (
                    "xpassed" if report.longreprtext.startswith("[XPASS(strict)]") else "failed"
                )
            elif report.when == "call":
                self.outcomes[key] = "xpassed" if hasattr(report, "wasxfail") else "passed"

    capture = Capture()
    pytest_args = ["-q", "-p", "no:cacheprovider", *cases]
    if args.mode == "candidate":
        pytest_args.insert(0, "--runxfail")
    status = pytest.main(pytest_args, plugins=[capture])
    report = {}
    for key, case in cases.items():
        lines = capture.lines.get(key, set())
        prerequisites = {}
        for required in case["prerequisites"]:
            proven = required in lines
            if required == "installed-sdk":
                proven = args.mode == "candidate"
            elif required == "native-cli-control-pair":
                proven = key in capture.scans
            elif required == "grep-verification":
                proven = key in capture.grep_scans
            elif required == "grep-verification-disabled":
                proven = key in capture.grep_disables
            elif required == "runtime-trace-import":
                proven = key in capture.trace_imports
            elif required == "runtime-trace-disabled":
                proven = key in capture.trace_disables
            elif required == "runtime-trace-rejected":
                proven = key in capture.trace_rejections
            prerequisites[required] = "passed" if proven else "blocked"
        reached = [assertion for assertion in case["assertions"] if assertion in lines]
        outcome = capture.outcomes.get(key, "error")
        proved = (
            key in capture.scans
            and all(value == "passed" for value in prerequisites.values())
            and reached == case["assertions"]
        )
        if args.mode == "candidate" and outcome == "passed" and not proved:
            outcome = "blocked"
        report[key] = {
            "status": outcome,
            "assertions": reached,
            "failed_assertion": capture.failed_assertions.get(key),
            "prerequisites": prerequisites,
        }
    args.output.write_text(json.dumps(report), encoding="utf-8")
    return int(status)


def write_receipt(args):
    from check_dead_code_gate import sha256, validate_revision

    inventory, digest, variant, cases = port_cases(args)
    revision = inventory["reference_revision"]
    validate_revision(args.candidate_revision, "candidate revision")
    artifact_digest = sha256(args.artifact)
    env = environment()
    with TemporaryDirectory(prefix="skarmory-phase03-") as temporary:
        work = Path(temporary)
        commands = {}
        reports = {}
        errors = []
        common = [
            sys.executable, str(SCRIPT), "--upstream", str(args.upstream), "--inventory",
            str(args.inventory), "--variant", args.variant, "--artifact", str(args.artifact),
        ]
        if args.sdk_python is not None:
            common.extend(("--sdk-python", str(args.sdk_python)))
        for mode in ("reference", "candidate"):
            output = work / (mode + ".json")
            commands[mode] = [*common, "--mode", mode, "--output", str(output)]

        try:
            platform_id = f"{sys.platform}-{platform.machine()}-cp{sys.version_info.major}{sys.version_info.minor}"
            if platform.python_implementation() != "CPython" or variant["platform"] != platform_id:
                raise ValueError(f"required platform {variant['platform']}, found {platform_id}")
            head = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=args.upstream, text=True)
            if head.strip() != revision:
                raise ValueError("Skylos checkout is at the wrong revision")
            if subprocess.check_output(["git", "status", "--porcelain"], cwd=args.upstream, text=True):
                raise ValueError("Skylos checkout has uncommitted changes")
            lock_digest = hashlib.sha256((args.upstream / "uv.lock").read_bytes()).hexdigest()
            if lock_digest not in variant["environment"]:
                raise ValueError("upstream lock digest differs from the inventory")
        except (OSError, ValueError, subprocess.SubprocessError) as error:
            errors.append(str(error))
        else:
            for mode, command in commands.items():
                try:
                    if mode == "candidate" and variant["layer"] == "sdk":
                        check_candidate_wheel(
                            args.sdk_python, args.artifact, variant["platform"], work, env,
                        )
                    run = subprocess.run(command, cwd=args.upstream, env=env, timeout=180, check=False)
                    if run.returncode not in {0, 1}:
                        raise ValueError(f"{mode} pytest runner exited {run.returncode}")
                    reports[mode] = json.loads(Path(command[-1]).read_text(encoding="utf-8"))
                except (OSError, ValueError, subprocess.SubprocessError) as error:
                    errors.append(f"{mode}: {error}")

    if sha256(args.artifact) != artifact_digest:
        errors.append("candidate artifact changed during execution")
    results = []
    for case_id, case in cases.items():
        reference = reports.get("reference", {}).get(case_id, {})
        candidate = reports.get("candidate", {}).get(case_id, {})
        if reference.get("status") == "passed" and reference["assertions"] != case["assertions"]:
            errors.append(f"{case_id}: reference passed before all mapped assertions ran")
        results.append({
            "case_id": case_id,
            "reference_status": reference.get("status", "error"),
            "reference_assertions": reference.get("assertions", []),
            "reference_failed_assertion": reference.get("failed_assertion"),
            "status": candidate.get("status", "error"),
            "assertions": candidate.get("assertions", []),
            "prerequisites": candidate.get(
                "prerequisites", dict.fromkeys(case["prerequisites"], "blocked")
            ),
        })
    receipt = {
        "schema_version": 1,
        "reference_revision": revision,
        "candidate_revision": args.candidate_revision,
        "inventory_sha256": digest,
        "variant": args.variant,
        "environment": variant["environment"],
        "artifact_sha256": artifact_digest,
        "runner_status": "error" if errors else "passed",
        "runner_errors": errors,
        "reference_command": commands["reference"],
        "candidate_command": commands["candidate"],
        "results": results,
    }
    args.output.write_text(json.dumps(receipt, indent=2) + "\n", encoding="utf-8")
    for error in errors:
        print(error, file=sys.stderr)
    # Leave reconciliation and the publication verdict to the gate.
    return 0


def main():
    sys.path.insert(0, str(ROOT / "scripts"))
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--upstream", required=True, type=Path)
    parser.add_argument("--inventory", required=True, type=Path)
    parser.add_argument("--variant", required=True)
    parser.add_argument("--sdk-python", type=Path)
    parser.add_argument("--artifact", required=True, type=Path)
    parser.add_argument("--candidate-revision")
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--mode", choices=("reference", "candidate"), help=argparse.SUPPRESS)
    args = parser.parse_args()
    for name in ("upstream", "inventory", "sdk_python", "output", "artifact"):
        path = getattr(args, name)
        if path is not None:
            # Preserve the venv interpreter symlink and its sys.prefix.
            setattr(args, name, path.absolute())
    if not args.mode and args.candidate_revision is None:
        parser.error("--candidate-revision is required for a receipt")
    try:
        return pytest_worker(args) if args.mode else write_receipt(args)
    except (OSError, ValueError) as error:
        parser.error(str(error))


if __name__ == "__main__":
    raise SystemExit(main())
