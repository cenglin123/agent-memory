//! Device provider: the explicitly configured device-notes root (§3.4 Device).
//!
//! Readable set = direct children matching `^\d{4}-\d{2}-\d{2}_.+\.md$`, no
//! recursion, hidden entries excluded. A filename + body document is matched
//! with the same lexical rule as OpenCode; `text_range` is the first body hit's
//! line, or the body's first block when only the filename matched.

use crate::recall::budget::Budget;
use crate::recall::config::SourceConfig;
use crate::recall::contract::{
    codes, status, DEVICE_FILENAME_BLOCK_MAX_BYTES, DEVICE_FILE_PATTERN, RANK_BASIS_LEXICAL_WINDOW,
    SCHEMA_MD_V1, SCORE_KIND_NONE, SOURCE_KIND_DEVICE,
};
use crate::recall::device_meta;
use crate::recall::graph::RelatedItem;
use crate::recall::hash::{file_sha256_streaming, root_sha256, sha256_hex, TextRange};
use crate::recall::providers::{rank_hits, SourceHit, SourceOutcome};
use crate::recall::refs::{hit_id, Fingerprint, HitRef, Locator};
use crate::recall::snapshot::Window;
use crate::recall::text::{fold_ascii, snippet, utf8_floor};
use chrono::NaiveDate;
use regex::Regex;
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::path::Path;
use std::sync::OnceLock;
use std::time::Instant;

pub struct DeviceRequest<'a> {
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

pub fn search(request: &DeviceRequest<'_>) -> SourceOutcome {
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

fn pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| Regex::new(DEVICE_FILE_PATTERN).expect("device pattern compiles"))
}

