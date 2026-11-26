#!/usr/bin/env python3
"""Materialize the reviewed Phase 0.3 fixture corrections on pinned Skylos."""

import argparse
import ast
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
COMMIT_MESSAGE = b"Apply the reviewed Skarmory Phase 0.3 fixture corrections\n"


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


def inline_fixtures(source, rules):
    tree = ast.parse(source)
    ids = [remove_parametrized_fixture(tree, rule) for rule in rules]
    assertions = [node.lineno for node in ast.walk(tree) if isinstance(node, ast.Assert)]
    return ast.dump(tree), assertions, ids


def remove_parametrized_fixture(tree, rule):
    [function] = [
        node for node in tree.body
        if isinstance(node, ast.FunctionDef) and node.name == rule["function"]
    ]
    [decorator] = [
        node for node in function.decorator_list
        if isinstance(node, ast.Call)
        and ast.unparse(node.func) == "pytest.mark.parametrize"
        and len(node.args) == 2
        and isinstance(node.args[0], ast.Constant)
        and node.args[0].value == rule["parameter"]
    ]
    values = ast.literal_eval(decorator.args[1])
    if not isinstance(values, (list, tuple)) or not all(isinstance(value, str) for value in values):
        raise ValueError("inline fixture correction must use literal string inputs")
    ids = values
    if decorator.keywords:
        [keyword] = decorator.keywords
        if keyword.arg != "ids":
            raise ValueError("inline fixture correction may only specify explicit pytest IDs")
        ids = ast.literal_eval(keyword.value)
    if (not isinstance(ids, (list, tuple)) or len(ids) != len(values)
            or not all(isinstance(case_id, str) for case_id in ids)):
        raise ValueError("inline fixture correction must name every input")
    function.decorator_list.remove(decorator)
    return list(ids)


def validate_inline_fixtures(before, after, rules):
    if inline_fixtures(before, rules) != inline_fixtures(after, rules):
        raise ValueError(f"{rules[0]['path']}: correction changed test code, assertions, or pytest IDs")


def clear_reviewed_parameter_strings(test, columns):
    [decorator] = [
        node for node in test.decorator_list
        if isinstance(node, ast.Call)
        and ast.unparse(node.func) == "pytest.mark.parametrize"
        and len(node.args) == 2
        and isinstance(node.args[0], ast.Constant)
        and isinstance(node.args[0].value, str)
    ]
    names = [name.strip() for name in decorator.args[0].value.split(",")]
    [keyword] = [keyword for keyword in decorator.keywords if keyword.arg == "ids"]
    ids = ast.literal_eval(keyword.value)
    rows = decorator.args[1]
    if (not set(columns) <= set(names) or not isinstance(rows, (ast.List, ast.Tuple))
            or not isinstance(ids, (list, tuple)) or len(ids) != len(rows.elts)
            or not all(isinstance(case_id, str) for case_id in ids)):
        raise ValueError(f"{test.name}: fixture columns require explicit pytest IDs")
    for row in rows.elts:
        if not isinstance(row, (ast.List, ast.Tuple)) or len(row.elts) != len(names):
            raise ValueError(f"{test.name}: fixture input rows do not match their named columns")
        for index, argument in enumerate(row.elts):
            if names[index] in columns and isinstance(argument, ast.Constant) and isinstance(argument.value, str):
                argument.value = ""


def test_contract(source, fixture_columns):
    tree = ast.parse(source)
    assertions = [
        (node.lineno, ast.dump(node))
        for node in ast.walk(tree) if isinstance(node, ast.Assert)
    ]
    tests = []
    for node in tree.body:
        if not isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)) or not node.name.startswith("test_"):
            continue
        if node.name in fixture_columns:
            clear_reviewed_parameter_strings(node, fixture_columns[node.name])
        tests.append((node.name, node.lineno, ast.dump(node.args), tuple(map(ast.dump, node.decorator_list))))
    return assertions, tests


def validate_fixture_setup(before, after, rule):
    old_lines, new_lines = before.splitlines(keepends=True), after.splitlines(keepends=True)
    changed_lines = {
        line for line, (old, new) in enumerate(zip(old_lines, new_lines), 1) if old != new
    }
    reviewed_lines = rule["lines"]
    if (len(old_lines) != len(new_lines) or not reviewed_lines
            or len(set(reviewed_lines)) != len(reviewed_lines)
            or changed_lines != set(reviewed_lines)):
        raise ValueError(f"{rule['path']}: fixture edit differs from the reviewed lines")
    fixture_columns = rule.get("parametrized_fixture_columns", {})
    if test_contract(before, fixture_columns) != test_contract(after, fixture_columns):
        raise ValueError(f"{rule['path']}: fixture setup changed assertions or test collection")


def validate_combined_fixtures(before, after, inline, setup):
    old_lines, new_lines = before.splitlines(keepends=True), after.splitlines(keepends=True)
    if len(old_lines) != len(new_lines):
        raise ValueError(f"{setup['path']}: combined fixture edits must preserve line positions")
    setup_lines = set(setup["lines"])
    separator = b"" if isinstance(before, bytes) else ""
    with_only_inline = separator.join(
        old if line in setup_lines else new
        for line, (old, new) in enumerate(zip(old_lines, new_lines), 1)
    )
    validate_inline_fixtures(before, with_only_inline, inline)
    validate_fixture_setup(with_only_inline, after, setup)


def checked_test_fixture(checkout, original, env, paths, path):
    fixture = PurePosixPath(path)
    if (path not in paths or fixture.parts[:1] != ("test",)
            or ".." in fixture.parts or fixture.suffix != ".py"):
        raise ValueError(f"{path}: missing or invalid inline fixture correction")
    paths.remove(path)
    return git(checkout, "show", f"{original}:{path}"), git(checkout, "show", f":{path}", env=env)


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
        corpus_paths = paths.copy()
        before = json.loads(git(checkout, "show", f"{original}:{CORPUS}"))
        after = json.loads(git(checkout, "show", f":{CORPUS}", env=env))
        inline = {}
        for rule in corrections.get("inline_corrections", ()):
            inline.setdefault(rule["path"], []).append(rule)
        setups = corrections.get("fixture_setup_corrections", ())
        setup_paths = {rule["path"] for rule in setups}
        if len(setup_paths) != len(setups):
            raise ValueError("expected unique fixture setup paths")
        for path, rules in inline.items():
            if path not in setup_paths:
                old, new = checked_test_fixture(checkout, original, env, corpus_paths, path)
                validate_inline_fixtures(old, new, rules)
        for rule in setups:
            path = rule["path"]
            old, new = checked_test_fixture(checkout, original, env, corpus_paths, path)
            if path in inline:
                validate_combined_fixtures(old, new, inline[path], rule)
            else:
                validate_fixture_setup(old, new, rule)
        validate_corpus_changes(before, after, corpus_paths, reviewed)
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
