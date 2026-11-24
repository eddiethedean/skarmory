//! Shared discovery and file-local facts for the additional Phase 0.3 frontends.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::{Analysis, cpp, go, java, rust};
use crate::Issue;
use crate::fact_cache::FactCache;
use crate::preview_source::Project;

type ExtractFacts = fn(&str, &str) -> Result<Facts, Issue>;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum Language {
    Go,
    Rust,
    Cpp,
    Java,
    Swift,
    Php,
    CSharp,
    Kotlin,
    Ruby,
    Dart,
    Robot,
}

impl Language {
    pub(crate) fn from_path(path: &Path) -> Option<Self> {
        let extension = path.extension()?.to_str()?;
        Some(match extension {
            "go" => Self::Go,
            "rs" => Self::Rust,
            "java" => Self::Java,
            "swift" => Self::Swift,
            "php" => Self::Php,
            "cs" => Self::CSharp,
            "kt" | "kts" => Self::Kotlin,
            "rb" | "rake" | "gemspec" => Self::Ruby,
            "dart" => Self::Dart,
            "robot" | "resource" => Self::Robot,
            _ if ["cpp", "cc", "cxx", "hpp", "hh", "hxx"]
                .iter()
                .any(|suffix| extension.eq_ignore_ascii_case(suffix)) =>
            {
                Self::Cpp
            }
            _ => return None,
        })
    }

    pub(crate) fn is_implemented(self) -> bool {
        matches!(self, Self::Go | Self::Rust | Self::Cpp | Self::Java)
    }

    pub(super) fn unsupported(self, path: &str) -> Issue {
        Issue::new(
            "unsupported_frontend",
            path,
            format!("{} dead-code analysis is not implemented", self.name()),
        )
    }

    pub(super) fn name(self) -> &'static str {
        match self {
            Self::Go => "Go",
            Self::Rust => "Rust",
            Self::Cpp => "C++",
            Self::Java => "Java",
            Self::Swift => "Swift",
            Self::Php => "PHP",
            Self::CSharp => "C#",
            Self::Kotlin => "Kotlin",
            Self::Ruby => "Ruby",
            Self::Dart => "Dart",
            Self::Robot => "Robot Framework",
        }
    }

    pub(super) fn extract(
        self,
        path: &str,
        source: &str,
        cache: Option<&FactCache>,
    ) -> Result<Facts, Issue> {
        let (variant, extract): (&str, ExtractFacts) = match self {
            Self::Go => ("phase03-go-v1/ts-0.25.0", |path, source| {
                go::extract(path, source).map(Facts::Go)
            }),
            Self::Rust => ("phase03-rust-v1/ts-0.24.2", |path, source| {
                rust::extract(path, source).map(Facts::Rust)
            }),
            Self::Cpp => ("phase03-cpp-v1/ts-0.23.4", |path, source| {
                cpp::extract(path, source).map(Facts::Cpp)
            }),
            Self::Java => ("phase03-java-v1/ts-0.23.5", |path, source| {
                java::extract(path, source).map(Facts::Java)
            }),
            _ => return Err(self.unsupported(path)),
        };
        // A frontend can derive facts from the source's path as well as its text.
        match cache {
            Some(cache) => {
                cache.get_or_extract_variant(&format!("{variant}/{path}"), source, |source| {
                    extract(path, source)
                })
            }
            None => extract(path, source),
        }
    }
}

#[derive(Deserialize, Serialize)]
pub(super) enum Facts {
    Go(go::Facts),
    Rust(rust::Facts),
    Cpp(cpp::Facts),
    Java(java::Facts),
}

#[derive(Default)]
pub(super) struct Files {
    go: BTreeMap<String, go::Facts>,
    rust: BTreeMap<String, rust::Facts>,
    cpp: BTreeMap<String, cpp::Facts>,
    java: BTreeMap<String, java::Facts>,
}

impl Files {
    pub(super) fn insert(&mut self, path: String, facts: Facts) {
        match facts {
            Facts::Go(facts) => {
                self.go.insert(path, facts);
            }
            Facts::Rust(facts) => {
                self.rust.insert(path, facts);
            }
            Facts::Cpp(facts) => {
                self.cpp.insert(path, facts);
            }
            Facts::Java(facts) => {
                self.java.insert(path, facts);
            }
        }
    }

    pub(super) fn len(&self) -> usize {
        self.go.len() + self.rust.len() + self.cpp.len() + self.java.len()
    }

    pub(super) fn analyze(
        &self,
        project: &Project,
        single_file: bool,
        analyses: &mut BTreeMap<Language, Analysis>,
    ) {
        for (&language, analysis) in analyses {
            if !analysis.issues.is_empty() {
                continue;
            }
            if single_file && matches!(language, Language::Go | Language::Java | Language::Rust) {
                analysis.issues.push(Issue::new(
                    "unsupported_frontend_input_scope",
                    &project.files[0].label,
                    format!(
                        "{} reachability requires scanning the project directory",
                        language.name()
                    ),
                ));
                continue;
            }
            match language {
                Language::Go => go::analyze(project, &self.go, analysis),
                Language::Rust => rust::analyze(project, &self.rust, analysis),
                Language::Cpp => cpp::analyze(project, &self.cpp, analysis),
                Language::Java => java::analyze(project, &self.java, analysis),
                _ => {}
            }
        }
    }
}
