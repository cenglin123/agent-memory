//! Generation dispatch: one `Reader` per configured OpenCode source.

use crate::recall::contract::{SCHEMA_JSON_LEGACY_V1, SCHEMA_SQLITE_V1, SCHEMA_SQLITE_V2};
use crate::recall::port::legacy_json::LegacyReader;
use crate::recall::port::model::{Health, PartRecord, PortError, SessionMeta};
use crate::recall::port::probe::{open_readonly, probe};
use crate::recall::port::sqlite_v1::V1Reader;
use crate::recall::port::sqlite_v2::V2Reader;
use crate::recall::refs::Locator;
use std::io::{BufRead, Read};
use std::path::{Path, PathBuf};

pub enum Reader {
    V1(V1Reader),
    V2(V2Reader),
    Legacy(LegacyReader),
}

pub struct Resolved {
    pub reader: Reader,
    pub generation: &'static str,
    pub session_v2_state: Option<&'static str>,
    pub parent_id_available: bool,
}

/// The SQLite carrier of one OpenCode source, resolved but not opened for
/// reading. `memory_recent` needs the same path/rel-path/probe decisions as
/// `resolve` without an unbounded `Reader`.
pub struct SqliteCarrier {
    pub path: PathBuf,
    pub rel_path: String,
    pub probe: crate::recall::port::probe::Probe,
}

/// Resolve the configured SQLite carrier: the explicit `db`, or
/// `<root>/opencode.db`. `Ok(None)` when neither exists (the caller falls
/// through to legacy storage); a configured path outside `root` fails closed.
pub fn resolve_sqlite(
    root: &Path,
    root_str: &str,
    db: Option<&str>,
) -> Result<Option<SqliteCarrier>, PortError> {
    let db_path: PathBuf = match db {
        Some(value) => PathBuf::from(value),
        None => root.join("opencode.db"),
    };
    if !db_path.exists() {
        return Ok(None);
    }
    let Some(rel_path) = rel_from_root(root_str, &db_path) else {
        return Err(PortError::Io(
            "database path is outside the configured source root".into(),
        ));
    };
    let conn = open_readonly(&db_path)?;
    let probed = probe(&conn)?;
    Ok(Some(SqliteCarrier {
        path: db_path,
        rel_path,
        probe: probed,
    }))
}


impl Reader {
    pub fn generation(&self) -> &'static str {
        match self {
            Reader::V1(_) => SCHEMA_SQLITE_V1,
            Reader::V2(_) => SCHEMA_SQLITE_V2,
            Reader::Legacy(_) => SCHEMA_JSON_LEGACY_V1,
        }
    }

    pub fn sessions(&mut self, health: &mut Health) -> Result<Vec<SessionMeta>, PortError> {
        match self {
            Reader::V1(reader) => reader.sessions(),
            Reader::V2(reader) => reader.sessions(),
            Reader::Legacy(reader) => reader.sessions(health),
        }
    }

    pub fn parts_for_session(
        &mut self,
        session_id: &str,
        max_object_bytes: u64,
        health: &mut Health,
    ) -> Result<Vec<PartRecord>, PortError> {
        match self {
            Reader::V1(reader) => reader.parts_for_session(session_id, max_object_bytes, health),
            Reader::V2(reader) => reader.parts_for_session(session_id, max_object_bytes, health),
            Reader::Legacy(reader) => reader.parts_for_session(session_id, max_object_bytes, health),
        }
    }

    pub fn load_part(&mut self, locator: &Locator) -> Result<PartRecord, PortError> {
        match self {
            Reader::V1(reader) => reader.load_part(locator),
            Reader::V2(reader) => reader.load_part(locator),
            Reader::Legacy(reader) => reader.load_part(locator),
        }
    }
}

