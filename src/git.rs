//! Git plumbing for `--changed` and for cloning runtime grammars.
//!
//! Git child processes are started without the variables that redirect git
//! to another repository. When lintent runs inside a git hook, git exports
//! `GIT_DIR`, `GIT_INDEX_FILE` and friends; inheriting them would make every
//! command here act on the hook's repository instead of the one in
//! `current_dir`. Other `GIT_*` settings (SSH command, config, trace) are
//! the user's and pass through.

use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context, Result};

/// The variables that point git at a specific repository or work tree.
const REPOSITORY_VARIABLES: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_COMMON_DIR",
    "GIT_PREFIX",
    "GIT_NAMESPACE",
];

/// A `git` command that acts on the repository at `dir`.
pub fn command(dir: &Path) -> Command {
    let mut command = Command::new("git");
    command.current_dir(dir);
    for variable in REPOSITORY_VARIABLES {
        command.env_remove(variable);
    }
    command
}

/// Runs git and returns stdout; a non-zero exit is an error carrying stderr.
pub fn run(dir: &Path, args: &[&str]) -> Result<String> {
    let output = command(dir)
        .args(args)
        .output()
        .with_context(|| format!("running git {}", args.join(" ")))?;
    if !output.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Files changed since the merge-base with `base` (default `origin/main`,
/// falling back to `main`), plus uncommitted and untracked files. Paths are
/// relative to `root`, deleted files are left out.
pub fn changed_files(root: &Path, base: Option<&str>) -> Result<BTreeSet<String>> {
    let candidates: Vec<&str> = match base {
        Some(base) => vec![base],
        None => vec!["origin/main", "main"],
    };
    let merge_base = candidates
        .iter()
        .find_map(|candidate| run(root, &["merge-base", candidate, "HEAD"]).ok())
        .map(|output| output.trim().to_string())
        .with_context(|| {
            format!(
                "no merge-base with {} (pass --base <ref>)",
                candidates.join(" or ")
            )
        })?;

    // `--relative` limits the diff to `root` and makes paths relative to it,
    // which matters when the lintent project is a subdirectory of the repo.
    let diff = run(
        root,
        &[
            "diff",
            "--name-only",
            "-z",
            "--relative",
            "--diff-filter=ACMRT",
            &merge_base,
        ],
    )?;
    let untracked = run(root, &["ls-files", "--others", "--exclude-standard", "-z"])?;
    Ok(diff
        .split('\0')
        .chain(untracked.split('\0'))
        .filter(|path| !path.is_empty())
        .map(str::to_string)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn git(dir: &Path, args: &[&str]) {
        let mut full = vec![
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "commit.gpgsign=false",
        ];
        full.extend_from_slice(args);
        run(dir, &full).unwrap();
    }

    #[test]
    fn changed_files_since_merge_base() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        git(root, &["init", "--quiet", "--initial-branch=main"]);
        fs::create_dir_all(root.join("app")).unwrap();
        fs::write(root.join("a.ts"), "1").unwrap();
        fs::write(root.join("gone.ts"), "1").unwrap();
        fs::write(root.join("app/same.ts"), "1").unwrap();
        git(root, &["add", "."]);
        git(root, &["commit", "--quiet", "-m", "base"]);
        git(root, &["checkout", "--quiet", "-b", "feature"]);
        fs::write(root.join("b.ts"), "1").unwrap();
        git(root, &["add", "b.ts"]);
        git(root, &["commit", "--quiet", "-m", "feature"]);
        fs::write(root.join("a.ts"), "2").unwrap();
        fs::write(root.join("new.ts"), "1").unwrap();
        fs::remove_file(root.join("gone.ts")).unwrap();

        let changed = changed_files(root, None).unwrap();
        let expected: BTreeSet<String> = ["a.ts", "b.ts", "new.ts"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(changed, expected);

        let nested = changed_files(&root.join("app"), Some("main")).unwrap();
        assert!(nested.is_empty());
        assert!(changed_files(root, Some("no-such-ref")).is_err());
    }
}