fn scan(request: &DeviceRequest<'_>) -> SourceOutcome {
    let source = request.source;
    let started = Instant::now();
    let entries = match std::fs::read_dir(request.read_root) {
        Ok(entries) => entries,
        Err(err) => {
            return SourceOutcome {
                source_id: source.source_id.clone(),
                status: status::UNAVAILABLE,
                code: Some(codes::SOURCE_ROOT_INVALID),
                detail: Some(serde_json::json!({"reason": "root_unreadable"})),
                hits: Vec::new(),
                warnings: Vec::new(),
                error_message: Some(format!("device root cannot be read: {err}")),
                side_effects: Vec::new(),
                violations: Vec::new(),
            }
        }
    };

    let root_sha = root_sha256(&source.root);
    let mut hits: Vec<SourceHit> = Vec::new();
    let mut skipped = 0u64;
    let mut unresolved = 0u64;
    let mut invalid_date = false;
    let mut unreadable = false;
    let mut consumed = 0u64;
    let mut stop: Option<StopReason> = None;

    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            skipped += 1;
            continue;
        };
        if file_type.is_dir() {
            // No recursion: sub-directories are simply not part of the set.
            continue;
        }
        if file_type.is_symlink() {
            // Never follow a junction/symlink to the outside of the root.
            skipped += 1;
            continue;
        }
        let Ok(name) = entry.file_name().into_string() else {
            skipped += 1;
            continue;
        };
        if name.starts_with('.') {
            continue;
        }
        if !pattern().is_match(&name) {
            continue;
        }
        let date = match NaiveDate::parse_from_str(&name[..10], "%Y-%m-%d") {
            Ok(date) => date,
            Err(_) => {
                skipped += 1;
                invalid_date = true;
                continue;
            }
        };
        if consumed >= request.budget.max_bytes_per_source {
            stop = Some(StopReason::Bytes);
            break;
        }
        if started.elapsed().as_millis() as u64 >= request.budget.max_elapsed_ms_per_source {
            stop = Some(StopReason::Time);
            break;
        }
        let path = entry.path();
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(_) => {
                skipped += 1;
                unreadable = true;
                continue;
            }
        };
        let body = match std::str::from_utf8(&bytes) {
            Ok(body) => body,
            Err(_) => {
                skipped += 1;
                unreadable = true;
                continue;
            }
        };
        consumed += bytes.len() as u64;
        let (meta, note_body) = device_meta::split(body);
        let title = device_meta::title(&meta, note_body, &name);
        let tags = device_meta::tags(&meta, note_body);
        // Search the body without the YAML block (its keys are present in every
        // event and would match queries like `status`). The summary and tags
        // remain searchable as metadata; a metadata-only hit projects the first
        // body block, never the `---` delimiter.
        let prefix_len = body.len() - note_body.len();
        let prefix_lines = body[..prefix_len].bytes().filter(|byte| *byte == b'\n').count() as u64;
        let meta_terms = distinct_terms(
            &format!(
                "{name}\n{}\n{}",
                meta.summary.as_deref().unwrap_or(""),
                tags.join(" ")
            ),
            request.terms,
        );
        let body_found = match_document(&name, note_body, request.terms);
        let Some(mut found) = body_found.or_else(|| {
            (meta_terms > 0).then_some(DocumentMatch {
                distinct_terms: meta_terms,
                document_offset: 0,
                body_hit: None,
            })
        }) else {
            continue;
        };
        if meta_terms > found.distinct_terms {
            found.distinct_terms = meta_terms;
        }
        let (mut range, snippet_text, snippet_truncated) =
            project(note_body.as_bytes(), found, request.snippet_bytes);
        range.byte_start += prefix_len as u64;
        range.byte_end += prefix_len as u64;
        range.line_start += prefix_lines;
        range.line_end += prefix_lines;
        if range.byte_start == range.byte_end {
            // Every emitted ref must be readable (§3.1). An empty range is not:
            // a filename-only hit on an empty body is dropped and reported as
            // CONTENT_UNRESOLVED instead of producing an unreadable hit.
            unresolved += 1;
            continue;
        }
        let sort_time = date
            .and_hms_opt(0, 0, 0)
            .map(|stamp| stamp.and_utc().timestamp())
            .unwrap_or(0);
        let hit = HitRef {
            source_id: source.source_id.clone(),
            source_kind: crate::recall::contract::SOURCE_KIND_DEVICE.to_string(),
            root_sha256: root_sha.clone(),
            rel_path: name.clone(),
            schema_gen: crate::recall::contract::SCHEMA_MD_V1.to_string(),
            locator: Locator::empty(),
            fingerprint: Fingerprint {
                range_sha256: sha256_hex(
                    &body.as_bytes()[range.byte_start as usize..range.byte_end as usize],
                ),
                file_sha256: file_sha256_streaming(&path).ok(),
                row_sha256: None,
            },
            text_range: range,
        };
        hits.push(SourceHit {
            hit,
            title,
            snippet: snippet_text,
            snippet_truncated,
            score: None,
            score_kind: SCORE_KIND_NONE,
            rank_basis: RANK_BASIS_LEXICAL_WINDOW,
            distinct_terms: found.distinct_terms,
            first_byte: found.document_offset,
            sort_time,
            sort_id: name,
            related: Vec::new(),
                        exact: None,
        });
    }

    rank_hits(&mut hits);

    let mut detail = Map::new();
    if skipped > 0 {
        if invalid_date {
            detail.insert("reason".into(), Value::String("invalid_date".to_string()));
        } else if unreadable {
            detail.insert("reason".into(), Value::String("unreadable".to_string()));
        }
    }
    if unresolved > 0 {
        detail.insert("CONTENT_UNRESOLVED".into(), Value::from(unresolved));
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

    let (status_value, code, message) = if skipped > 0 || unresolved > 0 {
        (
            status::PARTIAL,
            Some(if skipped > 0 {
                codes::RECORD_SKIPPED
            } else {
                codes::CONTENT_UNRESOLVED
            }),
            Some(format!(
                "{skipped} device record(s) skipped, {unresolved} hit(s) without a readable range"
            )),
        )
    } else if let Some(reason) = stop {
        (
            status::TRUNCATED,
            Some(match reason {
                StopReason::Bytes => codes::BUDGET_BYTES,
                StopReason::Time => codes::BUDGET_TIME,
            }),
            Some("device source budget exhausted".to_string()),
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
        detail: if detail.is_empty() {
            None
        } else {
            Some(Value::Object(detail))
        },
        hits,
        warnings: Vec::new(),
        error_message: message,
        side_effects: Vec::new(),
        violations: Vec::new(),
    }
}

#[derive(Debug, Clone, Copy)]
struct DocumentMatch {
    distinct_terms: usize,
    /// Earliest hit in document order (filename, then body).
    document_offset: usize,
    /// Earliest body hit and the byte length of that term (for the snippet).
    body_hit: Option<(usize, usize)>,
}

fn distinct_terms(text: &str, terms: &[String]) -> usize {
    let folded = fold_ascii(text);
    terms
        .iter()
        .filter(|term| folded.contains(fold_ascii(term).as_str()))
        .count()
}

fn match_document(name: &str, body: &str, terms: &[String]) -> Option<DocumentMatch> {
    if terms.is_empty() {
        return None;
    }
    let folded_name = fold_ascii(name);
    let folded_body = fold_ascii(body);
    let body_base = name.len() + 1;
    let mut distinct = 0usize;
    let mut document_offset: Option<usize> = None;
    let mut body_hit: Option<(usize, usize)> = None;
    for term in terms {
        let in_name = folded_name.find(term.as_str());
        let in_body = folded_body.find(term.as_str());
        if in_name.is_none() && in_body.is_none() {
            continue;
        }
        distinct += 1;
        let offset = match (in_name, in_body) {
            (Some(offset), _) => offset,
            (None, Some(offset)) => body_base + offset,
            (None, None) => continue,
        };
        document_offset = Some(document_offset.map_or(offset, |current| current.min(offset)));
        if let Some(offset) = in_body {
            body_hit = Some(match body_hit {
                Some((current, len)) if current <= offset => (current, len),
                _ => (offset, term.len()),
            });
        }
    }
    document_offset.map(|document_offset| DocumentMatch {
        distinct_terms: distinct,
        document_offset,
        body_hit,
    })
}

fn project(body: &[u8], found: DocumentMatch, snippet_bytes: usize) -> (TextRange, String, bool) {
    if let Some((offset, term_len)) = found.body_hit {
        let start = crate::recall::text::line_start_of(body, offset);
        let mut end = body[start..]
            .iter()
            .position(|byte| *byte == b'\n')
            .map(|index| start + index)
            .unwrap_or(body.len());
        // A CRLF terminator is one line ending: keep neither `\r` nor `\n`.
        if end > start && body[end - 1] == b'\r' {
            end -= 1;
        }
        let range = TextRange::from_span(body, start, end.max(start));
        let text = std::str::from_utf8(&body[start..end.max(start)]).unwrap_or_default();
        let (display, truncated) = snippet(text, offset - start, term_len, snippet_bytes);
        return (range, display, truncated);
    }

    // Filename-only hit: the body's first block.
    let empty = body.is_empty();
    let text = std::str::from_utf8(body).unwrap_or_default();
    let end = if empty {
        0
    } else {
        let line_end = body
            .iter()
            .position(|byte| *byte == b'\n')
            .unwrap_or(body.len());
        let cap = line_end.min(DEVICE_FILENAME_BLOCK_MAX_BYTES);
        utf8_floor(text, cap)
    };
    let range = TextRange::from_span(body, 0, end);
    let (display, truncated) = snippet(text, 0, 0, snippet_bytes);
    (range, display, truncated)
}

// ---------------------------------------------------------------------------
// Enrichment: frontmatter titles, related notes, and reuse ordering
// ---------------------------------------------------------------------------

struct NoteDoc {
    name: String,
    body: String,
    title: String,
    tags: Vec<String>,
    links: Vec<String>,
}

/// Enrichment is best-effort: the graph pass is bounded so a large or
/// OneDrive-backed memory root cannot turn every search into a full read.
const ENRICH_MAX_FILES: usize = 512;
const ENRICH_MAX_BYTES: u64 = 8 * 1024 * 1024;
const ENRICH_MAX_HITS: usize = 20;

/// Read the note corpus once, bounded by count and bytes. Returns
/// `(docs, limited)`; `limited` is reported in the source detail.
fn load_notes(root: &Path) -> (Vec<NoteDoc>, bool) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return (Vec::new(), false);
    };
    let mut docs = Vec::new();
    let mut bytes_read = 0u64;
    for entry in entries.flatten() {
        if docs.len() >= ENRICH_MAX_FILES || bytes_read >= ENRICH_MAX_BYTES {
            return (docs, true);
        }
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() || file_type.is_symlink() {
            continue;
        }
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if name.starts_with('.') || !pattern().is_match(&name) {
            continue;
        }
        let Ok(bytes) = std::fs::read(entry.path()) else {
            continue;
        };
        bytes_read += bytes.len() as u64;
        let Ok(body) = String::from_utf8(bytes) else {
            continue;
        };
        let (meta, note_body) = device_meta::split(&body);
        docs.push(NoteDoc {
            title: device_meta::title(&meta, note_body, &name),
            tags: device_meta::tags(&meta, note_body),
            links: device_meta::links(&meta, note_body),
            name,
            body,
        });
    }
    (docs, false)
}

