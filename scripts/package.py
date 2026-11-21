#!/usr/bin/env python3
"""Package a verified native zmem binary and its documentation."""
from __future__ import annotations

import argparse
import gzip
import hashlib
import io
import json
import os
import platform
import re
import subprocess
import tarfile
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--arch", choices=["x86_64", "aarch64"], required=True)
    parser.add_argument("--output", type=Path, default=ROOT / "dist")
    args = parser.parse_args()
    version = tomllib.loads((ROOT / "Cargo.toml").read_text())["package"]["version"]
    if not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+", version):
        raise SystemExit("release packaging requires an X.Y.Z version")
    binary = args.binary.resolve(strict=True)
    actual = subprocess.check_output([str(binary), "--version"], text=True).strip()
    if actual != f"zmem {version}":
        raise SystemExit(f"binary version mismatch: {actual!r}")
    machine = platform.machine()
    if machine != args.arch:
        raise SystemExit(f"native packaging requires {args.arch}; running on {machine}")
    commit = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip()
    epoch = int(os.environ.get("SOURCE_DATE_EPOCH") or subprocess.check_output(["git", "show", "-s", "--format=%ct", "HEAD"], cwd=ROOT, text=True))
    name = f"zmem-{version}-linux-{args.arch}"
    payload: dict[str, tuple[bytes, int]] = {
        "bin/zmem": (binary.read_bytes(), 0o755),
        "README.md": ((ROOT / "README.md").read_bytes(), 0o644),
        "LICENSE": ((ROOT / "LICENSE").read_bytes(), 0o644),
        "CHANGELOG.md": ((ROOT / "CHANGELOG.md").read_bytes(), 0o644),
        "build.json": ((json.dumps({"schema": 1, "version": version, "commit": commit, "os": "linux", "arch": args.arch, "libc": platform.libc_ver(), "source_date_epoch": epoch}, indent=2) + "\n").encode(), 0o644),
    }
    manifest = "".join(f"{hashlib.sha256(data).hexdigest()}  {path}\n" for path, (data, _) in sorted(payload.items()))
    payload["MANIFEST.sha256"] = (manifest.encode(), 0o644)
    args.output.mkdir(parents=True, exist_ok=True)
    archive = args.output / f"{name}.tar.gz"
    with archive.open("wb") as raw, gzip.GzipFile(fileobj=raw, mode="wb", filename="", mtime=epoch) as compressed:
        with tarfile.open(fileobj=compressed, mode="w") as tar:
            for path, (data, mode) in sorted(payload.items()):
                info = tarfile.TarInfo(f"{name}/{path}")
                info.size = len(data)
                info.mode = mode
                info.mtime = epoch
                info.uid = info.gid = 0
                info.uname = info.gname = "root"
                tar.addfile(info, io.BytesIO(data))
    digest = hashlib.sha256(archive.read_bytes()).hexdigest()
    archive.with_suffix(archive.suffix + ".sha256").write_text(f"{digest}  {archive.name}\n")
    print(archive)


if __name__ == "__main__":
    main()
