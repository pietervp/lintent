//! The verdict cache (`.lintent/cache.json`).
//!
//! Keys are per *question* — one (unit, rule) pair — so adding a rule to a
//! file does not invalidate the verdicts of the rules already there. A key
//! covers everything that can change the answer: where the question goes
//! (provider, base URL, model), how it is asked (`isolate_rules`, since a
//! question asked next to others is a different request), the rule file's
//! exact text, and the unit — including its path, which is sent as state.
//!
//! Several lintent runs may share one cache (parallel CI jobs, an agent and
//! an editor). Saving therefore re-reads the file under a lock file and
//! merges, so no run drops another's verdicts; entries unused for 30 days
//! are pruned on the way.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::extract::Unit;
use crate::jev::{Answer, Choice};
use crate::rules::Rule;

/// Bump when the request shape or its meaning changes.
pub const CACHE_VERSION: &str = "lintent-cache-v2";
const PRUNE_AFTER_SECS: u64 = 30 * 24 * 3600;
/// Re-stamping a read entry at most daily keeps cache-only runs from
/// rewriting the file every time.
const TOUCH_EVERY_SECS: u64 = 24 * 3600;
/// A lock older than this belongs to a crashed run.
const STALE_LOCK: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
struct Entry {
    choice: Choice,
    confidence: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    p_fail: Option<f64>,
    /// Unix seconds of the last write or (daily) use.
    #[serde(default)]
    touched: u64,
}

impl Entry {
    fn answer(&self) -> Answer {
        Answer {
            choice: self.choice,
            confidence: self.confidence,
            p_fail: self.p_fail,
        }
    }
}

pub struct Cache {
    path: PathBuf,
    entries: BTreeMap<String, Entry>,
    fresh: BTreeMap<String, Entry>,
    used: std::cell::RefCell<BTreeSet<String>>,
}

impl Cache {
    /// Loads the cache; a missing file is empty, a corrupt one is discarded
    /// with a warning (it is only a cache).
    pub fn load(path: &Path) -> Cache {
        Cache {
            path: path.to_path_buf(),
            entries: read_entries(path),
            fresh: BTreeMap::new(),
            used: Default::default(),
        }
    }

    /// An in-memory cache that is never written (for `--no-cache`).
    pub fn disabled() -> Cache {
        Cache {
            path: PathBuf::new(),
            entries: BTreeMap::new(),
            fresh: BTreeMap::new(),
            used: Default::default(),
        }
    }

    pub fn get(&self, key: &str) -> Option<Answer> {
        let entry = self.fresh.get(key).or_else(|| self.entries.get(key))?;
        self.used.borrow_mut().insert(key.to_string());
        Some(entry.answer())
    }

    pub fn put(&mut self, key: String, answer: Answer) {
        let entry = Entry {
            choice: answer.choice,
            confidence: answer.confidence,
            p_fail: answer.p_fail,
            touched: now(),
        };
        self.fresh.insert(key, entry);
    }

    /// Merges this run's verdicts into the file on disk, under a lock.
    pub fn save(&self) -> Result<()> {
        if self.path.as_os_str().is_empty() {
            return Ok(());
        }
        let now = now();
        let used = self.used.borrow();
        let stale_touches = used.iter().any(|key| {
            self.entries
                .get(key)
                .is_some_and(|entry| entry.touched + TOUCH_EVERY_SECS < now)
        });
        if self.fresh.is_empty() && !stale_touches {
            return Ok(());
        }
        if let Some(dir) = self.path.parent() {
            fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let _lock = Lock::acquire(&self.path.with_extension("json.lock"))?;
        let mut merged = read_entries(&self.path);
        for key in used.iter() {
            if let Some(entry) = merged.get_mut(key) {
                entry.touched = entry.touched.max(now);
            }
        }
        merged.extend(self.fresh.iter().map(|(key, entry)| (key.clone(), *entry)));
        merged.retain(|_, entry| entry.touched + PRUNE_AFTER_SECS >= now);

        let temp = self
            .path
            .with_extension(format!("json.{}.tmp", std::process::id()));
        let text = serde_json::to_string_pretty(&merged)?;
        fs::write(&temp, text).with_context(|| format!("writing {}", temp.display()))?;
        fs::rename(&temp, &self.path)
            .with_context(|| format!("writing {}", self.path.display()))?;
        Ok(())
    }
}

fn read_entries(path: &Path) -> BTreeMap<String, Entry> {
    match fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_else(|error| {
            eprintln!(
                "lintent: warning: ignoring unreadable cache {}: {error}",
                path.display()
            );
            BTreeMap::new()
        }),
        Err(_) => BTreeMap::new(),
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or_default()
}

/// An exclusive lock file, removed on drop.
struct Lock {
    path: PathBuf,
}

impl Lock {
    fn acquire(path: &Path) -> Result<Lock> {
        for _ in 0..100 {
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)
            {
                Ok(_) => {
                    return Ok(Lock {
                        path: path.to_path_buf(),
                    })
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    let stale = fs::metadata(path)
                        .and_then(|meta| meta.modified())
                        .ok()
                        .and_then(|modified| modified.elapsed().ok())
                        .is_some_and(|age| age > STALE_LOCK);
                    if stale {
                        fs::remove_file(path).ok();
                    } else {
                        std::thread::sleep(Duration::from_millis(50));
                    }
                }
                Err(error) => {
                    return Err(error).with_context(|| format!("locking {}", path.display()))
                }
            }
        }
        bail!("timed out waiting for the cache lock {}", path.display())
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        fs::remove_file(&self.path).ok();
    }
}

