//! `agent-memory compact` (stage-2 S2.5): migrate plain-encoded archive
//! objects to the zstd storage encoding, in place.
//!
//! Content addresses never change: an object id stays the sha256 of the
//! *decoded* bytes, manifests and the record index are not touched. For every
//! plain object the run streams the file through a level-3 zstd encoder into a
//! staging file and keeps the compressed encoding only when it is worth it
//! (strictly smaller than [`ZSTD_WORTH_PERCENT`] percent of the raw size —
//! already-compressed content such as `.jsonl.zst` whole files or SQLite zstd
//! chunk/delta objects therefore stays plain automatically). The per-object
//! write order is: compress → staging file → decode-readback and sha256 verify
//! → atomic rename into `<sha>.zst` → remove the plain file.
//!
//! Crash window: between the rename and the removal both encodings of one sha
//! exist. Reads prefer the `.zst` file (it was verified before its atomic
//! rename), and the next `compact` run verifies it again, removes the plain
//! leftover and thereby converges the store to a single encoding per sha.
//!
//! `--dry-run` estimates the migration without writing anything: compression
//! goes to an in-memory buffer, no staging file is created, no object is
//! renamed or removed.

use std::io::{Read, Write};

use serde::Serialize;

use crate::archive::{
    now_ms, worth_compressing, Archive, ArchiveError, ObjectEncoding, ZSTD_COMPRESSION_LEVEL,
};

/// Compression candidates larger than this are skipped in the in-memory
/// `--dry-run` (whose buffer holds the *compressed* bytes; the cap bounds the
/// estimate's memory). Real runs stream to staging files and have no cap.
const DRY_RUN_MAX_CANDIDATE_BYTES: u64 = 2 * 1024 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CompactReport {
    pub ok: bool,
    pub root: String,
    pub dry_run: bool,
    pub finished_at_ms: i64,
    /// Object files seen (both encodings, including crash-window leftovers).
    pub objects_scanned: u64,
    /// Plain objects evaluated as migration candidates.
    pub candidates: u64,
    /// Plain objects migrated to zstd (projected in `--dry-run`).
    pub compressed: u64,
    /// Plain objects kept: the encoded form was not worth it.
    pub not_worth: u64,
    /// Objects already stored as zstd (skipped).
    pub already_compressed: u64,
    /// Dual-encoding leftovers converged by verifying the `.zst` file and
    /// removing the plain file (projected in `--dry-run`).
    pub converged: u64,
    /// Candidates skipped by the `--dry-run` memory cap.
    pub too_large: u64,
    /// Objects that failed verification — left untouched, never destructive.
    pub corrupted: u64,
    /// I/O failures during the run.
    pub errors: u64,
    /// Bytes stored before the run (both encodings).
    pub stored_bytes_before: u64,
    /// Bytes stored after the run (projected in `--dry-run`).
    pub stored_bytes_after: u64,
    /// `stored_bytes_before - stored_bytes_after`.
    pub saved_bytes: u64,
    /// First error/corruption messages (bounded sample; never object content).
    pub error_samples: Vec<String>,
}

