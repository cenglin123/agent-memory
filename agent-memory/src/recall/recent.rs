//! `memory_recent`: the live, index-free session hot layer.
//!
//! This module answers "which interactive sessions were active most recently,
//! and what do their last turns say" straight from the host's own records:
//!
//! * OpenCode: the session database, read-only, bounded by `time_updated` and
//!   by `session_message(session_id, seq)` (never a full message scan);
//! * Claude Code: top-level `<project>/<session>.jsonl` chosen by file mtime,
//!   then its tail read backwards in one bounded window;
//! * Codex: the state database's thread update clock plus the matching
//!   rollout tail.
//!
//! Nothing here reads or writes `cache_root`: there is no index, no vector
//! cache and no file write on this path. Parse/projection rules are not
//! restated either — records go through the same `fulltext::build` projectors
//! the live index uses, so a `tail[].ref` is byte-identical to the ref a
//! `memory_search` hit of the same record would carry and can be handed to
//! `memory_read` unchanged.

use crate::providers::claude::{QueuedDedup, QueuedVerdict};
use crate::recall::config::{RecallConfig, SourceConfig, SourceKind};
use crate::recall::contract::{
    codes, search_exit_code, status, status_for_code, status_priority, ErrorEntry, RecallError,
    SourceStatus, WarningEntry, ALL_ROLES, ALL_SESSION_KINDS, CONTRACT_VERSION, ROLE_ASSISTANT,
    ROLE_TOOL, ROLE_USER, SCHEMA_CLAUDE_JSONL_V1, SCHEMA_CODEX_ROLLOUT_V1, SCHEMA_DSH_JSONL,
    SCHEMA_HERMES_SQLITE_V1, SCHEMA_SQLITE_V2, SOURCE_KIND_CLAUDE_CODE, SOURCE_KIND_CODEX,
    SOURCE_KIND_DSH, SOURCE_KIND_HERMES, SOURCE_KIND_OPENCODE, UNIT_KIND_TEXT,
};
use crate::recall::fulltext::build::{
    claude_record_units, codex_files, codex_marker_from_reader, codex_record_units,
    dsh_record_units, opencode_part_unit, opencode_units, truncate_chars, BuildError, CodexMarker,
};
use crate::recall::fulltext::{
    claude_session_kind_with_entrypoint, codex_session_kind, opencode_session_kind, rfc3339,
};
use crate::recall::hash::{root_sha256, sha256_hex, TextRange};
use crate::recall::port::model::map_sqlite_error;
use crate::recall::port::probe::{open_readonly, table_columns};
use crate::recall::port::reader::{rel_from_root, resolve_sqlite};
use crate::recall::port::sqlite_v2::{session_parts, MAX_ROW_BYTES};
use crate::recall::providers::codex::{codex_db_path, codex_storage_dir};
use crate::recall::providers::dsh;
use crate::recall::providers::hermes;
use crate::recall::providers::{port_code, port_detail};
use crate::recall::refs::{Fingerprint, HitRef, Locator};
use crate::recall::search::{
    checked_integer, invalid_type, parse_enum_array, parse_rfc3339_ms, reject_unknown,
};
use serde::Serialize;
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::Duration;

// ---------------------------------------------------------------------------
// Public contract constants
// ---------------------------------------------------------------------------

/// Request fields, all optional. Unknown keys are rejected.
pub const RECENT_REQUEST_KEYS: [&str; 10] = [
    "sources",
    "session_id",
    "limit",
    "session_kinds",
    "exclude_session_ids",
    "since",
    "tail_turns",
    "tail_roles",
    "max_chars_per_turn",
    "max_bytes",
];

pub const RECENT_DEFAULT_LIMIT: usize = 5;
pub const RECENT_MAX_LIMIT: usize = 20;
pub const RECENT_DEFAULT_TAIL_TURNS: usize = 0;
pub const RECENT_MAX_TAIL_TURNS: usize = 20;
pub const RECENT_DEFAULT_MAX_CHARS: usize = 800;
pub const RECENT_MAX_CHARS: usize = 8_000;
pub const RECENT_DEFAULT_MAX_BYTES: usize = 65_536;
/// The output ceiling is a response budget, so it stays far below a whole
/// transcript; 4 MiB is the largest a 20×20×8 000-character tail can need.
pub const RECENT_MAX_BYTES: usize = 4 * 1024 * 1024;

/// The session-bearing source kinds `memory_recent` enumerates by default.
pub const RECENT_SOURCE_KINDS: [&str; 5] = [
    SOURCE_KIND_OPENCODE,
    SOURCE_KIND_CODEX,
    SOURCE_KIND_CLAUDE_CODE,
    SOURCE_KIND_DSH,
    SOURCE_KIND_HERMES,
];

/// First backwards JSONL window per carrier. Claude Code transcripts carry
/// large bookkeeping records (request snapshots, attachments, images) that can
/// fill a fixed window without a single conversation turn, so the window grows
/// by [`RECENT_TAIL_WINDOW_GROWTH`] until the requested turns and a
/// conversation clock are found, the file start is reached, or
/// [`RECENT_TAIL_WINDOW_MAX_BYTES`] is hit. A still-short tail is disclosed.
pub const RECENT_TAIL_WINDOW_BYTES: u64 = 256 * 1024;
pub const RECENT_TAIL_WINDOW_GROWTH: u64 = 4;
pub const RECENT_TAIL_WINDOW_MAX_BYTES: u64 = 16 * 1024 * 1024;
/// One bounded head window (session metadata, cwd, Claude `entrypoint`).
pub const RECENT_HEAD_WINDOW_BYTES: u64 = 64 * 1024;
/// Candidate factor over `limit` for mtime / page ordered enumeration.
pub const RECENT_CANDIDATE_FACTOR: usize = 4;
pub const RECENT_MIN_CANDIDATES: usize = 8;
/// Page ceiling for the OpenCode session table: every page is a small,
/// `session_v2`-only ordered read, never a message-table scan.
pub const RECENT_MAX_PAGES: usize = 5;
/// `session_message` rows read from the tail of one OpenCode session. Bounded
/// independently of `tail_turns` so a run of tool rows cannot unbound the read.
pub const RECENT_OPENCODE_TAIL_ROWS: usize = 200;

/// The exact tail query production issues. Public so the query-plan test
/// asserts the SQL that actually runs instead of a look-alike.
pub const OPENCODE_TAIL_SQL: &str = "SELECT id, seq, type, time_created, length(data), data \
     FROM session_message \
     WHERE session_id = ?1 AND (data IS NULL OR length(data) <= ?2) \
     ORDER BY seq DESC LIMIT ?3";
pub const OPENCODE_TAIL_PLAN_SQL: &str = "EXPLAIN QUERY PLAN SELECT id, seq, type, \
     time_created, length(data), data FROM session_message \
     WHERE session_id = ?1 AND (data IS NULL OR length(data) <= ?2) \
     ORDER BY seq DESC LIMIT ?3";

// ---------------------------------------------------------------------------
// Request
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct RecentRequest {
    /// Configured source ids. `None` means every enabled session-type source.
    pub sources: Option<Vec<String>>,
    /// Only this session. It overrides `session_kinds` (a named session is
    /// wanted whatever its kind) and is looked up directly in every source
    /// instead of inside the newest-first candidate window.
    pub session_id: Option<String>,
    pub limit: usize,
    pub session_kinds: Vec<String>,
    pub exclude_session_ids: Vec<String>,
    pub since_ms: Option<i64>,
    pub tail_turns: usize,
    pub tail_roles: Vec<String>,
    pub max_chars_per_turn: usize,
    pub max_bytes: usize,
    /// Not part of the JSON contract (`parse` always leaves it `false`). The
    /// opening brief sets it to stay inside a hook deadline; it trades two
    /// guarantees for speed:
    /// * JSONL tails are read from one bounded end-of-file window without
    ///   counting the lines before it, so a large transcript costs the window
    ///   and not the file. The tail `ref`s are then NOT valid locators, so
    ///   only a caller that never surfaces refs may set it;
    /// * with `since`, a file-per-session carrier last modified well before
    ///   the threshold is skipped unread (see `since_mtime_floor`).
    pub fast_scan: bool,
}

impl Default for RecentRequest {
    fn default() -> Self {
        RecentRequest {
            sources: None,
            session_id: None,
            limit: RECENT_DEFAULT_LIMIT,
            session_kinds: vec![crate::recall::contract::SESSION_KIND_INTERACTIVE.to_string()],
            exclude_session_ids: Vec::new(),
            since_ms: None,
            tail_turns: RECENT_DEFAULT_TAIL_TURNS,
            tail_roles: vec![
                crate::recall::contract::ROLE_USER.to_string(),
                crate::recall::contract::ROLE_ASSISTANT.to_string(),
            ],
            max_chars_per_turn: RECENT_DEFAULT_MAX_CHARS,
            max_bytes: RECENT_DEFAULT_MAX_BYTES,
            fast_scan: false,
        }
    }
}

