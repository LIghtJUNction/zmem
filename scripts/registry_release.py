#!/usr/bin/env python3
"""Validate a registry release with Project Z's shared projection and receipt gate."""

import argparse
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tomllib
import urllib.parse
import urllib.request


TOOLS_COMMIT = "dbeecf0c038d9184825e6fb2a25dfdc5eb951e40"
TOOLS_FILES = (
    ".gitmodules",
    "scripts/cargo_versions.py",
    "scripts/release_projection.py",
    "scripts/release_receipts.py",
)
REGISTRY_SOURCE = "registry+https://github.com/rust-lang/crates.io-index"
TAG = re.compile(r"v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)")


def require(condition, message):
    if not condition:
        raise ValueError(message)


def run(*command, cwd=None, env=None):
    result = subprocess.run(
        [str(part) for part in command], cwd=cwd, check=True, capture_output=True,
        text=True, env={**os.environ, "GIT_NO_REPLACE_OBJECTS": "1", **(env or {})},
    )
    if result.stderr:
        print(result.stderr, file=sys.stderr, end="")
    return result.stdout


def check_tools(tools):
    require(run("git", "-C", tools, "rev-parse", "HEAD").strip() == TOOLS_COMMIT,
            "Project Z tools must be checked out at " + TOOLS_COMMIT)
    for relative in TOOLS_FILES:
        path = tools / relative
        expected = run("git", "-C", tools, "show", TOOLS_COMMIT + ":" + relative)
        require(path.is_file() and not path.is_symlink()
                and path.read_bytes() == expected.encode(),
                "Shared release tool differs from its pinned Git source: " + relative)


def source_commit(repo, tag):
    require(TAG.fullmatch(tag), "Expected an existing stable release tag vX.Y.Z")
    commit = run("git", "-C", repo, "rev-parse", "refs/tags/" + tag + "^{commit}").strip()
    require(run("git", "-C", repo, "rev-parse", "HEAD").strip() == commit,
            "Source checkout must be at the requested release tag")
    require(not run("git", "-C", repo, "status", "--porcelain", "--untracked-files=no"),
            "Source checkout has tracked modifications")
    package = tomllib.loads(run("git", "-C", repo, "show", commit + ":Cargo.toml"))["package"]
    require(package["name"] == "zmem-manager" and tag == "v" + package["version"],
            "Release tag does not match zmem-manager's committed version")
    return commit


def download(url, destination):
    require(urllib.parse.urlsplit(url).scheme == "https", "Release inputs require HTTPS")
    request = urllib.request.Request(url, headers={"User-Agent": "zmem-registry-release"})
    with urllib.request.urlopen(request, timeout=30) as response:
        require(urllib.parse.urlsplit(response.geturl()).scheme == "https",
                "Release download redirected away from HTTPS")
        payload = response.read(64 * 1024 * 1024 + 1)
    require(len(payload) <= 64 * 1024 * 1024, "Release input exceeds 64 MiB")
    destination.write_bytes(payload)


def check_registry_lock(source, dependencies):
    packages = tomllib.loads((source / "Cargo.lock").read_text())["package"]
    require(not any(item.get("source", "").startswith("git+") for item in packages),
            "Registry staging lock still contains a Git dependency")
    for dependency in dependencies:
        matches = [item for item in packages if item["name"] == dependency["package"]
                   and item["version"] == dependency["version"]]
        require(len(matches) == 1 and matches[0].get("source") == REGISTRY_SOURCE
                and matches[0].get("checksum") == dependency["archive_sha256"],
                "Registry lock does not select the verified archive: " + dependency["package"])


def prepare(args):
    repo, tools, output = (Path(value).resolve() for value in (args.repo, args.tools, args.output))
    check_tools(tools)
    commit = source_commit(repo, args.tag)
    # Shared scripts import only authenticated source, never a pre-existing bytecode cache.
    shared_env = {"PYTHONDONTWRITEBYTECODE": "1", "PYTHONPYCACHEPREFIX": str(output / "python-cache")}
    projection = json.loads(run(
        sys.executable, tools / "scripts/release_projection.py", "stage",
        "--repo", repo, "--commit", commit, "--output", output, env=shared_env,
    ))
    require(not projection["remaining_git_dependencies"],
            "Registry staging contains Git dependencies outside the shared projection")
    source = output / "source"
    inside_git = subprocess.run(
        ["git", "-C", str(source), "rev-parse", "--show-toplevel"], capture_output=True,
        env={**os.environ, "GIT_NO_REPLACE_OBJECTS": "1"},
    )
    require(inside_git.returncode != 0, "Registry staging must be outside every Git checkout")

    receipts = output / "receipts.json"
    download(args.receipts_url, receipts)
    archives = output / "archives"
    archives.mkdir()
    for package, version in sorted({(item["package"], item["version"]) for item in projection["conversions"]}):
        require(re.fullmatch(r"[A-Za-z0-9_-]+", package)
                and re.fullmatch(r"[A-Za-z0-9.+-]+", version), "Invalid crate archive name")
        label = package + "-" + version + ".crate"
        url = "https://static.crates.io/crates/" + urllib.parse.quote(package, safe="") + "/" + urllib.parse.quote(label, safe="")
        download(url, archives / label)

    verified = json.loads(run(
        sys.executable, tools / "scripts/release_receipts.py",
        "--repo", repo, "--commit", commit, "--receipts", receipts,
        "--trusted-receipts-sha256", args.trusted_receipts_sha256, "--archives", archives,
        env=shared_env,
    ))
    (output / "verified-dependencies.json").write_text(json.dumps(verified, indent=2) + "\n")
    run("cargo", "generate-lockfile", cwd=source)
    check_registry_lock(source, verified["dependencies"])
    run("cargo", "publish", "--dry-run", "--locked", "--registry", "crates-io", cwd=source)
    result = {
        "status": "REGISTRY_DRY_RUN_PASSED_NO_UPLOAD",
        "tag": args.tag, "source_commit": commit, "projection_tools_commit": TOOLS_COMMIT,
        "dependencies": verified["dependencies"],
    }
    (output / "registry-validation.json").write_text(json.dumps(result, indent=2) + "\n")
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", type=Path, required=True)
    parser.add_argument("--tools", type=Path, required=True)
    parser.add_argument("--tag", required=True)
    parser.add_argument("--receipts-url", required=True)
    parser.add_argument("--trusted-receipts-sha256", required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    try:
        print(json.dumps(prepare(args), indent=2))
    except subprocess.CalledProcessError as error:
        parser.error((error.stderr or str(error)).strip())
    except (OSError, ValueError) as error:
        parser.error(str(error))


if __name__ == "__main__":
    main()
