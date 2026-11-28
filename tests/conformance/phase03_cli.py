"""Bridge pinned real-CLI cases to the candidate executable."""

import hashlib
import inspect
import json
import shlex
import subprocess
import sys
from pathlib import Path
from tempfile import TemporaryDirectory
from textwrap import dedent

CASE = (
    "test/test_reachability_integration_qa.py"
    "::test_real_cli_reports_dead_cycle_and_keeps_live_variant"
)
SDK_CLI_PARITY_CASE = (
    "test/test_reachability_integration_qa.py"
    "::test_unused_import_does_not_share_its_function_grep_verdict"
)
METHOD_SOURCE = "test/test_method_reachability.py"
METHOD_CLI_CASE = f"{METHOD_SOURCE}::test_default_cli_reports_methods_and_explains_unreachable_groups"
CONTROL_PAIR_CASES = {
    CASE,
    f"{METHOD_CLI_CASE}[same-name]",
    f"{METHOD_CLI_CASE}[cycle]",
}
WORKFLOW_SOURCES = {"test/test_pr_impact.py", "test/test_cli_json_ci_format.py"}
CLEANUP_SOURCE = "test/test_cleanup_workflow.py"
CLEANUP_CASES = {
    f"{CLEANUP_SOURCE}::{name}": actions
    for name, actions in (
        ("test_preview_then_apply_rescans_and_skipped_tests_stay_unverified", ("queue", "preview", "apply")),
        ("test_authorized_passing_tests_and_same_scope_rescan_produce_verified_receipt", ("queue", "verified")),
        ("test_failing_authorized_check_rolls_back_and_returns_failure", ("queue", "rollback")),
        ("test_actual_analyzer_patch_roundtrip", ("queue", "apply")),
        ("test_policy_changed_by_authorized_check_cannot_verify_a_fix", ("queue", "policy")),
    )
}
INVALID_IMPACT = (
    "test/test_pr_impact.py::test_impact_rejects_missing_diff_or_partial_head"
)
INVALID_IMPACT_ARGS = {
    f"{INVALID_IMPACT}[extra0]": ["--diff-mode", "impact"],
    f"{INVALID_IMPACT}[extra1]": [
        "--diff-mode", "impact", "--diff", "HEAD", "--diff-base", "HEAD",
    ],
}


class PairedSourceCli:
    def __init__(self, binary):
        self.binary = binary
        self.completed_sources = []

    @property
    def proved(self):
        return len(self.completed_sources) == len(set(self.completed_sources)) == 2

    def run(self, command, **kwargs):
        project = pinned_json_project(command)
        source = (project / "worker.py").read_bytes()
        completed = run_json_cli(self.binary, project, **kwargs)
        if completed.returncode == 0:
            report = json.loads(completed.stdout)
            if (
                report["coverage"]["status"] == "complete"
                and report["analysis_summary"]["grep_verify"]["enabled"] is True
            ):
                self.completed_sources.append(source)
        return completed


def pinned_json_project(command):
    # Translate the public interface while preserving the fixture and omitted options.
    if (
        len(command) != 7
        or command[:3] != [sys.executable, "-c", "from skylos.cli import main; main()"]
        or command[4:] != ["--format", "json", "--no-upload"]
    ):
        raise ValueError(f"unexpected pinned CLI command: {command!r}")
    project = Path(command[3])
    if not project.is_absolute():
        raise ValueError("pinned CLI project path must be absolute")
    return project


def run_json_cli(binary, project, **kwargs):
    # Keep the reference source checkout out of the native invocation.
    kwargs["env"] = {
        name: value for name, value in kwargs["env"].items() if name != "PYTHONPATH"
    }
    return subprocess.run(
        [str(binary), str(project), "--profile", "skylos-0.3", "--json"],
        check=False,
        **kwargs,
    )


