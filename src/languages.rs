//! The language registry: which grammar parses a file, and whether a tags
//! query or the node-name heuristic finds its scopes.
//!
//! Two sources feed it. Built-ins are grammar crates compiled into the
//! binary, each with the tags query its crate ships (plus a few lintent
//! additions). Anything else tree-sitter can parse is added at runtime with a
//! `[languages.<name>]` table in `lintent.toml` — see [`crate::grammar`].

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::path::Path;
use std::sync::OnceLock;

use anyhow::{anyhow, bail, Context, Result};
use tree_sitter::Query;
use tree_sitter_language::LanguageFn;

use crate::config::LanguageConfig;
use crate::grammar;

/// A grammar compiled into the binary.
struct BuiltIn {
    name: &'static str,
    extensions: &'static [&'static str],
    grammar: LanguageFn,
    /// Concatenated into one query. Empty = no tags query (heuristic).
    tags: &'static [&'static str],
}

const TS_EXTRA: &str = include_str!("../queries/typescript-extra.scm");
const JS_EXTRA: &str = include_str!("../queries/javascript-extra.scm");
const SCALA_TAGS: &str = include_str!("../queries/scala-tags.scm");

/// The TypeScript crate's tags query only covers TypeScript-only syntax and
/// is meant to be layered on the JavaScript one, as the tree-sitter CLI does.
const ECMASCRIPT_TS_TAGS: &[&str] = &[
    tree_sitter_javascript::TAGS_QUERY,
    tree_sitter_typescript::TAGS_QUERY,
    TS_EXTRA,
];

const BUILT_INS: &[BuiltIn] = &[
    BuiltIn {
        name: "typescript",
        extensions: &["ts", "mts", "cts"],
        grammar: tree_sitter_typescript::LANGUAGE_TYPESCRIPT,
        tags: ECMASCRIPT_TS_TAGS,
    },
    BuiltIn {
        name: "tsx",
        extensions: &["tsx"],
        grammar: tree_sitter_typescript::LANGUAGE_TSX,
        tags: ECMASCRIPT_TS_TAGS,
    },
    BuiltIn {
        name: "javascript",
        extensions: &["js", "mjs", "cjs", "jsx"],
        grammar: tree_sitter_javascript::LANGUAGE,
        tags: &[tree_sitter_javascript::TAGS_QUERY, JS_EXTRA],
    },
    BuiltIn {
        name: "rust",
        extensions: &["rs"],
        grammar: tree_sitter_rust::LANGUAGE,
        tags: &[tree_sitter_rust::TAGS_QUERY],
    },
    BuiltIn {
        name: "python",
        extensions: &["py", "pyi"],
        grammar: tree_sitter_python::LANGUAGE,
        tags: &[tree_sitter_python::TAGS_QUERY],
    },
    BuiltIn {
        name: "go",
        extensions: &["go"],
        grammar: tree_sitter_go::LANGUAGE,
        tags: &[tree_sitter_go::TAGS_QUERY],
    },
    BuiltIn {
        name: "java",
        extensions: &["java"],
        grammar: tree_sitter_java::LANGUAGE,
        tags: &[tree_sitter_java::TAGS_QUERY],
    },
    BuiltIn {
        name: "c",
        extensions: &["c", "h"],
        grammar: tree_sitter_c::LANGUAGE,
        tags: &[tree_sitter_c::TAGS_QUERY],
    },
    BuiltIn {
        name: "cpp",
        extensions: &["cc", "cpp", "cxx", "c++", "hh", "hpp", "hxx"],
        grammar: tree_sitter_cpp::LANGUAGE,
        tags: &[tree_sitter_cpp::TAGS_QUERY],
    },
    BuiltIn {
        name: "csharp",
        extensions: &["cs"],
        grammar: tree_sitter_c_sharp::LANGUAGE,
        tags: &[tree_sitter_c_sharp::TAGS_QUERY],
    },
    BuiltIn {
        name: "ruby",
        extensions: &["rb", "rake", "gemspec"],
        grammar: tree_sitter_ruby::LANGUAGE,
        tags: &[tree_sitter_ruby::TAGS_QUERY],
    },
    BuiltIn {
        name: "php",
        extensions: &["php"],
        grammar: tree_sitter_php::LANGUAGE_PHP,
        tags: &[tree_sitter_php::TAGS_QUERY],
    },
    BuiltIn {
        name: "swift",
        extensions: &["swift"],
        grammar: tree_sitter_swift::LANGUAGE,
        tags: &[tree_sitter_swift::TAGS_QUERY],
    },
    BuiltIn {
        name: "scala",
        extensions: &["scala", "sc"],
        grammar: tree_sitter_scala::LANGUAGE,
        tags: &[SCALA_TAGS],
    },
    BuiltIn {
        name: "lua",
        extensions: &["lua"],
        grammar: tree_sitter_lua::LANGUAGE,
        tags: &[tree_sitter_lua::TAGS_QUERY],
    },
    BuiltIn {
        name: "elixir",
        extensions: &["ex", "exs"],
        grammar: tree_sitter_elixir::LANGUAGE,
        tags: &[tree_sitter_elixir::TAGS_QUERY],
    },
    BuiltIn {
        name: "kotlin",
        extensions: &["kt", "kts"],
        grammar: tree_sitter_kotlin_ng::LANGUAGE,
        tags: &[],
    },
    BuiltIn {
        name: "bash",
        extensions: &["sh", "bash"],
        grammar: tree_sitter_bash::LANGUAGE,
        tags: &[],
    },
];

