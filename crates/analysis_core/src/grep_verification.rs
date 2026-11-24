//! Bounded textual verification over the selected project's configuration.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Component, Path, PathBuf};

use serde::Serialize;
use skarmory_python::Location;

use crate::Issue;
use crate::phase02::config::Selection;
use crate::preview_source::{Project, Stamp, read_snapshot};

const MAX_DIRECTORY_ENTRIES: usize = 65_536;
const MAX_CONFIG_FILES: usize = 256;
const MAX_CONFIG_BYTES: u64 = 4 * 1024 * 1024;
const MAX_REFERENCES: usize = 4_096;
pub(super) const MAX_EVIDENCE: usize = 16_384;

#[derive(Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub(super) struct Evidence {
    pub symbol_id: String,
    pub qualified_name: String,
    pub definition_path: String,
    pub definition_line: usize,
    pub definition_column: usize,
    pub path: String,
    pub line: usize,
    pub column: usize,
    pub kind: &'static str,
    pub certainty: &'static str,
}

#[derive(Debug)]
pub(super) struct ConfigReference {
    pub path: String,
    pub at: Location,
    pub kind: &'static str,
}

type Inventory = BTreeMap<PathBuf, Stamp>;
type References = BTreeMap<String, Vec<ConfigReference>>;

#[derive(Default)]
pub(super) struct Snapshot {
    inventory: Option<Inventory>,
    module_paths: References,
    qualified_paths: References,
    qualified_names: References,
    issues: Vec<Issue>,
}

impl Snapshot {
    pub(super) fn input_stamps(&self) -> impl Iterator<Item = (&PathBuf, &Stamp)> {
        self.inventory.iter().flat_map(|inventory| inventory.iter())
    }

    pub(super) fn has_configuration_references(&self) -> bool {
        !self.module_paths.is_empty()
            || !self.qualified_paths.is_empty()
            || !self.qualified_names.is_empty()
    }

    pub(super) fn read(project: &Project, selection: Option<&Selection>) -> Self {
        let mut snapshot = Self::default();
        let inventory = match inventory(project, selection) {
            Ok(inventory) => inventory,
            Err(issue) => {
                snapshot.issues.push(issue);
                return snapshot;
            }
        };
        let mut references = 0;
        'files: for (path, stamp) in &inventory {
            let label = project.label(path);
            let source = match read_configuration(path, &label, stamp) {
                Ok(source) => source,
                Err(issue) => {
                    snapshot.issues.push(issue);
                    continue;
                }
            };
            let mut scalars = Scalars::default();
            let mut lines = source.split('\n').enumerate();
            while let Some((line, text)) = lines.next() {
                let remaining = lines.clone().map(|(_, text)| text);
                let Some((target, column)) = scalars.value(text, line == 0, remaining) else {
                    continue;
                };
                let Some((key, kind, target_map)) = snapshot.target(target) else {
                    continue;
                };
                if references == MAX_REFERENCES {
                    snapshot.issues.push(
                        budget(
                            &label,
                            format!("exceeds {MAX_REFERENCES} configuration references"),
                        )
                        .at(Location {
                            line: line + 1,
                            column,
                        }),
                    );
                    break 'files;
                }
                references += 1;
                target_map.entry(key).or_default().push(ConfigReference {
                    path: label.clone(),
                    at: Location {
                        line: line + 1,
                        column,
                    },
                    kind,
                });
            }
        }
        snapshot.inventory = Some(inventory);
        snapshot
    }

    fn target(&mut self, target: &str) -> Option<(String, &'static str, &mut References)> {
        if let Some((owner, member)) = target.split_once(':') {
            if !dotted_name(member) {
                return None;
            }
            let (owner, targets) = if let Some(path) = python_path(owner) {
                (path, &mut self.qualified_paths)
            } else if dotted_name(owner) {
                (owner.to_owned(), &mut self.qualified_names)
            } else {
                return None;
            };
            return Some((format!("{owner}:{member}"), "configuration_symbol", targets));
        }
        if let Some(path) = python_path(target) {
            return Some((path, "configuration_module", &mut self.module_paths));
        }
        (target.contains('.') && dotted_name(target)).then(|| {
            (
                target.to_owned(),
                "configuration_symbol",
                &mut self.qualified_names,
            )
        })
    }

    pub(super) fn matches<'a>(
        &'a self,
        path: &str,
        module: &str,
        member: &str,
        include_module_reference: bool,
    ) -> impl Iterator<Item = &'a ConfigReference> {
        let module_file = include_module_reference
            .then(|| self.module_paths.get(path))
            .flatten();
        // Filenames and dotted module names can have identical spellings.
        let by_path = (!self.qualified_paths.is_empty())
            .then(|| self.qualified_paths.get(&format!("{path}:{member}")))
            .flatten();
        let [by_module, by_qualified_name] = if self.qualified_names.is_empty() {
            [None; 2]
        } else {
            [
                self.qualified_names.get(&format!("{module}:{member}")),
                self.qualified_names.get(&format!("{module}.{member}")),
            ]
        };
        [module_file, by_path, by_module, by_qualified_name]
            .into_iter()
            .flatten()
            .flatten()
    }

    pub(super) fn finish(mut self, project: &Project, selection: Option<&Selection>) -> Vec<Issue> {
        if let Some(previous) = self.inventory.take() {
            match inventory(project, selection) {
                Ok(current) if previous != current => self.issues.push(Issue::new(
                    "configuration_changed",
                    ".",
                    "grep verification configuration changed during analysis",
                )),
                Err(issue) => self.issues.push(issue),
                Ok(_) => {}
            }
        }
        self.issues
    }
}

