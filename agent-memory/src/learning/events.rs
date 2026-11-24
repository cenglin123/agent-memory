//! Stage 3 S5: reuse and independent-validation event records.
//!
//! Two append-only event streams sit next to the dedup record:
//!
//! - [`LearningStore::record_reuse`] (`learning reuse`) records that one
//!   candidate was actually applied to one task with a `pass`/`fail` outcome.
//!   For a *knowledge* candidate one `(candidate_id, task_id)` is recorded once:
//!   a repeat is [`REUSE_TASK_CONFLICT`] (conflict, exit 2). v4 no longer
//!   enforces that pair with a table constraint; the check is an in-transaction
//!   SELECT that shares the `BEGIN IMMEDIATE` transaction with the insert
//!   (defence depth is now the transaction, not a UNIQUE index). The v4 index is
//!   the idempotency key `(task_id, actor_id, client_seq)` instead.
//!   A *task* candidate may accumulate reuse events; its `--task` must equal its
//!   own `candidate_id` ([`TASK_MISMATCH`] otherwise) and its reuse rows are
//!   never mirrored into the payload (the projection reads the event tables).
//! - [`LearningStore::record_validation`] (`learning validate`) records one
//!   independent review with a `pass`/`fail` verdict and a report reference.
//!   There is no uniqueness: the same reviewer may review several rounds and
//!   every round is kept.
//!
//! # Storage
//!
//! Each call is one `BEGIN IMMEDIATE` transaction. It reads the current
//! candidate, appends one history row, appends one event object to
//! `record.reuse[]` / `record.validation[]` (each event is exactly
//! `{task_id, outcome, recorded_at}` / `{reviewer, verdict, report_ref,
//! recorded_at}`) and writes the record back with a refreshed `payload_sha256`
//! — the same "workflow state moves the payload hash" rule as dedup. A *task*
//! candidate is the one exception: its events live in the history tables only
//! and its payload is never rewritten.
//!
//! `status`, `revision`, `evidence`, `contradicting_evidence`,
//! `candidate_revisions`, `ingestions` and `status_history` are never touched.
//! Recording an event never promotes a candidate, never writes the Archive and
//! never provisions a target. History is append-only.
//!
//! # Idempotency
//!
//! Every reuse row carries the acting identity (`--actor`, required since v4)
//! and a per-`(task_id, actor_id)` sequence. A caller may pin that sequence with
//! `--client-seq`; `(task_id, actor_id, client_seq)` is the v4 unique
//! idempotency key. Without `--idempotent` a key collision is
//! [`REUSE_IDEMPOTENCY_CONFLICT`]; with `--idempotent` (only allowed together
//! with `--client-seq`) an existing row with the same candidate, `outcome` and
//! `notes` is returned unchanged with `duplicate: true`, and any content
//! difference is [`REUSE_IDEMPOTENCY_MISMATCH`].
//!
//! The event inputs are *not* part of the v1 candidate payload: `learning
//! ingest` still requires `reuse` and `validation` to be empty arrays, exactly
//! as S1 defined.
//!
//! # Stage 6 §C self-review guard
//!
//! [`LearningStore::record_validation`] recomputes the candidate's conflict set
//! from the store inside the write transaction and refuses an explicit
//! self-review with [`REVIEWER_CONFLICT`] before allocating an event sequence
//! (zero writes). A `[self-reflect]` task additionally refuses a reviewer that
//! normalizes to its creator and requires `--report-ref` to be an existing
//! absolute path ([`REPORT_REF_INVALID`]). Any other task candidate is not
//! intercepted.

use rusqlite::{params, ErrorCode, OptionalExtension, Transaction, TransactionBehavior};
use serde::Serialize;
use serde_json::{json, Value};

use crate::archive::hex_sha256;

