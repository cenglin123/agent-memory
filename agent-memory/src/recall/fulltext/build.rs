//! L2: build and incrementally update the live-source full-text index.
//!
//! Every builder follows the same shape:
//!
//! 1. open the source read-only (a live source is never written),
//! 2. compare the stored watermark with the current source identity,
//! 3. either rebuild the whole index (identity/schema change, row-count shrink,
//!    or `--rebuild`) or apply only the changed records,
//! 4. commit inside one transaction and rewrite `meta`.
//!
//! Deleting the index and rebuilding it produces the same rows: nothing in the
//! index depends on build order or on the previous index contents.
//!
//! # Per-carrier change classification
//!
//! JSONL and DSH carriers are classified by `size` / `mtime_ns` first, then by
//! the content probes and the file identity the watermark carries; the full
//! decision table (and the declared probe blind spot) is the documentation of
//! [`super::watermark`]. Two outcomes exist besides append / rescan:
//!
//! * **same bytes, new timestamp** — `index.mtime_only_refresh`; the round
//!   reads two 8 KiB windows and nothing else;
//! * **a completed trailing fragment** — the previous round stopped at a
//!   half-written final line, so `offset < size`; the round reads
//!   `[offset, size)` (`index.tail_completion`) and never the file;
//! * **a watermark written before the probes existed** — the entry is completed
//!   in place without a rescan (the upgrade round), so the next round can use
//!   the probes.
//!
//! Everything the finalize step reports (`rows`, `newest_unit_ts`) is answered
//! from the narrow `doc_keys` table, never from the FTS5 body: see
//! [`COUNT_UNITS_SQL`] and [`NEWEST_UNIT_TS_SQL`].

use super::watermark::{
    DshFile, DshWatermark, FileStamp, FilesWatermark, JsonlFile, JsonlWatermark, OversizeRow,
    SqliteStamp, SqliteWatermark,
};
use super::{
    claude_session_kind, codex_session_kind, configured_cache_root, file_stamp, index_path,
    open_writable, opencode_session_kind, projector_version, IndexMeta, Unit, INDEX_SCHEMA_VERSION,
    ORIGIN_LIVE, TOOL_OUTPUT_MAX_CHARS,
};
use crate::cost::{self, Cost};
use crate::providers::claude::{QueuedDedup, QueuedVerdict};
use crate::providers::utils::parse_timestamp_to_ms;
use crate::recall::config::{RecallConfig, SourceConfig, SourceKind};
use crate::recall::contract::{
    RecallError, DEVICE_FILE_PATTERN, ROLE_ASSISTANT, ROLE_DOC, ROLE_TOOL, ROLE_USER,
    SCHEMA_HERMES_SQLITE_V1, SCHEMA_SQLITE_V2, SESSION_KIND_INTERACTIVE, SESSION_KIND_UNKNOWN,
    UNIT_KIND_TEXT, UNIT_KIND_TOOL_INPUT, UNIT_KIND_TOOL_OUTPUT,
};
use crate::recall::hash::{sha256_hex, TextRange};
use crate::recall::port::model::PartData;
use crate::recall::port::probe::open_readonly;
use crate::recall::port::probe::probe;
use crate::recall::port::probe::table_columns;
use crate::recall::port::project::{classify_part, compact_json};
use crate::recall::port::reader::{read_line_bounded, rel_from_root, BoundedLine};
use crate::recall::port::sqlite_v2::session_parts;
use crate::recall::port::PartOutcome;
use crate::recall::providers::claude_code::{self, ClaudeRecordOutcome, ClaudeUnit};
use crate::recall::providers::codex::{project_record, RecordOutcome};
use crate::recall::providers::dsh::{self, DshEventOutcome, DshSessionMeta};
use crate::recall::providers::hermes;
use crate::recall::refs::Locator;
use crate::recall::semantic::embed::EmbeddingPlan;
use regex::Regex;
use rusqlite::{params, Connection};
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Instant;

/// JSONL line ceiling for indexing: a longer line is skipped and counted.
pub const INDEX_LINE_LIMIT_BYTES: u64 = 64 * 1024 * 1024;

/// One source's build report (also one element of the CLI `index` output).
#[derive(Debug, Clone, Serialize)]
pub struct BuildReport {
    pub source_id: String,
    pub source_kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// `built|updated|unchanged|skipped_disabled|skipped_not_indexable|skipped_eval_in_progress|unsupported|unavailable|failed`
    pub status: String,
    pub rows: u64,
    pub added: u64,
    pub removed: u64,
    pub rebuilt: bool,
    pub duration_ms: u64,
    pub index_bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// §A measurement of this source's build (counters and phase times only).
    pub cost: Cost,
}

impl BuildReport {
    fn skipped(source: &SourceConfig, status: &str, reason: &str) -> BuildReport {
        BuildReport {
            source_id: source.source_id.clone(),
            source_kind: source.kind.as_str().to_string(),
            path: None,
            status: status.to_string(),
            rows: 0,
            added: 0,
            removed: 0,
            rebuilt: false,
            duration_ms: 0,
            index_bytes: 0,
            reason: Some(reason.to_string()),
            detail: None,
            error: None,
            cost: Cost::default(),
        }
    }
}

#[derive(Debug, Default, Clone)]
struct Stats {
    rows: u64,
    added: u64,
    removed: u64,
    rebuilt: bool,
    /// Why a full rebuild happened, when it is worth naming (`index` reports it).
    reason: Option<&'static str>,
    detail: Option<Value>,
}

pub(crate) enum BuildError {
    Unsupported(String),
    Unavailable(String),
    Failed(String),
}

impl BuildError {
    pub(crate) fn message(&self) -> String {
        match self {
            BuildError::Unsupported(message)
            | BuildError::Unavailable(message)
            | BuildError::Failed(message) => message.clone(),
        }
    }

    fn status(&self) -> &'static str {
        match self {
            BuildError::Unsupported(_) => "unsupported",
            BuildError::Unavailable(_) => "unavailable",
            BuildError::Failed(_) => "failed",
        }
    }
}

/// Build every enabled, indexable source (or just `only`).
pub fn index_all(
    config: &RecallConfig,
    only: Option<&str>,
    rebuild: bool,
) -> Result<Vec<BuildReport>, RecallError> {
    let cache_root = configured_cache_root(config)?;
    if let Some(id) = only {
        if config.source(id).is_none() {
            return Err(RecallError::invalid(
                "source",
                "unknown_source",
                format!("--source {id} is not configured"),
            ));
        }
    }
    // Stage 7 §C rework: a live `eval-recall` pauses the refresh. The marker
    // is never deleted here (only the eval guard removes it); an expired one
    // is ignored and disclosed by the CLI report.
    let pause = crate::eval_pause::inspect(&cache_root, crate::eval_pause::now_unix_ms());
    let mut reports = Vec::new();
    for source in &config.sources {
        if let Some(id) = only {
            if source.source_id != id {
                continue;
            }
        }
        if !source.enabled {
            reports.push(BuildReport::skipped(source, "skipped_disabled", "disabled"));
            continue;
        }
        if !source.kind.is_indexable() {
            reports.push(BuildReport::skipped(
                source,
                "skipped_not_indexable",
                "kb keeps ask.py; archive keeps its replay generations",
            ));
            continue;
        }
        if pause.is_active() {
            let mut report =
                BuildReport::skipped(source, "skipped_eval_in_progress", "eval_in_progress");
            // The literal `skipped` marker names the reason in the same shape
            // as the capture driver's skip ledger (`skipped = locked`), so a
            // consumer can read it without knowing the status vocabulary.
            report.detail = Some(serde_json::json!({ "skipped": "eval_in_progress" }));
            reports.push(report);
            continue;
        }
        reports.push(index_source(config, source, &cache_root, rebuild));
    }
    Ok(reports)
}

/// Build or incrementally update one source's index, then its conversation
/// vector cache when `interpreter.embedding` is configured.
///
/// The vector step never turns a successful full-text build into a failure: an
/// unavailable model is reported as `detail.vectors.status = "degraded"` and the
/// command still exits 0. A full-text failure keeps its own non-zero status.
pub fn index_source(
    config: &RecallConfig,
    source: &SourceConfig,
    cache_root: &Path,
    rebuild: bool,
) -> BuildReport {
    let started = Instant::now();
    cost::begin_recording();
    let manifests_before = crate::archive::manifest_parse_count();
    let path = index_path(cache_root, &source.source_id);
    // Archive fallback preparation happens once per source build. Failures are
    // reported in the detail block, never fatal: the live index stays usable.
    let fallback = {
        // The inventory is a manifest-listing + manifest-parse pass; its own
        // phase separates it from the carrier rebuild it may trigger.
        let _manifest = cost::phase("archive_manifest_parse");
        match super::fallback::FallbackContext::prepare(config) {
            Ok(context) => Some(context),
            Err(error) => {
                if super::fallback::configured_archive_root(config).is_some() {
                    Some(super::fallback::FallbackContext::unavailable(error))
                } else {
                    None
                }
            }
        }
    };
    let result = match source.kind {
        SourceKind::Opencode => build_opencode(source, &path, rebuild, fallback.as_ref()),
        SourceKind::Codex | SourceKind::ClaudeCode => {
            build_jsonl(source, &path, rebuild, fallback.as_ref())
        }
        SourceKind::Dsh => build_dsh(source, &path, rebuild, fallback.as_ref()),
        SourceKind::Hermes => build_hermes(source, &path, rebuild),
        SourceKind::Device => build_files(source, &path, rebuild),
        SourceKind::Docs => build_docs(source, &path, rebuild),
        SourceKind::Kb | SourceKind::Archive => Err(BuildError::Unsupported(
            "source kind is not indexed".to_string(),
        )),
    };
    let index_bytes = std::fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0);
    match result {
        Ok(mut stats) => {
            let vector_report = match EmbeddingPlan::from_config(config) {
                Some(plan) => {
                    let _vectors = cost::phase("vectors");
                    super::vectors::build(config, source, &plan)
                }
                None => super::vectors::VectorReport::skipped("embedding_not_configured"),
            };
            let vector_value = serde_json::to_value(&vector_report).unwrap_or(Value::Null);
            match stats.detail.as_mut() {
                Some(Value::Object(map)) => {
                    map.insert("vectors".into(), vector_value);
                }
                _ => {
                    let mut map = serde_json::Map::new();
                    map.insert("vectors".into(), vector_value);
                    stats.detail = Some(Value::Object(map));
                }
            }
            BuildReport {
                source_id: source.source_id.clone(),
                source_kind: source.kind.as_str().to_string(),
                path: Some(path.to_string_lossy().to_string()),
                status: if stats.rebuilt {
                    "built".to_string()
                } else if stats.added > 0 || stats.removed > 0 {
                    "updated".to_string()
                } else {
                    "unchanged".to_string()
                },
                rows: stats.rows,
                added: stats.added,
                removed: stats.removed,
                rebuilt: stats.rebuilt,
                duration_ms: started.elapsed().as_millis() as u64,
                index_bytes,
                reason: stats.reason.map(str::to_string),
                detail: stats.detail,
                error: None,
                cost: {
                    cost::counter(
                        "manifest_parse_count",
                        crate::archive::manifest_parse_count().saturating_sub(manifests_before),
                    );
                    cost::finish_recording().unwrap_or_default()
                },
            }
        }
        Err(error) => BuildReport {
            source_id: source.source_id.clone(),
            source_kind: source.kind.as_str().to_string(),
            path: Some(path.to_string_lossy().to_string()),
            status: error.status().to_string(),
            rows: 0,
            added: 0,
            removed: 0,
            rebuilt: false,
            duration_ms: started.elapsed().as_millis() as u64,
            index_bytes,
            reason: None,
            detail: None,
            error: Some(error.message()),
            cost: {
                cost::counter(
                    "manifest_parse_count",
                    crate::archive::manifest_parse_count().saturating_sub(manifests_before),
                );
                cost::finish_recording().unwrap_or_default()
            },
        },
    }
}

// ---------------------------------------------------------------------------
// Writer
// ---------------------------------------------------------------------------

/// Registration facts of one archive-fallback session (`archive_sessions`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ArchiveRegistration {
    /// `FALLBACK_RULE_VERSION` that produced the rows.
    pub rule_version: Option<String>,
    /// Units the projection wrote (`None` on a row written before the column).
    pub projected_units: Option<u64>,
    /// Carrier-group fingerprint the projection read.
    pub carrier_fingerprint: Option<String>,
}

pub(crate) struct Writer<'a> {
    conn: &'a Connection,
    source_id: &'a str,
    added: u64,
    removed: u64,
    rebuilt: bool,
}

impl<'a> Writer<'a> {
    pub(crate) fn new(conn: &'a Connection, source_id: &'a str) -> Self {
        Writer {
            conn,
            source_id,
            added: 0,
            removed: 0,
            rebuilt: false,
        }
    }

