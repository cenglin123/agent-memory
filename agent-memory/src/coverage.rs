//! Per-source coverage projection (P2).
//!
//! Every physical source file gets a coverage row: how many records were
//! discovered in it, how many were captured, how many were already captured,
//! how many bytes were committed, how many error conditions were observed, and
//! which anomalies (discontinuity reasons, malformed records) were seen. The row
//! is a projection of `state/sources.json` (health) plus the committed manifests
//! (capture history); it maintains no separate truth.
//!
//! # Quantity scale (deliberately named per quantity)
//!
//! These counters answer different questions and are **not** three views of one
//! number, so the field names carry their scale:
//!
//! - `recordsCommitted` — records in the current committed prefix. The only
//!   scale-consistent denominator (it equals the archive's current extent for
//!   that source).
//! - `discoveredLatest` — `recordsCommitted` plus the records this source's most
//!   recent run saw, i.e. coverage of the last run.
//! - `capturedTotal` / `skippedTotal` — lifetime event counters. A record that
//!   was already in the committed prefix of an append run is counted in
//!   `skippedTotal` even though it was never re-read, and a forced recapture
//!   re-counts records it re-emits, so both can exceed `discoveredLatest`.
//! - `errorsTotal` — lifetime error conditions; `anomalies` is the **last run's**
//!   anomaly list (never a lifetime log).
//!
//! Scope note: this file owns the P2 per-source coverage surface, extended in P3
//! with the SQLite block and in P4 with the attachment block. P5
//! (`src/report.rs`, `src/replay.rs`, `src/scrub.rs`) adds provider aggregation,
//! source categories, observation windows, framework-baseline reconciliation,
//! derived checkpoint status and corruption bookkeeping on top of this
//! projection; it does not maintain a second authority.

use serde::Serialize;

use std::collections::BTreeMap;

use crate::archive::{
    Archive, ArchiveError, AttachmentCounts, CaptureSummary, SourceState, ATTACHMENT_SOURCE_KIND,
    ATTACHMENT_STATUS_ARCHIVED,
};
use crate::report::{
    CategoryTotals, DerivedReport, ObservationWindow, ProviderReport, ReconciliationReport,
    ReportConfig, CATEGORY_ATTACHMENT_MISSING, CATEGORY_PARSE_FAILED, CATEGORY_READ_FAILED,
};
use crate::scrub::ScrubCoverage;

#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct CoverageTotals {
    pub sources: u64,
    /// Records in the committed prefixes (scale-consistent denominator).
    pub records_committed: u64,
    pub discovered_latest: u64,
    pub captured_total: u64,
    pub skipped_total: u64,
    pub errors_total: u64,
    pub bytes_total: u64,
    pub anomalies: u64,
    pub incomplete_tail_bytes: u64,
    /// P3: SQLite evidence units across all sources.
    pub sqlite_baselines: u64,
    pub sqlite_deltas: u64,
    pub sqlite_duplicate_payload_rows: u64,
    /// P4: attachment reference outcomes across all sources (lifetime, derived
    /// from the committed manifests).
    pub attachments: AttachmentCounts,
    /// P4: bytes of archived attachment objects across all sources.
    pub attachment_bytes_total: u64,
}

/// P4: attachment coverage for one physical source.
///
/// `total` is derived from the committed manifests (the single authority for
/// capture facts). `last_run` / `anomalies` come from collector state and answer
/// only "how did the most recent run go" — an old missing attachment stays in
/// `total.missing` forever but does not keep current health red.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AttachmentCoverage {
    /// `archived` | `partial` | `failed` | `none`
    pub status: String,
    pub last_run_at_ms: i64,
    pub last_run: AttachmentCounts,
    pub total: AttachmentCounts,
    pub bytes_stored: u64,
    pub objects_new: u64,
    pub objects_deduped: u64,
    pub limit_bytes: u64,
    pub anomalies: Vec<String>,
}

