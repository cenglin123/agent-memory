//! Stage 3 S5: per-target mechanical requirement evaluation.
//!
//! `learning gate-target` answers one question for one candidate: *which of
//! the mechanical requirements of its target kind are already met?* It is
//! read-only. It never writes the learning store, the Archive or a target, and
//! its best verdict — [`VERDICT_READY`] — only means "the mechanical
//! requirements are met, so this may be submitted for human approval". It is
//! **not** a promotion: the store still has no promotion command and the
//! candidate status stays `candidate`.
//!
//! # Target matrix
//!
//! | target_kind | mechanical requirements |
//! |---|---|
//! | `feedback` | a source-gated owner+interactive statement (`user_statement`/`correction`/`approval`), a recorded dedup disposition, contradicting evidence listed |
//! | `user_profile` | every (non-counter) evidence item is owner+interactive (and, when a source gate is supplied, gated), dedup recorded, at least one `user_statement`/`correction` |
//! | `workflow` | at least two reuse records with distinct task ids, every outcome `pass`, non-empty `applicability`, dedup recorded |
//! | `skill` | the workflow requirements, at least one validation with every verdict `pass`, plus two **manual** items (cc-switch single source, no host copy written) |
//! | `note` | non-empty evidence (all gated when a source gate is supplied), dedup recorded |
//! | `rule` | none: the verdict is always [`VERDICT_PROPOSAL_ONLY`] |
//!
//! # Source gate
//!
//! `--archive/--domain` is optional. With it, the S2 gate runs first and every
//! item verdict is embedded in the report as `source_gate`; a candidate whose
//! evidence fails the source gate is degraded to
//! [`VERDICT_INSUFFICIENT`] with [`BLOCKED_BY_SOURCE_GATE`] in `blocking`.
//! Without it, `source_gate.status` is `skipped` and gate-dependent
//! requirements are `skipped` (a requirement that is not `met` keeps the
//! verdict at `insufficient_evidence`).
//!
//! # Statuses and verdicts
//!
//! Requirement status is one of `met | unmet | manual | skipped`. `manual`
//! marks a process gate that cannot be decided mechanically (it does not by
//! itself block human approval); `skipped` marks a mechanical requirement that
//! could not be evaluated because no source gate was supplied, and therefore
//! blocks. Only `ready_for_approval`, `insufficient_evidence` and
//! `proposal_only` are verdicts; `blocked_by_source_gate` is a `blocking` code.
//!
//! # Stage 6 §C
//!
//! The review package carries [`ReviewRouting`]: the normalized conflict set,
//! prior reviewers, related tasks (with their current projection), related
//! knowledge candidates, context refs and the fixed reviewer requirements. The
//! conflict set is computed from the store — never from the package — so
//! `learning validate` recomputes it and refuses an explicit self-review
//! ([`super::events::REVIEWER_CONFLICT`]). A task candidate still has no review
//! package ([`TASK_NOT_ROUTEABLE`]).

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, FixedOffset};
use rusqlite::{params, Connection};
use serde::Serialize;
use serde_json::Value;

use super::gate::{GateReport, VERDICT_PASSED};
use super::inject::fold_for_markers;
use super::model::{CandidateRecord, EvidenceItem, TASK_TARGET_KIND};
use super::store::{self, CandidateView, LearningStore};
use super::task::{self, project_task_state, TASK_NOT_ROUTEABLE};
use super::LearningError;

pub const VERDICT_READY: &str = "ready_for_approval";
pub const VERDICT_INSUFFICIENT: &str = "insufficient_evidence";
pub const VERDICT_PROPOSAL_ONLY: &str = "proposal_only";
/// Stage 7 §A: the candidate's newest qualification is `rejected`.
pub const VERDICT_REJECTED: &str = "rejected";
/// Stage 7 §A: the candidate's newest qualification is `deferred`.
pub const VERDICT_DEFERRED: &str = "deferred";

/// `blocking` code for a rejected qualification.
pub const BLOCKED_QUALIFICATION_REJECTED: &str = "qualification_rejected";
/// `blocking` code for a deferred qualification.
pub const BLOCKED_QUALIFICATION_DEFERRED: &str = "qualification_deferred";

pub const STATUS_MET: &str = "met";
pub const STATUS_UNMET: &str = "unmet";
pub const STATUS_MANUAL: &str = "manual";
pub const STATUS_SKIPPED: &str = "skipped";

pub const SOURCE_GATE_PASSED: &str = "passed";
pub const SOURCE_GATE_FAILED: &str = "failed";
pub const SOURCE_GATE_SKIPPED: &str = "skipped";

