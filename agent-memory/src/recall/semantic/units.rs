//! Retrieval units (§3.5): the text a vector actually covers.
//!
//! A unit is not the whole decoded record. For `claude` / `codex` JSONL records
//! the provider fields are projected: user/assistant message text, one tool call,
//! one tool result. Records whose shape is unknown fall back to the decoded record
//! text, so no evidence disappears silently. Over-long text is split on structural
//! boundaries (blank lines, then lines) at [`UNIT_MAX_BYTES`].
//!
//! The unit identity is `sha256(normalized text | unit type | parser generation)`
//! and a unit is deduplicated inside its provider family. Units are rebuilt from
//! the same committed records at replay and at query time, so the identity is
//! recomputed instead of stored twice.

use serde_json::Value;

/// Unit parser generation. Changing the unit rules must change this literal so an
/// older index cannot be read as if it were current.
pub const UNIT_PARSER: &str = "archive-unit-v3";
pub const UNIT_MAX_BYTES: usize = 2_048;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnitPiece {
    /// `message:<role>` | `tool_call` | `tool_result` | `record` | `summary`
    pub unit_type: String,
    pub text: String,
}

/// Provider-aware extraction into one or more retrieval units.
pub fn extract(provider: &str, record: &str) -> Vec<UnitPiece> {
    let parsed: Option<Value> = serde_json::from_str(record).ok();
    let pieces = match parsed.as_ref() {
        Some(value) => match provider {
            "codex" => codex_pieces(value),
            "claude" => claude_pieces(value),
            _ => generic_pieces(value),
        },
        None => None,
    };
    let pieces = match pieces {
        Some(pieces) => pieces,
        // Unknown provider shape (or unparseable record): keep the decoded text as
        // the unit rather than pretending the record has no body. Recognized
        // metadata-only records return `Some(vec![])` and are skipped on purpose:
        // session_meta/cwd/context JSON is not evidence and only adds noise.
        None => vec![UnitPiece {
            unit_type: "record".to_string(),
            text: record.to_string(),
        }],
    };
    pieces
        .into_iter()
        .flat_map(|piece| split_long(&piece.unit_type, &piece.text))
        .filter(|piece| !piece.text.trim().is_empty())
        .collect()
}

/// Does this committed record open a new conversational turn?
///
/// A turn is one user message plus every following assistant/tool record until
/// the next user message. Claude records tool results as `type=user`; a record
/// whose content is only `tool_result` blocks does not open a turn. Metadata
/// records never do.
pub fn is_user_turn_record(provider: &str, record: &str) -> bool {
    let Ok(value) = serde_json::from_str::<Value>(record) else {
        return false;
    };
    let record_type = value.get("type").and_then(Value::as_str).unwrap_or("");
    match provider {
        "codex" => {
            let payload = value.get("payload").unwrap_or(&Value::Null);
            if record_type == "response_item" {
                return payload.get("type").and_then(Value::as_str) == Some("message")
                    && payload.get("role").and_then(Value::as_str) == Some("user");
            }
            record_type == "event_msg"
                && payload.get("type").and_then(Value::as_str) == Some("user_message")
        }
        "claude" => {
            if record_type != "user" {
                return false;
            }
            let message = value.get("message").unwrap_or(&Value::Null);
            match message.get("content") {
                Some(Value::String(text)) => !text.trim().is_empty(),
                Some(Value::Array(blocks)) => blocks.iter().any(|block| {
                    block.get("type").and_then(Value::as_str) != Some("tool_result")
                }),
                Some(_) => true,
                None => false,
            }
        }
        _ => false,
    }
}

