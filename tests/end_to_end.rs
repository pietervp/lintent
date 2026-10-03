//! End-to-end tests: the real binary against a local mock System One server.

use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};
use std::thread;

use serde_json::{json, Value};

// ------------------------------------------------------------- mock server

#[derive(Debug, Clone)]
struct Recorded {
    path: String,
    headers: HashMap<String, String>,
    body: Value,
}

struct Reply {
    status: u16,
    headers: Vec<(&'static str, String)>,
    body: String,
}

impl Reply {
    fn ok(body: Value) -> Reply {
        Reply {
            status: 200,
            headers: Vec::new(),
            body: body.to_string(),
        }
    }
}

type Responder = dyn Fn(usize, &Recorded) -> Reply + Send + Sync;

struct MockServer {
    url: String,
    requests: Arc<Mutex<Vec<Recorded>>>,
}

impl MockServer {
    /// Serves forever on a background thread; `respond` gets the 0-based
    /// request number and the request.
    fn start(respond: impl Fn(usize, &Recorded) -> Reply + Send + Sync + 'static) -> MockServer {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/api/v1", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&requests);
        let respond: Arc<Responder> = Arc::new(respond);
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut request_line = String::new();
                if reader.read_line(&mut request_line).unwrap_or(0) == 0 {
                    continue;
                }
                let path = request_line
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or("")
                    .to_string();
                let mut headers = HashMap::new();
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    let line = line.trim_end();
                    if line.is_empty() {
                        break;
                    }
                    if let Some((name, value)) = line.split_once(':') {
                        headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
                    }
                }
                let length: usize = headers
                    .get("content-length")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0);
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                let request = Recorded {
                    path,
                    headers,
                    body: serde_json::from_slice(&body).unwrap_or(Value::Null),
                };
                let number = {
                    let mut all = recorded.lock().unwrap();
                    all.push(request.clone());
                    all.len() - 1
                };
                let reply = respond(number, &request);
                let mut response = format!(
                    "HTTP/1.1 {} Mock\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
                    reply.status,
                    reply.body.len()
                );
                for (name, value) in &reply.headers {
                    response.push_str(&format!("{name}: {value}\r\n"));
                }
                response.push_str("\r\n");
                response.push_str(&reply.body);
                let _ = stream.write_all(response.as_bytes());
            }
        });
        MockServer { url, requests }
    }

    fn requests(&self) -> Vec<Recorded> {
        self.requests.lock().unwrap().clone()
    }
}

/// Answers every question with `answers[rule]`, defaulting to a confident pass.
fn answering(answers: Value) -> impl Fn(usize, &Recorded) -> Reply + Send + Sync + 'static {
    move |_, request| {
        let mut out = serde_json::Map::new();
        for key in request.body["questions"].as_object().unwrap().keys() {
            let answer = answers
                .get(key)
                .cloned()
                .unwrap_or_else(|| json!({"type": "choice", "choice": "pass", "confidence": 0.9}));
            out.insert(key.clone(), answer);
        }
        Reply::ok(json!({
            "id": "gen-1",
            "model": "typesafe/jev-1.13-20260917",
            "provider": "TypeSafe",
            "answers": out,
            "usage": {"input_tokens": 100, "output_tokens": 2, "cost": 0.000005}
        }))
    }
}

// ----------------------------------------------------------------- project

struct Project {
    dir: tempfile::TempDir,
    /// Compiled grammars and clones live outside the project.
    grammar_cache: tempfile::TempDir,
    /// Passed as LINTENT_BASE_URL: a committed config may not redirect the key.
    base_url: Option<String>,
}

impl Project {
    fn new(config: &str) -> Project {
        let project = Project::empty();
        fs::write(project.root().join("lintent.toml"), config).unwrap();
        fs::create_dir_all(project.root().join(".lintent/rules")).unwrap();
        project
    }

    fn empty() -> Project {
        Project {
            dir: tempfile::tempdir().unwrap(),
            grammar_cache: tempfile::tempdir().unwrap(),
            base_url: None,
        }
    }

    fn with_server(server: &MockServer, extra: &str) -> Project {
        let mut project = Project::new(&format!("concurrency = 1\n{extra}"));
        project.base_url = Some(server.url.clone());
        project
    }

    fn root(&self) -> &Path {
        self.dir.path()
    }

    fn write(&self, path: &str, text: &str) -> &Project {
        let full = self.root().join(path);
        fs::create_dir_all(full.parent().unwrap()).unwrap();
        fs::write(full, text).unwrap();
        self
    }

    fn rule(&self, id: &str, extra: &str) -> &Project {
        self.write(
            &format!(".lintent/rules/{id}.toml"),
            &format!(
                "id = \"{id}\"\ndescription = \"Rule {id}.\"\nwhy = \"Because {id}.\"\nfix = \"Fix {id}.\"\ninclude = [\"src/**\"]\n{}{extra}",
                if extra.contains("scopes") { "" } else { "scopes = [\"function\", \"method\"]\n" }
            ),
        )
    }

    fn run(&self, args: &[&str]) -> Output {
        self.run_with(args, &[])
    }

