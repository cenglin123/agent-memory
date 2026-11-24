//! Live-source full-text index (L2) and exact retrieval (L3).
//!
//! The index is a **deletable local cache**: one SQLite file per source at
//! `<cache_root>/live/<source_id>/fts.sqlite`. It is derived only from that
//! source's own data, carries no evidence identity (`hit_id` and `ref` still
//! point at the original source), and is rebuilt from scratch when deleted.
//!
//! Index root is never discovered. `cache_root` is an explicit configuration
//! key; `agent-memory index` refuses to run without it, and
//! `memory_search`/`memory_read` only ever open an existing file read-only.
//!
//! Layout (`schema_version = 3`):
//!
//! ```text
//! meta(key, value)                     schema/source identity + watermark
//! doc_keys(doc_key, rowid, …, ts)      incremental delete index + narrow `ts`
//! sessions(session_id, title, kind)    one row per source session
//! archive_sessions(session_id)         sessions currently backed by archive rows
//! units(...)                           FTS5 trigram table, one row per unit
//! conversation_units(rowid, doc_key, …, title) the user/assistant text subset
//! ```
//!
//! `units` is an FTS5 table: every row is one record, so reading any column of a
//! row reads its text as well. The narrow tables therefore carry the few columns
//! a *refresh* needs — `doc_keys.ts` (with an index) for the newest unit time,
//! `conversation_units.title` for the vector metadata — so a steady-state round
//! never scans the FTS5 body (see `migrate_narrow_columns`).
//!
//! One *unit* is one searchable projection: a user/assistant text block, one
//! tool input (`name` + arguments) or one tool output. `text`, `tool_input`
//! and `tool_output` are the three indexed channels; every row fills exactly
//! one of them and names it in `kind`.

pub mod build;
pub mod coverage;
pub mod digest;
pub mod fallback;
pub mod hybrid;
pub mod query;
pub(crate) mod row;
pub mod stamp;
pub mod vectors;
pub use crate::recall::contract::{ORIGIN_ARCHIVE, ORIGIN_LIVE};

use crate::recall::config::{RecallConfig, SourceConfig, SourceKind};
use crate::recall::contract::{status, RecallError};
use crate::recall::hash::{root_sha256, sha256_hex};
use crate::recall::refs::Locator;
use rusqlite::{params, Connection, OpenFlags};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Version of the index layout. A different value is a stale index, never a
/// silent re-interpretation. Version 2 added the `conversation_units` ownership
/// table that the hybrid semantic branch scans. Version 3 (stage-2 S5) adds
/// the `origin` column (`live` / `archive`) to `units` and `doc_keys` plus the
/// `archive_sessions` registry, so an index can hold archive-fallback units
/// that are replaced wholesale when the session reappears in the live source.
pub const INDEX_SCHEMA_VERSION: u32 = 3;

/// Version of the **archive fallback rules** that add evidence-backed units for
/// sessions missing from the live source. A different value means archive rows
/// in an existing index were built by other rules: the index is rebuilt.
pub const FALLBACK_RULE_VERSION: &str = "fallback-1";

/// Version of the **projection rules** that turn source records into index
/// units, independent of the storage layout. Bump it whenever a projection fix
/// changes which text an existing index would contain, so an index built by the
/// old rules is rebuilt instead of staying silently incomplete.
///
/// The version is per **source kind**: a fix that only changes one source's
/// projection must not invalidate the others (re-indexing a 260 k-row OpenCode
/// source to fix a Claude Code channel is pure cost). [`projector_version`] is
/// the single lookup; `PROJECTOR_VERSION` is the generation shared by every
/// source whose rules still match `projector-2`.
///
/// `projector-2` covers the OpenCode V2 tool-part projection (`name` +
/// `state.content`, commit 543dbcf) and the contextual conversation embedding
/// input (title + preceding user turn, `ctx-v2`).
pub const PROJECTOR_VERSION: &str = "projector-2";

/// Claude Code: mid-turn prompts (`queued_command` attachments) are user text.
/// A Claude Code index built by `projector-2` is missing those units.
pub const CLAUDE_PROJECTOR_VERSION: &str = "projector-3-claude-qcmd";

/// OpenCode V2: old live indexes did not include assistant side-table text.
pub const OPENCODE_PROJECTOR_VERSION: &str = "projector-3-opencode-session-part";

/// Device notes: the indexed projection now carries the frontmatter summary as
/// the row title and indexes tags. A device index built by `projector-2` has
/// the filename title and no tag channel, so it is rebuilt on the next round.
pub const DEVICE_PROJECTOR_VERSION: &str = "projector-3-device-frontmatter";

/// Embedding-input generation stored in the vector-cache header.
///
/// It is deliberately **not** the per-source projection version: the cache is
/// validated row by row against the embedding-input hash (title + preceding
/// user turn + body), so a projection fix that only adds units reuses every
/// unchanged row instead of forcing a whole-source re-embed. Only a change to
/// the embedding **input** rules bumps this value. The stored string stays
/// `projector-2` so caches written before the split remain valid.
pub const EMBEDDING_PROJECTOR_VERSION: &str = "projector-2";

/// Projection-rule generation of one source kind. An index whose stored
/// `projector_version` differs is stale for that source only.
pub fn projector_version(kind: SourceKind) -> &'static str {
    match kind {
        SourceKind::ClaudeCode => CLAUDE_PROJECTOR_VERSION,
        SourceKind::Opencode => OPENCODE_PROJECTOR_VERSION,
        SourceKind::Device => DEVICE_PROJECTOR_VERSION,
        _ => PROJECTOR_VERSION,
    }
}

/// `<cache_root>/live/`
pub const LIVE_DIR: &str = "live";
pub const INDEX_FILENAME: &str = "fts.sqlite";
/// Conversation vector cache beside the index: `<…>/vectors.bin`.
pub const VECTORS_FILENAME: &str = "vectors.bin";

/// Tool output is indexed up to this many characters; the rest is dropped and
/// the row is flagged `truncated`.
pub const TOOL_OUTPUT_MAX_CHARS: usize = 20_000;

/// Trigram tokenizer minimum. Terms shorter than this cannot use `MATCH`.
pub const TRIGRAM_MIN_CHARS: usize = 3;

/// Candidate window for one source query. Short-term verification and
/// `group_by_session` run on this window; it bounds query latency instead of
/// scanning an unbounded match set.
pub const CANDIDATE_WINDOW: usize = 4_000;
pub const CANDIDATE_WINDOW_MAX: usize = 20_000;

// ---------------------------------------------------------------------------
// session_kind rules (named so they can be adjusted in one place)
// ---------------------------------------------------------------------------

/// OpenCode: a session title starting with this prefix is an OCSR worker.
pub const OPENCODE_WORKER_TITLE_PREFIX: &str = "ocsr-";
/// OpenCode: a title containing this marker is a dispatched subagent session.
pub const OPENCODE_WORKER_TITLE_MARKER: &str = "subagent)";
/// Claude Code: transcripts under this directory are subagent transcripts.
pub const CLAUDE_WORKER_DIR: &str = "subagents";
/// Codex: `session_meta.payload.source` values that mark a worker.
pub const CODEX_WORKER_SOURCE_MARKERS: [&str; 2] = ["exec", "subagent"];
/// Codex: `session_meta.payload.source` values that mark an interactive run.
pub const CODEX_INTERACTIVE_SOURCE_MARKERS: [&str; 3] = ["cli", "vscode", "app"];
/// Claude Code: the record flag that marks a sidechain (subagent) record.
pub const CLAUDE_SIDECHAIN_FIELD: &str = "isSidechain";
/// Claude Code: the transcript record field that names the client entry point.
/// Interactive CLI sessions write `cli`; SDK / print-mode runs (`claude -p`,
/// SDK hosts) write an `sdk*` value or `print`/`headless`.
pub const CLAUDE_ENTRYPOINT_FIELD: &str = "entrypoint";
/// Entry-point values that reliably mean "no human at the keyboard".
pub const CLAUDE_HEADLESS_ENTRYPOINTS: [&str; 5] = ["sdk", "print", "headless", "exec", "mcp"];
/// Entry-point values that reliably mean an interactive CLI session.
pub const CLAUDE_INTERACTIVE_ENTRYPOINTS: [&str; 2] = ["cli", "interactive"];

/// DSH: a session directory whose name is a bare UUID belongs to a subagent.
/// Top-level sessions use the `session-…` prefix (minted ids included).
pub const DSH_WORKER_DIR_PREFIX: &str = "session-";

/// OpenCode `session_kind`: a non-empty `parent_id`, an `ocsr-` title or a
/// `subagent)` title marker is a worker; anything else is interactive.
pub fn opencode_session_kind(parent_id: Option<&str>, title: Option<&str>) -> &'static str {
    if parent_id.map(|value| !value.is_empty()).unwrap_or(false) {
        return crate::recall::contract::SESSION_KIND_WORKER;
    }
    if let Some(title) = title {
        if title.starts_with(OPENCODE_WORKER_TITLE_PREFIX)
            || title.contains(OPENCODE_WORKER_TITLE_MARKER)
        {
            return crate::recall::contract::SESSION_KIND_WORKER;
        }
    }
    crate::recall::contract::SESSION_KIND_INTERACTIVE
}

