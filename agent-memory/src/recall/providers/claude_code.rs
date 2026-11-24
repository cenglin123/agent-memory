//! Claude Code provider (`<root>/<project>/<session-uuid>.jsonl`).
//!
//! A Claude Code transcript is a live append JSONL file. Each line is one
//! record with `type` (`user` / `assistant` / `system` / …), `message.content`
//! (a string or a block array of `text`, `tool_use` and `tool_result`),
//! `timestamp`, `sessionId`, `uuid` and `isSidechain`.
//!
//! One record can carry several searchable projections (text, tool input and
//! tool output), so a *unit* — not a line — is the searchable object. The ref
//! keeps the Codex JSONL shape (`session_id`, `message_id`, `record_index`,
//! `pointer`) under the `claude-jsonl-v1` generation: `record_index` is the
//! 0-based line, `pointer` the JSON pointer of the block inside the line.
//!
//! Subagent transcripts (`<session>/subagents/*.jsonl`, or records with
//! `isSidechain: true`) are workers: the lexical path never returns them (the
//! same policy OpenCode applies to `parent_id` sessions), while the full-text
//! index stores them with `session_kind = worker` so an exact query can include
//! or exclude them explicitly.

use crate::recall::port::compact_json;
use crate::providers::claude::{QueuedDedup, QueuedVerdict};
use crate::providers::utils::parse_timestamp_to_ms;
use crate::recall::budget::Budget;
use crate::recall::config::SourceConfig;
use crate::recall::contract::{
    codes, status, ROLE_ASSISTANT, ROLE_TOOL, ROLE_USER, SCHEMA_CLAUDE_JSONL_V1, SCORE_KIND_NONE,
    UNIT_KIND_TEXT, UNIT_KIND_TOOL_INPUT, UNIT_KIND_TOOL_OUTPUT,
};
use crate::recall::fulltext::CLAUDE_SIDECHAIN_FIELD;
use crate::recall::hash::{root_sha256, sha256_hex, TextRange};
use crate::recall::port::reader::{read_line_bounded, rel_from_root, BoundedLine};
use crate::recall::port::Health;
use crate::recall::providers::{rank_hits, SourceHit, SourceOutcome};
use crate::recall::refs::{Fingerprint, HitRef, Locator};
use crate::recall::text::{fold_ascii, lexical_match, snippet};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

/// Line ceiling shared by search, readback and indexing.
pub const CLAUDE_SCAN_LIMIT_BYTES: u64 = 64 * 1024 * 1024;

/// One searchable projection of one record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeUnit {
    pub pointer: String,
    pub text: String,
    pub role: &'static str,
    pub kind: &'static str,
}

/// Per-record metadata that does not depend on the content shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordMeta {
    pub session_id: Option<String>,
    pub uuid: Option<String>,
    pub timestamp_ms: Option<i64>,
    pub is_sidechain: bool,
    pub is_meta: bool,
}

pub fn record_meta(value: &Value) -> RecordMeta {
    RecordMeta {
        session_id: value
            .get("sessionId")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_string),
        uuid: value
            .get("uuid")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_string),
        // A `queued_command` attachment carries its own time; Claude Code omits
        // the record-level `timestamp` on some of them.
        timestamp_ms: value
            .get("timestamp")
            .and_then(parse_timestamp_to_ms)
            .or_else(|| {
                value
                    .get("attachment")
                    .and_then(|attachment| attachment.get("timestamp"))
                    .and_then(parse_timestamp_to_ms)
            }),
        is_sidechain: value
            .get(CLAUDE_SIDECHAIN_FIELD)
            .and_then(Value::as_bool)
            .unwrap_or(false),
        is_meta: value.get("isMeta").and_then(Value::as_bool).unwrap_or(false),
    }
}

/// Projection outcome for one transcript line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaudeRecordOutcome {
    Units(Vec<ClaudeUnit>),
    /// A legal record with no searchable text (metadata, images, thinking).
    Empty,
    /// Client-injected context (`isMeta`, `<system-reminder>`, …).
    Injected,
    /// A record type this source does not index; counted by literal.
    Ignored { type_literal: String },
    /// Structural damage in a known record.
    Unresolved { reason: &'static str },
}

