//! Hermes session source (`hermes-sqlite-v1`), read-only.
//!
//! Layout: one SQLite `state.db` explicitly configured as `db` (the Hermes
//! home's `state.db`). Two tables matter, per the cc-switch hermes provider
//! schema and the S4.5 synthetic capture fixture:
//!
//! * `sessions(id TEXT PRIMARY KEY, cwd, title, started_at REAL,
//!   ended_at REAL, last_activity_at REAL)` — epoch **seconds**, REAL;
//! * `messages(id INTEGER PRIMARY KEY, session_id, role, content, …,
//!   timestamp REAL NOT NULL, active INTEGER DEFAULT 1, display_metadata)`.
//!
//! Projection (one definition for search, the live index, `memory_read` and
//! `memory_recent`): user/assistant `content` rows with `active = 1`. Tool
//! payloads (`tool_calls` / `tool_name` / `tool_call_id`), `reasoning` and
//! model-only micro-compaction rows (`display_metadata.model_only`) are not
//! projected — they are context the model reads, not a message somebody
//! typed. The insertion order (`ORDER BY id`) is authoritative: Hermes
//! timestamps can regress (clock skew, compaction re-inserting rows with
//! their original time).
//!
//! Addressing: `Locator::codex`-shaped (`record_index` = the message `id`,
//! pointer `/content`), schema `hermes-sqlite-v1`. Seconds are converted to
//! milliseconds once, at the projection edge; every internal timestamp is ms.
//!
//! WAL: opened read-only; a writer's committed rows are visible to new
//! queries. A busy database degrades the request (`DB_LOCKED`) instead of
//! silently answering from stale pages.

use crate::recall::budget::Budget;
use crate::recall::config::SourceConfig;
use crate::recall::contract::{
    codes, status, RANK_BASIS_LEXICAL_WINDOW, ROLE_ASSISTANT, ROLE_USER, SCHEMA_HERMES_SQLITE_V1,
    SCORE_KIND_NONE,
};
use crate::recall::hash::{root_sha256, sha256_hex, TextRange};
use crate::recall::providers::{rank_hits, SourceHit, SourceOutcome};
use crate::recall::refs::{Fingerprint, HitRef, Locator};
use crate::recall::snapshot::Window;
use crate::recall::text::{fold_ascii, snippet};
use rusqlite::Connection;
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};
use std::time::Instant;

/// One projected message: the search/index/read unit.
pub(crate) struct HermesMessage {
    pub id: i64,
    pub role: &'static str,
    pub content: String,
    /// Milliseconds; `0` when the row's timestamp is missing.
    pub timestamp_ms: i64,
}

/// Open the configured database read-only. `read_root` follows the strict
/// snapshot when one is active.
pub(crate) fn open_db(
    source: &SourceConfig,
    read_root: &Path,
) -> Result<Connection, rusqlite::Error> {
    let mapped =
        crate::recall::read::remap_configured(read_root, &source.root, source.db.as_deref())
            .unwrap_or_else(|| source.db.clone().unwrap_or_default());
    if mapped.is_empty() {
        return Err(open_error("hermes source has no db path configured"));
    }
    let path = PathBuf::from(&mapped);
    if !path.is_file() {
        return Err(open_error(format!("hermes db is not a file: {mapped}")));
    }
    Connection::open_with_flags(
        &path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
}

fn open_error(message: impl Into<String>) -> rusqlite::Error {
    rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CANTOPEN),
        Some(message.into()),
    )
}

pub(crate) fn db_error_code(error: &rusqlite::Error) -> &'static str {
    match error {
        rusqlite::Error::SqliteFailure(error, _)
            if matches!(
                error.code,
                rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
            ) =>
        {
            codes::DB_LOCKED
        }
        _ => codes::DB_UNAVAILABLE,
    }
}

fn map_db_error(source: &SourceConfig, error: rusqlite::Error) -> SourceOutcome {
    let code = db_error_code(&error);
    let message = if code == codes::DB_LOCKED {
        "hermes db is busy; retry after the writer commits".to_string()
    } else {
        format!("hermes db query failed: {error}")
    };
    SourceOutcome {
        source_id: source.source_id.clone(),
        status: if code == codes::DB_LOCKED {
            status::DEGRADED
        } else {
            status::UNAVAILABLE
        },
        code: Some(code),
        detail: None,
        hits: Vec::new(),
        warnings: Vec::new(),
        error_message: Some(message),
        side_effects: Vec::new(),
        violations: Vec::new(),
    }
}

/// Indexed answers must not masquerade as live when the configured source has
/// disappeared, become corrupt, or is locked. Preparing both fixed projections
/// validates the schema without reading message bodies.
pub fn source_health(source: &SourceConfig, read_root: &Path) -> Option<SourceOutcome> {
    let conn = match open_db(source, read_root) {
        Ok(conn) => conn,
        Err(error) => return Some(map_db_error(source, error)),
    };
    for sql in [
        "SELECT id, title, cwd, COALESCE(last_activity_at, started_at) FROM sessions LIMIT 1",
        "SELECT id, session_id, role, content, timestamp, display_metadata FROM messages \
         WHERE active = 1 LIMIT 1",
    ] {
        if let Err(error) = conn.prepare(sql) {
            return Some(map_db_error(source, error));
        }
    }
    None
}

