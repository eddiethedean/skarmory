"""Adapt pinned Python unused dependency cases to the installed SDK."""

import json
from pathlib import Path

UNUSED_DEPENDENCY_PREREQUISITE = "native-sdk-unused-dependencies"
UNUSED_DEPENDENCY_SOURCES = {
    "test/test_requirements_includes_and_unused_deps.py",
    "test/test_setuptools_declarations.py",
    "test/test_vibe_rules.py",
}
FRAMEWORK = (
    "test/test_requirements_includes_and_unused_deps.py::test_framework_and_command_uses_count"
)
SETUP_PY = (
    "test/test_setuptools_declarations.py::test_setup_py_build_requirements_are_not_reported_unused"
)
VIBE_DEPENDENCIES = "test/test_vibe_rules.py::TestUnusedDependencies::"
PARENT_MANIFEST = VIBE_DEPENDENCIES + "test_manifest_inside_the_project_above_the_sources_is_used"
DYNAMIC_IMPORT = VIBE_DEPENDENCIES + "test_declared_and_imported_not_flagged"
DECLARATION_CASE = VIBE_DEPENDENCIES + "test_pyproject_comment_apostrophe_is_not_a_dependency"
DECLARATION_PREREQUISITES = {"pinned pyproject declaration names", "pinned project identity"}


def dependency_receipt(report, expected):
    receipt = report["analysis_summary"]["unused_dependency_analysis"]
    if (
        report["coverage"]["status"] != expected
        or receipt["rule_id"] != "SKY-U005"
        or receipt["status"] != expected
        or receipt["complete"] is not (expected == "complete")
    ):
        raise ValueError(f"candidate dependency coverage differs from the case contract: {receipt}")
    return receipt


def dependency_declarations(scan, root):
    """Project the native declaration inventory into the pinned helper's fields."""
    source_root = Path(root).resolve(strict=True)
    report = json.loads(scan(source_root, {"select": ["SKY-U005"]}))
    receipt = dependency_receipt(report, "complete")
    declarations = receipt.get("declarations")
    if not isinstance(declarations, dict):
        raise ValueError("candidate dependency scan did not expose its declaration inventory")
    dependencies = declarations.get("dependencies")
    project_name = declarations["project_name"]
    if (
        not isinstance(dependencies, list)
        or not all(isinstance(name, str) for name in dependencies)
        or len(dependencies) != len(set(dependencies))
        or not (project_name is None or isinstance(project_name, str))
    ):
        raise ValueError("candidate dependency declaration inventory has invalid fields")
    return set(dependencies), project_name


def unused_dependencies(case_id, scan, root, files, *, location_root=None, coverage=None):
    """Return the native rule results after proving the pinned source and report scope."""
    source_root = Path(root).resolve(strict=True)
    report_root = Path(location_root or root).resolve(strict=True)
    if report_root != source_root and case_id != PARENT_MANIFEST:
        raise ValueError("unexpected separate dependency report root")
    source_prefix = source_root.relative_to(report_root)
    selected = {
        (source_prefix / Path(file).resolve(strict=True).relative_to(source_root)).as_posix()
        for file in files
    }
    if not selected:
        raise ValueError("unused dependency fixture has no selected Python files")

    # Skylos reads the Sphinx config separately from the supplied Python files.
    # Skarmory models that same input through its Python configuration facts.
    if case_id == FRAMEWORK:
        selected.add("docs/conf.py")
    options = {"select": ["SKY-U005"]}
    if case_id == SETUP_PY:
        # Keep the setup file as a declaration, leaving it out of usage sources
        # as the pinned case does.
        options["exclude_folders"] = ["setup.py"]

    report = json.loads(scan(report_root, options))
    if sorted(report["selected_paths"]) != sorted(selected):
        raise ValueError("candidate dependency scan selected different fixture sources")
    expected = "incomplete" if case_id == DYNAMIC_IMPORT else "complete"
    receipt = dependency_receipt(report, expected)
    if case_id == FRAMEWORK and not any(
        row["distribution"] == "sphinx-click"
        and row["kind"] == "python_configuration"
        and Path(row["file"]) == report_root / "docs/conf.py"
        for row in report["dependency_usage_evidence"]
    ):
        raise ValueError("candidate did not account for the pinned Sphinx configuration")

    findings = report["quality"]
    if not isinstance(findings, list) or any(row["rule_id"] != "SKY-U005" for row in findings):
        raise ValueError("candidate report did not contain the selected dependency rule findings")
    if coverage is not None:
        coverage.update(receipt)
    return findings