/// P3: SQLite observation / generation / delta-chain coverage for one source.
///
/// `observation_seq` is the evidence watermark (the last committed observation
/// point); `observation_interval_ms` is the gap to the previous observation and
/// therefore the width of the blind window during which changes could revert
/// unseen. Nothing here claims continuous capture.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SqliteCoverage {
    pub observation_seq: u64,
    pub baseline_generation: u64,
    pub baseline_capture_seq: u64,
    /// Digest over the current baseline's ordered chunk list.
    pub baseline_digest: String,
    /// Total uncompressed baseline export bytes; the delta chain re-baselines
    /// when its raw bytes exceed 50% of this.
    pub baseline_bytes: u64,
    pub schema_fingerprint: String,
    pub known_tables: Vec<String>,
    /// Session time filter in force (`None` = unfiltered).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_since_ms: Option<i64>,
    /// Non-whitelisted tables: row counts only (row content is never read).
    pub excluded_tables: Vec<crate::capture_sqlite::SqliteExcludedTable>,
    /// With a session filter in force: whitelist child tables without a
    /// `session_id` column, captured in full (N3).
    pub unfiltered_tables: Vec<String>,
    /// When the last full row-hash verification ran.
    pub last_verify_at_ms: i64,
    pub verify_mismatches_total: u64,
    pub baselines_total: u64,
    pub deltas_total: u64,
    pub delta_chain_length: u64,
    pub delta_chain_bytes: u64,
    pub rows_discovered_latest: u64,
    pub rows_inserted_total: u64,
    pub rows_updated_total: u64,
    pub rows_deleted_total: u64,
    pub duplicate_payload_rows: u64,
    pub last_observed_at_ms: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_observed_at_ms: Option<i64>,
    /// Gap between the last two observations: the observation blind window.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observation_interval_ms: Option<i64>,
    pub wal_bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_unavailable: Option<String>,
    /// Why the last baseline was taken.
    pub last_snapshot_reason: String,
    /// Explicit statement of what this row does *not* prove.
    pub blind_window: String,
}

#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SourceCoverage {
    pub source: String,
    pub provider: String,
    /// Current health: captured | skipped_unchanged | source_missing | failed |
    /// degraded | disabled | not_captured
    pub status: String,
    /// Last change classification (initial|append|skipped_unchanged|
    /// forced_recapture|degraded|source_missing|failed).
    pub change: String,
    pub generation: u64,
    /// Committed byte offset (end of the last complete record).
    pub offset: u64,
    /// Complete physical lines committed.
    pub line: u64,
    /// Records in the committed prefix.
    pub records_committed: u64,
    /// Source length observed by the most recent run.
    pub observed_len: u64,
    /// Source length recorded by the most recent *commit*.
    pub watermark_len: u64,
    /// Bytes after the last complete record held back for the next run.
    pub incomplete_tail_bytes: u64,
    /// `recordsCommitted` + records seen by the most recent run.
    pub discovered_latest: u64,
    pub captured_total: u64,
    pub skipped_total: u64,
    pub errors_total: u64,
    pub bytes_total: u64,
    /// Last run's anomalies (malformed records, discontinuity reasons,
    /// degradation). Empty means the last run observed none.
    pub anomalies: Vec<String>,
    /// Capture history, oldest first, projected from committed manifests.
    pub captures: Vec<CaptureSummary>,
    /// P3: SQLite evidence state (observation watermark, snapshot generations,
    /// delta chain). `None` for non-SQLite sources.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sqlite: Option<SqliteCoverage>,
    /// P4: attachment coverage. `None` when this source has no attachment
    /// references and no attachment run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attachments: Option<AttachmentCoverage>,
    // -- P5 (additive) ------------------------------------------------------
    /// True when a declared source in `config.local.json` claimed this row.
    #[serde(default)]
    pub configured: bool,
    /// Primary P5 category (see `report::CATEGORY_*`).
    #[serde(default)]
    pub category: String,
    /// Every category that applies (a source can be both e.g. read-failed and
    /// attachment-missing). The coverage report never collapses this to one bit.
    #[serde(default)]
    pub categories: Vec<String>,
    /// Observation watermark and blind window for this physical source.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observation: Option<ObservationWindow>,
    /// True when a scrub ledger entry names this source as having a corrupt or
    /// missing object.
    #[serde(default)]
    pub corruption: bool,
}