fn is_injected_text(text: &str) -> bool {
    let trimmed = text.trim_start();
    trimmed.starts_with("<system-reminder>")
        || trimmed.starts_with("<local-command")
        || trimmed.starts_with("<task-notification>")
        || text.contains("<local-command-caveat>")
}

fn text_of_content(value: Option<&Value>) -> String {
    match value {
        None => String::new(),
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(items)) => {
            let mut parts = Vec::new();
            for item in items {
                match item {
                    Value::String(text) => parts.push(text.clone()),
                    other => {
                        if let Some(text) = other.get("text").and_then(Value::as_str) {
                            parts.push(text.to_string());
                        }
                    }
                }
            }
            parts.join("\n")
        }
        Some(_) => String::new(),
    }
}

/// The single definition of what one transcript line contributes to recall.
///
/// `isSidechain` records are projected (the index needs them); only `isMeta`
/// records are dropped as injected.
pub fn project_record(value: &Value) -> ClaudeRecordOutcome {
    let meta = record_meta(value);
    if meta.is_meta {
        return ClaudeRecordOutcome::Injected;
    }
    let record_type = value.get("type").and_then(Value::as_str).unwrap_or("");
    if record_type == "attachment" {
        return attachment_units(value);
    }
    let Some(message) = value.get("message") else {
        return if record_type.is_empty() {
            ClaudeRecordOutcome::Empty
        } else {
            ClaudeRecordOutcome::Ignored {
                type_literal: record_type.to_string(),
            }
        };
    };
    let content = message.get("content");
    match record_type {
        "user" => user_units(content),
        "assistant" => assistant_units(content),
        other => ClaudeRecordOutcome::Ignored {
            type_literal: if other.is_empty() {
                "message".to_string()
            } else {
                other.to_string()
            },
        },
    }
}

/// `type: "attachment"`.
///
/// A mid-turn prompt (`attachment.type == "queued_command"`,
/// `commandMode == "prompt"`, `origin.kind == "human"`) is the owner's own
/// message and projects as user text. `origin.kind == "peer"` (another session
/// sent it through SendMessage) is runtime-injected context, never the owner's
/// words: it is marked injected exactly like a `<system-reminder>` body.
/// `task-notification` and every other attachment type stay unindexed.
fn attachment_units(value: &Value) -> ClaudeRecordOutcome {
    let ignored = ClaudeRecordOutcome::Ignored {
        type_literal: "attachment".to_string(),
    };
    let Some(attachment) = value.get("attachment") else {
        return ignored;
    };
    if attachment.get("type").and_then(Value::as_str) != Some("queued_command") {
        return ignored;
    }
    if attachment.get("commandMode").and_then(Value::as_str) != Some("prompt") {
        return ignored;
    }
    let origin = attachment
        .get("origin")
        .and_then(|origin| origin.get("kind"))
        .and_then(Value::as_str);
    if origin != Some("human") {
        return ClaudeRecordOutcome::Injected;
    }
    let prompt = attachment.get("prompt");
    let mut units = Vec::new();
    let mut had_text = false;
    match prompt {
        Some(Value::String(text)) => {
            had_text = true;
            let pointer = "/attachment/prompt".to_string();
            if !text.trim().is_empty() && !is_injected_text(text) {
                units.push(ClaudeUnit {
                    pointer,
                    text: text.clone(),
                    role: ROLE_USER,
                    kind: UNIT_KIND_TEXT,
                });
            }
        }
        Some(Value::Array(items)) => {
            for (index, item) in items.iter().enumerate() {
                if item.get("type").and_then(Value::as_str) != Some("text") {
                    continue;
                }
                had_text = true;
                let Some(text) = item.get("text").and_then(Value::as_str) else {
                    continue;
                };
                if text.trim().is_empty() || is_injected_text(text) {
                    continue;
                }
                units.push(ClaudeUnit {
                    pointer: format!("/attachment/prompt/{index}/text"),
                    text: text.to_string(),
                    role: ROLE_USER,
                    kind: UNIT_KIND_TEXT,
                });
            }
        }
        Some(_) => {
            return ClaudeRecordOutcome::Unresolved {
                reason: "prompt_not_string_or_array",
            }
        }
        None => return ClaudeRecordOutcome::Empty,
    }
    if units.is_empty() {
        return if had_text && matches!(prompt, Some(Value::Array(_))) {
            // Every text block was injected context: one known channel, no text.
            ClaudeRecordOutcome::Injected
        } else {
            ClaudeRecordOutcome::Empty
        };
    }
    ClaudeRecordOutcome::Units(units)
}

