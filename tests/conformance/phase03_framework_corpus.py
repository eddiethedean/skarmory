"""Replay the pinned framework corpus scorer against the installed SDK."""

import argparse
import ast
import json
import os
from pathlib import Path
import subprocess
import sys
from unittest.mock import patch

from check_phase03_framework import SDK_SCAN, check_checkout, environment
from phase03_runtime_corpus import CONTROLS, evaluate_runtime_control


MANIFEST = "corpus/manifest.json"
FRAMEWORK_PREFIXES = (
    "celery-",
    "django-",
    "drf-",
    "fastapi-",
    "flask-",
    "marshmallow-",
    "pydantic-",
    "pytest-",
    "sqlalchemy-",
)
OTHER_FRAMEWORK_CASES = {
    "aiohttp-routes",
    "attrs-define-fields",
    "click-command-subclass",
    "gunicorn-config",
    "pluggy-hookimpl",
    "python-decorator-factory-registration",
    "python-src-package-routes",
    "python-unittest-lifecycle",
    "python-wrapper-decorator-registration",
    "sanic-register-listener",
    "starlette-add-route",
    "typer-callback",
}


class MethodOccurrences(ast.NodeVisitor):
    """Index the class-relative names of methods by their declaration lines."""

    def __init__(self):
        self.parents = []
        self.methods = {}

    def visit_ClassDef(self, node):
        self.visit_scope(node)

    def visit_FunctionDef(self, node):
        if self.parents and isinstance(self.parents[-1], ast.ClassDef):
            names = [parent.name for parent in self.parents]
            self.methods[(node.lineno, node.name)] = ".".join([*names, node.name])
        self.visit_scope(node)

    visit_AsyncFunctionDef = visit_FunctionDef

    def visit_scope(self, node):
        self.parents.append(node)
        self.generic_visit(node)
        self.parents.pop()


def project_method_names(report, scanned_path):
    """Expose Skylos's class-relative method name while preserving the full identity."""
    scanned_path = Path(scanned_path).resolve()
    root = scanned_path if scanned_path.is_dir() else scanned_path.parent
    definitions = {
        (row["file"], row["line"], row["column"], row["name"])
        for row in report["definitions"].values()
        if row["type"] == "function"
    }
    methods_by_path = {}
    projected = []
    for row in report["unused_functions"]:
        occurrence = (row["file"], row["line"], row["column"], row["full_name"])
        if occurrence in definitions:
            source = (root / row["file"]).resolve()
            if source.is_file() and source.is_relative_to(root):
                if source not in methods_by_path:
                    occurrences = MethodOccurrences()
                    occurrences.visit(ast.parse(source.read_bytes(), filename=str(source)))
                    methods_by_path[source] = occurrences.methods
                name = methods_by_path[source].get((row["line"], row["simple_name"]))
                if name and row["full_name"].endswith("." + name):
                    row = {**row, "name": name}
        projected.append(row)
    return {**report, "unused_functions": projected}


def framework_cases(manifest):
    return {
        case["id"]
        for case in manifest["cases"]
        if case["id"].startswith(FRAMEWORK_PREFIXES)
        or case["id"] in OTHER_FRAMEWORK_CASES
    }


def corpus_assertions(case):
    return [
        f"{mode}:{category}:{symbol if isinstance(symbol, str) else json.dumps(symbol, sort_keys=True)}"
        for mode in ("absent", "present")
        for category, symbols in case["expect"].get(mode, {}).items()
        for symbol in symbols
    ]


def score_corpus(corpus_ci, manifest, selected, scan=None):
    if scan is None:
        return corpus_ci.run_manifest(manifest, selected_cases=selected)

    def analyze(path, **options):
        # These corpus cases exercise the dead-code analyzer only.
        for flag in ("enable_ai_defects", "enable_danger", "enable_sca", "enable_secrets"):
            if options.pop(flag):
                raise ValueError(f"The framework corpus requires {flag} to be disabled")
        return json.dumps(project_method_names(scan(path, options), path))

    with patch.object(corpus_ci, "analyze", analyze):
        return corpus_ci.run_manifest(manifest, selected_cases=selected)


