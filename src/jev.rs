//! Jev over the System One API: wire types, request building, response
//! decoding and the HTTP client.
//!
//! Both providers (OpenRouter, TypeSafe) serve the same API at
//! `<base_url>/systemone`. A request carries one code unit as `state` and
//! one keyed `choice` question per rule, so several rules about the same
//! scope are separate questions that are never merged into one prompt.

use std::collections::BTreeMap;
use std::fmt;
use std::thread;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::extract::{ScopeKind, Unit};
use crate::rules::Rule;

pub const CRITERION_PASS: &str =
    "The code complies with the rule, or an explicit exception applies.";
pub const CRITERION_FAIL: &str = "The code violates the rule, and no explicit exception applies.";
pub const CRITERION_SKIP: &str =
    "The rule's subject is not present in this unit; the rule does not apply.";

const MAX_RETRIES: u32 = 2;
const BACKOFF_BASE: Duration = Duration::from_millis(500);
const BACKOFF_CAP: Duration = Duration::from_secs(5);
/// A server asking for longer than this is treated as a plain backoff.
const RETRY_AFTER_MAX: Duration = Duration::from_secs(60);
const TIMEOUT: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------- requests

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Request {
    pub model: String,
    /// One id per lintent run, so the provider can group a run's requests.
    pub session_id: String,
    pub state: State,
    pub questions: BTreeMap<String, Question>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct State {
    pub kind: ScopeKind,
    pub name: String,
    pub language: String,
    pub path: String,
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_source: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Question {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub instructions: String,
    pub criteria: Criteria,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Criteria {
    pub pass: &'static str,
    pub fail: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skip: Option<&'static str>,
}

/// One request: the unit as state, each rule its own keyed question.
pub fn build_request(model: &str, session_id: &str, unit: &Unit, rules: &[&Rule]) -> Request {
    let questions = rules
        .iter()
        .map(|rule| {
            let question = Question {
                kind: "choice",
                instructions: instructions(rule, unit),
                criteria: Criteria {
                    pass: CRITERION_PASS,
                    fail: CRITERION_FAIL,
                    skip: rule.allow_skip.then_some(CRITERION_SKIP),
                },
            };
            (rule.id.clone(), question)
        })
        .collect();
    Request {
        model: model.to_string(),
        session_id: session_id.to_string(),
        state: State {
            kind: unit.kind,
            name: unit.name.clone(),
            language: unit.language.clone(),
            path: unit.path.clone(),
            source: unit.source.clone(),
            parent_source: unit.parent_source.clone(),
        },
        questions,
    }
}

/// The question text. With a parent, the model is told explicitly that only
/// `state.source` is under review, so a method is not failed for something
/// its class does elsewhere.
pub fn instructions(rule: &Rule, unit: &Unit) -> String {
    let mut text = String::new();
    if unit.parent_source.is_some() {
        text.push_str(
            "Determine whether state.source violates this rule. Use state.parentSource only as surrounding context:\n",
        );
    } else {
        text.push_str("Determine whether the supplied code complies with this rule:\n");
    }
    text.push_str(rule.description.trim());
    if !rule.exceptions.is_empty() {
        text.push_str("\n\nExplicit exceptions:");
        for exception in &rule.exceptions {
            text.push_str("\n- ");
            text.push_str(exception);
        }
    }
    text
}

// --------------------------------------------------------------- responses

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Choice {
    Pass,
    Fail,
    Skip,
}

impl fmt::Display for Choice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(match self {
            Choice::Pass => "pass",
            Choice::Fail => "fail",
            Choice::Skip => "skip",
        })
    }
}

/// A validated answer to one question.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Answer {
    pub choice: Choice,
    pub confidence: f64,
    /// `probabilities.fail` when the model reported a distribution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p_fail: Option<f64>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub cost: f64,
}

impl Usage {
    pub fn add(&mut self, other: Usage) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.cost += other.cost;
    }
}

/// A decoded response: one result per question that was asked.
#[derive(Debug, Clone, PartialEq)]
pub struct Decoded {
    pub answers: BTreeMap<String, Result<Answer, String>>,
    pub usage: Usage,
}

#[derive(Deserialize)]
struct RawResponse {
    answers: Option<BTreeMap<String, RawAnswer>>,
    usage: Option<Usage>,
}

#[derive(Deserialize)]
struct RawAnswer {
    #[serde(rename = "type")]
    kind: Option<String>,
    choice: Option<String>,
    confidence: Option<f64>,
    probabilities: Option<BTreeMap<String, f64>>,
}

