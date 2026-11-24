//! §G.1 equivalence oracle: a **read-only, normalised semantic digest** of one
//! source's live full-text index.
//!
//! The digest answers one question only: do an incrementally maintained index
//! and a freshly rebuilt index over the same frozen input hold the same
//! content? It never writes a file, never takes the index write lock and never
//! changes a query result.
//!
//! # Normalisation (the exemption list)
//!
//! Only these differences are ignored, and each is machine-checkable:
//!
//! * **row order** — every table is reduced to a sorted line multiset before
//!   hashing (a rebuild may insert rows in a different order);
//! * **insert-time rowids** — `rowid` (and `doc_keys.rowid`, which is an
//!   insertion counter) never enters a digest; `doc_key` is the stable identity;
//! * **time fields** — `meta.built_at_ms`, `meta.newest_unit_ts` and the
//!   capture-side `captured_at_ms` are not part of any digest (see
//!   [`DIGEST_EXEMPT_FIELDS`]);
//! * **vector floats** — vectors are compared through their quantised bytes and
//!   their `doc_key → input_sha256` mapping, never through raw `f32` values.
//!
//! Everything else is content: `units` (every column), `doc_keys`,
//! `conversation_units`, `sessions`, `archive_sessions`, and the vector rows.
//!
//! # Shape
//!
//! Per source: `{source_id, status, reason?, units, doc_keys, conversation_units,
//! sessions, archive_sessions, vectors}` where each table entry is
//! `{count, digest}` (plus a per-key map for `sessions` / `archive_sessions`,
//! which is small and makes a mismatch localisable) and `vectors` is
//! `{status, count, generation_key, dimension, doc_key_input, input_quantized}`.

use std::collections::BTreeMap;

use rusqlite::types::Value as SqlValue;
use rusqlite::Connection;
use serde::Serialize;

use crate::recall::hash::sha256_hex;
use crate::recall::RecallConfig;

use super::vectors::{self, VectorDigest};
use super::{configured_cache_root, open_for_query, IndexState, SourceConfig};

/// Fields that are allowed to differ between two equivalent builds. They are
/// listed here once so the exemption is part of the code, not of a reviewer's
/// memory.
pub const DIGEST_EXEMPT_FIELDS: [&str; 5] = [
    "meta.built_at_ms",
    "meta.newest_unit_ts",
    "row order",
    "rowid (insertion counter)",
    "vector float values (quantised bytes are compared)",
];

/// One table's normalised content.
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct TableDigest {
    pub count: u64,
    pub digest: String,
    /// Small tables (sessions, archive_sessions) also expose a per-key digest so
    /// a mismatch names the row instead of only the table.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub rows: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SourceDigest {
    pub source_id: String,
    /// `ready` | `missing` | `stale` | `corrupt` | `not_indexable`
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub units: Option<TableDigest>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub doc_keys: Option<TableDigest>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conversation_units: Option<TableDigest>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sessions: Option<TableDigest>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub archive_sessions: Option<TableDigest>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vectors: Option<VectorDigest>,
}

impl SourceDigest {
    fn unavailable(source_id: &str, status: &str, reason: &str) -> SourceDigest {
        SourceDigest {
            source_id: source_id.to_string(),
            status: status.to_string(),
            reason: Some(reason.to_string()),
            units: None,
            doc_keys: None,
            conversation_units: None,
            sessions: None,
            archive_sessions: None,
            vectors: None,
        }
    }

    /// The tables that exist in this digest, for a per-table diff report.
    pub fn tables(&self) -> Vec<(&'static str, Option<&TableDigest>)> {
        vec![
            ("units", self.units.as_ref()),
            ("doc_keys", self.doc_keys.as_ref()),
            ("conversation_units", self.conversation_units.as_ref()),
            ("sessions", self.sessions.as_ref()),
            ("archive_sessions", self.archive_sessions.as_ref()),
        ]
    }
}

/// One canonical cell. Text and blobs are length-prefixed hashes (the digest is
/// a comparison, not a dump); reals compare bit-exactly.
fn canon(value: SqlValue) -> String {
    match value {
        SqlValue::Null => "z".to_string(),
        SqlValue::Integer(number) => format!("i{number}"),
        SqlValue::Real(number) => format!("r{:016x}", number.to_bits()),
        SqlValue::Text(text) => format!("t{}:{}", text.len(), sha256_hex(text.as_bytes())),
        SqlValue::Blob(bytes) => format!("b{}:{}", bytes.len(), sha256_hex(&bytes)),
    }
}

