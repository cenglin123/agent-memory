//! P3 (stage-2 S1): SQLite evidence = chunked whitelist baseline + watermark delta.
//!
//! # Evidence model (plan §4 S1)
//!
//! A SQLite source is archived as a *combination*, not as one file copy:
//!
//! - **Baseline**: a chunked export of the whitelisted tables' rows, taken in
//!   *one read transaction* and written as per-table content-addressed chunk
//!   objects (role `sqlite_baseline_chunk`, each ≤ [`BASELINE_CHUNK_MAX_BYTES`]
//!   uncompressed, zstd-compressed JSONL). The manifest records the ordered
//!   chunk list, per-table row counts, the `CREATE TABLE` statements, the
//!   per-table watermark and a digest over the chunk list. There is **no
//!   backup-API copy** of the source database and **no whole-file image**:
//!   at an 8.8 GB source the baseline cost is proportional to the whitelisted
//!   payload, and credentials never enter the object store.
//! - **Raw row-level deltas**: between baselines the collector performs one
//!   *watermark observation* per table: rows whose watermark column
//!   (`time_updated`, else `seq`, else `time_created`, else `created`) moved
//!   past the stored `(value, pk)` watermark are re-read; deletes are found by
//!   comparing the primary-key set against the previous observation; tables
//!   without any watermark column are read in full and compared by row hash.
//!   Each delta line carries the observable facts: capture sequence,
//!   observation time, table, primary key, operation (`insert`/`update`/
//!   `delete`) and the observed before/after row content.
//!
//! # Table whitelist (credentials never enter the evidence)
//!
//! Only the capture profile's whitelisted tables are ever read: for the
//! OpenCode profile ([`SQLITE_TABLE_WHITELIST`]) those are `session`,
//! `message`, `part`, `session_v2`, `session_message`, `project`, `todo`. The
//! real OpenCode database also holds `credential`, `account`,
//! `control_account`, `kv`, `event`, ... — those tables are **never read
//! row-by-row**; every observation records only `{table, row_count}` facts
//! (`SELECT count(*)`), which appear in the capture and coverage reports.
//! Unknown *columns* on whitelisted tables are still stored verbatim.
//!
//! # Capture profiles (S4.5)
//!
//! Every framework-specific identifier above — the whitelist, the session
//! tables and their key columns, the session recency column, the child→session
//! link columns and the per-table watermark candidates — lives in a
//! [`SqliteCaptureProfile`], selected per provider
//! ([`profile_for_provider`]). OpenCode is the built-in profile and the
//! fallback; a new framework is a cc-switch provider plus a registered profile
//! ([`register_sqlite_capture_profile`]) — no engine change. Manifests and
//! collector state record the profile id; evidence written before profiles
//! existed (no id) is read as OpenCode.
//!
//! # Baseline triggers (deliberately rare)
//!
//! | trigger | reason tag |
//! |---|---|
//! | first capture | `initial` |
//! | whitelist schema differs from the baseline generation | `schema_change` |
//! | a whitelist table/column unknown to the baseline appears | `unknown_table_or_column` |
//! | explicit `--rebaseline` | `rebaseline_requested` |
//! | session filter (`--session-since`) changed | `filter_changed` |
//! | delta chain bytes exceed 50% of the baseline bytes | `delta_chain_threshold` |
//! | observation state or baseline chunks unavailable | `observation_state_missing` / `baseline_unavailable` |
//! | collector state cannot be trusted for a diff | `unclassifiable_state` |
//!
//! The old fixed chain limits (8 deltas / 512 KiB / 15 minutes) are gone: at a
//! 30-minute capture cycle they re-baselined — an 8.8 GB copy — on every run.
//! The remaining byte rule is *relative to the baseline*, so a quiet database
//! never re-baselines.
//!
//! # Blind window and verification
//!
//! The watermark method cannot see a row whose content changed while its
//! watermark column did not, nor writes at or below the watermark. This is
//! declared (`blindWindow`). `--verify` (or the 7-day cycle recorded in the
//! state) re-hashes every whitelisted row and compares against the archived
//! baseline+delta content; mismatching rows are re-recorded as correction
//! deltas and counted in `verifyMismatchesTotal`. Verification never writes to
//! the source and never copies the database.
//!
//! # Replay and readback
//!
//! [`replay_sqlite`] rebuilds the row state of any committed observation point
//! from the baseline chunk objects plus the deltas applied in order.
//! [`read_baseline_rows`] returns the archived baseline rows of one baseline
//! generation. Database materialization (schema + rows, parent-first insert
//! order, `PRAGMA foreign_keys = OFF`) lives with the derived replay.
//!
//! # Honest limits
//!
//! Observations are periodic, not continuous. A row that changes and changes
//! back entirely between two observations is indistinguishable from a row that
//! never changed; with watermark columns the same applies to content-only
//! edits. This module reports the observation watermark, the gap to the
//! previous observation (`observationIntervalMs`) and the resulting blind
//! window instead of claiming completeness. Nothing here claims a transaction
//! boundary or a point-in-time guarantee beyond "this is what one consistent
//! read observed".
//!
//! # Read-only source, single writer
//!
//! The source is opened read-only (`SQLITE_OPEN_READ_ONLY`); the collector
//! never writes to it (no session extension, no triggers, no shadow tables, no
//! index creation, no WAL frame or page decoding). Lock/busy conditions
//! surface as explicit health errors, never as an empty success.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use rusqlite::types::ValueRef;
use rusqlite::{Connection, ErrorCode, OpenFlags};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::Digest;
use std::io::{BufRead, BufReader, Cursor};

use crate::archive::{
    hex_lower, hex_sha256, new_id, now_ms, Archive, ArchiveError, Manifest, ManifestObject,
    ManifestRecord, ManifestSource, ManifestSqlite, ManifestSqliteChunk, RecordRef, SourceState,
    SourcesFile, SqliteState, Watermark, MANIFEST_SCHEMA,
};
use crate::capture::{CaptureOptions, CaptureReport, Failpoint};
use crate::config::atomic_write;
use crate::cost;
use crate::error::AppError;

/// SQLite database file magic (`SQLite format 3\0`).
pub const SQLITE_MAGIC: &[u8; 16] = b"SQLite format 3\0";

/// Tables whose row content is archived. Anything outside this list is only
/// counted (`SELECT count(*)`), never read row-by-row — this is the boundary
/// that keeps credentials out of the evidence store.
///
/// S4.5: this is the OpenCode capture profile's table list, kept as a
/// compatibility alias; the engine itself reads every framework-specific name
/// from a [`SqliteCaptureProfile`].
pub static SQLITE_TABLE_WHITELIST: &[&str] = SqliteCaptureProfile::OPENCODE.tables;

/// Schema tag of one raw row-level delta line (documentation only: each line
/// carries the facts, and its owning manifest carries `deltaSchema`).
pub const DELTA_LINE_SCHEMA: &str = "agent-memory/sqlite-delta@2";
/// Schema tag of the compressed collector observation state file.
pub const OBSERVATION_SCHEMA: &str = "agent-memory/sqlite-observation@2";
/// Schema tag of one baseline chunk export (first line of every chunk).
pub const BASELINE_CHUNK_SCHEMA: &str = "agent-memory/sqlite-baseline@2";

/// Default busy timeout for the read-only source connection.
pub const DEFAULT_LOCK_TIMEOUT_MS: u64 = 250;

/// Maximum uncompressed size of one baseline chunk object.
pub const BASELINE_CHUNK_MAX_BYTES: usize = 32 * 1024 * 1024;
/// A delta chain whose raw bytes exceed this percentage of the baseline bytes
/// triggers a new baseline (the only remaining automatic baseline trigger).
pub const BASELINE_CHAIN_BYTE_PERCENT: u64 = 50;
/// Tables without a watermark column up to this many rows are expected; larger
/// ones take the same full-scan hash path (correct, but the report names them
/// because the cost is no longer bounded).
pub const FULL_SCAN_HASH_MAX_ROWS: u64 = 10_000;
/// Periodic verification cycle (7 days), recorded in the collector state.
pub const VERIFY_INTERVAL_MS: i64 = 7 * 24 * 60 * 60 * 1000;

/// Insert order for replay materialization: parents before children so foreign
/// keys never see a dangling row, whitelisted extras last. S4.5: the OpenCode
/// profile's replay order, kept as a compatibility alias.
pub static PARENT_FIRST_TABLE_ORDER: &[&str] = SqliteCaptureProfile::OPENCODE.replay_order;

// ---------------------------------------------------------------------------
// Capture profiles (S4.5)
// ---------------------------------------------------------------------------

/// One framework's SQLite capture profile: everything the engine used to
/// hardcode as OpenCode constants and column names, per framework.
///
/// The engine reads *every* framework-specific identifier from here: the
/// ordered table whitelist, the session tables with their session key column,
/// the session recency column for `--session-since`, the child→session link
/// columns, and the per-table watermark column candidates. Adding a framework
/// means adding a profile (plus its cc-switch provider) — no engine change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqliteCaptureProfile {
    /// Stable profile id recorded in manifests, collector state and
    /// observation state (`opencode`, ...). Evidence or state without this
    /// field is interpreted as `opencode`.
    pub profile_id: &'static str,
    /// Whitelisted tables in baseline export order (chunk objects follow this
    /// order). Anything else is only counted, never read row-by-row.
    pub tables: &'static [&'static str],
    /// Parent-first insert order used when replaying evidence into a database
    /// (foreign-key-safe materialization).
    pub replay_order: &'static [&'static str],
    /// Session tables as `(table, session key column)`.
    pub session_tables: &'static [(&'static str, &'static str)],
    /// Session recency column the `--session-since` filter applies to (epoch
    /// milliseconds, like the filter itself).
    pub session_time_column: &'static str,
    /// Child tables and the column linking a row to its session:
    /// `(table, session link column)`. Tables not listed here are never
    /// session-scoped.
    pub child_session_columns: &'static [(&'static str, &'static str)],
    /// Watermark column candidates in priority order, used for every table
    /// without a [`Self::table_watermark_overrides`] entry. A table matching
    /// none takes the full-scan hash path.
    pub watermark_columns: &'static [&'static str],
    /// Per-table watermark candidates, checked before
    /// [`Self::watermark_columns`].
    pub table_watermark_overrides: &'static [(&'static str, &'static [&'static str])],
    /// Explicitly excluded tables, documented with the profile. Outside the
    /// whitelist nothing is read anyway (`SELECT count(*)` facts only); this
    /// list names the credential-like tables so the exclusion is a recorded
    /// decision, not an accident.
    pub excluded_tables: &'static [&'static str],
}

impl SqliteCaptureProfile {
    /// The OpenCode profile — item-for-item the constants and column names the
    /// engine hardcoded before S4.5. Behaviour is unchanged for OpenCode.
    pub const OPENCODE: SqliteCaptureProfile = SqliteCaptureProfile {
        profile_id: "opencode",
        tables: &[
            "session",
            "message",
            "part",
            "session_v2",
            "session_message",
            "project",
            "todo",
        ],
        replay_order: &[
            "project",
            "session",
            "session_v2",
            "message",
            "session_message",
            "part",
            "todo",
        ],
        session_tables: &[("session", "id"), ("session_v2", "id")],
        session_time_column: "time_updated",
        child_session_columns: &[
            ("message", "session_id"),
            ("part", "session_id"),
            ("session_message", "session_id"),
            ("todo", "session_id"),
        ],
        watermark_columns: &["time_updated", "seq", "time_created", "created"],
        table_watermark_overrides: &[],
        excluded_tables: &[],
    };

    /// True when `table` is one of the profile's session tables.
    pub fn is_session_table(&self, table: &str) -> bool {
        self.session_key_column(table).is_some()
    }

    /// The session key column of a session table (`session`.`id` for OpenCode).
    pub fn session_key_column(&self, table: &str) -> Option<&'static str> {
        self.session_tables
            .iter()
            .find(|(known, _)| *known == table)
            .map(|(_, column)| *column)
    }

    /// The column linking `table`'s rows to a session, when the table is
    /// session-scoped at all.
    pub fn child_session_column(&self, table: &str) -> Option<&'static str> {
        self.child_session_columns
            .iter()
            .find(|(known, _)| *known == table)
            .map(|(_, column)| *column)
    }

    /// Watermark column candidates for `table`: the per-table override when
    /// present, else the profile default.
    pub fn watermark_candidates(&self, table: &str) -> &'static [&'static str] {
        self.table_watermark_overrides
            .iter()
            .find(|(known, _)| *known == table)
            .map(|(_, columns)| *columns)
            .unwrap_or(self.watermark_columns)
    }
}

/// The OpenCode profile (the engine's original behaviour, now by profile).
pub static OPENCODE_PROFILE: &SqliteCaptureProfile = &SqliteCaptureProfile::OPENCODE;

/// Profiles registered for future framework providers. Built-ins
/// (`opencode`) are resolved first; registration is the extension point —
/// wiring a new framework's provider to its profile, without engine changes.
static PROFILE_REGISTRY: std::sync::OnceLock<std::sync::RwLock<ProfileRegistry>> =
    std::sync::OnceLock::new();

#[derive(Default)]
struct ProfileRegistry {
    /// provider name -> profile (`profile_for_provider`).
    by_provider: BTreeMap<String, &'static SqliteCaptureProfile>,
    /// profile id -> profile (`sqlite_capture_profile_by_id`).
    by_id: BTreeMap<String, &'static SqliteCaptureProfile>,
}

fn profile_registry() -> &'static std::sync::RwLock<ProfileRegistry> {
    PROFILE_REGISTRY.get_or_init(|| std::sync::RwLock::new(ProfileRegistry::default()))
}

/// Registration entry point for a future framework provider: makes `profile`
/// resolvable as `provider`'s capture profile and by its own
/// [`SqliteCaptureProfile::profile_id`]. Idempotent per provider and per
/// profile id (re-registration replaces). Production wiring for a new
/// framework is this call plus the cc-switch provider — the engine and the
/// capture enumeration stay unchanged.
pub fn register_sqlite_capture_profile(
    provider: &str,
    profile: &'static SqliteCaptureProfile,
) {
    let mut registry = profile_registry()
        .write()
        .unwrap_or_else(|poison| poison.into_inner());
    registry
        .by_provider
        .insert(provider.to_string(), profile);
    registry.by_id.insert(profile.profile_id.to_string(), profile);
}

/// Resolve a capture profile by its [`SqliteCaptureProfile::profile_id`]
/// (built-ins first, then registered profiles). Evidence and collector state
/// record exactly this id.
pub fn sqlite_capture_profile_by_id(profile_id: &str) -> Option<&'static SqliteCaptureProfile> {
    if profile_id == SqliteCaptureProfile::OPENCODE.profile_id {
        return Some(OPENCODE_PROFILE);
    }
    profile_registry()
        .read()
        .unwrap_or_else(|poison| poison.into_inner())
        .by_id
        .get(profile_id)
        .copied()
}

/// The capture profile a provider captures under: the profile registered for
/// it, else the OpenCode profile. The fallback covers `claude`/`codex` (JSONL
/// providers whose rare SQLite sources were always captured with these
/// constants) and every provider registered before S4.5.
pub fn profile_for_provider(provider: &str) -> &'static SqliteCaptureProfile {
    if provider == SqliteCaptureProfile::OPENCODE.profile_id {
        return OPENCODE_PROFILE;
    }
    profile_registry()
        .read()
        .unwrap_or_else(|poison| poison.into_inner())
        .by_provider
        .get(provider)
        .copied()
        .unwrap_or(OPENCODE_PROFILE)
}

const ZSTD_LEVEL: i32 = 3;

static LOCK_TIMEOUT_OVERRIDE_MS: AtomicU64 = AtomicU64::new(0);

/// Test seam: shorten the busy timeout so lock-timeout behaviour is exercised
/// quickly. `None` restores [`DEFAULT_LOCK_TIMEOUT_MS`]. Production never calls
/// this.
pub fn set_lock_timeout_for_tests(ms: Option<u64>) {
    LOCK_TIMEOUT_OVERRIDE_MS.store(ms.unwrap_or(0), Ordering::Relaxed);
}

fn lock_timeout_ms() -> u64 {
    match LOCK_TIMEOUT_OVERRIDE_MS.load(Ordering::Relaxed) {
        0 => DEFAULT_LOCK_TIMEOUT_MS,
        ms => ms,
    }
}

// ---------------------------------------------------------------------------
// Health errors
// ---------------------------------------------------------------------------

/// An explicit SQLite health failure. Never converted into an empty success.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SqliteHealthError {
    /// `sqlite_unavailable` | `sqlite_locked` | `sqlite_corrupt`.
    pub code: String,
    pub message: String,
}

impl SqliteHealthError {
    fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.to_string(),
            message: message.into(),
        }
    }
}

impl std::fmt::Display for SqliteHealthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

fn is_busy(err: &rusqlite::Error) -> bool {
    matches!(
        err.sqlite_error_code(),
        Some(ErrorCode::DatabaseBusy) | Some(ErrorCode::DatabaseLocked)
    )
}

fn map_sqlite_error(err: rusqlite::Error) -> SqliteHealthError {
    if is_busy(&err) {
        SqliteHealthError::new(
            "sqlite_locked",
            format!("source database stayed locked beyond the busy timeout: {err}"),
        )
    } else {
        SqliteHealthError::new("sqlite_unavailable", err.to_string())
    }
}

/// Baseline export writes to the archive while reading the source; an archive
/// failure surfaces as an explicit health failure of the run.
impl From<ArchiveError> for SqliteHealthError {
    fn from(err: ArchiveError) -> Self {
        SqliteHealthError::new("sqlite_unavailable", err.to_string())
    }
}

/// A verification-pass health failure propagates after the capture unit is
/// already committed; the next run retries verification (the timestamp only
/// advances when verification completed).
impl From<SqliteHealthError> for ArchiveError {
    fn from(err: SqliteHealthError) -> Self {
        ArchiveError::Corrupt(format!("sqlite {}: {}", err.code, err.message))
    }
}

// ---------------------------------------------------------------------------
// Source detection
// ---------------------------------------------------------------------------

/// True when `path` is a SQLite database (by file magic, not by extension).
pub fn is_sqlite_file(path: &Path) -> bool {
    use std::io::Read;
    let Ok(mut file) = fs::File::open(path) else {
        return false;
    };
    let mut head = [0u8; 16];
    match file.read_exact(&mut head) {
        Ok(()) => &head == SQLITE_MAGIC,
        Err(_) => false,
    }
}

/// True when the file *name* says SQLite. A damaged OpenCode database (header
/// corrupted) must surface as an explicit `sqlite_unavailable` health error, not
/// silently fall through to whole-file capture.
pub fn looks_like_sqlite_path(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|extension| extension.to_str())
            .map(|extension| extension.to_ascii_lowercase())
            .as_deref(),
        Some("db") | Some("sqlite") | Some("sqlite3")
    )
}

// ---------------------------------------------------------------------------
// Schema
// ---------------------------------------------------------------------------

/// A table as declared by the database: column order, primary key, rowid
/// availability. No model of the framework's schema is imposed.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SchemaTable {
    pub columns: Vec<String>,
    pub primary_key: Vec<String>,
    /// False for `WITHOUT ROWID` tables.
    pub rowid: bool,
}

fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn open_readonly(path: &Path) -> Result<Connection, SqliteHealthError> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|err| {
        if is_busy(&err) {
            map_sqlite_error(err)
        } else {
            SqliteHealthError::new(
                "sqlite_unavailable",
                format!("cannot open {} read-only: {err}", path.display()),
            )
        }
    })?;
    conn.busy_timeout(Duration::from_millis(lock_timeout_ms()))
        .map_err(map_sqlite_error)?;
    // Bound the collector's page cache: incremental capture and verification
    // must stay memory-predictable at an 8.8 GB source, and the default cache
    // lets the pager grow with the table instead of with the observation.
    conn.pragma_update(None, "cache_size", -16_000i64)
        .map_err(map_sqlite_error)?;
    Ok(conn)
}

/// Read the declared schema: every non-internal table, its columns in declared
/// order, its primary key (in key order) and whether it has a rowid. Only
/// table *declarations* are read here, for every table including
/// non-whitelisted ones; row content is never touched.
fn read_schema(conn: &Connection) -> Result<BTreeMap<String, SchemaTable>, SqliteHealthError> {
    let mut statement = conn
        .prepare(
            "SELECT name, sql FROM sqlite_master \
             WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
        )
        .map_err(map_sqlite_error)?;
    let entries = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
        })
        .map_err(map_sqlite_error)?;
    let mut declared: Vec<(String, Option<String>)> = Vec::new();
    for entry in entries {
        declared.push(entry.map_err(map_sqlite_error)?);
    }
    let mut tables = BTreeMap::new();
    for (name, sql) in declared {
        let mut info = conn
            .prepare(&format!("PRAGMA table_info({})", quote_ident(&name)))
            .map_err(map_sqlite_error)?;
        let rows = info
            .query_map([], |row| Ok((row.get::<_, String>(1)?, row.get::<_, i64>(5)?)))
            .map_err(map_sqlite_error)?;
        let mut columns = Vec::new();
        let mut keys: Vec<(i64, String)> = Vec::new();
        for row in rows {
            let (column, pk) = row.map_err(map_sqlite_error)?;
            if pk > 0 {
                keys.push((pk, column.clone()));
            }
            columns.push(column);
        }
        keys.sort();
        let without_rowid = sql
            .as_deref()
            .map(|s| s.to_uppercase().replace(' ', "").contains("WITHOUTROWID"))
            .unwrap_or(false);
        tables.insert(
            name,
            SchemaTable {
                columns,
                primary_key: keys.into_iter().map(|(_, name)| name).collect(),
                rowid: !without_rowid,
            },
        );
    }
    Ok(tables)
}

/// Read the schema and its whitelist fingerprint, inside one read transaction.
fn probe_schema(
    profile: &SqliteCaptureProfile,
    conn: &Connection,
) -> Result<(BTreeMap<String, SchemaTable>, String), SqliteHealthError> {
    in_read_transaction(conn, |conn| {
        let schema = read_schema(conn)?;
        let fingerprint = schema_fingerprint_in(profile, &schema);
        Ok((schema, fingerprint))
    })
}

fn whitelist_schema(
    profile: &SqliteCaptureProfile,
    schema: &BTreeMap<String, SchemaTable>,
) -> BTreeMap<String, SchemaTable> {
    schema
        .iter()
        .filter(|(name, _)| profile.tables.contains(&name.as_str()))
        .map(|(name, table)| (name.clone(), table.clone()))
        .collect()
}

/// Schema fingerprint over the whitelisted tables only. Non-whitelisted tables
/// (credentials, kv, event, ...) must never influence capture decisions — the
/// collector does not even read their rows.
fn schema_fingerprint_in(
    profile: &SqliteCaptureProfile,
    schema: &BTreeMap<String, SchemaTable>,
) -> String {
    let mut canonical = String::new();
    for (name, table) in whitelist_schema(profile, schema) {
        canonical.push_str(&name);
        canonical.push('(');
        for column in &table.columns {
            canonical.push_str(column);
            canonical.push(',');
        }
        canonical.push_str(")pk(");
        for key in &table.primary_key {
            canonical.push_str(key);
            canonical.push(',');
        }
        canonical.push_str(")rowid=");
        canonical.push_str(if table.rowid { "1" } else { "0" });
        canonical.push(';');
    }
    hex_sha256(canonical.as_bytes())
}

/// Whitelist tables/columns the previous baseline generation did not know.
fn unknown_whitelist_schema_in(
    profile: &SqliteCaptureProfile,
    previous_columns: &BTreeMap<String, Vec<String>>,
    schema: &BTreeMap<String, SchemaTable>,
) -> bool {
    for (name, table) in whitelist_schema(profile, schema) {
        match previous_columns.get(&name) {
            None => return true,
            Some(columns) => {
                if table.columns.iter().any(|column| !columns.contains(column)) {
                    return true;
                }
            }
        }
    }
    false
}

/// The watermark column of a table by the profile's candidate priority, if any.
fn watermark_column_in(
    profile: &SqliteCaptureProfile,
    table: &str,
    columns: &[String],
) -> Option<String> {
    profile
        .watermark_candidates(table)
        .iter()
        .find(|candidate| columns.iter().any(|column| column == *candidate))
        .map(|column| (*column).to_string())
}

/// OpenCode-profile variants of the helpers above, kept only for the
/// pre-existing unit tests that pin the original constant behaviour.
#[cfg(test)]
fn schema_fingerprint(schema: &BTreeMap<String, SchemaTable>) -> String {
    schema_fingerprint_in(&SqliteCaptureProfile::OPENCODE, schema)
}

#[cfg(test)]
fn unknown_whitelist_schema(
    previous_columns: &BTreeMap<String, Vec<String>>,
    schema: &BTreeMap<String, SchemaTable>,
) -> bool {
    unknown_whitelist_schema_in(&SqliteCaptureProfile::OPENCODE, previous_columns, schema)
}

#[cfg(test)]
fn watermark_column(columns: &[String]) -> Option<String> {
    watermark_column_in(&SqliteCaptureProfile::OPENCODE, "", columns)
}

fn value_to_json(value: ValueRef<'_>) -> Value {
    match value {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(number) => Value::from(number),
        ValueRef::Real(number) => match serde_json::Number::from_f64(number) {
            Some(number) => Value::Number(number),
            None => Value::String(if number.is_nan() {
                "nan".to_string()
            } else if number > 0.0 {
                "inf".to_string()
            } else {
                "-inf".to_string()
            }),
        },
        // Text that is not valid UTF-8 (SQLite does not enforce it) and blobs
        // are preserved as exact hex, so no value is silently dropped.
        ValueRef::Text(text) => match std::str::from_utf8(text) {
            Ok(text) => Value::String(text.to_string()),
            Err(_) => serde_json::json!({"invalid_utf8_hex": hex_lower(text)}),
        },
        ValueRef::Blob(bytes) => serde_json::json!({"blob_hex": hex_lower(bytes)}),
    }
}

/// Materialize one row as the canonical column-ordered JSON object.
fn row_object(
    row: &rusqlite::Row<'_>,
    declared: &SchemaTable,
    offset: usize,
) -> Result<Map<String, Value>, SqliteHealthError> {
    let mut object = Map::new();
    for (index, column) in declared.columns.iter().enumerate() {
        let value = row.get_ref(index + offset).map_err(map_sqlite_error)?;
        object.insert(column.clone(), value_to_json(value));
    }
    Ok(object)
}

fn rowid_value(row: &rusqlite::Row<'_>, declared: &SchemaTable) -> Option<i64> {
    if !declared.rowid {
        return None;
    }
    match row.get_ref(0).ok()? {
        ValueRef::Integer(number) => Some(number),
        _ => None,
    }
}

/// Canonical row key: primary key when declared, else rowid, else content hash.
fn row_key_from_object(
    declared: &SchemaTable,
    object: &Map<String, Value>,
    rowid: Option<i64>,
) -> String {
    if !declared.primary_key.is_empty() {
        let values: Vec<Value> = declared
            .primary_key
            .iter()
            .map(|key| object.get(key).cloned().unwrap_or(Value::Null))
            .collect();
        return format!("pk:{}", serde_json::to_string(&values).unwrap_or_default());
    }
    if let Some(rowid) = rowid {
        return format!("rowid:{rowid}");
    }
    let canonical = serde_json::to_string(object).unwrap_or_default();
    format!("row:{}", hex_sha256(canonical.as_bytes()))
}

fn row_key(
    row: &rusqlite::Row<'_>,
    declared: &SchemaTable,
    object: &Map<String, Value>,
) -> String {
    row_key_from_object(declared, object, rowid_value(row, declared))
}

fn row_hash(object: &Map<String, Value>) -> String {
    hex_sha256(&serde_json::to_vec(&Value::Object(object.clone())).unwrap_or_default())
}

fn row_hash_value(row: &Value) -> String {
    match row.as_object() {
        Some(object) => row_hash(object),
        None => hex_sha256(&serde_json::to_vec(row).unwrap_or_default()),
    }
}

/// Streaming duplicate-payload fact (payloads seen identically in more than
/// one table). Keeps only payload hashes, never rows.
#[derive(Default)]
struct DuplicateTracker {
    payload_tables: BTreeMap<String, BTreeSet<String>>,
}

impl DuplicateTracker {
    fn observe(&mut self, table: &str, object: &Map<String, Value>) {
        for column in ["data", "content"] {
            let Some(value) = object.get(column) else {
                continue;
            };
            let text = match value {
                Value::String(text) => text.clone(),
                other => serde_json::to_string(other).unwrap_or_default(),
            };
            if text.is_empty() {
                continue;
            }
            self.payload_tables
                .entry(hex_sha256(text.as_bytes()))
                .or_default()
                .insert(table.to_string());
        }
    }

    fn count(&self) -> u64 {
        self.payload_tables
            .values()
            .filter(|tables| tables.len() > 1)
            .count() as u64
    }
}

/// Run `f` inside one read transaction (one consistent observation).
fn in_read_transaction<T>(
    conn: &Connection,
    f: impl FnOnce(&Connection) -> Result<T, SqliteHealthError>,
) -> Result<T, SqliteHealthError> {
    conn.execute_batch("BEGIN").map_err(map_sqlite_error)?;
    let result = f(conn);
    let _ = conn.execute_batch("COMMIT");
    result
}

/// Canonical iteration order for one table: primary-key order when declared
/// (so keys, chunk exports and source scans line up for the streaming
/// verification merge), else rowid order (keys are `rowid:N`), else unordered.
fn order_suffix(declared: &SchemaTable) -> String {
    if !declared.primary_key.is_empty() {
        format!(
            " ORDER BY {}",
            declared
                .primary_key
                .iter()
                .map(|column| quote_ident(column))
                .collect::<Vec<_>>()
                .join(", ")
        )
    } else if declared.rowid {
        " ORDER BY rowid".to_string()
    } else {
        String::new()
    }
}

/// True when a table has a canonical order, so verification can use the
/// streaming merge (both sides iterate in `order_suffix` order).
fn orderable(declared: &SchemaTable) -> bool {
    !declared.primary_key.is_empty() || declared.rowid
}

/// Append a disambiguation suffix for a repeated (non-unique) primary key.
/// SQLite allows repeated NULLs (and duplicates) in a non-INTEGER primary key,
/// so a key collision is broken with the rowid (or the iteration index for
/// WITHOUT ROWID tables) instead of letting one row overwrite another. Both
/// the content read and the key scan apply the same rule in the same order.
fn disambiguate_key(
    collision: bool,
    rowid: Option<i64>,
    iteration_index: usize,
    key: String,
) -> String {
    if !collision {
        return key;
    }
    match rowid {
        Some(rowid) => format!("{key}#rowid:{rowid}"),
        None => format!("{key}#dup:{iteration_index}"),
    }
}

