//! Record ledger, on-disk form (implementation-006): a canonical file plus an
//! append-only tail.
//!
//! # Why two files, one ledger
//!
//! The record ledger maps `record sha -> object span`. Before this module the
//! only form was `state/records.json`, a single JSON document holding every
//! entry, rewritten in full after every capture round. On the production store
//! that is ≈135 MB rewritten per round (~148 MB including the temp copy) for
//! the handful of records a round actually adds, so the write cost followed the
//! ledger's *total* size instead of the change.
//!
//! The ledger now has two on-disk forms of the **same** ledger:
//!
//! ```text
//! <root>/state/records.json    canonical: the whole ledger, one JSON document
//! <root>/state/records.jsonl   tail: one JSON line per entry appended since
//! ```
//!
//! The tail is a journal of the same ledger, not a second index: it carries no
//! state of its own, and every reader merges the two forms into the one
//! [`RecordsFile`] view the rest of the crate has always used.
//!
//! # Merge order (first writer wins)
//!
//! `canonical` first, then the tail's lines in file order, each taken **only
//! when the sha is not already present**. That single rule covers every crash
//! window:
//!
//! - *compaction wrote the canonical and crashed before deleting the tail*: the
//!   canonical already holds those entries, so the tail lines are duplicates and
//!   are ignored;
//! - *a round crashed after appending and the next round re-captured the same
//!   records*: the duplicate lines are ignored, the first mapping stays;
//! - *normal operation*: the canonical holds everything up to the last
//!   compaction, the tail holds what came after.
//!
//! # Crash windows
//!
//! - **Append crash.** A record line is written with one `write_all` followed by
//!   `flush` + `sync_all`. A crash can leave a *prefix* of that line at the end
//!   of the file. That unterminated remainder is the only thing this module ever
//!   ignores: it is counted (`records_tail.incomplete_lines`) and dropped on the
//!   next append, so a half line never merges into the following entry. The
//!   record it carried is re-captured by the next round (the manifest is
//!   committed but the watermark was not advanced), which makes recovery
//!   idempotent.
//! - **Compaction crash.** The canonical is written atomically first; only then
//!   is the tail removed. A crash in between leaves a canonical that is already
//!   complete plus a tail that only repeats it — readable and equal to the
//!   manifest replay. The reverse order would lose the tail's entries.
//!
//! A *complete* line that does not parse, carries a sha that is not 64 lowercase
//! hex characters, or is missing a field is **fail-closed**: the ledger is
//! reported corrupt instead of silently dropping entries.
//!
//! # Automatic compaction
//!
//! A capture round appends; it rewrites the canonical only when the tail is due
//! ([`RECORDS_TAIL_COMPACT_BYTES`], [`RECORDS_TAIL_COMPACT_MAX_AGE_MS`], or the
//! canonical file does not exist yet). Compaction itself holds
//! `locks/records-compact.lock`, so two compacting processes serialise; the
//! loser skips (automatic path) or reports a held lock (explicit command).
//!
//! # Concurrency: the journal's own lock
//!
//! Two writers touch the journal — the append (repair a half line, then write)
//! and the compacting side (check that the journal is still the one it merged,
//! then remove it). They share `locks/records-tail.lock`, which is held for
//! milliseconds on both sides (one append and one `fsync`; one `stat`, one read
//! of the whole journal and one `remove_file`), never across a canonical
//! rewrite. A line therefore can only reach the journal while no compaction is
//! inside its removal window, and a compaction only deletes bytes it merged. A
//! flush that cannot take the lock for its budget does **not** write the journal
//! (see `Archive::flush_records_inner`); a reader is not part of this lock:
//! [`super::read_ledger`] samples both file stamps before and after its read and
//! retries when they moved, so the merged view it returns is one state the files
//! had, unless the journal was rewritten to the same length in the same
//! millisecond or the retry budget ran out (both disclosed by counters).

use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::{ArchiveError, RecordRef, RECORDS_SCHEMA};
use crate::error::AppError;

