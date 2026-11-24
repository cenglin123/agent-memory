//! Stage 4: workspace tasks — the write path for `task` candidates and the
//! read-side projection core.
//!
//! A task is a first-class candidate (`target_kind = "task"`) that never leaves
//! `status = candidate`. Its *state* is derived, never stored:
//!
//! - `task_events` (`claim` / `abandon`) carries intent;
//! - `reuse_history` carries "this candidate was applied to this task"
//!   progress, both for the task itself and for knowledge candidates whose
//!   `task_id` points at it;
//! - `validation_history` carries independent review verdicts.
//!
//! [`project_task_state`] folds one task's unified event view (all three
//! tables, ordered by the global `event_seq`) into `active / done / abandoned /
//! stale`. It is a pure function of `(events, now, stale_days)` so the
//! workspace surface and its tests agree bit for bit.
//!
//! # Write path
//!
//! - `task create` is one `BEGIN IMMEDIATE` transaction that first loads the
//!   content-addressed candidate id: a hit returns `created:false,
//!   duplicate:true` and writes **nothing** (not even an ingestion row).
//!   Otherwise it ingests the task payload (`extractor = "task"`,
//!   `source_checkpoint = "task:<candidate_id>"`, revision 1) and appends the
//!   first `claim` event, whose actor is the creator.
//! - `task claim` / `task abandon` append one `task_events` row; both refuse a
//!   non-task candidate with [`NOT_A_TASK`] (usage class).
//!
//! Task candidates are only created by `task create`; `learning ingest`
//! refuses a task payload with [`TASK_REQUIRES_TASK_CREATE`], so no task can
//! exist without its first claim event.

use chrono::{DateTime, Duration as ChronoDuration, FixedOffset};
use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;

use super::inject::INJECT_NOTICE;
use super::model::{
    candidate_id_for, validate_candidate, validate_derived_from, validate_task_claim,
    CandidateRecord, TASK_PROPOSED_TARGET, TASK_TARGET_KIND,
};
use super::store::{self, LearningStore};
use super::{LearningError, LearningErrorKind};

/// `task claim` / `task abandon` were given a candidate that is not a task.
pub const NOT_A_TASK: &str = "NOT_A_TASK";
/// `learning ingest` was given a task payload; tasks are only created by
/// `learning task create` so that every task has a first claim event.
pub const TASK_REQUIRES_TASK_CREATE: &str = "TASK_REQUIRES_TASK_CREATE";
/// A task candidate is never routed to a knowledge target. It stays a
/// candidate, has no review package and no mechanical ready path, so `gate`,
/// `dedup`, `gate-target` and `export` all refuse it with this one code.
pub const TASK_NOT_ROUTEABLE: &str = "TASK_NOT_ROUTEABLE";
/// Default TTL for the `stale` projection (plan §5: 30 days).
pub const DEFAULT_STALE_DAYS: u32 = 30;

/// Derived task states (R9 — no fifth "parked" state).
pub const TASK_STATE_ACTIVE: &str = "active";
pub const TASK_STATE_DONE: &str = "done";
pub const TASK_STATE_ABANDONED: &str = "abandoned";
pub const TASK_STATE_STALE: &str = "stale";

/// Event kinds of the unified view.
pub const TASK_EVENT_CLAIM: &str = "claim";
pub const TASK_EVENT_ABANDON: &str = "abandon";
pub const TASK_EVENT_REUSE: &str = "reuse";
pub const TASK_EVENT_VALIDATION: &str = "validation";

/// `task create` payload. `state` is the derived projection right after the
/// write (a fresh task is always `active`); a duplicate create reports the
/// current projection instead.
#[derive(Debug, Clone, Serialize)]
pub struct TaskCreateOutcome {
    pub ok: bool,
    pub created: bool,
    pub duplicate: bool,
    pub candidate_id: String,
    pub claim: String,
    pub actor: String,
    pub state: String,
}

