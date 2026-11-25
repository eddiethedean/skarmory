"""Checks for the installed SDK and its source-only failure mode."""

import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
from tempfile import TemporaryDirectory
import unittest

import skarmory
from skarmory.compat import skylos


FIXTURES = Path(__file__).resolve().parents[1] / "conformance" / "fixtures"
NATIVE_REQUIRED = os.environ.get("SKARMORY_REQUIRE_NATIVE") == "1"
NATIVE_PRESENT = importlib.util.find_spec("skarmory._native") is not None


class SdkTests(unittest.TestCase):
    @unittest.skipIf(NATIVE_PRESENT or NATIVE_REQUIRED, "source-only check")
    def test_source_only_scan_raises(self):
        with self.assertRaises(skarmory.AnalysisUnavailableError):
            skarmory.analyze(FIXTURES / "reachable")
        with self.assertRaises(skarmory.AnalysisUnavailableError):
            skylos.analyze(FIXTURES / "reachable")

    @unittest.skipUnless(NATIVE_PRESENT or NATIVE_REQUIRED, "requires an installed wheel")
    def test_native_reports(self):
        from skarmory import _native

        self.assertEqual(_native.__version__, skarmory.__version__)
        report = skarmory.analyze(FIXTURES / "reachable", phase0=True)
        self.assertEqual(report["profile"], "phase0-python311-flat-application")
        self.assertEqual(report["coverage"]["status"], "complete")
        self.assertEqual(
            [(item["path"], item["symbol"]) for item in report["findings"]],
            [("main.py", "dead_left"), ("main.py", "dead_right"), ("other.py", "work")],
        )
        incomplete = skarmory.analyze(FIXTURES / "invalid")
        self.assertEqual(incomplete["schema_version"], 2)
        self.assertEqual(incomplete["coverage"]["status"], "incomplete")
        self.assertEqual(incomplete["coverage"]["issues"][0]["kind"], "parse_error")
        self.assertEqual(incomplete["findings"], [])

    @unittest.skipUnless(NATIVE_PRESENT or NATIVE_REQUIRED, "requires an installed wheel")
    @unittest.skipUnless(os.name == "posix", "requires POSIX filesystem encoding")
    def test_native_preserves_surrogateescaped_path(self):
        with TemporaryDirectory() as directory:
            path = Path(directory) / os.fsdecode(b"source-\xff")
            if sys.platform == "linux":
                path.mkdir()
                (path / "main.py").write_text("pass\n")
                expected_status = "complete"
                expected_exit = 0
            else:
                expected_status = "incomplete"
                expected_exit = 2

            report = skarmory.analyze(path)
            self.assertEqual(report["coverage"]["status"], expected_status)
            if cli := os.environ.get("SKARMORY_TEST_CLI"):
                result = subprocess.run([cli, path], capture_output=True)
                self.assertEqual(result.returncode, expected_exit)
                self.assertEqual(report, json.loads(result.stdout))

    @unittest.skipUnless(os.environ.get("SKARMORY_TEST_CLI"), "requires an installed CLI")
    def test_sdk_equals_cli(self):
        for fixture in sorted(FIXTURES.iterdir()):
            with self.subTest(fixture=fixture.name):
                result = subprocess.run(
                    [os.environ["SKARMORY_TEST_CLI"], fixture], capture_output=True
                )
                self.assertIn(result.returncode, (0, 2))
                self.assertEqual(skarmory.analyze(fixture), json.loads(result.stdout))

    @unittest.skipUnless(NATIVE_PRESENT or NATIVE_REQUIRED, "requires an installed wheel")
    def test_package_preview_options_and_worker_equivalence(self):
        with TemporaryDirectory() as directory:
            project = Path(directory)
            root = project / "src"
            package = root / "pkg"
            package.mkdir(parents=True)
            (package / "__init__.py").write_text(
                'from .impl import public\n__all__ = ["public"]\n', encoding="utf-8"
            )
            (package / "impl.py").write_text(
                "def helper():\n    pass\n\n"
                "def public():\n    helper()\n\n"
                "def dead():\n    pass\n",
                encoding="utf-8",
            )

            options = {
                "source_roots": [root],
                "policy": "library",
                "public_modules": ["pkg"],
            }
            serial = skarmory.analyze(project, **options)
            self.assertEqual(serial["coverage"]["status"], "complete", serial)
            self.assertEqual([item["symbol"] for item in serial["findings"]], ["dead"])
            self.assertEqual(serial, skarmory.analyze(project, workers=2, **options))
            cache = project / "facts"
            self.assertEqual(serial, skarmory.analyze(project, cache_dir=cache, **options))
            self.assertEqual(
                serial, skarmory.analyze(project, workers=2, cache_dir=cache, **options)
            )
            if cli := os.environ.get("SKARMORY_TEST_CLI"):
                result = subprocess.run(
                    [
                        cli, "--source-root", root, "--policy", "library",
                        "--public-module", "pkg", "--workers", "2", project,
                    ],
                    capture_output=True,
                    check=True,
                )
                self.assertEqual(serial, json.loads(result.stdout))

    @unittest.skipUnless(NATIVE_PRESENT or NATIVE_REQUIRED, "requires an installed wheel")
    def test_phase0_rejects_preview_options(self):
        with self.assertRaisesRegex(ValueError, "Phase 0 does not accept preview"):
            skarmory.analyze(FIXTURES / "reachable", phase0=True, workers=2)

    @unittest.skipUnless(NATIVE_PRESENT or NATIVE_REQUIRED, "requires an installed wheel")
    def test_compatibility_sdk_returns_json_string(self):
        with TemporaryDirectory() as directory:
            project = Path(directory)
            config = project / ".skylos.toml"
            config.write_text("root = true\n", encoding="utf-8")
            (project / "main.py").write_text(
                "def kept():\n    pass\nkept()\n\ndef unused():\n    pass\n",
                encoding="utf-8",
            )
            result = skylos.analyze(project, config_file=config, all_advice=False)
            self.assertIsInstance(result, str)
            report = json.loads(result)
            self.assertTrue(report["unused_functions"], report)
            self.assertEqual(report["analysis_errors"], [])
            if cli := os.environ.get("SKARMORY_TEST_CLI"):
                output = subprocess.run(
                    [
                        cli, project, "--profile", "skylos", "--json", "--no-grep-verify",
                        "--config-file", config, "--include-generated", "--confidence", "60",
                    ],
                    capture_output=True,
                    check=True,
                )
                self.assertEqual(report, json.loads(output.stdout))

            config.write_text("unknown_key = true\n", encoding="utf-8")
            with self.assertRaises(ValueError):
                skylos.analyze(project, config_file=config)

    @unittest.skipUnless(NATIVE_PRESENT or NATIVE_REQUIRED, "requires an installed wheel")
    def test_compatibility_cache_and_worker_equivalence(self):
        with TemporaryDirectory() as directory:
            root = Path(directory)
            project = root / "project"
            project.mkdir()
            cache = root / "facts"
            config = project / ".skylos.toml"
            config.write_text("root = true\n", encoding="utf-8")
            (project / "helper.py").write_text("def target():\n    pass\n", encoding="utf-8")
            entry = project / "main.py"
            options = {"config_file": config, "all_advice": False}

            for source, expected, cache_runs in (
                ("from helper import target\ntarget()\n", [], [(1, 0, 2), (2, 2, 0)]),
                ("pass\n", ["helper.target"], [(2, 1, 1)]),
            ):
                entry.write_text(source, encoding="utf-8")
                clean = json.loads(skylos.analyze(project, **options))
                self.assertEqual(clean["coverage"]["status"], "complete", clean)
                self.assertEqual([row["full_name"] for row in clean["unused_functions"]], expected)
                self.assertEqual(json.loads(skylos.analyze(project, workers=2, **options)), clean)
                for workers, hits, misses in cache_runs:
                    report = json.loads(skylos.analyze(project, workers=workers, cache_dir=cache, **options))
                    stats = report.pop("cache")
                    self.assertEqual((stats["hits"], stats["misses"]), (hits, misses))
                    self.assertEqual(report, clean)

                if cli := os.environ.get("SKARMORY_TEST_CLI"):
                    output = subprocess.check_output([
                        cli, project, "--profile", "skylos", "--json", "--config-file", config,
                        "--include-generated", "--confidence", "60", "--workers", "2", "--cache-dir", cache,
                    ])
                    report = json.loads(output)
                    self.assertEqual(report.pop("cache")["hits"], 2)
                    self.assertEqual(report, clean)

    def test_compatibility_rejects_unsupported_work(self):
        for key in ("grep_verify", "grep_cache", "enable_dependency_hallucinations"):
            with self.subTest(option=key), self.assertRaises(ValueError):
                skylos.analyze(FIXTURES / "reachable", **{key: True})
        with self.assertRaises(TypeError):
            skylos.analyze(FIXTURES / "reachable", enable_quality=True)

    @unittest.skipUnless(NATIVE_PRESENT or NATIVE_REQUIRED, "requires an installed wheel")
    def test_unused_dependency_quality_opt_in(self):
        with TemporaryDirectory() as directory:
            project = Path(directory)
            (project / "app.py").write_text("import yaml\n", encoding="utf-8")
            (project / "requirements.txt").write_text("pyyaml\npyserial\n", encoding="utf-8")
            (project / "pyproject.toml").write_text(
                '[project]\nname = "Example_Project"\n', encoding="utf-8"
            )
            options = {"profile": "skylos-0.3", "grep_verify": False}
            default = json.loads(skylos.analyze(project, **options))
            self.assertNotIn("quality", default)
            result = json.loads(skylos.analyze(project, enable_quality=True, **options))
            finding, = result["quality"]
            self.assertEqual((finding["rule_id"], finding["name"], finding["line"]), ("SKY-U005", "pyserial", 2))
            self.assertEqual(Path(finding["file"]), (project / "requirements.txt").resolve())
            receipt = result["analysis_summary"]["unused_dependency_analysis"]
            self.assertTrue(receipt["complete"])
            self.assertEqual(receipt["declarations"], {
                "dependencies": ["pyserial", "pyyaml"],
                "project_name": "example-project",
            })
            if cli := os.environ.get("SKARMORY_TEST_CLI"):
                result_cli = json.loads(subprocess.check_output([
                    cli, str(project), "--profile", "skylos-0.3", "--no-grep-verify", "--quality", "--all-advice",
                ]))
                self.assertEqual(result_cli, result)

    @unittest.skipUnless(NATIVE_PRESENT or NATIVE_REQUIRED, "requires an installed wheel")
    def test_grep_verification_sdk_profiles_and_cache_edits(self):
        with TemporaryDirectory() as directory:
            project = Path(directory) / "project"
            project.mkdir()
            cache = project.with_name("facts")
            (project / ".skylos.toml").write_text("root = true\n", encoding="utf-8")
            (project / "main.py").write_text("pass\n", encoding="utf-8")
            (project / "operations.py").write_text(
                "def launch_job():\n    return 1\n", encoding="utf-8"
            )
            jobs = project / "jobs.yml"
            jobs.write_text("handler_module: operations.py\n", encoding="utf-8")

            default_native = skarmory.analyze(project)
            self.assertEqual(
                [finding["symbol"] for finding in default_native["findings"]], ["launch_job"]
            )
            self.assertNotIn("grep_verification_evidence", default_native)
            native = skarmory.analyze(project, grep_verify=True)
            self.assertEqual(native["coverage"]["status"], "complete", native)
            self.assertEqual(native["findings"], [])
            self.assertEqual(
                [
                    (row["qualified_name"], row["path"], row["certainty"])
                    for row in native["grep_verification_evidence"]
                ],
                [("operations.launch_job", "jobs.yml", "possible")],
            )

            options = {"profile": "skylos-0.3", "all_advice": False, "trace_file": False}
            default_phase02 = json.loads(skylos.analyze(project, all_advice=False))
            disabled = json.loads(skylos.analyze(project, grep_verify=False, **options))
            for report in (default_phase02, disabled):
                self.assertEqual(report["coverage"]["status"], "complete", report)
                self.assertEqual(
                    [finding["full_name"] for finding in report["unused_functions"]],
                    ["operations.launch_job"],
                )

            enabled = json.loads(skylos.analyze(project, **options))
            self.assertEqual(enabled["coverage"]["status"], "complete", enabled)
            self.assertEqual(enabled["unused_functions"], [])
            self.assertEqual(json.loads(skylos.analyze(project, grep_cache=True, **options)), enabled)
            for expected_hits, workers in ((0, 1), (2, 2)):
                cached = json.loads(skylos.analyze(
                    project, grep_cache=True, cache_dir=cache, workers=workers, **options
                ))
                self.assertEqual(cached.pop("cache")["hits"], expected_hits)
                self.assertEqual(cached, enabled)

            jobs.write_text("handler_module: other_operations.py\n", encoding="utf-8")
            edited = json.loads(skylos.analyze(project, **options))
            self.assertEqual(edited["coverage"]["status"], "complete", edited)
            self.assertEqual(
                [finding["full_name"] for finding in edited["unused_functions"]],
                ["operations.launch_job"],
            )
            cached = json.loads(skylos.analyze(
                project, grep_cache=True, cache_dir=cache, workers=2, **options
            ))
            self.assertEqual(cached.pop("cache")["hits"], 2)
            self.assertEqual(cached, edited)

            if cli := os.environ.get("SKARMORY_TEST_CLI"):
                output = subprocess.check_output(
                    [cli, project, "--profile", "skylos-0.3", "--include-generated"]
                )
                self.assertEqual(json.loads(output), edited)

    @unittest.skipUnless(NATIVE_PRESENT or NATIVE_REQUIRED, "requires an installed wheel")
    def test_phase03_package_export_does_not_retain_same_name_local(self):
        # Independent control for the strict reference xfail at test/test_package_local_exports.py:60-81
        # in Skylos 87978e69adf91b3811e97b00c40aa39d7937cd94.
        with TemporaryDirectory() as directory:
            project = Path(directory)
            package = project / "pkg"
            package.mkdir()
            (package / "__init__.py").write_text(
                '__all__ = ["STATE", "run"]\n\n'
                'STATE = "public"\n\n'
                'def run():\n    STATE = "local"\n    return 0\n',
                encoding="utf-8",
            )
            report = json.loads(skylos.analyze(
                project, conf=60, profile="skylos-0.3", grep_verify=True, trace_file=False,
            ))
            self.assertEqual(report["coverage"]["status"], "complete", report)
            variables = {finding["full_name"] for finding in report["unused_variables"]}
            self.assertIn("pkg.run.STATE", variables)
            self.assertNotIn("pkg.STATE", variables)

    @unittest.skipUnless(NATIVE_PRESENT or NATIVE_REQUIRED, "requires an installed wheel")
    def test_phase03_package_function_namespace_reads_local_bindings(self):
        # SDK controls for the locals() and vars() variants at test/test_package_local_exports.py:126-138
        # in Skylos 87978e69adf91b3811e97b00c40aa39d7937cd94.
        with TemporaryDirectory() as directory:
            project = Path(directory)
            package = project / "pkg"
            package.mkdir()
            initializer = package / "__init__.py"
            for expression in ("locals()", "vars()", "source"):
                with self.subTest(expression=expression):
                    initializer.write_text(
                        "def evaluate(source):\n"
                        "    retained_value = 17\n"
                        f"    return {expression}\n",
                        encoding="utf-8",
                    )
                    report = json.loads(skylos.analyze(
                        project, conf=60, profile="skylos-0.3", grep_verify=True, trace_file=False,
                    ))
                    self.assertEqual(report["coverage"]["status"], "complete", report)
                    variables = {finding["full_name"] for finding in report["unused_variables"]}
                    if expression == "source":
                        self.assertIn("pkg.evaluate.retained_value", variables)
                    else:
                        self.assertNotIn("pkg.evaluate.retained_value", variables)

    @unittest.skipUnless(NATIVE_PRESENT or NATIVE_REQUIRED, "requires an installed wheel")
    def test_phase03_bounded_receivers_allocators_and_lazy_tables(self):
        with TemporaryDirectory() as directory:
            project = Path(directory)
            package = project / "pkg"
            package.mkdir()
            (package / "__init__.py").write_text(
                "import importlib\n"
                "_LAZY = {'work': 'pkg.provider'}\n"
                "__all__ = ['work']\n"
                "def __getattr__(name):\n"
                "    if name in _LAZY:\n"
                "        return getattr(importlib.import_module(_LAZY[name]), name)\n"
                "    raise AttributeError(name)\n",
                encoding="utf-8",
            )
            (package / "provider.py").write_text(
                "def work():\n    return helper()\n"
                "def helper():\n    return 1\n"
                "def unused_provider():\n    return 2\n",
                encoding="utf-8",
            )
            (project / "main.py").write_text(
                "class Requested:\n"
                "    def __new__(cls):\n"
                "        return Actual()\n"
                "    def deliver(self):\n"
                "        return 0\n"
                "class Actual:\n"
                "    def deliver(self):\n"
                "        return 1\n"
                "Requested().deliver()\n"
                "class Parent:\n"
                "    def run(self):\n"
                "        return self.step()\n"
                "    def step(self):\n"
                "        return 0\n"
                "class Child(Parent):\n"
                "    def step(self):\n"
                "        return 1\n"
                "Child().run()\n"
                "class First:\n"
                "    def act(self):\n"
                "        return 1\n"
                "class Second:\n"
                "    def act(self):\n"
                "        return 2\n"
                "receiver = First() if choose else Second()\n"
                "receiver.act()\n",
                encoding="utf-8",
            )

            report = json.loads(skylos.analyze(
                project, conf=0, profile="skylos-0.3", grep_verify=False, trace_file=False,
            ))
            self.assertEqual(report["coverage"]["status"], "complete", report)
            unused = {finding["full_name"] for finding in report["unused_functions"]}
            self.assertIn("main.Requested.deliver", unused)
            self.assertIn("main.Parent.step", unused)
            self.assertIn("pkg.provider.unused_provider", unused)
            for retained in (
                "main.Actual.deliver",
                "main.Child.step",
                "main.First.act",
                "main.Second.act",
                "pkg.provider.work",
                "pkg.provider.helper",
            ):
                self.assertNotIn(retained, unused)

    @unittest.skipUnless(NATIVE_PRESENT or NATIVE_REQUIRED, "requires an installed wheel")
    def test_phase03_trace_selection_and_cli_parity(self):
        with TemporaryDirectory() as directory:
            project = Path(directory)
            module = project / "app.py"
            module.write_text(
                "def primary():\n    return helper()\n"
                "def secondary():\n    return 2\n"
                "def helper():\n    return 3\n",
                encoding="utf-8",
            )
            for path, function, line in (
                (".skylos_trace", "primary", 1),
                ("other.json", "secondary", 3),
            ):
                project.joinpath(path).write_text(
                    json.dumps({
                        "version": 1,
                        "calls": [{"file": str(module), "function": function, "line": line}],
                    }),
                    encoding="utf-8",
                )
            for options, flags, expected in (
                ({}, [], {"secondary"}),
                ({"trace_file": Path("other.json")}, ["--trace-file", "other.json"], {"primary", "helper"}),
                ({"trace_file": False}, ["--no-trace"], {"primary", "secondary", "helper"}),
            ):
                with self.subTest(options=options):
                    report = json.loads(skylos.analyze(
                        project, profile="skylos-0.3", conf=0, grep_verify=False, **options
                    ))
                    self.assertEqual(report["coverage"]["status"], "complete")
                    self.assertEqual({row["name"] for row in report["unused_functions"]}, expected)
                    if options.get("trace_file") is not False:
                        self.assertEqual(report["runtime_evidence"][0]["provenance"], "unverified")
                    if cli := os.environ.get("SKARMORY_TEST_CLI"):
                        raw = subprocess.check_output([
                            cli, project, "--profile", "skylos-0.3", "--no-grep-verify",
                            "--include-generated", "--all-advice", "--confidence", "0", *flags,
                        ])
                        self.assertEqual(report, json.loads(raw))

            phase02 = json.loads(skylos.analyze(project, conf=0))
            self.assertEqual(len(phase02["unused_functions"]), 3)
            self.assertNotIn("runtime_evidence", phase02)
            with self.assertRaises(TypeError):
                skylos.analyze(project, trace_file=Path("other.json"))

    @unittest.skipUnless(NATIVE_PRESENT or NATIVE_REQUIRED, "requires an installed wheel")
    def test_phase03_git_impact_baseline_gate_and_json_ci(self):
        with TemporaryDirectory() as directory:
            project = Path(directory)
            (project / "app.py").write_text(
                "def legacy():\n    return 1\n\ndef old():\n    return 2\n",
                encoding="utf-8",
            )
            caller = project / "caller.py"
            caller.write_text("from app import legacy\nlegacy()\n", encoding="utf-8")
            for args in (
                ["init", "-q"],
                ["add", "."],
                ["commit", "-qm", "Initial"],
            ):
                subprocess.run(
                    [
                        "git", "-c", "user.name=Fixture", "-c", "user.email=fixture@example.com",
                        "-c", "commit.gpgsign=false", *args,
                    ],
                    cwd=project, check=True, capture_output=True,
                )

            options = {"profile": "skylos-0.3", "grep_verify": False}
            baseline = project / ".skylos" / "baseline.json"
            saved = json.loads(skylos.analyze(project, write_baseline=baseline, **options))
            self.assertEqual(saved["analysis_summary"]["baseline"]["status"], "written")
            caller.write_text("print('done')\n", encoding="utf-8")

            report = json.loads(skylos.analyze(
                project, diff="HEAD", diff_mode="impact", strict=True,
                baseline=True, output_format="json-ci", **options,
            ))
            self.assertEqual([row["name"] for row in report["unused_functions"]], ["legacy"])
            self.assertEqual(report["unused_functions"][0]["change_status"], "introduced")
            self.assertTrue(report["unused_functions"][0]["dead_code_evidence"])
            self.assertEqual(report["gate"]["exit_code"], 1)
            self.assertEqual(report["analysis_summary"]["baseline"]["matched_count"], 1)
            self.assertNotIn("definitions", report)
            self.assertNotIn("dead_code_evidence", report)
            if cli := os.environ.get("SKARMORY_TEST_CLI"):
                result = subprocess.run(
                    [
                        cli, project, "--profile", "skylos-0.3", "--no-grep-verify",
                        "--include-generated", "--all-advice", "--confidence", "60",
                        "--diff", "HEAD", "--diff-mode", "impact", "--strict",
                        "--baseline", "--format", "json-ci",
                    ],
                    capture_output=True,
                )
                self.assertEqual(result.returncode, 1, result.stderr)
                self.assertEqual(report, json.loads(result.stdout))

    @unittest.skipUnless(NATIVE_PRESENT or NATIVE_REQUIRED, "requires an installed wheel")
    def test_phase03_javascript_file_reachability_and_cli_parity(self):
        with TemporaryDirectory() as directory:
            project = Path(directory)
            (project / "app.py").write_text("def unused():\n    return 1\n", encoding="utf-8")
            baseline = json.loads(skylos.analyze(project, profile="skylos-0.3", conf=0))
            self.assertEqual(baseline["coverage"]["status"], "complete")
            self.assertEqual(baseline["unused_functions"][0]["name"], "unused")
            for name, text in (
                ("package.json", '{"main":"main.ts"}'),
                ("main.ts", "import './barrel.ts';"),
                ("barrel.ts", "export { live } from './provider.ts';"),
                ("provider.ts", "export const live = 1;"),
                ("unused.js", "console.log('unused');"),
            ):
                (project / name).write_text(text, encoding="utf-8")

            for exit_code, status, expected in (
                (0, "complete", [("unused.js", "SKY-E003")]),
                (2, "incomplete", []),
            ):
                with self.subTest(status=status):
                    if status == "incomplete":
                        (project / "provider.ts").unlink()
                    default = json.loads(skylos.analyze(project, profile="skylos-0.3", conf=0))
                    self.assertEqual(default, baseline)
                    report = json.loads(skylos.analyze(
                        project, profile="skylos-0.3", experimental_javascript=True,
                    ))
                    self.assertEqual(report["coverage"]["status"], status)
                    self.assertEqual(
                        [(row["file"], row["rule_id"]) for row in report["unused_files"]],
                        expected,
                    )
                    if cli := os.environ.get("SKARMORY_TEST_CLI"):
                        output = subprocess.run([
                            cli, project, "--profile", "skylos-0.3", "--all-advice",
                            "--experimental-javascript", "--include-generated", "--confidence", "60",
                        ], capture_output=True)
                        self.assertEqual(output.returncode, exit_code, output.stderr)
                        self.assertEqual(report, json.loads(output.stdout))

    @unittest.skipUnless(NATIVE_PRESENT or NATIVE_REQUIRED, "requires an installed wheel")
    @unittest.skipUnless(os.name == "posix", "requires POSIX symlink creation")
    def test_phase03_default_trace_rejects_symlink(self):
        with TemporaryDirectory() as directory:
            project = Path(directory)
            project.joinpath("app.py").write_text("def work():\n    return 1\n", encoding="utf-8")
            trace = project / "observations.json"
            trace.write_text(
                '{"version":1,"calls":[{"file":"app.py","function":"work","line":1}]}',
                encoding="utf-8",
            )
            project.joinpath(".skylos_trace").symlink_to(trace)
            report = json.loads(skylos.analyze(project, profile="skylos-0.3", grep_verify=False))
            self.assertEqual(report["coverage"]["status"], "complete")
            self.assertEqual(report["runtime_evidence_issues"][0]["kind"], "runtime_evidence_rejected")
            self.assertEqual([row["name"] for row in report["unused_functions"]], ["work"])

    @unittest.skipUnless(NATIVE_PRESENT or NATIVE_REQUIRED, "requires an installed wheel")
    def test_runtime_collection_can_be_imported_by_the_sdk(self):
        from runpy import run_path

        from skarmory.runtime import collect_trace

        with TemporaryDirectory() as directory:
            project = Path(directory)
            module = project / "app.py"
            module.write_text(
                "def called():\n    return 1\ndef untouched():\n    return 2\n",
                encoding="utf-8",
            )
            namespace = run_path(str(module.resolve()))
            with collect_trace(project, "taken.json"):
                self.assertEqual(namespace["called"](), 1)
            report = json.loads(skylos.analyze(
                project, profile="skylos-0.3", grep_verify=False, trace_file="taken.json"
            ))
            self.assertEqual(report["coverage"]["status"], "complete")
            self.assertEqual([row["name"] for row in report["unused_functions"]], ["untouched"])
            self.assertEqual(report["runtime_evidence"][0]["provenance"], "unverified")
