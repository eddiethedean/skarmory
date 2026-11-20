"""Checks for the installed SDK and its source-only failure mode."""

import importlib.util
import json
import os
from pathlib import Path
import subprocess
import unittest

import skarmory


FIXTURES = Path(__file__).resolve().parents[1] / "conformance" / "fixtures"
NATIVE_REQUIRED = os.environ.get("SKARMORY_REQUIRE_NATIVE") == "1"
NATIVE_PRESENT = importlib.util.find_spec("skarmory._native") is not None


class SdkTests(unittest.TestCase):
    @unittest.skipIf(NATIVE_PRESENT or NATIVE_REQUIRED, "source-only check")
    def test_source_only_scan_raises(self):
        with self.assertRaises(skarmory.AnalysisUnavailableError):
            skarmory.analyze(FIXTURES / "reachable")

    @unittest.skipUnless(NATIVE_PRESENT or NATIVE_REQUIRED, "requires an installed wheel")
    def test_native_reports(self):
        from skarmory import _native

        self.assertEqual(_native.__version__, skarmory.__version__)
        report = skarmory.analyze(FIXTURES / "reachable")
        self.assertEqual(report["coverage"]["status"], "complete")
        self.assertEqual(
            [(item["path"], item["symbol"]) for item in report["findings"]],
            [("main.py", "dead_left"), ("main.py", "dead_right"), ("other.py", "work")],
        )
        incomplete = skarmory.analyze(FIXTURES / "invalid")
        self.assertEqual(incomplete["coverage"]["status"], "incomplete")
        self.assertEqual(incomplete["coverage"]["issues"][0]["kind"], "parse_error")
        self.assertEqual(incomplete["findings"], [])

    @unittest.skipUnless(os.environ.get("SKARMORY_TEST_CLI"), "requires an installed CLI")
    def test_sdk_equals_cli(self):
        for fixture in sorted(FIXTURES.iterdir()):
            with self.subTest(fixture=fixture.name):
                result = subprocess.run(
                    [os.environ["SKARMORY_TEST_CLI"], fixture], capture_output=True
                )
                self.assertIn(result.returncode, (0, 2))
                self.assertEqual(skarmory.analyze(fixture), json.loads(result.stdout))
