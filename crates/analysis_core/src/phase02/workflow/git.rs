use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Seek, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

use super::ConfigError;

const MAX_TREE_BYTES: usize = 8 * 1024 * 1024;
const MAX_FILES: usize = 20_000;
const MAX_BLOB_BYTES: usize = 2 * 1024 * 1024;
const MAX_SNAPSHOT_BYTES: usize = 128 * 1024 * 1024;

pub(super) struct Repository {
    pub(super) root: PathBuf,
}

pub(super) struct Base {
    pub(super) commit: String,
    pub(super) entries: BTreeMap<String, Entry>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    File,
    Link,
    Submodule,
}

pub(super) struct Entry {
    object: String,
    pub(super) kind: Kind,
    size: usize,
}

impl Repository {
    pub(super) fn open(path: &Path) -> Result<Self, ConfigError> {
        let root = output(path, &["rev-parse", "--show-toplevel"], 64 * 1024)?;
        let root = std::str::from_utf8(&root).map_err(ConfigError::new)?;
        let root = PathBuf::from(root.trim_end_matches(['\r', '\n']))
            .canonicalize()
            .map_err(ConfigError::new)?;
        Ok(Self { root })
    }

    pub(super) fn revision(&self, reference: &str) -> Result<String, ConfigError> {
        let bytes = output(
            &self.root,
            &[
                "rev-parse",
                "--verify",
                "--end-of-options",
                &format!("{reference}^{{commit}}"),
            ],
            128,
        )?;
        object_id(&bytes)
    }

    pub(super) fn base(&self, reference: &str) -> Result<Base, ConfigError> {
        let auto;
        let reference = if reference == "auto" {
            auto = std::env::var("GITHUB_BASE_REF")
                .ok()
                .filter(|branch| !branch.is_empty())
                .map(|branch| format!("refs/remotes/origin/{branch}"))
                .or_else(|| {
                    output(
                        &self.root,
                        &["symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"],
                        4096,
                    )
                    .ok()
                    .and_then(|bytes| String::from_utf8(bytes).ok())
                    .map(|target| target.trim().to_owned())
                })
                .unwrap_or_else(|| "refs/remotes/origin/main".into());
            auto.as_str()
        } else {
            reference
        };
        let revision = self.revision(reference)?;
        let commit = object_id(&output(
            &self.root,
            &["merge-base", &revision, "HEAD"],
            128,
        )?)?;
        let tree = output(
            &self.root,
            &["ls-tree", "--full-tree", "-rlz", &commit],
            MAX_TREE_BYTES,
        )?;
        let mut entries = BTreeMap::new();
        for record in tree
            .split(|byte| *byte == 0)
            .filter(|record| !record.is_empty())
        {
            if entries.len() >= MAX_FILES {
                return Err(ConfigError::new("Git tree exceeds the 20,000-entry limit"));
            }
            let record = std::str::from_utf8(record).map_err(ConfigError::new)?;
            let (header, name) = record.split_once('\t').ok_or_else(invalid_tree)?;
            let [mode, kind, object, size]: [_; 4] = header
                .split_whitespace()
                .collect::<Vec<_>>()
                .try_into()
                .map_err(|_| invalid_tree())?;
            let kind = match (mode, kind) {
                ("100644" | "100755", "blob") => Kind::File,
                ("120000", "blob") => Kind::Link,
                ("160000", "commit") => Kind::Submodule,
                _ => return Err(invalid_tree()),
            };
            let size = if kind == Kind::Submodule {
                0
            } else {
                size.parse().map_err(|_| invalid_tree())?
            };
            if !safe_path(name) || !valid_oid(object) {
                return Err(invalid_tree());
            }
            if entries
                .insert(
                    name.to_owned(),
                    Entry {
                        object: object.into(),
                        kind,
                        size,
                    },
                )
                .is_some()
            {
                return Err(invalid_tree());
            }
        }
        Ok(Base { commit, entries })
    }

    pub(super) fn visible_paths(&self) -> Result<BTreeSet<String>, ConfigError> {
        let bytes = output(
            &self.root,
            &[
                "ls-files",
                "--cached",
                "--others",
                "--exclude-standard",
                "-z",
            ],
            MAX_TREE_BYTES,
        )?;
        Ok(bytes
            .split(|byte| *byte == 0)
            .filter_map(|entry| std::str::from_utf8(entry).ok())
            .filter(|path| !path.is_empty())
            .map(str::to_owned)
            .collect())
    }