/// Read all rows of one whitelist table. `session_since` restricts the
/// profile's session time column on session tables; `selected` restricts
/// children by their session link column (no restriction when the set is
/// empty).
fn read_all_rows(
    profile: &SqliteCaptureProfile,
    conn: &Connection,
    table: &str,
    declared: &SchemaTable,
    session_since: Option<i64>,
    selected: &BTreeSet<String>,
) -> Result<BTreeMap<String, Value>, SqliteHealthError> {
    let is_session_table = profile.is_session_table(table);
    let session_link = profile.child_session_column(table);
    let mut sql = if declared.rowid {
        format!("SELECT rowid, * FROM {}", quote_ident(table))
    } else {
        format!("SELECT * FROM {}", quote_ident(table))
    };
    if let Some(since) = session_since {
        if is_session_table && declared.columns.iter().any(|c| c == profile.session_time_column)
        {
            sql += &format!(" WHERE {} >= {since}", quote_ident(profile.session_time_column));
        }
    }
    sql += &order_suffix(declared);
    let mut statement = conn.prepare(&sql).map_err(map_sqlite_error)?;
    let mut rows = statement.query([]).map_err(map_sqlite_error)?;
    let offset = if declared.rowid { 1 } else { 0 };
    let mut out = BTreeMap::new();
    // N3: a child table without its session link column cannot be
    // session-scoped; it is read in full (and reported by the caller) instead
    // of silently dropping every row.
    let child_scopable = session_link
        .map(|column| declared.columns.iter().any(|c| c == column))
        .unwrap_or(false);
    while let Some(row) = rows.next().map_err(map_sqlite_error)? {
        let object = row_object(row, declared, offset)?;
        if child_scopable && !selected.is_empty() {
            let link = session_link.unwrap_or_default();
            let session_id = object.get(link).and_then(Value::as_str);
            match session_id {
                Some(id) if selected.contains(id) => {}
                _ => continue,
            }
        }
        let key = row_key(row, declared, &object);
        let key = disambiguate_key(
            out.contains_key(&key),
            rowid_value(row, declared),
            out.len(),
            key,
        );
        out.insert(key, Value::Object(object));
    }
    Ok(out)
}

/// Live primary keys of one table (only key columns are read).
fn scan_table_keys(
    profile: &SqliteCaptureProfile,
    conn: &Connection,
    table: &str,
    declared: &SchemaTable,
) -> Result<BTreeSet<String>, SqliteHealthError> {
    let mut out = BTreeSet::new();
    if !declared.primary_key.is_empty() {
        let mut select: Vec<String> = declared
            .primary_key
            .iter()
            .map(|column| quote_ident(column))
            .collect();
        if declared.rowid {
            select.push("rowid".to_string());
        }
        let mut sql = format!(
            "SELECT {} FROM {}",
            select.join(", "),
            quote_ident(table)
        );
        sql += &order_suffix(declared);
        let mut statement = conn.prepare(&sql).map_err(map_sqlite_error)?;
        let mut rows = statement.query([]).map_err(map_sqlite_error)?;
        while let Some(row) = rows.next().map_err(map_sqlite_error)? {
            let values: Vec<Value> = declared
                .primary_key
                .iter()
                .enumerate()
                .map(|(index, _)| {
                    row.get_ref(index)
                        .map(value_to_json)
                        .unwrap_or(Value::Null)
                })
                .collect();
            let key = format!(
                "pk:{}",
                serde_json::to_string(&values).unwrap_or_default()
            );
            let rowid = if declared.rowid {
                Some(row.get::<_, i64>(declared.primary_key.len()).map_err(map_sqlite_error)?)
            } else {
                None
            };
            let key = disambiguate_key(out.contains(&key), rowid, out.len(), key);
            out.insert(key);
        }
        return Ok(out);
    }
    if declared.rowid {
        let sql = format!("SELECT rowid FROM {}", quote_ident(table));
        let mut statement = conn.prepare(&sql).map_err(map_sqlite_error)?;
        let mut rows = statement.query([]).map_err(map_sqlite_error)?;
        while let Some(row) = rows.next().map_err(map_sqlite_error)? {
            out.insert(format!(
                "rowid:{}",
                row.get::<_, i64>(0).map_err(map_sqlite_error)?
            ));
        }
        return Ok(out);
    }
    // WITHOUT ROWID without a declared key: keys are content hashes.
    Ok(
        read_all_rows(profile, conn, table, declared, None, &BTreeSet::new())?
            .into_keys()
            .collect(),
    )
}

/// Raw session ids of one session table, scoped to the filter.
fn scan_session_ids(
    profile: &SqliteCaptureProfile,
    conn: &Connection,
    table: &str,
    declared: &SchemaTable,
    since: i64,
) -> Result<BTreeSet<String>, SqliteHealthError> {
    let mut out = BTreeSet::new();
    let Some(key_column) = profile.session_key_column(table) else {
        return Ok(out);
    };
    if !declared
        .columns
        .iter()
        .any(|column| column == profile.session_time_column)
    {
        return Ok(out);
    }
    let mut statement = conn
        .prepare(&format!(
            "SELECT {} FROM {} WHERE {} >= ?1",
            quote_ident(key_column),
            quote_ident(table),
            quote_ident(profile.session_time_column)
        ))
        .map_err(map_sqlite_error)?;
    let rows = statement
        .query_map([since], |row| row.get::<_, String>(0))
        .map_err(map_sqlite_error)?;
    for id in rows {
        out.insert(id.map_err(map_sqlite_error)?);
    }
    Ok(out)
}

/// Live primary keys of a session table, scoped to the filter.
fn scan_session_keys(
    profile: &SqliteCaptureProfile,
    conn: &Connection,
    table: &str,
    declared: &SchemaTable,
    since: i64,
) -> Result<BTreeSet<String>, SqliteHealthError> {
    if !declared
        .columns
        .iter()
        .any(|column| column == profile.session_time_column)
    {
        return scan_table_keys(profile, conn, table, declared);
    }
    let Some(key_column) = profile.session_key_column(table) else {
        return scan_table_keys(profile, conn, table, declared);
    };
    // Fast path (the OpenCode shape): the declared primary key is exactly the
    // session key column, so the synthesized `pk` keys are the row keys the
    // observation read produces.
    if declared.primary_key.len() == 1 && declared.primary_key[0] == key_column {
        return Ok(scan_session_ids(profile, conn, table, declared, since)?
            .into_iter()
            .map(|id| format!("pk:{}", serde_json::to_string(&[Value::from(id)]).unwrap_or_default()))
            .collect());
    }
    // Any other shape (e.g. a session table without a declared primary key,
    // whose rows key by rowid): keys must follow the same row_key rule as the
    // observation read, or the delete comparison would compare two different
    // key spaces.
    let mut out = BTreeSet::new();
    let mut sql = if declared.rowid {
        format!("SELECT rowid, * FROM {}", quote_ident(table))
    } else {
        format!("SELECT * FROM {}", quote_ident(table))
    };
    sql += &format!(
        " WHERE {} >= ?1",
        quote_ident(profile.session_time_column)
    );
    sql += &order_suffix(declared);
    let mut statement = conn.prepare(&sql).map_err(map_sqlite_error)?;
    let mut rows = statement.query([since]).map_err(map_sqlite_error)?;
    let offset = if declared.rowid { 1 } else { 0 };
    while let Some(row) = rows.next().map_err(map_sqlite_error)? {
        let object = row_object(row, declared, offset)?;
        let key = row_key(row, declared, &object);
        let key = disambiguate_key(out.contains(&key), rowid_value(row, declared), out.len(), key);
        out.insert(key);
    }
    Ok(out)
}

/// Live primary keys of a child table, scoped to the selected sessions.
fn scan_child_keys(
    profile: &SqliteCaptureProfile,
    conn: &Connection,
    table: &str,
    declared: &SchemaTable,
    selected: &BTreeSet<String>,
) -> Result<BTreeSet<String>, SqliteHealthError> {
    let session_link = profile.child_session_column(table);
    let link_scopable = session_link
        .map(|column| declared.columns.iter().any(|c| c == column))
        .unwrap_or(false);
    if selected.is_empty() || !link_scopable {
        return scan_table_keys(profile, conn, table, declared);
    }
    let link = session_link.unwrap_or_default();
    if declared.primary_key.is_empty() {
        // Without a declared key, keys come from the full row read.
        return Ok(
            read_all_rows(profile, conn, table, declared, None, selected)?
                .into_keys()
                .collect(),
        );
    }
    let mut out = BTreeSet::new();
    let mut columns: Vec<String> = declared
        .primary_key
        .iter()
        .map(|column| quote_ident(column))
        .collect();
    columns.push(quote_ident(link));
    let sql = format!(
        "SELECT {} FROM {}",
        columns.join(", "),
        quote_ident(table)
    );
    let mut statement = conn.prepare(&sql).map_err(map_sqlite_error)?;
    let mut rows = statement.query([]).map_err(map_sqlite_error)?;
    let session_index = declared.primary_key.len();
    while let Some(row) = rows.next().map_err(map_sqlite_error)? {
        let session_id: Option<String> = row.get(session_index).ok();
        match session_id.as_deref() {
            Some(id) if selected.contains(id) => {}
            _ => continue,
        }
        let values: Vec<Value> = declared
            .primary_key
            .iter()
            .enumerate()
            .map(|(index, _)| {
                row.get_ref(index)
                    .map(value_to_json)
                    .unwrap_or(Value::Null)
            })
            .collect();
        out.insert(format!(
            "pk:{}",
            serde_json::to_string(&values).unwrap_or_default()
        ));
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Live observation readback (tests, verification support)
// ---------------------------------------------------------------------------

/// One consistent whole read of the whitelisted rows (used by readback and
/// tests; the incremental collector uses the watermark path instead).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Observation {
    pub schema: String,
    pub schema_fingerprint: String,
    /// Whitelisted table -> row key -> canonical row object.
    pub tables: BTreeMap<String, BTreeMap<String, Value>>,
}

impl Observation {
    /// Total rows observed.
    pub fn row_count(&self) -> u64 {
        self.tables.values().map(|t| t.len() as u64).sum()
    }
}

/// Observe one database through an already-open read-only connection, inside
/// one read transaction. Only whitelist rows are read.
fn observe_connection(
    profile: &SqliteCaptureProfile,
    conn: &Connection,
) -> Result<Observation, SqliteHealthError> {
    in_read_transaction(conn, |conn| {
        let schema = read_schema(conn)?;
        let fingerprint = schema_fingerprint_in(profile, &schema);
        let whitelisted = whitelist_schema(profile, &schema);
        let mut tables = BTreeMap::new();
        for (name, declared) in &whitelisted {
            let rows = read_all_rows(profile, conn, name, declared, None, &BTreeSet::new())?;
            tables.insert(name.clone(), rows);
        }
        Ok(Observation {
            schema: OBSERVATION_SCHEMA.to_string(),
            schema_fingerprint: fingerprint,
            tables,
        })
    })
}

/// Perform one consistent read of a database file under the OpenCode capture
/// profile. The connection is read-only.
pub fn observe_database(path: &Path) -> Result<Observation, SqliteHealthError> {
    observe_database_in(&SqliteCaptureProfile::OPENCODE, path)
}

/// [`observe_database`] under an explicit capture profile.
pub fn observe_database_in(
    profile: &SqliteCaptureProfile,
    path: &Path,
) -> Result<Observation, SqliteHealthError> {
    let conn = open_readonly(path)?;
    observe_connection(profile, &conn)
}

// ---------------------------------------------------------------------------
// Observation state (collector state, not evidence)
// ---------------------------------------------------------------------------

/// Per-table collector state (sqlite-observation@2): watermark + primary-key
/// set (+ full-scan row hashes for tables without a watermark column). No row
/// content is stored — row content lives in the evidence, and `before` images
/// are read back from there.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct TableState {
    pub columns: Vec<String>,
    pub primary_key: Vec<String>,
    pub rowid: bool,
    /// Watermark column in use ("" = full-scan hashing).
    #[serde(default)]
    pub watermark_column: String,
    /// Watermark `(value, key)`: every row at or below this was archived.
    #[serde(default)]
    pub watermark_value: i64,
    #[serde(default)]
    pub watermark_key: String,
    #[serde(default)]
    pub watermark_known: bool,
    /// True when `primary_keys` is a complete observation of the table (a
    /// crash-adopted baseline starts with an empty, untrusted set: same-value
    /// unseen keys must not be reported as new inserts then, N1).
    #[serde(default)]
    pub primary_keys_known: bool,
    /// Primary keys present at the last observation (delete detection).
    #[serde(default)]
    pub primary_keys: BTreeSet<String>,
    /// Tables without a watermark column: pk -> canonical row hash.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub row_hashes: Option<BTreeMap<String, String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ObservationFile {
    schema: String,
    source: String,
    /// S4.5: capture profile this state belongs to. Empty on state written
    /// before profiles existed (read as the OpenCode profile); any other
    /// profile mismatch is rejected so a profile switch re-baselines.
    #[serde(default)]
    profile_id: String,
    /// Evidence watermark this observation belongs to (state must never claim a
    /// different point than the committed evidence).
    capture_seq: u64,
    observed_at_ms: i64,
    schema_fingerprint: String,
    /// sha256 over the canonical state map: detects state tampering or
    /// structural corruption instead of silently diffing against altered state.
    state_digest: String,
    /// Session filter in force at this observation.
    #[serde(default)]
    session_since_ms: Option<i64>,
    /// Sessions selected by the filter (union over observations; children are
    /// scoped by these ids).
    #[serde(default)]
    selected_sessions: BTreeSet<String>,
    tables: BTreeMap<String, TableState>,
    /// Non-whitelisted tables: row counts only.
    #[serde(default)]
    excluded: BTreeMap<String, u64>,
}

impl ObservationFile {
    fn new(
        profile: &SqliteCaptureProfile,
        source: &str,
        capture_seq: u64,
        observed_at_ms: i64,
        schema_fingerprint: String,
        session_since_ms: Option<i64>,
    ) -> Self {
        Self {
            schema: OBSERVATION_SCHEMA.to_string(),
            source: source.to_string(),
            profile_id: profile.profile_id.to_string(),
            capture_seq,
            observed_at_ms,
            schema_fingerprint,
            state_digest: String::new(),
            session_since_ms,
            selected_sessions: BTreeSet::new(),
            tables: BTreeMap::new(),
            excluded: BTreeMap::new(),
        }
    }

    fn compute_digest(&self) -> String {
        let canonical =
            serde_json::to_vec(&(&self.tables, &self.excluded, &self.selected_sessions))
                .unwrap_or_default();
        hex_sha256(&canonical)
    }

    fn seal(&mut self) {
        self.state_digest = self.compute_digest();
    }

    fn verify_digest(&self) -> Result<(), ArchiveError> {
        if self.state_digest != self.compute_digest() {
            return Err(ArchiveError::Corrupt(
                "observation state digest mismatch".to_string(),
            ));
        }
        Ok(())
    }
}

fn observation_relative_path(source_key: &str) -> String {
    format!(
        "state/sqlite/{}.obs.zst",
        &hex_sha256(source_key.as_bytes())[..32]
    )
}

/// Gate bookkeeping file of one source's automatic verification pass.
///
/// Scheduling state, not evidence: it holds only the deferral counter and the
/// start of the current deferral block, is rebuilt by normal operation, and is
/// safe to delete (the next round starts a fresh block).
fn verify_gate_relative_path(source_key: &str) -> String {
    format!(
        "state/sqlite/{}.gate-verify.json",
        &hex_sha256(source_key.as_bytes())[..32]
    )
}

/// §E gate verdict for the automatic 7-day verification of one source.
///
/// Records the deferral block in the source's gate file (a blocked round counts
/// one deferral and keeps the block start, an executed round clears it) so a
/// permanently busy desktop still verifies once every 48 hours of blocking.
fn verify_gate_decision(
    archive: &Archive,
    source_key: &str,
    now: i64,
) -> crate::gate::GateDecision {
    let path = archive.root().join(verify_gate_relative_path(source_key));
    let mut state = crate::gate::GateState::load(&path, "verify");
    let observation = crate::gate::observe();
    let mut decision = crate::gate::assess_job(
        &observation,
        &crate::gate::GatePolicy::default(),
        &state,
        now,
    );
    state.record(&decision, &crate::gate::stamp(now));
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    // Bookkeeping only: a failed write never fails the capture. It is never silent
    // either — with an unwritable state file the deferral block cannot accumulate,
    // so the 48-hour ceiling would never force the pass, and the report says so.
    if let Err(error) = state.save(&path) {
        decision.reasons.push(format!(
            "gate_state_unwritable: {} ({error}); the deferral block does not persist, \
             so the 48 h ceiling cannot accumulate",
            path.display()
        ));
        decision.detection.insert(
            "gate_state".to_string(),
            format!("write_failed: {error}"),
        );
    }
    cost::counter(
        if decision.action.executes() {
            "verify_gate.executed"
        } else {
            "verify_gate.deferred"
        },
        1,
    );
    if decision.forced {
        cost::counter("verify_gate.forced", 1);
    }
    decision
}

/// Attach the gate verdict to the report (both the delta and the unchanged
/// path publish it, so a deferred verification is visible, never silent).
fn apply_verify_gate(
    report: &mut CaptureReport,
    gate: &Option<crate::gate::GateDecision>,
) {
    if let Some(info) = report.sqlite.as_mut() {
        info.verify_gate = gate.clone();
    }
}

/// Parse the state file's schema tag without committing to the full @2 shape,
/// so an older version is reported explicitly instead of as a generic decode
/// error.
#[derive(Deserialize)]
struct ObservationSchemaProbe {
    #[serde(default)]
    schema: String,
}

fn load_observation_in(
    archive: &Archive,
    relative: &str,
    source_key: &str,
    profile: &SqliteCaptureProfile,
) -> Result<Option<ObservationFile>, ArchiveError> {
    // §A: reading (and validating) the observation state was one of the
    // unmeasured paths; it decompresses a full key-set snapshot.
    let _phase = cost::phase("observation_state");
    let path = archive.root().join(relative);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(AppError::io(&path, err).into()),
    };
    cost::counter("observation_state.reads", 1);
    cost::counter("observation_state.read_bytes", bytes.len() as u64);
    let decoded = zstd::stream::decode_all(bytes.as_slice())
        .map_err(|err| ArchiveError::Corrupt(format!("{relative}: {err}")))?;
    cost::counter(
        "observation_state.decompressed_bytes",
        decoded.len() as u64,
    );
    let probe: ObservationSchemaProbe = serde_json::from_slice(&decoded).map_err(|err| {
        ArchiveError::Corrupt(format!("{relative}: not an observation state: {err}"))
    })?;
    if probe.schema != OBSERVATION_SCHEMA {
        // Older formats are explicitly untrusted (production never held @1
        // data): the caller must re-baseline, never diff against them.
        return Err(ArchiveError::Corrupt(format!(
            "{relative}: unsupported observation schema {} (expected {}); \
             the state is not trusted and a new baseline is required",
            probe.schema, OBSERVATION_SCHEMA
        )));
    }
    let file: ObservationFile = serde_json::from_slice(&decoded)
        .map_err(|err| ArchiveError::Corrupt(format!("{relative}: {err}")))?;
    if file.source != source_key {
        return Err(ArchiveError::Corrupt(format!(
            "{relative}: observation belongs to another source"
        )));
    }
    // S4.5: state belongs to exactly one capture profile. Empty = written
    // before profiles existed = OpenCode; anything else must match the
    // profile in force, or the state is not trusted for a diff.
    let state_profile_id = if file.profile_id.is_empty() {
        SqliteCaptureProfile::OPENCODE.profile_id
    } else {
        file.profile_id.as_str()
    };
    if state_profile_id != profile.profile_id {
        return Err(ArchiveError::Corrupt(format!(
            "{relative}: observation belongs to capture profile {state_profile_id:?} but the \
             capture runs under {:?}; the state is not trusted and a new \
             baseline is required",
            profile.profile_id
        )));
    }
    file.verify_digest()?;
    Ok(Some(file))
}

/// OpenCode-profile variant kept for the pre-existing unit tests.
#[cfg(test)]
fn load_observation(
    archive: &Archive,
    relative: &str,
    source_key: &str,
) -> Result<Option<ObservationFile>, ArchiveError> {
    load_observation_in(archive, relative, source_key, &SqliteCaptureProfile::OPENCODE)
}

fn save_observation(
    archive: &Archive,
    relative: &str,
    file: &ObservationFile,
) -> Result<(), ArchiveError> {
    let _phase = cost::phase("observation_state");
    let mut file = file.clone();
    file.seal();
    let json = serde_json::to_vec(&file)
        .map_err(|err| ArchiveError::Corrupt(format!("observation state: {err}")))?;
    let compressed = zstd::stream::encode_all(json.as_slice(), ZSTD_LEVEL)
        .map_err(|err| ArchiveError::Corrupt(format!("observation state: {err}")))?;
    atomic_write(&archive.root().join(relative), &compressed)?;
    cost::counter("observation_state.writes", 1);
    cost::counter("observation_state.write_bytes", compressed.len() as u64);
    Ok(())
}

fn wal_bytes(path: &Path) -> u64 {
    let wal = PathBuf::from(format!("{}-wal", path.display()));
    fs::metadata(&wal).map(|meta| meta.len()).unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Baseline chunk export
// ---------------------------------------------------------------------------

/// One row line of a baseline chunk (`{"key":…,"row":…}`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BaselineRowLine {
    pub key: String,
    pub row: Value,
}

/// A fetched (decoded + verified) evidence object. The archive-fallback
/// batch reader supplies this closure so a batch of chunk/batch reads pays
/// the manifest access scan once instead of once per object.
pub type ObjectFetch<'a> = &'a dyn Fn(&str) -> Result<Vec<u8>, ArchiveError>;

/// First line of a baseline chunk: format tag + table identity.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct BaselineChunkHeader {
    schema: String,
    table: String,
    columns: Vec<String>,
}

/// A content-addressed baseline chunk (manifest projection).
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
struct ExportedChunk {
    chunk: ManifestSqliteChunk,
    compressed_bytes: u64,
}

/// Accumulates baseline rows and flushes content-addressed chunk objects.
struct ChunkWriter<'a> {
    archive: &'a Archive,
    table: String,
    columns: Vec<String>,
    max_bytes: usize,
    buffer: Vec<u8>,
    rows: u64,
    min_key: Option<String>,
    max_key: Option<String>,
    chunks: Vec<ExportedChunk>,
    objects_new: u64,
    objects_deduped: u64,
}

impl<'a> ChunkWriter<'a> {
    fn new(
        archive: &'a Archive,
        table: &str,
        columns: Vec<String>,
        max_bytes: usize,
    ) -> Result<Self, ArchiveError> {
        let mut writer = Self {
            archive,
            table: table.to_string(),
            columns,
            max_bytes: max_bytes.max(64),
            buffer: Vec::new(),
            rows: 0,
            min_key: None,
            max_key: None,
            chunks: Vec::new(),
            objects_new: 0,
            objects_deduped: 0,
        };
        writer.write_header();
        Ok(writer)
    }

    fn write_header(&mut self) {
        let header = BaselineChunkHeader {
            schema: BASELINE_CHUNK_SCHEMA.to_string(),
            table: self.table.clone(),
            columns: self.columns.clone(),
        };
        serde_json::to_writer(&mut self.buffer, &header)
            .map_err(|err| ArchiveError::Corrupt(format!("baseline chunk header: {err}")))
            .expect("baseline chunk header serializes");
        self.buffer.push(b'\n');
    }

    fn push(&mut self, key: &str, object: &Map<String, Value>) -> Result<(), ArchiveError> {
        let line = BaselineRowLine {
            key: key.to_string(),
            row: Value::Object(object.clone()),
        };
        serde_json::to_writer(&mut self.buffer, &line)
            .map_err(|err| ArchiveError::Corrupt(format!("baseline chunk line: {err}")))?;
        self.buffer.push(b'\n');
        // Track the chunk's key range so `before` lookups can skip chunks
        // whose range cannot contain the target keys.
        self.min_key = Some(match self.min_key.take() {
            Some(min) if min.as_str() <= key => min,
            _ => key.to_string(),
        });
        self.max_key = Some(match self.max_key.take() {
            Some(max) if max.as_str() >= key => max,
            _ => key.to_string(),
        });
        self.rows += 1;
        if self.buffer.len() >= self.max_bytes {
            self.flush()?;
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<(), ArchiveError> {
        if self.rows == 0 {
            return Ok(());
        }
        let raw = std::mem::take(&mut self.buffer);
        let raw_sha = hex_sha256(&raw);
        let compressed = zstd::stream::encode_all(raw.as_slice(), ZSTD_LEVEL)
            .map_err(|err| ArchiveError::Corrupt(format!("baseline chunk compression: {err}")))?;
        let (sha, compressed_bytes, is_new) = self.archive.put_object(&compressed)?;
        if is_new {
            self.objects_new += 1;
        } else {
            self.objects_deduped += 1;
        }
        self.chunks.push(ExportedChunk {
            chunk: ManifestSqliteChunk {
                table: self.table.clone(),
                sha256: sha,
                bytes: raw.len() as u64,
                rows: self.rows,
                raw_sha256: raw_sha,
                min_key: self.min_key.take().unwrap_or_default(),
                max_key: self.max_key.take().unwrap_or_default(),
            },
            compressed_bytes,
        });
        self.rows = 0;
        // Every chunk is self-describing: re-emit the header.
        self.write_header();
        Ok(())
    }
}

/// Read one chunk object back (decompress + hash-verify).
fn read_chunk_rows(
    archive: &Archive,
    domain: &str,
    chunk: &ManifestSqliteChunk,
) -> Result<Vec<BaselineRowLine>, ArchiveError> {
    let fetch = |sha256: &str| archive.reader(domain).read_object(sha256);
    read_chunk_rows_fetch(&fetch, chunk)
}

/// Read one baseline chunk over a caller-supplied object fetcher (batch
/// readers precompute the object access map, so N chunk reads cost one
/// manifest scan).
pub fn read_chunk_rows_fetch(
    fetch: ObjectFetch<'_>,
    chunk: &ManifestSqliteChunk,
) -> Result<Vec<BaselineRowLine>, ArchiveError> {
    let compressed = fetch(&chunk.sha256)?;
    if hex_sha256(&compressed) != chunk.sha256 {
        return Err(ArchiveError::Integrity(format!(
            "baseline chunk {} does not match its content address",
            chunk.sha256
        )));
    }
    let raw = zstd::stream::decode_all(compressed.as_slice()).map_err(|err| {
        ArchiveError::Integrity(format!("chunk {} is not zstd: {err}", chunk.sha256))
    })?;
    note_decompressed(raw.len() as u64);
    if raw.len() as u64 != chunk.bytes || hex_sha256(&raw) != chunk.raw_sha256 {
        return Err(ArchiveError::Integrity(format!(
            "baseline chunk {} failed raw content verification",
            chunk.sha256
        )));
    }
    let mut lines = raw.split(|byte| *byte == b'\n');
    let header_line = lines.next().unwrap_or_default();
    let header: BaselineChunkHeader = serde_json::from_slice(header_line)
        .map_err(|err| ArchiveError::Corrupt(format!("chunk {} header: {err}", chunk.sha256)))?;
    if header.schema != BASELINE_CHUNK_SCHEMA || header.table != chunk.table {
        return Err(ArchiveError::Corrupt(format!(
            "chunk {} declares table {:?} but the manifest says {:?}",
            chunk.sha256, header.table, chunk.table
        )));
    }
    let mut rows = Vec::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let parsed: BaselineRowLine = serde_json::from_slice(line)
            .map_err(|err| ArchiveError::Corrupt(format!("chunk {} line: {err}", chunk.sha256)))?;
        rows.push(parsed);
    }
    if rows.len() as u64 != chunk.rows {
        return Err(ArchiveError::Integrity(format!(
            "chunk {} holds {} rows, manifest says {}",
            chunk.sha256,
            rows.len(),
            chunk.rows
        )));
    }
    Ok(rows)
}

/// Total evidence bytes this process has decompressed (baseline chunks and
/// delta objects). A capture whose cost were proportional to the whole
/// evidence store would drive this up by the store size every cycle; the
/// collector reads only chunks/batches whose key ranges contain the keys it
/// needs, so a steady-state cycle stays far below that. Exposed for the scale
/// assertions and honest reporting.
pub static EVIDENCE_DECOMPRESSED_BYTES: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Read-side metric snapshot.
pub fn evidence_decompressed_bytes() -> u64 {
    EVIDENCE_DECOMPRESSED_BYTES.load(std::sync::atomic::Ordering::Relaxed)
}

/// Test seam: zero the read-side metric.
pub fn reset_evidence_decompressed_bytes() {
    EVIDENCE_DECOMPRESSED_BYTES.store(0, std::sync::atomic::Ordering::Relaxed);
}

fn note_decompressed(bytes: u64) {
    EVIDENCE_DECOMPRESSED_BYTES.fetch_add(bytes, std::sync::atomic::Ordering::Relaxed);
}

/// Streaming, hash-verified reader over one baseline chunk: yields one row
/// line at a time (memory ≈ one line + the decompression window), so the
/// verification merge and bounded `before` lookups never materialize a whole
/// chunk.
struct ChunkLineStream {
    inner: BufReader<zstd::stream::read::Decoder<'static, Cursor<Vec<u8>>>>,
    buffer: Vec<u8>,
    hasher: sha2::Sha256,
    table: String,
    rows_seen: u64,
    bytes_seen: u64,
    header_parsed: bool,
    finished: bool,
}

impl ChunkLineStream {
    fn open(
        archive: &Archive,
        domain: &str,
        chunk: &ManifestSqliteChunk,
    ) -> Result<Self, ArchiveError> {
        let compressed = archive.reader(domain).read_object(&chunk.sha256)?;
        Self::open_compressed(compressed, chunk)
    }

    /// Open over a caller-supplied object fetcher, so a pass that already owns
    /// an access snapshot reads and checks the object exactly once.
    fn open_with(fetch: ObjectFetch<'_>, chunk: &ManifestSqliteChunk) -> Result<Self, ArchiveError> {
        let compressed = fetch(&chunk.sha256)?;
        Self::open_compressed(compressed, chunk)
    }

    fn open_compressed(
        compressed: Vec<u8>,
        chunk: &ManifestSqliteChunk,
    ) -> Result<Self, ArchiveError> {
        if hex_sha256(&compressed) != chunk.sha256 {
            return Err(ArchiveError::Integrity(format!(
                "baseline chunk {} does not match its content address",
                chunk.sha256
            )));
        }
        let inner = BufReader::new(
            zstd::stream::read::Decoder::with_buffer(Cursor::new(compressed)).map_err(|err| {
                ArchiveError::Integrity(format!("chunk {} is not zstd: {err}", chunk.sha256))
            })?,
        );
        Ok(Self {
            inner,
            buffer: Vec::new(),
            hasher: sha2::Sha256::new(),
            table: chunk.table.clone(),
            rows_seen: 0,
            bytes_seen: 0,
            header_parsed: false,
            finished: false,
        })
    }

