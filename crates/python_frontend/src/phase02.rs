//! Serializable source facts for the bounded Python 3.11 compatibility profile.

use std::collections::{BTreeMap, BTreeSet};

use ruff_python_ast::{
    Alias, BoolOp, CmpOp, ElifElseClause, Expr, InterpolatedStringElement, Mod, Operator, Stmt,
    UnaryOp,
};
use ruff_python_ast::{
    token::TokenKind,
    visitor::{self, Visitor},
};
use ruff_text_size::Ranged;
use serde::{Deserialize, Serialize};

use crate::{FrontendError, Lines, Location, parse_source};

// Limit recursive walks and the depth of serialized facts.
const MAX_NESTING: usize = 32;

fn lambda_parameter(lambda: &ruff_python_ast::ExprLambda) -> Option<&str> {
    let parameters = lambda.parameters.as_ref()?;
    if parameters.len() != 1 || !parameters.kwonlyargs.is_empty() {
        return None;
    }
    let parameter = parameters.iter_non_variadic_params().next()?;
    parameter
        .default
        .is_none()
        .then(|| parameter.name().as_str())
}

fn signed_integer(expression: &Expr) -> Option<i64> {
    let (number, negative) = match expression {
        Expr::UnaryOp(unary) if matches!(unary.op, UnaryOp::UAdd | UnaryOp::USub) => {
            (unary.operand.as_ref(), unary.op == UnaryOp::USub)
        }
        number => (number, false),
    };
    let Expr::NumberLiteral(number) = number else {
        return None;
    };
    let ruff_python_ast::Number::Int(integer) = &number.value else {
        return None;
    };
    let value = i64::try_from(integer.as_u64()?).ok()?;
    Some(if negative { -value } else { value })
}

fn backend_list_name(expression: &Expr) -> bool {
    matches!(expression, Expr::Name(name) if name.id.as_str() == "AUTHENTICATION_BACKENDS")
}

