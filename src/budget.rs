//! The cost budget: estimate a run before anything is sent, and refuse to
//! send when it is over `[budget] max_cost_usd`.
//!
//! The estimate must never come in under the bill, because an estimate that
//! errs low lets an over-budget run through. Per request it assumes
//! `request_overhead_tokens + question_overhead_tokens × questions +
//! ceil(body bytes / 2.5)` input tokens and 40 output tokens per question.
//! Calibration (typesafe/jev-1.13, 2026-10): a 509-byte request with one
//! question was billed 394 input and 38 output tokens — about 2.3× a plain
//! bytes/3 guess, because the provider wraps every request in its own
//! prompt. The defaults give 584 for that request. Only requests that would
//! actually be sent are counted — cached and kept questions cost $0.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use serde::Serialize;

use crate::config::Budget;
use crate::engine::Job;
use crate::extract::Unit;
use crate::rules::Rule;

/// Request-body bytes per input token, as a fraction (2.5 = 5 / 2).
const BYTES_PER_TOKEN_NUMERATOR: u64 = 5;
const BYTES_PER_TOKEN_DENOMINATOR: u64 = 2;
/// Output tokens assumed per question.
pub const OUTPUT_TOKENS_PER_QUESTION: u64 = 40;

/// Estimated input tokens for one request.
pub fn input_tokens(body_bytes: usize, questions: usize, prices: &Budget) -> u64 {
    let text =
        (body_bytes as u64 * BYTES_PER_TOKEN_DENOMINATOR).div_ceil(BYTES_PER_TOKEN_NUMERATOR);
    prices.request_overhead_tokens + prices.question_overhead_tokens * questions as u64 + text
}
/// How many contributors the guidance lists per category.
const TOP: usize = 5;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Contributor {
    /// A rule id, file path or directory.
    pub name: String,
    pub questions: usize,
    pub estimated_cost_usd: f64,
    pub share: f64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct LargestScope {
    pub path: String,
    pub line: usize,
    pub kind: String,
    pub name: String,
    pub estimated_cost_usd: f64,
    pub estimated_input_tokens: u64,
    /// True when this one scope alone exceeds the budget.
    pub alone_over_budget: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Estimate {
    pub estimated_cost_usd: f64,
    pub max_cost_usd: f64,
    pub estimated_input_tokens: u64,
    pub estimated_output_tokens: u64,
    pub requests: usize,
    pub questions: usize,
    pub scopes: usize,
    pub over_budget: bool,
    pub by_rule: Vec<Contributor>,
    pub by_path: Vec<Contributor>,
    pub by_directory: Vec<Contributor>,
    pub largest_scope: Option<LargestScope>,
    /// Filled in after a real run, from the provider's `usage.cost`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actual_cost_usd: Option<f64>,
}

/// Estimates the cost of sending `jobs` and compares it with `max_cost_usd`.
pub fn estimate(
    jobs: &[Job],
    units: &[Unit],
    rules: &[Rule],
    prices: &Budget,
    max_cost_usd: f64,
) -> Estimate {
    let price = |input: u64, output: u64| {
        input as f64 * prices.input_price_per_million / 1e6
            + output as f64 * prices.output_price_per_million / 1e6
    };
    let mut input_total = 0;
    let mut output_total = 0;
    let mut by_rule: BTreeMap<String, (usize, f64)> = BTreeMap::new();
    let mut by_path: BTreeMap<String, (usize, f64)> = BTreeMap::new();
    let mut by_directory: BTreeMap<String, (usize, f64)> = BTreeMap::new();
    let mut by_unit: BTreeMap<usize, (u64, f64)> = BTreeMap::new();
    let mut questions = 0;

    for job in jobs {
        let bytes = serde_json::to_string(&job.request)
            .map(|body| body.len())
            .unwrap_or(0);
        let input = input_tokens(bytes, job.rules.len(), prices);
        let output = OUTPUT_TOKENS_PER_QUESTION * job.rules.len() as u64;
        let cost = price(input, output);
        input_total += input;
        output_total += output;
        questions += job.rules.len();

        // A batched request shares its state between its questions, so each
        // question carries an equal share of the request's cost.
        let share = cost / job.rules.len().max(1) as f64;
        for &rule in &job.rules {
            let entry = by_rule.entry(rules[rule].id.clone()).or_default();
            entry.0 += 1;
            entry.1 += share;
        }
        let path = &units[job.unit].path;
        let entry = by_path.entry(path.clone()).or_default();
        entry.0 += job.rules.len();
        entry.1 += cost;
        let directory = path
            .rsplit_once('/')
            .map_or(".", |(dir, _)| dir)
            .to_string();
        let entry = by_directory.entry(directory).or_default();
        entry.0 += job.rules.len();
        entry.1 += cost;
        let entry = by_unit.entry(job.unit).or_default();
        entry.0 += input;
        entry.1 += cost;
    }

    let total = price(input_total, output_total);
    let ranked = |map: BTreeMap<String, (usize, f64)>| {
        let mut list: Vec<Contributor> = map
            .into_iter()
            .map(|(name, (questions, cost))| Contributor {
                name,
                questions,
                estimated_cost_usd: cost,
                share: if total > 0.0 { cost / total } else { 0.0 },
            })
            .collect();
        list.sort_by(|a, b| {
            b.estimated_cost_usd
                .total_cmp(&a.estimated_cost_usd)
                .then(a.name.cmp(&b.name))
        });
        list
    };
    let largest_scope =
        by_unit
            .iter()
            .max_by(|a, b| a.1 .1.total_cmp(&b.1 .1))
            .map(|(&unit, &(input, cost))| {
                let unit = &units[unit];
                LargestScope {
                    path: unit.path.clone(),
                    line: unit.start_line,
                    kind: unit.kind.to_string(),
                    name: unit.name.clone(),
                    estimated_cost_usd: cost,
                    estimated_input_tokens: input,
                    alone_over_budget: cost > max_cost_usd,
                }
            });

    Estimate {
        estimated_cost_usd: total,
        max_cost_usd,
        estimated_input_tokens: input_total,
        estimated_output_tokens: output_total,
        requests: jobs.len(),
        questions,
        scopes: by_unit.len(),
        over_budget: total > max_cost_usd,
        by_rule: ranked(by_rule),
        by_path: ranked(by_path),
        by_directory: ranked(by_directory),
        largest_scope,
        actual_cost_usd: None,
    }
}

/// One line for dry runs and summaries.
pub fn summary_line(estimate: &Estimate) -> String {
    let verdict = if estimate.over_budget {
        "OVER budget"
    } else {
        "within budget"
    };
    let actual = estimate
        .actual_cost_usd
        .map(|cost| format!(" · actual {}", usd(cost)))
        .unwrap_or_default();
    format!(
        "estimate: ~{} ({} input tokens, {} question(s) on {} scope(s)) — {verdict} of {}{actual}",
        usd(estimate.estimated_cost_usd),
        thousands(estimate.estimated_input_tokens),
        estimate.questions,
        estimate.scopes,
        usd(estimate.max_cost_usd),
    )
}

/// The refusal, written for the agent (or person) who has to narrow the run.
pub fn guidance(estimate: &Estimate, eval: bool) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "lintent: this run would cost ~{} ({} input tokens, {} question(s) on {} scope(s)) — over the {} budget. \
         Nothing was sent. Narrow the scope before re-running:",
        usd(estimate.estimated_cost_usd),
        thousands(estimate.estimated_input_tokens),
        estimate.questions,
        estimate.scopes,
        usd(estimate.max_cost_usd),
    );
    let section = |out: &mut String, title: &str, list: &[Contributor]| {
        let _ = writeln!(out, "\n{title}:");
        for item in list.iter().take(TOP) {
            let _ = writeln!(
                out,
                "  {:<40} {:>4} question(s)  ~{:<10} {:>5.1}%",
                item.name,
                item.questions,
                usd(item.estimated_cost_usd),
                item.share * 100.0
            );
        }
        if list.len() > TOP {
            let _ = writeln!(out, "  … and {} more", list.len() - TOP);
        }
    };
    section(&mut out, "By rule", &estimate.by_rule);
    section(&mut out, "By file", &estimate.by_path);
    section(&mut out, "By directory", &estimate.by_directory);

    if let Some(largest) = estimate
        .largest_scope
        .as_ref()
        .filter(|scope| scope.alone_over_budget)
    {
        let _ = writeln!(
            out,
            "\nThe scope {}:{} {} {} alone costs ~{} ({} input tokens), more than the whole budget. \
             Split it, exclude its file from the rule, or mark it `lintent-keep <rule-id> -- <reason>` if it is acceptable as is.",
            largest.path,
            largest.line,
            largest.kind,
            largest.name,
            usd(largest.estimated_cost_usd),
            thousands(largest.estimated_input_tokens),
        );
    }

    let top_rule = estimate
        .by_rule
        .first()
        .map_or("<id>", |rule| rule.name.as_str());
    out.push_str("\nWays to narrow it:\n");
    let _ = writeln!(
        out,
        "  - tighten `include` / add `exclude` globs in .lintent/rules/{top_rule}.toml (preview: `lintent scopes --rule {top_rule}`)"
    );
    out.push_str("  - restrict the rule's `scopes` (e.g. only \"method\") or `languages`\n");
    if eval {
        out.push_str(
            "  - evaluate one rule at a time: `lintent eval --rule <id>`; keep fixtures small\n",
        );
    } else {
        out.push_str("  - lint only what changed: `lintent check --changed`\n");
        out.push_str("  - pass explicit PATHS: `lintent check src/feature/`\n");
        out.push_str("  - check one rule at a time: `lintent check --rule <id>`\n");
    }
    out.push_str(
        "  - verdicts are cached: questions answered before cost nothing, so small runs add up without re-paying\n",
    );
    out.push_str(
        "  - only if the spend is intended: raise `[budget] max_cost_usd` in lintent.toml or pass `--max-cost <USD>`\n",
    );
    out
}