impl RecentRequest {
    pub fn parse(value: &Value) -> Result<RecentRequest, RecallError> {
        let map = value
            .as_object()
            .ok_or_else(|| invalid_type("request", "object"))?;
        reject_unknown(map, &RECENT_REQUEST_KEYS, "")?;

        let sources = match map.get("sources") {
            None => None,
            Some(Value::Array(items)) => {
                if items.is_empty() {
                    return Err(RecallError::invalid(
                        "sources",
                        "empty_sources",
                        "sources must not be an empty array",
                    ));
                }
                let mut ids = Vec::with_capacity(items.len());
                for (index, item) in items.iter().enumerate() {
                    let Value::String(id) = item else {
                        return Err(invalid_type(&format!("sources[{index}]"), "string"));
                    };
                    if id.is_empty() {
                        return Err(RecallError::invalid(
                            &format!("sources[{index}]"),
                            "empty_source_id",
                            "source ids must not be empty",
                        ));
                    }
                    if ids.iter().any(|existing| existing == id) {
                        return Err(RecallError::invalid(
                            &format!("sources[{index}]"),
                            "duplicate_source_id",
                            "sources must not repeat a source_id",
                        ));
                    }
                    ids.push(id.clone());
                }
                Some(ids)
            }
            Some(_) => return Err(invalid_type("sources", "array")),
        };

        let session_id = match map.get("session_id") {
            None => None,
            Some(Value::String(id)) if !id.trim().is_empty() => Some(id.clone()),
            Some(Value::String(_)) => {
                return Err(RecallError::invalid(
                    "session_id",
                    "empty_value",
                    "session_id must not be empty",
                ))
            }
            Some(_) => return Err(invalid_type("session_id", "string")),
        };

        let limit = match map.get("limit") {
            None => RECENT_DEFAULT_LIMIT,
            Some(value) => checked_integer(value, "limit", 1, RECENT_MAX_LIMIT as i64)?,
        };
        let session_kinds = parse_enum_array(
            map.get("session_kinds"),
            "session_kinds",
            &ALL_SESSION_KINDS,
        )?
        .unwrap_or_else(|| vec![crate::recall::contract::SESSION_KIND_INTERACTIVE.to_string()]);

        let exclude_session_ids = match map.get("exclude_session_ids") {
            None => Vec::new(),
            Some(Value::Array(items)) => {
                if items.is_empty() {
                    return Err(RecallError::invalid(
                        "exclude_session_ids",
                        "empty_array",
                        "exclude_session_ids must not be empty",
                    ));
                }
                let mut ids = Vec::with_capacity(items.len());
                for (index, item) in items.iter().enumerate() {
                    let Value::String(id) = item else {
                        return Err(invalid_type(
                            &format!("exclude_session_ids[{index}]"),
                            "string",
                        ));
                    };
                    if id.trim().is_empty() {
                        return Err(RecallError::invalid(
                            &format!("exclude_session_ids[{index}]"),
                            "empty_value",
                            "exclude_session_ids values must not be empty",
                        ));
                    }
                    if ids.iter().any(|existing| existing == id) {
                        return Err(RecallError::invalid(
                            &format!("exclude_session_ids[{index}]"),
                            "duplicate_value",
                            "exclude_session_ids must not repeat a value",
                        ));
                    }
                    ids.push(id.clone());
                }
                ids
            }
            Some(_) => return Err(invalid_type("exclude_session_ids", "array")),
        };

        let since_ms = parse_rfc3339_ms(map.get("since"), "since")?;
        let tail_turns = match map.get("tail_turns") {
            None => RECENT_DEFAULT_TAIL_TURNS,
            Some(value) => checked_integer(value, "tail_turns", 0, RECENT_MAX_TAIL_TURNS as i64)?,
        };
        let tail_roles = parse_enum_array(map.get("tail_roles"), "tail_roles", &ALL_ROLES)?
            .unwrap_or_else(|| {
                vec![
                    crate::recall::contract::ROLE_USER.to_string(),
                    crate::recall::contract::ROLE_ASSISTANT.to_string(),
                ]
            });
        let max_chars_per_turn = match map.get("max_chars_per_turn") {
            None => RECENT_DEFAULT_MAX_CHARS,
            Some(value) => {
                checked_integer(value, "max_chars_per_turn", 1, RECENT_MAX_CHARS as i64)?
            }
        };
        let max_bytes = match map.get("max_bytes") {
            None => RECENT_DEFAULT_MAX_BYTES,
            Some(value) => checked_integer(value, "max_bytes", 1, RECENT_MAX_BYTES as i64)?,
        };

        Ok(RecentRequest {
            sources,
            session_id,
            limit,
            session_kinds,
            exclude_session_ids,
            since_ms,
            tail_turns,
            tail_roles,
            max_chars_per_turn,
            max_bytes,
            fast_scan: false,
        })
    }

    fn wants_kind(&self, kind: &str) -> bool {
        if self.session_id.is_some() {
            return true;
        }
        self.session_kinds.iter().any(|value| value == kind)
    }

    fn wants_role(&self, role: &str) -> bool {
        self.tail_roles.iter().any(|value| value == role)
    }

    fn excluded(&self, session_id: &str) -> bool {
        self.exclude_session_ids
            .iter()
            .any(|value| value == session_id)
    }
}

// ---------------------------------------------------------------------------
// Response
// ---------------------------------------------------------------------------

/// One tail entry. `ref` is the frozen `memory_read` reference shape.
#[derive(Debug, Clone, Serialize)]
pub struct RecentTailTurn {
    pub role: &'static str,
    pub timestamp: Option<String>,
    pub text: String,
    pub truncated: bool,
    /// Addresses the first row of the turn when the preview spans multiple rows.
    #[serde(rename = "ref")]
    pub reference: HitRef,
    /// Further rows represented in the preview, ordered after `ref`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub additional_refs: Vec<HitRef>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RecentSession {
    /// Configured source id (`source_status[].source_id`), so a caller can
    /// point `memory_search.sources` at the same value.
    pub source: String,
    pub session_id: String,
    pub session_kind: &'static str,
    pub title: Option<String>,
    pub cwd: Option<String>,
    /// The same instant in UTC (`Z`), from the source's own session clock.
    pub last_activity_at: Option<String>,
    /// The same instant in the host's local zone, seconds precision with the
    /// numeric offset, e.g. `2026-11-06T23:57:38+08:00`. Read this when
    /// telling a person when the session was last active.
    pub last_activity_local: Option<String>,
    /// The session clock at full precision. Not serialized: the rendered
    /// stamps are cut to seconds, so a caller that merges sessions from
    /// several responses orders them by this instead.
    #[serde(skip)]
    pub last_activity_ms: Option<i64>,
    pub tail: Vec<RecentTailTurn>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RecentResponse {
    pub contract_version: u32,
    pub sessions: Vec<RecentSession>,
    pub source_status: Vec<SourceStatus>,
    pub errors: Vec<ErrorEntry>,
    pub warnings: Vec<WarningEntry>,
    #[serde(skip)]
    pub exit_code: i32,
}

impl RecentResponse {
    pub fn request_failure(error: &RecallError) -> Self {
        RecentResponse {
            contract_version: CONTRACT_VERSION,
            sessions: Vec::new(),
            source_status: Vec::new(),
            errors: vec![ErrorEntry {
                code: error.code,
                message: error.message.clone(),
                source_id: None,
            }],
            warnings: Vec::new(),
            exit_code: error.exit_code(),
        }
    }

    /// The serialized envelope, ready for the CLI / MCP transports.
    pub fn to_value(&self) -> Value {
        serde_json::to_value(self).expect("recent response serializes")
    }
}

/// Execution options shared with the CLI / MCP wrapper.
#[derive(Debug, Clone, Copy, Default)]
pub struct RecentOptions {
    /// `--strict-snapshot` copies a source root; `memory_recent` reads live
    /// carriers in place and refuses instead of silently ignoring the flag.
    pub strict_snapshot: bool,
    /// Fixed UTC offset in minutes for `last_activity_local`. `None` means the
    /// host's local zone (`chrono::Local`), which is the production path; the
    /// hook's `--utc-offset` only ever sets this for byte-stable tests.
    pub utc_offset_minutes: Option<i32>,
}

// ---------------------------------------------------------------------------
// Internal model
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct RawTurn {
    role: &'static str,
    timestamp_ms: i64,
    text: String,
    truncated: bool,
    reference: HitRef,
    additional_refs: Vec<HitRef>,
}

#[derive(Debug, Clone)]
struct Candidate {
    source_id: String,
    session_id: String,
    session_kind: &'static str,
    title: Option<String>,
    cwd: Option<String>,
    last_activity_ms: Option<i64>,
    tail: Vec<RawTurn>,
}

impl Candidate {
    fn to_recent(&self, utc_offset_minutes: Option<i32>) -> RecentSession {
        RecentSession {
            source: self.source_id.clone(),
            session_id: self.session_id.clone(),
            session_kind: self.session_kind,
            title: self.title.clone(),
            cwd: self.cwd.clone(),
            last_activity_at: self.last_activity_ms.and_then(rfc3339),
            last_activity_local: self
                .last_activity_ms
                .and_then(|ms| local_rfc3339(ms, utc_offset_minutes)),
            last_activity_ms: self.last_activity_ms,
            tail: self
                .tail
                .iter()
                .map(|turn| RecentTailTurn {
                    role: turn.role,
                    timestamp: rfc3339(turn.timestamp_ms),
                    text: turn.text.clone(),
                    truncated: turn.truncated,
                    reference: turn.reference.clone(),
                    additional_refs: turn.additional_refs.clone(),
                })
                .collect(),
        }
    }
}

/// One source's contribution: candidates plus the frozen status triple.
struct Collected {
    source_id: String,
    status: &'static str,
    code: Option<&'static str>,
    detail: Option<Value>,
    message: Option<String>,
    sessions: Vec<Candidate>,
    scanned_candidates: u64,
    warnings: Vec<WarningEntry>,
}

impl Collected {
    fn new(source_id: &str, status_value: &'static str) -> Self {
        Collected {
            source_id: source_id.to_string(),
            status: status_value,
            code: None,
            detail: None,
            message: None,
            sessions: Vec::new(),
            scanned_candidates: 0,
            warnings: Vec::new(),
        }
    }

    fn unconfigured(source_id: &str, configured_session_sources: &[String]) -> Self {
        let list = if configured_session_sources.is_empty() {
            "(none)".to_string()
        } else {
            configured_session_sources.join(", ")
        };
        Collected {
            source_id: source_id.to_string(),
            status: status::UNCONFIGURED,
            code: Some(codes::SOURCE_NOT_CONFIGURED),
            detail: None,
            message: Some(format!(
                "source is not configured or is disabled; configured session sources: {list}"
            )),
            sessions: Vec::new(),
            scanned_candidates: 0,
            warnings: Vec::new(),
        }
    }

    fn unsupported(source_id: &str, reason: &str, message: String) -> Self {
        Collected {
            source_id: source_id.to_string(),
            status: status::UNSUPPORTED,
            code: Some(codes::SCHEMA_UNSUPPORTED),
            detail: Some(json!({ "reason": reason })),
            message: Some(message),
            sessions: Vec::new(),
            scanned_candidates: 0,
            warnings: Vec::new(),
        }
    }

    fn unavailable(source_id: &str, code: &'static str, reason: &str, message: String) -> Self {
        Collected {
            source_id: source_id.to_string(),
            status: status_for_code(code),
            code: Some(code),
            detail: Some(json!({ "reason": reason })),
            message: Some(message),
            sessions: Vec::new(),
            scanned_candidates: 0,
            warnings: Vec::new(),
        }
    }

    fn from_port(source_id: &str, error: &crate::recall::port::PortError) -> Self {
        let code = port_code(error);
        Collected {
            source_id: source_id.to_string(),
            status: status_for_code(code),
            code: Some(code),
            detail: port_detail(error),
            message: Some(error.message()),
            sessions: Vec::new(),
            scanned_candidates: 0,
            warnings: Vec::new(),
        }
    }

