"""Check reviewed runtime expectations separately from pinned Skylos parity."""

from dataclasses import dataclass


@dataclass(frozen=True)
class Control:
    unused: tuple[tuple[str, str], ...] = ()
    absent_evidence: tuple[tuple[str, str], ...] = ()
    diagnostic: tuple[str, int, str] | None = None


CONTROLS = {
    # Starlette.add_route accepts route=. Router.add_route accepts endpoint=.
    "starlette-add-route": Control(
        absent_evidence=(("app.homepage", "starlette_route"),),
        diagnostic=("app.py", 10, "unresolved_registration"),
    ),
    # The fixture creates patterns but supplies no selected ROOT_URLCONF.
    "django-urlpatterns": Control(
        absent_evidence=(("django_urlpatterns.home", "django_registration"),),
        diagnostic=("django_urlpatterns.py", 9, "unresolved_registration"),
    ),
    "django-error-handlers": Control(unused=tuple(
        ("unused_variables", f"urls.{name}")
        for name in ("handler400", "handler403", "handler404", "handler500", "fallback_callback")
    )),
    "django-class-based-view": Control(unused=(
        ("unused_classes", "django_cbv.HomeView"),
        ("unused_classes", "django_cbv.PageFormatter"),
        ("unused_functions", "django_cbv.PageRenderer.format_page"),
    )),
    "django-management-command": Control(unused=(
        ("unused_classes", "django_management_command.Command"),
        ("unused_classes", "django_management_command.CommandPreview"),
        ("unused_functions", "django_management_command.parse_command"),
    )),
    "django-signal-receiver": Control(
        absent_evidence=(("django_signal_receiver.on_save", "django_signal"),),
        diagnostic=("django_signal_receiver.py", 5, "unresolved_registration"),
    ),
    "django-appconfig-ready": Control(unused=(
        ("unused_classes", "django_appconfig.DemoConfig"),
        ("unused_classes", "django_appconfig.StagingConfig"),
        ("unused_functions", "django_appconfig.inspect_config"),
    )),
    # This module loads zero tests. A test-file whitelist is a separate
    # reporting policy and cannot establish that a lifecycle hook runs.
    "python-unittest-lifecycle": Control(unused=tuple(
        ("unused_functions", f"test_case.{name}")
        for name in (
            "TestCase.setUpClass", "TestCase.tearDownClass",
            "setUpModule", "tearDownModule", "prepare_record",
        )
    )),
    # Gin does not supply timed(). The coverage diagnostic must stay visible.
    "python-decorator-factory-registration": Control(
        diagnostic=("trainer/components.py", 35, "unresolved_decorator"),
    ),
}


def evaluate_runtime_control(report, selected_paths, control):
    failures = []
    coverage = report["coverage"]
    expected_status = "incomplete" if control.diagnostic else "complete"
    if coverage["status"] != expected_status:
        failures.append(f"expected {expected_status} coverage, got {coverage['status']}")
    if (
        sorted(report["selected_paths"]) != selected_paths
        or coverage["selected_files"] != len(selected_paths)
        or coverage["extracted_files"] != len(selected_paths)
    ):
        failures.append("scan did not extract the original fixture's complete source set")
    if control.diagnostic:
        diagnostics = {
            (issue["path"], issue.get("line"), issue["kind"])
            for issue in coverage["issues"]
        }
        if control.diagnostic not in diagnostics:
            failures.append(f"missing coverage diagnostic {control.diagnostic}")
    elif coverage["issues"] or report["analysis_errors"]:
        failures.append("complete runtime control reported unresolved analysis")
    for bucket, name in control.unused:
        if not any(row["full_name"] == name for row in report[bucket]):
            failures.append(f"expected {name} in {bucket}")
    definitions = {row["name"] for row in report["definitions"].values()}
    evidence = {
        (row["qualified_name"], edge["kind"])
        for row in report["dead_code_evidence"]["symbols"]
        for edge in row["evidence"]
    }
    for name, kind in control.absent_evidence:
        if name not in definitions:
            failures.append(f"missing fixture definition {name}")
        if (name, kind) in evidence:
            failures.append(f"unexpected {kind} evidence on {name}")
    return failures
