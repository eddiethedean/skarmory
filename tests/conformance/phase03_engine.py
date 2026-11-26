"""Stage pinned fixtures and verify their exact Rust engine ports."""

import argparse
import hashlib
import json
import re
import runpy
import subprocess
import sys
from collections import defaultdict
from itertools import product
from pathlib import Path
from tempfile import TemporaryDirectory
from types import SimpleNamespace
from unittest.mock import patch

SOURCE = "test/test_cleanup_execution.py"
CASE = f"{SOURCE}::test_source_removed_after_rescan_preserves_the_incomplete_receipt"
ASSERTIONS = [f"{SOURCE}:{line}" for line in range(387, 392)]
PREREQUISITES = [f"{SOURCE}:372", "native-core-cleanup-removal"]
TEST = "phase02::cleanup::tests::pinned_source_removed_after_comparison_preserves_receipts"
IMPORT_ROOT_SOURCE = "test/test_fastapi_nested_projects.py"
IMPORT_ROOT_TEST = "test_import_roots_name_package_and_project_roots"
IMPORT_ROOT_CASE = f"{IMPORT_ROOT_SOURCE}::{IMPORT_ROOT_TEST}"
IMPORT_ROOT_ASSERTIONS = [f"{IMPORT_ROOT_SOURCE}:{line}" for line in (209, 212, 216, 217)]
IMPORT_ROOT_PREREQUISITES = [
    "original regular package and source-layout roots",
    "tool-only manifest control",
]
INCLUDE_SOURCE = "test/test_fastapi_import_scope_boundaries.py"
INCLUDE_BUDGET_CASE = f"{INCLUDE_SOURCE}::test_possible_include_graph_honors_abstract_step_budget"
INCLUDE_CYCLE_CASE = f"{INCLUDE_SOURCE}::test_possible_include_graph_retains_cycles_and_rejects_uncalled_owner"
INCLUDE_TEST = "phase02::analysis::fastapi::tests::pinned_fastapi_include_graph"
INCLUDE_CASES = {
    INCLUDE_BUDGET_CASE: (
        [f"{INCLUDE_SOURCE}:{line}" for line in (76, 78, 79)],
        ["original twenty-edge router graph and bounded step limit"],
    ),
    INCLUDE_CYCLE_CASE: (
        [f"{INCLUDE_SOURCE}:95"],
        ["original router graph with cycle and uncalled owner"],
    ),
}
TRUTH_SOURCE = "test/test_fastapi_truth_cycles.py"
TRUTH_FUNCTION = "test_acyclic_truth_alias_preserves_short_circuit_parity"
TRUTH_TEST = "phase02::analysis::tests::pinned_fastapi_truth_chain"
TRUTH_ASSERTIONS = [f"{TRUTH_SOURCE}:{line}" for line in (23, 65, 66)]
TRUTH_PREREQUISITES = [
    "Native FastAPI route registration index",
    "Original synthesized Python route fixture",
]
TRUTH_CASES = {
    f"{TRUTH_SOURCE}::{TRUTH_FUNCTION}[{operator}-{initial}-{length}]": (length, initial, operator)
    for length, initial, operator in product((0, 1, 2, 1100, 1101), (False, True), ("and", "or"))
}
TRACED_ENGINE_CASES = {*INCLUDE_CASES, *TRUTH_CASES}
ENGINE_CASES = {CASE, IMPORT_ROOT_CASE, *TRACED_ENGINE_CASES}
ROOT = Path(__file__).resolve().parents[2]


def locate_test_artifact(messages, source):
    executables = []
    for line in messages:
        row = json.loads(line)
        target = row.get("target", {})
        if (
            row.get("reason") == "compiler-artifact" and row.get("profile", {}).get("test") is True
            and target.get("name") == "skarmory_core" and target.get("kind") == ["lib"]
            and Path(target["src_path"]).resolve() == source.resolve() and row.get("executable")
        ):
            executables.append(Path(row["executable"]).resolve())
    if len(executables) != 1 or not executables[0].is_file():
        raise ValueError("Cargo must report exactly one built skarmory_core library test executable")
    return executables[0]


def passed_exact_test(completed, test=TEST):
    return (
        completed.returncode == 0 and not completed.stderr.strip()
        and re.fullmatch(
            rf"running 1 test\ntest {re.escape(test)} \.\.\. ok\n\n"
            r"test result: ok\. 1 passed; 0 failed; 0 ignored; 0 measured; "
            r"\d+ filtered out; finished in \d+(?:\.\d+)?s",
            completed.stdout.strip(),
        ) is not None
    )


def stage_import_roots(source, fixture):
    class StagedFixture(Exception):
        pass

    def stage_complete(root):
        if root != fixture:
            raise ValueError("the reference fixture must write to the staged root")
        raise StagedFixture

    original = source[IMPORT_ROOT_TEST]
    with patch.dict(original.__globals__, {"PythonImportRoots": stage_complete}):
        try:
            original(fixture)
        except StagedFixture:
            return
    raise ValueError("the pinned import-root fixture did not construct its project")