    /// Next `(key, row)` line, or `None` at the chunk end (after verifying the
    /// row count and the raw-content hash).
    fn next_row(
        &mut self,
        chunk: &ManifestSqliteChunk,
    ) -> Result<Option<(String, Value)>, ArchiveError> {
        loop {
            self.buffer.clear();
            let read = self
                .inner
                .read_until(b'\n', &mut self.buffer)
                .map_err(|err| {
                    ArchiveError::Integrity(format!(
                        "chunk {} decompress failed: {err}",
                        chunk.sha256
                    ))
                })?;
            if read == 0 {
                if !self.finished {
                    self.finished = true;
                    self.finish(chunk)?;
                }
                return Ok(None);
            }
            self.hasher.update(&self.buffer[..read]);
            self.bytes_seen += read as u64;
            note_decompressed(read as u64);
            while self
                .buffer
                .last()
                .is_some_and(|byte| *byte == b'\n' || *byte == b'\r')
            {
                // read_until keeps the delimiter; strip it for parsing.
                let _ = self.buffer.pop();
            }
            if self.buffer.is_empty() {
                continue;
            }
            if !self.header_parsed {
                self.header_parsed = true;
                let header: BaselineChunkHeader = serde_json::from_slice(&self.buffer)
                    .map_err(|err| {
                        ArchiveError::Corrupt(format!("chunk {} header: {err}", chunk.sha256))
                    })?;
                if header.schema != BASELINE_CHUNK_SCHEMA || header.table != self.table {
                    return Err(ArchiveError::Corrupt(format!(
                        "chunk {} declares table {:?} but the manifest says {:?}",
                        chunk.sha256, header.table, chunk.table
                    )));
                }
                continue;
            }
            let parsed: BaselineRowLine = serde_json::from_slice(&self.buffer).map_err(|err| {
                ArchiveError::Corrupt(format!("chunk {} line: {err}", chunk.sha256))
            })?;
            self.rows_seen += 1;
            return Ok(Some((parsed.key, parsed.row)));
        }
    }

    fn finish(&mut self, chunk: &ManifestSqliteChunk) -> Result<(), ArchiveError> {
        use sha2::Digest;
        let digest = hex_lower(self.hasher.finalize_reset().as_slice());
        if self.rows_seen != chunk.rows
            || self.bytes_seen != chunk.bytes
            || digest != chunk.raw_sha256
        {
            return Err(ArchiveError::Integrity(format!(
                "baseline chunk {} failed raw content verification \
                 (rows {}/{}, bytes {}/{})",
                chunk.sha256, self.rows_seen, chunk.rows, self.bytes_seen, chunk.bytes
            )));
        }
        Ok(())
    }
}

/// Baseline export result (one read transaction).
struct BaselineExport {
    chunks: Vec<ManifestSqliteChunk>,
    /// Compressed size per chunk (parallel to `chunks`).
    chunk_compressed_bytes: Vec<u64>,
    compressed_bytes_total: u64,
    table_rows: BTreeMap<String, u64>,
    columns: BTreeMap<String, Vec<String>>,
    ddl: BTreeMap<String, String>,
    watermarks: BTreeMap<String, (i64, String)>,
    table_states: BTreeMap<String, TableState>,
    excluded: BTreeMap<String, u64>,
    selected_sessions: BTreeSet<String>,
    total_bytes: u64,
    digest: String,
    schema_fingerprint: String,
    rows_discovered: u64,
    duplicates: u64,
    oversize_full_scan_tables: Vec<String>,
    unfiltered_tables: Vec<String>,
    objects_new: u64,
    objects_deduped: u64,
}

/// `{table, row_count}` facts for every table outside the whitelist. This is
/// the only access non-whitelisted tables ever get (`SELECT count(*)`).
fn count_excluded_tables(
    profile: &SqliteCaptureProfile,
    conn: &Connection,
    schema: &BTreeMap<String, SchemaTable>,
) -> Result<BTreeMap<String, u64>, SqliteHealthError> {
    let mut excluded = BTreeMap::new();
    for name in schema.keys() {
        if profile.tables.contains(&name.as_str()) {
            continue;
        }
        let count: i64 = conn
            .query_row(
                &format!("SELECT count(*) FROM {}", quote_ident(name)),
                [],
                |row| row.get(0),
            )
            .map_err(map_sqlite_error)?;
        excluded.insert(name.clone(), count.max(0) as u64);
    }
    Ok(excluded)
}

/// Export the whitelisted rows of every whitelist table as chunk objects, in
/// one read transaction. Children are scoped to the selected sessions when a
/// session filter is active; non-session extra tables (`project` for OpenCode)
/// are always exported in full; tables outside the whitelist are only counted.
fn export_baseline(
    profile: &SqliteCaptureProfile,
    archive: &Archive,
    conn: &Connection,
    schema: &BTreeMap<String, SchemaTable>,
    session_since_ms: Option<i64>,
) -> Result<BaselineExport, SqliteHealthError> {
    let whitelisted = whitelist_schema(profile, schema);
    // Session selection (only when a session table actually exists; otherwise
    // the filter has nothing to select from and children stay unfiltered).
    let mut selected: BTreeSet<String> = BTreeSet::new();
    let has_session_table = profile
        .session_tables
        .iter()
        .any(|(table, _)| whitelisted.contains_key(*table));
    let filter_active = session_since_ms.is_some() && has_session_table;
    if let Some(since) = session_since_ms.filter(|_| has_session_table) {
        for (table, _) in profile.session_tables {
            let Some(declared) = whitelisted.get(*table) else {
                continue;
            };
            selected.extend(scan_session_ids(profile, conn, table, declared, since)?);
        }
    }

    // Children filtering goes through a connection-private TEMP table so
    // non-selected rows are skipped by SQLite itself (the main database is
    // never written; TEMP lives in the connection's private temp store).
    if filter_active {
        conn.execute_batch("CREATE TEMP TABLE am_selected_sessions (id TEXT PRIMARY KEY)")
            .map_err(map_sqlite_error)?;
        let mut statement = conn
            .prepare("INSERT INTO am_selected_sessions (id) VALUES (?1)")
            .map_err(map_sqlite_error)?;
        for id in &selected {
            statement.execute([id]).map_err(map_sqlite_error)?;
        }
    }

    let mut export = BaselineExport {
        chunks: Vec::new(),
        chunk_compressed_bytes: Vec::new(),
        compressed_bytes_total: 0,
        table_rows: BTreeMap::new(),
        columns: BTreeMap::new(),
        ddl: BTreeMap::new(),
        watermarks: BTreeMap::new(),
        table_states: BTreeMap::new(),
        excluded: count_excluded_tables(profile, conn, schema)?,
        selected_sessions: selected.clone(),
        total_bytes: 0,
        digest: String::new(),
        schema_fingerprint: schema_fingerprint_in(profile, schema),
        rows_discovered: 0,
        duplicates: 0,
        oversize_full_scan_tables: Vec::new(),
        unfiltered_tables: Vec::new(),
        objects_new: 0,
        objects_deduped: 0,
    };
    let mut duplicates = DuplicateTracker::default();

    for table in profile.tables.iter().copied() {
        let Some(declared) = whitelisted.get(table) else {
            continue;
        };
        // §A: the baseline export reads every whitelisted table in full; the
        // per-table sub-phase is part of `watermark_read`.
        let _table_phase = cost::phase(format!("watermark_read.tables.{table}"));
        let ddl_sql: Option<String> = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = ?1",
                [table],
                |row| row.get::<_, Option<String>>(0),
            )
            .map_err(map_sqlite_error)?;
        if let Some(sql) = ddl_sql {
            export.ddl.insert(table.to_string(), sql);
        }
        export
            .columns
            .insert(table.to_string(), declared.columns.clone());

        let watermark = watermark_column_in(profile, table, &declared.columns);
        let session_link = profile.child_session_column(table);
        // N3: a child table without its session link column cannot be scoped
        // by the session filter — it is captured in full and named in the
        // report instead of failing the whole capture.
        let child_scopable = session_link
            .map(|column| declared.columns.iter().any(|c| c == column))
            .unwrap_or(false);
        let child_filtered = filter_active && session_link.is_some() && child_scopable;
        if filter_active && session_link.is_some() && !child_scopable
            && !export.unfiltered_tables.contains(&table.to_string())
        {
            export.unfiltered_tables.push(table.to_string());
        }
        let mut writer = ChunkWriter::new(
            archive,
            table,
            declared.columns.clone(),
            BASELINE_CHUNK_MAX_BYTES,
        )?;
        let mut keys = BTreeSet::new();
        let mut hashes = watermark.is_none().then(BTreeMap::new);
        let mut watermark_max: Option<(i64, String)> = None;
        let mut table_row_count = 0u64;

        let mut sql = if declared.rowid {
            format!("SELECT rowid, * FROM {}", quote_ident(table))
        } else {
            format!("SELECT * FROM {}", quote_ident(table))
        };
        if child_filtered {
            sql += &format!(
                " WHERE {} IN (SELECT id FROM am_selected_sessions)",
                quote_ident(session_link.unwrap_or_default())
            );
        } else if filter_active
            && profile.is_session_table(table)
            && declared
                .columns
                .iter()
                .any(|c| c == profile.session_time_column)
        {
            sql += &format!(
                " WHERE {} >= {}",
                quote_ident(profile.session_time_column),
                session_since_ms.unwrap_or(0)
            );
        }
        sql += &order_suffix(declared);
        {
            let mut statement = conn.prepare(&sql).map_err(map_sqlite_error)?;
            let mut rows = statement.query([]).map_err(map_sqlite_error)?;
            let offset = if declared.rowid { 1 } else { 0 };
            while let Some(row) = rows.next().map_err(map_sqlite_error)? {
                let object = row_object(row, declared, offset)?;
                let raw_key = row_key(row, declared, &object);
                let key = disambiguate_key(
                    keys.contains(&raw_key),
                    rowid_value(row, declared),
                    keys.len(),
                    raw_key,
                );
                if let Some(column) = &watermark {
                    if let Some(value) = object.get(column).and_then(Value::as_i64) {
                        let candidate = (value, key.clone());
                        if watermark_max.as_ref().is_none_or(|current| candidate > *current) {
                            watermark_max = Some(candidate);
                        }
                    }
                }
                if let Some(hashes) = hashes.as_mut() {
                    hashes.insert(key.clone(), row_hash(&object));
                }
                duplicates.observe(table, &object);
                keys.insert(key.clone());
                writer.push(&key, &object)?;
                table_row_count += 1;
            }
        }
        writer.flush()?;
        for exported in &writer.chunks {
            export.total_bytes += exported.chunk.bytes;
            export.compressed_bytes_total += exported.compressed_bytes;
            export.chunk_compressed_bytes.push(exported.compressed_bytes);
            export.chunks.push(exported.chunk.clone());
        }
        export.objects_new += writer.objects_new;
        export.objects_deduped += writer.objects_deduped;
        record_table_scanned(table, table_row_count);
        export.table_rows.insert(table.to_string(), table_row_count);
        export.rows_discovered += table_row_count;
        if watermark.is_none() && table_row_count > FULL_SCAN_HASH_MAX_ROWS {
            export.oversize_full_scan_tables.push(table.to_string());
        }
        if let Some((value, key)) = watermark_max.clone() {
            export.watermarks.insert(table.to_string(), (value, key));
        }
        export.table_states.insert(
            table.to_string(),
            TableState {
                columns: declared.columns.clone(),
                primary_key: declared.primary_key.clone(),
                rowid: declared.rowid,
                watermark_column: watermark.clone().unwrap_or_default(),
                watermark_value: watermark_max.as_ref().map(|(value, _)| *value).unwrap_or(0),
                watermark_key: watermark_max
                    .as_ref()
                    .map(|(_, key)| key.clone())
                    .unwrap_or_default(),
                watermark_known: true,
                primary_keys_known: true,
                primary_keys: keys,
                row_hashes: hashes,
            },
        );
    }

    if filter_active {
        let _ = conn.execute_batch("DROP TABLE IF EXISTS am_selected_sessions");
    }

    export.duplicates = duplicates.count();
    // Digest over the ordered chunk list: two exports of the same content are
    // one baseline (dedupe), independent of chunking boundaries.
    export.digest = baseline_digest_of(&export.chunks);
    Ok(export)
}

fn baseline_digest_of(chunks: &[ManifestSqliteChunk]) -> String {
    let digest_input: Vec<(&String, &String, &u64, &u64, &String, &String, &String)> = chunks
        .iter()
        .map(|chunk| {
            (
                &chunk.table,
                &chunk.sha256,
                &chunk.bytes,
                &chunk.rows,
                &chunk.raw_sha256,
                &chunk.min_key,
                &chunk.max_key,
            )
        })
        .collect();
    hex_sha256(&serde_json::to_vec(&digest_input).unwrap_or_default())
}

/// Verify every baseline chunk object in the list is present in the store.
fn baseline_objects_present(archive: &Archive, chunks: &[ManifestSqliteChunk]) -> bool {
    chunks.iter().all(|chunk| archive.has_object(&chunk.sha256))
}

// ---------------------------------------------------------------------------
// Watermark observation (delta detection)
// ---------------------------------------------------------------------------

/// One detected change. `before` is filled from the archived evidence (never
/// from collector state).
struct Change {
    table: String,
    key: String,
    op: &'static str,
    before: Option<Value>,
    after: Option<Value>,
}

/// Result of one watermark observation.
struct PendingObservation {
    changes: Vec<Change>,
    tables: BTreeMap<String, TableState>,
    excluded: BTreeMap<String, u64>,
    selected_sessions: BTreeSet<String>,
    rows_total: u64,
    duplicates: u64,
    oversize_full_scan_tables: Vec<String>,
    /// Filter is in force but these child tables have no `session_id` column,
    /// so they were captured in full (N3).
    unfiltered_tables: Vec<String>,
}

/// Observe one database against the previous observation state. Must run
/// inside one read transaction; `before` content is fetched from the evidence
/// afterwards (see [`fill_before_from_evidence`]).
#[allow(clippy::too_many_arguments)]
fn observe_one_table(
    profile: &SqliteCaptureProfile,
    conn: &Connection,
    table: &str,
    declared: &SchemaTable,
    previous: &ObservationFile,
    filter_active: bool,
    session_since_ms: Option<i64>,
    selected: &BTreeSet<String>,
    // Apply session-child scoping (filter active, table is a child with its
    // session link column present). Children without the column are captured
    // in full and reported as `unfilteredTables` (N3).
    child_scoped: bool,
    // Newly selected sessions whose pre-existing child rows must be backfilled
    // in this observation (N2; children only).
    backfill_sessions: &BTreeSet<String>,
    duplicates: &mut DuplicateTracker,
    oversize: &mut Vec<String>,
) -> Result<(TableState, Vec<Change>, u64), SqliteHealthError> {
    let previous_state = previous.tables.get(table);
    let watermark = watermark_column_in(profile, table, &declared.columns);
    let is_session_table = profile.is_session_table(table);
    let session_link = profile.child_session_column(table).unwrap_or("session_id");
    let mut keys = BTreeSet::new();
    let mut changes_table: Vec<Change> = Vec::new();
    // §A: rows materialized for this table (one counter per table per run, not
    // one per row).
    let mut rows_scanned = 0u64;

    if let Some(column) = &watermark {
        let (previous_value, previous_key, watermark_known) = previous_state
            .filter(|state| state.watermark_column == *column)
            .map(|state| {
                (
                    state.watermark_value,
                    state.watermark_key.clone(),
                    state.watermark_known,
                )
            })
            .unwrap_or((i64::MIN, String::new(), false));
        // The previous key set is only trusted after a full observation or
        // baseline export; right after a crash-adopted baseline it is empty
        // and must not mark already-archived rows as unseen (N1 guard).
        let previous_keys_known = previous_state
            .map(|state| state.primary_keys_known)
            .unwrap_or(false);
        // Candidate read: rows at or above the watermark cross into the
        // collector; everything at or below is already archived (declared
        // blind window). No index is consulted or created (the source is
        // read-only); the predicate only bounds what is materialized.
        let mut sql = if declared.rowid {
            format!("SELECT rowid, * FROM {}", quote_ident(table))
        } else {
            format!("SELECT * FROM {}", quote_ident(table))
        };
        let mut params: Vec<i64> = Vec::new();
        let lower = if is_session_table
            && column == profile.session_time_column
            && filter_active
            && watermark_known
        {
            previous_value.max(session_since_ms.unwrap_or(i64::MIN))
        } else if watermark_known {
            previous_value
        } else {
            i64::MIN
        };
        if lower != i64::MIN {
            sql += &format!(" WHERE {} >= ?1", quote_ident(column));
            params.push(lower);
        }
        let mut statement = conn.prepare(&sql).map_err(map_sqlite_error)?;
        let mut rows = statement
            .query(rusqlite::params_from_iter(params.iter()))
            .map_err(map_sqlite_error)?;
        let offset = if declared.rowid { 1 } else { 0 };
        let mut watermark_max: Option<(i64, String)> = None;
        while let Some(row) = rows.next().map_err(map_sqlite_error)? {
            rows_scanned += 1;
            let object = row_object(row, declared, offset)?;
            let raw_key = row_key(row, declared, &object);
            let key = disambiguate_key(
                keys.contains(&raw_key),
                rowid_value(row, declared),
                keys.len(),
                raw_key,
            );
            // Scope filters first: out-of-scope rows are not tracked and do
            // not advance the watermark.
            if child_scoped {
                let session_id = object.get(session_link).and_then(Value::as_str);
                match session_id {
                    Some(id) if selected.contains(id) => {}
                    _ => continue,
                }
            }
            if is_session_table && filter_active {
                let updated = object
                    .get(profile.session_time_column)
                    .and_then(Value::as_i64)
                    .unwrap_or(i64::MAX);
                if updated < session_since_ms.unwrap_or(i64::MIN) {
                    continue;
                }
            }
            if let Some(value) = object.get(column).and_then(Value::as_i64) {
                let candidate = (value, key.clone());
                if watermark_max.as_ref().is_none_or(|current| candidate > *current) {
                    watermark_max = Some(candidate);
                }
            }
            // Candidate test: strictly past the (value, key) watermark — plus
            // every row sharing the watermark value whose key the previous
            // observation never saw (N1: an insert after the observation with
            // a smaller key is a new row, not a blind spot).
            let value_now = object.get(column).and_then(Value::as_i64);
            let unseen_at_same_value = previous_keys_known
                && !previous_state
                    .map(|state| state.primary_keys.contains(&key))
                    .unwrap_or(false);
            let is_candidate = !watermark_known
                || match value_now {
                    Some(value) if value == previous_value => {
                        key > previous_key || unseen_at_same_value
                    }
                    Some(value) => value > previous_value,
                    // Non-integer watermark value: re-queue; the archived
                    // content comparison drops it when nothing changed.
                    None => true,
                };
            if is_candidate {
                let op = if previous_state
                    .map(|state| state.primary_keys.contains(&key))
                    .unwrap_or(false)
                {
                    "update"
                } else {
                    "insert"
                };
                duplicates.observe(table, &object);
                changes_table.push(Change {
                    table: table.to_string(),
                    key: key.clone(),
                    op,
                    before: None,
                    after: Some(Value::Object(object.clone())),
                });
            }
            keys.insert(key);
        }
        let (new_value, new_key) = watermark_max
            .clone()
            .or(previous_state.map(|state| (state.watermark_value, state.watermark_key.clone())))
            .unwrap_or((0, String::new()));
        keys.extend_backfill(
            profile,
            conn,
            table,
            declared,
            backfill_sessions,
            &mut changes_table,
            previous_state,
            duplicates,
        )?;
        let state = TableState {
            columns: declared.columns.clone(),
            primary_key: declared.primary_key.clone(),
            rowid: declared.rowid,
            watermark_column: column.clone(),
            watermark_value: new_value,
            watermark_key: new_key,
            watermark_known: true,
            primary_keys_known: true,
            primary_keys: keys.clone(),
            row_hashes: None,
        };
        record_table_scanned(table, rows_scanned);
        return Ok((state, changes_table, keys.len() as u64));
    }

    // No watermark column: full read + row-hash compare (scoped when the
    // filter is in force and the table is a session child with its session
    // link column).
    let rows = read_all_rows(
        profile,
        conn,
        table,
        declared,
        (is_session_table && filter_active).then(|| session_since_ms.unwrap_or(0)),
        &selected,
    )?;
    let previous_hashes = previous_state.and_then(|state| state.row_hashes.clone());
    for (key, row) in &rows {
        let object = row.as_object().cloned().unwrap_or_default();
        let hash = row_hash(&object);
        keys.insert(key.clone());
        if let Some(hashes) = previous_hashes.as_ref() {
            match hashes.get(key) {
                None => {
                    duplicates.observe(table, &object);
                    changes_table.push(Change {
                        table: table.to_string(),
                        key: key.clone(),
                        op: "insert",
                        before: None,
                        after: Some(Value::Object(object.clone())),
                    });
                }
                Some(previous_hash) if previous_hash != &hash => {
                    duplicates.observe(table, &object);
                    changes_table.push(Change {
                        table: table.to_string(),
                        key: key.clone(),
                        op: "update",
                        before: None,
                        after: Some(Value::Object(object.clone())),
                    });
                }
                Some(_) => {}
            }
        }
    }
    if let Some(hashes) = previous_hashes.as_ref() {
        for key in hashes.keys() {
            if !rows.contains_key(key) {
                changes_table.push(Change {
                    table: table.to_string(),
                    key: key.clone(),
                    op: "delete",
                    before: None,
                    after: None,
                });
            }
        }
    }
    if rows.len() as u64 > FULL_SCAN_HASH_MAX_ROWS {
        oversize.push(table.to_string());
    }
    let state = TableState {
        columns: declared.columns.clone(),
        primary_key: declared.primary_key.clone(),
        rowid: declared.rowid,
        watermark_column: String::new(),
        watermark_value: 0,
        watermark_key: String::new(),
        watermark_known: true,
        primary_keys_known: true,
        primary_keys: keys.clone(),
        row_hashes: Some(
            rows.iter()
                .map(|(key, row)| (key.clone(), row_hash_value(row)))
                .collect(),
        ),
    };
    record_table_scanned(table, rows.len() as u64);
    Ok((state, changes_table, keys.len() as u64))
}

/// §A: rows materialized while observing one table (counter, per table).
fn record_table_scanned(table: &str, rows: u64) {
    if cost::is_recording() {
        cost::counter(&format!("watermark_rows.{table}"), rows);
    }
}

/// N2 backfill: read every child row of the newly selected sessions (their
/// historical rows sit below the child watermark and are invisible to the
/// watermark read), recording the unseen ones as inserts.
#[allow(clippy::too_many_arguments)]
trait BackfillExt {
    fn extend_backfill(
        &mut self,
        profile: &SqliteCaptureProfile,
        conn: &Connection,
        table: &str,
        declared: &SchemaTable,
        sessions: &BTreeSet<String>,
        changes: &mut Vec<Change>,
        previous_state: Option<&TableState>,
        duplicates: &mut DuplicateTracker,
    ) -> Result<(), SqliteHealthError>;
}

impl BackfillExt for BTreeSet<String> {
    fn extend_backfill(
        &mut self,
        profile: &SqliteCaptureProfile,
        conn: &Connection,
        table: &str,
        declared: &SchemaTable,
        sessions: &BTreeSet<String>,
        changes: &mut Vec<Change>,
        previous_state: Option<&TableState>,
        duplicates: &mut DuplicateTracker,
    ) -> Result<(), SqliteHealthError> {
        let session_link = profile.child_session_column(table);
        if sessions.is_empty()
            || !session_link
                .map(|column| declared.columns.iter().any(|c| c == column))
                .unwrap_or(false)
        {
            return Ok(());
        }
        let link = session_link.unwrap_or_default();
        // R1: a backfill session's rows may straddle the child watermark — the
        // rows at/above it were already collected (and keyed) by the regular
        // watermark read of this same observation. Re-running them through key
        // disambiguation minted `#rowid:N` phantom keys and duplicate inserts,
        // so any key already in `self` is skipped outright. Rows whose key is
        // in the previous observation's key set are already archived and
        // skipped as well (that is what makes backfilling the children of an
        // already-selected but re-touched session safe). Both skips need a
        // trustworthy previous key set: right after a crash-adopted baseline
        // the sets are empty and untrusted, so backfilling is skipped entirely
        // (declared blind window; the verification pass after an adopt heals
        // it — see README).
        let previous_keys_known = previous_state
            .map(|state| state.primary_keys_known)
            .unwrap_or(false);
        static EMPTY_KEYS: std::sync::LazyLock<BTreeSet<String>> =
            std::sync::LazyLock::new(BTreeSet::new);
        let previous_keys: &BTreeSet<String> = previous_state
            .map(|state| &state.primary_keys)
            .unwrap_or(&EMPTY_KEYS);
        let _ = previous_keys_known; // enforced by the caller gating `sessions`
        let mut sql = if declared.rowid {
            format!("SELECT rowid, * FROM {}", quote_ident(table))
        } else {
            format!("SELECT * FROM {}", quote_ident(table))
        };
        sql += &format!(
            " WHERE {} IN (SELECT id FROM am_backfill_sessions)",
            quote_ident(link)
        );
        sql += &order_suffix(declared);
        let mut statement = conn.prepare(&sql).map_err(map_sqlite_error)?;
        let mut rows = statement.query([]).map_err(map_sqlite_error)?;
        let offset = if declared.rowid { 1 } else { 0 };
        while let Some(row) = rows.next().map_err(map_sqlite_error)? {
            let object = row_object(row, declared, offset)?;
            let raw_key = row_key(row, declared, &object);
            if self.contains(&raw_key) || previous_keys.contains(&raw_key) {
                // Already collected by this observation's regular read, or
                // already archived before it (R1).
                continue;
            }
            // Untracked key: this row was never archived (out-of-scope
            // sessions are not tracked), so it is an insert. Within-backfill
            // duplicate primary keys beyond the first are skipped (pathological
            // shape; the plain key now exists in `self`).
            duplicates.observe(table, &object);
            changes.push(Change {
                table: table.to_string(),
                key: raw_key.clone(),
                op: "insert",
                before: None,
                after: Some(Value::Object(object.clone())),
            });
            self.insert(raw_key);
        }
        Ok(())
    }
}

/// Observe one database against the previous observation state. Must run
/// inside one read transaction; `before` content is fetched from the evidence
/// afterwards (see [`fill_before_from_evidence`]).
fn observe_changes(
    profile: &SqliteCaptureProfile,
    conn: &Connection,
    previous: &ObservationFile,
    schema: &BTreeMap<String, SchemaTable>,
    session_since_ms: Option<i64>,
) -> Result<PendingObservation, SqliteHealthError> {
    let whitelisted = whitelist_schema(profile, schema);
    let filter_active = session_since_ms.is_some()
        && profile
            .session_tables
            .iter()
            .any(|(table, _)| whitelisted.contains_key(*table));
    let mut selected = previous.selected_sessions.clone();

    let mut tables = BTreeMap::new();
    let mut changes: Vec<Change> = Vec::new();
    let mut rows_total = 0u64;
    let mut duplicates = DuplicateTracker::default();
    let mut oversize = Vec::new();
    let mut unfiltered_tables = Vec::new();

    let child_scoped = |table: &str, declared: &SchemaTable| -> bool {
        filter_active
            && profile
                .child_session_column(table)
                .map(|column| declared.columns.iter().any(|c| c == column))
                .unwrap_or(false)
    };

    // Phase 1: session tables observe first and update the selection scope.
    for (table, _) in profile.session_tables.iter().copied() {
        let Some(declared) = whitelisted.get(table) else {
            continue;
        };
        let (state, changes_table, key_count) = {
            let _watermark = cost::phase("watermark_read");
            let _table = cost::phase(format!("watermark_read.tables.{table}"));
            observe_one_table(
                profile,
                conn,
                table,
                declared,
                previous,
                filter_active,
                session_since_ms,
                &selected,
                false,
                &BTreeSet::new(),
                &mut duplicates,
                &mut oversize,
            )?
        };
        changes.extend(changes_table);
        if filter_active
            && declared
                .columns
                .iter()
                .any(|c| c == profile.session_time_column)
        {
            let session_ids = {
                let _filter = cost::phase("session_filter_read");
                scan_session_ids(profile, conn, table, declared, session_since_ms.unwrap_or(0))?
            };
            selected.extend(session_ids);
        }
        // Watermark tables detect deletes by primary-key set comparison (only
        // key columns are read); see the child phase below.
        if watermark_column_in(profile, table, &declared.columns).is_some() {
            let mut state = state;
            let previous_state = previous.tables.get(table);
            let watermark_known = previous_state
                .filter(|s| s.watermark_column == state.watermark_column)
                .map(|s| s.watermark_known)
                .unwrap_or(false);
            let previous_keys: BTreeSet<String> = previous_state
                .map(|s| s.primary_keys.clone())
                .unwrap_or_default();
            let live_keys = {
                let _compare = cost::phase("key_compare");
                let live_keys = if filter_active {
                    scan_session_keys(profile, conn, table, declared, session_since_ms.unwrap_or(0))?
                } else {
                    scan_table_keys(profile, conn, table, declared)?
                };
                if watermark_known {
                    for key in &previous_keys {
                        if !live_keys.contains(key) {
                            changes.push(Change {
                                table: table.to_string(),
                                key: key.clone(),
                                op: "delete",
                                before: None,
                                after: None,
                            });
                        }
                    }
                }
                live_keys
            };
            let mut keys = state.primary_keys.clone();
            for key in live_keys {
                keys.insert(key);
            }
            state.primary_keys = keys;
            tables.insert(table.to_string(), state);
            rows_total += key_count;
            continue;
        }
        tables.insert(table.to_string(), state);
        rows_total += key_count;
    }

    // N2 (+R1 hardening): sessions whose pre-existing child rows must be
    // backfilled in phase 2 — those that ENTERED the scope with this
    // observation, plus those whose session row was an update candidate here
    // (a re-created same-id session or any re-touched session can carry child
    // rows below the child watermark). Rows already archived (previous key
    // set) or already collected by this observation's regular read are skipped
    // by the backfill itself; both skips need a trustworthy previous key set,
    // so after a crash-adopted baseline backfilling is skipped entirely and
    // the automatic post-adopt verification heals the window (N-R4).
    let mut backfill: BTreeSet<String> = selected
        .difference(&previous.selected_sessions)
        .cloned()
        .collect();
    for change in &changes {
        if change.op == "update" && profile.is_session_table(&change.table) {
            // The session key column of this session table identifies the
            // session whose children may need the backfill.
            let key_column = profile
                .session_key_column(&change.table)
                .unwrap_or("id");
            if let Some(id) = change
                .after
                .as_ref()
                .and_then(|after| after.get(key_column))
                .and_then(Value::as_str)
            {
                if selected.contains(id) {
                    backfill.insert(id.to_string());
                }
            }
        }
    }
    let backfill_trusted = previous
        .tables
        .values()
        .next()
        .map(|state| state.primary_keys_known)
        .unwrap_or(false);
    if !backfill_trusted {
        backfill.clear();
    }
    if filter_active && !backfill.is_empty() {
        conn.execute_batch(
            "CREATE TEMP TABLE am_backfill_sessions (id TEXT PRIMARY KEY)",
        )
        .map_err(map_sqlite_error)?;
        let mut statement = conn
            .prepare("INSERT INTO am_backfill_sessions (id) VALUES (?1)")
            .map_err(map_sqlite_error)?;
        for id in &backfill {
            statement.execute([id]).map_err(map_sqlite_error)?;
        }
    } else {
        backfill.clear();
    }

    // Phase 2: children and non-session tables.
    for table in profile.tables.iter().copied() {
        if profile.is_session_table(table) {
            continue;
        }
        let Some(declared) = whitelisted.get(table) else {
            continue;
        };
        let scoped = child_scoped(table, declared);
        if filter_active
            && profile.child_session_column(table).is_some()
            && !scoped
            && !unfiltered_tables.contains(&table.to_string())
        {
            // N3: the filter is in force but this child table has no session
            // link column — captured in full and named in the report.
            unfiltered_tables.push(table.to_string());
        }
        let (state, changes_table, key_count) = {
            let _watermark = cost::phase("watermark_read");
            let _table = cost::phase(format!("watermark_read.tables.{table}"));
            observe_one_table(
                profile,
                conn,
                table,
                declared,
                previous,
                filter_active,
                session_since_ms,
                &selected,
                scoped,
                &backfill,
                &mut duplicates,
                &mut oversize,
            )?
        };
        changes.extend(changes_table);
        // Watermark tables detect deletes by primary-key set comparison (only
        // key columns are read). Hash tables already found their deletes by
        // hash-map comparison. Delete detection needs a trustworthy previous
        // key set: right after a crash-adopted baseline the sets start empty,
        // which is delete-safe (nothing can be reported as deleted).
        if watermark_column_in(profile, table, &declared.columns).is_some() {
            let mut state = state;
            let previous_state = previous.tables.get(table);
            let watermark_known = previous_state
                .filter(|s| s.watermark_column == state.watermark_column)
                .map(|s| s.watermark_known)
                .unwrap_or(false);
            let previous_keys: BTreeSet<String> = previous_state
                .map(|s| s.primary_keys.clone())
                .unwrap_or_default();
            let live_keys = {
                let _compare = cost::phase("key_compare");
                let live_keys = if scoped {
                    let _filter = cost::phase("session_filter_read");
                    scan_child_keys(profile, conn, table, declared, &selected)?
                } else {
                    scan_table_keys(profile, conn, table, declared)?
                };
                if watermark_known {
                    for key in &previous_keys {
                        if !live_keys.contains(key) {
                            changes.push(Change {
                                table: table.to_string(),
                                key: key.clone(),
                                op: "delete",
                                before: None,
                                after: None,
                            });
                        }
                    }
                }
                live_keys
            };
            let mut keys = state.primary_keys.clone();
            for key in live_keys {
                keys.insert(key);
            }
            state.primary_keys = keys;
            tables.insert(table.to_string(), state);
            rows_total += key_count;
            continue;
        }
        tables.insert(table.to_string(), state);
        rows_total += key_count;
    }

    if !backfill.is_empty() {
        let _ = conn.execute_batch("DROP TABLE IF EXISTS am_backfill_sessions");
    }

    let excluded = {
        let _count = cost::phase("excluded_table_count");
        count_excluded_tables(profile, conn, schema)?
    };
    Ok(PendingObservation {
        changes,
        tables,
        excluded,
        selected_sessions: selected,
        rows_total,
        duplicates: duplicates.count(),
        oversize_full_scan_tables: oversize,
        unfiltered_tables,
    })
}

