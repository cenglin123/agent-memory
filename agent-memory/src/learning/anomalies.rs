//! Stage 6 §A: `learning anomalies` — the read-only anomaly-signal surface.
//!
//! The surface is the production end of Self-Reflect: it turns the coordination
//! store into a deterministic list of *signals* an independent auditor can
//! inspect. It never writes: the CLI opens
//! [`LearningStore::open_read_only`], every query is a select, `event_clock` is
//! never advanced and no candidate row is touched.
//!
//! # Signals
//!
//! Eight kinds are defined by one constant table ([`ANOMALY_KINDS`]); the
//! thresholds may only be tightened against the shipped constants:
//!
//! | kind | type | rule |
//! |---|---|---|
//! | [`ANOMALY_NEW_ACTOR`] | event | first event of an actor in the actor set |
//! | [`ANOMALY_MARKER_HIT`] | state | non-abandoned task claim hits an inject visibility marker |
//! | [`ANOMALY_LONG_CLAIM`] | state | non-abandoned task claim ≥ [`LONG_CLAIM_CODEPOINTS`] code points |
//! | [`ANOMALY_CREATE_BURST`] | event | one actor creates ≥ [`CREATE_BURST_MIN_EVENTS`] tasks in [`CREATE_BURST_WINDOW_MINUTES`] |
//! | [`ANOMALY_MULTI_ACTOR`] | event | one task collects ≥ [`MULTI_ACTOR_MIN_ACTORS`] actors in [`MULTI_ACTOR_WINDOW_HOURS`] |
//! | [`ANOMALY_LEGACY_INVALID`] | state | a task row the stage-5 write-side gate rejects |
//! | [`ANOMALY_NEVER_REFLECTED`] | environment | tasks exist but no reflect task ever reached the watermark |
//! | [`ANOMALY_REJECT_CLEAR_FLIP`] | event | one candidate flips `rejected` ↔ `cleared` ≥ [`QUALIFICATION_FLIP_MIN`] times in [`QUALIFICATION_FLIP_WINDOW_DAYS`] days |
//!
//! The **actor set** is `task create` `created_by` (the first claim event) ∪
//! every `task_events.actor_id` ∪ every `reuse_history.actor_id`.
//! `validation_history.reviewer` is a different namespace: it is never an actor
//! and therefore never triggers an actor-class signal.
//!
//! # Time
//!
//! Every window is measured on the stored `recorded_at` (RFC 3339, UTC) as the
//! half-open interval `(t − W, t]`; the deterministic order is
//! `(recorded_at, event_seq)`. `--now` is only the stale anchor of the state
//! projection (exactly like `learning workspace`); it never clips an event.
//!
//! # Watermark (zero new storage)
//!
//! A Self-Reflect run is registered as a task whose claim starts with
//! [`REFLECT_CLAIM_PREFIX`]. Such a task is *counted* when its projection is
//! `done`, its newest verdict is `pass`, and the reviewer of that verdict
//! (trimmed + case-folded) differs from `created_by`. The watermark is the
//! largest creation event seq among the counted tasks (ties resolve to the
//! lexicographically smallest `task_id`). Events of a reflect task itself
//! (creation, takeover, validation) produce no signal at all, so a reflect run
//! cannot feed its own successor audit.
//!
//! # Output contract
//!
//! Every signal is `{kind, signal_seq, anchor_seq, candidate_id, actor,
//! detail}`. Event signals carry the anchor event seq in both seq fields; state
//! signals carry the task's newest event seq (a new event makes the state
//! signal resurface); the environment signal carries `0`. Signals are sorted by
//! `(kind bytes, anchor_seq, candidate_id)`. The default filter is
//! `signal_seq > since_seq` with `since_seq` = the watermark (0 when there is
//! none); environment signals are never filtered.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use chrono::{DateTime, Duration as ChronoDuration, FixedOffset};
use rusqlite::Connection;
use serde::Serialize;

use super::inject::{legacy_invalid_reason, marker_classes};
use super::model::{claim_codepoint_count, TASK_TARGET_KIND};
use super::store::{self, LearningStore};
use super::task::{
    load_task_events, project_task_state, TaskEventView, DEFAULT_STALE_DAYS, TASK_EVENT_ABANDON,
    TASK_EVENT_CLAIM, TASK_STATE_ABANDONED, TASK_STATE_DONE,
};
use super::LearningError;

