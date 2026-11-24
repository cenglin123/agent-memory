//! Stage 7 §B (user ruling: moved here from stage 8): mechanical pairing of
//! contradictory statements about the same ASCII identifier.
//!
//! # What is compared
//!
//! Only **user** text units of the existing live full-text indexes are read
//! (`role = 'user'`, `kind = 'text'`). The index projection already excludes
//! assistant/tool channels and the agent-written compaction summary records
//! (Claude Code `isCompactSummary`, OpenCode `synthetic`, …); this scan never
//! re-introduces them. Archive-fallback units live in the same index with
//! `origin = 'archive'` and are read under exactly the same rule, so the
//! archive does not need a second projection.
//!
//! # Mechanical pairing
//!
//! One identifier (for example `qwen3.5:9b`) in two user sentences of opposite
//! polarity — affirmative use (`用`, `默认用`, `改用`, `就用`, `use`, …) and
//! negative prohibition (`不要用`, `不使用`, `不得使用`, `别用`, `禁止`,
//! `停止使用`, `don't use`, …) — becomes one `needs_review` pair. The report
//! names both `ref`s (readable through `memory_read`), both times, both
//! sessions and both polarities. **Nothing is decided and nothing is written**:
//! neither statement is marked right.
//!
//! # Excluded sentences
//!
//! Six exclusion classes are implemented as rules, each with a positive and a
//! negative synthetic test:
//!
//! | code | meaning |
//! |---|---|
//! | `negated_question` | a question that also carries a negation (`要不要…？`) |
//! | `question` | a question, including the no-mark forms (`是不是`, `能不能`) |
//! | `conditional` | condition or hypothesis (`如果`, `要是`, `……的话`, `if`) |
//! | `example` | an example (`比如`, `例如`, `for example`) |
//! | `quotation` | quoted text (`「…」`, `“…”`, `"…"`) or a `>` quote line |
//! | `third_party` | reported speech at the line start (`他说`, `某某：`) |

use std::collections::BTreeMap;
use std::sync::OnceLock;

use chrono::{DateTime, SecondsFormat};
use regex::Regex;
use serde::Serialize;

use crate::recall::config::{RecallConfig, SourceConfig};
use crate::recall::fulltext::{self, row::Row, row::ROW_COLUMNS, IndexState};

use super::extract::normalize;

/// Both occurrences are present, a human must decide.
pub const STATUS_NEEDS_REVIEW: &str = "needs_review";
/// The identifier was used affirmatively.
pub const POLARITY_AFFIRMATIVE: &str = "affirmative";
/// The identifier was prohibited.
pub const POLARITY_NEGATIVE: &str = "negative";

pub const EXCLUSION_NEGATED_QUESTION: &str = "negated_question";
pub const EXCLUSION_QUESTION: &str = "question";
pub const EXCLUSION_CONDITIONAL: &str = "conditional";
pub const EXCLUSION_EXAMPLE: &str = "example";
pub const EXCLUSION_QUOTATION: &str = "quotation";
pub const EXCLUSION_THIRD_PARTY: &str = "third_party";

/// Upper bound of user text units examined per source. Newest rows are read
/// first; a source that hits the bound is disclosed as `truncated`.
pub const MAX_UNITS_PER_SOURCE: usize = 50_000;

/// Broad negation words used only to classify an excluded *question* as a
/// negated question. They are not usage verbs and never create a polarity.
const NEGATION_WORDS: [&str; 10] = [
    "不要", "不用", "不得", "别", "禁止", "停止", "不再", "don't", "never", "do not",
];

/// Usage verbs that make a prohibition, matched only when they end within six
/// characters before the identifier (see [`adjacency_polarity`]).
const NEGATIVE_MARKERS: [&str; 11] = [
    "不要使用",
    "不要用",
    "不用",
    "不使用",
    "不得使用",
    "别用",
    "禁止使用",
    "停止使用",
    "不再使用",
    "don't use",
    "stop using",
];

/// Usage verbs that make an affirmative statement.
const AFFIRMATIVE_MARKERS: [&str; 9] = [
    "默认用",
    "改用",
    "采用",
    "选用",
    "换成",
    "使用",
    "用",
    "switch to",
    "use",
];

/// How many characters may sit between a usage verb and the identifier.
const MAX_VERB_GAP: usize = 6;

/// File extensions that are never conflict anchors.
const FILENAME_EXTENSIONS: [&str; 20] = [
    "py", "md", "rs", "json", "jsonl", "txt", "ps1", "exe", "db", "sqlite", "yaml", "yml", "toml",
    "sh", "js", "ts", "html", "css", "log", "csv",
];

