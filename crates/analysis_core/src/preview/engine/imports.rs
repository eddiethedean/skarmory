use skarmory_python::preview::Import;

use super::*;

impl<'a> Engine<'a> {
    pub(super) fn resolve(
        &mut self,
        name: &str,
        site: Site,
    ) -> Option<crate::preview_source::ResolvedModule> {
        use crate::preview_source::LookupError;
        if interpreter_module(name) {
            self.issue(
                site,
                "unresolved_import",
                format!("{name:?} is supplied by the interpreter before source-root lookup"),
            );
            return None;
        }
        match self.project.lookup(name) {
            Ok(module) => Some(module),
            Err(error) => {
                let (kind, detail) = match error {
                    LookupError::Missing => {
                        ("unresolved_import", "has no target in the selected sources")
                    }
                    LookupError::Namespace => (
                        "unsupported_namespace_package",
                        "requires a namespace package",
                    ),
                    LookupError::Ambiguous => ("ambiguous_module", "has ambiguous module identity"),
                };
                self.issue(site, kind, format!("{name:?} {detail}"));
                None
            }
        }
    }

    pub(super) fn load_parent(
        &mut self,
        state: State,
        parent: Option<&str>,
        site: Site,
    ) -> Vec<State> {
        if let Some(parent) = parent {
            self.load_module(state, parent, site, true)
                .into_iter()
                .map(|(state, _)| state)
                .collect()
        } else {
            vec![state]
        }
    }

    pub(super) fn load(&mut self, state: State, name: &str, site: Site) -> Outcomes<FrameId> {
        self.load_module(state, name, site, false)
    }

    fn load_module(
        &mut self,
        state: State,
        name: &str,
        site: Site,
        allow_loading: bool,
    ) -> Outcomes<FrameId> {
        if !self.step(site) {
            return Vec::new();
        }
        if name == "__main__" {
            self.issue(
                site,
                "unsupported_import",
                "importing the running __main__ module is outside the preview",
            );
            return Vec::new();
        }
        if let Some(loaded) = state.modules.get(name).copied() {
            return self.cached_module(state, name, loaded, site, allow_loading);
        }
        let Some(module) = self.resolve(name, site) else {
            return Vec::new();
        };
        if !self.enter(site) {
            return Vec::new();
        }
        let parent = name.rsplit_once('.');
        let package = if module.package {
            name
        } else {
            parent.map_or("", |(parent, _)| parent)
        };
        let mut out = Vec::new();
        for state in self.load_parent(state, parent.map(|(parent, _)| parent), site) {
            if let Some(loaded) = state.modules.get(name).copied() {
                out.extend(self.cached_module(state, name, loaded, site, allow_loading));
                continue;
            }
            if !self.static_package_path(&state, parent.map(|(parent, _)| parent), site) {
                continue;
            }
            for (mut state, frame) in self.initialize(state, name, module.file, Some(package), site)
            {
                if let Some((parent, child)) = parent {
                    let owner = state.modules[parent].frame;
                    state.bind(owner, child.into(), Value::Module(name.into()));
                }
                out.push((state, frame));
            }
            if !self.check_paths(out.len(), site) {
                break;
            }
        }
        self.leave();
        if self.limited { Vec::new() } else { out }
    }

    fn cached_module(
        &mut self,
        state: State,
        name: &str,
        loaded: Loaded,
        site: Site,
        allow_loading: bool,
    ) -> Outcomes<FrameId> {
        if loaded.loading && !allow_loading {
            self.issue(
                site,
                "unsupported_import_cycle",
                format!("import of {name:?} while it is initializing"),
            );
            Vec::new()
        } else {
            vec![(state, loaded.frame)]
        }
    }

    pub(super) fn static_package_path(
        &mut self,
        state: &State,
        parent: Option<&str>,
        site: Site,
    ) -> bool {
        if let Some(parent) = parent
            && state
                .frame(state.modules[parent].frame)
                .locals
                .contains_key("__path__")
        {
            self.issue(
                site,
                "unsupported_import_path",
                format!("{parent:?} replaces its package search path"),
            );
            return false;
        }
        true
    }