/// A new actor appears for the first time (event type).
pub const ANOMALY_NEW_ACTOR: &str = "new_actor";
/// A non-abandoned task claim hits an inject visibility marker (state type).
pub const ANOMALY_MARKER_HIT: &str = "marker_hit";
/// A non-abandoned task claim is at least [`LONG_CLAIM_CODEPOINTS`] code points
/// (state type).
pub const ANOMALY_LONG_CLAIM: &str = "long_claim";
/// One actor creates at least [`CREATE_BURST_MIN_EVENTS`] tasks inside
/// [`CREATE_BURST_WINDOW_MINUTES`] (event type).
pub const ANOMALY_CREATE_BURST: &str = "create_burst";
/// One task collects at least [`MULTI_ACTOR_MIN_ACTORS`] distinct actors inside
/// [`MULTI_ACTOR_WINDOW_HOURS`] (event type).
pub const ANOMALY_MULTI_ACTOR: &str = "multi_actor";
/// A task row violates the stage-5 write-side gate (state type).
pub const ANOMALY_LEGACY_INVALID: &str = "legacy_invalid";
/// Tasks exist but no reflect task ever reached the watermark (environment
/// type).
pub const ANOMALY_NEVER_REFLECTED: &str = "never_reflected";
/// One candidate flips `rejected` ↔ `cleared` repeatedly in a short window
/// (event type, WARN level).
pub const ANOMALY_REJECT_CLEAR_FLIP: &str = "reject_clear_flip";

/// The fixed signal-kind table; byte order is the report's primary sort key.
pub const ANOMALY_KINDS: [&str; 8] = [
    ANOMALY_NEW_ACTOR,
    ANOMALY_MARKER_HIT,
    ANOMALY_LONG_CLAIM,
    ANOMALY_CREATE_BURST,
    ANOMALY_MULTI_ACTOR,
    ANOMALY_LEGACY_INVALID,
    ANOMALY_NEVER_REFLECTED,
    ANOMALY_REJECT_CLEAR_FLIP,
];

/// `reject_clear_flip` window length in days.
pub const QUALIFICATION_FLIP_WINDOW_DAYS: i64 = 7;
/// `reject_clear_flip` minimum number of flips in the window.
pub const QUALIFICATION_FLIP_MIN: usize = 3;

/// `long_claim` threshold, in Unicode code points (the write-side counter).
pub const LONG_CLAIM_CODEPOINTS: usize = 400;
/// `create_burst` window length in minutes.
pub const CREATE_BURST_WINDOW_MINUTES: i64 = 60;
/// `create_burst` minimum number of creates per window.
pub const CREATE_BURST_MIN_EVENTS: usize = 5;
/// `multi_actor` window length in hours.
pub const MULTI_ACTOR_WINDOW_HOURS: i64 = 24;
/// `multi_actor` minimum number of distinct actors per window.
pub const MULTI_ACTOR_MIN_ACTORS: usize = 3;

/// The fixed claim prefix that registers a Self-Reflect run. Only the prefix is
/// compared.
pub const REFLECT_CLAIM_PREFIX: &str = "[self-reflect]";

/// One reported anomaly signal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AnomalySignal {
    pub kind: String,
    /// Filtering / ordering sequence: the anchor event for event signals, the
    /// newest event of the task for state signals, `0` for the environment one.
    pub signal_seq: i64,
    /// The event the signal is anchored at (same as `signal_seq`).
    pub anchor_seq: i64,
    /// The subject candidate (a task id, a knowledge candidate for a reuse
    /// event, or `""` for the environment signal).
    pub candidate_id: String,
    /// The acting identity the signal is about (`""` when the row carries no
    /// actor, or for the environment signal).
    pub actor: String,
    /// One-line, deterministic detail; see the module docs of each kind.
    pub detail: String,
}

/// The watermark: the newest counted Self-Reflect task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Watermark {
    pub event_seq: i64,
    pub task_id: String,
}

/// `learning anomalies` payload. `text` is the rendered text mode and stays out
/// of the JSON shape.
#[derive(Debug, Clone, Serialize)]
pub struct AnomaliesReport {
    pub ok: bool,
    pub root: String,
    pub watermark: Option<Watermark>,
    /// The effective filter bound (`--since-seq`, or the watermark, or 0).
    pub since_seq: i64,
    pub signals: Vec<AnomalySignal>,
    /// The watermark-based default count (independent of `--since-seq`); this
    /// is the number `learning inject` and 90 `AM-REFLECT` report.
    pub unreviewed_total: usize,
    /// Counts of the reported signals per kind (sorted keys).
    pub by_kind: BTreeMap<String, usize>,
    #[serde(skip)]
    pub text: String,
}