/// `blocking` code for "the supplied source gate did not pass".
pub const BLOCKED_BY_SOURCE_GATE: &str = "blocked_by_source_gate";
/// `blocking` code for a `rule` target (there is no mechanical ready path).
pub const BLOCKED_RULE_GOVERNANCE: &str = "rule_requires_governance";

/// Evidence kinds that can carry owner testimony (the S2 `owner_backed` set).
const OWNER_BACKED_KINDS: [&str; 3] = ["user_statement", "correction", "approval"];
/// Evidence kinds accepted for a `user_profile` identity statement.
const PROFILE_STATEMENT_KINDS: [&str; 2] = ["user_statement", "correction"];

/// One mechanical requirement line.
#[derive(Debug, Clone, Serialize)]
pub struct Requirement {
    pub requirement: String,
    pub status: &'static str,
    pub detail: String,
}

impl Requirement {
    fn new(requirement: &str, status: &'static str, detail: impl Into<String>) -> Self {
        Self {
            requirement: requirement.to_string(),
            status,
            detail: detail.into(),
        }
    }

    fn met(requirement: &str, detail: impl Into<String>) -> Self {
        Self::new(requirement, STATUS_MET, detail)
    }

    fn unmet(requirement: &str, detail: impl Into<String>) -> Self {
        Self::new(requirement, STATUS_UNMET, detail)
    }

    fn manual(requirement: &str, detail: impl Into<String>) -> Self {
        Self::new(requirement, STATUS_MANUAL, detail)
    }

    fn skipped(requirement: &str, detail: impl Into<String>) -> Self {
        Self::new(requirement, STATUS_SKIPPED, detail)
    }
}

/// The embedded source-gate result: `skipped` when no archive was supplied.
#[derive(Debug, Clone, Serialize)]
pub struct SourceGateSummary {
    pub status: &'static str,
    pub report: Option<GateReport>,
}

/// The single JSON report of `learning gate-target`.
#[derive(Debug, Clone, Serialize)]
pub struct TargetReport {
    /// True when the verdict is [`VERDICT_READY`] (the mechanical
    /// requirements are met). This is a verdict flag, not a process status:
    /// the command still exits 0 whenever a report is produced.
    pub ok: bool,
    pub candidate_id: String,
    pub target_kind: String,
    pub requirements: Vec<Requirement>,
    pub verdict: &'static str,
    pub blocking: Vec<String>,
    pub source_gate: SourceGateSummary,
    /// Stage 7 §A: the newest qualification (state `open` when there is none)
    /// and its history. A `rejected` / `deferred` state overrides the normal
    /// verdict and is never `ready_for_approval`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub qualification: Option<super::qualify::QualificationView>,
}

fn gate_item<'a>(
    gate: &'a GateReport,
    list: &str,
    index: usize,
) -> Option<&'a super::gate::GateItem> {
    gate.items
        .iter()
        .find(|item| item.list == list && item.index == index)
}

fn gate_passed(gate: &GateReport, list: &str, index: usize) -> bool {
    gate_item(gate, list, index)
        .map(|item| item.verdict == VERDICT_PASSED)
        .unwrap_or(false)
}

fn owner_interactive(item: &EvidenceItem) -> bool {
    item.origin_class == "owner" && item.session_kind == "interactive"
}

fn owner_backed(item: &EvidenceItem) -> bool {
    owner_interactive(item) && OWNER_BACKED_KINDS.contains(&item.evidence_kind.as_str())
}

fn reuse_entries(record: &CandidateRecord) -> Vec<&serde_json::Value> {
    record.reuse.iter().collect()
}

fn reuse_task(entry: &serde_json::Value) -> Option<&str> {
    entry.get("task_id").and_then(|value| value.as_str())
}

fn reuse_outcome(entry: &serde_json::Value) -> Option<&str> {
    entry.get("outcome").and_then(|value| value.as_str())
}

fn validation_verdict(entry: &serde_json::Value) -> Option<&str> {
    entry.get("verdict").and_then(|value| value.as_str())
}

fn dedup_requirement(record: &CandidateRecord) -> Requirement {
    if record.dedup.disposition != "pending" {
        Requirement::met(
            "dedup_recorded",
            format!(
                "dedup disposition {:?} recorded at {}",
                record.dedup.disposition,
                record.dedup.checked_at.as_deref().unwrap_or("unknown")
            ),
        )
    } else {
        Requirement::unmet(
            "dedup_recorded",
            "dedup.disposition is still \"pending\"; run `learning dedup` first",
        )
    }
}

