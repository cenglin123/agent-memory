//! Lexical window rules, snippet projection and line/byte helpers (§3.5).
//!
//! Only ASCII `A-Z` folds to lowercase. CJK and everything else is kept as-is;
//! there is no NFC/NFKC normalization, stemming or Chinese segmentation.

use std::collections::BTreeSet;

pub const SNIPPET_BYTES_DEFAULT: usize = 400;

pub fn is_cjk(ch: char) -> bool {
    matches!(ch as u32,
        0x3400..=0x4DBF
        | 0x4E00..=0x9FFF
        | 0xF900..=0xFAFF
        | 0x3040..=0x30FF
        | 0xAC00..=0xD7AF
        | 0x20000..=0x2FA1F)
}

/// Fold ASCII uppercase only; byte length is preserved, so offsets stay valid.
pub fn fold_ascii(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        if ch.is_ascii_uppercase() {
            out.push(ch.to_ascii_lowercase());
        } else {
            out.push(ch);
        }
    }
    out
}

fn push_term(terms: &mut Vec<String>, term: String) {
    if !term.is_empty() && !terms.iter().any(|existing| *existing == term) {
        terms.push(term);
    }
}

fn flush_ascii(buffer: &mut String, terms: &mut Vec<String>) {
    if buffer.chars().count() >= 2 {
        push_term(terms, std::mem::take(buffer));
    } else {
        buffer.clear();
    }
}

fn flush_cjk(buffer: &mut Vec<char>, terms: &mut Vec<String>) {
    if buffer.is_empty() {
        return;
    }
    if buffer.len() == 1 {
        push_term(terms, buffer[0].to_string());
    } else {
        push_term(terms, buffer.iter().collect());
        for pair in buffer.windows(2) {
            push_term(terms, pair.iter().collect());
        }
    }
    buffer.clear();
}

/// Derive query terms: ASCII runs of `[A-Za-z0-9_]+` (length >= 2), and for
/// every contiguous CJK run the whole phrase plus adjacent 2-char bigrams.
/// An empty result means the query has no usable term (`INVALID_REQUEST`).
pub fn tokenize(query: &str) -> Vec<String> {
    let mut terms = Vec::new();
    let mut ascii = String::new();
    let mut cjk: Vec<char> = Vec::new();
    for ch in query.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            flush_cjk(&mut cjk, &mut terms);
            ascii.push(ch.to_ascii_lowercase());
        } else if is_cjk(ch) {
            flush_ascii(&mut ascii, &mut terms);
            cjk.push(ch);
        } else {
            flush_ascii(&mut ascii, &mut terms);
            flush_cjk(&mut cjk, &mut terms);
        }
    }
    flush_ascii(&mut ascii, &mut terms);
    flush_cjk(&mut cjk, &mut terms);
    terms
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LexMatch {
    /// Number of distinct query terms that occur contiguously.
    pub distinct_terms: usize,
    /// Byte offset of the earliest occurrence inside the folded text.
    pub first_byte: usize,
    /// Byte length of the term that occurs at `first_byte`.
    pub first_term_len: usize,
}

/// Match terms against an already folded text. A hit requires at least one
/// contiguous term occurrence; the offset is comparable across parts because
/// folding preserves byte lengths.
pub fn lexical_match(folded_text: &str, terms: &[String]) -> Option<LexMatch> {
    if terms.is_empty() {
        return None;
    }
    let mut distinct = 0usize;
    let mut first: Option<(usize, usize)> = None;
    for term in terms {
        if let Some(offset) = folded_text.find(term.as_str()) {
            distinct += 1;
            first = Some(match first {
                Some((current, current_len)) if current <= offset => (current, current_len),
                _ => (offset, term.len()),
            });
        }
    }
    first.map(|(first_byte, first_term_len)| LexMatch {
        distinct_terms: distinct,
        first_byte,
        first_term_len,
    })
}

/// Start of the line containing `position`.
pub fn line_start_of(bytes: &[u8], position: usize) -> usize {
    let position = position.min(bytes.len());
    match bytes[..position].iter().rposition(|b| *b == b'\n') {
        Some(index) => index + 1,
        None => 0,
    }
}

