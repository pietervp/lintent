//! Keep marks: the named, reasoned escape hatch.
//!
//! ```text
//! // lintent-keep no-db-in-routes, other-rule -- health check must hit the DB
//! ```
//!
//! A mark binds to exactly one scope: the one starting on the mark's own
//! line, else the first scope below it with only blank and comment lines in
//! between (decorators are part of the scope). It suppresses those rules for
//! that scope only — a mark on a class does not cover its methods, and it
//! never leaks onto the next function. Marks are read from the parse tree's
//! comment nodes, so they work in any language's comment syntax.
//!
//! A mark without a reason, or naming a rule that does not exist, is itself
//! an error: an unexplained or stale suppression is exactly the kind of debt
//! the escape hatch must not hide.

use std::collections::{BTreeMap, BTreeSet};

use crate::extract::{CommentLine, Unit};

pub const KEEP_TOKEN: &str = "lintent-keep";
/// The rule id keep problems are reported under.
pub const KEEP_RULE_ID: &str = "lintent/keep";
#[derive(Debug, Clone, PartialEq)]
pub struct KeepMark {
    pub line: usize,
    pub rules: Vec<String>,
    pub reason: String,
    /// Index of the unit the mark applies to, once [`Keeps::bind`] ran.
    pub unit: Option<usize>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct KeepProblem {
    pub line: usize,
    pub message: String,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Keeps {
    pub marks: Vec<KeepMark>,
    pub problems: Vec<KeepProblem>,
}

impl Keeps {
    /// Parses every mark in `comments`. `known` decides which rule ids
    /// exist; unknown ids become problems and do not suppress anything.
    pub fn parse(comments: &[CommentLine], known: impl Fn(&str) -> bool) -> Keeps {
        let mut keeps = Keeps::default();
        for comment in comments {
            match parse_mark(&comment.text) {
                None => {}
                Some(Err(message)) => keeps.problems.push(KeepProblem {
                    line: comment.line,
                    message,
                }),
                Some(Ok((rules, reason))) => {
                    let (known_rules, unknown): (Vec<String>, Vec<String>) =
                        rules.into_iter().partition(|rule| known(rule));
                    for rule in unknown {
                        keeps.problems.push(KeepProblem {
                            line: comment.line,
                            message: format!("`{KEEP_TOKEN}` names unknown rule `{rule}`"),
                        });
                    }
                    if !known_rules.is_empty() {
                        keeps.marks.push(KeepMark {
                            line: comment.line,
                            rules: known_rules,
                            reason,
                            unit: None,
                        });
                    }
                }
            }
        }
        keeps
    }

    /// Binds every mark to its scope. `units` must be sorted by start line;
    /// `filler` holds the blank and comment-only lines of the file.
    pub fn bind(&mut self, units: &[Unit], filler: &BTreeSet<usize>) {
        for mark in &mut self.marks {
            mark.unit = units
                .iter()
                .position(|unit| unit.start_line == mark.line)
                .or_else(|| {
                    let next = units.iter().position(|unit| unit.start_line > mark.line)?;
                    let gap_is_filler =
                        (mark.line + 1..units[next].start_line).all(|line| filler.contains(&line));
                    gap_is_filler.then_some(next)
                });
        }
    }

    /// Rules suppressed for unit `unit`, with their reasons.
    pub fn suppressed(&self, unit: usize) -> BTreeMap<&str, &str> {
        self.marks
            .iter()
            .filter(|mark| mark.unit == Some(unit))
            .flat_map(|mark| {
                mark.rules
                    .iter()
                    .map(|rule| (rule.as_str(), mark.reason.as_str()))
            })
            .collect()
    }
}

/// `None` when the line has no mark; `Err` for a malformed one.
fn parse_mark(line: &str) -> Option<Result<(Vec<String>, String), String>> {
    let start = find_token(line)?;
    let mut rest = &line[start + KEEP_TOKEN.len()..];
    for closer in ["*/", "-->", "*)", "-}"] {
        rest = rest.trim_end().strip_suffix(closer).unwrap_or(rest);
    }
    let (ids, reason) = match rest.split_once("--") {
        Some((ids, reason)) => (ids, reason.trim()),
        None => (rest, ""),
    };
    let rules: Vec<String> = ids
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter(|id| !id.is_empty())
        .map(str::to_string)
        .collect();
    if rules.is_empty() {
        return Some(Err(format!(
            "`{KEEP_TOKEN}` names no rule; write `{KEEP_TOKEN} <rule-id> -- <reason>`"
        )));
    }
    if reason.is_empty() {
        return Some(Err(format!(
            "`{KEEP_TOKEN} {}` has no reason; write `{KEEP_TOKEN} {} -- <why this is fine>`",
            rules.join(", "),
            rules.join(", ")
        )));
    }
    Some(Ok((rules, reason.to_string())))
}

/// The token as a whole word (not `lintent-keeper`, not `x-lintent-keep`).
fn find_token(line: &str) -> Option<usize> {
    let mut offset = 0;
    while let Some(found) = line[offset..].find(KEEP_TOKEN) {
        let start = offset + found;
        let end = start + KEEP_TOKEN.len();
        let before_ok = line[..start]
            .chars()
            .next_back()
            .is_none_or(|c| !(c.is_alphanumeric() || c == '-' || c == '_'));
        let after_ok = line[end..]
            .chars()
            .next()
            .is_none_or(|c| c.is_whitespace() || c == ',');
        if before_ok && after_ok {
            return Some(start);
        }
        offset = end;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn comments(lines: &[(usize, &str)]) -> Vec<CommentLine> {
        lines
            .iter()
            .map(|(line, text)| CommentLine {
                line: *line,
                text: text.to_string(),
            })
            .collect()
    }

    fn known(rule: &str) -> bool {
        ["a", "b", "c"].contains(&rule)
    }

    #[test]
    fn parses_marks_in_any_comment_syntax() {
        let keeps = Keeps::parse(
            &comments(&[
                (1, "// lintent-keep a -- legacy endpoint"),
                (2, "# lintent-keep b, c -- generated"),
                (3, "/* lintent-keep a -- block comment */"),
                (4, "-- lintent-keep c -- sql style"),
                (5, "// unrelated comment"),
                (6, "// lintent-keeper a -- not a mark"),
            ]),
            known,
        );
        assert!(keeps.problems.is_empty(), "{:?}", keeps.problems);
        let marks: Vec<_> = keeps
            .marks
            .iter()
            .map(|m| (m.line, m.rules.join("+"), m.reason.as_str()))
            .collect();
        assert_eq!(
            marks,
            vec![
                (1, "a".to_string(), "legacy endpoint"),
                (2, "b+c".to_string(), "generated"),
                (3, "a".to_string(), "block comment"),
                (4, "c".to_string(), "sql style"),
            ]
        );
    }

    #[test]
    fn malformed_marks_are_problems() {
        let keeps = Keeps::parse(
            &comments(&[
                (1, "// lintent-keep a"),
                (2, "// lintent-keep a --   "),
                (3, "// lintent-keep -- reason only"),
                (4, "// lintent-keep a, nope -- partly unknown"),
            ]),
            known,
        );
        let problems: Vec<_> = keeps.problems.iter().map(|p| p.line).collect();
        assert_eq!(problems, vec![1, 2, 3, 4]);
        assert!(keeps.problems[0].message.contains("has no reason"));
        assert!(keeps.problems[2].message.contains("names no rule"));
        assert!(keeps.problems[3].message.contains("unknown rule `nope`"));
        // The known half of a partly-unknown mark still applies.
        assert_eq!(keeps.marks.len(), 1);
        assert_eq!(keeps.marks[0].rules, vec!["a"]);
    }

    fn unit(start: usize, end: usize) -> Unit {
        Unit {
            kind: crate::extract::ScopeKind::Function,
            name: format!("f{start}"),
            language: "typescript".into(),
            path: "a.ts".into(),
            start_line: start,
            end_line: end,
            source: String::new(),
            parent_source: None,
            truncated: false,
        }
    }

    #[test]
    fn a_mark_binds_to_exactly_one_scope() {
        // 1: mark → binds to the unit on 4 across a comment (2) and blank (3)
        // 6: mark on a unit's own first line
        // 9: mark followed by code (10) before the next unit: binds nowhere
        let mut keeps = Keeps::parse(
            &comments(&[
                (1, "// lintent-keep a -- first"),
                (6, "// lintent-keep b -- trailing"),
                (9, "// lintent-keep c -- stale"),
            ]),
            known,
        );
        let units = vec![unit(4, 5), unit(6, 7), unit(8, 8), unit(11, 12)];
        let filler: BTreeSet<usize> = [1, 2, 3, 9].into_iter().collect();
        keeps.bind(&units, &filler);
        assert_eq!(keeps.suppressed(0).get("a"), Some(&"first"));
        assert!(keeps.suppressed(1).contains_key("b"));
        assert!(
            !keeps.suppressed(2).contains_key("a"),
            "never leaks onto the next scope"
        );
        assert!(keeps.suppressed(3).is_empty());
        assert_eq!(keeps.marks[2].unit, None);
    }
}