    fn remove_rows(&mut self, rows: Vec<(i64, String)>) -> rusqlite::Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let mut delete_units = self.conn.prepare("DELETE FROM units WHERE rowid = ?1")?;
        let mut delete_keys = self
            .conn
            .prepare("DELETE FROM doc_keys WHERE doc_key = ?1")?;
        let mut delete_conversation = self
            .conn
            .prepare("DELETE FROM conversation_units WHERE rowid = ?1")?;
        for (rowid, key) in rows {
            delete_units.execute([rowid])?;
            delete_keys.execute([key])?;
            delete_conversation.execute([rowid])?;
            self.removed += 1;
        }
        Ok(())
    }

    fn remove_doc_key(&mut self, key: &str) -> rusqlite::Result<()> {
        let rows = self.select_rows(
            "SELECT rowid, doc_key FROM doc_keys WHERE doc_key = ?1",
            params![key],
        )?;
        self.remove_rows(rows)
    }

    /// Drop rows of one message that the latest projection no longer produces
    /// (a text block replaced by a tool call, an output that vanished, or a row
    /// that stopped projecting at all — empty `keep`).
    ///
    /// Only `origin = 'live'` rows are matched: an archive-fallback row of the
    /// same session/message is evidence replayed from the archive, not something
    /// a live read is entitled to delete.
    fn prune_message(
        &mut self,
        session_id: &str,
        message_id: &str,
        keep: &[String],
    ) -> rusqlite::Result<u64> {
        let rows = self.select_rows(
            "SELECT rowid, doc_key FROM doc_keys \
             WHERE session_id = ?1 AND message_id = ?2 AND origin = 'live'",
            params![session_id, message_id],
        )?;
        let stale: Vec<(i64, String)> = rows
            .into_iter()
            .filter(|(_, key)| !keep.iter().any(|k| k == key))
            .collect();
        let removed = stale.len() as u64;
        self.remove_rows(stale)?;
        Ok(removed)
    }

    /// Drop the live rows of one session whose message the host no longer
    /// holds. Empty `host_ids` therefore removes every live row of the session —
    /// the whole-session-delete case, after which the archive fallback decides
    /// whether the session has evidence.
    fn remove_stale_session_rows(
        &mut self,
        session_id: &str,
        host_ids: &BTreeSet<String>,
    ) -> rusqlite::Result<u64> {
        let stale = self.indexed_live_rows(
            "SELECT rowid, doc_key, message_id FROM doc_keys \
             WHERE session_id = ?1 AND origin = 'live' AND message_id IS NOT NULL",
            params![session_id],
            host_ids,
        )?;
        let removed = stale.len() as u64;
        self.remove_rows(stale)?;
        Ok(removed)
    }

    /// The pre-004b reconcile, kept for the upgrade round: every indexed live
    /// row whose message the host table no longer holds.
    fn remove_stale_live_rows(&mut self, host_ids: &BTreeSet<String>) -> rusqlite::Result<u64> {
        let stale = self.indexed_live_rows(
            "SELECT rowid, doc_key, message_id FROM doc_keys \
             WHERE origin = 'live' AND message_id IS NOT NULL",
            [],
            host_ids,
        )?;
        let removed = stale.len() as u64;
        self.remove_rows(stale)?;
        Ok(removed)
    }

    fn indexed_live_rows(
        &self,
        sql: &str,
        params: impl rusqlite::Params,
        host_ids: &BTreeSet<String>,
    ) -> rusqlite::Result<Vec<(i64, String)>> {
        let mut statement = self.conn.prepare(sql)?;
        let rows = statement.query_map(params, |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        let mut stale: Vec<(i64, String)> = Vec::new();
        for row in rows {
            let (rowid, doc_key, message_id) = row?;
            if !host_ids.contains(&message_id) {
                stale.push((rowid, doc_key));
            }
        }
        Ok(stale)
    }

    fn remove_rel_path(&mut self, rel_path: &str) -> rusqlite::Result<()> {
        let rows = self.select_rows(
            "SELECT rowid, doc_key FROM doc_keys WHERE rel_path = ?1",
            params![rel_path],
        )?;
        self.remove_rows(rows)
    }

    fn select_rows(
        &self,
        sql: &str,
        params: impl rusqlite::Params,
    ) -> rusqlite::Result<Vec<(i64, String)>> {
        let mut statement = self.conn.prepare(sql)?;
        let rows = statement.query_map(params, |row| Ok((row.get(0)?, row.get(1)?)))?;
        rows.collect()
    }

    pub(crate) fn insert(&mut self, unit: &Unit) -> rusqlite::Result<()> {
        let key = unit.doc_key(self.source_id);
        let digest = unit.doc_digest(self.source_id);
        let stored: Option<String> = self
            .conn
            .query_row(
                "SELECT digest FROM doc_keys WHERE doc_key = ?1",
                [&key],
                |row| row.get(0),
            )
            .ok();
        if stored.as_deref() == Some(digest.as_str()) {
            return Ok(());
        }
        self.remove_doc_key(&key)?;
        let (text, tool_input, tool_output) = match unit.kind {
            UNIT_KIND_TOOL_INPUT => (String::new(), unit.text.clone(), String::new()),
            UNIT_KIND_TOOL_OUTPUT => (String::new(), String::new(), unit.text.clone()),
            _ => (unit.text.clone(), String::new(), String::new()),
        };
        self.conn.execute(
            "INSERT INTO units(text, tool_input, tool_output, session_id, message_id, \
             record_index, pointer, locator, rel_path, ts, role, kind, session_kind, title, range_len, \
             range_lines, range_sha256, row_sha256, origin) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)",
            params![
                text,
                tool_input,
                tool_output,
                unit.session_id,
                unit.locator_message_id(),
                unit.locator.record_index.map(|value| value as i64),
                unit.locator.pointer,
                unit.locator.compact_json(),
                unit.rel_path,
                unit.timestamp_ms,
                unit.role,
                unit.kind,
                unit.session_kind,
                unit.title,
                unit.range_len as i64,
                unit.range_lines as i64,
                unit.range_sha256,
                unit.row_sha256,
                unit.origin,
            ],
        )?;
        let rowid = self.conn.last_insert_rowid();
        self.conn.execute(
            "INSERT INTO doc_keys(doc_key, rowid, session_id, message_id, rel_path, digest, origin, ts) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                key,
                rowid,
                unit.session_id,
                unit.locator_message_id(),
                unit.rel_path,
                digest,
                unit.origin,
                unit.timestamp_ms,
            ],
        )?;
        if is_conversation_unit(unit) {
            // The hybrid semantic branch scans only this owned subset: it avoids
            // touching the tool-text payloads that dominate an FTS5 full scan.
            // `title` is denormalised here so the vector stage can read its
            // metadata without joining into the FTS5 table.
            self.conn.execute(
                "INSERT INTO conversation_units(rowid, doc_key, text_sha256, session_id, \
                 message_id, ts, role, session_kind, title) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    rowid,
                    key,
                    sha256_hex(unit.text.as_bytes()),
                    unit.session_id,
                    unit.locator_message_id(),
                    unit.timestamp_ms,
                    unit.role,
                    unit.session_kind,
                    unit.title,
                ],
            )?;
        }
        self.added += 1;
        Ok(())
    }

    fn reset(&mut self) -> rusqlite::Result<()> {
        self.conn.execute("DELETE FROM units", [])?;
        self.conn.execute("DELETE FROM doc_keys", [])?;
        self.conn.execute("DELETE FROM conversation_units", [])?;
        self.conn.execute("DELETE FROM sessions", [])?;
        self.conn.execute("DELETE FROM archive_sessions", [])?;
        self.conn.execute("DELETE FROM meta", [])?;
        self.rebuilt = true;
        Ok(())
    }

    /// Remove only the archive-fallback rows of one session. Live rows of the
    /// same session (it reappeared in the live source) are never touched.
    pub(crate) fn remove_archive_session(&mut self, session_id: &str) -> rusqlite::Result<()> {
        let rows = self.select_rows(
            "SELECT rowid, doc_key FROM doc_keys WHERE session_id = ?1 AND origin = 'archive'",
            params![session_id],
        )?;
        self.remove_rows(rows)
    }

    /// True when the index already holds archive-fallback rows for a session.
    pub(crate) fn archive_rows_present(&self, session_id: &str) -> bool {
        self.conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM doc_keys WHERE session_id = ?1 AND origin = 'archive')",
                params![session_id],
                |row| row.get::<_, i64>(0),
            )
            .map(|value| value != 0)
            .unwrap_or(false)
    }

    /// Register a session the archive fallback now backs, together with the
    /// rule version that produced its rows, how many units it projected, and
    /// the carrier-group fingerprint the projection read from.
    ///
    /// A session whose projection is empty (`projected_units == 0`) is
    /// recorded as such **for that carrier state**, so the next run can skip
    /// re-reading its carrier while the evidence stays identical. A new
    /// capture changes the fingerprint and forces the session through the
    /// projector again even though the rule version is unchanged.
    pub(crate) fn remember_archive_session(
        &self,
        session_id: &str,
        projected_units: u64,
        carrier_fingerprint: &str,
    ) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO archive_sessions(session_id, rule_version, projected_units, carrier_fingerprint) VALUES (?1, ?2, ?3, ?4)",
            params![
                session_id,
                super::FALLBACK_RULE_VERSION,
                projected_units as i64,
                carrier_fingerprint
            ],
        )?;
        Ok(())
    }

    /// Registration facts of an archive-backed session; `None` when it is not
    /// registered at all. Old rows written before the bookkeeping columns
    /// existed read as all-`None` fields.
    pub(crate) fn archive_session_state(
        &self,
        session_id: &str,
    ) -> rusqlite::Result<Option<ArchiveRegistration>> {
        let mut statement = self.conn.prepare(
            "SELECT rule_version, projected_units, carrier_fingerprint FROM archive_sessions WHERE session_id = ?1",
        )?;
        let mut rows = statement.query(params![session_id])?;
        match rows.next()? {
            Some(row) => {
                let rule_version: Option<String> = row.get(0)?;
                let units: Option<i64> = row.get(1)?;
                let carrier_fingerprint: Option<String> = row.get(2)?;
                Ok(Some(ArchiveRegistration {
                    rule_version,
                    projected_units: units.map(|value| value.max(0) as u64),
                    carrier_fingerprint,
                }))
            }
            None => Ok(None),
        }
    }

    /// Archive rows this session currently has in the index.
    pub(crate) fn archive_unit_count(&self, session_id: &str) -> rusqlite::Result<u64> {
        self.conn
            .query_row(
                "SELECT COUNT(*) FROM doc_keys WHERE session_id = ?1 AND origin = 'archive'",
                params![session_id],
                |row| row.get::<_, i64>(0),
            )
            .map(|value| value.max(0) as u64)
    }

    pub(crate) fn forget_archive_session(&self, session_id: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "DELETE FROM archive_sessions WHERE session_id = ?1",
            params![session_id],
        )?;
        Ok(())
    }

    /// Sessions the live scan currently backs with live rows.
    pub(crate) fn live_sessions(&self) -> rusqlite::Result<BTreeSet<String>> {
        let mut statement = self
            .conn
            .prepare("SELECT DISTINCT session_id FROM doc_keys WHERE origin = 'live'")?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        rows.collect()
    }

    /// Sessions the registry says are currently backed by archive rows.
    pub(crate) fn archive_sessions(&self) -> rusqlite::Result<BTreeSet<String>> {
        let mut statement = self
            .conn
            .prepare("SELECT session_id FROM archive_sessions")?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        rows.collect()
    }

    /// Sessions the live OpenCode source currently holds: rows the live scan
    /// indexed **plus** sessions whose rows produced no units (every message
    /// deleted, say) but whose `session_v2` row the live scan still refreshed
    /// into the `sessions` table. Registry entries are excluded — they are the
    /// archive-fallback sessions, never "live". A session that reappeared in
    /// the live source is covered by its live rows even while it is still
    /// registered (the registry is cleaned up in the same pass).
    pub(crate) fn opencode_live_sessions(&self) -> rusqlite::Result<BTreeSet<String>> {
        let registered = self.archive_sessions()?;
        let mut live = self.live_sessions()?;
        let mut statement = self.conn.prepare("SELECT session_id FROM sessions")?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        for session in rows {
            let session = session?;
            if !registered.contains(&session) {
                live.insert(session);
            }
        }
        Ok(live)
    }
}

/// The units the hybrid semantic branch may embed and scan: user/assistant
/// conversation text only. Tool inputs and outputs stay lexical.
fn is_conversation_unit(unit: &Unit) -> bool {
    unit.kind == UNIT_KIND_TEXT && matches!(unit.role, ROLE_USER | ROLE_ASSISTANT)
}

/// The finalize queries.
///
/// Both read the **narrow** `doc_keys` table (the newest time through its `ts`
/// index): `COUNT(*)` / `MAX(ts)` over the FTS5 table would read every unit body
/// on every round (~3 GB for the OpenCode source). The regression suite asserts
/// these statements' plans never touch `units`.
///
/// `COUNT(*)` rather than a running counter on purpose: rows can disappear from
/// the index without going through the writer (a lost-rows recovery, a damaged
/// file), and the reported number has to describe what is actually there.
pub const COUNT_UNITS_SQL: &str = "SELECT COUNT(*) FROM doc_keys";
pub const NEWEST_UNIT_TS_SQL: &str = "SELECT COALESCE(MAX(ts), 0) FROM doc_keys";

fn newest_unit_ts(conn: &Connection) -> rusqlite::Result<i64> {
    conn.query_row(NEWEST_UNIT_TS_SQL, [], |row| row.get::<_, i64>(0))
}

fn count_units(conn: &Connection) -> rusqlite::Result<u64> {
    conn.query_row(COUNT_UNITS_SQL, [], |row| row.get::<_, i64>(0))
        .map(|value| value.max(0) as u64)
}

/// Close one build: the two values `meta` and `coverage` report, without reading
/// a single FTS5 row.
///
/// The values keep their meaning: the number of units in the index and the newest
/// unit timestamp inside it (0 when no unit carries a time).
struct FinalCounts {
    rows: u64,
    newest_unit_ts: i64,
}

/// The newest-unit-time floor a build must not fall below.
fn newest_floor(previous: Option<&IndexMeta>) -> i64 {
    previous.map(|meta| meta.newest_unit_ts).unwrap_or(0)
}

fn finish_counts(conn: &Connection, floor: i64) -> rusqlite::Result<FinalCounts> {
    Ok(FinalCounts {
        rows: count_units(conn)?,
        // Monotonic guard: the reported newest time never goes backwards. A row
        // deleted since the last build can leave it above the current maximum;
        // that over-report is bounded by the newest row this index ever held and
        // is accepted (README). It also means a narrow `ts` that a downgraded
        // writer left at 0 can never make the number regress.
        newest_unit_ts: newest_unit_ts(conn)?.max(floor),
    })
}

/// Why the stored index cannot be reused, or `None` when it can.
///
/// `projector_version_changed` is reported verbatim by `index`: a projection fix
/// changes which text an index contains, so an index built by older rules is not
/// merely smaller, it can be missing whole channels.
///
/// The archive-fallback rule version and the archive root join the identity: a
/// rule change means existing archive rows were built by other rules, and a
/// different archive root means they address other evidence. An inventory
/// fingerprint change is deliberately **not** a rebuild (new evidence is
/// applied incrementally).
fn rebuild_reason(
    previous: Option<&IndexMeta>,
    source: &SourceConfig,
    fallback: Option<&FallbackIdentity>,
) -> Option<&'static str> {
    let meta = previous?;
    if meta.schema_version != INDEX_SCHEMA_VERSION {
        return Some("index_schema_changed");
    }
    if meta.source_id != source.source_id
        || meta.source_kind != source.kind.as_str()
        || meta.source_root != source.root
    {
        return Some("source_identity_changed");
    }
    if meta.projector_version != projector_version(source.kind) {
        return Some("projector_version_changed");
    }
    if let Some(fallback) = fallback {
        if let Some(reason) = fallback.rebuild_reason(previous) {
            return Some(reason);
        }
    }
    None
}

/// Identity of the archive fallback for one build: resolved once per `index`
/// run from the configuration, passed to every builder.
#[derive(Debug, Clone, PartialEq)]
pub struct FallbackIdentity {
    pub rule_version: &'static str,
    /// Archive runtime root (empty when no archive source is configured).
    pub archive_root: String,
    /// Manifest-set fingerprint of the archive at preparation time.
    pub inventory_fingerprint: String,
}

impl FallbackIdentity {
    /// The rebuild decision against a stored meta. `Some` when existing archive
    /// rows were built by other rules or address other evidence. `None` —
    /// including "archive just configured, index has none yet" — updates
    /// incrementally.
    fn rebuild_reason(&self, previous: Option<&IndexMeta>) -> Option<&'static str> {
        let stored = &previous?.fallback;
        let stored = stored.as_ref()?;
        if self.archive_root.is_empty() {
            return Some("fallback_removed");
        }
        if stored.rule_version != self.rule_version {
            return Some("fallback_rule_changed");
        }
        if stored.archive_root != self.archive_root {
            return Some("fallback_archive_changed");
        }
        None
    }
}

// ---------------------------------------------------------------------------
// OpenCode (`session_message`, SQLite V2)
// ---------------------------------------------------------------------------

struct SessionRow {
    title: String,
    session_kind: &'static str,
}

/// One buffered `session_message` row: `(id, session_id, time_created, data,
/// type)`.
type OpencodeRow = (String, String, i64, Vec<u8>, String);

/// A host row that is still there but cannot project into units — NULL `data`,
/// above the per-record ceiling, or a body the projector turns into nothing
/// (`content: []`, an empty text block, invalid JSON).
///
/// A rebuild skips such a row entirely, so a round that sees it must drop the
/// live units the index still holds for that message. Otherwise an in-place
/// emptying leaves units that a rebuild never produces, and the index stops
/// being equivalent to a rebuild for good.
///
/// `oversize` separates the rows that feed `skipped_oversize` (the ceiling) from
/// a NULL body, which was never counted there.
struct SkippedRow {
    session_id: String,
    message_id: String,
    oversize: bool,
}

/// The tail read: the rows above `max_rowid`, the sessions they touch, and the
/// rows that cannot project.
type TailRead = (Vec<OpencodeRow>, BTreeSet<String>, Vec<SkippedRow>);

/// One session read: the message ids the host holds, the rows to project, and
/// the rows that cannot project.
type SessionRead = (BTreeSet<String>, Vec<OpencodeRow>, Vec<SkippedRow>);

/// Sessions a rolling reconcile pass visits over one day of 30-minute rounds.
///
/// The slice size is `ceil(sessions / OPENCODE_RECONCILE_ROUNDS)`, so one pass
/// costs about 1/48 of the corpus whatever the corpus size, and a full rotation
/// takes `ceil(sessions / slice)` rounds. A rotation is therefore bounded by 48
/// rounds (24 h) once the corpus holds at least 48 sessions; a smaller corpus
/// rotates in as many rounds as it has sessions. The bound is reported by every
/// round (`detail.opencode.window_rounds`).
pub const OPENCODE_RECONCILE_ROUNDS: u64 = 48;

/// Project buffered `session_message` rows into live units (the shared body
/// of the watermark scan and the stage-2 S5 rework-2 targeted rescan).
///
/// A row that no longer projects — invalid JSON, a non-object body, or a body
/// whose projection is empty (`content: []`, an empty text block) — drops the
/// live units the index still holds for that message. The host still holds the
/// row, so the id-set reconcile ([`Writer::remove_stale_session_rows`]) cannot
/// see it; without this prune an in-place emptying would leave units forever.
fn project_opencode_buffer(
    parts_db: Option<&Connection>,
    source: &SourceConfig,
    sessions: &BTreeMap<String, SessionRow>,
    rel_path: &str,
    buffered: Vec<OpencodeRow>,
    writer: &mut Writer<'_>,
) -> Result<(), BuildError> {
    for (message_id, session_id, time_created, data, message_type) in buffered {
        let parsed = serde_json::from_slice::<Value>(&data).ok();
        let value = match parsed {
            Some(value) if value.is_object() => value,
            // Invalid JSON and a non-object body both project to nothing.
            _ => {
                drop_units(writer, &session_id, &message_id)?;
                cost::counter("index.opencode_unprojectable_rows", 1);
                continue;
            }
        };
        let session = sessions.get(&session_id);
        let title = session
            .map(|session| session.title.clone())
            .unwrap_or_else(|| session_id.clone());
        let session_kind = session
            .map(|session| session.session_kind)
            .unwrap_or(SESSION_KIND_INTERACTIVE);
        let row_sha256 = sha256_hex(&data);
        let mut units = opencode_units(
            &session_id,
            &message_id,
            &title,
            session_kind,
            &rel_path,
            time_created,
            &message_type,
            &row_sha256,
            &value,
        );
        if let Some(db) = parts_db.filter(|_| session_parts::is_fallback(&message_type, &value)) {
            session_parts::visit_message(db, &session_id, &message_id, |part| {
                units.extend(opencode_part_unit(
                    &session_id,
                    &message_id,
                    &title,
                    session_kind,
                    rel_path,
                    time_created,
                    &part,
                ));
                true
            })
            .map_err(|error| BuildError::Failed(error.to_string()))?;
        }
        if units.is_empty() {
            drop_units(writer, &session_id, &message_id)?;
            cost::counter("index.opencode_unprojectable_rows", 1);
            continue;
        }
        let mut keys = Vec::with_capacity(units.len());
        for unit in &units {
            keys.push(unit.doc_key(&source.source_id));
            writer
                .insert(unit)
                .map_err(|error| BuildError::Failed(error.to_string()))?;
        }
        writer
            .prune_message(&session_id, &message_id, &keys)
            .map_err(|error| BuildError::Failed(error.to_string()))?;
    }
    Ok(())
}

/// Drop every live unit one message still has — the projection a rebuild would
/// produce for it is empty.
fn drop_units(
    writer: &mut Writer<'_>,
    session_id: &str,
    message_id: &str,
) -> Result<(), BuildError> {
    let removed = writer
        .prune_message(session_id, message_id, &[])
        .map_err(|error| BuildError::Failed(error.to_string()))?;
    if removed > 0 {
        cost::counter("index.opencode_unprojectable_units_removed", removed);
    }
    Ok(())
}

/// Apply [`drop_units`] to the rows a host read skipped before projection (NULL
/// `data`, above the ceiling). They are counted as unprojectable here so the
/// counter covers both halves of the same rule.
fn drop_unprojectable(writer: &mut Writer<'_>, rows: &[SkippedRow]) -> Result<(), BuildError> {
    for row in rows {
        drop_units(writer, &row.session_id, &row.message_id)?;
        cost::counter("index.opencode_unprojectable_rows", 1);
    }
    Ok(())
}

