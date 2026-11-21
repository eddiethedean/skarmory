# Phase 0.2 Python dead-code compatibility

Phase 0.2 is in development. The starting point is the
[0.1 packaged-project preview](phase-01.md). The reference is Skylos 5.7.0 at
revision `87978e69adf91b3811e97b00c40aa39d7937cd94`, with the artifact,
environment, and backends recorded in the
[baseline manifest](../tests/conformance/baselines/skylos-5.7.0.toml).
Compatibility applies to the Python 3.11 cases and options declared below.
See the [reference results](phase-02-reference.md), the
[carry-forward holdout](phase-02-holdout.md), and the
[cache benchmark](../benchmarks/phase02.md) for the measured scope.

## Profiles and interfaces

Keep existing native CLI and SDK invocations, including `--phase0` and
`phase0=True`, valid. Preserve their existing result fields, meaning, and exit
behavior. New native capabilities that need different defaults or incompatible
report changes require an explicit option or a new schema version. Type the
native Python report while retaining its dictionary return type.

Select the profile with `--profile skylos` on the native CLI or call
`skarmory.compat.skylos.analyze` in the SDK. The compatibility adapter
returns a JSON string. Both interfaces call the same Rust analysis pipeline.

```sh
skarmory --profile skylos . --json --strict
skarmory --profile skylos . --json --sarif results.sarif.json
skarmory --profile skylos . --json --cache-dir ../analysis-cache --workers 4
```

```python
import json
from skarmory.compat.skylos import analyze

report = json.loads(analyze(".", conf=60, cache_dir="../analysis-cache", workers=4))
if report["coverage"]["status"] == "complete":
    print(report["unused_functions"])
```

The compatibility entry points accept the options below and reject unsupported
requests explicitly. JSON reports include the scan's configuration sources and
any different layers applied to individual files.

| Input | CLI | SDK |
| --- | --- | --- |
| Scan scope | `PATH`, `--exclude`, `--include-folder`, `--no-default-excludes`, `--include-generated` | `path`, `exclude_folders`, `skip_generated`, `generated_keep_paths` |
| Configuration | `--config-file` | `config_file` |
| Filtering | `--confidence`, `--category`, `--all-advice` | `conf=60`, `all_advice=True` |
| Output | `--json` or `--format json`, JSON with `--strict`, and `--sarif` | JSON string |
| Reference verification | `--no-grep-verify` | `grep_verify=False`, `grep_cache=False` |

The reference workload is offline Python dead-code analysis with grep
verification and its cache disabled, other rule families disabled, and ambient
policy cleared. Record the complete reference invocation for CLI and API
separately. The adapter documents these defaults and rejects requests to enable
unsupported work. Keep advice visibility explicit in comparisons because
reference CLI and API defaults can differ.

## Declared analysis scope

Each row below is a proposed release target. Freeze positive, retain, and
incomplete or uncertain controls for each supported pattern before implementing
it. The [reference inventory](skylos-5.7.0-contract.md) guides those fixtures.

| Surface | Required 0.2 behavior |
| --- | --- |
| Discovery | Support explicit Python files and directories, ordinary packages and `src` layouts, documented include and exclude patterns, and the generated-source policy. Verify selected paths and exclusion reasons independently of findings. Exercise ignored parents, nested projects, symlinks, and unreadable or undecodable selected files. |
| Unused functions and classes | Find unreachable functions, including isolated recursive groups, and unused module-level classes in the declared model. Retain reachable definitions, imported identities, explicit exports, and proven instantiations. Track definition-time effects independently of body calls. |
| Unused imports, variables, and parameters | Cover straightforward lexical bindings, direct reads, aliases, and shadowing. Test an unused import beside a used alias and a re-export, an unused assignment beside a read or captured binding, and an unused parameter beside a read or a parameter kept by a supported signature contract. |
| Empty Python files | Match `SKY-E002` in `unused_files` for empty, comment-only, and docstring-only Python modules. Include `__init__.py`, `__main__.py`, and `main.py` as exempt controls. |
| Public API | Resolve explicit re-export chains and effective literal `__all__` after supported initialization. Model a single unshadowed module `__getattr__` that maps literal names to statically resolved local exports. An existing binding takes precedence over lazy lookup. An opaque export that affects the public boundary produces uncertainty or incomplete coverage. |
| Tests and framework roots | Start with statically collected pytest tests and fixtures and FastAPI route decorators, imported app and router constructors, and direct `include_router` registration. Exercise a mounted router, a dormant router, and a shadowed or unrelated decorator with the same spelling. Record how a selected test, fixture, app, or router becomes active. |
| Callbacks and closures | Resolve finite captured bindings and direct local callback targets. Cover canonical `asyncio.create_task`, executor `submit`, and `multiprocessing.Process(target=...)` followed by a supported `start()`. Check active, dormant, shadowed, and unresolved consumers. Distinguish creation of a coroutine or process from scheduling or starting it. |

