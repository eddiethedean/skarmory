"""Verify Phase 0.1 findings, coverage, and worker equivalence."""

import argparse
import json
import os
from pathlib import Path
import subprocess
from tempfile import TemporaryDirectory
import tomllib


HERE = Path(__file__).resolve().parent
MANIFEST = tomllib.loads((HERE / "phase01.toml").read_text(encoding="utf-8"))


def scan(cli, project, args, coverage, workers=1):
    result = subprocess.run(
        [cli, project, *args, "--workers", str(workers)], capture_output=True
    )
    assert result.returncode == (0 if coverage == "complete" else 2), result
    assert not result.stderr, result.stderr
    report = json.loads(result.stdout)
    assert (report["schema_version"], report["profile"]) == (
        MANIFEST["schema_version"], MANIFEST["profile"]
    ), report
    assert report["coverage"]["status"] == coverage, report
    assert report["coverage"]["selected_files"] == len(report["selected_paths"]), report
    if coverage == "complete":
        assert not report["coverage"]["issues"], report
        assert report["coverage"]["extracted_files"] == len(report["selected_paths"]), report
    else:
        assert report["coverage"]["issues"], report
        assert not report["findings"], report
    return report


def symbol_key(symbol_id):
    path, chain = symbol_id.split("::", 1)
    *ancestors, name = (part.split("@", 1)[0] for part in chain.rstrip("/").split("/"))
    return path, ".".join(ancestors), name


def check_case(cli, case):
    project = HERE / case["fixture"]
    args = case["args"]
    report = scan(cli, project, args, case["coverage"])
    for workers in (4, 2, 4):
        assert scan(cli, project, args, case["coverage"], workers=workers) == report

    findings = report["findings"]
    actual = sorted((item["path"], item["scope"], item["symbol"]) for item in findings)
    assert actual == sorted(map(tuple, case.get("findings", []))), (actual, report)
    for finding in findings:
        assert finding["rule"] == "unreachable_function", finding
        assert finding["evidence"] == "no_call_path_from_entry", finding
        assert finding["line"] > 0 and finding["column"] > 0, finding
        assert symbol_key(finding["symbol_id"]) == (
            finding["path"], finding["scope"], finding["symbol"]
        ), finding
    assert len({item["symbol_id"] for item in findings}) == len(findings), findings

    for path, scope, symbol, certainty, *kind in case.get("retained", []):
        assert any(
            symbol_key(item["symbol_id"]) == (path, scope, symbol)
            and item["certainty"] == certainty
            and (not kind or item["kind"] == kind[0])
            for item in report["evidence"]
        ), ((path, scope, symbol, certainty, kind), report)
    if case.get("only_possible_exports"):
        assert all(
            item["certainty"] == "possible"
            for item in report["evidence"] if item["kind"] == "public_export"
        ), report
    if "paths" in case:
        assert report["selected_paths"] == case["paths"], report
    if "import_roots" in case:
        assert report["import_roots"] == case["import_roots"], report
    if "roots" in case:
        assert [[item["kind"], item["target"]] for item in report["roots"]] == case["roots"]
    if "uncertainty" in case:
        assert any(item["kind"] == case["uncertainty"] for item in report["uncertainties"]), report
    if "issue_path" in case:
        assert any(
            item["path"] == case["issue_path"]
            and ("issue" not in case or item["kind"] == case["issue"])
            and ("issue_line" not in case or item.get("line") == case["issue_line"])
            for item in report["coverage"]["issues"]
        ), report
    return report


def check_file_order(cli, case, expected):
    original = HERE / case["fixture"]
    relative = sorted(path.relative_to(original) for path in original.rglob("*.py"))
    with TemporaryDirectory() as directory:
        base = Path(directory)
        for order, files in (("forward", relative), ("reverse", reversed(relative))):
            project = base / order
            for index, file in enumerate(files):
                target = project / file
                target.parent.mkdir(parents=True, exist_ok=True)
                source = (original / file).read_bytes()
                # Change which file costs more to read and parse without
                # moving any analyzed code or source locations.
                if index == 0:
                    source += b"\n# retained snapshot padding\n" * 2048
                target.write_bytes(source)
            for workers in (1, 4):
                assert scan(cli, project, case["args"], "complete", workers) == expected


