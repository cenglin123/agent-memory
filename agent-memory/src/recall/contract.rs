//! Frozen contract constants: source statuses, error codes, exit codes,
//! warning code sets and the frozen `detail` key set.
//!
//! Every table in the plan is exported from here so tests can assert the
//! document binding instead of restating literals.

use serde::Serialize;

/// Version of this crate's own response contract (not the KB child contract).
pub const CONTRACT_VERSION: u32 = 2;

/// Stage 7 §C: the frozen `ask.py` time-decay triple. It is appended **only**
/// by `eval-recall` (through the KB provider's internal option) so a
/// cross-run comparison does not move with the wall clock; it is never part of
/// the request contract and `memory_search` never sends it.
pub const KB_EVAL_FROZEN_DECAY: &str = "1.0,1.0,1.0";

pub mod codes {
    pub const INVALID_REQUEST: &str = "INVALID_REQUEST";
    pub const INVALID_OFFSET: &str = "INVALID_OFFSET";
    pub const SOURCE_NOT_CONFIGURED: &str = "SOURCE_NOT_CONFIGURED";
    pub const SOURCE_ROOT_INVALID: &str = "SOURCE_ROOT_INVALID";
    pub const PATH_ESCAPE: &str = "PATH_ESCAPE";
    pub const DB_UNAVAILABLE: &str = "DB_UNAVAILABLE";
    pub const DB_LOCKED: &str = "DB_LOCKED";
    pub const DB_CORRUPT: &str = "DB_CORRUPT";
    pub const SCHEMA_UNSUPPORTED: &str = "SCHEMA_UNSUPPORTED";
    pub const KB_CONTRACT_UNSUPPORTED: &str = "KB_CONTRACT_UNSUPPORTED";
    pub const RECORD_SKIPPED: &str = "RECORD_SKIPPED";
    pub const CONTENT_UNRESOLVED: &str = "CONTENT_UNRESOLVED";
    pub const KB_TIMEOUT: &str = "KB_TIMEOUT";
    pub const KB_QUERY_FAILED: &str = "KB_QUERY_FAILED";
    pub const KB_ROOT_MISMATCH: &str = "KB_ROOT_MISMATCH";
    pub const BUDGET_BYTES: &str = "BUDGET_BYTES";
    pub const BUDGET_TIME: &str = "BUDGET_TIME";
    pub const REF_SOURCE_MISMATCH: &str = "REF_SOURCE_MISMATCH";
    pub const REF_ROOT_MISMATCH: &str = "REF_ROOT_MISMATCH";
    pub const REF_STALE: &str = "REF_STALE";
    pub const REF_LOCATOR_MISSING: &str = "REF_LOCATOR_MISSING";
    pub const REF_OUT_OF_RANGE: &str = "REF_OUT_OF_RANGE";
    pub const REF_READ_FAILED: &str = "REF_READ_FAILED";
    pub const SIDE_EFFECT_VIOLATION: &str = "SIDE_EFFECT_VIOLATION";
    pub const DOMAIN_DENIED: &str = "DOMAIN_DENIED";
    pub const DOMAIN_UNREACHABLE: &str = "DOMAIN_UNREACHABLE";
    pub const ARCHIVE_PROVIDER_UNAVAILABLE: &str = "ARCHIVE_PROVIDER_UNAVAILABLE";
}

/// All error codes, in documentation order. Tests assert uniqueness + length.
pub const ALL_ERROR_CODES: [&str; 27] = [
    codes::INVALID_REQUEST,
    codes::INVALID_OFFSET,
    codes::SOURCE_NOT_CONFIGURED,
    codes::SOURCE_ROOT_INVALID,
    codes::PATH_ESCAPE,
    codes::DB_UNAVAILABLE,
    codes::DB_LOCKED,
    codes::DB_CORRUPT,
    codes::SCHEMA_UNSUPPORTED,
    codes::KB_CONTRACT_UNSUPPORTED,
    codes::RECORD_SKIPPED,
    codes::CONTENT_UNRESOLVED,
    codes::KB_TIMEOUT,
    codes::KB_QUERY_FAILED,
    codes::KB_ROOT_MISMATCH,
    codes::BUDGET_BYTES,
    codes::BUDGET_TIME,
    codes::REF_SOURCE_MISMATCH,
    codes::REF_ROOT_MISMATCH,
    codes::REF_STALE,
    codes::REF_LOCATOR_MISSING,
    codes::REF_OUT_OF_RANGE,
    codes::REF_READ_FAILED,
    codes::SIDE_EFFECT_VIOLATION,
    codes::DOMAIN_DENIED,
    codes::DOMAIN_UNREACHABLE,
    codes::ARCHIVE_PROVIDER_UNAVAILABLE,
];

