//! L5b: hybrid retrieval over one live source.
//!
//! Two branches answer the same query and are fused with reciprocal-rank
//! fusion (`k = 60`):
//!
//! * **lexical** — FTS5 `bm25` over every indexed channel. Query terms of at
//!   least [`super::TRIGRAM_MIN_CHARS`] characters are OR-ed, so a natural
//!   question does not require every word to occur in one unit; shorter terms
//!   cannot use the trigram index and do not enter `MATCH`.
//! * **semantic** — cosine between the query vector and the conversation units
//!   of [`super::vectors`]. Filters (source, `session_id`, `since`/`until`,
//!   `roles`, `kinds`, `session_kinds`) are applied in SQL before a candidate is
//!   produced, so a filtered-out session can never surface as a semantic hit.
//!
//! When the semantic branch is available the archive-relevance-v4 acceptance
//! gate is applied unchanged: a candidate survives on a strong cosine, on
//! sufficient lexical coverage, or on same-turn double support. Without the
//! semantic branch the gate is skipped and the lexical branch answers alone
//! (`detail.hybrid.semantic = "degraded"`).

use super::query::FallbackReason;
use super::row::{Row, ROW_COLUMNS};
use super::vectors::{Similarity, VectorCache};
use super::{open_for_query, parse_terms, IndexMeta, IndexState, Term};
use crate::recall::config::{RecallConfig, SourceConfig, SourceKind};
use crate::recall::contract::{
    status, INDEX_REASON_CORRUPT, RANK_BASIS_RRF, SCORE_KIND_RRF, UNIT_KIND_TEXT,
};
use crate::recall::providers::{SourceHit, SourceOutcome};
use crate::recall::semantic::embed::{SemanticQuery, SemanticState};
use crate::recall::semantic::index::{rrf_score, RRF_K};
use crate::recall::semantic::relevance::{
    ARCHIVE_RELEVANCE_VERSION, LEXICAL_ACCEPT_COVERAGE, LEXICAL_ACCEPT_DISTINCT,
    SEMANTIC_ACCEPT_SIMILARITY, SEMANTIC_MAX_CANDIDATES, SEMANTIC_SUPPORT_SIMILARITY,
    SEMANTIC_SUPPORT_UNITS, SEMANTIC_TURN_MAX_SIMILARITY,
};
use crate::recall::text::fold_ascii;
use rusqlite::{Connection, ToSql};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;

/// Candidates kept per branch before fusion. RRF only needs the head of each
/// list; the window bounds both the `MATCH` set and the session dedupe.
pub const HYBRID_BRANCH_WINDOW: usize = 200;

/// Request filters. Every one is applied before a candidate is produced.
#[derive(Debug, Clone, Default)]
pub struct HybridFilters<'a> {
    pub session_id: Option<&'a str>,
    pub since_ms: Option<i64>,
    pub until_ms: Option<i64>,
    pub roles: Option<&'a [String]>,
    pub kinds: Option<&'a [String]>,
    pub session_kinds: Option<&'a [String]>,
}

pub enum HybridSearch {
    Hits(Box<SourceOutcome>),
    /// The index cannot answer at all; the caller runs the legacy scan and
    /// reports this reason.
    Fallback(FallbackReason),
}

fn placeholder(values: &[Box<dyn ToSql>]) -> String {
    format!("?{}", values.len())
}

