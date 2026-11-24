//! SQLite single-writer store for candidate drafts.
//!
//! One `learning.db` per explicit learning root. Every ingest runs inside one
//! `BEGIN IMMEDIATE` transaction: conflict checks, revision/evidence/ingestion
//! and status-history writes commit together or not at all.
//!
//! # Schema version
//!
//! `learning.db` carries its own `PRAGMA user_version` ([`DB_SCHEMA_VERSION`]),
//! which is independent of the candidate *payload* schema
//! ([`super::model::SCHEMA_VERSION`], still v1). `init` creates v5 directly;
//! `open` accepts v1 through v5, migrating a v1 database through v2, v3 and v4
//! and a v2/v3/v4 database forward in one transaction per step before probing
//! it (see [`LearningStore::open`]).
//!
//! v4 adds the single global event clock (`event_clock`), the `task_events`
//! stream and an `event_seq` column on every event table. Any event write
//! allocates its `event_seq` from `event_clock` inside the caller's
//! `BEGIN IMMEDIATE` transaction, so event order is comparable across tables
//! (the per-table `seq` columns remain primary keys only).
//!
//! v5 adds the append-only `qualification_history` and `rejection_overrides`
//! tables ([`super::qualify`]). Before the v4 -> v5 migration the store is
//! snapshotted to `learning.db.bak-v4-<UTC timestamp>`; v5 has **no downgrade
//! path**, so restoring that backup is the only rollback.
//!
//! The forward migration is automatic and fail-closed; the only explicit
//! downgrade path is [`LearningStore::downgrade_to_v3`] (`learning migrate --to
//! 3`), which accepts a v4 store, snapshots it into `learning.db.bak` first and
//! never runs automatically.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use chrono::DateTime;
use rusqlite::{
    params, Connection, ErrorCode, OpenFlags, OptionalExtension, Transaction, TransactionBehavior,
};
use serde::Serialize;
use serde_json::Value;

use crate::archive::hex_sha256;

use super::model::{
    evidence_sha256, forbidden_codepoint_class, validate_candidate, CandidateRecord,
    CANDIDATE_STATUS, TASK_TARGET_KIND,
};
use super::LearningError;

/// The live database file of a learning root. A root holds only this file (plus
/// SQLite's WAL sidecars while open) and, after an explicit downgrade, the
/// declared `learning.db.bak` + `learning.db.bak.meta.json` pair.
pub const DB_FILE: &str = "learning.db";
/// The fixed downgrade snapshot (`learning migrate --to 3`). Declared as the
/// one allowed companion of `learning.db` inside a learning root.
pub const DOWNGRADE_BACKUP_FILE: &str = "learning.db.bak";
/// Sidecar metadata of [`DOWNGRADE_BACKUP_FILE`]: creation timestamp and the
/// source/backup fingerprints. Written before any row is downgraded.
pub const DOWNGRADE_SIDECAR_FILE: &str = "learning.db.bak.meta.json";
/// Stage 6 §D: `learning migrate --to 3` is refused because a stored candidate
/// (current row or a historical revision snapshot) carries the `origin_tasks`
/// key. Conflict class; the refusal happens before the backup pair is written,
/// so the database and the root directory stay byte-identical.
pub const DOWNGRADE_BLOCKED_ORIGIN_TASKS: &str = "DOWNGRADE_BLOCKED_ORIGIN_TASKS";
/// Prefix of the automatic pre-v5 snapshot: `learning.db.bak-v4-<UTC timestamp>`.
pub const UPGRADE_BACKUP_PREFIX: &str = "learning.db.bak-v4-";
/// Storage schema version (`PRAGMA user_version`) of `learning.db`.
///
/// v1 = S1 (four tables), v2 = S3 (`dedup_history` added), v3 = S5
/// (`reuse_history` and `validation_history` added), v4 = stage 4 (`event_clock`,
/// `task_events`, global `event_seq` on every event table), v5 = stage 7
/// (`qualification_history` and `rejection_overrides`). This is *not* the
/// candidate payload schema, which stays [`super::model::SCHEMA_VERSION`] = 1.
pub const DB_SCHEMA_VERSION: u32 = 5;
/// The v1 -> v2 migration target (S3).
const DB_SCHEMA_V2: i64 = 2;
/// The v2 -> v3 migration target (S5).
const DB_SCHEMA_V3: i64 = 3;
/// The v3 -> v4 migration target (stage 4).
const DB_SCHEMA_V4: i64 = 4;
const BUSY_TIMEOUT_SECS: u64 = 15;
/// Busy timeout of the read-only injection connection. Short on purpose: a
/// SessionStart hook must fail fast instead of delaying the session.
const READ_ONLY_BUSY_TIMEOUT_MS: u64 = 2000;
const CONSTRAINT_FALLBACK_CODE: &str = "INGEST_KEY_CONSTRAINT";
/// Actor attributed to every event row that predates actor attribution (v3
/// events carried in by the v3 -> v4 migration). Never a real session id.
///
/// The migration DDL hardcodes the same literal (SQL cannot interpolate a Rust
/// constant); this constant is the canonical spelling referenced by the
/// migration docs. No live write path uses it since v4 requires `--actor`.
#[allow(dead_code)]
pub(crate) const MIGRATED_V3_ACTOR: &str = "migrated:v3";

