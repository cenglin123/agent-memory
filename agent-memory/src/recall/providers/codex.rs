//! Codex provider: direct read of `rollout-*.jsonl[.zst]` under the configured
//! `storage` root (default `<root>/sessions`), with metadata-only session
//! enumeration, per-line projection, budget progress and `coverage`.
//!
//! Nothing here writes, copies or archives: Codex rollouts are live append
//! files, so no observed-root window is opened and `--strict-snapshot` is
//! refused by the caller. Titles come only from the explicitly configured
//! `db` (`state_5.sqlite`) and `<root>/session_index.jsonl`.

use crate::codex_rollout_file;
use crate::codex_state_db::CODEX_STATE_DB_FILENAME;
use crate::providers::codex::{
    infer_session_id_from_filename, load_thread_titles_from_paths, recall_user_text,
    scan_session_file,
};
use crate::providers::utils::{extract_text, parse_timestamp_to_ms, truncate_summary, TITLE_MAX_CHARS};
use crate::recall::budget::Budget;
use crate::recall::config::SourceConfig;
use crate::recall::contract::{
    codes, status, RANK_BASIS_LEXICAL_WINDOW, SCORE_KIND_NONE,
};
use crate::recall::hash::{root_sha256, sha256_hex, TextRange};
use crate::recall::port::{read_line_bounded, BoundedLine, Health};
use crate::recall::port::compact_json;
use crate::recall::providers::{rank_hits, SourceHit, SourceOutcome};
use crate::recall::refs::{Fingerprint, HitRef, Locator};
use crate::recall::text::{fold_ascii, lexical_match, snippet};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::time::Instant;

/// Decompressed scan ceiling shared by Codex search and readback. A record that
/// would end past this offset cannot be read back, so search never emits it.
pub(crate) const CODEX_SCAN_LIMIT_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StopReason {
    Bytes,
    Time,
}

/// One line's searchable projection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RecordText {
    pub message_id: Option<String>,
    pub pointer: String,
    pub text: String,
}

/// Projection outcome for one decrypted JSONL record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RecordOutcome {
    Text(RecordText),
    /// Legal empty projection: no unit, no error.
    Empty,
    /// Client-injected context (AGENTS.md, environment, developer/system role).
    Injected,
    /// A type recall does not index; counted by literal.
    Ignored { type_literal: String },
    /// Structural damage in a known unit.
    Unresolved { reason: &'static str },
}

fn payload(value: &Value) -> Option<&Value> {
    value.get("payload")
}

fn field_str<'a>(value: Option<&'a Value>, key: &str) -> Option<&'a str> {
    value?.get(key)?.as_str()
}

fn item_id(payload: &Value) -> Option<String> {
    payload
        .get("id")
        .and_then(Value::as_str)
        .or_else(|| payload.get("call_id").and_then(Value::as_str))
        .map(|value| value.to_string())
}

fn content_pointer(content: Option<&Value>) -> String {
    match content {
        Some(Value::Array(items))
            if items.len() == 1 && items[0].get("text").and_then(Value::as_str).is_some() =>
        {
            "/payload/content/0/text".to_string()
        }
        _ => "/payload/content".to_string(),
    }
}

fn item_text(item: &Value) -> Option<String> {
    for key in ["text", "input_text", "output_text"] {
        if let Some(text) = item.get(key).and_then(Value::as_str) {
            return Some(text.to_string());
        }
    }
    item.get("content").map(extract_text).filter(|text| !text.is_empty())
}

/// Text plus pointer for a tool output value (string, content array or object).
fn output_text(value: &Value, base: &str) -> Option<(String, String)> {
    match value {
        Value::String(text) => Some((text.clone(), base.to_string())),
        Value::Array(items) => {
            let texts: Vec<String> = items.iter().filter_map(item_text).collect();
            match texts.len() {
                0 => None,
                1 => {
                    let text = texts.into_iter().next().unwrap_or_default();
                    Some((text, format!("{base}/0/text")))
                }
                _ => Some((texts.join("\n"), base.to_string())),
            }
        }
        Value::Object(_) => {
            let text = extract_text(value);
            (!text.is_empty()).then_some((text, base.to_string()))
        }
        _ => None,
    }
}

