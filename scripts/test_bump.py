"""Release-version consistency checks using isolated source fixtures."""
import tempfile
import tomllib
import unittest
from pathlib import Path
from unittest.mock import patch

import bump


class SiteVersionTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.root_patch = patch.object(bump, "ROOT", self.root)
        self.root_patch.start()
        self.addCleanup(self.root_patch.stop)
        (self.root / "Cargo.toml").write_text(
            '[package]\nname = "zmem-manager"\nversion = "0.1.1"\n\n'
            '[lib]\nname = "zmem"\n'
        )
        (self.root / "Cargo.lock").write_text(
            'version = 4\n\n[[package]]\nname = "zmem-manager"\nversion = "0.1.1"\n'
        )
        (self.root / "site").mkdir()
        (self.root / "site/version.js").write_text('window.ZMEM_VERSION = "0.1.1";\n')

    def test_bump_updates_stale_html_and_script_together(self):
        original = (
            '<span data-version>v0.1.0</span>\n'
            '<a data-version class="version" href="/releases">v0.1.0</a>\n'
            '<p>Historical release v0.1.0</p>\n'
        )
        index = self.root / "site/index.html"
        index.write_text(original)

        changed = bump.updated_files("0.1.2")

        self.assertEqual(changed[index], original.replace(
            '>v0.1.0</', '>v0.1.2</'
        ))
        self.assertEqual(changed[self.root / "site/version.js"],
                         'window.ZMEM_VERSION = "0.1.2";\n')
        self.assertEqual(tomllib.loads(changed[self.root / "Cargo.toml"])["package"]["version"], "0.1.2")
        self.assertEqual(tomllib.loads(changed[self.root / "Cargo.lock"])["package"][0]["version"], "0.1.2")
        self.assertEqual(index.read_text(), original)

    def test_missing_html_version_marker_rejects_the_bump(self):
        (self.root / "site/index.html").write_text('<span data-version-label>v0.1.0</span>\n')
        with self.assertRaisesRegex(ValueError, "no data-version fallback"):
            bump.updated_files("0.1.2")
        self.assertEqual(tomllib.loads((self.root / "Cargo.toml").read_text())["package"]["version"], "0.1.1")


if __name__ == "__main__":
    unittest.main()
