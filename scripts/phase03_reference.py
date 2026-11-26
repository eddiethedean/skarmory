#!/usr/bin/env python3
"""Materialize the reviewed Phase 0.3 fixture corrections on pinned Skylos."""

import argparse
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import re
import subprocess
from tempfile import TemporaryDirectory
import tomllib


BASELINE = (
    Path(__file__).resolve().parents[1]
    / "tests/conformance/baselines/skylos-5.7.0.toml"
)
CORPUS = "corpus/manifest.json"
COMMIT_ENV = {
    "GIT_AUTHOR_NAME": "Skarmory",
    "GIT_AUTHOR_EMAIL": "skarmory-fixtures@example.invalid",
    "GIT_AUTHOR_DATE": "2026-11-19T00:00:00+00:00",
    "GIT_COMMITTER_NAME": "Skarmory",
    "GIT_COMMITTER_EMAIL": "skarmory-fixtures@example.invalid",
    "GIT_COMMITTER_DATE": "2026-11-19T00:00:00+00:00",
}
COMMIT_MESSAGE = b"Apply the reviewed Skarmory Phase 0.3 corpus fixture corrections\n"


def phase03_revisions(baseline):
    original = baseline["reference"]["git_revision"]
    selected = baseline.get("phase03_reference", {}).get("git_revision", original)
    if any(not isinstance(rev, str) or not re.fullmatch(r"[0-9a-f]{40}", rev)
           for rev in (original, selected)):
        raise ValueError("Skylos baseline revisions must be full lowercase Git SHAs")
    return original, selected


def git(checkout, *args, env=None, data=None):
    result = subprocess.run(
        ["git", "-C", str(checkout), *args],
        input=data, env=env, capture_output=True, check=False,
    )
    if result.returncode:
        raise ValueError(f"git {args[0]} failed: {result.stderr.decode(errors='replace').strip()}")
    return result.stdout


def fixture_path(case):
    path = PurePosixPath(case["path"])
    if path.parts[:1] != ("fixtures",) or len(path.parts) < 2 or ".." in path.parts:
        raise ValueError(f"{case['id']}: expected a corpus fixture path")
    return str(PurePosixPath("corpus") / path)


def validate_corpus_changes(before, after, paths, reviewed):
    old_cases = before.pop("cases")
    new_cases = after.pop("cases")
    if before != after or [case["id"] for case in old_cases] != [case["id"] for case in new_cases]:
        raise ValueError("fixture corrections changed the corpus manifest or case IDs")
    fixture_changes = paths - {CORPUS}
    allowed = set()
    observed = set()
    for old, new in zip(old_cases, new_cases, strict=True):
        case_id = old["id"]
        if case_id not in reviewed:
            if old != new:
                raise ValueError(f"{case_id}: unreviewed corpus manifest change")
            continue
        if {**old, "path": new["path"]} != new:
            raise ValueError(f"{case_id}: correction changed more than the fixture path")
        roots = (fixture_path(old), fixture_path(new))
        matched = {
            path for path in fixture_changes
            if any(path == root or path.startswith(root + "/") for root in roots)
        }
        if matched:
            observed.add(case_id)
        allowed.update(matched)
    if allowed != fixture_changes or observed != reviewed:
        raise ValueError("fixture changes do not match the reviewed corpus cases")


def materialize(checkout, manifest=BASELINE):
    baseline = tomllib.loads(manifest.read_text(encoding="utf-8"))
    original, selected = phase03_revisions(baseline)
    corrections = baseline["phase03_reference"]
    reviewed = {case["id"] for case in corrections["corrections"]}
    if not reviewed or len(reviewed) != len(corrections["corrections"]):
        raise ValueError("expected unique reviewed fixture correction IDs")
    patch = manifest.with_name(corrections["patch"]).read_bytes()
    if hashlib.sha256(patch).hexdigest() != corrections["patch_sha256"]:
        raise ValueError("Phase 0.3 fixture correction patch SHA-256 mismatch")
    if git(checkout, "rev-parse", "HEAD").decode().strip() != original:
        raise ValueError("fixture corrections require the frozen Skylos checkout")
    if git(checkout, "status", "--porcelain", "--untracked-files=all"):
        raise ValueError("fixture corrections require a clean Skylos checkout")

    # Validate the patched tree in a throwaway index before changing the checkout.
    with TemporaryDirectory(prefix="skarmory-phase03-reference-") as temporary:
        env = {**os.environ, **COMMIT_ENV, "GIT_INDEX_FILE": str(Path(temporary) / "index")}
        git(checkout, "read-tree", original, env=env)
        git(checkout, "apply", "--cached", "-", env=env, data=patch)
        changed = git(checkout, "diff", "--cached", "--name-only", "--no-renames", "-z", env=env)
        paths = set(changed.decode().rstrip("\0").split("\0"))
        before = json.loads(git(checkout, "show", f"{original}:{CORPUS}"))
        after = json.loads(git(checkout, "show", f":{CORPUS}", env=env))
        validate_corpus_changes(before, after, paths, reviewed)
        for path in paths:
            entry = git(checkout, "ls-files", "--stage", "--", path, env=env)
            if entry and not entry.startswith(b"100644 "):
                raise ValueError(f"{path}: fixture corrections must be ordinary files")
        tree = git(checkout, "write-tree", env=env).decode().strip()
        actual = git(checkout, "-c", "commit.gpgSign=false", "commit-tree", tree, "-p", original,
                     env=env, data=COMMIT_MESSAGE).decode().strip()
        if actual != selected:
            raise ValueError(f"derived Phase 0.3 checkout is {actual}, expected {selected}")
    git(checkout, "switch", "--detach", selected)
    return selected


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("checkout", type=Path)
    args = parser.parse_args()
    try:
        print(materialize(args.checkout))
    except (OSError, ValueError) as error:
        parser.exit(1, f"{error}\n")


if __name__ == "__main__":
    main()