/// Fill `before` images from the archived evidence (baseline chunks + delta
/// lines).
///
/// # One read per object (implementation-005)
///
/// Every object this pass needs is fetched, verified and decompressed **once**,
/// and its lines are indexed by key for all target tables/keys of the run:
///
/// 1. the touched keys are collected per table first;
/// 2. baseline chunks are streamed once each, keeping only the target keys of
///    their own table;
/// 3. delta batches are decompressed once each, in capture order, and their
///    lines are applied to every table that has a target key in that batch's
///    recorded range.
///
/// The previous per-table loop re-read a multi-table delta batch once per table
/// (`batch x tables` object reads for data read once), which on the production
/// evidence store multiplied out because each object read also re-parsed the
/// whole manifest set. Both amplifiers are gone: the read count is bounded by
/// the objects actually hit, and access is decided against the run's shared
/// manifest snapshot ([`crate::archive::ManifestSnapshot`]).
///
/// Cost is still bounded by what changed, not by the evidence size: only
/// baseline chunks whose `[min_key, max_key]` range contains a target key and
/// only delta batches whose recorded range for some target table contains a
/// target key are read, and legacy chunks/batches without a recorded range are
/// still read conservatively. Drops no-op updates (content identical to the
/// archived content) — a watermark-column touch without a content change is not
/// a row change.
fn fill_before_from_evidence(
    archive: &Archive,
    snapshot: &crate::archive::ManifestSnapshot,
    domain: &str,
    baseline: &SqliteBaseline,
    batch_metas: &[SqliteDeltaBatchMeta],
    changes: &mut Vec<Change>,
    missing_before: &mut u64,
) -> Result<(), ArchiveError> {
    // §A counters for the evidence read path: objects read (the read count the
    // one-read-per-object rule bounds), decompressed baseline blocks and bytes,
    // delta batches and bytes.
    let mut baseline_chunks = 0u64;
    let mut baseline_bytes = 0u64;
    let mut delta_batches = 0u64;
    let mut delta_bytes = 0u64;
    let mut object_reads = 0u64;
    let mut distinct_objects: BTreeSet<String> = BTreeSet::new();
    let fetch = |sha256: &str| -> Result<Vec<u8>, ArchiveError> {
        // Fail-closed against the run's snapshot before a single byte is read.
        snapshot.check_object(sha256, domain)?;
        archive.read_object_bytes(sha256)
    };
    let mut by_table: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (index, change) in changes.iter().enumerate() {
        if change.op == "insert" {
            continue;
        }
        by_table
            .entry(change.table.clone())
            .or_default()
            .push(index);
    }
    if by_table.is_empty() {
        return Ok(());
    }
    let targets: BTreeMap<String, BTreeSet<String>> = by_table
        .iter()
        .map(|(table, indexes)| {
            (
                table.clone(),
                indexes
                    .iter()
                    .map(|index| changes[*index].key.clone())
                    .collect(),
            )
        })
        .collect();
    let mut archived: BTreeMap<String, BTreeMap<String, Value>> = BTreeMap::new();
    let range_hit = |meta: &SqliteDeltaBatchMeta, table: &str, targets: &BTreeSet<String>| -> bool {
        match meta.table_ranges.get(table) {
            // N-R3: a batch without recorded ranges (legacy manifest) may
            // contain the target key and is always read.
            None => meta.table_ranges.is_empty(),
            Some((min, max)) => targets
                .iter()
                .any(|key| key.as_str() >= min.as_str() && key.as_str() <= max.as_str()),
        }
    };

    // (1) Baseline chunks: once each, streamed, restricted to the target keys of
    // their own table.
    for chunk in &baseline.chunks {
        let Some(table_targets) = targets.get(&chunk.table) else {
            continue;
        };
        let range_hit = (chunk.min_key.is_empty() && chunk.max_key.is_empty())
            || table_targets.iter().any(|key| {
                key.as_str() >= chunk.min_key.as_str() && key.as_str() <= chunk.max_key.as_str()
            });
        if !range_hit {
            continue;
        }
        object_reads += 1;
        distinct_objects.insert(chunk.sha256.clone());
        let mut stream = ChunkLineStream::open_with(&fetch, chunk)?;
        while let Some((key, row)) = stream.next_row(chunk)? {
            if table_targets.contains(key.as_str()) {
                archived
                    .entry(chunk.table.clone())
                    .or_default()
                    .insert(key, row);
            }
        }
        baseline_chunks += 1;
        baseline_bytes = baseline_bytes.saturating_add(stream.bytes_seen);
    }

    // (2) Delta lines of this generation, in capture order: one read per batch,
    // applied to every table whose targets intersect the batch.
    for meta in batch_metas {
        if meta.baseline_generation != baseline.baseline_generation
            || meta.capture_seq <= baseline.capture_seq
        {
            continue;
        }
        let hit_tables: Vec<&String> = targets
            .iter()
            .filter(|(table, table_targets)| range_hit(meta, table, table_targets))
            .map(|(table, _)| table)
            .collect();
        if hit_tables.is_empty() {
            continue;
        }
        object_reads += 1;
        let (lines, raw_bytes) = read_delta_lines_fetch_measured(&fetch, meta)?;
        distinct_objects.insert(meta.delta_object.clone());
        delta_batches += 1;
        delta_bytes = delta_bytes.saturating_add(raw_bytes);
        for line in lines {
            let Some(table_targets) = targets.get(&line.table) else {
                continue;
            };
            if !table_targets.contains(line.key.as_str()) {
                continue;
            }
            let slot = archived.entry(line.table.clone()).or_default();
            match line.op.as_str() {
                "delete" => {
                    slot.remove(&line.key);
                }
                _ => {
                    if let Some(after) = &line.after {
                        slot.insert(line.key.clone(), after.clone());
                    }
                }
            }
        }
    }

    // (3) Apply the resolved images: unchanged content is a no-op touch.
    for (table, indexes) in &by_table {
        let table_archived = archived.get(table);
        for index in indexes {
            let change = &mut changes[*index];
            match table_archived.and_then(|slot| slot.get(&change.key)) {
                Some(before) => {
                    if change.op == "update" && Some(before) == change.after.as_ref() {
                        // Touch without content change: not a row change.
                        change.op = "noop";
                    } else {
                        change.before = Some(before.clone());
                    }
                }
                None => {
                    // No archived image to cite: recorded with before=null.
                    *missing_before += 1;
                }
            }
        }
    }
    changes.retain(|change| change.op != "noop");
    cost::counter("evidence_read.baseline_chunks", baseline_chunks);
    cost::counter("evidence_read.baseline_bytes", baseline_bytes);
    cost::counter("evidence_read.delta_batches", delta_batches);
    cost::counter("evidence_read.delta_bytes", delta_bytes);
    cost::counter("evidence_read.object_reads", object_reads);
    cost::counter(
        "evidence_read.distinct_objects",
        distinct_objects.len() as u64,
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Plan
// ---------------------------------------------------------------------------

enum Plan {
    Baseline(&'static str),
    Delta,
}

#[allow(clippy::too_many_arguments)]
fn decide_plan(
    profile: &SqliteCaptureProfile,
    archive: &Archive,
    source_key: &str,
    previous: Option<&SqliteState>,
    previous_observation: Option<&ObservationFile>,
    schema_fingerprint_now: &str,
    schema: &BTreeMap<String, SchemaTable>,
    options: CaptureOptions,
) -> Result<Plan, ArchiveError> {
    if options.rebaseline {
        return Ok(Plan::Baseline("rebaseline_requested"));
    }
    let Some(previous) = previous else {
        return Ok(Plan::Baseline("initial"));
    };
    if previous.baseline_generation == 0 || previous.baseline_digest.is_empty() {
        return Ok(Plan::Baseline("initial"));
    }
    if previous.known_columns.is_empty() || previous.schema_fingerprint.is_empty() {
        // State written by an older/partial version cannot be diffed safely.
        return Ok(Plan::Baseline("unclassifiable_state"));
    }
    if previous_observation.is_none() {
        return Ok(Plan::Baseline("observation_state_missing"));
    }
    let observation = previous_observation.expect("checked above");
    // Every whitelisted table must have collector state; a table the state
    // never observed cannot be watermarked.
    for name in whitelist_schema(profile, schema).keys() {
        if !observation.tables.contains_key(name) {
            return Ok(Plan::Baseline("unclassifiable_state"));
        }
    }
    // The baseline chunk list comes from the committed manifests; when the
    // current baseline's chunks are gone, incremental capture cannot continue.
    let current_baseline = sqlite_baselines(archive, source_key)?
        .into_iter()
        .filter(|baseline| baseline.baseline_digest == previous.baseline_digest)
        .max_by_key(|baseline| baseline.capture_seq);
    match current_baseline {
        Some(baseline) if !baseline_objects_present(archive, &baseline.chunks) => {
            return Ok(Plan::Baseline("baseline_unavailable"));
        }
        None => return Ok(Plan::Baseline("baseline_unavailable")),
        Some(_) => {}
    }
    if previous.schema_fingerprint != schema_fingerprint_now {
        if unknown_whitelist_schema_in(profile, &previous.known_columns, schema) {
            return Ok(Plan::Baseline("unknown_table_or_column"));
        }
        return Ok(Plan::Baseline("schema_change"));
    }
    if previous.session_since_ms != options.session_since_ms {
        return Ok(Plan::Baseline("filter_changed"));
    }
    if previous.baseline_bytes > 0
        && previous.delta_chain_bytes * 100 > previous.baseline_bytes * BASELINE_CHAIN_BYTE_PERCENT
    {
        return Ok(Plan::Baseline("delta_chain_threshold"));
    }
    Ok(Plan::Delta)
}

// ---------------------------------------------------------------------------
// Report surface
// ---------------------------------------------------------------------------

/// `{table, row_count}` fact for one non-whitelisted table.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SqliteExcludedTable {
    pub table: String,
    pub row_count: u64,
}

#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SqliteCaptureInfo {
    /// `baseline` | `delta` | `unchanged`
    pub mode: String,
    pub capture_seq: u64,
    pub baseline_generation: u64,
    pub schema_fingerprint: String,
    /// Why a baseline was taken (empty for delta units).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub snapshot_trigger: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub baseline_digest: String,
    #[serde(default)]
    pub baseline_bytes: u64,
    #[serde(default)]
    pub baseline_chunks: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delta_object: Option<String>,
    #[serde(default)]
    pub delta_rows: u64,
    #[serde(default)]
    pub rows_inserted: u64,
    #[serde(default)]
    pub rows_updated: u64,
    #[serde(default)]
    pub rows_deleted: u64,
    #[serde(default)]
    pub rows_discovered: u64,
    /// Distinct payloads seen identically in more than one table (baseline:
    /// all rows; delta: changed rows).
    #[serde(default)]
    pub duplicate_payload_rows: u64,
    /// Gap since the previous observation (the blind window).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observation_interval_ms: Option<i64>,
    #[serde(default)]
    pub delta_chain_length: u64,
    #[serde(default)]
    pub delta_chain_bytes: u64,
    #[serde(default)]
    pub wal_bytes: u64,
    /// Whitelisted tables observed at this observation point.
    #[serde(default)]
    pub schema_tables: Vec<String>,
    /// Non-whitelisted tables: row counts only (row content is never read).
    #[serde(default)]
    pub excluded_tables: Vec<SqliteExcludedTable>,
    /// Session filter in force for this run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_since_ms: Option<i64>,
    /// True when a verification pass ran in this capture.
    #[serde(default)]
    pub verify: bool,
    #[serde(default)]
    pub verify_mismatches: u64,
    /// Rows whose archived `before` image could not be resolved (recorded with
    /// `before=null`).
    #[serde(default)]
    pub missing_before_rows: u64,
    /// Whitelist tables without a watermark column that exceed the full-scan
    /// hash threshold (correct but unbounded cost; named, never silent).
    #[serde(default)]
    pub oversize_full_scan_tables: Vec<String>,
    /// With `--session-since` in force: child tables without a `session_id`
    /// column were captured in full and out of the session scope (N3).
    #[serde(default)]
    pub unfiltered_tables: Vec<String>,
    /// Verification ran these tables on the bounded path instead of the
    /// streaming merge (non-BINARY pk collation, or a merge-order misalignment
    /// detected at runtime; R2).
    #[serde(default)]
    pub verify_bounded_tables: Vec<String>,
    /// §E heavy-job gate verdict for the automatic full-table verification of
    /// this run (absent when verification was not due, or when an explicit
    /// `--verify` bypassed the gate).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verify_gate: Option<crate::gate::GateDecision>,
}

fn excluded_facts(excluded: &BTreeMap<String, u64>) -> Vec<SqliteExcludedTable> {
    excluded
        .iter()
        .map(|(table, row_count)| SqliteExcludedTable {
            table: table.clone(),
            row_count: *row_count,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Delta lines
// ---------------------------------------------------------------------------

/// One raw row-level delta line. The field set is fixed to observable facts:
/// capture sequence, observation time, table, primary key, operation, observed
/// before/after row content.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DeltaLine {
    pub capture_seq: u64,
    pub observed_at_ms: i64,
    pub table: String,
    pub key: String,
    /// `insert` | `update` | `delete` (by observed difference, not by a database
    /// journal).
    pub op: String,
    pub before: Option<Value>,
    pub after: Option<Value>,
}

impl DeltaLine {
    fn to_line_bytes(&self) -> Vec<u8> {
        let mut bytes = serde_json::to_vec(self).expect("delta line serializes");
        bytes.push(b'\n');
        bytes
    }
}

fn parse_delta_lines(raw: &[u8]) -> Result<Vec<DeltaLine>, ArchiveError> {
    let mut lines = Vec::new();
    for line in raw.split(|byte| *byte == b'\n') {
        if line.is_empty() {
            continue;
        }
        let parsed: DeltaLine = serde_json::from_slice(line)
            .map_err(|err| ArchiveError::Corrupt(format!("delta line is not JSON: {err}")))?;
        lines.push(parsed);
    }
    Ok(lines)
}

// ---------------------------------------------------------------------------
// Evidence projections (baselines, deltas, replay)
// ---------------------------------------------------------------------------

fn decompress_object(archive: &Archive, domain: &str, sha256: &str) -> Result<Vec<u8>, ArchiveError> {
    let compressed = archive.reader(domain).read_object(sha256)?;
    let raw = zstd::stream::decode_all(compressed.as_slice())
        .map_err(|err| ArchiveError::Integrity(format!("object {sha256} is not zstd: {err}")))?;
    note_decompressed(raw.len() as u64);
    Ok(raw)
}

/// The committed manifest snapshot of one SQLite capture, with the §A
/// `manifest_read` stage and its parse counters.
///
/// Every baseline / delta-batch / change-detection projection of one source
/// build shares this snapshot: the manifest set is parsed at most once per
/// change of the manifest directory listing, and a round with nothing new
/// parses nothing. The per-source filtering is applied by
/// [`sqlite_baselines_in`] / [`sqlite_delta_batch_metas_in`], which never read a
/// manifest body again.
fn sqlite_manifests(
    archive: &Archive,
    _source: &str,
) -> Result<Arc<crate::archive::ManifestSnapshot>, ArchiveError> {
    // §A: every baseline / delta-batch projection starts from the shared
    // manifest snapshot; its own stage separates that read from the evidence
    // decompression.
    let _phase = cost::phase("manifest_read");
    let parsed_before = crate::archive::manifest_parse_count();
    let manifests = archive.manifests()?;
    cost::counter("manifest_read.calls", 1);
    cost::counter(
        "manifest_read.parsed",
        crate::archive::manifest_parse_count().saturating_sub(parsed_before),
    );
    Ok(manifests)
}

/// One committed baseline generation, projected from a committed manifest.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SqliteBaseline {
    pub capture_id: String,
    pub capture_seq: u64,
    pub baseline_generation: u64,
    /// S4.5: capture profile of the committed unit (empty = pre-profile
    /// evidence = OpenCode).
    #[serde(default)]
    pub profile_id: String,
    pub observed_at_ms: i64,
    /// Digest over the ordered chunk list.
    pub baseline_digest: String,
    /// Total uncompressed export bytes.
    pub baseline_bytes: u64,
    pub compressed_bytes: u64,
    pub schema_fingerprint: String,
    pub snapshot_reason: String,
    pub observation_interval_ms: Option<i64>,
    pub rows_discovered: u64,
    pub chunks: Vec<ManifestSqliteChunk>,
    /// Row count per archived table.
    pub tables: BTreeMap<String, u64>,
    /// Declared column order per archived table.
    pub columns: BTreeMap<String, Vec<String>>,
    /// `CREATE TABLE` statement per archived table.
    pub ddl: BTreeMap<String, String>,
}

/// All committed baselines of one SQLite source, oldest first.
pub fn sqlite_baselines(
    archive: &Archive,
    source: &str,
) -> Result<Vec<SqliteBaseline>, ArchiveError> {
    Ok(sqlite_baselines_in(&sqlite_manifests(archive, source)?, source))
}

/// Baselines over pre-parsed manifests (batch readers parse the manifest
/// list once and slice it per source here).
pub fn sqlite_baselines_in(manifests: &[Manifest], source: &str) -> Vec<SqliteBaseline> {
    let mut out = Vec::new();
    for manifest in manifests.iter().filter(|manifest| {
        manifest.source.path == source
            && manifest
                .source
                .sqlite
                .as_ref()
                .map(|sqlite| sqlite.capture_seq > 0)
                .unwrap_or(false)
    }) {
        let Some(sqlite) = manifest.source.sqlite.as_ref() else {
            continue;
        };
        if sqlite.baseline_chunks.is_empty() {
            continue;
        }
        let mut tables = BTreeMap::new();
        for chunk in &sqlite.baseline_chunks {
            *tables.entry(chunk.table.clone()).or_insert(0u64) += chunk.rows;
        }
        out.push(SqliteBaseline {
            capture_id: manifest.capture_id.clone(),
            capture_seq: sqlite.capture_seq,
            baseline_generation: sqlite.baseline_generation,
            profile_id: sqlite.profile_id.clone(),
            observed_at_ms: sqlite.observed_at_ms,
            baseline_digest: sqlite.baseline_digest.clone(),
            baseline_bytes: sqlite.baseline_bytes,
            compressed_bytes: manifest
                .objects
                .iter()
                .map(|object| object.bytes)
                .sum::<u64>(),
            schema_fingerprint: sqlite.schema_fingerprint.clone(),
            snapshot_reason: sqlite.snapshot_reason.clone(),
            observation_interval_ms: sqlite.observation_interval_ms,
            rows_discovered: sqlite.rows_discovered,
            chunks: sqlite.baseline_chunks.clone(),
            tables,
            columns: sqlite.baseline_columns.clone(),
            ddl: sqlite.baseline_ddl.clone(),
        });
    }
    out.sort_by_key(|baseline| baseline.capture_seq);
    out
}

impl SqliteBaseline {
    /// The capture profile this baseline was taken under (missing field =
    /// pre-profile evidence = OpenCode).
    pub fn profile(&self) -> &'static SqliteCaptureProfile {
        sqlite_capture_profile_by_id(&self.profile_id).unwrap_or(OPENCODE_PROFILE)
    }
}

/// One committed delta batch, projected from a committed manifest.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SqliteDeltaBatch {
    pub capture_id: String,
    pub capture_seq: u64,
    pub baseline_generation: u64,
    pub observed_at_ms: i64,
    pub delta_object: String,
    pub compressed_bytes: u64,
    pub raw_bytes: u64,
    pub observation_interval_ms: Option<i64>,
    pub rows_inserted: u64,
    pub rows_updated: u64,
    pub rows_deleted: u64,
    pub verify_correction: bool,
    pub lines: Vec<DeltaLine>,
}

/// All committed delta batches of one SQLite source, oldest first.
///
/// Delta lines are content, so they are read through the caller's access domain
/// (fail-closed): a domain without access to a batch gets `AccessDenied`, never
/// a silently partial replay.
pub fn sqlite_deltas(
    archive: &Archive,
    source: &str,
    domain: &str,
) -> Result<Vec<SqliteDeltaBatch>, ArchiveError> {
    let mut out = Vec::new();
    for meta in sqlite_delta_batch_metas(archive, source)? {
        let raw_len;
        let lines = {
            let raw = decompress_object(archive, domain, &meta.delta_object)?;
            raw_len = raw.len() as u64;
            parse_delta_lines(&raw)
                .map_err(|err| ArchiveError::Corrupt(format!("delta {}: {err}", meta.delta_object)))?
        };
        out.push(SqliteDeltaBatch {
            capture_id: meta.capture_id.clone(),
            capture_seq: meta.capture_seq,
            baseline_generation: meta.baseline_generation,
            observed_at_ms: meta.observed_at_ms,
            delta_object: meta.delta_object.clone(),
            compressed_bytes: meta.compressed_bytes,
            raw_bytes: raw_len,
            observation_interval_ms: meta.observation_interval_ms,
            rows_inserted: meta.rows_inserted,
            rows_updated: meta.rows_updated,
            rows_deleted: meta.rows_deleted,
            verify_correction: meta.verify_correction,
            lines,
        });
    }
    out.sort_by_key(|batch| batch.capture_seq);
    Ok(out)
}

/// Manifest-only projection of one committed delta batch: identity, ordering
/// and per-table key ranges, with **no content read**. Incremental capture and
/// verification decide from this whether a batch's lines can matter
/// (`delta_table_ranges`) before decompressing anything.
#[derive(Debug, Clone)]
pub struct SqliteDeltaBatchMeta {
    pub capture_id: String,
    pub capture_seq: u64,
    pub baseline_generation: u64,
    pub observed_at_ms: i64,
    pub delta_object: String,
    pub compressed_bytes: u64,
    pub observation_interval_ms: Option<i64>,
    pub rows_inserted: u64,
    pub rows_updated: u64,
    pub rows_deleted: u64,
    pub verify_correction: bool,
    /// Per-table `(min key, max key)` over this batch's lines (empty when the
    /// manifest predates the field).
    pub table_ranges: BTreeMap<String, (String, String)>,
}

/// All committed delta batches of one source, oldest first, manifest-only.
pub fn sqlite_delta_batch_metas(
    archive: &Archive,
    source: &str,
) -> Result<Vec<SqliteDeltaBatchMeta>, ArchiveError> {
    Ok(sqlite_delta_batch_metas_in(
        &sqlite_manifests(archive, source)?,
        source,
    ))
}

/// Delta batch metas over pre-parsed manifests (batch readers).
pub fn sqlite_delta_batch_metas_in(
    manifests: &[Manifest],
    source: &str,
) -> Vec<SqliteDeltaBatchMeta> {
    let mut out = Vec::new();
    for manifest in manifests.iter().filter(|m| m.source.path == source) {
        let Some(sqlite) = manifest.source.sqlite.as_ref() else {
            continue;
        };
        let Some(object) = manifest
            .objects
            .iter()
            .find(|object| object.role == "sqlite_delta")
        else {
            continue;
        };
        out.push(SqliteDeltaBatchMeta {
            capture_id: manifest.capture_id.clone(),
            capture_seq: sqlite.capture_seq,
            baseline_generation: sqlite.baseline_generation,
            observed_at_ms: sqlite.observed_at_ms,
            delta_object: object.sha256.clone(),
            compressed_bytes: object.bytes,
            observation_interval_ms: sqlite.observation_interval_ms,
            rows_inserted: sqlite.rows_inserted,
            rows_updated: sqlite.rows_updated,
            rows_deleted: sqlite.rows_deleted,
            verify_correction: sqlite.verify_correction,
            table_ranges: sqlite.delta_table_ranges.clone(),
        });
    }
    out.sort_by_key(|meta| meta.capture_seq);
    out
}

/// Decompress and parse one delta batch (read-side metric counts the bytes).
pub fn read_delta_lines(
    archive: &Archive,
    domain: &str,
    meta: &SqliteDeltaBatchMeta,
) -> Result<Vec<DeltaLine>, ArchiveError> {
    let fetch = |sha256: &str| archive.reader(domain).read_object(sha256);
    read_delta_lines_fetch(&fetch, meta)
}

/// [`read_delta_lines`] plus the decompressed batch size, for the evidence-read
/// counters (§A).
pub fn read_delta_lines_measured(
    archive: &Archive,
    domain: &str,
    meta: &SqliteDeltaBatchMeta,
) -> Result<(Vec<DeltaLine>, u64), ArchiveError> {
    let fetch = |sha256: &str| archive.reader(domain).read_object(sha256);
    read_delta_lines_fetch_measured(&fetch, meta)
}

/// Read one delta batch over a caller-supplied object fetcher (batch
/// readers). Delta batches are app-level zstd - the fetch returns the
/// storage-decoded object and this decodes the batch payload.
pub fn read_delta_lines_fetch(
    fetch: ObjectFetch<'_>,
    meta: &SqliteDeltaBatchMeta,
) -> Result<Vec<DeltaLine>, ArchiveError> {
    read_delta_lines_fetch_measured(fetch, meta).map(|(lines, _)| lines)
}

/// [`read_delta_lines_fetch`] plus the decompressed batch size.
pub fn read_delta_lines_fetch_measured(
    fetch: ObjectFetch<'_>,
    meta: &SqliteDeltaBatchMeta,
) -> Result<(Vec<DeltaLine>, u64), ArchiveError> {
    let compressed = fetch(&meta.delta_object)?;
    let raw = zstd::stream::decode_all(compressed.as_slice()).map_err(|err| {
        ArchiveError::Integrity(format!("delta {} is not zstd: {err}", meta.delta_object))
    })?;
    note_decompressed(raw.len() as u64);
    let lines = parse_delta_lines(&raw)
        .map_err(|err| ArchiveError::Corrupt(format!("delta {}: {err}", meta.delta_object)))?;
    Ok((lines, raw.len() as u64))
}

/// Read one baseline generation's archived rows (chunk-verified, fail-closed).
/// Tables without chunks (empty at export time) are present with empty maps.
pub fn read_baseline_rows(
    archive: &Archive,
    baseline: &SqliteBaseline,
    domain: &str,
) -> Result<BTreeMap<String, BTreeMap<String, Value>>, ArchiveError> {
    let mut tables: BTreeMap<String, BTreeMap<String, Value>> = baseline
        .columns
        .keys()
        .map(|table| (table.clone(), BTreeMap::new()))
        .collect();
    for chunk in &baseline.chunks {
        let rows = read_chunk_rows(archive, domain, chunk)?;
        let slot = tables.entry(chunk.table.clone()).or_default();
        for line in rows {
            if let Some(object) = line.row.as_object() {
                slot.insert(line.key, Value::Object(object.clone()));
            }
        }
    }
    Ok(tables)
}

/// Replayed row state at one observation point.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SqliteReplay {
    pub source: String,
    pub requested_capture_seq: Option<u64>,
    /// Highest observation point included (never beyond the request).
    pub capture_seq: u64,
    pub baseline_generation: u64,
    pub baseline_capture_seq: u64,
    pub baseline_digest: String,
    pub deltas_applied: u64,
    pub schema_fingerprint: String,
    pub columns: BTreeMap<String, Vec<String>>,
    /// table -> row key -> observed row content.
    pub tables: BTreeMap<String, BTreeMap<String, Value>>,
}

/// Rebuild the row state of one observation point.
///
/// `upto_capture_seq = None` means the latest committed observation point. The
/// nearest baseline at or before the target is loaded from its chunk objects
/// and the deltas of that baseline generation are applied in capture order.
/// Intermediate states between observations are deliberately not invented.
pub fn replay_sqlite(
    archive: &Archive,
    source: &str,
    upto_capture_seq: Option<u64>,
    domain: &str,
) -> Result<SqliteReplay, ArchiveError> {
    replay_sqlite_tables(archive, source, &[], upto_capture_seq, domain)
}