/// Claude Code `session_kind`: a transcript under `<session>/subagents/` or a
/// record with `isSidechain: true` is a worker.
pub fn claude_session_kind(rel_path: &str, is_sidechain: bool) -> &'static str {
    if is_sidechain || rel_path_has_component(rel_path, CLAUDE_WORKER_DIR) {
        return crate::recall::contract::SESSION_KIND_WORKER;
    }
    crate::recall::contract::SESSION_KIND_INTERACTIVE
}

/// Entry-point classification of one Claude Code transcript, or `None` when
/// the marker is absent or not one of the named values (then the caller keeps
/// the pre-existing rule unchanged instead of guessing).
///
/// Only `memory_recent` consults this: the live index keeps the original
/// sidechain/path rule, so an index built before this marker existed still
/// answers exactly as before.
pub fn claude_entrypoint_kind(entrypoint: Option<&str>) -> Option<&'static str> {
    let value = entrypoint?.trim().to_ascii_lowercase();
    if value.is_empty() {
        return None;
    }
    if CLAUDE_INTERACTIVE_ENTRYPOINTS.contains(&value.as_str()) {
        return Some(crate::recall::contract::SESSION_KIND_INTERACTIVE);
    }
    let headless = CLAUDE_HEADLESS_ENTRYPOINTS
        .iter()
        .any(|marker| value == *marker || value.starts_with(&format!("{marker}-")));
    if headless {
        return Some(crate::recall::contract::SESSION_KIND_WORKER);
    }
    None
}

/// Claude Code `session_kind` with the optional headless entry-point marker:
/// a sidechain/path worker stays a worker; an interactive transcript whose
/// records carry a reliable headless entry point is a worker too.
pub fn claude_session_kind_with_entrypoint(
    rel_path: &str,
    is_sidechain: bool,
    entrypoint: Option<&str>,
) -> &'static str {
    let base = claude_session_kind(rel_path, is_sidechain);
    if base == crate::recall::contract::SESSION_KIND_WORKER {
        return base;
    }
    claude_entrypoint_kind(entrypoint).unwrap_or(base)
}

fn rel_path_has_component(rel_path: &str, component: &str) -> bool {
    rel_path
        .split(['/', '\\'])
        .any(|part| part.eq_ignore_ascii_case(component))
}

/// DSH `session_kind`: a header `origin = "subagent"`, a `parentSession`, or a
/// bare-UUID session directory is a worker; otherwise interactive.
pub fn dsh_session_kind(
    origin: Option<&str>,
    parent_session: Option<&str>,
    dir_name: &str,
) -> &'static str {
    if origin == Some("subagent")
        || parent_session
            .map(|value| !value.is_empty())
            .unwrap_or(false)
        || is_bare_uuid(dir_name)
    {
        return crate::recall::contract::SESSION_KIND_WORKER;
    }
    crate::recall::contract::SESSION_KIND_INTERACTIVE
}

/// A canonical 8-4-4-4-12 hexadecimal UUID (the DSH subagent directory name).
pub fn is_bare_uuid(value: &str) -> bool {
    let groups: Vec<&str> = value.split('-').collect();
    if groups.len() != 5 {
        return false;
    }
    let expected = [8usize, 4, 4, 4, 12];
    groups.iter().zip(expected).all(|(group, length)| {
        group.len() == length && group.bytes().all(|byte| byte.is_ascii_hexdigit())
    })
}

/// Codex `session_kind`: derived only from `session_meta.payload.source`.
pub fn codex_session_kind(source: Option<&str>) -> &'static str {
    let Some(source) = source else {
        return crate::recall::contract::SESSION_KIND_UNKNOWN;
    };
    let lowered = source.to_ascii_lowercase();
    if CODEX_WORKER_SOURCE_MARKERS
        .iter()
        .any(|marker| lowered.contains(marker))
    {
        return crate::recall::contract::SESSION_KIND_WORKER;
    }
    if CODEX_INTERACTIVE_SOURCE_MARKERS
        .iter()
        .any(|marker| lowered.contains(marker))
    {
        return crate::recall::contract::SESSION_KIND_INTERACTIVE;
    }
    crate::recall::contract::SESSION_KIND_UNKNOWN
}

// ---------------------------------------------------------------------------
// One index unit
// ---------------------------------------------------------------------------

/// One searchable projection of one source record.
#[derive(Debug, Clone)]
pub struct Unit {
    pub session_id: String,
    pub session_kind: &'static str,
    pub title: String,
    pub rel_path: String,
    pub locator: Locator,
    /// Milliseconds since the Unix epoch; `0` when the source has no time.
    pub timestamp_ms: i64,
    pub role: &'static str,
    pub kind: &'static str,
    /// Indexed text (the channel body).
    pub text: String,
    /// Length of the readable projection the ref points at. `text_range` is
    /// always `0..range_len` inside that projection.
    pub range_len: u64,
    /// `line_end` of `TextRange::from_span(projection, 0, range_len)`, stored so
    /// a truncated tool output still yields a ref that readback accepts.
    pub range_lines: u64,
    pub range_sha256: String,
    pub row_sha256: Option<String>,
    pub truncated: bool,
    /// Stage-2 S5: `live` (default) or `archive` (built from committed
    /// evidence because the session is gone from the live source).
    pub origin: &'static str,
}

impl Unit {
    /// Content digest of one unit. An incremental update re-reads the rows at
    /// the watermark edge; an unchanged digest lets it skip the delete+insert,
    /// so a repeated build is genuinely a no-op.
    pub fn doc_digest(&self, source_id: &str) -> String {
        let canonical = [
            source_id.to_string(),
            self.rel_path.clone(),
            self.locator.compact_json(),
            self.kind.to_string(),
            self.role.to_string(),
            self.timestamp_ms.to_string(),
            self.session_kind.to_string(),
            self.title.clone(),
            self.range_len.to_string(),
            self.range_lines.to_string(),
            self.range_sha256.clone(),
            self.row_sha256.clone().unwrap_or_default(),
            if self.truncated {
                "1".into()
            } else {
                "0".into()
            },
            self.text.clone(),
        ]
        .join("\u{1f}");
        sha256_hex(canonical.as_bytes())
    }

    /// The message/row id of the locator, whichever field the generation uses
    /// (`session_message_id` for `sqlite-v2`, `message_id` for JSONL).
    pub fn locator_message_id(&self) -> Option<&str> {
        self.locator
            .session_message_id
            .as_deref()
            .or(self.locator.message_id.as_deref())
    }

    /// Stable identity of one index unit. Two builds of the same source produce
    /// the same key, so an update replaces instead of duplicating.
    pub fn doc_key(&self, source_id: &str) -> String {
        format!(
            "{}\u{1f}{}\u{1f}{}\u{1f}{}",
            source_id,
            self.rel_path,
            self.locator.compact_json(),
            self.kind
        )
    }
}

// ---------------------------------------------------------------------------
// Index location
// ---------------------------------------------------------------------------

/// The configured cache root, or an explicit error. Never a home default.
pub fn configured_cache_root(config: &RecallConfig) -> Result<PathBuf, RecallError> {
    match config.cache_root.as_deref() {
        Some(value) if !value.trim().is_empty() => Ok(PathBuf::from(value)),
        _ => Err(RecallError::invalid(
            "cache_root",
            "missing_key",
            "cache_root is required to build or query live-source full-text indexes; \
             add an explicit drive-absolute cache_root to the configuration",
        )),
    }
}

/// `<cache_root>/live/<source_id>/fts.sqlite`
pub fn index_path(cache_root: &Path, source_id: &str) -> PathBuf {
    source_dir(cache_root, source_id).join(INDEX_FILENAME)
}

/// `<cache_root>/live/<source_id>/vectors.bin`
pub fn vectors_path(cache_root: &Path, source_id: &str) -> PathBuf {
    source_dir(cache_root, source_id).join(VECTORS_FILENAME)
}

/// `<cache_root>/live/<source_id>/`
pub fn source_dir(cache_root: &Path, source_id: &str) -> PathBuf {
    cache_root
        .join(LIVE_DIR)
        .join(sanitize_source_id(source_id))
}

/// Keep the configured `source_id` as the directory name when it is already a
/// safe single path component; otherwise replace unsafe characters and append a
/// short hash so two ids can never collide.
pub fn sanitize_source_id(source_id: &str) -> String {
    let safe = !source_id.is_empty()
        && source_id.len() <= 64
        && source_id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-'))
        && source_id != "."
        && source_id != "..";
    if safe {
        return source_id.to_string();
    }
    let cleaned: String = source_id
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
                ch
            } else {
                '_'
            }
        })
        .take(40)
        .collect();
    format!("{}-{}", cleaned, &sha256_hex(source_id.as_bytes())[..8])
}

