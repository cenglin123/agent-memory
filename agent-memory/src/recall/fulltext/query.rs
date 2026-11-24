//! L3: exact retrieval over the live-source full-text index.
//!
//! A query is split on whitespace (double quotes group a phrase). Every term
//! must match the same unit (AND). Terms of at least [`TRIGRAM_MIN_CHARS`]
//! characters are handed to the FTS5 trigram `MATCH`; shorter terms cannot be
//! indexed by trigram, so they are verified as substrings on the candidates.
//! A query made only of short terms cannot use the index at all and falls back
//! to the existing scan (`short_terms_fallback`).
//!
//! Filters, ordering and `group_by_session` run on the candidate stream, so a
//! source never materialises more than [`CANDIDATE_WINDOW`] rows.

use super::row::{Row, ROW_COLUMNS};

/// Column ordinal just past [`ROW_COLUMNS`] — where a trailing computed score
/// column lands (`bm25(units) AS score`).
const ROW_COLUMNS_COUNT: usize = 20;use super::{
    fts_phrase, match_expression, open_for_query, parse_terms, IndexMeta, Order, Term,
    CANDIDATE_WINDOW,
};
use crate::recall::config::{RecallConfig, SourceConfig, SourceKind};
use crate::recall::contract::{
    status, INDEX_REASON_CORRUPT, SCORE_KIND_BM25, SHORT_TERMS_FALLBACK, UNIT_KIND_TOOL_INPUT,
    UNIT_KIND_TOOL_OUTPUT,
};
use crate::recall::providers::{SourceHit, SourceOutcome};
use crate::recall::text::fold_ascii;
use rusqlite::{Connection, ToSql};
use serde_json::{json, Map, Value};

/// Exact-mode request filters.
#[derive(Debug, Clone)]
pub struct ExactFilters {
    pub session_id: Option<String>,
    /// Inclusive lower bound, milliseconds since the epoch.
    pub since_ms: Option<i64>,
    /// Exclusive upper bound, milliseconds since the epoch.
    pub until_ms: Option<i64>,
    pub roles: Option<Vec<String>>,
    pub kinds: Option<Vec<String>>,
    pub session_kinds: Option<Vec<String>>,
    pub order: Order,
    pub group_by_session: bool,
}

impl Default for ExactFilters {
    fn default() -> Self {
        ExactFilters {
            session_id: None,
            since_ms: None,
            until_ms: None,
            roles: None,
            kinds: None,
            session_kinds: None,
            order: Order::Newest,
            group_by_session: false,
        }
    }
}

impl ExactFilters {
    pub fn new() -> ExactFilters {
        ExactFilters::default()
    }
}

/// One exact query against one source.
pub enum ExactSearch {
    /// The index answered (possibly with zero hits).
    Hits(Box<SourceOutcome>),
    /// The index cannot answer; the caller runs the existing scan and reports
    /// this reason (`INDEX_MISSING` / `INDEX_STALE` / `INDEX_CORRUPT` /
    /// `short_terms_fallback`).
    Fallback(FallbackReason),
}

/// Why exact mode had to scan. `code` is the status reason (`INDEX_*` /
/// `short_terms_fallback`); `detail` names the precise cause.
#[derive(Debug, Clone)]
pub struct FallbackReason {
    pub code: &'static str,
    pub detail: Option<String>,
}

impl FallbackReason {
    fn of(state: &super::IndexState) -> FallbackReason {
        FallbackReason {
            code: state.reason().unwrap_or(INDEX_REASON_CORRUPT),
            detail: state.detail().map(str::to_string),
        }
    }

    fn short_terms() -> FallbackReason {
        FallbackReason {
            code: SHORT_TERMS_FALLBACK,
            detail: None,
        }
    }
}

