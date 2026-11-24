//! Stage 7 §B: mechanical key-item extraction and equivalence forms.
//!
//! Version 1 understands exactly two mechanical families:
//!
//! * numbers — dates (`2026-11-07`, `2026年11月7日`, `2026/11/7`), versions
//!   (`v0.3`, `V1.2`), plain numbers and numbers with a unit (`16 MiB`);
//! * ASCII identifiers — at least three characters, containing a digit,
//!   underscore, hyphen, dot or an internal capital (`memory_recent`,
//!   `GLM-5.3`).
//!
//! CJK proper nouns are deliberately **not** extracted: they need a lexicon and
//! the false-positive rate is uncontrollable (see the plan's open items).
//!
//! # Normalization and equivalence
//!
//! There is exactly one normalizer: [`crate::learning::qualify::normalize_for_rejection`]
//! (NFKC, drop whitespace/punctuation/symbols, ASCII lower-case). This module
//! never re-implements it. Equivalence classes are produced by generating the
//! accepted *spellings* of an item and normalizing each spelling:
//!
//! * dates: `2026-11-07`, `2026-11-7`, `2026年11月7日`, `2026/11/07`,
//!   `2026.11.7` and the digit-only `20261107` all normalize to the same value;
//! * numbers: thousands separators are dropped by the normalizer
//!   (`1,129` = `1129`);
//! * identifiers: ASCII lower-case and full/half width fold in the normalizer.
//!
//! A derived item is *grounded* when any of its normalized forms is a substring
//! of the normalized source text. There is no semantic matching, no model call
//! and no approximate matching in this decision; approximate string similarity
//! is used only by the read-only correction report.

use std::sync::OnceLock;

use regex::Regex;
use serde::Serialize;
use unicode_normalization::UnicodeNormalization;

use crate::learning::qualify::{jaccard_similarity, normalize_for_rejection};

pub const KIND_DATE: &str = "date";
pub const KIND_VERSION: &str = "version";
pub const KIND_NUMBER: &str = "number";
/// A small integer plus a counting quantifier: the summary's own count, which
/// the original text cannot be expected to contain. Disclosed separately and
/// never counted as an ungrounded item.
pub const KIND_COUNT: &str = "count";
pub const KIND_IDENTIFIER: &str = "identifier";

/// At most this many nearest source items are reported per ungrounded item.
pub const CORRECTION_CANDIDATES: usize = 3;

/// Units that may follow a number and become part of the extracted item. Only
/// unambiguous, multi-character units are listed: a bare `m`/`s`/`in` would
/// swallow ordinary prose.
const ASCII_UNITS: [&str; 28] = [
    "bytes", "byte", "kib", "mib", "gib", "tib", "kb", "mb", "gb", "tb", "ms", "sec", "secs",
    "second", "seconds", "min", "mins", "minute", "minutes", "hour", "hours", "hr", "hrs", "day",
    "days", "px", "chars", "lines",
];

/// Trailing Chinese quantity / unit words stripped from a number before
/// matching its numeric core. Longest first so `方面` wins over `方`.
const CJK_UNIT_WORDS: [&str; 23] = [
    "方面", "个", "项", "条", "篇", "名", "位", "人", "次", "元", "万", "亿", "倍", "年", "月",
    "日", "号", "类", "点", "步", "种", "字", "天",
];

/// A small integer plus one of these is the summary author's own count
/// ("指出了5个问题"), not a restatement of the original text.
const COUNT_UNITS: [&str; 8] = ["个", "项", "条", "类", "点", "步", "种", "方面"];

/// The largest integer treated as an author's count.
const COUNT_MAX: u32 = 20;

/// One extracted key item and where it occurred in the derived text.
#[derive(Debug, Clone, Serialize)]
pub struct KeyItem {
    pub kind: &'static str,
    pub text: String,
    /// Byte offset in the derived text.
    pub offset: usize,
    /// 1-based line number in the derived text.
    pub line: usize,
    /// Normalized equivalence forms (never serialized: internal matching data).
    #[serde(skip)]
    pub forms: Vec<String>,
    /// Numeric core without a trailing Chinese unit, when the item is numeric.
    #[serde(skip)]
    pub core_form: Option<String>,
    /// Full `万`/`亿` spelling, when the item carries such a magnitude unit.
    #[serde(skip)]
    pub magnitude_form: Option<String>,
}