fn contradicting_requirement(record: &CandidateRecord) -> Requirement {
    let refs: Vec<&str> = record
        .contradicting_evidence
        .iter()
        .map(|item| item.reference.as_str())
        .collect();
    Requirement::met(
        "contradicting_evidence_listed",
        if refs.is_empty() {
            "no contradicting evidence recorded (listing it is required, emptiness is not a failure)"
                .to_string()
        } else {
            format!(
                "{} contradicting evidence item(s) listed: {}",
                refs.len(),
                refs.join(", ")
            )
        },
    )
}

fn evidence_all_owner_interactive(
    record: &CandidateRecord,
    gate: Option<&GateReport>,
) -> Requirement {
    if record.evidence.is_empty() {
        return Requirement::unmet(
            "all_evidence_owner_interactive",
            "candidate has no evidence items",
        );
    }
    let mut offenders = Vec::new();
    for (index, item) in record.evidence.iter().enumerate() {
        if !owner_interactive(item) {
            offenders.push(format!(
                "evidence[{index}] is {}/{}",
                item.origin_class, item.session_kind
            ));
        } else if let Some(report) = gate {
            if !gate_passed(report, super::gate::LIST_EVIDENCE, index) {
                offenders.push(format!("evidence[{index}] did not pass the source gate"));
            }
        }
    }
    if offenders.is_empty() {
        let suffix = if gate.is_some() {
            " (source-gated)"
        } else {
            " (source gate skipped)"
        };
        Requirement::met(
            "all_evidence_owner_interactive",
            format!(
                "all {} evidence item(s) are owner+interactive{suffix}",
                record.evidence.len()
            ),
        )
    } else {
        Requirement::unmet("all_evidence_owner_interactive", offenders.join("; "))
    }
}

fn profile_statement_requirement(record: &CandidateRecord) -> Requirement {
    let count = record
        .evidence
        .iter()
        .filter(|item| PROFILE_STATEMENT_KINDS.contains(&item.evidence_kind.as_str()))
        .count();
    if count > 0 {
        Requirement::met(
            "owner_statement_or_correction",
            format!("{count} evidence item(s) are user_statement/correction"),
        )
    } else {
        Requirement::unmet(
            "owner_statement_or_correction",
            "no evidence item is user_statement or correction",
        )
    }
}

fn feedback_owner_requirement(record: &CandidateRecord, gate: Option<&GateReport>) -> Requirement {
    let Some(report) = gate else {
        return Requirement::skipped(
            "owner_backed_evidence",
            "source gate skipped: pass --archive/--domain to prove owner+interactive evidence",
        );
    };
    let backing = record
        .evidence
        .iter()
        .enumerate()
        .filter(|(index, item)| {
            owner_backed(item) && gate_passed(report, super::gate::LIST_EVIDENCE, *index)
        })
        .count();
    if backing > 0 {
        Requirement::met(
            "owner_backed_evidence",
            format!(
                "{backing} evidence item(s) passed the source gate as owner+interactive \
                 user_statement/correction/approval"
            ),
        )
    } else {
        Requirement::unmet(
            "owner_backed_evidence",
            "no evidence item passed the source gate as owner+interactive \
             user_statement/correction/approval",
        )
    }
}

fn reuse_distinct_tasks_requirement(record: &CandidateRecord) -> Requirement {
    let mut tasks: Vec<&str> = Vec::new();
    for entry in reuse_entries(record) {
        if let Some(task) = reuse_task(entry) {
            if !tasks.contains(&task) {
                tasks.push(task);
            }
        }
    }
    if tasks.len() >= 2 {
        Requirement::met(
            "reuse_distinct_tasks",
            format!(
                "{} distinct task(s) recorded: {}",
                tasks.len(),
                tasks.join(", ")
            ),
        )
    } else {
        Requirement::unmet(
            "reuse_distinct_tasks",
            format!(
                "{} distinct task(s) recorded; a workflow needs at least 2",
                tasks.len()
            ),
        )
    }
}

fn reuse_outcomes_requirement(record: &CandidateRecord) -> Requirement {
    let entries = reuse_entries(record);
    if entries.is_empty() {
        return Requirement::unmet(
            "reuse_all_passed",
            "no reuse records; nothing shows the workflow was applied successfully",
        );
    }
    let failures: Vec<&str> = entries
        .iter()
        .filter(|entry| reuse_outcome(entry) != Some("pass"))
        .filter_map(|entry| reuse_task(entry))
        .collect();
    if failures.is_empty() {
        Requirement::met(
            "reuse_all_passed",
            format!("all {} reuse outcome(s) are pass", entries.len()),
        )
    } else {
        Requirement::unmet(
            "reuse_all_passed",
            format!("non-pass reuse task(s): {}", failures.join(", ")),
        )
    }
}

