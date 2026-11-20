"""Verify an installed native CLI and SDK wheel, optionally testing a combined wheel."""

import argparse
from hashlib import sha256
import json
import os
from pathlib import Path
import platform
import shutil
from statistics import median
import subprocess
import sys
from tempfile import TemporaryDirectory
from time import perf_counter_ns
from zipfile import ZipFile


ROOT = Path(__file__).resolve().parents[1]
WINDOWS = sys.platform == "win32"
BIN_DIR = "Scripts" if WINDOWS else "bin"
CLI_NAME = "skarmory.exe" if WINDOWS else "skarmory"
PYTHON_NAME = "python.exe" if WINDOWS else "python"
FIXTURE = ROOT / "tests" / "conformance" / "fixtures" / "reachable"


def run(*args, **kwargs):
    return subprocess.run(args, check=True, **kwargs)


def captured(*args, **kwargs):
    return run(*args, capture_output=True, text=True, **kwargs).stdout


def linkage(path):
    if WINDOWS:
        objdump = shutil.which("llvm-objdump")
        if objdump is None:
            objdump = Path(os.environ["ProgramFiles"]) / "LLVM" / "bin" / "llvm-objdump.exe"
        return [
            line.strip() for line in captured(objdump, "-p", path).splitlines()
            if "DLL Name:" in line
        ]
    command = ["otool", "-L"] if sys.platform == "darwin" else ["ldd"]
    lines = captured(*command, path).splitlines()
    if sys.platform == "darwin":
        return [line.strip() for line in lines[1:]]
    return [line.strip() for line in lines]


def artifact(path):
    return {
        "filename": path.name,
        "bytes": path.stat().st_size,
        "sha256": sha256(path.read_bytes()).hexdigest(),
    }


def startup(command, env):
    # Warm OS cache with the same command once, then use fresh processes.
    captured(*command, env=env)
    samples = []
    for _ in range(7):
        start = perf_counter_ns()
        captured(*command, env=env)
        samples.append(perf_counter_ns() - start)
    return {"median_ns": median(samples), "samples_ns": samples}


def verify(wheel, cli, temporary, combined):
    venv = temporary / "venv"
    run(sys.executable, "-m", "venv", venv)
    python = venv / BIN_DIR / PYTHON_NAME
    run(python, "-m", "pip", "install", "--no-deps", wheel, stdout=sys.stderr)
    wheel_cli = venv / BIN_DIR / CLI_NAME
    assert wheel_cli.is_file() == combined, wheel_cli
    if combined:
        assert wheel_cli.read_bytes() == cli.read_bytes(), "wheel changed the native executable"
        cli = wheel_cli

    no_tools = temporary / "empty-path"
    no_tools.mkdir()
    native_env = dict(os.environ, PATH=str(no_tools))
    cli_links = linkage(cli)
    assert cli_links, "no native dependency information"
    assert not any("python" in line.lower() or "pypy" in line.lower() for line in cli_links), cli_links
    version = captured(cli, "--version", env=native_env).strip()
    assert version.startswith("skarmory "), version
    report = json.loads(captured(cli, FIXTURE, env=native_env))
    assert report["coverage"]["status"] == "complete", report

    env = dict(os.environ, SKARMORY_REQUIRE_NATIVE="1", SKARMORY_TEST_CLI=str(cli))
    env.pop("PYTHONPATH", None)
    run(python, "-m", "unittest", "discover", "-s", ROOT / "tests" / "python", "-v", env=env)
    run(python, ROOT / "tests" / "conformance" / "check_phase0.py", cli, stdout=sys.stderr)

    extension = Path(captured(python, "-c", "import skarmory._native as m; print(m.__file__)").strip())
    with ZipFile(wheel) as archive:
        assert any(name.endswith(f".data/scripts/{CLI_NAME}") for name in archive.namelist()) == combined
        assert any(name.endswith(".dist-info/licenses/LICENSE") for name in archive.namelist())
    return {
        "layout": "combined" if combined else "separate",
        "wheel": artifact(wheel),
        "cli": artifact(cli),
        "extension": artifact(extension),
        "cli_linkage": cli_links,
        "extension_linkage": linkage(extension),
        "cli_version_startup": startup([cli, "--version"], native_env),
        "sdk_import_startup": startup([python, "-c", "import skarmory._native"], env),
    }


def build_combined(cli, temporary, maturin):
    # Stage a separate project so the supported SDK wheel keeps its own layout.
    project = temporary / "project"
    project.mkdir()
    for name in ("Cargo.toml", "Cargo.lock", "README.md", "LICENSE"):
        shutil.copy2(ROOT / name, project / name)
    for name in ("crates", "python"):
        shutil.copytree(ROOT / name, project / name, ignore=shutil.ignore_patterns("__pycache__"))
    pyproject = (ROOT / "pyproject.toml").read_text(encoding="utf-8")
    (project / "pyproject.toml").write_text(pyproject + '\ndata = "wheel-data"\n', encoding="utf-8")
    scripts = project / "wheel-data" / "scripts"
    scripts.mkdir(parents=True)
    shutil.copy2(cli, scripts / CLI_NAME)
    out = temporary / "wheel"
    env = dict(os.environ, CARGO_TARGET_DIR=str(ROOT / "target"))
    run(
        maturin, "build", "--locked", "--release", "--interpreter", sys.executable, "--out", out,
        cwd=project, env=env, stdout=sys.stderr,
    )
    wheel, = out.glob("*.whl")
    return wheel


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("wheel", type=Path)
    parser.add_argument("cli", type=Path)
    parser.add_argument("--combined", action="store_true")
    parser.add_argument("--maturin", default="maturin")
    args = parser.parse_args()
    wheel, cli = args.wheel.resolve(), args.cli.resolve()

    with TemporaryDirectory(prefix="skarmory-distributions-") as directory:
        temporary = Path(directory)
        results = [verify(wheel, cli, temporary / "separate", combined=False)]
        if args.combined:
            combined_dir = temporary / "combined"
            combined_dir.mkdir()
            combined_wheel = build_combined(cli, combined_dir, args.maturin)
            results.append(verify(combined_wheel, cli, combined_dir, combined=True))

    print(json.dumps({
        "platform": platform.platform(),
        "python": platform.python_version(),
        "rust": captured("rustc", "--version").strip(),
        "distributions": results,
    }, indent=2))


if __name__ == "__main__":
    main()
