"""Run the native Phase 0 fixture assertions from the reviewed manifest."""

import argparse
import json
import os
from pathlib import Path
import subprocess
from tempfile import TemporaryDirectory
import tomllib


HERE = Path(__file__).resolve().parent


def scan(cli: tuple[Path | str, ...], path: str | Path, expected_exit: int) -> dict:
    result = subprocess.run([*cli, path], capture_output=True, check=False)
    assert result.returncode == expected_exit, result
    assert not result.stderr, result.stderr
    return json.loads(result.stdout)


def check(cli: tuple[Path | str, ...]) -> None:
    manifest = tomllib.loads((HERE / "phase0.toml").read_text(encoding="utf-8"))
    for case in manifest["case"]:
        label = case["id"]
        expected_exit = 0 if case["coverage"] == "complete" else 2
        report = scan(cli, HERE / "fixtures" / label, expected_exit)
        assert report["schema_version"] == manifest["schema_version"], label
        assert report["profile"] == manifest["profile"], label
        assert report["entry"] == "main", label
        coverage = report["coverage"]
        assert coverage["status"] == case["coverage"], (label, coverage)
        assert coverage["selected_files"] == case["files"], (label, coverage)
        assert coverage["extracted_files"] == case["extracted"], (label, coverage)
        assert [issue["kind"] for issue in coverage["issues"]] == (
            [case["issue"]] if "issue" in case else []
        ), (label, coverage)
        assert [item["kind"] for item in report["uncertainties"]] == (
            [case["uncertainty"]] if "uncertainty" in case else []
        ), (label, report)
        findings = report["findings"]
        assert all(f["rule"] == "unreachable_function" for f in findings), label
        assert all(f["evidence"] == "no_call_path_from_entry" for f in findings), label
        keys = ("path", "module", "symbol", "line", "column")
        actual = [[finding[key] for key in keys] for finding in findings]
        assert actual == case.get("findings", []), (label, actual)
        assert all(symbol not in actual for symbol in case.get("retained", [])), label
        print(f"{label}: pass")


def check_incomplete(cli: tuple[Path | str, ...], path: str | Path, kind: str) -> dict:
    report = scan(cli, path, 2)
    assert report["coverage"]["status"] == "incomplete", report
    assert not report["findings"], report
    issue, = report["coverage"]["issues"]
    assert issue["kind"] == kind, report
    return issue


def check_regressions(cli: tuple[Path | str, ...]) -> None:
    with TemporaryDirectory() as directory:
        root = Path(directory) / "app"
        root.mkdir()
        main = root / "main.py"
        main.write_text("from helper import boot\ndef needed():\n    pass\nboot()\n")
        (root / "__main__.py").write_text("def needed():\n    pass\n")
        helper = root / "helper.py"
        for source in (
            "import __main__ as entry\ndef boot():\n    entry.needed()\n",
            "from __main__ import needed\ndef boot():\n    needed()\n",
        ):
            helper.write_text(source)
            issue = check_incomplete(cli, root, "unsupported_import")
            assert (issue["path"], issue["line"]) == ("helper.py", 1), issue
        print("running-entry-module imports: pass")

        for prefix, column in ((b"# bad\x00\n", 6), (b'"""doc\x00string"""\n', 7)):
            main.write_bytes(prefix + b"def unused():\n    pass\n")
            issue = check_incomplete(cli, main, "parse_error")
            assert (issue["line"], issue["column"]) == (1, column), issue
        print("NUL bytes in source: pass")

        flat = Path(directory) / "flat"
        flat.mkdir()
        (flat / "main.py").write_text("pass\n")
        assets = flat / "assets.py"
        assets.mkdir()
        (assets / "ignored.py").write_text("def broken(\n")
        report = scan(cli, flat, 0)
        assert report["coverage"] == {
            "status": "complete", "selected_files": 1, "extracted_files": 1, "issues": [],
        }, report
        nameless = flat / ".py"
        nameless.write_text("pass\n")
        issue = check_incomplete(cli, flat, "unsupported_module_name")
        assert issue["path"] == ".py", issue
        nameless.unlink()
        print("flat source selection: pass")

        if os.name == "posix":
            link = Path(directory) / "link"
            link.symlink_to(flat, target_is_directory=True)
            for suffix in ("", "/", "/."):
                check_incomplete(cli, f"{link}{suffix}", "discovery_error")
            (flat / "linked.py").symlink_to(flat / "main.py")
            issue = check_incomplete(cli, flat, "unsupported_file")
            assert issue["path"] == "linked.py", issue
            print("symlink roots and sources: pass")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("cli", type=Path)
    parser.add_argument(
        "--released-cli",
        action="store_true",
        help="Run against the 0.0.1 CLI, which used the Phase 0 profile by default.",
    )
    args = parser.parse_args()
    cli = (args.cli.resolve(),) if args.released_cli else (args.cli.resolve(), "--phase0")
    check(cli)
    check_regressions(cli)