fn applicability_requirement(record: &CandidateRecord) -> Requirement {
    let values: Vec<&str> = record
        .applicability
        .iter()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .collect();
    if values.is_empty() {
        Requirement::unmet(
            "applicability_present",
            "applicability is empty; a workflow must state where it applies",
        )
    } else {
        Requirement::met(
            "applicability_present",
            format!("applicability: {}", values.join(", ")),
        )
    }
}

fn validation_requirement(record: &CandidateRecord) -> Requirement {
    let entries = &record.validation;
    if entries.is_empty() {
        return Requirement::unmet(
            "validation_present_and_passed",
            "no validation record; an independent validation with verdict pass is required",
        );
    }
    let failures: Vec<&str> = entries
        .iter()
        .filter(|entry| validation_verdict(entry) != Some("pass"))
        .filter_map(|entry| entry.get("reviewer").and_then(|value| value.as_str()))
        .collect();
    if failures.is_empty() {
        Requirement::met(
            "validation_present_and_passed",
            format!("{} validation record(s), all verdict=pass", entries.len()),
        )
    } else {
        Requirement::unmet(
            "validation_present_and_passed",
            format!(
                "{} validation record(s); non-pass reviewer(s): {}",
                entries.len(),
                failures.join(", ")
            ),
        )
    }
}

fn workflow_requirements(record: &CandidateRecord) -> Vec<Requirement> {
    vec![
        reuse_distinct_tasks_requirement(record),
        reuse_outcomes_requirement(record),
        applicability_requirement(record),
        dedup_requirement(record),
    ]
}

fn source_gate_summary(gate: Option<&GateReport>) -> SourceGateSummary {
    match gate {
        Some(report) => SourceGateSummary {
            status: if report.ok {
                SOURCE_GATE_PASSED
            } else {
                SOURCE_GATE_FAILED
            },
            report: Some(report.clone()),
        },
        None => SourceGateSummary {
            status: SOURCE_GATE_SKIPPED,
            report: None,
        },
    }
}

/// Evaluate the mechanical requirements of `record` for its target kind.
///
/// `gate` is the S2 report for the same candidate, or `None` when no archive
/// was supplied (`source_gate.status = skipped`).
///
/// A `target_kind = "task"` candidate is never routeable: it stays a
/// candidate, has no review package and no mechanical ready path, so the
/// report is `ok:false` with [`TASK_NOT_ROUTEABLE`] in `blocking` (R11). The
/// function signature is unchanged — the refusal is carried by the report.
pub fn evaluate(record: &CandidateRecord, gate: Option<&GateReport>) -> TargetReport {
    evaluate_with_qualification(record, gate, None)
}

