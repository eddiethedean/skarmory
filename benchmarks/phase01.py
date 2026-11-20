"""Measure equivalent Phase 0.1 packaged scans with different worker budgets."""

import argparse
from datetime import datetime, timezone
from hashlib import sha256
import json
import os
from pathlib import Path
import platform
from statistics import median
import subprocess
from tempfile import TemporaryDirectory

from phase0 import fresh_process


ROOT = Path(__file__).resolve().parents[1]


def generate(project, module_count, dead_per_module):
    package = project / "src" / "benchapp"
    tasks = package / "tasks"
    tasks.mkdir(parents=True)
    (package / "__init__.py").write_text("from .dispatch import launch\n", encoding="utf-8")
    (package / "__main__.py").write_text(
        'from benchapp import launch\n\nif __name__ == "__main__":\n    launch()\n',
        encoding="utf-8",
    )
    (tasks / "__init__.py").write_text("pass\n", encoding="utf-8")

    imports, calls = [], []
    expected = set()
    for index in range(module_count):
        module = f"worker_{index:03}"
        prefix = ".tasks" if index % 2 == 0 else "benchapp.tasks"
        imports.append(f"from {prefix}.{module} import run as run_{index:03}")
        calls.append(f"    run_{index:03}()")
        body = (
            "def setup():\n    return 1\n\n"
            "def helper():\n    pass\n\n"
            "def run():\n"
            "    def nested():\n        helper()\n"
            "    nested()\n\n"
            "def dormant(value=setup()):\n    pass\n"
        )
        dead = ["dormant", *(f"unused_{index:03}_{n:03}" for n in range(dead_per_module))]
        body += "".join(f"\ndef {name}():\n    pass\n" for name in dead[1:])
        file = tasks / f"{module}.py"
        file.write_text(body, encoding="utf-8")
        expected.update((file.relative_to(project).as_posix(), name) for name in dead)
    (package / "dispatch.py").write_text(
        "\n".join([*imports, "", "def launch():", *calls, ""]), encoding="utf-8"
    )
    return expected


def fingerprint(items):
    digest = sha256()
    for name, data in sorted(items.items()):
        path = name.encode()
        digest.update(len(path).to_bytes(8, "big"))
        digest.update(path)
        digest.update(len(data).to_bytes(8, "big"))
        digest.update(data)
    return digest.hexdigest()


def sha(value):
    return sha256(json.dumps(value, sort_keys=True, separators=(",", ":")).encode()).hexdigest()


def output(*command):
    result = subprocess.run(command, cwd=ROOT, capture_output=True, text=True, check=True)
    return result.stdout.strip()


def cpu_info():
    if platform.system() == "Darwin":
        result = subprocess.run(
            ["sysctl", "-n", "machdep.cpu.brand_string"], capture_output=True, text=True
        )
        return result.stdout.strip() or platform.processor()
    if Path("/proc/cpuinfo").is_file():
        for line in Path("/proc/cpuinfo").read_text(encoding="utf-8").splitlines():
            key, _, value = line.partition(":")
            if key.strip() == "model name":
                return value.strip()
    return platform.processor()


def memory_bytes():
    try:
        pages = os.sysconf("SC_PHYS_PAGES")
        page_size = os.sysconf("SC_PAGE_SIZE")
    except (ValueError, OSError):
        return None
    return pages * page_size if pages > 0 and page_size > 0 else None


def summarize(samples):
    wall_ns = [sample["wall_ns"] for sample in samples if "wall_ns" in sample]
    return {
        "wall_ns_median": median(wall_ns) if wall_ns else None,
        "samples": samples,
    }


