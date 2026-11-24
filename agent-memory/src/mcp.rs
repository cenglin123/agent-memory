//! stdio MCP wrapper (JSON-RPC 2.0, newline-delimited).
//!
//! This is a thin transport: the three recall tools call the same
//! [`crate::recall::execute`] core as the CLI, so `code`, exit codes,
//! pagination, `warnings[]` and `isError` are identical by construction. A
//! fourth read-only tool, `memory_qualified`, calls the same learning-store
//! reader as `learning qualified`. Every declared tool carries
//! `readOnlyHint: true`; `memory_read` is `false` because a device read appends
//! one local feedback line under the cache root (the source stays read-only).
//!
//! `memory_qualified` does not take a learning root argument. The root is the
//! `learning` subdirectory of the `recall.json` directory
//! (`<user-home>/.agent-memory/learning` in production); no other path is
//! guessed. A missing or unreadable store is a tool error with a stable code.

use crate::recall::contract::{
    codes, is_error_code, CONTRACT_VERSION, MCP_TOOL_NAMES,
};
use crate::recall::{execute, load_config, ExecutionOptions, Op};
use serde_json::{json, Value};
use std::io::{BufRead, Write};
use std::path::Path;

pub const PROTOCOL_VERSION: &str = "2024-11-05";
pub const PARSE_ERROR: i32 = -32700;
pub const INVALID_PARAMS: i32 = -32602;
pub const UNKNOWN_METHOD: i32 = -32601;

/// Run the server until stdin closes. Returns the process exit code.
pub fn run_stdio(config_path: &Path, strict_snapshot: bool) -> i32 {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    let options = ExecutionOptions { strict_snapshot };
    for line in stdin.lock().lines() {
        let Ok(line) = line else {
            return 0;
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let response = match serde_json::from_str::<Value>(trimmed) {
            Ok(message) => handle_message(&message, config_path, &options),
            Err(err) => Some(error_response(
                &Value::Null,
                PARSE_ERROR,
                &format!("invalid JSON: {err}"),
                None,
            )),
        };
        if let Some(response) = response {
            if writeln!(stdout, "{response}").and_then(|_| stdout.flush()).is_err() {
                return 0;
            }
        }
    }
    0
}

fn handle_message(
    message: &Value,
    config_path: &Path,
    options: &ExecutionOptions,
) -> Option<Value> {
    let id = message.get("id").cloned();
    let method = message.get("method").and_then(Value::as_str).unwrap_or_default();
    let params = message.get("params").cloned().unwrap_or(Value::Null);

    // Notifications (no id) never get a response.
    if id.is_none() {
        return None;
    }
    let id = id.unwrap_or(Value::Null);

    match method {
        "initialize" => Some(success_response(
            &id,
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {"tools": {"listChanged": false}},
                "serverInfo": {"name": "agent-memory", "version": env!("CARGO_PKG_VERSION")},
            }),
        )),
        "ping" => Some(success_response(&id, json!({}))),
        "shutdown" => Some(success_response(&id, json!({}))),
        "tools/list" => Some(success_response(&id, tools_list())),
        "tools/call" => Some(call_tool(&id, &params, config_path, options)),
        other => Some(error_response(
            &id,
            UNKNOWN_METHOD,
            &format!("unknown method: {other}"),
            None,
        )),
    }
}

