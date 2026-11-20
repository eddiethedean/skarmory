# Contributing to Skarmory

Skarmory is at the scaffold stage. Start with the
[roadmap](docs/roadmap.md) and keep changes scoped to an explicit milestone.
The initial focus is the native Python dead-code vertical slice.

## Working on a change

1. Read the relevant architecture and compatibility sections.
2. Define the user-visible behavior and the limits of the change.
3. Implement the smallest complete slice with evidence for correctness.
4. Run the checks relevant to that slice and update the implementation status.
5. Describe behavior, validation, remaining gaps, and the roadmap task in the PR.

Rust owns the analysis pipeline. Python wrappers convert inputs and outputs;
they should not grow a second analysis engine. Keep the native CLI usable
without starting Python.

## Validation

The [README](README.md#checks) lists the scaffold checks. CI validates Rust
formatting, Clippy, workspace tests, extension-wheel installation, the native
CLI, and rebuilding an SDK wheel from a source distribution.

For analyzer changes, add paired semantic examples that establish both the
required finding and a closely related case that must be retained. Cached and
uncached scans, and single-worker and multi-worker scans, must agree on
normalized findings, evidence, and coverage. Follow the
[conformance plan](tests/conformance/README.md).

Do not change expected Skylos output merely to make a comparison pass. Record
and review each difference as an implementation defect, an upstream defect,
an intentional change, or an unsupported capability.

## Dependencies and packaging

The initial PyO3 dependency and Maturin build tool are pinned. A real generated
`Cargo.lock` must be committed once dependency resolution is captured; never
hand-author registry checksums or claim `--locked` validation without it.
CI currently resolves the dependency graph because the scaffold has no
committed lockfile. Reproducible dependency resolution, the intended MSRV,
source encoding, and the parser selection are Phase 0 gates.

The initial Python wheel contains the SDK extension. The native CLI is a
separate Cargo target. Do not document a combined wheel as supported until the
packaging experiment has verified executable installation and native linkage
on each supported platform.

Select and document the project's distribution license before its first
release. This scaffold does not assign an outgoing license. Record provenance
and applicable license obligations before importing third-party source or
fixtures; links to upstream projects do not make their code part of this repo.

## Reporting results

Performance claims need matched rules, inputs, complete analysis, pinned
versions, hardware information, and repeatable measurements. Report accuracy
differences beside timing. Follow the [benchmark plan](benchmarks/README.md).

No scanner rule is implemented in the initial scaffold. Analysis must continue
to fail explicitly until a real result can include its coverage state. Avoid
empty-success placeholders, implicit target-code execution, and silent feature
fallbacks.
