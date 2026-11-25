import unittest

from phase03_framework import unittest_report


class UnittestReportTests(unittest.TestCase):
    def test_only_certain_evidence_for_the_actual_definition_marks_the_case(self):
        definitions = {
            f"tests.py:{line}:1:tests.{name}": {
                "name": f"tests.{name}", "file": "tests.py", "line": line,
            }
            for line, name in enumerate(["Live", "Possible", "Lookalike", "Stale"], start=1)
        }
        evidence = [
            {
                "qualified_name": f"tests.{name}", "file": "tests.py", "line": line,
                "evidence": [{"kind": kind, "certainty": certainty}],
            }
            for line, name, kind, certainty in [
                (1, "Live", "unittest_discovery", "certain"),
                (2, "Possible", "unittest_discovery", "possible"),
                (3, "Lookalike", "pytest_hook", "certain"),
                (99, "Stale", "unittest_discovery", "certain"),
            ]
        ]
        findings = [{"full_name": "tests.Stale"}]
        original = {
            "definitions": definitions,
            "dead_code_evidence": {"symbols": evidence},
            "unused_classes": findings,
        }

        projected = unittest_report(original)

        self.assertEqual({"tests.Live"}, {
            name for name, definition in projected["definitions"].items()
            if definition.get("dead_code_evidence") == [{"reason": "unittest_discovery"}]
        })
        self.assertEqual(projected["unused_classes"], findings)
        self.assertEqual(original["definitions"], definitions)
