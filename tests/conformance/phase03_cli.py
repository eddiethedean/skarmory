"""Bridge pinned real-CLI cases to the candidate executable."""

import json
import subprocess
import sys
from pathlib import Path

CASE = (
    "test/test_reachability_integration_qa.py"
    "::test_real_cli_reports_dead_cycle_and_keeps_live_variant"
)
WORKFLOW_SOURCES = {"test/test_pr_impact.py", "test/test_cli_json_ci_format.py"}
INVALID_IMPACT = (
    "test/test_pr_impact.py::test_impact_rejects_missing_diff_or_partial_head"
)
INVALID_IMPACT_ARGS = {
    f"{INVALID_IMPACT}[extra0]": ["--diff-mode", "impact"],
    f"{INVALID_IMPACT}[extra1]": [
        "--diff-mode", "impact", "--diff", "HEAD", "--diff-base", "HEAD",
    ],
}


class RecursiveCaseCli:
    def __init__(self, binary):
        self.binary = binary
        self.completed_sources = []

    @property
    def proved(self):
        return len(self.completed_sources) == len(set(self.completed_sources)) == 2

    def run(self, command, **kwargs):
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
        source = (project / "worker.py").read_bytes()
        # The reference needs PYTHONPATH to import its source checkout. The native
        # candidate receives only the scan path and no source-checkout injection.
        kwargs["env"] = {
            name: value for name, value in kwargs["env"].items()
            if name != "PYTHONPATH"
        }
        completed = subprocess.run(
            [str(self.binary), str(project), "--profile", "skylos-0.3", "--json"],
            check=False,
            **kwargs,
        )
        if completed.returncode == 0:
            report = json.loads(completed.stdout)
            if (
                report["coverage"]["status"] == "complete"
                and report["analysis_summary"]["grep_verify"]["enabled"] is True
            ):
                self.completed_sources.append(source)
        return completed


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