use super::anomalies::REFLECT_CLAIM_PREFIX;
use super::inject::fold_for_markers;
use super::model::TASK_TARGET_KIND;
use super::store::{self, LearningStore};
use super::task::{self, project_task_state, DEFAULT_STALE_DAYS};
use super::{LearningError, LearningErrorKind};

/// A reuse record for the same candidate and task already exists. Conflict
/// class (exit 2); also used for a blank `--task`, which cannot name a task.
pub const REUSE_TASK_CONFLICT: &str = "REUSE_TASK_CONFLICT";

/// The `(task_id, actor_id, client_seq)` idempotency key already exists and the
/// caller did not opt into idempotent replay. Conflict class (exit 2).
pub const REUSE_IDEMPOTENCY_CONFLICT: &str = "REUSE_IDEMPOTENCY_CONFLICT";

/// The idempotency key exists but the stored `candidate_id`/`outcome`/`notes`
/// differ from this request, so it is not the same event. Conflict class
/// (exit 2).
pub const REUSE_IDEMPOTENCY_MISMATCH: &str = "REUSE_IDEMPOTENCY_MISMATCH";

/// A task candidate's `--task` must be its own `candidate_id`. Usage class
/// (exit 2).
pub const TASK_MISMATCH: &str = "TASK_MISMATCH";

/// The only accepted reuse outcomes and validation verdicts.
pub const EVENT_RESULTS: [&str; 2] = ["pass", "fail"];

/// Stage 6 §C: the reviewer is an actor of the candidate itself (its authors,
/// evidence actors, reuse actors, or the participants of an origin task), or a
/// `[self-reflect]` task's own creator. Conflict class (exit 2); nothing is
/// written.
pub const REVIEWER_CONFLICT: &str = "REVIEWER_CONFLICT";

/// Stage 6 §C: a `[self-reflect]` task validation must point at an existing
/// absolute report path. Usage class (exit 2); nothing is written.
pub const REPORT_REF_INVALID: &str = "REPORT_REF_INVALID";

/// One validated `learning reuse` invocation.
#[derive(Debug, Clone)]
pub struct ReuseRequest {
    pub candidate_id: String,
    pub task_id: String,
    pub outcome: String,
    pub notes: String,
    /// The acting identity (`<host>:<session or agent>`, free text). Required
    /// since v4: `actor_id` records provenance, it is not authentication.
    pub actor: String,
    /// Caller-pinned per-`(task_id, actor_id)` idempotency sequence.
    pub client_seq: Option<i64>,
    /// Opt into "same key + same content returns the stored event".
    pub idempotent: bool,
}

impl ReuseRequest {
    /// Validate the arguments and resolve the candidate id. Nothing is read
    /// from or written to the store here.
    pub fn parse(
        candidate_id: &str,
        task: &str,
        outcome: &str,
        notes: Option<&str>,
        actor: &str,
    ) -> Result<Self, LearningError> {
        Self::parse_with_idempotency(candidate_id, task, outcome, notes, actor, None, false)
    }

    /// [`ReuseRequest::parse`] plus the v4 idempotency options. `--idempotent`
    /// is only meaningful together with `--client-seq`: without a pinned
    /// sequence the server allocates the next one, so a retry can never land on
    /// the same key (R4).
    pub fn parse_with_idempotency(
        candidate_id: &str,
        task: &str,
        outcome: &str,
        notes: Option<&str>,
        actor: &str,
        client_seq: Option<i64>,
        idempotent: bool,
    ) -> Result<Self, LearningError> {
        let candidate_id = candidate_id.trim();
        if candidate_id.is_empty() {
            return Err(LearningError::usage(
                "--candidate-id must be a non-empty candidate id",
            ));
        }
        let task_id = task.trim();
        if task_id.is_empty() {
            return Err(task_conflict(
                "--task must be a non-empty task identifier; an empty task cannot be recorded",
            ));
        }
        let outcome = outcome.trim();
        if !EVENT_RESULTS.contains(&outcome) {
            return Err(LearningError::usage(format!(
                "--outcome must be one of {EVENT_RESULTS:?}, got {outcome:?}"
            )));
        }
        let actor = super::store::require_actor(actor)?;
        if idempotent && client_seq.is_none() {
            return Err(LearningError::usage(
                "--idempotent must be given together with --client-seq: without a pinned \
                 sequence the server allocates the next one and a retry cannot reuse the key",
            ));
        }
        Ok(Self {
            candidate_id: candidate_id.to_string(),
            task_id: task_id.to_string(),
            outcome: outcome.to_string(),
            notes: notes.unwrap_or_default().trim().to_string(),
            actor,
            client_seq,
            idempotent,
        })
    }
}