/// One round of the OpenCode live index.
///
/// # What a round reads (implementation-004b)
///
/// The batch replaces the full passes a round used to make (`SELECT COUNT(*)`
/// and `SELECT MAX(<ts>)` over `session_message`, plus the whole primary-key set
/// for deletion detection) with three bounded reads:
///
/// * **the source stamp** — `size`, `mtime` and file identity of `opencode.db`
///   and `opencode.db-wal`, plus the WAL's checkpoint counter and salts. When
///   neither file moved since the last round, the source holds what the index
///   holds, so the round skips the live scan **and** the rolling reconcile: no
///   table is read, no cursor advances, `status = unchanged`
///   (`index.opencode_unchanged = 1`).
/// * **the tail** — `MAX(rowid)` is an index dive into the rowid btree; the new
///   rows are those above the recorded `max_rowid`, read in rowid order. A row
///   appended to an idle session is visible in the same round.
/// * **the rolling slice** — `ceil(sessions / 48)` sessions, taken in a
///   deterministic order after the recorded cursor, are read through the host's
///   `session_id` index and compared message by message. This is the path that
///   sees an in-place update whose clock did not move, a row deleted without
///   leaving anything above the tail, or a write to `session_part` without a
///   parent message write. One rotation takes at most
///   [`OPENCODE_RECONCILE_ROUNDS`] rounds, so every change is visible within
///   that window (`detail.opencode.window_rounds`).
///
/// A full rebuild (`--rebuild`, identity/schema change, `MAX(rowid)` regression)
/// still reads the whole table; nothing else does. The one upgrade round of a
/// pre-004b index runs the old conservative rule once (clock watermark plus the
/// full key-set reconcile) so no change is lost, then switches to the tail.
fn build_opencode(
    source: &SourceConfig,
    index_path: &Path,
    rebuild: bool,
    fallback: Option<&super::fallback::FallbackContext>,
) -> Result<Stats, BuildError> {
    let live_scan = cost::phase("live_scan");
    let db_path = match source.db.as_deref() {
        Some(value) => PathBuf::from(value),
        None => Path::new(&source.root).join("opencode.db"),
    };
    if !db_path.is_file() {
        return Err(BuildError::Unavailable(format!(
            "opencode database does not exist: {}",
            db_path.display()
        )));
    }
    let rel_path = rel_from_root(&source.root, &db_path)
        .ok_or_else(|| BuildError::Failed("database is outside the configured root".into()))?;

    let db = open_readonly(&db_path).map_err(|error| BuildError::Unavailable(error.message()))?;
    let probed = probe(&db).map_err(|error| BuildError::Unsupported(error.message()))?;
    if probed.generation != SCHEMA_SQLITE_V2 {
        return Err(BuildError::Unsupported(format!(
            "the live index covers SQLite V2 (session_v2/session_message) only; \
             the configured database probes as {}",
            probed.generation
        )));
    }
    let has_parts =
        session_parts::available(&db).map_err(|error| BuildError::Failed(error.to_string()))?;
    let parts_db = has_parts.then_some(&db);
    let columns = table_columns(&db, "session_message")
        .map_err(|error| BuildError::Failed(error.message()))?
        .unwrap_or_default();
    let ts_column = if columns.contains("time_updated") {
        "time_updated"
    } else {
        "time_created"
    };

    // §B "skip": two `stat`s (plus the WAL's 24-byte header when a WAL exists)
    // before any table is touched. The stamp is read *before* the scan, so a
    // write that lands while the round runs is not covered by it, and the next
    // round sees a different stamp and does the work then.
    let stamp = HostStamp::read(&db_path);
    // The rightmost rowid: an index dive into the rowid btree, never a scan.
    let host_max_rowid: i64 = db
        .query_row(
            "SELECT COALESCE(MAX(rowid), 0) FROM session_message",
            [],
            |row| row.get::<_, i64>(0),
        )
        .map_err(|error| BuildError::Failed(error.to_string()))?;
    cost::counter("index.opencode_max_rowid_queries", 1);

    let mut conn = open_writable(index_path)
        .map_err(|error| BuildError::Failed(format!("index cannot be opened: {error}")))?;
    let tx = conn
        .transaction()
        .map_err(|error| BuildError::Failed(format!("index transaction failed: {error}")))?;
    let previous = IndexMeta::load(&tx)
        .map_err(|error| BuildError::Failed(format!("index meta unreadable: {error}")))?;
    let previous_watermark = previous
        .as_ref()
        .and_then(|meta| serde_json::from_value::<SqliteWatermark>(meta.watermark.clone()).ok());
    let mut writer = Writer::new(&tx, &source.source_id);
    let identity = fallback.as_ref().map(|context| context.identity());
    let identity_reason = rebuild_reason(previous.as_ref(), source, identity.as_ref());
    // A `MAX(rowid)` below the recorded one means the rowid space was renumbered
    // (VACUUM on a table without an INTEGER PRIMARY KEY) or the database was
    // replaced: the tail pointer no longer names a position, so the round falls
    // back to a full rebuild instead of resuming in the middle.
    let rowid_regression = !rebuild
        && previous_watermark
            .as_ref()
            .and_then(|watermark| watermark.max_rowid)
            .map(|stored| host_max_rowid < stored)
            .unwrap_or(false);
    let full = rebuild
        || identity_reason.is_some()
        || rowid_regression
        || previous_watermark
            .as_ref()
            .map(|watermark| {
                watermark.generation != SCHEMA_SQLITE_V2 || watermark.ts_column != ts_column
            })
            .unwrap_or(true);
    // A watermark written before this batch carries no `max_rowid`: there is no
    // tail pointer to resume from, so the upgrade round runs the pre-004b rule
    // once (clock watermark + full key-set reconcile) and records one.
    let legacy = !full
        && previous_watermark
            .as_ref()
            .and_then(|watermark| watermark.max_rowid)
            .is_none();
    let relabel = if full {
        Vec::new()
    } else {
        super::narrow_relabel_sessions(&tx)
            .map_err(|error| BuildError::Failed(format!("relabel list unreadable: {error}")))?
    };
    // A relabel list means a downgraded writer renamed units in place and this
    // round must re-project those sessions, so it is never an "unchanged" round.
    //
    // The fast path also stays off while a rotation is open
    // (`quiet_rounds < reconcile_window`): a round that saw a write may not have
    // reached the written session with its slice, and the following rounds carry
    // no write signal of their own, so the rotation has to finish before the
    // source counts as quiet.
    let stamp_matches = |watermark: &SqliteWatermark| {
        stamp
            .as_ref()
            .map(|current| current.matches(watermark))
            .unwrap_or(false)
    };
    let unchanged = !full
        && !legacy
        && relabel.is_empty()
        && previous_watermark
            .as_ref()
            .map(|watermark| {
                watermark.quiet_rounds >= watermark.reconcile_window && stamp_matches(watermark)
            })
            .unwrap_or(false);
    let stamp_changed = previous_watermark
        .as_ref()
        .map(|watermark| !stamp_matches(watermark))
        .unwrap_or(true);

    // `skipped_oversize` is a current count, so the rows it names are kept in
    // the watermark and maintained per session: a session read in full replaces
    // its entries with what it just saw, a tail adds, a whole-session delete
    // drops. A row that is no longer above the ceiling stops being counted once
    // the rotation reaches its session (bounded by `window_rounds`).
    let mut oversize: BTreeSet<OversizeRow> = previous_watermark
        .as_ref()
        .map(|watermark| watermark.oversize_rows.iter().cloned().collect())
        .unwrap_or_default();
    let mut mode = "full";
    let mut max_ts = previous_watermark
        .as_ref()
        .map(|watermark| watermark.max_ts)
        .unwrap_or(0);
    let mut cursor = previous_watermark
        .as_ref()
        .and_then(|watermark| watermark.reconcile_cursor.clone());
    let mut round = previous_watermark
        .as_ref()
        .map(|watermark| watermark.reconcile_round)
        .unwrap_or(0);
    let mut window_rounds = previous_watermark
        .as_ref()
        .map(|watermark| watermark.reconcile_window)
        .unwrap_or(0);
    let mut tail_rows = 0u64;
    let mut tail_sessions = 0u64;
    let mut reconcile_sessions = 0u64;
    let mut reconcile_rows = 0u64;
    let mut stale_removed = 0u64;
    let mut slice_len = 0u64;
    let mut quiet_rounds = previous_watermark
        .as_ref()
        .map(|watermark| watermark.quiet_rounds)
        .unwrap_or(0);
    let session_count: u64;

    if unchanged {
        // Nothing was written to the host since the last round: no table is
        // read, no cursor moves, and the round reports `unchanged`.
        mode = "unchanged";
        session_count = tx
            .query_row("SELECT COUNT(*) FROM sessions", [], |row| {
                row.get::<_, i64>(0)
            })
            .map(|value| value.max(0) as u64)
            .unwrap_or(0);
        cost::counter("index.opencode_unchanged", 1);
    } else {
        let sessions = load_sessions(&db, probed.parent_id_available)
            .map_err(|error| BuildError::Failed(error.message()))?;
        session_count = sessions.len() as u64;
        if full {
            writer
                .reset()
                .map_err(|error| BuildError::Failed(format!("index clear failed: {error}")))?;
        }
        // Session labels move independently of message rows. A title or parent
        // change is content for this index (the digest embeds both), so the
        // affected sessions are re-projected from their live rows.
        let mut renamed = update_session_columns(&tx, &sessions, full)
            .map_err(|error| BuildError::Failed(format!("session refresh failed: {error}")))?;
        for session_id in &relabel {
            if !renamed.contains(session_id) {
                renamed.push(session_id.clone());
            }
        }
        // Sessions the index mirrored last round: the difference to the host's
        // list is exactly the set whose live rows must go — a whole-session
        // delete is reconciled in the round that observes it, whether or not the
        // rolling slice reaches the session.
        let previous_sessions = if full {
            BTreeSet::new()
        } else {
            index_session_ids(&tx)
                .map_err(|error| BuildError::Failed(format!("session list unreadable: {error}")))?
        };
        refresh_sessions(&tx, &sessions).map_err(|error| {
            BuildError::Failed(format!("session table refresh failed: {error}"))
        })?;
        let removed_sessions: Vec<String> = previous_sessions
            .into_iter()
            .filter(|session_id| !sessions.contains_key(session_id))
            .collect();

        if full {
            let (buffered, skipped, observed_ts) = scan_opencode_rows(&db, ts_column, 0)?;
            // The index was just cleared, so the prune is a no-op here; the call
            // keeps one rule for every path, and the skipped rows seed the
            // oversize set.
            drop_unprojectable(&mut writer, &skipped)?;
            oversize = skipped
                .iter()
                .filter(|row| row.oversize)
                .map(|row| OversizeRow {
                    session_id: row.session_id.clone(),
                    message_id: row.message_id.clone(),
                })
                .collect();
            max_ts = max_ts.max(observed_ts);
            project_opencode_buffer(
                parts_db,
                source,
                &sessions,
                &rel_path,
                buffered,
                &mut writer,
            )?;
            // A full rebuild reconciled every session, so the source counts as
            // quiet for as long as a rotation would take.
            slice_len = reconcile_slice_len(sessions.len());
            window_rounds = reconcile_window(sessions.len());
            quiet_rounds = window_rounds;
        } else if legacy {
            // The pre-004b round, running once. It is the only place that still
            // counts the table: after it, `max_rowid` exists and the tail takes
            // over.
            mode = "legacy";
            cost::counter("index.opencode_legacy_rounds", 1);
            // The old rule cannot see an in-place text edit, so a rotation runs
            // after it (one slice per round) until the corpus has been walked.
            slice_len = reconcile_slice_len(sessions.len());
            window_rounds = reconcile_window(sessions.len());
            quiet_rounds = 0;
            let changed_since = previous_watermark
                .as_ref()
                .map(|watermark| watermark.max_ts)
                .unwrap_or(0);
            let (buffered, skipped, observed_ts) =
                scan_opencode_rows(&db, ts_column, changed_since)?;
            // Only the rows at or above the clock watermark are in hand, so the
            // oversize set is kept for the rows the host still holds and takes
            // this scan's witnesses: the rest wait for their session's turn in
            // the rotation that follows.
            let witnessed: Vec<SkippedRow> = {
                drop_unprojectable(&mut writer, &skipped)?;
                skipped.into_iter().filter(|row| row.oversize).collect()
            };
            max_ts = max_ts.max(observed_ts);
            project_opencode_buffer(
                parts_db,
                source,
                &sessions,
                &rel_path,
                buffered,
                &mut writer,
            )?;
            for session_id in &renamed {
                let skipped = reproject_opencode_session(
                    &db,
                    parts_db,
                    ts_column,
                    source,
                    &sessions,
                    &rel_path,
                    session_id,
                    &mut writer,
                )?;
                // A rename reads the session in full: its entries are replaced.
                oversize.retain(|row| &row.session_id != session_id);
                oversize.extend(
                    skipped
                        .iter()
                        .filter(|row| row.oversize)
                        .map(|row| OversizeRow {
                            session_id: row.session_id.clone(),
                            message_id: row.message_id.clone(),
                        }),
                );
            }
            // The old reconcile: every indexed live row whose message the host no
            // longer holds. This is what a fresh index (or the upgrade round)
            // needs; the rolling slice replaces it from the next round on. The
            // key set doubles as this round's row count and as the live filter
            // for the oversize set, so no `COUNT(*)` runs.
            let live_ids = opencode_message_ids(&db)?;
            cost::counter("index.opencode_legacy_rows", live_ids.len() as u64);
            oversize.retain(|row| live_ids.contains(&row.message_id));
            oversize.extend(witnessed.into_iter().map(|row| OversizeRow {
                session_id: row.session_id,
                message_id: row.message_id,
            }));
            stale_removed += writer
                .remove_stale_live_rows(&live_ids)
                .map_err(|error| BuildError::Failed(error.to_string()))?;
        } else {
            mode = "incremental";
            // (A) The tail: rows above the recorded rowid, in rowid order.
            let tail_from = previous_watermark
                .as_ref()
                .and_then(|watermark| watermark.max_rowid)
                .unwrap_or(-1);
            let (tail, changed_sessions, skipped) =
                read_opencode_tail(&db, ts_column, tail_from, &mut max_ts)?;
            drop_unprojectable(&mut writer, &skipped)?;
            // A tail witness is authoritative for its own row; it adds to the
            // current set (the row is new, so nothing of it was counted before).
            oversize.extend(
                skipped
                    .iter()
                    .filter(|row| row.oversize)
                    .map(|row| OversizeRow {
                        session_id: row.session_id.clone(),
                        message_id: row.message_id.clone(),
                    }),
            );
            tail_rows = tail.len() as u64;
            tail_sessions = changed_sessions.len() as u64;
            // (B) The deterministic rolling slice over the host's sessions.
            let host_ids: Vec<String> = sessions.keys().cloned().collect();
            let total = host_ids.len();
            slice_len = reconcile_slice_len(total);
            let start = match cursor.as_deref() {
                Some(position) => host_ids.partition_point(|id| id.as_str() <= position),
                None => 0,
            };
            let start = if total > 0 { start % total } else { 0 };
            let take = (slice_len as usize).min(total);
            let mut slice: Vec<String> = Vec::new();
            for offset in 0..take {
                slice.push(host_ids[(start + offset) % total].clone());
            }
            window_rounds = reconcile_window(total);
            if total > 0 {
                let last = (start + take - 1) % total;
                cursor = Some(host_ids[last].clone());
                // The position inside the rotation; a corpus that changed size
                // makes it approximate, which is why the guarantee reported to
                // callers is `window_rounds`, not this number.
                round = (start / slice_len as usize) as u64;
            } else {
                cursor = None;
                round = 0;
            }
            let renamed_set: BTreeSet<&String> = renamed.iter().collect();
            // (C) The union: sessions the tail touched, sessions whose label
            // changed, sessions the host no longer has, and this round's slice.
            let mut work: BTreeSet<String> = BTreeSet::new();
            work.extend(removed_sessions.iter().cloned());
            work.extend(renamed.iter().cloned());
            work.extend(slice.iter().cloned());
            // A tail row of a session the host's session table does not know (a
            // dangling row) is reconciled here so its stale rows cannot stay.
            work.extend(
                changed_sessions
                    .iter()
                    .filter(|session_id| !sessions.contains_key(*session_id))
                    .cloned(),
            );
            for session_id in &work {
                let (host_ids, buffered, skipped) =
                    read_opencode_session(&db, ts_column, session_id, true)?;
                drop_unprojectable(&mut writer, &skipped)?;
                // A full session read replaces that session's oversize entries:
                // the set stays a current count, not a tally.
                oversize.retain(|row| &row.session_id != session_id);
                oversize.extend(
                    skipped
                        .iter()
                        .filter(|row| row.oversize)
                        .map(|row| OversizeRow {
                            session_id: row.session_id.clone(),
                            message_id: row.message_id.clone(),
                        }),
                );
                reconcile_sessions += 1;
                reconcile_rows += buffered.len() as u64;
                if renamed_set.contains(session_id) {
                    // The rename cost the documented counters too: the label is
                    // content, so the session is re-projected, not relabelled.
                    cost::counter("index.session_reprojections", 1);
                    cost::counter("index.session_reprojection_rows", buffered.len() as u64);
                }
                project_opencode_buffer(
                    parts_db,
                    source,
                    &sessions,
                    &rel_path,
                    buffered,
                    &mut writer,
                )?;
                stale_removed += writer
                    .remove_stale_session_rows(session_id, &host_ids)
                    .map_err(|error| BuildError::Failed(error.to_string()))?;
            }
            // Sessions only the tail touched are not re-read in full: their tail
            // rows are already in memory, and only the host's id set is needed to
            // drop a child row deleted since the last round. Their oversize
            // entries therefore stay as the tail left them until the rotation
            // reaches the session.
            let tail_only: Vec<String> = changed_sessions
                .iter()
                .filter(|session_id| !work.contains(*session_id))
                .cloned()
                .collect();
            for session_id in &tail_only {
                let (host_ids, _, _) = read_opencode_session(&db, ts_column, session_id, false)?;
                let rows: Vec<OpencodeRow> = tail
                    .iter()
                    .filter(|row| &row.1 == session_id)
                    .cloned()
                    .collect();
                project_opencode_buffer(parts_db, source, &sessions, &rel_path, rows, &mut writer)?;
                stale_removed += writer
                    .remove_stale_session_rows(session_id, &host_ids)
                    .map_err(|error| BuildError::Failed(error.to_string()))?;
            }
            // A session the host dropped can no longer hold an oversized row.
            for session_id in &removed_sessions {
                oversize.retain(|row| &row.session_id != session_id);
            }
            cost::counter("index.opencode_reconcile_sessions", reconcile_sessions);
            cost::counter("index.opencode_reconcile_rows", reconcile_rows);
            cost::counter(
                "index.opencode_tail_reconcile_sessions",
                tail_only.len() as u64,
            );
            cost::counter("index.opencode_slice_sessions", slice.len() as u64);
            cost::counter("index.opencode_reconcile_window", window_rounds);
            cost::counter("index.opencode_reconcile_round", round);
            cost::counter("index.opencode_tail_rows", tail_rows);
            cost::counter("index.opencode_tail_sessions", tail_sessions);
            // Quiet rounds are what closes the rotation: each one visits the next
            // slice, so after `window_rounds` quiet rounds every session has been
            // read once since the last write.
            quiet_rounds = if stamp_changed { 0 } else { quiet_rounds + 1 };
        }

        // Return-to-live (stage-2 S5 rework 2, verifier B2): a session the
        // registry holds as archive-only but that exists in the live source again
        // switches back immediately — its archive rows are dropped and its rows
        // are re-scanned **by session id**, not by the time watermark (a restored
        // row can carry an older timestamp than `watermark.max_ts`). The fallback
        // step below then sees it as live and leaves it alone.
        if !full {
            let registered = writer
                .archive_sessions()
                .map_err(|error| BuildError::Failed(error.to_string()))?;
            for session_id in
                registered.intersection(&sessions.keys().cloned().collect::<BTreeSet<String>>())
            {
                let session_id = session_id.clone();
                writer
                    .remove_archive_session(&session_id)
                    .map_err(|error| BuildError::Failed(error.to_string()))?;
                writer
                    .forget_archive_session(&session_id)
                    .map_err(|error| BuildError::Failed(error.to_string()))?;
                let (_, buffered, skipped) =
                    read_opencode_session(&db, ts_column, &session_id, true)?;
                drop_unprojectable(&mut writer, &skipped)?;
                oversize.retain(|row| row.session_id != session_id);
                oversize.extend(
                    skipped
                        .iter()
                        .filter(|row| row.oversize)
                        .map(|row| OversizeRow {
                            session_id: row.session_id.clone(),
                            message_id: row.message_id.clone(),
                        }),
                );
                project_opencode_buffer(
                    parts_db,
                    source,
                    &sessions,
                    &rel_path,
                    buffered,
                    &mut writer,
                )?;
            }
        }
    }
    cost::counter("index.opencode_stale_rows_removed", stale_removed);
    let rotation_open = quiet_rounds < window_rounds;
    cost::counter("index.opencode_rotation_open", rotation_open as u64);
    // The recorded set *is* the count: a rotation that reached a session replaced
    // its entries, so this number describes the table now (up to the window).
    let skipped_oversize = oversize.len() as u64;

    drop(live_scan);
    // Archive fallback (stage-2 S5): sessions present in the evidence but gone
    // from the live database get units built from the replayed rows, marked
    // `origin = archive` in the same index. Failures degrade the detail block,
    // never the live build.
    let mut archive_report = super::fallback::Report::new();
    if let Some(context) = fallback {
        archive_report = super::fallback::apply_opencode(context, source, &tx, &mut writer)
            .unwrap_or_else(|error| super::fallback::Report::failed(error));
    }

    let finalize = cost::phase("finalize");
    let counts = finish_counts(&tx, newest_floor(previous.as_ref()))
        .map_err(|error| BuildError::Failed(format!("index finalize failed: {error}")))?;
    let rows = counts.rows;
    let mut meta = IndexMeta::new(source, Value::Null);
    meta.rows = rows;
    meta.built_at_ms = super::now_ms();
    meta.newest_unit_ts = counts.newest_unit_ts;
    meta.skipped_oversize = skipped_oversize;
    meta.watermark = serde_json::to_value(SqliteWatermark {
        kind: "sqlite-v2".to_string(),
        ts_column: ts_column.to_string(),
        // Retired: carried over so a downgraded binary reads a consistent value.
        rows: previous_watermark
            .as_ref()
            .map(|watermark| watermark.rows)
            .unwrap_or(0),
        max_ts,
        generation: SCHEMA_SQLITE_V2.to_string(),
        max_rowid: Some(host_max_rowid),
        reconcile_cursor: cursor.clone(),
        reconcile_round: round,
        reconcile_window: window_rounds,
        db_stamp: stamp.as_ref().map(|current| current.db.clone()),
        wal_stamp: stamp.as_ref().and_then(|current| current.wal.clone()),
        wal_salt: stamp
            .as_ref()
            .map(|current| current.salt.clone())
            .unwrap_or_default(),
        quiet_rounds,
        oversize_rows: oversize.into_iter().collect(),
    })
    .unwrap_or(Value::Null);
    if let Some(context) = fallback {
        if context.available() {
            meta.fallback = Some(context.meta(archive_report.archive_only_sessions));
        }
    }
    meta.store(&tx)
        .map_err(|error| BuildError::Failed(format!("index meta write failed: {error}")))?;
    // After `meta` is rewritten (that write clears the table): the narrow-table
    // fingerprint the next open compares against.
    super::record_narrow_fingerprint(&tx)
        .map_err(|error| BuildError::Failed(format!("index fingerprint write failed: {error}")))?;
    // The finalize stage ends here: the commit and the report are not part of it.
    drop(finalize);
    let added = writer.added;
    let removed = writer.removed;
    let rebuilt = writer.rebuilt;
    tx.commit()
        .map_err(|error| BuildError::Failed(format!("index commit failed: {error}")))?;

    let mut detail = json!({
        "ts_column": ts_column,
        "max_rowid": host_max_rowid,
        "max_ts": max_ts,
        "sessions": session_count,
        "opencode": {
            "mode": mode,
            "tail_rows": tail_rows,
            "tail_sessions": tail_sessions,
            "reconcile_sessions": reconcile_sessions,
            "reconcile_rows": reconcile_rows,
            "slice_sessions": slice_len,
            "stale_rows_removed": stale_removed,
            // The guarantee: a session is verified again at most this many rounds
            // after its last verification (`ceil(sessions / slice)`).
            "window_rounds": window_rounds,
            "round": round,
            "oldest_rounds": window_rounds,
            "rotation_open": rotation_open,
            "cursor": cursor,
        },
    });
    if skipped_oversize > 0 {
        detail["skipped_oversize"] = json!(skipped_oversize);
    }
    if let Some(context) = fallback {
        detail["archive"] = context.detail(&archive_report, identity_reason);
    }
    let reason = if rebuild {
        None
    } else if identity_reason.is_some() {
        identity_reason
    } else if rowid_regression {
        Some("watermark_rowid_regression")
    } else {
        None
    };
    Ok(Stats {
        rows,
        added,
        removed,
        rebuilt,
        reason,
        detail: Some(detail),
    })
}

