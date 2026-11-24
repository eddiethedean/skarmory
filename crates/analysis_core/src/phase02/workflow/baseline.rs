use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::super::{Diagnostic, Report};
use super::{ConfigError, git};

pub(super) const DEFAULT_PATH: &str = ".skylos/baseline.json";
const VERSION: u8 = 1;
const MAX_BYTES: usize = 2_000_000;

#[derive(Clone, Debug)]
pub enum Input {
    File(PathBuf),
    Revision(String),
}

#[derive(Debug, Serialize)]
pub(in crate::phase02) struct Summary {
    pub(super) status: &'static str,
    pub(super) matched_count: usize,
    pub(super) stale_count: Option<usize>,
    pub(super) message: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Identity {
    rule: String,
    path: String,
    symbol: String,
}

impl From<&Diagnostic> for Identity {
    fn from(item: &Diagnostic) -> Self {
        Self {
            rule: item.rule_id.into(),
            path: item.file.clone(),
            symbol: item.identity_name.clone(),
        }
    }
}

pub(super) type Inventory = BTreeMap<Identity, usize>;

pub(super) fn inventory(report: &Report) -> Inventory {
    let mut counts = Inventory::new();
    for item in report.categories.iter() {
        *counts.entry(Identity::from(item)).or_default() += 1;
    }
    counts
}

pub(super) fn take(inventory: &mut Inventory, item: &Diagnostic) -> bool {
    let Some(count) = inventory.get_mut(&Identity::from(item)) else {
        return false;
    };
    if *count == 0 {
        return false;
    }
    *count -= 1;
    true
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Baseline {
    schema_version: u8,
    findings: Vec<Identity>,
}

pub(super) fn read(input: &Input, project: &Path) -> Result<Inventory, ConfigError> {
    let bytes = match input {
        Input::File(path) => read_file(&project.join(path))?,
        Input::Revision(reference) => {
            let repository = git::Repository::open(project)?;
            let commit = repository.revision(reference)?;
            let scope = project
                .strip_prefix(&repository.root)
                .map_err(ConfigError::new)?;
            let path = super::super::preview_source::display(&scope.join(DEFAULT_PATH));
            repository.read_path(&commit, &path, MAX_BYTES)?
        }
    };
    let baseline: Baseline = serde_json::from_slice(&bytes).map_err(ConfigError::new)?;
    if baseline.schema_version != VERSION {
        return Err(ConfigError::new(format!(
            "unsupported baseline schema version {}, expected {VERSION}",
            baseline.schema_version
        )));
    }
    let mut known = Inventory::new();
    for identity in baseline.findings {
        if !git::safe_path(&identity.path)
            || !super::DEAD_CODE_RULES.contains(&identity.rule.as_str())
            || (identity.rule == "SKY-E002") != identity.symbol.is_empty()
        {
            return Err(ConfigError::new("invalid dead-code baseline entry"));
        }
        *known.entry(identity).or_default() += 1;
    }
    Ok(known)
}

fn read_file(path: &Path) -> Result<Vec<u8>, ConfigError> {
    if !fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_file()) {
        return Err(ConfigError::new(format!(
            "{} is not a regular baseline file",
            path.display()
        )));
    }
    let mut data = Vec::new();
    File::open(path)
        .map_err(ConfigError::new)?
        .take(MAX_BYTES as u64 + 1)
        .read_to_end(&mut data)
        .map_err(ConfigError::new)?;
    if data.len() > MAX_BYTES {
        return Err(ConfigError::new("baseline exceeds the 2 MB limit"));
    }
    Ok(data)
}

pub(super) fn write(path: &Path, report: &Report) -> Result<Summary, ConfigError> {
    if !report.is_complete() {
        return Ok(Summary {
            status: "incomplete",
            matched_count: 0,
            stale_count: None,
            message: Some("baseline requires complete analysis".into()),
        });
    }
    let findings = inventory(report)
        .into_iter()
        .flat_map(|(identity, count)| std::iter::repeat_n(identity, count))
        .collect();
    let mut bytes = serde_json::to_vec_pretty(&Baseline {
        schema_version: VERSION,
        findings,
    })
    .map_err(ConfigError::new)?;
    bytes.push(b'\n');
    if bytes.len() > MAX_BYTES {
        return Err(ConfigError::new("baseline exceeds the 2 MB limit"));
    }
    if fs::symlink_metadata(path).is_ok_and(|metadata| !metadata.is_file()) {
        return Err(ConfigError::new(
            "baseline destination is not a regular file",
        ));
    }
    let parent = path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent).map_err(ConfigError::new)?;
    let mut file = tempfile::NamedTempFile::new_in(parent).map_err(ConfigError::new)?;
    file.write_all(&bytes)
        .and_then(|()| file.flush())
        .map_err(ConfigError::new)?;
    file.persist(path)
        .map_err(|error| ConfigError::new(io::Error::from(error)))?;
    Ok(Summary {
        status: "written",
        matched_count: 0,
        stale_count: None,
        message: None,
    })
}