/// `task claim` / `task abandon` payload.
#[derive(Debug, Clone, Serialize)]
pub struct TaskEventOutcome {
    pub ok: bool,
    pub candidate_id: String,
    pub event_kind: String,
    pub actor: String,
    pub note: String,
    pub recorded_at: String,
    pub event_seq: i64,
}

/// One event of a task's unified view. `source` names the table
/// (`task` / `reuse` / `validation`); `kind` is the event kind as stored
/// (`claim` / `abandon` / `reuse` / `validation`, or an unknown kind written by
/// hand, which the projection treats as an ordinary event).
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct TaskEventView {
    pub source: String,
    pub kind: String,
    pub event_seq: i64,
    pub recorded_at: String,
    pub actor: String,
    /// `outcome` for reuse, `verdict` for validation, empty otherwise.
    pub outcome: String,
    /// `note` for task events, `notes` for reuse, `report_ref` for validation.
    pub note: String,
}

/// The newest event of a task's unified view (`last_event.seq` is `event_seq`).
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct TaskEventSummary {
    pub kind: String,
    pub actor: String,
    pub event_seq: i64,
    pub recorded_at: String,
}

/// The newest validation of a task's own validation stream.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct TaskValidationSummary {
    pub reviewer: String,
    pub verdict: String,
    pub event_seq: i64,
    pub recorded_at: String,
}

/// Result of [`project_task_state`].
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct TaskProjection {
    pub state: String,
    pub owner: Option<String>,
    pub created_by: Option<String>,
    pub last_event: Option<TaskEventSummary>,
    pub last_validation: Option<TaskValidationSummary>,
    pub reuse_total: usize,
    pub event_total: usize,
}

/// One row of `learning workspace` (plan §5).
#[derive(Debug, Clone, Serialize)]
pub struct WorkspaceTaskRow {
    pub candidate_id: String,
    pub claim: String,
    pub state: String,
    pub created_by: Option<String>,
    pub owner: Option<String>,
    pub last_event: Option<TaskEventSummary>,
    pub last_validation: Option<TaskValidationSummary>,
    pub reuse_total: usize,
    pub event_total: usize,
    /// Stage 6 §D read side: the ids of the knowledge candidates that name this
    /// task in their `origin_tasks`, sorted lexicographically. Empty is the
    /// normal case; a `done` task with no distillation is not an anomaly.
    pub distilled_into: Vec<String>,
    /// Stage 6 §C: the newest validation was written by the creator or the
    /// current owner (normalized). Derived, never stored; `false` without a
    /// validation.
    pub self_validated: bool,
}

/// `learning workspace` payload. `now` is the projection reference time (the
/// explicit `--now` or the wall clock); `generated_at` is always the wall clock.
#[derive(Debug, Clone, Serialize)]
pub struct WorkspaceReport {
    pub ok: bool,
    pub root: String,
    /// The stage 5 positioning statement (`stage 5 §B` baseline text): the
    /// same single constant `inject` renders. Additive JSON field.
    pub header: String,
    pub generated_at: String,
    pub stale_days: u32,
    pub now: String,
    pub total: usize,
    pub tasks: Vec<WorkspaceTaskRow>,
}

/// `learning task show` payload: the candidate summary, its projection and the
/// whole unified event view ordered by `event_seq`.
#[derive(Debug, Clone, Serialize)]
pub struct TaskShowReport {
    pub ok: bool,
    pub candidate_id: String,
    pub claim: String,
    pub target_kind: String,
    /// Stage 5 §A `derived_from` (task only). `None` when the field was absent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub derived_from: Option<String>,
    pub state: String,
    pub created_by: Option<String>,
    pub owner: Option<String>,
    pub last_event: Option<TaskEventSummary>,
    pub last_validation: Option<TaskValidationSummary>,
    pub reuse_total: usize,
    pub event_total: usize,
    pub events: Vec<TaskEventView>,
    /// Stage 6 §D read side: knowledge candidates that name this task in their
    /// `origin_tasks`, sorted lexicographically.
    pub distilled_into: Vec<String>,
    /// Stage 6 §C: the newest validation came from the creator or the current
    /// owner (normalized).
    pub self_validated: bool,
    /// The stage 5 positioning statement (`stage 5 §B` baseline text): the
    /// same single constant `inject` renders. Additive JSON field.
    pub header: String,
}