def check_platform_import_precedence(cli):
    with TemporaryDirectory() as directory:
        project = Path(directory)
        (project / "main.py").write_text("from math import local\nlocal()\n", encoding="utf-8")
        (project / "math.py").write_text(
            "def local(): pass\ndef unused(): pass\n", encoding="utf-8"
        )
        # The standard CPython Windows build links math into the interpreter.
        if os.name == "nt":
            report = scan(cli, project, [], "incomplete")
            issue, = report["coverage"]["issues"]
            assert (issue["kind"], issue["path"]) == ("unresolved_import", "main.py"), report
        else:
            report = scan(cli, project, [], "complete")
            assert [(item["path"], item["symbol"]) for item in report["findings"]] == [
                ("math.py", "unused")
            ], report


def check_limits_and_selection(cli):
    with TemporaryDirectory() as directory:
        project = Path(directory) / "project"
        project.mkdir()
        main = project / "main.py"
        main.write_text(
            "".join(f"def f{i}():\n    f{i + 1}()\n\n" for i in range(64))
            + "def f64():\n    pass\n\nf0()\n", encoding="utf-8"
        )
        report = scan(cli, project, [], "incomplete")
        assert any(item["kind"] == "analysis_limit" for item in report["coverage"]["issues"])
        assert scan(cli, project, [], "incomplete", workers=4) == report

        def nested_lists(depth):
            return (
                "def live(): pass\nx = None\n"
                + "x = [x]\n" * depth
                + "if x == x:\n    live()\n"
            )

        main.write_text(nested_lists(16), encoding="utf-8")
        assert not scan(cli, project, [], "complete")["findings"]
        main.write_text(nested_lists(40_000), encoding="utf-8")
        report = scan(cli, project, [], "incomplete")
        issue, = report["coverage"]["issues"]
        assert issue["kind"] == "analysis_limit" and "value nesting" in issue["message"], report

        main.write_text("pass\n", encoding="utf-8")
        namespace = project / "namespace"
        namespace.mkdir()
        child = namespace / "standalone.py"
        child.write_text("pass\n", encoding="utf-8")
        report = scan(cli, project, [], "incomplete")
        assert any(
            item["kind"] == "unsupported_namespace_package"
            and item["path"] == "namespace/standalone.py"
            for item in report["coverage"]["issues"]
        ), report
        child.unlink()
        namespace.rmdir()

        if os.name == "posix":
            outside = project.parent / "outside"
            outside.mkdir()
            (outside / "invalid.py").write_text("def broken(\n", encoding="utf-8")
            (project / "linked_dir").symlink_to(outside, target_is_directory=True)
            report = scan(cli, project, [], "complete")
            assert report["selected_paths"] == ["main.py"], report
            assert any(item["path"] == "linked_dir" for item in report["exclusions"]), report
            linked_file = project / "linked.py"
            linked_file.symlink_to(main)
            report = scan(cli, project, [], "incomplete")
            assert any(
                item["kind"] == "unsupported_file" and item["path"] == "linked.py"
                for item in report["coverage"]["issues"]
            ), report


def check(cli):
    for case in MANIFEST["case"]:
        try:
            report = check_case(cli, case)
            if case["id"] == "packaged-module":
                check_file_order(cli, case, report)
        except AssertionError as error:
            raise AssertionError(f"{case['id']}: {error}") from error
        print(f"{case['id']}: pass")
    check_platform_import_precedence(cli)
    print("platform import precedence: pass")
    check_limits_and_selection(cli)
    print("analysis bounds and source selection: pass")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("cli", type=Path)
    check(parser.parse_args().cli.resolve())
