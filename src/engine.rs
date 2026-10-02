//! Turns (unit, rule) questions into System One requests and runs them.
//!
//! Shared by `check` and `eval`. Planning is pure (and is all `--dry-run`
//! needs); execution fans the requests out over a fixed number of threads.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;

use crate::cache::{question_key, Cache};
use crate::extract::Unit;
use crate::jev::{build_request, Answer, ApiError, Client, Decoded, Request, Usage};
use crate::rules::Rule;

/// Index of a unit and a rule in the caller's slices.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Question {
    pub unit: usize,
    pub rule: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Answered { answer: Answer, cached: bool },
    Error(String),
}

/// One HTTP request to make.
#[derive(Debug, Clone)]
pub struct Job {
    pub unit: usize,
    pub rules: Vec<usize>,
    pub request: Request,
}

pub struct Settings<'a> {
    pub model: &'a str,
    /// Everything besides rule and unit that keys the cache (`Config::cache_scope`).
    pub cache_scope: &'a str,
    pub session_id: &'a str,
    pub isolate_rules: bool,
    /// False for `--no-cache` / `--refresh`: ask again even if cached.
    pub read_cache: bool,
}

#[derive(Debug, Default)]
pub struct Plan {
    pub jobs: Vec<Job>,
    pub outcomes: HashMap<Question, Outcome>,
    pub questions: usize,
    pub cache_hits: usize,
}

/// Groups questions by unit: one request per unit with each rule as its own
/// keyed question, or one request per question with `isolate_rules`.
/// Questions already answered in the cache never reach a request.
pub fn plan(
    settings: &Settings,
    units: &[Unit],
    rules: &[Rule],
    questions: &[Question],
    cache: &Cache,
) -> Plan {
    let mut plan = Plan {
        questions: questions.len(),
        ..Plan::default()
    };
    let mut by_unit: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for question in questions {
        let cached = settings
            .read_cache
            .then(|| {
                cache.get(&question_key(
                    settings.cache_scope,
                    &rules[question.rule],
                    &units[question.unit],
                ))
            })
            .flatten();
        match cached {
            Some(answer) => {
                plan.cache_hits += 1;
                plan.outcomes.insert(
                    *question,
                    Outcome::Answered {
                        answer,
                        cached: true,
                    },
                );
            }
            None => by_unit
                .entry(question.unit)
                .or_default()
                .push(question.rule),
        }
    }
    for (unit, rule_indices) in by_unit {
        let groups: Vec<Vec<usize>> = if settings.isolate_rules {
            rule_indices.into_iter().map(|rule| vec![rule]).collect()
        } else {
            vec![rule_indices]
        };
        for group in groups {
            let refs: Vec<&Rule> = group.iter().map(|&index| &rules[index]).collect();
            let request = build_request(settings.model, settings.session_id, &units[unit], &refs);
            plan.jobs.push(Job {
                unit,
                rules: group,
                request,
            });
        }
    }
    plan
}

#[derive(Debug, Default)]
pub struct Execution {
    pub requests: usize,
    pub usage: Usage,
    /// Set when the API refused the whole run (bad key, no credits). Jobs
    /// not yet sent are dropped and have no outcome.
    pub fatal: Option<String>,
}

