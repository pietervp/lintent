//! Project configuration (`lintent.toml`), project-root discovery and the
//! environment (real env first, then a `.env` in the working directory).

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

pub const CONFIG_FILE: &str = "lintent.toml";
pub const STATE_DIR: &str = ".lintent";
pub const CACHE_FILE: &str = ".lintent/cache.json";

/// Excluded from every walk unless the project config says otherwise.
pub const DEFAULT_EXCLUDE: &[&str] = &[
    "**/node_modules/**",
    "**/dist/**",
    "**/target/**",
    "**/*.gen.ts",
    "**/*.min.js",
    "**/*.min.css",
    ".lintent/fixtures/**",
];

/// Which service hosts Jev. Both speak the same System One API; the provider
/// only decides the base URL, the default model and which key is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    #[default]
    Openrouter,
    Typesafe,
}

impl Provider {
    pub fn as_str(self) -> &'static str {
        match self {
            Provider::Openrouter => "openrouter",
            Provider::Typesafe => "typesafe",
        }
    }

    pub fn default_model(self) -> &'static str {
        match self {
            Provider::Openrouter => "typesafe/jev-1.13",
            Provider::Typesafe => "jev-latest",
        }
    }

    pub fn default_base_url(self) -> &'static str {
        match self {
            Provider::Openrouter => "https://openrouter.ai/api/v1",
            Provider::Typesafe => "https://api.typesafe.ai/v1",
        }
    }

    pub fn key_variable(self) -> &'static str {
        match self {
            Provider::Openrouter => "OPENROUTER_API_KEY",
            Provider::Typesafe => "TYPESAFE_API_KEY",
        }
    }
}

impl fmt::Display for Provider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(self.as_str())
    }
}

/// A `[languages.<name>]` table: extra extensions or a replacement tags
/// query for a built-in, or a whole runtime grammar (see [`crate::grammar`]).
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LanguageConfig {
    #[serde(default)]
    pub extensions: Vec<String>,
    /// A compiled grammar library, or a grammar directory with `src/parser.c`.
    pub grammar: Option<String>,
    /// A git URL to clone the grammar from.
    pub repo: Option<String>,
    pub rev: Option<String>,
    pub subdir: Option<String>,
    pub tags_query: Option<String>,
    /// The exported language function, default `tree_sitter_<name>`.
    pub symbol: Option<String>,
}

impl LanguageConfig {
    pub fn has_grammar(&self) -> bool {
        self.grammar.is_some() || self.repo.is_some()
    }

    pub fn describe(&self) -> String {
        match (&self.grammar, &self.repo) {
            (Some(grammar), _) => grammar.clone(),
            (None, Some(repo)) => match &self.rev {
                Some(rev) => format!("{repo}@{rev}"),
                None => repo.clone(),
            },
            (None, None) => "built-in".to_string(),
        }
    }

    fn validate(&self, name: &str) -> Result<()> {
        if self.grammar.is_some() && self.repo.is_some() {
            bail!("[languages.{name}]: set `grammar` or `repo`, not both");
        }
        if self.repo.is_none() && (self.rev.is_some() || self.subdir.is_some()) {
            bail!("[languages.{name}]: `rev` and `subdir` only apply to `repo`");
        }
        for (field, value) in [
            ("grammar", &self.grammar),
            ("subdir", &self.subdir),
            ("tags_query", &self.tags_query),
        ] {
            if let Some(value) = value {
                let path = Path::new(value);
                if path.is_absolute()
                    || path
                        .components()
                        .any(|c| matches!(c, std::path::Component::ParentDir))
                {
                    bail!("[languages.{name}]: `{field}` must be a relative path inside the project (no `..`), got {value:?}");
                }
            }
        }
        if let Some(repo) = &self.repo {
            if repo.starts_with('-') {
                bail!("[languages.{name}]: `repo` {repo:?} is not a URL");
            }
            // A branch or tag can be moved to point at different code; a full
            // commit id cannot.
            let pinned = self
                .rev
                .as_deref()
                .is_some_and(|rev| rev.len() == 40 && rev.chars().all(|c| c.is_ascii_hexdigit()));
            if !pinned {
                bail!(
                    "[languages.{name}]: `repo` needs `rev` set to a full 40-character commit SHA"
                );
            }
        }
        Ok(())
    }
}