/// Resolve the reader for one source.
///
/// Order (§3.4): the configured `db` (or `<root>/opencode.db`) exists ->
/// probe SQLite V2/V1; otherwise `storage` exists -> legacy JSON; otherwise
/// `DB_UNAVAILABLE`. A configured `db`/`storage` outside `root` fails closed.
///
/// `root_str` is the tree the child paths are relative to: the configured root
/// string, or the strict-snapshot copy when reading from one. It only drives
/// containment checks and `rel_path` derivation; it is never hashed.
pub fn resolve(
    root: &Path,
    root_str: &str,
    db: Option<&str>,
    storage: Option<&str>,
) -> Result<Resolved, PortError> {
    if !root.is_dir() {
        return Err(PortError::NotFound(format!(
            "source root is not a directory: {}",
            root.display()
        )));
    }
    if let Some(carrier) = resolve_sqlite(root, root_str, db)? {
        return match carrier.probe.generation {
            SCHEMA_SQLITE_V2 => Ok(Resolved {
                reader: Reader::V2(V2Reader::open(
                    &carrier.path,
                    &carrier.rel_path,
                    carrier.probe.parent_id_available,
                )?),
                generation: SCHEMA_SQLITE_V2,
                session_v2_state: None,
                parent_id_available: carrier.probe.parent_id_available,
            }),
            _ => Ok(Resolved {
                reader: Reader::V1(V1Reader::open(&carrier.path, &carrier.rel_path)?),
                generation: SCHEMA_SQLITE_V1,
                session_v2_state: carrier.probe.session_v2_state,
                parent_id_available: false,
            }),
        };
    }

    let storage_path: PathBuf = match storage {
        Some(value) => PathBuf::from(value),
        None => root.join("storage"),
    };
    if storage_path.is_dir() {
        if rel_from_root(root_str, &storage_path).is_none() {
            return Err(PortError::Io(
                "storage path is outside the configured source root".into(),
            ));
        }
        return Ok(Resolved {
            reader: Reader::Legacy(LegacyReader::new(root, &storage_path)),
            generation: SCHEMA_JSON_LEGACY_V1,
            session_v2_state: None,
            parent_id_available: false,
        });
    }

    let db_path: PathBuf = match db {
        Some(value) => PathBuf::from(value),
        None => root.join("opencode.db"),
    };
    Err(PortError::NotFound(format!(
        "neither {} nor {} exists",
        db_path.display(),
        storage_path.display()
    )))
}

/// One bounded physical line read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoundedLine {
    /// End of input.
    Eof,
    /// A line (possibly a too-long one). `consumed` counts every byte read,
    /// including a drained remainder; `buffer` holds at most `limit + 1` bytes.
    Line {
        consumed: u64,
        terminated: bool,
        too_long: bool,
    },
}

/// Read one `\n`-terminated line without ever buffering more than `limit + 1`
/// bytes.
///
/// `drain_remainder` decides what happens when a record exceeds `limit`: `true`
/// skips to the next line (search keeps scanning), `false` leaves the rest
/// unread so the caller can stop (readback already found its record).
pub fn read_line_bounded<R: BufRead>(
    reader: &mut R,
    buffer: &mut Vec<u8>,
    limit: u64,
    drain_remainder: bool,
) -> std::io::Result<BoundedLine> {
    buffer.clear();
    let cap = limit.saturating_add(1);
    let read = {
        let mut limited = (&mut *reader).take(cap);
        limited.read_until(b'\n', buffer)?
    };
    if read == 0 {
        return Ok(BoundedLine::Eof);
    }
    let terminated = buffer.last() == Some(&b'\n');
    // A newline exactly at `limit + 1` is a legal line whose content is `limit`
    // bytes; anything longer has no terminating newline inside the window.
    let too_long = read as u64 > limit && !(terminated && read as u64 == cap);
    let mut terminated = terminated;
    let mut consumed = read as u64;
    if too_long && drain_remainder {
        loop {
            let chunk = reader.fill_buf()?;
            if chunk.is_empty() {
                break;
            }
            if let Some(position) = chunk.iter().position(|byte| *byte == b'\n') {
                let take = position + 1;
                reader.consume(take);
                consumed += take as u64;
                terminated = true;
                break;
            }
            let take = chunk.len();
            reader.consume(take);
            consumed += take as u64;
        }
    }
    Ok(BoundedLine::Line {
        consumed,
        terminated,
        too_long,
    })
}

