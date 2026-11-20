"""Validate scaffold manifests and local documentation links using the stdlib."""

import ast
from pathlib import Path
import re
import sys
import tomllib
from urllib.parse import unquote, urlsplit


ROOT = Path(__file__).resolve().parents[1]


def read_toml(path: Path) -> dict:
    with path.open("rb") as stream:
        return tomllib.load(stream)


def main() -> int:
    errors: list[str] = []
    cargo = read_toml(ROOT / "Cargo.toml")
    python = read_toml(ROOT / "pyproject.toml")

    for member in cargo["workspace"]["members"]:
        manifest = ROOT / member / "Cargo.toml"
        if not manifest.is_file():
            errors.append(f"Missing Cargo member: {manifest.relative_to(ROOT)}")
            continue
        read_toml(manifest)
        if not any((manifest.parent / "src" / name).is_file() for name in ("lib.rs", "main.rs")):
            errors.append(f"Missing source entry point: {member}")

    maturin = python["tool"]["maturin"]
    if not (ROOT / maturin["manifest-path"]).is_file():
        errors.append("Maturin's Cargo manifest does not exist")
    package = ROOT / maturin["python-source"] / "skarmory"
    for name in ("__init__.py", "api.py", "_native.pyi", "py.typed"):
        if not (package / name).is_file():
            errors.append(f"Missing SDK file: {name}")

    tree = ast.parse((package / "_version.py").read_text(encoding="utf-8"))
    versions = [
        ast.literal_eval(node.value)
        for node in tree.body
        if isinstance(node, ast.Assign)
        and any(isinstance(target, ast.Name) and target.id == "__version__" for target in node.targets)
    ]
    if versions != [cargo["workspace"]["package"]["version"]]:
        errors.append("Python and Rust development versions differ")

    baseline = read_toml(ROOT / "tests/conformance/baselines/skylos-5.7.0.toml")
    if baseline["reference"]["git_revision"] != "87978e69adf91b3811e97b00c40aa39d7937cd94":
        errors.append("The documented Skylos baseline needs an explicit update")

    for document in ROOT.rglob("*.md"):
        if any(part in {"target", ".venv", "dist"} for part in document.relative_to(ROOT).parts):
            continue
        text = document.read_text(encoding="utf-8")
        for target in re.findall(r"\[[^\]]*\]\(([^\s)]+)\)", text):
            parsed = urlsplit(target)
            if parsed.scheme or parsed.netloc or not parsed.path:
                continue
            linked = document.parent / unquote(parsed.path)
            if not linked.exists():
                errors.append(f"Broken local link in {document.relative_to(ROOT)}: {target}")

    if errors:
        for error in errors:
            print(error, file=sys.stderr)
        return 1
    print("Scaffold manifests, versions, baseline, and local documentation links are consistent.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
