//! Periodic integrity scrub (P5).
//!
//! The scrub re-reads every object referenced by a committed manifest, recomputes
//! its SHA-256 and compares it with the content address. Corruption and missing
//! objects are **recorded in `state/scrub.json`** (the corruption ledger) and
//! projected into the coverage report: they are never silently skipped. Orphan
//! objects (on disk, no committed reference) stay "hanging" as P1 defined; a
//! normal in-flight object is explicitly listed as staging residue, not as
//! corruption.
//!
//! # Consistent view
//!
//! The scan runs under the runtime-root **single-writer lock** (the same lock a
//! capture takes). That is the consistent view: no manifest can be committed
//! while the scan enumerates objects, so a newly staged object cannot be
//! mistaken for corruption. When a writer holds the lock the default behaviour is
//! to refuse ([`ArchiveError::LockHeld`]); `skip_if_locked` is the explicit
//! alternative and produces a report with `status = skipped_writer_active`
//! instead of a clean verdict. The two options are never conflated.
//!
//! # What a scrub does *not* prove
//!
//! It verifies byte integrity against the manifest, not authenticity: anyone who
//! can write the runtime root can rewrite both an object and its manifest. It
//! does not repair anything and never deletes evidence.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;

use serde::{Deserialize, Serialize};

use crate::archive::{now_ms, Archive, ArchiveError};

pub const SCRUB_LEDGER_SCHEMA: &str = "agent-memory/scrub@1";
pub const SCRUB_STATUS_CLEAN: &str = "clean";
pub const SCRUB_STATUS_CORRUPTED: &str = "corrupted";
pub const SCRUB_STATUS_SKIPPED_WRITER: &str = "skipped_writer_active";

/// One corrupt or missing object, with every committed reference that names it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ScrubCorruption {
    pub sha256: String,
    /// `corrupted` (bytes hash differently) or `missing` (no object on disk).
    pub status: String,
    /// Capture ids of the manifests referencing this object.
    pub references: Vec<String>,
    /// Physical source paths of those manifests.
    pub sources: Vec<String>,
    pub error: String,
    /// First and most recent detection (ledger bookkeeping).
    #[serde(default)]
    pub first_detected_at_ms: i64,
    #[serde(default)]
    pub last_detected_at_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ScrubRunSummary {
    pub status: String,
    pub started_at_ms: i64,
    pub finished_at_ms: i64,
    pub objects_checked: u64,
    pub bytes_checked: u64,
    pub corrupted: u64,
    pub missing: u64,
    pub orphan_objects: u64,
    pub staging_skipped: u64,
    /// Which view the scan used: `exclusive_lock` or `skipped_writer_active`.
    pub writer: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ScrubLedger {
    pub schema: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_run: Option<ScrubRunSummary>,
    /// Current corruption findings (an entry disappears once the object verifies
    /// again). This is the damage bookkeeping, not a lifetime log.
    #[serde(default)]
    pub corruption: Vec<ScrubCorruption>,
}

/// Full machine-readable scrub report (CLI contract).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScrubReport {
    pub ok: bool,
    pub status: String,
    pub writer: String,
    pub started_at_ms: i64,
    pub finished_at_ms: i64,
    pub objects_checked: u64,
    pub bytes_checked: u64,
    pub corrupted: Vec<ScrubCorruption>,
    pub orphan_objects: Vec<String>,
    pub staging_skipped: Vec<String>,
    pub ledger_path: String,
    pub anomalies: Vec<String>,
}

/// Coverage-facing projection of the last scrub (never a second authority).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScrubCoverage {
    pub status: String,
    pub checked_at_ms: i64,
    pub objects_checked: u64,
    pub bytes_checked: u64,
    pub corrupted: u64,
    pub missing: u64,
    pub orphan_objects: u64,
    pub staging_skipped: u64,
    pub anomalies: Vec<String>,
}

