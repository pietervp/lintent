//! `lintent eval`: prove each rule against its fixtures.
//!
//! `.lintent/fixtures/<rule>/pass/*` must produce no confident fail;
//! `.lintent/fixtures/<rule>/fail/*` must produce at least one. A rule with
//! no fail fixture is "unproven": nothing shows it can ever fire, which is
//! how a vaguely worded rule slips through. Fixtures ignore the rule's
//! include globs (they live outside the source tree) but still respect its
//! scopes and languages.

use std::fmt::Write as _;
use std::path::Path;

use anyhow::Result;
use ignore::WalkBuilder;

use crate::budget;
use crate::cache::Cache;
use crate::check::{client, print_dry_run, session_id};
use crate::engine::{self, Outcome, Question, Settings};
use crate::extract::Unit;
use crate::jev::Choice;
use crate::report::Report;
use crate::rules::Rule;
use crate::workspace::{SourceFile, Workspace};

#[derive(Debug, Clone, Default)]
pub struct EvalOptions {
    pub rules: Vec<String>,
    pub dry_run: bool,
    pub no_cache: bool,
    /// Overrides `[budget] max_cost_usd`.
    pub max_cost: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Expect {
    Pass,
    Fail,
}

struct Fixture {
    rule: usize,
    expect: Expect,
    path: String,
    /// Indices into the pooled unit list.
    units: Vec<usize>,
    error: Option<String>,
}

pub fn run(workspace: &Workspace, options: &EvalOptions) -> Result<i32> {
    let config = &workspace.project.config;
    let rules = workspace.select_rules(&options.rules)?;
    let mut units: Vec<Unit> = Vec::new();
    let mut questions = Vec::new();
    let mut fixtures = Vec::new();

    for (rule_index, rule) in rules.iter().enumerate() {
        for expect in [Expect::Pass, Expect::Fail] {
            let dir = workspace.project.fixtures_dir(&rule.id).join(match expect {
                Expect::Pass => "pass",
                Expect::Fail => "fail",
            });
            for file in fixture_files(workspace, &dir) {
                let mut fixture = Fixture {
                    rule: rule_index,
                    expect,
                    path: file.path.clone(),
                    units: Vec::new(),
                    error: None,
                };
                if !rule.applies_to_language(&file.language) {
                    fixture.error = Some(format!("the rule does not apply to {}", file.language));
                } else {
                    match workspace.parse(&file) {
                        Err(error) => fixture.error = Some(format!("{error:#}")),
                        Ok(None) => fixture.error = Some("not UTF-8 text".to_string()),
                        Ok(Some((parsed, _keeps))) => {
                            for unit in parsed
                                .units
                                .into_iter()
                                .filter(|unit| rule.applies_to(unit))
                            {
                                questions.push(Question {
                                    unit: units.len(),
                                    rule: rule_index,
                                });
                                fixture.units.push(units.len());
                                units.push(unit);
                            }
                        }
                    }
                }
                fixtures.push(fixture);
            }
        }
    }

    let mut cache = if options.no_cache {
        Cache::disabled()
    } else {
        Cache::load(&workspace.project.cache_path())
    };
    let session = session_id();
    let cache_scope = config.cache_scope();
    let settings = Settings {
        model: &config.model,
        cache_scope: &cache_scope,
        session_id: &session,
        isolate_rules: config.isolate_rules,
        read_cache: !options.no_cache,
    };
    let mut plan = engine::plan(&settings, &units, &rules, &questions, &cache);

    let max_cost = options.max_cost.unwrap_or(config.budget.max_cost_usd);
    let estimate = budget::estimate(&plan.jobs, &units, &rules, &config.budget, max_cost);
    let over_budget = estimate.over_budget;
    if options.dry_run {
        let mut report = Report::default();
        report.stats.units = units.len();
        report.stats.questions = plan.questions;
        report.stats.cache_hits = plan.cache_hits;
        report.budget = Some(estimate.clone());
        print_dry_run(&config.endpoint(), &plan.jobs, &units, &report, false);
    }
    if over_budget {
        eprint!("{}", budget::guidance(&estimate, true));
        return Ok(3);
    }
    if options.dry_run {
        return Ok(static_problems(&rules, &fixtures));
    }

    let mut fatal = None;
    let mut actual = None;
    if !plan.jobs.is_empty() {
        let client = client(workspace)?;
        let execution = engine::execute(
            &mut plan,
            &units,
            &rules,
            &client,
            config.concurrency,
            &cache_scope,
            &mut cache,
        );
        fatal = execution.fatal;
        actual = Some(execution.usage.cost);
        if let Err(error) = cache.save() {
            eprintln!("lintent: warning: could not save the cache: {error:#}");
        }
    }

    let (table, summary, verdict) = judge(&rules, &fixtures, &plan.outcomes, config.min_confidence);
    print!("{table}");
    eprint!("{summary}");
    let estimate = budget::Estimate {
        actual_cost_usd: actual,
        ..estimate
    };
    eprintln!("{}", budget::summary_line(&estimate));
    if let Some(fatal) = fatal {
        eprintln!("lintent: stopped: {fatal}");
        return Ok(2);
    }
    Ok(verdict)
}

/// What a dry run can already tell without answers: fixtures that cannot be
/// evaluated or prove nothing, and rules with no fail fixture. Same codes as
/// a real run (2 errors, 1 vacuous/unproven, 0 otherwise).
fn static_problems(rules: &[Rule], fixtures: &[Fixture]) -> i32 {
    let mut code = 0;
    for fixture in fixtures {
        if let Some(error) = &fixture.error {
            eprintln!("error: {}: {error}", fixture.path);
            code = 2;
        } else if fixture.units.is_empty() {
            eprintln!(
                "vacuous: {} has no scope rule `{}` applies to",
                fixture.path, rules[fixture.rule].id
            );
            code = code.max(1);
        }
    }
    for (index, rule) in rules.iter().enumerate() {
        if !fixtures
            .iter()
            .any(|f| f.rule == index && f.expect == Expect::Fail)
        {
            eprintln!(
                "unproven: rule `{}` has no fixture under .lintent/fixtures/{}/fail/",
                rule.id, rule.id
            );
            code = code.max(1);
        }
    }
    code
}

fn fixture_files(workspace: &Workspace, dir: &Path) -> Vec<SourceFile> {
    if !dir.is_dir() {
        return Vec::new();
    }
    let mut files: Vec<SourceFile> = WalkBuilder::new(dir)
        .require_git(false)
        .hidden(true)
        .build()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_some_and(|kind| kind.is_file()))
        .filter_map(|entry| {
            let spec = workspace.registry.for_path(entry.path())?;
            Some(SourceFile {
                path: workspace.relative(entry.path())?,
                absolute: entry.path().to_path_buf(),
                language: spec.name.clone(),
            })
        })
        .collect();
    files.sort_by(|a, b| a.path.cmp(&b.path));
    files
}

