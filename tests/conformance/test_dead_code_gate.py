"""Release gate regression tests using fabricated inventories and receipts."""

import hashlib
import json
import subprocess
import sys
import tempfile
import tomllib
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
GATE = ROOT / "scripts/check_dead_code_gate.py"
BASELINE = ROOT / "tests/conformance/baselines/skylos-5.7.0.toml"
REFERENCE = tomllib.loads(BASELINE.read_text(encoding="utf-8"))["phase03_reference"]["git_revision"]
CANDIDATE = "a" * 40
CASE_ID = "test/test_exports.py::test_reexport[pkg]"
NATIVE_CASE = "native:python_engine::resolve::preserve"
ASSERTION = "retain the re-exported implementation"
EARLIER_ASSERTION = "initialize the export provider"
PREREQUISITE = "fixture package created"
FIXTURE = "manual/python_fixture.py"
DEFERRED_ID = "test/test_typescript.py::test_resolve_mixed_references"


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

    def write_inventory(self, complete=True, extra_case="", fixtures="", deferred=""):
        self.inventory.write_text(
            f"""
schema_version = 1
reference_revision = "{REFERENCE}"
inventory_complete = {str(complete).lower()}
{fixtures}

[[variants]]
id = "python-sdk"
platform = "linux-x86_64"
backend = "python"
layer = "sdk"
environment = "reference-lock"
artifact = "candidate"

[[variants]]
id = "python-engine"
platform = "linux-x86_64"
backend = "native"
layer = "engine"
environment = "native-lock"
artifact = "candidate"

[[cases]]
id = "{CASE_ID}"
milestone = "0.3"
languages = ["python"]
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
milestone = "0.3"
languages = ["python"]
source = "engine/resolve_test.rs"
provenance = "pinned test source"
assertions = ["{ASSERTION}"]
prerequisites = ["{PREREQUISITE}"]
omitted_assertions = []
variants = ["python-engine"]
port = "crates/analysis_core/tests/resolve.rs"
{deferred}
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
            "runner_errors": [],
            "reference_command": ["pytest", CASE_ID] if sdk else ["cargo", "test"],
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

    def run_gate(self, *receipts, artifacts=True, fixture_root=None, diagnostic_inventory=None,
                 companion=None):
        command = [
            sys.executable, str(GATE), "--inventory", str(self.inventory),
            "--candidate-revision", CANDIDATE, "--report", str(self.report),
        ]
        if artifacts:
            command.extend(["--artifact", f"candidate={self.artifact}"])
        if companion is not None:
            command.extend(["--artifact", f"cli={companion}"])
        if fixture_root is not None:
            command.extend(["--fixture-root", str(fixture_root)])
        if diagnostic_inventory is not None:
            command.extend(["--diagnostic-inventory", str(diagnostic_inventory)])
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
        run, report = self.run_gate(self.receipt("python-engine"), self.receipt("python-sdk"))
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertEqual(report["status"], "passed")
        self.assertEqual({case["case_id"] for case in report["cases"]}, {CASE_ID, NATIVE_CASE})
        self.assertEqual(report["ledgers"][1]["results"][0]["reference_status"], "xfailed")
        self.assertEqual(report["artifacts"]["candidate"]["sha256"], digest(self.artifact))

    def test_mixed_sdk_parity_requires_the_exact_native_cli_artifact(self):
        self.write_inventory()
        parity = "installed-sdk-native-cli-parity"
        self.inventory.write_text(self.inventory.read_text().replace(
            f'prerequisites = ["{PREREQUISITE}"]',
            f'prerequisites = ["{PREREQUISITE}", "{parity}"]',
            1,
        ).replace('environment = "native-lock"\nartifact = "candidate"',
                  'environment = "native-lock"\nartifact = "cli"'))
        companion = self.folder / "native-cli"
        companion.write_bytes(b"native cli")
        sdk = self.receipt("python-sdk")
        native = self.receipt("python-engine")
        native["artifact_sha256"] = digest(companion)
        result = sdk["results"][0]
        result["prerequisites"][parity] = "passed"
        for incorrect_digest in (None, digest(self.artifact)):
            with self.subTest(incorrect_digest=incorrect_digest):
                result["native_cli_sha256"] = incorrect_digest
                self.assert_blocked(
                    [sdk, native], "native CLI parity artifact", companion=companion,
                )
        result["native_cli_sha256"] = digest(companion)
        run, report = self.run_gate(sdk, native, companion=companion)
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertEqual(report["status"], "passed")

    def test_a_passing_diagnostic_receipt_cannot_qualify_a_release_case(self):
        self.write_inventory()
        sdk = self.receipt("python-sdk")
        sdk["diagnostic_only"] = True
        self.assert_blocked(
            [self.receipt("python-engine"), sdk], "diagnostic receipts cannot qualify release cases",
        )

    def approved_click_receipt(self):
        case = "test/test_fastapi_truth_cycles.py::test_non_fastapi_click_scan_survives_reassigned_truth_alias"
        test = "phase02::analysis::tests::pinned_click_dead_code_control"
        fixture_hash = "b" * 64
        self.write_inventory(deferred=f"""
[[cases]]
id = "{case}"
milestone = "0.3"
languages = ["python"]
source = "test/test_fastapi_truth_cycles.py"
provenance = "original fixture"
assertions = ["{ASSERTION}"]
prerequisites = ["{PREREQUISITE}"]
omitted_assertions = []
variants = ["python-engine"]
port = "tests/conformance/run_phase03_seed.py"
reviewed_exception = {{ reason = "combined scan outside Phase 0.3", approval = "approved in the issue 43 implementation request", tracking_issue = "https://github.com/eddiethedean/skarmory/issues/41", fixture_sha256 = "{fixture_hash}" }}
""")
        receipt = self.receipt("python-engine")
        receipt["results"].append({
            "case_id": case,
            "reference_status": "passed",
            "reference_assertions": [ASSERTION],
            "status": "blocked",
            "assertions": [],
            "prerequisites": {PREREQUISITE: "passed"},
            "native_test": {
                "argv": ["native-test", "--ignored", "--exact", test],
                "fixture_sha256": {"app.py": fixture_hash},
                "stdout": f"test {test} ... ok",
                "returncode": 0,
                "exception_control": {
                    "complete": True,
                    "unused": ["app.unused_neighbor"],
                    "entry_declarations": ["possible"],
                    "entry_called": False,
                },
            },
        })
        return receipt

    def test_approved_exception_preserves_original_outcomes_without_pass_credit(self):
        engine = self.approved_click_receipt()
        run, report = self.run_gate(engine, self.receipt("python-sdk"))
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertEqual(report["counts"], {"passed": 2, "blocked": 0})
        [exception] = report["reviewed_exceptions"]
        self.assertEqual(exception["status"], "accepted")
        self.assertEqual(exception["original_reference_status"], "passed")
        self.assertEqual(exception["original_candidate_status"], "blocked")
        self.assertEqual(exception["original_assertions"], [ASSERTION])

    def test_approved_exception_requires_reference_assertions_and_exact_native_control(self):
        for change in ("missing", "reference", "fixture", "evidence", "original_pass"):
            with self.subTest(change=change):
                engine = self.approved_click_receipt()
                result = engine["results"][-1]
                if change == "missing":
                    engine["results"].pop()
                elif change == "reference":
                    result["reference_assertions"] = []
                elif change == "fixture":
                    result["native_test"]["fixture_sha256"]["app.py"] = "c" * 64
                elif change == "evidence":
                    result["native_test"]["exception_control"]["entry_called"] = True
                elif change == "original_pass":
                    result["status"] = "passed"
                report = self.assert_blocked(
                    [engine, self.receipt("python-sdk")], "approved exception control"
                )
                self.assertEqual(report["reviewed_exceptions"][0]["status"], "blocked")

    def test_mounted_django_exception_requires_the_exact_original_failure_and_sdk_control(self):
        source = "test/test_django_function_view_roots.py"
        case = f"{source}::test_unproved_function_callbacks_do_not_create_provider_roots[async_view-80]"
        original = [f"{source}:{line}" for line in (29, 120, 121, 125)]
        prerequisites = ["installed-sdk", "native-framework-evidence", "grep-verification-disabled"]
        checks = [
            "coverage:complete",
            "evidence:django_registration:certain:views.view",
            "evidence:direct_call:certain:pipeline.dispatch",
            "evidence:direct_call:certain:providers.NcaaClient.schedule_dates",
            "unused:unused_functions:providers.NjcaaClient.schedule_dates",
            "unused:unused_functions:providers.UnrelatedClient.schedule_dates",
        ]
        fixture = "d" * 64
        self.write_inventory(deferred=f"""
[[cases]]
id = "{case}"
milestone = "0.3"
languages = ["python"]
source = "{source}"
provenance = "original mounted async fixture"
assertions = {json.dumps(original)}
prerequisites = {json.dumps(prerequisites)}
omitted_assertions = []
variants = ["python-sdk"]
port = "tests/conformance/run_phase03_seed.py"
runtime_review = {{ reason = "mounted async executes", evidence = "Django runtime", assertions = {json.dumps(checks)} }}
reviewed_exception = {{ reason = "original expects dormant provider", approval = "approved for the two mounted Django async cases", tracking_issue = "https://github.com/eddiethedean/skarmory/issues/41", fixture_sha256 = "{fixture}" }}
""")
        sdk = self.receipt("python-sdk")
        control = {
            "status": "passed",
            "assertions": checks,
            "fixture_sha256": fixture,
            "selected_paths": ["manage.py", "pipeline.py", "providers.py", "settings.py", "urls.py", "views.py"],
            "coverage": {
                "status": "complete", "selected_files": 6, "extracted_files": 6, "issues": [],
            },
        }
        result = {
            "case_id": case,
            "reference_status": "passed",
            "reference_assertions": original,
            "status": "failed",
            "assertions": original[:1],
            "candidate_failed_assertion": original[1],
            "prerequisites": dict.fromkeys(prerequisites, "passed"),
            "reviewed_runtime": control,
        }
        sdk["results"].append(result)
        engine = self.receipt("python-engine")
        run, report = self.run_gate(engine, sdk)
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertEqual(report["counts"], {"passed": 2, "blocked": 0})
        [exception] = report["reviewed_exceptions"]
        self.assertEqual(exception["original_candidate_status"], "failed")
        self.assertEqual(exception["original_candidate_failed_assertion"], original[1])
        self.assertEqual(exception["native_control"], control)
        for changes in (
            {"candidate_failed_assertion": original[2]},
            {"status": "passed"},
            {"reviewed_runtime": {**control, "fixture_sha256": "e" * 64}},
            {"reviewed_runtime": {**control, "status": "failed"}},
        ):
            with self.subTest(changes=changes):
                sdk["results"][-1] = {**result, **changes}
                broken = self.assert_blocked([engine, sdk], "approved exception control")
                self.assertEqual(broken["reviewed_exceptions"][0]["status"], "blocked")

    def reviewed_receipt(self, checks=None):
        if checks is None:
            checks = ["coverage:incomplete", "issue:unresolved_import:app.py:2"]
        self.write_inventory(extra_case=(
            'runtime_review = { reason = "import stops", evidence = "verified exact fixture", '
            f'assertions = {json.dumps(checks)} }}'
        ))
        self.inventory.write_text(
            self.inventory.read_text().replace(
                'port = "tests/python/test_sdk.py"',
                'port = "tests/conformance/run_phase03_seed.py"',
            ),
            encoding="utf-8",
        )
        receipt = self.receipt("python-sdk")
        receipt["results"][0].update(
            status="failed", assertions=[EARLIER_ASSERTION],
            candidate_failed_assertion=ASSERTION,
            reviewed_runtime={
                "status": "passed", "assertions": checks, "selected_paths": ["app.py"],
                "coverage": {
                    "status": "incomplete", "selected_files": 1, "extracted_files": 1,
                    "issues": [{"kind": "unresolved_import", "path": "app.py", "line": 2}],
                },
            },
        )
        return receipt

    def test_reviewed_runtime_previews_but_cannot_override_the_original_failure(self):
        sdk = self.reviewed_receipt()
        engine = self.receipt("python-engine")
        report = self.assert_blocked([engine, sdk], "is not an ordinary pass")
        self.assertEqual(report["counts"], {"passed": 1, "blocked": 1})
        self.assertEqual(report["reviewed_runtime_preview_passes"], 1)
        decision = next(row for row in report["cases"] if row["case_id"] == CASE_ID)
        self.assertEqual(decision["status"], "blocked")
        self.assertEqual(decision["preview_status"], "passed")
        self.assertEqual(decision["original_candidate_status"], "failed")
        self.assertEqual(decision["original_candidate_failed_assertion"], ASSERTION)
        self.assertEqual(decision["runtime_review"]["reason"], "import stops")

        result = sdk["results"][0]
        result["status"] = "passed"
        del result["candidate_failed_assertion"]
        self.assert_blocked([engine, sdk], "executed assertions do not match")
        result["assertions"].append(ASSERTION)
        run, report = self.run_gate(engine, sdk)
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertEqual(report["counts"], {"passed": 2, "blocked": 0})

    def test_reviewed_runtime_still_requires_fixture_coverage_and_original_execution(self):
        sdk = self.reviewed_receipt()
        row = sdk["results"][0]
        runtime = row["reviewed_runtime"]
        engine = self.receipt("python-engine")
        changed = (
            ({"status": "skipped"}, "original candidate case did not execute"),
            ({"candidate_failed_assertion": None}, "does not identify a mapped assertion"),
            ({"reference_status": "error"}, "reference outcome"),
            ({"prerequisites": {PREREQUISITE: "blocked"}}, "required prerequisites"),
            ({"reviewed_runtime": None}, "reviewed runtime result is missing"),
            ({"reviewed_runtime": {**runtime, "assertions": []}}, "assertions do not match"),
            ({"reviewed_runtime": {
                **runtime, "coverage": {**runtime["coverage"], "extracted_files": 0},
            }}, "coverage does not match"),
            ({"reviewed_runtime": {
                **runtime, "coverage": {**runtime["coverage"], "issues": [
                    {"kind": "unresolved_import", "path": "other.py", "line": 2},
                ]},
            }}, "reviewed runtime diagnostic is missing"),
        )
        for update, reason in changed:
            with self.subTest(reason=reason):
                report = self.assert_blocked(
                    [engine, {**sdk, "results": [{**row, **update}]}], reason,
                )
                decision = next(row for row in report["cases"] if row["case_id"] == CASE_ID)
                self.assertEqual(decision["preview_status"], "blocked")

    def test_reviewed_scope_must_extract_the_matching_fixture_sources(self):
        sdk = self.reviewed_receipt([
            "coverage:incomplete", "issue:unresolved_import:app.py:2",
            "scoped-unused:shop:unused_files:placeholder.py",
            "scoped-evidence:shop:framework_declaration:possible:app.handler",
        ])
        engine = self.receipt("python-engine")
        runtime = sdk["results"][0]["reviewed_runtime"]
        runtime["selected_paths"].append("shop/placeholder.py")
        runtime["coverage"].update(selected_files=2, extracted_files=2)
        self.assert_blocked([engine, sdk], "scopes do not match")
        scope = {
            "selected_paths": ["some-other.py"],
            "coverage": {"status": "complete", "selected_files": 1, "extracted_files": 1, "issues": []},
        }
        runtime["scopes"] = {"shop": scope}
        self.assert_blocked([engine, sdk], "did not extract the same fixture sources")
        scope["selected_paths"] = ["placeholder.py"]
        report = self.assert_blocked([engine, sdk], "is not an ordinary pass")
        decision = next(row for row in report["cases"] if row["case_id"] == CASE_ID)
        self.assertEqual(decision["preview_status"], "passed")

    def test_reviewed_incomplete_fixture_can_fail_its_pinned_coverage_check(self):
        sdk = self.reviewed_receipt()
        row = sdk["results"][0]
        row["candidate_failed_assertion"] = "test/web_framework_fixtures.py:16"
        engine = self.receipt("python-engine")
        report = self.assert_blocked([engine, sdk], "is not an ordinary pass")
        decision = next(row for row in report["cases"] if row["case_id"] == CASE_ID)
        self.assertEqual(decision["preview_status"], "passed")

        sdk = self.reviewed_receipt(["coverage:complete", "definition:app.main"])
        row = sdk["results"][0]
        row["candidate_failed_assertion"] = "test/web_framework_fixtures.py:16"
        row["reviewed_runtime"]["coverage"].update(status="complete", issues=[])
        self.assert_blocked(
            [self.receipt("python-engine"), sdk], "does not identify a mapped assertion",
        )

    def test_parity_result_cannot_use_an_unreviewed_runtime_control(self):
        self.write_inventory()
        sdk = self.receipt("python-sdk")
        sdk["results"][0]["reviewed_runtime"] = {}
        self.assert_blocked(
            [self.receipt("python-engine"), sdk], "unexpected reviewed runtime result",
        )

    def test_deferred_fixtures_and_results_do_not_affect_release(self):
        self.write_inventory(
            fixtures='[[fixtures]]\npath = "manual/mixed_repo/missing.ts"',
            deferred=f"""
[[variants]]
id = "typescript-engine"
platform = "linux-x86_64"
backend = "typescript"
layer = "engine"
environment = "future-lock"
artifact = "typescript"

[[cases]]
id = "{DEFERRED_ID}"
milestone = "0.7"
languages = ["python", "typescript"]
source = "test/test_typescript.py"
provenance = "pinned test source"
assertions = ["reference resolves through both frontends"]
prerequisites = ["original TypeScript input"]
omitted_assertions = []
variants = ["typescript-engine", "python-sdk"]
fixtures = ["manual/mixed_repo/missing.ts"]
blocker = "original TypeScript input unavailable"
port = ""
""",
        )
        sdk, engine = self.receipt("python-sdk"), self.receipt("python-engine")
        for status in (None, "failed", "passed"):
            with self.subTest(status=status):
                results = sdk["results"].copy()
                receipts = [engine]
                if status is not None:
                    result = {"case_id": DEFERRED_ID, "status": status}
                    results.append(result)
                    receipts.append({"variant": "typescript-engine", "runner_status": status})
                receipts.append(dict(sdk, results=results))
                run, report = self.run_gate(*receipts)
                self.assertEqual(run.returncode, 0, run.stderr)
                self.assertEqual(report["status"], "passed")
                self.assertEqual(report["fixtures"], {})
                self.assertEqual({case["case_id"] for case in report["cases"]}, {CASE_ID, NATIVE_CASE})
                self.assertEqual(report["deferred_cases"][0]["id"], DEFERRED_ID)
                self.assertEqual(report["deferred_cases"][0]["milestone"], "0.7")

        self.assert_blocked(
            [engine, dict(sdk, results=[{"case_id": DEFERRED_ID, "status": "passed"}])],
            "required result is missing",
        )

    def test_non_python_case_cannot_be_required_for_phase03(self):
        self.write_inventory()
        raw = self.inventory.read_text(encoding="utf-8")
        self.inventory.write_text(raw.replace('languages = ["python"]', 'languages = ["go"]'))
        self.assert_blocked([], "Phase 0.3 only accepts Python cases")

    def test_incomplete_inventory_and_documented_fixture_block_release(self):
        self.write_inventory(complete=False, extra_case='blocker = "fixture unavailable"')
        report = self.assert_blocked(
            [self.receipt("python-engine"), self.receipt("python-sdk")],
            "inventory is not complete",
        )
        self.assertIn("fixture unavailable", report["cases"][0]["reasons"])

    def test_diagnostic_cases_block_publication_with_a_complete_main_inventory(self):
        self.write_inventory()
        diagnostic = self.folder / "diagnostic.toml"
        diagnostic.write_text(
            self.inventory.read_text(encoding="utf-8")
            .replace(CASE_ID, "test/test_exports.py::test_pending_export")
            .replace(NATIVE_CASE, "native:python_engine::resolve::pending"),
            encoding="utf-8",
        )
        receipts = [self.receipt("python-engine"), self.receipt("python-sdk")]
        report = self.assert_blocked(
            receipts, "2 required case variants remain in the diagnostic inventory",
            diagnostic_inventory=diagnostic,
        )
        self.assertTrue(report["inventory_complete"])
        self.assertEqual(report["counts"], {"passed": 2, "blocked": 0})
        self.assertTrue(all(row["preview_status"] == "passed" for row in report["cases"]))

    def test_missing_diagnostic_inventory_blocks_publication(self):
        self.write_inventory()
        self.assert_blocked(
            [self.receipt("python-engine"), self.receipt("python-sdk")],
            "unable to read diagnostic inventory",
            diagnostic_inventory=self.folder / "missing.toml",
        )

    def test_missing_and_unreviewed_fixture_block_even_with_passing_results(self):
        self.write_inventory(
            extra_case=f'fixtures = ["{FIXTURE}"]',
            fixtures=f'[[fixtures]]\npath = "{FIXTURE}"',
        )
        receipts = [self.receipt("python-engine"), self.receipt("python-sdk")]
        self.assert_blocked(receipts, "fixture root was not supplied")
        report = self.assert_blocked(receipts, "required fixture is missing", fixture_root=self.folder)
        self.assertIn(FIXTURE, report["fixtures"])
        self.assertEqual(report["cases"][1]["status"], "passed")
        path = self.folder / FIXTURE
        path.parent.mkdir(parents=True)
        path.write_bytes(b"unverified substitute")
        self.assert_blocked(
            receipts, "fixture provenance, digest, and license are unverified", fixture_root=self.folder,
        )

    def test_reviewed_fixture_bytes_and_receipt_must_both_match(self):
        path = self.folder / FIXTURE
        path.parent.mkdir(parents=True)
        path.write_bytes(b"test data for a reviewed fixture")
        expected_hash = digest(path)
        self.write_inventory(
            extra_case=f'fixtures = ["{FIXTURE}"]',
            fixtures=(
                f'[[fixtures]]\npath = "{FIXTURE}"\nsha256 = "{expected_hash}"\n'
                'provenance = "reviewed source record"\nlicense = "Apache-2.0"'
            ),
        )
        engine, sdk = self.receipt("python-engine"), self.receipt("python-sdk")
        self.assert_blocked([engine], "required result is missing", fixture_root=self.folder)
        self.assert_blocked([engine, sdk], "receipt fixture hashes", fixture_root=self.folder)
        sdk["results"][0]["fixture_sha256"] = {FIXTURE: "0" * 64}
        self.assert_blocked([engine, sdk], "receipt fixture hashes", fixture_root=self.folder)
        sdk["results"][0]["fixture_sha256"] = {FIXTURE: expected_hash}
        run, report = self.run_gate(engine, sdk, fixture_root=self.folder)
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertEqual(report["fixtures"][FIXTURE]["observed_sha256"], expected_hash)
        path.write_bytes(b"replacement")
        self.assert_blocked([engine, sdk], "SHA-256 does not match", fixture_root=self.folder)

    def test_unsafe_and_undeclared_fixture_paths_are_rejected(self):
        for path in ("../outside.ts", "C:/outside.ts"):
            with self.subTest(path=path):
                self.write_inventory(fixtures=f'[[fixtures]]\npath = "{path}"')
                self.assert_blocked([], "fixture path must be a normalized relative path")
        self.write_inventory(extra_case=f'fixtures = ["{FIXTURE}"]')
        self.assert_blocked([], "unknown fixture")

    def test_missing_shard_result_and_duplicate_receipts_block_release(self):
        self.write_inventory()
        sdk = self.receipt("python-sdk")
        engine = self.receipt("python-engine")
        self.assert_blocked([sdk], "python-engine: missing ledger")
        self.assert_blocked([sdk, sdk, engine], "python-sdk: duplicate ledgers")
        self.assert_blocked([sdk, dict(engine, results=[])], "required result is missing")
        results = engine["results"] * 2
        self.assert_blocked([sdk, dict(engine, results=results)], "duplicate case_id")
        results = [*engine["results"], dict(engine["results"][0], case_id="outside")]
        self.assert_blocked([sdk, dict(engine, results=results)], "unexpected case outside")

    def test_ledger_identity_and_runner_errors_block_release(self):
        self.write_inventory()
        sdk = self.receipt("python-sdk")
        engine = self.receipt("python-engine")
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
        del engine["runner_errors"]
        self.assert_blocked([sdk, engine], "runner did not complete")
        engine["runner_errors"] = []
        self.assert_blocked([sdk, '{"variant":"python-engine","variant":"python-engine"}'], "duplicate JSON key")
        self.assert_blocked([sdk, engine], "candidate artifact is missing", artifacts=False)
        self.artifact.write_bytes(b"rebuilt artifact")
        self.assert_blocked([sdk, engine], "artifact_sha256 does not match")

    def test_inventory_must_use_the_pinned_reference(self):
        self.write_inventory()
        raw = self.inventory.read_text(encoding="utf-8")
        self.inventory.write_text(raw.replace(REFERENCE, "d" * 40), encoding="utf-8")
        self.assert_blocked([], "differs from the pinned Phase 0.3 Skylos baseline")

    def test_reference_failure_requires_the_reviewed_marker_and_assertion(self):
        self.write_inventory()
        sdk = self.receipt("python-sdk")
        engine = self.receipt("python-engine")
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
        ledger = dict(sdk, results=[dict(outcome, reference_assertions=[EARLIER_ASSERTION, ASSERTION])])
        self.assert_blocked([ledger, engine], "completed reference assertions")

    def test_reference_pass_requires_completed_assertions(self):
        self.write_inventory()
        sdk = self.receipt("python-sdk")
        engine = self.receipt("python-engine")
        outcome = engine["results"][0]
        for assertions in ([], ["an unrelated assertion"]):
            with self.subTest(assertions=assertions):
                ledger = dict(engine, results=[dict(outcome, reference_assertions=assertions)])
                self.assert_blocked([sdk, ledger], "completed reference assertions")

    def test_skips_errors_and_failed_assertions_block_release(self):
        self.write_inventory()
        engine = self.receipt("python-engine")
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