fn inventory(project: &Project, selection: Option<&Selection>) -> Result<Inventory, Issue> {
    let mut files = BTreeMap::new();
    let mut entries = 0;
    let mut bytes = 0;
    for directory in project.verification_directories() {
        let label = project.label(directory);
        let read_error = |error: std::io::Error| Issue::new("grep_read_error", &label, error);
        if fs::canonicalize(directory).map_err(read_error)? != *directory {
            return Err(Issue::new(
                "configuration_changed",
                &label,
                "grep verification directory was replaced",
            ));
        }
        for child in fs::read_dir(directory).map_err(read_error)? {
            entries += 1;
            if entries > MAX_DIRECTORY_ENTRIES {
                return Err(budget(
                    &label,
                    format!("exceeds {MAX_DIRECTORY_ENTRIES} configuration directory entries"),
                ));
            }
            let child = child.map_err(read_error)?;
            let path = child.path();
            if !matches!(
                path.extension().and_then(|ext| ext.to_str()),
                Some("yaml" | "yml")
            ) || selection
                .is_some_and(|selection| selection.exclusion(&project.label(&path)).is_some())
            {
                continue;
            }
            let metadata = child.metadata().map_err(read_error)?;
            if !metadata.is_file() {
                continue;
            }
            if files.len() == MAX_CONFIG_FILES {
                return Err(budget(
                    &label,
                    format!("exceeds {MAX_CONFIG_FILES} configuration files"),
                ));
            }
            bytes = u64::saturating_add(bytes, metadata.len());
            if bytes > MAX_CONFIG_BYTES {
                return Err(budget(
                    &label,
                    "configuration exceeds the 4 MiB read budget",
                ));
            }
            files.insert(path, Stamp::new(&metadata));
        }
    }
    Ok(files)
}

fn budget(path: &str, message: impl ToString) -> Issue {
    Issue::new("grep_verification_limit", path, message)
}

pub(super) fn evidence_limit() -> Issue {
    budget(
        ".",
        format!("exceeds {MAX_EVIDENCE} grep verification evidence entries"),
    )
}

fn read_configuration(path: &Path, label: &str, stamp: &Stamp) -> Result<String, Issue> {
    let (bytes, _) = read_snapshot(path, label, Some(stamp)).map_err(|issue| match issue.kind {
        "source_limit" => budget(label, "configuration file exceeds 1 MiB"),
        "source_changed" => Issue::new("configuration_changed", label, issue.message),
        _ => Issue::new("grep_read_error", label, issue.message),
    })?;
    String::from_utf8(bytes)
        .map(|source| source.replace("\r\n", "\n").replace('\r', "\n"))
        .map_err(|error| Issue::new("grep_read_error", label, error))
}

fn dotted_name(name: &str) -> bool {
    name.split('.').all(skarmory_python::simple_name)
}

fn python_path(text: &str) -> Option<String> {
    let path = text.strip_prefix("./").unwrap_or(text);
    let valid = path.ends_with(".py")
        && !path.contains(['\\', ':'])
        && path
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
        && Path::new(path)
            .components()
            .all(|part| matches!(part, Component::Normal(_)));
    valid.then(|| path.to_owned())
}

#[derive(Default)]
struct Scalars {
    multiline_indent: Option<usize>,
    open_quote: Option<u8>,
}

