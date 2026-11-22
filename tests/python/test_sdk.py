"""Checks for the installed SDK and its source-only failure mode."""

import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
from tempfile import TemporaryDirectory
import unittest

import skarmory
from skarmory.compat import skylos


FIXTURES = Path(__file__).resolve().parents[1] / "conformance" / "fixtures"
NATIVE_REQUIRED = os.environ.get("SKARMORY_REQUIRE_NATIVE") == "1"
NATIVE_PRESENT = importlib.util.find_spec("skarmory._native") is not None


class SdkTests(unittest.TestCase):
    @unittest.skipIf(NATIVE_PRESENT or NATIVE_REQUIRED, "source-only check")
    def test_source_only_scan_raises(self):
        with self.assertRaises(skarmory.AnalysisUnavailableError):
            skarmory.analyze(FIXTURES / "reachable")
        with self.assertRaises(skarmory.AnalysisUnavailableError):
            skylos.analyze(FIXTURES / "reachable")

    @unittest.skipUnless(NATIVE_PRESENT or NATIVE_REQUIRED, "requires an installed wheel")
    def test_native_reports(self):
        from skarmory import _native

        self.assertEqual(_native.__version__, skarmory.__version__)
        report = skarmory.analyze(FIXTURES / "reachable", phase0=True)
        self.assertEqual(report["profile"], "phase0-python311-flat-application")
        self.assertEqual(report["coverage"]["status"], "complete")
        self.assertEqual(
            [(item["path"], item["symbol"]) for item in report["findings"]],
            [("main.py", "dead_left"), ("main.py", "dead_right"), ("other.py", "work")],
        )
        incomplete = skarmory.analyze(FIXTURES / "invalid")
        self.assertEqual(incomplete["schema_version"], 2)
        self.assertEqual(incomplete["coverage"]["status"], "incomplete")
        self.assertEqual(incomplete["coverage"]["issues"][0]["kind"], "parse_error")
        self.assertEqual(incomplete["findings"], [])

    @unittest.skipUnless(NATIVE_PRESENT or NATIVE_REQUIRED, "requires an installed wheel")
    @unittest.skipUnless(os.name == "posix", "requires POSIX filesystem encoding")
    def test_native_preserves_surrogateescaped_path(self):
        with TemporaryDirectory() as directory:
            path = Path(directory) / os.fsdecode(b"source-\xff")
            if sys.platform == "linux":
                path.mkdir()
                (path / "main.py").write_text("pass\n")
                expected_status = "complete"
                expected_exit = 0
            else:
                expected_status = "incomplete"
                expected_exit = 2

            report = skarmory.analyze(path)
            self.assertEqual(report["coverage"]["status"], expected_status)
            if cli := os.environ.get("SKARMORY_TEST_CLI"):
                result = subprocess.run([cli, path], capture_output=True)
                self.assertEqual(result.returncode, expected_exit)
                self.assertEqual(report, json.loads(result.stdout))

    @unittest.skipUnless(os.environ.get("SKARMORY_TEST_CLI"), "requires an installed CLI")
    def test_sdk_equals_cli(self):
        for fixture in sorted(FIXTURES.iterdir()):
            with self.subTest(fixture=fixture.name):
                result = subprocess.run(
                    [os.environ["SKARMORY_TEST_CLI"], fixture], capture_output=True
                )
                self.assertIn(result.returncode, (0, 2))
                self.assertEqual(skarmory.analyze(fixture), json.loads(result.stdout))

    @unittest.skipUnless(NATIVE_PRESENT or NATIVE_REQUIRED, "requires an installed wheel")
    def test_package_preview_options_and_worker_equivalence(self):
        with TemporaryDirectory() as directory:
            project = Path(directory)
            root = project / "src"
            package = root / "pkg"
            package.mkdir(parents=True)
            (package / "__init__.py").write_text(
                'from .impl import public\n__all__ = ["public"]\n', encoding="utf-8"
            )
            (package / "impl.py").write_text(
                "def helper():\n    pass\n\n"
                "def public():\n    helper()\n\n"
                "def dead():\n    pass\n",
                encoding="utf-8",
            )

            options = {
                "source_roots": [root],
                "policy": "library",
                "public_modules": ["pkg"],
            }
            serial = skarmory.analyze(project, **options)
            self.assertEqual(serial["coverage"]["status"], "complete", serial)
            self.assertEqual([item["symbol"] for item in serial["findings"]], ["dead"])
            self.assertEqual(serial, skarmory.analyze(project, workers=2, **options))
            cache = project / "facts"
            self.assertEqual(serial, skarmory.analyze(project, cache_dir=cache, **options))
            self.assertEqual(
                serial, skarmory.analyze(project, workers=2, cache_dir=cache, **options)
            )
            if cli := os.environ.get("SKARMORY_TEST_CLI"):
                result = subprocess.run(
                    [
                        cli, "--source-root", root, "--policy", "library",
                        "--public-module", "pkg", "--workers", "2", project,
                    ],
                    capture_output=True,
                    check=True,
                )
                self.assertEqual(serial, json.loads(result.stdout))

    @unittest.skipUnless(NATIVE_PRESENT or NATIVE_REQUIRED, "requires an installed wheel")
    def test_phase0_rejects_preview_options(self):
        with self.assertRaisesRegex(ValueError, "Phase 0 does not accept preview"):
            skarmory.analyze(FIXTURES / "reachable", phase0=True, workers=2)

    @unittest.skipUnless(NATIVE_PRESENT or NATIVE_REQUIRED, "requires an installed wheel")
    def test_compatibility_sdk_returns_json_string(self):
        with TemporaryDirectory() as directory:
            project = Path(directory)
            config = project / ".skylos.toml"
            config.write_text("root = true\n", encoding="utf-8")
            (project / "main.py").write_text(
                "def kept():\n    pass\nkept()\n\ndef unused():\n    pass\n",
                encoding="utf-8",
            )
            result = skylos.analyze(project, config_file=config, all_advice=False)
            self.assertIsInstance(result, str)
            report = json.loads(result)
            self.assertTrue(report["unused_functions"], report)
            self.assertEqual(report["analysis_errors"], [])
            if cli := os.environ.get("SKARMORY_TEST_CLI"):
                output = subprocess.run(
                    [
                        cli, project, "--profile", "skylos", "--json", "--no-grep-verify",
                        "--config-file", config, "--include-generated", "--confidence", "60",
                    ],
                    capture_output=True,
                    check=True,
                )
                self.assertEqual(report, json.loads(output.stdout))

            config.write_text("unknown_key = true\n", encoding="utf-8")
            with self.assertRaises(ValueError):
                skylos.analyze(project, config_file=config)

    @unittest.skipUnless(NATIVE_PRESENT or NATIVE_REQUIRED, "requires an installed wheel")
    def test_compatibility_cache_and_worker_equivalence(self):
        with TemporaryDirectory() as directory:
            root = Path(directory)
            project = root / "project"
            project.mkdir()
            cache = root / "facts"
            config = project / ".skylos.toml"
            config.write_text("root = true\n", encoding="utf-8")
            (project / "helper.py").write_text("def target():\n    pass\n", encoding="utf-8")
            entry = project / "main.py"
            options = {"config_file": config, "all_advice": False}

            for source, expected, cache_runs in (
                ("from helper import target\ntarget()\n", [], [(1, 0, 2), (2, 2, 0)]),
                ("pass\n", ["helper.target"], [(2, 1, 1)]),
            ):
                entry.write_text(source, encoding="utf-8")
                clean = json.loads(skylos.analyze(project, **options))
                self.assertEqual(clean["coverage"]["status"], "complete", clean)
                self.assertEqual([row["full_name"] for row in clean["unused_functions"]], expected)
                self.assertEqual(json.loads(skylos.analyze(project, workers=2, **options)), clean)
                for workers, hits, misses in cache_runs:
                    report = json.loads(skylos.analyze(project, workers=workers, cache_dir=cache, **options))
                    stats = report.pop("cache")
                    self.assertEqual((stats["hits"], stats["misses"]), (hits, misses))
                    self.assertEqual(report, clean)

                if cli := os.environ.get("SKARMORY_TEST_CLI"):
                    output = subprocess.check_output([
                        cli, project, "--profile", "skylos", "--json", "--config-file", config,
                        "--include-generated", "--confidence", "60", "--workers", "2", "--cache-dir", cache,
                    ])
                    report = json.loads(output)
                    self.assertEqual(report.pop("cache")["hits"], 2)
                    self.assertEqual(report, clean)

    def test_compatibility_rejects_unsupported_work(self):
        for key in ("grep_verify", "grep_cache", "enable_dependency_hallucinations"):
            with self.subTest(option=key), self.assertRaises(ValueError):
                skylos.analyze(FIXTURES / "reachable", **{key: True})
        with self.assertRaises(TypeError):
            skylos.analyze(FIXTURES / "reachable", enable_quality=True)
