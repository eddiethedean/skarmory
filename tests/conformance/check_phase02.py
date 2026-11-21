"""Check the declared Python compatibility fixtures and optional pinned reference."""

import argparse
import importlib.metadata
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
from tempfile import TemporaryDirectory
import tomllib

HERE = Path(__file__).resolve().parent
BASELINE = HERE / "baselines" / "skylos-5.7.0.toml"
FIXTURES = HERE / "compatibility"
CATEGORIES = (
    "unused_functions", "unused_classes", "unused_imports", "unused_variables",
    "unused_parameters", "unused_files",
)
EXPECTED = {
    "application": (
        ["main.py"],
        [
            ["unused_functions", "main.py", "main.orphan", 6, 90],
            ["unused_variables", "main.py", "main.orphan.spare", 7, 100],
        ],
    ),
    "library": (
        ["src/widget/__init__.py", "src/widget/implementation.py"],
        [
            ["unused_functions", "src/widget/implementation.py", "widget.implementation._unused", 9, 90],
        ],
    ),
    "bindings": (
        ["empty.py", "helpers.py", "main.py"],
        [
            ["unused_functions", "helpers.py", "helpers.standby", 5, 90],
            ["unused_classes", "main.py", "main._Abandoned", 5, 90],
            ["unused_imports", "main.py", "helpers.standby", 2, 100],
            ["unused_parameters", "main.py", "main._bound.ignored", 9, 100],
            ["unused_files", "empty.py", None, 1, None],
        ],
    ),
    "fastapi": (
        ["main.py"],
        [["unused_functions", "main.py", "main.dormant", 14, 90]],
    ),
    "pytest": (["test_resource.py"], []),
    "invalid": (["main.py"], []),
}


def projection(report, root):
    def relative(path):
        path = Path(path)
        return path.relative_to(root).as_posix() if path.is_absolute() else path.as_posix()

    return [
        [category, relative(row["file"]), row.get("full_name"), row["line"], row.get("confidence")]
        for category in CATEGORIES
        for row in report[category]
    ]


def environment():
    result = {key: value for key, value in os.environ.items() if not key.startswith("SKYLOS_")}
    result.update(PYTHONHASHSEED="0", RAYON_NUM_THREADS="1")
    return result


def reference_worker(backend, root):
    baseline = tomllib.loads(BASELINE.read_text(encoding="utf-8"))
    assert importlib.metadata.version("skylos") == baseline["reference"]["version"]
    if backend == "python":
        sys.modules["skylos_fast"] = None

    from skylos import analyzer
    from skylos.core.fast import FAST_AVAILABLE

    assert FAST_AVAILABLE == (backend == "skylos_fast")
    native_discovery_calls = 0
    if FAST_AVAILABLE:
        original_discover = analyzer._fast_discover
        assert original_discover is not None

        def native_discover(*args, **kwargs):
            nonlocal native_discovery_calls
            files = original_discover(*args, **kwargs)
            native_discovery_calls += 1
            return files

        analyzer._fast_discover = native_discover

    files = set()
    discover = analyzer.Skylos._get_python_files

    def observed_discovery(self, *args, **kwargs):
        paths, project = discover(self, *args, **kwargs)
        files.update(Path(path).resolve().relative_to(root).as_posix() for path in paths)
        return paths, project

    analyzer.Skylos._get_python_files = observed_discovery
    config = BASELINE.parent / baseline["environment"]["config"]
    report = json.loads(analyzer.analyze(str(root), config_file=str(config), **baseline["api_options"]))
    assert (native_discovery_calls > 0) == FAST_AVAILABLE
    print(json.dumps({
        "files": sorted(files),
        "findings": projection(report, root),
        "errors": [row["rule_id"] for row in report["analysis_errors"]],
    }))


def run(cli, root, *args, status=0):
    completed = subprocess.run(
        [cli, "--profile", "skylos", ".", "--json", *args],
        cwd=root, env=environment(), capture_output=True, text=True, check=False,
    )
    assert completed.returncode == status, (completed.args, completed.returncode, completed.stderr, completed.stdout)
    return json.loads(completed.stdout)