const SCHEMA_SQL: &str = "
CREATE TABLE candidates (
    candidate_id TEXT PRIMARY KEY NOT NULL,
    revision INTEGER NOT NULL,
    status TEXT NOT NULL,
    record_json TEXT NOT NULL,
    payload_sha256 TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE TABLE candidate_revisions (
    candidate_id TEXT NOT NULL,
    revision INTEGER NOT NULL,
    status TEXT NOT NULL,
    record_json TEXT NOT NULL,
    payload_sha256 TEXT NOT NULL,
    recorded_at TEXT NOT NULL,
    PRIMARY KEY (candidate_id, revision),
    FOREIGN KEY (candidate_id) REFERENCES candidates(candidate_id)
);

CREATE TABLE ingestions (
    ingest_key TEXT PRIMARY KEY NOT NULL,
    candidate_id TEXT NOT NULL,
    revision INTEGER NOT NULL,
    payload_sha256 TEXT NOT NULL,
    extractor TEXT NOT NULL,
    source_checkpoint TEXT NOT NULL,
    created_at TEXT NOT NULL,
    FOREIGN KEY (candidate_id) REFERENCES candidates(candidate_id)
);

CREATE TABLE status_history (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    candidate_id TEXT NOT NULL,
    revision INTEGER NOT NULL,
    status TEXT NOT NULL,
    recorded_at TEXT NOT NULL,
    FOREIGN KEY (candidate_id) REFERENCES candidates(candidate_id)
);

CREATE INDEX status_history_candidate_idx ON status_history(candidate_id, seq);

CREATE TABLE dedup_history (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    candidate_id TEXT NOT NULL,
    query TEXT NOT NULL,
    matches_json TEXT NOT NULL,
    disposition TEXT NOT NULL,
    reason TEXT NOT NULL,
    checked_at TEXT NOT NULL,
    FOREIGN KEY (candidate_id) REFERENCES candidates(candidate_id)
);

CREATE INDEX dedup_history_candidate_idx ON dedup_history(candidate_id, seq);

CREATE TABLE event_clock (
    seq INTEGER PRIMARY KEY AUTOINCREMENT
);

CREATE TABLE task_events (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    event_seq INTEGER NOT NULL,
    candidate_id TEXT NOT NULL REFERENCES candidates(candidate_id),
    event_kind TEXT NOT NULL CHECK (event_kind IN ('claim', 'abandon')),
    actor_id TEXT NOT NULL,
    note TEXT NOT NULL DEFAULT '',
    recorded_at TEXT NOT NULL
);

CREATE INDEX task_events_candidate_idx ON task_events(candidate_id, seq);

CREATE TABLE reuse_history (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    event_seq INTEGER NOT NULL,
    candidate_id TEXT NOT NULL REFERENCES candidates(candidate_id),
    task_id TEXT NOT NULL,
    outcome TEXT NOT NULL,
    notes TEXT NOT NULL,
    recorded_at TEXT NOT NULL,
    actor_id TEXT NOT NULL DEFAULT 'migrated:v3',
    client_seq INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX reuse_history_candidate_idx ON reuse_history(candidate_id, seq);
CREATE UNIQUE INDEX reuse_history_idem_idx ON reuse_history(task_id, actor_id, client_seq);

CREATE TABLE validation_history (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    event_seq INTEGER NOT NULL,
    candidate_id TEXT NOT NULL REFERENCES candidates(candidate_id),
    reviewer TEXT NOT NULL,
    verdict TEXT NOT NULL,
    report_ref TEXT NOT NULL,
    recorded_at TEXT NOT NULL
);

CREATE INDEX validation_history_candidate_idx ON validation_history(candidate_id, seq);

CREATE TABLE qualification_history (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    event_seq INTEGER NOT NULL,
    candidate_id TEXT NOT NULL REFERENCES candidates(candidate_id),
    revision INTEGER NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('rejected', 'deferred', 'cleared')),
    basis TEXT NOT NULL DEFAULT '',
    counterexample TEXT NOT NULL DEFAULT '',
    counterexample_ref TEXT NOT NULL DEFAULT '',
    owner TEXT NOT NULL DEFAULT '',
    trigger TEXT NOT NULL DEFAULT '',
    actor TEXT NOT NULL,
    recorded_at TEXT NOT NULL
);

CREATE INDEX qualification_history_candidate_idx ON qualification_history(candidate_id, seq);

CREATE TABLE rejection_overrides (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    event_seq INTEGER NOT NULL,
    new_candidate_id TEXT NOT NULL,
    matched_candidate_id TEXT NOT NULL,
    reason TEXT NOT NULL,
    actor TEXT NOT NULL,
    recorded_at TEXT NOT NULL
);

CREATE INDEX rejection_overrides_new_candidate_idx ON rejection_overrides(new_candidate_id, seq);
";

/// v1 -> v2: add the dedup history table and its index. Every statement is
/// idempotent and no existing row is touched, so the migration is re-entrant
/// and never rewrites evidence.
const MIGRATION_V1_TO_V2_SQL: &str = "
CREATE TABLE IF NOT EXISTS dedup_history (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    candidate_id TEXT NOT NULL,
    query TEXT NOT NULL,
    matches_json TEXT NOT NULL,
    disposition TEXT NOT NULL,
    reason TEXT NOT NULL,
    checked_at TEXT NOT NULL,
    FOREIGN KEY (candidate_id) REFERENCES candidates(candidate_id)
);

CREATE INDEX IF NOT EXISTS dedup_history_candidate_idx ON dedup_history(candidate_id, seq);
";

/// v2 -> v3: add the reuse and validation history tables with their indexes.
/// Every statement is idempotent and no existing row is touched, so the
/// migration is re-entrant and never rewrites evidence or dedup history.
const MIGRATION_V2_TO_V3_SQL: &str = "
CREATE TABLE IF NOT EXISTS reuse_history (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    candidate_id TEXT NOT NULL REFERENCES candidates(candidate_id),
    task_id TEXT NOT NULL,
    outcome TEXT NOT NULL,
    notes TEXT NOT NULL,
    recorded_at TEXT NOT NULL,
    UNIQUE (candidate_id, task_id)
);

CREATE INDEX IF NOT EXISTS reuse_history_candidate_idx ON reuse_history(candidate_id, seq);

CREATE TABLE IF NOT EXISTS validation_history (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    candidate_id TEXT NOT NULL REFERENCES candidates(candidate_id),
    reviewer TEXT NOT NULL,
    verdict TEXT NOT NULL,
    report_ref TEXT NOT NULL,
    recorded_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS validation_history_candidate_idx ON validation_history(candidate_id, seq);
";

/// v3 -> v4, part 1: the new tables. Every statement is idempotent and no
/// existing row is touched; the two rebuilt event tables follow in
/// [`MIGRATION_V3_TO_V4_REUSE_REBUILD_SQL`] /
/// [`MIGRATION_V3_TO_V4_VALIDATION_REBUILD_SQL`], which copy (never rewrite)
/// the v3 rows and are entered only when the v3 shape is still present.
const MIGRATION_V3_TO_V4_EVENT_TABLES_SQL: &str = "
CREATE TABLE IF NOT EXISTS event_clock (
    seq INTEGER PRIMARY KEY AUTOINCREMENT
);

CREATE TABLE IF NOT EXISTS task_events (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    event_seq INTEGER NOT NULL,
    candidate_id TEXT NOT NULL REFERENCES candidates(candidate_id),
    event_kind TEXT NOT NULL CHECK (event_kind IN ('claim', 'abandon')),
    actor_id TEXT NOT NULL,
    note TEXT NOT NULL DEFAULT '',
    recorded_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS task_events_candidate_idx ON task_events(candidate_id, seq);
";

/// v3 -> v4: rebuild `reuse_history` into the v4 shape (SQLite cannot drop a
/// table-level UNIQUE in place). The copy keeps every v3 column verbatim and
/// explicit `seq`; `event_seq` starts at the placeholder `0` and is assigned by
/// the ordered backfill in [`backfill_event_sequence`]. `actor_id` and
/// `client_seq` are the v3-migration attribution: one `migrated:v3` actor and
/// the original per-table `seq` (unique table-wide, so the idempotency index
/// cannot collide).
const MIGRATION_V3_TO_V4_REUSE_REBUILD_SQL: &str = "
CREATE TABLE reuse_history_v4 (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    event_seq INTEGER NOT NULL,
    candidate_id TEXT NOT NULL REFERENCES candidates(candidate_id),
    task_id TEXT NOT NULL,
    outcome TEXT NOT NULL,
    notes TEXT NOT NULL,
    recorded_at TEXT NOT NULL,
    actor_id TEXT NOT NULL DEFAULT 'migrated:v3',
    client_seq INTEGER NOT NULL DEFAULT 0
);

INSERT INTO reuse_history_v4
    (seq, event_seq, candidate_id, task_id, outcome, notes, recorded_at, actor_id, client_seq)
SELECT seq, 0, candidate_id, task_id, outcome, notes, recorded_at, 'migrated:v3', seq
FROM reuse_history;

DROP TABLE reuse_history;
ALTER TABLE reuse_history_v4 RENAME TO reuse_history;

CREATE INDEX reuse_history_candidate_idx ON reuse_history(candidate_id, seq);
CREATE UNIQUE INDEX reuse_history_idem_idx ON reuse_history(task_id, actor_id, client_seq);
";

/// v3 -> v4: rebuild `validation_history` with the added `event_seq` column;
/// every other column and constraint is copied verbatim.
const MIGRATION_V3_TO_V4_VALIDATION_REBUILD_SQL: &str = "
CREATE TABLE validation_history_v4 (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    event_seq INTEGER NOT NULL,
    candidate_id TEXT NOT NULL REFERENCES candidates(candidate_id),
    reviewer TEXT NOT NULL,
    verdict TEXT NOT NULL,
    report_ref TEXT NOT NULL,
    recorded_at TEXT NOT NULL
);

INSERT INTO validation_history_v4
    (seq, event_seq, candidate_id, reviewer, verdict, report_ref, recorded_at)
SELECT seq, 0, candidate_id, reviewer, verdict, report_ref, recorded_at
FROM validation_history;

DROP TABLE validation_history;
ALTER TABLE validation_history_v4 RENAME TO validation_history;

CREATE INDEX validation_history_candidate_idx ON validation_history(candidate_id, seq);
";

/// v4 -> v5: add the two append-only qualification tables and their indexes.
/// Every statement is idempotent and no existing row is touched, so the
/// migration is re-entrant and never rewrites evidence or events.
const MIGRATION_V4_TO_V5_SQL: &str = "
CREATE TABLE IF NOT EXISTS qualification_history (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    event_seq INTEGER NOT NULL,
    candidate_id TEXT NOT NULL REFERENCES candidates(candidate_id),
    revision INTEGER NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('rejected', 'deferred', 'cleared')),
    basis TEXT NOT NULL DEFAULT '',
    counterexample TEXT NOT NULL DEFAULT '',
    counterexample_ref TEXT NOT NULL DEFAULT '',
    owner TEXT NOT NULL DEFAULT '',
    trigger TEXT NOT NULL DEFAULT '',
    actor TEXT NOT NULL,
    recorded_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS qualification_history_candidate_idx
    ON qualification_history(candidate_id, seq);

CREATE TABLE IF NOT EXISTS rejection_overrides (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    event_seq INTEGER NOT NULL,
    new_candidate_id TEXT NOT NULL,
    matched_candidate_id TEXT NOT NULL,
    reason TEXT NOT NULL,
    actor TEXT NOT NULL,
    recorded_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS rejection_overrides_new_candidate_idx
    ON rejection_overrides(new_candidate_id, seq);
";

/// v4 -> v3: rebuild `reuse_history` into the exact v3 shape. Rows are copied
/// with their original `seq` (explicit rowids keep the append counter
/// continuous); the table-level `UNIQUE (candidate_id, task_id)` is restored.
const MIGRATION_V4_TO_V3_REUSE_REBUILD_SQL: &str = "
CREATE TABLE reuse_history_v3 (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    candidate_id TEXT NOT NULL REFERENCES candidates(candidate_id),
    task_id TEXT NOT NULL,
    outcome TEXT NOT NULL,
    notes TEXT NOT NULL,
    recorded_at TEXT NOT NULL,
    UNIQUE (candidate_id, task_id)
);

INSERT INTO reuse_history_v3 (seq, candidate_id, task_id, outcome, notes, recorded_at)
SELECT seq, candidate_id, task_id, outcome, notes, recorded_at
FROM reuse_history
ORDER BY seq;

DROP TABLE reuse_history;
ALTER TABLE reuse_history_v3 RENAME TO reuse_history;

CREATE INDEX reuse_history_candidate_idx ON reuse_history(candidate_id, seq);
";

/// v4 -> v3: drop the v4-only `event_seq` column by rebuilding the table into
/// the exact v3 shape (rows and `seq` preserved).
const MIGRATION_V4_TO_V3_VALIDATION_REBUILD_SQL: &str = "
CREATE TABLE validation_history_v3 (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    candidate_id TEXT NOT NULL REFERENCES candidates(candidate_id),
    reviewer TEXT NOT NULL,
    verdict TEXT NOT NULL,
    report_ref TEXT NOT NULL,
    recorded_at TEXT NOT NULL
);

INSERT INTO validation_history_v3
    (seq, candidate_id, reviewer, verdict, report_ref, recorded_at)
SELECT seq, candidate_id, reviewer, verdict, report_ref, recorded_at
FROM validation_history
ORDER BY seq;

DROP TABLE validation_history;
ALTER TABLE validation_history_v3 RENAME TO validation_history;

CREATE INDEX validation_history_candidate_idx ON validation_history(candidate_id, seq);
";

/// Column probes for a v1 learning database. They run *before* the v1 -> v2
/// migration so a foreign or partial v1 database is rejected without being
/// written to.
const PROBES_V1: [&str; 4] = [
    "SELECT candidate_id, revision, status, record_json, payload_sha256, created_at, updated_at FROM candidates LIMIT 0",
    "SELECT candidate_id, revision, status, record_json, payload_sha256, recorded_at FROM candidate_revisions LIMIT 0",
    "SELECT ingest_key, candidate_id, revision, payload_sha256, extractor, source_checkpoint, created_at FROM ingestions LIMIT 0",
    "SELECT candidate_id, revision, status, recorded_at FROM status_history LIMIT 0",
];

/// Column probes that a v2 learning database must satisfy.
const PROBES_V2: [&str; 5] = [
    "SELECT candidate_id, revision, status, record_json, payload_sha256, created_at, updated_at FROM candidates LIMIT 0",
    "SELECT candidate_id, revision, status, record_json, payload_sha256, recorded_at FROM candidate_revisions LIMIT 0",
    "SELECT ingest_key, candidate_id, revision, payload_sha256, extractor, source_checkpoint, created_at FROM ingestions LIMIT 0",
    "SELECT candidate_id, revision, status, recorded_at FROM status_history LIMIT 0",
    "SELECT seq, candidate_id, query, matches_json, disposition, reason, checked_at FROM dedup_history LIMIT 0",
];

/// Column probes that a v3 learning database must satisfy: the five S1/S3
/// tables plus the two S5 event tables.
const PROBES_V3: [&str; 7] = [
    "SELECT candidate_id, revision, status, record_json, payload_sha256, created_at, updated_at FROM candidates LIMIT 0",
    "SELECT candidate_id, revision, status, record_json, payload_sha256, recorded_at FROM candidate_revisions LIMIT 0",
    "SELECT ingest_key, candidate_id, revision, payload_sha256, extractor, source_checkpoint, created_at FROM ingestions LIMIT 0",
    "SELECT candidate_id, revision, status, recorded_at FROM status_history LIMIT 0",
    "SELECT seq, candidate_id, query, matches_json, disposition, reason, checked_at FROM dedup_history LIMIT 0",
    "SELECT seq, candidate_id, task_id, outcome, notes, recorded_at FROM reuse_history LIMIT 0",
    "SELECT seq, candidate_id, reviewer, verdict, report_ref, recorded_at FROM validation_history LIMIT 0",
];

/// The eight column probes a v4 learning database must satisfy before it is
/// written to: the five S1/S3 tables, the global clock, and the two v4 event
/// shapes (`reuse_history` with actor/idempotency columns, `task_events`).
///
/// `validation_history.event_seq` is checked by [`PROBE_V4_VALIDATION`] next to
/// this list: it is the same table with one added column, so keeping it out of
/// the eight keeps the list exactly the set the stage-4 spec names.
const PROBES_V4: [&str; 8] = [
    "SELECT candidate_id, revision, status, record_json, payload_sha256, created_at, updated_at FROM candidates LIMIT 0",
    "SELECT candidate_id, revision, status, record_json, payload_sha256, recorded_at FROM candidate_revisions LIMIT 0",
    "SELECT ingest_key, candidate_id, revision, payload_sha256, extractor, source_checkpoint, created_at FROM ingestions LIMIT 0",
    "SELECT candidate_id, revision, status, recorded_at FROM status_history LIMIT 0",
    "SELECT seq, candidate_id, query, matches_json, disposition, reason, checked_at FROM dedup_history LIMIT 0",
    "SELECT seq FROM event_clock LIMIT 0",
    "SELECT seq, event_seq, candidate_id, task_id, outcome, notes, recorded_at, actor_id, client_seq FROM reuse_history LIMIT 0",
    "SELECT seq, event_seq, candidate_id, event_kind, actor_id, note, recorded_at FROM task_events LIMIT 0",
];

/// The v4 shape of the rebuilt validation stream (v3 columns plus `event_seq`).
const PROBE_V4_VALIDATION: &str =
    "SELECT seq, event_seq, candidate_id, reviewer, verdict, report_ref, recorded_at FROM validation_history LIMIT 0";

/// The ten column probes a v5 learning database must satisfy: the eight v4
/// probes plus the two stage-7 qualification tables. `validation_history` is
/// checked by [`PROBE_V4_VALIDATION`] (unchanged in v5).
const PROBES_V5: [&str; 10] = [
    "SELECT candidate_id, revision, status, record_json, payload_sha256, created_at, updated_at FROM candidates LIMIT 0",
    "SELECT candidate_id, revision, status, record_json, payload_sha256, recorded_at FROM candidate_revisions LIMIT 0",
    "SELECT ingest_key, candidate_id, revision, payload_sha256, extractor, source_checkpoint, created_at FROM ingestions LIMIT 0",
    "SELECT candidate_id, revision, status, recorded_at FROM status_history LIMIT 0",
    "SELECT seq, candidate_id, query, matches_json, disposition, reason, checked_at FROM dedup_history LIMIT 0",
    "SELECT seq FROM event_clock LIMIT 0",
    "SELECT seq, event_seq, candidate_id, task_id, outcome, notes, recorded_at, actor_id, client_seq FROM reuse_history LIMIT 0",
    "SELECT seq, event_seq, candidate_id, event_kind, actor_id, note, recorded_at FROM task_events LIMIT 0",
    "SELECT seq, event_seq, candidate_id, revision, state, basis, counterexample, counterexample_ref, owner, trigger, actor, recorded_at FROM qualification_history LIMIT 0",
    "SELECT seq, event_seq, new_candidate_id, matched_candidate_id, reason, actor, recorded_at FROM rejection_overrides LIMIT 0",
];

/// Result of one ingest. Field names are the CLI contract.
#[derive(Debug, Clone, Serialize)]
pub struct IngestOutcome {
    pub ok: bool,
    pub candidate_id: String,
    pub revision: u64,
    pub status: String,
    pub ingest_key: String,
    pub payload_sha256: String,
    pub replayed: bool,
    pub evidence_added: usize,
    /// Stage 7 §A (c): claims that are *near* a rejected claim (character
    /// 3-gram Jaccard `>= 0.6`). Present only when the list is non-empty.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub rejected_matches: Vec<super::qualify::RejectedMatch>,
    /// Stage 7 §A: the explicit override that let this ingest past a rejected
    /// claim match. Present only when an override was recorded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub r#override: Option<OverrideOutcome>,
}

/// The explicit `--override-rejected` record carried by an ingest result.
#[derive(Debug, Clone, Serialize)]
pub struct OverrideOutcome {
    pub reason: String,
    pub actor: String,
    /// The first matched candidate id (see `matched_candidate_ids` for the
    /// complete set when several rejected claims normalize identically).
    pub matched_candidate_id: String,
    pub matched_candidate_ids: Vec<String>,
    pub recorded_at: String,
    pub seq: i64,
}

/// Per-ingest stage-7 §A control values (not part of the v1 payload).
struct IngestControl<'a> {
    override_reason: Option<&'a str>,
    override_actor: Option<&'a str>,
}

impl<'a> IngestControl<'a> {
    fn none() -> Self {
        Self {
            override_reason: None,
            override_actor: None,
        }
    }
}

/// One storage-generated ingestion record.
#[derive(Debug, Clone, Serialize)]
pub struct StoredIngestion {
    pub ingest_key: String,
    pub candidate_id: String,
    pub revision: u64,
    pub payload_sha256: String,
    pub extractor: String,
    pub source_checkpoint: String,
    pub created_at: String,
}

/// Audit view of one stored revision snapshot.
#[derive(Debug, Clone, Serialize)]
pub struct RevisionInfo {
    pub revision: u64,
    pub status: String,
    pub payload_sha256: String,
    pub recorded_at: String,
}

/// One `status_history` row. S1 only ever writes `candidate`.
#[derive(Debug, Clone, Serialize)]
pub struct StatusEntry {
    pub revision: u64,
    pub status: String,
    pub recorded_at: String,
}

/// `learning show` payload.
#[derive(Debug, Clone, Serialize)]
pub struct CandidateView {
    pub ok: bool,
    pub candidate: CandidateRecord,
    pub payload_sha256: String,
    pub ingestions: Vec<StoredIngestion>,
    pub revisions: Vec<RevisionInfo>,
    pub status_history: Vec<StatusEntry>,
    /// Stage 7 §A: the newest qualification, its full history and every
    /// anti-rebirth override recorded for this candidate.
    pub qualification: super::qualify::QualificationView,
}

/// One row of `learning list`; a summary only, never a recall entry.
#[derive(Debug, Clone, Serialize)]
pub struct CandidateSummary {
    pub candidate_id: String,
    pub revision: u64,
    pub status: String,
    pub claim: String,
    pub target_kind: String,
    pub created_at: String,
    pub updated_at: String,
}

/// `learning list` payload.
#[derive(Debug, Clone, Serialize)]
pub struct ListReport {
    pub ok: bool,
    pub count: usize,
    pub candidates: Vec<CandidateSummary>,
}

/// Rows removed by `learning migrate --to 3`. `orphan_task_refs` counts
/// knowledge-candidate `reuse_history` rows whose `task_id` pointed at a
/// deleted task candidate (they cannot survive without their task).
#[derive(Debug, Clone, Serialize)]
pub struct DowngradeDropped {
    pub candidates: usize,
    pub candidate_revisions: usize,
    pub ingestions: usize,
    pub status_history: usize,
    pub dedup_history: usize,
    pub task_events: usize,
    pub reuse: usize,
    pub validation: usize,
    pub orphan_task_refs: usize,
}

/// Rows kept by `learning migrate --to 3`. `event_seq_max` is the highest
/// global event sequence that was discarded with the clock; the v3 store has no
/// cross-table order, so it is reported for audit only.
#[derive(Debug, Clone, Serialize)]
pub struct DowngradeKept {
    pub candidates: usize,
    pub reuse: usize,
    pub validation: usize,
    pub event_seq_max: i64,
}

/// `learning migrate --to 3` payload. Field names are the CLI contract.
#[derive(Debug, Clone, Serialize)]
pub struct DowngradeReport {
    pub ok: bool,
    pub from_version: i64,
    pub to_version: i64,
    pub backup: String,
    pub sidecar: String,
    pub dropped: DowngradeDropped,
    pub kept: DowngradeKept,
}

/// An open learning store. Holds one connection; callers do not share it
/// across threads (SQLite serializes writers, one connection per writer).
#[derive(Debug)]
pub struct LearningStore {
    root: PathBuf,
    db_path: PathBuf,
    conn: Connection,
}

/// Reject anything that is not an explicit absolute path. No default root, no
/// environment variable, no `~` expansion, no real-home inference.
pub fn resolve_root(root: &Path) -> Result<PathBuf, LearningError> {
    let text = root.to_string_lossy();
    if text.trim().is_empty() {
        return Err(LearningError::invalid_root(
            "learning root must be an explicit absolute path (it was empty)",
        ));
    }
    if text.trim_start().starts_with('~') {
        return Err(LearningError::invalid_root(
            "learning root must be an explicit absolute path (\"~\" is not expanded)",
        ));
    }
    if !root.is_absolute() {
        return Err(LearningError::invalid_root(format!(
            "learning root must be an absolute path, got {}",
            root.display()
        )));
    }
    Ok(root.to_path_buf())
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn is_busy(error: &rusqlite::Error) -> bool {
    matches!(
        error,
        rusqlite::Error::SqliteFailure(failure, _)
            if failure.code == ErrorCode::DatabaseBusy || failure.code == ErrorCode::DatabaseLocked
    )
}

pub(crate) fn db_error(error: rusqlite::Error) -> LearningError {
    if is_busy(&error) {
        return LearningError::locked(format!("learning database is locked: {error}"));
    }
    LearningError::state(format!("learning database error: {error}"))
}

/// The storage timestamp written for a row this store appends (RFC 3339,
/// millisecond precision, `Z` suffix).
pub(crate) fn storage_now() -> String {
    now_rfc3339()
}

/// `--actor` violates the stage 5 whitelist `[A-Za-z0-9._:@/-]{1,64}`. Usage
/// class. The whitelist keeps `by=`/`owner=` source-line fields unforgeable:
/// no whitespace, `=`, brackets, parentheses or full-width colon can enter an
/// actor value. A blank actor keeps the pre-stage-5 `INVALID_USAGE` result so
/// existing callers and tests see the same code.
pub const ACTOR_INVALID: &str = "ACTOR_INVALID";

/// Longest accepted normalized actor id, in code points.
pub const ACTOR_MAX_CODEPOINTS: usize = 64;

fn is_actor_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | ':' | '@' | '/' | '-')
}

/// Normalize the required `--actor` identity. v4 records provenance on every
/// event; the `<host>:<session or agent>` shape is a convention, not
/// authentication (R14). Stage 5 §A tightens the accepted alphabet to
/// `[A-Za-z0-9._:@/-]{1,64}` so an actor value can never forge an injection
/// source-line field.
pub(crate) fn require_actor(value: &str) -> Result<String, LearningError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(LearningError::usage(
            "--actor must be a non-empty acting identity such as <host>:<session>",
        ));
    }
    // The whitelist applies to the trimmed identity: the historical contract
    // trims `--actor` before recording it, and an existing unit test pins that
    // a surrounding space is normalization, not part of the value. Whitespace
    // *inside* the identity is rejected by the whitelist.
    if trimmed.chars().count() > ACTOR_MAX_CODEPOINTS {
        return Err(LearningError::new(
            ACTOR_INVALID,
            super::LearningErrorKind::Usage,
            format!(
                "--actor must be at most {ACTOR_MAX_CODEPOINTS} code points, got {}",
                trimmed.chars().count()
            ),
        ));
    }
    if let Some(ch) = trimmed
        .chars()
        .find(|ch| !is_actor_char(*ch) || forbidden_codepoint_class(*ch).is_some())
    {
        let detail = match forbidden_codepoint_class(ch) {
            Some(class) => format!("forbidden Unicode class {class} (U+{:04X})", ch as u32),
            None => format!("character {ch:?} (U+{:04X})", ch as u32),
        };
        return Err(LearningError::new(
            ACTOR_INVALID,
            super::LearningErrorKind::Usage,
            format!(
                "--actor must match [A-Za-z0-9._:@/-]{{1,{ACTOR_MAX_CODEPOINTS}}}; it contains \
                 {detail}"
            ),
        ));
    }
    Ok(trimmed.to_string())
}

