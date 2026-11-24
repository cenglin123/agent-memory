//! Observed-root snapshots for `side_effects[]` (§2.3).
//!
//! Only `stat`-level data is collected (`path + size + mtime`), never content.
//! A diff proves "something changed in this window"; it never claims who wrote
//! it. Known telemetry paths and source-body changes are contract violations.

use crate::recall::contract::{SideEffect, KNOWN_AB_PATHS};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Strict-snapshot ceilings: a copy above this is refused explicitly instead of
/// silently reading the original root.
pub const STRICT_SNAPSHOT_MAX_BYTES: u64 = 512 * 1024 * 1024;
pub const STRICT_SNAPSHOT_MAX_ENTRIES: usize = 200_000;

static SNAPSHOT_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Default, Clone)]
pub struct Snapshot {
    entries: BTreeMap<String, (u64, u128)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChangeKind {
    Added,
    Removed,
    Modified,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub rel_path: String,
    pub kind: ChangeKind,
}

impl Snapshot {
    pub fn capture(root: &Path) -> Snapshot {
        let mut snapshot = Snapshot::default();
        collect(root, root, &mut snapshot.entries);
        snapshot
    }

    pub fn diff(&self, after: &Snapshot) -> Vec<Change> {
        let mut changes = Vec::new();
        for (path, meta) in &after.entries {
            match self.entries.get(path) {
                None => changes.push(Change {
                    rel_path: path.clone(),
                    kind: ChangeKind::Added,
                }),
                Some(previous) if previous != meta => {
                    // A zero-byte entry in the "before" snapshot is a file that
                    // was still being created (create+write is not atomic).
                    // It has no stable prior content, so it is reported as an
                    // addition, never as a mutation.
                    changes.push(Change {
                        rel_path: path.clone(),
                        kind: if previous.0 == 0 {
                            ChangeKind::Added
                        } else {
                            ChangeKind::Modified
                        },
                    });
                }
                Some(_) => {}
            }
        }
        for (path, meta) in &self.entries {
            // A zero-byte entry that vanished was never a stable carrier.
            if after.entries.contains_key(path) || meta.0 == 0 {
                continue;
            }
            changes.push(Change {
                rel_path: path.clone(),
                kind: ChangeKind::Removed,
            });
        }
        changes.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
        changes
    }
}

fn collect(root: &Path, current: &Path, entries: &mut BTreeMap<String, (u64, u128)>) {
    let Ok(read_dir) = std::fs::read_dir(current) else {
        return;
    };
    for entry in read_dir.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_symlink() {
            continue;
        }
        let path = entry.path();
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if file_type.is_dir() {
            collect(root, &path, entries);
            continue;
        }
        let rel = path
            .strip_prefix(root)
            .map(|rel| {
                rel.to_string_lossy()
                    .replace('\\', "/")
            })
            .unwrap_or_else(|_| path.to_string_lossy().to_string());
        let mtime = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        entries.insert(rel, (metadata.len(), mtime));
    }
}

/// Runtime artifacts that are allowed C-level window changes.
pub fn is_runtime_artifact(rel_path: &str) -> bool {
    let lowered = rel_path.to_ascii_lowercase();
    if lowered.ends_with("-wal") || lowered.ends_with("-shm") || lowered.ends_with("-journal") {
        return true;
    }
    lowered.split('/').any(|segment| segment == "__pycache__")
        || lowered.ends_with(".pyc")
}

pub fn is_known_ab_path(rel_path: &str) -> bool {
    KNOWN_AB_PATHS.contains(&rel_path)
}

