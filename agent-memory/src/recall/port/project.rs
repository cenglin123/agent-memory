//! Part projection (§3.2): `project_part_text(block)` is the single definition
//! of a part's text. `_part_text` / `project_content` / `extract_text` are not
//! used as definitions here, and no `preview` path exists.

use crate::recall::port::model::PartData;
use serde_json::Value;

/// Outcome of classifying one part body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PartOutcome {
    /// A known text unit. `projection` may be empty (a legal empty block).
    Text { projection: String, is_tool: bool },
    /// Excluded by the injected predicate (type literal, `synthetic`, `origin`).
    Injected,
    /// A non-text unit type, skipped and counted by type literal.
    Ignored { type_literal: String },
    /// Structural damage in a known `text` / `tool` unit.
    Unresolved { reason: &'static str },
}

impl PartOutcome {
    pub fn projection(&self) -> Option<&str> {
        match self {
            PartOutcome::Text { projection, .. } => Some(projection.as_str()),
            _ => None,
        }
    }

    pub fn is_legal_empty(&self) -> bool {
        matches!(self, PartOutcome::Text { projection, .. } if projection.is_empty())
    }
}

/// The three-predicate injection rule, evaluated in order.
pub fn is_injected(block: &Value) -> bool {
    if block.get("type").and_then(Value::as_str) == Some("injected") {
        return true;
    }
    if block.get("synthetic").and_then(Value::as_bool) == Some(true) {
        return true;
    }
    matches!(
        block.get("origin").and_then(Value::as_str),
        Some("injected") | Some("system")
    )
}

fn block_type(block: &Value) -> Option<&'static str> {
    match block.get("type").and_then(Value::as_str) {
        Some("text") => Some("text"),
        Some("tool") => Some("tool"),
        _ => None,
    }
}

pub fn classify_part(data: &PartData) -> PartOutcome {
    match data {
        PartData::SessionText(value) => {
            if is_injected(value) {
                return PartOutcome::Injected;
            }
            match value {
                Value::String(text) => PartOutcome::Text {
                    projection: text.clone(),
                    is_tool: false,
                },
                _ => PartOutcome::Unresolved {
                    reason: "data_text_not_string",
                },
            }
        }
        PartData::Block(block) => {
            if is_injected(block) {
                return PartOutcome::Injected;
            }
            match block_type(block) {
                Some("text") => match block.get("text") {
                    Some(Value::String(text)) => PartOutcome::Text {
                        projection: text.clone(),
                        is_tool: false,
                    },
                    _ => PartOutcome::Unresolved {
                        reason: "data_text_not_string",
                    },
                },
                Some("tool") => project_tool(block),
                None => match block.get("type").and_then(Value::as_str) {
                    Some(literal) => PartOutcome::Ignored {
                        type_literal: literal.to_string(),
                    },
                    // No type literal at all: still not a text unit, but there
                    // is no literal to count it under.
                    None => PartOutcome::Ignored {
                        type_literal: String::new(),
                    },
                },
                Some(_) => unreachable!(),
            }
        }
    }
}

/// Tool name of a tool part: V1 stores it in `tool`, V2 (`session_message`) in `name`.
pub(crate) fn tool_part_name(block: &Value) -> Option<String> {
    match block.get("tool").or_else(|| block.get("name")) {
        Some(Value::String(name)) => Some(name.clone()),
        _ => None,
    }
}