/// Where a language came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    BuiltIn,
    Config,
}

impl fmt::Display for Origin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(match self {
            Origin::BuiltIn => "built-in",
            Origin::Config => "config",
        })
    }
}

enum GrammarSource {
    Static(LanguageFn),
    /// Resolved (and compiled, if needed) on first use, so a project with a
    /// custom grammar pays nothing unless a matching file is actually linted.
    Runtime(Box<LanguageConfig>),
}

enum TagsSource {
    Static(&'static [&'static str]),
    File(std::path::PathBuf),
    /// `queries/tags.scm` next to a runtime grammar, if it exists once the
    /// grammar is available.
    Conventional,
}

/// A grammar ready to parse, with its compiled tags query if it has one.
pub struct Loaded {
    pub grammar: tree_sitter::Language,
    pub tags: Option<Query>,
}

/// One language lintent can extract scopes from.
pub struct LanguageSpec {
    pub name: String,
    pub extensions: Vec<String>,
    pub origin: Origin,
    grammar: GrammarSource,
    tags: TagsSource,
    root: std::path::PathBuf,
    /// Whether runtime grammar code may be compiled and loaded (see
    /// [`crate::grammar`]); built-ins are always trusted.
    trusted: bool,
    loaded: OnceLock<std::result::Result<Loaded, String>>,
}

impl LanguageSpec {
    /// Loads the grammar and compiles the tags query once per run.
    pub fn load(&self) -> Result<&Loaded> {
        self.loaded
            .get_or_init(|| self.load_uncached().map_err(|error| format!("{error:#}")))
            .as_ref()
            .map_err(|error| anyhow!("language {}: {error}", self.name))
    }

