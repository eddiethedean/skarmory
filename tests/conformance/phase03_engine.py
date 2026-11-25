"""Stage pinned fixtures and verify their exact Rust engine ports."""

import argparse
import hashlib
import json
import re
import runpy
import subprocess
import sys
from pathlib import Path
from tempfile import TemporaryDirectory
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
ENGINE_CASES = {CASE, IMPORT_ROOT_CASE}
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


def engine_case(args, key, case, env):
    if key == IMPORT_ROOT_CASE:
        source_file = IMPORT_ROOT_SOURCE
        assertions = IMPORT_ROOT_ASSERTIONS
        prerequisites = IMPORT_ROOT_PREREQUISITES
        test = "phase02::analysis::tests::pinned_fastapi_import_roots"
    elif key == CASE:
        source_file, assertions, prerequisites, test = SOURCE, ASSERTIONS, PREREQUISITES, TEST
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
        if key == CASE:
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
        fixtures = {
            path.relative_to(fixture).as_posix(): hashlib.sha256(path.read_bytes()).hexdigest()
            for path in sorted(fixture.rglob("*")) if path.is_file()
        }
        outcome = subprocess.run(
            argv, env={**env, **fixture_env},
            capture_output=True, text=True, timeout=60, check=False,
        )
    passed = passed_exact_test(outcome, test)
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
