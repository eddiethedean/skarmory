import json
import subprocess
import sys
import unittest
from contextlib import redirect_stderr, redirect_stdout
from io import StringIO
from pathlib import Path
from tempfile import TemporaryDirectory
from unittest.mock import patch

from phase03_cli import INVALID_IMPACT, RecursiveCaseCli, WorkflowCaseCli


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


class WorkflowCaseCliTests(unittest.TestCase):
    def setUp(self):
        temporary = TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.fixture = Path(temporary.name).resolve()
        self.binary = self.fixture / "skarmory"
        self.command = ["skylos", str(self.fixture)]

    @patch("phase03_cli.subprocess.run")
    def test_preserves_impact_and_output_options_and_checks_the_report(self, run):
        output = self.fixture / "report.json"
        output.write_text(json.dumps({
            "profile": "skylos-5.7.0-python-dead-code-phase03",
            "selected_paths": ["app.py"],
            "analysis_coverage": {"status": "complete"},
        }), encoding="utf-8")
        options = ["--diff", "HEAD", "--diff-mode", "impact", "--output", str(output)]
        cli = WorkflowCaseCli(self.binary, self.fixture, "mapped-workflow")
        run.return_value = subprocess.CompletedProcess([], 0, stdout="", stderr="")
        with patch.object(sys, "argv", [*self.command, "--no-upload", "--no-provenance", *options]):
            cli.run()
            self.assertTrue(cli.proved)
            output.write_text("{}", encoding="utf-8")
            cli.run()
            self.assertFalse(cli.proved)
        self.assertEqual(run.call_args.args[0], [
            str(self.binary), str(self.fixture), "--profile", "skylos-0.3", *options,
        ])

    @patch("phase03_cli.subprocess.run")
    def test_invalid_impact_requires_a_real_cli_error(self, run):
        cli = WorkflowCaseCli(self.binary, self.fixture, f"{INVALID_IMPACT}[extra0]")
        options = ["--diff-mode", "impact"]
        with patch.object(sys, "argv", [*self.command, *options, "--diff", "HEAD"]):
            with self.assertRaisesRegex(ValueError, "invalid impact invocation changed"):
                cli.run()
        run.assert_not_called()

        cli = WorkflowCaseCli(self.binary, self.fixture, f"{INVALID_IMPACT}[extra0]")
        run.return_value = subprocess.CompletedProcess([], 2, stdout="", stderr="requires --diff")
        with (
            patch.object(sys, "argv", [*self.command, *options]),
            redirect_stdout(StringIO()),
            redirect_stderr(StringIO()),
            self.assertRaises(SystemExit) as raised,
        ):
            cli.run()
        self.assertEqual(raised.exception.code, 2)
        self.assertTrue(cli.proved)


if __name__ == "__main__":
    unittest.main()
