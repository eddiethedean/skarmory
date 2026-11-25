"""Run pinned pytest and benchmark cases against Skylos and candidate artifacts."""

import argparse
import ast
import hashlib
import json
import os
import platform
import subprocess
import sys
from contextlib import ExitStack
from pathlib import Path
from tempfile import TemporaryDirectory
from types import SimpleNamespace

from phase03_cli import CASE as CLI_CASE
from phase03_cli import CLEANUP_CASES, WORKFLOW_SOURCES, CleanupCaseCli, RecursiveCaseCli, WorkflowCaseCli
from phase03_cleanup_execution import CLEANUP_EXECUTION_CASES, CleanupExecutionCli
from phase03_dependencies import (
    DECLARATION_CASE,
    DECLARATION_PREREQUISITES,
    UNUSED_DEPENDENCY_PREREQUISITE,
    UNUSED_DEPENDENCY_SOURCES,
    dependency_declarations,
    unused_dependencies,
)
from phase03_engine import ENGINE_CASES, INCLUDE_CASES, INCLUDE_SOURCE, engine_case
from phase03_framework import framework_report, patch_framework_analysis
from phase03_fastapi import CASES as FASTAPI_CASES
from phase03_fastapi import (
    IMPORT_SCOPE_CASE,
    IMPORT_SCOPE_PREREQUISITES,
    LIVE_ROUTER,
    import_scope_prerequisites,
    import_scope_report,
    prove_live_router,
    verify_wheels,
)
from phase03_framework_corpus import MANIFEST as CORPUS_SOURCE
from phase03_framework_corpus import corpus_assertions, framework_cases, score_corpus

PORT = "tests/conformance/run_phase03_seed.py"
BENCHMARK_SOURCE = "benchmarks/dead_code/manifest.json"
BENCHMARK_ID = "basic-unused-symbols"
BENCHMARK_CASE = f"{BENCHMARK_SOURCE}::{BENCHMARK_ID}"
ROOT = Path(__file__).resolve().parents[2]
SCRIPT = Path(__file__).resolve()
SOURCE_EDIT_CACHE = "installed-sdk-source-edit-cache-reuse"


# Run in an isolated interpreter so the candidate never imports the Skylos
# checkout or the Skarmory source tree. Check the installed package against the
# exact wheel before letting the original assertions exercise its SDK.
CHECK_WHEEL = """
from pathlib import Path, PurePosixPath
import platform
import sys
from zipfile import ZipFile

expected = sys.argv[2]
observed = f"{sys.platform}-{platform.machine()}-cp{sys.version_info.major}{sys.version_info.minor}"
implementation = platform.python_implementation()
if implementation != "CPython" or observed != expected:
    raise SystemExit(f"Required candidate SDK platform {expected}, found {implementation} {observed}")

import skarmory
from skarmory import _native

root = Path(skarmory.__file__).resolve().parent.parent
native = Path(_native.__file__).resolve().relative_to(root).as_posix()
with ZipFile(sys.argv[1]) as wheel:
    files = [
        info for info in wheel.infolist()
        if info.filename.startswith("skarmory/") and not info.is_dir()
    ]
    if native not in {info.filename for info in files}:
        raise SystemExit("Installed native extension is absent from the candidate wheel")
    for info in files:
        path = root.joinpath(*PurePosixPath(info.filename).parts)
        if path.read_bytes() != wheel.read(info):
            raise SystemExit(f"Installed package differs from the wheel: {info.filename}")
"""
SCAN = """
import json
import sys
from skarmory.compat.skylos import analyze
path, options = json.load(sys.stdin)
print(analyze(path, **options))
"""


def environment():
    env = {
        name: value for name, value in os.environ.items()
        if not name.startswith(("SKYLOS_", "PYTEST_", "PYTHON"))
    }
    env.update(PYTEST_DISABLE_PLUGIN_AUTOLOAD="1", PYTHONHASHSEED="0")
    return env


def check_candidate_wheel(sdk_python, artifact, expected_platform, cwd, env):
    result = subprocess.run(
        [str(sdk_python), "-I", "-c", CHECK_WHEEL, str(artifact), expected_platform],
        cwd=cwd, env=env, text=True, capture_output=True, timeout=60, check=False,
    )
    if result.returncode:
        raise ValueError(result.stderr.strip() or "candidate SDK wheel validation failed")


