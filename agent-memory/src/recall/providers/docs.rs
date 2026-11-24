//! Docs provider: the scan path for a process-document source.
//!
//! The index answers ordinary queries; this scan answers the two cases the
//! trigram index cannot: an explicit `mode:"legacy"` and a query whose CJK runs
//! are shorter than three characters (no lexical part exists, so the hybrid
//! branch is empty). The chunking rule is the index builder's
//! ([`crate::recall::docs`]), so a scan hit and an index hit describe the same
//! byte range.

use crate::recall::budget::Budget;
use crate::recall::config::SourceConfig;
use crate::recall::contract::{
    codes, status, RANK_BASIS_LEXICAL_WINDOW, SCORE_KIND_NONE,
};
use crate::recall::hash::{file_sha256_streaming, root_sha256, sha256_hex, TextRange};
use crate::recall::providers::{rank_hits, SourceHit, SourceOutcome};
use crate::recall::refs::{Fingerprint, HitRef, Locator};
use crate::recall::snapshot::Window;
use crate::recall::text::{fold_ascii, snippet};
use serde_json::{Map, Value};
use std::path::Path;
use std::time::Instant;

pub struct DocsRequest<'a> {
    pub source: &'a SourceConfig,
    pub terms: &'a [String],
    pub budget: &'a Budget,
    pub snippet_bytes: usize,
    pub read_root: &'a Path,
    pub observe_root: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StopReason {
    Bytes,
    Time,
}

struct Match {
    distinct_terms: usize,
    first_byte: usize,
}

/// Substring match over the folded chunk text; returns the distinct term count
/// and the first hit's byte offset in the display text.
fn match_text(text: &str, terms: &[String]) -> Option<Match> {
    let folded = fold_ascii(text);
    let mut distinct = 0usize;
    let mut first: Option<usize> = None;
    for term in terms {
        let needle = fold_ascii(term);
        if needle.is_empty() {
            continue;
        }
        if let Some(offset) = folded.find(needle.as_str()) {
            distinct += 1;
            first = Some(first.map_or(offset, |current| current.min(offset)));
        }
    }
    (distinct > 0).then(|| Match {
        distinct_terms: distinct,
        first_byte: first.unwrap_or(0),
    })
}

pub fn search(request: &DocsRequest<'_>) -> SourceOutcome {
    let window = if request.observe_root {
        Some(Window::open(request.read_root))
    } else {
        None
    };
    let mut outcome = scan(request);
    if let Some(window) = window {
        let (side_effects, violations) = window.close();
        outcome.side_effects = side_effects;
        if !violations.is_empty() {
            outcome.violations = violations;
            outcome.status = status::ERROR;
            outcome.error_message =
                Some("observed root changed outside the C-level runtime artifacts".to_string());
        }
    }
    outcome
}

