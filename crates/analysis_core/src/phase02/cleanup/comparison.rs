use std::collections::BTreeMap;
use std::ops::Range;

use serde_json::{Value, json};

use super::Report;

pub(super) fn new_risks(
    before: &Report,
    after: &Report,
    edited_file: &str,
    original: &str,
    replacement: &str,
    removal: &Range<usize>,
    statement: &Range<usize>,
) -> Vec<Value> {
    compare(
        native_risks(before),
        native_risks(after),
        edited_file,
        original,
        replacement,
        removal,
        statement,
    )
}

struct Finding<'a> {
    family: &'static str,
    file: &'a str,
    scoped_identity: Option<&'a str>,
    row: Value,
}

impl<'a> Finding<'a> {
    fn risk(
        family: &'static str,
        file: &'a str,
        scoped_identity: Option<&'a str>,
        purpose: &str,
        severity: Option<&str>,
        row: impl FnOnce() -> Value,
    ) -> Option<Self> {
        let high = severity.is_some_and(|severity| {
            severity.eq_ignore_ascii_case("high") || severity.eq_ignore_ascii_case("critical")
        });
        (purpose == "defect" || (purpose == "review" && high)).then(|| Self {
            family,
            file,
            scoped_identity,
            row: row(),
        })
    }

    fn identity(
        &self,
        edited_file: &str,
        lines: &[(usize, &'a str)],
        removed: Option<&Range<usize>>,
        statement: &Range<usize>,
        statement_text: &'a str,
    ) -> Identity<'a> {
        let line = position(&self.row["line"]).or_else(|| position(&self.row["lineno"]));
        let column = position(&self.row["column"])
            .or_else(|| position(&self.row["col"]))
            .unwrap_or(1);
        let location = if self.file == edited_file {
            line.and_then(|line| lines.get(line - 1))
                .filter(|(_, text)| text.len() >= column - 1)
                .map(|(start, _)| {
                    let offset = start + column - 1;
                    let original_offset = removed
                        .filter(|range| offset >= range.start)
                        .map_or(offset, |range| offset + range.len());
                    let statement = statement.contains(&offset).then_some(statement_text);
                    Occurrence::Edited(original_offset, statement)
                })
        } else {
            None
        };
        Identity {
            family: self.family,
            file: self.file,
            scoped_identity: self.scoped_identity,
            location: location.unwrap_or(Occurrence::Reported(line, column)),
            details: json!([
                self.row["rule_id"],
                self.row["name"],
                self.row["simple_name"],
                self.row["message"],
                self.row["value"],
                self.row["reason"],
                evidence(&self.row["evidence"]),
                evidence(&self.row["dead_code_evidence"]),
            ])
            .to_string(),
        }
    }
}

fn native_risks(report: &Report) -> impl Iterator<Item = Finding<'_>> {
    // Native Phase 0.3 reports currently contain review checks. The pinned
    // cleanup contract also accepts defects regardless of their severity.
    report
        .categories
        .iter()
        .filter_map(|finding| {
            Finding::risk(
                "dead_code",
                &finding.file,
                Some(&finding.identity_name),
                "review",
                finding.severity,
                || serde_json::to_value(finding).expect("diagnostic serializes"),
            )
        })
        .chain(report.quality.iter().flatten().filter_map(|finding| {
            Finding::risk(
                "quality",
                finding.relative_file.as_deref().unwrap_or(&finding.file),
                None,
                "review",
                Some(finding.severity),
                || serde_json::to_value(finding).expect("quality finding serializes"),
            )
        }))
}

#[derive(PartialEq, Eq, PartialOrd, Ord)]
struct Identity<'a> {
    family: &'static str,
    file: &'a str,
    scoped_identity: Option<&'a str>,
    location: Occurrence<'a>,
    details: String,
}

#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum Occurrence<'a> {
    Edited(usize, Option<&'a str>),
    Reported(Option<usize>, usize),
}

