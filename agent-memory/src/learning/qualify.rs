//! Stage 7 §A: rejection / deferral qualification (`learning qualify`).
//!
//! A knowledge candidate may be *qualified*: `rejected` (it was disproved and
//! must not be used as a premise again), `deferred` (it cannot be judged yet
//! and needs a named owner plus a re-review trigger) or `cleared` (the previous
//! qualification is withdrawn and the candidate is back to `open`). The
//! qualification of a candidate is its **newest** appended row; a candidate
//! with no row is `open`.
//!
//! # Storage
//!
//! Two append-only tables live in the same `learning.db` (schema v5):
//!
//! - `qualification_history` — one row per qualification event;
//! - `rejection_overrides` — one row per explicit override of the
//!   anti-rebirth check in `learning ingest`.
//!
//! Every write is one `BEGIN IMMEDIATE` transaction and allocates one global
//! `event_seq` from `event_clock`, exactly like the reuse/validation/task event
//! streams, so qualification events are orderable across tables and the anomaly
//! scan can anchor on them.
//!
//! # Required fields
//!
//! | state | required |
//! |---|---|
//! | `rejected` | `basis`, `counterexample` |
//! | `deferred` | `owner`, `trigger` |
//! | `cleared` | `basis` |
//!
//! Missing fields are `INVALID_USAGE` (exit 2). A `counterexample_ref`, when
//! given, must match gate v1 (`archive-record:<64 lowercase hex>`); with an
//! explicit `--archive --domain` pair it is read back and fingerprint-checked,
//! otherwise the result carries `ref_verified: false`.
//!
//! # Idempotency
//!
//! Only the newest row of a candidate is compared: the same `state` **and** the
//! same `basis` appends nothing and returns `duplicate: true`; for `deferred`
//! the `owner` and `trigger` must match as well, so reassigning a deferral is
//! recorded. Every other call appends, so `rejected -> cleared -> rejected`
//! with the same basis is two new rows, not a replay.
//!
//! # Anti-rebirth
//!
//! `learning ingest` refuses a claim whose target candidate is `rejected`
//! (a new revision of the same claim) or whose normalized claim equals the
//! claim of any other `rejected` candidate (`REJECTED_CLAIM_MATCH`, exit 2) —
//! unless `--override-rejected <reason>` records a `rejection_overrides` row.
//! A claim that is merely *near* a rejected claim (character 3-gram Jaccard
//! `>= 0.6`) is accepted with a `rejected_matches[]` warning.
//!
//! Normalization is one function ([`normalize_for_rejection`]): NFKC, drop
//! whitespace/punctuation/symbols, ASCII lower-case.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Duration as ChronoDuration, FixedOffset};
use rusqlite::{params, Connection, Transaction, TransactionBehavior};
use serde::Serialize;
use unicode_normalization::UnicodeNormalization;

use crate::archive::{hex_sha256, Archive};

use super::gate::{E_REF_FORMAT, E_REF_UNRESOLVED, REF_PREFIX};
use super::model::TASK_TARGET_KIND;
use super::store::{self, LearningStore};
use super::{LearningError, LearningErrorKind};

/// A candidate is rejected: it was disproved.
pub const STATE_REJECTED: &str = "rejected";
/// A candidate cannot be judged yet; an owner must re-review it on a trigger.
pub const STATE_DEFERRED: &str = "deferred";
/// The previous qualification is withdrawn; the candidate is back to `open`.
pub const STATE_CLEARED: &str = "cleared";
/// The implicit state of a candidate with no qualification row.
pub const STATE_OPEN: &str = "open";

/// The three writable qualification states.
pub const QUALIFY_STATES: [&str; 3] = [STATE_REJECTED, STATE_DEFERRED, STATE_CLEARED];

/// A task candidate is not a qualification subject; tasks use `abandon`.
/// Usage class (exit 2).
pub const TASK_NOT_QUALIFIABLE: &str = "TASK_NOT_QUALIFIABLE";
/// `cleared` was requested for a candidate that has no qualification row.
/// Usage class (exit 2).
pub const NOTHING_TO_CLEAR: &str = "NOTHING_TO_CLEAR";
/// An ingest claim matches a rejected claim (the target itself or another
/// candidate). Conflict class (exit 2), zero writes unless overridden.
pub const REJECTED_CLAIM_MATCH: &str = "REJECTED_CLAIM_MATCH";

/// Character 3-gram Jaccard threshold for the near-duplicate *warning*.
pub const NEAR_REJECTED_THRESHOLD: f64 = 0.6;
/// Character 3-gram containment threshold for the near-duplicate *warning*.
///
/// Jaccard penalises rewordings that add or drop context: the OA paraphrase
/// probe measured Jaccard 0.48 but containment 0.65 — one claim is mostly a
/// sub-phrase of the other. Containment (intersection over the smaller set)
/// catches exactly that shape, so a match on either metric warns.
pub const NEAR_REJECTED_CONTAINMENT: f64 = 0.62;
/// A `deferred` row older than this many days is an audit WARN.
pub const DEFERRED_REVIEW_DAYS: i64 = 30;
/// The flip window of the audit / anomaly signal.
pub const FLIP_WINDOW_DAYS: i64 = 7;
/// At least this many rejected <-> cleared flips in the window is a WARN.
pub const FLIP_MIN: usize = 3;

// ---------------------------------------------------------------------------
// Normalization and similarity
// ---------------------------------------------------------------------------

