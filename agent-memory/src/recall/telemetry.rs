//! Retrieval-event schema, append-only storage, statistics and retention.
//!
//! The four event kinds are semantically distinct (§3.4): `retrieved` is
//! exposure/diagnostics only, `read` requires an explicit CLI/wrapper, and
//! `applied` / `confirmed` require an **external** anchor that is verified
//! mechanically here (the evidence file must exist and contain the `hit_id`
//! string). A caller's self-report is never trusted.
//!
//! Storage is a single append-only SQLite database at the explicitly
//! configured `telemetry.path`. Only `events record` creates it; `prune`,
//! `stats` and `export` never create a file (`stats`/`export` open read-only,
//! `prune` opens `READ_WRITE` without `CREATE`). Nothing in this module may be
//! called from `memory_search` / `memory_read`, from MCP or from replay/report/
//! coverage/scrub: those are read-only and never produce or consume telemetry.
//!
//! Persistence intent (§3.3): events are **not** reconstructible from the
//! sources. Details are retained for `retention_days`, then physically deleted;
//! the per-(hit, kind) aggregate counters are kept, so counts never go
//! backwards. Expired whitelisted query text is blanked by `events prune` in
//! place (the row itself and the counts are unaffected).

use chrono::{Duration as ChronoDuration, NaiveDateTime, Utc};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use serde::Serialize;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration as StdDuration;

/// Version of the event command envelope.
pub const EVENTS_SCHEMA_VERSION: u32 = 1;

/// Version of the on-disk event schema (`events_meta.schema_version`).
pub const EVENTS_STORAGE_SCHEMA_VERSION: u32 = 1;

/// Exit codes for `agent-memory events`.
pub const EVENTS_EXIT_OK: u8 = 0;
pub const EVENTS_EXIT_USAGE: u8 = 2;
pub const EVENTS_EXIT_UNAVAILABLE: u8 = 11;

pub const INVALID_EVENT_CODE: &str = "INVALID_REQUEST";
pub const TELEMETRY_DISABLED_CODE: &str = "TELEMETRY_DISABLED";
pub const EVIDENCE_UNVERIFIED_CODE: &str = "EVIDENCE_UNVERIFIED";
pub const EVENTS_STORAGE_ERROR_CODE: &str = "EVENTS_STORAGE_ERROR";
pub const EVENTS_RECORDED_CODE: &str = "EVENTS_RECORDED";
pub const EVENTS_DUPLICATE_CODE: &str = "EVENTS_DUPLICATE";
pub const EVENTS_PRUNED_CODE: &str = "EVENTS_PRUNED";
pub const EVENTS_STATS_CODE: &str = "EVENTS_STATS";
pub const EVENTS_EXPORT_CODE: &str = "EVENTS_EXPORT";

/// Bounded retention for event **details** (the aggregate counters are kept).
pub const DEFAULT_RETENTION_DAYS: u32 = 365;
pub const MIN_RETENTION_DAYS: u32 = 1;
pub const MAX_RETENTION_DAYS: u32 = 3650;

/// The four event kinds are semantically distinct (§3.4). `retrieved` is
/// exposure/diagnostics only; `read` requires an explicit CLI/wrapper;
/// `applied` and `confirmed` require an external anchor.
pub const EVENT_KINDS: [&str; 4] = ["retrieved", "read", "applied", "confirmed"];

pub const EVENT_KEYS: [&str; 11] = [
    "kind",
    "hit_id",
    "ts",
    "source_id",
    "session_id",
    "task_id",
    "evidence",
    "actor",
    "query",
    "expires_at",
    "idempotency_key",
];

const BUSY_TIMEOUT_MS: u64 = 2_000;

/// Configuration block parsed from a v2 `telemetry` object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TelemetryConfig {
    pub enabled: bool,
    pub path: Option<String>,
    pub query_text_ttl_days: u32,
    pub retention_days: u32,
}