#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct CoverageReport {
    /// False when any source is degraded/failed or its last run reported an
    /// anomaly. Lifetime counters do not by themselves decide current health.
    pub ok: bool,
    pub sources: Vec<SourceCoverage>,
    pub totals: CoverageTotals,
    /// Aggregate anomalies, prefixed with the source they belong to.
    pub anomalies: Vec<String>,
    // -- P5 (additive) ------------------------------------------------------
    #[serde(default)]
    pub categories: CategoryTotals,
    #[serde(default)]
    pub providers: Vec<ProviderReport>,
    #[serde(default)]
    pub reconciliation: ReconciliationReport,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scrub: Option<ScrubCoverage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub derived: Option<DerivedReport>,
    /// Parser generation this report classified `derivedLag` against.
    #[serde(default)]
    pub derived_identity: Option<crate::replay::ReplayIdentity>,
    /// S2.5: per-encoding object storage statistics (disk projection).
    #[serde(default)]
    pub storage: crate::archive::StorageStats,
}

impl Archive {
    /// Build the per-source coverage report from state + committed manifests.
    ///
    /// P4 asset sources join the union: a source that only has committed
    /// attachment manifests (state lost or never captured) still gets a row, so
    /// attachment evidence is never invisible just because `sources.json` is
    /// gone.
    pub fn coverage(&self) -> Result<CoverageReport, ArchiveError> {
        let mut report = self.coverage_base()?;
        crate::report::evaluate(self, &ReportConfig::default(), &mut report)?;
        Ok(report)
    }

    /// P5: build the coverage report with declared sources and framework
    /// baselines from an explicit `config.local.json` model.
    pub fn coverage_with(&self, config: &ReportConfig) -> Result<CoverageReport, ArchiveError> {
        let mut report = self.coverage_base()?;
        crate::report::evaluate(self, config, &mut report)?;
        Ok(report)
    }