/// Sends every job, records answers in `plan.outcomes` and fresh answers in
/// the cache.
pub fn execute(
    plan: &mut Plan,
    units: &[Unit],
    rules: &[Rule],
    client: &Client,
    concurrency: usize,
    cache_scope: &str,
    cache: &mut Cache,
) -> Execution {
    let next = AtomicUsize::new(0);
    let abort = AtomicBool::new(false);
    let results: Mutex<Vec<(usize, Result<Decoded, ApiError>)>> = Mutex::new(Vec::new());
    let jobs = &plan.jobs;
    std::thread::scope(|scope| {
        for _ in 0..concurrency.clamp(1, jobs.len().max(1)) {
            scope.spawn(|| loop {
                if abort.load(Ordering::SeqCst) {
                    break;
                }
                let index = next.fetch_add(1, Ordering::SeqCst);
                let Some(job) = jobs.get(index) else {
                    break;
                };
                let result = client.evaluate(&job.request);
                if matches!(result, Err(ApiError::Fatal(_))) {
                    abort.store(true, Ordering::SeqCst);
                }
                results
                    .lock()
                    .expect("no worker panics while holding the lock")
                    .push((index, result));
            });
        }
    });

    let mut execution = Execution::default();
    let mut results = results.into_inner().expect("workers have finished");
    results.sort_by_key(|(index, _)| *index);
    for (index, result) in results {
        execution.requests += 1;
        let job = &plan.jobs[index];
        match result {
            Ok(decoded) => {
                execution.usage.add(decoded.usage);
                for &rule in &job.rules {
                    let question = Question {
                        unit: job.unit,
                        rule,
                    };
                    let outcome = match decoded.answers.get(&rules[rule].id) {
                        Some(Ok(answer)) => {
                            cache.put(
                                question_key(cache_scope, &rules[rule], &units[job.unit]),
                                *answer,
                            );
                            Outcome::Answered {
                                answer: *answer,
                                cached: false,
                            }
                        }
                        Some(Err(message)) => Outcome::Error(message.clone()),
                        None => Outcome::Error("no answer".to_string()),
                    };
                    plan.outcomes.insert(question, outcome);
                }
            }
            Err(ApiError::Request(message)) => {
                for &rule in &job.rules {
                    plan.outcomes.insert(
                        Question {
                            unit: job.unit,
                            rule,
                        },
                        Outcome::Error(message.clone()),
                    );
                }
            }
            Err(ApiError::Fatal(message)) => {
                execution.fatal.get_or_insert(message);
            }
        }
    }
    execution
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extract::ScopeKind;
    use crate::jev::Choice;
    use crate::rules::test_rule;

    fn unit(name: &str) -> Unit {
        Unit {
            kind: ScopeKind::Function,
            name: name.into(),
            language: "typescript".into(),
            path: "src/a.ts".into(),
            start_line: 1,
            end_line: 1,
            source: format!("function {name}() {{}}"),
            parent_source: None,
            truncated: false,
        }
    }

    fn settings(isolate_rules: bool, read_cache: bool) -> Settings<'static> {
        Settings {
            model: "m",
            cache_scope: "m",
            session_id: "s",
            isolate_rules,
            read_cache,
        }
    }

    #[test]
    fn rules_on_one_unit_are_separate_keyed_questions_in_one_request() {
        let units = vec![unit("f"), unit("g")];
        let rules = vec![test_rule("a", ""), test_rule("b", "")];
        let questions = vec![
            Question { unit: 0, rule: 0 },
            Question { unit: 0, rule: 1 },
            Question { unit: 1, rule: 1 },
        ];
        let plan = plan(
            &settings(false, true),
            &units,
            &rules,
            &questions,
            &Cache::disabled(),
        );
        assert_eq!(plan.jobs.len(), 2);
        let keys: Vec<_> = plan.jobs[0].request.questions.keys().cloned().collect();
        assert_eq!(keys, vec!["a", "b"]);
        assert_eq!(plan.jobs[1].rules, vec![1]);
    }

    #[test]
    fn isolate_rules_sends_one_request_per_question() {
        let units = vec![unit("f")];
        let rules = vec![test_rule("a", ""), test_rule("b", "")];
        let questions = vec![Question { unit: 0, rule: 0 }, Question { unit: 0, rule: 1 }];
        let plan = plan(
            &settings(true, true),
            &units,
            &rules,
            &questions,
            &Cache::disabled(),
        );
        assert_eq!(plan.jobs.len(), 2);
        assert!(plan.jobs.iter().all(|job| job.request.questions.len() == 1));
    }

    #[test]
    fn cached_questions_are_not_sent_unless_refreshing() {
        let units = vec![unit("f")];
        let rules = vec![test_rule("a", ""), test_rule("b", "")];
        let questions = vec![Question { unit: 0, rule: 0 }, Question { unit: 0, rule: 1 }];
        let mut cache = Cache::disabled();
        let answer = Answer {
            choice: Choice::Pass,
            confidence: 0.9,
            p_fail: None,
        };
        cache.put(question_key("m", &rules[0], &units[0]), answer);

        let cached = plan(&settings(false, true), &units, &rules, &questions, &cache);
        assert_eq!(cached.cache_hits, 1);
        assert_eq!(cached.jobs.len(), 1);
        assert_eq!(cached.jobs[0].rules, vec![1]);
        assert_eq!(
            cached.outcomes[&Question { unit: 0, rule: 0 }],
            Outcome::Answered {
                answer,
                cached: true
            }
        );

        let refreshed = plan(&settings(false, false), &units, &rules, &questions, &cache);
        assert_eq!(refreshed.cache_hits, 0);
        assert_eq!(refreshed.jobs[0].rules, vec![0, 1]);
    }
}