    fn exit_status(&self) -> &'static str {
        self.status
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub fn memory_recent(config: &RecallConfig, request: &RecentRequest) -> RecentResponse {
    memory_recent_with(config, request, &RecentOptions::default())
}

pub fn memory_recent_with(
    config: &RecallConfig,
    request: &RecentRequest,
    options: &RecentOptions,
) -> RecentResponse {
    if options.strict_snapshot {
        return RecentResponse::request_failure(&RecallError::new(
            codes::DB_UNAVAILABLE,
            "--strict-snapshot is not supported for memory_recent: the hot layer reads \
             live carriers in place",
        ));
    }

    let mut configured_session_sources: Vec<String> = config
        .enabled_sources()
        .into_iter()
        .filter(|id| {
            config
                .source(id)
                .map(|source| RECENT_SOURCE_KINDS.contains(&source.kind.as_str()))
                .unwrap_or(false)
        })
        .collect();
    // The names are echoed in `SOURCE_NOT_CONFIGURED`, so keep them ordered and
    // stable instead of following configuration order.
    configured_session_sources.sort();

    let requested: Vec<String> = match &request.sources {
        Some(ids) => ids.clone(),
        None => configured_session_sources.clone(),
    };

    let mut outcomes: Vec<Collected> = Vec::with_capacity(requested.len());
    for source_id in &requested {
        match config.source(source_id) {
            Some(source) if source.enabled => {
                if !RECENT_SOURCE_KINDS.contains(&source.kind.as_str()) {
                    outcomes.push(Collected::unsupported(
                        source_id,
                        "not_a_session_source",
                        format!(
                            "source {source_id} has kind {}; memory_recent only enumerates \
                             opencode, codex, claude_code, dsh and hermes sessions",
                            source.kind.as_str()
                        ),
                    ));
                    continue;
                }
                let mut collected = collect_source(source, request);
                if collected.status == status::OK || collected.status == status::PARTIAL {
                    let coverage_complete = collected.status == status::OK;
                    collected.detail = Some(coverage_detail(
                        &collected.detail,
                        &collected.sessions,
                        collected.scanned_candidates,
                        coverage_complete,
                    ));
                    if collected.sessions.is_empty() && coverage_complete {
                        collected.status = status::EMPTY;
                    }
                }
                outcomes.push(collected);
            }
            _ => outcomes.push(Collected::unconfigured(
                source_id,
                &configured_session_sources,
            )),
        }
    }

    let mut candidates: Vec<Candidate> = Vec::new();
    let mut warnings: Vec<WarningEntry> = config.unsupported_source_warnings("recent");
    warnings.extend(config.unsupported_key_warnings("recent"));
    for outcome in &outcomes {
        candidates.extend(outcome.sessions.iter().cloned());
        warnings.extend(outcome.warnings.iter().cloned());
    }

    candidates.sort_by(|left, right| {
        right
            .last_activity_ms
            .unwrap_or(0)
            .cmp(&left.last_activity_ms.unwrap_or(0))
            .then_with(|| left.source_id.cmp(&right.source_id))
            .then_with(|| left.session_id.cmp(&right.session_id))
    });
    candidates.truncate(request.limit);

    apply_budget(&mut candidates, request.max_bytes, &mut warnings);

    // Status array: priority, then configuration order, then request order.
    let mut order: Vec<usize> = (0..outcomes.len()).collect();
    order.sort_by_key(|index| {
        (
            status_priority(outcomes[*index].exit_status()),
            config
                .source_order_index(&outcomes[*index].source_id)
                .unwrap_or(usize::MAX),
            *index,
        )
    });

    let source_status: Vec<SourceStatus> = order
        .iter()
        .map(|index| {
            let outcome = &outcomes[*index];
            let mut entry = SourceStatus::new(outcome.source_id.clone(), outcome.exit_status());
            if let Some(code) = outcome.code {
                entry = entry.with_code(code);
            }
            if let Some(detail) = &outcome.detail {
                entry = entry.with_detail(detail.clone());
            }
            entry
        })
        .collect();

    let errors: Vec<ErrorEntry> = order
        .iter()
        .filter_map(|index| {
            let outcome = &outcomes[*index];
            if !crate::recall::contract::is_incomplete(outcome.exit_status()) {
                return None;
            }
            Some(ErrorEntry {
                code: outcome.code.unwrap_or(codes::INVALID_REQUEST),
                message: outcome
                    .message
                    .clone()
                    .unwrap_or_else(|| format!("source {}", outcome.exit_status())),
                source_id: Some(outcome.source_id.clone()),
            })
        })
        .collect();

    let statuses: Vec<&str> = outcomes
        .iter()
        .map(|outcome| outcome.exit_status())
        .collect();
    let exit_code = search_exit_code(!candidates.is_empty(), &statuses);

    RecentResponse {
        contract_version: CONTRACT_VERSION,
        sessions: candidates
            .iter()
            .map(|candidate| candidate.to_recent(options.utc_offset_minutes))
            .collect(),
        source_status,
        errors,
        warnings,
        exit_code,
    }
}

fn coverage_detail(
    current: &Option<Value>,
    sessions: &[Candidate],
    scanned_candidates: u64,
    coverage_complete: bool,
) -> Value {
    let mut detail = match current.clone() {
        Some(Value::Object(map)) => map,
        _ => Map::new(),
    };
    let mut coverage = match detail.remove("coverage") {
        Some(Value::Object(map)) => map,
        _ => Map::new(),
    };
    // Accepted candidates, not the total number of rows in the source store.
    coverage.insert("sessions_total".into(), json!(sessions.len() as u64));
    detail.insert("coverage".into(), Value::Object(coverage));
    detail.insert("coverage_complete".into(), json!(coverage_complete));
    detail.insert("scanned_candidates".into(), json!(scanned_candidates));
    Value::Object(detail)
}

fn collect_source(source: &SourceConfig, request: &RecentRequest) -> Collected {
    match source.kind {
        SourceKind::Opencode => collect_opencode(source, request),
        SourceKind::Codex => collect_codex(source, request),
        SourceKind::ClaudeCode => collect_claude(source, request),
        SourceKind::Dsh => collect_dsh(source, request),
        SourceKind::Hermes => collect_hermes(source, request),
        SourceKind::Kb | SourceKind::Device | SourceKind::Docs | SourceKind::Archive => {
            Collected::unsupported(
                &source.source_id,
                "not_a_session_source",
                format!(
                    "source {} has kind {}; memory_recent only enumerates session sources",
                    source.source_id,
                    source.kind.as_str()
                ),
            )
        }
    }
}

// ---------------------------------------------------------------------------
// OpenCode (SQLite V2)
// ---------------------------------------------------------------------------

fn collect_opencode(source: &SourceConfig, request: &RecentRequest) -> Collected {
    let root = Path::new(&source.root);
    let carrier = match resolve_sqlite(root, &source.root, source.db.as_deref()) {
        Ok(Some(carrier)) => carrier,
        Ok(None) => {
            return Collected::unavailable(
                &source.source_id,
                codes::DB_UNAVAILABLE,
                "database_missing",
                format!(
                    "opencode database is not available under {}",
                    root.display()
                ),
            )
        }
        Err(error) => return Collected::from_port(&source.source_id, &error),
    };
    if carrier.probe.generation != SCHEMA_SQLITE_V2 {
        return Collected::unsupported(
            &source.source_id,
            "recent_requires_sqlite_v2",
            format!(
                "memory_recent reads session_v2/session_message; this database probes as {}",
                carrier.probe.generation
            ),
        );
    }
    let conn = match open_readonly(&carrier.path) {
        Ok(conn) => conn,
        Err(error) => return Collected::from_port(&source.source_id, &error),
    };
    let _ = conn.busy_timeout(Duration::from_millis(250));
    let _ = conn.execute_batch("PRAGMA query_only = ON;");
    let tail_indexed = opencode_tail_is_indexed(&conn);
    let has_parts = match session_parts::available(&conn) {
        Ok(available) => available,
        Err(error) => return Collected::from_port(&source.source_id, &map_sqlite_error(error)),
    };

    let parent_expr = if carrier.probe.parent_id_available {
        "parent_id"
    } else {
        "NULL"
    };
    let sessions_sql = format!(
        "SELECT id, title, directory, {parent_expr}, time_updated FROM session_v2 \
         WHERE (?3 IS NULL OR id = ?3) \
         ORDER BY time_updated DESC, id ASC LIMIT ?1 OFFSET ?2"
    );

    let page = (request.limit * RECENT_CANDIDATE_FACTOR).max(RECENT_MIN_CANDIDATES);
    let mut collected = Collected::new(&source.source_id, status::OK);
    let mut accepted: Vec<Candidate> = Vec::new();
    let mut offset = 0usize;
    let mut unreadable = 0u64;
    let mut considered = 0u64;

    for _page in 0..RECENT_MAX_PAGES {
        if accepted.len() >= request.limit {
            break;
        }
        let mut statement = match conn.prepare(&sessions_sql) {
            Ok(statement) => statement,
            Err(error) => {
                let error = crate::recall::port::model::map_sqlite_error(error);
                return Collected::from_port(&source.source_id, &error);
            }
        };
        let rows = statement.query_map(
            rusqlite::params![page as i64, offset as i64, request.session_id.as_deref()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<i64>>(4)?.unwrap_or(0),
                ))
            },
        );
        let rows = match rows {
            Ok(rows) => rows,
            Err(error) => {
                let error = crate::recall::port::model::map_sqlite_error(error);
                return Collected::from_port(&source.source_id, &error);
            }
        };
        let mut seen_this_page = 0usize;
        for row in rows {
            let (session_id, title, cwd, parent_id, time_updated) = match row {
                Ok(row) => row,
                Err(_) => {
                    unreadable += 1;
                    continue;
                }
            };
            seen_this_page += 1;
            considered += 1;
            let session_kind = opencode_session_kind(parent_id.as_deref(), title.as_deref());
            let last_activity_ms = (time_updated > 0).then_some(time_updated);
            if !wanted(&session_id, session_kind, last_activity_ms, request) {
                continue;
            }
            let tail = opencode_tail(
                &conn,
                has_parts,
                source,
                &carrier.rel_path,
                &session_id,
                title.as_deref(),
                session_kind,
                request,
            );
            accepted.push(Candidate {
                source_id: source.source_id.clone(),
                session_id,
                session_kind,
                title: title.filter(|value| !value.is_empty()),
                cwd: cwd.filter(|value| !value.is_empty()),
                last_activity_ms,
                tail,
            });
            if accepted.len() >= request.limit {
                break;
            }
        }
        if seen_this_page < page {
            break;
        }
        offset += seen_this_page;
    }

    if unreadable > 0 {
        collected.status = status::PARTIAL;
        collected.code = Some(codes::RECORD_SKIPPED);
        collected.message = Some(format!(
            "opencode session rows could not be read (unreadable={unreadable})"
        ));
    }
    let mut detail = Map::new();
    detail.insert(
        "coverage".into(),
        json!({ "sessions_considered": considered }),
    );
    if !tail_indexed {
        // The database has no usable `(session_id, seq)` index: the tail read
        // is still row-capped, but it is no longer an index range. Disclose it
        // instead of claiming a bound the schema cannot back.
        detail.insert("reason".into(), json!("tail_query_unindexed"));
    }
    collected.detail = Some(Value::Object(detail));
    collected.scanned_candidates = considered;
    collected.sessions = accepted;
    collected
}

/// Whether the tail query resolves to an index range on this database. A
/// missing `(session_id, seq)` index turns a plan into `SCAN session_message`;
/// the caller discloses that instead of pretending the read is bounded.
fn opencode_tail_is_indexed(conn: &rusqlite::Connection) -> bool {
    let Ok(mut statement) = conn.prepare(OPENCODE_TAIL_PLAN_SQL) else {
        return false;
    };
    let rows = statement.query_map(
        rusqlite::params!["", MAX_ROW_BYTES as i64, RECENT_OPENCODE_TAIL_ROWS as i64],
        |row| row.get::<_, String>(3),
    );
    let Ok(rows) = rows else {
        return false;
    };
    for detail in rows.flatten() {
        if detail.contains("SCAN session_message") {
            return false;
        }
    }
    true
}

