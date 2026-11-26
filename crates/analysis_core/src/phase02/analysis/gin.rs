//! Project-root Gin inputs and declaration evidence for selected bindings.

use std::cell::RefCell;
use std::path::Component;
use std::sync::Arc;

use super::*;
use crate::phase02::config::Selection;
use crate::preview_source::{Project, Stamp, display, read_snapshot, reject_symlink_components};

const MAX_INPUTS: usize = 32;
const MAX_CONFIG_CALLS: usize = 64;
const MAX_BINDINGS: usize = 1024;
const MAX_IMPORTS: usize = 4096;

pub(super) const CONFIG_BINDING: &str = "gin_config_binding";

pub(in crate::phase02) struct GinProject<'a> {
    project: &'a Project,
    selection: &'a Selection,
    inputs: RefCell<BTreeMap<PathBuf, Input>>,
}

struct Input {
    stamp: Stamp,
    digest: blake3::Hash,
    bindings: Arc<[ConfigBinding]>,
}

struct ConfigBinding {
    selector: String,
    parameter: String,
    line: usize,
}

impl<'a> GinProject<'a> {
    pub(in crate::phase02) fn new(project: &'a Project, selection: &'a Selection) -> Self {
        Self {
            project,
            selection,
            inputs: RefCell::default(),
        }
    }

    fn read(&self, name: &str) -> Result<(String, Arc<[ConfigBinding]>), Issue> {
        let relative = Path::new(name);
        if relative.extension().and_then(OsStr::to_str) != Some("gin")
            || !relative
                .components()
                .all(|part| matches!(part, Component::Normal(_)))
            || name.contains(['\\', ':'])
        {
            return Err(Issue::new(
                "unresolved_gin_config",
                ".",
                "Gin configuration path must name a relative .gin file inside the project",
            ));
        }
        let path = self.project.base().join(relative);
        let label = self.project.label(&path);
        if self.selection.exclusion(&label).is_some() {
            return Err(Issue::new(
                "unresolved_gin_config",
                &label,
                "Gin configuration is excluded",
            ));
        }
        let mut inputs = self.inputs.borrow_mut();
        let at_limit = inputs.len() >= MAX_INPUTS;
        let input = match inputs.entry(path) {
            Entry::Occupied(input) => input.into_mut(),
            Entry::Vacant(input) => {
                if at_limit {
                    return Err(Issue::new(
                        "analysis_limit",
                        &label,
                        "Gin file limit exceeded",
                    ));
                }
                reject_symlink_components(self.project.base(), input.key())
                    .map_err(|message| Issue::new("unsupported_file", &label, message))?;
                let (bytes, stamp) = read_snapshot(input.key(), &label, None)?;
                let source = std::str::from_utf8(&bytes).map_err(|_| {
                    Issue::new("read_error", &label, "Gin configuration is not UTF-8")
                })?;
                input.insert(Input {
                    stamp,
                    digest: blake3::hash(&bytes),
                    bindings: config_bindings(source, &label)?.into(),
                })
            }
        };
        Ok((label, Arc::clone(&input.bindings)))
    }

    pub(in crate::phase02) fn input_digests(&self) -> BTreeMap<String, blake3::Hash> {
        self.inputs
            .borrow()
            .iter()
            .map(|(path, input)| (display(path), input.digest))
            .collect()
    }

    pub(in crate::phase02) fn verify(self) -> Vec<Issue> {
        self.inputs
            .into_inner()
            .into_iter()
            .filter_map(|(path, input)| {
                let valid = reject_symlink_components(self.project.base(), &path).is_ok()
                    && std::fs::symlink_metadata(&path).is_ok_and(|metadata| {
                        metadata.is_file() && Stamp::new(&metadata) == input.stamp
                    });
                (!valid).then(|| {
                    Issue::new(
                        "source_changed",
                        &self.project.label(&path),
                        "Gin configuration changed after lookup",
                    )
                })
            })
            .collect()
    }
}

