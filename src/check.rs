//! `lintent check`: extract scopes, match rules, ask Jev, report.

use std::path::PathBuf;

use anyhow::{bail, Result};

use crate::budget;
use crate::cache::Cache;
use crate::engine::{self, Outcome, Question, Settings};
use crate::extract::Unit;
use crate::git;
use crate::jev::{Choice, Client};
use crate::keep::KEEP_RULE_ID;
use crate::report::{ErrorEntry, Finding, Report, Status};
use crate::rules::{Rule, Severity};
use crate::workspace::Workspace;

#[derive(Debug, Clone, Default)]
pub struct CheckOptions {
    pub paths: Vec<PathBuf>,
    pub rules: Vec<String>,
    pub changed: bool,
    pub base: Option<String>,
    pub json: bool,
    pub dry_run: bool,
    pub no_cache: bool,
    pub refresh: bool,
    /// Overrides `[budget] max_cost_usd`.
    pub max_cost: Option<f64>,
    /// The lowest severity whose confident fails set exit 1.
    pub fail_on: Severity,
}

/// Units and the questions to ask about them, plus problems found on the way.
pub struct Collected {
    pub units: Vec<Unit>,
    pub questions: Vec<Question>,
    pub report: Report,
}

/// Parses every matching file and decides which (unit, rule) questions to
/// ask. Files no selected rule includes are never parsed.
pub fn collect(workspace: &Workspace, rules: &[Rule], options: &CheckOptions) -> Result<Collected> {
    let changed = if options.changed {
        Some(git::changed_files(
            &workspace.project.root,
            options.base.as_deref(),
        )?)
    } else {
        None
    };
    let discovery = workspace.discover(&options.paths, changed.as_ref())?;
    for note in discovery.skipped_notes(&options.paths) {
        eprintln!("{note}");
    }

    let mut collected = Collected {
        units: Vec::new(),
        questions: Vec::new(),
        report: Report::default(),
    };
    if options.paths.is_empty() && !options.changed {
        warn_unmatched_includes(rules, &discovery.files);
    }
    for file in &discovery.files {
        let file_rules: Vec<usize> = (0..rules.len())
            .filter(|&index| {
                rules[index].matches_path(&file.path)
                    && rules[index].applies_to_language(&file.language)
            })
            .collect();
        if file_rules.is_empty() {
            continue;
        }
        let (parsed, keeps) = match workspace.parse(file) {
            Ok(Some(parsed)) => parsed,
            Ok(None) => {
                collected.report.stats.skipped_files += 1;
                continue;
            }
            Err(error) => {
                collected.report.errors.push(ErrorEntry {
                    path: Some(file.path.clone()),
                    line: None,
                    rule: None,
                    message: format!("{error:#}"),
                });
                continue;
            }
        };
        for problem in &keeps.problems {
            collected.report.findings.push(keep_finding(
                &file.path,
                problem.line,
                Severity::Error,
                problem.message.clone(),
            ));
        }
        // A mark that names a selected rule but suppresses nothing is stale:
        // the code it excused moved or was deleted.
        for mark in &keeps.marks {
            for id in &mark.rules {
                let Some(rule) = rules.iter().find(|rule| &rule.id == id) else {
                    continue;
                };
                let used = mark.unit.is_some_and(|unit| {
                    rule.matches_path(&file.path) && rule.applies_to(&parsed.units[unit])
                });
                if !used {
                    collected.report.findings.push(keep_finding(
                        &file.path,
                        mark.line,
                        Severity::Warning,
                        format!(
                            "`lintent-keep {id}` suppresses nothing: no {} scope starts on this line or directly below it",
                            rule.scopes.iter().map(|s| s.as_str()).collect::<Vec<_>>().join("/")
                        ),
                    ));
                }
            }
        }
        for (index, unit) in parsed.units.into_iter().enumerate() {
            let suppressed = keeps.suppressed(index);
            let asked: Vec<usize> = file_rules
                .iter()
                .copied()
                .filter(|&rule| {
                    rules[rule].applies_to(&unit)
                        && !suppressed.contains_key(rules[rule].id.as_str())
                })
                .collect();
            if asked.is_empty() {
                continue;
            }
            let unit_index = collected.units.len();
            collected.units.push(unit);
            collected
                .questions
                .extend(asked.into_iter().map(|rule| Question {
                    unit: unit_index,
                    rule,
                }));
        }
    }
    collected.report.stats.units = collected.units.len();
    collected.report.stats.truncated = collected.units.iter().filter(|unit| unit.truncated).count();
    if collected.questions.is_empty() && !rules.is_empty() {
        eprintln!(
            "lintent: warning: the selected rule(s) match no scopes here; preview a rule's reach with `lintent scopes --rule <id>`"
        );
    }
    Ok(collected)
}