    fn run_with(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_lintent"));
        command.args(args).current_dir(self.root());
        for (key, _) in std::env::vars() {
            if key.starts_with("GIT_") || key.starts_with("LINTENT_") || key.ends_with("_API_KEY") {
                command.env_remove(key);
            }
        }
        command.env("OPENROUTER_API_KEY", "test-key-openrouter");
        command.env("LINTENT_CACHE_DIR", self.grammar_cache.path());
        if let Some(url) = &self.base_url {
            command.env("LINTENT_BASE_URL", url);
        }
        for (key, value) in env {
            command.env(key, value);
        }
        command.output().unwrap()
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn json_out(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|_| panic!("not JSON:\n{}\n{}", stdout(output), stderr(output)))
}

const TS: &str = "export class Billing {\n  total(x: number) {\n    return x * 2;\n  }\n}\n\nexport const helper = (n: number) => n + 1;\n";

fn question_keys(request: &Recorded) -> Vec<String> {
    request.body["questions"]
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect()
}

// ------------------------------------------------------------------- tests

#[test]
fn two_rules_on_one_scope_are_two_keyed_questions_in_one_request() {
    let server = MockServer::start(answering(json!({
        "rule-a": {"type": "choice", "choice": "fail", "confidence": 0.95, "probabilities": {"pass": 0.05, "fail": 0.95}}
    })));
    let project = Project::with_server(&server, "");
    project
        .rule("rule-a", "scopes = [\"method\"]")
        .rule("rule-b", "scopes = [\"method\"]")
        .write("src/billing.ts", TS);

    let output = project.run(&["check", "--json"]);
    assert_eq!(
        output.status.code(),
        Some(1),
        "{}{}",
        stdout(&output),
        stderr(&output)
    );

    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.path, "/api/v1/systemone");
    assert_eq!(question_keys(request), vec!["rule-a", "rule-b"]);
    assert_eq!(
        request.headers["authorization"],
        "Bearer test-key-openrouter"
    );
    assert!(request.headers["user-agent"].starts_with("lintent/"));
    assert_eq!(request.headers["x-title"], "lintent");
    assert_eq!(request.body["model"], "typesafe/jev-1.13");
    assert!(request.body["session_id"]
        .as_str()
        .unwrap()
        .starts_with("lintent-"));
    assert_eq!(request.body["state"]["kind"], "method");
    assert_eq!(request.body["state"]["name"], "Billing.total");
    assert!(request.body["state"]["parentSource"]
        .as_str()
        .unwrap()
        .starts_with("export class Billing"));

    let report = json_out(&output);
    let findings = report["findings"].as_array().unwrap();
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0]["rule"], "rule-a");
    assert_eq!(findings[0]["status"], "fail");
    assert_eq!(findings[0]["line"], 2);
    assert_eq!(findings[0]["p_fail"], 0.95);
    assert_eq!(findings[0]["why"], "Because rule-a.");
    assert_eq!(report["stats"]["requests"], 1);
    assert_eq!(report["stats"]["questions"], 2);
    assert_eq!(report["stats"]["input_tokens"], 100);
}

#[test]
fn isolate_rules_sends_one_request_per_rule_in_one_session() {
    let server = MockServer::start(answering(json!({})));
    let project = Project::with_server(&server, "isolate_rules = true");
    project
        .rule("rule-a", "scopes = [\"method\"]")
        .rule("rule-b", "scopes = [\"method\"]")
        .write("src/billing.ts", TS);

    let output = project.run(&["check"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}{}",
        stdout(&output),
        stderr(&output)
    );
    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    let mut keys: Vec<Vec<String>> = requests.iter().map(question_keys).collect();
    keys.sort();
    assert_eq!(
        keys,
        vec![vec!["rule-a".to_string()], vec!["rule-b".to_string()]]
    );
    assert_eq!(
        requests[0].body["session_id"],
        requests[1].body["session_id"]
    );
    assert!(
        stderr(&output).contains("cost $0.00001 "),
        "{}",
        stdout(&output)
    );
}