fn opencode_tail(
    conn: &rusqlite::Connection,
    has_parts: bool,
    source: &SourceConfig,
    rel_path: &str,
    session_id: &str,
    title: Option<&str>,
    session_kind: &'static str,
    request: &RecentRequest,
) -> Vec<RawTurn> {
    if request.tail_turns == 0 {
        return Vec::new();
    }
    let mut statement = match conn.prepare(OPENCODE_TAIL_SQL) {
        Ok(statement) => statement,
        Err(_) => return Vec::new(),
    };
    let rows = match statement.query_map(
        rusqlite::params![
            session_id,
            MAX_ROW_BYTES as i64,
            RECENT_OPENCODE_TAIL_ROWS as i64
        ],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<i64>>(3)?.unwrap_or(0),
                row.get::<_, Option<String>>(5)?,
                row.get::<_, Option<i64>>(4)?.unwrap_or(0),
            ))
        },
    ) {
        Ok(rows) => rows,
        Err(_) => return Vec::new(),
    };
    let mut turns: Vec<RawTurn> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    'rows: for row in rows {
        let Ok((message_id, message_type, time_created, data, data_len)) = row else {
            continue;
        };
        // Only conversation turns. `synthetic` rows carry instruction files the
        // host injected (AGENTS.md and the like), `system` rows host notices;
        // `compaction`, `idle` and the `*-switched` rows are bookkeeping.
        if !matches!(message_type.as_deref(), Some("user") | Some("assistant")) {
            continue;
        }
        let Some(data) = data else { continue };
        if data_len as u64 > MAX_ROW_BYTES {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(&data) else {
            continue;
        };
        if !value.is_object() {
            continue;
        }
        if has_parts
            && request.wants_role(ROLE_ASSISTANT)
            && session_parts::is_fallback(message_type.as_deref().unwrap_or(""), &value)
        {
            if let Some(turn) = opencode_part_turn(
                conn,
                source,
                rel_path,
                session_id,
                &message_id,
                title.unwrap_or(session_id),
                session_kind,
                time_created,
                request.max_chars_per_turn,
            ) {
                turns.push(turn);
                if turns.len() >= request.tail_turns {
                    break;
                }
            }
            continue;
        }
        let units = opencode_units(
            session_id,
            &message_id,
            title.unwrap_or(session_id),
            session_kind,
            rel_path,
            time_created,
            message_type.as_deref().unwrap_or(""),
            &sha256_hex(data.as_bytes()),
            &value,
        );
        for unit in units.iter().rev() {
            let role = if unit.kind == UNIT_KIND_TEXT {
                unit.role
            } else {
                ROLE_TOOL
            };
            if !request.wants_role(role) {
                continue;
            }
            if !seen.insert(format!("{}|{}", unit.locator.compact_json(), unit.kind)) {
                continue;
            }
            turns.push(raw_turn(
                source,
                SOURCE_KIND_OPENCODE,
                SCHEMA_SQLITE_V2,
                rel_path,
                unit,
                request.max_chars_per_turn,
            ));
            if turns.len() >= request.tail_turns {
                break 'rows;
            }
        }
    }
    turns.reverse();
    turns
}

/// Recent counts one assistant message as one turn, but each source row
/// retains its own readback ref, just as it does in the full-text index.
#[allow(clippy::too_many_arguments)]
fn opencode_part_turn(
    conn: &rusqlite::Connection,
    source: &SourceConfig,
    rel_path: &str,
    session_id: &str,
    message_id: &str,
    title: &str,
    session_kind: &'static str,
    time_created: i64,
    max_chars: usize,
) -> Option<RawTurn> {
    let mut turn: Option<RawTurn> = None;
    let mut remaining = max_chars;
    session_parts::visit_message(conn, session_id, message_id, |part| {
        let Some(unit) = opencode_part_unit(
            session_id,
            message_id,
            title,
            session_kind,
            rel_path,
            time_created,
            &part,
        ) else {
            return true;
        };
        // Use the same blank-line separator as SessionMessage's text projection.
        const SEPARATOR: &str = "\n\n";
        if let Some(acc) = &mut turn {
            if remaining <= SEPARATOR.len() {
                acc.truncated = true;
                return false;
            }
            let next = raw_turn(
                source,
                SOURCE_KIND_OPENCODE,
                SCHEMA_SQLITE_V2,
                rel_path,
                &unit,
                remaining - SEPARATOR.len(),
            );
            remaining -= SEPARATOR.len() + next.text.chars().count();
            acc.text.push_str(SEPARATOR);
            acc.text.push_str(&next.text);
            acc.additional_refs.push(next.reference);
            acc.truncated = next.truncated;
            !acc.truncated
        } else {
            let first = raw_turn(
                source,
                SOURCE_KIND_OPENCODE,
                SCHEMA_SQLITE_V2,
                rel_path,
                &unit,
                remaining,
            );
            remaining -= first.text.chars().count();
            let keep_reading = !first.truncated;
            turn = Some(first);
            keep_reading
        }
    })
    .ok()?;
    turn
}

// ---------------------------------------------------------------------------
// Claude Code (top-level JSONL transcripts)
// ---------------------------------------------------------------------------

fn collect_claude(source: &SourceConfig, request: &RecentRequest) -> Collected {
    let root = Path::new(&source.root);
    let mut files: Vec<(String, PathBuf, i64)> = Vec::new();
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) => {
            return Collected::unavailable(
                &source.source_id,
                codes::SOURCE_ROOT_INVALID,
                "root_unreadable",
                format!(
                    "claude_code root cannot be enumerated: {}: {error}",
                    root.display()
                ),
            )
        }
    };
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_dir() || file_type.is_symlink() {
            continue;
        }
        let project = entry.file_name().to_string_lossy().to_string();
        let Ok(children) = std::fs::read_dir(entry.path()) else {
            continue;
        };
        for child in children.flatten() {
            let Ok(kind) = child.file_type() else {
                continue;
            };
            // Top-level transcripts only: `<session>/subagents/*.jsonl` is a
            // worker transcript and is not a session candidate.
            if !kind.is_file() || kind.is_symlink() {
                continue;
            }
            let name = child.file_name().to_string_lossy().to_string();
            if !crate::recall::providers::claude_code::is_transcript_name(&name) {
                continue;
            }
            let mtime = mtime_ms(&child.path()).unwrap_or(0);
            files.push((format!("{project}/{name}"), child.path(), mtime));
        }
    }
    if let Some(target) = request.session_id.as_deref() {
        files.retain(|(rel_path, _, _)| {
            crate::recall::providers::claude_code::infer_session_id(rel_path).as_deref()
                == Some(target)
        });
    } else if let Some(floor) = since_mtime_floor(request) {
        files.retain(|(_, _, mtime)| *mtime == 0 || *mtime >= floor);
    }
    files.sort_by(|left, right| right.2.cmp(&left.2).then_with(|| left.0.cmp(&right.0)));

    let window = (request.limit * RECENT_CANDIDATE_FACTOR)
        .max(RECENT_MIN_CANDIDATES)
        .min(files.len());
    let mut accepted: Vec<Candidate> = Vec::new();
    let mut unreadable = 0u64;
    let mut collected = Collected::new(&source.source_id, status::OK);
    // The candidate window is selected by file mtime, but the ranking key is
    // the conversation clock, and the two diverge once bookkeeping records
    // (queue-operation etc.) touch the file: a session whose last turn is the
    // newest can carry an older mtime than its neighbours. Every file in the
    // window is therefore projected; the shared cross-source sort and
    // `truncate(limit)` decide which sessions survive.
    for (rel_path, path, mtime) in files.into_iter().take(window) {
        collected.scanned_candidates += 1;
        match claude_candidate(source, &rel_path, &path, mtime, request) {
            Ok(Some((candidate, warnings))) => {
                if !wanted(
                    &candidate.session_id,
                    candidate.session_kind,
                    candidate.last_activity_ms,
                    request,
                ) {
                    continue;
                }
                accepted.push(candidate);
                collected.warnings.extend(warnings);
            }
            Ok(None) => {}
            Err(_) => unreadable += 1,
        }
    }
    if unreadable > 0 {
        collected.status = status::PARTIAL;
        collected.code = Some(codes::RECORD_SKIPPED);
        collected.message = Some(format!(
            "claude_code transcripts could not be read (unreadable={unreadable})"
        ));
    }
    collected.sessions = accepted;
    collected
}

/// A Claude Code record that carries a conversation turn. Compaction summary
/// records are `type: "user"` and therefore count; a human mid-turn prompt
/// (`queued_command` attachment) is a turn too. `queue-operation`, `system`,
/// `summary`, `file-history-snapshot`, `task-notification` attachments and any
/// other type do not.
fn claude_is_conversation_record(value: &Value) -> bool {
    match value.get("type").and_then(Value::as_str) {
        Some("user") | Some("assistant") => true,
        Some("attachment") => matches!(
            crate::recall::providers::claude_code::project_record(value),
            crate::recall::providers::claude_code::ClaudeRecordOutcome::Units(_)
        ),
        _ => false,
    }
}

