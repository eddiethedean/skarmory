//! Positive runtime observations matched against the current source snapshot.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{ErrorKind, Write};
use std::path::{Component, Path, PathBuf};

use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};

use crate::Issue;
use crate::preview_source::{Project, Stamp, read_snapshot};

const MAX_OBSERVATIONS: usize = 32_768;

#[derive(Clone, Debug, Default)]
pub enum Input {
    #[default]
    Default,
    Disabled,
    Path(PathBuf),
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Provenance {
    Unverified,
    Verified,
}

#[derive(Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(super) struct Observation {
    pub(super) artifact: String,
    pub(super) file: String,
    pub(super) line: usize,
    pub(super) function: Option<String>,
    pub(super) provenance: Provenance,
}

impl Observation {
    pub(super) fn kind(&self) -> &'static str {
        if self.function.is_some() {
            "runtime_trace"
        } else {
            "runtime_coverage"
        }
    }
}

#[derive(Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub(super) struct Evidence {
    pub(super) artifact: String,
    pub(super) file: String,
    pub(super) line: usize,
    pub(super) full_name: String,
    pub(super) kind: &'static str,
    pub(super) provenance: Provenance,
}

#[derive(Default)]
pub(super) struct Snapshot {
    pub(super) observations: BTreeSet<Observation>,
    pub(super) issues: Vec<Issue>,
}

#[derive(Deserialize)]
struct Artifact {
    version: u32,
    #[serde(default)]
    calls: Option<Vec<Call>>,
    #[serde(default)]
    lines: Vec<Line>,
    revision: Option<String>,
    fingerprints: Option<BTreeMap<String, String>>,
}

#[derive(Deserialize)]
struct Call {
    file: String,
    line: usize,
    function: String,
    #[serde(default = "one")]
    count: u64,
}

fn one() -> u64 {
    1
}

#[derive(Deserialize)]
struct Line {
    file: String,
    line: usize,
}

fn revision(sources: &BTreeMap<String, String>) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"skarmory-source-snapshot-v1\0");
    for (path, source) in sources {
        hasher.update(&(path.len() as u64).to_le_bytes());
        hasher.update(path.as_bytes());
        hasher.update(blake3::hash(source.as_bytes()).as_bytes());
    }
    hasher.finalize().to_hex().to_string()
}