def record(args, project):
    expected = generate(project, args.modules, args.dead_per_module)
    sources = {
        file.relative_to(project).as_posix(): file.read_bytes()
        for file in project.rglob("*.py")
    }
    observed = {}
    samples = {budget: [] for budget in args.workers}
    for repeat in range(args.samples):
        # Rotate the order to avoid always measuring the same budget first.
        offset = repeat % len(args.workers)
        for budget in args.workers[offset:] + args.workers[:offset]:
            command = [
                args.cli, ".", "--source-root", "src", "--entry-module",
                "benchapp.__main__", "--workers", str(budget), "--timings",
            ]
            try:
                report, stderr, sample = fresh_process(command, project, os.environ)
                sample["phases_ns"] = json.loads(stderr)
            except (OSError, RuntimeError, ValueError) as error:
                samples[budget].append({"error": str(error)})
                continue
            findings = [(item["path"], item["symbol"]) for item in report["findings"]]
            digest = sha(report)
            observed.setdefault(digest, {
                "coverage": report["coverage"],
                "finding_count": len(findings),
                "evidence_count": len(report["evidence"]),
                "uncertainty_count": len(report["uncertainties"]),
            })
            sample["report_sha256"] = digest
            sample["complete_and_expected"] = (
                sample["exit"] == 0
                and report["coverage"]["status"] == "complete"
                and report["coverage"]["selected_files"] == len(sources)
                and report["coverage"]["extracted_files"] == len(sources)
                and sorted(report["selected_paths"]) == sorted(sources)
                and sorted(findings) == sorted(expected)
            )
            samples[budget].append(sample)

    return {
        "source_sha256": fingerprint(sources),
        "files": len(sources),
        "lines": sum(len(raw.splitlines()) for raw in sources.values()),
        "bytes": sum(map(len, sources.values())),
        "expected_finding_count": len(expected),
        "expected_findings_sha256": sha(sorted(expected)),
        "reports": observed,
        "worker_reports_equivalent": len(observed) == 1 and all(
            sample.get("complete_and_expected", False)
            for runs in samples.values() for sample in runs
        ),
        "runs": {str(budget): summarize(runs) for budget, runs in samples.items()},
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("cli", type=Path)
    parser.add_argument("--samples", type=int, default=5)
    parser.add_argument("--modules", type=int, default=24)
    parser.add_argument("--dead-per-module", type=int, default=16)
    parser.add_argument("--workers", type=int, nargs="+", default=[1, 4])
    args = parser.parse_args()
    args.cli = args.cli.resolve()
    if not hasattr(os, "wait4") or args.samples < 1 or args.modules < 2 or args.dead_per_module < 0:
        parser.error("a POSIX host, one sample, and two modules are required")
    if (
        1 not in args.workers
        or len(set(args.workers)) != len(args.workers)
        or len(args.workers) < 2
        or any(worker not in range(1, 33) for worker in args.workers)
    ):
        parser.error("provide distinct worker budgets between 1 and 32 including 1")

    tracked = [ROOT / name for name in ("Cargo.toml", "Cargo.lock", "rust-toolchain.toml")]
    tracked.extend((ROOT / "crates").glob("*/Cargo.toml"))
    tracked.extend((ROOT / "crates").glob("*/src/**/*.rs"))
    tracked_sources = {file.relative_to(ROOT).as_posix(): file.read_bytes() for file in tracked}
    with TemporaryDirectory(prefix="skarmory-phase01-") as work:
        measured = record(args, Path(work))
    result = {
        "schema_version": 1,
        "timestamp_utc": datetime.now(timezone.utc).isoformat(),
        "workload": "packaged-application",
        "modules": args.modules,
        "dead_per_module": args.dead_per_module,
        "samples_per_budget": args.samples,
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
            "profile": "Cargo release",
            "cargo_lock_sha256": sha256(tracked_sources["Cargo.lock"]).hexdigest(),
        },
        "binary": {
            "version": output(str(args.cli), "--version"),
            "sha256": sha256(args.cli.read_bytes()).hexdigest(),
            "workspace_base_revision": output("git", "rev-parse", "HEAD"),
            "build_sources_sha256": fingerprint(tracked_sources),
        },
        "execution": {
            "argv": [
                "skarmory", ".", "--source-root", "src", "--entry-module",
                "benchapp.__main__", "--workers", "<budget>", "--timings",
            ],
            "policy": "application",
            "rule": "unreachable_function",
            "target_python": "3.11",
            "analysis_cache": "none",
            "os_page_cache": "no flush; source files generated before first sample",
        },
        **measured,
    }
    print(json.dumps(result, indent=2))
    return 0 if result["worker_reports_equivalent"] else 2


if __name__ == "__main__":
    raise SystemExit(main())
