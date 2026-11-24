//! `memory_search`: request validation, source dispatch, merge, pagination,
//! status/error/warning assembly and exit-code derivation.

use crate::recall::budget::Budget;
use crate::recall::config::{RecallConfig, SourceConfig, SourceKind};
use crate::recall::contract::{
    codes, is_incomplete, search_exit_code, status, status_priority, ErrorEntry, RecallError,
    SideEffect, SourceStatus, WarningEntry, CONTRACT_VERSION, RESOLVED_MODE_KB_LEXICAL,
    SEARCH_REQUEST_KEY_GROUP_BY_SESSION, SEARCH_REQUEST_KEY_KINDS, SEARCH_REQUEST_KEY_MODE,
    SEARCH_REQUEST_KEY_ORDER, SEARCH_REQUEST_KEY_RELATED_HOPS, SEARCH_REQUEST_KEY_ROLES,
    SEARCH_REQUEST_KEY_SESSION_KINDS, SEARCH_REQUEST_KEY_SINCE, SEARCH_REQUEST_KEY_UNTIL,
    SEARCH_MODE_AUTO, SEARCH_MODE_EXACT, SEARCH_MODE_HYBRID, SEARCH_MODE_LEGACY,
    SHORT_TERMS_FALLBACK,
};
use crate::recall::fulltext::hybrid::HybridFilters;
use crate::recall::fulltext::{self, query::ExactFilters, Order};
use crate::recall::providers::{
    archive, claude_code, codex, device, docs, dsh, hermes, kb, opencode, DomainSelection,
    SourceHit, SourceOutcome,
};
use crate::recall::refs::{hit_id, HitRef};
use crate::recall::snapshot::StrictSnapshot;
use crate::recall::text::tokenize;
use serde::Serialize;
use serde_json::{Map, Value};
use std::path::Path;

/// Execution options shared by the CLI and the stdio MCP wrapper.
#[derive(Debug, Clone, Copy, Default)]
pub struct SearchOptions {
    /// `--strict-snapshot`: read from a throwaway consistency copy of each
    /// source root instead of the originals.
    pub strict_snapshot: bool,
    /// Stage 7 §C: append the frozen `ask.py` time-decay triple to KB child
    /// calls. Only `eval-recall` sets it; the request contract and the ordinary
    /// `memory_search` path never carry it.
    pub eval_freeze_decay: bool,
}

const SEARCH_KEYS: [&str; 17] = [
    "query",
    "sources",
    "limit",
    "offset",
    "snippet_bytes",
    "budget",
    "session_id",
    "domain",
    SEARCH_REQUEST_KEY_RELATED_HOPS,
    SEARCH_REQUEST_KEY_MODE,
    SEARCH_REQUEST_KEY_SINCE,
    SEARCH_REQUEST_KEY_UNTIL,
    SEARCH_REQUEST_KEY_ROLES,
    SEARCH_REQUEST_KEY_KINDS,
    SEARCH_REQUEST_KEY_SESSION_KINDS,
    SEARCH_REQUEST_KEY_ORDER,
    SEARCH_REQUEST_KEY_GROUP_BY_SESSION,
];
const BUDGET_KEYS: [&str; 3] = [
    "max_bytes_per_source",
    "max_elapsed_ms_per_source",
    "kb_top_k",
];

/// Upper bound for `session_id`: OpenCode `ses_…` ids and Codex UUIDs are far
/// shorter, so anything longer is a caller mistake, not a real id.
const SESSION_ID_MAX_LEN: usize = 200;

/// `mode` selects the retrieval path.
///
/// * `Auto` — hybrid when the source has a readable index, otherwise the
///   pre-L3 scan (resolved per source; a source without a cache is unaffected).
/// * `Exact` — the live-source full-text index only (AND-ed terms).
/// * `Hybrid` — FTS5 bm25 OR conversation vectors, fused with RRF.
/// * `Legacy` — force the pre-L3 scan even when an index exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchMode {
    Auto,
    Exact,
    Hybrid,
    Legacy,
}

impl SearchMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            SearchMode::Auto => SEARCH_MODE_AUTO,
            SearchMode::Exact => SEARCH_MODE_EXACT,
            SearchMode::Hybrid => SEARCH_MODE_HYBRID,
            SearchMode::Legacy => SEARCH_MODE_LEGACY,
        }
    }
}

#[derive(Debug, Clone)]
pub struct SearchRequest {
    pub query: String,
    pub sources: Option<Vec<String>>,
    pub limit: usize,
    pub offset: usize,
    pub snippet_bytes: usize,
    pub budget: Budget,
    /// Exact session-id filter; applied after enumeration, before any body read.
    pub session_id: Option<String>,
    /// Request-layer access domain. `None` falls back to the configuration
    /// domain, then to `default`.
    pub domain: Option<String>,
    /// P4: opt-in bounded `next`-edge expansion of Archive hits (1–2 hops).
    /// `None` (the default) keeps the response identical to P3.
    pub related_hops: Option<usize>,
    /// L3 retrieval mode. `Auto` never emits an `exact` detail block.
    pub mode: SearchMode,
    /// Exact-only filters (RFC3339 UTC bounds, already converted to ms).
    pub since_ms: Option<i64>,
    pub until_ms: Option<i64>,
    pub roles: Option<Vec<String>>,
    pub kinds: Option<Vec<String>>,
    pub session_kinds: Option<Vec<String>>,
    pub order: Order,
    pub group_by_session: bool,
    /// `order` or `group_by_session` was present in the request. The default
    /// mode uses it to resolve to exact (those two only exist there), so an
    /// explicit `order: "newest"` is not silently ignored.
    pub exact_hints: bool,
    /// Any of `since`/`until`/`roles`/`kinds`/`session_kinds` was present. Only
    /// the indexed paths honour them, so the default mode needs an index.
    pub has_index_filters: bool,
}