pub mod status {
    pub const UNCONFIGURED: &str = "unconfigured";
    pub const UNAVAILABLE: &str = "unavailable";
    pub const UNSUPPORTED: &str = "unsupported";
    pub const ERROR: &str = "error";
    pub const PARTIAL: &str = "partial";
    pub const TRUNCATED: &str = "truncated";
    /// Exact mode only: the source answered, but through a fallback because its
    /// full-text index was missing, stale or unreadable. A degraded source is
    /// never healthy (exit `4`/`10`) and never an `errors[]` entry: `errors[]`
    /// stays reserved for the six incomplete states.
    pub const DEGRADED: &str = "degraded";
    pub const EMPTY: &str = "empty";
    pub const OK: &str = "ok";
}

/// Status precedence, highest first.
pub const STATUS_PRIORITY: [&str; 9] = [
    status::UNCONFIGURED,
    status::UNAVAILABLE,
    status::UNSUPPORTED,
    status::ERROR,
    status::PARTIAL,
    status::TRUNCATED,
    status::DEGRADED,
    status::EMPTY,
    status::OK,
];

/// Statuses that make a source "incomplete" (the six non-healthy states).
pub const INCOMPLETE_STATUSES: [&str; 6] = [
    status::UNCONFIGURED,
    status::UNAVAILABLE,
    status::UNSUPPORTED,
    status::ERROR,
    status::PARTIAL,
    status::TRUNCATED,
];

/// Source statuses that force a non-zero search exit code when results exist.
pub const DEGRADED_STATUSES: [&str; 6] = [
    status::UNAVAILABLE,
    status::UNSUPPORTED,
    status::ERROR,
    status::PARTIAL,
    status::TRUNCATED,
    status::DEGRADED,
];

pub const WARN_RERANK_UNAVAILABLE: &str = "WARN_RERANK_UNAVAILABLE";
pub const WARN_RERANK_TIMEOUT: &str = "WARN_RERANK_TIMEOUT";
pub const WARN_GRAPH_FALLBACK: &str = "WARN_GRAPH_FALLBACK";
pub const WARN_FILE_STALE: &str = "WARN_FILE_STALE";
/// `memory_recent`: the response byte budget dropped the oldest tail turns.
pub const WARN_RECENT_TAIL_BUDGET: &str = "WARN_RECENT_TAIL_BUDGET";
/// `memory_recent`: a bounded backwards window ended before the requested
/// number of turns was reached.
pub const WARN_RECENT_TAIL_WINDOW: &str = "WARN_RECENT_TAIL_WINDOW";

/// The configuration names a source whose `kind` this binary does not know
/// (written by a newer installer). The source is skipped instead of failing
/// every request, so a long-running older process keeps serving the rest.
pub const WARN_SOURCE_KIND_UNSUPPORTED: &str = "WARN_SOURCE_KIND_UNSUPPORTED";

/// A v2 configuration carries top-level keys this binary does not know
/// (written by a newer installer). They are ignored instead of rejecting the
/// whole file; `detail.keys` names every ignored key.
pub const WARN_CONFIG_KEY_UNSUPPORTED: &str = "WARN_CONFIG_KEY_UNSUPPORTED";

/// `memory_recent` warning code set (closed).
pub const RECENT_WARNING_CODES: [&str; 4] = [
    WARN_RECENT_TAIL_BUDGET,
    WARN_RECENT_TAIL_WINDOW,
    WARN_SOURCE_KIND_UNSUPPORTED,
    WARN_CONFIG_KEY_UNSUPPORTED,
];

/// `memory_search` warning code set (closed).
pub const SEARCH_WARNING_CODES: [&str; 5] = [
    WARN_RERANK_UNAVAILABLE,
    WARN_RERANK_TIMEOUT,
    WARN_GRAPH_FALLBACK,
    WARN_SOURCE_KIND_UNSUPPORTED,
    WARN_CONFIG_KEY_UNSUPPORTED,
];