/// Decodes a response for `request`. A broken body fails the whole request;
/// a broken or missing answer is an error for that question only.
pub fn decode(body: &str, request: &Request) -> Result<Decoded, String> {
    let response: RawResponse = serde_json::from_str(body)
        .map_err(|error| format!("response is not valid JSON: {error}"))?;
    let usage = response.usage.unwrap_or_default();
    let answers = response
        .answers
        .ok_or_else(|| "response has no `answers`".to_string())?;
    let decoded = request
        .questions
        .iter()
        .map(|(id, question)| {
            let result = match answers.get(id) {
                None => Err(format!("no answer for rule `{id}`")),
                Some(answer) => decode_answer(answer, question.criteria.skip.is_some()),
            };
            (id.clone(), result)
        })
        .collect();
    Ok(Decoded {
        answers: decoded,
        usage,
    })
}

fn decode_answer(answer: &RawAnswer, skip_allowed: bool) -> Result<Answer, String> {
    if let Some(kind) = &answer.kind {
        if kind != "choice" {
            return Err(format!("answer has type `{kind}`, expected `choice`"));
        }
    }
    let raw_choice = answer.choice.as_deref().ok_or("answer has no `choice`")?;
    let choice = match raw_choice {
        "pass" => Choice::Pass,
        "fail" => Choice::Fail,
        "skip" if skip_allowed => Choice::Skip,
        "skip" => return Err("answered `skip`, which this rule does not allow".to_string()),
        other => return Err(format!("unknown choice `{other}`")),
    };
    let probability = |key: &str| {
        answer
            .probabilities
            .as_ref()
            .and_then(|p| p.get(key))
            .copied()
    };
    // An answer without any confidence is an error, never assumed: a silent
    // default would turn uncertain fails into blocking ones (or the reverse).
    let confidence = answer
        .confidence
        .or_else(|| probability(raw_choice))
        .ok_or("answer has neither `confidence` nor `probabilities` for its choice")?;
    let p_fail = probability("fail");
    for value in std::iter::once(confidence).chain(p_fail) {
        if !(0.0..=1.0).contains(&value) {
            return Err(format!("confidence {value} is outside 0..1"));
        }
    }
    Ok(Answer {
        choice,
        confidence,
        p_fail,
    })
}

// ------------------------------------------------------------------ client

/// What went wrong with one HTTP exchange.
#[derive(Debug, Clone, PartialEq)]
pub enum ApiError {
    /// The run cannot continue (bad key, no credits): stop sending.
    Fatal(String),
    /// This request failed; others may still succeed.
    Request(String),
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ApiError::Fatal(message) | ApiError::Request(message) => f.pad(message),
        }
    }
}

pub struct Client {
    agent: ureq::Agent,
    endpoint: String,
    key: String,
    key_variable: &'static str,
    /// OpenRouter shows this as the app name in its activity log.
    title: Option<&'static str>,
    backoff_base: Duration,
}

impl Client {
    pub fn new(
        endpoint: String,
        key: String,
        key_variable: &'static str,
        title: Option<&'static str>,
    ) -> Client {
        Client {
            agent: ureq::AgentBuilder::new().timeout(TIMEOUT).build(),
            endpoint,
            key,
            key_variable,
            title,
            backoff_base: BACKOFF_BASE,
        }
    }

    /// Shorter backoffs, for tests against a local mock server.
    pub fn with_backoff_base(mut self, base: Duration) -> Client {
        self.backoff_base = base;
        self
    }

    /// Sends one request and decodes the answers, retrying 408/429/5xx.
    pub fn evaluate(&self, request: &Request) -> Result<Decoded, ApiError> {
        let body = serde_json::to_string(request).expect("requests always serialize");
        let response = self.send(&body)?;
        decode(&response, request).map_err(ApiError::Request)
    }