fn keep_finding(path: &str, line: usize, severity: Severity, message: String) -> Finding {
    Finding {
        path: path.to_string(),
        line,
        end_line: line,
        kind: None,
        name: None,
        rule: KEEP_RULE_ID.to_string(),
        severity,
        status: Status::Fail,
        confidence: None,
        min_confidence: None,
        p_fail: None,
        why: None,
        fix: None,
        message: Some(message),
    }
}

/// Warns about rules whose `include` matches no file in the project — almost
/// always a typo in a glob, which would otherwise look like a clean pass.
pub fn warn_unmatched_includes(rules: &[Rule], files: &[crate::workspace::SourceFile]) {
    for rule in rules {
        if !files.iter().any(|file| rule.matches_path(&file.path)) {
            eprintln!(
                "lintent: warning: rule `{}` matches no files (include = {:?}); check its globs",
                rule.id, rule.include
            );
        }
    }
}

/// A per-run id, sent as `session_id` so a run's requests group together.
pub fn session_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    format!("lintent-{:x}-{:x}", nanos, std::process::id())
}

/// The API client for a real run; fails early when the key is missing.
pub fn client(workspace: &Workspace) -> Result<Client> {
    let config = &workspace.project.config;
    let variable = config.provider.key_variable();
    let Some(key) = workspace.env.get(variable) else {
        bail!("{variable} is not set (in the environment or ./.env); use --dry-run to see the requests without sending them");
    };
    let title = (config.provider == crate::config::Provider::Openrouter).then_some("lintent");
    Ok(Client::new(config.endpoint(), key, variable, title))
}

pub fn run(workspace: &Workspace, options: &CheckOptions) -> Result<i32> {
    let config = &workspace.project.config;
    let rules = workspace.select_rules(&options.rules)?;
    let Collected {
        units,
        questions,
        mut report,
    } = collect(workspace, &rules, options)?;
    report.fail_on = options.fail_on;

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
        read_cache: !(options.no_cache || options.refresh),
    };
    let mut plan = engine::plan(&settings, &units, &rules, &questions, &cache);
    report.stats.questions = plan.questions;
    report.stats.cache_hits = plan.cache_hits;

    // The budget gate: everything below this point may cost money.
    let max_cost = options.max_cost.unwrap_or(config.budget.max_cost_usd);
    let estimate = budget::estimate(&plan.jobs, &units, &rules, &config.budget, max_cost);
    let over_budget = estimate.over_budget;
    report.budget = Some(estimate);

    if options.dry_run {
        print_dry_run(
            &config.endpoint(),
            &plan.jobs,
            &units,
            &report,
            options.json,
        );
        if let Some(estimate) = report.budget.as_ref().filter(|_| over_budget) {
            eprint!("\n{}", budget::guidance(estimate, false));
        }
        return Ok(report.exit_code());
    }
    if over_budget {
        report.sort();
        if options.json {
            println!("{}", report.to_json());
        } else {
            print!("{}", report.to_human());
        }
        if let Some(estimate) = &report.budget {
            eprint!("{}", budget::guidance(estimate, false));
        }
        return Ok(report.exit_code());
    }

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
        report.stats.requests = execution.requests;
        report.stats.input_tokens = execution.usage.input_tokens;
        report.stats.output_tokens = execution.usage.output_tokens;
        report.stats.cost = execution.usage.cost;
        if let Some(estimate) = report.budget.as_mut() {
            estimate.actual_cost_usd = Some(execution.usage.cost);
        }
        if let Some(fatal) = execution.fatal {
            report.errors.push(ErrorEntry {
                path: None,
                line: None,
                rule: None,
                message: format!("stopped: {fatal}"),
            });
        }
        if let Err(error) = cache.save() {
            eprintln!("lintent: warning: could not save the cache: {error:#}");
        }
    }

    for question in &questions {
        let unit = &units[question.unit];
        let rule = &rules[question.rule];
        match plan.outcomes.get(question) {
            Some(Outcome::Answered { answer, .. }) => {
                let threshold = rule.threshold(config.min_confidence);
                let status = match answer.choice {
                    Choice::Fail if answer.confidence >= threshold => Status::Fail,
                    Choice::Fail => Status::Uncertain,
                    Choice::Pass | Choice::Skip => continue,
                };
                report.findings.push(finding(
                    unit,
                    rule,
                    status,
                    Some(answer.confidence),
                    Some(threshold),
                    answer.p_fail,
                ));
            }
            Some(Outcome::Error(message)) => report.errors.push(ErrorEntry {
                path: Some(unit.path.clone()),
                line: Some(unit.start_line),
                rule: Some(rule.id.clone()),
                message: format!("{} {}: {message}", unit.kind, unit.name),
            }),
            // Never sent because the run stopped; the fatal error says why.
            None => {}
        }
    }

    report.sort();
    emit(&report, options.json);
    if !options.json {
        for unit in units.iter().filter(|unit| unit.truncated) {
            eprintln!(
                "note: {}:{} {} {} was truncated before sending",
                unit.path, unit.start_line, unit.kind, unit.name
            );
        }
    }
    Ok(report.exit_code())
}

