use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, VecDeque};

use skarmory_python::{Call, Location};

use crate::{Finding, Issue, ModuleFile};

#[derive(Clone, Copy)]
enum Binding {
    Function(usize),
    ImportedFunction(usize),
    Module(usize),
}

pub(super) enum Failure {
    Unsupported(Issue),
    Unresolved(Issue),
}

pub(super) struct Graph<'a> {
    modules: BTreeMap<&'a str, usize>,
    edges: Vec<Vec<usize>>,
    function_starts: Vec<usize>,
}

pub(super) fn resolve(files: &[ModuleFile]) -> Result<Graph<'_>, Failure> {
    let modules: BTreeMap<_, _> = files
        .iter()
        .enumerate()
        .map(|(index, file)| (file.name.as_str(), index))
        .collect();
    let mut bindings = Vec::with_capacity(files.len());
    let mut function_starts = Vec::with_capacity(files.len());
    let mut nodes = files.len();
    for file in files {
        let mut scope = BTreeMap::new();
        function_starts.push(nodes);
        for function in &file.facts.functions {
            bind(
                &mut scope,
                &function.name,
                Binding::Function(nodes),
                file,
                function.location,
            )?;
            nodes += 1;
        }
        bindings.push(scope);
    }

    let mut edges = vec![Vec::new(); nodes];
    for (index, file) in files.iter().enumerate() {
        for import in &file.facts.imports {
            let missing = || {
                Failure::Unresolved(
                    Issue::new(
                        "unresolved_import",
                        &file.path,
                        format!(
                            "could not resolve {} in the selected flat modules",
                            import.module
                        ),
                    )
                    .at(import.location),
                )
            };
            let &target = modules.get(import.module.as_str()).ok_or_else(missing)?;
            let binding = match &import.member {
                Some(member) => match bindings[target].get(member.as_str()) {
                    Some(Binding::Function(function)) => Binding::ImportedFunction(*function),
                    _ => {
                        return Err(Failure::Unresolved(
                            Issue::new(
                                "unresolved_import",
                                &file.path,
                                format!(
                                    "{} has no directly defined function named {member}",
                                    import.module
                                ),
                            )
                            .at(import.location),
                        ));
                    }
                },
                None => Binding::Module(target),
            };
            bind(
                &mut bindings[index],
                &import.local,
                binding,
                file,
                import.location,
            )?;
            edges[index].push(target);
        }
    }
    check_import_order(files, &edges[..files.len()])?;
    for (index, file) in files.iter().enumerate() {
        let calls = std::iter::once((index, &file.facts.calls)).chain(
            file.facts
                .functions
                .iter()
                .enumerate()
                .map(|(offset, function)| (function_starts[index] + offset, &function.calls)),
        );
        for (caller, calls) in calls {
            for call in calls {
                let target = resolve_call(&bindings, index, call).ok_or_else(|| {
                    let name = match &call.member {
                        Some(member) => format!("{}.{member}", call.name),
                        None => call.name.clone(),
                    };
                    Failure::Unresolved(
                        Issue::new(
                            "unresolved_call",
                            &file.path,
                            format!("could not resolve {name} to a direct function"),
                        )
                        .at(call.location),
                    )
                })?;
                edges[caller].push(target);
            }
        }
    }
    Ok(Graph {
        modules,
        edges,
        function_starts,
    })
}

fn bind<'a>(
    scope: &mut BTreeMap<&'a str, Binding>,
    name: &'a str,
    binding: Binding,
    file: &ModuleFile,
    location: Location,
) -> Result<(), Failure> {
    match scope.entry(name) {
        Entry::Vacant(slot) => {
            slot.insert(binding);
            Ok(())
        }
        Entry::Occupied(_) => Err(Failure::Unsupported(
            Issue::new(
                "duplicate_binding",
                &file.path,
                format!("duplicate module binding {name}"),
            )
            .at(location),
        )),
    }
}

fn resolve_call(bindings: &[BTreeMap<&str, Binding>], module: usize, call: &Call) -> Option<usize> {
    match (bindings[module].get(call.name.as_str())?, &call.member) {
        (Binding::Function(node) | Binding::ImportedFunction(node), None) => Some(*node),
        (Binding::Module(module), Some(member)) => match bindings[*module].get(member.as_str()) {
            Some(Binding::Function(node)) => Some(*node),
            _ => None,
        },
        _ => None,
    }
}

fn check_import_order(files: &[ModuleFile], edges: &[Vec<usize>]) -> Result<(), Failure> {
    let mut incoming = vec![0; edges.len()];
    for &target in edges.iter().flatten() {
        incoming[target] += 1;
    }
    let mut ready: VecDeque<_> = (0..edges.len())
        .filter(|&index| incoming[index] == 0)
        .collect();
    while let Some(module) = ready.pop_front() {
        for &target in &edges[module] {
            incoming[target] -= 1;
            if incoming[target] == 0 {
                ready.push_back(target);
            }
        }
    }
    if incoming.iter().any(|&count| count > 0) {
        Err(Failure::Unsupported(Issue::new(
            "cyclic_imports",
            ".",
            format!(
                "import graph for {} selected files contains a cycle",
                files.len()
            ),
        )))
    } else {
        Ok(())
    }
}

impl Graph<'_> {
    pub(super) fn reachable(&self, entry: &str) -> Vec<bool> {
        let mut seen = vec![false; self.edges.len()];
        let mut pending = vec![self.modules[entry]];
        while let Some(node) = pending.pop() {
            if !std::mem::replace(&mut seen[node], true) {
                pending.extend(&self.edges[node]);
            }
        }
        seen
    }

    pub(super) fn findings(&self, modules: &[ModuleFile], reachable: &[bool]) -> Vec<Finding> {
        let mut findings = Vec::new();
        for (module, file) in modules.iter().enumerate() {
            for (offset, function) in file.facts.functions.iter().enumerate() {
                if !reachable[self.function_starts[module] + offset] {
                    findings.push(Finding {
                        rule: "unreachable_function",
                        module: file.name.clone(),
                        symbol: function.name.clone(),
                        path: file.path.clone(),
                        line: function.location.line,
                        column: function.location.column,
                        evidence: "no_call_path_from_entry",
                    });
                }
            }
        }
        findings
    }
}
