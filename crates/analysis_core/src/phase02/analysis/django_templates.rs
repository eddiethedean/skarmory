//! Evidence from selected Django templates and source-backed context.

use std::cell::RefCell;
use std::fs;
use std::io::ErrorKind;
use std::path::{Component, PathBuf};
use std::sync::Arc;

use super::*;
use crate::phase02::config::Selection;
use crate::preview_source::{Project, Stamp, display, read_snapshot, reject_symlink_components};

mod markup;

const MAX_INPUTS: usize = 256;
const MAX_INPUT_BYTES: usize = 4 * 1024 * 1024;
const MAX_TEMPLATE_DEPTH: usize = 16;

pub(in crate::phase02) struct TemplateProject<'a> {
    project: &'a Project,
    selection: &'a Selection,
    snapshot: RefCell<TemplateSnapshot>,
}

#[derive(Default)]
struct TemplateSnapshot {
    paths: BTreeMap<PathBuf, Option<Stamp>>,
    contents: BTreeMap<PathBuf, Arc<str>>,
    bytes: usize,
}

impl<'a> TemplateProject<'a> {
    pub(in crate::phase02) fn new(project: &'a Project, selection: &'a Selection) -> Self {
        Self {
            project,
            selection,
            snapshot: RefCell::default(),
        }
    }

    fn relative_directory(&self, value: &str) -> Option<PathBuf> {
        let path = Path::new(value);
        let relative = if path.is_absolute() {
            path.strip_prefix(self.project.base()).ok()?
        } else {
            path
        };
        relative
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
            .then(|| relative.to_owned())
    }

    fn load(&self, directories: &[PathBuf], name: &str) -> Result<Option<Arc<str>>, Issue> {
        let name = Path::new(name);
        if name.as_os_str().is_empty()
            || !name
                .components()
                .all(|part| matches!(part, Component::Normal(_)))
        {
            return Err(Issue::new(
                "unresolved_django_template",
                ".",
                "template name is outside the project loader roots",
            ));
        }
        let base = self.project.base();
        let mut snapshot = self.snapshot.borrow_mut();
        let mut selected = None;
        for directory in directories {
            let path = base.join(directory).join(name);
            let label = self.project.label(&path);
            if !snapshot.paths.contains_key(&path) {
                if snapshot.paths.len() >= MAX_INPUTS {
                    return Err(Issue::new(
                        "analysis_limit",
                        &label,
                        "Django template lookup limit exceeded",
                    ));
                }
                let stamp = template_stamp(base, &path, &label)?;
                snapshot.paths.insert(path.clone(), stamp);
            }
            if snapshot.paths[&path].is_some() {
                if selected.is_some() {
                    // Django can select amongst these files by loader order.
                    // The compatibility profile requires an unambiguous file.
                    return Ok(None);
                }
                selected = Some((path, label));
            }
        }
        let Some((path, label)) = selected else {
            return Ok(None);
        };
        if self.selection.exclusion(&label).is_some() {
            return Ok(None);
        }
        if let Some(source) = snapshot.contents.get(&path) {
            return Ok(Some(Arc::clone(source)));
        }
        let (bytes, _) = read_snapshot(&path, &label, snapshot.paths[&path].as_ref())?;
        snapshot.bytes += bytes.len();
        if snapshot.bytes > MAX_INPUT_BYTES {
            return Err(Issue::new(
                "analysis_limit",
                &label,
                "Django template byte limit exceeded",
            ));
        }
        let source: Arc<str> = String::from_utf8(bytes)
            .map_err(|_| Issue::new("read_error", &label, "Django template is not UTF-8"))?
            .into();
        snapshot.contents.insert(path, Arc::clone(&source));
        Ok(Some(source))
    }

    pub(in crate::phase02) fn input_digests(&self) -> BTreeMap<String, blake3::Hash> {
        self.snapshot
            .borrow()
            .contents
            .iter()
            .map(|(path, source)| (display(path), blake3::hash(source.as_bytes())))
            .collect()
    }

    pub(in crate::phase02) fn verify(self) -> Vec<Issue> {
        self.snapshot
            .into_inner()
            .paths
            .into_iter()
            .filter_map(|(path, previous)| {
                let label = self.project.label(&path);
                match template_stamp(self.project.base(), &path, &label) {
                    Ok(current) if current == previous => None,
                    Ok(_) => Some(Issue::new(
                        "source_changed",
                        &label,
                        "Django template changed after lookup",
                    )),
                    Err(issue) => Some(issue),
                }
            })
            .collect()
    }
}

fn template_stamp(base: &Path, path: &Path, label: &str) -> Result<Option<Stamp>, Issue> {
    reject_symlink_components(base, path)
        .map_err(|message| Issue::new("unsupported_file", label, message))?;
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() => Ok(Some(Stamp::new(&metadata))),
        Ok(_) => Err(Issue::new(
            "unsupported_file",
            label,
            "expected a regular Django template file",
        )),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(Issue::new("read_error", label, error)),
    }
}

