"""Project native framework evidence into the pinned Skylos assertion fields."""

_ROOT_KINDS = {
    "django_admin_registration",
    "django_path_converter",
    "django_registration",
    "fastapi_dependency",
    "fastapi_exception_handler",
    "fastapi_lifecycle",
    "fastapi_lifespan",
    "fastapi_middleware",
    "fastapi_registration",
    "flask_registration",
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