#[test]
fn human_output_and_uncertain_fails() {
    let server = MockServer::start(answering(json!({
        "rule-a": {"type": "choice", "choice": "fail", "confidence": 0.93},
        "rule-b": {"type": "choice", "choice": "fail", "confidence": 0.4}
    })));
    let project = Project::with_server(&server, "");
    project
        .rule("rule-a", "")
        .rule("rule-b", "")
        .write("src/billing.ts", TS);

    let output = project.run(&["check"]);
    let text = stdout(&output);
    assert_eq!(output.status.code(), Some(1), "{text}");
    assert!(text.contains("src/billing.ts:2  error  rule-a  method Billing.total  (confidence 0.93)\n    why: Because rule-a.\n    fix: Fix rule-a.\n"), "{text}");
    assert!(text.contains("Uncertain (below min_confidence"));
    assert!(
        text.contains("src/billing.ts:7  error  rule-b  function helper  (confidence 0.40 < 0.80)"),
        "{text}"
    );
    assert!(
        stderr(&output).contains("2 error(s), 0 warning(s), 2 uncertain"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn verdicts_are_cached_per_question() {
    let server = MockServer::start(answering(json!({})));
    let project = Project::with_server(&server, "");
    project.rule("rule-a", "").write("src/billing.ts", TS);

    assert_eq!(project.run(&["check"]).status.code(), Some(0));
    assert_eq!(server.requests().len(), 2, "one per scope");

    let second = json_out(&project.run(&["check", "--json"]));
    assert_eq!(second["stats"]["cache_hits"], 2);
    assert_eq!(second["stats"]["requests"], 0);
    assert_eq!(server.requests().len(), 2);

    // A second rule only asks the new question.
    project.rule("rule-b", "");
    let third = json_out(&project.run(&["check", "--json"]));
    assert_eq!(third["stats"]["cache_hits"], 2);
    assert_eq!(server.requests().len(), 4);
    assert!(server.requests()[2..]
        .iter()
        .all(|r| question_keys(r) == vec!["rule-b"]));

    // What the model never sees is applied to cached answers for free.
    project.rule("rule-a", "severity = \"warning\"\nmin_confidence = 0.95\n");
    let retuned = json_out(&project.run(&["check", "--json"]));
    assert_eq!(retuned["stats"]["cache_hits"], 4);
    assert_eq!(server.requests().len(), 4);
    // Rewording the rule changes the question, so it is asked again.
    project.rule("rule-a", "exceptions = [\"Helpers are fine\"]\n");
    let reworded = json_out(&project.run(&["check", "--json"]));
    assert_eq!(reworded["stats"]["cache_hits"], 2);
    assert_eq!(server.requests().len(), 6);

    project.run(&["check", "--refresh"]);
    assert_eq!(server.requests().len(), 8);
    project.run(&["check", "--no-cache"]);
    assert_eq!(server.requests().len(), 10);
}

#[test]
fn fail_on_warning_blocks_on_confident_warnings_only() {
    let server = MockServer::start(answering(json!({
        "rule-a": {"type": "choice", "choice": "fail", "confidence": 0.93},
        "rule-b": {"type": "choice", "choice": "fail", "confidence": 0.4}
    })));
    let project = Project::with_server(&server, "");
    project
        .rule("rule-a", "severity = \"warning\"\n")
        .rule("rule-b", "")
        .write("src/billing.ts", TS);

    assert_eq!(project.run(&["check"]).status.code(), Some(0));
    let strict = project.run(&["check", "--fail-on", "warning"]);
    assert_eq!(strict.status.code(), Some(1), "{}", stdout(&strict));
    // rule-b is an uncertain error: it never blocks, whatever --fail-on says.
    let only_uncertain = project.run(&["check", "--rule", "rule-b", "--fail-on", "warning"]);
    assert_eq!(only_uncertain.status.code(), Some(0));
}

#[test]
fn confidence_falls_back_to_probabilities_and_is_never_assumed() {
    let server = MockServer::start(answering(json!({
        "rule-a": {"type": "choice", "choice": "fail", "probabilities": {"pass": 0.1, "fail": 0.9}},
        "rule-b": {"type": "choice", "choice": "fail"}
    })));
    let project = Project::with_server(&server, "");
    project
        .rule("rule-a", "scopes = [\"function\"]")
        .rule("rule-b", "scopes = [\"function\"]")
        .write("src/billing.ts", TS);

    let output = project.run(&["check", "--json"]);
    assert_eq!(output.status.code(), Some(2));
    let report = json_out(&output);
    assert_eq!(report["findings"][0]["rule"], "rule-a");
    assert_eq!(report["findings"][0]["confidence"], 0.9);
    let error = &report["errors"][0];
    assert_eq!(error["rule"], "rule-b");
    assert!(error["message"]
        .as_str()
        .unwrap()
        .contains("neither `confidence` nor `probabilities`"));
}

#[test]
fn http_402_stops_the_run_with_a_clear_message() {
    let server = MockServer::start(|_, _| Reply {
        status: 402,
        headers: Vec::new(),
        body: json!({"error": {"code": 402, "message": "Insufficient credits"}}).to_string(),
    });
    let project = Project::with_server(&server, "");
    project.rule("rule-a", "").write("src/billing.ts", TS);

    let output = project.run(&["check"]);
    assert_eq!(output.status.code(), Some(2));
    let text = stdout(&output);
    assert!(text.contains("out of credits"), "{text}");
    assert!(text.contains("OPENROUTER_API_KEY"));
    assert!(text.contains("Insufficient credits"));
    assert_eq!(
        server.requests().len(),
        1,
        "no further requests after a fatal error"
    );
}

#[test]
fn http_529_is_retried() {
    let answer = answering(json!({}));
    let server = MockServer::start(move |number, request| {
        if number == 0 {
            Reply {
                status: 529,
                headers: vec![("retry-after-ms", "1".to_string())],
                body: json!({"error": "overloaded"}).to_string(),
            }
        } else {
            answer(number, request)
        }
    });
    let project = Project::with_server(&server, "");
    project
        .rule("rule-a", "scopes = [\"function\"]")
        .write("src/billing.ts", TS);

    let output = project.run(&["check"]);
    assert_eq!(output.status.code(), Some(0), "{}", stdout(&output));
    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[1].headers.get("x-retry-count").map(String::as_str),
        Some("1")
    );
    assert!(stderr(&output).contains("1 request(s)"));
}

#[test]
fn http_413_names_the_unit() {
    let server = MockServer::start(|_, _| Reply {
        status: 413,
        headers: Vec::new(),
        body: "too big".to_string(),
    });
    let project = Project::with_server(&server, "");
    project
        .rule("rule-a", "scopes = [\"function\"]")
        .write("src/billing.ts", TS);
    let output = project.run(&["check", "--json"]);
    assert_eq!(output.status.code(), Some(2));
    let report = json_out(&output);
    let message = report["errors"][0]["message"].as_str().unwrap();
    assert!(
        message.contains("function helper") && message.contains("413"),
        "{message}"
    );
    assert_eq!(report["errors"][0]["path"], "src/billing.ts");
}

#[test]
fn dry_run_needs_no_key_and_sends_nothing() {
    let server = MockServer::start(answering(json!({})));
    let project = Project::with_server(&server, "");
    project
        .rule("rule-a", "scopes = [\"method\"]")
        .rule("rule-b", "scopes = [\"method\"]")
        .write("src/billing.ts", TS);

    let output = project.run_with(&["check", "--dry-run"], &[("OPENROUTER_API_KEY", "")]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("questions: rule-a, rule-b"), "{text}");
    assert!(text.contains("\"rule-a\": {") && text.contains("\"rule-b\": {"));
    assert!(stderr(&output).contains("dry run: 1 request(s) for 2 question(s)"));
    assert!(
        stderr(&output).contains("within budget of $0.001"),
        "{}",
        stderr(&output)
    );
    assert!(server.requests().is_empty());

    let repeated = project.run_with(
        &["check", "--dry-run", "--rule", "rule-a", "--rule", "rule-a"],
        &[("OPENROUTER_API_KEY", "")],
    );
    assert!(
        stdout(&repeated).contains("questions: rule-a\n"),
        "a repeated --rule is asked once"
    );

    let json = json_out(&project.run(&["check", "--dry-run", "--json"]));
    assert_eq!(json["requests"].as_array().unwrap().len(), 1);
    assert_eq!(json["endpoint"], format!("{}/systemone", server.url));

    let missing = project.run_with(&["check"], &[("OPENROUTER_API_KEY", "")]);
    assert_eq!(missing.status.code(), Some(2));
    assert!(stderr(&missing).contains("OPENROUTER_API_KEY is not set"));
}

#[test]
fn keep_marks_suppress_and_bad_marks_are_errors() {
    let server = MockServer::start(answering(json!({})));
    let project = Project::with_server(&server, "");
    project
        .rule("rule-a", "scopes = [\"function\"]")
        .rule("rule-b", "scopes = [\"function\"]");
    project.write(
        "src/a.ts",
        "// lintent-keep rule-a -- generated by the codegen\nexport function kept() {}\n\n// lintent-keep rule-b\nexport function noReason() {}\n",
    );

    let output = project.run(&["check", "--json"]);
    let report = json_out(&output);
    assert_eq!(output.status.code(), Some(1));
    let asked: Vec<(String, Vec<String>)> = server
        .requests()
        .iter()
        .map(|r| {
            (
                r.body["state"]["name"].as_str().unwrap().to_string(),
                question_keys(r),
            )
        })
        .collect();
    assert_eq!(
        asked,
        vec![
            ("kept".to_string(), vec!["rule-b".to_string()]),
            (
                "noReason".to_string(),
                vec!["rule-a".to_string(), "rule-b".to_string()]
            ),
        ]
    );
    let finding = &report["findings"][0];
    assert_eq!(finding["rule"], "lintent/keep");
    assert_eq!(finding["line"], 4);
    assert!(finding["message"]
        .as_str()
        .unwrap()
        .contains("has no reason"));
}

#[test]
fn typesafe_provider_uses_its_own_key_and_no_title() {
    let server = MockServer::start(answering(json!({})));
    let project = Project::with_server(&server, "provider = \"typesafe\"");
    project
        .rule("rule-a", "scopes = [\"function\"]")
        .write("src/billing.ts", TS);
    let output = project.run_with(&["check"], &[("TYPESAFE_API_KEY", "test-key-typesafe")]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let request = &server.requests()[0];
    assert_eq!(request.headers["authorization"], "Bearer test-key-typesafe");
    assert!(!request.headers.contains_key("x-title"));
    assert_eq!(request.body["model"], "jev-latest");
}

#[test]
fn eval_judges_fixtures() {
    // Fails exactly the scopes whose source mentions BAD.
    let server = MockServer::start(|_, request| {
        let bad = request.body["state"]["source"]
            .as_str()
            .unwrap()
            .contains("BAD");
        let choice = if bad { "fail" } else { "pass" };
        let mut answers = serde_json::Map::new();
        for key in request.body["questions"].as_object().unwrap().keys() {
            answers.insert(
                key.clone(),
                json!({"type": "choice", "choice": choice, "confidence": 0.97}),
            );
        }
        Reply::ok(json!({"answers": answers}))
    });
    let project = Project::with_server(&server, "");
    project
        .rule("good-rule", "")
        .write(
            ".lintent/fixtures/good-rule/pass/ok.ts",
            "export function ok() { return 1; }\n",
        )
        .write(
            ".lintent/fixtures/good-rule/fail/bad.ts",
            "export function bad() { return 'BAD'; }\n",
        )
        .rule("unproven-rule", "")
        .write(
            ".lintent/fixtures/unproven-rule/pass/ok.ts",
            "export function ok() { return 1; }\n",
        );

    let proven = project.run(&["eval", "--rule", "good-rule"]);
    assert_eq!(proven.status.code(), Some(0), "{}", stdout(&proven));
    assert!(stdout(&proven).contains("fail (0.97)"));

    let all = project.run(&["eval"]);
    assert_eq!(all.status.code(), Some(1));
    assert!(stdout(&all).contains("unproven"));
}

#[test]
fn init_rule_new_rules_and_scopes() {
    let project = Project::empty();
    fs::write(project.root().join(".gitignore"), "target/\n").unwrap();
    let init = project.run(&["init"]);
    assert_eq!(init.status.code(), Some(0));
    assert!(fs::read_to_string(project.root().join(".gitignore"))
        .unwrap()
        .contains(".lintent/cache.json"));
    let new = project.run(&[
        "rule",
        "new",
        "demo",
        "--include",
        "src/**",
        "--scopes",
        "method",
    ]);
    assert_eq!(new.status.code(), Some(0), "{}", stderr(&new));
    project.write("src/billing.ts", TS).write("notes.txt", "hi");

    let rules = project.run(&["rules"]);
    assert!(stdout(&rules).starts_with("demo"), "{}", stdout(&rules));
    let rules_json: Value =
        serde_json::from_slice(&project.run(&["rules", "--json"]).stdout).unwrap();
    assert_eq!(rules_json[0]["scopes"], json!(["method"]));
    assert_eq!(
        rules_json[0]["min_confidence"],
        json!(0.8),
        "the project default"
    );
    assert!(
        stdout(&rules).contains(" error    0.80  method"),
        "{}",
        stdout(&rules)
    );

    let all = stdout(&project.run(&["scopes", "--verbose"]));
    assert!(all.contains("src/billing.ts:1  class    Billing"), "{all}");
    assert!(all.contains("src/billing.ts:7  function helper"));
    assert!(all.contains("notes.txt  skipped"));
    let reach = project.run(&["scopes", "--rule", "demo"]);
    assert_eq!(stdout(&reach), "src/billing.ts:2  method   Billing.total\n");
    assert!(stderr(&reach).contains("1 scope(s) in 1 file(s) matched by demo"));

    let unknown = project.run(&["scopes", "--rule", "nope"]);
    assert_eq!(unknown.status.code(), Some(2));
}

// --------------------------------------------------------- runtime grammars

fn toy_grammar() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/toy-grammar")
}

const TOY: &str =
    "class Cart {\n  fn total() { x; }\n}\n\n// lintent-keep demo -- toy keep\nfn main() { y; }\n";

/// Runtime grammars run native code, so every toy run opts in.
fn trusted(project: &Project, args: &[&str]) -> Output {
    let mut all = vec!["--trust-grammars"];
    all.extend_from_slice(args);
    project.run(&all)
}

fn toy_project(language_table: &str) -> Project {
    let project = Project::new(&format!(
        "[languages.toy]\nextensions = [\"toy\"]\n{language_table}"
    ));
    project.write("src/cart.toy", TOY);
    project.rule("demo", "scopes = [\"class\", \"method\", \"function\"]");
    project
}

fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).unwrap();
        }
    }
}