/// `lintent.toml` as written on disk. Unknown keys are rejected so a typo
/// (`isolate_rule`) fails loudly instead of silently doing nothing.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigFile {
    #[serde(default)]
    provider: Provider,
    model: Option<String>,
    base_url: Option<String>,
    #[serde(default = "default_min_confidence")]
    min_confidence: f64,
    #[serde(default = "default_concurrency")]
    concurrency: usize,
    #[serde(default)]
    isolate_rules: bool,
    #[serde(default = "default_exclude")]
    exclude: Vec<String>,
    #[serde(default)]
    languages: BTreeMap<String, LanguageConfig>,
    #[serde(default)]
    budget: Budget,
}

/// The per-run cost ceiling, checked before anything is sent.
///
/// Prices default to `typesafe/jev-1.13` on OpenRouter ($0.042 per million
/// prompt tokens, completions free). They are not looked up anywhere: update
/// them when you change `model`, or the estimate is meaningless.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Budget {
    /// 0.0001 USD = 0.01 US cent per run.
    pub max_cost_usd: f64,
    pub input_price_per_million: f64,
    pub output_price_per_million: f64,
    /// Fixed input tokens the provider adds to every request (its own
    /// prompt around the state and questions).
    pub request_overhead_tokens: u64,
    /// Fixed input tokens per question on top of its text.
    pub question_overhead_tokens: u64,
}

impl Default for Budget {
    fn default() -> Self {
        Budget {
            max_cost_usd: 0.0001,
            input_price_per_million: 0.042,
            output_price_per_million: 0.0,
            // Calibrated on typesafe/jev-1.13 (2026-10): a 509-byte request
            // with one question was billed 394 input tokens.
            request_overhead_tokens: 300,
            question_overhead_tokens: 80,
        }
    }
}

fn default_min_confidence() -> f64 {
    0.8
}

fn default_concurrency() -> usize {
    4
}

fn default_exclude() -> Vec<String> {
    DEFAULT_EXCLUDE.iter().map(|s| s.to_string()).collect()
}

/// The resolved configuration: per-provider defaults and env overrides applied.
#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub provider: Provider,
    pub model: String,
    pub base_url: String,
    pub min_confidence: f64,
    pub concurrency: usize,
    /// One request per (scope, rule) instead of one per scope with every
    /// rule as its own keyed question.
    pub isolate_rules: bool,
    pub exclude: Vec<String>,
    pub languages: BTreeMap<String, LanguageConfig>,
    pub budget: Budget,
}

impl Config {
    /// Parses `lintent.toml` and applies `LINTENT_MODEL` / `LINTENT_BASE_URL`.
    pub fn parse(text: &str, env: &Env) -> Result<Config> {
        let file: ConfigFile = toml::from_str(text)?;
        let provider = file.provider;
        let model = env
            .get("LINTENT_MODEL")
            .or(file.model)
            .unwrap_or_else(|| provider.default_model().to_string());
        let base_url = resolve_base_url(provider, file.base_url.as_deref(), env)?;
        if model.trim().is_empty() {
            bail!("model must not be empty");
        }
        if !(0.0..=1.0).contains(&file.min_confidence) {
            bail!(
                "min_confidence must be between 0 and 1, got {}",
                file.min_confidence
            );
        }
        if file.concurrency == 0 {
            bail!("concurrency must be at least 1");
        }
        let budget = file.budget;
        for (field, value) in [
            ("max_cost_usd", budget.max_cost_usd),
            ("input_price_per_million", budget.input_price_per_million),
            ("output_price_per_million", budget.output_price_per_million),
        ] {
            if !(value.is_finite() && value >= 0.0) {
                bail!("[budget] {field} must be a non-negative number, got {value}");
            }
        }
        for (name, language) in &file.languages {
            language.validate(name)?;
        }
        Ok(Config {
            provider,
            model,
            base_url,
            min_confidence: file.min_confidence,
            concurrency: file.concurrency,
            isolate_rules: file.isolate_rules,
            exclude: file.exclude,
            languages: file.languages,
            budget: file.budget,
        })
    }

    /// What, besides the rule and the unit, decides an answer: where it is
    /// asked and how. Part of every cache key.
    pub fn cache_scope(&self) -> String {
        format!(
            "{}\0{}\0{}\0isolate={}",
            self.provider, self.base_url, self.model, self.isolate_rules
        )
    }

    /// The System One endpoint.
    pub fn endpoint(&self) -> String {
        format!("{}/systemone", self.base_url)
    }
}