fn user_units(content: Option<&Value>) -> ClaudeRecordOutcome {
    match content {
        None => ClaudeRecordOutcome::Empty,
        Some(Value::String(text)) => {
            if is_injected_text(text) {
                return ClaudeRecordOutcome::Injected;
            }
            if text.trim().is_empty() {
                return ClaudeRecordOutcome::Empty;
            }
            ClaudeRecordOutcome::Units(vec![ClaudeUnit {
                pointer: "/message/content".to_string(),
                text: text.clone(),
                role: ROLE_USER,
                kind: UNIT_KIND_TEXT,
            }])
        }
        Some(Value::Array(items)) => {
            let mut units = Vec::new();
            let mut had_text = false;
            for (index, item) in items.iter().enumerate() {
                let kind = item.get("type").and_then(Value::as_str).unwrap_or("");
                let pointer = format!("/message/content/{index}");
                match kind {
                    "text" => {
                        had_text = true;
                        let Some(text) = item.get("text").and_then(Value::as_str) else {
                            continue;
                        };
                        if text.trim().is_empty() || is_injected_text(text) {
                            continue;
                        }
                        units.push(ClaudeUnit {
                            pointer,
                            text: text.to_string(),
                            role: ROLE_USER,
                            kind: UNIT_KIND_TEXT,
                        });
                    }
                    "tool_result" => {
                        let text = text_of_content(item.get("content"));
                        if text.trim().is_empty() {
                            continue;
                        }
                        units.push(ClaudeUnit {
                            pointer,
                            text,
                            role: ROLE_TOOL,
                            kind: UNIT_KIND_TOOL_OUTPUT,
                        });
                    }
                    _ => {}
                }
            }
            match (units.is_empty(), had_text) {
                (true, true) => ClaudeRecordOutcome::Injected,
                (true, false) => ClaudeRecordOutcome::Empty,
                _ => ClaudeRecordOutcome::Units(units),
            }
        }
        Some(_) => ClaudeRecordOutcome::Unresolved {
            reason: "content_not_string_or_array",
        },
    }
}

fn assistant_units(content: Option<&Value>) -> ClaudeRecordOutcome {
    match content {
        None => ClaudeRecordOutcome::Empty,
        Some(Value::String(text)) => {
            if text.trim().is_empty() {
                return ClaudeRecordOutcome::Empty;
            }
            ClaudeRecordOutcome::Units(vec![ClaudeUnit {
                pointer: "/message/content".to_string(),
                text: text.clone(),
                role: ROLE_ASSISTANT,
                kind: UNIT_KIND_TEXT,
            }])
        }
        Some(Value::Array(items)) => {
            let mut units = Vec::new();
            for (index, item) in items.iter().enumerate() {
                let kind = item.get("type").and_then(Value::as_str).unwrap_or("");
                let pointer = format!("/message/content/{index}");
                match kind {
                    "text" => {
                        let Some(text) = item.get("text").and_then(Value::as_str) else {
                            continue;
                        };
                        if text.trim().is_empty() {
                            continue;
                        }
                        units.push(ClaudeUnit {
                            pointer,
                            text: text.to_string(),
                            role: ROLE_ASSISTANT,
                            kind: UNIT_KIND_TEXT,
                        });
                    }
                    "tool_use" => {
                        let name = item
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        let input = item.get("input").cloned().unwrap_or(Value::Null);
                        let text = format!("{name}\n{}", compact_json(&input));
                        units.push(ClaudeUnit {
                            pointer,
                            text,
                            role: ROLE_ASSISTANT,
                            kind: UNIT_KIND_TOOL_INPUT,
                        });
                    }
                    _ => {}
                }
            }
            if units.is_empty() {
                ClaudeRecordOutcome::Empty
            } else {
                ClaudeRecordOutcome::Units(units)
            }
        }
        Some(_) => ClaudeRecordOutcome::Unresolved {
            reason: "content_not_string_or_array",
        },
    }
}