const QUESTION_MARKERS: [&str; 13] = [
    "是不是",
    "要不要",
    "能不能",
    "会不会",
    "可不可以",
    "行不行",
    "该不该",
    "有没有",
    "是否",
    "哪种",
    "哪个",
    "什么",
    "为什么",
];

const CONDITIONAL_MARKERS: [&str; 10] = [
    "如果",
    "要是",
    "万一",
    "假如",
    "假设",
    "倘若",
    "若是",
    "的话",
    "如果没",
    "if ",
];

const EXAMPLE_MARKERS: [&str; 8] = [
    "比如",
    "例如",
    "譬如",
    "比如说",
    "for example",
    "e.g.",
    "such as",
    "举例",
];

/// One occurrence of an identifier with a polarity.
#[derive(Debug, Clone, Serialize)]
pub struct ConflictOccurrence {
    pub polarity: &'static str,
    #[serde(rename = "ref")]
    pub reference: crate::recall::refs::HitRef,
    pub hit_id: String,
    pub snippet: String,
    /// RFC 3339 UTC, or `null` when the source carries no timestamp.
    pub time: Option<String>,
    pub time_ms: i64,
    pub session_id: String,
    pub session_kind: String,
    pub source_id: String,
    pub title: String,
    /// `live` or `archive` (archive-fallback unit in the same index).
    pub origin: String,
}

/// One contradictory pair, marked for human review.
#[derive(Debug, Clone, Serialize)]
pub struct ConflictPair {
    pub identifier: String,
    /// The normalized grouping key (punctuation and case folded).
    pub anchor: String,
    pub status: &'static str,
    pub affirmative: ConflictOccurrence,
    pub negative: ConflictOccurrence,
    /// All affirmative occurrences of this identifier (interactive user text).
    pub affirmative_count: usize,
    /// All negative occurrences of this identifier (interactive user text).
    pub negative_count: usize,
}

/// What one configured source contributed to the scan.
#[derive(Debug, Clone, Serialize)]
pub struct ConflictSource {
    pub source_id: String,
    /// `scanned`, or the index reason when the index was unusable.
    pub status: String,
    pub rows_examined: usize,
    pub truncated: bool,
}

/// The full result of one conflict scan.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ConflictScan {
    pub sources: Vec<ConflictSource>,
    pub pairs: Vec<ConflictPair>,
    pub excluded: BTreeMap<&'static str, usize>,
}

/// One classified segment of a user unit.
struct Segment<'a> {
    text: &'a str,
    offset: usize,
}

/// One extracted identifier occurrence before pairing.
#[derive(Clone)]
struct Occurrence {
    polarity: &'static str,
    display: String,
    anchor: String,
    reference: crate::recall::refs::HitRef,
    hit_id: String,
    snippet: String,
    time: Option<String>,
    time_ms: i64,
    session_id: String,
    session_kind: String,
    source_id: String,
    title: String,
    origin: String,
    rowid: i64,
}