impl SearchRequest {
    pub fn parse(value: &Value) -> Result<SearchRequest, RecallError> {
        let map = value
            .as_object()
            .ok_or_else(|| invalid_type("request", "object"))?;
        reject_unknown(map, &SEARCH_KEYS, "")?;

        let query = require(map, "query")?;
        let query = match query {
            Value::String(query) if !query.is_empty() => query.clone(),
            Value::String(_) => {
                return Err(RecallError::invalid(
                    "query",
                    "empty_query",
                    "query must not be empty",
                ))
            }
            _ => return Err(invalid_type("query", "string")),
        };

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
                    let id = item
                        .as_str()
                        .ok_or_else(|| invalid_type(&format!("sources[{index}]"), "string"))?;
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
                    ids.push(id.to_string());
                }
                Some(ids)
            }
            Some(_) => return Err(invalid_type("sources", "array")),
        };

        let limit = match map.get("limit") {
            None => 10,
            Some(value) => checked_integer(value, "limit", 1, 100)?,
        };
        let offset = match map.get("offset") {
            None => 0,
            Some(value) => match integer_in_range(value, 0, 10_000) {
                Some(offset) => offset,
                None => {
                    return Err(RecallError::new(
                        codes::INVALID_OFFSET,
                        "offset must be an integer in 0..10000",
                    )
                    .with_detail(serde_json::json!({ "path": "offset" })))
                }
            },
        };
        let snippet_bytes = match map.get("snippet_bytes") {
            None => 400,
            Some(value) => checked_integer(value, "snippet_bytes", 1, 8_192)?,
        };

        let budget = match map.get("budget") {
            None => Budget::default(),
            Some(Value::Object(inner)) => {
                reject_unknown(inner, &BUDGET_KEYS, "budget")?;
                let max_bytes = match inner.get("max_bytes_per_source") {
                    None => Budget::default().max_bytes_per_source as u64,
                    Some(value) => checked_integer(
                        value,
                        "budget.max_bytes_per_source",
                        8_192,
                        1_073_741_824,
                    )? as u64,
                };
                let max_elapsed = match inner.get("max_elapsed_ms_per_source") {
                    None => Budget::default().max_elapsed_ms_per_source,
                    Some(value) => checked_integer(
                        value,
                        "budget.max_elapsed_ms_per_source",
                        4_000,
                        600_000,
                    )? as u64,
                };
                let kb_top_k = match inner.get("kb_top_k") {
                    None => Budget::default().kb_top_k,
                    Some(value) => {
                        checked_integer(value, "budget.kb_top_k", 1, 50)? as u32
                    }
                };
                Budget {
                    max_bytes_per_source: max_bytes,
                    max_elapsed_ms_per_source: max_elapsed,
                    kb_top_k,
                }
            }
            Some(_) => {
                return Err(RecallError::invalid(
                    "budget",
                    "invalid_type",
                    "budget must be an object",
                ))
            }
        };

        let session_id = parse_session_id(map.get("session_id"))?;
        let domain = match map.get("domain") {
            None => None,
            Some(value) => Some(crate::recall::domain::parse_domain_value(value, "domain")?),
        };
        let related_hops = match map.get(SEARCH_REQUEST_KEY_RELATED_HOPS) {
            None => None,
            Some(value) => {
                let hops = checked_integer(
                    value,
                    SEARCH_REQUEST_KEY_RELATED_HOPS,
                    1,
                    crate::recall::graph::RELATED_MAX_HOPS as i64,
                )?;
                Some(hops)
            }
        };

        // L3: `mode` and the exact-only filters. A non-exact request that
        // carries any exact-only key is rejected instead of being silently
        // ignored, so a typo can never look like a working filter.
        let mode = match map.get(SEARCH_REQUEST_KEY_MODE) {
            None => SearchMode::Auto,
            Some(Value::String(value)) if value == SEARCH_MODE_AUTO => SearchMode::Auto,
            Some(Value::String(value)) if value == SEARCH_MODE_EXACT => SearchMode::Exact,
            Some(Value::String(value)) if value == SEARCH_MODE_HYBRID => SearchMode::Hybrid,
            Some(Value::String(value)) if value == SEARCH_MODE_LEGACY => SearchMode::Legacy,
            Some(_) => {
                return Err(RecallError::invalid(
                    SEARCH_REQUEST_KEY_MODE,
                    "unsupported_mode",
                    "mode must be \"auto\", \"exact\", \"hybrid\" or \"legacy\"",
                ))
            }
        };
        // Filters are meaningful on both indexed paths; `order` and
        // `group_by_session` belong to exact mode, whose candidate stream can
        // honour them. The default mode accepts both and resolves per source
        // (`order`/`group_by_session` force exact, filters need an index).
        let filter_keys = [
            SEARCH_REQUEST_KEY_SINCE,
            SEARCH_REQUEST_KEY_UNTIL,
            SEARCH_REQUEST_KEY_ROLES,
            SEARCH_REQUEST_KEY_KINDS,
            SEARCH_REQUEST_KEY_SESSION_KINDS,
        ];
        let exact_only_keys = [SEARCH_REQUEST_KEY_ORDER, SEARCH_REQUEST_KEY_GROUP_BY_SESSION];
        let has_index_filters = filter_keys.iter().any(|key| map.contains_key(*key));
        let exact_hints = exact_only_keys.iter().any(|key| map.contains_key(*key));
        match mode {
            SearchMode::Exact | SearchMode::Auto => {}
            SearchMode::Hybrid => {
                if let Some(key) = exact_only_keys.iter().find(|key| map.contains_key(**key)) {
                    return Err(RecallError::invalid(
                        key,
                        "hybrid_fixed_order",
                        format!(
                            "{key} is not valid with mode \"hybrid\": hybrid orders by \
                             reciprocal-rank fusion and always groups by session"
                        ),
                    ));
                }
            }
            SearchMode::Legacy => {
                let rejected = filter_keys
                    .iter()
                    .chain(exact_only_keys.iter())
                    .find(|key| map.contains_key(**key));
                if let Some(key) = rejected {
                    return Err(RecallError::invalid(
                        key,
                        "exact_only",
                        format!(
                            "{key} is not valid with mode \"legacy\": use the default mode \
                             (auto) or \"exact\""
                        ),
                    ));
                }
            }
        }
        let since_ms = parse_rfc3339_ms(map.get(SEARCH_REQUEST_KEY_SINCE), SEARCH_REQUEST_KEY_SINCE)?;
        let until_ms = parse_rfc3339_ms(map.get(SEARCH_REQUEST_KEY_UNTIL), SEARCH_REQUEST_KEY_UNTIL)?;
        if let (Some(since), Some(until)) = (since_ms, until_ms) {
            if since >= until {
                return Err(RecallError::invalid(
                    SEARCH_REQUEST_KEY_UNTIL,
                    "empty_range",
                    "until must be strictly later than since",
                ));
            }
        }
        let roles = parse_enum_array(
            map.get(SEARCH_REQUEST_KEY_ROLES),
            SEARCH_REQUEST_KEY_ROLES,
            &crate::recall::contract::ALL_ROLES,
        )?;
        let kinds = parse_enum_array(
            map.get(SEARCH_REQUEST_KEY_KINDS),
            SEARCH_REQUEST_KEY_KINDS,
            &crate::recall::contract::ALL_UNIT_KINDS,
        )?;
        let session_kinds = parse_enum_array(
            map.get(SEARCH_REQUEST_KEY_SESSION_KINDS),
            SEARCH_REQUEST_KEY_SESSION_KINDS,
            &crate::recall::contract::ALL_SESSION_KINDS,
        )?;
        let order = match map.get(SEARCH_REQUEST_KEY_ORDER) {
            None => Order::Newest,
            Some(Value::String(value)) => Order::parse(value).ok_or_else(|| {
                RecallError::invalid(
                    SEARCH_REQUEST_KEY_ORDER,
                    "unsupported_order",
                    "order must be \"newest\", \"oldest\" or \"relevance\"",
                )
            })?,
            Some(_) => return Err(invalid_type(SEARCH_REQUEST_KEY_ORDER, "string")),
        };
        let group_by_session = match map.get(SEARCH_REQUEST_KEY_GROUP_BY_SESSION) {
            None => false,
            Some(Value::Bool(value)) => *value,
            Some(_) => return Err(invalid_type(SEARCH_REQUEST_KEY_GROUP_BY_SESSION, "boolean")),
        };

        Ok(SearchRequest {
            query,
            sources,
            limit,
            offset,
            snippet_bytes,
            budget,
            session_id,
            domain,
            related_hops,
            mode,
            since_ms,
            until_ms,
            roles,
            kinds,
            session_kinds,
            order,
            group_by_session,
            exact_hints,
            has_index_filters,
        })
    }
}