fn tools_list() -> Value {
    let search_schema = json!({
        "type": "object",
        "properties": {
            "query": {"type": "string", "minLength": 1},
            "sources": {"type": "array", "items": {"type": "string"}, "minItems": 1},
            "limit": {"type": "integer", "minimum": 1, "maximum": 100},
            "offset": {"type": "integer", "minimum": 0, "maximum": 10000},
            "snippet_bytes": {"type": "integer", "minimum": 1, "maximum": 8192},
            "session_id": {"type": "string", "minLength": 1, "maxLength": 200},
            "domain": {"type": "string", "minLength": 1, "maxLength": 64},
            "related_hops": {"type": "integer", "minimum": 1, "maximum": 2},
            "mode": {"type": "string", "enum": ["auto", "exact", "hybrid", "legacy"]},
            "since": {"type": "string", "minLength": 1},
            "until": {"type": "string", "minLength": 1},
            "roles": {
                "type": "array",
                "items": {"type": "string", "enum": ["user", "assistant", "tool"]},
                "minItems": 1
            },
            "kinds": {
                "type": "array",
                "items": {"type": "string", "enum": ["text", "tool_input", "tool_output"]},
                "minItems": 1
            },
            "session_kinds": {
                "type": "array",
                "items": {"type": "string", "enum": ["interactive", "worker", "unknown"]},
                "minItems": 1
            },
            "order": {"type": "string", "enum": ["newest", "oldest", "relevance"]},
            "group_by_session": {"type": "boolean"},
            "budget": {
                "type": "object",
                "properties": {
                    "max_bytes_per_source": {"type": "integer"},
                    "max_elapsed_ms_per_source": {"type": "integer"},
                    "kb_top_k": {"type": "integer", "minimum": 1, "maximum": 50}
                },
                "additionalProperties": false
            }
        },
        "required": ["query"],
        "additionalProperties": false
    });
    let read_schema = json!({
        "type": "object",
        "properties": {
            "ref": {"type": "object"},
            "context_before": {"type": "integer", "minimum": 0, "maximum": 1000},
            "context_after": {"type": "integer", "minimum": 0, "maximum": 1000},
            "max_bytes": {"type": "integer", "minimum": 1, "maximum": 1048576},
            "domain": {"type": "string", "minLength": 1, "maxLength": 64},
            "context": {"type": "string", "enum": ["turn"]},
            "session_id": {"type": "string", "minLength": 1, "maxLength": 200,
                           "description": "Optional caller session id; device reads use it only for local reuse dedup."}
        },
        "required": ["ref"],
        "additionalProperties": false
    });
    let qualified_schema = json!({
        "type": "object",
        "properties": {
            "state": {
                "type": "string",
                "enum": ["rejected", "deferred", "cleared"],
                "description": "Only pass candidates whose newest qualification has this state."
            },
            "query": {
                "type": "string",
                "minLength": 1,
                "description": "Normalized substring match against the candidate claim and the qualification basis/counterexample."
            },
            "limit": {"type": "integer", "minimum": 1, "maximum": 200}
        },
        "additionalProperties": false
    });
    let recent_schema = json!({
        "type": "object",
        "properties": {
            "sources": {
                "type": "array",
                "items": {"type": "string", "minLength": 1},
                "minItems": 1,
                "description": "Session source ids: the source_id of session-type sources in recall.json (default install: claude_code, opencode, codex, dsh, with underscores). Omit to include every enabled session source."
            },
            "session_id": {
                "type": "string",
                "minLength": 1,
                "description": "Return only this session (any kind), looked up directly in each source; combine with tail_turns to read its last turns."
            },
            "limit": {"type": "integer", "minimum": 1, "maximum": 20},
            "session_kinds": {
                "type": "array",
                "items": {"type": "string", "enum": ["interactive", "worker", "unknown"]},
                "minItems": 1
            },
            "exclude_session_ids": {
                "type": "array",
                "items": {"type": "string", "minLength": 1},
                "minItems": 1
            },
            "since": {"type": "string", "minLength": 1},
            "tail_turns": {"type": "integer", "minimum": 0, "maximum": 20},
            "tail_roles": {
                "type": "array",
                "items": {"type": "string", "enum": ["user", "assistant", "tool"]},
                "minItems": 1
            },
            "max_chars_per_turn": {"type": "integer", "minimum": 1, "maximum": 8000},
            "max_bytes": {"type": "integer", "minimum": 1, "maximum": 4194304}
        },
        "additionalProperties": false
    });
    json!({
        "tools": [
            {
                "name": MCP_TOOL_NAMES[0],
                "description": "Read-only unified recall across KB, device notes, OpenCode, Codex, Claude Code and DSH history; optional session_id filtering, no Archive required. Identifier lookups (error codes, paths, version numbers, parameters, session ids) use mode=\"exact\": the local trigram index with AND-ed terms, filters (since/until/roles/kinds/session_kinds), ordering (newest/oldest/relevance) and group_by_session. Conceptual, fuzzy or reworded recall uses the default mode: full-text bm25 and local conversation vectors fused by rank, deduped by session; it accepts since/until/roles/kinds/session_kinds, and order or group_by_session makes it run as exact (detail.resolved_mode says which path answered). A source without an index reports INDEX_REQUIRED for those requests: run agent-memory index. mode=\"legacy\" forces the old scan and rejects those keys. Use memory_read on a hit ref to fetch the surrounding text; context=\"turn\" returns the whole turn where the source proves turn order. When a result set is empty, check source_status[].detail.coverage first: the index may be behind the source data (stale_since).",
                "inputSchema": search_schema,
                "annotations": {"readOnlyHint": true}
            },
            {
                "name": MCP_TOOL_NAMES[1],
                "description": "Readback of a recall reference with context and byte budget. The source is never modified; a device read appends one local feedback line under the cache root (reuse counter, no content), so this tool is not strictly read-only.",
                "inputSchema": read_schema,
                "annotations": {"readOnlyHint": false, "destructiveHint": false, "idempotentHint": true}
            },
            {
                "name": MCP_TOOL_NAMES[2],
                "description": "Read-only, real-time (no index) listing of the most recently active interactive sessions across OpenCode, Codex, Claude Code and DSH, with each session's last turns; every tail ref can be handed to memory_read for the full text. Times ending in Z are UTC; when telling the user when a session was last active, use last_activity_local.",
                "inputSchema": recent_schema,
                "annotations": {"readOnlyHint": true}
            },
            {
                "name": MCP_TOOL_NAMES[3],
                "description": "Read-only list of knowledge candidates that currently carry a qualification: rejected (disproved; do not use as a premise), deferred (cannot be judged yet; owner and re-review trigger recorded) or cleared (previous qualification withdrawn). The learning root is the `learning` directory next to the configured recall.json; there is no root argument. Use it before presenting a conclusion to check whether that conclusion was already rejected.",
                "inputSchema": qualified_schema,
                "annotations": {"readOnlyHint": true}
            }
        ]
    })
}