/// Where the API key may be sent. A committed lintent.toml is not trusted to
/// redirect it: in the file, `base_url` may only point at the provider's own
/// host over https (a different path is fine). Another host has to come from
/// the real process environment (`LINTENT_BASE_URL`, not a dotenv file), and
/// plain http is accepted only for loopback (local mocks and proxies).
fn resolve_base_url(provider: Provider, from_file: Option<&str>, env: &Env) -> Result<String> {
    let default = provider.default_base_url();
    let normalize = |url: &str| url.trim().trim_end_matches('/').to_string();
    if let Some(url) = env.get_process("LINTENT_BASE_URL") {
        let url = normalize(&url);
        let host = url_host(&url)
            .with_context(|| format!("LINTENT_BASE_URL {url:?} is not an http(s) URL"))?;
        if url.starts_with("http://") && !is_loopback(&host) {
            bail!("LINTENT_BASE_URL {url:?}: plain http is only allowed for loopback hosts; use https");
        }
        return Ok(url);
    }
    let Some(url) = from_file else {
        return Ok(default.to_string());
    };
    let url = normalize(url);
    let expected = url_host(default).expect("defaults are valid URLs");
    if !url.starts_with("https://") || url_host(&url).as_deref() != Some(expected.as_str()) {
        bail!(
            "base_url {url:?} must be https://{expected}/… for provider \"{provider}\": the API key is only \
             sent to the provider's own host from a committed config. To use another endpoint, set \
             LINTENT_BASE_URL in the environment."
        );
    }
    Ok(url)
}

/// The host of an http(s) URL, without userinfo or port.
fn url_host(url: &str) -> Option<String> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    let authority = rest.split(['/', '?', '#']).next()?;
    let host_port = authority.rsplit('@').next()?;
    let host = if let Some(bracketed) = host_port.strip_prefix('[') {
        bracketed.split(']').next()?.to_string()
    } else {
        host_port.split(':').next()?.to_string()
    };
    (!host.is_empty()).then(|| host.to_ascii_lowercase())
}

fn is_loopback(host: &str) -> bool {
    host == "localhost" || host == "::1" || host.starts_with("127.")
}

/// The file `lintent init` writes.
pub fn default_config_toml() -> String {
    let exclude = DEFAULT_EXCLUDE
        .iter()
        .map(|glob| format!("\"{glob}\""))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        r#"# lintent — plain-language lint rules judged by Jev (System One API).
# Rules live in .lintent/rules/<rule-id>.toml.

# "openrouter" (key in OPENROUTER_API_KEY) or "typesafe" (key in TYPESAFE_API_KEY).
provider = "openrouter"
# Default per provider: openrouter → "typesafe/jev-1.13", typesafe → "jev-latest". LINTENT_MODEL overrides.
model = "typesafe/jev-1.13"
# Default per provider: openrouter → "https://openrouter.ai/api/v1", typesafe → "https://api.typesafe.ai/v1".
# Requests go to <base_url>/systemone. LINTENT_BASE_URL overrides.
base_url = "https://openrouter.ai/api/v1"
# A "fail" below this confidence is reported as "uncertain" and never blocks.
min_confidence = 0.8
concurrency = 4
# false: one request per scope, each matching rule its own keyed question.
# true: one request per (scope, rule).
isolate_rules = false
exclude = [{exclude}]

# Nothing is sent when a run's estimated cost exceeds max_cost_usd (exit code 3).
# The prices are for typesafe/jev-1.13 — update them when you change `model`.
[budget]
max_cost_usd = 0.0001              # USD per run (0.01 US cent); `--max-cost` overrides
input_price_per_million = 0.042    # USD per million input tokens
output_price_per_million = 0.0     # USD per million output tokens
request_overhead_tokens = 300      # provider prompt added to every request
question_overhead_tokens = 80      # added per question

# Any tree-sitter grammar can be added (see `lintent languages`):
# [languages.haskell]
# extensions = ["hs"]
# repo = "https://github.com/tree-sitter/tree-sitter-haskell"
# rev = "<full 40-character commit SHA>"
# Runtime grammars are native code: they only load with --trust-grammars or LINTENT_TRUST_GRAMMARS=1.
"#
    )
}

/// A located project: the directory holding `lintent.toml`.
#[derive(Debug, Clone)]
pub struct Project {
    pub root: PathBuf,
    pub config: Config,
}