/// One candidate correction for an ungrounded item: the most similar key item
/// found in the source text. Similarity is character 3-gram Jaccard over the
/// §A normalization; nothing here is semantic.
#[derive(Debug, Clone, Serialize)]
pub struct CorrectionCandidate {
    pub kind: &'static str,
    pub text: String,
    pub offset: usize,
    pub line: usize,
    pub similarity: f64,
}

#[derive(Debug, Clone, Copy)]
struct Candidate {
    start: usize,
    end: usize,
    priority: u8,
    kind: &'static str,
}

/// The one normalization used for grounding as well.
pub fn normalize(value: &str) -> String {
    normalize_for_rejection(value)
}

/// Extract every v1 key item of `text`, in document order.
///
/// Extraction runs on an NFKC projection of the text so full-width spellings
/// (`２０２６－１１－０７`, `ＭＥＭＯＲＹ＿ＲＥＣＥＮＴ`) are the same item as
/// their ASCII spellings. Every reported offset is mapped back to the original
/// text, so a location always addresses the caller's bytes.
pub fn extract_items(text: &str) -> Vec<KeyItem> {
    let folded = NfkcText::new(text);
    let lines = LineIndex::new(text);
    let normalized = folded.text.as_str();
    let mut candidates: Vec<Candidate> = Vec::new();

    for regex in date_regexes() {
        for captures in regex.captures_iter(normalized) {
            let Some(whole) = captures.get(0) else {
                continue;
            };
            // An impossible month/day is not this item's date: let the number
            // candidate handle the digits instead of asserting a false class.
            if date_forms_from(&captures).is_none() {
                continue;
            }
            candidates.push(Candidate {
                start: whole.start(),
                end: whole.end(),
                priority: 0,
                kind: KIND_DATE,
            });
        }
    }
    for captures in month_day_regex().captures_iter(normalized) {
        let Some(whole) = captures.get(0) else {
            continue;
        };
        if month_day_forms_from(&captures).is_none() {
            continue;
        }
        candidates.push(Candidate {
            start: whole.start(),
            end: whole.end(),
            priority: 0,
            kind: KIND_DATE,
        });
    }
    for captures in version_regex().captures_iter(normalized) {
        let Some(whole) = captures.get(0) else {
            continue;
        };
        candidates.push(Candidate {
            start: whole.start(),
            end: whole.end(),
            priority: 1,
            kind: KIND_VERSION,
        });
    }
    for captures in identifier_regex().captures_iter(normalized) {
        let Some(whole) = captures.get(0) else {
            continue;
        };
        let value = whole.as_str();
        if is_identifier(value) {
            candidates.push(Candidate {
                start: whole.start(),
                end: whole.end(),
                priority: 2,
                kind: KIND_IDENTIFIER,
            });
        }
    }
    for captures in number_regex().captures_iter(normalized) {
        let Some(whole) = captures.get(0) else {
            continue;
        };
        let mut end = whole.end();
        if let Some(unit_end) = unit_end(normalized, end) {
            end = unit_end;
        }
        candidates.push(Candidate {
            start: whole.start(),
            end,
            priority: 3,
            kind: KIND_NUMBER,
        });
    }

    candidates.sort_by_key(|candidate| (candidate.start, candidate.priority));
    let mut selected: Vec<Candidate> = Vec::new();
    let mut spans: Vec<(usize, usize)> = Vec::new();
    for candidate in candidates {
        if spans
            .iter()
            .any(|(start, end)| candidate.start < *end && *start < candidate.end)
        {
            continue;
        }
        spans.push((candidate.start, candidate.end));
        selected.push(candidate);
    }

    selected
        .into_iter()
        .map(|candidate| {
            let text = normalized[candidate.start..candidate.end].to_string();
            let offset = folded.original_offset(candidate.start);
            let (kind, core_form, magnitude_form) = if candidate.kind == KIND_NUMBER {
                classify_number(&text)
            } else {
                (candidate.kind, None, None)
            };
            KeyItem {
                kind,
                text: text.clone(),
                offset,
                line: lines.line_of(offset),
                forms: equivalence_forms(kind, &text),
                core_form,
                magnitude_form,
            }
        })
        .collect()
}

/// An NFKC projection plus a normalized-byte -> original-byte offset map.
struct NfkcText {
    text: String,
    /// `(normalized byte start, original byte start)`, ascending.
    map: Vec<(usize, usize)>,
}