/// `memory_read` warning code set (closed).
pub const READ_WARNING_CODES: [&str; 1] = [WARN_FILE_STALE];

/// Frozen keys of `source_status[].detail`.
///
/// `archive` is the P1 Archive provider block: record/denial/session-mapping
/// counters and the derived-generation state. It never carries object bytes,
/// capture ids or fields that would enter `hit_id`.
pub const DETAIL_KEYS: [&str; 14] = [
    "kb_contract_version",
    "coverage",
    "budget",
    "code",
    "CONTENT_UNRESOLVED",
    "ignored_types",
    "escalated",
    "reason",
    "session_v2",
    "archive",
    "semantic",
    // Exact-mode block: index state, fallback reason and per-source counters.
    "exact",
    // Hybrid block: branch counters, fusion identity, thresholds version and
    // the semantic degradation reason.
    "hybrid",
    // Which retrieval path the request actually resolved to for this source
    // (`hybrid` / `exact` / `legacy`).
    "resolved_mode",
];

/// P4 request keys. `memory_search.related_hops` asks for bounded `next`-edge
/// expansion; `memory_read.context` selects the non-default context mode.
pub const SEARCH_REQUEST_KEY_RELATED_HOPS: &str = "related_hops";
pub const READ_REQUEST_KEY_CONTEXT: &str = "context";
/// The only non-default `context` mode: the complete turn around the ref.
pub const READ_CONTEXT_TURN: &str = "turn";

// ---------------------------------------------------------------------------
// Full-text L3: `memory_search.mode`
// ---------------------------------------------------------------------------

/// `mode` is absent or `"auto"`: hybrid when the source has a readable index,
/// otherwise the pre-L3 scan.
pub const SEARCH_MODE_AUTO: &str = "auto";
/// `mode = "exact"`: the live-source full-text index answers the query.
pub const SEARCH_MODE_EXACT: &str = "exact";
/// `mode = "hybrid"`: FTS5 bm25 and conversation vectors, fused with RRF.
pub const SEARCH_MODE_HYBRID: &str = "hybrid";
/// `mode = "legacy"`: force the pre-L3 lexical scan even when an index exists.
pub const SEARCH_MODE_LEGACY: &str = "legacy";
pub const ALL_SEARCH_MODES: [&str; 4] = [
    SEARCH_MODE_AUTO,
    SEARCH_MODE_EXACT,
    SEARCH_MODE_HYBRID,
    SEARCH_MODE_LEGACY,
];

/// Hybrid hit provenance, in report order.
pub const HYBRID_CHANNEL_LEXICAL: &str = "lexical";
pub const HYBRID_CHANNEL_SEMANTIC: &str = "semantic";
pub const ALL_HYBRID_CHANNELS: [&str; 2] = [HYBRID_CHANNEL_LEXICAL, HYBRID_CHANNEL_SEMANTIC];

pub const SEARCH_REQUEST_KEY_MODE: &str = "mode";
pub const SEARCH_REQUEST_KEY_SINCE: &str = "since";
pub const SEARCH_REQUEST_KEY_UNTIL: &str = "until";
pub const SEARCH_REQUEST_KEY_ROLES: &str = "roles";
pub const SEARCH_REQUEST_KEY_KINDS: &str = "kinds";
pub const SEARCH_REQUEST_KEY_SESSION_KINDS: &str = "session_kinds";
pub const SEARCH_REQUEST_KEY_ORDER: &str = "order";
pub const SEARCH_REQUEST_KEY_GROUP_BY_SESSION: &str = "group_by_session";

/// Exact-mode ordering. `newest` is the default.
pub const ORDER_NEWEST: &str = "newest";
pub const ORDER_OLDEST: &str = "oldest";
/// `bm25` over the trigram index.
pub const ORDER_RELEVANCE: &str = "relevance";
pub const ALL_ORDERS: [&str; 3] = [ORDER_NEWEST, ORDER_OLDEST, ORDER_RELEVANCE];

