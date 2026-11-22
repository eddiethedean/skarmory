"""Measure cached and clean scans of the same generated application and library."""

import argparse
from collections import defaultdict
from datetime import datetime, timezone
from hashlib import sha256
import json
import os
from pathlib import Path
import platform
from tempfile import TemporaryDirectory

from phase0 import fresh_process
from phase01 import ROOT, cpu_info, fingerprint, memory_bytes, output, sha, summarize


def generate(project, workload, modules):
    package = project / "src" / "example"
    package.mkdir(parents=True)
    source = {}
    for index in range(modules):
        source[f"src/example/worker_{index:03}.py"] = (
            f"def run_{index:03}():\n    return {index}\n\n"
            f"def unused_{index:03}():\n    return -{index}\n"
        )
    imports = [f"from .worker_{i:03} import run_{i:03}" for i in range(modules)]
    calls = [f"    run_{i:03}()" for i in range(modules)]
    source["src/example/logic.py"] = "\n".join(
        [*imports, "", "def launch():", *calls, "", "def fallback():", "    pass", ""]
    )
    bridge = "src/example/api.py"
    source[bridge] = "from .logic import launch as start\n"
    if workload == "application":
        source["src/example/__init__.py"] = "pass\n"
        entry = "src/example/__main__.py"
        source[entry] = 'from .api import start\n\nif __name__ == "__main__":\n    start()\n'
        root_edit = {
            entry: 'from .logic import fallback\n\nif __name__ == "__main__":\n    fallback()\n',
        }
    else:
        source["src/example/__init__.py"] = 'from .api import start\n\n__all__ = ["start"]\n'
        root_edit = {
            ".skylos.toml": 'root = true\n\n[[dead_code.entrypoints]]\nfull_name = "example.logic.fallback"\n',
        }
    source[".skylos.toml"] = "root = true\n"
    for name, contents in source.items():
        (project / name).write_bytes(contents.encode("utf-8"))
    return source, {
        "leaf": {"src/example/worker_000.py": source["src/example/worker_000.py"].replace("return 0", "return 99", 1)},
        "root": root_edit,
        "reexport": {bridge: "from .logic import fallback as start\n"},
    }


def commands(args, workload, workers):
    common = [str(args.cli), ".", "--workers", str(workers)]
    if args.profile == "skylos":
        return [*common, "--profile", "skylos", "--json", "--no-grep-verify"]
    if workload == "application":
        return [*common, "--source-root", "src", "--entry-module", "example.__main__", "--timings"]
    return [
        *common, "--source-root", "src", "--policy", "library",
        "--public-module", "example", "--timings",
    ]


def run(command, project, env, expected_paths, cache=None):
    if cache is not None:
        command = [*command, "--cache-dir", str(cache)]
    try:
        report, stderr, sample = fresh_process(command, project, env)
    except (OSError, RuntimeError, ValueError) as error:
        return {"error": str(error), "complete": False}
    stats = report.pop("cache", None)
    if "--timings" in command and stderr:
        try:
            timings = json.loads(stderr)
        except ValueError:
            sample["stderr"] = stderr.decode(errors="replace")
        else:
            stats = timings.pop("cache", stats)
            sample["timings"] = timings
    elif stderr:
        sample["stderr"] = stderr.decode(errors="replace")
    coverage = report.get("coverage", {})
    selected = {path.removeprefix("./") for path in report.get("selected_paths", [])}
    sample.update(
        report_sha256=sha(report),
        coverage=coverage,
        complete=(
            sample["exit"] == 0
            and coverage.get("status") == "complete"
            and selected == expected_paths
        ),
        cache=stats,
    )
    return sample