/// Builds the result table and summary; the code is 2 on errors, 1 on a
/// misjudged or vacuous fixture or an unproven rule, else 0.
fn judge(
    rules: &[Rule],
    fixtures: &[Fixture],
    outcomes: &std::collections::HashMap<Question, Outcome>,
    default_threshold: f64,
) -> (String, String, i32) {
    let mut out = String::new();
    let mut misjudged = 0;
    let mut errors = 0;
    let _ = writeln!(
        out,
        "{:<28} {:<6} {:<10} {:<22} FIXTURE",
        "RULE", "EXPECT", "RESULT", "GOT"
    );
    for (rule_index, rule) in rules.iter().enumerate() {
        let threshold = rule.threshold(default_threshold);
        let mine: Vec<&Fixture> = fixtures.iter().filter(|f| f.rule == rule_index).collect();
        for fixture in &mine {
            let (got, result) = verdict(fixture, rule_index, outcomes, threshold);
            match result {
                "ok" => {}
                "error" => errors += 1,
                _ => misjudged += 1,
            }
            let expect = match fixture.expect {
                Expect::Pass => "pass",
                Expect::Fail => "fail",
            };
            let _ = writeln!(
                out,
                "{:<28} {:<6} {:<10} {:<22} {}",
                rule.id, expect, result, got, fixture.path
            );
        }
        if !mine.iter().any(|f| f.expect == Expect::Fail) {
            misjudged += 1;
            let _ = writeln!(
                out,
                "{:<28} {:<6} {:<10} {:<22} add a fixture under .lintent/fixtures/{}/fail/",
                rule.id, "fail", "unproven", "no fail fixture", rule.id
            );
        }
    }
    let summary = format!(
        "{} fixture(s), {misjudged} misjudged, vacuous or unproven, {errors} error(s)\n",
        fixtures.len()
    );
    let code = if errors > 0 {
        2
    } else if misjudged > 0 {
        1
    } else {
        0
    };
    (out, summary, code)
}