impl Default for TelemetryConfig {
    fn default() -> Self {
        TelemetryConfig {
            enabled: false,
            path: None,
            query_text_ttl_days: 7,
            retention_days: DEFAULT_RETENTION_DAYS,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EventErrorView {
    pub path: String,
    pub reason: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventError {
    pub path: String,
    pub reason: String,
    pub message: String,
}

impl EventError {
    fn new(path: &str, reason: &str, message: impl Into<String>) -> Self {
        EventError {
            path: path.to_string(),
            reason: reason.to_string(),
            message: message.into(),
        }
    }

    pub fn view(&self) -> EventErrorView {
        EventErrorView {
            path: self.path.clone(),
            reason: self.reason.clone(),
            message: self.message.clone(),
        }
    }
}

/// One validated retrieval event. Query text is only ever accompanied by an
/// explicit `expires_at`; storage keeps it until `expires_at`, then `prune`
/// blanks it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetrievalEvent {
    pub kind: &'static str,
    pub hit_id: String,
    pub ts: String,
    pub source_id: Option<String>,
    pub session_id: Option<String>,
    pub task_id: Option<String>,
    pub evidence: Vec<String>,
    pub actor: Option<String>,
    pub query: Option<String>,
    pub expires_at: Option<String>,
    pub idempotency_key: Option<String>,
}

impl RetrievalEvent {
    pub fn parse(value: &Value) -> Result<RetrievalEvent, EventError> {
        let map = value
            .as_object()
            .ok_or_else(|| EventError::new("event", "invalid_type", "event must be an object"))?;
        if let Some(key) = map.keys().find(|key| !EVENT_KEYS.contains(&key.as_str())) {
            return Err(EventError::new(
                key,
                "unknown_key",
                format!("unknown event key: {key}"),
            ));
        }

        let kind = required_string(map, "kind")?;
        let kind = EVENT_KINDS
            .iter()
            .find(|candidate| **candidate == kind)
            .copied()
            .ok_or_else(|| {
                EventError::new(
                    "kind",
                    "unsupported_kind",
                    "kind must be one of retrieved, read, applied, confirmed",
                )
            })?;
        let hit_id = required_string(map, "hit_id")?;
        let ts = required_string(map, "ts")?;
        if !is_rfc3339_utc(&ts) {
            return Err(EventError::new(
                "ts",
                "invalid_timestamp",
                "ts must be an RFC3339 UTC timestamp such as 2026-10-28T09:30:00Z",
            ));
        }
        let source_id = optional_string(map, "source_id")?;
        let session_id = optional_string(map, "session_id")?;
        let task_id = optional_string(map, "task_id")?;
        let actor = optional_string(map, "actor")?;
        let idempotency_key = optional_string(map, "idempotency_key")?;

        let evidence = match map.get("evidence") {
            None => Vec::new(),
            Some(Value::Array(items)) => {
                if items.is_empty() {
                    return Err(EventError::new(
                        "evidence",
                        "empty_evidence",
                        "evidence must contain at least one path when present",
                    ));
                }
                let mut paths = Vec::with_capacity(items.len());
                for (index, item) in items.iter().enumerate() {
                    match item {
                        Value::String(path) if !path.is_empty() => paths.push(path.clone()),
                        Value::String(_) => {
                            return Err(EventError::new(
                                &format!("evidence[{index}]"),
                                "empty_value",
                                "evidence paths must not be empty",
                            ))
                        }
                        _ => {
                            return Err(EventError::new(
                                &format!("evidence[{index}]"),
                                "invalid_type",
                                "evidence entries must be strings",
                            ))
                        }
                    }
                }
                paths
            }
            Some(_) => {
                return Err(EventError::new(
                    "evidence",
                    "invalid_type",
                    "evidence must be an array of strings",
                ))
            }
        };

        let (query, expires_at) = match map.get("query") {
            None => {
                if map.get("expires_at").is_some() {
                    return Err(EventError::new(
                        "expires_at",
                        "expires_at_without_query",
                        "expires_at is only valid together with query",
                    ));
                }
                (None, None)
            }
            Some(Value::String(query)) if !query.is_empty() => {
                let expires_at = required_string(map, "expires_at").map_err(|_| {
                    EventError::new(
                        "expires_at",
                        "required_with_query",
                        "query text requires an explicit expires_at (short-lived whitelist)",
                    )
                })?;
                if !is_rfc3339_utc(&expires_at) {
                    return Err(EventError::new(
                        "expires_at",
                        "invalid_timestamp",
                        "expires_at must be an RFC3339 UTC timestamp",
                    ));
                }
                (Some(query.clone()), Some(expires_at))
            }
            Some(Value::String(_)) => {
                return Err(EventError::new(
                    "query",
                    "empty_value",
                    "query must not be empty",
                ))
            }
            Some(_) => {
                return Err(EventError::new(
                    "query",
                    "invalid_type",
                    "query must be a string",
                ))
            }
        };

        match kind {
            "applied" => {
                if task_id.is_none() {
                    return Err(EventError::new(
                        "task_id",
                        "required_for_applied",
                        "applied requires a task_id",
                    ));
                }
                if evidence.is_empty() {
                    return Err(EventError::new(
                        "evidence",
                        "required_for_applied",
                        "applied requires at least one evidence path",
                    ));
                }
            }
            "confirmed" => {
                if actor.is_none() {
                    return Err(EventError::new(
                        "actor",
                        "required_for_confirmed",
                        "confirmed requires an actor",
                    ));
                }
                if evidence.is_empty() {
                    return Err(EventError::new(
                        "evidence",
                        "required_for_confirmed",
                        "confirmed requires at least one evidence path",
                    ));
                }
            }
            _ => {}
        }

        Ok(RetrievalEvent {
            kind,
            hit_id,
            ts,
            source_id,
            session_id,
            task_id,
            evidence,
            actor,
            query,
            expires_at,
            idempotency_key,
        })
    }

    /// Canonical content hash used when the caller supplies no explicit
    /// `idempotency_key`. Keys are sorted, so equivalent events share a key.
    pub fn content_hash(&self) -> String {
        let mut map: BTreeMap<&str, Value> = BTreeMap::new();
        map.insert("kind", Value::String(self.kind.to_string()));
        map.insert("hit_id", Value::String(self.hit_id.clone()));
        map.insert("ts", Value::String(self.ts.clone()));
        map.insert(
            "source_id",
            self.source_id.clone().map(Value::String).unwrap_or(Value::Null),
        );
        map.insert(
            "session_id",
            self.session_id.clone().map(Value::String).unwrap_or(Value::Null),
        );
        map.insert(
            "task_id",
            self.task_id.clone().map(Value::String).unwrap_or(Value::Null),
        );
        map.insert(
            "evidence",
            Value::Array(self.evidence.iter().cloned().map(Value::String).collect()),
        );
        map.insert(
            "actor",
            self.actor.clone().map(Value::String).unwrap_or(Value::Null),
        );
        map.insert(
            "query",
            self.query.clone().map(Value::String).unwrap_or(Value::Null),
        );
        map.insert(
            "expires_at",
            self.expires_at.clone().map(Value::String).unwrap_or(Value::Null),
        );
        let canonical = serde_json::to_string(&map).expect("canonical event JSON");
        sha256_hex(canonical.as_bytes())
    }

    /// Whitelisted query text is short-lived by construction (§3.4): its
    /// `expires_at` may not lie beyond `ts + query_text_ttl_days`, whatever the
    /// caller asks for. Unparseable timestamps fail closed.
    pub fn query_ttl_error(&self, ttl_days: u32) -> Option<EventError> {
        let expires_at = self.expires_at.as_deref()?;
        let parse = |value: &str| NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H:%M:%SZ").ok();
        let (Some(ts), Some(expires)) = (parse(&self.ts), parse(expires_at)) else {
            return Some(EventError::new(
                "expires_at",
                "invalid_timestamp",
                "ts and expires_at must be real calendar timestamps",
            ));
        };
        if expires > ts + ChronoDuration::days(i64::from(ttl_days)) {
            return Some(EventError::new(
                "expires_at",
                "exceeds_query_ttl",
                format!("expires_at may not exceed ts + query_text_ttl_days ({ttl_days} days)"),
            ));
        }
        None
    }

    /// The external anchor rule (§3.4), checked against the filesystem.
    ///
    /// `retrieved` / `read` never need an anchor. `applied` / `confirmed`
    /// require that **every** evidence path is an existing regular file whose
    /// bytes contain this event's `hit_id`.
    pub fn anchor_error(&self) -> Option<EventError> {
        if self.kind != "applied" && self.kind != "confirmed" {
            return None;
        }
        for (index, path) in self.evidence.iter().enumerate() {
            let field = format!("evidence[{index}]");
            let candidate = Path::new(path);
            if !candidate.exists() {
                return Some(EventError::new(
                    &field,
                    "evidence_missing",
                    format!("evidence path does not exist: {path}"),
                ));
            }
            if !candidate.is_file() {
                return Some(EventError::new(
                    &field,
                    "evidence_not_a_file",
                    format!("evidence path is not a regular file: {path}"),
                ));
            }
            let bytes = match std::fs::read(candidate) {
                Ok(bytes) => bytes,
                Err(error) => {
                    return Some(EventError::new(
                        &field,
                        "evidence_unreadable",
                        format!("evidence path cannot be read: {path}: {error}"),
                    ))
                }
            };
            if !contains_bytes(&bytes, self.hit_id.as_bytes()) {
                return Some(EventError::new(
                    &field,
                    "evidence_without_hit_id",
                    format!(
                        "evidence file does not contain the hit_id {}: {path}",
                        self.hit_id
                    ),
                ));
            }
        }
        None
    }
}

/// Per-hit aggregate counters. `events` is the sum of the four kinds.
#[derive(Debug, Clone, Serialize, PartialEq, Eq, Default)]
pub struct HitStats {
    pub hit_id: String,
    pub retrieved: u64,
    pub read: u64,
    pub applied: u64,
    pub confirmed: u64,
    pub events: u64,
    pub last_event_at: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq, Default)]
pub struct StatsTotals {
    pub hits: u64,
    pub events: u64,
    pub retrieved: u64,
    pub read: u64,
    pub applied: u64,
    pub confirmed: u64,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct EventsResponse {
    pub ok: bool,
    pub events_version: u32,
    pub code: &'static str,
    pub reason: String,
    pub wrote: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<EventErrorView>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duplicate: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pruned_query_text: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pruned_events: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remaining_events: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preserved_count_rows: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exported: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hits: Option<Vec<HitStats>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub totals: Option<StatsTotals>,
}

impl EventsResponse {
    fn base(code: &'static str, reason: impl Into<String>, wrote: bool) -> Self {
        EventsResponse {
            ok: true,
            events_version: EVENTS_SCHEMA_VERSION,
            code,
            reason: reason.into(),
            wrote,
            location: None,
            error: None,
            idempotency_key: None,
            duplicate: None,
            pruned_query_text: None,
            pruned_events: None,
            remaining_events: None,
            preserved_count_rows: None,
            exported: None,
            hits: None,
            totals: None,
        }
    }

    fn failure(code: &'static str, reason: impl Into<String>) -> Self {
        EventsResponse {
            ok: false,
            ..EventsResponse::base(code, reason, false)
        }
    }

    fn disabled() -> Self {
        EventsResponse::failure(
            TELEMETRY_DISABLED_CODE,
            "telemetry.enabled is not true; no event may be written or read",
        )
    }

    fn storage_error(path: Option<&str>, reason: &str, message: impl Into<String>) -> Self {
        let message = message.into();
        let mut response = EventsResponse::failure(EVENTS_STORAGE_ERROR_CODE, message.clone());
        response.location = path.map(|value| value.to_string());
        response.error = Some(EventError {
            path: "storage".to_string(),
            reason: reason.to_string(),
            message,
        }
        .view());
        response
    }
}

struct StorageFailure {
    reason: String,
    message: String,
}

impl StorageFailure {
    fn new(reason: impl Into<String>, message: impl Into<String>) -> Self {
        StorageFailure {
            reason: reason.into(),
            message: message.into(),
        }
    }

    fn sqlite(error: rusqlite::Error) -> Self {
        StorageFailure::new("sqlite_error", error.to_string())
    }
}

struct WriteOutcome {
    idempotency_key: String,
    duplicate: bool,
}

/// Validate one `events record` request and append it to the configured store.
///
/// Exit codes: `2` for an unknown/invalid event or an unverifiable external
/// anchor, `11` for disabled/missing telemetry or a storage failure.
pub fn record_event(config: Option<&TelemetryConfig>, request: &Value) -> (EventsResponse, u8) {
    let event = match RetrievalEvent::parse(request) {
        Ok(event) => event,
        Err(error) => {
            return (
                EventsResponse {
                    error: Some(error.view()),
                    ..EventsResponse::failure(INVALID_EVENT_CODE, error.message.clone())
                },
                EVENTS_EXIT_USAGE,
            )
        }
    };
    let Some(config) = config else {
        return (EventsResponse::disabled(), EVENTS_EXIT_UNAVAILABLE);
    };
    if !config.enabled {
        return (EventsResponse::disabled(), EVENTS_EXIT_UNAVAILABLE);
    }
    let Some(path_text) = config.path.clone() else {
        return (
            EventsResponse::storage_error(
                None,
                "missing_path",
                "telemetry.path is required to store events",
            ),
            EVENTS_EXIT_UNAVAILABLE,
        );
    };

    if let Some(error) = event.query_ttl_error(config.query_text_ttl_days) {
        let mut response = EventsResponse::failure(INVALID_EVENT_CODE, error.message.clone());
        response.location = Some(path_text.clone());
        response.error = Some(error.view());
        return (response, EVENTS_EXIT_USAGE);
    }

    // External anchor: never trust the caller's claim (§3.4).
    if let Some(error) = event.anchor_error() {
        let mut response =
            EventsResponse::failure(EVIDENCE_UNVERIFIED_CODE, error.message.clone());
        response.location = Some(path_text.clone());
        response.error = Some(error.view());
        return (response, EVENTS_EXIT_USAGE);
    }

    match write_event(Path::new(&path_text), &event) {
        Ok(outcome) => {
            let mut response = if outcome.duplicate {
                EventsResponse::base(
                    EVENTS_DUPLICATE_CODE,
                    format!(
                        "duplicate {} event for hit {} was already stored",
                        event.kind, event.hit_id
                    ),
                    false,
                )
            } else {
                EventsResponse::base(
                    EVENTS_RECORDED_CODE,
                    format!("recorded {} event for hit {}", event.kind, event.hit_id),
                    true,
                )
            };
            response.location = Some(path_text);
            response.idempotency_key = Some(outcome.idempotency_key);
            response.duplicate = Some(outcome.duplicate);
            (response, EVENTS_EXIT_OK)
        }
        Err(failure) => (
            EventsResponse::storage_error(Some(&path_text), &failure.reason, failure.message),
            EVENTS_EXIT_UNAVAILABLE,
        ),
    }
}

/// `events stats`: per-hit counters and the most recent event time.
pub fn stats_events(
    config: Option<&TelemetryConfig>,
    hit_id: Option<&str>,
) -> (EventsResponse, u8) {
    let Some(config) = config else {
        return (EventsResponse::disabled(), EVENTS_EXIT_UNAVAILABLE);
    };
    if !config.enabled {
        return (EventsResponse::disabled(), EVENTS_EXIT_UNAVAILABLE);
    }
    let Some(path_text) = config.path.clone() else {
        return (
            EventsResponse::storage_error(
                None,
                "missing_path",
                "telemetry.path is required to read events",
            ),
            EVENTS_EXIT_UNAVAILABLE,
        );
    };
    let path = Path::new(&path_text);

    let mut hits: Vec<HitStats> = Vec::new();
    if path.exists() {
        let conn = match open_read_only(path) {
            Ok(conn) => conn,
            Err(failure) => {
                return (
                    EventsResponse::storage_error(
                        Some(&path_text),
                        &failure.reason,
                        failure.message,
                    ),
                    EVENTS_EXIT_UNAVAILABLE,
                )
            }
        };
        if let Err(failure) = ensure_schema_read(&conn) {
            return (
                EventsResponse::storage_error(Some(&path_text), &failure.reason, failure.message),
                EVENTS_EXIT_UNAVAILABLE,
            );
        }
        match read_counts(&conn, hit_id) {
            Ok(rows) => hits = rows,
            Err(failure) => {
                return (
                    EventsResponse::storage_error(
                        Some(&path_text),
                        &failure.reason,
                        failure.message,
                    ),
                    EVENTS_EXIT_UNAVAILABLE,
                )
            }
        }
    }

    let mut totals = StatsTotals {
        hits: hits.len() as u64,
        ..StatsTotals::default()
    };
    for hit in &hits {
        totals.events += hit.events;
        totals.retrieved += hit.retrieved;
        totals.read += hit.read;
        totals.applied += hit.applied;
        totals.confirmed += hit.confirmed;
    }
    let mut response = EventsResponse::base(
        EVENTS_STATS_CODE,
        format!("{} hit(s) with event counters", totals.hits),
        false,
    );
    response.location = Some(path_text);
    response.hits = Some(hits);
    response.totals = Some(totals);
    (response, EVENTS_EXIT_OK)
}

/// `events export`: one JSON object per event, ordered by insertion (`seq`).
///
/// `query` is never exported unless `include_query` is set, and even then only
/// while the event's `expires_at` is still in the future.
pub fn export_events(
    config: Option<&TelemetryConfig>,
    include_query: bool,
) -> (EventsResponse, Vec<String>, u8) {
    let Some(config) = config else {
        return (EventsResponse::disabled(), Vec::new(), EVENTS_EXIT_UNAVAILABLE);
    };
    if !config.enabled {
        return (EventsResponse::disabled(), Vec::new(), EVENTS_EXIT_UNAVAILABLE);
    }
    let Some(path_text) = config.path.clone() else {
        return (
            EventsResponse::storage_error(
                None,
                "missing_path",
                "telemetry.path is required to read events",
            ),
            Vec::new(),
            EVENTS_EXIT_UNAVAILABLE,
        );
    };
    let path = Path::new(&path_text);
    let now = utc_now_string();

    let mut lines = Vec::new();
    if path.exists() {
        let conn = match open_read_only(path) {
            Ok(conn) => conn,
            Err(failure) => {
                return (
                    EventsResponse::storage_error(
                        Some(&path_text),
                        &failure.reason,
                        failure.message,
                    ),
                    Vec::new(),
                    EVENTS_EXIT_UNAVAILABLE,
                )
            }
        };
        if let Err(failure) = ensure_schema_read(&conn) {
            return (
                EventsResponse::storage_error(Some(&path_text), &failure.reason, failure.message),
                Vec::new(),
                EVENTS_EXIT_UNAVAILABLE,
            );
        }
        match read_event_lines(&conn, include_query, &now) {
            Ok(rows) => lines = rows,
            Err(failure) => {
                return (
                    EventsResponse::storage_error(
                        Some(&path_text),
                        &failure.reason,
                        failure.message,
                    ),
                    Vec::new(),
                    EVENTS_EXIT_UNAVAILABLE,
                )
            }
        }
    }

    let mut response = EventsResponse::base(
        EVENTS_EXPORT_CODE,
        format!("exported {} event(s)", lines.len()),
        false,
    );
    response.location = Some(path_text);
    response.exported = Some(lines.len() as u64);
    (response, lines, EVENTS_EXIT_OK)
}

/// `events prune`:
///
/// 1. blank the query text of every whitelisted event whose `expires_at` is in
///    the past (the row and the counters stay);
/// 2. delete event **details** older than `telemetry.retention_days` (the
///    aggregate counters stay, so counts never decrease).
///
/// Both steps are one transaction and are reported as counts.
pub fn prune_events(config: Option<&TelemetryConfig>) -> (EventsResponse, u8) {
    let Some(config) = config else {
        return (EventsResponse::disabled(), EVENTS_EXIT_UNAVAILABLE);
    };
    if !config.enabled {
        return (EventsResponse::disabled(), EVENTS_EXIT_UNAVAILABLE);
    }
    let Some(path_text) = config.path.clone() else {
        return (
            EventsResponse::storage_error(
                None,
                "missing_path",
                "telemetry.path is required to prune events",
            ),
            EVENTS_EXIT_UNAVAILABLE,
        );
    };
    let path = Path::new(&path_text);
    if !path.exists() {
        // Never create storage from `prune`: nothing exists, nothing to delete.
        return (empty_prune_response(&path_text), EVENTS_EXIT_OK);
    }

    let mut conn = match open_read_write_existing(path) {
        Ok(conn) => conn,
        Err(failure) => {
            return (
                EventsResponse::storage_error(Some(&path_text), &failure.reason, failure.message),
                EVENTS_EXIT_UNAVAILABLE,
            )
        }
    };
    // `prune` never initialises a schema: a file that is not an event store is
    // reported, not converted.
    if let Err(failure) = ensure_schema_read(&conn) {
        return (
            EventsResponse::storage_error(Some(&path_text), &failure.reason, failure.message),
            EVENTS_EXIT_UNAVAILABLE,
        );
    }

    let now = utc_now_string();
    let cutoff = (Utc::now() - ChronoDuration::days(config.retention_days as i64))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();

    let outcome = (|| -> Result<(u64, u64, u64, u64), StorageFailure> {
        let tx = conn.transaction().map_err(StorageFailure::sqlite)?;
        let blanked = tx
            .execute(
                "UPDATE retrieval_events SET query = NULL
                 WHERE query IS NOT NULL AND expires_at IS NOT NULL AND expires_at < ?1",
                params![now],
            )
            .map_err(StorageFailure::sqlite)? as u64;
        let deleted = tx
            .execute("DELETE FROM retrieval_events WHERE ts < ?1", params![cutoff])
            .map_err(StorageFailure::sqlite)? as u64;
        let remaining: i64 = tx
            .query_row("SELECT COUNT(*) FROM retrieval_events", [], |row| row.get(0))
            .map_err(StorageFailure::sqlite)?;
        let preserved: i64 = tx
            .query_row("SELECT COUNT(*) FROM retrieval_event_counts", [], |row| {
                row.get(0)
            })
            .map_err(StorageFailure::sqlite)?;
        tx.commit().map_err(StorageFailure::sqlite)?;
        Ok((blanked, deleted, remaining.max(0) as u64, preserved.max(0) as u64))
    })();

    match outcome {
        Ok((blanked, deleted, remaining, preserved)) => {
            let mut response = EventsResponse::base(
                EVENTS_PRUNED_CODE,
                format!(
                    "pruned {blanked} expired query text(s) and {deleted} event detail(s) older than {} day(s); {preserved} count row(s) preserved",
                    config.retention_days
                ),
                blanked > 0 || deleted > 0,
            );
            response.location = Some(path_text);
            response.pruned_query_text = Some(blanked);
            response.pruned_events = Some(deleted);
            response.remaining_events = Some(remaining);
            response.preserved_count_rows = Some(preserved);
            (response, EVENTS_EXIT_OK)
        }
        Err(failure) => (
            EventsResponse::storage_error(Some(&path_text), &failure.reason, failure.message),
            EVENTS_EXIT_UNAVAILABLE,
        ),
    }
}

fn empty_prune_response(path_text: &str) -> EventsResponse {
    let mut response = EventsResponse::base(
        EVENTS_PRUNED_CODE,
        "no event storage exists; nothing to prune",
        false,
    );
    response.location = Some(path_text.to_string());
    response.pruned_query_text = Some(0);
    response.pruned_events = Some(0);
    response.remaining_events = Some(0);
    response.preserved_count_rows = Some(0);
    response
}

// ---------------------------------------------------------------------------
// Storage
// ---------------------------------------------------------------------------

const SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS events_meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS retrieval_events (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    idempotency_key TEXT NOT NULL UNIQUE,
    kind TEXT NOT NULL,
    hit_id TEXT NOT NULL,
    ts TEXT NOT NULL,
    source_id TEXT,
    session_id TEXT,
    task_id TEXT,
    evidence TEXT NOT NULL,
    actor TEXT,
    query TEXT,
    expires_at TEXT
);
CREATE INDEX IF NOT EXISTS retrieval_events_hit ON retrieval_events(hit_id, kind);
CREATE INDEX IF NOT EXISTS retrieval_events_ts ON retrieval_events(ts);
CREATE TABLE IF NOT EXISTS retrieval_event_counts (
    hit_id TEXT NOT NULL,
    kind TEXT NOT NULL,
    count INTEGER NOT NULL,
    first_ts TEXT NOT NULL,
    last_ts TEXT NOT NULL,
    PRIMARY KEY (hit_id, kind)
);
CREATE TRIGGER IF NOT EXISTS retrieval_events_append_only
BEFORE UPDATE ON retrieval_events
WHEN NOT (
    OLD.seq IS NEW.seq
    AND OLD.idempotency_key IS NEW.idempotency_key
    AND OLD.kind IS NEW.kind
    AND OLD.hit_id IS NEW.hit_id
    AND OLD.ts IS NEW.ts
    AND OLD.source_id IS NEW.source_id
    AND OLD.session_id IS NEW.session_id
    AND OLD.task_id IS NEW.task_id
    AND OLD.evidence IS NEW.evidence
    AND OLD.actor IS NEW.actor
    AND OLD.expires_at IS NEW.expires_at
    AND OLD.query IS NOT NULL
    AND NEW.query IS NULL
)
BEGIN
    SELECT RAISE(ABORT, 'retrieval_events is append-only');
END;
"#;

fn prepare_parent(path: &Path) -> Result<(), StorageFailure> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    if parent.as_os_str().is_empty() {
        return Ok(());
    }
    std::fs::create_dir_all(parent).map_err(|error| {
        StorageFailure::new(
            "path_unwritable",
            format!("cannot create {}: {error}", parent.display()),
        )
    })
}

fn configure(conn: &Connection) -> Result<(), StorageFailure> {
    conn.busy_timeout(StdDuration::from_millis(BUSY_TIMEOUT_MS))
        .map_err(StorageFailure::sqlite)
}

fn open_read_write_create(path: &Path) -> Result<Connection, StorageFailure> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
    )
    .map_err(|error| StorageFailure::new("path_unwritable", error.to_string()))?;
    configure(&conn)?;
    Ok(conn)
}