/// `AND`-joined filters over a table whose column prefix is `prefix`.
fn filter_sql(
    values: &mut Vec<Box<dyn ToSql>>,
    filters: &HybridFilters<'_>,
    prefix: &str,
    with_kind: bool,
) -> String {
    let mut sql = String::new();
    if let Some(since) = filters.since_ms {
        values.push(Box::new(since));
        sql.push_str(&format!(" AND {prefix}ts >= {}", placeholder(values)));
    }
    if let Some(until) = filters.until_ms {
        values.push(Box::new(until));
        sql.push_str(&format!(" AND {prefix}ts < {}", placeholder(values)));
    }
    if let Some(session_id) = filters.session_id {
        values.push(Box::new(session_id.to_string()));
        sql.push_str(&format!(
            " AND {prefix}session_id = {}",
            placeholder(values)
        ));
    }
    let mut lists: Vec<(&str, Option<&[String]>)> = vec![
        ("role", filters.roles),
        ("session_kind", filters.session_kinds),
    ];
    if with_kind {
        lists.push(("kind", filters.kinds));
    }
    for (column, list) in lists {
        let Some(list) = list else { continue };
        if list.is_empty() {
            continue;
        }
        let mut placeholders = Vec::new();
        for item in list {
            values.push(Box::new(item.clone()));
            placeholders.push(placeholder(values));
        }
        sql.push_str(&format!(
            " AND {prefix}{column} IN ({})",
            placeholders.join(", ")
        ));
    }
    sql
}

fn list_contains(list: Option<&[String]>, value: &str) -> bool {
    list.map(|items| items.iter().any(|item| item == value))
        .unwrap_or(false)
}

/// Maximum number of `MATCH` parts one query may carry. A long CJK question
/// produces one part per trigram; the cap keeps the expression and the `OR`
/// cost bounded.
pub const MAX_LEXICAL_PARTS: usize = 48;

/// Character-level CJK test: Han (all planes), kana, Hangul. Punctuation and
/// everything else is non-CJK, so it separates runs instead of joining them.
pub fn is_cjk(character: char) -> bool {
    // CJK punctuation (U+3000..=U+303F) is deliberately excluded: it separates
    // runs instead of extending them.
    matches!(character as u32,
        0x3400..=0x4DBF      // CJK unified ideographs extension A
        | 0x4E00..=0x9FFF    // CJK unified ideographs
        | 0xF900..=0xFAFF    // CJK compatibility ideographs
        | 0x3040..=0x309F    // hiragana
        | 0x30A0..=0x30FF    // katakana
        | 0x31F0..=0x31FF    // katakana phonetic extensions
        | 0xFF66..=0xFF9D    // halfwidth katakana
        | 0x1100..=0x11FF    // hangul jamo
        | 0x3130..=0x318F    // hangul compatibility jamo
        | 0xA960..=0xA97F    // hangul jamo extended-A
        | 0xAC00..=0xD7AF    // hangul syllables
        | 0x20000..=0x2FA1F) // CJK unified ideographs extensions B..
}

/// The lexical parts of one query, in first-appearance order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LexicalParts {
    pub parts: Vec<String>,
    /// Parts dropped by [`MAX_LEXICAL_PARTS`].
    pub truncated: usize,
}

/// Split query terms into `MATCH` parts.
///
/// A quoted phrase keeps its literal text (quotes mean "this exact string"). An
/// unquoted term is split into CJK and non-CJK runs: a CJK run of three or more
/// characters becomes its overlapping 3-character windows (trigram is what the
/// index stores, and a whole Chinese sentence never appears contiguously), while
/// a non-CJK run keeps the previous rule (one phrase, at least three chars).
pub fn lexical_parts(terms: &[Term]) -> LexicalParts {
    let mut parts: Vec<String> = Vec::new();
    let mut push = |part: String, parts: &mut Vec<String>| {
        if !parts.iter().any(|existing| existing == &part) {
            parts.push(part);
        }
    };
    for term in terms {
        if term.phrase {
            if term.is_long_enough() {
                push(term.text.clone(), &mut parts);
            }
            continue;
        }
        let mut run = String::new();
        let mut run_is_cjk = false;
        for character in term.text.chars() {
            let cjk = is_cjk(character);
            if !run.is_empty() && cjk != run_is_cjk {
                push_run(&run, run_is_cjk, &mut parts, &mut push);
                run.clear();
            }
            run_is_cjk = cjk;
            run.push(character);
        }
        if !run.is_empty() {
            push_run(&run, run_is_cjk, &mut parts, &mut push);
        }
    }
    let truncated = parts.len().saturating_sub(MAX_LEXICAL_PARTS);
    parts.truncate(MAX_LEXICAL_PARTS);
    LexicalParts { parts, truncated }
}

