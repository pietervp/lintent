//! Command-line interface.

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Result;
use clap::{Args, Parser, Subcommand};

use crate::check::{self, CheckOptions};
use crate::config::find_config;
use crate::eval::{self, EvalOptions};
use crate::extract::ScopeKind;
use crate::inspect;
use crate::languages::Registry;
use crate::rules::Severity;
use crate::scaffold;
use crate::workspace::Workspace;

const ABOUT: &str =
    "Plain-language lint rules, judged by Jev (System One API) one scope at a time.";

const LONG_ABOUT: &str =
    "Plain-language lint rules, judged by Jev (System One API) one scope at a time.

Tree-sitter cuts each file into class / method / function scopes; every rule that
matches a scope is asked as its own keyed question about that scope.

Provider and key: provider = \"openrouter\" (default, OPENROUTER_API_KEY) or
\"typesafe\" (TYPESAFE_API_KEY) in lintent.toml; keys are also read from .env.local / .env (cwd, then project root); the real environment wins.
LINTENT_MODEL and LINTENT_BASE_URL override the config.

Suppress a rule for one scope with a comment on its first line, or directly above it
(only blank and comment lines in between; decorators belong to the scope):
    // lintent-keep <rule-id>[, <rule-id>] -- <reason>
The mark covers that one scope only. A mark without a reason or naming an unknown rule
is an error; one that suppresses nothing is reported as a stale-mark warning.

Trust: runtime grammars ([languages.*] with grammar/repo) are native code; they are only
compiled and loaded with --trust-grammars or LINTENT_TRUST_GRAMMARS=1. The API key is only
sent to the provider's own host unless LINTENT_BASE_URL is set in the environment.

Budget: before anything is sent, the run's cost is estimated (input tokens ≈ request
bytes / 3); over [budget] max_cost_usd (default $0.001) nothing is sent and lintent
explains how to narrow the run. `check --dry-run` applies the same check.

Exit codes (highest wins): 3 over budget, nothing sent · 2 usage, config or API
error (the run is incomplete) · 1 a confident error-severity fail (or warning,
with --fail-on warning) or an invalid keep mark · 0 clean. --dry-run exits with the same codes.";

#[derive(Debug, Parser)]
#[command(name = "lintent", version, about = ABOUT, long_about = LONG_ABOUT)]
pub struct Cli {
    /// Path to lintent.toml (default: found by walking up from the current directory).
    #[arg(long, global = true, value_name = "PATH")]
    config: Option<PathBuf>,