fn open_read_write_existing(path: &Path) -> Result<Connection, StorageFailure> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE)
        .map_err(|error| StorageFailure::new("path_unwritable", error.to_string()))?;
    configure(&conn)?;
    Ok(conn)
}

fn open_read_only(path: &Path) -> Result<Connection, StorageFailure> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|error| StorageFailure::new("storage_unreadable", error.to_string()))?;
    configure(&conn)?;
    Ok(conn)
}

fn has_meta_table(conn: &Connection) -> Result<bool, StorageFailure> {
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'events_meta'",
            [],
            |row| row.get(0),
        )
        .map_err(StorageFailure::sqlite)?;
    Ok(count > 0)
}

fn read_schema_version(conn: &Connection) -> Result<Option<String>, StorageFailure> {
    conn.query_row(
        "SELECT value FROM events_meta WHERE key = 'schema_version'",
        [],
        |row| row.get(0),
    )
    .optional()
    .map_err(StorageFailure::sqlite)
}

fn ensure_schema_write(conn: &Connection) -> Result<(), StorageFailure> {
    if has_meta_table(conn)? {
        if let Some(version) = read_schema_version(conn)? {
            if version != EVENTS_STORAGE_SCHEMA_VERSION.to_string() {
                return Err(StorageFailure::new(
                    "schema_unsupported",
                    format!(
                        "event storage schema version {version} is not supported (expected {EVENTS_STORAGE_SCHEMA_VERSION})"
                    ),
                ));
            }
        }
    }
    conn.execute_batch(SCHEMA_SQL)
        .map_err(StorageFailure::sqlite)?;
    conn.execute(
        "INSERT INTO events_meta (key, value) VALUES ('schema_version', ?1)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![EVENTS_STORAGE_SCHEMA_VERSION.to_string()],
    )
    .map_err(StorageFailure::sqlite)?;
    Ok(())
}