// ---------------------------------------------------------------------------
// OpenCode round helpers
// ---------------------------------------------------------------------------

/// `opencode.db` + `opencode.db-wal` stamps and the WAL's generation marker.
struct HostStamp {
    db: SqliteStamp,
    wal: Option<SqliteStamp>,
    salt: String,
}

impl HostStamp {
    /// `None` when the database file itself cannot be stamped: the round then
    /// never takes the "unchanged" path.
    fn read(db_path: &Path) -> Option<HostStamp> {
        let db = sqlite_stamp(db_path)?;
        let wal_path = sqlite_wal_path(db_path);
        let wal = sqlite_stamp(&wal_path);
        let salt = if wal.is_some() {
            read_wal_salt(&wal_path)
        } else {
            String::new()
        };
        Some(HostStamp { db, wal, salt })
    }

    /// Both files, their identities and the WAL generation are what the
    /// watermark recorded.
    fn matches(&self, watermark: &SqliteWatermark) -> bool {
        watermark.db_stamp.as_ref() == Some(&self.db)
            && watermark.wal_stamp == self.wal
            && watermark.wal_salt == self.salt
    }
}

fn sqlite_stamp(path: &Path) -> Option<SqliteStamp> {
    let (size, mtime_ns) = file_stamp(path)?;
    Some(SqliteStamp {
        size,
        mtime_ns,
        identity: super::stamp::file_identity(path).unwrap_or_default(),
    })
}

/// `<db>-wal`, the name SQLite gives its write-ahead log.
fn sqlite_wal_path(db_path: &Path) -> PathBuf {
    let mut name = db_path.as_os_str().to_os_string();
    name.push("-wal");
    PathBuf::from(name)
}

/// Bytes 12..24 of the WAL header: the checkpoint counter and the two salts. A
/// checkpoint resets both salts and may leave the file the same size, so this is
/// what separates "rewritten" from "appended" when size and mtime are
/// inconclusive. Empty when the header cannot be read (the round then relies on
/// size and mtime).
fn read_wal_salt(path: &Path) -> String {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(_) => return String::new(),
    };
    let mut header = [0u8; 24];
    match std::io::Read::read_exact(&mut file, &mut header) {
        Ok(()) => {
            crate::cost::counter("index.opencode_wal_header_reads", 1);
            header[12..]
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect()
        }
        Err(_) => String::new(),
    }
}

/// Sessions one round reconciles, for a corpus of `total` sessions: `ceil(total / 48)`.
fn reconcile_slice_len(total: usize) -> u64 {
    if total == 0 {
        0
    } else {
        (total as u64).div_ceil(OPENCODE_RECONCILE_ROUNDS).max(1)
    }
}

/// Rounds one full rotation takes, for a corpus of `total` sessions.
fn reconcile_window(total: usize) -> u64 {
    let slice = reconcile_slice_len(total);
    if slice == 0 {
        0
    } else {
        (total as u64).div_ceil(slice)
    }
}

/// When the host database moved since the watermark was recorded, in
/// milliseconds (the newest of the database and WAL modification times);
/// `None` when both files still match.
///
/// Two `stat`s and a 24-byte WAL header read — never a table, so the freshness
/// check a query pays for cannot cost the corpus. A write the index has not seen
/// yet always moves one of the files (an in-place edit included), which is why
/// this is a *superset* of what the round itself can act on; the round's own
/// window is documented in `README.md`.
pub(crate) fn opencode_source_changed_at(
    db_path: &Path,
    watermark: &SqliteWatermark,
) -> Option<i64> {
    let stamp = HostStamp::read(db_path)?;
    if stamp.matches(watermark) {
        return None;
    }
    let mut newest = stamp.db.mtime_ns;
    if let Some(wal) = &stamp.wal {
        newest = newest.max(wal.mtime_ns);
    }
    Some((newest / 1_000_000).min(i64::MAX as u128) as i64)
}

/// `session_id`s the index currently mirrors from the host (`sessions`).
fn index_session_ids(conn: &Connection) -> rusqlite::Result<BTreeSet<String>> {
    let mut statement = conn.prepare("SELECT session_id FROM sessions")?;
    let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
    rows.collect()
}

/// Every message id the host table holds — the pre-004b reconcile's key set,
/// used by the upgrade round only.
fn opencode_message_ids(db: &Connection) -> Result<BTreeSet<String>, BuildError> {
    let mut statement = db
        .prepare("SELECT id FROM session_message")
        .map_err(|error| BuildError::Failed(error.to_string()))?;
    let rows = statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|error| BuildError::Failed(error.to_string()))?;
    let mut ids = BTreeSet::new();
    for row in rows {
        ids.insert(row.map_err(|error| BuildError::Failed(error.to_string()))?);
        cost::counter("index.opencode_host_rows_read", 1);
    }
    Ok(ids)
}

/// Rows at or above `changed_since`, in clock order. Only a full rebuild and the
/// one upgrade round call it; neither is a steady-state path.
///
/// A NULL body or a body above the ceiling is returned in the skipped list: the
/// caller drops the live units the index still holds for it (the host still has
/// the row, so an id-set reconcile would never do it).
fn scan_opencode_rows(
    db: &Connection,
    ts_column: &str,
    changed_since: i64,
) -> Result<(Vec<OpencodeRow>, Vec<SkippedRow>, i64), BuildError> {
    let query = format!(
        "SELECT id, session_id, time_created, length(data), data, type, {ts_column} \
         FROM session_message WHERE {ts_column} >= ?1 ORDER BY {ts_column}, id"
    );
    let mut statement = db
        .prepare(&query)
        .map_err(|error| BuildError::Failed(error.to_string()))?;
    let mut rows = statement
        .query([changed_since])
        .map_err(|error| BuildError::Failed(error.to_string()))?;
    let mut buffered: Vec<OpencodeRow> = Vec::new();
    let mut skipped: Vec<SkippedRow> = Vec::new();
    let mut max_ts = 0i64;
    while let Some(row) = rows
        .next()
        .map_err(|error| BuildError::Failed(error.to_string()))?
    {
        cost::counter("index.opencode_host_rows_read", 1);
        let message_id: String = row.get(0).map_err(failed)?;
        let session_id: String = row.get(1).map_err(failed)?;
        let time_created: i64 = row.get::<_, Option<i64>>(2).map_err(failed)?.unwrap_or(0);
        let data_len: i64 = row.get::<_, Option<i64>>(3).map_err(failed)?.unwrap_or(0);
        let data: Option<String> = row.get(4).map_err(failed)?;
        let message_type: Option<String> = row.get(5).map_err(failed)?;
        let ts: i64 = row.get::<_, Option<i64>>(6).map_err(failed)?.unwrap_or(0);
        if ts > max_ts {
            max_ts = ts;
        }
        let Some(data) = data else {
            skipped.push(SkippedRow {
                session_id,
                message_id,
                oversize: false,
            });
            continue;
        };
        if data_len as u64 > crate::recall::port::sqlite_v2::MAX_ROW_BYTES {
            skipped.push(SkippedRow {
                session_id,
                message_id,
                oversize: true,
            });
            continue;
        }
        buffered.push((
            message_id,
            session_id,
            time_created,
            data.into_bytes(),
            message_type.unwrap_or_default(),
        ));
    }
    Ok((buffered, skipped, max_ts))
}

fn failed(error: rusqlite::Error) -> BuildError {
    BuildError::Failed(error.to_string())
}

/// The tail: rows above `tail_from`, in rowid order, plus the ids of the sessions
/// they belong to. `max_ts` follows the clock values those rows carry. A row the
/// tail cannot hand to the projector is returned in the skipped list (see
/// [`SkippedRow`]) and its units are pruned straight from that list, so such a
/// session does not have to enter the reconcile work set.
fn read_opencode_tail(
    db: &Connection,
    ts_column: &str,
    tail_from: i64,
    max_ts: &mut i64,
) -> Result<TailRead, BuildError> {
    let query = format!(
        "SELECT id, session_id, time_created, length(data), data, type, {ts_column} \
         FROM session_message WHERE rowid > ?1 ORDER BY rowid"
    );
    let mut statement = db
        .prepare(&query)
        .map_err(|error| BuildError::Failed(error.to_string()))?;
    let mut rows = statement
        .query([tail_from])
        .map_err(|error| BuildError::Failed(error.to_string()))?;
    let mut buffered: Vec<OpencodeRow> = Vec::new();
    let mut sessions: BTreeSet<String> = BTreeSet::new();
    let mut skipped: Vec<SkippedRow> = Vec::new();
    while let Some(row) = rows
        .next()
        .map_err(|error| BuildError::Failed(error.to_string()))?
    {
        cost::counter("index.opencode_host_rows_read", 1);
        let message_id: String = row.get(0).map_err(failed)?;
        let session_id: String = row.get(1).map_err(failed)?;
        let time_created: i64 = row.get::<_, Option<i64>>(2).map_err(failed)?.unwrap_or(0);
        let data_len: i64 = row.get::<_, Option<i64>>(3).map_err(failed)?.unwrap_or(0);
        let data: Option<String> = row.get(4).map_err(failed)?;
        let message_type: Option<String> = row.get(5).map_err(failed)?;
        let ts: i64 = row.get::<_, Option<i64>>(6).map_err(failed)?.unwrap_or(0);
        if ts > *max_ts {
            *max_ts = ts;
        }
        let Some(data) = data else {
            skipped.push(SkippedRow {
                session_id,
                message_id,
                oversize: false,
            });
            continue;
        };
        if data_len as u64 > crate::recall::port::sqlite_v2::MAX_ROW_BYTES {
            skipped.push(SkippedRow {
                session_id,
                message_id,
                oversize: true,
            });
            continue;
        }
        sessions.insert(session_id.clone());
        buffered.push((
            message_id,
            session_id,
            time_created,
            data.into_bytes(),
            message_type.unwrap_or_default(),
        ));
    }
    Ok((buffered, sessions, skipped))
}

/// One session's host rows through the `session_id` index: the ids that exist
/// (for the stale-row diff), the rows themselves when `with_data`, and the rows
/// that cannot project (pruned by the caller — see [`SkippedRow`]).
fn read_opencode_session(
    db: &Connection,
    ts_column: &str,
    session_id: &str,
    with_data: bool,
) -> Result<SessionRead, BuildError> {
    let query = if with_data {
        format!(
            "SELECT id, session_id, time_created, length(data), data, type FROM session_message \
             WHERE session_id = ?1 ORDER BY {ts_column}, id"
        )
    } else {
        "SELECT id, session_id, 0, 0, NULL, type FROM session_message WHERE session_id = ?1"
            .to_string()
    };
    let mut statement = db
        .prepare(&query)
        .map_err(|error| BuildError::Failed(error.to_string()))?;
    let mut rows = statement
        .query([session_id])
        .map_err(|error| BuildError::Failed(error.to_string()))?;
    let mut ids: BTreeSet<String> = BTreeSet::new();
    let mut buffered: Vec<OpencodeRow> = Vec::new();
    let mut skipped: Vec<SkippedRow> = Vec::new();
    while let Some(row) = rows
        .next()
        .map_err(|error| BuildError::Failed(error.to_string()))?
    {
        cost::counter("index.opencode_host_rows_read", 1);
        let message_id: String = row.get(0).map_err(failed)?;
        let row_session: String = row.get(1).map_err(failed)?;
        let time_created: i64 = row.get::<_, Option<i64>>(2).map_err(failed)?.unwrap_or(0);
        let data_len: i64 = row.get::<_, Option<i64>>(3).map_err(failed)?.unwrap_or(0);
        let data: Option<String> = row.get(4).map_err(failed)?;
        let message_type: Option<String> = row.get(5).map_err(failed)?;
        ids.insert(message_id.clone());
        if !with_data {
            // The id-only read (a session the tail touched) cannot judge a row
            // body, so nothing is reported as skipped here.
            continue;
        }
        let Some(data) = data else {
            skipped.push(SkippedRow {
                session_id: row_session,
                message_id,
                oversize: false,
            });
            continue;
        };
        if data_len as u64 > crate::recall::port::sqlite_v2::MAX_ROW_BYTES {
            skipped.push(SkippedRow {
                session_id: row_session,
                message_id,
                oversize: true,
            });
            continue;
        }
        buffered.push((
            message_id,
            row_session,
            time_created,
            data.into_bytes(),
            message_type.unwrap_or_default(),
        ));
    }
    Ok((ids, buffered, skipped))
}