/// Parse a stored RFC 3339 timestamp; a stored value that does not parse is an
/// integrity failure, never silently ordered.
fn parse_stored_timestamp(
    field: &str,
    value: &str,
) -> Result<DateTime<chrono::FixedOffset>, LearningError> {
    DateTime::parse_from_rfc3339(value).map_err(|error| {
        LearningError::schema_invalid(format!(
            "stored {field} {value:?} is not an RFC 3339 timestamp: {error}"
        ))
    })
}

fn apply_pragmas(conn: &Connection) -> Result<(), LearningError> {
    conn.busy_timeout(Duration::from_secs(BUSY_TIMEOUT_SECS))
        .map_err(db_error)?;
    conn.execute_batch("PRAGMA foreign_keys=ON; PRAGMA synchronous=FULL;")
        .map_err(db_error)?;
    let mode: String = conn
        .query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))
        .map_err(db_error)?;
    if !mode.eq_ignore_ascii_case("wal") {
        return Err(LearningError::state(format!(
            "learning database refused WAL journaling (mode={mode})"
        )));
    }
    Ok(())
}

pub(crate) fn parse_record(json: &str) -> Result<CandidateRecord, LearningError> {
    serde_json::from_str(json).map_err(|error| {
        LearningError::schema_invalid(format!("stored candidate record is not readable: {error}"))
    })
}

fn lookup_ingestion(
    conn: &Connection,
    ingest_key: &str,
) -> Result<Option<StoredIngestion>, LearningError> {
    conn.query_row(
        "SELECT ingest_key, candidate_id, revision, payload_sha256, extractor, source_checkpoint, created_at \
         FROM ingestions WHERE ingest_key = ?1",
        params![ingest_key],
        |row| {
            Ok(StoredIngestion {
                ingest_key: row.get(0)?,
                candidate_id: row.get(1)?,
                revision: row.get::<_, i64>(2)? as u64,
                payload_sha256: row.get(3)?,
                extractor: row.get(4)?,
                source_checkpoint: row.get(5)?,
                created_at: row.get(6)?,
            })
        },
    )
    .optional()
    .map_err(db_error)
}