impl NfkcText {
    fn new(original: &str) -> Self {
        let mut text = String::with_capacity(original.len());
        let mut map = Vec::new();
        for (original_byte, ch) in original.char_indices() {
            for folded in ch.nfkc() {
                map.push((text.len(), original_byte));
                text.push(folded);
            }
        }
        Self { text, map }
    }

    fn original_offset(&self, normalized_byte: usize) -> usize {
        let index = self
            .map
            .partition_point(|(start, _)| *start < normalized_byte);
        match index.checked_sub(1).and_then(|index| self.map.get(index)) {
            Some((_, original)) => *original,
            None => 0,
        }
    }
}

/// The normalized equivalence forms of one item spelling.
///
/// Numbers and counts are matched by their **numeric core**: a trailing
/// Chinese quantity/unit word (`个`, `项`, `篇`, `年`, `%`, …) is dropped, so
/// 「约385个样本」 is grounded by an original that only says `385`. A Latin unit
/// (`16 MiB`, `285MB`) stays part of the item: a rounded rewrite such as
/// `285MB` versus `285.52 MB` remains a real ungrounded item.
pub fn equivalence_forms(kind: &str, text: &str) -> Vec<String> {
    let mut raw: Vec<String> = Vec::new();
    if kind == KIND_NUMBER || kind == KIND_COUNT {
        let core = numeric_core(text);
        raw.push(core.clone());
        if core != text.trim() {
            // Keep the exact magnitude spelling as an additional form; the
            // numeric core remains the fallback.
            raw.push(text.trim().to_string());
        }
    } else if kind == KIND_DATE {
        if let Some(mut forms) = date_forms_from_text(text) {
            raw.append(&mut forms);
        } else if let Some(mut forms) = month_day_forms_from_text(text) {
            raw.append(&mut forms);
        } else if is_year_only(text) {
            // `2024年` is equivalent to any date spelling containing 2024.
            raw.push(numeric_core(text));
        } else {
            raw.push(text.to_string());
        }
    } else {
        raw.push(text.to_string());
    }
    let mut forms: Vec<String> = Vec::new();
    for spelling in raw {
        let normalized = normalize(&spelling);
        if !normalized.is_empty() && !forms.contains(&normalized) {
            forms.push(normalized);
        }
    }
    forms
}

/// Classify one numeric spelling: `count` (small integer + counting
/// quantifier), `date` (a bare `YYYY年`), or `number`; plus the numeric core
/// and the full `万`/`亿` spelling.
fn classify_number(text: &str) -> (&'static str, Option<String>, Option<String>) {
    let core = numeric_core(text);
    let core_form = Some(normalize(&core));
    let magnitude = has_magnitude_unit(text).then(|| normalize(text.trim()));
    let unit = trailing_cjk_unit(text);
    let kind = match unit {
        Some(unit) if COUNT_UNITS.contains(&unit) && is_small_integer(&core) => KIND_COUNT,
        Some("年") if is_four_digit_year(&core) => KIND_DATE,
        _ => KIND_NUMBER,
    };
    (kind, core_form, magnitude)
}

/// The numeric core: the spelling with one trailing `%`/`‰` and any trailing
/// Chinese quantity/unit word removed.
pub fn numeric_core(text: &str) -> String {
    let mut core = text.trim().to_string();
    for _ in 0..2 {
        let trimmed = core
            .trim_end_matches(['%', '‰', ' ', '\t'])
            .trim_end()
            .to_string();
        core = trimmed;
        let Some(unit) = trailing_cjk_unit(&core) else {
            break;
        };
        core.truncate(core.len() - unit.len());
        core = core.trim_end().to_string();
    }
    core
}

fn trailing_cjk_unit(text: &str) -> Option<&'static str> {
    let trimmed = text.trim_end();
    CJK_UNIT_WORDS
        .iter()
        .find(|unit| trimmed.ends_with(**unit))
        .copied()
}

fn has_magnitude_unit(text: &str) -> bool {
    matches!(trailing_cjk_unit(text), Some("万") | Some("亿"))
}

fn is_small_integer(core: &str) -> bool {
    let digits: String = core.chars().filter(|ch| !matches!(ch, ',' | '，')).collect();
    if digits.is_empty() || !digits.chars().all(|ch| ch.is_ascii_digit()) {
        return false;
    }
    digits
        .parse::<u32>()
        .map(|value| (1..=COUNT_MAX).contains(&value))
        .unwrap_or(false)
}

