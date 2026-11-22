"""Run original pinned pytest cases against Skylos and an installed SDK wheel."""

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

PORT = "tests/conformance/run_phase03_seed.py"
ROOT = Path(__file__).resolve().parents[2]
SCRIPT = Path(__file__).resolve()


# Run in an isolated interpreter so the candidate never imports the Skylos
# checkout or the Skarmory source tree. Check the installed package against the
# exact wheel before letting the original assertions exercise its SDK.
CHECK_WHEEL = """
from pathlib import Path, PurePosixPath
import sys
from zipfile import ZipFile
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


def port_cases(args):
    from check_dead_code_gate import read_inventory

    inventory, digest, variants, expected = read_inventory(args.inventory)
    if args.variant not in variants:
        raise ValueError(f"unknown inventory variant: {args.variant}")
    variant = variants[args.variant]
    if variant["layer"] != "sdk" or variant["backend"] != "python":
        raise ValueError("the seed runner requires a Python backend SDK variant")
    cases = {key: case for key, case in expected[args.variant].items() if case["port"] == PORT}
    if not cases:
        raise ValueError(f"no seed SDK port mappings for {args.variant}")
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

    _, _, _, cases = port_cases(args)
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
            self.scans = set()
            self.missing_grep = set()

        @pytest.hookimpl(wrapper=True)
        def pytest_runtest_call(self, item):
            key = item.nodeid
            case = cases[key]
            trace = AssertionTrace(args.upstream / case["source"])
            old_trace = sys.gettrace()
            original = item.module.analyze

            def analyze(path, *, trace_file=None, grep_verify=None, **options):
                if grep_verify is not False or trace_file is not False:
                    self.missing_grep.add(key)
                    pytest.skip("Phase 0.2 SDK cannot preserve the upstream evidence policy")
                scanned = subprocess.run(
                    [str(args.sdk_python), "-I", "-c", SCAN],
                    input=json.dumps([path, {"grep_verify": False, **options}]),
                    cwd=path, env=environment(), text=True, capture_output=True, timeout=60,
                    check=True,
                )
                report = json.loads(scanned.stdout)
                if report["coverage"]["status"] != "complete":
                    pytest.fail(f"candidate scan incomplete: {report['coverage']}")
                self.scans.add(key)
                return scanned.stdout

            if args.mode == "candidate":
                item.module.analyze = analyze
            try:
                sys.settrace(trace)
                return (yield)
            finally:
                sys.settrace(old_trace)
                item.module.analyze = original
                self.lines[key] = {f"{case['source']}:{line}" for line in trace.completed}

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
            prerequisites[required] = "passed" if proven else "blocked"
        reached = [assertion for assertion in case["assertions"] if assertion in lines]
        outcome = capture.outcomes.get(key, "error")
        if key in capture.missing_grep and outcome == "skipped":
            outcome = "blocked"
        proved = (
            key in capture.scans
            and all(value == "passed" for value in prerequisites.values())
            and reached == case["assertions"]
        )
        if args.mode == "candidate" and outcome == "passed" and not proved:
            outcome = "blocked"
        report[key] = {"status": outcome, "assertions": reached, "prerequisites": prerequisites}
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
            str(args.inventory), "--variant", args.variant, "--sdk-python", str(args.sdk_python),
        ]
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
                    if mode == "candidate":
                        subprocess.run(
                            [str(args.sdk_python), "-I", "-c", CHECK_WHEEL, str(args.artifact)],
                            cwd=work, env=env, timeout=60, check=True,
                        )
                    run = subprocess.run(command, cwd=args.upstream, env=env, timeout=180, check=False)
                    if run.returncode not in {0, 1}:
                        raise ValueError(f"{mode} pytest runner exited {run.returncode}")
                    reports[mode] = json.loads(Path(command[-1]).read_text(encoding="utf-8"))
                except (OSError, ValueError, subprocess.SubprocessError) as error:
                    errors.append(f"{mode}: {error}")

    if sha256(args.artifact) != artifact_digest:
        errors.append("candidate wheel changed during execution")
    results = []
    for case_id, case in cases.items():
        reference = reports.get("reference", {}).get(case_id, {})
        candidate = reports.get("candidate", {}).get(case_id, {})
        if reference.get("status") == "passed" and reference["assertions"] != case["assertions"]:
            errors.append(f"{case_id}: reference passed before all mapped assertions ran")
        results.append({
            "case_id": case_id,
            "reference_status": reference.get("status", "error"),
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
    parser.add_argument("--sdk-python", required=True, type=Path)
    parser.add_argument("--artifact", type=Path)
    parser.add_argument("--candidate-revision")
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--mode", choices=("reference", "candidate"), help=argparse.SUPPRESS)
    args = parser.parse_args()
    for name in ("upstream", "inventory", "sdk_python", "output", "artifact"):
        path = getattr(args, name)
        if path is not None:
            # Preserve the venv interpreter symlink and its sys.prefix.
            setattr(args, name, path.absolute())
    if not args.mode and (args.artifact is None or args.candidate_revision is None):
        parser.error("--artifact and --candidate-revision are required for a receipt")
    try:
        return pytest_worker(args) if args.mode else write_receipt(args)
    except (OSError, ValueError) as error:
        parser.error(str(error))


if __name__ == "__main__":
    raise SystemExit(main())
