//! Bounded class declarations from dependency source. These files never enter
//! the project graph or execute as part of analysis.

use std::cell::{Ref, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use skarmory_python::phase02::{
    Definition, DefinitionKind, Expression, ExpressionKind, Import, ModuleFacts, StatementKind,
    extract,
};

use crate::preview_source::{Stamp, opaque_python_module, read_snapshot};
use crate::source;

const MAX_MODULES: usize = 128;
const MAX_DEPTH: usize = 32;
const MAX_DIRECTORY_ENTRIES: usize = 4096;
const SITE_NAMES: [&str; 2] = ["site-packages", "dist-packages"];
const ENVIRONMENT_NAMES: [&str; 4] = [".venv", "venv", "env", ".env"];

/// One project environment takes precedence over interpreter-supplied paths.
/// A vendored tree inside the project is never a fallback dependency root.
pub(super) fn search_roots(project: &Path, supplied: &[PathBuf]) -> Vec<PathBuf> {
    let project = if project.is_file() {
        project
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or(Path::new("."))
    } else {
        project
    };
    let Ok(project) = project.canonicalize() else {
        return Vec::new();
    };
    for name in ENVIRONMENT_NAMES {
        let environment = project.join(name);
        if !fs::symlink_metadata(&environment).is_ok_and(|meta| meta.is_dir()) {
            continue;
        }
        let mut candidates = Vec::new();
        if let Ok(entries) = fs::read_dir(environment.join("lib")) {
            candidates.extend(entries.filter_map(Result::ok).filter_map(|entry| {
                let version = entry.file_name();
                (entry.file_type().ok()?.is_dir() && version.to_str()?.starts_with("python"))
                    .then(|| entry.path().join("site-packages"))
            }));
            candidates.sort_unstable();
        }
        candidates.extend([
            environment.join("lib/site-packages"),
            environment.join("Lib/site-packages"),
        ]);
        let selected = unique_directories(candidates, |path| path.starts_with(&environment));
        if !selected.is_empty() {
            return selected;
        }
    }
    unique_directories(
        supplied
            .iter()
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| SITE_NAMES.contains(&name))
            })
            .cloned(),
        |path| !path.starts_with(&project),
    )
}

fn unique_directories(
    candidates: impl IntoIterator<Item = PathBuf>,
    accept: impl Fn(&Path) -> bool,
) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    for candidate in candidates {
        if let Ok(path) = candidate.canonicalize()
            && path.is_dir()
            && accept(&path)
            && !roots.contains(&path)
        {
            roots.push(path);
        }
    }
    roots
}

#[derive(Clone)]
pub(super) struct Class {
    pub(super) members: BTreeMap<String, String>,
}

struct Module {
    name: String,
    package: bool,
    facts: ModuleFacts,
}

#[derive(Clone)]
enum Binding {
    Class(Class),
    Module(String),
    Function(Rc<Module>, usize),
    Type,
    TypeSubclass,
}

pub(super) struct ExternalClassIndex {
    roots: Vec<PathBuf>,
    modules: RefCell<BTreeMap<String, Option<Rc<Module>>>>,
    stamps: RefCell<BTreeMap<PathBuf, Stamp>>,
}

impl ExternalClassIndex {
    pub(super) fn new(roots: Vec<PathBuf>) -> Self {
        Self {
            roots,
            modules: RefCell::new(BTreeMap::new()),
            stamps: RefCell::new(BTreeMap::new()),
        }
    }

    pub(super) fn class(&self, name: &str) -> Option<Class> {
        let (module, member) = name.rsplit_once('.')?;
        match self.binding(&self.module(module)?, member, None, 0)? {
            Binding::Class(class) => Some(class),
            _ => None,
        }
    }

    pub(super) fn class_with_bounded_initializers(&self, name: &str) -> Option<Class> {
        let (module, _) = name.rsplit_once('.')?;
        let class = self.class(name)?;
        let mut active = BTreeSet::new();
        let mut complete = BTreeSet::new();
        for end in module
            .match_indices('.')
            .map(|(end, _)| end)
            .chain(std::iter::once(module.len()))
        {
            if !self.initializer(&module[..end], &mut active, &mut complete) {
                return None;
            }
        }
        Some(class)
    }

