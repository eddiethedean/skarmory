#!/usr/bin/env python3
"""Verify Phase 0.3 release evidence against the reviewed case inventory."""

import argparse
import hashlib
import json
import re
import sys
import tomllib
from collections import Counter
from pathlib import Path, PurePosixPath

from phase03_reference import BASELINE, phase03_revisions
from phase03_review_assertions import SCOPED_ASSERTIONS, parse_assertion

SCHEMA_VERSION = 1
MILESTONE = "0.3"
RESULT_STATUSES = {
    "passed", "failed", "xfailed", "xpassed", "skipped", "blocked", "error",
}
REVIEW_PORT = "tests/conformance/run_phase03_seed.py"
SDK_CLI_PARITY = "installed-sdk-native-cli-parity"
FRAMEWORK_CORPUS_SOURCE = "corpus/manifest.json"
REVIEW_COVERAGE_ASSERTION = "test/web_framework_fixtures.py:16"
APPROVED_EXCEPTION_TESTS = {
    "test/test_fastapi_truth_cycles.py::test_non_fastapi_click_scan_survives_reassigned_truth_alias":
        "phase02::analysis::tests::pinned_click_dead_code_control",
    "test/test_fastapi_route_precision.py::test_incomplete_fastapi_proofs_preserve_all_legacy_hints":
        "phase02::analysis::tests::pinned_fastapi_initializer_call_budget",
}
CLICK_CONTROL = "phase02::analysis::tests::pinned_click_dead_code_control"
DJANGO_ASYNC_SOURCE = "test/test_django_function_view_roots.py"
DJANGO_ASYNC_EXCEPTIONS = frozenset(
    f"{DJANGO_ASYNC_SOURCE}::test_unproved_function_callbacks_do_not_create_provider_roots[async_view-{confidence}]"
    for confidence in (0, 80)
)
DJANGO_ASYNC_ASSERTIONS = [f"{DJANGO_ASYNC_SOURCE}:{line}" for line in (29, 120, 121, 125)]
DJANGO_ASYNC_PREREQUISITES = [
    "installed-sdk", "native-framework-evidence", "grep-verification-disabled",
]
DJANGO_ASYNC_PATHS = ["manage.py", "pipeline.py", "providers.py", "settings.py", "urls.py", "views.py"]
DJANGO_ASYNC_CONTROLS = [
    "coverage:complete",
    "evidence:django_registration:certain:views.view",
    "evidence:direct_call:certain:pipeline.dispatch",
    "evidence:direct_call:certain:providers.NcaaClient.schedule_dates",
    "unused:unused_functions:providers.NjcaaClient.schedule_dates",
    "unused:unused_functions:providers.UnrelatedClient.schedule_dates",
]


