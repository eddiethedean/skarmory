import json
import subprocess
import unittest
from pathlib import Path
from tempfile import TemporaryDirectory

from phase03_engine import EXCEPTION_CONTROLS, TEST, engine_case, locate_test_artifact, passed_exact_test


class EngineReceiptTests(unittest.TestCase):
    def test_exception_replay_requires_the_original_assertions_and_setup(self):
        for case_id, (source, _, _, assertions, prerequisites) in EXCEPTION_CONTROLS.items():
            case = {"source": source, "assertions": assertions, "prerequisites": prerequisites}
            for field in ("assertions", "prerequisites"):
                with self.subTest(case_id=case_id, field=field):
                    reduced = {**case, field: case[field][:-1]}
                    with self.assertRaisesRegex(ValueError, "reviewed assertion and fixture mapping"):
                        engine_case(None, case_id, reduced, {})

    def test_requires_exactly_the_named_passing_test(self):
        success = (
            f"\nrunning 1 test\ntest {TEST} ... ok\n\n"
            "test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; "
            "108 filtered out; finished in 0.63s\n\n"
        )
        self.assertTrue(passed_exact_test(subprocess.CompletedProcess([], 0, success, "")))
        for unproved in (
            "running 0 tests\n\ntest result: ok. 0 passed; 0 failed; 0 ignored; "
            "0 measured; 109 filtered out; finished in 0.00s",
            success.replace(TEST, "different::test"),
            success.replace("... ok", "... ignored"),
            success.replace("running 1 test", "running 2 tests"),
            success.replace("1 passed", "0 passed"),
            success.removesuffix("0.63s\n\n"),
        ):
            with self.subTest(unproved=unproved):
                self.assertFalse(passed_exact_test(subprocess.CompletedProcess([], 0, unproved, "")))
        self.assertFalse(passed_exact_test(subprocess.CompletedProcess([], 1, success, "")))

    def test_only_accepts_the_cargo_library_test_artifact(self):
        with TemporaryDirectory() as workspace:
            root = Path(workspace).resolve()
            source = root / "src" / "lib.rs"
            artifact = root / "skarmory-core-test"
            artifact.touch()
            test = {
                "reason": "compiler-artifact", "profile": {"test": True},
                "target": {"name": "skarmory_core", "kind": ["lib"], "src_path": str(source)},
                "executable": str(artifact),
            }
            normal = {**test, "profile": {"test": False}}
            self.assertEqual(locate_test_artifact(map(json.dumps, (normal, test)), source), artifact)
            for rows in ((normal,), (test, test)):
                with self.subTest(rows=rows), self.assertRaisesRegex(ValueError, "exactly one"):
                    locate_test_artifact(map(json.dumps, rows), source)


if __name__ == "__main__":
    unittest.main()