/// One validated `learning validate` invocation.
#[derive(Debug, Clone)]
pub struct ValidationRequest {
    pub candidate_id: String,
    pub reviewer: String,
    pub verdict: String,
    pub report_ref: String,
    /// Accepted for CLI symmetry. The v3 `validation_history` schema has no
    /// notes column: `report_ref` is the authoritative reference.
    pub notes: String,
}

impl ValidationRequest {
    /// Validate the arguments and resolve the candidate id. Nothing is read
    /// from or written to the store here.
    pub fn parse(
        candidate_id: &str,
        reviewer: &str,
        verdict: &str,
        report_ref: &str,
        notes: Option<&str>,
    ) -> Result<Self, LearningError> {
        let candidate_id = require_text("--candidate-id", candidate_id)?;
        let reviewer = require_text("--reviewer", reviewer)?;
        let report_ref = require_text("--report-ref", report_ref)?;
        let verdict = require_text("--verdict", verdict)?;
        if !EVENT_RESULTS.contains(&verdict.as_str()) {
            return Err(LearningError::usage(format!(
                "--verdict must be one of {EVENT_RESULTS:?}, got {verdict:?}"
            )));
        }
        Ok(Self {
            candidate_id,
            reviewer,
            verdict,
            report_ref,
            notes: notes.unwrap_or_default().trim().to_string(),
        })
    }
}

fn require_text(name: &str, value: &str) -> Result<String, LearningError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(LearningError::usage(format!("{name} must not be empty")));
    }
    Ok(trimmed.to_string())
}

fn task_conflict(message: impl Into<String>) -> LearningError {
    LearningError::new(REUSE_TASK_CONFLICT, LearningErrorKind::Conflict, message)
}

fn unique_violation(error: &rusqlite::Error) -> bool {
    matches!(
        error,
        rusqlite::Error::SqliteFailure(failure, _)
            if failure.code == ErrorCode::ConstraintViolation
    )
}

/// Result of one reuse record.
#[derive(Debug, Clone, Serialize)]
pub struct ReuseOutcome {
    pub ok: bool,
    pub candidate_id: String,
    pub revision: u64,
    pub status: String,
    pub task_id: String,
    pub outcome: String,
    pub notes: String,
    pub recorded_at: String,
    pub history_seq: i64,
    pub event_seq: i64,
    pub actor: String,
    pub client_seq: i64,
    /// `true` when an idempotent replay returned the already stored event and
    /// wrote nothing.
    pub duplicate: bool,
    pub payload_sha256: String,
    pub reuse_total: usize,
}

/// Result of one validation record.
#[derive(Debug, Clone, Serialize)]
pub struct ValidationOutcome {
    pub ok: bool,
    pub candidate_id: String,
    pub revision: u64,
    pub status: String,
    pub reviewer: String,
    pub verdict: String,
    pub report_ref: String,
    pub recorded_at: String,
    pub history_seq: i64,
    pub payload_sha256: String,
    pub validation_total: usize,
}

impl LearningStore {
    /// Record one reuse event atomically (see the module docs).
    pub fn record_reuse(&mut self, request: &ReuseRequest) -> Result<ReuseOutcome, LearningError> {
        let tx = self
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(store::db_error)?;
        let outcome = record_reuse_in_transaction(&tx, request)?;
        tx.commit().map_err(store::db_error)?;
        Ok(outcome)
    }

