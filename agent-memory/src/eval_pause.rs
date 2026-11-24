//! Evaluation pause marker (stage 7 §C rework).
//!
//! The module sits next to `recall` rather than inside `src/recall` on purpose:
//! recall is under a reverse tripwire that bans write entry points (`fs::write`,
//! `File::create`, `OpenOptions`) in that tree, and this marker is the one
//! intentional write. It is still recall-side state (it lives under the recall
//! cache root) and does not depend on `Archive`.
//!
//! A long `eval-recall` run must not race the scheduled index refresh: any
//! rebuild under it changes `coverage.indexed_at`/`rows`, which the corpus
//! fingerprint correctly reports as `corpus_changed`. The marker is the
//! cooperative half of that contract:
//!
//! * `eval-recall` creates `<cache_root>/locks/eval-pause.lock` exclusively
//!   (`create_new`, same shape as the archive named locks) and removes it in
//!   the guard's `Drop`, so an error path cannot leave it behind;
//! * `agent-memory index` sees an unexpired marker and skips every indexable
//!   source (`skipped_eval_in_progress`, exit 0) — the next refresh cycle
//!   catches up;
//! * a marker older than six hours (or unparseable) is treated as residue:
//!   `index` ignores it and discloses `stale_eval_pause` but never deletes it,
//!   while `eval-recall` replaces it and discloses the fact in the report.
//!
//! There is deliberately no PID liveness probe: Windows PID reuse makes it
//! unreliable, and the six-hour ceiling is enough for an abandoned marker.
//! Only the cache root marker is written; nothing else about the run changes.

use serde_json::{json, Value};
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub const EVAL_PAUSE_FILENAME: &str = "eval-pause.lock";
/// A marker older than this is residue, not a live evaluation.
pub const EVAL_PAUSE_MAX_AGE_MS: i64 = 6 * 60 * 60 * 1000;

/// What the marker says right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PauseState {
    Absent,
    Active {
        pid: String,
        acquired_at_unix_ms: i64,
    },
    Stale {
        /// `expired` or `unparseable` (the report uses `stale_eval_pause`).
        reason: &'static str,
        acquired_at_unix_ms: Option<i64>,
    },
}

impl PauseState {
    pub fn is_active(&self) -> bool {
        matches!(self, PauseState::Active { .. })
    }
}

/// Wall-clock milliseconds; only used for the marker's age.
pub fn now_unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or(0)
}

pub fn marker_path(cache_root: &Path) -> PathBuf {
    cache_root.join("locks").join(EVAL_PAUSE_FILENAME)
}

/// Read the marker without changing it.
pub fn inspect(cache_root: &Path, now_ms: i64) -> PauseState {
    let path = marker_path(cache_root);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return PauseState::Absent,
        Err(_) => {
            return PauseState::Stale {
                reason: "unparseable",
                acquired_at_unix_ms: None,
            }
        }
    };
    parse_marker(&text, now_ms)
}

fn parse_marker(text: &str, now_ms: i64) -> PauseState {
    let mut pid = String::new();
    let mut acquired: Option<i64> = None;
    for token in text.split_whitespace() {
        if let Some(value) = token.strip_prefix("pid=") {
            pid = value.to_string();
        } else if let Some(value) = token.strip_prefix("acquired_at_unix_ms=") {
            acquired = value.parse::<i64>().ok();
        }
    }
    let Some(acquired_at_unix_ms) = acquired else {
        return PauseState::Stale {
            reason: "unparseable",
            acquired_at_unix_ms: None,
        };
    };
    // A clock that moved backwards must not turn a live marker into residue.
    let age = now_ms.saturating_sub(acquired_at_unix_ms);
    if age > EVAL_PAUSE_MAX_AGE_MS {
        return PauseState::Stale {
            reason: "expired",
            acquired_at_unix_ms: Some(acquired_at_unix_ms),
        };
    }
    PauseState::Active {
        pid,
        acquired_at_unix_ms,
    }
}

