//! Legacy OpenCode JSON storage reader (`json-legacy-v1`).
//!
//! Layout is fixed: `<storage>/session/**/*.json`, `<storage>/message/<sessionID>/<messageID>.json`,
//! `<storage>/part/<messageID>/<partID>.json`. Nothing else under `<storage>` is
//! scanned and no attachment is read.

use crate::recall::contract::SCHEMA_JSON_LEGACY_V1;
use crate::recall::hash::sha256_hex;
use crate::recall::port::blocks::read_bounded_file;
use crate::recall::port::model::{Health, PartData, PartRecord, PortError, SessionMeta};
use crate::recall::refs::Locator;
use serde_json::Value;
use std::path::{Path, PathBuf};

/// Hard cap for session/message JSON objects and for readback. Part files are
/// additionally bounded by the request's per-source byte budget.
pub const MAX_JSON_OBJECT_BYTES: usize = 32 * 1024 * 1024;

pub struct LegacyReader {
    root: PathBuf,
    storage: PathBuf,
}

impl LegacyReader {
    pub fn new(root: &Path, storage: &Path) -> Self {
        LegacyReader {
            root: root.to_path_buf(),
            storage: storage.to_path_buf(),
        }
    }

    pub fn sessions(&self, health: &mut Health) -> Result<Vec<SessionMeta>, PortError> {
        let mut files = Vec::new();
        let session_dir = self.storage.join("session");
        if session_dir.is_dir() {
            walk_json_files(&session_dir, &mut files)?;
        }
        files.sort();
        let mut sessions = Vec::new();
        for path in files {
            let too_large = std::fs::metadata(&path)
                .map(|meta| meta.len() > MAX_JSON_OBJECT_BYTES as u64)
                .unwrap_or(false);
            if too_large {
                health.skip_without_bytes();
                continue;
            }
            match read_json_object(&path, MAX_JSON_OBJECT_BYTES) {
                Ok((value, bytes)) => match session_from_object(&value) {
                    Some(session) => {
                        health.record(bytes.len() as u64);
                        sessions.push(session);
                    }
                    None => health.skip(bytes.len() as u64),
                },
                Err(_) => health.error(),
            }
        }
        sessions.sort_by(|a, b| {
            b.time_updated
                .cmp(&a.time_updated)
                .then_with(|| a.session_id.cmp(&b.session_id))
        });
        Ok(sessions)
    }

