"""Verify execution evidence for the pinned source provider fixtures."""

import json
from pathlib import Path
from tempfile import TemporaryDirectory


SOURCE = "test/test_provider_method_precision.py"
PREREQUISITE = "installed-sdk-source-factory-method-evidence"
TARGET = "provider.Provider.inspect_payload"
SELECTED = ("app.py", "provider.py")
FACTORY_METHOD = "source_factory_method"

FACTORY_CASES = {
    f"{SOURCE}::test_expression_bodies_keep_eager_provider_calls_without_inventing_dead_arms"
    "[make_provider().inspect_payload() if False else None-False]": False,
    **{
        f"{SOURCE}::test_completed_or_short_circuited_sites_do_not_prove_provider_calls[{body}-{runs}]": runs
        for body, runs in (
            (r"if True:\n    return\nmake_provider().inspect_payload()", False),
            (r"if False:\n    return\nmake_provider().inspect_payload()", True),
            ("False and make_provider().inspect_payload()", False),
            ("True or make_provider().inspect_payload()", False),
            ("True and make_provider().inspect_payload()", True),
            ("False or make_provider().inspect_payload()", True),
            (r"stop()\nmake_provider().inspect_payload()", False),
            (r"finish()\nmake_provider().inspect_payload()", True),
        )
    },
}
DEFERRED_CASES = {
    f"{SOURCE}::test_creating_a_deferred_consumer_does_not_prove_method_execution[{kind}]"
    for kind in ("coroutine", "generator")
}
METHOD_CASES = FACTORY_CASES.keys() | DEFERRED_CASES


def verify_factory_method(case_id, path, options, report, scan):
    factory = case_id in FACTORY_CASES
    if case_id not in METHOD_CASES:
        return False
    if options != {"conf": 0, "grep_verify": False, "grep_cache": False}:
        return False
    executes = FACTORY_CASES.get(case_id, False)
    if not _matches(report, factory=factory, executes=executes):
        return False

    # Keep the original provider source. The independent controls differ only
    # in whether they invoke the wrapper that performs the method lookup.
    provider = (Path(path) / "provider.py").read_bytes()
    receiver = "make_provider" if factory else "Provider"
    wrapper = (
        f"from provider import {receiver}\n"
        f"def consume(): return {receiver}().inspect_payload()\n"
    )
    with TemporaryDirectory(prefix="skarmory-factory-method-control-") as directory:
        project = Path(directory)
        (project / "provider.py").write_bytes(provider)
        for runs in (False, True):
            (project / "app.py").write_text(
                wrapper + ("RESULT = consume()\n" if runs else ""), encoding="utf-8"
            )
            control = json.loads(scan(project, options))
            if not _matches(control, factory=factory, executes=runs):
                return False
    return True


def _matches(report, *, factory, executes):
    if (
        report["coverage"]["status"] != "complete"
        or tuple(report["selected_paths"]) != SELECTED
        or report["analysis_summary"]["grep_verify"]["enabled"]
    ):
        return False
    events = [
        event
        for row in report["dead_code_evidence"]["symbols"]
        if row["qualified_name"] == TARGET
        for event in row["evidence"]
    ]
    called = any(event["kind"] == "direct_call" for event in events)
    proved_factory = any(event["kind"] == FACTORY_METHOD for event in events)
    rescued = any(
        row["name"] == TARGET and row["reason"] == FACTORY_METHOD
        for row in report["analysis_summary"]["dead_code_liveness"]["rescued"]
    )
    return called == executes and proved_factory == rescued == (factory and executes)