impl ScrubLedger {
    pub fn summary(&self) -> ScrubCoverage {
        let Some(run) = self.last_run.as_ref() else {
            return ScrubCoverage {
                status: "never_run".to_string(),
                checked_at_ms: 0,
                objects_checked: 0,
                bytes_checked: 0,
                corrupted: 0,
                missing: 0,
                orphan_objects: 0,
                staging_skipped: 0,
                anomalies: Vec::new(),
            };
        };
        let mut anomalies = Vec::new();
        for entry in &self.corruption {
            anomalies.push(format!(
                "object {} is {}: {}",
                entry.sha256, entry.status, entry.error
            ));
        }
        ScrubCoverage {
            status: if self.corruption.is_empty() {
                SCRUB_STATUS_CLEAN.to_string()
            } else {
                SCRUB_STATUS_CORRUPTED.to_string()
            },
            checked_at_ms: run.finished_at_ms,
            objects_checked: run.objects_checked,
            bytes_checked: run.bytes_checked,
            corrupted: self
                .corruption
                .iter()
                .filter(|entry| entry.status == "corrupted")
                .count() as u64,
            missing: self
                .corruption
                .iter()
                .filter(|entry| entry.status == "missing")
                .count() as u64,
            orphan_objects: run.orphan_objects,
            staging_skipped: run.staging_skipped,
            anomalies,
        }
    }
}

fn ledger_path(archive: &Archive) -> std::path::PathBuf {
    archive.root().join("state").join("scrub.json")
}

/// Read the corruption ledger. `Ok(None)` means no scrub has run yet. A corrupt
/// ledger is an error, never an empty "everything is fine" answer.
pub fn read_ledger(archive: &Archive) -> Result<Option<ScrubLedger>, ArchiveError> {
    let path = ledger_path(archive);
    let raw = match fs::read(&path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(ArchiveError::Io(crate::error::AppError::io(&path, error))),
    };
    let mut ledger: ScrubLedger = serde_json::from_slice(&raw)
        .map_err(|error| ArchiveError::Corrupt(format!("state/scrub.json: {error}")))?;
    if ledger.schema.is_empty() {
        ledger.schema = SCRUB_LEDGER_SCHEMA.to_string();
    }
    if ledger.schema != SCRUB_LEDGER_SCHEMA {
        return Err(ArchiveError::Corrupt(format!(
            "unsupported scrub ledger schema {}",
            ledger.schema
        )));
    }
    Ok(Some(ledger))
}

/// Object sha -> (capture ids, source paths) over every committed manifest.
type ObjectReferences = BTreeMap<String, (BTreeSet<String>, BTreeSet<String>)>;

fn references(archive: &Archive) -> Result<ObjectReferences, ArchiveError> {
    let mut out: ObjectReferences = BTreeMap::new();
    for manifest in archive.manifests()?.iter() {
        let mut shas: Vec<String> = manifest.objects.iter().map(|o| o.sha256.clone()).collect();
        // A record locator names its object explicitly; include it even if a
        // legacy manifest's `objects[]` were incomplete.
        shas.extend(manifest.records.iter().map(|record| record.object.clone()));
        // P4 attachment mappings use the same object index; include the explicit
        // content address even if the manifest object list were ever incomplete.
        for attachment in &manifest.attachments {
            if let Some(sha) = attachment.object_sha256.as_ref() {
                shas.push(sha.clone());
            }
        }
        for sha in shas {
            let slot = out.entry(sha).or_default();
            slot.0.insert(manifest.capture_id.clone());
            slot.1.insert(manifest.source.path.clone());
        }
    }
    Ok(out)
}

fn list_staging(archive: &Archive) -> Result<Vec<String>, ArchiveError> {
    let dir = archive.root().join("archive").join("staging");
    let mut out = Vec::new();
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(out),
        Err(error) => return Err(ArchiveError::Io(crate::error::AppError::io(&dir, error))),
    };
    for entry in entries {
        let entry = entry.map_err(|error| ArchiveError::Io(crate::error::AppError::io(&dir, error)))?;
        out.push(entry.file_name().to_string_lossy().to_string());
    }
    out.sort();
    Ok(out)
}

