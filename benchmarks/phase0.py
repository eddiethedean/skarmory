"""Measure the native prototype and two Skylos backends in fresh processes."""

import argparse
from datetime import datetime, timezone
from hashlib import sha256
import importlib.metadata
import json
import os
from pathlib import Path
import platform
import shutil
import subprocess
import sys
from tempfile import TemporaryDirectory, TemporaryFile
from time import perf_counter_ns
import tomllib


SCRIPT = Path(__file__).resolve()
ROOT = SCRIPT.parents[1]
CONFORMANCE = ROOT / "tests" / "conformance"
BASELINES = CONFORMANCE / "baselines"
MANIFEST = BASELINES / "skylos-5.7.0.toml"


def run_reference(mode, path):
    baseline = tomllib.loads(MANIFEST.read_text(encoding="utf-8"))
    assert importlib.metadata.version("skylos") == baseline["reference"]["version"]
    if mode == "python":
        sys.modules["skylos_fast"] = None

    from skylos import analyzer
    from skylos.core.fast import FAST_AVAILABLE

    assert FAST_AVAILABLE == (mode == "skylos_fast")
    native_calls = 0
    if FAST_AVAILABLE:
        discover = analyzer._fast_discover
        assert discover is not None

        def record_native(*args, **kwargs):
            nonlocal native_calls
            files = discover(*args, **kwargs)
            native_calls += 1
            return files

        analyzer._fast_discover = record_native

    discovered = set()
    get_files = analyzer.Skylos._get_python_files

    def record_files(self, *args, **kwargs):
        files, root = get_files(self, *args, **kwargs)
        discovered.update(Path(file).resolve().relative_to(path).as_posix() for file in files)
        return files, root

    analyzer.Skylos._get_python_files = record_files
    config = str(BASELINES / baseline["environment"]["config"])
    report = json.loads(analyzer.analyze(str(path), config_file=config, **baseline["api_options"]))
    assert (native_calls > 0) == FAST_AVAILABLE
    summary = report["analysis_summary"]
    paths = (str(path) + "/", str(path) + "\\")

    def relative(value):
        if isinstance(value, str):
            for prefix in paths:
                value = value.replace(prefix, "")
        return value

    findings = [
        {key: relative(item[key]) for key in (
            "file", "full_name", "line", "confidence", "dead_code_disposition"
        )}
        for item in report["unused_functions"]
    ]
    print(json.dumps({
        "files": sorted(discovered),
        "reported_file_count": summary["total_files"],
        "complete_repository": summary.get("comparison_scope", {}).get("complete_repository"),
        "resolution_scope_limited": summary.get("resolution_scope_limited"),
        "errors": [{key: relative(value) for key, value in e.items()} for e in report["analysis_errors"]],
        "abstentions": len(report.get("dead_code_abstentions", [])),
        "native_discovery_calls": native_calls,
        "findings": sorted(findings, key=lambda f: (f["file"], f["line"], f["full_name"])),
        "other_nonempty_categories": sorted(
            key for key, rows in report.items()
            if key.startswith("unused_") and key != "unused_functions" and rows
        ),
    }))


def fresh_process(command, project, env):
    with TemporaryFile() as stdout, TemporaryFile() as stderr:
        start = perf_counter_ns()
        with subprocess.Popen(command, cwd=project, env=env, stdout=stdout, stderr=stderr) as child:
            _, status, usage = os.wait4(child.pid, 0)
            elapsed = perf_counter_ns() - start
            child.returncode = os.waitstatus_to_exitcode(status)
        stdout.seek(0)
        stderr.seek(0)
        out, err = stdout.read(), stderr.read()
    if not out:
        raise RuntimeError(f"{command[0]} exited {child.returncode}: {err.decode(errors='replace')}")
    return json.loads(out), err, {
        "wall_ns": elapsed,
        "user_cpu_s": usage.ru_utime,
        "system_cpu_s": usage.ru_stime,
        "peak_rss_bytes": usage.ru_maxrss * (1 if sys.platform == "darwin" else 1024),
        "exit": child.returncode,
        "stdout_bytes": len(out),
    }


def generate(workload):
    workload.mkdir()
    imports, calls = [], []
    for index in range(32):
        module = f"worker_{index:02}"
        function = f"run_{index:02}"
        imports.append(f"from {module} import {function}")
        calls.append(f"{function}()")
        source = f"def {function}():\n    pass\n"
        for dead in range(8):
            source += f"\ndef unused_{index:02}_{dead:02}():\n    pass\n"
        (workload / f"{module}.py").write_text(source, encoding="utf-8")
    (workload / "main.py").write_text("\n".join(imports + [""] + calls) + "\n", encoding="utf-8")