/// Environment signals are never filtered: they describe the library as a
/// whole, not an event inside it.
pub fn is_environmental(kind: &str) -> bool {
    kind == ANOMALY_NEVER_REFLECTED
}

// ---------------------------------------------------------------------------
// Internal scan
// ---------------------------------------------------------------------------

/// The four event sources of the actor set. `Create` is the task's first claim
/// event (the one that derives `created_by`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActorSource {
    Create,
    Claim,
    Abandon,
    Reuse,
}

impl ActorSource {
    fn as_str(self) -> &'static str {
        match self {
            ActorSource::Create => "create",
            ActorSource::Claim => "claim",
            ActorSource::Abandon => "abandon",
            ActorSource::Reuse => "reuse",
        }
    }
}

/// One actor-bearing event, reduced to what the actor-class signals need.
#[derive(Debug, Clone)]
struct ActorEvent {
    event_seq: i64,
    recorded_at: DateTime<FixedOffset>,
    actor: String,
    source: ActorSource,
    candidate_id: String,
}

/// One task candidate plus its projection, unified event view and the facts the
/// state signals need.
#[derive(Debug, Clone)]
struct TaskFacts {
    candidate_id: String,
    claim: String,
    derived_from: Option<String>,
    is_reflect: bool,
    state: String,
    created_by: Option<String>,
    owner: Option<String>,
    last_event_seq: i64,
    last_validation: Option<super::task::TaskValidationSummary>,
    /// The task's first `claim` event seq (the creation event), `None` only for
    /// a hand-edited row without a claim event.
    create_event_seq: Option<i64>,
    events: Vec<TaskEventView>,
}

fn parse_time(value: &str) -> Option<DateTime<FixedOffset>> {
    DateTime::parse_from_rfc3339(value).ok()
}

/// Trim + the stage-5 detection fold, as required by the watermark's
/// self-review test. It is the one review-identity normalization shared with
/// the stage-6 §C conflict set (`review_identity`).
fn fold_identity(value: &str) -> String {
    super::routing::review_identity(value)
}

fn load_tasks(
    conn: &Connection,
    now: DateTime<FixedOffset>,
) -> Result<Vec<TaskFacts>, LearningError> {
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
        // The same Rust-side kind test the rest of the surface uses (no JSON1
        // dependency, no second index).
        let value: serde_json::Value = serde_json::from_str(&record_json).map_err(|error| {
            LearningError::schema_invalid(format!(
                "stored candidate {candidate_id} is not readable: {error}"
            ))
        })?;
        if value.get("target_kind").and_then(serde_json::Value::as_str) != Some(TASK_TARGET_KIND) {
            continue;
        }
        let record = store::parse_record(&record_json)?;
        let events = load_task_events(conn, &record.candidate_id)?;
        let projection = project_task_state(&events, now, DEFAULT_STALE_DAYS);
        let create_event_seq = events
            .iter()
            .filter(|event| event.source == "task" && event.kind == TASK_EVENT_CLAIM)
            .map(|event| event.event_seq)
            .min();
        tasks.push(TaskFacts {
            is_reflect: record.claim.starts_with(REFLECT_CLAIM_PREFIX),
            candidate_id: record.candidate_id,
            claim: record.claim,
            derived_from: record.derived_from,
            state: projection.state,
            created_by: projection.created_by,
            owner: projection.owner,
            last_event_seq: projection
                .last_event
                .as_ref()
                .map(|event| event.event_seq)
                .unwrap_or(0),
            last_validation: projection.last_validation,
            create_event_seq,
            events,
        });
    }
    Ok(tasks)
}

