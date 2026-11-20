//! Owned syntax and lexical binding facts for the Phase 0.1 preview.

use std::collections::BTreeSet;

use ruff_python_ast::str_prefix::StringLiteralPrefix;
use ruff_python_ast::{
    Alias, BoolOp, CmpOp, Expr, ExprCall, Number, Stmt, StmtFunctionDef, StmtIf, UnaryOp,
};
use ruff_text_size::Ranged;

use crate::{FrontendError, Lines, Location, parse, simple_name};

/// Bound recursion in the owned representation and subsequent traversals.
const MAX_NESTING: usize = 128;

#[derive(Clone, Debug)]
pub struct Module {
    pub body: Vec<Statement>,
}

#[derive(Clone, Debug)]
pub struct Statement {
    pub location: Location,
    pub kind: StatementKind,
}

#[derive(Clone, Debug)]
pub enum StatementKind {
    Function(Function),
    Import(Import),
    Assign {
        names: Vec<String>,
        value: Expression,
    },
    Return(Option<Expression>),
    If {
        test: Expression,
        body: Vec<Statement>,
        otherwise: Vec<Statement>,
    },
    Expr(Expression),
}

#[derive(Clone, Debug)]
pub struct Function {
    pub name: String,
    pub location: Location,
    pub parameters: Vec<Parameter>,
    /// All names owned by this function, including bindings on other branches.
    pub locals: BTreeSet<String>,
    /// Names read from enclosing scopes, including reads by nested functions.
    pub free_names: BTreeSet<String>,
    pub body: Vec<Statement>,
}