// ---------------------------------------------------------------------------
// Meta
// ---------------------------------------------------------------------------

/// Live index identity of the archive fallback, stored in `meta` when the
/// configuration has an archive source. `rule_version` is a rebuild trigger;
/// the other fields document the state the archive rows were built from and
/// feed `detail.coverage`.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct FallbackMeta {
    pub rule_version: String,
    /// Manifest-set fingerprint the archived-session inventory was computed
    /// from. A change is applied incrementally (new sessions added, replaced
    /// sessions dropped), never by a rebuild.
    pub inventory_fingerprint: String,
    /// Archive runtime root the fallback evidence came from.
    pub archive_root: String,
    /// Sessions currently backed by archive rows only.
    pub archive_only_sessions: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct IndexMeta {
    pub schema_version: u32,
    pub source_id: String,
    pub source_kind: String,
    pub source_root: String,
    pub root_sha256: String,
    pub rows: u64,
    pub built_at_ms: i64,
    /// Projection-rule generation the rows were built with. A different value
    /// means the stored units are not what the current projector would produce.
    pub projector_version: String,
    /// Newest unit timestamp in the index (`0` when no unit carries a time).
    pub newest_unit_ts: i64,
    /// Rows skipped because one record exceeded the per-record ceiling. Kept in
    /// the index (not only in the build report) so a query can disclose that the
    /// index is not a superset of the lexical scan.
    pub skipped_oversize: u64,
    /// Source-specific cursor (`sqlite-v2`, `jsonl` or `files`).
    pub watermark: Value,
    /// Stage-2 S5: archive-fallback identity. `None` when the source was
    /// indexed without an archive source (every pre-S5 index answers exactly
    /// as before).
    pub fallback: Option<FallbackMeta>,
}

impl IndexMeta {
    pub fn new(source: &SourceConfig, watermark: Value) -> IndexMeta {
        IndexMeta {
            schema_version: INDEX_SCHEMA_VERSION,
            source_id: source.source_id.clone(),
            source_kind: source.kind.as_str().to_string(),
            source_root: source.root.clone(),
            root_sha256: root_sha256(&source.root),
            rows: 0,
            built_at_ms: now_ms(),
            projector_version: projector_version(source.kind).to_string(),
            newest_unit_ts: 0,
            skipped_oversize: 0,
            watermark,
            fallback: None,
        }
    }
}

pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// Milliseconds since the epoch as an RFC3339 UTC string; `None` for "unknown".
/// The same rendering `since`/`until` accept, so a caller can feed it back.
pub fn rfc3339(ms: i64) -> Option<String> {
    if ms <= 0 {
        return None;
    }
    chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ms)
        .map(|stamp| stamp.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
}

fn meta_key_rows(conn: &Connection) -> rusqlite::Result<Vec<(String, String)>> {
    let mut statement = conn.prepare("SELECT key, value FROM meta")?;
    let rows = statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    rows.collect()
}

