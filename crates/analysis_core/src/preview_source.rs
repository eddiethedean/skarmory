use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, FileType, Metadata};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Instant, SystemTime};

use rayon::prelude::*;
use serde::Serialize;
use walkdir::WalkDir;

use crate::phase02::config::Selection;
use crate::{Issue, source};

const MAX_SELECTED_FILES: usize = 16_384;
const MAX_SNAPSHOT_BYTES: u64 = 32 * 1024 * 1024;
static RESERVED_WORKERS: AtomicUsize = AtomicUsize::new(0);

pub(super) enum FileEntry<'a> {
    None,
    Default,
    Explicit(&'a Path),
}

#[derive(Debug, Serialize)]
pub(super) struct Exclusion {
    pub(super) path: String,
    pub(super) reason: &'static str,
}

const CONVENTIONAL_PYTHON_ROOTS: &[&str] = &["src", "lib", "python"];

pub(super) struct Project {
    pub files: Vec<SourceFile>,
    pub roots: Vec<String>,
    pub import_roots: Vec<String>,
    pub exclusions: Vec<Exclusion>,
    pub issues: Vec<Issue>,
    pub script: Option<usize>,
    base: PathBuf,
    search: Vec<PathBuf>,
    files_by_path: BTreeMap<PathBuf, usize>,
    directories: BTreeMap<PathBuf, DirectoryEntries>,
    ambiguous: BTreeSet<usize>,
    include_javascript: bool,
    selected_package: bool,
}

pub(super) struct SourceFile {
    pub label: String,
    pub name: String,
    pub loose_name: bool,
    path: PathBuf,
    discovered: Option<Stamp>,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct ResolvedModule {
    pub file: usize,
    pub package: bool,
}

#[derive(Clone, Copy, Debug)]
pub(super) enum LookupError {
    Missing,
    Namespace,
    Ambiguous,
}

pub(super) struct ProjectWork<T> {
    pub files: Vec<Option<FileWork<T>>>,
    pub javascript_budget_issue: Option<Issue>,
}

pub(super) struct FileWork<T> {
    pub result: Result<T, Issue>,
    pub read_ns: u128,
    pub extract_ns: u128,
    pub stamp: Option<Stamp>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct Stamp {
    len: u64,
    modified: Option<SystemTime>,
    created: Option<SystemTime>,
    #[cfg(unix)]
    identity: (u64, u64, i64, i64),
}

type DirectoryEntries = BTreeSet<(OsString, SeenEntry)>;

#[derive(Clone, Copy, Eq, Ord, PartialEq, PartialOrd)]
enum SeenEntry {
    Directory,
    LinkedDirectory,
    SourceFile,
    LinkedSource,
    OtherSource,
    OpaquePythonModule,
    JavaScriptContext,
}

impl Stamp {
    pub(super) fn modified(&self) -> Option<SystemTime> {
        self.modified
    }

