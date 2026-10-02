//! `lintent init` and `lintent rule new`. Both are idempotent and never
//! overwrite a file that already exists.

use std::fs;
use std::path::Path;

use anyhow::{bail, Context, Result};

use crate::config::{default_config_toml, find_config, CACHE_FILE, CONFIG_FILE, STATE_DIR};
use crate::extract::ScopeKind;
use crate::rules::is_kebab_case;

/// Creates `lintent.toml` and `.lintent/rules/` in `dir`, and ignores the
/// cache in an existing `.gitignore`. Returns what was done, line by line.
pub fn init(dir: &Path) -> Result<Vec<String>> {
    let mut done = Vec::new();
    let config = dir.join(CONFIG_FILE);
    if config.exists() {
        done.push(format!("kept existing {CONFIG_FILE}"));
    } else {
        fs::write(&config, default_config_toml())
            .with_context(|| format!("writing {}", config.display()))?;
        done.push(format!("created {CONFIG_FILE}"));
    }
    let rules = dir.join(STATE_DIR).join("rules");
    if rules.is_dir() {
        done.push(format!("kept existing {STATE_DIR}/rules/"));
    } else {
        fs::create_dir_all(&rules).with_context(|| format!("creating {}", rules.display()))?;
        done.push(format!("created {STATE_DIR}/rules/"));
    }
    let gitignore = dir.join(".gitignore");
    if gitignore.is_file() {
        let text = fs::read_to_string(&gitignore).context("reading .gitignore")?;
        let present = text.lines().any(|line| {
            matches!(line.trim(), CACHE_FILE) || line.trim() == format!("/{CACHE_FILE}")
        });
        if present {
            done.push(format!(".gitignore already ignores {CACHE_FILE}"));
        } else {
            let separator = if text.is_empty() || text.ends_with('\n') {
                ""
            } else {
                "\n"
            };
            fs::write(&gitignore, format!("{text}{separator}{CACHE_FILE}\n"))
                .context("writing .gitignore")?;
            done.push(format!("added {CACHE_FILE} to .gitignore"));
        }
    }
    Ok(done)
}

/// Scaffolds `.lintent/rules/<id>.toml` plus empty fixture directories in
/// the project containing `cwd`.
pub fn new_rule(
    cwd: &Path,
    config: Option<&Path>,
    id: &str,
    scopes: &[ScopeKind],
    include: &[String],
) -> Result<Vec<String>> {
    if !is_kebab_case(id) {
        bail!("rule id {id:?} must be kebab-case (a-z, 0-9, single dashes)");
    }
    let config_path = match config {
        Some(path) => cwd.join(path),
        None => find_config(cwd)
            .with_context(|| format!("no {CONFIG_FILE} found (run `lintent init` first)"))?,
    };
    let root = config_path.parent().unwrap_or(cwd);
    let state = root.join(STATE_DIR);
    let path = state.join("rules").join(format!("{id}.toml"));
    if path.exists() {
        bail!("{} already exists; edit it instead", path.display());
    }
    let mut done = Vec::new();
    let include: Vec<String> = if include.is_empty() {
        done.push(
            "warning: no --include given; the rule matches every file until you narrow `include`"
                .to_string(),
        );
        vec!["**".to_string()]
    } else {
        include.to_vec()
    };
    fs::create_dir_all(path.parent().expect("rules dir"))
        .context("creating the rules directory")?;
    fs::write(&path, rule_template(id, scopes, &include))
        .with_context(|| format!("writing {}", path.display()))?;
    done.push(format!("created {STATE_DIR}/rules/{id}.toml"));
    for kind in ["pass", "fail"] {
        let dir = state.join("fixtures").join(id).join(kind);
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        done.push(format!("created {STATE_DIR}/fixtures/{id}/{kind}/"));
    }
    Ok(done)
}