/// Index-unit channels and roles stored per row.
pub const ROLE_USER: &str = "user";
/// Process documents (`docs`) only: a documentation chunk is not a turn.
pub const ROLE_DOC: &str = "doc";
pub const ROLE_ASSISTANT: &str = "assistant";
pub const ROLE_TOOL: &str = "tool";
pub const ALL_ROLES: [&str; 4] = [ROLE_USER, ROLE_ASSISTANT, ROLE_TOOL, ROLE_DOC];

pub const UNIT_KIND_TEXT: &str = "text";
pub const UNIT_KIND_TOOL_INPUT: &str = "tool_input";
pub const UNIT_KIND_TOOL_OUTPUT: &str = "tool_output";
pub const ALL_UNIT_KINDS: [&str; 3] =
    [UNIT_KIND_TEXT, UNIT_KIND_TOOL_INPUT, UNIT_KIND_TOOL_OUTPUT];

pub const SESSION_KIND_INTERACTIVE: &str = "interactive";
pub const SESSION_KIND_WORKER: &str = "worker";
pub const SESSION_KIND_UNKNOWN: &str = "unknown";
pub const ALL_SESSION_KINDS: [&str; 3] = [
    SESSION_KIND_INTERACTIVE,
    SESSION_KIND_WORKER,
    SESSION_KIND_UNKNOWN,
];

/// Exact-mode fallback reasons reported in `source_status[].detail`.
pub const INDEX_REASON_MISSING: &str = "INDEX_MISSING";
/// The default mode needs a readable index for what the request asked
/// (filters or ordering) and this source has none: degraded, not failed.
pub const INDEX_REQUIRED: &str = "INDEX_REQUIRED";
pub const INDEX_REASON_STALE: &str = "INDEX_STALE";
pub const INDEX_REASON_CORRUPT: &str = "INDEX_CORRUPT";
/// Every term is shorter than the trigram minimum: the scan answered instead.
pub const SHORT_TERMS_FALLBACK: &str = "short_terms_fallback";

/// S5.1: the KB answered an explicit `mode:"exact"` request through its own
/// lexical entry point (`ask.py --bm25` + the Rust-side whole-term filter).
/// This is a resolved path reported in `source_status[].detail.resolved_mode`
/// and `detail.exact.mode`, never a request `mode` value.
pub const RESOLVED_MODE_KB_LEXICAL: &str = "kb_lexical";

pub const SCORE_KIND_BM25: &str = "bm25";
pub const RANK_BASIS_FULLTEXT_INDEX: &str = "fulltext_index";

/// Frozen `ref` key set.
pub const REF_KEYS: [&str; 8] = [    "source_id",
    "source_kind",
    "root_sha256",
    "rel_path",
    "schema_gen",
    "locator",
    "fingerprint",
    "text_range",
];

/// Frozen `memory_read` response key set.
pub const READ_RESPONSE_KEYS: [&str; 9] = [
    "ok",
    "code",
    "segments",
    "text",
    "text_range",
    "range_sha256",
    "truncated",
    "warnings",
    "side_effects",
];

pub const SCHEMA_MD_V1: &str = "md-v1";
pub const SCHEMA_JSON_LEGACY_V1: &str = "json-legacy-v1";
pub const SCHEMA_SQLITE_V1: &str = "sqlite-v1";
pub const SCHEMA_SQLITE_V2: &str = "sqlite-v2";
pub const SCHEMA_CODEX_ROLLOUT_V1: &str = "codex-rollout-v1";
/// Claude Code transcript generation: one JSONL line is one record, addressed
/// by `record_index` + JSON pointer (same shape as `codex-rollout-v1`).
pub const SCHEMA_CLAUDE_JSONL_V1: &str = "claude-jsonl-v1";
/// P1 Archive record generation: one committed capture record, read from the
/// decoded object at `record_sha256` (+ projection offset).
pub const SCHEMA_ARCHIVE_RECORD_V1: &str = "archive-record-v1";
/// DSH session log generation: one decompressed logical JSONL line is one event
/// envelope, addressed by `record_index` + JSON pointer (+ `event_seq`). The
/// physical generation (legacy v0/v1, `v3`, `v4`) is named by the carrier file
/// inside `rel_path`, so one schema generation covers all of them.
pub const SCHEMA_DSH_JSONL: &str = "dsh-jsonl";
/// Hermes live source: one SQLite `state.db`, codex-shaped locators.
pub const SCHEMA_HERMES_SQLITE_V1: &str = "hermes-sqlite-v1";
/// Process-document generation: a markdown file read through a heading chunk.
/// `locator.pointer` is `/chunk/<start byte>`; `text_range` is chunk-relative.
pub const SCHEMA_DOCS_MD_V1: &str = "docs-md-v1";

