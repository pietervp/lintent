//! Read-only listings: `lintent rules`, `lintent scopes`, `lintent languages`.
//! None of them call the API.

use std::fmt::Write as _;
use std::path::PathBuf;

use anyhow::Result;
use serde_json::json;

use crate::languages::Registry;
use crate::workspace::Workspace;

pub fn rules(workspace: &Workspace, as_json: bool) -> String {
    let default_threshold = workspace.project.config.min_confidence;
    if as_json {
        let rules: Vec<_> = workspace
            .rules
            .iter()
            .map(|rule| {
                json!({
                    "id": rule.id,
                    "severity": rule.severity,
                    "min_confidence": rule.threshold(default_threshold),
                    "scopes": rule.scopes,
                    "languages": rule.languages,
                    "include": rule.include,
                    "exclude": rule.exclude,
                    "description": rule.description.trim(),
                    "why": rule.why,
                    "fix": rule.fix,
                })
            })
            .collect();
        return serde_json::to_string_pretty(&rules).expect("serializable") + "\n";
    }
    let mut out = String::new();
    if workspace.rules.is_empty() {
        out.push_str("no rules yet; create one with `lintent rule new <id> --include <glob>`\n");
    }
    for rule in &workspace.rules {
        let scopes: Vec<String> = rule.scopes.iter().map(|scope| scope.to_string()).collect();
        let _ = writeln!(
            out,
            "{:<32} {:<8} {:<5} {:<24} {}",
            rule.id,
            rule.severity,
            format!("{:.2}", rule.threshold(default_threshold)),
            scopes.join(","),
            rule.include.join(" ")
        );
    }
    out
}

/// Units that `check` would consider, `path:line  kind  name`. With a rule,
/// only what that rule reaches (include/exclude/scopes/languages), with keep
/// marks shown — the preview for "is this rule scoped right?". Returns the
/// listing (stdout) and the summary (stderr).
pub fn scopes(
    workspace: &Workspace,
    paths: &[PathBuf],
    rule: Option<&str>,
    verbose: bool,
) -> Result<(String, String)> {
    let rule = match rule {
        Some(id) => Some(workspace.select_rules(&[id.to_string()])?.remove(0)),
        None => None,
    };
    let discovery = workspace.discover(paths, None)?;
    let mut out = String::new();
    let (mut count, mut files) = (0, 0);
    for file in &discovery.files {
        if let Some(rule) = &rule {
            if !rule.matches_path(&file.path) || !rule.applies_to_language(&file.language) {
                continue;
            }
        }
        let (parsed, keeps) = match workspace.parse(file) {
            Ok(Some(parsed)) => parsed,
            Ok(None) => continue,
            Err(error) => {
                let _ = writeln!(out, "{}  error: {error:#}", file.path);
                continue;
            }
        };
        let mut listed = false;
        for (index, unit) in parsed.units.iter().enumerate() {
            let mut notes = String::new();
            if let Some(rule) = &rule {
                if !rule.applies_to(unit) {
                    continue;
                }
                if let Some(reason) = keeps.suppressed(index).get(rule.id.as_str()) {
                    let _ = write!(notes, "  [kept: {reason}]");
                }
            }
            if unit.truncated {
                notes.push_str("  [truncated]");
            }
            let _ = writeln!(
                out,
                "{}:{}  {:<8} {}{notes}",
                unit.path, unit.start_line, unit.kind, unit.name
            );
            count += 1;
            listed = true;
        }
        if listed {
            files += 1;
        }
        for problem in &keeps.problems {
            let _ = writeln!(
                out,
                "{}:{}  keep-error  {}",
                file.path, problem.line, problem.message
            );
        }
    }
    if verbose {
        for path in &discovery.unsupported {
            let _ = writeln!(out, "{path}  skipped: no language for this extension");
        }
    }
    let target = rule
        .map(|rule| format!(" matched by {}", rule.id))
        .unwrap_or_default();
    let mut summary = format!("{count} scope(s) in {files} file(s){target}\n");
    for note in discovery.skipped_notes(paths) {
        let _ = writeln!(summary, "{note}");
    }
    Ok((out, summary))
}

pub fn languages(registry: &Registry, as_json: bool) -> String {
    if as_json {
        let list: Vec<_> = registry
            .specs()
            .iter()
            .map(|spec| {
                json!({
                    "name": spec.name,
                    "extensions": spec.extensions,
                    "source": spec.origin.to_string(),
                    "grammar": spec.grammar_description(),
                    "scopes_via": spec.strategy(),
                })
            })
            .collect();
        return serde_json::to_string_pretty(&list).expect("serializable") + "\n";
    }
    let mut out = String::new();
    let _ = writeln!(
        out,
        "{:<12} {:<9} {:<10} {:<28} GRAMMAR",
        "LANGUAGE", "SOURCE", "SCOPES VIA", "EXTENSIONS"
    );
    for spec in registry.specs() {
        let extensions: Vec<String> = spec.extensions.iter().map(|e| format!(".{e}")).collect();
        let strategy = match spec.strategy() {
            "tags" => "tags",
            "heuristic" => "heuristic",
            _ => "tags?",
        };
        let _ = writeln!(
            out,
            "{:<12} {:<9} {:<10} {:<28} {}",
            spec.name,
            spec.origin.to_string(),
            strategy,
            extensions.join(" "),
            spec.grammar_description()
        );
    }
    out.push_str(
        "\ntags = the grammar's tags query finds scopes; heuristic = node-type names do (see `lintent languages --help`).\n\
         Add any tree-sitter grammar with a [languages.<name>] table in lintent.toml.\n",
    );
    out
}
