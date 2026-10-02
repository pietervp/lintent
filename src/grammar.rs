//! Runtime grammars: any tree-sitter grammar, configured in `lintent.toml`.
//!
//! ```toml
//! [languages.haskell]
//! extensions = ["hs"]
//! repo = "https://github.com/tree-sitter/tree-sitter-haskell"  # or: grammar = "<dir or .so/.dylib>"
//! rev = "<40-char SHA>"    # required with repo: a full commit id, never a movable tag
//! subdir = ""              # optional: grammar directory inside the repo
//! tags_query = "queries/haskell-tags.scm"  # optional; else the grammar's queries/tags.scm, else heuristic
//! symbol = "tree_sitter_haskell"           # optional; default tree_sitter_<name from grammar.json>
//! ```
//!
//! **Trust.** A grammar is native code: compiling it runs the C compiler on
//! the repository's files and loading it runs that code inside lintent. A
//! linter must not execute code just because a pull request edited
//! `lintent.toml`, so runtime grammars load only when the person running
//! lintent opts in with `--trust-grammars` or `LINTENT_TRUST_GRAMMARS=1` in
//! the process environment (a dotenv file does not count).
//!
//! A grammar directory (or cloned repo) must contain the generated
//! `src/parser.c`, as published grammar repositories do. It is compiled with
//! the system C compiler (`$CC`, default `cc`; `$CXX` for a C++ scanner) and
//! the shared library is cached under the user cache directory, so only the
//! first run pays for it. A precompiled library is loaded directly.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};

use crate::config::LanguageConfig;
use crate::git;

pub struct LoadedGrammar {
    pub language: tree_sitter::Language,
    /// The grammar's source directory, used to find `queries/tags.scm`.
    pub grammar_dir: Option<PathBuf>,
}

/// Resolves a configured grammar to a loaded tree-sitter language.
pub fn load(name: &str, config: &LanguageConfig, root: &Path) -> Result<LoadedGrammar> {
    if let Some(grammar) = &config.grammar {
        let path = root.join(grammar);
        if path.is_dir() {
            let language = compile_directory(name, &path, config.symbol.as_deref())?;
            return Ok(LoadedGrammar {
                language,
                grammar_dir: Some(path),
            });
        }
        if path.is_file() {
            let symbol = config
                .symbol
                .clone()
                .unwrap_or_else(|| format!("tree_sitter_{}", name.replace('-', "_")));
            return Ok(LoadedGrammar {
                language: load_library(&path, &symbol)?,
                grammar_dir: None,
            });
        }
        bail!("grammar {} does not exist", path.display());
    }
    if let Some(repo) = &config.repo {
        let checkout = clone(name, repo, config.rev.as_deref())?;
        let dir = match &config.subdir {
            Some(subdir) => checkout.join(subdir),
            None => checkout,
        };
        let language = compile_directory(name, &dir, config.symbol.as_deref())?;
        return Ok(LoadedGrammar {
            language,
            grammar_dir: Some(dir),
        });
    }
    bail!("no `grammar` or `repo` configured")
}