fn claude_candidate(
    source: &SourceConfig,
    rel_path: &str,
    path: &Path,
    mtime: i64,
    request: &RecentRequest,
) -> Result<Option<(Candidate, Vec<WarningEntry>)>, std::io::Error> {
    let head = read_head_values(path, RECENT_HEAD_WINDOW_BYTES)?;

    let mut session_id: Option<String> =
        crate::recall::providers::claude_code::infer_session_id(rel_path);
    let mut cwd: Option<String> = None;
    let mut title: Option<String> = None;
    let mut entrypoint: Option<String> = None;
    let mut is_sidechain = false;

    for (_, value) in &head {
        let meta = crate::recall::providers::claude_code::record_meta(value);
        is_sidechain |= meta.is_sidechain;
        if session_id.is_none() {
            session_id = meta.session_id.clone();
        }
        if cwd.is_none() {
            cwd = value
                .get("cwd")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(str::to_string);
        }
        if entrypoint.is_none() {
            entrypoint = value
                .get("entrypoint")
                .and_then(Value::as_str)
                .filter(|value| !value.trim().is_empty())
                .map(str::to_string);
        }
        if title.is_none() && value.get("type").and_then(Value::as_str) == Some("summary") {
            title = value
                .get("summary")
                .and_then(Value::as_str)
                .filter(|value| !value.trim().is_empty())
                .map(str::to_string);
        }
    }

    let mut window_bytes = RECENT_TAIL_WINDOW_BYTES;
    let (turns, conversation_ms, any_record_ms, window_truncated) = loop {
        let tail = read_tail_lines(
            path,
            window_bytes,
            request.tail_turns > 0 && !request.fast_scan,
        )?;
        let mut turns: Vec<Option<RawTurn>> = Vec::new();
        let mut held: Vec<(String, usize, RawTurn)> = Vec::new();
        let mut dedup = QueuedDedup::default();
        let mut seen: HashSet<String> = HashSet::new();
        let mut conversation_ms: Option<i64> = None;
        let mut any_record_ms: Option<i64> = None;
        for line in tail.lines.iter().rev() {
            let Ok(value) = serde_json::from_slice::<Value>(&line.bytes) else {
                continue;
            };
            let meta = crate::recall::providers::claude_code::record_meta(&value);
            is_sidechain |= meta.is_sidechain;
            if let Some(ms) = meta.timestamp_ms {
                if any_record_ms.is_none() {
                    any_record_ms = Some(ms);
                }
                if conversation_ms.is_none() && claude_is_conversation_record(&value) {
                    conversation_ms = Some(ms);
                }
            }
            if cwd.is_none() {
                cwd = value
                    .get("cwd")
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string);
            }
            if entrypoint.is_none() {
                entrypoint = value
                    .get("entrypoint")
                    .and_then(Value::as_str)
                    .filter(|value| !value.trim().is_empty())
                    .map(str::to_string);
            }
            if title.is_none() && value.get("type").and_then(Value::as_str) == Some("summary") {
                title = value
                    .get("summary")
                    .and_then(Value::as_str)
                    .filter(|value| !value.trim().is_empty())
                    .map(str::to_string);
            }
            if request.tail_turns == 0 {
                continue;
            }
            // Once the requested turns are collected and no mid-turn prompt is
            // still waiting for a verdict, older lines cannot change the answer:
            // skip them (the clocks above are already updated).
            if held.is_empty() && turns.len() >= request.tail_turns {
                continue;
            }
            let row_sha256 = sha256_hex(&line.bytes);
            let units = claude_record_units(rel_path, line.index, &value, &row_sha256);
            for unit in units.iter().rev() {
                // A mid-turn prompt whose text an ordinary user record also
                // carries is one turn, not two. The verdict is resolved after
                // the whole window is projected (`held` slots).
                let ordinary_user_text = unit.kind == UNIT_KIND_TEXT && unit.role == ROLE_USER;
                let verdict = dedup.observe(
                    unit.locator.pointer.as_deref(),
                    ordinary_user_text,
                    &unit.text,
                );
                if verdict == QueuedVerdict::Drop {
                    continue;
                }
                let role = if unit.kind == UNIT_KIND_TEXT {
                    unit.role
                } else {
                    ROLE_TOOL
                };
                if !request.wants_role(role) {
                    continue;
                }
                if !seen.insert(format!("{}|{}", unit.locator.compact_json(), unit.kind)) {
                    continue;
                }
                let turn = raw_turn(
                    source,
                    SOURCE_KIND_CLAUDE_CODE,
                    SCHEMA_CLAUDE_JSONL_V1,
                    rel_path,
                    unit,
                    request.max_chars_per_turn,
                );
                if verdict == QueuedVerdict::Hold {
                    held.push((unit.text.trim().to_string(), turns.len(), turn));
                    turns.push(None);
                } else {
                    turns.push(Some(turn));
                }
            }
        }
        // Release the held prompts no ordinary user record duplicated, then keep
        // the newest `tail_turns` (the window was walked newest first).
        for (key, slot, turn) in held {
            if dedup.release(&key) {
                turns[slot] = Some(turn);
            }
        }
        let mut turns: Vec<RawTurn> = turns.into_iter().flatten().collect();
        turns.truncate(request.tail_turns);
        turns.reverse();
        let short = turns.len() < request.tail_turns || conversation_ms.is_none();
        if !short || !tail.window_truncated || window_bytes >= RECENT_TAIL_WINDOW_MAX_BYTES {
            break (turns, conversation_ms, any_record_ms, tail.window_truncated);
        }
        window_bytes = (window_bytes * RECENT_TAIL_WINDOW_GROWTH).min(RECENT_TAIL_WINDOW_MAX_BYTES);
    };

    let session_id = session_id.unwrap_or_else(|| rel_path.to_string());
    // A turn is what happened in the conversation; later bookkeeping records
    // (queue-operation, system, attachment, summary, file-history-snapshot)
    // must not move the clock. Fall back to the newest record of any type, then
    // to the file mtime.
    let last_activity_ms = conversation_ms
        .or(any_record_ms)
        .or((mtime > 0).then_some(mtime));
    let session_kind =
        claude_session_kind_with_entrypoint(rel_path, is_sidechain, entrypoint.as_deref());

    let mut warnings = Vec::new();
    if request.tail_turns > 0 && window_truncated && turns.len() < request.tail_turns {
        warnings.push(WarningEntry {
            scope: "recent",
            source_id: Some(source.source_id.clone()),
            hit_id: None,
            code: crate::recall::contract::WARN_RECENT_TAIL_WINDOW,
            detail: Some(json!({
                "reason": "tail_window",
                "rel_path": rel_path,
                "window_bytes": window_bytes,
            })),
        });
    }

    Ok(Some((
        Candidate {
            source_id: source.source_id.clone(),
            session_id,
            session_kind,
            title,
            cwd,
            last_activity_ms,
            tail: turns,
        },
        warnings,
    )))
}

// ---------------------------------------------------------------------------
// DSH (frame-append session logs)
// ---------------------------------------------------------------------------

fn collect_dsh(source: &SourceConfig, request: &RecentRequest) -> Collected {
    let root = Path::new(&source.root);
    let inventory = match dsh::inventory(root) {
        Ok(inventory) => inventory,
        Err(error) => {
            return Collected::unavailable(
                &source.source_id,
                codes::SOURCE_ROOT_INVALID,
                "root_unreadable",
                format!("dsh root cannot be enumerated: {}: {error}", root.display()),
            )
        }
    };
    let mut files: Vec<(String, PathBuf, String, i64)> = Vec::new();
    for session in &inventory.sessions {
        if let Some(target) = request.session_id.as_deref() {
            if session.dir_name != target {
                // The header id is authoritative; the directory name is a path
                // encoding of it and may carry a prefix.
                match dsh::read_header(&session.path, &session.rel_path) {
                    Ok(header) if header.session_id == target => {}
                    _ => continue,
                }
            }
        }
        let mtime = mtime_ms(&session.path).unwrap_or(0);
        if request.session_id.is_none() {
            if let Some(floor) = since_mtime_floor(request) {
                if mtime != 0 && mtime < floor {
                    continue;
                }
            }
        }
        files.push((
            session.rel_path.clone(),
            session.path.clone(),
            session.dir_name.clone(),
            mtime,
        ));
    }
    files.sort_by(|left, right| right.3.cmp(&left.3).then_with(|| left.0.cmp(&right.0)));
    let window = if request.session_id.is_some() {
        files.len()
    } else {
        (request.limit * RECENT_CANDIDATE_FACTOR)
            .max(RECENT_MIN_CANDIDATES)
            .min(files.len())
    };

    let mut collected = Collected::new(&source.source_id, status::OK);
    let mut accepted: Vec<Candidate> = Vec::new();
    let mut unreadable = 0u64;
    let mut unsupported = inventory.unsupported.len() as u64;
    collected.scanned_candidates = unsupported;
    for (rel_path, path, dir_name, mtime) in files.into_iter().take(window) {
        collected.scanned_candidates += 1;
        let header = match dsh::read_header(&path, &rel_path) {
            Ok(header) => header,
            Err(_) => {
                unreadable += 1;
                continue;
            }
        };
        if header.version > dsh::DSH_MAX_GENERATION {
            unsupported += 1;
            continue;
        }
        let log = match dsh::open_tail(&path, &rel_path) {
            Ok(log) => log,
            Err(_) => {
                unreadable += 1;
                continue;
            }
        };
        if log.state == dsh::DshReadState::Corrupt {
            // An interior corrupt frame means the tail after it is not the real
            // tail; the session is disclosed as unreadable instead of guessed.
            unreadable += 1;
            continue;
        }
        let Some(meta) = dsh::scan_meta_with_header(&header, &log, &dir_name) else {
            unsupported += 1;
            continue;
        };
        let last_activity_ms = meta
            .conversation_ms
            .or(meta.any_ms)
            .or((mtime > 0).then_some(mtime));
        if !wanted(
            &meta.session_id,
            meta.session_kind,
            last_activity_ms,
            request,
        ) {
            continue;
        }
        let mut turns: Vec<RawTurn> = Vec::new();
        if request.tail_turns > 0 {
            let lines: Vec<_> = log.lines().collect();
            let mut seen: HashSet<String> = HashSet::new();
            for line in lines.iter().rev() {
                if turns.len() >= request.tail_turns {
                    break;
                }
                let Ok(value) = serde_json::from_slice::<Value>(line.bytes) else {
                    continue;
                };
                let row_sha256 = sha256_hex(line.bytes);
                let units =
                    dsh_record_units(rel_path.as_str(), line.index, &value, &row_sha256, &meta);
                for unit in units.iter().rev() {
                    if turns.len() >= request.tail_turns {
                        break;
                    }
                    let role = if unit.kind == UNIT_KIND_TEXT {
                        unit.role
                    } else {
                        ROLE_TOOL
                    };
                    if !request.wants_role(role) {
                        continue;
                    }
                    if !seen.insert(format!("{}|{}", unit.locator.compact_json(), unit.kind)) {
                        continue;
                    }
                    turns.push(raw_turn(
                        source,
                        SOURCE_KIND_DSH,
                        SCHEMA_DSH_JSONL,
                        &rel_path,
                        unit,
                        request.max_chars_per_turn,
                    ));
                }
            }
            turns.reverse();
        }

        let mut warnings = Vec::new();
        if request.tail_turns > 0 && log.window_truncated && turns.len() < request.tail_turns {
            warnings.push(WarningEntry {
                scope: "recent",
                source_id: Some(source.source_id.clone()),
                hit_id: None,
                code: crate::recall::contract::WARN_RECENT_TAIL_WINDOW,
                detail: Some(json!({
                    "reason": "tail_window",
                    "rel_path": rel_path,
                    "window_bytes": dsh::DSH_SCAN_LIMIT_BYTES,
                })),
            });
        }
        collected.warnings.extend(warnings);
        accepted.push(Candidate {
            source_id: source.source_id.clone(),
            session_id: meta.session_id,
            session_kind: meta.session_kind,
            title: if meta.title.is_empty() {
                None
            } else {
                Some(meta.title)
            },
            cwd: meta.cwd,
            last_activity_ms,
            tail: turns,
        });
    }
    if unsupported > 0 {
        let mut detail = Map::new();
        detail.insert(
            "coverage".into(),
            json!({ "unsupported_sessions": unsupported }),
        );
        collected.detail = Some(Value::Object(detail));
    }
    if unreadable > 0 {
        collected.status = status::PARTIAL;
        collected.code = Some(codes::RECORD_SKIPPED);
        collected.message = Some(format!(
            "dsh session logs could not be read (unreadable={unreadable})"
        ));
    } else if unsupported > 0 && accepted.is_empty() {
        collected.status = status::UNSUPPORTED;
        collected.code = Some(codes::SCHEMA_UNSUPPORTED);
        collected.message = Some(format!(
            "{unsupported} session(s) use an unknown DSH generation"
        ));
    } else if unsupported > 0 {
        collected.status = status::PARTIAL;
        collected.code = Some(codes::SCHEMA_UNSUPPORTED);
        collected.message = Some(format!(
            "{unsupported} session(s) use an unknown DSH generation"
        ));
    }
    collected.sessions = accepted;
    collected
}

// ---------------------------------------------------------------------------
// Codex (state DB thread clock + rollout tail)
// ---------------------------------------------------------------------------

struct CodexThread {
    id: String,
    title: Option<String>,
    cwd: Option<String>,
    updated_ms: Option<i64>,
    rollout_path: Option<PathBuf>,
    rollout_only: bool,
}

