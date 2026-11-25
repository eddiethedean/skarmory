import hashlib
import json
import platform
import sys
import unittest
from pathlib import Path
from tempfile import TemporaryDirectory
from types import SimpleNamespace
from unittest.mock import patch

from run_phase03_seed import (
    CORPUS_SOURCE,
    AssertionTrace,
    benchmark_assertions,
    check_candidate_wheel,
    environment,
    scan_candidate,
    write_receipt,
)


def original_assertions(stop, value):
    assert isinstance(value, int)
    if stop:
        return
    assert value > 0


class AssertionTraceTests(unittest.TestCase):
    def test_unreached_and_failed_assertions_earn_no_credit(self):
        start = original_assertions.__code__.co_firstlineno
        first, last = start + 1, start + 4
        for stop, value, expected in [
            (True, 1, {first}),
            (False, -1, {first}),
            (False, 1, {first, last}),
        ]:
            with self.subTest(stop=stop, value=value):
                trace = AssertionTrace(Path(__file__))
                previous = sys.gettrace()
                try:
                    sys.settrace(trace)
                    if value < 0:
                        with self.assertRaises(AssertionError):
                            original_assertions(stop, value)
                    else:
                        original_assertions(stop, value)
                finally:
                    sys.settrace(previous)
                self.assertEqual(trace.completed & {first, last}, expected)


class BenchmarkEvidenceTests(unittest.TestCase):
    def test_rejects_missing_scoring_expectations_and_overruns(self):
        labels = ["coverage", "unused", "live", "budget"]
        case = {
            "id": "basic-unused-symbols",
            "scored": True,
            "analysis_status": "complete",
            "true_positives": 5,
            "false_negatives": 0,
            "true_negatives": 3,
            "false_positives": 0,
            "elapsed_seconds": 0.4,
        }
        for changes, expected in (
            ({}, labels),
            ({"analysis_status": "incomplete"}, []),
            ({"scored": False}, []),
            ({"true_positives": 4, "false_negatives": 1}, ["coverage", "live", "budget"]),
            ({"true_negatives": 2, "false_positives": 1}, ["coverage", "unused", "budget"]),
            ({"elapsed_seconds": 5.1}, ["coverage", "unused", "live"]),
        ):
            with self.subTest(changes=changes):
                _, reached = benchmark_assertions(
                    {"cases": [{**case, **changes}], "skipped_cases": []}, labels,
                )
                self.assertEqual(reached, expected)
        with self.assertRaisesRegex(ValueError, "did not run exactly once"):
            benchmark_assertions({"cases": [], "skipped_cases": [case]}, labels)


class CandidateWheelTests(unittest.TestCase):
    def test_rejects_candidate_with_wrong_interpreter_or_architecture(self):
        actual_platform = (
            f"{sys.platform}-{platform.machine()}-cp{sys.version_info.major}{sys.version_info.minor}"
        )
        wrong_python = f"{sys.platform}-{platform.machine()}-cp999"
        wrong_architecture = actual_platform.replace(platform.machine(), "unsupported-arch")
        for declared in (wrong_python, wrong_architecture):
            with (
                self.subTest(declared=declared),
                self.assertRaisesRegex(ValueError, "Required candidate SDK platform"),
            ):
                check_candidate_wheel(
                    Path(sys.executable), Path("unused.whl"), declared, Path.cwd(), environment(),
                )


class CandidateScanTests(unittest.TestCase):
    def test_selects_phase03_and_preserves_runtime_and_grep_options(self):
        for options, expected in (
            ({}, {}),
            (
                {"grep_verify": False, "trace_file": False},
                {
                    "grep_verify": False,
                    "trace_file": False,
                },
            ),
            (
                {"grep_verify": True, "trace_file": Path("trace.json")},
                {
                    "grep_verify": True,
                    "trace_file": "trace.json",
                },
            ),
        ):
            with (
                self.subTest(options=options),
                patch("run_phase03_seed.subprocess.run") as run,
            ):
                scan_candidate(Path("sdk/bin/python"), "fixture", options)
                payload = json.loads(run.call_args.kwargs["input"])
                self.assertEqual(
                    payload, ["fixture", {"profile": "skylos-0.3", **expected}]
                )


class CorpusReceiptTests(unittest.TestCase):
    def test_accepts_only_matching_worker_fixture_hashes(self):
        case_id = f"{CORPUS_SOURCE}::attrs-define-fields"
        fixture = "corpus/fixtures/attrs_define.py"
        lock = b"pinned dependencies\n"
        lock_digest = hashlib.sha256(lock).hexdigest()
        revision = "a" * 40
        variant = {
            "id": "diagnostic-sdk",
            "layer": "sdk",
            "platform": f"{sys.platform}-{platform.machine()}-cp{sys.version_info.major}{sys.version_info.minor}",
            "environment": lock_digest,
        }
        assertions = ["absent:unused_variables:id"]
        case = {"source": CORPUS_SOURCE, "assertions": assertions, "prerequisites": []}
        expected = {fixture: "b" * 64}
        for candidate in (expected, {fixture: "c" * 64}, None):
            with self.subTest(candidate=candidate), TemporaryDirectory() as temporary:
                root = Path(temporary)
                (root / "uv.lock").write_bytes(lock)
                artifact = root / "candidate.whl"
                artifact.write_bytes(b"candidate wheel")
                args = SimpleNamespace(
                    artifact=artifact, candidate_revision=revision, inventory=root / "inventory.toml",
                    output=root / "receipt.json", sdk_python=Path(sys.executable), upstream=root,
                    variant=variant["id"],
                )

                def run_worker(command, **_kwargs):
                    result = {"status": "passed", "assertions": assertions, "prerequisites": {}}
                    fingerprints = expected if command[-3] == "reference" else candidate
                    if fingerprints is not None:
                        result["fixture_sha256"] = fingerprints
                    Path(command[-1]).write_text(json.dumps({case_id: result}), encoding="utf-8")
                    return SimpleNamespace(returncode=0)

                with (
                    patch("run_phase03_seed.port_cases", return_value=(
                        {"reference_revision": revision}, "digest", variant, {case_id: case},
                    )),
                    patch("run_phase03_seed.subprocess.check_output", side_effect=[revision, ""]),
                    patch("run_phase03_seed.check_candidate_wheel"),
                    patch("run_phase03_seed.subprocess.run", side_effect=run_worker),
                ):
                    write_receipt(args)

                receipt = json.loads(args.output.read_text(encoding="utf-8"))
                result = receipt["results"][0]
                if candidate == expected:
                    self.assertEqual(receipt["runner_status"], "passed")
                    self.assertEqual(result["fixture_sha256"], expected)
                else:
                    self.assertEqual(receipt["runner_status"], "error")
                    self.assertNotIn("fixture_sha256", result)


if __name__ == "__main__":
    unittest.main()