def sha256(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def validate_revision(value, label):
    if not isinstance(value, str) or not re.fullmatch(r"[0-9a-f]{40}", value):
        raise ValueError(f"{label} must be a full lowercase Git SHA")


def passed_pytest_ids(report, source):
    module = source.removesuffix(".py").replace("/", ".")
    passed = set()
    for record in report.iter("testcase"):
        classname = record.attrib["classname"]
        if classname == module:
            scope = ""
        elif classname.startswith(f"{module}."):
            scope = classname.removeprefix(f"{module}.").replace(".", "::") + "::"
        else:
            continue
        case_id = f"{source}::{scope}{record.attrib['name']}"
        for outcome in ("skipped", "failure", "error"):
            if record.find(outcome) is not None:
                raise ValueError(f"{case_id}: frozen reference did not pass ({outcome})")
        if case_id in passed:
            raise ValueError(f"{case_id}: duplicate frozen reference result")
        passed.add(case_id)
    return passed


def require_string(record, field):
    value = record.get(field)
    if not isinstance(value, str) or not value.strip():
        raise ValueError(f"{field} must be a nonempty string")
    return value


def string_list(record, field, *, required=False, unique=False):
    values = record.get(field)
    if not isinstance(values, list) or (required and not values):
        raise ValueError(f"{field} must be {'a nonempty' if required else 'a'} list")
    if any(not isinstance(value, str) or not value.strip() for value in values):
        raise ValueError(f"{field} must contain nonempty strings")
    if unique and len(set(values)) != len(values):
        raise ValueError(f"{field} contains duplicates")
    return values


def indexed_records(records, key, *, deferred_ids=()):
    if not isinstance(records, list):
        raise ValueError(f"expected a list of records with {key}")
    indexed = {}
    for record in records:
        if not isinstance(record, dict):
            raise ValueError(f"expected a record with {key}")
        identity = require_string(record, key)
        if identity in deferred_ids:
            continue
        if identity in indexed:
            raise ValueError(f"duplicate {key} {identity!r}")
        indexed[identity] = record
    return indexed


def read_fixtures(inventory):
    fixtures = indexed_records(inventory.get("fixtures", []), "path")
    for name, fixture in fixtures.items():
        relative_path = PurePosixPath(name)
        if (
            relative_path.is_absolute() or not relative_path.parts
            or ".." in relative_path.parts or relative_path.as_posix() != name
            or any(character in name for character in ("\\", ":", "\0"))
        ):
            raise ValueError(f"fixture path must be a normalized relative path: {name!r}")
        review = ("sha256", "provenance", "license")
        if any(field in fixture for field in review):
            for field in review:
                require_string(fixture, field)
            if not re.fullmatch(r"[0-9a-f]{64}", fixture["sha256"]):
                raise ValueError(f"{name}: fixture sha256 must be a full lowercase SHA-256")
    return fixtures


def verify_fixtures(fixtures, root):
    evidence = {}
    for fixture in fixtures:
        name = fixture["path"]
        result = {
            key: fixture[key]
            for key in ("path", "sha256", "provenance", "license")
            if key in fixture
        }
        evidence[name] = result
        if root is None:
            result["blocker"] = f"{name}: fixture root was not supplied"
            continue
        try:
            path = root.resolve() / name
            if path.resolve(strict=True) != path or not path.is_file():
                result["blocker"] = f"{name}: fixture must be a regular file at its declared path"
                continue
            result["observed_sha256"] = sha256(path)
        except FileNotFoundError:
            result["blocker"] = f"{name}: required fixture is missing"
            continue
        except (OSError, RuntimeError) as error:
            result["blocker"] = f"{name}: unable to verify fixture: {error}"
            continue
        if "sha256" not in fixture:
            result["blocker"] = f"{name}: original fixture provenance, digest, and license are unverified"
        elif result["observed_sha256"] != fixture["sha256"]:
            result["blocker"] = f"{name}: fixture SHA-256 does not match the reviewed baseline"
    return evidence


def read_inventory(path):
    raw = path.read_bytes()
    inventory = tomllib.loads(raw.decode("utf-8"))
    if type(inventory.get("schema_version")) is not int or (
        inventory["schema_version"] != SCHEMA_VERSION
    ):
        raise ValueError("unsupported inventory schema_version")
    reference = tomllib.loads(BASELINE.read_text(encoding="utf-8"))
    _, selected = phase03_revisions(reference)
    if inventory.get("reference_revision") != selected:
        raise ValueError("inventory revision differs from the pinned Phase 0.3 Skylos baseline")
    if type(inventory.get("inventory_complete")) is not bool:
        raise ValueError("inventory_complete must be a boolean")

    variants = indexed_records(inventory.get("variants"), "id")
    cases = indexed_records(inventory.get("cases"), "id")
    fixtures = read_fixtures(inventory)
    if not variants or not cases:
        raise ValueError("inventory must include cases and variants")
    for variant in variants.values():
        for field in ("platform", "backend", "layer", "environment", "artifact"):
            require_string(variant, field)
        if variant["layer"] not in {"engine", "cli", "sdk", "consumer"}:
            raise ValueError(f"unknown layer for {variant['id']}")

    by_variant = {variant: {} for variant in variants}
    for case_id, case in cases.items():
        for field in ("source", "provenance"):
            require_string(case, field)
        milestone = require_string(case, "milestone")
        if not re.fullmatch(r"0\.(?:[3-9]|1[0-8])", milestone):
            raise ValueError(f"{case_id}: unknown owning milestone {milestone!r}")
        languages = string_list(case, "languages", required=True, unique=True)
        if milestone == MILESTONE and languages != ["python"]:
            raise ValueError(f"{case_id}: Phase 0.3 only accepts Python cases")
        if not isinstance(case.get("port"), str):
            raise ValueError(f"{case_id}: port must be a string")
        if "blocker" in case:
            require_string(case, "blocker")
        if "diagnostic_port" in case:
            require_string(case, "diagnostic_port")
            if milestone != MILESTONE or case["port"] or "blocker" not in case:
                raise ValueError(f"{case_id}: a diagnostic port requires a release blocker and no release port")
        if "reviewed_exception" in case:
            exception = case["reviewed_exception"]
            sdk_exception = case_id in DJANGO_ASYNC_EXCEPTIONS
            if ((case_id not in APPROVED_EXCEPTION_TESTS and not sdk_exception)
                    or milestone != MILESTONE
                    or case["port"] != REVIEW_PORT
                    or not isinstance(exception, dict)
                    or set(exception) != {"reason", "approval", "tracking_issue", "fixture_sha256"}
                    or {"reference_xfail", "blocker"} & case.keys()
                    or sdk_exception != ("runtime_review" in case)):
                raise ValueError(f"{case_id}: invalid approved compatibility exception")
            require_string(exception, "reason")
            if not re.fullmatch(r"[0-9a-f]{64}", require_string(exception, "fixture_sha256")):
                raise ValueError(f"{case_id}: approved exception must pin the original fixture bytes")
            approval = (
                "approved for the two mounted Django async cases"
                if sdk_exception else "approved in the issue 43 implementation request"
            )
            if (exception["approval"] != approval
                    or exception["tracking_issue"] != "https://github.com/eddiethedean/skarmory/issues/41"):
                raise ValueError(f"{case_id}: exception must identify the Phase 0.3 release review")
        if "runtime_review" in case:
            review = case["runtime_review"]
            if not isinstance(review, dict) or set(review) != {"reason", "evidence", "assertions"}:
                raise ValueError(f"{case_id}: invalid reviewed runtime control")
            require_string(review, "reason")
            require_string(review, "evidence")
            checks = [
                parse_assertion(value)
                for value in string_list(review, "assertions", required=True, unique=True)
            ]
            coverage = [parts[0] for kind, parts in checks if kind == "coverage"]
            if len(coverage) != 1 or len(checks) < 2 or (
                coverage == ["incomplete"] and not any(kind == "issue" for kind, _ in checks)
            ):
                raise ValueError(f"{case_id}: reviewed runtime coverage requires explicit controls")
            if milestone != MILESTONE or case["port"] != REVIEW_PORT or "blocker" in case:
                raise ValueError(f"{case_id}: reviewed runtime control requires a mapped Phase 0.3 SDK case")
        assertions = string_list(case, "assertions", required=True, unique=True)
        if (
            "reference_xfail" in case
            and require_string(case, "reference_xfail") not in assertions
        ):
            raise ValueError(f"{case_id}: reference_xfail must name a mapped assertion")
        string_list(case, "prerequisites", unique=True)
        string_list(case, "omitted_assertions", unique=True)
        if "reviewed_exception" in case and case_id in DJANGO_ASYNC_EXCEPTIONS and (
            case["source"] != DJANGO_ASYNC_SOURCE
            or assertions != DJANGO_ASYNC_ASSERTIONS
            or case["prerequisites"] != DJANGO_ASYNC_PREREQUISITES
            or case["omitted_assertions"]
            or case["runtime_review"]["assertions"] != DJANGO_ASYNC_CONTROLS
        ):
            raise ValueError(f"{case_id}: approved exception must preserve the original assertions and controls")
        if "fixtures" in case:
            for fixture in string_list(case, "fixtures", unique=True):
                if fixture not in fixtures:
                    raise ValueError(f"{case_id}: unknown fixture {fixture!r}")
        for variant in string_list(case, "variants", required=True, unique=True):
            if variant not in variants:
                raise ValueError(f"{case_id}: unknown variant {variant!r}")
            if "runtime_review" in case and variants[variant]["layer"] != "sdk":
                raise ValueError(f"{case_id}: reviewed runtime controls require the installed SDK")
            if (case_id in APPROVED_EXCEPTION_TESTS and "reviewed_exception" in case
                    and variants[variant]["layer"] != "engine"):
                raise ValueError(f"{case_id}: the approved native control requires an engine artifact")
            by_variant[variant][case_id] = case
    if unused := [variant for variant, mapped in by_variant.items() if not mapped]:
        raise ValueError(f"variants without inventory cases: {', '.join(unused)}")
    expected = {}
    for variant, mapped in by_variant.items():
        required = {case_id: case for case_id, case in mapped.items() if case["milestone"] == MILESTONE}
        if required:
            expected[variant] = required
    if not expected:
        raise ValueError("inventory must include required Phase 0.3 Python cases")
    return inventory, hashlib.sha256(raw).hexdigest(), variants, expected


def reject_duplicate_json_keys(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValueError(f"duplicate JSON key {key!r}")
        result[key] = value
    return result


def reject_invalid_json_constant(value):
    raise ValueError(f"invalid JSON constant {value!r}")


def load_ledgers(paths, errors):
    ledgers = []
    for path in paths:
        try:
            receipt = json.loads(
                path.read_text(encoding="utf-8"),
                object_pairs_hook=reject_duplicate_json_keys,
                parse_constant=reject_invalid_json_constant,
            )
            if not isinstance(receipt, dict):
                raise ValueError("expected a receipt object")
            require_string(receipt, "variant")
        except (OSError, ValueError) as error:
            errors.append(f"{path}: {error}")
        else:
            ledgers.append(receipt)
    return ledgers


def verify_preview_result(result, case):
    reasons = []
    reference_status = require_string(result, "reference_status")
    status = require_string(result, "status")
    expected_failure = case.get("reference_xfail")
    expected_status = "xfailed" if expected_failure else "passed"
    if reference_status != expected_status:
        reasons.append(
            f"reference outcome {reference_status!r} lacks executed evidence "
            f"for the expected {expected_status!r} result"
        )
    if result.get("reference_failed_assertion") != expected_failure:
        reasons.append("expected failing reference assertion does not match the execution evidence")
    mapped = case["assertions"]
    required_reference = mapped[:mapped.index(expected_failure)] if expected_failure else mapped
    reference_assertions = string_list(result, "reference_assertions", unique=True)
    if reference_assertions != required_reference:
        reasons.append("completed reference assertions do not match the inventory")
    is_exception = "reviewed_exception" in case
    if is_exception and case["id"] in APPROVED_EXCEPTION_TESTS:
        reasons.extend(verify_exception_control(result, case))
        return reasons
    review = case.get("runtime_review")
    if not review and status != "passed":
        if status not in RESULT_STATUSES:
            reasons.append(f"unknown Skarmory outcome {status!r}")
        else:
            reasons.append(f"Skarmory outcome {status!r} is not an ordinary pass")

    assertions = set(string_list(result, "assertions", unique=True))
    if (not review or status == "passed") and assertions != set(mapped):
        reasons.append("executed assertions do not match the inventory")
    if review:
        if status not in {"passed", "failed"} or not assertions <= set(mapped):
            reasons.append("the original candidate case did not execute on the reviewed fixture")
        if (
            status == "failed" and case["source"] != FRAMEWORK_CORPUS_SOURCE
            and not matches_reviewed_failure(case, result.get("candidate_failed_assertion"))
        ):
            reasons.append("original candidate failure does not identify a mapped assertion")
        reasons.extend(verify_runtime_result(result.get("reviewed_runtime"), review))
    elif "reviewed_runtime" in result:
        reasons.append("unexpected reviewed runtime result for an ordinary parity case")
    prerequisites = result.get("prerequisites")
    if not isinstance(prerequisites, dict) or (
        set(prerequisites) != set(case["prerequisites"])
        or any(status != "passed" for status in prerequisites.values())
    ):
        reasons.append("required prerequisites were not all verified")
    if is_exception and case["id"] in DJANGO_ASYNC_EXCEPTIONS:
        if (
            status != "failed"
            or assertions != {DJANGO_ASYNC_ASSERTIONS[0]}
            or result.get("candidate_failed_assertion") != DJANGO_ASYNC_ASSERTIONS[1]
        ):
            reasons.append("approved Django exception requires the original candidate assertion failure")
        control = result.get("reviewed_runtime")
        if (not isinstance(control, dict)
                or control.get("fixture_sha256") != case["reviewed_exception"]["fixture_sha256"]):
            reasons.append("approved Django exception fixture bytes differ from the review")
        elif control.get("selected_paths") != DJANGO_ASYNC_PATHS:
            reasons.append("approved Django exception did not analyze every original source")
    return reasons


def verify_exception_control(result, case):
    if result["status"] != "blocked" or string_list(result, "assertions", unique=True):
        return ["approved exception must preserve the original candidate case without pass credit"]
    prerequisites = result.get("prerequisites")
    if prerequisites != dict.fromkeys(case["prerequisites"], "passed"):
        return ["approved exception control did not verify its original setup"]
    evidence = result.get("native_test")
    expected = APPROVED_EXCEPTION_TESTS[case["id"]]
    if not isinstance(evidence, dict) or evidence.get("returncode") != 0:
        return ["approved exception control did not pass"]
    argv = string_list(evidence, "argv", required=True)
    fixtures = evidence.get("fixture_sha256")
    stdout = require_string(evidence, "stdout")
    if (argv[1:4] != ["--ignored", "--exact", expected]
            or not isinstance(fixtures, dict) or set(fixtures) != {"app.py"}
            or not re.search(rf"^test {re.escape(expected)} \.\.\. ok$", stdout, re.MULTILINE)):
        return ["approved exception did not run the named native control on its staged fixture"]
    if fixtures["app.py"] != case["reviewed_exception"]["fixture_sha256"]:
        return ["approved exception fixture bytes differ from the review"]
    control = evidence.get("exception_control")
    if not isinstance(control, dict):
        return ["approved exception native observations are missing"]
    if expected == CLICK_CONTROL:
        verified = (
            control.get("complete") is True
            and string_list(control, "unused", unique=True) == ["app.unused_neighbor"]
            and control.get("entry_declarations") == ["possible"]
            and control.get("entry_called") is False
        )
    else:
        verified = (
            control.get("complete") is False
            and control.get("incomplete_reason") == "call_event_budget"
            and control.get("call_events") == 200
            and control.get("roots") == []
            and control.get("callback_parameters") == 0
            and control.get("injected_parameters") == 0
            and control.get("issues") == [{
                "kind": "analysis_limit",
                "path": "app.py",
                "message": "FastAPI initializer call-event budget exceeded",
            }]
        )
    return [] if verified else ["approved exception native observations did not satisfy the control"]


def matches_reviewed_failure(case, assertion):
    return assertion in case["assertions"] or (
        assertion == REVIEW_COVERAGE_ASSERTION
        and "coverage:incomplete" in case.get("runtime_review", {}).get("assertions", ())
    )


def read_runtime_coverage(result, expected):
    paths = string_list(result, "selected_paths", required=True, unique=True)
    coverage = result.get("coverage")
    if not isinstance(coverage, dict) or (
        coverage.get("status") != expected
        or type(coverage.get("selected_files")) is not int
        or type(coverage.get("extracted_files")) is not int
        or coverage.get("selected_files") != len(paths)
        or coverage.get("extracted_files") != len(paths)
        or not isinstance(coverage.get("issues"), list)
        or bool(coverage["issues"]) != (expected == "incomplete")
    ):
        return paths, None
    return paths, coverage["issues"]


def verify_runtime_result(result, review):
    if not isinstance(result, dict):
        return ["reviewed runtime result is missing"]
    reasons = []
    if result.get("status") != "passed":
        reasons.append("reviewed runtime control did not pass")
    if string_list(result, "assertions", unique=True) != review["assertions"]:
        reasons.append("executed reviewed runtime assertions do not match the inventory")
    expected = "complete" if "coverage:complete" in review["assertions"] else "incomplete"
    paths, issues = read_runtime_coverage(result, expected)
    checks = [parse_assertion(value) for value in review["assertions"]]
    if issues is None:
        reasons.append("reviewed runtime coverage does not match the inventory")
    elif expected == "incomplete":
        for kind, parts in checks:
            if kind != "issue":
                continue
            category, path, line = parts
            if not any(
                isinstance(issue, dict) and issue.get("kind") == category
                and issue.get("path") == path and (line == "0" or issue.get("line") == int(line))
                for issue in issues
            ):
                reasons.append(f"reviewed runtime diagnostic is missing: {':'.join(parts)}")
    scopes = result.get("scopes", {})
    expected_scopes = {parts[0] for kind, parts in checks if kind in SCOPED_ASSERTIONS}
    if not isinstance(scopes, dict) or scopes.keys() != expected_scopes:
        reasons.append("reviewed runtime scopes do not match the inventory")
    else:
        for scope, observed in scopes.items():
            prefix = scope + "/"
            selected = {path.removeprefix(prefix) for path in paths if path.startswith(prefix)}
            if scope in paths:
                selected.add(PurePosixPath(scope).name)
            if not isinstance(observed, dict):
                reasons.append(f"reviewed runtime scope is missing: {scope}")
                continue
            scoped_paths, scoped_issues = read_runtime_coverage(observed, "complete")
            if scoped_issues is None or set(scoped_paths) != selected:
                reasons.append(f"reviewed runtime scope did not extract the same fixture sources: {scope}")
    return reasons


def verify_ledger(ledger, identity, cases, deferred_ids, errors):
    variant = ledger["variant"]
    if ledger.get("diagnostic_only"):
        errors.append(f"{variant}: diagnostic receipts cannot qualify release cases")
        return {}
    for field, expected in identity.items():
        if ledger.get(field) != expected:
            errors.append(f"{variant}: {field} does not match the release input")
    if type(ledger.get("schema_version")) is not int:
        errors.append(f"{variant}: schema_version must be an integer")
    if identity["artifact_sha256"] is None:
        errors.append(f"{variant}: candidate artifact is missing")
    if ledger.get("runner_status") != "passed" or ledger.get("runner_errors") != []:
        errors.append(f"{variant}: runner did not complete successfully")
    try:
        string_list(ledger, "reference_command", required=True)
        string_list(ledger, "candidate_command", required=True)
        results = indexed_records(ledger.get("results"), "case_id", deferred_ids=deferred_ids)
    except ValueError as error:
        errors.append(f"{variant}: {error}")
        return {}
    for case_id in sorted(results.keys() - cases.keys()):
        errors.append(f"{variant}: unexpected case {case_id}")
    return results


def evaluate(
    inventory_path, candidate_revision, artifact_inputs, ledger_paths, fixture_root=None,
    *, diagnostic_inventory=None,
):
    report = {
        "schema_version": SCHEMA_VERSION,
        "candidate_revision": candidate_revision,
        "status": "blocked",
        "errors": [],
        "cases": [],
        "deferred_cases": [],
        "reviewed_exceptions": [],
        "artifacts": {},
        "fixtures": {},
        "ledgers": [],
    }
    errors = report["errors"]
    try:
        validate_revision(candidate_revision, "candidate_revision")
        inventory, inventory_hash, variants, expected = read_inventory(inventory_path)
    except (OSError, ValueError) as error:
        errors.append(str(error))
        return report

    report["reference_revision"] = inventory["reference_revision"]
    report["inventory_sha256"] = inventory_hash
    report["inventory_complete"] = inventory["inventory_complete"]
    required_fixtures = {
        fixture for cases in expected.values() for case in cases.values()
        for fixture in case.get("fixtures", [])
    }
    report["fixtures"] = verify_fixtures(
        [fixture for fixture in inventory.get("fixtures", []) if fixture["path"] in required_fixtures],
        fixture_root,
    )
    deferred_by_variant = {variant: set() for variant in variants}
    for case in inventory["cases"]:
        if case["milestone"] != MILESTONE:
            report["deferred_cases"].append(case)
            for variant in case["variants"]:
                deferred_by_variant[variant].add(case["id"])
    if not inventory["inventory_complete"]:
        errors.append("the case and variant inventory is not complete")
    if diagnostic_inventory is not None:
        try:
            _, _, _, diagnostic_cases = read_inventory(diagnostic_inventory)
        except (OSError, ValueError) as error:
            errors.append(f"unable to read diagnostic inventory: {error}")
        else:
            pending = sum(len(cases) for cases in diagnostic_cases.values())
            errors.append(f"{pending} required case variants remain in the diagnostic inventory")

    roles = {variants[variant]["artifact"] for variant in expected}
    deferred_roles = {variant["artifact"] for variant in variants.values()} - roles
    for item in artifact_inputs:
        role, separator, raw_path = item.partition("=")
        if role in deferred_roles:
            continue
        if not separator or not raw_path or role not in roles:
            errors.append(f"unknown or malformed candidate artifact input {item!r}")
            continue
        if role in report["artifacts"]:
            errors.append(f"duplicate candidate artifact {role!r}")
            continue
        path = Path(raw_path)
        try:
            report["artifacts"][role] = {"name": path.name, "sha256": sha256(path)}
        except OSError as error:
            errors.append(f"{role}: {error}")

    deferred_variants = variants.keys() - expected.keys()
    ledgers = [
        ledger for ledger in load_ledgers(ledger_paths, errors)
        if ledger["variant"] not in deferred_variants
    ]
    report["ledgers"] = ledgers
    counts = Counter(ledger["variant"] for ledger in ledgers)
    by_variant = {ledger["variant"]: ledger for ledger in ledgers}
    for variant in sorted(by_variant.keys() - variants.keys()):
        errors.append(f"unexpected ledger variant {variant!r}")
    for variant_id, cases in expected.items():
        variant = variants[variant_id]
        variant_errors = []
        if counts[variant_id] == 0:
            variant_errors.append(f"{variant_id}: missing ledger")
            results = {}
        elif counts[variant_id] > 1:
            variant_errors.append(f"{variant_id}: duplicate ledgers")
            results = {}
        else:
            artifact = report["artifacts"].get(variant["artifact"], {})
            identity = {
                "schema_version": SCHEMA_VERSION,
                "reference_revision": inventory["reference_revision"],
                "inventory_sha256": inventory_hash,
                "candidate_revision": candidate_revision,
                "environment": variant["environment"],
                "artifact_sha256": artifact.get("sha256"),
            }
            results = verify_ledger(
                by_variant[variant_id], identity, cases, deferred_by_variant[variant_id], variant_errors,
            )
        errors.extend(variant_errors)

        for case_id, case in cases.items():
            reasons = []
            if not case["port"].strip():
                reasons.append("Skarmory port is not mapped")
            if blocker := case.get("blocker"):
                reasons.append(blocker)
            case_fixtures = {
                name: report["fixtures"][name] for name in case.get("fixtures", [])
            }
            fixture_blockers = [
                fixture["blocker"] for fixture in case_fixtures.values() if "blocker" in fixture
            ]
            reasons.extend(fixture_blockers)
            if variant_errors:
                reasons.append("variant ledger is missing or invalid")
            result = results.get(case_id, {})
            if not result:
                reasons.append("required result is missing")
            else:
                try:
                    reasons.extend(verify_preview_result(result, case))
                    if SDK_CLI_PARITY in case["prerequisites"]:
                        cli_digest = report["artifacts"].get("cli", {}).get("sha256")
                        if cli_digest is None or result.get("native_cli_sha256") != cli_digest:
                            reasons.append("native CLI parity artifact does not match the release input")
                    if case_fixtures and not fixture_blockers:
                        expected_hashes = {name: item["sha256"] for name, item in case_fixtures.items()}
                        if result.get("fixture_sha256") != expected_hashes:
                            reasons.append("receipt fixture hashes do not match the reviewed originals")
                except ValueError as error:
                    reasons.append(str(error))
            preview_status = "blocked" if reasons else "passed"
            candidate_status = result.get("status")
            if "reviewed_exception" in case:
                report["reviewed_exceptions"].append({
                    "case_id": case_id,
                    "variant": variant_id,
                    "status": "blocked" if reasons else "accepted",
                    "preview_status": preview_status,
                    "reasons": reasons,
                    "review": case["reviewed_exception"],
                    "original_assertions": case["assertions"],
                    "original_reference_status": result.get("reference_status"),
                    "original_candidate_status": candidate_status,
                    "original_candidate_failed_assertion": result.get("candidate_failed_assertion"),
                    "native_control": result.get("native_test", result.get("reviewed_runtime")),
                })
                if reasons:
                    errors.append(f"{case_id}: approved exception control is missing or invalid")
                continue
            if preview_status == "passed" and candidate_status != "passed":
                reasons.append(f"Skarmory outcome {candidate_status!r} is not an ordinary pass")
            decision = {
                "case_id": case_id,
                "variant": variant_id,
                "status": "blocked" if reasons else "passed",
                "preview_status": preview_status,
                "reasons": reasons,
            }
            if "runtime_review" in case:
                decision["runtime_review"] = case["runtime_review"]
                decision["original_candidate_status"] = candidate_status
                decision["original_candidate_failed_assertion"] = result.get("candidate_failed_assertion")
            report["cases"].append(decision)

    counts = Counter(case["status"] for case in report["cases"])
    report["counts"] = {status: counts[status] for status in ("passed", "blocked")}
    report["reviewed_runtime_preview_passes"] = sum(
        "runtime_review" in case and case["preview_status"] == "passed" for case in report["cases"]
    )
    if not errors and not counts["blocked"]:
        report["status"] = "passed"
    return report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--inventory", type=Path, required=True)
    parser.add_argument(
        "--diagnostic-inventory", type=Path,
        help="inventory of required cases still outside release reconciliation",
    )
    parser.add_argument("--candidate-revision", required=True)
    parser.add_argument("--artifact", action="append", default=[], metavar="ROLE=PATH")
    parser.add_argument("--ledger", type=Path, action="append", default=[])
    parser.add_argument("--fixture-root", type=Path, help="checkout or directory containing reviewed fixtures")
    parser.add_argument("--report", type=Path, required=True)
    args = parser.parse_args()
    report = evaluate(
        args.inventory, args.candidate_revision, args.artifact, args.ledger, args.fixture_root,
        diagnostic_inventory=args.diagnostic_inventory,
    )
    args.report.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    for error in report["errors"]:
        print(error, file=sys.stderr)
    counts = report.get("counts", {})
    print(
        f"Dead-code release gate {report['status']} "
        f"({counts.get('passed', 0)} original passes, {counts.get('blocked', 0)} blocked case variants)"
    )
    print(f"Reviewed runtime preview passes: {report.get('reviewed_runtime_preview_passes', 0)}")
    return 0 if report["status"] == "passed" else 1


if __name__ == "__main__":
    sys.exit(main())