/// The single rejection-matching normalization: NFKC, drop whitespace,
/// punctuation and symbols, then ASCII lower-case.
///
/// Keeping only alphanumeric code points makes fullwidth/halfwidth spellings,
/// punctuation, case and spacing irrelevant; CJK text is preserved because CJK
/// ideographs are alphanumeric.
pub fn normalize_for_rejection(value: &str) -> String {
    value
        .nfkc()
        .filter(|ch| ch.is_alphanumeric())
        .map(|ch| ch.to_ascii_lowercase())
        .collect()
}

fn trigrams(value: &str) -> BTreeSet<String> {
    let chars: Vec<char> = value.chars().collect();
    if chars.is_empty() {
        return BTreeSet::new();
    }
    if chars.len() < 3 {
        let mut set = BTreeSet::new();
        set.insert(value.to_string());
        return set;
    }
    chars
        .windows(3)
        .map(|window| window.iter().collect())
        .collect()
}

/// Character 3-gram Jaccard similarity of two already normalized values.
pub fn jaccard_similarity(left: &str, right: &str) -> f64 {
    let left_grams = trigrams(left);
    let right_grams = trigrams(right);
    if left_grams.is_empty() && right_grams.is_empty() {
        return 1.0;
    }
    let intersection = left_grams.intersection(&right_grams).count() as f64;
    let union = left_grams.union(&right_grams).count() as f64;
    if union == 0.0 {
        0.0
    } else {
        intersection / union
    }
}

/// Character 3-gram containment: intersection over the smaller gram set.
///
/// 1.0 when one normalized claim is a sub-phrase of the other; symmetric.
pub fn containment_similarity(left: &str, right: &str) -> f64 {
    let left_grams = trigrams(left);
    let right_grams = trigrams(right);
    if left_grams.is_empty() && right_grams.is_empty() {
        return 1.0;
    }
    let smaller = left_grams.len().min(right_grams.len());
    if smaller == 0 {
        return 0.0;
    }
    let intersection = left_grams.intersection(&right_grams).count() as f64;
    intersection / smaller as f64
}

/// The near-duplicate test: Jaccard or containment above its threshold.
///
/// The OA paraphrase ("容器中心点击" → "容器中间就能点到") measured Jaccard
/// 0.48 / containment 0.65, so either metric alone misses real rewordings.
pub fn near_rejected_similarity(left: &str, right: &str) -> Option<f64> {
    let jaccard = jaccard_similarity(left, right);
    let containment = containment_similarity(left, right);
    let best = jaccard.max(containment);
    (best >= NEAR_REJECTED_THRESHOLD || containment >= NEAR_REJECTED_CONTAINMENT)
        .then_some(best)
}

/// `archive-record:<64 lowercase hex>` -> the sha, or `None`. This is the
/// gate-v1 parser itself, so the two shapes cannot drift.
pub fn parse_counterexample_ref(reference: &str) -> Option<String> {
    super::gate::parse_reference(reference)
}

fn ref_format(message: impl Into<String>) -> LearningError {
    LearningError::new(E_REF_FORMAT, LearningErrorKind::Usage, message)
}

fn ref_unresolved(message: impl Into<String>) -> LearningError {
    LearningError::new(E_REF_UNRESOLVED, LearningErrorKind::Integrity, message)
}

/// Read a `counterexample_ref` back from a committed archive and verify its
/// SHA-256. The reference shape is checked first (`E_REF_FORMAT`); an
/// unresolved record or a broken fingerprint is `E_REF_UNRESOLVED`.
pub fn verify_counterexample_ref(
    archive: &Archive,
    domain: &str,
    reference: &str,
) -> Result<bool, LearningError> {
    let Some(sha) = parse_counterexample_ref(reference) else {
        return Err(ref_format(format!(
            "--counterexample-ref must look like {REF_PREFIX}<64 lowercase hex>, got {reference:?}"
        )));
    };
    let records = archive.load_records().map_err(|error| {
        ref_unresolved(format!("committed archive state is unreadable: {error}"))
    })?;
    if !records.records.contains_key(&sha) {
        return Err(ref_unresolved(format!(
            "counterexample ref {reference:?} is not a committed archive record"
        )));
    }
    let bytes = archive
        .reader(domain)
        .read_record(&sha)
        .map_err(|error| ref_unresolved(format!("counterexample ref {reference:?}: {error}")))?;
    if hex_sha256(&bytes) != sha {
        return Err(ref_unresolved(format!(
            "counterexample ref {reference:?} failed its SHA-256 fingerprint check"
        )));
    }
    Ok(true)
}

// ---------------------------------------------------------------------------
// Rows and views
// ---------------------------------------------------------------------------

/// One `qualification_history` row.
#[derive(Debug, Clone, Serialize)]
pub struct QualificationRow {
    pub seq: i64,
    /// Global `event_clock` sequence (comparable with reuse/validation/task
    /// events; the per-table `seq` is the primary key only).
    pub event_seq: i64,
    pub candidate_id: String,
    pub revision: u64,
    pub state: String,
    pub basis: String,
    pub counterexample: String,
    pub counterexample_ref: String,
    pub owner: String,
    pub trigger: String,
    pub actor: String,
    pub recorded_at: String,
}

/// One `rejection_overrides` row.
#[derive(Debug, Clone, Serialize)]
pub struct RejectionOverride {
    pub seq: i64,
    pub event_seq: i64,
    pub new_candidate_id: String,
    pub matched_candidate_id: String,
    pub reason: String,
    pub actor: String,
    pub recorded_at: String,
}

