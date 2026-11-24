//! Stage 3 S2: structured provenance facts and their adjudication.
//!
//! The only source of facts is the committed archive: the `provider` of the
//! manifest that holds the record plus the record bytes themselves. Nothing
//! here trusts a candidate's own labels, and nothing here writes anywhere.
//!
//! # What is adjudicated
//!
//! S2 understands exactly one structured provider, [`CLAUDE_PROVIDER`]. A
//! record of any other provider (or a record that does not parse into the
//! documented shape) has **no** structured facts: [`adjudicate`] then accepts
//! only an all-`unknown` declaration (fail-closed, never a guess).
//!
//! For a Claude Code JSONL record the facts are the top-level `type`,
//! `isMeta`, `isSidechain` and — for a `type == "user"` record — whether
//! `message.content` is an array containing a `tool_result` item. A missing
//! `isMeta` / `isSidechain` is read as `false` (that is how Claude Code writes
//! those flags); a missing or non-boolean `type`, a non-object body, a
//! non-boolean `isMeta` / `isSidechain`, or a user record without a usable
//! `message.content` is a shape failure (`parse_ok = false`).
//!
//! # Rule order
//!
//! The first matching row decides; `isSidechain` and `isMeta` take precedence
//! over the `type` rows, and a meta tool-result record is judged by the meta
//! row. Every row only rejects; it never promotes anything.

use serde::Serialize;
use serde_json::Value;

use super::gate::E_PROVENANCE_MISMATCH;
use super::model::EvidenceItem;

/// The one provider S2 adjudicates structurally.
pub const CLAUDE_PROVIDER: &str = "claude";
/// The fail-closed declaration value.
pub const UNKNOWN: &str = "unknown";

const OWNER: &str = "owner";
const RUNTIME: &str = "runtime";
const AGENT_DERIVED: &str = "agent_derived";
const SUBAGENT: &str = "subagent";
const INTERACTIVE: &str = "interactive";

/// The structured facts an adjudication is based on. `None` means "not
/// established", which is never treated as a passing fact.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct RecordFacts {
    pub provider: Option<String>,
    #[serde(rename = "type")]
    pub record_type: Option<String>,
    #[serde(rename = "isMeta")]
    pub is_meta: Option<bool>,
    #[serde(rename = "isSidechain")]
    pub is_sidechain: Option<bool>,
    pub tool_result: Option<bool>,
    pub parse_ok: Option<bool>,
}

impl RecordFacts {
    /// Nothing was read or decided (format failure, unresolved ref, ...).
    pub fn unknown() -> Self {
        Self::default()
    }

    /// The provider is known but no structured facts could be established
    /// (non-Claude provider or a record that does not parse).
    pub fn unstructured(provider: &str) -> Self {
        Self {
            provider: Some(provider.to_string()),
            record_type: None,
            is_meta: None,
            is_sidechain: None,
            tool_result: None,
            parse_ok: Some(false),
        }
    }

    /// `true` only when the facts are a parsed Claude record.
    pub fn structured(&self) -> bool {
        self.parse_ok == Some(true)
            && self.provider.as_deref() == Some(CLAUDE_PROVIDER)
            && self.record_type.is_some()
    }
}

/// Read the optional boolean flag: absent is `false`, a non-boolean value makes
/// the record shape unusable.
fn flag(value: Option<&Value>) -> Option<bool> {
    match value {
        None => Some(false),
        Some(Value::Bool(flag)) => Some(*flag),
        Some(_) => None,
    }
}

/// Establish the structured facts of one committed record.
///
/// `provider` is the manifest provider; `bytes` are the committed record bytes
/// (never the live source).
pub fn facts_for(provider: &str, bytes: &[u8]) -> RecordFacts {
    if provider != CLAUDE_PROVIDER {
        return RecordFacts::unstructured(provider);
    }
    let unparsed = || RecordFacts::unstructured(provider);
    let Ok(text) = std::str::from_utf8(bytes) else {
        return unparsed();
    };
    let Ok(value) = serde_json::from_str::<Value>(text) else {
        return unparsed();
    };
    let Some(body) = value.as_object() else {
        return unparsed();
    };
    let Some(record_type) = body.get("type").and_then(Value::as_str) else {
        return unparsed();
    };
    let Some(is_meta) = flag(body.get("isMeta")) else {
        return unparsed();
    };
    let Some(is_sidechain) = flag(body.get("isSidechain")) else {
        return unparsed();
    };
    let tool_result = if record_type == "user" {
        let Some(message) = body.get("message").and_then(Value::as_object) else {
            return unparsed();
        };
        match message.get("content") {
            Some(Value::Array(items)) => Some(
                items
                    .iter()
                    .any(|item| item.get("type").and_then(Value::as_str) == Some("tool_result")),
            ),
            Some(Value::String(_)) => Some(false),
            _ => return unparsed(),
        }
    } else {
        None
    };
    RecordFacts {
        provider: Some(provider.to_string()),
        record_type: Some(record_type.to_string()),
        is_meta: Some(is_meta),
        is_sidechain: Some(is_sidechain),
        tool_result,
        parse_ok: Some(true),
    }
}

