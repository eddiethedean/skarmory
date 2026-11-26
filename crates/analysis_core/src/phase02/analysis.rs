//! Project binding resolution and reachability for the Skylos profile.

use std::borrow::Cow;
use std::cell::OnceCell;
use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::ffi::OsStr;
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};

use skarmory_python::Location;
use skarmory_python::phase02::{
    ComprehensionForm, ComprehensionIterator, Definition, DefinitionKind, DjangoBackendListEdit,
    Expression, ExpressionKind, Import, Name, Statement, StatementKind,
};

use super::config::{Entrypoint, FastapiRouterMode};
use super::{
    Analysis, AnalyzedFile, Category, DefinitionContext, Evidence, Finding, WEAK_HELPER_FUNCTION,
};
use super::{package_entrypoints, runtime_evidence};
use crate::Issue;
use crate::grep_verification::{self, Evidence as VerificationEvidence};

mod aiohttp;
mod celery;
mod celery_routes;
mod class_parameter_calls;
mod comprehension;
mod django_choices;
mod django_forms;
mod django_globals;
mod django_middleware;
mod django_migrations;
mod django_template_paths;
mod django_templates;
mod django_urls;
mod django_views;
mod drf;
mod fastapi;
mod field_dispatch;
mod flask;
mod frameworks;
mod gin;
mod gunicorn;
mod literal_text;
mod logging_config;
mod mcp_tools;
mod metaclasses;
mod mutations;
mod pydantic;
mod sanic;
mod source_provider_calls;
mod starlette;
mod terminal;
mod traitlets;
mod wagtail;

pub(super) use django_templates::TemplateProject;
pub(super) use gin::GinProject;

const MAX_BINDING_CHAIN: usize = 64;
const MAX_FRAMEWORK_PASSES: usize = 64;
const MAX_PYTEST_REQUEST_STATES: usize = 16_384;
const MAX_CONDITIONAL_STATES: usize = 4_096;
const END: Location = Location {
    line: usize::MAX,
    column: usize::MAX,
};

type SymbolId = usize;
type ScopeId = usize;
type Object = (usize, usize, usize);

#[derive(Clone)]
enum Value {
    Callable(Invocation),
    Instance(SymbolId),
    Alternatives(Vec<Value>),
    ReceiverParameter(SymbolId),
    ReceiverMethod(SymbolId, String),
    Coroutine(Invocation),
    ScheduledTask,
    Module(usize),
    InitializedStdlibOs,
    CommandNamespace(ScopeId),
    CommandNamespaceGet(ScopeId),
    FrameworkCode(SymbolId),
    Provider(String),
    Object(Object, ObjectKind),
    Method(Object, ObjectKind, String),
    DjangoInclude(Object),
    DjangoInvalidRegistration,
    DjangoPattern(Box<Value>, Option<django_views::ViewArguments>),
    DjangoViewFactory(SymbolId),
    DjangoView(SymbolId),
    DjangoField(django_forms::Field),
    DjangoFormValidation(Option<SymbolId>),
    DjangoSignal(Object),
    DjangoSignalConnect,
    DjangoReceiver(frameworks::DjangoReceiver),
    FastapiDependency(Option<SymbolId>),
    FastapiInferredDependency,
    FastapiAnnotated(Option<SymbolId>),
    FastapiFieldMarker,
    FastapiSubclassApp(Object, SymbolId),
    FastapiInheritedMethod,
    FastapiMiddleware(Option<SymbolId>),
    PydanticDump(SymbolId),
    McpServer(Object),
    PluggyHookMarker,
    CeleryConfig(Object),
    CeleryBackend(Object),
    DeferredComprehension,
    ComprehensionResult,
    DeferredLambda,
    Executor,
    Submit,
    Process(Box<Value>),
    Start(Box<Value>),
    Text(String),
    ProjectPath(PathBuf),
    ResolveProjectPath(PathBuf),
    Bool(bool),
    Integer(i64),
    None,
    Sequence(Vec<Value>),
    Mapping(Vec<Value>),
    // Python 3.11 can refresh a previously returned dictionary on a later read.
    LocalNamespace(ScopeId, [usize; 2]),
    NamespaceCopy(Box<NamespaceCopy>),
    NamespaceParameter(SymbolId),
    OpaqueNamespace,
    Range,
    /// An exact built-in container with scalar elements and untracked length and order.
    BuiltinIterable,
    Scalar,
    ConstantFunction,
    NullaryConstantFunction,
    IdentityFunction,
    IdentityDecoratorFactory,
    EmptyFlaskContextEntry,
    FlaskConfig,
    MissingFrameworkMember,
    DeletedModuleBinding,
    UnittestComparison,
    OperatorResult,
    Unknown,
}

impl Value {
    fn alternatives(values: Vec<Self>) -> Self {
        fn flatten(value: Value, flattened: &mut Vec<Value>) {
            match value {
                Value::Alternatives(values) => {
                    for value in values {
                        flatten(value, flattened);
                    }
                }
                value => flattened.push(value),
            }
        }

        let mut flattened = Vec::new();
        for value in values {
            flatten(value, &mut flattened);
        }
        Self::Alternatives(flattened)
    }

    fn contains_unknown(&self) -> bool {
        match self {
            Self::Unknown => true,
            Self::Sequence(values) | Self::Mapping(values) => {
                values.iter().any(Self::contains_unknown)
            }
            Self::Alternatives(values) => values.iter().any(Self::contains_unknown),
            _ => false,
        }
    }

    fn may_contain_namespace(&self) -> bool {
        match self {
            Self::LocalNamespace(..) | Self::NamespaceCopy(_) | Self::OpaqueNamespace => true,
            Self::Sequence(values) | Self::Mapping(values) => {
                values.iter().any(Self::may_contain_namespace)
            }
            Self::Alternatives(values) => values.iter().any(Self::may_contain_namespace),
            _ => false,
        }
    }

    fn is_builtin_scalar(&self) -> bool {
        matches!(
            self,
            Self::Scalar | Self::Bool(_) | Self::Text(_) | Self::None
        )
    }

    fn is_builtin_scalar_or_iterable(&self) -> bool {
        self.is_builtin_scalar() || matches!(self, Self::Range | Self::BuiltinIterable)
    }
}

#[derive(Clone)]
struct NamespaceCopy {
    owner: ScopeId,
    first: [usize; 2],
    copied_at: (ScopeId, [usize; 2]),
    site: (ScopeId, [usize; 2]),
    keywords: BTreeMap<String, Value>,
}

#[derive(Clone, Copy)]
struct Invocation {
    target: SymbolId,
    receiver: Receiver,
    implicit_descriptor_receiver: bool,
    verified: bool,
}

impl Invocation {
    fn ordinary(target: SymbolId) -> Self {
        Self {
            target,
            receiver: Receiver::Ordinary,
            implicit_descriptor_receiver: false,
            verified: false,
        }
    }

    fn bind_receiver(self, receiver: Receiver) -> Self {
        if matches!(self.receiver, Receiver::Bound(_)) {
            self
        } else {
            Self { receiver, ..self }
        }
    }
}

#[derive(Clone, Copy)]
enum Receiver {
    Ordinary,
    Bound(SymbolId),
    Explicit(Option<SymbolId>),
}

impl Receiver {
    fn supplied(self) -> Option<SymbolId> {
        match self {
            Self::Ordinary => None,
            Self::Bound(class) => Some(class),
            Self::Explicit(class) => class,
        }
    }
}

#[derive(Clone, Copy, Eq, Ord, PartialEq, PartialOrd)]
enum ObjectKind {
    App,
    Router,
    FlaskApp,
    FlaskBlueprint,
    FlaskLoginManager,
    TyperApp,
    SanicApp,
    StarletteApp,
    AiohttpRouteTable,
    CeleryApp,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum FrameworkClassRole {
    BuiltinDict,
    StdlibAstVisitor,
    StdlibAsyncProtocol,
    StdlibCtypesStructure,
    TraitletsConfigurable,
    Unittest,
    UnittestAsync,
    DjangoTestCase,
    DjangoDatabaseTestCase,
    DjangoAppConfig,
    DjangoCommand,
    DjangoView,
    DjangoModel,
    DjangoManager,
    DjangoQuerySet,
    DjangoForm,
    DjangoModelForm,
    DjangoFormField,
    DjangoRelationshipField,
    DjangoSignal,
    DjangoAdmin,
    DjangoGuardianAdmin,
    DjangoSimpleListFilter,
    DjangoStorageBase,
    DjangoS3Storage,
    ClickCommand,
    DrfSerializer,
    DrfViewSet,
    DrfField,
    DrfAuthTokenView,
    CeleryTask,
    CeleryRedisBackend,
    SqlAlchemyModel,
    SqlModel,
    PydanticModel,
    MarshmallowSchema,
}

impl FrameworkClassRole {
    fn is_unittest(self) -> bool {
        matches!(
            self,
            Self::Unittest
                | Self::UnittestAsync
                | Self::DjangoTestCase
                | Self::DjangoDatabaseTestCase
        )
    }
}

#[derive(Clone, Eq, PartialEq)]
enum DjangoSettings {
    Unset,
    Module(String),
    Unknown,
}

struct Scope<'a> {
    file: usize,
    parent: Option<ScopeId>,
    owner: Option<SymbolId>,
    body: &'a [Statement],
    names: BTreeMap<&'a str, Vec<SymbolId>>,
    open_wildcard: bool,
    namespace_reads: BTreeMap<[usize; 2], NamespaceRead>,
    namespace_copy_cutoffs: BTreeMap<[usize; 2], [usize; 2]>,
    namespace_captures: OnceCell<Vec<SymbolId>>,
    namespace_exclusions: Vec<RangeInclusive<[usize; 2]>>,
    namespace_escape_exclusions: BTreeMap<[usize; 2], Vec<RangeInclusive<[usize; 2]>>>,
    edges: Vec<Edge>,
    imports: Vec<usize>,
    evaluated_imports: Vec<SymbolId>,
    initializer_imports: Vec<(SymbolId, usize)>,
    class_bodies: Vec<ScopeId>,
    activated_apps: Vec<Object>,
    mounts: Vec<(Object, Object)>,
    routes: Vec<(Object, SymbolId, &'static str)>,
    optional_flask_routes: Vec<(Object, SymbolId, &'static str)>,
    django_settings: DjangoSettings,
    starts_django: Vec<frameworks::DjangoStartup>,
    django_signals: Vec<frameworks::DjangoSignalRegistration>,
}

struct NamespaceRead {
    bindings: Vec<SymbolId>,
    exclusions: Vec<RangeInclusive<[usize; 2]>>,
}

enum Binding<'a> {
    Definition(&'a Definition, ScopeId),
    Import(&'a Import),
    Variable(Option<&'a Expression>, Location),
    UnpackItem {
        value: &'a Expression,
        arity: usize,
        index: usize,
        before: Location,
    },
    Parameter,
    Deleted,
}

enum LexicalBinding {
    Found(SymbolId),
    UnboundLocal,
    Missing,
}

struct Symbol<'a> {
    scope: ScopeId,
    name: &'a Name,
    binding: Binding<'a>,
}

#[derive(Clone, Copy)]
struct Edge {
    target: SymbolId,
    kind: &'static str,
    certainty: &'static str,
}

struct ClassInvocation {
    scope: ScopeId,
    at: Location,
    edge: Edge,
    receiver: Receiver,
    deferred_atexit_receiver: Option<SymbolId>,
}

#[derive(Clone)]
struct ReceiverCall {
    scope: ScopeId,
    parameter: SymbolId,
    member: String,
    at: Location,
    bounded_comparison: bool,
}

struct DirectArguments {
    caller: ScopeId,
    at: Location,
    implicit_receiver: bool,
    values: BTreeMap<String, Value>,
    forwarded: BTreeMap<String, SymbolId>,
    fresh_provider: Option<SymbolId>,
    verified: bool,
    conditional: bool,
    arguments_bound: bool,
}

#[derive(Clone, Copy)]
struct CallArguments<'a> {
    scope: ScopeId,
    before: Location,
    at: Location,
    positional: &'a [Expression],
    keywords: &'a [(String, Expression)],
}

struct Export {
    scope: ScopeId,
    at: Location,
    kind: &'static str,
    value: Value,
}

struct ParameterCall {
    scope: ScopeId,
    parameter: SymbolId,
    at: Location,
}

struct PytestFixture {
    name: Option<String>,
    autouse: bool,
}

struct CallableBinding {
    target: SymbolId,
    bindings: Vec<SymbolId>,
}

type PytestNames<'a> = BTreeMap<&'a str, Vec<SymbolId>>;

struct PytestFixtureChoices {
    alternatives: Vec<CallableBinding>,
    may_be_absent: bool,
}

struct PytestFixtureRequests<'a> {
    tests: &'a [(CallableBinding, bool)],
    certain_case: bool,
}

struct LazyBranch<'a> {
    scope: ScopeId,
    imports: &'a [Statement],
    value: &'a Expression,
    at: Location,
    argument: Option<(SymbolId, &'a str)>,
}

#[derive(Clone, Copy)]
enum ValueEvaluation<'a> {
    Identity,
    Lazy {
        execution_scope: Option<ScopeId>,
        argument: Option<(SymbolId, &'a str)>,
    },
}

enum LazyMember<'a> {
    Branch(LazyBranch<'a>),
    Missing,
    Unknown,
    Conditional,
}

enum ModuleMember<'a> {
    Binding(SymbolId, Option<SymbolId>),
    Submodule(usize, Vec<SymbolId>),
    Lazy(SymbolId, LazyBranch<'a>),
    Missing,
    UnattachedSubmodule,
    Unknown,
    Ambiguous,
    ConditionalHook,
    OpaqueHook,
}

#[derive(Clone, Copy)]
enum ModuleMemberAccess<'a> {
    Attribute,
    FromImport,
    Export {
        at: Location,
        initialization_edges: &'a [Vec<ScopeId>],
        initialization_imports: &'a OnceCell<BTreeSet<SymbolId>>,
    },
}

enum SubmoduleAttachment {
    Attached(SymbolId),
    Rebound(SymbolId),
    Absent,
    Ambiguous,
}

#[cfg(test)]
fn analyze(files: &[AnalyzedFile], entrypoints: &[Entrypoint]) -> Analysis {
    analyze_with_verification(
        files,
        entrypoints,
        None,
        None,
        None,
        false,
        FrameworkInputs::default(),
    )
}

#[cfg(test)]
fn analyze_phase03(files: &[AnalyzedFile], entrypoints: &[Entrypoint]) -> Analysis {
    analyze_with_verification(
        files,
        entrypoints,
        None,
        None,
        None,
        true,
        FrameworkInputs::default(),
    )
}

#[derive(Default)]
struct NestedPackages<'a> {
    roots: Vec<Option<&'a Path>>,
    names: BTreeMap<(&'a Path, &'a str), &'a str>,
}

impl<'a> NestedPackages<'a> {
    fn index(files: &'a [AnalyzedFile]) -> Self {
        let packages: BTreeMap<_, _> = files
            .iter()
            .filter(|file| file.is_package)
            .filter_map(|file| Some((Path::new(&file.path).parent()?, file.module.as_str())))
            .collect();
        let names: BTreeMap<_, _> = packages
            .iter()
            .filter_map(|(&package, &module)| {
                let root = package.parent()?;
                let name = package.file_name()?.to_str()?;
                // A regular package beneath a non-package directory can be
                // imported by its short name from that source root.
                (module != name
                    && skarmory_python::simple_name(name)
                    && !packages.contains_key(root))
                .then_some(((root, name), module))
            })
            .collect();
        let roots = files
            .iter()
            .map(|file| {
                Path::new(&file.path)
                    .ancestors()
                    .skip(1)
                    .find(|directory| !packages.contains_key(directory))
            })
            .collect();
        Self { roots, names }
    }

    fn resolve<'b>(&self, file: usize, module: &'b str) -> Cow<'b, str> {
        let Some(root) = self.roots.get(file).copied().flatten() else {
            return Cow::Borrowed(module);
        };
        let top = module.split('.').next().unwrap_or(module);
        self.names
            .get(&(root, top))
            .map_or(Cow::Borrowed(module), |package| {
                Cow::Owned(format!("{package}{}", &module[top.len()..]))
            })
    }
}

#[derive(Clone, Copy, Default)]
pub(super) struct FrameworkInputs<'a> {
    pub(super) settings: Option<&'a OsStr>,
    pub(super) templates: Option<&'a TemplateProject<'a>>,
    pub(super) gin: Option<&'a GinProject<'a>>,
    pub(super) discovery: Option<&'a crate::preview_source::Project>,
    pub(super) unresolved_project_wildcards: bool,
    #[cfg(test)]
    pub(super) fastapi_call_event_limit: Option<usize>,
}

pub(super) fn analyze_with_verification(
    files: &[AnalyzedFile],
    entrypoints: &[Entrypoint],
    verification: Option<&grep_verification::Snapshot>,
    package_entrypoints: Option<&package_entrypoints::Snapshot>,
    runtime: Option<&runtime_evidence::Snapshot>,
    phase03: bool,
    framework: FrameworkInputs<'_>,
) -> Analysis {
    let mut graph = Graph {
        files,
        discovery: framework.discovery,
        modules: BTreeMap::new(),
        nested_packages: if phase03 {
            NestedPackages::index(files)
        } else {
            NestedPackages::default()
        },
        scopes: Vec::new(),
        symbols: Vec::new(),
        direct_submodule_imports: BTreeMap::new(),
        by_location: BTreeMap::new(),
        read: BTreeSet::new(),
        namespace_exports: BTreeMap::new(),
        direct: BTreeSet::new(),
        conditional: BTreeMap::new(),
        conditional_groups: BTreeMap::new(),
        next_conditional_group: 0,
        evaluated_module_booleans: BTreeMap::new(),
        branch_depth: 0,
        dormant_classes: BTreeSet::new(),
        subclassed: BTreeSet::new(),
        instance_receivers: BTreeMap::new(),
        class_invocations: Vec::new(),
        atexit_receivers: BTreeMap::new(),
        method_receivers: BTreeMap::new(),
        receiver_calls: Vec::new(),
        fresh_field_calls: BTreeMap::new(),
        fresh_field_invocations: Vec::new(),
        constructor_hook_stores: Vec::new(),
        source_provider_calls: BTreeMap::new(),
        source_provider_invalidated: None,
        source_provider_opaque_arguments: BTreeSet::new(),
        resolved_receiver_calls: BTreeSet::new(),
        receiver_call_issues: BTreeSet::new(),
        issues: Vec::new(),
        scope_issues: Vec::new(),
        probing_identity: false,
        check_dormant_declarations: false,
        nested_lazy_imports: Vec::new(),
        uncertainties: Vec::new(),
        fixtures: BTreeMap::new(),
        direct_arguments: BTreeMap::new(),
        executor_arguments: BTreeMap::new(),
        parameter_calls: Vec::new(),
        static_parameter_calls: Vec::new(),
        closed_class_replacements: None,
        class_argument_consumers: BTreeMap::new(),
        has_ordering_hooks: false,
        has_equality_hooks: false,
        has_hash_hooks: false,
        has_bool_hooks: false,
        has_length_hooks: false,
        has_format_hooks: false,
        has_iteration_hooks: false,
        iteration_locals: Vec::new(),
        grep_verify: verification.is_some(),
        dynamic_references: BTreeSet::new(),
        phase03,
        terminal_proofs: entrypoints.is_empty()
            && package_entrypoints.is_none_or(|snapshot| snapshot.entries.is_empty())
            && runtime.is_none_or(|snapshot| snapshot.observations.is_empty()),
        failed_terminal_modules: BTreeSet::new(),
        failed_flask_provider_initializers: BTreeMap::new(),
        failed_flask_import_initializers: BTreeMap::new(),
        flask_initializer_probe_cutoffs: BTreeMap::new(),
        mutations: mutations::Mutations::default(),
        metaclasses: BTreeMap::new(),
        framework_lifecycle: Vec::new(),
        framework_class_whitelists: BTreeMap::new(),
        framework_roots: Vec::new(),
        framework_fields: BTreeSet::new(),
        pydantic_serialization: pydantic::Serialization::default(),
        mcp_registrations: BTreeMap::new(),
        gin_registrations: BTreeMap::new(),
        gin_config_calls: Vec::new(),
        celery_backends: celery::Backends::default(),
        celery_routes: celery_routes::Routes::default(),
        fastapi_dependency_issues: Vec::new(),
        fastapi: fastapi::Fastapi::default(),
        flask_hooks: Vec::new(),
        flask_context_writes: None,
        django_roots: BTreeSet::new(),
        django_middleware_sites: Vec::new(),
        logging_os_initializers: BTreeMap::new(),
        logging_record_calls: Vec::new(),
        logging_formatter_methods: BTreeSet::new(),
        django_view_arguments: BTreeMap::new(),
        django_failed_view_initializers: BTreeMap::new(),
        django_globals: django_globals::Globals::default(),
        rejected_django_callbacks: BTreeSet::new(),
        django_converter: frameworks::DjangoConverter::default(),
        django_autoreload_wrappers: Vec::new(),
        django_autoreload_method_arguments: BTreeMap::new(),
        django_configured_settings: None,
        django_startup_scopes: BTreeSet::new(),
        django_ready_hooks: BTreeSet::new(),
        django_settings_sources: BTreeSet::new(),
        django_template_apps: BTreeMap::new(),
        django_registration_issues: Vec::new(),
        framework_route_receivers: BTreeMap::new(),
        registered_django_admin_models: BTreeSet::new(),
        initial_django_settings: framework.settings.map_or(DjangoSettings::Unset, |value| {
            value.to_str().map_or(DjangoSettings::Unknown, |value| {
                DjangoSettings::Module(value.to_owned())
            })
        }),
        unittest_classes: BTreeMap::new(),
        pytest_plugins: Vec::new(),
        pytest_unittest_cases: BTreeMap::new(),
    };
    for (file, source) in files.iter().enumerate() {
        graph.push_scope(file, None, None, &source.facts.body);
    }
    for (file, source) in files.iter().enumerate() {
        if graph.modules.insert(source.module.as_str(), file).is_some() {
            graph.issue(
                file,
                Location { line: 1, column: 1 },
                "ambiguous_module",
                "multiple selected files have the same module name",
            );
        }
    }
    for (file, source) in files.iter().enumerate() {
        graph.index(file, &source.facts.body);
    }
    if framework.unresolved_project_wildcards {
        // An unresolved star import may add or replace arbitrary bindings.
        // Preserve source declarations without inferring any runtime evidence.
        return Analysis {
            definitions: (0..graph.symbols.len())
                .filter_map(|id| graph.definition_context(id))
                .collect(),
            issues: graph.issues,
            ..Analysis::default()
        };
    }
    if phase03 {
        graph.index_logging_os_initializers();
    }
    for symbol in &graph.symbols {
        if graph.scopes[symbol.scope]
            .owner
            .is_some_and(|id| graph.is_class(id))
        {
            match symbol.name.text.as_str() {
                "__lt__" | "__le__" | "__gt__" | "__ge__" => graph.has_ordering_hooks = true,
                "__eq__" => graph.has_equality_hooks = true,
                "__hash__" => graph.has_hash_hooks = true,
                "__bool__" => graph.has_bool_hooks = true,
                "__len__" => graph.has_length_hooks = true,
                "__format__" | "__str__" | "__repr__" => graph.has_format_hooks = true,
                "__iter__" | "__next__" | "__getitem__" => graph.has_iteration_hooks = true,
                _ => {}
            }
        }
    }
    graph.index_submodule_imports();
    if phase03 {
        graph.index_metaclasses();
        graph.index_framework_mutations();
    }
    graph.index_base_classes();
    if phase03 {
        graph.index_django_global_writes();
        graph.index_failed_flask_provider_initializers();
        graph.index_failed_django_view_initializers();
        graph.index_flask_context();
    }
    for scope in 0..graph.scopes.len() {
        let body = graph.scopes[scope].body;
        graph.suite(scope, body);
    }
    if phase03 {
        graph.resolve_fresh_field_invocations();
        graph.collect_logging_configs();
        graph.collect_django_converter_wrappers();
        graph.index_fastapi_parameter_routes();
        graph.index_fastapi_mounts();
        graph.collect_fastapi_deployment_factory();
        graph.collect_conventional_app(ObjectKind::SanicApp);
        graph.collect_conventional_app(ObjectKind::StarletteApp);
        graph.collect_gunicorn_config();
        graph.collect_phase03_frameworks();
        graph.collect_pydantic_serialization();
        graph.collect_mcp_registrations();
        graph.collect_celery_backends();
        graph.collect_celery_routes();
    }
    graph.resolve_receiver_calls();
    graph.resolve_class_invocations();
    let callbacks = graph.initialization_callbacks();
    graph.resolve_receiver_calls();
    graph.resolve_class_invocations();
    let Some(initialization_edges) = graph.acyclic_initialization_edges(&callbacks) else {
        graph.issues.push(Issue::new(
            "cyclic_imports",
            ".",
            "cyclic module initialization cannot use final module bindings to prove non-use",
        ));
        return Analysis {
            issues: graph.issues,
            ..Analysis::default()
        };
    };
    if phase03 {
        graph.collect_gin_bindings(framework.gin);
        let limit = fastapi::MAX_INITIALIZER_CALL_EVENTS;
        #[cfg(test)]
        let limit = framework.fastapi_call_event_limit.unwrap_or(limit);
        graph.bound_fastapi_initializers(limit);
    }
    let mut roots = graph.external_roots(entrypoints, &initialization_edges);
    if let Some(package_entrypoints) = package_entrypoints {
        graph.retain_package_entrypoints(package_entrypoints, &initialization_edges);
    }
    roots.extend(graph.test_edges());
    let mut possible_roots = Vec::new();
    if phase03 {
        for edge in graph.phase03_framework_edges() {
            if matches!(
                edge.kind,
                "pytest_hook_candidate" | "flask_cli_factory" | "fastapi_deployment_factory"
            ) {
                possible_roots.push(edge);
            } else {
                roots.push(edge);
            }
        }
    }
    // A dormant consumer must not consume another scope's export traversal.
    for exports in std::mem::take(&mut graph.namespace_exports).into_values() {
        graph.retain_exports(exports, &initialization_edges, "possible");
    }
    let (observed, rejected) = runtime
        .filter(|snapshot| !snapshot.observations.is_empty())
        .map_or_else(Default::default, |snapshot| {
            graph.runtime_roots(snapshot, &mut roots, &mut possible_roots)
        });
    graph.report_failed_terminal_module_consumers();
    let mut analysis = graph.finish(roots, possible_roots, verification, framework.templates);
    analysis.runtime_evidence = observed;
    analysis.runtime_evidence_issues = rejected;
    analysis
}

struct Graph<'a> {
    files: &'a [AnalyzedFile],
    discovery: Option<&'a crate::preview_source::Project>,
    modules: BTreeMap<&'a str, usize>,
    nested_packages: NestedPackages<'a>,
    scopes: Vec<Scope<'a>>,
    symbols: Vec<Symbol<'a>>,
    direct_submodule_imports: BTreeMap<usize, Vec<SymbolId>>,
    by_location: BTreeMap<(ScopeId, usize, usize), SymbolId>,
    read: BTreeSet<SymbolId>,
    namespace_exports: BTreeMap<ScopeId, Vec<Export>>,
    direct: BTreeSet<SymbolId>,
    conditional: BTreeMap<SymbolId, Vec<RangeInclusive<[usize; 2]>>>,
    conditional_groups: BTreeMap<SymbolId, Vec<(usize, bool)>>,
    next_conditional_group: usize,
    evaluated_module_booleans: BTreeMap<SymbolId, bool>,
    branch_depth: usize,
    dormant_classes: BTreeSet<SymbolId>,
    subclassed: BTreeSet<SymbolId>,
    instance_receivers: BTreeMap<SymbolId, SymbolId>,
    class_invocations: Vec<ClassInvocation>,
    atexit_receivers: BTreeMap<ScopeId, Vec<(Edge, SymbolId)>>,
    method_receivers: BTreeMap<SymbolId, BTreeSet<SymbolId>>,
    receiver_calls: Vec<ReceiverCall>,
    fresh_field_calls: BTreeMap<SymbolId, Location>,
    fresh_field_invocations: Vec<field_dispatch::FreshFieldInvocation>,
    constructor_hook_stores: Vec<field_dispatch::ConstructorHookStore>,
    source_provider_calls: BTreeMap<SymbolId, source_provider_calls::SourceProviderCall<'a>>,
    source_provider_invalidated: Option<BTreeSet<SymbolId>>,
    source_provider_opaque_arguments: BTreeSet<SymbolId>,
    resolved_receiver_calls: BTreeSet<(usize, SymbolId)>,
    receiver_call_issues: BTreeSet<usize>,
    issues: Vec<Issue>,
    scope_issues: Vec<(ScopeId, Issue)>,
    probing_identity: bool,
    check_dormant_declarations: bool,
    nested_lazy_imports: Vec<(ScopeId, &'a [Statement])>,
    uncertainties: Vec<Issue>,
    fixtures: BTreeMap<SymbolId, PytestFixture>,
    direct_arguments: BTreeMap<SymbolId, Vec<DirectArguments>>,
    executor_arguments: BTreeMap<SymbolId, Vec<DirectArguments>>,
    parameter_calls: Vec<ParameterCall>,
    static_parameter_calls: Vec<class_parameter_calls::StaticParameterCall>,
    closed_class_replacements: Option<BTreeSet<SymbolId>>,
    class_argument_consumers: BTreeMap<SymbolId, BTreeSet<(ScopeId, SymbolId)>>,
    has_ordering_hooks: bool,
    has_equality_hooks: bool,
    has_hash_hooks: bool,
    has_bool_hooks: bool,
    has_length_hooks: bool,
    has_format_hooks: bool,
    has_iteration_hooks: bool,
    iteration_locals: Vec<([usize; 2], Value)>,
    grep_verify: bool,
    dynamic_references: BTreeSet<(ScopeId, SymbolId, usize, usize)>,
    phase03: bool,
    terminal_proofs: bool,
    failed_terminal_modules: BTreeSet<ScopeId>,
    failed_flask_provider_initializers: BTreeMap<ScopeId, Location>,
    failed_flask_import_initializers: BTreeMap<ScopeId, Location>,
    flask_initializer_probe_cutoffs: BTreeMap<ScopeId, Location>,
    mutations: mutations::Mutations,
    metaclasses: BTreeMap<SymbolId, metaclasses::ClassCreation>,
    framework_lifecycle: Vec<(SymbolId, SymbolId, &'static str)>,
    framework_class_whitelists: BTreeMap<SymbolId, &'static str>,
    framework_roots: Vec<Edge>,
    framework_fields: BTreeSet<SymbolId>,
    pydantic_serialization: pydantic::Serialization,
    mcp_registrations: BTreeMap<(Object, String), Vec<mcp_tools::Registration>>,
    gin_registrations: BTreeMap<String, Vec<gin::Registration>>,
    gin_config_calls: Vec<gin::ConfigCall>,
    celery_backends: celery::Backends,
    celery_routes: celery_routes::Routes,
    fastapi_dependency_issues: Vec<(SymbolId, Issue)>,
    fastapi: fastapi::Fastapi,
    flask_hooks: Vec<flask::ParameterHook>,
    flask_context_writes: Option<BTreeSet<String>>,
    django_roots: BTreeSet<Object>,
    django_middleware_sites: Vec<django_middleware::MiddlewareSite>,
    logging_os_initializers: BTreeMap<ScopeId, logging_config::OsInitializer>,
    logging_record_calls: Vec<ParameterCall>,
    logging_formatter_methods: BTreeSet<SymbolId>,
    django_view_arguments: django_views::ViewRouteArguments,
    django_failed_view_initializers: BTreeMap<ScopeId, Location>,
    django_globals: django_globals::Globals,
    rejected_django_callbacks: BTreeSet<SymbolId>,
    django_converter: frameworks::DjangoConverter,
    django_autoreload_wrappers: Vec<frameworks::DjangoParameterRegistration>,
    django_autoreload_method_arguments: BTreeMap<SymbolId, Vec<DirectArguments>>,
    django_configured_settings: Option<usize>,
    django_startup_scopes: BTreeSet<ScopeId>,
    django_ready_hooks: BTreeSet<SymbolId>,
    django_settings_sources: BTreeSet<(ScopeId, ScopeId)>,
    django_template_apps: BTreeMap<ScopeId, BTreeMap<usize, Option<String>>>,
    django_registration_issues: Vec<(Object, Issue)>,
    framework_route_receivers: BTreeMap<(ScopeId, Object, SymbolId), BTreeSet<SymbolId>>,
    registered_django_admin_models: BTreeSet<(ScopeId, SymbolId)>,
    initial_django_settings: DjangoSettings,
    unittest_classes: BTreeMap<SymbolId, FrameworkClassRole>,
    pytest_plugins: Vec<ScopeId>,
    pytest_unittest_cases: BTreeMap<ScopeId, Vec<(PytestNames<'a>, bool)>>,
}

impl<'a> Graph<'a> {
    fn push_scope(
        &mut self,
        file: usize,
        parent: Option<ScopeId>,
        owner: Option<SymbolId>,
        body: &'a [Statement],
    ) -> ScopeId {
        let index = self.scopes.len();
        self.scopes.push(Scope {
            file,
            parent,
            owner,
            body,
            names: BTreeMap::new(),
            open_wildcard: false,
            namespace_reads: BTreeMap::new(),
            namespace_copy_cutoffs: BTreeMap::new(),
            namespace_captures: OnceCell::new(),
            namespace_exclusions: Vec::new(),
            namespace_escape_exclusions: BTreeMap::new(),
            edges: Vec::new(),
            imports: Vec::new(),
            evaluated_imports: Vec::new(),
            initializer_imports: Vec::new(),
            class_bodies: Vec::new(),
            activated_apps: Vec::new(),
            mounts: Vec::new(),
            routes: Vec::new(),
            optional_flask_routes: Vec::new(),
            django_settings: self.initial_django_settings.clone(),
            starts_django: Vec::new(),
            django_signals: Vec::new(),
        });
        index
    }

    fn bind(&mut self, scope: ScopeId, name: &'a Name, binding: Binding<'a>) -> SymbolId {
        let id = self.symbols.len();
        self.symbols.push(Symbol {
            scope,
            name,
            binding,
        });
        self.scopes[scope]
            .names
            .entry(&name.text)
            .or_default()
            .push(id);
        if !matches!(self.symbols[id].binding, Binding::Import(import) if import.wildcard_member) {
            self.by_location
                .insert((scope, name.at.line, name.at.column), id);
        }
        id
    }

    fn index(&mut self, scope: ScopeId, body: &'a [Statement]) {
        for statement in body {
            match &statement.kind {
                StatementKind::Define(definition) => {
                    let file = self.scopes[scope].file;
                    let id = self.symbols.len();
                    let child = self.push_scope(file, Some(scope), Some(id), &definition.body);
                    self.bind(
                        scope,
                        &definition.name,
                        Binding::Definition(definition, child),
                    );
                    for parameter in &definition.parameters {
                        self.bind(child, parameter, Binding::Parameter);
                    }
                    self.index(child, &definition.body);
                }
                StatementKind::Import(import) => {
                    self.bind(scope, &import.local, Binding::Import(import));
                }
                StatementKind::OptionalProjectClassFallback {
                    package,
                    missing_member,
                    fallback,
                } if self
                    .optional_project_package(scope, statement.at, package, missing_member)
                    .is_some() =>
                {
                    self.index(scope, fallback);
                }
                StatementKind::ProviderWildcardImport(imports)
                    if self.supported_provider_wildcard(scope, statement.at, imports) =>
                {
                    // typing has a finite __all__. FastAPI additionally exposes
                    // implementation modules and other undocumented names.
                    self.scopes[scope].open_wildcard = imports
                        .first()
                        .is_some_and(|import| import.module == "fastapi");
                    for import in imports {
                        self.bind(scope, &import.local, Binding::Import(import));
                    }
                }
                StatementKind::Assign { names, value, .. } => {
                    for name in names {
                        self.bind(scope, name, Binding::Variable(value.as_ref(), statement.at));
                    }
                }
                StatementKind::AssignUnpack { names, value } => {
                    let items = match &value.kind {
                        ExpressionKind::Sequence { items, .. } if items.len() == names.len() => {
                            Some(items)
                        }
                        _ => None,
                    };
                    let split = self.phase03
                        && matches!(&value.kind, ExpressionKind::Call { function, .. }
                            if matches!(&function.kind, ExpressionKind::Member(_, method) if method == "split"));
                    for (index, name) in names.iter().enumerate() {
                        // Every RHS element sees the namespace before any of the stores.
                        let binding = if split {
                            Binding::UnpackItem {
                                value,
                                arity: names.len(),
                                index,
                                before: statement.at,
                            }
                        } else {
                            Binding::Variable(
                                items.and_then(|items| items.get(index)),
                                statement.at,
                            )
                        };
                        self.bind(scope, name, binding);
                    }
                }
                StatementKind::DeleteName(name)
                    if self.phase03 && scope == self.scopes[scope].file =>
                {
                    self.bind(scope, name, Binding::Deleted);
                }
                StatementKind::LiteralRouteLoop { path, last, .. } => {
                    self.bind(scope, path, Binding::Variable(Some(last), statement.at));
                }
                StatementKind::With { items, body } => {
                    for (expression, name) in items {
                        if let Some(name) = name {
                            self.bind(
                                scope,
                                name,
                                Binding::Variable(Some(expression), statement.at),
                            );
                        }
                    }
                    self.index(scope, body);
                }
                StatementKind::If {
                    test,
                    body,
                    otherwise,
                } => {
                    if let Some(enabled) = static_bool(test) {
                        self.index(scope, if enabled { body } else { otherwise });
                        continue;
                    }
                    let first_symbol = self.symbols.len();
                    match self.indexed_bool(scope, test, statement.at, 0) {
                        Some(true) => {
                            self.index(scope, body);
                            self.index_dormant(scope, otherwise);
                        }
                        Some(false) => {
                            self.index_dormant(scope, body);
                            self.index(scope, otherwise);
                        }
                        None => {
                            let group = self.next_conditional_group;
                            self.next_conditional_group += 1;
                            self.index(scope, body);
                            self.mark_conditional(scope, first_symbol, body, group, true);
                            let first_symbol = self.symbols.len();
                            self.index(scope, otherwise);
                            self.mark_conditional(scope, first_symbol, otherwise, group, false);
                        }
                    }
                }
                _ => {}
            }
        }
    }

    fn optional_project_package(
        &self,
        scope: ScopeId,
        at: Location,
        package: &str,
        missing_member: &str,
    ) -> Option<ScopeId> {
        if !self.phase03
            || !self.terminal_proofs
            || scope != self.scopes[scope].file
            || self.local(scope, "ImportError", at).is_some()
            || self.local(scope, "__builtins__", at).is_some()
        {
            return None;
        }
        let project = self.discovery?;
        let &file = self.modules.get(package)?;
        let initializer = &self.files[file];
        // An inert regular package has a fixed lookup path and no lazy
        // attribute hook. The discovery snapshot covers source and opaque
        // import candidates, including names excluded from analysis.
        (initializer.is_package
            && initializer.facts.body.is_empty()
            && project.root_package_source_unambiguous(
                &initializer.path,
                &initializer.path,
                package,
                "__init__",
            )
            && project.root_package_submodule_absent(&initializer.path, missing_member))
        .then_some(file)
    }

    fn indexed_bool(
        &self,
        scope: ScopeId,
        expression: &Expression,
        before: Location,
        depth: usize,
    ) -> Option<bool> {
        if depth >= MAX_BINDING_CHAIN {
            return None;
        }
        match &expression.kind {
            ExpressionKind::Bool(value) => Some(*value),
            ExpressionKind::Not(value) => self
                .indexed_bool(scope, value, before, depth + 1)
                .map(|value| !value),
            // Module guards can use prior local bindings during indexing.
            // Nested scopes can depend on bindings that are still being indexed.
            ExpressionKind::Name(name) if self.scopes[scope].file == scope => {
                let id = self.local(scope, name, before)?;
                if self.binding_depends_on_branch(id, before) {
                    return None;
                }
                let Binding::Variable(Some(value), assigned) = self.symbols[id].binding else {
                    return None;
                };
                self.indexed_bool(scope, value, assigned, depth + 2)
            }
            _ => None,
        }
    }

    fn index_dormant(&mut self, scope: ScopeId, body: &'a [Statement]) {
        let first = self.symbols.len();
        self.index(scope, body);
        // These symbols are still reportable. They never change the module's
        // effective bindings at runtime.
        let names = &mut self.scopes[scope].names;
        for id in (first..self.symbols.len()).rev() {
            let symbol = &self.symbols[id];
            if matches!(symbol.binding, Binding::Definition(definition, _)
                if definition.kind == DefinitionKind::Class)
            {
                self.dormant_classes.insert(id);
            }
            if symbol.scope == scope
                && let Entry::Occupied(mut entry) = names.entry(symbol.name.text.as_str())
                && entry.get().last() == Some(&id)
            {
                entry.get_mut().pop();
                if entry.get().is_empty() {
                    entry.remove();
                }
            }
        }
    }

    fn mark_conditional(
        &mut self,
        scope: ScopeId,
        first_symbol: usize,
        body: &[Statement],
        group: usize,
        branch: bool,
    ) {
        let Some(first) = body.first() else { return };
        let end = suite_end(body);
        for id in first_symbol..self.symbols.len() {
            if self.symbols[id].scope == scope {
                self.conditional_groups
                    .entry(id)
                    .or_default()
                    .push((group, branch));
                self.conditional
                    .entry(id)
                    .or_default()
                    .push(position(first.at)..=end);
            }
        }
    }

    fn issue(&mut self, file: usize, at: Location, kind: &'static str, message: impl ToString) {
        self.issues
            .push(Issue::new(kind, &self.files[file].path, message).at(at));
    }

    fn scope_issue(
        &mut self,
        scope: ScopeId,
        at: Location,
        kind: &'static str,
        message: impl ToString,
    ) {
        // Speculative identity lookups can inform the graph. A runtime
        // diagnostic requires an evaluated read.
        if self.probing_identity {
            return;
        }
        let file = &self.files[self.scopes[scope].file];
        self.scope_issues
            .push((scope, Issue::new(kind, &file.path, message).at(at)));
    }

    fn uncertain(&mut self, scope: ScopeId, at: Location, message: &str) {
        self.uncertainties.push(
            Issue::new(
                "possible_use",
                &self.files[self.scopes[scope].file].path,
                message,
            )
            .at(at),
        );
    }

    fn local(&self, scope: ScopeId, name: &str, before: Location) -> Option<SymbolId> {
        self.scopes[scope]
            .names
            .get(name)?
            .iter()
            .copied()
            .rev()
            .find(|&id| {
                let at = self.symbols[id].name.at;
                (at.line, at.column) < (before.line, before.column)
            })
    }

    fn supported_provider_wildcard(
        &self,
        scope: ScopeId,
        at: Location,
        imports: &[Import],
    ) -> bool {
        let row = &self.scopes[scope];
        self.phase03
            && row.parent.is_none()
            && imports.first().is_some_and(|import| {
                let module = self.nested_packages.resolve(row.file, &import.module);
                !self.modules.contains_key(module.as_ref())
            })
            && row
                .body
                .iter()
                .take_while(|statement| position(statement.at) < position(at))
                .all(|statement| {
                    matches!(
                        &statement.kind,
                        StatementKind::Evaluate(Expression {
                            kind: ExpressionKind::Text(_),
                            ..
                        })
                    )
                })
            && row
                .body
                .iter()
                .any(|statement| position(statement.at) == position(at))
    }

    fn lexical(&self, scope: ScopeId, name: &str, before: Location) -> Option<SymbolId> {
        match self.lexical_binding(scope, name, before, false) {
            LexicalBinding::Found(id) => Some(id),
            LexicalBinding::UnboundLocal | LexicalBinding::Missing => None,
        }
    }

    fn lexical_binding(
        &self,
        origin: ScopeId,
        name: &str,
        before: Location,
        check_dormant_declarations: bool,
    ) -> LexicalBinding {
        let mut scope = origin;
        let mut before = before;
        loop {
            let row = &self.scopes[scope];
            let class = row.owner.is_some_and(|id| self.is_class(id));
            // A class body sees its own namespace, but nested scopes do not
            // capture a class namespace as an enclosing lexical scope.
            if scope == origin || !class {
                if let Some(id) = self.local(scope, name, before) {
                    return LexicalBinding::Found(id);
                }
                if row.owner.is_some()
                    && !class
                    && (row.names.contains_key(name)
                        || (check_dormant_declarations || self.is_namespace_builtin(name))
                            && suite_binds_name(row.body, name))
                {
                    // A lexical declaration can shadow an outer binding even
                    // in a branch omitted from the runtime binding index.
                    return LexicalBinding::UnboundLocal;
                }
            }
            let Some(parent) = row.parent else {
                return LexicalBinding::Missing;
            };
            scope = parent;
            if !self.phase03 || !class {
                before = END;
            }
        }
    }

    fn immediately_encloses_class_body(&self, mut scope: ScopeId, outer: ScopeId) -> bool {
        if !self.phase03 {
            return false;
        }
        while scope != outer {
            let row = &self.scopes[scope];
            if !row.owner.is_some_and(|owner| self.is_class(owner)) {
                return false;
            }
            scope = row.parent.expect("class body has an enclosing scope");
        }
        true
    }

    fn is_class(&self, id: SymbolId) -> bool {
        matches!(self.symbols[id].binding, Binding::Definition(def, _) if def.kind == DefinitionKind::Class)
    }

    fn index_base_classes(&mut self) {
        for id in 0..self.symbols.len() {
            let scope = self.symbols[id].scope;
            let Binding::Definition(definition, _) = self.symbols[id].binding else {
                continue;
            };
            if definition.kind == DefinitionKind::Class && !self.dormant_classes.contains(&id) {
                for base in &definition.bases {
                    if let Value::Callable(Invocation { target: base, .. }) =
                        self.probe_identity(scope, base, definition.name.at)
                        && self.is_class(base)
                    {
                        self.subclassed.insert(base);
                    }
                }
            }
        }
    }

    fn definition_context(&self, id: SymbolId) -> Option<(String, DefinitionContext)> {
        let symbol = &self.symbols[id];
        let kind = match symbol.binding {
            Binding::Deleted => return None,
            Binding::Import(import) if import.wildcard_member => return None,
            Binding::Definition(def, _) if def.kind == DefinitionKind::Class => "class",
            Binding::Definition(..) => "function",
            Binding::Import(_) => "import",
            Binding::Variable(..) | Binding::UnpackItem { .. } => "variable",
            Binding::Parameter => "parameter",
        };
        let file = &self.files[self.scopes[symbol.scope].file].path;
        let Location { line, column } = symbol.name.at;
        let qualified = self.full_name(id);
        Some((
            format!("{file}:{line}:{column}:{qualified}"),
            DefinitionContext {
                name: qualified,
                kind,
                file: file.clone(),
                line,
                column,
            },
        ))
    }

    fn full_name(&self, id: SymbolId) -> String {
        let symbol = &self.symbols[id];
        let mut names = vec![symbol.name.text.as_str()];
        let mut scope = symbol.scope;
        while let Some(owner) = self.scopes[scope].owner {
            let entry = &self.symbols[owner];
            names.push(&entry.name.text);
            scope = entry.scope;
        }
        names.push(&self.files[self.scopes[scope].file].module);
        names.reverse();
        names.join(".")
    }

    fn visible_import_module<'b>(
        &self,
        file: usize,
        import: &'b Import,
        module: &'b str,
    ) -> Cow<'b, str> {
        if import.exposes_top_level {
            let top = import.module.split('.').next().unwrap_or(&import.module);
            self.nested_packages.resolve(file, top)
        } else {
            Cow::Borrowed(module)
        }
    }

    fn import_module(&self, file: usize, import: &Import) -> Option<String> {
        if import.level == 0 {
            return Some(
                self.nested_packages
                    .resolve(file, &import.module)
                    .into_owned(),
            );
        }
        let source = &self.files[file];
        let mut parts: Vec<_> = source.module.split('.').collect();
        if !source.is_package {
            parts.pop();
        }
        if import.level as usize > parts.len() {
            return None;
        }
        parts.truncate(parts.len() + 1 - import.level as usize);
        if !import.module.is_empty() {
            parts.push(&import.module);
        }
        Some(parts.join("."))
    }

    fn import_member_position(
        &self,
        scope: ScopeId,
        module: &str,
        import: &Import,
    ) -> Option<Location> {
        let file = self.scopes[scope].file;
        if scope == file
            && self.files[file].module == module
            && (import.level > 0
                || self.phase03 && self.files[file].is_package && import.member.is_some())
        {
            return Some(import.local.at);
        }
        if self.phase03
            && scope == file
            && let Some(member) = &import.member
        {
            return self.flask_blueprint_import_position(file, module, member);
        }
        None
    }

    fn index_submodule_imports(&mut self) {
        for (id, symbol) in self.symbols.iter().enumerate() {
            let Binding::Import(import) = symbol.binding else {
                continue;
            };
            if !self.scopes[symbol.scope]
                .names
                .get(symbol.name.text.as_str())
                .is_some_and(|ids| ids.binary_search(&id).is_ok())
            {
                continue;
            }
            let source = self.scopes[symbol.scope].file;
            let Some(module) = self.import_module(source, import) else {
                continue;
            };
            let unconditional = symbol.scope == source && !self.conditional.contains_key(&id);
            let module_file = self.modules.get(module.as_str()).copied();
            let fallback =
                module_file
                    .zip(import.member.as_deref())
                    .and_then(|(package, member)| {
                        self.fallback_submodule(package, member, import, symbol.scope)
                    });
            let loaded_child = fallback
                .filter(|_| module_file.is_some_and(|package| self.files[package].is_package));
            let loads_module = unconditional
                && module_file.is_some_and(|file| {
                    loaded_child.is_some()
                        || import.member.as_ref().is_none_or(|member| {
                            self.local(file, member, END)
                                .is_some_and(|binding| !self.conditional.contains_key(&binding))
                        })
                });
            for end in module
                .match_indices('.')
                .map(|(end, _)| end)
                .chain(std::iter::once(module.len()))
            {
                let name = &module[..end];
                let Some(&target) = self.modules.get(name) else {
                    continue;
                };
                if name.contains('.') {
                    self.direct_submodule_imports
                        .entry(target)
                        .or_default()
                        .push(id);
                }
                // A successful dotted import also runs its parent packages.
                // The source and its own ancestors may still be initializing.
                if loads_module
                    && (end == module.len() || self.files[target].is_package)
                    && !within_package(&self.files[source].module, name)
                {
                    self.scopes[source].initializer_imports.push((id, target));
                }
            }
            if let Some(child) = fallback {
                self.direct_submodule_imports
                    .entry(child)
                    .or_default()
                    .push(id);
            }
            if unconditional
                && let Some(child) = loaded_child
                && child != source
            {
                self.scopes[source].initializer_imports.push((id, child));
            }
        }
    }

    fn fallback_submodule(
        &self,
        package: usize,
        member: &str,
        import: &Import,
        scope: ScopeId,
    ) -> Option<usize> {
        let file = &self.files[package];
        let child = self
            .modules
            .get(format!("{}.{member}", file.module).as_str())?;
        let before = self
            .import_member_position(scope, &file.module, import)
            .unwrap_or(END);
        // From-import can load a child when ordinary and lazy lookup fail.
        (!has_implicit_module_member(member, file.is_package)
            && self.local(package, member, before).is_none()
            && self
                .local(package, "__getattr__", before)
                .is_none_or(|hook| {
                    matches!(self.lazy_lookup(hook, member, before), LazyMember::Missing)
                }))
        .then_some(*child)
    }

    fn submodule_attachment(
        &self,
        package: usize,
        child: usize,
        binding: Option<SymbolId>,
        access: (ScopeId, Location),
        mode: ModuleMemberAccess<'_>,
    ) -> SubmoduleAttachment {
        let candidates = self.direct_submodule_imports.get(&child);
        let initialization_imports = match mode {
            ModuleMemberAccess::Export {
                initialization_edges,
                initialization_imports,
                ..
            } if candidates.is_some() => Some(
                initialization_imports
                    .get_or_init(|| self.initialization_imports(package, initialization_edges)),
            ),
            _ => None,
        };
        let (scope, before) = access;
        let mut attached: Option<SymbolId> = None;
        let mut possible_attachment = false;
        let mut possible_earlier_attachment = false;
        for &id in candidates.into_iter().flatten() {
            if initialization_imports.is_some_and(|imports| !imports.contains(&id)) {
                continue;
            }
            let symbol = &self.symbols[id];
            let earlier_in_scope =
                symbol.scope == scope && position(symbol.name.at) < position(before);
            let guaranteed = if earlier_in_scope {
                self.conditional
                    .get(&id)
                    .is_none_or(|guards| guards.iter().all(|span| span.contains(&position(before))))
            } else {
                symbol.scope == package
                    && self.scopes[scope].file != package
                    && !self.conditional.contains_key(&id)
            };
            // Other scopes may already have executed. A later statement in
            // this module's own initializer has not yet attached the child.
            let possible = symbol.scope != scope || scope >= self.files.len() || earlier_in_scope;
            if binding.is_some_and(|binding| {
                symbol.scope == package
                    && position(symbol.name.at) <= position(self.symbols[binding].name.at)
            }) {
                // The first successful child import sets the package attribute.
                // A later import from the module cache leaves a reassignment in place.
                if guaranteed {
                    return SubmoduleAttachment::Rebound(id);
                }
                possible_earlier_attachment |= possible;
            } else {
                if guaranteed
                    && attached.is_none_or(|first| {
                        symbol.scope == package && self.symbols[first].scope != package
                    })
                {
                    attached = Some(id);
                }
                possible_attachment |= possible;
            }
        }
        if let Some(import) = attached
            && !possible_earlier_attachment
        {
            SubmoduleAttachment::Attached(import)
        } else if possible_attachment {
            SubmoduleAttachment::Ambiguous
        } else {
            SubmoduleAttachment::Absent
        }
    }

    fn imported_submodule(
        &self,
        child: usize,
        access: (ScopeId, Location),
    ) -> Option<Vec<SymbolId>> {
        let (source, before) = access;
        if source >= self.files.len() {
            return None;
        }
        let attachments: BTreeMap<_, _> = self
            .direct_submodule_imports
            .get(&child)?
            .iter()
            .copied()
            .filter(|id| !self.conditional.contains_key(id))
            .map(|id| (self.symbols[id].scope, id))
            .collect();
        // Follow earlier imports through completed module initializers. Each
        // link matters because deleting it could remove the attachment.
        let mut pending = VecDeque::from([source]);
        let mut via = BTreeMap::new();
        while let Some(module) = pending.pop_front() {
            if module != source
                && let Some(&attachment) = attachments.get(&module)
            {
                let mut evidence = vec![attachment];
                let mut current = module;
                while let Some(&(caller, import)) = via.get(&current) {
                    evidence.push(import);
                    current = caller;
                }
                return Some(evidence);
            }
            for &(import, target) in &self.scopes[module].initializer_imports {
                if target != source
                    && (module != source
                        || position(self.symbols[import].name.at) < position(before))
                    && let std::collections::btree_map::Entry::Vacant(entry) = via.entry(target)
                {
                    entry.insert((module, import));
                    pending.push_back(target);
                }
            }
        }
        None
    }

    fn record_import(
        &mut self,
        scope: ScopeId,
        source_scope: ScopeId,
        module: Option<&str>,
        import: &Import,
    ) -> Option<LazyBranch<'a>> {
        let source = self.scopes[source_scope].file;
        let binding = self
            .by_location
            .get(&(source_scope, import.local.at.line, import.local.at.column))
            .copied();
        if let Some(id) = binding {
            // A selected lazy branch evaluates its imports in the caller's
            // execution, even though their bindings belong to the hook.
            self.scopes[scope].evaluated_imports.push(id);
        }
        if self.logging_import_uses_initialized_os(source_scope, import) {
            return None;
        }
        let Some(module) = module else {
            self.import_issue(
                scope,
                source,
                import,
                "relative import escapes its selected package".into(),
            );
            return None;
        };
        let own_module = &self.files[source].module;
        let module_file = self.modules.get(module).copied();
        let before = self.import_member_position(source_scope, module, import);
        let member = import.member.as_deref();
        let lookup = module_file.zip(member).map(|(file, member)| {
            self.selected_module_member(
                file,
                member,
                before.unwrap_or(END),
                (source_scope, import.local.at),
                ModuleMemberAccess::FromImport,
            )
        });
        let child = match &lookup {
            Some(ModuleMember::Submodule(child, _)) => Some(*child),
            None => member.and_then(|member| {
                self.modules
                    .get(format!("{module}.{member}").as_str())
                    .copied()
            }),
            _ => None,
        };
        if import.level > 0 && module_file.is_none() && child.is_none() {
            self.import_issue(
                scope,
                source,
                import,
                "relative import has no selected module".into(),
            );
            return None;
        }
        // Package self-imports can read the bindings already established at
        // this statement. Sibling submodule imports also need no final parent state.
        let local_initializer = binding.filter(|id| {
            self.phase03
                && scope != source
                && scope == source_scope
                && !self.conditional.contains_key(id)
        });
        if let Some(target) = module_file
            && before.is_none()
            && !(child.is_some() && within_package(own_module, module))
        {
            self.scopes[scope].imports.push(target);
            if let Some(id) = local_initializer {
                self.scopes[scope].initializer_imports.push((id, target));
            }
        }
        if let Some(child) = child {
            self.scopes[scope].imports.push(child);
            if let Some(id) = local_initializer {
                self.scopes[scope].initializer_imports.push((id, child));
            }
        }
        // Importing a dotted module can also execute its package initializers.
        let mut has_selected_parent = false;
        for (end, _) in module.match_indices('.') {
            let parent = &module[..end];
            if let Some(&target) = self.modules.get(parent) {
                has_selected_parent = true;
                if !within_package(own_module, parent) {
                    self.scopes[scope].imports.push(target);
                }
            }
        }
        if import.level == 0 && module_file.is_none() && has_selected_parent {
            self.import_issue(
                scope,
                source,
                import,
                format!("no source selected for submodule {module:?}"),
            );
        }
        if let (Some(member), Some(lookup)) = (member, lookup) {
            if let Some(write) =
                self.module_member_write(module, member, (source_scope, import.local.at))
            {
                if matches!(write, mutations::MemberWriteValue::Ambiguous) {
                    self.scope_issue(
                        scope, import.local.at, "unresolved_attribute_write",
                        "import may depend on a module member write with unresolved ordering or value",
                    );
                }
                return None;
            }
            if let ModuleMember::Binding(id, _) = &lookup {
                self.record_django_global_read(scope, *id, import.local.at);
            }
            match lookup {
                ModuleMember::Lazy(_, branch) => return Some(branch),
                ModuleMember::Missing => self.import_issue(
                    scope,
                    source,
                    import,
                    format!("{module:?} has no selected binding for {member:?}"),
                ),
                ModuleMember::Binding(id, _)
                    if self.binding_depends_on_branch(id, before.unwrap_or(END)) =>
                {
                    // A from-import reads the member even if the local alias
                    // is used only by an external collector such as pytest.
                    self.scope_issues.push((
                        scope,
                        Issue::new(
                            "ambiguous_binding",
                            &self.files[source].path,
                            format!("import of {module}.{member} depends on an unresolved branch"),
                        )
                        .at(import.local.at),
                    ));
                }
                ModuleMember::Binding(id, _) if matches!(self.symbols[id].binding, Binding::Deleted) => {
                    self.import_issue(
                        scope,
                        source,
                        import,
                        format!("{module:?} deletes the selected binding for {member:?}"),
                    );
                }
                ModuleMember::Ambiguous => self.scope_issues.push((
                    scope,
                    Issue::new("ambiguous_binding", &self.files[source].path,
                        format!("cannot establish whether submodule {module}.{member} was attached before this lookup"))
                        .at(import.local.at),
                )),
                ModuleMember::ConditionalHook => self.scope_issues.push((
                    scope,
                    Issue::new(
                        "ambiguous_binding",
                        &self.files[source].path,
                        format!("lazy import of {module}.{member} depends on a conditional __getattr__"),
                    )
                    .at(import.local.at),
                )),
                ModuleMember::OpaqueHook => self.scope_issues.push((
                    scope,
                    Issue::new(
                        "unresolved_lazy_export",
                        &self.files[source].path,
                        format!("lazy import of {module}.{member} is outside the bounded __getattr__ model"),
                    )
                    .at(import.local.at),
                )),
                _ => {}
            }
        }
        None
    }

    fn import_issue(&mut self, scope: ScopeId, file: usize, import: &Import, message: String) {
        self.scope_issues.push((
            scope,
            Issue::new("unresolved_import", &self.files[file].path, message).at(import.local.at),
        ));
    }

    fn execute_lazy_branch(&mut self, scope: ScopeId, branch: &LazyBranch<'a>) -> Value {
        // Keep imports discovered through return expressions on the same
        // worklist as nested from-imports. All execute for the original caller.
        let value = self.lazy_value(scope, branch, 0, Some(scope));
        let mut pending = vec![(branch.scope, branch.imports)];
        pending.append(&mut self.nested_lazy_imports);
        let mut visited = BTreeSet::new();
        while let Some((handler, imports)) = pending.pop() {
            let file = self.scopes[handler].file;
            for statement in imports {
                let StatementKind::Import(import) = &statement.kind else {
                    continue;
                };
                if !visited.insert((handler, position(import.local.at))) {
                    continue;
                }
                let module = self.import_module(file, import);
                if let Some(branch) = self.record_import(scope, handler, module.as_deref(), import)
                {
                    self.lazy_value(scope, &branch, 0, Some(scope));
                    pending.push((branch.scope, branch.imports));
                    pending.append(&mut self.nested_lazy_imports);
                }
            }
        }
        value
    }

    fn synchronous_calls(
        &self,
        scope: ScopeId,
        include_verification: bool,
    ) -> impl Iterator<Item = ScopeId> {
        self.scopes[scope].edges.iter().filter_map(move |edge| {
            if (matches!(
                edge.kind,
                "direct_call"
                    | field_dispatch::DIRECT
                    | "constructor"
                    | "decorator"
                    | "subclass_hook"
                    | "pydantic_schema_build"
                    | "truth_test"
                    | "length_call"
                    | "formatting_hook"
            ) || include_verification && edge.kind == "grep_dynamic_call")
                && let Binding::Definition(_, body) = self.symbols[edge.target].binding
            {
                Some(body)
            } else {
                None
            }
        })
    }

    fn initialization_callbacks(&mut self) -> Vec<Vec<ScopeId>> {
        let mut invocations = BTreeMap::<_, Vec<_>>::new();
        for call in &self.parameter_calls {
            let parameter = &self.symbols[call.parameter];
            let owner = self.scopes[parameter.scope]
                .owner
                .expect("parameters belong to a definition");
            if self.direct_arguments.contains_key(&owner) {
                invocations
                    .entry((owner, parameter.scope))
                    .or_default()
                    .push(call);
            }
        }
        let mut callbacks = vec![Vec::new(); self.scopes.len()];
        let mut constructors = Vec::new();
        for ((callee, body), invocations) in invocations {
            // A captured parameter call matters only in a nested body that
            // can execute during this invocation of the enclosing function.
            let mut reached = BTreeSet::new();
            if invocations.iter().any(|call| call.scope != body) {
                let mut pending = vec![body];
                while let Some(scope) = pending.pop() {
                    if reached.insert(scope) {
                        pending.extend(&self.scopes[scope].class_bodies);
                        pending.extend(self.synchronous_calls(scope, true));
                    }
                }
            }
            let parameters: BTreeSet<_> = invocations
                .into_iter()
                .filter(|call| call.scope == body || reached.contains(&call.scope))
                .map(|call| self.symbols[call.parameter].name.text.as_str())
                .collect();
            for call in &self.direct_arguments[&callee] {
                for parameter in &parameters {
                    if let Some(&Value::Callable(invocation)) = call.values.get(*parameter)
                        && !self.receiver_conflicts(invocation)
                        && let Binding::Definition(definition, body) =
                            self.symbols[invocation.target].binding
                    {
                        match definition.kind {
                            DefinitionKind::Function => callbacks[call.caller].push(body),
                            DefinitionKind::Class => {
                                constructors.push((call.caller, invocation.target, call.at));
                            }
                            DefinitionKind::Coroutine => {}
                        }
                    }
                }
            }
        }
        for (caller, class, at) in constructors {
            self.constructor(caller, class, "constructor", "possible", at);
        }
        callbacks
    }

    fn acyclic_initialization_edges(
        &self,
        callbacks: &[Vec<ScopeId>],
    ) -> Option<Vec<Vec<ScopeId>>> {
        let count = self.scopes.len();
        let mut outgoing = vec![Vec::new(); count];
        let mut incoming = vec![Vec::new(); count];
        let mut reached = vec![false; count];
        let mut finished = Vec::with_capacity(count);
        let mut pending: Vec<_> = (0..self.files.len()).map(|file| (file, false)).collect();
        while let Some((id, exiting)) = pending.pop() {
            if exiting {
                finished.push(id);
                continue;
            }
            if std::mem::replace(&mut reached[id], true) {
                continue;
            }
            pending.push((id, true));
            let scope = &self.scopes[id];
            for target in scope
                .imports
                .iter()
                .chain(&scope.class_bodies)
                .chain(&callbacks[id])
                .copied()
                .chain(self.synchronous_calls(id, true))
            {
                incoming[target].push(id);
                outgoing[id].push(target);
                if !reached[target] {
                    pending.push((target, false));
                }
            }
        }
        // Reverse finishing order gives the strongly connected components.
        // A recursive call alone is valid. A component with an import edge
        // can consult a module before its final bindings are established.
        let mut component = vec![usize::MAX; count];
        let mut pending = Vec::new();
        for &root in finished.iter().rev() {
            pending.push(root);
            while let Some(id) = pending.pop() {
                if component[id] == usize::MAX {
                    component[id] = root;
                    pending.extend(&incoming[id]);
                }
            }
        }
        if finished.iter().any(|&id| {
            self.scopes[id]
                .imports
                .iter()
                .any(|&target| component[id] == component[target])
        }) {
            return None;
        }
        Some(outgoing)
    }

    fn initialization_imports(
        &self,
        file: usize,
        initialization_edges: &[Vec<ScopeId>],
    ) -> BTreeSet<SymbolId> {
        let mut imports = BTreeSet::new();
        let mut reached = BTreeSet::new();
        let mut pending = vec![file];
        while let Some(scope) = pending.pop() {
            if reached.insert(scope) {
                imports.extend(&self.scopes[scope].evaluated_imports);
                pending.extend(&initialization_edges[scope]);
            }
        }
        imports
    }

    fn binding_depends_on_branch(&self, id: SymbolId, at: Location) -> bool {
        self.conditional
            .get(&id)
            .is_some_and(|guards| guards.iter().any(|span| !span.contains(&position(at))))
    }

    fn binding(&mut self, id: SymbolId, before: Location, depth: usize) -> Value {
        let scope = self.symbols[id].scope;
        let at = self.symbols[id].name.at;
        if depth >= MAX_BINDING_CHAIN {
            self.issue(
                self.scopes[scope].file,
                at,
                "analysis_limit",
                "binding resolution limit exceeded",
            );
            return Value::Unknown;
        }
        self.read.insert(id);
        if self.binding_depends_on_branch(id, before) {
            if let Some(value) = self.finite_conditional_value(id, before, depth + 1) {
                return value;
            }
            self.scope_issue(
                scope,
                at,
                "ambiguous_binding",
                "a required binding depends on an unresolved branch",
            );
            return Value::Unknown;
        }
        self.binding_value(id, depth)
    }

    fn finite_conditional_value(
        &mut self,
        id: SymbolId,
        before: Location,
        depth: usize,
    ) -> Option<Value> {
        let symbol = &self.symbols[id];
        let candidates = self.scopes[symbol.scope]
            .names
            .get(symbol.name.text.as_str())?;
        let mut eligible = Vec::new();
        for &candidate in candidates {
            if position(self.symbols[candidate].name.at) >= position(before) {
                break;
            }
            eligible.push(candidate);
        }
        if !self.conditional_groups.contains_key(&id) {
            return None;
        }
        if let Some(fallback) = eligible
            .iter()
            .rposition(|candidate| !self.conditional_groups.contains_key(candidate))
        {
            eligible.drain(..fallback);
        }
        let groups = eligible
            .iter()
            .filter_map(|candidate| self.conditional_groups.get(candidate))
            .flatten()
            .map(|(group, _)| *group)
            .collect::<BTreeSet<_>>();
        let group_bits = groups
            .into_iter()
            .enumerate()
            .map(|(bit, group)| (group, bit))
            .collect::<BTreeMap<_, _>>();
        let states = 1usize.checked_shl(group_bits.len() as u32)?;
        if states > MAX_CONDITIONAL_STATES {
            return None;
        }
        let mut selected = BTreeSet::new();
        for state in 0..states {
            let candidate = eligible.iter().rev().copied().find(|candidate| {
                self.conditional_groups
                    .get(candidate)
                    .is_none_or(|conditions| {
                        conditions.iter().all(|(group, branch)| {
                            let bit = group_bits[group];
                            ((state >> bit) & 1 == 1) == *branch
                        })
                    })
            })?;
            selected.insert(candidate);
        }
        let mut values = Vec::new();
        for candidate in selected {
            self.read.insert(candidate);
            let value = self.binding_value(candidate, depth + 1);
            if value.contains_unknown() {
                return None;
            }
            values.push(value);
        }
        Some(Value::alternatives(values))
    }

    fn binding_value(&mut self, id: SymbolId, depth: usize) -> Value {
        if self.rejected_django_callbacks.contains(&id) {
            return Value::Unknown;
        }
        let symbol = &self.symbols[id];
        let scope = symbol.scope;
        let file = self.scopes[scope].file;
        if scope == file
            && self
                .failed_flask_import_initializers
                .get(&file)
                .is_some_and(|&failure| position(failure) <= position(symbol.name.at))
        {
            // This binding cannot be installed on the initializer path.
            return Value::Unknown;
        }
        if let Some(&value) = self.evaluated_module_booleans.get(&id) {
            return Value::Bool(value);
        }
        match symbol.binding {
            Binding::Definition(definition, _) => {
                if let Some(creation) = self.metaclass_creation(id) {
                    match creation {
                        metaclasses::ClassCreation::Redirect { replacement, .. } => {
                            return Value::Callable(Invocation::ordinary(replacement));
                        }
                        metaclasses::ClassCreation::Unknown => return Value::Unknown,
                        metaclasses::ClassCreation::Rewrite { .. } => {}
                    }
                }
                if self.phase03
                    && let [decorator] = definition.decorators.as_slice()
                    && (self.bounded_decorator_replaces_function(
                        scope,
                        decorator,
                        id,
                        definition.name.at,
                        depth + 1,
                    ) || self.gin_decorator_replaces_function(
                        scope,
                        decorator,
                        id,
                        definition.name.at,
                        depth + 1,
                    ) || self.click_command_replaces_function(
                        scope,
                        decorator,
                        id,
                        definition.name.at,
                        depth + 1,
                    ) || self.traitlets_replaces_function(
                        scope,
                        decorator,
                        id,
                        definition.name.at,
                        depth + 1,
                    ))
                {
                    return Value::Unknown;
                }
                if self.phase03
                    && let [decorator] = definition.decorators.as_slice()
                    && !matches!(decorator.kind, ExpressionKind::Call { .. })
                    && let Value::Callable(decorator_fn) =
                        self.identity(scope, decorator, definition.name.at, depth + 1)
                    && let Some(replacement) = self.known_definition_replacement(id, decorator_fn)
                {
                    return replacement;
                }
                if self.phase03
                    && let Some(replacement) = self.fastapi_decorated_value(id, depth)
                {
                    return replacement;
                }
                Value::Callable(Invocation::ordinary(id))
            }
            Binding::Variable(Some(value), before) => {
                self.identity(scope, value, before, depth + 1)
            }
            Binding::UnpackItem {
                value,
                arity,
                index,
                before,
            } => self
                .literal_split_items(scope, value, before, depth + 1)
                .filter(|items| items.len() == arity)
                .and_then(|items| items.into_iter().nth(index))
                .unwrap_or(Value::Unknown),
            Binding::Variable(None, _) => Value::Unknown,
            Binding::Deleted => Value::DeletedModuleBinding,
            Binding::Parameter => {
                if let Some(&class) = self.instance_receivers.get(&id) {
                    return if self.subclassed.contains(&class) {
                        Value::ReceiverParameter(id)
                    } else {
                        Value::Instance(class)
                    };
                }
                Value::Unknown
            }
            Binding::Import(import) => {
                if self.logging_import_uses_initialized_os(scope, import) {
                    return Value::InitializedStdlibOs;
                }
                let Some(module) = self.import_module(file, import) else {
                    return Value::Unknown;
                };
                if let Some(member) = import.member.as_deref() {
                    let before = self
                        .import_member_position(scope, &module, import)
                        .unwrap_or(END);
                    self.module_member_at(
                        &module,
                        member,
                        before,
                        (scope, import.local.at),
                        ModuleMemberAccess::FromImport,
                        depth + 1,
                    )
                } else {
                    let visible_module = self.visible_import_module(file, import, &module);
                    if let Some(replacement) =
                        self.imported_module_write(&visible_module, (scope, import.local.at))
                    {
                        return replacement;
                    }
                    self.modules.get(visible_module.as_ref()).map_or_else(
                        || Value::Provider(visible_module.into_owned()),
                        |file| Value::Module(*file),
                    )
                }
            }
        }
    }

    fn selected_module_member(
        &self,
        file: usize,
        member: &str,
        before: Location,
        access: (ScopeId, Location),
        mode: ModuleMemberAccess<'_>,
    ) -> ModuleMember<'a> {
        let submodule = format!("{}.{member}", self.files[file].module);
        let child = self.modules.get(submodule.as_str()).copied();
        if let Some(binding) = self.local(file, member, before) {
            if let Some(child) = child {
                return match self.submodule_attachment(file, child, Some(binding), access, mode) {
                    SubmoduleAttachment::Attached(import) => {
                        ModuleMember::Submodule(child, vec![import])
                    }
                    SubmoduleAttachment::Rebound(import) => {
                        ModuleMember::Binding(binding, Some(import))
                    }
                    SubmoduleAttachment::Ambiguous => ModuleMember::Ambiguous,
                    SubmoduleAttachment::Absent => ModuleMember::Binding(binding, None),
                };
            }
            return ModuleMember::Binding(binding, None);
        }
        if has_implicit_module_member(member, self.files[file].is_package) {
            return ModuleMember::Unknown;
        }
        let hook = self.local(file, "__getattr__", before);
        let lazy = hook.map_or(LazyMember::Missing, |hook| {
            self.lazy_lookup(hook, member, before)
        });
        if let Some(child) = child {
            match self.submodule_attachment(file, child, None, access, mode) {
                SubmoduleAttachment::Attached(import) => {
                    return ModuleMember::Submodule(child, vec![import]);
                }
                _ if matches!(
                    mode,
                    ModuleMemberAccess::FromImport | ModuleMemberAccess::Export { .. }
                ) && matches!(lazy, LazyMember::Missing) =>
                {
                    return ModuleMember::Submodule(child, Vec::new());
                }
                SubmoduleAttachment::Absent if matches!(lazy, LazyMember::Missing) => {
                    return ModuleMember::UnattachedSubmodule;
                }
                SubmoduleAttachment::Absent => {}
                SubmoduleAttachment::Ambiguous => {
                    return self
                        .imported_submodule(child, access)
                        .map_or(ModuleMember::Ambiguous, |imports| {
                            ModuleMember::Submodule(child, imports)
                        });
                }
                SubmoduleAttachment::Rebound(_) => return ModuleMember::Ambiguous,
            }
        }
        match (hook, lazy) {
            (Some(hook), LazyMember::Branch(branch)) => ModuleMember::Lazy(hook, branch),
            (_, LazyMember::Missing) => ModuleMember::Missing,
            (_, LazyMember::Conditional) => ModuleMember::ConditionalHook,
            (_, LazyMember::Unknown) => ModuleMember::OpaqueHook,
            _ => ModuleMember::Unknown,
        }
    }

    fn module_member_at(
        &mut self,
        module: &str,
        member: &str,
        before: Location,
        access: (ScopeId, Location),
        mode: ModuleMemberAccess<'_>,
        depth: usize,
    ) -> Value {
        let submodule = format!("{module}.{member}");
        if depth >= MAX_BINDING_CHAIN {
            return Value::Unknown;
        }
        let (scope, at) = match mode {
            ModuleMemberAccess::Export { at, .. } => (access.0, at),
            _ => access,
        };
        if let Some(&file) = self.modules.get(module)
            && let Some(write) = self.module_global_write(file, member, before, access)
        {
            return self.module_global_value(scope, at, write);
        }
        if matches!(mode, ModuleMemberAccess::FromImport)
            && let Some(replacement) = self.imported_module_write(module, access)
        {
            return replacement;
        }
        if let Some(write) = self.module_member_write(module, member, access) {
            return match write {
                mutations::MemberWriteValue::Replacement(value) => value,
                mutations::MemberWriteValue::Deleted => Value::MissingFrameworkMember,
                mutations::MemberWriteValue::Ambiguous => {
                    self.scope_issue(
                        scope,
                        at,
                        "unresolved_attribute_write",
                        "module lookup may depend on a write with unresolved ordering or value",
                    );
                    Value::Unknown
                }
            };
        }
        if let Some(&file) = self.modules.get(module) {
            return match self.selected_module_member(file, member, before, access, mode) {
                ModuleMember::Binding(id, import) => {
                    self.read.extend(import);
                    self.record_django_global_read(scope, id, at);
                    if matches!(self.symbols[id].binding, Binding::Deleted)
                        && (self.local(file, "__getattr__", before).is_some()
                            || self.modules.contains_key(submodule.as_str()))
                    {
                        self.scope_issue(scope, at, "unresolved_attribute", "module lookup after deletion can invoke a dynamic export or attach a submodule");
                        return Value::Unknown;
                    }
                    if matches!(self.symbols[id].binding, Binding::Deleted)
                        && matches!(mode, ModuleMemberAccess::Attribute)
                    {
                        self.scope_issue(
                            scope,
                            at,
                            "unresolved_attribute",
                            format!("module {module:?} deleted {member:?}"),
                        );
                    }
                    self.binding(id, before, depth + 1)
                }
                ModuleMember::Submodule(file, import) => {
                    self.read.extend(import);
                    Value::Module(file)
                }
                ModuleMember::Lazy(id, branch) => {
                    self.read.insert(id);
                    self.lazy_value(scope, &branch, depth + 2, None)
                }
                ModuleMember::Ambiguous => {
                    self.scope_issue(scope, at, "ambiguous_binding",
                        format!("cannot establish whether submodule {submodule} was attached before this lookup"));
                    Value::Unknown
                }
                ModuleMember::ConditionalHook => {
                    self.scope_issue(
                        scope,
                        at,
                        "ambiguous_binding",
                        format!("lazy lookup of {submodule} depends on a conditional __getattr__"),
                    );
                    Value::Unknown
                }
                ModuleMember::OpaqueHook => {
                    self.scope_issue(
                        scope,
                        at,
                        "unresolved_lazy_export",
                        format!(
                            "lazy lookup of {submodule} is outside the bounded __getattr__ model"
                        ),
                    );
                    Value::Unknown
                }
                ModuleMember::UnattachedSubmodule => {
                    self.scope_issue(
                        scope,
                        at,
                        "unresolved_attribute",
                        format!("submodule {submodule} is not attached to package {module:?}"),
                    );
                    Value::Unknown
                }
                ModuleMember::Missing if matches!(mode, ModuleMemberAccess::Attribute) => {
                    self.scope_issue(
                        scope,
                        at,
                        "unresolved_attribute",
                        format!("module {module:?} has no selected binding for {member:?}"),
                    );
                    Value::Unknown
                }
                ModuleMember::Missing | ModuleMember::Unknown => Value::Unknown,
            };
        }
        self.modules
            .get(submodule.as_str())
            .map_or(Value::Provider(submodule), |file| Value::Module(*file))
    }

    fn lazy_value(
        &mut self,
        caller: ScopeId,
        branch: &LazyBranch<'a>,
        depth: usize,
        execution_scope: Option<ScopeId>,
    ) -> Value {
        let first_issue = self.scope_issues.len();
        let evaluation = ValueEvaluation::Lazy {
            execution_scope,
            argument: branch.argument,
        };
        let value = self.expression_value(branch.scope, branch.value, branch.at, depth, evaluation);
        if value.contains_unknown() && self.scope_issues.len() == first_issue {
            self.scope_issue(
                branch.scope,
                branch.value.at,
                "unresolved_lazy_export",
                "lazy return value or its evaluation is outside the bounded model",
            );
        }
        // A member access executes only the selected lazy branch. Keep the
        // original diagnostic locations and use the caller for reachability.
        for (owner, _) in &mut self.scope_issues[first_issue..] {
            *owner = caller;
        }
        value
    }

    fn lazy_call(
        &mut self,
        scope: ScopeId,
        expression: &Expression,
        before: Location,
        depth: usize,
        evaluation: ValueEvaluation<'a>,
    ) -> Value {
        let ExpressionKind::Call {
            function,
            arguments,
            keywords,
        } = &expression.kind
        else {
            unreachable!()
        };
        let ValueEvaluation::Lazy {
            execution_scope, ..
        } = evaluation
        else {
            unreachable!()
        };
        let recurse = |this: &mut Self, expression| {
            this.expression_value(scope, expression, before, depth + 1, evaluation)
        };
        let plain_hook = self.scopes[scope].owner.is_some_and(|id| {
            matches!(self.symbols[id].binding, Binding::Definition(definition, _) if definition.decorators.is_empty())
        });
        if plain_hook && keywords.is_empty() {
            match (recurse(self, function), arguments.as_slice()) {
                (Value::Provider(provider), [base, name]) if provider == "builtins.getattr" => {
                    if let (base @ Value::Module(_), Value::Text(name)) =
                        (recurse(self, base), recurse(self, name))
                    {
                        return self.lazy_attribute(
                            base,
                            &name,
                            (scope, before),
                            depth + 1,
                            execution_scope,
                        );
                    }
                }
                (Value::Provider(provider), [name]) if provider == "importlib.import_module" => {
                    if let Value::Text(name) = recurse(self, name)
                        && let Some(&target) = self.modules.get(name.as_str())
                        && !within_package(&self.files[self.scopes[scope].file].module, &name)
                    {
                        // An omitted initializer can change the imported module or
                        // prevent it from loading. This path requires selected packages.
                        let parents: Option<Vec<_>> = name
                            .match_indices('.')
                            .map(|(end, _)| self.lazy_import_parent(&name, end))
                            .collect();
                        if let Some(parents) = parents {
                            if let Some(caller) = execution_scope {
                                let caller_module = &self.files[self.scopes[caller].file].module;
                                self.scopes[caller]
                                    .imports
                                    .extend(parents.into_iter().filter(|&file| {
                                        !within_package(caller_module, &self.files[file].module)
                                    }));
                                self.scopes[caller].imports.push(target);
                            }
                            return Value::Module(target);
                        }
                    }
                }
                _ => {}
            }
        }
        self.scope_issue(
            scope,
            expression.at,
            "unresolved_lazy_export",
            "effects of the lazy return expression are outside the bounded model",
        );
        Value::Unknown
    }

    fn lazy_import_parent(&self, name: &str, end: usize) -> Option<usize> {
        let &parent = self.modules.get(&name[..end])?;
        let child = name[end + 1..].split('.').next()?;
        // Loading a child can replace an existing attribute on its package.
        // The static import attachment index cannot order this lazy effect.
        let replaces_search_path =
            self.scopes[parent]
                .names
                .get("__path__")
                .is_some_and(|bindings| {
                    bindings
                        .iter()
                        .any(|&id| !matches!(self.symbols[id].binding, Binding::Variable(None, _)))
                });
        if !self.files[parent].is_package
            || replaces_search_path
            || has_implicit_module_member(child, true)
            || self.local(parent, child, END).is_some()
            || self.local(parent, "__getattr__", END).is_some_and(|hook| {
                !matches!(self.lazy_lookup(hook, child, END), LazyMember::Missing)
            })
        {
            return None;
        }
        Some(parent)
    }

    fn lazy_attribute(
        &mut self,
        base: Value,
        name: &str,
        access: (ScopeId, Location),
        depth: usize,
        execution_scope: Option<ScopeId>,
    ) -> Value {
        if let Some(caller) = execution_scope
            && let Value::Module(file) = base
            && let ModuleMember::Lazy(hook, branch) =
                self.selected_module_member(file, name, END, access, ModuleMemberAccess::Attribute)
        {
            self.read.insert(hook);
            self.nested_lazy_imports
                .push((branch.scope, branch.imports));
            return self.lazy_value(access.0, &branch, depth + 2, Some(caller));
        }
        let value = self.member(base, name, access, depth);
        if let Some(caller) = execution_scope {
            self.record_class_reference(caller, &value);
        }
        value
    }

    fn lazy_lookup(&self, id: SymbolId, member: &str, before: Location) -> LazyMember<'a> {
        self.lazy_dispatch(id, before, |name, scope, body, argument| {
            (name == member).then(|| Self::lazy_branch(scope, body, argument))
        })
    }

    fn lazy_dispatch(
        &self,
        id: SymbolId,
        before: Location,
        mut visit: impl FnMut(
            &'a str,
            ScopeId,
            &'a [Statement],
            Option<(SymbolId, &'a str)>,
        ) -> Option<LazyMember<'a>>,
    ) -> LazyMember<'a> {
        if self.binding_depends_on_branch(id, before) {
            return LazyMember::Conditional;
        }
        let Binding::Definition(definition, scope) = self.symbols[id].binding else {
            return LazyMember::Unknown;
        };
        let [parameter] = definition.parameters.as_slice() else {
            return LazyMember::Unknown;
        };
        // Python supplies the lookup name as one positional string argument.
        if definition.kind != DefinitionKind::Function
            || !(definition.has_positional_parameter
                || definition.has_variadic_positional_parameter)
        {
            return LazyMember::Unknown;
        }
        let parameter_id = self
            .by_location
            .get(&(scope, parameter.at.line, parameter.at.column))
            .copied();
        // Prove literal dispatch only, after ordinary module lookup failed.
        for statement in &definition.body {
            if let StatementKind::Raise(Some(exception)) = &statement.kind {
                let target = match &exception.kind {
                    ExpressionKind::Call {
                        function,
                        arguments,
                        keywords,
                    } if definition.has_positional_parameter
                        || (keywords.is_empty()
                            && arguments.iter().all(|arg| match &arg.kind {
                                ExpressionKind::Text(_)
                                | ExpressionKind::Bool(_)
                                | ExpressionKind::None
                                | ExpressionKind::Scalar(_) => true,
                                ExpressionKind::Name(name) => name == &parameter.text,
                                _ => false,
                            })) =>
                    {
                        function
                    }
                    _ => exception,
                };
                if matches!(&target.kind, ExpressionKind::Name(name) if name == "AttributeError")
                    && self
                        .lexical(scope, "AttributeError", statement.at)
                        .is_none()
                    && !self.scopes[scope].names.contains_key("AttributeError")
                {
                    return LazyMember::Missing;
                }
            }
            // A variadic hook receives a tuple, so only a proven missing
            // member can be resolved without matching a string guard.
            if !definition.has_positional_parameter {
                break;
            }
            let StatementKind::If {
                test,
                body,
                otherwise,
            } = &statement.kind
            else {
                break;
            };
            if !otherwise.is_empty() {
                break;
            }
            if let ExpressionKind::Membership {
                element,
                container,
                contains: true,
            } = &test.kind
                && matches!(&element.kind, ExpressionKind::Name(name) if name == &parameter.text)
                && let ExpressionKind::Name(mapping) = &container.kind
                && let Some(table) = self.lexical(scope, mapping, statement.at)
                && !self.binding_depends_on_branch(table, statement.at)
                && let Binding::Variable(
                    Some(Expression {
                        kind: ExpressionKind::Dictionary(items),
                        ..
                    }),
                    _,
                ) = self.symbols[table].binding
            {
                for pair in items.as_chunks::<2>().0 {
                    let ExpressionKind::Text(text) = &pair[0].kind else {
                        return LazyMember::Unknown;
                    };
                    if let Some(result) = visit(
                        text,
                        scope,
                        body,
                        parameter_id.map(|id| (id, text.as_str())),
                    ) {
                        return result;
                    }
                }
                continue;
            }
            let ExpressionKind::Compare {
                left,
                right,
                equal: true,
                identity: false,
            } = &test.kind
            else {
                break;
            };
            let (ExpressionKind::Name(name), ExpressionKind::Text(text)) =
                (&left.kind, &right.kind)
            else {
                break;
            };
            if name != &parameter.text {
                break;
            }
            if let Some(result) = visit(
                text,
                scope,
                body,
                parameter_id.map(|id| (id, text.as_str())),
            ) {
                return result;
            }
        }
        LazyMember::Unknown
    }

    fn lazy_branch(
        scope: ScopeId,
        body: &'a [Statement],
        argument: Option<(SymbolId, &'a str)>,
    ) -> LazyMember<'a> {
        if let Some((
            Statement {
                kind: StatementKind::Return(Some(value)),
                at,
            },
            imports,
        )) = body.split_last()
            && imports
                .iter()
                .all(|stmt| matches!(stmt.kind, StatementKind::Import(_)))
        {
            return LazyMember::Branch(LazyBranch {
                scope,
                imports,
                value,
                at: *at,
                argument,
            });
        }
        LazyMember::Unknown
    }

    fn member(
        &mut self,
        base: Value,
        name: &str,
        access: (ScopeId, Location),
        depth: usize,
    ) -> Value {
        match base {
            Value::Alternatives(values) => Value::alternatives(
                values
                    .into_iter()
                    .map(|value| self.member(value, name, access, depth + 1))
                    .collect(),
            ),
            Value::ReceiverParameter(parameter) => {
                Value::ReceiverMethod(parameter, name.to_owned())
            }
            Value::CommandNamespace(file) if name == "get" => Value::CommandNamespaceGet(file),
            Value::FrameworkCode(target) if name == "co_name" => {
                Value::Text(self.symbols[target].name.text.clone())
            }
            Value::Callable(invocation)
                if name == "__code__"
                    && !invocation.verified
                    && self.has_command_namespace(self.scopes[access.0].file)
                    && matches!(self.symbols[invocation.target].binding,
                        Binding::Definition(definition, _)
                            if definition.kind == DefinitionKind::Function
                                && !definition.is_generator && definition.decorators.is_empty()) =>
            {
                match self.django_command_code_write(invocation.target, access) {
                    Some(mutations::MemberWriteValue::Replacement(code)) => code,
                    Some(_) => {
                        self.scope_issue(
                            access.0,
                            access.1,
                            "unresolved_attribute_write",
                            "Django command code object may depend on an unresolved write",
                        );
                        Value::Unknown
                    }
                    None => Value::FrameworkCode(invocation.target),
                }
            }
            Value::Module(file) => self.module_member_at(
                &self.files[file].module,
                name,
                END,
                access,
                ModuleMemberAccess::Attribute,
                depth,
            ),
            Value::InitializedStdlibOs if name == "PathLike" => {
                Value::Provider("os.PathLike".into())
            }
            Value::Provider(module) => self.module_member_at(
                &module,
                name,
                END,
                access,
                ModuleMemberAccess::Attribute,
                depth,
            ),
            Value::ProjectPath(path) => self.project_path_member(path, name, access),
            Value::Instance(class) | Value::Callable(Invocation { target: class, .. })
                if self.is_class(class) =>
            {
                if matches!(base, Value::Instance(_))
                    && self.unresolved_django_command_lookup(class, access)
                {
                    return Value::Unknown;
                }
                let member = self
                    .class_member_at(class, name, depth, Some(access))
                    .unwrap_or(Value::Unknown);
                if matches!(&member, Value::Provider(provider) if provider == "django.views.View.as_view")
                    && matches!(base, Value::Callable(_))
                {
                    return Value::DjangoViewFactory(class);
                }
                if matches!(member, Value::DjangoFormValidation(None)) {
                    return match base {
                        Value::Instance(_) => Value::DjangoFormValidation(Some(class)),
                        _ => Value::Unknown,
                    };
                }
                if matches!(member, Value::DjangoSignalConnect)
                    && matches!(base, Value::Callable(_))
                {
                    return Value::Unknown;
                }
                if matches!(&member, Value::Provider(provider) if matches!(provider.as_str(),
                    "pydantic.BaseModel.model_dump" | "pydantic.BaseModel.model_dump_json"))
                {
                    return match base {
                        Value::Instance(_) => Value::PydanticDump(class),
                        _ => Value::Unknown,
                    };
                }
                if let Value::Callable(invocation) = &member
                    && self.unittest_property(invocation.target)
                {
                    if matches!(base, Value::Instance(_)) {
                        self.scope_issue(
                            access.0,
                            access.1,
                            "unresolved_descriptor",
                            "explicit property access is outside the unittest model",
                        );
                    }
                    return Value::Unknown;
                }
                if let Value::Callable(invocation) = member
                    && self.receiver_parameter(invocation.target).is_some()
                {
                    let receiver = match base {
                        Value::Callable(_)
                            if self.phase03
                                && self.fastapi_class_factory(class, invocation.target, depth) =>
                        {
                            Receiver::Bound(class)
                        }
                        Value::Callable(_) => Receiver::Explicit(None),
                        _ => Receiver::Bound(class),
                    };
                    Value::Callable(invocation.bind_receiver(receiver))
                } else if let Value::Callable(invocation) = member
                    && matches!(base, Value::Instance(_))
                    && !self.is_class(invocation.target)
                {
                    // A function assigned to the class namespace still binds
                    // on instance access when declared outside the class.
                    Value::Callable(Invocation {
                        implicit_descriptor_receiver: true,
                        ..invocation
                    })
                } else {
                    member
                }
            }
            Value::FastapiSubclassApp(object, class) => {
                self.fastapi_subclass_member(object, class, name, access, depth)
            }
            Value::DjangoSignal(site) if name == "connect" => {
                match self.signal_member_write(site, name, access) {
                    Some(mutations::MemberWriteValue::Replacement(value)) => value,
                    Some(mutations::MemberWriteValue::Deleted) | None => Value::DjangoSignalConnect,
                    Some(mutations::MemberWriteValue::Ambiguous) => Value::Unknown,
                }
            }
            Value::Object(object, ObjectKind::TyperApp) if name == "callback" => {
                self.typer_callback_member(object, access)
            }
            Value::Object(
                object,
                kind @ (ObjectKind::FlaskApp
                | ObjectKind::FlaskBlueprint
                | ObjectKind::FlaskLoginManager),
            ) if self.phase03 => self.flask_member(object, kind, name, access),
            Value::Object(object, ObjectKind::CeleryApp) if name == "conf" => {
                self.celery_config_member(object, access)
            }
            Value::Object(object, ObjectKind::CeleryApp) if name == "backend" => {
                Value::CeleryBackend(object)
            }
            Value::CeleryConfig(object) if name == "update" => {
                Value::Method(object, ObjectKind::CeleryApp, "conf.update".into())
            }
            Value::Object(
                site,
                kind @ (ObjectKind::App
                | ObjectKind::Router
                | ObjectKind::StarletteApp
                | ObjectKind::AiohttpRouteTable),
            ) if self.phase03 => match self.fastapi_member_write(site, name, access) {
                Some(mutations::MemberWriteValue::Replacement(value)) => value,
                Some(mutations::MemberWriteValue::Deleted) | None
                    if matches!(kind, ObjectKind::App)
                        && self.fastapi_base_member_changed(name, access) =>
                {
                    Value::Unknown
                }
                Some(mutations::MemberWriteValue::Deleted) | None => {
                    Value::Method(site, kind, name.into())
                }
                Some(mutations::MemberWriteValue::Ambiguous) => {
                    self.scope_issue(
                        access.0,
                        access.1,
                        "unresolved_attribute_write",
                        "ASGI registration method depends on an unresolved write",
                    );
                    Value::Unknown
                }
            },
            Value::Object(object, kind) => Value::Method(object, kind, name.into()),
            Value::Executor if name == "submit" => Value::Submit,
            Value::Process(target) if name == "start" => Value::Start(target),
            _ => Value::Unknown,
        }
    }

    fn record_class_reference(&mut self, scope: ScopeId, base: &Value) {
        match base {
            Value::Callable(invocation) if self.is_class(invocation.target) => {
                self.edge(scope, invocation.target, "class_reference", "certain");
            }
            Value::Alternatives(values) => {
                for value in values {
                    self.record_class_reference(scope, value);
                }
            }
            _ => {}
        }
    }

    fn comparison_may_call_method(&mut self, left: &Value, right: &Value, equal: bool) -> bool {
        if let Value::Alternatives(values) = left {
            return values
                .iter()
                .any(|value| self.comparison_may_call_method(value, right, equal));
        }
        if let Value::Alternatives(values) = right {
            return values
                .iter()
                .any(|value| self.comparison_may_call_method(left, value, equal));
        }
        // Sequence equality tests corresponding elements, and unequal
        // lengths short-circuit even when the outer operation is !=.
        if let (Value::Sequence(left), Value::Sequence(right)) = (left, right) {
            return left.len() == right.len()
                && left
                    .iter()
                    .zip(right)
                    .any(|(left, right)| self.comparison_may_call_method(left, right, true));
        }

        [(left, right), (right, left)]
            .into_iter()
            .any(|(value, other)| match value {
                Value::Instance(class) => {
                    self.class_member(*class, "__eq__", 0).is_some()
                        || !equal && self.class_member(*class, "__ne__", 0).is_some()
                }
                Value::LocalNamespace(scope, first)
                    if matches!(
                        other,
                        Value::Mapping(_)
                            | Value::LocalNamespace(..)
                            | Value::Unknown
                            | Value::OpaqueNamespace
                    ) =>
                {
                    self.has_equality_hooks
                        && (!self.namespace_has_only_scalars(*scope, *first)
                            || match other {
                                Value::Mapping(items) => {
                                    !items.iter().all(Value::is_builtin_scalar)
                                }
                                Value::LocalNamespace(scope, first) => {
                                    !self.namespace_has_only_scalars(*scope, *first)
                                }
                                _ => true,
                            })
                }
                Value::NamespaceCopy(_)
                    if matches!(
                        other,
                        Value::Mapping(_)
                            | Value::LocalNamespace(..)
                            | Value::NamespaceCopy(_)
                            | Value::BuiltinIterable
                            | Value::Unknown
                            | Value::OpaqueNamespace
                    ) =>
                {
                    self.has_equality_hooks
                }
                Value::OpaqueNamespace => self.has_equality_hooks,
                // A mapping can compare keys and values independent of the
                // order in which the literals list their entries.
                Value::Mapping(items)
                    if matches!(
                        other,
                        Value::Mapping(_) | Value::BuiltinIterable | Value::Unknown
                    ) =>
                {
                    items
                        .iter()
                        .any(|item| self.comparison_may_call_method(item, &Value::Unknown, true))
                }
                Value::Sequence(items)
                    if matches!(other, Value::BuiltinIterable | Value::Unknown) =>
                {
                    items
                        .iter()
                        .any(|item| self.comparison_may_call_method(item, &Value::Unknown, true))
                }
                _ => false,
            })
    }

    fn namespace_has_only_scalars(&mut self, scope: ScopeId, first: [usize; 2]) -> bool {
        let reads = &self.scopes[scope].namespace_reads;
        if !reads.contains_key(&first) {
            return false;
        }
        // Python 3.11 can refresh an earlier namespace through another read.
        let bindings: BTreeSet<_> = reads
            .range(first..)
            .flat_map(|(_, read)| read.bindings.iter().copied())
            .collect();
        bindings
            .into_iter()
            .all(|id| self.binding_value(id, 0).is_builtin_scalar())
    }

    fn ordering_may_call_method(&self, left: &Value, right: &Value) -> bool {
        let operands = [left, right];
        // Only two potential sequences can invoke element equality during ordering.
        let unknown_may_dispatch = self.has_ordering_hooks
            || (self.has_equality_hooks
                && !operands
                    .into_iter()
                    .any(|value| value.is_builtin_scalar() || matches!(value, Value::Range)));
        operands.into_iter().any(|operand| match operand {
            Value::Unknown | Value::OperatorResult | Value::OpaqueNamespace => unknown_may_dispatch,
            _ => !operand.is_builtin_scalar_or_iterable(),
        })
    }

    /// None means the attribute is absent in a fully resolved base chain.
    fn class_member(&mut self, id: SymbolId, name: &str, depth: usize) -> Option<Value> {
        self.class_member_at(id, name, depth, None)
    }

    fn class_member_at(
        &mut self,
        id: SymbolId,
        name: &str,
        depth: usize,
        access: Option<(ScopeId, Location)>,
    ) -> Option<Value> {
        if depth >= MAX_BINDING_CHAIN {
            return Some(Value::Unknown);
        }
        let symbol = &self.symbols[id];
        let Binding::Definition(definition, class_scope) = symbol.binding else {
            unreachable!()
        };
        if matches!(
            self.metaclass_creation(id),
            Some(metaclasses::ClassCreation::Unknown)
        ) {
            return Some(Value::Unknown);
        }
        match access.and_then(|site| self.class_member_write(id, name, site)) {
            Some(mutations::MemberWriteValue::Replacement(value)) => return Some(value),
            Some(mutations::MemberWriteValue::Ambiguous) => {
                if let Some((scope, at)) = access {
                    self.scope_issue(
                        scope,
                        at,
                        "unresolved_attribute_write",
                        "class attribute may depend on a write with unresolved ordering or value",
                    );
                }
                return Some(Value::Unknown);
            }
            Some(mutations::MemberWriteValue::Deleted) => {}
            None => {
                if let Some(attribute) = self
                    .metaclass_member(id, name)
                    .or_else(|| self.local(class_scope, name, END))
                {
                    return Some(self.binding(attribute, END, depth + 1));
                }
            }
        }
        let inherited = match definition.bases.as_slice() {
            [] => None,
            [base] => match self.class_base(symbol.scope, base, definition.name.at, depth + 1) {
                Value::Callable(Invocation { target: parent, .. }) if self.is_class(parent) => {
                    self.class_member_at(parent, name, depth + 1, access)
                }
                Value::Provider(name) if name == "builtins.object" => None,
                Value::Provider(provider)
                    if self.phase03 && fastapi::is_app_constructor(&provider) =>
                {
                    if access.is_some_and(|site| self.fastapi_base_member_changed(name, site)) {
                        return Some(Value::Unknown);
                    }
                    match name {
                        "__init__" | "__new__" | "__init_subclass__" => None,
                        _ if fastapi::is_supported_app_method(name) => {
                            Some(Value::FastapiInheritedMethod)
                        }
                        _ => Some(Value::Unknown),
                    }
                }
                Value::Provider(base) if self.phase03 && is_django_string_converter(&base) => {
                    match name {
                        "__init__" | "__new__" | "__init_subclass__" => None,
                        "regex" => Some(Value::Text("[^/]+".into())),
                        "to_python" | "to_url" => Some(Value::Provider(format!("{base}.{name}"))),
                        _ => Some(Value::Unknown),
                    }
                }
                Value::Provider(base)
                    if name == "__init_subclass__" && builtin_noop_subclass_hook(&base) =>
                {
                    None
                }
                Value::Provider(base) if self.phase03 && framework_managed_base(&base) => {
                    match (framework_class_provider(&base), name) {
                        (Some(role), "assertEqual" | "assertNotEqual") if role.is_unittest() => {
                            Some(Value::UnittestComparison)
                        }
                        (
                            Some(
                                FrameworkClassRole::DjangoCommand
                                | FrameworkClassRole::DjangoManager
                                | FrameworkClassRole::DjangoQuerySet
                                | FrameworkClassRole::DjangoRelationshipField
                                | FrameworkClassRole::DjangoStorageBase
                                | FrameworkClassRole::BuiltinDict
                                | FrameworkClassRole::DjangoView,
                            ),
                            "__new__",
                        ) => None,
                        (_, "__init_subclass__")
                        | (
                            Some(
                                FrameworkClassRole::PydanticModel
                                | FrameworkClassRole::CeleryTask
                                | FrameworkClassRole::DjangoSignal
                                | FrameworkClassRole::DjangoForm
                                | FrameworkClassRole::DjangoModelForm
                                | FrameworkClassRole::DjangoFormField,
                            ),
                            "__init__" | "__new__",
                        ) => None,
                        (
                            Some(FrameworkClassRole::PydanticModel),
                            "model_dump" | "model_dump_json" | "model_validate",
                        ) => Some(Value::Provider(format!("pydantic.BaseModel.{name}"))),
                        (Some(FrameworkClassRole::DjangoView), "as_view") => {
                            Some(Value::Provider("django.views.View.as_view".into()))
                        }
                        (
                            Some(
                                FrameworkClassRole::DjangoForm
                                | FrameworkClassRole::DjangoModelForm,
                            ),
                            "is_valid" | "full_clean" | "non_field_errors",
                        ) => Some(Value::DjangoFormValidation(None)),
                        (Some(FrameworkClassRole::DjangoSignal), "connect") => {
                            Some(Value::DjangoSignalConnect)
                        }
                        _ => Some(Value::Unknown),
                    }
                }
                _ => Some(Value::Unknown),
            },
            _ if self.phase03 => self
                .django_form_multiple_member(id, name, depth, access)
                .unwrap_or(Some(Value::Unknown)),
            _ => Some(Value::Unknown),
        };
        if matches!(inherited, Some(Value::UnittestComparison))
            && [
                "_getAssertEqualityFunc",
                "_baseAssertEqual",
                "assertMultiLineEqual",
                "_formatMessage",
                "_truncateMessage",
                "failureException",
            ]
            .iter()
            .any(|member| self.local(class_scope, member, END).is_some())
        {
            Some(Value::Unknown)
        } else {
            inherited
        }
    }

    fn class_base(
        &mut self,
        scope: ScopeId,
        base: &Expression,
        before: Location,
        depth: usize,
    ) -> Value {
        if self.phase03
            && matches!(&base.kind, ExpressionKind::Name(name) if name == "type"
                && matches!(self.lexical_binding(scope, name, before, true), LexicalBinding::Missing))
            && !self.scopes[self.scopes[scope].file].open_wildcard
            && self
                .module_member_write("builtins", "type", (scope, before))
                .is_none()
        {
            return Value::Provider("builtins.type".into());
        }
        let value = self.identity(scope, base, before, depth);
        if !self.phase03 || !matches!(value, Value::Unknown) || depth >= MAX_BINDING_CHAIN {
            return value;
        }
        if let ExpressionKind::Call {
            function,
            arguments,
            keywords,
        } = &base.kind
            && arguments.is_empty()
            && keywords.is_empty()
            && let Value::Callable(invocation) = self.probe_identity(scope, function, before)
            && !invocation.verified
            && matches!(invocation.receiver, Receiver::Ordinary)
            && let Binding::Definition(definition, body) = self.symbols[invocation.target].binding
            && definition.kind == DefinitionKind::Function
            && !definition.is_generator
            && definition.decorators.is_empty()
            && definition.parameters.is_empty()
            && let [
                Statement {
                    kind: StatementKind::Return(Some(result)),
                    at,
                },
            ] = definition.body.as_slice()
            && matches!(self.probe_identity(body, result, *at),
                Value::Provider(provider) if provider == "builtins.object")
        {
            // The factory body still executes normally. Only the base value
            // is recovered from its sole statically resolved return.
            return Value::Provider("builtins.object".into());
        }
        value
    }

    fn subclass_hook(&mut self, scope: ScopeId, base: Value, at: Location, subclass: &Definition) {
        let single_base = subclass.bases.len() == 1;
        if self.phase03
            && self.is_ctypes_structure_base(&base)
            && (!single_base || !self.bounded_ctypes_layout(scope, subclass))
        {
            self.scope_issue(
                scope,
                at,
                "unresolved_subclass_hook",
                "ctypes structure layout is outside the bounded class model",
            );
            return;
        }
        let hook = match base {
            Value::Callable(Invocation { target: id, .. }) if self.is_class(id) => {
                self.class_member_at(id, "__init_subclass__", 0, Some((scope, at)))
            }
            Value::MissingFrameworkMember => None,
            Value::Provider(name)
                if builtin_noop_subclass_hook(&name)
                    || self.phase03
                        && (name == "builtins.type"
                            || framework_managed_base(&name)
                            || self.wagtail_declaration_base(&name)
                            || self.wagtail_plain_snippet_base(&name, subclass)
                            || self.django_plain_widget_or_field(&name, subclass)
                            || fastapi::is_app_constructor(&name)) =>
            {
                None
            }
            _ => Some(Value::Unknown),
        };
        match hook {
            None => {}
            Some(Value::Callable(Invocation { target: id, .. }))
                if single_base
                    && matches!(self.symbols[id].binding, Binding::Definition(definition, _)
                        if definition.kind == DefinitionKind::Function) =>
            {
                self.edge(scope, id, "subclass_hook", "certain");
            }
            _ => self.scope_issue(
                scope,
                at,
                "unresolved_subclass_hook",
                "class creation hook dispatch is outside the bounded class hierarchy",
            ),
        }
    }

    fn probe_identity(
        &mut self,
        scope: ScopeId,
        expression: &Expression,
        before: Location,
    ) -> Value {
        let previous = std::mem::replace(&mut self.probing_identity, true);
        let value = self.identity(scope, expression, before, 0);
        self.probing_identity = previous;
        value
    }

    fn identity(
        &mut self,
        scope: ScopeId,
        expression: &Expression,
        before: Location,
        depth: usize,
    ) -> Value {
        self.expression_value(scope, expression, before, depth, ValueEvaluation::Identity)
    }

    fn identity_with_dormant_declarations(
        &mut self,
        scope: ScopeId,
        expression: &Expression,
        before: Location,
        depth: usize,
    ) -> Value {
        let previous = std::mem::replace(&mut self.check_dormant_declarations, true);
        let value = self.identity(scope, expression, before, depth);
        self.check_dormant_declarations = previous;
        value
    }

    fn call_target_identity(
        &mut self,
        scope: ScopeId,
        expression: &Expression,
        before: Location,
        depth: usize,
    ) -> Value {
        let value = self.identity(scope, expression, before, depth);
        if self.phase03
            && !self.check_dormant_declarations
            && matches!(&value, Value::Provider(name) if name == "atexit.register")
        {
            // Dormant assignments still determine Python's lexical scopes.
            // Recheck the provider through any local or module aliases.
            self.identity_with_dormant_declarations(scope, expression, before, depth)
        } else {
            value
        }
    }

    fn expression_value(
        &mut self,
        scope: ScopeId,
        expression: &Expression,
        before: Location,
        depth: usize,
        evaluation: ValueEvaluation<'a>,
    ) -> Value {
        if depth >= MAX_BINDING_CHAIN {
            self.issue(
                self.scopes[scope].file,
                expression.at,
                "analysis_limit",
                "expression identity limit exceeded",
            );
            return Value::Unknown;
        }
        if matches!(evaluation, ValueEvaluation::Lazy { .. })
            && matches!(expression.kind, ExpressionKind::Call { .. })
        {
            return self.lazy_call(scope, expression, before, depth, evaluation);
        }
        // Other effectful operations require additional execution.
        if matches!(evaluation, ValueEvaluation::Lazy { .. })
            && matches!(
                expression.kind,
                ExpressionKind::Await(_)
                    | ExpressionKind::FormattedString(_)
                    | ExpressionKind::KeywordMappingCall { .. }
                    | ExpressionKind::RepeatedKeywordCall { .. }
                    | ExpressionKind::Comprehension { .. }
                    | ExpressionKind::OrderingComparison { .. }
                    | ExpressionKind::Addition(_)
                    | ExpressionKind::Divide(_)
                    | ExpressionKind::Operands(_)
            )
        {
            self.scope_issue(
                scope,
                expression.at,
                "unresolved_lazy_export",
                "effects of the lazy return expression are outside the bounded model",
            );
            return Value::Unknown;
        }
        let recurse = |this: &mut Self, expression| {
            this.expression_value(scope, expression, before, depth + 1, evaluation)
        };
        match &expression.kind {
            ExpressionKind::IterationVariable(binding) => {
                let binding = position(*binding);
                if let Some((_, value)) = self
                    .iteration_locals
                    .iter()
                    .rev()
                    .find(|(target, _)| *target == binding)
                {
                    value.clone()
                } else {
                    self.scope_issue(
                        scope,
                        expression.at,
                        "unbound_local",
                        "comprehension variable has not been bound by its iterator",
                    );
                    Value::Unknown
                }
            }
            ExpressionKind::Comprehension {
                form: ComprehensionForm::Generator,
                ..
            } => Value::DeferredComprehension,
            ExpressionKind::Comprehension { .. } => Value::ComprehensionResult,
            ExpressionKind::FormattedString(_) => Value::Scalar,
            ExpressionKind::Name(name) => {
                let lazy = matches!(evaluation, ValueEvaluation::Lazy { .. });
                let binding = self.lexical_binding(
                    scope,
                    name,
                    before,
                    lazy || self.check_dormant_declarations,
                );
                if (lazy || self.check_dormant_declarations)
                    && matches!(binding, LexicalBinding::UnboundLocal)
                {
                    if self.check_dormant_declarations {
                        self.scope_issue(
                            scope,
                            expression.at,
                            "unbound_local",
                            format!("{name} has no active local binding"),
                        );
                    }
                    return Value::Unknown;
                }
                if let LexicalBinding::Found(id) = binding {
                    self.record_django_global_read(scope, id, expression.at);
                    if let ValueEvaluation::Lazy {
                        argument: Some((parameter, text)),
                        ..
                    } = evaluation
                        && id == parameter
                    {
                        self.read.insert(id);
                        return Value::Text(text.to_owned());
                    }
                    let binding_scope = self.symbols[id].scope;
                    let binding_before = if binding_scope == scope
                        || self.immediately_encloses_class_body(scope, binding_scope)
                    {
                        // A class suite runs where it is declared. Unlike a
                        // deferred function body, it can select the binding
                        // currently installed by each enclosing suite.
                        before
                    } else {
                        if self.scopes[binding_scope].names[name.as_str()].len() > 1 {
                            self.scope_issue(scope, expression.at, "ambiguous_binding",
                                "an enclosing name has multiple bindings and the active version is unresolved");
                            return Value::Unknown;
                        }
                        END
                    };
                    if binding_scope == self.scopes[binding_scope].file
                        && let Some(write) = self.module_global_write(
                            binding_scope,
                            name,
                            binding_before,
                            (scope, before),
                        )
                    {
                        return self.module_global_value(scope, expression.at, write);
                    }
                    let value = self.binding(id, binding_before, depth + 1);
                    if matches!(value, Value::DeletedModuleBinding) {
                        self.scope_issue(
                            scope,
                            expression.at,
                            "unbound_local",
                            format!("{name} was deleted before this read"),
                        );
                        return Value::Unknown;
                    }
                    if self.phase03
                        && !lazy
                        && binding_scope != scope
                        && matches!(&value, Value::Provider(provider)
                            if matches!(provider.as_str(), "builtins" | "builtins.eval" | "builtins.exec"))
                        && matches!(
                            self.lexical_binding(scope, name, before, true),
                            LexicalBinding::UnboundLocal
                        )
                    {
                        // A dormant local declaration can shadow an imported
                        // builtin, even when the alias has a different name.
                        return Value::Unknown;
                    }
                    if let ValueEvaluation::Lazy {
                        execution_scope: Some(caller),
                        ..
                    } = evaluation
                    {
                        self.record_class_reference(caller, &value);
                    }
                    return value;
                }
                if self.is_namespace_builtin(name) {
                    let file = self.scopes[scope].file;
                    if self.scopes[file].open_wildcard
                        && (scope == file || matches!(name.as_str(), "eval" | "exec"))
                    {
                        self.scope_issue(
                            scope,
                            expression.at,
                            "unresolved_import",
                            "namespace inspection may read unmodeled FastAPI star imports",
                        );
                    }
                    return match binding {
                        LexicalBinding::Missing => Value::Provider(format!("builtins.{name}")),
                        _ => Value::Unknown,
                    };
                }
                if name == "__name__" {
                    return Value::Unknown;
                }
                if ((self.grep_verify || lazy) && name == "getattr"
                    || self.phase03 && matches!(name.as_str(), "setattr" | "delattr")
                    || matches!(
                        name.as_str(),
                        "print"
                            | "len"
                            | "range"
                            | "str"
                            | "int"
                            | "bool"
                            | "open"
                            | "object"
                            | "list"
                            | "tuple"
                            | "dict"
                            | "set"
                            | "float"
                            | "bytes"
                            | "ValueError"
                            | "RuntimeError"
                            | "AttributeError"
                            | "TypeError"
                            | "Exception"
                            | "staticmethod"
                            | "classmethod"
                            | "property"
                    ))
                    && !self.scopes[scope].names.contains_key(name.as_str())
                {
                    return Value::Provider(format!("builtins.{name}"));
                }
                if matches!(binding, LexicalBinding::Missing)
                    && self.scopes[self.scopes[scope].file].open_wildcard
                    && !name.starts_with('_')
                {
                    self.scope_issue(
                        scope,
                        expression.at,
                        "unresolved_import",
                        format!("{name} may be an unmodeled FastAPI star import"),
                    );
                }
                Value::Unknown
            }
            ExpressionKind::Member(base, name) => {
                let base = recurse(self, base);
                if let ValueEvaluation::Lazy {
                    execution_scope, ..
                } = evaluation
                {
                    return self.lazy_attribute(
                        base,
                        name,
                        (scope, before),
                        depth + 1,
                        execution_scope,
                    );
                }
                self.member(base, name, (scope, before), depth + 1)
            }
            ExpressionKind::Call {
                function,
                arguments,
                keywords,
            } if self.phase03
                && let Some(call) = self.literal_text_call(
                    function,
                    CallArguments {
                        scope,
                        before,
                        at: expression.at,
                        positional: arguments,
                        keywords,
                    },
                    depth,
                )
                && let Value::Text(value) = call.value =>
            {
                Value::Text(value)
            }
            ExpressionKind::Call {
                function,
                arguments,
                keywords,
            } => match self.call_target_identity(scope, function, before, depth + 1) {
                Value::Provider(provider)
                    if self.phase03 && mcp_tools::is_server_provider(&provider) =>
                {
                    self.mcp_server(scope, expression.at, arguments, keywords)
                }
                Value::Provider(name)
                    if self.empty_flask_context_pop(&name, arguments, keywords) =>
                {
                    Value::EmptyFlaskContextEntry
                }
                Value::Provider(name) if self.phase03 && django_forms::is_field_provider(&name) => {
                    self.django_field_value(
                        &name,
                        CallArguments {
                            scope,
                            before,
                            at: expression.at,
                            positional: arguments,
                            keywords,
                        },
                    )
                    .map_or(Value::Unknown, Value::DjangoField)
                }
                Value::DjangoViewFactory(class) if arguments.is_empty() && keywords.is_empty() => {
                    Value::DjangoView(class)
                }
                Value::DjangoViewFactory(_)
                    if self.django_view_has_missing_option(scope, before, arguments, keywords) =>
                {
                    Value::DjangoInvalidRegistration
                }
                Value::Callable(invocation) if self.is_class(invocation.target) => {
                    if self.phase03
                        && let Some(app) = self.fastapi_subclass_app(
                            invocation,
                            CallArguments {
                                scope,
                                at: expression.at,
                                before,
                                positional: arguments,
                                keywords,
                            },
                            depth + 1,
                        )
                    {
                        return app;
                    }
                    self.allocation_class(invocation.target, depth + 1)
                        .map_or(Value::Unknown, Value::Instance)
                }
                Value::Alternatives(values) => {
                    let mut instances = Vec::new();
                    for value in values {
                        let Value::Callable(Invocation { target: id, .. }) = value else {
                            return Value::Unknown;
                        };
                        if !self.is_class(id) {
                            return Value::Unknown;
                        }
                        let Some(class) = self.allocation_class(id, depth + 1) else {
                            return Value::Unknown;
                        };
                        instances.push(Value::Instance(class));
                    }
                    Value::alternatives(instances)
                }
                Value::Callable(invocation) if matches!(self.symbols[invocation.target].binding, Binding::Definition(def, _) if def.kind == DefinitionKind::Coroutine) => {
                    Value::Coroutine(
                        self.with_explicit_receiver(invocation, arguments, keywords, recurse),
                    )
                }
                Value::Callable(invocation) if self.phase03 => {
                    if let Some(app) = self.fastapi_subclass_app(
                        invocation,
                        CallArguments {
                            scope,
                            at: expression.at,
                            before,
                            positional: arguments,
                            keywords,
                        },
                        depth + 1,
                    ) {
                        return app;
                    }
                    if arguments.is_empty()
                        && keywords.is_empty()
                        && let Some(value) =
                            self.flask_factory_result(invocation, scope, expression.at, depth + 1)
                    {
                        return value;
                    }
                    self.fastapi_conditional_app_result(invocation, arguments, keywords, depth + 1)
                }
                Value::ConstantFunction => Value::Scalar,
                Value::NullaryConstantFunction if arguments.is_empty() && keywords.is_empty() => {
                    Value::Scalar
                }
                Value::IdentityFunction if arguments.len() == 1 && keywords.is_empty() => {
                    recurse(self, &arguments[0])
                }
                Value::Provider(name) if self.grep_verify && name == "builtins.getattr" => self
                    .verification_member(scope, arguments, keywords, before, depth)
                    .map_or(Value::Unknown, |(value, _)| value),
                Value::Provider(name) if name == "builtins.open" => {
                    Value::Provider("builtins.open.result".into())
                }
                Value::Provider(name) if name == "asyncio.create_task" => Value::ScheduledTask,
                Value::Provider(name) if self.phase03 && name == "atexit.register" => {
                    // register returns the same callable it receives.
                    arguments.first().map_or(Value::Unknown, |argument| {
                        self.identity_with_dormant_declarations(scope, argument, before, depth + 1)
                    })
                }
                Value::Provider(name)
                    if name == "builtins.object" && arguments.is_empty() && keywords.is_empty() =>
                {
                    Value::Scalar
                }
                Value::Provider(name)
                    if name == "builtins.globals"
                        && self.has_command_namespace(scope)
                        && arguments.is_empty()
                        && keywords.is_empty() =>
                {
                    Value::CommandNamespace(scope)
                }
                Value::Provider(name)
                    if name == "builtins.vars"
                        && self.has_command_namespace(scope)
                        && matches!(arguments.as_slice(), [module] if matches!(recurse(self, module), Value::Module(file) if file == scope))
                        && keywords.is_empty() =>
                {
                    Value::CommandNamespace(scope)
                }
                Value::CommandNamespaceGet(file)
                    if matches!(arguments.as_slice(), [key] if matches!(&key.kind, ExpressionKind::Text(key) if key == "__name__"))
                        && keywords.is_empty() =>
                {
                    self.local(file, "__name__", before)
                        .map_or(Value::Scalar, |binding| {
                            self.binding(binding, before, depth + 1)
                        })
                }
                Value::Provider(name)
                    if matches!(name.as_str(), "builtins.locals" | "builtins.vars")
                        && arguments.is_empty()
                        && keywords.is_empty()
                        && self.in_function(scope) =>
                {
                    Value::LocalNamespace(scope, position(expression.at))
                }
                Value::Provider(name)
                    if name == "builtins.len" && arguments.len() == 1 && keywords.is_empty() =>
                {
                    // The call walker handles __len__ dispatch independently of its integer result.
                    Value::Scalar
                }
                Value::Provider(name) if name == "builtins.bool" && keywords.is_empty() => {
                    match arguments.as_slice() {
                        [] => Value::Scalar,
                        [argument] => {
                            let value = recurse(self, argument);
                            // A scalar result alone cannot account for a custom truth test.
                            if value.is_builtin_scalar_or_iterable()
                                || matches!(
                                    value,
                                    Value::Sequence(_)
                                        | Value::Mapping(_)
                                        | Value::LocalNamespace(..)
                                        | Value::NamespaceCopy(_)
                                )
                            {
                                Value::Scalar
                            } else {
                                Value::Unknown
                            }
                        }
                        _ => Value::Unknown,
                    }
                }
                Value::Provider(name) if name == "builtins.dict" => {
                    let named: BTreeMap<_, _> = keywords
                        .iter()
                        .map(|(name, value)| (name.clone(), recurse(self, value)))
                        .collect();
                    let site = (scope, position(expression.at));
                    match arguments.as_slice() {
                        [] => Value::Mapping(mapping_entries(named).collect()),
                        [argument] => match recurse(self, argument) {
                            mapping @ Value::Mapping(_) if named.is_empty() => mapping,
                            Value::Mapping(entries)
                                if entries.iter().any(Value::may_contain_namespace)
                                    || named.values().any(Value::may_contain_namespace) =>
                            {
                                // Exact string keys overwritten by keywords
                                // no longer expose their old values.
                                let (pairs, _) = entries.as_chunks::<2>();
                                let mut result: Vec<_> = pairs
                                    .iter()
                                    .filter(|entry| !matches!(&entry[0], Value::Text(key) if named.contains_key(key)))
                                    .flatten()
                                    .cloned()
                                    .collect();
                                result.extend(mapping_entries(named));
                                Value::Mapping(result)
                            }
                            Value::LocalNamespace(owner, first) => {
                                let cutoff = self.scopes[scope]
                                    .namespace_copy_cutoffs
                                    .get(&site.1)
                                    .copied()
                                    .unwrap_or(site.1);
                                Value::NamespaceCopy(Box::new(NamespaceCopy {
                                    owner,
                                    first,
                                    copied_at: (scope, cutoff),
                                    site,
                                    keywords: named,
                                }))
                            }
                            Value::NamespaceCopy(mut copy) => {
                                copy.site = site;
                                copy.keywords.extend(named);
                                Value::NamespaceCopy(copy)
                            }
                            Value::OpaqueNamespace => Value::OpaqueNamespace,
                            _ if named.values().any(Value::may_contain_namespace) => {
                                Value::OpaqueNamespace
                            }
                            _ => Value::Unknown,
                        },
                        _ => Value::Unknown,
                    }
                }
                Value::Provider(name)
                    if keywords.is_empty()
                        && matches!(name.as_str(), "builtins.list" | "builtins.tuple") =>
                {
                    match arguments.as_slice() {
                        [] => Value::Sequence(Vec::new()),
                        [argument] => match recurse(self, argument) {
                            value @ (Value::Sequence(_) | Value::BuiltinIterable) => value,
                            Value::Range | Value::Text(_) => Value::BuiltinIterable,
                            _ => Value::Unknown,
                        },
                        _ => Value::Unknown,
                    }
                }
                Value::Provider(name) if name == "builtins.set" && keywords.is_empty() => {
                    // A mutable input can acquire elements through aliases. When
                    // selected hooks can matter, trust scalar elements only in a
                    // fresh literal or an immutable input.
                    let has_element_hooks = self.has_hash_hooks || self.has_equality_hooks;
                    match arguments.as_slice() {
                        [] => Value::BuiltinIterable,
                        [argument] => match recurse(self, argument) {
                            Value::Range | Value::Text(_) => Value::BuiltinIterable,
                            Value::BuiltinIterable if !has_element_hooks => Value::BuiltinIterable,
                            Value::Sequence(items)
                                if (!has_element_hooks
                                    || matches!(
                                        argument.kind,
                                        ExpressionKind::Sequence { .. }
                                    ))
                                    && items.iter().all(Value::is_builtin_scalar) =>
                            {
                                Value::BuiltinIterable
                            }
                            _ => Value::Unknown,
                        },
                        _ => Value::Unknown,
                    }
                }
                Value::Provider(name)
                    if name == "builtins.range"
                        && keywords.is_empty()
                        && arguments
                            .iter()
                            .all(|argument| recurse(self, argument).is_builtin_scalar()) =>
                {
                    Value::Range
                }
                Value::Provider(name)
                    if keywords.is_empty()
                        && matches!(
                            name.as_str(),
                            "builtins.int"
                                | "builtins.float"
                                | "builtins.str"
                                | "builtins.bytes"
                                | "builtins.print"
                        )
                        && arguments.iter().all(|argument| {
                            recurse(self, argument).is_builtin_scalar_or_iterable()
                        }) =>
                {
                    Value::Scalar
                }
                Value::Provider(name)
                    if matches!(
                        name.as_str(),
                        "fastapi.FastAPI" | "fastapi.applications.FastAPI"
                    ) =>
                {
                    Value::Object(
                        (
                            self.scopes[scope].file,
                            expression.at.line,
                            expression.at.column,
                        ),
                        ObjectKind::App,
                    )
                }
                Value::Provider(name)
                    if matches!(
                        name.as_str(),
                        "fastapi.APIRouter" | "fastapi.routing.APIRouter"
                    ) =>
                {
                    Value::Object(
                        (
                            self.scopes[scope].file,
                            expression.at.line,
                            expression.at.column,
                        ),
                        ObjectKind::Router,
                    )
                }
                Value::Provider(name)
                    if self.phase03
                        && matches!(name.as_str(), "flask.Flask" | "flask.app.Flask") =>
                {
                    self.flask_instance(scope, expression.at, before, ObjectKind::FlaskApp)
                }
                Value::Provider(name)
                    if self.phase03
                        && matches!(
                            name.as_str(),
                            "flask.Blueprint" | "flask.blueprints.Blueprint"
                        ) =>
                {
                    self.flask_instance(scope, expression.at, before, ObjectKind::FlaskBlueprint)
                }
                Value::Provider(name)
                    if self.phase03
                        && is_flask_login_constructor(&name)
                        && registration_arguments(
                            arguments,
                            keywords,
                            ["app", "add_context_processor"],
                        )
                        .is_some() =>
                {
                    if arguments.is_empty()
                        && keywords.is_empty()
                        && let Some((_, class)) = self.flask_login_allocation(scope, before)
                    {
                        Value::Instance(class)
                    } else {
                        self.flask_instance(
                            scope,
                            expression.at,
                            before,
                            ObjectKind::FlaskLoginManager,
                        )
                    }
                }
                Value::Provider(name)
                    if self.phase03
                        && is_typer_constructor(&name)
                        && arguments.is_empty()
                        && keywords.is_empty() =>
                {
                    Value::Object(
                        (
                            self.scopes[scope].file,
                            expression.at.line,
                            expression.at.column,
                        ),
                        ObjectKind::TyperApp,
                    )
                }
                Value::Provider(name) if self.phase03 && name == "celery.Celery" => Value::Object(
                    (
                        self.scopes[scope].file,
                        expression.at.line,
                        expression.at.column,
                    ),
                    ObjectKind::CeleryApp,
                ),
                Value::Provider(name) if self.phase03 && name == "pluggy.HookimplMarker" => {
                    match (arguments.as_slice(), keywords.as_slice()) {
                        ([project], []) if matches!(recurse(self, project), Value::Text(name) if !name.is_empty()) => {
                            Value::PluggyHookMarker
                        }
                        _ => Value::Unknown,
                    }
                }
                Value::Provider(name)
                    if self.phase03
                        && aiohttp::is_route_table(&name)
                        && arguments.is_empty()
                        && keywords.is_empty() =>
                {
                    Value::Object(
                        (
                            self.scopes[scope].file,
                            expression.at.line,
                            expression.at.column,
                        ),
                        ObjectKind::AiohttpRouteTable,
                    )
                }
                Value::Provider(name)
                    if self.phase03
                        && starlette::is_constructor(&name)
                        && arguments.is_empty()
                        && keywords.is_empty() =>
                {
                    Value::Object(
                        (
                            self.scopes[scope].file,
                            expression.at.line,
                            expression.at.column,
                        ),
                        ObjectKind::StarletteApp,
                    )
                }
                Value::Provider(name) if self.phase03 && sanic::is_constructor(&name) => {
                    self.sanic_constructor(scope, expression.at, before, arguments, keywords)
                }
                Value::Provider(name) if self.phase03 && is_django_signal_constructor(&name) => {
                    if registration_arguments(arguments, keywords, ["use_caching"]).is_some() {
                        Value::DjangoSignal((
                            self.scopes[scope].file,
                            expression.at.line,
                            expression.at.column,
                        ))
                    } else {
                        Value::Unknown
                    }
                }
                Value::Provider(name) if self.phase03 && is_django_receiver(&name) => {
                    Value::DjangoReceiver(
                        self.django_receiver(scope, arguments, keywords, before, depth),
                    )
                }
                Value::Provider(name) if self.phase03 && name == "pathlib.Path" => self
                    .source_project_path(scope, before, arguments, keywords)
                    .map_or(Value::Unknown, Value::ProjectPath),
                Value::ResolveProjectPath(path) if arguments.is_empty() && keywords.is_empty() => {
                    Value::ProjectPath(path)
                }
                Value::Provider(name) if self.phase03 && name == "django.urls.include" => self
                    .django_include(
                        CallArguments {
                            scope,
                            before,
                            at: expression.at,
                            positional: arguments,
                            keywords,
                        },
                        recurse,
                    ),
                Value::Provider(name)
                    if self.phase03
                        && matches!(name.as_str(), "django.urls.path" | "django.urls.re_path") =>
                {
                    self.django_path(arguments, keywords, name == "django.urls.path", recurse)
                }
                Value::Provider(name) if self.phase03 && name == "sqlalchemy.event.listens_for" => {
                    Value::Provider("sqlalchemy.event.listens_for.callback".into())
                }
                Value::Provider(name)
                    if self.phase03
                        && is_declarative_base_factory(&name)
                        && arguments.is_empty()
                        && keywords.is_empty() =>
                {
                    Value::Provider("sqlalchemy.orm.declarative_base.result".into())
                }
                Value::Provider(name)
                    if self.phase03
                        && matches!(name.as_str(), "fastapi.Depends" | "fastapi.Security") =>
                {
                    if name == "fastapi.Depends" && arguments.is_empty() && keywords.is_empty() {
                        return Value::FastapiInferredDependency;
                    }
                    let dependency = arguments.first().or_else(|| {
                        keywords
                            .iter()
                            .find(|(name, _)| name == "dependency")
                            .map(|(_, value)| value)
                    });
                    let target = dependency
                        .map(|value| recurse(self, value))
                        .and_then(|value| self.fastapi_dependency_target(value));
                    Value::FastapiDependency(target)
                }
                Value::Provider(name) if self.phase03 && is_fastapi_field_marker(&name) => {
                    Value::FastapiFieldMarker
                }
                Value::Provider(name)
                    if self.phase03
                        && matches!(
                            name.as_str(),
                            "starlette.middleware.Middleware" | "fastapi.middleware.Middleware"
                        ) =>
                {
                    Value::FastapiMiddleware(
                        self.fastapi_middleware_callback(scope, before, arguments, keywords, "cls"),
                    )
                }
                Value::Provider(name)
                    if matches!(
                        name.as_str(),
                        "concurrent.futures.ThreadPoolExecutor"
                            | "concurrent.futures.ProcessPoolExecutor"
                    ) =>
                {
                    Value::Executor
                }
                Value::Provider(name) if name == "multiprocessing.Process" => {
                    let target = keywords
                        .iter()
                        .find(|(name, _)| name == "target")
                        .map_or(Value::Unknown, |(_, target)| recurse(self, target));
                    Value::Process(Box::new(target))
                }
                _ => Value::Unknown,
            },
            ExpressionKind::Text(text) => Value::Text(text.clone()),
            ExpressionKind::Bool(value) => Value::Bool(*value),
            ExpressionKind::None if self.phase03 => Value::None,
            ExpressionKind::Scalar(_) | ExpressionKind::None => Value::Scalar,
            ExpressionKind::ConstantLambda => Value::ConstantFunction,
            ExpressionKind::NullaryConstantLambda => Value::NullaryConstantFunction,
            ExpressionKind::IdentityLambda => Value::Unknown,
            ExpressionKind::IdentityDecoratorLambda => Value::IdentityDecoratorFactory,
            ExpressionKind::DeferredLambda(_) | ExpressionKind::DeferredBoolLambda => {
                Value::DeferredLambda
            }
            ExpressionKind::EmptyListComprehension => Value::Sequence(Vec::new()),
            ExpressionKind::Divide(operands) if self.phase03 => {
                let [left, right] = operands.as_slice() else {
                    return Value::Unknown;
                };
                let path = recurse(self, left);
                if let Value::ProjectPath(path) = path {
                    return self.join_project_path(path, right, (scope, before));
                }
                if self.has_namespace_reads(scope)
                    && (path.may_contain_namespace()
                        || recurse(self, right).may_contain_namespace())
                {
                    Value::OpaqueNamespace
                } else {
                    Value::OperatorResult
                }
            }
            ExpressionKind::Addition(operands)
            | ExpressionKind::Divide(operands)
            | ExpressionKind::Operands(operands) => {
                if self.has_namespace_reads(scope)
                    && operands
                        .iter()
                        .any(|value| recurse(self, value).may_contain_namespace())
                {
                    Value::OpaqueNamespace
                } else {
                    Value::OperatorResult
                }
            }
            ExpressionKind::Sequence { items, .. } => {
                Value::Sequence(items.iter().map(|expr| recurse(self, expr)).collect())
            }
            ExpressionKind::Dictionary(items) => {
                Value::Mapping(items.iter().map(|expr| recurse(self, expr)).collect())
            }
            ExpressionKind::Subscript { value, index } => {
                let value = recurse(self, value);
                if self.is_current_command_module(scope, &value, index, before) {
                    return Value::Module(scope);
                }
                if self.phase03
                    && is_typing_annotated(&value)
                    && let ExpressionKind::Sequence {
                        items: arguments, ..
                    } = &index.kind
                    && let Some((annotation, metadata)) = arguments.split_first()
                {
                    return self.fastapi_selected_metadata(
                        scope,
                        before,
                        annotation,
                        metadata,
                        depth + 1,
                    );
                }
                let index = recurse(self, index);
                match (value, index) {
                    (Value::Mapping(items), Value::Text(index)) => items
                        .as_chunks::<2>()
                        .0
                        .iter()
                        .rev()
                        .find_map(|pair| match &pair[0] {
                            Value::Text(key) if key == &index => Some(pair[1].clone()),
                            _ => None,
                        })
                        .unwrap_or(Value::Unknown),
                    _ => Value::Unknown,
                }
            }
            ExpressionKind::Conditional {
                test,
                body,
                otherwise,
            } => match recurse(self, test) {
                Value::Bool(true) => recurse(self, body),
                Value::Bool(false) => recurse(self, otherwise),
                _ => {
                    let values = vec![recurse(self, body), recurse(self, otherwise)];
                    if values.iter().any(Value::contains_unknown) {
                        Value::Unknown
                    } else {
                        Value::alternatives(values)
                    }
                }
            },
            ExpressionKind::Membership {
                element,
                container,
                contains,
            } => {
                let element = recurse(self, element);
                let container = recurse(self, container);
                match (element, container) {
                    (Value::Text(element), Value::Mapping(items)) => {
                        let found =
                            items.as_chunks::<2>().0.iter().any(
                                |pair| matches!(&pair[0], Value::Text(key) if key == &element),
                            );
                        Value::Bool(found == *contains)
                    }
                    _ => Value::Unknown,
                }
            }
            ExpressionKind::Compare {
                left,
                right,
                equal,
                identity,
            } => {
                let left_none = matches!(left.kind, ExpressionKind::None);
                let right_none = matches!(right.kind, ExpressionKind::None);
                let left = recurse(self, left);
                let right = recurse(self, right);
                if *identity
                    && (left_none && matches!(right, Value::EmptyFlaskContextEntry)
                        || right_none && matches!(left, Value::EmptyFlaskContextEntry))
                {
                    return Value::Bool(*equal);
                }
                let same = match (&left, &right) {
                    (Value::Text(left), Value::Text(right)) if !identity => left == right,
                    (Value::Bool(left), Value::Bool(right)) => left == right,
                    _ if *identity
                        || (left.is_builtin_scalar_or_iterable()
                            && right.is_builtin_scalar_or_iterable()) =>
                    {
                        return Value::Scalar;
                    }
                    _ => return Value::Unknown,
                };
                Value::Bool(same == *equal)
            }
            ExpressionKind::Not(value) => match recurse(self, value) {
                Value::Bool(value) => Value::Bool(!value),
                _ => Value::Scalar,
            },
            ExpressionKind::Boolean { and, values } => {
                let mut result = Value::Bool(*and);
                for (index, value) in values.iter().enumerate() {
                    match recurse(self, value) {
                        Value::Bool(value) if value != *and => {
                            if matches!(result, Value::Bool(_)) {
                                result = Value::Bool(value);
                            }
                            break;
                        }
                        Value::Bool(_) => {}
                        Value::LocalNamespace(owner, first)
                            if *and
                                && index + 1 < values.len()
                                && self.namespace_is_nonempty(owner, first) => {}
                        value if value.may_contain_namespace() => {
                            return Value::OpaqueNamespace;
                        }
                        _ if !self.has_namespace_reads(scope) => return Value::Unknown,
                        _ => result = Value::Unknown,
                    }
                }
                result
            }
            ExpressionKind::OrderingComparison { left, right } => {
                let left = recurse(self, left);
                let right = recurse(self, right);
                if self.ordering_may_call_method(&left, &right) {
                    Value::Unknown
                } else {
                    Value::Scalar
                }
            }
            ExpressionKind::RepeatedKeywordCall { function, .. }
                if self.phase03 && django_urls::is_django_url_factory(&recurse(self, function)) =>
            {
                Value::DjangoInvalidRegistration
            }
            ExpressionKind::Await(_)
            | ExpressionKind::KeywordMappingCall { .. }
            | ExpressionKind::RepeatedKeywordCall { .. }
            | ExpressionKind::ForwardingCall { .. } => Value::Unknown,
        }
    }

    fn edge(
        &mut self,
        scope: ScopeId,
        target: SymbolId,
        kind: &'static str,
        certainty: &'static str,
    ) {
        self.scopes[scope].edges.push(Edge {
            target,
            kind,
            certainty,
        });
    }

    fn invoke(
        &mut self,
        scope: ScopeId,
        invocation: Invocation,
        kind: &'static str,
        certainty: &'static str,
        at: Location,
    ) {
        if self.skip_changed_command_body(scope, invocation.target, at, kind) {
            return;
        }
        let atexit = is_atexit_edge(kind);
        let (kind, certainty) = match (invocation.verified, kind) {
            (true, "direct_call") => ("grep_dynamic_call", "possible"),
            (true, _) => ("grep_dynamic", "possible"),
            (false, _) => (kind, certainty),
        };
        let owner = self.scopes[self.symbols[invocation.target].scope].owner;
        let mut deferred_atexit_receiver = None;
        if let Some(class) = invocation.receiver.supplied()
            && self.receiver_parameter(invocation.target).is_some()
        {
            if atexit {
                // A shutdown receiver only affects dispatch when its registration runs.
                deferred_atexit_receiver = Some(class);
            } else {
                self.method_receivers
                    .entry(invocation.target)
                    .or_default()
                    .insert(class);
            }
        }
        if deferred_atexit_receiver.is_none()
            && (matches!(invocation.receiver, Receiver::Ordinary)
                || matches!(invocation.receiver, Receiver::Bound(class) if Some(class) == owner))
        {
            self.edge(scope, invocation.target, kind, certainty);
        } else {
            self.class_invocations.push(ClassInvocation {
                scope,
                at,
                edge: Edge {
                    target: invocation.target,
                    kind,
                    certainty,
                },
                receiver: invocation.receiver,
                deferred_atexit_receiver,
            });
        }
    }

    fn skip_changed_command_body(
        &mut self,
        scope: ScopeId,
        target: SymbolId,
        at: Location,
        kind: &str,
    ) -> bool {
        if !self.django_command_has_code_write(target) {
            return false;
        }
        if kind == "direct_call" {
            match self.django_command_code_write(target, (scope, at)) {
                Some(mutations::MemberWriteValue::Replacement(_)) => return true,
                None => return false,
                _ => {}
            }
        }
        self.scope_issue(
            scope,
            at,
            "unresolved_call",
            "Django command code can change before this invocation",
        );
        true
    }

    fn receiver_conflicts(&self, invocation: Invocation) -> bool {
        if matches!(invocation.receiver, Receiver::Ordinary) {
            return false;
        }
        let inferred_class = self
            .receiver_parameter(invocation.target)
            .and_then(|parameter| {
                self.instance_receivers
                    .get(&parameter)
                    .copied()
                    .filter(|class| {
                        self.read.contains(&parameter) && !self.subclassed.contains(class)
                    })
            });
        inferred_class.is_some() && inferred_class != invocation.receiver.supplied()
    }

    fn resolve_class_invocations(&mut self) {
        // A module can read a method before analysis has visited its class suite
        // and determined whether decorators preserve instance binding.
        for call in std::mem::take(&mut self.class_invocations) {
            if self.receiver_conflicts(Invocation {
                target: call.edge.target,
                receiver: call.receiver,
                implicit_descriptor_receiver: false,
                verified: false,
            }) {
                self.scope_issue(
                    call.scope,
                    call.at,
                    "unresolved_call",
                    "explicit method receiver is outside the bounded model",
                );
            } else {
                if let Some(class) = call.deferred_atexit_receiver {
                    self.atexit_receivers
                        .entry(call.scope)
                        .or_default()
                        .push((call.edge, class));
                }
                self.scopes[call.scope].edges.push(call.edge);
            }
        }
    }

    fn resolve_receiver_calls(&mut self) {
        loop {
            let before = self.resolved_receiver_calls.len();
            for index in 0..self.receiver_calls.len() {
                let call = self.receiver_calls[index].clone();
                let Some(owner) = self.scopes[self.symbols[call.parameter].scope].owner else {
                    continue;
                };
                let receivers = self
                    .method_receivers
                    .get(&owner)
                    .cloned()
                    .unwrap_or_default();
                let certainty = if receivers.len() == 1 {
                    "certain"
                } else {
                    "possible"
                };
                for class in receivers {
                    if !self.resolved_receiver_calls.insert((index, class)) {
                        continue;
                    }
                    if self.unresolved_django_command_lookup(class, (call.scope, call.at)) {
                        continue;
                    }
                    match self.class_member_at(class, &call.member, 0, Some((call.scope, call.at)))
                    {
                        Some(Value::ConstantFunction) => {}
                        Some(Value::UnittestComparison) if call.bounded_comparison => {}
                        Some(Value::Callable(invocation)) => self.invoke(
                            call.scope,
                            invocation.bind_receiver(Receiver::Bound(class)),
                            "direct_call",
                            certainty,
                            call.at,
                        ),
                        _ => {
                            if self.receiver_call_issues.insert(index) {
                                self.scope_issue(
                                    call.scope,
                                    call.at,
                                    "unresolved_call",
                                    "receiver method dispatch is outside the bounded class hierarchy",
                                );
                            }
                        }
                    }
                }
            }
            if self.resolved_receiver_calls.len() == before {
                break;
            }
        }
    }

    fn report_unbound_receiver_calls(&mut self, reach: &Reachability) {
        let resolved: BTreeSet<_> = self
            .resolved_receiver_calls
            .iter()
            .chain(&reach.resolved_deferred_receiver_calls)
            .map(|&(index, _)| index)
            .chain(reach.resolved_django_reloader_parameters.iter().copied())
            .collect();
        for (index, call) in self.receiver_calls.iter().enumerate() {
            if reach.scopes.contains(&call.scope) && !resolved.contains(&index) {
                self.issues.push(
                    Issue::new(
                        "unresolved_call",
                        &self.files[self.scopes[call.scope].file].path,
                        "receiver type is outside the bounded invocation model",
                    )
                    .at(call.at),
                );
            }
        }
    }

    fn expose(&mut self, scope: ScopeId, value: Value, kind: &'static str, at: Location) {
        let invocation = match value {
            Value::Callable(invocation) | Value::Coroutine(invocation) => invocation,
            Value::Provider(provider)
                if self.phase03 && is_bounded_declaration_provider(&provider) =>
            {
                self.scope_issue(
                    scope,
                    at,
                    "unresolved_attribute_write",
                    "framework declaration provider escapes to an unmodeled consumer",
                );
                return;
            }
            Value::Sequence(values) | Value::Mapping(values) | Value::Alternatives(values) => {
                for value in values {
                    self.expose(scope, value, kind, at);
                }
                return;
            }
            Value::OpaqueNamespace => {
                self.scope_issue(
                    scope,
                    at,
                    "unresolved_export",
                    "a function namespace escapes through an expression outside the bounded model",
                );
                return;
            }
            Value::DeferredComprehension | Value::ComprehensionResult | Value::DeferredLambda => {
                self.report_unmodeled_deferred_value(scope, at, &value);
                return;
            }
            Value::LocalNamespace(..) | Value::NamespaceCopy(_) => {
                let row = &mut self.scopes[scope];
                row.namespace_escape_exclusions
                    .entry(position(at))
                    .or_insert_with(|| row.namespace_exclusions.clone());
                self.namespace_exports
                    .entry(scope)
                    .or_default()
                    .push(Export {
                        scope,
                        at,
                        kind: "local_namespace",
                        value,
                    });
                return;
            }
            _ => return,
        };
        self.invoke(scope, invocation, kind, "possible", at);
        self.uncertain(
            scope,
            at,
            "a known target may be invoked through an external or asynchronous consumer",
        );
    }

    fn constructor(
        &mut self,
        scope: ScopeId,
        class: SymbolId,
        kind: &'static str,
        certainty: &'static str,
        at: Location,
    ) {
        if let Some(allocator) = self.class_member(class, "__new__", 0) {
            if let Value::Callable(invocation) = allocator {
                // An allocator receives the class itself. Its result determines
                // whether an instance initializer runs and which one is selected.
                self.invoke(
                    scope,
                    invocation.bind_receiver(Receiver::Explicit(None)),
                    kind,
                    certainty,
                    at,
                );
            }
            if self.allocation_class(class, 0).is_none() {
                self.scope_issue(
                    scope,
                    at,
                    "unresolved_constructor",
                    "allocation result is outside the bounded class model",
                );
            }
            return;
        }

        match self.class_member(class, "__init__", 0) {
            None => {}
            Some(Value::Callable(invocation)) => self.invoke(
                scope,
                invocation.bind_receiver(Receiver::Bound(class)),
                kind,
                certainty,
                at,
            ),
            _ => self.scope_issue(
                scope,
                at,
                "unresolved_constructor",
                "constructor dispatch is outside the bounded class hierarchy",
            ),
        }
    }

    fn allocation_class(&mut self, class: SymbolId, depth: usize) -> Option<SymbolId> {
        let Some(allocator) = self.class_member(class, "__new__", depth + 1) else {
            return Some(class);
        };
        let Value::Callable(invocation) = allocator else {
            return None;
        };
        let Binding::Definition(definition, body) = self.symbols[invocation.target].binding else {
            return None;
        };
        let [
            Statement {
                kind: StatementKind::Return(Some(value)),
                ..
            },
        ] = definition.body.as_slice()
        else {
            return None;
        };
        if definition.kind != DefinitionKind::Function || !definition.decorators.is_empty() {
            return None;
        }
        match self.identity(body, value, END, depth + 2) {
            Value::Instance(actual) => Some(actual),
            _ => None,
        }
    }

    fn verification_member(
        &mut self,
        scope: ScopeId,
        args: &[Expression],
        keywords: &[(String, Expression)],
        before: Location,
        depth: usize,
    ) -> Option<(Value, Location)> {
        let [receiver, member] = args else {
            return None;
        };
        let ExpressionKind::Text(name) = &member.kind else {
            return None;
        };
        if !keywords.is_empty() {
            return None;
        }
        let receiver = self.identity(scope, receiver, before, depth + 1);
        let known_receiver = matches!(receiver, Value::Module(_) | Value::Instance(_))
            || matches!(receiver, Value::Callable(invocation) if self.is_class(invocation.target));
        if !known_receiver {
            return None;
        }
        let value = self.member(receiver, name, (scope, before), depth + 1);
        let value = match value {
            Value::Unknown => return None,
            Value::Callable(invocation) => Value::Callable(Invocation {
                verified: true,
                ..invocation
            }),
            value => value,
        };
        Some((value, member.at))
    }

    fn walk(&mut self, scope: ScopeId, expr: &Expression, before: Location) {
        self.walk_expression(scope, expr, before, false);
    }

    fn is_module_getattr_parameter(
        &self,
        scope: ScopeId,
        expression: &Expression,
        before: Location,
    ) -> bool {
        let ExpressionKind::Name(name) = &expression.kind else {
            return false;
        };
        let Some(parameter) = self.lexical(scope, name, before) else {
            return false;
        };
        let Some(owner) = self.scopes[scope].owner else {
            return false;
        };
        matches!(self.symbols[parameter].binding, Binding::Parameter)
            && self.symbols[parameter].scope == scope
            && self.symbols[owner].name.text == "__getattr__"
            && self.scopes[self.symbols[owner].scope].owner.is_none()
    }

    fn has_literal_mapping_keys(value: &Value) -> bool {
        let Value::Mapping(items) = value else {
            return false;
        };
        items
            .as_chunks::<2>()
            .0
            .iter()
            .all(|pair| pair[0].is_builtin_scalar())
    }

    fn walk_if_test(
        &mut self,
        scope: ScopeId,
        test: &Expression,
        before: Location,
    ) -> Option<bool> {
        if self.logging_guard_uses_initialized_os(scope, test)
            && let ExpressionKind::Call { arguments, .. } = &test.kind
        {
            for argument in arguments {
                self.walk(scope, argument, before);
            }
            return Some(false);
        }
        match &test.kind {
            ExpressionKind::Not(operand) => self
                .walk_if_test(scope, operand, before)
                .map(|value| !value),
            ExpressionKind::Boolean { and, values } => {
                let mut known = true;
                for operand in values {
                    match self.walk_if_test(scope, operand, before) {
                        Some(value) if value != *and => return Some(value),
                        Some(_) => {}
                        None => known = false,
                    }
                }
                known.then_some(*and)
            }
            _ => {
                self.walk(scope, test, before);
                let value = self.identity(scope, test, before, 0);
                self.record_truth_test(scope, test, &value);
                match value {
                    Value::Bool(value) => Some(value),
                    _ => None,
                }
            }
        }
    }

    fn record_truth_test(&mut self, scope: ScopeId, test: &Expression, value: &Value) {
        match value {
            Value::Alternatives(values) => {
                for value in values {
                    self.record_truth_test(scope, test, value);
                }
                return;
            }
            Value::Instance(class) => {
                // Fall back to length only after proving that __bool__ is absent.
                let Some(hook) = self
                    .class_member(*class, "__bool__", 0)
                    .or_else(|| self.class_member(*class, "__len__", 0))
                else {
                    return;
                };
                if self.invoke_instance_hook(scope, *class, hook, "truth_test", test.at) {
                    return;
                }
            }
            // Comparison expressions check their own dispatch during the walk.
            Value::Unknown | Value::OperatorResult | Value::OpaqueNamespace
                if (self.has_bool_hooks || self.has_length_hooks)
                    && !matches!(
                        test.kind,
                        ExpressionKind::Compare { .. } | ExpressionKind::OrderingComparison { .. }
                    ) => {}
            _ => return,
        }
        self.scope_issue(
            scope,
            test.at,
            "unresolved_truth_test",
            "truth-value method dispatch is outside the bounded model",
        );
    }

    fn record_length_call(&mut self, scope: ScopeId, value: Value, at: Location) {
        match value {
            Value::Alternatives(values) => {
                for value in values {
                    self.record_length_call(scope, value, at);
                }
                return;
            }
            Value::Instance(class) => {
                if let Some(hook) = self.class_member(class, "__len__", 0)
                    && self.invoke_instance_hook(scope, class, hook, "length_call", at)
                {
                    return;
                }
            }
            Value::Unknown | Value::OperatorResult | Value::OpaqueNamespace
                if self.has_length_hooks => {}
            _ => return,
        }
        self.scope_issue(
            scope,
            at,
            "unresolved_length_call",
            "len() dispatch is outside the bounded model",
        );
    }

    fn invoke_instance_hook(
        &mut self,
        scope: ScopeId,
        class: SymbolId,
        hook: Value,
        kind: &'static str,
        at: Location,
    ) -> bool {
        let Value::Callable(invocation) = hook else {
            return false;
        };
        if invocation.verified
            || !matches!(self.symbols[invocation.target].binding, Binding::Definition(definition, _)
                if definition.kind == DefinitionKind::Function && definition.decorators.is_empty())
        {
            return false;
        }
        let invocation = if self.receiver_parameter(invocation.target).is_some() {
            invocation.bind_receiver(Receiver::Bound(class))
        } else {
            invocation
        };
        self.invoke(scope, invocation, kind, "certain", at);
        true
    }

    fn in_function(&self, scope: ScopeId) -> bool {
        self.scopes[scope]
            .owner
            .is_some_and(|owner| !self.is_class(owner))
    }

    fn record_formatting(&mut self, scope: ScopeId, value: Value, at: Location, use_format: bool) {
        if value.is_builtin_scalar() {
            return;
        }
        match value {
            Value::Alternatives(values) => {
                for value in values {
                    self.record_formatting(scope, value, at, use_format);
                }
                return;
            }
            Value::Instance(class) => {
                // Object's default formatter uses str(value), which can fall back to repr.
                let hooks = [("__format__", 2), ("__str__", 1), ("__repr__", 1)];
                let hook =
                    hooks[usize::from(!use_format)..]
                        .iter()
                        .copied()
                        .find_map(|(name, arity)| {
                            self.class_member_at(class, name, 0, Some((scope, at)))
                                .map(|value| (value, arity))
                        });
                match hook {
                    None => return,
                    Some((Value::Callable(invocation), arity))
                        if matches!(self.symbols[invocation.target].binding, Binding::Definition(definition, _)
                            if definition.simple_positional_parameters && definition.parameters.len() == arity)
                            && self.invoke_instance_hook(
                                scope,
                                class,
                                Value::Callable(invocation),
                                "formatting_hook",
                                at,
                            ) =>
                    {
                        return;
                    }
                    _ => {}
                }
            }
            _ if !use_format || !self.has_format_hooks => return,
            _ => {}
        }
        let message = if use_format {
            "formatted string dispatch is outside the bounded model"
        } else {
            "str() dispatch is outside the bounded model"
        };
        self.scope_issue(scope, at, "unresolved_formatting", message);
    }

    fn namespace_is_nonempty(&self, owner: ScopeId, first: [usize; 2]) -> bool {
        let at = Location {
            line: first[0],
            column: first[1],
        };
        self.scopes[owner]
            .namespace_reads
            .get(&first)
            .is_some_and(|read| {
                read.bindings.iter().any(|&id| {
                    self.symbols[id].scope == owner && !self.binding_depends_on_branch(id, at)
                })
            })
    }

    fn has_namespace_reads(&self, mut scope: ScopeId) -> bool {
        loop {
            let row = &self.scopes[scope];
            if !row.namespace_reads.is_empty() {
                return true;
            }
            let Some(parent) = row.parent else {
                return false;
            };
            scope = parent;
        }
    }

    fn is_namespace_builtin(&self, name: &str) -> bool {
        matches!(name, "locals" | "vars")
            || self.phase03 && matches!(name, "globals" | "eval" | "exec")
    }

    fn local_namespace_bindings(&self, scope: ScopeId, before: Location) -> Vec<SymbolId> {
        let row = &self.scopes[scope];
        let visible = |id: SymbolId| {
            let symbol = &self.symbols[id];
            !matches!(symbol.binding, Binding::Variable(None, _))
                && !row
                    .namespace_exclusions
                    .iter()
                    .any(|span| span.contains(&position(symbol.name.at)))
        };
        let mut bindings = Vec::new();
        for names in row.names.values() {
            let end =
                names.partition_point(|&id| position(self.symbols[id].name.at) < position(before));
            // Walk the possible assignments back to the last definite value.
            for id in names[..end].iter().rev().copied().filter(|&id| visible(id)) {
                bindings.push(id);
                if !self.binding_depends_on_branch(id, before) {
                    break;
                }
            }
        }
        bindings.extend(
            row.namespace_captures
                .get_or_init(|| self.namespace_captures(scope)),
        );
        bindings
    }

    fn read_local_namespace(&mut self, scope: ScopeId, before: Location, at: Location) {
        let bindings = self.local_namespace_bindings(scope, before);
        self.read.extend(bindings.iter().copied());
        let row = &mut self.scopes[scope];
        row.namespace_reads.insert(
            position(at),
            NamespaceRead {
                bindings,
                exclusions: row.namespace_exclusions.clone(),
            },
        );
    }

    fn namespace_captures(&self, scope: ScopeId) -> Vec<SymbolId> {
        let owner = self.scopes[scope]
            .owner
            .expect("namespace belongs to a function");
        let Binding::Definition(definition, _) = self.symbols[owner].binding else {
            unreachable!("function scope has a definition");
        };
        let postponed = self.files[self.scopes[scope].file]
            .facts
            .postponed_annotations;
        let free_names = namespace_free_names(definition, postponed);
        let mut captures = Vec::new();
        for name in free_names {
            let mut parent = self.scopes[scope].parent;
            while let Some(outer) = parent {
                let row = &self.scopes[outer];
                if row.owner.is_none() {
                    break;
                }
                if self.in_function(outer) {
                    if let Some(bindings) = row.names.get(name) {
                        // A closure can run before or after a later assignment.
                        captures.extend(bindings.iter().copied().filter(|&id| {
                            !matches!(self.symbols[id].binding, Binding::Variable(None, _))
                        }));
                        break;
                    }
                    if suite_binds_name(row.body, name) {
                        break;
                    }
                }
                parent = row.parent;
            }
        }
        captures
    }

    fn walk_expression(
        &mut self,
        scope: ScopeId,
        expr: &Expression,
        before: Location,
        as_callee: bool,
    ) {
        match &expr.kind {
            ExpressionKind::ForwardingCall {
                function,
                positional,
                keywords,
            } => {
                self.walk_expression(scope, function, before, true);
                self.walk(scope, positional, before);
                self.walk(scope, keywords, before);
                if self.phase03 {
                    self.scope_issue(
                        scope,
                        expr.at,
                        "unresolved_call",
                        "variadic forwarding is outside the bounded callable model",
                    );
                } else {
                    self.issue(
                        self.scopes[scope].file,
                        expr.at,
                        "unsupported_construct",
                        "expression is outside the compatibility subset",
                    );
                }
            }
            ExpressionKind::RepeatedKeywordCall {
                function,
                arguments,
            } => {
                self.walk_expression(scope, function, before, true);
                for argument in arguments {
                    self.walk(scope, argument, before);
                }
                self.scope_issue(
                    scope,
                    expr.at,
                    "unsupported_construct",
                    "keyword expansion with a duplicate argument is outside the bounded call model",
                );
            }
            ExpressionKind::KeywordMappingCall {
                function,
                arguments,
                mapping,
            } => {
                self.walk_expression(scope, function, before, true);
                for argument in arguments {
                    self.walk(scope, argument, before);
                }
                self.walk(scope, mapping, before);
                if self.phase03
                    && arguments.is_empty()
                    && let Value::Method(object, ObjectKind::CeleryApp, method) =
                        self.call_target_identity(scope, function, before, 0)
                    && method == "conf.update"
                {
                    self.record_celery_expanded_config(object, scope, mapping, before, expr.at);
                } else {
                    self.scope_issue(
                        scope,
                        expr.at,
                        "unsupported_construct",
                        "unbounded keyword expansion",
                    );
                }
            }
            ExpressionKind::Call {
                function,
                arguments,
                keywords,
            } => {
                if self.phase03
                    && self.walk_literal_text_call(
                        function,
                        CallArguments {
                            scope,
                            before,
                            at: expr.at,
                            positional: arguments,
                            keywords,
                        },
                    )
                {
                    return;
                }
                if self.phase03
                    && arguments.is_empty()
                    && keywords.is_empty()
                    && let ExpressionKind::DeferredLambda(body) = &function.kind
                    && self.fastapi_immediate_route_lambda(scope, body, before)
                {
                    return;
                }
                let rebound_converter =
                    if self.phase03 && arguments.len() == 2 && keywords.is_empty() {
                        self.django_converter_noop_rebinding(scope, function, expr.at, before)
                    } else {
                        None
                    };
                if rebound_converter.is_none() {
                    self.walk_expression(scope, function, before, true);
                }
                for expr in arguments
                    .iter()
                    .chain(keywords.iter().map(|(_, value)| value))
                {
                    self.walk(scope, expr, before);
                    if let ExpressionKind::Comprehension { form, .. } = expr.kind {
                        let value = if matches!(form, ComprehensionForm::Generator) {
                            Value::DeferredComprehension
                        } else {
                            Value::ComprehensionResult
                        };
                        self.report_unmodeled_deferred_value(scope, expr.at, &value);
                    } else if matches!(
                        expr.kind,
                        ExpressionKind::DeferredLambda(_) | ExpressionKind::DeferredBoolLambda
                    ) {
                        self.report_unmodeled_deferred_value(
                            scope,
                            expr.at,
                            &Value::DeferredLambda,
                        );
                    }
                }
                if let Some(invocation) = rebound_converter {
                    self.direct.insert(invocation.target);
                    self.invoke(scope, invocation, "direct_call", "certain", expr.at);
                    return;
                }
                let callee = self.call_target_identity(scope, function, before, 0);
                if self.phase03
                    && arguments.is_empty()
                    && keywords.is_empty()
                    && self.fastapi_class_factory_allocation(scope, function)
                {
                    return;
                }
                match callee {
                    Value::Provider(name)
                        if self.empty_flask_context_pop(&name, arguments, keywords) => {}
                    Value::ConstantFunction => {}
                    Value::NullaryConstantFunction
                        if arguments.is_empty() && keywords.is_empty() => {}
                    Value::DeferredLambda => {
                        self.report_unmodeled_deferred_value(scope, expr.at, &callee)
                    }
                    Value::IdentityFunction if arguments.len() == 1 && keywords.is_empty() => {}
                    Value::DjangoFormValidation(Some(class)) => self.record_django_form_validation(
                        class,
                        function,
                        CallArguments {
                            scope, before, at: expr.at, positional: arguments, keywords,
                        },
                    ),
                    Value::Provider(name) if self.phase03
                        && self.wagtail_field_constructor(
                            CallArguments {
                                scope, before, at: expr.at, positional: arguments, keywords,
                            },
                            &name,
                        ) => {}
                    Value::Provider(name) if self.phase03 && django_forms::is_field_provider(&name) => {
                        if self.django_field_value(
                            &name,
                            CallArguments {
                                scope, before, at: expr.at, positional: arguments, keywords,
                            },
                        ).is_none() {
                            self.scope_issue(
                                scope,
                                expr.at,
                                "unresolved_constructor",
                                "Django field options are outside the bounded model",
                            );
                        }
                    }
                    Value::PydanticDump(class) => self.record_pydantic_dump(
                        class,
                        scope,
                        expr.at,
                        arguments.is_empty() && keywords.is_empty(),
                    ),
                    Value::UnittestComparison => {
                        if !self.bounded_unittest_comparison(scope, arguments, keywords, before) {
                            self.scope_issue(
                                scope,
                                expr.at,
                                "unresolved_comparison",
                                "unittest equality operands are outside the bounded comparison model",
                            );
                        }
                    }
                    Value::Provider(name)
                        if self.phase03
                            && matches!(name.as_str(), "builtins.setattr" | "builtins.delattr")
                            && self.modeled_framework_write(scope, before) => {}
                    Value::Callable(invocation) => {
                        let call = CallArguments {
                            scope, before, at: expr.at, positional: arguments, keywords,
                        };
                        if self.phase03 && let Some(Value::FastapiSubclassApp(object, _)) =
                            self.fastapi_subclass_app(invocation, call, 0)
                        {
                            self.fastapi_app(object, scope);
                        }
                        if self.phase03
                            && arguments.is_empty()
                            && keywords.is_empty()
                            && let Some(Value::Object(object, ObjectKind::FlaskApp)) =
                                self.flask_factory_result(invocation, scope, expr.at, 0)
                        {
                            self.scopes[scope].activated_apps.push(object);
                        }
                        if !self.phase03
                            || !self.invoke_fresh_field_method(invocation, function, call)
                        {
                            self.call_invocation(invocation, call, "certain");
                        }
                    }
                    Value::Alternatives(values)
                        if self.phase03 && !values.is_empty() && values.iter().all(|value| {
                            matches!(value, Value::Method(_, ObjectKind::App, name) if name == "add_exception_handler")
                        }) =>
                    {
                        let objects: BTreeSet<_> = values.into_iter().filter_map(|value| {
                            match value {
                                Value::Method(object, ..) => Some(object),
                                _ => None,
                            }
                        }).collect();
                        self.fastapi_exception_handlers(objects, CallArguments {
                            scope, before, at: expr.at, positional: arguments, keywords,
                        }, true);
                    }
                    Value::Alternatives(values) => {
                        let mut resolved = false;
                        let mut unresolved = false;
                        for value in values {
                            if let Value::Callable(invocation) = value {
                                resolved = true;
                                self.call_invocation(
                                    invocation,
                                    CallArguments {
                                        scope,
                                        before,
                                        at: expr.at,
                                        positional: arguments,
                                        keywords,
                                    },
                                    "possible",
                                );
                            } else {
                                unresolved = true;
                            }
                        }
                        if !resolved || unresolved {
                            self.scope_issue(
                                scope,
                                expr.at,
                                "unresolved_call",
                                "one or more finite call targets are outside the bounded model",
                            );
                        }
                    }
                    Value::ReceiverMethod(parameter, member) => {
                        let bounded_comparison =
                            matches!(member.as_str(), "assertEqual" | "assertNotEqual")
                                && self.bounded_unittest_comparison(
                                    scope, arguments, keywords, before,
                                );
                        self.receiver_calls.push(ReceiverCall {
                            scope,
                            parameter,
                            member,
                            at: expr.at,
                            bounded_comparison,
                        });
                    }
                    Value::Provider(name)
                        if matches!(name.as_str(), "builtins.globals" | "builtins.vars")
                            && matches!(self.identity(scope, expr, before, 0), Value::CommandNamespace(_)) => {}
                    Value::CommandNamespaceGet(_)
                        if !matches!(self.identity(scope, expr, before, 0), Value::Unknown) => {}
                    Value::Provider(name)
                        if matches!(name.as_str(), "builtins.locals" | "builtins.vars") =>
                    {
                        if arguments.is_empty() && keywords.is_empty() && self.in_function(scope) {
                            self.read_local_namespace(scope, before, expr.at);
                        } else {
                            self.scope_issue(
                                scope,
                                expr.at,
                                "unresolved_call",
                                "only zero-argument namespace reads in functions are bounded",
                            );
                        }
                    }
                    Value::Provider(name)
                        if matches!(name.as_str(), "builtins.eval" | "builtins.exec") =>
                    {
                        if self.phase03
                            && arguments.len() == 1
                            && keywords.is_empty()
                            && self.in_function(scope)
                        {
                            // An unread-local finding concerns the function's own
                            // body, even if no project root reaches that body.
                            let bindings = self.local_namespace_bindings(scope, before);
                            self.read.extend(bindings);
                        }
                        self.scope_issue(
                            scope,
                            expr.at,
                            "unresolved_call",
                            format!("{name} can execute code outside the bounded model"),
                        );
                    }
                    Value::Provider(name) if self.phase03 && name == "builtins.str" => {
                        if let [argument] = arguments.as_slice()
                            && keywords.is_empty()
                        {
                            let value = self.identity(scope, argument, before, 0);
                            self.record_formatting(scope, value, expr.at, false);
                        }
                    }
                    Value::Provider(name) if name == "builtins.len" => {
                        if let [argument] = arguments.as_slice()
                            && keywords.is_empty()
                        {
                            let value = self.identity(scope, argument, before, 0);
                            self.record_length_call(scope, value, expr.at);
                        }
                    }
                    Value::Provider(name) if name == "asyncio.create_task" => {
                        self.activate_argument(
                            scope,
                            arguments.first(),
                            before,
                            "scheduled_coroutine",
                            expr.at,
                        );
                    }
                    Value::Provider(name)
                        if self.phase03
                            && name == "pluggy.HookimplMarker"
                            && matches!(
                                self.identity(scope, expr, before, 0),
                                Value::PluggyHookMarker
                            ) => {}
                    Value::Provider(name)
                        if self.phase03
                            && name == "types.SimpleNamespace"
                            && arguments.is_empty() =>
                    {
                        for (_, argument) in keywords {
                            let value = self.identity(scope, argument, before, 0);
                            self.expose(scope, value, "passed_callable", argument.at);
                        }
                    }
                    Value::Provider(name)
                        if self.phase03
                            && is_declarative_base_factory(&name)
                            && arguments.is_empty()
                            && keywords.is_empty() => {}
                    Value::Provider(name) if self.phase03 && name == "atexit.register" => {
                        self.register_atexit(CallArguments {
                            scope,
                            before,
                            at: expr.at,
                            positional: arguments,
                            keywords,
                        });
                    }
                    Value::Submit => {
                        self.activate_submission(scope, before, expr.at, arguments, keywords);
                    }

                    Value::Start(value) => match *value {
                        Value::Callable(_) => {
                            self.activate_callback(scope, *value, "process_target", expr.at);
                        }
                        _ => self.scope_issue(
                            scope,
                            expr.at,
                            "unresolved_callback",
                            "process target could not be bounded",
                        ),
                    },
                    Value::Method(parent, ObjectKind::App | ObjectKind::Router, method)
                        if method == "include_router" =>
                    {
                        let target = arguments.first().or_else(|| {
                            keywords
                                .iter()
                                .find(|(name, _)| name == "router")
                                .map(|(_, value)| value)
                        });
                        if let Some(target) = target
                            && let Value::Object(child, ObjectKind::Router) =
                                self.identity(scope, target, before, 0)
                        {
                            if self.phase03 {
                                self.fastapi_mount(
                                    parent,
                                    child,
                                    fastapi::Site {
                                        scope,
                                        at: expr.at,
                                        before,
                                    },
                                    arguments,
                                    keywords,
                                );
                            } else {
                                self.scopes[scope].mounts.push((parent, child));
                            }
                        } else {
                            self.scope_issue(
                                scope,
                                expr.at,
                                "unresolved_registration",
                                "router identity could not be resolved",
                            );
                        }
                    }
                    Value::Method(
                        parent,
                        ObjectKind::FlaskApp | ObjectKind::FlaskBlueprint,
                        method,
                    ) if self.phase03 && method == "register_blueprint" => {
                        let target = arguments.first().or_else(|| {
                            keywords
                                .iter()
                                .find(|(name, _)| name == "blueprint")
                                .map(|(_, value)| value)
                        });
                        if let Some(target) = target
                            && let Value::Object(child, ObjectKind::FlaskBlueprint) =
                                self.identity(scope, target, before, 0)
                        {
                            self.scopes[scope].mounts.push((parent, child));
                        } else {
                            self.scope_issue(
                                scope,
                                expr.at,
                                "unresolved_registration",
                                "Flask blueprint identity could not be resolved",
                            );
                        }
                    }
                    Value::Method(parent, ObjectKind::CeleryApp, method)
                        if self.phase03 && method == "register_task" =>
                    {
                        let value = registration_arguments(arguments, keywords, ["task"])
                            .and_then(|[task]| task)
                            .map_or(Value::Unknown, |task| self.identity(scope, task, before, 0));
                        self.register_celery_task(scope, parent, value, expr.at);
                    }
                    Value::Method(object, ObjectKind::SanicApp, method) if method == "register_listener" => {
                        self.register_sanic_listener(object, CallArguments {
                            scope, before, at: expr.at, positional: arguments, keywords,
                        });
                    }
                    Value::Method(object, ObjectKind::SanicApp, method)
                        if method == "run" && arguments.is_empty() && keywords.is_empty() =>
                    {
                        self.scopes[scope].activated_apps.push(object);
                    }
                    Value::Method(object, ObjectKind::StarletteApp, method) if method == "add_route" => {
                        self.register_starlette_route(object, CallArguments {
                            scope, before, at: expr.at, positional: arguments, keywords,
                        });
                    }
                    Value::Method(object, ObjectKind::CeleryApp, method)
                        if self.phase03
                            && matches!(method.as_str(), "conf.update" | "config_from_object") =>
                    {
                        self.record_celery_backend_config(
                            object,
                            &method,
                            CallArguments {
                                scope,
                                before,
                                at: expr.at,
                                positional: arguments,
                                keywords,
                            },
                        );
                    }
                    Value::Method(object, ObjectKind::CeleryApp, method)
                        if self.phase03 && method == "send_task" =>
                    {
                        if keywords.is_empty() && matches!(arguments.as_slice(), [task] if matches!(task.kind, ExpressionKind::Text(_))) {
                            self.record_celery_route_load(object, scope, expr.at);
                        }
                        self.scope_issue(scope, expr.at, "unresolved_call", "Celery task submission is outside the bounded model");
                    }
                    Value::Method(parent, ObjectKind::App, method)
                        if self.phase03 && method == "mount" =>
                    {
                        self.fastapi_asgi_mount(
                            parent,
                            CallArguments {
                                scope,
                                before,
                                at: expr.at,
                                positional: arguments,
                                keywords,
                            },
                        );
                    }
                    Value::Method(parent, kind, method)
                        if self.phase03 && is_imperative_registration(kind, &method) =>
                    {
                        if method == "add_exception_handler" {
                            self.fastapi_exception_handlers([parent], CallArguments {
                                scope, before, at: expr.at, positional: arguments, keywords,
                            }, false);
                            return;
                        }
                        let target = callback_argument(kind, &method, arguments, keywords);
                        let value = target.map(|target| self.identity(scope, target, before, 0));
                        match value {
                            Some(Value::Callable(invocation)) => {
                                if matches!(kind, ObjectKind::App | ObjectKind::Router) {
                                    self.fastapi_route(
                                        parent,
                                        invocation.target,
                                        CallArguments {
                                            scope,
                                            before,
                                            at: expr.at,
                                            positional: arguments,
                                            keywords,
                                        },
                                        &method,
                                    );
                                } else {
                                    let conditional_teardown = self.branch_depth != 0
                                        && kind == ObjectKind::FlaskApp
                                        && method == "teardown_appcontext";
                                    let routes = if conditional_teardown {
                                        &mut self.scopes[scope].optional_flask_routes
                                    } else {
                                        &mut self.scopes[scope].routes
                                    };
                                    routes.push((
                                        parent,
                                        invocation.target,
                                        framework_registration_kind(kind),
                                    ));
                                    if let Some(class) = invocation.receiver.supplied() {
                                        self.framework_route_receivers
                                            .entry((scope, parent, invocation.target))
                                            .or_default()
                                            .insert(class);
                                    }
                                }
                            }
                            _ => self.scope_issue(
                                scope,
                                expr.at,
                                "unresolved_registration",
                                "framework callback identity could not be resolved",
                            ),
                        }
                    }
                    Value::Method(parent, ObjectKind::App, method)
                        if self.phase03 && method == "add_middleware" =>
                    {
                        self.fastapi_middleware(
                            parent,
                            fastapi::Site {
                                scope,
                                at: expr.at,
                                before,
                            },
                            arguments,
                            keywords,
                        );
                    }
                    Value::DjangoSignalConnect => {
                        self.connect_django_signal(scope, arguments, keywords, before, expr.at);
                    }
                    Value::Provider(name)
                        if self.phase03
                            && name.strip_suffix(".connect").is_some_and(is_django_signal) =>
                    {
                        self.connect_django_signal(scope, arguments, keywords, before, expr.at);
                    }
                    Value::DjangoReceiver(signal) => {
                        let callback = registration_arguments(arguments, keywords, ["func"])
                            .and_then(|[callback]| callback)
                            .map(|target| self.identity(scope, target, before, 0));
                        self.record_django_signal(scope, signal, callback, expr.at);
                    }
                    Value::Provider(name)
                        if self.phase03 && name == "django.utils.autoreload.run_with_reloader" =>
                    {
                        self.register_django_autoreload(CallArguments {
                            scope,
                            before,
                            at: expr.at,
                            positional: arguments,
                            keywords,
                        });
                    }
                    Value::Provider(name) if self.phase03 && name == "sqlalchemy.event.listen" => {
                        let callback = arguments.get(2).or_else(|| {
                            keywords
                                .iter()
                                .find(|(name, _)| name == "fn")
                                .map(|(_, value)| value)
                        });
                        self.activate_argument(
                            scope,
                            callback,
                            before,
                            "sqlalchemy_listener",
                            expr.at,
                        );
                    }
                    Value::Provider(name)
                        if self.phase03 && name == "django.contrib.admin.site.register" =>
                    {
                        self.django_admin_site_register(
                            scope, arguments, keywords, before, expr.at,
                        );
                    }
                    Value::Provider(name)
                        if self.phase03 && name == "sqlalchemy.event.listens_for.callback" =>
                    {
                        self.activate_argument(
                            scope,
                            arguments.first(),
                            before,
                            "sqlalchemy_listener",
                            expr.at,
                        );
                    }
                    Value::Provider(name)
                        if self.phase03
                            && name == "os.environ.get"
                            && keywords.is_empty()
                            && matches!(arguments.as_slice(), [key] | [key, _]
                                if matches!(key.kind, ExpressionKind::Text(_))) =>
                    {
                        // The lookup has no source-backed callback. Keep its
                        // result unknown and analyze both reachable branches.
                    }
                    Value::Provider(name) if self.phase03 && name == "os.environ.setdefault" => {
                        if let [key, settings] = arguments.as_slice()
                            && keywords.is_empty()
                            && matches!(self.identity(scope, key, before, 0),
                                Value::Text(key) if key == "DJANGO_SETTINGS_MODULE")
                        {
                            if self.scopes[scope].django_settings == DjangoSettings::Unset {
                                self.scopes[scope].django_settings =
                                    match self.identity(scope, settings, before, 0) {
                                        Value::Text(settings) => DjangoSettings::Module(settings),
                                        _ => DjangoSettings::Unknown,
                                    };
                            }
                        } else {
                            self.scope_issue(
                                scope,
                                expr.at,
                                "unresolved_call",
                                "environment update is outside the bounded Django startup model",
                            );
                        }
                    }
                    Value::DjangoViewFactory(_) if arguments.is_empty() && keywords.is_empty() => {}
                    Value::Provider(name) if self.phase03 && name == "django.urls.include" => {
                        self.record_failed_django_include(CallArguments {
                            scope, before, at: expr.at, positional: arguments, keywords,
                        });
                    }
                    Value::Provider(name) if self.phase03 && name == "django.core.management.call_command" => {
                        self.register_django_programmatic_command(CallArguments {
                            scope, before, at: expr.at, positional: arguments, keywords,
                        });
                    }
                    Value::Provider(name)
                        if self.phase03
                            && matches!(
                                name.as_str(),
                                "django.setup"
                                    | "django.core.wsgi.get_wsgi_application"
                                    | "django.core.asgi.get_asgi_application"
                                    | "django.core.management.execute_from_command_line"
                            ) =>
                    {
                        let mut settings = match &self.scopes[scope].django_settings {
                            DjangoSettings::Module(module) => Some(module.clone()),
                            DjangoSettings::Unset | DjangoSettings::Unknown => None,
                        };
                        let commands = if name == "django.core.management.execute_from_command_line"
                        {
                            let argv = arguments.first().or_else(|| {
                                keywords
                                    .iter()
                                    .find(|(name, _)| name == "argv")
                                    .map(|(_, value)| value)
                            });
                            match argv.map(|value| self.identity(scope, value, before, 0)) {
                                Some(Value::Sequence(values)) => {
                                    match django_management_settings(&values) {
                                        DjangoSettings::Unset => {}
                                        DjangoSettings::Module(module) => settings = Some(module),
                                        DjangoSettings::Unknown => settings = None,
                                    }
                                    frameworks::DjangoCommands::from_argv(&values)
                                }
                                _ => frameworks::DjangoCommands::Available,
                            }
                        } else {
                            frameworks::DjangoCommands::None
                        };
                        self.scopes[scope]
                            .starts_django
                            .push(frameworks::DjangoStartup {
                                at: expr.at,
                                settings,
                                creates_application: matches!(
                                    name.as_str(),
                                    "django.core.wsgi.get_wsgi_application"
                                        | "django.core.asgi.get_asgi_application"
                                ),
                                definite_setup: self.branch_depth == 0
                                    && arguments.is_empty() && keywords.is_empty()
                                    && name != "django.core.management.execute_from_command_line",
                                commands,
                            });
                    }
                    Value::Provider(name)
                        if self.phase03
                            && matches!(
                                name.as_str(),
                                "fastapi.FastAPI" | "fastapi.applications.FastAPI"
                            ) =>
                    {
                        let Value::Object(object, ObjectKind::App) =
                            self.identity(scope, expr, before, 0)
                        else {
                            unreachable!()
                        };
                        let site = fastapi::Site {
                            scope,
                            at: expr.at,
                            before,
                        };
                        self.record_fastapi_query_owner(object, site, arguments, keywords, false);
                        let custom_lifespan = keywords
                            .iter()
                            .find(|(name, _)| name == "lifespan")
                            .is_some_and(|(_, value)| self.fastapi_lifespan(object, site, value));
                        self.fastapi_defaults(object, site, keywords);
                        for (keyword, value) in keywords {
                            if keyword == "middleware"
                                && !matches!(value.kind, ExpressionKind::None)
                            {
                                self.fastapi_middleware_list(object, site, value);
                            }
                            if keyword == "exception_handlers"
                                || !custom_lifespan
                                    && matches!(keyword.as_str(), "on_startup" | "on_shutdown")
                            {
                                let value = self.identity(scope, value, before, 0);
                                let kind = if keyword == "exception_handlers" {
                                    "fastapi_exception_handler"
                                } else {
                                    "fastapi_lifecycle"
                                };
                                if !self.activate_framework_value(scope, value, kind) {
                                    self.scope_issue(
                                        scope,
                                        expr.at,
                                        "unresolved_registration",
                                        "FastAPI lifecycle callback could not be resolved",
                                    );
                                }
                            }
                        }
                        self.fastapi_app(object, scope);
                    }
                    Value::Provider(name)
                        if self.phase03
                            && matches!(
                                name.as_str(),
                                "fastapi.APIRouter" | "fastapi.routing.APIRouter"
                            ) =>
                    {
                        if let Value::Object(object, ObjectKind::Router) =
                            self.identity(scope, expr, before, 0)
                        {
                            let site = fastapi::Site {
                                scope,
                                at: expr.at,
                                before,
                            };
                            self.record_fastapi_query_owner(object, site, arguments, keywords, true);
                            if let Some((_, lifespan)) =
                                keywords.iter().find(|(name, _)| name == "lifespan")
                            {
                                self.fastapi_lifespan(object, site, lifespan);
                            }
                            for (name, callbacks) in keywords {
                                if matches!(name.as_str(), "on_startup" | "on_shutdown") {
                                    self.fastapi_router_lifecycle(object, site, callbacks);
                                }
                            }
                            self.fastapi_defaults(object, site, keywords);
                        }
                    }
                    Value::Provider(name)
                        if self.phase03
                            && matches!(
                                name.as_str(),
                                "django.urls.register_converter"
                                    | "django.urls.converters.register_converter"
                            ) =>
                    {
                        self.record_django_converter(CallArguments {
                            scope,
                            before,
                            at: expr.at,
                            positional: arguments,
                            keywords,
                        });
                    }
                    Value::Provider(name) if self.phase03 && is_django_runpython(&name) => {
                        self.record_django_runpython(CallArguments {
                            scope,
                            before,
                            at: expr.at,
                            positional: arguments,
                            keywords,
                        });
                    }
                    Value::Provider(name) if self.grep_verify && name == "builtins.getattr" => {
                        match self.verification_member(scope, arguments, keywords, before, 0) {
                            Some((Value::Callable(invocation), at)) => {
                                // An immediately called function gets its edge and
                                // arguments from the enclosing call. A standalone
                                // lookup still supplies possible external use.
                                if !as_callee
                                    || !matches!(self.symbols[invocation.target].binding,
                                        Binding::Definition(def, _) if def.kind == DefinitionKind::Function)
                                {
                                    self.invoke(scope, invocation, "grep_dynamic", "possible", at);
                                }
                                let reference = (scope, invocation.target, at.line, at.column);
                                if self.dynamic_references.len() < grep_verification::MAX_EVIDENCE {
                                    self.dynamic_references.insert(reference);
                                } else if !self.dynamic_references.contains(&reference) {
                                    self.issues.push(grep_verification::evidence_limit());
                                }
                            }
                            Some(_) => {}
                            None => self.scope_issue(
                                scope,
                                expr.at,
                                "unresolved_call",
                                "dynamic member is outside the bounded verification model",
                            ),
                        }
                    }
                    Value::Provider(name) if self.phase03 && gin::is_config_loader(&name) => {
                        self.gin_config_call(CallArguments { scope, at: expr.at, before, positional: arguments, keywords });
                    }
                    Value::Provider(name) if self.phase03 && wagtail::is_snippet_registrar(&name)
                        && arguments.len() == 1
                        && matches!(keywords.as_slice(), [(key, _)] if key == "viewset") => {
                        self.register_wagtail_snippet(CallArguments {
                            scope, at: expr.at, before, positional: arguments, keywords,
                        });
                    }
                    Value::Provider(name) if self.phase03 && gin::is_stop_after_attempt(&name)
                        && gin::supported_stop_arguments(arguments, keywords) => {}
                    Value::Provider(name) if self.phase03 && traitlets::is_text_trait(&name)
                        && self.traitlets_text_declaration(CallArguments {
                            scope, before, at: expr.at, positional: arguments, keywords,
                        }) => {}
                    Value::Provider(name) if self.phase03 && sanic::is_constructor(&name) => {
                        if !matches!(self.identity(scope, expr, before, 0), Value::Object(_, ObjectKind::SanicApp)) {
                            self.scope_issue(scope, expr.at, "unresolved_constructor", "Sanic app requires a supported literal name");
                        }
                    }
                    Value::Provider(name) if self.phase03 && starlette::is_constructor(&name) => {
                        if !arguments.is_empty() || !keywords.is_empty() {
                            self.scope_issue(scope, expr.at, "unresolved_constructor", "Starlette constructor arguments are outside the bounded model");
                        }
                    }
                    Value::Provider(name) if self.phase03 && aiohttp::is_route_table(&name) => {
                        if !arguments.is_empty() || !keywords.is_empty() {
                            self.scope_issue(scope, expr.at, "unresolved_constructor", "aiohttp route table requires a zero-argument constructor");
                        }
                    }
                    Value::Provider(name) if self.phase03 && mcp_tools::is_server_provider(&name) => {
                        if !matches!(self.mcp_server(scope, expr.at, arguments, keywords), Value::McpServer(_)) {
                            self.scope_issue(scope, expr.at, "unresolved_constructor", "FastMCP server arguments are outside the bounded model");
                        }
                    }
                    Value::Provider(name) if self.phase03 && drf::is_method_field(&name) => {
                        if drf::method_field_name(arguments, keywords).is_none() {
                            self.scope_issue(scope, expr.at, "unresolved_constructor", "DRF method field requires an omitted or literal callback name");
                        }
                    }
                    Value::Provider(name) if self.phase03 && drf::is_field(&name) => {
                        if self.drf_field_writable(scope, before, arguments, keywords).is_none() {
                            self.scope_issue(scope, expr.at, "unresolved_constructor", "DRF serializer field arguments are outside the bounded model");
                        }
                    }
                    Value::Provider(name)
                        if self.phase03 && name == "pathlib.Path"
                            && self.source_project_path(scope, before, arguments, keywords).is_some() => {}
                    Value::ResolveProjectPath(_) if arguments.is_empty() && keywords.is_empty() => {}
                    Value::Provider(name) if self.phase03 && name == "json.dumps"
                        && arguments.len() == 1 && keywords.is_empty()
                        && self.defer_log_record_json(scope, &arguments[0], before, expr.at) => {}
                    Value::Provider(name) if self.phase03 && is_flask_login_constructor(&name)
                        && self.flask_constructor_changed(ObjectKind::FlaskLoginManager, (scope, before)) =>
                    {
                        if arguments.is_empty() && keywords.is_empty()
                            && let Some((allocator, _)) = self.flask_login_allocation(scope, before)
                        {
                            self.invoke(scope, allocator, "flask_login_allocator", "certain", expr.at);
                        } else {
                            self.scope_issue(
                                scope,
                                expr.at,
                                "unresolved_constructor",
                                "Flask-Login allocator cannot be resolved",
                            );
                        }
                    }
                    Value::Provider(name) if is_known_call(&name, self.phase03) => {
                        if self.phase03 && matches!(name.as_str(), "builtins.setattr" | "builtins.delattr")
                            && let Some(receiver) = arguments.first()
                        {
                            self.record_unmodeled_class_write(scope, receiver, before);
                            if self.bounded_declaration_provider_write(scope, receiver, before) {
                                self.scope_issue(
                                    scope,
                                    expr.at,
                                    "unresolved_attribute_write",
                                    "framework declaration provider mutations are outside the bounded model",
                                );
                            }
                        }
                        let row = &mut self.scopes[scope];
                        if name == "builtins.dict"
                            && let Some((&read, _)) = row.namespace_reads.last_key_value()
                            && read > position(expr.at)
                        {
                            // The call copies after all arguments have run,
                            // including any reads that refreshed its input.
                            row.namespace_copy_cutoffs.insert(position(expr.at), read);
                        }
                        if let Value::Object(object, kind) = self.identity(scope, expr, before, 0)
                            && matches!(
                                kind,
                                ObjectKind::App
                                    | ObjectKind::FlaskApp
                                    | ObjectKind::FlaskLoginManager
                                    | ObjectKind::TyperApp
                                    | ObjectKind::CeleryApp
                            )
                        {
                            self.scopes[scope].activated_apps.push(object);
                            if matches!(kind, ObjectKind::CeleryApp) {
                                self.record_celery_backend_creation(
                                    object,
                                    CallArguments {
                                        scope,
                                        before,
                                        at: expr.at,
                                        positional: arguments,
                                        keywords,
                                    },
                                );
                            }
                        }
                    }
                    _ => {
                        if self.phase03
                            && !self.probing_identity
                            && self.branch_depth == 0
                            && let ExpressionKind::Member(base, member) = &function.kind
                            && member == "run_with_reloader"
                            && let Some(parameter) = self.parameter_binding(scope, base, before)
                            && self.scopes[scope].parent.is_some_and(|parent| {
                                self.scopes[parent].owner.is_none_or(|owner| !self.is_class(owner))
                            })
                        {
                            self.receiver_calls.push(ReceiverCall {
                                scope,
                                parameter,
                                member: member.clone(),
                                at: expr.at,
                                bounded_comparison: false,
                            });
                            return;
                        }
                        if self.phase03
                            && !self.probing_identity
                            && arguments.is_empty()
                            && keywords.is_empty()
                            && let ExpressionKind::Member(base, name) = &function.kind
                            && name == "lower"
                            && let Some(parameter) = self.parameter_binding(scope, base, before)
                        {
                            self.django_converter.string_calls.push(ParameterCall {
                                scope,
                                parameter,
                                at: expr.at,
                            });
                            return;
                        }
                        if self.phase03
                            && self.record_flask_parameter_hook(
                                scope, function, arguments, keywords, before, expr.at,
                            )
                        {
                            return;
                        }
                        if self.phase03
                            && (self.defer_fresh_field_call(scope, expr)
                                || self.defer_source_provider_call(scope, expr))
                        {
                            return;
                        }
                        if self.phase03 && arguments.is_empty() && keywords.is_empty()
                            && self.defer_log_record_message(scope, function, before, expr.at)
                        {
                            return;
                        }
                        if self.phase03 && self.defer_static_parameter_call(function, expr, CallArguments {
                            scope,
                            before,
                            at: expr.at,
                            positional: arguments,
                            keywords,
                        }) {
                            self.scope_issue(scope, expr.at, "unresolved_call", "call target is outside the bounded model");
                            return;
                        }
                        let parameter = self.parameter_binding(scope, function, before);
                        if let Some(parameter) = parameter {
                            self.parameter_calls.push(ParameterCall {
                                scope,
                                parameter,
                                at: expr.at,
                            });
                        } else {
                            if self.phase03 {
                                self.record_opaque_source_provider_arguments(CallArguments {
                                    scope,
                                    before,
                                    at: expr.at,
                                    positional: arguments,
                                    keywords,
                                });
                            }
                            self.scope_issue(
                                scope,
                                expr.at,
                                "unresolved_call",
                                "call target is outside the bounded model",
                            );
                        }
                    }
                }
            }
            ExpressionKind::Await(value) => {
                self.walk(scope, value, before);
                let target = self.identity(scope, value, before, 0);
                if !matches!(target, Value::ScheduledTask) {
                    self.activate_callback(scope, target, "awaited_coroutine", expr.at);
                }
            }
            ExpressionKind::Member(base, name) => {
                self.walk(scope, base, before);
                let base = self.identity(scope, base, before, 1);
                let value = if let Value::Module(file) = base
                    && self
                        .module_member_write(&self.files[file].module, name, (scope, before))
                        .is_none()
                    && let ModuleMember::Lazy(hook, branch) = self.selected_module_member(
                        file,
                        name,
                        END,
                        (scope, before),
                        ModuleMemberAccess::Attribute,
                    ) {
                    self.read.insert(hook);
                    self.execute_lazy_branch(scope, &branch)
                } else {
                    self.member(base, name, (scope, before), 1)
                };
                if let Value::CeleryBackend(object) = value {
                    self.record_celery_backend_read(object, scope, expr.at);
                }
                self.read_pydantic_computed_field(scope, &value, expr.at);
                self.record_class_reference(scope, &value);
                if let Value::FrameworkCode(code) = value {
                    self.edge(scope, code, "code_reference", "certain");
                }
            }
            ExpressionKind::Not(base) => self.walk(scope, base, before),
            ExpressionKind::Compare {
                left,
                right,
                equal,
                identity,
            } => {
                self.walk(scope, left, before);
                self.walk(scope, right, before);
                if !identity {
                    let left = self.identity(scope, left, before, 0);
                    let right = self.identity(scope, right, before, 0);
                    if self.comparison_may_call_method(&left, &right, *equal) {
                        self.scope_issue(
                            scope,
                            expr.at,
                            "unresolved_comparison",
                            "comparison method dispatch is outside the bounded model",
                        );
                    }
                }
            }
            ExpressionKind::OrderingComparison { left, right } => {
                self.walk(scope, left, before);
                self.walk(scope, right, before);
                let left = self.identity(scope, left, before, 0);
                let right = self.identity(scope, right, before, 0);
                if self.ordering_may_call_method(&left, &right) {
                    self.scope_issue(
                        scope,
                        expr.at,
                        "unresolved_comparison",
                        "ordering comparison dispatch is outside the bounded model",
                    );
                }
            }
            ExpressionKind::Sequence { items: values, .. }
            | ExpressionKind::Dictionary(values)
            | ExpressionKind::Addition(values)
            | ExpressionKind::Divide(values)
            | ExpressionKind::Operands(values) => {
                for item in values {
                    self.walk(scope, item, before);
                }
            }
            ExpressionKind::FormattedString(values) => {
                for value in values {
                    self.walk(scope, value, before);
                    if self.phase03 {
                        let formatted = self.identity(scope, value, before, 0);
                        self.record_formatting(scope, formatted, value.at, true);
                    }
                }
                if !self.phase03 {
                    self.scope_issue(
                        scope,
                        expr.at,
                        "unsupported_construct",
                        "formatted strings are outside this compatibility profile",
                    );
                }
            }
            ExpressionKind::Comprehension {
                form,
                iterators,
                elements,
            } => {
                self.walk_comprehension(scope, before, expr.at, *form, iterators, elements);
            }
            ExpressionKind::Subscript { value, index } => {
                self.walk(scope, value, before);
                self.walk(scope, index, before);
                let mapping = self.identity(scope, value, before, 0);
                if self.is_current_command_module(scope, &mapping, index, before) {
                    return;
                }
                let key = self.identity(scope, index, before, 0);
                if !(self.phase03 && is_typing_annotated(&mapping))
                    && (!Self::has_literal_mapping_keys(&mapping)
                        || !(key.is_builtin_scalar()
                            || self.is_module_getattr_parameter(scope, index, before)))
                {
                    self.scope_issue(
                        scope,
                        expr.at,
                        "unresolved_subscript",
                        "subscript dispatch is outside the bounded literal mapping model",
                    );
                }
            }
            ExpressionKind::Conditional {
                test,
                body,
                otherwise,
            } => match self.walk_if_test(scope, test, before) {
                Some(true) => self.walk(scope, body, before),
                Some(false) => self.walk(scope, otherwise, before),
                None => {
                    self.branch_depth += 1;
                    self.walk(scope, body, before);
                    self.walk(scope, otherwise, before);
                    self.branch_depth -= 1;
                }
            },
            ExpressionKind::Membership {
                element, container, ..
            } => {
                self.walk(scope, element, before);
                self.walk(scope, container, before);
                let mapping = self.identity(scope, container, before, 0);
                let key = self.identity(scope, element, before, 0);
                if !Self::has_literal_mapping_keys(&mapping)
                    || !(key.is_builtin_scalar()
                        || self.is_module_getattr_parameter(scope, element, before))
                {
                    self.scope_issue(
                        scope,
                        expr.at,
                        "unresolved_membership",
                        "membership dispatch is outside the bounded literal mapping model",
                    );
                }
            }
            ExpressionKind::Boolean { and, values } => {
                let mut conditional = false;
                for value in values {
                    self.branch_depth += usize::from(conditional);
                    self.walk(scope, value, before);
                    self.branch_depth -= usize::from(conditional);
                    match self.identity(scope, value, before, 0) {
                        Value::Bool(result) if result != *and => break,
                        Value::Bool(_) => {}
                        _ => conditional = self.phase03,
                    }
                }
            }
            ExpressionKind::Name(_) | ExpressionKind::IterationVariable(_) => {
                let value = self.identity(scope, expr, before, 0);
                self.report_unmodeled_deferred_value(scope, expr.at, &value);
                self.record_class_reference(scope, &value);
            }
            ExpressionKind::ConstantLambda
            | ExpressionKind::NullaryConstantLambda
            | ExpressionKind::IdentityLambda
            | ExpressionKind::IdentityDecoratorLambda
            | ExpressionKind::DeferredLambda(_)
            | ExpressionKind::DeferredBoolLambda
                if !self.phase03 =>
            {
                self.scope_issue(
                    scope,
                    expr.at,
                    "unsupported_construct",
                    "lambda expressions are outside this compatibility profile",
                )
            }
            ExpressionKind::EmptyListComprehension if !self.phase03 => self.scope_issue(
                scope,
                expr.at,
                "unsupported_construct",
                "comprehensions are outside this compatibility profile",
            ),
            _ => {}
        }
    }

    fn call_invocation(
        &mut self,
        invocation: Invocation,
        call: CallArguments<'_>,
        certainty: &'static str,
    ) {
        let CallArguments {
            scope,
            before,
            at,
            positional,
            keywords,
        } = call;
        let id = invocation.target;
        if self.skip_changed_command_body(scope, id, at, "direct_call") {
            return;
        }
        if self.is_generator(id) {
            self.edge(scope, id, "generator_creation", certainty);
            self.scope_issue(
                scope,
                at,
                "unresolved_generator_execution",
                "generator value consumption is outside the bounded model",
            );
            return;
        }
        if !invocation.verified {
            self.direct.insert(id);
        }
        self.record_django_global_call(invocation, call, certainty);
        self.record_direct_arguments(invocation, call, certainty);
        if !matches!(self.symbols[id].binding, Binding::Definition(def, _) if def.kind == DefinitionKind::Coroutine)
        {
            let invocation =
                self.with_explicit_receiver(invocation, positional, keywords, |graph, value| {
                    graph.identity(scope, value, before, 0)
                });
            self.record_django_autoreload_method_arguments(invocation, call, certainty);
            if !invocation.verified
                && let Some(class) = invocation.receiver.supplied()
                && self.is_django_command_class(class)
            {
                for parameter in self.django_command_parameters(invocation.target) {
                    self.edge(scope, parameter, "django_command_parameter", "possible");
                }
            }
            self.invoke(scope, invocation, "direct_call", certainty, at);
        }
        if self.is_class(id) {
            let (kind, certainty) = if invocation.verified {
                ("grep_dynamic_call", "possible")
            } else {
                ("constructor", certainty)
            };
            if !self.phase03 || !self.framework_default_constructor(id, call, certainty) {
                self.constructor(scope, id, kind, certainty, at);
            }
            if self.phase03 {
                self.check_django_form_initializer(id, call);
            }
        }
        // Callback parameter substitution is bounded by retaining the finite
        // supplied values even if the callee dispatches dynamically.
        for argument in positional
            .iter()
            .chain(keywords.iter().map(|(_, value)| value))
        {
            let value = self.identity(scope, argument, before, 0);
            if self.phase03 {
                self.record_class_argument_consumer(scope, id, &value);
            }
            self.expose(scope, value, "passed_callable", argument.at);
        }
    }

    fn record_direct_arguments(
        &mut self,
        invocation: Invocation,
        call: CallArguments<'_>,
        certainty: &'static str,
    ) {
        let callee = invocation.target;
        let CallArguments {
            scope: caller,
            before,
            at,
            positional: args,
            keywords,
        } = call;
        if let Some(values) = self.call_argument_values(caller, callee, before, args, keywords) {
            let conditional = invocation.verified
                || certainty != "certain"
                || self.branch_depth != 0
                || self.scopes[caller]
                    .owner
                    .is_some_and(|owner| self.conditional.contains_key(&owner));
            let mut forwarded = BTreeMap::new();
            let Binding::Definition(definition, _) = self.symbols[callee].binding else {
                unreachable!("only definitions with bounded signatures have argument values");
            };
            let parameters = &definition.parameters;
            let required = parameters.len() - definition.defaults.len();
            let arguments_bound = args.len() <= parameters.len()
                && values.len() == args.len() + keywords.len()
                && parameters[..required]
                    .iter()
                    .all(|parameter| values.contains_key(&parameter.text))
                && keywords
                    .iter()
                    .all(|(name, _)| parameters.iter().any(|parameter| parameter.text == *name));
            if self.phase03 && arguments_bound {
                for (parameter, argument) in definition
                    .parameters
                    .iter()
                    .zip(args)
                    .map(|(parameter, argument)| (&parameter.text, argument))
                    .chain(
                        keywords
                            .iter()
                            .map(|(parameter, argument)| (parameter, argument)),
                    )
                {
                    if let Some(source) = self.parameter_binding(caller, argument, before) {
                        forwarded.insert(parameter.clone(), source);
                    }
                }
            }
            let fresh_provider = (self.phase03 && arguments_bound && !invocation.verified)
                .then(|| self.fresh_provider_argument(definition, call, &values))
                .flatten();
            self.direct_arguments
                .entry(callee)
                .or_default()
                .push(DirectArguments {
                    caller,
                    at,
                    implicit_receiver: invocation.implicit_descriptor_receiver
                        || matches!(invocation.receiver, Receiver::Bound(_)),
                    values,
                    forwarded,
                    fresh_provider,
                    verified: invocation.verified,
                    conditional,
                    arguments_bound,
                });
        }
    }

    fn parameter_binding(
        &self,
        scope: ScopeId,
        expression: &Expression,
        before: Location,
    ) -> Option<SymbolId> {
        let ExpressionKind::Name(name) = &expression.kind else {
            return None;
        };
        self.lexical(scope, name, before)
            .filter(|&id| matches!(self.symbols[id].binding, Binding::Parameter))
    }

    fn bounded_declaration_provider_write(
        &mut self,
        scope: ScopeId,
        receiver: &Expression,
        before: Location,
    ) -> bool {
        let is_provider = |value| matches!(value, Value::Provider(name) if is_bounded_declaration_provider(&name));
        is_provider(self.probe_identity(scope, receiver, before))
            || self
                .parameter_binding(scope, receiver, before)
                .and_then(|parameter| self.namespace_parameter_default(parameter))
                .is_some_and(is_provider)
    }

    fn call_argument_values(
        &mut self,
        caller: ScopeId,
        callee: SymbolId,
        before: Location,
        args: &[Expression],
        keywords: &[(String, Expression)],
    ) -> Option<BTreeMap<String, Value>> {
        let Binding::Definition(definition, _) = self.symbols[callee].binding else {
            return None;
        };
        if definition.kind != DefinitionKind::Function
            || !definition.simple_positional_parameters
            || self.scopes[self.symbols[callee].scope]
                .owner
                .is_some_and(|owner| self.is_class(owner))
        {
            return None;
        }
        let mut values = BTreeMap::new();
        for (parameter, argument) in definition.parameters.iter().zip(args) {
            values.insert(
                parameter.text.clone(),
                self.identity(caller, argument, before, 0),
            );
        }
        for (parameter, argument) in keywords {
            values.insert(
                parameter.clone(),
                self.identity(caller, argument, before, 0),
            );
        }
        Some(values)
    }

    fn register_atexit(&mut self, call: CallArguments<'_>) {
        let CallArguments {
            scope,
            before,
            at,
            positional,
            keywords,
        } = call;
        if let Some((callback, forwarded)) = positional.split_first() {
            let callback = self.identity_with_dormant_declarations(scope, callback, before, 0);
            if self.retain_atexit_callback(scope, callback, at) {
                for argument in forwarded
                    .iter()
                    .chain(keywords.iter().map(|(_, value)| value))
                {
                    let value = self.identity_with_dormant_declarations(scope, argument, before, 0);
                    self.expose(scope, value, "atexit_argument", argument.at);
                }
                return;
            }
        }
        self.scope_issue(
            scope,
            at,
            "unresolved_callback",
            "atexit.register requires a bounded synchronous callback as its first positional argument",
        );
    }

    fn retain_atexit_callback(&mut self, scope: ScopeId, value: Value, at: Location) -> bool {
        match value {
            Value::Callable(invocation)
                if !invocation.verified
                    && !matches!(invocation.receiver, Receiver::Explicit(_))
                    && matches!(self.symbols[invocation.target].binding,
                        Binding::Definition(definition, _) if definition.kind == DefinitionKind::Function) =>
            {
                self.invoke(scope, invocation, "atexit_registration", "possible", at);
                true
            }
            Value::Alternatives(values) => {
                let mut resolved = true;
                for value in values {
                    resolved &= self.retain_atexit_callback(scope, value, at);
                }
                resolved
            }
            _ => false,
        }
    }

    fn activate_submission(
        &mut self,
        scope: ScopeId,
        before: Location,
        at: Location,
        arguments: &[Expression],
        keywords: &[(String, Expression)],
    ) {
        let target = arguments.first().map_or(Value::Unknown, |argument| {
            self.identity(scope, argument, before, 0)
        });
        if let Value::Callable(invocation) = &target
            && let Binding::Definition(definition, _) = self.symbols[invocation.target].binding
        {
            let values = self
                .call_argument_values(scope, invocation.target, before, &arguments[1..], keywords)
                .unwrap_or_else(|| {
                    definition
                        .parameters
                        .iter()
                        .map(|p| (p.text.clone(), Value::Unknown))
                        .collect()
                });
            // Executor submissions can supply namespace values but cannot
            // establish a synchronous call during module initialization.
            self.executor_arguments
                .entry(invocation.target)
                .or_default()
                .push(DirectArguments {
                    caller: scope,
                    at,
                    implicit_receiver: false,
                    values,
                    forwarded: BTreeMap::new(),
                    fresh_provider: None,
                    verified: false,
                    conditional: true,
                    arguments_bound: false,
                });
        }
        self.activate_callback(scope, target, "executor_submission", at);
    }

    fn record_django_runpython(&mut self, call: CallArguments<'_>) {
        let reject = |graph: &mut Self| {
            graph.scope_issue(
                call.scope,
                call.at,
                "unresolved_registration",
                "Django RunPython requires a resolved forward callback and optional reverse callback",
            );
        };
        let Some([Some(code), reverse, ..]) = registration_arguments(
            call.positional,
            call.keywords,
            ["code", "reverse_code", "atomic", "hints", "elidable"],
        ) else {
            reject(self);
            return;
        };
        let mut callbacks = Vec::new();
        for expression in [Some(code), reverse].into_iter().enumerate() {
            let (index, Some(expression)) = expression else {
                continue;
            };
            match self.probe_identity(call.scope, expression, call.before) {
                Value::None if index == 1 => {}
                Value::Scalar if index == 1 && self.django_migration_none(call, expression) => {}
                Value::Provider(provider)
                    if provider
                        .strip_suffix(".noop")
                        .is_some_and(is_django_runpython) => {}
                Value::Callable(invocation) if !self.is_class(invocation.target) => {
                    callbacks.push(invocation);
                }
                _ => {
                    reject(self);
                    return;
                }
            }
        }
        for invocation in callbacks {
            let target = invocation.target;
            let Binding::Definition(definition, body) = self.symbols[target].binding else {
                continue;
            };
            let receiver = usize::from(matches!(invocation.receiver, Receiver::Bound(_)));
            // Construction stores the callable. A later migration supplies
            // two positional arguments; construction does not run its body.
            self.edge(
                call.scope,
                target,
                "django_migration_callback_declaration",
                "possible",
            );
            let supplied = receiver + 2;
            let positional = definition.positional_parameter_count;
            let bound = if supplied > positional && definition.has_variadic_positional_parameter {
                positional + 1
            } else {
                supplied.min(positional)
            };
            for parameter in definition.parameters.iter().take(bound).skip(receiver) {
                let binding = self.by_location[&(body, parameter.at.line, parameter.at.column)];
                self.edge(
                    call.scope,
                    binding,
                    "django_migration_parameter",
                    "possible",
                );
            }
        }
    }

    fn django_migration_none(&self, call: CallArguments<'_>, expression: &Expression) -> bool {
        match &expression.kind {
            ExpressionKind::None => true,
            ExpressionKind::Name(name) => self
                .lexical(call.scope, name, call.before)
                .is_some_and(|id| self.django_literal_none(id, 0) == Some(true)),
            _ => false,
        }
    }

    fn activate_argument(
        &mut self,
        scope: ScopeId,
        arg: Option<&Expression>,
        before: Location,
        kind: &'static str,
        at: Location,
    ) {
        let target = arg.map_or(Value::Unknown, |expr| self.identity(scope, expr, before, 0));
        self.activate_callback(scope, target, kind, at);
    }

    fn activate_framework_value(
        &mut self,
        scope: ScopeId,
        value: Value,
        kind: &'static str,
    ) -> bool {
        match value {
            Value::NullaryConstantFunction if kind == "fastapi_lifecycle" => true,
            value @ (Value::Callable(_) | Value::Instance(_)) => {
                if let Some(target) = self.fastapi_hook_target(value) {
                    self.edge(scope, target, kind, "certain");
                    true
                } else {
                    false
                }
            }
            Value::Sequence(values) | Value::Alternatives(values) => {
                let mut resolved = true;
                for value in values {
                    resolved &= self.activate_framework_value(scope, value, kind);
                }
                resolved
            }
            Value::Mapping(entries) => {
                let mut resolved = true;
                for value in entries.into_iter().skip(1).step_by(2) {
                    resolved &= self.activate_framework_value(scope, value, kind);
                }
                resolved
            }
            _ => false,
        }
    }

    fn record_fastapi_dependency(
        &mut self,
        owner: SymbolId,
        expression: &Expression,
        value: Value,
    ) {
        match value {
            Value::FastapiDependency(Some(target)) | Value::FastapiAnnotated(Some(target)) => {
                self.record_fastapi_injected_parameter(owner, expression.at);
                self.fastapi_parameter_dependency(owner, target);
            }
            Value::FastapiDependency(None)
            | Value::FastapiAnnotated(None)
            | Value::FastapiInferredDependency => self.fastapi_dependency_issues.push((
                owner,
                Issue::new(
                    "unresolved_registration",
                    &self.files[self.scopes[self.symbols[owner].scope].file].path,
                    "FastAPI parameter dependency could not be resolved",
                )
                .at(expression.at),
            )),
            _ => {}
        }
    }

    fn fastapi_annotation_metadata(
        &mut self,
        scope: ScopeId,
        before: Location,
        annotation: &Expression,
    ) -> Value {
        // The frontend lowers subscription and tuple annotations into nested
        // operand groups. Resolve the generic's binding before using metadata.
        if let ExpressionKind::Operands(parts) = &annotation.kind
            && let [generic, arguments] = parts.as_slice()
            && is_typing_annotated(&self.probe_identity(scope, generic, before))
            && let ExpressionKind::Operands(arguments) = &arguments.kind
            && let Some((annotation, metadata)) = arguments.split_first()
        {
            self.fastapi_selected_metadata(scope, before, annotation, metadata, 0)
        } else {
            match self.probe_identity(scope, annotation, before) {
                value @ Value::FastapiAnnotated(_) => value,
                _ => Value::Unknown,
            }
        }
    }

    fn fastapi_selected_metadata(
        &mut self,
        scope: ScopeId,
        before: Location,
        annotation: &Expression,
        metadata: &[Expression],
        depth: usize,
    ) -> Value {
        // FastAPI uses the final supported field or dependency marker.
        for meta in metadata.iter().rev() {
            match self.identity(scope, meta, before, depth) {
                Value::FastapiDependency(target) => return Value::FastapiAnnotated(target),
                Value::FastapiInferredDependency => {
                    let annotated = self.identity(scope, annotation, before, depth);
                    let target = if matches!(annotated, Value::Callable(class) if self.is_class(class.target))
                    {
                        self.fastapi_dependency_target(annotated)
                    } else {
                        None
                    };
                    return Value::FastapiAnnotated(target);
                }
                Value::FastapiFieldMarker => return Value::Scalar,
                _ => {}
            }
        }
        Value::Scalar
    }

    fn activate_callback(
        &mut self,
        scope: ScopeId,
        target: Value,
        kind: &'static str,
        at: Location,
    ) {
        if matches!(target, Value::Callable(_) | Value::Coroutine(_)) {
            if matches!(kind, "executor_submission" | "process_target")
                && let Value::Callable(invocation) = &target
                && self.is_class(invocation.target)
            {
                self.constructor(
                    scope,
                    invocation.target,
                    "callback_constructor",
                    "possible",
                    at,
                );
            }
            self.expose(scope, target, kind, at);
        } else {
            self.scope_issue(
                scope,
                at,
                "unresolved_callback",
                "callback target could not be bounded",
            );
        }
    }

    fn receiver_parameter(&self, target: SymbolId) -> Option<SymbolId> {
        let symbol = &self.symbols[target];
        let Binding::Definition(definition, body) = symbol.binding else {
            return None;
        };
        if !definition.has_positional_parameter
            || matches!(
                definition.name.text.as_str(),
                "__new__" | "__init_subclass__" | "__class_getitem__"
            )
            || !self.scopes[symbol.scope]
                .owner
                .is_some_and(|owner| self.is_class(owner))
        {
            return None;
        }
        let parameter = definition.parameters.first()?;
        Some(self.by_location[&(body, parameter.at.line, parameter.at.column)])
    }

    fn with_explicit_receiver<'expr>(
        &mut self,
        mut invocation: Invocation,
        arguments: &'expr [Expression],
        keywords: &'expr [(String, Expression)],
        mut evaluate: impl FnMut(&mut Self, &'expr Expression) -> Value,
    ) -> Invocation {
        // A plain call to a function from a class namespace has no
        // descriptor binding. Its receiver, if any, comes from the arguments.
        if matches!(invocation.receiver, Receiver::Explicit(_))
            || (matches!(invocation.receiver, Receiver::Ordinary)
                && self.receiver_parameter(invocation.target).is_some())
        {
            let argument = arguments.first().or_else(|| {
                let Binding::Definition(definition, _) = self.symbols[invocation.target].binding
                else {
                    return None;
                };
                // A simple positional signature also accepts keywords by name.
                if !definition.simple_positional_parameters {
                    return None;
                }
                let parameter = definition.parameters.first()?;
                keywords
                    .iter()
                    .find_map(|(name, value)| (name == &parameter.text).then_some(value))
            });
            let class = match argument.map(|argument| evaluate(self, argument)) {
                Some(Value::Instance(class)) => Some(class),
                _ => None,
            };
            invocation.receiver = Receiver::Explicit(class);
        }
        invocation
    }

    fn annotation(&mut self, scope: ScopeId, expr: &Expression, before: Location, evaluated: bool) {
        let evaluated = evaluated
            && !self.files[self.scopes[scope].file]
                .facts
                .postponed_annotations;
        match &expr.kind {
            ExpressionKind::Addition(values)
            | ExpressionKind::Divide(values)
            | ExpressionKind::Operands(values)
            | ExpressionKind::Sequence { items: values, .. } => {
                for reference in values {
                    self.annotation(scope, reference, before, evaluated);
                }
            }
            ExpressionKind::Call { .. }
            | ExpressionKind::KeywordMappingCall { .. }
            | ExpressionKind::RepeatedKeywordCall { .. }
                if evaluated =>
            {
                self.walk(scope, expr, before);
            }
            _ => {
                let value = if evaluated {
                    self.walk(scope, expr, before);
                    self.identity(scope, expr, before, 0)
                } else {
                    self.probe_identity(scope, expr, before)
                };
                if let Value::Callable(Invocation { target: id, .. }) = value {
                    self.edge(scope, id, "type_reference", "possible");
                }
            }
        }
    }

    fn record_evaluated_module_boolean(
        &mut self,
        scope: ScopeId,
        names: &[Name],
        value: &Expression,
        before: Location,
    ) {
        if !self.phase03
            || scope != self.scopes[scope].file
            || self.branch_depth != 0
            || self.has_command_namespace(scope)
        {
            return;
        }
        let (operand, negate) = match &value.kind {
            ExpressionKind::Not(operand) => (operand.as_ref(), true),
            _ => (value, false),
        };
        let value = match &operand.kind {
            ExpressionKind::Bool(value) => Some(*value),
            ExpressionKind::Name(name) => self
                .local(scope, name, before)
                .and_then(|id| self.evaluated_module_booleans.get(&id).copied()),
            _ => None,
        };
        if let Some(value) = value {
            // The walk has already recorded the RHS reads. Capture the value of
            // this specific binding so a later alias needs only one lookup.
            for name in names {
                let id = self.by_location[&(scope, name.at.line, name.at.column)];
                if !self.conditional.contains_key(&id) {
                    self.evaluated_module_booleans.insert(id, value != negate);
                }
            }
        }
    }

    fn suite(&mut self, scope: ScopeId, body: &[Statement]) -> bool {
        for statement in body {
            let at = statement.at;
            if self.phase03 && self.prove_terminal_module_failure(scope, statement) {
                self.failed_terminal_modules.insert(scope);
                return true;
            }
            match &statement.kind {
                StatementKind::Define(definition) => {
                    if definition.is_generator && !self.phase03 {
                        self.scope_issue(
                            scope,
                            at,
                            "unsupported_construct",
                            "generator definitions are outside this compatibility profile",
                        );
                    }
                    if matches!(
                        definition.name.text.as_str(),
                        "__getattribute__" | "__getattr__"
                    ) && let Some(owner) = self.scopes[scope].owner
                        && self.is_class(owner)
                        && !self.is_django_command_class(owner)
                    {
                        self.scope_issue(
                            scope,
                            at,
                            "unresolved_descriptor",
                            "dynamic instance attribute resolution is outside the bounded model",
                        );
                    }
                    let target = self.by_location
                        [&(scope, definition.name.at.line, definition.name.at.column)];
                    let Binding::Definition(_, definition_scope) = self.symbols[target].binding
                    else {
                        unreachable!()
                    };
                    if self.phase03
                        && definition.kind != DefinitionKind::Class
                        && definition.defaults.is_empty()
                        && definition.annotations.is_empty()
                        && let [decorator] = definition.decorators.as_slice()
                        && self.django_receiver_rejects_definition(scope, decorator, at)
                    {
                        if let ExpressionKind::KeywordMappingCall {
                            function,
                            arguments,
                            ..
                        } = &decorator.kind
                        {
                            // These operands are evaluated before the missing
                            // mapping aborts the decorator expression.
                            self.walk(scope, function, at);
                            for argument in arguments {
                                self.walk(scope, argument, at);
                            }
                        } else {
                            self.walk(scope, decorator, at);
                        }
                        self.rejected_django_callbacks.insert(target);
                        return true;
                    }
                    for (index, annotation) in definition.annotations.iter().enumerate() {
                        self.annotation(scope, annotation, at, true);
                        if self.phase03
                            && index < definition.parameter_annotation_count
                            && !self.files[self.scopes[scope].file]
                                .facts
                                .postponed_annotations
                        {
                            let metadata = self.fastapi_annotation_metadata(scope, at, annotation);
                            self.record_fastapi_dependency(target, annotation, metadata);
                        }
                    }
                    for expression in &definition.defaults {
                        self.walk(scope, expression, at);
                        if self.phase03 {
                            let value = self.probe_identity(scope, expression, at);
                            self.record_fastapi_dependency(target, expression, value);
                        }
                    }
                    let mut valid_bases = true;
                    let literal_enum = self.phase03
                        && definition.kind == DefinitionKind::Class
                        && (self.literal_string_enum(target)
                            || self.record_django_choice_declaration(target));
                    for base in &definition.bases {
                        self.walk(scope, base, at);
                        let value = self.class_base(scope, base, at, 0);
                        valid_bases &= !matches!(value, Value::MissingFrameworkMember);
                        if let Value::Callable(Invocation { target: id, .. }) = value {
                            self.edge(scope, id, "base_class", "certain");
                        }
                        if !literal_enum {
                            self.subclass_hook(scope, value, base.at, definition);
                        }
                    }
                    if definition.kind == DefinitionKind::Class {
                        if self.phase03 {
                            if let Some(expression) = &definition.metaclass {
                                self.walk(scope, expression, at);
                            }
                            if valid_bases {
                                self.apply_metaclass(scope, target, at);
                            }
                        } else if definition.metaclass.is_some() {
                            self.scope_issue(
                                scope,
                                at,
                                "unsupported_construct",
                                "metaclass keywords are outside the compatibility subset",
                            );
                        }
                        if let Some(table) = definition.table_keyword {
                            if self.phase03
                                && self.framework_class_role(target, &mut BTreeSet::new(), 0)
                                    == Some(FrameworkClassRole::SqlModel)
                            {
                                if table && valid_bases {
                                    self.register_sqlmodel_table(scope, target);
                                }
                            } else {
                                self.scope_issue(
                                    scope,
                                    at,
                                    "unsupported_construct",
                                    "class table keyword requires a resolved SQLModel base",
                                );
                            }
                        }
                        if valid_bases {
                            self.scopes[scope].class_bodies.push(definition_scope);
                        } else {
                            self.dormant_classes.insert(target);
                        }
                    }
                    let mut preserves_receiver = false;
                    if !self.phase03 || !self.apply_fastapi_replacement_stack(target) {
                        preserves_receiver = true;
                        for decorator in &definition.decorators {
                            preserves_receiver &= self.decorator(scope, decorator, target, at);
                        }
                    }
                    if preserves_receiver
                        && let Some(receiver) = self.receiver_parameter(target)
                        && let Some(class) = self.scopes[scope].owner
                    {
                        self.instance_receivers.insert(receiver, class);
                    }
                }
                StatementKind::Assign {
                    names,
                    value,
                    annotation,
                } => {
                    if let Some(value) = value {
                        self.walk(scope, value, at);
                        self.record_evaluated_module_boolean(scope, names, value, at);
                        if annotation.is_none() {
                            self.record_django_global_initializer(scope, names, value, at);
                        }
                    }
                    if let Some(annotation) = annotation {
                        let evaluated = self.scopes[scope]
                            .owner
                            .is_none_or(|owner| self.is_class(owner));
                        self.annotation(scope, annotation, at, evaluated);
                    }
                }
                StatementKind::AssignUnpack { names, value } => {
                    if !self.phase03 {
                        self.issue(
                            self.scopes[scope].file,
                            at,
                            "unsupported_construct",
                            "unpacking is outside the compatibility subset",
                        );
                        continue;
                    }
                    self.walk(scope, value, at);
                    let known_length = match &value.kind {
                        ExpressionKind::Sequence { items, .. } => Some(items.len()),
                        _ => self
                            .literal_split_items(scope, value, at, 0)
                            .map(|items| items.len()),
                    };
                    match known_length {
                        Some(length) if length == names.len() => {}
                        Some(_) => {
                            self.scope_issue(
                                scope,
                                at,
                                "invalid_unpack",
                                "assignment unpacking has a known element count mismatch",
                            );
                            return true;
                        }
                        _ => self.scope_issue(
                            scope,
                            at,
                            "unresolved_unpack",
                            "assignment unpacking requires a bounded list or tuple value",
                        ),
                    }
                }
                StatementKind::AssignDjangoSettings { environment, value } => {
                    self.walk(scope, value, at);
                    self.walk(scope, environment, at);
                    if self.phase03
                        && matches!(self.identity(scope, environment, at, 0),
                            Value::Provider(provider) if provider == "os.environ")
                    {
                        self.scopes[scope].django_settings =
                            match self.identity(scope, value, at, 0) {
                                Value::Text(module) => DjangoSettings::Module(module),
                                _ => DjangoSettings::Unknown,
                            };
                    } else {
                        self.scope_issue(scope, at, "unsupported_construct", "assignment target must be a simple name or a resolved Django settings update");
                    }
                }
                StatementKind::SetGlobalName { value, .. } => {
                    self.record_django_global_write(scope, value, at);
                }
                StatementKind::DjangoSettingsGlobalCall(call) => {
                    if self.phase03 {
                        self.walk(scope, call, at);
                    } else {
                        self.scope_issue(
                            scope,
                            at,
                            "unsupported_construct",
                            "global declarations are outside the compatibility subset",
                        );
                    }
                }
                StatementKind::EditDjangoBackendList { receiver, edit } => {
                    if let Some(value) = edit.value() {
                        self.walk(scope, value, at);
                    }
                    self.walk(scope, receiver, at);
                    self.scope_issue(
                        scope,
                        at,
                        "unresolved_setting_mutation",
                        "Django backend list mutation requires a proven list in the selected settings",
                    );
                }
                StatementKind::DeleteName(name) => {
                    if !self.phase03
                        || scope != self.scopes[scope].file
                        || self.local(scope, &name.text, at).is_none_or(|previous| {
                            self.binding_depends_on_branch(previous, at)
                                || matches!(
                                    self.symbols[previous].binding,
                                    Binding::Deleted | Binding::Variable(None, _)
                                )
                        })
                    {
                        self.scope_issue(
                            scope,
                            at,
                            "unsupported_construct",
                            "deleting a name requires a previously bound module-local name",
                        );
                    }
                }
                StatementKind::WriteAttribute {
                    receiver, value, ..
                }
                | StatementKind::WriteModuleEntry {
                    mapping: receiver,
                    value,
                    ..
                } => {
                    if let Some(value) = value {
                        self.walk(scope, value, at);
                    }
                    self.walk(scope, receiver, at);
                    let flask_write = matches!(&statement.kind,
                        StatementKind::WriteAttribute { member, value: Some(_), .. }
                        if self.modeled_flask_context_write(scope, receiver, member, at));
                    let celery_write = self.phase03
                        && self.record_celery_backend_write(scope, at, &statement.kind);
                    let fresh_field_write = self.phase03
                        && (self.is_fresh_field_initializer(scope, statement)
                            || self.defer_constructor_hook_store(scope, statement));
                    let form_field_write =
                        self.phase03 && self.modeled_django_instance_field_write(scope, statement);
                    if !flask_write
                        && !celery_write
                        && !fresh_field_write
                        && !form_field_write
                        && !self.modeled_framework_write(scope, at)
                    {
                        if self.phase03
                            && matches!(&statement.kind, StatementKind::WriteAttribute { .. })
                        {
                            self.record_unmodeled_class_write(scope, receiver, at);
                        }
                        self.scope_issue(
                            scope,
                            at,
                            "unresolved_attribute_write",
                            "attribute mutation is outside the bounded framework model",
                        );
                    }
                }
                StatementKind::ForwardSuperInit {
                    super_name,
                    positional,
                    keywords,
                } => {
                    if !self.phase03 {
                        self.issue(
                            self.scopes[scope].file,
                            at,
                            "unsupported_construct",
                            "forwarded super initializer is outside the compatibility subset",
                        );
                        continue;
                    }
                    self.walk(scope, super_name, at);
                    self.walk(scope, positional, at);
                    self.walk(scope, keywords, at);
                    if !self.modeled_django_form_super_init(scope) {
                        self.scope_issue(
                            scope,
                            at,
                            "unsupported_construct",
                            "forwarded super initializer is outside the bounded Django form model",
                        );
                    }
                }
                StatementKind::WriteCeleryConfigEntry {
                    mapping,
                    key,
                    value,
                } => {
                    if let Some(value) = value {
                        self.walk(scope, value, at);
                    }
                    self.walk(scope, mapping, at);
                    self.walk(scope, key, at);
                    if !self.phase03
                        || !self.record_celery_config_key(scope, at, mapping, key, value)
                    {
                        self.scope_issue(
                            scope,
                            at,
                            "unsupported_construct",
                            "a computed configuration key requires a resolved Celery settings object",
                        );
                    }
                }
                StatementKind::Return(value) => {
                    if let Some(value) = value {
                        self.walk(scope, value, at);
                        let result = self.identity(scope, value, at, 0);
                        self.expose(scope, result, "returned_callable", value.at);
                    }
                    return true;
                }
                StatementKind::Raise(value) => {
                    if let Some(value) = value {
                        self.walk(scope, value, at);
                    }
                    return true;
                }
                StatementKind::Assert(test) => {
                    self.walk(scope, test, at);
                    if matches!(self.identity(scope, test, at, 0), Value::Bool(false)) {
                        return true;
                    }
                }
                StatementKind::Evaluate(value) => {
                    if !self.phase03
                        || !(self.discarded_generator_call(scope, value, at)
                            || self.walk_flask_globals_update(scope, value, at)
                            || self.walk_flask_login_mock_start(scope, value, at))
                    {
                        self.walk(scope, value, at);
                    }
                }
                StatementKind::DiscardedComprehension(iterable) => {
                    if self.phase03 {
                        self.walk(scope, iterable, at);
                    } else {
                        self.scope_issue(
                            scope,
                            at,
                            "unsupported_construct",
                            "comprehensions are outside the compatibility subset",
                        );
                    }
                }
                StatementKind::LiteralRouteLoop { registration, .. } => {
                    if !self.phase03 || !self.fastapi_literal_route_loop(scope, registration) {
                        self.scope_issue(
                            scope,
                            at,
                            "unsupported_construct",
                            "route loop is outside the bounded FastAPI registration model",
                        );
                    }
                }
                StatementKind::If {
                    test,
                    body,
                    otherwise,
                } => {
                    let stop = match self.walk_if_test(scope, test, at) {
                        Some(true) => {
                            self.scopes[scope]
                                .namespace_exclusions
                                .extend(namespace_span(otherwise));
                            self.suite(scope, body)
                        }
                        Some(false) => {
                            self.scopes[scope]
                                .namespace_exclusions
                                .extend(namespace_span(body));
                            self.suite(scope, otherwise)
                        }
                        None => {
                            let django_before = self.scopes[scope].django_settings.clone();
                            self.branch_depth += 1;
                            let body_stops = self.suite(scope, body);
                            let django_body = std::mem::replace(
                                &mut self.scopes[scope].django_settings,
                                django_before,
                            );
                            let excluded_branch = self.scopes[scope].namespace_exclusions.len();
                            self.scopes[scope]
                                .namespace_exclusions
                                .extend(namespace_span(body));
                            let otherwise_stops = self.suite(scope, otherwise);
                            self.branch_depth -= 1;
                            let django_other = &mut self.scopes[scope].django_settings;
                            if otherwise_stops || *django_other == django_body {
                                *django_other = django_body;
                            } else if !body_stops {
                                *django_other = DjangoSettings::Unknown;
                            }
                            if !body_stops && !body.is_empty() {
                                self.scopes[scope]
                                    .namespace_exclusions
                                    .remove(excluded_branch);
                            }
                            if otherwise_stops {
                                self.scopes[scope]
                                    .namespace_exclusions
                                    .extend(namespace_span(otherwise));
                            }
                            body_stops && otherwise_stops
                        }
                    };
                    if stop {
                        return true;
                    }
                }
                StatementKind::With { items, body } => {
                    for (expression, _) in items {
                        self.walk(scope, expression, at);
                        if !matches!(self.identity(scope, expression, at, 0), Value::Executor) {
                            self.scope_issue(
                                scope,
                                expression.at,
                                "unresolved_context_manager",
                                "context manager is outside the bounded model",
                            );
                        }
                    }
                    if self.suite(scope, body) {
                        return true;
                    }
                }
                StatementKind::Import(import) => {
                    let module = self.import_module(self.scopes[scope].file, import);
                    if self.phase03
                        && self.stop_at_failed_flask_provider_import(
                            scope,
                            module.as_deref(),
                            import,
                        )
                    {
                        return true;
                    }
                    if let Some(branch) =
                        self.record_import(scope, scope, module.as_deref(), import)
                    {
                        self.execute_lazy_branch(scope, &branch);
                    }
                }
                StatementKind::ProviderWildcardImport(imports) => {
                    if !self.supported_provider_wildcard(scope, at, imports) {
                        self.scope_issue(
                            scope,
                            at,
                            "unsupported_construct",
                            "star imports cannot resolve finite bindings at this location",
                        );
                    }
                }
                StatementKind::ProjectWildcardImport { .. } => {
                    self.scope_issue(
                        scope,
                        at,
                        "unsupported_construct",
                        "star imports cannot resolve finite bindings at this location",
                    );
                }
                StatementKind::OptionalProjectClassFallback {
                    package,
                    missing_member,
                    fallback,
                } => {
                    if let Some(file) =
                        self.optional_project_package(scope, at, package, missing_member)
                    {
                        self.scopes[scope].imports.push(file);
                        if self.suite(scope, fallback) {
                            return true;
                        }
                    } else {
                        self.scope_issue(
                            scope,
                            at,
                            "unresolved_optional_import",
                            "optional import requires a proven missing member of an inert selected regular package",
                        );
                    }
                }
            }
        }
        false
    }

    fn discarded_generator_call(
        &mut self,
        scope: ScopeId,
        value: &Expression,
        before: Location,
    ) -> bool {
        if self.scopes[scope]
            .owner
            .is_some_and(|owner| self.is_generator(owner))
        {
            // A yield statement can also lower to Evaluate in a generator body.
            return false;
        }
        let ExpressionKind::Call {
            function,
            arguments,
            keywords,
        } = &value.kind
        else {
            return false;
        };
        let ExpressionKind::Name(name) = &function.kind else {
            return false;
        };
        let Some(generator) = self.local(scope, name, before) else {
            return false;
        };
        let Binding::Definition(definition, _) = self.symbols[generator].binding else {
            return false;
        };
        if !definition.is_generator
            || definition.kind != DefinitionKind::Function
            || !definition.decorators.is_empty()
            || self.binding_depends_on_branch(generator, before)
            || (!definition.parameters.is_empty() && !definition.simple_positional_parameters)
            || definition.parameters.len() != arguments.len()
            || !keywords.is_empty()
        {
            return false;
        }
        self.walk_expression(scope, function, before, true);
        for argument in arguments {
            self.walk(scope, argument, before);
        }
        self.edge(scope, generator, "generator_creation", "certain");
        true
    }

    /// Returns whether the decorator preserves the function's instance binding.
    fn decorator(
        &mut self,
        scope: ScopeId,
        decorator: &Expression,
        target: SymbolId,
        before: Location,
    ) -> bool {
        if let ExpressionKind::KeywordMappingCall {
            function,
            arguments,
            mapping,
        } = &decorator.kind
        {
            self.walk(scope, function, before);
            for argument in arguments {
                self.walk(scope, argument, before);
            }
            self.walk(scope, mapping, before);
            self.scope_issue(
                scope,
                decorator.at,
                "unsupported_construct",
                "unbounded decorator keyword expansion",
            );
            return false;
        }
        let (function, args, keywords) = match &decorator.kind {
            ExpressionKind::Call {
                function,
                arguments,
                keywords,
            } => (function.as_ref(), arguments.as_slice(), keywords.as_slice()),
            _ => (decorator, [].as_slice(), [].as_slice()),
        };
        self.walk(scope, function, before);
        for value in args.iter().chain(keywords.iter().map(|(_, value)| value)) {
            self.walk(scope, value, before);
        }
        if self.phase03 && self.record_fastapi_parameter_route(scope, decorator, target, before) {
            return false;
        }
        if self.phase03 && self.apply_bounded_tool_decorator(scope, decorator, target, before) {
            return false;
        }
        let (decorator_value, reads_rebound_global) = self.track_rebound_global_reads(|graph| {
            graph.call_target_identity(scope, function, before, 0)
        });
        match decorator_value {
            Value::Method(_, ObjectKind::AiohttpRouteTable, name)
                if aiohttp::is_http_decorator(&name)
                    && self.aiohttp_route_declaration(scope, target, decorator, before) =>
            {
                return true;
            }
            Value::Method(object, ObjectKind::App, name)
                if self.phase03 && name == "middleware" =>
            {
                if let Some([Some(protocol)]) =
                    registration_arguments(args, keywords, ["middleware_type"])
                    && matches!(self.identity(scope, protocol, before, 0), Value::Text(protocol) if protocol == "http")
                {
                    self.fastapi_route(
                        object,
                        target,
                        CallArguments {
                            scope,
                            before,
                            at: decorator.at,
                            positional: args,
                            keywords,
                        },
                        &name,
                    );
                } else {
                    self.scope_issue(
                        scope,
                        decorator.at,
                        "unresolved_registration",
                        "FastAPI middleware protocol could not be resolved",
                    );
                }
            }
            Value::Method(object, ObjectKind::TyperApp, name)
                if name == "callback"
                    && matches!(decorator.kind, ExpressionKind::Call { .. })
                    && args.is_empty()
                    && keywords.is_empty() =>
            {
                self.scopes[scope]
                    .routes
                    .push((object, target, "typer_callback"));
                return true;
            }
            Value::Method(object, ObjectKind::FlaskLoginManager, name)
                if !matches!(decorator.kind, ExpressionKind::Call { .. })
                    && flask_login_callback(&name) =>
            {
                let routes = if reads_rebound_global {
                    // The lexical receiver is a candidate until write order is proved.
                    &mut self.scopes[scope].optional_flask_routes
                } else {
                    &mut self.scopes[scope].routes
                };
                routes.push((object, target, "flask_login_registration"));
                return true;
            }
            Value::Method(object, kind, name)
                if is_route(kind, &name, self.phase03)
                    && (!matches!(kind, ObjectKind::FlaskApp | ObjectKind::FlaskBlueprint)
                        || flask_callback_method(kind, &name)
                            != matches!(decorator.kind, ExpressionKind::Call { .. })) =>
            {
                if self.phase03 && matches!(kind, ObjectKind::App | ObjectKind::Router) {
                    self.fastapi_route(
                        object,
                        target,
                        CallArguments {
                            scope,
                            before,
                            at: decorator.at,
                            positional: args,
                            keywords,
                        },
                        &name,
                    );
                } else {
                    self.scopes[scope].routes.push((
                        object,
                        target,
                        framework_registration_kind(kind),
                    ));
                }
            }
            Value::Provider(name)
                if self.phase03
                    && !reads_rebound_global
                    && self.record_gin_decorator(scope, decorator, target, before, &name) => {}
            Value::Provider(name)
                if self.phase03
                    && !reads_rebound_global
                    && self.record_traitlets_decorator(scope, decorator, target, &name) => {}
            Value::Provider(name)
                if self.phase03
                    && !reads_rebound_global
                    && self.is_click_command_declaration(scope, decorator, target, &name) =>
            {
                self.edge(scope, target, "click_command_declaration", "possible");
            }
            Value::Provider(name) if self.phase03 && is_django_receiver(&name) => {
                let signal = self.django_receiver(scope, args, keywords, before, 0);
                self.record_django_signal(
                    scope,
                    signal,
                    Some(Value::Callable(Invocation::ordinary(target))),
                    decorator.at,
                );
            }
            Value::DjangoReceiver(signal)
                if !matches!(decorator.kind, ExpressionKind::Call { .. }) =>
            {
                self.record_django_signal(
                    scope,
                    signal,
                    Some(Value::Callable(Invocation::ordinary(target))),
                    decorator.at,
                );
            }
            Value::Provider(name)
                if self.phase03
                    && matches!(
                        name.as_str(),
                        "pydantic.computed_field"
                            | "pydantic.field_serializer"
                            | "pydantic.model_serializer"
                    ) =>
            {
                self.record_pydantic_serializer(scope, target, &name, args, keywords);
            }
            Value::Provider(name)
                if self.phase03
                    && matches!(
                        name.as_str(),
                        "sqlalchemy.event.listens_for" | "celery.shared_task"
                    ) =>
            {
                let kind = match name.as_str() {
                    "sqlalchemy.event.listens_for" => "sqlalchemy_listener",
                    "celery.shared_task" => "celery_task",
                    _ => unreachable!(),
                };
                self.edge(scope, target, kind, "certain");
            }
            Value::Provider(name) if self.phase03 && name == "atexit.register" => {
                if !matches!(decorator.kind, ExpressionKind::Call { .. })
                    && self.scopes[scope]
                        .owner
                        .is_none_or(|owner| !self.is_class(owner))
                    && self.retain_atexit_callback(
                        scope,
                        Value::Callable(Invocation::ordinary(target)),
                        decorator.at,
                    )
                {
                    return true;
                }
                self.scope_issue(
                    scope,
                    decorator.at,
                    "unresolved_decorator",
                    "atexit.register decorator requires a bounded synchronous function",
                );
            }
            Value::Provider(name) if self.phase03 && name == "pytest.hookimpl" => {}
            Value::Provider(name)
                if self.phase03
                    && args.is_empty()
                    && keywords.is_empty()
                    && matches!(
                        name.as_str(),
                        "marshmallow.post_load"
                            | "marshmallow.decorators.post_load"
                            | "marshmallow.validates_schema"
                            | "marshmallow.decorators.validates_schema"
                    )
                    && self.record_marshmallow_hook(scope, target) =>
            {
                return true;
            }
            Value::Provider(name)
                if self.phase03
                    && name == "attrs.define"
                    && args.is_empty()
                    && keywords.is_empty()
                    && self.register_attrs_fields(target) => {}
            Value::PluggyHookMarker
                if self.phase03
                    && args.is_empty()
                    && keywords.is_empty()
                    && !self.is_class(target)
                    && matches!(self.symbols[target].binding, Binding::Definition(definition, _)
                        if definition.decorators.len() == 1) =>
            {
                // Marking a function stores its HookimplOpts. Execution still
                // requires registration and a hook call, or a direct caller.
                self.edge(scope, target, "pluggy_hook_declaration", "certain");
                return true;
            }
            Value::Provider(name)
                if self.phase03
                    && name == "contextlib.asynccontextmanager"
                    && self.is_fastapi_lifespan(target) => {}
            Value::Provider(name)
                if self.phase03
                    && name == "sqlalchemy.ext.hybrid.hybrid_property"
                    && !matches!(decorator.kind, ExpressionKind::Call { .. })
                    && let Some(owner) = self.scopes[scope].owner
                    && self.is_class(owner) =>
            {
                if matches!(self.symbols[target].binding,
                    Binding::Definition(definition, _) if definition.decorators.len() == 1)
                {
                    self.edge(scope, target, "sqlalchemy_hybrid_descriptor", "certain");
                }
                self.framework_lifecycle
                    .push((owner, target, "framework_lifecycle"));
            }
            Value::Provider(name)
                if self.phase03
                    && matches!(
                        name.as_str(),
                        "pydantic.field_validator"
                            | "pydantic.model_validator"
                            | "pydantic.root_validator"
                            | "pydantic.validator"
                    ) =>
            {
                if let Some(owner) = self.scopes[scope].owner
                    && self.is_class(owner)
                    && self.framework_class_role(owner, &mut BTreeSet::new(), 0)
                        == Some(FrameworkClassRole::PydanticModel)
                {
                    self.framework_lifecycle
                        .push((owner, target, "framework_lifecycle"));
                }
            }
            Value::Provider(name) if self.phase03 && wagtail::is_setting_registrar(&name) => {
                self.register_wagtail_setting(scope, decorator, target, args, keywords);
            }
            Value::Provider(name)
                if self.phase03
                    && matches!(
                        name.as_str(),
                        "django.contrib.admin.register"
                            | "django.contrib.admin.decorators.register"
                    ) =>
            {
                self.django_admin_decorator(scope, target, args, keywords, before, decorator.at);
            }
            Value::Provider(name)
                if self.phase03
                    && matches!(
                        name.as_str(),
                        "django.contrib.admin.display"
                            | "django.contrib.admin.decorators.display"
                            | "django.contrib.admin.action"
                            | "django.contrib.admin.decorators.action"
                    ) =>
            {
                // These decorators attach metadata to the same callback object.
                return true;
            }
            Value::Provider(name) if name == "pytest.fixture" => {
                let registered_name = if let Some((_, expression)) =
                    keywords.iter().find(|(name, _)| name == "name")
                {
                    if let Value::Text(name) = self.identity(scope, expression, before, 0) {
                        Some(name)
                    } else {
                        self.scope_issue(
                            scope,
                            expression.at,
                            "unresolved_fixture_name",
                            "pytest fixture name could not be resolved",
                        );
                        return false;
                    }
                } else {
                    None
                };
                let mut autouse = false;
                if let Some((_, expression)) = keywords.iter().find(|(name, _)| name == "autouse") {
                    match self.identity(scope, expression, before, 0) {
                        Value::Bool(value) => autouse = value,
                        _ => self.scope_issue(
                            scope,
                            expression.at,
                            "unresolved_fixture_autouse",
                            "pytest fixture autouse could not be resolved",
                        ),
                    }
                }
                self.fixtures.insert(
                    target,
                    PytestFixture {
                        name: registered_name,
                        autouse,
                    },
                );
            }
            Value::Callable(invocation) => {
                if self.phase03
                    && !reads_rebound_global
                    && self.apply_atexit_forwarding_decorator(
                        scope, decorator, target, before, invocation,
                    )
                {
                    return true;
                }
                self.invoke(scope, invocation, "decorator", "certain", decorator.at);
                let Binding::Definition(definition, body) = self.symbols[invocation.target].binding
                else {
                    unreachable!()
                };
                if invocation.implicit_descriptor_receiver
                    && !matches!(decorator.kind, ExpressionKind::Call { .. })
                    && field_dispatch::plain_method(definition, 2)
                    && returns_parameter(definition, &definition.parameters[1])
                {
                    return true;
                }
                if is_identity_decorator(definition) {
                    return self.local_identity_decorator(Value::Callable(invocation), decorator);
                } else if self.phase03
                    && !matches!(decorator.kind, ExpressionKind::Call { .. })
                    && self
                        .known_definition_replacement(target, invocation)
                        .is_some()
                {
                    return false;
                } else if self.phase03
                    && definition.kind == DefinitionKind::Function
                    && definition.decorators.is_empty()
                    && definition.simple_positional_parameters
                    && definition.parameters.len()
                        == args.len()
                            + usize::from(matches!(invocation.receiver, Receiver::Bound(_)))
                    && keywords.is_empty()
                    && matches!(decorator.kind, ExpressionKind::Call { .. })
                {
                    if matches!(
                        body_without_docstring(definition),
                        [Statement {
                            kind: StatementKind::Return(Some(Expression {
                                kind: ExpressionKind::IdentityLambda,
                                ..
                            })),
                            ..
                        }]
                    ) {
                        return true;
                    }
                    if matches!(invocation.receiver, Receiver::Ordinary)
                        && let Some(identity) = identity_decorator_factory(definition)
                        && let Some(identity) = self.local(body, &identity.name.text, END)
                    {
                        self.edge(scope, identity, "decorator", "certain");
                        return true;
                    }
                }
                self.scope_issue(
                    scope,
                    decorator.at,
                    "unresolved_decorator",
                    "local decorator may replace or invoke the decorated definition",
                );
            }
            Value::IdentityDecoratorFactory
                if self.phase03 && args.len() == 1 && keywords.is_empty() =>
            {
                return true;
            }
            Value::ConstantFunction => {}
            Value::IdentityFunction
                if self.phase03 && !matches!(decorator.kind, ExpressionKind::Call { .. }) =>
            {
                return true;
            }
            Value::Provider(name)
                if matches!(
                    name.as_str(),
                    "builtins.staticmethod" | "builtins.classmethod"
                ) => {}
            Value::Provider(name)
                if self.phase03
                    && name == "builtins.property"
                    && let Some(owner) = self.scopes[scope].owner
                    && self.is_class(owner)
                    && self
                        .framework_class_role(owner, &mut BTreeSet::new(), 0)
                        .is_some_and(|role| {
                            role.is_unittest() || role == FrameworkClassRole::PydanticModel
                        }) => {}
            Value::Unknown
                if self.phase03 && self.empty_flask_lookalike(scope, function, before) => {}
            value => {
                if self.phase03
                    && matches!(value, Value::Provider(_))
                    && self.opaque_decorator_receives_function(scope, decorator, target, before)
                {
                    self.edge(scope, target, "opaque_decorator_argument", "possible");
                }
                self.scope_issue(
                    scope,
                    decorator.at,
                    "unresolved_decorator",
                    "decorator identity is outside the bounded model",
                );
            }
        };
        false
    }

    fn is_click_command_declaration(
        &self,
        scope: ScopeId,
        decorator: &Expression,
        target: SymbolId,
        name: &str,
    ) -> bool {
        click_command_provider(name)
            && scope == self.scopes[scope].file
            && matches!(
                &decorator.kind,
                ExpressionKind::Call { arguments, keywords, .. }
                    if arguments.is_empty() && keywords.is_empty()
            )
            && matches!(self.symbols[target].binding, Binding::Definition(definition, _)
                if definition.kind == DefinitionKind::Function
                    && !definition.is_generator
                    && definition.decorators.len() == 1)
    }

    fn click_command_replaces_function(
        &mut self,
        scope: ScopeId,
        decorator: &Expression,
        target: SymbolId,
        before: Location,
        depth: usize,
    ) -> bool {
        let ExpressionKind::Call {
            function,
            arguments,
            keywords,
        } = &decorator.kind
        else {
            return false;
        };
        arguments.is_empty()
            && keywords.is_empty()
            && matches!(self.identity(scope, function, before, depth), Value::Provider(name)
                if self.is_click_command_declaration(scope, decorator, target, &name))
    }

    fn opaque_decorator_receives_function(
        &mut self,
        scope: ScopeId,
        decorator: &Expression,
        target: SymbolId,
        before: Location,
    ) -> bool {
        let ExpressionKind::Name(name) = &decorator.kind else {
            return false;
        };
        if !self
            .local(scope, name, before)
            .is_some_and(|id| matches!(self.symbols[id].binding, Binding::Import(_)))
        {
            return false;
        }
        self.decorator_receives_function(scope, decorator, target, before)
    }

    fn apply_atexit_forwarding_decorator(
        &mut self,
        scope: ScopeId,
        decorator: &Expression,
        target: SymbolId,
        before: Location,
        invocation: Invocation,
    ) -> bool {
        let provider_scope = self.symbols[invocation.target].scope;
        if scope != self.scopes[scope].file
            || provider_scope != self.scopes[provider_scope].file
            || self.failed_terminal_modules.contains(&provider_scope)
            || self.branch_depth != 0
            || invocation.verified
            || !matches!(decorator.kind, ExpressionKind::Name(_))
        {
            return false;
        }
        let Some((definition, _)) = self.bounded_replacement_function(invocation) else {
            return false;
        };
        let [
            Statement {
                kind: StatementKind::Evaluate(call),
                ..
            },
            Statement {
                kind: StatementKind::Return(Some(result)),
                ..
            },
        ] = body_without_docstring(definition)
        else {
            return false;
        };
        let ExpressionKind::Call {
            function,
            arguments,
            keywords,
        } = &call.kind
        else {
            return false;
        };
        let callback = &definition.parameters[0].text;
        let ExpressionKind::Name(registrar) = &function.kind else {
            return false;
        };
        if definition.is_generator
            || registrar == callback
            || !keywords.is_empty()
            || !matches!(arguments.as_slice(), [argument]
                if matches!(&argument.kind, ExpressionKind::Name(name) if name == callback))
            || !matches!(&result.kind, ExpressionKind::Name(name) if name == callback)
            || !matches!(self.symbols[target].binding, Binding::Definition(callback, _)
                if callback.kind == DefinitionKind::Function && !callback.is_generator
                    && callback.parameters.is_empty())
        {
            return false;
        }
        let provider_before = if provider_scope == scope { before } else { END };
        let module = &self.files[provider_scope].module;
        if self
            .module_member_write(module, registrar, (scope, before))
            .is_some()
            || !matches!(self.probe_identity(provider_scope, function, provider_before),
                Value::Provider(name) if name == "atexit.register")
            || !self.decorator_receives_function(scope, decorator, target, before)
        {
            return false;
        }
        // The wrapper binds and returns its argument from the same frame.
        // The registered callback can run later, after module initialization.
        self.edge(
            scope,
            invocation.target,
            mcp_tools::DECORATOR_APPLICATION,
            "certain",
        );
        self.retain_atexit_callback(
            scope,
            Value::Callable(Invocation::ordinary(target)),
            decorator.at,
        )
    }

    fn decorator_receives_function(
        &mut self,
        scope: ScopeId,
        decorator: &Expression,
        target: SymbolId,
        before: Location,
    ) -> bool {
        let Binding::Definition(definition, _) = self.symbols[target].binding else {
            return false;
        };
        if definition.kind == DefinitionKind::Class {
            return false;
        }
        let Some(layer) = definition
            .decorators
            .iter()
            .position(|item| std::ptr::eq(item, decorator))
        else {
            return false;
        };
        // Inner decorators apply first. Only pass-through layers can prove
        // that the opaque decorator receives this function.
        definition.decorators[layer + 1..].iter().all(|inner| {
            let ExpressionKind::Call {
                function,
                arguments,
                keywords,
            } = &inner.kind
            else {
                let value = self.probe_identity(scope, inner, before);
                return matches!(value, Value::IdentityFunction)
                    || self.local_identity_decorator(value, inner);
            };
            if !keywords.is_empty()
                || !arguments.iter().all(|argument| {
                    matches!(
                        argument.kind,
                        ExpressionKind::None
                            | ExpressionKind::Bool(_)
                            | ExpressionKind::Text(_)
                            | ExpressionKind::Scalar(_)
                    )
                })
            {
                return false;
            }
            let Value::Callable(invocation) = self.probe_identity(scope, function, before) else {
                return false;
            };
            let Binding::Definition(factory, _) = self.symbols[invocation.target].binding else {
                return false;
            };
            !invocation.verified
                && matches!(invocation.receiver, Receiver::Ordinary | Receiver::Bound(_))
                && factory.kind == DefinitionKind::Function
                && !factory.is_generator
                && factory.decorators.is_empty()
                && factory.simple_positional_parameters
                && factory.parameters.len()
                    == arguments.len()
                        + usize::from(matches!(invocation.receiver, Receiver::Bound(_)))
                && matches!(
                    body_without_docstring(factory),
                    [Statement {
                        kind: StatementKind::Return(Some(Expression {
                            kind: ExpressionKind::IdentityLambda,
                            ..
                        })),
                        ..
                    }]
                )
        })
    }

    fn local_identity_decorator(&self, value: Value, decorator: &Expression) -> bool {
        let Value::Callable(invocation) = value else {
            return false;
        };
        let Binding::Definition(definition, _) = self.symbols[invocation.target].binding else {
            return false;
        };
        matches!(invocation.receiver, Receiver::Ordinary)
            && !matches!(decorator.kind, ExpressionKind::Call { .. })
            && definition.kind == DefinitionKind::Function
            && definition.has_positional_parameter
            && definition.decorators.is_empty()
            && is_identity_decorator(definition)
    }

    fn known_definition_replacement(
        &self,
        target: SymbolId,
        invocation: Invocation,
    ) -> Option<Value> {
        let symbol = &self.symbols[target];
        let Binding::Definition(decorated, _) = symbol.binding else {
            return None;
        };
        if decorated.decorators.len() != 1 {
            return None;
        }
        let (definition, _) = self.bounded_replacement_function(invocation)?;
        if let [
            Statement {
                kind: StatementKind::Return(Some(value)),
                ..
            },
        ] = body_without_docstring(definition)
        {
            match &value.kind {
                ExpressionKind::ConstantLambda => return Some(Value::ConstantFunction),
                ExpressionKind::NullaryConstantLambda => {
                    return Some(Value::NullaryConstantFunction);
                }
                ExpressionKind::Name(name)
                    if decorated.kind == DefinitionKind::Function
                        && !invocation.verified
                        && !invocation.implicit_descriptor_receiver
                        && symbol.scope == self.symbols[invocation.target].scope
                        && self.scopes[symbol.scope].owner.is_none()
                        && definition.parameters[0].text != *name
                        && matches!(decorated.decorators[0].kind, ExpressionKind::Name(_))
                        && decorated.defaults.is_empty()
                        && decorated.annotations.is_empty() =>
                {
                    return self.closed_decorator_class_return(symbol.scope, decorated, name);
                }
                ExpressionKind::Name(name)
                    if decorated.kind == DefinitionKind::Class
                        && symbol.scope == self.symbols[invocation.target].scope
                        && self.scopes[symbol.scope].owner.is_none()
                        && definition.parameters[0].text != *name
                        && decorated.bases.is_empty()
                        && decorated.table_keyword.is_none()
                        && decorated.metaclass.is_none()
                        && body_without_docstring(decorated).iter().all(|item| {
                            match &item.kind {
                                StatementKind::Define(method) => method.kind != DefinitionKind::Class
                                    && method.decorators.is_empty()
                                    && method.defaults.is_empty() && method.annotations.is_empty(),
                                StatementKind::Assign {
                                    names,
                                    value: Some(value),
                                    annotation: None,
                                } => matches!(names.as_slice(), [name] if !name.text.starts_with("__"))
                                    && matches!(value.kind, ExpressionKind::Text(_)
                                        | ExpressionKind::Bool(_) | ExpressionKind::None
                                        | ExpressionKind::Scalar(_)),
                                _ => false,
                            }
                        }) =>
                {
                    // The new class cannot invoke a hook or change the module
                    // binding before this decorator reads its return value.
                    let replacement = self.local(symbol.scope, name, decorated.name.at)?;
                    if !self.binding_depends_on_branch(replacement, decorated.name.at)
                        && matches!(self.symbols[replacement].binding,
                            Binding::Definition(class, _) if class.kind == DefinitionKind::Class
                                && class.decorators.is_empty())
                    {
                        return Some(Value::Callable(Invocation::ordinary(replacement)));
                    }
                }
                _ => {}
            }
        }
        if decorated.kind == DefinitionKind::Class
            || self.scopes[symbol.scope]
                .owner
                .is_some_and(|owner| !self.is_class(owner))
        {
            return None;
        }
        self.local_replacement_decorator(invocation)
    }

    fn closed_decorator_class_return(
        &self,
        scope: ScopeId,
        decorated: &Definition,
        name: &str,
    ) -> Option<Value> {
        let replacement = self.local(scope, name, decorated.name.at)?;
        let Binding::Definition(class, _) = self.symbols[replacement].binding else {
            return None;
        };
        if class.kind != DefinitionKind::Class
            || !class.decorators.is_empty()
            || !class.bases.is_empty()
            || class.has_class_keywords()
            || self.scopes[scope].open_wildcard
            || self.binding_depends_on_branch(replacement, decorated.name.at)
        {
            return None;
        }
        // Only declarations without initializer effects can occur between
        // binding this class and evaluating the decorated function.
        let decorator_line = decorated.decorators[0].at.line;
        let unmodified = self.scopes[scope]
            .body
            .iter()
            .take_while(|statement| statement.at.line < decorator_line)
            .filter(|statement| statement.at.line > class.name.at.line)
            .all(|statement| match &statement.kind {
                StatementKind::Define(definition) => {
                    definition.kind == DefinitionKind::Function
                        && definition.defaults.is_empty()
                        && definition.annotations.is_empty()
                        && definition.decorators.is_empty()
                }
                StatementKind::Evaluate(expression) => {
                    matches!(expression.kind, ExpressionKind::Text(_))
                }
                _ => false,
            });
        unmodified.then_some(Value::Callable(Invocation::ordinary(replacement)))
    }

    fn bounded_replacement_function(
        &self,
        invocation: Invocation,
    ) -> Option<(&'a Definition, ScopeId)> {
        let Binding::Definition(definition, body) = self.symbols[invocation.target].binding else {
            return None;
        };
        (matches!(invocation.receiver, Receiver::Ordinary)
            && definition.kind == DefinitionKind::Function
            && definition.decorators.is_empty()
            && definition.simple_positional_parameters
            && definition.parameters.len() == 1)
            .then_some((definition, body))
    }

    fn local_replacement_decorator(&self, invocation: Invocation) -> Option<Value> {
        let (definition, body) = self.bounded_replacement_function(invocation)?;
        if matches!(
            body_without_docstring(definition),
            [Statement {
                kind: StatementKind::Return(Some(Expression {
                    kind: ExpressionKind::NullaryConstantLambda,
                    ..
                })),
                ..
            }]
        ) {
            return Some(Value::NullaryConstantFunction);
        }
        let replacement = returned_nested_function(definition)?;
        // Returning a literal or a parameter cannot capture the decorated
        // callback or call its body.
        let [
            Statement {
                kind: StatementKind::Return(value),
                ..
            },
        ] = body_without_docstring(replacement)
        else {
            return None;
        };
        if let Some(value) = value {
            match &value.kind {
                ExpressionKind::None
                | ExpressionKind::Bool(_)
                | ExpressionKind::Text(_)
                | ExpressionKind::Scalar(_) => {}
                ExpressionKind::Name(name)
                    if replacement.parameters.iter().any(|arg| arg.text == *name) => {}
                _ => return None,
            }
        }
        self.local(body, &replacement.name.text, END)
            .map(|replacement| Value::Callable(Invocation::ordinary(replacement)))
    }

    fn empty_flask_lookalike(
        &mut self,
        scope: ScopeId,
        expression: &Expression,
        before: Location,
    ) -> bool {
        let ExpressionKind::Member(base, name) = &expression.kind else {
            return false;
        };
        if !is_route(ObjectKind::FlaskApp, name, true)
            && !is_route(ObjectKind::FlaskBlueprint, name, true)
            && !flask_login_callback(name)
        {
            return false;
        }
        // A resolved empty local class has no registration method to invoke.
        // Unresolved receivers and class hierarchies carry the normal diagnostic.
        matches!(self.identity(scope, base, before, 0), Value::Instance(class)
            if matches!(self.symbols[class].binding, Binding::Definition(definition, _)
                if definition.body.is_empty() && definition.bases.is_empty()
                    && definition.decorators.is_empty()))
    }

    fn class_lineage(&mut self, class: SymbolId) -> (Vec<ScopeId>, bool) {
        let mut lineage = Vec::new();
        let mut current = class;
        let mut resolved = false;
        for _ in 0..MAX_BINDING_CHAIN {
            let symbol = &self.symbols[current];
            let Binding::Definition(definition, scope) = symbol.binding else {
                unreachable!()
            };
            lineage.push(scope);
            let base = match definition.bases.as_slice() {
                [] => {
                    resolved = true;
                    break;
                }
                [base] => base,
                _ => break,
            };
            match self.class_base(symbol.scope, base, definition.name.at, 0) {
                Value::Callable(Invocation { target: parent, .. }) if self.is_class(parent) => {
                    current = parent
                }
                Value::Provider(name) if name == "builtins.object" => {
                    resolved = true;
                    break;
                }
                _ => break,
            }
        }
        (lineage, resolved)
    }

    fn pytest_class_names(&mut self, class: SymbolId) -> Option<(PytestNames<'a>, bool)> {
        let symbol = &self.symbols[class];
        if symbol.scope != self.scopes[symbol.scope].file || !self.is_class(class) {
            return None;
        }
        let (lineage, resolved) = self.class_lineage(class);
        // Pytest sees the effective attributes of the collected class, so a
        // subclass can shadow both a test method and a fixture of its base.
        let mut names = PytestNames::new();
        for scope in lineage.into_iter().rev() {
            for (&name, bindings) in &self.scopes[scope].names {
                names.entry(name).or_default().extend(bindings);
            }
        }
        let mut certain = true;
        for name in ["__init__", "__new__", "__test__"] {
            let Some(bindings) = names.get(name) else {
                continue;
            };
            let candidates = self.effective_bindings(bindings);
            let disables_collection = |id: SymbolId| {
                name != "__test__"
                    || matches!(self.symbols[id].binding, Binding::Variable(Some(value), _) if static_bool(value) == Some(false))
            };
            if candidates.len() == 1
                && !self.conditional.contains_key(&candidates[0])
                && disables_collection(candidates[0])
            {
                return None;
            }
            certain &= !candidates.iter().copied().any(disables_collection);
        }
        if !resolved {
            // Unresolved bases can supply tests and fixtures even when the
            // collected class declares none of its own.
            let symbol = &self.symbols[class];
            self.scope_issue(
                symbol.scope,
                symbol.name.at,
                "unresolved_pytest_class",
                "pytest test collection through this class hierarchy could not be resolved",
            );
            return None;
        }
        Some((names, certain))
    }

    fn is_pytest_function(&self, id: SymbolId) -> bool {
        matches!(self.symbols[id].binding, Binding::Definition(def, _) if def.kind != DefinitionKind::Class)
            && !self.fixtures.contains_key(&id)
    }

    fn is_coroutine(&self, id: SymbolId) -> bool {
        matches!(self.symbols[id].binding, Binding::Definition(definition, _)
            if definition.kind == DefinitionKind::Coroutine)
    }

    fn is_generator(&self, id: SymbolId) -> bool {
        matches!(self.symbols[id].binding, Binding::Definition(definition, _)
            if definition.is_generator)
    }

    fn bounded_unittest_comparison(
        &mut self,
        scope: ScopeId,
        arguments: &[Expression],
        keywords: &[(String, Expression)],
        before: Location,
    ) -> bool {
        let Some([Some(left), Some(right), message]) =
            registration_arguments(arguments, keywords, ["first", "second", "msg"])
        else {
            return false;
        };
        [Some(left), Some(right), message].into_iter().flatten().all(|expr| {
            if self.probe_identity(scope, expr, before).is_builtin_scalar() {
                return true;
            }
            let ExpressionKind::Call { function, arguments, keywords } = &expr.kind else {
                return false;
            };
            if !arguments.is_empty() || !keywords.is_empty() {
                return false;
            }
            let Value::Callable(invocation) = self.probe_identity(scope, function, before) else {
                return false;
            };
            let Binding::Definition(definition, _) = self.symbols[invocation.target].binding else {
                return false;
            };
            definition.kind == DefinitionKind::Function
                && !definition.is_generator
                && definition.parameters.is_empty()
                && definition.decorators.is_empty()
                && matches!(
                    definition.body.as_slice(),
                    [Statement { kind: StatementKind::Return(Some(result)), .. }]
                        if matches!(result.kind, ExpressionKind::Text(_) | ExpressionKind::Scalar(_) | ExpressionKind::Bool(_))
                )
        })
    }

    fn unittest_property(&mut self, id: SymbolId) -> bool {
        let symbol = &self.symbols[id];
        let Binding::Definition(definition, _) = symbol.binding else {
            return false;
        };
        let scope = symbol.scope;
        let [decorator] = definition.decorators.as_slice() else {
            return false;
        };
        definition.kind != DefinitionKind::Class
            && self.phase03
            && self.scopes[scope].owner.is_some_and(|class| {
                self.is_class(class)
                    && self
                        .framework_class_role(class, &mut BTreeSet::new(), 0)
                        .is_some_and(FrameworkClassRole::is_unittest)
            })
            && matches!(
                self.probe_identity(scope, decorator, definition.name.at),
                Value::Provider(provider) if provider == "builtins.property"
            )
    }

    fn is_pytest_file(&self, file: usize) -> bool {
        let filename = self.files[file].path.rsplit('/').next().unwrap_or_default();
        filename.starts_with("test_") || filename.ends_with("_test.py")
    }

    fn effective_bindings<'b>(&self, bindings: &'b [SymbolId]) -> &'b [SymbolId] {
        // Conditional attributes may leave a previous binding in place.
        let first = bindings
            .iter()
            .rposition(|id| !self.conditional.contains_key(id))
            .unwrap_or(0);
        &bindings[first..]
    }

    fn probe_callable_binding(&self, id: SymbolId) -> Option<CallableBinding> {
        let mut bindings = Vec::new();
        let Value::Callable(Invocation { target, .. }) =
            self.pytest_binding(id, None, &mut bindings, 0)?
        else {
            return None;
        };
        Some(CallableBinding { target, bindings })
    }

    // Collect identity evidence without marking unrelated namespace entries
    // as used. Only a test or fixture that is retained consumes this evidence.
    fn pytest_binding(
        &self,
        id: SymbolId,
        before: Option<Location>,
        bindings: &mut Vec<SymbolId>,
        depth: usize,
    ) -> Option<Value> {
        if depth >= MAX_BINDING_CHAIN
            || before.is_some_and(|at| self.binding_depends_on_branch(id, at))
        {
            return None;
        }
        let symbol = &self.symbols[id];
        match symbol.binding {
            Binding::Definition(_, _) => Some(Value::Callable(Invocation::ordinary(id))),
            Binding::Variable(Some(value), at) => {
                bindings.push(id);
                self.pytest_expression(symbol.scope, value, at, bindings, depth + 1)
            }
            Binding::Import(import) => {
                bindings.push(id);
                let source = self.scopes[symbol.scope].file;
                let module = self.import_module(source, import)?;
                if let Some(member) = &import.member {
                    if let Some(write) =
                        self.module_member_write(&module, member, (symbol.scope, import.local.at))
                    {
                        return match write {
                            mutations::MemberWriteValue::Replacement(value) => Some(value),
                            _ => None,
                        };
                    }
                    let file = *self.modules.get(module.as_str())?;
                    let before = self
                        .import_member_position(symbol.scope, &module, import)
                        .unwrap_or(END);
                    let selected = self.selected_module_member(
                        file,
                        member,
                        before,
                        (symbol.scope, import.local.at),
                        ModuleMemberAccess::FromImport,
                    );
                    self.pytest_member(selected, before, bindings, depth + 1)
                } else {
                    let visible = self.visible_import_module(source, import, &module);
                    self.modules
                        .get(visible.as_ref())
                        .copied()
                        .map(Value::Module)
                }
            }
            _ => None,
        }
    }

    fn pytest_expression(
        &self,
        scope: ScopeId,
        expression: &Expression,
        before: Location,
        bindings: &mut Vec<SymbolId>,
        depth: usize,
    ) -> Option<Value> {
        if depth >= MAX_BINDING_CHAIN {
            return None;
        }
        match &expression.kind {
            ExpressionKind::Name(name) => {
                let id = self.lexical(scope, name, before)?;
                let before = if self.symbols[id].scope == scope {
                    before
                } else {
                    END
                };
                self.pytest_binding(id, Some(before), bindings, depth)
            }
            ExpressionKind::Member(base, member) => {
                let Value::Module(file) =
                    self.pytest_expression(scope, base, before, bindings, depth + 1)?
                else {
                    return None;
                };
                if let Some(write) =
                    self.module_member_write(&self.files[file].module, member, (scope, before))
                {
                    return match write {
                        mutations::MemberWriteValue::Replacement(value) => Some(value),
                        _ => None,
                    };
                }
                let selected = self.selected_module_member(
                    file,
                    member,
                    END,
                    (scope, before),
                    ModuleMemberAccess::Attribute,
                );
                self.pytest_member(selected, END, bindings, depth + 1)
            }
            _ => None,
        }
    }

    fn pytest_member(
        &self,
        member: ModuleMember<'a>,
        before: Location,
        bindings: &mut Vec<SymbolId>,
        depth: usize,
    ) -> Option<Value> {
        match member {
            ModuleMember::Binding(id, import) => {
                bindings.extend(import);
                self.pytest_binding(id, Some(before), bindings, depth)
            }
            ModuleMember::Submodule(file, imports) => {
                bindings.extend(imports);
                Some(Value::Module(file))
            }
            ModuleMember::Lazy(hook, branch) => {
                bindings.push(hook);
                self.pytest_expression(branch.scope, branch.value, branch.at, bindings, depth)
            }
            _ => None,
        }
    }

    fn pytest_registrations(
        &self,
        names: &PytestNames<'a>,
        prefix: &str,
    ) -> Vec<(CallableBinding, bool)> {
        names
            .iter()
            .filter(|(name, _)| name.starts_with(prefix))
            .flat_map(|(_, bindings)| self.callable_candidates(bindings))
            .collect()
    }

    fn callable_candidates(&self, bindings: &[SymbolId]) -> Vec<(CallableBinding, bool)> {
        let candidates = self.effective_bindings(bindings);
        candidates
            .iter()
            .filter_map(|id| {
                let certain = candidates.len() == 1 && !self.conditional.contains_key(id);
                self.probe_callable_binding(*id)
                    .map(|target| (target, certain))
            })
            .collect()
    }

    fn test_edges(&mut self) -> Vec<Edge> {
        let mut roots = BTreeMap::new();
        let conftests: BTreeMap<_, _> = self
            .files
            .iter()
            .enumerate()
            .filter_map(|(file, source)| {
                pytest_conftest_prefix(&source.path).map(|prefix| (prefix, file))
            })
            .collect();
        for file in 0..self.files.len() {
            if !self.is_pytest_file(file) {
                continue;
            }
            let path = &self.files[file].path;
            let prefixes =
                std::iter::once("").chain(path.match_indices('/').map(|(end, _)| &path[..=end]));
            let ancestors = prefixes.filter_map(|prefix| conftests.get(prefix).copied());
            let namespaces: Vec<_> = ancestors.chain(std::iter::once(file)).collect();
            let module_tests = self.pytest_registrations(&self.scopes[file].names, "test_");
            self.record_pytest_tests(file, &namespaces, None, module_tests, &mut roots);
            if !self.fixtures.is_empty()
                && let Some(cases) = self.pytest_unittest_cases.remove(&file)
            {
                for (names, certain_case) in cases {
                    // pytest runs autouse fixtures for unittest cases but does
                    // not inject fixtures through unittest method parameters.
                    self.collect_pytest_fixtures(
                        file,
                        &namespaces,
                        Some(&names),
                        PytestFixtureRequests {
                            tests: &[],
                            certain_case,
                        },
                        &mut roots,
                    );
                }
            }
            for (class, collected) in self.pytest_registrations(&self.scopes[file].names, "Test") {
                if self.phase03 && self.unittest_classes.contains_key(&class.target) {
                    continue;
                }
                let Some((names, eligible)) = self.pytest_class_names(class.target) else {
                    continue;
                };
                let tests: Vec<_> = self
                    .pytest_registrations(&names, "test_")
                    .into_iter()
                    .filter(|(method, _)| self.is_pytest_function(method.target))
                    .map(|(method, certain)| (method, certain && collected && eligible))
                    .collect();
                if !tests.is_empty() {
                    *roots.entry(class.target).or_default() |=
                        tests.iter().any(|(_, certain)| *certain);
                    self.read.extend(class.bindings);
                    self.record_pytest_tests(file, &namespaces, Some(&names), tests, &mut roots);
                }
            }
        }
        roots
            .into_iter()
            .map(|(target, certain)| Edge {
                target,
                kind: "pytest_root",
                certainty: if certain { "certain" } else { "possible" },
            })
            .collect()
    }

    fn record_pytest_tests(
        &mut self,
        file: usize,
        namespaces: &[ScopeId],
        class: Option<&PytestNames<'a>>,
        tests: Vec<(CallableBinding, bool)>,
        roots: &mut BTreeMap<SymbolId, bool>,
    ) {
        let tests: Vec<_> = tests
            .into_iter()
            .filter_map(|(test, certain)| {
                self.is_pytest_function(test.target)
                    .then_some((test, certain))
            })
            .collect();
        if tests.is_empty() {
            return;
        }
        for (test, certain) in &tests {
            *roots.entry(test.target).or_default() |= *certain;
            self.read.extend(&test.bindings);
        }
        if !self.fixtures.is_empty() {
            self.collect_pytest_fixtures(
                file,
                namespaces,
                class,
                PytestFixtureRequests {
                    tests: &tests,
                    certain_case: tests.iter().any(|(_, certain)| *certain),
                },
                roots,
            );
        }
    }

    fn collect_pytest_fixtures(
        &mut self,
        file: usize,
        namespaces: &[ScopeId],
        class: Option<&PytestNames<'a>>,
        requests: PytestFixtureRequests<'_>,
        roots: &mut BTreeMap<SymbolId, bool>,
    ) {
        // Register from outermost to innermost. Pytest requests an autouse
        // fixture by name, so a closer override can satisfy that request too.
        let mut registrations = BTreeMap::<String, Vec<PytestFixtureChoices>>::new();
        for (plugin, names) in self
            .pytest_plugins
            .iter()
            .filter(|scope| !namespaces.contains(scope))
            .map(|&scope| (true, &self.scopes[scope].names))
            .chain(
                namespaces
                    .iter()
                    .map(|&scope| (false, &self.scopes[scope].names)),
            )
            .chain(class.map(|names| (false, names)))
        {
            for (&alias, bindings) in names {
                // Keep possible conditional bindings at the same precedence.
                let mut alternatives = BTreeMap::<&str, Vec<_>>::new();
                let candidates = self.effective_bindings(bindings);
                let has_unconditional_binding = !self.conditional.contains_key(&candidates[0]);
                for &binding in candidates.iter().rev() {
                    if let Some(resolved) = self.probe_callable_binding(binding)
                        && let Some(fixture) = self.fixtures.get(&resolved.target)
                    {
                        let name = fixture.name.as_deref().unwrap_or(alias);
                        alternatives.entry(name).or_default().push(resolved);
                    }
                }
                for (name, alternatives) in alternatives {
                    let may_be_absent =
                        !has_unconditional_binding || alternatives.len() != candidates.len();
                    let choices = registrations.entry(name.into()).or_default();
                    // A local fixture can shadow globally registered plugins.
                    // Keep competing plugin bindings possible when their load order
                    // is not established by collection.
                    if plugin && let Some(choice) = choices.last_mut() {
                        choice.alternatives.extend(alternatives);
                        choice.may_be_absent &= may_be_absent;
                    } else {
                        choices.push(PytestFixtureChoices {
                            alternatives,
                            may_be_absent,
                        });
                    }
                }
            }
        }

        let mut pending = Vec::new();
        for (name, choices) in &registrations {
            let autouse = choices
                .iter()
                .flat_map(|choice| &choice.alternatives)
                .any(|registration| self.fixtures[&registration.target].autouse);
            if autouse {
                let certain = choices.iter().any(|choice| {
                    !choice.may_be_absent
                        && choice
                            .alternatives
                            .iter()
                            .all(|registration| self.fixtures[&registration.target].autouse)
                });
                pending.push((
                    name.as_str(),
                    BTreeMap::new(),
                    certain && requests.certain_case,
                ));
            }
        }
        for (test, certain) in requests.tests {
            let Binding::Definition(definition, _) = self.symbols[test.target].binding else {
                continue;
            };
            for parameter in &definition.parameters {
                if let Some((name, _)) = registrations.get_key_value(&parameter.text) {
                    pending.push((name.as_str(), BTreeMap::new(), *certain));
                }
            }
        }
        let mut visited = BTreeSet::new();
        while let Some((name, active, mut certain)) = pending.pop() {
            if !visited.insert((name, active.clone(), certain)) {
                continue;
            }
            if visited.len() > MAX_PYTEST_REQUEST_STATES || active.len() > MAX_BINDING_CHAIN {
                self.issue(
                    file,
                    Location { line: 1, column: 1 },
                    "analysis_limit",
                    "pytest fixture request limit exceeded",
                );
                break;
            }
            let choices = &registrations[name];
            let upper_bound = active.get(name).copied().unwrap_or(choices.len());
            for (index, choice) in choices[..upper_bound].iter().enumerate().rev() {
                // The request chain can already include this fixture name,
                // even when the immediate requester has a different name.
                let mut nested = active.clone();
                if choices.len() > 1 {
                    nested.insert(name, index);
                }
                let selected_certain =
                    certain && !choice.may_be_absent && choice.alternatives.len() == 1;
                for registration in &choice.alternatives {
                    *roots.entry(registration.target).or_default() |= selected_certain;
                    self.read.extend(&registration.bindings);
                    let Binding::Definition(definition, _) =
                        self.symbols[registration.target].binding
                    else {
                        continue;
                    };
                    for parameter in &definition.parameters {
                        if let Some((name, _)) = registrations.get_key_value(&parameter.text) {
                            pending.push((name.as_str(), nested.clone(), selected_certain));
                        }
                    }
                }
                if !choice.may_be_absent {
                    break;
                }
                certain = false;
            }
        }
    }

    fn retain_exports(
        &mut self,
        exports: Vec<Export>,
        initialization_edges: &[Vec<ScopeId>],
        certainty: &'static str,
    ) {
        let mut pending: VecDeque<_> = exports
            .into_iter()
            .map(|export| (export, certainty))
            .collect();
        let mut modules = BTreeSet::new();
        let mut exported_constructors = BTreeSet::new();
        let mut visited_instances = BTreeSet::new();
        let mut visited_namespaces = BTreeSet::new();
        let mut visited_namespace_copies = BTreeSet::new();
        while let Some((
            Export {
                scope,
                at,
                kind,
                value,
            },
            certainty,
        )) = pending.pop_front()
        {
            let visit_scope = (kind == "local_namespace").then_some(scope);
            let members = match value {
                Value::Callable(invocation) => {
                    self.invoke(scope, invocation, kind, certainty, at);
                    if kind == "local_namespace" {
                        self.uncertain(
                            scope,
                            at,
                            "a callable may escape through a function namespace",
                        );
                    }
                    let class = invocation.target;
                    if self.is_class(class) && exported_constructors.insert((visit_scope, class)) {
                        self.constructor(scope, class, "constructor", "possible", at);
                        vec![Value::Instance(class)]
                    } else {
                        Vec::new()
                    }
                }
                Value::Coroutine(_) if kind == "local_namespace" => {
                    self.expose(scope, value, kind, at);
                    Vec::new()
                }
                Value::DeferredLambda => {
                    self.report_unmodeled_deferred_value(scope, at, &value);
                    Vec::new()
                }
                Value::NamespaceParameter(parameter) => {
                    let default = self.namespace_parameter_default(parameter);
                    let symbol = &self.symbols[parameter];
                    let owner = self.scopes[symbol.scope]
                        .owner
                        .expect("parameter belongs to a definition");
                    let mut callers = self
                        .direct_arguments
                        .get(&owner)
                        .into_iter()
                        .chain(self.executor_arguments.get(&owner))
                        .flatten()
                        .peekable();
                    if callers.peek().is_some() {
                        pending.extend(callers.map(|call| {
                            // Use the call site's scope so that an inactive
                            // caller cannot make its argument reachable.
                            let value = call
                                .values
                                .get(&symbol.name.text)
                                .cloned()
                                .or_else(|| default.clone())
                                .unwrap_or(Value::Unknown);
                            (
                                Export {
                                    scope: call.caller,
                                    at: call.at,
                                    kind,
                                    value,
                                },
                                "possible",
                            )
                        }));
                    } else if self.direct.contains(&owner) {
                        // This call shape has no bounded argument mapping.
                        self.scope_issue(
                            scope,
                            at,
                            "unresolved_export",
                            "a project argument in an escaping namespace could not be resolved",
                        );
                    }
                    default.into_iter().collect()
                }
                Value::Instance(class) if visited_instances.insert((visit_scope, class)) => {
                    self.exported_class_members(class, scope, at)
                }
                Value::Module(file) if modules.insert((visit_scope, file)) => {
                    self.exported_module_members(file, initialization_edges)
                }
                Value::LocalNamespace(owner, first)
                    if visited_namespaces.insert((scope, owner, first, position(at))) =>
                {
                    self.namespace_export_values(owner, first, scope, at, None)
                }
                Value::NamespaceCopy(copy)
                    if visited_namespace_copies.insert((scope, position(at), copy.site)) =>
                {
                    let mut values = self.namespace_export_values(
                        copy.owner,
                        copy.first,
                        scope,
                        at,
                        Some(&copy),
                    );
                    values.extend(copy.keywords.into_values());
                    values
                }
                Value::Sequence(items) | Value::Mapping(items) | Value::Alternatives(items) => {
                    items
                }
                Value::Unknown | Value::OpaqueNamespace => {
                    self.scope_issue(
                        scope,
                        at,
                        "unresolved_export",
                        if kind == "local_namespace" {
                            "an escaping function namespace contains an unresolved value"
                        } else {
                            "a public member could not be resolved"
                        },
                    );
                    Vec::new()
                }
                _ => Vec::new(),
            };
            pending.extend(members.into_iter().map(|value| {
                (
                    Export {
                        scope,
                        at,
                        kind,
                        value,
                    },
                    "possible",
                )
            }));
        }
    }

    fn namespace_export_values(
        &mut self,
        owner: ScopeId,
        first: [usize; 2],
        consumer: ScopeId,
        at: Location,
        copy: Option<&NamespaceCopy>,
    ) -> Vec<Value> {
        let reads = &self.scopes[owner].namespace_reads;
        let Some(initial) = reads.get(&first) else {
            return Vec::new();
        };
        let escape_at = position(at);
        let escape_exclusions = (owner == consumer)
            .then(|| {
                self.scopes[consumer]
                    .namespace_escape_exclusions
                    .get(&escape_at)
            })
            .flatten()
            .map_or(&[][..], Vec::as_slice);
        let excludes = |spans: &[RangeInclusive<_>], at| spans.iter().any(|span| span.contains(at));
        let copied_at = copy
            .filter(|copy| copy.copied_at.0 == owner)
            // The namespace expression can be an argument of the copy call.
            .map(|copy| copy.copied_at.1.max(first));
        // A later same-frame call can refresh the original dictionary.
        // Independent copies keep their contents at the time of the copy.
        let bindings: BTreeSet<_> = reads
            .range(first..)
            .take_while(|(candidate, _)| copied_at.is_none_or(|at| **candidate <= at))
            .filter(|(candidate, read)| {
                !excludes(&initial.exclusions, candidate)
                    && !excludes(escape_exclusions, candidate)
                    && !excludes(&read.exclusions, &first)
                    && (owner != consumer || !excludes(&read.exclusions, &escape_at))
            })
            .flat_map(|(_, read)| read.bindings.iter().copied())
            .filter(|&id| {
                copy.is_none_or(|copy| !copy.keywords.contains_key(&self.symbols[id].name.text))
            })
            .collect();
        bindings
            .into_iter()
            .map(|id| {
                let value = self.binding_value(id, 0);
                if matches!(value, Value::Unknown)
                    && matches!(self.symbols[id].binding, Binding::Parameter)
                {
                    Value::NamespaceParameter(id)
                } else {
                    value
                }
            })
            .collect()
    }

    fn namespace_parameter_default(&mut self, parameter: SymbolId) -> Option<Value> {
        let symbol = &self.symbols[parameter];
        let owner = self.scopes[symbol.scope].owner?;
        let function = &self.symbols[owner];
        let Binding::Definition(definition, _) = function.binding else {
            return None;
        };
        if definition.defaults.is_empty() {
            return None;
        }
        if !definition.simple_positional_parameters {
            return Some(Value::Unknown);
        }
        // Simple positional-or-keyword defaults occupy the final parameters.
        let first_default = definition.parameters.len() - definition.defaults.len();
        let index = definition.parameters[first_default..]
            .iter()
            .position(|candidate| candidate.text == symbol.name.text)?;
        Some(self.identity(
            function.scope,
            &definition.defaults[index],
            definition.name.at,
            0,
        ))
    }

    fn exported_class_members(
        &mut self,
        class: SymbolId,
        scope: ScopeId,
        at: Location,
    ) -> Vec<Value> {
        let (lineage, resolved) = self.class_lineage(class);
        if !resolved {
            self.scope_issue(
                scope,
                at,
                "unresolved_export",
                "public methods of the exported class could not be bounded",
            );
        }
        let names: BTreeSet<_> = lineage
            .into_iter()
            .flat_map(|scope| self.scopes[scope].names.keys().copied())
            .filter(|name| *name == "__call__" || !name.starts_with('_'))
            .collect();
        names
            .into_iter()
            .filter_map(|name| {
                let value = self.member(Value::Instance(class), name, (scope, at), 0);
                if name != "__call__" {
                    return Some(value);
                }
                // Calling the instance invokes its effective __call__ member.
                // A noncallable override does not expose the members of its value.
                match value {
                    Value::Callable(invocation)
                        if !invocation.verified
                            && !self.is_class(invocation.target)
                            && !self.fixtures.contains_key(&invocation.target) =>
                    {
                        Some(value)
                    }
                    Value::Sequence(_)
                    | Value::Mapping(_)
                    | Value::LocalNamespace(..)
                    | Value::NamespaceCopy(_)
                    | Value::Module(_)
                    | Value::Coroutine(_)
                    | Value::ScheduledTask => None,
                    value if value.is_builtin_scalar_or_iterable() => None,
                    _ => Some(Value::Unknown),
                }
            })
            .collect()
    }

    fn exported_module_members(
        &mut self,
        file: usize,
        initialization_edges: &[Vec<ScopeId>],
    ) -> Vec<Value> {
        let hook = self.local(file, "__getattr__", END);
        let mut lazy_members = BTreeMap::new();
        if let Some(hook) = hook {
            let fallback = self.lazy_dispatch(hook, END, |name, scope, body, argument| {
                if !name.starts_with('_') && self.local(file, name, END).is_none() {
                    lazy_members
                        .entry(name)
                        .or_insert_with(|| Self::lazy_branch(scope, body, argument));
                }
                None
            });
            if !matches!(fallback, LazyMember::Missing) {
                self.scope_issue(
                    file,
                    self.symbols[hook].name.at,
                    "unresolved_lazy_export",
                    "public lazy module members could not be bounded",
                );
            }
        }
        let names: Vec<_> = self.scopes[file]
            .names
            .iter()
            .filter(|(name, _)| !name.starts_with('_'))
            .filter_map(|(&name, bindings)| {
                bindings.last().map(|&id| (name, self.symbols[id].name.at))
            })
            .collect();
        let initialization_imports = OnceCell::new();
        let mut members: Vec<_> = names
            .into_iter()
            .map(|(name, at)| {
                self.module_member_at(
                    &self.files[file].module,
                    name,
                    END,
                    (file, END),
                    ModuleMemberAccess::Export {
                        at,
                        initialization_edges,
                        initialization_imports: &initialization_imports,
                    },
                    0,
                )
            })
            .collect();
        if self.files[file].is_package {
            // Dotted imports can attach public children without a lexical
            // binding in the package. A child found only on disk is absent.
            let prefix = format!("{}.", self.files[file].module);
            let children: Vec<_> = self
                .modules
                .range(prefix.as_str()..)
                .map_while(|(&module, &child)| Some((module.strip_prefix(&prefix)?, child)))
                .filter(|(name, _)| {
                    !name.contains('.')
                        && !name.starts_with('_')
                        && self.local(file, name, END).is_none()
                })
                .collect();
            let at = Location { line: 1, column: 1 };
            let mode = ModuleMemberAccess::Export {
                at,
                initialization_edges,
                initialization_imports: &initialization_imports,
            };
            for (name, child) in children {
                match self.submodule_attachment(file, child, None, (file, END), mode) {
                    SubmoduleAttachment::Attached(import) => {
                        self.read.insert(import);
                        members.push(Value::Module(child));
                        lazy_members.remove(name);
                    }
                    SubmoduleAttachment::Ambiguous => {
                        self.scope_issue(
                            file,
                            at,
                            "ambiguous_binding",
                            format!(
                                "cannot establish whether public submodule {name} was attached"
                            ),
                        );
                        lazy_members.remove(name);
                    }
                    _ => {}
                }
            }
        }
        if let Some(hook) = hook {
            for (name, member) in lazy_members {
                if let LazyMember::Branch(branch) = member {
                    self.read.insert(hook);
                    let value = self.execute_lazy_branch(file, &branch);
                    if !matches!(value, Value::Unknown) {
                        members.push(value);
                    }
                } else {
                    self.scope_issue(
                        file,
                        self.symbols[hook].name.at,
                        "unresolved_lazy_export",
                        format!("public lazy module member {name:?} could not be bounded"),
                    );
                }
            }
        }
        members
    }

    fn retain_package_entrypoints(
        &mut self,
        metadata: &package_entrypoints::Snapshot,
        initialization_edges: &[Vec<ScopeId>],
    ) {
        let mut exports = Vec::new();
        let at = Location { line: 1, column: 1 };
        for entry in &metadata.entries {
            let unresolved = || {
                let reference = if entry.attributes.is_empty() {
                    entry.module.clone()
                } else {
                    format!("{}:{}", entry.module, entry.attributes.join("."))
                };
                let mut issue = Issue::new(
                    "unresolved_package_entrypoint",
                    entry.manifest,
                    format!(
                        "package entry point {reference:?} could not be resolved in the selected sources"
                    ),
                );
                issue.line = entry.line;
                issue
            };
            let Some(&file) = self.modules.get(entry.module.as_str()) else {
                if !metadata.file_only {
                    self.issues.push(unresolved());
                }
                continue;
            };
            // Importing a module-only entry executes its top-level code, which
            // is already part of the compatibility profile's initial roots.
            let mut value = Value::Module(file);
            for attribute in &entry.attributes {
                let access = (file, END);
                value = match value {
                    Value::Module(module) => {
                        let initialization_imports = OnceCell::new();
                        let mode = ModuleMemberAccess::Export {
                            at,
                            initialization_edges,
                            initialization_imports: &initialization_imports,
                        };
                        self.external_member(module, attribute, access, mode)
                    }
                    value => {
                        if let Value::Callable(invocation) = &value
                            && self.is_class(invocation.target)
                        {
                            // Looking up Class.member loads Class without
                            // exposing the rest of its public members.
                            self.edge(file, invocation.target, "package_entrypoint", "certain");
                        }
                        self.member(value, attribute, access, 0)
                    }
                };
            }
            match value {
                Value::Unknown
                | Value::OpaqueNamespace
                | Value::OperatorResult
                | Value::Provider(_) => self.issues.push(unresolved()),
                Value::Module(_) => {}
                value => exports.push(Export {
                    scope: file,
                    at,
                    kind: "package_entrypoint",
                    value,
                }),
            }
        }
        self.retain_exports(exports, initialization_edges, "certain");
    }

    fn external_member(
        &mut self,
        file: usize,
        name: &str,
        access: (ScopeId, Location),
        mode: ModuleMemberAccess<'_>,
    ) -> Value {
        if let ModuleMember::Lazy(hook, branch) =
            self.selected_module_member(file, name, END, access, mode)
        {
            self.read.insert(hook);
            self.execute_lazy_branch(file, &branch)
        } else {
            self.module_member_at(&self.files[file].module, name, END, access, mode, 0)
        }
    }

    fn external_roots(
        &mut self,
        entrypoints: &[Entrypoint],
        initialization_edges: &[Vec<ScopeId>],
    ) -> Vec<Edge> {
        let mut exports = Vec::new();
        for file in 0..self.files.len() {
            let initialization_imports = OnceCell::new();
            if let Some(all) = self.local(file, "__all__", END) {
                let value = self.binding(all, END, 0);
                if let Value::Sequence(export_names) = value {
                    for export in export_names {
                        let Value::Text(export) = export else {
                            self.issue(
                                file,
                                self.symbols[all].name.at,
                                "opaque_exports",
                                "public exports must be literal names",
                            );
                            continue;
                        };
                        let access = (file, END);
                        let mode = ModuleMemberAccess::Export {
                            at: self.symbols[all].name.at,
                            initialization_edges,
                            initialization_imports: &initialization_imports,
                        };
                        let value = self.external_member(file, &export, access, mode);
                        match value {
                            Value::Unknown => self.issue(
                                file,
                                self.symbols[all].name.at,
                                "unresolved_export",
                                format!("public export {export} could not be resolved"),
                            ),
                            value => exports.push(Export {
                                scope: file,
                                at: self.symbols[all].name.at,
                                kind: "public_export",
                                value,
                            }),
                        }
                    }
                } else {
                    self.issue(
                        file,
                        self.symbols[all].name.at,
                        "opaque_exports",
                        "public exports must be a literal sequence",
                    );
                }
            }
            let aliases: Vec<_> = self.scopes[file]
                .names
                .values()
                .filter_map(|bindings| {
                    let &latest = bindings.last()?;
                    // The explicit import names the public member. Later
                    // assignments can change the value behind that name.
                    bindings.iter().any(|&id| {
                        matches!(self.symbols[id].binding, Binding::Import(import) if import.declares_export)
                    }).then_some(latest)
                })
                .collect();
            for alias in aliases {
                let name = self.symbols[alias].name;
                let at = name.at;
                let previous_issues = self.scope_issues.len();
                let value = if self.files[file].is_package {
                    self.module_member_at(
                        &self.files[file].module,
                        &name.text,
                        END,
                        (file, END),
                        ModuleMemberAccess::Export {
                            at,
                            initialization_edges,
                            initialization_imports: &initialization_imports,
                        },
                        0,
                    )
                } else {
                    self.binding(alias, END, 0)
                };
                match value {
                    Value::Unknown if self.scope_issues.len() == previous_issues => self
                        .scope_issue(
                            file,
                            at,
                            "unresolved_export",
                            "explicit re-export could not be resolved after initialization",
                        ),
                    Value::Unknown => {}
                    value => exports.push(Export {
                        scope: file,
                        at,
                        kind: "explicit_re_export",
                        value,
                    }),
                }
            }
        }
        self.retain_exports(exports, initialization_edges, "certain");

        let mut roots = Vec::new();
        for id in 0..self.symbols.len() {
            let symbol = &self.symbols[id];
            let Binding::Definition(definition, _) = symbol.binding else {
                continue;
            };
            if entrypoints.is_empty() {
                continue;
            }
            let scope = symbol.scope;
            let file = &self.files[self.scopes[scope].file];
            let full_name = self.full_name(id);
            let mut qualifiers = vec![(
                "type".into(),
                if self.is_class(id) {
                    "class"
                } else {
                    "function"
                }
                .into(),
            )];
            if let Some(parent) = self.scopes[scope].owner {
                qualifiers.push(("parent_name".into(), self.symbols[parent].name.text.clone()));
                qualifiers.push(("parent_full_name".into(), self.full_name(parent)));
                qualifiers.push(("parent_module".into(), file.module.clone()));
                qualifiers.push(("parent_path".into(), file.path.clone()));
                if let Binding::Definition(parent_definition, _) = self.symbols[parent].binding {
                    for base in &parent_definition.bases {
                        match self.probe_identity(
                            self.symbols[parent].scope,
                            base,
                            parent_definition.name.at,
                        ) {
                            Value::Provider(name) => {
                                qualifiers.push(("parent_base_class".into(), name))
                            }
                            Value::Callable(Invocation { target: id, .. }) => {
                                qualifiers.push(("parent_base_class".into(), self.full_name(id)))
                            }
                            _ => {}
                        }
                    }
                }
            }
            for (key, values) in [
                ("decorator", &definition.decorators),
                ("base_class", &definition.bases),
            ] {
                for expression in values {
                    let identity = match &expression.kind {
                        ExpressionKind::Call { function, .. } => function,
                        _ => expression,
                    };
                    match self.probe_identity(scope, identity, definition.name.at) {
                        Value::Provider(name) => qualifiers.push((key.into(), name)),
                        Value::Callable(Invocation { target: id, .. }) => {
                            qualifiers.push((key.into(), self.full_name(id)))
                        }
                        _ => {}
                    }
                }
            }
            if entrypoints.iter().any(|selector| {
                selector.matches(
                    &file.path,
                    &file.module,
                    &definition.name.text,
                    &full_name,
                    &qualifiers,
                )
            }) {
                roots.push(Edge {
                    target: id,
                    kind: "configured_entrypoint",
                    certainty: "certain",
                });
                if self.is_class(id) {
                    self.constructor(
                        self.scopes[scope].file,
                        id,
                        "constructor",
                        "possible",
                        definition.name.at,
                    );
                }
            }
        }
        roots
    }

    fn verification_evidence(
        &self,
        id: SymbolId,
        path: &str,
        at: Location,
        kind: &'static str,
    ) -> VerificationEvidence {
        let symbol = &self.symbols[id];
        let file = &self.files[self.scopes[symbol.scope].file];
        let qualified_name = self.full_name(id);
        let Location { line, column } = symbol.name.at;
        VerificationEvidence {
            symbol_id: format!("{}::{qualified_name}@{line}:{column}", file.path),
            qualified_name,
            definition_path: file.path.clone(),
            definition_line: line,
            definition_column: column,
            path: path.to_owned(),
            line: at.line,
            column: at.column,
            kind,
            certainty: "possible",
        }
    }

    fn configuration_roots(
        &self,
        snapshot: &grep_verification::Snapshot,
        derived: &mut Verification,
    ) -> bool {
        if !snapshot.has_configuration_references() {
            return true;
        }
        let effective: BTreeSet<_> = self
            .scopes
            .iter()
            .flat_map(|scope| scope.names.values())
            .flat_map(|bindings| self.effective_bindings(bindings))
            .copied()
            .filter(|&id| matches!(self.symbols[id].binding, Binding::Definition(..)))
            .collect();
        for &id in &effective {
            let symbol = &self.symbols[id];
            let Binding::Definition(definition, _) = symbol.binding else {
                continue;
            };
            let scope = &self.scopes[symbol.scope];
            // Only a module member or a member of an addressable class can
            // be selected by a qualified configuration reference.
            let mut owner = scope.owner;
            while let Some(id) = owner {
                if !self.is_class(id) || !effective.contains(&id) {
                    break;
                }
                owner = self.scopes[self.symbols[id].scope].owner;
            }
            if owner.is_some() {
                continue;
            }
            let file = &self.files[scope.file];
            let qualified = self.full_name(id);
            let member = &qualified[file.module.len() + 1..];
            let include_module_reference =
                scope.owner.is_none() && definition.kind != DefinitionKind::Class;
            for reference in
                snapshot.matches(&file.path, &file.module, member, include_module_reference)
            {
                if derived.evidence.len() + self.dynamic_references.len()
                    >= grep_verification::MAX_EVIDENCE
                {
                    return false;
                }
                derived.roots.insert(id);
                derived.evidence.push(self.verification_evidence(
                    id,
                    &reference.path,
                    reference.at,
                    reference.kind,
                ));
            }
        }
        true
    }

    fn runtime_roots(
        &self,
        snapshot: &runtime_evidence::Snapshot,
        roots: &mut Vec<Edge>,
        possible_roots: &mut Vec<Edge>,
    ) -> (Vec<runtime_evidence::Evidence>, Vec<Issue>) {
        type Observed<'a> = &'a runtime_evidence::Observation;
        let mut by_location = BTreeMap::<(&str, usize), Vec<Observed<'_>>>::new();
        for observation in &snapshot.observations {
            by_location
                .entry((&observation.file, observation.line))
                .or_default()
                .push(observation);
        }
        let mut matched = BTreeMap::<Observed<'_>, Option<SymbolId>>::new();
        let mut declarations = BTreeMap::<Observed<'_>, Option<SymbolId>>::new();
        for (id, symbol) in self.symbols.iter().enumerate() {
            let Binding::Definition(definition, body) = symbol.binding else {
                continue;
            };
            if definition.kind == DefinitionKind::Class {
                continue;
            }
            let file = self.files[self.scopes[body].file].path.as_str();
            let mut observe = |line, function, declaration| {
                if let Some(observations) = by_location.get(&(file, line)) {
                    for &observation in observations {
                        if observation.function.as_deref() != function {
                            continue;
                        }
                        let matches = if declaration {
                            if !observation.allow_declaration {
                                continue;
                            }
                            &mut declarations
                        } else {
                            &mut matched
                        };
                        matches
                            .entry(observation)
                            .and_modify(|previous| {
                                if *previous != Some(id) {
                                    *previous = None;
                                }
                            })
                            .or_insert(Some(id));
                    }
                }
            };
            let name = Some(definition.name.text.as_str());
            observe(definition.name.at.line, name, false);
            observe(definition.name.at.line, None, true);
            if let Some(decorator) = definition.decorators.first() {
                observe(decorator.at.line, name, false);
            }
            for &line in &definition.body_coverage_lines {
                if line != definition.name.at.line {
                    observe(line, None, false);
                }
            }
        }
        let mut evidence = Vec::new();
        let mut rejected = BTreeMap::<&str, usize>::new();
        for observation in &snapshot.observations {
            let (candidate, kind) = match matched.get(observation) {
                Some(target) => (*target, observation.kind()),
                None => (
                    declarations.get(observation).copied().flatten(),
                    runtime_evidence::COVERAGE_DECLARATION,
                ),
            };
            let Some(target) = candidate else {
                *rejected.entry(&observation.artifact).or_default() += 1;
                continue;
            };
            let (roots, certainty) = match observation.provenance {
                runtime_evidence::Provenance::Verified => (&mut *roots, "certain"),
                runtime_evidence::Provenance::Unverified => (&mut *possible_roots, "possible"),
            };
            roots.push(Edge {
                target,
                kind,
                certainty,
            });
            if kind != runtime_evidence::COVERAGE_DECLARATION {
                // A nested callable can only be observed after an enclosing
                // activation created it. Keep the dependencies of that activation.
                let mut owner = self.scopes[self.symbols[target].scope].owner;
                while let Some(target) = owner {
                    possible_roots.push(Edge {
                        target,
                        kind: "runtime_owner",
                        certainty: "possible",
                    });
                    owner = self.scopes[self.symbols[target].scope].owner;
                }
            }
            evidence.push(runtime_evidence::Evidence {
                artifact: observation.artifact.clone(),
                file: observation.file.clone(),
                line: observation.line,
                full_name: self.full_name(target),
                kind,
                provenance: observation.provenance,
            });
        }
        let issues = rejected
            .into_iter()
            .map(|(path, count)| {
                Issue::new(
                    "runtime_evidence_rejected",
                    path,
                    format!(
                        "{count} observations did not match a unique selected callable location"
                    ),
                )
            })
            .collect();
        (evidence, issues)
    }

    fn traverse(
        &mut self,
        reach: &mut Reachability,
        mut pending: VecDeque<Edge>,
        class_bodies: &BTreeSet<ScopeId>,
        verification: bool,
        possible: bool,
    ) -> bool {
        for _ in 0..MAX_FRAMEWORK_PASSES {
            let before = (
                reach.scopes.len(),
                reach.active_objects.len(),
                reach.symbols.len(),
                reach.django_setup_scopes.len(),
                reach.django_signal_modules.len(),
            );
            while let Some(edge) = pending.pop_front() {
                if !verification && is_verification_edge(edge.kind)
                    || !possible && is_deferred_possible_edge(edge.kind)
                    || self.rejected_django_callbacks.contains(&edge.target)
                {
                    continue;
                }
                if !possible && edge.kind == "fastapi_lifespan" && edge.certainty == "possible" {
                    reach
                        .optional_framework_callbacks
                        .insert((edge.target, edge.kind));
                    continue;
                }
                let certainty = if possible { "possible" } else { edge.certainty };
                if matches!(edge.kind, "fastapi_registration" | "fastapi_dependency")
                    && reach.fastapi_callbacks.insert(edge.target)
                {
                    pending.extend(self.fastapi_parameter_dependencies(edge.target).iter().map(
                        |&target| Edge {
                            target,
                            kind: "fastapi_dependency",
                            certainty,
                        },
                    ));
                }
                if edge.kind == "fastapi_dependency"
                    && let Some(effects) = self.fastapi_constructor_effects(edge.target)
                {
                    self.open_fastapi_effects(effects, reach, &mut pending);
                }
                reach.evidence.insert((edge.target, edge.kind, certainty));
                if edge.kind == "django_configured_class_declaration" {
                    continue;
                }
                reach.symbols.insert(edge.target);
                if matches!(
                    edge.kind,
                    "type_reference"
                        | "code_reference"
                        | "metaclass_creation"
                        | "generator_creation"
                        | "flask_factory_failed_call"
                        | "flask_initializer_failed_call"
                        | "django_settings_failed_initializer_call"
                        | "sqlalchemy_hybrid_descriptor"
                        | "pluggy_hook_declaration"
                        | aiohttp::DECLARATION
                        | gunicorn::DECLARATION
                        | "marshmallow_schema_registry"
                        | "marshmallow_schema_options"
                        | "marshmallow_hook_declaration"
                        | "celery_backend_default"
                        | "django_migration_callback_declaration"
                        | django_migrations::DECLARATION
                        | drf::PROTOCOL_DECLARATION
                        | drf::FIELD_DECLARATION
                        | wagtail::FIELD_DECLARATION
                        | wagtail::BLOCK_METADATA
                        | wagtail::MODEL_OPTION
                        | wagtail::CALLBACK_PARAMETER
                        | mcp_tools::DECORATOR_APPLICATION
                        | gin::CONFIG_BINDING
                        | "click_command_declaration"
                        | "celery_task_declaration"
                        | "django_relationship_field_declaration"
                        | frameworks::CLASS_ATTRIBUTE_DECLARATION
                        | frameworks::BUILTIN_OVERRIDE_DECLARATION
                        | frameworks::STDLIB_PROTOCOL_DECLARATION
                ) {
                    continue;
                }
                let Binding::Definition(_, scope) = self.symbols[edge.target].binding else {
                    continue;
                };
                if reach.scopes.insert(scope) {
                    pending.extend(&self.scopes[scope].edges);
                }
            }
            for &scope in class_bodies {
                if self.scopes[scope]
                    .parent
                    .is_some_and(|parent| reach.scopes.contains(&parent))
                    && reach.scopes.insert(scope)
                {
                    pending.extend(&self.scopes[scope].edges);
                }
            }
            for &scope_id in &reach.scopes {
                if possible && let Some(receivers) = self.atexit_receivers.get(&scope_id) {
                    for &(edge, class) in receivers {
                        if verification || !is_verification_edge(edge.kind) {
                            reach
                                .deferred_method_receivers
                                .entry(edge.target)
                                .or_default()
                                .insert(class);
                        }
                    }
                }
                let scope = &self.scopes[scope_id];
                reach.active_objects.extend(&scope.activated_apps);
                for &(parent, child) in &scope.mounts {
                    if reach.active_objects.contains(&parent) {
                        reach.active_objects.insert(child);
                    }
                }
                let routes = scope.routes.iter().map(|route| (route, false)).chain(
                    scope
                        .optional_flask_routes
                        .iter()
                        .map(|route| (route, true)),
                );
                for (&(object, target, kind), optional) in routes {
                    if !reach.active_objects.contains(&object) {
                        continue;
                    }
                    if let Some(receivers) = self
                        .framework_route_receivers
                        .get(&(scope_id, object, target))
                    {
                        reach
                            .deferred_method_receivers
                            .entry(target)
                            .or_default()
                            .extend(receivers);
                    }
                    if optional && !possible {
                        reach.optional_framework_callbacks.insert((target, kind));
                        continue;
                    }
                    let pending_route = if optional {
                        !reach.evidence.contains(&(target, kind, "possible"))
                    } else if self.phase03 {
                        !reach.has_evidence(target, kind)
                    } else {
                        !reach.symbols.contains(&target)
                    };
                    if pending_route {
                        pending.push_back(Edge {
                            target,
                            kind,
                            certainty: if optional
                                || matches!(kind, "sanic_listener" | "starlette_route")
                            {
                                "possible"
                            } else {
                                "certain"
                            },
                        });
                    }
                }
            }
            if self.phase03 {
                self.activate_django_signals(reach, &mut pending, verification, possible);
                self.activate_django_autoreload_wrappers(reach, &mut pending, possible);
                self.activate_fastapi(reach, &mut pending);
                self.activate_flask_parameter_hooks(reach, &mut pending);
                self.activate_static_parameter_calls(reach, &mut pending);
                self.activate_source_provider_calls(reach, &mut pending, possible);
                self.resolve_deferred_receiver_calls(reach, &mut pending);
                self.resolve_django_reloader_parameter_receivers(reach, &mut pending, possible);
            }
            for &(owner, target, kind) in &self.framework_lifecycle {
                if reach.symbols.contains(&owner) && !reach.has_evidence(target, kind) {
                    pending.push_back(Edge {
                        target,
                        kind,
                        certainty: "certain",
                    });
                }
            }
            if pending.is_empty()
                && before
                    == (
                        reach.scopes.len(),
                        reach.active_objects.len(),
                        reach.symbols.len(),
                        reach.django_setup_scopes.len(),
                        reach.django_signal_modules.len(),
                    )
            {
                return true;
            }
        }
        false
    }

    fn traverse_with_atexit(
        &mut self,
        reach: &mut Reachability,
        pending: VecDeque<Edge>,
        class_bodies: &BTreeSet<ScopeId>,
    ) -> bool {
        if !self.traverse(reach, pending, class_bodies, false, false) {
            return false;
        }
        if !self.phase03 {
            return true;
        }
        // Close certain reachability first so optional callbacks only add
        // possible evidence for descendants without an independent path.
        let mut pending: VecDeque<_> = reach
            .scopes
            .iter()
            .flat_map(|&scope| &self.scopes[scope].edges)
            .filter(|edge| is_deferred_possible_edge(edge.kind))
            .copied()
            .collect();
        pending.extend(
            std::mem::take(&mut reach.optional_framework_callbacks)
                .into_iter()
                .map(|(target, kind)| Edge {
                    target,
                    kind,
                    certainty: "possible",
                }),
        );
        (pending.is_empty() && !self.has_reachable_django_signals(reach))
            || self.traverse(reach, pending, class_bodies, false, true)
    }

    fn verification_edges(
        &self,
        reach: &Reachability,
        roots: &BTreeSet<SymbolId>,
    ) -> VecDeque<Edge> {
        let mut pending: VecDeque<_> = roots
            .iter()
            .map(|&target| Edge {
                target,
                kind: "grep_configuration",
                certainty: "possible",
            })
            .collect();
        pending.extend(reach.scopes.iter().flat_map(|&scope| {
            self.scopes[scope]
                .edges
                .iter()
                .copied()
                .filter(|edge| is_verification_edge(edge.kind))
        }));
        pending
    }

    fn establish_reachability(
        &mut self,
        roots: &[Edge],
        possible_roots: &[Edge],
        snapshot: Option<&grep_verification::Snapshot>,
    ) -> (Reachability, Option<Reachability>, Verification, bool) {
        let mut reach = Reachability {
            scopes: (0..self.files.len()).collect(),
            ..Reachability::default()
        };
        let runtime_targets: BTreeSet<_> = roots
            .iter()
            .chain(possible_roots)
            .filter(|edge| is_runtime_execution_edge(edge.kind))
            .map(|edge| edge.target)
            .collect();
        let observed_callback = !runtime_targets.is_empty()
            && self.parameter_calls.iter().any(|call| {
                self.scopes[self.symbols[call.parameter].scope]
                    .owner
                    .is_some_and(|owner| runtime_targets.contains(&owner))
            });
        let mut pending: VecDeque<_> = roots.iter().copied().collect();
        // Only visited class declarations execute when their parent is reached.
        let class_bodies: BTreeSet<_> = self
            .scopes
            .iter()
            .flat_map(|scope| scope.class_bodies.iter().copied())
            .collect();
        for &scope in &class_bodies {
            if self.scopes[scope]
                .parent
                .is_some_and(|parent| reach.scopes.contains(&parent))
            {
                reach.scopes.insert(scope);
            }
        }
        for &scope in &reach.scopes {
            pending.extend(&self.scopes[scope].edges);
        }
        // Check whether a runtime observation of a callback owner has an
        // independent static witness. Calls reached only through the runtime
        // roots, such as a recursive call, cannot explain the initial call.
        let mut static_reach = None;
        if observed_callback {
            let mut static_only = Reachability {
                scopes: reach.scopes.clone(),
                ..Reachability::default()
            };
            let static_pending = pending
                .iter()
                .copied()
                .filter(|edge| !is_runtime_execution_edge(edge.kind))
                .collect();
            if self.traverse_with_atexit(&mut static_only, static_pending, &class_bodies) {
                static_reach = Some(static_only);
            }
        }
        let mut converged = self.traverse_with_atexit(&mut reach, pending, &class_bodies);
        if !possible_roots.is_empty()
            && !self.traverse(
                &mut reach,
                possible_roots.iter().copied().collect(),
                &class_bodies,
                false,
                true,
            )
        {
            converged = false;
        }
        let mut verification = Verification::default();
        if let Some(snapshot) = snapshot {
            let baseline = reach.symbols.clone();
            if !self.configuration_roots(snapshot, &mut verification) {
                self.issues.push(grep_verification::evidence_limit());
            }
            if let Some(mut static_only) = static_reach.take() {
                let pending = self.verification_edges(&static_only, &verification.roots);
                if pending.is_empty()
                    || self.traverse(&mut static_only, pending, &class_bodies, true, true)
                {
                    static_reach = Some(static_only);
                }
            }
            let pending = self.verification_edges(&reach, &verification.roots);
            if !pending.is_empty() && !self.traverse(&mut reach, pending, &class_bodies, true, true)
            {
                converged = false;
            }
            for &(scope, target, line, column) in &self.dynamic_references {
                if reach.scopes.contains(&scope) && reach.symbols.contains(&target) {
                    if !baseline.contains(&target) {
                        verification.roots.insert(target);
                    }
                    verification.evidence.push(self.verification_evidence(
                        target,
                        &self.files[self.scopes[scope].file].path,
                        Location { line, column },
                        "dynamic_member",
                    ));
                }
            }
            verification.rescued_count = verification.roots.difference(&baseline).count();
            verification.transitive = reach
                .symbols
                .difference(&baseline)
                .filter(|target| !verification.roots.contains(target))
                .copied()
                .collect();
        }
        (reach, static_reach, verification, converged)
    }

    fn finish(
        mut self,
        roots: Vec<Edge>,
        mut possible_roots: Vec<Edge>,
        snapshot: Option<&grep_verification::Snapshot>,
        templates: Option<&TemplateProject<'_>>,
    ) -> Analysis {
        // Declaration hints retain only the definition in the compatibility
        // report. Leave them out of the execution graph and parameter analysis.
        let mut coverage_declarations = BTreeSet::new();
        possible_roots.retain(|edge| {
            if edge.kind == runtime_evidence::COVERAGE_DECLARATION {
                coverage_declarations.insert(edge.target);
                false
            } else {
                true
            }
        });
        self.resolve_class_invocations();
        self.resolve_django_global_signals(&roots, &possible_roots);
        let (mut reach, mut static_reach, mut verification, mut converged) =
            self.establish_reachability(&roots, &possible_roots, snapshot);
        if self.phase03
            && converged
            && self.activate_configured_django_after_dormant_startup(&reach)
        {
            self.resolve_receiver_calls();
            self.resolve_class_invocations();
            (reach, static_reach, verification, converged) =
                self.establish_reachability(&roots, &possible_roots, snapshot);
        }
        if self.phase03 && converged {
            let mut protocols = self.django_settings_protocol_roots(&reach);
            protocols.extend(self.external_django_setting_declarations(&reach));
            protocols.extend(self.standalone_django_converter_roots(&reach));
            if let Some(templates) = templates {
                protocols.extend(self.django_template_roots(&reach, templates));
            }
            if !protocols.is_empty() {
                possible_roots.extend(protocols);
                self.resolve_receiver_calls();
                self.resolve_class_invocations();
                (reach, static_reach, verification, converged) =
                    self.establish_reachability(&roots, &possible_roots, snapshot);
            }
        }
        if self.phase03 && converged && self.resolve_django_view_dispatch(&reach) {
            self.resolve_receiver_calls();
            self.resolve_class_invocations();
            (reach, static_reach, verification, converged) =
                self.establish_reachability(&roots, &possible_roots, snapshot);
        }
        if !converged {
            self.issues.push(framework_limit());
        }
        self.report_unbound_receiver_calls(&reach);
        self.report_fresh_field_calls(&reach);
        self.report_constructor_hook_stores(&reach);
        self.report_source_provider_calls(&reach);
        self.issues
            .extend(
                self.fastapi_dependency_issues
                    .drain(..)
                    .filter_map(|(target, issue)| {
                        reach.fastapi_callbacks.contains(&target).then_some(issue)
                    }),
            );
        self.report_fastapi_coverage(&reach);
        let fastapi_route_contracts = self.report_fastapi_signatures(&reach);
        if self.phase03 {
            self.report_django_startup_coverage(&reach);
            self.retain_django_ready_signal_imports(&mut reach);
            self.report_django_global_signals(&reach);
            self.report_django_converter_wrappers(&reach);
            self.report_converter_string_calls(&reach);
            self.report_django_autoreload_wrappers(&reach);
            self.report_django_middleware_calls(&reach);
            self.report_log_record_calls(&reach);
            self.report_flask_parameter_hooks(&reach);
        }
        self.issues.extend(
            self.django_registration_issues
                .drain(..)
                .filter_map(|(object, issue)| {
                    reach.active_objects.contains(&object).then_some(issue)
                }),
        );
        self.remove_resolved_static_parameter_diagnostics(&reach);
        self.report_fastapi_websocket_query_failures(&reach);
        #[cfg(test)]
        let fastapi_initializer_evidence = self.fastapi_initializer_evidence(&reach);
        let Reachability {
            scopes: reachable_scopes,
            symbols: reachable_symbols,
            evidence,
            ..
        } = reach;
        let django_class_declarations: BTreeSet<_> = evidence
            .iter()
            .filter_map(|&(target, kind, _)| {
                (kind == "django_configured_class_declaration").then_some(target)
            })
            .collect();
        let independently_retained: BTreeSet<_> = evidence
            .iter()
            .filter_map(|&(target, kind, _)| {
                if matches!(kind, "direct_call" | "grep_dynamic_call" | "type_reference") {
                    return None;
                }
                if is_runtime_execution_edge(kind)
                    && let Binding::Definition(_, body) = self.symbols[target].binding
                    && static_reach
                        .as_ref()
                        .is_some_and(|reach| reach.scopes.contains(&body))
                {
                    return None;
                }
                Some(target)
            })
            .collect();
        for call in self
            .parameter_calls
            .iter()
            .filter(|call| reachable_scopes.contains(&call.scope))
        {
            let parameter = &self.symbols[call.parameter];
            let owner = self.scopes[parameter.scope]
                .owner
                .expect("parameters belong to a definition");
            let mut callers = self
                .direct_arguments
                .get(&owner)
                .into_iter()
                .flatten()
                .filter(|callsite| reachable_scopes.contains(&callsite.caller))
                .peekable();
            let finite = !independently_retained.contains(&owner)
                && callers.peek().is_some()
                && callers.all(|callsite| {
                    matches!(
                        callsite.values.get(&parameter.name.text),
                        Some(Value::Callable(_))
                    )
                });
            if !finite && !self.pydantic_resolves_handler(call, owner, &evidence, &reachable_scopes)
            {
                self.issues.push(
                    Issue::new(
                        "unresolved_call",
                        &self.files[self.scopes[call.scope].file].path,
                        "callable parameter is outside the bounded model",
                    )
                    .at(call.at),
                );
            }
        }

        let mut findings = Vec::new();
        let mut retained = Vec::new();
        let registered_callback_contracts: BTreeSet<_> = evidence
            .iter()
            .filter_map(|&(target, kind, _)| {
                (self.retain_sanic_listener_parameters(target, kind)
                    || self.retain_fastapi_callback_parameters(target, kind)
                    || kind == celery_routes::ROUTER
                    || kind == "django_migration_callback_declaration")
                    .then_some(target)
            })
            .collect();
        let django_callback_contracts: BTreeSet<_> = evidence
            .iter()
            .filter_map(|&(target, kind, _)| {
                frameworks::is_django_signal_edge(kind).then_some(target)
            })
            .collect();
        let transitive = verification
            .transitive
            .iter()
            .map(|&target| (target, "reachable_from_root", "possible"));
        let declaration_evidence = coverage_declarations
            .iter()
            .map(|&target| (target, runtime_evidence::COVERAGE_DECLARATION, "possible"));
        for (id, kind, certainty) in evidence
            .into_iter()
            .chain(transitive)
            .chain(declaration_evidence)
        {
            if is_verification_edge(kind) {
                continue;
            }
            if matches!(
                kind,
                "attrs_field"
                    | frameworks::CLASS_ATTRIBUTE_DECLARATION
                    | "marshmallow_schema_options"
                    | "django_setting"
                    | "django_application"
                    | "django_ready_signal_import"
                    | "django_converter_regex"
                    | "django_migration_parameter"
                    | django_migrations::DECLARATION
                    | "django_command_parameter"
                    | celery_routes::ROUTER_PARAMETER
                    | "fastapi_parameter"
                    | gunicorn::DECLARATION
                    | drf::FIELD_DECLARATION
                    | wagtail::FIELD_DECLARATION
                    | wagtail::BLOCK_METADATA
                    | wagtail::MODEL_OPTION
                    | wagtail::SNIPPET_OPTION
                    | wagtail::CALLBACK_PARAMETER
            ) {
                self.read.insert(id);
            }
            let symbol = &self.symbols[id];
            retained.push(Evidence {
                file: self.files[self.scopes[symbol.scope].file].path.clone(),
                line: symbol.name.at.line,
                full_name: self.full_name(id),
                kind,
                certainty,
            });
        }
        let mut definitions = BTreeMap::new();
        for (id, symbol) in self.symbols.iter().enumerate() {
            if matches!(symbol.binding, Binding::Deleted) {
                continue;
            }
            let scope = &self.scopes[symbol.scope];
            let name = &symbol.name.text;
            if matches!(symbol.binding, Binding::Import(import) if import.wildcard_member) {
                continue;
            }
            if self.phase03
                && let Some((key, definition)) = self.definition_context(id)
            {
                definitions.insert(key, definition);
            }
            if name == "_" || (name.starts_with("__") && name.ends_with("__")) {
                continue;
            }
            let public = !name.starts_with('_');
            let (category, rule_id, confidence, reason) = match symbol.binding {
                Binding::Definition(def, _)
                    if !reachable_symbols.contains(&id)
                        && !django_class_declarations.contains(&id)
                        && def.kind == DefinitionKind::Class =>
                {
                    (Category::Class, "SKY-U004", 90, "unreachable_class")
                }
                Binding::Definition(def, _)
                    if !reachable_symbols.contains(&id)
                        && !coverage_declarations.contains(&id)
                        && def.kind != DefinitionKind::Class =>
                {
                    (
                        Category::Function,
                        "SKY-U001",
                        if public && scope.owner.is_some_and(|id| self.is_class(id)) {
                            80
                        } else {
                            90
                        },
                        if self.phase03
                            && scope.owner.is_none()
                            && !self.direct.contains(&id)
                            && (name.starts_with("format_") || name.ends_with("_name"))
                        {
                            WEAK_HELPER_FUNCTION
                        } else {
                            "no_call_path_from_root"
                        },
                    )
                }
                Binding::Import(_) if !self.read.contains(&id) => {
                    (Category::Import, "SKY-U002", 100, "binding_is_unread")
                }
                Binding::Variable(_, _) | Binding::UnpackItem { .. }
                    if !self.read.contains(&id)
                        && !(self.framework_fields.contains(&id)
                            && scope
                                .owner
                                .is_some_and(|owner| reachable_symbols.contains(&owner))) =>
                {
                    let confidence = match scope.owner {
                        Some(owner) if self.is_class(owner) => 80,
                        None => 90,
                        _ if !public => 90,
                        _ => 100,
                    };
                    (
                        Category::Variable,
                        "SKY-U003",
                        confidence,
                        "binding_is_unread",
                    )
                }
                Binding::Parameter
                    if !self.read.contains(&id)
                        && !name.starts_with('_')
                        && !matches!(name.as_str(), "self" | "cls")
                        && !scope.owner.is_some_and(|owner| {
                            self.fixtures.contains_key(&owner)
                                || reachable_symbols.contains(&owner)
                                    && !self.direct.contains(&owner)
                                    && !registered_callback_contracts.contains(&owner)
                                    && !fastapi_route_contracts.contains(&owner)
                                || django_callback_contracts.contains(&owner)
                        }) =>
                {
                    (
                        Category::Parameter,
                        "SKY-U006",
                        if scope.owner.is_some_and(|owner| {
                            self.symbols[owner].name.text.starts_with('_')
                                && self.direct.contains(&owner)
                        }) {
                            100
                        } else {
                            50
                        },
                        "parameter_is_unread",
                    )
                }
                _ => continue,
            };
            let (name, full_name) = if let Binding::Import(import) = &symbol.binding {
                let module = if import.level == 0 {
                    import.module.clone()
                } else {
                    self.import_module(scope.file, import)
                        .unwrap_or_else(|| import.module.clone())
                };
                let full_name = import
                    .member
                    .as_ref()
                    .map_or_else(|| module.clone(), |member| format!("{module}.{member}"));
                let name = full_name.rsplit('.').next().unwrap_or(name).to_owned();
                (name, full_name)
            } else {
                (name.clone(), self.full_name(id))
            };
            let identity_name = if matches!(symbol.binding, Binding::Import(_)) {
                format!("{} -> {full_name}", self.full_name(id))
            } else {
                full_name.clone()
            };
            findings.push(Finding {
                category,
                name,
                full_name,
                identity_name,
                file: self.files[scope.file].path.clone(),
                line: symbol.name.at.line,
                column: symbol.name.at.column,
                method_owner: scope
                    .owner
                    .filter(|&owner| category == Category::Function && self.is_class(owner))
                    .map(|owner| position(self.symbols[owner].name.at)),
                declaration_whitelist: self.framework_class_whitelists.get(&id).copied(),
                confidence,
                rule_id,
                reason,
            });
        }
        let django_admin_packages = self.django_admin_stub_packages();
        for file in self.files.iter().filter(|file| file.facts.empty) {
            let name = file.path.rsplit('/').next();
            let admin_stub = name == Some("admin.py")
                && Path::new(&file.path)
                    .parent()
                    .is_some_and(|parent| django_admin_packages.contains(parent));
            if !admin_stub && !matches!(name, Some("__init__.py" | "__main__.py" | "main.py")) {
                findings.push(Finding {
                    category: Category::File,
                    identity_name: String::new(),
                    name: file.path.clone(),
                    full_name: file.path.clone(),
                    file: file.path.clone(),
                    line: 1,
                    column: 1,
                    method_owner: None,
                    declaration_whitelist: None,
                    confidence: 100,
                    rule_id: "SKY-E002",
                    reason: "empty_or_docstring_only",
                });
            }
        }
        let mut fastapi_route_declarations: Vec<_> = self
            .fastapi
            .route_declarations
            .iter()
            .map(|&target| {
                let symbol = &self.symbols[target];
                super::FastapiRouteDeclaration {
                    file: self.files[self.scopes[symbol.scope].file].path.clone(),
                    line: position(symbol.name.at)[0],
                    qualified_name: self.full_name(target),
                }
            })
            .collect();
        fastapi_route_declarations.sort();
        self.issues.extend(
            self.scope_issues
                .into_iter()
                .filter_map(|(scope, issue)| reachable_scopes.contains(&scope).then_some(issue)),
        );
        self.issues.sort();
        self.issues.dedup();
        self.uncertainties.sort();
        self.uncertainties.dedup();
        Analysis {
            findings,
            definitions,
            evidence: retained,
            fastapi_route_declarations,
            #[cfg(test)]
            fastapi_initializer_evidence,
            issues: self.issues,
            uncertainties: self.uncertainties,
            verification: verification.evidence,
            rescued_count: verification.rescued_count,
            ..Analysis::default()
        }
    }
}

#[derive(Default)]
struct Reachability {
    scopes: BTreeSet<ScopeId>,
    symbols: BTreeSet<SymbolId>,
    active_objects: BTreeSet<Object>,
    django_setup_scopes: BTreeSet<ScopeId>,
    django_signal_modules: BTreeSet<usize>,
    deferred_method_receivers: BTreeMap<SymbolId, BTreeSet<SymbolId>>,
    resolved_deferred_receiver_calls: BTreeSet<(usize, SymbolId)>,
    resolved_django_reloader_parameters: BTreeSet<usize>,
    resolved_static_parameter_calls: BTreeSet<usize>,
    resolved_source_provider_calls: BTreeSet<SymbolId>,
    closed_class_call_sites: BTreeSet<(ScopeId, [usize; 2], &'static str)>,
    fastapi_callbacks: BTreeSet<SymbolId>,
    fastapi_websocket_query_bindings: BTreeMap<SymbolId, bool>,
    fastapi_lifecycle_effects: BTreeSet<(ScopeId, bool)>,
    optional_framework_callbacks: BTreeSet<(SymbolId, &'static str)>,
    unordered_fastapi_mounts: BTreeSet<usize>,
    version_sensitive_fastapi_mounts: BTreeSet<usize>,
    fastapi_limited: bool,
    fastapi_router_graph_limited: bool,
    evidence: BTreeSet<(SymbolId, &'static str, &'static str)>,
}

impl Reachability {
    fn has_evidence(&self, target: SymbolId, kind: &'static str) -> bool {
        self.evidence.contains(&(target, kind, "certain"))
            || self.evidence.contains(&(target, kind, "possible"))
    }
}

#[derive(Default)]
struct Verification {
    roots: BTreeSet<SymbolId>,
    transitive: BTreeSet<SymbolId>,
    evidence: Vec<VerificationEvidence>,
    rescued_count: usize,
}

fn is_verification_edge(kind: &str) -> bool {
    matches!(
        kind,
        "grep_configuration" | "grep_dynamic" | "grep_dynamic_call"
    )
}

fn is_runtime_execution_edge(kind: &str) -> bool {
    matches!(kind, "runtime_trace" | "runtime_coverage" | "runtime_owner")
}

fn framework_limit() -> Issue {
    Issue::new("analysis_limit", ".", "framework activation limit exceeded")
}

fn mapping_entries(keywords: BTreeMap<String, Value>) -> impl Iterator<Item = Value> {
    keywords
        .into_iter()
        .flat_map(|(name, value)| [Value::Text(name), value])
}

fn position(at: Location) -> [usize; 2] {
    [at.line, at.column]
}

fn namespace_span(body: &[Statement]) -> Option<RangeInclusive<[usize; 2]>> {
    body.first()
        .map(|first| position(first.at)..=[suite_end(body)[0], usize::MAX])
}

// Python creates closure cells from lexical references even in skipped code.
fn namespace_free_names(definition: &Definition, postponed: bool) -> BTreeSet<&str> {
    let class = definition.kind == DefinitionKind::Class;
    let mut assigned: BTreeSet<_> = definition
        .parameters
        .iter()
        .map(|p| p.text.as_str())
        .collect();
    let mut nested_reads = BTreeSet::new();
    let mut expressions = Vec::new();
    let mut statements: Vec<_> = definition.body.iter().collect();
    while let Some(statement) = statements.pop() {
        match &statement.kind {
            StatementKind::Define(child) => {
                assigned.insert(child.name.text.as_str());
                nested_reads.extend(namespace_free_names(child, postponed));
                expressions.extend(&child.decorators);
                expressions.extend(&child.bases);
                expressions.extend(&child.metaclass);
                expressions.extend(&child.defaults);
                if !postponed {
                    expressions.extend(&child.annotations);
                }
            }
            StatementKind::Import(import) => {
                assigned.insert(import.local.text.as_str());
            }
            StatementKind::ProviderWildcardImport(imports) => {
                assigned.extend(imports.iter().map(|import| import.local.text.as_str()));
            }
            StatementKind::ProjectWildcardImport { .. }
            | StatementKind::OptionalProjectClassFallback { .. } => {}
            StatementKind::Assign {
                names,
                value,
                annotation,
            } => {
                assigned.extend(names.iter().map(|name| name.text.as_str()));
                expressions.extend(value);
                if class && !postponed {
                    expressions.extend(annotation);
                }
            }
            StatementKind::AssignUnpack { names, value } => {
                assigned.extend(names.iter().map(|name| name.text.as_str()));
                expressions.push(value);
            }
            StatementKind::AssignDjangoSettings { environment, value } => {
                expressions.push(environment);
                expressions.push(value);
            }
            StatementKind::EditDjangoBackendList { receiver, edit } => {
                expressions.push(receiver);
                expressions.extend(edit.value());
            }
            StatementKind::LiteralRouteLoop {
                path, registration, ..
            } => {
                assigned.insert(path.text.as_str());
                expressions.push(registration);
            }
            StatementKind::WriteAttribute {
                receiver, value, ..
            }
            | StatementKind::WriteModuleEntry {
                mapping: receiver,
                value,
                ..
            } => {
                expressions.push(receiver);
                expressions.extend(value);
            }
            StatementKind::WriteCeleryConfigEntry {
                mapping,
                key,
                value,
            } => {
                expressions.extend([mapping, key]);
                expressions.extend(value);
            }
            StatementKind::If {
                test,
                body,
                otherwise,
            } => {
                expressions.push(test);
                statements.extend(body);
                statements.extend(otherwise);
            }
            StatementKind::With { items, body } => {
                for (value, name) in items {
                    expressions.push(value);
                    assigned.extend(name.iter().map(|name| name.text.as_str()));
                }
                statements.extend(body);
            }
            StatementKind::Return(value) | StatementKind::Raise(value) => expressions.extend(value),
            StatementKind::ForwardSuperInit {
                super_name,
                positional,
                keywords,
            } => {
                expressions.extend([super_name, positional, keywords]);
            }
            StatementKind::DeleteName(name) => {
                assigned.insert(name.text.as_str());
            }
            StatementKind::Assert(value)
            | StatementKind::Evaluate(value)
            | StatementKind::DjangoSettingsGlobalCall(value)
            | StatementKind::SetGlobalName { value, .. }
            | StatementKind::DiscardedComprehension(value) => expressions.push(value),
        }
    }
    let mut reads = BTreeSet::new();
    while let Some(expression) = expressions.pop() {
        match &expression.kind {
            ExpressionKind::Name(name) => {
                reads.insert(name.as_str());
            }
            ExpressionKind::Member(value, _)
            | ExpressionKind::Not(value)
            | ExpressionKind::DeferredLambda(value)
            | ExpressionKind::Await(value) => expressions.push(value),
            ExpressionKind::Call {
                function,
                arguments,
                keywords,
            } => {
                expressions.push(function);
                expressions.extend(arguments);
                expressions.extend(keywords.iter().map(|(_, value)| value));
            }
            ExpressionKind::ForwardingCall {
                function,
                positional,
                keywords,
            } => {
                expressions.extend([function.as_ref(), positional.as_ref(), keywords.as_ref()]);
            }
            ExpressionKind::KeywordMappingCall {
                function,
                arguments,
                mapping,
            } => {
                expressions.extend([function.as_ref(), mapping.as_ref()]);
                expressions.extend(arguments);
            }
            ExpressionKind::RepeatedKeywordCall {
                function,
                arguments,
            } => {
                expressions.push(function);
                expressions.extend(arguments);
            }
            ExpressionKind::Sequence { items: values, .. }
            | ExpressionKind::Dictionary(values)
            | ExpressionKind::FormattedString(values)
            | ExpressionKind::Boolean { values, .. }
            | ExpressionKind::Addition(values)
            | ExpressionKind::Divide(values)
            | ExpressionKind::Operands(values) => expressions.extend(values),
            ExpressionKind::Comprehension {
                iterators,
                elements,
                ..
            } => {
                expressions.extend(elements);
                for iterator in iterators {
                    expressions.push(&iterator.iterable);
                    expressions.extend(&iterator.filters);
                }
            }
            ExpressionKind::Compare { left, right, .. }
            | ExpressionKind::OrderingComparison { left, right } => {
                expressions.extend([left.as_ref(), right.as_ref()]);
            }
            ExpressionKind::Subscript { value, index } => {
                expressions.extend([value.as_ref(), index.as_ref()]);
            }
            ExpressionKind::Conditional {
                test,
                body,
                otherwise,
            } => expressions.extend([test.as_ref(), body.as_ref(), otherwise.as_ref()]),
            ExpressionKind::Membership {
                element, container, ..
            } => expressions.extend([element.as_ref(), container.as_ref()]),
            ExpressionKind::Text(_)
            | ExpressionKind::IterationVariable(_)
            | ExpressionKind::Bool(_)
            | ExpressionKind::Scalar(_)
            | ExpressionKind::None
            | ExpressionKind::ConstantLambda
            | ExpressionKind::NullaryConstantLambda
            | ExpressionKind::DeferredBoolLambda
            | ExpressionKind::IdentityLambda
            | ExpressionKind::EmptyListComprehension
            | ExpressionKind::IdentityDecoratorLambda => {}
        }
    }
    reads.retain(|name| !assigned.contains(name));
    // Class-local names cannot supply cells to nested functions.
    if !class {
        nested_reads.retain(|name| !assigned.contains(name));
    }
    reads.extend(nested_reads);
    reads
}

// Function locals are declared at compile time, including bindings in
// branches that the runtime index can prove are never executed.
fn suite_binds_name(body: &[Statement], name: &str) -> bool {
    body.iter().any(|statement| match &statement.kind {
        StatementKind::Define(definition) => definition.name.text == name,
        StatementKind::Import(import) => import.local.text == name,
        StatementKind::ProviderWildcardImport(imports) => {
            imports.iter().any(|import| import.local.text == name)
        }
        StatementKind::Assign { names, .. } | StatementKind::AssignUnpack { names, .. } => {
            names.iter().any(|target| target.text == name)
        }
        StatementKind::LiteralRouteLoop { path, .. } => path.text == name,
        StatementKind::If {
            body, otherwise, ..
        } => suite_binds_name(body, name) || suite_binds_name(otherwise, name),
        StatementKind::With { items, body } => {
            items
                .iter()
                .any(|(_, target)| target.as_ref().is_some_and(|target| target.text == name))
                || suite_binds_name(body, name)
        }
        _ => false,
    })
}

fn suite_end(body: &[Statement]) -> [usize; 2] {
    let Some(last) = body.last() else {
        return [0, 0];
    };
    let mut end = position(last.at);
    match &last.kind {
        StatementKind::If {
            body, otherwise, ..
        } => {
            end = end.max(suite_end(body)).max(suite_end(otherwise));
        }
        StatementKind::With { body, .. } => {
            end = end.max(suite_end(body));
        }
        StatementKind::Define(definition) => {
            end = end.max(suite_end(&definition.body));
        }
        _ => {}
    }
    end
}

fn within_package(module: &str, package: &str) -> bool {
    module
        .strip_prefix(package)
        .is_some_and(|suffix| suffix.is_empty() || suffix.starts_with('.'))
}

fn pytest_conftest_prefix(path: &str) -> Option<&str> {
    path.strip_suffix("conftest.py")
        .filter(|prefix| prefix.is_empty() || prefix.ends_with('/'))
}

fn has_implicit_module_member(name: &str, is_package: bool) -> bool {
    // Source modules inherit attributes from ModuleType and its base object.
    matches!(
        name,
        "__name__"
            | "__doc__"
            | "__package__"
            | "__loader__"
            | "__spec__"
            | "__file__"
            | "__cached__"
            | "__builtins__"
            | "__annotations__"
            | "__class__"
            | "__delattr__"
            | "__dict__"
            | "__dir__"
            | "__eq__"
            | "__format__"
            | "__ge__"
            | "__getattribute__"
            | "__getstate__"
            | "__gt__"
            | "__hash__"
            | "__init__"
            | "__init_subclass__"
            | "__le__"
            | "__lt__"
            | "__ne__"
            | "__new__"
            | "__reduce__"
            | "__reduce_ex__"
            | "__repr__"
            | "__setattr__"
            | "__sizeof__"
            | "__str__"
            | "__subclasshook__"
    ) || (is_package && name == "__path__")
}

fn static_bool(expression: &Expression) -> Option<bool> {
    match &expression.kind {
        ExpressionKind::Bool(value) => Some(*value),
        ExpressionKind::Not(value) => static_bool(value).map(|it| !it),
        _ => None,
    }
}

fn is_identity_decorator(definition: &Definition) -> bool {
    let [parameter] = definition.parameters.as_slice() else {
        return false;
    };
    returns_parameter(definition, parameter)
}

fn returns_parameter(definition: &Definition, parameter: &Name) -> bool {
    matches!(body_without_docstring(definition), [Statement { kind: StatementKind::Return(Some(Expression {
        kind: ExpressionKind::Name(name), ..
    })), .. }] if name == &parameter.text)
}

fn identity_decorator_factory(definition: &Definition) -> Option<&Definition> {
    returned_nested_function(definition).filter(|decorator| is_identity_decorator(decorator))
}

fn returned_nested_function(definition: &Definition) -> Option<&Definition> {
    let [
        Statement {
            kind: StatementKind::Define(decorator),
            ..
        },
        Statement {
            kind:
                StatementKind::Return(Some(Expression {
                    kind: ExpressionKind::Name(name),
                    ..
                })),
            ..
        },
    ] = body_without_docstring(definition)
    else {
        return None;
    };
    (name == &decorator.name.text
        && decorator.kind == DefinitionKind::Function
        && decorator.decorators.is_empty()
        && decorator.simple_positional_parameters
        && decorator.annotations.is_empty()
        && decorator.defaults.is_empty())
    .then_some(decorator)
}

fn body_without_docstring(definition: &Definition) -> &[Statement] {
    match definition.body.as_slice() {
        [
            Statement {
                kind:
                    StatementKind::Evaluate(Expression {
                        kind: ExpressionKind::Text(_),
                        ..
                    }),
                ..
            },
            rest @ ..,
        ] => rest,
        body => body,
    }
}

fn is_route(kind: ObjectKind, name: &str, phase03: bool) -> bool {
    let http = matches!(
        name,
        "get" | "post" | "put" | "delete" | "patch" | "head" | "options" | "trace"
    );
    match kind {
        ObjectKind::App | ObjectKind::Router => {
            http || matches!(
                name,
                "api_route" | "route" | "websocket" | "websocket_route"
            ) || phase03 && matches!(name, "exception_handler" | "on_event")
        }
        ObjectKind::FlaskApp | ObjectKind::FlaskBlueprint => {
            http || matches!(name, "route" | "errorhandler")
                || flask_callback_method(kind, name)
                || matches!(kind, ObjectKind::FlaskBlueprint) && name == "app_errorhandler"
        }
        ObjectKind::FlaskLoginManager
        | ObjectKind::TyperApp
        | ObjectKind::SanicApp
        | ObjectKind::StarletteApp
        | ObjectKind::AiohttpRouteTable => false,
        ObjectKind::CeleryApp => name == "task",
    }
}

fn is_imperative_registration(kind: ObjectKind, method: &str) -> bool {
    match kind {
        ObjectKind::App | ObjectKind::Router => {
            matches!(
                method,
                "add_api_route" | "add_api_websocket_route" | "add_route" | "add_websocket_route"
            ) || matches!(kind, ObjectKind::App) && method == "add_exception_handler"
        }
        ObjectKind::FlaskApp | ObjectKind::FlaskBlueprint => {
            matches!(method, "add_url_rule" | "register_error_handler")
                || flask_callback_method(kind, method)
        }
        ObjectKind::FlaskLoginManager => flask_login_callback(method),
        ObjectKind::TyperApp
        | ObjectKind::SanicApp
        | ObjectKind::StarletteApp
        | ObjectKind::AiohttpRouteTable => false,
        ObjectKind::CeleryApp => method == "task",
    }
}

fn is_flask_login_constructor(name: &str) -> bool {
    matches!(
        name,
        "flask_login.LoginManager" | "flask_login.login_manager.LoginManager"
    )
}

fn flask_login_callback(name: &str) -> bool {
    matches!(
        name,
        "user_loader" | "request_loader" | "unauthorized_handler" | "needs_refresh_handler"
    )
}

fn flask_callback_method(kind: ObjectKind, name: &str) -> bool {
    matches!(kind, ObjectKind::FlaskApp | ObjectKind::FlaskBlueprint)
        && (matches!(
            name,
            "before_request" | "after_request" | "teardown_request" | "context_processor"
        ) || match kind {
            ObjectKind::FlaskApp => {
                matches!(name, "shell_context_processor" | "teardown_appcontext")
            }
            ObjectKind::FlaskBlueprint => matches!(
                name,
                "before_app_request"
                    | "after_app_request"
                    | "teardown_app_request"
                    | "app_context_processor"
            ),
            _ => false,
        })
}

fn is_deferred_possible_edge(kind: &str) -> bool {
    is_atexit_edge(kind)
        || matches!(
            kind,
            django_forms::VALIDATION_HOOK
                | "logging_factory"
                | celery_routes::ROUTER
                | celery_routes::ROUTER_PARAMETER
                | "opaque_decorator_argument"
        )
}

fn is_atexit_edge(kind: &str) -> bool {
    matches!(kind, "atexit_registration" | "atexit_argument")
}

fn is_typing_annotated(value: &Value) -> bool {
    matches!(value, Value::Provider(name) if matches!(name.as_str(),
        "typing.Annotated" | "typing_extensions.Annotated"))
}

fn is_fastapi_field_marker(name: &str) -> bool {
    name.strip_prefix("fastapi.").is_some_and(|member| {
        matches!(
            member,
            "Path" | "Query" | "Header" | "Cookie" | "Body" | "Form" | "File"
        )
    })
}

fn is_typer_constructor(name: &str) -> bool {
    matches!(name, "typer.Typer" | "typer.main.Typer")
}

fn is_known_call(name: &str, phase03: bool) -> bool {
    name.starts_with("builtins.")
        || phase03
            && (name.starts_with("pydantic.BaseModel.")
                || is_fastapi_field_marker(name)
                || is_django_signal_constructor(name)
                || is_flask_login_constructor(name)
                || is_typer_constructor(name)
                || is_django_receiver(name)
                || matches!(
                    name,
                    "flask.Flask"
                        | "flask.app.Flask"
                        | "flask.Blueprint"
                        | "flask.blueprints.Blueprint"
                        | "celery.Celery"
                        | "fastapi.Depends"
                        | "fastapi.Security"
                        | "os.getenv"
                        | "starlette.middleware.Middleware"
                        | "fastapi.middleware.Middleware"
                        | "django.urls.path"
                        | "django.urls.re_path"
                        | "django.urls.include"
                        | "django.urls.register_converter"
                        | "django.setup"
                        | "django.core.wsgi.get_wsgi_application"
                        | "django.core.asgi.get_asgi_application"
                        | "django.core.management.execute_from_command_line"
                        | "sqlalchemy.event.listen"
                        | "sqlalchemy.event.listens_for"
                        | "sqlalchemy.event.listens_for.callback"
                        | "sqlalchemy.orm.mapped_column"
                        | "sqlalchemy.Column"
                        | "sqlalchemy.ForeignKey"
                        | "sqlalchemy.orm.relationship"
                        | "sqlmodel.Field"
                        | "sqlmodel.Relationship"
                        | "django.http.HttpResponse"
                        | "django.http.response.HttpResponse"
                        | "django.http.HttpResponseBadRequest"
                        | "django.http.HttpResponseForbidden"
                        | "django.http.HttpResponseNotFound"
                        | "django.http.HttpResponseServerError"
                ))
        || matches!(
            name,
            "fastapi.FastAPI"
                | "fastapi.applications.FastAPI"
                | "fastapi.APIRouter"
                | "fastapi.routing.APIRouter"
                | "concurrent.futures.ThreadPoolExecutor"
                | "concurrent.futures.ProcessPoolExecutor"
                | "multiprocessing.Process"
                | "setuptools.setup"
                | "distutils.core.setup"
        )
}

fn framework_module(file: usize) -> Object {
    (file, 0, 0)
}

fn django_loaded_module(file: usize) -> Object {
    (file, 0, 1)
}

fn django_management_settings(argv: &[Value]) -> DjangoSettings {
    // Django parses global options after the executable and subcommand before
    // consulting INSTALLED_APPS. The last settings option wins.
    let mut arguments = argv.iter().skip(2);
    let mut module = "";
    while let Some(argument) = arguments.next() {
        let Value::Text(option) = argument else {
            return DjangoSettings::Unknown;
        };
        match option.as_str() {
            "--" => break,
            "--settings" => match arguments.next() {
                Some(Value::Text(value)) if !value.starts_with('-') => module = value,
                _ => return DjangoSettings::Unknown,
            },
            _ => {
                if let Some(value) = option.strip_prefix("--settings=") {
                    module = value;
                }
            }
        }
    }
    if module.is_empty() {
        DjangoSettings::Unset
    } else {
        DjangoSettings::Module(module.to_owned())
    }
}

fn is_django_signal(provider: &str) -> bool {
    matches!(
        provider,
        "django.core.signals.request_started"
            | "django.core.signals.request_finished"
            | "django.core.signals.got_request_exception"
            | "django.core.signals.setting_changed"
            | "django.test.signals.setting_changed"
            | "django.test.signals.template_rendered"
    ) || provider
        .strip_prefix("django.db.models.signals.")
        .is_some_and(|name| {
            matches!(
                name,
                "pre_init"
                    | "post_init"
                    | "pre_save"
                    | "post_save"
                    | "pre_delete"
                    | "post_delete"
                    | "m2m_changed"
                    | "class_prepared"
                    | "pre_migrate"
                    | "post_migrate"
            )
        })
}

fn is_django_signal_constructor(provider: &str) -> bool {
    matches!(
        provider,
        "django.dispatch.Signal" | "django.dispatch.dispatcher.Signal"
    )
}

fn click_command_provider(provider: &str) -> bool {
    matches!(provider, "click.command" | "click.decorators.command")
}

fn is_django_receiver(provider: &str) -> bool {
    matches!(
        provider,
        "django.dispatch.receiver" | "django.dispatch.dispatcher.receiver"
    )
}

fn framework_registration_kind(kind: ObjectKind) -> &'static str {
    match kind {
        ObjectKind::App | ObjectKind::Router => "fastapi_registration",
        ObjectKind::FlaskApp | ObjectKind::FlaskBlueprint => "flask_registration",
        ObjectKind::FlaskLoginManager => "flask_login_registration",
        ObjectKind::TyperApp => "typer_callback",
        ObjectKind::SanicApp => "sanic_listener",
        ObjectKind::StarletteApp => "starlette_route",
        ObjectKind::AiohttpRouteTable => aiohttp::DECLARATION,
        ObjectKind::CeleryApp => "celery_task",
    }
}

fn callback_argument<'a>(
    kind: ObjectKind,
    method: &str,
    arguments: &'a [Expression],
    keywords: &'a [(String, Expression)],
) -> Option<&'a Expression> {
    let (position, names): (usize, &[&str]) = match method {
        "add_url_rule" => (2, &["view_func"]),
        "register_error_handler" => (1, &["f"]),
        "add_api_route" | "add_api_websocket_route" => (1, &["endpoint"]),
        "add_route" | "add_websocket_route" => (1, &["endpoint"]),
        "add_exception_handler" => (1, &["handler"]),
        "task" if arguments.len() == 1 && keywords.is_empty() => (0, &[]),
        _ if matches!(kind, ObjectKind::FlaskLoginManager) && flask_login_callback(method) => {
            return registration_arguments(arguments, keywords, ["callback"])
                .and_then(|[callback]| callback);
        }
        _ if flask_callback_method(kind, method) => {
            return match (arguments, keywords) {
                ([callback], []) => Some(callback),
                ([], [(name, callback)]) if name == "f" => Some(callback),
                _ => None,
            };
        }
        _ => return None,
    };
    arguments.get(position).or_else(|| {
        keywords
            .iter()
            .find(|(name, _)| names.contains(&name.as_str()))
            .map(|(_, value)| value)
    })
}

fn registration_arguments<'a, const N: usize>(
    positional: &'a [Expression],
    keywords: &'a [(String, Expression)],
    names: [&str; N],
) -> Option<[Option<&'a Expression>; N]> {
    if positional.len() > N {
        return None;
    }
    let mut slots = [None; N];
    for (slot, value) in slots.iter_mut().zip(positional) {
        *slot = Some(value);
    }
    for (name, value) in keywords {
        let index = names.iter().position(|key| key == name)?;
        if slots[index].replace(value).is_some() {
            return None;
        }
    }
    Some(slots)
}

fn builtin_noop_subclass_hook(name: &str) -> bool {
    // These subclassable built-ins inherit object's no-op hook.
    matches!(
        name,
        "builtins.object"
            | "builtins.str"
            | "builtins.int"
            | "builtins.list"
            | "builtins.dict"
            | "builtins.tuple"
            | "builtins.set"
            | "builtins.float"
            | "builtins.bytes"
            | "builtins.ValueError"
            | "builtins.RuntimeError"
            | "builtins.AttributeError"
            | "builtins.TypeError"
            | "builtins.Exception"
    )
}

fn framework_class_provider(name: &str) -> Option<FrameworkClassRole> {
    match name {
        "builtins.dict" => Some(FrameworkClassRole::BuiltinDict),
        "ast.NodeVisitor" => Some(FrameworkClassRole::StdlibAstVisitor),
        "asyncio.Protocol" | "asyncio.protocols.Protocol" => {
            Some(FrameworkClassRole::StdlibAsyncProtocol)
        }
        "ctypes.Structure" => Some(FrameworkClassRole::StdlibCtypesStructure),
        "traitlets.config.LoggingConfigurable"
        | "traitlets.config.configurable.LoggingConfigurable"
        | "traitlets.config.Configurable"
        | "traitlets.config.configurable.Configurable" => {
            Some(FrameworkClassRole::TraitletsConfigurable)
        }
        "unittest.TestCase" | "unittest.case.TestCase" => Some(FrameworkClassRole::Unittest),
        "unittest.IsolatedAsyncioTestCase" | "unittest.async_case.IsolatedAsyncioTestCase" => {
            Some(FrameworkClassRole::UnittestAsync)
        }
        "django.test.TestCase" | "django.test.testcases.TestCase" => {
            Some(FrameworkClassRole::DjangoDatabaseTestCase)
        }
        "django.test.SimpleTestCase"
        | "django.test.testcases.SimpleTestCase"
        | "django.test.TransactionTestCase"
        | "django.test.testcases.TransactionTestCase"
        | "django.test.LiveServerTestCase"
        | "django.test.testcases.LiveServerTestCase"
        | "django.contrib.staticfiles.testing.StaticLiveServerTestCase" => {
            Some(FrameworkClassRole::DjangoTestCase)
        }
        "django.apps.AppConfig" | "django.apps.config.AppConfig" => {
            Some(FrameworkClassRole::DjangoAppConfig)
        }
        "django.core.management.base.BaseCommand" => Some(FrameworkClassRole::DjangoCommand),
        "django.views.View"
        | "django.views.generic.View"
        | "django.views.generic.base.View"
        | "django.views.generic.TemplateView"
        | "django.views.generic.base.TemplateView"
        | "django.views.generic.RedirectView"
        | "django.views.generic.base.RedirectView"
        | "django.views.generic.DetailView"
        | "django.views.generic.detail.DetailView"
        | "django.views.generic.ListView"
        | "django.views.generic.list.ListView"
        | "django.views.generic.FormView"
        | "django.views.generic.edit.FormView"
        | "django.views.generic.CreateView"
        | "django.views.generic.edit.CreateView"
        | "django.views.generic.UpdateView"
        | "django.views.generic.edit.UpdateView"
        | "django.views.generic.DeleteView"
        | "django.views.generic.edit.DeleteView" => Some(FrameworkClassRole::DjangoView),
        "django.db.models.Model" | "django.db.models.base.Model" => {
            Some(FrameworkClassRole::DjangoModel)
        }
        "django.db.models.Manager" | "django.db.models.manager.Manager" => {
            Some(FrameworkClassRole::DjangoManager)
        }
        "django.db.models.QuerySet" | "django.db.models.query.QuerySet" => {
            Some(FrameworkClassRole::DjangoQuerySet)
        }
        "django.forms.Form" | "django.forms.forms.Form" => Some(FrameworkClassRole::DjangoForm),
        "django.forms.ModelForm" | "django.forms.models.ModelForm" => {
            Some(FrameworkClassRole::DjangoModelForm)
        }
        name if django_forms::is_relationship_field_class(name) => {
            Some(FrameworkClassRole::DjangoRelationshipField)
        }
        name if django_forms::provider_field_role(name).is_some() => {
            django_forms::provider_field_role(name)
        }
        name if is_django_signal_constructor(name) => Some(FrameworkClassRole::DjangoSignal),
        "django.contrib.admin.ModelAdmin" | "django.contrib.admin.options.ModelAdmin" => {
            Some(FrameworkClassRole::DjangoAdmin)
        }
        "guardian.admin.GuardedModelAdmin" => Some(FrameworkClassRole::DjangoGuardianAdmin),
        "django.contrib.admin.SimpleListFilter"
        | "django.contrib.admin.filters.SimpleListFilter" => {
            Some(FrameworkClassRole::DjangoSimpleListFilter)
        }
        "storages.backends.s3.S3Storage" | "storages.backends.s3boto3.S3Boto3Storage" => {
            Some(FrameworkClassRole::DjangoS3Storage)
        }
        "storages.base.BaseStorage" => Some(FrameworkClassRole::DjangoStorageBase),
        "click.Command" | "click.core.Command" => Some(FrameworkClassRole::ClickCommand),
        "rest_framework.serializers.Serializer" | "rest_framework.serializers.ModelSerializer" => {
            Some(FrameworkClassRole::DrfSerializer)
        }
        "rest_framework.serializers.Field" | "rest_framework.fields.Field" => {
            Some(FrameworkClassRole::DrfField)
        }
        "rest_framework.authtoken.views.ObtainAuthToken" => {
            Some(FrameworkClassRole::DrfAuthTokenView)
        }
        "rest_framework.viewsets.ViewSet"
        | "rest_framework.viewsets.GenericViewSet"
        | "rest_framework.viewsets.ModelViewSet"
        | "rest_framework.viewsets.ReadOnlyModelViewSet" => Some(FrameworkClassRole::DrfViewSet),
        "celery.Task" | "celery.app.task.Task" => Some(FrameworkClassRole::CeleryTask),
        "celery.backends.redis.RedisBackend" => Some(FrameworkClassRole::CeleryRedisBackend),
        "sqlalchemy.orm.DeclarativeBase"
        | "sqlalchemy.orm.decl_api.DeclarativeBase"
        | "sqlalchemy.orm.declarative_base.result" => Some(FrameworkClassRole::SqlAlchemyModel),
        "sqlmodel.SQLModel" | "sqlmodel.main.SQLModel" => Some(FrameworkClassRole::SqlModel),
        "pydantic.BaseModel"
        | "pydantic.main.BaseModel"
        | "pydantic_settings.BaseSettings"
        | "pydantic_settings.main.BaseSettings" => Some(FrameworkClassRole::PydanticModel),
        "marshmallow.Schema" | "marshmallow.schema.Schema" => {
            Some(FrameworkClassRole::MarshmallowSchema)
        }
        _ => None,
    }
}

fn framework_managed_base(name: &str) -> bool {
    framework_class_provider(name).is_some()
        || is_django_string_converter(name)
        || logging_config::LoggingBase::provider(name).is_some()
        || django_migrations::is_migration_base(name)
}

fn is_bounded_declaration_provider(name: &str) -> bool {
    wagtail::is_snippet_base_provider(name)
        || django_forms::is_declaration_provider(name)
        || django_choices::is_declaration_provider(name)
}

fn is_django_runpython(name: &str) -> bool {
    matches!(
        name,
        "django.db.migrations.RunPython"
            | "django.db.migrations.operations.RunPython"
            | "django.db.migrations.operations.special.RunPython"
    )
}

fn is_django_string_converter(name: &str) -> bool {
    name == "django.urls.converters.StringConverter"
}

fn is_declarative_base_factory(name: &str) -> bool {
    matches!(
        name,
        "sqlalchemy.orm.declarative_base"
            | "sqlalchemy.orm.decl_api.declarative_base"
            | "sqlalchemy.ext.declarative.declarative_base"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use skarmory_python::phase02::extract;

    fn module(path: &str, name: &str, text: &str) -> AnalyzedFile {
        AnalyzedFile {
            path: path.into(),
            module: name.into(),
            is_package: path.ends_with("__init__.py"),
            fastapi_router_mode: FastapiRouterMode::Auto,
            facts: extract(text).unwrap(),
        }
    }

    #[test]
    fn fastapi_initializer_limit_withholds_affected_routes_and_preserves_an_independent_app() {
        let route =
            "from fastapi import FastAPI\napp = FastAPI()\n@app.get('/')\ndef route(): return 1\n";
        let helpers = "def leaf(): pass\ndef initialize():\n    leaf()\n    leaf()\n";
        let scan = |invoke| {
            analyze_with_verification(
                &[
                    module("app.py", "app", &format!("{route}{helpers}{invoke}")),
                    module("other.py", "other", route),
                ],
                &[],
                None,
                None,
                None,
                true,
                FrameworkInputs {
                    fastapi_call_event_limit: Some(2),
                    ..Default::default()
                },
            )
        };
        let roots = |report: &Analysis| {
            report
                .evidence
                .iter()
                .filter(|row| row.kind == "fastapi_registration" && row.certainty == "certain")
                .map(|row| row.full_name.clone())
                .collect::<Vec<_>>()
        };
        let reached = scan("initialize()\n");
        assert_eq!(reached.fastapi_initializer_evidence.call_events, 2);
        assert!(reached.fastapi_initializer_evidence.limited);
        assert_eq!(roots(&reached), ["other.route"]);
        assert!(
            reached
                .issues
                .iter()
                .any(|issue| issue.kind == "analysis_limit" && issue.path == "app.py")
        );

        let dormant = scan("");
        assert_eq!(dormant.fastapi_initializer_evidence.call_events, 0);
        assert!(dormant.issues.is_empty());
        assert_eq!(roots(&dormant), ["app.route", "other.route"]);
    }

    #[test]
    #[ignore = "requires the pinned FastAPI initializer call-budget fixture"]
    fn pinned_fastapi_initializer_call_budget() {
        let fixture = std::env::var_os("SKARMORY_PHASE03_FASTAPI_BUDGET_FIXTURE").unwrap();
        let output = std::env::var_os("SKARMORY_PHASE03_FASTAPI_BUDGET_REPORT").unwrap();
        let limit: usize = std::env::var("SKARMORY_PHASE03_FASTAPI_CALL_LIMIT")
            .unwrap()
            .parse()
            .unwrap();
        let source = std::fs::read_to_string(fixture).unwrap();
        let report = analyze_with_verification(
            &[module("app.py", "app", &source)],
            &[],
            None,
            None,
            None,
            true,
            FrameworkInputs {
                fastapi_call_event_limit: Some(limit),
                ..Default::default()
            },
        );
        let evidence = report.fastapi_initializer_evidence;
        let roots: BTreeSet<_> = report
            .evidence
            .iter()
            .filter(|row| row.kind == "fastapi_registration" && row.certainty == "certain")
            .map(|row| (&row.file, row.line))
            .collect();
        let result = serde_json::json!({
            "complete": report.issues.is_empty(),
            "incomplete_reason": evidence.limited.then_some("call_event_budget"),
            "issues": report.issues,
            "call_events": evidence.call_events,
            "roots": roots,
            "callback_parameters": evidence.callback_parameters,
            "injected_parameters": evidence.injected_parameters,
        });
        std::fs::write(output, serde_json::to_vec(&result).unwrap()).unwrap();
    }

    fn names(result: &Analysis, category: Category) -> Vec<&str> {
        let mut names: Vec<_> = result
            .findings
            .iter()
            .filter(|finding| finding.category == category)
            .map(|finding| finding.name.as_str())
            .collect();
        names.sort_unstable();
        names
    }

    #[test]
    fn django_admin_stub_exemption_requires_a_package_and_a_genuine_import() {
        let build = |source| {
            [
                module("settings.py", "settings", source),
                module("shop/__init__.py", "shop", ""),
                module(
                    "shop/admin.py",
                    "shop.admin",
                    "# Register your models here.\n",
                ),
                module("shop/placeholder.py", "shop.placeholder", "# Empty.\n"),
                module("standalone/admin.py", "standalone.admin", "# Empty.\n"),
            ]
        };
        let context = analyze_phase03(&build("from django.apps import AppConfig\n"), &[]);
        assert_eq!(
            names(&context, Category::File),
            ["shop/placeholder.py", "standalone/admin.py"]
        );
        let unrelated = analyze_phase03(&build("from elsewhere import AppConfig\n"), &[]);
        assert_eq!(
            names(&unrelated, Category::File),
            [
                "shop/admin.py",
                "shop/placeholder.py",
                "standalone/admin.py"
            ]
        );
        let mut shadowed = Vec::from(build("from django.apps import AppConfig\n"));
        shadowed.push(module("src/django/__init__.py", "src.django", ""));
        let shadowed = analyze_phase03(&shadowed, &[]);
        assert!(names(&shadowed, Category::File).contains(&"shop/admin.py"));
    }

    #[test]
    fn django_wsgi_terminal_initializer_keeps_prefix_effects_and_unused_findings() {
        let prefix = "import os\nos.environ.setdefault('DJANGO_SETTINGS_MODULE', 'settings')\n";
        for (tail, complete) in [
            ("application = missing_factory()\n", true),
            (
                "from django.core.wsgi import get_wsgi_application\nget_wsgi_application = missing_factory\napplication = get_wsgi_application()\n",
                true,
            ),
            (
                "import django\ndjango = missing_replacement\ndjango.setup()\napplication = missing_factory()\n",
                true,
            ),
            (
                "from outside import supplied_factory\napplication = supplied_factory()\n",
                false,
            ),
        ] {
            let files = [
                module("settings.py", "settings", "DEBUG = True\n"),
                module("wsgi.py", "wsgi", &format!("{prefix}{tail}")),
            ];
            let report = analyze_phase03(&files, &[]);
            assert_eq!(
                report.issues.is_empty(),
                complete,
                "{tail}: {:?}",
                report.issues
            );
            assert!(!report.findings.iter().any(|finding| {
                finding.category == Category::Import && finding.full_name == "os"
            }));
            for qualified in ["settings.DEBUG", "wsgi.application"] {
                assert!(
                    report.findings.iter().any(|finding| {
                        finding.category == Category::Variable && finding.full_name == qualified
                    }),
                    "{tail}: no unused finding for {qualified}"
                );
            }
        }
    }

    #[test]
    fn django_settings_extracts_a_deferred_literal_callback() {
        let source = concat!(
            "DEBUG_TOOLBAR_CONFIG = {'SHOW_TOOLBAR_CALLBACK': lambda request: True}\n",
            "def optional_helper(): return 1\n",
        );
        let report = analyze_phase03(&[module("settings.py", "settings", source)], &[]);
        assert!(report.issues.is_empty(), "{:?}", report.issues);
        assert!(names(&report, Category::Function).contains(&"optional_helper"));

        let called = concat!(
            "def make_request(): return None\n",
            "(lambda request: True)(make_request())\n",
        );
        let report = analyze_phase03(&[module("consumer.py", "consumer", called)], &[]);
        assert!(
            report
                .issues
                .iter()
                .any(|issue| issue.kind == "unresolved_lambda_execution" && issue.line == Some(2)),
            "{:?}",
            report.issues
        );
    }

    #[test]
    fn django_settings_replays_literal_backend_list_edits_and_global_reads() {
        let cases = [
            (
                r#"AUTHENTICATION_BACKENDS = ['shop.backends.Retired']
AUTHENTICATION_BACKENDS *= 0
AUTHENTICATION_BACKENDS += ['shop.backends.Removed', 'shop.backends.Retired']
del AUTHENTICATION_BACKENDS[0]
AUTHENTICATION_BACKENDS[-1] = 'shop.backends.Active'
"#,
                "shop.backends.Active.get_user",
                "django_auth_backend",
            ),
            (
                r#"STORAGES = {}
def configure():
    global STORAGES
    STORAGES.update({'default': {'BACKEND': 'shop.backends.Active'}})
configure()
"#,
                "shop.backends.Active.save",
                "django_storage_backend",
            ),
        ];
        for (configuration, hook, kind) in cases {
            let settings =
                format!("INSTALLED_APPS = ['shop']\nSECRET_KEY = 'fixture'\n{configuration}");
            let files = [
                module("shop/__init__.py", "shop", ""),
                module("shop/settings.py", "shop.settings", &settings),
                module(
                    "shop/backends.py",
                    "shop.backends",
                    concat!(
                        "class Active:\n    def get_user(self, user_id): return user_id\n",
                        "    def save(self, name, value): return name\n",
                        "class Removed:\n    def get_user(self, user_id): return None\n",
                        "class Retired:\n    def get_user(self, user_id): return None\n",
                    ),
                ),
            ];
            let report = analyze_phase03(&files, &[]);
            assert!(report.issues.is_empty(), "{:?}", report.issues);
            let hooks: Vec<_> = report
                .evidence
                .iter()
                .filter_map(|row| {
                    (row.kind == kind).then_some((row.full_name.as_str(), row.certainty))
                })
                .collect();
            assert_eq!(hooks, [(hook, "possible")]);
        }
    }

    #[test]
    fn django_auth_backend_bare_annotation_preserves_the_effective_member() {
        for (body, expected) in [
            (
                "    get_user: object\n",
                Some("shop.backends.Parent.get_user"),
            ),
            ("    get_user: object = None\n", None),
            (
                "    def get_user(self, key): return key\n    get_user: object\n",
                Some("shop.backends.Selected.get_user"),
            ),
        ] {
            let backends = format!(
                "class Parent:\n    def get_user(self, key): return None\nclass Selected(Parent):\n{body}"
            );
            let files = [
                module("shop/__init__.py", "shop", ""),
                module(
                    "shop/settings.py",
                    "shop.settings",
                    "INSTALLED_APPS = ['shop']\nSECRET_KEY = 'fixture'\nAUTHENTICATION_BACKENDS = ['shop.backends.Selected']\n",
                ),
                module("shop/backends.py", "shop.backends", &backends),
            ];
            let report = analyze_phase03(&files, &[]);
            assert!(report.issues.is_empty(), "{:?}", report.issues);
            let hooks: Vec<_> = report
                .evidence
                .iter()
                .filter(|row| row.kind == "django_auth_backend")
                .map(|row| row.full_name.as_str())
                .collect();
            assert_eq!(hooks, expected.into_iter().collect::<Vec<_>>());
        }
    }

    #[test]
    fn django_logging_settings_follow_inherited_standard_formatter_hooks() {
        let source = |version| {
            format!(
                "INSTALLED_APPS = ['shop']\nSECRET_KEY = 'fixture'\nLOGGING = {{{version}}}\n\
             def configure():\n    global LOGGING\n    LOGGING.update({{'formatters': {{'active': {{'()': 'shop.formatters.Formatter'}}}}}})\nconfigure()\n"
            )
        };
        for (version, selected) in [("'version': 1", true), ("", false)] {
            let settings = source(version);
            let report = analyze_phase03(
                &[
                    module("shop/__init__.py", "shop", ""),
                    module("shop/settings.py", "shop.settings", &settings),
                    module(
                        "shop/formatters.py",
                        "shop.formatters",
                        "import json\nimport logging\nclass Base(logging.Formatter):\n    def format(self, record):\n        helper()\n        return json.dumps({'name': record.name, 'message': record.getMessage()})\nclass Formatter(Base):\n    def retired(self): return 1\ndef helper(): return 'message'\n",
                    ),
                ],
                &[],
            );
            assert!(report.issues.is_empty(), "{version}: {:?}", report.issues);
            assert_eq!(
                report
                    .evidence
                    .iter()
                    .any(|row| row.kind == "django_logging_factory"
                        && row.full_name == "shop.formatters.Base.format"),
                selected
            );
            assert_eq!(
                report
                    .evidence
                    .iter()
                    .any(|row| row.kind == "direct_call"
                        && row.full_name == "shop.formatters.helper"),
                selected
            );
        }
    }

    #[test]
    fn log_record_processing_requires_the_selected_formatter_contract() {
        let report = analyze_phase03(
            &[
                module("shop/__init__.py", "shop", ""),
                module(
                    "shop/settings.py",
                    "shop.settings",
                    "INSTALLED_APPS = ['shop']\nSECRET_KEY = 'fixture'\nLOGGING = {'version': 1, 'formatters': {'active': {'()': 'shop.formatters.Formatter'}}}\n",
                ),
                module(
                    "shop/formatters.py",
                    "shop.formatters",
                    "import logging\nclass Formatter(logging.Formatter):\n    def format(self, record): return record.getMessage()\nclass Custom:\n    def getMessage(self): return 'test'\nFormatter().format(Custom())\n",
                ),
            ],
            &[],
        );
        assert!(
            report
                .issues
                .iter()
                .any(|issue| issue.kind == "unresolved_call")
        );
    }

    #[test]
    fn django_settings_prove_literal_logging_versions_through_named_imports() {
        let formatters = "import logging\nclass Formatter(logging.Formatter):\n    def format(self, record): return record.getMessage()\n";
        let factories = "'formatters': {'active': {'()': 'shop.formatters.Formatter'}}";
        for version in [None, Some(0), Some(1), Some(2)] {
            let version_field =
                version.map_or_else(String::new, |value| format!("'version': {value}, "));
            let logging = format!("{{{version_field}{factories}}}");
            let base = "INSTALLED_APPS = ['shop']\nSECRET_KEY = 'fixture'\n";
            let origin = format!("ORIGINAL = {logging}\n");
            let updated = if let Some(version) = version {
                format!(
                    "{base}VERSION = {version}\nLOGGING = {{'version': VERSION, {factories}}}\n"
                )
            } else {
                format!("{base}LOGGING = {{}}\nLOGGING.update({logging})\n")
            };
            for settings in [
                format!("{base}LOGGING = {logging}\n"),
                updated,
                format!("{base}from shop.reexports import EXPORTED as LOGGING\n"),
                format!("{base}LOGGING = {logging}\nif enabled:\n    LOGGING = {{}}\n"),
                format!("{base}if enabled:\n    from shop.reexports import EXPORTED as LOGGING\n"),
            ] {
                let report = analyze_phase03(
                    &[
                        module("shop/__init__.py", "shop", ""),
                        module("shop/settings.py", "shop.settings", &settings),
                        module(
                            "shop/reexports.py",
                            "shop.reexports",
                            "from shop.logging_data import ORIGINAL as EXPORTED\n",
                        ),
                        module("shop/logging_data.py", "shop.logging_data", &origin),
                        module("shop/formatters.py", "shop.formatters", formatters),
                    ],
                    &[],
                );
                assert!(report.issues.is_empty(), "{settings}: {:?}", report.issues);
                assert_eq!(
                    report
                        .evidence
                        .iter()
                        .any(|row| row.kind == "django_logging_factory"),
                    version == Some(1),
                    "{version:?}: {settings}"
                );
            }
        }
    }

    #[test]
    fn wagtail_registration_and_block_schema_use_canonical_providers() {
        let providers = "from django.db import models\nfrom wagtail.contrib.settings.models import BaseSiteSetting, register_setting\nfrom wagtail.blocks import CharBlock, ListBlock, StreamBlock, StructBlock\n";
        let declarations = "@register_setting(icon='site')\nclass SiteSettings(BaseSiteSetting):\n    footer = models.CharField(max_length=10)\n    unrelated = 1\n\nclass Quote(StructBlock):\n    text = CharBlock()\n    class Meta:\n        label_format = '{text}'\n        ignored = 'control'\n\nclass Body(StreamBlock):\n    items = ListBlock(CharBlock())\n";
        let report = analyze_phase03(
            &[module(
                "site/models.py",
                "site.models",
                &format!("{providers}{declarations}"),
            )],
            &[],
        );
        assert!(report.issues.is_empty(), "{:?}", report.issues);
        for (name, kind) in [
            ("site.models.SiteSettings", "wagtail_setting_registration"),
            (
                "site.models.SiteSettings.footer",
                "wagtail_field_declaration",
            ),
            ("site.models.Quote.text", "wagtail_field_declaration"),
            ("site.models.Body.items", "wagtail_field_declaration"),
        ] {
            assert!(
                report
                    .evidence
                    .iter()
                    .any(|row| row.full_name == name && row.kind == kind),
                "{name}: {:?}",
                report.evidence
            );
        }
        assert!(
            report
                .evidence
                .iter()
                .any(|row| row.full_name.ends_with(".Meta.label_format")
                    && row.kind == "wagtail_block_metadata")
        );
        let reported: BTreeSet<_> = report
            .findings
            .iter()
            .map(|row| row.full_name.as_str())
            .collect();
        assert!(!reported.iter().any(|name| name.ends_with(".footer")));
        assert!(!reported.iter().any(|name| name.ends_with(".items")));
        assert!(!reported.iter().any(|name| name.ends_with(".label_format")));
        assert!(reported.iter().any(|name| name.ends_with(".unrelated")));
        assert!(
            !report
                .evidence
                .iter()
                .any(|row| row.kind.starts_with("wagtail_")
                    && (row.full_name.ends_with(".unrelated")
                        || row.full_name.ends_with(".ignored")))
        );

        let report = analyze_phase03(
            &[module(
                "site/models.py",
                "site.models",
                &format!(
                    "{providers}from external import ICON\n{}",
                    declarations.replace("icon='site'", "icon=ICON")
                ),
            )],
            &[],
        );
        assert!(
            report
                .issues
                .iter()
                .any(|issue| issue.kind == "unresolved_registration")
        );
        assert!(
            !report
                .evidence
                .iter()
                .any(|row| row.kind == "wagtail_setting_registration")
        );

        let report = analyze_phase03(
            &[
                module("wagtail/__init__.py", "wagtail", ""),
                module(
                    "site/models.py",
                    "site.models",
                    &format!("{providers}{declarations}"),
                ),
            ],
            &[],
        );
        assert!(
            !report
                .evidence
                .iter()
                .any(|row| row.kind.starts_with("wagtail_"))
        );
    }

    #[test]
    fn wagtail_models_retain_only_proven_fields_and_callbacks() {
        let source = "from django.db import models\nfrom modelcluster.fields import ParentalKey\nfrom wagtail.models import Page, Orderable, Task\nfrom wagtail.fields import RichTextField\nfrom wagtail.admin.panels import FieldPanel\nclass Home(Page):\n    lead = RichTextField(blank=True)\n    hero = models.ForeignKey('wagtailimages.Image', on_delete=models.SET_NULL, null=True)\n    content_panels = Page.content_panels + [FieldPanel('lead')]\n    subpage_types = ['home.Home']\n    max_count = 2\n    def get_preview_template(self, request, mode): return 'page.html'\n    def paginate(self, request, *args): return request\n    class Meta:\n        verbose_name = 'Home'\nclass MenuItem(Orderable):\n    owner = ParentalKey('home.Home', related_name='menu')\nclass Review(Task):\n    task_state_class = None\n    def user_can_access_editor(self, obj, user): return False\nclass CustomPreview(Page):\n    def serve_preview(self, request, mode): return None\n    def get_preview_template(self, request, mode): return 'unused.html'\nclass AbstractPage(Page):\n    class Meta:\n        abstract = True\n";
        let report = analyze_phase03(&[module("home/models.py", "home.models", source)], &[]);
        assert!(report.issues.is_empty(), "{:?}", report.issues);
        for (kind, ending) in [
            ("wagtail_field_declaration", ".Home.lead"),
            ("wagtail_field_declaration", ".Home.hero"),
            ("wagtail_field_declaration", ".MenuItem.owner"),
            ("wagtail_model_declaration", ".MenuItem"),
            ("wagtail_model_declaration", ".Review"),
            ("wagtail_model_option", ".Home.content_panels"),
            ("wagtail_model_option", ".Home.subpage_types"),
            ("wagtail_model_option", ".Home.max_count"),
            ("wagtail_model_option", ".Meta.verbose_name"),
            ("wagtail_model_option", ".Review.task_state_class"),
            (
                "wagtail_callback_parameter",
                ".Home.get_preview_template.request",
            ),
            (
                "wagtail_callback_parameter",
                ".Home.get_preview_template.mode",
            ),
            (
                "wagtail_callback_parameter",
                ".Review.user_can_access_editor.obj",
            ),
        ] {
            assert!(
                report
                    .evidence
                    .iter()
                    .any(|row| row.kind == kind && row.full_name.ends_with(ending)),
                "{ending}: {:?}",
                report.evidence
            );
        }
        assert!(!report.evidence.iter().any(|row| {
            row.kind.starts_with("wagtail_")
                && (row.full_name.contains(".paginate")
                    || row.full_name.ends_with(".AbstractPage")
                    || row
                        .full_name
                        .contains(".CustomPreview.get_preview_template"))
        }));
        assert!(
            report
                .findings
                .iter()
                .any(|row| row.full_name.ends_with(".paginate.args"))
        );
    }

    #[test]
    fn wagtail_unregistered_snippet_does_not_retain_viewset_options() {
        let report = analyze_phase03(
            &[module(
                "home/views.py",
                "home.views",
                concat!(
                    "from wagtail.snippets.views.snippets import SnippetViewSet\n",
                    "class PersonViewSet(SnippetViewSet):\n",
                    "    list_display = ('name',)\n",
                    "    sort_order_field = 'sort_order'\n",
                    "    unrelated_option = 1\n",
                ),
            )],
            &[],
        );
        assert!(report.issues.is_empty(), "{:?}", report.issues);
        for member in ["list_display", "sort_order_field", "unrelated_option"] {
            assert!(
                report
                    .findings
                    .iter()
                    .any(|row| { row.full_name == format!("home.views.PersonViewSet.{member}") }),
                "{member}: {:?}",
                report.findings
            );
        }
        assert!(
            !report
                .evidence
                .iter()
                .any(|row| row.kind.starts_with("wagtail_")
                    && row.full_name.starts_with("home.views.PersonViewSet")),
            "{:?}",
            report.evidence
        );
    }

    #[test]
    fn wagtail_snippet_subclass_hook_preserves_unbounded_inputs() {
        for (source, expected_issue) in [
            (
                "from vendor import SnippetViewSet\nclass Child(SnippetViewSet): pass\n",
                "unresolved_subclass_hook",
            ),
            (
                concat!(
                    "from wagtail.snippets.views.snippets import SnippetViewSet\n",
                    "class Mixin: pass\n",
                    "class Child(SnippetViewSet, Mixin): pass\n",
                ),
                "unresolved_subclass_hook",
            ),
            (
                concat!(
                    "from wagtail.snippets.views.snippets import SnippetViewSet\n",
                    "SnippetViewSet.__init_subclass__ = lambda cls: None\n",
                    "class Child(SnippetViewSet): pass\n",
                ),
                "unresolved_attribute_write",
            ),
            (
                concat!(
                    "from wagtail.snippets.views.snippets import SnippetViewSet\n",
                    "def replacement(cls): pass\n",
                    "def patch(parent):\n",
                    "    setattr(parent, '__init_subclass__', classmethod(replacement))\n",
                    "patch(SnippetViewSet)\n",
                    "class Child(SnippetViewSet): pass\n",
                ),
                "unresolved_attribute_write",
            ),
            (
                concat!(
                    "from wagtail.snippets.views.snippets import SnippetViewSet\n",
                    "def replacement(cls): pass\n",
                    "def patch(parent=SnippetViewSet):\n",
                    "    setattr(parent, '__init_subclass__', classmethod(replacement))\n",
                    "patch()\n",
                    "class Child(SnippetViewSet): pass\n",
                ),
                "unresolved_attribute_write",
            ),
            (
                concat!(
                    "from wagtail.snippets.views.snippets import SnippetViewSet\n",
                    "setattr(SnippetViewSet, '__init_subclass__', lambda cls: None)\n",
                    "class Child(SnippetViewSet): pass\n",
                ),
                "unresolved_attribute_write",
            ),
            (
                concat!(
                    "from wagtail.snippets.views.snippets import SnippetViewSet\n",
                    "from wagtail.snippets.models import register_snippet\n",
                    "class Child(SnippetViewSet): pass\n",
                    "register_snippet(Child)\n",
                ),
                "unresolved_call",
            ),
        ] {
            let report = analyze_phase03(&[module("views.py", "views", source)], &[]);
            assert!(
                report
                    .issues
                    .iter()
                    .any(|issue| issue.kind == expected_issue),
                "{source}: {:?}",
                report.issues
            );
        }
    }

    #[test]
    fn plain_django_widget_and_char_field_preserve_local_unused_parameters() {
        let report = analyze_phase03(
            &[
                module(
                    "views.py",
                    "views",
                    concat!(
                        "from django import forms\n",
                        "from django.views import View\n",
                        "class Thing(View):\n",
                        "    def helper(self, unused_arg): return None\n",
                        "class PickerWidget(forms.Widget):\n",
                        "    def value_from_datadict(self, data, files, name): return data.get(name)\n",
                        "def use(): return Thing, PickerWidget\n",
                        "use()\n",
                    ),
                ),
                module(
                    "fields.py",
                    "fields",
                    concat!(
                        "from django.db import models\n",
                        "class Upper(models.CharField):\n",
                        "    def from_db_value(self, value, expression, connection): return value\n",
                        "def _private(value, unused_flag): return value\n",
                        "_private(1, 2)\n",
                    ),
                ),
            ],
            &[],
        );
        assert!(report.issues.is_empty(), "{:?}", report.issues);
        for parameter in [
            "views.Thing.helper.unused_arg",
            "views.PickerWidget.value_from_datadict.files",
            "fields.Upper.from_db_value.expression",
            "fields.Upper.from_db_value.connection",
            "fields._private.unused_flag",
        ] {
            assert!(
                report
                    .findings
                    .iter()
                    .any(|row| row.category == Category::Parameter && row.full_name == parameter),
                "{parameter}: {:?}",
                report.findings
            );
        }
    }

    #[test]
    fn django_widget_and_char_field_keep_unmodeled_class_effects_incomplete() {
        for (source, issue) in [
            (
                concat!(
                    "from django.forms.widgets import Widget\n",
                    "class Child(Widget):\n",
                    "    class Media:\n",
                    "        js = ['app.js']\n",
                    "Child().media\n",
                ),
                "unresolved_subclass_hook",
            ),
            (
                concat!(
                    "from django.forms.widgets import Widget\n",
                    "from django.forms.widgets import MediaDefiningClass\n",
                    "def replacement(*args): return None\n",
                    "setattr(MediaDefiningClass, '__new__', replacement)\n",
                    "class Child(Widget): pass\n",
                ),
                "unresolved_attribute_write",
            ),
            (
                concat!(
                    "from django.db.models import CharField\n",
                    "def patch(parent):\n",
                    "    setattr(parent, '__init_subclass__', lambda: None)\n",
                    "patch(CharField)\n",
                    "class Child(CharField): pass\n",
                ),
                "unresolved_attribute_write",
            ),
        ] {
            let report = analyze_phase03(&[module("app.py", "app", source)], &[]);
            assert!(
                report.issues.iter().any(|row| row.kind == issue),
                "{source}: {:?}",
                report.issues
            );
        }
    }

    #[test]
    fn django_literal_choices_are_read_at_declaration_and_by_adjacent_model_field() {
        let report = analyze_phase03(
            &[module(
                "models.py",
                "models",
                concat!(
                    "from django.db import models\n",
                    "class Priority(models.IntegerChoices):\n",
                    "    LOW = 1, 'Low'\n",
                    "    HIGH = 3, 'High'\n",
                    "class Product(models.Model):\n",
                    "    class Themes(models.TextChoices):\n",
                    "        DARK = 'dark', 'Dark'\n",
                    "        LIGHT = 'light', 'Light'\n",
                    "    theme = models.CharField(choices=Themes.choices, max_length=20)\n",
                    "    def has_changed(self): return False\n",
                ),
            )],
            &[],
        );
        assert!(report.issues.is_empty(), "{:?}", report.issues);
        for name in [
            "models.Priority.LOW",
            "models.Priority.HIGH",
            "models.Product.Themes.DARK",
            "models.Product.Themes.LIGHT",
        ] {
            assert!(
                report
                    .evidence
                    .iter()
                    .any(|row| { row.kind == "django_choice_member" && row.full_name == name })
            );
        }
        assert!(
            report
                .findings
                .iter()
                .any(|row| row.full_name == "models.Product.has_changed")
        );
    }

    #[test]
    fn django_choices_keep_dynamic_class_creation_and_reads_incomplete() {
        let cases = [
            (
                "class Options(TextChoices):\n    A = 'x', 'X'\n    B = 'x', 'Other'\n",
                "unresolved_subclass_hook",
            ),
            (
                "class Options(TextChoices):\n    A = label(), 'X'\n",
                "unresolved_subclass_hook",
            ),
            (
                concat!(
                    "class Product(Model):\n",
                    "    class Options(TextChoices):\n",
                    "        A = 'x', 'X'\n",
                    "    setattr(Options, 'choices', ())\n",
                    "    field = CharField(choices=Options.choices)\n",
                ),
                "unresolved_constructor",
            ),
            (
                concat!(
                    "from django.db.models.base import ModelBase\n",
                    "class Meta(ModelBase): pass\n",
                    "class Product(Model, metaclass=Meta):\n",
                    "    class Options(TextChoices):\n",
                    "        A = 'x', 'X'\n",
                    "    field = CharField(choices=Options.choices)\n",
                ),
                "unresolved_constructor",
            ),
        ];
        for (body, kind) in cases {
            let source =
                format!("from django.db.models import Model, CharField, TextChoices\n{body}");
            let report = analyze_phase03(&[module("models.py", "models", &source)], &[]);
            assert!(
                report.issues.iter().any(|issue| issue.kind == kind),
                "{source}: {:?}",
                report.issues
            );
        }
    }

    #[test]
    fn django_middleware_preserves_only_the_configured_continuation() {
        let middleware = "class Active:\n    def __init__(self, next_handler): self.next = next_handler\n    def __call__(self, request): return self.next(request)\nclass Retired:\n    def __init__(self, next_handler): self.next = next_handler\n    def __call__(self, request): return self.next(request)\n";
        for (setting, selected) in [
            ("MIDDLEWARE = ['shop.middleware.Active']\n", true),
            (
                "MIDDLEWARE = ['shop.middleware.Active']\nMIDDLEWARE = []\n",
                false,
            ),
        ] {
            let settings = format!("INSTALLED_APPS = ['shop']\nSECRET_KEY = 'fixture'\n{setting}");
            let report = analyze_phase03(
                &[
                    module("shop/__init__.py", "shop", ""),
                    module("shop/settings.py", "shop.settings", &settings),
                    module("shop/middleware.py", "shop.middleware", middleware),
                ],
                &[],
            );
            assert!(report.issues.is_empty(), "{setting}: {:?}", report.issues);
            let callbacks: Vec<_> = report
                .evidence
                .iter()
                .filter_map(|row| {
                    (row.kind == "django_middleware").then_some(row.full_name.as_str())
                })
                .collect();
            if selected {
                assert_eq!(
                    callbacks,
                    [
                        "shop.middleware.Active.__init__",
                        "shop.middleware.Active.__call__"
                    ]
                );
            } else {
                assert!(callbacks.is_empty());
            }
        }

        let settings = "INSTALLED_APPS = ['shop']\nSECRET_KEY = 'fixture'\nMIDDLEWARE = ['shop.middleware.Active']\n";
        let middleware =
            format!("{middleware}from external import callback\nActive(callback)(None)\n");
        let report = analyze_phase03(
            &[
                module("shop/__init__.py", "shop", ""),
                module("shop/settings.py", "shop.settings", settings),
                module("shop/middleware.py", "shop.middleware", &middleware),
            ],
            &[],
        );
        assert!(
            report
                .issues
                .iter()
                .any(|issue| issue.kind == "unresolved_call")
        );
    }

    #[test]
    fn django_middleware_preserves_request_metadata_only_after_closed_dispatch() {
        let settings = r#"INSTALLED_APPS = ['shop']
SECRET_KEY = 'fixture'
ROOT_URLCONF = 'shop.urls'
MIDDLEWARE = ['shop.middleware.Active']
STORAGES = {'default': {'BACKEND': 'shop.storage.Media'}}
"#;
        let middleware = r#"def matched_path(request):
    path = getattr(request, 'path_info', None) or getattr(request, 'path', '')
    if not path:
        return None
    return path.rstrip('/') or '/'

class Active:
    def __init__(self, next_handler):
        self.next_handler = next_handler

    def __call__(self, request):
        response = self.next_handler(request)
        if route := matched_path(request):
            request.META = getattr(request, 'META', {})
            request.META['app.route'] = route
        return response
"#;
        for (routes, complete) in [
            ("urlpatterns = []\n", true),
            (
                "urlpatterns = []\nhandler404 = 'shop.handlers.custom_not_found'\n",
                false,
            ),
        ] {
            let report = analyze_phase03(
                &[
                    module("shop/__init__.py", "shop", ""),
                    module("shop/settings.py", "shop.settings", settings),
                    module("shop/urls.py", "shop.urls", routes),
                    module("shop/middleware.py", "shop.middleware", middleware),
                    module(
                        "shop/storage.py",
                        "shop.storage",
                        "class Media:\n    def save(self, name, content): return name\n",
                    ),
                ],
                &[],
            );
            assert_eq!(
                report.issues.is_empty(),
                complete,
                "{routes}: {:?}",
                report.issues
            );
            assert_eq!(
                report
                    .evidence
                    .iter()
                    .any(|row| row.full_name == "shop.middleware.matched_path"
                        && row.kind == "direct_call"),
                complete
            );
            assert!(
                report
                    .evidence
                    .iter()
                    .any(|row| row.full_name == "shop.storage.Media.save"
                        && row.kind == "django_storage_backend")
            );
        }
    }

    #[test]
    fn django_settings_keeps_source_imports_in_finite_initializer_branches() {
        for (initializer, expected) in [
            (
                "if enabled:\n    from .options import SELECTED as AUTHENTICATION_BACKENDS\n",
                vec!["shop.backends.Active.get_user"],
            ),
            (
                "AUTHENTICATION_BACKENDS = ['shop.backends.Retired']\n\
                 if enabled:\n    from .options import SELECTED as AUTHENTICATION_BACKENDS\n",
                vec![
                    "shop.backends.Active.get_user",
                    "shop.backends.Retired.get_user",
                ],
            ),
            (
                "if False:\n    from .options import SELECTED as AUTHENTICATION_BACKENDS\n",
                vec![],
            ),
        ] {
            let settings =
                format!("INSTALLED_APPS = ['shop']\nSECRET_KEY = 'fixture'\n{initializer}");
            let report = analyze_phase03(
                &[
                    module("shop/__init__.py", "shop", ""),
                    module(
                        "shop/options.py",
                        "shop.options",
                        "SELECTED = ['shop.backends.Active']\nTIMEOUT = 60\n",
                    ),
                    module(
                        "shop/backends.py",
                        "shop.backends",
                        "class Active:\n    def get_user(self, pk): return pk\n\
                         class Retired:\n    def get_user(self, pk): return pk\n",
                    ),
                    module("shop/settings.py", "shop.settings", &settings),
                ],
                &[],
            );
            assert!(
                report.issues.is_empty(),
                "{initializer}: {:?}",
                report.issues
            );
            let roots: Vec<_> = report
                .evidence
                .iter()
                .filter(|row| row.kind == "django_auth_backend")
                .map(|row| row.full_name.as_str())
                .collect();
            assert_eq!(roots, expected, "{initializer}");
        }
    }

    #[test]
    fn django_auth_backend_stops_at_proven_invalid_initializer_operations() {
        let header = "INSTALLED_APPS = ['shop']\nSECRET_KEY = 'fixture'\n";
        let selected = "AUTHENTICATION_BACKENDS = ['shop.backends.Configured']\n";
        for statement in [
            "AUTHENTICATION_BACKENDS = []\nAUTHENTICATION_BACKENDS[0]['why'] = 'unused'\n",
            "AUTHENTICATION_BACKENDS = []\nAUTHENTICATION_BACKENDS.missing_method(missing_argument())\n",
            "AUTHENTICATION_BACKENDS = []\nmissing_mutator(AUTHENTICATION_BACKENDS)\n",
            "AUTHENTICATION_BACKENDS = []\ndef configure():\n    AUTHENTICATION_BACKENDS.append('other.Backend')\nconfigure = replacement\nconfigure()\n",
        ] {
            let settings = format!("{header}{statement}{selected}");
            let files = [
                module("shop/__init__.py", "shop", ""),
                module("shop/settings.py", "shop.settings", &settings),
                module(
                    "shop/backends.py",
                    "shop.backends",
                    "class Configured:\n    def get_user(self, pk): return pk\n",
                ),
            ];
            let report = analyze_phase03(&files, &[]);
            assert!(report.issues.is_empty(), "{statement}: {:?}", report.issues);
            assert!(
                !report
                    .evidence
                    .iter()
                    .any(|row| row.kind == "django_auth_backend"),
                "{statement}"
            );
        }

        let settings = format!(
            "{header}{selected}from outside import mutate\nmutate(AUTHENTICATION_BACKENDS)\n"
        );
        let mutator = "from shop.settings import AUTHENTICATION_BACKENDS\nexternal(AUTHENTICATION_BACKENDS)\n";
        let files = [
            module("shop/__init__.py", "shop", ""),
            module("shop/settings.py", "shop.settings", &settings),
            module(
                "outside.py",
                "outside",
                "def mutate(value): external(value)\n",
            ),
            module("consumer.py", "consumer", mutator),
            module(
                "shop/backends.py",
                "shop.backends",
                "class Configured:\n    def get_user(self, pk): return pk\n",
            ),
        ];
        let report = analyze_phase03(&files, &[]);
        assert!(
            report
                .issues
                .iter()
                .any(|issue| issue.kind == "unresolved_call")
        );
    }

    #[test]
    fn django_auth_backend_uses_the_replacement_from_a_closed_decorator() {
        let files = [
            module("shop/__init__.py", "shop", ""),
            module(
                "shop/settings.py",
                "shop.settings",
                "INSTALLED_APPS = ['shop']\nSECRET_KEY = 'fixture'\n\
                AUTHENTICATION_BACKENDS = ['shop.backends.Configured', 'shop.backends.Filtered']\n",
            ),
            module(
                "shop/backends.py",
                "shop.backends",
                r#"class Actual:
    def get_user(self, pk):
        return pk
def redirect(cls):
    return Actual
class Redirect(type):
    def __new__(mcls, name, bases, namespace):
        return Actual
@redirect
class Configured:
    def get_user(self, pk):
        return retired(pk)
def discard(fn):
    return lambda self, pk: None
class Filtered:
    @discard
    def get_user(self, pk):
        return retired(pk)
def retired(pk):
    return pk
"#,
            ),
        ];
        let report = analyze_phase03(&files, &[]);
        assert!(report.issues.is_empty(), "{:?}", report.issues);
        let hooks: Vec<_> = report
            .evidence
            .iter()
            .filter_map(|row| (row.kind == "django_auth_backend").then_some(row.full_name.as_str()))
            .collect();
        assert_eq!(hooks, ["shop.backends.Actual.get_user"]);
        assert!(names(&report, Category::Function).contains(&"retired"));
    }

    #[test]
    fn django_auth_backend_stops_before_proven_unbound_writes() {
        const HEADER: &str = "INSTALLED_APPS = ['shop']\nSECRET_KEY = 'fixture'\n";
        const SETTING: &str = "AUTHENTICATION_BACKENDS = ['shop.backends.Selected']\n";
        const BACKEND: &str = "class Selected:\n    def get_user(self, pk): return pk\n";
        for (provider, settings, other, retains_hook) in [
            ("Selected.get_user = replacement\n", "", "", false),
            (
                "if enabled:\n    Selected.get_user = replacement\n",
                "",
                "",
                false,
            ),
            (
                "def replace():\n    Selected.get_user = replacement\nreplace()\n",
                "",
                "",
                false,
            ),
            (
                "",
                "from shop import backends\nbackends.Selected = replacement\n",
                "",
                false,
            ),
            (
                "",
                "",
                "from shop import backends\nbackends.Selected = replacement\n",
                true,
            ),
        ] {
            let settings_source = format!("{HEADER}{settings}{SETTING}");
            let backend = format!("{BACKEND}{provider}");
            let files = [
                module("shop/__init__.py", "shop", ""),
                module("shop/settings.py", "shop.settings", &settings_source),
                module("shop/backends.py", "shop.backends", &backend),
                module("consumer.py", "consumer", other),
            ];
            let report = analyze_phase03(&files, &[]);
            assert!(
                report.issues.is_empty(),
                "provider {provider:?}, settings {settings:?}, other {other:?}: {:?}",
                report.issues
            );
            assert_eq!(
                report
                    .evidence
                    .iter()
                    .any(|row| row.kind == "django_auth_backend"),
                retains_hook
            );
        }

        let settings = format!("{HEADER}{SETTING}");
        let backend =
            format!("{BACKEND}from external import replacement\nSelected.get_user = replacement\n");
        let report = analyze_phase03(
            &[
                module("shop/__init__.py", "shop", ""),
                module("shop/settings.py", "shop.settings", &settings),
                module("shop/backends.py", "shop.backends", &backend),
            ],
            &[],
        );
        assert!(
            report
                .issues
                .iter()
                .any(|issue| issue.kind == "unresolved_attribute_write")
        );
        let backend = format!("{BACKEND}Selected.get_user = replacement\n");
        let report = analyze_phase03(
            &[
                module("shop/__init__.py", "shop", ""),
                module("shop/settings.py", "shop.settings", &settings),
                module("shop/backends.py", "shop.backends", &backend),
                module(
                    "consumer.py",
                    "consumer",
                    "from shop.backends import Selected\n",
                ),
            ],
            &[],
        );
        assert!(
            report
                .issues
                .iter()
                .any(|issue| { issue.path == "consumer.py" && issue.kind == "unresolved_import" })
        );
    }

    #[test]
    fn django_backend_list_edit_keeps_coverage_incomplete_for_source_importers() {
        let files = [
            module("shop/__init__.py", "shop", ""),
            module(
                "shop/settings.py",
                "shop.settings",
                "INSTALLED_APPS = ['shop']\nSECRET_KEY = 'fixture'\nAUTHENTICATION_BACKENDS = []\nAUTHENTICATION_BACKENDS += ['shop.backends.Selected']\n",
            ),
            module(
                "shop/backends.py",
                "shop.backends",
                "class Selected:\n    def get_user(self, key): return key\n",
            ),
            module(
                "consumer.py",
                "consumer",
                "from shop.settings import AUTHENTICATION_BACKENDS\nif AUTHENTICATION_BACKENDS:\n    selected()\ndef selected(): pass\n",
            ),
        ];
        let report = analyze_phase03(&files, &[]);
        assert!(
            report
                .issues
                .iter()
                .any(|issue| issue.kind == "unresolved_setting_mutation")
        );
        assert!(
            !report
                .evidence
                .iter()
                .any(|row| row.kind == "django_auth_backend")
        );
    }

    #[test]
    fn django_settings_paths_require_an_original_file_anchor_and_path_provider() {
        let source = "from pathlib import Path as FilePath\n\
                      BASE = FilePath(__file__).resolve().parent\n\
                      TEMPLATES = [{'BACKEND': 'django.template.backends.django.DjangoTemplates', \
                                    'DIRS': [BASE / 'pages']}]\n";
        let settings = module(
            "shop/settings.py",
            "shop.settings",
            "from .satellite import TEMPLATES\n",
        );
        let package = module("shop/__init__.py", "shop", "");
        let sat = module("shop/satellite.py", "shop.satellite", source);
        let report = analyze_phase03(&[package, settings, sat], &[]);
        assert!(report.issues.is_empty(), "{:?}", report.issues);

        for replacement in [
            "__file__ = '/different/site.py'\n",
            "import pathlib\npathlib.Path.resolve = implementation\n",
        ] {
            let source = format!("{replacement}{source}");
            let sat = module("shop/satellite.py", "shop.satellite", &source);
            let report = analyze_phase03(&[sat], &[]);
            assert!(
                report
                    .issues
                    .iter()
                    .any(|issue| issue.kind == "unresolved_call")
            );
        }
    }

    #[test]
    fn dict_config_follows_reached_instance_settings_and_ordered_guard_paths() {
        let prefix = r#"import logging.config
import os
from os import PathLike as Path
CONFIG = {'version': 1, 'formatters': {'main': {'()': 'formatters.Selected'}}}
class Server:
    def __init__(self, config=CONFIG):
        self.config = config
    def apply(self):
        if isinstance(self.config, Path):
            self.config.clear()
        if isinstance(self.config, dict):
            logging.config.dictConfig(self.config)
server = Server()
"#;
        for (change, selected) in [
            ("", true),
            ("alias = server\nalias.config = {}\n", false),
            ("os.PathLike.register(dict)\n", false),
            ("path_type = os.PathLike\npath_type.register(dict)\n", false),
            ("register = os.PathLike.register\nregister(dict)\n", false),
        ] {
            let source = format!("{prefix}{change}server.apply()\n");
            let result = analyze_phase03(
                &[
                    module("app.py", "app", &source),
                    module("formatters.py", "formatters", "class Selected: pass\n"),
                ],
                &[],
            );
            assert!(result.issues.is_empty(), "{change}: {:?}", result.issues);
            assert_eq!(
                result.evidence.iter().any(|edge| {
                    edge.kind == "logging_factory" && edge.full_name == "formatters.Selected"
                }),
                selected,
                "{change}",
            );
        }
    }

    #[test]
    fn dict_config_instance_replay_keeps_coverage_for_untracked_external_dispatch() {
        let source = r#"import logging.config
CONFIG = {'version': 1, 'formatters': {'main': {'()': 'formatters.Selected'}}}
class Server:
    def __init__(self, config=CONFIG):
        self.config = config
    def apply(self):
        logging.config.dictConfig(self.config)
server = Server()
server.apply()
"#;
        let result = analyze_phase03(
            &[
                module("app.py", "app", source),
                module(
                    "consumer.py",
                    "consumer",
                    "from app import server\nserver.config = replacement\nserver.apply()\n",
                ),
                module("formatters.py", "formatters", "class Selected: pass\n"),
            ],
            &[],
        );
        assert!(!result.issues.is_empty());
        assert!(
            !result
                .evidence
                .iter()
                .any(|edge| edge.kind == "logging_factory")
        );
    }

    #[test]
    fn dict_config_stops_at_proven_unbound_initializer_operands() {
        const PREFIX: &str = "import logging.config\nimport os\n\
            CONFIG = {'version': 1, 'formatters': {'main': {'()': 'formatters.Selected'}}}\n";
        for failure in [
            "logging.config.dictConfig = replacement",
            "alias = logging.config\nsetattr(alias, 'dictConfig', replacement)",
            "unknown(CONFIG)",
            "def apply(logging): logging.config.dictConfig(CONFIG)\napply(replacement)",
            "alias = os\nalias.PathLike = replacement",
            "os.PathLike.__subclasshook__ = replacement",
            "setattr(os.PathLike, '__subclasshook__', replacement)",
            "unknown(os.PathLike)",
        ] {
            let source = format!("{PREFIX}{failure}\nlogging.config.dictConfig(CONFIG)\n");
            let result = analyze_phase03(
                &[
                    module("app.py", "app", &source),
                    module("formatters.py", "formatters", "class Selected: pass\n"),
                ],
                &[],
            );
            assert!(result.issues.is_empty(), "{failure}: {:?}", result.issues);
            assert_eq!(names(&result, Category::Class), ["Selected"], "{failure}");
            assert!(
                !result
                    .evidence
                    .iter()
                    .any(|edge| edge.kind == "logging_factory"),
                "{failure}"
            );
        }
        let source = format!(
            "{PREFIX}import external\nexternal.replace_logging()\nunknown(CONFIG)\nlogging.config.dictConfig(CONFIG)\n"
        );
        let result = analyze_phase03(&[module("app.py", "app", &source)], &[]);
        assert!(
            result
                .issues
                .iter()
                .any(|issue| issue.kind == "unresolved_call" && issue.line == Some(6)),
            "{:?}",
            result.issues
        );
        let source = format!("{PREFIX}unknown(CONFIG)\nlogging.config.dictConfig(CONFIG)\n");
        let result = analyze_phase03(
            &[
                module("app.py", "app", &source),
                module("consumer.py", "consumer", "from app import CONFIG\n"),
            ],
            &[],
        );
        assert!(
            result
                .issues
                .iter()
                .any(|issue| { issue.kind == "unresolved_import" && issue.path == "consumer.py" })
        );
    }

    #[test]
    fn dict_config_resolves_factory_at_the_reached_stdlib_consumer() {
        let source = "from logging.config import dictConfig as apply\n\
                      apply({'version': 1, 'formatters': {'main': {'()': 'formatters.Selected'}}})\n";
        let candidate = module("app.py", "app", source);
        let factories = module(
            "formatters.py",
            "formatters",
            "class Selected:\n    def format(self, rec): return render(rec)\n\
             def render(rec): return str(rec)\n\
             class Retired: pass\n",
        );
        let result = analyze_phase03(&[candidate, factories], &[]);
        assert!(result.issues.is_empty(), "{:?}", result.issues);
        assert_eq!(names(&result, Category::Class), ["Retired"]);
        assert!(result.evidence.iter().any(|entry| {
            entry.full_name == "formatters.Selected"
                && entry.kind == "logging_factory"
                && entry.certainty == "possible"
        }));
        assert_eq!(names(&result, Category::Function), ["format", "render"]);
    }

    #[test]
    fn dict_config_stops_before_later_factories_after_proven_binding_failure() {
        let source = "import logging.config as config\n\
                      apply = config.dictConfig\n\
                      config.dictConfig = lambda configuration: None\n\
                      apply({'version': 1, 'formatters': {'z': {'class': 'formatters.Invalid'}, 'a': {'()': 'formatters.Later'}}})\n";
        let result = analyze_phase03(
            &[
                module("app.py", "app", source),
                module(
                    "formatters.py",
                    "formatters",
                    "class Invalid: pass\nclass Later: pass\n",
                ),
            ],
            &[],
        );
        assert!(result.issues.is_empty(), "{:?}", result.issues);
        assert_eq!(names(&result, Category::Class), ["Later"]);
    }

    #[test]
    fn dict_config_observes_projected_alias_mutation_and_local_clears() {
        for (edit, selected) in [
            (
                "wrapped = {'selected': CONFIG['formatters']['main']}\nwrapped['selected']['()'] = 'formatters.Other'\n",
                Some("Other"),
            ),
            (
                "def mutate(config):\n    config.clear()\nmutate(CONFIG)\n",
                None,
            ),
            ("alias = CONFIG\nalias.clear()\n", None),
        ] {
            let source = format!(
                "import logging.config\nCONFIG = {{'version': 1, 'formatters': {{'main': {{'()': 'formatters.Original'}}}}}}\n\
                {edit}logging.config.dictConfig(CONFIG)\n"
            );
            let result = analyze_phase03(
                &[
                    module("app.py", "app", &source),
                    module(
                        "formatters.py",
                        "formatters",
                        "class Original: pass\nclass Other: pass\n",
                    ),
                ],
                &[],
            );
            assert!(result.issues.is_empty(), "{edit}: {:?}", result.issues);
            let expected = if selected.is_some() {
                vec!["Original"]
            } else {
                vec!["Original", "Other"]
            };
            assert_eq!(names(&result, Category::Class), expected);
        }
    }

    #[test]
    fn dict_config_with_opaque_mutation_keeps_coverage_incomplete() {
        let source = "import logging.config\n\
                      from external_mutator import configure\n\
                      CONFIG = {'version': 1, 'formatters': {'main': {'()': 'formatters.Selected'}}}\n\
                      configure(CONFIG)\n\
                      logging.config.dictConfig(CONFIG)\n";
        let result = analyze_phase03(
            &[
                module("app.py", "app", source),
                module("formatters.py", "formatters", "class Selected: pass\n"),
            ],
            &[],
        );
        assert!(
            result
                .issues
                .iter()
                .any(|issue| issue.kind == "unresolved_call")
        );
        assert!(
            !result
                .evidence
                .iter()
                .any(|entry| entry.kind == "logging_factory")
        );
    }

    #[test]
    fn dict_config_does_not_read_a_shadowed_package_member_as_a_submodule() {
        let source = "import logging.config\n\
                      logging.config.dictConfig({'version': 1, 'formatters': {'main': {'()': 'pkg.formatters.Selected'}}})\n";
        let result = analyze_phase03(
            &[
                module("app.py", "app", source),
                module("pkg/__init__.py", "pkg", "formatters = object()\n"),
                module(
                    "pkg/formatters.py",
                    "pkg.formatters",
                    "class Selected: pass\n",
                ),
            ],
            &[],
        );
        assert!(
            result
                .issues
                .iter()
                .any(|issue| issue.kind == "unresolved_registration")
        );
        assert!(
            !result
                .evidence
                .iter()
                .any(|entry| entry.kind == "logging_factory")
        );
    }

    #[test]
    fn celery_routes_use_the_effective_configuration_and_bind_only_router_slots() {
        let files = [
            module(
                "app.py",
                "app",
                concat!(
                    "from celery import Celery as Factory\n",
                    "from routers import active as route\n",
                    "app = Factory('shop', task_routes=('routers:stale',))\n",
                    "alias = app.conf\n",
                    "alias.update({'task_routes': (route, {'routers.mapping_key': {'queue': 'jobs'}})})\n",
                    "class Other:\n    app = object()\n",
                ),
            ),
            module(
                "routers.py",
                "routers",
                concat!(
                    "def stale(name, args, kwargs, options, task=None): pass\n",
                    "def mapping_key(name, args, kwargs, options, task=None): pass\n",
                    "def active(name, args, /, *rest, task, extra=None, **kw):\n    helper()\n",
                    "def helper(): pass\n",
                ),
            ),
        ];
        let result = analyze_phase03(&files, &[]);
        assert!(result.issues.is_empty(), "{:?}", result.issues);
        assert_eq!(names(&result, Category::Function), ["mapping_key", "stale"]);
        assert!(
            result
                .findings
                .iter()
                .any(|finding| finding.full_name == "routers.active.extra")
        );
        let injected: BTreeSet<_> = result
            .evidence
            .iter()
            .filter_map(|edge| {
                (edge.kind == celery_routes::ROUTER_PARAMETER).then_some(edge.full_name.as_str())
            })
            .collect();
        assert_eq!(
            injected,
            BTreeSet::from([
                "routers.active.args",
                "routers.active.kw",
                "routers.active.name",
                "routers.active.rest",
                "routers.active.task"
            ])
        );
        assert!(
            result
                .evidence
                .iter()
                .any(|edge| edge.full_name == "routers.helper" && edge.certainty == "possible")
        );
        assert!(
            !result
                .evidence
                .iter()
                .any(|edge| edge.full_name == "routers.helper" && edge.certainty == "certain")
        );
    }

    #[test]
    fn celery_function_router_requires_accepting_the_task_keyword() {
        for (keyword, callable) in [("", false), (", *, task=None", true)] {
            let result = analyze_phase03(
                &[
                    module(
                        "app.py",
                        "app",
                        concat!(
                            "from celery import Celery\n",
                            "app = Celery('shop')\n",
                            "app.conf.task_routes = ('routers.route_task',)\n",
                            "class Other:\n    app = object()\n",
                        ),
                    ),
                    module(
                        "routers.py",
                        "routers",
                        &format!(
                            "def route_task(name, args, kwargs, options{keyword}):\n    helper()\ndef helper(): pass\n"
                        ),
                    ),
                ],
                &[],
            );
            assert!(result.issues.is_empty(), "{:?}", result.issues);
            for (name, kind) in [
                ("routers.route_task", celery_routes::ROUTER),
                ("routers.route_task.args", celery_routes::ROUTER_PARAMETER),
                ("routers.helper", "direct_call"),
            ] {
                assert_eq!(
                    result
                        .evidence
                        .iter()
                        .any(|edge| edge.full_name == name && edge.kind == kind),
                    callable,
                    "{name}: {:?}",
                    result.evidence
                );
            }
            assert_eq!(
                names(&result, Category::Function).contains(&"helper"),
                !callable
            );
        }
    }

    #[test]
    fn celery_terminal_router_failures_preserve_only_reached_evaluation() {
        for failure in [
            "app.conf.update(missing)",
            "app = object()\napp.conf.task_routes = ('routers.route',)",
            "app.conf = object()\napp.conf.task_routes = ('routers.route',)",
        ] {
            let source = format!(
                "from celery import Celery\napp = Celery('shop', task_routes=('routers.route',))\n{failure}\n"
            );
            let files = [
                module("app.py", "app", &source),
                module(
                    "routers.py",
                    "routers",
                    "def route(*args, **kwargs): pass\n",
                ),
            ];
            let result = analyze_phase03(&files, &[]);
            assert!(result.issues.is_empty(), "{failure}: {:?}", result.issues);
            assert_eq!(names(&result, Category::Function), ["route"], "{failure}");
        }
        let files = [
            module(
                "app.py",
                "app",
                "from celery import Celery\napp = Celery('shop', task_routes=('routers.route',))\nfrom cfg import cfg\napp.conf.update(cfg)\n",
            ),
            module("cfg.py", "cfg", "cfg = object()\n"),
            module(
                "routers.py",
                "routers",
                "def route(*args, **kwargs): pass\n",
            ),
        ];
        let result = analyze_phase03(&files, &[]);
        assert!(
            result
                .issues
                .iter()
                .any(|issue| issue.kind == "unresolved_registration")
        );
        assert!(
            result
                .evidence
                .iter()
                .any(|edge| edge.full_name == "routers.route" && edge.certainty == "possible")
        );
    }

    #[test]
    fn celery_configuration_subscript_keys_follow_evaluation_order() {
        let prefix = "from celery import Celery\napp = Celery('demo')\napp.conf.task_routes = ('routers.old',)\n";
        for (tail, roots, complete) in [
            ("app.conf[unset] = replacement\n", vec![], true),
            (
                "key = 'task_routes'\napp.conf[key] = ('routers.new',)\n",
                vec!["routers.new"],
                true,
            ),
            (
                "from provider import route_key\napp.conf[route_key] = ('routers.new',)\n",
                vec!["routers.old", "routers.new"],
                false,
            ),
        ] {
            let source = format!("{prefix}{tail}");
            let report = analyze_phase03(
                &[
                    module("app.py", "app", &source),
                    module(
                        "routers.py",
                        "routers",
                        "def old(*args, **kwargs): pass\ndef new(*args, **kwargs): pass\n",
                    ),
                ],
                &[],
            );
            assert_eq!(
                report.issues.is_empty(),
                complete,
                "{tail}: {:?}",
                report.issues
            );
            let routed: Vec<_> = report
                .evidence
                .iter()
                .filter_map(|row| {
                    (row.kind == celery_routes::ROUTER).then_some(row.full_name.as_str())
                })
                .collect();
            assert_eq!(routed, roots, "{tail}");
        }
    }

    #[test]
    fn celery_early_submission_resolves_module_names_before_later_rebinding() {
        let files = [module(
            "app.py",
            "app",
            concat!(
                "from celery import Celery\n",
                "def original(name, args, kwargs, options, task=None): pass\n",
                "def late(name, args, kwargs, options, task=None): pass\n",
                "selected = original\n",
                "app = Celery('shop', task_routes=('app:selected',))\n",
                "app.send_task('jobs.run')\n",
                "selected = late\n",
                "app.conf.task_routes = ()\n",
            ),
        )];
        let result = analyze_phase03(&files, &[]);
        let routers: Vec<_> = result
            .evidence
            .iter()
            .filter_map(|edge| {
                (edge.kind == celery_routes::ROUTER).then_some(edge.full_name.as_str())
            })
            .collect();
        assert_eq!(routers, ["app.original"]);
    }

    #[test]
    fn celery_routes_respect_provider_rebinding_without_retiring_an_earlier_capture() {
        let files = [
            module(
                "app.py",
                "app",
                concat!(
                    "from celery import Celery as Original\n",
                    "import celery\n",
                    "def replacement(task_routes=None): return object()\n",
                    "celery.Celery = replacement\n",
                    "from celery import Celery as Recent\n",
                    "app = Original(task_routes=('routers.active',))\n",
                    "lookalike = Recent(task_routes=('routers.fake',))\n",
                ),
            ),
            module(
                "routers.py",
                "routers",
                "def active(*args, **kwargs): pass\ndef fake(*args, **kwargs): pass\n",
            ),
        ];
        let result = analyze_phase03(&files, &[]);
        assert!(result.issues.is_empty(), "{:?}", result.issues);
        assert_eq!(names(&result, Category::Function), ["fake"]);
    }

    #[test]
    fn django_runpython_preserves_only_supplied_callback_parameters() {
        let files = [
            module(
                "callbacks.py",
                "callbacks",
                "def apply_change(apps, editor, /, extra=None): pass\n",
            ),
            module(
                "shop/migrations/0001_data.py",
                "shop.migrations.0001_data",
                r#"from django.db import migrations
from callbacks import apply_change as forwards

def backwards(first, /, *rest, extra=None): pass
def unused(untouched): pass

class Migration(migrations.Migration):
    operations = [migrations.RunPython(forwards, reverse_code=backwards)]
"#,
            ),
        ];
        let report = analyze_phase03(&files, &[]);
        assert!(report.issues.is_empty(), "{:?}", report.issues);
        assert_eq!(
            names(&report, Category::Parameter),
            ["extra", "extra", "untouched"]
        );
        let retained: BTreeSet<_> = report
            .evidence
            .iter()
            .filter(|row| row.kind == "django_migration_parameter" && row.certainty == "possible")
            .map(|row| row.full_name.as_str())
            .collect();
        assert_eq!(
            retained,
            BTreeSet::from([
                "callbacks.apply_change.apps",
                "callbacks.apply_change.editor",
                "shop.migrations.0001_data.backwards.first",
                "shop.migrations.0001_data.backwards.rest",
            ])
        );
    }

    #[test]
    fn django_runpython_accepts_an_explicit_absent_reverse_callback() {
        for reverse in ["None", "absent"] {
            let source = format!(
                "from django.db.migrations import RunPython\n\
                 def forward(apps, editor, extra=None): pass\n\
                 absent = None\n\
                 operation = RunPython(forward, reverse_code={reverse})\n"
            );
            let files = [module("migration.py", "migration", &source)];
            let report = analyze_phase03(&files, &[]);
            assert!(report.issues.is_empty(), "{:?}", report.issues);
            assert_eq!(names(&report, Category::Parameter), ["extra"]);
        }
        let source = "from django.db.migrations import RunPython\n\
                      def forward(apps, editor): pass\n\
                      absent = 0\n\
                      operation = RunPython(forward, reverse_code=absent)\n";
        let files = [module("migration.py", "migration", source)];
        let report = analyze_phase03(&files, &[]);
        assert!(
            report
                .issues
                .iter()
                .any(|issue| issue.kind == "unresolved_registration")
        );
    }

    #[test]
    fn django_runpython_rejects_proven_inert_source_lookalikes_at_startup() {
        for statements in [
            "class RunPython: pass\noperation = RunPython(forwards)\n",
            "from django.db import migrations\nclass Holder:\n    migrations = object()\n    operation = migrations.RunPython(forwards)\n",
            "from django.db import migrations\nforwards = replacement\noperation = migrations.RunPython(forwards)\n",
        ] {
            let source = format!("def forwards(apps, editor): pass\n{statements}");
            let report = analyze_phase03(&[module("migration.py", "migration", &source)], &[]);
            assert!(
                report.issues.is_empty(),
                "{statements}: {:?}",
                report.issues
            );
            assert_eq!(names(&report, Category::Parameter), ["apps", "editor"]);
            assert!(
                !report
                    .evidence
                    .iter()
                    .any(|row| row.kind == "django_migration_parameter")
            );
        }

        let source = "from django.db import migrations\n\
                      from provider import operations as migrations\n\
                      def forwards(apps, editor): pass\n\
                      operation = migrations.RunPython(forwards)\n";
        let report = analyze_phase03(&[module("migration.py", "migration", source)], &[]);
        assert!(
            report
                .issues
                .iter()
                .any(|issue| issue.kind == "unresolved_call")
        );
    }

    #[test]
    fn django_runpython_rebinding_preserves_only_the_original_capture() {
        for (replacement, complete) in [("replace", true), ("install()", false)] {
            let source = format!(
                "from django.db import migrations\n\
                 def replace(callback): return None\n\
                 def retained(apps, editor): pass\n\
                 def retired(apps, editor): pass\n\
                 captured = migrations.RunPython\n\
                 migrations.RunPython = {replacement}\n\
                 old = captured(retained)\n\
                 new = migrations.RunPython(retired)\n"
            );
            let report = analyze_phase03(&[module("migration.py", "migration", &source)], &[]);
            let retained: Vec<_> = report
                .evidence
                .iter()
                .filter(|row| row.kind == "django_migration_callback_declaration")
                .map(|row| row.full_name.as_str())
                .collect();
            assert_eq!(retained, ["migration.retained"], "{replacement}");
            assert_eq!(report.issues.is_empty(), complete, "{:?}", report.issues);
            if !complete {
                assert!(
                    report
                        .issues
                        .iter()
                        .any(|issue| issue.kind == "unresolved_attribute_write")
                );
            }
        }
    }

    #[test]
    fn reports_required_categories_and_retains_rooted_definitions() {
        let files = [
            module(
                "main.py",
                "main",
                "import os\nfrom helper import work as run\n\nclass Unused:\n    pass\nclass Used:\n    pass\n\ndef _entry(unread):\n    quiet = 1\n    value = 2\n    print(value)\n    run()\n    Used()\n\ndef dormant():\n    dormant()\n\n_entry(0)\n",
            ),
            module("helper.py", "helper", "def work():\n    return 3\n"),
            module("empty.py", "empty", "# deliberately empty\n"),
        ];
        let result = analyze(&files, &[]);
        assert!(result.issues.is_empty(), "{:?}", result.issues);
        assert_eq!(names(&result, Category::Function), ["dormant"]);
        assert_eq!(names(&result, Category::Class), ["Unused"]);
        assert_eq!(names(&result, Category::Import), ["os"]);
        assert_eq!(names(&result, Category::Variable), ["quiet"]);
        assert_eq!(names(&result, Category::Parameter), ["unread"]);
        assert_eq!(names(&result, Category::File), ["empty.py"]);
    }

    #[test]
    fn local_namespace_observes_possible_bindings_at_the_call() {
        let files = [module(
            "main.py",
            "main",
            r#"import os

def _entry(flag):
    visible = 0
    visible = 1
    if flag:
        visible = 2
    ignored: int
    def callback():
        return 0
    result = locals()
    later = 1
    return result

_entry(True)
"#,
        )];
        let result = analyze(&files, &[]);
        assert!(result.issues.is_empty(), "{:?}", result.issues);
        assert_eq!(
            names(&result, Category::Variable),
            ["ignored", "later", "visible"]
        );
        assert!(result.findings.iter().any(|finding| {
            finding.category == Category::Variable && finding.name == "visible" && finding.line == 4
        }));
        assert_eq!(names(&result, Category::Import), ["os"]);
        assert!(result.evidence.iter().any(|evidence| {
            evidence.full_name == "main._entry.callback"
                && evidence.kind == "local_namespace"
                && evidence.certainty == "possible"
        }));
    }

    #[test]
    fn escaping_local_namespace_retains_instance_calls_and_constructors() {
        let files = [module(
            "pkg/__init__.py",
            "pkg",
            r#"__all__ = ["build", "register"]

def handle():
    return 42

class Handler:
    def __call__(self):
        return handle()

def build():
    snapshot = locals()
    action = Handler()
    vars()
    return snapshot

def initialize():
    return 1

def register():
    class Plugin:
        def __init__(self):
            initialize()
    return dict(vars(), registered=True)

def unused():
    return 0
"#,
        )];
        let result = analyze(&files, &[]);
        assert!(result.issues.is_empty(), "{:?}", result.issues);
        assert_eq!(names(&result, Category::Function), ["unused"]);
        for name in ["pkg.Handler.__call__", "pkg.register.Plugin.__init__"] {
            assert!(
                result
                    .evidence
                    .iter()
                    .any(|entry| { entry.full_name == name && entry.certainty == "possible" }),
                "{name}: {:?}",
                result.evidence
            );
        }
    }

    #[test]
    fn escaping_composite_namespace_reports_unresolved_values() {
        for expression in [
            "debug is True and locals() or {}",
            "vars() | {'version': 1}",
            "dict(vars() | {'version': 1})",
            "dict({'snapshot': vars() | {'version': 1}}, status='ok')",
        ] {
            let source = format!(
                r#"__all__ = ["build"]

def build(debug):
    def action():
        return 42
    namespace = {expression}
    return {{"snapshot": namespace}}
"#
            );
            let result = analyze(&[module("pkg/__init__.py", "pkg", &source)], &[]);
            assert!(
                result
                    .issues
                    .iter()
                    .any(|issue| issue.kind == "unresolved_export" && issue.line == Some(7)),
                "{expression}: {:?}",
                result.issues
            );
        }
    }

    #[test]
    fn dictionary_keywords_preserve_nested_namespace_entries() {
        let files = [module(
            "pkg/__init__.py",
            "pkg",
            r#"__all__ = ["build"]

def handle():
    return 42

def build():
    def action():
        return handle()
    return dict({"namespace": locals()}, status="ok")
"#,
        )];
        let result = analyze(&files, &[]);
        assert!(result.issues.is_empty(), "{:?}", result.issues);
        assert!(
            names(&result, Category::Function).is_empty(),
            "{:?}",
            result.findings
        );
        assert!(result.evidence.iter().any(|entry| {
            entry.full_name == "pkg.build.action" && entry.kind == "local_namespace"
        }));
    }

    #[test]
    fn namespace_dictionary_copy_respects_overrides_and_refreshes() {
        let files = [module(
            "pkg/__init__.py",
            "pkg",
            r#"from builtins import locals as collect

__all__ = ["build", "snapshot"]

def build():
    def callback(consumer):
        return consumer()
    def retained():
        return 42
    return dict(collect(), callback=None)

def snapshot():
    copied = dict(collect())
    def after_copy():
        return 42
    collect()
    return copied
"#,
        )];
        let result = analyze(&files, &[]);
        assert!(result.issues.is_empty(), "{:?}", result.issues);
        assert_eq!(
            names(&result, Category::Function),
            ["after_copy", "callback"]
        );
    }

    #[test]
    fn namespace_copy_includes_refreshes_during_argument_evaluation() {
        let files = [module(
            "pkg/__init__.py",
            "pkg",
            r#"__all__ = ["build", "snapshot"]

def handle():
    return 42

def build():
    saved = locals()
    def action():
        return handle()
    return dict(saved, size=len(locals()))

def snapshot():
    saved = locals()
    def too_late():
        return 42
    return dict(saved), len(locals())
"#,
        )];
        let result = analyze(&files, &[]);
        assert!(result.issues.is_empty(), "{:?}", result.issues);
        assert_eq!(names(&result, Category::Function), ["too_late"]);
        assert!(result.evidence.iter().any(|entry| {
            entry.full_name == "pkg.build.action" && entry.kind == "local_namespace"
        }));
    }

    #[test]
    fn namespace_truth_test_does_not_export_a_proven_nonempty_namespace() {
        let files = [module(
            "pkg/__init__.py",
            "pkg",
            r#"from builtins import locals as collect

__all__ = ["flag"]

def flag(enabled):
    def unused(consumer):
        return consumer()
    return collect() and True
"#,
        )];
        let result = analyze(&files, &[]);
        assert!(result.issues.is_empty(), "{:?}", result.issues);
        assert_eq!(names(&result, Category::Function), ["unused"]);
    }

    #[test]
    fn escaping_namespace_retains_executor_arguments() {
        let files = [module(
            "pkg/__init__.py",
            "pkg",
            r#"from concurrent.futures import ThreadPoolExecutor

__all__ = ["build"]

def handle():
    return 42

class Action:
    def __call__(self):
        return handle()

def worker(action):
    return locals()

def build():
    with ThreadPoolExecutor() as executor:
        return executor.submit(worker, action=Action())

class Dormant:
    def __call__(self, consumer):
        return consumer()

def unused():
    with ThreadPoolExecutor() as executor:
        return executor.submit(worker, Dormant())
"#,
        )];
        let result = analyze(&files, &[]);
        assert!(result.issues.is_empty(), "{:?}", result.issues);
        assert_eq!(names(&result, Category::Function), ["unused"]);
        assert_eq!(names(&result, Category::Class), ["Dormant"]);
        assert!(result.evidence.iter().any(|entry| {
            entry.full_name == "pkg.Action.__call__"
                && entry.kind == "local_namespace"
                && entry.certainty == "possible"
        }));
    }

    #[test]
    fn escaping_local_namespace_retains_project_arguments() {
        let files = [module(
            "pkg/__init__.py",
            "pkg",
            r#"__all__ = ["build"]

def live():
    return 42

class Action:
    def __call__(self):
        return live()

def _namespace(action):
    return locals()

def build():
    return _namespace(action=Action())

class Dormant:
    def __call__(self, callback):
        return callback()

def unused():
    return _namespace(Dormant())
"#,
        )];
        let result = analyze(&files, &[]);
        assert!(result.issues.is_empty(), "{:?}", result.issues);
        assert_eq!(names(&result, Category::Function), ["unused"]);
        assert_eq!(names(&result, Category::Class), ["Dormant"]);
        assert!(result.evidence.iter().any(|entry| {
            entry.full_name == "pkg.Action.__call__"
                && entry.kind == "local_namespace"
                && entry.certainty == "possible"
        }));
    }

    #[test]
    fn escaping_local_namespace_retains_coroutine_targets() {
        let files = [module(
            "pkg/__init__.py",
            "pkg",
            r#"__all__ = ["build"]

def handle():
    return 42

async def process():
    return handle()

def build():
    task = process()
    return locals()

async def unused():
    return 0
"#,
        )];
        let result = analyze(&files, &[]);
        assert!(result.issues.is_empty(), "{:?}", result.issues);
        assert_eq!(names(&result, Category::Function), ["unused"]);
        assert!(result.evidence.iter().any(|entry| {
            entry.full_name == "pkg.process"
                && entry.kind == "local_namespace"
                && entry.certainty == "possible"
        }));
    }

    #[test]
    fn scalar_local_namespace_equality_rules_out_unrelated_hooks() {
        for (expression, expected_issues) in [
            ("collect() == {'enabled': True}", 0),
            ("collect() == {'enabled': Entry()}", 1),
        ] {
            let source = format!(
                r#"from builtins import locals as collect

__all__ = ["check"]

class Entry:
    def __eq__(self, other):
        return False

def check():
    enabled = True
    return {expression}
"#
            );
            let result = analyze(&[module("pkg/__init__.py", "pkg", &source)], &[]);
            assert_eq!(
                result.issues.len(),
                expected_issues,
                "{expression}: {:?}",
                result.issues
            );
            assert!(
                result
                    .issues
                    .iter()
                    .all(|issue| issue.kind == "unresolved_comparison")
            );
        }
    }

    #[test]
    fn escaping_local_namespace_reports_opaque_local_values() {
        let files = [module(
            "pkg/__init__.py",
            "pkg",
            r#"__all__ = ["build"]

class Handler:
    def run(self):
        return 42

def make():
    return Handler()

def build():
    action = make()
    return locals()
"#,
        )];
        let result = analyze(&files, &[]);
        assert!(
            result
                .issues
                .iter()
                .any(|issue| { issue.kind == "unresolved_export" && issue.line == Some(12) }),
            "{:?}",
            result.issues
        );
    }

    #[test]
    fn counting_local_namespace_entries_does_not_expose_callables() {
        let files = [module(
            "pkg/__init__.py",
            "pkg",
            r#"from builtins import locals as collect

__all__ = ["count"]

class Sized:
    def __len__(self):
        return 1

def count(preview):
    if preview is True:
        def unused(handler):
            return handler()
        namespace = collect()
        return len(namespace)
    return vars()
"#,
        )];
        let result = analyze(&files, &[]);
        assert!(result.issues.is_empty(), "{:?}", result.issues);
        assert_eq!(names(&result, Category::Function), ["unused"]);
    }

    #[test]
    fn local_namespace_excludes_assignments_from_exited_branches() {
        let files = [module(
            "pkg/__init__.py",
            "pkg",
            r#"__all__ = ["build"]

def build(enabled):
    if enabled:
        unused = 1
        return {}
    else:
        carried = 2
    return locals()
"#,
        )];
        let result = analyze(&files, &[]);
        assert!(result.issues.is_empty(), "{:?}", result.issues);
        assert_eq!(names(&result, Category::Variable), ["unused"]);
    }

    #[test]
    fn local_namespace_retains_compiler_captures_including_skipped_reads() {
        let files = [module(
            "pkg/__init__.py",
            "pkg",
            r#"__all__ = ["build"]
DEBUG = False

def build():
    context = object()
    captured_through_child = object()
    unrelated = object()
    def collect():
        if DEBUG:
            print(context)
        def child():
            if DEBUG:
                print(captured_through_child)
        return vars()
    return collect()
"#,
        )];
        let result = analyze(&files, &[]);
        assert!(result.issues.is_empty(), "{:?}", result.issues);
        assert_eq!(names(&result, Category::Variable), ["unrelated"]);
    }

    #[test]
    fn local_namespace_requires_a_proven_zero_argument_builtin() {
        let files = [module(
            "main.py",
            "main",
            r#"from builtins import vars as collect

def _entry():
    def locals():
        return 0
    unread = 1
    return locals()

def _opaque(value, source):
    candidate = 1
    collect(value)
    return eval(source)

def _unbound():
    if False:
        vars = object
    return vars()

_entry()
_opaque(object(), "pass")
_unbound()
"#,
        )];
        let result = analyze(&files, &[]);
        assert_eq!(names(&result, Category::Variable), ["candidate", "unread"]);
        assert_eq!(
            result
                .issues
                .iter()
                .map(|issue| (issue.kind, issue.line))
                .collect::<BTreeSet<_>>(),
            [
                ("unresolved_call", Some(11)),
                ("unresolved_call", Some(12)),
                ("unresolved_call", Some(17))
            ]
            .into()
        );
    }

    #[test]
    fn phase03_opaque_evaluation_can_read_locals_in_dormant_functions() {
        for (import, expression) in [
            ("", "eval(source)"),
            (
                "from builtins import eval as evaluate_text",
                "evaluate_text(source)",
            ),
            ("import builtins as tools", "tools.exec(source)"),
        ] {
            let body = format!(
                "{import}

def evaluate(source):
    retained_value = object()
    return {expression}

def ordinary():
    forgotten_value = object()
"
            );
            let dormant = analyze_phase03(&[module("pkg/__init__.py", "pkg", &body)], &[]);
            assert!(dormant.issues.is_empty(), "{:?}", dormant.issues);
            assert_eq!(names(&dormant, Category::Variable), ["forgotten_value"]);
            assert!(names(&dormant, Category::Function).contains(&"evaluate"));

            let reached = format!(
                "{body}
__all__ = ['evaluate']
"
            );
            let active = analyze_phase03(&[module("pkg/__init__.py", "pkg", &reached)], &[]);
            assert!(
                active
                    .issues
                    .iter()
                    .any(|issue| issue.kind == "unresolved_call"
                        && issue
                            .message
                            .contains("can execute code outside the bounded model")),
                "{:?}",
                active.issues
            );
        }
    }

    #[test]
    fn phase03_opaque_namespace_reads_require_an_unshadowed_builtin_and_default_namespace() {
        let files = [module(
            "pkg/__init__.py",
            "pkg",
            r#"from builtins import eval as evaluate_text
import builtins as tools

def evaluate(source):
    def eval(value):
        return value
    shadowed = object()
    return eval(source)

def shadowed_alias(source):
    if False:
        evaluate_text = print
    unread_alias = object()
    return evaluate_text(source)

def shadowed_module(source):
    if False:
        tools = object()
    unread_module = object()
    return tools.exec(source)

def explicit(source):
    forgotten = object()
    return exec(source, {})
"#,
        )];
        let result = analyze_phase03(&files, &[]);
        assert!(result.issues.is_empty(), "{:?}", result.issues);
        assert_eq!(
            names(&result, Category::Variable),
            ["forgotten", "shadowed", "unread_alias", "unread_module"]
        );
    }

    #[test]
    fn models_mounted_routes_scheduling_and_process_start() {
        let files = [module(
            "app.py",
            "app",
            "from fastapi import FastAPI, APIRouter\nfrom asyncio import create_task\nfrom concurrent.futures import ThreadPoolExecutor\nfrom multiprocessing import Process\n\napp = FastAPI()\nactive = APIRouter()\ndormant = APIRouter()\napp.include_router(active)\n\n@active.get('/')\ndef mounted():\n    return {'ready': True}\n@dormant.get('/')\ndef unmounted():\n    return False\n\nasync def scheduled():\n    return 1\nasync def unopened():\n    return 2\n\ndef submit_target():\n    return 3\ndef launched():\n    return 4\ndef parked():\n    return 5\n\ncreate_task(scheduled())\nunopened()\nwith ThreadPoolExecutor() as pool:\n    pool.submit(submit_target)\nready = Process(target=launched)\nunused = Process(target=parked)\nready.start()\n",
        )];
        let result = analyze(&files, &[]);
        assert!(result.issues.is_empty(), "{:?}", result.issues);
        assert_eq!(
            names(&result, Category::Function),
            ["parked", "unmounted", "unopened"]
        );
        assert!(
            result
                .evidence
                .iter()
                .any(|evidence| evidence.full_name == "app.mounted"
                    && evidence.kind == "fastapi_registration")
        );
        assert!(result.evidence.iter().any(
            |evidence| evidence.full_name == "app.launched" && evidence.certainty == "possible"
        ));
    }

    #[test]
    fn exports_lazy_target_and_keeps_other_scoped_names_separate() {
        let files = [
            module(
                "pkg/__init__.py",
                "pkg",
                "__all__ = ['work']\n\ndef __getattr__(name):\n    if name == 'work':\n        from .worker import work\n        return work\n    raise AttributeError(name)\n",
            ),
            module("pkg/worker.py", "pkg.worker", "def work():\n    return 1\n"),
            module("other.py", "other", "def work():\n    return 2\n"),
        ];
        let result = analyze(&files, &[]);
        assert!(result.issues.is_empty(), "{:?}", result.issues);
        assert_eq!(names(&result, Category::Function), ["work"]);
        assert!(
            result
                .evidence
                .iter()
                .any(|evidence| evidence.full_name == "pkg.worker.work"
                    && evidence.kind == "public_export")
        );
        assert!(
            result
                .findings
                .iter()
                .any(|found| found.full_name == "other.work")
        );
    }

    #[test]
    fn resolves_literal_lazy_import_with_real_loader_identity() {
        let package = |loader: &str, name: &str| {
            module(
                "pkg/__init__.py",
                "pkg",
                &format!(
                    "{loader}\n__all__ = ['work']\ndef __getattr__(name):\n    if name == 'work':\n        return getattr(load({name}), name)\n    raise AttributeError(name)\n"
                ),
            )
        };
        let provider = || {
            module(
                "pkg/worker.py",
                "pkg.worker",
                "def work():\n    return 1\ndef unused():\n    return 2\n",
            )
        };
        let import = "from importlib import import_module as load";
        let result = analyze(&[package(import, "'pkg.worker'"), provider()], &[]);
        assert!(result.issues.is_empty(), "{:?}", result.issues);
        assert_eq!(names(&result, Category::Function), ["unused"]);
        assert!(result.evidence.iter().any(|evidence| {
            evidence.full_name == "pkg.worker.work" && evidence.kind == "public_export"
        }));

        let opaque = analyze(&[package(import, "name"), provider()], &[]);
        assert!(
            opaque
                .issues
                .iter()
                .any(|issue| issue.kind == "unresolved_lazy_export")
        );
        let unselected_parent = analyze(
            &[
                package(import, "'pkg.sub.worker'"),
                module(
                    "pkg/sub/worker.py",
                    "pkg.sub.worker",
                    "def work():\n    return 1\n",
                ),
            ],
            &[],
        );
        assert!(
            unselected_parent
                .issues
                .iter()
                .any(|issue| issue.kind == "unresolved_lazy_export")
        );

        for hook in [
            "def __getattr__(name):\n    if name == 'work':\n        return getattr(load('worker'), name)\n    if False:\n        getattr = None\n    raise AttributeError(name)\n",
            "@classmethod\ndef __getattr__(name):\n    if name == 'work':\n        return getattr(load('worker'), name)\n    raise AttributeError(name)\n",
        ] {
            let unresolved = analyze(
                &[
                    module(
                        "pkg/__init__.py",
                        "pkg",
                        &format!("{import}\n__all__ = ['work']\n{hook}"),
                    ),
                    module("worker.py", "worker", "def work():\n    return 1\n"),
                ],
                &[],
            );
            assert!(
                unresolved
                    .issues
                    .iter()
                    .any(|issue| issue.kind == "unresolved_lazy_export")
            );
        }

        let collision = analyze(
            &[
                package(
                    "from importlib import import_module as load\ndef worker():\n    return 3",
                    "'pkg.worker'",
                ),
                provider(),
                module(
                    "main.py",
                    "main",
                    "import pkg\nfrom pkg import work\nwork()\npkg.worker()\n",
                ),
            ],
            &[],
        );
        assert!(
            collision
                .issues
                .iter()
                .any(|issue| issue.kind == "unresolved_lazy_export")
        );

        let shadowed = analyze(
            &[
                package(import, "'pkg.worker'"),
                provider(),
                module(
                    "importlib.py",
                    "importlib",
                    "def import_module(name):\n    return None\n",
                ),
            ],
            &[],
        );
        assert!(
            shadowed
                .issues
                .iter()
                .any(|issue| issue.kind == "unresolved_lazy_export")
        );
    }

    #[test]
    fn resolves_literal_lazy_import_tables() {
        let files = [
            module(
                "pkg/__init__.py",
                "pkg",
                "import importlib\n_LAZY = {'work': 'pkg.worker'}\n__all__ = ['work']\ndef __getattr__(name):\n    if name in _LAZY:\n        return getattr(importlib.import_module(_LAZY[name]), name)\n    raise AttributeError(name)\n",
            ),
            module(
                "pkg/worker.py",
                "pkg.worker",
                "def work():\n    return helper()\ndef helper():\n    return 1\ndef unused():\n    return 2\n",
            ),
        ];
        let result = analyze(&files, &[]);
        assert!(result.issues.is_empty(), "{:?}", result.issues);
        assert_eq!(names(&result, Category::Function), ["unused"]);
        assert!(result.evidence.iter().any(|evidence| {
            evidence.full_name == "pkg.worker.work" && evidence.kind == "public_export"
        }));

        let duplicate = analyze(
            &[
                module(
                    "pkg/__init__.py",
                    "pkg",
                    "import importlib\n_LAZY = {'work': 'pkg.retired', 'work': 'pkg.worker'}\n__all__ = ['work']\ndef __getattr__(name):\n    if name in _LAZY:\n        return getattr(importlib.import_module(_LAZY[name]), name)\n    raise AttributeError(name)\n",
                ),
                module("pkg/worker.py", "pkg.worker", "def work():\n    return 1\n"),
                module(
                    "pkg/retired.py",
                    "pkg.retired",
                    "def work():\n    return 2\n",
                ),
            ],
            &[],
        );
        assert!(duplicate.issues.is_empty(), "{:?}", duplicate.issues);
        assert_eq!(names(&duplicate, Category::Function), ["work"]);
        assert!(duplicate.evidence.iter().any(|evidence| {
            evidence.full_name == "pkg.worker.work" && evidence.kind == "public_export"
        }));

        let opaque = analyze(
            &[
                module(
                    "pkg/__init__.py",
                    "pkg",
                    "import importlib\ndef build_table():\n    return {'work': 'pkg.worker'}\n_LAZY = build_table()\n__all__ = ['work']\ndef __getattr__(name):\n    if name in _LAZY:\n        return getattr(importlib.import_module(_LAZY[name]), name)\n    raise AttributeError(name)\n",
                ),
                module(
                    "pkg/worker.py",
                    "pkg.worker",
                    "def work():\n    return helper()\ndef helper():\n    return 1\ndef unused():\n    return 2\n",
                ),
            ],
            &[],
        );
        assert!(
            opaque
                .issues
                .iter()
                .any(|issue| issue.kind == "unresolved_lazy_export"),
            "{:?}",
            opaque.issues
        );
    }

    #[test]
    fn arbitrary_subscript_and_membership_dispatch_remain_incomplete() {
        let result = analyze(
            &[module(
                "main.py",
                "main",
                "def inspect(values, key):\n    return values[key], key in values\ninspect(object(), object())\n",
            )],
            &[],
        );
        for kind in ["unresolved_subscript", "unresolved_membership"] {
            assert!(
                result.issues.iter().any(|issue| issue.kind == kind),
                "missing {kind}: {:?}",
                result.issues
            );
        }
    }

    #[test]
    fn unknown_dispatch_blocks_reachable_but_not_dormant_code() {
        let source = "def go(callback):\n    callback()\n";
        let dormant = analyze(&[module("main.py", "main", source)], &[]);
        assert!(dormant.issues.is_empty(), "{:?}", dormant.issues);
        let active = analyze(
            &[module("main.py", "main", &format!("{source}\ngo(1)\n"))],
            &[],
        );
        assert!(
            active
                .issues
                .iter()
                .any(|issue| issue.kind == "unresolved_call" && issue.line == Some(2))
        );
    }

    #[test]
    fn direct_callback_calls_can_resolve_finite_parameter_targets() {
        let source =
            "def go(callback):\n    callback()\n\ndef local():\n    return 1\n\ngo(local)\n";
        let active = analyze(&[module("main.py", "main", source)], &[]);
        assert!(active.issues.is_empty(), "{:?}", active.issues);
        assert!(
            active
                .evidence
                .iter()
                .any(|edge| edge.full_name == "main.local" && edge.kind == "passed_callable")
        );

        let mixed = analyze(
            &[module("main.py", "main", &format!("{source}\ngo(42)\n"))],
            &[],
        );
        assert!(
            mixed
                .issues
                .iter()
                .any(|issue| issue.kind == "unresolved_call" && issue.line == Some(2))
        );
    }

    #[test]
    fn conditional_shadowing_retains_previous_and_new_targets() {
        let source = "def old():\n    pass\ndef new():\n    pass\nhandler = old\nif flag:\n    handler = new\nhandler()\n";
        let result = analyze(&[module("main.py", "main", source)], &[]);
        assert!(result.issues.is_empty(), "{:?}", result.issues);
        assert!(names(&result, Category::Function).is_empty());
        for target in ["main.old", "main.new"] {
            assert!(result.evidence.iter().any(|evidence| {
                evidence.full_name == target
                    && evidence.kind == "direct_call"
                    && evidence.certainty == "possible"
            }));
        }
    }

    #[test]
    fn a_later_parent_rebinding_cannot_prove_the_earlier_target_unused() {
        let project = tempfile::tempdir().unwrap();
        let config = project.path().join(".skylos.toml");
        let source = project.path().join("main.py");
        std::fs::write(&config, "root = true\n").unwrap();
        let options = super::super::Options {
            config_file: Some(config),
            ..super::super::Options::default()
        };
        for (program, call_line) in [
            (
                "def helper():\n    return 1\ndef caller():\n    helper()\ncaller()\ndef helper():\n    return 2\n",
                4,
            ),
            (
                "def outer():\n    def early():\n        return 1\n    def late():\n        return 2\n    target = early\n    def caller():\n        target()\n    caller()\n    target = late\nouter()\n",
                8,
            ),
        ] {
            std::fs::write(&source, program).unwrap();
            let report = super::super::scan(project.path(), &options).unwrap();
            let output: serde_json::Value =
                serde_json::from_str(&report.to_json().unwrap()).unwrap();
            assert!(!report.is_complete(), "{output}");
            assert_eq!(output["unused_functions"], serde_json::json!([]));
            assert!(output["coverage"]["issues"]
                .as_array()
                .unwrap()
                .iter()
                .any(|issue| issue["kind"] == "ambiguous_binding" && issue["line"] == call_line));
        }
    }

    #[test]
    fn subclass_hooks_follow_executed_class_definitions() {
        let base = "def helper():\n    pass\nclass Base:\n    def __init_subclass__(cls):\n        helper()\n";
        for (child, active) in [
            ("class Child(Base):\n    pass\n", true),
            (
                "ENABLED = False\nif ENABLED:\n    class Outer:\n        class Child(Base):\n            pass\n",
                false,
            ),
        ] {
            let result = analyze(&[module("main.py", "main", &format!("{base}{child}"))], &[]);
            assert!(result.issues.is_empty(), "{child}\n{:?}", result.issues);
            assert_eq!(
                result.evidence.iter().any(|edge| {
                    edge.full_name == "main.Base.__init_subclass__"
                        && edge.kind == "subclass_hook"
                        && edge.certainty == "certain"
                }),
                active,
                "{child}"
            );
            let unused = names(&result, Category::Function);
            assert_eq!(unused.contains(&"helper"), !active, "{child}\n{unused:?}");
        }
    }

    #[test]
    fn unresolved_subclass_hooks_make_class_creation_incomplete() {
        for source in [
            "from foreign import Base\nclass Child(Base):\n    pass\n",
            "class First:\n    def __init_subclass__(cls):\n        pass\nclass Second:\n    pass\nclass Child(First, Second):\n    pass\n",
        ] {
            let result = analyze(&[module("main.py", "main", source)], &[]);
            assert!(
                result
                    .issues
                    .iter()
                    .any(|issue| issue.kind == "unresolved_subclass_hook"),
                "{source}\n{:?}",
                result.issues
            );
        }
    }

    #[test]
    fn instantiating_a_subclass_executes_the_resolved_inherited_constructor() {
        let files = [module(
            "main.py",
            "main",
            "def helper():\n    return 1\ndef runner():\n    return 2\nclass Base:\n    def __init__(self):\n        helper()\n    def run(self):\n        runner()\nclass Middle(Base):\n    pass\nclass Child(Middle):\n    pass\nChild().run()\n",
        )];
        let result = analyze(&files, &[]);
        assert!(result.issues.is_empty(), "{:?}", result.issues);
        assert!(names(&result, Category::Function).is_empty());
        assert!(
            result
                .evidence
                .iter()
                .any(|evidence| evidence.full_name == "main.Base.__init__"
                    && evidence.kind == "constructor")
        );
        assert!(result.evidence.iter().any(
            |evidence| evidence.full_name == "main.Base.run" && evidence.kind == "direct_call"
        ));
    }

    #[test]
    fn custom_allocator_can_return_a_different_selected_class() {
        let files = [module(
            "main.py",
            "main",
            "class Requested:\n    def __new__(cls):\n        return Actual()\n    def deliver(self):\n        return 0\nclass Actual:\n    def deliver(self):\n        return 1\nRequested().deliver()\n",
        )];
        let result = analyze(&files, &[]);
        assert!(result.issues.is_empty(), "{:?}", result.issues);
        assert_eq!(names(&result, Category::Function), ["deliver"]);
        assert!(result.evidence.iter().any(|evidence| {
            evidence.full_name == "main.Actual.deliver" && evidence.kind == "direct_call"
        }));

        let opaque = analyze(
            &[module(
                "main.py",
                "main",
                "from external import allocate\nclass Requested:\n    def __new__(cls):\n        return allocate()\nRequested()\n",
            )],
            &[],
        );
        assert!(
            opaque
                .issues
                .iter()
                .any(|issue| issue.kind == "unresolved_constructor"),
            "{:?}",
            opaque.issues
        );
    }

    #[test]
    fn inherited_method_dispatches_to_the_selected_child_override() {
        let files = [module(
            "main.py",
            "main",
            "def finish_child_work():\n    return 1\nclass Parent:\n    def run(self):\n        return self.step()\n    def step(self):\n        return 0\nclass Child(Parent):\n    def step(self):\n        return finish_child_work()\nChild().run()\n",
        )];
        let result = analyze(&files, &[]);
        assert!(result.issues.is_empty(), "{:?}", result.issues);
        assert_eq!(names(&result, Category::Function), ["step"]);
        for target in [
            "main.Parent.run",
            "main.Child.step",
            "main.finish_child_work",
        ] {
            assert!(
                result
                    .evidence
                    .iter()
                    .any(|evidence| evidence.full_name == target),
                "missing evidence for {target}"
            );
        }
    }

    #[test]
    fn finite_receiver_branch_join_retains_each_selected_method() {
        let classes = "class First:\n    def run(self):\n        return 1\nclass Second:\n    def run(self):\n        return 2\n";
        for usage in [
            "if choose:\n    receiver = First()\nelse:\n    receiver = Second()\nreceiver.run()\n",
            "receiver = First() if choose else Second()\nreceiver.run()\n",
        ] {
            let files = [module("main.py", "main", &format!("{classes}{usage}"))];
            let result = analyze(&files, &[]);
            assert!(result.issues.is_empty(), "{usage}\n{:?}", result.issues);
            assert!(names(&result, Category::Function).is_empty());
            for target in ["main.First.run", "main.Second.run"] {
                assert!(result.evidence.iter().any(|evidence| {
                    evidence.full_name == target
                        && evidence.kind == "direct_call"
                        && evidence.certainty == "possible"
                }));
            }
        }

        let known = analyze(
            &[module(
                "main.py",
                "main",
                &format!("{classes}receiver = First() if True else Second()\nreceiver.run()\n"),
            )],
            &[],
        );
        assert!(known.issues.is_empty(), "{:?}", known.issues);
        assert_eq!(names(&known, Category::Function), ["run"]);
        assert!(known.evidence.iter().any(|evidence| {
            evidence.full_name == "main.First.run" && evidence.kind == "direct_call"
        }));
    }

    #[test]
    fn finite_constructor_alias_retains_each_selected_method() {
        let result = analyze(
            &[module(
                "main.py",
                "main",
                "class First:\n    def run(self):\n        return 1\nclass Second:\n    def run(self):\n        return 2\ndef unused():\n    return 3\nFactory = First if choose else Second\nreceiver = Factory()\nreceiver.run()\n",
            )],
            &[],
        );
        assert!(result.issues.is_empty(), "{:?}", result.issues);
        assert_eq!(names(&result, Category::Function), ["unused"]);
        for target in ["main.First.run", "main.Second.run"] {
            assert!(result.evidence.iter().any(|evidence| {
                evidence.full_name == target
                    && evidence.kind == "direct_call"
                    && evidence.certainty == "possible"
            }));
        }
    }

    #[test]
    fn nested_finite_receiver_paths_retain_every_selected_method() {
        let classes = "class First:\n    def run(self):\n        return 1\nclass Second:\n    def run(self):\n        return 2\nclass Third:\n    def run(self):\n        return 3\ndef unused():\n    return 4\n";
        for usage in [
            "receiver = Third()\nif outer:\n    if inner:\n        receiver = First()\nelse:\n    receiver = Second()\nreceiver.run()\n",
            "receiver = (First() if inner else Second()) if outer else Third()\nreceiver.run()\n",
        ] {
            let result = analyze(
                &[module("main.py", "main", &format!("{classes}{usage}"))],
                &[],
            );
            assert!(result.issues.is_empty(), "{usage}\n{:?}", result.issues);
            assert_eq!(names(&result, Category::Function), ["unused"]);
            for target in ["main.First.run", "main.Second.run", "main.Third.run"] {
                assert!(result.evidence.iter().any(|evidence| {
                    evidence.full_name == target
                        && evidence.kind == "direct_call"
                        && evidence.certainty == "possible"
                }));
            }
        }
    }

    #[test]
    fn conditional_state_limit_remains_incomplete() {
        let mut source =
            "class Target:\n    def run(self):\n        return 1\nreceiver = Target()\n".to_owned();
        for index in 0..13 {
            source.push_str(&format!("if choice_{index}:\n    receiver = Target()\n"));
        }
        source.push_str("receiver.run()\n");

        let result = analyze(&[module("main.py", "main", &source)], &[]);
        assert!(
            result
                .issues
                .iter()
                .any(|issue| issue.kind == "ambiguous_binding"),
            "{:?}",
            result.issues
        );
    }

    #[test]
    fn finite_conditional_export_retains_each_possible_target() {
        let result = analyze(
            &[module(
                "pkg/__init__.py",
                "pkg",
                "__all__ = ['exported']\ndef first():\n    return 1\ndef second():\n    return 2\ndef unused():\n    return 3\nexported = first if choose else second\n",
            )],
            &[],
        );
        assert!(result.issues.is_empty(), "{:?}", result.issues);
        assert_eq!(names(&result, Category::Function), ["unused"]);
        for target in ["pkg.first", "pkg.second"] {
            assert!(result.evidence.iter().any(|evidence| {
                evidence.full_name == target
                    && evidence.kind == "public_export"
                    && evidence.certainty == "possible"
            }));
        }
    }

    #[test]
    fn instance_dispatch_uses_the_first_positional_parameter() {
        let source = "\
def finish_setup():
    return 1

class Service:
    def __init__(instance):
        instance.prepare()
    def prepare(instance):
        return instance.finish()
    def finish(instance, /):
        return finish_setup()
    def unused(instance):
        return 0
";
        for caller in [
            "Service()\n",
            "Factory = Service\ncreated = Factory()\nService.prepare(instance=created)\n",
        ] {
            let result = analyze(
                &[module("main.py", "main", &format!("{source}{caller}"))],
                &[],
            );
            assert!(result.issues.is_empty(), "{caller}\n{:?}", result.issues);
            assert_eq!(names(&result, Category::Function), ["unused"]);
            for target in [
                "main.Service.prepare",
                "main.Service.finish",
                "main.finish_setup",
            ] {
                assert!(
                    result
                        .evidence
                        .iter()
                        .any(|edge| edge.full_name == target && edge.kind == "direct_call"),
                    "missing evidence for {target} after {caller}"
                );
            }
        }
    }

    #[test]
    fn bare_class_body_call_checks_its_supplied_receiver() {
        let files = [module(
            "main.py",
            "main",
            "\
def configured():
    return False

class Configuration:
    def __bool__(self):
        return configured()

class Status:
    def label(config):
        if config:
            return 'on'
        return 'off'
    default = label(Configuration())
print(Status.default)
",
        )];
        let result = analyze(&files, &[]);
        assert!(
            result
                .issues
                .iter()
                .any(|issue| { issue.kind == "unresolved_call" && issue.line == Some(13) }),
            "{:?}",
            result.issues
        );
    }

    #[test]
    fn active_unknown_mro_and_descriptor_dispatch_are_incomplete() {
        for (source, expected_issue) in [
            (
                "from external import Foreign\nclass Child(Foreign):\n    pass\nChild()\n",
                "unresolved_constructor",
            ),
            (
                "def getter():\n    return 1\nclass Base:\n    @property\n    def value(self):\n        return getter()\nclass Child(Base):\n    pass\nprint(Child().value)\n",
                "unresolved_decorator",
            ),
        ] {
            let result = analyze(&[module("main.py", "main", source)], &[]);
            assert!(
                result
                    .issues
                    .iter()
                    .any(|issue| issue.kind == expected_issue),
                "{source}\n{:?}",
                result.issues
            );
        }
    }

    #[test]
    fn pytest_assertion_retains_its_fixture() {
        let files = [module(
            "test_app.py",
            "test_app",
            "from pytest import fixture\n@fixture\ndef resource():\n    return 1\n\ndef test_app(resource):\n    assert resource\n",
        )];
        let result = analyze(&files, &[]);
        assert!(result.issues.is_empty(), "{:?}", result.issues);
        assert!(names(&result, Category::Function).is_empty());
        assert!(names(&result, Category::Parameter).is_empty());
        assert!(
            result
                .evidence
                .iter()
                .any(|evidence| evidence.full_name == "test_app.resource"
                    && evidence.kind == "pytest_root")
        );
    }

    #[test]
    fn phase03_activates_registered_flask_blueprints_only() {
        let files = [
            module(
                "routes.py",
                "routes",
                "from flask import Blueprint\nactive = Blueprint('active', __name__)\ndormant = Blueprint('dormant', __name__)\n@active.route('/')\ndef served():\n    return helper()\n@dormant.route('/')\ndef parked():\n    return 0\ndef helper():\n    return 1\n",
            ),
            module(
                "app.py",
                "app",
                "from flask import Flask\nfrom routes import active\napp = Flask(__name__)\napp.register_blueprint(active)\n",
            ),
        ];
        let result = analyze_phase03(&files, &[]);
        assert!(result.issues.is_empty(), "{:?}", result.issues);
        assert_eq!(names(&result, Category::Function), ["parked"]);
        assert!(result.evidence.iter().any(|entry| {
            entry.full_name == "routes.served" && entry.kind == "flask_registration"
        }));
    }

    #[test]
    fn phase03_django_form_validation_retains_only_effective_callbacks() {
        let source = "
from django import forms
from fields import PHONE

def normalize():
    return 'number'

class Replaced:
    pass

Form = forms.Form
forms.Form = Replaced

class Parent(Form):
    phone = PHONE

class Active(Parent):
    phone = 0
    def clean_phone(self):
        return normalize()
    def clean_missing(self):
        return 'unused'

class Removed(Parent):
    phone = None
    def clean_phone(self):
        return 'unused'

class Inactive(Form):
    def clean(self):
        return 'unused'

class Lookalike(forms.Form):
    def clean(self):
        return 'unused'

Active({'phone': '123'}).is_valid()
Removed({}).full_clean()
Inactive()
Lookalike()
";
        let result = analyze_phase03(
            &[
                module("forms.py", "forms", source),
                module(
                    "fields.py",
                    "fields",
                    "from django.forms.fields import CharField\nPHONE = CharField()\n",
                ),
            ],
            &[],
        );
        assert!(result.issues.is_empty(), "{:?}", result.issues);
        assert_eq!(
            names(&result, Category::Function),
            ["clean", "clean", "clean_missing", "clean_phone"],
        );
        for (target, kind) in [
            ("forms.Active.clean_phone", django_forms::VALIDATION_HOOK),
            ("forms.normalize", "direct_call"),
        ] {
            assert!(result.evidence.iter().any(|entry| {
                entry.full_name == target && entry.kind == kind && entry.certainty == "possible"
            }));
            assert!(
                !result
                    .evidence
                    .iter()
                    .any(|entry| { entry.full_name == target && entry.certainty == "certain" })
            );
        }
    }

    #[test]
    fn phase03_django_form_reports_unresolved_custom_submission_dispatch() {
        let source = "
from django.forms import Form, CharField

class Payload:
    def get(self, key):
        return 'number'

class Contact(Form):
    phone = CharField()

Contact(Payload()).is_valid()
";
        let result = analyze_phase03(&[module("forms.py", "forms", source)], &[]);
        assert!(result.issues.iter().any(|issue| {
            issue.kind == "unresolved_constructor"
                && issue.message.contains("Django form arguments")
        }));
    }

    #[test]
    fn phase03_django_form_validation_uses_c3_field_tables() {
        for (first, second, dispatched) in [("B", "C", false), ("C", "B", true)] {
            let source = format!(
                "
from django.forms import Form, CharField

class A(Form):
    phone = CharField()

class B(A):
    phone = None

class C(A):
    pass

class Contact({first}, {second}):
    def clean_phone(self):
        return 'number'

Contact({{'phone': '123'}}).is_valid()
"
            );
            let result = analyze_phase03(&[module("forms.py", "forms", &source)], &[]);
            assert!(result.issues.is_empty(), "{source}\n{:?}", result.issues);
            assert_eq!(
                result.evidence.iter().any(|entry| {
                    entry.full_name == "forms.Contact.clean_phone"
                        && entry.kind == django_forms::VALIDATION_HOOK
                }),
                dispatched,
            );
        }
    }

    #[test]
    fn phase03_django_model_form_validation_keeps_selected_and_declared_fields() {
        let source = "
from django.forms import ModelForm, CharField
from django.db.models import Model, CharField as Column

class Person(Model):
    phone = Column(max_length=20)
    private = Column(max_length=20, editable=False)

class Contact(ModelForm):
    private = CharField()
    class Meta:
        model = Person
        fields = ['phone']
        exclude = ['private']
    def clean_phone(self):
        return 'phone'
    def clean_private(self):
        return 'private'
    def clean_dormant(self):
        return 'dormant'

Contact({'phone': '123', 'private': 'confirmed'}).is_valid()
";
        let result = analyze_phase03(&[module("forms.py", "forms", source)], &[]);
        assert!(result.issues.is_empty(), "{:?}", result.issues);
        assert_eq!(names(&result, Category::Function), ["clean_dormant"]);
        for name in ["clean_phone", "clean_private"] {
            assert!(result.evidence.iter().any(|entry| {
                entry.full_name == format!("forms.Contact.{name}")
                    && entry.kind == django_forms::VALIDATION_HOOK
                    && entry.certainty == "possible"
            }));
        }
        let unresolved = source.replace("fields = ['phone']", "fields = ['unknown']");
        let result = analyze_phase03(&[module("forms.py", "forms", &unresolved)], &[]);
        assert!(result.issues.iter().any(|issue| {
            issue.kind == "unresolved_registration"
                && issue.message.contains("Django form validation")
        }));
    }

    #[test]
    fn phase03_selects_the_configured_django_urlconf() {
        let files = [
            module("settings.py", "settings", "ROOT_URLCONF = 'project.urls'\n"),
            module(
                "project/urls.py",
                "project.urls",
                "from django.urls import include, path\nfrom views import active\nimport signals\nurlpatterns = [path('', active), path('nested/', include('child_urls'))]\n",
            ),
            module(
                "child_urls.py",
                "child_urls",
                "from django.urls import path\nfrom django.views import View\nclass Child(View):\n    def get(self, request):\n        return 1\nurlpatterns = [path('', Child.as_view())]\n",
            ),
            module(
                "signals.py",
                "signals",
                "from django.dispatch import receiver\nfrom django.db.models.signals import post_save\n@receiver(post_save)\ndef loaded(sender):\n    return 1\n",
            ),
            module(
                "dormant_signals.py",
                "dormant_signals",
                "from django.dispatch import receiver\nfrom django.db.models.signals import post_save\n@receiver(post_save)\ndef unimported(sender):\n    return 1\n",
            ),
            module(
                "unused_urls.py",
                "unused_urls",
                "from django.urls import path\nfrom views import dormant\nurlpatterns = [path('', dormant)]\n",
            ),
            module(
                "views.py",
                "views",
                "def active(request):\n    return helper()\ndef dormant(request):\n    return 0\ndef helper():\n    return 1\n",
            ),
            module(
                "manage.py",
                "manage",
                "import os\nfrom django.core.management import execute_from_command_line\nos.environ.setdefault('DJANGO_SETTINGS_MODULE', 'settings')\nexecute_from_command_line([])\n",
            ),
        ];
        let result = analyze_phase03(&files, &[]);
        assert!(result.issues.is_empty(), "{:?}", result.issues);
        assert_eq!(
            names(&result, Category::Function),
            ["dormant", "unimported"]
        );
        assert!(result.evidence.iter().any(|entry| {
            entry.full_name == "views.active" && entry.kind == "django_registration"
        }));
        assert!(
            result.evidence.iter().any(|entry| {
                entry.full_name == "signals.loaded" && entry.kind == "django_signal"
            })
        );
        assert!(result.evidence.iter().any(|entry| {
            entry.full_name == "child_urls.Child.get" && entry.kind == "framework_lifecycle"
        }));
    }

    #[test]
    fn phase03_django_url_table_stops_at_a_proven_invalid_registration() {
        for invalid in [
            "path('broken/', view, route='duplicate/')",
            "path('broken/', view, name='first', **{'name': 'duplicate'})",
            "path('broken/', view, view=view)",
            "path('broken/', view, unexpected=True)",
            "path('broken/', view, None, None, None)",
            "path('broken/', view, kwargs='invalid')",
            "path('broken/', view, kwargs=[])",
            "path('', include('child_urls', arg='child_urls'))",
            "path('', include('child_urls', unexpected=True))",
            "path('', include('child_urls', None, None))",
            "path('', include('child_urls', namespace='missing'))",
        ] {
            let urls = format!(
                "from django.urls import include, path\nfrom views import view, before, after\n\
                 urlpatterns = [path('good/', before), {invalid}, path('after/', after())]\n\
                 handler404 = view\n"
            );
            let result = analyze_phase03(&django_url_test_files(&urls, ""), &[]);
            assert!(result.issues.is_empty(), "{invalid}: {:?}", result.issues);
            for target in ["views.view", "views.before", "views.after", "views.helper"] {
                assert!(
                    !result.evidence.iter().any(|edge| edge.full_name == target
                        && matches!(edge.kind, "django_registration" | "direct_call")),
                    "{invalid} should not reach {target}"
                );
            }
        }
    }

    #[test]
    fn phase03_django_url_defaults_and_child_namespace_activate_callbacks() {
        let urls = "from django.urls import include, path\nfrom views import before\n\
                    urlpatterns = [path('direct/', before, kwargs=None, name=None), \
                                    path('nested/', include(arg='child_urls', namespace='named'))]\n";
        let result = analyze_phase03(&django_url_test_files(urls, "app_name = 'child'\n"), &[]);
        assert!(result.issues.is_empty(), "{:?}", result.issues);
        for target in ["views.view", "views.before"] {
            assert!(
                result
                    .evidence
                    .iter()
                    .any(|edge| { edge.full_name == target && edge.kind == "django_registration" })
            );
        }
    }

    #[test]
    fn django_view_dispatch_uses_bound_literal_parameters() {
        let make_files = |view, route, extra: &str| {
            [
                module(
                    "manage.py",
                    "manage",
                    "import os\nfrom django.core.management import execute_from_command_line\nos.environ.setdefault('DJANGO_SETTINGS_MODULE', 'settings')\nexecute_from_command_line([])\n",
                ),
                module(
                    "settings.py",
                    "settings",
                    "ROOT_URLCONF = 'urls'\nINSTALLED_APPS = []\n",
                ),
                module(
                    "urls.py",
                    "urls",
                    &format!(
                        "from django.urls import path\nfrom views import view\nurlpatterns = [{route}]\n"
                    ),
                ),
                module("views.py", "views", view),
                module(
                    "pipeline.py",
                    "pipeline",
                    &format!(
                        "from providers import get_client\nclass Pipeline:\n    def __init__(self, client): self.client = client\n    def update(self): return self.client.schedule()\ndef dispatch(slug): return Pipeline(get_client(slug)).update()\n{extra}"
                    ),
                ),
                module(
                    "providers.py",
                    "providers",
                    "class First:\n    def schedule(self): return 1\nclass Second:\n    def schedule(self): return 2\ndef get_client(slug):\n    if slug == 'first': return First()\n    return Second()\n",
                ),
            ]
        };
        let default = "from pipeline import dispatch\ndef view(request, slug='first'): return dispatch(slug)\n";
        let required = default.replace("slug='first'", "slug");
        for (view, route, expected) in [
            (default, "path('', view)", "providers.First.schedule"),
            (
                default,
                "path('', view, {'slug': 'second'})",
                "providers.Second.schedule",
            ),
            (
                required.as_str(),
                "path('', view, {'slug': 'first'})",
                "providers.First.schedule",
            ),
        ] {
            let report = analyze_phase03(&make_files(view, route, ""), &[]);
            assert!(report.issues.is_empty(), "{route}: {:?}", report.issues);
            let reached: Vec<_> = report
                .evidence
                .iter()
                .filter(|edge| edge.kind == "direct_call" && edge.full_name.ends_with(".schedule"))
                .map(|edge| edge.full_name.as_str())
                .collect();
            assert_eq!(reached, [expected], "{route}");
        }
        for (route, extra) in [
            ("path('<str:slug>/', view)", ""),
            ("path('', view)", "dispatch(input())\n"),
        ] {
            let report = analyze_phase03(&make_files(default, route, extra), &[]);
            assert!(
                report
                    .issues
                    .iter()
                    .any(|issue| issue.kind == "unresolved_call"),
                "{route}: {:?}",
                report.issues
            );
        }
        let mut files = make_files(default, "path('', view)", "");
        files[2] = module(
            "urls.py",
            "urls",
            "from django.urls import path\nfrom views import view\nfrom external import mutate\nurlpatterns = [path('', view)]\nmutate(urlpatterns)\n",
        );
        let report = analyze_phase03(&files, &[]);
        assert!(
            report
                .issues
                .iter()
                .any(|issue| issue.kind == "unresolved_call")
        );
        assert!(
            !report
                .evidence
                .iter()
                .any(|edge| edge.full_name.ends_with(".schedule"))
        );
    }

    #[test]
    fn django_failed_view_initializer_cannot_register_a_callback() {
        let urls = "from django.urls import path\nfrom rejected import view\nurlpatterns = [path('', view)]\n";
        let mut files = django_url_test_files(urls, "");
        files.push(module(
            "rejected.py",
            "rejected",
            "def view(request): return helper()\ndef helper(): return 1\nraise external_cause()\n",
        ));
        let report = analyze_phase03(&files, &[]);
        assert!(
            report
                .issues
                .iter()
                .any(|issue| issue.path == "urls.py" && issue.kind == "unresolved_registration")
        );
        assert!(!report.evidence.iter().any(|edge| edge.kind == "django_registration" && edge.full_name == "rejected.view"));
        assert!(
            !report
                .evidence
                .iter()
                .any(|edge| edge.kind == "direct_call" && edge.full_name == "rejected.helper")
        );
    }

    #[test]
    fn django_proven_failed_callback_import_aborts_url_initialization() {
        let urls = "from django.urls import path\nfrom views import view\nfrom unrelated import after\nurlpatterns = [path('', view), path('after/', after)]\n";
        let mut files = django_url_test_files(urls, "");
        files[3] = module(
            "views.py",
            "views",
            "import prefix_effect\ndef view(request): return 1\nraise RuntimeError('cannot load')\n",
        );
        let effect = "from django.db.models.signals import post_save\ndef received(sender, **kwargs): pass\npost_save.connect(received)\n";
        files.extend([
            module("prefix_effect.py", "prefix_effect", effect),
            module(
                "unrelated.py",
                "unrelated",
                &format!("{effect}def after(request): return 2\n"),
            ),
        ]);
        for _ in 0..2 {
            let report = analyze_phase03(&files, &[]);
            assert!(report.issues.is_empty(), "{:?}", report.issues);
            assert!(
                !report
                    .evidence
                    .iter()
                    .any(|row| row.kind == "django_registration")
            );
            let receivers: BTreeSet<_> = report
                .evidence
                .iter()
                .filter(|row| row.kind == "django_signal" && row.certainty == "certain")
                .map(|row| row.full_name.as_str())
                .collect();
            assert_eq!(receivers, ["prefix_effect.received"].into());
            files.swap(1, 3);
        }
    }

    #[test]
    fn phase03_django_url_stops_before_an_unbound_view_option() {
        let urls = "from django.urls import path\nfrom django.views import View\n\
                    class Page(View):\n    def setup(self, request): return 1\n\
                    urlpatterns = [path('page/', Page.as_view(model=Missing))]\n";
        let report = analyze_phase03(&django_url_test_files(urls, ""), &[]);
        assert!(report.issues.is_empty(), "{:?}", report.issues);
        assert!(!report.evidence.iter().any(|edge| {
            (edge.full_name == "urls.Page" && edge.kind == "django_registration")
                || (edge.full_name == "urls.Page.setup" && edge.kind == "framework_lifecycle")
        }));
        for option in ["external()", "ValueError"] {
            let opaque = urls.replace("Missing", option);
            let report = analyze_phase03(&django_url_test_files(&opaque, ""), &[]);
            assert!(
                report
                    .issues
                    .iter()
                    .any(|issue| issue.kind == "unresolved_call")
            );
        }
    }

    #[test]
    fn phase03_migration_declarations_require_the_effective_conventional_export() {
        let source = "from django.db import migrations\nclass Migration(migrations.Migration):\n    dependencies = []\n    operations = []\n    def dormant(self): return 1\n";
        let mut files = vec![
            module("app/__init__.py", "app", ""),
            module("app/migrations/__init__.py", "app.migrations", ""),
            module(
                "app/migrations/0001_initial.py",
                "app.migrations.0001_initial",
                source,
            ),
            module("app/migrations/_old.py", "app.migrations._old", source),
            module(
                "app/migrations/0002_retired.py",
                "app.migrations.0002_retired",
                &format!("{source}Migration = object\n"),
            ),
            module(
                "app/migrations/0003_fake.py",
                "app.migrations.0003_fake",
                "class Migration: pass\n",
            ),
            module("app/tools.py", "app.tools", source),
        ];
        let report = analyze_phase03(&files, &[]);
        assert!(report.issues.is_empty(), "{:?}", report.issues);
        let declarations: BTreeSet<_> = report
            .evidence
            .iter()
            .filter(|row| row.kind == "django_migration_declaration" && row.certainty == "possible")
            .map(|row| row.full_name.as_str())
            .collect();
        assert_eq!(
            declarations,
            [
                "app.migrations.0001_initial.Migration",
                "app.migrations.0001_initial.Migration.dependencies",
                "app.migrations.0001_initial.Migration.operations",
            ]
            .into()
        );
        assert!(report.findings.iter().any(|finding| {
            finding.category == Category::Function
                && finding.full_name == "app.migrations.0001_initial.Migration.dormant"
        }));

        files.extend([
            module(
                "settings.py", "settings",
                "INSTALLED_APPS = ['app']\nMIGRATION_MODULES = {'app': None}\n",
            ),
            module("wsgi.py", "wsgi", "import os\nfrom django.core.wsgi import get_wsgi_application\nos.environ.setdefault('DJANGO_SETTINGS_MODULE', 'settings')\napplication = get_wsgi_application()\n"),
        ]);
        let report = analyze_phase03(&files, &[]);
        assert!(
            !report
                .evidence
                .iter()
                .any(|row| row.kind == "django_migration_declaration")
        );
    }

    #[test]
    fn phase03_django_url_include_preserves_terminal_view_option_failure() {
        let root = "from django.urls import include, path\nfrom views import view\n\
                    urlpatterns = [path('valid/', view), path('', include('child_urls'))]\n";
        for (option, terminal) in [("Missing", true), ("external()", false)] {
            let mut files = django_url_test_files(root, "");
            files[2] = module(
                "child_urls.py",
                "child_urls",
                &format!(
                    "from django.urls import path\nfrom views import Page\nimport included_effect\n\
                     urlpatterns = [path('page/', Page.as_view(model={option}))]\n"
                ),
            );
            files[3] = module(
                "views.py",
                "views",
                "from django.views.generic import DetailView\nclass Page(DetailView): model = None\ndef view(request): return 1\n",
            );
            files.push(module(
                "included_effect.py",
                "included_effect",
                "from django.db.models.signals import post_save\ndef received(sender, **kwargs): pass\npost_save.connect(received)\n",
            ));
            for _ in 0..2 {
                let report = analyze_phase03(&files, &[]);
                assert_eq!(report.issues.is_empty(), terminal, "{:?}", report.issues);
                assert_eq!(
                    report.evidence.iter().any(|edge| {
                        edge.kind == "django_registration" && edge.full_name == "views.view"
                    }),
                    !terminal,
                );
                assert!(report.evidence.iter().any(|edge| {
                    edge.kind == "django_signal"
                        && edge.certainty == "certain"
                        && edge.full_name == "included_effect.received"
                }));
                files.swap(1, 2);
            }
        }
    }

    #[test]
    fn phase03_django_class_based_view_uses_only_reached_dispatch_stages() {
        let prefix = "from django.urls import path\nfrom django.views import View\nclass Parent(View):\n    def get(self, request): return 1\nclass Page(Parent):\n    def post(self, request): return 2\n";
        let mounted = "urlpatterns = [path('page/', Page.as_view())]\n";
        for (override_method, registration, expected) in [
            ("", "Page()\nurlpatterns = []\n", &[][..]),
            ("", mounted, &["urls.Page.post", "urls.Parent.get"][..]),
            (
                "    http_method_names = ['head']\n",
                mounted,
                &["urls.Parent.get"][..],
            ),
            (
                "    def dispatch(self, request): return 3\n",
                mounted,
                &["urls.Page.dispatch"][..],
            ),
            (
                "    def setup(self, request): pass\n",
                mounted,
                &["urls.Page.setup"][..],
            ),
            (
                "    def __init__(self): raise RuntimeError()\n",
                mounted,
                &["urls.Page.__init__"][..],
            ),
        ] {
            let result = analyze_phase03(
                &django_url_test_files(&format!("{prefix}{override_method}{registration}"), ""),
                &[],
            );
            assert!(
                result.issues.is_empty(),
                "{override_method} {registration}: {:?}",
                result.issues
            );
            let callbacks: BTreeSet<_> = result
                .evidence
                .iter()
                .filter(|row| row.kind == "framework_lifecycle")
                .map(|row| row.full_name.as_str())
                .collect();
            assert_eq!(
                callbacks,
                expected.iter().copied().collect(),
                "{override_method} {registration}"
            );
        }
    }

    #[test]
    #[ignore = "requires the pinned FastAPI import-root fixture"]
    fn pinned_fastapi_import_roots() {
        let root = PathBuf::from(std::env::var_os("SKARMORY_PHASE03_IMPORT_ROOT_FIXTURE").unwrap());
        let options = super::super::Options {
            profile: super::super::Profile::SkylosPhase03,
            ..Default::default()
        };
        let config = super::super::config::load(&root, &options).unwrap();
        let project = crate::preview_source::discover_filtered(
            &root,
            &[],
            crate::preview_source::FileEntry::None,
            Some(&config.selection),
        );
        assert!(project.issues.is_empty(), "{:?}", project.issues);
        let files: Vec<_> = project
            .files
            .iter()
            .map(|file| {
                module(
                    &file.label,
                    file.name.strip_suffix(".__init__").unwrap_or(&file.name),
                    &std::fs::read_to_string(root.join(&file.label)).unwrap(),
                )
            })
            .collect();
        let mapped = NestedPackages::index(&files);
        let file = |path| files.iter().position(|file| file.path == path).unwrap();
        let package = file("backend/app/api.py");
        assert_eq!(mapped.roots[package], Some(Path::new("backend")));
        assert_eq!(mapped.resolve(package, "app.api"), "backend.app.api",);

        let source_layout = file("packages/lib/src/lib_pkg/core.py");
        assert_eq!(
            mapped.resolve(source_layout, "lib_pkg.core"),
            "packages.lib.src.lib_pkg.core",
        );

        for (path, name) in [("tools/scripts/run.py", "run"), ("top.py", "top")] {
            let file = file(path);
            assert!(matches!(mapped.resolve(file, name), Cow::Borrowed(_)));
            assert!(
                !mapped
                    .names
                    .keys()
                    .any(|(root, _)| Some(*root) == mapped.roots[file])
            );
        }
    }

    #[test]
    fn django_url_initializers_distinguish_undefined_calls_and_missing_tables() {
        let prefix = "from django.urls import path\nfrom views import view\n";
        for body in [
            "urlpatterns = [path('selected/', view)]\nmissing(urlpatterns)\n",
            "unused_patterns = [path('selected/', view)]\n",
        ] {
            let report =
                analyze_phase03(&django_url_test_files(&format!("{prefix}{body}"), ""), &[]);
            assert!(report.issues.is_empty(), "{body}: {:?}", report.issues);
            assert!(
                !report.evidence.iter().any(
                    |edge| edge.full_name == "views.view" && edge.kind == "django_registration"
                )
            );
        }

        for body in [
            "urlpatterns = [path('selected/', view)]\nfrom plugin import alter\nalter(urlpatterns)\n",
            "from plugin import update\nupdate(globals())\n",
        ] {
            let report =
                analyze_phase03(&django_url_test_files(&format!("{prefix}{body}"), ""), &[]);
            assert!(!report.issues.is_empty(), "{body}");
        }
    }

    #[test]
    fn django_url_callback_observes_a_closed_function_code_replacement() {
        let urls = "from django.urls import path\nfrom patched import view\n\
                    urlpatterns = [path('plain/', view)]\n";
        let source = "def original(): return 1\n\
                      def view(request): return original()\n\
                      def replacement(request): return None\n";
        for receiver in ["view", "selected"] {
            let mut files = django_url_test_files(urls, "");
            files.push(module(
                "patched.py",
                "patched",
                &format!("{source}selected = view\n{receiver}.__code__ = replacement.__code__\n"),
            ));
            let report = analyze_phase03(&files, &[]);
            assert!(report.issues.is_empty(), "{receiver}: {:?}", report.issues);
            assert!(!report.evidence.iter().any(|edge| {
                (edge.full_name == "patched.view" && edge.kind == "django_registration")
                    || (edge.full_name == "patched.original" && edge.kind == "direct_call")
            }));

            files.push(module(
                "consumer.py",
                "consumer",
                "from patched import view\nview(None)\n",
            ));
            let report = analyze_phase03(&files, &[]);
            assert!(report.issues.iter().any(|issue| {
                issue.path == "patched.py" && issue.kind == "unresolved_attribute_write"
            }));
        }

        let mut files = django_url_test_files(urls, "");
        files.push(module(
            "patched.py",
            "patched",
            &format!("{source}view.__code__ = replacement.__code__\nunknown(view)\n"),
        ));
        let report = analyze_phase03(&files, &[]);
        assert!(report.issues.iter().any(|issue| {
            issue.path == "patched.py" && issue.kind == "unresolved_attribute_write"
        }));

        let urls = urls.replace("view)]", "view), path('called/', view(None))]");
        let mut files = django_url_test_files(&urls, "");
        files.push(module(
            "patched.py",
            "patched",
            &format!("{source}view.__code__ = replacement.__code__\n"),
        ));
        let report = analyze_phase03(&files, &[]);
        assert!(report.issues.iter().any(|issue| {
            issue.path == "patched.py" && issue.kind == "unresolved_attribute_write"
        }));
    }

    #[test]
    fn django_url_table_applies_closed_initializer_edits_before_dispatch() {
        for (edit, selected) in [
            ("urlpatterns.clear()", None),
            ("urlpatterns[0].callback = lambda request: None", None),
            ("urlpatterns[0].callback = before", Some("views.before")),
        ] {
            let urls = format!(
                "from django.urls import path\nfrom views import view, before\n\
                 urlpatterns = [path('plain/', view)]\n{edit}\n"
            );
            let report = analyze_phase03(&django_url_test_files(&urls, ""), &[]);
            assert!(report.issues.is_empty(), "{edit}: {:?}", report.issues);
            let routes: Vec<_> = report
                .evidence
                .iter()
                .filter(|edge| edge.kind == "django_registration")
                .map(|edge| edge.full_name.as_str())
                .collect();
            assert_eq!(routes, selected.into_iter().collect::<Vec<_>>());
        }
        let urls = "from django.urls import path\nfrom views import view\n\
                    urlpatterns = [path('plain/', view)]\n\
                    alias = urlpatterns\nurlpatterns.clear()\nunknown(alias)\n";
        let report = analyze_phase03(&django_url_test_files(urls, ""), &[]);
        assert!(
            report
                .issues
                .iter()
                .any(|issue| issue.kind == "unresolved_registration")
        );
    }

    #[test]
    fn phase03_django_url_opaque_options_keep_coverage_incomplete() {
        for (invalid, child_body) in [
            ("path('opaque/', view, kwargs=options)", ""),
            ("path('opaque/', view, Pattern=pattern)", ""),
            (
                "path('', include('child_urls', namespace='site'))",
                "def __getattr__(name):\n    return lookup(name)\n",
            ),
        ] {
            let urls = format!(
                "from django.urls import include, path\nfrom views import view\n\
                 urlpatterns = [{invalid}]\n"
            );
            let result = analyze_phase03(&django_url_test_files(&urls, child_body), &[]);
            assert!(
                result
                    .issues
                    .iter()
                    .any(|issue| issue.kind == "unresolved_registration")
            );
            assert!(
                !result
                    .evidence
                    .iter()
                    .any(|edge| edge.kind == "django_registration")
            );
        }
    }

    #[test]
    fn phase03_django_url_include_keeps_failed_import_coverage_incomplete() {
        let urls = "from django.urls import include, path\n\
                    urlpatterns = [path('', include('child_urls'))]\n";
        let child = "urlpatterns = [path('', view, unexpected=True)]\n";
        let result = analyze_phase03(&django_url_test_files(urls, child), &[]);
        assert!(
            result
                .issues
                .iter()
                .any(|issue| issue.kind == "unresolved_registration")
        );
        assert!(
            !result
                .evidence
                .iter()
                .any(|edge| edge.kind == "django_registration")
        );
    }

    fn django_url_test_files(urls: &str, child_tail: &str) -> Vec<AnalyzedFile> {
        vec![
            module(
                "settings.py",
                "settings",
                "ROOT_URLCONF = 'urls'\nINSTALLED_APPS = []\n",
            ),
            module("urls.py", "urls", urls),
            module(
                "child_urls.py",
                "child_urls",
                &format!(
                    "from django.urls import path\nfrom views import view\n\
                     urlpatterns = [path('', view)]\n{child_tail}"
                ),
            ),
            module(
                "views.py",
                "views",
                "def view(request): return helper()\ndef helper(): return 1\n\
                 def before(request): return 2\ndef after(): return view\n",
            ),
            module(
                "manage.py",
                "manage",
                "import os\nfrom django.core.management import execute_from_command_line\n\
                 os.environ.setdefault('DJANGO_SETTINGS_MODULE', 'settings')\n\
                 execute_from_command_line([])\n",
            ),
        ]
    }

    #[test]
    fn phase03_loads_django_appconfig_only_for_installed_apps() {
        let files = [
            module(
                "settings.py",
                "settings",
                "INSTALLED_APPS = ['demo.apps.Selected', 'automatic']\n",
            ),
            module("demo/__init__.py", "demo", ""),
            module("automatic/__init__.py", "automatic", ""),
            module(
                "demo/apps.py",
                "demo.apps",
                "from django.apps import AppConfig\nclass Base(AppConfig):\n    def ready(self):\n        import demo.signals\nclass Selected(Base):\n    name = 'demo'\nclass Dormant(AppConfig):\n    name = 'demo'\n    def ready(self):\n        parked()\ndef parked():\n    pass\n",
            ),
            module(
                "automatic/apps.py",
                "automatic.apps",
                "from django.apps import AppConfig\nclass Selected(AppConfig):\n    name = 'automatic'\n    default = True\n    def ready(self):\n        loaded()\nclass Dormant(AppConfig):\n    name = 'automatic'\n    def ready(self):\n        parked()\ndef loaded():\n    pass\ndef parked():\n    pass\n",
            ),
            module(
                "demo/signals.py",
                "demo.signals",
                "from django.dispatch import receiver\nfrom django.db.models.signals import post_save\n@receiver(post_save)\ndef on_save(sender):\n    return sender\n",
            ),
            module(
                "main.py",
                "main",
                "import django\nimport os\nos.environ.setdefault('DJANGO_SETTINGS_MODULE', 'settings')\ndjango.setup()\n",
            ),
        ];
        let result = analyze_phase03(&files, &[]);
        assert!(result.issues.is_empty(), "{:?}", result.issues);
        assert_eq!(names(&result, Category::Class), ["Dormant", "Dormant"]);
        assert!(result.evidence.iter().any(|entry| {
            entry.full_name == "demo.apps.Base.ready" && entry.kind == "django_app_config"
        }));
        assert!(result.evidence.iter().any(|entry| {
            entry.full_name == "automatic.apps.Selected.ready" && entry.kind == "django_app_config"
        }));
        assert!(result.evidence.iter().any(|entry| {
            entry.full_name == "demo.signals.on_save" && entry.kind == "django_signal"
        }));
    }

    #[test]
    fn django_appconfig_ready_requires_startup_and_an_effective_app_name() {
        let launcher = "import django, os\nos.environ['DJANGO_SETTINGS_MODULE'] = 'settings'\ndjango.setup()\n";
        for (startup, name, ready, incomplete) in [
            ("", "", false, false),
            (launcher, "", false, true),
            (launcher, "    name = 'shop'\n", true, false),
        ] {
            let result = analyze_phase03(
                &[
                    module(
                        "settings.py",
                        "settings",
                        "INSTALLED_APPS = ['shop.apps.ShopConfig']\n",
                    ),
                    module("shop/__init__.py", "shop", ""),
                    module(
                        "shop/apps.py",
                        "shop.apps",
                        &format!(
                            "from django.apps import AppConfig\nclass ShopConfig(AppConfig):\n{name}    def ready(self):\n        initialize()\ndef initialize(): pass\n"
                        ),
                    ),
                    module("startup.py", "startup", startup),
                ],
                &[],
            );
            let expected_issues: &[&str] = if incomplete {
                &["unresolved_registration"]
            } else {
                &[]
            };
            assert_eq!(
                result
                    .issues
                    .iter()
                    .map(|issue| issue.kind)
                    .collect::<Vec<_>>(),
                expected_issues,
                "{:?}",
                result.issues
            );
            for (name, kind) in [
                ("shop.apps.ShopConfig.ready", "django_app_config"),
                ("shop.apps.initialize", "direct_call"),
            ] {
                assert_eq!(
                    result
                        .evidence
                        .iter()
                        .any(|edge| edge.full_name == name && edge.kind == kind),
                    ready,
                    "{name}: {:?}",
                    result.evidence
                );
            }
        }
    }

    #[test]
    fn phase03_discovers_django_commands_from_the_selected_launcher_and_apps() {
        for (argv, expected) in [
            (
                "",
                &[
                    "demo.management.commands.sync.Command.handle",
                    "demo.management.commands.audit.Command.handle",
                ][..],
            ),
            (
                "['manage.py', 'sync']",
                &["demo.management.commands.sync.Command.handle"][..],
            ),
            ("['manage.py']", &[][..]),
        ] {
            let files = [
                module(
                    "settings.py",
                    "settings",
                    "INSTALLED_APPS = ['demo', 'other']\n",
                ),
                module("demo/__init__.py", "demo", ""),
                module("other/__init__.py", "other", ""),
                module(
                    "manage.py",
                    "manage",
                    &format!(
                        "import os\nfrom django.core.management import execute_from_command_line\nos.environ.setdefault('DJANGO_SETTINGS_MODULE', 'settings')\nexecute_from_command_line({argv})\n"
                    ),
                ),
            ];
            let command_source = "from django.core.management.base import BaseCommand\nclass Command(BaseCommand):\n    def handle(self):\n        return helper()\ndef helper():\n    return 0\n";
            let files: Vec<_> = files
                .into_iter()
                .chain(
                    [
                        (
                            "demo/management/commands/sync.py",
                            "demo.management.commands.sync",
                        ),
                        (
                            "demo/management/commands/audit.py",
                            "demo.management.commands.audit",
                        ),
                        (
                            "demo/management/commands/_private.py",
                            "demo.management.commands._private",
                        ),
                        (
                            "other/management/commands/sync.py",
                            "other.management.commands.sync",
                        ),
                        (
                            "uninstalled/management/commands/sync.py",
                            "uninstalled.management.commands.sync",
                        ),
                    ]
                    .map(|(path, name)| module(path, name, command_source)),
                )
                .collect();
            let result = analyze_phase03(&files, &[]);
            assert!(result.issues.is_empty(), "{argv}: {:?}", result.issues);
            let found: BTreeSet<_> = result
                .evidence
                .iter()
                .filter(|entry| {
                    entry.kind == "django_command" && entry.full_name.ends_with(".handle")
                })
                .map(|entry| entry.full_name.as_str())
                .collect();
            assert_eq!(found, expected.iter().copied().collect(), "{argv}");
        }
    }

    #[test]
    fn phase03_django_command_dispatch_follows_effective_lifecycle() {
        let base = "from django.core.management.base import BaseCommand\nclass Parent(BaseCommand):\n    def handle(self, *args, **options):\n        return None\n";
        let module_name = "demo.management.commands.audit";
        for (source, expected) in [
            ("class Command(Parent):\n    pass\n", vec!["Parent.handle"]),
            (
                "class Command(Parent):\n    def __init__(self):\n        raise RuntimeError()\n",
                vec!["Command.__init__"],
            ),
            (
                "class Command(Parent):\n    def __init__(self, required):\n        return None\n",
                vec![],
            ),
            (
                "class Command(Parent):\n    def add_arguments(self, parser):\n        raise RuntimeError()\n",
                vec!["Command.add_arguments"],
            ),
            (
                "class Command(Parent):\n    def run_from_argv(self, argv):\n        pass\n",
                vec!["Command.run_from_argv"],
            ),
            (
                "class Command(Parent):\n    def execute(self, *args, **options):\n        pass\n",
                vec!["Command.execute"],
            ),
            (
                "class Command(Parent):\n    pass\ndef replacement(self, *args, **options):\n    return None\nCommand.handle = replacement\n",
                vec!["replacement"],
            ),
            (
                "class Command(Parent):\n    def handle(self, *args, **options):\n        return None\ndel Command.handle\n",
                vec!["Parent.handle"],
            ),
        ] {
            let result = analyze_phase03(
                &[
                    module("settings.py", "settings", "INSTALLED_APPS = ['demo']\n"),
                    module("demo/__init__.py", "demo", ""),
                    module(
                        "manage.py",
                        "manage",
                        "from django.core.management import execute_from_command_line\nexecute_from_command_line(['manage.py', 'audit', '--settings=settings'])\n",
                    ),
                    module(
                        "demo/management/commands/audit.py",
                        module_name,
                        &format!("{base}{source}"),
                    ),
                ],
                &[],
            );
            assert!(result.issues.is_empty(), "{source}: {:?}", result.issues);
            let dispatches: Vec<_> = result
                .evidence
                .iter()
                .filter(|entry| entry.kind == "django_command")
                .map(|entry| {
                    entry
                        .full_name
                        .strip_prefix(module_name)
                        .unwrap()
                        .trim_start_matches('.')
                })
                .filter(|name| *name != "Command")
                .collect();
            assert_eq!(dispatches, expected, "{source}");
        }
    }

    #[test]
    fn phase03_django_management_help_stops_after_parser_setup() {
        let command = "from django.core.management.base import BaseCommand\n\
class Command(BaseCommand):\n    def add_arguments(self, parser): return parser\n    def handle(self, *args, **options): return None\n";
        for (arguments, parser, handle) in [
            ("'audit'", true, true),
            ("'audit', '--help'", true, false),
            ("'audit', '-h'", true, false),
            ("'help', 'audit'", true, false),
            ("'help', 'audit', '--commands'", false, false),
            ("'help'", false, false),
            ("'version'", false, false),
        ] {
            let result = analyze_phase03(
                &[
                    module("settings.py", "settings", "INSTALLED_APPS = ['demo']\n"),
                    module("demo/__init__.py", "demo", ""),
                    module(
                        "manage.py",
                        "manage",
                        &format!(
                            "from django.core.management import execute_from_command_line\nexecute_from_command_line(['manage.py', {arguments}, '--settings=settings'])\n"
                        ),
                    ),
                    module(
                        "demo/management/commands/audit.py",
                        "demo.management.commands.audit",
                        command,
                    ),
                    module(
                        "demo/management/commands/version.py",
                        "demo.management.commands.version",
                        command,
                    ),
                    module(
                        "demo/management/commands/help.py",
                        "demo.management.commands.help",
                        command,
                    ),
                ],
                &[],
            );
            assert!(result.issues.is_empty(), "{arguments}: {:?}", result.issues);
            let reached = |method: &str| {
                result
                    .evidence
                    .iter()
                    .any(|edge| edge.kind == "django_command" && edge.full_name.ends_with(method))
            };
            assert_eq!(reached(".add_arguments"), parser, "{arguments}");
            assert_eq!(reached(".handle"), handle, "{arguments}");
        }
        assert!(matches!(frameworks::DjangoCommands::from_argv(&[
            Value::Text("manage.py".into()),
            Value::Text("audit".into()),
            Value::Text("--".into()),
            Value::Text("--help".into()),
        ]), frameworks::DjangoCommands::Selected(name) if name == "audit"));
    }

    #[test]
    fn phase03_django_call_command_follows_programmatic_dispatch_after_setup() {
        let setup = "django.setup()\n";
        let invoke = "run(command_name='audit')\n";
        let imports = "import django\nimport os\nfrom django.core.management import call_command as run\nos.environ.setdefault('DJANGO_SETTINGS_MODULE', 'settings')\n";
        let command = "from django.core.management.base import BaseCommand\ndef helper(): return 1\n\
class Command(BaseCommand):\n    def run_from_argv(self, argv): pass\n    def add_arguments(self, parser): return parser\n    def handle(self, *args, **kwargs): return helper()\n";
        for (actions, started) in [
            (format!("{setup}{invoke}"), true),
            (format!("{invoke}{setup}"), false),
        ] {
            let result = analyze_phase03(
                &[
                    module("settings.py", "settings", "INSTALLED_APPS = ['demo']\n"),
                    module("startup.py", "startup", &format!("{imports}{actions}")),
                    module("demo/__init__.py", "demo", ""),
                    module(
                        "demo/management/commands/audit.py",
                        "demo.management.commands.audit",
                        command,
                    ),
                ],
                &[],
            );
            assert_eq!(
                result.issues.is_empty(),
                started,
                "{actions}: {:?}",
                result.issues
            );
            let reached = |name: &str, kind: &str| {
                result
                    .evidence
                    .iter()
                    .any(|row| row.full_name.ends_with(name) && row.kind == kind)
            };
            assert_eq!(reached(".add_arguments", "django_command"), started);
            assert_eq!(reached(".handle", "django_command"), started);
            assert_eq!(reached(".helper", "direct_call"), started);
            assert!(!reached(".run_from_argv", "django_command"));
        }
    }

    #[test]
    fn phase03_flat_inert_django_shadow_stops_a_command_import() {
        let command = "from django.core.management.base import BaseCommand\ndef prepare(): return 1\nclass Command(BaseCommand):\n    def handle(self, *args, **options): return prepare()\nprepare()\n";
        for (shadow, closed) in [
            ("class BaseCommand: pass\n", true),
            ("modify_imports()\n", false),
        ] {
            let result = analyze_phase03(
                &[
                    module("django.py", "django", shadow),
                    module(
                        "demo/management/commands/audit.py",
                        "demo.management.commands.audit",
                        command,
                    ),
                ],
                &[],
            );
            assert_eq!(
                result.issues.is_empty(),
                closed,
                "{shadow}: {:?}",
                result.issues
            );
            if closed {
                assert!(names(&result, Category::Function).contains(&"prepare"));
                assert!(
                    !result
                        .evidence
                        .iter()
                        .any(|edge| edge.kind == "django_command"
                            || edge.full_name.ends_with(".prepare"))
                );
            }
        }
    }

    #[test]
    fn phase03_django_loader_reads_the_effective_command_module_namespace() {
        let body = "from django.core.management.base import BaseCommand\n\
class Command(BaseCommand):\n    def handle(self, *args, **options):\n        return 1\n\
class Replacement(BaseCommand):\n    def handle(self, *args, **options):\n        return 2\n";
        for (write, selected) in [
            ("globals()['Command'] = Replacement\n", "Replacement"),
            (
                "ns = globals()\nns['Command'] = Replacement\n",
                "Replacement",
            ),
            (
                "import sys\nsys.modules[__name__].Command = Replacement\n",
                "Replacement",
            ),
            (
                "import sys\nns = vars(sys.modules[__name__])\nns['Command'] = Replacement\n",
                "Replacement",
            ),
            (
                "saved = Command\nglobals()['Command'] = Replacement\nCommand = saved\n",
                "Command",
            ),
            ("name = globals().get('__name__')\n", "Command"),
        ] {
            let result = analyze_phase03(
                &[
                    module("settings.py", "settings", "INSTALLED_APPS = ['demo']\n"),
                    module("demo/__init__.py", "demo", ""),
                    module(
                        "manage.py",
                        "manage",
                        "from django.core.management import execute_from_command_line\nexecute_from_command_line(['manage.py', 'audit', '--settings=settings'])\n",
                    ),
                    module(
                        "demo/management/commands/audit.py",
                        "demo.management.commands.audit",
                        &format!("{body}{write}"),
                    ),
                ],
                &[],
            );
            assert!(result.issues.is_empty(), "{write}: {:?}", result.issues);
            let callbacks: Vec<_> = result
                .evidence
                .iter()
                .filter(|row| row.kind == "django_command" && row.full_name.ends_with(".handle"))
                .map(|row| row.full_name.as_str())
                .collect();
            assert_eq!(
                callbacks,
                [format!("demo.management.commands.audit.{selected}.handle")],
                "{write}"
            );
        }
    }

    #[test]
    fn phase03_django_command_code_aliases_observe_ordered_replacement() {
        let source = "from django.core.management.base import BaseCommand\n\
def original(): return 1\n\
class Command(BaseCommand):\n    def handle(self, *args, **options):\n        return original()\n\
def replacement(self, *args, **options): return None\n\
selected = Command.handle\n";
        for (actions, dispatched, direct) in [
            ("metadata = selected.__code__.co_name\n", true, true),
            (
                "Command.handle.__code__ = replacement.__code__\n",
                false,
                false,
            ),
            (
                "selected.__code__ = replacement.__code__\nselected(None)\n",
                false,
                false,
            ),
            (
                "selected(None)\nselected.__code__ = replacement.__code__\nselected(None)\n",
                false,
                true,
            ),
        ] {
            let result = analyze_phase03(
                &[
                    module("settings.py", "settings", "INSTALLED_APPS = ['demo']\n"),
                    module("demo/__init__.py", "demo", ""),
                    module(
                        "manage.py",
                        "manage",
                        "from django.core.management import execute_from_command_line\nexecute_from_command_line(['manage.py', 'audit', '--settings=settings'])\n",
                    ),
                    module(
                        "demo/management/commands/audit.py",
                        "demo.management.commands.audit",
                        &format!("{source}{actions}"),
                    ),
                ],
                &[],
            );
            assert!(result.issues.is_empty(), "{actions}: {:?}", result.issues);
            assert_eq!(
                result
                    .evidence
                    .iter()
                    .any(|entry| entry.kind == "django_command"
                        && entry.full_name.ends_with(".Command.handle")),
                dispatched,
                "{actions}"
            );
            assert_eq!(
                result
                    .evidence
                    .iter()
                    .any(|entry| entry.kind == "direct_call"
                        && entry.full_name.ends_with(".original")),
                direct,
                "{actions}"
            );
        }
        let opaque = analyze_phase03(
            &[module(
                "app.py",
                "app",
                &format!("{source}selected.__code__ = unknown\nselected(None)\n"),
            )],
            &[],
        );
        assert!(
            opaque
                .issues
                .iter()
                .any(|issue| issue.kind == "unresolved_call")
        );
        assert!(
            !opaque
                .evidence
                .iter()
                .any(|entry| entry.kind == "direct_call" && entry.full_name.ends_with(".original"))
        );
    }

    #[test]
    fn phase03_django_command_lookup_hooks_require_a_reached_dispatch() {
        for hook in ["__getattribute__", "__getattr__"] {
            let module_name = "demo.management.commands.audit";
            let source = format!(
                "from django.core.management.base import BaseCommand\n\
class Command(BaseCommand):\n    def {hook}(self, name): return unknown()\n    def handle(self, *args, **options): return 1\n"
            );
            let command = || module("demo/management/commands/audit.py", module_name, &source);
            let dormant = analyze_phase03(&[command()], &[]);
            assert!(dormant.issues.is_empty(), "{hook}: {:?}", dormant.issues);
            let active = analyze_phase03(
                &[
                    module("settings.py", "settings", "INSTALLED_APPS = ['demo']\n"),
                    module("demo/__init__.py", "demo", ""),
                    module(
                        "manage.py",
                        "manage",
                        "from django.core.management import execute_from_command_line\nexecute_from_command_line(['manage.py', 'audit', '--settings=settings'])\n",
                    ),
                    command(),
                ],
                &[],
            );
            assert!(
                active
                    .issues
                    .iter()
                    .any(|issue| issue.kind == "unresolved_registration"),
                "{hook}: {:?}",
                active.issues
            );
            assert!(
                !active
                    .evidence
                    .iter()
                    .any(|entry| entry.kind == "django_command"
                        && entry.full_name.ends_with(".Command.handle"))
            );
        }
    }

    #[test]
    fn phase03_django_orm_default_construction_only_reaches_actual_initializer_hooks() {
        let result = analyze_phase03(
            &[module(
                "app.py",
                "app",
                "from django.db.models import Manager, QuerySet\n\
class WorkManager(Manager):\n    def get_queryset(self): return None\n    def _set_creation_counter(self): return None\n\
class WorkSet(QuerySet):\n    def delete(self): return None\n\
WorkManager()\nWorkSet()\n",
            )],
            &[],
        );
        assert!(result.issues.is_empty(), "{:?}", result.issues);
        assert!(result.evidence.iter().any(|entry| {
            entry.kind == "django_manager_initializer"
                && entry.full_name == "app.WorkManager._set_creation_counter"
        }));
        assert_eq!(
            names(&result, Category::Function),
            ["delete", "get_queryset"]
        );
    }

    #[test]
    fn phase03_django_command_parameter_role_requires_a_genuine_receiver() {
        for (base, retained) in [
            (
                "from django.core.management.base import BaseCommand\n",
                true,
            ),
            ("class BaseCommand: pass\n", false),
        ] {
            let source = format!(
                "{base}class Worker(BaseCommand):\n    def handle(self, unused): return 1\nWorker().handle(2)\n"
            );
            let result = analyze_phase03(&[module("app.py", "app", &source)], &[]);
            assert!(result.issues.is_empty(), "{base}: {:?}", result.issues);
            assert_eq!(
                result.evidence.iter().any(|entry| {
                    entry.kind == "django_command_parameter"
                        && entry.full_name == "app.Worker.handle.unused"
                }),
                retained
            );
            assert_eq!(
                names(&result, Category::Parameter).contains(&"unused"),
                !retained
            );
            assert!(
                !result
                    .evidence
                    .iter()
                    .any(|entry| entry.kind == "django_command")
            );
        }
    }

    #[test]
    fn phase03_django_relationship_field_declaration_does_not_render_choices() {
        let definition = "from django import forms\n\
class RelatedField(forms.ModelChoiceField):\n    def label_from_instance(self, obj): return str(obj)\n\
class AnotherField(RelatedField): pass\n";
        let declaration = analyze_phase03(&[module("fields.py", "fields", definition)], &[]);
        assert!(declaration.issues.is_empty(), "{:?}", declaration.issues);
        assert!(
            !declaration
                .evidence
                .iter()
                .any(|row| row.full_name == "fields.RelatedField.label_from_instance")
        );
        let form = format!(
            "{definition}class Contact(forms.Form):\n    related = RelatedField(queryset=None, required=True)\n"
        );
        let installed = analyze_phase03(&[module("fields.py", "fields", &form)], &[]);
        assert!(installed.issues.is_empty(), "{:?}", installed.issues);
        assert!(installed.evidence.iter().any(|row| {
            row.full_name == "fields.RelatedField.label_from_instance"
                && row.kind == "django_relationship_field_declaration"
                && row.certainty == "possible"
        }));
        let dynamic = form.replace("queryset=None", "queryset=external_query");
        let dynamic = analyze_phase03(&[module("fields.py", "fields", &dynamic)], &[]);
        assert!(
            dynamic
                .issues
                .iter()
                .any(|issue| issue.kind == "unresolved_constructor"),
            "{:?}",
            dynamic.issues
        );

        let constructed = analyze_phase03(
            &[module(
                "fields.py",
                "fields",
                &format!("{definition}AnotherField(queryset=None)\n"),
            )],
            &[],
        );
        assert!(
            constructed
                .issues
                .iter()
                .any(|issue| issue.kind == "unresolved_constructor"),
            "{:?}",
            constructed.issues
        );
    }

    #[test]
    fn phase03_django_form_table_edits_follow_instance_construction() {
        let form = "from django import forms\n\
class Contact(forms.Form):\n    def clean_phone(self):\n        return 'ok'\n";
        for (table, instance, retained) in [
            ("base_fields", "", true),
            ("declared_fields", "", true),
            ("base_fields", "form = Contact({'phone': '123'})\n", false),
            (
                "declared_fields",
                "form = Contact({'phone': '123'})\n",
                false,
            ),
        ] {
            let validate = if retained {
                "Contact({'phone': '123'}).is_valid()"
            } else {
                "alias = form\nalias.is_valid()"
            };
            let source = format!(
                "{form}{instance}Contact.{table}['phone'] = forms.CharField()\n{validate}\n"
            );
            let result = analyze_phase03(&[module("app.py", "app", &source)], &[]);
            assert!(result.issues.is_empty(), "{source}: {:?}", result.issues);
            assert_eq!(
                result.evidence.iter().any(|entry| {
                    entry.kind == django_forms::VALIDATION_HOOK
                        && entry.full_name == "app.Contact.clean_phone"
                        && entry.certainty == "possible"
                }),
                retained,
                "{source}"
            );
        }
    }

    #[test]
    fn phase03_django_form_initializer_preserves_literal_instance_fields() {
        for (prefix, base, meta, expected) in [
            ("", "Form", "", true),
            (
                "",
                "ModelForm",
                "    class Meta:\n        model = Person\n        fields = ['name']\n",
                true,
            ),
            ("super = object\n", "Form", "", false),
        ] {
            let source = format!(
                concat!(
                    "from django import forms\nfrom django.db import models\n{}",
                    "class Person(models.Model):\n    name = models.CharField(max_length=20)\n",
                    "class Contact(forms.{}):\n{}",
                    "    def __init__(self, *args, **kwargs):\n",
                    "        super().__init__(*args, **kwargs)\n",
                    "        self.fields['phone'] = forms.CharField()\n",
                    "    def clean_phone(self):\n        return 'ok'\n",
                    "Contact({{'name': 'Jane', 'phone': '123'}}).is_valid()\n"
                ),
                prefix, base, meta
            );
            let result = analyze_phase03(&[module("app.py", "app", &source)], &[]);
            assert_eq!(
                result.issues.is_empty(),
                expected,
                "{base}: {:?}",
                result.issues
            );
            assert_eq!(
                result.evidence.iter().any(|entry| {
                    entry.kind == django_forms::VALIDATION_HOOK
                        && entry.full_name == "app.Contact.clean_phone"
                        && entry.certainty == "possible"
                }),
                expected,
                "{base}"
            );
        }
    }

    #[test]
    fn phase03_deleted_django_autoreload_member_stops_before_arguments_and_following_code() {
        let first = "from django.utils import autoreload\ndef callback():\n    return 1\ndef argument():\n    return callback\ndelattr(autoreload, 'run_with_reloader')\n";
        for failing in [
            "reloader = autoreload.run_with_reloader\nreloader(callback)",
            "autoreload.run_with_reloader(argument())",
        ] {
            let source = format!("{first}{failing}\ncallback()\n");
            let result = analyze_phase03(&[module("app.py", "app", &source)], &[]);
            assert!(result.issues.is_empty(), "{failing}: {:?}", result.issues);
            assert_eq!(names(&result, Category::Function), ["argument", "callback"]);
        }
    }

    #[test]
    fn phase03_shadowed_django_reloader_stops_at_missing_builtin_attribute() {
        let result = analyze_phase03(
            &[
                module("django/__init__.py", "django", ""),
                module("django/utils.py", "django.utils", "autoreload = object()\n"),
                module(
                    "app.py",
                    "app",
                    "from django.utils import autoreload\ndef callback():\n    return 1\n\nautoreload.run_with_reloader(callback())\n",
                ),
            ],
            &[],
        );
        assert!(result.issues.is_empty(), "{:?}", result.issues);
        assert_eq!(names(&result, Category::Function), ["callback"]);
    }

    #[test]
    fn phase03_autoreload_parameter_proof_excludes_descriptor_bound_arguments() {
        let definitions = "\
def callback():
    return None
def start(loader, optional=None):
    loader.run_with_reloader(callback)
class Target:
    def run_with_reloader(self, callback):
        return None
class Command:
    start = start
    def run_with_reloader(self, callback):
        return None
";
        for (calls, bounded) in [
            ("start(Target())\n", true),
            ("Command().start(Target())\n", false),
            ("saved = Command().start\nsaved(Target())\n", false),
            ("start(Target())\nCommand().start(Target())\n", false),
        ] {
            let result = analyze_phase03(
                &[module("app.py", "app", &format!("{definitions}{calls}"))],
                &[],
            );
            assert_eq!(
                result.issues.is_empty(),
                bounded,
                "{calls}: {:?}",
                result.issues
            );
            assert_eq!(
                result.evidence.iter().any(|entry| {
                    entry.full_name == "app.Target.run_with_reloader"
                        && entry.kind == "direct_call"
                        && entry.certainty == "certain"
                }),
                bounded,
                "{calls}"
            );
        }
    }

    #[test]
    fn phase03_reports_unresolved_django_installation() {
        let settings = module(
            "settings.py",
            "settings",
            "INSTALLED_APPS = ['demo.apps.Unknown']\n",
        );
        let startup = module(
            "main.py",
            "main",
            "import django\nimport os\nos.environ.setdefault('DJANGO_SETTINGS_MODULE', 'settings')\ndjango.setup()\n",
        );
        let result = analyze_phase03(&[settings, startup], &[]);
        assert!(
            result.issues.iter().any(|issue| {
                issue.path == "settings.py"
                    && issue.kind == "unresolved_registration"
                    && issue.message.contains("INSTALLED_APPS")
            }),
            "{:?}",
            result.issues
        );
    }

    #[test]
    fn phase03_django_app_config_failure_precedes_models_and_ready_hooks() {
        let shop = || module("shop/__init__.py", "shop", "");
        let launch = || {
            module(
                "startup.py",
                "startup",
                "import django\nimport os\nos.environ.setdefault('DJANGO_SETTINGS_MODULE', 'settings')\ndjango.setup()\n",
            )
        };
        let ready = || {
            module(
                "shop/apps.py",
                "shop.apps",
                "from django.apps import AppConfig\nclass ShopConfig(AppConfig):\n    name = 'shop'\n    def ready(self):\n        from . import signals\n",
            )
        };
        let signal = "from django.db.models.signals import post_save\ndef callback(sender, **kwargs): return None\npost_save.connect(callback)\n";
        for (apps, loaded) in [
            ("['shop.apps.ShopConfig']", true),
            ("['shop.apps.ShopConfig', 'bad.apps.Broken']", false),
        ] {
            let result = analyze_phase03(
                &[
                    module(
                        "settings.py",
                        "settings",
                        &format!("INSTALLED_APPS = {apps}\n"),
                    ),
                    launch(),
                    shop(),
                    ready(),
                    module("shop/signals.py", "shop.signals", signal),
                    module("shop/models.py", "shop.models", signal),
                    module("bad/__init__.py", "bad", ""),
                    module("bad/apps.py", "bad.apps", "Broken = object\n"),
                ],
                &[],
            );
            assert_eq!(
                result.issues.is_empty(),
                loaded,
                "{apps}: {:?}",
                result.issues
            );
            let activated: BTreeSet<_> = result
                .evidence
                .iter()
                .filter(|event| event.kind == "django_signal" && event.certainty == "certain")
                .map(|event| event.full_name.as_str())
                .collect();
            assert_eq!(
                activated,
                if loaded {
                    BTreeSet::from(["shop.models.callback", "shop.signals.callback"])
                } else {
                    BTreeSet::new()
                },
                "{apps}"
            );
        }
    }

    #[test]
    fn phase03_django_setup_keeps_only_effects_before_a_proven_failure() {
        let signal = "from django.db.models.signals import post_save\ndef callback(sender, **kwargs): return None\npost_save.connect(callback)\n";
        for (failure, callbacks) in [
            ("constructor", &[][..]),
            ("models", &[][..]),
            (
                "ready",
                &["later.models.callback", "shop.models.callback"][..],
            ),
        ] {
            let constructor = if failure == "constructor" {
                "    def __init__(self, name, module): raise RuntimeError()\n"
            } else {
                ""
            };
            let ready = if failure == "ready" {
                "raise RuntimeError()"
            } else {
                "pass"
            };
            let models = if failure == "models" {
                "raise RuntimeError()\n"
            } else {
                signal
            };
            let result = analyze_phase03(
                &[
                    module(
                        "settings.py",
                        "settings",
                        "INSTALLED_APPS = ['shop.apps.ShopConfig', 'later.apps.LaterConfig']\n",
                    ),
                    module(
                        "startup.py",
                        "startup",
                        "import django\nimport os\nos.environ.setdefault('DJANGO_SETTINGS_MODULE', 'settings')\ndjango.setup()\n",
                    ),
                    module("shop/__init__.py", "shop", ""),
                    module(
                        "shop/apps.py",
                        "shop.apps",
                        &format!(
                            "from django.apps import AppConfig\nclass ShopConfig(AppConfig):\n    name = 'shop'\n{constructor}    def ready(self): {ready}\n"
                        ),
                    ),
                    module("shop/models.py", "shop.models", models),
                    module("later/__init__.py", "later", ""),
                    module(
                        "later/apps.py",
                        "later.apps",
                        "from django.apps import AppConfig\nclass LaterConfig(AppConfig):\n    name = 'later'\n    def ready(self):\n        from . import signals\n",
                    ),
                    module("later/models.py", "later.models", signal),
                    module("later/signals.py", "later.signals", signal),
                ],
                &[],
            );
            assert!(
                result
                    .issues
                    .iter()
                    .any(|issue| issue.kind == "unresolved_registration"),
                "{failure}: {:?}",
                result.issues
            );
            let active: BTreeSet<_> = result
                .evidence
                .iter()
                .filter(|row| row.kind == "django_signal" && row.certainty == "certain")
                .map(|row| row.full_name.as_str())
                .collect();
            assert_eq!(active, callbacks.iter().copied().collect(), "{failure}");
        }
    }

    #[test]
    fn phase03_django_receiver_unbound_keyword_mapping_stops_after_positional_arguments() {
        let result = analyze_phase03(
            &[module(
                "app.py",
                "app",
                "from django.dispatch import receiver\nfrom django.db.models.signals import post_save\ndef select(): return post_save\n@receiver(select(), **options)\ndef blocked(sender, **kwargs): return None\ndef later(): return 1\nlater()\n",
            )],
            &[],
        );
        assert!(result.issues.is_empty(), "{:?}", result.issues);
        assert!(
            result
                .evidence
                .iter()
                .any(|event| event.full_name == "app.select" && event.kind == "direct_call")
        );
        assert!(
            !result
                .evidence
                .iter()
                .any(|event| event.full_name == "app.blocked" || event.full_name == "app.later")
        );
    }

    #[test]
    fn phase03_launched_django_command_forwards_autoreload_through_a_bound_method() {
        for call in [
            "self.start(callback)",
            "saved = self.start\n        saved(target=callback)",
        ] {
            let command = format!(
                "from django.core.management.base import BaseCommand\nfrom django.utils import autoreload\ndef callback(): return None\n\
class Command(BaseCommand):\n    def handle(self, *args, **options):\n        {call}\n    def start(self, target):\n        autoreload.run_with_reloader(target)\n"
            );
            let selected = analyze_phase03(
                &[
                    module("settings.py", "settings", "INSTALLED_APPS = ['demo']\n"),
                    module("demo/__init__.py", "demo", ""),
                    module(
                        "manage.py",
                        "manage",
                        "from django.core.management import execute_from_command_line\nexecute_from_command_line(['manage.py', 'audit', '--settings=settings'])\n",
                    ),
                    module(
                        "demo/management/commands/audit.py",
                        "demo.management.commands.audit",
                        &command,
                    ),
                ],
                &[],
            );
            assert!(selected.issues.is_empty(), "{call}: {:?}", selected.issues);
            assert!(
                selected
                    .evidence
                    .iter()
                    .any(|event| event.full_name.ends_with(".callback")
                        && event.kind == "django_autoreload"
                        && event.certainty == "certain"),
                "{call}: {:?}",
                selected.evidence
            );
            let dormant = analyze_phase03(
                &[module(
                    "demo/management/commands/audit.py",
                    "demo.management.commands.audit",
                    &command,
                )],
                &[],
            );
            assert!(
                !dormant
                    .evidence
                    .iter()
                    .any(|event| event.kind == "django_autoreload")
            );
        }
    }

    #[test]
    fn phase03_autoreload_wrapper_rejects_an_unmapped_descriptor_receiver() {
        let result = analyze_phase03(
            &[module(
                "app.py",
                "app",
                "from django.utils import autoreload\ndef callback(): return None\ndef forward(target, extra=None):\n    autoreload.run_with_reloader(target)\nclass Adapter:\n    run = forward\nAdapter().run(callback)\n",
            )],
            &[],
        );
        assert!(
            result
                .issues
                .iter()
                .any(|issue| issue.kind == "unresolved_registration")
        );
        assert!(
            !result
                .evidence
                .iter()
                .any(|event| event.kind == "django_autoreload")
        );
    }

    #[test]
    fn phase03_flask_lookalike_does_not_register_and_unknown_blueprint_is_incomplete() {
        let lookalike = module(
            "fake.py",
            "fake",
            "def route(handler):\n    return handler\n@route\ndef parked():\n    return 1\n",
        );
        let result = analyze_phase03(&[lookalike], &[]);
        assert!(result.issues.is_empty(), "{:?}", result.issues);
        assert_eq!(names(&result, Category::Function), ["parked"]);

        let web = module(
            "web.py",
            "web",
            "from flask import Flask\napp = Flask(__name__)\napp.register_blueprint(selected_blueprint)\n",
        );
        let result = analyze_phase03(&[web], &[]);
        assert!(
            result
                .issues
                .iter()
                .any(|issue| issue.kind == "unresolved_registration")
        );
    }

    #[test]
    fn django_standalone_converter_routes_keep_bounded_possible_callbacks() {
        let source = "from django.http import HttpResponse\n\
                      from django.urls import path, register_converter\n\
                      from converters import Slug\n\
                      name = 'cu' + 'stom'\n\
                      register_converter(Slug, type_name=name)\n\
                      def view(request, value): return HttpResponse(value)\n\
                      urlpatterns = [path('<custom:value>/', view)]\n";
        let build = |urls: &str| {
            vec![
                module("urls.py", "urls", urls),
                module(
                    "converters.py",
                    "converters",
                    "class Slug:\n    regex = '[a-z]+'\n    def to_python(self, value): return value\n\
                     \x20   def to_url(self, value): return value\n",
                ),
            ]
        };
        let report = analyze_phase03(&build(source), &[]);
        assert!(report.issues.is_empty(), "{:?}", report.issues);
        assert!(report.evidence.iter().any(|edge| {
            edge.full_name == "urls.view"
                && edge.kind == "django_registration"
                && edge.certainty == "possible"
        }));
        assert!(!report.evidence.iter().any(|edge| {
            edge.full_name == "urls.view"
                && edge.kind == "django_registration"
                && edge.certainty == "certain"
        }));

        for unproved in [
            source.replace("<custom:value>", "<unknown:value>"),
            format!("{source}from plugin import mutate\nmutate(urlpatterns)\n"),
        ] {
            let report = analyze_phase03(&build(&unproved), &[]);
            assert!(
                report
                    .issues
                    .iter()
                    .any(|issue| issue.kind == "unresolved_registration")
            );
        }

        let mut files = build(source);
        files.extend([
            module("settings.py", "settings", "ROOT_URLCONF = 'other'\nINSTALLED_APPS = []\n"),
            module("other.py", "other", "urlpatterns = []\n"),
            module(
                "manage.py",
                "manage",
                "import os\nfrom django.core.management import execute_from_command_line\n\
                 os.environ.setdefault('DJANGO_SETTINGS_MODULE', 'settings')\nexecute_from_command_line([])\n",
            ),
        ]);
        let report = analyze_phase03(&files, &[]);
        assert!(
            !report.evidence.iter().any(|edge| {
                edge.full_name == "urls.view" && edge.kind == "django_registration"
            })
        );
    }

    #[test]
    fn django_converter_uses_effective_class_method_and_provider_bindings() {
        let converter = "class Converter:\n    regex = '[a-z]+'\n\
                         \x20   def to_python(self, value): return value\n\
                         \x20   def to_url(self, value): return value\n";
        let urls = "from django.urls import register_converter\n\
                    from converters import Converter\nregister_converter(Converter, 'custom')\n";
        let class = format!(
            "class Replacement:\n    regex = '.*'\n    def to_python(self, value): return value\n\
             def redirect(cls): return Replacement\n@redirect\n{converter}"
        );
        let method = format!(
            "def redirect(callback):\n    def replacement(self, value): return value\n\
             \x20   return replacement\n{}",
            converter.replace("    def to_python", "    @redirect\n    def to_python")
        );
        let shadowed = "import django.urls\nfrom converters import Converter\n\
            class Registry:\n    def register_converter(self, cls, name): return None\n\
            django.urls = Registry()\ndjango.urls.register_converter(Converter, 'custom')\n";
        for (source, urls, replacement) in [
            (
                class.as_str(),
                urls,
                Some("converters.Replacement.to_python"),
            ),
            (
                method.as_str(),
                urls,
                Some("converters.redirect.replacement"),
            ),
            (converter, shadowed, None),
        ] {
            let report = analyze_phase03(
                &[
                    module("converters.py", "converters", source),
                    module("urls.py", "urls", urls),
                ],
                &[],
            );
            assert!(report.issues.is_empty(), "{:?}", report.issues);
            assert!(!report.evidence.iter().any(|edge| {
                edge.kind == "django_path_converter"
                    && edge.full_name == "converters.Converter.to_python"
            }));
            if let Some(replacement) = replacement {
                assert!(report.evidence.iter().any(|edge| {
                    edge.kind == "django_path_converter" && edge.full_name == replacement
                }));
            }
        }
    }

    #[test]
    fn django_auth_backend_uses_the_class_returned_by_a_closed_metaclass() {
        let source = "class Effective:\n    def get_user(self, pk): return pk\n\
                      class Redirect(type):\n    def __new__(mcls, name, bases, namespace):\n\
                      \x20       return Effective\n\
                      class Configured(metaclass=Redirect):\n\
                      \x20   def get_user(self, pk): return retired(pk)\n\
                      def retired(pk): return pk\n";
        for (provider, complete) in [
            (source.to_owned(), true),
            (
                source.replace("return Effective", "return external(Effective)"),
                false,
            ),
            (
                source.replace(
                    "class Redirect(type):",
                    "class Redirect(type):\n    __prepare__ = external",
                ),
                false,
            ),
            (
                source.replace("return retired(pk)", "return super().get_user(pk)"),
                false,
            ),
        ] {
            let files = [
                module("shop/__init__.py", "shop", ""),
                module(
                    "shop/settings.py",
                    "shop.settings",
                    "INSTALLED_APPS = ['shop']\nSECRET_KEY = 'fixture'\n\
                    AUTHENTICATION_BACKENDS = ['shop.backends.Configured']\n",
                ),
                module("shop/backends.py", "shop.backends", &provider),
            ];
            let report = analyze_phase03(&files, &[]);
            assert_eq!(report.issues.is_empty(), complete, "{:?}", report.issues);
            let hooks: Vec<_> = report
                .evidence
                .iter()
                .filter_map(|row| {
                    (row.kind == "django_auth_backend").then_some(row.full_name.as_str())
                })
                .collect();
            if complete {
                assert_eq!(hooks, ["shop.backends.Effective.get_user"]);
                assert!(names(&report, Category::Function).contains(&"retired"));
            } else {
                assert!(hooks.is_empty(), "{hooks:?}");
                assert!(
                    report
                        .issues
                        .iter()
                        .any(|issue| issue.kind == "unresolved_metaclass")
                );
            }
        }
    }

    #[test]
    fn django_converter_resolves_the_inherited_metaclass_namespace_write() {
        let source = "def passthrough(self, value): return value\n\
                      class Rewrite(type):\n    def __new__(meta, name, bases, namespace):\n\
                      \x20       namespace['to_python'] = passthrough\n\
                      \x20       return super().__new__(meta, name, bases, namespace)\n\
                      class Base(metaclass=Rewrite): pass\n\
                      class Converter(Base):\n    regex = '.*'\n\
                      \x20   def to_python(self, value): return obsolete(value)\n\
                      \x20   def to_url(self, value): return value\n\
                      def obsolete(value): return value\n\
                      Converter()\n";
        for (provider, issue_kind) in [
            (source.to_owned(), None),
            (
                source.replace("super().__new__", "external().__new__"),
                Some("unresolved_metaclass"),
            ),
            (
                source.replace(
                    "class Base(metaclass=Rewrite): pass",
                    "class Base(metaclass=Rewrite):\n    def __init_subclass__(cls): unknown(cls)",
                ),
                Some("unresolved_metaclass"),
            ),
            (
                format!("{source}Converter.to_python = obsolete\n"),
                Some("unresolved_registration"),
            ),
        ] {
            let files = [
                module("converters.py", "converters", &provider),
                module(
                    "urls.py",
                    "urls",
                    "from django.urls import register_converter\n\
                     from converters import Converter\nregister_converter(Converter, 'custom')\n",
                ),
            ];
            let report = analyze_phase03(&files, &[]);
            assert_eq!(
                report.issues.is_empty(),
                issue_kind.is_none(),
                "{:?}",
                report.issues
            );
            let hooks: BTreeSet<_> = report
                .evidence
                .iter()
                .filter_map(|row| {
                    (row.kind == "django_path_converter").then_some(row.full_name.as_str())
                })
                .collect();
            if let Some(kind) = issue_kind {
                assert!(
                    !hooks.contains("converters.Converter.to_python"),
                    "{hooks:?}"
                );
                assert!(!hooks.contains("converters.passthrough"), "{hooks:?}");
                assert!(
                    report.issues.iter().any(|issue| issue.kind == kind),
                    "{:?}",
                    report.issues
                );
            } else {
                assert_eq!(
                    hooks,
                    BTreeSet::from([
                        "converters.Converter",
                        "converters.Converter.to_url",
                        "converters.passthrough"
                    ])
                );
                assert!(names(&report, Category::Function).contains(&"obsolete"));
            }
        }
    }

    #[test]
    fn phase03_standalone_django_registrations_report_unknown_activation() {
        let urls = module(
            "urls.py",
            "urls",
            "from django.urls import path\ndef endpoint(request):\n    return 1\nurlpatterns = [path('', endpoint)]\n",
        );
        let signals = module(
            "signals.py",
            "signals",
            "from django.dispatch import receiver\nfrom django.db.models.signals import post_save\n@receiver(post_save)\ndef on_save(sender):\n    return 1\n",
        );
        let result = analyze_phase03(&[urls, signals], &[]);
        for path in ["urls.py", "signals.py"] {
            assert!(
                result
                    .issues
                    .iter()
                    .any(|issue| { issue.path == path && issue.kind == "unresolved_registration" }),
                "{:?}",
                result.issues
            );
        }
        assert!(
            !result
                .evidence
                .iter()
                .any(|event| matches!(event.kind, "django_registration" | "django_signal"))
        );
    }

    #[test]
    fn phase03_django_startup_requires_a_reachable_launcher() {
        for launch in ["", "\napplication = create_app()\n"] {
            let files = [
                module("settings.py", "settings", "ROOT_URLCONF = 'urls'\n"),
                module(
                    "urls.py",
                    "urls",
                    "from django.urls import path\nimport signals\ndef home(request):\n    return None\nurlpatterns = [path('', home)]\n",
                ),
                module(
                    "signals.py",
                    "signals",
                    "from django.dispatch import receiver\nfrom django.db.models.signals import post_save\n@receiver(post_save)\ndef save(sender):\n    return sender\n",
                ),
                module(
                    "server.py",
                    "server",
                    &format!(
                        "def create_app():\n    import os\n    from django.core.asgi import get_asgi_application\n    os.environ.setdefault('DJANGO_SETTINGS_MODULE', 'settings')\n    return get_asgi_application()\n{launch}"
                    ),
                ),
            ];
            let result = analyze_phase03(&files, &[]);
            if launch.is_empty() {
                for path in ["urls.py", "signals.py"] {
                    assert!(
                        result.issues.iter().any(|issue| {
                            issue.path == path && issue.kind == "unresolved_registration"
                        }),
                        "{:?}",
                        result.issues
                    );
                }
                assert!(!result.evidence.iter().any(|entry| {
                    matches!(entry.kind, "django_registration" | "django_signal")
                }));
            } else {
                assert!(result.issues.is_empty(), "{:?}", result.issues);
                for (name, kind) in [
                    ("urls.home", "django_registration"),
                    ("signals.save", "django_signal"),
                ] {
                    assert!(
                        result
                            .evidence
                            .iter()
                            .any(|entry| { entry.full_name == name && entry.kind == kind })
                    );
                }
            }
        }
    }

    #[test]
    fn phase03_fastapi_dependencies_need_an_active_route() {
        let files = [module(
            "example.py",
            "example",
            "from fastapi import FastAPI, APIRouter, Depends\napp = FastAPI()\ndormant = APIRouter()\ndef active():\n    return object()\ndef inactive():\n    return object()\n@app.get('/')\ndef endpoint(dep=Depends(active)):\n    return 1\n@dormant.get('/')\ndef parked(dep=Depends(inactive)):\n    return 1\n",
        )];
        let result = analyze_phase03(&files, &[]);
        assert!(result.issues.is_empty(), "{:?}", result.issues);
        assert_eq!(names(&result, Category::Function), ["inactive", "parked"]);
        assert!(result.evidence.iter().any(|entry| {
            entry.full_name == "example.active" && entry.kind == "fastapi_dependency"
        }));
    }

    #[test]
    fn phase03_fastapi_reevaluates_postponed_dependencies_at_the_outer_mount() {
        let files = [module(
            "web.py",
            "web",
            "from __future__ import annotations\nfrom typing import Annotated\nfrom fastapi import APIRouter, Depends, FastAPI\nrouter = APIRouter()\nparent = APIRouter()\ndef initial():\n    return 1\nselected = initial\n@router.get('/')\ndef endpoint(value: Annotated[int, Depends(selected)]):\n    return value\ndef intermediate():\n    return 2\nselected = intermediate\nparent.include_router(router)\ndef final():\n    return 3\nselected = final\napp = FastAPI()\napp.include_router(parent)\n",
        )];
        let result = analyze_phase03(&files, &[]);
        assert!(result.issues.is_empty(), "{:?}", result.issues);
        assert_eq!(
            names(&result, Category::Function),
            ["initial", "intermediate"]
        );
        assert!(
            result.evidence.iter().any(|entry| {
                entry.full_name == "web.final" && entry.kind == "fastapi_dependency"
            })
        );
    }

    #[test]
    fn phase03_reports_version_sensitive_fastapi_dependency_selection() {
        let file = module(
            "web.py",
            "web",
            "from __future__ import annotations\nfrom typing import Annotated\nfrom fastapi import APIRouter, Depends, FastAPI\nrouter = APIRouter()\ndef initial():\n    return 1\nselected = initial\n@router.get('/')\ndef endpoint(value: Annotated[int, Depends(selected)]):\n    return value\nfirst_app = FastAPI()\nfirst_app.include_router(router)\ndef replacement():\n    return 2\nselected = replacement\nsecond_app = FastAPI()\nsecond_app.include_router(router)\n",
        );
        let result = analyze_phase03(&[file], &[]);
        assert!(
            result.issues.iter().any(|issue| {
                issue.kind == "unresolved_registration"
                    && issue.message.contains("framework versions")
            }),
            "{:?}",
            result.issues
        );
        assert!(!result.evidence.iter().any(|entry| {
            entry.full_name == "web.initial" && entry.kind == "fastapi_dependency"
        }));
    }

    #[test]
    fn selected_fastapi_router_modes_use_the_matching_dependency_binding() {
        let source = "from __future__ import annotations\nfrom typing import Annotated\nfrom fastapi import APIRouter, Depends, FastAPI\nrouter = APIRouter()\ndef original(): return 1\nselected = original\n@router.get('/')\ndef endpoint(value: Annotated[int, Depends(selected)]): return value\napp = FastAPI()\napp.include_router(router)\ndef current(): return 2\nselected = current\n";
        for (mode, expected) in [
            (FastapiRouterMode::Snapshot, "web.original"),
            (FastapiRouterMode::Live, "web.current"),
        ] {
            let mut file = module("web.py", "web", source);
            file.fastapi_router_mode = mode;
            let result = analyze_phase03(&[file], &[]);
            assert!(result.issues.is_empty(), "{:?}", result.issues);
            let dependencies: Vec<_> = result
                .evidence
                .iter()
                .filter(|entry| entry.kind == "fastapi_dependency")
                .map(|entry| entry.full_name.as_str())
                .collect();
            assert_eq!(dependencies, [expected]);
        }
    }

    #[test]
    fn phase03_fastapi_mixed_registration_preserves_evidence_and_incomplete_coverage() {
        let files = [module(
            "example.py",
            "example",
            "from fastapi import Depends, FastAPI\nfrom external import opaque\ndef known():\n    return 1\napp = FastAPI(dependencies=[Depends(known), Depends(opaque)])\n@app.get('/')\ndef index():\n    return 2\n",
        )];
        let result = analyze_phase03(&files, &[]);
        assert!(
            result
                .issues
                .iter()
                .any(|issue| issue.kind == "unresolved_registration")
        );
        assert!(result.evidence.iter().any(|entry| {
            entry.full_name == "example.known" && entry.kind == "fastapi_dependency"
        }));
    }

    #[test]
    fn phase03_models_tasks_listeners_and_pydantic_lifecycle() {
        let files = [
            module(
                "tasks.py",
                "tasks",
                "from celery import Celery\napp = Celery('jobs')\n@app.task\ndef queued():\n    return task_helper()\ndef task_helper():\n    return 1\ndef dormant_task():\n    return 0\n",
            ),
            module(
                "listeners.py",
                "listeners",
                "from sqlalchemy import event\nclass Engine: pass\n@event.listens_for(Engine, 'connect')\ndef connected(dbapi, record):\n    return None\ndef disconnected(dbapi, record):\n    return None\n",
            ),
            module(
                "models.py",
                "models",
                "from pydantic import BaseModel\nclass User(BaseModel):\n    name: str\n    def model_post_init(self, context):\n        return None\nclass Draft(BaseModel):\n    name: str\nUser(name='Ada')\n",
            ),
        ];
        let result = analyze_phase03(&files, &[]);
        assert!(result.issues.is_empty(), "{:?}", result.issues);
        assert_eq!(
            names(&result, Category::Function),
            ["disconnected", "dormant_task"]
        );
        assert_eq!(names(&result, Category::Class), ["Draft"]);
        for target in [
            "tasks.queued",
            "listeners.connected",
            "models.User.model_post_init",
        ] {
            assert!(
                result
                    .evidence
                    .iter()
                    .any(|entry| entry.full_name == target),
                "missing framework evidence for {target}: {:?}",
                result.evidence
            );
        }
    }

    #[test]
    fn phase03_discovers_unittest_and_pytest_plugin_hooks() {
        let files = [
            module(
                "test_case.py",
                "test_case",
                "import unittest\npytest_plugins = ['plugin']\nclass Example(unittest.TestCase):\n    @classmethod\n    def setUpClass(cls):\n        return None\n    def test_value(self):\n        assert True\ndef setUpModule():\n    return None\ndef helper():\n    return None\n",
            ),
            module(
                "plugin.py",
                "plugin",
                "import pytest\ndef pytest_addhooks(pluginmanager):\n    return pluginmanager\ndef serialize(value):\n    return str(value)\n",
            ),
        ];
        let result = analyze_phase03(&files, &[]);
        assert!(result.issues.is_empty(), "{:?}", result.issues);
        assert_eq!(names(&result, Category::Function), ["helper", "serialize"]);
        for target in [
            "test_case.Example.setUpClass",
            "test_case.Example.test_value",
            "test_case.setUpModule",
            "plugin.pytest_addhooks",
        ] {
            assert!(
                result
                    .evidence
                    .iter()
                    .any(|entry| entry.full_name == target),
                "missing discovery evidence for {target}: {:?}",
                result.evidence
            );
        }
    }
}
