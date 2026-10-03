//! Everything a command needs about the project being linted: config,
//! environment, languages, rules, and which files to look at.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use globset::GlobSet;
use ignore::WalkBuilder;

use crate::config::{Env, Project};
use crate::extract::{parse_file, ParsedFile};
use crate::keep::Keeps;
use crate::languages::Registry;
use crate::rules::{build_globset, load_rules, Rule};

/// Gitignore-syntax files listing paths lintent never looks at, read in
/// every directory like `.gitignore`.
pub const IGNORE_FILE: &str = ".lintentignore";

pub struct Workspace {
    pub project: Project,
    pub env: Env,
    pub registry: Registry,
    pub rules: Vec<Rule>,
    pub cwd: PathBuf,
    exclude: GlobSet,
}

/// A file lintent can parse.
#[derive(Debug, Clone)]
pub struct SourceFile {
    /// Repo-relative, `/`-separated.
    pub path: String,
    pub absolute: PathBuf,
    pub language: String,
}

/// The result of walking the requested paths.
#[derive(Debug, Default)]
pub struct Discovery {
    pub files: Vec<SourceFile>,
    /// Files with no registered language, for `scopes --verbose`.
    pub unsupported: Vec<String>,
    /// Files skipped by the config's `exclude` globs.
    pub excluded: usize,
    /// Files hidden by `.lintentignore`; counted only for explicit paths.
    pub ignored: usize,
}

impl Discovery {
    /// Notes for explicit paths that reach files lintent then skips, so a
    /// `check some/dir` that judges nothing says why.
    pub fn skipped_notes(&self, paths: &[PathBuf]) -> Vec<String> {
        let mut notes = Vec::new();
        if paths.is_empty() {
            return notes;
        }
        if self.excluded > 0 {
            notes.push(format!(
                "note: {} file(s) under the given paths are excluded by `exclude` in lintent.toml",
                self.excluded
            ));
        }
        if self.ignored > 0 {
            notes.push(format!(
                "note: {} file(s) under the given paths are ignored by {IGNORE_FILE}",
                self.ignored
            ));
        }
        notes
    }
}

impl Workspace {
    /// `trust_grammars` is the `--trust-grammars` flag; the process
    /// environment can also grant it with `LINTENT_TRUST_GRAMMARS=1`.
    pub fn load(config: Option<&Path>, trust_grammars: bool) -> Result<Workspace> {
        let cwd = std::env::current_dir().context("reading the current directory")?;
        let mut env = Env::load(&cwd);
        let project = Project::locate(config, &cwd, &mut env)?;
        let trusted = trust_grammars
            || env
                .get_process("LINTENT_TRUST_GRAMMARS")
                .is_some_and(|v| v == "1");
        let registry = Registry::new(&project.config.languages, &project.root, trusted)?;
        let rules = load_rules(&project.rules_dir(), &registry)?;
        let exclude =
            build_globset(&project.config.exclude).context("invalid `exclude` in lintent.toml")?;
        Ok(Workspace {
            project,
            env,
            registry,
            rules,
            cwd,
            exclude,
        })
    }

    /// The rules named by `--rule` (all when none), erroring on unknown ids.
    pub fn select_rules(&self, ids: &[String]) -> Result<Vec<Rule>> {
        if ids.is_empty() {
            return Ok(self.rules.clone());
        }
        // `--rule a --rule a` asks once.
        let mut unique: Vec<&String> = Vec::new();
        for id in ids {
            if !unique.contains(&id) {
                unique.push(id);
            }
        }
        unique
            .into_iter()
            .map(|id| {
                self.rules
                    .iter()
                    .find(|rule| &rule.id == id)
                    .cloned()
                    .with_context(|| {
                        let known: Vec<&str> =
                            self.rules.iter().map(|rule| rule.id.as_str()).collect();
                        format!("unknown rule `{id}` (known: {})", known.join(", "))
                    })
            })
            .collect()
    }

    pub fn is_known_rule(&self, id: &str) -> bool {
        self.rules.iter().any(|rule| rule.id == id)
    }