pub(crate) fn parse_rfc3339_ms(
    value: Option<&Value>,
    path: &str,
) -> Result<Option<i64>, RecallError> {
    let Some(value) = value else {
        return Ok(None);
    };
    let Value::String(text) = value else {
        return Err(invalid_type(path, "string"));
    };
    chrono::DateTime::parse_from_rfc3339(text)
        .map(|stamp| Some(stamp.timestamp_millis()))
        .map_err(|_| {
            RecallError::invalid(
                path,
                "invalid_rfc3339",
                format!("{path} must be an RFC3339 UTC timestamp such as 2026-10-25T13:25:00Z"),
            )
        })
}

pub(crate) fn parse_enum_array(
    value: Option<&Value>,
    path: &str,
    allowed: &[&str],
) -> Result<Option<Vec<String>>, RecallError> {
    let Some(value) = value else {
        return Ok(None);
    };
    let Value::Array(items) = value else {
        return Err(invalid_type(path, "array"));
    };
    if items.is_empty() {
        return Err(RecallError::invalid(
            path,
            "empty_array",
            format!("{path} must not be empty"),
        ));
    }
    let mut out = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        let Value::String(text) = item else {
            return Err(invalid_type(&format!("{path}[{index}]"), "string"));
        };
        if !allowed.contains(&text.as_str()) {
            return Err(RecallError::invalid(
                &format!("{path}[{index}]"),
                "unsupported_value",
                format!("{path} values must be one of {}", allowed.join(", ")),
            ));
        }
        if out.iter().any(|existing| existing == text) {
            return Err(RecallError::invalid(
                &format!("{path}[{index}]"),
                "duplicate_value",
                format!("{path} must not repeat a value"),
            ));
        }
        out.push(text.clone());
    }
    Ok(Some(out))
}

impl SearchRequest {
    /// `request > configuration > default` for this request.
    pub fn effective_domain(&self, config: &RecallConfig) -> String {
        config.effective_domain(self.domain.as_deref())
    }

    pub fn domain_is_explicit(&self, config: &RecallConfig) -> bool {
        config.domain_is_explicit(self.domain.as_deref())
    }
}

/// `session_id` is matched exactly: the value is never trimmed or folded.
fn parse_session_id(value: Option<&Value>) -> Result<Option<String>, RecallError> {
    let Some(value) = value else {
        return Ok(None);
    };
    let Value::String(id) = value else {
        return Err(invalid_type("session_id", "string"));
    };
    if id.trim().is_empty() {
        return Err(RecallError::invalid(
            "session_id",
            "empty_value",
            "session_id must not be empty",
        ));
    }
    if id.chars().count() > SESSION_ID_MAX_LEN {
        return Err(RecallError::invalid(
            "session_id",
            "too_long",
            format!("session_id must be at most {SESSION_ID_MAX_LEN} characters"),
        ));
    }
    if id.chars().any(char::is_control) {
        return Err(RecallError::invalid(
            "session_id",
            "control_char",
            "session_id must not contain control characters",
        ));
    }
    Ok(Some(id.clone()))
}

