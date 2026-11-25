"""Report assertions for individually reviewed Phase 0.3 runtime differences."""

from pathlib import Path, PurePosixPath


FINDING_BUCKETS = {
    "unused_functions", "unused_classes", "unused_imports", "unused_variables",
    "unused_parameters", "suppressed",
}


def relative_path(value):
    path = PurePosixPath(value)
    return (
        bool(path.parts) and not path.is_absolute() and ".." not in path.parts
        and path.as_posix() == value and "\\" not in value
    )


def parse_assertion(value):
    kind, _, arguments = value.partition(":")
    items = arguments.split(":")
    valid = {
        "coverage": lambda: len(items) == 1 and items[0] in {"complete", "incomplete"},
        "definition": lambda: len(items) == 1,
        "no-unused": lambda: len(items) == 1,
        "unused": lambda: len(items) == 2 and items[0] in FINDING_BUCKETS,
        "scoped-unused": lambda: (
            len(items) == 3 and relative_path(items[0])
            and items[1] in FINDING_BUCKETS | {"unused_files"}
        ),
        "issue": lambda: (
            len(items) == 3 and relative_path(items[1]) and items[2].isascii() and items[2].isdigit()
        ),
        "evidence": lambda: len(items) == 3 and items[1] in {"certain", "possible", "*"},
        "no-evidence": lambda: len(items) == 3 and items[1] in {"certain", "possible", "*"},
    }
    if not all(items) or kind not in valid or not valid[kind]():
        raise ValueError(f"invalid reviewed runtime assertion {value!r}")
    return kind, items


def scan_coverage(report, path):
    coverage = report["coverage"]
    sources = sorted(
        file.relative_to(path).as_posix() for file in path.rglob("*.py") if file.is_file()
    ) if path.is_dir() else [path.name]
    exact = (
        bool(sources) and sorted(report["selected_paths"]) == sources
        and coverage["selected_files"] == coverage["extracted_files"] == len(sources)
    )
    return {
        "coverage": coverage, "selected_paths": list(report["selected_paths"]),
    }, exact


def has_finding(report, bucket, name):
    if bucket == "unused_files":
        return any(row["file"] == name and row["rule_id"] == "SKY-E002" for row in report[bucket])
    return any(
        row["full_name"] == name and (
            bucket != "suppressed"
            or row.get("reason") == "covered by the unused class finding"
            and row.get("dead_code_classification") == "likely_dead"
        )
        for row in report.get(bucket, ())
    )


def observe_review(report, fixture, assertions, scan):
    path = Path(fixture).resolve()
    observation, complete_fixture = scan_coverage(report, path)
    coverage = observation["coverage"]
    definitions = {row["name"] for row in report["definitions"].values()}
    evidence = {
        (edge["kind"], edge["certainty"], row["qualified_name"])
        for row in report["dead_code_evidence"]["symbols"] for edge in row["evidence"]
    }
    scoped_reports = {}
    scoped_observations = {}
    passed = []
    for assertion in assertions:
        kind, parts = parse_assertion(assertion)
        if kind == "coverage":
            matched = coverage["status"] == parts[0] and (
                bool(coverage["issues"]) if parts[0] == "incomplete"
                else not coverage["issues"] and not report["analysis_errors"]
            )
        elif kind == "definition":
            matched = parts[0] in definitions
        elif kind == "no-unused":
            matched = parts[0] in definitions and not any(
                has_finding(report, bucket, parts[0]) for bucket in FINDING_BUCKETS
            )
        elif kind == "unused":
            matched = has_finding(report, *parts)
        elif kind == "scoped-unused":
            scope, bucket, name = parts
            child = path / scope
            if not path.is_dir() or child.resolve() != child or not child.exists():
                raise ValueError(f"review scope must be an existing fixture path: {scope!r}")
            if scope not in scoped_reports:
                current = scan(child)
                scoped_reports[scope] = current
                observed, exact = scan_coverage(current, child)
                scoped_observations[scope] = observed
                complete_fixture &= (
                    exact and current["coverage"]["status"] == "complete"
                    and not current["coverage"]["issues"] and not current["analysis_errors"]
                )
            matched = has_finding(scoped_reports[scope], bucket, name)
        elif kind == "issue":
            category, filename, line = parts
            matched = any(
                row["kind"] == category and row["path"] == filename
                and (line == "0" or row.get("line") == int(line))
                for row in coverage["issues"]
            )
        else:
            edge, certainty, name = parts
            observed = any(
                actual_edge == edge and target == name
                and certainty in {"*", actual_certainty}
                for actual_edge, actual_certainty, target in evidence
            )
            matched = observed if kind == "evidence" else name in definitions and not observed
        if matched:
            passed.append(assertion)
    return {
        "status": "passed" if complete_fixture and passed == assertions else "failed",
        "assertions": passed,
        **observation,
        **({"scopes": scoped_observations} if scoped_observations else {}),
    }