/// The single definition of what one rollout line contributes to recall.
///
/// The projection depends only on the line bytes; object-valued inputs use
/// `compact_json` (sorted keys) so a plain and a compressed carrier project
/// identically.
pub(crate) fn project_record(value: &Value) -> RecordOutcome {
    let top = value.get("type").and_then(Value::as_str).unwrap_or("");
    match top {
        "response_item" => {
            let Some(payload) = payload(value) else {
                return RecordOutcome::Empty;
            };
            let kind = payload.get("type").and_then(Value::as_str).unwrap_or("");
            match kind {
                "message" => {
                    let role = payload.get("role").and_then(Value::as_str).unwrap_or("");
                    let content = payload.get("content");
                    match role {
                        "user" => {
                            let raw = content.map(extract_text).unwrap_or_default();
                            match recall_user_text(&raw) {
                                Some(text) => RecordOutcome::Text(RecordText {
                                    message_id: item_id(payload),
                                    pointer: content_pointer(content),
                                    text,
                                }),
                                None => RecordOutcome::Injected,
                            }
                        }
                        "assistant" => {
                            let raw = content.map(extract_text).unwrap_or_default();
                            if raw.trim().is_empty() {
                                RecordOutcome::Empty
                            } else {
                                RecordOutcome::Text(RecordText {
                                    message_id: item_id(payload),
                                    pointer: content_pointer(content),
                                    text: raw,
                                })
                            }
                        }
                        "developer" | "system" => RecordOutcome::Injected,
                        other => RecordOutcome::Ignored {
                            type_literal: format!("message:{other}"),
                        },
                    }
                }
                "function_call" => {
                    let Some(name) = field_str(Some(payload), "name") else {
                        return RecordOutcome::Unresolved {
                            reason: "function_call_name_missing",
                        };
                    };
                    let arguments = match payload.get("arguments") {
                        Some(Value::String(text)) => text.clone(),
                        Some(value) => compact_json(value),
                        None => String::new(),
                    };
                    RecordOutcome::Text(RecordText {
                        message_id: item_id(payload),
                        pointer: "/payload/arguments".to_string(),
                        text: format!("{name}\n{arguments}"),
                    })
                }
                "custom_tool_call" => {
                    let Some(name) = field_str(Some(payload), "name") else {
                        return RecordOutcome::Unresolved {
                            reason: "custom_tool_call_name_missing",
                        };
                    };
                    let input = match payload.get("input") {
                        Some(Value::String(text)) => text.clone(),
                        Some(value) => compact_json(value),
                        None => String::new(),
                    };
                    RecordOutcome::Text(RecordText {
                        message_id: item_id(payload),
                        pointer: "/payload/input".to_string(),
                        text: format!("{name}\n{input}"),
                    })
                }
                "local_shell_call" => {
                    let action = payload
                        .get("action")
                        .cloned()
                        .unwrap_or_else(|| Value::Object(Map::new()));
                    RecordOutcome::Text(RecordText {
                        message_id: item_id(payload),
                        pointer: "/payload/action".to_string(),
                        text: format!("local_shell\n{}", compact_json(&action)),
                    })
                }
                "function_call_output" | "custom_tool_call_output" => {
                    match payload.get("output") {
                        None => RecordOutcome::Empty,
                        Some(output) => match output_text(output, "/payload/output") {
                            Some((text, pointer)) if !text.trim().is_empty() => {
                                RecordOutcome::Text(RecordText {
                                    message_id: item_id(payload),
                                    pointer,
                                    text,
                                })
                            }
                            Some(_) => RecordOutcome::Empty,
                            None => RecordOutcome::Unresolved {
                                reason: "tool_output_unresolved",
                            },
                        },
                    }
                }
                other => RecordOutcome::Ignored {
                    type_literal: other.to_string(),
                },
            }
        }
        "event_msg" => {
            let Some(payload) = payload(value) else {
                return RecordOutcome::Empty;
            };
            let kind = payload.get("type").and_then(Value::as_str).unwrap_or("");
            if kind != "item_completed" {
                return RecordOutcome::Ignored {
                    type_literal: format!("event_msg:{kind}"),
                };
            }
            let Some(item) = payload.get("item") else {
                return RecordOutcome::Ignored {
                    type_literal: "item_completed".to_string(),
                };
            };
            let item_kind = item.get("type").and_then(Value::as_str).unwrap_or("");
            match item_kind {
                "CommandExecution" => match item.get("aggregated_output") {
                    Some(Value::String(text)) if !text.trim().is_empty() => {
                        RecordOutcome::Text(RecordText {
                            message_id: item_id(item),
                            pointer: "/payload/item/aggregated_output".to_string(),
                            text: text.clone(),
                        })
                    }
                    Some(Value::String(_)) | None => RecordOutcome::Empty,
                    Some(_) => RecordOutcome::Unresolved {
                        reason: "aggregated_output_not_string",
                    },
                },
                "McpToolCall" => match item.get("result").and_then(|result| result.get("content")) {
                    None => RecordOutcome::Empty,
                    Some(content) => {
                        match output_text(content, "/payload/item/result/content") {
                            Some((text, pointer)) if !text.trim().is_empty() => {
                                RecordOutcome::Text(RecordText {
                                    message_id: item_id(item),
                                    pointer,
                                    text,
                                })
                            }
                            Some(_) => RecordOutcome::Empty,
                            None => RecordOutcome::Unresolved {
                                reason: "mcp_tool_result_unresolved",
                            },
                        }
                    }
                },
                other => RecordOutcome::Ignored {
                    type_literal: format!("event_msg:item_completed:{other}"),
                },
            }
        }
        other => RecordOutcome::Ignored {
            type_literal: other.to_string(),
        },
    }
}