fn resolve_link(link: &str, by_name: &HashMap<String, usize>) -> Option<usize> {
    if let Some(index) = by_name.get(link) {
        return Some(*index);
    }
    by_name.get(&format!("{link}.md")).copied()
}

fn reference_for(source: &SourceConfig, doc: &NoteDoc) -> HitRef {
    HitRef {
        source_id: source.source_id.clone(),
        source_kind: SOURCE_KIND_DEVICE.to_string(),
        root_sha256: root_sha256(&source.root),
        rel_path: doc.name.clone(),
        schema_gen: SCHEMA_MD_V1.to_string(),
        locator: Locator::empty(),
        fingerprint: Fingerprint {
            range_sha256: sha256_hex(doc.body.as_bytes()),
            // The full-file hash is optional for a device ref; skipping it
            // avoids re-streaming every related note on each search.
            file_sha256: None,
            row_sha256: None,
        },
        text_range: TextRange::from_span(doc.body.as_bytes(), 0, doc.body.len()),
    }
}

fn related_for(
    source: &SourceConfig,
    docs: &[NoteDoc],
    by_name: &HashMap<String, usize>,
    tag_index: &HashMap<String, Vec<usize>>,
    index: usize,
) -> Vec<RelatedItem> {
    let Some(doc) = docs.get(index) else {
        return Vec::new();
    };
    let mut scores: HashMap<usize, (u32, bool)> = HashMap::new();
    for link in &doc.links {
        if let Some(other) = resolve_link(link, by_name) {
            if other != index {
                let entry = scores.entry(other).or_insert((0, false));
                if entry.0 < 2 {
                    *entry = (2, true);
                }
            }
        }
    }
    for tag in &doc.tags {
        if let Some(others) = tag_index.get(tag) {
            for other in others {
                if *other != index {
                    let entry = scores.entry(*other).or_insert((0, false));
                    if entry.0 < 1 {
                        entry.0 = 1;
                    }
                }
            }
        }
    }
    let mut ranked: Vec<(usize, u32, bool)> = scores
        .into_iter()
        .map(|(index, (score, is_link))| (index, score, is_link))
        .collect();
    ranked.sort_by(|a, b| {
        b.1.cmp(&a.1)
            .then_with(|| b.2.cmp(&a.2))
            .then_with(|| docs[a.0].name.cmp(&docs[b.0].name))
    });
    ranked
        .into_iter()
        .take(3)
        .map(|(other, _, is_link)| {
            let reference = reference_for(source, &docs[other]);
            RelatedItem {
                hit_id: hit_id(&reference),
                title: docs[other].title.clone(),
                reference,
                edge: if is_link { "link" } else { "tag" },
                direction: "out",
                hops: 1,
                turn: 0,
            }
        })
        .collect()
}