    /// Record one independent validation event atomically (see the module
    /// docs). Multiple rounds by the same reviewer are all kept.
    pub fn record_validation(
        &mut self,
        request: &ValidationRequest,
    ) -> Result<ValidationOutcome, LearningError> {
        let tx = self
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(store::db_error)?;
        let outcome = record_validation_in_transaction(&tx, request)?;
        tx.commit().map_err(store::db_error)?;
        Ok(outcome)
    }
}

fn load_record(
    tx: &Transaction<'_>,
    candidate_id: &str,
) -> Result<(super::model::CandidateRecord, String), LearningError> {
    let Some((record, payload_sha256)) = store::load_current(tx, candidate_id)? else {
        return Err(LearningError::not_found(format!(
            "no candidate {candidate_id} in this learning root"
        )));
    };
    Ok((record, payload_sha256))
}

fn reuse_task_seen(record: &super::model::CandidateRecord, task_id: &str) -> bool {
    record
        .reuse
        .iter()
        .any(|entry| entry.get("task_id").and_then(Value::as_str) == Some(task_id))
}

fn idempotency_conflict(message: impl Into<String>) -> LearningError {
    LearningError::new(
        REUSE_IDEMPOTENCY_CONFLICT,
        LearningErrorKind::Conflict,
        message,
    )
}

fn idempotency_mismatch(message: impl Into<String>) -> LearningError {
    LearningError::new(
        REUSE_IDEMPOTENCY_MISMATCH,
        LearningErrorKind::Conflict,
        message,
    )
}

fn task_mismatch(message: impl Into<String>) -> LearningError {
    LearningError::new(TASK_MISMATCH, LearningErrorKind::Usage, message)
}

fn reviewer_conflict(message: impl Into<String>) -> LearningError {
    LearningError::new(REVIEWER_CONFLICT, LearningErrorKind::Conflict, message)
}

fn report_ref_invalid(message: impl Into<String>) -> LearningError {
    LearningError::new(REPORT_REF_INVALID, LearningErrorKind::Usage, message)
}

/// Stage 6 §C: a `[self-reflect]` report reference must be an existing
/// absolute path. `~` is never expanded and a relative path is refused.
fn is_existing_absolute_path(value: &str) -> bool {
    if value.trim_start().starts_with('~') {
        return false;
    }
    let path = std::path::Path::new(value);
    path.is_absolute() && path.exists()
}

/// Stage 6 §C self-review guard, inside the write transaction and before any
/// row is written. It always recomputes from the store: a review package's
/// `review_routing` is material, never authority.
///
/// - knowledge candidate: the normalized reviewer must not be in the store's
///   conflict set ([`super::routing::conflict_actors`]) — [`REVIEWER_CONFLICT`];
/// - `[self-reflect]` task: the reviewer must differ from the projected
///   creator ([`REVIEWER_CONFLICT`]) and `--report-ref` must be an existing
///   absolute path ([`REPORT_REF_INVALID`]);
/// - any other task: no interception (finishing and accepting one's own task is
///   the normal main path; the derived `self_validated` field records it).
fn guard_validation(
    tx: &Transaction<'_>,
    record: &super::model::CandidateRecord,
    request: &ValidationRequest,
) -> Result<(), LearningError> {
    let reviewer = fold_for_markers(request.reviewer.trim());
    if record.target_kind == TASK_TARGET_KIND {
        if !record.claim.starts_with(REFLECT_CLAIM_PREFIX) {
            return Ok(());
        }
        let events = task::load_task_events(tx, &record.candidate_id)?;
        let projection = project_task_state(
            &events,
            chrono::Utc::now().fixed_offset(),
            DEFAULT_STALE_DAYS,
        );
        if let Some(created_by) = projection.created_by.as_deref() {
            if fold_for_markers(created_by.trim()) == reviewer {
                return Err(reviewer_conflict(format!(
                    "candidate {} is a [self-reflect] task created by {created_by:?}; the same \
                     identity cannot validate it (stage 6 §C)",
                    record.candidate_id
                )));
            }
        }
        if !is_existing_absolute_path(&request.report_ref) {
            return Err(report_ref_invalid(format!(
                "--report-ref must be an existing absolute path for a [self-reflect] task, got {:?}",
                request.report_ref
            )));
        }
        return Ok(());
    }
    let conflicts = super::routing::conflict_actors(tx, record)?;
    if conflicts.contains(&reviewer) {
        return Err(reviewer_conflict(format!(
            "reviewer {:?} is an actor of candidate {} (normalized {:?}); an explicit \
             self-review is refused (stage 6 §C)",
            request.reviewer, record.candidate_id, reviewer
        )));
    }
    Ok(())
}