fn load_sessions(
    conn: &Connection,
    parent_id_available: bool,
) -> Result<BTreeMap<String, SessionRow>, crate::recall::port::PortError> {
    let sql = if parent_id_available {
        "SELECT id, title, parent_id FROM session_v2"
    } else {
        "SELECT id, title, NULL FROM session_v2"
    };
    let mut statement = conn
        .prepare(sql)
        .map_err(crate::recall::port::model::map_sqlite_error)?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        })
        .map_err(crate::recall::port::model::map_sqlite_error)?;
    let mut sessions = BTreeMap::new();
    for row in rows {
        let (id, title, parent) = row.map_err(crate::recall::port::model::map_sqlite_error)?;
        let session_kind = opencode_session_kind(parent.as_deref(), title.as_deref());
        sessions.insert(
            id,
            SessionRow {
                title: title.unwrap_or_default(),
                session_kind,
            },
        );
    }
    Ok(sessions)
}

/// Mirror the live source's session list. The table is rebuilt (delete +
/// re-insert) on **every** run, not only on full rebuilds: a session deleted
/// from the live source must leave it in the same pass — `coverage.sessions`
/// and the archive fallback's live-session set both read it (stage-2 S5
/// rework 2, verifier B1/N4). Rows are tiny and the whole build is one
/// transaction, so the cost is O(live sessions) inserts.
fn refresh_sessions(
    conn: &Connection,
    sessions: &BTreeMap<String, SessionRow>,
) -> rusqlite::Result<()> {
    conn.execute("DELETE FROM sessions", [])?;
    let mut statement = conn.prepare(
        "INSERT OR REPLACE INTO sessions(session_id, title, session_kind) VALUES (?1, ?2, ?3)",
    )?;
    for (id, session) in sessions {
        statement.execute(params![id, session.title, session.session_kind])?;
    }
    Ok(())
}

/// Sessions whose denormalised label (`title`, `session_kind`) differs from what
/// the index holds.
///
/// The label is **content** here: every unit's content digest embeds the title
/// and the kind, so relabelling in place would leave `doc_keys.digest`
/// describing the old label and the increment would stop matching a rebuild.
/// The caller re-projects each returned session from its live rows instead
/// (see [`reproject_opencode_session`]) — the same projector the watermark scan
/// uses, so the digests come out exactly as a rebuild would write them.
fn update_session_columns(
    conn: &Connection,
    sessions: &BTreeMap<String, SessionRow>,
    full: bool,
) -> rusqlite::Result<Vec<String>> {
    if full {
        return Ok(Vec::new());
    }
    let mut changed: Vec<String> = Vec::new();
    {
        let mut statement = conn.prepare("SELECT session_id, title, session_kind FROM sessions")?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        })?;
        for row in rows {
            let (id, title, kind) = row?;
            let Some(current) = sessions.get(&id) else {
                continue;
            };
            if title.as_deref() != Some(current.title.as_str())
                || kind.as_deref() != Some(current.session_kind)
            {
                changed.push(id);
            }
        }
    }
    Ok(changed)
}

/// Re-project one OpenCode session from the live database.
///
/// Reads that session's rows only (`session_id` has an index in the host
/// database), so the cost is proportional to the session, not to the source.
fn reproject_opencode_session(
    db: &Connection,
    parts_db: Option<&Connection>,
    ts_column: &str,
    source: &SourceConfig,
    sessions: &BTreeMap<String, SessionRow>,
    rel_path: &str,
    session_id: &str,
    writer: &mut Writer<'_>,
) -> Result<Vec<SkippedRow>, BuildError> {
    let query = format!(
        "SELECT id, session_id, time_created, length(data), data, type FROM session_message \
         WHERE session_id = ?1 ORDER BY {ts_column}, id"
    );
    let mut statement = db
        .prepare(&query)
        .map_err(|error| BuildError::Failed(error.to_string()))?;
    let mut buffered: Vec<(String, String, i64, Vec<u8>, String)> = Vec::new();
    let rows = statement
        .query_map([session_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<i64>>(2)?.unwrap_or(0),
                row.get::<_, Option<i64>>(3)?.unwrap_or(0),
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<String>>(5)?,
            ))
        })
        .map_err(|error| BuildError::Failed(error.to_string()))?;
    let mut skipped: Vec<SkippedRow> = Vec::new();
    for row in rows {
        let (message_id, session_id, time_created, data_len, data, message_type) =
            row.map_err(|error| BuildError::Failed(error.to_string()))?;
        let Some(data) = data else {
            skipped.push(SkippedRow {
                session_id,
                message_id,
                oversize: false,
            });
            continue;
        };
        if data_len as u64 > crate::recall::port::sqlite_v2::MAX_ROW_BYTES {
            skipped.push(SkippedRow {
                session_id,
                message_id,
                oversize: true,
            });
            continue;
        }
        buffered.push((
            message_id,
            session_id,
            time_created,
            data.into_bytes(),
            message_type.unwrap_or_default(),
        ));
    }
    if !buffered.is_empty() {
        cost::counter("index.session_reprojections", 1);
        cost::counter("index.session_reprojection_rows", buffered.len() as u64);
    }
    project_opencode_buffer(parts_db, source, sessions, rel_path, buffered, writer)?;
    drop_unprojectable(writer, &skipped)?;
    Ok(skipped)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn opencode_units(
    session_id: &str,
    message_id: &str,
    title: &str,
    session_kind: &'static str,
    rel_path: &str,
    time_created: i64,
    message_type: &str,
    row_sha256: &str,
    value: &Value,
) -> Vec<Unit> {
    let mut units = Vec::new();
    let message_role = if message_type == "user" {
        ROLE_USER
    } else {
        ROLE_ASSISTANT
    };

    if let Some(Value::Array(items)) = value.get("content") {
        for (index, item) in items.iter().enumerate() {
            let pointer = format!("/content/{index}");
            let outcome = classify_part(&PartData::Block(item.clone()));
            let locator = Locator::v2(session_id, message_id, &pointer);
            units.extend(opencode_block_units(
                session_id,
                title,
                session_kind,
                rel_path,
                time_created,
                message_role,
                row_sha256,
                locator,
                item,
                outcome,
            ));
        }
        return units;
    }

    if let Some(Value::String(text)) = value.get("text") {
        if crate::recall::port::project::is_injected(value) || text.is_empty() {
            return units;
        }
        units.push(Unit {
            session_id: session_id.to_string(),
            session_kind,
            title: title.to_string(),
            rel_path: rel_path.to_string(),
            locator: Locator::v2(session_id, message_id, "/text"),
            timestamp_ms: time_created,
            role: message_role,
            kind: UNIT_KIND_TEXT,
            text: text.clone(),
            range_len: text.len() as u64,
            range_lines: super::projection_line_end(&text),
            range_sha256: sha256_hex(text.as_bytes()),
            row_sha256: Some(row_sha256.to_string()),
            truncated: false,
            origin: ORIGIN_LIVE,
        });
    }
    units
}

/// Project one side-table text row using the same block path and unit shape
/// as an inline text item, but fingerprint the actual carrier row.
pub(crate) fn opencode_part_unit(
    session_id: &str,
    message_id: &str,
    title: &str,
    session_kind: &'static str,
    rel_path: &str,
    time_created: i64,
    part: &session_parts::TextPart,
) -> Option<Unit> {
    let value = part.value()?;
    let outcome = classify_part(&PartData::Block(value.clone()));
    opencode_block_units(
        session_id,
        title,
        session_kind,
        rel_path,
        time_created,
        ROLE_ASSISTANT,
        &sha256_hex(&part.bytes),
        Locator::v2_part(session_id, message_id, &part.id),
        &value,
        outcome,
    )
    .into_iter()
    .next()
}

/// The block-level OpenCode projector shared by the V2 row builder
/// ([`opencode_units`]) and the stage-2 S5 archive fallback (V1 `part` blocks):
/// one `text` block becomes one text unit, one `tool` block becomes separate
/// input and output units. The channel bodies differ per unit while `range_*`
/// always describe the whole block projection, so a ref of either channel
/// readback-verifies against the re-projected block.
///
/// Block parsing lives here (and in `classify_part`) only - no caller
/// re-parses blocks.
#[allow(clippy::too_many_arguments)]
pub(crate) fn opencode_block_units(
    session_id: &str,
    title: &str,
    session_kind: &'static str,
    rel_path: &str,
    time_created: i64,
    message_role: &'static str,
    row_sha256: &str,
    locator: Locator,
    block: &Value,
    outcome: PartOutcome,
) -> Vec<Unit> {
    let mut units = Vec::new();
    match outcome {
        PartOutcome::Text {
            projection,
            is_tool: false,
        } => {
            if projection.is_empty() {
                return units;
            }
            units.push(Unit {
                session_id: session_id.to_string(),
                session_kind,
                title: title.to_string(),
                rel_path: rel_path.to_string(),
                locator,
                timestamp_ms: time_created,
                role: message_role,
                kind: UNIT_KIND_TEXT,
                text: projection.clone(),
                range_len: projection.len() as u64,
                range_lines: super::projection_line_end(&projection),
                range_sha256: sha256_hex(projection.as_bytes()),
                row_sha256: Some(row_sha256.to_string()),
                truncated: false,
                origin: ORIGIN_LIVE,
            });
        }
        PartOutcome::Text {
            projection,
            is_tool: true,
        } => {
            if projection.is_empty() {
                return units;
            }
            let range_len = projection.len() as u64;
            let range_lines = super::projection_line_end(&projection);
            let range_sha256 = sha256_hex(projection.as_bytes());
            let name = crate::recall::port::project::tool_part_name(block).unwrap_or_default();
            let state = block.get("state");
            let input = state
                .and_then(|state| state.get("input"))
                .cloned()
                .unwrap_or_else(|| Value::Object(serde_json::Map::new()));
            let output = crate::recall::port::project::tool_part_output(state).unwrap_or_default();
            let input_text = format!(
                "{name}
{}",
                compact_json(&input)
            );
            if !input_text.trim().is_empty() {
                units.push(Unit {
                    session_id: session_id.to_string(),
                    session_kind,
                    title: title.to_string(),
                    rel_path: rel_path.to_string(),
                    locator: locator.clone(),
                    timestamp_ms: time_created,
                    role: ROLE_ASSISTANT,
                    kind: UNIT_KIND_TOOL_INPUT,
                    text: input_text,
                    range_len,
                    range_lines,
                    range_sha256: range_sha256.clone(),
                    row_sha256: Some(row_sha256.to_string()),
                    truncated: false,
                    origin: ORIGIN_LIVE,
                });
            }
            let (output_text, truncated) = truncate_chars(&output, TOOL_OUTPUT_MAX_CHARS);
            if !output_text.trim().is_empty() {
                units.push(Unit {
                    session_id: session_id.to_string(),
                    session_kind,
                    title: title.to_string(),
                    rel_path: rel_path.to_string(),
                    locator,
                    timestamp_ms: time_created,
                    role: ROLE_TOOL,
                    kind: UNIT_KIND_TOOL_OUTPUT,
                    text: output_text,
                    range_len,
                    range_lines,
                    range_sha256,
                    row_sha256: Some(row_sha256.to_string()),
                    truncated,
                    origin: ORIGIN_LIVE,
                });
            }
        }
        _ => {}
    }
    units
}

/// Truncate by characters (never splitting one) and report whether it cut.
pub fn truncate_chars(text: &str, max_chars: usize) -> (String, bool) {
    if text.chars().count() <= max_chars {
        return (text.to_string(), false);
    }
    (text.chars().take(max_chars).collect(), true)
}

// ---------------------------------------------------------------------------
// DSH session logs (`dsh-jsonl`)
// ---------------------------------------------------------------------------

/// One index unit per projected DSH unit. The locator keeps the logical line,
/// the block pointer and the event `seq`; nothing here depends on the physical
/// frame layout, so appending a frame keeps every existing `record_index`.
pub(crate) fn dsh_record_units(
    rel_path: &str,
    line_index: u64,
    value: &Value,
    row_sha256: &str,
    meta: &DshSessionMeta,
) -> Vec<Unit> {
    let DshEventOutcome::Units(units) = dsh::project_event(value) else {
        return Vec::new();
    };
    let timestamp_ms = dsh::event_time(value).unwrap_or(0);
    let event_seq = dsh::event_seq(value);
    let mut out = Vec::new();
    for unit in units {
        if unit.text.trim().is_empty() {
            continue;
        }
        let (indexed, truncated) = if unit.kind == UNIT_KIND_TOOL_OUTPUT {
            truncate_chars(&unit.text, TOOL_OUTPUT_MAX_CHARS)
        } else {
            (unit.text.clone(), false)
        };
        out.push(Unit {
            session_id: meta.session_id.clone(),
            session_kind: meta.session_kind,
            title: if meta.title.is_empty() {
                meta.session_id.clone()
            } else {
                meta.title.clone()
            },
            rel_path: rel_path.to_string(),
            locator: Locator::dsh(
                &meta.session_id,
                line_index,
                &unit.pointer,
                event_seq,
                Some(rel_path),
            ),
            timestamp_ms,
            role: unit.role,
            kind: unit.kind,
            text: indexed,
            range_len: unit.text.len() as u64,
            range_lines: super::projection_line_end(&unit.text),
            range_sha256: sha256_hex(unit.text.as_bytes()),
            row_sha256: Some(row_sha256.to_string()),
            truncated,
            origin: ORIGIN_LIVE,
        });
    }
    out
}

fn build_dsh(
    source: &SourceConfig,
    index_path: &Path,
    rebuild: bool,
    fallback: Option<&super::fallback::FallbackContext>,
) -> Result<Stats, BuildError> {
    let live_scan = cost::phase("live_scan");
    let inventory = dsh::inventory(Path::new(&source.root)).map_err(BuildError::Unavailable)?;
    let mut conn = open_writable(index_path)
        .map_err(|error| BuildError::Failed(format!("index cannot be opened: {error}")))?;
    let tx = conn
        .transaction()
        .map_err(|error| BuildError::Failed(format!("index transaction failed: {error}")))?;
    let previous = IndexMeta::load(&tx)
        .map_err(|error| BuildError::Failed(format!("index meta unreadable: {error}")))?;
    let identity = fallback.as_ref().map(|context| context.identity());
    let identity_reason = rebuild_reason(previous.as_ref(), source, identity.as_ref());
    let identity_changed = previous.is_none() || identity_reason.is_some();
    let mut watermark = if rebuild || identity_changed {
        DshWatermark::new()
    } else {
        previous
            .as_ref()
            .and_then(|meta| serde_json::from_value::<DshWatermark>(meta.watermark.clone()).ok())
            .unwrap_or_else(DshWatermark::new)
    };
    let mut writer = Writer::new(&tx, &source.source_id);
    if rebuild || identity_changed {
        writer
            .reset()
            .map_err(|error| BuildError::Failed(format!("index clear failed: {error}")))?;
    }

    let mut seen: Vec<String> = Vec::new();
    let mut partial_tail = 0u64;
    let mut corrupt_sessions = 0u64;
    let mut unsupported_sessions = inventory.unsupported.len() as u64;
    let mut generation_rebuilt = false;
    // Carriers of sessions an older writer renamed in place: the repair recorded
    // them and no disk byte moved since, so nothing in `decide_dsh` can see them.
    // A reset build re-projects everything anyway.
    let relabelled: Vec<String> = if rebuild || identity_changed {
        Vec::new()
    } else {
        super::narrow_relabel_paths(&tx)
            .map_err(|error| BuildError::Failed(format!("relabel list unreadable: {error}")))?
    };
    for session in &inventory.sessions {
        seen.push(session.rel_dir.clone());
        let Some((size, mtime_ns)) = file_stamp(&session.path) else {
            continue;
        };
        let identity = super::stamp::file_identity(&session.path);
        let recorded = watermark.files.get(&session.rel_dir).cloned();
        // The title is content and an older writer changed it in `units` only,
        // so every digest of this carrier still describes the old label while no
        // byte on disk moved. The repair named the session; re-read the carrier
        // so the projector writes the digests a rebuild would write. Its own
        // reason: this is not a carrier change.
        let plan = if relabelled.iter().any(|rel| rel == &session.rel_path) {
            CarrierPlan::Rescan("narrow_relabel")
        } else {
            decide_dsh(
                recorded.as_ref(),
                &session.rel_path,
                size,
                mtime_ns,
                identity.as_deref(),
                &session.path,
            )
        };
        let (mut start_line, mut must_rebuild) = match plan {
            CarrierPlan::Unchanged { fill: false } => continue,
            CarrierPlan::Unchanged { fill: true } => {
                if let Some(entry) = recorded {
                    let (head_probe, anchor_probe) = fresh_probes(&session.path, entry.size);
                    watermark.files.insert(
                        session.rel_dir.clone(),
                        DshFile {
                            head_probe,
                            anchor_probe,
                            file_identity: keep_identity(identity.as_deref(), &entry.file_identity),
                            ..entry
                        },
                    );
                }
                continue;
            }
            CarrierPlan::MtimeOnly => {
                let entry = recorded.expect("MtimeOnly requires a watermark");
                cost::counter("index.mtime_only_refresh", 1);
                watermark.files.insert(
                    session.rel_dir.clone(),
                    DshFile {
                        mtime_ns,
                        file_identity: keep_identity(identity.as_deref(), &entry.file_identity),
                        ..entry
                    },
                );
                continue;
            }
            CarrierPlan::Append { lines, .. } => (lines, false),
            // A DSH carrier's committed offset is its physical length, so
            // `decide_dsh` never produces this plan.
            CarrierPlan::CompleteTail { .. } => (0, true),
            CarrierPlan::Rescan(reason) => {
                note_rescan_reason(reason);
                (0, true)
            }
        };
        let log = match dsh::open_log(&session.path, &session.rel_path) {
            Ok(log) => log,
            Err(_) => {
                corrupt_sessions += 1;
                continue;
            }
        };
        if log.state == dsh::DshReadState::PartialTail {
            partial_tail += 1;
        }
        if log.state == dsh::DshReadState::Corrupt {
            corrupt_sessions += 1;
        }
        if start_line > log.line_count() {
            // The file grew but lost lines (restored/rewritten prefix, or a
            // watermark written by a build that counted differently): the
            // recorded cursor is not a valid logical offset any more, so the
            // carrier is read again in full. Counted like every other rescan.
            if !must_rebuild {
                note_rescan_reason("dsh_line_regression");
            }
            start_line = 0;
            must_rebuild = true;
        }
        // The session title is part of every unit's content digest, and it can
        // arrive after the messages it names (`session/title`). A rename must
        // therefore re-project the session, not relabel it: an in-place relabel
        // would leave the digests describing the old title.
        let scanned_meta = dsh::scan_meta(&log, &session.dir_name);
        if let Some(meta) = &scanned_meta {
            if !must_rebuild
                && start_line > 0
                && dsh_title_changed(&tx, &session.rel_path, meta)
                    .map_err(|error| BuildError::Failed(error.to_string()))?
            {
                note_rescan_reason("dsh_title_changed");
                start_line = 0;
                must_rebuild = true;
            }
        }
        if must_rebuild {
            if recorded
                .as_ref()
                .map(|entry| entry.file != session.rel_path)
                .unwrap_or(false)
            {
                generation_rebuilt = true;
            }
            writer
                .remove_rel_path(&session.rel_path)
                .map_err(|error| BuildError::Failed(error.to_string()))?;
            if let Some(entry) = &recorded {
                if entry.file != session.rel_path {
                    writer
                        .remove_rel_path(&entry.file)
                        .map_err(|error| BuildError::Failed(error.to_string()))?;
                }
            }
        }
        let Some(meta) = scanned_meta else {
            unsupported_sessions += 1;
            continue;
        };
        let mut lines = 0u64;
        for line in log.lines() {
            lines += 1;
            if line.index < start_line {
                continue;
            }
            let Ok(value) = serde_json::from_slice::<Value>(line.bytes) else {
                continue;
            };
            let row_sha256 = sha256_hex(line.bytes);
            for unit in dsh_record_units(&session.rel_path, line.index, &value, &row_sha256, &meta)
            {
                writer.insert(&unit).map_err(|error| {
                    BuildError::Failed(format!("{}: {error}", session.rel_path))
                })?;
            }
        }
        if log.state == dsh::DshReadState::Complete {
            let (head_probe, anchor_probe) = fresh_probes(&session.path, size);
            watermark.files.insert(
                session.rel_dir.clone(),
                DshFile {
                    file: session.rel_path.clone(),
                    size,
                    mtime_ns,
                    lines,
                    head_probe,
                    anchor_probe,
                    file_identity: keep_identity(
                        identity.as_deref(),
                        recorded
                            .as_ref()
                            .map(|entry| entry.file_identity.as_str())
                            .unwrap_or(""),
                    ),
                },
            );
        }
    }
    let stale: Vec<String> = watermark
        .files
        .keys()
        .filter(|rel| !seen.contains(rel))
        .cloned()
        .collect();
    for rel in stale {
        if let Some(entry) = watermark.files.remove(&rel) {
            writer
                .remove_rel_path(&entry.file)
                .map_err(|error| BuildError::Failed(error.to_string()))?;
        }
    }

    drop(live_scan);
    // Archive fallback (stage-2 S5): a session whose carrier is gone from the
    // live root keeps its units, built from the committed logical ranges with
    // the same DSH projector and marked `origin = archive`. A failure degrades
    // the detail block, never the live build.
    let mut archive_report = super::fallback::Report::new();
    if let Some(context) = fallback {
        archive_report = super::fallback::apply_dsh(context, source, &mut writer)
            .unwrap_or_else(super::fallback::Report::failed);
    }

    let finalize = cost::phase("finalize");
    let counts = finish_counts(&tx, newest_floor(previous.as_ref()))
        .map_err(|error| BuildError::Failed(format!("index finalize failed: {error}")))?;
    let rows = counts.rows;
    let mut meta = IndexMeta::new(source, Value::Null);
    meta.rows = rows;
    meta.built_at_ms = super::now_ms();
    meta.newest_unit_ts = counts.newest_unit_ts;
    meta.watermark = serde_json::to_value(&watermark).unwrap_or(Value::Null);
    if let Some(context) = fallback {
        if context.available() {
            meta.fallback = Some(context.meta(archive_report.archive_only_sessions));
        }
    }
    meta.store(&tx)
        .map_err(|error| BuildError::Failed(format!("index meta write failed: {error}")))?;
    // After `meta` is rewritten (that write clears the table): the narrow-table
    // fingerprint the next open compares against.
    super::record_narrow_fingerprint(&tx)
        .map_err(|error| BuildError::Failed(format!("index fingerprint write failed: {error}")))?;
    // The finalize stage ends here: the commit and the report are not part of it.
    drop(finalize);
    let added = writer.added;
    let removed = writer.removed;
    let rebuilt = writer.rebuilt;
    tx.commit()
        .map_err(|error| BuildError::Failed(format!("index commit failed: {error}")))?;

    let mut detail = json!({ "files": watermark.files.len() });
    if partial_tail > 0 {
        detail["partial_tail"] = json!(partial_tail);
    }
    if corrupt_sessions > 0 {
        detail["corrupt_sessions"] = json!(corrupt_sessions);
    }
    if unsupported_sessions > 0 {
        detail["unsupported_sessions"] = json!(unsupported_sessions);
    }
    if let Some(context) = fallback {
        detail["archive"] = context.detail(&archive_report, identity_reason);
    }
    Ok(Stats {
        rows,
        added,
        removed,
        rebuilt: rebuilt || generation_rebuilt,
        reason: if rebuild { None } else { identity_reason },
        detail: Some(detail),
    })
}