/// `Some(root)` when the configuration has a usable cache root, so callers can
/// distinguish "no cache to pause" from "marker absent".
pub fn configured_cache_root(config: &crate::recall::config::RecallConfig) -> Option<PathBuf> {
    crate::recall::fulltext::configured_cache_root(config).ok()
}

/// JSON disclosure for the `index` report.
pub fn report(cache_root: Option<&Path>, state: &PauseState) -> Value {
    let marker = cache_root
        .map(|root| marker_path(root).to_string_lossy().to_string())
        .unwrap_or_default();
    match state {
        PauseState::Absent => json!({ "status": "absent", "marker": marker }),
        PauseState::Active {
            pid,
            acquired_at_unix_ms,
        } => json!({
            "status": "active",
            "marker": marker,
            "reason": "eval_in_progress",
            "pid": pid,
            "acquired_at_unix_ms": acquired_at_unix_ms,
        }),
        PauseState::Stale {
            reason,
            acquired_at_unix_ms,
        } => json!({
            "status": "stale",
            "marker": marker,
            "reason": "stale_eval_pause",
            "detail": reason,
            "acquired_at_unix_ms": acquired_at_unix_ms,
        }),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PauseError {
    InProgress {
        pid: String,
        acquired_at_unix_ms: i64,
    },
    Io(String),
}

/// The live marker; deleting it is the only cleanup and happens exactly once.
#[derive(Debug)]
pub struct EvalPauseGuard {
    path: PathBuf,
    locks_dir: PathBuf,
    /// Exact bytes this guard wrote; the marker is removed only while it still
    /// holds them, so a guard never deletes a marker another eval wrote after
    /// replacing this one as stale.
    content: String,
}

impl EvalPauseGuard {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for EvalPauseGuard {
    fn drop(&mut self) {
        match fs::read_to_string(&self.path) {
            Ok(current) if current == self.content => {
                let _ = fs::remove_file(&self.path);
            }
            _ => return,
        }
        // Only succeeds when the directory is empty (i.e. we created it for
        // this marker); an existing shared `locks/` stays untouched.
        let _ = fs::remove_dir(&self.locks_dir);
    }
}

#[derive(Debug)]
pub struct AcquiredPause {
    pub guard: Option<EvalPauseGuard>,
    /// A stale marker was replaced; the report discloses `stale_eval_pause`.
    pub stale_replaced: bool,
}

/// Create the marker. No cache root (or a missing one) means there is no live
/// index to pause, so the run proceeds without a marker.
pub fn acquire(cache_root: Option<&Path>, now_ms: i64) -> Result<AcquiredPause, PauseError> {
    let Some(cache_root) = cache_root else {
        return Ok(AcquiredPause {
            guard: None,
            stale_replaced: false,
        });
    };
    if !cache_root.is_dir() {
        return Ok(AcquiredPause {
            guard: None,
            stale_replaced: false,
        });
    }
    let locks_dir = cache_root.join("locks");
    fs::create_dir_all(&locks_dir).map_err(|error| {
        PauseError::Io(format!(
            "cannot create the eval pause directory {}: {error}",
            locks_dir.display()
        ))
    })?;
    let path = locks_dir.join(EVAL_PAUSE_FILENAME);
    let mut stale_replaced = false;
    let content = match create_marker(&path, now_ms) {
        Ok(content) => content,
        Err(error) if error.kind() != io::ErrorKind::AlreadyExists => {
            return Err(PauseError::Io(format!(
                "cannot create {}: {error}",
                path.display()
            )));
        }
        Err(_) => {
            match inspect(cache_root, now_ms) {
                PauseState::Active {
                    pid,
                    acquired_at_unix_ms,
                } => {
                    return Err(PauseError::InProgress {
                        pid,
                        acquired_at_unix_ms,
                    })
                }
                PauseState::Absent => {}
                PauseState::Stale { .. } => {
                    // Residue from an abandoned run: replace it.
                    let _ = fs::remove_file(&path);
                    stale_replaced = true;
                }
            }
            match create_marker(&path, now_ms) {
                Ok(content) => content,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    return Err(PauseError::InProgress {
                        pid: "unknown".to_string(),
                        acquired_at_unix_ms: 0,
                    })
                }
                Err(error) => {
                    return Err(PauseError::Io(format!(
                        "cannot replace {}: {error}",
                        path.display()
                    )))
                }
            }
        }
    };
    Ok(AcquiredPause {
        guard: Some(EvalPauseGuard {
            path,
            locks_dir,
            content,
        }),
        stale_replaced,
    })
}

fn create_marker(path: &Path, now_ms: i64) -> io::Result<String> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    let content = format!(
        "pid={} acquired_at_unix_ms={}\n",
        std::process::id(),
        now_ms
    );
    let _ = file.write_all(content.as_bytes());
    let _ = file.sync_all();
    Ok(content)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root() -> tempfile::TempDir {
        tempfile::tempdir().expect("temp dir")
    }

    #[test]
    fn an_absent_marker_is_not_active() {
        let directory = temp_root();
        assert_eq!(inspect(directory.path(), 1_000), PauseState::Absent);
    }

    #[test]
    fn an_unexpired_marker_is_active_and_an_old_one_is_stale() {
        let directory = temp_root();
        fs::create_dir_all(directory.path().join("locks")).expect("locks");
        let path = marker_path(directory.path());
        fs::write(&path, "pid=42 acquired_at_unix_ms=1000").expect("marker");
        assert_eq!(
            inspect(directory.path(), 1_000 + EVAL_PAUSE_MAX_AGE_MS),
            PauseState::Active {
                pid: "42".to_string(),
                acquired_at_unix_ms: 1_000
            }
        );
        assert_eq!(
            inspect(directory.path(), 1_000 + EVAL_PAUSE_MAX_AGE_MS + 1),
            PauseState::Stale {
                reason: "expired",
                acquired_at_unix_ms: Some(1_000)
            }
        );
        fs::write(&path, "not a marker").expect("marker");
        assert_eq!(
            inspect(directory.path(), 1_000),
            PauseState::Stale {
                reason: "unparseable",
                acquired_at_unix_ms: None
            }
        );
    }

    #[test]
    fn acquire_refuses_a_live_marker_and_replaces_a_stale_one() {
        let directory = temp_root();
        fs::create_dir_all(directory.path().join("locks")).expect("locks");
        let path = marker_path(directory.path());
        fs::write(&path, "pid=1 acquired_at_unix_ms=1000").expect("marker");
        let refusal = acquire(Some(directory.path()), 2_000).expect_err("live marker");
        assert_eq!(
            refusal,
            PauseError::InProgress {
                pid: "1".to_string(),
                acquired_at_unix_ms: 1_000
            }
        );
        assert!(path.is_file(), "a refused acquire never deletes the marker");

        let stale = acquire(Some(directory.path()), 1_000 + EVAL_PAUSE_MAX_AGE_MS + 1)
            .expect("stale marker is replaced");
        assert!(stale.stale_replaced);
        assert!(path.is_file());
        let text = fs::read_to_string(&path).expect("marker");
        assert!(text.contains(&format!("pid={}", std::process::id())));
        drop(stale.guard);
        assert!(!path.exists(), "the guard removes the marker");
        assert!(
            !directory.path().join("locks").exists(),
            "an empty locks directory is cleaned up"
        );
    }

    #[test]
    fn no_cache_root_means_no_marker() {
        let acquired = acquire(None, 1_000).expect("no cache root is fine");
        assert!(acquired.guard.is_none());
        assert!(!acquired.stale_replaced);
        let missing = temp_root();
        let acquired = acquire(Some(&missing.path().join("absent")), 1_000).expect("missing");
        assert!(acquired.guard.is_none());
    }

    #[test]
    fn a_guard_leaves_a_marker_it_did_not_write() {
        let directory = temp_root();
        let acquired = acquire(Some(directory.path()), 1_000).expect("acquire");
        let guard = acquired.guard.expect("guard");
        let path = guard.path().to_path_buf();
        // Another eval replaced this marker as stale; it now owns the file.
        fs::write(&path, "pid=999999 acquired_at_unix_ms=2000
").expect("rewrite");
        drop(guard);
        assert!(path.exists(), "a foreign marker must survive the old guard");
    }
}