// ---------------------------------------------------------------------------
// File enumeration
// ---------------------------------------------------------------------------

/// Every transcript under `root`, as `(rel_path, absolute path)` pairs with
/// forward-slash relative paths. Subagent transcripts are included; callers
/// decide whether to use them.
pub fn transcript_files(root: &Path) -> Result<Vec<(String, PathBuf)>, String> {
    let entries = std::fs::read_dir(root)
        .map_err(|error| format!("claude code root cannot be enumerated: {error}"))?;
    let mut projects: Vec<PathBuf> = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| format!("claude code root cannot be enumerated: {error}"))?;
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() && !file_type.is_symlink() {
            projects.push(entry.path());
        }
    }
    projects.sort();
    let mut files: Vec<(String, PathBuf)> = Vec::new();
    for project in projects {
        let project_name = match project.file_name().and_then(|name| name.to_str()) {
            Some(name) => name.to_string(),
            None => continue,
        };
        let Ok(entries) = std::fs::read_dir(&project) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_symlink() {
                continue;
            }
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            if file_type.is_file() {
                if !is_transcript_name(&name) {
                    continue;
                }
                files.push((format!("{project_name}/{name}"), path));
                continue;
            }
            if !file_type.is_dir() {
                continue;
            }
            // `<project>/<session>/subagents/*.jsonl`
            let subagents = path.join("subagents");
            let Ok(sub_entries) = std::fs::read_dir(&subagents) else {
                continue;
            };
            for sub in sub_entries.flatten() {
                let Ok(sub_type) = sub.file_type() else {
                    continue;
                };
                if sub_type.is_symlink() || !sub_type.is_file() {
                    continue;
                }
                let sub_name = sub.file_name().to_string_lossy().to_string();
                if !is_transcript_name(&sub_name) {
                    continue;
                }
                files.push((
                    format!("{project_name}/{name}/subagents/{sub_name}"),
                    sub.path(),
                ));
            }
        }
    }
    files.retain(|(rel, _)| !rel.ends_with("//") && !rel.contains("//"));
    files.sort();
    Ok(files)
}

pub(crate) fn is_transcript_name(name: &str) -> bool {
    name.ends_with(".jsonl") && name != "journal.jsonl"
}

/// Session id from a transcript path: the file stem, or the session directory
/// for a subagent transcript.
pub fn infer_session_id(rel_path: &str) -> Option<String> {
    let parts: Vec<&str> = rel_path.split(['/', '\\']).collect();
    let file = parts.last()?;
    let stem = file.strip_suffix(".jsonl").unwrap_or(file);
    if parts.len() >= 3 && parts[parts.len() - 2] == "subagents" {
        return Some(parts[parts.len() - 3].to_string());
    }
    Some(stem.to_string())
}

/// True when this transcript is a subagent (worker) transcript.
pub fn is_subagent_path(rel_path: &str) -> bool {
    rel_path
        .split(['/', '\\'])
        .any(|part| part == crate::recall::fulltext::CLAUDE_WORKER_DIR)
}

