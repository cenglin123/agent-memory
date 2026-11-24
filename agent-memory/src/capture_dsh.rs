//! DSH (DeepSeek Harness) archive capture: logical JSONL lines decoded from a
//! checksummed Zstandard frame stream.
//!
//! # What is archived
//!
//! The evidence unit is one **decompressed logical JSONL line**. The physical
//! carrier (one or more Zstandard frames) is never stored verbatim: the archive
//! holds exact slices of the logical stream, so a record can be read back and
//! projected without re-implementing DSH's frame framing. Decoding is delegated
//! to the single reader in `recall::providers::dsh` (the same code the live
//! search, index and readback use); this module never opens a frame itself.
//!
//! * A torn trailing frame contributes only its complete lines: the incomplete
//!   tail is not archived and is reported as `partial_tail`.
//! * An interior frame whose boundary is unreadable is skipped (the reader
//!   resynchronizes on the next readable frame): its lines are not archived and
//!   the carrier is counted `corrupt`. A frame that parses but fails to decode
//!   ends the readable stream there; the prefix before it is archived.
//!
//! # Incremental checkpoint
//!
//! Per physical generation file (`SourceState.dsh`, one definition):
//!
//! * `rel_path` — the carrier relative path (a generation switch changes it);
//! * `lines` — complete logical lines committed;
//! * `last_line_sha256` — digest of the last committed logical line.
//!
//! The watermark additionally carries the logical prefix end (`offset`), the
//! digest of the whole committed prefix (`sha256`) and the generation.
//!
//! | observation | classification |
//! |---|---|
//! | no checkpoint | `initial` (full capture) |
//! | checkpoint without a carrier path | `forced_recapture` (`legacy_checkpoint`) |
//! | carrier path differs | `forced_recapture` (`generation_switch`) |
//! | fewer logical lines than committed | `forced_recapture` (`logical_lines_shrunk`) |
//! | last committed line digest differs | `forced_recapture` (`last_line_rewritten`) |
//! | committed prefix digest differs | `forced_recapture` (`prefix_rewritten`) |
//! | same line count, everything matches | `skipped_unchanged` |
//! | more lines, prefix intact | `append` from the committed prefix |
//!
//! A forced recapture bumps the manifest generation and writes a new object;
//! earlier generations' manifests and objects stay on disk (never deleted), so
//! old evidence remains readable. The caller records the old→new carrier link in
//! its report/ledger.
//!
//! # Honest limits
//!
//! Detection reads the whole logical stream (bounded by the reader's 64 MiB
//! logical ceiling) because a frame cannot be addressed by logical line. That is
//! O(file) per run for this source, unlike the byte-offset JSONL collector; the
//! committed evidence is still append-only and the archive cost stays
//! proportional to the delta. A log above the logical ceiling is archived up to
//! the ceiling and reported `degraded` — the collector never claims to have
//! captured beyond what it could decode.

use std::path::Path;

use crate::archive::{
    hex_sha256, new_id, now_ms, Archive, ArchiveError, DshState, Manifest, ManifestObject,
    ManifestRecord, ManifestSource, RecordRef, SourceState, SourcesFile, StorageHint, Watermark,
};
use crate::capture::{canonical_key, fingerprint, CaptureReport, Failpoint};
use crate::cost;
use crate::error::AppError;
use crate::recall::providers::dsh;

/// Manifest `source.kind` of a DSH carrier (append-aware, logical byte ranges).
pub const DSH_CAPTURE_KIND: &str = "dsh_jsonl";

/// Collector facts a driver needs for its report/ledger (never session bodies).
#[derive(Debug, Clone, Default)]
pub struct DshCaptureFacts {
    /// Carrier relative path of this run.
    pub rel_path: String,
    /// Carrier relative path of the previous checkpoint when it differs.
    pub previous_rel_path: Option<String>,
    /// Discontinuity reason of this run (`generation_switch`, `last_line_rewritten`, ...).
    pub change_reason: Option<String>,
    /// Complete logical lines covered by the checkpoint after this run.
    pub logical_lines: u64,
    /// Digest of the last committed logical line.
    pub last_line_sha256: String,
    /// The trailing frame was torn or failed its checksum.
    pub partial_tail: bool,
    /// An interior frame is unreadable (lines after it are not archived).
    pub corrupt: bool,
    /// The logical read ceiling was hit; only the prefix is archived.
    pub truncated: bool,
    /// Newly committed lines that are not valid JSON.
    pub malformed_lines: u64,
}