def measure(args, directory, workload):
    project = directory / workload
    sources, edits = generate(project, workload, args.modules)
    if args.profile == "native" and workload == "library":
        # Change a declared root while reusing all baseline source facts.
        edits["root"] = {}
    expected_paths = {name for name in sources if name.endswith(".py")}
    env = {key: value for key, value in os.environ.items() if not key.startswith("SKYLOS_")}
    buckets = defaultdict(list)
    for repeat in range(args.samples):
        offset = repeat % len(args.workers)
        for workers in args.workers[offset:] + args.workers[:offset]:
            command = commands(args, workload, workers)
            cache = directory / f"{workload}-{workers}-{repeat}"
            cache.mkdir()

            def sample(state, scenario, invocation, cached):
                result = run(invocation, project, env, expected_paths, cache if cached else None)
                buckets[state, workers, scenario].append(result)

            sample("baseline", "disabled", command, False)
            sample("baseline", "cold", command, True)
            sample("baseline", "warm", command, True)
            for name, changes in edits.items():
                invocation = command
                if not changes:
                    invocation = [*command, "--public-callable", "example.logic:fallback"]
                for path, contents in changes.items():
                    (project / path).write_bytes(contents.encode("utf-8"))
                sample(name, "disabled", invocation, False)
                sample(name, "edited", invocation, True)
                for path in changes:
                    (project / path).write_bytes(sources[path].encode("utf-8"))

    results = {}
    states = {"baseline": {}, **edits}
    for state, changes in states.items():
        state_sources = sources | changes
        state_runs = {str(workers): {} for workers in args.workers}
        digests = set()
        complete = True
        for (label, workers, scenario), samples in buckets.items():
            if label != state:
                continue
            state_runs[str(workers)][scenario] = summarize(samples)
            for record in samples:
                digests.add(record.get("report_sha256"))
                complete &= record["complete"]
                stats = record.get("cache") or {}
                if scenario == "warm":
                    complete &= stats.get("hits") == len(expected_paths)
                elif scenario == "cold":
                    complete &= stats.get("writes", 0) > 0
                elif scenario == "edited":
                    files_edited = sum(path.endswith(".py") for path in changes)
                    complete &= (
                        stats.get("hits") == len(expected_paths) - files_edited
                        and stats.get("misses") == files_edited
                    )
        # Root and re-export changes affect liveness, so equal cached/clean
        # output alone would not prove that the workload exercised the edit.
        if state in {"root", "reexport"}:
            complete &= results["baseline"]["report_sha256"] not in digests
        results[state] = {
            "source_sha256": fingerprint({name: contents.encode("utf-8") for name, contents in state_sources.items()}),
            "edits": changes,
            "extra_options": ["--public-callable", "example.logic:fallback"] if state == "root" and not changes else [],
            "complete_and_equivalent": complete and len(digests) == 1,
            "report_sha256": next(iter(digests)) if len(digests) == 1 else None,
            "runs": state_runs,
        }
    return {
        "files": len(expected_paths),
        "lines": sum(len(sources[name].splitlines()) for name in expected_paths),
        "argv": commands(args, workload, 1)[1:],
        "states": results,
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("cli", type=Path)
    parser.add_argument("--profile", choices=["native", "skylos"], default="skylos")
    parser.add_argument("--build-profile", choices=["debug", "release"], default="release")
    parser.add_argument("--samples", type=int, default=5)
    parser.add_argument("--modules", type=int, default=32)
    parser.add_argument("--workers", type=int, nargs="+", default=[1, 4])
    args = parser.parse_args()
    args.cli = args.cli.resolve()
    if not hasattr(os, "wait4") or args.samples < 1 or args.modules < 2:
        parser.error("use a POSIX host, at least one sample, and at least two modules")
    if (
        len(set(args.workers)) != len(args.workers)
        or len(args.workers) < 2
        or 1 not in args.workers
        or any(worker not in range(1, 33) for worker in args.workers)
    ):
        parser.error("provide distinct worker budgets between 1 and 32 including 1")

    tracked = [ROOT / file for file in ("Cargo.toml", "Cargo.lock", "rust-toolchain.toml")]
    tracked.extend((ROOT / "crates").glob("*/Cargo.toml"))
    tracked.extend((ROOT / "crates").glob("*/src/**/*.rs"))
    sources = {path.relative_to(ROOT).as_posix(): path.read_bytes() for path in tracked}
    with TemporaryDirectory(prefix="skarmory-phase02-bench-") as work:
        workloads = {name: measure(args, Path(work), name) for name in ("application", "library")}
    accepted = all(
        state["complete_and_equivalent"]
        for workload in workloads.values()
        for state in workload["states"].values()
    )
    report = {
        "schema_version": 1,
        "timestamp_utc": datetime.now(timezone.utc).isoformat(),
        "workload_generator_sha256": sha256(Path(__file__).read_bytes()).hexdigest(),
        "profile": args.profile,
        "target_python": "3.11",
        "modules_per_workload": args.modules,
        "samples_per_budget": args.samples,
        "worker_budgets": args.workers,
        "cache": "fresh cache per workload, repetition, and worker budget; OS page cache uncontrolled",
        "host": {
            "platform": platform.platform(),
            "processor": cpu_info(),
            "logical_cpus": os.cpu_count(),
            "memory_bytes": memory_bytes(),
            "storage": "local temporary directory",
        },
        "toolchain": {
            "rust": output("rustc", "--version"),
            "python": platform.python_version(),
            "cargo_profile": args.build_profile,
            "cargo_lock_sha256": sha256(sources["Cargo.lock"]).hexdigest(),
        },
        "binary": {
            "version": output(str(args.cli), "--version"),
            "sha256": sha256(args.cli.read_bytes()).hexdigest(),
            "workspace_base_revision": output("git", "rev-parse", "HEAD"),
            "build_sources_sha256": fingerprint(sources),
        },
        "complete_and_equivalent": accepted,
        "workloads": workloads,
    }
    print(json.dumps(report, indent=2))
    return 0 if accepted else 2


if __name__ == "__main__":
    raise SystemExit(main())