    pub(super) fn new(metadata: &Metadata) -> Self {
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;

        Self {
            len: metadata.len(),
            modified: metadata.modified().ok(),
            created: metadata.created().ok(),
            #[cfg(unix)]
            identity: (
                metadata.dev(),
                metadata.ino(),
                metadata.ctime(),
                metadata.ctime_nsec(),
            ),
        }
    }
}

pub(super) fn discover(path: &Path, roots: &[PathBuf], entry: FileEntry<'_>) -> Project {
    discover_filtered(path, roots, entry, None)
}

pub(super) fn discover_filtered(
    path: &Path,
    roots: &[PathBuf],
    entry: FileEntry<'_>,
    selection: Option<&Selection>,
) -> Project {
    discover_sources(path, roots, entry, selection, false, false)
}

pub(super) fn discover_sources(
    path: &Path,
    roots: &[PathBuf],
    entry: FileEntry<'_>,
    selection: Option<&Selection>,
    include_javascript: bool,
    infer_package: bool,
) -> Project {
    let mut project = Project {
        files: Vec::new(),
        roots: Vec::new(),
        import_roots: Vec::new(),
        exclusions: Vec::new(),
        issues: Vec::new(),
        script: None,
        base: PathBuf::new(),
        search: Vec::new(),
        files_by_path: BTreeMap::new(),
        directories: BTreeMap::new(),
        ambiguous: BTreeSet::new(),
        include_javascript,
        selected_package: false,
    };
    let path = normalized(path);
    let (input, metadata) = match checked_path(&path) {
        Ok(input) => input,
        Err(message) => {
            project
                .issues
                .push(Issue::new("discovery_error", &display(&path), message));
            return project;
        }
    };
    let input_file = metadata.is_file()
        && input
            .file_name()
            .is_some_and(|name| project.is_source(name));
    if !input_file && !metadata.is_dir() {
        project.issues.push(Issue::new(
            "discovery_error",
            &display(&path),
            if include_javascript {
                "expected a directory or a regular Python, JavaScript, or TypeScript file"
            } else {
                "expected a directory or a regular .py file"
            },
        ));
        return project;
    }
    project.base = if input_file {
        input.parent().expect("absolute file path").to_owned()
    } else {
        input.clone()
    };

    let entry_path = project.entry_path(input_file.then_some(&input), entry);
    let mut search = Vec::new();
    if let Some(script) = entry_path.as_ref().and_then(|path| path.parent()) {
        search.push(script.to_owned());
    }

    let mut selected_roots = Vec::new();
    for path in roots {
        let path = normalized(&project.base.join(path));
        let valid = reject_symlink_components(&project.base, &path).and_then(|()| {
            let (resolved, metadata) = checked_path(&path)?;
            if metadata.is_dir() {
                Ok(resolved)
            } else {
                Err("expected a source directory".into())
            }
        });
        match valid {
            Ok(path) if !selected_roots.contains(&path) => selected_roots.push(path),
            Ok(_) => {}
            Err(message) => project.issues.push(Issue::new(
                "discovery_error",
                &project.label(&path),
                message,
            )),
        }
    }
    if roots.is_empty() {
        selected_roots.push(project.base.clone());
    }
    for root in &selected_roots {
        project.roots.push(project.label(root));
        if (entry_path.is_none() || !roots.is_empty()) && !search.contains(root) {
            search.push(root.clone());
        }
    }
    project.import_roots = search.iter().map(|path| project.label(path)).collect();
    project.search = search;

    let mut selected = BTreeMap::new();
    if input_file {
        if let Some(reason) = selection
            .and_then(|selection| selection.explicit_file_exclusion(&project.label(&input)))
        {
            project.exclusions.push(Exclusion {
                path: project.label(&input),
                reason,
            });
        } else {
            selected.insert(input, Some(Stamp::new(&metadata)));
        }
    }
    if !input_file || !roots.is_empty() {
        for root in &selected_roots {
            project.walk(root, &mut selected, selection);
            if selected.len() > MAX_SELECTED_FILES {
                break;
            }
        }
    }
    let mut labels = BTreeMap::new();
    for (path, discovered) in selected {
        let index = project.files.len();
        let label = project.label(&path);
        if let Some(earlier) = labels.insert(label.clone(), index) {
            project.ambiguous.extend([earlier, index]);
            project.issues.push(Issue::new(
                "ambiguous_source_path",
                &label,
                "distinct source paths share a report label",
            ));
        }
        project.files_by_path.insert(path.clone(), index);
        project.files.push(SourceFile {
            label,
            name: String::new(),
            loose_name: false,
            path,
            discovered,
        });
    }
    if infer_package
        && !input_file
        && project
            .files_by_path
            .contains_key(&project.base.join("__init__.py"))
        && project
            .base
            .file_name()
            .and_then(OsStr::to_str)
            .is_some_and(skarmory_python::simple_name)
        && let Some(parent) = project.base.parent()
    {
        // The enclosing directory supplies the import prefix only. Keep the
        // scan and all labels relative to the selected package directory.
        project.search = vec![parent.to_owned()];
        project.import_roots = vec![project.label(parent)];
        project.selected_package = true;
        project.identify_files(&[], None, selection.is_some());
    } else if infer_package && !input_file {
        project.infer_python_source_roots();
        project.identify_files(&[], None, selection.is_some());
    } else {
        project.identify_files(&selected_roots, entry_path.as_deref(), selection.is_some());
    }
    if let Some(path) = entry_path {
        project.script = project.files_by_path.get(&path).copied();
        if project.script.is_none() {
            project.issues.push(Issue::new(
                "missing_entry",
                &project.label(&path),
                "entry file is outside the selected sources",
            ));
        }
    }
    project
        .exclusions
        .sort_by(|left, right| left.path.cmp(&right.path));
    project
        .exclusions
        .dedup_by(|left, right| left.path == right.path);
    project
}

impl Project {
    fn infer_python_source_roots(&mut self) {
        // Keep the established src layout. Infer other conventional roots
        // only when they contain a selected package and do not alias a
        // module or package that an earlier root already supplies.
        let mut roots = Vec::new();
        for name in CONVENTIONAL_PYTHON_ROOTS {
            let directory = self.base.join(name);
            let Some(entries) = self.directories.get(&directory) else {
                continue;
            };
            if entries.iter().any(|(name, _)| name == "__init__.py") {
                continue;
            }
            if *name != "src"
                && (!entries.iter().any(|(name, kind)| {
                    *kind == SeenEntry::Directory
                        && self
                            .files_by_path
                            .contains_key(&directory.join(name).join("__init__.py"))
                }) || entries.iter().any(|(entry, kind)| {
                    let Some(module) = entry_module_name(entry, *kind) else {
                        return true;
                    };
                    skarmory_python::simple_name(module)
                        && self
                            .search
                            .iter()
                            .chain(&roots)
                            .any(|root| !self.import_name_absent(root, module))
                }))
            {
                continue;
            }
            roots.push(directory);
        }
        roots.append(&mut self.search);
        self.import_roots = roots.iter().map(|root| self.label(root)).collect();
        self.search = roots;
    }

    pub(super) fn package_reference_boundary(&self, path: &str) -> bool {
        self.selected_package
            || self.import_roots.iter().any(|root| {
                CONVENTIONAL_PYTHON_ROOTS.contains(&root.as_str())
                    && Path::new(path).starts_with(root)
            })
    }

    fn entry_path(&mut self, input: Option<&PathBuf>, entry: FileEntry<'_>) -> Option<PathBuf> {
        let candidate = match entry {
            FileEntry::None => return None,
            FileEntry::Default => input.cloned().unwrap_or_else(|| self.base.join("main.py")),
            FileEntry::Explicit(path) => normalized(&self.base.join(path)),
        };
        let valid = reject_symlink_components(&self.base, &candidate).and_then(|()| {
            let (path, metadata) = checked_path(&candidate)?;
            if metadata.is_file() && path.file_name().is_some_and(source::is_python) {
                Ok(path)
            } else {
                Err("expected a regular .py entry file".into())
            }
        });
        match valid {
            Ok(path) => Some(path),
            Err(message) => {
                self.issues.push(Issue::new(
                    "missing_entry",
                    &self.label(&candidate),
                    message,
                ));
                None
            }
        }
    }

