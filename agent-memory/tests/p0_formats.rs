//! P0 acceptance: format-generation matrix is real, not stubbed.
//!
//! What lives where (this file only covers the matrix contract plus the two
//! Codex rollout generations and the Claude injected/compact semantics):
//!
//! * here — `format_support_matrix()` completeness; Codex plain + zstd
//!   rollout parsing through `load_messages`; Claude injected / mixed /
//!   compact semantics.
//! * `src/providers/opencode.rs` unit tests — OpenCode legacy JSON storage
//!   (`load_messages_includes_tool_parts`), SQLite V1
//!   (`load_messages_sqlite_reads_messages_and_parts`), SQLite V2
//!   (`load_messages_sqlite_v2_reads_messages`, `..._orders_by_seq_...`) and
//!   the mixed V1/V2 scan.
//! * `src/providers/claude.rs` unit tests — sidecar `tool-results/`
//!   ContentRef semantics and the rest of the Claude generations.
//! * `src/model.rs` unit tests — fixture round-trip of the unified model.
//! * `tests/boundary_no_default_roots.rs` — boundary reverse tests (no
//!   default real-session-root channels; see also the `compile_fail`
//!   doctests in `src/lib.rs`).

use agent_memory::model::{EventKind, SessionBlock};
use agent_memory::{format_support_matrix, load_messages, SUPPORTED_PROVIDERS};

fn codex_fixture_lines() -> String {
    concat!(
        "{\"timestamp\":\"2026-03-06T21:50:12Z\",\"type\":\"session_meta\",\"payload\":{\"id\":\"s-matrix\",\"cwd\":\"/tmp/project\"}}\n",
        "{\"timestamp\":\"2026-03-06T21:50:13Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"hello matrix\"}]}}\n",
        "{\"timestamp\":\"2026-03-06T21:50:14Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"hi there\"}]}}\n"
    )
    .to_string()
}

#[test]
fn matrix_marks_current_default_formats_supported() {
    let matrix = format_support_matrix();
    for provider in SUPPORTED_PROVIDERS {
        assert!(
            matrix.iter().any(|e| e.provider == provider && e.supported),
            "provider {provider} has no supported generation"
        );
    }
    // Every generation named in the P0 contract is present.
    for generation in [
        "rollout-jsonl",
        "rollout-jsonl.zst",
        "projects-jsonl",
        "injected",
        "sidecar",
        "compact",
        "legacy-json-storage",
        "sqlite-v1",
        "sqlite-v2",
    ] {
        assert!(
            matrix.iter().any(|e| e.generation == generation),
            "matrix is missing generation {generation}"
        );
    }
    // Nothing in this slice is allowed to masquerade: unsupported entries must
    // say so explicitly (none at P0 — every contracted generation is supported).
    for e in &matrix {
        if !e.supported {
            panic!("generation {} must be reported, not silently parsed", e.generation);
        }
    }
}

#[test]
fn codex_plain_rollout_parses() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("rollout-s-matrix.jsonl");
    std::fs::write(&path, codex_fixture_lines()).unwrap();
    let messages = load_messages("codex", path.to_str().unwrap()).unwrap();
    assert!(messages.iter().any(|m| m.role == "user"));
    assert!(messages.iter().any(|m| m.role == "assistant"));
}

#[test]
fn codex_compressed_rollout_parses() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("rollout-s-matrix.jsonl.zst");
    let compressed = zstd::stream::encode_all(codex_fixture_lines().as_bytes(), 3).unwrap();
    std::fs::write(&path, compressed).unwrap();
    let messages = load_messages("codex", path.to_str().unwrap()).unwrap();
    assert!(messages.iter().any(|m| m.role == "user"));
    assert!(messages.iter().any(|m| m.role == "assistant"));
}

#[test]
fn claude_injected_and_compact_semantics_hold() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("session.jsonl");
    // system-reminder-only user record is injected (original text kept); a
    // mixed record gets the reminder stripped and stays non-injected; /compact
    // shows up as a compaction event in the unified model.
    let line_user_injected = serde_json::json!({
        "type": "user", "uuid": "u1", "timestamp": "2026-01-01T00:00:00Z",
        "message": {"role": "user", "content": "<system-reminder>secret rules</system-reminder>"}
    });
    let line_user_mixed = serde_json::json!({
        "type": "user", "uuid": "u2", "timestamp": "2026-01-01T00:00:30Z",
        "message": {"role": "user", "content": "<system-reminder>noise</system-reminder>\n\nreal question"}
    });
    let line_compact = serde_json::json!({
        "type": "system", "subtype": "compact_boundary", "uuid": "c1",
        "timestamp": "2026-01-01T00:01:00Z",
        "content": "Compacted",
        "compactMetadata": {"trigger": "auto", "preTokens": 1234}
    });
    std::fs::write(
        &path,
        format!("{}\n{}\n{}\n", line_user_injected, line_user_mixed, line_compact),
    )
    .unwrap();
    let messages = load_messages("claude", path.to_str().unwrap()).unwrap();
    let injected_msg = messages
        .iter()
        .find(|m| m.injected)
        .expect("system-reminder-only record must be marked injected");
    assert!(
        injected_msg.content.contains("secret rules"),
        "injected content keeps the original text"
    );
    let mixed = messages
        .iter()
        .find(|m| !m.injected && m.role == "user")
        .expect("mixed record must remain a normal user message");
    assert_eq!(mixed.content, "real question", "reminder stripped from view");
    let has_compaction = messages.iter().any(|m| {
        m.blocks.iter().any(|b| matches!(
            b,
            SessionBlock::Event { kind: EventKind::Compaction, .. }
        ))
    });
    assert!(has_compaction, "compact boundary must surface as a compaction event");
}