/// The task's unified event view: `task_events(candidate_id=T)` ∪
/// `reuse_history(candidate_id=T or task_id=T)` ∪
/// `validation_history(candidate_id=T)`, ordered by the global `event_seq`.
pub(crate) fn load_task_events(
    conn: &Connection,
    candidate_id: &str,
) -> Result<Vec<TaskEventView>, LearningError> {
    let mut stmt = conn
        .prepare(
            "SELECT source, kind, event_seq, recorded_at, actor, outcome, note FROM ( \
                 SELECT 'task' AS source, event_kind AS kind, event_seq, recorded_at, \
                        actor_id AS actor, '' AS outcome, note \
                 FROM task_events WHERE candidate_id = ?1 \
                 UNION ALL \
                 SELECT 'reuse' AS source, 'reuse' AS kind, event_seq, recorded_at, \
                        actor_id AS actor, outcome, notes AS note \
                 FROM reuse_history WHERE candidate_id = ?1 OR task_id = ?1 \
                 UNION ALL \
                 SELECT 'validation' AS source, 'validation' AS kind, event_seq, recorded_at, \
                        reviewer AS actor, verdict AS outcome, report_ref AS note \
                 FROM validation_history WHERE candidate_id = ?1 \
             ) ORDER BY event_seq",
        )
        .map_err(store::db_error)?;
    let events = stmt
        .query_map(params![candidate_id], |row| {
            Ok(TaskEventView {
                source: row.get(0)?,
                kind: row.get(1)?,
                event_seq: row.get(2)?,
                recorded_at: row.get(3)?,
                actor: row.get(4)?,
                outcome: row.get(5)?,
                note: row.get(6)?,
            })
        })
        .map_err(store::db_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(store::db_error)?;
    Ok(events)
}

fn parse_event_time(event: &TaskEventView) -> Option<DateTime<FixedOffset>> {
    // Stored timestamps are validated on write; a hand-edited value is skipped
    // for TTL purposes rather than making the pure projection fallible.
    DateTime::parse_from_rfc3339(&event.recorded_at).ok()
}

/// Fold one task's unified event view into its derived state (plan §5).
///
/// Rules, in order:
///
/// 1. the newest event is an `abandon` → `abandoned`;
/// 2. otherwise the newest `validation` decides: `pass` → `done`, anything
///    else → `active`;
/// 3. otherwise → `active` (a task always has at least its first `claim`);
/// 4. an `active` result whose newest recorded event is older than
///    `stale_days` (measured against `now`) → `stale`; `done` and `abandoned`
///    never decay, and `claim`/`reuse` never lift `done` (only a later `fail`
///    verdict does);
/// 5. unknown event kinds (only reachable through a hand-written database)
///    participate as ordinary non-abandon events.
///
/// `owner` is the newest `claim` actor, `created_by` the first one; `reuse`
/// events never move ownership.
pub fn project_task_state(
    events: &[TaskEventView],
    now: DateTime<FixedOffset>,
    stale_days: u32,
) -> TaskProjection {
    let mut ordered: Vec<&TaskEventView> = events.iter().collect();
    ordered.sort_by_key(|event| event.event_seq);

    let mut owner: Option<String> = None;
    let mut created_by: Option<String> = None;
    let mut last_validation: Option<TaskValidationSummary> = None;
    let mut reuse_total = 0usize;
    let mut newest_time: Option<DateTime<FixedOffset>> = None;

    for event in &ordered {
        if event.kind == TASK_EVENT_CLAIM {
            if created_by.is_none() {
                created_by = Some(event.actor.clone());
            }
            owner = Some(event.actor.clone());
        }
        if event.kind == TASK_EVENT_VALIDATION {
            last_validation = Some(TaskValidationSummary {
                reviewer: event.actor.clone(),
                verdict: event.outcome.clone(),
                event_seq: event.event_seq,
                recorded_at: event.recorded_at.clone(),
            });
        }
        if event.kind == TASK_EVENT_REUSE {
            reuse_total += 1;
        }
        if let Some(time) = parse_event_time(event) {
            if newest_time.map(|current| time > current).unwrap_or(true) {
                newest_time = Some(time);
            }
        }
    }

    let mut state = if ordered
        .last()
        .map(|event| event.kind == TASK_EVENT_ABANDON)
        .unwrap_or(false)
    {
        TASK_STATE_ABANDONED
    } else if let Some(validation) = last_validation.as_ref() {
        if validation.verdict == "pass" {
            TASK_STATE_DONE
        } else {
            TASK_STATE_ACTIVE
        }
    } else {
        TASK_STATE_ACTIVE
    };

    if state == TASK_STATE_ACTIVE {
        if let Some(newest) = newest_time {
            let ttl = ChronoDuration::days(i64::from(stale_days));
            if now.signed_duration_since(newest) > ttl {
                state = TASK_STATE_STALE;
            }
        }
    }

    TaskProjection {
        state: state.to_string(),
        owner,
        created_by,
        last_event: ordered.last().map(|event| TaskEventSummary {
            kind: event.kind.clone(),
            actor: event.actor.clone(),
            event_seq: event.event_seq,
            recorded_at: event.recorded_at.clone(),
        }),
        last_validation,
        reuse_total,
        event_total: ordered.len(),
    }
}

/// Stage 6 §C: the newest validation was written by the task's creator or its
/// current owner, after the same normalization the review guard uses. `false`
/// when the task has no validation at all.
fn is_self_validated(projection: &TaskProjection) -> bool {
    let Some(validation) = projection.last_validation.as_ref() else {
        return false;
    };
    let reviewer = super::routing::review_identity(&validation.reviewer);
    let mut identities: Vec<String> = Vec::new();
    if let Some(created_by) = projection.created_by.as_deref() {
        identities.push(super::routing::review_identity(created_by));
    }
    if let Some(owner) = projection.owner.as_deref() {
        identities.push(super::routing::review_identity(owner));
    }
    identities.contains(&reviewer)
}

/// Reference instant for a write's state projection: parse the timestamp that
/// was just stored, falling back to the wall clock (the value is generated by
/// this process, so the fallback is unreachable in practice).
fn projection_now(stamp: &str) -> DateTime<FixedOffset> {
    DateTime::parse_from_rfc3339(stamp).unwrap_or_else(|_| chrono::Utc::now().fixed_offset())
}

fn project_at(
    conn: &Connection,
    candidate_id: &str,
    now: DateTime<FixedOffset>,
) -> Result<TaskProjection, LearningError> {
    let events = load_task_events(conn, candidate_id)?;
    Ok(project_task_state(&events, now, DEFAULT_STALE_DAYS))
}

/// Normalize `--state`. `None`, `""` and `"all"` mean "no filter"; the other
/// four values are exactly the projection states.
fn normalize_state_filter(value: Option<&str>) -> Result<Option<String>, LearningError> {
    let Some(raw) = value else {
        return Ok(None);
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed == "all" {
        return Ok(None);
    }
    for state in [
        TASK_STATE_ACTIVE,
        TASK_STATE_DONE,
        TASK_STATE_ABANDONED,
        TASK_STATE_STALE,
    ] {
        if trimmed == state {
            return Ok(Some(state.to_string()));
        }
    }
    Err(LearningError::usage(format!(
        "--state must be one of active|done|abandoned|stale|all, got {raw:?}"
    )))
}

/// Stage 6 §D read side: one `origin_task_id -> [candidate_id]` map over every
/// *knowledge* candidate's `origin_tasks`, built once per report so the row loop
/// never rescans the table. Each list is sorted lexicographically and deduped.
///
/// A dangling origin id is kept as a key: the read side never validates links
/// (only the ingest write path does), so `show`/`workspace` still report a
/// knowledge candidate whose origin task has disappeared.
pub(crate) fn load_distilled_into(
    conn: &Connection,
) -> Result<BTreeMap<String, Vec<String>>, LearningError> {
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

    let mut map: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (candidate_id, record_json) in rows {
        let value: Value = serde_json::from_str(&record_json).map_err(|error| {
            LearningError::schema_invalid(format!(
                "stored candidate {candidate_id} is not readable: {error}"
            ))
        })?;
        if value.get("target_kind").and_then(Value::as_str) == Some(TASK_TARGET_KIND) {
            continue;
        }
        let Some(list) = value.get("origin_tasks").and_then(Value::as_array) else {
            continue;
        };
        for origin in list.iter().filter_map(Value::as_str) {
            map.entry(origin.to_string())
                .or_default()
                .push(candidate_id.clone());
        }
    }
    for ids in map.values_mut() {
        ids.sort();
        ids.dedup();
    }
    Ok(map)
}

/// Read-only `learning workspace`: every task candidate in the store, projected
/// at `now` and ordered by `last_event.event_seq` descending.
///
/// Task detection parses `record_json.target_kind` in Rust — the same
/// Rust-side judgement `migrate --to 3` and the task guards use (the
/// `candidates` table has no `target_kind` column and JSON1 is not assumed).
/// The scan is full-table by design (plan §10: single-machine store, no second
/// index this stage). Nothing is written: no `event_clock` row, no update.
pub(crate) fn workspace_report(
    conn: &Connection,
    root: &str,
    state: Option<&str>,
    now: DateTime<FixedOffset>,
    stale_days: u32,
) -> Result<WorkspaceReport, LearningError> {
    let wanted = normalize_state_filter(state)?;
    // Stage 6 §D: one map build for the whole report (never a per-row scan).
    let distilled = load_distilled_into(conn)?;
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

    let mut tasks = Vec::new();
    for (candidate_id, record_json) in rows {
        let value: Value = serde_json::from_str(&record_json).map_err(|error| {
            LearningError::schema_invalid(format!(
                "stored candidate {candidate_id} is not readable: {error}"
            ))
        })?;
        if value.get("target_kind").and_then(Value::as_str) != Some(TASK_TARGET_KIND) {
            continue;
        }
        let record = store::parse_record(&record_json)?;
        let events = load_task_events(conn, &candidate_id)?;
        let projection = project_task_state(&events, now, stale_days);
        if let Some(wanted) = wanted.as_deref() {
            if projection.state != wanted {
                continue;
            }
        }
        let distilled_into = distilled
            .get(&record.candidate_id)
            .cloned()
            .unwrap_or_default();
        let self_validated = is_self_validated(&projection);
        tasks.push(WorkspaceTaskRow {
            candidate_id: record.candidate_id,
            claim: record.claim,
            state: projection.state,
            created_by: projection.created_by,
            owner: projection.owner,
            last_event: projection.last_event,
            last_validation: projection.last_validation,
            reuse_total: projection.reuse_total,
            event_total: projection.event_total,
            distilled_into,
            self_validated,
        });
    }

    tasks.sort_by(|a, b| {
        let left = a
            .last_event
            .as_ref()
            .map(|event| event.event_seq)
            .unwrap_or(i64::MIN);
        let right = b
            .last_event
            .as_ref()
            .map(|event| event.event_seq)
            .unwrap_or(i64::MIN);
        right.cmp(&left)
    });

    Ok(WorkspaceReport {
        ok: true,
        root: root.to_string(),
        header: INJECT_NOTICE.to_string(),
        generated_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        stale_days,
        now: now.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        total: tasks.len(),
        tasks,
    })
}

/// Read-only `learning task show`: one task's summary, projection and full
/// unified event view (all three event tables, `event_seq` ascending).
pub(crate) fn task_show_report(
    conn: &Connection,
    candidate_id: &str,
    now: DateTime<FixedOffset>,
    stale_days: u32,
) -> Result<TaskShowReport, LearningError> {
    let id = require_candidate_id(candidate_id)?;
    let Some((record, _)) = store::load_current(conn, &id)? else {
        return Err(LearningError::not_found(format!(
            "no candidate {id} in this learning root"
        )));
    };
    if record.target_kind != TASK_TARGET_KIND {
        return Err(not_a_task(&record.candidate_id, &record.target_kind));
    }
    let events = load_task_events(conn, &id)?;
    let projection = project_task_state(&events, now, stale_days);
    let distilled_into = load_distilled_into(conn)?
        .get(&record.candidate_id)
        .cloned()
        .unwrap_or_default();
    let self_validated = is_self_validated(&projection);
    Ok(TaskShowReport {
        ok: true,
        candidate_id: record.candidate_id,
        claim: record.claim,
        target_kind: record.target_kind,
        derived_from: record.derived_from,
        state: projection.state,
        created_by: projection.created_by,
        owner: projection.owner,
        last_event: projection.last_event,
        last_validation: projection.last_validation,
        reuse_total: projection.reuse_total,
        event_total: projection.event_total,
        events,
        distilled_into,
        self_validated,
        header: INJECT_NOTICE.to_string(),
    })
}

/// Append one `task_events` row inside the caller's `BEGIN IMMEDIATE`
/// transaction. Returns `(event_seq, recorded_at)`.
pub(crate) fn append_task_event(
    tx: &Transaction<'_>,
    candidate_id: &str,
    event_kind: &str,
    actor: &str,
    note: &str,
) -> Result<(i64, String), LearningError> {
    let event_seq = store::allocate_event_seq(tx)?;
    let recorded_at = store::storage_now();
    tx.execute(
        "INSERT INTO task_events (event_seq, candidate_id, event_kind, actor_id, note, recorded_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![event_seq, candidate_id, event_kind, actor, note, recorded_at],
    )
    .map_err(store::db_error)?;
    Ok((event_seq, recorded_at))
}

fn not_a_task(candidate_id: &str, target_kind: &str) -> LearningError {
    LearningError::new(
        NOT_A_TASK,
        LearningErrorKind::Usage,
        format!(
            "candidate {candidate_id} has target_kind {target_kind:?}; this command only \
             applies to a target_kind \"task\" candidate"
        ),
    )
}

/// `TASK_NOT_ROUTEABLE` in its usage class: `gate` / `dedup` refuse a task
/// candidate before doing any work.
pub(crate) fn not_routeable(candidate_id: &str, command: &str) -> LearningError {
    LearningError::new(
        TASK_NOT_ROUTEABLE,
        LearningErrorKind::Usage,
        format!(
            "candidate {candidate_id} is a workspace task (target_kind \"task\"); {command} \
             applies only to knowledge candidates — a task is never routed, deduped or exported"
        ),
    )
}

/// Load the candidate and prove it is a task, inside the write transaction.
fn load_task(tx: &Transaction<'_>, candidate_id: &str) -> Result<CandidateRecord, LearningError> {
    let Some((record, _)) = store::load_current(tx, candidate_id)? else {
        return Err(LearningError::not_found(format!(
            "no candidate {candidate_id} in this learning root"
        )));
    };
    if record.target_kind != TASK_TARGET_KIND {
        return Err(not_a_task(&record.candidate_id, &record.target_kind));
    }
    Ok(record)
}

fn require_candidate_id(value: &str) -> Result<String, LearningError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(LearningError::usage(
            "--candidate-id must be a non-empty candidate id",
        ));
    }
    Ok(trimmed.to_string())
}