/// Canonical ledger file name (runtime-root relative, below `state/`).
pub const RECORDS_CANONICAL_FILENAME: &str = "records.json";
/// Append-only journal file name (runtime-root relative, below `state/`).
pub const RECORDS_TAIL_FILENAME: &str = "records.jsonl";
/// Canonical file name before the `state/` component (diagnostics, errors).
pub const RECORDS_CANONICAL_PATH: &str = "state/records.json";
/// Journal file name before the `state/` component (diagnostics, errors).
pub const RECORDS_TAIL_PATH: &str = "state/records.jsonl";

/// Automatic compaction threshold: a journal larger than this is compacted at
/// the end of the round that appended to it. 1 MiB is roughly 5 000 record
/// lines, i.e. days of production capture, so a round normally appends only.
pub const RECORDS_TAIL_COMPACT_BYTES: u64 = 1024 * 1024;

/// Automatic compaction age bound: a non-empty journal whose newest append is
/// older than this is compacted, so a quiet install never keeps a journal
/// forever (bounded recovery work).
pub const RECORDS_TAIL_COMPACT_MAX_AGE_MS: i64 = 7 * 24 * 60 * 60 * 1000;

/// One journal line: the record sha plus its stored span. Field names are the
/// wire format; a missing field is a fail-closed read error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordJournalLine {
    pub sha: String,
    pub object: String,
    pub offset: u64,
    pub len: u64,
    pub capture_id: String,
}

impl RecordJournalLine {
    /// Borrow one staged entry as a journal line.
    pub fn of(sha: &str, reference: &RecordRef) -> RecordJournalLine {
        RecordJournalLine {
            sha: sha.to_string(),
            object: reference.object.clone(),
            offset: reference.offset,
            len: reference.len,
            capture_id: reference.capture_id.clone(),
        }
    }

    /// Whether `self.sha` names this entry (the map key and the line agree).
    pub fn into_entry(self) -> (String, RecordRef) {
        (
            self.sha,
            RecordRef {
                object: self.object,
                offset: self.offset,
                len: self.len,
                capture_id: self.capture_id,
            },
        )
    }
}

/// One journal line, newline-terminated, exactly as it is appended.
pub fn journal_line(sha: &str, reference: &RecordRef) -> Result<String, ArchiveError> {
    let line = RecordJournalLine::of(sha, reference);
    let mut text = serde_json::to_string(&line)
        .map_err(|e| ArchiveError::Corrupt(format!("{RECORDS_TAIL_PATH}: {e}")))?;
    text.push('\n');
    Ok(text)
}

/// True for a 64-character lowercase hex sha (record shas and object shas).
fn is_hex_sha(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Result of parsing the tail bytes.
#[derive(Debug, Default)]
pub struct TailParse {
    /// Entries in file order (first line first).
    pub entries: Vec<(String, RecordRef)>,
    /// Bytes of the complete lines (the part an append continues from).
    pub complete_bytes: u64,
    /// Bytes of the unterminated last line (never merged; disclosed).
    pub incomplete_bytes: u64,
    /// Complete, non-blank lines read.
    pub lines: u64,
    /// Complete lines that were blank (skipped; nothing to merge).
    pub blank_lines: u64,
}

/// Parse the tail exactly as the reader sees it.
///
/// The last line counts as incomplete when the file does not end with `\n`; it
/// is the only ignored content. Every complete line must parse and carry a
/// valid sha, or the whole ledger is corrupt (fail-closed).
pub fn parse_tail(bytes: &[u8]) -> Result<TailParse, ArchiveError> {
    let mut parsed = TailParse::default();
    let mut offset = 0usize;
    while offset < bytes.len() {
        let rest = &bytes[offset..];
        let Some(newline) = rest.iter().position(|byte| *byte == b'\n') else {
            // Unterminated remainder: a crash in the middle of one append.
            parsed.incomplete_bytes = rest.len() as u64;
            break;
        };
        let line = &rest[..newline];
        offset += newline + 1;
        parsed.complete_bytes = offset as u64;
        parsed.lines += 1;
        if line.iter().all(|byte| byte.is_ascii_whitespace()) {
            parsed.blank_lines += 1;
            continue;
        }
        let line_no = parsed.lines;
        let entry: RecordJournalLine = serde_json::from_slice(line).map_err(|error| {
            ArchiveError::Corrupt(format!("{RECORDS_TAIL_PATH}: line {line_no}: {error}"))
        })?;
        if !is_hex_sha(&entry.sha) {
            return Err(ArchiveError::Corrupt(format!(
                "{RECORDS_TAIL_PATH}: line {line_no}: record sha is not 64 lowercase hex characters"
            )));
        }
        if !is_hex_sha(&entry.object) {
            return Err(ArchiveError::Corrupt(format!(
                "{RECORDS_TAIL_PATH}: line {line_no}: object sha is not 64 lowercase hex characters"
            )));
        }
        parsed.entries.push(entry.into_entry());
    }
    Ok(parsed)
}

/// Lines inserted and lines skipped because the sha was already present.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MergeStats {
    pub inserted: u64,
    pub duplicates: u64,
}