fn rule_template(id: &str, scopes: &[ScopeKind], include: &[String]) -> String {
    let list = |items: Vec<String>| {
        items
            .iter()
            .map(|item| toml_string(item))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let scopes = list(scopes.iter().map(|scope| scope.to_string()).collect());
    let include = list(include.to_vec());
    format!(
        r#"id = "{id}"
# Plain language, judged one scope at a time. Say what FAILS and what PASSES.
description = """
Describe what fails this rule and what passes it.
"""
# Printed with every finding: the ADR, doc or incident behind the rule.
why = ""
# Printed as the hint: what the author should do instead.
fix = ""
severity = "error"                 # error | warning
scopes = [{scopes}]   # class | method | function
# languages = ["typescript"]       # optional; default: every language
include = [{include}]
exclude = []
exceptions = []
# min_confidence = 0.8             # optional per-rule override
allow_skip = true                  # the model may answer "skip" when the rule's subject is absent
"#
    )
}

fn toml_string(value: &str) -> String {
    toml::Value::String(value.to_string()).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, Env};
    use crate::languages::Registry;
    use crate::rules::Rule;

    #[test]
    fn init_is_idempotent_and_never_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(".gitignore"), "node_modules").unwrap();
        let first = init(dir.path()).unwrap();
        assert!(first.iter().any(|line| line == "created lintent.toml"));
        assert_eq!(
            fs::read_to_string(dir.path().join(".gitignore")).unwrap(),
            "node_modules\n.lintent/cache.json\n"
        );
        Config::parse(
            &fs::read_to_string(dir.path().join(CONFIG_FILE)).unwrap(),
            &Env::default(),
        )
        .unwrap();

        fs::write(dir.path().join(CONFIG_FILE), "concurrency = 9").unwrap();
        let second = init(dir.path()).unwrap();
        assert!(second
            .iter()
            .any(|line| line.starts_with("kept existing lintent.toml")));
        assert_eq!(
            fs::read_to_string(dir.path().join(CONFIG_FILE)).unwrap(),
            "concurrency = 9"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join(".gitignore"))
                .unwrap()
                .matches("cache.json")
                .count(),
            1
        );
    }

    #[test]
    fn init_without_gitignore_does_not_create_one() {
        let dir = tempfile::tempdir().unwrap();
        init(dir.path()).unwrap();
        assert!(!dir.path().join(".gitignore").exists());
    }

    #[test]
    fn new_rule_scaffolds_a_valid_rule_and_fixture_dirs() {
        let dir = tempfile::tempdir().unwrap();
        init(dir.path()).unwrap();
        let nested = dir.path().join("src");
        fs::create_dir_all(&nested).unwrap();
        new_rule(
            &nested,
            None,
            "demo",
            &[ScopeKind::Function, ScopeKind::Method],
            &["src/**".to_string()],
        )
        .unwrap();
        let raw = fs::read_to_string(dir.path().join(".lintent/rules/demo.toml")).unwrap();
        let rule = Rule::parse("demo", &raw, &Registry::builtin().names()).unwrap();
        assert_eq!(rule.scopes, vec![ScopeKind::Function, ScopeKind::Method]);
        assert_eq!(rule.include, vec!["src/**"]);
        assert!(dir.path().join(".lintent/fixtures/demo/pass").is_dir());
        assert!(dir.path().join(".lintent/fixtures/demo/fail").is_dir());

        assert!(
            new_rule(dir.path(), None, "demo", &[ScopeKind::Class], &[]).is_err(),
            "never overwrites"
        );
        assert!(new_rule(dir.path(), None, "Bad Id", &[ScopeKind::Class], &[]).is_err());
        let done = new_rule(dir.path(), None, "broad", &[ScopeKind::Class], &[]).unwrap();
        assert!(done[0].starts_with("warning"));
    }

    #[test]
    fn new_rule_needs_a_project() {
        let dir = tempfile::tempdir().unwrap();
        assert!(new_rule(dir.path(), None, "demo", &[ScopeKind::Class], &[]).is_err());
    }
}
