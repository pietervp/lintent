//! Findings, errors and stats of a `check` run, and how they are printed.

use std::fmt::Write as _;

use serde::Serialize;

use crate::budget::{summary_line, Estimate};
use crate::extract::ScopeKind;
use crate::rules::Severity;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    /// A fail at or above the rule's confidence threshold.
    Fail,
    /// A fail below the threshold: shown, never blocking.
    Uncertain,
}

#[derive(Debug, Clone, Serialize)]
pub struct Finding {
    pub path: String,
    pub line: usize,
    pub end_line: usize,
    /// `None` for keep-mark problems, which belong to a comment, not a scope.
    pub kind: Option<ScopeKind>,
    pub name: Option<String>,
    pub rule: String,
    pub severity: Severity,
    pub status: Status,
    pub confidence: Option<f64>,
    /// `probabilities.fail`, when the model reported a distribution.
    pub p_fail: Option<f64>,
    pub why: Option<String>,
    pub fix: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

impl Finding {
    pub fn blocks(&self) -> bool {
        self.status == Status::Fail && self.severity == Severity::Error
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ErrorEntry {
    pub path: Option<String>,
    pub line: Option<usize>,
    pub rule: Option<String>,
    pub message: String,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Stats {
    pub units: usize,
    pub questions: usize,
    pub cache_hits: usize,
    pub requests: usize,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost: f64,
    /// Scopes cut to the size limit before sending.
    pub truncated: usize,
    /// Files skipped because they are not UTF-8 text.
    pub skipped_files: usize,
}

#[derive(Debug, Default, Serialize)]
pub struct Report {
    pub findings: Vec<Finding>,
    pub errors: Vec<ErrorEntry>,
    pub stats: Stats,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub budget: Option<Estimate>,
}

impl Report {
    /// Precedence, highest first:
    /// 3 over budget (nothing was sent, so there is no verdict at all);
    /// 2 the run is incomplete (any error) — a partial result must not read
    /// as clean; 1 an error-severity rule confidently failed or a keep mark
    /// is invalid; 0 clean. `--dry-run` uses the same codes.
    pub fn exit_code(&self) -> i32 {
        if self
            .budget
            .as_ref()
            .is_some_and(|budget| budget.over_budget)
        {
            3
        } else if !self.errors.is_empty() {
            2
        } else if self.findings.iter().any(Finding::blocks) {
            1
        } else {
            0
        }
    }

    pub fn sort(&mut self) {
        self.findings
            .sort_by(|a, b| (&a.path, a.line, &a.rule).cmp(&(&b.path, b.line, &b.rule)));
        self.errors
            .sort_by(|a, b| (&a.path, a.line, &a.rule).cmp(&(&b.path, b.line, &b.rule)));
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).expect("reports always serialize")
    }

    /// Findings and errors: the part of the output meant for stdout.
    pub fn to_human(&self) -> String {
        let mut out = String::new();
        let confident: Vec<&Finding> = self
            .findings
            .iter()
            .filter(|f| f.status == Status::Fail)
            .collect();
        let doubtful: Vec<&Finding> = self
            .findings
            .iter()
            .filter(|f| f.status != Status::Fail)
            .collect();
        for finding in &confident {
            write_finding(&mut out, finding);
        }
        if !doubtful.is_empty() {
            if !confident.is_empty() {
                out.push('\n');
            }
            out.push_str("Uncertain (below min_confidence; not blocking):\n");
            for finding in &doubtful {
                write_finding(&mut out, finding);
            }
        }
        if !self.errors.is_empty() {
            out.push_str("\nErrors (the run is incomplete):\n");
            for error in &self.errors {
                let location = match (&error.path, error.line) {
                    (Some(path), Some(line)) => format!("{path}:{line}  "),
                    (Some(path), None) => format!("{path}  "),
                    _ => String::new(),
                };
                let rule = error
                    .rule
                    .as_deref()
                    .map(|r| format!("{r}  "))
                    .unwrap_or_default();
                let _ = writeln!(out, "{location}{rule}{}", error.message);
            }
        }
        out
    }

    /// Counts, cost and the budget estimate: meant for stderr, so stdout
    /// stays pipe-clean.
    pub fn summary(&self) -> String {
        let mut out = String::new();
        let confident = |severity: Severity| {
            self.findings
                .iter()
                .filter(|f| f.status == Status::Fail && f.severity == severity)
                .count()
        };
        let doubtful = self
            .findings
            .iter()
            .filter(|f| f.status != Status::Fail)
            .count();
        let stats = &self.stats;
        let _ = writeln!(
            out,
            "{} error(s), {} warning(s), {} uncertain · {} scope(s), {} question(s) ({} cached), {} request(s) · cost {} ({} in / {} out tokens)",
            confident(Severity::Error),
            confident(Severity::Warning),
            doubtful,
            stats.units,
            stats.questions,
            stats.cache_hits,
            stats.requests,
            crate::budget::usd(stats.cost),
            stats.input_tokens,
            stats.output_tokens,
        );
        if let Some(budget) = &self.budget {
            let _ = writeln!(out, "{}", summary_line(budget));
        }
        if stats.truncated > 0 {
            let _ = writeln!(
                out,
                "note: {} scope(s) were longer than the size limit; only their head was sent",
                stats.truncated
            );
        }
        out
    }
}

fn write_finding(out: &mut String, finding: &Finding) {
    let scope = match (&finding.kind, &finding.name) {
        (Some(kind), Some(name)) => format!("{kind} {name}"),
        _ => "keep mark".to_string(),
    };
    let confidence = finding
        .confidence
        .map(|c| format!("  (confidence {c:.2})"))
        .unwrap_or_default();
    let _ = writeln!(
        out,
        "{}:{}  {}  {}  {}{}",
        finding.path, finding.line, finding.severity, finding.rule, scope, confidence
    );
    if let Some(message) = &finding.message {
        let _ = writeln!(out, "    {message}");
    }
    if let Some(why) = &finding.why {
        let _ = writeln!(out, "    why: {}", why.trim());
    }
    if let Some(fix) = &finding.fix {
        let _ = writeln!(out, "    fix: {}", fix.trim());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn finding(status: Status, severity: Severity) -> Finding {
        Finding {
            path: "src/a.ts".into(),
            line: 3,
            end_line: 5,
            kind: Some(ScopeKind::Method),
            name: Some("Foo.bar".into()),
            rule: "demo".into(),
            severity,
            status,
            confidence: Some(0.93),
            p_fail: None,
            why: Some("ADR-1".into()),
            fix: Some("Do X.".into()),
            message: None,
        }
    }

    #[test]
    fn exit_codes() {
        let mut report = Report::default();
        assert_eq!(report.exit_code(), 0);
        report
            .findings
            .push(finding(Status::Uncertain, Severity::Error));
        report
            .findings
            .push(finding(Status::Fail, Severity::Warning));
        assert_eq!(report.exit_code(), 0);
        report.findings.push(finding(Status::Fail, Severity::Error));
        assert_eq!(report.exit_code(), 1);
        report.errors.push(ErrorEntry {
            path: None,
            line: None,
            rule: None,
            message: "boom".into(),
        });
        assert_eq!(report.exit_code(), 2);
        let mut budget =
            crate::budget::estimate(&[], &[], &[], &crate::config::Budget::default(), 0.0001);
        report.budget = Some(budget.clone());
        assert_eq!(report.exit_code(), 2, "within budget changes nothing");
        budget.over_budget = true;
        report.budget = Some(budget);
        assert_eq!(report.exit_code(), 3, "over budget wins");
    }

    #[test]
    fn human_output_lists_why_fix_and_uncertain_separately() {
        let mut report = Report::default();
        report.findings.push(finding(Status::Fail, Severity::Error));
        report
            .findings
            .push(finding(Status::Uncertain, Severity::Error));
        report.stats.cost = 0.000123;
        report.stats.requests = 1;
        let text = report.to_human();
        assert!(text.starts_with("src/a.ts:3  error  demo  method Foo.bar  (confidence 0.93)\n    why: ADR-1\n    fix: Do X.\n"));
        assert!(text.contains("Uncertain (below min_confidence"));
        let summary = report.summary();
        assert!(summary.contains("1 error(s), 0 warning(s), 1 uncertain"));
        assert!(summary.contains("cost $0.000123"));
    }

    #[test]
    fn json_shape() {
        let mut report = Report::default();
        report.findings.push(finding(Status::Fail, Severity::Error));
        let value: serde_json::Value = serde_json::from_str(&report.to_json()).unwrap();
        let first = &value["findings"][0];
        for key in [
            "path",
            "line",
            "end_line",
            "kind",
            "name",
            "rule",
            "severity",
            "status",
            "confidence",
            "p_fail",
            "why",
            "fix",
        ] {
            assert!(first.get(key).is_some(), "missing {key}");
        }
        for key in ["units", "questions", "cache_hits", "requests", "cost"] {
            assert!(value["stats"].get(key).is_some(), "missing stats.{key}");
        }
        assert!(value["errors"].as_array().unwrap().is_empty());
    }
}