Resolve external APIs through the declared static models and imported identity.
Keep type-only references, test use, possible calls, ordinary calls, and
registration evidence distinct. A finite possible target retains the affected
definitions with evidence. An unbounded target that affects project-wide
non-use makes the relevant analysis incomplete. Reuse Phase 0.1 source
snapshots and coverage diagnostics. Static analysis reads sources and
configuration without importing or executing target code.

Other framework and library APIs, namespace-package and arbitrary import-hook
resolution, dynamic plugin loading, opaque lazy exports, text-based grep
verification, arbitrary Python visitors and progress callbacks, and non-Python
rules are outside the declared 0.2 comparison. CI gates and baseline policy,
diff and runtime evidence, cleanup, and resident sessions follow later
milestones. Document unsupported observations in the result when they affect
the requested scan.

## Configuration, findings, and output

- Freeze discovery from a directory or the parent of a file. Per directory,
  use `.skylos.toml`, then `skylos.toml`, then `[tool.skylos]` in
  `pyproject.toml`. Stop at `.git`, a filesystem root, or `root = true`.
  Merge ancestors before descendants. An explicit CLI/API path wins over
  `SKYLOS_CONFIG_FILE` and replaces discovered user layers. Exercise ordinary
  list replacement, supported `dead_code` section merges, and relative pattern
  bases. Capture malformed discovered and explicit files separately. Synced and
  operator policy are outside this profile.
- Test source exclusions, rule and inline ignores, documented and expiring
  whitelist entries, path overrides, and bounded custom entry-point selectors.
  Start with `name` or `full_name` constrained by `module` or `path`.
  Add decorator, base-class, and parent selectors for resolved identities in
  the declared class and framework fixtures. Capture a matching and a
  nonmatching combination for each supported selector. Use a fixed date for
  expiry tests. Compare selection, suppression reason,
  confidence adjustment, root creation, and final finding visibility separately.
- Freeze scoring order, caps, threshold inclusivity, category exceptions,
  and advice visibility against the pinned analyzer. Test below, at, and above
  the cutoff for each supported category. Compare rule and symbol identity,
  normalized path, source location, confidence, severity where present,
  disposition, evidence, and coverage. A confidence threshold or output filter
  must preserve incomplete status.
- Deliver legacy JSON report mode, JSON with `--strict`, and SARIF 2.1.0
  export for the declared findings. Capture stdout, stderr, output file, and
  process status from the pinned CLI. Include a complete clean scan, a report
  listing findings with exit 0, the same findings with strict exit 1, and
  invalid invocation or incomplete scan cases. Freeze separate expectations
  for output failures and the supported SARIF invocation. Test Unicode paths,
  escaping, location conventions, and meaningful missing, null, empty, and
  false values. Keep parseable machine output on its documented channel.
- Expose effective scope, retained and uncertain candidates, suppressions,
  abstentions, and analysis errors wherever the selected report format
  supports them. Where the reference has weaker coverage reporting, preserve
  Skarmory's explicit coverage and publish the adapter difference. A failed,
  skipped, or bounded-out scan cannot appear as a complete clean result.

## Persistent module facts

