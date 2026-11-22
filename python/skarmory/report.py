"""Types for dictionary reports returned by the native analyzer."""

from typing import Literal, NotRequired, TypedDict


class Issue(TypedDict):
    kind: str
    path: str
    message: str
    line: NotRequired[int]
    column: NotRequired[int]


class Coverage(TypedDict):
    status: Literal["complete", "incomplete"]
    selected_files: int
    extracted_files: int
    issues: list[Issue]


class Finding(TypedDict):
    rule: Literal["unreachable_function"]
    module: str
    symbol: str
    path: str
    line: int
    column: int
    evidence: str


class PreviewFinding(Finding):
    symbol_id: str
    scope: str


class Evidence(TypedDict):
    symbol_id: str
    certainty: Literal["established", "possible"]
    kind: str
    path: str
    line: int
    column: int


class GrepVerificationEvidence(TypedDict):
    symbol_id: str
    qualified_name: str
    definition_path: str
    definition_line: int
    definition_column: int
    path: str
    line: int
    column: int
    kind: str
    certainty: Literal["possible"]


class Root(TypedDict):
    kind: str
    target: str


class Exclusion(TypedDict):
    path: str
    reason: str


class _Report(TypedDict):
    entry: str | None
    coverage: Coverage
    uncertainties: list[Issue]


class Phase0Report(_Report):
    schema_version: Literal[1]
    profile: Literal["phase0-python311-flat-application"]
    findings: list[Finding]


class PreviewReport(_Report):
    schema_version: Literal[2]
    profile: Literal["phase01-python311-dead-code-preview"]
    policy: Literal["application", "library"]
    roots: list[Root]
    source_roots: list[str]
    import_roots: list[str]
    selected_paths: list[str]
    exclusions: list[Exclusion]
    evidence: list[Evidence]
    grep_verification_evidence: NotRequired[list[GrepVerificationEvidence]]
    findings: list[PreviewFinding]


NativeReport = Phase0Report | PreviewReport