pub const ALL_SCHEMA_GENS: [&str; 10] = [
    SCHEMA_MD_V1,
    SCHEMA_JSON_LEGACY_V1,
    SCHEMA_SQLITE_V1,
    SCHEMA_SQLITE_V2,
    SCHEMA_CODEX_ROLLOUT_V1,
    SCHEMA_CLAUDE_JSONL_V1,
    SCHEMA_ARCHIVE_RECORD_V1,
    SCHEMA_DSH_JSONL,
    SCHEMA_HERMES_SQLITE_V1,
    SCHEMA_DOCS_MD_V1,
];

pub const SOURCE_KIND_KB: &str = "kb";
pub const SOURCE_KIND_DEVICE: &str = "device";
pub const SOURCE_KIND_OPENCODE: &str = "opencode";
pub const SOURCE_KIND_CODEX: &str = "codex";
pub const SOURCE_KIND_DOCS: &str = "docs";
pub const SOURCE_KIND_CLAUDE_CODE: &str = "claude_code";
pub const SOURCE_KIND_DSH: &str = "dsh";
/// Hermes live source kind (the recall source, not the 006 brief plugin).
pub const SOURCE_KIND_HERMES: &str = "hermes";
pub const SOURCE_KIND_ARCHIVE: &str = "archive";

/// Stage-2 S5 index-unit origin: the unit was built from the live source.
pub const ORIGIN_LIVE: &str = "live";
/// Stage-2 S5 index-unit origin: the unit was built from committed evidence
/// because the session no longer exists in the live source (archive fallback).
pub const ORIGIN_ARCHIVE: &str = "archive";

// ---------------------------------------------------------------------------
// KB child contract (`ask.py --json`, §3.3 / §3.4)
// ---------------------------------------------------------------------------

/// `contract_version` the KB child envelope must carry.
pub const KB_CONTRACT_VERSION: u32 = 1;

pub const KB_MODE_DENSE: &str = "dense";
pub const KB_MODE_HYBRID: &str = "hybrid";
pub const KB_MODE_RERANK: &str = "rerank";
/// S5.1: the KB child's pure BM25 lexical mode (`ask.py --json --bm25`), used
/// for an explicit `mode:"exact"` request on a KB source.
pub const KB_MODE_BM25: &str = "bm25";

/// `detail.mode` enum. Missing or outside this set is `KB_QUERY_FAILED`.
pub const ALL_KB_MODES: [&str; 4] = [KB_MODE_DENSE, KB_MODE_HYBRID, KB_MODE_RERANK, KB_MODE_BM25];

/// `score_kind` produced only by the KB envelope's `detail.mode` (§3.1).
pub fn score_kind_for_mode(mode: &str) -> Option<&'static str> {
    match mode {
        KB_MODE_DENSE => Some(SCORE_KIND_NATIVE_DENSE),
        KB_MODE_HYBRID => Some(SCORE_KIND_NATIVE_HYBRID),
        KB_MODE_RERANK => Some(SCORE_KIND_NATIVE_RERANK),
        KB_MODE_BM25 => Some(SCORE_KIND_NATIVE_BM25),
        _ => None,
    }
}

/// `rank_basis` has the same single producer as `score_kind` for KB hits.
pub fn rank_basis_for_mode(mode: &str) -> Option<&'static str> {
    score_kind_for_mode(mode)
}

/// ask.py exit codes with a dedicated mapping row (§3.3).
pub const ASK_EXIT_LOCAL_UNAVAILABLE: i32 = 12;
pub const ASK_EXIT_PROFILE_MISMATCH: i32 = 13;
pub const ASK_EXIT_REMOTE_AUTH_REQUIRED: i32 = 16;

/// KB subprocess hard timeout: `max(1000, min(18000, max_elapsed - 2000))`.
pub fn kb_hard_timeout_ms(max_elapsed_ms: u64) -> u64 {
    max_elapsed_ms.saturating_sub(2_000).clamp(1_000, 18_000)
}