    fn walk(
        &mut self,
        root: &Path,
        selected: &mut BTreeMap<PathBuf, Option<Stamp>>,
        selection: Option<&Selection>,
    ) {
        let mut walk = WalkDir::new(root)
            .follow_links(false)
            .follow_root_links(false)
            .sort_by_file_name()
            .into_iter();
        while let Some(entry) = walk.next() {
            if selected.len() > MAX_SELECTED_FILES {
                break;
            }
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    self.issues.push(Issue::new(
                        "discovery_error",
                        &self.label(error.path().unwrap_or(root)),
                        error,
                    ));
                    continue;
                }
            };
            let path = entry.path();
            let kind = entry.file_type();
            let seen = seen_entry(path, kind, self.include_javascript);
            if entry.depth() > 0
                && let Some(seen) = seen
                && let Some(parent) = path
                    .parent()
                    .and_then(|path| self.directories.get_mut(path))
            {
                parent.insert((entry.file_name().to_owned(), seen));
            }
            if entry.depth() > 0
                && let Some(selection) = selection
                && let label = self.label(path)
                && let Some(reason) = selection.exclusion(&label)
                && (kind.is_dir() || self.is_source(entry.file_name()))
                && (!kind.is_dir() || !selection.may_include_children(&label))
            {
                self.exclusions.push(Exclusion {
                    path: label,
                    reason,
                });
                if kind.is_dir() {
                    walk.skip_current_dir();
                }
                continue;
            }
            if kind.is_symlink() {
                if entry.depth() == 0 {
                    self.issues.push(Issue::new(
                        "source_changed",
                        &self.label(path),
                        "source root became a symlink",
                    ));
                    continue;
                }
                if seen == Some(SeenEntry::LinkedDirectory) {
                    self.exclusions.push(Exclusion {
                        path: self.label(path),
                        reason: "symlink_directory",
                    });
                } else if self.is_source(entry.file_name()) {
                    let stamp = entry.metadata().ok().map(|metadata| Stamp::new(&metadata));
                    selected.insert(path.to_owned(), stamp);
                }
                continue;
            }
            if kind.is_dir() {
                if selection.is_none()
                    && entry.depth() > 0
                    && [".git", ".venv", "__pycache__"]
                        .iter()
                        .any(|name| entry.file_name() == *name)
                {
                    self.exclusions.push(Exclusion {
                        path: self.label(path),
                        reason: "excluded_directory",
                    });
                    walk.skip_current_dir();
                    continue;
                }
                self.directories.entry(path.to_owned()).or_default();
            } else if self.is_source(entry.file_name()) {
                let stamp = match entry.metadata() {
                    Ok(metadata) => Some(Stamp::new(&metadata)),
                    Err(error) => {
                        self.issues
                            .push(Issue::new("discovery_error", &self.label(path), error));
                        None
                    }
                };
                selected.entry(path.to_owned()).or_insert(stamp);
            }
        }
    }

    fn identify_files(&mut self, roots: &[PathBuf], script: Option<&Path>, loose_files: bool) {
        // Prefer declared source identities. A direct script also makes the files
        // in its directory importable as top-level modules.
        let lookup_roots = roots
            .iter()
            .chain(self.search.iter().filter(|root| !roots.contains(root)))
            .collect::<Vec<_>>();
        for index in 0..self.files.len() {
            let file = &self.files[index];
            if file
                .path
                .file_name()
                .is_some_and(|name| !source::is_python(name))
            {
                continue;
            }
            let mut namespace = false;
            let mut name = None;
            for root in &lookup_roots {
                match module_name(&file.path, root, &self.files_by_path) {
                    Ok(Some(module)) => {
                        name = Some(module);
                        break;
                    }
                    Ok(None) => {}
                    Err(LookupError::Namespace) => namespace = true,
                    Err(_) => {}
                }
            }
            if name.is_none() && script == Some(&file.path) {
                name = file
                    .path
                    .file_stem()
                    .and_then(|value| value.to_str())
                    .filter(|name| skarmory_python::simple_name(name))
                    .map(str::to_owned);
            }
            match name {
                Some(name) => self.files[index].name = name,
                None if loose_files => {
                    // A standalone source under a test or script directory
                    // still needs a stable identity without declaring a package.
                    self.files[index].name = file
                        .label
                        .strip_suffix(".py")
                        .unwrap_or(&file.label)
                        .replace('/', ".");
                    self.files[index].loose_name = true;
                }
                None => self.issues.push(if namespace {
                    Issue::new(
                        "unsupported_namespace_package",
                        &file.label,
                        "a source ancestor has no selected __init__.py",
                    )
                } else {
                    Issue::new(
                        "unsupported_module_name",
                        &file.label,
                        "expected ASCII Python module and package names",
                    )
                }),
            }
        }
    }

    pub(super) fn lookup(&self, name: &str) -> Result<ResolvedModule, LookupError> {
        let mut parts = name.split('.');
        let first = parts.next().ok_or(LookupError::Missing)?;
        let mut namespace = false;
        let mut target = None;
        for root in &self.search {
            match self.lookup_child(root, first) {
                Ok(module) => {
                    target = Some(module);
                    break;
                }
                Err(LookupError::Namespace) => namespace = true,
                Err(LookupError::Missing) => {}
                Err(error) => return Err(error),
            }
        }
        let mut target = target.ok_or(if namespace {
            LookupError::Namespace
        } else {
            LookupError::Missing
        })?;
        for part in parts {
            if !target.package {
                return Err(LookupError::Missing);
            }
            let parent = self.files[target.file]
                .path
                .parent()
                .expect("absolute source path");
            target = self.lookup_child(parent, part)?;
        }
        Ok(target)
    }

