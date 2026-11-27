"""Publish the verified workspace, resuming after a partial crates.io upload."""

from hashlib import file_digest
import json
from pathlib import Path
import subprocess
import sys
from urllib.error import HTTPError
from urllib.request import Request, urlopen


def index_path(name):
    name = name.lower()
    if len(name) < 3:
        return f"{len(name)}/{name}"
    if len(name) == 3:
        return f"3/{name[0]}/{name}"
    return f"{name[:2]}/{name[2:4]}/{name}"


def published_version(name, version):
    request = Request(
        f"https://index.crates.io/{index_path(name)}",
        headers={"User-Agent": "skarmory-release (https://github.com/eddiethedean/skarmory)"},
    )
    try:
        with urlopen(request, timeout=30) as response:
            for line in response:
                entry = json.loads(line)
                if entry["name"] == name and entry["vers"] == version:
                    return entry
    except HTTPError as error:
        if error.code != 404:
            raise
    return None


def publish(directory):
    metadata = json.loads(subprocess.check_output([
        "cargo", "metadata", "--locked", "--no-deps", "--format-version", "1",
    ]))
    members = set(metadata["workspace_members"])
    packages = {
        package["name"]: package["version"]
        for package in metadata["packages"]
        if package["id"] in members
        and (package["publish"] is None or "crates-io" in package["publish"])
    }
    archives = {f"{name}-{version}.crate" for name, version in packages.items()}
    if not archives or {path.name for path in directory.glob("*.crate")} != archives:
        raise SystemExit("The verified crate archives must match the publishable workspace")

    command = ["cargo", "publish", "--workspace", "--locked", "--registry", "crates-io"]
    published = []
    for name, version in sorted(packages.items()):
        existing = published_version(name, version)
        if existing is None:
            continue
        with (directory / f"{name}-{version}.crate").open("rb") as archive:
            checksum = file_digest(archive, "sha256").hexdigest()
        if existing["yanked"] or existing["cksum"] != checksum:
            raise SystemExit(f"Registry version {name}@{version} differs from the verified crate or is yanked")
        published.append(name)
        command.extend(("--exclude", name))
        print(f"Verified {name}@{version} is already published", flush=True)

    if len(published) < len(packages):
        subprocess.run(command, check=True)


if __name__ == "__main__":
    if len(sys.argv) != 2:
        raise SystemExit("Usage: publish_crates.py VERIFIED_CRATE_DIRECTORY")
    publish(Path(sys.argv[1]))