def scan_candidate(sdk_python, path, options):
    def json_value(value):
        if isinstance(value, os.PathLike):
            return os.fspath(value)
        if isinstance(value, dict):
            return {key: json_value(item) for key, item in value.items()}
        if isinstance(value, (list, tuple)):
            return [json_value(item) for item in value]
        return value

    return subprocess.run(
        [str(sdk_python), "-I", "-c", SCAN],
        input=json.dumps(json_value([path, {**options, "profile": "skylos-0.3"}])),
        cwd=Path(path) if Path(path).is_dir() else Path(path).parent,
        env=environment(), text=True, capture_output=True, timeout=60,
        check=True,
    ).stdout


def port_cases(args):
    from check_dead_code_gate import read_inventory

    inventory, digest, variants, expected = read_inventory(args.inventory)
    if args.variant not in expected:
        raise ValueError(f"not a required Phase 0.3 variant: {args.variant}")
    variant = variants[args.variant]
    layer = variant["layer"]
    if layer not in {"sdk", "cli", "engine"} or variant["backend"] != "python":
        raise ValueError("the seed runner requires a Python backend SDK, CLI, or engine variant")
    cases = {key: case for key, case in expected[args.variant].items() if case["port"] == PORT}
    if not cases:
        raise ValueError(f"no seed port mappings for {args.variant}")
    if (layer == "engine" and not cases.keys() <= ENGINE_CASES) or (
        layer != "engine" and cases.keys() & ENGINE_CASES
    ):
        raise ValueError("a mapped engine case requires a registered native port and engine variant")
    if layer == "cli" and any(
        key != CLI_CASE and key not in CLEANUP_CASES and key not in CLEANUP_EXECUTION_CASES
        and case["source"] not in WORKFLOW_SOURCES
        for key, case in cases.items()
    ):
        raise ValueError("missing native CLI adapter for a mapped case")
    if layer == "sdk" and args.sdk_python is None:
        raise ValueError("--sdk-python is required for the SDK variant")
    if any(
        UNUSED_DEPENDENCY_PREREQUISITE in case["prerequisites"]
        and (layer != "sdk" or case["source"] not in UNUSED_DEPENDENCY_SOURCES)
        for case in cases.values()
    ):
        raise ValueError("missing native SDK adapter for a mapped dependency case")
    if layer != "sdk" and any(case["source"] == CORPUS_SOURCE for case in cases.values()):
        raise ValueError("the framework corpus requires a native SDK variant")
    if any(
        IMPORT_SCOPE_PREREQUISITES & set(case["prerequisites"])
        and (layer != "sdk" or not key.startswith(IMPORT_SCOPE_CASE))
        for key, case in cases.items()
    ):
        raise ValueError("missing native SDK adapter for a mapped FastAPI import case")
    live_router_cases = {
        key for key, case in cases.items() if LIVE_ROUTER in case["prerequisites"]
    }
    if live_router_cases and (layer != "sdk" or set(cases) != FASTAPI_CASES
                              or live_router_cases != FASTAPI_CASES):
        raise ValueError("the live-router variant must contain the eight pinned FastAPI cases")
    return inventory, digest, variant, cases


class AssertionTrace:
    """Record source lines only as they finish without raising an exception."""

    def __init__(self, root, sources):
        self.sources = {str((root / source).resolve()): source for source in sources}
        self.pending = {}
        self.completed = set()

    def __call__(self, frame, event, _arg):
        source = self.sources.get(frame.f_code.co_filename)
        if source is None:
            return None
        if event == "exception":
            self.pending.pop(frame, None)
        elif event in {"line", "return"}:
            if frame in self.pending:
                self.completed.add(self.pending.pop(frame))
            if event == "line":
                self.pending[frame] = f"{source}:{frame.f_lineno}"
        return self


def case_assertion_sources(upstream, cases):
    sources_by_case = {}
    assertions_by_source = {}
    for key, case in cases.items():
        sources = {case["source"]}
        for assertion in case["assertions"]:
            source, _, _ = assertion.rpartition(":")
            if source not in assertions_by_source:
                path = (upstream / source).resolve()
                if not source or not path.is_relative_to(upstream):
                    raise ValueError(f"{key}: invalid assertion source {source!r}")
                tree = ast.parse(path.read_text(encoding="utf-8"))
                assertions_by_source[source] = {
                    f"{source}:{node.lineno}"
                    for node in ast.walk(tree)
                    if isinstance(node, ast.Assert) or (
                        source == INCLUDE_SOURCE and isinstance(node, ast.With)
                        and any(
                            isinstance(item.context_expr, ast.Call)
                            and ast.unparse(item.context_expr.func) == "pytest.raises"
                            for item in node.items
                        )
                    )
                }
            if assertion not in assertions_by_source[source]:
                raise ValueError(f"{key}: inventory points outside original assertions")
            sources.add(source)
        sources_by_case[key] = sources
    return sources_by_case