    fn lookup_child(&self, root: &Path, name: &str) -> Result<ResolvedModule, LookupError> {
        if !skarmory_python::simple_name(name) {
            return Err(LookupError::Missing);
        }
        let directory = root.join(name);
        let package = directory.join("__init__.py");
        let module = root.join(format!("{name}.py"));
        if let Some(&file) = self.files_by_path.get(&package) {
            self.resolve(file, true)
        } else if let Some(&file) = self.files_by_path.get(&module) {
            self.resolve(file, false)
        } else if self.directories.contains_key(&directory) {
            Err(LookupError::Namespace)
        } else {
            Err(LookupError::Missing)
        }
    }

    fn resolve(&self, file: usize, package: bool) -> Result<ResolvedModule, LookupError> {
        if self.ambiguous.contains(&file) {
            Err(LookupError::Ambiguous)
        } else {
            Ok(ResolvedModule { file, package })
        }
    }

    pub(super) fn map_files<T: Send>(
        &self,
        workers: usize,
        extract: impl Fn(&str) -> T + Sync,
    ) -> Result<Vec<FileWork<T>>, Issue> {
        let work = self.map_file_sources(workers, |_, source| extract(source))?;
        if let Some(issue) = work.javascript_budget_issue {
            return Err(issue);
        }
        Ok(work.files.into_iter().flatten().collect())
    }

    pub(super) fn map_file_sources<T: Send>(
        &self,
        workers: usize,
        extract: impl Fn(&SourceFile, &str) -> T + Sync,
    ) -> Result<ProjectWork<T>, Issue> {
        if workers == 0 {
            return Err(Issue::new("worker_budget", ".", "workers must be positive"));
        }
        if self.files.len() > MAX_SELECTED_FILES {
            return Err(Issue::new(
                "analysis_limit",
                ".",
                format!("source discovery exceeded {MAX_SELECTED_FILES} selected files"),
            ));
        }
        let mut python_bytes = 0;
        let mut python_count = 0;
        let mut javascript_bytes = 0;
        for file in &self.files {
            // Reserve a file's full read allowance when its metadata was unavailable.
            let bytes = file
                .discovered
                .as_ref()
                .map_or(source::MAX_SOURCE_BYTES, |stamp| {
                    stamp.len.min(source::MAX_SOURCE_BYTES)
                });
            if file.is_javascript() {
                javascript_bytes += bytes;
            } else {
                python_bytes += bytes;
                python_count += 1;
            }
        }
        if python_bytes > MAX_SNAPSHOT_BYTES {
            return Err(Issue::new(
                "analysis_limit",
                ".",
                "selected sources exceed the 32 MiB snapshot budget",
            ));
        }
        // Keep the combined read bound while preserving the Python work that
        // fits it. An incomplete JavaScript snapshot cannot support a file graph.
        let javascript_over_budget = javascript_bytes > MAX_SNAPSHOT_BYTES - python_bytes;
        let admitted = if javascript_over_budget {
            python_count
        } else {
            self.files.len()
        };
        let run = |file: &SourceFile| {
            (!javascript_over_budget || !file.is_javascript()).then(|| file.work(&extract))
        };

        let capacity = std::thread::available_parallelism().map_or(1, usize::from);
        let requested = workers.min(admitted);
        let files = if let Some(reservation) =
            WorkerReservation::acquire(&RESERVED_WORKERS, requested, capacity)
        {
            let pool = rayon::ThreadPoolBuilder::new()
                .num_threads(reservation.count)
                .build()
                .map_err(|error| Issue::new("worker_error", ".", error))?;
            pool.install(|| self.files.par_iter().map(run).collect())
        } else {
            self.files.iter().map(run).collect()
        };
        Ok(ProjectWork {
            files,
            javascript_budget_issue: javascript_over_budget.then(|| {
                Issue::new(
                    "analysis_limit",
                    ".",
                    "selected JavaScript and TypeScript sources exceed the space left in the 32 MiB snapshot budget",
                )
            }),
        })
    }

    pub(super) fn verify(&self, stamps: impl IntoIterator<Item = Option<Stamp>>) -> Vec<Issue> {
        let mut issues = Vec::new();
        for (file, stamp) in self.files.iter().zip(stamps) {
            if let Some(stamp) = stamp
                && !fs::symlink_metadata(&file.path)
                    .is_ok_and(|metadata| metadata.is_file() && Stamp::new(&metadata) == stamp)
            {
                issues.push(Issue::new(
                    "source_changed",
                    &file.label,
                    "source changed after its snapshot was read",
                ));
            }
        }
        for (directory, entries) in &self.directories {
            if !read_directory_entries(directory, self.include_javascript)
                .is_ok_and(|current| current == *entries)
            {
                issues.push(Issue::new(
                    "source_changed",
                    &self.label(directory),
                    "directory changed after source discovery",
                ));
            }
        }
        issues
    }

    pub(super) fn verification_directories(&self) -> impl Iterator<Item = &PathBuf> {
        self.directories.keys()
    }

