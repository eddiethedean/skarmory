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
    f"{SOURCE}::test_expression_bodies_keep_eager_provider_calls_without_inventing_dead_arms"
    "[lambda value=make_provider().inspect_payload(): value-True]": True,
    **{
        f"{SOURCE}::test_completed_or_short_circuited_sites_do_not_prove_provider_calls[{body}-{runs}]": runs
        for body, runs in (
            (r"if True:\n    return\nmake_provider().inspect_payload()", False),
            (r"if False:\n    return\nmake_provider().inspect_payload()", True),
            (r"for item in [1]:\n    break\n    make_provider().inspect_payload()", False),
            (r"for item in [1]:\n    continue\n    make_provider().inspect_payload()", False),
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
PROXY_CASES = {
    f"{SOURCE}::test_legacy_proxy_call_requires_an_executing_owner[{verify}-{confidence}]": (
        confidence, verify,
    )
    for confidence in (0, 80)
    for verify in (False, True)
}
PROXY_OWNER = "commands.retired_shell"
PROXY_TARGET = "app.App.make_shell_context"
PROXY_PATHS = ("app.py", "commands.py")


def verify_dormant_proxy(case_id, path, options, report, scan):
    confidence, verify = PROXY_CASES[case_id]
    if options != {"conf": confidence, "grep_verify": verify, "grep_cache": False}:
        return False
    if not _proxy_report(report, verify, coverage="complete"):
        return False
    if not _proxy_unused(report, PROXY_OWNER) or _proxy_called(report):
        return False

    # Preserve the original files. Activate the unresolved receiver in one
    # control and call the actual provider independently in the other.
    original = Path(path)
    app = (original / "app.py").read_bytes()
    commands = (original / "commands.py").read_bytes()
    with TemporaryDirectory(prefix="skarmory-dormant-proxy-control-") as directory:
        project = Path(directory)
        (project / "app.py").write_bytes(app)
        (project / "commands.py").write_bytes(commands + b"\nretired_shell()\n")
        activated = json.loads(scan(project, options))
        if not _proxy_report(activated, verify, coverage="incomplete") or _proxy_called(activated):
            return False
        if not any(
            item["path"] == "commands.py" and item["kind"] == "unresolved_call"
            for item in activated["coverage"]["issues"]
        ):
            return False
        (project / "commands.py").write_bytes(
            commands + b"\nfrom app import app as selected_app\nselected_app.make_shell_context()\n"
        )
        selected = json.loads(scan(project, options))
        return (
            _proxy_report(selected, verify, coverage="complete")
            and _proxy_unused(selected, PROXY_OWNER)
            and not _proxy_unused(selected, PROXY_TARGET)
            and _proxy_called(selected)
        )


def _proxy_report(report, verify, *, coverage):
    return (
        report["coverage"]["status"] == coverage
        and tuple(report["selected_paths"]) == PROXY_PATHS
        and report["analysis_summary"]["grep_verify"]["enabled"] is verify
    )


def _proxy_unused(report, target):
    return any(item["full_name"] == target for item in report["unused_functions"])


def _proxy_called(report):
    return any(
        item["qualified_name"] == PROXY_TARGET
        and any(edge["kind"] == "direct_call" for edge in item["evidence"])
        for item in report["dead_code_evidence"]["symbols"]
    )


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