/// Recompute every referenced object's hash and persist the corruption ledger.
///
/// Default: acquiring the single-writer lock is mandatory (refuse when held).
/// `skip_if_locked = true`: return an explicit skipped report instead.
pub fn scrub(archive: &Archive, skip_if_locked: bool) -> Result<ScrubReport, ArchiveError> {
    let started_at_ms = now_ms();
    if archive.lock_held() && skip_if_locked {
        return Ok(ScrubReport {
            ok: true,
            status: SCRUB_STATUS_SKIPPED_WRITER.to_string(),
            writer: "skipped_writer_active".to_string(),
            started_at_ms,
            finished_at_ms: now_ms(),
            objects_checked: 0,
            bytes_checked: 0,
            corrupted: Vec::new(),
            orphan_objects: Vec::new(),
            staging_skipped: Vec::new(),
            ledger_path: ledger_path(archive).to_string_lossy().to_string(),
            anomalies: vec![
                "a writer holds the capture lock; no consistent view available, \
                 integrity was not verified (explicit skip, not a clean result)"
                    .to_string(),
            ],
        });
    }
    // Consistent view: the same exclusive lock a capture takes. A concurrent
    // writer makes this fail rather than reporting a mixed-state scan.
    let _lock = archive.acquire_lock()?;

    let refs = references(archive)?;
    let mut corrupted: Vec<ScrubCorruption> = Vec::new();
    let mut objects_checked = 0u64;
    let mut bytes_checked = 0u64;
    for (sha, (capture_ids, sources)) in &refs {
        if !archive.has_object(sha) {
            corrupted.push(ScrubCorruption {
                sha256: sha.clone(),
                status: "missing".to_string(),
                references: capture_ids.iter().cloned().collect(),
                sources: sources.iter().cloned().collect(),
                error: "no object at its content address".to_string(),
                first_detected_at_ms: 0,
                last_detected_at_ms: 0,
            });
            continue;
        }
        match archive.verify_object_stream(sha) {
            Ok(bytes) => {
                objects_checked = objects_checked.saturating_add(1);
                bytes_checked = bytes_checked.saturating_add(bytes);
            }
            Err(error) => corrupted.push(ScrubCorruption {
                sha256: sha.clone(),
                status: "corrupted".to_string(),
                references: capture_ids.iter().cloned().collect(),
                sources: sources.iter().cloned().collect(),
                error: error.to_string(),
                first_detected_at_ms: 0,
                last_detected_at_ms: 0,
            }),
        }
    }

    let reconcile = archive.reconcile(&[])?;
    let staging_skipped = list_staging(archive)?;

    // Merge with the previous ledger so first-detection time survives.
    let previous = read_ledger(archive)?;
    let mut first_seen: BTreeMap<String, i64> = BTreeMap::new();
    if let Some(ledger) = previous.as_ref() {
        for entry in &ledger.corruption {
            first_seen.insert(entry.sha256.clone(), entry.first_detected_at_ms);
        }
    }
    for entry in &mut corrupted {
        entry.first_detected_at_ms = first_seen.get(&entry.sha256).copied().unwrap_or(started_at_ms);
        entry.last_detected_at_ms = now_ms();
    }

    let finished_at_ms = now_ms();
    let status = if corrupted.is_empty() {
        SCRUB_STATUS_CLEAN.to_string()
    } else {
        SCRUB_STATUS_CORRUPTED.to_string()
    };
    let mut anomalies = Vec::new();
    for entry in &corrupted {
        anomalies.push(format!(
            "object {} is {} (references: {}): {}",
            entry.sha256,
            entry.status,
            entry.references.join(","),
            entry.error
        ));
    }
    let ledger = ScrubLedger {
        schema: SCRUB_LEDGER_SCHEMA.to_string(),
        last_run: Some(ScrubRunSummary {
            status: status.clone(),
            started_at_ms,
            finished_at_ms,
            objects_checked,
            bytes_checked,
            corrupted: corrupted
                .iter()
                .filter(|entry| entry.status == "corrupted")
                .count() as u64,
            missing: corrupted
                .iter()
                .filter(|entry| entry.status == "missing")
                .count() as u64,
            orphan_objects: reconcile.orphan_objects.len() as u64,
            staging_skipped: staging_skipped.len() as u64,
            writer: "exclusive_lock".to_string(),
        }),
        corruption: corrupted.clone(),
    };
    let path = ledger_path(archive);
    let data = serde_json::to_string_pretty(&ledger)
        .map_err(|error| ArchiveError::Corrupt(error.to_string()))?;
    crate::config::atomic_write(&path, data.as_bytes())?;

    Ok(ScrubReport {
        ok: corrupted.is_empty(),
        status,
        writer: "exclusive_lock".to_string(),
        started_at_ms,
        finished_at_ms,
        objects_checked,
        bytes_checked,
        corrupted,
        orphan_objects: reconcile.orphan_objects,
        staging_skipped,
        ledger_path: path.to_string_lossy().to_string(),
        anomalies,
    })
}