    fn initializer(
        &self,
        name: &str,
        active: &mut BTreeSet<String>,
        complete: &mut BTreeSet<String>,
    ) -> bool {
        if complete.contains(name) {
            return true;
        }
        if let Some((parent, _)) = name.rsplit_once('.')
            && !active.contains(parent)
            && !self.initializer(parent, active, complete)
        {
            return false;
        }
        if complete.contains(name) {
            return true;
        }
        if active.len() >= MAX_DEPTH || !active.insert(name.to_owned()) {
            return false;
        }
        let bounded = self.module(name).is_some_and(|module| {
            module
                .facts
                .body
                .iter()
                .enumerate()
                .all(|(index, statement)| match &statement.kind {
                    StatementKind::Import(import) => {
                        self.initializer_import(&module, import, active, complete)
                    }
                    StatementKind::Define(definition)
                        if definition.kind == DefinitionKind::Class =>
                    {
                        self.class_definition(&module, index, definition, 0)
                            .is_some()
                    }
                    StatementKind::Assign {
                        value: Some(value), ..
                    } => literal(value) || self.expression(&module, value, index, 0).is_some(),
                    _ => true,
                })
        });
        active.remove(name);
        if bounded {
            complete.insert(name.to_owned());
        }
        bounded
    }

    fn initializer_import(
        &self,
        module: &Module,
        import: &Import,
        active: &mut BTreeSet<String>,
        complete: &mut BTreeSet<String>,
    ) -> bool {
        let target = if import.level == 0 {
            import.module.clone()
        } else {
            let Some(target) = relative_module(module, import) else {
                return false;
            };
            target
        };
        // "from . import child" can load a child beneath an initializer
        // already on the stack. Reading an arbitrary partially initialized
        // member still needs an order proof.
        if active.contains(&target) {
            return import
                .member
                .as_ref()
                .and_then(|member| self.child_module(self.module(&target)?.as_ref(), member))
                .is_some_and(|value| {
                    matches!(value, Binding::Module(child)
                if self.initializer(&child, active, complete))
                });
        }
        if !self.initializer(&target, active, complete) {
            return false;
        }
        if import.member.is_none() {
            return true;
        }
        match self.import(module, import, 0) {
            Some(Binding::Module(child)) => self.initializer(&child, active, complete),
            Some(_) => true,
            None => false,
        }
    }