/// Merge journal entries into an existing map, first writer wins.
///
/// `records` starts as the canonical entries, so a canonical key always wins;
/// inside the tail the earliest line wins. Nothing is ever overwritten, which is
/// exactly the `entry(..).or_insert(..)` rule the single-file ledger used.
pub fn merge_first_writer(
    records: &mut BTreeMap<String, RecordRef>,
    entries: Vec<(String, RecordRef)>,
) -> MergeStats {
    let mut stats = MergeStats::default();
    for (sha, reference) in entries {
        if records.contains_key(&sha) {
            stats.duplicates += 1;
            continue;
        }
        records.insert(sha, reference);
        stats.inserted += 1;
    }
    stats
}

/// The complete-line prefix of the tail and its unterminated remainder.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TailLayout {
    /// Current file length.
    pub len: u64,
    /// Length of the complete-line prefix (where an append may start).
    pub complete_len: u64,
    /// Length of the unterminated remainder (0 when the file ends with `\n`).
    pub incomplete_bytes: u64,
}

/// Byte layout of the tail on disk. `None` when the file does not exist.
///
/// Only the file's tail is read: a scan walks backwards one 64 KiB window at a
/// time until it finds the last newline, so this costs a few KiB however large
/// the journal is (and however large one record line is).
pub fn tail_layout(path: &Path) -> Result<Option<TailLayout>, ArchiveError> {
    let len = match fs::metadata(path) {
        Ok(meta) => meta.len(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(AppError::io(path, error).into()),
    };
    if len == 0 {
        return Ok(Some(TailLayout::default()));
    }
    let complete_len = match last_newline(path, len)? {
        Some(index) => index + 1,
        None => 0,
    };
    Ok(Some(TailLayout {
        len,
        complete_len,
        incomplete_bytes: len - complete_len,
    }))
}

/// Offset of the last `\n` in the file, scanning backwards from the end.
fn last_newline(path: &Path, len: u64) -> Result<Option<u64>, ArchiveError> {
    const WINDOW: u64 = 64 * 1024;
    let mut file = fs::File::open(path).map_err(|e| AppError::io(path, e))?;
    let mut buffer = vec![0u8; WINDOW as usize];
    let mut end = len;
    while end > 0 {
        let start = end.saturating_sub(WINDOW);
        let want = (end - start) as usize;
        file.seek(SeekFrom::Start(start))
            .map_err(|e| AppError::io(path, e))?;
        file.read_exact(&mut buffer[..want])
            .map_err(|e| AppError::io(path, e))?;
        if let Some(index) = buffer[..want].iter().rposition(|byte| *byte == b'\n') {
            return Ok(Some(start + index as u64));
        }
        end = start;
    }
    Ok(None)
}

/// Drop an unterminated last line, returning the bytes removed.
///
/// Called before an append: appending after a half line would otherwise glue
/// the new entry onto it and make every later read fail-closed. The dropped
/// bytes belong to a record whose append crashed; the next capture round stages
/// it again (its watermark did not advance), so nothing is lost.
pub fn repair_incomplete_tail(path: &Path) -> Result<u64, ArchiveError> {
    let Some(layout) = tail_layout(path)? else {
        return Ok(0);
    };
    if layout.incomplete_bytes == 0 {
        return Ok(0);
    }
    let file = fs::OpenOptions::new()
        .write(true)
        .open(path)
        .map_err(|e| AppError::io(path, e))?;
    file.set_len(layout.complete_len)
        .map_err(|e| AppError::io(path, e))?;
    file.sync_all().map_err(|e| AppError::io(path, e))?;
    Ok(layout.incomplete_bytes)
}

/// Append already-encoded journal lines, then flush and fsync them.
pub fn append_tail(path: &Path, data: &[u8]) -> Result<(), ArchiveError> {
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| AppError::io(path, e))?;
    file.write_all(data).map_err(|e| AppError::io(path, e))?;
    file.flush().map_err(|e| AppError::io(path, e))?;
    file.sync_all().map_err(|e| AppError::io(path, e))?;
    Ok(())
}