fn table_digest(conn: &Connection, table: &str, columns: &str) -> rusqlite::Result<TableDigest> {
    let mut statement = conn.prepare(&format!("SELECT {columns} FROM {table}"))?;
    let width = statement.column_count();
    let mut rows = statement.query([])?;
    let mut lines: Vec<String> = Vec::new();
    while let Some(row) = rows.next()? {
        let mut line = String::new();
        for index in 0..width {
            if index > 0 {
                line.push('\u{1f}');
            }
            line.push_str(&canon(row.get::<_, SqlValue>(index)?));
        }
        lines.push(line);
    }
    Ok(digest_of(lines))
}

fn digest_of(mut lines: Vec<String>) -> TableDigest {
    lines.sort();
    let mut hasher = String::with_capacity(lines.len() * 65);
    for line in &lines {
        hasher.push_str(line);
        hasher.push('\n');
    }
    TableDigest {
        count: lines.len() as u64,
        digest: sha256_hex(hasher.as_bytes()),
        rows: BTreeMap::new(),
    }
}

/// A per-key map for the small tables: `key → sha256(canonical row)`. One pass
/// over the table (the key is the first selected column), so no second query
/// can depend on the source's scan order.
fn keyed_digest(conn: &Connection, table: &str, columns: &str) -> rusqlite::Result<TableDigest> {
    let mut statement = conn.prepare(&format!("SELECT {columns} FROM {table}"))?;
    let width = statement.column_count();
    let mut rows = statement.query([])?;
    let mut map: BTreeMap<String, String> = BTreeMap::new();
    let mut lines: Vec<String> = Vec::new();
    while let Some(row) = rows.next()? {
        let mut canonical = String::new();
        for column in 0..width {
            if column > 0 {
                canonical.push('\u{1f}');
            }
            canonical.push_str(&canon(row.get::<_, SqlValue>(column)?));
        }
        let key = canon(row.get::<_, SqlValue>(0)?);
        map.insert(key, sha256_hex(canonical.as_bytes()));
        lines.push(canonical);
    }
    let mut table_digest = digest_of(lines);
    table_digest.rows = map;
    Ok(table_digest)
}