    pub(super) fn initialize(
        &mut self,
        mut state: State,
        name: &str,
        file: usize,
        package: Option<&str>,
        site: Site,
    ) -> Outcomes<FrameId> {
        if !self.step(site) {
            return Vec::new();
        }
        let frame = state.push(Frame {
            parent: None,
            module: state.frames.len(),
            file,
            locals: BTreeMap::from([
                (
                    "__name__".into(),
                    Value::Constant(Constant::String(name.into())),
                ),
                (
                    "__package__".into(),
                    Value::Constant(
                        package.map_or(Constant::None, |name| Constant::String(name.into())),
                    ),
                ),
            ]),
            definition: None,
            package: package.map(str::to_owned),
        });
        Rc::make_mut(&mut state.modules).insert(
            name.into(),
            Loaded {
                frame,
                loading: true,
            },
        );
        state.revision += 1;
        let body = &self.modules[file].body;
        self.block(state, frame, body)
            .into_iter()
            .map(|(mut state, _)| {
                Rc::make_mut(&mut state.modules)
                    .get_mut(name)
                    .expect("module frame")
                    .loading = false;
                state.revision += 1;
                (state, frame)
            })
            .collect()
    }

    pub(super) fn import(
        &mut self,
        state: State,
        frame: FrameId,
        import: &Import,
        site: Site,
    ) -> Vec<State> {
        let Some(name) = self.import_name(&state, frame, import, site) else {
            return Vec::new();
        };
        let module = state.frame(frame).module;
        // A package initializer can use `from . import child` on its own namespace.
        let own_namespace = import.member.is_some()
            && state
                .modules
                .get(&name)
                .is_some_and(|loaded| loaded.frame == module);
        let mut out = Vec::new();
        for (state, imported) in self.load_module(state, &name, site, own_namespace) {
            if let Some(member) = &import.member {
                let namespace = &state.frame(imported).locals;
                if let Some(value) = namespace.get(member).cloned() {
                    let mut state = state;
                    state.bind(frame, import.local_name().into(), value);
                    out.push(state);
                } else if namespace.contains_key("__getattr__") {
                    self.issue(
                        site,
                        "unsupported_dynamic_export",
                        format!("{name:?} may provide {member:?} through __getattr__"),
                    );
                } else if state.frame(imported).package.as_deref() == Some(&name) {
                    let child = format!("{name}.{member}");
                    for (mut state, _) in self.load(state, &child, site) {
                        state.bind(
                            frame,
                            import.local_name().into(),
                            Value::Module(child.clone()),
                        );
                        out.push(state);
                    }
                } else {
                    self.issue(
                        site,
                        "unresolved_import",
                        format!("{name:?} has no binding for {member:?}"),
                    );
                }
            } else if let Some(value) =
                self.imported_value(&state, &name, import.alias.is_some(), site)
            {
                let mut state = state;
                state.bind(frame, import.local_name().into(), value);
                out.push(state);
            }
            if !self.check_paths(out.len(), site) {
                return Vec::new();
            }
        }
        out
    }

    fn imported_value(
        &mut self,
        state: &State,
        name: &str,
        aliased: bool,
        site: Site,
    ) -> Option<Value> {
        let mut components = name.split('.');
        let mut value = Value::Module(components.next().expect("first import component").into());
        if !aliased {
            return Some(value);
        }
        // Python follows package attributes for dotted aliases even when the
        // qualified submodule is already in the module cache.
        for child in components {
            let Value::Module(parent) = value else {
                self.issue(
                    site,
                    "unresolved_import",
                    "dotted alias traverses a non-module binding",
                );
                return None;
            };
            let namespace = &state.frame(state.modules[&parent].frame).locals;
            if let Some(binding) = namespace.get(child) {
                value = binding.clone();
            } else if namespace.contains_key("__getattr__") {
                self.issue(
                    site,
                    "unsupported_dynamic_export",
                    format!("{parent:?} may provide {child:?} through __getattr__"),
                );
                return None;
            } else if let Some(Value::Constant(Constant::String(parent_name))) =
                namespace.get("__name__")
                && state
                    .modules
                    .contains_key(&format!("{parent_name}.{child}"))
            {
                value = Value::Module(format!("{parent_name}.{child}"));
            } else {
                self.issue(
                    site,
                    "unresolved_import",
                    format!("{parent:?} has no binding for {child:?}"),
                );
                return None;
            }
        }
        Some(value)
    }