impl Snapshot {
    pub(super) fn read(
        path: &Path,
        project: &Project,
        sources: &BTreeMap<String, String>,
        stamps: &[Option<Stamp>],
        trace: &Input,
        coverage: &Input,
    ) -> Self {
        let mut snapshot = Self::default();
        let requested_base = std::path::absolute(path).ok().and_then(|absolute| {
            if path.is_file() {
                absolute.parent().map(Path::to_owned)
            } else {
                Some(absolute)
            }
        });
        let modified: BTreeMap<_, _> = project
            .files
            .iter()
            .zip(stamps)
            .filter_map(|(file, stamp)| Some((file.label.as_str(), stamp.as_ref()?.modified()?)))
            .collect();
        let mut revision_cache = None;
        let mut digests = BTreeMap::<String, blake3::Hash>::new();
        for (input, default) in [(trace, ".skylos_trace"), (coverage, ".coverage")] {
            let (requested, implicit) = match input {
                Input::Default => (Path::new(default), true),
                Input::Disabled => continue,
                Input::Path(path) => (path.as_path(), false),
            };
            let path = project.base().join(requested);
            let label = project.label(&path);
            let stamp = match fs::symlink_metadata(&path) {
                Ok(metadata) => Stamp::new(&metadata),
                Err(error) if implicit && error.kind() == ErrorKind::NotFound => continue,
                Err(error) => {
                    snapshot.reject(&label, error);
                    continue;
                }
            };
            let (bytes, stamp) = match read_snapshot(&path, &label, Some(&stamp)) {
                Ok(result) => result,
                Err(issue) => {
                    snapshot.reject(&label, issue.message);
                    continue;
                }
            };
            let sqlite = default == ".coverage" && bytes.starts_with(b"SQLite format 3\0");
            let artifact = if sqlite {
                sqlite_coverage(&bytes).map(|lines| Artifact {
                    version: 1,
                    calls: None,
                    lines,
                    revision: None,
                    fingerprints: None,
                })
            } else {
                serde_json::from_slice::<Artifact>(&bytes).map_err(|error| error.to_string())
            };
            let artifact = match artifact {
                Ok(value) => value,
                Err(error) => {
                    snapshot.reject(&label, error);
                    continue;
                }
            };
            let provenance = match (artifact.version, &artifact.revision, &artifact.fingerprints) {
                (1, None, None)
                    if sqlite || (artifact.calls.is_some() && artifact.lines.is_empty()) =>
                {
                    Provenance::Unverified
                }
                (2, Some(expected), Some(_))
                    if revision_cache.get_or_insert_with(|| revision(sources)) == expected =>
                {
                    Provenance::Verified
                }
                _ => {
                    snapshot.reject(&label, "unsupported version or mismatched source revision");
                    continue;
                }
            };
            let calls = artifact.calls.unwrap_or_default();
            if calls.len().saturating_add(artifact.lines.len()) > MAX_OBSERVATIONS
                || calls.iter().any(|call| {
                    call.count == 0
                        || call.line == 0
                        || call.file.is_empty()
                        || call.function.is_empty()
                })
                || artifact
                    .lines
                    .iter()
                    .any(|line| line.line == 0 || line.file.is_empty())
            {
                snapshot.reject(&label, "invalid observations or observation limit exceeded");
                continue;
            }
            let mut rejected = 0;
            for (file, line, function) in calls
                .into_iter()
                .map(|call| (call.file, call.line, Some(call.function)))
                .chain(
                    artifact
                        .lines
                        .into_iter()
                        .map(|line| (line.file, line.line, None)),
                )
            {
                let Some(file) =
                    selected_label(project.base(), requested_base.as_deref(), Path::new(&file))
                else {
                    continue;
                };
                let Some(source) = sources.get(&file) else {
                    continue;
                };
                let fresh = if let Some(fingerprints) = &artifact.fingerprints {
                    let digest = digests
                        .entry(file.clone())
                        .or_insert_with(|| blake3::hash(source.as_bytes()));
                    fingerprints
                        .get(&file)
                        .is_some_and(|expected| digest.to_hex().as_str() == expected)
                } else {
                    !matches!((modified.get(file.as_str()), stamp.modified()), (Some(source), Some(artifact)) if *source > artifact)
                };
                if !fresh {
                    rejected += 1;
                    continue;
                }
                snapshot.observations.insert(Observation {
                    artifact: label.clone(),
                    file,
                    line,
                    function,
                    provenance,
                });
            }
            if rejected > 0 {
                snapshot.reject(
                    &label,
                    format!("{rejected} observations have stale or missing source fingerprints"),
                );
            }
        }
        snapshot
    }

    fn reject(&mut self, path: &str, message: impl ToString) {
        self.issues
            .push(Issue::new("runtime_evidence_rejected", path, message));
    }
}

fn selected_label(base: &Path, requested: Option<&Path>, path: &Path) -> Option<String> {
    let relative = if path.is_absolute() {
        path.strip_prefix(base)
            .ok()
            .or_else(|| path.strip_prefix(requested?).ok())?
    } else {
        path
    };
    let mut parts = Vec::new();
    for component in relative.components() {
        match component {
            Component::Normal(name) => parts.push(name.to_str()?),
            Component::CurDir => {}
            _ => return None,
        }
    }
    Some(parts.join("/"))
}