pub(crate) fn load_current(
    conn: &Connection,
    candidate_id: &str,
) -> Result<Option<(CandidateRecord, String)>, LearningError> {
    let row: Option<(String, String)> = conn
        .query_row(
            "SELECT record_json, payload_sha256 FROM candidates WHERE candidate_id = ?1",
            params![candidate_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(db_error)?;
    match row {
        Some((json, payload)) => Ok(Some((parse_record(&json)?, payload))),
        None => Ok(None),
    }
}

/// The JSON key of stage 6 §D `origin_tasks`. It is checked structurally (key
/// presence, not value) by the downgrade guard: once a payload ever carried
/// the key, a v3 downgrade would silently drop it.
const ORIGIN_TASKS_KEY: &str = "origin_tasks";

/// Stage 6 §D write-side link checks, inside the ingest transaction and before
/// any row is written: every origin id must exist
/// ([`super::model::ORIGIN_TASK_UNKNOWN`]), must be a task candidate
/// ([`super::model::ORIGIN_TARGET_NOT_TASK`]) and must not repeat the
/// candidate claim ([`super::model::ORIGIN_CLAIM_COPY`], normalized with
/// [`super::model::normalize_claim_for_copy_lint`]). Selects only.
fn validate_origin_links(conn: &Connection, record: &CandidateRecord) -> Result<(), LearningError> {
    if record.origin_tasks.is_empty() {
        return Ok(());
    }
    let candidate_claim = super::model::normalize_claim_for_copy_lint(&record.claim);
    for origin in &record.origin_tasks {
        let Some((target, _)) = load_current(conn, origin)? else {
            return Err(LearningError::new(
                super::model::ORIGIN_TASK_UNKNOWN,
                super::LearningErrorKind::Usage,
                format!(
                    "origin_tasks names {origin:?}, which is not a candidate in this learning \
                     root; a knowledge candidate may only distill an existing task"
                ),
            ));
        };
        if target.target_kind != TASK_TARGET_KIND {
            return Err(LearningError::new(
                super::model::ORIGIN_TARGET_NOT_TASK,
                super::LearningErrorKind::Usage,
                format!(
                    "origin_tasks names {origin:?}, whose target_kind is {:?}; only a \
                     target_kind \"task\" candidate can be distilled",
                    target.target_kind
                ),
            ));
        }
        if super::model::normalize_claim_for_copy_lint(&target.claim) == candidate_claim {
            return Err(LearningError::new(
                super::model::ORIGIN_CLAIM_COPY,
                super::LearningErrorKind::Usage,
                format!(
                    "claim copies the claim of origin task {origin:?} (equal after trim, case \
                     folding, whitespace collapsing and fullwidth folding); distillation must \
                     not copy the task claim verbatim"
                ),
            ));
        }
    }
    Ok(())
}

/// Stage 6 §D downgrade guard: the candidate ids whose stored current record or
/// any historical revision snapshot carries the `origin_tasks` key, sorted.
/// Read-only. A `record_json` that does not parse is *not* counted here (the
/// existing downgrade path already refuses such a store later), so a store
/// without the key keeps the exact pre-stage-6 behaviour.
fn origin_tasks_blockers(conn: &Connection) -> Result<Vec<String>, LearningError> {
    let mut blockers = BTreeSet::new();
    for sql in [
        "SELECT candidate_id, record_json FROM candidates",
        "SELECT candidate_id, record_json FROM candidate_revisions",
    ] {
        let mut stmt = conn.prepare(sql).map_err(db_error)?;
        let rows = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(db_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(db_error)?;
        for (candidate_id, record_json) in rows {
            let Ok(value) = serde_json::from_str::<Value>(&record_json) else {
                continue;
            };
            if value.get(ORIGIN_TASKS_KEY).is_some() {
                blockers.insert(candidate_id);
            }
        }
    }
    Ok(blockers.into_iter().collect())
}

/// The fail-closed refusal of a downgrade blocked by `origin_tasks`. The
/// message carries the distinct candidate count and the first 20 sorted ids.
fn origin_tasks_blocked_error(ids: &[String]) -> LearningError {
    const LIST_LIMIT: usize = 20;
    let listed: Vec<String> = ids.iter().take(LIST_LIMIT).cloned().collect();
    let more = ids.len().saturating_sub(listed.len());
    let suffix = if more > 0 {
        format!(" (and {more} more)")
    } else {
        String::new()
    };
    LearningError::new(
        DOWNGRADE_BLOCKED_ORIGIN_TASKS,
        super::LearningErrorKind::Conflict,
        format!(
            "{} candidate(s) carry origin_tasks, so this store cannot be downgraded to v3 \
             without losing the distillation links: {}{suffix}; the downgrade was refused \
             before any backup or row change",
            ids.len(),
            listed.join(", ")
        ),
    )
}

fn probe_schema(conn: &Connection, probes: &[&str], db_path: &Path) -> Result<(), LearningError> {
    for probe in probes {
        conn.prepare(probe).map_err(|error| {
            LearningError::schema_invalid(format!(
                "{} is not a learning database (schema probe failed: {error})",
                db_path.display()
            ))
        })?;
    }
    Ok(())
}

/// v1 -> v2 in one `BEGIN IMMEDIATE` transaction.
///
/// The version is re-read under the write lock: a concurrent opener that
/// already migrated makes this a no-op, so the migration is re-entrant. Only
/// `CREATE TABLE IF NOT EXISTS` / `CREATE INDEX IF NOT EXISTS` and the
/// `user_version` bump run; no existing row is read or rewritten.
fn migrate_v1_to_v2(conn: &mut Connection, db_path: &Path) -> Result<(), LearningError> {
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(db_error)?;
    let version: i64 = tx
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .map_err(db_error)?;
    // Another opener may have completed v1 -> v2 (or the whole chain to v3/v4/
    // v5) while this one waited for the write lock: all of those are a no-op.
    if version == DB_SCHEMA_V2
        || version == DB_SCHEMA_V3
        || version == DB_SCHEMA_V4
        || version == i64::from(DB_SCHEMA_VERSION)
    {
        tx.commit().map_err(db_error)?;
        return Ok(());
    }
    if version != 1 {
        return Err(LearningError::schema_version_unsupported(format!(
            "{} changed to user_version {version} while migrating to {DB_SCHEMA_V2}",
            db_path.display()
        )));
    }
    tx.execute_batch(MIGRATION_V1_TO_V2_SQL).map_err(db_error)?;
    tx.pragma_update(None, "user_version", DB_SCHEMA_V2)
        .map_err(db_error)?;
    tx.commit().map_err(db_error)?;
    Ok(())
}

/// v2 -> v3 in one `BEGIN IMMEDIATE` transaction.
///
/// The version is re-read under the write lock: a concurrent opener that
/// already migrated makes this a no-op, so the migration is re-entrant. Only
/// `CREATE TABLE IF NOT EXISTS` / `CREATE INDEX IF NOT EXISTS` and the
/// `user_version` bump run; no existing row is read or rewritten.
fn migrate_v2_to_v3(conn: &mut Connection, db_path: &Path) -> Result<(), LearningError> {
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(db_error)?;
    let version: i64 = tx
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .map_err(db_error)?;
    if version == DB_SCHEMA_V3
        || version == DB_SCHEMA_V4
        || version == i64::from(DB_SCHEMA_VERSION)
    {
        tx.commit().map_err(db_error)?;
        return Ok(());
    }
    if version != DB_SCHEMA_V2 {
        return Err(LearningError::schema_version_unsupported(format!(
            "{} changed to user_version {version} while migrating to {DB_SCHEMA_V3}",
            db_path.display()
        )));
    }
    tx.execute_batch(MIGRATION_V2_TO_V3_SQL).map_err(db_error)?;
    tx.pragma_update(None, "user_version", DB_SCHEMA_V3)
        .map_err(db_error)?;
    tx.commit().map_err(db_error)?;
    Ok(())
}

/// Whether `table` currently exposes `column`. Used only by the v3 -> v4
/// migration to tell the v3 event shape from an already-rebuilt v4 table, so a
/// re-entrant run never rebuilds twice.
fn has_column(conn: &Connection, table: &str, column: &str) -> Result<bool, LearningError> {
    let present: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info(?1) WHERE name = ?2",
            params![table, column],
            |row| row.get(0),
        )
        .map_err(db_error)?;
    Ok(present > 0)
}

/// Assign the global event sequence to every v3 event row, then advance
/// `event_clock` so the next writer continues at `n + 1`.
///
/// Ordering is `(recorded_at, table_rank, seq)` with rank reuse=1,
/// validation=2, task_events=3. `(table_rank, seq)` is unique, so the order is
/// total and the backfill is deterministic. The clock rows are inserted with
/// `DEFAULT VALUES` (not explicit rowids) so `sqlite_sequence` ends at `n`
/// regardless of how SQLite treats explicit AUTOINCREMENT rowids.
fn backfill_event_sequence(tx: &Transaction<'_>) -> Result<i64, LearningError> {
    let mut stmt = tx
        .prepare(
            "SELECT kind, seq FROM ( \
                 SELECT 'reuse' AS kind, seq, recorded_at, 1 AS table_rank FROM reuse_history \
                 UNION ALL \
                 SELECT 'validation' AS kind, seq, recorded_at, 2 AS table_rank FROM validation_history \
                 UNION ALL \
                 SELECT 'task_events' AS kind, seq, recorded_at, 3 AS table_rank FROM task_events \
             ) ORDER BY recorded_at, table_rank, seq",
        )
        .map_err(db_error)?;
    let ordered = stmt
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })
        .map_err(db_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(db_error)?;
    drop(stmt);

    for (index, (kind, seq)) in ordered.iter().enumerate() {
        let sql = match kind.as_str() {
            "reuse" => "UPDATE reuse_history SET event_seq = ?2 WHERE seq = ?1",
            "validation" => "UPDATE validation_history SET event_seq = ?2 WHERE seq = ?1",
            "task_events" => "UPDATE task_events SET event_seq = ?2 WHERE seq = ?1",
            other => {
                return Err(LearningError::schema_invalid(format!(
                    "unknown event table {other:?} in the v3 -> v4 backfill"
                )))
            }
        };
        tx.execute(sql, params![seq, (index as i64) + 1])
            .map_err(db_error)?;
    }
    for _ in 0..ordered.len() {
        tx.execute("INSERT INTO event_clock DEFAULT VALUES", [])
            .map_err(db_error)?;
    }
    Ok(ordered.len() as i64)
}

/// v3 -> v4 in one `BEGIN IMMEDIATE` transaction.
///
/// The version is re-read under the write lock: a concurrent opener that
/// already migrated makes this a no-op, so the migration is re-entrant. The two
/// event tables are rebuilt only when their v3 shape is still present (the
/// `event_seq` column is the guard), and their rows are copied with explicit
/// `seq` — no evidence row is rewritten beyond the new columns. Any failure
/// rolls the whole rebuild back, including the `user_version` bump.
fn migrate_v3_to_v4(conn: &mut Connection, db_path: &Path) -> Result<(), LearningError> {
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(db_error)?;
    let version: i64 = tx
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .map_err(db_error)?;
    if version == DB_SCHEMA_V4 || version == i64::from(DB_SCHEMA_VERSION) {
        tx.commit().map_err(db_error)?;
        return Ok(());
    }
    if version != DB_SCHEMA_V3 {
        return Err(LearningError::schema_version_unsupported(format!(
            "{} changed to user_version {version} while migrating to {DB_SCHEMA_V4}",
            db_path.display()
        )));
    }

    tx.execute_batch(MIGRATION_V3_TO_V4_EVENT_TABLES_SQL)
        .map_err(db_error)?;
    let reuse_rebuilt = !has_column(&tx, "reuse_history", "event_seq")?;
    let validation_rebuilt = !has_column(&tx, "validation_history", "event_seq")?;
    if reuse_rebuilt {
        tx.execute_batch(MIGRATION_V3_TO_V4_REUSE_REBUILD_SQL)
            .map_err(db_error)?;
    }
    if validation_rebuilt {
        tx.execute_batch(MIGRATION_V3_TO_V4_VALIDATION_REBUILD_SQL)
            .map_err(db_error)?;
    }

    let clock_rows: i64 = tx
        .query_row("SELECT COUNT(*) FROM event_clock", [], |row| row.get(0))
        .map_err(db_error)?;
    let event_rows: i64 = tx
        .query_row(
            "SELECT (SELECT COUNT(*) FROM reuse_history) \
                  + (SELECT COUNT(*) FROM validation_history) \
                  + (SELECT COUNT(*) FROM task_events)",
            [],
            |row| row.get(0),
        )
        .map_err(db_error)?;
    if reuse_rebuilt || validation_rebuilt || (clock_rows == 0 && event_rows > 0) {
        backfill_event_sequence(&tx)?;
    }

    tx.pragma_update(None, "user_version", DB_SCHEMA_V4)
        .map_err(db_error)?;
    tx.commit().map_err(db_error)?;
    Ok(())
}

/// The pre-migration snapshot of a v4 store. The name is
/// [`UPGRADE_BACKUP_PREFIX`] + a UTC timestamp; an existing file is never
/// overwritten (a numeric suffix is appended on the unlikely collision).
fn upgrade_backup_path(root: &Path) -> PathBuf {
    let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%S%.3fZ").to_string();
    let base = format!("{UPGRADE_BACKUP_PREFIX}{stamp}");
    let mut candidate = root.join(&base);
    let mut suffix = 1u32;
    while candidate.exists() {
        candidate = root.join(format!("{base}-{suffix}"));
        suffix += 1;
    }
    candidate
}

/// v4 -> v5 in one `BEGIN IMMEDIATE` transaction, after a consistent
/// `VACUUM INTO` snapshot of the v4 store.
///
/// The version is re-read under the write lock: a concurrent opener that
/// already migrated makes this a no-op, so the migration is re-entrant. Only
/// the two new tables/indexes and the `user_version` bump run; no existing row
/// is read or rewritten.
fn migrate_v4_to_v5(conn: &mut Connection, db_path: &Path) -> Result<(), LearningError> {
    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .map_err(db_error)?;
    if version == i64::from(DB_SCHEMA_VERSION) {
        return Ok(());
    }
    if version != DB_SCHEMA_V4 {
        return Err(LearningError::schema_version_unsupported(format!(
            "{} changed to user_version {version} while migrating to {DB_SCHEMA_VERSION}",
            db_path.display()
        )));
    }
    // Prove the v4 shape before writing anything, so a foreign or damaged
    // database is never backed up and never migrated.
    probe_v4(conn, db_path)?;

    let root = db_path.parent().unwrap_or_else(|| Path::new("."));
    let backup = upgrade_backup_path(root);
    conn.execute(
        "VACUUM INTO ?1",
        params![backup.to_string_lossy().to_string()],
    )
    .map_err(db_error)?;

    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(db_error)?;
    let locked_version: i64 = tx
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .map_err(db_error)?;
    if locked_version == i64::from(DB_SCHEMA_VERSION) {
        tx.commit().map_err(db_error)?;
        return Ok(());
    }
    if locked_version != DB_SCHEMA_V4 {
        return Err(LearningError::schema_version_unsupported(format!(
            "{} changed to user_version {locked_version} while migrating to {DB_SCHEMA_VERSION}",
            db_path.display()
        )));
    }
    tx.execute_batch(MIGRATION_V4_TO_V5_SQL).map_err(db_error)?;
    tx.pragma_update(None, "user_version", i64::from(DB_SCHEMA_VERSION))
        .map_err(db_error)?;
    tx.commit().map_err(db_error)?;
    Ok(())
}

/// Allocate the next global event sequence inside the caller's
/// `BEGIN IMMEDIATE` transaction.
///
/// `event_clock` is the single sequence source: every write to
/// `reuse_history`, `validation_history` or `task_events` calls this first, so
/// `event_seq` is comparable across tables while each table's `seq` remains a
/// per-table primary key only.
pub(crate) fn allocate_event_seq(tx: &Transaction<'_>) -> Result<i64, LearningError> {
    tx.execute("INSERT INTO event_clock DEFAULT VALUES", [])
        .map_err(db_error)?;
    Ok(tx.last_insert_rowid())
}

/// The next per-`(task_id, actor_id)` idempotency sequence, computed inside the
/// caller's write transaction. The v3-compatible `record_reuse` path uses it to
/// keep every legacy write unique under `reuse_history_idem_idx`.
pub(crate) fn next_client_seq(
    tx: &Transaction<'_>,
    task_id: &str,
    actor_id: &str,
) -> Result<i64, LearningError> {
    tx.query_row(
        "SELECT COALESCE(MAX(client_seq), 0) + 1 FROM reuse_history \
         WHERE task_id = ?1 AND actor_id = ?2",
        params![task_id, actor_id],
        |row| row.get(0),
    )
    .map_err(db_error)
}

/// Whether `candidate_id` already recorded `task_id` in `reuse_history`.
///
/// This is the v4 in-transaction replacement for the v3 table-level
/// `UNIQUE (candidate_id, task_id)`: the SELECT must run in the same
/// `BEGIN IMMEDIATE` transaction as the INSERT (see the module docs).
pub(crate) fn reuse_recorded(
    tx: &Transaction<'_>,
    candidate_id: &str,
    task_id: &str,
) -> Result<bool, LearningError> {
    let found: i64 = tx
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM reuse_history WHERE candidate_id = ?1 AND task_id = ?2)",
            params![candidate_id, task_id],
            |row| row.get(0),
        )
        .map_err(db_error)?;
    Ok(found != 0)
}