fn ensure_schema_read(conn: &Connection) -> Result<(), StorageFailure> {
    if !has_meta_table(conn)? {
        return Err(StorageFailure::new(
            "schema_missing",
            "database does not contain the event schema",
        ));
    }
    match read_schema_version(conn)? {
        Some(version) if version == EVENTS_STORAGE_SCHEMA_VERSION.to_string() => Ok(()),
        Some(version) => Err(StorageFailure::new(
            "schema_unsupported",
            format!(
                "event storage schema version {version} is not supported (expected {EVENTS_STORAGE_SCHEMA_VERSION})"
            ),
        )),
        None => Err(StorageFailure::new(
            "schema_missing",
            "event storage has no schema_version marker",
        )),
    }
}

fn write_event(path: &Path, event: &RetrievalEvent) -> Result<WriteOutcome, StorageFailure> {
    prepare_parent(path)?;
    let mut conn = open_read_write_create(path)?;
    ensure_schema_write(&conn)?;

    let idempotency_key = event
        .idempotency_key
        .clone()
        .unwrap_or_else(|| event.content_hash());
    let evidence = serde_json::to_string(&event.evidence).expect("evidence JSON");

    let tx = conn.transaction().map_err(StorageFailure::sqlite)?;
    let inserted = tx
        .execute(
            "INSERT INTO retrieval_events
                (idempotency_key, kind, hit_id, ts, source_id, session_id, task_id, evidence, actor, query, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
             ON CONFLICT(idempotency_key) DO NOTHING",
            params![
                idempotency_key,
                event.kind,
                event.hit_id,
                event.ts,
                event.source_id,
                event.session_id,
                event.task_id,
                evidence,
                event.actor,
                event.query,
                event.expires_at,
            ],
        )
        .map_err(StorageFailure::sqlite)?;
    if inserted == 0 {
        tx.commit().map_err(StorageFailure::sqlite)?;
        return Ok(WriteOutcome {
            idempotency_key,
            duplicate: true,
        });
    }
    tx.execute(
        "INSERT INTO retrieval_event_counts (hit_id, kind, count, first_ts, last_ts)
         VALUES (?1, ?2, 1, ?3, ?3)
         ON CONFLICT(hit_id, kind) DO UPDATE SET
             count = count + 1,
             first_ts = MIN(first_ts, excluded.first_ts),
             last_ts = MAX(last_ts, excluded.last_ts)",
        params![event.hit_id, event.kind, event.ts],
    )
    .map_err(StorageFailure::sqlite)?;
    tx.commit().map_err(StorageFailure::sqlite)?;
    Ok(WriteOutcome {
        idempotency_key,
        duplicate: false,
    })
}

fn read_counts(conn: &Connection, hit_id: Option<&str>) -> Result<Vec<HitStats>, StorageFailure> {
    let mut hits: BTreeMap<String, HitStats> = BTreeMap::new();
    let mut collect = |hit: String, kind: String, count: i64, last_ts: String| {
        let entry = hits.entry(hit.clone()).or_insert_with(|| HitStats {
            hit_id: hit,
            ..HitStats::default()
        });
        let count = count.max(0) as u64;
        match kind.as_str() {
            "retrieved" => entry.retrieved = count,
            "read" => entry.read = count,
            "applied" => entry.applied = count,
            "confirmed" => entry.confirmed = count,
            _ => {}
        }
        entry.events += count;
        if last_ts > entry.last_event_at {
            entry.last_event_at = last_ts;
        }
    };

    if let Some(hit_id) = hit_id {
        let mut stmt = conn
            .prepare(
                "SELECT hit_id, kind, count, last_ts FROM retrieval_event_counts
                 WHERE hit_id = ?1 ORDER BY hit_id, kind",
            )
            .map_err(StorageFailure::sqlite)?;
        let rows = stmt
            .query_map(params![hit_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })
            .map_err(StorageFailure::sqlite)?;
        for row in rows {
            let (hit, kind, count, last_ts) = row.map_err(StorageFailure::sqlite)?;
            collect(hit, kind, count, last_ts);
        }
    } else {
        let mut stmt = conn
            .prepare(
                "SELECT hit_id, kind, count, last_ts FROM retrieval_event_counts
                 ORDER BY hit_id, kind",
            )
            .map_err(StorageFailure::sqlite)?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })
            .map_err(StorageFailure::sqlite)?;
        for row in rows {
            let (hit, kind, count, last_ts) = row.map_err(StorageFailure::sqlite)?;
            collect(hit, kind, count, last_ts);
        }
    }
    Ok(hits.into_values().collect())
}