/// [`evaluate`] with the stage-7 §A qualification of the candidate.
///
/// A newest state of `rejected` or `deferred` overrides the mechanical verdict
/// ([`VERDICT_REJECTED`] / [`VERDICT_DEFERRED`]) and is never
/// `ready_for_approval`; `open` / `cleared` leave the mechanical verdict
/// unchanged. `cleared` is reported as-is in `qualification`.
pub fn evaluate_with_qualification(
    record: &CandidateRecord,
    gate: Option<&GateReport>,
    qualification: Option<&super::qualify::QualificationView>,
) -> TargetReport {
    if record.target_kind == TASK_TARGET_KIND {
        return TargetReport {
            ok: false,
            candidate_id: record.candidate_id.clone(),
            target_kind: record.target_kind.clone(),
            requirements: vec![Requirement::unmet(
                "task_not_routeable",
                "a workspace task is never routed to a target: it remains a candidate, has no \
                 review package and no mechanical ready path",
            )],
            verdict: VERDICT_INSUFFICIENT,
            blocking: vec![TASK_NOT_ROUTEABLE.to_string()],
            source_gate: source_gate_summary(gate),
            qualification: None,
        };
    }

    let mut requirements = match record.target_kind.as_str() {
        "feedback" => vec![
            feedback_owner_requirement(record, gate),
            dedup_requirement(record),
            contradicting_requirement(record),
        ],
        "user_profile" => vec![
            evidence_all_owner_interactive(record, gate),
            dedup_requirement(record),
            profile_statement_requirement(record),
        ],
        "workflow" => workflow_requirements(record),
        "skill" => {
            let mut requirements = workflow_requirements(record);
            requirements.push(validation_requirement(record));
            requirements.push(Requirement::manual(
                "cc_switch_single_source",
                "manual: confirm cc-switch is the single enabled source for this skill; \
                 not mechanically checkable here",
            ));
            requirements.push(Requirement::manual(
                "host_copy_not_written",
                "manual: confirm no host skill copy is written; distribution is manual",
            ));
            requirements
        }
        "note" => {
            let evidence = if record.evidence.is_empty() {
                Requirement::unmet("evidence_present", "candidate has no evidence items")
            } else if let Some(report) = gate {
                let failed = record
                    .evidence
                    .iter()
                    .enumerate()
                    .filter(|(index, _)| !gate_passed(report, super::gate::LIST_EVIDENCE, *index))
                    .count();
                if failed == 0 {
                    Requirement::met(
                        "evidence_present",
                        format!(
                            "{} evidence item(s) present and all passed the source gate",
                            record.evidence.len()
                        ),
                    )
                } else {
                    Requirement::unmet(
                        "evidence_present",
                        format!("{failed} evidence item(s) failed the source gate"),
                    )
                }
            } else {
                Requirement::met(
                    "evidence_present",
                    format!(
                        "{} evidence item(s) present (source gate skipped)",
                        record.evidence.len()
                    ),
                )
            };
            vec![evidence, dedup_requirement(record)]
        }
        // `rule` has no mechanical ready path: the only path is the governance
        // process plus explicit user approval, never a direct deployment.
        "rule" => vec![Requirement::unmet(
            "mechanical_ready_path",
            "常驻规则必须走对应治理流程与用户确认，绝不直接部署（no mechanical ready path）",
        )],
        // `model::validate_candidate` rejects any other kind, so a stored
        // record cannot reach this arm; the report still fails closed.
        other => vec![Requirement::unmet(
            "known_target_kind",
            format!("unknown target_kind {other:?}; no requirements can be evaluated"),
        )],
    };

    let gate_failed = gate.map(|report| !report.ok).unwrap_or(false);
    if gate_failed {
        // Make the degradation visible at the front of the requirement list.
        requirements.insert(
            0,
            Requirement::unmet(
                "source_gate",
                "the supplied source gate did not pass; the target evaluation is degraded",
            ),
        );
    }

    let source_gate = source_gate_summary(gate);

    let (mut verdict, mut blocking) = if record.target_kind == "rule" {
        (
            VERDICT_PROPOSAL_ONLY,
            vec![BLOCKED_RULE_GOVERNANCE.to_string()],
        )
    } else {
        let mut blocking: Vec<String> = Vec::new();
        if gate_failed {
            blocking.push(BLOCKED_BY_SOURCE_GATE.to_string());
        }
        for requirement in &requirements {
            if requirement.status == STATUS_UNMET || requirement.status == STATUS_SKIPPED {
                if !blocking.contains(&requirement.requirement) {
                    blocking.push(requirement.requirement.clone());
                }
            }
        }
        if blocking.is_empty() {
            (VERDICT_READY, blocking)
        } else {
            (VERDICT_INSUFFICIENT, blocking)
        }
    };

    if let Some(qualification) = qualification {
        match qualification.state.as_str() {
            super::qualify::STATE_REJECTED => {
                verdict = VERDICT_REJECTED;
                if !blocking.iter().any(|code| code == BLOCKED_QUALIFICATION_REJECTED) {
                    blocking.insert(0, BLOCKED_QUALIFICATION_REJECTED.to_string());
                }
                requirements.insert(
                    0,
                    Requirement::unmet(
                        "qualification_state",
                        qualification
                            .latest
                            .as_ref()
                            .map(|row| {
                                format!(
                                    "candidate is rejected (basis: {}; counterexample: {})",
                                    row.basis, row.counterexample
                                )
                            })
                            .unwrap_or_else(|| "candidate is rejected".to_string()),
                    ),
                );
            }
            super::qualify::STATE_DEFERRED => {
                verdict = VERDICT_DEFERRED;
                if !blocking.iter().any(|code| code == BLOCKED_QUALIFICATION_DEFERRED) {
                    blocking.insert(0, BLOCKED_QUALIFICATION_DEFERRED.to_string());
                }
                requirements.insert(
                    0,
                    Requirement::unmet(
                        "qualification_state",
                        qualification
                            .latest
                            .as_ref()
                            .map(|row| {
                                format!(
                                    "candidate is deferred (owner: {}; trigger: {})",
                                    row.owner, row.trigger
                                )
                            })
                            .unwrap_or_else(|| "candidate is deferred".to_string()),
                    ),
                );
            }
            _ => {}
        }
    }

    TargetReport {
        ok: verdict == VERDICT_READY,
        candidate_id: record.candidate_id.clone(),
        target_kind: record.target_kind.clone(),
        requirements,
        verdict,
        blocking,
        source_gate,
        qualification: qualification.cloned(),
    }
}

// ---------------------------------------------------------------------------
// Stage 6 §C: review routing (conflict set, related material, requirements)
// ---------------------------------------------------------------------------