fn model_only(metadata: Option<&str>) -> bool {
    metadata
        .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
        .and_then(|value| value.get("model_only").and_then(Value::as_bool))
        .unwrap_or(false)
}

/// Load one session's projected messages, insertion order.
pub(crate) fn load_messages(
    conn: &Connection,
    session_id: &str,
) -> Result<Vec<HermesMessage>, rusqlite::Error> {
    let mut stmt = conn.prepare_cached(
        "SELECT id, role, content, timestamp, display_metadata FROM messages \
         WHERE session_id = ?1 AND active = 1 AND role IN ('user', 'assistant') \
           AND content IS NOT NULL AND content != '' \
         ORDER BY id ASC",
    )?;
    let rows = stmt
        .query_map([session_id], |row| {
            let id: i64 = row.get(0)?;
            let role: String = row.get(1)?;
            let content: String = row.get(2)?;
            let timestamp: Option<f64> = row.get(3)?;
            let metadata: Option<String> = row.get(4)?;
            Ok((id, role, content, timestamp, metadata))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows
        .into_iter()
        .filter(|(_, _, _, _, metadata)| !model_only(metadata.as_deref()))
        .map(|(id, role, content, timestamp, _)| HermesMessage {
            id,
            role: if role == "assistant" {
                ROLE_ASSISTANT
            } else {
                ROLE_USER
            },
            content,
            timestamp_ms: timestamp
                .map(|value| (value * 1000.0).round() as i64)
                .unwrap_or(0),
        })
        .collect())
}

/// Sessions metadata: id, title, cwd and last-activity milliseconds.
pub(crate) struct HermesSession {
    pub id: String,
    pub title: Option<String>,
    pub cwd: Option<String>,
    pub last_activity_ms: Option<i64>,
}

#[derive(Default)]
pub struct ScanFilters<'a> {
    pub session_id: Option<&'a str>,
    pub since_ms: Option<i64>,
    pub until_ms: Option<i64>,
    pub roles: Option<&'a [String]>,
    pub kinds: Option<&'a [String]>,
    pub session_kinds: Option<&'a [String]>,
}

impl ScanFilters<'_> {
    fn allows(&self, message: &HermesMessage) -> bool {
        if self
            .since_ms
            .is_some_and(|since| message.timestamp_ms < since)
            || self
                .until_ms
                .is_some_and(|until| message.timestamp_ms >= until)
        {
            return false;
        }
        if self
            .roles
            .is_some_and(|roles| !roles.iter().any(|role| role == message.role))
        {
            return false;
        }
        if self.kinds.is_some_and(|kinds| {
            !kinds
                .iter()
                .any(|kind| kind == crate::recall::contract::UNIT_KIND_TEXT)
        }) {
            return false;
        }
        if self.session_kinds.is_some_and(|kinds| {
            !kinds
                .iter()
                .any(|kind| kind == crate::recall::contract::SESSION_KIND_INTERACTIVE)
        }) {
            return false;
        }
        true
    }
}

pub(crate) fn load_sessions(conn: &Connection) -> Result<Vec<HermesSession>, rusqlite::Error> {
    let mut stmt = conn.prepare_cached(
        "SELECT id, title, cwd, COALESCE(last_activity_at, started_at) FROM sessions \
         ORDER BY COALESCE(last_activity_at, started_at, 0) DESC, id ASC",
    )?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<f64>>(3)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows
        .into_iter()
        .map(|(id, title, cwd, last_activity_at)| HermesSession {
            id,
            title: title.filter(|value| !value.is_empty()),
            cwd: cwd.filter(|value| !value.is_empty()),
            last_activity_ms: last_activity_at.map(|value| (value * 1000.0).round() as i64),
        })
        .collect())
}

fn reference_for(
    source: &SourceConfig,
    session_id: &str,
    message: &HermesMessage,
    projection: &str,
) -> HitRef {
    HitRef {
        source_id: source.source_id.clone(),
        source_kind: crate::recall::contract::SOURCE_KIND_HERMES.to_string(),
        root_sha256: root_sha256(&source.root),
        rel_path: source.db.clone().unwrap_or_default().replace('\\', "/"),
        schema_gen: SCHEMA_HERMES_SQLITE_V1.to_string(),
        locator: Locator::codex(
            session_id,
            Some(&message.id.to_string()),
            message.id as u64,
            "/content",
        ),
        fingerprint: Fingerprint {
            range_sha256: sha256_hex(projection.as_bytes()),
            file_sha256: None,
            row_sha256: Some(sha256_hex(
                format!("{}:{}", message.id, message.content).as_bytes(),
            )),
        },
        text_range: TextRange::new(
            0,
            projection.len() as u64,
            0,
            projection.lines().count() as u64,
        ),
    }
}