// ---------------------------------------------------------------------------
// Carrier change classification (JSONL and DSH share one table)
// ---------------------------------------------------------------------------

/// What one index round does with one carrier. The decision table is the
/// `watermark` module documentation; both carriers use it, with the committed
/// offset of a JSONL file and the committed physical length of a DSH carrier
/// playing the same role for the anchor probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CarrierPlan {
    /// Nothing changed. `fill` is set when the stored entry lacks a field this
    /// build writes (a watermark from before implementation-004a): the entry is
    /// completed from two windows, without reading the body or rescanning.
    Unchanged { fill: bool },
    /// The bytes are the same and only the timestamp moved: record the mtime,
    /// read nothing (`index.mtime_only_refresh`).
    MtimeOnly,
    /// Same length, same verified prefix, but the carrier ended in an
    /// uncommitted fragment last round and that fragment is now a complete line
    /// (the probes cannot see past the committed offset). Re-read **from the
    /// committed offset only** — the tail, never the file.
    CompleteTail { offset: u64, lines: u64 },
    /// The recorded prefix is verified: resume after it.
    Append { offset: u64, lines: u64 },
    /// Read the carrier in full; the reason names the counter.
    Rescan(&'static str),
}

/// The verdict of comparing the recorded probes with the file's prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Probes {
    Match,
    /// Different bytes, or a window that could not be read. A prefix this build
    /// could not verify is never resumed on.
    Mismatch,
    /// The watermark predates the probes: nothing to compare, so the branch
    /// keeps its pre-004a behaviour.
    Absent,
}

fn compare_probes(path: &Path, head: &str, anchor: &str, offset: u64) -> Probes {
    if head.is_empty() || anchor.is_empty() {
        return Probes::Absent;
    }
    match super::stamp::probes(path, offset) {
        Some((now_head, now_anchor)) if now_head == head && now_anchor == anchor => Probes::Match,
        _ => Probes::Mismatch,
    }
}

/// A different file at the same path (rotation, copy-then-rename, a tool that
/// writes a new file) is never an append and never a no-op. An identity that is
/// absent on either side carries no claim.
fn identity_changed(stored: &str, current: Option<&str>) -> bool {
    !stored.is_empty() && current.map(|current| current != stored).unwrap_or(false)
}

/// The identity to store. A handle that cannot be queried right now must not
/// erase an identity that is still true: erasing it would silence the
/// replacement signal for every later round, so the recorded value is kept.
fn keep_identity(current: Option<&str>, recorded: &str) -> String {
    match current {
        Some(value) if !value.is_empty() => value.to_string(),
        _ => recorded.to_string(),
    }
}

/// Does the stored entry lack a field this build writes?
fn entry_needs_fill(
    head: &str,
    anchor: &str,
    identity: &str,
    current_identity: Option<&str>,
) -> bool {
    head.is_empty() || anchor.is_empty() || (identity.is_empty() && current_identity.is_some())
}

/// One JSONL carrier's plan for this round.
fn decide_jsonl(
    recorded: Option<&JsonlFile>,
    size: u64,
    mtime_ns: u128,
    identity: Option<&str>,
    path: &Path,
    compressed: bool,
) -> CarrierPlan {
    let Some(entry) = recorded else {
        return CarrierPlan::Rescan("new_file");
    };
    if size < entry.size || size < entry.offset {
        return CarrierPlan::Rescan("size_regression");
    }
    if identity_changed(&entry.file_identity, identity) {
        return CarrierPlan::Rescan("file_identity_changed");
    }
    if size == entry.size {
        if mtime_ns == entry.mtime_ns {
            return CarrierPlan::Unchanged {
                fill: entry_needs_fill(
                    &entry.head_probe,
                    &entry.anchor_probe,
                    &entry.file_identity,
                    identity,
                ),
            };
        }
        // Same size, new timestamp: only the probes separate "touched" from
        // "rewritten in place".
        return match compare_probes(path, &entry.head_probe, &entry.anchor_probe, entry.offset) {
            // A round that stopped at a half-written final line left
            // `offset < size`. Those bytes are outside every probe window, so a
            // later round that finds them completed has to read them back: from
            // the committed offset, i.e. the tail only.
            Probes::Match if entry.offset < size => CarrierPlan::CompleteTail {
                offset: entry.offset,
                lines: entry.lines,
            },
            Probes::Match => CarrierPlan::MtimeOnly,
            Probes::Mismatch => CarrierPlan::Rescan("probe_mismatch"),
            Probes::Absent => CarrierPlan::Rescan("same_size_mtime_changed"),
        };
    }
    // A compressed carrier never appends: Codex decompresses to `.jsonl` first,
    // and then `dedupe_siblings` hands over the plain file (a different file
    // identity, so it rescans). Growth means the archive was rebuilt, and the
    // only safe answer is a full re-read of the decompressed stream.
    if compressed {
        return CarrierPlan::Rescan("compressed_grew");
    }
    // The file grew. A backwards timestamp still means "appended bytes are not
    // proven" (restored copy, clock skew, prefix rewrite): that branch is kept.
    if mtime_ns < entry.mtime_ns {
        return CarrierPlan::Rescan("mtime_regression");
    }
    match compare_probes(path, &entry.head_probe, &entry.anchor_probe, entry.offset) {
        Probes::Mismatch => CarrierPlan::Rescan("probe_mismatch"),
        // `Match` resumes after a verified prefix; `Absent` is the pre-004a rule.
        Probes::Match | Probes::Absent => CarrierPlan::Append {
            offset: entry.offset,
            lines: entry.lines,
        },
    }
}

/// One DSH carrier's plan for this round. A different carrier name is a
/// generation switch (the session was rewritten by a newer DSH), which the
/// `file_identity_changed` counter already named before this batch.
fn decide_dsh(
    recorded: Option<&DshFile>,
    rel_path: &str,
    size: u64,
    mtime_ns: u128,
    identity: Option<&str>,
    path: &Path,
) -> CarrierPlan {
    let Some(entry) = recorded else {
        return CarrierPlan::Rescan("new_file");
    };
    if entry.file != rel_path {
        return CarrierPlan::Rescan("file_identity_changed");
    }
    if size < entry.size {
        return CarrierPlan::Rescan("size_regression");
    }
    if identity_changed(&entry.file_identity, identity) {
        return CarrierPlan::Rescan("file_identity_changed");
    }
    if size == entry.size {
        if mtime_ns == entry.mtime_ns {
            return CarrierPlan::Unchanged {
                fill: entry_needs_fill(
                    &entry.head_probe,
                    &entry.anchor_probe,
                    &entry.file_identity,
                    identity,
                ),
            };
        }
        // A backwards timestamp at an unchanged length is the same case: the
        // probes decide, and equal bytes mean nothing was appended.
        return match compare_probes(path, &entry.head_probe, &entry.anchor_probe, entry.size) {
            Probes::Match => CarrierPlan::MtimeOnly,
            Probes::Mismatch => CarrierPlan::Rescan("probe_mismatch"),
            Probes::Absent => CarrierPlan::Rescan("same_size_mtime_changed"),
        };
    }
    if mtime_ns < entry.mtime_ns {
        return CarrierPlan::Rescan("mtime_regression");
    }
    match compare_probes(path, &entry.head_probe, &entry.anchor_probe, entry.size) {
        Probes::Mismatch => CarrierPlan::Rescan("probe_mismatch"),
        Probes::Match | Probes::Absent => CarrierPlan::Append {
            offset: entry.size,
            lines: entry.lines,
        },
    }
}

/// Probes for the entry just written. A window that cannot be read leaves both
/// empty: the next round reads that as "absent" (the pre-004a rule) instead of
/// trusting bytes it could not verify.
fn fresh_probes(path: &Path, offset: u64) -> (String, String) {
    super::stamp::probes(path, offset).unwrap_or_default()
}

/// Does the index hold a different title for this carrier than the one the log
/// now names? `rel_path` is indexed in `doc_keys`, so this is one lookup plus one
/// row read; a session that has no rows yet is "unchanged" (nothing to fix).
fn dsh_title_changed(
    conn: &Connection,
    rel_path: &str,
    meta: &dsh::DshSessionMeta,
) -> rusqlite::Result<bool> {
    use rusqlite::OptionalExtension;
    let stored: Option<String> = conn
        .query_row(
            "SELECT title FROM units WHERE rowid = \
             (SELECT rowid FROM doc_keys WHERE rel_path = ?1 LIMIT 1)",
            params![rel_path],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten();
    let Some(stored) = stored else {
        return Ok(false);
    };
    let expected = if meta.title.is_empty() {
        meta.session_id.clone()
    } else {
        meta.title.clone()
    };
    Ok(stored != expected)
}

fn note_rescan_reason(reason: &'static str) {
    if cost::is_recording() {
        cost::counter(&format!("index.full_rescan.{reason}"), 1);
    }
}

fn build_jsonl(
    source: &SourceConfig,
    index_path: &Path,
    rebuild: bool,
    fallback: Option<&super::fallback::FallbackContext>,
) -> Result<Stats, BuildError> {
    let live_scan = cost::phase("live_scan");
    let files = match source.kind {
        SourceKind::Codex => codex_files(source)?,
        SourceKind::ClaudeCode => claude_code::transcript_files(Path::new(&source.root))
            .map_err(BuildError::Unavailable)?,
        _ => return Err(BuildError::Unsupported("not a JSONL source".to_string())),
    };
    let mut conn = open_writable(index_path)
        .map_err(|error| BuildError::Failed(format!("index cannot be opened: {error}")))?;
    let tx = conn
        .transaction()
        .map_err(|error| BuildError::Failed(format!("index transaction failed: {error}")))?;
    let previous = IndexMeta::load(&tx)
        .map_err(|error| BuildError::Failed(format!("index meta unreadable: {error}")))?;
    let identity = fallback.as_ref().map(|context| context.identity());
    let identity_reason = rebuild_reason(previous.as_ref(), source, identity.as_ref());
    let identity_changed = previous.is_none() || identity_reason.is_some();
    let mut watermark = if rebuild || identity_changed {
        JsonlWatermark::new()
    } else {
        previous
            .as_ref()
            .and_then(|meta| serde_json::from_value::<JsonlWatermark>(meta.watermark.clone()).ok())
            .unwrap_or_else(JsonlWatermark::new)
    };
    let mut writer = Writer::new(&tx, &source.source_id);
    if rebuild || identity_changed {
        writer
            .reset()
            .map_err(|error| BuildError::Failed(format!("index clear failed: {error}")))?;
    }

    let mut seen: Vec<String> = Vec::new();
    let mut skipped_oversize = 0u64;
    let mut compressed_carriers = 0u64;
    let mut compressed_rows = 0u64;
    let mut compressed_errors = 0u64;
    for (rel_path, path) in &files {
        seen.push(rel_path.clone());
        let compressed = crate::codex_rollout_file::is_compressed(path);
        if compressed {
            compressed_carriers += 1;
        }
        let Some((size, mtime_ns)) = file_stamp(path) else {
            continue;
        };
        let identity = super::stamp::file_identity(path);
        let recorded = watermark.files.get(rel_path).cloned();
        let (start_offset, start_line, must_rebuild) = match decide_jsonl(
            recorded.as_ref(),
            size,
            mtime_ns,
            identity.as_deref(),
            path,
            compressed,
        ) {
            CarrierPlan::Unchanged { fill: false } => continue,
            CarrierPlan::Unchanged { fill: true } => {
                // A watermark written before the probes existed: complete it from
                // two windows. No rescan, no counter — this is the upgrade round.
                if let Some(entry) = recorded {
                    let (head_probe, anchor_probe) = fresh_probes(path, entry.offset);
                    watermark.files.insert(
                        rel_path.clone(),
                        JsonlFile {
                            head_probe,
                            anchor_probe,
                            file_identity: keep_identity(identity.as_deref(), &entry.file_identity),
                            ..entry
                        },
                    );
                }
                continue;
            }
            CarrierPlan::MtimeOnly => {
                let entry = recorded.expect("MtimeOnly requires a watermark");
                cost::counter("index.mtime_only_refresh", 1);
                watermark.files.insert(
                    rel_path.clone(),
                    JsonlFile {
                        mtime_ns,
                        file_identity: keep_identity(identity.as_deref(), &entry.file_identity),
                        ..entry
                    },
                );
                continue;
            }
            CarrierPlan::Append { offset, lines } => (offset, lines, false),
            CarrierPlan::CompleteTail { offset, lines } => {
                // The tail is read from the committed offset: proportional to the
                // fragment, never to the file.
                cost::counter("index.tail_completion", 1);
                (offset, lines, false)
            }
            CarrierPlan::Rescan(reason) => {
                note_rescan_reason(reason);
                (0, 0, true)
            }
        };
        if must_rebuild {
            writer
                .remove_rel_path(rel_path)
                .map_err(|error| BuildError::Failed(error.to_string()))?;
        }
        let marker = match source.kind {
            SourceKind::Codex => codex_session_marker(path),
            _ => None,
        };
        let scanned = scan_jsonl_file(
            source,
            rel_path,
            path,
            start_offset,
            start_line,
            marker.as_ref(),
            &mut writer,
        );
        let (offset, lines, oversize) = match scanned {
            Ok(value) => value,
            // A corrupt archive must not fail the whole source or leave half a
            // session in the index: drop what this carrier wrote, keep the rest,
            // and retry it next round.
            Err(_error) if compressed => {
                cost::counter("index.compressed_error", 1);
                writer
                    .remove_rel_path(rel_path)
                    .map_err(|error| BuildError::Failed(error.to_string()))?;
                compressed_errors += 1;
                continue;
            }
            Err(error) => return Err(error),
        };
        skipped_oversize += oversize;
        if compressed {
            compressed_rows += lines;
        }
        let (head_probe, anchor_probe) = fresh_probes(path, offset);
        watermark.files.insert(
            rel_path.clone(),
            JsonlFile {
                size,
                mtime_ns,
                offset,
                lines,
                head_probe,
                anchor_probe,
                file_identity: keep_identity(
                    identity.as_deref(),
                    recorded
                        .as_ref()
                        .map(|entry| entry.file_identity.as_str())
                        .unwrap_or(""),
                ),
            },
        );
    }
    let stale: Vec<String> = watermark
        .files
        .keys()
        .filter(|rel| !seen.contains(rel))
        .cloned()
        .collect();
    for rel in stale {
        writer
            .remove_rel_path(&rel)
            .map_err(|error| BuildError::Failed(error.to_string()))?;
        watermark.files.remove(&rel);
    }

    drop(live_scan);
    // Archive fallback (stage-2 S5): sessions still in the evidence but gone
    // from the live source get units built from the rebuilt carrier bytes,
    // marked `origin = archive` in the same index. Failures degrade the detail
    // block, never the live build.
    let mut archive_report = super::fallback::Report::new();
    if let Some(context) = fallback {
        archive_report = super::fallback::apply_jsonl(context, source, &mut writer)
            .unwrap_or_else(|error| super::fallback::Report::failed(error));
    }

    let finalize = cost::phase("finalize");
    let counts = finish_counts(&tx, newest_floor(previous.as_ref()))
        .map_err(|error| BuildError::Failed(format!("index finalize failed: {error}")))?;
    let rows = counts.rows;
    let mut meta = IndexMeta::new(source, Value::Null);
    meta.rows = rows;
    meta.built_at_ms = super::now_ms();
    meta.newest_unit_ts = counts.newest_unit_ts;
    meta.skipped_oversize = skipped_oversize;
    meta.watermark = serde_json::to_value(&watermark).unwrap_or(Value::Null);
    if let Some(context) = fallback {
        if context.available() {
            meta.fallback = Some(context.meta(archive_report.archive_only_sessions));
        }
    }
    meta.store(&tx)
        .map_err(|error| BuildError::Failed(format!("index meta write failed: {error}")))?;
    // After `meta` is rewritten (that write clears the table): the narrow-table
    // fingerprint the next open compares against.
    super::record_narrow_fingerprint(&tx)
        .map_err(|error| BuildError::Failed(format!("index fingerprint write failed: {error}")))?;
    // The finalize stage ends here: the commit and the report are not part of it.
    drop(finalize);
    let added = writer.added;
    let removed = writer.removed;
    let rebuilt = writer.rebuilt;
    tx.commit()
        .map_err(|error| BuildError::Failed(format!("index commit failed: {error}")))?;

    let mut detail = json!({ "files": watermark.files.len() });
    if skipped_oversize > 0 {
        detail["skipped_oversize"] = json!(skipped_oversize);
    }
    // Codex compresses old rollouts to `.jsonl.zst`; without these counts a
    // migration silently shrinks the index and no report says why.
    if source.kind == SourceKind::Codex && compressed_carriers > 0 {
        detail["compressed_carriers"] = json!(compressed_carriers);
        detail["compressed_rows"] = json!(compressed_rows);
        if compressed_errors > 0 {
            detail["compressed_errors"] = json!(compressed_errors);
        }
    }
    if let Some(context) = fallback {
        detail["archive"] = context.detail(&archive_report, identity_reason);
    }
    Ok(Stats {
        rows,
        added,
        removed,
        rebuilt,
        reason: if rebuild { None } else { identity_reason },
        detail: Some(detail),
    })
}

/// `session_meta` facts of one Codex rollout: the session id and the run source
/// (`cli`, `exec`, `vscode`, …). Read from the first line only, so an appended
/// tail is still classified correctly.
pub(crate) struct CodexMarker {
    pub(crate) session_id: Option<String>,
    pub(crate) source: Option<String>,
    /// `session_meta.payload.cwd`: the recorded working directory.
    pub(crate) cwd: Option<String>,
}

fn codex_session_marker(path: &Path) -> Option<CodexMarker> {
    // Compressed carriers are decoded transparently; a raw read would treat the
    // zstd frame as JSON and lose the session id.
    let mut reader = crate::codex_rollout_file::open_reader(path).ok()?;
    codex_marker_from_reader(&mut reader)
}

/// `session_meta` facts from any reader positioned at the start of a rollout
/// (a live file, or archive evidence rebuilt into memory).
pub(crate) fn codex_marker_from_reader<R: std::io::BufRead>(reader: &mut R) -> Option<CodexMarker> {
    let mut buffer: Vec<u8> = Vec::new();
    let line = read_line_bounded(reader, &mut buffer, INDEX_LINE_LIMIT_BYTES, true).ok()?;
    if let BoundedLine::Line { .. } = line {
    } else {
        return None;
    }
    let value: Value = serde_json::from_slice(&buffer).ok()?;
    codex_marker_from_value(&value)
}

pub(crate) fn codex_marker_from_value(value: &Value) -> Option<CodexMarker> {
    if value.get("type").and_then(Value::as_str) != Some("session_meta") {
        return None;
    }
    let payload = value.get("payload")?;
    Some(CodexMarker {
        session_id: payload
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_string),
        source: payload.get("source").map(source_marker),
        cwd: payload
            .get("cwd")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_string),
    })
}