// ---------------------------------------------------------------------------
// Lexical search (same capability as the other live sources)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StopReason {
    Bytes,
    Time,
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
    let files = match transcript_files(read_root) {
        Ok(files) => files,
        Err(message) => {
            return SourceOutcome {
                source_id: source.source_id.clone(),
                status: status::UNAVAILABLE,
                code: Some(codes::SOURCE_ROOT_INVALID),
                detail: Some(json!({ "reason": "root_unreadable" })),
                hits: Vec::new(),
                warnings: Vec::new(),
                error_message: Some(message),
                side_effects: Vec::new(),
                violations: Vec::new(),
            }
        }
    };
    // Worker transcripts are never returned by the lexical path.
    let files: Vec<(String, PathBuf)> = files
        .into_iter()
        .filter(|(rel, _)| {
            if is_subagent_path(rel) {
                return false;
            }
            match session_filter {
                Some(id) => infer_session_id(rel).as_deref() == Some(id) || rel.contains(id),
                None => true,
            }
        })
        .collect();
    let total_sessions = files.len() as u64;
    let mut scanned_sessions = 0u64;
    let mut health = Health::default();
    let mut hits: Vec<SourceHit> = Vec::new();
    let mut ignored_types: BTreeMap<String, u64> = BTreeMap::new();
    let mut unresolved = 0u64;
    let mut stop: Option<StopReason> = None;
    let mut buffer: Vec<u8> = Vec::new();

    'files: for (rel_path, path) in &files {
        if health.bytes >= budget.max_bytes_per_source {
            stop = Some(StopReason::Bytes);
            break;
        }
        if started.elapsed().as_millis() as u64 >= budget.max_elapsed_ms_per_source {
            stop = Some(StopReason::Time);
            break;
        }
        let Ok(file) = std::fs::File::open(path) else {
            health.error();
            continue;
        };
        let mut reader = std::io::BufReader::with_capacity(256 * 1024, file);
        scanned_sessions += 1;
        let mut line_index: u64 = 0;
        let mut carrier_bytes: u64 = 0;
        // Mid-turn prompts are held until the file ends: the same text may also
        // be carried by an ordinary `type: "user"` record further down.
        let mut dedup = QueuedDedup::default();
        let mut held: Vec<(String, SourceHit)> = Vec::new();
        loop {
            if health.bytes >= budget.max_bytes_per_source {
                stop = Some(StopReason::Bytes);
                break 'files;
            }
            if started.elapsed().as_millis() as u64 >= budget.max_elapsed_ms_per_source {
                stop = Some(StopReason::Time);
                break 'files;
            }
            let remaining = CLAUDE_SCAN_LIMIT_BYTES.saturating_sub(carrier_bytes);
            let line_limit = budget.max_bytes_per_source.min(remaining);
            buffer.clear();
            let line = match read_line_bounded(&mut reader, &mut buffer, line_limit, true) {
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
            health.consume(consumed);
            if too_long {
                health.note_oversize();
                line_index += 1;
                if !terminated {
                    break;
                }
                continue;
            }
            if carrier_bytes > CLAUDE_SCAN_LIMIT_BYTES {
                health.skip_without_bytes();
                break;
            }
            while matches!(buffer.last(), Some(b'\n') | Some(b'\r')) {
                buffer.pop();
            }
            if buffer.is_empty() {
                line_index += 1;
                continue;
            }
            let Ok(value) = serde_json::from_slice::<Value>(&buffer) else {
                if !terminated {
                    break;
                }
                health.skip(0);
                line_index += 1;
                continue;
            };
            if !terminated {
                // Half-written final line of a live append: not a record.
                break;
            }
            let meta = record_meta(&value);
            if meta.is_sidechain {
                line_index += 1;
                continue;
            }
            match project_record(&value) {
                ClaudeRecordOutcome::Units(units) => {
                    let row_sha256 = sha256_hex(&buffer);
                    let line_time = meta.timestamp_ms.unwrap_or(0);
                    let session_id = meta
                        .session_id
                        .clone()
                        .or_else(|| infer_session_id(rel_path))
                        .unwrap_or_else(|| rel_path.clone());
                    for unit in units {
                        if unit.text.is_empty() {
                            continue;
                        }
                        let ordinary_user_text =
                            unit.role == ROLE_USER && unit.kind == UNIT_KIND_TEXT;
                        let verdict =
                            dedup.observe(Some(&unit.pointer), ordinary_user_text, &unit.text);
                        if verdict == QueuedVerdict::Drop {
                            continue;
                        }
                        let Some(found) = lexical_match(&fold_ascii(&unit.text), terms) else {
                            continue;
                        };
                        let text_bytes = unit.text.as_bytes();
                        let (display, truncated) =
                            snippet(&unit.text, found.first_byte, found.first_term_len, snippet_bytes);
                        let reference = HitRef {
                            source_id: source.source_id.clone(),
                            source_kind: crate::recall::contract::SOURCE_KIND_CLAUDE_CODE
                                .to_string(),
                            root_sha256: root_sha256(&source.root),
                            rel_path: rel_path.clone(),
                            schema_gen: SCHEMA_CLAUDE_JSONL_V1.to_string(),
                            locator: Locator::codex(
                                &session_id,
                                meta.uuid.as_deref(),
                                line_index,
                                &unit.pointer,
                            ),
                            fingerprint: Fingerprint {
                                range_sha256: sha256_hex(text_bytes),
                                file_sha256: None,
                                row_sha256: Some(row_sha256.clone()),
                            },
                            text_range: TextRange::from_span(text_bytes, 0, text_bytes.len()),
                        };
                        let hit = SourceHit {
                            hit: reference,
                            title: session_id.clone(),
                            snippet: display,
                            snippet_truncated: truncated,
                            score: None,
                            score_kind: SCORE_KIND_NONE,
                            rank_basis: crate::recall::contract::RANK_BASIS_LEXICAL_WINDOW,
                            distinct_terms: found.distinct_terms,
                            first_byte: found.first_byte,
                            sort_time: if line_time != 0 {
                                line_time
                            } else {
                                line_index as i64
                            },
                            sort_id: format!("{}:{:020}", session_id, line_index),
                            related: Vec::new(),
                            exact: None,
                        };
                        if verdict == QueuedVerdict::Hold {
                            held.push((unit.text.trim().to_string(), hit));
                        } else {
                            hits.push(hit);
                        }
                    }
                }
                ClaudeRecordOutcome::Empty | ClaudeRecordOutcome::Injected => {}
                ClaudeRecordOutcome::Ignored { type_literal } => {
                    if !type_literal.is_empty() {
                        *ignored_types.entry(type_literal).or_insert(0) += 1;
                    }
                }
                ClaudeRecordOutcome::Unresolved { .. } => unresolved += 1,
            }
            line_index += 1;
        }
        // Release the held mid-turn prompts no ordinary user record duplicated.
        for (key, hit) in held.drain(..) {
            if dedup.release(&key) {
                hits.push(hit);
            }
        }
    }

    rank_hits(&mut hits);
    let mut detail = Map::new();
    let mut coverage = json!({
        "scanned_sessions": scanned_sessions,
        "total_sessions": total_sessions,
    });
    if let Some(id) = session_filter {
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

/// Search reading from `read_root` (configured root or a snapshot copy).
pub fn search(
    source: &SourceConfig,
    terms: &[String],
    budget: &Budget,
    snippet_bytes: usize,
    session_filter: Option<&str>,
) -> SourceOutcome {
    search_at(
        source,
        terms,
        budget,
        snippet_bytes,
        session_filter,
        Path::new(&source.root),
    )
}

/// Resolution helper shared with readback: a ref is only readable from inside
/// the configured Claude Code root.
pub fn ensure_inside_root(source: &SourceConfig, read_root: &Path, carrier: &Path) -> bool {
    let root_str = read_root.to_string_lossy().to_string();
    rel_from_root(&root_str, carrier).is_some() && rel_from_root(&source.root, carrier).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn attachment(prompt: Value, mode: &str, origin: &str) -> Value {
        json!({
            "type": "attachment",
            "uuid": "q1",
            "timestamp": "2026-10-31T17:38:51.169Z",
            "attachment": {
                "type": "queued_command",
                "prompt": prompt,
                "commandMode": mode,
                "origin": { "kind": origin },
                "timestamp": "2026-10-31T17:38:51.169Z",
                "humanTurn": true
            },
            "renderedRole": "system"
        })
    }

    fn texts(outcome: ClaudeRecordOutcome) -> Vec<(String, String)> {
        match outcome {
            ClaudeRecordOutcome::Units(units) => units
                .into_iter()
                .map(|unit| (unit.pointer, unit.text))
                .collect(),
            other => panic!("expected units, got {other:?}"),
        }
    }

    #[test]
    fn human_queued_command_projects_user_text_at_the_attachment_pointer() {
        let human = attachment(json!("不要使用 model-x 模型"), "prompt", "human");
        assert_eq!(
            texts(project_record(&human)),
            [(
                "/attachment/prompt".to_string(),
                "不要使用 model-x 模型".to_string()
            )]
        );

        let blocks = attachment(
            json!([
                { "type": "text", "text": "先看这个缺口" },
                { "type": "image", "source": { "type": "base64", "media_type": "image/png", "data": "iVBORw0KGgo=" } },
                { "type": "text", "text": "再改配置" }
            ]),
            "prompt",
            "human",
        );
        assert_eq!(
            texts(project_record(&blocks)),
            [
                (
                    "/attachment/prompt/0/text".to_string(),
                    "先看这个缺口".to_string()
                ),
                (
                    "/attachment/prompt/2/text".to_string(),
                    "再改配置".to_string()
                ),
            ],
            "image blocks produce no text unit"
        );
    }

    #[test]
    fn peer_and_task_notification_are_never_user_text() {
        let peer = attachment(json!("另一会话的指示"), "prompt", "peer");
        assert_eq!(project_record(&peer), ClaudeRecordOutcome::Injected);

        let notification = attachment(json!("任务已完成"), "task-notification", "task-notification");
        assert_eq!(
            project_record(&notification),
            ClaudeRecordOutcome::Ignored {
                type_literal: "attachment".to_string()
            }
        );

        let other = json!({ "type": "attachment", "attachment": { "type": "deferred_tools_delta" } });
        assert_eq!(
            project_record(&other),
            ClaudeRecordOutcome::Ignored {
                type_literal: "attachment".to_string()
            }
        );
    }

    #[test]
    fn the_attachment_time_is_the_fallback_clock() {
        let mut value = attachment(json!("中途补充"), "prompt", "human");
        value.as_object_mut().unwrap().remove("timestamp");
        // 2026-10-31T17:38:51.169Z
        assert_eq!(record_meta(&value).timestamp_ms, Some(1_793_468_331_169));
    }

    #[test]
    fn the_dedup_prefers_the_ordinary_user_record_in_either_order() {
        let mut dedup = QueuedDedup::default();
        // queued first, ordinary later
        assert_eq!(
            dedup.observe(Some("/attachment/prompt"), false, "用 pnpm 安装"),
            QueuedVerdict::Hold
        );
        assert_eq!(dedup.observe(None, true, "  用 pnpm 安装  "), QueuedVerdict::Keep);
        assert!(!dedup.release("用 pnpm 安装"), "被普通记录取消");

        // ordinary first, queued later
        assert_eq!(dedup.observe(None, true, "改用 rustls"), QueuedVerdict::Keep);
        assert_eq!(
            dedup.observe(Some("/attachment/prompt/0/text"), false, "改用 rustls"),
            QueuedVerdict::Drop
        );

        // an unrelated prompt survives, and each held occurrence is released once
        assert_eq!(
            dedup.observe(Some("/attachment/prompt"), false, "顺便跑测试"),
            QueuedVerdict::Hold
        );
        assert_eq!(
            dedup.observe(Some("/attachment/prompt"), false, "顺便跑测试"),
            QueuedVerdict::Hold
        );
        assert!(dedup.release("顺便跑测试"));
        assert!(dedup.release("顺便跑测试"));
        assert!(!dedup.release("顺便跑测试"));
    }
}
