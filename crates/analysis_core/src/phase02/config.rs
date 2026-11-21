use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use globset::{GlobBuilder, GlobMatcher};
use serde::Deserialize;
use time::{Date, Month, OffsetDateTime};

use super::{ConfigError, Finding, Options};

const MAX_CONFIG_BYTES: u64 = 1024 * 1024;
const MAX_CONFIG_LAYERS: usize = 64;
const DEFAULT_EXCLUDES: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    ".venv",
    "venv",
    "__pycache__",
    "node_modules",
    ".mypy_cache",
    ".pytest_cache",
    "build",
    "dist",
    ".tox",
    "htmlcov",
    ".coverage",
    "vendor",
    ".next",
    ".nuxt",
    ".turbo",
    ".idea",
    ".vscode",
];

pub(super) struct Config {
    pub(super) entrypoints: Vec<Entrypoint>,
    pub(super) confidence: u8,
    pub(super) sources: Vec<String>,
    pub(super) selection: Selection,
    pub(super) warnings: Vec<String>,
    digests: Vec<blake3::Hash>,
    path_rules: PathRules,
    date: Date,
}

pub(crate) struct Selection {
    exclude: Vec<Pattern>,
    include: Vec<Pattern>,
    keep_generated: Vec<Pattern>,
    defaults: bool,
    include_generated: bool,
}

#[derive(Clone)]
struct Pattern {
    text: String,
    matcher: GlobMatcher,
    anchored: bool,
}

impl Pattern {
    fn new(pattern: &str) -> Result<Self, ConfigError> {
        if pattern.is_empty() {
            return Err(ConfigError::new("empty configuration pattern"));
        }
        let text = pattern.replace('\\', "/");
        let matcher = GlobBuilder::new(&translate_glob(&text))
            .case_insensitive(cfg!(windows))
            .literal_separator(false)
            .backslash_escape(false)
            .build()
            .map_err(|error| ConfigError::new(format!("invalid pattern {pattern:?}: {error}")))?
            .compile_matcher();
        Ok(Self {
            text,
            matcher,
            anchored: false,
        })
    }

    fn selection(value: &str, input: &Path) -> Result<Self, ConfigError> {
        let path = Path::new(value);
        if !path.is_absolute() {
            let text = value.replace('\\', "/");
            let normalized = text.trim_start_matches("./").trim_end_matches('/');
            return Self::new(if normalized.is_empty() {
                &text
            } else {
                normalized
            });
        }
        let mut text = value.to_owned();
        if let Ok(root) = fs::canonicalize(input) {
            let root = if root.is_file() {
                root.parent().unwrap_or(&root)
            } else {
                &root
            };
            let absolute = fs::canonicalize(path).unwrap_or_else(|_| path.to_owned());
            if let Ok(relative) = absolute.strip_prefix(root)
                && !relative.as_os_str().is_empty()
            {
                text = relative.to_string_lossy().into_owned();
            }
        }
        let mut pattern = Self::new(&text)?;
        // A literal absolute path must not also match its relative suffix in
        // a different directory under the project.
        pattern.anchored = true;
        Ok(pattern)
    }

    fn matches(&self, value: &str) -> bool {
        self.matcher.is_match(value)
    }

    fn path_matches(&self, value: &str) -> bool {
        self.matches(value)
            || (!self.text.contains('/') && value.split('/').any(|part| self.matches(part)))
            || value.ends_with(&self.text)
    }

    fn selection_matches(&self, value: &str) -> bool {
        if self.matches(value) {
            return true;
        }
        if self.anchored {
            return false;
        }
        if !self.text.contains('/') {
            return value.split('/').any(|part| self.matches(part));
        }
        !self.has_glob()
            && value
                .match_indices('/')
                .any(|(end, _)| self.matches(&value[end + 1..]))
    }

    fn selection_matches_ancestor(&self, value: &str) -> bool {
        self.selection_matches(value)
            || value
                .match_indices('/')
                .any(|(end, _)| self.selection_matches(&value[..end]))
    }

    fn has_glob(&self) -> bool {
        self.text.contains(['*', '?', '['])
    }
}

