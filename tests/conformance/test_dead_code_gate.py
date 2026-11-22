"""Release gate regression tests using fabricated inventories and receipts."""

import hashlib
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]
GATE = ROOT / "scripts/check_dead_code_gate.py"
REFERENCE = "87978e69adf91b3811e97b00c40aa39d7937cd94"
CANDIDATE = "a" * 40
CASE_ID = "test/test_exports.py::test_reexport[pkg]"
NATIVE_CASE = "go:example.org/engine::TestResolve/preserve"
ASSERTION = "retain the re-exported implementation"
EARLIER_ASSERTION = "initialize the export provider"
PREREQUISITE = "fixture package created"


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


class ReleaseGateTests(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.folder = Path(directory.name)
        self.inventory = self.folder / "inventory.toml"
        self.artifact = self.folder / "skarmory"
        self.artifact.write_bytes(b"candidate artifact")
        self.report = self.folder / "report.json"

    def write_inventory(self, complete=True, extra_case=""):
        self.inventory.write_text(
            f"""
schema_version = 1
reference_revision = "{REFERENCE}"
inventory_complete = {str(complete).lower()}

[[variants]]
id = "python-sdk"
platform = "linux-x86_64"
backend = "python"
layer = "sdk"
environment = "reference-lock"
artifact = "candidate"

[[variants]]
id = "go-engine"
platform = "linux-x86_64"
backend = "native"
layer = "engine"
environment = "native-lock"
artifact = "candidate"

[[cases]]
id = "{CASE_ID}"
source = "test/test_exports.py"
provenance = "pinned checkout fixture"
assertions = ["{EARLIER_ASSERTION}", "{ASSERTION}"]
reference_xfail = "{ASSERTION}"
prerequisites = ["{PREREQUISITE}"]
omitted_assertions = ["independent quality report assertion"]
variants = ["python-sdk"]
port = "tests/python/test_sdk.py"
{extra_case}

[[cases]]
id = "{NATIVE_CASE}"
source = "engine/resolve_test.go"
provenance = "pinned test source"
assertions = ["{ASSERTION}"]
prerequisites = ["{PREREQUISITE}"]
omitted_assertions = []
variants = ["go-engine"]
port = "crates/analysis_core/tests/resolve.rs"
""",
            encoding="utf-8",
        )

    def receipt(self, variant):
        sdk = variant == "python-sdk"
        return {
            "schema_version": 1,
            "reference_revision": REFERENCE,
            "candidate_revision": CANDIDATE,
            "inventory_sha256": digest(self.inventory),
            "variant": variant,
            "environment": "reference-lock" if sdk else "native-lock",
            "artifact_sha256": digest(self.artifact),
            "runner_status": "passed",
            "reference_command": ["pytest", CASE_ID] if sdk else ["go", "test"],
            "candidate_command": ["run", "candidate"],
            "results": [{
                "case_id": CASE_ID if sdk else NATIVE_CASE,
                "reference_status": "xfailed" if sdk else "passed",
                "reference_assertions": [EARLIER_ASSERTION] if sdk else [ASSERTION],
                "reference_failed_assertion": ASSERTION if sdk else None,
                "status": "passed",
                "assertions": [EARLIER_ASSERTION, ASSERTION] if sdk else [ASSERTION],
                "prerequisites": {PREREQUISITE: "passed"},
            }],
        }

    def run_gate(self, *receipts, artifacts=True):
        command = [
            sys.executable, str(GATE), "--inventory", str(self.inventory),
            "--candidate-revision", CANDIDATE, "--report", str(self.report),
        ]
        if artifacts:
            command.extend(["--artifact", f"candidate={self.artifact}"])
        for index, receipt in enumerate(receipts):
            path = self.folder / f"ledger-{index}.json"
            serialized = receipt if isinstance(receipt, str) else json.dumps(receipt)
            path.write_text(serialized, encoding="utf-8")
            command.extend(["--ledger", str(path)])
        run = subprocess.run(command, capture_output=True, text=True, check=False)
        return run, json.loads(self.report.read_text(encoding="utf-8"))

    def assert_blocked(self, receipts, reason, **options):
        run, report = self.run_gate(*receipts, **options)
        self.assertEqual(run.returncode, 1, run.stderr)
        self.assertEqual(report["status"], "blocked")
        messages = report["errors"] + [
            note for case in report["cases"] for note in case["reasons"]
        ]
        self.assertTrue(any(reason in message for message in messages), messages)
        return report

    def test_complete_join_passes_and_preserves_the_upstream_xfail(self):
        self.write_inventory()
        run, report = self.run_gate(self.receipt("go-engine"), self.receipt("python-sdk"))
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertEqual(report["status"], "passed")
        self.assertEqual({case["case_id"] for case in report["cases"]}, {CASE_ID, NATIVE_CASE})
        self.assertEqual(report["ledgers"][1]["results"][0]["reference_status"], "xfailed")
        self.assertEqual(report["artifacts"]["candidate"]["sha256"], digest(self.artifact))

    def test_incomplete_inventory_and_documented_fixture_block_release(self):
        self.write_inventory(complete=False, extra_case='blocker = "fixture unavailable"')
        report = self.assert_blocked(
            [self.receipt("go-engine"), self.receipt("python-sdk")],
            "inventory is not complete",
        )
        self.assertIn("fixture unavailable", report["cases"][0]["reasons"])

    def test_missing_shard_result_and_duplicate_receipts_block_release(self):
        self.write_inventory()
        sdk = self.receipt("python-sdk")
        engine = self.receipt("go-engine")
        self.assert_blocked([sdk], "go-engine: missing ledger")
        self.assert_blocked([sdk, sdk, engine], "python-sdk: duplicate ledgers")
        self.assert_blocked([sdk, dict(engine, results=[])], "required result is missing")
        results = engine["results"] * 2
        self.assert_blocked([sdk, dict(engine, results=results)], "duplicate case_id")
        results = [*engine["results"], dict(engine["results"][0], case_id="outside")]
        self.assert_blocked([sdk, dict(engine, results=results)], "unexpected case outside")

    def test_ledger_identity_and_runner_errors_block_release(self):
        self.write_inventory()
        sdk = self.receipt("python-sdk")
        engine = self.receipt("go-engine")
        for field, wrong in (
            ("candidate_revision", "b" * 40),
            ("reference_revision", "c" * 40),
            ("inventory_sha256", "2" * 64),
            ("artifact_sha256", "3" * 64),
            ("environment", "other-lock"),
        ):
            with self.subTest(field=field):
                report = self.assert_blocked(
                    [sdk, dict(engine, **{field: wrong})], f"{field} does not match"
                )
                self.assertEqual(report["cases"][1]["status"], "blocked")
        self.assert_blocked([sdk, dict(engine, runner_status="error")], "runner did not complete")
        self.assert_blocked([sdk, dict(engine, runner_errors=["collection error"])], "runner did not complete")
        self.assert_blocked([sdk, '{"variant":"go-engine","variant":"go-engine"}'], "duplicate JSON key")
        self.assert_blocked([sdk, engine], "candidate artifact is missing", artifacts=False)
        self.artifact.write_bytes(b"rebuilt artifact")
        self.assert_blocked([sdk, engine], "artifact_sha256 does not match")

    def test_inventory_must_use_the_pinned_reference(self):
        self.write_inventory()
        raw = self.inventory.read_text(encoding="utf-8")
        self.inventory.write_text(raw.replace(REFERENCE, "d" * 40), encoding="utf-8")
        self.assert_blocked([], "differs from the pinned Skylos baseline")

    def test_reference_failure_requires_the_reviewed_marker_and_assertion(self):
        self.write_inventory()
        sdk = self.receipt("python-sdk")
        engine = self.receipt("go-engine")
        outcome = sdk["results"][0]
        for status in ("failed", "xpassed", "passed"):
            with self.subTest(status=status):
                ledger = dict(sdk, results=[dict(outcome, reference_status=status)])
                self.assert_blocked([ledger, engine], "expected 'xfailed'")
        for assertion in (None, "failure in unrelated fixture setup"):
            with self.subTest(assertion=assertion):
                ledger = dict(sdk, results=[dict(outcome, reference_failed_assertion=assertion)])
                self.assert_blocked([ledger, engine], "expected failing reference assertion")
        ledger = dict(sdk, results=[dict(outcome, reference_assertions=[])])
        self.assert_blocked([ledger, engine], "completed reference assertions")

    def test_reference_pass_requires_completed_assertions(self):
        self.write_inventory()
        sdk = self.receipt("python-sdk")
        engine = self.receipt("go-engine")
        outcome = engine["results"][0]
        for assertions in ([], ["an unrelated assertion"]):
            with self.subTest(assertions=assertions):
                ledger = dict(engine, results=[dict(outcome, reference_assertions=assertions)])
                self.assert_blocked([sdk, ledger], "completed reference assertions")

    def test_skips_errors_and_failed_assertions_block_release(self):
        self.write_inventory()
        engine = self.receipt("go-engine")
        sdk = self.receipt("python-sdk")
        observed = sdk["results"][0]
        for status in ("failed", "skipped", "blocked", "error", "xfailed", "xpassed"):
            with self.subTest(status=status):
                ledger = dict(sdk, results=[dict(observed, status=status)])
                self.assert_blocked([ledger, engine], f"outcome '{status}'")
        ledger = dict(sdk, results=[dict(observed, reference_status="skipped")])
        self.assert_blocked([ledger, engine], "lacks executed evidence")
        ledger = dict(sdk, results=[dict(observed, assertions=[])])
        self.assert_blocked([ledger, engine], "executed assertions")
        for prerequisites in ({}, {PREREQUISITE: "blocked"}):
            ledger = dict(sdk, results=[dict(observed, prerequisites=prerequisites)])
            self.assert_blocked([ledger, engine], "prerequisites were not all verified")


if __name__ == "__main__":
    unittest.main()