impl Project {
    /// Uses `explicit` when given, else walks up from `cwd` to the first
    /// directory containing `lintent.toml`.
    pub fn locate(explicit: Option<&Path>, cwd: &Path, env: &mut Env) -> Result<Project> {
        let config_path = match explicit {
            Some(path) => cwd.join(path),
            None => find_config(cwd).with_context(|| {
                format!(
                    "no {CONFIG_FILE} found in {} or any parent directory (run `lintent init`)",
                    cwd.display()
                )
            })?,
        };
        let root = config_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| cwd.to_path_buf());
        let root = root
            .canonicalize()
            .with_context(|| format!("resolving {}", root.display()))?;
        // Dotenv files next to lintent.toml count too, so running from a
        // subdirectory still finds the key `dev:setup` wrote at the root.
        env.add_dir(&root);
        let text = fs::read_to_string(&config_path)
            .with_context(|| format!("reading {}", config_path.display()))?;
        let config = Config::parse(&text, env)
            .with_context(|| format!("invalid {}", config_path.display()))?;
        Ok(Project { root, config })
    }

    pub fn rules_dir(&self) -> PathBuf {
        self.root.join(STATE_DIR).join("rules")
    }

    pub fn fixtures_dir(&self, rule: &str) -> PathBuf {
        self.root.join(STATE_DIR).join("fixtures").join(rule)
    }

    pub fn cache_path(&self) -> PathBuf {
        self.root.join(CACHE_FILE)
    }
}

pub fn find_config(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .map(|dir| dir.join(CONFIG_FILE))
        .find(|candidate| candidate.is_file())
}

/// Environment lookups. Precedence: the real environment, then
/// `<cwd>/.env.local`, `<cwd>/.env`, `<project root>/.env.local`,
/// `<project root>/.env`. The real environment wins so CI can override a
/// developer's file without editing it. Values are never printed.
#[derive(Clone, Default)]
pub struct Env {
    dotenv: HashMap<String, String>,
    /// Stands in for the process environment in tests.
    #[cfg(test)]
    process: Option<HashMap<String, String>>,
}

/// Keys only: values are secrets and must never reach a log.
impl fmt::Debug for Env {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut keys: Vec<&String> = self.dotenv.keys().collect();
        keys.sort();
        f.debug_struct("Env")
            .field("dotenv_keys", &keys)
            .finish_non_exhaustive()
    }
}

impl Env {
    pub fn load(cwd: &Path) -> Env {
        let mut env = Env::default();
        env.add_dir(cwd);
        env
    }

    /// Adds `dir/.env.local` then `dir/.env`, never overriding a key that is
    /// already known (earlier sources win).
    pub fn add_dir(&mut self, dir: &Path) {
        for name in [".env.local", ".env"] {
            let Ok(text) = fs::read_to_string(dir.join(name)) else {
                continue;
            };
            for (key, value) in parse_dotenv(&text) {
                self.dotenv.entry(key).or_insert(value);
            }
        }
    }

    /// A non-empty, trimmed value from the process environment or a dotenv file.
    pub fn get(&self, key: &str) -> Option<String> {
        self.get_process(key).or_else(|| {
            self.dotenv
                .get(key)
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
        })
    }

    /// A non-empty value from the process environment only. Used for
    /// settings a file in the repository must not be able to make (where the
    /// key is sent, whether native grammar code may run).
    pub fn get_process(&self, key: &str) -> Option<String> {
        #[cfg(test)]
        if let Some(process) = &self.process {
            return process
                .get(key)
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty());
        }
        std::env::var(key)
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    }
}