fn load_reuse(source: &SourceConfig) -> HashMap<String, u32> {
    // The derived state is written by `Update-Index.ps1` (memory_runtime.py
    // stats); reading it is constant-size and avoids parsing the touch log on
    // every search.
    let path = Path::new(&source.root).join(".recall").join("state.json");
    let Ok(text) = std::fs::read_to_string(path) else {
        return HashMap::new();
    };
    let Ok(data) = serde_json::from_str::<Value>(&text) else {
        return HashMap::new();
    };
    let Some(files) = data.get("files").and_then(Value::as_object) else {
        return HashMap::new();
    };
    let mut result = HashMap::new();
    for (name, entry) in files {
        let days = entry.get("days").and_then(Value::as_u64).unwrap_or(0);
        let sessions = entry.get("sessions").and_then(Value::as_u64).unwrap_or(0);
        result.insert(name.clone(), days.max(sessions) as u32);
    }
    result
}

/// Attach frontmatter titles, explicit-link/tag neighbours and reuse ordering
/// to one device outcome. Best-effort: an unreadable note never fails search.
pub fn enrich(source: &SourceConfig, outcome: &mut SourceOutcome) {
    if outcome.hits.is_empty() {
        return;
    }
    let (docs, limited) = load_notes(Path::new(&source.root));
    if docs.is_empty() {
        return;
    }
    if limited {
        let mut detail = outcome
            .detail
            .take()
            .and_then(|value| value.as_object().cloned())
            .unwrap_or_default();
        detail.insert("related_limited".into(), Value::Bool(true));
        outcome.detail = Some(Value::Object(detail));
    }
    let by_name: HashMap<String, usize> = docs
        .iter()
        .enumerate()
        .map(|(index, doc)| (doc.name.clone(), index))
        .collect();
    let mut tag_index: HashMap<String, Vec<usize>> = HashMap::new();
    for (index, doc) in docs.iter().enumerate() {
        for tag in &doc.tags {
            tag_index.entry(tag.clone()).or_default().push(index);
        }
    }
    let reuse = load_reuse(source);
    for hit in outcome.hits.iter_mut().take(ENRICH_MAX_HITS) {
        if let Some(index) = by_name.get(&hit.hit.rel_path).copied() {
            hit.title = docs[index].title.clone();
            hit.related = related_for(source, &docs, &by_name, &tag_index, index);
        }
    }
    // A bounded boost, not a re-rank: 10% of the best native score per
    // log2(reuse + 1), capped at 30%. A scan outcome has no native score, so
    // it keeps the lexical order and uses reuse only inside an equal
    // distinct-term tier.
    let max_score = outcome
        .hits
        .iter()
        .filter_map(|hit| hit.score)
        .fold(0.0_f64, f64::max);
    if max_score > 0.0 {
        for hit in outcome.hits.iter_mut() {
            if let Some(score) = hit.score {
                let count = reuse.get(&hit.hit.rel_path).copied().unwrap_or(0) as f64;
                let boost = (0.1 * (1.0 + count).log2()).min(0.3) * max_score;
                hit.score = Some(score + boost);
            }
        }
        outcome
            .hits
            .sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    } else {
        outcome.hits.sort_by(|a, b| {
            b.distinct_terms
                .cmp(&a.distinct_terms)
                .then_with(|| {
                    reuse
                        .get(&b.hit.rel_path)
                        .copied()
                        .unwrap_or(0)
                        .cmp(&reuse.get(&a.hit.rel_path).copied().unwrap_or(0))
                })
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recall::text::tokenize;

    #[test]
    fn filename_and_body_are_one_document() {
        let terms = tokenize("event");
        let found = match_document("2026-10-25_event.md", "body without it", &terms).unwrap();
        assert_eq!(found.distinct_terms, 1);
        assert_eq!(found.document_offset, "2026-10-25_".len());
        assert!(found.body_hit.is_none(), "hit is in the filename only");

        let terms = tokenize("断电");
        let found = match_document("2026-10-25_event.md", "今天断电了", &terms).unwrap();
        assert!(found.body_hit.is_some());
    }

    #[test]
    fn filename_only_hit_takes_the_first_body_block() {
        let terms = tokenize("event");
        let body = "first line\nsecond line\n";
        let found = match_document("2026-10-25_event.md", body, &terms).unwrap();
        let (range, _, _) = project(body.as_bytes(), found, 400);
        assert_eq!(
            (range.byte_start, range.byte_end),
            (0, "first line".len() as u64)
        );

        let (range, _, _) = project(b"", found, 400);
        assert_eq!((range.byte_start, range.byte_end), (0, 0));
    }

    #[test]
    fn scan_ignores_frontmatter_keys_and_reads_after_the_block() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("2026-11-01_a.md");
        crate::test_support::write_file(
            &path,
            "---\nschema: 1\nsummary: BT 代理\nstatus: active\n---\nClash 吞掉 BT 的 UDP\n",
        )
        .unwrap();
        let source = SourceConfig {
            source_id: "device".to_string(),
            kind: crate::recall::config::SourceKind::Device,
            root: dir.path().to_string_lossy().to_string(),
            enabled: true,
            db: None,
            storage: None,
            include: Vec::new(),
            exclude: Vec::new(),
        };
        let budget = Budget::default();
        let status_terms = vec!["status".to_string()];
        let none = search(&DeviceRequest {
            source: &source,
            terms: &status_terms,
            budget: &budget,
            snippet_bytes: 200,
            read_root: dir.path(),
            observe_root: false,
        });
        assert!(none.hits.is_empty(), "frontmatter keys must not match");
        let bt_terms = vec!["BT".to_string()];
        let hit = search(&DeviceRequest {
            source: &source,
            terms: &bt_terms,
            budget: &budget,
            snippet_bytes: 200,
            read_root: dir.path(),
            observe_root: false,
        });
        assert_eq!(hit.hits.len(), 1);
        let range = hit.hits[0].hit.text_range;
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body[range.byte_start as usize..range.byte_end as usize].contains("BT"));
        assert!(!body[range.byte_start as usize..range.byte_end as usize].contains("schema:"));
    }

    #[test]
    fn reuse_reads_the_derived_state_instead_of_the_touch_log() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".recall")).unwrap();
        crate::test_support::write_file(
            dir.path().join(".recall/state.json"),
            r#"{"schema":1,"files":{"a.md":{"reads":9,"days":3,"sessions":2}}}"#,
        )
        .unwrap();
        let source = SourceConfig {
            source_id: "device".to_string(),
            kind: crate::recall::config::SourceKind::Device,
            root: dir.path().to_string_lossy().to_string(),
            enabled: true,
            db: None,
            storage: None,
            include: Vec::new(),
            exclude: Vec::new(),
        };
        let reuse = load_reuse(&source);
        assert_eq!(reuse.get("a.md"), Some(&3));
    }

    #[test]
    fn related_uses_explicit_links_before_shared_tags() {
        let dir = tempfile::tempdir().unwrap();
        crate::test_support::write_file(
            dir.path().join("2026-11-01_a.md"),
            "---\nsummary: A\ntags: [网络]\nlinks: [2026-11-02_b]\n---\nA body\n",
        )
        .unwrap();
        crate::test_support::write_file(
            dir.path().join("2026-11-02_b.md"),
            "---\nsummary: B\ntags: [网络]\n---\nB body\n",
        )
        .unwrap();
        crate::test_support::write_file(
            dir.path().join("2026-11-03_c.md"),
            "---\nsummary: C\ntags: [网络]\n---\nC body\n",
        )
        .unwrap();
        let source = SourceConfig {
            source_id: "device".to_string(),
            kind: crate::recall::config::SourceKind::Device,
            root: dir.path().to_string_lossy().to_string(),
            enabled: true,
            db: None,
            storage: None,
            include: Vec::new(),
            exclude: Vec::new(),
        };
        let (docs, _) = load_notes(dir.path());
        let by_name: HashMap<String, usize> = docs
            .iter()
            .enumerate()
            .map(|(index, doc)| (doc.name.clone(), index))
            .collect();
        let mut tag_index: HashMap<String, Vec<usize>> = HashMap::new();
        for (index, doc) in docs.iter().enumerate() {
            for tag in &doc.tags {
                tag_index.entry(tag.clone()).or_default().push(index);
            }
        }
        let index = by_name["2026-11-01_a.md"];
        let related = related_for(&source, &docs, &by_name, &tag_index, index);
        assert_eq!(related.len(), 2);
        assert_eq!(related[0].reference.rel_path, "2026-11-02_b.md");
        assert_eq!(related[0].edge, "link");
        assert_eq!(related[1].edge, "tag");
    }

    #[test]
    fn filename_only_block_never_splits_a_character() {
        let terms = tokenize("event");
        let body = format!("{}\nrest", "中".repeat(2000));
        let found = match_document("2026-10-25_event.md", &body, &terms).unwrap();
        let (range, _, _) = project(body.as_bytes(), found, 400);
        assert_eq!(range.byte_end as usize, DEVICE_FILENAME_BLOCK_MAX_BYTES - 1);
        assert!(body.is_char_boundary(range.byte_end as usize));
    }
}