fn compare<'a>(
    before: impl Iterator<Item = Finding<'a>>,
    after: impl Iterator<Item = Finding<'a>>,
    edited_file: &str,
    original: &'a str,
    replacement: &'a str,
    removal: &Range<usize>,
    statement: &Range<usize>,
) -> Vec<Value> {
    let mut before = before.peekable();
    let mut after = after.peekable();
    if after.peek().is_none() {
        return Vec::new();
    }
    if before.peek().is_none() {
        return after.map(|finding| finding.row).collect();
    }

    // The caller has already established that the project snapshots differ
    // only by this removal. Map surviving occurrences back to their original
    // byte locations so line shifts cannot hide or invent a risky finding.
    let before_lines = line_starts(original);
    let after_lines = line_starts(replacement);
    let remaining_offset =
        |offset: usize| offset - offset.saturating_sub(removal.start).min(removal.len());
    let after_statement = remaining_offset(statement.start)..remaining_offset(statement.end);
    let before_text = &original[statement.clone()];
    let after_text = &replacement[after_statement.clone()];
    let mut remaining = BTreeMap::new();
    for finding in before {
        let identity = finding.identity(edited_file, &before_lines, None, statement, before_text);
        *remaining.entry(identity).or_insert(0_usize) += 1;
    }
    let mut introduced = Vec::new();
    for finding in after {
        let identity = finding.identity(
            edited_file,
            &after_lines,
            Some(removal),
            &after_statement,
            after_text,
        );
        if let Some(count) = remaining.get_mut(&identity).filter(|count| **count > 0) {
            *count -= 1;
        } else {
            introduced.push(finding.row);
        }
    }
    introduced
}

fn line_starts(source: &str) -> Vec<(usize, &str)> {
    let mut offset = 0;
    source
        .split_inclusive('\n')
        .map(|line| {
            let start = offset;
            offset += line.len();
            (start, line)
        })
        .collect()
}

fn position(value: &Value) -> Option<usize> {
    usize::try_from(value.as_u64()?).ok().filter(|&n| n > 0)
}