def replay_include_graph(key, source, fixture, argv, env, trace):
    import pytest

    upstream = source["fastapi_liveness"]
    default_limit = upstream._MAX_ANALYSIS_STEPS
    graph_file = fixture / "include-graph.json"
    native_report = fixture / "include-report.json"
    stages = []

    class Project:
        def __init__(self, definitions, parsed, root):
            if definitions or parsed or root != fixture:
                raise ValueError("the pinned include fixture must build its graph directly")
            self.active_objects = set()
            self.possible_includes = []
            self.includes = defaultdict(set)
            self.report = SimpleNamespace()

        def _possibly_included(self):
            if stages or any(edge.owner_key != edge.owner for edge in self.possible_includes):
                raise ValueError("unexpected invocation or owner key in the pinned include graph")
            edges = [
                {"receiver": edge.receiver, "child": edge.child, "owner": edge.owner}
                for edge in self.possible_includes
            ]
            edges.extend(
                {"receiver": receiver, "child": child, "owner": None}
                for receiver, children in sorted(self.includes.items()) for child in sorted(children)
            )
            limit = upstream._MAX_ANALYSIS_STEPS
            graph_file.write_text(json.dumps({
                "roots": sorted(self.active_objects),
                "edges": edges,
                "limit": None if limit == default_limit else limit,
            }), encoding="utf-8")
            fixtures = {graph_file.name: hashlib.sha256(graph_file.read_bytes()).hexdigest()}
            outcome = subprocess.run(
                argv,
                env={
                    **env,
                    "SKARMORY_PHASE03_FASTAPI_INCLUDE_FIXTURE": str(graph_file),
                    "SKARMORY_PHASE03_FASTAPI_INCLUDE_REPORT": str(native_report),
                },
                capture_output=True, text=True, timeout=60, check=False,
            )
            stages.append((outcome, fixtures))
            if not passed_exact_test(outcome, INCLUDE_TEST):
                raise ValueError("the named native include graph test failed")
            result = json.loads(native_report.read_text(encoding="utf-8"))
            self.report.complete = result["status"] == "complete"
            if result["status"] == "incomplete":
                self.report.incomplete_reason = result["reason"]
                raise source["FastAPIAnalysisLimit"](result["reason"])
            if not self.report.complete:
                raise ValueError("unexpected native include graph status")
            return set(result["reachable"])

    original = source[key.split("::")[1]]
    previous_trace = sys.gettrace()
    failure = None
    try:
        with patch.dict(original.__globals__, {"FastAPIProject": Project}):
            with pytest.MonkeyPatch.context() as monkeypatch:
                sys.settrace(trace)
                if key == INCLUDE_BUDGET_CASE:
                    original(fixture, monkeypatch)
                else:
                    original(fixture)
    except (AssertionError, ValueError, OSError, pytest.fail.Exception,
            source["FastAPIAnalysisLimit"]) as error:
        failure = str(error)
    finally:
        sys.settrace(previous_trace)
    if len(stages) != 1:
        raise ValueError("the original include fixture did not invoke the native graph exactly once")
    outcome, fixtures = stages[0]
    result = json.loads(native_report.read_text(encoding="utf-8")) if native_report.is_file() else None
    return outcome, fixtures, {"graph": result, "assertion_error": failure}


def replay_truth_chain(key, source, fixture, argv, env, trace):
    source_path = fixture / "app.py"
    native_report = fixture / "truth-report.json"
    stages = []

    class Project:
        def __init__(self, definitions, parsed, root):
            if definitions or root != fixture or len(parsed) != 1 or parsed[0].path != source_path:
                raise ValueError("the original truth fixture must parse its staged app.py")

        def collect(self):
            if stages:
                raise ValueError("the original truth fixture must invoke native analysis once")
            fixtures = {source_path.name: hashlib.sha256(source_path.read_bytes()).hexdigest()}
            outcome = subprocess.run(
                argv,
                env={
                    **env,
                    "SKARMORY_PHASE03_FASTAPI_TRUTH_FIXTURE": str(source_path),
                    "SKARMORY_PHASE03_FASTAPI_TRUTH_REPORT": str(native_report),
                },
                capture_output=True, text=True, timeout=60, check=False,
            )
            stages.append((outcome, fixtures))
            if not passed_exact_test(outcome, TRUTH_TEST):
                raise ValueError("the named native truth-chain test failed")
            report = json.loads(native_report.read_text(encoding="utf-8"))
            return SimpleNamespace(
                complete=report["complete"],
                incomplete_reason=report["issues"],
                roots={(str(fixture / path), line) for path, line in report["roots"]},
                uncertain={(str(fixture / path), line) for path, line in report["uncertain"]},
            )

    original = source[TRUTH_FUNCTION]
    previous_trace = sys.gettrace()
    failure = None
    try:
        with patch.dict(original.__globals__, {"FastAPIProject": Project}):
            sys.settrace(trace)
            original(fixture, *TRUTH_CASES[key])
    except (AssertionError, ValueError, OSError) as error:
        failure = str(error)
    finally:
        sys.settrace(previous_trace)
    if len(stages) != 1:
        raise ValueError("the original truth fixture did not invoke native analysis exactly once")
    outcome, fixtures = stages[0]
    report = json.loads(native_report.read_text(encoding="utf-8")) if native_report.is_file() else None
    return outcome, fixtures, {"truth": report, "assertion_error": failure}