impl Scalars {
    // Single-line block mapping values and sequence items are the supported
    // YAML subset. Ignore comments and skip interior text of multiline scalars.
    fn value<'a>(
        &mut self,
        line: &'a str,
        first: bool,
        remaining: impl Iterator<Item = &'a str>,
    ) -> Option<(&'a str, usize)> {
        let visible = if first {
            line.strip_prefix('\u{feff}').unwrap_or(line)
        } else {
            line
        };
        if let Some(quote) = self.open_quote {
            if quoted_end(visible, quote).is_some() {
                self.open_quote = None;
            }
            return None;
        }
        let entry = visible.trim_start_matches(' ');
        let indent = visible.len() - entry.len();
        if entry.is_empty()
            || entry.starts_with('#')
            || self.multiline_indent.is_some_and(|parent| indent > parent)
        {
            return None;
        }
        self.multiline_indent = None;
        let sequence = entry.strip_prefix("- ");
        let entry = sequence.unwrap_or(entry).trim_start();
        let mapping = mapping_value(entry);
        let value = mapping.or(sequence)?.trim_start();
        // For "- key: value", a sibling key starts at the key indentation.
        let parent = if mapping.is_some() {
            visible.len() - entry.len()
        } else {
            indent
        };
        if value.starts_with(['|', '>']) {
            self.multiline_indent = Some(parent);
            return None;
        }
        let column = line.len() - value.len() + 1;
        if let Some(quote @ (b'\'' | b'"')) = value.as_bytes().first().copied() {
            let quoted = &value[1..];
            let Some(end) = quoted_end(quoted, quote) else {
                self.open_quote = Some(quote);
                return None;
            };
            let tail = quoted[end + 1..].trim();
            return (tail.is_empty() || tail.starts_with('#'))
                .then_some((&quoted[..end], column + 1));
        }
        // A comment marker inside a plain scalar is literal without whitespace.
        let end = value
            .match_indices('#')
            .find(|(index, _)| *index == 0 || value.as_bytes()[index - 1].is_ascii_whitespace())
            .map_or(value.len(), |(index, _)| index);
        let value = value[..end].trim_end();
        if !value.is_empty()
            && remaining
                .map(|line| (line, line.trim_start_matches(' ')))
                .find(|(_, content)| !content.is_empty() && !content.starts_with('#'))
                .is_some_and(|(line, content)| line.len() - content.len() > parent)
        {
            self.multiline_indent = Some(parent);
            return None;
        }
        Some((value, column))
    }
}

fn mapping_value(entry: &str) -> Option<&str> {
    let value = if let Some(quote @ (b'\'' | b'"')) = entry.as_bytes().first().copied() {
        let end = quoted_end(&entry[1..], quote)?;
        entry[end + 2..]
            .trim_start_matches([' ', '\t'])
            .strip_prefix(':')?
    } else {
        let (key, value) = entry.split_once(':')?;
        let key = key.trim_end();
        if key.is_empty()
            || !key
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
        {
            return None;
        }
        value
    };
    (value.is_empty() || value.starts_with([' ', '\t'])).then_some(value)
}