class GrepParityCli:
    paths = ("importer.py", "worker.py")

    def __init__(self, binary, fixture):
        self.binary = binary
        self.fixture = fixture.resolve()
        self.cli_sources = None
        self.cli_report = None
        self.proved = False

    def sources(self):
        return tuple(
            hashlib.sha256((self.fixture / path).read_bytes()).hexdigest() for path in self.paths
        )

    def run(self, command, **kwargs):
        project = pinned_json_project(command)
        if project != self.fixture or self.cli_sources is not None:
            raise ValueError("mixed parity requires one native CLI scan of the pinned fixture")
        self.cli_sources = self.sources()
        completed = run_json_cli(self.binary, project, **kwargs)
        if self.sources() != self.cli_sources:
            raise ValueError("the native CLI scan changed the pinned fixture")
        if completed.returncode == 0:
            self.cli_report = json.loads(completed.stdout)
        return completed

    def verify_sdk(self, path, options, report):
        if (Path(path) != self.fixture or options != {"trace_file": False}
                or self.proved or self.cli_report is None or self.sources() != self.cli_sources):
            raise ValueError("mixed parity requires matched native CLI and installed SDK scans")
        fields = (
            "profile", "selected_paths", "coverage", "unused_functions", "unused_imports",
            "grep_verification_evidence",
        )
        if (
            report["profile"] != "skylos-5.7.0-python-dead-code-phase03"
            or sorted(report["selected_paths"]) != list(self.paths)
            or report["coverage"] != {
                "status": "complete", "selected_files": 2, "extracted_files": 2, "issues": [],
            }
            or any(
                item["analysis_summary"]["grep_verify"]["enabled"] is not True
                or item.get("runtime_evidence")
                for item in (report, self.cli_report)
            )
            or any(report.get(field) != self.cli_report.get(field) for field in fields)
        ):
            raise ValueError("the pinned CLI and SDK reports disagree on the import verification")
        self.proved = True


class WorkflowCaseCli:
    def __init__(self, binary, fixture, case_id):
        self.binary = binary
        self.fixture = fixture.resolve()
        self.invalid_args = INVALID_IMPACT_ARGS.get(case_id)
        self.invocations = 0
        self.verified = 0

    @property
    def proved(self):
        return self.invocations == self.verified > 0

    def run(self):
        self.invocations += 1
        launcher, source, *args = sys.argv
        source = Path(source)
        if launcher != "skylos" or not source.is_absolute() or (
            not source.resolve().is_relative_to(self.fixture)
        ):
            raise ValueError("pinned workflow CLI did not select its fixture")
        output = None
        if self.invalid_args is not None:
            if args != self.invalid_args:
                raise ValueError("pinned invalid impact invocation changed")
        else:
            for flag in ("--no-upload", "--no-provenance"):
                if args.count(flag) != 1:
                    raise ValueError(f"pinned workflow invocation requires {flag}")
                args.remove(flag)
            if "--output" in args:
                output = Path(args[args.index("--output") + 1])
                if not output.is_absolute() or not output.resolve().is_relative_to(self.fixture):
                    raise ValueError("pinned workflow output escaped its fixture")

        completed = subprocess.run(
            [str(self.binary), str(source), "--profile", "skylos-0.3", *args],
            capture_output=True, text=True, timeout=60, check=False,
        )
        if self.invalid_args is not None:
            if completed.returncode == 2 and not completed.stdout and completed.stderr.strip():
                self.verified += 1
        elif completed.returncode in {0, 1, 2}:
            report = json.loads(
                completed.stdout if output is None else output.read_text(encoding="utf-8")
            )
            if (
                report.get("profile") == "skylos-5.7.0-python-dead-code-phase03"
                and report.get("selected_paths")
                and "analysis_coverage" in report
            ):
                self.verified += 1

        sys.stdout.write(completed.stdout)
        sys.stderr.write(completed.stderr)
        if completed.returncode:
            raise SystemExit(completed.returncode)