/// [`replay_sqlite`] restricted to a table set: chunks and delta lines of other
/// tables are never decompressed or retained (stage-2 S5 archive fallback reads
/// only the session tables it needs). An empty `tables` slice means all tables.
///
/// The bookkeeping (baseline choice, capture order, delete semantics) is
/// exactly the unfiltered replay's; only the amount of content read differs.
pub fn replay_sqlite_tables(
    archive: &Archive,
    source: &str,
    tables: &[&str],
    upto_capture_seq: Option<u64>,
    domain: &str,
) -> Result<SqliteReplay, ArchiveError> {
    let target = upto_capture_seq.unwrap_or(u64::MAX);
    let baselines = sqlite_baselines(archive, source)?;
    let baseline = baselines
        .iter()
        .filter(|baseline| baseline.capture_seq <= target)
        .max_by_key(|baseline| baseline.capture_seq)
        .ok_or_else(|| {
            ArchiveError::NotFound(format!(
                "no SQLite baseline at or before capture_seq {target} for {source}"
            ))
        })?;
    let mut state = read_baseline_rows_filtered(archive, baseline, domain, tables)?;
    let mut capture_seq = baseline.capture_seq;
    let mut deltas_applied = 0u64;
    // Manifest-only metas first: batches of other generations or beyond the
    // target are never decompressed (replay cost follows the replayed chain).
    for meta in sqlite_delta_batch_metas(archive, source)? {
        if meta.baseline_generation != baseline.baseline_generation
            || meta.capture_seq <= baseline.capture_seq
            || meta.capture_seq > target
        {
            continue;
        }
        for line in read_delta_lines(archive, domain, &meta)? {
            if !tables.is_empty() && !tables.contains(&line.table.as_str()) {
                continue;
            }
            let table = state.entry(line.table.clone()).or_default();
            match line.op.as_str() {
                "delete" => {
                    table.remove(&line.key);
                }
                _ => {
                    if let Some(after) = line.after.clone() {
                        table.insert(line.key.clone(), after);
                    }
                }
            }
        }
        capture_seq = meta.capture_seq;
        deltas_applied += 1;
    }
    Ok(SqliteReplay {
        source: source.to_string(),
        requested_capture_seq: upto_capture_seq,
        capture_seq,
        baseline_generation: baseline.baseline_generation,
        baseline_capture_seq: baseline.capture_seq,
        baseline_digest: baseline.baseline_digest.clone(),
        deltas_applied,
        schema_fingerprint: baseline.schema_fingerprint.clone(),
        columns: baseline.columns.clone(),
        tables: state,
    })
}

/// [`read_baseline_rows`] restricted to a table set: chunks of other tables are
/// never decompressed. An empty `tables` slice means all tables.
pub fn read_baseline_rows_filtered(
    archive: &Archive,
    baseline: &SqliteBaseline,
    domain: &str,
    tables: &[&str],
) -> Result<BTreeMap<String, BTreeMap<String, Value>>, ArchiveError> {
    let mut state: BTreeMap<String, BTreeMap<String, Value>> = baseline
        .columns
        .keys()
        .map(|table| (table.clone(), BTreeMap::new()))
        .collect();
    for chunk in &baseline.chunks {
        if !tables.is_empty() && !tables.contains(&chunk.table.as_str()) {
            continue;
        }
        let rows = read_chunk_rows(archive, domain, chunk)?;
        let slot = state.entry(chunk.table.clone()).or_default();
        for line in rows {
            if let Some(object) = line.row.as_object() {
                slot.insert(line.key, Value::Object(object.clone()));
            }
        }
    }
    Ok(state)
}

/// Committed delta batches of one baseline generation above `after_capture_seq`
/// (oldest first, lines decompressed). Stage-2 S5 uses this to advance a cached
/// row mirror with content reads proportional to the new batches only.
pub fn sqlite_delta_batches_after(
    archive: &Archive,
    source: &str,
    baseline_generation: u64,
    after_capture_seq: u64,
    domain: &str,
) -> Result<Vec<SqliteDeltaBatch>, ArchiveError> {
    let mut out = Vec::new();
    for meta in sqlite_delta_batch_metas(archive, source)? {
        if meta.baseline_generation != baseline_generation || meta.capture_seq <= after_capture_seq
        {
            continue;
        }
        let lines = read_delta_lines(archive, domain, &meta)?;
        out.push(SqliteDeltaBatch {
            capture_id: meta.capture_id.clone(),
            capture_seq: meta.capture_seq,
            baseline_generation: meta.baseline_generation,
            observed_at_ms: meta.observed_at_ms,
            delta_object: meta.delta_object.clone(),
            compressed_bytes: meta.compressed_bytes,
            raw_bytes: 0,
            observation_interval_ms: meta.observation_interval_ms,
            rows_inserted: meta.rows_inserted,
            rows_updated: meta.rows_updated,
            rows_deleted: meta.rows_deleted,
            verify_correction: meta.verify_correction,
            lines,
        });
    }
    out.sort_by_key(|batch| batch.capture_seq);
    Ok(out)
}


// ---------------------------------------------------------------------------
// Capture
// ---------------------------------------------------------------------------

fn entry_for<'a>(
    state: &'a mut SourcesFile,
    source_key: &str,
    provider: &str,
) -> &'a mut SourceState {
    let entry = state
        .sources
        .entry(source_key.to_string())
        .or_insert_with(|| SourceState {
            provider: provider.to_string(),
            ..SourceState::default()
        });
    entry.provider = provider.to_string();
    entry
}

fn record_health_failure(
    archive: &Archive,
    profile: &SqliteCaptureProfile,
    state: &mut SourcesFile,
    source_key: &str,
    provider: &str,
    report: &mut CaptureReport,
    error: &SqliteHealthError,
    db_len: u64,
    wal_len: u64,
) -> Result<(), ArchiveError> {
    report.ok = false;
    report.status = "failed".to_string();
    report.change = error.code.clone();
    report.change_reason = Some(error.code.clone());
    report.errors = report.errors.saturating_add(1);
    report.error = Some(format!("{}: {}", error.code, error.message));
    report.anomalies = vec![format!("sqlite:{}", error.code)];
    report.file_len = db_len;
    let entry = entry_for(state, source_key, provider);
    entry.status = "failed".to_string();
    entry.change = error.code.clone();
    entry.last_error = Some(error.message.clone());
    entry.errors = entry.errors.saturating_add(1);
    entry.observed_len = Some(db_len);
    entry.incomplete_tail_bytes = 0;
    entry.anomalies = report.anomalies.clone();
    entry.sqlite = Some(match entry.sqlite.take() {
        Some(mut sqlite) => {
            sqlite.last_unavailable = Some(error.message.clone());
            sqlite.wal_bytes = wal_len;
            sqlite
        }
        None => SqliteState {
            profile_id: profile.profile_id.to_string(),
            last_unavailable: Some(error.message.clone()),
            wal_bytes: wal_len,
            last_observed_at_ms: now_ms(),
            ..SqliteState::default()
        },
    });
    archive.save_sources(state)
}

fn observation_interval(previous: Option<&SqliteState>, now: i64) -> Option<i64> {
    previous
        .filter(|state| state.last_observed_at_ms > 0)
        .map(|state| now.saturating_sub(state.last_observed_at_ms))
}

// ---------------------------------------------------------------------------
// Crash recovery: adopt a unit committed before its state was written
// ---------------------------------------------------------------------------

/// The committed manifest of `source` occupying `capture_seq`, if any.
///
/// State is always advanced *after* the manifest commit, so a manifest at
/// `previous.capture_seq + 1` while state still says `previous.capture_seq`
/// means the process died in that window. Re-running must adopt the committed
/// evidence instead of writing a second unit for the same change.
fn committed_unit_at(
    archive: &Archive,
    source: &str,
    capture_seq: u64,
) -> Result<Option<Manifest>, ArchiveError> {
    for manifest in sqlite_manifests(archive, source)?.iter() {
        if manifest.source.path == source
            && manifest
                .source
                .sqlite
                .as_ref()
                .map(|sqlite| sqlite.capture_seq == capture_seq)
                .unwrap_or(false)
        {
            return Ok(Some(manifest.clone()));
        }
    }
    Ok(None)
}

fn max_committed_generation(archive: &Archive, source: &str) -> Result<u64, ArchiveError> {
    Ok(sqlite_baselines(archive, source)?
        .iter()
        .map(|baseline| baseline.baseline_generation)
        .max()
        .unwrap_or(0))
}

fn max_committed_capture_seq(archive: &Archive, source: &str) -> Result<u64, ArchiveError> {
    Ok(sqlite_manifests(archive, source)?
        .iter()
        .filter(|manifest| manifest.source.path == source)
        .filter_map(|manifest| manifest.source.sqlite.as_ref().map(|s| s.capture_seq))
        .max()
        .unwrap_or(0))
}

/// True when the recomputed change set is exactly the committed delta line set
/// (per table/key/op; updates and inserts also compare their `after` content).
fn changes_match_lines(changes: &[Change], lines: &[DeltaLine]) -> bool {
    if changes.len() != lines.len() {
        return false;
    }
    let fingerprint = |table: &str, key: &str, op: &str, after: Option<&Value>| {
        (
            table.to_string(),
            key.to_string(),
            op.to_string(),
            after.map(|value| serde_json::to_string(value).unwrap_or_default()),
        )
    };
    let mut left: Vec<(String, String, String, Option<String>)> = changes
        .iter()
        .map(|change| {
            fingerprint(
                &change.table,
                &change.key,
                change.op,
                change.after.as_ref(),
            )
        })
        .collect();
    let mut right: Vec<(String, String, String, Option<String>)> = lines
        .iter()
        .map(|line| {
            fingerprint(&line.table, &line.key, &line.op, line.after.as_ref())
        })
        .collect();
    left.sort();
    right.sort();
    left == right
}

/// Adopt a committed-but-unrecorded unit. Returns `false` when the committed
/// delta no longer describes the live database (further writes happened), in
/// which case the caller captures normally under a fresh sequence number.
#[allow(clippy::too_many_arguments)]
fn adopt_committed_unit(
    archive: &Archive,
    profile: &SqliteCaptureProfile,
    source: &Path,
    source_key: &str,
    provider: &str,
    domain: &str,
    mut state: &mut SourcesFile,
    report: &mut CaptureReport,
    previous: Option<&SqliteState>,
    previous_observed: Option<&ObservationFile>,
    manifest: &Manifest,
    options: CaptureOptions,
    now: i64,
    db_len: u64,
    wal_len: u64,
    interval: Option<i64>,
) -> Result<bool, ArchiveError> {
    let Some(meta) = manifest.source.sqlite.clone() else {
        return Ok(false);
    };
    // S4.5: never adopt a unit committed under another capture profile — the
    // caller re-baselines instead (the observation-state guard reports the
    // switch).
    if !meta.profile_id.is_empty() && meta.profile_id != profile.profile_id {
        return Ok(false);
    }
    let role = manifest.objects.first().map(|object| object.role.clone());

    if role.as_deref() == Some("sqlite_baseline_chunk") {
        // Only a *successful* adoption is a crash recovery: if the checks below
        // cannot adopt, the run continues as a normal capture and must not
        // inherit a recovery anomaly (that would make `ok=true` and
        // `coverage.ok` disagree).
        if !baseline_objects_present(archive, &meta.baseline_chunks) {
            return Ok(false);
        }
        let recomputed = baseline_digest_of(&meta.baseline_chunks);
        if !meta.baseline_digest.is_empty() && recomputed != meta.baseline_digest {
            return Err(ArchiveError::Integrity(format!(
                "baseline digest {recomputed} does not match the manifest chunk list"
            )));
        }
        // Observation state rebuilt from manifest facts: watermarks are exact,
        // primary-key sets start empty (delete-safe) and full-scan hashes are
        // repopulated by the next observation without reporting changes.
        let mut observation = ObservationFile::new(
            profile,
            source_key,
            meta.capture_seq,
            now,
            meta.schema_fingerprint.clone(),
            previous.and_then(|p| p.session_since_ms),
        );
        for (table, columns) in &meta.baseline_columns {
            let mark = meta.baseline_watermarks.get(table);
            observation.tables.insert(
                table.clone(),
                TableState {
                    columns: columns.clone(),
                    primary_key: Vec::new(),
                    rowid: true,
                    watermark_column: mark
                        .map(|_| watermark_column_in(profile, table, columns).unwrap_or_default())
                        .unwrap_or_default(),
                    watermark_value: mark.map(|(value, _)| *value).unwrap_or(0),
                    watermark_key: mark.map(|(_, key)| key.clone()).unwrap_or_default(),
                    watermark_known: mark.is_some(),
                    primary_keys_known: false,
                    primary_keys: BTreeSet::new(),
                    row_hashes: None,
                },
            );
        }
        let rows = meta.rows_discovered;
        let observation_relative = observation_relative_path(source_key);
        save_observation(archive, &observation_relative, &observation)?;
        let entry = entry_for(&mut state, source_key, provider);
        entry.status = "degraded".to_string();
        entry.change = "recovered_after_crash".to_string();
        entry.generation = meta.baseline_generation;
        entry.discovered = rows;
        entry.captured = entry.captured.saturating_add(rows);
        entry.bytes = entry
            .bytes
            .saturating_add(manifest.objects.iter().map(|object| object.bytes).sum::<u64>());
        entry.errors = entry.errors.saturating_add(report.errors);
        entry.observed_len = Some(db_len);
        entry.incomplete_tail_bytes = 0;
        entry.last_error = None;
        entry.anomalies = report.anomalies.clone();
        entry.watermark = Some(Watermark {
            len: db_len,
            sha256: meta.baseline_digest.clone(),
            fingerprint: manifest.source.fingerprint.clone(),
            capture_id: manifest.capture_id.clone(),
            captured_at_ms: now,
            offset: db_len,
            line: 0,
            records: rows,
            generation: meta.baseline_generation,
            head_probe: String::new(),
            anchor_probe: String::new(),
            probe_len: 0,
        });
        entry.sqlite = Some(SqliteState {
            profile_id: profile.profile_id.to_string(),
            capture_seq: meta.capture_seq,
            baseline_generation: meta.baseline_generation,
            baseline_capture_seq: meta.capture_seq,
            baseline_object: String::new(),
            baseline_digest: meta.baseline_digest.clone(),
            baseline_bytes: meta.baseline_bytes,
            session_since_ms: observation.session_since_ms,
            // N-R4: an adopted baseline's observation window hides deletes and
            // low-key same-value inserts until the next verification, so the
            // next capture runs one automatically (last_verify_at_ms == 0).
            last_verify_at_ms: 0,
            verify_mismatches_total: previous.map(|p| p.verify_mismatches_total).unwrap_or(0),
            excluded_tables: BTreeMap::new(),
            unfiltered_tables: Vec::new(),
            schema_fingerprint: meta.schema_fingerprint.clone(),
            known_tables: meta.baseline_columns.keys().cloned().collect(),
            known_columns: meta.baseline_columns.clone(),
            delta_chain_length: 0,
            delta_chain_bytes: 0,
            delta_chain_started_ms: 0,
            deltas_total: previous.map(|p| p.deltas_total).unwrap_or(0),
            baselines_total: previous
                .map(|p| p.baselines_total.max(meta.baseline_generation))
                .unwrap_or(meta.baseline_generation),
            rows_inserted_total: previous.map(|p| p.rows_inserted_total).unwrap_or(0),
            rows_updated_total: previous.map(|p| p.rows_updated_total).unwrap_or(0),
            rows_deleted_total: previous.map(|p| p.rows_deleted_total).unwrap_or(0),
            rows_discovered: rows,
            duplicate_payload_rows: 0,
            last_observed_at_ms: now,
            previous_observed_at_ms: previous.and_then(|p| {
                (p.last_observed_at_ms > 0).then_some(p.last_observed_at_ms)
            }),
            observation_interval_ms: interval,
            wal_bytes: wal_len,
            last_unavailable: None,
            last_snapshot_reason: meta.snapshot_reason.clone(),
            observation_file: observation_relative,
        });
        archive.save_sources(state)?;
        report.ok = false;
        report.status = "degraded".to_string();
        report.change = "recovered_after_crash".to_string();
        report.error = Some(
            "the previous run crashed after committing a baseline; the committed unit was \
             adopted without writing a duplicate (degraded acquisition). The adopted \
             window cannot see deletes or same-value low-key inserts: the next capture \
             verifies it automatically (--verify forces it immediately)"
                .to_string(),
        );
        report.capture_id = Some(manifest.capture_id.clone());
        report.sha256 = Some(meta.baseline_digest.clone());
        report.watermark_len = Some(db_len);
        report.generation = meta.baseline_generation;
        report.records_discovered = rows;
        report.records_new = rows;
        report.records_deduped = 0;
        report.bytes = meta.baseline_bytes;
        report.sqlite = Some(SqliteCaptureInfo {
            mode: "baseline".to_string(),
            capture_seq: meta.capture_seq,
            baseline_generation: meta.baseline_generation,
            schema_fingerprint: meta.schema_fingerprint.clone(),
            snapshot_trigger: meta.snapshot_reason.clone(),
            baseline_digest: meta.baseline_digest.clone(),
            baseline_bytes: meta.baseline_bytes,
            baseline_chunks: meta.baseline_chunks.len() as u64,
            delta_object: None,
            delta_rows: 0,
            rows_inserted: 0,
            rows_updated: 0,
            rows_deleted: 0,
            rows_discovered: rows,
            duplicate_payload_rows: 0,
            observation_interval_ms: interval,
            delta_chain_length: 0,
            delta_chain_bytes: 0,
            wal_bytes: wal_len,
            schema_tables: meta.baseline_columns.keys().cloned().collect(),
            excluded_tables: Vec::new(),
            session_since_ms: observation.session_since_ms,
            verify: false,
            verify_mismatches: 0,
            missing_before_rows: 0,
            oversize_full_scan_tables: Vec::new(),
            unfiltered_tables: Vec::new(),
            verify_bounded_tables: Vec::new(),
            verify_gate: None,
        });
        return Ok(true);
    }

    if role.as_deref() == Some("sqlite_delta") {
        let (Some(previous), Some(previous_observed)) = (previous, previous_observed) else {
            return Ok(false);
        };
        // The recomputed change set (from the same previous observation the
        // crashed run used) must describe exactly the committed unit.
        let conn = match open_readonly(source) {
            Ok(conn) => conn,
            Err(_) => return Ok(false),
        };
        let (schema, schema_fingerprint_now) = match probe_schema(profile, &conn) {
            Ok(value) => value,
            Err(_) => return Ok(false),
        };
        if schema_fingerprint_now != previous.schema_fingerprint {
            return Ok(false);
        }
        let pending = match in_read_transaction(&conn, |conn| {
            observe_changes(
                profile,
                conn,
                previous_observed,
                &schema,
                options.session_since_ms,
            )
        }) {
            Ok(pending) => pending,
            Err(_) => return Ok(false),
        };
        let Some(object) = manifest.objects.first() else {
            return Ok(false);
        };
        let raw = decompress_object(archive, domain, &object.sha256)?;
        let committed = parse_delta_lines(&raw)?;
        if !changes_match_lines(&pending.changes, &committed) {
            return Ok(false);
        }
        // Adoption succeeds from here on: the run is a (degraded) crash recovery.
        report
            .anomalies
            .push("sqlite:recovered_after_crash".to_string());
        let rows = pending.rows_total;
        let raw_bytes = raw.len() as u64;
        let observation_relative = observation_relative_path(source_key);
        let mut observation = ObservationFile {
            capture_seq: meta.capture_seq,
            observed_at_ms: now,
            schema_fingerprint: schema_fingerprint_now.clone(),
            session_since_ms: previous_observed.session_since_ms,
            ..previous_observed.clone()
        };
        observation.tables = pending.tables.clone();
        observation.excluded = pending.excluded.clone();
        observation.selected_sessions = pending.selected_sessions.clone();
        save_observation(archive, &observation_relative, &observation)?;
        let entry = entry_for(&mut state, source_key, provider);
        entry.status = "degraded".to_string();
        entry.change = "recovered_after_crash".to_string();
        entry.generation = meta.baseline_generation;
        entry.discovered = rows;
        entry.captured = entry.captured.saturating_add(committed.len() as u64);
        entry.skipped = entry
            .skipped
            .saturating_add(rows.saturating_sub(committed.len() as u64));
        entry.bytes = entry.bytes.saturating_add(object.bytes);
        entry.errors = entry.errors.saturating_add(report.errors);
        entry.observed_len = Some(db_len);
        entry.incomplete_tail_bytes = 0;
        entry.last_error = None;
        entry.anomalies = report.anomalies.clone();
        entry.watermark = Some(Watermark {
            len: db_len,
            sha256: object.sha256.clone(),
            fingerprint: manifest.source.fingerprint.clone(),
            capture_id: manifest.capture_id.clone(),
            captured_at_ms: now,
            offset: db_len,
            line: 0,
            records: rows,
            generation: meta.baseline_generation,
            head_probe: String::new(),
            anchor_probe: String::new(),
            probe_len: 0,
        });
        entry.sqlite = Some(SqliteState {
            profile_id: profile.profile_id.to_string(),
            capture_seq: meta.capture_seq,
            baseline_generation: meta.baseline_generation,
            baseline_capture_seq: previous.baseline_capture_seq,
            baseline_object: String::new(),
            baseline_digest: previous.baseline_digest.clone(),
            baseline_bytes: previous.baseline_bytes,
            session_since_ms: observation.session_since_ms,
            last_verify_at_ms: previous.last_verify_at_ms,
            verify_mismatches_total: previous.verify_mismatches_total,
            excluded_tables: pending.excluded.clone(),
            unfiltered_tables: pending.unfiltered_tables.clone(),
            schema_fingerprint: schema_fingerprint_now.clone(),
            known_tables: previous.known_tables.clone(),
            known_columns: previous.known_columns.clone(),
            delta_chain_length: meta.delta_chain_length,
            delta_chain_bytes: previous.delta_chain_bytes.saturating_add(raw_bytes),
            delta_chain_started_ms: if previous.delta_chain_started_ms > 0 {
                previous.delta_chain_started_ms
            } else {
                now
            },
            deltas_total: previous.deltas_total + 1,
            baselines_total: previous.baselines_total,
            rows_inserted_total: previous
                .rows_inserted_total
                .saturating_add(meta.rows_inserted),
            rows_updated_total: previous.rows_updated_total.saturating_add(meta.rows_updated),
            rows_deleted_total: previous.rows_deleted_total.saturating_add(meta.rows_deleted),
            rows_discovered: rows,
            duplicate_payload_rows: pending.duplicates,
            last_observed_at_ms: now,
            previous_observed_at_ms: (previous.last_observed_at_ms > 0)
                .then_some(previous.last_observed_at_ms),
            observation_interval_ms: interval,
            wal_bytes: wal_len,
            last_unavailable: None,
            last_snapshot_reason: previous.last_snapshot_reason.clone(),
            observation_file: observation_relative,
        });
        archive.save_sources(state)?;
        report.ok = false;
        report.status = "degraded".to_string();
        report.change = "recovered_after_crash".to_string();
        report.error = Some(
            "the previous run crashed after committing evidence; the committed unit was \
             adopted without writing a duplicate (degraded acquisition)"
                .to_string(),
        );
        report.capture_id = Some(manifest.capture_id.clone());
        report.sha256 = Some(object.sha256.clone());
        report.watermark_len = Some(db_len);
        report.generation = meta.baseline_generation;
        report.records_discovered = rows;
        report.records_new = committed.len() as u64;
        report.records_deduped = rows.saturating_sub(committed.len() as u64);
        report.bytes = object.bytes;
        report.sqlite = Some(SqliteCaptureInfo {
            mode: "delta".to_string(),
            capture_seq: meta.capture_seq,
            baseline_generation: meta.baseline_generation,
            schema_fingerprint: schema_fingerprint_now.clone(),
            snapshot_trigger: String::new(),
            baseline_digest: previous.baseline_digest.clone(),
            baseline_bytes: previous.baseline_bytes,
            baseline_chunks: 0,
            delta_object: Some(object.sha256.clone()),
            delta_rows: committed.len() as u64,
            rows_inserted: meta.rows_inserted,
            rows_updated: meta.rows_updated,
            rows_deleted: meta.rows_deleted,
            rows_discovered: rows,
            duplicate_payload_rows: pending.duplicates,
            observation_interval_ms: interval,
            delta_chain_length: meta.delta_chain_length,
            delta_chain_bytes: previous.delta_chain_bytes.saturating_add(raw_bytes),
            wal_bytes: wal_len,
            schema_tables: pending.tables.keys().cloned().collect(),
            excluded_tables: excluded_facts(&pending.excluded),
            session_since_ms: observation.session_since_ms,
            verify: meta.verify_correction,
            verify_mismatches: 0,
            missing_before_rows: 0,
            oversize_full_scan_tables: pending.oversize_full_scan_tables.clone(),
            unfiltered_tables: pending.unfiltered_tables.clone(),
            verify_bounded_tables: Vec::new(),
            verify_gate: None,
        });
        return Ok(true);
    }
    Ok(false)
}

// ---------------------------------------------------------------------------
// Capture entry point
// ---------------------------------------------------------------------------

/// Capture one SQLite source as chunked whitelist baseline + watermark delta.
///
/// The caller holds the runtime-root single-writer lock and resolved the
/// canonical source key. The capture profile comes from the provider
/// (`profile_for_provider`); `options.sqlite_profile` overrides it explicitly
/// (used by tests and future provider wiring; an unknown id is an error, never
/// a silent fallback).
pub(crate) fn capture_sqlite_source(
    archive: &Archive,
    source: &Path,
    source_key: &str,
    provider: &str,
    domain: &str,
    failpoint: Option<Failpoint>,
    options: CaptureOptions,
    mut state: SourcesFile,
    mut report: CaptureReport,
) -> Result<CaptureReport, ArchiveError> {
    let profile = match options.sqlite_profile {
        Some(profile_id) => match sqlite_capture_profile_by_id(profile_id) {
            Some(profile) => profile,
            None => {
                return Err(ArchiveError::Corrupt(format!(
                    "unknown sqlite capture profile {profile_id:?}"
                )));
            }
        },
        None => profile_for_provider(provider),
    };
    report.source_kind = Some("sqlite".to_string());
    let now = now_ms();
    let db_len = fs::metadata(source).map(|meta| meta.len()).unwrap_or(0);
    let wal_len = wal_bytes(source);
    report.file_len = db_len;
    let previous_entry = state.sources.get(source_key).cloned().unwrap_or_default();
    let previous = previous_entry.sqlite.clone();
    let previous_observation_file = previous
        .as_ref()
        .map(|sqlite| sqlite.observation_file.clone())
        .filter(|relative| !relative.is_empty())
        .unwrap_or_else(|| observation_relative_path(source_key));
    let mut observation_state_rejected = false;
    let previous_observation = match load_observation_in(
        archive,
        &previous_observation_file,
        source_key,
        profile,
    ) {
        // A state file may legitimately be one sequence ahead of `state` only in
        // the real crash window (manifest committed, state not yet written), and
        // then a committed unit must actually exist at that sequence. Anything
        // else is rejected: an unverifiable `+1` state could otherwise smuggle a
        // "before" row image that was never observed.
        Ok(Some(file)) => {
            let mut accept = true;
            if let Some(prev) = previous.as_ref() {
                let seq = file.capture_seq;
                if seq == prev.capture_seq + 1 {
                    if committed_unit_at(archive, source_key, seq)?.is_none() {
                        accept = false;
                    }
                } else if seq < prev.capture_seq || seq > prev.capture_seq + 1 {
                    accept = false;
                }
            }
            if accept {
                Some(file)
            } else {
                observation_state_rejected = true;
                report
                    .anomalies
                    .push("sqlite:observation_state_unreadable".to_string());
                report.errors = report.errors.saturating_add(1);
                None
            }
        }
        Ok(None) => None,
        Err(err) => {
            // Unreadable collector state (including an older schema version) is
            // not silently ignored: it forces a new baseline and is reported.
            observation_state_rejected = true;
            report
                .anomalies
                .push(format!("sqlite:observation_state_unreadable: {err}"));
            report.errors = report.errors.saturating_add(1);
            None
        }
    };
    // (1) Cheap preflight: schema only, one read transaction. Lock timeouts and
    // unavailable databases become explicit health failures here.
    let preflight = open_readonly(source).and_then(|conn| probe_schema(profile, &conn));
    let (schema, schema_fingerprint_now) = match preflight {
        Ok(value) => value,
        Err(error) => {
            let mut state = state;
            record_health_failure(
                archive,
                profile,
                &mut state,
                source_key,
                provider,
                &mut report,
                &error,
                db_len,
                wal_len,
            )?;
            return Ok(report);
        }
    };

    let plan = decide_plan(
        profile,
        archive,
        source_key,
        previous.as_ref(),
        previous_observation.as_ref(),
        &schema_fingerprint_now,
        &schema,
        options,
    )?;
    // A rejected observation state must never produce a clean success: even when
    // there is no usable prior state at all (plan `initial`), the run re-baselines
    // in an unclassifiable-state condition and reports degraded, so `report.ok`
    // keeps agreeing with `coverage.ok`.
    let plan = match plan {
        Plan::Baseline("initial") if observation_state_rejected => {
            Plan::Baseline("unclassifiable_state")
        }
        other => other,
    };
    let interval = observation_interval(previous.as_ref(), now);

    // Crash window: the manifest is the commit point, state is written after it.
    // If a unit already sits at the next sequence number, adopt it rather than
    // writing the same change twice. This only applies when a state exists:
    // without one, a manifest at sequence 1 is just history, not evidence of a
    // crash window, and adopting it would misreport a lost state as recovery.
    if let Some(expected_seq) = previous.as_ref().map(|sqlite| sqlite.capture_seq + 1) {
        if let Some(manifest) = committed_unit_at(archive, source_key, expected_seq)? {
            if adopt_committed_unit(
                archive,
                profile,
                source,
                source_key,
                provider,
                domain,
                &mut state,
                &mut report,
                previous.as_ref(),
                previous_observation.as_ref(),
                &manifest,
                options,
                now,
                db_len,
                wal_len,
                interval,
            )? {
                return Ok(report);
            }
        }
    }

    match plan {
        Plan::Baseline(trigger) => capture_baseline(
            archive,
            profile,
            source,
            source_key,
            provider,
            domain,
            failpoint,
            options,
            state,
            report,
            previous.as_ref(),
            trigger,
            now,
            db_len,
            wal_len,
            interval,
        ),
        Plan::Delta => capture_delta(
            archive,
            profile,
            source,
            source_key,
            provider,
            domain,
            failpoint,
            options,
            state,
            report,
            previous.as_ref().expect("delta plan requires previous state"),
            previous_observation.expect("delta plan requires a previous observation"),
            now,
            db_len,
            wal_len,
            interval,
        ),
    }
}

// ---------------------------------------------------------------------------
// Baseline capture
// ---------------------------------------------------------------------------

/// Test seam: run a hook immediately before the baseline read transaction, so a
/// test can take a write lock *after* the preflight read and prove that the
/// baseline path is bounded by the busy timeout. The collector itself still
/// only reads the source.
pub fn set_pre_snapshot_hook(hook: Option<fn(&Path)>) {
    let mut guard = PRE_SNAPSHOT_HOOK
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    *guard = hook;
}

fn run_pre_snapshot_hook(source: &Path) {
    let hook = {
        let guard = PRE_SNAPSHOT_HOOK
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        *guard
    };
    if let Some(hook) = hook {
        hook(source);
    }
}

static PRE_SNAPSHOT_HOOK: std::sync::Mutex<Option<fn(&Path)>> = std::sync::Mutex::new(None);

