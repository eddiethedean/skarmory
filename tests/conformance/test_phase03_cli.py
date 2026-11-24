import json
import subprocess
import sys
import unittest
from pathlib import Path
from tempfile import TemporaryDirectory
from unittest.mock import patch

from phase03_cli import RecursiveCaseCli


class RecursiveCaseCliTests(unittest.TestCase):
    def setUp(self):
        temporary = TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.project = Path(temporary.name).resolve()
        self.source = self.project / "worker.py"
        self.source.write_text("print('inactive')\n", encoding="utf-8")
        self.binary = self.project / "skarmory"
        self.cli = RecursiveCaseCli(self.binary)
        self.command = [
            sys.executable, "-c", "from skylos.cli import main; main()",
            str(self.project), "--format", "json", "--no-upload",
        ]

    def invoke(self):
        return self.cli.run(
            self.command, cwd=self.project, env={"PYTHONPATH": "checkout", "PATH": "/bin"},
            capture_output=True, text=True, timeout=30,
        )

    @patch("phase03_cli.subprocess.run")
    def test_keeps_fixture_and_uses_default_options(self, run):
        run.return_value = self.report()
        self.assertIs(self.invoke(), run.return_value)
        run.assert_called_once_with(
            [str(self.binary), str(self.project), "--profile", "skylos-0.3", "--json"],
            cwd=self.project, env={"PATH": "/bin"}, capture_output=True, text=True, timeout=30,
            check=False,
        )
        self.assertFalse(self.cli.proved)
        self.source.write_text("print('active')\n", encoding="utf-8")
        self.invoke()
        self.assertTrue(self.cli.proved)

    @patch("phase03_cli.subprocess.run")
    def test_rejects_changed_original_options(self, run):
        self.command.append("--no-grep-verify")
        with self.assertRaisesRegex(ValueError, "unexpected pinned CLI command"):
            self.invoke()
        run.assert_not_called()

    @patch("phase03_cli.subprocess.run")
    def test_two_identical_fixtures_do_not_prove_the_control_pair(self, run):
        run.return_value = self.report()
        self.invoke()
        self.invoke()
        self.assertFalse(self.cli.proved)

    @patch("phase03_cli.subprocess.run")
    def test_incomplete_scan_or_disabled_default_cannot_prove_control_pair(self, run):
        run.return_value = self.report()
        self.invoke()
        self.source.write_text("print('active')\n", encoding="utf-8")
        for failure in (
            self.report(coverage="incomplete"),
            self.report(grep_enabled=False),
            subprocess.CompletedProcess([], 2, stdout="", stderr="incomplete"),
        ):
            with self.subTest(failure=failure):
                run.return_value = failure
                self.invoke()
                self.assertFalse(self.cli.proved)

    @staticmethod
    def report(*, coverage="complete", grep_enabled=True):
        return subprocess.CompletedProcess([], 0, stdout=json.dumps({
            "coverage": {"status": coverage},
            "analysis_summary": {"grep_verify": {"enabled": grep_enabled}},
        }))


if __name__ == "__main__":
    unittest.main()