/// Scan every configured live-session source. Strictly read-only.
pub fn scan_conflicts(
    config: &RecallConfig,
    since_ms: Option<i64>,
    limit: Option<usize>,
) -> ConflictScan {
    let mut scan = ConflictScan::default();
    let mut occurrences: Vec<Occurrence> = Vec::new();

    for source in config.sources.iter().filter(|source| source.enabled) {
        if !is_session_source(source) {
            continue;
        }
        let (conn, _path, state) = fulltext::open_for_query(config, source);
        if state != IndexState::Ready {
            scan.sources.push(ConflictSource {
                source_id: source.source_id.clone(),
                status: state
                    .reason()
                    .unwrap_or("index_unavailable")
                    .to_string(),
                rows_examined: 0,
                truncated: false,
            });
            continue;
        }
        let sql = format!(
            "SELECT {ROW_COLUMNS} FROM units WHERE role = 'user' AND kind = 'text' \
             AND session_kind = '{}' ORDER BY ts DESC, rowid DESC",
            crate::recall::contract::SESSION_KIND_INTERACTIVE
        );
        let Ok(mut statement) = conn.prepare(&sql) else {
            scan.sources.push(ConflictSource {
                source_id: source.source_id.clone(),
                status: "index_unreadable".to_string(),
                rows_examined: 0,
                truncated: false,
            });
            continue;
        };
        let Ok(rows) = statement.query_map([], Row::from_query_row) else {
            scan.sources.push(ConflictSource {
                source_id: source.source_id.clone(),
                status: "index_unreadable".to_string(),
                rows_examined: 0,
                truncated: false,
            });
            continue;
        };
        let mut examined = 0usize;
        let mut truncated = false;
        for row in rows {
            let Ok(row) = row else {
                continue;
            };
            if examined >= MAX_UNITS_PER_SOURCE {
                truncated = true;
                break;
            }
            examined += 1;
            if row.origin == "archive" && !archive_source_configured(config) {
                // An archive-fallback row is readable through the live source
                // when the source still answers; a ref that cannot be read back
                // is not a conflict anchor.
                continue;
            }
            if let Some(since) = since_ms {
                if row.ts < since {
                    continue;
                }
            }
            let text = row.text.as_str();
            if text.trim().is_empty() {
                continue;
            }
            let reference = row.build_ref(source);
            let hit = crate::recall::refs::hit_id(&reference);
            for (segment, exclusion) in classify_unit(text, &mut scan.excluded) {
                if exclusion.is_some() {
                    continue;
                }
                for anchor in anchors_of(segment.text) {
                    if anchor_quoted(segment.text, anchor.offset) {
                        *scan
                            .excluded
                            .entry(EXCLUSION_QUOTATION)
                            .or_insert(0) += 1;
                        continue;
                    }
                    let Some(polarity) = adjacency_polarity(segment.text, anchor.offset) else {
                        continue;
                    };
                    let (snippet, _) =
                        crate::recall::text::snippet(text, segment.offset + anchor.offset, anchor.len, 200);
                    let key = normalize(&anchor.text);
                    if key.is_empty() {
                        continue;
                    }
                    occurrences.push(Occurrence {
                        polarity,
                        display: anchor.text.clone(),
                        anchor: key,
                        reference: reference.clone(),
                        hit_id: hit.clone(),
                        snippet,
                        time: rfc3339_ms(row.ts),
                        time_ms: row.ts,
                        session_id: row.session_id.clone(),
                        session_kind: row.session_kind.clone(),
                        source_id: source.source_id.clone(),
                        title: row.title.clone(),
                        origin: row.origin.clone(),
                        rowid: row.rowid,
                    });
                }
            }
        }
        scan.sources.push(ConflictSource {
            source_id: source.source_id.clone(),
            status: "scanned".to_string(),
            rows_examined: examined,
            truncated,
        });
    }

    scan.pairs = pair_occurrences(occurrences, limit);
    scan
}

fn is_session_source(source: &SourceConfig) -> bool {
    use crate::recall::config::SourceKind;
    matches!(
        source.kind,
        SourceKind::Opencode
            | SourceKind::Codex
            | SourceKind::ClaudeCode
            | SourceKind::Dsh
            | SourceKind::Hermes
    )
}

fn archive_source_configured(config: &RecallConfig) -> bool {
    config
        .sources
        .iter()
        .any(|source| matches!(source.kind, crate::recall::config::SourceKind::Archive))
}

fn rfc3339_ms(ts: i64) -> Option<String> {
    if ts <= 0 {
        return None;
    }
    DateTime::from_timestamp_millis(ts)
        .map(|stamp| stamp.to_rfc3339_opts(SecondsFormat::Millis, true))
}

// ---------------------------------------------------------------------------
// Classification
// ---------------------------------------------------------------------------

/// Split one unit into sentence-like segments and classify the exclusion.
/// The returned iterator yields every segment; `Some(code)` means excluded.
fn classify_unit<'a>(
    text: &'a str,
    excluded: &mut BTreeMap<&'static str, usize>,
) -> Vec<(Segment<'a>, Option<&'static str>)> {
    let mut out = Vec::new();
    for segment in split_segments(text) {
        let exclusion = exclusion_of(text, &segment);
        if let Some(code) = exclusion {
            // Count only exclusions that actually suppressed an identifier
            // occurrence: a question without an anchor is not a screening event.
            if !anchors_of(segment.text).is_empty() {
                *excluded.entry(code).or_insert(0) += 1;
            }
        }
        out.push((segment, exclusion));
    }
    out
}

fn split_segments(text: &str) -> Vec<Segment<'_>> {
    let mut segments = Vec::new();
    let mut start = 0usize;
    for (index, ch) in text.char_indices() {
        if matches!(ch, '\n' | '。' | '！' | '？' | '!' | '?' | ';' | '；') {
            let end = index + ch.len_utf8();
            push_segment(text, start, index, &mut segments);
            start = end;
        }
    }
    push_segment(text, start, text.len(), &mut segments);
    segments
}