/// Test seam: run a hook immediately before the full observation of a delta run,
/// i.e. after the schema preflight and after the plan was decided. A test can use
/// it to change the schema in that window and prove the explicit degradation path.
/// The collector itself still only reads the source.
pub fn set_pre_observation_hook(hook: Option<fn(&Path)>) {
    let mut guard = PRE_OBSERVATION_HOOK
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    *guard = hook;
}

fn run_pre_observation_hook(source: &Path) {
    let hook = {
        let guard = PRE_OBSERVATION_HOOK
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        *guard
    };
    if let Some(hook) = hook {
        hook(source);
    }
}

static PRE_OBSERVATION_HOOK: std::sync::Mutex<Option<fn(&Path)>> = std::sync::Mutex::new(None);

#[allow(clippy::too_many_arguments)]
fn capture_baseline(
    archive: &Archive,
    profile: &SqliteCaptureProfile,
    source: &Path,
    source_key: &str,
    provider: &str,
    domain: &str,
    failpoint: Option<Failpoint>,
    options: CaptureOptions,
    mut state: SourcesFile,
    mut report: CaptureReport,
    previous: Option<&SqliteState>,
    trigger: &'static str,
    now: i64,
    db_len: u64,
    wal_len: u64,
    interval: Option<i64>,
) -> Result<CaptureReport, ArchiveError> {
    run_pre_snapshot_hook(source);
    // (2) Chunked export inside one read transaction: no backup copy, no
    // whole-database image, whitelist rows only.
    let export = {
        let conn = match open_readonly(source) {
            Ok(conn) => conn,
            Err(error) => {
                return record_health_failure(
                    archive,
                    profile,
                    &mut state,
                    source_key,
                    provider,
                    &mut report,
                    &error,
                    db_len,
                    wal_len,
                )
                .map(|_| report);
            }
        };
        match in_read_transaction(&conn, |conn| {
            let schema = read_schema(conn)?;
            let _read = cost::phase("watermark_read");
            export_baseline(profile, archive, conn, &schema, options.session_since_ms)
        }) {
            Ok(export) => export,
            Err(error) => {
                return record_health_failure(
                    archive,
                    profile,
                    &mut state,
                    source_key,
                    provider,
                    &mut report,
                    &error,
                    db_len,
                    wal_len,
                )
                .map(|_| report);
            }
        }
    };
    // Read-back verification of every chunk (crash order: object → verify).
    for chunk in &export.chunks {
        if let Err(err) = archive.read_object_bytes(&chunk.sha256) {
            return Err(ArchiveError::Integrity(format!(
                "baseline chunk {} failed write verification: {err}",
                chunk.sha256
            )));
        }
    }
    report.objects_new += export.objects_new;
    report.objects_deduped += export.objects_deduped;

    // Dedupe: an identical baseline cannot become a second generation. The
    // observation state is still (re)written from the export facts, so a lost
    // observation file heals here instead of forcing a baseline on every later
    // run. Two separate questions, deliberately kept apart:
    // - `degraded_trigger`: incremental capture was not safe, so the run must
    //   report `degraded`/`ok=false`.
    // - `dedupe_blocked`: the previous baseline identity cannot be checked (the
    //   chunks are gone or the bookkeeping is broken) — re-store instead.
    let degraded_trigger = matches!(
        trigger,
        "observation_state_missing" | "baseline_unavailable" | "unclassifiable_state"
    );
    // An explicit `--rebaseline` must commit a new generation even when the
    // content is identical (the caller asked for a fresh baseline, not a
    // dedupe); the unavailable/unclassifiable triggers cannot check identity.
    let dedupe_blocked = matches!(
        trigger,
        "baseline_unavailable" | "unclassifiable_state" | "rebaseline_requested"
    );
    if !dedupe_blocked {
        if let Some(previous) = previous {
            if !previous.baseline_digest.is_empty()
                && previous.baseline_digest == export.digest
                && previous.schema_fingerprint == export.schema_fingerprint
                && baseline_objects_present_ok(archive, source_key, &export.digest)?
            {
                if degraded_trigger {
                    report
                        .anomalies
                        .insert(0, format!("sqlite_snapshot:{trigger}"));
                    report.errors = report.errors.saturating_add(1);
                }
                let observation_relative = observation_relative_path(source_key);
                let mut observation = ObservationFile::new(
                    profile,
                    source_key,
                    previous.capture_seq,
                    now,
                    export.schema_fingerprint.clone(),
                    options.session_since_ms,
                );
                observation.tables = export.table_states.clone();
                observation.excluded = export.excluded.clone();
                observation.selected_sessions = export.selected_sessions.clone();
                save_observation(archive, &observation_relative, &observation)?;
                let unchanged_state = sqlite_state_after_baseline(
                    profile,
                    Some(previous),
                    &export,
                    trigger,
                    options.session_since_ms,
                    now,
                    wal_len,
                    interval,
                    observation_relative,
                    true,
                    previous.capture_seq,
                    previous.baseline_generation,
                );
                return finish_unchanged(
                    archive,
                    profile,
                    &mut state,
                    report,
                    source_key,
                    provider,
                    unchanged_state,
                    Some(&export),
                    now,
                    db_len,
                    wal_len,
                    interval,
                    Some(format!("snapshot_deduplicated:{trigger}")),
                    degraded_trigger.then(|| trigger.to_string()),
                    true,
                );
            }
        }
    }

    let generation = previous
        .map(|state| state.baseline_generation.max(1) + 1)
        .unwrap_or(1)
        .max(max_committed_generation(archive, source_key)? + 1);
    let capture_seq = previous
        .map(|state| state.capture_seq + 1)
        .unwrap_or(1)
        .max(max_committed_capture_seq(archive, source_key)? + 1);
    let change = if capture_seq == 1 { "initial" } else { "baseline" };
    report.change = change.to_string();
    report.change_reason = Some(trigger.to_string());
    // A baseline trigger is a capture fact, not an anomaly: `schema_change`,
    // `unknown_table_or_column`, `filter_changed`, `rebaseline_requested` and
    // `delta_chain_threshold` are the designed policy and stay visible through
    // `snapshotTrigger` / `lastSnapshotReason`. Triggers that mean "incremental
    // capture was not safe" are health conditions: the run commits a usable
    // baseline but is reported degraded, because the previous evidence point
    // could not be continued.
    if degraded_trigger {
        report
            .anomalies
            .insert(0, format!("sqlite_snapshot:{trigger}"));
        report.errors = report.errors.saturating_add(1);
    }

    let fingerprint = crate::capture::fingerprint(source)?;
    let observation_relative = observation_relative_path(source_key);
    let mut observation = ObservationFile::new(
        profile,
        source_key,
        capture_seq,
        now,
        export.schema_fingerprint.clone(),
        options.session_since_ms,
    );
    observation.tables = export.table_states.clone();
    observation.excluded = export.excluded.clone();
    observation.selected_sessions = export.selected_sessions.clone();

    let objects: Vec<ManifestObject> = export
        .chunks
        .iter()
        .zip(export.chunk_compressed_bytes.iter())
        .map(|(chunk, compressed)| ManifestObject {
            sha256: chunk.sha256.clone(),
            bytes: *compressed,
            role: "sqlite_baseline_chunk".to_string(),
            access: vec![domain.to_string()],
            source_offset: Some(0),
            generation,
        })
        .collect();
    let compressed_total: u64 = objects.iter().map(|object| object.bytes).sum();

    if failpoint == Some(Failpoint::BeforeManifestCommit) {
        panic!("agent-memory failpoint: before_manifest_commit");
    }
    let manifest = Manifest {
        schema: MANIFEST_SCHEMA.to_string(),
        capture_id: new_id("cap"),
        captured_at_ms: now,
        device_id: archive.device_id()?,
        provider: provider.to_string(),
        source: ManifestSource {
            kind: "sqlite".to_string(),
            path: source_key.to_string(),
            compressed: true,
            fingerprint: fingerprint.clone(),
            source_state_at_capture: "present".to_string(),
            capture_reason: change.to_string(),
            generation,
            range_start: 0,
            carrier: None,
            sqlite: Some(ManifestSqlite {
                profile_id: profile.profile_id.to_string(),
                capture_seq,
                baseline_generation: generation,
                schema_fingerprint: export.schema_fingerprint.clone(),
                snapshot_reason: trigger.to_string(),
                observed_at_ms: now,
                observation_interval_ms: interval,
                rows_discovered: export.rows_discovered,
                rows_inserted: 0,
                rows_updated: 0,
                rows_deleted: 0,
                delta_chain_length: 0,
                baseline_schema: BASELINE_CHUNK_SCHEMA.to_string(),
                baseline_digest: export.digest.clone(),
                baseline_bytes: export.total_bytes,
                baseline_chunks: export.chunks.clone(),
                baseline_columns: export.columns.clone(),
                baseline_ddl: export.ddl.clone(),
                baseline_watermarks: export.watermarks.clone(),
                delta_table_ranges: BTreeMap::new(),
                verify_correction: false,
                delta_schema: String::new(),
            }),
        },
        objects,
        records: Vec::new(),
        attachments: Vec::new(),
    };
    let write_phase = cost::phase("evidence_write");
    archive.commit_manifest(&manifest)?;
    if failpoint == Some(Failpoint::AfterManifestBeforeState) {
        // Crash window: manifest committed, observation/source state not yet
        // written (the next run adopts this baseline from manifest facts).
        panic!("agent-memory failpoint: after_manifest_before_state");
    }
    save_observation(archive, &observation_relative, &observation)?;

    let entry = entry_for(&mut state, source_key, provider);
    entry.status = if degraded_trigger {
        "degraded".to_string()
    } else {
        "captured".to_string()
    };
    entry.change = change.to_string();
    entry.generation = generation;
    entry.discovered = export.rows_discovered;
    entry.captured = entry.captured.saturating_add(export.rows_discovered);
    entry.bytes = entry.bytes.saturating_add(compressed_total);
    entry.errors = entry.errors.saturating_add(report.errors);
    entry.incomplete_tail_bytes = 0;
    entry.observed_len = Some(db_len);
    entry.anomalies = report.anomalies.clone();
    entry.last_error = report.error.clone();
    entry.watermark = Some(Watermark {
        len: db_len,
        sha256: export.digest.clone(),
        fingerprint,
        capture_id: manifest.capture_id.clone(),
        captured_at_ms: now,
        offset: db_len,
        line: 0,
        records: export.rows_discovered,
        generation,
        head_probe: String::new(),
        anchor_probe: String::new(),
        probe_len: 0,
    });
    entry.sqlite = Some(sqlite_state_after_baseline(
        profile,
        previous,
        &export,
        trigger,
        options.session_since_ms,
        now,
        wal_len,
        interval,
        observation_relative,
        false,
        capture_seq,
        generation,
    ));
    archive.save_sources(&state)?;
    drop(write_phase);

    report.ok = !degraded_trigger;
    report.status = if degraded_trigger {
        "degraded".to_string()
    } else {
        "captured".to_string()
    };
    if degraded_trigger {
        report.error = Some(format!(
            "incremental capture was not safe ({trigger}); a new baseline was committed"
        ));
    }
    report.capture_id = Some(manifest.capture_id.clone());
    report.sha256 = Some(export.digest.clone());
    report.watermark_len = Some(db_len);
    report.offset = db_len;
    report.line = 0;
    report.generation = generation;
    report.records_discovered = export.rows_discovered;
    report.records_new = export.rows_discovered;
    report.records_deduped = 0;
    report.bytes = compressed_total;
    report.incomplete_tail_bytes = 0;
    report.sqlite = Some(SqliteCaptureInfo {
        mode: "baseline".to_string(),
        capture_seq,
        baseline_generation: generation,
        schema_fingerprint: export.schema_fingerprint.clone(),
        snapshot_trigger: trigger.to_string(),
        baseline_digest: export.digest.clone(),
        baseline_bytes: export.total_bytes,
        baseline_chunks: export.chunks.len() as u64,
        delta_object: None,
        delta_rows: 0,
        rows_inserted: 0,
        rows_updated: 0,
        rows_deleted: 0,
        rows_discovered: export.rows_discovered,
        duplicate_payload_rows: export.duplicates,
        observation_interval_ms: interval,
        delta_chain_length: 0,
        delta_chain_bytes: 0,
        wal_bytes: wal_len,
        schema_tables: export.table_rows.keys().cloned().collect(),
        excluded_tables: excluded_facts(&export.excluded),
        session_since_ms: options.session_since_ms,
        verify: false,
        verify_mismatches: 0,
        missing_before_rows: 0,
        oversize_full_scan_tables: export.oversize_full_scan_tables.clone(),
        unfiltered_tables: export.unfiltered_tables.clone(),
        verify_bounded_tables: Vec::new(),
        verify_gate: None,
    });
    // One object-directory walk per round: inside a capture-all round the
    // reconcile is deferred to the round end (and reported there).
    if let Some(reconcile) = archive.reconcile_after_unit()? {
        report.orphan_objects = reconcile.orphan_objects;
    }
    Ok(report)
}

/// Chunk existence check for the dedupe path (the current baseline's chunks
/// must still be in the store before an identical export may dedupe).
fn baseline_objects_present_ok(
    archive: &Archive,
    source_key: &str,
    digest: &str,
) -> Result<bool, ArchiveError> {
    Ok(sqlite_baselines(archive, source_key)?
        .into_iter()
        .filter(|baseline| baseline.baseline_digest == digest)
        .max_by_key(|baseline| baseline.capture_seq)
        .map(|baseline| baseline_objects_present(archive, &baseline.chunks))
        .unwrap_or(false))
}

/// Collector state after a baseline (fresh or deduplicated).
#[allow(clippy::too_many_arguments)]
fn sqlite_state_after_baseline(
    profile: &SqliteCaptureProfile,
    previous: Option<&SqliteState>,
    export: &BaselineExport,
    trigger: &str,
    session_since_ms: Option<i64>,
    now: i64,
    wal_len: u64,
    interval: Option<i64>,
    observation_relative: String,
    deduplicated: bool,
    capture_seq: u64,
    generation: u64,
) -> SqliteState {
    SqliteState {
        profile_id: profile.profile_id.to_string(),
        capture_seq,
        baseline_generation: generation,
        baseline_capture_seq: capture_seq,
        baseline_object: String::new(),
        baseline_digest: export.digest.clone(),
        baseline_bytes: export.total_bytes,
        session_since_ms,
        last_verify_at_ms: now,
        verify_mismatches_total: previous.map(|p| p.verify_mismatches_total).unwrap_or(0),
        excluded_tables: export.excluded.clone(),
        unfiltered_tables: export.unfiltered_tables.clone(),
        schema_fingerprint: export.schema_fingerprint.clone(),
        known_tables: export.table_rows.keys().cloned().collect(),
        known_columns: export.columns.clone(),
        delta_chain_length: if deduplicated {
            previous.map(|state| state.delta_chain_length).unwrap_or(0)
        } else {
            0
        },
        delta_chain_bytes: if deduplicated {
            previous.map(|state| state.delta_chain_bytes).unwrap_or(0)
        } else {
            0
        },
        delta_chain_started_ms: if deduplicated {
            previous.map(|state| state.delta_chain_started_ms).unwrap_or(0)
        } else {
            0
        },
        deltas_total: previous.map(|state| state.deltas_total).unwrap_or(0),
        baselines_total: if deduplicated {
            previous.map(|state| state.baselines_total).unwrap_or(0)
        } else {
            generation
        },
        rows_inserted_total: previous.map(|state| state.rows_inserted_total).unwrap_or(0),
        rows_updated_total: previous.map(|state| state.rows_updated_total).unwrap_or(0),
        rows_deleted_total: previous.map(|state| state.rows_deleted_total).unwrap_or(0),
        rows_discovered: export.rows_discovered,
        duplicate_payload_rows: export.duplicates,
        last_observed_at_ms: now,
        previous_observed_at_ms: previous.and_then(|state| {
            (state.last_observed_at_ms > 0).then_some(state.last_observed_at_ms)
        }),
        observation_interval_ms: interval,
        wal_bytes: wal_len,
        last_unavailable: None,
        last_snapshot_reason: trigger.to_string(),
        observation_file: observation_relative,
    }
}

// ---------------------------------------------------------------------------
// Delta capture
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn capture_delta(
    archive: &Archive,
    profile: &SqliteCaptureProfile,
    source: &Path,
    source_key: &str,
    provider: &str,
    domain: &str,
    failpoint: Option<Failpoint>,
    options: CaptureOptions,
    mut state: SourcesFile,
    mut report: CaptureReport,
    previous: &SqliteState,
    previous_observation: ObservationFile,
    now: i64,
    db_len: u64,
    wal_len: u64,
    interval: Option<i64>,
) -> Result<CaptureReport, ArchiveError> {
    run_pre_observation_hook(source);
    let baseline = {
        let baselines = sqlite_baselines(archive, source_key)?;
        baselines
            .into_iter()
            .filter(|baseline| {
                baseline.baseline_digest == previous.baseline_digest
                    || baseline.capture_seq <= previous.capture_seq
            })
            .max_by_key(|baseline| baseline.capture_seq)
    };
    let Some(baseline) = baseline else {
        // Without the baseline projection the watermark diff has no evidence
        // anchor; report degraded unchanged and let the next run re-baseline.
        return finish_unchanged(
            archive,
            profile,
            &mut state,
            report,
            source_key,
            provider,
            previous.clone(),
            None,
            now,
            db_len,
            wal_len,
            interval,
            None,
            Some("baseline_unavailable".to_string()),
            false,
        );
    };

    // (3) Watermark observation inside one read transaction.
    let mut pending = {
        let conn = match open_readonly(source) {
            Ok(conn) => conn,
            Err(error) => {
                return record_health_failure(
                    archive,
                    profile,
                    &mut state,
                    source_key,
                    provider,
                    &mut report,
                    &error,
                    db_len,
                    wal_len,
                )
                .map(|_| report);
            }
        };
        match in_read_transaction(&conn, |conn| {
            let schema = read_schema(conn)?;
            // A schema drift between the preflight and the observation cannot
            // be diffed against the old baseline; reported by the caller.
            if schema_fingerprint_in(profile, &schema) != previous.schema_fingerprint {
                return Ok(None);
            }
            observe_changes(
                profile,
                conn,
                &previous_observation,
                &schema,
                options.session_since_ms,
            )
            .map(Some)
        }) {
            Ok(Some(pending)) => pending,
            Ok(None) => {
                report
                    .anomalies
                    .push("sqlite:schema_changed_during_observation".to_string());
                report.errors = report.errors.saturating_add(1);
                return finish_unchanged(
                    archive,
                    profile,
                    &mut state,
                    report,
                    source_key,
                    provider,
                    previous.clone(),
                    None,
                    now,
                    db_len,
                    wal_len,
                    interval,
                    None,
                    Some("schema_changed_during_observation".to_string()),
                    false,
                );
            }
            Err(error) => {
                return record_health_failure(
                    archive,
                    profile,
                    &mut state,
                    source_key,
                    provider,
                    &mut report,
                    &error,
                    db_len,
                    wal_len,
                )
                .map(|_| report);
            }
        }
    };
    let mut changes = std::mem::take(&mut pending.changes);
    let mut missing_before = 0u64;
    if std::env::var_os("AM_TRACE_DECOMPRESS").is_some() {
        eprintln!(
            "[delta] pending changes: {} ops: {:?}",
            changes.len(),
            changes.iter().map(|c| (c.table.as_str(), c.key.as_str(), c.op)).collect::<Vec<_>>()
        );
    }
    if changes.iter().any(|change| change.op != "insert") {
        // `before` images come from the evidence, never from collector state —
        // and only from the chunks/batches whose key ranges contain the
        // touched keys (N4). An observation with inserts only, or with no
        // changes at all, decompresses no evidence object.
        let batch_metas = sqlite_delta_batch_metas(archive, source_key)?;
        let snapshot = archive.manifests()?;
        let _evidence = cost::phase("evidence_read");
        fill_before_from_evidence(
            archive,
            &snapshot,
            domain,
            &baseline,
            &batch_metas,
            &mut changes,
            &mut missing_before,
        )?;
        drop(snapshot);
    }
    let rows = pending.rows_total;
    let duplicates = pending.duplicates;
    // Due: explicit --verify, the 7-day cycle, or the first capture after a
    // crash-adopted baseline (N-R4: the adopt window is verified next run).
    let verify_wanted = options.verify
        || previous.last_verify_at_ms == 0
        || now.saturating_sub(previous.last_verify_at_ms) >= VERIFY_INTERVAL_MS;
    // §E heavy-job gate: the periodic (7-day) full-table verification waits for
    // an idle desktop, no full-screen application and AC power. An explicit
    // `--verify` is an operator request and is never gated, and neither is the
    // first verification after a crash-adopted baseline (that window is
    // untrusted evidence: the repair is not routine maintenance). A blocked
    // cycle is deferred to the next round and runs unconditionally once the
    // block outlasts 48 h.
    let periodic_cycle = previous.last_verify_at_ms != 0;
    let verify_gate = if verify_wanted && !options.verify && periodic_cycle {
        Some(verify_gate_decision(archive, source_key, now))
    } else {
        None
    };
    let verify_due = match &verify_gate {
        Some(gate) => gate.action.executes(),
        None => verify_wanted,
    };
    if changes.is_empty() {
        // Nothing observable changed. This covers the blind-window cases: a row
        // that changed and changed back, or a content-only edit the watermark
        // could not see (both caught by the verification pass).
        let mut observation = ObservationFile {
            capture_seq: previous.capture_seq,
            observed_at_ms: now,
            schema_fingerprint: previous.schema_fingerprint.clone(),
            session_since_ms: options.session_since_ms,
            ..previous_observation
        };
        observation.tables = pending.tables.clone();
        observation.excluded = pending.excluded.clone();
        observation.selected_sessions = pending.selected_sessions.clone();
        let mut report = finish_unchanged_with_state(
            archive,
            &mut state,
            report,
            source_key,
            provider,
            previous.clone(),
            Some(observation),
            &pending,
            now,
            db_len,
            wal_len,
            interval,
            None,
            None,
            false,
        )?;
        apply_verify_gate(&mut report, &verify_gate);
        if verify_due {
            return run_verification_and_correct(
                archive,
                profile,
                source,
                source_key,
                provider,
                domain,
                state,
                report,
                &baseline,
                now,
                interval,
            );
        }
        // One object-directory walk per round: inside a capture-all round the
        // reconcile is deferred to the round end (and reported there).
        if let Some(reconcile) = archive.reconcile_after_unit()? {
            report.orphan_objects = reconcile.orphan_objects;
        }
        return Ok(report);
    }

    let capture_seq = (previous.capture_seq + 1)
        .max(max_committed_capture_seq(archive, source_key)? + 1);
    // Deterministic line order (table, key, op). This puts child rows
    // (session_message) before their parents (session_v2) when both change in
    // one window; replay application is a row map and the derived
    // materialization inserts parent-first with foreign_keys OFF.
    changes.sort_by(|a, b| {
        a.table
            .cmp(&b.table)
            .then_with(|| a.key.cmp(&b.key))
            .then_with(|| a.op.cmp(b.op))
    });
    let mut lines: Vec<DeltaLine> = Vec::with_capacity(changes.len());
    let mut inserted = 0u64;
    let mut updated = 0u64;
    let mut deleted = 0u64;
    for change in &changes {
        match change.op {
            "insert" => inserted += 1,
            "update" => updated += 1,
            "delete" => deleted += 1,
            _ => {}
        }
        lines.push(DeltaLine {
            capture_seq,
            observed_at_ms: now,
            table: change.table.clone(),
            key: change.key.clone(),
            op: change.op.to_string(),
            before: change.before.clone(),
            after: change.after.clone(),
        });
    }
    let write_phase = cost::phase("evidence_write");
    let mut raw = Vec::new();
    for line in &lines {
        raw.extend_from_slice(&line.to_line_bytes());
    }
    let raw_len = raw.len() as u64;
    let compressed = zstd::stream::encode_all(raw.as_slice(), ZSTD_LEVEL)
        .map_err(|err| ArchiveError::Corrupt(format!("delta compression: {err}")))?;
    let (object_sha, compressed_len, is_new) = archive.put_object(&compressed)?;
    archive.read_object_bytes(&object_sha).map_err(|err| {
        ArchiveError::Integrity(format!("delta {object_sha} failed write verification: {err}"))
    })?;
    if is_new {
        report.objects_new += 1;
    } else {
        report.objects_deduped += 1;
    }

    // One manifest record per delta line, so a single row change is locatable
    // and hash-verifiable without unpacking the whole batch object.
    let mut records = Vec::with_capacity(lines.len());
    let mut offset = 0u64;
    for (index, line) in lines.iter().enumerate() {
        let bytes = line.to_line_bytes();
        let content_len = bytes.len().saturating_sub(1) as u64;
        records.push(ManifestRecord {
            seq: index as u64,
            offset,
            len: content_len,
            sha256: hex_sha256(&bytes[..content_len as usize]),
            object: object_sha.clone(),
            access: vec![domain.to_string()],
            source_offset: None,
            source_line: Some(index as u64 + 1),
        });
        offset += bytes.len() as u64;
    }

    let chain_length = previous.delta_chain_length + 1;
    let chain_started = if previous.delta_chain_started_ms > 0 {
        previous.delta_chain_started_ms
    } else {
        now
    };
    // Per-table key ranges of this batch: before-lookups (and verification)
    // decompress only batches whose range contains the keys they need (N4).
    let mut delta_table_ranges: BTreeMap<String, (String, String)> = BTreeMap::new();
    for line in &lines {
        let entry = delta_table_ranges
            .entry(line.table.clone())
            .or_insert_with(|| (line.key.clone(), line.key.clone()));
        if line.key < entry.0 {
            entry.0 = line.key.clone();
        }
        if line.key > entry.1 {
            entry.1 = line.key.clone();
        }
    }
    let fingerprint = crate::capture::fingerprint(source)?;
    let observation_relative = observation_relative_path(source_key);
    let mut observation = ObservationFile {
        profile_id: profile.profile_id.to_string(),
        capture_seq,
        observed_at_ms: now,
        schema_fingerprint: previous.schema_fingerprint.clone(),
        session_since_ms: options.session_since_ms,
        ..previous_observation
    };
    observation.tables = pending.tables.clone();
    observation.excluded = pending.excluded.clone();
    observation.selected_sessions = pending.selected_sessions.clone();

    if failpoint == Some(Failpoint::BeforeManifestCommit) {
        panic!("agent-memory failpoint: before_manifest_commit");
    }
    let manifest = Manifest {
        schema: MANIFEST_SCHEMA.to_string(),
        capture_id: new_id("cap"),
        captured_at_ms: now,
        device_id: archive.device_id()?,
        provider: provider.to_string(),
        source: ManifestSource {
            kind: "sqlite".to_string(),
            path: source_key.to_string(),
            compressed: true,
            fingerprint: fingerprint.clone(),
            source_state_at_capture: "present".to_string(),
            capture_reason: "delta".to_string(),
            generation: previous.baseline_generation.max(1),
            range_start: 0,
            carrier: None,
            sqlite: Some(ManifestSqlite {
                profile_id: profile.profile_id.to_string(),
                capture_seq,
                baseline_generation: previous.baseline_generation.max(1),
                schema_fingerprint: previous.schema_fingerprint.clone(),
                snapshot_reason: String::new(),
                observed_at_ms: now,
                observation_interval_ms: interval,
                rows_discovered: lines.len() as u64,
                rows_inserted: inserted,
                rows_updated: updated,
                rows_deleted: deleted,
                delta_chain_length: chain_length,
                baseline_schema: String::new(),
                baseline_digest: String::new(),
                baseline_bytes: 0,
                baseline_chunks: Vec::new(),
                baseline_columns: BTreeMap::new(),
                baseline_ddl: BTreeMap::new(),
                baseline_watermarks: BTreeMap::new(),
                delta_table_ranges,
                verify_correction: false,
                delta_schema: DELTA_LINE_SCHEMA.to_string(),
            }),
        },
        objects: vec![ManifestObject {
            sha256: object_sha.clone(),
            bytes: compressed_len,
            role: "sqlite_delta".to_string(),
            access: vec![domain.to_string()],
            source_offset: None,
            generation: previous.baseline_generation.max(1),
        }],
        records: records.clone(),
        attachments: Vec::new(),
    };
    archive.commit_manifest(&manifest)?;

    // Staged in memory; the ledger is written once per capture-all round (and
    // immediately for a single-source capture).
    for record in &records {
        archive.stage_record_ref(
            &record.sha256,
            RecordRef {
                object: record.object.clone(),
                offset: record.offset,
                len: record.len,
                capture_id: manifest.capture_id.clone(),
            },
        )?;
    }
    archive.flush_records()?;
    if failpoint == Some(Failpoint::AfterManifestBeforeState) {
        // Crash window: manifest + record index committed, observation state
        // and source state not yet written (the next run adopts this unit).
        panic!("agent-memory failpoint: after_manifest_before_state");
    }
    save_observation(archive, &observation_relative, &observation)?;
    let entry = entry_for(&mut state, source_key, provider);
    entry.status = "captured".to_string();
    entry.change = "delta".to_string();
    entry.generation = previous.baseline_generation.max(1);
    entry.discovered = lines.len() as u64;
    entry.captured = entry.captured.saturating_add(lines.len() as u64);
    entry.skipped = entry
        .skipped
        .saturating_add(rows.saturating_sub(lines.len() as u64));
    entry.bytes = entry.bytes.saturating_add(compressed_len);
    entry.errors = entry.errors.saturating_add(report.errors);
    entry.incomplete_tail_bytes = 0;
    entry.observed_len = Some(db_len);
    entry.anomalies = report.anomalies.clone();
    entry.last_error = None;
    entry.watermark = Some(Watermark {
        len: db_len,
        sha256: object_sha.clone(),
        fingerprint,
        capture_id: manifest.capture_id.clone(),
        captured_at_ms: now,
        offset: db_len,
        line: 0,
        records: rows,
        generation: previous.baseline_generation.max(1),
        head_probe: String::new(),
        anchor_probe: String::new(),
        probe_len: 0,
    });
    entry.sqlite = Some(SqliteState {
        profile_id: profile.profile_id.to_string(),
        capture_seq,
        baseline_generation: previous.baseline_generation.max(1),
        baseline_capture_seq: previous.baseline_capture_seq,
        baseline_object: String::new(),
        baseline_digest: previous.baseline_digest.clone(),
        baseline_bytes: previous.baseline_bytes,
        session_since_ms: options.session_since_ms,
        last_verify_at_ms: previous.last_verify_at_ms,
        verify_mismatches_total: previous.verify_mismatches_total,
        excluded_tables: observation.excluded.clone(),
        unfiltered_tables: pending.unfiltered_tables.clone(),
        schema_fingerprint: previous.schema_fingerprint.clone(),
        known_tables: previous.known_tables.clone(),
        known_columns: previous.known_columns.clone(),
        delta_chain_length: chain_length,
        delta_chain_bytes: previous.delta_chain_bytes.saturating_add(raw_len),
        delta_chain_started_ms: chain_started,
        deltas_total: previous.deltas_total + 1,
        baselines_total: previous.baselines_total,
        rows_inserted_total: previous.rows_inserted_total.saturating_add(inserted),
        rows_updated_total: previous.rows_updated_total.saturating_add(updated),
        rows_deleted_total: previous.rows_deleted_total.saturating_add(deleted),
        rows_discovered: rows,
        duplicate_payload_rows: duplicates,
        last_observed_at_ms: now,
        previous_observed_at_ms: (previous.last_observed_at_ms > 0)
            .then_some(previous.last_observed_at_ms),
        observation_interval_ms: interval,
        wal_bytes: wal_len,
        last_unavailable: None,
        last_snapshot_reason: previous.last_snapshot_reason.clone(),
        observation_file: observation_relative,
    });
    archive.save_sources(&state)?;
    drop(write_phase);

    report.ok = true;
    report.status = "captured".to_string();
    report.change = "delta".to_string();
    report.capture_id = Some(manifest.capture_id.clone());
    report.sha256 = Some(object_sha.clone());
    report.watermark_len = Some(db_len);
    report.offset = db_len;
    report.line = 0;
    report.generation = previous.baseline_generation.max(1);
    report.records_discovered = rows;
    report.records_new = lines.len() as u64;
    report.records_deduped = rows.saturating_sub(lines.len() as u64);
    report.bytes = compressed_len;
    report.incomplete_tail_bytes = 0;
    report.sqlite = Some(SqliteCaptureInfo {
        mode: "delta".to_string(),
        capture_seq,
        baseline_generation: previous.baseline_generation.max(1),
        schema_fingerprint: previous.schema_fingerprint.clone(),
        snapshot_trigger: String::new(),
        baseline_digest: previous.baseline_digest.clone(),
        baseline_bytes: previous.baseline_bytes,
        baseline_chunks: 0,
        delta_object: Some(object_sha),
        delta_rows: lines.len() as u64,
        rows_inserted: inserted,
        rows_updated: updated,
        rows_deleted: deleted,
        rows_discovered: rows,
        duplicate_payload_rows: duplicates,
        observation_interval_ms: interval,
        delta_chain_length: chain_length,
        delta_chain_bytes: previous.delta_chain_bytes.saturating_add(raw_len),
        wal_bytes: wal_len,
        schema_tables: observation.tables.keys().cloned().collect(),
        excluded_tables: excluded_facts(&observation.excluded),
        session_since_ms: options.session_since_ms,
        verify: false,
        verify_mismatches: 0,
        missing_before_rows: missing_before,
        oversize_full_scan_tables: pending.oversize_full_scan_tables.clone(),
        unfiltered_tables: pending.unfiltered_tables.clone(),
        verify_bounded_tables: Vec::new(),
        verify_gate: None,
    });

    // (4) Periodic verification (explicit --verify or the 7-day cycle), gated
    // by §E when it is the automatic cycle.
    apply_verify_gate(&mut report, &verify_gate);
    if verify_due {
        return run_verification_and_correct(
            archive,
            profile,
            source,
            source_key,
            provider,
            domain,
            state,
            report,
            &baseline,
            now,
            interval,
        );
    }
    // One object-directory walk per round: inside a capture-all round the
    // reconcile is deferred to the round end (and reported there).
    if let Some(reconcile) = archive.reconcile_after_unit()? {
        report.orphan_objects = reconcile.orphan_objects;
    }
    Ok(report)
}