/// One complete, non-blank logical line selected for this run's object range.
struct LineEntry {
    /// 0-based logical line index in the carrier.
    index: u64,
    /// Byte offset of the line inside the logical stream.
    start: u64,
    bytes: Vec<u8>,
    sha256: String,
    malformed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Plan {
    Initial,
    Append,
    Forced(&'static str),
    Unchanged,
}

fn stat(path: &Path) -> Result<(u64, i64), ArchiveError> {
    let meta = std::fs::metadata(path).map_err(|e| AppError::io(path, e))?;
    let mtime_ms = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .and_then(|d| i64::try_from(d.as_millis()).ok())
        .unwrap_or(0);
    Ok((meta.len(), mtime_ms))
}

/// Mark a source that cannot be read at all; the previous watermark is kept.
fn fail_without_state(
    archive: &Archive,
    state: &mut SourcesFile,
    source_key: &str,
    provider: &str,
    message: String,
    report: &mut CaptureReport,
) -> Result<(), ArchiveError> {
    let entry = state.sources.entry(source_key.to_string()).or_insert_with(|| SourceState {
        provider: provider.to_string(),
        ..SourceState::default()
    });
    entry.provider = provider.to_string();
    entry.status = "failed".to_string();
    entry.change = "failed".to_string();
    entry.last_error = Some(message.clone());
    entry.errors = entry.errors.saturating_add(1);
    report.ok = false;
    report.status = "failed".to_string();
    report.change = "failed".to_string();
    report.errors = report.errors.saturating_add(1);
    report.error = Some(message);
    archive.save_sources(state)
}

/// Stable source key of a DSH carrier: its **session directory**.
///
/// The carrier file name changes when DSH publishes a new format generation, so
/// keying by the file would turn a migration into a brand-new source. The
/// directory is the session's stable identity; the carrier path is part of the
/// incremental checkpoint instead.
pub fn source_key(carrier: &Path) -> String {
    canonical_key(carrier.parent().unwrap_or(carrier))
}

/// Capture one DSH generation carrier.
///
/// The caller holds (or lets this function acquire, when `lock` is true) the
/// runtime-root single-writer lock.
///
/// Returns the capture report plus the collector facts the driver needs. A
/// decode/read failure is an `Err` with the source state left untouched; a
/// classified-but-degraded run (logical ceiling) commits the readable prefix and
/// returns `Ok` with `report.ok = false`.
pub fn capture_dsh_source(
    archive: &Archive,
    source: &Path,
    rel_path: &str,
    provider: &str,
    domain: &str,
    failpoint: Option<Failpoint>,
) -> Result<(CaptureReport, DshCaptureFacts), ArchiveError> {
    let _lock = archive.acquire_lock()?;
    let source_key = source_key(source);
    let mut report = CaptureReport::new(provider, source_key.clone());
    let mut facts = DshCaptureFacts {
        rel_path: rel_path.to_string(),
        ..DshCaptureFacts::default()
    };
    let mut state = archive.load_sources()?;
    let had_watermark = state
        .sources
        .get(&source_key)
        .and_then(|entry| entry.watermark.as_ref())
        .is_some();

    if !source.exists() {
        // Source removal never deletes archived evidence; it is a source state.
        let entry = state.sources.entry(source_key.clone()).or_insert_with(|| SourceState {
            provider: provider.to_string(),
            ..SourceState::default()
        });
        entry.provider = provider.to_string();
        entry.observed_len = Some(0);
        entry.incomplete_tail_bytes = 0;
        if had_watermark {
            entry.status = "source_missing".to_string();
            entry.change = "source_missing".to_string();
            entry.last_error = None;
            entry.anomalies.clear();
            report.status = "source_missing".to_string();
            report.change = "source_missing".to_string();
            report.ok = true;
        } else {
            entry.status = "failed".to_string();
            entry.change = "failed".to_string();
            entry.anomalies.clear();
            entry.last_error = Some("source not found before any successful capture".into());
            entry.errors = entry.errors.saturating_add(1);
            report.error = Some("source not found".to_string());
            report.errors = 1;
            report.change = "failed".to_string();
        }
        archive.save_sources(&state)?;
        return Ok((report, facts));
    }

    let (physical_len, _mtime_ms) = stat(source)?;
    let opened = {
        let _read = cost::phase("read");
        dsh::open_log(source, rel_path)
    };
    let log = match opened {
        Ok(log) => log,
        Err(message) => {
            fail_without_state(
                archive,
                &mut state,
                &source_key,
                provider,
                message,
                &mut report,
            )?;
            return Ok((report, facts));
        }
    };
    facts.partial_tail = log.state == dsh::DshReadState::PartialTail;
    facts.truncated = log.truncated;
    facts.corrupt = log.state == dsh::DshReadState::Corrupt && !log.truncated;

    let logical = log.logical_bytes();
    let prefix_state = state
        .sources
        .get(&source_key)
        .and_then(|entry| entry.dsh.clone())
        .filter(|dsh| !dsh.rel_path.is_empty());
    let previous = state
        .sources
        .get(&source_key)
        .and_then(|entry| entry.watermark.clone());

    let mut line_count = 0u64;
    let mut logical_end = 0u64;
    let mut last_line_sha256 = hex_sha256(b"");
    let mut observed_checkpoint_line_sha256: Option<String> = None;
    let checkpoint_line = prefix_state.as_ref().map(|dsh| dsh.lines);
    let mut entries: Vec<LineEntry> = Vec::new();
    let measuring = cost::is_recording();
    let mut hash_nanos = 0u64;
    let mut parse_nanos = 0u64;
    for line in log.lines() {
        line_count += 1;
        logical_end = line.end;
        let digest = if measuring {
            let started = std::time::Instant::now();
            let digest = hex_sha256(line.bytes);
            hash_nanos = hash_nanos.saturating_add(started.elapsed().as_nanos() as u64);
            digest
        } else {
            hex_sha256(line.bytes)
        };
        last_line_sha256 = digest.clone();
        if checkpoint_line == Some(line.index + 1) {
            observed_checkpoint_line_sha256 = Some(digest.clone());
        }
        if line.bytes.iter().all(|byte| byte.is_ascii_whitespace()) {
            continue;
        }
        let malformed = if measuring {
            let started = std::time::Instant::now();
            let malformed = serde_json::from_slice::<serde_json::Value>(line.bytes).is_err();
            parse_nanos = parse_nanos.saturating_add(started.elapsed().as_nanos() as u64);
            malformed
        } else {
            serde_json::from_slice::<serde_json::Value>(line.bytes).is_err()
        };
        entries.push(LineEntry {
            index: line.index,
            start: line.start,
            bytes: line.bytes.to_vec(),
            sha256: digest,
            malformed,
        });
    }
    cost::record_duration("hash", std::time::Duration::from_nanos(hash_nanos));
    cost::record_duration("parse", std::time::Duration::from_nanos(parse_nanos));

    let plan = match (&prefix_state, &previous) {
        (None, _) | (_, None) => Plan::Initial,
        (Some(checkpoint), Some(watermark))
            if checkpoint.rel_path == rel_path
                && watermark.line == 0
                && watermark.offset == 0 =>
        {
            // A tracked source that has never committed a line: the prefix is
            // empty, so growth is an ordinary append.
            if line_count == 0 {
                Plan::Unchanged
            } else {
                Plan::Append
            }
        }
        (Some(checkpoint), Some(watermark)) => {
            if checkpoint.rel_path != rel_path {
                Plan::Forced("generation_switch")
            } else if line_count < checkpoint.lines {
                Plan::Forced("logical_lines_shrunk")
            } else if checkpoint.lines > 0
                && observed_checkpoint_line_sha256.as_deref()
                    != Some(checkpoint.last_line_sha256.as_str())
            {
                Plan::Forced("last_line_rewritten")
            } else if (watermark.offset as usize) > logical.len()
                || hex_sha256(&logical[..watermark.offset as usize]) != watermark.sha256
            {
                Plan::Forced("prefix_rewritten")
            } else if line_count == checkpoint.lines {
                Plan::Unchanged
            } else {
                Plan::Append
            }
        }
    };

    if let Some(checkpoint) = &prefix_state {
        if checkpoint.rel_path != rel_path {
            facts.previous_rel_path = Some(checkpoint.rel_path.clone());
        }
    }

    let full = matches!(plan, Plan::Initial | Plan::Forced(_));
    let generation = if full {
        previous
            .as_ref()
            .map(|watermark| watermark.generation.max(1) + 1)
            .unwrap_or(1)
    } else {
        previous
            .as_ref()
            .map(|watermark| watermark.generation.max(1))
            .unwrap_or(1)
    };
    let range_start = match (&plan, &previous) {
        (Plan::Append, Some(watermark)) => watermark.offset,
        _ => 0,
    };

    // ---- nothing committable changed -------------------------------------
    if matches!(plan, Plan::Unchanged) {
        let watermark = previous.clone().expect("Unchanged requires a watermark");
        report.ok = !log.truncated;
        report.status = if log.truncated { "degraded" } else { "skipped_unchanged" }.to_string();
        report.change = if log.truncated { "degraded" } else { "skipped_unchanged" }.to_string();
        report.sha256 = Some(watermark.sha256.clone());
        report.watermark_len = Some(watermark.offset);
        report.offset = watermark.offset;
        report.line = watermark.line;
        report.generation = watermark.generation;
        report.bytes = watermark.len;
        report.records_discovered = watermark.records;
        report.records_deduped = watermark.records;
        report.file_len = physical_len;
        facts.logical_lines = line_count;
        facts.last_line_sha256 = last_line_sha256;
        if log.truncated {
            report.errors = report.errors.saturating_add(1);
            report.anomalies.push("logical_cap".to_string());
            report.error = Some(format!(
                "logical read ceiling reached; only the decoded prefix is archived ({rel_path})"
            ));
        }
        record_noop(archive, &mut state, &source_key, provider, &report, &facts)?;
        return Ok((report, facts));
    }

    // ---- (1) capture object: complete logical lines only ------------------
    let new_end = logical_end;
    let chunk_start = range_start.min(new_end);
    let chunk: Vec<u8> = logical[chunk_start as usize..new_end as usize].to_vec();
    let (object_sha, objects) = if chunk.is_empty() {
        (String::new(), Vec::new())
    } else {
        let (sha, is_new) = {
            let _write = cost::phase("compress_write");
            let (sha, _, is_new, _) =
                archive.put_object_encoded(&chunk, StorageHint::Compressible)?;
            // Landed-and-verified: read the object back through its content
            // address before the manifest commit point.
            archive.read_object_bytes(&sha).map_err(|error| {
                ArchiveError::Integrity(format!("object {sha} failed write verification: {error}"))
            })?;
            (sha, is_new)
        };
        if is_new {
            report.objects_new += 1;
        } else {
            report.objects_deduped += 1;
        }
        let role = if chunk_start == 0 { "whole_file" } else { "append_range" };
        (
            sha.clone(),
            vec![ManifestObject {
                sha256: sha,
                bytes: chunk.len() as u64,
                role: role.to_string(),
                access: vec![domain.to_string()],
                source_offset: Some(chunk_start),
                generation,
            }],
        )
    };
    report.bytes = chunk.len() as u64;
    if !object_sha.is_empty() {
        report.sha256 = Some(object_sha.clone());
    }

    let mut seen_in_run: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut records: Vec<ManifestRecord> = Vec::new();
    let mut new_in_chunk = 0u64;
    let mut dup_in_chunk = 0u64;
    let mut malformed = 0u64;
    let start_line = if matches!(plan, Plan::Append) {
        previous
            .as_ref()
            .map(|watermark| watermark.line)
            .unwrap_or(0)
    } else {
        0
    };
    let prefix_records = if matches!(plan, Plan::Append) {
        previous.as_ref().map(|watermark| watermark.records).unwrap_or(0)
    } else {
        0
    };
    let mut anomalies: Vec<String> = Vec::new();
    for entry in &entries {
        if entry.index < start_line {
            continue;
        }
        if entry.malformed {
            malformed += 1;
            anomalies.push(format!("malformed_json:line={}", entry.index + 1));
        }
        let known = archive.records_contains(&entry.sha256)?
            || !seen_in_run.insert(entry.sha256.clone());
        if known {
            dup_in_chunk += 1;
        } else {
            new_in_chunk += 1;
        }
        records.push(ManifestRecord {
            seq: prefix_records + records.len() as u64,
            offset: entry.start.saturating_sub(chunk_start),
            len: entry.bytes.len() as u64,
            sha256: entry.sha256.clone(),
            object: object_sha.clone(),
            access: vec![domain.to_string()],
            source_offset: Some(entry.start),
            source_line: Some(entry.index + 1),
        });
    }
    report.records_new = new_in_chunk;
    report.records_deduped = prefix_records + dup_in_chunk;
    report.records_discovered = prefix_records + records.len() as u64;
    report.records_malformed = malformed;
    report.errors = malformed;
    facts.malformed_lines = malformed;

    let new_line = line_count;
    let new_records = prefix_records + records.len() as u64;
    report.file_len = physical_len;
    report.incomplete_tail_bytes = 0;

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
    facts.change_reason = report.change_reason.clone();
    if let Plan::Forced(reason) = &plan {
        anomalies.insert(0, format!("discontinuity:{reason}"));
    }
    if facts.partial_tail {
        anomalies.push("partial_tail".to_string());
    }
    if facts.corrupt {
        anomalies.push("corrupt_frame".to_string());
    }
    if log.truncated {
        anomalies.push("logical_cap".to_string());
    }
    report.anomalies = anomalies;
    if facts.corrupt {
        report.errors = report.errors.saturating_add(1);
    }

    let file_fingerprint = fingerprint(source)?;

    // ---- (2) manifest commit point ----------------------------------------
    if failpoint == Some(Failpoint::BeforeManifestCommit) {
        panic!("agent-memory failpoint: before_manifest_commit");
    }
    let manifest = Manifest {
        schema: crate::archive::MANIFEST_SCHEMA.to_string(),
        capture_id: new_id("cap"),
        captured_at_ms: now_ms(),
        device_id: archive.device_id()?,
        provider: provider.to_string(),
        source: ManifestSource {
            kind: DSH_CAPTURE_KIND.to_string(),
            path: source_key.clone(),
            compressed: false,
            fingerprint: file_fingerprint.clone(),
            source_state_at_capture: "present".to_string(),
            capture_reason: change.to_string(),
            generation,
            range_start: chunk_start,
            carrier: Some(rel_path.to_string()),
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

    // ---- (3) record index + watermark (only after the commit point) -------
    for record in &records {
        archive.stage_record_ref(
            &record.sha256,
            RecordRef {
                object: record.object.clone(),
                offset: record.offset,
                len: record.len,
                capture_id: manifest.capture_id.clone(),
            },
        )?;
    }
    archive.flush_records()?;

    let prefix_bytes = &logical[..new_end as usize];
    let entry = state.sources.entry(source_key.clone()).or_insert_with(|| SourceState {
        provider: provider.to_string(),
        ..SourceState::default()
    });
    entry.provider = provider.to_string();
    entry.status = if log.truncated { "degraded" } else { "captured" }.to_string();
    entry.change = change.to_string();
    entry.generation = generation;
    entry.discovered = report.records_discovered;
    entry.captured = entry.captured.saturating_add(report.records_new);
    entry.skipped = entry.skipped.saturating_add(report.records_deduped);
    entry.bytes = entry.bytes.saturating_add(report.bytes);
    entry.errors = entry.errors.saturating_add(report.errors);
    entry.incomplete_tail_bytes = 0;
    entry.observed_len = Some(physical_len);
    entry.anomalies = report.anomalies.clone();
    entry.watermark = Some(Watermark {
        len: physical_len,
        sha256: hex_sha256(prefix_bytes),
        fingerprint: file_fingerprint,
        capture_id: manifest.capture_id.clone(),
        captured_at_ms: manifest.captured_at_ms,
        offset: new_end,
        line: new_line,
        records: new_records,
        generation,
        head_probe: String::new(),
        anchor_probe: String::new(),
        probe_len: 0,
    });
    entry.dsh = Some(DshState {
        rel_path: rel_path.to_string(),
        lines: new_line,
        last_line_sha256: last_line_sha256.clone(),
    });
    entry.last_error = None;
    entry.last_parse_error = None;
    archive.save_sources(&state)?;

    facts.logical_lines = new_line;
    facts.last_line_sha256 = last_line_sha256;
    report.capture_id = Some(manifest.capture_id.clone());
    report.generation = generation;
    report.offset = new_end;
    report.line = new_line;
    report.watermark_len = Some(new_end);
    report.ok = !log.truncated;
    report.status = if log.truncated { "degraded" } else { "captured" }.to_string();
    if log.truncated {
        report.error = Some(format!(
            "logical read ceiling reached; only the decoded prefix is archived ({rel_path})"
        ));
    }
    // One object-directory walk per round: inside a capture-all round the
    // reconcile is deferred to the round end (and reported there).
    if let Some(reconcile) = archive.reconcile_after_unit()? {
        report.orphan_objects = reconcile.orphan_objects;
    }
    Ok((report, facts))
}

/// Record the outcome of a run that commits nothing (unchanged / degraded).
fn record_noop(
    archive: &Archive,
    state: &mut SourcesFile,
    source_key: &str,
    provider: &str,
    report: &CaptureReport,
    facts: &DshCaptureFacts,
) -> Result<(), ArchiveError> {
    let entry = state.sources.entry(source_key.to_string()).or_insert_with(|| SourceState {
        provider: provider.to_string(),
        ..SourceState::default()
    });
    entry.provider = provider.to_string();
    entry.status = report.status.clone();
    entry.change = report.change.clone();
    entry.errors = entry.errors.saturating_add(report.errors);
    entry.observed_len = Some(report.file_len);
    entry.anomalies = report.anomalies.clone();
    entry.last_error = report.error.clone();
    if let Some(dsh) = entry.dsh.as_mut() {
        if dsh.rel_path == facts.rel_path {
            dsh.lines = facts.logical_lines;
            dsh.last_line_sha256 = facts.last_line_sha256.clone();
        }
    }
    archive.save_sources(state)
}
