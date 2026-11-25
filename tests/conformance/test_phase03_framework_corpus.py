import tempfile
import unittest
from pathlib import Path

from phase03_framework_corpus import check_runtime_corpus, project_method_names
from phase03_runtime_corpus import CONTROLS, evaluate_runtime_control


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


class RuntimeCorpusTests(unittest.TestCase):
    def test_rejected_route_requires_coverage_and_no_registration(self):
        control = CONTROLS["starlette-add-route"]
        diagnostic = {"path": "app.py", "line": 10, "kind": "unresolved_registration"}
        report = {
            "selected_paths": ["app.py"],
            "coverage": {
                "status": "incomplete", "selected_files": 1, "extracted_files": 1,
                "issues": [diagnostic],
            },
            "definitions": {"homepage": {"name": "app.homepage"}},
            "dead_code_evidence": {"symbols": []},
        }
        self.assertEqual(evaluate_runtime_control(report, ["app.py"], control), [])

        report["dead_code_evidence"]["symbols"] = [{
            "qualified_name": "app.homepage",
            "evidence": [{"kind": "starlette_route", "certainty": "possible"}],
        }]
        report["coverage"].update(status="complete", issues=[], extracted_files=0)
        failures = evaluate_runtime_control(report, ["app.py"], control)
        self.assertEqual(len(failures), 4)
        self.assertTrue(any("unexpected starlette_route" in reason for reason in failures))

    def test_runtime_score_is_independent_of_original_assertions(self):
        control = CONTROLS["django-management-command"]
        with tempfile.TemporaryDirectory() as directory:
            source = Path(directory) / "django_management_command.py"
            source.touch()
            report = {
                "selected_paths": [source.name],
                "coverage": {
                    "status": "complete", "selected_files": 1, "extracted_files": 1,
                    "issues": [],
                },
                "analysis_errors": [],
                "definitions": {},
                "dead_code_evidence": {"symbols": []},
                "unused_functions": [],
                "unused_classes": [],
            }
            for bucket, name in control.unused:
                report[bucket].append({
                    "full_name": name, "file": source.name, "line": 1, "column": 1,
                })

            class Corpus:
                analyze = None

                def run_manifest(self, manifest, selected_cases):
                    (case_id,) = selected_cases
                    self.analyze(
                        str(source), enable_ai_defects=False, enable_danger=False,
                        enable_sca=False, enable_secrets=False,
                    )
                    return {"cases": [{"id": case_id, "scored": True, "failures": ["legacy"]}]}

            corpus = Corpus()
            selection = ["django-management-command"]
            passed = check_runtime_corpus(corpus, None, selection, lambda *_: report)
            self.assertEqual(passed["pass_count"], 1)
            report["unused_classes"].clear()
            failed = check_runtime_corpus(corpus, None, selection, lambda *_: report)
            self.assertEqual(failed["pass_count"], 0)