/// Record an observation that commits no new evidence (deduplicated baseline or
/// schema-drift/baseline-unavailable runs). `export` carries the fresh
/// observation state when one may be persisted.
#[allow(clippy::too_many_arguments)]
fn finish_unchanged(
    archive: &Archive,
    profile: &SqliteCaptureProfile,
    state: &mut SourcesFile,
    report: CaptureReport,
    source_key: &str,
    provider: &str,
    previous: SqliteState,
    export: Option<&BaselineExport>,
    now: i64,
    db_len: u64,
    wal_len: u64,
    interval: Option<i64>,
    deduplicated: Option<String>,
    degraded_reason: Option<String>,
    chain_reset: bool,
) -> Result<CaptureReport, ArchiveError> {
    let rows = export
        .map(|export| export.rows_discovered)
        .unwrap_or(previous.rows_discovered);
    let duplicates = export
        .map(|export| export.duplicates)
        .unwrap_or(previous.duplicate_payload_rows);
    let excluded = export
        .map(|export| export.excluded.clone())
        .unwrap_or(previous.excluded_tables.clone());
    let oversize = export
        .map(|export| export.oversize_full_scan_tables.clone())
        .unwrap_or_default();
    let unfiltered = export
        .map(|export| export.unfiltered_tables.clone())
        .unwrap_or(previous.unfiltered_tables.clone());
    let observation = export.map(|export| {
        let mut file = ObservationFile::new(
            profile,
            source_key,
            previous.capture_seq,
            now,
            export.schema_fingerprint.clone(),
            previous.session_since_ms,
        );
        file.tables = export.table_states.clone();
        file.excluded = export.excluded.clone();
        file.selected_sessions = export.selected_sessions.clone();
        file
    });
    let schema_tables: Vec<String> = match export {
        Some(export) => export.table_rows.keys().cloned().collect(),
        None => previous.known_tables.clone(),
    };
    let pending = PendingObservation {
        changes: Vec::new(),
        tables: BTreeMap::new(),
        excluded,
        selected_sessions: BTreeSet::new(),
        rows_total: rows,
        duplicates,
        oversize_full_scan_tables: oversize,
        unfiltered_tables: unfiltered,
    };
    let _ = schema_tables;
    finish_unchanged_with_state(
        archive,
        state,
        report,
        source_key,
        provider,
        previous,
        observation,
        &pending,
        now,
        db_len,
        wal_len,
        interval,
        deduplicated,
        degraded_reason,
        chain_reset,
    )
}

/// Record an observation that commits no new evidence. `observation` is the
/// fresh observation state to persist (watermarks advance even when nothing
/// changed — otherwise every run would re-read the same candidates).
#[allow(clippy::too_many_arguments)]
fn finish_unchanged_with_state(
    archive: &Archive,
    state: &mut SourcesFile,
    mut report: CaptureReport,
    source_key: &str,
    provider: &str,
    previous: SqliteState,
    observation: Option<ObservationFile>,
    pending: &PendingObservation,
    now: i64,
    db_len: u64,
    wal_len: u64,
    interval: Option<i64>,
    deduplicated: Option<String>,
    degraded_reason: Option<String>,
    chain_reset: bool,
) -> Result<CaptureReport, ArchiveError> {
    let rows = if pending.rows_total > 0 {
        pending.rows_total
    } else {
        previous.rows_discovered
    };
    let duplicates = pending.duplicates;
    let excluded = if !pending.excluded.is_empty() {
        pending.excluded.clone()
    } else {
        previous.excluded_tables.clone()
    };
    let schema_tables: Vec<String> = match &observation {
        Some(file) if !file.tables.is_empty() => file.tables.keys().cloned().collect(),
        _ => previous.known_tables.clone(),
    };
    let degraded = degraded_reason.is_some();
    report.ok = !degraded;
    report.status = if degraded {
        "degraded".to_string()
    } else {
        "skipped_unchanged".to_string()
    };
    report.change = if degraded {
        "degraded".to_string()
    } else {
        "skipped_unchanged".to_string()
    };
    if let Some(reason) = degraded_reason {
        report.change_reason = Some(reason.clone());
        report.error = Some(format!(
            "incremental capture was not safe ({reason}); no delta was written"
        ));
    }
    report.generation = previous.baseline_generation.max(1);
    report.records_discovered = rows;
    report.records_deduped = rows;
    report.file_len = db_len;
    report.watermark_len = Some(db_len);
    report.incomplete_tail_bytes = 0;
    report.sqlite = Some(SqliteCaptureInfo {
        mode: "unchanged".to_string(),
        capture_seq: previous.capture_seq,
        baseline_generation: previous.baseline_generation.max(1),
        schema_fingerprint: previous.schema_fingerprint.clone(),
        // Deduplicated baseline triggers stay visible as a capture fact without
        // being reported as a health problem.
        snapshot_trigger: deduplicated.unwrap_or_default(),
        baseline_digest: previous.baseline_digest.clone(),
        baseline_bytes: previous.baseline_bytes,
        baseline_chunks: 0,
        delta_object: None,
        delta_rows: 0,
        rows_inserted: 0,
        rows_updated: 0,
        rows_deleted: 0,
        rows_discovered: rows,
        duplicate_payload_rows: duplicates,
        observation_interval_ms: interval,
        delta_chain_length: if chain_reset {
            0
        } else {
            previous.delta_chain_length
        },
        delta_chain_bytes: if chain_reset {
            0
        } else {
            previous.delta_chain_bytes
        },
        wal_bytes: wal_len,
        schema_tables,
        excluded_tables: excluded_facts(&excluded),
        session_since_ms: previous.session_since_ms,
        verify: false,
        verify_mismatches: 0,
        missing_before_rows: 0,
        oversize_full_scan_tables: pending.oversize_full_scan_tables.clone(),
        unfiltered_tables: pending.unfiltered_tables.clone(),
        verify_bounded_tables: Vec::new(),
        verify_gate: None,
    });

    // Persist the fresh observation state (watermarks/pk sets advance even on
    // unchanged runs). On drift-unchanged runs (`observation = None`) the state
    // is left untouched: a schema the plan did not see must not be diffed next.
    let mut sqlite = previous.clone();
    if let Some(file) = observation {
        let relative = if previous.observation_file.is_empty() {
            observation_relative_path(source_key)
        } else {
            previous.observation_file.clone()
        };
        save_observation(archive, &relative, &file)?;
        sqlite.observation_file = relative;
    }
    sqlite.rows_discovered = rows;
    sqlite.duplicate_payload_rows = duplicates;
    sqlite.unfiltered_tables = pending.unfiltered_tables.clone();
    sqlite.excluded_tables = excluded;
    sqlite.previous_observed_at_ms =
        (sqlite.last_observed_at_ms > 0).then_some(sqlite.last_observed_at_ms);
    sqlite.last_observed_at_ms = now;
    sqlite.observation_interval_ms = interval;
    sqlite.wal_bytes = wal_len;
    if chain_reset {
        // A chain whose net effect is zero is provably back at the baseline:
        // reset it, otherwise every later run would take a baseline only to
        // deduplicate it again.
        sqlite.delta_chain_length = 0;
        sqlite.delta_chain_bytes = 0;
        sqlite.delta_chain_started_ms = 0;
    }
    // A successful observation of a readable, unlocked database clears the
    // previous health failure: `ok` reports current health, not a lifetime
    // counter. (The failure itself stays in the capture history/manifests of the
    // failed run, which wrote no evidence.)
    sqlite.last_unavailable = None;
    let entry = entry_for(state, source_key, provider);
    entry.status = report.status.clone();
    entry.change = report.change.clone();
    entry.observed_len = Some(db_len);
    entry.anomalies = report.anomalies.clone();
    entry.errors = entry.errors.saturating_add(report.errors);
    entry.last_error = report.error.clone();
    entry.watermark = entry.watermark.clone().map(|mut watermark| {
        watermark.len = db_len;
        watermark
    });
    entry.sqlite = Some(sqlite);
    archive.save_sources(&state)?;
    Ok(report)
}

// ---------------------------------------------------------------------------
// Verification (blind-window corrections)
// ---------------------------------------------------------------------------

/// A delta overlay for one table: key -> `None` (deleted by a delta) or
/// `Some(row hash)` (inserted/updated by a delta). Content hashes only, so the
/// overlay scales with the delta chain, never with the table.
type DeltaOverlay = BTreeMap<String, Option<String>>;

/// Build the delta overlays for every archived table of `baseline`, in capture
/// order. Batches that contain lines for none of the tracked tables are never
/// decompressed.
fn build_delta_overlays(
    archive: &Archive,
    domain: &str,
    baseline: &SqliteBaseline,
    batch_metas: &[SqliteDeltaBatchMeta],
) -> Result<BTreeMap<String, DeltaOverlay>, ArchiveError> {
    let mut overlays: BTreeMap<String, DeltaOverlay> = baseline
        .columns
        .keys()
        .map(|table| (table.clone(), BTreeMap::new()))
        .collect();
    for meta in batch_metas {
        if meta.baseline_generation != baseline.baseline_generation
            || meta.capture_seq <= baseline.capture_seq
        {
            continue;
        }
        // N-R3: a batch without recorded ranges (legacy manifest) may contain
        // lines for any tracked table and is always read; a batch WITH ranges
        // is skipped only when none of its tables is tracked.
        if !meta.table_ranges.is_empty()
            && meta
                .table_ranges
                .keys()
                .all(|table| !overlays.contains_key(table))
        {
            continue;
        }
        for line in read_delta_lines(archive, domain, meta)? {
            let Some(overlay) = overlays.get_mut(&line.table) else {
                continue;
            };
            match line.op.as_str() {
                "delete" => {
                    overlay.insert(line.key, None);
                }
                _ => {
                    if let Some(after) = &line.after {
                        overlay.insert(line.key, Some(row_hash_value(after)));
                    }
                }
            }
        }
    }
    Ok(overlays)
}

/// Expected-side stream: the baseline's chunk lines in export order, hashed
/// one line at a time. Memory is one line + the decompression window (N5).
struct ExpectedStream<'a> {
    chunks: Vec<&'a ManifestSqliteChunk>,
    index: usize,
    stream: Option<ChunkLineStream>,
}

impl<'a> ExpectedStream<'a> {
    fn new(baseline: &'a SqliteBaseline, table: &str) -> Self {
        Self {
            chunks: baseline
                .chunks
                .iter()
                .filter(|chunk| chunk.table == table)
                .collect(),
            index: 0,
            stream: None,
        }
    }

    fn next(
        &mut self,
        archive: &Archive,
        domain: &str,
    ) -> Result<Option<(Vec<SortValue>, String, String)>, ArchiveError> {
        loop {
            if self.index >= self.chunks.len() {
                return Ok(None);
            }
            let chunk = self.chunks[self.index];
            if self.stream.is_none() {
                self.stream = Some(ChunkLineStream::open(archive, domain, chunk)?);
            }
            match self
                .stream
                .as_mut()
                .expect("stream is open while the chunk is unfinished")
                .next_row(chunk)?
            {
                Some((key, row)) => {
                    let hash = row_hash_value(&row);
                    match sort_values_from_key(&key) {
                        Some(sort) => return Ok(Some((sort, key, hash))),
                        // A chunk key that cannot be ordered is a hard error,
                        // not a bounded-path fallback: keys of an orderable
                        // table are always `pk:`/`rowid:`-derived, so this
                        // only fires on corrupt evidence.
                        None => {
                            return Err(ArchiveError::Corrupt(format!(
                                "baseline chunk key {} is not orderable",
                                key
                            )))
                        }
                    }
                }
                None => {
                    self.stream = None;
                    self.index += 1;
                }
            }
        }
    }
}

/// One merge verdict for the live row behind `key` (if any).
enum Verdict {
    Match,
    Update(Value),
    Insert(Value),
    Delete,
}

fn judge(
    overlay: &mut DeltaOverlay,
    key: &str,
    baseline_hash: Option<&str>,
    live: Option<(&str, &Value)>,
) -> Verdict {
    match overlay.remove(key) {
        Some(Some(overlay_hash)) => match live {
            Some((live_hash, _)) if live_hash == overlay_hash.as_str() => Verdict::Match,
            Some((_, row)) => Verdict::Update(row.clone()),
            None => Verdict::Delete,
        },
        // Deleted in the evidence but still live: an insert correction.
        Some(None) => match live {
            Some((_, row)) => Verdict::Insert(row.clone()),
            None => Verdict::Match,
        },
        None => match (baseline_hash, live) {
            (Some(baseline), Some((live_hash, _))) if baseline == live_hash => Verdict::Match,
            (Some(_), Some((_, row))) => Verdict::Update(row.clone()),
            (Some(_), None) => Verdict::Delete,
            (None, Some((_, row))) => Verdict::Insert(row.clone()),
            (None, None) => Verdict::Match,
        },
    }
}

fn push_delete(changes: &mut Vec<Change>, table: &str, key: String) {
    changes.push(Change {
        table: table.to_string(),
        key,
        op: "delete",
        before: None,
        after: None,
    });
}

/// Streaming merge verification for one orderable table (N5): the archived
/// rows (chunk stream + delta overlay) and the live rows (ordered source scan)
/// are walked in the same canonical order, so memory stays at one line per
/// stream plus the delta overlay — never at the table's row count.
/// One primary-key component, typed for SQLite-`ORDER BY`-compatible
/// comparison (R2): `NULL < INTEGER/REAL < TEXT < BLOB`, numbers by value,
/// text and blobs by byte order (BINARY collation).
#[derive(Debug, Clone, PartialEq, Eq)]
enum SortValue {
    Null,
    Integer(i64),
    /// f64 payload stored as an order-preserving bit pattern (derive-friendly).
    Real(u64),
    Text(Vec<u8>),
    Blob(Vec<u8>),
}

fn f64_to_ordered_bits(value: f64) -> u64 {
    let bits = value.to_bits();
    if bits & 0x8000_0000_0000_0000 != 0 {
        !bits
    } else {
        bits | 0x8000_0000_0000_0000
    }
}

impl Ord for SortValue {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        fn rank(value: &SortValue) -> u8 {
            match value {
                SortValue::Null => 0,
                SortValue::Integer(_) | SortValue::Real(_) => 1,
                SortValue::Text(_) => 2,
                SortValue::Blob(_) => 3,
            }
        }
        let (rank_a, rank_b) = (rank(self), rank(other));
        if rank_a != rank_b {
            return rank_a.cmp(&rank_b);
        }
        match (self, other) {
            (SortValue::Integer(a), SortValue::Integer(b)) => a.cmp(b),
            (SortValue::Real(a), SortValue::Real(b)) => a.cmp(b),
            // Mixed integer/real compares numerically, like SQLite.
            (SortValue::Integer(a), SortValue::Real(b)) => {
                f64_to_ordered_bits(*a as f64).cmp(b)
            }
            (SortValue::Real(a), SortValue::Integer(b)) => {
                a.cmp(&f64_to_ordered_bits(*b as f64))
            }
            (SortValue::Text(a), SortValue::Text(b)) => a.cmp(b),
            (SortValue::Blob(a), SortValue::Blob(b)) => a.cmp(b),
            _ => std::cmp::Ordering::Equal,
        }
    }
}