fn collect_codex(source: &SourceConfig, request: &RecentRequest) -> Collected {
    let root = Path::new(&source.root);
    if codex_storage_dir(source, root)
        .map(|dir| !dir.is_dir())
        .unwrap_or(true)
    {
        return Collected::unavailable(
            &source.source_id,
            codes::DB_UNAVAILABLE,
            "storage_missing",
            format!(
                "codex storage directory is not available under {}",
                root.display()
            ),
        );
    }
    let files = match codex_files(source) {
        Ok(files) => files,
        Err(error) => {
            let code = match error {
                BuildError::Unsupported(_) => codes::SCHEMA_UNSUPPORTED,
                _ => codes::DB_UNAVAILABLE,
            };
            return Collected::unavailable(
                &source.source_id,
                code,
                "storage_unreadable",
                error.message(),
            );
        }
    };
    let mut by_id: HashMap<String, (String, PathBuf)> = HashMap::new();
    for (rel_path, path) in &files {
        // Codex keeps rotated/retired carriers under backup directories. They
        // are not live sessions: supplementing from them resurrects history the
        // state DB has already moved on from.
        if is_retired_codex_rel(rel_path) {
            continue;
        }
        if let Some(id) = crate::providers::codex::infer_session_id_from_filename(path) {
            by_id
                .entry(id)
                .or_insert_with(|| (rel_path.clone(), path.clone()));
        }
    }

    let pool = (request.limit * RECENT_CANDIDATE_FACTOR * 2)
        .max(RECENT_MIN_CANDIDATES * 2)
        .min(400);
    let db = codex_db_path(source, root);
    let threads = match db.as_deref() {
        Some(db) if db.is_file() => load_codex_threads(db, pool, request.session_id.as_deref()),
        _ => Err("codex state database is not available".to_string()),
    };

    let mut collected = Collected::new(&source.source_id, status::OK);
    let (mut threads, fallback_reason) = match threads {
        Ok(mut threads) => {
            // A successful SQL query does not prove coverage: an empty table,
            // a directed miss, or rows filtered below the requested limit all
            // need the rollout inventory. A rollout whose thread row exists is
            // dropped here, so the database row always wins its session entry
            // no matter which tier is more active.
            if request.session_id.is_none() || threads.is_empty() {
                // Only pay for the anti-join when the supplement actually runs:
                // a directed lookup or a full page needs no extra query.
                let known_ids = db
                    .as_deref()
                    .filter(|path| path.is_file())
                    .map(|path| codex_known_ids(path, by_id.keys()))
                    .unwrap_or_default();
                // A rollout supplement must not be crowded out by DB rows that
                // sit outside the SQL window: exclude every rollout that has a
                // thread row anywhere in the table, not just the fetched page.
                threads.extend(codex_rollout_threads(
                    &by_id,
                    request.session_id.as_deref(),
                    pool,
                    &known_ids,
                ));
            }
            (threads, None)
        }
        Err(reason) => (
            codex_rollout_threads(&by_id, request.session_id.as_deref(), pool, &HashSet::new()),
            Some(reason),
        ),
    };
    // Both tiers form one list on the same normalized activity clock the
    // response ranks by (`updated_ms`, else the rollout mtime, exactly like
    // `codex_candidate`): the accept loop's early `limit` cut then always
    // keeps the newest sessions, so `limit=1` is a prefix of `limit=2` even
    // when a missing thread row hides the newest rollout. Stable order keeps
    // the SQL order among ties; rows without any clock sort last instead of
    // failing the source.
    threads.sort_by(|left, right| {
        codex_thread_activity(right, &by_id).cmp(&codex_thread_activity(left, &by_id))
    });
    // The merged candidate pool stays bounded: two tiers may contribute up to
    // one pool each, so trim back to the advertised window after ranking.
    threads.truncate(pool);
    if let Some(reason) = fallback_reason {
        let mut detail = Map::new();
        detail.insert(
            "reason".into(),
            json!(format!("state_db_fallback: {reason}")),
        );
        collected.detail = Some(Value::Object(detail));
    }

    let mut accepted: Vec<Candidate> = Vec::new();
    let mut unreadable = 0u64;
    let mut seen: HashSet<String> = HashSet::new();
    for thread in threads {
        if accepted.len() >= request.limit {
            break;
        }
        // Dedupe by the session a rollout really carries, not by the filename
        // key: a retired alias file may embed another session's id, and
        // reserving that id before the filters run would hide the real session.
        // Only an accepted candidate consumes its identity.
        collected.scanned_candidates += 1;
        match codex_candidate(source, root, &by_id, thread, request) {
            Ok(Some(candidate)) => {
                if !wanted(
                    &candidate.session_id,
                    candidate.session_kind,
                    candidate.last_activity_ms,
                    request,
                ) {
                    continue;
                }
                if !seen.insert(candidate.session_id.clone()) {
                    continue;
                }
                accepted.push(candidate);
            }
            Ok(None) => {}
            Err(_) => unreadable += 1,
        }
    }

    if unreadable > 0 {
        collected.status = status::PARTIAL;
        collected.code = Some(codes::RECORD_SKIPPED);
        collected.message = Some(format!(
            "codex rollouts could not be read (unreadable={unreadable})"
        ));
    }
    collected.sessions = accepted;
    collected
}

/// Rollout carriers under retired directories are archived history, not live
/// sessions. Codex rotates them out of `sessions/` but a crash can leave a
/// copy behind; supplementing recall from those resurrects finished work.
fn is_retired_codex_rel(rel_path: &str) -> bool {
    rel_path
        .split(['/', '\\'])
        .any(|part| matches!(part, "backup" | "backups" | "archived_sessions"))
}

/// Thread ids the state DB already knows, for the rollout supplement's
/// anti-join. Only the ids present on disk are probed, in chunks, so a large
/// `threads` table is never loaded wholesale; any read failure degrades to an
/// empty set (the supplement then behaves like the no-DB fallback).
fn codex_known_ids<'a>(path: &Path, ids: impl Iterator<Item = &'a String>) -> HashSet<String> {
    let Ok(conn) = open_readonly(path) else {
        return HashSet::new();
    };
    let _ = conn.execute_batch("PRAGMA query_only = ON;");
    let all: Vec<&String> = ids.collect();
    let mut known = HashSet::new();
    for chunk in all.chunks(400) {
        let placeholders = std::iter::repeat_n("?", chunk.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!("SELECT id FROM threads WHERE id IN ({placeholders})");
        let Ok(mut statement) = conn.prepare(&sql) else {
            continue;
        };
        let Ok(rows) = statement.query_map(rusqlite::params_from_iter(chunk.iter()), |row| {
            row.get::<_, String>(0)
        }) else {
            continue;
        };
        for id in rows.flatten() {
            known.insert(id);
        }
    }
    known
}

fn codex_rollout_threads(
    by_id: &HashMap<String, (String, PathBuf)>,
    session_id: Option<&str>,
    pool: usize,
    exclude: &HashSet<String>,
) -> Vec<CodexThread> {
    let mut fallback: Vec<(String, String, PathBuf, i64)> = by_id
        .iter()
        .filter(|(id, _)| {
            session_id
                .map(|target| target == id.as_str())
                .unwrap_or(true)
        })
        .filter(|(id, _)| !exclude.contains(id.as_str()))
        .map(|(id, (rel_path, path))| {
            (
                id.clone(),
                rel_path.clone(),
                path.clone(),
                mtime_ms(path).unwrap_or(0),
            )
        })
        .collect();
    fallback.sort_by(|left, right| right.3.cmp(&left.3).then_with(|| left.1.cmp(&right.1)));
    fallback
        .into_iter()
        .take(pool)
        .map(|(id, _, path, mtime)| CodexThread {
            id,
            title: None,
            cwd: None,
            updated_ms: (mtime > 0).then_some(mtime),
            rollout_path: Some(path),
            rollout_only: true,
        })
        .collect()
}

/// The activity clock a thread is ranked by: the normalized thread clock, or
/// the rollout mtime when the state DB carries no usable clock. Mirrors
/// `codex_candidate`'s `last_activity_ms` fallback so the merge order and the
/// ranked response never disagree.
fn codex_thread_activity(
    thread: &CodexThread,
    by_id: &HashMap<String, (String, PathBuf)>,
) -> Option<i64> {
    thread.updated_ms.or_else(|| {
        thread
            .rollout_path
            .as_deref()
            .and_then(|path| mtime_ms(path))
            .filter(|ms| *ms > 0)
            .or_else(|| {
                by_id
                    .get(&thread.id)
                    .and_then(|(_, path)| mtime_ms(path))
                    .filter(|ms| *ms > 0)
            })
    })
}

fn codex_candidate(
    source: &SourceConfig,
    root: &Path,
    by_id: &HashMap<String, (String, PathBuf)>,
    thread: CodexThread,
    request: &RecentRequest,
) -> Result<Option<Candidate>, std::io::Error> {
    let (rel_path, path) = match thread.rollout_path.as_deref() {
        Some(path)
            if !thread.rollout_only
                && path.is_file()
                && rel_from_root(&source.root, path).is_some()
                && rel_from_root(&root.to_string_lossy(), path).is_some() =>
        {
            (
                rel_from_root(&source.root, path).unwrap_or_default(),
                path.to_path_buf(),
            )
        }
        _ => match by_id.get(&thread.id) {
            Some(entry) => entry.clone(),
            None => return Ok(None),
        },
    };
    let marker = read_codex_marker(&path);
    let session_id = marker
        .as_ref()
        .and_then(|marker| marker.session_id.clone())
        .or_else(|| crate::providers::codex::infer_session_id_from_filename(&path))
        .unwrap_or_else(|| thread.id.clone());
    let session_kind = codex_session_kind(marker.as_ref().and_then(|m| m.source.as_deref()));
    let file_mtime = mtime_ms(&path).unwrap_or(0);
    let last_activity_ms = thread.updated_ms.or((file_mtime > 0).then_some(file_mtime));
    let cwd = thread
        .cwd
        .or_else(|| marker.as_ref().and_then(|marker| marker.cwd.clone()));
    let title = thread.title.filter(|value| !value.trim().is_empty());

    let mut turns: Vec<RawTurn> = Vec::new();
    if request.tail_turns > 0 {
        let tail = read_tail_lines(&path, RECENT_TAIL_WINDOW_BYTES, !request.fast_scan)?;
        let mut seen: HashSet<String> = HashSet::new();
        for line in tail.lines.iter().rev() {
            let Ok(value) = serde_json::from_slice::<Value>(&line.bytes) else {
                continue;
            };
            if turns.len() >= request.tail_turns {
                break;
            }
            let row_sha256 = sha256_hex(&line.bytes);
            let units =
                codex_record_units(&rel_path, line.index, &value, &row_sha256, marker.as_ref());
            for unit in units.iter().rev() {
                if turns.len() >= request.tail_turns {
                    break;
                }
                let role = if unit.kind == UNIT_KIND_TEXT {
                    unit.role
                } else {
                    ROLE_TOOL
                };
                if !request.wants_role(role) {
                    continue;
                }
                if !seen.insert(format!("{}|{}", unit.locator.compact_json(), unit.kind)) {
                    continue;
                }
                turns.push(raw_turn(
                    source,
                    SOURCE_KIND_CODEX,
                    SCHEMA_CODEX_ROLLOUT_V1,
                    &rel_path,
                    unit,
                    request.max_chars_per_turn,
                ));
            }
        }
        turns.reverse();
    }

    Ok(Some(Candidate {
        source_id: source.source_id.clone(),
        session_id,
        session_kind,
        title,
        cwd,
        last_activity_ms,
        tail: turns,
    }))
}

fn read_codex_marker(path: &Path) -> Option<CodexMarker> {
    let file = std::fs::File::open(path).ok()?;
    let mut reader = BufReader::with_capacity(64 * 1024, file);
    codex_marker_from_reader(&mut reader)
}

fn load_codex_threads(
    path: &Path,
    limit: usize,
    session_id: Option<&str>,
) -> Result<Vec<CodexThread>, String> {
    let conn = open_readonly(path).map_err(|error| error.message())?;
    let _ = conn.busy_timeout(Duration::from_millis(250));
    let _ = conn.execute_batch("PRAGMA query_only = ON;");
    let columns = table_columns(&conn, "threads")
        .map_err(|error| error.message())?
        .ok_or_else(|| "codex state database has no threads table".to_string())?;
    if !columns.contains("id") {
        return Err("codex threads table has no id column".to_string());
    }
    // Codex bumps the state schema; probe for a clock instead of guessing one.
    const TIME_COLUMNS: [&str; 6] = [
        "updated_at_ms",
        "updated_at",
        "updated_ms",
        "updated",
        "created_at_ms",
        "created_at",
    ];
    let time_column = TIME_COLUMNS
        .iter()
        .find(|column| columns.contains(**column))
        .copied();
    let title = if columns.contains("title") {
        "title"
    } else {
        "NULL"
    };
    let cwd = if columns.contains("cwd") {
        "cwd"
    } else {
        "NULL"
    };
    let rollout = if columns.contains("rollout_path") {
        "rollout_path"
    } else {
        "NULL"
    };
    let time_expr = time_column.unwrap_or("NULL");
    let order = match time_column {
        Some(column) => {
            // Normalize numeric clocks before LIMIT; otherwise every millisecond
            // value sorts ahead of every seconds value regardless of the actual
            // instant. SQLite must sort the candidate rows instead of using a
            // raw clock index: a large threads table pays one bounded top-N sort,
            // while tail reads and decoded rows remain capped by `limit`.
            let key = format!(
                "CASE WHEN typeof({column}) IN ('integer', 'real') THEN \
                 CASE WHEN CAST({column} AS REAL) >= 100000000000.0 \
                 THEN CAST({column} AS INTEGER) \
                 ELSE CAST({column} AS INTEGER) * 1000 END \
                 WHEN typeof({column}) = 'text' \
                 THEN CAST(ROUND((julianday({column}) - 2440587.5) * 86400000.0) AS INTEGER) \
                 ELSE NULL END"
            );
            // Unparseable clocks sort after every parseable row and stay
            // deterministic (`id ASC`); a full window may still exclude them,
            // which is the same tail the raw ORDER BY dropped. Injecting a
            // synthetic instant would promote malformed rows instead.
            format!("({key} IS NULL), {key} DESC, id ASC")
        }
        None => "rowid DESC".to_string(),
    };
    let sql = format!(
        "SELECT id, {title}, {cwd}, {rollout}, {time_expr} FROM threads \
         WHERE (?2 IS NULL OR id = ?2) ORDER BY {order} LIMIT ?1"
    );
    let mut statement = conn
        .prepare(&sql)
        .map_err(|error| map_sqlite_error(error).message())?;
    let rows = statement
        .query_map(rusqlite::params![limit as i64, session_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, rusqlite::types::Value>(4)?,
            ))
        })
        .map_err(|error| map_sqlite_error(error).message())?;
    let mut threads = Vec::new();
    for row in rows {
        let (id, title, cwd, rollout_path, raw_time): (
            String,
            Option<String>,
            Option<String>,
            Option<String>,
            rusqlite::types::Value,
        ) = row.map_err(|error| map_sqlite_error(error).message())?;
        threads.push(CodexThread {
            id,
            title,
            cwd,
            updated_ms: sqlite_time_ms(&raw_time),
            rollout_path: rollout_path
                .filter(|value| !value.trim().is_empty())
                .map(PathBuf::from),
            rollout_only: false,
        });
    }
    Ok(threads)
}

