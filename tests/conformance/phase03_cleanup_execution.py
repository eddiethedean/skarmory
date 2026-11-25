"""Route selected pinned cleanup execution contracts through the native CLI."""

import inspect
import json
import shlex
import subprocess
import sys
from pathlib import Path
from tempfile import TemporaryDirectory
from textwrap import dedent

EXECUTION_SOURCE = "test/test_cleanup_execution.py"
LIMITS_SOURCE = "test/test_cleanup_fstring_verification.py"
CLEANUP_EXECUTION_CASES = {
    f"{EXECUTION_SOURCE}::{name}": action
    for name, action in (
        ("test_failed_explicit_test_rolls_back_exact_applied_bytes", "rollback"),
        ("test_failed_test_preserves_a_user_change_instead_of_rollback", "concurrent"),
        ("test_test_command_unavailable_is_incomplete_never_passed", "missing"),
        ("test_complete_rescan_and_authorized_tests_are_required_for_verified", "verified"),
        ("test_rescan_alone_does_not_claim_runtime_verification", "rescan_only"),
        ("test_zero_exit_without_observed_tests_cannot_verify_a_cleanup", "opaque"),
        ("test_original_anchor_returning_during_rescan_is_not_verified", "restored"),
    )
}
CLEANUP_EXECUTION_CASES[
    f"{LIMITS_SOURCE}::test_deletion_only_verification_explains_its_coverage_limit"
] = "deletion_limit"


