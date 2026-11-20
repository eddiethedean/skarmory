# Contributing to Skarmory

Skarmory has a bounded Phase 0 prototype. Start with the
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

The [README](README.md#checks) lists the development checks. CI validates Rust
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

The pinned Ruff and PyO3 dependencies are resolved in `Cargo.lock`.
Regenerate it with Cargo for deliberate dependency upgrades and run the locked
checks at the declared minimum Rust version. Maturin is pinned in the SDK build
manifest and CI.

The default wheel contains the SDK extension. Cargo installs the separate
native CLI. Keep the combined-wheel trial separate from the default build.
Its validation commands and receipts are in the [measurement report](benchmarks/phase0.md).

Skarmory is licensed under [MIT](LICENSE). Record provenance and applicable
license obligations before importing third-party source or fixtures.

## Reporting results

Performance claims need matched rules, inputs, complete analysis, pinned
versions, hardware information, and repeatable measurements. Report accuracy
differences beside timing. Follow the [benchmark plan](benchmarks/README.md).

Preserve the prototype's coverage and uncertainty reporting when expanding its
rule set. An incomplete run must always carry the reason work could not finish.
Do not execute target source during static analysis.
