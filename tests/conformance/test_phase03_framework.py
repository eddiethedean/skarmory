from phase03_framework import framework_report


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