fn is_four_digit_year(core: &str) -> bool {
    let digits: String = core.chars().filter(|ch| !matches!(ch, ',' | '，')).collect();
    digits.len() == 4 && digits.chars().all(|ch| ch.is_ascii_digit())
}

/// True when a grounded item was matched only by its numeric core because the
/// `万`/`亿` magnitude spelling does not occur in the source.
pub fn unit_was_dropped(item: &KeyItem, normalized_source: &str) -> bool {
    match (&item.magnitude_form, &item.core_form) {
        (Some(magnitude), Some(core)) => {
            magnitude != core
                && !normalized_source.contains(magnitude.as_str())
                && normalized_source.contains(core.as_str())
        }
        _ => false,
    }
}

/// True when the item is grounded in the already-normalized source text.
pub fn is_grounded(forms: &[String], normalized_source: &str) -> bool {
    forms
        .iter()
        .any(|form| !form.is_empty() && normalized_source.contains(form.as_str()))
}

/// The nearest source items for one ungrounded item (string similarity only).
pub fn corrections(
    item: &KeyItem,
    source_items: &[KeyItem],
    limit: usize,
) -> Vec<CorrectionCandidate> {
    let needle = normalize(&item.text);
    if needle.is_empty() {
        return Vec::new();
    }
    let mut scored: Vec<CorrectionCandidate> = source_items
        .iter()
        .filter_map(|candidate| {
            let haystack = normalize(&candidate.text);
            if haystack.is_empty() {
                return None;
            }
            let similarity = jaccard_similarity(&needle, &haystack);
            if similarity <= 0.0 {
                return None;
            }
            Some(CorrectionCandidate {
                kind: candidate.kind,
                text: candidate.text.clone(),
                offset: candidate.offset,
                line: candidate.line,
                similarity,
            })
        })
        .collect();
    scored.sort_by(|left, right| {
        right
            .similarity
            .partial_cmp(&left.similarity)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| left.offset.cmp(&right.offset))
    });
    scored.truncate(limit);
    scored
}

// ---------------------------------------------------------------------------
// Line index
// ---------------------------------------------------------------------------

struct LineIndex {
    newlines: Vec<usize>,
}

impl LineIndex {
    fn new(text: &str) -> Self {
        Self {
            newlines: text
                .bytes()
                .enumerate()
                .filter_map(|(index, byte)| (byte == b'\n').then_some(index))
                .collect(),
        }
    }

    fn line_of(&self, offset: usize) -> usize {
        self.newlines.partition_point(|newline| *newline < offset) + 1
    }
}

// ---------------------------------------------------------------------------
// Regexes and shapes
// ---------------------------------------------------------------------------

fn date_regexes() -> &'static [Regex; 2] {
    static REGEXES: OnceLock<[Regex; 2]> = OnceLock::new();
    REGEXES.get_or_init(|| {
        [
            Regex::new(r"(\d{4})\s*[-/.]\s*(\d{1,2})\s*[-/.]\s*(\d{1,2})").expect("date regex"),
            Regex::new(r"(\d{4})\s*年\s*(\d{1,2})\s*月\s*(\d{1,2})\s*日").expect("cjk date regex"),
        ]
    })
}

fn month_day_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        Regex::new(r"(\d{1,2})\s*月\s*(\d{1,2})\s*日").expect("month-day regex")
    })
}

fn version_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| Regex::new(r"(?i)\bv\d+(?:\.\d+)+\b").expect("version regex"))
}

fn identifier_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        Regex::new(r"[A-Za-z0-9][A-Za-z0-9_.\-]*").expect("identifier regex")
    })
}

fn number_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        Regex::new(r"\d+(?:[,，]\d{3})*(?:\.\d+)?[%‰]?").expect("number regex")
    })
}

fn is_identifier(value: &str) -> bool {
    let chars: Vec<char> = value.chars().collect();
    if chars.len() < 3 {
        return false;
    }
    if !chars.iter().any(char::is_ascii_alphabetic) {
        return false;
    }
    let special = chars
        .iter()
        .any(|ch| ch.is_ascii_digit() || matches!(ch, '_' | '-' | '.'));
    let internal_capital = chars
        .iter()
        .skip(1)
        .any(|ch| ch.is_ascii_uppercase());
    special || internal_capital
}