def check_config(cli, root):
    code = "def retained():\n    pass\n\ndef filtered():\n    pass\n\ndef visible():\n    pass\n"
    child = root / "child"
    child.mkdir()
    (child / "main.py").write_text(code, encoding="utf-8")
    (child / "hidden.py").write_text("def hidden():\n    pass\n", encoding="utf-8")
    (root / ".skylos.toml").write_text(
        'root = true\nexclude = ["hidden.py"]\n'
        '[[dead_code.entrypoints]]\nname = "retained"\nmodule = "main"\n',
        encoding="utf-8",
    )
    (child / "skylos.toml").write_text('whitelist = ["filtered"]\n', encoding="utf-8")
    (child / "pyproject.toml").write_text('[tool.skylos]\nexclude = ["main.py"]\n', encoding="utf-8")
    inherited = run(cli, child)
    assert len(inherited["config_sources"]) == 2, inherited
    assert inherited["selected_paths"] == ["main.py"], inherited
    assert [row["name"] for row in inherited["unused_functions"]] == ["visible"], inherited
    assert [row["name"] for row in inherited["whitelisted"]] == ["filtered"], inherited
    assert any(row["kind"] == "configured_entrypoint" for row in inherited["dead_code_evidence"]), inherited

    explicit = root / "explicit.toml"
    explicit.write_text("root = true\n", encoding="utf-8")
    override = run(cli, child, "--config-file", explicit)
    assert override["selected_paths"] == ["hidden.py", "main.py"], override
    assert override["config_sources"] == [explicit.as_posix()], override
    assert {row["name"] for row in override["unused_functions"]} == {
        "hidden", "retained", "filtered", "visible",
    }, override

    (root / "main.py").write_text(code, encoding="utf-8")
    (child / "__init__.py").touch()
    nested = run(cli, root)
    assert {row["file"] for row in nested["whitelisted"]} == {"child/main.py"}, nested
    assert ("main.py", "filtered") in {
        (row["file"], row["name"]) for row in nested["unused_functions"]
    }, nested
    assert "child/main.py" in nested["file_config_sources"], nested


def check(cli, reference):
    with TemporaryDirectory(prefix="skarmory-phase02-") as temporary:
        work = Path(temporary).resolve()
        for name, (expected_paths, expected_projection) in EXPECTED.items():
            root = work / name
            shutil.copytree(FIXTURES / name, root, ignore=shutil.ignore_patterns("__pycache__"))
            invalid = name == "invalid"
            report = run(cli, root, "--no-grep-verify", status=2 if invalid else 0)
            assert report["selected_paths"] == expected_paths, (name, report)
            assert report["coverage"]["status"] == ("incomplete" if invalid else "complete"), report
            assert projection(report, root) == expected_projection, (name, projection(report, root))
            assert [row["rule_id"] for row in report["analysis_errors"]] == (
                ["SKY-ANALYSIS-INCOMPLETE"] if invalid else []
            ), (name, report)
            if name == "fastapi":
                assert any(
                    row["full_name"] == "main.active" and row["kind"] == "fastapi_registration"
                    for row in report["dead_code_evidence"]
                ), report
            if name == "pytest":
                assert any(
                    row["full_name"] == "test_resource.resource" and row["kind"] == "pytest_root"
                    for row in report["dead_code_evidence"]
                ), report
            if reference:
                for backend in ("python", "skylos_fast"):
                    result = subprocess.run([
                        reference, __file__, "--reference-worker", backend, root,
                    ], cwd=root, env=environment(), text=True, capture_output=True, check=True)
                    assert json.loads(result.stdout) == {
                        "files": expected_paths,
                        "findings": expected_projection,
                        "errors": ["SKY-ANALYSIS-INCOMPLETE"] if invalid else [],
                    }, (name, backend, result.stdout)
            print(f"Phase 0.2 {name}: pass")

        root = work / "application"
        json_strict = run(cli, root, "--strict", status=1)
        assert json_strict["gate"]["exit_code"] == 1
        assert json_strict["gate"]["coverage_status"] == "complete"
        assert [row["full_name"] for row in run(cli, root, "--confidence", "90")["unused_functions"]] == ["main.orphan"]
        assert run(cli, root, "--confidence", "91")["unused_functions"] == []
        output = work / "fixture.sarif"
        run(cli, root, "--sarif", output)
        sarif = json.loads(output.read_text(encoding="utf-8"))
        assert sarif["version"] == "2.1.0"
        assert sarif["runs"][0]["invocations"][0]["executionSuccessful"]
        assert {row["ruleId"] for row in sarif["runs"][0]["results"]} == {"SKY-U001", "SKY-U003"}
        config_tree = work / "config"
        config_tree.mkdir()
        check_config(cli, config_tree)
        print("Phase 0.2 config, strict, confidence, and SARIF: pass")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("cli", type=Path, nargs="?")
    parser.add_argument("--reference-python", type=Path)
    parser.add_argument("--reference-worker", nargs=2, metavar=("BACKEND", "PATH"), help=argparse.SUPPRESS)
    args = parser.parse_args()
    if args.reference_worker:
        mode, path = args.reference_worker
        reference_worker(mode, Path(path).resolve())
    elif args.cli:
        check(args.cli.resolve(), args.reference_python.absolute() if args.reference_python else None)
    else:
        parser.error("CLI path required")
