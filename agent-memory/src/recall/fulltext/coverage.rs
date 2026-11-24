//! L5b rework: coverage and freshness of one indexed live source.
//!
//! `memory_search` answers from the last `index` snapshot. An agent that gets
//! zero hits cannot tell "nothing was ever said" from "the index is behind", so
//! every exact/hybrid source detail carries a `coverage` block:
//!
//! ```json
//! {"indexed_at": "2026-10-30T02:11:07Z", "newest_unit_ts": "2026-10-30T01:58:12Z",
//!  "rows": 207230, "sessions": 812, "vectors": 28926, "stale_since": "…"?}
//! ```
//!
//! Freshness is deliberately cheap: one aggregate query on the OpenCode database
//! or `stat`-only comparisons against the JSONL/files watermarks. No record body
//! is read, and every failure only omits the block — coverage never changes a
//! query's status or exit code.

use super::{
    file_stamp, index_path, rfc3339, sanitize_source_id, IndexMeta, LIVE_DIR, VECTORS_FILENAME,
};
use crate::recall::config::{RecallConfig, SourceConfig, SourceKind};
use crate::recall::semantic::embed::EmbeddingPlan;
use rusqlite::Connection;
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};

/// Build the `coverage` block for one source, or `None` when there is no index
/// that belongs to this configuration (missing, unreadable, other identity).
///
/// The projector/schema generation is intentionally **not** part of this check:
/// a stale index still has useful coverage numbers, and the source status
/// already names the stale reason.
pub fn source_coverage(config: &RecallConfig, source: &SourceConfig) -> Option<Value> {
    let cache_root = super::configured_cache_root(config).ok()?;
    let path = index_path(&cache_root, &source.source_id);
    let conn = open_index(&path)?;
    let meta = IndexMeta::load(&conn).ok().flatten()?;
    if meta.source_id != source.source_id || meta.source_root != source.root {
        return None;
    }
    if meta.source_kind != source.kind.as_str() {
        return None;
    }
    Some(coverage_block(config, &cache_root, source, &conn, &meta))
}

fn open_index(path: &Path) -> Option<Connection> {
    if !path.is_file() {
        return None;
    }
    let flags =
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX;
    Connection::open_with_flags(path, flags).ok()
}

fn coverage_block(
    config: &RecallConfig,
    cache_root: &Path,
    source: &SourceConfig,
    conn: &Connection,
    meta: &IndexMeta,
) -> Value {
    let mut block = Map::new();
    if let Some(indexed_at) = rfc3339(meta.built_at_ms) {
        block.insert("indexed_at".into(), Value::String(indexed_at));
    }
    if let Some(newest) = rfc3339(meta.newest_unit_ts) {
        block.insert("newest_unit_ts".into(), Value::String(newest));
    }
    block.insert("rows".into(), json!(meta.rows));
    // `sessions` counts live-source sessions only; sessions served from the
    // archive are reported separately as `archive_only_sessions` below
    // (rework 3 / N4).
    let sessions: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sessions              WHERE session_id NOT IN (SELECT session_id FROM archive_sessions)",
            [],
            |row| row.get(0),
        )
        .unwrap_or(0);
    block.insert("sessions".into(), json!(sessions));
    let vectors = EmbeddingPlan::from_config(config).and_then(|plan| {
        let path = cache_root
            .join(LIVE_DIR)
            .join(sanitize_source_id(&source.source_id))
            .join(VECTORS_FILENAME);
        super::vectors::cached_units(&path, &plan)
    });
    block.insert(
        "vectors".into(),
        vectors.map(Value::from).unwrap_or(Value::Null),
    );
    // Stage-2 S5 archive fallback: present only when this index was built with
    // an archive source; every pre-S5 coverage block is unchanged.
    if let Some(fallback) = &meta.fallback {
        block.insert(
            "archive_only_sessions".into(),
            json!(fallback.archive_only_sessions),
        );
        block.insert("archive_root".into(), json!(fallback.archive_root));
    }
    if let Some(stale_since) = stale_since(source, meta) {
        if let Some(label) = rfc3339(stale_since) {
            block.insert("stale_since".into(), Value::String(label));
        }
    }
    Value::Object(block)
}

/// Newest source change not covered by the index, in milliseconds. `None` means
/// the index is current as far as a cheap check can tell.
fn stale_since(source: &SourceConfig, meta: &IndexMeta) -> Option<i64> {
    match source.kind {
        SourceKind::Opencode => opencode_stale_since(source, meta),
        SourceKind::Codex | SourceKind::ClaudeCode => jsonl_stale_since(source, meta),
        SourceKind::Dsh => dsh_stale_since(source, meta),
        SourceKind::Hermes => hermes_stale_since(source, meta),
        SourceKind::Device => files_stale_since(source, meta),
        SourceKind::Docs => super::build::docs_stale_since(source, meta),
        SourceKind::Kb | SourceKind::Archive => None,
    }
}

