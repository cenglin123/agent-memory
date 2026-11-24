//! P2: incremental capture of plain JSONL session sources (Claude Code
//! `projects/*.jsonl`, Codex `sessions/**/rollout-*.jsonl`).
//!
//! # Watermark contract (per *physical source file*)
//!
//! A watermark is `{ offset, line, records, generation, fingerprint, probes }`:
//!
//! - `offset` is the committed byte offset: the end (including the trailing
//!   newline) of the last *complete* record. Only complete records whose bytes
//!   are on disk as a verified object, whose manifest is committed and whose
//!   record index is saved move this value. An incomplete trailing record is
//!   held back and re-read next run, so a crash or an interrupt never resumes
//!   past the last complete record.
//! - `line` / `records` are the committed physical-line and record counters.
//!   They define `seq` and the coverage counters for the next run.
//! - `generation` is a 1-based capture generation for this path. Appends keep
//!   it; a forced recapture after a non-append change bumps it. Old objects are
//!   never overwritten or deleted, so every generation stays readable.
//! - `head_probe` / `anchor_probe` are sha256 digests of the first and last
//!   `probe_len = min(offset, 8192)` bytes of the committed prefix. They are
//!   cheap O(8 KiB) append-versus-rewrite signals that survive growth (unlike
//!   the whole-file head/tail fingerprint, which moves when the file grows).
//!
//! # Change classification (cheap signals first)
//!
//! | observation | classification |
//! |---|---|
//! | no watermark | `initial` (full capture) |
//! | `len < offset` | `length_regression` → forced recapture |
//! | anchor probe mismatch | `prefix_rewritten` → forced recapture |
//! | `len == offset`, head probe differs | `same_length_rewrite` → forced recapture |
//! | `len == offset`, head probe matches | `skipped_unchanged` |
//! | `len > offset`, head probe matches | `append` from `offset` |
//! | `len > offset`, head probe differs | `prefix_rewritten` → forced recapture |
//!
//! A forced recapture re-reads the whole file, archives it as a **new**
//! generation object (the previous generation's objects stay on disk and stay
//! readable through their manifests) and never resumes from the stale offset.
//! Rotation (a different file at the same path) and "same path, different
//! content" both reduce to one of the discontinuity rows above; nothing is
//! inferred from the file name.
//!
//! # Honest limits
//!
//! Detection is bounded-verification, not a full-prefix scan: an in-place
//! rewrite that keeps the file length, the first 8 KiB, the last 8 KiB *and*
//! the probe window at the committed offset byte-identical is indistinguishable
//! from an unchanged file and is reported as `skipped_unchanged`. Verifying the
//! whole committed prefix would make every run O(file size), which §4.4
//! explicitly forbids; the residual blind spot is the price of a scan cost that
//! is independent of file size.
//!
//! Logical session ownership (resume / fork / rollback parenting) is *not*
//! decided here. Each physical file is located independently; grouping them is
//! a derived-layer concern that replays the archive.

use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use crate::archive::{
    hex_sha256, new_id, now_ms, Archive, ArchiveError, Manifest, ManifestObject, ManifestRecord,
    ManifestSource, RecordRef, SourceState, SourcesFile, StorageHint, Watermark, MANIFEST_SCHEMA,
};
use crate::capture::{CaptureReport, Failpoint};
use crate::cost;
use crate::error::AppError;
use crate::providers::utils::LineSpans;