def benchmark_assertions(summary, assertions):
    cases = summary["cases"]
    if len(cases) != 1 or cases[0]["id"] != BENCHMARK_ID or summary["skipped_cases"]:
        raise ValueError("the original benchmark case did not run exactly once")
    result = cases[0]
    complete = result["scored"] and result["analysis_status"] == "complete"
    passed = (
        complete,
        complete and result["true_positives"] == 5 and result["false_negatives"] == 0,
        complete and result["true_negatives"] == 3 and result["false_positives"] == 0,
        complete and result["elapsed_seconds"] <= 5.0,
    )
    reached = [label for label, verified in zip(assertions, passed, strict=True) if verified]
    return result, reached


def run_basic_benchmark(args, case):
    from unittest.mock import patch

    from skylos.benchmarks import dead_code

    original = dead_code.analyze
    fixture = "benchmarks/dead_code/fixtures/basic_unused_symbols"
    tracked = subprocess.check_output(
        ["git", "ls-files", "-z", "--", fixture], cwd=args.upstream,
    ).decode("utf-8").split("\0")
    expected = {
        Path(name).relative_to(fixture).as_posix(): (args.upstream / name).read_bytes()
        for name in tracked if name
    }
    if not expected:
        raise ValueError("the benchmark's pinned fixture tree is empty or missing")

    candidate = args.mode == "candidate"
    calls = 0
    prerequisites = dict.fromkeys(case["prerequisites"], "blocked")

    def analyze(path, **options):
        nonlocal calls
        calls += 1
        folder = Path(path)
        actual = {
            item.relative_to(folder).as_posix(): item.read_bytes()
            for item in folder.rglob("*") if item.is_file()
        }
        if actual != expected:
            raise ValueError("the staged benchmark inputs differ from the tracked fixtures")
        if options != {
            "conf": 0, "enable_quality": False, "enable_danger": False,
            "enable_secrets": False, "grep_verify": True,
        }:
            raise ValueError("the pinned benchmark options changed")
        prerequisites["tracked fixture tree available"] = "passed"
        if not candidate:
            return original(path, **options)
        # The Skarmory SDK has no danger or secrets options. Both are disabled.
        del options["enable_danger"], options["enable_secrets"]
        raw = scan_candidate(args.sdk_python, path, options)
        report = json.loads(raw)
        prerequisites["installed-sdk"] = "passed"
        if report["analysis_summary"]["grep_verify"]["enabled"] is True:
            prerequisites["grep-verification"] = "passed"
        if set(report["selected_paths"]) != expected.keys():
            prerequisites["tracked fixture tree available"] = "blocked"
        if report["confidence_threshold"] == 0:
            prerequisites["confidence=0"] = "passed"
        return raw

    with patch.object(dead_code, "analyze", analyze):
        summary = dead_code.run_manifest(
            args.upstream / BENCHMARK_SOURCE, selected_cases={BENCHMARK_ID},
            scanner="skylos", strict_labels=False,
        )
    if calls != 1:
        raise ValueError("the original benchmark must analyze the fixture exactly once")
    result, reached = benchmark_assertions(summary, case["assertions"])
    passed = not summary["failure_count"] and reached == case["assertions"]
    if candidate:
        passed = passed and all(value == "passed" for value in prerequisites.values())
    return {
        "status": "passed" if passed else "failed",
        "assertions": reached,
        "prerequisites": prerequisites,
        "benchmark": result,
    }


