"""Adapt pinned FastAPI route and import-scope cases to native evidence."""

import hashlib
import subprocess
import sys
from pathlib import Path
from types import SimpleNamespace

SOURCE = "test/test_fastapi_route_precision.py"
LIVE_ROUTER = "fastapi-0.141.1-live-router"
CASES = {
    f"{SOURCE}::test_fastapi_routes_need_real_executing_registration[{grep}-{confidence}-{case}]"
    for grep in (False, True)
    for confidence in (0, 80)
    for case in ("nested_router_included_late", "router_added_after_include")
}
IMPORT_SCOPE_SOURCE = "test/test_fastapi_import_scope_boundaries.py"
IMPORT_SCOPE_CASE = f"{IMPORT_SCOPE_SOURCE}::test_missing_nested_submodule_does_not_resolve_from_root_package["
REGULAR_PACKAGE = "selected project-local regular package"
SHADOWED_ROUTE = "shadowed root route control"
IMPORT_SCOPE_PREREQUISITES = {REGULAR_PACKAGE, SHADOWED_ROUTE}
LOCK = Path(__file__).with_name("baselines") / "skylos-5.7.0-fastapi-runtime.txt"
PROBE = """
import runpy
import sys
from pathlib import Path

app_file, *wheels = sys.argv[1:]
sys.path[:0] = wheels

import fastapi
from fastapi.testclient import TestClient

assert fastapi.__version__ == "0.141.1", fastapi.__version__
assert Path(fastapi.__file__).is_relative_to(wheels[0]), fastapi.__file__
app = runpy.run_path(app_file)["app"]
with TestClient(app) as client:
    response = client.get("/")
    assert response.status_code == 200 and response.json() == 1, response.text
"""


def verify_wheels(directory, environment):
    if directory is None:
        raise ValueError("the live-router variant needs the verified FastAPI wheels")
    lock = LOCK.read_bytes()
    if hashlib.sha256(lock).hexdigest() not in environment:
        raise ValueError("FastAPI runtime lock digest differs from the inventory")
    wheels = []
    for line in lock.decode("ascii").splitlines():
        requirement, sha256 = line.split(" --hash=sha256:")
        name, version = requirement.split("==")
        wheel = directory / f"{name.replace('-', '_')}-{version}-py3-none-any.whl"
        with wheel.open("rb") as stream:
            actual = hashlib.file_digest(stream, "sha256").hexdigest()
        if actual != sha256:
            raise ValueError(f"FastAPI runtime wheel differs from its lock: {wheel.name}")
        wheels.append(wheel)
    return wheels


def prove_live_router(fixture, wheels, env):
    result = subprocess.run(
        [sys.executable, "-I", "-B", "-c", PROBE, str(Path(fixture).resolve() / "app.py"),
         *map(str, wheels)],
        env=env,
        capture_output=True,
        text=True,
        timeout=30,
        check=False,
    )
    if result.returncode:
        raise ValueError(f"pinned FastAPI route could not be served: {result.stderr.strip()}")


def import_scope_report(report, fixture):
    """Project native declaration and activation facts for FastAPIProject.collect."""
    def location(row):
        return str((Path(fixture) / row["file"]).resolve()), row["line"]

    declarations = report["analysis_summary"].get("fastapi_route_declarations", ())
    registers = {
        "certain": set(),
        "possible": set(),
    }
    for row in report["dead_code_evidence"]["symbols"]:
        for event in row["evidence"]:
            if event["kind"] == "fastapi_registration":
                registers[event["certainty"]].add(location(row))
    return SimpleNamespace(
        complete=report["coverage"]["status"] == "complete",
        candidates={location(row) for row in declarations},
        roots=registers["certain"],
        uncertain=registers["possible"],
    )


def import_scope_prerequisites(report, fixture):
    paths = set(report["selected_paths"])
    fixture = Path(fixture)
    route = "app/routes.py"
    proven = set()
    if (
        {"app/__init__.py", "backend/app/__init__.py", "backend/app/main.py"} <= paths
        and (fixture / "app/__init__.py").is_file()
        and (fixture / "backend/app/__init__.py").is_file()
        and not any((fixture / "backend/app").glob("routes*"))
    ):
        proven.add(REGULAR_PACKAGE)
    if route in paths and (fixture / route).is_file():
        proven.add(SHADOWED_ROUTE)
    return proven