impl IndexMeta {
    pub fn load(conn: &Connection) -> rusqlite::Result<Option<IndexMeta>> {
        let entries = meta_key_rows(conn)?;
        let get = |key: &str| {
            entries
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value.clone())
        };
        let Some(version) = get("schema_version") else {
            return Ok(None);
        };
        let Ok(schema_version) = version.parse::<u32>() else {
            return Ok(None);
        };
        let watermark = get("watermark")
            .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
            .unwrap_or(Value::Null);
        Ok(Some(IndexMeta {
            schema_version,
            source_id: get("source_id").unwrap_or_default(),
            source_kind: get("source_kind").unwrap_or_default(),
            source_root: get("source_root").unwrap_or_default(),
            root_sha256: get("root_sha256").unwrap_or_default(),
            rows: get("rows")
                .and_then(|raw| raw.parse::<u64>().ok())
                .unwrap_or(0),
            built_at_ms: get("built_at_ms")
                .and_then(|raw| raw.parse::<i64>().ok())
                .unwrap_or(0),
            projector_version: get("projector_version").unwrap_or_default(),
            newest_unit_ts: get("newest_unit_ts")
                .and_then(|raw| raw.parse::<i64>().ok())
                .unwrap_or(0),
            skipped_oversize: get("skipped_oversize")
                .and_then(|raw| raw.parse::<u64>().ok())
                .unwrap_or(0),
            watermark,
            fallback: get("fallback").and_then(|raw| serde_json::from_str(&raw).ok()),
        }))
    }

    pub fn store(&self, conn: &Connection) -> rusqlite::Result<()> {
        conn.execute("DELETE FROM meta", [])?;
        let mut entries = vec![
            ("schema_version", self.schema_version.to_string()),
            ("source_id", self.source_id.clone()),
            ("source_kind", self.source_kind.clone()),
            ("source_root", self.source_root.clone()),
            ("root_sha256", self.root_sha256.clone()),
            ("rows", self.rows.to_string()),
            ("built_at_ms", self.built_at_ms.to_string()),
            ("projector_version", self.projector_version.clone()),
            ("newest_unit_ts", self.newest_unit_ts.to_string()),
            ("skipped_oversize", self.skipped_oversize.to_string()),
            (
                "watermark",
                serde_json::to_string(&self.watermark).unwrap_or_else(|_| "null".into()),
            ),
        ];
        if let Some(fallback) = &self.fallback {
            entries.push((
                "fallback",
                serde_json::to_string(fallback).unwrap_or_else(|_| "null".into()),
            ));
        }
        let mut statement = conn.prepare("INSERT INTO meta(key, value) VALUES (?1, ?2)")?;
        for (key, value) in entries {
            statement.execute(rusqlite::params![key, value])?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Schema
// ---------------------------------------------------------------------------

const SCHEMA_SQL: &str = "
CREATE TABLE IF NOT EXISTS meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS doc_keys (
    doc_key TEXT PRIMARY KEY,
    rowid INTEGER NOT NULL,
    session_id TEXT,
    message_id TEXT,
    rel_path TEXT,
    digest TEXT,
    origin TEXT NOT NULL DEFAULT 'live',
    ts INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS doc_keys_message ON doc_keys(session_id, message_id);
CREATE INDEX IF NOT EXISTS doc_keys_rel_path ON doc_keys(rel_path);
CREATE TABLE IF NOT EXISTS sessions (
    session_id TEXT PRIMARY KEY,
    title TEXT,
    session_kind TEXT
);
CREATE TABLE IF NOT EXISTS archive_sessions (
    session_id TEXT PRIMARY KEY,
    rule_version TEXT,
    projected_units INTEGER,
    carrier_fingerprint TEXT
);
CREATE TABLE IF NOT EXISTS conversation_units (
    rowid INTEGER PRIMARY KEY,
    doc_key TEXT NOT NULL,
    text_sha256 TEXT NOT NULL,
    session_id TEXT NOT NULL,
    message_id TEXT,
    ts INTEGER NOT NULL,
    role TEXT NOT NULL,
    session_kind TEXT NOT NULL,
    title TEXT NOT NULL DEFAULT ''
);
CREATE INDEX IF NOT EXISTS conversation_units_session
    ON conversation_units(session_id, ts);
CREATE INDEX IF NOT EXISTS conversation_units_doc_key
    ON conversation_units(doc_key);
CREATE VIRTUAL TABLE IF NOT EXISTS units USING fts5(
    text,
    tool_input,
    tool_output,
    source_id UNINDEXED,
    session_id UNINDEXED,
    message_id UNINDEXED,
    record_index UNINDEXED,
    pointer UNINDEXED,
    locator UNINDEXED,
    rel_path UNINDEXED,
    ts UNINDEXED,
    role UNINDEXED,
    kind UNINDEXED,
    session_kind UNINDEXED,
    title UNINDEXED,
    range_len UNINDEXED,
    range_lines UNINDEXED,
    range_sha256 UNINDEXED,
    row_sha256 UNINDEXED,
    origin UNINDEXED,
    tokenize = 'trigram'
);
";

pub fn create_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(SCHEMA_SQL)?;
    migrate_archive_sessions(conn)?;
    migrate_narrow_columns(conn)?;
    ensure_narrow_indexes(conn)?;
    repair_narrow_columns(conn)
}

/// `meta` key holding the **fingerprint of the narrow tables** (`rows:max_rowid`
/// of `doc_keys`), written at the end of every build and checked at open.
///
/// It exists to notice a writer that does not know the narrow columns. An older
/// binary of this crate starts `IndexMeta::store` with `DELETE FROM meta` and
/// re-inserts a fixed key list, so this row disappears on its next build while
/// the rows it inserted keep the column defaults (`ts = 0`, `title = ''`). Both
/// the missing row and a shape mismatch mean "someone else wrote this index", and
/// [`repair_narrow_columns`] then fixes exactly the affected rows.
pub const NARROW_FINGERPRINT_KEY: &str = "narrow_fingerprint";

/// Layout generation of the **narrow** bookkeeping columns, stored in the
/// database's `user_version` (not in `meta`, which every build rewrites):
/// `0` = an index written before them, `1` = current. The version is written in
/// the same transaction as the backfill, so a crash rolls both back and the next
/// open migrates again instead of trusting half-filled columns.
const NARROW_LAYOUT_VERSION: i64 = 1;

/// Widen the two **narrow** tables (`doc_keys`, `conversation_units`) with the
/// columns a refresh needs without reading the FTS5 body, and backfill them once.
///
/// `units` stores every row as one record: reading `ts` or `title` out of it
/// reads the row's text too, so a per-round `COUNT(*)` / `MAX(ts)` / metadata
/// select over it costs the whole index in I/O. The narrow twins answer the same
/// questions from their own pages (the `doc_keys.ts` index makes the newest-time
/// query O(log n)). They are not a second index of the corpus: they are the
/// columns of the existing bookkeeping tables.
///
/// A file written before this layout is widened and backfilled exactly once, with
/// one sequential scan of `units` — the only FTS5 scan the migration is allowed
/// to do — plus one pass of each narrow table; the read volume is therefore
/// proportional to the file, never to its rows times its size. Every later round
/// maintains the columns through `Writer`.
fn migrate_narrow_columns(conn: &Connection) -> rusqlite::Result<()> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version >= NARROW_LAYOUT_VERSION {
        return Ok(());
    }
    let add_ts = !table_columns(conn, "doc_keys")
        .iter()
        .any(|name| name == "ts");
    let add_title = !table_columns(conn, "conversation_units")
        .iter()
        .any(|name| name == "title");
    let _phase = crate::cost::phase("index_migration");
    let reads_before = pages_read(conn);
    let tx = conn.unchecked_transaction()?;
    if add_ts {
        add_column(&tx, "doc_keys", "ts INTEGER NOT NULL DEFAULT 0")?;
    }
    if add_title {
        add_column(&tx, "conversation_units", "title TEXT NOT NULL DEFAULT ''")?;
    }
    // Both columns, even when only one was missing: the file is being migrated
    // anyway, and a partial layout says nothing about the column it already had.
    // No relabel detection: a column that did not exist a moment ago is all
    // defaults, and a rename needs both columns to be visible.
    let outcome = backfill_narrow_columns(&tx, true, true, false)?;
    crate::cost::counter("index.narrow_backfill_rows", outcome.rows);
    // The file is consistent now, so record it: the repair after this function
    // would otherwise restage the same rows a second time on every pre-004a file.
    record_narrow_fingerprint(&tx)?;
    tx.execute_batch(&format!("PRAGMA user_version = {NARROW_LAYOUT_VERSION}"))?;
    tx.commit()?;
    crate::cost::counter(
        "index.narrow_backfill_pages_read",
        pages_read(conn).saturating_sub(reads_before),
    );
    Ok(())
}

/// The index the narrow columns need. It is created here (not in `SCHEMA_SQL`)
/// because `doc_keys_ts` names a column an older file only gains in
/// [`migrate_narrow_columns`]; this runs after it and is idempotent.
///
/// An index file repaired by an earlier build may still carry the partial
/// `doc_keys_unset` / `conversation_units_unset` indexes the per-row repair used;
/// the full restage needs no candidate lookup, and the leftovers are harmless
/// (they only hold rows that still carry a default value).
fn ensure_narrow_indexes(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch("CREATE INDEX IF NOT EXISTS doc_keys_ts ON doc_keys(ts);")
}

/// `(row count, max rowid)` of the narrow key table.
///
/// This is **one pass over `doc_keys`**, not an index lookup: the table declares
/// a column named `rowid` (`rowid INTEGER NOT NULL`, next to the `doc_key`
/// primary key), so `MAX(rowid)` reads that unindexed column and the plan is
/// `SCAN doc_keys`. The cost is a narrow table (megabytes), checked once per open
/// — it is what makes the detection affordable without touching the FTS5 body.
fn narrow_shape(conn: &Connection) -> rusqlite::Result<(u64, i64)> {
    conn.query_row(
        "SELECT COUNT(*), COALESCE(MAX(rowid), 0) FROM doc_keys",
        [],
        |row| Ok((row.get::<_, i64>(0)?.max(0) as u64, row.get::<_, i64>(1)?)),
    )
}

fn narrow_fingerprint(rows: u64, max_rowid: i64) -> String {
    format!("{rows}:{max_rowid}")
}

fn stored_narrow_fingerprint(conn: &Connection) -> rusqlite::Result<Option<String>> {
    use rusqlite::OptionalExtension;
    conn.query_row(
        "SELECT value FROM meta WHERE key = ?1",
        [NARROW_FINGERPRINT_KEY],
        |row| row.get::<_, String>(0),
    )
    .optional()
}

/// Record the current shape. Called at the end of every build (after `meta` is
/// rewritten) and at the end of a repair.
pub(crate) fn record_narrow_fingerprint(conn: &Connection) -> rusqlite::Result<()> {
    let (rows, max_rowid) = narrow_shape(conn)?;
    conn.execute(
        "INSERT OR REPLACE INTO meta(key, value) VALUES (?1, ?2)",
        params![NARROW_FINGERPRINT_KEY, narrow_fingerprint(rows, max_rowid)],
    )?;
    Ok(())
}

/// Repair the narrow columns after a writer that did not know about them.
///
/// Detection is one pass over the narrow key table (`COUNT(*)` + `MAX(rowid)` of
/// `doc_keys`, see [`narrow_shape`]): the recorded fingerprint must match. A
/// mismatch — or a missing fingerprint on a non-empty index, which is what an
/// older binary leaves because its `meta` write starts with `DELETE FROM meta` —
/// means another writer touched this index.
///
/// The repair is then a **complete narrow-column backfill**, not a per-row
/// patch: an older writer's damage is not confined to the column defaults it
/// leaves behind. Its `INSERT`s write `ts = 0` / `title = ''`, but its in-place
/// rename (`UPDATE units SET title = … WHERE session_id = ?`) leaves the *old*
/// title in `conversation_units.title` — a non-default value no default-value
/// candidate could ever find. Restaging every row from `units` costs one
/// sequential FTS5 pass (the same shape as [`migrate_narrow_columns`], through
/// [`backfill_narrow_columns`]), which is proportional to the file and
/// independent of how many rows the other writer touched; a per-row patch would
/// have to prove it found every damaged shape, and this one cannot.
///
/// The digest of a relabelled session is content too (`doc_keys.digest` embeds
/// the title), and it can only be recomputed by the projector that has the
/// source. So the sessions whose narrow title disagreed with `units` are
/// recorded under [`NARROW_RELABEL_KEY`]; the builder re-projects them this same
/// round, exactly like a rename it detected itself. The backfill and the
/// fingerprint commit together.
fn repair_narrow_columns(conn: &Connection) -> rusqlite::Result<()> {
    let (rows, max_rowid) = narrow_shape(conn)?;
    let expected = narrow_fingerprint(rows, max_rowid);
    if stored_narrow_fingerprint(conn)?.as_deref() == Some(expected.as_str()) {
        return Ok(());
    }
    let _phase = crate::cost::phase("index_repair");
    let reads_before = pages_read(conn);
    let tx = conn.unchecked_transaction()?;
    let outcome = backfill_narrow_columns(&tx, true, true, true)?;
    if !outcome.relabelled.is_empty() {
        record_narrow_relabel_sessions(&tx, &outcome.relabelled)?;
    }
    record_narrow_fingerprint(&tx)?;
    tx.commit()?;
    crate::cost::counter("index.narrow_repair_rows", outcome.rows);
    crate::cost::counter(
        "index.narrow_relabel_sessions",
        outcome.relabelled.len() as u64,
    );
    crate::cost::counter(
        "index.narrow_repair_pages_read",
        pages_read(conn).saturating_sub(reads_before),
    );
    Ok(())
}

/// `meta` key naming the sessions whose narrow title disagreed with `units` when
/// the repair ran — i.e. sessions an older writer renamed in place.
///
/// The repair cannot recompute `doc_keys.digest` (the projector owns the source),
/// so it hands the ids to the builder, which re-projects them this round. The key
/// is transient by construction: `IndexMeta::store` starts with `DELETE FROM meta`
/// at the end of the build, and the fingerprint is written after it.
pub const NARROW_RELABEL_KEY: &str = "narrow_relabel_sessions";

fn record_narrow_relabel_sessions(conn: &Connection, sessions: &[String]) -> rusqlite::Result<()> {
    let encoded = serde_json::to_string(sessions).unwrap_or_else(|_| "[]".into());
    conn.execute(
        "INSERT OR REPLACE INTO meta(key, value) VALUES (?1, ?2)",
        params![NARROW_RELABEL_KEY, encoded],
    )?;
    Ok(())
}

/// The sessions [`repair_narrow_columns`] recorded for re-projection, if any.
pub(crate) fn narrow_relabel_sessions(conn: &Connection) -> rusqlite::Result<Vec<String>> {
    use rusqlite::OptionalExtension;
    let raw: Option<String> = conn
        .query_row(
            "SELECT value FROM meta WHERE key = ?1",
            [NARROW_RELABEL_KEY],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    Ok(raw
        .and_then(|raw| serde_json::from_str::<Vec<String>>(&raw).ok())
        .unwrap_or_default())
}

/// The `rel_path` carriers of those sessions (the DSH builder addresses a carrier
/// by its rel path, the OpenCode builder by session id).
pub(crate) fn narrow_relabel_paths(conn: &Connection) -> rusqlite::Result<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    let mut statement = conn.prepare(
        "SELECT DISTINCT rel_path FROM doc_keys WHERE session_id = ?1 AND rel_path IS NOT NULL",
    )?;
    for session in narrow_relabel_sessions(conn)? {
        let rows = statement.query_map(params![session], |row| row.get::<_, String>(0))?;
        for rel_path in rows {
            let rel_path = rel_path?;
            if !out.contains(&rel_path) {
                out.push(rel_path);
            }
        }
    }
    Ok(out)
}

fn table_columns(conn: &Connection, table: &str) -> Vec<String> {
    let Ok(mut statement) = conn.prepare(&format!("PRAGMA table_info({table})")) else {
        return Vec::new();
    };
    let Ok(rows) = statement.query_map([], |row| row.get::<_, String>(1)) else {
        return Vec::new();
    };
    rows.flatten().collect()
}

fn add_column(conn: &Connection, table: &str, declaration: &str) -> rusqlite::Result<()> {
    let sql = format!("ALTER TABLE {table} ADD COLUMN {declaration}");
    match conn.execute(&sql, []) {
        Ok(_) => Ok(()),
        // A concurrent first open added it first: the column exists, which is
        // all this call promises.
        Err(error) if is_duplicate_column(&error) => Ok(()),
        Err(error) => Err(error),
    }
}

/// The one-time backfill: one sequential scan of `units` into a TEMP table, then
/// one set-based `UPDATE` per narrow table. Runs inside the migration's
/// transaction (the caller owns it).
///
/// **Read once, then write.** The obvious loop — step a `units` cursor and issue
/// one `UPDATE … WHERE rowid = ?` per row — is quadratic, and not because of
/// FTS5: `doc_keys` declares *a column named* `rowid` (`rowid INTEGER NOT NULL`
/// next to the `doc_key` primary key), so the implicit rowid is shadowed and
/// `WHERE rowid = ?` matches that unindexed column. Every row therefore ran a
/// full table scan of `doc_keys` (`EXPLAIN QUERY PLAN`: `SCAN doc_keys`), and the
/// partial index that could serve it (`doc_keys_unset`, `WHERE ts = 0`) is not
/// usable there because the statement has no `ts` predicate. On a 197 MB index
/// that turned ~20 k rows into ~240 GB of page reads.
///
/// The scan must also survive the writes it performs. Staging the three
/// columns in a TEMP table keeps the FTS5 scan at exactly one pass (the scan is
/// finished before the first narrow table is touched) and turns the writes into
/// two statements whose plans are `SCAN doc_keys` / `SEARCH narrow_fill USING
/// INTEGER PRIMARY KEY` — every narrow row visited once, one indexed lookup into
/// the staged data. Memory stays bounded by the SQLite TEMP store and the page
/// cache, not by the number of rows: nothing is accumulated in a `Vec`.
/// What one narrow-column backfill did.
struct BackfillOutcome {
    /// Rows staged from `units`.
    rows: u64,
    /// Sessions whose `conversation_units.title` disagreed with `units.title`
    /// **before** the fill — the signature of a writer that renamed a session in
    /// place. Only collected when the caller asks for it.
    relabelled: Vec<String>,
}

fn backfill_narrow_columns(
    conn: &Connection,
    fill_ts: bool,
    fill_title: bool,
    detect_relabels: bool,
) -> rusqlite::Result<BackfillOutcome> {
    conn.execute_batch(
        "DROP TABLE IF EXISTS temp.narrow_fill;
         CREATE TEMP TABLE narrow_fill (
             rowid INTEGER PRIMARY KEY,
             ts INTEGER NOT NULL,
             title TEXT NOT NULL
         );
         INSERT INTO narrow_fill(rowid, ts, title)
             SELECT rowid, COALESCE(ts, 0), COALESCE(title, '')
               FROM units;",
    )?;
    let rows: u64 = conn.query_row("SELECT COUNT(*) FROM narrow_fill", [], |row| {
        row.get::<_, i64>(0)
    })? as u64;
    // Before the fill overwrites the narrow title: a non-empty narrow title that
    // differs from the unit's is a rename the other writer applied to `units`
    // only. The join is a `conversation_units` scan with one indexed lookup into
    // the staged rows — no extra FTS5 pass.
    let relabelled = if detect_relabels && fill_title {
        let mut statement = conn.prepare(
            "SELECT DISTINCT narrow.session_id
               FROM conversation_units AS narrow
               JOIN narrow_fill AS fill ON fill.rowid = narrow.rowid
              WHERE narrow.title <> '' AND narrow.title <> fill.title
              ORDER BY narrow.session_id",
        )?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        rows.collect::<rusqlite::Result<Vec<String>>>()?
    } else {
        Vec::new()
    };
    if fill_ts {
        conn.execute_batch(
            "UPDATE doc_keys SET ts = fill.ts
               FROM narrow_fill AS fill
              WHERE fill.rowid = doc_keys.rowid;",
        )?;
    }
    if fill_title {
        conn.execute_batch(
            "UPDATE conversation_units SET title = fill.title
               FROM narrow_fill AS fill
              WHERE fill.rowid = conversation_units.rowid;",
        )?;
    }
    conn.execute_batch("DROP TABLE narrow_fill")?;
    Ok(BackfillOutcome { rows, relabelled })
}

/// Pages this connection has asked its pager for so far (cache hits plus cache
/// misses).
///
/// This is the platform-independent read measure. Process I/O counters miss
/// every read the OS cache serves — on a warm machine a whole synthetic index is
/// served that way, so `read_bytes` reports 0 while the pager still walks the
/// same pages — and the *miss* counter alone misses the defect for the same
/// reason at a smaller scale: a `doc_keys` table that fits in the page cache is
/// re-read from memory on every row, so nothing is loaded but the page traffic
/// still multiplies. Counting hits *and* misses gives "page references", which
/// is what the page cache serves and what the production read volume was made
/// of. Both counters reset when read, so callers diff two samples.
fn pages_read(conn: &Connection) -> u64 {
    fn status(conn: &Connection, op: i32) -> u64 {
        let mut current = 0i32;
        let mut highwater = 0i32;
        // SAFETY: `conn` owns a live `sqlite3*`; both out-pointers are valid,
        // writable `int`s, and `sqlite3_db_status` touches nothing else.
        let rc = unsafe {
            rusqlite::ffi::sqlite3_db_status(conn.handle(), op, &mut current, &mut highwater, 0)
        };
        if rc == rusqlite::ffi::SQLITE_OK {
            current.max(0) as u64
        } else {
            0
        }
    }
    status(conn, rusqlite::ffi::SQLITE_DBSTATUS_CACHE_HIT)
        .saturating_add(status(conn, rusqlite::ffi::SQLITE_DBSTATUS_CACHE_MISS))
}

/// Add the archive-fallback bookkeeping columns to a registry written by an
/// older layout (rows keep `NULL` there until the session is processed again).
/// `ALTER TABLE ... ADD COLUMN` is the only widening SQLite does in place, and
/// the registry is a derived cache, so an old file is upgraded in place instead
/// of rebuilding the index.
fn migrate_archive_sessions(conn: &Connection) -> rusqlite::Result<()> {
    let mut columns: Vec<String> = Vec::new();
    {
        let mut statement = conn.prepare("PRAGMA table_info(archive_sessions)")?;
        let rows = statement.query_map([], |row| row.get::<_, String>(1))?;
        for name in rows {
            columns.push(name?);
        }
    }
    if !columns.iter().any(|name| name == "rule_version") {
        add_archive_sessions_column(conn, "rule_version TEXT")?;
    }
    if !columns.iter().any(|name| name == "projected_units") {
        add_archive_sessions_column(conn, "projected_units INTEGER")?;
    }
    if !columns.iter().any(|name| name == "carrier_fingerprint") {
        add_archive_sessions_column(conn, "carrier_fingerprint TEXT")?;
    }
    Ok(())
}

/// One `ALTER TABLE archive_sessions ADD COLUMN` of the in-place migration.
///
/// Two `index` runs can open the same pre-migration file at once: both read the
/// column list before either `ALTER TABLE` lands, and the loser gets
/// `duplicate column name`. The column it asked for exists by then, so the
/// error is the concurrent first open, not a failure: treat it as success.
fn add_archive_sessions_column(conn: &Connection, declaration: &str) -> rusqlite::Result<()> {
    add_column(conn, "archive_sessions", declaration)
}

/// Does this SQLite error say the column was already added? Matched on the
/// engine's message: `ALTER TABLE` has no idempotent form.
fn is_duplicate_column(error: &rusqlite::Error) -> bool {
    match error {
        rusqlite::Error::SqliteFailure(_, Some(message)) => message
            .to_ascii_lowercase()
            .contains("duplicate column name"),
        _ => false,
    }
}

/// Drop every table of an index written by an older layout. The index is a
/// deletable cache: a layout change rebuilds instead of re-interpreting, and
/// FTS5 tables cannot gain columns in place.
const KNOWN_TABLES: [&str; 6] = [
    "meta",
    "doc_keys",
    "sessions",
    "archive_sessions",
    "conversation_units",
    "units",
];

fn migrate_layout(conn: &Connection) -> rusqlite::Result<()> {
    let stale = match meta_schema_version(conn) {
        // A file without a `meta` table is fresh (or foreign): create_schema
        // below decides.
        None => false,
        Some(version) => version != INDEX_SCHEMA_VERSION,
    };
    if !stale {
        return Ok(());
    }
    for table in KNOWN_TABLES {
        conn.execute(&format!("DROP TABLE IF EXISTS {table}"), [])?;
    }
    Ok(())
}

fn meta_schema_version(conn: &Connection) -> Option<u32> {
    let exists: i64 = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'meta')",
            [],
            |row| row.get(0),
        )
        .ok()?;
    if exists == 0 {
        return None;
    }
    conn.query_row(
        "SELECT value FROM meta WHERE key = 'schema_version'",
        [],
        |row| row.get::<_, String>(0),
    )
    .ok()?
    .parse::<u32>()
    .ok()
}