fn push_run(
    run: &str,
    is_cjk: bool,
    parts: &mut Vec<String>,
    push: &mut impl FnMut(String, &mut Vec<String>),
) {
    let characters: Vec<char> = run.chars().collect();
    if is_cjk {
        if characters.len() < 3 {
            return;
        }
        for window in characters.windows(3) {
            push(window.iter().collect(), parts);
        }
    } else if characters.len() >= 3 {
        push(run.to_string(), parts);
    }
}

/// The `OR`-ed `MATCH` expression used by the lexical branch.
fn or_expression(parts: &[String]) -> Option<String> {
    if parts.is_empty() {
        return None;
    }
    let quoted: Vec<String> = parts.iter().map(|part| super::fts_phrase(part)).collect();
    Some(quoted.join(" OR "))
}

fn lexical_branch(
    conn: &Connection,
    parts: &[String],
    filters: &HybridFilters<'_>,
) -> rusqlite::Result<Vec<Row>> {
    let Some(expression) = or_expression(parts) else {
        return Ok(Vec::new());
    };
    let mut values: Vec<Box<dyn ToSql>> = vec![Box::new(expression)];
    let filter = filter_sql(&mut values, filters, "", true);
    // `ORDER BY rank` is the one ordering FTS5 answers from its own accumulator;
    // adding `ts`/`rowid` as SQL tie-breakers instead makes SQLite build a temp
    // B-tree over every match (a natural question matches hundreds of thousands
    // of units). The tie-break is applied to the fetched window below, which is
    // the same rule over the same rows.
    let sql = format!(
        "SELECT {ROW_COLUMNS}, bm25(units) AS score FROM units WHERE units MATCH ?1{filter} \
         ORDER BY rank LIMIT {HYBRID_BRANCH_WINDOW}"
    );
    let mut statement = conn.prepare(&sql)?;
    let params: Vec<&dyn ToSql> = values.iter().map(|value| value.as_ref()).collect();
    let rows = statement.query_map(params.as_slice(), |row| {
        Ok((Row::from_query_row(row)?, row.get::<_, f64>(20)?))
    })?;
    let mut scored: Vec<(Row, f64)> = rows.collect::<rusqlite::Result<Vec<_>>>()?;
    scored.sort_by(|left, right| {
        left.1
            .partial_cmp(&right.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| right.0.ts.cmp(&left.0.ts))
            .then_with(|| left.0.rowid.cmp(&right.0.rowid))
    });
    Ok(scored.into_iter().map(|(row, _)| row).collect())
}

struct SemanticCandidate {
    rowid: i64,
    doc_key: String,
    session_id: String,
    message_id: Option<String>,
    similarity: f64,
}

/// Ranked conversation units, filtered before scoring.
fn semantic_branch(
    conn: &Connection,
    cache: &VectorCache,
    query: &[f32],
    query_norm: f32,
    filters: &HybridFilters<'_>,
) -> rusqlite::Result<(
    Vec<SemanticCandidate>,
    u64,
    BTreeMap<(String, String), Vec<f64>>,
)> {
    if filters
        .kinds
        .map(|kinds| !list_contains(Some(kinds), UNIT_KIND_TEXT))
        .unwrap_or(false)
    {
        // Nothing outside the text channel is embedded.
        return Ok((Vec::new(), 0, BTreeMap::new()));
    }
    let mut values: Vec<Box<dyn ToSql>> = Vec::new();
    let filter = filter_sql(&mut values, filters, "c.", false);
    let sql = format!(
        "SELECT c.rowid, c.doc_key, c.text_sha256, c.session_id, c.message_id \
         FROM conversation_units c WHERE 1 = 1{filter}"
    );
    let mut statement = conn.prepare(&sql)?;
    let params: Vec<&dyn ToSql> = values.iter().map(|value| value.as_ref()).collect();
    let rows = statement.query_map(params.as_slice(), |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, Option<String>>(4)?,
        ))
    })?;

    let mut candidates: Vec<SemanticCandidate> = Vec::new();
    let mut stale = 0u64;
    for row in rows {
        let (rowid, doc_key, text_sha256, session_id, message_id) = row?;
        let similarity = match cache.similarity(&doc_key, &text_sha256, query, query_norm) {
            Similarity::Value(value) => value,
            Similarity::Stale => {
                stale += 1;
                continue;
            }
            Similarity::Missing => continue,
        };
        candidates.push(SemanticCandidate {
            rowid,
            doc_key,
            session_id,
            message_id,
            similarity,
        });
    }
    candidates.sort_by(|left, right| {
        right
            .similarity
            .partial_cmp(&left.similarity)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| left.doc_key.cmp(&right.doc_key))
    });
    candidates.truncate(SEMANTIC_MAX_CANDIDATES);

    let mut turn_sims: BTreeMap<(String, String), Vec<f64>> = BTreeMap::new();
    for candidate in &candidates {
        turn_sims
            .entry((
                candidate.session_id.clone(),
                candidate.message_id.clone().unwrap_or_default(),
            ))
            .or_default()
            .push(candidate.similarity);
    }
    Ok((candidates, stale, turn_sims))
}

