//! Port primitives: normalized model, health counters, typed errors.

use crate::recall::refs::Locator;
use serde_json::Value;

/// Health / skip counters returned by every scan. `records` counts units that
/// were successfully parsed and considered, `skipped` counts units that could
/// not be used (bad record, oversize, path/ID mismatch), `errors` counts
/// low-level read failures, `oversize` is the subset of `skipped` that was
/// rejected because a single object exceeded the byte budget, and `bytes` is
/// the number of raw record bytes consumed for budget purposes. None of these
/// become `DB_*` codes here.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Health {
    pub records: u64,
    pub skipped: u64,
    pub errors: u64,
    pub oversize: u64,
    pub bytes: u64,
}

impl Health {
    pub fn record(&mut self, bytes: u64) {
        self.records += 1;
        self.bytes += bytes;
    }

    pub fn record_without_bytes(&mut self) {
        self.records += 1;
    }

    pub fn skip(&mut self, bytes: u64) {
        self.skipped += 1;
        self.bytes += bytes;
    }

    /// Skip an oversize object without charging its bytes to the scan budget.
    pub fn skip_without_bytes(&mut self) {
        self.skipped += 1;
    }

    /// Record a single object rejected because it exceeds the byte budget.
    pub fn note_oversize(&mut self) {
        self.skipped += 1;
        self.oversize += 1;
    }

    pub fn error(&mut self) {
        self.errors += 1;
    }

    pub fn consume(&mut self, bytes: u64) {
        self.bytes += bytes;
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionMeta {
    pub session_id: String,
    pub title: Option<String>,
    pub parent_id: Option<String>,
    pub time_created: i64,
    pub time_updated: i64,
}

impl SessionMeta {
    /// Whole-session exclusion predicate (§3.4): non-empty `parent_id`, or a
    /// title with the `ocsr-` sub-session prefix.
    pub fn is_searchable(&self) -> bool {
        let has_parent = self
            .parent_id
            .as_deref()
            .map(|parent| !parent.is_empty())
            .unwrap_or(false);
        let is_sub_session = self
            .title
            .as_deref()
            .map(|title| title.starts_with("ocsr-"))
            .unwrap_or(false);
        !has_parent && !is_sub_session
    }

    /// `title`, or the session id when the title is empty.
    pub fn display_title(&self) -> String {
        match self.title.as_deref() {
            Some(title) if !title.is_empty() => title.to_string(),
            _ => self.session_id.clone(),
        }
    }
}

/// One part's raw body, interpreted per generation.
#[derive(Debug, Clone, PartialEq)]
pub enum PartData {
    /// A part block object (`sqlite-v1` `part.data`, legacy JSON part object,
    /// or one element of a `sqlite-v2` `data.content[i]` array).
    Block(Value),
    /// The `sqlite-v2` `/text` branch: `session_message.data.text`.
    SessionText(Value),
}

#[derive(Debug, Clone, PartialEq)]
pub struct PartRecord {
    pub locator: Locator,
    pub rel_path: String,
    pub schema_gen: &'static str,
    pub data: PartData,
    /// `sha256` of the raw carrier bytes: the `data` column for SQLite, the
    /// part file bytes for legacy JSON. Warning-only, never part of `hit_id`.
    pub row_sha256: String,
    pub time_created: i64,
    pub sort_id: String,
    pub raw_bytes: u64,
}

impl PartRecord {
    pub fn session_id(&self) -> &str {
        self.locator.session_id.as_deref().unwrap_or("")
    }
}

/// Typed port failure. This layer only classifies the failure and renders a
/// human-readable message; the `DB_*` / `SCHEMA_UNSUPPORTED` code mapping and
/// the response `detail` shape belong to `providers/` (§4, P1.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PortError {
    NotFound(String),
    Locked(String),
    Corrupt(String),
    Schema { missing: Vec<String> },
    Io(String),
}

impl PortError {
    pub fn message(&self) -> String {
        match self {
            PortError::NotFound(what) => format!("database not available: {what}"),
            PortError::Locked(what) => format!("database is locked: {what}"),
            PortError::Corrupt(what) => format!("database is corrupt: {what}"),
            PortError::Schema { missing } => {
                format!("schema is not supported: missing {}", missing.join(", "))
            }
            PortError::Io(what) => format!("i/o failure: {what}"),
        }
    }
}

impl std::fmt::Display for PortError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message())
    }
}

impl std::error::Error for PortError {}

/// Map a `rusqlite` failure to a typed port error. Lock/busy, corrupt and
/// cannot-open are distinct; everything else is reported as corruption rather
/// than being flattened into an empty result.
pub fn map_sqlite_error(err: rusqlite::Error) -> PortError {
    match &err {
        rusqlite::Error::SqliteFailure(inner, message) => {
            let detail = message.clone().unwrap_or_else(|| inner.to_string());
            match inner.code {
                rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked => {
                    PortError::Locked(detail)
                }
                rusqlite::ErrorCode::NotADatabase
                | rusqlite::ErrorCode::DatabaseCorrupt => PortError::Corrupt(detail),
                rusqlite::ErrorCode::CannotOpen => PortError::NotFound(detail),
                _ => PortError::Corrupt(detail),
            }
        }
        rusqlite::Error::InvalidPath(path) => PortError::NotFound(path.display().to_string()),
        other => PortError::Corrupt(other.to_string()),
    }
}