fn date_forms_from(captures: &regex::Captures<'_>) -> Option<Vec<String>> {
    let year = captures.get(1)?.as_str();
    let month: u32 = captures.get(2)?.as_str().parse().ok()?;
    let day: u32 = captures.get(3)?.as_str().parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    Some(date_forms(year, month, day))
}

fn date_forms_from_text(text: &str) -> Option<Vec<String>> {
    for regex in date_regexes() {
        if let Some(captures) = regex.captures(text) {
            return date_forms_from(&captures);
        }
    }
    None
}

fn month_day_forms_from(captures: &regex::Captures<'_>) -> Option<Vec<String>> {
    let month: u32 = captures.get(1)?.as_str().parse().ok()?;
    let day: u32 = captures.get(2)?.as_str().parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    Some(month_day_forms(month, day))
}

fn month_day_forms_from_text(text: &str) -> Option<Vec<String>> {
    let captures = month_day_regex().captures(text)?;
    month_day_forms_from(&captures)
}

/// `9月22日` is equivalent to `9.22`, `9/22`, `9-22` and `09-22` (no year).
fn month_day_forms(month: u32, day: u32) -> Vec<String> {
    let mut forms = Vec::new();
    let month_plain = month.to_string();
    let day_plain = day.to_string();
    let month_padded = format!("{month:02}");
    let day_padded = format!("{day:02}");
    for separator in ["-", "/", "."] {
        for (month, day) in [
            (&month_padded, &day_padded),
            (&month_plain, &day_plain),
            (&month_plain, &day_padded),
            (&month_padded, &day_plain),
        ] {
            forms.push(format!("{month}{separator}{day}"));
        }
    }
    for (month, day) in [
        (&month_plain, &day_plain),
        (&month_padded, &day_padded),
    ] {
        forms.push(format!("{month}月{day}日"));
    }
    forms.push(format!("{month_padded}{day_padded}"));
    forms.push(format!("{month_plain}{day_plain}"));
    for name in english_month_names(month) {
        forms.push(format!("{name}{day_plain}"));
        forms.push(format!("{day_plain}{name}"));
    }
    forms
}

/// Lowercase English month names (full, three-letter and `sept`) for `month`.
fn english_month_names(month: u32) -> Vec<&'static str> {
    const FULL: [&str; 12] = [
        "january", "february", "march", "april", "may", "june", "july", "august",
        "september", "october", "november", "december",
    ];
    const SHORT: [&str; 12] = [
        "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
    ];
    if !(1..=12).contains(&month) {
        return Vec::new();
    }
    let index = (month - 1) as usize;
    let mut names = vec![FULL[index]];
    if SHORT[index] != FULL[index] {
        names.push(SHORT[index]);
    }
    if month == 9 {
        names.push("sept");
    }
    names
}

fn is_year_only(text: &str) -> bool {
    trailing_cjk_unit(text) == Some("年") && is_four_digit_year(&numeric_core(text))
}

fn date_forms(year: &str, month: u32, day: u32) -> Vec<String> {
    let month_plain = month.to_string();
    let day_plain = day.to_string();
    let month_padded = format!("{month:02}");
    let day_padded = format!("{day:02}");
    let mut forms = Vec::new();
    for separator in ["-", "/", "."] {
        for (month, day) in [(&month_padded, &day_padded), (&month_plain, &day_plain)] {
            forms.push(format!("{year}{separator}{month}{separator}{day}"));
        }
    }
    for (month, day) in [(&month_plain, &day_plain), (&month_padded, &day_padded)] {
        forms.push(format!("{year}年{month}月{day}日"));
    }
    forms.push(format!("{year}{month_padded}{day_padded}"));
    for name in english_month_names(month) {
        forms.push(format!("{name}{day_plain}{year}"));
        forms.push(format!("{day_plain}{name}{year}"));
    }
    forms
}