fn read_event_lines(
    conn: &Connection,
    include_query: bool,
    now: &str,
) -> Result<Vec<String>, StorageFailure> {
    let mut stmt = conn
        .prepare(
            "SELECT seq, kind, hit_id, ts, source_id, session_id, task_id, evidence, actor, query, expires_at, idempotency_key
             FROM retrieval_events ORDER BY seq",
        )
        .map_err(StorageFailure::sqlite)?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, Option<String>>(6)?,
                row.get::<_, String>(7)?,
                row.get::<_, Option<String>>(8)?,
                row.get::<_, Option<String>>(9)?,
                row.get::<_, Option<String>>(10)?,
                row.get::<_, String>(11)?,
            ))
        })
        .map_err(StorageFailure::sqlite)?;

    let mut lines = Vec::new();
    for row in rows {
        let (seq, kind, hit_id, ts, source_id, session_id, task_id, evidence, actor, query, expires_at, idempotency_key) =
            row.map_err(StorageFailure::sqlite)?;
        let evidence = serde_json::from_str::<Value>(&evidence).unwrap_or_else(|_| Value::Array(Vec::new()));
        let mut line = Map::new();
        line.insert("seq".to_string(), Value::Number(seq.into()));
        line.insert("kind".to_string(), Value::String(kind));
        line.insert("hit_id".to_string(), Value::String(hit_id));
        line.insert("ts".to_string(), Value::String(ts));
        line.insert("source_id".to_string(), optional_json(source_id));
        line.insert("session_id".to_string(), optional_json(session_id));
        line.insert("task_id".to_string(), optional_json(task_id));
        line.insert("evidence".to_string(), evidence);
        line.insert("actor".to_string(), optional_json(actor));
        line.insert("expires_at".to_string(), optional_json(expires_at.clone()));
        line.insert(
            "idempotency_key".to_string(),
            Value::String(idempotency_key),
        );
        if include_query {
            let fresh = match (&query, &expires_at) {
                (Some(_), Some(expiry)) => expiry.as_str() > now,
                _ => false,
            };
            line.insert(
                "query".to_string(),
                if fresh {
                    Value::String(query.unwrap_or_default())
                } else {
                    Value::Null
                },
            );
        }
        lines.push(serde_json::to_string(&Value::Object(line)).expect("event line JSON"));
    }
    Ok(lines)
}

