//! `agent-memory audit-scan` (stage-2 S2.5): pattern scan over *decoded*
//! object bytes for the suite's credential audit (90-audit.ps1).
//!
//! PowerShell has no zstd decoder, so the audit delegates the compressed side
//! of the runtime root to this command: every object is streamed through the
//! same explicit-encoding read path as any other reader, matched in bounded
//! memory (64 KiB chunks plus a 64 KiB carry-over window so a match may span
//! chunk boundaries), and the report carries **counts only** — never matched
//! content. The rules themselves stay single-sourced in
//! `agent-memory-common.ps1` and are passed in per invocation.
//!
//! This is deliberately not a fail-closed `read` surface: it answers one
//! boolean per (object, rule), which a local process could compute from the
//! object files directly. No byte of object content is ever emitted.

use std::collections::BTreeMap;
use std::io::Read;

use serde::Serialize;

use crate::archive::{Archive, ArchiveError, ObjectEncoding, OBJECT_CHUNK_BYTES};

/// Chunk carry-over: every match window includes the tail of the previous
/// chunk, so a pattern spanning a read boundary still matches as long as it is
/// shorter than the window (the credential rules are tens of bytes).
const MATCH_WINDOW_BYTES: usize = 64 * 1024;

/// One audit rule (compiled by the caller after CLI-side validation). The
/// `bytes` engine matches on raw decoded bytes: objects are session JSONL, but
/// nothing guarantees UTF-8 in general.
pub struct ScanRule {
    pub name: String,
    pub regex: regex::bytes::Regex,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AuditScanReport {
    pub ok: bool,
    pub root: String,
    /// Object files scanned (both encodings).
    pub objects: u64,
    /// Decoded bytes examined in total.
    pub decoded_bytes: u64,
    /// Objects where at least one rule matched.
    pub files_with_hits: u64,
    /// Per-rule count of matching *objects* (an object counts once per rule).
    pub hits_by_rule: BTreeMap<String, u64>,
    /// Objects that could not be read/decoded (bounded sample; never content).
    pub errors: Vec<String>,
}

/// Scan every object of the runtime root against the given rules. Read-only;
/// no lock is taken (a concurrent capture may add objects, which at worst
/// makes the count a snapshot of a moment — the audit re-runs on demand).
pub fn scan(archive: &Archive, rules: &[ScanRule]) -> Result<AuditScanReport, ArchiveError> {
    let mut report = AuditScanReport {
        ok: true,
        root: archive.root().to_string_lossy().to_string(),
        objects: 0,
        decoded_bytes: 0,
        files_with_hits: 0,
        hits_by_rule: BTreeMap::new(),
        errors: Vec::new(),
    };
    for entry in archive.object_files()? {
        report.objects += 1;
        let matched = match scan_object(archive, &entry.sha256, entry.encoding, rules) {
            Ok((decoded_bytes, hits)) => {
                report.decoded_bytes += decoded_bytes;
                hits
            }
            Err(message) => {
                if report.errors.len() < 10 {
                    report.errors.push(message);
                }
                continue;
            }
        };
        if !matched.is_empty() {
            report.files_with_hits += 1;
            for name in matched {
                *report.hits_by_rule.entry(name).or_insert(0) += 1;
            }
        }
    }
    report.ok = report.errors.is_empty();
    Ok(report)
}

/// Stream one object (decoded per its explicit encoding) through every rule.
/// Returns the decoded byte count and the names of rules that matched at least
/// once. Memory stays bounded by chunk + window, never by object size.
fn scan_object(
    archive: &Archive,
    sha256: &str,
    encoding: ObjectEncoding,
    rules: &[ScanRule],
) -> Result<(u64, Vec<String>), String> {
    use std::fs::File;
    let path = archive.object_path_for(sha256, encoding);
    let file = File::open(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut reader: Box<dyn Read> = match encoding {
        ObjectEncoding::Plain => Box::new(file),
        ObjectEncoding::Zstd => match zstd::stream::read::Decoder::new(file) {
            Ok(decoder) => Box::new(decoder),
            Err(e) => return Err(format!("object {sha256} is not valid zstd: {e}")),
        },
    };
    let mut matched: Vec<String> = Vec::new();
    let mut still_unmatched: Vec<&ScanRule> = rules.iter().collect();
    let mut carry: Vec<u8> = Vec::new();
    let mut chunk = vec![0u8; OBJECT_CHUNK_BYTES];
    let mut decoded_bytes = 0u64;
    loop {
        let read = reader
            .read(&mut chunk)
            .map_err(|e| format!("object {sha256}: {e}"))?;
        if read == 0 {
            break;
        }
        decoded_bytes += read as u64;
        if !still_unmatched.is_empty() {
            let mut window = std::mem::take(&mut carry);
            window.extend_from_slice(&chunk[..read]);
            let keep_from = window.len().saturating_sub(MATCH_WINDOW_BYTES);
            carry = window[keep_from..].to_vec();
            still_unmatched.retain(|rule| {
                if rule.regex.is_match(&window) {
                    matched.push(rule.name.clone());
                    false
                } else {
                    true
                }
            });
        }
    }
    Ok((decoded_bytes, matched))
}