#[test]
fn runtime_grammar_directory_uses_the_heuristic_then_a_tags_query() {
    let project = toy_project("grammar = \"grammars/toy\"\n");
    copy_dir(&toy_grammar(), &project.root().join("grammars/toy"));

    let languages = stdout(&project.run(&["languages"]));
    assert!(languages.contains("toy"), "{languages}");
    assert!(languages.contains("untrusted"));

    // Without trust the grammar's native code is never compiled or loaded.
    let refused = project.run(&["scopes", "--rule", "demo"]);
    assert!(
        stdout(&refused).contains("--trust-grammars"),
        "{}",
        stdout(&refused)
    );
    assert!(fs::read_dir(project.grammar_cache.path())
        .unwrap()
        .next()
        .is_none());

    let output = trusted(&project, &["scopes", "--rule", "demo"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(
        stdout(&output),
        "src/cart.toy:1  class    Cart\nsrc/cart.toy:2  method   Cart.total\nsrc/cart.toy:6  function main  [kept: toy keep]\n"
    );

    // A tags query in the grammar's conventional place takes over.
    project.write(
        "grammars/toy/queries/tags.scm",
        "(function_definition name: (identifier) @name) @definition.function\n",
    );
    let tagged = stdout(&trusted(&project, &["scopes", "src"]));
    assert!(
        tagged.contains("src/cart.toy:2  method   Cart.total"),
        "{tagged}"
    );
    assert!(
        !tagged.contains("class    Cart"),
        "only functions are tagged now:\n{tagged}"
    );
}

#[test]
fn runtime_grammar_from_a_git_repo_and_a_compiled_library() {
    let repo = tempfile::tempdir().unwrap();
    copy_dir(&toy_grammar(), repo.path());
    let git = |args: &[&str]| {
        let mut command = Command::new("git");
        command.args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "commit.gpgsign=false",
        ]);
        command.args(args).current_dir(repo.path());
        for (key, _) in std::env::vars() {
            if key.starts_with("GIT_") {
                command.env_remove(key);
            }
        }
        assert!(command.status().unwrap().success());
    };
    git(&["init", "--quiet"]);
    git(&["add", "."]);
    git(&["commit", "--quiet", "-m", "toy"]);
    let sha = String::from_utf8(
        Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(repo.path())
            .env_remove("GIT_DIR")
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .to_string();

    let url = format!("file://{}", repo.path().display());
    let tag_only = toy_project(&format!("repo = \"{url}\"\nrev = \"v1\"\n"));
    let rejected = tag_only.run(&["--trust-grammars", "scopes"]);
    assert_eq!(rejected.status.code(), Some(2));
    assert!(stderr(&rejected).contains("40-character commit SHA"));

    let project = toy_project(&format!("repo = \"{url}\"\nrev = \"{sha}\"\n"));
    let output = trusted(&project, &["scopes", "src"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(
        stdout(&output).contains("src/cart.toy:2  method   Cart.total"),
        "{}",
        stdout(&output)
    );

    // The compiled library is cached; copy it into a second project and load it directly.
    let library = find_file(
        &project.grammar_cache.path().join("grammars"),
        std::env::consts::DLL_EXTENSION,
    )
    .expect("a compiled grammar library in the cache");
    let file_name = format!("toy.{}", std::env::consts::DLL_EXTENSION);
    let direct = toy_project(&format!("grammar = \"grammars/{file_name}\"\n"));
    fs::create_dir_all(direct.root().join("grammars")).unwrap();
    fs::copy(&library, direct.root().join("grammars").join(&file_name)).unwrap();
    let output = trusted(&direct, &["scopes", "--rule", "demo"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(stderr(&output).contains("3 scope(s) in 1 file(s)"));
}

fn find_file(dir: &Path, extension: &str) -> Option<PathBuf> {
    for entry in fs::read_dir(dir).ok()? {
        let path = entry.ok()?.path();
        if path.is_dir() {
            if let Some(found) = find_file(&path, extension) {
                return Some(found);
            }
        } else if path.extension().and_then(|e| e.to_str()) == Some(extension) {
            return Some(path);
        }
    }
    None
}

// ------------------------------------------------------------------ budget

/// A file with `count` small functions, one scope each.
fn many_functions(count: usize) -> String {
    (0..count)
        .map(|i| format!("export function f{i}(a: number) {{\n  return a + {i};\n}}\n\n"))
        .collect()
}

#[test]
fn over_budget_sends_nothing_and_tells_the_agent_how_to_narrow() {
    let server = MockServer::start(answering(json!({})));
    let project = Project::with_server(&server, "[budget]\nmax_cost_usd = 0.0001\n");
    project
        .rule("rule-a", "scopes = [\"function\"]")
        .write("src/a/many.ts", &many_functions(12));

    let output = project.run(&["check"]);
    assert_eq!(output.status.code(), Some(3), "{}", stderr(&output));
    assert!(
        server.requests().is_empty(),
        "nothing may be sent over budget"
    );
    let text = stderr(&output);
    assert!(
        text.starts_with("lintent: this run would cost ~$"),
        "{text}"
    );
    assert!(text.contains(
        "over the $0.0001 budget. Nothing was sent. Narrow the scope before re-running:"
    ));
    assert!(text.contains("By rule:\n  rule-a"));
    assert!(text.contains("By file:\n  src/a/many.ts"));
    assert!(text.contains("By directory:\n  src/a"));
    assert!(text.contains(".lintent/rules/rule-a.toml"));
    assert!(text.contains("lintent scopes --rule rule-a"));
    assert!(text.contains("--changed"));

    // JSON carries the same estimate.
    let json = json_out(&project.run(&["check", "--json"]));
    let budget = &json["budget"];
    assert_eq!(budget["over_budget"], true);
    assert_eq!(budget["max_cost_usd"], 0.0001);
    assert!(budget["estimated_cost_usd"].as_f64().unwrap() > 0.0001);
    assert!(budget["estimated_input_tokens"].as_u64().unwrap() > 2400);
    assert_eq!(budget["by_rule"][0]["name"], "rule-a");
    assert_eq!(budget["by_rule"][0]["questions"], 12);
    assert_eq!(budget["by_path"][0]["name"], "src/a/many.ts");
    assert!(server.requests().is_empty());

    // Dry run is a budget check too.
    let dry = project.run(&["check", "--dry-run"]);
    assert_eq!(dry.status.code(), Some(3));
    assert!(stderr(&dry).contains("OVER budget"));

    // Raising the budget for one run lets it through, and reports the actual cost.
    let allowed = project.run(&["check", "--max-cost", "0.01"]);
    assert_eq!(allowed.status.code(), Some(0), "{}", stderr(&allowed));
    assert_eq!(server.requests().len(), 12);
    assert!(
        stderr(&allowed).contains("within budget of $0.01 · actual $0.00006"),
        "{}",
        stderr(&allowed)
    );
}

#[test]
fn cached_questions_cost_nothing_against_the_budget() {
    let server = MockServer::start(answering(json!({})));
    let project = Project::with_server(&server, "[budget]\nmax_cost_usd = 0.0001\n");
    project
        .rule("rule-a", "scopes = [\"function\"]")
        .write("src/many.ts", &many_functions(12));
    assert_eq!(
        project.run(&["check", "--max-cost", "1"]).status.code(),
        Some(0)
    );
    assert_eq!(server.requests().len(), 12);

    // Everything is cached now: the budget is no obstacle.
    let output = project.run(&["check", "--json"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let json = json_out(&output);
    assert_eq!(json["budget"]["estimated_cost_usd"], 0.0);
    assert_eq!(json["budget"]["over_budget"], false);
    assert_eq!(json["stats"]["cache_hits"], 12);
    assert_eq!(server.requests().len(), 12);
}

#[test]
fn budget_is_configurable_in_the_config() {
    let server = MockServer::start(answering(json!({})));
    let project = Project::with_server(&server, "[budget]\nmax_cost_usd = 0.00001\n");
    project
        .rule("rule-a", "scopes = [\"function\"]")
        .write("src/a.ts", &many_functions(1));
    let output = project.run(&["check"]);
    assert_eq!(output.status.code(), Some(3));
    assert!(stderr(&output).contains("over the $0.00001 budget"));
    assert!(
        stderr(&output).contains("alone costs"),
        "one scope alone is over budget"
    );
    assert!(server.requests().is_empty());
}

#[test]
fn eval_is_budgeted_too() {
    let server = MockServer::start(answering(json!({})));
    let project = Project::with_server(&server, "[budget]\nmax_cost_usd = 0.0001\n");
    project
        .rule("rule-a", "scopes = [\"function\"]")
        .write(".lintent/fixtures/rule-a/fail/many.ts", &many_functions(12));
    let output = project.run(&["eval"]);
    assert_eq!(output.status.code(), Some(3));
    assert!(stderr(&output).contains("lintent eval --rule"));
    assert!(server.requests().is_empty());
    let dry = project.run(&["eval", "--dry-run"]);
    assert_eq!(dry.status.code(), Some(3));
}

// ------------------------------------------------------ smoke-test findings

#[test]
fn dry_run_exit_codes_match_check() {
    let project = Project::new("");
    project.rule("rule-a", "scopes = [\"function\"]").write(
        "src/a.ts",
        "// lintent-keep rule-a\nexport function f() {}\n",
    );
    let dry = project.run(&["check", "--dry-run"]);
    assert_eq!(
        dry.status.code(),
        Some(1),
        "an invalid keep mark fails a dry run too"
    );
    assert!(stdout(&dry).contains("lintent/keep"));

    let eval = Project::new("");
    eval.rule("rule-a", "scopes = [\"function\"]").write(
        ".lintent/fixtures/rule-a/pass/ok.ts",
        "export function ok() {}\n",
    );
    assert_eq!(
        eval.run(&["eval", "--dry-run"]).status.code(),
        Some(1),
        "unproven rule"
    );
    eval.write(
        ".lintent/fixtures/rule-a/fail/bad.ts",
        "export function bad() {}\n",
    );
    assert_eq!(eval.run(&["eval", "--dry-run"]).status.code(), Some(0));
}

#[test]
fn piping_into_head_exits_quietly() {
    let project = Project::new("");
    project.write("src/many.ts", &many_functions(3000));
    let output = Command::new("sh")
        .arg("-c")
        .arg(format!(
            "'{}' scopes | head -1",
            env!("CARGO_BIN_EXE_lintent")
        ))
        .current_dir(project.root())
        .output()
        .unwrap();
    assert_eq!(stdout(&output), "src/many.ts:1  function f0\n");
    assert!(!stderr(&output).contains("panicked"), "{}", stderr(&output));
    assert!(
        !stderr(&output).contains("Broken pipe"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn explicit_paths_inside_excludes_get_a_note() {
    let project = Project::new("");
    project
        .rule("rule-a", "")
        .write("src/dist/out.ts", "export function f() {}\n");
    let output = project.run(&["scopes", "src/dist"]);
    assert_eq!(stdout(&output), "");
    assert!(
        stderr(&output).contains("1 file(s) under the given paths are excluded by `exclude`"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn lintentignore_files_hide_paths_like_gitignore() {
    let project = Project::new("");
    project
        .rule("rule-a", "")
        .write(".lintentignore", "generated/\n*.snap.ts\n")
        .write("src/.lintentignore", "legacy.ts\n")
        .write("generated/api.ts", "export function g() {}\n")
        .write("src/view.snap.ts", "export function s() {}\n")
        .write("src/legacy.ts", "export function l() {}\n")
        .write("src/kept.ts", "export function k() {}\n");
    let all = stdout(&project.run(&["scopes"]));
    assert_eq!(all, "src/kept.ts:1  function k\n");
    assert!(!stderr(&project.run(&["scopes"])).contains("note:"));
    let explicit = project.run(&["scopes", "src"]);
    assert_eq!(stdout(&explicit), "src/kept.ts:1  function k\n");
    assert!(
        stderr(&explicit).contains("2 file(s) under the given paths are ignored by .lintentignore"),
        "{}",
        stderr(&explicit)
    );
}

#[test]
fn eval_flags_vacuous_pass_fixtures() {
    let server = MockServer::start(answering(json!({
        "rule-a": {"type": "choice", "choice": "fail", "confidence": 0.99}
    })));
    let project = Project::with_server(&server, "");
    project
        .rule("rule-a", "scopes = [\"class\"]")
        .write(
            ".lintent/fixtures/rule-a/pass/no-class.ts",
            "export function f() {}\n",
        )
        .write(
            ".lintent/fixtures/rule-a/fail/bad.ts",
            "export class Bad {}\n",
        );
    let output = project.run(&["eval"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(stdout(&output).contains("vacuous"), "{}", stdout(&output));
}

#[test]
fn key_from_dotenv_local_at_the_project_root() {
    let server = MockServer::start(answering(json!({})));
    let project = Project::with_server(&server, "");
    project
        .rule("rule-a", "scopes = [\"function\"]")
        .write("src/a.ts", "export function f() {}\n")
        .write(".env", "OPENROUTER_API_KEY=from-dotenv\n")
        .write(".env.local", "OPENROUTER_API_KEY=from-dotenv-local\n");
    fs::create_dir_all(project.root().join("src/deep")).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_lintent"));
    command
        .args(["check", "--no-cache"])
        .current_dir(project.root().join("src/deep"))
        .env_remove("OPENROUTER_API_KEY")
        .env("LINTENT_BASE_URL", &server.url);
    let output = command.output().unwrap();
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(
        server.requests()[0].headers["authorization"],
        "Bearer from-dotenv-local"
    );
    assert!(
        !stderr(&output).contains("from-dotenv"),
        "values are never printed"
    );
}

// ------------------------------------------------------------ review fixes

#[test]
fn a_committed_config_cannot_redirect_the_key() {
    let server = MockServer::start(answering(json!({})));
    let project = Project::new(&format!("base_url = \"{}\"\n", server.url));
    project
        .rule("rule-a", "scopes = [\"function\"]")
        .write("src/a.ts", "export function f() {}\n");
    let output = project.run(&["check"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(
        stderr(&output).contains("LINTENT_BASE_URL"),
        "{}",
        stderr(&output)
    );
    assert!(server.requests().is_empty());
}

#[test]
fn binary_and_non_utf8_files_are_skipped_not_errors() {
    let server = MockServer::start(answering(json!({})));
    let project = Project::with_server(&server, "");
    project
        .rule("rule-a", "scopes = [\"function\"]")
        .write("src/ok.ts", "export function f() {}\n");
    fs::write(
        project.root().join("src/video.ts"),
        [0x47u8, 0x00, 0xff, 0x10],
    )
    .unwrap();
    fs::write(
        project.root().join("src/latin1.py"),
        b"def f():\n    return '\xe9'\n",
    )
    .unwrap();
    let output = project.run(&["check", "--json"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(json_out(&output)["stats"]["skipped_files"], 2);
    assert!(stderr(&output).contains("skipping src/video.ts"));
}

#[test]
fn hidden_files_are_linted_but_git_is_not_walked() {
    let project = Project::new("");
    project
        .write("src/.config/setup.ts", "export function setup() {}\n")
        .write(".git/hooks/x.ts", "export function hook() {}\n");
    let output = stdout(&project.run(&["scopes"]));
    assert!(
        output.contains("src/.config/setup.ts:1  function setup"),
        "{output}"
    );
    assert!(!output.contains("hook"));
}

#[test]
fn stale_keep_marks_are_warnings_and_marks_do_not_leak() {
    let server = MockServer::start(answering(json!({})));
    let project = Project::with_server(&server, "");
    project.rule("rule-a", "scopes = [\"function\"]").write(
        "src/a.ts",
        "// lintent-keep rule-a -- generated\nexport function kept() {}\nexport function next() {}\n\nconst x = 1;\n// lintent-keep rule-a -- nothing here\nconst y = 2;\n",
    );
    let output = project.run(&["check", "--json"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "a stale mark is a warning, not a failure"
    );
    let names: Vec<String> = server
        .requests()
        .iter()
        .map(|r| r.body["state"]["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(names, vec!["next"], "the mark covers `kept` only");
    let report = json_out(&output);
    let stale = &report["findings"][0];
    assert_eq!(stale["rule"], "lintent/keep");
    assert_eq!(stale["severity"], "warning");
    assert_eq!(stale["line"], 6);
    assert!(stale["message"]
        .as_str()
        .unwrap()
        .contains("suppresses nothing"));
}

#[test]
fn rules_matching_nothing_are_warned_about() {
    let project = Project::new("");
    project
        .rule("rule-a", "")
        .write(
            ".lintent/rules/typo.toml",
            "id = \"typo\"\ndescription = \"d\"\nscopes = [\"function\"]\ninclude = [\"scr/**\"]\n",
        )
        .write("src/a.ts", "export function f() {}\n");
    let rules = project.run(&["rules"]);
    assert!(
        stderr(&rules).contains("rule `typo` matches no files"),
        "{}",
        stderr(&rules)
    );
    let check = project.run(&["check", "--dry-run", "--rule", "typo", "--rule", "typo"]);
    assert!(stderr(&check).contains("rule `typo` matches no files"));
    assert!(stderr(&check).contains("match no scopes"));
    assert_eq!(check.status.code(), Some(0));
}
