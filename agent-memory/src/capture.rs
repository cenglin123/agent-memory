//! Capture engine: single-source full capture into the evidence archive.
//!
//! Order of operations is the atomicity contract:
//!   1. objects are staged + hashed + renamed into the object store;
//!   2. the manifest is committed (the commit point);
//!   3. only then are the record index and the source watermark advanced.
//! A crash before (2) leaves orphan objects (reported, never auto-deleted) and
//! an untouched watermark, so re-running the capture recovers cleanly.
//!
//! P1 scope: full capture of one source per run (file-level watermark). JSONL
//! incremental offsets, SQLite baseline+delta capture and attachment archiving
//! live in `capture_jsonl.rs` (P2), `capture_sqlite.rs` (P3) and
//! `attachment.rs` (P4) and are intentionally not implemented here.

use std::fs;
use std::io::Read;
use std::path::Path;

use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::archive::{
    hex_lower, hex_sha256, now_ms, new_id, Archive, ArchiveError, Manifest, ManifestObject,
    ManifestRecord, ManifestSource, RecordRef, SourceFingerprint, SourceState, StorageHint,
    Watermark,
};
use crate::codex_rollout_file;
use crate::cost;
use crate::error::AppError;
use crate::providers::utils::LineSpans;
use crate::SUPPORTED_PROVIDERS;

const FINGERPRINT_EDGE_BYTES: usize = 8192;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failpoint {
    /// Simulate a crash after objects are on disk but before the manifest commit.
    BeforeManifestCommit,
    /// Simulate a crash after the manifest commit but before the record index and
    /// the watermark are saved (P2 recovery path: rerun must not duplicate
    /// objects or messages, and must not skip the records).
    AfterManifestBeforeState,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ParseProbe {
    pub attempted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub messages: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CaptureReport {
    pub ok: bool,
    /// captured | skipped_unchanged | source_missing | failed | degraded
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capture_id: Option<String>,
    pub provider: String,
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_kind: Option<String>,
    #[serde(default)]
    pub objects_new: u64,
    #[serde(default)]
    pub objects_deduped: u64,
    #[serde(default)]
    pub records_discovered: u64,
    #[serde(default)]
    pub records_new: u64,
    #[serde(default)]
    pub records_deduped: u64,
    #[serde(default)]
    pub bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub watermark_len: Option<u64>,
    pub parse: ParseProbe,
    #[serde(default)]
    pub orphan_objects: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    // -- P2 incremental fields (additive; existing CLI consumers keep working) --
    /// Change classification for this run: initial | append | skipped_unchanged |
    /// forced_recapture | degraded.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub change: String,
    /// Why a discontinuity forced a recapture (`length_regression`,
    /// `prefix_rewritten`, `same_length_rewrite`, ...).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub change_reason: Option<String>,
    /// Capture generation for this physical source path (1-based).
    #[serde(default)]
    pub generation: u64,
    /// Committed byte offset after this run (end of the last complete record).
    #[serde(default)]
    pub offset: u64,
    /// Complete physical lines committed up to `offset`.
    #[serde(default)]
    pub line: u64,
    /// Whole source length observed at capture time.
    #[serde(default)]
    pub file_len: u64,
    /// Source bytes after the last complete record, held back for the next run.
    #[serde(default)]
    pub incomplete_tail_bytes: u64,
    /// New records whose bytes are not valid JSON (captured anyway, reported).
    #[serde(default)]
    pub records_malformed: u64,
    /// Error conditions observed in this run (malformed records, degradation).
    #[serde(default)]
    pub errors: u64,
    /// Explicit anomalies (never a silent skip).
    #[serde(default)]
    pub anomalies: Vec<String>,
    /// P3: SQLite evidence facts for this run (baseline/delta/unchanged).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sqlite: Option<crate::capture_sqlite::SqliteCaptureInfo>,
}

impl CaptureReport {
    pub(crate) fn new(provider: &str, source: String) -> Self {
        Self {
            ok: false,
            status: "failed".to_string(),
            capture_id: None,
            provider: provider.to_string(),
            source,
            source_kind: None,
            objects_new: 0,
            objects_deduped: 0,
            records_discovered: 0,
            records_new: 0,
            records_deduped: 0,
            bytes: 0,
            sha256: None,
            watermark_len: None,
            parse: ParseProbe {
                attempted: false,
                messages: None,
                error: None,
            },
            orphan_objects: Vec::new(),
            error: None,
            change: String::new(),
            change_reason: None,
            generation: 0,
            offset: 0,
            line: 0,
            file_len: 0,
            incomplete_tail_bytes: 0,
            records_malformed: 0,
            errors: 0,
            anomalies: Vec::new(),
            sqlite: None,
        }
    }
}

/// Options for one capture run (all default-off; additive to the P2 surface).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CaptureOptions {
    /// SQLite only: capture sessions whose last update is at/after this epoch
    /// millisecond, plus their child rows (`--session-since <rfc3339>`).
    /// `project` is always captured in full. Changing the value re-baselines.
    pub session_since_ms: Option<i64>,
    /// SQLite only: force a new chunked baseline (`--rebaseline`).
    pub rebaseline: bool,
    /// SQLite only: run the blind-window verification pass after the capture
    /// unit (`--verify`); mismatching rows are committed as correction deltas.
    pub verify: bool,
    /// SQLite only (S4.5): capture-profile override by
    /// [`crate::capture_sqlite::SqliteCaptureProfile::profile_id`], resolved
    /// through the profile registry. `None` (production) selects the profile
    /// from the provider (`profile_for_provider`); an unknown id fails the
    /// capture instead of falling back.
    pub sqlite_profile: Option<&'static str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SourceKind {
    JsonlFile,
    CompressedJsonlFile,
    WholeFile,
    /// P3: a SQLite database (baseline snapshot + raw row-level delta).
    Sqlite,
}

impl SourceKind {
    fn as_str(&self) -> &'static str {
        match self {
            Self::JsonlFile => "jsonl_file",
            Self::CompressedJsonlFile => "compressed_jsonl_file",
            Self::WholeFile => "whole_file",
            Self::Sqlite => "sqlite",
        }
    }
}