fn optional_json(value: Option<String>) -> Value {
    value.map(Value::String).unwrap_or(Value::Null)
}

fn utc_now_string() -> String {
    Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() {
        return true;
    }
    if haystack.len() < needle.len() {
        return false;
    }
    haystack.windows(needle.len()).any(|window| window == needle)
}

fn required_string(map: &Map<String, Value>, key: &str) -> Result<String, EventError> {
    match map.get(key) {
        Some(Value::String(value)) if !value.is_empty() => Ok(value.clone()),
        Some(Value::String(_)) => Err(EventError::new(
            key,
            "empty_value",
            format!("{key} must not be empty"),
        )),
        Some(_) => Err(EventError::new(
            key,
            "invalid_type",
            format!("{key} must be a string"),
        )),
        None => Err(EventError::new(
            key,
            "missing_key",
            format!("{key} is required"),
        )),
    }
}

fn optional_string(map: &Map<String, Value>, key: &str) -> Result<Option<String>, EventError> {
    match map.get(key) {
        None => Ok(None),
        Some(Value::String(value)) if !value.is_empty() => Ok(Some(value.clone())),
        Some(Value::String(_)) => Err(EventError::new(
            key,
            "empty_value",
            format!("{key} must not be empty when present"),
        )),
        Some(_) => Err(EventError::new(
            key,
            "invalid_type",
            format!("{key} must be a string"),
        )),
    }
}

