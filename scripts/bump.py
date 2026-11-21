#!/usr/bin/env python3
"""Bump zmem's release version; --release also commits and publishes its tag."""
from __future__ import annotations

import argparse
import re
import subprocess
import sys
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
VERSION = re.compile(r"(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\Z")


def run(*args: str, capture: bool = False) -> str:
    result = subprocess.run(args, cwd=ROOT, check=True, text=True, capture_output=capture)
    return result.stdout.strip() if capture else ""


def next_version(current: str, requested: str) -> str:
    if not VERSION.fullmatch(current):
        raise ValueError(f"invalid current version: {current}")
    major, minor, patch = map(int, current.split("."))
    if requested == "major":
        result = f"{major + 1}.0.0"
    elif requested == "minor":
        result = f"{major}.{minor + 1}.0"
    elif requested == "patch":
        result = f"{major}.{minor}.{patch + 1}"
    else:
        result = requested.removeprefix("v")
    if not VERSION.fullmatch(result):
        raise ValueError("version must be patch, minor, major or X.Y.Z")
    if tuple(map(int, result.split("."))) <= (major, minor, patch):
        raise ValueError("new version must be greater than the current version")
    return result


def updated_files(version: str) -> dict[Path, str]:
    manifest = ROOT / "Cargo.toml"
    content = manifest.read_text()
    match = re.search(r"(?ms)^\[package\]\s*\n(.*?)(?=^\[|\Z)", content)
    if not match:
        raise ValueError("Cargo.toml has no [package] section")
    body, count = re.subn(
        r'(?m)(^version[ \t]*=[ \t]*")[^"\n]+(")',
        lambda item: item[1] + version + item[2],
        match.group(1),
    )
    if count != 1:
        raise ValueError("expected exactly one package version in Cargo.toml")
    manifest_text = content[:match.start(1)] + body + content[match.end(1):]
    lock = ROOT / "Cargo.lock"
    lock_content = lock.read_text()
    package = tomllib.loads(content)["package"]["name"]
    pattern = r'(?m)(^name = "' + re.escape(package) + r'"\nversion = ")[^"]+("$)'
    lock_text, count = re.subn(pattern, lambda m: m[1] + version + m[2], lock_content)
    if count != 1:
        raise ValueError(f"expected exactly one {package} package in Cargo.lock")
    return {manifest: manifest_text, lock: lock_text, ROOT / "site/version.js": f'window.ZMEM_VERSION = "{version}";\n'}


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("version", help="patch, minor, major or an explicit X.Y.Z")
    parser.add_argument("--release", action="store_true", help="commit, tag and atomically push main + tag")
    args = parser.parse_args()
    current = tomllib.loads((ROOT / "Cargo.toml").read_text())["package"]["version"]
    version = next_version(current, args.version)
    tag = f"v{version}"
    if args.release:
        if run("git", "branch", "--show-current", capture=True) != "main":
            raise ValueError("--release requires the main branch")
        if run("git", "status", "--porcelain", capture=True):
            raise ValueError("--release requires a clean worktree")
        run("git", "fetch", "--no-recurse-submodules", "origin", "main", "--tags")
        if run("git", "rev-parse", "HEAD", capture=True) != run("git", "rev-parse", "origin/main", capture=True):
            raise ValueError("main must match origin/main before release")
        if subprocess.run(["git", "show-ref", "--verify", "--quiet", f"refs/tags/{tag}"], cwd=ROOT).returncode == 0:
            raise ValueError(f"tag already exists: {tag}")
    changed = updated_files(version)
    for path, text in changed.items():
        path.write_text(text)
    if args.release:
        run("cargo", "test", "--locked")
        run("cargo", "check", "--lib", "--no-default-features", "--locked")
        run("git", "add", *(str(path.relative_to(ROOT)) for path in changed))
        run("git", "commit", "-m", f"release: zmem {version}")
        run("git", "tag", "-a", tag, "-m", f"zmem {version}")
        run("git", "push", "--atomic", "origin", "HEAD:main", f"refs/tags/{tag}")
    print(version)
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (OSError, ValueError, subprocess.CalledProcessError) as error:
        print(f"zmem bump: {error}", file=sys.stderr)
        raise SystemExit(1)