/// A writable index handle. The caller owns the transaction. An existing file
/// from an older layout generation is cleared first (the caller rebuilds).
pub fn open_writable(path: &Path) -> rusqlite::Result<Connection> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|err| {
            rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CANTOPEN),
                Some(format!("cannot create {}: {err}", parent.display())),
            )
        })?;
    }
    let conn = Connection::open(path)?;
    conn.busy_timeout(Duration::from_secs(10))?;
    migrate_layout(&conn)?;
    create_schema(&conn)?;
    Ok(conn)
}

// ---------------------------------------------------------------------------
// Query-time opening
// ---------------------------------------------------------------------------

/// Why exact mode could not use the index for a source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexState {
    /// Index exists, matches the configured source identity, is readable.
    Ready,
    /// No index file (or no configured `cache_root`).
    Missing,
    /// Index exists but belongs to another configuration/schema generation.
    Stale(&'static str),
    /// Index exists but cannot be read as this crate's index.
    Corrupt(&'static str),
}

impl IndexState {
    pub fn reason(&self) -> Option<&'static str> {
        match self {
            IndexState::Ready => None,
            IndexState::Missing => Some(crate::recall::contract::INDEX_REASON_MISSING),
            IndexState::Stale(_) => Some(crate::recall::contract::INDEX_REASON_STALE),
            IndexState::Corrupt(_) => Some(crate::recall::contract::INDEX_REASON_CORRUPT),
        }
    }

    pub fn detail(&self) -> Option<&'static str> {
        match self {
            IndexState::Stale(reason) | IndexState::Corrupt(reason) => Some(reason),
            _ => None,
        }
    }
}