/// `$0.00041`: dollars with up to 6 decimals, trailing zeros trimmed.
pub fn usd(value: f64) -> String {
    let text = format!("{value:.6}");
    let text = text.trim_end_matches('0');
    let text = if text.ends_with('.') {
        format!("{text}0")
    } else {
        text.to_string()
    };
    format!("${text}")
}

fn thousands(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::new();
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::Cache;
    use crate::engine::{plan, Question, Settings};
    use crate::extract::ScopeKind;
    use crate::rules::test_rule;

    fn unit(path: &str, source: &str) -> Unit {
        Unit {
            kind: ScopeKind::Function,
            name: "f".into(),
            language: "typescript".into(),
            path: path.into(),
            start_line: 1,
            end_line: 1,
            source: source.into(),
            parent_source: None,
            truncated: false,
        }
    }

    fn jobs(units: &[Unit], rules: &[Rule], questions: &[Question]) -> Vec<Job> {
        let settings = Settings {
            model: "m",
            cache_scope: "m",
            session_id: "s",
            isolate_rules: false,
            read_cache: true,
        };
        plan(&settings, units, rules, questions, &Cache::disabled()).jobs
    }

    #[test]
    fn estimates_from_body_size_and_ranks_contributors() {
        let units = vec![
            unit("src/a/big.ts", &"x".repeat(800)),
            unit("src/b/small.ts", "x"),
        ];
        let rules = vec![test_rule("alpha", ""), test_rule("beta", "")];
        let questions = vec![
            Question { unit: 0, rule: 0 },
            Question { unit: 0, rule: 1 },
            Question { unit: 1, rule: 1 },
        ];
        let jobs = jobs(&units, &rules, &questions);
        let body_bytes: usize = jobs
            .iter()
            .map(|j| serde_json::to_string(&j.request).unwrap().len())
            .sum();
        let estimate = estimate(&jobs, &units, &rules, &Budget::default(), 0.0001);

        assert_eq!(estimate.requests, 2);
        assert_eq!(estimate.questions, 3);
        assert_eq!(estimate.scopes, 2);
        assert_eq!(
            estimate.estimated_input_tokens,
            2 * 300
                + 3 * 80
                + jobs
                    .iter()
                    .map(
                        |j| (serde_json::to_string(&j.request).unwrap().len() as u64 * 2)
                            .div_ceil(5)
                    )
                    .sum::<u64>()
        );
        assert!(estimate.estimated_input_tokens > (body_bytes / 3) as u64);
        assert_eq!(estimate.estimated_output_tokens, 120);
        let expected = estimate.estimated_input_tokens as f64 * 0.042 / 1e6;
        assert!((estimate.estimated_cost_usd - expected).abs() < 1e-15);
        assert!(!estimate.over_budget, "~1.8k tokens is under $0.0001");

        assert_eq!(estimate.by_path[0].name, "src/a/big.ts");
        assert_eq!(estimate.by_directory[0].name, "src/a");
        assert_eq!(
            estimate.by_rule[0].name, "beta",
            "beta is asked about both files"
        );
        let shares: f64 = estimate.by_rule.iter().map(|r| r.share).sum();
        assert!((shares - 1.0).abs() < 1e-9);
        assert_eq!(
            estimate.largest_scope.as_ref().unwrap().path,
            "src/a/big.ts"
        );

        let tight = super::estimate(&jobs, &units, &rules, &Budget::default(), 0.00001);
        assert!(tight.over_budget);
        assert!(tight.largest_scope.unwrap().alone_over_budget);
    }

    #[test]
    fn never_under_the_calibration_bill() {
        // The measured request: 509 bytes, one question, billed 394 input tokens.
        assert!(input_tokens(509, 1, &Budget::default()) >= 394);
    }

    #[test]
    fn nothing_to_send_costs_nothing() {
        let estimate = estimate(&[], &[], &[], &Budget::default(), 0.0001);
        assert_eq!(estimate.estimated_cost_usd, 0.0);
        assert!(!estimate.over_budget);
        assert!(estimate.largest_scope.is_none());
    }

    #[test]
    fn guidance_names_rules_files_and_actions() {
        let units = vec![unit("src/a/big.ts", &"x".repeat(9000))];
        let rules = vec![test_rule("alpha", "")];
        let jobs = jobs(&units, &rules, &[Question { unit: 0, rule: 0 }]);
        let estimate = estimate(&jobs, &units, &rules, &Budget::default(), 0.0001);
        assert!(estimate.over_budget);
        let text = guidance(&estimate, false);
        assert!(
            text.starts_with("lintent: this run would cost ~$0.0001"),
            "{text}"
        );
        assert!(text.contains("over the $0.0001 budget. Nothing was sent."));
        assert!(text.contains("By rule:\n  alpha"));
        assert!(text.contains("By file:\n  src/a/big.ts"));
        assert!(text.contains("alone costs"));
        assert!(text.contains(".lintent/rules/alpha.toml"));
        assert!(text.contains("lintent scopes --rule alpha"));
        assert!(text.contains("--changed"));
        assert!(text.contains("--max-cost"));
    }

    #[test]
    fn formatting() {
        assert_eq!(usd(0.0001), "$0.0001");
        assert_eq!(usd(0.00041234), "$0.000412");
        assert_eq!(usd(0.0), "$0.0");
        assert_eq!(usd(1.5), "$1.5");
        assert_eq!(thousands(9812), "9,812");
        assert_eq!(thousands(1234567), "1,234,567");
        assert_eq!(thousands(12), "12");
    }
}