const VIEW_HOOKS: &[&str] = &[
    "__new__",
    "__init__",
    "__getattribute__",
    "__getattr__",
    "as_view",
    "setup",
    "dispatch",
    "get",
    "get_object",
    "get_queryset",
    "get_context_data",
    "get_context_object_name",
    "get_template_names",
    "render_to_response",
    "get_slug_field",
    "queryset",
    "response_class",
    "template_engine",
];

impl Graph<'_> {
    pub(super) fn django_template_roots(
        &mut self,
        reach: &Reachability,
        project: &TemplateProject<'_>,
    ) -> Vec<Edge> {
        let settings: BTreeSet<_> = self
            .django_settings_sources
            .iter()
            .filter(|(scope, _)| reach.scopes.contains(scope))
            .map(|(_, settings)| *settings)
            .collect();
        let mut roots = Vec::new();
        for settings in settings {
            let classes = self.django_template_views(settings, reach);
            if classes.is_empty() {
                continue;
            }
            let Some(config) = self.django_template_setting(settings, reach) else {
                continue;
            };
            let Some(directories) = self.django_template_directories(project, settings, &config)
            else {
                continue;
            };
            let Some(apps) = self.django_template_apps.get(&settings).cloned() else {
                continue;
            };
            for class in classes {
                let Some((model, template, aliases)) =
                    self.django_detail_context(class, settings, &apps)
                else {
                    continue;
                };
                match markup::references(project, &directories, &template, aliases) {
                    Ok(members) => {
                        for member in members {
                            if let Some(target) =
                                self.django_template_method(model, &member, settings)
                            {
                                roots.push(Edge {
                                    target,
                                    kind: "django_template_method",
                                    certainty: "possible",
                                });
                            }
                        }
                    }
                    Err(issue) => self.issues.push(issue),
                }
            }
        }
        roots
    }

    fn django_template_views(
        &mut self,
        settings: ScopeId,
        reach: &Reachability,
    ) -> BTreeSet<SymbolId> {
        let Some(binding) = self.local(settings, "ROOT_URLCONF", END) else {
            return BTreeSet::new();
        };
        let Value::Text(urlconf) = self.binding(binding, END, 0) else {
            return BTreeSet::new();
        };
        let Some(&file) = self.modules.get(urlconf.as_str()) else {
            return BTreeSet::new();
        };
        let mut visited = BTreeSet::new();
        let mut pending = vec![framework_module(file)];
        let mut classes = BTreeSet::new();
        while let Some(object) = pending.pop() {
            if !reach.active_objects.contains(&object) || !visited.insert(object) {
                continue;
            }
            let row = &self.scopes[object.0];
            classes.extend(row.routes.iter().filter_map(|&(root, target, kind)| {
                (root == object
                    && kind == "django_registration"
                    && self.is_class(target)
                    && reach.symbols.contains(&target))
                .then_some(target)
            }));
            pending.extend(
                row.mounts
                    .iter()
                    .filter_map(|&(parent, child)| (parent == object).then_some(child)),
            );
        }
        classes
    }

    fn django_template_directories(
        &self,
        project: &TemplateProject<'_>,
        settings: ScopeId,
        config: &Value,
    ) -> Option<Vec<PathBuf>> {
        let Value::Sequence(engines) = config else {
            return None;
        };
        let [engine] = engines.as_slice() else {
            return None;
        };
        if !matches!(setting(engine, "BACKEND"), Some(Value::Text(name))
            if name == "django.template.backends.django.DjangoTemplates")
        {
            return None;
        }
        if let Some(options) = setting(engine, "OPTIONS")
            && (!matches!(options, Value::Mapping(_))
                || ["loaders", "libraries", "builtins", "file_charset"]
                    .iter()
                    .any(|key| setting(options, key).is_some()))
        {
            return None;
        }
        let mut directories = match setting(engine, "DIRS") {
            None => Vec::new(),
            Some(Value::Sequence(dirs)) => dirs
                .iter()
                .map(|item| match item {
                    Value::Text(path) => project.relative_directory(path),
                    Value::ProjectPath(path) => Some(path.clone()),
                    _ => None,
                })
                .collect::<Option<Vec<_>>>()?,
            _ => return None,
        };
        match setting(engine, "APP_DIRS") {
            Some(Value::Bool(true)) => {
                for (&app, label) in self.django_template_apps.get(&settings)? {
                    label.as_ref()?;
                    let path = Path::new(&self.files[app].path);
                    if !self.files[app].is_package {
                        return None;
                    }
                    directories.push(path.parent()?.join("templates"));
                }
            }
            Some(Value::Bool(false)) | None => {}
            _ => return None,
        }
        directories.sort();
        directories.dedup();
        Some(directories)
    }

    fn django_detail_context(
        &mut self,
        class: SymbolId,
        settings: ScopeId,
        apps: &BTreeMap<usize, Option<String>>,
    ) -> Option<(SymbolId, String, BTreeSet<String>)> {
        let (lineage, _) = self.class_lineage(class);
        let last = *lineage.last()?;
        let leaf = self.scopes[last].owner?;
        let binding = &self.symbols[leaf];
        let Binding::Definition(definition, _) = binding.binding else {
            return None;
        };
        let [base] = definition.bases.as_slice() else {
            return None;
        };
        if !matches!(self.class_base(binding.scope, base, definition.name.at, 0),
            Value::Provider(name) if matches!(name.as_str(), "django.views.generic.DetailView" | "django.views.generic.detail.DetailView"))
        {
            return None;
        }
        if self.django_template_overrides(&lineage, settings, VIEW_HOOKS) {
            return None;
        }
        let Value::Callable(invocation) =
            self.django_template_member(&lineage, settings, "model")?
        else {
            return None;
        };
        let model = invocation.target;
        if !self.is_class(model)
            || self.framework_class_role(model, &mut BTreeSet::new(), 0)
                != Some(FrameworkClassRole::DjangoModel)
        {
            return None;
        }
        let model_name = self.symbols[model].name.text.to_lowercase();
        let model_module = &self.files[self.scopes[self.symbols[model].scope].file].module;
        let (_, app) = apps
            .iter()
            .filter_map(|(&app, label)| {
                let module = &self.files[app].module;
                model_module
                    .strip_prefix(module)
                    .filter(|rest| rest.starts_with('.'))?;
                Some((module.len(), label))
            })
            .max_by_key(|&(length, _)| length)?;
        let app = app.as_ref()?;
        let (model_lineage, _) = self.class_lineage(model);
        if self.django_template_overrides(
            &model_lineage,
            settings,
            &[
                "Meta",
                "_meta",
                "__getattribute__",
                "__getattr__",
                "__getitem__",
            ],
        ) {
            return None;
        }
        let suffix = match self.django_template_member(&lineage, settings, "template_name_suffix") {
            None => "_detail".to_owned(),
            Some(Value::Text(suffix)) => suffix,
            _ => return None,
        };
        let template = match self.django_template_member(&lineage, settings, "template_name") {
            None => format!("{app}/{model_name}{suffix}.html"),
            Some(Value::Text(name)) if !name.is_empty() => name,
            _ => return None,
        };
        if self
            .django_template_member(&lineage, settings, "template_name_field")
            .is_some()
        {
            return None;
        }
        let name = match self.django_template_member(&lineage, settings, "context_object_name") {
            None => model_name,
            Some(Value::Text(name)) if skarmory_python::simple_name(&name) => name,
            _ => return None,
        };
        let mut aliases = BTreeSet::from(["object".to_owned(), name]);
        match self.django_template_member(&lineage, settings, "extra_context") {
            None => {}
            Some(Value::Mapping(items)) => {
                for [key, _] in items.as_chunks::<2>().0 {
                    let Value::Text(key) = key else { return None };
                    aliases.remove(key);
                }
            }
            _ => return None,
        }
        Some((model, template, aliases))
    }

    fn django_template_overrides(
        &self,
        lineage: &[ScopeId],
        settings: ScopeId,
        names: &[&str],
    ) -> bool {
        lineage.iter().any(|&body| {
            let class = self.scopes[body].owner.expect("class body owner");
            let Binding::Definition(definition, _) = self.symbols[class].binding else {
                unreachable!()
            };
            !definition.decorators.is_empty()
                || names.iter().any(|name| {
                    self.local(body, name, END).is_some()
                        || self.class_discovery_write(class, name, settings).is_some()
                })
        })
    }

    fn django_template_member(
        &mut self,
        lineage: &[ScopeId],
        settings: ScopeId,
        name: &str,
    ) -> Option<Value> {
        for &body in lineage {
            let class = self.scopes[body].owner.expect("class body owner");
            if self.class_discovery_write(class, name, settings).is_some() {
                return Some(Value::Unknown);
            }
            if let Some(binding) = self.local(body, name, END) {
                return Some(self.binding(binding, END, 0));
            }
        }
        None
    }

    fn django_template_method(
        &mut self,
        model: SymbolId,
        name: &str,
        settings: ScopeId,
    ) -> Option<SymbolId> {
        let (lineage, _) = self.class_lineage(model);
        let Value::Callable(invocation) = self.django_template_member(&lineage, settings, name)?
        else {
            return None;
        };
        let target = invocation.target;
        let Binding::Definition(definition, _) = self.symbols[target].binding else {
            return None;
        };
        if definition.kind != DefinitionKind::Function
            || definition.is_generator
            || !definition.decorators.is_empty()
            || !definition.simple_positional_parameters
            || definition.positional_parameter_count == 0
            || definition.positional_parameter_count - 1 > definition.defaults.len()
            || self.receiver_parameter(target).is_none()
        {
            return None;
        }
        self.method_receivers
            .entry(target)
            .or_default()
            .insert(model);
        Some(target)
    }
}

fn setting<'a>(mapping: &'a Value, name: &str) -> Option<&'a Value> {
    let Value::Mapping(entries) = mapping else {
        return None;
    };
    entries
        .as_chunks::<2>()
        .0
        .iter()
        .rev()
        .find_map(|[key, value]| matches!(key, Value::Text(key) if key == name).then_some(value))
}