fn probe_v4(conn: &Connection, db_path: &Path) -> Result<(), LearningError> {
    probe_schema(conn, &PROBES_V4, db_path)?;
    probe_schema(conn, &[PROBE_V4_VALIDATION], db_path)
}

fn probe_v5(conn: &Connection, db_path: &Path) -> Result<(), LearningError> {
    probe_schema(conn, &PROBES_V5, db_path)?;
    probe_schema(conn, &[PROBE_V4_VALIDATION], db_path)
}

/// S3 §4: an accepted conclusion is only replaced by a *later* revision.
///
/// A submission that claims `revision = current + 1` must not carry an
/// `updated_at` earlier than the stored current revision (RFC 3339 compared as
/// instants, so mixed offsets order correctly). Equality is accepted: a batch
/// that observes the same instant is not out of order. A late-arriving *older*
/// revision is refused here; old evidence that arrives after the fact has only
/// one path — a same-revision append to the current revision — so an accepted
/// revision is never rolled back to an earlier one.
fn ensure_supersession_order(
    current: &CandidateRecord,
    incoming: &CandidateRecord,
) -> Result<(), LearningError> {
    let stored = parse_stored_timestamp("updated_at", &current.updated_at)?;
    let next = DateTime::parse_from_rfc3339(&incoming.updated_at).map_err(|error| {
        LearningError::invalid_candidate(format!(
            "updated_at {:?} is not an RFC 3339 timestamp: {error}",
            incoming.updated_at
        ))
    })?;
    if next < stored {
        return Err(LearningError::revision_conflict(format!(
            "candidate {} revision {} is an out-of-order revision: updated_at {} is earlier than \
             the stored revision {} updated_at {}",
            incoming.candidate_id,
            incoming.revision,
            incoming.updated_at,
            current.revision,
            current.updated_at
        )));
    }
    Ok(())
}

/// S3 §3 / S5: `dedup`, `reuse` and `validation` are workflow state, not
/// candidate content.
///
/// Two submissions of the same revision are otherwise compared field for field
/// exactly as S1 does; only those three event blocks are equalized first, so a
/// candidate whose dedup/reuse/validation result was recorded can still receive
/// a same-revision evidence append. A pre-S5 database can never hold a non-empty
/// `reuse`/`validation` (the v1 input rejects them and there is no other
/// writer), so this cannot alter any S1–S3 behaviour; it only keeps the S5
/// events from freezing the evidence of the revision they were recorded on.
/// Every other field — including `claim`, `promotion` and `created_at` — keeps
/// S1's strict equality, and the merged record keeps the stored event state (an
/// append never resets it to `pending`/empty).
fn same_revision_content(current: &CandidateRecord, incoming: &CandidateRecord) -> bool {
    let mut probe = current.clone();
    probe.dedup = incoming.dedup.clone();
    probe.reuse = incoming.reuse.clone();
    probe.validation = incoming.validation.clone();
    probe.same_revision_fields(incoming)
}

impl LearningStore {
    /// Crate-internal connection access for sibling modules that own a
    /// workflow transaction (the dedup disposition record).
    pub(crate) fn connection_mut(&mut self) -> &mut Connection {
        &mut self.conn
    }