/// One already stored reuse row, read back for an idempotent replay.
struct StoredReuse {
    history_seq: i64,
    event_seq: i64,
    candidate_id: String,
    outcome: String,
    notes: String,
    recorded_at: String,
    client_seq: i64,
}

fn load_idempotent_reuse(
    tx: &Transaction<'_>,
    task_id: &str,
    actor_id: &str,
    client_seq: i64,
) -> Result<Option<StoredReuse>, LearningError> {
    tx.query_row(
        "SELECT seq, event_seq, candidate_id, outcome, notes, recorded_at, client_seq \
         FROM reuse_history WHERE task_id = ?1 AND actor_id = ?2 AND client_seq = ?3",
        params![task_id, actor_id, client_seq],
        |row| {
            Ok(StoredReuse {
                history_seq: row.get(0)?,
                event_seq: row.get(1)?,
                candidate_id: row.get(2)?,
                outcome: row.get(3)?,
                notes: row.get(4)?,
                recorded_at: row.get(5)?,
                client_seq: row.get(6)?,
            })
        },
    )
    .optional()
    .map_err(store::db_error)
}

fn reuse_total(tx: &Transaction<'_>, candidate_id: &str) -> Result<usize, LearningError> {
    let total: i64 = tx
        .query_row(
            "SELECT COUNT(*) FROM reuse_history WHERE candidate_id = ?1",
            params![candidate_id],
            |row| row.get(0),
        )
        .map_err(store::db_error)?;
    Ok(total as usize)
}

/// Resolve a colliding idempotency key into either an idempotent replay or the
/// precise failure. Called with the current record already loaded, so nothing
/// is written on any of these paths.
fn resolve_stored_reuse(
    tx: &Transaction<'_>,
    request: &ReuseRequest,
    record: &super::model::CandidateRecord,
    payload_sha256: &str,
    stored: StoredReuse,
) -> Result<ReuseOutcome, LearningError> {
    if !request.idempotent {
        return Err(idempotency_conflict(format!(
            "reuse key (task {:?}, actor {:?}, client_seq {}) is already recorded; pass \
             --idempotent together with --client-seq to replay it",
            request.task_id, request.actor, stored.client_seq
        )));
    }
    if stored.candidate_id != record.candidate_id
        || stored.outcome != request.outcome
        || stored.notes != request.notes
    {
        return Err(idempotency_mismatch(format!(
            "reuse key (task {:?}, actor {:?}, client_seq {}) is already recorded with \
             candidate {:?}, outcome {:?}, notes {:?}; this request has candidate {:?}, outcome \
             {:?}, notes {:?}",
            request.task_id,
            request.actor,
            stored.client_seq,
            stored.candidate_id,
            stored.outcome,
            stored.notes,
            record.candidate_id,
            request.outcome,
            request.notes
        )));
    }
    Ok(ReuseOutcome {
        ok: true,
        candidate_id: record.candidate_id.clone(),
        revision: record.revision,
        status: record.status.clone(),
        task_id: request.task_id.clone(),
        outcome: stored.outcome,
        notes: stored.notes,
        recorded_at: stored.recorded_at,
        history_seq: stored.history_seq,
        event_seq: stored.event_seq,
        actor: request.actor.clone(),
        client_seq: stored.client_seq,
        duplicate: true,
        payload_sha256: payload_sha256.to_string(),
        reuse_total: reuse_total(tx, &record.candidate_id)?,
    })
}