class CleanupExecutionCli:
    def __init__(self, binary, fixture, case_id, source):
        self.binary = binary
        self.fixture = fixture.resolve()
        self.source = source.resolve()
        self.action = CLEANUP_EXECUTION_CASES[case_id]
        self.task_id = None
        self.edit = None
        self.proved = False

    def run(self, *options):
        completed = subprocess.run(
            [str(self.binary), "cleanup", str(self.fixture), *options, "--format", "json"],
            cwd=self.fixture, capture_output=True, text=True, timeout=60, check=False,
        )
        if not completed.stdout:
            raise ValueError(completed.stderr.strip() or "native cleanup did not report its outcome")
        report = json.loads(completed.stdout)
        if report.get("schema_version") != 1 or report.get("analysis_errors") != []:
            raise ValueError("native cleanup did not produce a complete planning scan")
        return completed.returncode, report

    def plan(self, task, root):
        if Path(root).resolve() != self.fixture or self.edit is not None:
            raise ValueError("unexpected pinned cleanup planning target")
        path = self.fixture / "app.py"
        original = path.read_bytes()
        code, queue = self.run()
        matching = [
            item for item in queue.get("tasks", [])
            if item.get("file") == "app.py" and item.get("rule_id") == task["rule_id"]
            and item.get("name") == task["name"] and item.get("line") == task["line"]
            and item.get("eligible") is True
        ]
        if code != 0 or queue.get("status") != "review" or len(matching) != 1:
            raise ValueError("native cleanup could not prove the pinned task")
        finding = matching[0]
        self.task_id = finding["id"]
        code, preview = self.run("--task", self.task_id)
        edit = preview.get("edit", {})
        if (
            code != 0 or preview.get("status") != "preview" or not edit.get("proof")
            or not finding.get("source_fingerprint")
            or edit.get("source_fingerprint") != finding["source_fingerprint"]
            or path.read_bytes() != original
        ):
            raise ValueError("native cleanup did not prove a read-only preview")
        self.edit = {"_source": original.decode("utf-8")}
        return self.edit

    def apply(self, edit, root, *, test_cmd=None, rescan=None):
        if Path(root).resolve() != self.fixture or edit is not self.edit:
            raise ValueError("unexpected pinned cleanup edit")
        if (self.fixture / "app.py").read_bytes() != edit["_source"].encode("utf-8"):
            raise ValueError("pinned source changed before native apply")
        python = sys.executable
        expected = {
            "rollback": [python, "-c", "raise SystemExit(3)"],
            "missing": ["skylos-nonexistent-test-binary"],
            "verified": [python, "-m", "pytest", "-q", "test_app.py"],
            "rescan_only": None,
            "opaque": [python, "-c", "pass"],
            "concurrent": ["pytest", "-q"],
            "restored": [python, "-c", "pass"],
            "deletion_limit": [python, "-m", "pytest", "-q", "test_local.py"],
        }[self.action]
        if (shlex.split(test_cmd) if test_cmd is not None else None) != expected:
            raise ValueError("pinned cleanup validation invocation changed")

        # The CLI obtains a fresh proof from the original source and the actual
        # fixture files, including tests the pinned case created after planning.
        # No synthetic scanner report drives candidate verification.
        options = ["--task", self.task_id, "--apply"]
        if self.action in {"concurrent", "restored"}:
            code, report = self.run_with_mutation(options, expected, rescan)
        else:
            if test_cmd is not None:
                options.extend(("--test-cmd", test_cmd))
            code, report = self.run(*options)
        receipt = report.get("execution", {})
        self.proved = self.check_receipt(code, receipt)
        return receipt

    def callback_source(self, callback, name, variables):
        if (
            callback is None or callback.__name__ != name
            or Path(inspect.getfile(callback)).resolve() != self.source
        ):
            raise ValueError("unexpected pinned cleanup mutation callback")
        closure = inspect.getclosurevars(callback).nonlocals
        if (
            set(closure) != variables or closure["tmp_path"].resolve() != self.fixture
            or ("edit" in closure and closure["edit"] is not self.edit)
        ):
            raise ValueError("pinned cleanup mutation callback captured another fixture or edit")
        return dedent(inspect.getsource(callback))

    def run_with_mutation(self, options, original_argv, rescan):
        if self.action == "concurrent":
            from skylos.cleanup_checks import run_cleanup_tests as callback

            prefix = "import pytest\n"
            body = self.callback_source(callback, "concurrent_edit", {"tmp_path"})
            suffix = (
                "\ndef test_concurrent_edit():\n"
                f"    concurrent_edit(tmp_path, {original_argv!r})\n"
                "    pytest.fail('pinned concurrent check failed')\n"
            )
            command = [sys.executable, "-m", "pytest", "-q"]
        else:
            prefix = f"import subprocess\nedit = {self.edit!r}\ntask = {{'_baseline_report': None}}\n"
            body = self.callback_source(rescan, "stale_source", {"tmp_path", "edit", "task"})
            suffix = f"\nsubprocess.run({original_argv!r}, check=True)\nstale_source()\n"
            command = [sys.executable]
        # Keep generated harness files outside the analyzed source set. Run
        # the original callback in the authorized check child. For restoration
        # it runs after the original command, as the last action before rescan.
        with TemporaryDirectory(prefix="skarmory-cleanup-execution-") as workspace:
            script = Path(workspace) / "test_mutation.py"
            script.write_text(
                "from pathlib import Path\ntmp_path = Path.cwd()\n" + prefix + body + suffix,
                encoding="utf-8",
            )
            return self.run(*options, "--test-cmd", shlex.join([*command, str(script)]))

    def check_receipt(self, code, receipt):
        rescan = receipt.get("rescan", {})
        tests = receipt.get("tests", {})
        if self.action == "concurrent":
            return (
                code == 1 and tests.get("evidence") == "pytest_junit"
                and tests.get("status") == "failed" and tests.get("observed") is True
                and rescan.get("status") == "incomplete" and receipt.get("rolled_back") is False
            )
        if self.action == "restored":
            return (
                code == 1 and tests.get("returncode") == 0 and rescan.get("status") == "failed"
                and rescan.get("original_present") is True and receipt.get("rolled_back") is False
            )
        if rescan.get("status") != "passed" or rescan.get("original_present") is not False:
            return False
        if self.action == "rollback":
            return code == 1 and tests.get("returncode") == 3 and receipt.get("rolled_back") is True
        if self.action == "missing":
            return code == 2 and tests.get("status") == "incomplete" and receipt.get("applied") is True
        if self.action in {"verified", "deletion_limit"}:
            return (
                code == 0 and tests.get("status") == "passed" and tests.get("observed") is True
                and tests.get("evidence") == "pytest_junit" and tests.get("count") == 1
            )
        if self.action == "rescan_only":
            return code == 0 and tests.get("status") == "skipped"
        return code == 0 and tests.get("evidence") == "exit_code" and tests.get("observed") is False
