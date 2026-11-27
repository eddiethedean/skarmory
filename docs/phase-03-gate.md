# Phase 0.3 release gate

The [release workflow](../.github/workflows/release.yml) requires the
dead-code gate for versions 0.3.0 and later. The [initial inventory](../tests/conformance/phase03-inventory.toml)
is a reviewed subset of the pinned Skylos 5.7.0 suite. The complete case audit,
reference runs, and Skarmory ports remain release requirements. The reviewed
inventory assigns 1,816 cases to Phase 0.3 and defers 42 to later language
milestones. The reported release decision requires receipts from the same
candidate and a complete inventory.
[Phase 0.3 acceptance criteria](phase-03.md#required-upstream-test-gate) ·
[Recorded seed reference outcomes](phase-03-reference.md)

## Reviewed Phase 0.3 reference

The release workflow derives its conformance checkout from Skylos 5.7.0 at
`87978e69adf91b3811e97b00c40aa39d7937cd94`. The tracked patch supplies the
reviewed Starlette, unittest, Django, and Gin fixture corrections while those
upstream PRs are in review. It also supplies runtime-dependent FastAPI
reassignment inputs with the original pytest IDs. The baseline manifest records
the source revision, derived revision, exact patch hash, and correction provenance.
The fixture patch includes context from the original Apache-2.0-licensed
Skylos files. The pinned license text is kept with the baseline.
[Skylos fixture license](../tests/conformance/baselines/skylos-5.7.0-LICENSE.txt)

The materializer verifies the patch, checks the reviewed corpus and inline
fixture changes, and verifies the expected Git commit before switching to it.
Analyzer code, test assertions, and the
dependency lock come from the frozen revision. The gate checks fixture
hashes and requires an ordinary pass on the original assertions from the
derived checkout. The release retains separate controls on the frozen
fixture bytes for review evidence.

The derived checkout also supplies runnable setup for the reviewed Flask
replacements, FastAPI nested package imports, Django commands, form
validation, startup, logging, Celery routing, and concrete provider
and optional-import fixtures. The review retains each
pytest ID and assertion. For fixture setup in a test file, the materializer
checks the exact edited source lines and protects test signatures,
parametrization IDs, expected values, and assertions.

For a clean checkout of the frozen revision, materialize the same reference
locally with:

```sh
python scripts/phase03_reference.py /path/to/skylos-5.7.0
```

Use the patched checkout for the SDK, engine, and CLI receipts. Use a
separate unmodified checkout at the original revision for frozen controls.

## Reviewed framework mismatches

The [case inventory](../tests/conformance/phase03-inventory.toml) records the
original assertions and verified runtime discrepancies. The corpus rows in
the table below describe the frozen inputs preserved for runtime controls. A mapped
`runtime_review` includes the reason, runtime evidence and corrected
assertions. Receipts preserve the original Skylos and Skarmory outcomes.
The checker counts validated runtime reviews as preview passes under
`reviewed_runtime_preview_passes`. Release pass counts require ordinary
passes on the original assertions.

| Required cases | Observed behavior |
| --- | --- |
| Flask callback providers and blueprint exports | The original source executions failed before publishing a callback. Twenty fixtures raised `NameError` for an unbound replacement, opaque consumer, or decorator. The remaining two raised `ImportError` during a named re-export cycle or `TypeError` while constructing a local blueprint lookalike. The SDK preserves incomplete coverage for the failed consumer and records no registration. |
| External decorator in the FastAPI evidence suite | The frozen project supplies no `plugin` module. The derived fixture supplies a local shutdown registrar. CPython runs the callback and its helper after initialization, and the SDK records possible shutdown evidence while preserving the no-FastAPI-root assertion. |
| FastAPI nested projects | With FastAPI 0.141.1, the tool-only namespace fixture fails to import from the repository root and serves `/namespace_route` with HTTP 200 when `backend` is on the import path. The fixture leaves the source root unspecified. The sibling-service fixture fails to import `other` from `backend`. Including `worker` on the path finds `router = None` and fails at router inclusion. |
| Django function view negatives | Django supports a mounted async function view and can dispatch its provider. A view module that raises while the selected URLconf imports it cannot supply a callback. An unconditional source-proven failure can completely establish that the callback and later URL mappings are unreachable. |
| Django source-layout admin stubs | The frozen fixture leaves the import path unspecified and its local package lacks `django.apps`. The derived fixture selects the local source layout explicitly and supplies an inert `django.apps` lookalike that imports successfully. |
| Django command, autoreload and hook tests | The standalone fixtures leave installed apps, command selection, and invocation unspecified. Dispatching `handle`, validation, saving, and queries requires separate lifecycle actions. Escaped callbacks and classes remain unresolved without the external installer's code. |
| Django corpus URL, error handler, view, signal and app config cases | The fixtures declare genuine framework symbols. Each lacks the matching selected URLconf, mounted view, or Django startup path needed for activation. The declaration and activation evidence remain separate. [Upstream fixture PR](https://github.com/duriantaco/skylos/pull/1302) |
| Starlette imperative route corpus case | The pinned Starlette accepts the handler under the `route` parameter. Using `endpoint` as a keyword on `Starlette.add_route` fails before registration. The derived reference supplies the runtime-verified fixture correction. [Upstream fixture PR](https://github.com/duriantaco/skylos/pull/1300) |
| Gin decorator factory corpus case | Gin 0.5.0 raises `AttributeError` at `@gin.timed()` while importing the original component module. The fixture also supplies no `config.gin`. The derived reference loads both named bindings, and the original corpus assertions pass on Skylos and Skarmory. [Upstream fixture PR](https://github.com/duriantaco/skylos/pull/1304) |
| Django provider and settings fixtures | The original concrete imports invoke only `NcaaProvider.schedule_dates` and `ConcreteProvider.scoreboard` in their respective projects. The frozen Pontoon-shaped settings evaluate `DEV` to false and leave both development-only settings unbound. The launcher-free fixture needs explicit settings selection. Its auth application also needs `django.contrib.contenttypes` to finish setup. |
| Pontoon-shaped Django startup | The frozen WSGI fixture omits `django.contrib.contenttypes` and fails while loading admin. The derived project selects its development settings, loads the required apps, and mounts a view that uses the configured form and exception. The production control leaves the development settings inactive. |
| Django signature conventions | CPython 3.11 resolves `site` to its frozen non-package module. The derived project uses an importable package and exercises the original callbacks through Django startup, recorded checks, a mounted view, an admin registration, a bound form, and a real ORM query. |
| Logging configuration | The satellite settings omit `version`, so `dictConfig` raises before invoking either factory. In the path-like lookalike fixture, CPython 3.11 selects frozen `os`, the dictionary fails the `PathLike` guard, and `dictConfig` creates the formatter the original expects to be unused. |
| Wagtail snippet settings | The frozen snippet viewset supplies neither a model nor a `register_snippet` call. The derived fixture registers a real model, and Wagtail consumes its viewset options. |
| unittest lifecycle corpus case | CPython discovers zero tests in the original module. The original scorer also requires a path whitelist for an unrelated helper. Those declarations remain dormant in the analyzer. [Upstream fixture PR](https://github.com/duriantaco/skylos/pull/1301) |

The 35 cases in `test/test_django_static_entrypoints.py` build JavaScript
assets and HTML templates with Django static tags. They assert browser-script
entrypoints and JavaScript file, export, and handler liveness. The review
assigns all 35 to Phase 0.7 and retains every assertion and parametrized ID.
The unconsumed browser export in `test_django_app_repro_liveness.py` also
belongs to Phase 0.7. It checks the JavaScript export and browser reachability.
The native frontend or installed SDK variant follows the original entry point.
This family contributes neither required results nor pass credit to Phase 0.3.

The inventory also records the 27 original cases in `test_fastapi_truth_cycles.py`.
The engine port replays all 20 acyclic guards, including the 1,100- and
1,101-assignment variants. The pinned pytest function writes the Python
fixture. Its original assertions check native coverage and FastAPI route
evidence from the Rust engine. The frozen reassignment fixtures use a literal
seed that yields a determinate registration decision in FastAPI 0.141.1.
The derived baseline supplies a runtime seed for the six reassignment
cases, preserving each reassignment chain, pytest ID, and assertion. The
engine port checks the route declaration and possible registration
independently. The native Click control checks the declaration and dormant
callback after reassignment on the pinned fixture. The original combined
Click case still runs on Skylos with the danger and secrets analyzers enabled.

The native engine adapter replays the original acyclic initializer fixture
with its 40-call-event budget. It measures the bounded native expansion and
verifies the original incomplete reason and empty activation and parameter
sets. The separate legacy fallback test requires no public analysis error
even when its internal proof is incomplete. Native coverage records an
exhausted budget explicitly and preserves unresolved route evidence.

Four cases have approved release-scope exceptions tracked under
[#41](https://github.com/eddiethedean/skarmory/issues/41). The gate executes
their original assertions on Skylos and keeps the original candidate outcomes
visible without awarding ordinary pass credit. The installed SDK lacks the
combined Click analyzer options. The legacy FastAPI case requires root
evidence and a clean error list after its proof is exhausted. The native
engine controls use the original staged sources from the same release
candidate. They verify the Click declaration, dormant call, and unused
neighbor, and require an explicit FastAPI work-limit diagnostic with no
unproved activation or parameter evidence. Django awaits the directly mounted
async view at both tested confidence levels. Each exception preserves the
original unused-provider assertion failure and pins the six fixture source
files by digest. The same installed wheel must show a complete scan, the live
view and provider, and the two unused neighboring providers. Missing or failed
controls block qualification.

The class-attribute provider suite contributes 19 original Python cases.
The SDK ports use genuine Celery, Django admin, and django-storages providers
through direct imports, aliases, and a project intermediary. Ten derived
fixtures export the configured subclass. Celery 5.5.3 and Django 5.2.12
rejected the original constructor arguments, and the S3 backend in
django-storages 1.14.6 required configured Django settings. The derived
inputs preserve all original assertions and case IDs. The generic storage
case retains its original zero-argument construction and unread S3 settings.
Eight original controls cover local lookalikes, shadowed imports, and a
source package named like Celery.

The separate liveness suite exercises a local constructor and three optional
lookalike imports. The derived lookalike fixtures retain the missing imports
and define explicit fallback classes using `except ImportError`.
The original venv case checks the native dependency source roots and public
liveness from the installed SDK. The vendored-package cases exercise native
dependency discovery and class lookup.

The external-override suite exercises stdlib ancestry, builtin `dict`
allocation, Traitlets, DRF, Guardian, and an unavailable external base with an
explicit fallback. The Traitlets fixture constructs its configurable and
declares distinct fields for the two default handlers. The external source case
passes the original fixture search path to the installed SDK and checks
retained overrides and dead sibling members. The native engine replay
evaluates the original index assertions using real Rust lookup results
for defining classes, re-exports, aliases, missing members, and cycles.

### Runtime-derived test expectations

The release workflow tests nine discrepancies directly through the installed
SDK on the frozen corpus fixture bytes. These checks expect dormant Django
and unittest declarations where the fixture supplies no invocation. For
invalid Starlette and Gin calls and unresolved Django startup, they check
that the relevant coverage diagnostic remains visible. Separate native
controls exercise successful registration and startup.

Run these checks with a pinned Skylos checkout and an installed SDK.

```sh
phase03-skylos/.venv/bin/python tests/conformance/phase03_framework_corpus.py \
  phase03-skylos --runtime-controls --sdk-python "$sdk_python"
```

The workflow saves `runtime-controls.json` beside the candidate receipts.
The checker requires the original reference outcome and all prerequisites.
On reviewed rows it also requires each corrected assertion to run on the
candidate SDK and pass with the specified coverage. When a correction expects
incomplete coverage, the receipt must contain its diagnostic and show that
every fixture source was extracted.
When an unresolved import prevents full-project analysis, a correction can
preserve a finding or declaration in a complete scan of a declared fixture subtree.
The receipt records the full-project diagnostic and verifies that the subtree
scan extracted the same sources selected within that subtree.

[Django async views](https://docs.djangoproject.com/en/5.2/topics/async/#async-views) ·
[Django command dispatch](https://github.com/django/django/blob/stable/5.2.x/django/core/management/base.py) ·
[Django app population](https://github.com/django/django/blob/stable/5.2.x/django/apps/registry.py) ·
[Starlette 1.3.1 API](https://github.com/Kludex/starlette/blob/1.3.1/starlette/applications.py#L99-L107) ·
[unittest test loading](https://docs.python.org/3.11/library/unittest.html#loading-and-running-tests) ·
[Gin configuration](https://github.com/google/gin-config) ·
[Wagtail snippet configuration](https://docs.wagtail.org/en/stable/topics/snippets/customizing.html)

## Inventory and receipts

The installed SDK replays all 57 parametrized cases from
`test/test_closure_reachability.py`. Receipts verify fixture creation, preserve
each case's grep setting, and require runtime trace input to stay disabled.
The cases distinguish creating a closure from calling it and cover nested
ownership, captured bindings, callback escape, and private names.
The bounded nonlocal write model requires a setter immediately before the
final zero-argument closure call. Inferring a closure parameter's receiver from
the initial call requires the closure body to have no reference to its own
binding.
The frozen fixtures refer to an absent `external` module. The reviewed inputs
use CPython's `atexit.register` and `asyncio.run`, keep the list append and
dictionary item assignment on real callback registries, and use `__debug__`
to select both branches across normal and optimized interpreter runs. The
materializer preserves the original assertions and pytest IDs. The release
job publishes the frozen Skylos results alongside the paired derived replay.

### Closure safety audit

The release inventory requires all 26 cases in the pinned Skylos 5.7.0
`test/test_closure_reachability_safety.py` suite. Each installed SDK receipt
must prove fixture creation, default grep verification, and disabled runtime
trace input. The release job also records the frozen Skylos results and
checks that the required SDK cases match the complete frozen test ID set.
[Original closure safety tests](https://github.com/duriantaco/skylos/blob/87978e69adf91b3811e97b00c40aa39d7937cd94/test/test_closure_reachability_safety.py)

The reviewed input for two cases supplies the missing `external.py` next to
the original `worker.py` without changing its source or either assertion.
In normal CPython 3.11, the provider invokes the dispatched callback and
returns true from `choose()`. Under `-O` it leaves the callback uncalled and
returns false. Together the two executions reach each receiver in both
fixtures.

Skarmory uses the lexical class's mangled names for private attributes and
lambda parameters. It tracks captured binding updates through bounded calls
and definition headers, including optional rebinding when the possible
receivers are known. It distinguishes eager comprehensions from deferred
generator expressions and models materialization by resolved built-in
`list` and `tuple` for the bounded scalar callback form. Other generator
consumers and escaping generator values can still make coverage incomplete.
Skarmory follows a `finally` suite after straight-line literal assignments
to fresh local names. An overwrite can release an object and run its finalizer.

### Method reachability audit

The inventory accounts for all 80 cases in the pinned
`test_method_reachability.py` and `test_method_reachability_safety.py` suites.
All 80 pass on frozen Skylos. The unchanged assertions qualify 57 cases on
the installed Skarmory SDK and two on the native CLI in the local macOS
ARM64 CPython 3.11 replay. CI checks that the frozen pytest IDs match the
SDK and CLI inventory entries and replays the mapped cases on Linux.

The receiver-alias, import-alias, and recursion controls keep their original
grep settings. The method-root edit case runs uncached with one worker and
repeats each source state with the native fact cache and two workers. The
receipt requires cache misses for changed bytes and cache hits on the
repeated and restored source. It compares findings, evidence, and coverage
with the uncached report for each state.

For an exported module-level function that directly or conditionally returns
a statically identified instance, the analyzer keeps that instance's public
methods as possible uses. If a conditional return also has an unresolved
branch, the known methods keep that evidence and the scan reports incomplete
coverage. Calling an unexported factory and discarding its result leaves its
uncalled methods eligible for findings.

The analyzer also follows positional and keyword dispatch from a named
instance created by a plain constructor at module scope. This covers a
standalone call with one argument when intervening statements preserve the
fresh instance and its method bindings. A possible finalizer on the receiver
or an intervening instance leaves coverage incomplete, as do unmodeled
mutation and escape.

A named, one-parameter wrapper can dispatch directly to a method on a fresh
instance from a plain zero-argument constructor, supplied through a single
`*(value,)` tuple or `**{'name': value}` dictionary. A possible receiver
finalizer or a return value that cannot be proved scalar leaves coverage
incomplete. The frozen cases exercise both forms with grep verification
enabled and disabled.

An immediate method call through a builtin property can use the result of
a source-owned getter that returns a fresh instance. The analyzer checks
that the constructors and method bindings are stable and reports incomplete
coverage when a temporary may run a finalizer.

File-only scans and directory scans with effective additional exclusions
abstain from reporting unrooted, statically referenced methods and their
callees. The same policy applies to functions called only from unreachable
code. A caller outside the selected sources could reach the group.
The report records the abstention separately from coverage and still reports
unrelated unused definitions. In a whole-project scan, reported methods with
unrooted callers carry a per-symbol `no_reachable_callers` decision tag.
Unreachable functions with static callers also carry this tag and evidence.
An isolated module-level function can receive `no_refs` when the graph has
no static reference to it and no unresolved lookup could hide one. A `from`
import counts as a reference to the source binding even if its alias goes
unread.

The SDK accepts `changed_files` as an explicit report selector for a directory
scan. It analyzes the full project, including callers outside the selected
files, then limits findings to the requested files. The pinned case compares
the source selection and coverage against a full SDK scan. An independent
installed-wheel control checks that the projection keeps whole-project evidence,
retains an unused function in a selected file, and removes one outside it.
The changed-file report uses
the same abstention for unrooted groups as the other partial reports.
[Projection control issue](https://github.com/eddiethedean/skarmory/issues/172)
The remaining 21 method cases have diagnostic receipts and block publication.

Explicit `bool()` calls and `assert` reuse the native truth-test model.
The analyzer follows `__bool__` when present and falls back to `__len__`
when it can prove `__bool__` is absent. A truth hook outside the bounded
model leaves coverage incomplete.

### Local bindings, import evidence, and provider methods

The frozen reference checks all 101 cases in `test_dead_code_local_bindings.py`,
`test_dead_code_import_evidence.py`, and `test_provider_method_precision.py`.
The inventory preserves the original assertions, fixture setup and grep modes.
The installed-wheel replay qualifies 7 local-binding cases, 9 import-evidence
cases, and 16 provider-method cases. Imports read in bounded quoted type
annotations count as uses, including imports on each feasible conditional
path. The analyzer skips `Literal` values and `Annotated` metadata when
resolving type references. Quoted calls remain unevaluated.
Calls to the standard `Path` and `PurePath` constructors with one literal
string preserve the import reads without resolving path protocol calls on
arbitrary objects. A terminal call to a local closure can read imports from
bounded branches that complete before the closure is defined.
A literal `str.format(**locals())` call with one simple named field reads
the corresponding local binding when its value is a builtin scalar.
Formatting that may invoke a user-defined hook remains incomplete.
Calls to the genuine `typing.cast` can read imports in a quoted type argument
without evaluating the string. The native analyzer also handles default
`json.dumps` for literal values and a bounded `json.dump` into a text stream
from builtin `open`. The JSON cases pair the original scan with an installed
SDK control that proves a real call to the otherwise unused invoice method.
Provider replacements and unresolved writes can invalidate these proofs.
Two import cases exercise a function that reads the replacement of an
imported callable. A straight-line proof checks that neither the reader
nor the replacement can be called before the overwrite has completed.
Calls, decorators, and assignments that could release a finalizer keep
this analysis incomplete.

Diagnostics retain incomplete coverage, failed assertions, and cases where
the native report cannot establish the evidence required by the assertion.
The remaining 69 cases carry no release credit.

### Dead-code proof regressions

All 63 frozen cases in `test_dead_code_proof_regressions.py` pass on Skylos.
The installed Skarmory SDK qualifies 37 public analysis cases with the
original assertions. Seven internal lazy-export cases qualify against the
native engine. The analyzer can match a type-only import to the exact name
and provider returned by a bounded lazy getter, including a getter that
loads the provider into a local temporary. The bounded guards verify a
literal name or membership in a stable literal `__all__`. Provider mutations
or an export list exposed to another module can make the proof incomplete.
The analyzer also resolves an unexposed literal table of module and attribute
pairs when the getter uses a proved loader and attribute lookup. The native
replays pair each frozen mutation, escape, and missing-`fromlist` case with
a control that proves the target through the corresponding stable table.
Both replays use a package so the original unguarded hook is never called
to provide `__path__`. The analyzer withholds a lazy-table import proof when
the facade or builtin loader target could run an unresolved `__path__` probe.

Bounded deprecation evidence caps the finding confidence at 40 and records
`deprecated_api`. The SDK cases cover recognized docstrings and leading
warnings from the genuine provider, including warnings after passive module
metadata. Two native engine cases check the aliased `typing_extensions`
decorator and verify that adding a local `warnings.py` invalidates the proof
when the application source is served from the fact cache.

The diagnostics record the remaining 17 SDK cases. The workflow checks the
complete frozen case list against the SDK, CLI, and engine inventory entries.

### Reachability integration

The frozen reference passes all 51 cases in
`test_reachability_integration_qa.py`. The installed SDK qualifies 48 cases and
the native CLI qualifies one. Two unresolved receiver cases have diagnostic
receipts. The paired SDK and native CLI case verifies that grep evidence can
retain an imported function declaration while reporting the unread alias.
The SDK receipt binds the companion CLI digest to the release input with
`--native-cli` and executes the original comparison against both artifacts.

The changed-file cases compare whole-project source selection and coverage
before checking the projected findings. Two cache cases compare findings,
evidence, and coverage with uncached runs and check native fact-cache hits
and misses after source edits or repeated scans of external configuration.

### Analyzer regression tranche

The frozen workflow runs 38 reviewed cases from `test/test_analyzer.py` by
their class-qualified pytest IDs. They cover package surfaces, source
selection, suppressions, and reachability. Twenty-one qualify through the
installed SDK, including four cases for `# noqa: F401`. The native
frontend associates that directive with the Python import aliases it covers,
and the Rust report retains each suppression as evidence. Sixteen cases
have diagnostic receipts and one still requires a candidate port. The
malformed-configuration replay verifies that the installed SDK warns about
unusable legacy fields in a discovered `pyproject.toml` and still reports
the unused function. Its independent danger-rule assertion stays outside
Phase 0.3.

Add one case per upstream pytest node, native subtest, or deterministic
benchmark case. Record its source, assertions, prerequisites, fixture
provenance, required variants, Skarmory port, languages, and owning milestone.
Record independently separable Python assertions in Phase 0.3. Assign
assertions that require other frontends to the first release with all
required languages. Each variant identifies
its platform, backend, test layer, environment, and candidate artifact role.
Keep `inventory_complete = false` until the entire pinned suite has been
audited and every required case and variant is represented.

The checker uses evidence from the release workflow's own candidate jobs.
Each JSON receipt covers one variant and identifies the pinned Skylos
revision, candidate revision, exact inventory, environment, and SHA-256 of the
artifact actually tested. A receipt records the reference and Skarmory
commands and their separate case outcomes. A Skarmory pass must account for
the listed assertions and verify every listed prerequisite. A reference
pass must account for the listed assertions. For a reviewed `xfail`, record
the expected failing assertion as `reference_xfail` in the inventory. The
receipt records the failure location in `reference_failed_assertion` and
the preceding completed assertions in `reference_assertions`. Run the candidate
with the upstream marker disabled. Release credit requires passing the original
assertions or an approved exception with an accepted native control. A
reviewed runtime receipt records any mapped candidate assertion
failure or failure of the pinned shared fixture's complete-coverage assertion
when the correction requires incomplete coverage. A passing runtime control
can qualify the case for the preview while the original failure blocks release.

Use `--fixture-root` to check declared fixture paths for required cases.
The gate checks fixture provenance, license, and SHA-256, and requires
the receipt to identify the same content. Deferred cases retain their
fixture paths and prerequisites in `deferred_cases`. The four TypeScript
benchmarks are deferred to 0.7 and still need the original sources for that
milestone. [Fixture issue #45](https://github.com/eddiethedean/skarmory/issues/45)

For a local check, use the same artifact file that the runner tested and
provide its receipt. Repeat `--artifact` and `--ledger` for additional
variant evidence.

```sh
python scripts/check_dead_code_gate.py \
  --inventory tests/conformance/phase03-inventory.toml \
  --candidate-revision "$(git rev-parse HEAD)" \
  --artifact "sdk=/path/to/candidate.whl" \
  --fixture-root /path/to/pinned-skylos \
  --ledger /path/to/sdk-receipt.json \
  --report gate.json
```

The report includes artifact hashes, receipt contents, and each required case
and variant's decision. The ordinary pass and blocker counts cover the Python
cases outside the four approved exceptions. The approved cases have their own
acceptance status and retain their original reference and candidate outcomes.
The checker exits 1 if a required result is missing,
blocked, skipped, duplicated, stale, or unsuccessful, or if the inventory is
incomplete. The existing Phase 0.2 conformance fixtures and independent
controls continue to run through the packaging checks.

The release workflow installs the same-run Linux wheel in a clean SDK
environment and builds the native CLI and Rust library test executable from
the same candidate checkout. The [seed adapter](../tests/conformance/run_phase03_seed.py)
records separate SDK, CLI, and engine receipts. It runs the original upstream
assertions with their markers in the frozen Skylos environment. Candidate
pytest runs select `skylos-0.3` and disable upstream expected-failure markers.

The SDK shard runs the original framework corpus scorer against the selected
fixtures and records each original assertion, fixture hash, and outcome.
The framework corpus maps 33 of 42 cases, including DRF declarations and
FastMCP registration through project wrappers. The latest qualified fixtures
cover Celery task classes, Click command subclasses, and Pydantic computed
fields.
The FastAPI source-layout fixture retains its mounted WebSocket handler. A
request that supplies the unannotated query argument enters the handler with a
string and fails at `accept()`. The corpus also checks that the unmounted
route and the unrelated helper are reported as unused.
The framework audit now includes the FastAPI dependency-parameter tests and
all 40 nested-project route variants, each with an SDK port. Eight derived
variants supply the missing package prerequisites. FastAPI 0.141.1 serves
the root package's route and resolves the backend's own dependency. The
unrelated namespace and sibling-service routes stay dormant. The audit also records
eight import-scope cases where a project-local package has no such submodule.
Those cases now replay the original declaration, activation and unused-route
assertions through the installed SDK. The import-root alias engine port stages
the original nested project and verifies both negative controls through the
native index. Two router graph ports run the original budget and cycle
assertions against the native traversal. The receipts preserve the original
edges, the uncalled-owner control, and the fixture-specific work limit.

The eight version-sensitive FastAPI variants use a separate live-router
receipt. The runner verifies the FastAPI 0.141.1 runtime wheels against a
dedicated lock and serves the original fixture before crediting the
installed SDK scan with the explicit live mode. The original pytest
assertions and PyProject remain unchanged. In the runtime cross-check,
FastAPI 0.136.0 returns HTTP 404 for both late routes, while 0.141.1 returns
HTTP 200. [FastAPI router inclusion release notes](https://fastapi.tiangolo.com/release-notes/#01370-2026-06-14)

The mapped Django SDK cases also verify assertions in the pinned shared
fixture helper. The DRF method-field case checks the inherited getter and
reads the unused sibling method from the native report's grouped findings.
The Django view ports cover a source-proven unbound initializer and an unbound
decorator. The converter and backend ports cover the class that a closed
metaclass returns or modifies. The application fixture traces the called
converter through both static methods and its returned instance. The
configuration replacement suite has thirty ports exercising effective Django
settings protocols and ordered collection mutations.

| Django family | Mapped | Blocked |
| --- | ---: | ---: |
| URL function callbacks and view providers | 49 | 2 |
| Path converter registration and protocol | 60 | 0 |
| Authentication backend selection and hooks | 70 | 1 |
| Named settings imports and overrides | 26 | 2 |
| Model, command, and form hook provenance | 37 | 10 |
| Management command reachability | 24 | 12 |
| Python template liveness | 49 | 0 |
| Form hook evidence after source edits | 2 | 0 |
| Autoreload callback selection | 29 | 3 |
| Admin stub context | 6 | 1 |

The Django hook cache ports reuse a cache across the original edits. The separate Django
converter probes for Skylos's private parser and index hooks exercise
implementation call counts outside the native report contract. The broader
framework inventory remains under review.

The logging configuration suite maps 42 of 43 cases. The Flask callback
provider suite maps 38 of 58 cases. Proven provider initialization failures
stop later Flask registration, while the selected importers still report
incomplete coverage.

The SDK shard also executes the pinned basic dead-code benchmark with
the original scorer and staged fixtures. Both score reports are included in
the receipt. Credit requires complete analysis, the labeled detections and live
controls, the original timing limit, and verified candidate scan options.

For SDK cases, the adapter forwards upstream scan options, checks the
candidate interpreter version and architecture, and compares the installed
package with the wheel. Runtime-evidence cases verify import, explicit
disablement, or rejection as appropriate. For the recursive CLI case, the
adapter records the binary hash and verifies complete scans of both fixture
states with the default grep verification. The CLI shard also runs the
landed Git impact and real-scan JSON-CI cases using the original fixtures
and options. It reads the native CLI's stdout and report files or verifies
the CLI error for invalid impact invocations. The adapters require the
original assertion lines to execute.

The cleanup CLI ports perform live previews, edits, and rescans. They run
authorized checks in subprocesses, including the pinned configuration and
source mutations. The engine receipt covers the late-removal case, which
injects its mutation after report comparison. It stages the pinned fixture,
runs real pytest and Skarmory analysis, and requires exactly the named
Rust test to pass. The receipt identifies the test executable and the
fixture bytes.

For versions below 0.3, matching pull requests and manual workflow runs check
each mapped SDK, CLI, and engine case's `preview_status` and upload the full
gate decision. The main SDK reference worker can run for up to five minutes.
Candidate workers and reference workers for the smaller shards have a
three-minute limit. A worker timeout records a failed receipt and blocks
qualification. Publication of 0.3 and later requires the full decision to pass,
including a complete inventory and an ordinary pass for every required case.

Run the checker regression tests with the following command.

```sh
python -m unittest discover -s tests/conformance -v
```