struct CodexSession {
    path: PathBuf,
    session_id: String,
    title: String,
    last_active: i64,
}

pub(crate) fn codex_storage_dir(source: &SourceConfig, read_root: &Path) -> Option<PathBuf> {
    match &source.storage {
        Some(value) => remap_child(read_root, &source.root, value),
        None => Some(read_root.join("sessions")),
    }
}

pub(crate) fn codex_db_path(source: &SourceConfig, read_root: &Path) -> Option<PathBuf> {
    match &source.db {
        Some(value) => remap_child(read_root, &source.root, value),
        None => {
            let default = read_root.join(CODEX_STATE_DB_FILENAME);
            default.is_file().then_some(default)
        }
    }
}

fn remap_child(read_root: &Path, config_root: &str, value: &str) -> Option<PathBuf> {
    if read_root == Path::new(config_root) {
        return Some(PathBuf::from(value));
    }
    crate::recall::port::reader::rel_from_root(config_root, Path::new(value))
        .map(|rel| read_root.join(rel))
}

fn load_titles(read_root: &Path, source: &SourceConfig) -> HashMap<String, String> {
    let index = read_root.join("session_index.jsonl");
    let dbs: Vec<PathBuf> = codex_db_path(source, read_root).into_iter().collect();
    load_thread_titles_from_paths(&index, &dbs)
}

/// Enumerate rollout files under `root`.
///
/// The top-level `read_dir` failure propagates so the caller can report an
/// unavailable storage root with the OS reason. A failure below the top level
/// is counted as `health.error()` (partial) and the remaining directories are
/// still scanned; it is never reported as a healthy empty source.
fn collect_rollout_files(
    root: &Path,
    files: &mut Vec<PathBuf>,
    health: &mut Health,
) -> std::io::Result<()> {
    let entries = std::fs::read_dir(root)?;
    let mut paths: Vec<(PathBuf, bool)> = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => return Err(err),
        };
        match entry.file_type() {
            Ok(kind) if kind.is_symlink() => continue,
            Ok(kind) => paths.push((entry.path(), kind.is_dir())),
            Err(_) => {
                health.error();
            }
        }
    }
    paths.sort();
    for (path, is_dir) in paths {
        if is_dir {
            if collect_rollout_files(&path, files, health).is_err() {
                health.error();
            }
        } else if codex_rollout_file::is_rollout_file(&path) {
            files.push(path);
        }
    }
    Ok(())
}