impl PartialOrd for SortValue {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

fn decode_hex_pairs(hex: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(hex.len() / 2);
    let bytes = hex.as_bytes();
    let mut pairs = bytes.chunks_exact(2);
    for pair in &mut pairs {
        let high = (pair[0] as char).to_digit(16);
        let low = (pair[1] as char).to_digit(16);
        match (high, low) {
            (Some(high), Some(low)) => out.push(((high << 4) | low) as u8),
            _ => return out,
        }
    }
    out
}

/// Sort tuple of one canonical row key: `pk:[…]` decodes per component,
/// `rowid:N` is a single integer. Suffixed duplicate keys (`#rowid:`/`#dup:`)
/// sort by their base tuple; `row:` content keys return `None` (their tables
/// take the bounded verification path).
fn sort_values_from_key(key: &str) -> Option<Vec<SortValue>> {
    fn parse(key: &str) -> Option<Vec<SortValue>> {
        if let Some(rest) = key.strip_prefix("pk:") {
            let values: Vec<Value> = serde_json::from_str(rest).ok()?;
            return Some(values.iter().map(sort_value_from_json).collect());
        }
        if let Some(rest) = key.strip_prefix("rowid:") {
            return rest
                .parse::<i64>()
                .ok()
                .map(|rowid| vec![SortValue::Integer(rowid)]);
        }
        None
    }
    if let Some(values) = parse(key) {
        return Some(values);
    }
    // Suffixed duplicate keys: retry with the suffix stripped (the marker may
    // also appear inside a text value, so cut at the last occurrence only).
    for marker in ["#rowid:", "#dup:"] {
        if let Some(position) = key.rfind(marker) {
            if let Some(values) = parse(&key[..position]) {
                return Some(values);
            }
        }
    }
    None
}

fn sort_value_from_json(value: &Value) -> SortValue {
    match value {
        Value::Null => SortValue::Null,
        Value::Number(number) => number
            .as_i64()
            .map(SortValue::Integer)
            .unwrap_or_else(|| SortValue::Real(f64_to_ordered_bits(number.as_f64().unwrap_or(0.0)))),
        Value::String(text) => SortValue::Text(text.as_bytes().to_vec()),
        Value::Object(map) => {
            let hex = map
                .get("blob_hex")
                .or_else(|| map.get("invalid_utf8_hex"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            SortValue::Blob(decode_hex_pairs(hex))
        }
        other => SortValue::Text(other.to_string().into_bytes()),
    }
}

/// Total order over sort tuples: per-column SQLite order; equal tuples (only
/// possible for duplicate primary keys) fall back to the key string, which is
/// also what the disambiguation suffixes order by within one export.
fn compare_sort_keys(a: &[SortValue], b: &[SortValue]) -> std::cmp::Ordering {
    for (x, y) in a.iter().zip(b.iter()) {
        let ordering = x.cmp(y);
        if ordering != std::cmp::Ordering::Equal {
            return ordering;
        }
    }
    a.len().cmp(&b.len())
}

/// True when any of the table's PK index columns declares a non-BINARY
/// collation: the streaming merge's byte-order comparator would then disagree
/// with SQLite's `ORDER BY`, so such tables take the bounded path. Rowid-table
/// primary keys are detectable through `index_xinfo`; WITHOUT ROWID tables
/// carry no pk index entry and rely on the merge's own monotonicity check
/// (a misaligned merge falls back to the bounded path in the same run).
fn pk_collation_non_binary(
    conn: &Connection,
    table: &str,
) -> Result<bool, SqliteHealthError> {
    let mut statement = conn
        .prepare(&format!("PRAGMA index_list({})", quote_ident(table)))
        .map_err(map_sqlite_error)?;
    let mut names: Vec<String> = Vec::new();
    let mut rows = statement.query([]).map_err(map_sqlite_error)?;
    while let Some(row) = rows.next().map_err(map_sqlite_error)? {
        // index_list: seq(0), name(1), unique(2), origin(3), partial(4)
        let origin: String = row.get(3).map_err(map_sqlite_error)?;
        if origin == "pk" || origin == "c" {
            names.push(row.get::<_, String>(1).map_err(map_sqlite_error)?);
        }
    }
    drop(rows);
    drop(statement);
    for name in names {
        let mut info = conn
            .prepare(&format!("PRAGMA index_xinfo({})", quote_ident(&name)))
            .map_err(map_sqlite_error)?;
        let mut rows = info.query([]).map_err(map_sqlite_error)?;
        while let Some(row) = rows.next().map_err(map_sqlite_error)? {
            // index_xinfo: seqno(0), cid(1), name(2), desc(3), coll(4), key(5)
            let is_key: i64 = row.get(5).map_err(map_sqlite_error)?;
            if is_key == 0 {
                continue;
            }
            let collation: String = row.get(4).map_err(map_sqlite_error)?;
            if !collation.eq_ignore_ascii_case("BINARY") {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

#[allow(clippy::too_many_arguments)]
/// Streaming merge verification for one orderable table (N5): the archived
/// rows (chunk stream + delta overlay) and the live rows (ordered source scan)
/// are walked in the same canonical order, so memory stays at one line per
/// stream plus the delta overlay — never at the table's row count.
///
/// R2: keys are compared through [`compare_sort_keys`] (typed per-column
/// SQLite order), not by key-string bytes — integer, composite and rowid keys
/// sort identically on both sides. Both streams are checked for monotonicity
/// under that comparator; any inversion (e.g. a non-BINARY collation the
/// detector missed) aborts the merge with `misaligned = true` so the caller
/// can fall back to the bounded path in the same run instead of emitting a
/// false delete/insert storm.
#[allow(clippy::too_many_arguments)]
fn verify_table_merge(
    profile: &SqliteCaptureProfile,
    archive: &Archive,
    domain: &str,
    baseline: &SqliteBaseline,
    table: &str,
    declared: &SchemaTable,
    overlay: &mut DeltaOverlay,
    conn: &Connection,
    scope_sql: &str,
    session_since_ms: Option<i64>,
    is_session_table: bool,
    scoped_child: bool,
    selected: &BTreeSet<String>,
) -> Result<(Vec<Change>, u64, bool), SqliteHealthError> {
    let mut changes: Vec<Change> = Vec::new();
    let mut mismatch_count: u64 = 0;
    let mut misaligned = false;
    let mut sql = if declared.rowid {
        format!("SELECT rowid, * FROM {}", quote_ident(table))
    } else {
        format!("SELECT * FROM {}", quote_ident(table))
    };
    sql += scope_sql;
    sql += &order_suffix(declared);
    let mut statement = conn.prepare(&sql).map_err(map_sqlite_error)?;
    let mut rows = statement.query([]).map_err(map_sqlite_error)?;
    let offset = if declared.rowid { 1 } else { 0 };
    // Duplicate primary keys are adjacent under the canonical order, so the
    // `#rowid:`/`#dup:` suffixes are assigned with O(1) state and match the
    // chunk export exactly.
    let mut last_raw_key: Option<String> = None;
    let mut dup_index = 0usize;
    let mut expected_stream = ExpectedStream::new(baseline, table);
    let mut expected = expected_stream.next(archive, domain)?;
    let mut last_source_sort: Option<Vec<SortValue>> = None;
    let mut last_expected_sort: Option<Vec<SortValue>> = None;

    while !misaligned {
        let step = rows.next().map_err(map_sqlite_error)?;
        let Some(row) = step else {
            break;
        };
        let object = row_object(row, declared, offset)?;
        // B1 scope: out-of-scope rows are invisible to verification.
        if is_session_table && session_since_ms.is_some() {
            let updated = object
                .get(profile.session_time_column)
                .and_then(Value::as_i64)
                .unwrap_or(i64::MAX);
            if updated < session_since_ms.unwrap_or(i64::MIN) {
                continue;
            }
        }
        if scoped_child {
            let session_link = profile.child_session_column(table).unwrap_or("session_id");
            let session_id = object.get(session_link).and_then(Value::as_str);
            match session_id {
                Some(id) if selected.contains(id) => {}
                _ => continue,
            }
        }
        let raw_key = row_key(row, declared, &object);
        let Some(source_sort) = sort_values_from_key(&raw_key) else {
            // Unorderable key on an orderable table: fall back rather than
            // guess an order.
            misaligned = true;
            break;
        };
        if let Some(last) = &last_source_sort {
            if compare_sort_keys(last, &source_sort) == std::cmp::Ordering::Greater {
                misaligned = true;
                break;
            }
        }
        last_source_sort = Some(source_sort.clone());
        let is_dup = last_raw_key.as_deref() == Some(raw_key.as_str());
        let (key, hash, source_row) = if is_dup {
            dup_index += 1;
            let key = disambiguate_key(
                true,
                rowid_value(row, declared),
                dup_index,
                raw_key.clone(),
            );
            let hash = row_hash(&object);
            (key, hash, Value::Object(object))
        } else {
            dup_index = 0;
            let hash = row_hash(&object);
            (raw_key.clone(), hash, Value::Object(object))
        };
        last_raw_key = Some(raw_key);

        // Merge: walk the expected stream against this source row.
        loop {
            if misaligned {
                break;
            }
            let Some((e_sort, e_key, e_hash)) = &expected else {
                // Expected exhausted: the source row is not in the evidence.
                match judge(overlay, &key, None, Some((hash.as_str(), &source_row))) {
                    Verdict::Match => {}
                    Verdict::Update(after) | Verdict::Insert(after) => {
                        mismatch_count += 1;
                        changes.push(Change {
                            table: table.to_string(),
                            key: key.clone(),
                            op: "insert",
                            before: None,
                            after: Some(after),
                        });
                    }
                    Verdict::Delete => {
                        mismatch_count += 1;
                        push_delete(&mut changes, table, key.clone());
                    }
                }
                break;
            };
            if let Some(last) = &last_expected_sort {
                if compare_sort_keys(last, e_sort) == std::cmp::Ordering::Greater {
                    misaligned = true;
                    break;
                }
            }
            last_expected_sort = Some(e_sort.clone());
            match compare_sort_keys(e_sort, &source_sort) {
                std::cmp::Ordering::Less => {
                    // Baseline-only key: absent from the live scan.
                    if !matches!(
                        judge(overlay, e_key, Some(e_hash.as_str()), None),
                        Verdict::Match
                    ) {
                        mismatch_count += 1;
                        push_delete(&mut changes, table, e_key.clone());
                    }
                    expected = expected_stream.next(archive, domain)?;
                    // stay in the inner loop: re-compare with the same source row
                }
                std::cmp::Ordering::Greater => {
                    // Source-only key (expected stream has not reached it).
                    match judge(overlay, &key, None, Some((hash.as_str(), &source_row))) {
                        Verdict::Match => {}
                        Verdict::Update(after) | Verdict::Insert(after) => {
                            mismatch_count += 1;
                            changes.push(Change {
                                table: table.to_string(),
                                key: key.clone(),
                                op: "insert",
                                before: None,
                                after: Some(after),
                            });
                        }
                        Verdict::Delete => {
                            mismatch_count += 1;
                            push_delete(&mut changes, table, key.clone());
                        }
                    }
                    break;
                }
                std::cmp::Ordering::Equal => {
                    match judge(
                        overlay,
                        e_key,
                        Some(e_hash.as_str()),
                        Some((hash.as_str(), &source_row)),
                    ) {
                        Verdict::Match => {}
                        Verdict::Update(after) => {
                            mismatch_count += 1;
                            changes.push(Change {
                                table: table.to_string(),
                                key: e_key.clone(),
                                op: "update",
                                before: None,
                                after: Some(after),
                            });
                        }
                        Verdict::Insert(after) => {
                            mismatch_count += 1;
                            changes.push(Change {
                                table: table.to_string(),
                                key: e_key.clone(),
                                op: "insert",
                                before: None,
                                after: Some(after),
                            });
                        }
                        Verdict::Delete => {
                            mismatch_count += 1;
                            push_delete(&mut changes, table, e_key.clone());
                        }
                    }
                    expected = expected_stream.next(archive, domain)?;
                    break;
                }
            }
        }
    }
    if !misaligned {
        // Drain the merge's pending entry (greater than every source row)
        // first, then the rest of the expected stream: baseline keys absent
        // from the live scan (subject to the overlay). Skipping the pending
        // entry would keep a deleted highest-key row in the evidence forever.
        let mut pending = expected.take();
        loop {
            let (e_sort, e_key, e_hash) = match pending.take() {
                Some(entry) => entry,
                None => match expected_stream.next(archive, domain)? {
                    Some(entry) => entry,
                    None => break,
                },
            };
            if let Some(last) = &last_expected_sort {
                if compare_sort_keys(last, &e_sort) == std::cmp::Ordering::Greater {
                    misaligned = true;
                    break;
                }
            }
            last_expected_sort = Some(e_sort.clone());
            if !matches!(
                judge(overlay, &e_key, Some(e_hash.as_str()), None),
                Verdict::Match
            ) {
                mismatch_count += 1;
                push_delete(&mut changes, table, e_key);
            }
        }
    }
    if !misaligned {
        // Overlay entries left over: the key is in neither stream, so the
        // evidence holds a row the source no longer has (a delete correction).
        for (key, entry) in overlay.iter() {
            if entry.is_some() {
                mismatch_count += 1;
                push_delete(&mut changes, table, key.clone());
            }
        }
    }
    if misaligned {
        changes.clear();
        mismatch_count = 0;
    }
    Ok((changes, mismatch_count, misaligned))
}

/// Bounded-map verification for tables without a canonical order (`row:hash`
/// keys): full expected map + full live read, both session-scoped. Memory is
/// proportional to the table, which is why it is a fallback, not the default.
#[allow(clippy::too_many_arguments)]
fn verify_table_bounded(
    profile: &SqliteCaptureProfile,
    archive: &Archive,
    domain: &str,
    baseline: &SqliteBaseline,
    table: &str,
    declared: &SchemaTable,
    overlay: &mut DeltaOverlay,
    conn: &Connection,
    scope_sql: &str,
    session_since_ms: Option<i64>,
    is_session_table: bool,
    scoped_child: bool,
    selected: &BTreeSet<String>,
    changes: &mut Vec<Change>,
    mismatch_count: &mut u64,
) -> Result<(), SqliteHealthError> {
    let mut expected: BTreeMap<String, String> = BTreeMap::new();
    for chunk in baseline.chunks.iter().filter(|chunk| chunk.table == table) {
        let mut stream = ChunkLineStream::open(archive, domain, chunk)?;
        while let Some((key, row)) = stream.next_row(chunk)? {
            expected.insert(key, row_hash_value(&row));
        }
    }
    for (key, entry) in overlay.iter() {
        match entry {
            Some(hash) => {
                expected.insert(key.clone(), hash.clone());
            }
            None => {
                expected.remove(key);
            }
        }
    }
    let session_filter =
        (is_session_table && session_since_ms.is_some()).then(|| session_since_ms.unwrap_or(0));
    if scoped_child && session_filter.is_none() {
        // Session scope via the selected set is applied by read_all_rows.
    }
    let live = read_all_rows(profile, conn, table, declared, session_filter, selected)?;
    for (key, row) in &live {
        let object = row.as_object().cloned().unwrap_or_default();
        let hash = row_hash(&object);
        match expected.get(key) {
            None => {
                *mismatch_count += 1;
                changes.push(Change {
                    table: table.to_string(),
                    key: key.clone(),
                    op: "insert",
                    before: None,
                    after: Some(Value::Object(object)),
                });
            }
            Some(expected_hash) if expected_hash != &hash => {
                *mismatch_count += 1;
                changes.push(Change {
                    table: table.to_string(),
                    key: key.clone(),
                    op: "update",
                    before: None,
                    after: Some(Value::Object(object)),
                });
            }
            Some(_) => {}
        }
    }
    for key in expected.keys() {
        if !live.contains_key(key) {
            *mismatch_count += 1;
            push_delete(changes, table, key.clone());
        }
    }
    let _ = scope_sql;
    Ok(())
}

/// Run the verification pass and commit correction deltas for every row whose
/// live content differs from the archived content (the watermark blind
/// window). B1: the scan reuses the capture's session scope (session tables by
/// `time_updated`, children by the selected session set), so out-of-scope rows
/// are neither corrected nor keyed. N5: orderable tables are verified by a
/// streaming merge (memory is one line per stream plus the delta overlay).
/// Never writes to the source, never copies the database, and never rewrites
/// the observation key sets — the next regular observation refreshes them from
/// the source itself, within the same scope.
#[allow(clippy::too_many_arguments)]
fn run_verification_and_correct(
    archive: &Archive,
    profile: &SqliteCaptureProfile,
    source: &Path,
    source_key: &str,
    provider: &str,
    domain: &str,
    mut state: SourcesFile,
    mut report: CaptureReport,
    baseline: &SqliteBaseline,
    now: i64,
    interval: Option<i64>,
) -> Result<CaptureReport, ArchiveError> {
    // The verification pass reads the whole scoped table set; the correction
    // delta it may commit is measured as `evidence_write`, so this guard ends
    // right before that write.
    let verify_phase = cost::phase("verify");
    let entry_state = state
        .sources
        .get(source_key)
        .and_then(|entry| entry.sqlite.clone())
        .expect("verification requires existing sqlite state");
    let session_since_ms = entry_state.session_since_ms;
    let observation_relative = if entry_state.observation_file.is_empty() {
        observation_relative_path(source_key)
    } else {
        entry_state.observation_file.clone()
    };
    let selected = load_observation_in(archive, &observation_relative, source_key, profile)?
        .map(|file| file.selected_sessions.clone())
        .unwrap_or_default();
    let batch_metas = sqlite_delta_batch_metas(archive, source_key)?;
    let mut overlays = build_delta_overlays(archive, domain, baseline, &batch_metas)?;

    let mut changes: Vec<Change> = Vec::new();
    let mut mismatch_count = 0u64;
    // R2: tables verified on the bounded path instead of the streaming merge
    // (non-BINARY pk collation, or a merge-order misalignment detected at
    // runtime) are named in the report.
    let mut verify_bounded_tables: Vec<String> = Vec::new();
    {
        let conn = open_readonly(source)?;
        in_read_transaction(&conn, |conn| {
            let schema = read_schema(conn)?;
            let whitelisted = whitelist_schema(profile, &schema);
            let filter_active = session_since_ms.is_some()
                && profile
                    .session_tables
                    .iter()
                    .any(|(table, _)| whitelisted.contains_key(*table));
            for table in profile.tables.iter().copied() {
                let Some(declared) = whitelisted.get(table) else {
                    continue;
                };
                let is_session_table = profile.is_session_table(table);
                let scoped_child = filter_active
                    && profile
                        .child_session_column(table)
                        .map(|column| declared.columns.iter().any(|c| c == column))
                        .unwrap_or(false);
                let scope_sql = if is_session_table
                    && filter_active
                    && declared
                        .columns
                        .iter()
                        .any(|c| c == profile.session_time_column)
                {
                    format!(
                        " WHERE {} >= {}",
                        quote_ident(profile.session_time_column),
                        session_since_ms.unwrap_or(0)
                    )
                } else {
                    String::new()
                };
                let overlay_snapshot = overlays.remove(table).unwrap_or_default();
                let merge_possible = orderable(declared)
                    && !pk_collation_non_binary(conn, table).map_err(|err| {
                        ArchiveError::Corrupt(format!("sqlite {}: {err}", err.code))
                    })?;
                if !merge_possible {
                    verify_bounded_tables.push(table.to_string());
                    let mut overlay = overlay_snapshot;
                    verify_table_bounded(
                        profile,
                        archive,
                        domain,
                        baseline,
                        table,
                        declared,
                        &mut overlay,
                        conn,
                        &scope_sql,
                        session_since_ms,
                        is_session_table,
                        scoped_child,
                        &selected,
                        &mut changes,
                        &mut mismatch_count,
                    )?;
                    continue;
                }
                // The merge consumes the overlay while judging; keep a snapshot
                // so a misalignment can immediately rerun the bounded path.
                let mut overlay = overlay_snapshot.clone();
                let (table_changes, table_mismatches, misaligned) = verify_table_merge(
                    profile,
                    archive,
                    domain,
                    baseline,
                    table,
                    declared,
                    &mut overlay,
                    conn,
                    &scope_sql,
                    session_since_ms,
                    is_session_table,
                    scoped_child,
                    &selected,
                )?;
                if misaligned {
                    verify_bounded_tables.push(table.to_string());
                    let mut overlay = overlay_snapshot;
                    verify_table_bounded(
                        profile,
                        archive,
                        domain,
                        baseline,
                        table,
                        declared,
                        &mut overlay,
                        conn,
                        &scope_sql,
                        session_since_ms,
                        is_session_table,
                        scoped_child,
                        &selected,
                        &mut changes,
                        &mut mismatch_count,
                    )?;
                    continue;
                }
                changes.extend(table_changes);
                mismatch_count += table_mismatches;
            }
            Ok(())
        })?;
    }

    if !changes.is_empty() {
        let mut missing_before = 0u64;
        let snapshot = archive.manifests()?;
        fill_before_from_evidence(
            archive,
            &snapshot,
            domain,
            baseline,
            &batch_metas,
            &mut changes,
            &mut missing_before,
        )?;
        drop(snapshot);
    }

    // Collector state: only the verification timestamp and the lifetime
    // counter. The observation key sets are deliberately not rewritten here —
    // a scoped verify must not key out-of-scope rows, and the next regular
    // observation rebuilds them from the source within the same scope.
    let mut sqlite = entry_state;
    sqlite.last_verify_at_ms = now;
    sqlite.verify_mismatches_total = sqlite
        .verify_mismatches_total
        .saturating_add(mismatch_count);

    if changes.is_empty() {
        let entry = entry_for(&mut state, source_key, provider);
        entry.sqlite = Some(sqlite);
        archive.save_sources(&state)?;
        if let Some(info) = report.sqlite.as_mut() {
            info.verify = true;
            info.verify_mismatches = 0;
            info.verify_bounded_tables = verify_bounded_tables.clone();
        }
        return Ok(report);
    }

    // Commit the correction delta unit.
    drop(verify_phase);
    let capture_seq = (sqlite.capture_seq + 1)
        .max(max_committed_capture_seq(archive, source_key)? + 1);
    let mut lines: Vec<DeltaLine> = Vec::with_capacity(changes.len());
    let mut inserted = 0u64;
    let mut updated = 0u64;
    let mut deleted = 0u64;
    for change in &changes {
        match change.op {
            "insert" => inserted += 1,
            "update" => updated += 1,
            "delete" => deleted += 1,
            _ => {}
        }
        lines.push(DeltaLine {
            capture_seq,
            observed_at_ms: now,
            table: change.table.clone(),
            key: change.key.clone(),
            op: change.op.to_string(),
            before: change.before.clone(),
            after: change.after.clone(),
        });
    }
    let _write_phase = cost::phase("evidence_write");
    let mut raw = Vec::new();
    for line in &lines {
        raw.extend_from_slice(&line.to_line_bytes());
    }
    let raw_len = raw.len() as u64;
    let compressed = zstd::stream::encode_all(raw.as_slice(), ZSTD_LEVEL)
        .map_err(|err| ArchiveError::Corrupt(format!("delta compression: {err}")))?;
    let (object_sha, compressed_len, is_new) = archive.put_object(&compressed)?;
    archive.read_object_bytes(&object_sha).map_err(|err| {
        ArchiveError::Integrity(format!("delta {object_sha} failed write verification: {err}"))
    })?;
    if is_new {
        report.objects_new += 1;
    } else {
        report.objects_deduped += 1;
    }
    let mut records = Vec::with_capacity(lines.len());
    let mut offset = 0u64;
    for (index, line) in lines.iter().enumerate() {
        let bytes = line.to_line_bytes();
        let content_len = bytes.len().saturating_sub(1) as u64;
        records.push(ManifestRecord {
            seq: index as u64,
            offset,
            len: content_len,
            sha256: hex_sha256(&bytes[..content_len as usize]),
            object: object_sha.clone(),
            access: vec![domain.to_string()],
            source_offset: None,
            source_line: Some(index as u64 + 1),
        });
        offset += bytes.len() as u64;
    }
    let chain_length = sqlite.delta_chain_length + 1;
    let mut delta_table_ranges: BTreeMap<String, (String, String)> = BTreeMap::new();
    for line in &lines {
        let entry = delta_table_ranges
            .entry(line.table.clone())
            .or_insert_with(|| (line.key.clone(), line.key.clone()));
        if line.key < entry.0 {
            entry.0 = line.key.clone();
        }
        if line.key > entry.1 {
            entry.1 = line.key.clone();
        }
    }
    let fingerprint = crate::capture::fingerprint(source)?;
    let manifest = Manifest {
        schema: MANIFEST_SCHEMA.to_string(),
        capture_id: new_id("cap"),
        captured_at_ms: now,
        device_id: archive.device_id()?,
        provider: provider.to_string(),
        source: ManifestSource {
            kind: "sqlite".to_string(),
            path: source_key.to_string(),
            compressed: true,
            fingerprint: fingerprint.clone(),
            source_state_at_capture: "present".to_string(),
            capture_reason: "delta".to_string(),
            generation: sqlite.baseline_generation.max(1),
            range_start: 0,
            carrier: None,
            sqlite: Some(ManifestSqlite {
                profile_id: profile.profile_id.to_string(),
                capture_seq,
                baseline_generation: sqlite.baseline_generation.max(1),
                schema_fingerprint: sqlite.schema_fingerprint.clone(),
                snapshot_reason: String::new(),
                observed_at_ms: now,
                observation_interval_ms: interval,
                rows_discovered: lines.len() as u64,
                rows_inserted: inserted,
                rows_updated: updated,
                rows_deleted: deleted,
                delta_chain_length: chain_length,
                baseline_schema: String::new(),
                baseline_digest: String::new(),
                baseline_bytes: 0,
                baseline_chunks: Vec::new(),
                baseline_columns: BTreeMap::new(),
                baseline_ddl: BTreeMap::new(),
                baseline_watermarks: BTreeMap::new(),
                delta_table_ranges,
                verify_correction: true,
                delta_schema: DELTA_LINE_SCHEMA.to_string(),
            }),
        },
        objects: vec![ManifestObject {
            sha256: object_sha.clone(),
            bytes: compressed_len,
            role: "sqlite_delta".to_string(),
            access: vec![domain.to_string()],
            source_offset: None,
            generation: sqlite.baseline_generation.max(1),
        }],
        records,
        attachments: Vec::new(),
    };
    archive.commit_manifest(&manifest)?;
    for record in &manifest.records {
        archive.stage_record_ref(
            &record.sha256,
            RecordRef {
                object: record.object.clone(),
                offset: record.offset,
                len: record.len,
                capture_id: manifest.capture_id.clone(),
            },
        )?;
    }
    archive.flush_records()?;
    // Keep the observation file's capture_seq in lockstep with the state (the
    // crash-window gate rejects a lagging seq). Only the sequence is re-sealed:
    // the keys stay exactly as the last regular observation wrote them.
    if let Some(mut file) =
        load_observation_in(archive, &observation_relative, source_key, profile)?
    {
        file.capture_seq = capture_seq;
        save_observation(archive, &observation_relative, &file)?;
    }

    sqlite.capture_seq = capture_seq;
    sqlite.delta_chain_length = chain_length;
    sqlite.delta_chain_bytes = sqlite.delta_chain_bytes.saturating_add(raw_len);
    sqlite.deltas_total += 1;
    sqlite.rows_updated_total = sqlite.rows_updated_total.saturating_add(updated);
    sqlite.rows_inserted_total = sqlite.rows_inserted_total.saturating_add(inserted);
    sqlite.rows_deleted_total = sqlite.rows_deleted_total.saturating_add(deleted);
    let entry = entry_for(&mut state, source_key, provider);
    entry.status = "captured".to_string();
    entry.change = "delta".to_string();
    entry.captured = entry.captured.saturating_add(lines.len() as u64);
    entry.bytes = entry.bytes.saturating_add(compressed_len);
    entry.anomalies = report.anomalies.clone();
    entry.sqlite = Some(sqlite.clone());
    archive.save_sources(&state)?;

    report.ok = true;
    report.status = "captured".to_string();
    report.change = "delta".to_string();
    report.capture_id = Some(manifest.capture_id.clone());
    report.sha256 = Some(object_sha.clone());
    report.generation = sqlite.baseline_generation.max(1);
    report.bytes = compressed_len;
    let info = report.sqlite.get_or_insert_with(SqliteCaptureInfo::default);
    info.mode = "delta".to_string();
    info.capture_seq = capture_seq;
    info.delta_object = Some(object_sha);
    info.delta_rows = lines.len() as u64;
    info.rows_inserted = inserted;
    info.rows_updated = updated;
    info.rows_deleted = deleted;
    info.verify = true;
    info.verify_mismatches = mismatch_count;
    info.verify_bounded_tables = verify_bounded_tables;
    info.delta_chain_length = chain_length;
    info.delta_chain_bytes = sqlite.delta_chain_bytes;
    info.baseline_digest = sqlite.baseline_digest.clone();
    info.baseline_bytes = sqlite.baseline_bytes;
    // One object-directory walk per round: inside a capture-all round the
    // reconcile is deferred to the round end (and reported there).
    if let Some(reconcile) = archive.reconcile_after_unit()? {
        report.orphan_objects = reconcile.orphan_objects;
    }
    Ok(report)
}
// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn columns(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| name.to_string()).collect()
    }

    #[test]
    fn watermark_column_priority_is_time_updated_first() {
        assert_eq!(
            watermark_column(&columns(&["id", "time_created", "time_updated"])),
            Some("time_updated".to_string())
        );
        assert_eq!(
            watermark_column(&columns(&["id", "seq", "created"])),
            Some("seq".to_string())
        );
        assert_eq!(
            watermark_column(&columns(&["id", "time_created"])),
            Some("time_created".to_string())
        );
        assert_eq!(watermark_column(&columns(&["id", "data"])), None);
    }

    #[test]
    fn whitelist_fingerprint_ignores_non_whitelist_tables() {
        let mut schema = BTreeMap::new();
        schema.insert(
            "session".to_string(),
            SchemaTable {
                columns: columns(&["id", "time_updated"]),
                primary_key: columns(&["id"]),
                rowid: true,
            },
        );
        schema.insert(
            "credential".to_string(),
            SchemaTable {
                columns: columns(&["id", "secret"]),
                primary_key: columns(&["id"]),
                rowid: true,
            },
        );
        let fingerprint = schema_fingerprint(&schema);
        schema.remove("credential").unwrap();
        assert_eq!(
            schema_fingerprint(&schema),
            fingerprint,
            "non-whitelist schema must not influence capture decisions"
        );
        // A whitelist column change does move the fingerprint.
        schema
            .get_mut("session")
            .unwrap()
            .columns
            .push("extra".to_string());
        assert_ne!(schema_fingerprint(&schema), fingerprint);
    }

    #[test]
    fn unknown_whitelist_schema_detects_new_tables_and_columns_only() {
        let mut known = BTreeMap::new();
        known.insert("part".to_string(), columns(&["id", "data"]));
        let mut schema = BTreeMap::new();
        schema.insert(
            "part".to_string(),
            SchemaTable {
                columns: columns(&["id", "data", "zz"]),
                primary_key: columns(&["id"]),
                rowid: true,
            },
        );
        assert!(unknown_whitelist_schema(&known, &schema));
        // `mystery` is not whitelisted: it never reaches this check.
        schema.insert(
            "mystery".to_string(),
            SchemaTable {
                columns: columns(&["k"]),
                primary_key: columns(&["k"]),
                rowid: true,
            },
        );
        assert!(unknown_whitelist_schema(&known, &schema));
        schema.remove("mystery").unwrap();
        schema.get_mut("part").unwrap().columns = columns(&["data", "id"]);
        assert!(!unknown_whitelist_schema(&known, &schema));
    }

    #[test]
    fn chunk_writer_splits_and_roundtrips() {
        let temp = tempfile::tempdir().unwrap();
        let archive = Archive::init(temp.path()).unwrap();
        let mut writer =
            ChunkWriter::new(&archive, "part", columns(&["id", "data"]), 64).unwrap();
        for index in 0..10 {
            let mut object = Map::new();
            object.insert("id".to_string(), Value::from(format!("p{index}")));
            object.insert("data".to_string(), Value::from("x".repeat(32)));
            writer
                .push(&format!("pk:[\"p{index}\"]"), &object)
                .unwrap();
        }
        writer.flush().unwrap();
        assert!(
            writer.chunks.len() >= 2,
            "a small chunk limit must split: {} chunks",
            writer.chunks.len()
        );
        // Chunks are content-addressed, self-describing objects; a full
        // readback through the fail-closed reader is covered by the
        // integration tests (a committed manifest is required there).
        let mut seen = 0u64;
        for exported in &writer.chunks {
            let compressed = archive
                .read_object_bytes(&exported.chunk.sha256)
                .unwrap();
            assert_eq!(compressed.len() as u64, exported.compressed_bytes);
            let raw = zstd::stream::decode_all(compressed.as_slice()).unwrap();
            assert_eq!(raw.len() as u64, exported.chunk.bytes);
            assert_eq!(hex_sha256(&raw), exported.chunk.raw_sha256);
            let header: BaselineChunkHeader =
                serde_json::from_slice(raw.split(|b| *b == b'\n').next().unwrap()).unwrap();
            assert_eq!(header.table, "part");
            assert_eq!(header.schema, BASELINE_CHUNK_SCHEMA);
            seen += exported.chunk.rows;
        }
        assert_eq!(seen, 10);
    }

    #[test]
    fn observation_state_rejects_v1_schema_explicitly() {
        let temp = tempfile::tempdir().unwrap();
        let archive = Archive::init(temp.path()).unwrap();
        let v1 = r#"{"schema":"agent-memory/sqlite-observation@1","source":"x"}"#;
        let compressed = zstd::stream::encode_all(v1.as_bytes(), 3).unwrap();
        let relative = "state/sqlite/test.obs.zst";
        std::fs::create_dir_all(archive.root().join("state/sqlite")).unwrap();
        std::fs::write(archive.root().join(relative), &compressed).unwrap();
        let err = load_observation(&archive, relative, "x").unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("sqlite-observation@1") && message.contains("not trusted"),
            "explicit version error, got: {message}"
        );
    }

    #[test]
    fn baseline_digest_depends_on_chunk_content_and_order() {
        let chunk = |sha: &str| ManifestSqliteChunk {
            table: "part".to_string(),
            sha256: sha.to_string(),
            bytes: 10,
            rows: 1,
            raw_sha256: "aa".to_string(),
            min_key: String::new(),
            max_key: String::new(),
        };
        let one = baseline_digest_of(&[chunk("a"), chunk("b")]);
        let same = baseline_digest_of(&[chunk("a"), chunk("b")]);
        let reordered = baseline_digest_of(&[chunk("b"), chunk("a")]);
        let different = baseline_digest_of(&[chunk("a"), chunk("c")]);
        assert_eq!(one, same);
        assert_ne!(one, reordered);
        assert_ne!(one, different);
    }

    #[test]
    fn duplicate_payloads_are_counted_across_tables_only() {
        let row = |id: &str, data: &str| {
            let mut object = Map::new();
            object.insert("id".to_string(), Value::from(id));
            object.insert("data".to_string(), Value::from(data));
            object
        };
        let mut tracker = DuplicateTracker::default();
        tracker.observe("part", &row("p1", "same"));
        tracker.observe("session_message", &row("m1", "same"));
        tracker.observe("session", &row("s1", "x"));
        assert_eq!(tracker.count(), 1);
        let mut single_table = DuplicateTracker::default();
        single_table.observe("part", &row("p1", "same"));
        single_table.observe("part", &row("p2", "same"));
        assert_eq!(single_table.count(), 0);
    }

    #[test]
    fn changes_match_lines_compares_content_and_order_independently() {
        let change = |table: &str, key: &str, op: &'static str, data: &str| Change {
            table: table.to_string(),
            key: key.to_string(),
            op,
            before: None,
            after: Some(serde_json::json!({ "data": data })),
        };
        let line = |table: &str, key: &str, op: &str, data: &str| DeltaLine {
            capture_seq: 2,
            observed_at_ms: 1,
            table: table.to_string(),
            key: key.to_string(),
            op: op.to_string(),
            before: None,
            after: Some(serde_json::json!({ "data": data })),
        };
        let changes = vec![
            change("part", "pk:[\"b\"]", "update", "new"),
            change("part", "pk:[\"c\"]", "insert", "x"),
        ];
        let lines = vec![
            line("part", "pk:[\"c\"]", "insert", "x"),
            line("part", "pk:[\"b\"]", "update", "new"),
        ];
        assert!(changes_match_lines(&changes, &lines));
        let other = vec![
            line("part", "pk:[\"c\"]", "insert", "x"),
            line("part", "pk:[\"b\"]", "update", "different"),
        ];
        assert!(!changes_match_lines(&changes, &other));
    }

    // -- S4.5: capture profiles ------------------------------------------------

    #[test]
    fn opencode_profile_is_item_for_item_the_previous_constants() {
        let profile = &SqliteCaptureProfile::OPENCODE;
        // Ordered whitelist = previous `SQLITE_TABLE_WHITELIST`.
        assert_eq!(
            profile.tables,
            &[
                "session",
                "message",
                "part",
                "session_v2",
                "session_message",
                "project",
                "todo"
            ][..]
        );
        // Replay order = previous `PARENT_FIRST_TABLE_ORDER`.
        assert_eq!(
            profile.replay_order,
            &[
                "project",
                "session",
                "session_v2",
                "message",
                "session_message",
                "part",
                "todo"
            ][..]
        );
        // Session tables and their key columns = the hardcoded pair set.
        assert_eq!(
            profile.session_tables,
            &[("session", "id"), ("session_v2", "id")][..]
        );
        // Session time column and watermark candidates = previous priority.
        assert_eq!(profile.session_time_column, "time_updated");
        assert_eq!(
            profile.watermark_candidates("session"),
            &["time_updated", "seq", "time_created", "created"][..]
        );
        assert_eq!(
            profile.watermark_candidates("part"),
            &["time_updated", "seq", "time_created", "created"][..]
        );
        // Child tables = previous SESSION_CHILD_TABLES, linked by session_id.
        assert_eq!(
            profile.child_session_columns,
            &[
                ("message", "session_id"),
                ("part", "session_id"),
                ("session_message", "session_id"),
                ("todo", "session_id")
            ][..]
        );
        assert_eq!(profile.child_session_column("project"), None);
        assert_eq!(profile.child_session_column("session"), None);
        assert_eq!(profile.profile_id, "opencode");
        // The compatibility aliases resolve to the same slices.
        assert_eq!(SQLITE_TABLE_WHITELIST, profile.tables);
        assert_eq!(PARENT_FIRST_TABLE_ORDER, profile.replay_order);
    }

    #[test]
    fn profile_watermark_override_beats_profile_default() {
        let profile = SqliteCaptureProfile {
            profile_id: "t",
            tables: &[],
            replay_order: &[],
            session_tables: &[],
            session_time_column: "ts",
            child_session_columns: &[],
            watermark_columns: &["updated_at_ms", "created_at_ms"],
            table_watermark_overrides: &[("rows", &["created_at_ms"])],
            excluded_tables: &[],
        };
        // Per-table override wins where present.
        assert_eq!(
            watermark_column_in(&profile, "rows", &columns(&["id", "created_at_ms"])),
            Some("created_at_ms".to_string())
        );
        // Profile default applies elsewhere (first matching candidate).
        assert_eq!(
            watermark_column_in(
                &profile,
                "other",
                &columns(&["id", "created_at_ms", "updated_at_ms"])
            ),
            Some("updated_at_ms".to_string())
        );
        assert_eq!(
            watermark_column_in(&profile, "other", &columns(&["id", "data"])),
            None
        );
    }

    #[test]
    fn profile_registry_resolves_by_provider_and_id_with_opencode_fallback() {
        static TEST_PROFILE: SqliteCaptureProfile = SqliteCaptureProfile {
            profile_id: "registry-test",
            tables: &["s"],
            replay_order: &["s"],
            session_tables: &[("s", "sid")],
            session_time_column: "updated_ms",
            child_session_columns: &[],
            watermark_columns: &["updated_ms"],
            table_watermark_overrides: &[],
            excluded_tables: &["secrets"],
        };
        register_sqlite_capture_profile("registry-test-provider", &TEST_PROFILE);
        assert_eq!(
            profile_for_provider("registry-test-provider").profile_id,
            "registry-test"
        );
        assert_eq!(
            sqlite_capture_profile_by_id("registry-test").unwrap().profile_id,
            "registry-test"
        );
        // Unknown providers and unknown ids fall back to (respectively) or
        // miss the OpenCode built-in.
        assert_eq!(profile_for_provider("never-seen-provider").profile_id, "opencode");
        assert_eq!(profile_for_provider("opencode").profile_id, "opencode");
        assert!(sqlite_capture_profile_by_id("never-registered").is_none());
        assert_eq!(
            sqlite_capture_profile_by_id("opencode").unwrap().profile_id,
            "opencode"
        );
    }

    #[test]
    fn evidence_without_profile_id_is_interpreted_as_opencode() {
        // A manifest written before S4.5 has no profileId field.
        let legacy = r#"{
            "capture_seq": 3,
            "baseline_generation": 1,
            "schema_fingerprint": "abc",
            "snapshot_reason": "initial",
            "observed_at_ms": 1
        }"#;
        let meta: ManifestSqlite = serde_json::from_str(legacy).unwrap();
        assert!(meta.profile_id.is_empty());
        let baseline = SqliteBaseline {
            capture_id: "cap_x".to_string(),
            capture_seq: 3,
            baseline_generation: 1,
            profile_id: meta.profile_id,
            observed_at_ms: 1,
            baseline_digest: "d".to_string(),
            baseline_bytes: 0,
            compressed_bytes: 0,
            schema_fingerprint: "abc".to_string(),
            snapshot_reason: "initial".to_string(),
            observation_interval_ms: None,
            rows_discovered: 0,
            chunks: Vec::new(),
            tables: BTreeMap::new(),
            columns: BTreeMap::new(),
            ddl: BTreeMap::new(),
        };
        assert_eq!(baseline.profile().profile_id, "opencode");
        // Explicit ids resolve through the same path.
        assert_eq!(
            sqlite_capture_profile_by_id("opencode").unwrap().profile_id,
            "opencode"
        );
    }

    #[test]
    fn observation_state_without_profile_id_loads_under_opencode() {
        let temp = tempfile::tempdir().unwrap();
        let archive = Archive::init(temp.path()).unwrap();
        // A sealed @2 observation file without profile_id (pre-S4.5 shape).
        let mut file = ObservationFile::new(
            &SqliteCaptureProfile::OPENCODE,
            "src",
            1,
            1,
            "fp".to_string(),
            None,
        );
        file.profile_id = String::new();
        file.seal();
        let relative = "state/sqlite/legacy.obs.zst";
        std::fs::create_dir_all(archive.root().join("state/sqlite")).unwrap();
        let json = serde_json::to_vec(&file).unwrap();
        let compressed = zstd::stream::encode_all(json.as_slice(), 3).unwrap();
        std::fs::write(archive.root().join(relative), &compressed).unwrap();
        let loaded =
            load_observation_in(&archive, relative, "src", &SqliteCaptureProfile::OPENCODE)
                .unwrap();
        assert!(loaded.is_some());
        // A state naming another profile is rejected (forces a baseline).
        let mut other = ObservationFile::new(
            &SqliteCaptureProfile::OPENCODE,
            "src2",
            1,
            1,
            "fp".to_string(),
            None,
        );
        other.profile_id = "some-other-profile".to_string();
        other.seal();
        let json = serde_json::to_vec(&other).unwrap();
        let compressed = zstd::stream::encode_all(json.as_slice(), 3).unwrap();
        std::fs::write(archive.root().join(relative), &compressed).unwrap();
        let err = load_observation_in(&archive, relative, "src2", &SqliteCaptureProfile::OPENCODE)
            .unwrap_err();
        assert!(
            err.to_string().contains("capture profile"),
            "explicit profile-mismatch error, got: {err}"
        );
    }
}