fn verdict(
    fixture: &Fixture,
    rule: usize,
    outcomes: &std::collections::HashMap<Question, Outcome>,
    threshold: f64,
) -> (String, &'static str) {
    if let Some(error) = &fixture.error {
        return (error.clone(), "error");
    }
    if fixture.units.is_empty() {
        // Either way the fixture proves nothing: a pass fixture with no
        // scope the rule applies to passes vacuously.
        return ("no matching scopes".to_string(), "vacuous");
    }
    let mut strongest_fail: Option<f64> = None;
    for &unit in &fixture.units {
        match outcomes.get(&Question { unit, rule }) {
            Some(Outcome::Answered { answer, .. }) => {
                if answer.choice == Choice::Fail && answer.confidence >= threshold {
                    strongest_fail = Some(
                        strongest_fail.map_or(answer.confidence, |c: f64| c.max(answer.confidence)),
                    );
                }
            }
            Some(Outcome::Error(message)) => return (message.clone(), "error"),
            None => return ("not evaluated".to_string(), "error"),
        }
    }
    let got = match strongest_fail {
        Some(confidence) => format!("fail ({confidence:.2})"),
        None => "pass".to_string(),
    };
    let ok = match fixture.expect {
        Expect::Pass => strongest_fail.is_none(),
        Expect::Fail => strongest_fail.is_some(),
    };
    (got, if ok { "ok" } else { "misjudged" })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jev::Answer;
    use crate::rules::test_rule;
    use std::collections::HashMap;

    fn answered(choice: Choice, confidence: f64) -> Outcome {
        Outcome::Answered {
            answer: Answer {
                choice,
                confidence,
                p_fail: None,
            },
            cached: false,
        }
    }

    fn fixture(expect: Expect, units: Vec<usize>) -> Fixture {
        Fixture {
            rule: 0,
            expect,
            path: format!("f{}", units.first().copied().unwrap_or(99)),
            units,
            error: None,
        }
    }

    #[test]
    fn judges_fixtures_and_flags_unproven_rules() {
        let rules = vec![test_rule("a", ""), test_rule("b", "")];
        let mut outcomes = HashMap::new();
        outcomes.insert(Question { unit: 0, rule: 0 }, answered(Choice::Pass, 0.9));
        outcomes.insert(Question { unit: 1, rule: 0 }, answered(Choice::Fail, 0.95));
        outcomes.insert(Question { unit: 2, rule: 0 }, answered(Choice::Fail, 0.5));
        let fixtures = vec![
            fixture(Expect::Pass, vec![0]),
            fixture(Expect::Fail, vec![1, 2]),
        ];
        let (table, _, code) = judge(&rules[..1], &fixtures, &outcomes, 0.8);
        assert_eq!(code, 0, "{table}");
        assert!(table.contains("fail (0.95)"));

        let (table, _, code) = judge(&rules, &fixtures, &outcomes, 0.8);
        assert_eq!(code, 1);
        assert!(table.contains("unproven"));

        let weak = vec![fixture(Expect::Fail, vec![2])];
        let (table, _, code) = judge(&rules[..1], &weak, &outcomes, 0.8);
        assert_eq!(code, 1);
        assert!(table.contains("misjudged"));
    }

    #[test]
    fn fixtures_without_matching_scopes_are_vacuous_either_way() {
        let rules = vec![test_rule("a", "")];
        let outcomes = HashMap::new();
        for expect in [Expect::Pass, Expect::Fail] {
            let (table, _, code) = judge(
                &rules,
                &[fixture(expect, vec![]), fixture(Expect::Fail, vec![])],
                &outcomes,
                0.8,
            );
            assert_eq!(code, 1, "{table}");
            assert!(table.contains("vacuous"));
        }
    }

    #[test]
    fn errors_win() {
        let rules = vec![test_rule("a", "")];
        let mut outcomes = HashMap::new();
        outcomes.insert(
            Question { unit: 0, rule: 0 },
            Outcome::Error("HTTP 500".into()),
        );
        let (table, _, code) = judge(&rules, &[fixture(Expect::Fail, vec![0])], &outcomes, 0.8);
        assert_eq!(code, 2);
        assert!(table.contains("HTTP 500"));
    }
}