/// The qualification material of one candidate, as carried by `show` and
/// `export`.
#[derive(Debug, Clone, Serialize)]
pub struct QualificationView {
    /// The newest state, or [`STATE_OPEN`] when there is no row.
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latest: Option<QualificationRow>,
    pub history: Vec<QualificationRow>,
    pub overrides: Vec<RejectionOverride>,
}

/// One row of `learning qualified`.
#[derive(Debug, Clone, Serialize)]
pub struct QualifiedEntry {
    pub candidate_id: String,
    pub claim: String,
    pub target_kind: String,
    pub revision: u64,
    pub state: String,
    pub basis: String,
    pub counterexample: String,
    pub counterexample_ref: String,
    pub owner: String,
    pub trigger: String,
    pub actor: String,
    pub recorded_at: String,
    pub history_count: usize,
}

/// `learning qualified` payload.
#[derive(Debug, Clone, Serialize)]
pub struct QualifiedReport {
    pub ok: bool,
    pub root: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    /// Entries matching the filters before `limit`.
    pub total: usize,
    /// Entries returned (after `limit`).
    pub count: usize,
    pub qualified: Vec<QualifiedEntry>,
}

/// One audit finding. `level` is `FAIL` (data a normal write cannot produce)
/// or `WARN` (needs a human re-review).
#[derive(Debug, Clone, Serialize)]
pub struct QualificationFinding {
    pub level: &'static str,
    pub code: &'static str,
    pub candidate_id: String,
    pub detail: String,
}

/// `learning qualified --audit` payload.
#[derive(Debug, Clone, Serialize)]
pub struct QualificationAudit {
    pub ok: bool,
    pub total_candidates: usize,
    pub total_rows: usize,
    /// Newest-state counts (`rejected` / `deferred` / `cleared`).
    pub counts: BTreeMap<String, usize>,
    pub deferred_overdue: usize,
    pub rejected_missing_basis: usize,
    pub rejected_missing_counterexample: usize,
    pub deferred_missing_owner: usize,
    pub deferred_missing_trigger: usize,
    pub cleared_conflicts: usize,
    pub recent_flips: usize,
    pub findings: Vec<QualificationFinding>,
}

/// The result of one `learning qualify` write.
#[derive(Debug, Clone, Serialize)]
pub struct QualifyOutcome {
    pub ok: bool,
    pub candidate_id: String,
    pub revision: u64,
    pub status: String,
    pub state: String,
    pub basis: String,
    pub counterexample: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub counterexample_ref: Option<String>,
    pub owner: String,
    pub trigger: String,
    pub actor: String,
    pub recorded_at: String,
    pub history_seq: i64,
    pub event_seq: i64,
    /// `true` only when `--archive --domain` was supplied and the ref was read
    /// back with a matching fingerprint.
    pub ref_verified: bool,
    /// `true` when the newest row already had the same state and basis.
    pub duplicate: bool,
    /// The state before this call (`open` when there was no row).
    pub prior_state: String,
    /// `cleared` by an actor different from the cleared `rejected` row's actor.
    pub cleared_conflict: bool,
}

/// One validated `learning qualify` invocation.
#[derive(Debug, Clone)]
pub struct QualifyRequest {
    pub candidate_id: String,
    pub state: String,
    pub actor: String,
    pub basis: String,
    pub counterexample: String,
    pub counterexample_ref: Option<String>,
    pub owner: String,
    pub trigger: String,
}

fn trimmed(value: Option<&str>) -> String {
    value.unwrap_or_default().trim().to_string()
}

impl QualifyRequest {
    /// Validate the required-field matrix and the optional ref shape. Nothing
    /// is read from or written to the store here.
    #[allow(clippy::too_many_arguments)]
    pub fn parse(
        candidate_id: &str,
        state: &str,
        actor: &str,
        basis: Option<&str>,
        counterexample: Option<&str>,
        counterexample_ref: Option<&str>,
        owner: Option<&str>,
        trigger: Option<&str>,
    ) -> Result<Self, LearningError> {
        let candidate_id = candidate_id.trim();
        if candidate_id.is_empty() {
            return Err(LearningError::usage(
                "--candidate-id must be a non-empty candidate id",
            ));
        }
        let state = state.trim();
        if !QUALIFY_STATES.contains(&state) {
            return Err(LearningError::usage(format!(
                "--state must be one of {QUALIFY_STATES:?}, got {state:?}"
            )));
        }
        let actor = store::require_actor(actor)?;
        let basis = trimmed(basis);
        let counterexample = trimmed(counterexample);
        let owner = trimmed(owner);
        let trigger = trimmed(trigger);
        let counterexample_ref = counterexample_ref
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string);
        if let Some(reference) = &counterexample_ref {
            if parse_counterexample_ref(reference).is_none() {
                return Err(ref_format(format!(
                    "--counterexample-ref must look like {REF_PREFIX}<64 lowercase hex>, got \
                     {reference:?}"
                )));
            }
        }

        match state {
            STATE_REJECTED => {
                if basis.is_empty() {
                    return Err(LearningError::usage(
                        "--basis is required for --state rejected: state what disproved the claim",
                    ));
                }
                if counterexample.is_empty() {
                    return Err(LearningError::usage(
                        "--counterexample is required for --state rejected: a rejection needs the \
                         concrete case that contradicts the claim",
                    ));
                }
            }
            STATE_DEFERRED => {
                if owner.is_empty() {
                    return Err(LearningError::usage(
                        "--owner is required for --state deferred: someone must own re-review",
                    ));
                }
                if trigger.is_empty() {
                    return Err(LearningError::usage(
                        "--trigger is required for --state deferred: name the evidence or event \
                         that should reopen the judgment",
                    ));
                }
            }
            STATE_CLEARED => {
                if basis.is_empty() {
                    return Err(LearningError::usage(
                        "--basis is required for --state cleared: state why the qualification is \
                         withdrawn",
                    ));
                }
            }
            _ => unreachable!("state validated above"),
        }