fn call_tool(
    id: &Value,
    params: &Value,
    config_path: &Path,
    options: &ExecutionOptions,
) -> Value {
    let Some(name) = params.get("name").and_then(Value::as_str) else {
        return error_response(id, INVALID_PARAMS, "tools/call requires a tool name", None);
    };
    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    if name == MCP_TOOL_NAMES[3] {
        return call_qualified(id, &arguments, config_path);
    }
    let Some(op) = Op::parse(name) else {
        return error_response(
            id,
            UNKNOWN_METHOD,
            &format!("unknown tool: {name}"),
            None,
        );
    };

    let executed = match load_config(config_path) {
        Ok(config) => execute(&config, op, &arguments, options),
        Err(error) => crate::recall::ops::config_failure(op, &error),
    };

    if executed.exit_code == 2 {
        // `INVALID_REQUEST` / `INVALID_OFFSET`: protocol-level invalid params
        // *and* `isError:true`, with the frozen envelope in `data`.
        let code = first_error_code(&executed.value);
        return error_response(
            id,
            INVALID_PARAMS,
            &format!("invalid params: {code}"),
            Some(json!({
                "code": code,
                "isError": true,
                "response": executed.value,
            })),
        );
    }

    let text = executed.json();
    success_response(
        id,
        json!({
            "content": [{"type": "text", "text": text}],
            "structuredContent": executed.value,
            "isError": executed.is_error,
        }),
    )
}

/// `memory_qualified`: the fourth read-only tool. It reads the learning store
/// that sits next to the configured `recall.json` (`<recall-dir>/learning`);
/// no root argument and no other path guess.
fn call_qualified(id: &Value, arguments: &Value, config_path: &Path) -> Value {
    let Some(object) = arguments.as_object() else {
        return qualified_error(id, "INVALID_USAGE", "arguments must be a JSON object");
    };
    for key in object.keys() {
        if !matches!(key.as_str(), "state" | "query" | "limit") {
            return qualified_error(
                id,
                "INVALID_USAGE",
                &format!("unknown field {key:?}; memory_qualified accepts state, query and limit"),
            );
        }
    }
    let state = match object.get("state") {
        None => None,
        Some(Value::String(value)) => {
            if !crate::learning::QUALIFY_STATES.contains(&value.as_str()) {
                return qualified_error(
                    id,
                    "INVALID_USAGE",
                    &format!(
                        "state must be one of {:?}, got {value:?}",
                        crate::learning::QUALIFY_STATES
                    ),
                );
            }
            Some(value.clone())
        }
        Some(_) => return qualified_error(id, "INVALID_USAGE", "state must be a string"),
    };
    let query = match object.get("query") {
        None => None,
        Some(Value::String(value)) if !value.trim().is_empty() => Some(value.clone()),
        Some(Value::String(_)) => {
            return qualified_error(id, "INVALID_USAGE", "query must not be empty")
        }
        Some(_) => return qualified_error(id, "INVALID_USAGE", "query must be a string"),
    };
    let limit = match object.get("limit") {
        None => 50usize,
        Some(Value::Number(number)) => match number.as_u64() {
            Some(value) if (1..=200).contains(&value) => value as usize,
            _ => {
                return qualified_error(
                    id,
                    "INVALID_USAGE",
                    "limit must be an integer between 1 and 200",
                )
            }
        },
        Some(_) => {
            return qualified_error(id, "INVALID_USAGE", "limit must be an integer")
        }
    };

    // `--config` may be relative. Resolve it against the process cwd first so
    // the derived root is always absolute (the learning store rejects relative
    // roots on purpose).
    let config_path = std::path::absolute(config_path)
        .unwrap_or_else(|_| config_path.to_path_buf());
    let learning_root = config_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("learning");
    if !learning_root.is_dir() {
        return qualified_error(
            id,
            "LEARNING_ROOT_MISSING",
            &format!(
                "learning root {} does not exist; memory_qualified never guesses another path",
                learning_root.display()
            ),
        );
    }
    if !learning_root.join(crate::learning::DB_FILE).is_file() {
        return qualified_error(
            id,
            "LEARNING_DB_MISSING",
            &format!(
                "learning root {} holds no {}; run `agent-memory learning init --root <root>` \
                 first",
                learning_root.display(),
                crate::learning::DB_FILE
            ),
        );
    }
    let store = match crate::learning::LearningStore::open_read_only(&learning_root) {
        Ok(store) => store,
        Err(error) => return qualified_error(id, error.code, &error.message),
    };
    let report = match store.qualified(state.as_deref(), query.as_deref(), Some(limit)) {
        Ok(report) => report,
        Err(error) => return qualified_error(id, error.code, &error.message),
    };
    let value = serde_json::to_value(&report).expect("qualified report serializes");
    let text = serde_json::to_string(&value).expect("qualified report serializes");
    success_response(
        id,
        json!({
            "content": [{"type": "text", "text": text}],
            "structuredContent": value,
            "isError": false,
        }),
    )
}

