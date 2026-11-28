import unittest
from pathlib import Path
from tempfile import TemporaryDirectory
from types import MethodType

from phase03_config import FUNCTION, PROJECT_CONFIG, PYTHON_SOURCE, candidate_options, candidate_test


class MalformedConfigPortTests(unittest.TestCase):
    def test_candidate_runs_original_setup_and_dead_code_assertion(self):
        with TemporaryDirectory() as temp:
            path = Path(temp) / "test_analyzer.py"
            path.write_text(
                "\n" * 2051
                + "class TestAnalyze:\n"
                + f"    def {FUNCTION}(self):\n"
                + "        result = self.scan()\n"
                + '        assert result["unused_functions"]\n'
                + '        assert "SKY-D201" in {f.get("rule_id") for f in result.get("danger", [])}\n',
                encoding="utf-8",
            )
            namespace = {}
            exec(compile(path.read_bytes(), str(path), "exec"), namespace)
            calls = []
            report = {"unused_functions": ["unused_func"]}

            def scan(self):
                calls.append(self)
                return report

            tester = type("TestAnalyze", (), {"scan": scan})()
            original = MethodType(vars(namespace["TestAnalyze"])[FUNCTION], tester)
            candidate = candidate_test(original)
            with self.assertRaises(AssertionError):
                original()
            candidate()
            report["unused_functions"] = []
            with self.assertRaises(AssertionError):
                candidate()
            self.assertEqual(calls, [tester, tester, tester])

    def test_candidate_requires_exact_original_inputs_and_scan_options(self):
        with TemporaryDirectory() as temp:
            root = Path(temp)
            (root / "pyproject.toml").write_bytes(PROJECT_CONFIG)
            (root / "app.py").write_bytes(PYTHON_SOURCE)
            original = {"conf": 0, "grep_verify": False, "enable_danger": True}
            self.assertEqual(candidate_options(root, original), {"conf": 0, "grep_verify": False})
            with self.assertRaisesRegex(ValueError, "scan options changed"):
                candidate_options(root, {**original, "grep_verify": True})
            (root / "pyproject.toml").write_bytes(PROJECT_CONFIG + b"\n")
            with self.assertRaisesRegex(ValueError, "original project files"):
                candidate_options(root, original)