/// Every actor-bearing event of the store, ordered by `(recorded_at,
/// event_seq)`. Events whose `recorded_at` does not parse are dropped: a
/// hand-edited timestamp cannot be placed in a window.
///
/// `task_events` rows are read once each (so an actor is never double-counted);
/// the task's first `claim` event is labelled `create`. `reuse_history` rows are
/// read once each, and rows whose owning candidate or target task is a reflect
/// task are skipped (self-absorption exclusion).
fn load_actor_events(
    conn: &Connection,
    reflect_ids: &BTreeSet<String>,
) -> Result<Vec<ActorEvent>, LearningError> {
    let mut stmt = conn
        .prepare(
            "SELECT event_seq, candidate_id, event_kind, actor_id, recorded_at \
             FROM task_events",
        )
        .map_err(store::db_error)?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
            ))
        })
        .map_err(store::db_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(store::db_error)?;
    drop(stmt);

    let mut first_claim: HashMap<String, i64> = HashMap::new();
    for (event_seq, candidate_id, kind, _, _) in &rows {
        if kind != TASK_EVENT_CLAIM {
            continue;
        }
        first_claim
            .entry(candidate_id.clone())
            .and_modify(|current| {
                if *event_seq < *current {
                    *current = *event_seq;
                }
            })
            .or_insert(*event_seq);
    }

    let mut events = Vec::new();
    for (event_seq, candidate_id, kind, actor, recorded_at) in rows {
        if reflect_ids.contains(&candidate_id) {
            continue;
        }
        let Some(time) = parse_time(&recorded_at) else {
            continue;
        };
        let source = if kind == TASK_EVENT_ABANDON {
            ActorSource::Abandon
        } else if first_claim.get(&candidate_id).copied() == Some(event_seq) {
            ActorSource::Create
        } else {
            ActorSource::Claim
        };
        events.push(ActorEvent {
            event_seq,
            recorded_at: time,
            actor,
            source,
            candidate_id,
        });
    }

    let mut stmt = conn
        .prepare(
            "SELECT event_seq, candidate_id, task_id, actor_id, recorded_at \
             FROM reuse_history",
        )
        .map_err(store::db_error)?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
            ))
        })
        .map_err(store::db_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(store::db_error)?;
    drop(stmt);
    for (event_seq, candidate_id, task_id, actor, recorded_at) in rows {
        if reflect_ids.contains(&candidate_id) || reflect_ids.contains(&task_id) {
            continue;
        }
        let Some(time) = parse_time(&recorded_at) else {
            continue;
        };
        events.push(ActorEvent {
            event_seq,
            recorded_at: time,
            actor,
            source: ActorSource::Reuse,
            candidate_id,
        });
    }

    events.sort_by(|left, right| {
        left.recorded_at
            .cmp(&right.recorded_at)
            .then_with(|| left.event_seq.cmp(&right.event_seq))
    });
    Ok(events)
}

/// Compute the watermark from the counted reflect tasks.
fn watermark_of(tasks: &[TaskFacts]) -> Option<Watermark> {
    let mut best: Option<Watermark> = None;
    for task in tasks {
        if !task.is_reflect || task.state != TASK_STATE_DONE {
            continue;
        }
        let Some(validation) = task.last_validation.as_ref() else {
            continue;
        };
        if validation.verdict != "pass" {
            continue;
        }
        let creator = task
            .created_by
            .as_deref()
            .map(fold_identity)
            .unwrap_or_default();
        if fold_identity(&validation.reviewer) == creator {
            // An explicit self-review never moves the watermark (stage 6 §C
            // also refuses it on the write path from implementation-004 on).
            continue;
        }
        let Some(event_seq) = task.create_event_seq else {
            continue;
        };
        let candidate = Watermark {
            event_seq,
            task_id: task.candidate_id.clone(),
        };
        best = match best {
            None => Some(candidate),
            Some(current) => {
                if candidate.event_seq > current.event_seq
                    || (candidate.event_seq == current.event_seq
                        && candidate.task_id < current.task_id)
                {
                    Some(candidate)
                } else {
                    Some(current)
                }
            }
        };
    }
    best
}

fn push_signal(
    signals: &mut Vec<AnomalySignal>,
    kind: &'static str,
    seq: i64,
    candidate_id: &str,
    actor: &str,
    detail: String,
) {
    signals.push(AnomalySignal {
        kind: kind.to_string(),
        signal_seq: seq,
        anchor_seq: seq,
        candidate_id: candidate_id.to_string(),
        actor: actor.to_string(),
        detail,
    });
}