fn lexical_accepts(distinct_terms: usize, query_terms: usize) -> bool {
    if query_terms == 0 {
        return false;
    }
    if query_terms == 1 {
        return distinct_terms >= 1;
    }
    distinct_terms >= LEXICAL_ACCEPT_DISTINCT
        && (distinct_terms as f64 / query_terms as f64) >= LEXICAL_ACCEPT_COVERAGE
}

/// How many of the query's lexical parts occur in one unit. The parts are the
/// same ones handed to `MATCH` (CJK trigrams included), so the coverage gate and
/// the lexical branch speak about the same query.
fn distinct_terms(text: &str, parts: &[String]) -> usize {
    let folded = fold_ascii(text);
    parts
        .iter()
        .filter(|part| folded.contains(fold_ascii(part).as_str()))
        .count()
}

struct Candidate {
    row: Row,
    lexical_rank: Option<usize>,
    semantic_rank: Option<usize>,
    similarity: f64,
}

impl Candidate {
    fn score(&self) -> f64 {
        rrf_score(self.lexical_rank, self.semantic_rank)
    }

    fn channels(&self) -> Vec<&'static str> {
        let mut channels = Vec::new();
        if self.lexical_rank.is_some() {
            channels.push(crate::recall::contract::HYBRID_CHANNEL_LEXICAL);
        }
        if self.semantic_rank.is_some() {
            channels.push(crate::recall::contract::HYBRID_CHANNEL_SEMANTIC);
        }
        channels
    }
}