/// Normalized unit text (CRLF folded, whitespace runs collapsed).
pub fn normalize_unit_text(text: &str) -> String {
    text.replace("\r\n", "\n")
        .replace('\r', "\n")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn unit_sha256(normalized: &str, unit_type: &str) -> String {
    let mut canonical = Vec::with_capacity(normalized.len() + unit_type.len() + UNIT_PARSER.len() + 2);
    canonical.extend_from_slice(normalized.as_bytes());
    canonical.push(0);
    canonical.extend_from_slice(unit_type.as_bytes());
    canonical.push(0);
    canonical.extend_from_slice(UNIT_PARSER.as_bytes());
    crate::archive::hex_sha256(&canonical)
}

fn split_long(unit_type: &str, text: &str) -> Vec<UnitPiece> {
    if text.len() <= UNIT_MAX_BYTES {
        return vec![UnitPiece {
            unit_type: unit_type.to_string(),
            text: text.to_string(),
        }];
    }
    let mut out = Vec::new();
    let mut current = String::new();
    for paragraph in text.split_inclusive("\n\n") {
        for line in paragraph.split_inclusive('\n') {
            if current.len() + line.len() <= UNIT_MAX_BYTES {
                current.push_str(line);
                continue;
            }
            if !current.trim().is_empty() {
                out.push(UnitPiece {
                    unit_type: unit_type.to_string(),
                    text: current.trim_end_matches(['\r', '\n']).to_string(),
                });
            }
            current.clear();
            if line.len() <= UNIT_MAX_BYTES {
                current.push_str(line);
                continue;
            }
            // A single structural line is still too long: hard-split on a UTF-8
            // boundary so no unit exceeds the budget.
            let mut rest = line;
            while rest.len() > UNIT_MAX_BYTES {
                let cut = crate::recall::text::utf8_floor(rest, UNIT_MAX_BYTES);
                let cut = if cut == 0 { UNIT_MAX_BYTES.min(rest.len()) } else { cut };
                out.push(UnitPiece {
                    unit_type: unit_type.to_string(),
                    text: rest[..cut].trim_end_matches(['\r', '\n']).to_string(),
                });
                rest = &rest[cut..];
            }
            current.push_str(rest);
        }
        if !current.trim().is_empty() && current.len() >= UNIT_MAX_BYTES / 2 {
            out.push(UnitPiece {
                unit_type: unit_type.to_string(),
                text: current.trim_end_matches(['\r', '\n']).to_string(),
            });
            current.clear();
        }
    }
    if !current.trim().is_empty() {
        out.push(UnitPiece {
            unit_type: unit_type.to_string(),
            text: current.trim_end_matches(['\r', '\n']).to_string(),
        });
    }
    out
}

// ---------------------------------------------------------------------------
// Codex rollouts
// ---------------------------------------------------------------------------

fn codex_pieces(value: &Value) -> Option<Vec<UnitPiece>> {
    let record_type = value.get("type").and_then(Value::as_str).unwrap_or("");
    let payload = value.get("payload").unwrap_or(&Value::Null);
    let pieces = match record_type {
        "session_meta" | "turn_context" => Vec::new(),
        "response_item" => {
            let item_type = payload.get("type").and_then(Value::as_str).unwrap_or("");
            match item_type {
                "message" => {
                    let role = payload.get("role").and_then(Value::as_str).unwrap_or("unknown");
                    let text = content_text(payload.get("content"));
                    one(format!("message:{role}"), text)
                }
                "function_call" | "custom_tool_call" => {
                    let name = payload.get("name").and_then(Value::as_str).unwrap_or("tool");
                    let input = payload
                        .get("arguments")
                        .or_else(|| payload.get("input"))
                        .map(render)
                        .unwrap_or_default();
                    one("tool_call".to_string(), format!("{name} {input}"))
                }
                "function_call_output" | "custom_tool_call_output" => {
                    let output = payload.get("output").map(render).unwrap_or_default();
                    one("tool_result".to_string(), output)
                }
                "local_shell_call" => {
                    let command = payload
                        .get("action")
                        .and_then(|action| action.get("command"))
                        .map(render)
                        .unwrap_or_default();
                    one("tool_call".to_string(), command)
                }
                "reasoning" => {
                    let summary = payload
                        .get("summary")
                        .map(|value| content_text(Some(value)))
                        .unwrap_or_default();
                    one("thinking".to_string(), summary)
                }
                other => one(format!("record:{other}"), render(value)),
            }
        }
        "event_msg" => {
            let event_type = payload.get("type").and_then(Value::as_str).unwrap_or("");
            match event_type {
                "agent_message" => one(
                    "message:assistant".to_string(),
                    payload
                        .get("message")
                        .map(render)
                        .unwrap_or_default(),
                ),
                "user_message" => one(
                    "message:user".to_string(),
                    payload
                        .get("message")
                        .map(render)
                        .unwrap_or_default(),
                ),
                "item_completed" => {
                    let item = payload.get("item").unwrap_or(&Value::Null);
                    let item_type = item.get("type").and_then(Value::as_str).unwrap_or("");
                    match item_type {
                        "CommandExecution" => one(
                            "tool_result".to_string(),
                            item.get("aggregated_output")
                                .map(render)
                                .unwrap_or_default(),
                        ),
                        "McpToolCall" => one(
                            "tool_result".to_string(),
                            item.get("result")
                                .map(|result| content_text(result.get("content")))
                                .filter(|text| !text.is_empty())
                                .unwrap_or_else(|| render(item)),
                        ),
                        _ => one("tool_result".to_string(), render(item)),
                    }
                }
                _ => Vec::new(),
            }
        }
        _ => return None,
    };
    Some(pieces)
}

// ---------------------------------------------------------------------------
// Claude JSONL
// ---------------------------------------------------------------------------

fn claude_pieces(value: &Value) -> Option<Vec<UnitPiece>> {
    let record_type = value.get("type").and_then(Value::as_str).unwrap_or("");
    let pieces = match record_type {
        "user" | "assistant" => {
            let message = value.get("message").unwrap_or(&Value::Null);
            let role = message
                .get("role")
                .and_then(Value::as_str)
                .unwrap_or(record_type);
            let mut out = Vec::new();
            let mut text = String::new();
            if let Some(blocks) = message.get("content").and_then(Value::as_array) {
                for block in blocks {
                    let block_type = block.get("type").and_then(Value::as_str).unwrap_or("");
                    match block_type {
                        "text" => {
                            if let Some(value) = block.get("text").and_then(Value::as_str) {
                                text.push_str(value);
                                text.push('\n');
                            }
                        }
                        "thinking" => {
                            if let Some(value) = block.get("thinking").and_then(Value::as_str) {
                                out.push(UnitPiece {
                                    unit_type: "thinking".to_string(),
                                    text: value.to_string(),
                                });
                            }
                        }
                        "tool_use" => {
                            let name = block.get("name").and_then(Value::as_str).unwrap_or("tool");
                            out.push(UnitPiece {
                                unit_type: "tool_call".to_string(),
                                text: format!(
                                    "{name} {}",
                                    block.get("input").map(render).unwrap_or_default()
                                ),
                            });
                        }
                        "tool_result" => {
                            let content = block
                                .get("content")
                                .map(render)
                                .unwrap_or_default();
                            out.push(UnitPiece {
                                unit_type: "tool_result".to_string(),
                                text: content,
                            });
                        }
                        _ => {}
                    }
                }
            } else if let Some(content) = message.get("content") {
                text.push_str(&content_text(Some(content)));
            }
            if !text.trim().is_empty() {
                out.insert(
                    0,
                    UnitPiece {
                        unit_type: format!("message:{role}"),
                        text,
                    },
                );
            }
            out
        }
        "summary" => one(
            "summary".to_string(),
            value
                .get("summary")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        ),
        "system" | "result" | "file-history-snapshot" => Vec::new(),
        _ => return None,
    };
    Some(pieces)
}

fn generic_pieces(value: &Value) -> Option<Vec<UnitPiece>> {
    let role = value
        .get("role")
        .and_then(Value::as_str)
        .or_else(|| value.get("message").and_then(|message| message.get("role")).and_then(Value::as_str))
        .unwrap_or("unknown");
    let text = value
        .get("content")
        .or_else(|| value.get("text"))
        .or_else(|| value.get("message").and_then(|message| message.get("content")))
        .map(|content| content_text(Some(content)))
        .unwrap_or_default();
    Some(one(format!("message:{role}"), text))
}

fn one(unit_type: String, text: String) -> Vec<UnitPiece> {
    if text.trim().is_empty() {
        Vec::new()
    } else {
        vec![UnitPiece { unit_type, text }]
    }
}

/// String or array of content blocks: `{"text": …}`, `{"content": …}`, plain
/// strings. Never renders container keys, so ids/timestamps stay out of the text.
fn content_text(value: Option<&Value>) -> String {
    let Some(value) = value else {
        return String::new();
    };
    match value {
        Value::String(text) => text.clone(),
        Value::Array(items) => items
            .iter()
            .map(|item| content_text(Some(item)))
            .filter(|text| !text.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Object(map) => {
            if let Some(text) = map.get("text").and_then(Value::as_str) {
                return text.to_string();
            }
            if let Some(content) = map.get("content") {
                return content_text(Some(content));
            }
            if let Some(output) = map.get("output") {
                return content_text(Some(output));
            }
            String::new()
        }
        _ => String::new(),
    }
}

fn render(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn codex_messages_calls_and_results_become_separate_units() {
        let message = json!({
            "type": "response_item",
            "payload": {"type": "message", "role": "user",
                        "content": [{"type": "input_text", "text": "缓存怎么失效"}]}
        })
        .to_string();
        let pieces = extract("codex", &message);
        assert_eq!(pieces.len(), 1);
        assert_eq!(pieces[0].unit_type, "message:user");
        assert!(pieces[0].text.contains("缓存怎么失效"));

        let call = json!({
            "type": "response_item",
            "payload": {"type": "function_call", "name": "shell",
                        "arguments": "{\"command\":\"cargo test\"}"}
        })
        .to_string();
        let pieces = extract("codex", &call);
        assert_eq!(pieces[0].unit_type, "tool_call");
        assert!(pieces[0].text.contains("cargo test"));

        let result = json!({
            "type": "response_item",
            "payload": {"type": "function_call_output", "output": "ok 12 passed"}
        })
        .to_string();
        assert_eq!(extract("codex", &result)[0].unit_type, "tool_result");

        let meta = json!({"type": "session_meta", "payload": {"id": "s"}}).to_string();
        // Metadata-only records are not evidence and produce no unit.
        assert!(extract("codex", &meta).is_empty());
    }

    #[test]
    fn claude_blocks_are_projected_not_dumped() {
        let record = json!({
            "type": "assistant",
            "message": {"role": "assistant", "content": [
                {"type": "text", "text": "先看缓存命中率"},
                {"type": "tool_use", "name": "Read", "input": {"file_path": "a.rs"}},
                {"type": "tool_result", "content": "file body"}
            ]},
            "uuid": "0000",
            "timestamp": "2026-01-01T00:00:00Z"
        })
        .to_string();
        let pieces = extract("claude", &record);
        let types: Vec<&str> = pieces.iter().map(|piece| piece.unit_type.as_str()).collect();
        assert_eq!(types, vec!["message:assistant", "tool_call", "tool_result"]);
        assert!(!pieces[0].text.contains("0000"));
    }

    #[test]
    fn long_text_is_split_on_structure_within_the_budget() {
        let line = "x".repeat(900);
        let body = format!("{line}\n\n{line}\n\n{line}\n\n{line}");
        let pieces = split_long("tool_result", &body);
        assert!(pieces.len() >= 2);
        assert!(pieces.iter().all(|piece| piece.text.len() <= UNIT_MAX_BYTES));
        // Reassembly keeps every structural character.
        let joined: usize = pieces.iter().map(|piece| piece.text.len()).sum();
        assert!(joined + pieces.len() * 2 >= body.len());
    }

    #[test]
    fn user_turn_detection_matches_the_turn_definition() {
        assert!(is_user_turn_record(
            "codex",
            &json!({"type": "response_item", "payload": {
                "type": "message", "role": "user", "content": "q"}}).to_string()
        ));
        assert!(!is_user_turn_record(
            "codex",
            &json!({"type": "response_item", "payload": {
                "type": "function_call_output", "output": "q"}}).to_string()
        ));
        assert!(!is_user_turn_record(
            "codex",
            &json!({"type": "session_meta", "payload": {"id": "s"}}).to_string()
        ));
        assert!(is_user_turn_record(
            "claude",
            &json!({"type": "user", "message": {"role": "user", "content": "q"}}).to_string()
        ));
        assert!(!is_user_turn_record(
            "claude",
            &json!({"type": "user", "message": {"role": "user", "content": [
                {"type": "tool_result", "content": "ok"}]}}).to_string()
        ));
        assert!(is_user_turn_record(
            "claude",
            &json!({"type": "user", "message": {"role": "user", "content": [
                {"type": "tool_result", "content": "ok"}, {"type": "text", "text": "q"}]}})
                .to_string()
        ));
    }

    #[test]
    fn unit_identity_depends_on_text_type_and_parser_generation() {
        let text = normalize_unit_text("  Hello\r\n world  ");
        assert_eq!(text, "Hello world");
        let first = unit_sha256(&text, "message:user");
        assert_eq!(first, unit_sha256(&text, "message:user"));
        assert_ne!(first, unit_sha256(&text, "message:assistant"));
        assert_ne!(first, unit_sha256("other", "message:user"));
        assert_eq!(first.len(), 64);
    }

    #[test]
    fn invalid_json_falls_back_to_the_decoded_record() {
        let pieces = extract("codex", "not json at all");
        assert_eq!(pieces.len(), 1);
        assert_eq!(pieces[0].unit_type, "record");
        assert_eq!(pieces[0].text, "not json at all");
    }
}