/// Probe window for the cheap prefix signals (head of the prefix and the anchor
/// ending at the committed offset).
const PROBE_WINDOW: u64 = 8192;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Plan {
    /// No watermark yet: full capture, generation 1.
    Initial,
    /// Prefix verified: capture `[offset, end of last complete record)`.
    Append,
    /// Fingerprint discontinuity: full capture, new generation.
    Forced(&'static str),
    /// Nothing committable changed.
    Unchanged,
}

struct LineRec {
    /// Offset inside the object holding this run's bytes.
    rel_offset: u64,
    /// Offset inside the physical source file.
    abs_offset: u64,
    content_len: u64,
    sha256: String,
    /// 1-based physical line number in the source file.
    line_no: u64,
    malformed: bool,
}

struct Scan {
    records: Vec<LineRec>,
    /// Bytes up to and including the last complete line's newline.
    consumed: u64,
    /// Complete physical lines consumed (blank lines included).
    lines: u64,
    /// Trailing bytes without a terminator, held back for the next run.
    incomplete: u64,
    /// Time spent hashing record bytes (the `hash` phase; measured only while
    /// phase recording is active).
    hash_nanos: u64,
}

fn stat_len(path: &Path) -> Result<u64, ArchiveError> {
    let meta = fs::metadata(path).map_err(|e| AppError::io(path, e))?;
    Ok(meta.len())
}

fn probe_window_len(offset: u64) -> u64 {
    offset.min(PROBE_WINDOW)
}

/// Read `[start, start + len)` without going past EOF.
fn read_window(path: &Path, start: u64, len: u64) -> Result<Vec<u8>, ArchiveError> {
    if len == 0 {
        return Ok(Vec::new());
    }
    let mut file = File::open(path).map_err(|e| AppError::io(path, e))?;
    file.seek(SeekFrom::Start(start))
        .map_err(|e| AppError::io(path, e))?;
    let mut buf = vec![0u8; len as usize];
    let mut filled = 0usize;
    while filled < buf.len() {
        let read = file
            .read(&mut buf[filled..])
            .map_err(|e| AppError::io(path, e))?;
        if read == 0 {
            break;
        }
        filled += read;
    }
    buf.truncate(filled);
    Ok(buf)
}

fn read_from(path: &Path, offset: u64) -> Result<Vec<u8>, ArchiveError> {
    let mut file = File::open(path).map_err(|e| AppError::io(path, e))?;
    file.seek(SeekFrom::Start(offset))
        .map_err(|e| AppError::io(path, e))?;
    let mut out = Vec::new();
    file.read_to_end(&mut out).map_err(|e| AppError::io(path, e))?;
    Ok(out)
}

// ---------------------------------------------------------------------------
// Test seam: concurrent-writer simulation
// ---------------------------------------------------------------------------

type PostReadHook = fn(&Path);

static POST_READ_HOOK: std::sync::Mutex<Option<PostReadHook>> = std::sync::Mutex::new(None);

/// Install a hook that runs after the byte range is read and before the
/// post-read source-consistency verification.
///
/// A single-process test cannot race the collector, so the "writer changed the
/// file mid-read" degradation path would otherwise be untestable. The hook may
/// only be installed by tests; it exists so the *test* can play the concurrent
/// writer. The collector itself still only reads the source.
pub fn set_post_read_hook(hook: Option<PostReadHook>) {
    let mut guard = POST_READ_HOOK.lock().unwrap_or_else(|p| p.into_inner());
    *guard = hook;
}

fn run_post_read_hook(source: &Path) {
    let hook = {
        let guard = POST_READ_HOOK.lock().unwrap_or_else(|p| p.into_inner());
        *guard
    };
    if let Some(hook) = hook {
        hook(source);
    }
}

/// Split a byte range into complete JSONL records. A range that does not end
/// with a newline keeps its trailing partial record for the next run; it is
/// never counted as captured.
fn scan_chunk(
    chunk: &[u8],
    base_offset: u64,
    first_line_no: u64,
    source: &Path,
) -> Result<Scan, ArchiveError> {
    let mut spans = LineSpans::new(std::io::Cursor::new(chunk));
    let mut records = Vec::new();
    let mut consumed = 0u64;
    let mut lines = 0u64;
    let measuring = cost::is_recording();
    let mut hash_nanos = 0u64;
    while let Some(line) = spans
        .next_line()
        .map_err(|e| AppError::io(source, e))?
    {
        let start = line.span.offset;
        let raw_len = line.span.len as u64;
        let end = start.saturating_add(raw_len);
        if end == 0 || end > chunk.len() as u64 {
            break;
        }
        if chunk[(end - 1) as usize] != b'\n' {
            // Incomplete tail record: stop here, do not consume it.
            break;
        }
        lines += 1;
        consumed = end;
        let bytes = line.bytes;
        if bytes.iter().all(|b| b.is_ascii_whitespace()) {
            continue;
        }
        let sha256 = if measuring {
            let started = std::time::Instant::now();
            let digest = hex_sha256(bytes);
            hash_nanos = hash_nanos.saturating_add(started.elapsed().as_nanos() as u64);
            digest
        } else {
            hex_sha256(bytes)
        };
        records.push(LineRec {
            rel_offset: start,
            abs_offset: base_offset.saturating_add(start),
            content_len: bytes.len() as u64,
            sha256,
            line_no: first_line_no.saturating_add(lines - 1),
            malformed: serde_json::from_slice::<serde_json::Value>(bytes).is_err(),
        });
    }
    Ok(Scan {
        records,
        consumed,
        lines,
        incomplete: (chunk.len() as u64).saturating_sub(consumed),
        hash_nanos,
    })
}

fn decide_plan(source: &Path, wm: &Watermark, len_now: u64) -> Result<Plan, ArchiveError> {
    if len_now < wm.offset {
        // Length regression / truncation: the old offsets mean nothing now.
        return Ok(Plan::Forced("length_regression"));
    }
    let window = if wm.probe_len > 0 && wm.probe_len <= wm.offset {
        wm.probe_len
    } else {
        probe_window_len(wm.offset)
    };
    let anchor = hex_sha256(&read_window(
        source,
        wm.offset.saturating_sub(window),
        window,
    )?);
    let head = hex_sha256(&read_window(source, 0, window)?);
    let prefix_intact = anchor == wm.anchor_probe && head == wm.head_probe;
    Ok(match (prefix_intact, len_now == wm.offset) {
        (true, true) => Plan::Unchanged,
        (true, false) => Plan::Append,
        // Same length but different bytes: an in-place rewrite. A grown file
        // whose committed prefix changed is a rewrite/rotation at the same
        // path. Either way the old offset is stale — never resume on it.
        (false, true) => Plan::Forced("same_length_rewrite"),
        (false, false) => Plan::Forced("prefix_rewritten"),
    })
}

fn entry_for<'a>(
    state: &'a mut SourcesFile,
    source_key: &str,
    provider: &str,
) -> &'a mut SourceState {
    let entry = state
        .sources
        .entry(source_key.to_string())
        .or_insert_with(|| SourceState {
            provider: provider.to_string(),
            ..SourceState::default()
        });
    entry.provider = provider.to_string();
    entry
}