fn quoted_end(value: &str, quote: u8) -> Option<usize> {
    let mut bytes = value.bytes().enumerate().peekable();
    while let Some((index, byte)) = bytes.next() {
        if quote == b'"' && byte == b'\\' {
            bytes.next();
        } else if byte == quote {
            if quote == b'\'' && bytes.peek().is_some_and(|(_, next)| *next == quote) {
                bytes.next();
            } else {
                return Some(index);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::preview_source::{FileEntry, discover};

    #[test]
    fn yaml_matches_preserve_location_and_owner() {
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("jobs.yaml");
        fs::write(
            &config,
            concat!(
                "# callback: south/tasks.py\n",
                "description: mentions south/tasks.py\n",
                "notes: |\n",
                "  callback: south/tasks.py\n",
                "long_string: 'documentation\n",
                "  callback: south/tasks.py'\n",
                "jobs:\n",
                "  - module: 'north/tasks.py' # south/tasks.py\n",
                "  - callback: north/tasks.py:Handler.start\n",
                "  - north.tasks.run\n",
                "  - south.tasks:start_backup\n",
            ),
        )
        .unwrap();
        let project = discover(root.path(), &[], FileEntry::None);
        let snapshot = Snapshot::read(&project, None);
        let module_match = snapshot
            .matches("north/tasks.py", "north.tasks", "worker", true)
            .collect::<Vec<_>>();
        assert_eq!(module_match.len(), 1);
        assert_eq!(module_match[0].kind, "configuration_module");
        assert_eq!(module_match[0].path, "jobs.yaml");
        assert_eq!(
            (module_match[0].at.line, module_match[0].at.column),
            (8, 14)
        );
        let method = snapshot
            .matches("north/tasks.py", "north.tasks", "Handler.start", false)
            .next()
            .unwrap();
        assert_eq!(method.kind, "configuration_symbol");
        assert_eq!((method.at.line, method.at.column), (9, 15));
        assert_eq!(
            snapshot
                .matches("north/tasks.py", "north.tasks", "run", false)
                .count(),
            1
        );
        assert_eq!(
            snapshot
                .matches("south/tasks.py", "south.tasks", "start", true)
                .count(),
            0
        );
        assert_eq!(
            snapshot
                .matches("south/tasks.py", "south.tasks", "start_backup", true)
                .count(),
            1
        );
        assert!(snapshot.finish(&project, None).is_empty());
    }

    #[test]
    fn quoted_yaml_keys_preserve_block_scalar_boundaries() {
        let root = tempfile::tempdir().unwrap();
        fs::write(
            root.path().join("jobs.yaml"),
            concat!(
                "\"documentation\": |\n",
                "  handler: worker.py:run\n",
                "steps:\n",
                "  - 'escaped'' colon: in key': >-\n",
                "      handler: worker.py\n",
                "    \"handler\": 'selected.py:run'\n",
            ),
        )
        .unwrap();
        let project = discover(root.path(), &[], FileEntry::None);
        let snapshot = Snapshot::read(&project, None);
        assert_eq!(
            snapshot.matches("worker.py", "worker", "run", true).count(),
            0
        );
        let target = snapshot
            .matches("selected.py", "selected", "run", true)
            .next()
            .unwrap();
        assert_eq!(target.path, "jobs.yaml");
        assert_eq!((target.at.line, target.at.column), (6, 17));
        assert!(snapshot.finish(&project, None).is_empty());
    }

    #[test]
    fn plain_yaml_matches_must_be_single_line() {
        let root = tempfile::tempdir().unwrap();
        fs::write(
            root.path().join("jobs.yaml"),
            concat!(
                "handler: worker.py:run\n",
                "\n",
                "  additional prose\n",
                "steps:\n",
                "  - worker.py\n",
                "    more prose\n",
                "  - handler: selected.py:run\n",
                "    label: standalone\n",
            ),
        )
        .unwrap();
        let project = discover(root.path(), &[], FileEntry::None);
        let snapshot = Snapshot::read(&project, None);
        assert_eq!(
            snapshot.matches("worker.py", "worker", "run", true).count(),
            0
        );
        let target = snapshot
            .matches("selected.py", "selected", "run", true)
            .next()
            .unwrap();
        assert_eq!((target.at.line, target.at.column), (7, 14));
        assert!(snapshot.finish(&project, None).is_empty());
    }

    #[test]
    fn yaml_file_qualified_names_do_not_match_dotted_module_owners() {
        let root = tempfile::tempdir().unwrap();
        fs::write(
            root.path().join("jobs.yaml"),
            "file: workflow.py:run\nmodule: workflow.py.run\n",
        )
        .unwrap();
        let project = discover(root.path(), &[], FileEntry::None);
        let snapshot = Snapshot::read(&project, None);
        let matching_lines = |path, module| {
            snapshot
                .matches(path, module, "run", false)
                .map(|reference| reference.at.line)
                .collect::<Vec<_>>()
        };
        assert_eq!(matching_lines("workflow.py", "workflow"), [1]);
        assert_eq!(matching_lines("workflow/py.py", "workflow.py"), [2]);
        assert!(snapshot.finish(&project, None).is_empty());
    }

    #[test]
    fn yaml_scope_and_snapshot_track_only_selected_configuration() {
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("jobs.yml");
        fs::write(&config, "handler: selected.py\n").unwrap();
        let input = root.path().join("selected.py");
        fs::write(&input, "").unwrap();
        let excluded = root.path().join(".venv");
        fs::create_dir(&excluded).unwrap();
        fs::write(excluded.join("jobs.yml"), "handler: excluded.py\n").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(excluded.join("jobs.yml"), root.path().join("linked.yaml"))
            .unwrap();
        let script = discover(&input, &[], FileEntry::None);
        let snapshot = Snapshot::read(&script, None);
        assert_eq!(
            snapshot
                .matches("selected.py", "selected", "run", true)
                .count(),
            0
        );
        assert!(snapshot.finish(&script, None).is_empty());
        let project = discover(root.path(), &[], FileEntry::None);
        let snapshot = Snapshot::read(&project, None);
        assert_eq!(
            snapshot
                .matches("selected.py", "selected", "run", true)
                .count(),
            1
        );
        assert_eq!(
            snapshot
                .matches("excluded.py", "excluded", "run", true)
                .count(),
            0
        );
        fs::write(config, "handler: replacement.py\n").unwrap();
        assert_eq!(
            snapshot.finish(&project, None)[0].kind,
            "configuration_changed"
        );
    }

    #[test]
    fn reference_budget_keeps_positive_evidence_and_reports_unchecked_scope() {
        let root = tempfile::tempdir().unwrap();
        fs::write(
            root.path().join("jobs.yml"),
            "handler: worker.py\n".repeat(MAX_REFERENCES + 1),
        )
        .unwrap();
        let project = discover(root.path(), &[], FileEntry::None);
        let snapshot = Snapshot::read(&project, None);
        assert_eq!(
            snapshot.matches("worker.py", "worker", "run", true).count(),
            MAX_REFERENCES
        );
        let issues = snapshot.finish(&project, None);
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].kind, "grep_verification_limit");
        assert_eq!(issues[0].line, Some(MAX_REFERENCES + 1));
    }

    #[test]
    fn native_dynamic_dispatch_retains_only_the_selected_owner_and_its_callee() {
        let root = tempfile::tempdir().unwrap();
        fs::write(
            root.path().join("main.py"),
            concat!(
                "import north\n",
                "import south\n",
                "# getattr(south, 'run')()\n",
                "note = \"getattr(south, 'run')()\"\n",
                "getattr(north, 'run')()\n",
            ),
        )
        .unwrap();
        fs::write(
            root.path().join("north.py"),
            "def helper():\n    return 1\n\ndef run():\n    return helper()\n",
        )
        .unwrap();
        fs::write(root.path().join("south.py"), "def run():\n    return 2\n").unwrap();
        let (report, _) = crate::scan(
            root.path(),
            &crate::ScanOptions {
                grep_verify: true,
                ..crate::ScanOptions::default()
            },
        );
        let report: serde_json::Value = serde_json::from_str(&report.to_json().unwrap()).unwrap();
        assert_eq!(report["coverage"]["status"], "complete", "{report}");
        let evidence = report["grep_verification_evidence"].as_array().unwrap();
        assert_eq!(evidence.len(), 1);
        assert_eq!(evidence[0]["qualified_name"], "north.run");
        assert_eq!(evidence[0]["definition_path"], "north.py");
        assert_eq!(evidence[0]["path"], "main.py");
        assert_eq!(evidence[0]["line"], 5);
        assert_eq!(evidence[0]["column"], 16);
        assert_eq!(evidence[0]["kind"], "dynamic_member");
        assert_eq!(evidence[0]["certainty"], "possible");
        let findings = report["findings"].as_array().unwrap();
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0]["path"], "south.py");
        assert_eq!(findings[0]["symbol"], "run");
        let reachability = report["evidence"].as_array().unwrap();
        assert!(
            reachability
                .iter()
                .all(|event| event["symbol_id"] != evidence[0]["symbol_id"])
        );
        assert!(reachability.iter().any(|event| {
            event["symbol_id"]
                .as_str()
                .is_some_and(|id| id.contains("helper"))
                && event["kind"] == "call"
                && event["certainty"] == "possible"
        }));
    }

    #[test]
    fn native_standalone_lookup_can_reach_a_callee_after_an_ordinary_call() {
        let root = tempfile::tempdir().unwrap();
        fs::write(
            root.path().join("main.py"),
            "import worker\nworker.run(False)\ngetattr(worker, 'run')\n",
        )
        .unwrap();
        fs::write(
            root.path().join("worker.py"),
            concat!(
                "def helper():\n",
                "    return 1\n\n",
                "def run(enabled):\n",
                "    if enabled:\n",
                "        return helper()\n",
            ),
        )
        .unwrap();
        let (report, _) = crate::scan(
            root.path(),
            &crate::ScanOptions {
                grep_verify: true,
                ..crate::ScanOptions::default()
            },
        );
        let report: serde_json::Value = serde_json::from_str(&report.to_json().unwrap()).unwrap();
        assert_eq!(report["coverage"]["status"], "complete", "{report}");
        assert!(report["findings"].as_array().unwrap().is_empty());
        let evidence = report["grep_verification_evidence"].as_array().unwrap();
        assert_eq!(evidence.len(), 1);
        assert_eq!(evidence[0]["qualified_name"], "worker.run");
        assert_eq!(evidence[0]["line"], 3);
    }
}