def run_framework_corpus_cases(args, cases):
    from check_dead_code_gate import sha256
    from phase03_review_assertions import observe_review
    from skylos.benchmarks import corpus_ci

    manifest = args.upstream / CORPUS_SOURCE
    original = corpus_ci.load_manifest(manifest)
    selected = framework_cases(original)
    originals = {case["id"]: case for case in original["cases"] if case["id"] in selected}

    results = {}
    for key, case in cases.items():
        prefix, separator, case_id = key.partition("::")
        if prefix != CORPUS_SOURCE or not separator or case_id not in originals:
            raise ValueError(f"{key}: inventory points outside the pinned framework corpus")
        expected = corpus_assertions(originals[case_id])
        if expected != case["assertions"]:
            raise ValueError(f"{key}: inventory changes the original framework corpus assertions")
        fixtures = {name: args.upstream / name for name in case["fixtures"]}
        fixture_hashes = {name: sha256(path) for name, path in fixtures.items()}
        scanned = 0
        review = case.get("runtime_review")
        runtime_results = []

        def scan(path, options):
            nonlocal scanned
            scanned += 1
            def read(scope):
                return json.loads(scan_candidate(args.sdk_python, scope, options))

            report = read(path)
            if review:
                runtime_results.append(observe_review(report, path, review["assertions"], read))
            return report

        summary = score_corpus(
            corpus_ci, manifest, {case_id}, scan if args.mode == "candidate" else None,
        )
        if any(sha256(path) != fixture_hashes[name] for name, path in fixtures.items()):
            raise ValueError(f"{key}: fixture bytes changed during corpus execution")
        if summary["case_count"] != 1 or summary["cases"][0]["id"] != case_id:
            raise ValueError(f"{key}: the original scorer did not run exactly the mapped case")
        outcome = summary["cases"][0]
        prerequisites = {
            name: "passed" if (
                name == "pinned-framework-corpus" and outcome["analyzed_source_count"]
                or name == "installed-sdk" and scanned == 1
            ) else "blocked"
            for name in case["prerequisites"]
        }
        executed = outcome["scored"] and outcome["checked_expectation_count"] == len(expected)
        failed = {
            f"{failure['mode']}:{failure['category']}:{failure['expected']}"
            for failure in outcome["failures"]
        }
        reached = [label for label in expected if executed and label not in failed]
        passed = executed and not failed and (
            args.mode == "reference"
            or all(status == "passed" for status in prerequisites.values())
        )
        results[key] = {
            "status": "passed" if passed else "failed",
            "assertions": reached,
            "prerequisites": prerequisites,
            "fixture_sha256": fixture_hashes,
            "corpus": outcome,
        }
        if len(runtime_results) == 1:
            results[key]["reviewed_runtime"] = runtime_results[0]
    return results