fn django_settings_global_call<'a>(statement: &'a Stmt, name: &str) -> Option<&'a Expr> {
    if !matches!(
        name,
        "AUTHENTICATION_BACKENDS" | "LOGGING" | "STORAGES" | "TEMPLATES"
    ) {
        return None;
    }
    let Stmt::Expr(statement) = statement else {
        return None;
    };
    let Expr::Call(call) = statement.value.as_ref() else {
        return None;
    };
    matches!(call.func.as_ref(), Expr::Attribute(member)
        if matches!(member.value.as_ref(), Expr::Name(receiver) if receiver.id.as_str() == name))
    .then_some(statement.value.as_ref())
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ModuleFacts {
    pub body: Vec<Statement>,
    pub empty: bool,
    pub inline_ignores: Vec<InlineIgnore>,
    pub postponed_annotations: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct InlineIgnore {
    pub line: usize,
    /// An empty list means the whole line is ignored.
    pub rules: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Statement {
    pub at: Location,
    pub kind: StatementKind,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum StatementKind {
    Define(Definition),
    Import(Import),
    ProviderWildcardImport(Vec<Import>),
    Assign {
        names: Vec<Name>,
        value: Option<Expression>,
        annotation: Option<Expression>,
    },
    AssignDjangoSettings {
        environment: Expression,
        value: Expression,
    },
    SetGlobalName {
        name: Name,
        value: Expression,
    },
    DjangoSettingsGlobalCall(Expression),
    EditDjangoBackendList {
        receiver: Expression,
        edit: DjangoBackendListEdit,
    },
    DiscardedComprehension(Expression),
    DeleteName(Name),
    LiteralRouteLoop {
        path: Name,
        last: Expression,
        registration: Expression,
    },
    WriteAttribute {
        receiver: Expression,
        member: String,
        value: Option<Expression>,
    },
    WriteModuleEntry {
        mapping: Expression,
        module: String,
        value: Option<Expression>,
    },
    If {
        test: Expression,
        body: Vec<Statement>,
        otherwise: Vec<Statement>,
    },
    With {
        items: Vec<(Expression, Option<Name>)>,
        body: Vec<Statement>,
    },
    Return(Option<Expression>),
    Raise(Option<Expression>),
    Assert(Expression),
    Evaluate(Expression),
}

#[derive(Debug, Serialize, Deserialize)]
pub enum DjangoBackendListEdit {
    Extend(Expression),
    Repeat(i64),
    Item {
        index: i64,
        value: Option<Expression>,
    },
}

impl DjangoBackendListEdit {
    pub fn value(&self) -> Option<&Expression> {
        match self {
            Self::Extend(value) => Some(value),
            Self::Item { value, .. } => value.as_ref(),
            Self::Repeat(_) => None,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Definition {
    pub name: Name,
    pub kind: DefinitionKind,
    pub table_keyword: Option<bool>,
    pub is_generator: bool,
    pub body: Vec<Statement>,
    pub body_coverage_lines: Vec<usize>,
    pub decorators: Vec<Expression>,
    pub parameters: Vec<Name>,
    pub positional_only_parameter_count: usize,
    pub positional_parameter_count: usize,
    pub required_positional_parameter_count: usize,
    pub required_keyword_only_parameters: Vec<usize>,
    pub has_positional_parameter: bool,
    pub has_variadic_positional_parameter: bool,
    pub has_variadic_keyword_parameter: bool,
    pub simple_positional_parameters: bool,
    pub defaults: Vec<Expression>,
    pub annotations: Vec<Expression>,
    pub parameter_annotation_count: usize,
    pub bases: Vec<Expression>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum DefinitionKind {
    Function,
    Coroutine,
    Class,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Name {
    pub text: String,
    pub at: Location,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Import {
    pub local: Name,
    pub level: u32,
    pub module: String,
    pub member: Option<String>,
    pub declares_export: bool,
    pub exposes_top_level: bool,
    pub wildcard_member: bool,
}

// CPython 3.11 typing.__all__ and the documented FastAPI top-level re-exports.
fn provider_wildcard_members(module: &str) -> Option<&'static str> {
    match module {
        "fastapi" => Some(
            "APIRouter BackgroundTasks Body Cookie Depends FastAPI File Form HTTPException Header \
             Path Query Request Response Security UploadFile WebSocket WebSocketDisconnect \
             WebSocketException status",
        ),
        "typing" => Some(
            "AbstractSet Annotated Any AnyStr AsyncContextManager AsyncGenerator AsyncIterable \
             AsyncIterator Awaitable BinaryIO ByteString Callable ChainMap ClassVar Collection \
             Concatenate Container ContextManager Coroutine Counter DefaultDict Deque Dict Final \
             ForwardRef FrozenSet Generator Generic Hashable IO ItemsView Iterable Iterator KeysView \
             List Literal LiteralString Mapping MappingView Match MutableMapping MutableSequence \
             MutableSet NamedTuple Never NewType NoReturn NotRequired Optional OrderedDict ParamSpec \
             ParamSpecArgs ParamSpecKwargs Pattern Protocol Required Reversible Self Sequence Set \
             Sized SupportsAbs SupportsBytes SupportsComplex SupportsFloat SupportsIndex SupportsInt \
             SupportsRound TYPE_CHECKING Text TextIO Tuple Type TypeAlias TypeGuard TypeVar \
             TypeVarTuple TypedDict Union Unpack ValuesView assert_never assert_type cast \
             clear_overloads dataclass_transform final get_args get_origin get_overloads \
             get_type_hints is_typeddict no_type_check no_type_check_decorator overload reveal_type \
             runtime_checkable",
        ),
        _ => None,
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Expression {
    pub at: Location,
    pub kind: ExpressionKind,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum ExpressionKind {
    Name(String),
    IterationVariable(Location),
    Member(Box<Expression>, String),
    Call {
        function: Box<Expression>,
        arguments: Vec<Expression>,
        keywords: Vec<(String, Expression)>,
    },
    KeywordMappingCall {
        function: Box<Expression>,
        mapping: Box<Expression>,
    },
    RepeatedKeywordCall {
        function: Box<Expression>,
        arguments: Vec<Expression>,
    },
    Text(String),
    FormattedString(Vec<Expression>),
    Bool(bool),
    None,
    Scalar(Option<i64>),
    ConstantLambda,
    NullaryConstantLambda,
    IdentityLambda,
    IdentityDecoratorLambda,
    DeferredLambda(Box<Expression>),
    EmptyListComprehension,
    Sequence {
        items: Vec<Expression>,
        list: bool,
    },
    Dictionary(Vec<Expression>),
    Comprehension {
        form: ComprehensionForm,
        iterators: Vec<ComprehensionIterator>,
        elements: Vec<Expression>,
    },
    Subscript {
        value: Box<Expression>,
        index: Box<Expression>,
    },
    Conditional {
        test: Box<Expression>,
        body: Box<Expression>,
        otherwise: Box<Expression>,
    },
    Membership {
        element: Box<Expression>,
        container: Box<Expression>,
        contains: bool,
    },
    Compare {
        left: Box<Expression>,
        right: Box<Expression>,
        equal: bool,
        identity: bool,
    },
    OrderingComparison {
        left: Box<Expression>,
        right: Box<Expression>,
    },
    Boolean {
        and: bool,
        values: Vec<Expression>,
    },
    Not(Box<Expression>),
    Await(Box<Expression>),
    Addition(Vec<Expression>),
    Divide(Vec<Expression>),
    Operands(Vec<Expression>),
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub enum ComprehensionForm {
    Generator,
    List,
    Set,
    Dictionary,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ComprehensionIterator {
    pub target: Location,
    pub iterable: Expression,
    pub filters: Vec<Expression>,
}

/// Extract Rust-owned facts and comment locations from the same parsed snapshot.
pub fn extract(source: &str) -> Result<ModuleFacts, FrontendError> {
    let (parsed, lines) = parse_source(source)?;
    let Mod::Module(syntax) = parsed.syntax() else {
        unreachable!("module mode always produces a module AST");
    };
    let mut inline_ignores = Vec::new();
    let mut block_start = None;
    for token in parsed
        .tokens()
        .iter()
        .filter(|token| token.kind() == TokenKind::Comment)
    {
        let range = token.range();
        let comment = &source[usize::from(range.start())..usize::from(range.end())];
        let line = lines.locate(range.start()).line;
        let marker = comment.trim_start_matches('#').trim().to_ascii_lowercase();
        let directive = marker.strip_prefix("skylos:").map(str::trim);
        match directive {
            Some("ignore-start") => {
                block_start.get_or_insert(line);
            }
            Some("ignore-end") => {
                let start = block_start.take().unwrap_or(line);
                append_block(&mut inline_ignores, start, line)?;
            }
            _ => {
                if let Some(rules) = ignore_rules(&marker) {
                    let before_comment = source[..usize::from(range.start())]
                        .rsplit('\n')
                        .next()
                        .unwrap_or("");
                    if before_comment.trim_start().starts_with('@') {
                        inline_ignores.push(InlineIgnore {
                            line: line + 1,
                            rules: rules.clone(),
                        });
                    }
                    inline_ignores.push(InlineIgnore { line, rules });
                }
            }
        }
    }
    if let Some(start) = block_start {
        append_block(&mut inline_ignores, start, lines.locate(source.len()).line)?;
    }
    let empty = syntax.body.is_empty()
        || (syntax.body.len() == 1
            && matches!(&syntax.body[0], Stmt::Expr(node) if node.value.is_string_literal_expr()));
    Ok(ModuleFacts {
        body: Lowering(&lines, SyntaxScope::ModuleOrClass, BTreeMap::new()).suite(&syntax.body, 0)?,
        empty,
        inline_ignores,
        postponed_annotations: syntax.body.iter().any(|statement|
            matches!(statement, Stmt::ImportFrom(node)
                if node.level == 0 && node.module.as_ref().is_some_and(|name| name.as_str() == "__future__")
                    && node.names.iter().any(|alias| alias.name.as_str() == "annotations"))
        ),
    })
}

fn append_block(
    ignores: &mut Vec<InlineIgnore>,
    start: usize,
    end: usize,
) -> Result<(), FrontendError> {
    if end.saturating_sub(start) + ignores.len() > 16_384 {
        return Err(FrontendError {
            kind: "analysis_limit",
            message: "inline ignore block exceeds the supported line budget".into(),
            location: Location {
                line: start,
                column: 1,
            },
        });
    }
    ignores.extend((start..=end).map(|line| InlineIgnore {
        line,
        rules: Vec::new(),
    }));
    Ok(())
}

fn ignore_rules(lower: &str) -> Option<Vec<String>> {
    if lower == "noqa: skylos" || lower.starts_with("pragma: no skylos") {
        return Some(Vec::new());
    }
    let rest = if let Some(rest) = lower.strip_prefix("noqa") {
        rest
    } else {
        lower
            .strip_prefix("skylos:")?
            .trim_start()
            .strip_prefix("ignore")?
    };
    let rest = rest.trim();
    if rest.is_empty() {
        return Some(Vec::new());
    }
    let rules: Vec<_> = rest
        .strip_prefix([':', '['])?
        .split(|c: char| c.is_whitespace() || matches!(c, ',' | ']'))
        .filter(|rule| rule.starts_with("sky-"))
        .map(str::to_ascii_uppercase)
        .collect();
    (!rules.is_empty()).then_some(rules)
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum SyntaxScope {
    ModuleOrClass,
    Function,
    AsyncFunction,
}

struct Lowering<'a>(&'a Lines, SyntaxScope, BTreeMap<String, Location>);

struct BodyCoverage<'a> {
    source: &'a Lines,
    lines: BTreeSet<usize>,
    visit_suite: bool,
    is_generator: bool,
}

impl BodyCoverage<'_> {
    fn record(&mut self, node: &impl Ranged) {
        self.lines.insert(self.source.locate(node.start()).line);
    }
}

impl<'ast> Visitor<'ast> for BodyCoverage<'_> {
    fn visit_stmt(&mut self, statement: &'ast Stmt) {
        self.record(statement);
        // Nested declarations can execute header expressions in this scope.
        // Their suites belong to a separate scope.
        let visit_suite = !matches!(statement, Stmt::FunctionDef(_) | Stmt::ClassDef(_));
        let previous = std::mem::replace(&mut self.visit_suite, visit_suite);
        visitor::walk_stmt(self, statement);
        self.visit_suite = previous;
    }

    fn visit_body(&mut self, statements: &'ast [Stmt]) {
        if self.visit_suite {
            visitor::walk_body(self, statements);
        }
    }

    fn visit_elif_else_clause(&mut self, clause: &'ast ElifElseClause) {
        if clause.test.is_some() {
            self.record(clause);
        }
        visitor::walk_elif_else_clause(self, clause);
    }

    fn visit_expr(&mut self, expression: &'ast Expr) {
        self.record(expression);
        if matches!(expression, Expr::Yield(_) | Expr::YieldFrom(_)) {
            self.is_generator = true;
        }
        if let Expr::Attribute(attribute) = expression {
            self.record(&attribute.attr);
        }
        visitor::walk_expr(self, expression);
    }
}

impl Lowering<'_> {
    fn repeated_keyword_arguments(call: &ruff_python_ast::ExprCall) -> Option<Vec<&Expr>> {
        let (expansion, explicit) = call.arguments.keywords.split_last()?;
        if expansion.arg.is_some() || explicit.is_empty() {
            return None;
        }
        let names: BTreeSet<_> = explicit
            .iter()
            .map(|keyword| keyword.arg.as_ref().map(|name| name.as_str()))
            .collect::<Option<_>>()?;
        if names.len() != explicit.len() {
            return None;
        }
        let Expr::Dict(mapping) = &expansion.value else {
            return None;
        };
        let mut keys = BTreeSet::new();
        let mut repeated = false;
        for item in &mapping.items {
            let Some(Expr::StringLiteral(key)) = item.key.as_ref() else {
                return None;
            };
            let name = key.value.to_str();
            if !keys.insert(name) {
                return None;
            }
            repeated |= names.contains(name);
        }
        repeated.then(|| {
            call.arguments
                .args
                .iter()
                .chain(explicit.iter().map(|keyword| &keyword.value))
                .chain(mapping.items.iter().map(|item| &item.value))
                .collect()
        })
    }

    fn check(&self, node: &impl Ranged, depth: usize) -> Result<(), FrontendError> {
        if depth >= MAX_NESTING {
            return Err(self
                .0
                .error("analysis_limit", node, "Python fact nesting exceeded"));
        }
        Ok(())
    }

    fn binding_name(&self, text: &str, node: &impl Ranged) -> Result<Name, FrontendError> {
        if text == "__debug__" {
            return Err(self
                .0
                .error("parse_error", node, "cannot assign to __debug__"));
        }
        Ok(Name {
            text: text.into(),
            at: self.0.locate(node.start()),
        })
    }

    fn target(&self, target: &Expr) -> Result<Name, FrontendError> {
        match target {
            Expr::Name(name) => self.binding_name(name.id.as_str(), name),
            _ => Err(self
                .0
                .unsupported(target, "assignment target must be a simple name")),
        }
    }

    fn write_target(
        &self,
        target: &Expr,
        value: Option<Expression>,
        depth: usize,
    ) -> Result<StatementKind, FrontendError> {
        match target {
            Expr::Attribute(node) => Ok(StatementKind::WriteAttribute {
                receiver: self.expression(&node.value, depth)?,
                member: node.attr.to_string(),
                value,
            }),
            Expr::Subscript(node) => {
                if depth == 1
                    && backend_list_name(&node.value)
                    && let Some(index) = signed_integer(&node.slice)
                {
                    return Ok(StatementKind::EditDjangoBackendList {
                        receiver: self.expression(&node.value, depth)?,
                        edit: DjangoBackendListEdit::Item { index, value },
                    });
                }
                let Expr::StringLiteral(module) = node.slice.as_ref() else {
                    return Err(self.0.unsupported(target, "mutation key must be literal"));
                };
                Ok(StatementKind::WriteModuleEntry {
                    mapping: self.expression(&node.value, depth)?,
                    module: module.value.to_str().into(),
                    value,
                })
            }
            _ => Err(self
                .0
                .unsupported(target, "assignment target must be a simple name")),
        }
    }

    fn expressions(
        &self,
        expressions: &[Expr],
        depth: usize,
    ) -> Result<Vec<Expression>, FrontendError> {
        expressions
            .iter()
            .map(|expr| self.expression(expr, depth))
            .collect()
    }

    fn imports(
        &self,
        output: &mut Vec<Statement>,
        names: &[Alias],
        module: &str,
        level: u32,
        from: bool,
    ) -> Result<(), FrontendError> {
        for alias in names {
            let name = alias.name.as_str();
            if name == "*" {
                if from
                    && level == 0
                    && let Some(members) = provider_wildcard_members(module)
                {
                    let at = self.0.locate(alias.start());
                    let imports = members
                        .split_ascii_whitespace()
                        .map(|member| Import {
                            local: Name {
                                text: member.into(),
                                at,
                            },
                            level: 0,
                            module: module.into(),
                            member: Some(member.into()),
                            declares_export: false,
                            exposes_top_level: false,
                            wildcard_member: true,
                        })
                        .collect();
                    output.push(Statement {
                        at,
                        kind: StatementKind::ProviderWildcardImport(imports),
                    });
                    continue;
                }
                return Err(self
                    .0
                    .unsupported(alias, "star imports cannot resolve finite bindings"));
            }
            let identifier = alias.asname.as_ref().unwrap_or(&alias.name);
            let (local_name, exposes_top_level) = if let Some((root, _)) = name.split_once('.')
                && !from
                && alias.asname.is_none()
            {
                (root, true)
            } else {
                (identifier.as_str(), false)
            };
            let local = self.binding_name(local_name, identifier)?;
            output.push(Statement {
                at: self.0.locate(alias.start()),
                kind: StatementKind::Import(Import {
                    local,
                    level,
                    module: if from { module.into() } else { name.into() },
                    member: from.then(|| name.into()),
                    declares_export: from
                        && alias
                            .asname
                            .as_ref()
                            .is_some_and(|alias| alias.as_str() == name),
                    exposes_top_level,
                    wildcard_member: false,
                }),
            });
        }
        Ok(())
    }

    fn suite(&self, statements: &[Stmt], depth: usize) -> Result<Vec<Statement>, FrontendError> {
        let mut result = Vec::new();
        let mut allows_future_import = depth == 0;
        for (index, statement) in statements.iter().enumerate() {
            self.check(statement, depth)?;
            if let Stmt::ImportFrom(node) = statement
                && !node.is_lazy
                && node.level == 0
                && node
                    .module
                    .as_ref()
                    .is_some_and(|module| module.as_str() == "__future__")
            {
                if !allows_future_import {
                    return Err(self.0.error(
                        "parse_error",
                        node,
                        "from __future__ imports must occur at the beginning of the file",
                    ));
                }
                if node
                    .names
                    .iter()
                    .all(|alias| alias.name.as_str() == "annotations" && alias.asname.is_none())
                {
                    continue;
                }
                return Err(self.0.unsupported(node, "unsupported future directive"));
            }
            // Only a leading module docstring can precede future imports.
            allows_future_import &= index == 0
                && matches!(statement, Stmt::Expr(node) if node.value.is_string_literal_expr());

            let next = depth + 1;
            let expression = |expr: &Expr| self.expression(expr, next);
            let kind = match statement {
                Stmt::Pass(_) => continue,
                Stmt::Import(node) if !node.is_lazy => {
                    self.imports(&mut result, &node.names, "", 0, false)?;
                    continue;
                }
                Stmt::ImportFrom(node) if !node.is_lazy => {
                    let module = node.module.as_ref().map_or("", |module| module.as_str());
                    self.imports(&mut result, &node.names, module, node.level, true)?;
                    continue;
                }
                Stmt::FunctionDef(node) => {
                    let mut annotations = Vec::new();
                    let mut parameters = Vec::new();
                    let mut parameter_names = BTreeSet::new();
                    for parameter in node.parameters.iter() {
                        let name = parameter.name();
                        if !parameter_names.insert(name.as_str()) {
                            return Err(self.0.error(
                                "parse_error",
                                name,
                                "duplicate parameter name",
                            ));
                        }
                        parameters.push(self.binding_name(name.as_str(), name)?);
                        if let Some(annotation) = parameter.annotation() {
                            annotations.push(self.annotation(annotation, next)?);
                        }
                    }
                    let parameter_annotation_count = annotations.len();
                    if let Some(returns) = &node.returns {
                        annotations.push(self.annotation(returns, next)?);
                    }
                    let name = self.binding_name(node.name.as_str(), &node.name)?;
                    let is_async = node.is_async;
                    let lowering = Lowering(
                        self.0,
                        if is_async {
                            SyntaxScope::AsyncFunction
                        } else {
                            SyntaxScope::Function
                        },
                        BTreeMap::new(),
                    );
                    let body = if !is_async
                        && parameters.is_empty()
                        && node.decorator_list.is_empty()
                        && let [Stmt::Global(declare), Stmt::Assign(assign)] = node.body.as_slice()
                        && let [name] = declare.names.as_slice()
                        && let [Expr::Name(target)] = assign.targets.as_slice()
                        && target.id.as_str() == name.as_str()
                        && matches!(assign.value.as_ref(), Expr::Name(_))
                    {
                        vec![Statement {
                            at: self.0.locate(declare.start()),
                            kind: StatementKind::SetGlobalName {
                                name: self.binding_name(name.as_str(), target)?,
                                value: lowering.expression(&assign.value, next + 1)?,
                            },
                        }]
                    } else if depth == 0
                        && !is_async
                        && let [Stmt::Global(declare), statement] = node.body.as_slice()
                        && let [global] = declare.names.as_slice()
                        && !parameter_names.contains(global.as_str())
                        && let Some(call) = django_settings_global_call(statement, global.as_str())
                    {
                        // This top-level function only reads the declared
                        // name. There are no enclosing function locals.
                        vec![Statement {
                            at: self.0.locate(call.start()),
                            kind: StatementKind::DjangoSettingsGlobalCall(
                                lowering.expression(call, next + 1)?,
                            ),
                        }]
                    } else {
                        lowering.suite(&node.body, next)?
                    };
                    let mut coverage = BodyCoverage {
                        source: self.0,
                        lines: BTreeSet::new(),
                        visit_suite: true,
                        is_generator: false,
                    };
                    coverage.visit_body(&node.body);
                    StatementKind::Define(Definition {
                        name,
                        kind: if is_async {
                            DefinitionKind::Coroutine
                        } else {
                            DefinitionKind::Function
                        },
                        table_keyword: None,
                        is_generator: coverage.is_generator,
                        body,
                        body_coverage_lines: coverage.lines.into_iter().collect(),
                        decorators: node
                            .decorator_list
                            .iter()
                            .map(|item| expression(&item.expression))
                            .collect::<Result<_, _>>()?,
                        parameters,
                        positional_only_parameter_count: node.parameters.posonlyargs.len(),
                        positional_parameter_count: node.parameters.posonlyargs.len()
                            + node.parameters.args.len(),
                        required_positional_parameter_count: node
                            .parameters
                            .posonlyargs
                            .iter()
                            .chain(&node.parameters.args)
                            .filter(|parameter| parameter.default.is_none())
                            .count(),
                        required_keyword_only_parameters: node
                            .parameters
                            .kwonlyargs
                            .iter()
                            .enumerate()
                            .filter_map(|(index, parameter)| {
                                parameter.default.is_none().then_some(index)
                            })
                            .collect(),
                        has_positional_parameter: !(node.parameters.posonlyargs.is_empty()
                            && node.parameters.args.is_empty()),
                        has_variadic_positional_parameter: node.parameters.vararg.is_some(),
                        has_variadic_keyword_parameter: node.parameters.kwarg.is_some(),
                        simple_positional_parameters: (node.parameters.posonlyargs.is_empty()
                            && node.parameters.kwonlyargs.is_empty()
                            && node.parameters.vararg.is_none()
                            && node.parameters.kwarg.is_none()),
                        defaults: node
                            .parameters
                            .iter_non_variadic_params()
                            .filter_map(|parameter| parameter.default.as_deref())
                            .map(expression)
                            .collect::<Result<_, _>>()?,
                        annotations,
                        parameter_annotation_count,
                        bases: Vec::new(),
                    })
                }
                Stmt::ClassDef(node)
                    if node.arguments.as_ref().is_none_or(|args| {
                        args.keywords.is_empty()
                            || matches!(args.keywords.as_slice(), [keyword]
                                if keyword.arg.as_ref().is_some_and(|name| name.as_str() == "table")
                                    && matches!(keyword.value, Expr::BooleanLiteral(_)))
                    }) =>
                {
                    StatementKind::Define(Definition {
                        name: self.binding_name(node.name.as_str(), &node.name)?,
                        kind: DefinitionKind::Class,
                        table_keyword: node.arguments.as_ref().and_then(|args| {
                            if let [keyword] = args.keywords.as_slice()
                                && let Expr::BooleanLiteral(value) = &keyword.value
                            {
                                Some(value.value)
                            } else {
                                None
                            }
                        }),
                        is_generator: false,
                        body: Lowering(self.0, SyntaxScope::ModuleOrClass, BTreeMap::new())
                            .suite(&node.body, next)?,
                        body_coverage_lines: Vec::new(),
                        decorators: node
                            .decorator_list
                            .iter()
                            .map(|item| expression(&item.expression))
                            .collect::<Result<_, _>>()?,
                        parameters: Vec::new(),
                        positional_only_parameter_count: 0,
                        positional_parameter_count: 0,
                        required_positional_parameter_count: 0,
                        required_keyword_only_parameters: Vec::new(),
                        has_positional_parameter: false,
                        has_variadic_positional_parameter: false,
                        has_variadic_keyword_parameter: false,
                        simple_positional_parameters: false,
                        defaults: Vec::new(),
                        annotations: Vec::new(),
                        parameter_annotation_count: 0,
                        bases: node
                            .arguments
                            .as_ref()
                            .map(|args| self.expressions(&args.args, next))
                            .transpose()?
                            .unwrap_or_default(),
                    })
                }
                Stmt::Assign(node) => {
                    if let [Expr::Subscript(target)] = node.targets.as_slice()
                        && matches!(target.slice.as_ref(), Expr::StringLiteral(key)
                            if key.value.to_str() == "DJANGO_SETTINGS_MODULE")
                    {
                        StatementKind::AssignDjangoSettings {
                            environment: expression(&target.value)?,
                            value: expression(&node.value)?,
                        }
                    } else if let [target] = node.targets.as_slice()
                        && matches!(target, Expr::Attribute(_) | Expr::Subscript(_))
                    {
                        self.write_target(target, Some(expression(&node.value)?), next)?
                    } else {
                        StatementKind::Assign {
                            names: node
                                .targets
                                .iter()
                                .map(|target| self.target(target))
                                .collect::<Result<_, _>>()?,
                            value: Some(expression(&node.value)?),
                            annotation: None,
                        }
                    }
                }
                Stmt::AnnAssign(node) => StatementKind::Assign {
                    names: vec![self.target(&node.target)?],
                    value: node.value.as_deref().map(expression).transpose()?,
                    annotation: Some(self.annotation(&node.annotation, next)?),
                },
                Stmt::AugAssign(node) if depth == 0 && backend_list_name(&node.target) => {
                    let edit = match node.op {
                        Operator::Add => DjangoBackendListEdit::Extend(expression(&node.value)?),
                        Operator::Mult => DjangoBackendListEdit::Repeat(
                            signed_integer(&node.value).ok_or_else(|| {
                                self.0.unsupported(
                                    node.value.as_ref(),
                                    "list repetition requires a literal integer",
                                )
                            })?,
                        ),
                        _ => {
                            return Err(self
                                .0
                                .unsupported(statement, "unsupported list assignment"));
                        }
                    };
                    StatementKind::EditDjangoBackendList {
                        receiver: expression(&node.target)?,
                        edit,
                    }
                }
                Stmt::Delete(node) if node.targets.len() == 1 => match &node.targets[0] {
                    Expr::Name(name) => {
                        StatementKind::DeleteName(self.binding_name(name.id.as_str(), name)?)
                    }
                    target => self.write_target(target, None, next)?,
                },
                Stmt::If(node) => {
                    self.check(node, depth.saturating_add(node.elif_else_clauses.len()))?;
                    let mut otherwise = Vec::new();
                    for clause in node.elif_else_clauses.iter().rev() {
                        let body = self.suite(&clause.body, next)?;
                        otherwise = match &clause.test {
                            Some(test) => vec![Statement {
                                at: self.0.locate(clause.start()),
                                kind: StatementKind::If {
                                    test: expression(test)?,
                                    body,
                                    otherwise,
                                },
                            }],
                            None => body,
                        };
                    }
                    StatementKind::If {
                        test: expression(&node.test)?,
                        body: self.suite(&node.body, next)?,
                        otherwise,
                    }
                }
                Stmt::For(node) => {
                    let Some((last, registration)) = literal_route_loop(node) else {
                        return Err(self
                            .0
                            .unsupported(node, "statement is outside the compatibility subset"));
                    };
                    StatementKind::LiteralRouteLoop {
                        path: self.target(&node.target)?,
                        last: expression(last)?,
                        registration: expression(registration)?,
                    }
                }
                Stmt::With(node) if !node.is_async => StatementKind::With {
                    items: node
                        .items
                        .iter()
                        .map(|item| {
                            Ok((
                                expression(&item.context_expr)?,
                                item.optional_vars
                                    .as_deref()
                                    .map(|target| self.target(target))
                                    .transpose()?,
                            ))
                        })
                        .collect::<Result<_, FrontendError>>()?,
                    body: self.suite(&node.body, next)?,
                },
                Stmt::Return(node) => {
                    if self.1 == SyntaxScope::ModuleOrClass {
                        return Err(self
                            .0
                            .error("parse_error", node, "return outside a function"));
                    }
                    StatementKind::Return(node.value.as_deref().map(expression).transpose()?)
                }
                Stmt::Raise(node) if node.cause.is_none() => {
                    StatementKind::Raise(node.exc.as_deref().map(expression).transpose()?)
                }
                Stmt::Assert(node) => {
                    if node
                        .msg
                        .as_deref()
                        .is_some_and(|message| !message.is_string_literal_expr())
                    {
                        return Err(self.0.unsupported(
                            node,
                            "dynamic assertion messages are outside the compatibility subset",
                        ));
                    }
                    StatementKind::Assert(expression(&node.test)?)
                }
                Stmt::Expr(node) => {
                    if let Some(iterable) = discarded_comprehension_iterable(&node.value) {
                        StatementKind::DiscardedComprehension(expression(iterable)?)
                    } else if let Expr::Yield(yielded) = &*node.value
                        && self.1 != SyntaxScope::ModuleOrClass
                    {
                        let Some(value) = &yielded.value else {
                            continue;
                        };
                        StatementKind::Evaluate(expression(value)?)
                    } else if let Expr::YieldFrom(yielded) = &*node.value
                        && self.1 == SyntaxScope::Function
                        && match &*yielded.value {
                            Expr::List(list) => list.elts.is_empty(),
                            Expr::Tuple(tuple) => tuple.elts.is_empty(),
                            _ => false,
                        }
                    {
                        // Delegation to an empty built-in literal has no
                        // effects. The function remains a generator.
                        continue;
                    } else {
                        StatementKind::Evaluate(expression(&node.value)?)
                    }
                }
                _ => {
                    return Err(self
                        .0
                        .unsupported(statement, "statement is outside the compatibility subset"));
                }
            };
            result.push(Statement {
                at: self.0.locate(statement.start()),
                kind,
            });
        }
        Ok(result)
    }

    fn annotation(&self, expression: &Expr, depth: usize) -> Result<Expression, FrontendError> {
        self.check(expression, depth)?;
        let descend = |node: &Expr| self.annotation(node, depth + 1);
        let references = match expression {
            Expr::Subscript(node) => vec![descend(&node.value)?, descend(&node.slice)?],
            Expr::BinOp(node) if node.op == ruff_python_ast::Operator::BitOr => {
                vec![descend(&node.left)?, descend(&node.right)?]
            }
            Expr::Tuple(node) => node.elts.iter().map(descend).collect::<Result<_, _>>()?,
            Expr::List(node) => node.elts.iter().map(descend).collect::<Result<_, _>>()?,
            _ => return self.expression(expression, depth),
        };
        Ok(Expression {
            at: self.0.locate(expression.start()),
            kind: ExpressionKind::Operands(references),
        })
    }

    fn expression(&self, expression: &Expr, depth: usize) -> Result<Expression, FrontendError> {
        self.check(expression, depth)?;
        let next = depth + 1;
        let lower = |expr: &Expr| self.expression(expr, next);
        let kind = match expression {
            Expr::Name(node) => self.2.get(node.id.as_str()).map_or_else(
                || ExpressionKind::Name(node.id.to_string()),
                |&target| ExpressionKind::IterationVariable(target),
            ),
            Expr::Attribute(node) => {
                ExpressionKind::Member(Box::new(lower(&node.value)?), node.attr.to_string())
            }
            Expr::Call(node)
                if node.arguments.args.is_empty()
                    && matches!(node.arguments.keywords.as_slice(), [keyword] if keyword.arg.is_none()) =>
            {
                ExpressionKind::KeywordMappingCall {
                    function: Box::new(lower(&node.func)?),
                    mapping: Box::new(lower(&node.arguments.keywords[0].value)?),
                }
            }
            Expr::Call(node) if let Some(arguments) = Self::repeated_keyword_arguments(node) => {
                ExpressionKind::RepeatedKeywordCall {
                    function: Box::new(lower(&node.func)?),
                    arguments: arguments.into_iter().map(lower).collect::<Result<_, _>>()?,
                }
            }
            Expr::Call(node) => {
                let mut keyword_names = BTreeSet::new();
                ExpressionKind::Call {
                    function: Box::new(lower(&node.func)?),
                    arguments: self.expressions(&node.arguments.args, next)?,
                    keywords: node
                        .arguments
                        .keywords
                        .iter()
                        .map(|keyword| {
                            let name = keyword.arg.as_ref().ok_or_else(|| {
                                self.0.unsupported(keyword, "unbounded keyword expansion")
                            })?;
                            if !keyword_names.insert(name.as_str()) {
                                return Err(self.0.error(
                                    "parse_error",
                                    keyword,
                                    &format!("keyword argument repeated: {name}"),
                                ));
                            }
                            Ok((name.to_string(), lower(&keyword.value)?))
                        })
                        .collect::<Result<_, FrontendError>>()?,
                }
            }
            Expr::StringLiteral(node) => ExpressionKind::Text(node.value.to_str().into()),
            Expr::FString(node) => {
                let mut expressions = Vec::new();
                for part in node.value.elements() {
                    if let InterpolatedStringElement::Interpolation(part) = part {
                        if !part.conversion.is_none() || part.format_spec.is_some() || part.debug_text.is_some() {
                            return Err(self.0.unsupported(part, "formatted string has an unbounded conversion or format specification"));
                        }
                        expressions.push(lower(&part.expression)?);
                    }
                }
                ExpressionKind::FormattedString(expressions)
            }
            Expr::BooleanLiteral(node) => ExpressionKind::Bool(node.value),
            Expr::Lambda(node)
                if lambda_parameter(node).is_some()
                    && matches!(node.body.as_ref(), Expr::Lambda(inner)
                        if lambda_parameter(inner).is_some_and(|parameter|
                            matches!(inner.body.as_ref(), Expr::Name(name) if name.id.as_str() == parameter))) =>
            {
                ExpressionKind::IdentityDecoratorLambda
            }
            Expr::Lambda(node)
                if lambda_parameter(node).is_some_and(|parameter|
                    matches!(node.body.as_ref(), Expr::Name(name) if name.id.as_str() == parameter)) =>
            {
                ExpressionKind::IdentityLambda
            }
            Expr::Lambda(node)
                if matches!(node.body.as_ref(), Expr::NoneLiteral(_))
                    && node.parameters.as_ref().is_none_or(|parameters| parameters.is_empty()) =>
            {
                ExpressionKind::NullaryConstantLambda
            }
            Expr::Lambda(node)
                if matches!(node.body.as_ref(), Expr::NoneLiteral(_))
                    && node.parameters.as_ref().is_none_or(|parameters| {
                        parameters
                            .iter_non_variadic_params()
                            .all(|parameter| parameter.default.is_none())
                    }) =>
            {
                ExpressionKind::ConstantLambda
            }
            Expr::Lambda(node) if node.parameters.as_ref().is_none_or(|parameters| parameters.is_empty()) => {
                ExpressionKind::DeferredLambda(Box::new(lower(&node.body)?))
            }
            Expr::NoneLiteral(_) => ExpressionKind::None,
            Expr::ListComp(node)
                if node.generators.first().is_some_and(|first| {
                    match &first.iter {
                        Expr::List(iterable) => iterable.elts.is_empty(),
                        Expr::Tuple(iterable) => iterable.elts.is_empty(),
                        _ => false,
                    }
                }) && node.generators.iter().all(|generator| !generator.is_async) =>
            {
                // A literal empty outer iterator never evaluates the body or
                // any subsequent iterator or filter.
                ExpressionKind::EmptyListComprehension
            }
            Expr::NumberLiteral(node) => ExpressionKind::Scalar(match &node.value {
                ruff_python_ast::Number::Int(value) => value.as_u64().and_then(|v| i64::try_from(v).ok()),
                _ => None,
            }),
            Expr::BytesLiteral(_) | Expr::EllipsisLiteral(_) => ExpressionKind::Scalar(None),
            Expr::Tuple(node) => ExpressionKind::Sequence { items: self.expressions(&node.elts, next)?, list: false },
            Expr::List(node) => ExpressionKind::Sequence { items: self.expressions(&node.elts, next)?, list: true },
            Expr::Dict(node) if node.items.iter().all(|item| item.key.is_some()) => {
                let values = node
                    .items
                    .iter()
                    .flat_map(|item| item.key.iter().chain(std::iter::once(&item.value)));
                ExpressionKind::Dictionary(values.map(lower).collect::<Result<_, _>>()?)
            }
            Expr::Generator(node) => self.comprehension(
                &node.generators,
                &[&node.elt],
                ComprehensionForm::Generator,
                next,
            )?,
            Expr::ListComp(node) => self.comprehension(
                &node.generators,
                &[&node.elt],
                ComprehensionForm::List,
                next,
            )?,
            Expr::SetComp(node) => {
                self.comprehension(&node.generators, &[&node.elt], ComprehensionForm::Set, next)?
            }
            Expr::DictComp(node) if let Some(key) = node.key.as_deref() => self.comprehension(
                &node.generators,
                &[key, &node.value],
                ComprehensionForm::Dictionary,
                next,
            )?,
            Expr::Subscript(node) => ExpressionKind::Subscript {
                value: Box::new(lower(&node.value)?),
                index: Box::new(lower(&node.slice)?),
            },
            Expr::If(node) => ExpressionKind::Conditional {
                test: Box::new(lower(&node.test)?),
                body: Box::new(lower(&node.body)?),
                otherwise: Box::new(lower(&node.orelse)?),
            },
            Expr::Compare(node)
                if node.ops.len() == 1 && matches!(node.ops[0], CmpOp::In | CmpOp::NotIn) =>
            {
                ExpressionKind::Membership {
                    element: Box::new(lower(&node.left)?),
                    container: Box::new(lower(&node.comparators[0])?),
                    contains: node.ops[0] == CmpOp::In,
                }
            }
            Expr::Compare(node)
                if node.ops.len() == 1
                    && matches!(
                        node.ops[0],
                        CmpOp::Eq | CmpOp::NotEq | CmpOp::Is | CmpOp::IsNot
                    ) =>
            {
                ExpressionKind::Compare {
                    left: Box::new(lower(&node.left)?),
                    right: Box::new(lower(&node.comparators[0])?),
                    equal: matches!(node.ops[0], CmpOp::Eq | CmpOp::Is),
                    identity: matches!(node.ops[0], CmpOp::Is | CmpOp::IsNot),
                }
            }
            Expr::Compare(node)
                if node.ops.len() == 1
                    && matches!(node.ops[0], CmpOp::Lt | CmpOp::LtE | CmpOp::Gt | CmpOp::GtE) =>
            {
                ExpressionKind::OrderingComparison {
                    left: Box::new(lower(&node.left)?),
                    right: Box::new(lower(&node.comparators[0])?),
                }
            }
            Expr::BoolOp(node) => ExpressionKind::Boolean {
                and: node.op == BoolOp::And,
                values: self.expressions(&node.values, next)?,
            },
            Expr::UnaryOp(node) if node.op == UnaryOp::Not => {
                ExpressionKind::Not(Box::new(lower(&node.operand)?))
            }
            Expr::UnaryOp(node) => ExpressionKind::Operands(vec![lower(&node.operand)?]),
            Expr::BinOp(node) if node.op == ruff_python_ast::Operator::Add => {
                ExpressionKind::Addition(vec![lower(&node.left)?, lower(&node.right)?])
            }
            Expr::BinOp(node)
                if node.op == Operator::Div && matches!(node.right.as_ref(), Expr::StringLiteral(_)) =>
            {
                ExpressionKind::Divide(vec![lower(&node.left)?, lower(&node.right)?])
            }
            Expr::BinOp(node) => {
                ExpressionKind::Operands(vec![lower(&node.left)?, lower(&node.right)?])
            }
            Expr::Await(node) => {
                if self.1 != SyntaxScope::AsyncFunction {
                    return Err(self.0.error(
                        "parse_error",
                        node,
                        "await outside an async function",
                    ));
                }
                ExpressionKind::Await(Box::new(lower(&node.value)?))
            }
            _ => {
                return Err(self
                    .0
                    .unsupported(expression, "expression is outside the compatibility subset"));
            }
        };
        Ok(Expression {
            at: self.0.locate(expression.start()),
            kind,
        })
    }

    fn comprehension(
        &self,
        generators: &[ruff_python_ast::Comprehension],
        elements: &[&Expr],
        form: ComprehensionForm,
        depth: usize,
    ) -> Result<ExpressionKind, FrontendError> {
        let mut local_targets = BTreeMap::new();
        let mut targets = Vec::new();
        for (index, generator) in generators.iter().enumerate() {
            let depth = depth + index;
            self.check(generator, depth)?;
            let Expr::Name(target) = &generator.target else {
                return Err(self.0.unsupported(
                    generator,
                    "comprehension iterator target is not a simple name",
                ));
            };
            if generator.is_async {
                return Err(self.0.unsupported(
                    generator,
                    "asynchronous comprehension is outside the compatibility subset",
                ));
            }
            let target = *local_targets
                .entry(target.id.to_string())
                .or_insert_with(|| self.0.locate(target.start()));
            targets.push(target);
        }
        let mut child = Self(self.0, self.1, self.2.clone());
        child.2.extend(local_targets);
        let iterators = generators
            .iter()
            .zip(targets)
            .enumerate()
            .map(|(index, (generator, target))| {
                let depth = depth + index;
                // Only the first iterable is evaluated in the enclosing scope.
                let enclosing = if index == 0 { self } else { &child };
                Ok(ComprehensionIterator {
                    target,
                    iterable: enclosing.expression(&generator.iter, depth)?,
                    filters: child.expressions(&generator.ifs, depth)?,
                })
            })
            .collect::<Result<_, FrontendError>>()?;
        Ok(ExpressionKind::Comprehension {
            form,
            iterators,
            elements: elements
                .iter()
                .map(|element| child.expression(element, depth + generators.len()))
                .collect::<Result<_, _>>()?,
        })
    }
}

fn discarded_comprehension_iterable(expression: &Expr) -> Option<&Expr> {
    let (generator, must_be_empty) = match expression {
        Expr::Generator(node) => (node.generators.first()?, false),
        Expr::ListComp(node) => (node.generators.first()?, true),
        _ => return None,
    };
    let elements = match &generator.iter {
        Expr::List(list) => &list.elts,
        Expr::Tuple(tuple) => &tuple.elts,
        _ => return None,
    };
    (!generator.is_async && (!must_be_empty || elements.is_empty())).then_some(&generator.iter)
}

fn literal_route_loop(node: &ruff_python_ast::StmtFor) -> Option<(&Expr, &Expr)> {
    if node.is_async || !node.orelse.is_empty() {
        return None;
    }
    let Expr::Name(path) = node.target.as_ref() else {
        return None;
    };
    let values = match node.iter.as_ref() {
        Expr::Tuple(tuple) => &tuple.elts,
        Expr::List(list) => &list.elts,
        _ => return None,
    };
    let [Stmt::Expr(statement)] = node.body.as_slice() else {
        return None;
    };
    let Expr::Call(call) = statement.value.as_ref() else {
        return None;
    };
    let Expr::Attribute(method) = call.func.as_ref() else {
        return None;
    };
    let [Expr::Name(argument), Expr::Name(endpoint)] = &*call.arguments.args else {
        return None;
    };
    let last = values.last()?;
    (method.attr.as_str() == "add_api_route"
        && matches!(method.value.as_ref(), Expr::Name(owner) if owner.id != path.id)
        && argument.id == path.id
        && endpoint.id != path.id
        && call.arguments.keywords.is_empty()
        && values.iter().all(|value| {
            matches!(value, Expr::StringLiteral(path)
            if path.value.to_str().starts_with('/') && !path.value.to_str().contains('{'))
        }))
    .then_some((last, statement.value.as_ref()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comment_directives_do_not_match_literal_text() {
        let facts = extract("text = '# noqa'\nfrom lib import f  # noqa: SKY-U002\n").unwrap();
        assert_eq!(facts.inline_ignores.len(), 1);
        assert_eq!(facts.inline_ignores[0].line, 2);
        assert_eq!(facts.inline_ignores[0].rules, ["SKY-U002"]);
    }

    #[test]
    fn unsupported_scope_operations_report_a_location() {
        let error = extract("def helper():\n    global name\n").unwrap_err();
        assert_eq!(error.kind, "unsupported_construct");
        assert_eq!(error.location.line, 2);
    }

    #[test]
    fn rejects_compiler_invalid_contexts_and_duplicate_parameters() {
        for (source, line) in [
            ("return 1\n", 1),
            ("async def job(): return 1\nawait job()\n", 2),
            ("def foo(a, a): return a\n", 1),
            ("async def job():\n    class Bad:\n        await job()\n", 3),
            ("async def job():\n    def bad():\n        await job()\n", 3),
        ] {
            let error = extract(source).unwrap_err();
            assert_eq!(error.kind, "parse_error", "{source}");
            assert_eq!(error.location.line, line, "{source}");
        }
        assert!(extract("async def job(x):\n    return await x()\n").is_ok());
    }

    #[test]
    fn lowers_assertions_and_rejects_dynamic_messages() {
        let facts =
            extract("def test_app(resource):\n    assert resource, 'missing resource'\n").unwrap();
        assert!(
            matches!(&facts.body[0].kind, StatementKind::Define(definition)
            if matches!(&definition.body[0].kind, StatementKind::Assert(Expression {
                kind: ExpressionKind::Name(name), ..
            }) if name == "resource"))
        );
        let error =
            extract("def test_app(resource):\n    assert resource, build_failure()\n").unwrap_err();
        assert_eq!(error.kind, "unsupported_construct");
        assert_eq!(error.location.line, 2);
    }
}