    /// A short description of how scopes are found, without loading anything.
    pub fn strategy(&self) -> &'static str {
        match &self.tags {
            TagsSource::Static(parts) if !parts.is_empty() => "tags",
            TagsSource::File(_) => "tags",
            TagsSource::Conventional => {
                "tags if the grammar ships queries/tags.scm, else heuristic"
            }
            _ => "heuristic",
        }
    }

    pub fn grammar_description(&self) -> String {
        match &self.grammar {
            GrammarSource::Static(_) => "compiled in".to_string(),
            GrammarSource::Runtime(config) if self.trusted => config.describe(),
            GrammarSource::Runtime(config) => {
                format!("{} (untrusted: needs --trust-grammars)", config.describe())
            }
        }
    }

    fn load_uncached(&self) -> Result<Loaded> {
        let (grammar, grammar_dir) = match &self.grammar {
            GrammarSource::Static(function) => (tree_sitter::Language::new(*function), None),
            GrammarSource::Runtime(_) if !self.trusted => bail!(
                "lintent.toml configures a runtime grammar ({}), which is native code compiled and run \
                 inside lintent. A linter must not execute code from an untrusted change, so this is off \
                 by default: pass --trust-grammars or set LINTENT_TRUST_GRAMMARS=1 once you have reviewed it",
                self.grammar_description()
            ),
            GrammarSource::Runtime(config) => {
                let loaded = grammar::load(&self.name, config, &self.root)?;
                (loaded.language, loaded.grammar_dir)
            }
        };
        let tags_text = match &self.tags {
            TagsSource::Static([]) => None,
            TagsSource::Static(parts) => Some(parts.join("\n")),
            TagsSource::File(path) => Some(
                std::fs::read_to_string(path)
                    .with_context(|| format!("reading tags query {}", path.display()))?,
            ),
            TagsSource::Conventional => grammar_dir
                .map(|dir| dir.join("queries").join("tags.scm"))
                .filter(|path| path.is_file())
                .map(std::fs::read_to_string)
                .transpose()
                .context("reading the grammar's queries/tags.scm")?,
        };
        let tags = tags_text
            .map(|text| {
                Query::new(&grammar, &text)
                    .map_err(|error| anyhow!("tags query does not compile: {error}"))
            })
            .transpose()?;
        Ok(Loaded { grammar, tags })
    }
}

/// All languages for one run.
pub struct Registry {
    specs: Vec<LanguageSpec>,
    by_extension: HashMap<String, usize>,
}

impl Registry {
    /// Only the built-in grammars.
    pub fn builtin() -> Registry {
        Registry::new(&BTreeMap::new(), Path::new("."), false)
            .expect("built-in languages are valid")
    }

    /// Built-ins, then `[languages.*]` from the config. A config entry with a
    /// built-in's name and no grammar only adjusts it (extra extensions, a
    /// replacement tags query); with a grammar it replaces it.
    pub fn new(
        configured: &BTreeMap<String, LanguageConfig>,
        root: &Path,
        trust_grammars: bool,
    ) -> Result<Registry> {
        let mut specs: Vec<LanguageSpec> = BUILT_INS
            .iter()
            .map(|builtin| LanguageSpec {
                name: builtin.name.to_string(),
                extensions: builtin.extensions.iter().map(|e| e.to_string()).collect(),
                origin: Origin::BuiltIn,
                grammar: GrammarSource::Static(builtin.grammar),
                tags: TagsSource::Static(builtin.tags),
                root: root.to_path_buf(),
                trusted: true,
                loaded: OnceLock::new(),
            })
            .collect();

        // Config-listed extensions are applied last so they override built-ins.
        let mut overrides: Vec<(String, String)> = Vec::new();
        for (name, config) in configured {
            if !crate::rules::is_kebab_case(name) {
                bail!("[languages.{name}]: language names must be lower-case kebab-case");
            }
            let extensions: Vec<String> = config
                .extensions
                .iter()
                .map(|e| normalize_extension(e))
                .collect();
            overrides.extend(extensions.iter().map(|e| (e.clone(), name.clone())));
            let tags_file = config.tags_query.as_ref().map(|path| root.join(path));
            let existing = specs.iter().position(|spec| &spec.name == name);
            if config.has_grammar() {
                let spec = LanguageSpec {
                    name: name.clone(),
                    extensions,
                    origin: Origin::Config,
                    grammar: GrammarSource::Runtime(Box::new(config.clone())),
                    tags: match tags_file {
                        Some(path) => TagsSource::File(path),
                        None => TagsSource::Conventional,
                    },
                    root: root.to_path_buf(),
                    trusted: trust_grammars,
                    loaded: OnceLock::new(),
                };
                if spec.extensions.is_empty() {
                    bail!("[languages.{name}]: `extensions` must list at least one file extension");
                }
                match existing {
                    Some(index) => specs[index] = spec,
                    None => specs.push(spec),
                }
            } else {
                let Some(index) = existing else {
                    bail!(
                        "[languages.{name}]: not a built-in language, so it needs `grammar` (a compiled \
                         library or a grammar directory) or `repo` (a git URL)"
                    );
                };
                let spec = &mut specs[index];
                spec.extensions.extend(extensions);
                spec.origin = Origin::Config;
                if let Some(path) = tags_file {
                    spec.tags = TagsSource::File(path);
                }
            }
        }

        let mut by_extension = HashMap::new();
        for (index, spec) in specs.iter().enumerate() {
            for extension in &spec.extensions {
                by_extension.entry(extension.clone()).or_insert(index);
            }
        }
        for (extension, name) in overrides {
            if let Some(index) = specs.iter().position(|spec| spec.name == name) {
                by_extension.insert(extension, index);
            }
        }
        Ok(Registry {
            specs,
            by_extension,
        })
    }