fn sqlite_time_ms(value: &rusqlite::types::Value) -> Option<i64> {
    use rusqlite::types::Value as SqlValue;
    match value {
        SqlValue::Integer(number) => ms_from_integer(*number),
        SqlValue::Real(number) => ms_from_integer(*number as i64),
        SqlValue::Text(text) => chrono::DateTime::parse_from_rfc3339(text.trim())
            .ok()
            .map(|stamp| stamp.timestamp_millis()),
        _ => None,
    }
}

/// SQLite stores the thread clock as milliseconds in current Codex builds and
/// as seconds in older rows; `1e11` cleanly separates the two.
fn ms_from_integer(number: i64) -> Option<i64> {
    if number <= 0 {
        return None;
    }
    if number >= 100_000_000_000 {
        Some(number)
    } else {
        number.checked_mul(1_000)
    }
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Slack between a carrier's mtime and the `since` threshold before the
/// carrier is skipped unread.
const SINCE_MTIME_SLACK_MS: i64 = 24 * 60 * 60 * 1_000;

/// `fast_scan` only. A file-per-session carrier is appended to, so its mtime
/// is normally never older than its newest record: one last modified well
/// before `since` is skipped without being opened. That is an assumption about
/// the host's clock and file handling (a carrier restored with an old mtime,
/// or records stamped in the future, break it), which is why the exact path
/// never applies it. An unknown mtime (0) is never skipped.
fn since_mtime_floor(request: &RecentRequest) -> Option<i64> {
    if !request.fast_scan {
        return None;
    }
    request
        .since_ms
        .map(|since| since.saturating_sub(SINCE_MTIME_SLACK_MS))
}

fn wanted(
    session_id: &str,
    session_kind: &str,
    last_activity_ms: Option<i64>,
    request: &RecentRequest,
) -> bool {
    if !request.wants_kind(session_kind) {
        return false;
    }
    if let Some(target) = request.session_id.as_deref() {
        if session_id != target {
            return false;
        }
    }
    if request.excluded(session_id) {
        return false;
    }
    if let Some(since) = request.since_ms {
        match last_activity_ms {
            Some(ms) if ms >= since => {}
            _ => return false,
        }
    }
    true
}

/// Build one tail entry from an already projected index unit. `range_*` and
/// `row_sha256` keep describing the whole projection, so the ref verifies even
/// when the displayed text was cut to `max_chars_per_turn` characters.
fn raw_turn(
    source: &SourceConfig,
    source_kind: &str,
    schema_gen: &str,
    rel_path: &str,
    unit: &crate::recall::fulltext::Unit,
    max_chars: usize,
) -> RawTurn {
    let role = if unit.kind == UNIT_KIND_TEXT {
        unit.role
    } else {
        ROLE_TOOL
    };
    let (text, truncated) = truncate_chars(&unit.text, max_chars);
    RawTurn {
        role,
        timestamp_ms: unit.timestamp_ms,
        text,
        truncated: truncated || unit.truncated,
        additional_refs: Vec::new(),
        reference: HitRef {
            source_id: source.source_id.clone(),
            source_kind: source_kind.to_string(),
            root_sha256: root_sha256(&source.root),
            rel_path: rel_path.to_string(),
            schema_gen: schema_gen.to_string(),
            locator: unit.locator.clone(),
            fingerprint: Fingerprint {
                range_sha256: unit.range_sha256.clone(),
                file_sha256: None,
                row_sha256: unit.row_sha256.clone(),
            },
            text_range: TextRange::new(0, unit.range_len, 0, unit.range_lines),
        },
    }
}

fn mtime_ms(path: &Path) -> Option<i64> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    let duration = modified.duration_since(std::time::UNIX_EPOCH).ok()?;
    i64::try_from(duration.as_millis()).ok()
}

/// The same instant rendered in the host's local zone: seconds precision with a
/// numeric offset (`2026-11-06T23:57:38+08:00`). `offset_minutes` pins the zone
/// for byte-stable tests; `None` uses `chrono::Local`. The process never reads
/// `TZ`/`TZDIR` itself — `recall_boundary` forbids environment reads in this
/// tree, so zone resolution stays inside `chrono`.
fn local_rfc3339(ms: i64, offset_minutes: Option<i32>) -> Option<String> {
    if ms <= 0 {
        return None;
    }
    let utc = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ms)?;
    match offset_minutes {
        Some(minutes) => {
            let offset = chrono::FixedOffset::east_opt(minutes.checked_mul(60)?)?;
            Some(
                utc.with_timezone(&offset)
                    .to_rfc3339_opts(chrono::SecondsFormat::Secs, false),
            )
        }
        None => Some(
            utc.with_timezone(&chrono::Local)
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, false),
        ),
    }
}

struct TailLine {
    /// 0-based ordinal of this line in the whole file.
    index: u64,
    /// Line bytes without the terminating `\n` (a trailing `\r` is stripped,
    /// matching what readback hashes).
    bytes: Vec<u8>,
}

struct TailRead {
    lines: Vec<TailLine>,
    /// The window did not reach the start of the file.
    window_truncated: bool,
}

/// Read the last `max_bytes` bytes of a live JSONL carrier in one bounded
/// window and return only complete, newline-terminated lines with their
/// absolute 0-based ordinals. A half-written final line is dropped, never an
/// error. When `need_indices` is set, the newline count is tracked in the same
/// pass so line indices are exact without ever holding the file in memory;
/// when no refs are requested the read starts at the window offset directly.
fn read_tail_lines(
    path: &Path,
    max_bytes: u64,
    need_indices: bool,
) -> Result<TailRead, std::io::Error> {
    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    let mut window: Vec<u8> = Vec::new();
    let mut newlines: u64 = 0;
    if need_indices {
        let mut chunk = vec![0u8; 64 * 1024];
        loop {
            let read = file.read(&mut chunk)?;
            if read == 0 {
                break;
            }
            newlines += chunk[..read].iter().filter(|byte| **byte == b'\n').count() as u64;
            window.extend_from_slice(&chunk[..read]);
            let keep = max_bytes as usize;
            let hard = keep.saturating_add(chunk.len());
            if window.len() > hard {
                let drop = window.len() - keep;
                window.drain(..drop);
            }
        }
    } else {
        // No refs requested: one bounded backwards read is enough.
        let start = len.saturating_sub(max_bytes);
        file.seek(SeekFrom::Start(start))?;
        window.resize((len - start) as usize, 0);
        file.read_exact(&mut window)?;
    }
    if window.len() > max_bytes as usize {
        let drop = window.len() - max_bytes as usize;
        window.drain(..drop);
    }

    let window_start = len.saturating_sub(window.len() as u64);
    let mut first_is_partial = false;
    if window_start > 0 {
        file.seek(SeekFrom::Start(window_start - 1))?;
        let mut previous = [0u8; 1];
        if file.read(&mut previous)? == 1 {
            first_is_partial = previous[0] != b'\n';
        }
    }

    let mut complete: Vec<&[u8]> = Vec::new();
    let mut start = 0usize;
    for (position, byte) in window.iter().enumerate() {
        if *byte == b'\n' {
            complete.push(&window[start..position]);
            start = position + 1;
        }
    }
    // `window[start..]` is the trailing partial line (or empty): never a record.
    if first_is_partial && !complete.is_empty() {
        complete.remove(0);
    }
    let count = complete.len() as u64;
    let mut lines = Vec::with_capacity(complete.len());
    for (position, mut bytes) in complete.into_iter().enumerate() {
        while matches!(bytes.last(), Some(b'\r')) {
            bytes = &bytes[..bytes.len() - 1];
        }
        lines.push(TailLine {
            index: newlines.saturating_sub(count) + position as u64,
            bytes: bytes.to_vec(),
        });
    }
    Ok(TailRead {
        lines,
        window_truncated: window_start > 0,
    })
}