/// Tool output of a tool part: V1 stores a string in `state.output`; V2 stores
/// `state.content` as a string or an array of `{type:"text", text}` blocks.
/// `Err` means a present but unprojectable value.
pub(crate) fn tool_part_output(state: Option<&Value>) -> Result<String, &'static str> {
    match state.and_then(|state| state.get("output")) {
        Some(Value::String(output)) => return Ok(output.clone()),
        Some(_) => return Err("data_state_output_not_string"),
        None => {}
    }
    match state.and_then(|state| state.get("content")) {
        None | Some(Value::Null) => Ok(String::new()),
        Some(Value::String(content)) => Ok(content.clone()),
        Some(Value::Array(blocks)) => Ok(blocks
            .iter()
            .filter_map(|block| match block.get("text") {
                Some(Value::String(text)) => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")),
        Some(_) => Err("data_state_content_not_text"),
    }
}

fn project_tool(block: &Value) -> PartOutcome {
    let name = match tool_part_name(block) {
        Some(name) => name,
        None => {
            return PartOutcome::Unresolved {
                reason: "data_tool_not_string",
            }
        }
    };
    let state = block.get("state");
    let input = state
        .and_then(|state| state.get("input"))
        .cloned()
        .unwrap_or_else(|| Value::Object(serde_json::Map::new()));
    let output = match tool_part_output(state) {
        Ok(output) => output,
        Err(reason) => return PartOutcome::Unresolved { reason },
    };
    let mut projection = String::new();
    projection.push_str(&name);
    projection.push('\n');
    projection.push_str(&compact_json(&input));
    projection.push('\n');
    projection.push_str(&output);
    PartOutcome::Text {
        projection,
        is_tool: true,
    }
}

/// Compact JSON with every nested object's keys sorted ascending, no spaces,
/// UTF-8 preserved, arrays in original order.
pub fn compact_json(value: &Value) -> String {
    let mut out = String::new();
    write_compact(value, &mut out);
    out
}

fn write_compact(value: &Value, out: &mut String) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Number(number) => out.push_str(&number.to_string()),
        Value::String(text) => out.push_str(&serde_json::to_string(text).expect("string")),
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_compact(item, out);
            }
            out.push(']');
        }
        Value::Object(map) => {
            out.push('{');
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            for (index, key) in keys.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                out.push_str(&serde_json::to_string(key).expect("key"));
                out.push(':');
                write_compact(&map[*key], out);
            }
            out.push('}');
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tool_projection(block: Value) -> String {
        match classify_part(&PartData::Block(block)) {
            PartOutcome::Text { projection, is_tool: true } => projection,
            _ => panic!("expected a tool projection"),
        }
    }

    #[test]
    fn v1_tool_projection_is_unchanged() {
        let block = json!({"type": "tool", "tool": "bash",
            "state": {"input": {"command": "ls"}, "output": "a.txt"}});
        assert_eq!(tool_projection(block), "bash\n{\"command\":\"ls\"}\na.txt");
    }

    #[test]
    fn v2_tool_part_uses_name_and_content_blocks() {
        let block = json!({"type": "tool", "id": "t1", "name": "shell", "executed": false,
            "state": {"status": "completed",
                      "input": {"command": "python-3.13.17-amd64.exe /quiet PrependPath=1"},
                      "content": [{"type": "text", "text": "安装器退出码: 0"},
                                  {"type": "text", "text": "done"}]}});
        assert_eq!(
            tool_projection(block),
            "shell\n{\"command\":\"python-3.13.17-amd64.exe /quiet PrependPath=1\"}\n安装器退出码: 0\ndone"
        );
    }

    #[test]
    fn v2_tool_part_without_output_still_projects_its_input() {
        let block = json!({"type": "tool", "name": "read",
            "state": {"status": "running", "input": {"path": "a.md"}}});
        assert_eq!(tool_projection(block), "read\n{\"path\":\"a.md\"}\n");
    }

    #[test]
    fn compact_json_sorts_nested_objects_and_keeps_array_order() {
        let value = json!({"b": 1, "a": {"z": [3, {"y": 2, "x": 1}], "c": "中"}});
        assert_eq!(
            compact_json(&value),
            r#"{"a":{"c":"中","z":[3,{"x":1,"y":2}]},"b":1}"#
        );
    }

    #[test]
    fn text_part_projection_is_verbatim() {
        let data = PartData::Block(json!({"type": "text", "text": "a\r\nb"}));
        assert_eq!(
            classify_part(&data),
            PartOutcome::Text {
                projection: "a\r\nb".into(),
                is_tool: false
            }
        );
    }

    #[test]
    fn tool_projection_uses_full_output_and_input() {
        let long = "x".repeat(20_000);
        let data = PartData::Block(json!({
            "type": "tool",
            "tool": "bash",
            "state": {"input": {"z": 1, "a": "mid"}, "output": long}
        }));
        let outcome = classify_part(&data);
        let projection = outcome.projection().expect("tool projects");
        assert!(projection.starts_with("bash\n{\"a\":\"mid\",\"z\":1}\n"));
        assert!(projection.ends_with(&long));
    }

    #[test]
    fn tool_defaults_and_damage() {
        let data = PartData::Block(json!({"type": "tool", "tool": "read"}));
        assert_eq!(
            classify_part(&data),
            PartOutcome::Text {
                projection: "read\n{}\n".into(),
                is_tool: true
            }
        );

        let data = PartData::Block(json!({
            "type": "tool", "tool": "bash", "state": {"output": 42}
        }));
        assert_eq!(
            classify_part(&data),
            PartOutcome::Unresolved {
                reason: "data_state_output_not_string"
            }
        );

        let data = PartData::Block(json!({"type": "text", "text": 7}));
        assert_eq!(
            classify_part(&data),
            PartOutcome::Unresolved {
                reason: "data_text_not_string"
            }
        );
    }

    #[test]
    fn injected_predicate_has_three_hits_and_two_non_hits() {
        for block in [
            json!({"type": "injected", "text": "x"}),
            json!({"type": "text", "text": "x", "synthetic": true}),
            json!({"type": "text", "text": "x", "origin": "system"}),
        ] {
            assert!(is_injected(&block), "{block} must be injected");
        }
        for block in [
            json!({"type": "text", "text": "x", "synthetic": "true"}),
            json!({"type": "text", "text": "x", "origin": "user"}),
        ] {
            assert!(!is_injected(&block), "{block} must not be injected");
        }
    }

    #[test]
    fn metadata_types_are_ignored_by_literal() {
        for literal in [
            "snapshot", "patch", "file", "agent", "compaction", "retry", "subtask", "widget",
            "reasoning", "step",
        ] {
            let data = PartData::Block(json!({"type": literal}));
            assert_eq!(
                classify_part(&data),
                PartOutcome::Ignored {
                    type_literal: literal.to_string()
                }
            );
        }
    }
}