fn record_reuse_in_transaction(
    tx: &Transaction<'_>,
    request: &ReuseRequest,
) -> Result<ReuseOutcome, LearningError> {
    let (mut record, mut payload_sha256) = load_record(tx, &request.candidate_id)?;
    let is_task = record.target_kind == super::model::TASK_TARGET_KIND;
    if is_task && request.task_id != record.candidate_id {
        return Err(task_mismatch(format!(
            "task candidate {} may only record reuse against its own candidate id; --task was \
             {:?}",
            record.candidate_id, request.task_id
        )));
    }
    // A caller-pinned sequence is the idempotency key: resolve a collision
    // before allocating anything so a replay writes nothing (not even a clock
    // row). This intentionally precedes the knowledge-pair check: an explicit
    // idempotent retry is a declared replay, while an unpinned repeat of the
    // same (candidate, task) keeps the S5 `REUSE_TASK_CONFLICT`.
    if let Some(client_seq) = request.client_seq {
        if let Some(stored) =
            load_idempotent_reuse(tx, &request.task_id, &request.actor, client_seq)?
        {
            return resolve_stored_reuse(tx, request, &record, &payload_sha256, stored);
        }
    }
    // S5 contract for knowledge candidates: one (candidate, task) pair once.
    // A task candidate accumulates reuse events instead (each with its own
    // client sequence), so the pair check does not apply to it.
    if !is_task
        && (reuse_task_seen(&record, &request.task_id)
            || store::reuse_recorded(tx, &record.candidate_id, &request.task_id)?)
    {
        return Err(task_conflict(format!(
            "candidate {} already has a reuse record for task {:?}; one task is recorded once",
            record.candidate_id, request.task_id
        )));
    }

    let client_seq = match request.client_seq {
        Some(seq) => seq,
        None => store::next_client_seq(tx, &request.task_id, &request.actor)?,
    };

    let recorded_at = store::storage_now();
    let event_seq = store::allocate_event_seq(tx)?;
    if let Err(error) = tx.execute(
        "INSERT INTO reuse_history \
         (event_seq, candidate_id, task_id, outcome, notes, recorded_at, actor_id, client_seq) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            event_seq,
            record.candidate_id,
            request.task_id,
            request.outcome,
            request.notes,
            recorded_at,
            request.actor,
            client_seq
        ],
    ) {
        if unique_violation(&error) {
            // The pre-check and this insert share one `BEGIN IMMEDIATE`
            // transaction, so this branch is a backstop only. Never resolve it
            // by returning a replay here: the clock row allocated above would
            // commit without an event.
            if is_task {
                return Err(idempotency_conflict(format!(
                    "reuse key (task {:?}, actor {:?}, client_seq {}) is already recorded",
                    request.task_id, request.actor, client_seq
                )));
            }
            return Err(task_conflict(format!(
                "candidate {} already has a reuse record for task {:?}; one task is recorded once",
                request.candidate_id, request.task_id
            )));
        }
        return Err(store::db_error(error));
    }
    let history_seq = tx.last_insert_rowid();

    if is_task {
        // Task events live in the tables only: the payload is never rewritten,
        // and the read-side projection never looks at `reuse[]` (S8).
        return Ok(ReuseOutcome {
            ok: true,
            candidate_id: record.candidate_id.clone(),
            revision: record.revision,
            status: record.status.clone(),
            task_id: request.task_id.clone(),
            outcome: request.outcome.clone(),
            notes: request.notes.clone(),
            recorded_at,
            history_seq,
            event_seq,
            actor: request.actor.clone(),
            client_seq,
            duplicate: false,
            payload_sha256,
            reuse_total: reuse_total(tx, &record.candidate_id)?,
        });
    }

    record.reuse.push(json!({
        "task_id": request.task_id,
        "outcome": request.outcome,
        "recorded_at": recorded_at,
    }));
    payload_sha256 = write_record(tx, &record)?;

    Ok(ReuseOutcome {
        ok: true,
        candidate_id: record.candidate_id,
        revision: record.revision,
        status: record.status,
        task_id: request.task_id.clone(),
        outcome: request.outcome.clone(),
        notes: request.notes.clone(),
        recorded_at,
        history_seq,
        event_seq,
        actor: request.actor.clone(),
        client_seq,
        duplicate: false,
        payload_sha256,
        reuse_total: record.reuse.len(),
    })
}