/// `KEY=value` lines; `export ` prefixes, comments and surrounding quotes are
/// tolerated because `.env` files are written for shells as often as for
/// dotenv libraries.
pub fn parse_dotenv(text: &str) -> HashMap<String, String> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter_map(|line| {
            let line = line.strip_prefix("export ").unwrap_or(line);
            let (key, value) = line.split_once('=')?;
            let value = value.trim();
            let value = ['"', '\'']
                .iter()
                .find_map(|quote| {
                    value
                        .strip_prefix(*quote)
                        .and_then(|inner| inner.strip_suffix(*quote))
                })
                .unwrap_or(value);
            Some((key.trim().to_string(), value.to_string()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_with(pairs: &[(&str, &str)]) -> Env {
        Env {
            dotenv: pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            process: Some(HashMap::new()),
        }
    }

    fn process_env(pairs: &[(&str, &str)]) -> Env {
        Env {
            dotenv: HashMap::new(),
            process: Some(
                pairs
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
            ),
        }
    }

    #[test]
    fn empty_config_uses_openrouter_defaults() {
        let config = Config::parse("", &Env::default()).unwrap();
        assert_eq!(config.provider, Provider::Openrouter);
        assert_eq!(config.model, "typesafe/jev-1.13");
        assert_eq!(config.endpoint(), "https://openrouter.ai/api/v1/systemone");
        assert_eq!(config.min_confidence, 0.8);
        assert_eq!(config.concurrency, 4);
        assert!(!config.isolate_rules);
        assert_eq!(config.exclude.len(), DEFAULT_EXCLUDE.len());
    }

    #[test]
    fn budget_defaults_and_validation() {
        let env = Env::default();
        assert_eq!(Config::parse("", &env).unwrap().budget, Budget::default());
        let custom = Config::parse("[budget]\nmax_cost_usd = 0.5\n", &env).unwrap();
        assert_eq!(custom.budget.max_cost_usd, 0.5);
        assert_eq!(custom.budget.input_price_per_million, 0.042);
        assert!(Config::parse("[budget]\nmax_cost_usd = -1.0\n", &env).is_err());
        assert!(Config::parse("[budget]\nmax_cost = 1.0\n", &env).is_err());
    }

    #[test]
    fn typesafe_provider_defaults() {
        let config = Config::parse("provider = \"typesafe\"", &Env::default()).unwrap();
        assert_eq!(config.model, "jev-latest");
        assert_eq!(config.endpoint(), "https://api.typesafe.ai/v1/systemone");
        assert_eq!(config.provider.key_variable(), "TYPESAFE_API_KEY");
    }

    #[test]
    fn language_tables_are_validated() {
        let env = Env::default();
        let config = Config::parse(
            "[languages.toy]\nextensions = [\"toy\"]\ngrammar = \"grammars/toy\"\n",
            &env,
        )
        .unwrap();
        assert_eq!(
            config.languages["toy"].grammar.as_deref(),
            Some("grammars/toy")
        );
        assert!(Config::parse("[languages.x]\ngrammar = \"a\"\nrepo = \"b\"\n", &env).is_err());
        assert!(Config::parse("[languages.x]\nrev = \"v1\"\n", &env).is_err());
        assert!(Config::parse("[languages.x]\nparser = \"a\"\n", &env).is_err());
    }

    #[test]
    fn env_overrides_file() {
        let env = process_env(&[
            ("LINTENT_MODEL", "other-model"),
            ("LINTENT_BASE_URL", "http://127.0.0.1:9/"),
        ]);
        let config = Config::parse("model = \"x\"", &env).unwrap();
        assert_eq!(config.model, "other-model");
        assert_eq!(config.base_url, "http://127.0.0.1:9");
    }

    #[test]
    fn the_key_only_goes_to_the_providers_host_unless_the_process_env_says_otherwise() {
        let none = process_env(&[]);
        let ok = Config::parse("base_url = \"https://openrouter.ai/api/v2/\"", &none).unwrap();
        assert_eq!(ok.base_url, "https://openrouter.ai/api/v2");
        for bad in [
            "https://evil.example/api/v1",
            "http://openrouter.ai/api/v1",
            "http://127.0.0.1:9",
            "https://openrouter.ai.evil.example",
            "https://openrouter.ai@evil.example/",
        ] {
            let error = Config::parse(&format!("base_url = \"{bad}\""), &none).unwrap_err();
            assert!(format!("{error}").contains("LINTENT_BASE_URL"), "{bad}");
        }
        assert!(Config::parse(
            "provider = \"typesafe\"\nbase_url = \"https://openrouter.ai/api/v1\"",
            &none
        )
        .is_err());
        // A dotenv file in the repo is not the process environment.
        let dotenv = env_with(&[("LINTENT_BASE_URL", "https://evil.example")]);
        assert_eq!(
            Config::parse("", &dotenv).unwrap().base_url,
            "https://openrouter.ai/api/v1"
        );
        assert_eq!(
            Config::parse(
                "",
                &process_env(&[("LINTENT_BASE_URL", "https://proxy.example/v1")])
            )
            .unwrap()
            .base_url,
            "https://proxy.example/v1"
        );
        assert!(Config::parse(
            "",
            &process_env(&[("LINTENT_BASE_URL", "http://proxy.example")])
        )
        .is_err());
        assert!(Config::parse(
            "",
            &process_env(&[("LINTENT_BASE_URL", "http://localhost:8080")])
        )
        .is_ok());
    }

    #[test]
    fn env_debug_never_shows_values() {
        let env = env_with(&[("OPENROUTER_API_KEY", "sk-secret")]);
        let shown = format!("{env:?}");
        assert!(shown.contains("OPENROUTER_API_KEY") && !shown.contains("sk-secret"));
    }

    #[test]
    fn runtime_grammar_paths_and_pins_are_validated() {
        let env = process_env(&[]);
        let sha = "0123456789abcdef0123456789abcdef01234567";
        let parse = |table: &str| {
            Config::parse(
                &format!("[languages.x]\nextensions = [\"x\"]\n{table}"),
                &env,
            )
        };
        assert!(parse("grammar = \"grammars/x\"").is_ok());
        assert!(parse("grammar = \"../outside\"").is_err());
        assert!(parse("grammar = \"/abs/x.so\"").is_err());
        assert!(parse("tags_query = \"a/../../b.scm\"").is_err());
        assert!(parse(&format!(
            "repo = \"https://x/y\"\nrev = \"{sha}\"\nsubdir = \"..\""
        ))
        .is_err());
        assert!(parse(&format!("repo = \"https://x/y\"\nrev = \"{sha}\"")).is_ok());
        assert!(parse("repo = \"https://x/y\"\nrev = \"v1.0\"").is_err());
        assert!(parse("repo = \"https://x/y\"").is_err());
        assert!(parse(&format!("repo = \"--upload-pack=x\"\nrev = \"{sha}\"")).is_err());
    }

    #[test]
    fn invalid_values_are_rejected() {
        let env = Env::default();
        assert!(Config::parse("min_confidence = 1.5", &env).is_err());
        assert!(Config::parse("concurrency = 0", &env).is_err());
        assert!(Config::parse("base_url = \"ftp://x\"", &env).is_err());
        assert!(Config::parse("isolate_rule = true", &env).is_err());
        assert!(Config::parse("provider = \"anthropic\"", &env).is_err());
    }

    #[test]
    fn default_config_round_trips() {
        let config = Config::parse(&default_config_toml(), &Env::default()).unwrap();
        assert_eq!(config, Config::parse("", &Env::default()).unwrap());
    }

    #[test]
    fn dotenv_parsing() {
        let parsed =
            parse_dotenv("# comment\nexport A=1\nB = \"two words\"\nC='x'\n\nnot a pair\nD=\n");
        assert_eq!(parsed["A"], "1");
        assert_eq!(parsed["B"], "two words");
        assert_eq!(parsed["C"], "x");
        assert_eq!(parsed["D"], "");
        assert!(!parsed.contains_key("not a pair"));
    }

    #[test]
    fn dotenv_precedence_local_then_plain_then_root() {
        let root = tempfile::tempdir().unwrap();
        let cwd = root.path().join("sub");
        fs::create_dir_all(&cwd).unwrap();
        fs::write(cwd.join(".env.local"), "LINTENT_T_A=cwd-local\n").unwrap();
        fs::write(cwd.join(".env"), "LINTENT_T_A=cwd\nLINTENT_T_B=cwd\n").unwrap();
        fs::write(
            root.path().join(".env.local"),
            "LINTENT_T_B=root-local\nLINTENT_T_C=root-local\n",
        )
        .unwrap();
        fs::write(
            root.path().join(".env"),
            "LINTENT_T_C=root\nLINTENT_T_D=root\n",
        )
        .unwrap();
        fs::write(root.path().join(CONFIG_FILE), "").unwrap();
        let mut env = Env::load(&cwd);
        Project::locate(None, &cwd, &mut env).unwrap();
        assert_eq!(env.get("LINTENT_T_A").as_deref(), Some("cwd-local"));
        assert_eq!(env.get("LINTENT_T_B").as_deref(), Some("cwd"));
        assert_eq!(env.get("LINTENT_T_C").as_deref(), Some("root-local"));
        assert_eq!(env.get("LINTENT_T_D").as_deref(), Some("root"));
    }

    #[test]
    fn locate_walks_up() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(CONFIG_FILE), "concurrency = 2").unwrap();
        let nested = dir.path().join("a/b");
        fs::create_dir_all(&nested).unwrap();
        let project = Project::locate(None, &nested, &mut Env::default()).unwrap();
        assert_eq!(project.root, dir.path().canonicalize().unwrap());
        assert_eq!(project.config.concurrency, 2);
    }
}