/// 64-bit FNV-1a of journal bytes.
///
/// A compaction compares this against the digest of the bytes it merged before it
/// removes the journal: "the journal is still the one I merged" has to be a
/// content claim, because a size + millisecond-mtime pair can be reproduced by a
/// different journal (truncate a half line, append a line of the same length, in
/// the same millisecond). The digest is over the exact bytes read, so equal
/// digest + equal length means equal journal.
pub fn digest(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Remove the tail. A missing file is success (another process compacted).
pub fn remove_tail(path: &Path) -> Result<(), ArchiveError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(AppError::io(path, error).into()),
    }
}

/// Empty ledger view (the shape a reader gets from a root with no ledger yet).
pub fn empty_ledger() -> super::RecordsFile {
    super::RecordsFile {
        schema: RECORDS_SCHEMA.to_string(),
        records: BTreeMap::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reference(object: char, capture: &str) -> RecordRef {
        RecordRef {
            object: object.to_string().repeat(64),
            offset: 1,
            len: 2,
            capture_id: capture.to_string(),
        }
    }

    fn line(sha: char, object: char, capture: &str) -> String {
        journal_line(&sha.to_string().repeat(64), &reference(object, capture)).expect("line")
    }

    #[test]
    fn a_journal_line_round_trips() {
        let text = line('a', 'b', "cap-1");
        assert!(text.ends_with('\n'));
        assert_eq!(text.matches('\n').count(), 1);
        let parsed = parse_tail(text.as_bytes()).expect("parses");
        assert!(parsed.incomplete_bytes == 0);
        assert_eq!(parsed.lines, 1);
        assert_eq!(parsed.entries.len(), 1);
        let (sha, entry) = &parsed.entries[0];
        assert_eq!(sha, &"a".repeat(64));
        assert_eq!(entry.object, "b".repeat(64));
        assert_eq!(entry.capture_id, "cap-1");
    }

    #[test]
    fn an_unterminated_last_line_is_the_only_ignored_content() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(line('a', 'a', "cap-1").as_bytes());
        bytes.extend_from_slice(line('b', 'b', "cap-2").as_bytes());
        let whole = bytes.len() as u64;
        bytes.extend_from_slice(b"{\"sha\":\"cccc");
        let parsed = parse_tail(&bytes).expect("the half line is not an error");
        assert_eq!(parsed.lines, 2);
        assert_eq!(parsed.entries.len(), 2);
        assert_eq!(parsed.complete_bytes, whole);
        assert_eq!(parsed.incomplete_bytes, bytes.len() as u64 - whole);
    }

    #[test]
    fn a_complete_line_that_does_not_parse_fails_closed() {
        let mut bytes = line('a', 'a', "cap-1").into_bytes();
        bytes.extend_from_slice(b"{not json}\n");
        let error = parse_tail(&bytes).expect_err("must fail closed");
        assert!(matches!(error, ArchiveError::Corrupt(_)), "{error:?}");
    }

    #[test]
    fn a_complete_line_with_an_invalid_sha_or_object_fails_closed() {
        // Uppercase hex is not the canonical form the ledger stores.
        let uppercase = format!(
            "{}\n",
            serde_json::json!({
                "sha": "A".repeat(64),
                "object": "b".repeat(64),
                "offset": 0,
                "len": 1,
                "capture_id": "cap",
            })
        );
        assert!(parse_tail(uppercase.as_bytes()).is_err());

        // A short sha is not a sha.
        let short = format!(
            "{}\n",
            serde_json::json!({
                "sha": "abc",
                "object": "b".repeat(64),
                "offset": 0,
                "len": 1,
                "capture_id": "cap",
            })
        );
        assert!(parse_tail(short.as_bytes()).is_err());

        // The record's carrier must be an object sha too.
        let bad_object = format!(
            "{}\n",
            serde_json::json!({
                "sha": "a".repeat(64),
                "object": "not-a-sha",
                "offset": 0,
                "len": 1,
                "capture_id": "cap",
            })
        );
        assert!(parse_tail(bad_object.as_bytes()).is_err());
    }

    #[test]
    fn a_complete_line_with_a_missing_field_fails_closed() {
        let missing = format!(
            "{}\n",
            serde_json::json!({
                "sha": "a".repeat(64),
                "object": "b".repeat(64),
                "offset": 0,
                "len": 1,
            })
        );
        assert!(parse_tail(missing.as_bytes()).is_err());
    }

    #[test]
    fn canonical_keys_win_and_the_first_tail_line_wins_after_them() {
        let mut records = BTreeMap::new();
        records.insert("a".repeat(64), reference('1', "canonical"));
        let entries = vec![
            ("a".repeat(64), reference('2', "tail-first")),
            ("b".repeat(64), reference('3', "tail-b")),
            ("b".repeat(64), reference('4', "tail-duplicate")),
        ];
        let stats = merge_first_writer(&mut records, entries);
        assert_eq!(stats.inserted, 1);
        assert_eq!(stats.duplicates, 2);
        assert_eq!(records.len(), 2);
        assert_eq!(records[&"a".repeat(64)].capture_id, "canonical");
        assert_eq!(records[&"b".repeat(64)].capture_id, "tail-b");
    }

    #[test]
    fn a_blank_line_carries_nothing_and_is_not_an_error() {
        let bytes = format!("\n{}", line('a', 'b', "cap-1"));
        let parsed = parse_tail(bytes.as_bytes()).expect("blank line is skipped");
        assert_eq!(parsed.blank_lines, 1);
        assert_eq!(parsed.entries.len(), 1);
    }

    #[test]
    fn the_tail_layout_finds_the_last_newline_across_a_window_boundary() {
        let dir = tempfile::tempdir().expect("temp");
        let path = dir.path().join("records.jsonl");
        // One long complete line (larger than the 64 KiB scan window) plus a
        // short unterminated remainder: the backwards scan must find the single
        // newline, not give up at the window edge.
        let mut bytes = vec![b'x'; 70 * 1024];
        bytes.push(b'\n');
        let complete = bytes.len() as u64;
        bytes.extend_from_slice(b"{\"partial");
        fs::write(&path, &bytes).expect("write");
        let layout = tail_layout(&path).expect("layout").expect("exists");
        assert_eq!(layout.len, bytes.len() as u64);
        assert_eq!(layout.complete_len, complete);
        assert_eq!(layout.incomplete_bytes, 9);

        let removed = repair_incomplete_tail(&path).expect("repair");
        assert_eq!(removed, 9);
        assert_eq!(fs::metadata(&path).expect("stat").len(), complete);
        assert_eq!(repair_incomplete_tail(&path).expect("idempotent"), 0);
    }

    #[test]
    fn a_missing_tail_is_an_absent_layout() {
        let dir = tempfile::tempdir().expect("temp");
        assert!(tail_layout(&dir.path().join("nothing.jsonl"))
            .expect("ok")
            .is_none());
    }
}