def compare(case, name):
    runs = case["runs"]
    native = runs["skarmory"]["report"]
    actual = {
        (f["path"], f'{f["module"]}.{f["symbol"]}', f["line"])
        for f in native["findings"]
    }
    projection = {"skarmory": actual}
    for mode in ("python", "skylos_fast"):
        report = runs[mode]["report"]
        projection[mode] = {(f["file"], f["full_name"], f["line"]) for f in report["findings"]}
    case["projection_differences"] = {
        mode: {
            "skarmory_only": sorted(actual - projection[mode]),
            "skylos_only": sorted(projection[mode] - actual),
        }
        for mode in ("python", "skylos_fast")
    }
    case["reference_backends_agree"] = all(
        value == runs["skylos_fast"]["report"][key]
        for key, value in runs["python"]["report"].items()
        if key != "native_discovery_calls"
    )
    if name == "scale":
        expected = {
            (f"worker_{module:02}.py", f"worker_{module:02}.unused_{module:02}_{dead:02}", 4 + dead * 3)
            for module in range(32) for dead in range(8)
        }
        assert actual == expected and native["coverage"]["status"] == "complete"
        for mode, findings in projection.items():
            report = runs[mode]["report"]
            report["projected_finding_count"] = len(findings)
            report["projected_finding_sha256"] = sha256(json.dumps(sorted(findings)).encode()).hexdigest()
            del report["findings"]


def measure(args, temporary, env):
    projects = {}
    for fixture in sorted((CONFORMANCE / "fixtures").iterdir()):
        projects[fixture.name] = temporary / fixture.name
        shutil.copytree(fixture, projects[fixture.name])
    projects["scale"] = temporary / "scale"
    generate(projects["scale"])

    results = {}
    for name, project in projects.items():
        project = project.resolve()
        files = {p.name: p.read_bytes() for p in sorted(project.glob("*.py"))}
        results[name] = result = {
            "sources": {path: sha256(data).hexdigest() for path, data in files.items()},
            "bytes": sum(map(len, files.values())),
            "lines": sum(len(data.splitlines()) for data in files.values()),
            "runs": {},
        }
        commands = {
            "skarmory": [args.cli, "--timings", project],
            **{
                backend: [args.reference_python, SCRIPT, "--worker", backend, project]
                for backend in ("python", "skylos_fast")
            },
        }
        repeats = args.samples if name in {"reachable", "scale"} else 1
        order = list(commands)
        for sample in range(repeats):
            offset = sample % len(order)
            for mode in order[offset:] + order[:offset]:
                report, stderr, sample_metrics = fresh_process(commands[mode], project, env)
                run = result["runs"].setdefault(mode, {"report": report, "samples": []})
                assert run["report"] == report, (name, mode, "nondeterministic report")
                if mode == "skarmory":
                    sample_metrics["phases"] = json.loads(stderr)
                    assert report["coverage"]["selected_files"] == len(files)
                    assert sample_metrics["exit"] == (0 if report["coverage"]["status"] == "complete" else 2)
                else:
                    assert sample_metrics["exit"] == 0, (name, mode, stderr)
                    assert report["files"] == sorted(files), (name, mode, report)
                run["samples"].append(sample_metrics)
        compare(result, name)
        print(f"{name}: {repeats} sample(s) per backend", file=sys.stderr)
    return results


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("cli", type=Path)
    parser.add_argument("reference_python", type=Path)
    parser.add_argument("--samples", type=int, default=5)
    args = parser.parse_args()
    if not hasattr(os, "wait4") or args.samples < 1:
        parser.error("a POSIX host and at least one sample are required")
    args.cli = args.cli.resolve()
    # Resolving the venv's python symlink would bypass that environment.
    args.reference_python = args.reference_python.absolute()

    env = {key: value for key, value in os.environ.items() if not key.startswith("SKYLOS_")}
    env.pop("PYTHONPATH", None)
    env.update(RAYON_NUM_THREADS="1", PYTHONHASHSEED="0")
    with TemporaryDirectory(prefix="skarmory-phase0-") as directory:
        results = measure(args, Path(directory), env)
    print(json.dumps({
        "schema_version": 1,
        "timestamp_utc": datetime.now(timezone.utc).isoformat(),
        "platform": platform.platform(),
        "logical_cpus": os.cpu_count(),
        "rust": subprocess.check_output(["rustc", "--version"], text=True).strip(),
        "cargo_lock_sha256": sha256((ROOT / "Cargo.lock").read_bytes()).hexdigest(),
        "native_executable_sha256": sha256(args.cli.read_bytes()).hexdigest(),
        "cache_state": "Fresh processes without an analysis cache; OS cache uncontrolled.",
        "rayon_threads": 1,
        "equal_work_speedup_claim": False,
        "cases": results,
    }, indent=2, sort_keys=True))


if __name__ == "__main__":
    if len(sys.argv) == 4 and sys.argv[1] == "--worker":
        run_reference(sys.argv[2], Path(sys.argv[3]))
    else:
        main()