fn push_segment<'a>(text: &'a str, start: usize, end: usize, out: &mut Vec<Segment<'a>>) {
    if start >= end || end > text.len() {
        return;
    }
    let slice = &text[start..end];
    if slice.trim().is_empty() {
        return;
    }
    out.push(Segment {
        text: slice,
        offset: start,
    });
}

fn exclusion_of(text: &str, segment: &Segment<'_>) -> Option<&'static str> {
    if is_blockquote(text, segment) {
        return Some(EXCLUSION_QUOTATION);
    }
    if is_fully_quoted(segment.text) {
        return Some(EXCLUSION_QUOTATION);
    }
    let lowered = segment.text.to_ascii_lowercase();
    let negative = contains_any(&lowered, &NEGATION_WORDS);
    if is_question(segment.text, &lowered) {
        return Some(if negative {
            EXCLUSION_NEGATED_QUESTION
        } else {
            EXCLUSION_QUESTION
        });
    }
    if contains_any(&lowered, &CONDITIONAL_MARKERS) {
        return Some(EXCLUSION_CONDITIONAL);
    }
    if contains_any(&lowered, &EXAMPLE_MARKERS) {
        return Some(EXCLUSION_EXAMPLE);
    }
    if is_third_party(segment.text) {
        return Some(EXCLUSION_THIRD_PARTY);
    }
    None
}

fn is_question(original: &str, lowered: &str) -> bool {
    if original.contains('?') || original.contains('？') {
        return true;
    }
    if contains_any(lowered, &QUESTION_MARKERS) {
        return true;
    }
    let trimmed = original.trim_end_matches(|ch: char| {
        ch.is_whitespace() || matches!(ch, '。' | '！' | '!' | '；' | ';' | '，' | ',')
    });
    trimmed.ends_with('吗') || trimmed.ends_with('呢')
}

fn contains_any(haystack: &str, needles: &[&str]) -> bool {
    needles
        .iter()
        .any(|needle| contains_marker(haystack, needle))
}

/// ASCII markers need a word boundary (`use` must not match `user`); CJK
/// markers are plain substrings.
fn contains_marker(haystack: &str, needle: &str) -> bool {
    if !needle.is_ascii() {
        return haystack.contains(needle);
    }
    let bytes = haystack.as_bytes();
    let is_word = |byte: u8| byte.is_ascii_alphanumeric() || byte == b'_';
    let mut offset = 0usize;
    while let Some(found) = haystack[offset..].find(needle) {
        let start = offset + found;
        let end = start + needle.len();
        let before_ok = start == 0 || !is_word(bytes[start - 1]);
        let after_ok = end == bytes.len() || !is_word(bytes[end]);
        if before_ok && after_ok {
            return true;
        }
        offset = end;
    }
    false
}

fn is_blockquote(text: &str, segment: &Segment<'_>) -> bool {
    let line_start = text[..segment.offset].rfind('\n').map(|index| index + 1).unwrap_or(0);
    if line_start == segment.offset {
        return segment.text.trim_start().starts_with('>');
    }
    text[line_start..segment.offset].trim_start().starts_with('>')
}

/// A segment whose non-space content is one quoted span.
fn is_fully_quoted(segment: &str) -> bool {
    let trimmed = segment.trim();
    let pairs: [(char, char); 4] = [('「', '」'), ('『', '』'), ('“', '”'), ('"', '"')];
    for (open, close) in pairs {
        if let Some(rest) = trimmed.strip_prefix(open) {
            if let Some(inner) = rest.strip_suffix(close) {
                if !inner.is_empty() && !inner.contains(close) {
                    return true;
                }
            }
        }
    }
    false
}

fn is_third_party(segment: &str) -> bool {
    let trimmed = segment.trim_start();
    for (index, ch) in trimmed.char_indices() {
        if matches!(ch, '说' | '：' | ':') {
            let prefix = &trimmed[..index];
            if is_name_prefix(prefix) {
                return true;
            }
            return false;
        }
    }
    false
}

fn is_name_prefix(prefix: &str) -> bool {
    let prefix = prefix.trim();
    if prefix.is_empty() || prefix.chars().count() > 12 {
        return false;
    }
    if prefix
        .chars()
        .any(|ch| ch.is_ascii_digit() || matches!(ch, '.' | '-' | '_' | '/' | '\\'))
    {
        return false;
    }
    if prefix.chars().any(|ch| matches!(ch, '不' | '别' | '没' | '勿' | '未')) {
        return false;
    }
    // The speaker recounting their own words ("我刚才说", "我之前说") is not a
    // third party.
    if prefix.starts_with('我') || prefix.to_ascii_lowercase().starts_with("i ") {
        return false;
    }
    prefix
        .chars()
        .all(|ch| ch.is_alphanumeric() || ch.is_whitespace())
}

/// Polarity only when a usage verb ends within [`MAX_VERB_GAP`] characters
/// before the identifier and inside the same clause. 「不要改 X」「不要读 X」
/// therefore carry no polarity: `改`/`读` are not usage verbs.
fn adjacency_polarity(segment: &str, anchor_offset: usize) -> Option<&'static str> {
    let start = clause_start(segment, anchor_offset);
    let prefix = segment[start..anchor_offset].trim_end();
    if prefix.is_empty() {
        return None;
    }
    for marker in NEGATIVE_MARKERS {
        if marker_ends_within(prefix, marker) {
            return Some(POLARITY_NEGATIVE);
        }
    }
    for marker in AFFIRMATIVE_MARKERS {
        if marker_ends_within(prefix, marker) {
            return Some(POLARITY_AFFIRMATIVE);
        }
    }
    None
}