/// `reject_clear_flip` signals: per candidate, count adjacent
/// `rejected` ↔ `cleared` transitions whose later row falls in the last
/// [`QUALIFICATION_FLIP_WINDOW_DAYS`] days. The signal is anchored at the
/// `event_seq` of the row that completed the threshold.
fn collect_qualification_flips(
    conn: &Connection,
    now: DateTime<FixedOffset>,
) -> Result<Vec<AnomalySignal>, LearningError> {
    let mut stmt = conn
        .prepare(
            "SELECT candidate_id, state, event_seq, actor, recorded_at \
             FROM qualification_history ORDER BY candidate_id, seq",
        )
        .map_err(store::db_error)?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
            ))
        })
        .map_err(store::db_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(store::db_error)?;
    drop(stmt);

    let start = now - ChronoDuration::days(QUALIFICATION_FLIP_WINDOW_DAYS);
    let mut signals = Vec::new();
    let mut index = 0;
    while index < rows.len() {
        let candidate_id = rows[index].0.clone();
        let mut flips = 0usize;
        let mut anchor: Option<(i64, String)> = None;
        let mut cursor = index;
        while cursor + 1 < rows.len() && rows[cursor + 1].0 == candidate_id {
            let prior = &rows[cursor];
            let current = &rows[cursor + 1];
            let pair = (prior.1.as_str(), current.1.as_str());
            let in_window = parse_time(&current.4)
                .map(|recorded| recorded > start)
                .unwrap_or(false);
            if in_window
                && matches!(
                    pair,
                    ("rejected", "cleared") | ("cleared", "rejected")
                )
            {
                flips += 1;
                anchor = Some((current.2, current.3.clone()));
            }
            cursor += 1;
        }
        if flips >= QUALIFICATION_FLIP_MIN {
            if let Some((event_seq, actor)) = anchor {
                push_signal(
                    &mut signals,
                    ANOMALY_REJECT_CLEAR_FLIP,
                    event_seq,
                    &candidate_id,
                    &actor,
                    format!(
                        "flips={flips} window_days={QUALIFICATION_FLIP_WINDOW_DAYS}"
                    ),
                );
            }
        }
        index = cursor + 1;
    }
    Ok(signals)
}