    pub(super) fn read_path(
        &self,
        revision: &str,
        path: &str,
        limit: usize,
    ) -> Result<Vec<u8>, ConfigError> {
        output(
            &self.root,
            &["cat-file", "blob", &format!("{revision}:{path}")],
            limit,
        )
    }

    pub(super) fn blobs<'a>(
        &self,
        entries: impl IntoIterator<Item = (&'a str, &'a Entry)>,
        mut consume: impl FnMut(&str, &Entry, Vec<u8>) -> Result<(), ConfigError>,
    ) -> Result<(), ConfigError> {
        let entries: Vec<_> = entries.into_iter().collect();
        if entries
            .iter()
            .map(|(_, entry)| entry.size as u128)
            .sum::<u128>()
            > MAX_SNAPSHOT_BYTES as u128
        {
            return Err(ConfigError::new("Git snapshot exceeds the 128 MiB limit"));
        }
        let mut input = tempfile::tempfile().map_err(ConfigError::new)?;
        for (_, entry) in &entries {
            if entry.size > MAX_BLOB_BYTES || entry.kind == Kind::Submodule {
                return Err(ConfigError::new(
                    "Git input exceeds the 2 MiB blob limit or is a submodule",
                ));
            }
            writeln!(input, "{}", entry.object).map_err(ConfigError::new)?;
        }
        input.rewind().map_err(ConfigError::new)?;
        with_stdout(
            &self.root,
            &["cat-file", "--batch"],
            Stdio::from(input),
            |stdout| {
                let mut stdout = BufReader::new(stdout);
                for (name, entry) in entries {
                    let mut header = String::new();
                    (&mut stdout)
                        .take(128)
                        .read_line(&mut header)
                        .map_err(ConfigError::new)?;
                    if header != format!("{} blob {}\n", entry.object, entry.size) {
                        return Err(ConfigError::new(format!(
                            "Git blob unavailable for {name:?}"
                        )));
                    }
                    let mut body = vec![0; entry.size];
                    stdout.read_exact(&mut body).map_err(ConfigError::new)?;
                    let mut end = [0];
                    stdout.read_exact(&mut end).map_err(ConfigError::new)?;
                    if end != *b"\n" {
                        return Err(ConfigError::new("invalid Git blob terminator"));
                    }
                    consume(name, entry, body)?;
                }
                Ok(())
            },
        )
    }

    pub(super) fn materialize_policy(
        &self,
        base: &Base,
        root: &Path,
        scope: &Path,
    ) -> Result<(), ConfigError> {
        let entries = base.entries.iter().filter(|(name, _)| {
            let file = Path::new(name);
            let directory = file.parent().expect("Git entry has parent");
            let policy_directory = if file.ends_with(".skylos/config.yaml") {
                directory
                    .parent()
                    .expect("synced policy has project directory")
            } else if matches!(
                file.file_name().and_then(|name| name.to_str()),
                Some(".skylos.toml" | "skylos.toml" | "pyproject.toml")
            ) {
                directory
            } else {
                return false;
            };
            policy_directory.starts_with(scope) || scope.starts_with(policy_directory)
        });
        self.blobs(
            entries.map(|(name, entry)| (name.as_str(), entry)),
            |name, entry, bytes| {
                if entry.kind != Kind::File {
                    return Err(ConfigError::new(format!(
                        "Git policy file {name:?} must be regular"
                    )));
                }
                let path = root.join(name);
                fs::create_dir_all(path.parent().expect("Git entry has parent"))
                    .map_err(ConfigError::new)?;
                File::create_new(path)
                    .and_then(|mut file| file.write_all(&bytes))
                    .map_err(ConfigError::new)
            },
        )
    }

    pub(super) fn materialize(&self, base: &Base, root: &Path) -> Result<(), ConfigError> {
        let mut links = Vec::new();
        self.blobs(
            base.entries
                .iter()
                .map(|(name, entry)| (name.as_str(), entry)),
            |name, entry, bytes| {
                let path = root.join(name);
                fs::create_dir_all(path.parent().expect("Git entry has parent"))
                    .map_err(ConfigError::new)?;
                if entry.kind == Kind::Link {
                    links.push((name.to_owned(), bytes));
                } else {
                    File::create_new(path)
                        .and_then(|mut file| file.write_all(&bytes))
                        .map_err(ConfigError::new)?;
                }
                Ok(())
            },
        )?;
        // Resolve relative targets lexically inside the private tree. Reject
        // traversing a symlink before '..', which would change that meaning.
        let linked: BTreeSet<_> = links.iter().map(|(name, _)| Path::new(name)).collect();
        for (name, target) in &links {
            let target = std::str::from_utf8(target).map_err(ConfigError::new)?;
            let target = link_target(Path::new(name), target, &linked)?;
            create_link(&root.join(target), &root.join(name))?;
        }
        Ok(())
    }
}