pub fn rel_from_root(root: &str, child: &Path) -> Option<String> {
    if let Some(rel) = rel_from_strings(root, &child.to_string_lossy()) {
        return Some(rel);
    }
    // Windows can spell one directory two ways: the 8.3 short name
    // (RUNNER~1) in a config path, the long name (runneradmin) in a path
    // canonicalised during capture. Retry on canonical forms so the same
    // directory still matches.
    let root_long = crate::path_spelling::canonical_spelling(Path::new(root))?;
    let child_long = crate::path_spelling::canonical_spelling(child)?;
    rel_from_strings(&root_long, &child_long)
}

fn rel_from_strings(root: &str, child: &str) -> Option<String> {
    let root = root.replace('\\', "/");
    let child = child.replace('\\', "/");
    let root_trimmed = root.trim_end_matches('/');
    let root_ascii = root_trimmed.to_ascii_lowercase();
    let child_ascii = child.to_ascii_lowercase();
    if child_ascii.len() > root_ascii.len()
        && child_ascii.starts_with(&root_ascii)
        && child_ascii.as_bytes()[root_ascii.len()] == b'/'
    {
        Some(child[root_ascii.len() + 1..].to_string())
    } else {
        None
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn bounded_line_never_buffers_more_than_limit_plus_one() {
        let data = vec![b'x'; 100_000];
        let mut cursor = Cursor::new(data);
        let mut buffer = Vec::new();
        let line = read_line_bounded(&mut cursor, &mut buffer, 1024, true).expect("read");
        assert_eq!(
            line,
            BoundedLine::Line {
                consumed: 100_000,
                terminated: false,
                too_long: true
            }
        );
        assert_eq!(buffer.len(), 1025, "the rest of the line is drained, not buffered");
        assert_eq!(cursor.position(), 100_000);
        assert_eq!(
            read_line_bounded(&mut cursor, &mut buffer, 1024, true).expect("eof"),
            BoundedLine::Eof
        );
    }

    #[test]
    fn bounded_line_accepts_a_newline_at_the_limit() {
        let mut data = vec![b'x'; 1024];
        data.push(b'\n');
        let mut cursor = Cursor::new(data);
        let mut buffer = Vec::new();
        assert_eq!(
            read_line_bounded(&mut cursor, &mut buffer, 1024, true).expect("read"),
            BoundedLine::Line {
                consumed: 1025,
                terminated: true,
                too_long: false
            }
        );
        assert_eq!(buffer.len(), 1025);
    }

    #[test]
    fn bounded_line_can_stop_mid_line_and_resume() {
        let mut data = vec![b'x'; 5000];
        data.push(b'\n');
        data.extend_from_slice(b"next\n");
        let mut cursor = Cursor::new(data);
        let mut buffer = Vec::new();
        let first = read_line_bounded(&mut cursor, &mut buffer, 100, false).expect("read");
        assert_eq!(
            first,
            BoundedLine::Line {
                consumed: 101,
                terminated: false,
                too_long: true
            }
        );
        assert_eq!(cursor.position(), 101);
        // Resuming drains the remainder of the same physical line, then the
        // next line is read normally.
        let second = read_line_bounded(&mut cursor, &mut buffer, 100, true).expect("drain");
        assert_eq!(
            second,
            BoundedLine::Line {
                consumed: 5000 - 101 + 1,
                terminated: true,
                too_long: true
            }
        );
        let third = read_line_bounded(&mut cursor, &mut buffer, 100, true).expect("next");
        assert_eq!(
            third,
            BoundedLine::Line {
                consumed: 5,
                terminated: true,
                too_long: false
            }
        );
        assert_eq!(buffer, b"next\n");
    }

    #[test]
    fn relative_path_uses_configured_spelling_and_slashes() {
        assert_eq!(
            rel_from_root(r"C:\Root", Path::new(r"c:\root\opencode.db")),
            Some("opencode.db".to_string())
        );
        assert_eq!(
            rel_from_root("C:/Root", Path::new("C:/Root/storage")),
            Some("storage".to_string())
        );
        assert_eq!(rel_from_root("C:/Root", Path::new("C:/Other/db")), None);
    }

}
