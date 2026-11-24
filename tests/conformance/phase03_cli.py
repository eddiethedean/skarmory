"""Bridge the pinned real-CLI reachability case to the candidate executable."""

import json
import subprocess
import sys
from pathlib import Path

CASE = (
    "test/test_reachability_integration_qa.py"
    "::test_real_cli_reports_dead_cycle_and_keeps_live_variant"
)


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