    pub(super) fn direct_submodule_absent(
        &self,
        initializer: &str,
        module: &str,
        provider: &str,
    ) -> bool {
        let Some(package) = Path::new(initializer).parent() else {
            return false;
        };
        let Some(service) = package.parent() else {
            return false;
        };
        // Selection can hide a package initializer or a local framework shim
        // without changing Python's import path. Use the verified directory
        // snapshot to close both the constructor prefix and the attempted import.
        let has_no_import = |directory: &Path, names: &[&str]| {
            self.directories.get(directory).is_some_and(|entries| {
                !entries.iter().any(|(name, kind)| {
                    names
                        .iter()
                        .any(|module| entry_may_supply_module(name, *kind, module))
                })
            })
        };
        has_no_import(&self.base, &[provider])
            && self
                .search
                .iter()
                .all(|root| has_no_import(root, &[provider]))
            && has_no_import(&self.base.join(service), &["__init__", provider])
            && self.package_submodule_absent(package, module)
    }

    pub(super) fn root_package_submodule_absent(&self, initializer: &str, module: &str) -> bool {
        let Some(package) = Path::new(initializer).parent() else {
            return false;
        };
        package.components().count() == 1
            && package
                .to_str()
                .is_some_and(|name| self.no_other_import_root_supplies(name))
            && self.package_submodule_absent(package, module)
    }

    pub(super) fn root_package_source_unambiguous(
        &self,
        initializer: &str,
        source: &str,
        package: &str,
        module: &str,
    ) -> bool {
        !package.contains('.')
            && Path::new(initializer) == Path::new(package).join("__init__.py")
            && self.package_source_unambiguous(package, module, Some(source))
    }

    pub(super) fn package_source_unambiguous(
        &self,
        package: &str,
        module: &str,
        source: Option<&str>,
    ) -> bool {
        if !package
            .split('.')
            .next()
            .is_some_and(|name| self.no_other_import_root_supplies(name))
            || !skarmory_python::simple_name(module)
        {
            return false;
        }
        let no_competitor = |directory: &Path, module, chosen, kind| {
            self.directories.get(directory).is_some_and(|entries| {
                entries
                    .iter()
                    .any(|(name, entry)| name == OsStr::new(chosen) && *entry == kind)
                    && entries.iter().all(|(name, entry)| {
                        (name == OsStr::new(chosen) && *entry == kind)
                            || !entry_may_supply_module(name, *entry, module)
                    })
            })
        };
        let mut directory = self.base.clone();
        for name in package.split('.') {
            if !skarmory_python::simple_name(name)
                || !no_competitor(&directory, name, name, SeenEntry::Directory)
            {
                return false;
            }
            directory.push(name);
            if !no_competitor(&directory, "__init__", "__init__.py", SeenEntry::SourceFile) {
                return false;
            }
        }
        let name = format!("{module}.py");
        match source {
            Some(source) => {
                directory.join(&name) == self.base.join(source)
                    && no_competitor(&directory, module, &name, SeenEntry::SourceFile)
            }
            None => directory
                .strip_prefix(&self.base)
                .is_ok_and(|package| self.package_submodule_absent(package, module)),
        }
    }

    pub(super) fn root_flat_module_unambiguous(&self, source: &str, module: &str) -> bool {
        Path::new(source)
            .parent()
            .is_some_and(|parent| parent.as_os_str().is_empty())
            && self.no_other_import_root_supplies(module)
            && self.directories.get(&self.base).is_some_and(|entries| {
                !entries.iter().any(|(name, kind)| {
                    name != OsStr::new(source) && entry_may_supply_module(name, *kind, module)
                })
            })
    }

    pub(super) fn top_level_module_absent(&self, module: &str) -> bool {
        std::iter::once(&self.base)
            .chain(&self.search)
            .all(|root| self.import_name_absent(root, module))
    }

    fn no_other_import_root_supplies(&self, module: &str) -> bool {
        self.search.contains(&self.base)
            && self
                .search
                .iter()
                .filter(|root| *root != &self.base)
                .all(|root| self.import_name_absent(root, module))
    }

    fn import_name_absent(&self, root: &Path, module: &str) -> bool {
        self.directories.get(root).is_some_and(|entries| {
            !entries
                .iter()
                .any(|(name, kind)| entry_may_supply_module(name, *kind, module))
        })
    }

    fn package_submodule_absent(&self, package: &Path, module: &str) -> bool {
        self.directories
            .get(&self.base.join(package))
            .is_some_and(|entries| {
                !entries.iter().any(|(name, kind)| {
                    entry_may_supply_module(name, *kind, module)
                        || *kind == SeenEntry::OpaquePythonModule
                            && entry_may_supply_module(name, *kind, "__init__")
                })
            })
    }

    pub(super) fn base(&self) -> &Path {
        &self.base
    }

    pub(super) fn label(&self, path: &Path) -> String {
        display(path.strip_prefix(&self.base).unwrap_or(path))
    }
    fn is_source(&self, name: &OsStr) -> bool {
        is_source(name, self.include_javascript)
    }

    pub(super) fn javascript_context(&self) -> impl Iterator<Item = PathBuf> + '_ {
        self.discovered_paths(|name, _| is_javascript_context(name))
    }

    pub(super) fn javascript_sources(&self) -> impl Iterator<Item = PathBuf> + '_ {
        self.discovered_paths(|name, kind| {
            javascript_extension(name).is_some()
                && matches!(
                    kind,
                    SeenEntry::SourceFile | SeenEntry::LinkedSource | SeenEntry::OtherSource
                )
        })
    }

    fn discovered_paths(
        &self,
        matches: fn(&OsStr, SeenEntry) -> bool,
    ) -> impl Iterator<Item = PathBuf> + '_ {
        self.directories
            .iter()
            .flat_map(move |(directory, entries)| {
                entries
                    .iter()
                    .filter(move |(name, kind)| matches(name, *kind))
                    .map(|(name, _)| directory.join(name))
            })
    }
}

fn is_source(name: &OsStr, include_javascript: bool) -> bool {
    source::is_python(name) || (include_javascript && javascript_extension(name).is_some())
}