/// Record the outcome of a run that commits nothing (unchanged / degraded).
fn record_noop(
    archive: &Archive,
    state: &mut SourcesFile,
    source_key: &str,
    provider: &str,
    report: &CaptureReport,
) -> Result<(), ArchiveError> {
    let entry = entry_for(state, source_key, provider);
    entry.status = report.status.clone();
    entry.change = report.change.clone();
    entry.errors = entry.errors.saturating_add(report.errors);
    entry.incomplete_tail_bytes = report.incomplete_tail_bytes;
    entry.observed_len = Some(report.file_len);
    entry.anomalies = report.anomalies.clone();
    entry.last_error = report.error.clone();
    archive.save_sources(state)
}

/// Capture one plain JSONL source incrementally. The caller holds the
/// runtime-root single-writer lock and has already resolved the canonical
/// source key.
pub(crate) fn capture_jsonl_file(
    archive: &Archive,
    source: &Path,
    source_key: &str,
    provider: &str,
    domain: &str,
    failpoint: Option<Failpoint>,
    mut state: SourcesFile,
    mut report: CaptureReport,
) -> Result<CaptureReport, ArchiveError> {
    report.source_kind = Some("jsonl_file".to_string());

    let prev_state = state.sources.get(source_key).cloned().unwrap_or_default();
    let prev_wm = prev_state.watermark.clone();
    let len_now = stat_len(source)?;
    report.file_len = len_now;

    let plan = match &prev_wm {
        None => Plan::Initial,
        Some(wm) if wm.anchor_probe.is_empty() => {
            // A watermark without P2 probes cannot be verified; never resume on it.
            Plan::Forced("legacy_watermark_without_probes")
        }
        Some(wm) => decide_plan(source, wm, len_now)?,
    };

    // ---- nothing committable changed -------------------------------------
    if plan == Plan::Unchanged {
        let wm = prev_wm.clone().expect("Unchanged requires a watermark");
        report.ok = true;
        report.status = "skipped_unchanged".to_string();
        report.change = "skipped_unchanged".to_string();
        report.sha256 = Some(wm.sha256.clone());
        report.watermark_len = Some(wm.offset);
        report.offset = wm.offset;
        report.line = wm.line;
        report.generation = wm.generation;
        report.bytes = wm.len;
        report.records_discovered = wm.records;
        report.records_deduped = wm.records;
        report.incomplete_tail_bytes = len_now.saturating_sub(wm.offset);
        record_noop(archive, &mut state, source_key, provider, &report)?;
        return Ok(report);
    }

    let full = !matches!(plan, Plan::Append);
    let generation = if full {
        prev_wm
            .as_ref()
            .map(|wm| wm.generation.max(1) + 1)
            .unwrap_or(1)
    } else {
        prev_wm
            .as_ref()
            .map(|wm| wm.generation.max(1))
            .unwrap_or(1)
    };
    let range_start = match (&plan, &prev_wm) {
        (Plan::Append, Some(wm)) => wm.offset,
        _ => 0,
    };

    // Capture fact: the file as it looked when this run decided what to read.
    // Taken before the chunk read so the manifest can never describe bytes that
    // land after the read; never recomputed after the commit point either.
    let file_fingerprint = crate::capture::fingerprint(source)?;
    let chunk = {
        let _read = cost::phase("read");
        if full {
            fs::read(source).map_err(|e| AppError::io(source, e))?
        } else {
            read_from(source, range_start)?
        }
    };

    let first_line_no = match (&plan, &prev_wm) {
        (Plan::Append, Some(wm)) => wm.line + 1,
        _ => 1,
    };
    let scan_started = std::time::Instant::now();
    let scan = scan_chunk(&chunk, range_start, first_line_no, source)?;
    let scan_elapsed = scan_started.elapsed();
    let hash_elapsed = std::time::Duration::from_nanos(scan.hash_nanos);
    cost::record_duration("hash", hash_elapsed);
    cost::record_duration("parse", scan_elapsed.saturating_sub(hash_elapsed));

    // ---- append with nothing complete yet: hold the tail, advance nothing --
    // Only the append path may stop here. A forced recapture (truncation /
    // rewrite) whose new content happens to have no terminator must still be
    // committed and reported — otherwise a truncation would be silently
    // reported as `skipped_unchanged` and never reach the coverage report.
    // (A brand-new source with no complete record takes the normal path below
    // and commits a zero-record watermark, so the file is tracked.)
    if scan.consumed == 0 && matches!(plan, Plan::Append) {
        report.ok = true;
        report.status = "skipped_unchanged".to_string();
        report.change = "skipped_unchanged".to_string();
        report.incomplete_tail_bytes = scan.incomplete;
        if let Some(wm) = &prev_wm {
            report.sha256 = Some(wm.sha256.clone());
            report.watermark_len = Some(wm.offset);
            report.offset = wm.offset;
            report.line = wm.line;
            report.generation = wm.generation;
            report.records_discovered = wm.records;
            report.records_deduped = wm.records;
        }
        record_noop(archive, &mut state, source_key, provider, &report)?;
        return Ok(report);
    }

    // ---- post-read verification (the source may be written while we read) --
    run_post_read_hook(source);
    let len_after = stat_len(source)?;
    let committed_end = range_start.saturating_add(scan.consumed);
    let degraded = if len_after < committed_end {
        Some("source_shrank_during_read")
    } else if full {
        // Full capture: the bytes we read must still be the file's bytes.
        let window = probe_window_len(committed_end);
        let head_now = read_window(source, 0, window)?;
        if hex_sha256(&head_now) != hex_sha256(&chunk[..window as usize]) {
            Some("source_rewritten_during_read")
        } else {
            None
        }
    } else {
        // Append: the verified prefix must still start at the same place.
        let window = probe_window_len(range_start);
        let anchor_now = hex_sha256(&read_window(
            source,
            range_start.saturating_sub(window),
            window,
        )?);
        match &prev_wm {
            Some(wm) if anchor_now != wm.anchor_probe => Some("source_rewritten_during_read"),
            _ => None,
        }
    };
    if let Some(reason) = degraded {
        report.ok = false;
        report.status = "degraded".to_string();
        report.change = "degraded".to_string();
        report.change_reason = Some(reason.to_string());
        report.errors = 1;
        report.error = Some(format!(
            "cannot classify the source safely this run ({reason}); watermark not advanced"
        ));
        report.anomalies.push(format!("degraded:{reason}"));
        if let Some(wm) = &prev_wm {
            report.offset = wm.offset;
            report.line = wm.line;
            report.generation = wm.generation;
            report.watermark_len = Some(wm.offset);
            report.incomplete_tail_bytes = len_now.saturating_sub(wm.offset);
        }
        record_noop(archive, &mut state, source_key, provider, &report)?;
        return Ok(report);
    }

    // ---- (1) capture object: complete-record bytes only --------------------
    let committed_bytes = chunk[..scan.consumed as usize].to_vec();
    // A run may legitimately commit zero bytes (a source whose first line is
    // still incomplete): no object is written, but the watermark is still
    // committed so the file is tracked.
    let (object_sha, objects) = if committed_bytes.is_empty() {
        (String::new(), Vec::new())
    } else {
        // JSONL segments are highly compressible: zstd level 3 with the plain
        // fallback when the encoding is not worth it (S2.5).
        let (sha, is_new) = {
            let _write = cost::phase("compress_write");
            let (sha, _, is_new, _) =
                archive.put_object_encoded(&committed_bytes, StorageHint::Compressible)?;
            // Landed-and-verified: read the object back through its content
            // address before the manifest commit point. Cost is proportional to
            // this run's delta, never to the whole file.
            archive.read_object_bytes(&sha).map_err(|e| {
                ArchiveError::Integrity(format!("object {sha} failed write verification: {e}"))
            })?;
            (sha, is_new)
        };
        if is_new {
            report.objects_new += 1;
        } else {
            report.objects_deduped += 1;
        }
        let role = if range_start == 0 {
            "whole_file"
        } else {
            "append_range"
        };
        (
            sha.clone(),
            vec![ManifestObject {
                sha256: sha,
                bytes: committed_bytes.len() as u64,
                role: role.to_string(),
                access: vec![domain.to_string()],
                source_offset: Some(range_start),
                generation,
            }],
        )
    };
    report.bytes = committed_bytes.len() as u64;
    if !object_sha.is_empty() {
        report.sha256 = Some(object_sha.clone());
    }

    let mut seen_in_run: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut records: Vec<ManifestRecord> = Vec::new();
    let mut new_in_chunk = 0u64;
    let mut dup_in_chunk = 0u64;
    let mut malformed = 0u64;
    let prefix_records = match (&plan, &prev_wm) {
        (Plan::Append, Some(wm)) => wm.records,
        _ => 0,
    };
    let mut anomalies: Vec<String> = Vec::new();
    for rec in &scan.records {
        if rec.malformed {
            malformed += 1;
            anomalies.push(format!("malformed_json:line={}", rec.line_no));
        }
        let known = archive.records_contains(&rec.sha256)?
            || !seen_in_run.insert(rec.sha256.clone());
        if known {
            dup_in_chunk += 1;
        } else {
            new_in_chunk += 1;
        }
        records.push(ManifestRecord {
            seq: prefix_records + records.len() as u64,
            offset: rec.rel_offset,
            len: rec.content_len,
            sha256: rec.sha256.clone(),
            object: object_sha.clone(),
            access: vec![domain.to_string()],
            source_offset: Some(rec.abs_offset),
            source_line: Some(rec.line_no),
        });
    }
    report.records_new = new_in_chunk;
    // `recordsDeduped` = records covered by this source that were not committed
    // again in this run: the already-committed prefix (not re-read, because the
    // append path never rescans it) plus hash duplicates inside this run.
    report.records_deduped = prefix_records + dup_in_chunk;
    report.records_discovered = prefix_records + scan.records.len() as u64;
    report.records_malformed = malformed;
    report.errors = malformed;
    report.anomalies = anomalies;

    let new_offset = committed_end;
    let new_line = match (&plan, &prev_wm) {
        (Plan::Append, Some(wm)) => wm.line + scan.lines,
        _ => scan.lines,
    };
    let new_records = prefix_records + scan.records.len() as u64;
    // Use the post-read length: reading to EOF may have picked up bytes written
    // while we read. Recording the pre-read length would leave `len < offset`
    // and make the next run flag a bogus length regression.
    report.file_len = len_after;
    let incomplete = len_after.saturating_sub(new_offset);

    // Parse probe: a derived view, failures are health state. Only full
    // captures probe (an append must stay proportional to the delta, §4.4).
    if full {
        report.parse.attempted = true;
        // §A: the probe re-parses the entire file; it is its own stage (and
        // its byte count is the whole file, not the delta).
        let _probe = cost::phase("parse_probe");
        cost::counter("parse_probe.full_reads", 1);
        cost::counter("parse_probe.bytes", len_after);
        match crate::load_messages(provider, source_key) {
            Ok(messages) => report.parse.messages = Some(messages.len() as u64),
            Err(e) => report.parse.error = Some(e),
        }
    }

    let change = match &plan {
        Plan::Initial => "initial",
        Plan::Append => "append",
        Plan::Forced(_) => "forced_recapture",
        Plan::Unchanged => "skipped_unchanged",
    };
    report.change = change.to_string();
    report.change_reason = match &plan {
        Plan::Forced(reason) => Some((*reason).to_string()),
        _ => None,
    };
    if let Plan::Forced(reason) = &plan {
        report
            .anomalies
            .insert(0, format!("discontinuity:{reason}"));
    }

    // ---- (2) manifest commit point ----------------------------------------
    if failpoint == Some(Failpoint::BeforeManifestCommit) {
        panic!("agent-memory failpoint: before_manifest_commit");
    }
    let manifest = Manifest {
        schema: MANIFEST_SCHEMA.to_string(),
        capture_id: new_id("cap"),
        captured_at_ms: now_ms(),
        device_id: archive.device_id()?,
        provider: provider.to_string(),
        source: ManifestSource {
            kind: "jsonl_file".to_string(),
            path: source_key.to_string(),
            compressed: false,
            fingerprint: file_fingerprint.clone(),
            source_state_at_capture: "present".to_string(),
            capture_reason: change.to_string(),
            generation,
            range_start,
            carrier: None,
            sqlite: None,
        },
        objects,
        records: records.clone(),
        attachments: Vec::new(),
    };
    archive.commit_manifest(&manifest)?;

    if failpoint == Some(Failpoint::AfterManifestBeforeState) {
        panic!("agent-memory failpoint: after_manifest_before_state");
    }

    // ---- (3) record index + watermark (only after the commit point) --------
    for rec in &records {
        archive.stage_record_ref(
            &rec.sha256,
            RecordRef {
                object: rec.object.clone(),
                offset: rec.offset,
                len: rec.len,
                capture_id: manifest.capture_id.clone(),
            },
        )?;
    }
    archive.flush_records()?;

    // Probes describe the bytes we actually committed, not whatever the live
    // file happens to hold now: computing them from the verified chunk closes
    // the "writer mutates the prefix after our consistency check" window.
    let window = probe_window_len(new_offset);
    let window_usize = window as usize;
    let head_probe = if range_start == 0 {
        // Full capture: the whole committed prefix is in hand.
        hex_sha256(&committed_bytes[..window_usize])
    } else if let Some(wm) = prev_wm.as_ref() {
        if window == wm.probe_len {
            // A verified append cannot change the head of the prefix.
            wm.head_probe.clone()
        } else {
            // The window grew past the previous one (offset crossed the probe
            // window during this run). Cheap one-off read; the prefix head was
            // verified by the plan decision moments ago.
            hex_sha256(&read_window(source, 0, window)?)
        }
    } else {
        hex_sha256(&read_window(source, 0, window)?)
    };
    let anchor_probe = if window_usize <= committed_bytes.len() {
        hex_sha256(&committed_bytes[committed_bytes.len() - window_usize..])
    } else {
        // The anchor window starts before this run's delta; the older part is
        // the already-verified prefix. Bounded-verification caveat: a writer
        // that rewrites exactly that older part *during* this run is not seen.
        hex_sha256(&read_window(
            source,
            new_offset.saturating_sub(window),
            window,
        )?)
    };

    let entry = entry_for(&mut state, source_key, provider);
    entry.status = "captured".to_string();
    entry.change = change.to_string();
    entry.generation = generation;
    entry.discovered = report.records_discovered;
    entry.captured = entry.captured.saturating_add(report.records_new);
    entry.skipped = entry.skipped.saturating_add(report.records_deduped);
    entry.bytes = entry.bytes.saturating_add(report.bytes);
    entry.errors = entry.errors.saturating_add(report.errors);
    entry.incomplete_tail_bytes = incomplete;
    entry.observed_len = Some(len_after);
    entry.anomalies = report.anomalies.clone();
    entry.watermark = Some(Watermark {
        len: len_after,
        // sha256 of this run's committed bytes; the empty-content digest when a
        // run committed no object (source with no complete record yet).
        sha256: hex_sha256(&committed_bytes),
        fingerprint: file_fingerprint,
        capture_id: manifest.capture_id.clone(),
        captured_at_ms: manifest.captured_at_ms,
        offset: new_offset,
        line: new_line,
        records: new_records,
        generation,
        head_probe,
        anchor_probe,
        probe_len: window,
    });
    entry.last_error = None;
    if report.parse.attempted {
        entry.last_parse_error = report.parse.error.clone();
    }
    archive.save_sources(&state)?;

    report.ok = true;
    report.status = "captured".to_string();
    report.capture_id = Some(manifest.capture_id.clone());
    report.generation = generation;
    report.offset = new_offset;
    report.line = new_line;
    report.incomplete_tail_bytes = incomplete;
    report.watermark_len = Some(new_offset);
    // One object-directory walk per round: inside a capture-all round the
    // reconcile is deferred to the round end (and reported there).
    if let Some(reconcile) = archive.reconcile_after_unit()? {
        report.orphan_objects = reconcile.orphan_objects;
    }
    Ok(report)
}