/// Strict `YYYY-MM-DDTHH:MM:SSZ` check (no offsets, no fractional seconds).
pub fn is_rfc3339_utc(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() != 20
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
        || bytes[19] != b'Z'
    {
        return false;
    }
    let digits_ok = [0, 1, 2, 3, 5, 6, 8, 9, 11, 12, 14, 15, 17, 18]
        .iter()
        .all(|index| bytes[*index].is_ascii_digit());
    if !digits_ok {
        return false;
    }
    let number = |start: usize, end: usize| -> u32 { value[start..end].parse::<u32>().unwrap_or(0) };
    let month = number(5, 7);
    let day = number(8, 10);
    let hour = number(11, 13);
    let minute = number(14, 16);
    let second = number(17, 19);
    (1..=12).contains(&month)
        && (1..=31).contains(&day)
        && hour <= 23
        && minute <= 59
        && second <= 59
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn base(kind: &str) -> Value {
        json!({
            "kind": kind,
            "hit_id": "a".repeat(64),
            "ts": "2026-10-28T09:30:00Z"
        })
    }

    fn enabled(path: &Path) -> TelemetryConfig {
        TelemetryConfig {
            enabled: true,
            path: Some(path.to_string_lossy().to_string()),
            query_text_ttl_days: 7,
            retention_days: DEFAULT_RETENTION_DAYS,
        }
    }

    #[test]
    fn event_kinds_are_exactly_four_and_unique() {
        assert_eq!(EVENT_KINDS.len(), 4);
        let unique: std::collections::BTreeSet<&&str> = EVENT_KINDS.iter().collect();
        assert_eq!(unique.len(), 4);
    }

    #[test]
    fn accepts_each_kind_with_its_required_anchor() {
        RetrievalEvent::parse(&base("retrieved")).expect("retrieved");
        RetrievalEvent::parse(&base("read")).expect("read");

        let mut applied = base("applied");
        applied["task_id"] = json!("task-1");
        applied["evidence"] = json!(["docs/plan.md#"]);
        RetrievalEvent::parse(&applied).expect("applied");

        let mut confirmed = base("confirmed");
        confirmed["actor"] = json!("user");
        confirmed["evidence"] = json!(["CHANGELOG.md"]);
        RetrievalEvent::parse(&confirmed).expect("confirmed");
    }

    #[test]
    fn applied_and_confirmed_require_external_anchors() {
        let mut applied = base("applied");
        applied["evidence"] = json!(["plan.md"]);
        let error = RetrievalEvent::parse(&applied).unwrap_err();
        assert_eq!(error.reason, "required_for_applied");

        let mut applied = base("applied");
        applied["task_id"] = json!("t");
        let error = RetrievalEvent::parse(&applied).unwrap_err();
        assert_eq!(error.reason, "required_for_applied");

        let mut confirmed = base("confirmed");
        confirmed["evidence"] = json!(["x"]);
        let error = RetrievalEvent::parse(&confirmed).unwrap_err();
        assert_eq!(error.reason, "required_for_confirmed");

        let mut confirmed = base("confirmed");
        confirmed["actor"] = json!("user");
        let error = RetrievalEvent::parse(&confirmed).unwrap_err();
        assert_eq!(error.reason, "required_for_confirmed");
    }

    #[test]
    fn query_text_requires_an_expiry_and_vice_versa() {
        let mut event = base("retrieved");
        event["query"] = json!("secret needle");
        let error = RetrievalEvent::parse(&event).unwrap_err();
        assert_eq!(error.path, "expires_at");
        assert_eq!(error.reason, "required_with_query");

        event["expires_at"] = json!("2026-11-04T09:30:00Z");
        RetrievalEvent::parse(&event).expect("whitelisted query");

        let mut event = base("read");
        event["expires_at"] = json!("2026-11-04T09:30:00Z");
        let error = RetrievalEvent::parse(&event).unwrap_err();
        assert_eq!(error.reason, "expires_at_without_query");
    }

    #[test]
    fn rejects_unknown_keys_bad_timestamps_and_bad_shapes() {
        let mut event = base("read");
        event["extra"] = json!(1);
        assert_eq!(
            RetrievalEvent::parse(&event).unwrap_err().reason,
            "unknown_key"
        );

        for ts in [
            "2026-10-28",
            "2026-13-28T09:30:00Z",
            "2026-10-28T24:00:00Z",
            "yesterday",
        ] {
            let mut event = base("read");
            event["ts"] = json!(ts);
            let error = RetrievalEvent::parse(&event).unwrap_err();
            assert_eq!(error.reason, "invalid_timestamp", "{ts}");
        }

        let mut event = base("other");
        assert_eq!(
            RetrievalEvent::parse(&event).unwrap_err().reason,
            "unsupported_kind"
        );

        event = base("read");
        event.as_object_mut().unwrap().remove("hit_id");
        assert_eq!(RetrievalEvent::parse(&event).unwrap_err().reason, "missing_key");
    }

    #[test]
    fn content_hash_is_stable_and_key_order_independent() {
        let first = RetrievalEvent::parse(&base("read")).unwrap();
        let reordered = json!({
            "ts": "2026-10-28T09:30:00Z",
            "hit_id": "a".repeat(64),
            "kind": "read"
        });
        let second = RetrievalEvent::parse(&reordered).unwrap();
        assert_eq!(first.content_hash(), second.content_hash());
        assert_eq!(first.content_hash().len(), 64);

        let mut other = base("retrieved");
        other["hit_id"] = json!("b".repeat(64));
        let third = RetrievalEvent::parse(&other).unwrap();
        assert_ne!(first.content_hash(), third.content_hash());
    }

    #[test]
    fn record_event_never_writes_when_disabled_or_invalid() {
        let event = base("retrieved");
        let (response, code) = record_event(None, &event);
        assert_eq!(code, EVENTS_EXIT_UNAVAILABLE);
        assert!(!response.wrote);
        assert_eq!(response.code, TELEMETRY_DISABLED_CODE);

        let mut invalid = base("read");
        invalid["ts"] = json!("nope");
        let (response, code) = record_event(None, &invalid);
        assert_eq!(code, EVENTS_EXIT_USAGE);
        assert_eq!(response.code, INVALID_EVENT_CODE);
        assert!(!response.wrote);

        // `applied` without verifiable evidence never reaches storage.
        let mut unanchored = base("applied");
        unanchored["task_id"] = json!("task-1");
        unanchored["evidence"] = json!(["C:/does/not/exist.md"]);
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state").join("events.sqlite");
        let (response, code) = record_event(Some(&enabled(&path)), &unanchored);
        assert_eq!(code, EVENTS_EXIT_USAGE);
        assert_eq!(response.code, EVIDENCE_UNVERIFIED_CODE);
        assert!(!path.exists());
    }

    #[test]
    fn append_only_trigger_rejects_row_updates_except_query_blanking() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("events.sqlite");
        let config = enabled(&path);
        let event = base("retrieved");
        let (response, code) = record_event(Some(&config), &event);
        assert_eq!(code, EVENTS_EXIT_OK, "{response:?}");
        assert!(response.wrote);

        let conn = Connection::open(&path).unwrap();
        let error = conn
            .execute(
                "UPDATE retrieval_events SET kind = 'applied'",
                [],
            )
            .unwrap_err();
        assert!(
            error.to_string().contains("append-only"),
            "unexpected error: {error}"
        );
        let error = conn
            .execute("UPDATE retrieval_events SET hit_id = 'b'", [])
            .unwrap_err();
        assert!(error.to_string().contains("append-only"));
    }

    #[test]
    fn prune_reports_zero_without_creating_storage() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state").join("events.sqlite");
        let (response, code) = prune_events(Some(&enabled(&path)));
        assert_eq!(code, EVENTS_EXIT_OK);
        assert_eq!(response.pruned_events, Some(0));
        assert!(!path.exists());
        assert!(!path.parent().unwrap().exists());
    }

    #[test]
    fn stats_and_export_do_not_create_storage() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state").join("events.sqlite");
        let (response, code) = stats_events(Some(&enabled(&path)), None);
        assert_eq!(code, EVENTS_EXIT_OK);
        assert_eq!(response.totals.unwrap().events, 0);
        let (response, lines, code) = export_events(Some(&enabled(&path)), true);
        assert_eq!(code, EVENTS_EXIT_OK);
        assert_eq!(response.exported, Some(0));
        assert!(lines.is_empty());
        assert!(!path.exists());
    }
}