    pub(super) fn input_stamps(&self) -> Ref<'_, BTreeMap<PathBuf, Stamp>> {
        self.stamps.borrow()
    }

    pub(super) fn roots_used(&self) -> Vec<&Path> {
        let stamps = self.stamps.borrow();
        self.roots
            .iter()
            .filter(|root| stamps.keys().any(|path| path.starts_with(root)))
            .map(PathBuf::as_path)
            .collect()
    }

    pub(super) fn changed_sources(&self) -> Vec<PathBuf> {
        self.stamps
            .borrow()
            .iter()
            .filter(|(path, stamp)| {
                !fs::symlink_metadata(path)
                    .is_ok_and(|meta| meta.is_file() && Stamp::new(&meta) == **stamp)
            })
            .map(|(path, _)| path.clone())
            .collect()
    }

    fn module(&self, name: &str) -> Option<Rc<Module>> {
        if let Some(cached) = self.modules.borrow().get(name) {
            return cached.clone();
        }
        if self.modules.borrow().len() >= MAX_MODULES {
            return None;
        }
        let module = self.read_module(name).map(Rc::new);
        self.modules
            .borrow_mut()
            .insert(name.into(), module.clone());
        module
    }

    fn read_module(&self, name: &str) -> Option<Module> {
        let (path, package) = self.locate(name)?;
        let (bytes, stamp) = read_snapshot(&path, name, None).ok()?;
        self.stamps.borrow_mut().insert(path, stamp);
        let facts = extract(&source::decode(bytes, name).ok()?).ok()?;
        // An arbitrary module initializer can rewrite even a known class.
        if !facts.body.iter().all(|statement| match &statement.kind {
            StatementKind::Import(_) => true,
            StatementKind::Define(definition) => {
                definition.name.text != "__getattr__" && plain_definition(definition)
            }
            StatementKind::Assign {
                value: Some(value), ..
            } => alias_expression(value) || literal(value),
            StatementKind::Evaluate(value) => matches!(value.kind, ExpressionKind::Text(_)),
            _ => false,
        }) {
            return None;
        }
        Some(Module {
            name: name.into(),
            package,
            facts,
        })
    }

    fn locate(&self, name: &str) -> Option<(PathBuf, bool)> {
        let children = name.split('.').collect::<Vec<_>>();
        if children.len() > MAX_DEPTH
            || !children
                .iter()
                .all(|part| skarmory_python::simple_name(part))
        {
            return None;
        }
        for root in &self.roots {
            if opaque_or_unreadable(root, children[0])? {
                return None;
            }
            let mut directory = root.clone();
            let mut selected = false;
            for (index, child) in children.iter().enumerate() {
                if index > 0 && opaque_or_unreadable(&directory, child)? {
                    return None;
                }
                let package = directory.join(child);
                let initializer = package.join("__init__.py");
                let source_path = directory.join(format!("{child}.py"));
                let package_meta = fs::symlink_metadata(&package).ok();
                if package_meta
                    .as_ref()
                    .is_some_and(|meta| meta.file_type().is_symlink())
                {
                    return None;
                }
                let is_directory = package_meta.is_some_and(|meta| meta.is_dir());
                if is_directory && opaque_or_unreadable(&package, "__init__")? {
                    return None;
                }
                let package_selected = is_directory
                    && fs::symlink_metadata(&initializer).is_ok_and(|meta| meta.is_file());
                let source_selected =
                    fs::symlink_metadata(&source_path).is_ok_and(|meta| meta.is_file());
                if fs::symlink_metadata(&source_path)
                    .is_ok_and(|meta| meta.file_type().is_symlink())
                {
                    return None;
                }
                if !package_selected && !source_selected {
                    if selected || fs::symlink_metadata(&initializer).is_ok() {
                        return None;
                    }
                    break;
                }
                if index + 1 == children.len() {
                    return Some(if package_selected {
                        (initializer, true)
                    } else {
                        (source_path, false)
                    });
                }
                // A selected regular package (or module) owns its subtree.
                if !package_selected {
                    return None;
                }
                selected = true;
                directory = package;
            }
        }
        None
    }

    fn binding(
        &self,
        module: &Rc<Module>,
        name: &str,
        before: Option<usize>,
        depth: usize,
    ) -> Option<Binding> {
        if depth >= MAX_DEPTH {
            return None;
        }
        let mut selected = None;
        for (index, statement) in module
            .facts
            .body
            .iter()
            .enumerate()
            .take(before.unwrap_or(usize::MAX))
        {
            let matches = match &statement.kind {
                StatementKind::Define(definition) => definition.name.text == name,
                StatementKind::Import(import) => import.local.text == name,
                StatementKind::Assign { names, .. } => {
                    names.iter().any(|binding| binding.text == name)
                }
                _ => false,
            };
            if matches && selected.replace((index, &statement.kind)).is_some() {
                return None;
            }
        }
        match selected {
            Some((index, StatementKind::Define(definition)))
                if definition.kind == DefinitionKind::Class =>
            {
                self.class_definition(module, index, definition, depth + 1)
            }
            Some((index, StatementKind::Define(definition))) if plain_definition(definition) => {
                Some(Binding::Function(module.clone(), index))
            }
            Some((_, StatementKind::Import(import))) => self.import(module, import, depth + 1),
            Some((
                index,
                StatementKind::Assign {
                    value: Some(value), ..
                },
            )) => self.expression(module, value, index, depth + 1),
            None if before.is_some() && name == "type" => Some(Binding::Type),
            _ => None,
        }
    }

    fn import(&self, module: &Module, import: &Import, depth: usize) -> Option<Binding> {
        let target = if import.level == 0 {
            import.module.clone()
        } else {
            relative_module(module, import)?
        };
        if let Some(member) = &import.member {
            let source = self.module(&target)?;
            // A package initializer can attach its own separately sourced child.
            if target == module.name {
                return self.child_module(&source, member);
            }
            self.binding(&source, member, None, depth + 1)
                .or_else(|| self.child_module(&source, member))
        } else {
            self.module(&target)?;
            let visible = if import.exposes_top_level {
                target.split('.').next()?.into()
            } else {
                target
            };
            Some(Binding::Module(visible))
        }
    }

    fn child_module(&self, module: &Module, member: &str) -> Option<Binding> {
        if !module.package
            || module
                .facts
                .body
                .iter()
                .any(|statement| match &statement.kind {
                    StatementKind::Define(item) => {
                        item.name.text == member || item.name.text == "__getattr__"
                    }
                    StatementKind::Import(import) => {
                        import.local.text == member && import.member.as_deref() != Some(member)
                    }
                    StatementKind::Assign { names, .. } => {
                        names.iter().any(|name| name.text == member)
                    }
                    _ => false,
                })
        {
            return None;
        }
        let child = format!("{}.{member}", module.name);
        self.module(&child)?;
        Some(Binding::Module(child))
    }

    fn expression(
        &self,
        module: &Rc<Module>,
        expression: &Expression,
        before: usize,
        depth: usize,
    ) -> Option<Binding> {
        if depth >= MAX_DEPTH {
            return None;
        }
        match &expression.kind {
            ExpressionKind::Name(name) => self.binding(module, name, Some(before), depth + 1),
            ExpressionKind::Member(receiver, name) => {
                let Binding::Module(target) =
                    self.expression(module, receiver, before, depth + 1)?
                else {
                    return None;
                };
                self.binding(&self.module(&target)?, name, None, depth + 1)
            }
            ExpressionKind::Call {
                function,
                arguments,
                keywords,
            } if keywords.is_empty() => {
                let [meta, base] = arguments.as_slice() else {
                    return None;
                };
                let helper = self.expression(module, function, before, depth + 1)?;
                if !identity_metaclass_helper(helper)
                    || !matches!(
                        self.expression(module, meta, before, depth + 1),
                        Some(Binding::TypeSubclass)
                    )
                {
                    return None;
                }
                self.expression(module, base, before, depth + 1)
            }
            _ => None,
        }
    }

    fn class_definition(
        &self,
        module: &Rc<Module>,
        index: usize,
        definition: &Definition,
        depth: usize,
    ) -> Option<Binding> {
        if !plain_definition(definition) || definition.has_class_keywords() {
            return None;
        }
        let mut members = BTreeMap::new();
        let name = format!("{}.{}", module.name, definition.name.text);
        for statement in &definition.body {
            match &statement.kind {
                StatementKind::Define(member)
                    if member.kind != DefinitionKind::Class && plain_definition(member) =>
                {
                    members.insert(member.name.text.clone(), name.clone());
                }
                StatementKind::Assign {
                    names,
                    value: Some(value),
                    annotation: None,
                } if literal(value) => {
                    members.extend(
                        names
                            .iter()
                            .map(|member| (member.text.clone(), name.clone())),
                    );
                }
                StatementKind::Evaluate(value) if matches!(value.kind, ExpressionKind::Text(_)) => {
                }
                _ => return None,
            }
        }
        match definition.bases.as_slice() {
            [] => {}
            [base] => match self.expression(module, base, index, depth + 1)? {
                Binding::Class(base) => {
                    let mut inherited = base.members;
                    inherited.extend(members);
                    members = inherited;
                }
                Binding::Type if members.is_empty() => return Some(Binding::TypeSubclass),
                _ => return None,
            },
            _ => return None,
        }
        Some(Binding::Class(Class { members }))
    }
}