fn config_bindings(source: &str, label: &str) -> Result<Vec<ConfigBinding>, Issue> {
    let mut bindings = Vec::new();
    for (offset, line) in source.lines().enumerate() {
        let line = line
            .split_once('#')
            .map_or(line, |(statement, _)| statement)
            .trim();
        if line.is_empty() {
            continue;
        }
        let line_number = offset + 1;
        let at = Location {
            line: line_number,
            column: 1,
        };
        if bindings.len() == MAX_BINDINGS {
            return Err(Issue::new("analysis_limit", label, "Gin binding limit exceeded").at(at));
        }
        let Some(((selector, parameter), _)) = line
            .split_once('=')
            .and_then(|(key, literal)| key.trim().rsplit_once('.').map(|key| (key, literal.trim())))
            .filter(|((selector, parameter), literal)| {
                selector.split('.').all(identifier)
                    && identifier(parameter)
                    && config_literal(literal)
            })
        else {
            return Err(Issue::new(
                "unresolved_gin_config",
                label,
                "Gin configuration requires literal selector.parameter assignments",
            )
            .at(at));
        };
        bindings.push(ConfigBinding {
            selector: selector.into(),
            parameter: parameter.into(),
            line: line_number,
        });
    }
    Ok(bindings)
}

fn identifier(value: &str) -> bool {
    let mut bytes = value.bytes();
    bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn config_literal(value: &str) -> bool {
    if matches!(value, "None" | "True" | "False") {
        return true;
    }
    let bytes = value.as_bytes();
    if let [quote @ (b'\'' | b'"'), inner @ .., close] = bytes {
        return quote == close
            && inner
                .iter()
                .all(|byte| byte.is_ascii_graphic() || *byte == b' ')
            && !inner.contains(quote)
            && !inner.contains(&b'\\');
    }
    let unsigned = value.strip_prefix('-').unwrap_or(value);
    let digits = |part: &str| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit());
    if let Some((whole, fraction)) = unsigned.split_once('.') {
        digits(whole) && digits(fraction) && value.parse::<f64>().is_ok_and(f64::is_finite)
    } else {
        digits(unsigned)
            && (!unsigned.starts_with('0') || unsigned.bytes().all(|byte| byte == b'0'))
            && value.parse::<i64>().is_ok()
    }
}

pub(super) struct Registration {
    qualified_name: String,
    allowed: BTreeSet<String>,
    scope: ScopeId,
    at: Location,
    target: SymbolId,
}

pub(super) struct ConfigCall {
    scope: ScopeId,
    at: Location,
    name: String,
}

pub(super) fn is_config_loader(name: &str) -> bool {
    matches!(
        name,
        "gin.parse_config_file" | "gin.config.parse_config_file"
    )
}

fn is_registrar(name: &str) -> bool {
    matches!(name, "gin.configurable" | "gin.config.configurable")
}

pub(super) fn is_stop_after_attempt(name: &str) -> bool {
    matches!(
        name,
        "tenacity.stop_after_attempt" | "tenacity.stop.stop_after_attempt"
    )
}

pub(super) fn supported_stop_arguments(
    arguments: &[Expression],
    keywords: &[(String, Expression)],
) -> bool {
    matches!(arguments, [value] if matches!(value.kind, ExpressionKind::Scalar(Some(attempts)) if attempts > 0))
        && keywords.is_empty()
}

pub(super) fn tracks_provider_member(name: &str) -> bool {
    is_config_loader(name)
        || is_registrar(name)
        || is_stop_after_attempt(name)
        || name == "tenacity.retry"
        || is_validation_wrapper(name)
}

fn is_validation_wrapper(name: &str) -> bool {
    matches!(
        name,
        "pydantic.validate_call" | "pydantic.validate_call_decorator.validate_call"
    )
}

