//! SQLite capability probing (§3.4 generation matrix) and read-only opening.

use crate::recall::contract::{SCHEMA_SQLITE_V1, SCHEMA_SQLITE_V2};
use crate::recall::port::model::{map_sqlite_error, PortError};
use rusqlite::{Connection, OpenFlags};
use std::collections::BTreeSet;
use std::path::Path;

pub const V1_COLUMNS: [(&str, &[&str]); 3] = [
    (
        "session",
        &["id", "parent_id", "title", "time_created", "time_updated"],
    ),
    (
        "message",
        &["id", "session_id", "time_created", "time_updated", "data"],
    ),
    (
        "part",
        &[
            "id",
            "message_id",
            "session_id",
            "time_created",
            "time_updated",
            "data",
        ],
    ),
];

pub const V2_COLUMNS: [(&str, &[&str]); 2] = [
    (
        "session_v2",
        &["id", "title", "directory", "time_created", "time_updated"],
    ),
    (
        "session_message",
        &["id", "session_id", "seq", "type", "time_created", "data"],
    ),
];

/// Open a database strictly read-only. A missing path fails closed and a
/// read-only handle never creates a file.
pub fn open_readonly(path: &Path) -> Result<Connection, PortError> {
    if !path.exists() {
        return Err(PortError::NotFound(path.display().to_string()));
    }
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    Connection::open_with_flags(path, flags).map_err(map_sqlite_error)
}

/// Column names of `table`, or `None` when the table does not exist.
pub fn table_columns(
    conn: &Connection,
    table: &str,
) -> Result<Option<BTreeSet<String>>, PortError> {
    let mut statement = conn
        .prepare("SELECT name FROM pragma_table_info(?1)")
        .map_err(map_sqlite_error)?;
    let rows = statement
        .query_map([table], |row| row.get::<_, String>(0))
        .map_err(map_sqlite_error)?;
    let mut columns = BTreeSet::new();
    for column in rows {
        columns.insert(column.map_err(map_sqlite_error)?);
    }
    if columns.is_empty() {
        Ok(None)
    } else {
        Ok(Some(columns))
    }
}

pub fn table_exists(conn: &Connection, table: &str) -> Result<bool, PortError> {
    let exists: i64 = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
            [table],
            |row| row.get(0),
        )
        .map_err(map_sqlite_error)?;
    Ok(exists != 0)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Probe {
    pub generation: &'static str,
    /// When `sqlite-v1` wins while `session_v2` exists but is incomplete.
    pub session_v2_state: Option<&'static str>,
    /// `session_v2.parent_id` is present (V2 exclusion predicate).
    pub parent_id_available: bool,
}

fn missing_tables(
    conn: &Connection,
    required: &[(&str, &[&str])],
) -> Result<Vec<String>, PortError> {
    let mut missing = Vec::new();
    for (table, columns) in required {
        match table_columns(conn, table)? {
            Some(actual) => {
                for column in *columns {
                    if !actual.contains(*column) {
                        missing.push(format!("column:{table}.{column}"));
                    }
                }
            }
            None => missing.push(format!("table:{table}")),
        }
    }
    Ok(missing)
}

/// Probe the generation. V2 needs `session_v2` + `session_message` with all
/// required columns; otherwise a complete V1 wins (reporting
/// `session_v2 = absent|incomplete`); otherwise the source is unsupported and
/// the missing tables/columns are listed.
pub fn probe(conn: &Connection) -> Result<Probe, PortError> {
    let missing_v2 = missing_tables(conn, &V2_COLUMNS)?;
    if missing_v2.is_empty() {
        let parent_id_available = table_columns(conn, "session_v2")?
            .map(|columns| columns.contains("parent_id"))
            .unwrap_or(false);
        return Ok(Probe {
            generation: SCHEMA_SQLITE_V2,
            session_v2_state: None,
            parent_id_available,
        });
    }

    let v2_present = table_exists(conn, "session_v2")? || table_exists(conn, "session_message")?;
    let missing_v1 = missing_tables(conn, &V1_COLUMNS)?;
    if missing_v1.is_empty() {
        return Ok(Probe {
            generation: SCHEMA_SQLITE_V1,
            session_v2_state: Some(if v2_present { "incomplete" } else { "absent" }),
            parent_id_available: false,
        });
    }

    let mut missing = missing_v2;
    missing.extend(missing_v1);
    Err(PortError::Schema { missing })
}

pub fn require_v1(conn: &Connection) -> Result<(), PortError> {
    let missing = missing_tables(conn, &V1_COLUMNS)?;
    if missing.is_empty() {
        Ok(())
    } else {
        Err(PortError::Schema { missing })
    }
}

pub fn require_v2(conn: &Connection) -> Result<(), PortError> {
    let missing = missing_tables(conn, &V2_COLUMNS)?;
    if missing.is_empty() {
        Ok(())
    } else {
        Err(PortError::Schema { missing })
    }
}