fn clause_start(segment: &str, offset: usize) -> usize {
    let head = &segment[..offset];
    let mut start = 0usize;
    for (index, ch) in head.char_indices() {
        if matches!(
            ch,
            '，' | ',' | '、' | '：' | ':' | '；' | ';' | '。' | '！' | '？' | '!' | '?' | '\n'
        ) {
            start = index + ch.len_utf8();
        }
    }
    start
}

fn marker_ends_within(prefix: &str, marker: &str) -> bool {
    // English markers match case-insensitively ("Use", "Stop using"); ASCII
    // lowercasing is a 1:1 byte mapping, so offsets stay valid in `prefix`.
    let haystack = prefix.to_ascii_lowercase();
    let needle = marker.to_ascii_lowercase();
    let mut offset = 0usize;
    while let Some(found) = haystack[offset..].find(&needle) {
        let start = offset + found;
        let end = start + needle.len();
        let gap = haystack[end..].chars().count();
        if gap <= MAX_VERB_GAP && ascii_marker_boundary(&haystack, start, marker) {
            return true;
        }
        offset = end;
    }
    false
}

fn ascii_marker_boundary(prefix: &str, start: usize, marker: &str) -> bool {
    if !marker.is_ascii() {
        return true;
    }
    match prefix[..start].chars().next_back() {
        None => true,
        Some(ch) => !(ch.is_ascii_alphanumeric() || ch == '_'),
    }
}

// ---------------------------------------------------------------------------
// Anchors
// ---------------------------------------------------------------------------

struct Anchor {
    text: String,
    offset: usize,
    len: usize,
}

fn anchor_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        Regex::new(r"[A-Za-z0-9][A-Za-z0-9_.:@\-]*[A-Za-z0-9]").expect("anchor regex")
    })
}

fn anchors_of(segment: &str) -> Vec<Anchor> {
    let mut anchors: Vec<Anchor> = Vec::new();
    for found in anchor_regex().find_iter(segment) {
        let value = found.as_str();
        if !is_anchor(value) {
            continue;
        }
        let normalized = normalize(value);
        if anchors
            .iter()
            .any(|existing| normalize(&existing.text) == normalized)
        {
            continue;
        }
        anchors.push(Anchor {
            text: value.to_string(),
            offset: found.start(),
            len: value.len(),
        });
    }
    anchors
}

/// Only concrete-object identifiers are anchors: a digit, `:` or `@` must be
/// present. Plain words (`Glob`, `Read`, `Users`, `Only`, `API`), all-uppercase
/// abbreviations (`SDS`, `UNCERTAIN`) and file names (`*.py`, `*.md`) are not.
fn is_anchor(value: &str) -> bool {
    let chars: Vec<char> = value.chars().collect();
    if chars.len() < 3 || !chars.iter().any(|ch| ch.is_ascii_alphabetic()) {
        return false;
    }
    let concrete = chars.iter().any(|ch| ch.is_ascii_digit()) || value.contains(':') || value.contains('@');
    if !concrete {
        return false;
    }
    let lowered = value.to_ascii_lowercase();
    if FILENAME_EXTENSIONS
        .iter()
        .any(|extension| lowered.ends_with(&format!(".{extension}")))
    {
        return false;
    }
    // All-uppercase abbreviations are labels, not concrete objects (a model id
    // such as `GLM-5.3` is deliberately not a conflict anchor).
    if !chars.iter().any(|ch| ch.is_ascii_lowercase()) && !value.contains(':') && !value.contains('@')
    {
        return false;
    }
    true
}