fn opaque_or_unreadable(directory: &Path, component: &str) -> Option<bool> {
    for (index, entry) in fs::read_dir(directory).ok()?.enumerate() {
        if index >= MAX_DIRECTORY_ENTRIES {
            return None;
        }
        let entry = entry.ok()?;
        if entry.file_name().to_str().is_some_and(|name| {
            name.split_once('.')
                .is_some_and(|(stem, _)| stem == component)
        }) && opaque_python_module(&entry.path())
        {
            return Some(true);
        }
    }
    Some(false)
}

fn relative_module(module: &Module, import: &Import) -> Option<String> {
    let package = if module.package {
        module.name.as_str()
    } else {
        module.name.rsplit_once('.')?.0
    };
    let mut parts = package.split('.').collect::<Vec<_>>();
    let keep = parts.len().checked_sub(import.level as usize - 1)?;
    if keep == 0 {
        return None;
    }
    parts.truncate(keep);
    if !import.module.is_empty() {
        parts.push(&import.module);
    }
    Some(parts.join("."))
}

fn plain_definition(definition: &Definition) -> bool {
    definition.decorators.is_empty()
        && definition.defaults.is_empty()
        && definition.annotations.is_empty()
}

fn alias_expression(expression: &Expression) -> bool {
    match &expression.kind {
        ExpressionKind::Name(_) => true,
        ExpressionKind::Member(value, _) => alias_expression(value),
        _ => false,
    }
}

fn literal(expression: &Expression) -> bool {
    matches!(
        expression.kind,
        ExpressionKind::Text(_)
            | ExpressionKind::Scalar(_)
            | ExpressionKind::None
            | ExpressionKind::Bool(_)
    )
}