    /// Parts of one session in the fixed order: messages `(time.created, id)`,
    /// then parts `(time.created, id)`.
    pub fn parts_for_session(
        &self,
        session_id: &str,
        max_object_bytes: u64,
        health: &mut Health,
    ) -> Result<Vec<PartRecord>, PortError> {
        let Some(session_component) = safe_component(session_id) else {
            return Ok(Vec::new());
        };
        let message_dir = self.storage.join("message").join(session_component);
        let mut message_files = list_json_files(&message_dir)?;
        message_files.sort();
        let mut messages = Vec::new();
        for path in message_files {
            let stem = file_stem(&path);
            let too_large = std::fs::metadata(&path)
                .map(|meta| meta.len() > MAX_JSON_OBJECT_BYTES as u64)
                .unwrap_or(false);
            if too_large {
                health.skip_without_bytes();
                continue;
            }
            match read_json_object(&path, MAX_JSON_OBJECT_BYTES) {
                Ok((value, bytes)) => {
                    let id = value.get("id").and_then(Value::as_str).unwrap_or("");
                    if id.is_empty() || id != stem {
                        health.skip(bytes.len() as u64);
                        continue;
                    }
                    health.record(bytes.len() as u64);
                    messages.push((
                        timestamp_of(&value, "time", "created"),
                        id.to_string(),
                        path,
                    ));
                }
                Err(_) => health.error(),
            }
        }
        messages.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));

        let mut parts = Vec::new();
        for (_, message_id, _) in messages {
            let part_dir = self.storage.join("part").join(&message_id);
            let mut part_files = list_json_files(&part_dir)?;
            part_files.sort();
            let mut message_parts: Vec<PartRecord> = Vec::new();
            for path in part_files {
                let stem = file_stem(&path);
                // One part file is one object: refuse it before reading when it
                // exceeds the per-source byte budget.
                let file_len = std::fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0);
                if file_len > max_object_bytes {
                    health.note_oversize();
                    continue;
                }
                match read_json_object(
                    &path,
                    max_object_bytes.min(MAX_JSON_OBJECT_BYTES as u64) as usize,
                ) {
                    Ok((value, bytes)) => {
                        let id = value.get("id").and_then(Value::as_str).unwrap_or("");
                        let owner = value
                            .get("messageID")
                            .and_then(Value::as_str)
                            .unwrap_or(&message_id);
                        let owner_session = value.get("sessionID").and_then(Value::as_str);
                        if id.is_empty()
                            || id != stem
                            || owner != message_id
                            || owner_session.map(|owner| owner != session_id).unwrap_or(false)
                        {
                            health.skip(bytes.len() as u64);
                            continue;
                        }
                        let Some(rel_path) = rel_from_root(&self.root, &path) else {
                            health.skip(bytes.len() as u64);
                            continue;
                        };
                        health.record(bytes.len() as u64);
                        message_parts.push(PartRecord {
                            locator: Locator::v1(session_id, &message_id, id),
                            rel_path,
                            schema_gen: SCHEMA_JSON_LEGACY_V1,
                            data: PartData::Block(value.clone()),
                            row_sha256: sha256_hex(&bytes),
                            time_created: timestamp_of(&value, "time", "created"),
                            sort_id: id.to_string(),
                            raw_bytes: bytes.len() as u64,
                        });
                    }
                    Err(_) => health.error(),
                }
            }
            // Part order inside one message: `(time.created, id)`.
            message_parts.sort_by(|a, b| {
                a.time_created
                    .cmp(&b.time_created)
                    .then_with(|| a.sort_id.cmp(&b.sort_id))
            });
            parts.extend(message_parts);
        }
        Ok(parts)
    }

    pub fn load_part(&self, locator: &Locator) -> Result<PartRecord, PortError> {
        let session_id = locator.session_id.as_deref().unwrap_or("");
        let message_id = locator.message_id.as_deref().unwrap_or("");
        let part_id = locator.part_id.as_deref().unwrap_or("");
        let (Some(_session_id), Some(message_id), Some(part_id)) = (
            safe_component(session_id),
            safe_component(message_id),
            safe_component(part_id),
        ) else {
            return Err(PortError::NotFound("unsafe locator component".into()));
        };
        let path = self
            .storage
            .join("part")
            .join(message_id)
            .join(format!("{part_id}.json"));
        let file_len = std::fs::metadata(&path)
            .map(|meta| meta.len())
            .map_err(|err| {
                if err.kind() == std::io::ErrorKind::NotFound {
                    PortError::NotFound(path.display().to_string())
                } else {
                    PortError::Io(err.to_string())
                }
            })?;
        if file_len > MAX_JSON_OBJECT_BYTES as u64 {
            return Err(PortError::Corrupt("part object above read budget".into()));
        }
        let (bytes, _fingerprint) = read_bounded_file(&path, MAX_JSON_OBJECT_BYTES)?;
        let value: Value = serde_json::from_slice(&bytes)
            .map_err(|err| PortError::Corrupt(format!("part file is not JSON: {err}")))?;
        let id = value.get("id").and_then(Value::as_str).unwrap_or("");
        let owner = value
            .get("messageID")
            .and_then(Value::as_str)
            .unwrap_or(message_id);
        if id != part_id || owner != message_id {
            return Err(PortError::Corrupt(
                "part file id does not match its path".into(),
            ));
        }
        let rel_path = rel_from_root(&self.root, &path)
            .ok_or_else(|| PortError::NotFound("part file outside the source root".into()))?;
        Ok(PartRecord {
            locator: locator.clone(),
            rel_path,
            schema_gen: SCHEMA_JSON_LEGACY_V1,
            data: PartData::Block(value.clone()),
            row_sha256: sha256_hex(&bytes),
            time_created: timestamp_of(&value, "time", "created"),
            sort_id: part_id.to_string(),
            raw_bytes: bytes.len() as u64,
        })
    }
}

fn read_json_object(path: &Path, limit: usize) -> Result<(Value, Vec<u8>), PortError> {
    // Read through the 64 KiB block reader; the metadata check inside refuses
    // an over-limit object before any byte is read.
    let (bytes, _fingerprint) = read_bounded_file(path, limit)?;
    let value: Value = serde_json::from_slice(&bytes)
        .map_err(|err| PortError::Corrupt(format!("{}: {err}", path.display())))?;
    if !value.is_object() {
        return Err(PortError::Corrupt(format!(
            "{} is not a JSON object",
            path.display()
        )));
    }
    Ok((value, bytes))
}

