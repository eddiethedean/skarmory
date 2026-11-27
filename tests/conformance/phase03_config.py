"""Port the dead-code assertion of the pinned malformed-config test."""

import ast
from pathlib import Path
from types import MethodType

SOURCE = "test/test_analyzer.py"
FUNCTION = "test_analyze_malformed_pyproject_config_does_not_suppress_findings"
CASE = f"{SOURCE}::TestAnalyze::{FUNCTION}"
PREREQUISITE = "installed-sdk-malformed-config-recovery"
REQUIRED_ASSERTION = f"{SOURCE}:2055"
OMITTED_ASSERTION = f"{SOURCE}:2056"
PROJECT_CONFIG = b'[tool.skylos]\nignore = 1\ncomplexity = "boom"\nmax_args = false'
PYTHON_SOURCE = b'def unused_func(a, b, c, d, e, f):\n    return eval("1+1")\n'


def validate_scope(case):
    if (
        case["source"] != SOURCE
        or case["assertions"] != [REQUIRED_ASSERTION]
        or case["omitted_assertions"] != [OMITTED_ASSERTION]
        or PREREQUISITE not in case["prerequisites"]
    ):
        raise ValueError("The malformed-config port requires the original dead-code assertion and its exact scope")


def candidate_test(original):
    """Execute the original test body with the independent danger assertion omitted."""
    filename = original.__code__.co_filename
    tree = ast.parse(Path(filename).read_bytes(), filename=filename)
    [owner] = [
        node for node in tree.body if isinstance(node, ast.ClassDef) and node.name == "TestAnalyze"
    ]
    [test] = [
        node for node in owner.body if isinstance(node, ast.FunctionDef) and node.name == FUNCTION
    ]
    [expected] = ast.parse("def test(self): pass").body
    if (
        test.lineno != original.__code__.co_firstlineno
        or test.decorator_list
        or test.returns is not None
        or ast.dump(test.args) != ast.dump(expected.args)
    ):
        raise ValueError("The pinned malformed-config test changed its function declaration")
    dead_code, danger = test.body[-2:]
    for assertion, expected, line in (
        (dead_code, 'assert result["unused_functions"]', REQUIRED_ASSERTION),
        (
            danger,
            'assert "SKY-D201" in {f.get("rule_id") for f in result.get("danger", [])}',
            OMITTED_ASSERTION,
        ),
    ):
        [expected_node] = ast.parse(expected).body
        if ast.dump(assertion) != ast.dump(expected_node) or f"{SOURCE}:{assertion.lineno}" != line:
            raise ValueError("The pinned malformed-config assertions changed")

    test.body.pop()
    namespace = {}
    unit = ast.Module(body=[test], type_ignores=[])
    exec(compile(unit, filename, "exec", optimize=0), original.__globals__, namespace)
    return MethodType(namespace[FUNCTION], original.__self__)


def candidate_options(path, options):
    project = Path(path)
    if (project / "pyproject.toml").read_bytes() != PROJECT_CONFIG or (
        project / "app.py"
    ).read_bytes() != PYTHON_SOURCE:
        raise ValueError("The malformed-config test must scan the original project files")
    if options != {"conf": 0, "enable_danger": True, "grep_verify": False}:
        raise ValueError("The pinned malformed-config scan options changed")
    return {"conf": options["conf"], "grep_verify": options["grep_verify"]}


def verify_recovery(path, report):
    config = (Path(path) / "pyproject.toml").resolve()
    expected_warnings = {
        f"{config}: ignored malformed tool.skylos.ignore, expected an array of rule IDs",
        *(f"{config}: ignored tool.skylos.{key}, a quality threshold outside the dead-code profile"
          for key in ("complexity", "max_args")),
    }
    if (
        report["coverage"]["status"] != "complete"
        or report["selected_paths"] != ["app.py"]
        or report["config_sources"] != [str(config)]
        or set(report["config_warnings"]) != expected_warnings
    ):
        raise ValueError("The installed SDK did not prove recovery on the original configuration")
