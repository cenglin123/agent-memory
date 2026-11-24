//! SQLite V1 (`session` / `message` / `part`) reader.

use crate::recall::contract::SCHEMA_SQLITE_V1;
use crate::recall::port::model::{map_sqlite_error, Health, PartData, PartRecord, PortError, SessionMeta};
use crate::recall::port::probe::{open_readonly, require_v1};
use crate::recall::refs::Locator;
use rusqlite::types::ValueRef;
use rusqlite::Connection;
use std::path::Path;

pub struct V1Reader {
    conn: Connection,
    db_rel_path: String,
}

impl V1Reader {
    pub fn open(path: &Path, db_rel_path: &str) -> Result<Self, PortError> {
        let conn = open_readonly(path)?;
        require_v1(&conn)?;
        Ok(V1Reader {
            conn,
            db_rel_path: db_rel_path.to_string(),
        })
    }

    pub fn sessions(&self) -> Result<Vec<SessionMeta>, PortError> {
        let mut statement = self
            .conn
            .prepare(
                "SELECT id, title, parent_id, time_created, time_updated FROM session",
            )
            .map_err(map_sqlite_error)?;
        let rows = statement
            .query_map([], |row| {
                Ok(SessionMeta {
                    session_id: row.get::<_, String>(0)?,
                    title: row.get::<_, Option<String>>(1)?,
                    parent_id: row.get::<_, Option<String>>(2)?,
                    time_created: row.get::<_, Option<i64>>(3)?.unwrap_or(0),
                    time_updated: row.get::<_, Option<i64>>(4)?.unwrap_or(0),
                })
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

    /// All parts of one session in the fixed order: message `(time_created, id)`
    /// ascending, then part `(time_created, id)` ascending.
    ///
    /// A single session-scoped join keeps the scan linear; querying per message
    /// would rescan the part table once per message.
    pub fn parts_for_session(
        &self,
        session_id: &str,
        max_object_bytes: u64,
        health: &mut Health,
    ) -> Result<Vec<PartRecord>, PortError> {
        // Count oversize parts without fetching their `data` column, then read
        // only the parts that fit the byte budget.
        let oversize: i64 = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM part WHERE session_id = ?1 \
                 AND data IS NOT NULL AND length(data) > ?2",
                rusqlite::params![session_id, max_object_bytes],
                |row| row.get(0),
            )
            .map_err(map_sqlite_error)?;
        for _ in 0..oversize.max(0) {
            health.note_oversize();
        }

        let mut statement = self
            .conn
            .prepare(
                "SELECT p.id, p.message_id, p.session_id, p.time_created, p.data \
                 FROM part p LEFT JOIN message m ON m.id = p.message_id \
                 WHERE p.session_id = ?1 AND (p.data IS NULL OR length(p.data) <= ?2) \
                 ORDER BY COALESCE(m.time_created, 0), p.message_id, p.time_created, p.id",
            )
            .map_err(map_sqlite_error)?;
        let rows = statement
            .query_map(rusqlite::params![session_id, max_object_bytes], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<i64>>(3)?.unwrap_or(0),
                    raw_bytes(row.get_ref(4)?).to_vec(),
                ))
            })
            .map_err(map_sqlite_error)?;
        let mut parts = Vec::new();
        for row in rows {
            let (part_id, message_id, row_session_id, time_created, bytes) =
                row.map_err(map_sqlite_error)?;
            let raw_len = bytes.len() as u64;
            if part_id.is_empty() || row_session_id != session_id {
                health.skip(raw_len);
                continue;
            }
            match serde_json::from_slice::<serde_json::Value>(&bytes) {
                Ok(value) if value.is_object() => {
                    health.record(raw_len);
                    parts.push(PartRecord {
                        locator: Locator::v1(session_id, &message_id, &part_id),
                        rel_path: self.db_rel_path.clone(),
                        schema_gen: SCHEMA_SQLITE_V1,
                        data: PartData::Block(value),
                        row_sha256: crate::recall::hash::sha256_hex(&bytes),
                        time_created,
                        sort_id: part_id,
                        raw_bytes: raw_len,
                    });
                }
                Ok(_) => health.skip(raw_len),
                Err(_) => health.skip(raw_len),
            }
        }
        Ok(parts)
    }

    /// Load exactly one part by locator (no session-wide scan).
    pub fn load_part(&self, locator: &Locator) -> Result<PartRecord, PortError> {
        let session_id = locator.session_id.as_deref().unwrap_or("");
        let message_id = locator.message_id.as_deref().unwrap_or("");
        let part_id = locator.part_id.as_deref().unwrap_or("");
        if session_id.is_empty() || message_id.is_empty() || part_id.is_empty() {
            return Err(PortError::NotFound("incomplete v1 locator".into()));
        }
        let mut statement = self
            .conn
            .prepare(
                "SELECT time_created, data FROM part \
                 WHERE id = ?1 AND message_id = ?2 AND session_id = ?3",
            )
            .map_err(map_sqlite_error)?;
        let mut rows = statement
            .query_map([part_id, message_id, session_id], |row| {
                Ok((
                    row.get::<_, Option<i64>>(0)?.unwrap_or(0),
                    raw_bytes(row.get_ref(1)?).to_vec(),
                ))
            })
            .map_err(map_sqlite_error)?;
        match rows.next() {
            None => Err(PortError::NotFound(format!(
                "part {part_id} in message {message_id} of session {session_id}"
            ))),
            Some(row) => {
                let (time_created, bytes) = row.map_err(map_sqlite_error)?;
                let value: serde_json::Value =
                    serde_json::from_slice(&bytes).map_err(|err| {
                        PortError::Corrupt(format!("part {part_id} data is not JSON: {err}"))
                    })?;
                Ok(PartRecord {
                    locator: Locator::v1(session_id, message_id, part_id),
                    rel_path: self.db_rel_path.clone(),
                    schema_gen: SCHEMA_SQLITE_V1,
                    data: PartData::Block(value),
                    row_sha256: crate::recall::hash::sha256_hex(&bytes),
                    time_created,
                    sort_id: part_id.to_string(),
                    raw_bytes: bytes.len() as u64,
                })
            }
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