/// Device readable set: direct children matching this pattern (§3.4 Device).
pub const DEVICE_FILE_PATTERN: &str = r"^\d{4}-\d{2}-\d{2}_.+\.md$";

/// Body first-block cap for filename-only device hits.
pub const DEVICE_FILENAME_BLOCK_MAX_BYTES: usize = 4_096;

/// stdio MCP tool names (read-only trio, nothing else).
pub const MCP_TOOL_NAMES: [&str; 4] = [
    "memory_search",
    "memory_read",
    "memory_recent",
    "memory_qualified",
];

/// Codes that make MCP `isError:true` (§3.3). `partial`/`truncated`/`warnings`
/// never do.
pub const IS_ERROR_CODES: [&str; 9] = [
    codes::INVALID_REQUEST,
    codes::INVALID_OFFSET,
    codes::KB_CONTRACT_UNSUPPORTED,
    codes::REF_SOURCE_MISMATCH,
    codes::REF_ROOT_MISMATCH,
    codes::REF_STALE,
    codes::REF_LOCATOR_MISSING,
    codes::REF_OUT_OF_RANGE,
    codes::REF_READ_FAILED,
];

pub fn is_error_code(code: &str) -> bool {
    IS_ERROR_CODES.contains(&code)
}

pub const SCORE_KIND_NATIVE_DENSE: &str = "native_dense";
pub const SCORE_KIND_NATIVE_HYBRID: &str = "native_hybrid";
pub const SCORE_KIND_NATIVE_RERANK: &str = "native_rerank";
/// S5.1: KB hits ranked by the child's own BM25 order (`detail.mode = "bm25"`).
/// Deliberately not `native_dense`/`native_hybrid`: the ranking producer must
/// stay identifiable.
pub const SCORE_KIND_NATIVE_BM25: &str = "native_bm25";
pub const SCORE_KIND_NONE: &str = "none";
/// P3 fused rank score (reciprocal-rank fusion). Comparable inside one source's
/// candidate list, not across sources.
pub const SCORE_KIND_RRF: &str = "rrf";
pub const RANK_BASIS_LEXICAL_WINDOW: &str = "lexical_window";
/// P3: the hit was ranked by fusing lexical and semantic candidate ranks.
pub const RANK_BASIS_RRF: &str = "rrf_lexical_semantic";
/// P3 rework v4: the hit was ranked by semantic cosine with a bounded lexical
/// coverage bonus (semantic available only).
pub const SCORE_KIND_SEMANTIC_BONUS: &str = "semantic_plus_lexical_bonus";
pub const RANK_BASIS_SEMANTIC_BONUS: &str = "semantic_cosine_lexical_bonus";

/// Side effect levels (§2.3). Only C-level runtime artifacts are ever reported.
pub const SIDE_EFFECT_LEVEL_C: &str = "C";
pub const SIDE_EFFECT_LEVEL_A: &str = "A";
pub const SIDE_EFFECT_LEVEL_B: &str = "B";

/// Known telemetry paths whose change is a contract violation (§2.3).
pub const KNOWN_AB_PATHS: [&str; 5] = [
    ".meta/usage-log.jsonl",
    ".meta/co-retrieval.log.jsonl",
    ".meta/co-retrieval.log.lock",
    ".meta/co-retrieval.log.meta.json",
    ".meta/escalation-stats.jsonl",
];

/// A runtime side effect entry. `note` is only ever `window_only` for C-level
/// entries: it says "something changed in this window", never "this tool wrote
/// it".
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SideEffect {
    pub level: &'static str,
    pub kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<&'static str>,
}

impl SideEffect {
    pub fn window_change(target: String) -> Self {
        SideEffect {
            level: SIDE_EFFECT_LEVEL_C,
            kind: "runtime_artifact",
            target: Some(target),
            note: Some("window_only"),
        }
    }
}

pub fn status_priority(status: &str) -> usize {
    STATUS_PRIORITY
        .iter()
        .position(|s| *s == status)
        .unwrap_or(STATUS_PRIORITY.len())
}

pub fn is_incomplete(status: &str) -> bool {
    INCOMPLETE_STATUSES.contains(&status)
}

pub fn is_degraded(status: &str) -> bool {
    DEGRADED_STATUSES.contains(&status)
}

/// Source status element schema: exactly `{source_id, status, code?, detail?}`.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SourceStatus {
    pub source_id: String,
    pub status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<serde_json::Value>,
}

