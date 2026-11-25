from phase03_framework import framework_definitions, framework_report


def test_only_matching_native_evidence_proves_framework_activation():
    definitions = {
        name: {"name": name, "file": "app.py", "line": line}
        for line, name in enumerate(("app.live", "app.possible", "app.dead"), start=1)
    }

    def evidence(name, line, kind, certainty):
        return {
            "qualified_name": name,
            "file": "app.py",
            "line": line,
            "evidence": [{"kind": kind, "certainty": certainty}],
        }

    report = {
        "definitions": definitions,
        "analysis_summary": {},
        "dead_code_evidence": {
            "symbols": [
                evidence("app.live", 1, "django_signal", "certain"),
                evidence(
                    "app.possible", 2, "django_signal_unknown_loading", "possible"
                ),
                evidence("app.dead", 4, "flask_registration", "certain"),
            ]
        },
        "unused_functions": [{"full_name": "app.dead", "file": "app.py", "line": 3}],
        "unused_classes": [],
        "unused_variables": [],
        "unused_parameters": [],
    }
    projected = framework_report(report)
    assert projected["unused_functions"] == report["unused_functions"]
    assert projected["analysis_summary"]["dead_code_liveness"]["rescued"] == [
        {"name": "app.live", "reason": "django_web_signal_receiver"},
        {
            "name": "app.possible",
            "reason": "django_signal_registration_unknown_loading",
        },
    ]
    live, possible, dead = projected["dead_code_evidence"]["symbols"]
    assert live["classification"] == "alive"
    assert live["evidence"][0]["kind"] == "framework_root"
    assert possible["classification"] == "uncertain"
    assert possible["evidence"][0]["certainty"] == "possible"
    assert dead["classification"] == "likely_dead"
    assert dead["evidence"] == []

    definitions = framework_definitions(projected)
    assert definitions["app.live"].name == "app.live"
    assert definitions["app.live"].heuristic_refs == {"framework_root": 1.0}
    assert definitions["app.possible"].heuristic_refs == {}
    assert definitions["app.dead"].heuristic_refs == {}


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
