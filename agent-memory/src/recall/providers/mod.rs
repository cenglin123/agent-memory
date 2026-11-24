//! Providers orchestrate port primitives into source outcomes.
//!
//! `providers -> port` is the only allowed direction. Mapping `PortError` to
//! `DB_*` / `SCHEMA_UNSUPPORTED`, budget progress and `coverage` live here.

use crate::recall::contract::{
    codes, status, status_for_code, SideEffect, SourceStatus,
};
use crate::recall::port::{PortError, SessionMeta};
use crate::recall::refs::HitRef;
use serde_json::{Map, Value};

pub mod archive;
pub mod claude_code;
pub mod codex;
pub mod device;
pub mod docs;
pub mod dsh;
pub mod hermes;
pub mod kb;
pub mod opencode;

pub use archive::{ArchiveRequest, DomainSelection};

/// Indexed-path hit metadata (`mode = "exact"` and `mode = "hybrid"`). Absent
/// on every other path, so a legacy response keeps exactly its previous key set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExactMeta {
    pub timestamp_ms: i64,
    pub role: String,
    pub kind: String,
    pub session_kind: String,
    /// Number of hits in the hit's session (`group_by_session`, or the hybrid
    /// session dedupe).
    pub session_hits: Option<u64>,
    /// Hybrid only: `lexical`, `semantic`, or both. `None` on the exact path.
    pub channels: Option<Vec<&'static str>>,
    /// Stage-2 S5: `live` / `archive`, present only when the answering index
    /// carries archive-fallback rows (meta has a fallback identity). `None`
    /// keeps every pre-S5 response byte-identical.
    pub origin: Option<&'static str>,
}

/// Map a typed port failure to the frozen response code. This is the single
/// mapping authority (§4 / P1.1); `port/` never produces `DB_*` codes.
pub fn port_code(error: &PortError) -> &'static str {
    match error {
        PortError::NotFound(_) => codes::DB_UNAVAILABLE,
        PortError::Locked(_) => codes::DB_LOCKED,
        PortError::Corrupt(_) => codes::DB_CORRUPT,
        PortError::Schema { .. } => codes::SCHEMA_UNSUPPORTED,
        PortError::Io(_) => codes::DB_UNAVAILABLE,
    }
}

/// Response `detail` for a port failure. Only frozen `detail` keys are used;
/// missing tables/columns are folded into `reason`.
pub fn port_detail(error: &PortError) -> Option<Value> {
    match error {
        PortError::Schema { missing } => {
            let mut detail = Map::new();
            detail.insert("reason".into(), Value::String(missing.join(",")));
            Some(Value::Object(detail))
        }
        _ => None,
    }
}

/// One ranked hit produced by a source adapter.
#[derive(Debug, Clone)]
pub struct SourceHit {
    pub hit: HitRef,
    pub title: String,
    pub snippet: String,
    pub snippet_truncated: bool,
    /// Native score, or `None` for lexical sources.
    pub score: Option<f64>,
    pub score_kind: &'static str,
    pub rank_basis: &'static str,
    pub distinct_terms: usize,
    pub first_byte: usize,
    pub sort_time: i64,
    pub sort_id: String,
    /// P4: bounded `next`-edge neighbours of an Archive hit. Always empty for
    /// live sources and when the request did not ask for `related_hops`; it
    /// never participates in ranking or gating.
    pub related: Vec<crate::recall::graph::RelatedItem>,
    /// Exact-mode extras; `None` on every other path.
    pub exact: Option<ExactMeta>,
}

/// Per-source outcome of one search call.
#[derive(Debug, Clone)]
pub struct SourceOutcome {
    pub source_id: String,
    pub status: &'static str,
    pub code: Option<&'static str>,
    pub detail: Option<Value>,
    pub hits: Vec<SourceHit>,
    pub warnings: Vec<crate::recall::contract::WarningEntry>,
    pub error_message: Option<String>,
    pub side_effects: Vec<SideEffect>,
    pub violations: Vec<String>,
}

impl SourceOutcome {
    pub fn unconfigured(source_id: &str) -> Self {
        SourceOutcome {
            source_id: source_id.to_string(),
            status: status::UNCONFIGURED,
            code: Some(codes::SOURCE_NOT_CONFIGURED),
            detail: None,
            hits: Vec::new(),
            warnings: Vec::new(),
            error_message: Some("source is not configured or is disabled".into()),
            side_effects: Vec::new(),
            violations: Vec::new(),
        }
    }

    pub fn from_port_error(
        source_id: &str,
        error: &PortError,
        side_effects: Vec<SideEffect>,
        violations: Vec<String>,
    ) -> Self {
        let code = port_code(error);
        SourceOutcome {
            source_id: source_id.to_string(),
            status: status_for_code(code),
            code: Some(code),
            detail: port_detail(error),
            hits: Vec::new(),
            warnings: Vec::new(),
            error_message: Some(error.message()),
            side_effects,
            violations,
        }
    }

    pub fn source_status(&self) -> SourceStatus {
        let mut status = SourceStatus::new(self.source_id.clone(), self.exit_status());
        if let Some(code) = self.effective_code() {
            status = status.with_code(code);
        }
        if let Some(detail) = &self.detail {
            status = status.with_detail(detail.clone());
        }
        status
    }

    pub fn exit_status(&self) -> &'static str {
        if !self.violations.is_empty() {
            status::ERROR
        } else {
            self.status
        }
    }

    pub fn effective_code(&self) -> Option<&'static str> {
        if !self.violations.is_empty() {
            Some(codes::SIDE_EFFECT_VIOLATION)
        } else {
            self.code
        }
    }
}

/// Source-internal ranking (§3.5): distinct terms desc, first hit offset asc,
/// time desc, then id/path asc. Deterministic for a fixed corpus.
pub fn rank_hits(hits: &mut [SourceHit]) {
    hits.sort_by(|a, b| {
        b.distinct_terms
            .cmp(&a.distinct_terms)
            .then_with(|| a.first_byte.cmp(&b.first_byte))
            .then_with(|| b.sort_time.cmp(&a.sort_time))
            .then_with(|| a.sort_id.cmp(&b.sort_id))
    });
}

/// Searchable sessions with the same predicate used for `total_sessions`.
pub fn searchable_sessions(sessions: &[SessionMeta]) -> Vec<&SessionMeta> {
    sessions.iter().filter(|session| session.is_searchable()).collect()
}