/// Run one hybrid query against one source.
#[allow(clippy::too_many_arguments)]
pub fn search(
    config: &RecallConfig,
    source: &SourceConfig,
    query: &str,
    filters: &HybridFilters<'_>,
    limit: usize,
    offset: usize,
    snippet_bytes: usize,
    semantic_query: &SemanticQuery,
) -> HybridSearch {
    let terms = parse_terms(query);
    let lexical = lexical_parts(&terms);
    // A docs chunk has no semantic branch and the trigram index needs three
    // characters: a two-character CJK query would otherwise return "nothing
    // found" with no reason. Fall back to the scan, which matches substrings.
    if source.kind == SourceKind::Docs && lexical.parts.is_empty() {
        return HybridSearch::Fallback(FallbackReason {
            code: crate::recall::contract::SHORT_TERMS_FALLBACK,
            detail: None,
        });
    }
    let (conn, _path, state) = open_for_query(config, source);
    if state != IndexState::Ready {
        return HybridSearch::Fallback(FallbackReason {
            code: state.reason().unwrap_or(INDEX_REASON_CORRUPT),
            detail: state.detail().map(str::to_string),
        });
    }
    let meta = IndexMeta::load(&conn).ok().flatten();

    let lexical_rows = match lexical_branch(&conn, &lexical.parts, filters) {
        Ok(rows) => rows,
        Err(error) => {
            return HybridSearch::Fallback(FallbackReason {
                code: INDEX_REASON_CORRUPT,
                detail: Some(format!("index_query_failed: {error}")),
            })
        }
    };
    let lexical_count = lexical_rows.len();

    // ---- semantic branch ---------------------------------------------------
    let plan = semantic_query.plan();
    let mut semantic_reason: Option<String> = None;
    let mut semantic_rows: Vec<SemanticCandidate> = Vec::new();
    let mut turn_sims: BTreeMap<(String, String), Vec<f64>> = BTreeMap::new();
    let mut stale_vectors = 0u64;
    let mut semantic_available = false;
    match plan {
        None => semantic_reason = Some("embedding_not_configured".to_string()),
        Some(plan) => {
            let cache_root = match super::configured_cache_root(config) {
                Err(_) => {
                    semantic_reason = Some("cache_root_missing".to_string());
                    None
                }
                Ok(root) => Some(root),
            };
            if let Some(cache_root) = cache_root {
                let path = super::vectors_path(&cache_root, &source.source_id);
                match VectorCache::load(&path, plan) {
                    Err(reason) => semantic_reason = Some(reason.to_string()),
                    Ok(cache) => match semantic_query.state() {
                        SemanticState::NotConfigured => {
                            semantic_reason = Some("embedding_not_configured".to_string())
                        }
                        SemanticState::Unavailable(reason) => {
                            semantic_reason = Some(reason.clone())
                        }
                        SemanticState::Ready { vector, norm } => {
                            match semantic_branch(&conn, &cache, vector, *norm, filters) {
                                Ok((rows, stale, turns)) => {
                                    semantic_rows = rows;
                                    turn_sims = turns;
                                    stale_vectors = stale;
                                    semantic_available = true;
                                }
                                Err(error) => {
                                    semantic_reason =
                                        Some(format!("index_query_failed: {error}"))
                                }
                            }
                        }
                    },
                }
            }
        }
    }
    let semantic_count = semantic_rows.len();

    // ---- fuse --------------------------------------------------------------
    let mut candidates: BTreeMap<i64, Candidate> = BTreeMap::new();
    for (rank, row) in lexical_rows.into_iter().enumerate() {
        let rowid = row.rowid;
        candidates.insert(
            rowid,
            Candidate {
                row,
                lexical_rank: Some(rank),
                semantic_rank: None,
                similarity: 0.0,
            },
        );
    }
    let mut similarity_by_rowid: BTreeMap<i64, f64> = BTreeMap::new();
    let mut ranks: BTreeMap<i64, usize> = BTreeMap::new();
    let mut missing: Vec<i64> = Vec::new();
    for (rank, candidate) in semantic_rows.into_iter().enumerate() {
        similarity_by_rowid.insert(candidate.rowid, candidate.similarity);
        match candidates.get_mut(&candidate.rowid) {
            Some(existing) => {
                existing.semantic_rank = Some(rank);
                existing.similarity = candidate.similarity;
            }
            None => {
                ranks.insert(candidate.rowid, rank);
                missing.push(candidate.rowid);
            }
        }
    }
    if !missing.is_empty() {
        match Row::load_by_rowids(&conn, &missing) {
            Ok(rows) => {
                for row in rows {
                    let rowid = row.rowid;
                    candidates.insert(
                        rowid,
                        Candidate {
                            similarity: similarity_by_rowid.get(&rowid).copied().unwrap_or(0.0),
                            row,
                            lexical_rank: None,
                            semantic_rank: ranks.get(&rowid).copied(),
                        },
                    );
                }
            }
            Err(error) => {
                semantic_available = false;
                semantic_reason = Some(format!("index_query_failed: {error}"));
            }
        }
    }

    let before_gate = candidates.len();
    if semantic_available {
        let query_terms = lexical.parts.len();
        candidates.retain(|_rowid, candidate| {
            let turn = (
                candidate.row.session_id.clone(),
                candidate.row.message_id.clone().unwrap_or_default(),
            );
            let turn_values = turn_sims.get(&turn).cloned().unwrap_or_default();
            let turn_max = turn_values.iter().copied().fold(f64::MIN, f64::max);
            let support = turn_values
                .iter()
                .filter(|similarity| **similarity >= SEMANTIC_SUPPORT_SIMILARITY)
                .count();
            let rule_a = candidate.similarity >= SEMANTIC_ACCEPT_SIMILARITY
                || turn_max >= SEMANTIC_ACCEPT_SIMILARITY;
            let rule_b = lexical_accepts(
                distinct_terms(candidate.row.unit_text(), &lexical.parts),
                query_terms,
            );
            let rule_c =
                support >= SEMANTIC_SUPPORT_UNITS && turn_max >= SEMANTIC_TURN_MAX_SIMILARITY;
            rule_a || rule_b || rule_c
        });
    }
    let gated_out = before_gate.saturating_sub(candidates.len());

    let mut ordered: Vec<Candidate> = candidates.into_values().collect();
    ordered.sort_by(|left, right| {
        right
            .score()
            .partial_cmp(&left.score())
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| right.row.ts.cmp(&left.row.ts))
            .then_with(|| left.row.rowid.cmp(&right.row.rowid))
    });

    // One hit per session: the best fused candidate represents its session and
    // carries how many candidates that session contributed.
    let mut session_counts: BTreeMap<String, u64> = BTreeMap::new();
    let mut kept: Vec<Candidate> = Vec::new();
    if source.kind == SourceKind::Docs {
        // One docs file holds many chunks; collapsing by "session" would make
        // every chunk but the best unreachable.
        kept = ordered;
    } else {
        let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for candidate in ordered {
            let session_id = candidate.row.session_id.clone();
            *session_counts.entry(session_id.clone()).or_insert(0) += 1;
            if seen.insert(session_id) {
                kept.push(candidate);
            }
        }
    }
    let session_total = kept.len();
    let page: Vec<Candidate> = kept.into_iter().skip(offset).take(limit).collect();
    let hits: Vec<SourceHit> = page
        .into_iter()
        .map(|candidate| {
            let score = candidate.score();
            let channels = candidate.channels();
            let session_id = candidate.row.session_id.clone();
            let mut hit = candidate.row.into_hit(
                source,
                &terms,
                snippet_bytes,
                meta.as_ref().map(|meta| meta.fallback.is_some()).unwrap_or(false),
            );
            hit.score = Some(score);
            hit.score_kind = SCORE_KIND_RRF;
            hit.rank_basis = RANK_BASIS_RRF;
            if let Some(exact) = hit.exact.as_mut() {
                exact.session_hits = Some(session_counts.get(&session_id).copied().unwrap_or(0));
                exact.channels = Some(channels);
            }
            hit
        })
        .collect();

    let mut detail = Map::new();
    detail.insert("mode".into(), json!("hybrid"));
    detail.insert(
        "semantic".into(),
        json!(if semantic_available {
            "available"
        } else {
            "degraded"
        }),
    );
    if let Some(reason) = semantic_reason {
        detail.insert("reason".into(), Value::String(reason));
    }
    detail.insert(
        "thresholdsVersion".into(),
        json!(ARCHIVE_RELEVANCE_VERSION),
    );
    detail.insert("fusion".into(), json!("rrf"));
    detail.insert("rrf_k".into(), json!(RRF_K as u64));
    detail.insert("lexical_candidates".into(), json!(lexical_count));
    if lexical.truncated > 0 {
        detail.insert("lexical_terms_truncated".into(), json!(lexical.truncated));
    }
    detail.insert("semantic_candidates".into(), json!(semantic_count));
    detail.insert("gated_out".into(), json!(gated_out));
    detail.insert("sessions".into(), json!(session_total));
    if stale_vectors > 0 {
        detail.insert("stale_vectors".into(), json!(stale_vectors));
    }
    if let Some(meta) = meta.as_ref() {
        if meta.skipped_oversize > 0 {
            detail.insert("skipped_oversize".into(), json!(meta.skipped_oversize));
        }
    }

    let status_value = if hits.is_empty() {
        status::EMPTY
    } else {
        status::OK
    };
    // Coverage/freshness of this index, next to the hybrid block.
    let mut outer = Map::new();
    outer.insert("hybrid".into(), Value::Object(detail));
    outer.insert("resolved_mode".into(), json!("hybrid"));
    if let Some(coverage) = super::coverage::source_coverage(config, source) {
        outer.insert("coverage".into(), coverage);
    }
    HybridSearch::Hits(Box::new(SourceOutcome {
        source_id: source.source_id.clone(),
        status: status_value,
        code: None,
        detail: Some(Value::Object(outer)),
        hits,
        warnings: Vec::new(),
        error_message: None,
        side_effects: Vec::new(),
        violations: Vec::new(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recall::fulltext::parse_terms;

    fn parts(query: &str) -> Vec<String> {
        lexical_parts(&parse_terms(query)).parts
    }

    #[test]
    fn cjk_classification_covers_han_kana_and_hangul() {
        assert!(is_cjk('缓'));
        assert!(is_cjk('あ'));
        assert!(is_cjk('キ'));
        assert!(is_cjk('한'));
        assert!(!is_cjk('，'));
        assert!(!is_cjk('。'));
        assert!(!is_cjk(' '));
        assert!(!is_cjk('a'));
        assert!(!is_cjk('-'));
    }

    #[test]
    fn a_chinese_phrase_becomes_overlapping_trigrams() {
        assert_eq!(parts("缓存失效"), vec!["缓存失", "存失效"]);
        // Terms are split independently: a 2-character neighbour contributes
        // nothing and does not join the previous run.
        assert_eq!(parts("缓存失效 排查"), vec!["缓存失", "存失效"]);
    }

    #[test]
    fn cjk_runs_shorter_than_three_characters_do_not_enter_match() {
        assert!(parts("缓存").is_empty());
        assert!(parts("失效").is_empty());
        assert!(parts("缓存 失效").is_empty());
        // ...but they do not swallow a longer neighbour either.
        assert_eq!(parts("缓存 第二阶段的结论"), vec!["第二阶", "二阶段", "阶段的", "段的结", "的结论"]);
    }

    #[test]
    fn mixed_scripts_keep_ascii_runs_and_split_the_cjk_part() {
        let value = parts("agent-memory 的索引为什么变慢");
        assert_eq!(value[0], "agent-memory");
        assert!(value.contains(&"的索引".to_string()));
        assert!(value.contains(&"索引为".to_string()));
        assert!(value.contains(&"么变慢".to_string()));
        // ASCII-only terms keep the old rule: one phrase, at least 3 chars.
        assert_eq!(parts("PrependPath=1 py3 ab"), vec!["PrependPath=1", "py3"]);
    }

    #[test]
    fn kana_and_hangul_are_split_like_han() {
        assert_eq!(parts("キャッシュ"), vec!["キャッ", "ャッシ", "ッシュ"]);
        assert!(parts("カー").is_empty());
        assert_eq!(parts("캐시가느려"), vec!["캐시가", "시가느", "가느려"]);
    }

    #[test]
    fn quotes_keep_the_literal_phrase() {
        assert_eq!(parts("\"缓存 失效\""), vec!["缓存 失效"]);
        assert!(parts("\"缓存\"").is_empty());
    }

    #[test]
    fn duplicate_parts_are_dropped_and_the_cap_reports_truncation() {
        let repeated = parts("缓存失效缓存失效");
        assert_eq!(repeated, vec!["缓存失", "存失效", "失效缓", "效缓存"]);
        assert!(lexical_parts(&parse_terms("缓存失效缓存失效")).truncated == 0);

        let long: String = (0u32..60)
            .map(|index| char::from_u32(0x4E00 + index).unwrap())
            .collect();
        let split = lexical_parts(&parse_terms(&long));
        assert_eq!(split.parts.len(), MAX_LEXICAL_PARTS);
        assert_eq!(split.truncated, 58 - MAX_LEXICAL_PARTS);
    }
}