/// Migrate plain objects of one runtime root to the zstd storage encoding.
/// Takes the runtime root's single-writer (capture) lock: no capture, scrub or
/// second `compact` can interleave. Idempotent — a settled root reports
/// `compressed = 0`.
pub fn compact(archive: &Archive, dry_run: bool) -> Result<CompactReport, ArchiveError> {
    // The same exclusive lock a capture takes: compact writes objects, so a
    // concurrent capture would be exactly the "two writers" case the lock
    // exists to prevent. (`--dry-run` also locks: it reports against a
    // consistent view, and a capture running mid-estimate would make the
    // projection lie.)
    let _lock = archive.acquire_lock()?;

    let mut report = CompactReport {
        ok: true,
        root: archive.root().to_string_lossy().to_string(),
        dry_run,
        finished_at_ms: 0,
        objects_scanned: 0,
        candidates: 0,
        compressed: 0,
        not_worth: 0,
        already_compressed: 0,
        converged: 0,
        too_large: 0,
        corrupted: 0,
        errors: 0,
        stored_bytes_before: 0,
        stored_bytes_after: 0,
        saved_bytes: 0,
        error_samples: Vec::new(),
    };

    for entry in archive.object_files()? {
        report.objects_scanned = report.objects_scanned.saturating_add(1);
        report.stored_bytes_before = report.stored_bytes_before.saturating_add(entry.stored_bytes);
        match entry.encoding {
            ObjectEncoding::Zstd => {
                let plain_path = archive.object_path_for(&entry.sha256, ObjectEncoding::Plain);
                if !plain_path.exists() {
                    report.already_compressed = report.already_compressed.saturating_add(1);
                    report.stored_bytes_after =
                        report.stored_bytes_after.saturating_add(entry.stored_bytes);
                    continue;
                }
                // Crash-window leftover: verify the zstd side, then drop the
                // plain file. The zstd file was verified before its atomic
                // rename, so a failure here means external corruption — the
                // plain file is kept as the fallback and nothing is removed.
                match archive.verify_object_stream(&entry.sha256) {
                    Ok(_) => {
                        let plain_len = std::fs::metadata(&plain_path)
                            .map(|m| m.len())
                            .unwrap_or(0);
                        if dry_run {
                            report.converged = report.converged.saturating_add(1);
                            report.saved_bytes = report.saved_bytes.saturating_add(plain_len);
                            report.stored_bytes_after =
                                report.stored_bytes_after.saturating_add(entry.stored_bytes);
                        } else {
                            match std::fs::remove_file(&plain_path) {
                                Ok(()) => {
                                    report.converged = report.converged.saturating_add(1);
                                    report.saved_bytes =
                                        report.saved_bytes.saturating_add(plain_len);
                                    report.stored_bytes_after = report
                                        .stored_bytes_after
                                        .saturating_add(entry.stored_bytes);
                                }
                                Err(e) => {
                                    report.errors = report.errors.saturating_add(1);
                                    push_sample(
                                        &mut report,
                                        format!("{}: {e}", plain_path.display()),
                                    );
                                    report.stored_bytes_after = report
                                        .stored_bytes_after
                                        .saturating_add(entry.stored_bytes + plain_len);
                                }
                            }
                        }
                    }
                    Err(e) => {
                        report.corrupted = report.corrupted.saturating_add(1);
                        push_sample(
                            &mut report,
                            format!(
                                "dual-encoded object {} failed verification: {e}",
                                entry.sha256
                            ),
                        );
                        let plain_len = std::fs::metadata(&plain_path)
                            .map(|m| m.len())
                            .unwrap_or(0);
                        report.stored_bytes_after = report
                            .stored_bytes_after
                            .saturating_add(entry.stored_bytes + plain_len);
                    }
                }
            }
            ObjectEncoding::Plain => {
                if archive.object_encoding(&entry.sha256) == Some(ObjectEncoding::Zstd) {
                    // The dual case was handled on the zstd side of the loop.
                    continue;
                }
                report.candidates = report.candidates.saturating_add(1);
                let outcome = if dry_run {
                    estimate_compression(&entry.path, entry.stored_bytes)
                } else {
                    migrate_object(archive, &entry.sha256, &entry.path, entry.stored_bytes)
                };
                match outcome {
                    Ok(Outcome::Compressed { compressed_bytes }) => {
                        report.compressed = report.compressed.saturating_add(1);
                        let saved = entry.stored_bytes.saturating_sub(compressed_bytes);
                        report.saved_bytes = report.saved_bytes.saturating_add(saved);
                        report.stored_bytes_after =
                            report.stored_bytes_after.saturating_add(compressed_bytes);
                    }
                    Ok(Outcome::NotWorth) => {
                        report.not_worth = report.not_worth.saturating_add(1);
                        report.stored_bytes_after =
                            report.stored_bytes_after.saturating_add(entry.stored_bytes);
                    }
                    Ok(Outcome::TooLarge) => {
                        report.too_large = report.too_large.saturating_add(1);
                        report.stored_bytes_after =
                            report.stored_bytes_after.saturating_add(entry.stored_bytes);
                    }
                    Err(message) => {
                        report.corrupted = report.corrupted.saturating_add(1);
                        push_sample(&mut report, message);
                        report.stored_bytes_after =
                            report.stored_bytes_after.saturating_add(entry.stored_bytes);
                    }
                }
            }
        }
    }

    report.finished_at_ms = now_ms();
    report.ok = report.errors == 0 && report.corrupted == 0;
    Ok(report)
}

