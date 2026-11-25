"""Project native framework evidence into the pinned Skylos assertion fields."""

from types import SimpleNamespace


_ROOT_KINDS = {
    "celery_task",
    "django_admin_registration",
    "django_command",
    "django_path_converter",
    "django_registration",
    "fastapi_dependency",
    "fastapi_exception_handler",
    "fastapi_lifecycle",
    "fastapi_lifespan",
    "fastapi_middleware",
    "fastapi_registration",
    "flask_registration",
    "sqlalchemy_listener",
}
_CLASS_ROOT_REASONS = {
    "django_command": "dead_code_liveness:django_web_management_command_handle",
}
_SIGNAL_REASONS = {
    ("django_signal", "certain"): "django_web_signal_receiver",
    (
        "django_signal_unknown_loading",
        "possible",
    ): "django_signal_registration_unknown_loading",
}


def framework_report(report):
    """Match evidence to definitions by qualified name, file, and line."""

    def key(entry, name):
        return entry[name], entry["file"], entry["line"]

    events = {
        key(symbol, "qualified_name"): symbol["evidence"]
        for symbol in report["dead_code_evidence"]["symbols"]
    }
    dead = {
        key(finding, "full_name")
        for category in (
            "unused_functions",
            "unused_classes",
            "unused_variables",
            "unused_parameters",
        )
        for finding in report[category]
    }
    rescued = []
    definitions = {}
    symbols = []
    for definition in report["definitions"].values():
        name = definition["name"]
        occurrence = key(definition, "name")
        evidence = events.get(occurrence, ())
        certain = {row["kind"] for row in evidence if row["certainty"] == "certain"}
        projected = []
        for row in evidence:
            native_kind, certainty = row["kind"], row["certainty"]
            if (reason := _SIGNAL_REASONS.get((native_kind, certainty))) is not None:
                rescued.append({"name": name, "reason": reason})
                kind = "framework_root" if certainty == "certain" else native_kind
            elif native_kind == "unittest_discovery" and certainty == "certain":
                kind = "framework_root"
                reason = "unittest_discovery"
            elif native_kind in _ROOT_KINDS and certainty == "certain":
                kind = "framework_root"
                reason = native_kind
            else:
                kind = reason = native_kind
            projected.append(
                {**row, "kind": kind, "reason": reason, "native_kind": native_kind}
            )
        projected_definition = {**definition, "dead_code_evidence": projected}
        classification = None
        if occurrence in dead:
            classification = "likely_dead"
        elif certain:
            classification = "alive"
        elif projected:
            classification = "uncertain"
        if classification:
            projected_definition["dead_code_classification"] = classification
            symbols.append(
                {
                    "qualified_name": name,
                    "file": definition["file"],
                    "line": definition["line"],
                    "classification": classification,
                    "evidence": projected,
                }
            )
        # The native report can describe multiple occurrences of a qualified
        # name. Prefer the definition with matching liveness evidence.
        if name not in definitions or projected:
            definitions[name] = projected_definition

    summary = report["analysis_summary"]
    liveness = summary.get("dead_code_liveness") or {}
    return {
        **report,
        "definitions": definitions,
        "dead_code_evidence": {**report["dead_code_evidence"], "symbols": symbols},
        "analysis_summary": {
            **summary,
            "dead_code_liveness": {**liveness, "rescued": rescued},
        },
    }


def framework_definitions(report):
    """Expose the pinned analyzer's definition fields backed by native evidence."""
    definitions = {}
    for name, definition in report["definitions"].items():
        root_evidence = [
            evidence
            for evidence in definition["dead_code_evidence"]
            if evidence["kind"] == "framework_root" and evidence["certainty"] == "certain"
        ]
        refs = {"framework_root": 1.0} if root_evidence else {}
        for evidence in root_evidence:
            if reason := _CLASS_ROOT_REASONS.get(evidence["native_kind"]):
                refs[reason] = 1.0
        definitions[name] = SimpleNamespace(
            **definition,
            simple_name=name.rpartition(".")[2],
            heuristic_refs=refs,
        )
    return definitions


"""Present native unittest root evidence in the pinned assertion shape."""


def unittest_report(report):
    # The SDK groups evidence by symbol occurrence. Skylos's _marked helper
    # reads the reason of each certain unittest root under its definition name.
    discovered = {
        (symbol["qualified_name"], symbol["file"], symbol["line"])
        for symbol in report["dead_code_evidence"]["symbols"]
        if any(
            row["kind"] == "unittest_discovery" and row["certainty"] == "certain"
            for row in symbol["evidence"]
        )
    }
    definitions = {}
    for definition in report["definitions"].values():
        name = definition["name"]
        if (name, definition["file"], definition["line"]) in discovered:
            definitions[name] = {
                **definition,
                "dead_code_evidence": [{"reason": "unittest_discovery"}],
            }
        else:
            definitions.setdefault(name, definition)
    return {**report, "definitions": definitions}