/// The v1 payload of a fresh task candidate. `evidence` stays empty (a task's
/// evidence lives in the Archive and is linked through the event tables) and
/// `proposed_target` is the fixed constant, so equal claims converge on the
/// same `candidate_id`.
///
/// `derived_from` is stage 5 §A: the key is present only when the caller
/// supplied it, so the no-flag payload keeps the exact pre-stage-5 bytes.
fn task_payload(claim: &str, candidate_id: &str, now: &str, derived_from: Option<&str>) -> Value {
    let mut payload = json!({
        "schema_version": super::model::SCHEMA_VERSION,
        "candidate_id": candidate_id,
        "revision": 1,
        "status": super::model::CANDIDATE_STATUS,
        "claim": claim,
        "target_kind": TASK_TARGET_KIND,
        "proposed_target": TASK_PROPOSED_TARGET,
        "applicability": [],
        "counterexamples": [],
        "unknowns": [],
        "evidence": [],
        "contradicting_evidence": [],
        "dedup": {
            "query": "",
            "matches": [],
            "disposition": "pending",
            "checked_at": null,
        },
        "reuse": [],
        "validation": [],
        "promotion": {
            "approval": null,
            "apply": null,
            "verification": null,
        },
        "ingestions": [],
        "source_checkpoint": format!("task:{candidate_id}"),
        "extractor": "task",
        "created_at": now,
        "updated_at": now,
    });
    if let Some(value) = derived_from {
        payload["derived_from"] = Value::String(value.to_string());
    }
    payload
}