/// Extend `start` back by `lines` whole lines (clamped at 0). With
/// `lines == 0` the input is returned unchanged.
pub fn expand_back(bytes: &[u8], start: usize, lines: usize) -> usize {
    let mut position = start.min(bytes.len());
    if lines == 0 {
        return position;
    }
    position = line_start_of(bytes, position);
    for _ in 0..lines {
        if position == 0 {
            return 0;
        }
        position = line_start_of(bytes, position - 1);
    }
    position
}

/// Extend `end` forward by `lines` whole lines, including their newline
/// terminators (clamped at the end of the buffer). With `lines == 0` the input
/// is returned unchanged.
pub fn expand_forward(bytes: &[u8], end: usize, lines: usize) -> usize {
    let mut position = end.min(bytes.len());
    if lines == 0 {
        return position;
    }
    // Move past the line containing `end` first: that line is not context.
    position = match bytes[position..].iter().position(|b| *b == b'\n') {
        Some(index) => position + index + 1,
        None => return bytes.len(),
    };
    for _ in 0..lines {
        if position >= bytes.len() {
            return bytes.len();
        }
        position = match bytes[position..].iter().position(|b| *b == b'\n') {
            Some(index) => position + index + 1,
            None => return bytes.len(),
        };
    }
    position
}

/// Largest char boundary of `text` that is `<= max_bytes`; always at least one
/// character when `text` is non-empty.
pub fn utf8_floor(text: &str, max_bytes: usize) -> usize {
    if max_bytes >= text.len() {
        return text.len();
    }
    let mut cut = max_bytes;
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    if cut == 0 && !text.is_empty() {
        text.chars().next().map(|ch| ch.len_utf8()).unwrap_or(0)
    } else {
        cut
    }
}

/// Truncate to at most `max_bytes` on a UTF-8 boundary; returns the prefix and
/// whether anything was dropped.
pub fn truncate_utf8(text: &str, max_bytes: usize) -> (&str, bool) {
    if text.len() <= max_bytes {
        return (text, false);
    }
    let cut = utf8_floor(text, max_bytes);
    (&text[..cut], true)
}

/// Display projection centered on the first hit (§3.5): the window is placed
/// around `first_byte`, always contains the matched term bytes
/// (`hit_len <= limit`), and is aligned to UTF-8 character boundaries. When the
/// whole line containing the hit fits in the window, the window starts at that
/// line start; otherwise it starts mid-line so a hit at the end of a long
/// single line stays visible.
pub fn snippet(text: &str, first_byte: usize, hit_len: usize, limit: usize) -> (String, bool) {
    if text.is_empty() {
        return (String::new(), false);
    }
    let bytes = text.as_bytes();
    let limit = limit.max(1);
    let hit_start = first_byte.min(bytes.len());
    let hit_end = (hit_start + hit_len).min(bytes.len());

    let mut start = if hit_len >= limit {
        hit_start
    } else {
        let budget = limit - hit_len;
        hit_start.saturating_sub(budget / 2)
    };
    while start > 0 && !text.is_char_boundary(start) {
        start -= 1;
    }
    // Prefer a whole-line window when the line prefix plus the hit fits.
    let line_start = line_start_of(bytes, start);
    if hit_len < limit && hit_end.saturating_sub(line_start) <= limit {
        start = line_start;
    }

    let mut end = (start + limit).min(bytes.len());
    if end < hit_end {
        end = hit_end;
    }
    // Align down to a character boundary; the hit end is itself a boundary, so
    // this never cuts the term and never exceeds `limit`.
    while end > hit_end && end > start && !text.is_char_boundary(end) {
        end -= 1;
    }
    // Prefer a line boundary for the window end when one exists inside the
    // window (long single lines keep the byte cut instead).
    if end < bytes.len() && end > hit_end {
        if let Some(index) = bytes[hit_end..end].iter().rposition(|byte| *byte == b'\n') {
            end = hit_end + index + 1;
        }
    }
    if end < start {
        end = start;
    }
    let snippet = text[start..end].to_string();
    let truncated = start > 0 || end < bytes.len();
    (snippet, truncated)
}