fn scan_jsonl_file(
    source: &SourceConfig,
    rel_path: &str,
    path: &Path,
    start_offset: u64,
    start_line: u64,
    marker: Option<&CodexMarker>,
    writer: &mut Writer<'_>,
) -> Result<(u64, u64, u64), BuildError> {
    // A compressed carrier is immutable and has no seekable logical offset: read
    // it whole through the transparent decoder and record its *physical* size as
    // the offset, so the watermark's "complete" invariant (offset == size) holds
    // and probes keep comparing physical bytes.
    if crate::codex_rollout_file::is_compressed(path) {
        let size = std::fs::metadata(path)
            .map(|meta| meta.len())
            .map_err(|error| BuildError::Failed(format!("{rel_path}: {error}")))?;
        let reader = crate::codex_rollout_file::open_reader(path)
            .map_err(|error| BuildError::Failed(format!("{rel_path}: {error}")))?;
        let scanned = scan_jsonl_reader(
            source,
            rel_path,
            reader,
            0,
            0,
            marker,
            writer,
            ORIGIN_LIVE,
            None,
        )?;
        crate::cost::counter("index.scan_bytes", size);
        return Ok((size, scanned.1, scanned.2));
    }
    let mut file =
        File::open(path).map_err(|error| BuildError::Failed(format!("{rel_path}: {error}")))?;
    if start_offset > 0 {
        file.seek(SeekFrom::Start(start_offset))
            .map_err(|error| BuildError::Failed(format!("{rel_path}: {error}")))?;
    }
    let reader = BufReader::with_capacity(256 * 1024, file);
    let scanned = scan_jsonl_reader(
        source,
        rel_path,
        reader,
        start_offset,
        start_line,
        marker,
        writer,
        ORIGIN_LIVE,
        None,
    )?;
    // §A: carrier bytes this round consumed (a resume counts only the tail, a
    // full read counts the file). This is the platform-independent proof that a
    // refresh is proportional to the change.
    crate::cost::counter("index.scan_bytes", scanned.0.saturating_sub(start_offset));
    Ok(scanned)
}

/// Scan JSONL bytes from any reader (a live file, or archive evidence rebuilt
/// into memory). `origin` decides whether units are live rows or archive
/// fallback rows (the latter get the archive-fallback locator marker whose
/// `record_sha256` is the line's content address — the same digest
/// `state/records.json` indexes).
pub(crate) fn scan_jsonl_reader<R: std::io::BufRead>(
    source: &SourceConfig,
    rel_path: &str,
    mut reader: R,
    start_offset: u64,
    start_line: u64,
    marker: Option<&CodexMarker>,
    writer: &mut Writer<'_>,
    origin: &'static str,
    // Archive fallback only: when a carrier holds several session ids but
    // only some are wanted, units of the other sessions are skipped so a
    // carrier rebuild can never write rows for a session the live source
    // still serves (verifier B3).
    session_filter: Option<&BTreeSet<String>>,
) -> Result<(u64, u64, u64), BuildError> {
    let mut buffer: Vec<u8> = Vec::new();
    let mut offset = start_offset;
    let mut line_index = start_line;
    let mut skipped_oversize = 0u64;
    // Claude Code writes a mid-turn prompt twice (attachment + ordinary user
    // record). The ordinary record wins; the attachment-derived unit is held
    // until the file is done because the duplicate may come later.
    let mut dedup = (source.kind == SourceKind::ClaudeCode).then(QueuedDedup::default);
    let mut held: Vec<(String, Unit)> = Vec::new();
    loop {
        buffer.clear();
        let line = read_line_bounded(&mut reader, &mut buffer, INDEX_LINE_LIMIT_BYTES, true)
            .map_err(|error| BuildError::Failed(format!("{rel_path}: {error}")))?;
        let (consumed, terminated, too_long) = match line {
            BoundedLine::Eof => break,
            BoundedLine::Line {
                consumed,
                terminated,
                too_long,
            } => (consumed, terminated, too_long),
        };
        offset = offset.saturating_add(consumed);
        if !terminated {
            // A live append: the final fragment is not a record yet. The cursor
            // stays at the start of the fragment.
            offset = offset.saturating_sub(consumed);
            break;
        }
        if too_long {
            skipped_oversize += 1;
            line_index += 1;
            continue;
        }
        while matches!(buffer.last(), Some(b'\n') | Some(b'\r')) {
            buffer.pop();
        }
        if buffer.is_empty() {
            line_index += 1;
            continue;
        }
        let Ok(value) = serde_json::from_slice::<Value>(&buffer) else {
            // A malformed interior line is skipped; the cursor still advances so
            // it is never rescanned on every run.
            line_index += 1;
            continue;
        };
        let row_sha256 = sha256_hex(&buffer);
        let units = match source.kind {
            SourceKind::Codex => {
                codex_record_units(rel_path, line_index, &value, &row_sha256, marker)
            }
            _ => claude_record_units(rel_path, line_index, &value, &row_sha256),
        };
        for mut unit in units {
            if origin != ORIGIN_LIVE {
                if let Some(filter) = session_filter {
                    if !filter.contains(&unit.session_id) {
                        continue;
                    }
                }
                unit.origin = origin;
                unit.locator.archive_fallback =
                    Some(crate::recall::refs::ArchiveLocator::record(&row_sha256));
            }
            let verdict = match dedup.as_mut() {
                Some(dedup) => {
                    let ordinary_user_text = unit.role == ROLE_USER && unit.kind == UNIT_KIND_TEXT;
                    dedup.observe(
                        unit.locator.pointer.as_deref(),
                        ordinary_user_text,
                        &unit.text,
                    )
                }
                None => QueuedVerdict::Keep,
            };
            match verdict {
                QueuedVerdict::Drop => continue,
                QueuedVerdict::Hold => {
                    held.push((unit.text.trim().to_string(), unit));
                    continue;
                }
                QueuedVerdict::Keep => {}
            }
            writer
                .insert(&unit)
                .map_err(|error| BuildError::Failed(format!("{rel_path}: {error}")))?;
        }
        line_index += 1;
    }
    // Release the held mid-turn prompts no ordinary user record duplicated.
    if let Some(dedup) = dedup.as_mut() {
        for (key, unit) in held.drain(..) {
            if !dedup.release(&key) {
                continue;
            }
            writer
                .insert(&unit)
                .map_err(|error| BuildError::Failed(format!("{rel_path}: {error}")))?;
        }
    }
    Ok((offset, line_index, skipped_oversize))
}

pub(crate) fn codex_files(source: &SourceConfig) -> Result<Vec<(String, PathBuf)>, BuildError> {
    let root = Path::new(&source.root);
    let storage = match source.storage.as_deref() {
        Some(value) => PathBuf::from(value),
        None => root.join("sessions"),
    };
    if !storage.is_dir() {
        return Err(BuildError::Unavailable(format!(
            "codex storage directory is not available: {}",
            storage.display()
        )));
    }
    let mut paths = Vec::new();
    collect_files(&storage, &mut paths).map_err(|error| BuildError::Failed(error.to_string()))?;
    paths.retain(|path| crate::codex_rollout_file::is_rollout_file(path));
    let paths = crate::codex_rollout_file::dedupe_siblings(paths);
    let mut files = Vec::new();
    for path in paths {
        // A compressed carrier is indexed under its logical name, so the same
        // session keeps the same `hit_id` before and after Codex compresses it
        // (the retrieval path already reads `.jsonl.zst` the same way).
        let logical = crate::codex_rollout_file::logical_path(&path);
        let Some(rel) = rel_from_root(&source.root, &logical) else {
            continue;
        };
        if !rel.ends_with(".jsonl") {
            continue;
        }
        files.push((rel, path));
    }
    files.sort();
    Ok(files)
}

