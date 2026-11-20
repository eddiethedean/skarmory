use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};

use crate::Issue;

const MAX_SOURCE_BYTES: u64 = 1024 * 1024;

pub(super) struct Discovery {
    pub entry: Option<String>,
    pub files: Vec<SourceFile>,
    pub issues: Vec<Issue>,
}

pub(super) struct SourceFile {
    pub name: String,
    pub label: String,
    path: PathBuf,
}

pub(super) fn discover(path: &Path) -> Discovery {
    let mut result = Discovery {
        entry: None,
        files: Vec::new(),
        issues: Vec::new(),
    };
    let is_python = |path: &Path| path.extension() == Some(OsStr::new("py"));
    let paths = match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_file() && is_python(path) => vec![path.to_owned()],
        Ok(meta) if meta.is_dir() => {
            result.entry = Some("main".into());
            let mut paths = Vec::new();
            match fs::read_dir(path) {
                Ok(entries) => {
                    for entry in entries {
                        match entry {
                            Ok(entry) if is_python(&entry.path()) => paths.push(entry.path()),
                            Ok(_) => {}
                            Err(error) => {
                                result
                                    .issues
                                    .push(Issue::new("discovery_error", ".", error))
                            }
                        }
                    }
                }
                Err(error) => result
                    .issues
                    .push(Issue::new("discovery_error", ".", error)),
            }
            paths.sort_unstable();
            paths
        }
        outcome => {
            let message = outcome.err().map_or_else(
                || "expected a directory or a regular .py file".into(),
                |error| error.to_string(),
            );
            result.issues.push(Issue::new(
                "discovery_error",
                &path.to_string_lossy(),
                message,
            ));
            return result;
        }
    };

    for path in paths {
        let label = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let name = path.file_stem().and_then(OsStr::to_str).unwrap_or_default();
        if !skarmory_python::simple_name(name) || matches!(name, "__init__" | "__future__") {
            result.issues.push(Issue::new(
                "unsupported_module_name",
                &label,
                "expected a flat ASCII module name",
            ));
        }
        if result.entry.is_none() {
            result.entry = Some(name.into());
        }
        result.files.push(SourceFile {
            name: name.into(),
            label,
            path,
        });
    }
    if let Some(entry) = &result.entry
        && !result.files.iter().any(|file| file.name == *entry)
    {
        result.issues.push(Issue::new(
            "missing_entry",
            "main.py",
            "directory analysis requires main.py",
        ));
    }
    result
}

impl SourceFile {
    pub fn read(&self) -> Result<String, Issue> {
        let failure = |error: std::io::Error| Issue::new("read_error", &self.label, error);
        let metadata = fs::symlink_metadata(&self.path).map_err(failure)?;
        if !metadata.is_file() {
            return Err(Issue::new(
                "unsupported_file",
                &self.label,
                "expected a regular file without a symlink",
            ));
        }
        let mut file = File::open(&self.path).map_err(failure)?;
        let mut bytes = Vec::new();
        (&mut file)
            .take(MAX_SOURCE_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(failure)?;
        if bytes.len() as u64 > MAX_SOURCE_BYTES {
            return Err(Issue::new(
                "source_limit",
                &self.label,
                "source exceeds 1 MiB",
            ));
        }
        let after = file.metadata().map_err(failure)?;
        if after.len() != bytes.len() as u64
            || metadata.len() != after.len()
            || metadata.modified().ok() != after.modified().ok()
        {
            return Err(Issue::new(
                "source_changed",
                &self.label,
                "file metadata changed during read",
            ));
        }
        let mut source = String::from_utf8(bytes)
            .map_err(|error| Issue::new("unsupported_encoding", &self.label, error))?;
        if source.starts_with('\u{feff}') {
            source.drain(..'\u{feff}'.len_utf8());
        }
        if source.contains('\r') {
            source = source.replace("\r\n", "\n").replace('\r', "\n");
        }
        for line in source.lines().take(2) {
            let line = line.trim_start_matches([' ', '\t', '\x0c']);
            if line.is_empty() {
                continue;
            }
            let Some(comment) = line.strip_prefix('#') else {
                break;
            };
            if let Some(name) = encoding_cookie(comment) {
                if !name.eq_ignore_ascii_case("utf-8") && !name.eq_ignore_ascii_case("utf_8") {
                    return Err(Issue::new(
                        "unsupported_encoding",
                        &self.label,
                        format!("source declares {name}; the prototype accepts UTF-8"),
                    ));
                }
                break;
            }
        }
        Ok(source)
    }
}

fn encoding_cookie(comment: &str) -> Option<&str> {
    comment.match_indices("coding").find_map(|(index, word)| {
        let suffix = &comment[index + word.len()..];
        let value = suffix
            .strip_prefix([':', '='])?
            .trim_start_matches([' ', '\t']);
        let end = value.find(|c: char| !c.is_ascii_alphanumeric() && !matches!(c, '_' | '-' | '.'));
        let name = &value[..end.unwrap_or(value.len())];
        (!name.is_empty()).then_some(name)
    })
}