impl LearningStore {
    /// `learning task create`: create the task candidate and its first `claim`
    /// event atomically, or report the existing candidate without writing.
    ///
    /// The pre-stage-5 signature is kept so existing callers are source
    /// compatible; it is exactly `create_task_with_derived_from(..., None)`.
    pub fn create_task(
        &mut self,
        claim: &str,
        actor: &str,
    ) -> Result<TaskCreateOutcome, LearningError> {
        self.create_task_with_derived_from(claim, actor, None)
    }

    /// `learning task create` with the stage 5 §A optional `derived_from`.
    ///
    /// The claim is validated even on the duplicate path (input legality is
    /// not a write, and it keeps every create attempt subject to one rule
    /// set). `derived_from` is validated and stored only on a first write: the
    /// load-first duplicate path returns the existing record unchanged, so the
    /// first value wins and a later create can never rewrite it.
    pub fn create_task_with_derived_from(
        &mut self,
        claim: &str,
        actor: &str,
        derived_from: Option<&str>,
    ) -> Result<TaskCreateOutcome, LearningError> {
        let actor = store::require_actor(actor)?;
        validate_task_claim(claim)?;
        // Validate the field up front so an invalid value is refused even when
        // the claim happens to be a duplicate; the value itself is still only
        // written on a first create.
        let derived_from = validate_derived_from(derived_from)?;
        let candidate_id = candidate_id_for(claim, TASK_TARGET_KIND, TASK_PROPOSED_TARGET);
        let tx = self
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(store::db_error)?;

        // Load first, by the identity derived from the claim alone (R13): a
        // duplicate create must not add an ingestion, a revision or an event.
        if let Some((existing, _)) = store::load_current(&tx, &candidate_id)? {
            let now = projection_now(&store::storage_now());
            let projection = project_at(&tx, &candidate_id, now)?;
            tx.commit().map_err(store::db_error)?;
            return Ok(TaskCreateOutcome {
                ok: true,
                created: false,
                duplicate: true,
                candidate_id,
                claim: existing.claim,
                actor,
                state: projection.state,
            });
        }

        let now = store::storage_now();
        let validated = validate_candidate(&task_payload(
            claim,
            &candidate_id,
            &now,
            derived_from.as_deref(),
        ))?;
        LearningStore::ingest_in_transaction(&tx, &validated)?;
        append_task_event(&tx, &candidate_id, TASK_EVENT_CLAIM, &actor, "")?;
        let projection = project_at(&tx, &candidate_id, projection_now(&now))?;
        let claim = validated.record.claim.clone();
        tx.commit().map_err(store::db_error)?;
        Ok(TaskCreateOutcome {
            ok: true,
            created: true,
            duplicate: false,
            candidate_id,
            claim,
            actor,
            state: projection.state,
        })
    }