fn classify(provider: &str, path: &Path) -> SourceKind {
    if codex_rollout_file::is_compressed(path) {
        return SourceKind::CompressedJsonlFile;
    }
    // OpenCode keeps its sessions in a SQLite database (V1/V2 generations); the
    // evidence model for it is snapshot + row delta, never a whole-file copy.
    // A `.db` name also routes here so a damaged header becomes an explicit
    // `sqlite_unavailable` failure instead of a silent whole-file success.
    if provider == "opencode"
        && (crate::capture_sqlite::is_sqlite_file(path)
            || crate::capture_sqlite::looks_like_sqlite_path(path))
    {
        return SourceKind::Sqlite;
    }
    let is_jsonl = path.extension().and_then(|e| e.to_str()) == Some("jsonl");
    match provider {
        "claude" | "codex" if is_jsonl => SourceKind::JsonlFile,
        _ => SourceKind::WholeFile,
    }
}

fn edge_hash(path: &Path, len: u64, tail: bool) -> Result<String, AppError> {
    use std::io::Seek;
    let mut file = fs::File::open(path).map_err(|e| AppError::io(path, e))?;
    if tail && len > FINGERPRINT_EDGE_BYTES as u64 {
        file.seek(std::io::SeekFrom::End(-(FINGERPRINT_EDGE_BYTES as i64)))
            .map_err(|e| AppError::io(path, e))?;
    }
    let mut hasher = Sha256::new();
    let mut remaining = FINGERPRINT_EDGE_BYTES.min(len as usize) as u64;
    let mut buf = [0u8; 8192];
    while remaining > 0 {
        let want = remaining.min(buf.len() as u64) as usize;
        let read = file
            .read(&mut buf[..want])
            .map_err(|e| AppError::io(path, e))?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
        remaining -= read as u64;
    }
    Ok(hex_lower(&hasher.finalize()))
}

pub(crate) fn fingerprint(path: &Path) -> Result<SourceFingerprint, AppError> {
    let meta = fs::metadata(path).map_err(|e| AppError::io(path, e))?;
    let len = meta.len();
    let mtime_ms = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .and_then(|d| i64::try_from(d.as_millis()).ok())
        .unwrap_or(0);
    let head = edge_hash(path, len, false)?;
    let tail = edge_hash(path, len, true)?;
    Ok(SourceFingerprint {
        mtime_ms,
        len,
        head_sha256: head,
        tail_sha256: tail,
    })
}