/// `$LINTENT_CACHE_DIR`, else the platform cache dir + `/lintent`.
pub fn cache_root() -> PathBuf {
    if let Some(dir) = std::env::var_os("LINTENT_CACHE_DIR").filter(|v| !v.is_empty()) {
        return PathBuf::from(dir);
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    if cfg!(target_os = "macos") {
        return home.join("Library/Caches/lintent");
    }
    std::env::var_os("XDG_CACHE_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".cache"))
        .join("lintent")
}

/// Compiles `dir/src/parser.c` (+ scanner) once and loads the result.
fn compile_directory(
    name: &str,
    dir: &Path,
    symbol: Option<&str>,
) -> Result<tree_sitter::Language> {
    let src = dir.join("src");
    if !src.join("parser.c").is_file() {
        bail!(
            "{} has no src/parser.c — point `grammar` at a generated grammar directory \
             (run `tree-sitter generate` in it first)",
            dir.display()
        );
    }
    let dir = dir
        .canonicalize()
        .with_context(|| format!("resolving {}", dir.display()))?;
    let src = dir.join("src");
    // One library per grammar source, so two projects defining the same
    // language name from different grammars never share a binary.
    let library = cache_root()
        .join("grammars")
        .join(format!("{name}-{}", short_hash(&[&dir.to_string_lossy()])))
        .join(format!("{name}.{}", std::env::consts::DLL_EXTENSION));
    let sources = grammar_sources(&src);
    if needs_build(&library, &sources) {
        build_library(&src, &sources, &library)
            .with_context(|| format!("compiling the {name} grammar in {}", dir.display()))?;
    }
    let symbol = match symbol {
        Some(symbol) => symbol.to_string(),
        None => format!(
            "tree_sitter_{}",
            grammar_json_name(&src)
                .unwrap_or_else(|| name.to_string())
                .replace('-', "_")
        ),
    };
    load_library(&library, &symbol)
}

/// `parser.c` plus an external scanner (`scanner.c`, or C++ `scanner.cc`).
fn grammar_sources(src: &Path) -> Vec<PathBuf> {
    ["parser.c", "scanner.c", "scanner.cc"]
        .iter()
        .map(|file| src.join(file))
        .filter(|path| path.is_file())
        .collect()
}

fn needs_build(library: &Path, sources: &[PathBuf]) -> bool {
    let Ok(built) = fs::metadata(library).and_then(|meta| meta.modified()) else {
        return true;
    };
    sources.iter().any(|source| {
        fs::metadata(source)
            .and_then(|meta| meta.modified())
            .is_ok_and(|changed| changed > built)
    })
}

/// Builds the shared library with the system compiler (`$CC`, default `cc`;
/// `$CXX`/`c++` when the scanner is C++). The output is written to a temp
/// name and renamed, so concurrent runs never load a half-written library.
fn build_library(src: &Path, sources: &[PathBuf], library: &Path) -> Result<()> {
    let dir = library.parent().expect("library path has a directory");
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let temp = library.with_extension(format!("{}.tmp", std::process::id()));
    let cpp = sources
        .iter()
        .any(|path| path.extension().is_some_and(|e| e == "cc"));
    let compiler = if cpp {
        std::env::var("CXX").unwrap_or_else(|_| "c++".to_string())
    } else {
        std::env::var("CC").unwrap_or_else(|_| "cc".to_string())
    };
    let mut command = std::process::Command::new(&compiler);
    command
        .args(["-shared", "-fPIC", "-O2", "-w"])
        .arg("-I")
        .arg(src);
    for source in sources {
        let language = if source.extension().is_some_and(|e| e == "cc") {
            "c++"
        } else {
            "c"
        };
        command.args(["-x", language]).arg(source);
    }
    command.arg("-o").arg(&temp);
    let output = command.output().with_context(|| {
        format!("running the C compiler `{compiler}` (set CC to choose another)")
    })?;
    if !output.status.success() {
        fs::remove_file(&temp).ok();
        bail!(
            "`{compiler}` failed:\n{}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    fs::rename(&temp, library).with_context(|| format!("installing {}", library.display()))?;
    Ok(())
}

fn grammar_json_name(src: &Path) -> Option<String> {
    let text = fs::read_to_string(src.join("grammar.json")).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    value.get("name")?.as_str().map(str::to_string)
}

/// Loads a precompiled grammar library and resolves its language function.
fn load_library(path: &Path, symbol: &str) -> Result<tree_sitter::Language> {
    // SAFETY: loading a library runs its initialisers. The path comes from
    // the project's own lintent.toml, which is as trusted as the code it lints.
    let library = unsafe { libloading::Library::new(path) }
        .with_context(|| format!("loading grammar library {}", path.display()))?;
    // SAFETY: tree-sitter grammars export `const TSLanguage *tree_sitter_<name>(void)`.
    let function = unsafe {
        let symbol: libloading::Symbol<unsafe extern "C" fn() -> *const ()> = library
            .get(symbol.as_bytes())
            .with_context(|| format!("{} does not export {symbol}", path.display()))?;
        *symbol
    };
    // The language's tables live in the library, so it must never unload.
    std::mem::forget(library);
    // SAFETY: `function` is a tree-sitter language function (checked by name
    // above); the parser rejects an incompatible ABI version at set_language.
    let language_fn = unsafe { tree_sitter_language::LanguageFn::from_raw(function) };
    Ok(tree_sitter::Language::new(language_fn))
}

/// Clones `repo` at `rev` (a full SHA, checked by the config) into the cache
/// once; later runs reuse the checkout.
fn clone(name: &str, repo: &str, rev: Option<&str>) -> Result<PathBuf> {
    let repos = cache_root().join("repos");
    let target = repos.join(format!("{name}-{}", short_hash(&[repo, rev.unwrap_or("")])));
    if target.join(".git").exists() {
        return Ok(target);
    }
    fs::create_dir_all(&repos).with_context(|| format!("creating {}", repos.display()))?;
    // Clone next to the target and rename, so an interrupted clone is never
    // mistaken for a complete one.
    let staging = repos.join(format!(".staging-{}-{}", name, std::process::id()));
    if staging.exists() {
        fs::remove_dir_all(&staging).ok();
    }
    let result = clone_into(&repos, &staging, repo, rev);
    if let Err(error) = result {
        fs::remove_dir_all(&staging).ok();
        return Err(error);
    }
    if let Err(error) = fs::rename(&staging, &target) {
        fs::remove_dir_all(&staging).ok();
        // Another lintent process finished the same clone first.
        if target.join(".git").exists() {
            return Ok(target);
        }
        return Err(error).with_context(|| format!("moving the {repo} checkout into place"));
    }
    Ok(target)
}

fn clone_into(repos: &Path, staging: &Path, repo: &str, rev: Option<&str>) -> Result<()> {
    let staging_str = staging.to_string_lossy().to_string();
    // `--` so a repo value can never be read as an option.
    git::run(
        repos,
        &[
            "clone",
            "--quiet",
            "--no-checkout",
            "--",
            repo,
            &staging_str,
        ],
    )
    .with_context(|| format!("cloning {repo}"))?;
    let rev = rev.context("`repo` needs `rev`")?;
    git::run(
        staging,
        &[
            "-c",
            "advice.detachedHead=false",
            "checkout",
            "--quiet",
            "--detach",
            rev,
        ],
    )
    .with_context(|| format!("checking out {rev} in {repo}"))?;
    Ok(())
}

/// Stable across toolchains (unlike `DefaultHasher`), so cache paths survive
/// a Rust upgrade.
fn short_hash(parts: &[&str]) -> String {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(part.as_bytes());
        hasher.update([0]);
    }
    hasher
        .finalize()
        .iter()
        .take(8)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