/// Findings (and errors) to stdout, the summary to stderr.
fn emit(report: &Report, json: bool) {
    if json {
        println!("{}", report.to_json());
    } else {
        print!("{}", report.to_human());
        eprint!("{}", report.summary());
    }
}

/// Explicit paths that land in an `exclude` glob would otherwise look like
/// "nothing to lint" with no explanation.
fn finding(
    unit: &Unit,
    rule: &Rule,
    status: Status,
    confidence: Option<f64>,
    min_confidence: Option<f64>,
    p_fail: Option<f64>,
) -> Finding {
    Finding {
        path: unit.path.clone(),
        line: unit.start_line,
        end_line: unit.end_line,
        kind: Some(unit.kind),
        name: Some(unit.name.clone()),
        rule: rule.id.clone(),
        severity: rule.severity,
        status,
        confidence,
        min_confidence,
        p_fail,
        why: rule.why.clone(),
        fix: rule.fix.clone(),
        message: None,
    }
}

/// Prints the exact request bodies; nothing is sent and no key is needed.
pub fn print_dry_run(
    endpoint: &str,
    jobs: &[engine::Job],
    units: &[Unit],
    report: &Report,
    json: bool,
) {
    if json {
        let requests: Vec<&crate::jev::Request> = jobs.iter().map(|job| &job.request).collect();
        let value = serde_json::json!({
            "endpoint": endpoint,
            "requests": requests,
            "stats": {
                "units": report.stats.units,
                "questions": report.stats.questions,
                "cache_hits": report.stats.cache_hits,
                "requests": jobs.len(),
            },
            "budget": report.budget,
            "findings": report.findings,
            "errors": report.errors,
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&value).expect("serializable")
        );
        return;
    }
    for job in jobs {
        let unit = &units[job.unit];
        let keys: Vec<&str> = job.request.questions.keys().map(String::as_str).collect();
        println!(
            "# POST {endpoint}  {}:{} {} {}  questions: {}",
            unit.path,
            unit.start_line,
            unit.kind,
            unit.name,
            keys.join(", ")
        );
        println!(
            "{}",
            serde_json::to_string_pretty(&job.request).expect("serializable")
        );
    }
    for finding in &report.findings {
        println!(
            "{}:{}  {}  {}",
            finding.path,
            finding.line,
            finding.rule,
            finding.message.as_deref().unwrap_or("")
        );
    }
    for error in &report.errors {
        eprintln!(
            "error: {}: {}",
            error.path.as_deref().unwrap_or("-"),
            error.message
        );
    }
    eprintln!(
        "dry run: {} request(s) for {} question(s) on {} scope(s) ({} cached); nothing sent",
        jobs.len(),
        report.stats.questions,
        report.stats.units,
        report.stats.cache_hits
    );
    if let Some(budget) = &report.budget {
        eprintln!("{}", budget::summary_line(budget));
    }
}