fn session_from_object(value: &Value) -> Option<SessionMeta> {
    let session_id = value.get("id")?.as_str()?.to_string();
    if session_id.is_empty() {
        return None;
    }
    let title = value
        .get("title")
        .and_then(Value::as_str)
        .map(|title| title.to_string());
    let parent_id = value
        .get("parentID")
        .and_then(Value::as_str)
        .filter(|parent| !parent.is_empty())
        .map(|parent| parent.to_string());
    Some(SessionMeta {
        session_id,
        title,
        parent_id,
        time_created: timestamp_of(value, "time", "created"),
        time_updated: timestamp_of(value, "time", "updated"),
    })
}

/// Timestamps are either epoch milliseconds or RFC3339 strings. Unknown shapes
/// sort as `0` instead of failing the whole session.
pub fn timestamp_of(value: &Value, object: &str, field: &str) -> i64 {
    let Some(raw) = value.get(object).and_then(|inner| inner.get(field)) else {
        return 0;
    };
    timestamp_value(raw)
}

pub fn timestamp_value(raw: &Value) -> i64 {
    match raw {
        Value::Number(number) => number
            .as_i64()
            .or_else(|| number.as_f64().map(|float| float as i64))
            .unwrap_or(0),
        Value::String(text) => text.parse::<i64>().unwrap_or_else(|_| {
            chrono::DateTime::parse_from_rfc3339(text)
                .map(|instant| instant.timestamp_millis())
                .unwrap_or(0)
        }),
        _ => 0,
    }
}

pub fn safe_component(value: &str) -> Option<&str> {
    if value.is_empty() || value == "." || value == ".." {
        return None;
    }
    if value.contains('/') || value.contains('\\') || value.contains(':') {
        return None;
    }
    Some(value)
}

/// Path of `child` relative to `root`, using `/` and the configured spelling.
fn rel_from_root(root: &Path, child: &Path) -> Option<String> {
    let root = slashify(&root.to_string_lossy());
    let child = slashify(&child.to_string_lossy());
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

fn slashify(value: &str) -> String {
    value
        .chars()
        .map(|ch| if ch == '\\' { '/' } else { ch })
        .collect()
}

fn file_stem(path: &Path) -> &str {
    path.file_stem().and_then(|stem| stem.to_str()).unwrap_or("")
}

fn list_json_files(dir: &Path) -> Result<Vec<PathBuf>, PortError> {
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut files = Vec::new();
    let entries = std::fs::read_dir(dir).map_err(|err| PortError::Io(err.to_string()))?;
    for entry in entries {
        let entry = entry.map_err(|err| PortError::Io(err.to_string()))?;
        let file_type = entry
            .file_type()
            .map_err(|err| PortError::Io(err.to_string()))?;
        if file_type.is_symlink() || !file_type.is_file() {
            continue;
        }
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) == Some("json") {
            files.push(path);
        }
    }
    Ok(files)
}

fn walk_json_files(dir: &Path, files: &mut Vec<PathBuf>) -> Result<(), PortError> {
    let entries = std::fs::read_dir(dir).map_err(|err| PortError::Io(err.to_string()))?;
    for entry in entries {
        let entry = entry.map_err(|err| PortError::Io(err.to_string()))?;
        let file_type = entry
            .file_type()
            .map_err(|err| PortError::Io(err.to_string()))?;
        if file_type.is_symlink() {
            continue;
        }
        let path = entry.path();
        if file_type.is_dir() {
            walk_json_files(&path, files)?;
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("json") {
            files.push(path);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsafe_components_are_rejected() {
        assert!(safe_component("ok-id_1").is_some());
        assert!(safe_component("..").is_none());
        assert!(safe_component("a/b").is_none());
        assert!(safe_component(r"a\b").is_none());
        assert!(safe_component("C:evil").is_none());
        assert!(safe_component("").is_none());
    }

    #[test]
    fn timestamps_accept_ms_and_rfc3339() {
        assert_eq!(timestamp_value(&serde_json::json!(1730000000000i64)), 1730000000000);
        assert_eq!(timestamp_value(&serde_json::json!("1730000000")), 1730000000);
        assert_eq!(
            timestamp_value(&serde_json::json!("2026-10-25T12:00:00Z")),
            1792929600000
        );
        assert_eq!(timestamp_value(&serde_json::json!(null)), 0);
    }
}
