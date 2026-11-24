"""Collect the license texts supplied by the locked Rust dependencies."""

import argparse
from collections import defaultdict
import json
from pathlib import Path
import subprocess


ROOT = Path(__file__).resolve().parents[1]
NOTICES = ROOT / "THIRD_PARTY_NOTICES.txt"
NOTICE_PREFIXES = ("license", "copying", "notice", "copyright")
UPSTREAM_INPUTS = {
    ("https://github.com/oxc-project/oxc", "0.159.0"): "oxc-0.159.0.txt",
    ("https://github.com/astral-sh/ruff", "0.0.12"): "ruff-0.0.12.txt",
    ("https://github.com/ModProg/quote-use", "0.8.4"): "mit-terms.txt",
}


def crate_notices(crate):
    root = Path(crate["manifest_path"]).parent
    paths = sorted(
        path for path in root.rglob("*")
        if path.is_file() and path.name.lower().startswith(NOTICE_PREFIXES)
    )
    if crate["license_file"]:
        path = root / crate["license_file"]
        if path not in paths:
            paths.append(path)
    if crate["name"] == "r-efi" and crate["version"] == "6.0.0":
        paths.append(root / "AUTHORS")
    if paths:
        return [(path.relative_to(root).as_posix(), path) for path in paths]

    fallback = UPSTREAM_INPUTS.get((crate["repository"], crate["version"]))
    if fallback is None:
        raise SystemExit(f"Missing license text for {crate['name']} {crate['version']}")
    path = ROOT / "licenses" / fallback
    return [(path.relative_to(ROOT).as_posix(), path)]


def generate():
    cargo = subprocess.check_output(
        ["cargo", "metadata", "--format-version", "1", "--locked"], cwd=ROOT
    )
    crates = sorted(
        (crate for crate in json.loads(cargo)["packages"] if crate["source"]),
        key=lambda crate: (crate["name"], crate["version"]),
    )
    inventory = []
    texts = defaultdict(list)
    for crate in crates:
        label = f"{crate['name']} {crate['version']}"
        origin = crate["repository"] or crate["source"]
        inventory.append(f"{label} ({crate['license']}) — {origin}")
        for name, path in crate_notices(crate):
            text = path.read_text(encoding="utf-8-sig").strip()
            texts[text].append(f"{label} / {name}")

    sections = [
        "Skarmory Rust dependency notices\n"
        "Generated from Cargo.lock by scripts/update_third_party_notices.py.\n"
        "This inventory covers the locked dependencies across build targets.\n"
        "Pinned inputs for missing archive license texts are documented in licenses/README.md.",
        "DEPENDENCIES\n" + "\n".join(inventory),
    ]
    for text, labels in texts.items():
        sections.append("-" * 72 + "\n" + "\n".join(labels) + "\n\n" + text)
    return "\n\n".join(sections) + "\n"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--check", action="store_true")
    args = parser.parse_args()
    expected = generate()
    if args.check:
        if NOTICES.read_text(encoding="utf-8") != expected:
            raise SystemExit("Run python scripts/update_third_party_notices.py to refresh the notices")
    else:
        NOTICES.write_text(expected, encoding="utf-8")


if __name__ == "__main__":
    main()