#[derive(Clone, Debug)]
pub struct Parameter {
    pub name: String,
    pub kind: ParameterKind,
    pub default: Option<Expression>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ParameterKind {
    PositionalOnly,
    PositionalOrKeyword,
    VariadicPositional,
    KeywordOnly,
    VariadicKeyword,
}

#[derive(Clone, Debug)]
pub struct Import {
    pub level: u32,
    /// Empty for imports of the form `from . import name`.
    pub module: String,
    pub member: Option<String>,
    pub alias: Option<String>,
}

impl Import {
    pub fn local_name(&self) -> &str {
        self.alias
            .as_deref()
            .or(self.member.as_deref())
            .unwrap_or_else(|| {
                self.module
                    .split_once('.')
                    .map_or(&self.module, |(top_level, _)| top_level)
            })
    }
}

#[derive(Clone, Debug)]
pub struct Expression {
    pub location: Location,
    pub kind: ExpressionKind,
}

#[derive(Clone, Debug)]
pub enum ExpressionKind {
    Name(String),
    Attribute {
        value: Box<Expression>,
        name: String,
    },
    Call {
        function: Box<Expression>,
        positional: Vec<Expression>,
        keywords: Vec<(String, Expression)>,
    },
    Constant(Constant),
    List(Vec<Expression>),
    Tuple(Vec<Expression>),
    Compare {
        left: Box<Expression>,
        comparisons: Vec<(Comparison, Expression)>,
    },
    Boolean {
        op: BooleanOperator,
        values: Vec<Expression>,
    },
    Not(Box<Expression>),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Constant {
    None,
    Bool(bool),
    Int(i64),
    String(String),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Comparison {
    Equal,
    NotEqual,
    Is,
    IsNot,
    Less,
    LessEqual,
    Greater,
    GreaterEqual,
    In,
    NotIn,
}

impl From<CmpOp> for Comparison {
    fn from(op: CmpOp) -> Self {
        match op {
            CmpOp::Eq => Self::Equal,
            CmpOp::NotEq => Self::NotEqual,
            CmpOp::Is => Self::Is,
            CmpOp::IsNot => Self::IsNot,
            CmpOp::Lt => Self::Less,
            CmpOp::LtE => Self::LessEqual,
            CmpOp::Gt => Self::Greater,
            CmpOp::GtE => Self::GreaterEqual,
            CmpOp::In => Self::In,
            CmpOp::NotIn => Self::NotIn,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BooleanOperator {
    And,
    Or,
}

/// Parse normalized UTF-8 Python and lower the supported Python 3.11 subset.
///
/// All selected syntax is checked, including declarations and branches that
/// might later turn out to be unreachable. Import resolution is deferred.
pub fn extract(source: &str) -> Result<Module, FrontendError> {
    let (syntax, lines) = parse(source)?;
    Ok(Module {
        body: Lowering { lines, source }.suite(&syntax.body, false, 0)?,
    })
}

struct Lowering<'a> {
    lines: Lines,
    source: &'a str,
}

impl Lowering<'_> {
    fn suite(
        &self,
        statements: &[Stmt],
        in_function: bool,
        depth: usize,
    ) -> Result<Vec<Statement>, FrontendError> {
        let mut result = Vec::new();
        for (index, statement) in statements.iter().enumerate() {
            if index == 0
                && let Stmt::Expr(node) = statement
                && node.value.is_string_literal_expr()
            {
                continue;
            }
            self.statement(statement, in_function, depth, &mut result)?;
        }
        Ok(result)
    }

    fn statement(
        &self,
        statement: &Stmt,
        in_function: bool,
        depth: usize,
        result: &mut Vec<Statement>,
    ) -> Result<(), FrontendError> {
        self.check_depth(statement, depth)?;
        let kind = match statement {
            Stmt::Pass(_) => return Ok(()),
            Stmt::FunctionDef(node) => StatementKind::Function(self.function(node, depth + 1)?),
            Stmt::Import(node) if !node.is_lazy => {
                return self.imports(&node.names, None, 0, result);
            }
            Stmt::ImportFrom(node) if !node.is_lazy => {
                let module = node.module.as_ref().map_or("", |name| name.as_str());
                if node.level == 0 && module == "__future__" {
                    return Err(self.lines.unsupported(
                        node,
                        "future directives are outside the Python preview subset",
                    ));
                }
                return self.imports(&node.names, Some(module), node.level, result);
            }
            Stmt::Assign(node) => {
                let names = node
                    .targets
                    .iter()
                    .map(|target| match target {
                        Expr::Name(name) if name.id.as_str() != "__debug__" => {
                            Ok(name.id.to_string())
                        }
                        _ => Err(self
                            .lines
                            .unsupported(target, "assignment targets must be simple names")),
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                StatementKind::Assign {
                    names,
                    value: self.expression(&node.value, depth + 1)?,
                }
            }
            Stmt::Return(node) if in_function => StatementKind::Return(
                node.value
                    .as_deref()
                    .map(|value| self.expression(value, depth + 1))
                    .transpose()?,
            ),
            Stmt::Return(node) => {
                return Err(self
                    .lines
                    .error("parse_error", node, "'return' outside function"));
            }
            Stmt::If(node) => self.conditional(node, in_function, depth + 1)?,
            Stmt::Expr(node) => StatementKind::Expr(self.expression(&node.value, depth + 1)?),
            _ => {
                return Err(self
                    .lines
                    .unsupported(statement, "statement is outside the Python preview subset"));
            }
        };
        result.push(Statement {
            location: self.lines.locate(statement.start()),
            kind,
        });
        Ok(())
    }

    fn function(&self, node: &StmtFunctionDef, depth: usize) -> Result<Function, FrontendError> {
        if node.is_async
            || node.type_params.is_some()
            || !node.decorator_list.is_empty()
            || node.returns.is_some()
            || node.name.as_str() == "__debug__"
        {
            return Err(self.lines.unsupported(
                node,
                "functions must be synchronous and have no decorators or annotations",
            ));
        }
        let syntax = &node.parameters;
        let mut locals = BTreeSet::new();
        let mut parameters = Vec::with_capacity(syntax.len());
        let ordered_parameters = syntax
            .posonlyargs
            .iter()
            .map(|arg| (&arg.parameter, arg.default(), ParameterKind::PositionalOnly))
            .chain(syntax.args.iter().map(|arg| {
                (
                    &arg.parameter,
                    arg.default(),
                    ParameterKind::PositionalOrKeyword,
                )
            }))
            .chain(
                syntax
                    .vararg
                    .as_deref()
                    .map(|arg| (arg, None, ParameterKind::VariadicPositional)),
            )
            .chain(
                syntax
                    .kwonlyargs
                    .iter()
                    .map(|arg| (&arg.parameter, arg.default(), ParameterKind::KeywordOnly)),
            )
            .chain(
                syntax
                    .kwarg
                    .as_deref()
                    .map(|arg| (arg, None, ParameterKind::VariadicKeyword)),
            );
        for (parameter, default, kind) in ordered_parameters {
            let name = parameter.name.to_string();
            if parameter.annotation().is_some() || name == "__debug__" {
                return Err(self.lines.unsupported(
                    parameter,
                    "annotated parameters and binding __debug__ are outside the preview subset",
                ));
            }
            if !locals.insert(name.clone()) {
                return Err(self
                    .lines
                    .error("parse_error", parameter, "duplicate parameter name"));
            }
            parameters.push(Parameter {
                name,
                kind,
                default: default
                    .map(|expression| self.expression(expression, depth))
                    .transpose()?,
            });
        }
        let body = self.suite(&node.body, true, depth)?;
        let free_names = collect_scope_names(&body, &mut locals);
        Ok(Function {
            name: node.name.to_string(),
            location: self.lines.locate(node.name.start()),
            parameters,
            locals,
            free_names,
            body,
        })
    }

    fn conditional(
        &self,
        node: &StmtIf,
        in_function: bool,
        depth: usize,
    ) -> Result<StatementKind, FrontendError> {
        let test = self.expression(&node.test, depth)?;
        let body = self.suite(&node.body, in_function, depth)?;
        let mut otherwise = Vec::new();
        let mut elifs = Vec::new();
        for (index, clause) in node.elif_else_clauses.iter().enumerate() {
            let depth = depth + index;
            self.check_depth(clause, depth)?;
            if let Some(test) = &clause.test {
                elifs.push((
                    self.lines.locate(clause.start()),
                    self.expression(test, depth + 1)?,
                    self.suite(&clause.body, in_function, depth + 1)?,
                ));
            } else {
                otherwise = self.suite(&clause.body, in_function, depth)?;
            }
        }
        for (location, test, body) in elifs.into_iter().rev() {
            otherwise = vec![Statement {
                location,
                kind: StatementKind::If {
                    test,
                    body,
                    otherwise,
                },
            }];
        }
        Ok(StatementKind::If {
            test,
            body,
            otherwise,
        })
    }

    fn imports(
        &self,
        aliases: &[Alias],
        from: Option<&str>,
        level: u32,
        result: &mut Vec<Statement>,
    ) -> Result<(), FrontendError> {
        for alias in aliases {
            let name = alias.name.as_str();
            let module = from.unwrap_or(name);
            if name == "*" {
                return Err(self
                    .lines
                    .unsupported(alias, "star imports are unsupported"));
            }
            if !module.is_empty() && !module.split('.').all(simple_name) {
                return Err(self
                    .lines
                    .unsupported(alias, "import paths require ASCII module names"));
            }
            let local = alias.asname.as_ref().map_or_else(
                || name.split('.').next().unwrap_or(name),
                |name| name.as_str(),
            );
            if local == "__debug__" {
                return Err(self
                    .lines
                    .error("parse_error", alias, "cannot bind __debug__"));
            }
            result.push(Statement {
                location: self.lines.locate(alias.start()),
                kind: StatementKind::Import(Import {
                    level,
                    module: module.into(),
                    member: from.map(|_| name.into()),
                    alias: alias.asname.as_ref().map(ToString::to_string),
                }),
            });
        }
        Ok(())
    }

    fn expression(&self, expression: &Expr, depth: usize) -> Result<Expression, FrontendError> {
        self.check_depth(expression, depth)?;
        let child = |expression| self.expression(expression, depth + 1);
        let kind = match expression {
            Expr::Name(name) => ExpressionKind::Name(name.id.to_string()),
            Expr::Attribute(node) => ExpressionKind::Attribute {
                value: Box::new(child(&node.value)?),
                name: node.attr.to_string(),
            },
            Expr::Call(node) => self.call(node, depth + 1)?,
            Expr::NoneLiteral(_) => ExpressionKind::Constant(Constant::None),
            Expr::BooleanLiteral(node) => ExpressionKind::Constant(Constant::Bool(node.value)),
            Expr::StringLiteral(node) => {
                // Ruff replaces escaped surrogate code points with U+FFFD.
                // Reject that lossy input before using the cooked comparison value.
                for part in &node.value {
                    if part.contains(char::REPLACEMENT_CHARACTER)
                        && !matches!(part.flags.prefix(), StringLiteralPrefix::Raw { .. })
                        && has_surrogate_escape(&self.source[part.range])
                    {
                        return Err(self.lines.unsupported(
                            part,
                            "string surrogate escapes are outside the preview subset",
                        ));
                    }
                }
                ExpressionKind::Constant(Constant::String(node.value.to_str().into()))
            }
            Expr::NumberLiteral(_) => self.integer(expression, false)?,
            Expr::List(node) => ExpressionKind::List(self.expressions(&node.elts, depth + 1)?),
            Expr::Tuple(node) => ExpressionKind::Tuple(self.expressions(&node.elts, depth + 1)?),
            Expr::Compare(node) => ExpressionKind::Compare {
                left: Box::new(child(&node.left)?),
                comparisons: node
                    .ops
                    .iter()
                    .zip(node.comparators.iter())
                    .map(|(&op, expression)| {
                        self.expression(expression, depth + 1)
                            .map(|value| (op.into(), value))
                    })
                    .collect::<Result<_, _>>()?,
            },
            Expr::BoolOp(node) => ExpressionKind::Boolean {
                op: match node.op {
                    BoolOp::And => BooleanOperator::And,
                    BoolOp::Or => BooleanOperator::Or,
                },
                values: self.expressions(&node.values, depth + 1)?,
            },
            Expr::UnaryOp(node) => match node.op {
                UnaryOp::Not => ExpressionKind::Not(Box::new(child(&node.operand)?)),
                UnaryOp::UAdd | UnaryOp::USub => {
                    self.integer(&node.operand, node.op == UnaryOp::USub)?
                }
                _ => {
                    return Err(self
                        .lines
                        .unsupported(expression, "unary operation is outside the preview subset"));
                }
            },
            _ => {
                return Err(self.lines.unsupported(
                    expression,
                    "expression is outside the Python preview subset",
                ));
            }
        };
        Ok(Expression {
            location: self.lines.locate(expression.start()),
            kind,
        })
    }

    fn expressions(
        &self,
        expressions: &[Expr],
        depth: usize,
    ) -> Result<Vec<Expression>, FrontendError> {
        expressions
            .iter()
            .map(|expression| self.expression(expression, depth))
            .collect()
    }

    fn integer(&self, expression: &Expr, negate: bool) -> Result<ExpressionKind, FrontendError> {
        if let Expr::NumberLiteral(node) = expression
            && let Number::Int(value) = &node.value
            && let Some(magnitude) = value.as_u64()
        {
            let signed = i128::from(magnitude);
            if let Ok(value) = i64::try_from(if negate { -signed } else { signed }) {
                return Ok(ExpressionKind::Constant(Constant::Int(value)));
            }
        }
        Err(self.lines.unsupported(
            expression,
            "expected an integer literal in the signed 64-bit range",
        ))
    }

    fn call(&self, node: &ExprCall, depth: usize) -> Result<ExpressionKind, FrontendError> {
        let function = Box::new(self.expression(&node.func, depth)?);
        let positional = self.expressions(&node.arguments.args, depth)?;
        let mut names = BTreeSet::new();
        let mut keywords = Vec::with_capacity(node.arguments.keywords.len());
        for keyword in &node.arguments.keywords {
            let Some(name) = &keyword.arg else {
                return Err(self
                    .lines
                    .unsupported(keyword, "keyword argument unpacking is unsupported"));
            };
            if !names.insert(name.as_str()) {
                return Err(self
                    .lines
                    .error("parse_error", keyword, "duplicate keyword argument"));
            }
            keywords.push((name.to_string(), self.expression(&keyword.value, depth)?));
        }
        Ok(ExpressionKind::Call {
            function,
            positional,
            keywords,
        })
    }

    fn check_depth(&self, node: &impl Ranged, depth: usize) -> Result<(), FrontendError> {
        if depth >= MAX_NESTING {
            return Err(self.lines.error(
                "analysis_limit",
                node,
                "Python preview nesting limit exceeded",
            ));
        }
        Ok(())
    }
}

fn has_surrogate_escape(mut token: &str) -> bool {
    while let Some((_, escape)) = token.split_once('\\') {
        let Some(kind) = escape.chars().next() else {
            break;
        };
        token = &escape[kind.len_utf8()..];
        let width = match kind {
            'u' => 4,
            'U' => 8,
            _ => continue,
        };
        if let Some(digits) = token.get(..width)
            && let Ok(code_point) = u32::from_str_radix(digits, 16)
            && (0xd800..=0xdfff).contains(&code_point)
        {
            return true;
        }
    }
    false
}

fn collect_scope_names(
    statements: &[Statement],
    locals: &mut BTreeSet<String>,
) -> BTreeSet<String> {
    let mut reads = BTreeSet::new();
    let mut statements: Vec<_> = statements.iter().collect();
    let mut expressions = Vec::new();
    while let Some(statement) = statements.pop() {
        match &statement.kind {
            StatementKind::Function(function) => {
                locals.insert(function.name.clone());
                reads.extend(function.free_names.iter().cloned());
                // Defaults are evaluated when the nested function is defined.
                expressions.extend(
                    function
                        .parameters
                        .iter()
                        .filter_map(|p| p.default.as_ref()),
                );
            }
            StatementKind::Import(import) => {
                locals.insert(import.local_name().into());
            }
            StatementKind::Assign { names, value } => {
                locals.extend(names.iter().cloned());
                expressions.push(value);
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
            StatementKind::Return(value) => expressions.extend(value.as_ref()),
            StatementKind::Expr(value) => expressions.push(value),
        }
    }
    while let Some(expression) = expressions.pop() {
        match &expression.kind {
            ExpressionKind::Name(name) => {
                reads.insert(name.clone());
            }
            ExpressionKind::Attribute { value, .. } | ExpressionKind::Not(value) => {
                expressions.push(value);
            }
            ExpressionKind::Call {
                function,
                positional,
                keywords,
            } => {
                expressions.push(function);
                expressions.extend(positional);
                expressions.extend(keywords.iter().map(|(_, value)| value));
            }
            ExpressionKind::List(values)
            | ExpressionKind::Tuple(values)
            | ExpressionKind::Boolean { values, .. } => expressions.extend(values),
            ExpressionKind::Compare { left, comparisons } => {
                expressions.push(left);
                expressions.extend(comparisons.iter().map(|(_, value)| value));
            }
            ExpressionKind::Constant(_) => {}
        }
    }
    reads.retain(|name| !locals.contains(name));
    reads
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_lexical_locals_and_definition_time_defaults() {
        let source = "def outer(arg=initialize(), /, *args, flag=True, **kwargs):
    if False:
        from .helpers import load as alias
    elif flag:
        target = arg
    def nested(value=make_default()):
        inner = 1
        return arg or module_value
    return target
";
        let facts = extract(source).unwrap();
        let StatementKind::Function(outer) = &facts.body[0].kind else {
            panic!("expected a function definition");
        };
        assert_eq!(outer.name, "outer");
        assert_eq!((outer.location.line, outer.location.column), (1, 5));
        assert_eq!(
            outer.locals.iter().map(String::as_str).collect::<Vec<_>>(),
            ["alias", "arg", "args", "flag", "kwargs", "nested", "target"],
        );
        assert_eq!(
            outer
                .free_names
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["make_default", "module_value"],
        );
        assert_eq!(
            outer.parameters.iter().map(|p| p.kind).collect::<Vec<_>>(),
            [
                ParameterKind::PositionalOnly,
                ParameterKind::VariadicPositional,
                ParameterKind::KeywordOnly,
                ParameterKind::VariadicKeyword,
            ],
        );
        let default = outer.parameters[0].default.as_ref().unwrap();
        assert!(matches!(
            &default.kind,
            ExpressionKind::Call { function, .. }
                if matches!(&function.kind, ExpressionKind::Name(name) if name == "initialize")
        ));
        let StatementKind::If {
            body, otherwise, ..
        } = &outer.body[0].kind
        else {
            panic!("expected a conditional");
        };
        assert!(matches!(
            &body[0].kind, StatementKind::Import(import)
            if import.level == 1 && import.module == "helpers" && import.local_name() == "alias"
        ));
        assert!(matches!(&otherwise[0].kind, StatementKind::If { .. }));
    }

    #[test]
    fn keeps_unicode_bindings_and_byte_locations() {
        let facts = extract("def K():\n    pass\n'🙂'; K()\n").unwrap();
        let StatementKind::Function(function) = &facts.body[0].kind else {
            panic!("expected a function definition");
        };
        let StatementKind::Expr(call) = &facts.body[2].kind else {
            panic!("expected a call statement");
        };
        assert_eq!((call.location.line, call.location.column), (3, 9));
        assert!(matches!(
            &call.kind,
            ExpressionKind::Call { function: target, .. }
                if matches!(&target.kind, ExpressionKind::Name(name) if *name == function.name)
        ));
    }

    #[test]
    fn surfaces_unsupported_and_invalid_selected_syntax() {
        for (source, kind, line) in [
            (
                "if False:\n    from helpers import *",
                "unsupported_construct",
                2,
            ),
            ("@register\ndef f():\n    pass", "unsupported_construct", 1),
            ("type Alias = int", "unsupported_syntax_version", 1),
            ("return None", "parse_error", 1),
            ("def bad(x, x):\n    pass", "parse_error", 1),
            ("hello\0", "parse_error", 1),
        ] {
            let error = extract(source).unwrap_err();
            assert_eq!(error.kind, kind, "{source}: {error:?}");
            assert_eq!(error.location.line, line, "{source}: {error:?}");
        }
    }

    #[test]
    fn bounds_flat_elif_chains_before_building_owned_nesting() {
        let source = format!(
            "if False: pass\n{}",
            "elif False: pass\n".repeat(MAX_NESTING)
        );
        let error = extract(&source).unwrap_err();
        assert_eq!(error.kind, "analysis_limit");
    }

    #[test]
    fn rejects_strings_with_lossy_surrogate_decoding() {
        for literal in [r"result = '\ud800'", r"result = '\U0000dfff'"] {
            assert_eq!(extract(literal).unwrap_err().kind, "unsupported_construct");
        }
        for literal in [
            r"result = r'\ud800�'",
            r"result = '\\ud800�'",
            r"result = '\ufffd'",
        ] {
            assert!(extract(literal).is_ok(), "{literal}");
        }
    }
}
