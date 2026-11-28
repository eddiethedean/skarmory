from phase03_framework import framework_definitions, framework_report, unittest_report


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


def test_grouped_methods_expand_for_django_method_assertions():
    method = {
        "type": "function",
        "full_name": "app.Client.schedule",
        "file": "app.py",
        "line": 2,
        "dead_code_classification": "likely_dead",
        "reason": "covered by the unused class finding",
    }
    report = {
        "definitions": {
            "schedule": {"name": "app.Client.schedule", "file": "app.py", "line": 2},
        },
        "analysis_summary": {},
        "dead_code_evidence": {"symbols": []},
        "unused_functions": [],
        "unused_classes": [],
        "unused_variables": [],
        "unused_parameters": [],
        "suppressed": [method, {**method, "full_name": "app.filtered", "reason": "whitelist"}],
    }
    for source in (
        "test_django_function_view_roots.py",
        "test_django_converter_precision.py",
        "test_django_quiet_dead_code.py",
    ):
        projected = framework_report(report, source=f"test/{source}")
        assert projected["unused_functions"] == [method]
        assert projected["dead_code_evidence"]["symbols"][0]["classification"] == "likely_dead"
    assert framework_report(report)["unused_functions"] == report["unused_functions"] == []


def test_admin_stub_projection_uses_the_fixture_root(tmp_path):
    findings = [{"file": "shop/placeholder.py", "rule_id": "SKY-E002"}]
    report = {
        "definitions": {},
        "analysis_summary": {},
        "dead_code_evidence": {"symbols": []},
        "unused_functions": [],
        "unused_classes": [],
        "unused_variables": [],
        "unused_parameters": [],
        "unused_files": findings,
    }
    projected = framework_report(
        report, source="test/test_django_admin_stubs.py", project=tmp_path,
    )
    assert projected["unused_files"] == [
        {"file": str(tmp_path / "shop/placeholder.py"), "rule_id": "SKY-E002"},
    ]
    assert framework_report(report)["unused_files"] == findings
    assert report["unused_files"] == findings


def test_django_settings_and_template_roots_preserve_native_certainty():
    hooks = {
        "app.Backend.get_user": ("django_auth_backend", "django_web_auth_backend"),
        "app.Record.label": ("django_template_method", "django_web_template_member"),
    }
    definitions = {
        name: {"name": name, "file": "app.py", "line": line}
        for line, name in enumerate(hooks, start=1)
    }
    report = {
        "definitions": definitions,
        "analysis_summary": {},
        "dead_code_evidence": {
            "symbols": [
                {
                    "qualified_name": name,
                    "file": "app.py",
                    "line": definition["line"],
                    "evidence": [{"kind": hooks[name][0], "certainty": "possible"}],
                }
                for name, definition in definitions.items()
            ]
        },
        "unused_functions": [],
        "unused_classes": [],
        "unused_variables": [],
        "unused_parameters": [],
    }
    projected = framework_report(report)
    for symbol in projected["dead_code_evidence"]["symbols"]:
        native_kind, reason = hooks[symbol["qualified_name"]]
        assert symbol["classification"] == "uncertain"
        assert symbol["evidence"] == [{
            "kind": "framework_root",
            "native_kind": native_kind,
            "reason": reason,
            "certainty": "possible",
        }]
    assert all(
        definition.heuristic_refs == {}
        for definition in framework_definitions(projected).values()
    )
