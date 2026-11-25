import tempfile
import unittest
from pathlib import Path

from phase03_framework_corpus import project_method_names


class CorpusProjectionTests(unittest.TestCase):
    def test_projects_matching_source_occurrences_and_keeps_finding_identity(self):
        with tempfile.TemporaryDirectory() as directory:
            source = Path(directory) / "model.py"
            source.write_text(
                "class Payload:\n"
                "    def clean(self): pass\n"
                "class Extra:\n"
                "    async def clean(self): pass\n"
                "def clean(): pass\n",
                encoding="utf-8",
            )
            rows = [
                {
                    "file": "model.py",
                    "line": line,
                    "column": column,
                    "full_name": name,
                    "simple_name": "clean",
                    "name": "clean",
                }
                for line, column, name in (
                    (2, 9, "model.Payload.clean"),
                    (4, 15, "model.Extra.clean"),
                    (5, 5, "model.clean"),
                    (4, 15, "other.Payload.clean"),
                )
            ]
            report = {
                "definitions": {
                    str(index): {**row, "name": row["full_name"], "type": "function"}
                    for index, row in enumerate(rows[:3])
                },
                "unused_functions": rows,
                "coverage": {"status": "incomplete"},
            }
            projected = project_method_names(report, source)
            self.assertEqual(projected["coverage"], report["coverage"])
            self.assertEqual(
                [row["name"] for row in projected["unused_functions"]],
                ["Payload.clean", "Extra.clean", "clean", "clean"],
            )
            for original, result in zip(rows, projected["unused_functions"], strict=True):
                self.assertEqual(result["full_name"], original["full_name"])
                self.assertEqual(result["simple_name"], original["simple_name"])