impl SourceStatus {
    pub fn new(source_id: impl Into<String>, status: &'static str) -> Self {
        SourceStatus {
            source_id: source_id.into(),
            status,
            code: None,
            detail: None,
        }
    }

    pub fn with_code(mut self, code: &'static str) -> Self {
        self.code = Some(code);
        self
    }

    pub fn with_detail(mut self, detail: serde_json::Value) -> Self {
        self.detail = Some(detail);
        self
    }
}

/// `errors[]` element schema: exactly `{code, message, source_id?}`.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ErrorEntry {
    pub code: &'static str,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_id: Option<String>,
}

/// Warning entry schema: `{scope, source_id?, hit_id?, code, detail?}`.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct WarningEntry {
    pub scope: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hit_id: Option<String>,
    pub code: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<serde_json::Value>,
}

impl WarningEntry {
    pub fn file_stale(source_id: Option<String>, detail: Option<serde_json::Value>) -> Self {
        WarningEntry {
            scope: "ref",
            source_id,
            hit_id: None,
            code: WARN_FILE_STALE,
            detail,
        }
    }
}

/// Error value shared by configuration, request and readback validation.
#[derive(Debug, Clone, PartialEq)]
pub struct RecallError {
    pub code: &'static str,
    pub message: String,
    pub detail: Option<serde_json::Value>,
}

impl RecallError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        RecallError {
            code,
            message: message.into(),
            detail: None,
        }
    }

    pub fn with_detail(mut self, detail: serde_json::Value) -> Self {
        self.detail = Some(detail);
        self
    }

    /// `INVALID_REQUEST` with a `{path, reason}` detail.
    pub fn invalid(path: &str, reason: &str, message: impl Into<String>) -> Self {
        RecallError::new(codes::INVALID_REQUEST, message).with_detail(serde_json::json!({
            "path": path,
            "reason": reason,
        }))
    }

    pub fn exit_code(&self) -> i32 {
        match self.code {
            codes::REF_SOURCE_MISMATCH
            | codes::REF_ROOT_MISMATCH
            | codes::REF_STALE
            | codes::REF_LOCATOR_MISSING
            | codes::REF_OUT_OF_RANGE
            | codes::REF_READ_FAILED
            | codes::DOMAIN_DENIED
            | codes::DOMAIN_UNREACHABLE => 5,
            _ => 2,
        }
    }
}

impl std::fmt::Display for RecallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for RecallError {}

/// Derive the `memory_search` exit code from the frozen table (§3.3).
///
/// Row order: `3` (no results, all requested sources `unconfigured`),
/// `4` (no results, any degraded source), `10` (results + any degraded source),
/// `0` otherwise. `2`/`5` are request/readback failures and never reach here.
pub fn search_exit_code(has_results: bool, statuses: &[&str]) -> i32 {
    if !has_results {
        if !statuses.is_empty() && statuses.iter().all(|s| *s == status::UNCONFIGURED) {
            return 3;
        }
        if statuses.iter().any(|s| is_degraded(s)) {
            return 4;
        }
        return 0;
    }
    if statuses.iter().any(|s| is_degraded(s)) {
        return 10;
    }
    0
}

/// Map a port/provider failure to the frozen source status triple.
pub fn status_for_code(code: &str) -> &'static str {
    match code {
        codes::SOURCE_NOT_CONFIGURED => status::UNCONFIGURED,
        codes::DB_UNAVAILABLE
        | codes::DB_LOCKED
        | codes::DB_CORRUPT
        | codes::SOURCE_ROOT_INVALID
        | codes::PATH_ESCAPE
        | codes::DOMAIN_DENIED
        | codes::DOMAIN_UNREACHABLE => status::UNAVAILABLE,
        codes::SCHEMA_UNSUPPORTED
        | codes::KB_CONTRACT_UNSUPPORTED
        | codes::ARCHIVE_PROVIDER_UNAVAILABLE => status::UNSUPPORTED,
        codes::RECORD_SKIPPED | codes::CONTENT_UNRESOLVED => status::PARTIAL,
        codes::BUDGET_BYTES | codes::BUDGET_TIME | codes::KB_TIMEOUT => status::TRUNCATED,
        _ => status::ERROR,
    }
}