/// Split a diff into reportable C-level entries and contract violations.
///
/// Attribution rule (§2.3): a change only proves "something changed in this
/// window". Runtime artifacts (`-wal`/`-shm`/`-journal`, `__pycache__`) are
/// expected. A brand-new path of unknown provenance is reported as a C-level
/// window change (`note = window_only`) without claiming a violation. A change
/// to a path that already existed when the window opened, or to a known A/B
/// telemetry path, is a contract violation: this tool only ever reads.
pub fn classify_changes(changes: &[Change]) -> (Vec<SideEffect>, Vec<String>) {
    let mut side_effects = Vec::new();
    let mut violations = Vec::new();
    for change in changes {
        side_effects.push(SideEffect::window_change(change.rel_path.clone()));
        if is_runtime_artifact(&change.rel_path) {
            continue;
        }
        if is_known_ab_path(&change.rel_path) {
            violations.push(change.rel_path.clone());
            continue;
        }
        match change.kind {
            // Unknown new file: provenance cannot be attributed to this tool.
            ChangeKind::Added => {}
            // A pre-existing source carrier changed or vanished while we held
            // a read-only handle.
            ChangeKind::Modified | ChangeKind::Removed => {
                violations.push(change.rel_path.clone());
            }
        }
    }
    (side_effects, violations)
}

#[derive(Debug, Default, Clone)]
pub struct Window {
    pub root: Option<PathBuf>,
    pub before: Snapshot,
}

impl Window {
    pub fn open(root: &Path) -> Self {
        Window {
            root: Some(root.to_path_buf()),
            before: Snapshot::capture(root),
        }
    }

    /// Close the window and report C-level changes plus violations.
    pub fn close(&self) -> (Vec<SideEffect>, Vec<String>) {
        let Some(root) = &self.root else {
            return (Vec::new(), Vec::new());
        };
        let after = Snapshot::capture(root);
        let changes = self.before.diff(&after);
        classify_changes(&changes)
    }
}

/// A throwaway consistency copy of one source root (`--strict-snapshot`).
///
/// The original root is never opened; adapters read from [`path`]. The copy is
/// removed on drop and never appears in `side_effects[]` (§2.3): the originals
/// were not read in this window.
pub struct StrictSnapshot {
    dir: PathBuf,
    root: PathBuf,
}

impl StrictSnapshot {
    pub fn create(source_root: &Path) -> Result<StrictSnapshot, String> {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or(0);
        let sequence = SNAPSHOT_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agent-memory-strict-{}-{stamp}-{sequence}",
            std::process::id()
        ));
        let root = dir.join("root");
        if let Err(err) = std::fs::create_dir_all(&root) {
            let _ = std::fs::remove_dir_all(&dir);
            return Err(format!("strict snapshot directory cannot be created: {err}"));
        }
        let mut budget = CopyBudget::default();
        if let Err(err) = copy_tree(source_root, &root, &mut budget) {
            // A half-copied tree is worse than none: remove it (no Drop runs
            // when construction fails).
            let _ = std::fs::remove_dir_all(&dir);
            return Err(err);
        }
        Ok(StrictSnapshot { dir, root })
    }

    pub fn path(&self) -> &Path {
        &self.root
    }
}

impl Drop for StrictSnapshot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[derive(Default)]
struct CopyBudget {
    bytes: u64,
    entries: usize,
}

fn copy_tree(source: &Path, target: &Path, budget: &mut CopyBudget) -> Result<(), String> {
    let entries = std::fs::read_dir(source)
        .map_err(|err| format!("strict snapshot cannot list {}: {err}", source.display()))?;
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_symlink() {
            continue;
        }
        budget.entries += 1;
        if budget.entries > STRICT_SNAPSHOT_MAX_ENTRIES {
            return Err("strict snapshot exceeds its entry ceiling".to_string());
        }
        let from = entry.path();
        let to = target.join(entry.file_name());
        if file_type.is_dir() {
            std::fs::create_dir_all(&to)
                .map_err(|err| format!("strict snapshot cannot create {}: {err}", to.display()))?;
            copy_tree(&from, &to, budget)?;
            continue;
        }
        let metadata = entry
            .metadata()
            .map_err(|err| format!("strict snapshot cannot stat {}: {err}", from.display()))?;
        budget.bytes += metadata.len();
        if budget.bytes > STRICT_SNAPSHOT_MAX_BYTES {
            return Err("strict snapshot exceeds its byte ceiling".to_string());
        }
        std::fs::copy(&from, &to)
            .map_err(|err| format!("strict snapshot cannot copy {}: {err}", from.display()))?;
    }
    Ok(())
}