    /// Walks `paths` (relative to the working directory; default the
    /// project root), honouring `.gitignore`, `.lintentignore` and the
    /// config's `exclude`.
    /// With `only`, keeps just those repo-relative paths.
    pub fn discover(
        &self,
        paths: &[PathBuf],
        only: Option<&BTreeSet<String>>,
    ) -> Result<Discovery> {
        let targets: Vec<PathBuf> = if paths.is_empty() {
            vec![self.project.root.clone()]
        } else {
            paths
                .iter()
                .map(|path| {
                    let absolute = self.cwd.join(path);
                    absolute
                        .canonicalize()
                        .with_context(|| format!("{} does not exist", path.display()))
                })
                .collect::<Result<_>>()?
        };

        let mut seen = BTreeSet::new();
        let mut discovery = Discovery::default();
        for target in &targets {
            if !target.starts_with(&self.project.root) {
                bail!(
                    "{} is outside the project root {}",
                    target.display(),
                    self.project.root.display()
                );
            }
            for (path, absolute) in self.walk(target, true, only)? {
                if !seen.insert(path.clone()) {
                    continue;
                }
                if self.exclude.is_match(&path) {
                    discovery.excluded += 1;
                    continue;
                }
                match self.registry.for_path(&absolute) {
                    Some(spec) => discovery.files.push(SourceFile {
                        path,
                        absolute,
                        language: spec.name.clone(),
                    }),
                    None => discovery.unsupported.push(path),
                }
            }
        }
        // Only explicit paths get the note, so only they pay for the second
        // walk: whatever it finds that the first did not, `.lintentignore` hid
        // (a `!pattern` there can also reveal files; those are not counted).
        if !paths.is_empty() {
            let mut hidden = BTreeSet::new();
            for target in &targets {
                for (path, _) in self.walk(target, false, only)? {
                    if !seen.contains(&path) && !self.exclude.is_match(&path) {
                        hidden.insert(path);
                    }
                }
            }
            discovery.ignored = hidden.len();
        }
        discovery.files.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(discovery)
    }

    /// The files under `target` as (repo-relative, absolute) paths, honouring
    /// `.gitignore` and, with `lintentignore`, `.lintentignore`.
    fn walk(
        &self,
        target: &Path,
        lintentignore: bool,
        only: Option<&BTreeSet<String>>,
    ) -> Result<Vec<(String, PathBuf)>> {
        // Hidden files are linted too (`.github/scripts`, dotfile configs);
        // the ignore files still apply, and `.git/` is never entered.
        let mut builder = WalkBuilder::new(target);
        builder
            .require_git(false)
            .hidden(false)
            .filter_entry(|entry| entry.file_name() != ".git")
            .sort_by_file_name(|a, b| a.cmp(b));
        if lintentignore {
            builder.add_custom_ignore_filename(IGNORE_FILE);
        }
        let mut files = Vec::new();
        for entry in builder.build() {
            let entry = entry.context("walking the project")?;
            if !entry.file_type().is_some_and(|kind| kind.is_file()) {
                continue;
            }
            let Some(path) = self.relative(entry.path()) else {
                continue;
            };
            if only.is_some_and(|only| !only.contains(&path)) {
                continue;
            }
            files.push((path, entry.into_path()));
        }
        Ok(files)
    }

    /// Reads and parses one file and binds its keep marks. `Ok(None)` for a
    /// file that is not text (a NUL byte or invalid UTF-8 — an MPEG-TS `.ts`,
    /// a Latin-1 `.py`): it is skipped with a warning, not an error.
    pub fn parse(&self, file: &SourceFile) -> Result<Option<(ParsedFile, Keeps)>> {
        let bytes =
            std::fs::read(&file.absolute).with_context(|| format!("reading {}", file.path))?;
        let source = match String::from_utf8(bytes) {
            Ok(source) if !source.contains('\0') => source,
            _ => {
                eprintln!("lintent: warning: skipping {}: not UTF-8 text", file.path);
                return Ok(None);
            }
        };
        let spec = self
            .registry
            .get(&file.language)
            .with_context(|| format!("no language {}", file.language))?;
        let parsed = parse_file(&file.path, spec, &source)
            .with_context(|| format!("parsing {}", file.path))?;
        let mut keeps = Keeps::parse(&parsed.comments, |id| self.is_known_rule(id));
        keeps.bind(&parsed.units, &parsed.filler_lines);
        Ok(Some((parsed, keeps)))
    }

    pub fn relative(&self, path: &Path) -> Option<String> {
        let relative = path.strip_prefix(&self.project.root).ok()?;
        let parts: Vec<String> = relative
            .components()
            .map(|part| part.as_os_str().to_string_lossy().into_owned())
            .collect();
        Some(parts.join("/"))
    }
}