    /// Allow runtime grammars from [languages.*] (native code) to be compiled and loaded.
    /// Also granted by LINTENT_TRUST_GRAMMARS=1 in the environment.
    #[arg(long, global = true)]
    trust_grammars: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Create lintent.toml and .lintent/rules/ here; ignore the cache in an existing .gitignore.
    Init,
    /// Manage rule files.
    Rule {
        #[command(subcommand)]
        action: RuleCommand,
    },
    /// List the rules (id, severity, min_confidence in effect, scopes, include).
    Rules {
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
    /// Print the scopes `check` would look at, without calling the API.
    Scopes {
        /// Files or directories (default: the project root).
        paths: Vec<PathBuf>,
        /// Only the scopes this rule reaches (include/exclude/scopes/languages).
        #[arg(long, value_name = "ID")]
        rule: Option<String>,
        /// Also list files skipped because no language handles their extension.
        #[arg(long)]
        verbose: bool,
    },
    /// Judge the scopes against the rules.
    Check(CheckArgs),
    /// Run every rule against its pass/fail fixtures.
    Eval(EvalArgs),
    /// List the languages lintent can parse and how it finds their scopes
    ///
    /// "tags": the grammar's tags query (queries/tags.scm) marks definitions.
    /// "heuristic": no tags query, so named nodes are classified by type name —
    /// *class|struct|interface|trait|impl|enum|object|module*_{declaration,definition,item,specifier}
    /// are classes; *function|method|func|fun|procedure|sub*_{declaration,definition,item} are
    /// functions (methods inside a class).
    ///
    /// Add any tree-sitter grammar in lintent.toml:
    ///   [languages.haskell]
    ///   extensions = ["hs"]
    ///   repo = "https://github.com/tree-sitter/tree-sitter-haskell"   # or grammar = "<dir | .so | .dylib>"
    ///   rev = "<full 40-character commit SHA>"                        # required with repo
    ///   tags_query = "queries/haskell-tags.scm"                       # optional
    /// A grammar directory or repo is compiled with the system C compiler on first use and
    /// cached under the user cache directory ($LINTENT_CACHE_DIR overrides).
    #[command(verbatim_doc_comment)]
    Languages {
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
enum RuleCommand {
    /// Scaffold .lintent/rules/<id>.toml and empty fixture directories.
    New {
        /// Kebab-case rule id.
        id: String,
        /// Comma-separated scope kinds: class, method, function.
        #[arg(long, value_delimiter = ',', default_value = "function,method")]
        scopes: Vec<ScopeKind>,
        /// Glob (relative to the project root) the rule applies to; repeatable.
        #[arg(long, value_name = "GLOB")]
        include: Vec<String>,
    },
}

#[derive(Debug, Args)]
struct CheckArgs {
    /// Files or directories (default: the project root).
    paths: Vec<PathBuf>,
    /// Only these rules; repeatable.
    #[arg(long = "rule", value_name = "ID")]
    rules: Vec<String>,
    /// Only files changed since the merge-base with --base, plus uncommitted and untracked files.
    #[arg(long)]
    changed: bool,
    /// Base ref for --changed (default: origin/main, falling back to main).
    #[arg(long, value_name = "REF", requires = "changed")]
    base: Option<String>,
    /// Print JSON: {findings, errors, stats}.
    #[arg(long)]
    json: bool,
    /// Print the exact request bodies instead of sending them (no API key needed).
    #[arg(long)]
    dry_run: bool,
    /// Neither read nor write the cache.
    #[arg(long, conflicts_with = "refresh")]
    no_cache: bool,
    /// Ask again for everything and overwrite the cached verdicts.
    #[arg(long)]
    refresh: bool,
    /// Budget for this run in USD (default: [budget] max_cost_usd, 0.001). Over it, nothing is sent (exit 3).
    #[arg(long, value_name = "USD", value_parser = parse_usd)]
    max_cost: Option<f64>,
    /// Lowest severity whose confident fails set exit 1: error (default) or warning.
    #[arg(long, value_name = "SEVERITY", value_parser = parse_severity, default_value = "error")]
    fail_on: Severity,
}

#[derive(Debug, Args)]
struct EvalArgs {
    /// Only these rules; repeatable.
    #[arg(long = "rule", value_name = "ID")]
    rules: Vec<String>,
    /// Print the exact request bodies instead of sending them.
    #[arg(long)]
    dry_run: bool,
    /// Neither read nor write the cache.
    #[arg(long)]
    no_cache: bool,
    /// Budget for this run in USD (default: [budget] max_cost_usd). Over it, nothing is sent (exit 3).
    #[arg(long, value_name = "USD", value_parser = parse_usd)]
    max_cost: Option<f64>,
}

fn parse_severity(value: &str) -> Result<Severity, String> {
    match value {
        "error" => Ok(Severity::Error),
        "warning" => Ok(Severity::Warning),
        _ => Err(format!("{value:?} is not a severity; use error or warning")),
    }
}

fn parse_usd(value: &str) -> Result<f64, String> {
    let parsed: f64 = value
        .trim()
        .trim_start_matches('$')
        .parse()
        .map_err(|_| format!("{value:?} is not a USD amount"))?;
    if parsed.is_finite() && parsed >= 0.0 {
        Ok(parsed)
    } else {
        Err(format!("{value:?} must be a non-negative amount"))
    }
}

pub fn main() -> ExitCode {
    restore_sigpipe();
    let cli = Cli::parse();
    match run(cli) {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("lintent: error: {error:#}");
            ExitCode::from(2)
        }
    }
}

fn run(cli: Cli) -> Result<u8> {
    let config = cli.config.as_deref();
    let trust = cli.trust_grammars;
    let code = match cli.command {
        Command::Init => {
            let cwd = std::env::current_dir()?;
            for line in scaffold::init(&cwd)? {
                println!("{line}");
            }
            0
        }
        Command::Rule {
            action:
                RuleCommand::New {
                    id,
                    scopes,
                    include,
                },
        } => {
            let cwd = std::env::current_dir()?;
            for line in scaffold::new_rule(&cwd, config, &id, &scopes, &include)? {
                println!("{line}");
            }
            0
        }
        Command::Rules { json } => {
            let workspace = Workspace::load(config, trust)?;
            print!("{}", inspect::rules(&workspace, json));
            let discovery = workspace.discover(&[], None)?;
            check::warn_unmatched_includes(&workspace.rules, &discovery.files);
            0
        }
        Command::Scopes {
            paths,
            rule,
            verbose,
        } => {
            let workspace = Workspace::load(config, trust)?;
            let (listing, summary) = inspect::scopes(&workspace, &paths, rule.as_deref(), verbose)?;
            print!("{listing}");
            eprint!("{summary}");
            0
        }
        Command::Languages { json } => {
            // Outside a project this lists the built-ins.
            let in_project = config.is_some() || find_config(&std::env::current_dir()?).is_some();
            if in_project {
                print!(
                    "{}",
                    inspect::languages(&Workspace::load(config, trust)?.registry, json)
                );
            } else {
                print!("{}", inspect::languages(&Registry::builtin(), json));
            }
            0
        }
        Command::Check(args) => {
            let workspace = Workspace::load(config, trust)?;
            let options = CheckOptions {
                paths: args.paths,
                rules: args.rules,
                changed: args.changed,
                base: args.base,
                json: args.json,
                dry_run: args.dry_run,
                no_cache: args.no_cache,
                refresh: args.refresh,
                max_cost: args.max_cost,
                fail_on: args.fail_on,
            };
            exit_byte(check::run(&workspace, &options)?)
        }
        Command::Eval(args) => {
            let workspace = Workspace::load(config, trust)?;
            let options = EvalOptions {
                rules: args.rules,
                dry_run: args.dry_run,
                no_cache: args.no_cache,
                max_cost: args.max_cost,
            };
            exit_byte(eval::run(&workspace, &options)?)
        }
    };
    Ok(code)
}

/// Rust ignores SIGPIPE, so `lintent scopes | head` would panic on a closed
/// pipe. Restoring the default makes it exit quietly like any Unix tool.
#[cfg(unix)]
fn restore_sigpipe() {
    const SIGPIPE: i32 = 13;
    const SIG_DFL: usize = 0;
    extern "C" {
        fn signal(signum: i32, handler: usize) -> usize;
    }
    // SAFETY: resetting a signal to its default disposition at startup,
    // before any other thread exists.
    unsafe {
        signal(SIGPIPE, SIG_DFL);
    }
}

#[cfg(not(unix))]
fn restore_sigpipe() {}

fn exit_byte(code: i32) -> u8 {
    u8::try_from(code).unwrap_or(2)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn parses_check_flags() {
        let cli = Cli::try_parse_from([
            "lintent",
            "check",
            "src",
            "--rule",
            "a",
            "--rule",
            "b",
            "--changed",
            "--base",
            "dev",
            "--json",
            "--dry-run",
        ])
        .unwrap();
        let Command::Check(args) = cli.command else {
            panic!("expected check");
        };
        assert_eq!(args.rules, vec!["a", "b"]);
        assert_eq!(args.base.as_deref(), Some("dev"));
        assert!(args.changed && args.json && args.dry_run);
        assert!(
            Cli::try_parse_from(["lintent", "check", "--base", "dev"]).is_err(),
            "--base needs --changed"
        );
        assert!(Cli::try_parse_from(["lintent", "check", "--no-cache", "--refresh"]).is_err());
        let Command::Check(args) =
            Cli::try_parse_from(["lintent", "check", "--max-cost", "$0.002"])
                .unwrap()
                .command
        else {
            panic!("expected check");
        };
        assert_eq!(args.max_cost, Some(0.002));
        assert!(Cli::try_parse_from(["lintent", "check", "--max-cost", "-1"]).is_err());
        assert_eq!(args.fail_on, Severity::Error);
        let Command::Check(args) =
            Cli::try_parse_from(["lintent", "check", "--fail-on", "warning"])
                .unwrap()
                .command
        else {
            panic!("expected check");
        };
        assert_eq!(args.fail_on, Severity::Warning);
        assert!(Cli::try_parse_from(["lintent", "check", "--fail-on", "info"]).is_err());
        assert!(Cli::try_parse_from(["lintent", "eval", "--max-cost", "lots"]).is_err());
    }

    #[test]
    fn parses_rule_new_scopes() {
        let cli = Cli::try_parse_from([
            "lintent",
            "rule",
            "new",
            "demo",
            "--scopes",
            "class,method",
            "--include",
            "src/**",
        ])
        .unwrap();
        let Command::Rule {
            action: RuleCommand::New {
                scopes, include, ..
            },
        } = cli.command
        else {
            panic!("expected rule new");
        };
        assert_eq!(scopes, vec![ScopeKind::Class, ScopeKind::Method]);
        assert_eq!(include, vec!["src/**"]);
        assert!(
            Cli::try_parse_from(["lintent", "rule", "new", "demo", "--scopes", "block"]).is_err()
        );
    }
}