def pytest_worker(args):
    from check_dead_code_gate import matches_reviewed_failure
    from phase03_review_assertions import observe_review

    _, _, variant, cases = port_cases(args)
    if args.mode == "candidate" and variant["layer"] == "engine":
        env = environment()
        results = {
            key: engine_case(
                args, key, case, env,
                AssertionTrace(args.upstream, {case["source"]}) if key in INCLUDE_CASES else None,
            )
            for key, case in cases.items()
        }
        args.output.write_text(json.dumps(results), encoding="utf-8")
        return int(any(result["status"] != "passed" for result in results.values()))

    import pytest

    benchmark = cases.pop(BENCHMARK_CASE, None)
    corpus = {key: case for key, case in cases.items() if case["source"] == CORPUS_SOURCE}
    cases = {key: case for key, case in cases.items() if key not in corpus}
    sys.path.insert(0, str(args.upstream))
    sys.modules["skylos_fast"] = None
    from skylos.core.fast import FAST_AVAILABLE

    if FAST_AVAILABLE:
        raise RuntimeError("reference case requires the Python backend")
    assertion_sources = case_assertion_sources(args.upstream, cases)

    live_router = bool(cases) and set(cases) == FASTAPI_CASES and all(
        LIVE_ROUTER in case["prerequisites"] for case in cases.values()
    )
    router_policy = None
    if live_router:
        wheels = verify_wheels(args.fastapi_wheels, variant["environment"])
        if args.mode == "candidate":
            router_policy = args.output.with_name("fastapi-router-mode.toml").resolve()
            router_policy.write_text(
                'root = true\n[dead_code]\nfastapi_router_mode = "live"\n', encoding="utf-8"
            )

    class Capture:
        def __init__(self):
            self.outcomes = {}
            self.lines = {}
            self.failed_assertions = {}
            self.scans = set()
            self.dependency_declarations = set()
            self.grep_scans = set()
            self.grep_disables = set()
            self.trace_imports = set()
            self.trace_disables = set()
            self.trace_rejections = set()
            self.unittest_evidence = set()
            self.framework_evidence = set()
            self.live_router_receipts = set()
            self.source_edit_cache = set()
            self.import_scope_prerequisites = {}
            self.runtime_results = {}

        @pytest.hookimpl(wrapper=True)
        def pytest_runtest_call(self, item):
            key = item.nodeid
            case = cases[key]
            review = case.get("runtime_review")
            trace = AssertionTrace(args.upstream, assertion_sources[key])
            old_trace = sys.gettrace()
            cache_path = None
            scans_with_cache = []

            def analyze(path, **options):
                if router_policy is not None:
                    if options.get("config_file") is not None:
                        pytest.fail("the original case already supplies a configuration")
                    prove_live_router(path, wheels, environment())
                    options["config_file"] = router_policy
                if cache_path is not None:
                    if options.get("cache_dir") is not None:
                        pytest.fail("the original source-edit case already selects a cache")
                    options["cache_dir"] = cache_path
                scanned = scan_candidate(args.sdk_python, path, options)
                report = json.loads(scanned)
                if review:
                    def scan_scope(scope):
                        return json.loads(scan_candidate(args.sdk_python, scope, options))

                    self.runtime_results.setdefault(key, []).append(
                        observe_review(report, path, review["assertions"], scan_scope),
                    )
                elif report["coverage"]["status"] != "complete":
                    pytest.fail(f"candidate scan incomplete: {report['coverage']}")
                self.scans.add(key)
                if cache_path is not None:
                    cache = report.get("cache")
                    if not cache or cache["write_failures"] or cache["writes"] != cache["misses"]:
                        pytest.fail(f"candidate could not cache every selected source: {cache}")
                    if not scans_with_cache and cache["misses"] < 2:
                        pytest.fail(f"source-edit case expected a fresh cache: {cache}")
                    if len(scans_with_cache) == 1:
                        initial = scans_with_cache[0]
                        selected = initial["hits"] + initial["misses"]
                        if cache["misses"] != 1 or cache["hits"] != selected - 1:
                            pytest.fail(f"source-edit case expected only the edited file to miss: {cache}")
                        uncached = json.loads(scan_candidate(
                            args.sdk_python, path, {**options, "cache_dir": None},
                        ))
                        if {**report, "cache": None} != {**uncached, "cache": None}:
                            pytest.fail("cached and uncached reports differ after the source edit")
                    scans_with_cache.append(cache)
                if router_policy is not None and str(router_policy) in report["config_sources"]:
                    self.live_router_receipts.add(key)
                grep_enabled = report["analysis_summary"]["grep_verify"]["enabled"]
                if grep_enabled is True:
                    self.grep_scans.add(key)
                if options.get("grep_verify") is False and grep_enabled is False:
                    self.grep_disables.add(key)
                if any(
                    evidence["kind"] == "runtime_trace"
                    for evidence in report.get("runtime_evidence", [])
                ):
                    self.trace_imports.add(key)
                if (
                    options.get("trace_file") is False
                    and not report.get("runtime_evidence")
                    and not report.get("runtime_evidence_issues")
                ):
                    self.trace_disables.add(key)
                if any(
                    issue["kind"] == "runtime_evidence_rejected"
                    for issue in report.get("runtime_evidence_issues", [])
                ):
                    self.trace_rejections.add(key)
                if "native-framework-evidence" in case["prerequisites"]:
                    self.framework_evidence.add(key)
                    if "native-unittest-evidence" in case["prerequisites"]:
                        self.unittest_evidence.add(key)
                    return json.dumps(framework_report(
                        report, source=case["source"], project=path,
                    ))
                if "native-unittest-evidence" in case["prerequisites"]:
                    from phase03_framework import unittest_report

                    projected = unittest_report(report)
                    self.unittest_evidence.add(key)
                    return json.dumps(projected)
                return scanned

            cli = None
            with ExitStack() as stack:
                patch = stack.enter_context(pytest.MonkeyPatch.context())
                if args.mode == "candidate":
                    if SOURCE_EDIT_CACHE in case["prerequisites"]:
                        cache_path = stack.enter_context(TemporaryDirectory(
                            prefix="skarmory-pinned-source-edit-cache-",
                        ))
                    if variant["layer"] == "cli":
                        if key == CLI_CASE:
                            cli = RecursiveCaseCli(args.artifact)
                            patch.setattr(item.module, "subprocess", cli)
                        elif key in CLEANUP_CASES:
                            cli = CleanupCaseCli(
                                args.artifact, item.funcargs["tmp_path"], key,
                                args.upstream / case["source"],
                            )
                            patch.setattr(item.module, "run_cleanup_command", cli.run)
                        elif key in CLEANUP_EXECUTION_CASES:
                            cli = CleanupExecutionCli(
                                args.artifact, item.funcargs["tmp_path"], key,
                                args.upstream / case["source"],
                            )
                            patch.setattr(item.module, "plan_cleanup_edit", cli.plan)
                            patch.setattr(item.module, "apply_cleanup_edit", cli.apply)
                        else:
                            cli = WorkflowCaseCli(args.artifact, item.funcargs["tmp_path"], key)
                            patch.setattr(item.module.cli, "main", cli.run)
                    elif key.startswith(IMPORT_SCOPE_CASE):
                        def fastapi_project(_definitions, _parsed, path):
                            def collect():
                                report = json.loads(analyze(path, conf=0, grep_verify=False))
                                self.import_scope_prerequisites[key] = import_scope_prerequisites(
                                    report, path,
                                )
                                return import_scope_report(report, path)

                            return SimpleNamespace(collect=collect)

                        def analyze_instance(_instance, path, thr=60, **options):
                            return analyze(path, conf=thr, **options)

                        patch.setattr(item.module, "FastAPIProject", fastapi_project)
                        patch.setattr(item.module.Skylos, "analyze", analyze_instance)
                    elif "native-framework-evidence" in case["prerequisites"]:
                        patch_framework_analysis(patch, item.module, analyze)
                    elif "shared-framework-fixture" in case["prerequisites"]:
                        from test import web_framework_fixtures

                        patch.setattr(web_framework_fixtures, "analyze", analyze)
                    elif UNUSED_DEPENDENCY_PREREQUISITE in case["prerequisites"]:
                        def scan_rule(path, options):
                            return scan_candidate(args.sdk_python, path, options)

                        def analyze_dependencies(*inputs, **options):
                            findings = unused_dependencies(key, scan_rule, *inputs, **options)
                            self.scans.add(key)
                            return findings

                        patch.setattr(item.module, "scan_unused_dependencies", analyze_dependencies)
                        if key == DECLARATION_CASE:
                            def collect_declarations(path):
                                result = dependency_declarations(scan_rule, path)
                                self.dependency_declarations.add(key)
                                return result

                            patch.setattr(item.module, "_collect_declared_deps", collect_declarations)
                    else:
                        patch.setattr(item.module, "analyze", analyze)
                try:
                    sys.settrace(trace)
                    return (yield)
                finally:
                    sys.settrace(old_trace)
                    if cli is not None and cli.proved:
                        self.scans.add(key)
                    if len(scans_with_cache) == 2:
                        self.source_edit_cache.add(key)
                    self.lines[key] = trace.completed

        def pytest_runtest_makereport(self, item, call):
            if (
                call.when == "call"
                and call.excinfo
                and call.excinfo.errisinstance(AssertionError)
            ):
                case = cases[item.nodeid]
                failure = call.excinfo.traceback[-1]
                path = failure.path.resolve()
                if path.is_relative_to(args.upstream):
                    source = path.relative_to(args.upstream).as_posix()
                    assertion = f"{source}:{failure.lineno + 1}"
                    if assertion in case["assertions"] or (
                        args.mode == "candidate" and matches_reviewed_failure(case, assertion)
                    ):
                        self.failed_assertions[item.nodeid] = assertion

        def pytest_runtest_logreport(self, report):
            key = report.nodeid
            if report.when != "call" and not report.passed:
                self.outcomes[key] = "error" if report.failed else (
                    "xfailed" if hasattr(report, "wasxfail") else "skipped"
                )
            elif report.skipped:
                self.outcomes[key] = "xfailed" if hasattr(report, "wasxfail") else "skipped"
            elif report.failed:
                self.outcomes[key] = (
                    "xpassed" if report.longreprtext.startswith("[XPASS(strict)]") else "failed"
                )
            elif report.when == "call":
                self.outcomes[key] = "xpassed" if hasattr(report, "wasxfail") else "passed"

    capture = Capture()
    pytest_args = ["-q", "-p", "no:cacheprovider", *cases]
    if args.mode == "candidate":
        pytest_args.insert(0, "--runxfail")
    status = pytest.main(pytest_args, plugins=[capture]) if cases else 0
    report = {}
    for key, case in cases.items():
        lines = capture.lines.get(key, set())
        prerequisites = {}
        for required in case["prerequisites"]:
            proven = required in lines
            if required == "installed-sdk":
                proven = args.mode == "candidate"
            elif required in {
                "native-cli-control-pair", "native-cli-workflow", "native-cli-cleanup",
                "native-cli-cleanup-execution",
            }:
                proven = key in capture.scans
            elif required == "shared-framework-fixture":
                proven = key in capture.scans
            elif required == UNUSED_DEPENDENCY_PREREQUISITE:
                proven = key in capture.scans
            elif required in DECLARATION_PREREQUISITES:
                proven = key in capture.dependency_declarations
            elif required == "native-framework-evidence":
                proven = key in capture.framework_evidence
            elif required in IMPORT_SCOPE_PREREQUISITES:
                proven = required in capture.import_scope_prerequisites.get(key, ())
            elif required == LIVE_ROUTER:
                proven = key in capture.live_router_receipts
            elif required == "native-unittest-evidence":
                proven = key in capture.unittest_evidence
            elif required == "grep-verification":
                proven = key in capture.grep_scans
            elif required == "grep-verification-disabled":
                proven = key in capture.grep_disables
            elif required == "runtime-trace-import":
                proven = key in capture.trace_imports
            elif required == "runtime-trace-disabled":
                proven = key in capture.trace_disables
            elif required == SOURCE_EDIT_CACHE:
                proven = key in capture.source_edit_cache
            elif required == "runtime-trace-rejected":
                proven = key in capture.trace_rejections
            prerequisites[required] = "passed" if proven else "blocked"
        reached = [assertion for assertion in case["assertions"] if assertion in lines]
        outcome = capture.outcomes.get(key, "error")
        proved = (
            key in capture.scans
            and all(value == "passed" for value in prerequisites.values())
            and reached == case["assertions"]
        )
        if args.mode == "candidate" and outcome == "passed" and not proved:
            outcome = "blocked"
        report[key] = {
            "status": outcome,
            "assertions": reached,
            "failed_assertion": capture.failed_assertions.get(key),
            "prerequisites": prerequisites,
        }
        runtime_results = capture.runtime_results.get(key, ())
        if len(runtime_results) == 1 and (
            outcome == "passed" or outcome == "failed" and key in capture.failed_assertions
        ):
            report[key]["reviewed_runtime"] = runtime_results[0]
    if benchmark is not None:
        report[BENCHMARK_CASE] = run_basic_benchmark(args, benchmark)
    corpus_results = run_framework_corpus_cases(args, corpus)
    report.update(corpus_results)
    args.output.write_text(json.dumps(report), encoding="utf-8")
    return int(status) or int(
        benchmark is not None and report[BENCHMARK_CASE]["status"] != "passed"
        or any(result["status"] != "passed" for result in corpus_results.values())
    )