    fn coverage_base(&self) -> Result<CoverageReport, ArchiveError> {
        let state = self.load_sources()?;
        let manifests = self.manifests()?;

        // Attachment lifetime counters, derived from the committed manifests.
        let mut attachment_by_source: BTreeMap<String, (String, AttachmentCounts, u64, u64)> =
            BTreeMap::new();
        for manifest in manifests.iter() {
            if manifest.source.kind != ATTACHMENT_SOURCE_KIND {
                continue;
            }
            let slot = attachment_by_source
                .entry(manifest.source.path.clone())
                .or_insert_with(|| {
                    (
                        manifest.provider.clone(),
                        AttachmentCounts::default(),
                        0u64,
                        0u64,
                    )
                });
            for attachment in &manifest.attachments {
                slot.1.record(&attachment.status);
                if attachment.status == ATTACHMENT_STATUS_ARCHIVED {
                    if attachment.deduplicated {
                        slot.1.deduplicated = slot.1.deduplicated.saturating_add(1);
                        slot.3 = slot.3.saturating_add(1);
                    } else {
                        // Dedupe-aware storage bytes: a content-addressed hit
                        // adds no new bytes.
                        slot.2 = slot.2.saturating_add(attachment.bytes);
                    }
                }
            }
        }

        let mut sources = Vec::new();
        let mut totals = CoverageTotals {
            sources: 0,
            records_committed: 0,
            discovered_latest: 0,
            captured_total: 0,
            skipped_total: 0,
            errors_total: 0,
            bytes_total: 0,
            anomalies: 0,
            incomplete_tail_bytes: 0,
            sqlite_baselines: 0,
            sqlite_deltas: 0,
            sqlite_duplicate_payload_rows: 0,
            attachments: AttachmentCounts::default(),
            attachment_bytes_total: 0,
        };
        let mut anomalies = Vec::new();
        let mut ok = true;

        let mut keys: std::collections::BTreeSet<String> =
            state.sources.keys().cloned().collect();
        keys.extend(attachment_by_source.keys().cloned());

        for path in keys {
            let entry = state.sources.get(&path).cloned().unwrap_or_default();
            let captures = self.source_captures(&path)?;
            let watermark = entry.watermark.as_ref();
            let watermark_len = watermark.map(|w| w.len).unwrap_or(0);
            let observed_len = entry
                .observed_len
                // State written before `observedLen` existed.
                .unwrap_or(watermark_len);
            totals.sources += 1;
            totals.records_committed += watermark.map(|w| w.records).unwrap_or(0);
            totals.discovered_latest += entry.discovered;
            totals.captured_total += entry.captured;
            totals.skipped_total += entry.skipped;
            totals.errors_total += entry.errors;
            totals.bytes_total += entry.bytes;
            totals.anomalies += entry.anomalies.len() as u64;
            totals.incomplete_tail_bytes += entry.incomplete_tail_bytes;
            for anomaly in &entry.anomalies {
                anomalies.push(format!("{path}: {anomaly}"));
            }
            if entry.status == "degraded" || entry.status == "failed" {
                ok = false;
                anomalies.push(format!("{path}: status={}", entry.status));
            }
            if !entry.anomalies.is_empty() {
                // Current-view anomaly (last run), not a lifetime counter.
                ok = false;
            }

            let mut categories: Vec<String> = Vec::new();
            if entry.status == "degraded" || entry.status == "failed" {
                categories.push(CATEGORY_READ_FAILED.to_string());
            }
            if entry
                .last_parse_error
                .as_deref()
                .map(|error| !error.is_empty())
                .unwrap_or(false)
            {
                categories.push(CATEGORY_PARSE_FAILED.to_string());
                ok = false;
                anomalies.push(format!(
                    "{path}: parse failed: {}",
                    entry.last_parse_error.clone().unwrap_or_default()
                ));
            }

            let mut provider = entry.provider.clone();
            let mut status = entry.status.clone();
            let mut attachments = None;
            if let Some((manifest_provider, total, bytes, deduplicated)) =
                attachment_by_source.get(&path)
            {
                if provider.is_empty() {
                    provider = manifest_provider.clone();
                }
                if status.is_empty() {
                    status = "attachment_only".to_string();
                }
                let last = match entry.attachment.clone() {
                    Some(state) => state,
                    None => {
                        // A committed manifest always precedes the state write:
                        // manifests without collector state mean the state was
                        // lost (or the run crashed in that window). The mapping
                        // is intact, but current health is unknowable.
                        ok = false;
                        anomalies.push(format!(
                            "{path}: attachment manifests without collector state \
                             (last-run health unknown)"
                        ));
                        crate::archive::AttachmentState::default()
                    }
                };
                totals.attachments.add(total);
                totals.attachment_bytes_total = totals
                    .attachment_bytes_total
                    .saturating_add(*bytes);
                if last.last_run.rejected() > 0 || !last.anomalies.is_empty() {
                    // Current-view health: only the most recent attachment run.
                    ok = false;
                }
                totals.anomalies += last.anomalies.len() as u64;
                for anomaly in &last.anomalies {
                    anomalies.push(format!("{path}: {anomaly}"));
                }
                attachments = Some(AttachmentCoverage {
                    status: attachment_health(&last),
                    last_run_at_ms: last.last_run_at_ms,
                    last_run: last.last_run.clone(),
                    total: total.clone(),
                    bytes_stored: *bytes,
                    objects_new: total.archived.saturating_sub(*deduplicated),
                    objects_deduped: *deduplicated,
                    limit_bytes: last.last_limit_bytes,
                    anomalies: last.anomalies.clone(),
                });
            } else if let Some(last) = entry.attachment.as_ref() {
                // A committed manifest always precedes the state write: state
                // without evidence means the manifests were lost. Report it.
                ok = false;
                anomalies.push(format!(
                    "{path}: attachment state without committed attachment manifests \
                     (evidence missing)"
                ));
                attachments = Some(AttachmentCoverage {
                    status: attachment_health(last),
                    last_run_at_ms: last.last_run_at_ms,
                    last_run: last.last_run.clone(),
                    total: AttachmentCounts::default(),
                    bytes_stored: 0,
                    objects_new: 0,
                    objects_deduped: 0,
                    limit_bytes: last.last_limit_bytes,
                    anomalies: last.anomalies.clone(),
                });
            }
            if attachments
                .as_ref()
                .map(|coverage| {
                    coverage.total.missing > 0 || coverage.total.source_missing > 0
                })
                .unwrap_or(false)
            {
                categories.push(CATEGORY_ATTACHMENT_MISSING.to_string());
            }

            let sqlite = entry.sqlite.as_ref().map(|sqlite| {
                totals.sqlite_baselines += sqlite.baselines_total;
                totals.sqlite_deltas += sqlite.deltas_total;
                totals.sqlite_duplicate_payload_rows += sqlite.duplicate_payload_rows;
                if let Some(unavailable) = &sqlite.last_unavailable {
                    // A health failure is a current-view anomaly even when the
                    // last run has since recovered: it stays reportable until a
                    // successful observation clears it.
                    ok = false;
                    categories.push(CATEGORY_READ_FAILED.to_string());
                    anomalies.push(format!("{path}: sqlite {unavailable}"));
                }
                SqliteCoverage {
                    observation_seq: sqlite.capture_seq,
                    baseline_generation: sqlite.baseline_generation,
                    baseline_capture_seq: sqlite.baseline_capture_seq,
                    baseline_digest: sqlite.baseline_digest.clone(),
                    baseline_bytes: sqlite.baseline_bytes,
                    schema_fingerprint: sqlite.schema_fingerprint.clone(),
                    known_tables: sqlite.known_tables.clone(),
                    session_since_ms: sqlite.session_since_ms,
                    excluded_tables: sqlite
                        .excluded_tables
                        .iter()
                        .map(|(table, row_count)| {
                            crate::capture_sqlite::SqliteExcludedTable {
                                table: table.clone(),
                                row_count: *row_count,
                            }
                        })
                        .collect(),
                    unfiltered_tables: sqlite.unfiltered_tables.clone(),
                    last_verify_at_ms: sqlite.last_verify_at_ms,
                    verify_mismatches_total: sqlite.verify_mismatches_total,
                    baselines_total: sqlite.baselines_total,
                    deltas_total: sqlite.deltas_total,
                    delta_chain_length: sqlite.delta_chain_length,
                    delta_chain_bytes: sqlite.delta_chain_bytes,
                    rows_discovered_latest: sqlite.rows_discovered,
                    rows_inserted_total: sqlite.rows_inserted_total,
                    rows_updated_total: sqlite.rows_updated_total,
                    rows_deleted_total: sqlite.rows_deleted_total,
                    duplicate_payload_rows: sqlite.duplicate_payload_rows,
                    last_observed_at_ms: sqlite.last_observed_at_ms,
                    previous_observed_at_ms: sqlite.previous_observed_at_ms,
                    observation_interval_ms: sqlite.observation_interval_ms,
                    wal_bytes: sqlite.wal_bytes,
                    last_unavailable: sqlite.last_unavailable.clone(),
                    last_snapshot_reason: sqlite.last_snapshot_reason.clone(),
                    blind_window: "watermark observation, not continuous capture: row content \
                                   that changes while its watermark column (time_updated/seq) \
                                   does not, writes at or below the watermark, and changes that \
                                   revert entirely between two observations are invisible to \
                                   any read-only mechanism until the periodic verification pass \
                                   re-hashes every whitelisted row"
                        .to_string(),
                }
            });
            let observation = if watermark.is_some() || entry.sqlite.is_some() || !captures.is_empty()
            {
                Some(observation_window(&entry, &captures))
            } else {
                None
            };
            sources.push(SourceCoverage {
                source: path.clone(),
                provider,
                status,
                change: entry.change.clone(),
                generation: watermark
                    .map(|w| w.generation)
                    .unwrap_or(entry.generation),
                offset: watermark.map(|w| w.offset).unwrap_or(0),
                line: watermark.map(|w| w.line).unwrap_or(0),
                records_committed: watermark.map(|w| w.records).unwrap_or(0),
                observed_len,
                watermark_len,
                incomplete_tail_bytes: entry.incomplete_tail_bytes,
                discovered_latest: entry.discovered,
                captured_total: entry.captured,
                skipped_total: entry.skipped,
                errors_total: entry.errors,
                bytes_total: entry.bytes,
                anomalies: entry.anomalies.clone(),
                captures,
                sqlite,
                attachments,
                configured: false,
                category: String::new(),
                categories,
                observation,
                corruption: false,
            });
        }
        Ok(CoverageReport {
            ok,
            sources,
            totals,
            anomalies,
            categories: CategoryTotals::default(),
            providers: Vec::new(),
            reconciliation: ReconciliationReport::default(),
            scrub: None,
            derived: None,
            derived_identity: Some(crate::replay::ReplayIdentity::current()),
            storage: self.storage_stats()?,
        })
    }
}