def check_runtime_corpus(corpus_ci, manifest, selected, scan):
    selected = set(selected or CONTROLS)
    if unexpected := selected - CONTROLS.keys():
        raise ValueError(f"unknown runtime corpus controls: {', '.join(sorted(unexpected))}")
    results = []
    for case_id in sorted(selected):
        reports = []

        def capture(path, options):
            root = Path(path).resolve()
            paths = sorted(
                file.relative_to(root).as_posix() for file in root.rglob("*.py")
            ) if root.is_dir() else [root.name]
            report = scan(path, options)
            reports.append((report, paths))
            return report

        # The frozen scorer stages the original fixture. The runtime
        # expectations examine the raw scan independently of its verdict.
        original = score_corpus(corpus_ci, manifest, {case_id}, capture)
        if len(reports) != 1 or [row["id"] for row in original["cases"]] != [case_id]:
            raise ValueError(f"{case_id}: expected exactly one installed SDK scan")
        (report, paths), = reports
        failures = evaluate_runtime_control(report, paths, CONTROLS[case_id])
        results.append({
            "id": case_id,
            "status": "failed" if failures else "passed",
            "coverage": report["coverage"],
            "failures": failures,
        })
    return {
        "suite": "reviewed-runtime-controls",
        "case_count": len(results),
        "pass_count": sum(case["status"] == "passed" for case in results),
        "cases": results,
    }


def run(upstream, sdk_python, selected, backend, *, runtime_controls=False):
    check_checkout(upstream, "corpus", frozen_only=runtime_controls)
    sys.dont_write_bytecode = True
    sys.path.insert(0, str(upstream))
    if backend == "python":
        sys.modules["skylos_fast"] = None
    from skylos.benchmarks import corpus_ci
    from skylos.core.fast import FAST_AVAILABLE

    if FAST_AVAILABLE != (backend == "skylos_fast"):
        raise RuntimeError(f"Pinned reference backend {backend} is unavailable")
    env = environment()
    os.environ.clear()
    os.environ.update(env)
    manifest = upstream / MANIFEST

    def scan(path, options):
        candidate = Path(path).resolve()
        completed = subprocess.run(
            [sdk_python, "-I", "-c", SDK_SCAN],
            cwd=candidate if candidate.is_dir() else candidate.parent,
            env=env,
            input=json.dumps([str(candidate), options]),
            text=True,
            capture_output=True,
            timeout=60,
            check=True,
        )
        return json.loads(completed.stdout)

    if runtime_controls:
        if sdk_python is None:
            raise ValueError("runtime corpus controls require the installed SDK")
        return check_runtime_corpus(corpus_ci, manifest, selected, scan)

    return score_corpus(
        corpus_ci,
        manifest,
        set(selected) if selected else framework_cases(corpus_ci.load_manifest(manifest)),
        scan if sdk_python else None,
    )


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("upstream", type=Path)
    parser.add_argument("--sdk-python", type=Path)
    parser.add_argument("--backend", choices=("python", "skylos_fast"), default="python")
    parser.add_argument("--case", action="append")
    parser.add_argument(
        "--runtime-controls", action="store_true",
        help="check the separate runtime-derived expectations with the installed SDK",
    )
    args = parser.parse_args()

    report = run(
        args.upstream.resolve(),
        args.sdk_python.absolute() if args.sdk_python else None,
        args.case,
        args.backend,
        runtime_controls=args.runtime_controls,
    )
    print(json.dumps(report, indent=2))
    if args.runtime_controls:
        return int(report["pass_count"] != report["case_count"])
    return int(bool(report["failure_count"] or report["unscored_case_count"]))


if __name__ == "__main__":
    raise SystemExit(main())