/// Stable per-file source key.
///
/// Windows `canonicalize` returns a `\\?\` verbatim path; keeping that verbatim
/// prefix would make a key unresolvable-again (a deleted or renamed source has
/// no canonical form, so the fallback path would never match the stored key and
/// every deletion would look like a brand-new source). The prefix is therefore
/// normalized away so one physical file keeps one key across
/// present/absent states.
pub(crate) fn canonical_key(path: &Path) -> String {
    let resolved = match path.canonicalize() {
        Ok(canonical) => canonical,
        // A deleted source has no canonical form. Canonicalising its parent
        // keeps the key spelled the same way as when the file existed (Windows
        // may report one directory as `RUNNER~1` and as `runneradmin`), so a
        // removal is still recognised as the same source.
        Err(_) => match (path.parent(), path.file_name()) {
            (Some(parent), Some(name)) => parent
                .canonicalize()
                .map(|canonical| canonical.join(name))
                .unwrap_or_else(|_| path.to_path_buf()),
            _ => path.to_path_buf(),
        },
    };
    normalize_key(&resolved)
}

fn normalize_key(path: &Path) -> String {
    let raw = path.to_string_lossy().to_string();
    if let Some(rest) = raw.strip_prefix(r"\\?\UNC\") {
        return format!(r"\\{rest}");
    }
    if let Some(rest) = raw.strip_prefix(r"\\?\") {
        return rest.to_string();
    }
    raw
}

/// Capture one source into the archive (idempotent) with default options.
pub fn capture_source(
    archive: &Archive,
    source: &Path,
    provider: &str,
    domain: &str,
    failpoint: Option<Failpoint>,
) -> Result<CaptureReport, ArchiveError> {
    capture_source_with_options(
        archive,
        source,
        provider,
        domain,
        failpoint,
        CaptureOptions::default(),
    )
}