fn translate_glob(pattern: &str) -> String {
    // fnmatch treats braces as literals and does not give runs of stars a
    // different meaning when a wildcard can already match a separator.
    let mut translated = String::new();
    let mut star = false;
    for character in pattern.chars() {
        match character {
            '*' if star => continue,
            '{' => translated.push_str("[{]"),
            '}' => translated.push_str("[}]"),
            _ => translated.push(character),
        }
        star = character == '*';
    }
    translated
}

impl Selection {
    pub(crate) fn exclusion(&self, label: &str) -> Option<&'static str> {
        if self
            .include
            .iter()
            .any(|pattern| pattern.selection_matches_ancestor(label))
        {
            return None;
        }
        if self
            .exclude
            .iter()
            .any(|pattern| pattern.selection_matches_ancestor(label))
        {
            return Some("configured_exclusion");
        }
        if self.defaults
            && label
                .split('/')
                .any(|part| DEFAULT_EXCLUDES.contains(&part) || part.ends_with(".egg-info"))
        {
            return Some("default_exclusion");
        }
        None
    }

    pub(crate) fn may_include_children(&self, label: &str) -> bool {
        // A relative pattern can match a path suffix that begins anywhere
        // below this directory, even when the directory itself is excluded.
        self.include.iter().any(|pattern| {
            !pattern.anchored
                || pattern.has_glob()
                || pattern.text.starts_with(&format!("{label}/"))
        })
    }

    pub(super) fn suppress_generated_findings(&self, label: &str, source: &str) -> bool {
        !self.include_generated
            && !self
                .keep_generated
                .iter()
                .chain(&self.include)
                .any(|pattern| pattern.selection_matches_ancestor(label))
            && source
                .lines()
                .take(80)
                .map(str::trim_start)
                .take_while(|line| line.is_empty() || line.starts_with('#'))
                .any(generated_marker)
    }
}

fn generated_marker(line: &str) -> bool {
    let line = line.to_ascii_lowercase();
    let words: Vec<_> = line
        .split(|ch: char| !(ch.is_alphanumeric() || ch == '_' || ch == '@'))
        .filter(|word| !word.is_empty())
        .collect();
    let do_not_edit = words
        .windows(3)
        .rposition(|words| words == ["do", "not", "edit"]);
    for start in 0..words.len() {
        let text = &words[start..];
        match text {
            ["@generated", ..] => return true,
            ["code", "generated", ..] | ["generated", "by", ..]
                if do_not_edit.is_some_and(|end| end >= start + 2) =>
            {
                return true;
            }
            ["this", "file" | "module" | "code", "is" | "was", rest @ ..]
            | ["this", "file" | "module" | "code", "has", "been", rest @ ..]
                if matches!(
                    rest,
                    ["generated" | "autogenerated", ..]
                        | ["auto" | "automatically", "generated", ..]
                ) =>
            {
                return true;
            }
            [
                "autogenerated",
                "by" | "from" | "file" | "code" | "using" | "with",
                ..,
            ]
            | [
                "auto" | "automatically",
                "generated",
                "by" | "from" | "file" | "code" | "using" | "with",
                ..,
            ] => {
                return true;
            }
            _ => {}
        }
    }
    false
}

#[derive(Default)]
struct PathRules {
    ignores: Vec<String>,
    whitelist: Vec<Pattern>,
    documented: Vec<(Pattern, String)>,
    temporary: Vec<(Pattern, String)>,
    lower: Vec<Pattern>,
    overrides: Vec<(Pattern, Vec<Pattern>)>,
}