/// Open the index of `source` read-only for a query.
pub fn open_for_query(
    config: &RecallConfig,
    source: &SourceConfig,
) -> (Connection, PathBuf, IndexState) {
    let Ok(cache_root) = configured_cache_root(config) else {
        return (
            unreachable_connection(),
            PathBuf::new(),
            IndexState::Missing,
        );
    };
    let path = index_path(&cache_root, &source.source_id);
    if !path.is_file() {
        return (unreachable_connection(), path, IndexState::Missing);
    }
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let conn = match Connection::open_with_flags(&path, flags) {
        Ok(conn) => conn,
        Err(err) => {
            let state = match &err {
                rusqlite::Error::SqliteFailure(inner, _)
                    if matches!(
                        inner.code,
                        rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
                    ) =>
                {
                    IndexState::Corrupt("index_locked")
                }
                _ => IndexState::Corrupt("index_unreadable"),
            };
            return (unreachable_connection(), path, state);
        }
    };
    let _ = conn.busy_timeout(Duration::from_secs(5));
    let meta = match IndexMeta::load(&conn) {
        Ok(Some(meta)) => meta,
        Ok(None) => return (conn, path, IndexState::Corrupt("index_meta_missing")),
        Err(_) => return (conn, path, IndexState::Corrupt("index_meta_unreadable")),
    };
    if meta.schema_version != INDEX_SCHEMA_VERSION {
        return (conn, path, IndexState::Stale("index_schema_version"));
    }
    if meta.source_id != source.source_id || meta.source_root != source.root {
        return (conn, path, IndexState::Stale("source_identity_mismatch"));
    }
    if meta.source_kind != source.kind.as_str() {
        return (conn, path, IndexState::Stale("source_kind_mismatch"));
    }
    if meta.root_sha256 != root_sha256(&source.root) {
        return (conn, path, IndexState::Stale("source_root_mismatch"));
    }
    if meta.projector_version != projector_version(source.kind) {
        // The rows were projected by older rules: an index built by them is not
        // a smaller index, it is a different one (it can be missing whole
        // channels). Rebuild instead of answering from it. Checked last so an
        // identity mismatch keeps its more fundamental cause.
        return (conn, path, IndexState::Stale("projector_version_changed"));
    }
    (conn, path, IndexState::Ready)
}

/// `true` when this source has a readable index right now. `mode = "auto"`
/// uses it to choose between hybrid and the legacy scan; a source without an
/// index keeps exactly its previous behavior.
pub fn index_ready(config: &RecallConfig, source: &SourceConfig) -> bool {
    open_for_query(config, source).2 == IndexState::Ready
}

/// Placeholder connection handed back when the index cannot be opened. It is
/// never queried: callers branch on [`IndexState`] first.
fn unreachable_connection() -> Connection {
    Connection::open_in_memory().expect("in-memory sqlite is always available")
}

/// `line_end` of the whole projection (the ref always spans `0..len`).
pub fn projection_line_end(text: &str) -> u64 {
    crate::recall::hash::TextRange::from_span(text.as_bytes(), 0, text.len()).line_end
}

/// Build a degraded/fallback detail block for exact mode.
pub fn exact_detail(mode: &str, reason: &str) -> Value {
    json!({ "mode": mode, "reason": reason })
}

/// `status` for a source that answered through a scan fallback.
pub fn fallback_status(current: &'static str, short_terms: bool) -> &'static str {
    if short_terms {
        return current;
    }
    match current {
        status::OK | status::EMPTY => status::DEGRADED,
        other => other,
    }
}

// ---------------------------------------------------------------------------
// Query terms
// ---------------------------------------------------------------------------

/// One query term after whitespace splitting; a double-quoted span is a single
/// phrase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Term {
    pub text: String,
    /// True when the caller wrapped the term in double quotes.
    pub phrase: bool,
}

impl Term {
    pub fn chars(&self) -> usize {
        self.text.chars().count()
    }

    pub fn is_long_enough(&self) -> bool {
        self.chars() >= TRIGRAM_MIN_CHARS
    }
}

/// Split the raw query on unquoted whitespace; a `"..."` span is one term.
///
/// An unbalanced quote simply ends at the end of the query — the caller's
/// intent (one phrase) is still the closest reading, and exact mode never
/// rejects a query the scan path would accept.
pub fn parse_terms(raw: &str) -> Vec<Term> {
    let mut terms: Vec<Term> = Vec::new();
    let mut current = String::new();
    let mut phrase = false;
    let mut quoted = false;
    for ch in raw.chars() {
        if ch == '"' {
            if quoted {
                quoted = false;
                if !current.trim().is_empty() {
                    terms.push(Term {
                        text: current.trim().to_string(),
                        phrase: true,
                    });
                }
                current.clear();
            } else {
                if !current.trim().is_empty() {
                    terms.push(Term {
                        text: current.trim().to_string(),
                        phrase,
                    });
                }
                current.clear();
                quoted = true;
                phrase = true;
            }
            continue;
        }
        if ch.is_whitespace() && !quoted {
            if !current.trim().is_empty() {
                terms.push(Term {
                    text: current.trim().to_string(),
                    phrase,
                });
            }
            current.clear();
            phrase = false;
            continue;
        }
        current.push(ch);
    }
    if !current.trim().is_empty() {
        terms.push(Term {
            text: current.trim().to_string(),
            phrase: quoted || phrase,
        });
    }
    terms
}

/// Escape one phrase for an FTS5 string literal (`"` doubles inside).
pub fn fts_phrase(text: &str) -> String {
    let escaped = text.replace('"', "\"\"");
    format!("\"{escaped}\"")
}

