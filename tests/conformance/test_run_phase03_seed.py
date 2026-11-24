import json
import platform
import sys
import unittest
from pathlib import Path
from unittest.mock import patch

from run_phase03_seed import (
    AssertionTrace,
    check_candidate_wheel,
    environment,
    scan_candidate,
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


if __name__ == "__main__":
    unittest.main()