class CleanupCaseCli:
    def __init__(self, binary, fixture, case_id, source=None):
        self.binary = binary
        self.fixture = fixture.resolve()
        self.source = source
        self.actions = CLEANUP_CASES[case_id]
        self.invocations = 0
        self.verified = 0
        self.task_id = None

    @property
    def proved(self):
        return self.invocations == self.verified == len(self.actions)

    def run(self, command, *, stdout, analyze_func=None):
        # The pinned fixtures may supply a synthetic scanner. The candidate
        # always obtains its findings and post-edit comparison from the CLI.
        if self.invocations >= len(self.actions) or not command:
            raise ValueError("unexpected pinned cleanup invocation")
        action = self.actions[self.invocations]
        self.invocations += 1
        source, *options = command
        if Path(source).resolve() != self.fixture or options[-2:] != ["--format", "json"]:
            raise ValueError("pinned cleanup CLI did not select its fixture and JSON format")
        expected = [] if action == "queue" else ["--task", self.task_id]
        if action not in {"queue", "preview"}:
            expected.append("--apply")
        checks = {
            "verified": [sys.executable, "-m", "pytest", "-q", "test_app.py"],
            "rollback": [sys.executable, "-c", "raise SystemExit(1)"],
            "policy": ["pytest"],
        }
        if action in checks:
            if (
                len(options) < 4 or options[-4] != "--test-cmd"
                or shlex.split(options[-3]) != checks[action]
            ):
                raise ValueError("pinned cleanup validation command changed")
            expected.extend(("--test-cmd", options[-3]))
        if options[:-2] != expected:
            raise ValueError("unexpected pinned cleanup options")

        completed = self.run_check(options, action)
        stdout.write(completed.stdout)
        sys.stderr.write(completed.stderr)
        if completed.stdout:
            self.verify(action, completed.returncode, json.loads(completed.stdout))
        return completed.returncode

    def run_check(self, options, action):
        def invoke(args):
            return subprocess.run(
                [str(self.binary), "cleanup", str(self.fixture), *args],
                cwd=self.fixture, capture_output=True, text=True, timeout=60, check=False,
            )

        if action != "policy":
            return invoke(options)

        from skylos.cleanup_checks import run_cleanup_tests as callback

        nonlocals = inspect.getclosurevars(callback).nonlocals
        if (
            callback.__name__ != "changed_policy" or self.source is None
            or Path(inspect.getfile(callback)).resolve() != self.source.resolve()
            or set(nonlocals) != {"tmp_path"} or nonlocals["tmp_path"].resolve() != self.fixture
            or (self.fixture / "pyproject.toml").exists()
        ):
            raise ValueError("pinned cleanup policy callback did not capture its fixture")
        # Run the original mutation inside a real authorized pytest child, at
        # the boundary that the reference monkeypatch replaces. Pytest observes
        # the callback returning; its synthetic receipt is unused.
        with TemporaryDirectory(prefix="skarmory-cleanup-policy-") as workspace:
            test = Path(workspace) / "test_policy_change.py"
            test.write_text(
                "from pathlib import Path\n\ntmp_path = Path.cwd()\n\n"
                + dedent(inspect.getsource(callback))
                + "\n\ndef test_policy_change():\n    changed_policy()\n",
                encoding="utf-8",
            )
            argv = [sys.executable, "-m", "pytest", "-q", str(test)]
            return invoke([*options[:-3], shlex.join(argv), *options[-2:]])

    def verify(self, action, returncode, report):
        if report.get("schema_version") != 1 or report.get("analysis_errors") != []:
            return
        if action == "queue":
            if returncode != 0 or report.get("status") != "review":
                return
            matches = [
                item for item in report.get("tasks", [])
                if item.get("file") == "app.py" and item.get("name") == "math"
                and item.get("rule_id") == "SKY-U002" and item.get("eligible") is True
            ]
            if (
                len(matches) != 1 or not matches[0].get("source_fingerprint")
                or not matches[0].get("id")
            ):
                return
            self.task_id = matches[0]["id"]
        elif action == "preview":
            if (
                returncode != 0 or report.get("status") != "preview"
                or not report.get("edit", {}).get("proof")
            ):
                return
        else:
            execution = report.get("execution", {})
            rescan = execution.get("rescan", {})
            checks = execution.get("tests", {})
            if action != "policy" and (
                rescan.get("status") != "passed" or rescan.get("original_present") is not False
            ):
                return
            if action == "rollback":
                if (
                    returncode != 1 or checks.get("returncode") != 1
                    or checks.get("status") != "failed" or execution.get("rolled_back") is not True
                ):
                    return
            elif action in {"verified", "policy"}:
                if (
                    checks.get("status") != "passed" or checks.get("observed") is not True
                    or checks.get("evidence") != "pytest_junit" or checks.get("count") != 1
                ):
                    return
                if action == "verified" and returncode != 0:
                    return
                if action == "policy" and (
                    returncode != 2 or rescan.get("status") != "incomplete"
                    or not (self.fixture / "pyproject.toml").is_file()
                    or execution.get("rolled_back") is not False
                ):
                    return
            elif returncode != 0 or checks.get("status") != "skipped":
                return
        self.verified += 1
