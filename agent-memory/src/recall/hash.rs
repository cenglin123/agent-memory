//! Deterministic hashing / coordinate primitives (§3.2).
//!
//! `root_sha256` uses the configuration string verbatim (letter case folded,
//! separators normalized); `std::fs::canonicalize` is never applied here.

use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::Read;
use std::path::Path;

pub const BLOCK_BYTES: usize = 64 * 1024;

pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    to_hex(&hasher.finalize())
}

fn to_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Normalize a configured root string for hashing and runtime comparison.
///
/// Steps (§3.2): ASCII letters lowercased (byte length preserved, so relative
/// path slicing stays valid), `\` -> `/`, collapse repeated separators, drop
/// trailing separators while keeping a bare drive root as `c:/`. No symlink,
/// junction or 8.3 expansion.
pub fn normalize_root(raw: &str) -> String {
    let lowered = raw.to_ascii_lowercase();
    let mut out = String::with_capacity(lowered.len());
    let mut last_was_separator = false;
    for ch in lowered.chars() {
        if ch == '/' || ch == '\\' {
            if !last_was_separator {
                out.push('/');
            }
            last_was_separator = true;
        } else {
            out.push(ch);
            last_was_separator = false;
        }
    }
    while out.len() > 3 && out.ends_with('/') {
        out.pop();
    }
    out
}

pub fn root_sha256(raw_root: &str) -> String {
    sha256_hex(normalize_root(raw_root).as_bytes())
}

/// Stream a whole file through SHA-256 in 64 KiB blocks (never a full read).
pub fn file_sha256_streaming(path: &Path) -> std::io::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; BLOCK_BYTES];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(to_hex(&hasher.finalize()))
}

pub fn file_len(path: &Path) -> std::io::Result<u64> {
    Ok(std::fs::metadata(path)?.len())
}

/// Number of newlines strictly before `position` (0-based line index).
pub fn line_at(bytes: &[u8], position: usize) -> u64 {
    let position = position.min(bytes.len());
    bytes[..position].iter().filter(|b| **b == b'\n').count() as u64
}

/// A 0-based, half-open span. `byte_end` excludes the end; `line_end` excludes
/// the end line (a single-line span has `line_end = line_start + 1`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TextRange {
    pub line_start: u64,
    pub line_end: u64,
    pub byte_start: u64,
    pub byte_end: u64,
}

impl TextRange {
    pub fn new(byte_start: u64, byte_end: u64, line_start: u64, line_end: u64) -> Self {
        TextRange {
            line_start,
            line_end,
            byte_start,
            byte_end,
        }
    }

    /// Build the span of `bytes[byte_start..byte_end]`.
    pub fn from_span(bytes: &[u8], byte_start: usize, byte_end: usize) -> Self {
        let start = byte_start.min(bytes.len());
        let end = byte_end.min(bytes.len()).max(start);
        let line_start = line_at(bytes, start);
        let newlines = bytes[start..end].iter().filter(|b| **b == b'\n').count() as u64;
        let tail = if end > start && bytes[end - 1] != b'\n' {
            1
        } else {
            0
        };
        TextRange {
            line_start,
            line_end: line_start + newlines + tail,
            byte_start: start as u64,
            byte_end: end as u64,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.byte_start == self.byte_end
    }

    pub fn byte_len(&self) -> u64 {
        self.byte_end.saturating_sub(self.byte_start)
    }
}

pub fn range_bytes<'a>(bytes: &'a [u8], range: &TextRange) -> Option<&'a [u8]> {
    let start = usize::try_from(range.byte_start).ok()?;
    let end = usize::try_from(range.byte_end).ok()?;
    if start > end || end > bytes.len() {
        return None;
    }
    Some(&bytes[start..end])
}

pub fn range_sha256(bytes: &[u8], range: &TextRange) -> Option<String> {
    range_bytes(bytes, range).map(sha256_hex)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_normalization_is_case_and_separator_insensitive() {
        assert_eq!(normalize_root(r"C:\wt\x"), "c:/wt/x");
        assert_eq!(normalize_root("c:/WT/x/"), "c:/wt/x");
        assert_eq!(normalize_root("C:/wt//x///"), "c:/wt/x");
        assert_eq!(normalize_root(r"C:\"), "c:/");
        assert_ne!(root_sha256(r"C:\wt\x"), root_sha256(r"D:\wt\x"));
        assert_eq!(root_sha256(r"C:\wt\x"), root_sha256("c:/WT/x/"));
    }

    #[test]
    fn text_range_is_zero_based_half_open() {
        let bytes = b"alpha\nbeta\ngamma\n";
        // "beta" -> line 1, no trailing newline in the span.
        let range = TextRange::from_span(bytes, 6, 10);
        assert_eq!(range.line_start, 1);
        assert_eq!(range.line_end, 2);
        assert_eq!(range.byte_start, 6);
        assert_eq!(range.byte_end, 10);
        assert_eq!(range_bytes(bytes, &range).unwrap(), b"beta");

        // "alpha\n" -> exactly line 0.
        let range = TextRange::from_span(bytes, 0, 6);
        assert_eq!((range.line_start, range.line_end), (0, 1));

        // whole file -> three lines.
        let range = TextRange::from_span(bytes, 0, bytes.len());
        assert_eq!((range.line_start, range.line_end), (0, 3));

        // empty range keeps line_start == line_end.
        let range = TextRange::from_span(bytes, 11, 11);
        assert_eq!((range.line_start, range.line_end), (2, 2));
    }

    #[test]
    fn range_sha_matches_slice_bytes() {
        let bytes = b"hello world";
        let range = TextRange::from_span(bytes, 6, 11);
        assert_eq!(
            range_sha256(bytes, &range).unwrap(),
            sha256_hex(b"world")
        );
    }
}
