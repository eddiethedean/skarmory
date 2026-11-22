//! Serializable source facts for the bounded Python 3.11 compatibility profile.

use std::collections::BTreeSet;

use ruff_python_ast::{Alias, BoolOp, CmpOp, Expr, Mod, Stmt, UnaryOp, token::TokenKind};
use ruff_text_size::Ranged;
use serde::{Deserialize, Serialize};

use crate::{FrontendError, Lines, Location, parse_source};

// Limit recursive walks and the depth of serialized facts.
const MAX_NESTING: usize = 32;

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
    Assign {
        names: Vec<Name>,
        value: Option<Expression>,
        annotation: Option<Expression>,
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
pub struct Definition {
    pub name: Name,
    pub kind: DefinitionKind,
    pub body: Vec<Statement>,
    pub decorators: Vec<Expression>,
    pub parameters: Vec<Name>,
    pub has_positional_parameter: bool,
    pub simple_positional_parameters: bool,
    pub defaults: Vec<Expression>,
    pub annotations: Vec<Expression>,
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
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Expression {
    pub at: Location,
    pub kind: ExpressionKind,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum ExpressionKind {
    Name(String),
    Member(Box<Expression>, String),
    Call {
        function: Box<Expression>,
        arguments: Vec<Expression>,
        keywords: Vec<(String, Expression)>,
    },
    Text(String),
    Bool(bool),
    Scalar,
    Sequence(Vec<Expression>),
    Dictionary(Vec<Expression>),
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
    Operands(Vec<Expression>),
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
        body: Lowering(&lines, SyntaxScope::ModuleOrClass).suite(&syntax.body, 0)?,
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

struct Lowering<'a>(&'a Lines, SyntaxScope);

impl Lowering<'_> {
    fn check(&self, node: &impl Ranged, depth: usize) -> Result<(), FrontendError> {
        if depth >= MAX_NESTING {
            return Err(self
                .0
                .error("analysis_limit", node, "Python fact nesting exceeded"));
        }
        Ok(())
    }

    fn name(&self, identifier: &ruff_python_ast::Identifier) -> Name {
        Name {
            text: identifier.to_string(),
            at: self.0.locate(identifier.start()),
        }
    }

    fn target(&self, target: &Expr) -> Result<Name, FrontendError> {
        match target {
            Expr::Name(name) => Ok(Name {
                text: name.id.to_string(),
                at: self.0.locate(name.start()),
            }),
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
                return Err(self
                    .0
                    .unsupported(alias, "star imports cannot resolve finite bindings"));
            }
            let mut local = self.name(alias.asname.as_ref().unwrap_or(&alias.name));
            let exposes_top_level = !from && alias.asname.is_none() && name.contains('.');
            if exposes_top_level {
                local
                    .text
                    .truncate(name.find('.').expect("checked for a dot"));
            }
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
                    let mut parameter_names = BTreeSet::new();
                    for parameter in node.parameters.iter() {
                        if !parameter_names.insert(parameter.name().as_str()) {
                            return Err(self.0.error(
                                "parse_error",
                                parameter.name(),
                                "duplicate parameter name",
                            ));
                        }
                        if let Some(annotation) = parameter.annotation() {
                            annotations.push(self.annotation(annotation, next)?);
                        }
                    }
                    if let Some(returns) = &node.returns {
                        annotations.push(self.annotation(returns, next)?);
                    }
                    StatementKind::Define(Definition {
                        name: self.name(&node.name),
                        kind: if node.is_async {
                            DefinitionKind::Coroutine
                        } else {
                            DefinitionKind::Function
                        },
                        body: Lowering(
                            self.0,
                            if node.is_async {
                                SyntaxScope::AsyncFunction
                            } else {
                                SyntaxScope::Function
                            },
                        )
                        .suite(&node.body, next)?,
                        decorators: node
                            .decorator_list
                            .iter()
                            .map(|item| expression(&item.expression))
                            .collect::<Result<_, _>>()?,
                        parameters: node
                            .parameters
                            .iter()
                            .map(|parameter| self.name(parameter.name()))
                            .collect(),
                        has_positional_parameter: !(node.parameters.posonlyargs.is_empty()
                            && node.parameters.args.is_empty()),
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
                        bases: Vec::new(),
                    })
                }
                Stmt::ClassDef(node)
                    if node
                        .arguments
                        .as_ref()
                        .is_none_or(|args| args.keywords.is_empty()) =>
                {
                    StatementKind::Define(Definition {
                        name: self.name(&node.name),
                        kind: DefinitionKind::Class,
                        body: Lowering(self.0, SyntaxScope::ModuleOrClass)
                            .suite(&node.body, next)?,
                        decorators: node
                            .decorator_list
                            .iter()
                            .map(|item| expression(&item.expression))
                            .collect::<Result<_, _>>()?,
                        parameters: Vec::new(),
                        has_positional_parameter: false,
                        simple_positional_parameters: false,
                        defaults: Vec::new(),
                        annotations: Vec::new(),
                        bases: node
                            .arguments
                            .as_ref()
                            .map(|args| self.expressions(&args.args, next))
                            .transpose()?
                            .unwrap_or_default(),
                    })
                }
                Stmt::Assign(node) => StatementKind::Assign {
                    names: node
                        .targets
                        .iter()
                        .map(|target| self.target(target))
                        .collect::<Result<_, _>>()?,
                    value: Some(expression(&node.value)?),
                    annotation: None,
                },
                Stmt::AnnAssign(node) => StatementKind::Assign {
                    names: vec![self.target(&node.target)?],
                    value: node.value.as_deref().map(expression).transpose()?,
                    annotation: Some(self.annotation(&node.annotation, next)?),
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
                Stmt::Expr(node) => StatementKind::Evaluate(expression(&node.value)?),
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
            Expr::Name(node) => ExpressionKind::Name(node.id.to_string()),
            Expr::Attribute(node) => {
                ExpressionKind::Member(Box::new(lower(&node.value)?), node.attr.to_string())
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
            Expr::BooleanLiteral(node) => ExpressionKind::Bool(node.value),
            Expr::NumberLiteral(_)
            | Expr::NoneLiteral(_)
            | Expr::BytesLiteral(_)
            | Expr::EllipsisLiteral(_) => ExpressionKind::Scalar,
            Expr::Tuple(node) => ExpressionKind::Sequence(self.expressions(&node.elts, next)?),
            Expr::List(node) => ExpressionKind::Sequence(self.expressions(&node.elts, next)?),
            Expr::Dict(node) if node.items.iter().all(|item| item.key.is_some()) => {
                let values = node
                    .items
                    .iter()
                    .flat_map(|item| item.key.iter().chain(std::iter::once(&item.value)));
                ExpressionKind::Dictionary(values.map(lower).collect::<Result<_, _>>()?)
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