def write_receipt(args):
    from check_dead_code_gate import sha256, validate_revision

    inventory, digest, variant, cases = port_cases(args)
    revision = inventory["reference_revision"]
    validate_revision(args.candidate_revision, "candidate revision")
    artifact_digest = sha256(args.artifact)
    env = environment()
    with TemporaryDirectory(prefix="skarmory-phase03-") as temporary:
        work = Path(temporary).resolve()
        # Original fixture assertions compare absolute manifest paths. Use the
        # same real temp root for both analyzers so a host alias such as
        # /var -> /private/var does not change the spelling of fixture paths.
        env["TMPDIR"] = str(work)
        commands = {}
        reports = {}
        errors = []
        common = [
            sys.executable, str(SCRIPT), "--upstream", str(args.upstream), "--inventory",
            str(args.inventory), "--variant", args.variant, "--artifact", str(args.artifact),
        ]
        if args.sdk_python is not None:
            common.extend(("--sdk-python", str(args.sdk_python)))
        if args.fastapi_wheels is not None:
            common.extend(("--fastapi-wheels", str(args.fastapi_wheels)))
        for mode in ("reference", "candidate"):
            output = work / (mode + ".json")
            commands[mode] = [*common, "--mode", mode, "--output", str(output)]

        try:
            platform_id = f"{sys.platform}-{platform.machine()}-cp{sys.version_info.major}{sys.version_info.minor}"
            if platform.python_implementation() != "CPython" or variant["platform"] != platform_id:
                raise ValueError(f"required platform {variant['platform']}, found {platform_id}")
            head = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=args.upstream, text=True)
            if head.strip() != revision:
                raise ValueError("Skylos checkout is at the wrong revision")
            if subprocess.check_output(["git", "status", "--porcelain"], cwd=args.upstream, text=True):
                raise ValueError("Skylos checkout has uncommitted changes")
            lock_digest = hashlib.sha256((args.upstream / "uv.lock").read_bytes()).hexdigest()
            if lock_digest not in variant["environment"]:
                raise ValueError("upstream lock digest differs from the inventory")
        except (OSError, ValueError, subprocess.SubprocessError) as error:
            errors.append(str(error))
        else:
            for mode, command in commands.items():
                try:
                    if mode == "candidate" and variant["layer"] == "sdk":
                        check_candidate_wheel(
                            args.sdk_python, args.artifact, variant["platform"], work, env,
                        )
                    run = subprocess.run(command, cwd=args.upstream, env=env, timeout=180, check=False)
                    if run.returncode not in {0, 1}:
                        raise ValueError(f"{mode} conformance runner exited {run.returncode}")
                    reports[mode] = json.loads(Path(command[-1]).read_text(encoding="utf-8"))
                except (OSError, ValueError, subprocess.SubprocessError) as error:
                    errors.append(f"{mode}: {error}")

    if sha256(args.artifact) != artifact_digest:
        errors.append("candidate artifact changed during execution")
    results = []
    for case_id, case in cases.items():
        reference = reports.get("reference", {}).get(case_id, {})
        candidate = reports.get("candidate", {}).get(case_id, {})
        if reference.get("status") == "passed" and reference["assertions"] != case["assertions"]:
            errors.append(f"{case_id}: reference passed before all mapped assertions ran")
        result = {
            "case_id": case_id,
            "reference_status": reference.get("status", "error"),
            "reference_assertions": reference.get("assertions", []),
            "reference_failed_assertion": reference.get("failed_assertion"),
            "status": candidate.get("status", "error"),
            "assertions": candidate.get("assertions", []),
            "prerequisites": candidate.get(
                "prerequisites", dict.fromkeys(case["prerequisites"], "blocked")
            ),
        }
        if "reviewed_runtime" in candidate:
            result["reviewed_runtime"] = candidate["reviewed_runtime"]
            if candidate.get("failed_assertion"):
                result["candidate_failed_assertion"] = candidate["failed_assertion"]
        if case_id == BENCHMARK_CASE:
            result["benchmark"] = {
                "reference": reference.get("benchmark"),
                "candidate": candidate.get("benchmark"),
            }
        if case["source"] == CORPUS_SOURCE:
            fixture_hashes = candidate.get("fixture_sha256")
            if fixture_hashes and fixture_hashes == reference.get("fixture_sha256"):
                result["fixture_sha256"] = fixture_hashes
            else:
                errors.append(f"{case_id}: missing or different reference and candidate fixture hashes")
            result["corpus"] = {
                "reference": reference.get("corpus"),
                "candidate": candidate.get("corpus"),
            }
        if case_id in ENGINE_CASES:
            result["native_test"] = candidate.get("native_test")
        results.append(result)
    receipt = {
        "schema_version": 1,
        "reference_revision": revision,
        "candidate_revision": args.candidate_revision,
        "inventory_sha256": digest,
        "variant": args.variant,
        "environment": variant["environment"],
        "artifact_sha256": artifact_digest,
        "runner_status": "error" if errors else "passed",
        "runner_errors": errors,
        "reference_command": commands["reference"],
        "candidate_command": commands["candidate"],
        "results": results,
    }
    args.output.write_text(json.dumps(receipt, indent=2) + "\n", encoding="utf-8")
    for error in errors:
        print(error, file=sys.stderr)
    # Leave reconciliation and the publication verdict to the gate.
    return 0


def main():
    sys.path.insert(0, str(ROOT / "scripts"))
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--upstream", required=True, type=Path)
    parser.add_argument("--inventory", required=True, type=Path)
    parser.add_argument("--variant", required=True)
    parser.add_argument("--sdk-python", type=Path)
    parser.add_argument("--fastapi-wheels", type=Path)
    parser.add_argument("--artifact", required=True, type=Path)
    parser.add_argument("--candidate-revision")
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--mode", choices=("reference", "candidate"), help=argparse.SUPPRESS)
    args = parser.parse_args()
    args.upstream = args.upstream.resolve()
    for name in ("inventory", "sdk_python", "fastapi_wheels", "output", "artifact"):
        path = getattr(args, name)
        if path is not None:
            # Preserve the venv interpreter symlink and its sys.prefix.
            setattr(args, name, path.absolute())
    if not args.mode and args.candidate_revision is None:
        parser.error("--candidate-revision is required for a receipt")
    try:
        return pytest_worker(args) if args.mode else write_receipt(args)
    except (OSError, ValueError) as error:
        parser.error(str(error))


if __name__ == "__main__":
    raise SystemExit(main())
