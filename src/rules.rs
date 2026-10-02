//! Rule files (`.lintent/rules/<id>.toml`): parsing, validation, matching.

use std::fmt;
use std::fs;
use std::path::Path;

use anyhow::{bail, Context, Result};
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use serde::{Deserialize, Serialize};

use crate::extract::{ScopeKind, Unit};
use crate::languages::Registry;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    #[default]
    Error,
    Warning,
}

impl fmt::Display for Severity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(match self {
            Severity::Error => "error",
            Severity::Warning => "warning",
        })
    }
}

/// The file as written. Unknown keys are rejected so a misspelt field
/// (`exceptons`) cannot silently weaken a rule.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RuleFile {
    id: String,
    description: String,
    why: Option<String>,
    fix: Option<String>,
    #[serde(default)]
    severity: Severity,
    scopes: Vec<ScopeKind>,
    #[serde(default)]
    languages: Vec<String>,
    include: Vec<String>,
    #[serde(default)]
    exclude: Vec<String>,
    #[serde(default)]
    exceptions: Vec<String>,
    min_confidence: Option<f64>,
    #[serde(default = "default_allow_skip")]
    allow_skip: bool,
}

fn default_allow_skip() -> bool {
    true
}

/// A validated rule.
#[derive(Debug, Clone)]
pub struct Rule {
    pub id: String,
    pub description: String,
    pub why: Option<String>,
    pub fix: Option<String>,
    pub severity: Severity,
    pub scopes: Vec<ScopeKind>,
    /// Empty = every language.
    pub languages: Vec<String>,
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    pub exceptions: Vec<String>,
    pub min_confidence: Option<f64>,
    pub allow_skip: bool,
    include_set: GlobSet,
    exclude_set: GlobSet,
}

impl Rule {
    /// Parses one rule file. `stem` is the file name without `.toml`.
    pub fn parse(stem: &str, raw: &str, languages: &[&str]) -> Result<Rule> {
        let file: RuleFile = toml::from_str(raw)?;
        if file.id != stem {
            bail!("id {:?} must equal the file name {stem:?}", file.id);
        }
        if !is_kebab_case(&file.id) {
            bail!(
                "id {:?} must be kebab-case (a-z, 0-9, single dashes)",
                file.id
            );
        }
        if file.description.trim().is_empty() {
            bail!("description must not be empty");
        }
        if file.scopes.is_empty() {
            bail!("scopes must list at least one of class, method, function");
        }
        if file.include.is_empty() {
            bail!("include must list at least one glob (relative to the project root)");
        }
        if let Some(unknown) = file
            .languages
            .iter()
            .find(|name| !languages.contains(&name.as_str()))
        {
            bail!(
                "unknown language {unknown:?}; available: {}",
                languages.join(", ")
            );
        }
        if let Some(threshold) = file.min_confidence {
            if !(0.0..=1.0).contains(&threshold) {
                bail!("min_confidence must be between 0 and 1, got {threshold}");
            }
        }
        Ok(Rule {
            include_set: build_globset(&file.include).context("include")?,
            exclude_set: build_globset(&file.exclude).context("exclude")?,
            id: file.id,
            description: file.description,
            // The scaffold writes `why = ""`; an empty hint is no hint.
            why: file.why.filter(|text| !text.trim().is_empty()),
            fix: file.fix.filter(|text| !text.trim().is_empty()),
            severity: file.severity,
            scopes: file.scopes,
            languages: file.languages,
            include: file.include,
            exclude: file.exclude,
            exceptions: file.exceptions,
            min_confidence: file.min_confidence,
            allow_skip: file.allow_skip,
        })
    }

    /// Include/exclude globs against a repo-relative path.
    pub fn matches_path(&self, path: &str) -> bool {
        self.include_set.is_match(path) && !self.exclude_set.is_match(path)
    }

    pub fn applies_to_language(&self, language: &str) -> bool {
        self.languages.is_empty() || self.languages.iter().any(|name| name == language)
    }

    /// Scope kind and language only; paths are checked separately because
    /// eval fixtures deliberately ignore include globs.
    pub fn applies_to(&self, unit: &Unit) -> bool {
        self.scopes.contains(&unit.kind) && self.applies_to_language(&unit.language)
    }

    pub fn threshold(&self, default: f64) -> f64 {
        self.min_confidence.unwrap_or(default)
    }
}

/// Loads every `*.toml` in `dir`, sorted by id. A missing directory is an
/// empty rule set; any invalid file fails the whole load, naming the file.
pub fn load_rules(dir: &Path, registry: &Registry) -> Result<Vec<Rule>> {
    let languages = registry.names();
    let mut rules = Vec::new();
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(rules),
        Err(error) => return Err(error).with_context(|| format!("reading {}", dir.display())),
    };
    for entry in entries {
        let path = entry?.path();
        if path.extension().and_then(|e| e.to_str()) != Some("toml") {
            continue;
        }
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .context("rule file names must be UTF-8")?
            .to_string();
        let raw =
            fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        let rule = Rule::parse(&stem, &raw, &languages)
            .with_context(|| format!("invalid rule {}", path.display()))?;
        rules.push(rule);
    }
    rules.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(rules)
}

