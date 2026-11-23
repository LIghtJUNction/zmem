"""Offline integration fixtures for the adapter; these are not release receipts."""

import argparse
import hashlib
import io
import json
import os
from pathlib import Path
import subprocess
import tarfile
import tempfile
import unittest
from unittest import mock

import registry_release as release


@unittest.skipUnless(os.environ.get("PROJECT_Z_TOOLS"), "set PROJECT_Z_TOOLS to the pinned shared checkout")
class RegistryReleaseTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="zmem-registry-test-")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.repo = self.root / "source"
        self.repo.mkdir()
        self.revision = "a" * 40
        (self.repo / "Cargo.toml").write_text('''[package]
name = "zmem-manager"
version = "1.2.3"
edition = "2024"
[features]
default = ["std"]
std = ["dep:zhex"]
[dependencies]
zhex = { git = "https://github.com/LIghtJUNction/zhex.git", optional = true }
''')
        (self.repo / "Cargo.lock").write_text('''version = 4
[[package]]
name = "zmem-manager"
version = "1.2.3"
dependencies = ["zhex"]
[[package]]
name = "zhex"
version = "0.1.3"
source = "git+https://github.com/LIghtJUNction/zhex.git#''' + self.revision + '"\n')
        self.git("init", "-q", "-b", "main")
        self.commit()
        self.archive = self.crate_archive()
        self.checksum = hashlib.sha256(self.archive).hexdigest()
        self.receipt = {
            "package": "zhex", "version": "0.1.3",
            "github": "https://github.com/LIghtJUNction/zhex",
            "git_sha": self.revision, "archive_sha256": self.checksum,
        }
        self.args = argparse.Namespace(
            repo=self.repo, tools=Path(os.environ["PROJECT_Z_TOOLS"]), tag="v1.2.3",
            receipts_url="https://example.invalid/reviewed-receipts.json",
            output=self.root / "stage", trusted_receipts_sha256="",
        )
        self.set_receipts([self.receipt])
        self.cargo_commands = []
        self.lock_checksum = self.checksum
        self.dry_run_fails = False

    def git(self, *args):
        return subprocess.check_output(
            ["git", "-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid", *args],
            cwd=self.repo, text=True, stderr=subprocess.PIPE,
        ).strip()

    def commit(self):
        self.git("add", ".")
        self.git("commit", "-qm", "Offline fixture")
        self.git("tag", "-f", "v1.2.3")

    def crate_archive(self):
        buffer = io.BytesIO()
        files = {
            "Cargo.toml": b'[package]\nname="zhex"\nversion="0.1.3"\nrepository="https://github.com/LIghtJUNction/zhex"\n',
            ".cargo_vcs_info.json": json.dumps({"git": {"sha1": self.revision, "dirty": False}}).encode(),
        }
        with tarfile.open(fileobj=buffer, mode="w:gz") as archive:
            for name, payload in files.items():
                info = tarfile.TarInfo("zhex-0.1.3/" + name)
                info.size = len(payload)
                archive.addfile(info, io.BytesIO(payload))
        return buffer.getvalue()

    def set_receipts(self, records):
        self.receipts = (json.dumps({"schema_version": 1, "receipts": records}, indent=2) + "\n").encode()
        self.args.trusted_receipts_sha256 = hashlib.sha256(self.receipts).hexdigest()

    def write_registry_lock(self, source):
        (source / "Cargo.lock").write_text('''version = 4
[[package]]
name = "zmem-manager"
version = "1.2.3"
[[package]]
name = "zhex"
version = "0.1.3"
source = "''' + release.REGISTRY_SOURCE + '"\nchecksum = "' + self.lock_checksum + '"\n')

    def prepare(self):
        real_run = release.run

        def run(*command, cwd=None, env=None):
            if command[0] != "cargo":
                return real_run(*command, cwd=cwd, env=env)
            self.cargo_commands.append(command)
            if command[1] == "generate-lockfile":
                self.write_registry_lock(cwd)
            elif self.dry_run_fails:
                raise subprocess.CalledProcessError(101, command, stderr="fixture package verification failed")
            return ""

        def download(url, destination):
            if url == self.args.receipts_url:
                destination.write_bytes(self.receipts)
            else:
                self.assertEqual(url, "https://static.crates.io/crates/zhex/zhex-0.1.3.crate")
                destination.write_bytes(self.archive)

        with mock.patch.object(release, "run", side_effect=run), mock.patch.object(release, "download", side_effect=download):
            return release.prepare(self.args)

    def test_shared_tools_verify_source_before_locked_dry_run(self):
        original = {name: (self.repo / name).read_bytes() for name in ("Cargo.toml", "Cargo.lock")}
        result = self.prepare()
        self.assertEqual(result["status"], "REGISTRY_DRY_RUN_PASSED_NO_UPLOAD")
        self.assertEqual(result["source_commit"], self.git("rev-parse", "HEAD"))
        self.assertEqual(result["dependencies"][0]["archive_sha256"], self.checksum)
        self.assertEqual(self.cargo_commands, [
            ("cargo", "generate-lockfile"),
            ("cargo", "publish", "--dry-run", "--locked", "--registry", "crates-io"),
        ])
        self.assertEqual((self.args.output / "receipts.json").read_bytes(), self.receipts)
        for name, payload in original.items():
            self.assertEqual((self.repo / name).read_bytes(), payload)
        self.assertEqual(self.git("status", "--porcelain"), "")

    def test_missing_receipt_stops_before_cargo(self):
        self.set_receipts([])
        with self.assertRaises(subprocess.CalledProcessError) as error:
            self.prepare()
        self.assertIn("missing trusted receipt", error.exception.stderr)
        self.assertEqual(self.cargo_commands, [])

    def test_wrong_trusted_digest_stops_before_cargo(self):
        self.args.trusted_receipts_sha256 = "0" * 64
        with self.assertRaises(subprocess.CalledProcessError) as error:
            self.prepare()
        self.assertIn("trusted receipt SHA-256 mismatch", error.exception.stderr)
        self.assertEqual(self.cargo_commands, [])

    def test_different_source_with_same_version_stops_before_cargo(self):
        self.set_receipts([{**self.receipt, "git_sha": "b" * 40}])
        with self.assertRaises(subprocess.CalledProcessError) as error:
            self.prepare()
        self.assertIn("differs from locked Git SHA", error.exception.stderr)
        self.assertEqual(self.cargo_commands, [])

    def test_registry_checksum_mismatch_stops_before_dry_run(self):
        self.lock_checksum = "0" * 64
        with self.assertRaisesRegex(ValueError, "does not select the verified archive"):
            self.prepare()
        self.assertEqual(self.cargo_commands, [("cargo", "generate-lockfile")])
        self.assertFalse((self.args.output / "registry-validation.json").exists())

    def test_unprojected_dependency_stops_before_cargo(self):
        with (self.repo / "Cargo.toml").open("a") as manifest:
            manifest.write('unknown = { git = "https://github.com/example/unknown.git" }\n')
        self.commit()
        with self.assertRaisesRegex(ValueError, "outside the shared projection"):
            self.prepare()
        self.assertEqual(self.cargo_commands, [])

    def test_tag_must_match_checked_out_commit_and_version(self):
        self.git("tag", "v2.0.0")
        self.args.tag = "v2.0.0"
        with self.assertRaisesRegex(ValueError, "committed version"):
            self.prepare()
        self.args.tag = "v1.2.3"
        (self.repo / "README.md").write_text("Another commit")
        self.git("add", ".")
        self.git("commit", "-qm", "Another fixture commit")
        with self.assertRaisesRegex(ValueError, "requested release tag"):
            self.prepare()

    def test_failed_dry_run_never_records_ready_for_upload(self):
        self.dry_run_fails = True
        with self.assertRaises(subprocess.CalledProcessError):
            self.prepare()
        self.assertIn("--dry-run", self.cargo_commands[-1])
        self.assertFalse((self.args.output / "registry-validation.json").exists())


if __name__ == "__main__":
    unittest.main()