/// All signals of the store, already sorted by `(kind, anchor_seq,
/// candidate_id)` and *before* the `since_seq` filter.
fn collect_signals(
    tasks: &[TaskFacts],
    actor_events: &[ActorEvent],
    watermark: &Option<Watermark>,
    qualification_flips: &[AnomalySignal],
) -> Vec<AnomalySignal> {
    let mut signals = Vec::new();

    // new_actor: the first event of each actor in `(recorded_at, event_seq)`
    // order. `actor_events` is already sorted, so the first hit wins.
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    for event in actor_events {
        if !seen.insert(event.actor.as_str()) {
            continue;
        }
        push_signal(
            &mut signals,
            ANOMALY_NEW_ACTOR,
            event.event_seq,
            &event.candidate_id,
            &event.actor,
            format!("first_seen={}", event.source.as_str()),
        );
    }

    // create_burst: per actor, anchor on the 5th, 10th, ... create event of
    // every 60-minute window `(t - 60m, t]`.
    let mut creates: HashMap<&str, Vec<&ActorEvent>> = HashMap::new();
    for event in actor_events {
        if event.source == ActorSource::Create {
            creates.entry(event.actor.as_str()).or_default().push(event);
        }
    }
    let window = ChronoDuration::minutes(CREATE_BURST_WINDOW_MINUTES);
    for (_, events) in creates.iter_mut() {
        // `actor_events` is globally sorted, so each actor's list is too.
        events.sort_by(|left, right| {
            left.recorded_at
                .cmp(&right.recorded_at)
                .then_with(|| left.event_seq.cmp(&right.event_seq))
        });
        for index in 0..events.len() {
            let anchor = events[index];
            let start = anchor.recorded_at - window;
            let count = events[..=index]
                .iter()
                .filter(|event| event.recorded_at > start)
                .count();
            if count >= CREATE_BURST_MIN_EVENTS && count % CREATE_BURST_MIN_EVENTS == 0 {
                push_signal(
                    &mut signals,
                    ANOMALY_CREATE_BURST,
                    anchor.event_seq,
                    &anchor.candidate_id,
                    &anchor.actor,
                    format!("creates={count} window_minutes={CREATE_BURST_WINDOW_MINUTES}"),
                );
            }
        }
    }

    // multi_actor: per task, the event that makes the distinct-actor set of the
    // 24-hour window reach the threshold. Validation rows are not actors.
    let actor_window = ChronoDuration::hours(MULTI_ACTOR_WINDOW_HOURS);
    for task in tasks {
        if task.is_reflect {
            continue;
        }
        let mut events: Vec<&TaskEventView> = task
            .events
            .iter()
            .filter(|event| event.source != "validation")
            .filter(|event| parse_time(&event.recorded_at).is_some())
            .collect();
        events.sort_by(|left, right| {
            let left_time = parse_time(&left.recorded_at).expect("filtered above");
            let right_time = parse_time(&right.recorded_at).expect("filtered above");
            left_time
                .cmp(&right_time)
                .then_with(|| left.event_seq.cmp(&right.event_seq))
        });
        for index in 0..events.len() {
            let anchor_time = parse_time(&events[index].recorded_at).expect("filtered above");
            let start = anchor_time - actor_window;
            let mut distinct = BTreeSet::new();
            for event in events[..=index].iter() {
                let time = parse_time(&event.recorded_at).expect("filtered above");
                if time > start {
                    distinct.insert(event.actor.as_str());
                }
            }
            if distinct.len() >= MULTI_ACTOR_MIN_ACTORS {
                push_signal(
                    &mut signals,
                    ANOMALY_MULTI_ACTOR,
                    events[index].event_seq,
                    &task.candidate_id,
                    &events[index].actor,
                    format!(
                        "actors={} window_hours={MULTI_ACTOR_WINDOW_HOURS}",
                        distinct.len()
                    ),
                );
                break;
            }
        }
    }

    // State signals: one per task row. A reflect task is skipped whole (its own
    // events and its own state never feed a successor audit).
    for task in tasks {
        if task.is_reflect {
            continue;
        }
        if task.last_event_seq > 0 {
            if let Some(reason) = legacy_invalid_reason(
                &task.candidate_id,
                &task.claim,
                task.derived_from.as_deref(),
                task.created_by.as_deref(),
                task.owner.as_deref(),
            ) {
                push_signal(
                    &mut signals,
                    ANOMALY_LEGACY_INVALID,
                    task.last_event_seq,
                    &task.candidate_id,
                    task.created_by.as_deref().unwrap_or(""),
                    reason,
                );
            }
        }
        if task.state == TASK_STATE_ABANDONED {
            continue;
        }
        let markers = marker_classes(&task.claim);
        if !markers.is_empty() {
            push_signal(
                &mut signals,
                ANOMALY_MARKER_HIT,
                task.last_event_seq,
                &task.candidate_id,
                task.owner
                    .as_deref()
                    .or(task.created_by.as_deref())
                    .unwrap_or(""),
                format!("state={} markers={}", task.state, markers.join(",")),
            );
        }
        let codepoints = claim_codepoint_count(&task.claim);
        if codepoints >= LONG_CLAIM_CODEPOINTS {
            push_signal(
                &mut signals,
                ANOMALY_LONG_CLAIM,
                task.last_event_seq,
                &task.candidate_id,
                task.owner
                    .as_deref()
                    .or(task.created_by.as_deref())
                    .unwrap_or(""),
                format!("state={} codepoints={codepoints}", task.state),
            );
        }
    }

    // never_reflected: tasks exist and no reflect task ever reached the
    // watermark. An empty library is silent.
    let non_reflect = tasks.iter().filter(|task| !task.is_reflect).count();
    if non_reflect > 0 && watermark.is_none() {
        push_signal(
            &mut signals,
            ANOMALY_NEVER_REFLECTED,
            0,
            "",
            "",
            format!("tasks={non_reflect} qualifying_reflect=0"),
        );
    }

    // Stage 7 §A: repeated rejected/cleared flips (read-only, WARN level).
    signals.extend(qualification_flips.iter().cloned());

    // Fixed deterministic order: byte order of the kind, then anchor, then id.
    signals.sort_by(|left, right| {
        left.kind
            .cmp(&right.kind)
            .then_with(|| left.anchor_seq.cmp(&right.anchor_seq))
            .then_with(|| left.candidate_id.cmp(&right.candidate_id))
    });
    signals
}

struct Scan {
    watermark: Option<Watermark>,
    signals: Vec<AnomalySignal>,
}