pub(super) fn javascript_extension(name: &OsStr) -> Option<&'static str> {
    match Path::new(name).extension()?.to_str()? {
        "js" => Some("js"),
        "jsx" => Some("jsx"),
        "mjs" => Some("mjs"),
        "cjs" => Some("cjs"),
        "ts" => Some("ts"),
        "tsx" => Some("tsx"),
        "mts" => Some("mts"),
        "cts" => Some("cts"),
        _ => None,
    }
}

pub(super) fn is_javascript_context(name: &OsStr) -> bool {
    name.to_str().is_some_and(|name| {
        matches!(name, "package.json" | "tsconfig.json" | "jsconfig.json")
            || (name.starts_with("tsconfig.") && name.ends_with(".json"))
            || Path::new(name).extension().is_some_and(|extension| {
                matches!(
                    extension.to_str(),
                    Some("html" | "htm" | "vue" | "svelte" | "astro" | "mdx")
                )
            })
            || [
                "vite.config.",
                "webpack.config.",
                "rollup.config.",
                "next.config.",
                "astro.config.",
                "svelte.config.",
                "nuxt.config.",
            ]
            .iter()
            .any(|prefix| name.starts_with(prefix))
    })
}

fn entry_module_name(name: &OsStr, kind: SeenEntry) -> Option<&str> {
    let name = name.to_str()?;
    Some(match kind {
        SeenEntry::Directory | SeenEntry::LinkedDirectory => name,
        _ => name.split('.').next().unwrap_or(name),
    })
}

fn entry_may_supply_module(name: &OsStr, kind: SeenEntry, module: &str) -> bool {
    entry_module_name(name, kind).is_none_or(|name| name.eq_ignore_ascii_case(module))
}

/// Record entries that can affect source selection or module resolution.
fn seen_entry(path: &Path, kind: FileType, include_javascript: bool) -> Option<SeenEntry> {
    let name = path.file_name()?;
    let is_source = is_source(name, include_javascript);
    if !kind.is_dir() && include_javascript && is_javascript_context(name) {
        return Some(SeenEntry::JavaScriptContext);
    }
    if kind.is_symlink() {
        if fs::metadata(path).is_ok_and(|metadata| metadata.is_dir()) {
            Some(SeenEntry::LinkedDirectory)
        } else {
            if is_source {
                Some(SeenEntry::LinkedSource)
            } else {
                opaque_python_module(path).then_some(SeenEntry::OpaquePythonModule)
            }
        }
    } else if kind.is_dir() {
        Some(SeenEntry::Directory)
    } else if !is_source {
        opaque_python_module(path).then_some(SeenEntry::OpaquePythonModule)
    } else if kind.is_file() {
        Some(SeenEntry::SourceFile)
    } else {
        Some(SeenEntry::OtherSource)
    }
}

pub(super) fn opaque_python_module(path: &Path) -> bool {
    path.extension()
        .and_then(OsStr::to_str)
        .is_some_and(|suffix| {
            ["pyc", "pyw", "so", "pyd"]
                .iter()
                .any(|known| suffix.eq_ignore_ascii_case(known))
        })
}

fn read_directory_entries(
    directory: &Path,
    include_javascript: bool,
) -> std::io::Result<DirectoryEntries> {
    // Directory timestamps can settle after enumeration on Windows. Verify the
    // source universe by comparing entry names and kinds without rereading sources.
    fn require_directory(directory: &Path) -> std::io::Result<()> {
        let metadata = fs::symlink_metadata(directory)?;
        if metadata.is_dir() && !metadata.file_type().is_symlink() {
            Ok(())
        } else {
            Err(std::io::Error::new(
                std::io::ErrorKind::NotADirectory,
                "source directory was replaced",
            ))
        }
    }

    require_directory(directory)?;
    let mut entries = DirectoryEntries::new();
    for child in fs::read_dir(directory)? {
        let child = child?;
        if let Some(kind) = seen_entry(&child.path(), child.file_type()?, include_javascript) {
            entries.insert((child.file_name(), kind));
        }
    }
    require_directory(directory)?;
    Ok(entries)
}

struct WorkerReservation<'a> {
    reserved: &'a AtomicUsize,
    count: usize,
}

impl<'a> WorkerReservation<'a> {
    fn acquire(reserved: &'a AtomicUsize, requested: usize, capacity: usize) -> Option<Self> {
        if requested < 2 {
            return None;
        }
        let available = |active| requested.min(capacity.saturating_sub(active));
        let active = reserved
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |active| {
                let count = available(active);
                (count >= 2).then_some(active + count)
            })
            .ok()?;
        Some(Self {
            reserved,
            count: available(active),
        })
    }
}

impl Drop for WorkerReservation<'_> {
    fn drop(&mut self) {
        self.reserved.fetch_sub(self.count, Ordering::Relaxed);
    }
}

impl SourceFile {
    fn is_javascript(&self) -> bool {
        javascript_extension(self.path.as_os_str()).is_some()
    }

    pub(super) fn directory(&self) -> &Path {
        self.path.parent().expect("absolute Python source path")
    }

    fn work<T>(&self, extract: &impl Fn(&SourceFile, &str) -> T) -> FileWork<T> {
        let start = Instant::now();
        match self.read() {
            Ok((bytes, stamp)) => {
                let result = source::decode(bytes, &self.label);
                let read_ns = start.elapsed().as_nanos();
                let start = Instant::now();
                let result = result.map(|source| extract(self, &source));
                FileWork {
                    result,
                    read_ns,
                    extract_ns: start.elapsed().as_nanos(),
                    stamp: Some(stamp),
                }
            }
            Err(issue) => FileWork {
                result: Err(issue),
                read_ns: start.elapsed().as_nanos(),
                extract_ns: 0,
                stamp: None,
            },
        }
    }