fn evidence(value: &Value) -> Value {
    match value {
        Value::Object(fields) => Value::Object(
            fields
                .iter()
                .filter(|(key, _)| {
                    !matches!(
                        key.as_str(),
                        "line" | "lineno" | "col" | "column" | "start_line" | "end_line"
                    )
                })
                .map(|(key, value)| (key.clone(), evidence(value)))
                .collect(),
        ),
        Value::Array(values) => Value::Array(values.iter().map(evidence).collect()),
        _ => value.clone(),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::ops::Range;

    use serde_json::{Value, json};

    use super::{Finding, compare, new_risks};
    use crate::phase02::{Options, Profile, scan};

    const SOURCE: &str = "import math\r\ncheck()\r\ncheck()\r\n";
    const REMOVAL: Range<usize> = 0.."import math\r\n".len();
    const STATEMENT: Range<usize> = 0.."import math".len();

    fn row(
        rule: &'static str,
        purpose: &str,
        severity: &str,
        line: usize,
        detail: &str,
    ) -> Option<Finding<'static>> {
        let family = if rule == "SKY-L012" {
            "ai_defects"
        } else {
            "quality"
        };
        Finding::risk(family, "app.py", None, purpose, Some(severity), || {
            json!({
                "rule_id": rule,
                "file": "app.py",
                "line": line,
                "column": 1,
                "severity": severity,
                "name": "check",
                "evidence": {"events": [{"start_line": line, "column": 1, "detail": detail}]},
            })
        })
    }

    fn risks(
        before: impl IntoIterator<Item = Option<Finding<'static>>>,
        after: impl IntoIterator<Item = Option<Finding<'static>>>,
    ) -> Vec<Value> {
        compare(
            before.into_iter().flatten(),
            after.into_iter().flatten(),
            "app.py",
            SOURCE,
            &SOURCE[REMOVAL.end..],
            &REMOVAL,
            &STATEMENT,
        )
    }

    #[test]
    fn existing_risk_matches_across_location_only_changes() {
        let before = row("SKY-L012", "review", "HIGH", 3, "unknown name");
        let after = row("SKY-L012", "review", "HIGH", 2, "unknown name");
        assert!(risks([before], [after]).is_empty());
    }

    #[test]
    fn changed_conclusion_and_new_occurrence_are_reported_even_with_equal_counts() {
        for (line, detail) in [(1, "unknown name"), (2, "missing provider")] {
            let before = row("SKY-L012", "review", "CRITICAL", 3, "unknown name");
            let after = row("SKY-L012", "review", "CRITICAL", line, detail);
            let expected = after.as_ref().unwrap().row.clone();
            assert_eq!(risks([before], [after]), [expected]);
        }
    }

    #[test]
    fn same_location_requires_matching_source_content() {
        let original = "import math, json\n";
        let replacement = "import math\n";
        let before = row("SKY-L012", "review", "HIGH", 1, "unknown name");
        let after = row("SKY-L012", "review", "HIGH", 1, "unknown name");
        let expected = after.as_ref().unwrap().row.clone();
        assert_eq!(
            compare(
                before.into_iter(),
                after.into_iter(),
                "app.py",
                original,
                replacement,
                &(11..17),
                &(0..17),
            ),
            [expected]
        );
    }

    #[test]
    fn separate_same_line_statement_keeps_its_identity() {
        let original = "import math, json; check()\n";
        let replacement = "import math; check()\n";
        let mut before = row("SKY-L012", "review", "HIGH", 1, "unknown name").unwrap();
        before.row["column"] = json!(20);
        let mut after = row("SKY-L012", "review", "HIGH", 1, "unknown name").unwrap();
        after.row["column"] = json!(14);
        assert!(
            compare(
                [before].into_iter(),
                [after].into_iter(),
                "app.py",
                original,
                replacement,
                &(11..17),
                &(0..17),
            )
            .is_empty()
        );
    }

    #[test]
    fn risk_policy_preserves_multiplicity_and_excludes_advice() {
        let defect = |line| row("SKY-Q401", "defect", "LOW", line, "blocking call");
        let expected = defect(2).unwrap().row;
        let advice = row("SKY-Q301", "advice", "HIGH", 1, "complexity");
        let review = row("SKY-L012", "review", "MEDIUM", 1, "unknown name");
        assert_eq!(
            risks([defect(3)], [defect(2), defect(2), advice, review]),
            [expected]
        );
    }

    #[cfg(unix)]
    #[test]
    fn introduced_risk_rolls_back_after_a_real_rescan_but_shifted_risk_does_not() {
        use super::super::{
            Rescan, Scan, compare_rescan, finish_execution,
            tests::{apply_and_check, project},
        };

        for already_present in [true, false] {
            let (project, options, before, candidate) = project();
            let root = project.path();
            let (edit, tests) = apply_and_check(root, &candidate);
            let rescan =
                compare_rescan(root, &before, &candidate, &edit, Scan::read(root, &options));
            assert_eq!(rescan.status, "passed", "{:?}", rescan.error);

            // The native scanner has no SKY-L012 rule. Supply the pinned
            // review observation at the comparison boundary after a live scan.
            let baseline = already_present
                .then(|| row("SKY-L012", "review", "CRITICAL", 2, "unknown name"))
                .flatten();
            let observation = row("SKY-L012", "review", "CRITICAL", 1, "unknown name").unwrap();
            let expected = observation.row.clone();
            let findings = compare(
                baseline.into_iter(),
                [observation].into_iter(),
                &candidate.task.file,
                &edit.source,
                &edit.replacement,
                &edit.plan.removal,
                &edit.plan.statement,
            );
            let rescan = Rescan::compared(rescan.original_present.unwrap(), findings);
            let result = finish_execution(root, &candidate, &edit, tests, rescan);
            assert_eq!(result.tests.status, "passed");
            assert_eq!(result.rolled_back, !already_present);
            assert_eq!(result.applied, already_present);
            assert_eq!(result.rescan.original_present, Some(false));
            let actual = fs::read(root.join(&candidate.task.file)).unwrap();
            if already_present {
                assert_eq!(result.status, "verified");
                assert_eq!(result.rescan.status, "passed");
                assert!(result.rescan.new_findings.is_empty());
                assert_eq!(actual, edit.replacement.as_bytes());
            } else {
                assert_eq!(result.status, "failed");
                assert_eq!(result.rescan.status, "failed");
                assert_eq!(result.rescan.new_findings, [expected]);
                assert_eq!(actual, edit.source.as_bytes());
            }
        }
    }

    #[test]
    fn new_native_dependency_and_empty_file_reviews_are_below_the_risk_threshold() {
        let project = tempfile::tempdir().unwrap();
        let path = project.path().join("app.py");
        fs::write(project.path().join("requirements.txt"), "requests\n").unwrap();
        let original = "import requests\n";
        fs::write(&path, original).unwrap();
        let options = Options {
            profile: Profile::SkylosPhase03,
            enable_quality: Some(true),
            ..Options::default()
        };
        let before = scan(project.path(), &options).unwrap();
        fs::write(&path, "").unwrap();
        let after = scan(project.path(), &options).unwrap();
        assert!(before.quality.as_ref().unwrap().is_empty());
        assert_eq!(after.quality.as_ref().unwrap()[0].name, "requests");
        assert_eq!(after.categories.unused_files[0].rule_id, "SKY-E002");
        assert!(
            new_risks(
                &before,
                &after,
                "app.py",
                original,
                "",
                &(0..original.len()),
                &(0..original.trim_end().len()),
            )
            .is_empty()
        );
    }
}