fn record_validation_in_transaction(
    tx: &Transaction<'_>,
    request: &ValidationRequest,
) -> Result<ValidationOutcome, LearningError> {
    let (mut record, _) = load_record(tx, &request.candidate_id)?;
    guard_validation(tx, &record, request)?;

    let recorded_at = store::storage_now();
    let event_seq = store::allocate_event_seq(tx)?;
    tx.execute(
        "INSERT INTO validation_history \
         (event_seq, candidate_id, reviewer, verdict, report_ref, recorded_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            event_seq,
            record.candidate_id,
            request.reviewer,
            request.verdict,
            request.report_ref,
            recorded_at
        ],
    )
    .map_err(store::db_error)?;
    let history_seq = tx.last_insert_rowid();

    if record.target_kind == super::model::TASK_TARGET_KIND {
        // A task candidate's payload stays untouched; the projection reads the
        // validation stream, never `validation[]` (S8).
        let validation_total: i64 = tx
            .query_row(
                "SELECT COUNT(*) FROM validation_history WHERE candidate_id = ?1",
                params![record.candidate_id],
                |row| row.get(0),
            )
            .map_err(store::db_error)?;
        let payload_sha256: String = tx
            .query_row(
                "SELECT payload_sha256 FROM candidates WHERE candidate_id = ?1",
                params![record.candidate_id],
                |row| row.get(0),
            )
            .map_err(store::db_error)?;
        return Ok(ValidationOutcome {
            ok: true,
            candidate_id: record.candidate_id,
            revision: record.revision,
            status: record.status,
            reviewer: request.reviewer.clone(),
            verdict: request.verdict.clone(),
            report_ref: request.report_ref.clone(),
            recorded_at,
            history_seq,
            payload_sha256,
            validation_total: validation_total as usize,
        });
    }

    record.validation.push(json!({
        "reviewer": request.reviewer,
        "verdict": request.verdict,
        "report_ref": request.report_ref,
        "recorded_at": recorded_at,
    }));
    let payload_sha256 = write_record(tx, &record)?;

    Ok(ValidationOutcome {
        ok: true,
        candidate_id: record.candidate_id,
        revision: record.revision,
        status: record.status,
        reviewer: request.reviewer.clone(),
        verdict: request.verdict.clone(),
        report_ref: request.report_ref.clone(),
        recorded_at,
        history_seq,
        payload_sha256,
        validation_total: record.validation.len(),
    })
}