/// Run an exact query for `source`.
pub fn search(
    config: &RecallConfig,
    source: &SourceConfig,
    query: &str,
    filters: &ExactFilters,
    limit: usize,
    offset: usize,
    snippet_bytes: usize,
) -> ExactSearch {
    let terms = parse_terms(query);
    if terms.is_empty() {
        return ExactSearch::Fallback(FallbackReason::short_terms());
    }
    let Some(expression) = match_expression(&terms) else {
        // Every term is shorter than the trigram minimum: the index cannot
        // narrow anything, so the existing scan answers and the status says so.
        return ExactSearch::Fallback(FallbackReason::short_terms());
    };
    let (conn, _path, state) = open_for_query(config, source);
    if state != super::IndexState::Ready {
        return ExactSearch::Fallback(FallbackReason::of(&state));
    }
    // Archive fallback (stage-2 S5): only a fallback-armed index names `origin`
    // on its hits; every other index answers byte-identically as before.
    let mark_origin = IndexMeta::load(&conn)
        .ok()
        .flatten()
        .map(|meta| meta.fallback.is_some())
        .unwrap_or(false);
    let short: Vec<String> = terms
        .iter()
        .filter(|term| !term.is_long_enough())
        .map(|term| fold_ascii(&term.text))
        .collect();
    match run_query(
        config,
        &conn,
        source,
        &expression,
        &terms,
        &short,
        filters,
        limit,
        offset,
        snippet_bytes,
        mark_origin,
    ) {
        Ok(outcome) => ExactSearch::Hits(Box::new(outcome)),
        Err(error) => {
            // A read failure after opening (corrupt page, missing table, schema
            // drift) must not pretend the index answered.
            ExactSearch::Fallback(FallbackReason {
                code: INDEX_REASON_CORRUPT,
                detail: Some(format!("index_query_failed: {error}")),
            })
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn run_query(
    config: &RecallConfig,
    conn: &Connection,
    source: &SourceConfig,
    expression: &str,
    terms: &[Term],
    short: &[String],
    filters: &ExactFilters,
    limit: usize,
    offset: usize,
    snippet_bytes: usize,
    mark_origin: bool,
) -> rusqlite::Result<SourceOutcome> {
    let needed = offset.saturating_add(limit).saturating_add(1);
    let wants_full_window = !short.is_empty() || filters.group_by_session;
    let window = if wants_full_window {
        CANDIDATE_WINDOW
    } else {
        needed.min(CANDIDATE_WINDOW)
    };

    let mut values: Vec<Box<dyn ToSql>> = vec![Box::new(expression.to_string())];
    let mut where_sql = String::from("units MATCH ?1");
    if let Some(since) = filters.since_ms {
        values.push(Box::new(since));
        where_sql.push_str(&format!(" AND ts >= ?{}", values.len()));
    }
    if let Some(until) = filters.until_ms {
        values.push(Box::new(until));
        where_sql.push_str(&format!(" AND ts < ?{}", values.len()));
    }
    if let Some(session_id) = &filters.session_id {
        values.push(Box::new(session_id.clone()));
        where_sql.push_str(&format!(" AND session_id = ?{}", values.len()));
    }
    for (column, list) in [
        ("role", &filters.roles),
        ("kind", &filters.kinds),
        ("session_kind", &filters.session_kinds),
    ] {
        let Some(list) = list else { continue };
        if list.is_empty() {
            continue;
        }
        let mut placeholders = Vec::new();
        for item in list {
            values.push(Box::new(item.clone()));
            placeholders.push(format!("?{}", values.len()));
        }
        where_sql.push_str(&format!(
            " AND {column} IN ({})",
            placeholders.join(", ")
        ));
    }

    let (order_sql, score_sql) = match filters.order {
        Order::Newest => ("ts DESC, rowid ASC", "NULL"),
        Order::Oldest => ("ts ASC, rowid ASC", "NULL"),
        Order::Relevance => ("bm25(units) ASC, ts DESC, rowid ASC", "bm25(units)"),
    };
    let sql = format!(
        "SELECT {ROW_COLUMNS}, {score_sql} AS score \
         FROM units WHERE {where_sql} ORDER BY {order_sql} LIMIT {window}"
    );

    let mut statement = conn.prepare(&sql)?;
    let params: Vec<&dyn ToSql> = values.iter().map(|value| value.as_ref()).collect();
    let rows = statement.query_map(params.as_slice(), |row| {
        Ok((
            Row::from_query_row(row)?,
            row.get::<_, Option<f64>>(ROW_COLUMNS_COUNT)?,
        ))
    })?;

    let mut kept: Vec<SourceHit> = Vec::new();
    let mut group_counts: std::collections::BTreeMap<String, u64> = Default::default();
    let mut seen_sessions: std::collections::BTreeSet<String> = Default::default();
    let mut considered = 0u64;
    let mut dropped_short = 0u64;
    let mut window_clipped = false;
    for row in rows {
        let (row, score) = row?;
        considered += 1;
        let unit_text = match row.kind.as_str() {
            UNIT_KIND_TOOL_INPUT => row.tool_input.clone(),
            UNIT_KIND_TOOL_OUTPUT => row.tool_output.clone(),
            _ => row.text.clone(),
        };
        let folded = fold_ascii(&unit_text);
        if !short.iter().all(|term| folded.contains(term.as_str())) {
            dropped_short += 1;
            continue;
        }
        if filters.group_by_session {
            *group_counts.entry(row.session_id.clone()).or_insert(0) += 1;
            if !seen_sessions.insert(row.session_id.clone()) {
                continue;
            }
        }
        let mut hit = row.into_hit(source, terms, snippet_bytes, mark_origin);
        if filters.order == Order::Relevance {
            hit.score = score;
            hit.score_kind = SCORE_KIND_BM25;
        }
        kept.push(hit);
        if kept.len() >= needed {
            window_clipped = true;
            break;
        }
    }

    if filters.group_by_session {
        for hit in kept.iter_mut() {
            let session_id = hit
                .hit
                .locator
                .session_id
                .clone()
                .unwrap_or_default();
            let count = group_counts.get(&session_id).copied().unwrap_or(0);
            if let Some(exact) = hit.exact.as_mut() {
                exact.session_hits = Some(count);
            }
        }
    }

    let mut detail = Map::new();
    detail.insert(
        "mode".into(),
        json!(if filters.group_by_session {
            "index_grouped"
        } else {
            "index"
        }),
    );
    detail.insert("order".into(), json!(filters.order.as_str()));
    detail.insert("candidates".into(), json!(considered));
    detail.insert("group_by_session".into(), json!(filters.group_by_session));
    detail.insert("short_terms".into(), json!(short.len()));
    if dropped_short > 0 {
        detail.insert("dropped_short_terms".into(), json!(dropped_short));
    }
    if window_clipped {
        detail.insert("candidate_window".into(), json!(window));
    }
    // Records dropped at build time because they exceeded the per-record
    // ceiling: the index is not a superset of the lexical scan for those.
    if let Ok(Some(meta)) = IndexMeta::load(conn) {
        if meta.skipped_oversize > 0 {
            detail.insert("skipped_oversize".into(), json!(meta.skipped_oversize));
        }
    }

    let status_value = if kept.is_empty() {
        status::EMPTY
    } else {
        status::OK
    };
    // Coverage/freshness of this index, next to the exact block: a caller that
    // gets zero hits needs to know whether the index is behind the source.
    let mut outer = Map::new();
    outer.insert("exact".into(), Value::Object(detail));
    outer.insert("resolved_mode".into(), json!("exact"));
    if let Some(coverage) = super::coverage::source_coverage(config, source) {
        outer.insert("coverage".into(), coverage);
    }
    Ok(SourceOutcome {
        source_id: source.source_id.clone(),
        status: status_value,
        code: None,
        detail: Some(Value::Object(outer)),
        hits: kept,
        warnings: Vec::new(),
        error_message: None,
        side_effects: Vec::new(),
        violations: Vec::new(),
    })
}

/// The schema generation a ref of this source must carry.
pub fn schema_gen_for(source: &SourceConfig) -> &'static str {
    match source.kind {
        SourceKind::Opencode => crate::recall::contract::SCHEMA_SQLITE_V2,
        SourceKind::Codex => crate::recall::contract::SCHEMA_CODEX_ROLLOUT_V1,
        SourceKind::ClaudeCode => crate::recall::contract::SCHEMA_CLAUDE_JSONL_V1,
        SourceKind::Dsh => crate::recall::contract::SCHEMA_DSH_JSONL,
        SourceKind::Hermes => crate::recall::contract::SCHEMA_HERMES_SQLITE_V1,
        SourceKind::Device => crate::recall::contract::SCHEMA_MD_V1,
        SourceKind::Docs => crate::recall::contract::SCHEMA_DOCS_MD_V1,
        SourceKind::Kb | SourceKind::Archive => crate::recall::contract::SCHEMA_MD_V1,
    }
}

/// Escape a query term for an FTS5 literal (exposed for tests).
pub fn quote(term: &str) -> String {
    fts_phrase(term)
}