/// Capture one source into the archive (idempotent) with explicit options.
///
/// Plain JSONL sources (Claude Code / Codex rollouts) take the P2 incremental
/// path — byte-offset watermark, complete-record gate, append/rewrite
/// classification. Everything else (compressed rollouts, whole-file sources)
/// takes the full-capture path below. SQLite sources take the chunked
/// whitelist-baseline + watermark-delta path with `options`.
pub fn capture_source_with_options(
    archive: &Archive,
    source: &Path,
    provider: &str,
    domain: &str,
    failpoint: Option<Failpoint>,
    options: CaptureOptions,
) -> Result<CaptureReport, ArchiveError> {
    if !SUPPORTED_PROVIDERS.contains(&provider) {
        return Err(ArchiveError::Corrupt(format!(
            "unsupported provider {provider:?} (supported: {SUPPORTED_PROVIDERS:?})"
        )));
    }
    let _lock = archive.acquire_lock()?;
    let source_key = canonical_key(source);
    let mut report = CaptureReport::new(provider, source_key.clone());

    let mut state = archive.load_sources()?;
    let had_watermark = state
        .sources
        .get(&source_key)
        .and_then(|s| s.watermark.as_ref())
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
            // A deleted source observed nothing this run: do not replay the
            // previous run's anomalies as if they were current.
            entry.anomalies.clear();
            // P3: a pending SQLite health failure is historical once the source
            // itself is gone (it stays counted in `errorsTotal`), otherwise a
            // single past lock timeout would keep `coverage.ok=false` forever
            // while the capture report says `ok=true` for `source_missing`.
            if let Some(sqlite) = entry.sqlite.as_mut() {
                sqlite.last_unavailable = None;
                sqlite.wal_bytes = 0;
            }
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
        return Ok(report);
    }

    let kind = classify(provider, source);
    if matches!(kind, SourceKind::JsonlFile) {
        return crate::capture_jsonl::capture_jsonl_file(
            archive, source, &source_key, provider, domain, failpoint, state, report,
        );
    }
    if matches!(kind, SourceKind::Sqlite) {
        // SQLite evidence is periodic observation, not file fingerprinting:
        // route before the whole-file fingerprint short-circuit so WAL writes
        // that leave the main file's length/mtime untouched are still observed.
        return crate::capture_sqlite::capture_sqlite_source(
            archive, source, &source_key, provider, domain, failpoint, options, state, report,
        );
    }

    let fp = fingerprint(source)?;
    // Cheap-signal short-circuit: identical fingerprint => skip without reading.
    let prev_watermark = state
        .sources
        .get(&source_key)
        .and_then(|s| s.watermark.clone());
    if let Some(prev) = prev_watermark {
        if prev.fingerprint == fp {
            let discovered = state
                .sources
                .get(&source_key)
                .map(|s| s.discovered)
                .unwrap_or(0);
            let entry = state.sources.get_mut(&source_key).expect("entry exists");
            entry.status = "skipped_unchanged".to_string();
            entry.change = "skipped_unchanged".to_string();
            entry.observed_len = Some(prev.len);
            entry.last_error = None;
            archive.save_sources(&state)?;
            report.ok = true;
            report.status = "skipped_unchanged".to_string();
            report.change = "skipped_unchanged".to_string();
            report.sha256 = Some(prev.sha256.clone());
            report.watermark_len = Some(prev.len);
            report.bytes = prev.len;
            report.records_discovered = discovered;
            report.offset = prev.offset;
            report.line = prev.line;
            report.generation = prev.generation;
            report.file_len = prev.len;
            return Ok(report);
        }
    }

    report.source_kind = Some(kind.as_str().to_string());

    let raw = fs::read(source).map_err(|e| AppError::io(source, e))?;
    report.bytes = raw.len() as u64;
    let raw_sha = hex_sha256(&raw);
    report.sha256 = Some(raw_sha.clone());

    // (1) objects first. Whole-file objects keep their domain encoding: an
    // already-compressed rollout (`.jsonl.zst`) is stored verbatim, anything
    // else is zstd-compressed when the encoding is worth it (S2.5).
    let mut pending: Vec<String> = Vec::new();
    let whole_file_hint = match kind {
        SourceKind::CompressedJsonlFile => StorageHint::Raw,
        _ => StorageHint::Compressible,
    };
    let (obj_sha, _, is_new, _) = archive.put_object_encoded(&raw, whole_file_hint)?;
    if is_new {
        report.objects_new += 1;
    } else {
        report.objects_deduped += 1;
    }
    pending.push(obj_sha.clone());

    let mut objects = vec![ManifestObject {
        sha256: obj_sha.clone(),
        bytes: raw.len() as u64,
        role: "whole_file".to_string(),
        access: vec![domain.to_string()],
        source_offset: Some(0),
        generation: 1,
    }];

    // Decode compressed rollouts into a second object so record references stay
    // byte-exact without every reader re-implementing the compression framing.
    let record_bytes: Vec<u8> = match kind {
        SourceKind::CompressedJsonlFile => {
            let decoded = zstd::stream::decode_all(raw.as_slice())
                .map_err(|e| AppError::io(source, e))?;
            let (dsha, _, dnew, _) = archive.put_object_encoded(&decoded, StorageHint::Compressible)?;
            if dnew {
                report.objects_new += 1;
            } else {
                report.objects_deduped += 1;
            }
            pending.push(dsha.clone());
            objects.push(ManifestObject {
                sha256: dsha.clone(),
                bytes: decoded.len() as u64,
                role: "decoded".to_string(),
                access: vec![domain.to_string()],
                source_offset: Some(0),
                generation: 1,
            });
            decoded
        }
        _ => raw.clone(),
    };
    let record_object = if kind == SourceKind::CompressedJsonlFile {
        objects[1].sha256.clone()
    } else {
        obj_sha.clone()
    };

    // (1b) JSONL records with exact source spans (unknown fields preserved as-is).
    let mut record_entries: Vec<ManifestRecord> = Vec::new();
    if matches!(kind, SourceKind::JsonlFile | SourceKind::CompressedJsonlFile) {
        let mut lines = LineSpans::new(std::io::Cursor::new(record_bytes.as_slice()));
        let mut seq = 0u64;
        while let Some(line) = lines
            .next_line()
            .map_err(|e| AppError::io(source, e))?
        {
            let bytes = line.bytes;
            if bytes.iter().all(|b| b.is_ascii_whitespace()) {
                continue;
            }
            // Records hash to exactly their raw line bytes (line.span.len also
            // counts the trailing newline, which is not part of the record).
            let content_len = bytes.len();
            let sha = hex_sha256(bytes);
            let rec = ManifestRecord {
                seq,
                offset: line.span.offset,
                len: content_len as u64,
                sha256: sha.clone(),
                object: record_object.clone(),
                access: vec![domain.to_string()],
                source_offset: Some(line.span.offset),
                source_line: Some(seq + 1),
            };
            if archive.records_contains(&sha)? {
                report.records_deduped += 1;
            } else {
                report.records_new += 1;
            }
            record_entries.push(rec);
            seq += 1;
        }
    }
    report.records_discovered = record_entries.len() as u64;

    // Parse probe: the unified model is a derived view; failures are health
    // state, not capture blockers (raw evidence comes first).
    report.parse.attempted = true;
    {
        // §A: whole-file sources re-parse the whole file for the probe.
        let _probe = cost::phase("parse_probe");
        cost::counter("parse_probe.full_reads", 1);
        cost::counter("parse_probe.bytes", raw.len() as u64);
        match crate::load_messages(provider, &source_key) {
            Ok(messages) => report.parse.messages = Some(messages.len() as u64),
            Err(e) => report.parse.error = Some(e),
        }
    }

    // (2) manifest commit point. A crash before this leaves orphans + old watermark.
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
            kind: kind.as_str().to_string(),
            path: source_key.clone(),
            compressed: kind == SourceKind::CompressedJsonlFile,
            fingerprint: fp.clone(),
            source_state_at_capture: "present".to_string(),
            capture_reason: "initial".to_string(),
            generation: 1,
            range_start: 0,
            carrier: None,
            sqlite: None,
        },
        objects,
        records: record_entries.clone(),
        attachments: Vec::new(),
    };
    archive.commit_manifest(&manifest)?;

    // (3) record index + watermark advance (only after the commit point).
    // The ledger is staged in memory and written by `flush_records` (once per
    // capture-all round; immediately for a single-source capture).
    for rec in &record_entries {
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

    let entry = state.sources.entry(source_key.clone()).or_insert_with(|| SourceState {
        provider: provider.to_string(),
        ..SourceState::default()
    });
    entry.provider = provider.to_string();
    entry.status = "captured".to_string();
    entry.discovered = record_entries.len() as u64;
    entry.captured = entry.captured.saturating_add(report.records_new);
    entry.skipped = entry.skipped.saturating_add(report.records_deduped);
    entry.bytes = entry.bytes.saturating_add(report.bytes);
    entry.change = "initial".to_string();
    entry.generation = 1;
    entry.incomplete_tail_bytes = 0;
    entry.observed_len = Some(raw.len() as u64);
    entry.anomalies.clear();
    entry.watermark = Some(Watermark {
        len: raw.len() as u64,
        sha256: raw_sha,
        fingerprint: fp,
        capture_id: manifest.capture_id.clone(),
        captured_at_ms: manifest.captured_at_ms,
        offset: raw.len() as u64,
        line: record_entries.len() as u64,
        records: record_entries.len() as u64,
        generation: 1,
        head_probe: String::new(),
        anchor_probe: String::new(),
        probe_len: 0,
    });
    entry.last_error = None;
    entry.last_parse_error = report.parse.error.clone();
    archive.save_sources(&state)?;

    report.capture_id = Some(manifest.capture_id.clone());
    report.watermark_len = Some(raw.len() as u64);
    report.ok = true;
    report.status = "captured".to_string();
    report.change = "initial".to_string();
    report.generation = 1;
    report.offset = raw.len() as u64;
    report.line = record_entries.len() as u64;
    report.file_len = raw.len() as u64;
    // Post-commit reconcile from the same writer view: nothing pending anymore.
    // One object-directory walk per round: inside a capture-all round the
    // reconcile is deferred to the round end (and reported there).
    if let Some(reconcile) = archive.reconcile_after_unit()? {
        report.orphan_objects = reconcile.orphan_objects;
    }
    Ok(report)
}