/// The digest of one source. `ready` means every table was read; a source
/// whose index is missing/stale/corrupt is reported by status and never fails
/// the command (the caller decides).
pub fn source_digest(config: &RecallConfig, source: &SourceConfig) -> SourceDigest {
    if !source.kind.is_indexable() {
        return SourceDigest::unavailable(
            &source.source_id,
            "not_indexable",
            "this source kind has no live index",
        );
    }
    let (conn, path, state) = open_for_query(config, source);
    let (status, reason) = match &state {
        IndexState::Ready => ("ready", None),
        IndexState::Missing => ("missing", state.reason()),
        IndexState::Stale(why) => ("stale", Some(*why)),
        IndexState::Corrupt(why) => ("corrupt", Some(*why)),
    };
    if status != "ready" {
        return SourceDigest {
            source_id: source.source_id.clone(),
            status: status.to_string(),
            reason: reason.map(str::to_string),
            units: None,
            doc_keys: None,
            conversation_units: None,
            sessions: None,
            archive_sessions: None,
            vectors: None,
        };
    }
    let units = table_digest(
        &conn,
        "units",
        "text, tool_input, tool_output, source_id, session_id, message_id, record_index, \
         pointer, locator, rel_path, ts, role, kind, session_kind, title, range_len, \
         range_lines, range_sha256, row_sha256, origin",
    )
    .map_err(|error| format!("{}: units: {error}", path.display()));
    let doc_keys = table_digest(
        &conn,
        "doc_keys",
        "doc_key, session_id, message_id, rel_path, digest, origin",
    )
    .map_err(|error| format!("{}: doc_keys: {error}", path.display()));
    let conversation_units = table_digest(
        &conn,
        "conversation_units",
        "doc_key, text_sha256, session_id, message_id, ts, role, session_kind",
    )
    .map_err(|error| format!("{}: conversation_units: {error}", path.display()));
    let sessions = keyed_digest(&conn, "sessions", "session_id, title, session_kind")
        .map_err(|error| format!("{}: sessions: {error}", path.display()));
    let archive_sessions = keyed_digest(
        &conn,
        "archive_sessions",
        "session_id, rule_version, projected_units, carrier_fingerprint",
    )
    .map_err(|error| format!("{}: archive_sessions: {error}", path.display()));

    let cache_root = configured_cache_root(config);
    let vectors = cache_root
        .ok()
        .map(|root| vectors::digest_file(&super::vectors_path(&root, &source.source_id)));

    let mut result = SourceDigest {
        source_id: source.source_id.clone(),
        status: "ready".to_string(),
        reason: None,
        units: None,
        doc_keys: None,
        conversation_units: None,
        sessions: None,
        archive_sessions: None,
        vectors,
    };
    match (units, doc_keys, conversation_units, sessions, archive_sessions) {
        (Ok(units), Ok(doc_keys), Ok(conversation_units), Ok(sessions), Ok(archive_sessions)) => {
            result.units = Some(units);
            result.doc_keys = Some(doc_keys);
            result.conversation_units = Some(conversation_units);
            result.sessions = Some(sessions);
            result.archive_sessions = Some(archive_sessions);
        }
        (units, doc_keys, conversation_units, sessions, archive_sessions) => {
            result.status = "corrupt".to_string();
            result.reason = [
                units.err(),
                doc_keys.err(),
                conversation_units.err(),
                sessions.err(),
                archive_sessions.err(),
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .first()
            .cloned();
        }
    }
    result
}

/// The digests of every configured source (or the one named by `only`), in
/// configuration order.
pub fn digest_sources(config: &RecallConfig, only: Option<&str>) -> Vec<SourceDigest> {
    config
        .sources
        .iter()
        .filter(|source| only.map(|id| id == source.source_id).unwrap_or(true))
        .map(|source| source_digest(config, source))
        .collect()
}

/// A per-table, per-row diff between two digests of the same source. Empty when
/// they are equivalent.
pub fn diff(left: &SourceDigest, right: &SourceDigest) -> Vec<String> {
    let mut out = Vec::new();
    if left.status != right.status {
        out.push(format!("status: {} != {}", left.status, right.status));
        return out;
    }
    for (name, table) in left.tables() {
        let other = right
            .tables()
            .into_iter()
            .find(|(candidate, _)| *candidate == name)
            .and_then(|(_, value)| value);
        match (table, other) {
            (None, None) => {}
            (Some(a), Some(b)) if a.digest == b.digest && a.count == b.count => {
                for (key, value) in &a.rows {
                    match b.rows.get(key) {
                        Some(other) if other == value => {}
                        Some(_) => out.push(format!("{name}[{key}]: content differs")),
                        None => out.push(format!("{name}[{key}]: missing on the right")),
                    }
                }
                for key in b.rows.keys() {
                    if !a.rows.contains_key(key) {
                        out.push(format!("{name}[{key}]: missing on the left"));
                    }
                }
            }
            (Some(a), Some(b)) => out.push(format!(
                "{name}: digest {} ({} rows) != {} ({} rows)",
                a.digest, a.count, b.digest, b.count
            )),
            (Some(_), None) => out.push(format!("{name}: present only on the left")),
            (None, Some(_)) => out.push(format!("{name}: present only on the right")),
        }
    }
    match (&left.vectors, &right.vectors) {
        (None, None) => {}
        (Some(a), Some(b)) if a == b => {}
        (Some(a), Some(b)) => {
            if a.count != b.count {
                out.push(format!(
                    "vectors: row count {} != {}",
                    a.count, b.count
                ));
            }
            if a.doc_key_input != b.doc_key_input {
                let differing = a
                    .doc_key_input
                    .iter()
                    .filter(|(key, value)| b.doc_key_input.get(*key) != Some(value))
                    .count();
                out.push(format!(
                    "vectors: doc_key -> input_sha256 differs in {differing} entries"
                ));
            }
            if a.input_quantized != b.input_quantized {
                out.push("vectors: quantised bytes differ for the same input hash".to_string());
            }
        }
        (Some(_), None) => out.push("vectors: present only on the left".to_string()),
        (None, Some(_)) => out.push("vectors: present only on the right".to_string()),
    }
    out
}