impl Config {
    pub(super) fn decision(&self, finding: &Finding) -> Decision {
        if self
            .path_rules
            .ignores
            .iter()
            .any(|rule| rule == finding.rule_id)
        {
            return Decision::Suppress(format!("rule ignored ({})", finding.rule_id));
        }
        let name = finding.name.as_str();
        for (pattern, reason) in self
            .path_rules
            .temporary
            .iter()
            .chain(&self.path_rules.documented)
        {
            if pattern.matches(name) {
                return Decision::Whitelist(reason.clone());
            }
        }
        if let Some(pattern) = self
            .path_rules
            .whitelist
            .iter()
            .find(|pattern| pattern.matches(name))
        {
            return Decision::Whitelist(format!("matches '{}'", pattern.text));
        }
        for (path, patterns) in &self.path_rules.overrides {
            if path.path_matches(&finding.file)
                && patterns.iter().any(|pattern| pattern.matches(name))
            {
                return Decision::Whitelist(format!("per-file: {}", path.text));
            }
        }
        if self
            .path_rules
            .lower
            .iter()
            .any(|pattern| pattern.matches(name))
        {
            return Decision::Lower(30);
        }
        Decision::Keep
    }

    pub(super) fn unchanged(&self, path: &Path, options: &Options) -> bool {
        self.for_path(path, options)
            .is_ok_and(|next| self.same_layers(&next))
    }

    pub(super) fn for_path(&self, path: &Path, options: &Options) -> Result<Self, ConfigError> {
        load_at(path, options, self.date)
    }

    pub(super) fn same_layers(&self, other: &Self) -> bool {
        other.sources == self.sources && other.digests == self.digests
    }
}