    /// `learning task claim`: append one `claim` event (ownership moves to
    /// `actor`).
    pub fn claim_task(
        &mut self,
        candidate_id: &str,
        actor: &str,
        note: Option<&str>,
    ) -> Result<TaskEventOutcome, LearningError> {
        let candidate_id = require_candidate_id(candidate_id)?;
        let actor = store::require_actor(actor)?;
        let note = note.unwrap_or_default().trim().to_string();
        let tx = self
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(store::db_error)?;
        let record = load_task(&tx, &candidate_id)?;
        let (event_seq, recorded_at) =
            append_task_event(&tx, &record.candidate_id, TASK_EVENT_CLAIM, &actor, &note)?;
        tx.commit().map_err(store::db_error)?;
        Ok(TaskEventOutcome {
            ok: true,
            candidate_id: record.candidate_id,
            event_kind: TASK_EVENT_CLAIM.to_string(),
            actor,
            note,
            recorded_at,
            event_seq,
        })
    }

    /// `learning task abandon`: append one `abandon` event. `reason` is
    /// mandatory and stored as the event note.
    pub fn abandon_task(
        &mut self,
        candidate_id: &str,
        actor: &str,
        reason: &str,
    ) -> Result<TaskEventOutcome, LearningError> {
        let candidate_id = require_candidate_id(candidate_id)?;
        let actor = store::require_actor(actor)?;
        let reason = reason.trim();
        if reason.is_empty() {
            return Err(LearningError::usage(
                "--reason must be a non-empty abandonment reason",
            ));
        }
        let tx = self
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(store::db_error)?;
        let record = load_task(&tx, &candidate_id)?;
        let (event_seq, recorded_at) = append_task_event(
            &tx,
            &record.candidate_id,
            TASK_EVENT_ABANDON,
            &actor,
            reason,
        )?;
        tx.commit().map_err(store::db_error)?;
        Ok(TaskEventOutcome {
            ok: true,
            candidate_id: record.candidate_id,
            event_kind: TASK_EVENT_ABANDON.to_string(),
            actor,
            note: reason.to_string(),
            recorded_at,
            event_seq,
        })
    }