fn sqlite_coverage(bytes: &[u8]) -> Result<Vec<Line>, String> {
    use rusqlite::types::ValueRef;

    // WAL databases require a separate log to establish a complete snapshot.
    if bytes.get(18..20).is_some_and(|mode| mode.contains(&2)) {
        return Err("coverage database requires a standalone SQLite snapshot".into());
    }
    // Query the same regular-file bytes that passed snapshot validation.
    let mut file = tempfile::NamedTempFile::new().map_err(|error| error.to_string())?;
    file.write_all(bytes).map_err(|error| error.to_string())?;
    let database = Connection::open_with_flags(file.path(), OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|error| error.to_string())?;
    database
        .execute_batch("PRAGMA trusted_schema = OFF")
        .map_err(|error| error.to_string())?;
    let tables: u32 = database.query_row(
        "SELECT count(*) FROM pragma_table_list WHERE schema = 'main' AND type = 'table' AND name IN ('file', 'line_bits')",
        [],
        |row| row.get(0),
    ).map_err(|error| error.to_string())?;
    if tables != 2 {
        return Err("coverage database requires file and line_bits tables".into());
    }
    let mut query = database.prepare(
        "SELECT file.path, line_bits.numbits FROM line_bits JOIN file ON file.id = line_bits.file_id LIMIT 32769",
    ).map_err(|error| error.to_string())?;
    let mut rows = query.query([]).map_err(|error| error.to_string())?;
    let mut lines = Vec::new();
    let mut row_count = 0;
    while let Some(row) = rows.next().map_err(|error| error.to_string())? {
        row_count += 1;
        if row_count > MAX_OBSERVATIONS {
            return Err("coverage row limit exceeded".into());
        }
        let file: String = row.get(0).map_err(|error| error.to_string())?;
        if file.len() > 4096 {
            return Err("coverage source path exceeds 4096 bytes".into());
        }
        let ValueRef::Blob(bits) = row.get_ref(1).map_err(|error| error.to_string())? else {
            return Err("invalid coverage line bitmap".into());
        };
        for (index, &byte) in bits.iter().enumerate() {
            for bit in 0..8 {
                let line = index * 8 + bit;
                if line > 0 && byte & (1 << bit) != 0 {
                    if lines.len() == MAX_OBSERVATIONS {
                        return Err("coverage observation limit exceeded".into());
                    }
                    lines.push(Line {
                        file: file.clone(),
                        line,
                    });
                }
            }
        }
    }
    Ok(lines)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::phase02::{Options, Profile, scan};
    use serde_json::{Value, json};

    fn options() -> Options {
        Options {
            profile: Profile::SkylosPhase03,
            grep_verify: Some(false),
            ..Options::default()
        }
    }

    fn write(root: &Path, name: &str, payload: &Value) {
        fs::write(root.join(name), serde_json::to_vec(payload).unwrap()).unwrap();
    }

    fn report(root: &Path, options: &Options) -> Value {
        serde_json::from_str(&scan(root, options).unwrap().to_json().unwrap()).unwrap()
    }

    fn unused(report: &Value) -> BTreeSet<&str> {
        report["unused_functions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|finding| finding["full_name"].as_str().unwrap())
            .collect()
    }

    #[test]
    fn verified_evidence_is_rechecked_on_warm_scans() {
        let project = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let root = project.path();
        let source = "def helper():\n    return 1\ndef traced():\n    return helper()\ndef covered():\n    return 2\ndef unused():\n    return 3\n";
        fs::write(root.join("app.py"), source).unwrap();
        let sources = BTreeMap::from([("app.py".into(), source.into())]);
        let fingerprint = blake3::hash(source.as_bytes()).to_hex().to_string();
        let artifact = json!({
            "version": 2, "revision": revision(&sources),
            "fingerprints": {"app.py": fingerprint},
            "calls": [{"file": "app.py", "function": "traced", "line": 3}],
            "lines": [{"file": "app.py", "line": 6}]
        });
        let cached = Options {
            workers: 4,
            cache_dir: Some(cache.path().into()),
            ..options()
        };
        let compare = || {
            let clean = report(root, &options());
            let mut with_cache = report(root, &cached);
            let stats = with_cache.as_object_mut().unwrap().remove("cache").unwrap();
            assert_eq!(clean, with_cache);
            assert_eq!(clean["coverage"]["status"], "complete");
            (clean, stats)
        };
        write(root, ".skylos_trace", &artifact);
        let (valid, stats) = compare();
        assert_eq!(stats["misses"], 1);
        assert_eq!(unused(&valid), BTreeSet::from(["app.unused"]));
        let evidence = valid["runtime_evidence"].as_array().unwrap();
        assert_eq!(evidence.len(), 2);
        assert!(evidence.iter().all(|row| row["provenance"] == "verified"));
        assert_eq!(evidence[0]["kind"], "runtime_trace");
        assert_eq!(evidence[1]["kind"], "runtime_coverage");

        for (field, invalid) in [
            ("revision", json!("stale")),
            ("fingerprints", json!({"app.py": "wrong"})),
            (
                "calls",
                json!([{"file": "app.py", "function": "traced", "line": 8}]),
            ),
            (
                "calls",
                json!([{"file": "app.py", "function": "traced", "line": 3, "count": 0}]),
            ),
        ] {
            let mut changed = artifact.clone();
            changed[field] = invalid;
            write(root, ".skylos_trace", &changed);
            let (rejected, stats) = compare();
            assert_eq!(stats["hits"], 1);
            assert!(unused(&rejected).contains("app.traced"));
            assert_eq!(
                rejected["runtime_evidence_issues"][0]["kind"],
                "runtime_evidence_rejected"
            );
        }
        write(root, ".skylos_trace", &artifact);
        fs::write(root.join("app.py"), format!("{source}# newer source\n")).unwrap();
        let (stale, _) = compare();
        assert!(stale.get("runtime_evidence").is_none());
        assert!(unused(&stale).contains("app.traced"));
    }

    #[test]
    fn coverage_database_imports_only_positive_body_lines() {
        let project = tempfile::tempdir().unwrap();
        let root = project.path();
        fs::write(
            root.join("app.py"),
            "def covered():\n    def inner():\n        return 1\n    return 2\ndef unused():\n    return 3\n",
        ).unwrap();
        let path = root.join(".coverage");
        let database = Connection::open(path).unwrap();
        database
            .execute_batch(
                "CREATE TABLE file (id INTEGER PRIMARY KEY, path TEXT);
             CREATE TABLE line_bits (file_id INTEGER, context_id INTEGER, numbits BLOB);
             INSERT INTO file VALUES (1, 'app.py');
             INSERT INTO line_bits VALUES (1, 1, X'32');",
            )
            .unwrap();
        drop(database);
        let covered = report(root, &options());
        assert_eq!(covered["coverage"]["status"], "complete");
        assert_eq!(
            unused(&covered),
            BTreeSet::from(["app.covered.inner", "app.unused"])
        );
        assert_eq!(covered["runtime_evidence"][0]["line"], 4);
        assert_eq!(covered["runtime_evidence"][0]["provenance"], "unverified");
        let disabled = report(
            root,
            &Options {
                coverage_file: Input::Disabled,
                ..options()
            },
        );
        assert!(unused(&disabled).contains("app.covered"));
        let phase02 = report(root, &Options::default());
        assert!(unused(&phase02).contains("app.covered"));
    }

    #[test]
    fn artifact_rejection_does_not_change_static_incomplete_coverage() {
        let project = tempfile::tempdir().unwrap();
        let root = project.path();
        let source = root.join("app.py");
        fs::write(&source, "def unused():\n    return 1\n").unwrap();
        fs::write(root.join(".skylos_trace"), "malformed").unwrap();
        let malformed = report(root, &options());
        assert_eq!(malformed["coverage"]["status"], "complete");
        assert_eq!(
            malformed["runtime_evidence_issues"][0]["kind"],
            "runtime_evidence_rejected"
        );
        assert!(unused(&malformed).contains("app.unused"));

        write(
            root,
            ".skylos_trace",
            &json!({
                "version": 1, "calls": [{"file": "app.py", "function": "unused", "line": 1}]
            }),
        );
        let artifact_time = fs::metadata(root.join(".skylos_trace"))
            .unwrap()
            .modified()
            .unwrap();
        fs::File::options()
            .write(true)
            .open(&source)
            .unwrap()
            .set_times(
                fs::FileTimes::new()
                    .set_modified(artifact_time + std::time::Duration::from_secs(10)),
            )
            .unwrap();
        let stale = report(root, &options());
        assert_eq!(stale["coverage"]["status"], "complete");
        assert!(
            stale["runtime_evidence_issues"][0]["message"]
                .as_str()
                .unwrap()
                .contains("stale")
        );
        assert!(unused(&stale).contains("app.unused"));

        fs::write(&source, "def missing():\n    return @\n").unwrap();
        let incomplete = report(root, &options());
        assert_eq!(incomplete["coverage"]["status"], "incomplete");
        assert!(
            incomplete["unused_functions"]
                .as_array()
                .unwrap()
                .is_empty()
        );
    }
}