pub(super) enum Decision {
    Keep,
    Suppress(String),
    Whitelist(String),
    Lower(u8),
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Layer {
    root: bool,
    exclude: Option<Vec<String>>,
    ignore: Option<Vec<String>>,
    whitelist: Option<Whitelist>,
    overrides: Option<BTreeMap<String, Override>>,
    include_generated: Option<bool>,
    dead_code: Option<DeadCode>,
    nudges: Option<bool>,
    check_circular: Option<bool>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Whitelist {
    Names(Vec<String>),
    Detailed(WhitelistTables),
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct WhitelistTables {
    names: Vec<String>,
    documented: BTreeMap<String, String>,
    temporary: BTreeMap<String, Temporary>,
    lower_confidence: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Temporary {
    reason: Option<String>,
    expires: Option<StringOrDate>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum StringOrDate {
    String(String),
    Date(toml::value::Datetime),
}

impl StringOrDate {
    fn text(self) -> String {
        match self {
            Self::String(value) => value,
            Self::Date(value) => value.to_string(),
        }
    }
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Override {
    whitelist: Vec<String>,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct DeadCode {
    entrypoints: Option<Vec<Selector>>,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Selector {
    name: Values,
    full_name: Values,
    module: Values,
    path: Values,
    #[serde(rename = "type")]
    kind: Values,
    #[serde(alias = "decorators")]
    decorator: Values,
    #[serde(alias = "base_classes")]
    base_class: Values,
    #[serde(alias = "parent_class")]
    parent: Option<ParentSelector>,
    #[serde(rename = "reason")]
    _reason: Option<String>,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ParentSelector {
    name: Values,
    full_name: Values,
    module: Values,
    path: Values,
    #[serde(alias = "base_classes")]
    base_class: Values,
}

#[derive(Default, Deserialize)]
#[serde(untagged)]
enum Values {
    #[default]
    Absent,
    One(String),
    Many(Vec<String>),
}

impl Values {
    fn compile(self) -> Result<Vec<Pattern>, ConfigError> {
        match self {
            Self::Absent => Ok(Vec::new()),
            Self::One(value) => Ok(vec![Pattern::new(&value)?]),
            Self::Many(values) if values.is_empty() => Err(ConfigError::new(
                "entrypoint selector values must not be empty",
            )),
            Self::Many(values) => patterns(&values),
        }
    }
}

#[derive(Clone)]
pub(super) struct Entrypoint {
    fields: Vec<(&'static str, Vec<Pattern>)>,
    locations: Option<Arc<BTreeSet<String>>>,
}

impl Entrypoint {
    fn new(selector: Selector) -> Result<Self, ConfigError> {
        let mut selectors = vec![
            ("name", selector.name),
            ("full_name", selector.full_name),
            ("module", selector.module),
            ("path", selector.path),
            ("type", selector.kind),
            ("decorator", selector.decorator),
            ("base_class", selector.base_class),
        ];
        if let Some(parent) = selector.parent {
            selectors.extend([
                ("parent_name", parent.name),
                ("parent_full_name", parent.full_name),
                ("parent_module", parent.module),
                ("parent_path", parent.path),
                ("parent_base_class", parent.base_class),
            ]);
        }
        let mut fields = Vec::new();
        for (kind, values) in selectors {
            let values = values.compile()?;
            if !values.is_empty() {
                fields.push((kind, values));
            }
        }
        if !fields
            .iter()
            .any(|(kind, _)| !matches!(*kind, "type" | "path" | "module"))
        {
            return Err(ConfigError::new(
                "entrypoint selector requires a symbol or identity",
            ));
        }
        Ok(Self {
            fields,
            locations: None,
        })
    }

    pub(super) fn scoped(&self, locations: &Arc<BTreeSet<String>>) -> Self {
        let mut entrypoint = self.clone();
        entrypoint.locations = Some(Arc::clone(locations));
        entrypoint
    }

    pub(super) fn matches(
        &self,
        file: &str,
        module: &str,
        name: &str,
        full_name: &str,
        qualifiers: &[(String, String)],
    ) -> bool {
        if self
            .locations
            .as_ref()
            .is_some_and(|files| !files.contains(file))
        {
            return false;
        }
        self.fields.iter().all(|(kind, patterns)| {
            let matches = |value: &str| patterns.iter().any(|pattern| pattern.matches(value));
            match *kind {
                "path" => patterns.iter().any(|pattern| pattern.path_matches(file)),
                "module" => matches(module),
                "name" => matches(name),
                "full_name" => matches(full_name),
                other => qualifiers
                    .iter()
                    .any(|(key, value)| key == other && matches(value)),
            }
        })
    }
}

fn patterns(values: &[String]) -> Result<Vec<Pattern>, ConfigError> {
    values.iter().map(|value| Pattern::new(value)).collect()
}

fn selection_patterns(values: &[String], input: &Path) -> Result<Vec<Pattern>, ConfigError> {
    values
        .iter()
        .map(|value| Pattern::selection(value, input))
        .collect()
}

pub(super) fn load(path: &Path, options: &Options) -> Result<Config, ConfigError> {
    load_at(path, options, OffsetDateTime::now_utc().date())
}

fn load_at(path: &Path, options: &Options, date: Date) -> Result<Config, ConfigError> {
    let confidence = options.confidence.unwrap_or(60);
    if confidence > 100 {
        return Err(ConfigError::new("confidence must be between 0 and 100"));
    }
    if !(1..=32).contains(&options.workers) {
        return Err(ConfigError::new("workers must be between 1 and 32"));
    }
    let mut sources = Vec::new();
    let mut digests = Vec::new();
    let mut excludes = Vec::new();
    let mut include_generated = false;
    let mut selectors = Vec::new();
    let mut whitelist = WhitelistTables::default();
    let mut path_rules = PathRules::default();

    for (file, layer, digest) in layers(path, options)? {
        sources.push(file.to_string_lossy().replace('\\', "/"));
        digests.push(digest);
        if let Some(values) = layer.exclude {
            excludes = values;
        }
        if let Some(values) = layer.ignore {
            path_rules.ignores = values;
        }
        if let Some(values) = layer.whitelist {
            whitelist = match values {
                Whitelist::Names(names) => WhitelistTables {
                    names,
                    ..WhitelistTables::default()
                },
                Whitelist::Detailed(tables) => tables,
            };
        }
        if let Some(values) = layer.overrides {
            path_rules.overrides = values
                .into_iter()
                .map(|(path, entry)| Ok((Pattern::new(&path)?, patterns(&entry.whitelist)?)))
                .collect::<Result<_, ConfigError>>()?;
        }
        if let Some(value) = layer.include_generated {
            include_generated = value;
        }
        if let Some(values) = layer.dead_code.and_then(|code| code.entrypoints) {
            selectors = values
                .into_iter()
                .map(Entrypoint::new)
                .collect::<Result<_, _>>()?;
        }
    }
    excludes.extend(options.excludes.iter().cloned());
    path_rules.whitelist = patterns(&whitelist.names)?;
    path_rules.lower = patterns(&whitelist.lower_confidence)?;
    path_rules.documented = whitelist
        .documented
        .into_iter()
        .map(|(pattern, reason)| Ok((Pattern::new(&pattern)?, reason)))
        .collect::<Result<_, ConfigError>>()?;

    let mut warnings = Vec::new();
    for (pattern, temporary) in whitelist.temporary {
        let reason = temporary
            .reason
            .unwrap_or_else(|| "temporary whitelist".into());
        let expires = temporary.expires.map(StringOrDate::text);
        if let Some(expires) = expires.as_deref() {
            match parse_date(expires) {
                Some(expiry) if expiry >= date => {}
                Some(_) => {
                    warnings.push(format!("whitelist for {pattern:?} expired on {expires}"));
                    continue;
                }
                None => {
                    warnings.push(format!(
                        "whitelist for {pattern:?} has an invalid expiry {expires:?}"
                    ));
                    continue;
                }
            }
        }
        path_rules.temporary.push((
            Pattern::new(&pattern)?,
            format!(
                "{reason} (expires: {})",
                expires.as_deref().unwrap_or("None")
            ),
        ));
    }
    Ok(Config {
        entrypoints: selectors,
        confidence,
        date,
        sources,
        digests,
        warnings,
        path_rules,
        selection: Selection {
            include: selection_patterns(&options.includes, path)?,
            exclude: selection_patterns(&excludes, path)?,
            keep_generated: selection_patterns(&options.generated_keep_paths, path)?,
            defaults: !options.no_default_excludes,
            include_generated: options.include_generated.unwrap_or(include_generated),
        },
    })
}

fn parse_date(value: &str) -> Option<Date> {
    let [year, month, day]: [_; 3] = value.split('-').collect::<Vec<_>>().try_into().ok()?;
    if (year.len(), month.len(), day.len()) != (4, 2, 2) {
        return None;
    }
    Date::from_calendar_date(
        year.parse().ok()?,
        Month::try_from(month.parse::<u8>().ok()?).ok()?,
        day.parse().ok()?,
    )
    .ok()
}

type ConfigLayer = (PathBuf, Layer, blake3::Hash);

fn layers(path: &Path, options: &Options) -> Result<Vec<ConfigLayer>, ConfigError> {
    for variable in ["SKYLOS_POLICY_BASE", "SKYLOS_SYNCED_POLICY_FILE"] {
        if std::env::var_os(variable).is_some_and(|value| !value.is_empty()) {
            return Err(ConfigError::new(format!(
                "{variable} is outside the local dead-code profile"
            )));
        }
    }
    let env_file = std::env::var_os("SKYLOS_CONFIG_FILE");
    if let Some(file) = options
        .config_file
        .as_deref()
        .or_else(|| env_file.as_deref().map(Path::new))
        && !file.as_os_str().is_empty()
    {
        return read_layer(file).and_then(|layer| {
            layer
                .map(|(data, digest)| vec![(file.to_owned(), data, digest)])
                .ok_or_else(|| ConfigError::new(format!("no Skylos config in {}", file.display())))
        });
    }
    let cwd = std::env::current_dir().map_err(ConfigError::new)?;
    let absolute = cwd.join(path);
    let start = if absolute.is_file()
        || (!absolute.exists() && absolute.extension() == Some(OsStr::new("py")))
    {
        absolute.parent().unwrap_or(&absolute)
    } else {
        absolute.as_path()
    };
    let mut result = Vec::new();
    for (depth, directory) in start.ancestors().enumerate() {
        if depth >= MAX_CONFIG_LAYERS {
            return Err(ConfigError::new("configuration ancestor limit exceeded"));
        }
        if directory.join(".skylos/config.yaml").exists() {
            return Err(ConfigError::new(format!(
                "synced Skylos configuration in {} is outside the local dead-code profile",
                directory.display()
            )));
        }
        let mut stop = directory.join(".git").exists();
        for filename in [".skylos.toml", "skylos.toml", "pyproject.toml"] {
            let file = directory.join(filename);
            if !file.try_exists().map_err(ConfigError::new)? {
                continue;
            }
            if let Some((layer, digest)) = read_layer(&file)? {
                stop |= layer.root;
                result.push((file, layer, digest));
                break;
            }
        }
        if stop {
            break;
        }
    }
    result.reverse();
    Ok(result)
}

fn read_layer(file: &Path) -> Result<Option<(Layer, blake3::Hash)>, ConfigError> {
    use std::io::Read;
    let failure =
        |error: &dyn std::fmt::Display| ConfigError::new(format!("{}: {error}", file.display()));
    let input = fs::File::open(file).map_err(|error| failure(&error))?;
    if !input.metadata().map_err(|error| failure(&error))?.is_file() {
        return Err(ConfigError::new(format!(
            "{} is not a regular config file",
            file.display()
        )));
    }
    let mut bytes = Vec::new();
    input
        .take(MAX_CONFIG_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| failure(&error))?;
    if bytes.len() as u64 > MAX_CONFIG_BYTES {
        return Err(ConfigError::new(format!(
            "{} exceeds the configuration size limit",
            file.display()
        )));
    }
    let text = std::str::from_utf8(&bytes).map_err(|error| failure(&error))?;
    let mut table = toml::from_str::<toml::Value>(text).map_err(|error| failure(&error))?;
    let project = file.file_name() == Some(OsStr::new("pyproject.toml"));
    if let Some(value) = table.get("tool").and_then(|tool| tool.get("skylos")) {
        table = value.clone();
    } else if project {
        return Ok(None);
    }
    let layer: Layer = table.try_into().map_err(|error| failure(&error))?;
    if layer.nudges == Some(true) || layer.check_circular == Some(true) {
        return Err(ConfigError::new(format!(
            "{} enables analysis outside the Skylos dead-code profile",
            file.display()
        )));
    }
    Ok(Some((layer, blake3::hash(&bytes))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::phase02::Category;

    #[test]
    fn path_selection_honors_default_and_forced_includes() {
        let flags = Options {
            includes: vec!["vendor/keep.py".into()],
            excludes: vec!["nested/private*.py".into()],
            ..Options::default()
        };
        let root = tempfile::tempdir().unwrap();
        let config = load(root.path(), &flags).unwrap();
        assert_eq!(
            config.selection.exclusion("vendor"),
            Some("default_exclusion")
        );
        assert!(config.selection.may_include_children("vendor"));
        assert_eq!(config.selection.exclusion("vendor/keep.py"), None);
        assert_eq!(
            config.selection.exclusion("vendor/skip.py"),
            Some("default_exclusion")
        );
        assert_eq!(
            config.selection.exclusion("nested/private_01.py"),
            Some("configured_exclusion")
        );
        assert_eq!(config.selection.exclusion("nested/public.py"), None);
    }

    #[test]
    fn temporary_whitelists_expire_after_their_declared_date() {
        let project = tempfile::tempdir().unwrap();
        let path = project.path().join(".skylos.toml");
        fs::write(
            &path,
            r#"
[whitelist.temporary.active]
reason = "external consumer"
expires = 2026-01-02
[whitelist.temporary.expired]
reason = "old consumer"
expires = "2026-01-01"
[whitelist.temporary.invalid]
expires = "not a date"
"#,
        )
        .unwrap();
        let config = load_at(
            project.path(),
            &Options {
                config_file: Some(path),
                ..Options::default()
            },
            Date::from_calendar_date(2026, Month::January, 2).unwrap(),
        )
        .unwrap();
        let finding = |name: &str| Finding {
            category: Category::Function,
            name: name.into(),
            full_name: format!("main.{name}"),
            file: "main.py".into(),
            line: 1,
            column: 1,
            confidence: 90,
            rule_id: "SKY-U001",
            reason: "no_call_path_from_root",
        };
        assert!(
            matches!(config.decision(&finding("active")), Decision::Whitelist(reason) if reason.contains("external consumer"))
        );
        assert!(matches!(
            config.decision(&finding("expired")),
            Decision::Keep
        ));
        assert!(matches!(
            config.decision(&finding("invalid")),
            Decision::Keep
        ));
        assert_eq!(config.warnings.len(), 2);
    }
}
