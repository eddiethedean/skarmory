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
| FastAPI | One registered router and one dormant router in the same module | Only the dormant handler |
| pytest | A collected test with an injected fixture used by an assertion | No unused findings |
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
| Absolute source exclusions | Skarmory anchors an absolute exclusion to the selected path. In the same-name control, pinned Skylos also excluded a nested file with the same basename. |
| Coverage | Skarmory reports selection, configuration provenance, and completeness explicitly. Skylos's resolution-scope flags are separate reference results. The comparator preserves completion and error outcomes. |
| Generated Python headers | Both tools analyze files with recognized generated headers so their imports and uses count. Generated-file findings are hidden by default. Skarmory lists those files in `generated_sources`. |
| Pytest autouse | A fixture with a statically false autouse flag and no requesting test remains reportable, along with its uncalled helpers. Pinned Skylos retained both in this control. |
| Pytest fixture overrides | Pytest uses the closest matching fixture and follows the active request chain when a dependency requests an ancestor. Skarmory records both targets as possible when a conditional override may be absent. It reports an unused fixture imported by `conftest.py` when a test module provides a definite override. Both pinned Skylos backends retained the unused fixture in that control. |
| Imported pytest tests | Pytest collects a function imported under a `test_` name. Skarmory retains the imported test and its called helper. Both pinned Skylos backends reported those functions unused in the import alias control. |
| Conditional pytest imports | Pytest collects the callable that the source module binds at runtime. Skarmory reports incomplete coverage when a `from` import selects an unresolved conditional binding or when an evaluated literal lazy export returns one. Both pinned Skylos backends returned no findings or analysis errors in the direct import and lazy export controls. |
| Effective pytest test bindings | Pytest collects tests and fixtures under assignment aliases and collects inherited tests through a local base class. Skarmory retains the collected targets and reports test definitions shadowed before collection. Both pinned Skylos backends retained the shadowed definitions in these controls. |
| Pytest collection with multiple bases | Pytest collects the inherited test in the local multiple-base control. Skarmory reports incomplete coverage when the class hierarchy exceeds its single-base collection model. Both pinned Skylos backends returned no findings or analysis errors in that control. |
| Lazy exports and submodules | CPython uses a literal lazy export while a same-named submodule exists only on disk. The first import of the child sets the package attribute, and a later import from the module cache preserves a subsequent reassignment. Skarmory follows the applicable binding and reports the unused function in the lazy-export controls. Both pinned backends returned no unused functions in those controls. |
| Submodule attachment imports | Skarmory retains an import when a later lookup relies on the package attribute that it sets. The pinned Skylos CLI reported this import unused in the attachment control. |
| Missing package attributes | CPython raises `AttributeError` when the package has no binding and its literal hook rejects an ordinary attribute read, including when a same-named submodule exists only on disk. Skarmory reports incomplete coverage in these controls. Both pinned Skylos backends returned no analysis errors. |
| Lazy imports in annotations | CPython evaluates the literal lazy export in an eager annotation and raises `ModuleNotFoundError` for its missing import. Skarmory reports incomplete coverage for the eager read and completes the postponed annotation control. Both pinned Skylos backends returned no analysis errors in either control. |
| Module initialization | CPython calls the earlier definition when package initialization imports its own existing member. Skarmory retains that definition and reports the later uncalled one. When a synchronous callback or a selected lazy-export branch re-enters a partially initialized module, Skarmory reports incomplete coverage. Both pinned backends returned no findings or errors in these controls. |
| Missing local imports | CPython raises `ImportError` or `ModuleNotFoundError` when a selected package lacks the requested member or dotted submodule. This includes names rejected by a literal `__getattr__` handler. Skarmory reports incomplete coverage even when the imported binding is unread. Both pinned backends reported unused imports with no analysis errors in these controls. |
| Legacy report scope | The profile exposes the six declared result lists, suppressions, retention evidence, abstentions, and errors. It has its own schema and omits unrelated assessment, workspace, and full definition-ledger fields. Native evidence is produced by the Rust analyzer. |
| File output errors | The pinned CLI reports exit 1 for the tested unwritable output path. Skarmory returns its operation-failure exit 2. |
| Dynamic behavior | Unresolved bindings, unmodeled provider calls that can affect non-use, ambiguous cross-scope rebinding, cyclic module initialization, and unknown class dispatch may leave coverage incomplete. The fixtures cover bounded patterns. Full Python semantics, arbitrary framework coverage, and every Skylos scoring branch are outside the verified comparison. |
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