    pub fn for_path(&self, path: &Path) -> Option<&LanguageSpec> {
        let extension = path.extension()?.to_str()?.to_ascii_lowercase();
        self.by_extension
            .get(&extension)
            .map(|&index| &self.specs[index])
    }

    pub fn get(&self, name: &str) -> Option<&LanguageSpec> {
        self.specs.iter().find(|spec| spec.name == name)
    }

    pub fn specs(&self) -> &[LanguageSpec] {
        &self.specs
    }

    pub fn names(&self) -> Vec<&str> {
        self.specs.iter().map(|spec| spec.name.as_str()).collect()
    }
}

fn normalize_extension(extension: &str) -> String {
    extension
        .trim()
        .trim_start_matches('.')
        .to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_builtin_grammar_loads_and_its_tags_query_compiles() {
        let registry = Registry::builtin();
        for spec in registry.specs() {
            let loaded = spec.load().unwrap_or_else(|error| panic!("{error:#}"));
            let expects_tags = spec.strategy() == "tags";
            assert_eq!(loaded.tags.is_some(), expects_tags, "{}", spec.name);
        }
        assert!(registry.specs().len() >= 18);
    }

    #[test]
    fn extensions_map_to_languages() {
        let registry = Registry::builtin();
        let name = |path: &str| {
            registry
                .for_path(Path::new(path))
                .map(|spec| spec.name.as_str())
        };
        assert_eq!(name("a/b.tsx"), Some("tsx"));
        assert_eq!(name("a/b.JSX"), Some("javascript"));
        assert_eq!(name("x.kt"), Some("kotlin"));
        assert_eq!(name("README.md"), None);
    }

    #[test]
    fn config_can_extend_a_builtin() {
        let mut configured = BTreeMap::new();
        configured.insert(
            "typescript".to_string(),
            LanguageConfig {
                extensions: vec![".tsx".to_string()],
                ..LanguageConfig::default()
            },
        );
        let registry = Registry::new(&configured, Path::new("."), false).unwrap();
        assert_eq!(
            registry.for_path(Path::new("a.tsx")).unwrap().name,
            "typescript"
        );
        assert_eq!(registry.get("typescript").unwrap().origin, Origin::Config);
    }

    #[test]
    fn unknown_language_without_grammar_is_rejected() {
        let mut configured = BTreeMap::new();
        configured.insert("zig".to_string(), LanguageConfig::default());
        let error = Registry::new(&configured, Path::new("."), false)
            .err()
            .unwrap();
        assert!(format!("{error}").contains("needs `grammar`"));
    }
}