fn scan(conn: &Connection, now: DateTime<FixedOffset>) -> Result<Scan, LearningError> {
    let tasks = load_tasks(conn, now)?;
    let reflect_ids: BTreeSet<String> = tasks
        .iter()
        .filter(|task| task.is_reflect)
        .map(|task| task.candidate_id.clone())
        .collect();
    let watermark = watermark_of(&tasks);
    let actor_events = load_actor_events(conn, &reflect_ids)?;
    let qualification_flips = collect_qualification_flips(conn, now)?;
    let signals = collect_signals(&tasks, &actor_events, &watermark, &qualification_flips);
    Ok(Scan { watermark, signals })
}

fn default_since(watermark: &Option<Watermark>) -> i64 {
    watermark
        .as_ref()
        .map(|watermark| watermark.event_seq)
        .unwrap_or(0)
}

fn count_since(signals: &[AnomalySignal], since: i64) -> usize {
    signals
        .iter()
        .filter(|signal| is_environmental(&signal.kind) || signal.signal_seq > since)
        .count()
}

/// The number of signals not yet covered by the watermark (the default
/// `learning anomalies` report, environment signal included).
///
/// This is the one counter shared with `learning inject`'s pending-signal line
/// and with 90 `AM-REFLECT`, so all three surfaces always agree.
pub fn unreviewed_total(
    store: &LearningStore,
    now: DateTime<FixedOffset>,
) -> Result<usize, LearningError> {
    let scan = scan(store.connection(), now)?;
    let since = default_since(&scan.watermark);
    Ok(count_since(&scan.signals, since))
}

/// The text form: one line per signal, then one summary line. LF only, no
/// trailing spaces, exactly one final newline.
pub fn render_anomalies_text(report: &AnomaliesReport) -> String {
    let mut text = String::new();
    for signal in &report.signals {
        text.push_str(&signal.kind);
        text.push_str(&format!(
            " seq={} anchor={} candidate={} actor={}",
            signal.signal_seq,
            signal.anchor_seq,
            if signal.candidate_id.is_empty() {
                "-"
            } else {
                signal.candidate_id.as_str()
            },
            if signal.actor.is_empty() {
                "-"
            } else {
                signal.actor.as_str()
            },
        ));
        if !signal.detail.is_empty() {
            text.push(' ');
            text.push_str(&signal.detail);
        }
        text.push('\n');
    }
    let watermark = report
        .watermark
        .as_ref()
        .map(|watermark| format!("{}:{}", watermark.event_seq, watermark.task_id))
        .unwrap_or_else(|| "none".to_string());
    let by_kind = if report.by_kind.is_empty() {
        "none".to_string()
    } else {
        report
            .by_kind
            .iter()
            .map(|(kind, count)| format!("{kind}={count}"))
            .collect::<Vec<_>>()
            .join(",")
    };
    text.push_str(&format!(
        "anomalies: {} signal(s), unreviewed {}, since_seq {}, watermark {}, by_kind {}\n",
        report.signals.len(),
        report.unreviewed_total,
        report.since_seq,
        watermark,
        by_kind,
    ));
    text
}

impl LearningStore {
    /// `learning anomalies`: the deterministic signal list of this store.
    /// Strictly read-only.
    ///
    /// `since_seq` defaults to the watermark. `unreviewed_total` is always the
    /// watermark-based default count, so an explicit `--since-seq` changes the
    /// reported list without changing the pending count.
    pub fn anomalies(
        &self,
        since_seq: Option<i64>,
        now: DateTime<FixedOffset>,
    ) -> Result<AnomaliesReport, LearningError> {
        let scan = scan(self.connection(), now)?;
        let since = since_seq.unwrap_or_else(|| default_since(&scan.watermark));
        let reported: Vec<AnomalySignal> = scan
            .signals
            .iter()
            .filter(|signal| is_environmental(&signal.kind) || signal.signal_seq > since)
            .cloned()
            .collect();
        let mut by_kind: BTreeMap<String, usize> = BTreeMap::new();
        for signal in &reported {
            *by_kind.entry(signal.kind.clone()).or_insert(0) += 1;
        }
        let mut report = AnomaliesReport {
            ok: true,
            root: self.root().to_string_lossy().into_owned(),
            watermark: scan.watermark.clone(),
            since_seq: since,
            signals: reported,
            unreviewed_total: count_since(&scan.signals, default_since(&scan.watermark)),
            by_kind,
            text: String::new(),
        };
        report.text = render_anomalies_text(&report);
        Ok(report)
    }
}