        Ok(Self {
            candidate_id: candidate_id.to_string(),
            state: state.to_string(),
            actor,
            basis,
            counterexample,
            counterexample_ref,
            owner,
            trigger,
        })
    }
}

// ---------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------

fn row_from(row: &rusqlite::Row<'_>) -> rusqlite::Result<QualificationRow> {
    Ok(QualificationRow {
        seq: row.get(0)?,
        event_seq: row.get(1)?,
        candidate_id: row.get(2)?,
        revision: row.get::<_, i64>(3)? as u64,
        state: row.get(4)?,
        basis: row.get(5)?,
        counterexample: row.get(6)?,
        counterexample_ref: row.get(7)?,
        owner: row.get(8)?,
        trigger: row.get(9)?,
        actor: row.get(10)?,
        recorded_at: row.get(11)?,
    })
}

fn qualification_rows(
    conn: &Connection,
    candidate_id: &str,
) -> Result<Vec<QualificationRow>, LearningError> {
    let mut stmt = conn
        .prepare(
            "SELECT seq, event_seq, candidate_id, revision, state, basis, counterexample, \
             counterexample_ref, owner, trigger, actor, recorded_at \
             FROM qualification_history WHERE candidate_id = ?1 ORDER BY seq",
        )
        .map_err(store::db_error)?;
    let rows = stmt
        .query_map(params![candidate_id], row_from)
        .map_err(store::db_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(store::db_error)?;
    Ok(rows)
}

fn all_qualification_rows(conn: &Connection) -> Result<Vec<QualificationRow>, LearningError> {
    let mut stmt = conn
        .prepare(
            "SELECT seq, event_seq, candidate_id, revision, state, basis, counterexample, \
             counterexample_ref, owner, trigger, actor, recorded_at \
             FROM qualification_history ORDER BY candidate_id, seq",
        )
        .map_err(store::db_error)?;
    let rows = stmt
        .query_map([], row_from)
        .map_err(store::db_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(store::db_error)?;
    Ok(rows)
}

fn overrides_for(
    conn: &Connection,
    new_candidate_id: &str,
) -> Result<Vec<RejectionOverride>, LearningError> {
    let mut stmt = conn
        .prepare(
            "SELECT seq, event_seq, new_candidate_id, matched_candidate_id, reason, actor, \
             recorded_at FROM rejection_overrides WHERE new_candidate_id = ?1 ORDER BY seq",
        )
        .map_err(store::db_error)?;
    let rows = stmt
        .query_map(params![new_candidate_id], |row| {
            Ok(RejectionOverride {
                seq: row.get(0)?,
                event_seq: row.get(1)?,
                new_candidate_id: row.get(2)?,
                matched_candidate_id: row.get(3)?,
                reason: row.get(4)?,
                actor: row.get(5)?,
                recorded_at: row.get(6)?,
            })
        })
        .map_err(store::db_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(store::db_error)?;
    Ok(rows)
}

/// The view carried by `show` / `export`.
pub(crate) fn load_view(
    conn: &Connection,
    candidate_id: &str,
) -> Result<QualificationView, LearningError> {
    let history = qualification_rows(conn, candidate_id)?;
    let state = history
        .last()
        .map(|row| row.state.clone())
        .unwrap_or_else(|| STATE_OPEN.to_string());
    let latest = history.last().cloned();
    Ok(QualificationView {
        state,
        latest,
        history,
        overrides: overrides_for(conn, candidate_id)?,
    })
}

// ---------------------------------------------------------------------------
// Write
// ---------------------------------------------------------------------------

impl LearningStore {
    /// Append one qualification row (see the module docs).
    ///
    /// `ref_verified` is computed by the caller: `true` only when an explicit
    /// archive/domain pair was supplied and the ref passed readback +
    /// fingerprint checking.
    pub fn qualify(
        &mut self,
        request: &QualifyRequest,
        ref_verified: bool,
    ) -> Result<QualifyOutcome, LearningError> {
        let tx = self
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(store::db_error)?;
        let outcome = qualify_in_transaction(&tx, request, ref_verified)?;
        tx.commit().map_err(store::db_error)?;
        Ok(outcome)
    }
}

fn qualify_in_transaction(
    tx: &Transaction<'_>,
    request: &QualifyRequest,
    ref_verified: bool,
) -> Result<QualifyOutcome, LearningError> {
    let Some((record, _)) = store::load_current(tx, &request.candidate_id)? else {
        return Err(LearningError::not_found(format!(
            "no candidate {} in this learning root",
            request.candidate_id
        )));
    };
    if record.target_kind == TASK_TARGET_KIND {
        return Err(LearningError::new(
            TASK_NOT_QUALIFIABLE,
            LearningErrorKind::Usage,
            format!(
                "candidate {} is a workspace task; tasks are withdrawn with `learning task \
                 abandon`, never qualified",
                record.candidate_id
            ),
        ));
    }

    let history = qualification_rows(tx, &record.candidate_id)?;
    let latest = history.last();
    if request.state == STATE_CLEARED && latest.is_none() {
        return Err(LearningError::new(
            NOTHING_TO_CLEAR,
            LearningErrorKind::Usage,
            format!(
                "candidate {} has no qualification row; there is nothing to clear",
                record.candidate_id
            ),
        ));
    }

    if let Some(latest) = latest {
        let same_deferral = request.state != STATE_DEFERRED
            || (latest.owner == request.owner && latest.trigger == request.trigger);
        if latest.state == request.state && latest.basis == request.basis && same_deferral {
            let prior_state = history
                .get(history.len().saturating_sub(2))
                .map(|row| row.state.clone())
                .unwrap_or_else(|| STATE_OPEN.to_string());
            let cleared_conflict = latest.state == STATE_CLEARED
                && history
                    .get(history.len().saturating_sub(2))
                    .map(|prior| {
                        prior.state == STATE_REJECTED
                            && super::routing::review_identity(&prior.actor)
                                != super::routing::review_identity(&latest.actor)
                    })
                    .unwrap_or(false);
            return Ok(QualifyOutcome {
                ok: true,
                candidate_id: record.candidate_id,
                revision: latest.revision,
                status: record.status,
                state: latest.state.clone(),
                basis: latest.basis.clone(),
                counterexample: latest.counterexample.clone(),
                counterexample_ref: if latest.counterexample_ref.is_empty() {
                    None
                } else {
                    Some(latest.counterexample_ref.clone())
                },
                owner: latest.owner.clone(),
                trigger: latest.trigger.clone(),
                actor: latest.actor.clone(),
                recorded_at: latest.recorded_at.clone(),
                history_seq: latest.seq,
                event_seq: latest.event_seq,
                ref_verified,
                duplicate: true,
                prior_state,
                cleared_conflict,
            });
        }
    }

    let prior_state = latest
        .map(|row| row.state.clone())
        .unwrap_or_else(|| STATE_OPEN.to_string());
    let cleared_conflict = request.state == STATE_CLEARED
        && latest
            .map(|prior| {
                prior.state == STATE_REJECTED
                    && super::routing::review_identity(&prior.actor)
                        != super::routing::review_identity(&request.actor)
            })
            .unwrap_or(false);

    let recorded_at = store::storage_now();
    let event_seq = store::allocate_event_seq(tx)?;
    tx.execute(
        "INSERT INTO qualification_history \
         (event_seq, candidate_id, revision, state, basis, counterexample, counterexample_ref, \
          owner, trigger, actor, recorded_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            event_seq,
            record.candidate_id,
            record.revision as i64,
            request.state,
            request.basis,
            request.counterexample,
            request.counterexample_ref.as_deref().unwrap_or_default(),
            request.owner,
            request.trigger,
            request.actor,
            recorded_at
        ],
    )
    .map_err(store::db_error)?;
    let history_seq = tx.last_insert_rowid();

    Ok(QualifyOutcome {
        ok: true,
        candidate_id: record.candidate_id,
        revision: record.revision,
        status: record.status,
        state: request.state.clone(),
        basis: request.basis.clone(),
        counterexample: request.counterexample.clone(),
        counterexample_ref: request.counterexample_ref.clone(),
        owner: request.owner.clone(),
        trigger: request.trigger.clone(),
        actor: request.actor.clone(),
        recorded_at,
        history_seq,
        event_seq,
        ref_verified,
        duplicate: false,
        prior_state,
        cleared_conflict,
    })
}

// ---------------------------------------------------------------------------
// Read: `learning qualified`
// ---------------------------------------------------------------------------

impl LearningStore {
    /// List candidates that currently carry a qualification, newest first.
    /// Strictly read-only.
    pub fn qualified(
        &self,
        state: Option<&str>,
        query: Option<&str>,
        limit: Option<usize>,
    ) -> Result<QualifiedReport, LearningError> {
        let state = state.map(str::trim).filter(|value| !value.is_empty());
        if let Some(state) = state {
            if !QUALIFY_STATES.contains(&state) {
                return Err(LearningError::usage(format!(
                    "--state must be one of {QUALIFY_STATES:?}, got {state:?}"
                )));
            }
        }
        let query = query.map(str::trim).filter(|value| !value.is_empty());
        let normalized_query = query.map(normalize_for_rejection);

        let rows = all_qualification_rows(self.connection())?;
        // Full history per candidate. `rows` is ordered by `(candidate_id,
        // seq)`, so the last row of each group is the newest; `--query` still
        // searches every historical basis/counterexample (a withdrawn
        // rejection stays findable).
        let mut histories: BTreeMap<String, Vec<QualificationRow>> = BTreeMap::new();
        for row in rows {
            histories
                .entry(row.candidate_id.clone())
                .or_default()
                .push(row);
        }

        let mut stmt = self
            .connection()
            .prepare("SELECT candidate_id, record_json FROM candidates ORDER BY candidate_id")
            .map_err(store::db_error)?;
        let candidates = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(store::db_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(store::db_error)?;

        let mut entries = Vec::new();
        for (candidate_id, record_json) in candidates {
            let Some(history) = histories.remove(&candidate_id) else {
                continue;
            };
            let Some(row) = history.last().cloned() else {
                continue;
            };
            let history_count = history.len();
            let record = store::parse_record(&record_json)?;
            if record.target_kind == TASK_TARGET_KIND {
                continue;
            }
            if let Some(state) = state {
                if row.state != state {
                    continue;
                }
            }
            if let Some(needle) = &normalized_query {
                let matches_claim = normalize_for_rejection(&record.claim).contains(needle);
                let matches_history = history.iter().any(|entry| {
                    normalize_for_rejection(&entry.basis).contains(needle)
                        || normalize_for_rejection(&entry.counterexample).contains(needle)
                });
                if !matches_claim && !matches_history {
                    continue;
                }
            }
            entries.push(QualifiedEntry {
                candidate_id: record.candidate_id,
                claim: record.claim,
                target_kind: record.target_kind,
                revision: row.revision,
                state: row.state,
                basis: row.basis,
                counterexample: row.counterexample,
                counterexample_ref: row.counterexample_ref,
                owner: row.owner,
                trigger: row.trigger,
                actor: row.actor,
                recorded_at: row.recorded_at,
                history_count,
            });
        }
        entries.sort_by(|left, right| {
            right
                .recorded_at
                .cmp(&left.recorded_at)
                .then_with(|| left.candidate_id.cmp(&right.candidate_id))
        });

        let total = entries.len();
        let entries = match limit {
            Some(limit) => entries.into_iter().take(limit).collect(),
            None => entries,
        };
        Ok(QualifiedReport {
            ok: true,
            root: self.root().to_string_lossy().into_owned(),
            state: state.map(str::to_string),
            query: query.map(str::to_string),
            total,
            count: entries.len(),
            qualified: entries,
        })
    }

    /// Grade the qualification store for 90 `AM-QUAL` (read-only).
    ///
    /// FAIL: a `rejected` history row without basis/counterexample or a
    /// `deferred` history row without owner/trigger. A normal write path cannot
    /// produce those, so they indicate a direct database edit.
    /// WARN: a `deferred` newest row older than 30 days, a `cleared` row by a
    /// different actor than the `rejected` row it withdraws, or at least three
    /// `rejected`/`cleared` flips in the last 7 days.
    pub fn qualification_audit(
        &self,
        now: DateTime<FixedOffset>,
    ) -> Result<QualificationAudit, LearningError> {
        let rows = all_qualification_rows(self.connection())?;
        let mut by_candidate: BTreeMap<String, Vec<QualificationRow>> = BTreeMap::new();
        for row in &rows {
            by_candidate
                .entry(row.candidate_id.clone())
                .or_default()
                .push(row.clone());
        }

        let mut counts: BTreeMap<String, usize> = BTreeMap::new();
        for history in by_candidate.values() {
            if let Some(latest) = history.last() {
                *counts.entry(latest.state.clone()).or_insert(0) += 1;
            }
        }

        let mut findings = Vec::new();
        let mut deferred_overdue = 0usize;
        let mut rejected_missing_basis = 0usize;
        let mut rejected_missing_counterexample = 0usize;
        let mut deferred_missing_owner = 0usize;
        let mut deferred_missing_trigger = 0usize;
        let mut cleared_conflicts = 0usize;
        let mut recent_flips = 0usize;

        for (candidate_id, history) in &by_candidate {
            for row in history {
                if row.state == STATE_REJECTED {
                    if row.basis.trim().is_empty() {
                        rejected_missing_basis += 1;
                        findings.push(QualificationFinding {
                            level: "FAIL",
                            code: "QUAL_REJECTED_BASIS",
                            candidate_id: candidate_id.clone(),
                            detail: format!("rejected history row seq={} has no basis", row.seq),
                        });
                    }
                    if row.counterexample.trim().is_empty() {
                        rejected_missing_counterexample += 1;
                        findings.push(QualificationFinding {
                            level: "FAIL",
                            code: "QUAL_REJECTED_COUNTEREXAMPLE",
                            candidate_id: candidate_id.clone(),
                            detail: format!(
                                "rejected history row seq={} has no counterexample",
                                row.seq
                            ),
                        });
                    }
                }
                if row.state == STATE_DEFERRED {
                    if row.owner.trim().is_empty() {
                        deferred_missing_owner += 1;
                        findings.push(QualificationFinding {
                            level: "FAIL",
                            code: "QUAL_DEFERRED_OWNER",
                            candidate_id: candidate_id.clone(),
                            detail: format!("deferred history row seq={} has no owner", row.seq),
                        });
                    }
                    if row.trigger.trim().is_empty() {
                        deferred_missing_trigger += 1;
                        findings.push(QualificationFinding {
                            level: "FAIL",
                            code: "QUAL_DEFERRED_TRIGGER",
                            candidate_id: candidate_id.clone(),
                            detail: format!("deferred history row seq={} has no trigger", row.seq),
                        });
                    }
                }
            }

            if let Some(latest) = history.last() {
                if latest.state == STATE_DEFERRED {
                    if let Ok(recorded) = DateTime::parse_from_rfc3339(&latest.recorded_at) {
                        if recorded < now - ChronoDuration::days(DEFERRED_REVIEW_DAYS) {
                            deferred_overdue += 1;
                            findings.push(QualificationFinding {
                                level: "WARN",
                                code: "QUAL_DEFERRED_OVERDUE",
                                candidate_id: candidate_id.clone(),
                                detail: format!(
                                    "deferred since {} (> {DEFERRED_REVIEW_DAYS} days); owner \
                                     {} trigger {}",
                                    latest.recorded_at, latest.owner, latest.trigger
                                ),
                            });
                        }
                    }
                }
            }

            for index in 1..history.len() {
                let prior = &history[index - 1];
                let current = &history[index];
                if current.state == STATE_CLEARED
                    && prior.state == STATE_REJECTED
                    && super::routing::review_identity(&prior.actor)
                        != super::routing::review_identity(&current.actor)
                {
                    cleared_conflicts += 1;
                    findings.push(QualificationFinding {
                        level: "WARN",
                        code: "QUAL_CLEARED_CONFLICT",
                        candidate_id: candidate_id.clone(),
                        detail: format!(
                            "cleared by {:?} withdraws a rejection by {:?}",
                            current.actor, prior.actor
                        ),
                    });
                }
            }

            let flip_start = now - ChronoDuration::days(FLIP_WINDOW_DAYS);
            let mut flips = 0usize;
            for index in 1..history.len() {
                let prior = &history[index - 1];
                let current = &history[index];
                let Ok(recorded) = DateTime::parse_from_rfc3339(&current.recorded_at) else {
                    continue;
                };
                if recorded <= flip_start {
                    continue;
                }
                let pair = (prior.state.as_str(), current.state.as_str());
                if matches!(
                    pair,
                    (STATE_REJECTED, STATE_CLEARED) | (STATE_CLEARED, STATE_REJECTED)
                ) {
                    flips += 1;
                }
            }
            if flips >= FLIP_MIN {
                recent_flips += 1;
                findings.push(QualificationFinding {
                    level: "WARN",
                    code: "QUAL_FLIP_BURST",
                    candidate_id: candidate_id.clone(),
                    detail: format!(
                        "{flips} rejected/cleared flip(s) in the last {FLIP_WINDOW_DAYS} days"
                    ),
                });
            }
        }

        findings.sort_by(|left, right| {
            left.code
                .cmp(right.code)
                .then_with(|| left.candidate_id.cmp(&right.candidate_id))
        });
        let ok = findings.iter().all(|finding| finding.level != "FAIL");
        Ok(QualificationAudit {
            ok,
            total_candidates: by_candidate.len(),
            total_rows: rows.len(),
            counts,
            deferred_overdue,
            rejected_missing_basis,
            rejected_missing_counterexample,
            deferred_missing_owner,
            deferred_missing_trigger,
            cleared_conflicts,
            recent_flips,
            findings,
        })
    }
}

/// The text form of `learning qualified`: one line per entry, then a summary.
pub fn render_qualified_text(report: &QualifiedReport) -> String {
    let mut text = String::new();
    for entry in &report.qualified {
        text.push_str(&format!(
            "{} {} claim={}",
            entry.state, entry.candidate_id, entry.claim
        ));
        if !entry.basis.is_empty() {
            text.push_str(&format!(" basis={}", entry.basis));
        }
        if !entry.counterexample.is_empty() {
            text.push_str(&format!(" counterexample={}", entry.counterexample));
        }
        if !entry.owner.is_empty() || !entry.trigger.is_empty() {
            text.push_str(&format!(" owner={} trigger={}", entry.owner, entry.trigger));
        }
        text.push('\n');
    }
    text.push_str(&format!(
        "qualified: {} shown, {} matching, {} candidate(s) with a qualification\n",
        report.count,
        report.total,
        report.total
    ));
    text
}

/// The text form of `learning qualified --audit`.
pub fn render_qualification_audit_text(audit: &QualificationAudit) -> String {
    let mut text = String::new();
    for finding in &audit.findings {
        text.push_str(&format!(
            "{} {} {} {}\n",
            finding.level, finding.code, finding.candidate_id, finding.detail
        ));
    }
    let counts = if audit.counts.is_empty() {
        "none".to_string()
    } else {
        audit
            .counts
            .iter()
            .map(|(state, count)| format!("{state}={count}"))
            .collect::<Vec<_>>()
            .join(",")
    };
    text.push_str(&format!(
        "qualification audit: {} (rejected={} deferred={} cleared={} rows={} overdue={} flips={} \
         cleared_conflicts={})\n",
        if audit.ok { "PASS" } else { "FAIL" },
        audit.counts.get(STATE_REJECTED).copied().unwrap_or(0),
        audit.counts.get(STATE_DEFERRED).copied().unwrap_or(0),
        audit.counts.get(STATE_CLEARED).copied().unwrap_or(0),
        audit.total_rows,
        audit.deferred_overdue,
        audit.recent_flips,
        audit.cleared_conflicts,
    ));
    text.push_str(&format!("counts {counts}\n"));
    text
}

// ---------------------------------------------------------------------------
// Anti-rebirth (called by `learning ingest`)
// ---------------------------------------------------------------------------

/// One rejected claim that an ingest is compared against.
#[derive(Debug, Clone, Serialize)]
pub struct RejectedMatch {
    pub candidate_id: String,
    pub claim: String,
    /// Best near-duplicate similarity: the larger of character 3-gram Jaccard
    /// and containment (1.0 for an exact normalized match).
    pub similarity: f64,
    pub state: String,
}

/// The rejection picture of one incoming ingest.
#[derive(Debug, Clone, Default)]
pub(crate) struct RejectionCheck {
    /// Exact matches that block (or require an override).
    pub blocked: Vec<(String, String)>,
    /// Near matches that warn only.
    pub near: Vec<RejectedMatch>,
}

/// Compare the incoming record against the rejected candidates of the store.
/// Read-only; the caller decides whether to refuse or to write an override.
pub(crate) fn check_rejection(
    conn: &Connection,
    incoming: &super::model::CandidateRecord,
) -> Result<RejectionCheck, LearningError> {
    if incoming.target_kind == TASK_TARGET_KIND {
        return Ok(RejectionCheck::default());
    }
    let incoming_normalized = normalize_for_rejection(&incoming.claim);
    let latest = latest_states(conn)?;
    let mut check = RejectionCheck::default();

    let mut stmt = conn
        .prepare("SELECT candidate_id, record_json FROM candidates ORDER BY candidate_id")
        .map_err(store::db_error)?;
    let rows = stmt
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(store::db_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(store::db_error)?;

    for (candidate_id, record_json) in rows {
        let Some(state) = latest.get(&candidate_id) else {
            continue;
        };
        if state != STATE_REJECTED {
            continue;
        }
        let record = store::parse_record(&record_json)?;
        if record.target_kind == TASK_TARGET_KIND {
            continue;
        }
        let normalized = normalize_for_rejection(&record.claim);
        if candidate_id == incoming.candidate_id || normalized == incoming_normalized {
            check
                .blocked
                .push((record.candidate_id.clone(), record.claim.clone()));
            continue;
        }
        let similarity = near_rejected_similarity(&incoming_normalized, &normalized);
        if let Some(similarity) = similarity {
            check.near.push(RejectedMatch {
                candidate_id: record.candidate_id,
                claim: record.claim,
                similarity,
                state: state.clone(),
            });
        }
    }
    check
        .blocked
        .sort_by(|left, right| left.0.cmp(&right.0));
    check.near.sort_by(|left, right| {
        right
            .similarity
            .partial_cmp(&left.similarity)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| left.candidate_id.cmp(&right.candidate_id))
    });
    Ok(check)
}

fn latest_states(conn: &Connection) -> Result<BTreeMap<String, String>, LearningError> {
    let mut latest = BTreeMap::new();
    for row in all_qualification_rows(conn)? {
        latest.insert(row.candidate_id.clone(), row.state);
    }
    Ok(latest)
}

/// Write one `rejection_overrides` row inside the caller's transaction.
pub(crate) fn record_override(
    tx: &Transaction<'_>,
    new_candidate_id: &str,
    matched_candidate_id: &str,
    reason: &str,
    actor: &str,
) -> Result<RejectionOverride, LearningError> {
    let recorded_at = store::storage_now();
    let event_seq = store::allocate_event_seq(tx)?;
    tx.execute(
        "INSERT INTO rejection_overrides \
         (event_seq, new_candidate_id, matched_candidate_id, reason, actor, recorded_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            event_seq,
            new_candidate_id,
            matched_candidate_id,
            reason,
            actor,
            recorded_at
        ],
    )
    .map_err(store::db_error)?;
    let seq = tx.last_insert_rowid();
    Ok(RejectionOverride {
        seq,
        event_seq,
        new_candidate_id: new_candidate_id.to_string(),
        matched_candidate_id: matched_candidate_id.to_string(),
        reason: reason.to_string(),
        actor: actor.to_string(),
        recorded_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalization_folds_nfkc_case_space_and_punctuation() {
        assert_eq!(
            normalize_for_rejection("  Ｒｕｌｅ：  Always   Use  Explicit-Roots！ "),
            "rulealwaysuseexplicitroots"
        );
        assert_eq!(
            normalize_for_rejection("规则：始终使用显式根！"),
            "规则始终使用显式根"
        );
        assert_eq!(normalize_for_rejection(""), "");
    }

    #[test]
    fn trigram_jaccard_matches_near_claims_only() {
        let left = normalize_for_rejection("Always use explicit learning roots");
        let right = normalize_for_rejection("Always use explicit learning roots!");
        assert_eq!(jaccard_similarity(&left, &right), 1.0);
        let near = normalize_for_rejection("Always use explicit learning root");
        assert!(jaccard_similarity(&left, &near) >= NEAR_REJECTED_THRESHOLD);
        let far = normalize_for_rejection("Archive capture uses one writer");
        assert!(jaccard_similarity(&left, &far) < NEAR_REJECTED_THRESHOLD);
        assert_eq!(jaccard_similarity("", ""), 1.0);
    }

    #[test]
    fn containment_catches_the_oa_paraphrase_jaccard_missed() {
        // The deployment machine's F4 probe: reworded claim, same false claim.
        let left = normalize_for_rejection("点击 .wea-transfer-opration 容器中心即可命中转移箭头");
        let right = normalize_for_rejection("点 .wea-transfer-opration 的容器中间就能点到转移箭头");
        assert!(jaccard_similarity(&left, &right) < NEAR_REJECTED_THRESHOLD);
        assert!(containment_similarity(&left, &right) >= NEAR_REJECTED_CONTAINMENT);
        assert!(near_rejected_similarity(&left, &right).is_some());

        // Related but genuinely different claims must not warn.
        let other = normalize_for_rejection("转移列表的滚动条要拖到底部才能看到全部条目");
        assert!(near_rejected_similarity(&left, &other).is_none());

        // Exact and empty cases keep the Jaccard conventions.
        assert_eq!(containment_similarity(&left, &left), 1.0);
        assert_eq!(containment_similarity("", ""), 1.0);
        assert_eq!(containment_similarity("ab", ""), 0.0);
    }

    #[test]
    fn counterexample_ref_shape_is_gate_v1() {
        let sha = "a".repeat(64);
        assert_eq!(
            parse_counterexample_ref(&format!("{REF_PREFIX}{sha}")),
            Some(sha.clone())
        );
        assert_eq!(parse_counterexample_ref("claude:session:line-1"), None);
        assert_eq!(
            parse_counterexample_ref(&format!("{REF_PREFIX}{}", sha.to_uppercase())),
            None
        );
        assert_eq!(
            parse_counterexample_ref(&format!("{REF_PREFIX}{}", "a".repeat(63))),
            None
        );
    }
}