    fn read(&self) -> Result<(Vec<u8>, Stamp), Issue> {
        read_snapshot(&self.path, &self.label, self.discovered.as_ref())
    }
}

pub(super) fn read_snapshot(
    path: &Path,
    label: &str,
    discovered: Option<&Stamp>,
) -> Result<(Vec<u8>, Stamp), Issue> {
    let failure = |error: std::io::Error| Issue::new("read_error", label, error);
    let changed = || Issue::new("source_changed", label, "source changed during read");
    let before = fs::symlink_metadata(path).map_err(failure)?;
    if !before.is_file() {
        return Err(Issue::new(
            "unsupported_file",
            label,
            "expected a regular file without a symlink",
        ));
    }
    let stamp = Stamp::new(&before);
    if discovered.is_some_and(|seen| *seen != stamp) {
        return Err(changed());
    }
    let mut file = File::open(path).map_err(failure)?;
    if Stamp::new(&file.metadata().map_err(failure)?) != stamp {
        return Err(changed());
    }
    let mut bytes = Vec::new();
    (&mut file)
        .take(source::MAX_SOURCE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(failure)?;
    if bytes.len() as u64 > source::MAX_SOURCE_BYTES {
        return Err(Issue::new("source_limit", label, "source exceeds 1 MiB"));
    }
    let after = file.metadata().map_err(failure)?;
    let live = fs::symlink_metadata(path).map_err(failure)?;
    if !live.is_file()
        || after.len() != bytes.len() as u64
        || Stamp::new(&after) != stamp
        || Stamp::new(&live) != stamp
    {
        return Err(changed());
    }
    Ok((bytes, stamp))
}

fn module_name(
    file: &Path,
    root: &Path,
    files: &BTreeMap<PathBuf, usize>,
) -> Result<Option<String>, LookupError> {
    let Ok(relative) = file.strip_prefix(root) else {
        return Ok(None);
    };
    let mut parts = Vec::new();
    let mut directory = root.to_owned();
    for part in relative
        .parent()
        .expect("source relative path")
        .components()
    {
        let name = part.as_os_str().to_str().ok_or(LookupError::Ambiguous)?;
        if !skarmory_python::simple_name(name) {
            return Err(LookupError::Ambiguous);
        }
        directory.push(name);
        if !files.contains_key(&directory.join("__init__.py")) {
            return Err(LookupError::Namespace);
        }
        parts.push(name);
    }
    let stem = relative
        .file_stem()
        .and_then(|value| value.to_str())
        .filter(|name| skarmory_python::simple_name(name))
        .ok_or(LookupError::Ambiguous)?;
    if stem != "__init__" || parts.is_empty() {
        parts.push(stem);
    }
    Ok(Some(parts.join(".")))
}

fn checked_path(path: &Path) -> Result<(PathBuf, Metadata), String> {
    let metadata = fs::symlink_metadata(path).map_err(|error| error.to_string())?;
    if metadata.file_type().is_symlink() {
        return Err("symlinked scan targets are unsupported".into());
    }
    let resolved = fs::canonicalize(path).map_err(|error| error.to_string())?;
    let stamp = metadata.is_file().then(|| Stamp::new(&metadata));
    for live in [path, &resolved] {
        let live = fs::symlink_metadata(live).map_err(|error| error.to_string())?;
        if live.file_type().is_symlink()
            || live.is_dir() != metadata.is_dir()
            || live.is_file() != metadata.is_file()
            || stamp
                .as_ref()
                .is_some_and(|stamp| Stamp::new(&live) != *stamp)
        {
            return Err("scan target changed while resolving its path".into());
        }
    }
    Ok((resolved, metadata))
}

pub(super) fn reject_symlink_components(base: &Path, requested: &Path) -> Result<(), String> {
    let Ok(relative) = requested.strip_prefix(base) else {
        return Ok(());
    };
    let mut prefix = base.to_owned();
    for component in relative.components() {
        prefix.push(component.as_os_str());
        if fs::symlink_metadata(&prefix).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
            return Err("symlinked scan targets are unsupported".into());
        }
    }
    Ok(())
}

fn normalized(path: &Path) -> PathBuf {
    let path: PathBuf = path.components().collect();
    if path.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        path
    }
}