/// OpenCode: the cheap check is the **host file stamp**, the same signal the
/// round itself uses. `COUNT(*)` / `MAX(<ts>)` over `session_message` (the
/// pre-004b check) is a full pass over a table that can be gigabytes, paid by
/// every query; two `stat`s answer the same question — "did the source move
/// since the index?" — and can only ever be wrong in the safe direction (a
/// write that does not move the file stamps is invisible to both sides).
fn opencode_stale_since(source: &SourceConfig, meta: &IndexMeta) -> Option<i64> {
    let watermark =
        serde_json::from_value::<super::watermark::SqliteWatermark>(meta.watermark.clone()).ok()?;
    let db_path = match source.db.as_deref() {
        Some(value) => PathBuf::from(value),
        None => Path::new(&source.root).join("opencode.db"),
    };
    if !db_path.is_file() {
        return None;
    }
    super::build::opencode_source_changed_at(&db_path, &watermark)
}

fn jsonl_stale_since(source: &SourceConfig, meta: &IndexMeta) -> Option<i64> {
    let watermark =
        serde_json::from_value::<super::watermark::JsonlWatermark>(meta.watermark.clone()).ok()?;
    let files: Vec<(String, PathBuf)> = match source.kind {
        SourceKind::Codex => super::build::codex_files(source).ok()?,
        _ => crate::recall::providers::claude_code::transcript_files(Path::new(&source.root))
            .ok()?,
    };
    let mut newest: Option<i64> = None;
    let mut seen: Vec<String> = Vec::new();
    for (rel_path, path) in &files {
        seen.push(rel_path.clone());
        let Some((size, mtime_ns)) = file_stamp(path) else {
            continue;
        };
        let changed = match watermark.files.get(rel_path) {
            None => true,
            Some(entry) => size != entry.size || mtime_ns != entry.mtime_ns,
        };
        if changed {
            newest = Some(newest.unwrap_or(0).max(ms_from_ns(mtime_ns)));
        }
    }
    // A file that disappeared makes the index stale too, but it has no mtime:
    // the index itself is the newest evidence of that state.
    let deleted = watermark.files.keys().any(|rel| !seen.contains(rel));
    if deleted {
        newest = Some(newest.unwrap_or(0).max(meta.built_at_ms));
    }
    newest
}

/// DSH: a changed carrier (size, mtime or highest generation) or a vanished
/// session directory makes the index stale.
fn dsh_stale_since(source: &SourceConfig, meta: &IndexMeta) -> Option<i64> {
    let watermark =
        serde_json::from_value::<super::watermark::DshWatermark>(meta.watermark.clone()).ok()?;
    let inventory = crate::recall::providers::dsh::inventory(Path::new(&source.root)).ok()?;
    let mut newest: Option<i64> = None;
    let mut seen: Vec<String> = Vec::new();
    for session in &inventory.sessions {
        seen.push(session.rel_dir.clone());
        let Some((size, mtime_ns)) = file_stamp(&session.path) else {
            continue;
        };
        let changed = match watermark.files.get(&session.rel_dir) {
            None => true,
            Some(entry) => {
                entry.file != session.rel_path || entry.size != size || entry.mtime_ns != mtime_ns
            }
        };
        if changed {
            newest = Some(newest.unwrap_or(0).max(ms_from_ns(mtime_ns)));
        }
    }
    let deleted = watermark.files.keys().any(|rel| !seen.contains(rel));
    if deleted {
        newest = Some(newest.unwrap_or(0).max(meta.built_at_ms));
    }
    newest
}

/// Hermes uses the same database/WAL stamp comparison as the build round.
fn hermes_stale_since(source: &SourceConfig, meta: &IndexMeta) -> Option<i64> {
    let watermark =
        serde_json::from_value::<super::watermark::SqliteWatermark>(meta.watermark.clone()).ok()?;
    let path = source.db.as_deref()?;
    super::build::opencode_source_changed_at(Path::new(path), &watermark)
}

fn files_stale_since(source: &SourceConfig, meta: &IndexMeta) -> Option<i64> {
    let watermark =
        serde_json::from_value::<super::watermark::FilesWatermark>(meta.watermark.clone()).ok()?;
    let root = Path::new(&source.root);
    let entries = std::fs::read_dir(root).ok()?;
    let mut newest: Option<i64> = None;
    let mut seen: Vec<String> = Vec::new();
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() || file_type.is_symlink() {
            continue;
        }
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if name.starts_with('.') || !super::build::device_pattern().is_match(&name) {
            continue;
        }
        seen.push(name.clone());
        let Some((size, mtime_ns)) = file_stamp(&entry.path()) else {
            continue;
        };
        let changed = match watermark.files.get(&name) {
            None => true,
            Some(stamp) => stamp.size != size || stamp.mtime_ns != mtime_ns,
        };
        if changed {
            newest = Some(newest.unwrap_or(0).max(ms_from_ns(mtime_ns)));
        }
    }
    let deleted = watermark.files.keys().any(|name| !seen.contains(name));
    if deleted {
        newest = Some(newest.unwrap_or(0).max(meta.built_at_ms));
    }
    newest
}

fn ms_from_ns(nanos: u128) -> i64 {
    (nanos / 1_000_000).min(i64::MAX as u128) as i64
}
