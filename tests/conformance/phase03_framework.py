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