/// A `tools/call` result that failed inside the tool (not a protocol error):
/// `isError: true` and a stable `{ok:false, error:{code,message}}` payload.
fn qualified_error(id: &Value, code: &str, message: &str) -> Value {
    success_response(
        id,
        json!({
            "content": [{"type": "text", "text": format!("{code}: {message}")}],
            "structuredContent": {
                "ok": false,
                "error": {"code": code, "message": message}
            },
            "isError": true,
        }),
    )
}

fn first_error_code(value: &Value) -> String {
    if let Some(code) = value
        .get("errors")
        .and_then(Value::as_array)
        .and_then(|errors| errors.first())
        .and_then(|entry| entry.get("code"))
        .and_then(Value::as_str)
    {
        return code.to_string();
    }
    value
        .get("code")
        .and_then(Value::as_str)
        .unwrap_or(codes::INVALID_REQUEST)
        .to_string()
}

fn success_response(id: &Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn error_response(id: &Value, code: i32, message: &str, data: Option<Value>) -> Value {
    let mut error = json!({"code": code, "message": message});
    if let Some(data) = data {
        error["data"] = data;
    }
    json!({"jsonrpc": "2.0", "id": id, "error": error})
}

/// Kept as a single source of truth for the contract version echoed in tool
/// results; the wrapper never invents a second version.
pub fn contract_version() -> u32 {
    CONTRACT_VERSION
}

/// True when a response code makes `isError:true`. Exported so the MCP tests
/// assert the same table as the core instead of restating it.
pub fn code_is_error(code: &str) -> bool {
    is_error_code(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_list_has_four_tools_and_readback_is_not_pure_read_only() {
        let list = tools_list();
        let tools = list["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 4);
        let names: Vec<&str> = tools
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, MCP_TOOL_NAMES.to_vec());
        let read_only = tools
            .iter()
            .filter(|tool| tool["annotations"]["readOnlyHint"] == json!(true))
            .count();
        assert_eq!(read_only, 3);
        let readback = tools
            .iter()
            .find(|tool| tool["name"] == MCP_TOOL_NAMES[1])
            .unwrap();
        assert_eq!(readback["annotations"]["readOnlyHint"], json!(false));
        assert_eq!(readback["annotations"]["destructiveHint"], json!(false));
    }

    #[test]
    fn error_code_table_is_shared_with_the_core() {
        assert!(code_is_error(codes::INVALID_REQUEST));
        assert!(code_is_error(codes::KB_CONTRACT_UNSUPPORTED));
        assert!(!code_is_error(codes::CONTENT_UNRESOLVED));
        assert!(!code_is_error(codes::RECORD_SKIPPED));
    }

    #[test]
    fn protocol_error_codes_are_fixed() {
        assert_eq!(PARSE_ERROR, -32700);
        assert_eq!(INVALID_PARAMS, -32602);
        assert_eq!(UNKNOWN_METHOD, -32601);
        assert_eq!(contract_version(), 2);
    }
}