fn write_record(
    tx: &Transaction<'_>,
    record: &super::model::CandidateRecord,
) -> Result<String, LearningError> {
    let record_json = record.canonical_json();
    let payload_sha256 = hex_sha256(record_json.as_bytes());
    tx.execute(
        "UPDATE candidates SET record_json = ?2, payload_sha256 = ?3 WHERE candidate_id = ?1",
        params![record.candidate_id, record_json, payload_sha256],
    )
    .map_err(store::db_error)?;
    Ok(payload_sha256)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reuse_arguments_are_trimmed_and_validated() {
        let request = ReuseRequest::parse(
            " cand_x ",
            " task-1 ",
            " pass ",
            None,
            " claude-code:session-1 ",
        )
        .expect("valid");
        assert_eq!(request.candidate_id, "cand_x");
        assert_eq!(request.task_id, "task-1");
        assert_eq!(request.outcome, "pass");
        assert_eq!(request.notes, "");
        assert_eq!(request.actor, "claude-code:session-1");
        assert_eq!(request.client_seq, None);
        assert!(!request.idempotent);

        let request = ReuseRequest::parse(
            "cand_x",
            "task-1",
            "fail",
            Some("  applied twice  "),
            "test:actor",
        )
        .expect("ok");
        assert_eq!(request.notes, "applied twice");

        let error = ReuseRequest::parse("cand_x", "   ", "pass", None, "test:actor")
            .expect_err("blank task");
        assert_eq!(error.code, REUSE_TASK_CONFLICT);
        assert_eq!(error.exit_code(), 2);

        for outcome in ["", "Pass", "blocked"] {
            let error = ReuseRequest::parse("cand_x", "task-1", outcome, None, "test:actor")
                .expect_err("outcome must be rejected");
            assert_eq!(error.code, "INVALID_USAGE", "{outcome:?}");
            assert_eq!(error.exit_code(), 2);
        }

        // v4: the acting identity is required and never blank.
        for actor in ["", "   "] {
            let error = ReuseRequest::parse("cand_x", "task-1", "pass", None, actor)
                .expect_err("actor is required");
            assert_eq!(error.code, "INVALID_USAGE");
            assert_eq!(error.exit_code(), 2);
        }
    }

    #[test]
    fn reuse_idempotency_requires_a_pinned_sequence() {
        let request = ReuseRequest::parse_with_idempotency(
            "cand_x",
            "task-1",
            "pass",
            Some(" first "),
            "test:actor",
            Some(7),
            true,
        )
        .expect("pinned and idempotent");
        assert_eq!(request.client_seq, Some(7));
        assert!(request.idempotent);
        assert_eq!(request.notes, "first");

        // Without a pinned sequence the server allocates the next one, so an
        // idempotent retry could never reuse the key.
        let error = ReuseRequest::parse_with_idempotency(
            "cand_x",
            "task-1",
            "pass",
            None,
            "test:actor",
            None,
            true,
        )
        .expect_err("--idempotent needs --client-seq");
        assert_eq!(error.code, "INVALID_USAGE");
        assert_eq!(error.exit_code(), 2);
    }

    #[test]
    fn validation_arguments_are_mandatory() {
        let request =
            ValidationRequest::parse("cand_x", " reviewer-a ", "pass", " report-1 ", None)
                .expect("valid");
        assert_eq!(request.reviewer, "reviewer-a");
        assert_eq!(request.report_ref, "report-1");
        assert_eq!(request.notes, "");

        for verdict in ["", "passed", "fail "] {
            let expected = verdict.trim();
            let outcome =
                ValidationRequest::parse("cand_x", "reviewer-a", verdict, "report-1", None);
            if EVENT_RESULTS.contains(&expected) {
                assert!(outcome.is_ok(), "{verdict:?}");
            } else {
                let error = outcome.expect_err("verdict must be rejected");
                assert_eq!(error.code, "INVALID_USAGE", "{verdict:?}");
                assert_eq!(error.exit_code(), 2);
            }
        }

        for (reviewer, report_ref) in [("", "report-1"), ("reviewer-a", "  ")] {
            let error = ValidationRequest::parse("cand_x", reviewer, "pass", report_ref, None)
                .expect_err("blank identity is a usage error");
            assert_eq!(error.code, "INVALID_USAGE");
        }
    }
}