pub fn is_kebab_case(value: &str) -> bool {
    !value.is_empty()
        && !value.starts_with('-')
        && !value.ends_with('-')
        && !value.contains("--")
        && value
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// `*` stays inside one path segment; `**` crosses segments.
pub fn build_globset(patterns: &[String]) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        let glob = GlobBuilder::new(pattern)
            .literal_separator(true)
            .build()
            .with_context(|| format!("invalid glob {pattern:?}"))?;
        builder.add(glob);
    }
    Ok(builder.build()?)
}

#[cfg(test)]
pub(crate) fn test_rule_text(id: &str, extra: &str) -> String {
    format!(
        "id = \"{id}\"\ndescription = \"Rule {id}.\"\nscopes = [\"function\", \"method\"]\ninclude = [\"src/**\"]\n{extra}"
    )
}

#[cfg(test)]
pub(crate) fn test_rule(id: &str, extra: &str) -> Rule {
    Rule::parse(id, &test_rule_text(id, extra), &Registry::builtin().names()).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn languages() -> Vec<&'static str> {
        vec!["typescript", "rust"]
    }

    #[test]
    fn parses_a_full_rule() {
        let raw = r#"
id = "no-db-in-routes"
description = "Route handlers must not query the database."
why = "ADR-0004"
fix = "Call a service."
severity = "warning"
scopes = ["function", "method"]
languages = ["typescript"]
include = ["apps/web/src/routes/**"]
exclude = ["**/*.test.ts"]
exceptions = ["Health checks may ping the DB."]
min_confidence = 0.9
allow_skip = false
"#;
        let rule = Rule::parse("no-db-in-routes", raw, &languages()).unwrap();
        assert_eq!(rule.severity, Severity::Warning);
        assert_eq!(rule.threshold(0.5), 0.9);
        assert!(!rule.allow_skip);
        assert!(rule.matches_path("apps/web/src/routes/a/b.ts"));
        assert!(!rule.matches_path("apps/web/src/routes/a.test.ts"));
        assert!(!rule.matches_path("apps/api/src/x.ts"));
        assert!(rule.applies_to_language("typescript"));
        assert!(!rule.applies_to_language("rust"));
    }

    #[test]
    fn defaults() {
        let rule = test_rule("demo", "");
        assert_eq!(rule.severity, Severity::Error);
        assert!(rule.allow_skip);
        assert!(rule.languages.is_empty());
        assert!(rule.applies_to_language("anything"));
        assert_eq!(rule.threshold(0.8), 0.8);
    }

    #[test]
    fn single_star_does_not_cross_directories() {
        let rule = test_rule("demo", "").clone();
        let set = build_globset(&["src/*.ts".to_string()]).unwrap();
        assert!(set.is_match("src/a.ts"));
        assert!(!set.is_match("src/a/b.ts"));
        assert!(rule.matches_path("src/a/b.ts"));
    }

    #[test]
    fn validation_errors() {
        let base = |body: &str| {
            format!("description = \"d\"\nscopes = [\"function\"]\ninclude = [\"**\"]\n{body}")
        };
        let parse = |stem: &str, body: &str| Rule::parse(stem, &base(body), &languages());
        assert!(parse("a", "id = \"b\"").is_err(), "id must match file name");
        assert!(parse("Bad_Id", "id = \"Bad_Id\"").is_err());
        assert!(parse("a", "id = \"a\"\nlanguages = [\"cobol\"]").is_err());
        assert!(parse("a", "id = \"a\"\nmin_confidence = 2.0").is_err());
        assert!(parse("a", "id = \"a\"\nseverity = \"fatal\"").is_err());
        assert!(parse("a", "id = \"a\"\nexceptons = []").is_err());
        assert!(Rule::parse(
            "a",
            "id = \"a\"\ndescription = \"d\"\nscopes = [\"function\"]\ninclude = []",
            &languages()
        )
        .is_err());
        assert!(Rule::parse(
            "a",
            "id = \"a\"\ndescription = \"d\"\nscopes = [\"block\"]\ninclude = [\"**\"]",
            &languages()
        )
        .is_err());
        assert!(parse("a", "id = \"a\"").is_ok());
    }

    #[test]
    fn kebab_case() {
        assert!(is_kebab_case("no-db-in-routes"));
        assert!(is_kebab_case("rule2"));
        assert!(!is_kebab_case("-a"));
        assert!(!is_kebab_case("a--b"));
        assert!(!is_kebab_case("A"));
        assert!(!is_kebab_case(""));
    }

    #[test]
    fn load_rules_sorts_and_names_bad_files() {
        let dir = tempfile::tempdir().unwrap();
        let registry = Registry::builtin();
        assert!(load_rules(&dir.path().join("missing"), &registry)
            .unwrap()
            .is_empty());
        for id in ["zeta", "alpha"] {
            fs::write(
                dir.path().join(format!("{id}.toml")),
                test_rule_text(id, ""),
            )
            .unwrap();
        }
        fs::write(dir.path().join("notes.md"), "ignored").unwrap();
        let ids: Vec<_> = load_rules(dir.path(), &registry)
            .unwrap()
            .into_iter()
            .map(|r| r.id)
            .collect();
        assert_eq!(ids, vec!["alpha", "zeta"]);
        fs::write(dir.path().join("broken.toml"), "id = 1").unwrap();
        let error = load_rules(dir.path(), &registry).unwrap_err();
        assert!(format!("{error:#}").contains("broken.toml"));
    }
}