enum Outcome {
    Compressed { compressed_bytes: u64 },
    NotWorth,
    TooLarge,
}

/// In-memory estimate: compress into a bounded buffer, never touch the disk.
fn estimate_compression(path: &std::path::Path, stored_bytes: u64) -> Result<Outcome, String> {
    if stored_bytes > DRY_RUN_MAX_CANDIDATE_BYTES {
        return Ok(Outcome::TooLarge);
    }
    let mut raw = Vec::new();
    std::fs::File::open(path)
        .and_then(|mut file| file.read_to_end(&mut raw))
        .map_err(|e| format!("{}: {e}", path.display()))?;
    let compressed = zstd::stream::encode_all(raw.as_slice(), ZSTD_COMPRESSION_LEVEL)
        .map_err(|e| format!("{}: compression failed: {e}", path.display()))?;
    if worth_compressing(raw.len() as u64, compressed.len() as u64) {
        Ok(Outcome::Compressed {
            compressed_bytes: compressed.len() as u64,
        })
    } else {
        Ok(Outcome::NotWorth)
    }
}

/// Migrate one plain object: compress → staging file → decode-readback verify
/// → atomic rename to `<sha>.zst` → remove the plain file. The plain file is
/// only removed after the compressed encoding is durably in place, so a crash
/// leaves at most the (readable) dual-encoding state described in the module
/// docs.
fn migrate_object(
    archive: &Archive,
    sha256: &str,
    path: &std::path::Path,
    raw_bytes: u64,
) -> Result<Outcome, String> {
    let staging = archive.staging_path();
    let compress_result = (|| -> Result<u64, String> {
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&staging)
            .map_err(|e| format!("{}: {e}", staging.display()))?;
        let mut encoder = zstd::stream::write::Encoder::new(file, ZSTD_COMPRESSION_LEVEL)
            .map_err(|e| format!("compression failed: {e}"))?;
        let source = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let mut reader = std::io::BufReader::new(source);
        let mut buffer = vec![0u8; 1024 * 1024];
        loop {
            let read = reader
                .read(&mut buffer)
                .map_err(|e| format!("{}: {e}", path.display()))?;
            if read == 0 {
                break;
            }
            encoder
                .write_all(&buffer[..read])
                .map_err(|e| format!("{}: {e}", staging.display()))?;
        }
        encoder
            .finish()
            .map_err(|e| format!("compression failed: {e}"))
            .and_then(|file| {
                file.sync_all()
                    .map_err(|e| format!("{}: {e}", staging.display()))
            })?;
        std::fs::metadata(&staging)
            .map(|m| m.len())
            .map_err(|e| format!("{}: {e}", staging.display()))
    })();
    let compressed_bytes = match compress_result {
        Ok(bytes) => bytes,
        Err(message) => {
            let _ = std::fs::remove_file(&staging);
            return Err(message);
        }
    };
    if !worth_compressing(raw_bytes, compressed_bytes) {
        let _ = std::fs::remove_file(&staging);
        return Ok(Outcome::NotWorth);
    }
    // Verify the staged frame decodes back to the exact content address.
    if let Err(e) =
        crate::archive::verify_staged_zstd(&staging, sha256, raw_bytes)
    {
        let _ = std::fs::remove_file(&staging);
        return Err(format!("object {sha256}: staged encoding failed verification: {e}"));
    }
    match archive.place_object_file(staging, sha256, ObjectEncoding::Zstd) {
        Ok(true) => {}
        Ok(false) => {
            // A previous/parallel compact already placed this object.
            return Ok(Outcome::Compressed { compressed_bytes });
        }
        Err(e) => return Err(format!("object {sha256}: {e}")),
    }
    // The `.zst` file is in place and verified; now drop the plain original.
    if let Err(e) = std::fs::remove_file(path) {
        return Err(format!("{}: {e}", path.display()));
    }
    Ok(Outcome::Compressed { compressed_bytes })
}

fn push_sample(report: &mut CompactReport, message: String) {
    if report.error_samples.len() < 10 {
        report.error_samples.push(message);
    }
}