pub(super) fn safe_path(path: &str) -> bool {
    !path.is_empty()
        && path.split('/').all(|part| {
            !matches!(part, "" | "." | "..")
                && !part.eq_ignore_ascii_case(".git")
                && !part.contains(['\\', ':', '\0'])
                && (!cfg!(windows) || windows_component(part))
        })
}

fn windows_component(part: &str) -> bool {
    let stem = part
        .split('.')
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase();
    let device = matches!(stem.trim_end(), "CON" | "PRN" | "AUX" | "NUL")
        || ["COM", "LPT"].iter().any(|prefix| {
            stem.trim_end().strip_prefix(prefix).is_some_and(|suffix| {
                matches!(
                    suffix,
                    "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
                )
            })
        });
    !device && !part.ends_with(['.', ' ']) && !part.contains(['<', '>', '"', '|', '?', '*'])
}

fn link_target(
    name: &Path,
    target: &str,
    linked: &BTreeSet<&Path>,
) -> Result<PathBuf, ConfigError> {
    let invalid = || ConfigError::new(format!("unsafe Git snapshot symlink {name:?}"));
    if target.is_empty() || target.len() > 4096 || target.contains(['\\', ':', '\0']) {
        return Err(invalid());
    }
    let mut resolved = name.parent().expect("relative link").to_owned();
    for part in Path::new(target).components() {
        match part {
            Component::Normal(name) if name.to_str().is_some_and(safe_path) => resolved.push(name),
            Component::CurDir => {}
            Component::ParentDir
                if !resolved.ancestors().any(|part| linked.contains(part)) && resolved.pop() => {}
            _ => return Err(invalid()),
        }
    }
    Ok(resolved)
}

#[cfg(unix)]
fn create_link(target: &Path, path: &Path) -> Result<(), ConfigError> {
    std::os::unix::fs::symlink(target, path).map_err(ConfigError::new)
}

#[cfg(windows)]
fn create_link(target: &Path, path: &Path) -> Result<(), ConfigError> {
    let create = if target.is_dir() {
        std::os::windows::fs::symlink_dir
    } else {
        std::os::windows::fs::symlink_file
    };
    create(target, path).map_err(ConfigError::new)
}

fn invalid_tree() -> ConfigError {
    ConfigError::new("unsupported or invalid Git tree entry")
}

fn valid_oid(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn object_id(bytes: &[u8]) -> Result<String, ConfigError> {
    let value = std::str::from_utf8(bytes).map_err(ConfigError::new)?.trim();
    if !valid_oid(value) {
        return Err(ConfigError::new("invalid Git object ID"));
    }
    Ok(value.into())
}

fn output(path: &Path, args: &[&str], limit: usize) -> Result<Vec<u8>, ConfigError> {
    with_stdout(path, args, Stdio::null(), |stdout| {
        let mut data = Vec::new();
        stdout
            .take(limit as u64 + 1)
            .read_to_end(&mut data)
            .map_err(ConfigError::new)?;
        if data.len() > limit {
            return Err(ConfigError::new(format!(
                "git {} exceeded the output limit",
                args[0]
            )));
        }
        Ok(data)
    })
}

fn with_stdout<T>(
    path: &Path,
    args: &[&str],
    stdin: Stdio,
    read: impl FnOnce(std::process::ChildStdout) -> Result<T, ConfigError>,
) -> Result<T, ConfigError> {
    let mut command = Command::new("git");
    command
        .current_dir(path)
        .args(["--no-replace-objects", "-c", "protocol.allow=never"])
        .args(args)
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_TERMINAL_PROMPT", "0");
    // Read the repository containing the selected directory, even when the
    // caller runs Skarmory from a different Git command's environment.
    for variable in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_COMMON_DIR",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_NAMESPACE",
    ] {
        command.env_remove(variable);
    }
    let mut child = command
        .stdin(stdin)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| ConfigError::new(format!("could not start Git: {error}")))?;
    let result = read(child.stdout.take().expect("piped Git stdout"));
    if result.is_err() {
        let _ = child.kill();
    }
    let status = child.wait().map_err(ConfigError::new)?;
    let value = result?;
    if !status.success() {
        return Err(ConfigError::new(format!(
            "git {} failed ({status})",
            args[0]
        )));
    }
    Ok(value)
}