/// One searchable hit: the message content is the whole projection.
fn hit_for(
    source: &SourceConfig,
    session: &HermesSession,
    message: &HermesMessage,
    terms: &[String],
    snippet_bytes: usize,
) -> Option<SourceHit> {
    let folded = fold_ascii(&message.content);
    let mut distinct = 0usize;
    let mut first: Option<(usize, usize)> = None;
    for term in terms {
        let needle = fold_ascii(term);
        if let Some(offset) = folded.find(needle.as_str()) {
            distinct += 1;
            first = Some(match first {
                Some((current, current_len)) if current <= offset => (current, current_len),
                _ => (offset, needle.len()),
            });
        }
    }
    if distinct == 0 {
        return None;
    }
    let (first_byte, first_len) = first.unwrap_or((0, 1));
    let (display, truncated) = snippet(&message.content, first_byte, first_len, snippet_bytes);
    Some(SourceHit {
        hit: reference_for(source, &session.id, message, &message.content),
        title: session.title.clone().unwrap_or_else(|| session.id.clone()),
        snippet: display,
        snippet_truncated: truncated,
        score: None,
        score_kind: SCORE_KIND_NONE,
        rank_basis: RANK_BASIS_LEXICAL_WINDOW,
        distinct_terms: distinct,
        first_byte,
        sort_time: message.timestamp_ms,
        sort_id: format!("{}:{}", session.id, message.id),
        related: Vec::new(),
        exact: None,
    })
}

/// Term scan over every session (or one filtered session). The budget caps
/// bytes and wall clock exactly like the other scan providers.
pub fn search_at(
    source: &SourceConfig,
    terms: &[String],
    budget: &Budget,
    snippet_bytes: usize,
    filters: ScanFilters<'_>,
    read_root: &Path,
    observe_root: bool,
) -> SourceOutcome {
    let _started = Instant::now();
    let window = if observe_root {
        Some(Window::open(read_root))
    } else {
        None
    };
    let result = search_inner(source, terms, budget, snippet_bytes, filters, read_root);
    let mut outcome = match result {
        Ok(outcome) => outcome,
        Err(error) => return map_db_error(source, error),
    };
    if let Some(window) = window {
        let (side_effects, violations) = window.close();
        outcome.side_effects = side_effects;
        if !violations.is_empty() {
            outcome.violations = violations;
            outcome.status = status::ERROR;
            outcome.error_message =
                Some("observed root changed outside the C-level runtime artifacts".to_string());
        }
    }
    outcome
}

fn search_inner(
    source: &SourceConfig,
    terms: &[String],
    budget: &Budget,
    snippet_bytes: usize,
    filters: ScanFilters<'_>,
    read_root: &Path,
) -> Result<SourceOutcome, rusqlite::Error> {
    let conn = open_db(source, read_root)?;
    let started = Instant::now();
    let sessions = load_sessions(&conn)?;
    let mut hits: Vec<SourceHit> = Vec::new();
    let mut scanned_sessions = 0u64;
    let mut scanned_messages = 0u64;
    let mut consumed_bytes = 0u64;
    let mut stop: Option<&'static str> = None;
    'sessions: for session in &sessions {
        if consumed_bytes >= budget.max_bytes_per_source {
            stop = Some(codes::BUDGET_BYTES);
            break;
        }
        if started.elapsed().as_millis() as u64 >= budget.max_elapsed_ms_per_source {
            stop = Some(codes::BUDGET_TIME);
            break;
        }
        if filters
            .session_id
            .is_some_and(|session_id| session.id != session_id)
        {
            continue;
        }
        let messages = load_messages(&conn, &session.id)?;
        for message in &messages {
            scanned_messages += 1;
            consumed_bytes += (message.content.len() + 64) as u64;
            if consumed_bytes >= budget.max_bytes_per_source {
                stop = Some(codes::BUDGET_BYTES);
                break 'sessions;
            }
            if started.elapsed().as_millis() as u64 >= budget.max_elapsed_ms_per_source {
                stop = Some(codes::BUDGET_TIME);
                break 'sessions;
            }
            if filters.allows(message) {
                if let Some(hit) = hit_for(source, session, message, terms, snippet_bytes) {
                    hits.push(hit);
                }
            }
        }
        scanned_sessions += 1;
    }
    rank_hits(&mut hits);
    let mut detail = Map::new();
    detail.insert("sessions".into(), Value::from(scanned_sessions));
    detail.insert("messages".into(), Value::from(scanned_messages));
    if let Some(reason) = stop {
        detail.insert("budget".into(), json!({"reason": reason}));
    }
    Ok(SourceOutcome {
        source_id: source.source_id.clone(),
        status: if stop.is_some() {
            status::TRUNCATED
        } else if hits.is_empty() {
            status::EMPTY
        } else {
            status::OK
        },
        code: stop,
        detail: Some(Value::Object(detail)),
        hits,
        warnings: Vec::new(),
        error_message: None,
        side_effects: Vec::new(),
        violations: Vec::new(),
    })
}
