"""Run the native Phase 0 fixture assertions from the reviewed manifest."""

import argparse
import json
from pathlib import Path
import subprocess
import tomllib


HERE = Path(__file__).resolve().parent


def check(cli: Path) -> None:
    manifest = tomllib.loads((HERE / "phase0.toml").read_text(encoding="utf-8"))
    for case in manifest["case"]:
        result = subprocess.run(
            [cli, HERE / "fixtures" / case["id"]],
            capture_output=True,
            check=False,
        )
        label = case["id"]
        expected_exit = 0 if case["coverage"] == "complete" else 2
        assert result.returncode == expected_exit, (label, result)
        assert not result.stderr, (label, result.stderr)
        report = json.loads(result.stdout)
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


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("cli", type=Path)
    check(parser.parse_args().cli.resolve())
