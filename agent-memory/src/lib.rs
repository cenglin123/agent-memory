//! agent-memory: headless session evidence archive (P0–P5).
//!
//! The unified session model and the Claude / Codex / OpenCode providers are
//! adapted from cc-switch v4.0.20 (commit 85ad4969b3399500baaea9d46b1e08f87aba80e1,
//! MIT). See NOTICE for the exact provenance and the list of stripped
//! desktop couplings.
//!
//! # No default real-session-root entry points
//!
//! Scanning APIs take explicitly injected provider roots / config
//! ([`providers::codex::scan_sessions_in_roots_with_titles`],
//! [`providers::claude::scan_sessions_in_root`],
//! [`providers::opencode::scan_sessions_in`]); message loading takes an
//! explicit source path. The old no-argument scan entry that walked the three
//! frameworks' real session roots was deleted and is compile-time unreachable:
//!
//! ```compile_fail
//! // The default-root scan entry no longer exists (boundary fix).
//! let _ = agent_memory::scan_sessions("codex");
//! ```
//!
//! ```compile_fail
//! // Provider-level no-argument scans are compile-time unreachable too.
//! let _ = agent_memory::providers::codex::scan_sessions();
//! ```
//!
//! ```compile_fail
//! let _ = agent_memory::providers::claude::scan_sessions();
//! ```
//!
//! ```compile_fail
//! let _ = agent_memory::providers::opencode::scan_sessions();
//! ```
//!
//! ```compile_fail
//! // The home-fallback config resolvers are gone as well.
//! let _ = agent_memory::config::get_codex_config_dir();
//! ```
//!
//! These `compile_fail` blocks are reverse tests: if a default real-root
//! channel is ever reintroduced, they start compiling and the test suite goes
//! red. See `tests/boundary_no_default_roots.rs` for the runtime fail-closed
//! and decoy counterparts.

pub mod archive;
pub mod attachment;
pub mod evidence;
pub mod audit_scan;
pub mod cache_store;
pub mod capture;
pub mod capture_all;
pub mod capture_dsh;
pub mod capture_jsonl;
pub mod capture_sqlite;
pub mod config;
pub mod cost;
pub mod coverage;
pub mod codex_rollout_file;
pub(crate) mod codex_state_db;
pub mod compact;
pub mod error;
pub mod eval_pause;
pub mod gate;
pub mod grounding;
pub mod learning;
pub mod mcp;
pub mod model;
pub mod providers;
pub mod recall;
pub mod replay;
pub mod report;
pub mod scrub;

#[cfg(test)]
mod test_support;

use serde::Serialize;
use std::path::Path;

pub use model::SessionMessage;

/// Session list metadata (ported verbatim from cc-switch `session_manager::SessionMeta`).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionMeta {
    pub provider_id: String,
    pub session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_dir: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_active_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resume_command: Option<String>,
}

/// Provider identifiers supported by this headless slice.
pub const SUPPORTED_PROVIDERS: [&str; 3] = ["claude", "codex", "opencode"];

/// One entry of the format-generation support matrix (P0 contract).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FormatSupport {
    pub provider: String,
    pub generation: String,
    /// true = parse path really implemented and covered by tests;
    /// false = explicitly not supported (must surface as such, never faked).
    pub supported: bool,
    pub note: String,
}

/// Format-generation support matrix fixed as a P0 contract.
///
/// Coverage keys: Codex uncompressed/compressed rollout; Claude
/// injected/sidecar/compact; OpenCode legacy JSON / SQLite V1 / SQLite V2.
/// The current default write format of each provider is supported.
pub fn format_support_matrix() -> Vec<FormatSupport> {
    let entry = |provider: &str, generation: &str, supported: bool, note: &str| FormatSupport {
        provider: provider.to_string(),
        generation: generation.to_string(),
        supported,
        note: note.to_string(),
    };
    vec![
        entry("codex", "rollout-jsonl", true, "plain rollout-*.jsonl (current default write format)"),
        entry("codex", "rollout-jsonl.zst", true, "zstd single-frame compressed rollout, transparent decode via codex_rollout_file"),
        entry("claude", "projects-jsonl", true, "~/.claude/projects/<project>/*.jsonl (current default)"),
        entry("claude", "injected", true, "AGENTS.md/environment_context/system-reminder marked injected, original text kept"),
        entry("claude", "sidecar", true, "tool-results/ sidecar ContentRef semantics kept in model; referenced objects are archived by src/attachment.rs (P4)"),
        entry("claude", "compact", true, "compaction events parse as EventKind::Compaction"),
        entry("opencode", "legacy-json-storage", true, "storage/message + storage/part flat files"),
        entry("opencode", "sqlite-v1", true, "session/message/part tables"),
        entry("opencode", "sqlite-v2", true, "session_v2/session_message tables (current default)"),
    ]
}

/// Parse a session source into unified messages (dispatch by provider).
pub fn load_messages(provider_id: &str, source_path: &str) -> Result<Vec<SessionMessage>, String> {
    if provider_id == "opencode" && source_path.starts_with("sqlite:") {
        return providers::opencode::load_messages_sqlite(source_path);
    }
    let path = Path::new(source_path);
    match provider_id {
        "codex" => providers::codex::load_messages(path),
        "claude" => providers::claude::load_messages(path),
        "opencode" => providers::opencode::load_messages(path),
        _ => Err(format!("Unsupported provider: {provider_id}")),
    }
}

// NOTE: there is intentionally no `scan_sessions(provider_id)` entry point.
// Scanning requires explicitly injected provider roots / config (see the
// crate-level docs and `tests/boundary_no_default_roots.rs`): P2 must build on
// explicit injection, never on an implicit real-root back door.