/// Fixed reviewer requirements carried by every review package. This is
/// material for a human dispatcher, never a routing decision: the store does
/// not choose or spawn a reviewer.
pub const REVIEWER_REQUIREMENTS: [&str; 4] = [
    "fresh context",
    "not in conflict_actors",
    "not the main-session model",
    "advisory only; approval is human",
];

/// `related_tasks[].state` of an id that names no task candidate.
pub const RELATED_TASK_MISSING: &str = "missing";

/// One task named by a knowledge candidate's `origin_tasks` or `reuse`
/// records, with its current projection.
#[derive(Debug, Clone, Serialize)]
pub struct RelatedTask {
    pub task_id: String,
    /// The current projection (`active` / `done` / `abandoned` / `stale`) or
    /// [`RELATED_TASK_MISSING`] for a dangling id.
    pub state: String,
    /// The task claim, or `null` when the id is missing.
    pub claim: Option<String>,
}

/// Stage 6 §C review-routing material of one knowledge candidate. Every list
/// is always present (an empty one serializes as `[]`), because "no conflict
/// recorded" is itself information the reviewer needs.
#[derive(Debug, Clone, Serialize)]
pub struct ReviewRouting {
    /// Normalized actors the reviewer must not be: the candidate authors
    /// (record `extractor` and every `ingestions.extractor`), every
    /// `evidence[].actor`, every `reuse_history.actor_id` of this candidate,
    /// and each origin task's creator, every claim actor and every reuse actor.
    /// A task's validation reviewers are deliberately excluded: a reviewer of
    /// the task is not a participant of the task.
    pub conflict_actors: Vec<String>,
    /// Reviewers of this candidate's previous validation rounds, in their
    /// stored form, deduped and sorted.
    pub prior_reviewers: Vec<String>,
    /// `origin_tasks` plus every `reuse` task id that names a task candidate,
    /// sorted by task id. Other free-text reuse task ids are historical and are
    /// not listed.
    pub related_tasks: Vec<RelatedTask>,
    /// Other knowledge candidates sharing an origin task, a reuse task id or
    /// an evidence ref, sorted and deduped.
    pub related_candidates: Vec<String>,
    /// The candidate's evidence refs, in first-seen order, deduped.
    pub context_refs: Vec<String>,
    pub reviewer_requirements: [&'static str; 4],
}

/// The one review-identity normalization shared by the routing surface, the
/// validation guard and the watermark: trim, then the stage-5 detection fold
/// (fullwidth ASCII / ideographic space to ASCII, then lower-case).
pub fn review_identity(value: &str) -> String {
    fold_for_markers(value.trim())
}

fn push_actor(actors: &mut BTreeSet<String>, value: &str) {
    let normalized = review_identity(value);
    if !normalized.is_empty() {
        actors.insert(normalized);
    }
}

fn column_strings(conn: &Connection, sql: &str, id: &str) -> Result<Vec<String>, LearningError> {
    let mut stmt = conn.prepare(sql).map_err(store::db_error)?;
    let rows = stmt
        .query_map(params![id], |row| row.get::<_, String>(0))
        .map_err(store::db_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(store::db_error)?;
    Ok(rows)
}

/// The normalized conflict set of one candidate, recomputed from the store.
///
/// This is the single definition used by `learning export` (to render
/// `review_routing.conflict_actors`) and by `learning validate` (to refuse a
/// self-review). The store — not the review package — is authoritative: a
/// caller cannot clear its own conflicts by editing a package.
pub(crate) fn conflict_actors(
    conn: &Connection,
    record: &CandidateRecord,
) -> Result<BTreeSet<String>, LearningError> {
    let mut actors = BTreeSet::new();
    push_actor(&mut actors, &record.extractor);
    for extractor in column_strings(
        conn,
        "SELECT extractor FROM ingestions WHERE candidate_id = ?1 ORDER BY revision, ingest_key",
        &record.candidate_id,
    )? {
        push_actor(&mut actors, &extractor);
    }
    for item in &record.evidence {
        push_actor(&mut actors, &item.actor);
    }
    for actor in column_strings(
        conn,
        "SELECT actor_id FROM reuse_history WHERE candidate_id = ?1 ORDER BY seq",
        &record.candidate_id,
    )? {
        push_actor(&mut actors, &actor);
    }
    let origins: BTreeSet<&str> = record.origin_tasks.iter().map(String::as_str).collect();
    for origin in origins {
        for actor in column_strings(
            conn,
            "SELECT actor_id FROM task_events WHERE candidate_id = ?1 AND event_kind = 'claim' \
             ORDER BY event_seq",
            origin,
        )? {
            push_actor(&mut actors, &actor);
        }
        for actor in column_strings(
            conn,
            "SELECT actor_id FROM reuse_history WHERE candidate_id = ?1 OR task_id = ?1 \
             ORDER BY seq",
            origin,
        )? {
            push_actor(&mut actors, &actor);
        }
    }
    Ok(actors)
}

/// One scan of `candidates` that builds the three relatedness maps at once, so
/// the routing surface never rescans the table per candidate.
#[derive(Debug, Default)]
struct RealmIndex {
    /// task candidate id -> claim (only `target_kind = "task"` rows).
    tasks: BTreeMap<String, String>,
    by_origin: BTreeMap<String, Vec<String>>,
    by_reuse_task: BTreeMap<String, Vec<String>>,
    by_evidence_ref: BTreeMap<String, Vec<String>>,
}

fn index_realm(conn: &Connection, skip: &str) -> Result<RealmIndex, LearningError> {
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

    let mut index = RealmIndex::default();
    for (candidate_id, record_json) in rows {
        let value: Value = serde_json::from_str(&record_json).map_err(|error| {
            LearningError::schema_invalid(format!(
                "stored candidate {candidate_id} is not readable: {error}"
            ))
        })?;
        let target_kind = value
            .get("target_kind")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if target_kind == TASK_TARGET_KIND {
            let claim = value.get("claim").and_then(Value::as_str).unwrap_or("");
            index.tasks.insert(candidate_id, claim.to_string());
            continue;
        }
        if candidate_id == skip {
            continue;
        }
        if let Some(list) = value.get("origin_tasks").and_then(Value::as_array) {
            for origin in list.iter().filter_map(Value::as_str) {
                index
                    .by_origin
                    .entry(origin.to_string())
                    .or_default()
                    .push(candidate_id.clone());
            }
        }
        if let Some(list) = value.get("reuse").and_then(Value::as_array) {
            for entry in list {
                if let Some(task_id) = entry.get("task_id").and_then(Value::as_str) {
                    index
                        .by_reuse_task
                        .entry(task_id.to_string())
                        .or_default()
                        .push(candidate_id.clone());
                }
            }
        }
        if let Some(list) = value.get("evidence").and_then(Value::as_array) {
            for entry in list {
                if let Some(reference) = entry.get("ref").and_then(Value::as_str) {
                    index
                        .by_evidence_ref
                        .entry(reference.to_string())
                        .or_default()
                        .push(candidate_id.clone());
                }
            }
        }
    }
    for ids in index
        .by_origin
        .values_mut()
        .chain(index.by_reuse_task.values_mut())
        .chain(index.by_evidence_ref.values_mut())
    {
        ids.sort();
        ids.dedup();
    }
    Ok(index)
}

fn related_tasks(
    conn: &Connection,
    record: &CandidateRecord,
    index: &RealmIndex,
    now: DateTime<FixedOffset>,
    stale_days: u32,
) -> Result<Vec<RelatedTask>, LearningError> {
    let mut ids: BTreeSet<&str> = record.origin_tasks.iter().map(String::as_str).collect();
    for entry in &record.reuse {
        if let Some(task_id) = entry.get("task_id").and_then(Value::as_str) {
            if index.tasks.contains_key(task_id) {
                ids.insert(task_id);
            }
        }
    }
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        if let Some(claim) = index.tasks.get(id) {
            let events = task::load_task_events(conn, id)?;
            let projection = project_task_state(&events, now, stale_days);
            out.push(RelatedTask {
                task_id: id.to_string(),
                state: projection.state,
                claim: Some(claim.clone()),
            });
        } else {
            out.push(RelatedTask {
                task_id: id.to_string(),
                state: RELATED_TASK_MISSING.to_string(),
                claim: None,
            });
        }
    }
    Ok(out)
}

fn review_routing_values(
    conn: &Connection,
    record: &CandidateRecord,
    now: DateTime<FixedOffset>,
    stale_days: u32,
) -> Result<ReviewRouting, LearningError> {
    let conflict_actors: Vec<String> = conflict_actors(conn, record)?.into_iter().collect();

    let mut prior_reviewers = column_strings(
        conn,
        "SELECT reviewer FROM validation_history WHERE candidate_id = ?1 ORDER BY seq",
        &record.candidate_id,
    )?;
    prior_reviewers.sort();
    prior_reviewers.dedup();

    let mut context_refs = Vec::new();
    let mut seen_refs: BTreeSet<&str> = BTreeSet::new();
    for item in &record.evidence {
        if seen_refs.insert(item.reference.as_str()) {
            context_refs.push(item.reference.clone());
        }
    }

    let index = index_realm(conn, &record.candidate_id)?;
    let related_tasks = related_tasks(conn, record, &index, now, stale_days)?;

    let mut related: BTreeSet<&str> = BTreeSet::new();
    for origin in &record.origin_tasks {
        if let Some(ids) = index.by_origin.get(origin) {
            related.extend(ids.iter().map(String::as_str));
        }
    }
    for entry in &record.reuse {
        if let Some(task_id) = entry.get("task_id").and_then(Value::as_str) {
            if let Some(ids) = index.by_reuse_task.get(task_id) {
                related.extend(ids.iter().map(String::as_str));
            }
        }
    }
    for item in &record.evidence {
        if let Some(ids) = index.by_evidence_ref.get(&item.reference) {
            related.extend(ids.iter().map(String::as_str));
        }
    }
    let related_candidates: Vec<String> = related.into_iter().map(str::to_string).collect();

    Ok(ReviewRouting {
        conflict_actors,
        prior_reviewers,
        related_tasks,
        related_candidates,
        context_refs,
        reviewer_requirements: REVIEWER_REQUIREMENTS,
    })
}

impl LearningStore {
    /// Stage 6 §C: the review-routing material of one stored knowledge
    /// candidate, recomputed from the store at `now`. Read-only.
    pub fn review_routing(
        &self,
        candidate_id: &str,
        now: DateTime<FixedOffset>,
        stale_days: u32,
    ) -> Result<ReviewRouting, LearningError> {
        let id = candidate_id.trim();
        if id.is_empty() {
            return Err(LearningError::invalid_candidate(
                "--candidate-id must be a non-empty candidate id",
            ));
        }
        let Some((record, _)) = store::load_current(self.connection(), id)? else {
            return Err(LearningError::not_found(format!(
                "no candidate {id} in this learning root"
            )));
        };
        review_routing_values(self.connection(), &record, now, stale_days)
    }
}

/// Fixed top-level field of every review package.
pub const DECISION_REQUIRED: &str = "human approval";
/// The only promotion path a `rule` target may carry.
pub const RULE_PROMOTION_PATH: &str = "none — proposal only";

/// Full review package of `learning export`: the show-level candidate plus the
/// target requirement report, export metadata and (stage 6 §C) the review
/// routing material.
#[derive(Debug, Clone, Serialize)]
pub struct ReviewPackage {
    pub decision_required: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub promotion_path: Option<&'static str>,
    pub exported_at: String,
    pub tool_version: &'static str,
    pub ok: bool,
    pub candidate_id: String,
    pub candidate: CandidateRecord,
    pub payload_sha256: String,
    pub ingestions: Vec<super::store::StoredIngestion>,
    pub revisions: Vec<super::store::RevisionInfo>,
    pub status_history: Vec<super::store::StatusEntry>,
    pub dedup: super::model::DedupState,
    pub reuse: Vec<serde_json::Value>,
    pub validation: Vec<serde_json::Value>,
    pub target_report: TargetReport,
    /// Stage 6 §C: routing material for the human dispatcher. Always present
    /// for a knowledge candidate (empty lists serialize as `[]`).
    pub review_routing: ReviewRouting,
    /// Stage 7 §A: the newest qualification and its full history.
    pub qualification: super::qualify::QualificationView,
}

/// Build the proposal document. It always requires human approval; a `rule`
/// additionally carries `promotion_path: "none — proposal only"`.
pub fn review_package(
    view: &CandidateView,
    target_report: TargetReport,
    exported_at: String,
    review_routing: ReviewRouting,
) -> ReviewPackage {
    let rule = view.candidate.target_kind == "rule";
    ReviewPackage {
        decision_required: DECISION_REQUIRED,
        promotion_path: if rule {
            Some(RULE_PROMOTION_PATH)
        } else {
            None
        },
        exported_at,
        tool_version: env!("CARGO_PKG_VERSION"),
        ok: true,
        candidate_id: view.candidate.candidate_id.clone(),
        candidate: view.candidate.clone(),
        payload_sha256: view.payload_sha256.clone(),
        ingestions: view.ingestions.clone(),
        revisions: view.revisions.clone(),
        status_history: view.status_history.clone(),
        dedup: view.candidate.dedup.clone(),
        reuse: view.candidate.reuse.clone(),
        validation: view.candidate.validation.clone(),
        target_report,
        review_routing,
        qualification: view.qualification.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_gate_marks_the_source_gate_skipped() {
        assert_eq!(SOURCE_GATE_SKIPPED, "skipped");
        assert_eq!(VERDICT_READY, "ready_for_approval");
        assert_eq!(VERDICT_INSUFFICIENT, "insufficient_evidence");
        assert_eq!(VERDICT_PROPOSAL_ONLY, "proposal_only");
        assert_eq!(RULE_PROMOTION_PATH, "none — proposal only");
    }
}