Offer an opt-in persistent cache with an explicit directory. Reuse successful
file-local facts and source locations. Discover and read the current sources and
configuration each scan, fingerprint the exact source bytes used, and rebuild
imports, roots, resolution, reachability, policy decisions, and reports from
current inputs. A file's unchanged bytes alone cannot establish its reachability.

- Key each record by content and the inputs that affect fact extraction, including
  schema and extractor versions, parser and target versions, decoding policy,
  and any settings or identity used by that layer. Keep graph inputs in the
  current project snapshot. Changes to a threshold or a root can reuse facts
  when extraction is unaffected.
- Store bounded, validated Skarmory facts using a versioned format. Write via a
  temporary file in the same directory and an atomic commit. Limit cache I/O,
  validate the record's key, version, size, and contents before using it, and
  test interrupted writes and concurrent readers and writers on supported
  platforms.
- Treat absent, stale, corrupt, oversized, or inaccessible records as misses.
  A cache write failure can fall back to normal scanning. A source read or
  parse failure still produces its coverage diagnostic and cannot reuse past
  success. Expose cache activity as operational metrics separate from analysis
  coverage. Enabling the cache must preserve the selected file set.
- Test every edit against a fresh rebuild of that exact state. Required edit
  sequences cover an unchanged callee losing its caller, a recursive group
  losing its root, changed exports and configuration, module addition,
  renaming and deletion, a previously missing import becoming resolvable, and
  valid source becoming invalid and recovering.

Follow the [dependency plan](roadmap.md#planned-rust-dependencies) for TOML,
path matching, content hashing, atomic writes, and SARIF candidates. Qualify
only adopted dependencies on the pinned toolchain and supported platforms, and
refresh lockfile and distribution notices with those changes.

## Implementation order

1. Freeze the supported matrix, comparator, pinned reference invocations, and
   independent semantic fixtures. Capture Python and accelerated reference
   runs separately. Use the Python backend as the primary comparison and
   document backend-specific differences.
2. Implement effective configuration and source snapshots. Review the selected
   file set and provenance before comparing findings. Add scoped bindings,
   reachability, categories, and bounded external models in serial fixture
   slices. Keep the existing Phase 0 and 0.1 controls running.
3. Apply confidence, suppressions, and presentation to the shared result.
   Verify the installed SDK and native CLI paths, including JSON and SARIF
   contract fixtures. Reject unknown options and unsupported workflows.
4. Add the per-file cache once the uncached path passes. Run clean, cold, warm,
   and edited comparisons across worker counts and concurrent requests.
5. Review and publish the compatibility matrix, discrepancy log, holdout
   results, and reproducible benchmark receipts with the release.

## Release gates

- Every declared surface has runnable reference and independent semantic
  controls. Pin fixtures, environment, backends, and effective settings. Every
  difference has a reviewed classification and resolution under the
  [compatibility policy](compatibility.md#difference-policy). Publish exceptions
  to exact parity. Preserve the required detections and must-retain cases.
- Freeze an independent holdout before tuning rules. Record the reviewed
  truth, assumptions, and unresolved labels for a pinned application and
  library. Keep holdout results separate from regression pass counts.
- Compare serial and multi-worker output on clean scans and on cold, warm, and
  edited-cache scans. Normalize only documented timings, cache metrics, and
  temporary root prefixes. Findings, selected paths, confidence, locations,
  evidence, uncertainty, and coverage must agree. Run the same checks through
  the native CLI and installed SDK for their shared options.
- Publish a pinned packaged application and library benchmark with fresh CLI
  processes for cache disabled, cold, warm unchanged, and defined leaf, root,
  and export edits. Record exact inputs, settings, coverage, output
  fingerprints, workers, cache and OS page-cache state, raw timings, and memory
  under the [benchmark protocol](../benchmarks/README.md). Compare speed only
  over matched completed work. Numeric latency goals remain unmeasured targets
  until those receipts exist.
- Run the existing packaging checks and conformance fixtures alongside the
  new gates. Verify that the native CLI runs independently of Python and that
  the installed SDK produces equivalent native results.