/// Build the `MATCH` expression: every long term is one AND-ed phrase.
pub fn match_expression(terms: &[Term]) -> Option<String> {
    let long: Vec<String> = terms
        .iter()
        .filter(|term| term.is_long_enough())
        .map(|term| fts_phrase(&term.text))
        .collect();
    if long.is_empty() {
        return None;
    }
    Some(long.join(" AND "))
}

// ---------------------------------------------------------------------------
// Ordering
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Order {
    Newest,
    Oldest,
    Relevance,
}

impl Order {
    pub fn parse(value: &str) -> Option<Order> {
        match value {
            crate::recall::contract::ORDER_NEWEST => Some(Order::Newest),
            crate::recall::contract::ORDER_OLDEST => Some(Order::Oldest),
            crate::recall::contract::ORDER_RELEVANCE => Some(Order::Relevance),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Order::Newest => crate::recall::contract::ORDER_NEWEST,
            Order::Oldest => crate::recall::contract::ORDER_OLDEST,
            Order::Relevance => crate::recall::contract::ORDER_RELEVANCE,
        }
    }
}

/// Source-specific watermark shapes.
///
/// # JSONL / DSH carrier watermarks: content probes and file identity
///
/// A carrier entry is `{size, mtime_ns, offset|lines, head_probe, anchor_probe,
/// file_identity}`. `head_probe` / `anchor_probe` are sha256 digests of the
/// first and of the window ending at the committed offset (`stamp::PROBE_WINDOW`
/// bytes each, see [`super::stamp`]); `file_identity` is the Windows volume
/// serial number + file id (`GetFileInformationByHandle`), empty off Windows or
/// when the call fails.
///
/// Every round classifies one carrier with the cheap signals first:
///
/// | size | mtime | identity | probes | plan | counter |
/// |---|---|---|---|---|---|
/// | unchanged | unchanged | same / unknown | not read | skip | — |
/// | unchanged | unchanged | **different** | not read | whole-file rescan | `index.full_rescan.file_identity_changed` |
/// | unchanged | changed | same / unknown | both match, `offset == size` | record the new mtime, read nothing | `index.mtime_only_refresh` |
/// | unchanged | changed | same / unknown | both match, `offset < size` | read `[offset, size)` — the uncommitted tail | `index.tail_completion` |
/// | unchanged | changed | same / unknown | differ | whole-file rescan | `index.full_rescan.probe_mismatch` |
/// | unchanged | changed | same / unknown | absent (pre-004a entry) | whole-file rescan | `index.full_rescan.same_size_mtime_changed` |
/// | grew | forward | same / unknown | both match | append from the recorded offset | — |
/// | grew | forward | same / unknown | differ | whole-file rescan | `index.full_rescan.probe_mismatch` |
/// | grew | forward | same / unknown | absent (pre-004a entry) | append (the pre-004a rule) | — |
/// | grew | **backward** | any | not read | whole-file rescan | `index.full_rescan.mtime_regression` |
/// | shrank, or offset > size | any | any | not read | whole-file rescan | `index.full_rescan.size_regression` |
/// | no entry | — | — | — | whole-file rescan | `index.full_rescan.new_file` |
/// | DSH carrier name changed | any | any | not read | whole-file rescan (generation switch) | `index.full_rescan.file_identity_changed` |
/// | DSH grew, but lost logical lines | any | any | not read | whole-file rescan (the cursor is invalid) | `index.full_rescan.dsh_line_regression` |
///
/// Three rules that are easy to get wrong, stated once:
///
/// * **Size and timestamp both unchanged is a skip, and the probes are not
///   read.** That is the cheap path every round pays for every file; the
///   probes exist to separate "touched" from "rewritten" once the timestamp
///   moved, and the file identity to catch a replacement that moved neither.
/// * **An absent probe is not a mismatch.** A watermark written before
///   implementation-004a has `size`/`mtime`/`offset` only; the round behaves
///   exactly as before for that branch and writes the new fields for the next
///   one (no rescan, no counter).
/// * **A probe that cannot be read is a mismatch.** The build never resumes on
///   a prefix it could not verify.
///
/// Blind spot, declared: **the middle of a carrier is never probed — whatever the
/// length** (probing it would cost the file size on every round). What that means
/// per case:
///
/// * equal length: a rewrite whose first `PROBE_WINDOW` bytes and whose
///   `PROBE_WINDOW` bytes ending at the committed offset are byte-identical, that
///   keeps the file identity, and that lands in the middle is indistinguishable
///   from an unchanged file. It is reported as `index.mtime_only_refresh`, which
///   therefore means *"no change visible to the probes"*, not "the bytes are
///   provably identical";
/// * growth: a rewrite of the middle below the committed offset is not seen
///   either; the append resumes there exactly as it did before the probes
///   existed;
/// * past the committed offset: no probe covers those bytes. A trailing fragment
///   that becomes a complete line *and* moves the mtime is read back
///   (`index.tail_completion`, tail bytes only); one completed without moving the
///   mtime waits for the next growth or `--rebuild`.
///
/// The recovery path stays `agent-memory index --rebuild`.
pub mod watermark {
    use serde::{Deserialize, Serialize};
    use std::collections::BTreeMap;

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
    pub struct JsonlFile {
        /// Size at the last successful scan.
        pub size: u64,
        /// Modification time in nanoseconds since the epoch.
        pub mtime_ns: u128,
        /// Byte offset just past the last complete line that was indexed.
        pub offset: u64,
        /// Number of newline-terminated lines already indexed.
        pub lines: u64,
        /// sha256 of the first `stamp::PROBE_WINDOW` bytes of the indexed
        /// prefix. Empty on a watermark written before implementation-004a.
        #[serde(default)]
        pub head_probe: String,
        /// sha256 of the `stamp::PROBE_WINDOW` bytes ending at `offset`.
        #[serde(default)]
        pub anchor_probe: String,
        /// Windows volume serial number + file id; empty when unavailable.
        #[serde(default)]
        pub file_identity: String,
    }

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct JsonlWatermark {
        pub kind: String,
        pub files: BTreeMap<String, JsonlFile>,
    }

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
    pub struct FileStamp {
        pub size: u64,
        pub mtime_ns: u128,
    }

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct FilesWatermark {
        pub kind: String,
        pub files: BTreeMap<String, FileStamp>,
    }
    /// One DSH session directory. Keyed by the directory (not the carrier
    /// file) so a generation switch is visible as a different `file`. The
    /// probes and the identity follow the JSONL rules in the module doc; the
    /// anchor window ends at `size` (the committed physical length).
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct DshFile {
        /// Carrier file relative to the source root (`…/session.v4.jsonl.zstd`).
        pub file: String,
        /// Physical size at the last successful scan.
        pub size: u64,
        pub mtime_ns: u128,
        /// Number of complete logical lines already indexed.
        pub lines: u64,
        /// sha256 of the first `stamp::PROBE_WINDOW` bytes of the carrier.
        #[serde(default)]
        pub head_probe: String,
        /// sha256 of the `stamp::PROBE_WINDOW` bytes ending at `size`.
        #[serde(default)]
        pub anchor_probe: String,
        /// Windows volume serial number + file id; empty when unavailable.
        #[serde(default)]
        pub file_identity: String,
    }

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct DshWatermark {
        pub kind: String,
        pub files: BTreeMap<String, DshFile>,
    }

    impl DshWatermark {
        pub fn new() -> Self {
            DshWatermark {
                kind: "dsh".to_string(),
                files: BTreeMap::new(),
            }
        }
    }

    impl Default for DshWatermark {
        fn default() -> Self {
            DshWatermark::new()
        }
    }
    /// Size, modification time and (on Windows) file identity of one host
    /// database file — the same cheap signals the JSONL carriers use, applied to
    /// `opencode.db` and its `-wal`.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
    pub struct SqliteStamp {
        pub size: u64,
        /// Modification time in nanoseconds since the epoch.
        pub mtime_ns: u128,
        /// Windows volume serial number + file id; empty when unavailable.
        #[serde(default)]
        pub identity: String,
    }

    /// One `session_message` row the last reads witnessed above the per-record
    /// ceiling ([`crate::recall::port::sqlite_v2::MAX_ROW_BYTES`]).
    ///
    /// `IndexMeta::skipped_oversize` is a **current** count, not a tally since
    /// the first build: a rotation that reaches the session replaces its entries
    /// with what it just saw, so a row that shrank or disappeared stops being
    /// counted. The pairs are kept per session because only a session read can
    /// re-establish that session's set.
    #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Default)]
    pub struct OversizeRow {
        pub session_id: String,
        pub message_id: String,
    }