/// Extend a number match over an immediately following unit.
fn unit_end(text: &str, number_end: usize) -> Option<usize> {
    let rest = &text[number_end..];
    if rest.is_empty() {
        return None;
    }
    // A CJK unit may follow immediately or after one space; the longest known
    // unit wins (`方面` before `方`).
    let cjk_rest = rest.strip_prefix(' ').unwrap_or(rest);
    for unit in CJK_UNIT_WORDS {
        if cjk_rest.starts_with(unit) {
            return Some(number_end + (rest.len() - cjk_rest.len()) + unit.len());
        }
    }
    let (space, token) = match rest.strip_prefix(' ') {
        Some(stripped) => (1usize, stripped),
        None => (0usize, rest),
    };
    let token_len = token
        .chars()
        .take_while(|ch| ch.is_ascii_alphabetic())
        .count();
    if token_len == 0 {
        return None;
    }
    let word: String = token.chars().take(token_len).collect();
    if !ASCII_UNITS.contains(&word.to_ascii_lowercase().as_str()) {
        return None;
    }
    // An adjacent unit of a single letter is too ambiguous (`3s`, `2m`).
    if space == 0 && token_len < 2 {
        return None;
    }
    Some(number_end + space + word.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(text: &str) -> Vec<(&'static str, String)> {
        extract_items(text)
            .into_iter()
            .map(|item| (item.kind, item.text))
            .collect()
    }

    #[test]
    fn extracts_dates_versions_numbers_and_identifiers() {
        let items = kinds("自 2026-11-07 起用 v0.3，占 16 MiB，约 3.5 倍；键 memory_recent、GLM-5.3。");
        assert!(items.contains(&(KIND_DATE, "2026-11-07".to_string())));
        assert!(items.contains(&(KIND_VERSION, "v0.3".to_string())));
        assert!(items.contains(&(KIND_NUMBER, "16 MiB".to_string())));
        assert!(items.contains(&(KIND_IDENTIFIER, "memory_recent".to_string())));
        assert!(items.contains(&(KIND_IDENTIFIER, "GLM-5.3".to_string())));
        // A plain lowercase word is not an identifier.
        assert!(!items.iter().any(|(_, text)| text == "自"));
    }

    #[test]
    fn overlapping_candidates_keep_the_more_specific_kind() {
        let items = extract_items("2026-11-07");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].kind, KIND_DATE);
        let items = extract_items("v0.3");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].kind, KIND_VERSION);
    }

    #[test]
    fn equivalence_forms_cover_dates_thousands_and_case() {
        let forms = equivalence_forms(KIND_DATE, "2026年11月7日");
        for spelling in ["2026-11-07", "2026-11-7", "2026/11/07", "2026年11月7日", "20261107"] {
            assert!(
                forms.contains(&normalize(spelling)),
                "missing equivalence form {spelling}: {forms:?}"
            );
        }
        let forms = equivalence_forms(KIND_NUMBER, "1,129");
        assert_eq!(forms, vec!["1129".to_string()]);
        let forms = equivalence_forms(KIND_IDENTIFIER, "GLM-5.3");
        assert_eq!(forms, vec!["glm53".to_string()]);
        let fullwidth = equivalence_forms(KIND_IDENTIFIER, "ＭＥＭＯＲＹ＿ＲＥＣＥＮＴ");
        assert_eq!(fullwidth, vec!["memoryrecent".to_string()]);
    }

    #[test]
    fn grounding_accepts_equivalents_and_rejects_a_missing_number() {
        let source = normalize("发布时间 2026年11月7日，共 1,129 篇，使用 GLM-5.3。");
        let derived = extract_items("2026-11-07 收录 1129 篇，模型 glm-5.3");
        for item in &derived {
            assert!(
                is_grounded(&item.forms, &source),
                "{} {:?} must be grounded",
                item.kind,
                item.text
            );
        }
        let derived = extract_items("2026-11-08 收录 1130 篇");
        assert_eq!(
            derived
                .iter()
                .filter(|item| !is_grounded(&item.forms, &source))
                .count(),
            2
        );
    }

    #[test]
    fn corrections_are_similarity_ranked_source_items() {
        let source_items = extract_items("原价 16 MiB，2026-11-06 发布");
        let item = extract_items("2026-11-07").remove(0);
        let candidates = corrections(&item, &source_items, CORRECTION_CANDIDATES);
        assert_eq!(candidates[0].text, "2026-11-06");
        assert!(candidates[0].similarity > 0.5);
    }

    #[test]
    fn full_half_width_spellings_are_the_same_item() {
        let items = extract_items("２０２６－１１－０７ 与 ＭＥＭＯＲＹ＿ＲＥＣＥＮＴ，１２９ 篇");
        let texts: Vec<&str> = items.iter().map(|item| item.text.as_str()).collect();
        assert!(texts.contains(&"2026-11-07"));
        assert!(texts.contains(&"MEMORY_RECENT"));
        assert!(texts.contains(&"129 篇"));
        let source = normalize("2026年11月7日 memory_recent 129 篇");
        for item in &items {
            assert!(
                is_grounded(&item.forms, &source),
                "{} {:?} must be grounded",
                item.kind,
                item.text
            );
        }
    }

    #[test]
    fn chinese_units_are_stripped_and_counts_are_separate() {
        let source = normalize("共 385 个样本；平均 1.593 Tokens；问题与资料列表");
        let items = extract_items("约385个样本，平均约1.593个 Tokens");
        for item in &items {
            assert!(
                is_grounded(&item.forms, &source),
                "{} {:?} must be grounded: {items:?}",
                item.kind,
                item.text
            );
        }
        let counts = extract_items("指出了5个问题，列出8项资料");
        let count_items: Vec<&KeyItem> = counts
            .iter()
            .filter(|item| item.kind == KIND_COUNT)
            .collect();
        assert_eq!(count_items.len(), 2, "{counts:?}");
        let count_source = normalize("问题与资料列表，没有数字");
        for item in &count_items {
            assert!(!is_grounded(&item.forms, &count_source));
        }
        // `次` is not one of the counting quantifiers: a real number remains.
        assert!(extract_items("跑了5次")
            .iter()
            .all(|item| item.kind == KIND_NUMBER));
    }

    #[test]
    fn year_and_month_day_are_equivalent_to_dated_spellings() {
        let source = normalize("2024.2.8 与 2023.12.18；9/22 与 09-22");
        let year = extract_items("2024年的技术栈");
        assert_eq!(year.len(), 1);
        assert_eq!(year[0].kind, KIND_DATE);
        assert!(is_grounded(&year[0].forms, &source), "{year:?}");
        let month_day = extract_items("9月22日");
        assert_eq!(month_day.len(), 1);
        assert_eq!(month_day[0].kind, KIND_DATE);
        assert!(is_grounded(&month_day[0].forms, &source), "{month_day:?}");
        // A different month/day is not grounded by an unrelated date.
        let other = extract_items("10月22日");
        assert!(!is_grounded(&other[0].forms, &normalize("9.22")));
    }

    #[test]
    fn superscripts_and_fullwidth_digits_use_the_same_projection_on_both_sides() {
        let items = extract_items("E=mc²，x³，全角 １２０");
        assert!(!items.is_empty());
        let source = normalize("E=mc²；x³；120");
        for item in &items {
            assert!(
                is_grounded(&item.forms, &source),
                "both sides must share the NFKC projection: {} {:?}",
                item.kind,
                item.text
            );
        }
    }

    #[test]
    fn magnitude_units_fall_back_to_the_core_and_rounding_stays_ungrounded() {
        let item = extract_items("3.5万").remove(0);
        assert!(is_grounded(&item.forms, &normalize("35000 条")));
        assert!(unit_was_dropped(&item, &normalize("35000 条")));
        assert!(!unit_was_dropped(&item, &normalize("3.5万 条")));

        // Rounding and abbreviations are real problems, not unit noise.
        let rounded = extract_items("从285MB降至100MB");
        let source = normalize("285.52 MB 与 100.0 MB");
        assert!(
            rounded
                .iter()
                .any(|item| !is_grounded(&item.forms, &source)),
            "{rounded:?}"
        );
        let abbreviation = extract_items("使用 SDS 与 LLM 两种方案");
        let source = normalize("使用两种方案");
        assert!(abbreviation
            .iter()
            .filter(|item| item.kind == KIND_IDENTIFIER)
            .all(|item| !is_grounded(&item.forms, &source)));
    }

    #[test]
    fn line_numbers_are_one_based() {
        let items = extract_items("first\nsecond memory_recent\n");
        let item = items
            .iter()
            .find(|item| item.text == "memory_recent")
            .expect("identifier");
        assert_eq!(item.line, 2);
    }
}

#[cfg(test)]
mod english_month_tests {
    use super::*;

    #[test]
    fn english_month_names_ground_chinese_month_day() {
        let source = normalize("Please ship before September 22, 2025.");
        let items = extract_items("需在9月22日前发货");
        let month_day = items.iter().find(|item| item.text.contains("9月22")).expect("month-day item");
        assert!(is_grounded(&month_day.forms, &source), "{month_day:?}");
        let other = normalize("Please ship before September 23, 2025.");
        assert!(!is_grounded(&month_day.forms, &other), "a different day must not ground");
    }
}