/// True when the anchor starts inside a quoted span of the segment.
fn anchor_quoted(segment: &str, offset: usize) -> bool {
    let pairs: [(char, char); 4] = [('「', '」'), ('『', '』'), ('“', '”'), ('"', '"')];
    for (open, close) in pairs {
        let mut depth = false;
        let mut start = 0usize;
        for (index, ch) in segment.char_indices() {
            if ch == open {
                if open == close && depth {
                    // A closing straight quote.
                    if offset > start && offset < index {
                        return true;
                    }
                    depth = false;
                } else if !depth {
                    start = index;
                    depth = true;
                }
            } else if depth && ch == close {
                if offset > start && offset < index {
                    return true;
                }
                depth = false;
            }
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Pairing
// ---------------------------------------------------------------------------

fn pair_occurrences(mut occurrences: Vec<Occurrence>, limit: Option<usize>) -> Vec<ConflictPair> {
    occurrences.sort_by_key(|occurrence| {
        (
            occurrence.anchor.clone(),
            occurrence.time_ms,
            occurrence.rowid,
        )
    });
    let mut grouped: BTreeMap<String, (Vec<Occurrence>, Vec<Occurrence>)> = BTreeMap::new();
    for occurrence in occurrences {
        let entry = grouped.entry(occurrence.anchor.clone()).or_default();
        if occurrence.polarity == POLARITY_AFFIRMATIVE {
            entry.0.push(occurrence);
        } else {
            entry.1.push(occurrence);
        }
    }

    let mut pairs: Vec<ConflictPair> = Vec::new();
    for (anchor, (affirmatives, negatives)) in grouped {
        if affirmatives.is_empty() || negatives.is_empty() {
            continue;
        }
        // One group per identifier: the newest affirmative and the newest
        // negative. The two sides must come from different times; a same-turn
        // self-correction is not a conflict, so the newest same-time pair is
        // skipped in favour of the newest occurrences with distinct times.
        let Some(newest_aff) = affirmatives
            .iter()
            .max_by_key(|candidate| (candidate.time_ms, candidate.rowid))
        else {
            continue;
        };
        let Some(newest_neg) = negatives
            .iter()
            .max_by_key(|candidate| (candidate.time_ms, candidate.rowid))
        else {
            continue;
        };
        let (affirmative, negative) = if newest_aff.time_ms != newest_neg.time_ms {
            (newest_aff, newest_neg)
        } else {
            let alternative_affirmative = affirmatives
                .iter()
                .rev()
                .find(|candidate| candidate.time_ms != newest_neg.time_ms);
            let alternative_negative = negatives
                .iter()
                .rev()
                .find(|candidate| candidate.time_ms != newest_aff.time_ms);
            match (alternative_affirmative, alternative_negative) {
                (Some(affirmative), Some(negative)) => (affirmative, negative),
                _ => continue,
            }
        };
        if affirmative.time_ms == negative.time_ms {
            continue;
        }
        pairs.push(ConflictPair {
            identifier: negative.display.clone(),
            anchor: anchor.clone(),
            status: STATUS_NEEDS_REVIEW,
            affirmative: to_public(affirmative),
            negative: to_public(negative),
            affirmative_count: affirmatives.len(),
            negative_count: negatives.len(),
        });
    }

    pairs.sort_by(|left, right| {
        let left_time = left
            .affirmative
            .time_ms
            .max(left.negative.time_ms);
        let right_time = right
            .affirmative
            .time_ms
            .max(right.negative.time_ms);
        right_time
            .cmp(&left_time)
            .then_with(|| left.anchor.cmp(&right.anchor))
    });
    if let Some(limit) = limit {
        pairs.truncate(limit);
    }
    pairs
}

fn to_public(occurrence: &Occurrence) -> ConflictOccurrence {
    ConflictOccurrence {
        polarity: occurrence.polarity,
        reference: occurrence.reference.clone(),
        hit_id: occurrence.hit_id.clone(),
        snippet: occurrence.snippet.clone(),
        time: occurrence.time.clone(),
        time_ms: occurrence.time_ms,
        session_id: occurrence.session_id.clone(),
        session_kind: occurrence.session_kind.clone(),
        source_id: occurrence.source_id.clone(),
        title: occurrence.title.clone(),
        origin: occurrence.origin.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keep(text: &str) -> bool {
        let mut excluded = BTreeMap::new();
        let segments = classify_unit(text, &mut excluded);
        assert_eq!(segments.len(), 1, "one segment expected for {text:?}");
        segments[0].1.is_none()
    }

    fn exclusion(text: &str) -> Option<&'static str> {
        let mut excluded = BTreeMap::new();
        let segments = classify_unit(text, &mut excluded);
        segments[0].1
    }

    fn polarity(text: &str) -> Option<&'static str> {
        let anchors = anchors_of(text);
        let first = anchors.first()?;
        adjacency_polarity(text, first.offset)
    }

    #[test]
    fn polarity_is_bound_to_the_identifier_adjacency() {
        assert_eq!(polarity("默认用 qwen3.5:9b"), Some(POLARITY_AFFIRMATIVE));
        assert_eq!(polarity("不要用 qwen3.5:9b"), Some(POLARITY_NEGATIVE));
        assert_eq!(polarity("不用 qwen3.5:9b"), Some(POLARITY_NEGATIVE));
        assert_eq!(polarity("不要使用 qwen3.5:9b"), Some(POLARITY_NEGATIVE));
        assert_eq!(polarity("不使用 qwen3.5:9b"), Some(POLARITY_NEGATIVE));
        assert_eq!(polarity("不得使用 qwen3.5:9b"), Some(POLARITY_NEGATIVE));
        assert_eq!(polarity("禁止使用 qwen3.5:9b"), Some(POLARITY_NEGATIVE));
        assert_eq!(polarity("停止使用 qwen3.5:9b"), Some(POLARITY_NEGATIVE));
        assert_eq!(polarity("不再使用 qwen3.5:9b"), Some(POLARITY_NEGATIVE));
        assert_eq!(polarity("don't use qwen3.5:9b"), Some(POLARITY_NEGATIVE));
        assert_eq!(polarity("stop using qwen3.5:9b"), Some(POLARITY_NEGATIVE));
        assert_eq!(polarity("改用 qwen3.5:9b"), Some(POLARITY_AFFIRMATIVE));
        assert_eq!(polarity("换成 qwen3.5:9b"), Some(POLARITY_AFFIRMATIVE));
        assert_eq!(polarity("用一下 qwen3.5:9b"), Some(POLARITY_AFFIRMATIVE));
        assert_eq!(polarity("please use qwen3.5:9b"), Some(POLARITY_AFFIRMATIVE));
        // Non-usage verbs carry no polarity.
        assert_eq!(polarity("不要改 qwen3.5:9b"), None);
        assert_eq!(polarity("不要读 qwen3.5:9b"), None);
        // The verb must be near the identifier: a `不要` elsewhere does not pair.
        assert_eq!(
            polarity("不要在这个项目里随手换成别的方案，qwen3.5:9b 仍然保留"),
            None
        );
        assert_eq!(polarity("qwen3.5:9b 相关笔记"), None);
        // ASCII markers need word boundaries: `use` is not `user`.
        assert_eq!(polarity("user profile mentions qwen3.5:9b"), None);
    }

    #[test]
    fn exclusions_have_a_positive_and_a_negative_case() {
        // negated question
        assert_eq!(
            exclusion("要不要用 qwen3.5:9b？"),
            Some(EXCLUSION_NEGATED_QUESTION)
        );
        assert_eq!(exclusion("不要用 qwen3.5:9b"), None);
        // question
        assert_eq!(exclusion("用 qwen3.5:9b 吗"), Some(EXCLUSION_QUESTION));
        assert_eq!(exclusion("用 qwen3.5:9b 就行"), None);
        // conditional
        assert_eq!(
            exclusion("如果要用 qwen3.5:9b 就先测"),
            Some(EXCLUSION_CONDITIONAL)
        );
        assert_eq!(exclusion("用 qwen3.5:9b"), None);
        // example
        assert_eq!(
            exclusion("比如用 qwen3.5:9b"),
            Some(EXCLUSION_EXAMPLE)
        );
        assert_eq!(exclusion("用 qwen3.5:9b"), None);
        // quotation
        assert_eq!(
            exclusion("「不要用 qwen3.5:9b」"),
            Some(EXCLUSION_QUOTATION)
        );
        assert!(anchor_quoted("用户说「不要用 qwen3.5:9b」", 12));
        assert!(!anchor_quoted("不要用 qwen3.5:9b", 8));
        // third party
        assert_eq!(
            exclusion("他说不要用 qwen3.5:9b"),
            Some(EXCLUSION_THIRD_PARTY)
        );
        assert_eq!(exclusion("某某：不要用 qwen3.5:9b"), Some(EXCLUSION_THIRD_PARTY));
        assert_eq!(exclusion("不要用 qwen3.5:9b"), None);
    }

    #[test]
    fn keep_is_true_for_a_plain_prohibition() {
        assert!(keep("不要用 qwen3.5:9b"));
        assert!(keep("默认用 qwen3.5:9b"));
        assert!(!keep("> 不要用 qwen3.5:9b"));
    }

    #[test]
    fn anchors_are_concrete_objects_only() {
        let anchors = anchors_of(
            "默认用 qwen3.5:9b、deepseek-v4-flash、gpt-5.5，用 Glob 搜 Read 和 Users，\
             规则见 UNCERTAIN 与 sync_agents.py 以及 API",
        );
        let texts: Vec<&str> = anchors.iter().map(|anchor| anchor.text.as_str()).collect();
        assert!(texts.contains(&"qwen3.5:9b"));
        assert!(texts.contains(&"deepseek-v4-flash"));
        assert!(texts.contains(&"gpt-5.5"));
        for rejected in [
            "Glob",
            "Read",
            "Users",
            "Only",
            "API",
            "UNCERTAIN",
            "sync_agents.py",
            "use",
        ] {
            assert!(
                !texts.contains(&rejected),
                "{rejected} must not be an anchor: {texts:?}"
            );
        }
        // An all-uppercase model token is also a label, not an anchor.
        assert!(!is_anchor("GLM-5.3"));
        assert!(is_anchor("user@host"));
    }

    #[test]
    fn pairing_keeps_one_group_per_identifier_with_counts() {
        let occurrence = |polarity, rowid, ts| Occurrence {
            polarity,
            display: "qwen3.5:9b".to_string(),
            anchor: "qwen359b".to_string(),
            reference: crate::recall::refs::HitRef {
                source_id: "s".to_string(),
                source_kind: "claude_code".to_string(),
                root_sha256: "0".to_string(),
                rel_path: "p.jsonl".to_string(),
                schema_gen: "claude-jsonl-v1".to_string(),
                locator: crate::recall::refs::Locator::empty(),
                fingerprint: crate::recall::refs::Fingerprint {
                    range_sha256: "0".to_string(),
                    file_sha256: None,
                    row_sha256: None,
                },
                text_range: crate::recall::hash::TextRange::new(0, 0, 0, 0),
            },
            hit_id: format!("hit-{rowid}"),
            snippet: String::new(),
            time: None,
            time_ms: ts,
            session_id: "s".to_string(),
            session_kind: "interactive".to_string(),
            source_id: "s".to_string(),
            title: String::new(),
            origin: "live".to_string(),
            rowid,
        };
        let pairs = pair_occurrences(
            vec![
                occurrence(POLARITY_AFFIRMATIVE, 1, 100),
                occurrence(POLARITY_AFFIRMATIVE, 2, 300),
                occurrence(POLARITY_NEGATIVE, 3, 400),
                occurrence(POLARITY_NEGATIVE, 4, 200),
            ],
            None,
        );
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].affirmative.hit_id, "hit-2");
        assert_eq!(pairs[0].negative.hit_id, "hit-3");
        assert_eq!(pairs[0].affirmative_count, 2);
        assert_eq!(pairs[0].negative_count, 2);

        // Same-time sides are a same-turn self-correction, not a conflict.
        let pairs = pair_occurrences(
            vec![
                occurrence(POLARITY_AFFIRMATIVE, 5, 500),
                occurrence(POLARITY_NEGATIVE, 6, 500),
            ],
            None,
        );
        assert!(pairs.is_empty());
    }
}

#[cfg(test)]
mod case_and_first_person_tests {
    use super::*;

    #[test]
    fn english_markers_match_regardless_of_case() {
        assert!(marker_ends_within("Use ", "use"));
        assert!(marker_ends_within("Stop using ", "stop using"));
        assert!(marker_ends_within("Switch to ", "switch to"));
        assert!(!marker_ends_within("Reuse ", "use"), "word boundary still applies");
    }

    #[test]
    fn recounting_ones_own_words_is_not_a_third_party() {
        assert!(!is_third_party("我刚才说不要用 qwen3.5:9b"));
        assert!(!is_third_party("我之前说：默认用 qwen3.5:9b"));
        assert!(is_third_party("张三说不要用 qwen3.5:9b"));
    }
}