    fn import_name(
        &mut self,
        state: &State,
        frame: FrameId,
        import: &Import,
        site: Site,
    ) -> Option<String> {
        if import.level == 0 {
            return Some(import.module.clone());
        }
        let globals = state.frame(state.frame(frame).module);
        let package = match globals.locals.get("__package__") {
            Some(Value::Constant(Constant::String(package))) => package.as_str(),
            Some(Value::Constant(Constant::None)) if !globals.locals.contains_key("__spec__") => {
                globals.package.as_deref().unwrap_or("")
            }
            _ => {
                self.issue(
                    site,
                    "unresolved_import",
                    "relative import has an unresolved package context",
                );
                return None;
            }
        };
        if package.is_empty() {
            self.issue(
                site,
                "unresolved_import",
                "relative import has no package context",
            );
            return None;
        }
        let mut base = package;
        for _ in 1..import.level {
            let Some((parent, _)) = base.rsplit_once('.') else {
                self.issue(
                    site,
                    "unresolved_import",
                    "relative import exceeds its top-level package",
                );
                return None;
            };
            base = parent;
        }
        if import.module.is_empty() {
            Some(base.into())
        } else {
            Some(format!("{base}.{}", import.module))
        }
    }
}

fn interpreter_module(name: &str) -> bool {
    // Model standard CPython 3.11 on the scanner's platform. Extensions listed
    // in PC/config.c are built in on Windows and normally path-loaded on Unix,
    // where a selected source can take precedence.
    let top = name.split('.').next().unwrap_or(name);
    match top {
        "posix" | "pwd" => cfg!(unix),
        "_bisect" | "_blake2" | "_codecs_cn" | "_codecs_hk" | "_codecs_iso2022" | "_codecs_jp"
        | "_codecs_kr" | "_codecs_tw" | "_contextvars" | "_csv" | "_datetime" | "_heapq"
        | "_json" | "_lsprof" | "_md5" | "_multibytecodec" | "_opcode" | "_pickle" | "_random"
        | "_sha1" | "_sha256" | "_sha3" | "_sha512" | "_statistics" | "_struct" | "_typing"
        | "_winapi" | "_xxsubinterpreters" | "array" | "audioop" | "binascii" | "cmath"
        | "math" | "mmap" | "msvcrt" | "nt" | "winreg" | "zlib" => cfg!(windows),
        "__hello__"
        | "__hello_alias__"
        | "__hello_only__"
        | "__phello__"
        | "__phello_alias__"
        | "_abc"
        | "_ast"
        | "_codecs"
        | "_collections"
        | "_collections_abc"
        | "_frozen_importlib"
        | "_frozen_importlib_external"
        | "_functools"
        | "_imp"
        | "_io"
        | "_locale"
        | "_operator"
        | "_signal"
        | "_sitebuiltins"
        | "_sre"
        | "_stat"
        | "_string"
        | "_symtable"
        | "_thread"
        | "_tokenize"
        | "_tracemalloc"
        | "_warnings"
        | "_weakref"
        | "abc"
        | "atexit"
        | "builtins"
        | "codecs"
        | "encodings"
        | "errno"
        | "faulthandler"
        | "gc"
        | "genericpath"
        | "importlib"
        | "io"
        | "itertools"
        | "marshal"
        | "ntpath"
        | "os"
        | "posixpath"
        | "runpy"
        | "site"
        | "stat"
        | "sys"
        | "time"
        | "xxsubtype"
        | "zipimport" => true,
        _ => false,
    }
}