    /// Read-only unified event view of one task (used by the projection tests
    /// and, from implementation-003 on, by `task show` / `workspace`).
    pub fn task_event_view(&self, candidate_id: &str) -> Result<Vec<TaskEventView>, LearningError> {
        let id = require_candidate_id(candidate_id)?;
        let exists: Option<i64> = self
            .connection()
            .query_row(
                "SELECT 1 FROM candidates WHERE candidate_id = ?1",
                params![id],
                |row| row.get(0),
            )
            .optional()
            .map_err(store::db_error)?;
        if exists.is_none() {
            return Err(LearningError::not_found(format!(
                "no candidate {id} in this learning root"
            )));
        }
        load_task_events(self.connection(), &id)
    }

    /// Project one stored task's state at `now`.
    pub fn project_task(
        &self,
        candidate_id: &str,
        now: DateTime<FixedOffset>,
        stale_days: u32,
    ) -> Result<TaskProjection, LearningError> {
        let events = self.task_event_view(candidate_id)?;
        Ok(project_task_state(&events, now, stale_days))
    }

    /// `learning workspace`: all task candidates, projected at `now` and
    /// ordered by `last_event.event_seq` descending. Read-only.
    pub fn workspace(
        &self,
        state: Option<&str>,
        now: DateTime<FixedOffset>,
        stale_days: u32,
    ) -> Result<WorkspaceReport, LearningError> {
        let root = self.root().to_string_lossy().into_owned();
        workspace_report(self.connection(), &root, state, now, stale_days)
    }

    /// `learning task show`: one task's summary, projection and full unified
    /// event view. Read-only.
    pub fn task_show(
        &self,
        candidate_id: &str,
        now: DateTime<FixedOffset>,
        stale_days: u32,
    ) -> Result<TaskShowReport, LearningError> {
        task_show_report(self.connection(), candidate_id, now, stale_days)
    }

    /// `sqlite3_total_changes64` of this store's connection. A read-only
    /// diagnostic used by the A6 acceptance test to prove the workspace surface
    /// writes nothing at all on the connection it reads through.
    pub fn total_changes(&self) -> u64 {
        // SAFETY: `handle` is a live connection pointer borrowed from `&self`
        // and `sqlite3_total_changes64` only reads the connection counter.
        let total = unsafe { rusqlite::ffi::sqlite3_total_changes64(self.connection().handle()) };
        u64::try_from(total).unwrap_or(0)
    }
}