pub(super) fn display(path: &Path) -> String {
    if path.as_os_str().is_empty() {
        return ".".into();
    }
    let label = path.to_string_lossy();
    #[cfg(windows)]
    return label.replace('\\', "/");
    #[cfg(not(windows))]
    label.into_owned()
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicU64;

    use super::*;

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let serial = NEXT.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "skarmory-preview-source-{}-{serial}",
                std::process::id()
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn write(&self, path: &str, text: &str) {
            let path = self.0.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn ordered_roots_and_package_lookup_use_selected_paths() {
        let root = Fixture::new();
        for path in [
            "one/pkg/__init__.py",
            "one/pkg/entry.py",
            "one/pkg/helper.py",
            "one/helper.py",
            "one/pkg.py",
            "two/helper.py",
            "two/pkg/__init__.py",
            "two/pkg/only.py",
        ] {
            root.write(path, "pass\n");
        }

        let roots = [PathBuf::from("one"), PathBuf::from("two")];
        let project = discover(&root.0, &roots, FileEntry::None);
        assert!(project.issues.is_empty(), "{:?}", project.issues);
        assert_eq!(project.import_roots, ["one", "two"]);
        assert_eq!(
            project.files[project.lookup("helper").unwrap().file].label,
            "one/helper.py"
        );
        let package = project.lookup("pkg").unwrap();
        assert!(package.package);
        assert_eq!(project.files[package.file].label, "one/pkg/__init__.py");
        assert!(matches!(
            project.lookup("pkg.only"),
            Err(LookupError::Missing)
        ));

        let project = discover(
            &root.0,
            &roots,
            FileEntry::Explicit(Path::new("one/pkg/entry.py")),
        );
        assert!(project.issues.is_empty(), "{:?}", project.issues);
        assert_eq!(project.import_roots, ["one/pkg", "one", "two"]);
        let sibling = project.lookup("helper").unwrap().file;
        assert_eq!(project.files[sibling].label, "one/pkg/helper.py");
        assert_eq!(project.lookup("pkg.helper").unwrap().file, sibling);
    }

    #[test]
    fn snapshots_detect_stale_sources_and_preserve_index_order() {
        let root = Fixture::new();
        root.write("main.py", "pass\n");
        root.write("helper.py", "helper()\n");
        let project = discover(&root.0, &[], FileEntry::Default);
        assert!(project.issues.is_empty(), "{:?}", project.issues);

        let serial = project.map_files(1, str::to_owned).unwrap();
        let parallel = project.map_files(2, str::to_owned).unwrap();
        let serial_text: Vec<_> = serial
            .into_iter()
            .map(|work| work.result.unwrap())
            .collect();
        let (parallel_text, stamps): (Vec<_>, Vec<_>) = parallel
            .into_iter()
            .map(|work| (work.result.unwrap(), work.stamp))
            .unzip();
        assert_eq!(serial_text, ["helper()\n", "pass\n"]);
        assert_eq!(parallel_text, serial_text);
        assert!(project.verify(stamps.clone()).is_empty());

        root.write("main.py", "pass\npass\n");
        root.write("new.py", "pass\n");
        let issues = project.verify(stamps);
        assert!(
            issues
                .iter()
                .any(|issue| issue.kind == "source_changed" && issue.path == "main.py")
        );
        assert!(
            issues
                .iter()
                .any(|issue| issue.kind == "source_changed" && issue.path == ".")
        );
    }

    #[test]
    fn directory_verification_tracks_the_python_source_universe() {
        let root = Fixture::new();
        root.write("main.py", "pass\n");
        root.write("pkg/__init__.py", "pass\n");
        let project = discover(&root.0, &[], FileEntry::Default);
        let stamps: Vec<_> = project
            .map_files(2, str::to_owned)
            .unwrap()
            .into_iter()
            .map(|work| {
                work.result.unwrap();
                work.stamp
            })
            .collect();

        root.write("pkg/notes.txt", "unrelated to source selection\n");
        assert!(project.verify(stamps.clone()).is_empty());

        fs::rename(root.0.join("pkg/notes.txt"), root.0.join("pkg/new.py")).unwrap();
        let issues = project.verify(stamps);
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].kind, "source_changed");
        assert_eq!(issues[0].path, "pkg");
    }

    #[test]
    fn concurrent_scans_share_background_worker_capacity() {
        let reserved = AtomicUsize::new(0);
        let first = WorkerReservation::acquire(&reserved, 3, 5).unwrap();
        let second = WorkerReservation::acquire(&reserved, 4, 5).unwrap();
        assert_eq!((first.count, second.count), (3, 2));
        assert!(WorkerReservation::acquire(&reserved, 4, 5).is_none());
        drop(first);
        assert_eq!(
            WorkerReservation::acquire(&reserved, 4, 5).unwrap().count,
            3
        );
    }

    #[test]
    fn snapshot_budget_reports_incomplete_work_before_reading() {
        let root = Fixture::new();
        root.write("main.py", "pass\n");
        for index in 0..33 {
            File::create(root.0.join(format!("source_{index}.py")))
                .unwrap()
                .set_len(source::MAX_SOURCE_BYTES)
                .unwrap();
        }
        let project = discover(&root.0, &[], FileEntry::Default);
        assert!(project.issues.is_empty(), "{:?}", project.issues);
        let results = project.map_files(1, |_| panic!("over-budget snapshot was read"));
        assert!(matches!(results, Err(issue) if issue.kind == "analysis_limit"));
    }

    #[cfg(unix)]
    #[test]
    fn symlink_directories_are_excluded_and_linked_sources_fail() {
        use std::os::unix::fs::symlink;

        let outside = Fixture::new();
        outside.write("unknown.py", "pass\n");
        let root = Fixture::new();
        root.write("main.py", "pass\n");
        symlink(&outside.0, root.0.join("linked")).unwrap();
        symlink(outside.0.join("unknown.py"), root.0.join("linked.py")).unwrap();

        let project = discover(&root.0, &[], FileEntry::Default);
        assert_eq!(project.exclusions[0].path, "linked");
        assert_eq!(project.exclusions[0].reason, "symlink_directory");
        assert_eq!(
            project
                .files
                .iter()
                .map(|file| file.label.as_str())
                .collect::<Vec<_>>(),
            ["linked.py", "main.py"]
        );
        let work = project.map_files(1, str::to_owned).unwrap();
        let issue = work[0].result.as_ref().unwrap_err();
        assert_eq!(issue.kind, "unsupported_file");
        assert_eq!(issue.path, "linked.py");

        let target = root.0.join("linked").join(".");
        let project = discover(&target, &[], FileEntry::Default);
        assert_eq!(project.issues[0].kind, "discovery_error");
    }
}
