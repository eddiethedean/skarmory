"""Smoke tests for the public failure contract and installed native boundary."""

import importlib
import os
from pathlib import Path
import unittest

import skarmory


class ScaffoldTests(unittest.TestCase):
    def test_scan_does_not_report_a_clean_project(self):
        with self.assertRaises(skarmory.AnalysisUnavailableError):
            skarmory.analyze(Path("."))

    def test_native_wheel_is_present_when_required(self):
        if os.environ.get("SKARMORY_REQUIRE_NATIVE") != "1":
            self.skipTest("set SKARMORY_REQUIRE_NATIVE=1 after installing the wheel")

        native = importlib.import_module("skarmory._native")
        self.assertEqual(native.__version__, skarmory.__version__)
        with self.assertRaises(NotImplementedError):
            native.analyze(".")


if __name__ == "__main__":
    unittest.main()