    /// The OpenCode (`sqlite-v2`) watermark.
    ///
    /// # How a round is decided (implementation-004b)
    ///
    /// | signal | meaning | round |
    /// |---|---|---|
    /// | `db_stamp` + `wal_stamp` + `wal_salt` unchanged | no host write since the last round | skip the live scan **and** the rolling reconcile, report `unchanged`, advance no cursor |
    /// | `max_rowid` present, `MAX(rowid)` grew | rows were appended | read `rowid > max_rowid` only (`index.opencode_tail_rows`) |
    /// | `MAX(rowid)` **below** `max_rowid` | VACUUM renumbered, the database was replaced, rows were removed from the tail | conservative full rebuild (`reason: watermark_rowid_regression`) |
    /// | `max_rowid` **absent** (pre-004b watermark) | no tail pointer to resume from | one legacy round (clock watermark + full key-set reconcile), then the tail |
    /// | `reconcile_cursor` | the last session the rolling slice visited | the slice continues after it, `ceil(sessions / 48)` per round |
    ///
    /// `rows` is retired: the round no longer counts the table (that `COUNT(*)`
    /// was a full pass every 30 minutes). It is kept in the shape so an index
    /// written before this batch still deserializes, and is carried over
    /// unchanged so a downgraded binary reads a consistent value.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct SqliteWatermark {
        pub kind: String,
        /// Column used as the update clock (`time_updated` when present).
        pub ts_column: String,
        /// Row count at the last counting round (retired, see above).
        #[serde(default)]
        pub rows: u64,
        /// Highest observed clock value (the legacy round's scan start).
        #[serde(default)]
        pub max_ts: i64,
        /// Schema generation the watermark belongs to.
        pub generation: String,
        /// Highest `rowid` observed at the last full or tail observation.
        #[serde(default)]
        pub max_rowid: Option<i64>,
        /// Last session the rolling reconcile slice ended at.
        #[serde(default)]
        pub reconcile_cursor: Option<String>,
        /// Slices completed in the current rotation (`0..reconcile_window`).
        #[serde(default)]
        pub reconcile_round: u64,
        /// Rounds one full rotation took when it was last computed.
        #[serde(default)]
        pub reconcile_window: u64,
        /// `opencode.db` stamp at the last observation.
        #[serde(default)]
        pub db_stamp: Option<SqliteStamp>,
        /// `opencode.db-wal` stamp at the last observation (`None` when absent).
        #[serde(default)]
        pub wal_stamp: Option<SqliteStamp>,
        /// The WAL's checkpoint counter + salts (bytes 12..24 of its header),
        /// hex. A checkpoint resets the WAL without necessarily moving its
        /// size, and this value still changes.
        #[serde(default)]
        pub wal_salt: String,
        /// Consecutive rounds without an observed source write. The unchanged
        /// fast path stays off until this reaches `reconcile_window`: one slice
        /// per quiet round then covers every session once after the last write,
        /// so a change only the slice can see (an in-place edit) is still found.
        #[serde(default)]
        pub quiet_rounds: u64,
        /// The rows last seen above the per-record ceiling. Maintained per
        /// session by the full scan, the tail and the rolling reconcile, so
        /// `skipped_oversize` stays a current count (see [`OversizeRow`]).
        #[serde(default)]
        pub oversize_rows: Vec<OversizeRow>,
    }

    impl JsonlWatermark {
        pub fn new() -> Self {
            JsonlWatermark {
                kind: "jsonl".to_string(),
                files: BTreeMap::new(),
            }
        }
    }

    impl FilesWatermark {
        pub fn new() -> Self {
            FilesWatermark {
                kind: "files".to_string(),
                files: BTreeMap::new(),
            }
        }
    }
}

/// File stamp (size + mtime) used by the per-file incremental rules.
pub fn file_stamp(path: &Path) -> Option<(u64, u128)> {
    let metadata = std::fs::metadata(path).ok()?;
    let mtime = metadata
        .modified()
        .ok()
        .and_then(|value| value.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|value| value.as_nanos())
        .unwrap_or(0);
    Some((metadata.len(), mtime))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terms_split_on_whitespace_and_keep_phrases() {
        let terms = parse_terms("alpha beta");
        assert_eq!(
            terms,
            vec![
                Term {
                    text: "alpha".into(),
                    phrase: false
                },
                Term {
                    text: "beta".into(),
                    phrase: false
                }
            ]
        );
        let terms = parse_terms("\"PrependPath=1\" x");
        assert_eq!(terms.len(), 2);
        assert_eq!(terms[0].text, "PrependPath=1");
        assert!(terms[0].phrase);
        assert_eq!(terms[1].text, "x");
        assert!(!terms[1].phrase);
        // Unbalanced quote: the remainder is one phrase.
        let terms = parse_terms("\"open phrase");
        assert_eq!(terms.len(), 1);
        assert_eq!(terms[0].text, "open phrase");
        assert!(terms[0].phrase);
        assert!(parse_terms("   ").is_empty());
    }

    #[test]
    fn match_expression_quotes_and_ands_only_long_terms() {
        let terms = parse_terms("abc de 中文字");
        assert_eq!(match_expression(&terms).unwrap(), "\"abc\" AND \"中文字\"");
        // Two-character CJK terms are below the trigram minimum (short terms).
        assert_eq!(match_expression(&parse_terms("中文")), None);
        assert_eq!(match_expression(&parse_terms("ab cd")), None);
        // A quoted phrase is one term and participates in the AND.
        assert_eq!(
            match_expression(&parse_terms("say \"a b\"")),
            Some("\"say\" AND \"a b\"".to_string())
        );
        assert_eq!(fts_phrase("a\"b"), "\"a\"\"b\"");
    }

    #[test]
    fn session_kind_rules_are_named_and_cover_all_sources() {
        use crate::recall::contract::{
            SESSION_KIND_INTERACTIVE, SESSION_KIND_UNKNOWN, SESSION_KIND_WORKER,
        };
        assert_eq!(
            opencode_session_kind(None, Some("chat")),
            SESSION_KIND_INTERACTIVE
        );
        assert_eq!(
            opencode_session_kind(Some("parent"), Some("chat")),
            SESSION_KIND_WORKER
        );
        assert_eq!(
            opencode_session_kind(None, Some("ocsr-dispatch")),
            SESSION_KIND_WORKER
        );
        assert_eq!(
            opencode_session_kind(None, Some("do it (subagent)")),
            SESSION_KIND_WORKER
        );
        assert_eq!(
            claude_session_kind("proj/s1/subagents/agent-x.jsonl", false),
            SESSION_KIND_WORKER
        );
        assert_eq!(
            claude_session_kind("proj/s1.jsonl", true),
            SESSION_KIND_WORKER
        );
        assert_eq!(
            claude_session_kind("proj/s1.jsonl", false),
            SESSION_KIND_INTERACTIVE
        );
        assert_eq!(codex_session_kind(Some("exec")), SESSION_KIND_WORKER);
        assert_eq!(codex_session_kind(Some("vscode")), SESSION_KIND_INTERACTIVE);
        assert_eq!(codex_session_kind(Some("cli")), SESSION_KIND_INTERACTIVE);
        assert_eq!(codex_session_kind(None), SESSION_KIND_UNKNOWN);
        assert_eq!(codex_session_kind(Some("weird")), SESSION_KIND_UNKNOWN);
    }

    #[test]
    fn source_id_directory_is_safe_and_collision_free() {
        assert_eq!(sanitize_source_id("oc"), "oc");
        assert_eq!(sanitize_source_id("a/b"), sanitize_source_id("a/b"));
        assert_ne!(sanitize_source_id("a/b"), sanitize_source_id("a_b"));
        assert!(!sanitize_source_id("a/b").contains('/'));
        assert_eq!(sanitize_source_id(".."), sanitize_source_id(".."));
        assert!(!sanitize_source_id("..").is_empty());
    }

    /// A second migration pass over an already widened registry is a no-op,
    /// and a column another process added first is success (P3-1).
    #[test]
    fn archive_sessions_migration_tolerates_existing_columns() {
        let conn = rusqlite::Connection::open_in_memory().expect("in-memory db");
        conn.execute_batch(
            "CREATE TABLE archive_sessions (session_id TEXT PRIMARY KEY, title TEXT);
             INSERT INTO archive_sessions (session_id) VALUES ('s1');",
        )
        .expect("old layout");
        migrate_archive_sessions(&conn).expect("first migration");
        migrate_archive_sessions(&conn).expect("second migration is idempotent");
        // The concurrent-first-open case: the column exists, the ALTER is the
        // one that loses the race.
        add_archive_sessions_column(&conn, "rule_version TEXT")
            .expect("an already-added column is not an error");
        let mut statement = conn
            .prepare("PRAGMA table_info(archive_sessions)")
            .expect("table info");
        let columns: Vec<String> = statement
            .query_map([], |row| row.get::<_, String>(1))
            .expect("columns")
            .collect::<Result<_, _>>()
            .expect("names");
        for expected in ["rule_version", "projected_units", "carrier_fingerprint"] {
            assert!(columns.iter().any(|name| name == expected), "{columns:?}");
        }
        let rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM archive_sessions", [], |row| {
                row.get(0)
            })
            .expect("row count");
        assert_eq!(rows, 1, "the migration keeps existing rows");
    }
}
