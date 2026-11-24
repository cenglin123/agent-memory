//! Optional SQLite V2 text-sidecar storage shared by the provider and recall.

use rusqlite::{params, Connection, OptionalExtension, Row};
use serde_json::Value;

pub(crate) const TABLE: &str = "session_part";
pub(crate) const MAX_DATA_BYTES: u64 = 64 * 1024 * 1024;

pub(crate) fn available(conn: &Connection) -> rusqlite::Result<bool> {
    // The live OpenCode schema has no `id` column: a part is identified by
    // (message_id, ordinal, type). Only the columns we actually read are
    // required, so both the shipped schema and any variant that adds an id
    // column are accepted.
    conn.query_row(
        "SELECT COUNT(*) = 5 FROM pragma_table_info('session_part')
         WHERE name IN ('message_id', 'session_id', 'ordinal', 'type', 'data')",
        [],
        |row| row.get(0),
    )
}

/// A nonempty content array wins even when none of its items are usable text.
/// With no content key, preserve the existing inline text representation too.
pub(crate) fn is_fallback(message_type: &str, message: &Value) -> bool {
    message_type == "assistant"
        && message.is_object()
        && !crate::recall::port::project::is_injected(message)
        && match message.get("content") {
            Some(Value::Array(items)) => items.is_empty(),
            None | Some(Value::Null) => !message
                .get("text")
                .and_then(Value::as_str)
                .is_some_and(|text| !text.is_empty()),
            _ => false,
        }
}

pub(crate) struct TextPart {
    pub id: String,
    pub ordinal: i64,
    pub bytes: Vec<u8>,
}

impl TextPart {
    pub(crate) fn value(&self) -> Option<Value> {
        let value: Value = serde_json::from_slice(&self.bytes).ok()?;
        (value.get("type").and_then(Value::as_str) == Some("text")
            && !crate::recall::port::project::is_injected(&value)
            && value.get("ignored").and_then(Value::as_bool) != Some(true)
            && value
                .get("text")
                .and_then(Value::as_str)
                .is_some_and(|text| !text.trim().is_empty()))
        .then_some(value)
    }

    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        let ordinal: i64 = row.get(0)?;
        Ok(Self {
            // Identity within a message is the ordinal; the shipped schema has
            // no part id column, so the locator carries the ordinal as text.
            id: ordinal.to_string(),
            ordinal,
            bytes: row.get::<_, Option<Vec<u8>>>(1)?.unwrap_or_default(),
        })
    }
}

/// Visit one parent's bounded text rows, stopping if the visitor returns false.
pub(crate) fn visit_message(
    conn: &Connection,
    session_id: &str,
    message_id: &str,
    mut visit: impl FnMut(TextPart) -> bool,
) -> rusqlite::Result<()> {
    let mut statement = conn.prepare_cached(
        "SELECT ordinal, CAST(data AS BLOB) FROM session_part
         WHERE message_id = ?1 AND session_id = ?2 AND type = 'text'
             AND length(CAST(data AS BLOB)) <= ?3
         ORDER BY ordinal",
    )?;
    let rows = statement.query_map(
        params![message_id, session_id, MAX_DATA_BYTES],
        TextPart::from_row,
    )?;
    for row in rows {
        if !visit(row?) {
            break;
        }
    }
    Ok(())
}

/// All three coordinates must match the live part and its assistant parent.
/// A NULL or oversized payload is returned without bytes, invalidating old
/// fingerprints without allocating a pathological value.
pub(crate) fn load_part(
    conn: &Connection,
    session_id: &str,
    message_id: &str,
    part_id: &str,
) -> rusqlite::Result<Option<(TextPart, i64, i64)>> {
    // The shipped schema identifies a part by (message_id, ordinal, type);
    // there is no id column, so the locator stores the ordinal.
    let Ok(ordinal) = part_id.parse::<i64>() else {
        return Ok(None);
    };
    conn.query_row(
        "SELECT p.ordinal,
                CASE WHEN length(CAST(p.data AS BLOB)) <= ?4 THEN CAST(p.data AS BLOB) END,
                m.seq, m.time_created
         FROM session_part p JOIN session_message m
           ON m.id = p.message_id AND m.session_id = p.session_id
         WHERE p.ordinal = ?1 AND p.message_id = ?2 AND p.session_id = ?3
               AND p.type = 'text' AND m.type = 'assistant'",
        params![ordinal, message_id, session_id, MAX_DATA_BYTES],
        |row| {
            Ok((
                TextPart::from_row(row)?,
                row.get::<_, Option<i64>>(2)?.unwrap_or(0),
                row.get::<_, Option<i64>>(3)?.unwrap_or(0),
            ))
        },
    )
    .optional()
}
