# Phase 0.3 Python dead-code coverage expansion

**In development.** Phase 0.3 starts from the [Phase 0.2 profile](phase-02.md)
and ships before [quality and secrets](roadmap.md#04-quality-and-secrets).
Published 0.2.0 packages do not support the `skylos-0.3` selector. The preview
behaviors below describe source builds from `main`; the workstream and release
criteria describe the target contract. Every in-scope Python dead-code test at
the pinned [Skylos 5.7.0 revision](skylos-5.7.0-contract.md) must pass against
Skarmory for Phase 0.3 to release. The inventory covers Python
semantics, frameworks and workflows. Other language assertions and inseparable
mixed-language cases are assigned to their [own milestones](language-frontends.md).
The Phase 0.3 profile qualifies Python sources; existing non-Python preview work
must be isolated from the Python release gate and default Python scan scope.

## Required upstream test gate

Track release conformance in issue [#41](https://github.com/eddiethedean/skarmory/issues/41).

### Discover and review cases

Audit the entire pinned checkout. Classify at the case level by what the
assertion exercises. Include positive and negative dead-code assertions,
analysis facts that determine liveness, and workflow assertions that depend
on a dead-code result or its coverage. A fixture's language or a test's filename
is a discovery hint. For example, review individual cases in
`test/test_requirements_includes_and_unused_deps.py` for `SKY-U005` versus SCA,
and classify other language cases by their owning milestone. For each mixed
case, preserve independently separable Python assertions and defer assertions
that require an unavailable language frontend. Record deferred cases without
counting them as passes or failures for the Python release. Preserve the setup,
expected results, and positive and negative controls that contribute to the
dead-code contract. If a combined Python assertion depends on another rule or
workflow, keep that dependency in the mapped case. Assign cases requiring
other language frontends to the first milestone with all required languages.
Count tests embedded in fixture projects when they independently assert analysis
or workflow behavior. Otherwise, preserve them as inputs to the analysis case.

The initial source audit found these collection paths. The commands are
starting points for discovery and reference runs at the pinned checkout.
[Pinned Skylos source](https://github.com/duriantaco/skylos/tree/87978e69adf91b3811e97b00c40aa39d7937cd94)

| Source | Discovery and environment |
| --- | --- |
| Python tests in `test/` | Start with `python -m pytest --collect-only -q test/`. Review collection against tracked test sources, then collect in each environment needed by the cases. Include the Python and `skylos_fast` reference paths where asserted. |
| Corpus and deterministic benchmarks | Review `corpus/manifest.json` with `python scripts/corpus_ci.py --manifest corpus/manifest.json --json`. Use `python scripts/dead_code_benchmark.py --manifest PATH --json` for the default and adversarial dead-code manifests. Review deterministic assertions in the other benchmark runners separately. Record each case's original assertion mode and any external fixture revision it depends on. |
| Tests outside default collection | Search tracked paths, runner configurations, and CI for additional suites. Review `rust/` with `cargo test --locked --manifest-path rust/Cargo.toml -- --list` and report-contract assertions under `editors/vscode/test/` using `npm test --prefix editors/vscode`. Record the runner and applicable cases, or the reason for excluding a reviewed suite. |

Freeze a versioned manifest containing these fields before using pass counts
to track implementation:

- **Case ID** uses the pytest node ID with its parameter ID, full native test or
  subtest name, or the benchmark manifest path and case ID. Record the pinned source path and fixture provenance.
- **Scope** records the assertion being preserved, its required workstream,
  language dependencies, owning release milestone, and whether it is required
  for Phase 0.3 or deferred. Assign inseparable mixed-language cases to the
  first milestone with all required frontends. Record the reason for any
  exclusion and preserve every independently separable Python assertion.
- **Required variants** describe each platform, toolchain, optional dependency,
  or backend that the original assertion requires, along with any fixture
  capabilities the runner must verify. Use the upstream preconditions to define
  this matrix before recording results.
- **Port mapping** identifies the Skarmory test or adapter and the fields that
  carry the asserted behavior. Use the engine for semantic facts and the native
  CLI or installed SDK for public interface assertions.

### Execute and account for every required variant

1. Record pinned Skylos results separately with upstream result markers intact.
   Run the original assertions against Skarmory without upstream expected-failure
   markers. Preserve the original fixture, controls, and expected results when
   porting. Thin adapters may translate entry points and fields losslessly.
   Ports must exercise the corresponding Skarmory behavior. A report-consumer
   test may use an upstream synthetic report as input to the real Skarmory
   consumer. When a case asserts analyzer or rescan behavior, run the real
   Skarmory analysis and inject fixture events at the relevant boundary. Record
   a divergent reference result under the
   [difference policy](compatibility.md#difference-policy).
   [Pinned expected-failure example](https://github.com/duriantaco/skylos/blob/87978e69adf91b3811e97b00c40aa39d7937cd94/test/test_package_local_exports.py#L60-L81)
2. Give pass credit when a required case and variant executes against Skarmory
   and all mapped assertions run and pass. Check fixture prerequisites
   explicitly and record setup failures or early returns as blocking outcomes.
   For example, verify symlink creation before running the legacy trace symlink
   control. A missing prerequisite, collection error, skip, deselection, xfail,
   xpass, or absent implementation blocks that case. An upstream-only run,
   mocked analysis result, or reviewed difference earns zero credit.
   [Pinned symlink control](https://github.com/duriantaco/skylos/blob/87978e69adf91b3811e97b00c40aa39d7937cd94/test/test_trace_file_analyzer.py#L90-L103)
3. Require the aggregated Python-scoped suite to pass before publication in
   the [release workflow](../.github/workflows/release.yml). Join expected and
   observed required results by case ID and variant. Exclude deferred cases
   from the required result set, pass counts, and blocker counts. Preserve
   their IDs, prerequisites, and owning milestones for future gates. Verify each result's release
   candidate, test inventory, and applicable artifact identity before giving
   credit. Fail on missing, unexpected, duplicate, mismatched, or nonpassing
   results and on runner errors.
   Publish the inventory, port mapping, exact commands, locked environments,
   revision and artifact identities, and per-case results. Exercise rejection
   of a missing shard, skipped case, missing backend, silent fixture setup
   failure, and result from another release candidate. Verify that a missing or
   failing deferred-language case cannot block or receive credit for Phase 0.3,
   while a missing required Python result still blocks publication.

## Scope of the Python release inventory

Freeze the Python cases and variants for Phase 0.3. Preserve the complete
reviewed inventory with owning phases for deferred non-Python cases. Future
language prerequisites (including missing TypeScript fixture sources) are
tracked in [language milestones](language-frontends.md).

The default Python-only profile must select Python sources for analysis in a
mixed repository. Compare a valid Python project with the same project plus
unsupported or incomplete JavaScript/TypeScript sources. The Python findings
and coverage must agree. An explicitly requested experimental frontend must
report its own scope and incomplete coverage where applicable. This condition
and deferred-case gate validation are tracked in [#41](https://github.com/eddiethedean/skarmory/issues/41).

## Scope and acceptance

| Workstream | Deliverable | Required controls | Issue |
| --- | --- | --- | --- |
| Python resolution | Extend bounded receiver and constructor dispatch, literal lazy exports, and statically declared plugin entry points. Resolve only targets supported by the selected sources and configuration. | Retain live definitions reached through aliases, constructors, exports, and registration. Report unused controls beside them. Retain finite possible targets with evidence. Mark coverage incomplete when custom import hooks, opaque exports, or dynamic loading leave an unbounded target set that affects non-use. | [#42](https://github.com/eddiethedean/skarmory/issues/42) |
| Python frameworks and test discovery | Extend pytest and FastAPI coverage from Phase 0.2. Model Django URL configurations and view registration, Flask routes and blueprints, and the additional framework contracts required by the inventory. The corpus already includes Django signals and management commands, Celery tasks, SQLAlchemy models and listeners, Pydantic lifecycle hooks, and unittest discovery. | Test active registrations, inactive modules and blueprints, and unrelated decorators with the same names. Surface unresolved registrations that could affect a non-use finding. | [#43](https://github.com/eddiethedean/skarmory/issues/43) |
| Unused Python dependencies | Implement `SKY-U005` for direct runtime declarations in `pyproject.toml`, requirements files, `setup.cfg`, and statically readable `setup.py`. Follow bounded requirements includes and recover declarations from supported source manifests referenced by compiled locks. Resolve usage through imports, framework and tool configuration, and statically identifiable command invocations. | Test nested includes, generated locks, and exact setuptools declaration lines. Cover import names that differ from distribution names, production, test and type-checking use, and configuration-only and command-only use. Keep build requirements, extras, constraints, and transitive pins outside the pinned runtime findings. An install command alone supplies no usage evidence. Abstain when unresolved inputs or dynamic uses prevent a reliable finding. | [#44](https://github.com/eddiethedean/skarmory/issues/44) |
| Grep verification | Search selected sources and supported configuration with a bounded verifier. Record textual matches as possible-use evidence with their locations and resolved symbol owners. Apply the profile defaults below. | Cover lookalike names, comments, strings, and supported dynamic references. Verify that a retained root reached from configuration keeps its reachable callees, including with a warm verifier cache. An empty search or an exhausted budget cannot establish non-use. | [#47](https://github.com/eddiethedean/skarmory/issues/47) |
| Runtime evidence | Import trace and coverage artifacts. Verify revision, source fingerprints, and observed locations for native evidence, and handle legacy trace inputs under the compatibility policy below. Offer explicitly requested Python collection through a separate integration. | Cover fresh, stale, malformed, mismatched, and legacy inputs. Unobserved execution cannot establish non-use. Static CLI scans remain independent of Python and execute no target code. | [#48](https://github.com/eddiethedean/skarmory/issues/48) |
| Git diff, baselines, and gates | Build findings from the full selected head snapshot before changed-line filtering. Compare complete base and head analyses for impact mode, including changes to unchanged code. Add versioned baselines and a configurable dead-code gate. | Remove a caller while leaving its callee unchanged. Check stable identity across line shifts, already-dead symbols, incomplete base and head snapshots, and stale baselines. Test strict and non-strict verdicts independently of scan coverage. | [#49](https://github.com/eddiethedean/skarmory/issues/49) |
| Dead-code cleanup | Produce a review queue and read-only patch previews for eligible findings. Implement explicitly requested edit application and rollback where required by upstream tests. Define effects and applicability per transformation and attach a source fingerprint to each preview. | Cover a safe preview, an import or definition with observable effects, and source changes after the scan. Revalidate the fingerprint before applying a patch. Exercise pinned Python cleanup assertions; defer TypeScript-specific cases to Phase 0.7. | [#50](https://github.com/eddiethedean/skarmory/issues/50) |

### Unused Python dependencies

Enable Python `SKY-U005` with `skarmory --profile skylos-0.3 --quality .`,
`--select SKY-U005`, or the CLI configuration key `quality_enabled = true`.
The SDK uses `analyze(path, profile="skylos-0.3", enable_quality=True)`.
`--quality` adds this rule to the Python dead-code scan. `--select SKY-U005`
runs the dependency rule on its own. The Phase 0.3 quality selection currently
covers this Python rule.

The declaration inventory reads PEP 621 runtime dependencies, conventional
requirements inputs, `setup.cfg` and static `setup.py`. Requirements includes
stay inside the manifest root. The scanner follows ordinary direct inputs
and recovers inputs from pip-compile or uv compiled text locks via source
annotations, a supported compiler header, or the sibling `.in` file. Compiler
headers can identify `pyproject.toml` as the direct source. Findings use the
original declaration line. Constraint roles carry through nested includes and
compiler options.
An explicit root requirements file governs its include tree. The conventional
`requirements/` directory is a fallback when neither root input exists.
Build and setup requirements, constraints, lock pins, optional extras and
dependency groups are outside the runtime inventory. A native opt-in for
optional groups remains planned.

The rule counts static Python imports, including test and type-checking use,
literal import calls, selected Django and Sphinx settings, package entry
points, and supported pytest, mypy, and flake8 configuration. It also reads
static command invocations in Procfiles, Dockerfiles, Compose files, Makefiles,
and tox configuration. An install-only command supplies no use evidence.
Distribution matching uses normalized names and a bounded table of known
import and console entry names. Static scanning uses only project inputs and
executes no target code. Mappings that would need installed package metadata
or network resolution are outside this scope.

JSON reports findings in `quality`, positive references in
`dependency_usage_evidence`, and rule coverage in
`analysis_summary.unused_dependency_analysis`. Reported declarations keep
their manifest line. For a declaration above the report directory, the
finding uses the report directory and line zero. SARIF omits the location
for this fallback. The rule abstains when a source, include or dynamic import
prevents a reliable absence claim. Ambiguous providers only block the
affected distributions. Explicit single-file scans and scans without
eligible Python sources record that the rule did not run. These limits
affect aggregate coverage and strict exit status. A rule ignored in project
configuration is outside the requested coverage.

The pinned reference can read installed distribution metadata and tries
additional standalone requirements files even when a root requirements file
exists. Skarmory uses the selected project snapshot and its declaration roots
for reproducible results. In the reviewed directory fallback case, Skylos
reports a pin reached only through a nested constraint. Skarmory excludes
that pin. For relative `importlib.import_module` calls, Skarmory resolves
literal packages and abstains when it cannot resolve the package. The pinned
reference can report those distributions as unused. The compatibility report
keeps the upstream `SKY-U005` shape and tool, plugin, driver, and type-stub
exemptions.
[Pinned declaration and location cases](https://github.com/duriantaco/skylos/blob/87978e69adf91b3811e97b00c40aa39d7937cd94/test/test_setuptools_declarations.py#L378-L421)
· [Pinned configuration and command evidence](https://github.com/duriantaco/skylos/blob/87978e69adf91b3811e97b00c40aa39d7937cd94/test/test_requirements_includes_and_unused_deps.py#L167-L223)
· [Pinned mapping references](https://github.com/duriantaco/skylos/blob/87978e69adf91b3811e97b00c40aa39d7937cd94/skylos/core/python_distributions.py)
· [Pinned no-location case](https://github.com/duriantaco/skylos/blob/87978e69adf91b3811e97b00c40aa39d7937cd94/test/test_vibe_rules.py#L786-L806)

Non-Python dependency and mixed npm integration assertions are assigned to Phase 0.7.

## Shared analysis and interface contracts

- Feed language-specific facts into the shared Rust analysis pipeline. Keep
  source discovery, declaration identity, reference resolution, and reachability
  explicit. When a selected frontend is unavailable or an unbounded reference
  can affect non-use, report incomplete coverage with the affected scope and
  cause. Retain finite possible targets with uncertainty and evidence.
- Store versioned file-local facts. Rebuild project resolution from the selected
  snapshot, effective configuration, and supplied evidence. Check frontend,
  grammar, and fact schema identity before reusing cached facts. Exercise an
  unchanged consumer after a provider, export, project manifest, or entry point
  changes.
- Keep native CLI and SDK options equivalent for shared analysis. Before adding a
  public option, document its default, input scope, output fields, and operational
  errors. Preserve Phase 0.2 interfaces or introduce a versioned schema or
  explicit opt-in where additional behavior requires a contract change. Keep
  stronger native semantics and reviewed compatibility differences visible.
- Preserve the source snapshot through analysis, filtering, and patch preview.
  Native gates require complete analysis of their declared scope. Filtering,
  suppressions, baselines, and runtime evidence must preserve coverage failures.
  Revalidate edit inputs and proof dependencies before applying or rolling back
  an explicitly requested change.

### Bounded Python ordering guards

The Skylos profiles analyze both branches of a single `<`, `<=`, `>`, or `>=`
guard when its operands have bounded scalar behavior. Comparisons that could
invoke a selected class hook report incomplete coverage when reached. This
supports the pinned recursive-group and caller-edit cases. [#52](https://github.com/eddiethedean/skarmory/issues/52)

The compatibility report groups an unused Python method under its already
reported unused owning class. The grouped method is still recorded as a
suppressed non-use finding, and a method of a retained class still receives
its own finding.

### Bounded Python framework discovery

The `skylos-0.3` profile uses resolved imports for the framework identities in
these models. A selected Flask app can register routes, request and context
callbacks, and error handlers directly or mount a blueprint. Blueprint callbacks
require the mount to be reached. A unique conventional CLI factory that
returns a resolved Flask app contributes possible-use evidence. Local teardown
setup helpers can receive that app through a resolved call. A bounded
zero-argument factory call can return one fresh Flask app through local aliases.
Defaulted factory parameters are supported when the factory can allocate the app
without consulting them. A closed module initializer that stops on a proven
missing binding or incompatible decorator receiver contributes no later
registrations. This includes a factory call that fails argument binding or
reaches an unbound name before any application mutation. A fresh generator
used immediately as a decorator receiver cannot register the callback.
Selected importers of a failed module retain incomplete coverage. The scanner treats
literal `g.pop(key, None)` as empty only when the selected code establishes
that the context key has no writers or opaque access. A resolved Flask-Login
manager records reached loader and error callback registrations through its
actual provider methods. A registered blueprint exported by a package can
supply callbacks through a child import when initialization order is proved.
[Flask registration APIs](https://flask.palletsprojects.com/en/stable/api/)

A regular package in a nested service resolves absolute imports against its
own source root. Services can use the same package name while retaining
only the callbacks they register. A missing local submodule keeps coverage
incomplete when the registration depends on it.

A conventional module-level `create_app` factory that returns its resolved
FastAPI instance contributes possible deployment evidence. A plain async
function used as a lifespan provider does not enter its coroutine body.
Source-owned no-op ASGI middleware receives possible-use evidence and leaves
an ignored `dispatch` option unrooted.
[Uvicorn factory setting](https://uvicorn.dev/settings/#application)

FastAPI supports imperative endpoint registration and literal `Depends` or
`Security` dependencies in defaults, `Annotated` parameters and imported type
aliases, and route and router registration options. App-wide dependencies apply
to selected API routes. The analyzer also resolves selected HTTP middleware and
exception handlers and async context-manager lifespans for active apps and
included routers. A custom app lifespan supersedes that app's startup and
shutdown hooks. Constructor startup and shutdown callbacks on included routers
remain eligible when a merged default lifespan invokes them. FastAPI versions
before 0.137 copy routes at inclusion. Newer versions retain the router and
can expose routes added later. Set `fastapi_router_mode` under `[dead_code]`
in the Phase 0.3 configuration to `snapshot` for the older deployment or
`live` for the newer one. The analyzer uses the config for the application
construction site. The default `auto` reports incomplete coverage when the
two models can select different routes or postponed dependencies.
The selected mode resolves bounded initializer order and does not infer a
deployment version from the scanner's own environment. Unproven order stays
incomplete. In live mode, later routes can become visible while startup
callbacks still come from the lifecycle list captured at inclusion. It retains constructors of resolved class dependencies. The preview resolves bounded
leading wildcard imports from the supported FastAPI and typing export tables.
Unmodeled FastAPI wildcard names and opaque namespace inspection leave coverage
incomplete.
[FastAPI router inclusion release notes](https://fastapi.tiangolo.com/release-notes/#01370-2026-06-14) · [Lifespan events](https://fastapi.tiangolo.com/advanced/events/)

Django URL discovery follows a reachable, recognized launcher. An explicit
management `--settings` option takes precedence over the launcher's environment.
The environment starts with the scan process's `DJANGO_SETTINGS_MODULE`. A
literal assignment to the genuine `os.environ` entry overrides it, and
`setdefault` fills an unset entry. Without a reachable launcher or environment
selection, one conventional `package.settings` module with literal `INSTALLED_APPS` and
`ROOT_URLCONF = 'package.urls'` can describe an externally configured project
when the sibling URL module imports the genuine Django URL API and defines
bounded `urlpatterns`. This models registration if the configuration is used.
Ambiguous configurations do not establish activation. Discovery resolves
the selected settings module's `ROOT_URLCONF`, literal `urlpatterns`,
`path` and `re_path` registrations, and literal `include` modules. Registered
class-based views reach their effective `setup()` hook. Function views can
reach a source-owned provider method when a fresh object receives a provider
selected by a literal argument. Calling a generator view creates the
generator and leaves its body dormant. Unresolved URL
registrations in active URL modules produce coverage diagnostics.
An initializer with a proven invalid URL call cannot finish constructing
its URL table. The preview follows a final literal list's direct `clear()`
and indexed `URLPattern.callback` edits in a closed initializer. Opaque options,
escaped table edits, and selected imports of a failed URL module leave coverage
incomplete.
In a closed URL initializer, an unbound final call stops before Django can
serve the constructed table. A selected module that builds a private table
without exporting `urlpatterns` contributes no callbacks when the namespace is
closed. A source-owned function whose code is replaced by a compatible no-op
contributes no original body when its only external consumer is the closed URL
table.

For a resolved local `INSTALLED_APPS` sequence, startup also loads selected
application modules, their models modules, and the `ready()` hook of a resolved
AppConfig. It accepts explicit config class paths and resolves default selection
in a local `apps.py`. A management launcher exposes local `BaseCommand`
subclasses named `Command` in the installed apps' public
`management/commands` modules. Earlier installed apps take precedence when
command names collide. A literal argument list limits discovery to the chosen
command. An unresolved app or command export produces incomplete coverage.
The selected command follows its effective lifecycle after module initialization.
Ordered writes through the command module's own globals dictionary or module
object can replace the export before Django reads it. Later ordinary assignments
can replace it again.
Proven construction or argument setup failures leave later stages dormant.
Custom dispatch that may delegate through an unresolved path produces a coverage
diagnostic. An isolated command module does not establish an installed app.
[Django management implementation](https://github.com/django/django/tree/stable/5.2.x/django/core/management)

Django `RunPython` operations preserve possible declaration use of resolved
forward and reverse callbacks. Parameter evidence covers the two positional
arguments Django supplies. Constructing the operation does not execute the
callback. [Django migration operations](https://docs.djangoproject.com/en/5.2/ref/migration-operations/#runpython)

The analyzer resolves local classes and functions named by effective authentication,
storage, logging, and Django template settings. Bounded backend methods and
context processors contribute possible-use evidence. The model follows named
imports, locally defined class inheritance, and finite setting alternatives.
A unique conventional settings package with literal application and secret-key
settings can describe an externally selected configuration. Storage and logging
entries use their effective dictionary keys. Bounded literal list and
dictionary edits follow initializer order, including directly called local
helpers, reassignments, and named imports in finite conditional branches.
The authentication backend list supports literal augmentation, repetition,
and item edits in the selected settings module. Proven invalid list operations
can stop initialization before Django consumes the setting. A closed source
decorator can replace a backend class with another initialized class or
discard a hook without executing its body. Source importers of a mutated list
and unresolved mutations retain their coverage diagnostics.

The preview recognizes stdlib `logging.config.dictConfig` consumers with bounded
module initialization. It follows literal formatter and filter declarations,
ordered dictionary edits, aliases, and eligible one-argument mutation helpers.
For source-owned instances used only by that initializer, it follows
constructor defaults, field edits, and reached method calls. Type guards over
known values account for reached `os.PathLike.register(dict)` calls and
short-circuiting. A proven unbound name in the initializer can prevent later
configuration from running. An opaque configuration or consumer leaves
coverage incomplete.

Mounted Django DetailViews with resolved model and template context retain
possible references to source model methods. The preview follows selected
Django template directories, literal inheritance and includes, and supported
template scopes. Bounded pathlib paths start at the file that defines them,
including settings imported from another module. The preview applies file
exclusions and requires resolved loader settings and context. Unsupported
markup in a selected template produces a coverage diagnostic.
[Django generic views](https://docs.djangoproject.com/en/5.2/ref/class-based-views/generic-display/) · [Django template API](https://docs.djangoproject.com/en/5.2/ref/templates/api/)

A supported Django form validation call retains possible `clean` and
`clean_<field>` callbacks from its effective field table. The preview
follows supported inherited fields, `None` masks, and literal ModelForm
field selections. Plain `Form` instances use literal edits to `base_fields`
or `declared_fields` made before the instance copies its field table.
Dynamic field tables or validation dispatch produce
coverage diagnostics.

Django admin discovery follows installed apps when `django.contrib.admin` or
`AdminConfig` is selected. `SimpleAdminConfig` leaves the per-app admin modules
dormant unless the selected application imports them. The analyzer resolves
the default site's `register` decorator or method and follows the registered
`ModelAdmin`'s effective literal `list_display`, `readonly_fields`, and `actions`
entries that select local callbacks. A local getter can supply a literal
display or readonly list. `@admin.display` and `@admin.action` attach
metadata to a callback. A loaded registration with opaque options or an
unresolved condition produces a coverage diagnostic.
An empty `admin.py` inside a package is treated as a convention in projects
that import genuine Django. The filename alone does not retain definitions.
[Django admin discovery and options](https://docs.djangoproject.com/en/5.2/ref/contrib/admin/#discovery-of-admin-files)

A reached Django autoreloader call retains the callback bound at the call
site. Rebinding the provider, conditional calls, and unresolved callbacks
preserve their corresponding uncertainty.

Django signal discovery follows modules loaded through the launcher or selected
project configuration, including imports in an installed app's `ready()` hook.
It resolves selected Django signal providers, direct `Signal()` instances, and subclasses that inherit
Django's `connect()` method. It accepts `@receiver`, a stored receiver
decorator, and `connect()`, including the named signal and callback arguments
and literal lists or tuples of signals. A local signal with a resolved Django
`connect()` method can establish registration. Registrations inside unresolved
branches contribute possible-use evidence. A proven registration
whose module is outside the known load path of an externally configured project
contributes `django_signal_unknown_loading` possible-use evidence. Opaque
registrations in loaded modules and registrations without a selected setup produce
coverage diagnostics. If a reached entry module imports or calls a startup
helper, registrations in the entry module and its imports are considered
possible. Opaque registrations on those paths also produce coverage
diagnostics. Bound receivers contribute to method dispatch when their
registration is reached. The model does not prove which dispatches occur.
[Django signal registration](https://docs.djangoproject.com/en/5.2/topics/signals/#connecting-receiver-functions)

The profile also recognizes selected Celery task registrations, including
reached `register_task` calls on a resolved app. Bounded Celery backend
configuration tracks the effective provider per app and honors the first
reached backend read. Bounded task routing follows per-app `task_routes`
settings from constructors and updates. Callable entries and resolved import
paths retain possible-use evidence and the supported router argument slots.
Literal routing map keys are task selectors. Opaque changes can leave routing
coverage incomplete.
[Celery task routing](https://docs.celeryq.dev/en/stable/userguide/configuration.html#task-routes)

The profile models SQLAlchemy listeners
and declared fields through a resolved SQLAlchemy or SQLModel base, including
SQLModel's literal `table` class option. It recognizes Pydantic model hooks,
unittest tests and lifecycle methods through effective class bindings, and pytest
hooks and fixtures through selected conftest modules or literal `pytest_plugins`
declarations. In a conventional unselected plugin module, a bounded hook
signature and a resolved pytest import can contribute possible-use evidence.
pytest autouse fixtures also apply to unittest cases collected by pytest. A reachable eager Pydantic
model declaration can execute its core-schema hook during class creation.
A resolved model dump can invoke the selected Pydantic serializers and computed
fields, including a bounded wrapping serializer that delegates through its
handler argument. A definite instance access also reaches the selected
computed field. Ordinary formatted strings preserve operand evaluation and
resolve supported local formatting methods. Typer callbacks and supported Sanic
worker listeners use genuine application objects and reached registration.
The analyzer also records attrs field capture, Pluggy hook declarations, and
Marshmallow schema and hook metadata at their verified declaration sites.
These are bounded models under
[#43](https://github.com/eddiethedean/skarmory/issues/43). The complete pinned
suite mapping and qualification are tracked by the [upstream gate](#required-upstream-test-gate).
The expanded replay still includes unqualified cases in Django command and
signal discovery, FastAPI and Flask registration, and Celery backend selection.
Use the pinned case ledger to assess each contract before claiming parity.

The pinned `django-urlpatterns` and `django-signal-receiver` corpus fixtures
omit startup and a qualifying settings/URLconf pair, leaving coverage incomplete.
The standalone `django-class-based-view`, `django-management-command`, and
`django-appconfig-ready` fixtures expect retention without a selected
registration or launcher. The `celery-task-class` fixture declares a task
without registering it. The pinned `starlette-add-route` fixture supplies an
`endpoint` keyword to Starlette 1.3.1's `add_route`, which takes `route`.
The installed framework raises before registering the handler. The `python-unittest-lifecycle` fixture expects hooks
to be retained without a discovered test method and an unrelated helper to be
whitelisted by path. These expectations remain required differences in the
reviewed inventory and do not receive pass credit.

The Starlette preview records possible HTTP route use through the supported
`add_route` call on a resolved conventional ASGI application. The aiohttp
preview records the callback captured by a genuine route table decorator.
Mounting and serving an aiohttp table requires a separate activation proof.
Recognized Gunicorn config files contribute possible-use evidence for supported
setting and lifecycle hook declarations.
[aiohttp route tables](https://docs.aiohttp.org/en/stable/web_reference.html#routetabledef)
· [Gunicorn settings](https://gunicorn.org/reference/settings/)

Some pinned Flask and Django lookalike cases deliberately introduce an opaque
replacement and expect a complete report. Skarmory keeps the missing evidence
visible when the replacement can affect reachability. FastAPI fixtures whose
later router registrations depend on the installed FastAPI version still need
a version-specific qualification variant. Review these differences before
claiming compatibility for the affected cases.

### Python shutdown callbacks

In the `skylos-0.3` profile, a reached call to the resolved standard-library
`atexit.register` retains a selected synchronous function or bound method
passed as its first positional argument. Import aliases, finite callback
alternatives, and bare function decorators preserve that relationship. A
lexically shadowed provider or callback requires a resolved local binding. The
callback and code reached only through its body carry possible-use evidence.
Forwarded values can supply possible-use evidence for known callable arguments.
Dispatch through a callback parameter needs a separately resolved call target.
An unresolved callback or a shutdown operation outside the bounded model leaves
coverage incomplete when reached. [#118](https://github.com/eddiethedean/skarmory/issues/118)

### Bounded Python receivers and lazy imports

Ordinary constructors and instance methods use the first positional
parameter as the receiver regardless of its name. An explicit
keyword receiver is resolved when the supported signature proves that the name
accepts a keyword. The analyzer retains each finite receiver selected by an
exhaustive `if`/`else` assignment or conditional expression, propagates the
selected child class through inherited methods, and follows a simple `__new__`
that returns one selected class instance. Opaque receiver sources and custom
allocation with an unbounded result still make analysis incomplete.

A literal equality branch in a module's `__getattr__` can return a selected
member through `getattr(importlib.import_module("pkg.provider"), name)`.
The analyzer resolves the loader and member through their bindings and tracks
the selected import's initialization. The bounded model requires selected
package initializers and abstains when attaching a child could replace a
package member. Shadowed loaders and other unmodeled call effects also
leave coverage incomplete. A literal module-name mapping can provide the lazy
dispatch table when the hook checks membership and subscripts that same mapping;
computed or mutated tables remain incomplete.

### Static package entry points

The `skylos-0.3` profile reads package entry-point declarations from the scan
root. It supports PEP 621 script and group tables, Poetry scripts and plugin
groups, `[options.entry_points]` in `setup.cfg`, and literal entry points
passed to a statically identified setuptools or distutils `setup()` call.
Source modules and dotted members resolve against the selected Python files.
A class target retains its public surface. A dotted class member retains the
named member and the class needed to look it up.

Metadata reads use the current snapshot even when Python facts come from the
cache. Invalid or unresolved declarations produce coverage diagnostics.
`setup.py` metadata is interpreted statically. When the script is itself
selected for dead-code analysis, its Python statements follow the ordinary
coverage rules. Statically selected entry points are also listed in
`analysis_summary.dead_code_liveness.rescued` with `package_entrypoint`
evidence. A direct, unshadowed `open(...).close()` statement is treated as an
unrelated external side effect; other unsupported setup-script effects remain
incomplete.

### Bounded Python function namespace reads

In both Skylos profiles, zero-argument calls to the built-in `locals()` and
`vars()` within a function retain the bindings that can be present at the
call site, including captured cells. A namespace that escapes can retain
possible callable targets with evidence, including coroutines and values
supplied by selected callers and executor submissions. Dictionary copies keep
the entries present after argument evaluation and apply explicit keyword
overrides. Unresolved values and escaping results of unsupported namespace
expressions, such as a dictionary union, report incomplete coverage. Length
checks and provable truth tests can read the namespace without exposing its
values. Shadowed names use ordinary call resolution.
Object-argument `vars` and module and class namespace reads remain outside this
model. [#42](https://github.com/eddiethedean/skarmory/issues/42)
In the Phase 0.3 profile, a one-argument call to the resolved built-in
`eval()` or `exec()` within a function treats the local bindings visible
at the call site as possibly read, including in dormant function bodies.
In both Skylos profiles, reachable calls still report incomplete coverage
and withhold unused findings because the text can execute outside the
bounded model. Static analysis leaves the supplied text unevaluated.
[#41](https://github.com/eddiethedean/skarmory/issues/41) ·
[#78](https://github.com/eddiethedean/skarmory/issues/78)

### Grep verification preview

The native CLI enables grep verification with `--grep-verify`. In the native
SDK, pass `grep_verify=True` to `skarmory.analyze`. The Phase 0.2 `skylos`
profile keeps verification disabled by default. Select `--profile skylos-0.3`
in the CLI or `profile="skylos-0.3"` in
`skarmory.compat.skylos.analyze` to enable verification when omitted. Disable
it with `--no-grep-verify` in the CLI or `grep_verify=False` in the SDK. The
Phase 0.3 selector is a preview for this workstream.

The verifier considers selected Python sources and UTF-8 `.yaml` and `.yml`
files in the scanned directories. It reads plain or quoted single-line block
mapping values and sequence items. A project-relative path such as
`workflow.py` can retain module-level functions. A qualified target such as
`workflow:queue_task` or `workflow.py:queue_task` names a specific function.
In Python, a literal member in a built-in `getattr` call can supply evidence
when the receiver and member resolve to a selected definition. Comments and
ordinary Python strings supply no evidence. This bounded subset covers the
pinned YAML rescue case. Skylos also searches other configuration formats and
uses additional text patterns. [Pinned configuration rescue test](https://github.com/duriantaco/skylos/blob/87978e69adf91b3811e97b00c40aa39d7937cd94/test/test_reachability_integration_qa.py#L259-L286)

Matches in `grep_verification_evidence` carry a symbol identity, definition
location, original reference location, and `possible` certainty. Reachability
propagates from the retained definition in the same scan. The Phase 0.3
compatibility report groups evidence by symbol and counts direct rescues under
`analysis_summary.grep_verify`. An empty search contributes no evidence of
non-use. The verifier bounds directory entries, configuration files and bytes,
and the number of references and evidence records. A verification limit
produces a coverage issue and an incomplete scan.

The Phase 0.3 SDK accepts the pinned `grep_cache` flag. Skarmory uses the
explicit `cache_dir` option for persistent file-local facts, and rebuilds
verification and reachability from the current scan inputs even with a warm
cache. The pinned SDK flag creates no verification cache files.

### Runtime evidence

Select the Phase 0.3 profile to import runtime observations. By default it
looks for project-root `.skylos_trace` and `.coverage` files. In the CLI,
`--trace-file PATH` and `--coverage-file PATH` select explicit artifacts.
`--no-trace` and `--no-coverage` disable the respective inputs. The Python SDK
accepts `trace_file` and `coverage_file` with the same path behavior, or
`False` to disable either input. Relative paths start at the scan root
(or the containing directory for a file scan).

The reader accepts these formats:

- Legacy Skylos v1 JSON calls with `file`, `function`, and `line`. A call
  must match the selected file, simple function name, and definition or first
  decorator line. A positive `count` is optional.
- A standalone coverage.py SQLite database with `file` and `line_bits`
  tables. The analyzer retains a callable when a reported line matches a
  statement or expression location in its body. Coverage source paths must
  be absolute. The database does not record the base for relative paths, so
  those observations are reported as rejected. Set `relative_files = False`
  in coverage.py's `[run]` configuration to store absolute paths. Bare legacy
  bitmaps without a `context_id` column can also retain a declaration to match
  the pinned compatibility behavior. These unverified hints do not establish
  body execution or retain callees through that body.
- Version 2 JSON with a `revision`, a `fingerprints` map, and `calls` and/or
  `lines` arrays. Line observations contain `file` and a one-based `line`.
  Fingerprints are lowercase BLAKE3 hex digests of the selected file's UTF-8
  content after CRLF and bare CR normalization to LF. Keys are selected
  project-relative paths with forward slashes.

The version 2 revision is the lowercase BLAKE3 hex digest of the source
snapshot. Start the hash with the bytes `skarmory-source-snapshot-v1\0`
(including the terminating NUL). In lexicographic path order, append each
selected file's path length as an unsigned 64-bit little-endian integer,
the UTF-8 path bytes, and the file's 32 raw BLAKE3 fingerprint bytes.
Use the selected files for the same effective scan scope. Both the revision
and the observed file's fingerprint must match to earn verified provenance.

The `runtime_evidence` report field identifies the matched callable,
observed location, artifact, and provenance. It reports declaration-only
matches as `runtime_coverage_declaration`. The grouped
`dead_code_evidence` uses the compatibility event kinds `trace_hit` and
`coverage_hit`. Legacy artifacts are unverified
and contribute possible use. An observed nested callable supplies possible-use
evidence for the scopes that created it. Evidence binds to selected files and
unambiguous locations. The reader rejects symlink artifacts, caps inputs at
1 MiB and 32,768 observations per artifact, and rejects WAL-mode SQLite
databases. A source newer than a legacy artifact cannot supply a match.
Rejections appear in `runtime_evidence_issues`, independently of static scan
coverage. Absence from any artifact supplies no evidence of non-use. Each
scan reads current artifacts even when it reuses cached Python facts.

To collect an unverified call trace explicitly, use
`skarmory.runtime.collect_trace(project)` as a Python context manager around
the workload you choose to run. It records calls on the current thread and
writes `.skylos_trace` when the context exits successfully. Static scans read
artifacts through Rust and run independently of that collection step.

[Pinned trace inputs](https://github.com/duriantaco/skylos/blob/87978e69adf91b3811e97b00c40aa39d7937cd94/test/test_trace_file_analyzer.py) · [Pinned coverage database cases](https://github.com/duriantaco/skylos/blob/87978e69adf91b3811e97b00c40aa39d7937cd94/test/test_implicit_refs.py#L181-L232)

### Dead-code diff, baselines, and gates

Select `--profile skylos-0.3` in the CLI or `profile="skylos-0.3"` in
`skarmory.compat.skylos.analyze`. The CLI uses the local Git executable for
revision and tree data. Static workflows run independently of Python and
read raw Git objects without running checkout filters or repository code.

```sh
skarmory . --profile skylos-0.3 --diff HEAD --format json-ci
skarmory . --profile skylos-0.3 --diff origin/main --diff-mode impact --strict
skarmory . --profile skylos-0.3 --write-baseline .skylos/baseline.json
skarmory . --profile skylos-0.3 --baseline-ref origin/main --gate --require-complete
```

- `--diff REF` analyzes the entire selected project directory, then displays
  findings on added or modified head lines. `--diff-base REF` displays findings
  anywhere in changed files. Both compare working-tree content with the merge
  base of `REF` and `HEAD`. This covers committed, staged, unstaged, and
  non-ignored untracked sources. Omitted `REF` uses `GITHUB_BASE_REF` on
  `origin`, then the `origin/HEAD` symbolic ref, then `origin/main`.
- `--diff-mode impact` also reports newly unused symbols on unchanged lines.
  Both scans use policy from the base tree, including nested configuration, the
  same explicit operator configuration and flags, and the same expiry date.
  An unchanged finding is matched by rule, relative path, and scoped binding
  identity. A move to a different path can be introduced. Impact uses static
  analysis and disables implicit runtime artifacts on both sides. Supplying
  an explicit runtime artifact with impact is an invocation error.
- `analysis_summary.pr_impact` reports base and head coverage independently.
  Either incomplete snapshot prevents introduced annotations. Git paths
  must be portable, UTF-8 paths. Impact accepts internal relative symlinks
  when the platform can recreate them. External links, submodules, unavailable
  blobs, more than 20,000 entries, or a snapshot over 128 MiB make the base
  comparison incomplete. Regular blobs over 2 MiB with a recognized binary
  asset extension (`.bin`, `.bmp`, `.gif`, `.ico`, `.jpeg`, `.jpg`, `.pdf`,
  `.png`, `.tif`, `.tiff`, `.webp`, or `.zip`) are excluded from the base snapshot
  and counted in `pr_impact.omitted.oversize_non_source`. The 2 MiB limit
  continues to block other oversized inputs, including recognized configuration
  files with asset extensions. Very large changed-line comparisons report a
  work limit.
- `--write-baseline PATH` records Python dead-code findings from a complete,
  unfiltered scan using Skarmory baseline schema 1. `--baseline` reads
  `.skylos/baseline.json`. Supply a
  different path immediately after the flag to use that file. Relative
  baseline input paths are resolved from the project, and relative output
  paths from the working directory. `--baseline-ref REF` reads the default
  baseline file from that revision. The receipt reports matches and stale
  entries, including findings fixed since capture when the current scan is
  complete. An unavailable or unsupported baseline leaves findings visible
  and exits 2.
- `--gate` uses `[tool.skylos.gate]` in `pyproject.toml` or `[gate]` in a
  standalone config. A configured gate with a Git diff uses the base policy
  and any explicit operator configuration. `max_dead_code` defaults to unlimited and `max_quality`
  to 10 for the opt-in dependency rule. `--strict` or `strict = true` for the
  configured gate fails on any finding and requires complete coverage.
  `--require-complete` or `[tool.skylos.gate.coverage] require_complete = true`
  also requires complete coverage. The compatibility gate can evaluate
  selected file or grep gaps under non-strict policy. Its verdict, the actual
  scan coverage, and the unchecked files and analysis scope are separate.
  Gate exit codes are 0 for pass, 1 for failure, and 2 for incomplete analysis.
  `--gate --force` can override exit 1 and keeps the failed verdict visible.
  The native profile retains its incomplete-scan exit 2.
- `--select` accepts repeated or comma-separated IDs, case-insensitively.
  Supported IDs are `SKY-U001` through `SKY-U006` and `SKY-E002`.
  `SKY-U005` enables the opt-in dependency rule. Minimum `--severity` filters
  the listing after gate evaluation. Dead-code diagnostics default to low
  severity independently of confidence. `--format json-ci` omits the two
  top-level symbol inventories and preserves analysis errors, coverage,
  gate verdict, summary counts, findings, and individual finding evidence.

The SDK returns a JSON string. Its workflow keywords are `diff`, `diff_mode`
(`lines`, `files`, or `impact`), `baseline` (a path or `True`),
`baseline_ref`, `write_baseline`, `select`, `severity`, `gate`, `strict`,
`require_complete`, `force`, and `output_format`. The gate reports its
`exit_code` in JSON.

See the [pinned workflow assertion mapping](phase-03-workflow-reference.md)
for the native report and policy entry points and remaining release scope.
[#49](https://github.com/eddiethedean/skarmory/issues/49) · [#41](https://github.com/eddiethedean/skarmory/issues/41)

### Python cleanup preview

Run the native cleanup command from a source build:

```sh
skarmory cleanup . --format json
skarmory cleanup . --task TASK_ID --format json
skarmory cleanup . --task TASK_ID --apply --proof PREVIEW_PROOF --format json
```

The queue uses a complete static Python scan. It presents unused functions
and imports for review. A preview includes a unified diff and a proof covering
the edit, source bytes, and inputs to the analysis. A bare module-level
function must have no decorators, defaults, annotations, or type parameters.
An ordinary import must use an allowlisted standard-library module. Cleanup
checks for local providers that could shadow it or its modeled transitive
imports during initialization. Definitions and imports outside this contract
stay in the queue for manual review.

Apply requires an explicit task and a fresh preflight scan. Supply the
preview proof to bind the request to that preview. Guarded writes currently
require Unix. The optional `--test-cmd` runs an explicitly supplied command
without a shell. The CLI rescans with the same settings after applying an
edit. A deletion-only patch can be verified when tests ran and the rescan
confirms the original statement is gone and no new risk findings appeared.
Replacements remain unverified without changed-line execution evidence. A
failed command, a returned statement, or a newly introduced risk finding
triggers rollback when the edited file still has the exact bytes Skarmory
wrote and the proof dependencies agree. After a failed check, an unrelated
source save can invalidate the rescan while still allowing the target alone
to be restored. Pytest JUnit reports use a separate 64 MiB read limit.
Skarmory checks the file again
after comparing the scans. A late removal or rewrite leaves the rescan
result intact and makes the overall execution incomplete. When project
configuration enables dependency analysis, its inputs are included in the
proof and its medium-severity unused dependency findings remain review work.

Exit 0 indicates a queue, preview, or applied edit with a completed rescan.
Exit 1 indicates failed validation. Exit 2 indicates an invalid request,
an unavailable action, or incomplete validation. See the
[pinned Python assertion mapping](phase-03-cleanup-reference.md).

## Rust crate candidates

These versions record the research baseline. Select and lock dependencies after
the pinned Python test inventory establishes which capabilities are required.

| Workstream | Candidate | What to verify before adoption |
| --- | --- | --- |
| Python dependencies | Keep the Ruff AST and existing `toml` dependency. The Python rule uses `pep508_rs` `0.9.2` to parse requirement specifiers, extras, and markers. Evaluate `uv-requirements-txt` `0.0.101` if pinned fixtures warrant a fuller requirements parser. | Keep static setuptools and command interpretation, bounded requirements traversal, declaration provenance, and evidence-to-distribution mapping in Skarmory. Compare the fuller parser's dependency footprint and offline behavior. [`pep508_rs`](https://docs.rs/pep508_rs/0.9.2/pep508_rs/), [`uv-requirements-txt`](https://docs.rs/crate/uv-requirements-txt/0.0.101) |
| Grep verification | Evaluate `aho-corasick` `1.1.5`, already in the lockfile as a transitive dependency, for bounded multi-literal matching. | Verify identifier boundaries and original source offsets. Apply symbol ownership, search budgets, and possible-use classification in Skarmory. [`aho-corasick`](https://docs.rs/aho-corasick/1.1.5/aho_corasick/) |
| Git diff and gates | Use Git plumbing for merge bases, tree paths, and raw blobs. Use `similar` `3.2.1` for bounded diffs over the source text used by the analyzer. | The native CLI fixtures cover the full selected head, working-tree edits, and analyzer behavior across diverging snapshots. |
| Cleanup | Use `similar` `3.2.1` for unified diff previews. Reuse `blake3` for source fingerprints and `tempfile` for staging. | Keep edit eligibility, source revalidation, and apply and rollback behavior in Skarmory. Cover read-only previews and stale patches with the pinned fixtures. [`similar`](https://docs.rs/similar/3.2.1/similar/) |
| Runtime evidence | Start with existing `serde_json` for artifact ingestion and the Python integration for explicit collection and export. Evaluate `rusqlite` if the pinned tests require direct native reads of `.coverage` files. | Check artifact provenance and stale evidence. If SQLite is required, qualify the supported coverage database schema versions and distribution impact. [Coverage database schema](https://coverage.readthedocs.io/en/latest/dbschema.html), [`rusqlite`](https://github.com/rusqlite/rusqlite) |

### Smoke-test results and adoption checks

An isolated Cargo project passed the relevant Git, requirement-parsing, literal
match, and diff smoke tests with Rust 1.98.1 on aarch64-apple-darwin. The related
language grammar research is retained in [language milestones](language-frontends.md).

Before adoption, run the relevant pinned fixtures, verify the minimum Rust
version and builds across supported targets, and measure the native CLI and
Python package size impact. Review licenses, including those of native grammar
sources, and update third-party notices when adding locked dependencies. Verify
installed native CLI and SDK entry points, and keep static scans independent of
the Python interpreter and grammar downloads. The smoke tests provide local integration evidence. Phase 0.3 compatibility
requires the complete Python-scoped gate.

## Implementation order

| Step | Exit evidence |
| --- | --- |
| 1. Inventory and runner | Reviewed case and variant manifest, pinned reference results, and an initial Skarmory ledger that shows gaps. Prove a port at each relevant layer before scaling the harness. Freeze independent holdouts before using them to evaluate the implementations. |
| 2. Python analysis | Port the Python resolution and framework cases, then the dependency checks. Demonstrate a retained symbol, an unused counterpart, an incomplete control, and equivalence after an edit to a dependency. |
| 3. Verification and reporting | Pass grep and runtime evidence cases with distinct provenance and retention decisions. Verify changed-line filtering, snapshot comparisons, baselines, and gates once finding identity is stable across the edits the contracts support. Show that deleting the final caller exposes the unchanged callee in impact mode only after both scans complete. |
| 4. Cleanup | Pass read-only previews and the pinned apply and rollback cases after finding identity, edit eligibility, and source fingerprints are verified. Test a changed source or proof dependency and a failed explicit validation check. |
| 5. Release | Publish the reconciled upstream ledger, independent controls, reviewed differences, feature matrix, and install evidence for the native CLI, SDK wheels, and SDK rebuilt from the source distribution. |

## Release gates

- Pass every case in the [required upstream test gate](#required-upstream-test-gate).
- Run independent live, unused, and incomplete controls for every advertised
  feature. Preserve required detections and must-retain cases from the
  [Phase 0.2 comparator](../tests/conformance/check_phase02.py).
  Record differences under the [compatibility policy](compatibility.md#difference-policy).
- Run the native CLI and installed SDK with equivalent supported options. Keep
  existing Phase 0, 0.1, and 0.2 invocations and report contracts valid. Version
  any incompatible schema or policy and verify that the CLI still runs without
  a Python interpreter for static scans.
- Compare single- and multi-worker scans with caching disabled and with cold,
  warm, and edited caches. Check findings, locations, evidence, uncertainty,
  and coverage. Edits must include source changes, graph root changes, and
  changes to configuration or supplied runtime evidence.
- Check that incomplete analysis remains visible in reports and gates within
  the Python scan scope and after filtering. Check freshness for verified runtime evidence
  and explicit provenance for legacy retention in the compatibility profile.
  Verify that cleanup previews leave the analyzed sources intact.
- Record timing and memory for matched, completed workloads using the
  [benchmark protocol](../benchmarks/README.md). Capture optional verification
  and workflow costs separately. Publish evidence before making performance
  or compatibility claims.