fn collect_files(root: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    let entries = std::fs::read_dir(root)?;
    let mut paths: Vec<(PathBuf, bool)> = Vec::new();
    for entry in entries {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            continue;
        }
        paths.push((entry.path(), file_type.is_dir()));
    }
    paths.sort();
    for (path, is_dir) in paths {
        if is_dir {
            collect_files(&path, out)?;
        } else {
            out.push(path);
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Process documents (docs): one unit per heading chunk
// ---------------------------------------------------------------------------

/// Newest docs change the index has not seen: a changed file, a file that
/// disappeared (its rows must be dropped) or an unreadable one.
pub(crate) fn docs_stale_since(source: &SourceConfig, meta: &IndexMeta) -> Option<i64> {
    let watermark = serde_json::from_value::<FilesWatermark>(meta.watermark.clone()).ok()?;
    let files = crate::recall::docs::files(source).ok()?;
    let current: Vec<&String> = files.iter().map(|(rel, _)| rel).collect();
    let mut newest: Option<i64> = if watermark
        .files
        .keys()
        .any(|rel| !current.iter().any(|seen| *seen == rel))
    {
        // A deletion makes the index stale even though no current file moved.
        Some(meta.built_at_ms)
    } else {
        None
    };
    for (rel, path) in files {
        let Some((size, mtime_ns)) = file_stamp(&path) else {
            continue;
        };
        let changed = match watermark.files.get(&rel) {
            None => true,
            Some(stamp) => stamp.size != size || stamp.mtime_ns != mtime_ns,
        };
        if changed {
            newest = Some(newest.unwrap_or(0).max((mtime_ns / 1_000_000) as i64));
        }
    }
    newest
}

fn build_docs(
    source: &SourceConfig,
    index_path: &Path,
    rebuild: bool,
) -> Result<Stats, BuildError> {
    let files = crate::recall::docs::files(source)
        .map_err(|error| BuildError::Unavailable(error.to_string()))?;
    let mut conn = open_writable(index_path)
        .map_err(|error| BuildError::Failed(format!("index cannot be opened: {error}")))?;
    let tx = conn
        .transaction()
        .map_err(|error| BuildError::Failed(format!("index transaction failed: {error}")))?;
    let previous = IndexMeta::load(&tx)
        .map_err(|error| BuildError::Failed(format!("index meta unreadable: {error}")))?;
    let identity_reason = rebuild_reason(previous.as_ref(), source, None);
    let identity_changed = previous.is_none() || identity_reason.is_some();
    let mut watermark = if rebuild || identity_changed {
        FilesWatermark::new()
    } else {
        previous
            .as_ref()
            .and_then(|meta| serde_json::from_value::<FilesWatermark>(meta.watermark.clone()).ok())
            .unwrap_or_else(FilesWatermark::new)
    };
    let mut writer = Writer::new(&tx, &source.source_id);
    if rebuild || identity_changed {
        writer
            .reset()
            .map_err(|error| BuildError::Failed(format!("index clear failed: {error}")))?;
    }

    let mut seen: Vec<String> = Vec::new();
    let mut skipped_unreadable = 0u64;
    for (rel, path) in &files {
        seen.push(rel.clone());
        let Some((size, mtime_ns)) = file_stamp(path) else {
            skipped_unreadable += 1;
            continue;
        };
        let unchanged = !rebuild
            && !identity_changed
            && watermark
                .files
                .get(rel)
                .map(|stamp| stamp.size == size && stamp.mtime_ns == mtime_ns)
                .unwrap_or(false);
        if unchanged {
            continue;
        }
        let Ok(bytes) = std::fs::read(path) else {
            skipped_unreadable += 1;
            continue;
        };
        let Ok(body) = std::str::from_utf8(&bytes) else {
            skipped_unreadable += 1;
            continue;
        };
        writer
            .remove_rel_path(rel)
            .map_err(|error| BuildError::Failed(error.to_string()))?;
        let file_title = crate::recall::docs::file_title(rel, body);
        let timestamp_ms = (mtime_ns / 1_000_000) as i64;
        for chunk in crate::recall::docs::chunks(body, &file_title) {
            let text = &body[chunk.start..chunk.end];
            let range = TextRange::from_span(body.as_bytes(), chunk.start, chunk.end);
            let mut locator = Locator::empty();
            locator.pointer = Some(format!("/chunk/{}", chunk.start));
            let unit = Unit {
                session_id: rel.clone(),
                session_kind: SESSION_KIND_UNKNOWN,
                title: chunk.title.clone(),
                rel_path: rel.clone(),
                locator,
                timestamp_ms,
                role: ROLE_DOC,
                kind: UNIT_KIND_TEXT,
                text: format!(
                    "{}
{}",
                    chunk.title, text
                ),
                range_len: (chunk.end - chunk.start) as u64,
                range_lines: range.line_end.saturating_sub(range.line_start),
                range_sha256: sha256_hex(text.as_bytes()),
                row_sha256: None,
                truncated: false,
                origin: ORIGIN_LIVE,
            };
            writer
                .insert(&unit)
                .map_err(|error| BuildError::Failed(format!("{rel}: {error}")))?;
        }
        watermark
            .files
            .insert(rel.clone(), FileStamp { size, mtime_ns });
    }

    let stale: Vec<String> = watermark
        .files
        .keys()
        .filter(|rel| !seen.contains(rel))
        .cloned()
        .collect();
    for rel in stale {
        writer
            .remove_rel_path(&rel)
            .map_err(|error| BuildError::Failed(error.to_string()))?;
        watermark.files.remove(&rel);
    }

    let finalize = cost::phase("finalize");
    let counts = finish_counts(&tx, newest_floor(previous.as_ref()))
        .map_err(|error| BuildError::Failed(format!("index finalize failed: {error}")))?;
    let rows = counts.rows;
    let mut meta = IndexMeta::new(source, Value::Null);
    meta.rows = rows;
    meta.built_at_ms = super::now_ms();
    meta.watermark = serde_json::to_value(&watermark)
        .map_err(|error| BuildError::Failed(format!("watermark serialize failed: {error}")))?;
    meta.store(&tx)
        .map_err(|error| BuildError::Failed(format!("index meta write failed: {error}")))?;
    super::record_narrow_fingerprint(&tx)
        .map_err(|error| BuildError::Failed(format!("index fingerprint write failed: {error}")))?;
    drop(finalize);
    let added = writer.added;
    let removed = writer.removed;
    let rebuilt = writer.rebuilt;
    tx.commit()
        .map_err(|error| BuildError::Failed(format!("index commit failed: {error}")))?;

    let mut detail = json!({ "files": watermark.files.len() });
    if skipped_unreadable > 0 {
        detail["skipped_unreadable"] = json!(skipped_unreadable);
    }
    Ok(Stats {
        rows,
        added,
        removed,
        rebuilt,
        reason: if rebuild { None } else { identity_reason },
        detail: Some(detail),
    })
}

pub(crate) fn codex_record_units(
    rel_path: &str,
    line_index: u64,
    value: &Value,
    row_sha256: &str,
    marker: Option<&CodexMarker>,
) -> Vec<Unit> {
    let RecordOutcome::Text(record) = project_record(value) else {
        return Vec::new();
    };
    if record.text.trim().is_empty() {
        return Vec::new();
    }
    let session_id = marker
        .and_then(|marker| marker.session_id.clone())
        .or_else(|| crate::providers::codex::infer_session_id_from_filename(Path::new(rel_path)))
        .unwrap_or_else(|| rel_path.to_string());
    let session_kind = codex_session_kind(marker.and_then(|marker| marker.source.as_deref()));
    let timestamp_ms = value
        .get("timestamp")
        .and_then(parse_timestamp_to_ms)
        .unwrap_or(0);
    let (kind, role) = codex_channel(value);
    let (text, truncated) = if kind == UNIT_KIND_TOOL_OUTPUT {
        truncate_chars(&record.text, TOOL_OUTPUT_MAX_CHARS)
    } else {
        (record.text.clone(), false)
    };
    vec![Unit {
        session_id: session_id.clone(),
        session_kind,
        title: session_id.clone(),
        rel_path: rel_path.to_string(),
        locator: Locator::codex(
            &session_id,
            record.message_id.as_deref(),
            line_index,
            &record.pointer,
        ),
        timestamp_ms,
        role,
        kind,
        text,
        range_len: record.text.len() as u64,
        range_lines: super::projection_line_end(&record.text),
        range_sha256: sha256_hex(record.text.as_bytes()),
        row_sha256: Some(row_sha256.to_string()),
        truncated,
        origin: ORIGIN_LIVE,
    }]
}

fn source_marker(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => compact_json(other),
    }
}

fn codex_channel(value: &Value) -> (&'static str, &'static str) {
    let top = value.get("type").and_then(Value::as_str).unwrap_or("");
    if top == "event_msg" {
        return (UNIT_KIND_TOOL_OUTPUT, ROLE_TOOL);
    }
    let payload = value.get("payload");
    let kind = payload
        .and_then(|payload| payload.get("type"))
        .and_then(Value::as_str)
        .unwrap_or("");
    match kind {
        "message" => {
            let role = payload
                .and_then(|payload| payload.get("role"))
                .and_then(Value::as_str)
                .unwrap_or("");
            if role == "assistant" {
                (UNIT_KIND_TEXT, ROLE_ASSISTANT)
            } else {
                (UNIT_KIND_TEXT, ROLE_USER)
            }
        }
        "function_call" | "custom_tool_call" | "local_shell_call" => {
            (UNIT_KIND_TOOL_INPUT, ROLE_ASSISTANT)
        }
        "function_call_output" | "custom_tool_call_output" => (UNIT_KIND_TOOL_OUTPUT, ROLE_TOOL),
        _ => (UNIT_KIND_TEXT, ROLE_ASSISTANT),
    }
}

pub(crate) fn claude_record_units(
    rel_path: &str,
    line_index: u64,
    value: &Value,
    row_sha256: &str,
) -> Vec<Unit> {
    let meta = claude_code::record_meta(value);
    let session_id = meta
        .session_id
        .clone()
        .or_else(|| claude_code::infer_session_id(rel_path))
        .unwrap_or_default();
    if session_id.is_empty() {
        return Vec::new();
    }
    let ClaudeRecordOutcome::Units(units) = claude_code::project_record(value) else {
        return Vec::new();
    };
    let session_kind = claude_session_kind(rel_path, meta.is_sidechain);
    let timestamp_ms = meta.timestamp_ms.unwrap_or(0);
    let mut out = Vec::new();
    for ClaudeUnit {
        pointer,
        text,
        role,
        kind,
    } in units
    {
        if text.trim().is_empty() {
            continue;
        }
        let (indexed, truncated) = if kind == UNIT_KIND_TOOL_OUTPUT {
            truncate_chars(&text, TOOL_OUTPUT_MAX_CHARS)
        } else {
            (text.clone(), false)
        };
        out.push(Unit {
            session_id: session_id.clone(),
            session_kind,
            title: session_id.clone(),
            rel_path: rel_path.to_string(),
            locator: Locator::codex(&session_id, meta.uuid.as_deref(), line_index, &pointer),
            timestamp_ms,
            role,
            kind,
            text: indexed,
            range_len: text.len() as u64,
            range_lines: super::projection_line_end(&text),
            range_sha256: sha256_hex(text.as_bytes()),
            row_sha256: Some(row_sha256.to_string()),
            truncated,
            origin: ORIGIN_LIVE,
        });
    }
    out
}

// ---------------------------------------------------------------------------
// Device notes (one markdown file = one unit)
// ---------------------------------------------------------------------------

fn pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| Regex::new(DEVICE_FILE_PATTERN).expect("device pattern compiles"))
}

/// The device-note filename predicate, shared with the coverage check.
pub(crate) fn device_pattern() -> &'static Regex {
    pattern()
}

fn build_files(
    source: &SourceConfig,
    index_path: &Path,
    rebuild: bool,
) -> Result<Stats, BuildError> {
    let root = Path::new(&source.root);
    let entries = std::fs::read_dir(root)
        .map_err(|error| BuildError::Unavailable(format!("device root cannot be read: {error}")))?;
    let mut conn = open_writable(index_path)
        .map_err(|error| BuildError::Failed(format!("index cannot be opened: {error}")))?;
    let tx = conn
        .transaction()
        .map_err(|error| BuildError::Failed(format!("index transaction failed: {error}")))?;
    let previous = IndexMeta::load(&tx)
        .map_err(|error| BuildError::Failed(format!("index meta unreadable: {error}")))?;
    let identity_reason = rebuild_reason(previous.as_ref(), source, None);
    let identity_changed = previous.is_none() || identity_reason.is_some();
    let mut watermark = if rebuild || identity_changed {
        FilesWatermark::new()
    } else {
        previous
            .as_ref()
            .and_then(|meta| serde_json::from_value::<FilesWatermark>(meta.watermark.clone()).ok())
            .unwrap_or_else(FilesWatermark::new)
    };
    let mut writer = Writer::new(&tx, &source.source_id);
    if rebuild || identity_changed {
        writer
            .reset()
            .map_err(|error| BuildError::Failed(format!("index clear failed: {error}")))?;
    }

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
        if name.starts_with('.') || !pattern().is_match(&name) {
            continue;
        }
        let path = entry.path();
        let Some((size, mtime_ns)) = file_stamp(&path) else {
            continue;
        };
        seen.push(name.clone());
        let unchanged = !rebuild
            && !identity_changed
            && watermark
                .files
                .get(&name)
                .map(|stamp| stamp.size == size && stamp.mtime_ns == mtime_ns)
                .unwrap_or(false);
        if unchanged {
            continue;
        }
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        let Ok(body) = std::str::from_utf8(&bytes) else {
            continue;
        };
        writer
            .remove_rel_path(&name)
            .map_err(|error| BuildError::Failed(error.to_string()))?;
        let timestamp_ms = chrono::NaiveDate::parse_from_str(&name[..10], "%Y-%m-%d")
            .ok()
            .and_then(|date| date.and_hms_opt(0, 0, 0))
            .map(|stamp| stamp.and_utc().timestamp_millis())
            .unwrap_or(0);
        let (meta, note_body) = crate::recall::device_meta::split(body);
        let title = crate::recall::device_meta::title(&meta, note_body, &name);
        let tags = crate::recall::device_meta::tags(&meta, note_body);
        // The summary and tags are indexed through the same text channel; the
        // tags are repeated once so a tag hit outranks an incidental body word.
        let tags_line = if tags.is_empty() {
            String::new()
        } else {
            format!("{}\n", tags.join(" "))
        };
        let summary = meta.summary.as_deref().unwrap_or("");
        let unit = Unit {
            session_id: name.clone(),
            session_kind: SESSION_KIND_UNKNOWN,
            title,
            rel_path: name.clone(),
            locator: Locator::empty(),
            timestamp_ms,
            role: ROLE_USER,
            kind: UNIT_KIND_TEXT,
            // Body first: a body term yields a body snippet. The frontmatter
            // *keys* are not indexed (every event has `status`, `schema`, …);
            // its summary and tags are, so they stay searchable.
            text: format!("{note_body}\n{name}\n{summary}\n{tags_line}"),
            range_len: body.len() as u64,
            range_lines: super::projection_line_end(body),
            range_sha256: sha256_hex(body.as_bytes()),
            row_sha256: None,
            truncated: false,
            origin: ORIGIN_LIVE,
        };
        writer
            .insert(&unit)
            .map_err(|error| BuildError::Failed(error.to_string()))?;
        watermark.files.insert(name, FileStamp { size, mtime_ns });
    }
    let stale: Vec<String> = watermark
        .files
        .keys()
        .filter(|name| !seen.contains(name))
        .cloned()
        .collect();
    for name in stale {
        writer
            .remove_rel_path(&name)
            .map_err(|error| BuildError::Failed(error.to_string()))?;
        watermark.files.remove(&name);
    }

    let finalize = cost::phase("finalize");
    let counts = finish_counts(&tx, newest_floor(previous.as_ref()))
        .map_err(|error| BuildError::Failed(format!("index finalize failed: {error}")))?;
    let rows = counts.rows;
    let mut meta = IndexMeta::new(source, Value::Null);
    meta.rows = rows;
    meta.built_at_ms = super::now_ms();
    meta.newest_unit_ts = counts.newest_unit_ts;
    meta.watermark = serde_json::to_value(&watermark).unwrap_or(Value::Null);
    meta.store(&tx)
        .map_err(|error| BuildError::Failed(format!("index meta write failed: {error}")))?;
    // After `meta` is rewritten (that write clears the table): the narrow-table
    // fingerprint the next open compares against.
    super::record_narrow_fingerprint(&tx)
        .map_err(|error| BuildError::Failed(format!("index fingerprint write failed: {error}")))?;
    // The finalize stage ends here: the commit and the report are not part of it.
    drop(finalize);
    let added = writer.added;
    let removed = writer.removed;
    let rebuilt = writer.rebuilt;
    tx.commit()
        .map_err(|error| BuildError::Failed(format!("index commit failed: {error}")))?;

    Ok(Stats {
        rows,
        added,
        removed,
        rebuilt,
        reason: if rebuild { None } else { identity_reason },
        detail: Some(json!({ "files": watermark.files.len() })),
    })
}

// ---------------------------------------------------------------------------
// Hermes notes (one SQLite `state.db`; codex-shaped locators)
// ---------------------------------------------------------------------------

/// The Hermes build: a whole-store pass whose increment is the **database and
/// WAL file stamps**. Hermes timestamps can regress (compaction re-inserts rows
/// with their original time), so the rowid clock the OpenCode build uses is not
/// a safe increment; a changed stamp re-reads the whole store, which stays cheap
/// for a personal agent's database.
///
/// One projected row (active=1 user/assistant content) = one unit, addressed
/// by `Locator::codex(session_id, Some(id), id, "/content")`.
fn build_hermes(
    source: &SourceConfig,
    index_path: &Path,
    rebuild: bool,
) -> Result<Stats, BuildError> {
    let db_path = source.db.clone().unwrap_or_default();
    if db_path.is_empty() {
        return Err(BuildError::Unavailable(
            "hermes source has no db path configured".to_string(),
        ));
    }
    let db_file = Path::new(&db_path);
    if !db_file.is_file() {
        return Err(BuildError::Unavailable(format!(
            "hermes db is not a file: {db_path}"
        )));
    }
    let stamp = HostStamp::read(db_file).ok_or_else(|| {
        BuildError::Unavailable(format!("hermes db cannot be stamped: {db_path}"))
    })?;
    let mut conn = open_writable(index_path)
        .map_err(|error| BuildError::Failed(format!("index cannot be opened: {error}")))?;
    let tx = conn
        .transaction()
        .map_err(|error| BuildError::Failed(format!("index transaction failed: {error}")))?;
    let previous = IndexMeta::load(&tx)
        .map_err(|error| BuildError::Failed(format!("index meta unreadable: {error}")))?;
    let identity_reason = rebuild_reason(previous.as_ref(), source, None);
    let identity_changed = previous.is_none() || identity_reason.is_some();
    let previous_watermark: Option<SqliteWatermark> = previous
        .as_ref()
        .and_then(|meta| serde_json::from_value::<SqliteWatermark>(meta.watermark.clone()).ok());
    let stamp_unchanged = previous_watermark
        .as_ref()
        .map(|watermark| stamp.matches(watermark))
        .unwrap_or(false);
    if !rebuild && !identity_changed && stamp_unchanged {
        let counts = finish_counts(&tx, newest_floor(previous.as_ref()))
            .map_err(|error| BuildError::Failed(format!("index finalize failed: {error}")))?;
        let rows = counts.rows;
        drop(tx);
        return Ok(Stats {
            rows,
            added: 0,
            removed: 0,
            rebuilt: false,
            reason: None,
            detail: Some(json!({ "unchanged": true, "db": db_path })),
        });
    }
    let mut writer = Writer::new(&tx, &source.source_id);
    writer
        .reset()
        .map_err(|error| BuildError::Failed(format!("index clear failed: {error}")))?;

    let readonly = hermes::open_db(source, Path::new(&source.root))
        .map_err(|error| BuildError::Unavailable(format!("hermes db cannot be opened: {error}")))?;
    let sessions = hermes::load_sessions(&readonly)
        .map_err(|error| BuildError::Failed(format!("hermes sessions query failed: {error}")))?;
    let mut added = 0u64;
    let mut sessions_scanned = 0u64;
    let mut messages_scanned = 0u64;
    for session in &sessions {
        sessions_scanned += 1;
        let session_id = &session.id;
        let display_title = session.title.clone();
        let messages = hermes::load_messages(&readonly, session_id).map_err(|error| {
            BuildError::Failed(format!("hermes messages query failed: {error}"))
        })?;
        for message in messages {
            messages_scanned += 1;
            let id = message.id;
            let content = message.content;
            let timestamp_ms = if message.timestamp_ms == 0 {
                session.last_activity_ms.unwrap_or(0)
            } else {
                message.timestamp_ms
            };
            let unit = Unit {
                session_id: session_id.clone(),
                session_kind: SESSION_KIND_INTERACTIVE,
                title: display_title.clone().unwrap_or_else(|| session_id.clone()),
                rel_path: db_path.replace('\\', "/"),
                locator: Locator::codex(session_id, Some(&id.to_string()), id as u64, "/content"),
                timestamp_ms,
                role: message.role,
                kind: UNIT_KIND_TEXT,
                text: content.clone(),
                range_len: content.len() as u64,
                range_lines: super::projection_line_end(&content),
                range_sha256: sha256_hex(content.as_bytes()),
                row_sha256: Some(sha256_hex(format!("{}:{}", id, content).as_bytes())),
                truncated: false,
                origin: ORIGIN_LIVE,
            };
            writer
                .insert(&unit)
                .map_err(|error| BuildError::Failed(error.to_string()))?;
            added += 1;
        }
    }

    let finalize = cost::phase("finalize");
    let counts = finish_counts(&tx, newest_floor(previous.as_ref()))
        .map_err(|error| BuildError::Failed(format!("index finalize failed: {error}")))?;
    let rows = counts.rows;
    let mut meta = IndexMeta::new(source, Value::Null);
    meta.rows = rows;
    meta.built_at_ms = super::now_ms();
    meta.newest_unit_ts = counts.newest_unit_ts;
    meta.watermark = serde_json::to_value(SqliteWatermark {
        kind: "hermes".to_string(),
        ts_column: "last_activity_at".to_string(),
        rows: previous_watermark
            .as_ref()
            .map(|watermark| watermark.rows)
            .unwrap_or(0),
        max_ts: 0,
        generation: SCHEMA_HERMES_SQLITE_V1.to_string(),
        max_rowid: None,
        reconcile_cursor: None,
        reconcile_round: 0,
        reconcile_window: 0,
        db_stamp: Some(stamp.db),
        wal_stamp: stamp.wal,
        wal_salt: stamp.salt,
        quiet_rounds: 0,
        oversize_rows: Vec::new(),
    })
    .unwrap_or(Value::Null);
    meta.store(&tx)
        .map_err(|error| BuildError::Failed(format!("index meta write failed: {error}")))?;
    super::record_narrow_fingerprint(&tx)
        .map_err(|error| BuildError::Failed(format!("index fingerprint write failed: {error}")))?;
    drop(finalize);
    let removed = writer.removed;
    let rebuilt = writer.rebuilt;
    tx.commit()
        .map_err(|error| BuildError::Failed(format!("index commit failed: {error}")))?;
    Ok(Stats {
        rows,
        added,
        removed,
        rebuilt,
        reason: if rebuild { None } else { identity_reason },
        detail: Some(json!({
            "db": db_path,
            "sessions": sessions_scanned,
            "messages": messages_scanned,
        })),
    })
}