    fn send(&self, body: &str) -> Result<String, ApiError> {
        let mut attempt = 0;
        loop {
            let mut call = self
                .agent
                .post(&self.endpoint)
                .set("Authorization", &format!("Bearer {}", self.key))
                .set("Content-Type", "application/json")
                .set("Accept", "application/json")
                .set("User-Agent", concat!("lintent/", env!("CARGO_PKG_VERSION")));
            if let Some(title) = self.title {
                call = call.set("X-Title", title);
            }
            if attempt > 0 {
                call = call.set("X-Retry-Count", &attempt.to_string());
            }
            let delay = match call.send_string(body) {
                Ok(response) => {
                    return response.into_string().map_err(|error| {
                        ApiError::Request(format!("reading the response failed: {error}"))
                    });
                }
                Err(ureq::Error::Status(status, response)) => {
                    let retry_after_ms = response.header("retry-after-ms").map(str::to_string);
                    let retry_after = response.header("retry-after").map(str::to_string);
                    let request_id = response
                        .header("x-typesafe-request-id")
                        .or_else(|| response.header("x-request-id"))
                        .map(str::to_string);
                    let text = response.into_string().unwrap_or_default();
                    if !retryable(status) || attempt >= MAX_RETRIES {
                        return Err(self.status_error(status, &text, request_id.as_deref()));
                    }
                    retry_delay(
                        attempt,
                        retry_after_ms.as_deref(),
                        retry_after.as_deref(),
                        self.backoff_base,
                    )
                }
                Err(ureq::Error::Transport(transport)) => {
                    // Only retry when the request certainly never reached the
                    // server. A timeout or reset after sending may already
                    // have been billed; asking again would pay twice.
                    let never_sent = matches!(
                        transport.kind(),
                        ureq::ErrorKind::Dns | ureq::ErrorKind::ConnectionFailed
                    );
                    if !never_sent || attempt >= MAX_RETRIES {
                        return Err(ApiError::Request(format!(
                            "request to {} failed: {transport}",
                            self.endpoint
                        )));
                    }
                    retry_delay(attempt, None, None, self.backoff_base)
                }
            };
            thread::sleep(delay);
            attempt += 1;
        }
    }

    fn status_error(&self, status: u16, body: &str, request_id: Option<&str>) -> ApiError {
        let detail = error_message(body);
        let suffix = request_id
            .map(|id| format!(" (request {id})"))
            .unwrap_or_default();
        let variable = self.key_variable;
        match status {
            401 => ApiError::Fatal(format!("HTTP 401: the key in {variable} was rejected: {detail}{suffix}")),
            402 => ApiError::Fatal(format!(
                "HTTP 402: out of credits on the account behind {variable}; top it up and re-run: {detail}{suffix}"
            )),
            403 => ApiError::Fatal(format!("HTTP 403: the key in {variable} may not use this model: {detail}{suffix}")),
            // The endpoint or model does not exist: every request would fail.
            404 => ApiError::Fatal(format!("HTTP 404: endpoint or model not found: {detail}{suffix}")),
            400 if detail.to_ascii_lowercase().contains("model") => {
                ApiError::Fatal(format!("HTTP 400: the model was rejected: {detail}{suffix}"))
            }
            413 => ApiError::Request(format!("HTTP 413: payload too large for the model: {detail}{suffix}")),
            _ => ApiError::Request(format!("HTTP {status}: {detail}{suffix}")),
        }
    }
}

fn retryable(status: u16) -> bool {
    status == 408 || status == 429 || status >= 500
}

/// `retry-after-ms`, then `retry-after` (seconds), else 500ms·2ⁿ capped at 5s.
pub fn retry_delay(
    attempt: u32,
    retry_after_ms: Option<&str>,
    retry_after: Option<&str>,
    base: Duration,
) -> Duration {
    let header = |raw: Option<&str>, unit: f64| {
        raw.and_then(|value| value.trim().parse::<f64>().ok())
            .filter(|value| value.is_finite() && *value >= 0.0)
            .map(|value| Duration::from_secs_f64(value * unit))
            .filter(|delay| *delay <= RETRY_AFTER_MAX)
    };
    header(retry_after_ms, 0.001)
        .or_else(|| header(retry_after, 1.0))
        .unwrap_or_else(|| (base * 2u32.saturating_pow(attempt)).min(BACKOFF_CAP))
}