fn identity_metaclass_helper(binding: Binding) -> bool {
    let Binding::Function(module, index) = binding else {
        return false;
    };
    let StatementKind::Define(definition) = &module.facts.body[index].kind else {
        return false;
    };
    if definition.kind != DefinitionKind::Function
        || definition.is_generator
        || !definition.has_variadic_positional_parameter
        || definition.has_variadic_keyword_parameter
        || definition.positional_parameter_count != 1
    {
        return false;
    }
    let [meta, bases] = definition.parameters.as_slice() else {
        return false;
    };
    let [statement] = definition.body.as_slice() else {
        return false;
    };
    let StatementKind::Return(Some(value)) = &statement.kind else {
        return false;
    };
    let ExpressionKind::Call {
        function,
        arguments,
        keywords,
    } = &value.kind
    else {
        return false;
    };
    matches!(&function.kind, ExpressionKind::Name(name) if name == &meta.text)
        && keywords.is_empty()
        && matches!(arguments.as_slice(), [name, supplied, namespace]
            if matches!(&name.kind, ExpressionKind::Text(text) if skarmory_python::simple_name(text))
                && matches!(&supplied.kind, ExpressionKind::Name(name) if name == &bases.text)
                && matches!(&namespace.kind, ExpressionKind::Dictionary(items) if items.is_empty()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;
    use serde_json::json;

    fn write(root: &Path, path: &str, source: &str) {
        let file = root.join(path);
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(file, source).unwrap();
    }

    #[test]
    fn project_venv_precedence_and_vendored_exclusion_are_independent() {
        let directory = tempfile::tempdir().unwrap();
        let project = directory.path().join("project");
        let selected = project.join(".venv/lib/python3.11/site-packages");
        let stale = project.join("venv/lib/python3.12/site-packages");
        let vendored = project.join("vendor/site-packages");
        let scanner = directory.path().join("scanner/site-packages");
        for root in [&selected, &stale, &vendored, &scanner] {
            fs::create_dir_all(root).unwrap();
        }
        let supplied = [vendored, scanner.clone(), scanner.clone()];
        assert_eq!(
            search_roots(&project, &supplied),
            [selected.canonicalize().unwrap()]
        );
        fs::remove_dir_all(project.join(".venv")).unwrap();
        fs::remove_dir_all(project.join("venv")).unwrap();
        assert_eq!(
            search_roots(&project, &supplied),
            [scanner.canonicalize().unwrap()]
        );
    }

    #[test]
    fn reads_exported_class_members_without_executing_initializers_and_detects_edits() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        write(
            root,
            "package/__init__.py",
            "from .base import Base as Public\n",
        );
        write(root, "package/base.py", "class Base:\n    NAME = 'base'\n");
        write(root, "cycle/__init__.py", "from .next import Loop\n");
        write(root, "cycle/next.py", "from . import Loop\n");
        write(
            root,
            "opaque.py",
            "open('executed', 'w').close()\nclass Base: pass\n",
        );
        let index = ExternalClassIndex::new(vec![root.to_owned()]);
        assert_eq!(
            index.class("package.Public").unwrap().members["NAME"],
            "package.base.Base"
        );
        assert!(index.class("cycle.Loop").is_none());
        assert!(index.class("opaque.Base").is_none());
        assert!(!root.join("executed").exists());
        write(
            root,
            "package/base.py",
            "class Base:\n    different_name = 'base'\n",
        );
        assert_eq!(index.changed_sources(), [root.join("package/base.py")]);
        let next = ExternalClassIndex::new(vec![root.to_owned()]);
        assert!(
            !next
                .class("package.Public")
                .unwrap()
                .members
                .contains_key("NAME")
        );
    }

    #[test]
    fn retains_dependency_overrides_and_rebuilds_metadata_after_warm_scans() {
        use super::super::{Options, Profile, scan};

        let directory = tempfile::tempdir().unwrap();
        let cache = directory.path().join("cache");
        let project = directory.path().join("project");
        let venv = project.join(".venv/lib/python3.11/site-packages");
        write(
            &project,
            "app.py",
            "from fakelib.core import Base\nclass App(Base):\n    NAME = 'app'\n    orphan = 1\n    def render(self): return None\n    def stale(self): return None\nentry = App()\n",
        );
        write(&venv, "fakelib/__init__.py", "");
        write(
            &venv,
            "fakelib/core.py",
            "class Base:\n    NAME = 'base'\n    def render(self): return None\n",
        );
        let inspect = |workers, cache_dir| -> serde_json::Value {
            let options = Options {
                profile: Profile::SkylosPhase03,
                grep_verify: Some(false),
                workers,
                cache_dir,
                ..Options::default()
            };
            let report = scan(&project, &options).unwrap().to_json().unwrap();
            let mut parsed: serde_json::Value = serde_json::from_str(&report).unwrap();
            parsed.as_object_mut().unwrap().remove("cache");
            parsed
        };

        let before = inspect(1, None);
        assert_eq!(before["coverage"]["status"], "complete", "{before}");
        assert_eq!(before["selected_paths"], json!(["app.py"]));
        let findings = |report: &serde_json::Value, category: &str| {
            report[category]
                .as_array()
                .unwrap()
                .iter()
                .map(|row| row["full_name"].as_str().unwrap().to_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(findings(&before, "unused_functions"), ["app.App.stale"]);
        let unused = findings(&before, "unused_variables");
        assert!(unused.contains(&"app.App.orphan".into()));
        assert!(!unused.contains(&"app.App.NAME".into()));
        assert_eq!(inspect(4, Some(cache.clone())), before);
        assert_eq!(inspect(4, Some(cache.clone())), before);

        write(&venv, "fakelib/core.py", "class Base: pass\n");
        let after = inspect(1, None);
        assert_eq!(inspect(4, Some(cache)), after);
        assert_eq!(
            findings(&after, "unused_functions"),
            ["app.App.render", "app.App.stale"]
        );
        assert!(findings(&after, "unused_variables").contains(&"app.App.NAME".into()));

        write(&project, "fakelib/__init__.py", "");
        write(&project, "fakelib/core.py", "class Base: pass\n");
        write(
            &venv,
            "fakelib/core.py",
            "class Base:\n    NAME = 'base'\n    def render(self): return None\n",
        );
        let shadow = inspect(1, None);
        assert_eq!(shadow["coverage"]["status"], "complete", "{shadow}");
        assert!(
            !shadow
                .as_object()
                .unwrap()
                .contains_key("external_dependency_roots")
        );
        assert!(findings(&shadow, "unused_functions").contains(&"app.App.render".into()));
    }

    #[test]
    fn external_base_requires_its_parent_initializer_to_complete() {
        use super::super::{Options, Profile, scan};

        let directory = tempfile::tempdir().unwrap();
        let project = directory.path();
        let venv = project.join(".venv/lib/python3.11/site-packages");
        write(
            project,
            "app.py",
            "from fakelib.core import Base\nclass App(Base): pass\napp = App()\n",
        );
        write(&venv, "fakelib/core.py", "class Base: pass\n");
        let options = Options {
            profile: Profile::SkylosPhase03,
            grep_verify: Some(false),
            ..Options::default()
        };
        for initializer in [
            "raise RuntimeError('blocked')\n",
            "import missing_dependency\n",
        ] {
            write(&venv, "fakelib/__init__.py", initializer);
            let report = scan(project, &options).unwrap().to_json().unwrap();
            let parsed: serde_json::Value = serde_json::from_str(&report).unwrap();
            assert_eq!(parsed["coverage"]["status"], "incomplete", "{report}");
            assert!(
                parsed["coverage"]["issues"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|row| row["kind"] == "unresolved_subclass_hook"),
                "{report}"
            );
        }
    }

    #[derive(Deserialize)]
    struct Request {
        project: Option<PathBuf>,
        paths: Vec<PathBuf>,
        queries: BTreeMap<String, Vec<String>>,
    }

    #[test]
    #[ignore = "executed by the pinned dependency metadata replay"]
    fn pinned_external_class_metadata() {
        let request_path = PathBuf::from(
            std::env::var_os("SKARMORY_PHASE03_EXTERNAL_REQUEST")
                .expect("dependency metadata request path"),
        );
        let response_path = PathBuf::from(
            std::env::var_os("SKARMORY_PHASE03_EXTERNAL_RESPONSE")
                .expect("dependency metadata response path"),
        );
        let request: Request = serde_json::from_slice(&fs::read(request_path).unwrap()).unwrap();
        let roots = request.project.as_ref().map_or_else(
            || request.paths.clone(),
            |project| search_roots(project, &request.paths),
        );
        let index = ExternalClassIndex::new(roots.clone());
        let classes = request
            .queries
            .keys()
            .map(|target| {
                let members = index.class(target).map(|class| class.members);
                (target, members)
            })
            .collect::<BTreeMap<_, _>>();
        assert!(index.changed_sources().is_empty());
        fs::write(
            response_path,
            serde_json::to_vec(&json!({
                "roots": roots,
                "classes": classes,
                "sources": index.input_stamps().keys().collect::<Vec<_>>()
            }))
            .unwrap(),
        )
        .unwrap();
    }
}
