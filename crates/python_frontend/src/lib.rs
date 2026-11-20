//! Ruff adapters for the native Python analyzers.

pub mod phase02;
pub mod preview;

use std::num::NonZeroUsize;

use ruff_python_ast::{Expr, Mod, ModModule, PythonVersion, Stmt};
use ruff_python_parser::{Mode, ParseOptions, parse_unchecked};
use ruff_text_size::Ranged;
use serde::{Deserialize, Deserializer, Serialize};

#[derive(Debug)]
pub struct Module {
    pub imports: Vec<Import>,
    pub functions: Vec<Function>,
    pub calls: Vec<Call>,
}

#[derive(Debug)]
pub struct Import {
    pub local: String,
    pub module: String,
    pub member: Option<String>,
    pub location: Location,
}

#[derive(Debug)]
pub struct Function {
    pub name: String,
    pub location: Location,
    pub calls: Vec<Call>,
}

#[derive(Debug)]
pub struct Call {
    pub name: String,
    pub member: Option<String>,
    pub location: Location,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
pub struct Location {
    #[serde(deserialize_with = "one_based")]
    pub line: usize,
    #[serde(deserialize_with = "one_based")]
    pub column: usize,
}

fn one_based<'de, D: Deserializer<'de>>(input: D) -> Result<usize, D::Error> {
    NonZeroUsize::deserialize(input).map(NonZeroUsize::get)
}

#[derive(Debug)]
pub struct FrontendError {
    pub kind: &'static str,
    pub message: String,
    pub location: Location,
}

struct Lines(Vec<usize>);

impl Lines {
    fn locate(&self, offset: impl Into<usize>) -> Location {
        let offset = offset.into();
        let line = self.0.partition_point(|&start| start <= offset) - 1;
        Location {
            line: line + 1,
            column: offset - self.0[line] + 1,
        }
    }

    fn unsupported(&self, node: &impl Ranged, message: &str) -> FrontendError {
        self.error("unsupported_construct", node, message)
    }

    fn error(&self, kind: &'static str, node: &impl Ranged, message: &str) -> FrontendError {
        FrontendError {
            kind,
            message: message.into(),
            location: self.locate(node.start()),
        }
    }
}

pub fn simple_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes
        .next()
        .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
        && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
        && name != "__debug__"
}

fn parse(source: &str) -> Result<(ModModule, Lines), FrontendError> {
    let (parsed, lines) = parse_source(source)?;
    let Mod::Module(syntax) = parsed.into_syntax() else {
        unreachable!("module mode always produces a module AST");
    };
    Ok((syntax, lines))
}

fn parse_source(source: &str) -> Result<(ruff_python_parser::Parsed<Mod>, Lines), FrontendError> {
    let lines = Lines(
        std::iter::once(0)
            .chain(source.match_indices('\n').map(|(index, _)| index + 1))
            .collect(),
    );
    if let Some(offset) = source.find('\0') {
        return Err(FrontendError {
            kind: "parse_error",
            message: "Python source cannot contain NUL bytes".into(),
            location: lines.locate(offset),
        });
    }
    let options = ParseOptions::from(Mode::Module).with_target_version(PythonVersion::PY311);
    let parsed = parse_unchecked(source, options);
    if let Some(error) = parsed.errors().first() {
        return Err(FrontendError {
            kind: "parse_error",
            message: error.error.to_string(),
            location: lines.locate(error.start()),
        });
    }
    if let Some(error) = parsed.unsupported_syntax_errors().first() {
        return Err(FrontendError {
            kind: "unsupported_syntax_version",
            message: error.to_string(),
            location: lines.locate(error.start()),
        });
    }
    Ok((parsed, lines))
}

/// Parse normalized UTF-8 Python with Python 3.11 syntax and extract owned facts.
pub fn extract(source: &str) -> Result<Module, FrontendError> {
    let (syntax, lines) = parse(source)?;
    let mut module = Module {
        imports: Vec::new(),
        functions: Vec::new(),
        calls: Vec::new(),
    };
    for (index, stmt) in syntax.body.iter().enumerate() {
        let (aliases, module_name) = match stmt {
            Stmt::Import(node) if !node.is_lazy => (&node.names, None),
            Stmt::ImportFrom(node) if node.level == 0 && !node.is_lazy => {
                (&node.names, node.module.as_ref().map(|name| name.as_str()))
            }
            Stmt::FunctionDef(node)
                if module.calls.is_empty()
                    && !node.is_async
                    && node.parameters.is_empty()
                    && node.decorator_list.is_empty()
                    && node.returns.is_none()
                    && node.type_params.is_none()
                    && simple_name(node.name.as_str()) =>
            {
                let mut calls = Vec::new();
                for (index, stmt) in node.body.iter().enumerate() {
                    if let Some(call) = statement_call(stmt, index, &lines)? {
                        calls.push(call);
                    }
                }
                module.functions.push(Function {
                    name: node.name.to_string(),
                    location: lines.locate(node.name.start()),
                    calls,
                });
                continue;
            }
            _ => {
                if let Some(call) = statement_call(stmt, index, &lines)? {
                    module.calls.push(call);
                }
                continue;
            }
        };
        if !module.functions.is_empty() || !module.calls.is_empty() {
            return Err(lines.unsupported(stmt, "imports must precede definitions and calls"));
        }
        for alias in aliases {
            let name = alias.name.as_str();
            let local = alias.asname.as_ref().map_or(name, |name| name.as_str());
            if !simple_name(name)
                || !simple_name(local)
                || module_name.is_some_and(|name| !simple_name(name) || name == "__future__")
            {
                return Err(
                    lines.unsupported(alias, "expected an absolute flat module or function import")
                );
            }
            module.imports.push(Import {
                local: local.into(),
                module: module_name.unwrap_or(name).into(),
                member: module_name.map(|_| name.into()),
                location: lines.locate(alias.start()),
            });
        }
    }
    Ok(module)
}

fn statement_call(
    statement: &Stmt,
    index: usize,
    lines: &Lines,
) -> Result<Option<Call>, FrontendError> {
    match statement {
        Stmt::Pass(_) => Ok(None),
        Stmt::Expr(node) => match node.value.as_ref() {
            Expr::StringLiteral(_) if index == 0 => Ok(None),
            Expr::Call(call) if call.arguments.is_empty() => {
                let (name, member) = match call.func.as_ref() {
                    Expr::Name(name) => (name.id.as_str(), None),
                    Expr::Attribute(attr) => match attr.value.as_ref() {
                        Expr::Name(name) => (name.id.as_str(), Some(attr.attr.as_str())),
                        _ => {
                            return Err(lines.unsupported(call, "expected a module-qualified call"));
                        }
                    },
                    _ => return Err(lines.unsupported(call, "expected a direct function call")),
                };
                if !simple_name(name) || member.is_some_and(|name| !simple_name(name)) {
                    return Err(lines.unsupported(call, "expected ASCII identifiers"));
                }
                Ok(Some(Call {
                    name: name.into(),
                    member: member.map(str::to_owned),
                    location: lines.locate(call.start()),
                }))
            }
            _ => Err(lines.unsupported(node, "expected a zero-argument call or leading docstring")),
        },
        _ => Err(lines.unsupported(statement, "statement is outside the prototype subset")),
    }
}