fn restricted_kind(evidence_kind: &str) -> bool {
    matches!(evidence_kind, "user_statement" | "correction" | "approval")
}

/// Adjudicate one evidence declaration against the record facts.
///
/// `Ok(())` means the declaration is consistent with the committed record;
/// `Err(E_PROVENANCE_MISMATCH)` means it is not. There is no third outcome and
/// no partial credit: an unestablished fact never satisfies a constraint.
pub fn adjudicate(facts: &RecordFacts, item: &EvidenceItem) -> Result<(), &'static str> {
    // Row 1: no structured facts -> every declaration must be `unknown`.
    if !facts.structured() {
        let declared_unknown = item.session_kind == UNKNOWN
            && item.origin_class == UNKNOWN
            && item.evidence_kind == UNKNOWN;
        return if declared_unknown {
            Ok(())
        } else {
            Err(E_PROVENANCE_MISMATCH)
        };
    }

    // Row 2: sidechain records are subagent work, never owner work.
    if facts.is_sidechain == Some(true) {
        return if item.session_kind == SUBAGENT && item.origin_class != OWNER {
            Ok(())
        } else {
            Err(E_PROVENANCE_MISMATCH)
        };
    }

    // Row 3: meta (runtime-injected) records are runtime material.
    if facts.is_meta == Some(true) {
        return if item.origin_class == RUNTIME && !restricted_kind(&item.evidence_kind) {
            Ok(())
        } else {
            Err(E_PROVENANCE_MISMATCH)
        };
    }

    match facts.record_type.as_deref() {
        // Row 4: a tool result is execution material, not an owner statement.
        Some("user") if facts.tool_result == Some(true) => {
            let allowed = matches!(
                item.evidence_kind.as_str(),
                "execution_result" | "test_result" | "external_material" | "unknown"
            );
            if item.origin_class != OWNER && allowed {
                Ok(())
            } else {
                Err(E_PROVENANCE_MISMATCH)
            }
        }
        // Row 5: plain user text is the interactive channel (`owner` allowed).
        Some("user") => {
            if item.session_kind == INTERACTIVE {
                Ok(())
            } else {
                Err(E_PROVENANCE_MISMATCH)
            }
        }
        // Row 6: assistant output is derived, never an owner statement.
        Some("assistant") => {
            if item.origin_class == AGENT_DERIVED && !restricted_kind(&item.evidence_kind) {
                Ok(())
            } else {
                Err(E_PROVENANCE_MISMATCH)
            }
        }
        // Row 7: anything else is runtime material or unknown.
        _ => {
            let origin_ok = matches!(item.origin_class.as_str(), RUNTIME | UNKNOWN);
            if origin_ok && !restricted_kind(&item.evidence_kind) {
                Ok(())
            } else {
                Err(E_PROVENANCE_MISMATCH)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(session_kind: &str, origin_class: &str, evidence_kind: &str) -> EvidenceItem {
        EvidenceItem {
            reference: format!("archive-record:{}", "0".repeat(64)),
            span_sha256: "0".repeat(64),
            archive_generation: "1".to_string(),
            observed_at: "2026-11-01T00:00:00Z".to_string(),
            actor: "user".to_string(),
            session_kind: session_kind.to_string(),
            origin_class: origin_class.to_string(),
            evidence_kind: evidence_kind.to_string(),
            recall_taint: false,
        }
    }

    const USER_TEXT: &[u8] = br#"{"type":"user","message":{"role":"user","content":"hello"}}"#;
    const USER_TOOL: &[u8] = br#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","content":"ok"}]}}"#;
    const ASSISTANT: &[u8] = br#"{"type":"assistant","message":{"role":"assistant","content":[]}}"#;
    const META_USER: &[u8] =
        br#"{"type":"user","isMeta":true,"message":{"role":"user","content":"ctx"}}"#;
    const SIDECHAIN: &[u8] =
        br#"{"type":"user","isSidechain":true,"message":{"role":"user","content":"x"}}"#;

    #[test]
    fn missing_flags_read_as_false_and_text_is_not_a_tool_result() {
        let facts = facts_for(CLAUDE_PROVIDER, USER_TEXT);
        assert!(facts.structured());
        assert_eq!(facts.is_meta, Some(false));
        assert_eq!(facts.is_sidechain, Some(false));
        assert_eq!(facts.tool_result, Some(false));
    }

    #[test]
    fn malformed_shapes_have_no_structured_facts() {
        for body in [
            b"not json at all".as_slice(),
            br#"{"no_type":1}"#.as_slice(),
            br#"{"type":3}"#.as_slice(),
            br#"{"type":"user"}"#.as_slice(),
            br#"{"type":"user","message":{"content":7}}"#.as_slice(),
            br#"{"type":"assistant","isMeta":"yes"}"#.as_slice(),
        ] {
            let facts = facts_for(CLAUDE_PROVIDER, body);
            assert!(!facts.structured(), "body must not parse: {body:?}");
            assert_eq!(facts.parse_ok, Some(false));
            assert_eq!(facts.provider.as_deref(), Some(CLAUDE_PROVIDER));
        }
        let other = facts_for("codex", USER_TEXT);
        assert!(!other.structured());
        assert_eq!(other.provider.as_deref(), Some("codex"));
    }

    #[test]
    fn rule_rows_reject_and_accept_as_documented() {
        let text = facts_for(CLAUDE_PROVIDER, USER_TEXT);
        assert!(adjudicate(&text, &item("interactive", "owner", "user_statement")).is_ok());
        assert!(adjudicate(&text, &item("subagent", "owner", "user_statement")).is_err());

        let tool = facts_for(CLAUDE_PROVIDER, USER_TOOL);
        assert_eq!(tool.tool_result, Some(true));
        assert!(adjudicate(&tool, &item("interactive", "owner", "user_statement")).is_err());
        assert!(adjudicate(&tool, &item("interactive", "runtime", "execution_result")).is_ok());
        assert!(adjudicate(&tool, &item("interactive", "runtime", "approval")).is_err());

        let assistant = facts_for(CLAUDE_PROVIDER, ASSISTANT);
        assert!(adjudicate(&assistant, &item("interactive", "owner", "user_statement")).is_err());
        assert!(adjudicate(&assistant, &item("interactive", "agent_derived", "unknown")).is_ok());

        let meta = facts_for(CLAUDE_PROVIDER, META_USER);
        assert!(adjudicate(&meta, &item("interactive", "owner", "user_statement")).is_err());
        assert!(adjudicate(&meta, &item("interactive", "runtime", "unknown")).is_ok());
        assert!(adjudicate(&meta, &item("interactive", "runtime", "approval")).is_err());

        let sidechain = facts_for(CLAUDE_PROVIDER, SIDECHAIN);
        assert!(adjudicate(&sidechain, &item("interactive", "agent_derived", "unknown")).is_err());
        assert!(adjudicate(&sidechain, &item("subagent", "agent_derived", "unknown")).is_ok());
    }

    #[test]
    fn meta_row_precedes_the_tool_result_row() {
        let body =
            br#"{"type":"user","isMeta":true,"message":{"content":[{"type":"tool_result"}]}}"#;
        let facts = facts_for(CLAUDE_PROVIDER, body);
        assert_eq!(facts.tool_result, Some(true));
        // The tool-result row would accept a non-owner origin; the meta row
        // (which runs first) does not.
        assert!(adjudicate(&facts, &item("interactive", "external", "execution_result")).is_err());
        assert!(adjudicate(&facts, &item("interactive", "runtime", "execution_result")).is_ok());
    }

    #[test]
    fn other_record_types_are_runtime_or_unknown_only() {
        let facts = facts_for(CLAUDE_PROVIDER, br#"{"type":"system","content":"boot"}"#);
        assert!(facts.structured());
        assert_eq!(facts.tool_result, None);
        assert!(adjudicate(&facts, &item("interactive", "runtime", "unknown")).is_ok());
        assert!(adjudicate(&facts, &item("interactive", "unknown", "external_material")).is_ok());
        assert!(adjudicate(&facts, &item("interactive", "external", "unknown")).is_err());
        assert!(adjudicate(&facts, &item("interactive", "runtime", "approval")).is_err());
    }

    #[test]
    fn an_unstructured_record_only_accepts_all_unknown() {
        let facts = facts_for(CLAUDE_PROVIDER, b"broken");
        assert!(adjudicate(&facts, &item("unknown", "unknown", "unknown")).is_ok());
        assert!(adjudicate(&facts, &item("interactive", "unknown", "unknown")).is_err());
        assert!(adjudicate(&facts, &item("unknown", "owner", "unknown")).is_err());
        assert!(adjudicate(&facts, &item("unknown", "unknown", "user_statement")).is_err());
    }
}