/// `{"error": "…"}` or `{"error": {"message": "…"}}`, else the raw body.
fn error_message(body: &str) -> String {
    let parsed: Option<serde_json::Value> = serde_json::from_str(body).ok();
    let from_json = parsed.as_ref().and_then(|value| {
        let error = value.get("error")?;
        error
            .as_str()
            .or_else(|| error.get("message").and_then(|m| m.as_str()))
            .map(str::to_string)
    });
    let message = from_json.unwrap_or_else(|| body.trim().chars().take(300).collect());
    if message.is_empty() {
        "no details".to_string()
    } else {
        message
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::test_rule;
    use serde_json::json;

    fn unit(parent: Option<&str>) -> Unit {
        Unit {
            kind: ScopeKind::Method,
            name: "Foo.bar".into(),
            language: "typescript".into(),
            path: "src/x.ts".into(),
            start_line: 3,
            end_line: 5,
            source: "bar() { return 1; }".into(),
            parent_source: parent.map(str::to_string),
            truncated: false,
        }
    }

    #[test]
    fn golden_request_body_with_two_keyed_questions() {
        let first = test_rule("no-magic", "exceptions = [\"Zero is fine\", \"One too\"]");
        let second = test_rule("small-methods", "allow_skip = false");
        let request = build_request(
            "typesafe/jev-1.13",
            "run-1",
            &unit(Some("class Foo {}")),
            &[&first, &second],
        );
        assert_eq!(
            serde_json::to_value(&request).unwrap(),
            json!({
                "model": "typesafe/jev-1.13",
                "session_id": "run-1",
                "state": {
                    "kind": "method",
                    "name": "Foo.bar",
                    "language": "typescript",
                    "path": "src/x.ts",
                    "source": "bar() { return 1; }",
                    "parentSource": "class Foo {}"
                },
                "questions": {
                    "no-magic": {
                        "type": "choice",
                        "instructions": "Determine whether state.source violates this rule. Use state.parentSource only as surrounding context:\nRule no-magic.\n\nExplicit exceptions:\n- Zero is fine\n- One too",
                        "criteria": {"pass": CRITERION_PASS, "fail": CRITERION_FAIL, "skip": CRITERION_SKIP}
                    },
                    "small-methods": {
                        "type": "choice",
                        "instructions": "Determine whether state.source violates this rule. Use state.parentSource only as surrounding context:\nRule small-methods.",
                        "criteria": {"pass": CRITERION_PASS, "fail": CRITERION_FAIL}
                    }
                }
            })
        );
    }

    #[test]
    fn instructions_without_parent() {
        let rule = test_rule("demo", "");
        let text = instructions(&rule, &unit(None));
        assert_eq!(
            text,
            "Determine whether the supplied code complies with this rule:\nRule demo."
        );
        let request = build_request("m", "s", &unit(None), &[&rule]);
        assert!(serde_json::to_value(&request).unwrap()["state"]
            .get("parentSource")
            .is_none());
    }

    fn request_for(ids: &[(&str, &str)]) -> Request {
        let rules: Vec<Rule> = ids.iter().map(|(id, extra)| test_rule(id, extra)).collect();
        let refs: Vec<&Rule> = rules.iter().collect();
        build_request("m", "s", &unit(None), &refs)
    }

    #[test]
    fn decodes_answers_usage_and_probability_fallback() {
        let request = request_for(&[("a", ""), ("b", ""), ("c", "")]);
        let body = json!({
            "id": "gen-1",
            "model": "typesafe/jev-1.13-20260917",
            "answers": {
                "a": {"type": "choice", "choice": "fail", "confidence": 0.93},
                "b": {"type": "choice", "choice": "pass", "probabilities": {"pass": 0.7, "fail": 0.2, "skip": 0.1}},
                "c": {"type": "choice", "choice": "skip", "confidence": 0.99}
            },
            "usage": {"input_tokens": 120, "output_tokens": 3, "cost": 0.0000051}
        });
        let decoded = decode(&body.to_string(), &request).unwrap();
        assert_eq!(
            decoded.answers["a"],
            Ok(Answer {
                choice: Choice::Fail,
                confidence: 0.93,
                p_fail: None
            })
        );
        assert_eq!(
            decoded.answers["b"],
            Ok(Answer {
                choice: Choice::Pass,
                confidence: 0.7,
                p_fail: Some(0.2)
            })
        );
        assert_eq!(decoded.answers["c"].as_ref().unwrap().choice, Choice::Skip);
        assert_eq!(decoded.usage.input_tokens, 120);
        assert!((decoded.usage.cost - 0.0000051).abs() < 1e-12);
    }

    #[test]
    fn abstain_and_envelopes_are_not_part_of_the_api() {
        let request = request_for(&[("a", "")]);
        let body =
            json!({"answers": {"a": {"type": "choice", "choice": "abstain", "confidence": 0.5}}});
        let decoded = decode(&body.to_string(), &request).unwrap();
        assert!(decoded.answers["a"]
            .clone()
            .unwrap_err()
            .contains("unknown choice `abstain`"));
        let wrapped = json!({"result": {"answers": {"a": {"choice": "pass", "confidence": 0.5}}}});
        assert!(decode(&wrapped.to_string(), &request)
            .unwrap_err()
            .contains("no `answers`"));
    }

    #[test]
    fn per_question_errors() {
        let request = request_for(&[
            ("a", ""),
            ("b", ""),
            ("c", ""),
            ("d", "allow_skip = false"),
            ("e", ""),
            ("f", ""),
        ]);
        let body = json!({"answers": {
            "a": {"type": "choice", "choice": "fail"},
            "b": {"type": "choice", "choice": "maybe", "confidence": 0.5},
            "d": {"type": "choice", "choice": "skip", "confidence": 0.5},
            "e": {"type": "noul", "noul": 0.4},
            "f": {"type": "choice", "choice": "pass", "confidence": 1.5}
        }});
        let decoded = decode(&body.to_string(), &request).unwrap();
        let error = |id: &str| decoded.answers[id].clone().unwrap_err();
        assert!(error("a").contains("neither `confidence` nor `probabilities`"));
        assert!(error("b").contains("unknown choice `maybe`"));
        assert!(error("c").contains("no answer for rule `c`"));
        assert!(error("d").contains("does not allow"));
        assert!(error("e").contains("type `noul`"));
        assert!(error("f").contains("outside 0..1"));
    }

    #[test]
    fn whole_response_errors() {
        let request = request_for(&[("a", "")]);
        assert!(decode("not json", &request)
            .unwrap_err()
            .contains("not valid JSON"));
        assert!(decode("{}", &request).unwrap_err().contains("no `answers`"));
    }

    #[test]
    fn retry_delays() {
        let base = Duration::from_millis(500);
        assert_eq!(retry_delay(0, None, None, base), Duration::from_millis(500));
        assert_eq!(retry_delay(1, None, None, base), Duration::from_secs(1));
        assert_eq!(retry_delay(5, None, None, base), Duration::from_secs(5));
        assert_eq!(
            retry_delay(0, Some("250"), Some("9"), base),
            Duration::from_millis(250)
        );
        assert_eq!(
            retry_delay(0, None, Some("2"), base),
            Duration::from_secs(2)
        );
        assert_eq!(
            retry_delay(0, None, Some("3600"), base),
            Duration::from_millis(500)
        );
        assert_eq!(
            retry_delay(0, Some("junk"), None, base),
            Duration::from_millis(500)
        );
    }

    #[test]
    fn statuses_that_doom_the_whole_run_are_fatal() {
        let client = Client::new(
            "http://127.0.0.1:1/systemone".into(),
            "k".into(),
            "OPENROUTER_API_KEY",
            None,
        );
        let fatal = |status: u16, body: &str| {
            matches!(client.status_error(status, body, None), ApiError::Fatal(_))
        };
        assert!(fatal(401, ""));
        assert!(fatal(402, ""));
        assert!(fatal(403, ""));
        assert!(fatal(404, r#"{"error":{"message":"No endpoints found"}}"#));
        assert!(fatal(
            400,
            r#"{"error":"model typesafe/jev-9 is not a valid model ID"}"#
        ));
        assert!(!fatal(400, r#"{"error":"questions must not be empty"}"#));
        assert!(!fatal(413, "too big"));
        assert!(!fatal(500, ""));
    }

    #[test]
    fn connection_failures_are_retried_then_reported() {
        // Nothing listens on port 1, so the request never reaches a server:
        // the one transport failure that is safe to retry.
        let client = Client::new(
            "http://127.0.0.1:1/systemone".into(),
            "k".into(),
            "OPENROUTER_API_KEY",
            None,
        )
        .with_backoff_base(Duration::from_millis(1));
        let request = request_for(&[("a", "")]);
        let started = std::time::Instant::now();
        let error = client.evaluate(&request).unwrap_err();
        assert!(
            matches!(error, ApiError::Request(ref m) if m.contains("127.0.0.1:1")),
            "{error}"
        );
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn error_messages() {
        assert_eq!(error_message(r#"{"error":"bad"}"#), "bad");
        assert_eq!(
            error_message(r#"{"error":{"message":"no credits","code":402}}"#),
            "no credits"
        );
        assert_eq!(error_message("plain"), "plain");
        assert_eq!(error_message(""), "no details");
    }
}