/// Distinct, order-preserving set of the terms found in a text (diagnostics).
pub fn matched_terms(text: &str, terms: &[String]) -> BTreeSet<String> {
    let folded = fold_ascii(text);
    terms
        .iter()
        .filter(|term| folded.contains(term.as_str()))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenizer_keeps_ascii_runs_and_cjk_phrases() {
        assert_eq!(tokenize("Hello World"), vec!["hello", "world"]);
        assert_eq!(tokenize("a b"), Vec::<String>::new());
        assert_eq!(tokenize("最小统一回忆"), {
            let mut expected = vec!["最小统一回忆", "最小", "小统", "统一", "一回", "回忆"];
            expected.sort_by_key(|term| {
                // first-seen order is phrase, then bigrams in order
                match *term {
                    "最小统一回忆" => 0,
                    "最小" => 1,
                    "小统" => 2,
                    "统一" => 3,
                    "一回" => 4,
                    _ => 5,
                }
            });
            expected
        });
        assert_eq!(tokenize("单"), vec!["单"]);
        assert_eq!(tokenize("!!!"), Vec::<String>::new());
    }

    #[test]
    fn lex_match_counts_distinct_terms_and_first_offset() {
        let terms = tokenize("alpha beta");
        let found = lexical_match(&fold_ascii("xx BETA yy alpha"), &terms).unwrap();
        assert_eq!(found.distinct_terms, 2);
        assert_eq!(found.first_byte, 3);
        assert_eq!(found.first_term_len, 4);
        assert!(lexical_match(&fold_ascii("nothing here"), &terms).is_none());
    }

    #[test]
    fn snippet_is_centered_on_the_hit_and_always_visible() {
        let text = "one\ntwo\nthree\nfour\n";
        let (short, truncated) = snippet(text, 4, 3, 8); // hit "two"
        assert!(short.contains("two"), "snippet must contain the hit: {short:?}");
        assert!(
            short.starts_with("one"),
            "a short line window keeps the preceding context: {short:?}"
        );
        assert!(short.len() <= 8);
        assert!(truncated);

        let (whole, truncated) = snippet(text, 4, 3, 4096);
        assert_eq!(whole, text);
        assert!(!truncated);

        let (middle, _) = snippet(text, 13, 5, 4096); // hit "three"
        assert!(middle.contains("three"));

        let (single_line, truncated) = snippet("abcdef\n", 0, 3, 3);
        assert_eq!(single_line, "abc");
        assert!(truncated);
    }

    #[test]
    fn snippet_keeps_a_hit_at_the_end_of_a_long_single_line() {
        let text = format!("{}endoutput", "x".repeat(20_000));
        let (snippet, truncated) = snippet(&text, 20_000, "endoutput".len(), 400);
        assert!(
            snippet.contains("endoutput"),
            "a tail hit must stay inside the window (len {})",
            snippet.len()
        );
        assert!(snippet.len() <= 400);
        assert!(truncated);
        assert!(text.is_char_boundary(20_000));
    }

    #[test]
    fn snippet_window_never_splits_a_multibyte_character() {
        let text = format!("{}命中关键词{}", "前".repeat(500), "后".repeat(500));
        let offset = "前".repeat(500).len();
        let (snippet, _) = snippet(&text, offset, "命中关键词".len(), 50);
        assert!(snippet.contains("命中关键词"));
        // The window is cut on a character boundary and never exceeds the limit.
        assert!(snippet.len() <= 50);
        assert!(std::str::from_utf8(snippet.as_bytes()).is_ok());
    }

    #[test]
    fn utf8_truncation_never_splits_a_character() {
        let text = "中文abc";
        let (prefix, cut) = truncate_utf8(text, 4);
        assert!(cut);
        assert_eq!(prefix, "中");
        assert_eq!(utf8_floor(text, 1), 3);
    }

    #[test]
    fn line_expansion_is_zero_at_zero_and_covers_whole_lines() {
        let bytes = b"l0\nl1\nl2\nl3";
        assert_eq!(expand_back(bytes, 7, 0), 7);
        assert_eq!(expand_back(bytes, 7, 1), 3);
        assert_eq!(expand_forward(bytes, 7, 0), 7);
        assert_eq!(expand_forward(bytes, 7, 1), 11);
        assert_eq!(expand_forward(bytes, 7, 5), bytes.len());
    }
}