    /// Crate-internal read-only connection access (event views, projections).
    pub(crate) fn connection(&self) -> &Connection {
        &self.conn
    }
    /// Create `learning.db` in a missing or empty root. An already valid
    /// learning database is returned as-is (idempotent). Concurrent
    /// initializers wait for the winner instead of deleting its schema.
    pub fn init(root: &Path) -> Result<Self, LearningError> {
        let root = resolve_root(root)?;
        let deadline = Instant::now() + Duration::from_secs(BUSY_TIMEOUT_SECS);
        loop {
            match Self::init_once(&root) {
                Ok(store) => return Ok(store),
                Err(error)
                    if error.kind == super::LearningErrorKind::Lock
                        || error.code == "SCHEMA_VERSION_UNSUPPORTED" =>
                {
                    if Instant::now() >= deadline {
                        return Err(error);
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
                Err(error) => return Err(error),
            }
        }
    }

    fn init_once(root: &Path) -> Result<Self, LearningError> {
        let root = resolve_root(root)?;
        let db_path = root.join(DB_FILE);
        if db_path.exists() {
            return Self::open(&root);
        }
        if root.exists() {
            if !root.is_dir() {
                return Err(LearningError::state(format!(
                    "learning root {} exists and is not a directory",
                    root.display()
                )));
            }
            let mut entries =
                fs::read_dir(&root).map_err(|error| LearningError::io(&root, error))?;
            if let Some(entry) = entries.next() {
                let name = entry
                    .map_err(|error| LearningError::io(&root, error))?
                    .file_name();
                let name = name.to_string_lossy();
                // Another initializer may create learning.db between the first
                // existence check and this directory read. Re-open that DB (or
                // wait for its schema) instead of misclassifying it as foreign.
                if name == DB_FILE || db_path.exists() {
                    return Self::open(&root);
                }
                if name == format!("{DB_FILE}-wal") || name == format!("{DB_FILE}-shm") {
                    return Err(LearningError::locked(
                        "learning root is being initialized by another writer",
                    ));
                }
                return Err(LearningError::state(format!(
                    "learning root {} is not empty and holds no {DB_FILE}; refusing to initialize",
                    root.display()
                )));
            }
        } else {
            fs::create_dir_all(&root).map_err(|error| LearningError::io(&root, error))?;
        }

        let conn = Connection::open(&db_path)
            .map_err(|error| LearningError::io(&db_path, std::io::Error::other(error)))?;
        let created = (|| -> Result<(), LearningError> {
            apply_pragmas(&conn)?;
            let tx = conn.unchecked_transaction().map_err(db_error)?;
            tx.execute_batch(SCHEMA_SQL).map_err(db_error)?;
            tx.pragma_update(None, "user_version", i64::from(DB_SCHEMA_VERSION))
                .map_err(db_error)?;
            tx.commit().map_err(db_error)?;
            Ok(())
        })();
        if let Err(error) = created {
            drop(conn);
            // A concurrent initializer may have committed the same schema
            // while this connection was waiting. Never delete that valid DB;
            // only clean up when no readable learning store materialized.
            match Self::open(&root) {
                Ok(store) => return Ok(store),
                Err(open_error)
                    if open_error.kind == super::LearningErrorKind::Lock
                        || open_error.code == "SCHEMA_VERSION_UNSUPPORTED" =>
                {
                    return Err(open_error);
                }
                Err(_) => {
                    // A failed initializer never deletes files. Another process
                    // may own the schema transaction, and an incomplete local
                    // attempt must remain available for fail-closed diagnosis.
                    return Err(error);
                }
            }
        }
        drop(conn);
        Self::open(&root)
    }

    /// Open an existing learning database.
    ///
    /// `PRAGMA user_version = 5` opens directly; `user_version = 4` is probed
    /// as v4, snapshotted to `learning.db.bak-v4-<UTC>` and migrated to v5.
    /// `user_version = 3` is probed as v3, migrated to v4 (the global event
    /// clock, `task_events` and the two rebuilt event tables are
    /// created/backfilled if and only if the v3 shape is still present), probed
    /// as v4 and migrated to v5. `user_version = 2` (an S3 store) is probed,
    /// migrated to v3, v4 and v5; `user_version = 1` (an S1 store) runs the v1
    /// probes, migrates to v2, v3, v4 and v5. Each migration is re-entrant and
    /// never rewrites an existing row beyond the columns it adds.
    /// Any other version, a foreign database and a partial/damaged schema still
    /// fail closed; a v1/v2/v3 database that fails its probes is rejected
    /// *without* being migrated.
    pub fn open(root: &Path) -> Result<Self, LearningError> {
        let root = resolve_root(root)?;
        let db_path = root.join(DB_FILE);
        if !db_path.is_file() {
            return Err(LearningError::db_missing(format!(
                "no {DB_FILE} in {}; run `agent-memory learning init --root <absolute-root>`",
                root.display()
            )));
        }
        let mut conn = Connection::open(&db_path)
            .map_err(|error| LearningError::io(&db_path, std::io::Error::other(error)))?;
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .map_err(|error| {
                LearningError::schema_invalid(format!(
                    "{} is not a readable SQLite learning database: {error}",
                    db_path.display()
                ))
            })?;
        match version {
            1 => {
                // Prove it is a learning store before writing anything: a
                // foreign or partial v1 database must not be migrated.
                probe_schema(&conn, &PROBES_V1, &db_path)?;
                migrate_v1_to_v2(&mut conn, &db_path)?;
                probe_schema(&conn, &PROBES_V2, &db_path)?;
                migrate_v2_to_v3(&mut conn, &db_path)?;
                probe_schema(&conn, &PROBES_V3, &db_path)?;
                migrate_v3_to_v4(&mut conn, &db_path)?;
                probe_v4(&conn, &db_path)?;
                migrate_v4_to_v5(&mut conn, &db_path)?;
            }
            v if v == DB_SCHEMA_V2 => {
                // Prove it is a learning store before writing anything: a
                // foreign or partial v2 database must not be migrated.
                probe_schema(&conn, &PROBES_V2, &db_path)?;
                migrate_v2_to_v3(&mut conn, &db_path)?;
                probe_schema(&conn, &PROBES_V3, &db_path)?;
                migrate_v3_to_v4(&mut conn, &db_path)?;
                probe_v4(&conn, &db_path)?;
                migrate_v4_to_v5(&mut conn, &db_path)?;
            }
            v if v == DB_SCHEMA_V3 => {
                // Prove the v3 shape before writing anything: a foreign or
                // partial v3 database must not be rebuilt.
                probe_schema(&conn, &PROBES_V3, &db_path)?;
                migrate_v3_to_v4(&mut conn, &db_path)?;
                probe_v4(&conn, &db_path)?;
                migrate_v4_to_v5(&mut conn, &db_path)?;
            }
            v if v == DB_SCHEMA_V4 => {
                probe_v4(&conn, &db_path)?;
                migrate_v4_to_v5(&mut conn, &db_path)?;
            }
            v if v == i64::from(DB_SCHEMA_VERSION) => {}
            _ => {
                return Err(LearningError::schema_version_unsupported(format!(
                    "{} has user_version {version}; only versions 1, 2, 3 and 4 (migrated to \
                     {DB_SCHEMA_VERSION}) and version {DB_SCHEMA_VERSION} are supported",
                    db_path.display()
                )))
            }
        }
        probe_v5(&conn, &db_path)?;
        apply_pragmas(&conn)?;
        Ok(Self {
            root,
            db_path,
            conn,
        })
    }

    /// Open an existing learning root **read-only** for the stage 5 §B
    /// injection surface.
    ///
    /// Unlike [`LearningStore::open`] this never migrates: a v1/v2/v3 database
    /// is refused with `SCHEMA_VERSION_UNSUPPORTED` instead of being written to,
    /// and the connection itself is `SQLITE_OPEN_READ_ONLY` with
    /// `PRAGMA query_only=ON`. A missing database is `LEARNING_DB_MISSING`. The
    /// short busy timeout keeps a hook from hanging on a locked store.
    pub fn open_read_only(root: &Path) -> Result<Self, LearningError> {
        let root = resolve_root(root)?;
        let db_path = root.join(DB_FILE);
        if !db_path.is_file() {
            return Err(LearningError::db_missing(format!(
                "no {DB_FILE} in {}; run `agent-memory learning init --root <absolute-root>`",
                root.display()
            )));
        }
        let conn = Connection::open_with_flags(&db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|error| LearningError::io(&db_path, std::io::Error::other(error)))?;
        conn.busy_timeout(Duration::from_millis(READ_ONLY_BUSY_TIMEOUT_MS))
            .map_err(db_error)?;
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .map_err(|error| {
                if is_busy(&error) {
                    LearningError::locked(format!("learning database is locked: {error}"))
                } else {
                    LearningError::schema_invalid(format!(
                        "{} is not a readable SQLite learning database: {error}",
                        db_path.display()
                    ))
                }
            })?;
        if version != i64::from(DB_SCHEMA_VERSION) {
            return Err(LearningError::schema_version_unsupported(format!(
                "{} has user_version {version}; the read-only injection surface requires \
                 {DB_SCHEMA_VERSION} and never migrates in place — open the store with a \
                 read-write command first",
                db_path.display()
            )));
        }
        // Belt and braces: even a future code path cannot write through this
        // connection.
        conn.execute_batch("PRAGMA query_only=ON;")
            .map_err(db_error)?;
        Ok(Self {
            root,
            db_path,
            conn,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn db_path(&self) -> &Path {
        &self.db_path
    }

    /// Downgrade a v4 store to the exact v3 shape (`learning migrate --to 3`).
    ///
    /// This is the only *explicit* downgrade path; it never runs from `open`.
    /// Steps, in order:
    ///
    /// 1. a consistent snapshot is written **outside** any transaction with
    ///    `VACUUM INTO <root>/learning.db.bak`; an existing backup or sidecar
    ///    aborts without overwriting either;
    /// 2. `learning.db.bak.meta.json` records the timestamp and the
    ///    source/backup fingerprints before any row changes;
    /// 3. one `BEGIN IMMEDIATE` transaction: duplicate knowledge
    ///    `(candidate_id, task_id)` rows are refused with
    ///    `MIGRATION_PRECHECK_FAILED` (the v3 UNIQUE would reject them and the
    ///    whole downgrade must not lose data), then every `target_kind='task'`
    ///    candidate and its history are removed, plus knowledge reuse rows
    ///    whose `task_id` dangles at a deleted task (`orphan_task_refs`);
    /// 4. `reuse_history` and `validation_history` are rebuilt into the exact
    ///    v3 shape with their original `seq` values (AUTOINCREMENT continuity
    ///    is asserted with a savepoint-rolled-back probe insert);
    /// 5. `task_events` and `event_clock` are dropped and `user_version` is set
    ///    to 3 in the same transaction.
    ///
    /// Lossy for task rows only (the backup restores them); knowledge rows and
    /// knowledge event trunks are preserved, but the v4 actor attribution
    /// columns are dropped by the v3 shape (a later re-open migrates them back
    /// as `migrated:v3`).
    pub fn downgrade_to_v3(root: &Path) -> Result<DowngradeReport, LearningError> {
        let root = resolve_root(root)?;
        let db_path = root.join(DB_FILE);
        if !db_path.is_file() {
            return Err(LearningError::db_missing(format!(
                "no {DB_FILE} in {}; nothing to downgrade",
                root.display()
            )));
        }
        let mut conn = Connection::open(&db_path)
            .map_err(|error| LearningError::io(&db_path, std::io::Error::other(error)))?;
        conn.busy_timeout(Duration::from_secs(BUSY_TIMEOUT_SECS))
            .map_err(db_error)?;
        conn.execute_batch("PRAGMA foreign_keys=ON;")
            .map_err(db_error)?;
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .map_err(|error| {
                LearningError::schema_invalid(format!(
                    "{} is not a readable SQLite learning database: {error}",
                    db_path.display()
                ))
            })?;
        if version == DB_SCHEMA_V3 {
            return Err(LearningError::usage(
                "learning migrate --to 3 only downgrades a v4 store; this root is already \
                 user_version 3",
            ));
        }
        if version == i64::from(DB_SCHEMA_VERSION) {
            return Err(LearningError::usage(
                "learning migrate --to 3 only downgrades a v4 store; v5 has no downgrade path — \
                 restore the pre-v5 `learning.db.bak-v4-*` snapshot instead",
            ));
        }
        if version != DB_SCHEMA_V4 {
            return Err(LearningError::usage(format!(
                "learning migrate --to 3 only downgrades a v4 store; this root has user_version \
                 {version}"
            )));
        }
        // Fail closed before creating a backup: only a complete v4 store is
        // downgraded.
        probe_v4(&conn, &db_path)?;

        // Stage 6 §D: a store that ever wrote `origin_tasks` (current row or a
        // historical revision snapshot) cannot become v3 — the v3 payload has
        // no such field and the links would be lost silently. This scan is
        // read-only and runs before the backup pair and the sidecar, so a
        // refusal leaves the database and the directory byte-identical.
        let blocked_by_origin_tasks = origin_tasks_blockers(&conn)?;
        if !blocked_by_origin_tasks.is_empty() {
            return Err(origin_tasks_blocked_error(&blocked_by_origin_tasks));
        }

        let backup_path = root.join(DOWNGRADE_BACKUP_FILE);
        let sidecar_path = root.join(DOWNGRADE_SIDECAR_FILE);
        for existing in [&backup_path, &sidecar_path] {
            if existing.exists() {
                return Err(LearningError::new(
                    "MIGRATION_BACKUP_EXISTS",
                    super::LearningErrorKind::State,
                    format!(
                        "{} already exists; the downgrade never overwrites its backup, move or \
                         remove that pair first",
                        existing.display()
                    ),
                ));
            }
        }

        // Snapshot outside any transaction. `VACUUM INTO` reads a consistent
        // snapshot of the live database (including the WAL) and fails rather
        // than overwrite an existing file.
        let source_bytes = fs::metadata(&db_path)
            .map_err(|error| LearningError::io(&db_path, error))?
            .len();
        let source_sha256 =
            hex_sha256(&fs::read(&db_path).map_err(|error| LearningError::io(&db_path, error))?);
        conn.execute(
            "VACUUM INTO ?1",
            params![backup_path.to_string_lossy().to_string()],
        )
        .map_err(db_error)?;
        let backup_bytes = fs::metadata(&backup_path)
            .map_err(|error| LearningError::io(&backup_path, error))?
            .len();
        let backup_sha256 = hex_sha256(
            &fs::read(&backup_path).map_err(|error| LearningError::io(&backup_path, error))?,
        );
        let created_at = now_rfc3339();
        let sidecar = serde_json::json!({
            "ok": true,
            "created_at": created_at,
            "from_version": version,
            "to_version": DB_SCHEMA_V3,
            "source": {
                "path": db_path.to_string_lossy(),
                "bytes": source_bytes,
                "sha256": source_sha256,
                "user_version": version,
            },
            "backup": {
                "path": backup_path.to_string_lossy(),
                "bytes": backup_bytes,
                "sha256": backup_sha256,
            },
        });
        let mut sidecar_bytes = serde_json::to_vec_pretty(&sidecar).map_err(|error| {
            LearningError::state(format!("migrate sidecar cannot be serialized: {error}"))
        })?;
        sidecar_bytes.push(b'\n');
        // The sidecar is written before the data transaction: a failure here
        // aborts with the source untouched (only the backup pair exists).
        fs::write(&sidecar_path, sidecar_bytes)
            .map_err(|error| LearningError::io(&sidecar_path, error))?;

        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(db_error)?;
        let locked_version: i64 = tx
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .map_err(db_error)?;
        if locked_version != version {
            return Err(LearningError::state(format!(
                "{} changed to user_version {locked_version} while downgrading",
                db_path.display()
            )));
        }

        // Collect task candidates in Rust: the candidate payload carries
        // `target_kind`, the `candidates` table has no such column (JSON1 is not
        // assumed). No full candidate validation is attempted here — the row is
        // already stored, only its kind matters.
        let mut task_ids: Vec<String> = Vec::new();
        {
            let mut stmt = tx
                .prepare("SELECT candidate_id, record_json FROM candidates")
                .map_err(db_error)?;
            let rows = stmt
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })
                .map_err(db_error)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(db_error)?;
            for (candidate_id, record_json) in rows {
                let value: Value = serde_json::from_str(&record_json).map_err(|error| {
                    LearningError::schema_invalid(format!(
                        "stored candidate {candidate_id} is not readable: {error}"
                    ))
                })?;
                if value.get("target_kind").and_then(Value::as_str) == Some("task") {
                    task_ids.push(candidate_id);
                }
            }
        }

        tx.execute_batch(
            "CREATE TEMP TABLE IF NOT EXISTS migrate_task_ids (candidate_id TEXT PRIMARY KEY); \
             DELETE FROM migrate_task_ids;",
        )
        .map_err(db_error)?;
        for candidate_id in &task_ids {
            tx.execute(
                "INSERT INTO migrate_task_ids (candidate_id) VALUES (?1)",
                params![candidate_id],
            )
            .map_err(db_error)?;
        }

        // Preflight: a v4 database may hold several knowledge reuse rows for the
        // same (candidate, task) — the v4 idempotency index does not forbid it.
        // The v3 UNIQUE would reject the rebuilt table, so refuse before deleting
        // anything. Rows under a task candidate (or pointing at one) are dropped
        // by this downgrade and cannot collide afterwards.
        let duplicate = tx
            .query_row(
                "SELECT candidate_id, task_id, COUNT(*) FROM reuse_history \
                 WHERE candidate_id NOT IN (SELECT candidate_id FROM migrate_task_ids) \
                   AND task_id NOT IN (SELECT candidate_id FROM migrate_task_ids) \
                 GROUP BY candidate_id, task_id HAVING COUNT(*) > 1 LIMIT 1",
                [],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                },
            )
            .optional()
            .map_err(db_error)?;
        if let Some((candidate_id, task_id, count)) = duplicate {
            return Err(LearningError::new(
                "MIGRATION_PRECHECK_FAILED",
                super::LearningErrorKind::Conflict,
                format!(
                    "candidate {candidate_id} has {count} reuse rows for task {task_id:?}; the v3 \
                     shape requires exactly one, so the downgrade rolled back without deleting \
                     anything"
                ),
            ));
        }

        // Rows kept by the downgrade cannot collide; count the knowledge rows
        // that will be dropped only because their task is disappearing.
        let orphan_task_refs: i64 = tx
            .query_row(
                "SELECT COUNT(*) FROM reuse_history \
                 WHERE candidate_id NOT IN (SELECT candidate_id FROM migrate_task_ids) \
                   AND task_id IN (SELECT candidate_id FROM migrate_task_ids)",
                [],
                |row| row.get(0),
            )
            .map_err(db_error)?;
        let reuse_dropped = tx
            .execute(
                "DELETE FROM reuse_history \
                 WHERE candidate_id IN (SELECT candidate_id FROM migrate_task_ids) \
                    OR task_id IN (SELECT candidate_id FROM migrate_task_ids)",
                [],
            )
            .map_err(db_error)?;
        let validation_dropped = tx
            .execute(
                "DELETE FROM validation_history \
                 WHERE candidate_id IN (SELECT candidate_id FROM migrate_task_ids)",
                [],
            )
            .map_err(db_error)?;
        let task_events_dropped = tx
            .execute(
                "DELETE FROM task_events \
                 WHERE candidate_id IN (SELECT candidate_id FROM migrate_task_ids)",
                [],
            )
            .map_err(db_error)?;
        let status_dropped = tx
            .execute(
                "DELETE FROM status_history \
                 WHERE candidate_id IN (SELECT candidate_id FROM migrate_task_ids)",
                [],
            )
            .map_err(db_error)?;
        let dedup_dropped = tx
            .execute(
                "DELETE FROM dedup_history \
                 WHERE candidate_id IN (SELECT candidate_id FROM migrate_task_ids)",
                [],
            )
            .map_err(db_error)?;
        let revisions_dropped = tx
            .execute(
                "DELETE FROM candidate_revisions \
                 WHERE candidate_id IN (SELECT candidate_id FROM migrate_task_ids)",
                [],
            )
            .map_err(db_error)?;
        let ingestions_dropped = tx
            .execute(
                "DELETE FROM ingestions \
                 WHERE candidate_id IN (SELECT candidate_id FROM migrate_task_ids)",
                [],
            )
            .map_err(db_error)?;
        let candidates_dropped = tx
            .execute(
                "DELETE FROM candidates \
                 WHERE candidate_id IN (SELECT candidate_id FROM migrate_task_ids)",
                [],
            )
            .map_err(db_error)?;

        tx.execute_batch(MIGRATION_V4_TO_V3_REUSE_REBUILD_SQL)
            .map_err(db_error)?;
        tx.execute_batch(MIGRATION_V4_TO_V3_VALIDATION_REBUILD_SQL)
            .map_err(db_error)?;

        // AUTOINCREMENT continuity: inserting after the explicit-seq restore must
        // produce a rowid greater than every restored seq. A savepoint keeps the
        // probe out of the final database (including sqlite_sequence).
        let max_restored_seq: i64 = tx
            .query_row(
                "SELECT COALESCE(MAX(seq), 0) FROM reuse_history",
                [],
                |row| row.get(0),
            )
            .map_err(db_error)?;
        tx.execute_batch("SAVEPOINT migrate_seq_probe; PRAGMA defer_foreign_keys=ON;")
            .map_err(db_error)?;
        tx.execute(
            "INSERT INTO reuse_history (candidate_id, task_id, outcome, notes, recorded_at) \
             VALUES ('__migrate_probe__', '__migrate_probe__', 'pass', '', ?1)",
            params![now_rfc3339()],
        )
        .map_err(db_error)?;
        let probe_seq = tx.last_insert_rowid();
        tx.execute_batch("ROLLBACK TO migrate_seq_probe; RELEASE migrate_seq_probe;")
            .map_err(db_error)?;
        if probe_seq <= max_restored_seq {
            return Err(LearningError::new(
                "MIGRATION_SEQUENCE_UNCONTINUOUS",
                super::LearningErrorKind::State,
                format!(
                    "restored reuse_history ends at seq {max_restored_seq} but the next \
                     AUTOINCREMENT insert would reuse seq {probe_seq}"
                ),
            ));
        }

        let event_seq_max: i64 = tx
            .query_row("SELECT COALESCE(MAX(seq), 0) FROM event_clock", [], |row| {
                row.get(0)
            })
            .map_err(db_error)?;
        let kept_candidates: i64 = tx
            .query_row("SELECT COUNT(*) FROM candidates", [], |row| row.get(0))
            .map_err(db_error)?;
        let kept_reuse: i64 = tx
            .query_row("SELECT COUNT(*) FROM reuse_history", [], |row| row.get(0))
            .map_err(db_error)?;
        let kept_validation: i64 = tx
            .query_row("SELECT COUNT(*) FROM validation_history", [], |row| {
                row.get(0)
            })
            .map_err(db_error)?;

        tx.execute_batch(
            "DROP TABLE IF EXISTS task_events; \
             DROP TABLE IF EXISTS event_clock; \
             DROP TABLE IF EXISTS migrate_task_ids;",
        )
        .map_err(db_error)?;
        tx.pragma_update(None, "user_version", DB_SCHEMA_V3)
            .map_err(db_error)?;
        tx.commit().map_err(db_error)?;

        Ok(DowngradeReport {
            ok: true,
            from_version: version,
            to_version: DB_SCHEMA_V3,
            backup: backup_path.to_string_lossy().to_string(),
            sidecar: sidecar_path.to_string_lossy().to_string(),
            dropped: DowngradeDropped {
                candidates: candidates_dropped,
                candidate_revisions: revisions_dropped,
                ingestions: ingestions_dropped,
                status_history: status_dropped,
                dedup_history: dedup_dropped,
                task_events: task_events_dropped,
                reuse: reuse_dropped,
                validation: validation_dropped,
                orphan_task_refs: orphan_task_refs as usize,
            },
            kept: DowngradeKept {
                candidates: kept_candidates as usize,
                reuse: kept_reuse as usize,
                validation: kept_validation as usize,
                event_seq_max,
            },
        })
    }

    /// Ingest one v1 candidate atomically.
    pub fn ingest(&mut self, value: &Value) -> Result<IngestOutcome, LearningError> {
        self.ingest_with_override(value, None, None)
    }

    /// [`LearningStore::ingest`] with the stage-7 §A anti-rebirth override.
    ///
    /// `override_reason` is the verified `--override-rejected <reason>` value;
    /// `override_actor` defaults to the candidate's `extractor` when the CLI
    /// did not supply `--actor`.
    pub fn ingest_with_override(
        &mut self,
        value: &Value,
        override_reason: Option<&str>,
        override_actor: Option<&str>,
    ) -> Result<IngestOutcome, LearningError> {
        let override_reason = override_reason
            .map(str::trim)
            .filter(|value| !value.is_empty());
        if override_reason.is_none() && override_actor.is_some() {
            return Err(LearningError::usage(
                "--actor is only meaningful together with --override-rejected",
            ));
        }
        let override_actor = override_actor.map(require_actor).transpose()?;
        let control = IngestControl {
            override_reason,
            override_actor: override_actor.as_deref(),
        };
        let validated = validate_candidate(value)?;
        match self.ingest_attempt(&validated, &control) {
            Err(error) if error.code == CONSTRAINT_FALLBACK_CODE => {
                self.resolve_after_constraint(&validated)
            }
            other => other,
        }
    }

    /// Convenience wrapper for the CLI's `<json|@file>` input.
    pub fn ingest_json(&mut self, text: &str) -> Result<IngestOutcome, LearningError> {
        let value: Value = serde_json::from_str(text).map_err(|error| {
            LearningError::invalid_candidate(format!("candidate is not valid JSON: {error}"))
        })?;
        self.ingest(&value)
    }

    fn ingest_attempt(
        &mut self,
        validated: &super::model::ValidatedCandidate,
        control: &IngestControl<'_>,
    ) -> Result<IngestOutcome, LearningError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(db_error)?;
        let outcome = Self::ingest_in_transaction_with(&tx, validated, control)?;
        tx.commit().map_err(db_error)?;
        Ok(outcome)
    }

    /// Ingest one already-validated candidate inside the caller's
    /// `BEGIN IMMEDIATE` transaction (task create reuses it so the candidate
    /// row and its first `claim` event commit together).
    pub(crate) fn ingest_in_transaction(
        conn: &Transaction<'_>,
        validated: &super::model::ValidatedCandidate,
    ) -> Result<IngestOutcome, LearningError> {
        Self::ingest_in_transaction_with(conn, validated, &IngestControl::none())
    }

    fn ingest_in_transaction_with(
        conn: &Transaction<'_>,
        validated: &super::model::ValidatedCandidate,
        control: &IngestControl<'_>,
    ) -> Result<IngestOutcome, LearningError> {
        if let Some(existing) = lookup_ingestion(conn, &validated.ingest_key)? {
            return Self::replay_or_conflict(conn, validated, existing);
        }

        // Stage 6 §D: origin links are checked against the store on every fresh
        // write (a replay returned above); nothing has been written yet.
        validate_origin_links(conn, &validated.record)?;

        // Stage 7 §A: anti-rebirth. Exact matches block unless the caller
        // explicitly overrides; near matches only warn.
        let rejection = super::qualify::check_rejection(conn, &validated.record)?;
        let mut recorded_override = None;
        if !rejection.blocked.is_empty() {
            let Some(reason) = control.override_reason else {
                let listed = rejection
                    .blocked
                    .iter()
                    .map(|(id, claim)| format!("{id} ({claim:?})"))
                    .collect::<Vec<_>>()
                    .join(", ");
                return Err(LearningError::new(
                    super::qualify::REJECTED_CLAIM_MATCH,
                    super::LearningErrorKind::Conflict,
                    format!(
                        "claim matches a rejected candidate: {listed}; a rejected conclusion must \
                         not re-enter as a new premise — pass `learning qualify --state cleared` \
                         first, or record an explicit `--override-rejected <reason>`"
                    ),
                ));
            };
            let actor = control
                .override_actor
                .unwrap_or(validated.record.extractor.as_str());
            let mut rows = Vec::new();
            for (matched_candidate_id, _) in &rejection.blocked {
                rows.push(super::qualify::record_override(
                    conn,
                    &validated.record.candidate_id,
                    matched_candidate_id,
                    reason,
                    actor,
                )?);
            }
            let first = rows
                .first()
                .expect("blocked is non-empty when an override is recorded");
            recorded_override = Some(OverrideOutcome {
                reason: reason.to_string(),
                actor: actor.to_string(),
                matched_candidate_id: first.matched_candidate_id.clone(),
                matched_candidate_ids: rows
                    .iter()
                    .map(|row| row.matched_candidate_id.clone())
                    .collect(),
                recorded_at: first.recorded_at.clone(),
                seq: first.seq,
            });
        }

        let record = &validated.record;
        let stored = load_current(conn, &record.candidate_id)?;
        let evidence_added = match stored {
            None if record.revision != 1 => {
                return Err(LearningError::revision_conflict(format!(
                    "new candidate {} must start at revision 1 (got {})",
                    record.candidate_id, record.revision
                )));
            }
            None => Self::insert_new(conn, validated)?,
            Some((current, _)) if current.revision == record.revision => {
                Self::merge_same_revision(conn, validated, current)?
            }
            Some((current, _)) if current.revision + 1 == record.revision => {
                ensure_supersession_order(&current, record)?;
                Self::insert_next_revision(conn, validated)?
            }
            Some((current, _)) => {
                return Err(LearningError::revision_conflict(format!(
                    "candidate {} is at revision {}; only that revision or {} is accepted (got {})",
                    record.candidate_id,
                    current.revision,
                    current.revision + 1,
                    record.revision
                )));
            }
        };

        Ok(IngestOutcome {
            ok: true,
            candidate_id: record.candidate_id.clone(),
            revision: record.revision,
            status: CANDIDATE_STATUS.to_string(),
            ingest_key: validated.ingest_key.clone(),
            payload_sha256: validated.payload_sha256.clone(),
            replayed: false,
            evidence_added,
            rejected_matches: rejection.near,
            r#override: recorded_override,
        })
    }

    fn replay_or_conflict(
        conn: &Connection,
        validated: &super::model::ValidatedCandidate,
        existing: StoredIngestion,
    ) -> Result<IngestOutcome, LearningError> {
        if existing.payload_sha256 != validated.payload_sha256 {
            return Err(LearningError::ingest_key_conflict(format!(
                "ingest_key {} was already recorded with payload {}; this payload is {}",
                existing.ingest_key, existing.payload_sha256, validated.payload_sha256
            )));
        }
        let current = load_current(conn, &existing.candidate_id)?.ok_or_else(|| {
            LearningError::schema_invalid(format!(
                "ingestion {} references missing candidate {}",
                existing.ingest_key, existing.candidate_id
            ))
        })?;
        Ok(IngestOutcome {
            ok: true,
            candidate_id: existing.candidate_id,
            revision: existing.revision,
            status: current.0.status,
            ingest_key: existing.ingest_key,
            payload_sha256: validated.payload_sha256.clone(),
            replayed: true,
            evidence_added: 0,
            rejected_matches: Vec::new(),
            r#override: None,
        })
    }

    /// The UNIQUE constraint on `ingestions.ingest_key` is the backstop for a
    /// writer that slipped past the in-transaction lookup. It resolves to the
    /// same two outcomes as the serial path.
    fn resolve_after_constraint(
        &mut self,
        validated: &super::model::ValidatedCandidate,
    ) -> Result<IngestOutcome, LearningError> {
        match lookup_ingestion(&self.conn, &validated.ingest_key)? {
            Some(existing) => Self::replay_or_conflict(&self.conn, validated, existing),
            None => Err(LearningError::state(
                "ingest_key collided with the unique constraint but no ingestion row is visible",
            )),
        }
    }

    fn insert_new(
        conn: &Connection,
        validated: &super::model::ValidatedCandidate,
    ) -> Result<usize, LearningError> {
        let record = &validated.record;
        let recorded_at = now_rfc3339();
        conn.execute(
            "INSERT INTO candidates \
             (candidate_id, revision, status, record_json, payload_sha256, created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                record.candidate_id,
                record.revision as i64,
                record.status,
                validated.payload_json,
                validated.payload_sha256,
                record.created_at,
                record.updated_at
            ],
        )
        .map_err(db_error)?;
        conn.execute(
            "INSERT INTO candidate_revisions \
             (candidate_id, revision, status, record_json, payload_sha256, recorded_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                record.candidate_id,
                record.revision as i64,
                record.status,
                validated.payload_json,
                validated.payload_sha256,
                recorded_at
            ],
        )
        .map_err(db_error)?;
        Self::insert_ingestion(conn, validated)?;
        Self::append_status(conn, record, &recorded_at)?;
        Ok(record.evidence.len() + record.contradicting_evidence.len())
    }

    fn merge_same_revision(
        conn: &Connection,
        validated: &super::model::ValidatedCandidate,
        current: CandidateRecord,
    ) -> Result<usize, LearningError> {
        let incoming = &validated.record;
        if !same_revision_content(&current, incoming) {
            return Err(LearningError::revision_conflict(format!(
                "candidate {} at revision {} already has different field values; \
                 same-revision ingests may only append evidence and update updated_at",
                current.candidate_id, current.revision
            )));
        }

        let mut merged = current.clone();
        let mut added = 0usize;
        let mut seen_evidence: BTreeSet<String> = current
            .evidence
            .iter()
            .map(|item| evidence_sha256(item))
            .collect();
        for item in &incoming.evidence {
            if seen_evidence.insert(evidence_sha256(item)) {
                merged.evidence.push(item.clone());
                added += 1;
            }
        }
        let mut seen_contradicting: BTreeSet<String> = current
            .contradicting_evidence
            .iter()
            .map(|item| evidence_sha256(item))
            .collect();
        for item in &incoming.contradicting_evidence {
            if seen_contradicting.insert(evidence_sha256(item)) {
                merged.contradicting_evidence.push(item.clone());
                added += 1;
            }
        }
        merged.updated_at = incoming.updated_at.clone();

        let merged_json = merged.canonical_json();
        let merged_sha = hex_sha256(merged_json.as_bytes());
        conn.execute(
            "UPDATE candidates SET record_json = ?2, payload_sha256 = ?3, updated_at = ?4 \
             WHERE candidate_id = ?1",
            params![
                merged.candidate_id,
                merged_json,
                merged_sha,
                merged.updated_at
            ],
        )
        .map_err(db_error)?;
        Self::insert_ingestion(conn, validated)?;
        Ok(added)
    }

    fn insert_next_revision(
        conn: &Connection,
        validated: &super::model::ValidatedCandidate,
    ) -> Result<usize, LearningError> {
        let record = &validated.record;
        let recorded_at = now_rfc3339();
        conn.execute(
            "INSERT INTO candidate_revisions \
             (candidate_id, revision, status, record_json, payload_sha256, recorded_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                record.candidate_id,
                record.revision as i64,
                record.status,
                validated.payload_json,
                validated.payload_sha256,
                recorded_at
            ],
        )
        .map_err(db_error)?;
        conn.execute(
            "UPDATE candidates \
             SET revision = ?2, status = ?3, record_json = ?4, payload_sha256 = ?5, updated_at = ?6 \
             WHERE candidate_id = ?1",
            params![
                record.candidate_id,
                record.revision as i64,
                record.status,
                validated.payload_json,
                validated.payload_sha256,
                record.updated_at
            ],
        )
        .map_err(db_error)?;
        Self::insert_ingestion(conn, validated)?;
        Self::append_status(conn, record, &recorded_at)?;
        Ok(record.evidence.len() + record.contradicting_evidence.len())
    }

    fn insert_ingestion(
        conn: &Connection,
        validated: &super::model::ValidatedCandidate,
    ) -> Result<(), LearningError> {
        match conn.execute(
            "INSERT INTO ingestions (ingest_key, candidate_id, revision, payload_sha256, extractor, source_checkpoint, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                validated.ingest_key,
                validated.record.candidate_id,
                validated.record.revision as i64,
                validated.payload_sha256,
                validated.record.extractor,
                validated.record.source_checkpoint,
                now_rfc3339()
            ],
        ) {
            Ok(_) => Ok(()),
            Err(rusqlite::Error::SqliteFailure(failure, _))
                if failure.code == ErrorCode::ConstraintViolation =>
            {
                Err(LearningError::new(
                    CONSTRAINT_FALLBACK_CODE,
                    super::LearningErrorKind::Conflict,
                    "ingest_key unique constraint fired",
                ))
            }
            Err(error) => Err(db_error(error)),
        }
    }

    fn append_status(
        conn: &Connection,
        record: &CandidateRecord,
        recorded_at: &str,
    ) -> Result<(), LearningError> {
        conn.execute(
            "INSERT INTO status_history (candidate_id, revision, status, recorded_at) \
             VALUES (?1, ?2, ?3, ?4)",
            params![
                record.candidate_id,
                record.revision as i64,
                record.status,
                recorded_at
            ],
        )
        .map_err(db_error)?;
        Ok(())
    }

    /// Current candidate plus the storage-generated ingestion/revision/history
    /// audit lists.
    pub fn show(&self, candidate_id: &str) -> Result<CandidateView, LearningError> {
        let id = candidate_id.trim();
        if id.is_empty() {
            return Err(LearningError::invalid_candidate(
                "--candidate-id must not be empty",
            ));
        }
        let (json, payload): (String, String) = self
            .conn
            .query_row(
                "SELECT record_json, payload_sha256 FROM candidates WHERE candidate_id = ?1",
                params![id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(db_error)?
            .ok_or_else(|| {
                LearningError::not_found(format!("no candidate {id} in this learning root"))
            })?;
        let candidate = parse_record(&json)?;

        let mut stmt = self
            .conn
            .prepare(
                "SELECT ingest_key, candidate_id, revision, payload_sha256, extractor, source_checkpoint, created_at \
                 FROM ingestions WHERE candidate_id = ?1 ORDER BY revision, ingest_key",
            )
            .map_err(db_error)?;
        let ingestions = stmt
            .query_map(params![id], |row| {
                Ok(StoredIngestion {
                    ingest_key: row.get(0)?,
                    candidate_id: row.get(1)?,
                    revision: row.get::<_, i64>(2)? as u64,
                    payload_sha256: row.get(3)?,
                    extractor: row.get(4)?,
                    source_checkpoint: row.get(5)?,
                    created_at: row.get(6)?,
                })
            })
            .map_err(db_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(db_error)?;

        let mut stmt = self
            .conn
            .prepare(
                "SELECT revision, status, payload_sha256, recorded_at FROM candidate_revisions \
                 WHERE candidate_id = ?1 ORDER BY revision",
            )
            .map_err(db_error)?;
        let revisions = stmt
            .query_map(params![id], |row| {
                Ok(RevisionInfo {
                    revision: row.get::<_, i64>(0)? as u64,
                    status: row.get(1)?,
                    payload_sha256: row.get(2)?,
                    recorded_at: row.get(3)?,
                })
            })
            .map_err(db_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(db_error)?;

        let mut stmt = self
            .conn
            .prepare(
                "SELECT revision, status, recorded_at FROM status_history \
                 WHERE candidate_id = ?1 ORDER BY seq",
            )
            .map_err(db_error)?;
        let status_history = stmt
            .query_map(params![id], |row| {
                Ok(StatusEntry {
                    revision: row.get::<_, i64>(0)? as u64,
                    status: row.get(1)?,
                    recorded_at: row.get(2)?,
                })
            })
            .map_err(db_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(db_error)?;

        Ok(CandidateView {
            ok: true,
            candidate,
            payload_sha256: payload,
            ingestions,
            revisions,
            status_history,
            qualification: super::qualify::load_view(&self.conn, id)?,
        })
    }

    /// Summary list of the learning store. Optionally restricted to one status
    /// (S1 only knows `candidate`). Task candidates are hidden by default
    /// (R11); use [`LearningStore::list_with_tasks`] to include them.
    pub fn list(&self, status: Option<&str>) -> Result<ListReport, LearningError> {
        self.list_with_tasks(status, false)
    }

    /// [`LearningStore::list`] with an explicit task-candidate switch.
    ///
    /// `target_kind` lives in `record_json`, so the exclusion is a Rust-side
    /// filter over the already-parsed record (the same judgement as the task
    /// guards); `count` is the number of returned rows, so task candidates do
    /// not inflate the default totals.
    pub fn list_with_tasks(
        &self,
        status: Option<&str>,
        include_tasks: bool,
    ) -> Result<ListReport, LearningError> {
        let filter = status.map(str::trim).filter(|value| !value.is_empty());
        if let Some(value) = filter {
            if value != CANDIDATE_STATUS {
                return Err(LearningError::invalid_candidate(format!(
                    "--status must be {CANDIDATE_STATUS:?} in S1, got {value:?}"
                )));
            }
        }
        let sql = match filter {
            Some(_) => {
                "SELECT candidate_id, revision, status, record_json, created_at, updated_at \
                 FROM candidates WHERE status = ?1 ORDER BY candidate_id"
            }
            None => {
                "SELECT candidate_id, revision, status, record_json, created_at, updated_at \
                 FROM candidates ORDER BY candidate_id"
            }
        };
        let mut stmt = self.conn.prepare(sql).map_err(db_error)?;
        let mut rows = match filter {
            Some(value) => stmt.query(params![value]).map_err(db_error)?,
            None => stmt.query([]).map_err(db_error)?,
        };
        let mut candidates = Vec::new();
        while let Some(row) = rows.next().map_err(db_error)? {
            let json: String = row.get::<_, String>(3).map_err(db_error)?;
            let record = parse_record(&json)?;
            if !include_tasks && record.target_kind == TASK_TARGET_KIND {
                continue;
            }
            candidates.push(CandidateSummary {
                candidate_id: row.get::<_, String>(0).map_err(db_error)?,
                revision: row.get::<_, i64>(1).map_err(db_error)? as u64,
                status: row.get::<_, String>(2).map_err(db_error)?,
                claim: record.claim,
                target_kind: record.target_kind,
                created_at: row.get::<_, String>(4).map_err(db_error)?,
                updated_at: row.get::<_, String>(5).map_err(db_error)?,
            });
        }
        Ok(ListReport {
            ok: true,
            count: candidates.len(),
            candidates,
        })
    }
}
