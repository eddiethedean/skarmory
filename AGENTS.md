# Repository guidance

Read `README.md`, `docs/architecture.md`, `docs/roadmap.md`, and
`docs/compatibility.md` before changing the analysis design.

- Keep changes scoped to the requested milestone or issue.
- Rust owns analysis; Python is the SDK and compatibility boundary.
- Preserve the native CLI's independence from the Python interpreter.
- Never present unimplemented, skipped, failed, or bounded-out work as a
  successful complete scan.
- Preserve uncertainty, evidence, and analysis coverage separately from findings.
- Treat cached/uncached and single/multi-worker equivalence as semantic
  requirements when implementing analysis.
- Use the pinned Skylos baseline for claimed compatibility. Record and review
  differences instead of automatically rewriting expected output.
- Clearly label proposed APIs and unmeasured performance goals.
- Read `CONTRIBUTING.md` for checks, packaging status, dependency locking, and
  third-party provenance expectations.

Run only the checks relevant to the change. For the scaffold, the commands are
in `README.md`; installed-wheel checks require a built native extension.