fn scan(request: &DocsRequest<'_>) -> SourceOutcome {
    let source = request.source;
    let started = Instant::now();
    let root_sha = root_sha256(&source.root);
    let files = match crate::recall::docs::files_at(
        &request.read_root.to_string_lossy(),
        &source.include,
        &source.exclude,
    ) {
        Ok(files) => files,
        Err(error) => {
            return SourceOutcome {
                source_id: source.source_id.clone(),
                status: status::UNAVAILABLE,
                code: Some(codes::SOURCE_ROOT_INVALID),
                detail: Some(serde_json::json!({"reason": "root_unreadable"})),
                hits: Vec::new(),
                warnings: Vec::new(),
                error_message: Some(format!("docs root cannot be read: {error}")),
                side_effects: Vec::new(),
                violations: Vec::new(),
            }
        }
    };

    let mut hits: Vec<SourceHit> = Vec::new();
    let mut unreadable = 0u64;
    let mut consumed = 0u64;
    let mut stop: Option<StopReason> = None;
    for (rel, path) in files {
        if consumed >= request.budget.max_bytes_per_source {
            stop = Some(StopReason::Bytes);
            break;
        }
        if started.elapsed().as_millis() as u64 >= request.budget.max_elapsed_ms_per_source {
            stop = Some(StopReason::Time);
            break;
        }
        let Ok(bytes) = std::fs::read(&path) else {
            unreadable += 1;
            continue;
        };
        let Ok(body) = std::str::from_utf8(&bytes) else {
            unreadable += 1;
            continue;
        };
        consumed += bytes.len() as u64;
        let file_title = crate::recall::docs::file_title(&rel, body);
        let modified = std::fs::metadata(&path)
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|stamp| stamp.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|delta| delta.as_millis() as i64)
            .unwrap_or(0);
        for chunk in crate::recall::docs::chunks(body, &file_title) {
            let text = &body[chunk.start..chunk.end];
            let display = format!("{}\n{}", chunk.title, text);
            let Some(found) = match_text(&display, request.terms) else {
                continue;
            };
            let (snippet_text, snippet_truncated) =
                snippet(&display, found.first_byte, 1, request.snippet_bytes);
            let range = TextRange::from_span(body.as_bytes(), chunk.start, chunk.end);
            let mut locator = Locator::empty();
            locator.pointer = Some(format!("/chunk/{}", chunk.start));
            let hit = HitRef {
                source_id: source.source_id.clone(),
                source_kind: crate::recall::contract::SOURCE_KIND_DOCS.to_string(),
                root_sha256: root_sha.clone(),
                rel_path: rel.clone(),
                schema_gen: crate::recall::contract::SCHEMA_DOCS_MD_V1.to_string(),
                locator,
                fingerprint: Fingerprint {
                    range_sha256: sha256_hex(text.as_bytes()),
                    file_sha256: file_sha256_streaming(&path).ok(),
                    row_sha256: None,
                },
                // The ref is chunk-relative, like an indexed docs ref.
                text_range: TextRange::new(
                    0,
                    (chunk.end - chunk.start) as u64,
                    0,
                    range.line_end.saturating_sub(range.line_start),
                ),
            };
            hits.push(SourceHit {
                hit,
                title: chunk.title,
                snippet: snippet_text,
                snippet_truncated,
                score: None,
                score_kind: SCORE_KIND_NONE,
                rank_basis: RANK_BASIS_LEXICAL_WINDOW,
                distinct_terms: found.distinct_terms,
                first_byte: found.first_byte,
                sort_time: modified,
                sort_id: format!("{rel}#{}", chunk.start),
                related: Vec::new(),
                exact: None,
            });
        }
    }

    rank_hits(&mut hits);

    let mut detail = Map::new();
    if unreadable > 0 {
        detail.insert("reason".into(), Value::String("unreadable".to_string()));
        detail.insert("skipped_unreadable".into(), Value::from(unreadable));
    }
    if let Some(reason) = stop {
        let mut budget = Map::new();
        match reason {
            StopReason::Bytes => {
                budget.insert("reason".into(), Value::String(codes::BUDGET_BYTES.to_string()));
                budget.insert("consumed_bytes".into(), Value::from(consumed));
            }
            StopReason::Time => {
                budget.insert("reason".into(), Value::String(codes::BUDGET_TIME.to_string()));
                budget.insert(
                    "elapsed_ms".into(),
                    Value::from(started.elapsed().as_millis() as u64),
                );
            }
        }
        detail.insert("budget".into(), Value::Object(budget));
    }
    detail.insert("mode".into(), Value::String("scan_fallback".to_string()));

    let (status_value, code, message) = if unreadable > 0 {
        (
            status::PARTIAL,
            Some(codes::RECORD_SKIPPED),
            Some(format!("{unreadable} docs file(s) unreadable")),
        )
    } else if let Some(reason) = stop {
        (
            status::TRUNCATED,
            Some(match reason {
                StopReason::Bytes => codes::BUDGET_BYTES,
                StopReason::Time => codes::BUDGET_TIME,
            }),
            Some("docs source budget exhausted".to_string()),
        )
    } else if hits.is_empty() {
        (status::EMPTY, None, None)
    } else {
        (status::OK, None, None)
    };

    SourceOutcome {
        source_id: source.source_id.clone(),
        status: status_value,
        code,
        detail: Some(Value::Object(detail)),
        hits,
        warnings: Vec::new(),
        error_message: message,
        side_effects: Vec::new(),
        violations: Vec::new(),
    }
}