def engine_case(args, key, case, env, trace=None):
    if key == IMPORT_ROOT_CASE:
        source_file = IMPORT_ROOT_SOURCE
        assertions = IMPORT_ROOT_ASSERTIONS
        prerequisites = IMPORT_ROOT_PREREQUISITES
        test = "phase02::analysis::tests::pinned_fastapi_import_roots"
    elif key == CASE:
        source_file, assertions, prerequisites, test = SOURCE, ASSERTIONS, PREREQUISITES, TEST
    elif key in INCLUDE_CASES:
        assertions, prerequisites = INCLUDE_CASES[key]
        source_file, test = INCLUDE_SOURCE, INCLUDE_TEST
    elif key in TRUTH_CASES:
        source_file, assertions, prerequisites, test = (
            TRUTH_SOURCE, TRUTH_ASSERTIONS, TRUTH_PREREQUISITES, TRUTH_TEST,
        )
    else:
        raise ValueError(f"missing native engine port for {key}")
    if (case["source"], case["assertions"], case["prerequisites"]) != (
        source_file, assertions, prerequisites,
    ):
        raise ValueError(f"{key}: engine port must cover the reviewed assertion and fixture mapping")
    sys.path.insert(0, str(args.upstream))
    sys.modules["skylos_fast"] = None
    source = runpy.run_path(str(args.upstream / source_file))
    argv = [
        str(args.artifact), "--ignored", "--exact", test, "--test-threads=1",
        "--format", "pretty", "--color", "never",
    ]
    with TemporaryDirectory(prefix="skarmory-pinned-engine-") as workspace:
        fixture = Path(workspace).resolve()
        details = {}
        if key in INCLUDE_CASES:
            outcome, fixtures, details = replay_include_graph(key, source, fixture, argv, env, trace)
        elif key in TRUTH_CASES:
            outcome, fixtures, details = replay_truth_chain(key, source, fixture, argv, env, trace)
        elif key == CASE:
            # Keep the source inputs and the real pytest command from the
            # upstream fixture. Rust generates its own cleanup findings.
            source["_task"](fixture)
            check = source["_passing_tests"](fixture)
            fixture_env = {
                "SKARMORY_PHASE03_CLEANUP_FIXTURE": str(fixture),
                "SKARMORY_PHASE03_CLEANUP_CHECK": check,
            }
        else:
            stage_import_roots(source, fixture)
            fixture_env = {"SKARMORY_PHASE03_IMPORT_ROOT_FIXTURE": str(fixture)}
        if key not in TRACED_ENGINE_CASES:
            fixtures = {
                path.relative_to(fixture).as_posix(): hashlib.sha256(path.read_bytes()).hexdigest()
                for path in sorted(fixture.rglob("*")) if path.is_file()
            }
            outcome = subprocess.run(
                argv, env={**env, **fixture_env},
                capture_output=True, text=True, timeout=60, check=False,
            )
    passed = passed_exact_test(outcome, test)
    if key in TRACED_ENGINE_CASES:
        passed = passed and details["assertion_error"] is None and set(assertions) <= trace.completed
    sys.stdout.write(outcome.stdout)
    sys.stderr.write(outcome.stderr)
    return {
        "status": "passed" if passed else "failed",
        "assertions": assertions if passed else [],
        "prerequisites": {
            prerequisite: "passed" if passed or key == CASE and index == 0 else "blocked"
            for index, prerequisite in enumerate(prerequisites)
        },
        "native_test": {
            "argv": argv,
            **({"check": check} if key == CASE else {}),
            "fixture_sha256": fixtures,
            **details,
            "returncode": outcome.returncode,
            "stdout": outcome.stdout,
            "stderr": outcome.stderr,
        },
    }


def main():
    parser = argparse.ArgumentParser(description="Locate the engine test binary in Cargo JSON output.")
    parser.add_argument("messages", type=Path)
    args = parser.parse_args()
    try:
        with args.messages.open(encoding="utf-8") as messages:
            print(locate_test_artifact(messages, ROOT / "crates/analysis_core/src/lib.rs"))
    except (OSError, ValueError) as error:
        parser.error(str(error))


if __name__ == "__main__":
    main()