/// sha256 over the scope (provider, base URL, model, `isolate_rules` — see
/// `Config::cache_scope`), the rule file's exact text and everything about
/// the unit that is sent. Parts are NUL-separated so no two different inputs
/// concatenate to the same bytes.
pub fn question_key(scope: &str, rule: &Rule, unit: &Unit) -> String {
    let mut hasher = Sha256::new();
    let parent = match &unit.parent_source {
        Some(parent) => format!("some:{parent}"),
        None => "none".to_string(),
    };
    let parts: [&str; 9] = [
        CACHE_VERSION,
        scope,
        &rule.raw,
        unit.kind.as_str(),
        &unit.name,
        &unit.language,
        &unit.path,
        &unit.source,
        &parent,
    ];
    for part in parts {
        hasher.update(part.as_bytes());
        hasher.update([0]);
    }
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extract::ScopeKind;
    use crate::rules::test_rule;

    fn unit() -> Unit {
        Unit {
            kind: ScopeKind::Function,
            name: "f".into(),
            language: "typescript".into(),
            path: "src/a.ts".into(),
            start_line: 1,
            end_line: 1,
            source: "function f() {}".into(),
            parent_source: None,
            truncated: false,
        }
    }

    fn answer(choice: Choice) -> Answer {
        Answer {
            choice,
            confidence: 0.9,
            p_fail: Some(0.9),
        }
    }

    #[test]
    fn key_changes_with_every_input_that_is_sent() {
        let rule = test_rule("demo", "");
        let base = question_key("s", &rule, &unit());
        assert_eq!(base, question_key("s", &rule, &unit()));
        assert_ne!(base, question_key("other-scope", &rule, &unit()));
        assert_ne!(
            base,
            question_key("s", &test_rule("demo", "why = \"x\""), &unit())
        );
        let mut changed = unit();
        changed.source.push(' ');
        assert_ne!(base, question_key("s", &rule, &changed));
        let mut with_parent = unit();
        with_parent.parent_source = Some(String::new());
        assert_ne!(
            base,
            question_key("s", &rule, &with_parent),
            "absent and empty parent differ"
        );
        let mut moved = unit();
        moved.path = "src/b.ts".into();
        assert_ne!(
            base,
            question_key("s", &rule, &moved),
            "the path is part of the state"
        );
        let mut shifted = unit();
        shifted.start_line = 40;
        assert_eq!(
            base,
            question_key("s", &rule, &shifted),
            "line numbers are not sent"
        );
    }

    #[test]
    fn round_trips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".lintent/cache.json");
        let mut cache = Cache::load(&path);
        assert!(cache.get("k").is_none());
        cache.put("k".into(), answer(Choice::Fail));
        cache.save().unwrap();
        assert_eq!(Cache::load(&path).get("k"), Some(answer(Choice::Fail)));
        assert!(
            !path.with_extension("json.lock").exists(),
            "the lock is released"
        );

        fs::write(&path, "{not json").unwrap();
        assert!(Cache::load(&path).get("k").is_none());
    }

    #[test]
    fn concurrent_runs_merge_instead_of_overwriting() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache.json");
        let mut first = Cache::load(&path);
        let mut second = Cache::load(&path);
        first.put("a".into(), answer(Choice::Pass));
        second.put("b".into(), answer(Choice::Fail));
        first.save().unwrap();
        second.save().unwrap();
        let merged = Cache::load(&path);
        assert!(merged.get("a").is_some() && merged.get("b").is_some());
    }

    #[test]
    fn old_entries_are_pruned_and_stale_locks_broken() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache.json");
        let old = now() - PRUNE_AFTER_SECS - 10;
        fs::write(
            &path,
            format!(r#"{{"old": {{"choice": "pass", "confidence": 1.0, "touched": {old}}}}}"#),
        )
        .unwrap();
        let lock = path.with_extension("json.lock");
        fs::write(&lock, "").unwrap();
        let stale = SystemTime::now() - Duration::from_secs(120);
        fs::File::options()
            .write(true)
            .open(&lock)
            .unwrap()
            .set_modified(stale)
            .unwrap();

        let mut cache = Cache::load(&path);
        cache.put("new".into(), answer(Choice::Pass));
        cache.save().unwrap();
        let reloaded = Cache::load(&path);
        assert!(reloaded.get("old").is_none());
        assert!(reloaded.get("new").is_some());
    }

    #[test]
    fn disabled_cache_never_writes() {
        let mut cache = Cache::disabled();
        cache.put("k".into(), answer(Choice::Pass));
        cache.save().unwrap();
    }
}
