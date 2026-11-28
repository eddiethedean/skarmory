import json
from pathlib import Path
from tempfile import TemporaryDirectory
import unittest

from phase03_factory import (
    FACTORY_CASES,
    FACTORY_METHOD,
    PROXY_CASES,
    PROXY_OWNER,
    PROXY_TARGET,
    TARGET,
    verify_dormant_proxy,
    verify_factory_method,
)


def report(executes=False, *, factory=False):
    return {
        "coverage": {"status": "complete"},
        "selected_paths": ["app.py", "provider.py"],
        "analysis_summary": {
            "grep_verify": {"enabled": False},
            "dead_code_liveness": {"rescued": [{"name": TARGET, "reason": FACTORY_METHOD}] if factory else []},
        },
        "dead_code_evidence": {"symbols": [{
            "qualified_name": TARGET,
            "evidence": ([{"kind": "direct_call", "certainty": "certain"}] if executes else [])
            + ([{"kind": FACTORY_METHOD, "certainty": "certain"}] if factory else []),
        }]},
    }


class FactoryMethodProofTests(unittest.TestCase):
    def test_original_and_paired_controls_must_disagree_on_execution(self):
        case_id = next(case for case, executes in FACTORY_CASES.items() if not executes)
        options = {"conf": 0, "grep_verify": False, "grep_cache": False}
        with TemporaryDirectory() as directory:
            path = Path(directory)
            provider = path / "provider.py"
            provider.write_text("class Provider: pass\n", encoding="utf-8")
            observed = []

            def scan(project, settings):
                self.assertNotEqual(project, path)
                self.assertEqual(settings, options)
                self.assertEqual((project / "provider.py").read_bytes(), provider.read_bytes())
                runs = "RESULT = consume()" in (project / "app.py").read_text(encoding="utf-8")
                observed.append(runs)
                return json.dumps(report(runs, factory=runs))

            self.assertTrue(verify_factory_method(case_id, path, options, report(), scan))
            self.assertEqual(observed, [False, True])
            self.assertFalse(verify_factory_method(
                case_id, path, options, report(True, factory=True), scan,
            ))
            self.assertFalse(verify_factory_method(
                case_id, path, options, report(), lambda *_: json.dumps(report()),
            ))


    def test_dormant_proxy_requires_both_native_controls(self):
        case_id = next(iter(PROXY_CASES))
        options = {"conf": 0, "grep_verify": False, "grep_cache": False}

        def proxy_report(*, invoked=False, uncertain=False):
            unused = [] if uncertain else [{"full_name": PROXY_OWNER}]
            issues = [{"path": "commands.py", "kind": "unresolved_call"}] if uncertain else []
            return {
                "selected_paths": ["app.py", "commands.py"],
                "coverage": {"status": "incomplete" if uncertain else "complete", "issues": issues},
                "analysis_summary": {"grep_verify": {"enabled": False}},
                "dead_code_evidence": {
                    "symbols": [{
                        "qualified_name": PROXY_TARGET,
                        "evidence": [{"kind": "direct_call", "certainty": "certain"}],
                    }] if invoked else [],
                },
                "unused_functions": unused,
            }

        with TemporaryDirectory() as directory:
            path = Path(directory)
            (path / "app.py").write_text("app = None\n", encoding="utf-8")
            (path / "commands.py").write_text("def retired_shell(): pass\n", encoding="utf-8")
            observed = []

            def scan(project, settings):
                self.assertNotEqual(project, path)
                self.assertEqual(settings, options)
                action = (project / "commands.py").read_text(encoding="utf-8").splitlines()[-1]
                observed.append(action)
                return json.dumps(
                    proxy_report(uncertain=action == "retired_shell()",
                                 invoked=action == "selected_app.make_shell_context()")
                )

            self.assertTrue(verify_dormant_proxy(case_id, path, options, proxy_report(), scan))
            self.assertEqual(observed, ["retired_shell()", "selected_app.make_shell_context()"])
            self.assertFalse(verify_dormant_proxy(case_id, path, options, proxy_report(invoked=True), scan))
            self.assertFalse(verify_dormant_proxy(
                case_id, path, options, proxy_report(), lambda *_: json.dumps(proxy_report()),
            ))


if __name__ == "__main__":
    unittest.main()
