# Phase 0.2 reference results

The [Phase 0.2 comparator](../tests/conformance/check_phase02.py) uses the
Skylos 5.7.0 [frozen environment](../tests/conformance/baselines/skylos-5.7.0.toml)
with grep verification and its cache disabled. It runs the Python and
accelerated discovery backends separately. Fixtures are independently labeled
and copied outside the repository to isolate configuration discovery.

On macOS ARM64 with CPython 3.11.14, both Skylos backends and Skarmory agreed
on the selected paths, projected findings, and incomplete-analysis rule
identity in these fixtures. The projection includes finding category, relative
path, qualified symbol, line, and confidence when present.

| Fixture | Coverage checked | Projected findings |
| --- | --- | --- |
| Application | One file, one called function, one uncalled function with a local binding | Uncalled function and unread local |
| Library | Two files in a `src` package with a re-export and literal `__all__` | Private function outside the public call chain |
| Bindings | Three files with a used alias, an unused import, a class, and a called private function | Unused function, import, class, parameter, and comment-only file |
| Invalid | One selected file with a parse failure and no non-use proof | No unused findings and an explicit incomplete diagnostic |

The comparator also checks configuration inheritance and replacement, nested
file policy, strict exit status, a confidence boundary, and SARIF output.
Rust and installed-SDK tests cover additional semantic cases and cache edits.
The independent source assumptions and observed library difference are in the
[carry-forward holdout](phase-02-holdout.md).

## Reviewed differences and limits

| Surface | Decision |
| --- | --- |
| Source paths and error evidence | Skarmory uses stable project-relative labels and reports its own Ruff parser locations and messages. The invalid fixture has the same incomplete-analysis rule identity. The exact error text and column differ from CPython. |
| Coverage | Skarmory reports selection, configuration provenance, and completeness explicitly. Skylos's resolution-scope flags are separate reference results. The comparator preserves completion and error outcomes. |
| Legacy report scope | The profile exposes the six declared result lists, suppressions, retention evidence, abstentions, and errors. It has its own schema and omits unrelated assessment, workspace, and full definition-ledger fields. Native evidence is produced by the Rust analyzer. |
| File output errors | The pinned CLI reports exit 1 for the tested unwritable output path. Skarmory returns its operation-failure exit 2. |
| Dynamic behavior | Unresolved bindings, unmodeled provider calls that can affect non-use, and ambiguous conditional dispatch may leave coverage incomplete. The fixtures cover bounded patterns. Full Python semantics, arbitrary framework coverage, and every Skylos scoring branch are outside the verified comparison. |
| Local policy | The opt-in cache, strict input validation, and UTC date for temporary whitelist expiry are Skarmory policies. Tests use an explicit date for expiration boundaries. |

## Reproduce

Build the CLI and prepare the pinned reference interpreter using the
[Phase 0 instructions](../benchmarks/phase0.md#reproduce). Run both the
independent controls and the pinned differential comparison with

```sh
cargo build --locked -p skarmory_cli
python tests/conformance/check_phase02.py target/debug/skarmory \
  --reference-python "$REFERENCE_PYTHON"
```