/// Complete records from the first `max_bytes` bytes (bounded head window).
fn read_head_values(path: &Path, max_bytes: u64) -> Result<Vec<(u64, Value)>, std::io::Error> {
    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    let take = len.min(max_bytes) as usize;
    let mut bytes = vec![0u8; take];
    let read = file.read(&mut bytes)?;
    bytes.truncate(read);
    let truncated = len > take as u64;
    let mut values = Vec::new();
    let mut start = 0usize;
    let mut index = 0u64;
    for (position, byte) in bytes.iter().enumerate() {
        if *byte != b'\n' {
            continue;
        }
        let mut line = &bytes[start..position];
        while matches!(line.last(), Some(b'\r')) {
            line = &line[..line.len() - 1];
        }
        if let Ok(value) = serde_json::from_slice::<Value>(line) {
            values.push((index, value));
        }
        index += 1;
        start = position + 1;
    }
    if !truncated {
        let mut line = &bytes[start..];
        while matches!(line.last(), Some(b'\r')) {
            line = &line[..line.len() - 1];
        }
        if !line.is_empty() {
            if let Ok(value) = serde_json::from_slice::<Value>(line) {
                values.push((index, value));
            }
        }
    }
    Ok(values)
}

/// Drop the oldest tail turns until the emitted text fits `max_bytes`.
fn apply_budget(sessions: &mut [Candidate], max_bytes: usize, warnings: &mut Vec<WarningEntry>) {
    let total: usize = sessions
        .iter()
        .flat_map(|session| session.tail.iter())
        .map(|turn| turn.text.len())
        .sum();
    if total <= max_bytes {
        return;
    }
    let mut order: Vec<(i64, usize, usize)> = Vec::new();
    for (session_index, session) in sessions.iter().enumerate() {
        for (turn_index, turn) in session.tail.iter().enumerate() {
            order.push((turn.timestamp_ms, session_index, turn_index));
        }
    }
    order.sort_by(|left, right| {
        left.0
            .cmp(&right.0)
            .then_with(|| left.1.cmp(&right.1))
            .then_with(|| left.2.cmp(&right.2))
    });
    let mut dropped: Vec<Vec<bool>> = sessions
        .iter()
        .map(|session| vec![false; session.tail.len()])
        .collect();
    let mut remaining = total;
    for (_, session_index, turn_index) in order {
        if remaining <= max_bytes {
            break;
        }
        let turn = &sessions[session_index].tail[turn_index];
        dropped[session_index][turn_index] = true;
        remaining = remaining.saturating_sub(turn.text.len());
    }
    for (session_index, session) in sessions.iter_mut().enumerate() {
        let keep: Vec<bool> = dropped[session_index].clone();
        let dropped_turns = keep.iter().filter(|value| **value).count();
        if dropped_turns == 0 {
            continue;
        }
        let mut turn_index = 0usize;
        session.tail.retain(|_| {
            let keep_turn = !keep[turn_index];
            turn_index += 1;
            keep_turn
        });
        warnings.push(WarningEntry {
            scope: "recent",
            source_id: Some(session.source_id.clone()),
            hit_id: None,
            code: crate::recall::contract::WARN_RECENT_TAIL_BUDGET,
            detail: Some(json!({
                "session_id": session.session_id,
                "dropped_turns": dropped_turns as u64,
                "max_bytes": max_bytes,
            })),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(rfc3339: &str) -> i64 {
        chrono::DateTime::parse_from_rfc3339(rfc3339)
            .expect("timestamp")
            .timestamp_millis()
    }

    #[test]
    fn local_time_keeps_the_instant_and_carries_the_offset() {
        assert_eq!(
            local_rfc3339(ms("2026-11-06T15:57:38Z"), Some(8 * 60)).as_deref(),
            Some("2026-11-06T23:57:38+08:00")
        );
        assert_eq!(
            local_rfc3339(ms("2026-11-06T15:57:38Z"), Some(0)).as_deref(),
            Some("2026-11-06T15:57:38+00:00")
        );
        // Both directions cross the local date boundary.
        assert_eq!(
            local_rfc3339(ms("2026-11-06T20:30:00Z"), Some(8 * 60)).as_deref(),
            Some("2026-11-07T04:30:00+08:00")
        );
        assert_eq!(
            local_rfc3339(ms("2026-11-06T03:00:00Z"), Some(-5 * 60)).as_deref(),
            Some("2026-11-05T22:00:00-05:00")
        );
        // Seconds precision: sub-second input is truncated, not rounded.
        assert_eq!(
            local_rfc3339(ms("2026-11-06T15:57:38.999Z"), Some(8 * 60)).as_deref(),
            Some("2026-11-06T23:57:38+08:00")
        );
    }

    #[test]
    fn local_time_is_none_for_unknown_instants_and_impossible_offsets() {
        assert_eq!(local_rfc3339(0, Some(8 * 60)), None);
        assert_eq!(local_rfc3339(-1, Some(8 * 60)), None);
        assert_eq!(
            local_rfc3339(ms("2026-11-06T15:57:38Z"), Some(24 * 60)),
            None
        );
        assert_eq!(
            local_rfc3339(ms("2026-11-06T15:57:38Z"), Some(i32::MAX)),
            None
        );
    }
}

fn hermes_db_failure(
    source: &SourceConfig,
    reason: &'static str,
    error: &rusqlite::Error,
) -> Collected {
    let code = hermes::db_error_code(error);
    let mut collected = Collected::unavailable(
        &source.source_id,
        code,
        reason,
        format!("hermes db read failed: {error}"),
    );
    if code == codes::DB_LOCKED {
        collected.status = status::DEGRADED;
    }
    collected
}

/// Hermes sessions from the configured `state.db`. Ordering is the sessions
/// table's own `last_activity_at` (seconds → ms at the projection edge); a
/// tail is assembled from the projected rows in reverse insertion order.
fn collect_hermes(source: &SourceConfig, request: &RecentRequest) -> Collected {
    let read_root = Path::new(&source.root);
    let mapped = match crate::recall::read::remap_configured(
        read_root,
        &source.root,
        source.db.as_deref(),
    ) {
        Some(mapped) if !mapped.is_empty() => mapped,
        _ => {
            return Collected::unavailable(
                &source.source_id,
                codes::SOURCE_ROOT_INVALID,
                "db_unconfigured",
                "hermes source has no db path configured".to_string(),
            )
        }
    };
    let path = Path::new(&mapped);
    if !path.is_file() {
        return Collected::unavailable(
            &source.source_id,
            codes::SOURCE_ROOT_INVALID,
            "db_missing",
            format!("hermes db does not exist: {mapped}"),
        );
    }
    let readonly = match rusqlite::Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ) {
        Ok(conn) => conn,
        Err(error) => {
            return hermes_db_failure(source, "db_unavailable", &error);
        }
    };
    let sessions = match hermes::load_sessions(&readonly) {
        Ok(sessions) => sessions,
        Err(error) => {
            return hermes_db_failure(source, "db_query_failed", &error);
        }
    };
    let mut collected = Collected::new(&source.source_id, status::OK);
    let mut accepted = 0usize;
    let mut unreadable = 0u64;
    for session in &sessions {
        if accepted >= request.limit && request.session_id.is_none() {
            break;
        }
        collected.scanned_candidates += 1;
        if !wanted(
            &session.id,
            crate::recall::contract::SESSION_KIND_INTERACTIVE,
            session.last_activity_ms,
            request,
        ) {
            continue;
        }
        let messages = if request.tail_turns == 0 {
            Vec::new()
        } else {
            match hermes::load_messages(&readonly, &session.id) {
                Ok(messages) => messages,
                Err(error) => {
                    if hermes::db_error_code(&error) == codes::DB_LOCKED {
                        return hermes_db_failure(source, "db_query_failed", &error);
                    }
                    unreadable += 1;
                    collected.warnings.push(WarningEntry {
                        scope: "source",
                        source_id: Some(source.source_id.clone()),
                        hit_id: None,
                        code: codes::RECORD_SKIPPED,
                        detail: Some(json!({"session_id": session.id, "error": error.to_string()})),
                    });
                    continue;
                }
            }
        };
        let mut turns: Vec<RawTurn> = Vec::new();
        for message in messages.iter().rev() {
            if turns.len() >= request.tail_turns {
                break;
            }
            if !request.wants_role(message.role) {
                continue;
            }
            let reference = HitRef {
                source_id: source.source_id.clone(),
                source_kind: SOURCE_KIND_HERMES.to_string(),
                root_sha256: root_sha256(&source.root),
                rel_path: source.db.clone().unwrap_or_default().replace('\\', "/"),
                schema_gen: SCHEMA_HERMES_SQLITE_V1.to_string(),
                locator: Locator::codex(
                    &session.id,
                    Some(&message.id.to_string()),
                    message.id as u64,
                    "/content",
                ),
                fingerprint: Fingerprint {
                    range_sha256: sha256_hex(message.content.as_bytes()),
                    file_sha256: None,
                    row_sha256: Some(sha256_hex(
                        format!("{}:{}", message.id, message.content).as_bytes(),
                    )),
                },
                text_range: crate::recall::hash::TextRange::new(
                    0,
                    message.content.len() as u64,
                    0,
                    message.content.lines().count() as u64,
                ),
            };
            let (text, truncated) = truncate_chars(&message.content, request.max_chars_per_turn);
            turns.push(RawTurn {
                role: message.role,
                timestamp_ms: message.timestamp_ms,
                text,
                truncated,
                reference,
                additional_refs: Vec::new(),
            });
        }
        turns.reverse();
        collected.sessions.push(Candidate {
            source_id: source.source_id.clone(),
            session_id: session.id.clone(),
            session_kind: crate::recall::contract::SESSION_KIND_INTERACTIVE,
            title: session.title.clone(),
            cwd: session.cwd.clone(),
            last_activity_ms: session.last_activity_ms,
            tail: turns,
        });
        accepted += 1;
    }
    if unreadable > 0 {
        collected.status = status::PARTIAL;
        collected.code = Some(codes::RECORD_SKIPPED);
        collected.message = Some(format!(
            "hermes messages could not be read (unreadable={unreadable})"
        ));
    }
    collected
}