/// P4: last-run attachment health label (`none` when no run has happened).
fn attachment_health(state: &crate::archive::AttachmentState) -> String {
    if state.last_status.is_empty() {
        "none".to_string()
    } else {
        state.last_status.clone()
    }
}

/// P5: observation watermark + explicit blind window for one physical source.
fn observation_window(entry: &SourceState, captures: &[CaptureSummary]) -> ObservationWindow {
    if let Some(sqlite) = entry.sqlite.as_ref() {
        return ObservationWindow {
            kind: "sqlite_observation".to_string(),
            watermark_at_ms: sqlite.last_observed_at_ms,
            previous_at_ms: sqlite.previous_observed_at_ms,
            interval_ms: sqlite.observation_interval_ms,
            records: sqlite.rows_discovered,
            offset: 0,
            line: 0,
            observation_seq: sqlite.capture_seq,
            blind_window: "a watermark observation, not continuous capture: content changes \
                           invisible to the watermark column and changes that revert entirely \
                           between two observations are caught only by the verification pass"
                .to_string(),
        };
    }
    let watermark_at_ms = captures
        .last()
        .map(|capture| capture.captured_at_ms)
        .unwrap_or(0);
    let previous_at_ms = captures
        .get(captures.len().saturating_sub(2))
        .map(|capture| capture.captured_at_ms)
        .filter(|_| captures.len() >= 2);
    let interval_ms = previous_at_ms.map(|previous| (watermark_at_ms - previous).max(0));
    ObservationWindow {
        kind: "records".to_string(),
        watermark_at_ms,
        previous_at_ms,
        interval_ms,
        records: entry.watermark.as_ref().map(|w| w.records).unwrap_or(0),
        offset: entry.watermark.as_ref().map(|w| w.offset).unwrap_or(0),
        line: entry.watermark.as_ref().map(|w| w.line).unwrap_or(0),
        observation_seq: 0,
        blind_window: "file-backed source: only committed records are covered; writes that \
                       happen (or are rewritten) between two capture runs are not observed"
            .to_string(),
    }
}
