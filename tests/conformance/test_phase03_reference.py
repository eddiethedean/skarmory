"""Review boundaries for the downstream Phase 0.3 fixture reference."""

import hashlib
import json
import os
from pathlib import Path
import sys
from tempfile import TemporaryDirectory
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "scripts"))
from phase03_reference import COMMIT_ENV, COMMIT_MESSAGE, CORPUS, git, materialize


class Phase03ReferenceTests(unittest.TestCase):
    def setUp(self):
        temporary = TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.checkout = self.root / "skylos"
        self.checkout.mkdir()
        git(self.checkout, "init", "-q")
        self.write(CORPUS, json.dumps({
            "schema_version": 1,
            "cases": [{"id": "route", "path": "fixtures/app.py", "expect": {"alive": ["home"]}}],
        }))
        self.write("corpus/fixtures/app.py", "app.add_route('/', endpoint=home)\n")
        self.write("skylos/analyzer.py", "version = '5.7.0'\n")
        git(self.checkout, "add", "corpus", "skylos")
        git(self.checkout, "-c", "commit.gpgSign=false", "commit", "-qm", "Frozen fixture",
            env={**os.environ, **COMMIT_ENV})
        self.original = git(self.checkout, "rev-parse", "HEAD").decode().strip()
        git(self.checkout, "switch", "--detach", self.original)

    def write(self, name, content):
        path = self.checkout / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(content, encoding="utf-8")

    def make_patch(self, changed, *, digest_override=None):
        for name, content in changed.items():
            self.write(name, content)
        git(self.checkout, "add", *changed)
        patch = git(self.checkout, "diff", "--cached", "--binary")
        tree = git(self.checkout, "write-tree").decode().strip()
        revision = git(self.checkout, "commit-tree", tree, "-p", self.original,
                       env={**os.environ, **COMMIT_ENV}, data=COMMIT_MESSAGE).decode().strip()
        git(self.checkout, "restore", "--staged", "--worktree", *changed)
        (self.root / "fixtures.patch").write_bytes(patch)
        manifest = self.root / "skylos.toml"
        digest = digest_override or hashlib.sha256(patch).hexdigest()
        manifest.write_text(
            f'[reference]\ngit_revision = "{self.original}"\n'
            f'[phase03_reference]\ngit_revision = "{revision}"\n'
            f'patch = "fixtures.patch"\npatch_sha256 = "{digest}"\n'
            '[[phase03_reference.corrections]]\nid = "route"\n',
            encoding="utf-8",
        )
        return manifest, revision

    def test_materializes_only_reviewed_fixture(self):
        path = "corpus/fixtures/app.py"
        manifest, revision = self.make_patch({path: "app.add_route('/', home)\n"})
        self.assertEqual(materialize(self.checkout, manifest), revision)
        self.assertEqual(git(self.checkout, "rev-parse", "HEAD").decode().strip(), revision)
        self.assertEqual(git(self.checkout, "diff", "--name-only", self.original, revision).decode().splitlines(), [path])
        self.assertEqual(git(self.checkout, "status", "--porcelain"), b"")

    def test_rejects_patch_hash_mismatch_without_checkout_changes(self):
        manifest, _ = self.make_patch({"corpus/fixtures/app.py": "pass\n"}, digest_override="0" * 64)
        with self.assertRaisesRegex(ValueError, "SHA-256 mismatch"):
            materialize(self.checkout, manifest)
        self.assert_frozen()

    def test_rejects_changed_assertions_without_checkout_changes(self):
        manifest, _ = self.make_patch({
            CORPUS: json.dumps({
                "schema_version": 1,
                "cases": [{"id": "route", "path": "fixtures/app.py", "expect": {"alive": []}}],
            }),
            "corpus/fixtures/app.py": "pass\n",
        })
        with self.assertRaisesRegex(ValueError, "more than the fixture path"):
            materialize(self.checkout, manifest)
        self.assert_frozen()

    def test_rejects_analyzer_changes_even_with_matching_hash(self):
        manifest, _ = self.make_patch({
            "corpus/fixtures/app.py": "pass\n",
            "skylos/analyzer.py": "version = 'changed'\n",
        })
        with self.assertRaisesRegex(ValueError, "do not match the reviewed corpus"):
            materialize(self.checkout, manifest)
        self.assert_frozen()

    def assert_frozen(self):
        self.assertEqual(git(self.checkout, "rev-parse", "HEAD").decode().strip(), self.original)
        self.assertEqual(git(self.checkout, "status", "--porcelain"), b"")