/// Logical path: relative to the read root, with any `.zst` suffix removed, so
/// a plain <-> compressed migration never changes `hit_id`.
fn logical_rel_path(read_root: &str, path: &Path) -> Option<String> {
    let rel = crate::recall::port::reader::rel_from_root(read_root, path)?;
    Some(match rel.strip_suffix(".zst") {
        Some(plain) if plain.ends_with(".jsonl") => plain.to_string(),
        _ => rel,
    })
}

pub fn search_at(
    source: &SourceConfig,
    terms: &[String],
    budget: &Budget,
    snippet_bytes: usize,
    session_filter: Option<&str>,
    read_root: &Path,
) -> SourceOutcome {
    let started = Instant::now();
    let read_root_str = read_root.to_string_lossy().to_string();
    let Some(storage) = codex_storage_dir(source, read_root) else {
        return unavailable(
            source,
            "codex_storage_path_unmappable",
            "codex storage path cannot be mapped into the read root".to_string(),
        );
    };
    if !storage.is_dir() {
        return unavailable(
            source,
            "codex_storage_missing",
            format!(
                "codex storage directory is not available: {}",
                storage.display()
            ),
        );
    }

    let mut health = Health::default();
    let mut files = Vec::new();
    if let Err(err) = collect_rollout_files(&storage, &mut files, &mut health) {
        return unavailable(
            source,
            "codex_storage_unreadable",
            format!(
                "codex storage directory cannot be enumerated: {}: {err}",
                storage.display()
            ),
        );
    }
    let mut files = codex_rollout_file::dedupe_siblings(files);
    files.sort();

    let titles = load_titles(read_root, source);
    let mut sessions: Vec<CodexSession> = Vec::new();
    for path in files {
        // A standard rollout filename carries the session UUID: skip a
        // non-target file before opening it, so an unreadable non-target cannot
        // poison a filtered query. Files without a UUID still fall back to the
        // metadata filter below.
        if let Some(id) = session_filter {
            if let Some(file_id) = infer_session_id_from_filename(&path) {
                if file_id != id {
                    continue;
                }
            }
        }
        match scan_session_file(&path) {
            Ok(Some(meta)) => {
                if let Some(id) = session_filter {
                    if meta.session_id != id {
                        continue;
                    }
                }
                let title = titles
                    .get(&meta.session_id)
                    .map(|title| truncate_summary(title, TITLE_MAX_CHARS))
                    .or_else(|| meta.title.clone())
                    .unwrap_or_else(|| meta.session_id.clone());
                sessions.push(CodexSession {
                    path,
                    session_id: meta.session_id,
                    title,
                    last_active: meta.last_active_at.unwrap_or(0),
                });
            }
            Ok(None) => {}
            Err(_) => health.error(),
        }
    }

    let total_sessions = sessions.len() as u64;
    let mut scanned_sessions = 0u64;
    let mut hits: Vec<SourceHit> = Vec::new();
    let mut ignored_types: BTreeMap<String, u64> = BTreeMap::new();
    let mut unresolved = 0u64;
    let mut oversize = false;
    let mut read_limit = false;
    let mut stop: Option<StopReason> = None;

    'sessions: for session in &sessions {
        if health.bytes >= budget.max_bytes_per_source {
            stop = Some(StopReason::Bytes);
            break;
        }
        if started.elapsed().as_millis() as u64 >= budget.max_elapsed_ms_per_source {
            stop = Some(StopReason::Time);
            break;
        }
        let Some(rel_path) = logical_rel_path(&read_root_str, &session.path) else {
            health.error();
            continue;
        };
        scanned_sessions += 1;
        let Ok(reader) = codex_rollout_file::open_reader(&session.path) else {
            health.error();
            continue;
        };
        let mut reader = reader;
        let mut buffer: Vec<u8> = Vec::new();
        let mut index: u64 = 0;
        // Decompressed offset inside this carrier: readback starts from byte 0,
        // so a record ending past the shared ceiling cannot produce a readable
        // ref.
        let mut carrier_bytes: u64 = 0;
        let mut carrier_limit_hit = false;
        loop {
            if health.bytes >= budget.max_bytes_per_source {
                stop = Some(StopReason::Bytes);
                break 'sessions;
            }
            if started.elapsed().as_millis() as u64 >= budget.max_elapsed_ms_per_source {
                stop = Some(StopReason::Time);
                break 'sessions;
            }
            let remaining = CODEX_SCAN_LIMIT_BYTES.saturating_sub(carrier_bytes);
            let ceiling_limited = remaining <= budget.max_bytes_per_source;
            let line_limit = budget.max_bytes_per_source.min(remaining);
            buffer.clear();
            let line = match read_line_bounded(&mut reader, &mut buffer, line_limit, !ceiling_limited)
            {
                Ok(line) => line,
                Err(_) => {
                    health.error();
                    break;
                }
            };
            let (consumed, terminated, too_long) = match line {
                BoundedLine::Eof => break,
                BoundedLine::Line {
                    consumed,
                    terminated,
                    too_long,
                } => (consumed, terminated, too_long),
            };
            carrier_bytes = carrier_bytes.saturating_add(consumed);
            while matches!(buffer.last(), Some(b'\n') | Some(b'\r')) {
                buffer.pop();
            }
            if too_long {
                // F5: bytes actually read (including the drained remainder)
                // count against the byte budget.
                health.consume(consumed);
                if ceiling_limited {
                    carrier_limit_hit = true;
                    break;
                }
                health.note_oversize();
                oversize = true;
                index += 1;
                if !terminated {
                    break;
                }
                continue;
            }
            if carrier_bytes > CODEX_SCAN_LIMIT_BYTES {
                // The record ends past the shared read ceiling: never emit a ref
                // that readback would reject. The bytes were read, so charge them.
                health.consume(consumed);
                carrier_limit_hit = true;
                break;
            }
            health.consume(consumed);
            if buffer.is_empty() {
                index += 1;
                if !terminated {
                    break;
                }
                continue;
            }
            let value: Value = match serde_json::from_slice(&buffer) {
                Ok(value) => value,
                Err(_) => {
                    if !terminated {
                        // Live append: the final fragment is half a line, not a
                        // damaged record.
                        break;
                    }
                    health.skip(0);
                    index += 1;
                    continue;
                }
            };
            match project_record(&value) {
                RecordOutcome::Text(record) if !record.text.is_empty() => {
                    if let Some(found) = lexical_match(&fold_ascii(&record.text), terms) {
                        let text_bytes = record.text.as_bytes();
                        let line_time = value
                            .get("timestamp")
                            .and_then(parse_timestamp_to_ms)
                            .unwrap_or(0);
                        let hit = HitRef {
                            source_id: source.source_id.clone(),
                            source_kind: "codex".to_string(),
                            root_sha256: root_sha256(&source.root),
                            rel_path: rel_path.clone(),
                            schema_gen: crate::recall::contract::SCHEMA_CODEX_ROLLOUT_V1
                                .to_string(),
                            locator: Locator::codex(
                                &session.session_id,
                                record.message_id.as_deref(),
                                index,
                                &record.pointer,
                            ),
                            fingerprint: Fingerprint {
                                range_sha256: sha256_hex(text_bytes),
                                file_sha256: None,
                                row_sha256: Some(sha256_hex(&buffer)),
                            },
                            text_range: TextRange::from_span(text_bytes, 0, text_bytes.len()),
                        };
                        let (display, truncated) = snippet(
                            &record.text,
                            found.first_byte,
                            found.first_term_len,
                            snippet_bytes,
                        );
                        hits.push(SourceHit {
                            hit,
                            title: session.title.clone(),
                            snippet: display,
                            snippet_truncated: truncated,
                            score: None,
                            score_kind: SCORE_KIND_NONE,
                            rank_basis: RANK_BASIS_LEXICAL_WINDOW,
                            distinct_terms: found.distinct_terms,
                            first_byte: found.first_byte,
                            sort_time: if line_time != 0 {
                                line_time
                            } else {
                                session.last_active
                            },
                            sort_id: format!("{}:{:020}", session.session_id, index),
                            related: Vec::new(),
                        exact: None,
                        });
                    }
                }
                RecordOutcome::Text(_) | RecordOutcome::Empty | RecordOutcome::Injected => {}
                RecordOutcome::Ignored { type_literal } => {
                    if !type_literal.is_empty() {
                        *ignored_types.entry(type_literal).or_insert(0) += 1;
                    }
                }
                RecordOutcome::Unresolved { .. } => unresolved += 1,
            }
            index += 1;
            if !terminated {
                break;
            }
        }
        if carrier_limit_hit {
            read_limit = true;
            health.skip_without_bytes();
        }
    }

    rank_hits(&mut hits);

    if health.oversize > 0 {
        oversize = true;
    }

    let mut detail = Map::new();
    let mut coverage = json!({
        "scanned_sessions": scanned_sessions,
        "total_sessions": total_sessions,
    });
    if let Some(id) = session_filter {
        // Nested inside `coverage`: the filtered population is what
        // `total_sessions` counts.
        coverage["session_filter"] = json!(id);
    }
    detail.insert("coverage".into(), coverage);
    if !ignored_types.is_empty() {
        let map: Map<String, Value> = ignored_types
            .iter()
            .map(|(key, count)| (key.clone(), json!(count)))
            .collect();
        detail.insert("ignored_types".into(), Value::Object(map));
    }
    if unresolved > 0 {
        detail.insert("CONTENT_UNRESOLVED".into(), json!(unresolved));
    }
    if oversize {
        detail.insert("reason".into(), json!("oversize"));
    } else if read_limit {
        detail.insert("reason".into(), json!("read_limit"));
    }
    if let Some(reason) = stop {
        let mut budget_detail = Map::new();
        match reason {
            StopReason::Bytes => {
                budget_detail.insert("reason".into(), json!(codes::BUDGET_BYTES));
                budget_detail.insert("consumed_bytes".into(), json!(health.bytes));
            }
            StopReason::Time => {
                budget_detail.insert("reason".into(), json!(codes::BUDGET_TIME));
                budget_detail.insert(
                    "elapsed_ms".into(),
                    json!(started.elapsed().as_millis() as u64),
                );
            }
        }
        detail.insert("budget".into(), Value::Object(budget_detail));
    }

    let content_loss = health.skipped > 0 || health.errors > 0 || unresolved > 0;
    let (status_value, code, message) = if content_loss {
        (
            status::PARTIAL,
            Some(if health.skipped > 0 || health.errors > 0 {
                codes::RECORD_SKIPPED
            } else {
                codes::CONTENT_UNRESOLVED
            }),
            Some(format!(
                "scan lost records (skipped={}, errors={}, unresolved={})",
                health.skipped, health.errors, unresolved
            )),
        )
    } else if let Some(reason) = stop {
        (
            status::TRUNCATED,
            Some(match reason {
                StopReason::Bytes => codes::BUDGET_BYTES,
                StopReason::Time => codes::BUDGET_TIME,
            }),
            Some("source budget exhausted before the scan completed".to_string()),
        )
    } else if hits.is_empty() {
        (status::EMPTY, None, None)
    } else {
        (status::OK, None, None)
    };

    SourceOutcome {
        source_id: source.source_id.clone(),
        status: status_value,
        code,
        detail: if detail.is_empty() {
            None
        } else {
            Some(Value::Object(detail))
        },
        hits,
        warnings: Vec::new(),
        error_message: message,
        side_effects: Vec::new(),
        violations: Vec::new(),
    }
}

fn unavailable(source: &SourceConfig, reason: &str, message: String) -> SourceOutcome {
    SourceOutcome {
        source_id: source.source_id.clone(),
        status: status::UNAVAILABLE,
        code: Some(codes::DB_UNAVAILABLE),
        detail: Some(json!({ "reason": reason })),
        hits: Vec::new(),
        warnings: Vec::new(),
        error_message: Some(message),
        side_effects: Vec::new(),
        violations: Vec::new(),
    }
}