#[derive(Debug, Clone, Serialize)]
pub struct SearchResult {
    pub hit_id: String,
    pub source_id: String,
    pub source_kind: String,
    pub title: String,
    pub snippet: String,
    pub snippet_truncated: bool,
    pub score: Option<f64>,
    pub score_kind: &'static str,
    pub rank_basis: &'static str,
    pub source_rank: usize,
    #[serde(rename = "ref")]
    pub reference: HitRef,
    /// P4: bounded `next`-edge neighbours (Archive only, only when the request
    /// asked for `related_hops`). Never part of ranking or gating.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub related: Vec<crate::recall::graph::RelatedItem>,
    /// L3 exact mode only: RFC3339 UTC time of the hit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,
    /// L3 exact mode only: `user|assistant|tool`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// L3 exact mode only: `text|tool_input|tool_output`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// L3 exact mode only: `interactive|worker|unknown`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_kind: Option<String>,
    /// L3 exact mode + `group_by_session`, or the hybrid session dedupe: hits in
    /// the hit's session.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_hits: Option<u64>,
    /// L5b hybrid only: `["lexical"]`, `["semantic"]` or both.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channels: Option<Vec<String>>,
    /// Stage-2 S5: `live` / `archive`, present only when the answering index
    /// carries archive-fallback rows. Absent otherwise, so every pre-S5
    /// response is byte-identical.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin: Option<&'static str>,
    /// Stage-2 S5 rework: RFC3339 UTC time the evidence recorded the session's
    /// deletion (OpenCode delete delta). Present only on archive-fallback hits
    /// of a deleted session; the content is the pre-deletion state.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deleted_at: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Pagination {
    pub sources_incomplete: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SearchResponse {
    pub contract_version: u32,
    pub results: Vec<SearchResult>,
    pub next_offset: Option<usize>,
    pub pagination: Pagination,
    pub source_status: Vec<SourceStatus>,
    pub errors: Vec<ErrorEntry>,
    pub warnings: Vec<WarningEntry>,
    pub side_effects: Vec<SideEffect>,
    #[serde(skip)]
    pub exit_code: i32,
}

impl SearchResponse {
    /// Request-level failure: the frozen envelope with exactly one error, no
    /// source status and no pagination state.
    pub fn request_failure(error: &RecallError) -> Self {
        SearchResponse {
            contract_version: CONTRACT_VERSION,
            results: Vec::new(),
            next_offset: None,
            pagination: Pagination {
                sources_incomplete: Vec::new(),
            },
            source_status: Vec::new(),
            errors: vec![ErrorEntry {
                code: error.code,
                message: error.message.clone(),
                source_id: None,
            }],
            warnings: Vec::new(),
            side_effects: Vec::new(),
            exit_code: error.exit_code(),
        }
    }
}

pub fn memory_search(config: &RecallConfig, request: &SearchRequest) -> SearchResponse {
    memory_search_with(config, request, &SearchOptions::default())
}

pub fn memory_search_with(
    config: &RecallConfig,
    request: &SearchRequest,
    options: &SearchOptions,
) -> SearchResponse {
    let terms = tokenize(&request.query);
    if terms.is_empty() {
        return SearchResponse::request_failure(&RecallError::invalid(
            "query",
            "no_lexical_terms",
            "query contains no lexical term (at least one ASCII run of 2+ characters or one CJK character is required)",
        ));
    }

    let requested: Vec<String> = match &request.sources {
        Some(ids) => ids.clone(),
        None => config.enabled_sources(),
    };

    // One embedding plan per request: the query is embedded at most once inside
    // the Archive provider, and only when a valid semantic generation exists.
    let embedding = crate::recall::semantic::EmbeddingPlan::from_config(config);
    // Sources share one query vector: each source's semantic branch depends on
    // the query alone, so a second embedding would repeat an identical helper
    // run (the live path spawns a Python helper per embedding).
    let semantic_query = crate::recall::semantic::SemanticQuery::new(
        embedding.clone(),
        &request.query,
    );
    let effective_domain = config.effective_domain(request.domain.as_deref());
    let domain = DomainSelection {
        effective: &effective_domain,
        explicit: config.domain_is_explicit(request.domain.as_deref()),
    };
    // Sources are independent and each pays one index read, one vector-cache
    // read and one FTS query: run them side by side. `slots` keeps the outcomes
    // in request order, so the merge below cannot see the difference.
    let mut slots: Vec<Option<SourceOutcome>> = (0..requested.len()).map(|_| None).collect();
    // Borrow once so each spawned worker can capture the slice.
    let terms: &[String] = &terms;
    if requested.len() > 1 {
        std::thread::scope(|scope| {
            let mut handles = Vec::with_capacity(requested.len());
            for (index, source_id) in requested.iter().enumerate() {
                match config.source(source_id) {
                    Some(source) if source.enabled => {
                        let embedding = embedding.as_ref();
                        let semantic_query = &semantic_query;
                        handles.push(scope.spawn(move || {
                            (
                                index,
                                run_source(
                                    config,
                                    source,
                                    terms,
                                    request,
                                    options,
                                    domain,
                                    embedding,
                                    semantic_query,
                                ),
                            )
                        }));
                    }
                    _ => slots[index] = Some(SourceOutcome::unconfigured(source_id)),
                }
            }
            for handle in handles {
                match handle.join() {
                    Ok((index, outcome)) => slots[index] = Some(outcome),
                    // A panicking source is a bug, not a retrieval outcome:
                    // propagate it exactly as the sequential loop would.
                    Err(panic) => std::panic::resume_unwind(panic),
                }
            }
        });
    } else {
        for (index, source_id) in requested.iter().enumerate() {
            match config.source(source_id) {
                Some(source) if source.enabled => {
                    slots[index] = Some(run_source(
                        config,
                        source,
                        terms,
                        request,
                        options,
                        domain,
                        embedding.as_ref(),
                        &semantic_query,
                    ))
                }
                _ => slots[index] = Some(SourceOutcome::unconfigured(source_id)),
            }
        }
    }
    let outcomes: Vec<SourceOutcome> = slots
        .into_iter()
        .map(|slot| slot.expect("every requested source produced an outcome"))
        .collect();

    // Merge order: `source_order`, then request order for ids outside it.
    let mut merge_order: Vec<usize> = (0..outcomes.len()).collect();
    merge_order.sort_by_key(|index| {
        (
            config
                .source_order_index(&requested[*index])
                .unwrap_or(usize::MAX),
            *index,
        )
    });

    let mut merged: Vec<(usize, usize)> = Vec::new(); // (outcome index, rank)
    let mut rank = 0usize;
    loop {
        let mut pushed = false;
        for index in &merge_order {
            if rank < outcomes[*index].hits.len() {
                merged.push((*index, rank));
                pushed = true;
            }
        }
        if !pushed {
            break;
        }
        rank += 1;
    }
    // Exact mode orders by time across sources: timestamps are comparable,
    // while BM25 scores are per-corpus and stay in the round-robin merge.
    if request.mode == SearchMode::Exact && request.order != Order::Relevance {
        let newest_first = request.order == Order::Newest;
        merged.sort_by(|a, b| {
            let left = outcomes[a.0].hits[a.1].sort_time;
            let right = outcomes[b.0].hits[b.1].sort_time;
            let primary = if newest_first {
                right.cmp(&left)
            } else {
                left.cmp(&right)
            };
            primary
                .then_with(|| {
                    config
                        .source_order_index(&requested[a.0])
                        .unwrap_or(usize::MAX)
                        .cmp(
                            &config
                                .source_order_index(&requested[b.0])
                                .unwrap_or(usize::MAX),
                        )
                })
                .then_with(|| a.1.cmp(&b.1))
                .then_with(|| a.0.cmp(&b.0))
        });
    }
    let total = merged.len();
    let page: Vec<(usize, usize)> = merged
        .iter()
        .skip(request.offset)
        .take(request.limit)
        .copied()
        .collect();

    let results: Vec<SearchResult> = page
        .iter()
        .map(|(index, rank)| build_result(&outcomes[*index], *rank))
        .collect();

    // Source status array: status priority, then source_order, then request
    // order for ids that are not in `source_order`.
    let mut status_order: Vec<usize> = (0..outcomes.len()).collect();
    status_order.sort_by_key(|index| {
        let outcome = &outcomes[*index];
        (
            status_priority(outcome.exit_status()),
            config
                .source_order_index(&requested[*index])
                .unwrap_or(usize::MAX),
            *index,
        )
    });
    let source_status: Vec<SourceStatus> = status_order
        .iter()
        .map(|index| {
            let outcome = &outcomes[*index];
            let mut entry = SourceStatus::new(outcome.source_id.clone(), outcome.exit_status());
            if let Some(code) = outcome.effective_code() {
                entry = entry.with_code(code);
            }
            if let Some(detail) = &outcome.detail {
                entry = entry.with_detail(detail.clone());
            }
            entry
        })
        .collect();

    let errors: Vec<ErrorEntry> = status_order
        .iter()
        .filter_map(|index| {
            let outcome = &outcomes[*index];
            let status = outcome.exit_status();
            if !is_incomplete(status) {
                return None;
            }
            Some(ErrorEntry {
                code: outcome.effective_code().unwrap_or(codes::INVALID_REQUEST),
                message: outcome
                    .error_message
                    .clone()
                    .unwrap_or_else(|| format!("source {status}")),
                source_id: Some(outcome.source_id.clone()),
            })
        })
        .collect();

    let sources_incomplete: Vec<String> = source_status
        .iter()
        .filter(|entry| is_incomplete(entry.status))
        .map(|entry| entry.source_id.clone())
        .collect();

    let all_healthy = outcomes
        .iter()
        .all(|outcome| matches!(outcome.exit_status(), status::OK | status::EMPTY));
    let next_offset = if !results.is_empty()
        && all_healthy
        && request.offset + results.len() < total
    {
        Some(request.offset + results.len())
    } else {
        None
    };

    let mut side_effects = Vec::new();
    for outcome in &outcomes {
        side_effects.extend(outcome.side_effects.iter().cloned());
    }

    let mut warnings = config.unsupported_source_warnings("search");
    warnings.extend(config.unsupported_key_warnings("search"));
    for outcome in &outcomes {
        warnings.extend(outcome.warnings.iter().cloned());
    }

    let statuses: Vec<&str> = outcomes
        .iter()
        .map(|outcome| outcome.exit_status())
        .collect();
    let exit_code = search_exit_code(!results.is_empty(), &statuses);

    SearchResponse {
        contract_version: CONTRACT_VERSION,
        results,
        next_offset,
        pagination: Pagination {
            sources_incomplete,
        },
        source_status,
        errors,
        warnings,
        side_effects,
        exit_code,
    }
}

#[allow(clippy::too_many_arguments)]
fn run_source(
    config: &RecallConfig,
    source: &SourceConfig,
    terms: &[String],
    request: &SearchRequest,
    options: &SearchOptions,
    domain: DomainSelection<'_>,
    embedding: Option<&crate::recall::semantic::EmbeddingPlan>,
    semantic_query: &crate::recall::semantic::SemanticQuery,
) -> SourceOutcome {
    let mut outcome = run_source_inner(
        config, source, terms, request, options, domain, embedding, semantic_query,
    );
    if source.kind == SourceKind::Device {
        crate::recall::providers::device::enrich(source, &mut outcome);
    }
    outcome
}

#[allow(clippy::too_many_arguments)]
fn run_source_inner(
    config: &RecallConfig,
    source: &SourceConfig,
    terms: &[String],
    request: &SearchRequest,
    options: &SearchOptions,
    domain: DomainSelection<'_>,
    embedding: Option<&crate::recall::semantic::EmbeddingPlan>,
    semantic_query: &crate::recall::semantic::SemanticQuery,
) -> SourceOutcome {
    let resolution = effective_mode(config, source, request);
    let mode = resolution.mode;
    if source.kind == SourceKind::Hermes && matches!(mode, SearchMode::Exact | SearchMode::Hybrid) {
        if let Some(outcome) = hermes::source_health(source, Path::new(&source.root)) {
            return outcome;
        }
    }
    let mut fallback: Option<fulltext::query::FallbackReason> = None;
    let mut hybrid_fallback: Option<fulltext::query::FallbackReason> = None;
    if mode == SearchMode::Exact && source.kind.is_indexable() && !resolution.index_required {
        let filters = ExactFilters {
            session_id: request.session_id.clone(),
            since_ms: request.since_ms,
            until_ms: request.until_ms,
            roles: request.roles.clone(),
            kinds: request.kinds.clone(),
            session_kinds: request.session_kinds.clone(),
            order: request.order,
            group_by_session: request.group_by_session,
        };
        match fulltext::query::search(
            config,
            source,
            &request.query,
            &filters,
            request.limit,
            request.offset,
            request.snippet_bytes,
        ) {
            fulltext::query::ExactSearch::Hits(outcome) => return *outcome,
            fulltext::query::ExactSearch::Fallback(reason) => fallback = Some(reason),
        }
    }
    if mode == SearchMode::Hybrid && source.kind.is_indexable() && !resolution.index_required {
        // The index is keyed by the configured source identity, exactly like
        // exact mode: `--strict-snapshot` does not change which cache answers.
        // Only the scan fallback reads the snapshot copy.
        let filters = HybridFilters {
            session_id: request.session_id.as_deref(),
            since_ms: request.since_ms,
            until_ms: request.until_ms,
            roles: request.roles.as_deref(),
            kinds: request.kinds.as_deref(),
            session_kinds: request.session_kinds.as_deref(),
        };
        match fulltext::hybrid::search(
            config,
            source,
            &request.query,
            &filters,
            request.limit,
            request.offset,
            request.snippet_bytes,
            semantic_query,
        ) {
            fulltext::hybrid::HybridSearch::Hits(outcome) => return *outcome,
            fulltext::hybrid::HybridSearch::Fallback(reason) => hybrid_fallback = Some(reason),
        }
    }
    let mut outcome = scan_source(
        config, source, terms, request, options, domain, embedding,
    );
    let mut resolved = mode.as_str();
    if resolution.index_required {
        // The default mode needed the index for the filters/order the request
        // carried; this source has none. Answer from the scan, say why.
        annotate_index_required(config, source, &mut outcome, mode);
    } else {
        match mode {
            SearchMode::Exact => match fallback {
                Some(reason) => annotate_exact_fallback(config, source, &mut outcome, reason),
                None => {
                    // S5.1: KB answers an explicit exact request through its
                    // own lexical entry point (`ask.py --bm25` + the Rust-side
                    // whole-term filter). Only the `session_filter_not_
                    // applicable` corner — where no retrieval happens at all —
                    // keeps the previous direct marking.
                    if source.kind == SourceKind::Kb && request.session_id.is_none() {
                        annotate_exact_kb_lexical(&mut outcome);
                        resolved = RESOLVED_MODE_KB_LEXICAL;
                    } else {
                        annotate_exact_direct(&mut outcome);
                    }
                }
            },
            SearchMode::Hybrid => match hybrid_fallback {
                Some(reason) => annotate_hybrid_fallback(config, source, &mut outcome, reason),
                None => annotate_hybrid_direct(&mut outcome),
            },
            _ => {}
        }
    }
    mark_resolved_mode(&mut outcome, resolved);
    outcome
}

/// Record which path answered this source. The indexed paths write it inside
/// their own success blocks; this covers the scan and its fallbacks. `resolved`
/// is the frozen `resolved_mode` value (`exact`/`hybrid`/`legacy`/`kb_lexical`).
fn mark_resolved_mode(outcome: &mut SourceOutcome, resolved: &str) {
    let mut detail = match outcome.detail.take() {
        Some(Value::Object(map)) => map,
        _ => Map::new(),
    };
    detail.insert(
        "resolved_mode".into(),
        Value::String(resolved.to_string()),
    );
    outcome.detail = Some(Value::Object(detail));
}

/// The default mode asked for something only the index can do (filters or
/// ordering) and this source has no readable index: the scan answers, the
/// source is `degraded` with `INDEX_REQUIRED`, and the hint says what to run.
fn annotate_index_required(
    config: &RecallConfig,
    source: &SourceConfig,
    outcome: &mut SourceOutcome,
    mode: SearchMode,
) {
    let mut detail = match outcome.detail.take() {
        Some(Value::Object(map)) => map,
        _ => Map::new(),
    };
    let key = if mode == SearchMode::Exact {
        "exact"
    } else {
        "hybrid"
    };
    let mut block = serde_json::json!({
        "mode": "scan_fallback",
        "reason": crate::recall::contract::INDEX_REQUIRED,
        "hint": "run agent-memory index",
    });
    if mode != SearchMode::Exact {
        block["semantic"] = Value::String("degraded".to_string());
    }
    // A stale or unreadable index is the same remedy but a different cause:
    // keep the precise one next to the generic reason.
    if let Some(cause) = fulltext::open_for_query(config, source).2.detail() {
        block["cause"] = Value::String(cause.to_string());
    }
    detail.insert(key.into(), block);
    detail.insert(
        "reason".into(),
        Value::String(crate::recall::contract::INDEX_REQUIRED.to_string()),
    );
    attach_coverage(&mut detail, config, source);
    outcome.detail = Some(Value::Object(detail));
    outcome.code = Some(crate::recall::contract::INDEX_REQUIRED);
    outcome.status = fulltext::fallback_status(outcome.status, false);
}

/// One source's resolved retrieval path.
#[derive(Debug, Clone, Copy)]
struct Resolution {
    mode: SearchMode,
    /// The default mode needs the index for what was asked (filters or
    /// ordering) and this source has none: answer from the scan, degraded.
    index_required: bool,
}

/// Resolve the mode per source.
///
/// * explicit `exact` / `hybrid` / `legacy` — unchanged.
/// * `auto` with `order`/`group_by_session` — exact (only exact can honour
///   them); without an index the source is `INDEX_REQUIRED`.
/// * `auto` with filters — hybrid; without an index the source is
///   `INDEX_REQUIRED`, because the scan cannot apply the filters.
/// * `auto` with neither — hybrid when the index is readable, otherwise the
///   legacy scan, so an environment without a cache keeps its old behavior.
fn effective_mode(
    config: &RecallConfig,
    source: &SourceConfig,
    request: &SearchRequest,
) -> Resolution {
    if request.mode != SearchMode::Auto {
        return Resolution {
            mode: request.mode,
            index_required: false,
        };
    }
    if !source.kind.is_indexable() {
        // KB keeps ask.py and Archive keeps its generations: no index exists.
        return Resolution {
            mode: SearchMode::Legacy,
            index_required: false,
        };
    }
    let ready = fulltext::index_ready(config, source);
    if request.exact_hints {
        return Resolution {
            mode: SearchMode::Exact,
            index_required: !ready,
        };
    }
    if request.has_index_filters {
        return Resolution {
            mode: SearchMode::Hybrid,
            index_required: !ready,
        };
    }
    Resolution {
        mode: if ready {
            SearchMode::Hybrid
        } else {
            SearchMode::Legacy
        },
        // A docs source has no scan path: without its index the source is
        // degraded with INDEX_REQUIRED, never silently empty.
        index_required: source.kind == SourceKind::Docs && !ready,
    }
}

/// Exact mode could not use this source's index: the scan answered instead.
/// The degradation is explicit (`status = degraded` + `INDEX_*`), never silent.
fn annotate_exact_fallback(
    config: &RecallConfig,
    source: &SourceConfig,
    outcome: &mut SourceOutcome,
    reason: fulltext::query::FallbackReason,
) {
    let short_terms = reason.code == SHORT_TERMS_FALLBACK;
    let mut detail = match outcome.detail.take() {
        Some(Value::Object(map)) => map,
        _ => Map::new(),
    };
    let mut block = serde_json::json!({ "mode": "scan_fallback", "reason": reason.code });
    if let Some(cause) = reason.detail {
        block["cause"] = Value::String(cause);
    }
    detail.insert("exact".into(), block);
    if !detail.contains_key("reason") {
        detail.insert("reason".into(), Value::String(reason.code.to_string()));
    }
    attach_coverage(&mut detail, config, source);
    outcome.detail = Some(Value::Object(detail));
    outcome.status = fulltext::fallback_status(outcome.status, short_terms);
}

/// Hybrid could not use this source's index: the scan answered instead (the
/// index is missing, stale or unreadable). Explicit, never silent.
fn annotate_hybrid_fallback(
    config: &RecallConfig,
    source: &SourceConfig,
    outcome: &mut SourceOutcome,
    reason: fulltext::query::FallbackReason,
) {
    let mut detail = match outcome.detail.take() {
        Some(Value::Object(map)) => map,
        _ => Map::new(),
    };
    let mut block = serde_json::json!({
        "mode": "scan_fallback",
        "semantic": "degraded",
        "reason": reason.code,
    });
    if let Some(cause) = reason.detail {
        block["cause"] = Value::String(cause);
    }
    detail.insert("hybrid".into(), block);
    if !detail.contains_key("reason") {
        detail.insert("reason".into(), Value::String(reason.code.to_string()));
    }
    attach_coverage(&mut detail, config, source);
    outcome.detail = Some(Value::Object(detail));
    outcome.status = fulltext::fallback_status(outcome.status, false);
}

/// KB and Archive keep their own path in hybrid mode; nothing is indexed.
fn annotate_hybrid_direct(outcome: &mut SourceOutcome) {
    let mut detail = match outcome.detail.take() {
        Some(Value::Object(map)) => map,
        _ => Map::new(),
    };
    detail.insert(
        "hybrid".into(),
        serde_json::json!({
            "mode": "direct",
            "semantic": "not_applicable",
            "reason": "not_indexed",
        }),
    );
    outcome.detail = Some(Value::Object(detail));
}

/// Attach the index coverage to a fallback detail: the index is still readable
/// (that is exactly the case a caller needs to see: how old it is).
///
/// The scan providers write their own `detail.coverage`
/// (`scanned_sessions`/`total_sessions`/`session_filter`). A fallback answer came
/// from the scan, so the two blocks are merged instead of one replacing the
/// other; field names do not collide (`rows`/`sessions` are index-side,
/// `total_sessions` is scan-side).
fn attach_coverage(detail: &mut Map<String, Value>, config: &RecallConfig, source: &SourceConfig) {
    let Some(coverage) = fulltext::coverage::source_coverage(config, source) else {
        return;
    };
    match (detail.get_mut("coverage"), coverage) {
        (Some(Value::Object(existing)), Value::Object(index_fields)) => {
            for (key, value) in index_fields {
                existing.insert(key, value);
            }
        }
        (_, coverage) => {
            detail.insert("coverage".into(), coverage);
        }
    }
}

/// Archive keeps its own path in exact mode; nothing is indexed. (KB answers
/// an explicit exact request through `kb_lexical` instead, since S5.1.)
fn annotate_exact_direct(outcome: &mut SourceOutcome) {
    let mut detail = match outcome.detail.take() {
        Some(Value::Object(map)) => map,
        _ => Map::new(),
    };
    detail.insert(
        "exact".into(),
        serde_json::json!({ "mode": "direct", "reason": "not_indexed" }),
    );
    outcome.detail = Some(Value::Object(detail));
}

/// S5.1: an explicit exact request on KB was answered by the KB's own lexical
/// entry point (`ask.py --bm25`) with the Rust-side whole-term filter. This is
/// neither the indexed exact path nor a scan fallback; `kb_lexical` names the
/// resolved path and is never a request `mode` value.
fn annotate_exact_kb_lexical(outcome: &mut SourceOutcome) {
    let mut detail = match outcome.detail.take() {
        Some(Value::Object(map)) => map,
        _ => Map::new(),
    };
    detail.insert(
        "exact".into(),
        serde_json::json!({ "mode": "kb_lexical", "reason": "not_indexed" }),
    );
    outcome.detail = Some(Value::Object(detail));
}

#[allow(clippy::too_many_arguments)]
fn scan_source(
    config: &RecallConfig,
    source: &SourceConfig,
    terms: &[String],
    request: &SearchRequest,
    options: &SearchOptions,
    domain: DomainSelection<'_>,
    embedding: Option<&crate::recall::semantic::EmbeddingPlan>,
) -> SourceOutcome {
    if source.kind == SourceKind::Archive {
        if options.strict_snapshot {
            // Copying an archive root would duplicate every immutable object
            // for a read-only query and still race the collector. Refuse
            // explicitly and copy nothing.
            return SourceOutcome {
                source_id: source.source_id.clone(),
                status: status::UNAVAILABLE,
                code: Some(codes::DB_UNAVAILABLE),
                detail: Some(serde_json::json!({
                    "reason": "strict_snapshot_unsupported_for_archive_root",
                })),
                hits: Vec::new(),
                warnings: Vec::new(),
                error_message: Some(
                    "--strict-snapshot is not supported for archive sources".to_string(),
                ),
                side_effects: Vec::new(),
                violations: Vec::new(),
            };
        }
        return archive::search(&archive::ArchiveRequest {
            source,
            terms,
            query: &request.query,
            budget: &request.budget,
            snippet_bytes: request.snippet_bytes,
            session_id: request.session_id.as_deref(),
            domain,
            read_root: Path::new(&source.root),
            embedding,
            related_hops: request.related_hops,
        });
    }
    if !options.strict_snapshot {
        return dispatch(
            config,
            source,
            terms,
            request,
            Path::new(&source.root),
            true,
            domain,
            embedding,
            options,
        );
    }
    if source.kind == SourceKind::Codex {
        // A Codex home also holds `auth.json`, logs and a live append tree:
        // copying it would clone credentials and blow the snapshot ceilings.
        // Refuse explicitly and copy nothing.
        return SourceOutcome {
            source_id: source.source_id.clone(),
            status: status::UNAVAILABLE,
            code: Some(codes::DB_UNAVAILABLE),
            detail: Some(serde_json::json!({
                "reason": "strict_snapshot_unsupported_for_codex_home",
            })),
            hits: Vec::new(),
            warnings: Vec::new(),
            error_message: Some(
                "--strict-snapshot is not supported for codex sources".to_string(),
            ),
            side_effects: Vec::new(),
            violations: Vec::new(),
        };
    }
    if source.kind == SourceKind::ClaudeCode {
        // Claude Code transcripts are live append files; copying a projects
        // tree would freeze a half-written line into the snapshot.
        return SourceOutcome {
            source_id: source.source_id.clone(),
            status: status::UNAVAILABLE,
            code: Some(codes::DB_UNAVAILABLE),
            detail: Some(serde_json::json!({
                "reason": "strict_snapshot_unsupported_for_claude_code_root",
            })),
            hits: Vec::new(),
            warnings: Vec::new(),
            error_message: Some(
                "--strict-snapshot is not supported for claude_code sources".to_string(),
            ),
            side_effects: Vec::new(),
            violations: Vec::new(),
        };
    }
    if source.kind == SourceKind::Dsh {
        // DSH session logs are live frame-append files; a copy could freeze a
        // torn frame into the snapshot.
        return SourceOutcome {
            source_id: source.source_id.clone(),
            status: status::UNAVAILABLE,
            code: Some(codes::DB_UNAVAILABLE),
            detail: Some(serde_json::json!({
                "reason": "strict_snapshot_unsupported_for_dsh_root",
            })),
            hits: Vec::new(),
            warnings: Vec::new(),
            error_message: Some(
                "--strict-snapshot is not supported for dsh sources".to_string(),
            ),
            side_effects: Vec::new(),
            violations: Vec::new(),
        };
    }
    let snapshot = match StrictSnapshot::create(Path::new(&source.root)) {
        Ok(snapshot) => snapshot,
        Err(reason) => {
            let mut detail = Map::new();
            detail.insert(
                "reason".into(),
                Value::String("strict_snapshot_unavailable".to_string()),
            );
            return SourceOutcome {
                source_id: source.source_id.clone(),
                status: status::UNAVAILABLE,
                code: Some(codes::DB_UNAVAILABLE),
                detail: Some(Value::Object(detail)),
                hits: Vec::new(),
                warnings: Vec::new(),
                error_message: Some(format!(
                    "--strict-snapshot could not copy the source root: {reason}"
                )),
                side_effects: Vec::new(),
                violations: Vec::new(),
            };
        }
    };
    // Originals are never opened: no observed-root window is reported.
    dispatch(
        config,
        source,
        terms,
        request,
        snapshot.path(),
        false,
        domain,
        embedding,
        options,
    )
}

#[allow(clippy::too_many_arguments)]
fn dispatch(
    config: &RecallConfig,
    source: &SourceConfig,
    terms: &[String],
    request: &SearchRequest,
    read_root: &Path,
    observe_root: bool,
    domain: DomainSelection<'_>,
    embedding: Option<&crate::recall::semantic::EmbeddingPlan>,
    options: &SearchOptions,
) -> SourceOutcome {
    match source.kind {
        SourceKind::Archive => archive::search(&archive::ArchiveRequest {
            source,
            terms,
            query: &request.query,
            budget: &request.budget,
            snippet_bytes: request.snippet_bytes,
            session_id: request.session_id.as_deref(),
            domain,
            read_root,
            embedding,
            related_hops: request.related_hops,
        }),
        SourceKind::Opencode => opencode::search_at(
            source,
            terms,
            &request.budget,
            request.snippet_bytes,
            request.session_id.as_deref(),
            read_root,
            observe_root,
        ),
        SourceKind::Codex => codex::search_at(
            source,
            terms,
            &request.budget,
            request.snippet_bytes,
            request.session_id.as_deref(),
            read_root,
        ),
        SourceKind::ClaudeCode => claude_code::search_at(
            source,
            terms,
            &request.budget,
            request.snippet_bytes,
            request.session_id.as_deref(),
            read_root,
        ),
        SourceKind::Dsh => dsh::search_at(
            source,
            terms,
            &request.budget,
            request.snippet_bytes,
            request.session_id.as_deref(),
            read_root,
        ),
        SourceKind::Docs => {
            if request.session_id.is_some() {
                return session_filter_not_applicable(source);
            }
            docs::search(&docs::DocsRequest {
                source,
                terms,
                budget: &request.budget,
                snippet_bytes: request.snippet_bytes,
                read_root,
                observe_root,
            })
        }
        SourceKind::Hermes => hermes::search_at(
            source,
            terms,
            &request.budget,
            request.snippet_bytes,
            hermes::ScanFilters {
                session_id: request.session_id.as_deref(),
                since_ms: request.since_ms,
                until_ms: request.until_ms,
                roles: request.roles.as_deref(),
                kinds: request.kinds.as_deref(),
                session_kinds: request.session_kinds.as_deref(),
            },
            read_root,
            observe_root,
        ),
        SourceKind::Kb | SourceKind::Device => {
            if request.session_id.is_some() {
                return session_filter_not_applicable(source);
            }
            match source.kind {
                SourceKind::Kb => kb::search(&kb::KbRequest {
                    source,
                    interpreter: &config.interpreter.kb,
                    query: &request.query,
                    budget: &request.budget,
                    snippet_bytes: request.snippet_bytes,
                    read_root,
                    observe_root,
                    // S5.1: only an explicit `mode:"exact"` turns into
                    // `--bm25` + whole-term filtering; default/auto, hybrid
                    // and legacy keep the dense/hybrid child call.
                    exact_terms: if request.mode == SearchMode::Exact {
                        Some(terms)
                    } else {
                        None
                    },
                    // Stage 7 §C: frozen time decay belongs to the eval path
                    // only; ordinary searches keep the child's own clock.
                    frozen_decay: if options.eval_freeze_decay {
                        Some(crate::recall::contract::KB_EVAL_FROZEN_DECAY)
                    } else {
                        None
                    },
                }),
                _ => device::search(&device::DeviceRequest {
                    source,
                    terms,
                    budget: &request.budget,
                    snippet_bytes: request.snippet_bytes,
                    read_root,
                    observe_root,
                }),
            }
        }
    }
}

/// A non-session source cannot satisfy a `session_id` filter. It reports
/// `empty` (exit 0) with a machine-readable reason instead of `unsupported`,
/// which would turn every normal filtered query into a degraded exit code.
fn session_filter_not_applicable(source: &SourceConfig) -> SourceOutcome {
    SourceOutcome {
        source_id: source.source_id.clone(),
        status: status::EMPTY,
        code: None,
        detail: Some(serde_json::json!({
            "reason": "session_filter_not_applicable",
        })),
        hits: Vec::new(),
        warnings: Vec::new(),
        error_message: None,
        side_effects: Vec::new(),
        violations: Vec::new(),
    }
}

fn build_result(outcome: &SourceOutcome, rank: usize) -> SearchResult {
    let hit: &SourceHit = &outcome.hits[rank];
    let (timestamp, role, kind, session_kind, session_hits, channels, origin) = match &hit.exact {
        Some(exact) => (
            timestamp_label(exact.timestamp_ms),
            Some(exact.role.clone()),
            Some(exact.kind.clone()),
            Some(exact.session_kind.clone()),
            exact.session_hits,
            exact
                .channels
                .as_ref()
                .map(|channels| channels.iter().map(|c| c.to_string()).collect()),
            exact.origin,
        ),
        None => (None, None, None, None, None, None, None),
    };
    let deleted_at = hit
        .hit
        .locator
        .archive_fallback
        .as_ref()
        .and_then(|archive| archive.deleted_at_ms)
        .and_then(timestamp_label);
    SearchResult {
        hit_id: hit_id(&hit.hit),
        source_id: hit.hit.source_id.clone(),
        source_kind: hit.hit.source_kind.clone(),
        title: hit.title.clone(),
        snippet: hit.snippet.clone(),
        snippet_truncated: hit.snippet_truncated,
        score: hit.score,
        score_kind: hit.score_kind,
        rank_basis: hit.rank_basis,
        source_rank: rank + 1,
        reference: hit.hit.clone(),
        related: hit.related.clone(),
        timestamp,
        role,
        kind,
        session_kind,
        session_hits,
        channels,
        origin,
        deleted_at,
    }
}

/// Milliseconds since the epoch as an RFC3339 UTC string; `None` for "unknown".
fn timestamp_label(ms: i64) -> Option<String> {
    if ms <= 0 {
        return None;
    }
    chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ms)
        .map(|stamp| stamp.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
}

pub(crate) fn reject_unknown(
    map: &Map<String, Value>,
    allowed: &[&str],
    parent: &str,
) -> Result<(), RecallError> {
    if let Some(key) = map.keys().find(|key| !allowed.contains(&key.as_str())) {
        let path = if parent.is_empty() {
            key.clone()
        } else {
            format!("{parent}.{key}")
        };
        return Err(RecallError::invalid(
            &path,
            "unknown_key",
            format!("unknown request key: {path}"),
        ));
    }
    Ok(())
}

fn require<'a>(map: &'a Map<String, Value>, key: &str) -> Result<&'a Value, RecallError> {
    map.get(key)
        .ok_or_else(|| RecallError::invalid(key, "missing_key", format!("{key} is required")))
}

pub(crate) fn invalid_type(path: &str, expected: &str) -> RecallError {
    RecallError::invalid(path, "invalid_type", format!("{path} must be {expected}"))
}

fn integer_in_range(value: &Value, min: i64, max: i64) -> Option<usize> {
    let number = value.as_i64()?;
    if number < min || number > max {
        return None;
    }
    usize::try_from(number).ok()
}

pub(crate) fn checked_integer(
    value: &Value,
    path: &str,
    min: i64,
    max: i64,
) -> Result<usize, RecallError> {
    integer_in_range(value, min, max).ok_or_else(|| {
        RecallError::invalid(
            path,
            "out_of_range",
            format!("{path} must be an integer in {min}..={max}"),
        )
    })
}