fn registration_signature<'a>(
    definition: &'a Definition,
    args: &'a [Expression],
    keywords: &'a [(String, Expression)],
) -> Option<(&'a str, BTreeSet<String>)> {
    if !definition.simple_positional_parameters || definition.positional_only_parameter_count != 0 {
        return None;
    }
    let [name] = args else { return None };
    let ExpressionKind::Text(name) = &name.kind else {
        return None;
    };
    if !identifier(name) {
        return None;
    }
    let mut allowed: BTreeSet<_> = definition
        .parameters
        .iter()
        .map(|name| name.text.clone())
        .collect();
    match keywords {
        [] => {}
        [(option, value)] if option == "denylist" => {
            let ExpressionKind::Sequence { items, list: true } = &value.kind else {
                return None;
            };
            for item in items {
                let ExpressionKind::Text(name) = &item.kind else {
                    return None;
                };
                if !allowed.remove(name) {
                    return None;
                }
            }
        }
        _ => return None,
    }
    Some((name, allowed))
}

impl Graph<'_> {
    pub(super) fn gin_config_call(&mut self, call: CallArguments<'_>) {
        let CallArguments {
            scope,
            at,
            positional,
            keywords,
            ..
        } = call;
        if scope == self.scopes[scope].file
            && self.branch_depth == 0
            && keywords.is_empty()
            && let [path] = positional
            && let ExpressionKind::Text(name) = &path.kind
        {
            if self.gin_config_calls.len() == MAX_CONFIG_CALLS {
                self.scope_issue(
                    scope,
                    at,
                    "analysis_limit",
                    "Gin configuration call limit exceeded",
                );
                return;
            }
            self.gin_config_calls.push(ConfigCall {
                scope,
                at,
                name: name.clone(),
            });
        } else {
            self.scope_issue(
                scope,
                at,
                "unresolved_gin_config",
                "Gin configuration requires an unconditional module-level call with one literal path",
            );
        }
    }

    pub(super) fn record_gin_decorator(
        &mut self,
        scope: ScopeId,
        decorator: &Expression,
        target: SymbolId,
        before: Location,
        provider: &str,
    ) -> bool {
        if is_registrar(provider)
            && self.branch_depth == 0
            && scope == self.scopes[scope].file
            && let Some(definition) = self.gin_function(target)
            && let ExpressionKind::Call {
                arguments,
                keywords,
                ..
            } = &decorator.kind
            && let Some((name, allowed)) = registration_signature(definition, arguments, keywords)
        {
            let qualified_name = format!("{}.{name}", self.files[scope].module);
            let candidates = self.gin_registrations.entry(name.to_owned()).or_default();
            if candidates.iter().any(|prior| prior.scope == scope) {
                self.scope_issue(
                    scope,
                    decorator.at,
                    "unresolved_gin_config",
                    "Gin configurable name is registered more than once",
                );
                return true;
            }
            candidates.push(Registration {
                qualified_name,
                allowed,
                scope,
                at: decorator.at,
                target,
            });
            return true;
        }
        self.gin_wrapper(scope, decorator, target, before, 0, provider)
    }

    fn gin_function(&self, target: SymbolId) -> Option<&Definition> {
        match self.symbols[target].binding {
            Binding::Definition(definition, _)
                if definition.kind == DefinitionKind::Function
                    && !definition.is_generator
                    && definition.decorators.len() == 1 =>
            {
                Some(definition)
            }
            _ => None,
        }
    }

    fn gin_wrapper(
        &mut self,
        scope: ScopeId,
        decorator: &Expression,
        target: SymbolId,
        before: Location,
        depth: usize,
        provider: &str,
    ) -> bool {
        let Some(definition) = self.gin_function(target) else {
            return false;
        };
        let ExpressionKind::Call {
            arguments,
            keywords,
            ..
        } = &decorator.kind
        else {
            return false;
        };
        if !arguments.is_empty() || !definition.annotations.is_empty() {
            return false;
        }
        if is_validation_wrapper(provider) {
            return keywords.is_empty() && definition.parameters.is_empty();
        }
        provider == "tenacity.retry"
            && matches!(
                keywords.as_slice(),
                [(key, value)] if key == "stop"
                    && matches!(&value.kind, ExpressionKind::Call { function, arguments, keywords }
                        if supported_stop_arguments(arguments, keywords)
                            && matches!(self.identity(scope, function, before, depth + 1),
                                Value::Provider(name) if is_stop_after_attempt(&name)))
            )
    }

    pub(super) fn gin_decorator_replaces_function(
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
        let Value::Provider(provider) = self.identity(scope, function, before, depth) else {
            return false;
        };
        is_registrar(&provider)
            && scope == self.scopes[scope].file
            && self.gin_function(target).is_some_and(|definition| {
                registration_signature(definition, arguments, keywords).is_some()
            })
            || self.gin_wrapper(scope, decorator, target, before, depth, &provider)
    }

    pub(super) fn collect_gin_bindings(&mut self, project: Option<&GinProject<'_>>) {
        for call in std::mem::take(&mut self.gin_config_calls) {
            let Some(project) = project else {
                self.scope_issue(
                    call.scope,
                    call.at,
                    "unresolved_gin_config",
                    "Gin configuration requires a project scan",
                );
                continue;
            };
            let (label, bindings) = match project.read(&call.name) {
                Ok(input) => input,
                Err(issue) => {
                    self.scope_issues.push((call.scope, issue));
                    continue;
                }
            };
            if bindings.is_empty() {
                continue;
            }
            let Some(modules) = self.gin_prior_modules(call.scope, call.at) else {
                self.scope_issue(
                    call.scope,
                    call.at,
                    "analysis_limit",
                    "Gin initializer import limit exceeded",
                );
                continue;
            };
            let mut selected = BTreeSet::new();
            let mut unresolved = false;
            for binding in bindings.iter() {
                let suffix = binding.selector.rsplit('.').next().unwrap_or_default();
                let mut matches = self
                    .gin_registrations
                    .get(suffix)
                    .into_iter()
                    .flatten()
                    .filter(|registration| {
                        (registration.qualified_name == binding.selector
                            || suffix == binding.selector)
                            && (modules.contains(&registration.scope)
                                || registration.scope == call.scope
                                    && position(registration.at) < position(call.at))
                    });
                match (matches.next(), matches.next()) {
                    (Some(registration), None)
                        if registration.allowed.contains(&binding.parameter) =>
                    {
                        selected.insert(registration.target);
                    }
                    _ => {
                        unresolved = true;
                        self.scope_issues.push((call.scope, Issue::new(
                            "unresolved_gin_config",
                            &label,
                            "Gin binding needs a unique initialized configurable and an allowed parameter",
                        ).at(Location { line: binding.line, column: 1 })));
                    }
                }
            }
            if !unresolved {
                for target in selected {
                    self.edge(call.scope, target, CONFIG_BINDING, "possible");
                }
            }
        }
    }

    fn gin_prior_modules(&self, launcher: ScopeId, before: Location) -> Option<BTreeSet<ScopeId>> {
        let mut remaining = MAX_IMPORTS;
        let mut reached = BTreeSet::new();
        let mut pending = vec![(launcher, before)];
        while let Some((module, cutoff)) = pending.pop() {
            let scope = &self.scopes[module];
            remaining = remaining.checked_sub(scope.initializer_imports.len())?;
            if scope.initializer_imports.is_empty() {
                continue;
            }
            let evaluated: BTreeSet<_> = scope.evaluated_imports.iter().copied().collect();
            let imports: BTreeSet<_> = scope.imports.iter().copied().collect();
            for &(binding, imported) in &scope.initializer_imports {
                if imported != launcher
                    && position(self.symbols[binding].name.at) < position(cutoff)
                    && evaluated.contains(&binding)
                    && imports.contains(&imported)
                    && reached.insert(imported)
                {
                    pending.push((imported, END));
                }
            }
        }
        Some(reached)
    }
}
