"""Controls for the pinned unused dependency case adapter."""

import json
import tempfile
import unittest
from pathlib import Path

from phase03_dependencies import DYNAMIC_IMPORT, VIBE_DEPENDENCIES, unused_dependencies


class DependencyPortTests(unittest.TestCase):
    def setUp(self):
        folder = tempfile.TemporaryDirectory()
        self.addCleanup(folder.cleanup)
        self.root = Path(folder.name).resolve()
        self.source = self.root / "app.py"
        self.source.write_text("import requests\n", encoding="utf-8")

    def scan(self, selected, status="complete"):
        def analyze(root, options):
            self.assertEqual(root, self.root)
            self.assertEqual(options, {"select": ["SKY-U005"]})
            return json.dumps({
                "selected_paths": selected,
                "coverage": {"status": status},
                "analysis_summary": {"unused_dependency_analysis": {
                    "rule_id": "SKY-U005", "status": status, "complete": status == "complete",
                }},
                "quality": [],
            })

        return analyze

    def test_unexpected_source_is_not_credited(self):
        with self.assertRaisesRegex(ValueError, "different fixture sources"):
            unused_dependencies(
                VIBE_DEPENDENCIES + "test_no_manifest_no_findings",
                self.scan(["app.py", "extra.py"]), self.root, [self.source],
            )

    def test_only_the_dynamic_import_node_expects_an_incomplete_receipt(self):
        scan = self.scan(["app.py"], status="incomplete")
        with self.assertRaisesRegex(ValueError, "coverage differs"):
            unused_dependencies(
                VIBE_DEPENDENCIES + "test_no_manifest_no_findings", scan, self.root, [self.source],
            )
        coverage = {}
        self.assertEqual(
            unused_dependencies(DYNAMIC_IMPORT, scan, self.root, [self.source], coverage=coverage),
            [],
        )
        self.assertIs(coverage["complete"], False)
