//! SQLite V2 (`session_v2` / `session_message`) reader.
//!
//! Parts come from inline `session_message.data.content[i]` or `data.text`.
//! Assistant rows with no inline content can use ordered `session_part` text
//! rows instead. The legacy `message` / `part` tables are never read here.

use crate::recall::contract::SCHEMA_SQLITE_V2;
use crate::recall::port::model::{
    map_sqlite_error, Health, PartData, PartRecord, PortError, SessionMeta,
};
use crate::recall::port::probe::{open_readonly, require_v2};
use crate::recall::refs::{parse_content_pointer, Locator};
use rusqlite::types::ValueRef;
use rusqlite::Connection;
use serde_json::Value;
use std::path::Path;

#[path = "sqlite_v2_parts.rs"]
pub(crate) mod session_parts;

use self::session_parts::TextPart;

pub struct V2Reader {
    conn: Connection,
    db_rel_path: String,
    parent_id_available: bool,
    has_session_parts: bool,
}

/// Hard safety cap for one `session_message` row. Budget-level oversize is
/// decided per part (`content[i]`); this cap only prevents fetching a
/// pathological row.
pub const MAX_ROW_BYTES: u64 = 64 * 1024 * 1024;

impl V2Reader {
    pub fn open(
        path: &Path,
        db_rel_path: &str,
        parent_id_available: bool,
    ) -> Result<Self, PortError> {
        let conn = open_readonly(path)?;
        require_v2(&conn)?;
        let has_session_parts = session_parts::available(&conn).map_err(map_sqlite_error)?;
        Ok(V2Reader {
            conn,
            db_rel_path: db_rel_path.to_string(),
            parent_id_available,
            has_session_parts,
        })
    }

    pub fn sessions(&self) -> Result<Vec<SessionMeta>, PortError> {
        let sql = if self.parent_id_available {
            "SELECT id, title, parent_id, time_created, time_updated FROM session_v2"
        } else {
            "SELECT id, title, time_created, time_updated FROM session_v2"
        };
        let mut statement = self.conn.prepare(sql).map_err(map_sqlite_error)?;
        let rows = statement
            .query_map([], |row| {
                if self.parent_id_available {
                    Ok(SessionMeta {
                        session_id: row.get::<_, String>(0)?,
                        title: row.get::<_, Option<String>>(1)?,
                        parent_id: row.get::<_, Option<String>>(2)?,
                        time_created: row.get::<_, Option<i64>>(3)?.unwrap_or(0),
                        time_updated: row.get::<_, Option<i64>>(4)?.unwrap_or(0),
                    })
                } else {
                    Ok(SessionMeta {
                        session_id: row.get::<_, String>(0)?,
                        title: row.get::<_, Option<String>>(1)?,
                        parent_id: None,
                        time_created: row.get::<_, Option<i64>>(2)?.unwrap_or(0),
                        time_updated: row.get::<_, Option<i64>>(3)?.unwrap_or(0),
                    })
                }
            })
            .map_err(map_sqlite_error)?;
        let mut sessions = Vec::new();
        for session in rows {
            sessions.push(session.map_err(map_sqlite_error)?);
        }
        sessions.sort_by(|a, b| {
            b.time_updated
                .cmp(&a.time_updated)
                .then_with(|| a.session_id.cmp(&b.session_id))
        });
        Ok(sessions)
    }

    /// Parts of one session: `session_message` rows by `(seq, id)`, then
    /// inline parts by array index, or side-table text parts by `(ordinal, id)`.
    pub fn parts_for_session(
        &self,
        session_id: &str,
        _max_object_bytes: u64,
        health: &mut Health,
    ) -> Result<Vec<PartRecord>, PortError> {
        let oversize_rows: i64 = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM session_message WHERE session_id = ?1 \
                 AND data IS NOT NULL AND length(data) > ?2",
                rusqlite::params![session_id, MAX_ROW_BYTES],
                |row| row.get(0),
            )
            .map_err(map_sqlite_error)?;
        for _ in 0..oversize_rows.max(0) {
            health.note_oversize();
        }

