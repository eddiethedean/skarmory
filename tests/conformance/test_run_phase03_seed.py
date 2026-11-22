import sys
import unittest
from pathlib import Path

from run_phase03_seed import AssertionTrace


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


if __name__ == "__main__":
    unittest.main()
