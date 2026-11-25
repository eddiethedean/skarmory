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

BASELINE = (
    Path(__file__).resolve().parents[1]
    / "tests/conformance/baselines/skylos-5.7.0.toml"
)
SCHEMA_VERSION = 1
MILESTONE = "0.3"
RESULT_STATUSES = {
    "passed", "failed", "xfailed", "xpassed", "skipped", "blocked", "error",
}


def sha256(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def validate_revision(value, label):
    if not isinstance(value, str) or not re.fullmatch(r"[0-9a-f]{40}", value):
        raise ValueError(f"{label} must be a full lowercase Git SHA")


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
            result["blocker"] = f"{name}: fixture SHA-256 does not match the reviewed original"
    return evidence


def read_inventory(path):
    raw = path.read_bytes()
    inventory = tomllib.loads(raw.decode("utf-8"))
    if type(inventory.get("schema_version")) is not int or (
        inventory["schema_version"] != SCHEMA_VERSION
    ):
        raise ValueError("unsupported inventory schema_version")
    reference = tomllib.loads(BASELINE.read_text(encoding="utf-8"))
    if inventory.get("reference_revision") != reference["reference"]["git_revision"]:
        raise ValueError("inventory revision differs from the pinned Skylos baseline")
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
        assertions = string_list(case, "assertions", required=True, unique=True)
        if (
            "reference_xfail" in case
            and require_string(case, "reference_xfail") not in assertions
        ):
            raise ValueError(f"{case_id}: reference_xfail must name a mapped assertion")
        string_list(case, "prerequisites", unique=True)
        string_list(case, "omitted_assertions", unique=True)
        if "fixtures" in case:
            for fixture in string_list(case, "fixtures", unique=True):
                if fixture not in fixtures:
                    raise ValueError(f"{case_id}: unknown fixture {fixture!r}")
        for variant in string_list(case, "variants", required=True, unique=True):
            if variant not in variants:
                raise ValueError(f"{case_id}: unknown variant {variant!r}")
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


def verify_result(result, case):
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
    if status != "passed":
        if status not in RESULT_STATUSES:
            reasons.append(f"unknown Skarmory outcome {status!r}")
        else:
            reasons.append(f"Skarmory outcome {status!r} is not an ordinary pass")

    assertions = set(string_list(result, "assertions", unique=True))
    if assertions != set(mapped):
        reasons.append("executed assertions do not match the inventory")
    prerequisites = result.get("prerequisites")
    if not isinstance(prerequisites, dict) or (
        set(prerequisites) != set(case["prerequisites"])
        or any(status != "passed" for status in prerequisites.values())
    ):
        reasons.append("required prerequisites were not all verified")
    return reasons


def verify_ledger(ledger, identity, cases, deferred_ids, errors):
    variant = ledger["variant"]
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


def evaluate(inventory_path, candidate_revision, artifact_inputs, ledger_paths, fixture_root=None):
    report = {
        "schema_version": SCHEMA_VERSION,
        "candidate_revision": candidate_revision,
        "status": "blocked",
        "errors": [],
        "cases": [],
        "deferred_cases": [],
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
            if case_id not in results:
                reasons.append("required result is missing")
            else:
                try:
                    result = results[case_id]
                    reasons.extend(verify_result(result, case))
                    if case_fixtures and not fixture_blockers:
                        expected_hashes = {name: item["sha256"] for name, item in case_fixtures.items()}
                        if result.get("fixture_sha256") != expected_hashes:
                            reasons.append("receipt fixture hashes do not match the reviewed originals")
                except ValueError as error:
                    reasons.append(str(error))
            report["cases"].append({
                "case_id": case_id,
                "variant": variant_id,
                "status": "blocked" if reasons else "passed",
                "reasons": reasons,
            })

    if not errors and all(case["status"] == "passed" for case in report["cases"]):
        report["status"] = "passed"
    return report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--inventory", type=Path, required=True)
    parser.add_argument("--candidate-revision", required=True)
    parser.add_argument("--artifact", action="append", default=[], metavar="ROLE=PATH")
    parser.add_argument("--ledger", type=Path, action="append", default=[])
    parser.add_argument("--fixture-root", type=Path, help="checkout or directory containing reviewed fixtures")
    parser.add_argument("--report", type=Path, required=True)
    args = parser.parse_args()
    report = evaluate(
        args.inventory, args.candidate_revision, args.artifact, args.ledger, args.fixture_root
    )
    args.report.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    blocked = [case for case in report["cases"] if case["status"] != "passed"]
    for error in report["errors"]:
        print(error, file=sys.stderr)
    print(f"Dead-code release gate {report['status']} ({len(blocked)} blocked case variants)")
    return 0 if report["status"] == "passed" else 1


if __name__ == "__main__":
    sys.exit(main())