        let mut statement = self
            .conn
            .prepare(
                "SELECT id, seq, time_created, data, type FROM session_message \
                 WHERE session_id = ?1 AND (data IS NULL OR length(data) <= ?2) \
                 ORDER BY seq, id",
            )
            .map_err(map_sqlite_error)?;
        let rows = statement
            .query_map(rusqlite::params![session_id, MAX_ROW_BYTES], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<i64>>(1)?.unwrap_or(0),
                    row.get::<_, Option<i64>>(2)?.unwrap_or(0),
                    raw_bytes(row.get_ref(3)?).to_vec(),
                    row.get::<_, Option<String>>(4)?.unwrap_or_default(),
                ))
            })
            .map_err(map_sqlite_error)?;
        let mut parts = Vec::new();
        for row in rows {
            let (message_id, seq, time_created, bytes, message_type) =
                row.map_err(map_sqlite_error)?;
            let raw_len = bytes.len() as u64;
            let value: Value = match serde_json::from_slice::<Value>(&bytes) {
                Ok(value) if value.is_object() => value,
                _ => {
                    health.skip(raw_len);
                    continue;
                }
            };
            if self.has_session_parts && session_parts::is_fallback(&message_type, &value) {
                session_parts::visit_message(&self.conn, session_id, &message_id, |part| {
                    if let Some(value) = part.value() {
                        health.record(part.bytes.len() as u64);
                        parts.push(self.session_part_record(
                            Locator::v2_part(session_id, &message_id, &part.id),
                            &part,
                            seq,
                            time_created,
                            value,
                        ));
                    } else {
                        health.skip(part.bytes.len() as u64);
                    }
                    true
                })
                .map_err(map_sqlite_error)?;
                health.record(raw_len);
                continue;
            }
            let row_sha256 = crate::recall::hash::sha256_hex(&bytes);
            match value.get("content") {
                Some(Value::Array(items)) => {
                    for (index, item) in items.iter().enumerate() {
                        let pointer = format!("/content/{index}");
                        // Oversize is a per-part decision in V2: one huge row
                        // with small parts must not poison those parts.
                        let part_bytes = serde_json::to_vec(item)
                            .map(|bytes| bytes.len() as u64)
                            .unwrap_or(0);
                        parts.push(PartRecord {
                            locator: Locator::v2(session_id, &message_id, &pointer),
                            rel_path: self.db_rel_path.clone(),
                            schema_gen: SCHEMA_SQLITE_V2,
                            data: PartData::Block(item.clone()),
                            row_sha256: row_sha256.clone(),
                            time_created,
                            sort_id: format!("{seq:020}:{index:06}"),
                            raw_bytes: part_bytes,
                        });
                    }
                    health.record(raw_len);
                }
                Some(_) => health.skip(raw_len),
                None => match value.get("text") {
                    Some(text) => {
                        let part_bytes = match text {
                            Value::String(text) => text.len() as u64,
                            other => serde_json::to_vec(other)
                                .map(|bytes| bytes.len() as u64)
                                .unwrap_or(0),
                        };
                        parts.push(PartRecord {
                            locator: Locator::v2(session_id, &message_id, "/text"),
                            rel_path: self.db_rel_path.clone(),
                            schema_gen: SCHEMA_SQLITE_V2,
                            data: PartData::SessionText(text.clone()),
                            row_sha256,
                            time_created,
                            sort_id: format!("{seq:020}:000000"),
                            raw_bytes: part_bytes,
                        });
                        health.record(raw_len);
                    }
                    None => health.skip(raw_len),
                },
            }
        }
        Ok(parts)
    }

    pub fn load_part(&self, locator: &Locator) -> Result<PartRecord, PortError> {
        let session_id = locator.session_id.as_deref().unwrap_or("");
        let message_id = locator.session_message_id.as_deref().unwrap_or("");
        let pointer = locator.pointer.as_deref().unwrap_or("");
        if session_id.is_empty() || message_id.is_empty() {
            return Err(PortError::NotFound("incomplete v2 locator".into()));
        }
        if let Some(part_id) = locator.part_id.as_deref() {
            if !self.has_session_parts || pointer != "/text" {
                return Err(PortError::NotFound(
                    "unavailable session_part locator".into(),
                ));
            }
            let (part, seq, time_created) =
                session_parts::load_part(&self.conn, session_id, message_id, part_id)
                    .map_err(map_sqlite_error)?
                    .ok_or_else(|| {
                        PortError::NotFound(format!(
                            "session_part {part_id} of message {message_id} in session {session_id}"
                        ))
                    })?;
            // Even malformed or oversized rewrites must reach the raw row
            // fingerprint check before projection rejects their new content.
            let data = part.value().unwrap_or(Value::Null);
            return Ok(self.session_part_record(locator.clone(), &part, seq, time_created, data));
        }
        let index = parse_content_pointer(pointer)
            .ok_or_else(|| PortError::NotFound(format!("unsupported pointer {pointer}")))?;
        let mut statement = self
            .conn
            .prepare(
                "SELECT seq, time_created, data FROM session_message \
                 WHERE id = ?1 AND session_id = ?2",
            )
            .map_err(map_sqlite_error)?;
        let mut rows = statement
            .query_map([message_id, session_id], |row| {
                Ok((
                    row.get::<_, Option<i64>>(0)?.unwrap_or(0),
                    row.get::<_, Option<i64>>(1)?.unwrap_or(0),
                    raw_bytes(row.get_ref(2)?).to_vec(),
                ))
            })
            .map_err(map_sqlite_error)?;
        match rows.next() {
            None => Err(PortError::NotFound(format!(
                "session_message {message_id} of session {session_id}"
            ))),
            Some(row) => {
                let (seq, time_created, bytes) = row.map_err(map_sqlite_error)?;
                let value: Value = serde_json::from_slice(&bytes).map_err(|err| {
                    PortError::Corrupt(format!(
                        "session_message {message_id} data is not JSON: {err}"
                    ))
                })?;
                let (data, sort_suffix) = match index {
                    None => {
                        let text = value
                            .get("text")
                            .cloned()
                            .ok_or_else(|| PortError::NotFound("/text branch missing".into()))?;
                        (PartData::SessionText(text), 0usize)
                    }
                    Some(index) => {
                        let item = value
                            .get("content")
                            .and_then(Value::as_array)
                            .and_then(|items| items.get(index))
                            .cloned()
                            .ok_or_else(|| {
                                PortError::NotFound(format!("/content/{index} missing"))
                            })?;
                        (PartData::Block(item), index)
                    }
                };
                Ok(PartRecord {
                    locator: locator.clone(),
                    rel_path: self.db_rel_path.clone(),
                    schema_gen: SCHEMA_SQLITE_V2,
                    data,
                    row_sha256: crate::recall::hash::sha256_hex(&bytes),
                    time_created,
                    sort_id: format!("{seq:020}:{sort_suffix:06}"),
                    raw_bytes: bytes.len() as u64,
                })
            }
        }
    }

    fn session_part_record(
        &self,
        locator: Locator,
        part: &TextPart,
        seq: i64,
        time_created: i64,
        value: Value,
    ) -> PartRecord {
        PartRecord {
            locator,
            rel_path: self.db_rel_path.clone(),
            schema_gen: SCHEMA_SQLITE_V2,
            data: PartData::Block(value),
            row_sha256: crate::recall::hash::sha256_hex(&part.bytes),
            time_created,
            sort_id: format!("{seq:020}:{:020}:{}", part.ordinal, part.id),
            raw_bytes: part.bytes.len() as u64,
        }
    }
}

fn raw_bytes(value: ValueRef<'_>) -> &[u8] {
    match value {
        ValueRef::Blob(bytes) => bytes,
        ValueRef::Text(bytes) => bytes,
        _ => &[],
    }
}
