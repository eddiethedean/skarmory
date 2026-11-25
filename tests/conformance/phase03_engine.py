"""Stage the pinned cleanup fixture and verify the exact Rust engine port."""

import argparse
import hashlib
import json
import re
import runpy
import subprocess
import sys
from pathlib import Path
from tempfile import TemporaryDirectory

SOURCE = "test/test_cleanup_execution.py"
CASE = f"{SOURCE}::test_source_removed_after_rescan_preserves_the_incomplete_receipt"
ASSERTIONS = [f"{SOURCE}:{line}" for line in range(387, 392)]
PREREQUISITES = [f"{SOURCE}:372", "native-core-cleanup-removal"]
TEST = "phase02::cleanup::tests::pinned_source_removed_after_comparison_preserves_receipts"
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


def passed_exact_test(completed):
    return (
        completed.returncode == 0 and not completed.stderr.strip()
        and re.fullmatch(
            rf"running 1 test\ntest {re.escape(TEST)} \.\.\. ok\n\n"
            r"test result: ok\. 1 passed; 0 failed; 0 ignored; 0 measured; "
            r"\d+ filtered out; finished in \d+(?:\.\d+)?s",
            completed.stdout.strip(),
        ) is not None
    )


def engine_case(args, case, env):
    if case["source"] != SOURCE or case["assertions"] != ASSERTIONS or case["prerequisites"] != PREREQUISITES:
        raise ValueError("engine cleanup port must cover the reviewed assertion and fixture mapping")
    sys.path.insert(0, str(args.upstream))
    sys.modules["skylos_fast"] = None
    source = runpy.run_path(str(args.upstream / SOURCE))
    argv = [
        str(args.artifact), "--ignored", "--exact", TEST, "--test-threads=1",
        "--format", "pretty", "--color", "never",
    ]
    with TemporaryDirectory(prefix="skarmory-pinned-cleanup-engine-") as workspace:
        fixture = Path(workspace).resolve()
        # The upstream task helper creates app.py; its synthetic findings are
        # not consumed by the candidate. The real pytest command is retained.
        source["_task"](fixture)
        check = source["_passing_tests"](fixture)
        fixtures = {
            name: hashlib.sha256((fixture / name).read_bytes()).hexdigest()
            for name in ("app.py", "test_app.py")
        }
        outcome = subprocess.run(
            argv, env={
                **env,
                "SKARMORY_PHASE03_CLEANUP_FIXTURE": str(fixture),
                "SKARMORY_PHASE03_CLEANUP_CHECK": check,
            },
            capture_output=True, text=True, timeout=60, check=False,
        )
    passed = passed_exact_test(outcome)
    sys.stdout.write(outcome.stdout)
    sys.stderr.write(outcome.stderr)
    return {
        "status": "passed" if passed else "failed",
        "assertions": ASSERTIONS if passed else [],
        "prerequisites": {
            PREREQUISITES[0]: "passed",
            PREREQUISITES[1]: "passed" if passed else "blocked",
        },
        "native_test": {
            "argv": argv,
            "check": check,
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
