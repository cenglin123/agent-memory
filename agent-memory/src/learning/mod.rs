//! Stage 3 learning: candidate drafts (S1), a read-only source gate (S2) and the
//! dedup / supersession / gated-ingest mechanics (S3).
//!
//! S1 ([`model`], [`store`]) persists *candidate drafts* only. It never reads
//! an Archive, a recall configuration, a full-text/vector index or a long-term
//! memory file, and it never promotes a candidate: the single status S1
//! understands is [`model::CANDIDATE_STATUS`].
//!
//! S2 ([`gate`], [`provenance`]) is a read-only layer on top of an explicit
//! learning root and an explicit committed archive root. It reads candidate
//! evidence back from the archive, verifies span/generation/provenance and
//! reports verdicts. It writes neither store and never changes a candidate's
//! status, dedupe, reuse or promotion state.
//!
//! S3 ([`dedup`]) records dedup dispositions as append-only history, makes the
//! S2 gate an optional precondition of `learning ingest`, excludes `dedup` from
//! the same-revision comparison set (it is workflow state) and requires a
//! `revision = current + 1` submission to carry a non-decreasing `updated_at`.
//!
//! S5 ([`events`], [`routing`]) records reuse and independent-validation
//! events as append-only history and evaluates the mechanical requirements of
//! each of the six target kinds (`learning gate-target`, `learning export`).
//! A `ready_for_approval` verdict means "submit for human approval"; it is not
//! a promotion and never changes the candidate status.
//!
//! Stage 4 ([`task`]) makes a workspace task a first-class candidate
//! (`target_kind = "task"`, always `candidate`, never exportable). Task state is
//! never stored: [`task::project_task_state`] derives `active`/`done`/
//! `abandoned`/`stale` from the `task_events`, `reuse_history` and
//! `validation_history` rows of one candidate, ordered by the global
//! `event_seq`. `learning task create/claim/abandon` are the only task write
//! paths; `learning ingest` refuses task payloads.
//!
//! The read side is `learning workspace` (every task, projected, ordered by the
//! newest `event_seq`) and `learning task show` (one task plus its full unified
//! event view). Both are strictly read-only and never append to `event_clock`.
//! Isolation (R11): `learning list` hides task candidates unless
//! `--include-tasks` is given, and `gate` / `dedup` / `gate-target` / `export`
//! refuse a task candidate with [`task::TASK_NOT_ROUTEABLE`] (usage class for
//! `gate`/`dedup`, `ok:false` report for the other two).
//!
//! Stage 5 ([`inject`]) renders the bounded, read-only coordination block
//! (`learning inject`) from the same projection: the positioning statement is
//! one constant ([`inject::INJECT_NOTICE`]) reused by `inject`, `workspace` and
//! `task show`; task blocks are truncated whole under hard caps; a claim may
//! carry non-blocking visibility markers; and stored task rows that violate the
//! stage-5 write-side rules are skipped whole (listed by `inject --check`).
//!
//! Stage 6 ([`anomalies`]) adds the read-only anomaly-signal surface
//! (`learning anomalies`): seven fixed kinds over events, task state and the
//! library as a whole, a `[self-reflect]` task watermark with no new storage,
//! and one `unreviewed_total` counter shared with `learning inject` and 90
//! `AM-REFLECT`. It reuses the stage-5 marker classifier and the one legacy
//! write-side predicate rather than duplicating either.
//!
//! Stage 6 §D adds the optional knowledge-candidate field `origin_tasks`
//! (task ids the candidate distills experience from): validated on the ingest
//! write path (count, duplicates, existence, target kind, claim-copy lint),
//! never part of the candidate identity or ingest key, read back as the
//! derived `distilled_into` list on `workspace` / `task show`, and guarded by a
//! fail-closed `learning migrate --to 3` refusal
//! ([`store::DOWNGRADE_BLOCKED_ORIGIN_TASKS`]) once any stored record carries
//! the key.
//!
//! Stage 6 §C adds review routing and self-review interception: `learning
//! export` carries [`routing::ReviewRouting`] (the normalized conflict set,
//! prior reviewers, related tasks with their current projection, related
//! knowledge candidates, context refs and the fixed reviewer requirements),
//! and `learning validate` recomputes the conflict set from the store to refuse
//! an explicit self-review ([`events::REVIEWER_CONFLICT`]); a `[self-reflect]`
//! task additionally refuses a creator review and requires an existing
//! absolute report path ([`events::REPORT_REF_INVALID`]). `workspace` / `task
//! show` rows carry the derived `self_validated` flag. Routing is material for
//! a human dispatcher, never a routing decision.
//!
//! Stage 7 ([`qualify`]) adds rejection / deferral / cleared qualifications as
//! append-only history, the anti-rebirth check on ingest (exact rejected claims
//! block; near matches warn; `--override-rejected` leaves an auditable row), the
//! read-only `learning qualified` surface and the MCP `memory_qualified` tool.
//! `gate-target` / `export` report `rejected` / `deferred` as verdicts, and
//! `learning anomalies` gains the read-only `reject_clear_flip` signal.
//!
//! # Isolation
//!
//! Every entry point takes an explicit absolute learning root
//! ([`store::LearningStore::init`] / [`store::LearningStore::open`]); the gate
//! additionally takes an explicit absolute archive root. There is no default
//! root, no environment variable and no home-directory inference: a relative
//! path, a `~`-prefixed path or an empty path is rejected.
//!
//! # Storage
//!
//! The root holds exactly one database file, `learning.db` (plus SQLite's WAL
//! sidecars while it is open, the automatic `learning.db.bak-v4-<UTC>` snapshot
//! written before the v4 -> v5 migration, and the declared `learning.db.bak` +
//! `learning.db.bak.meta.json` pair after an explicit `learning migrate --to
//! 3`), with `PRAGMA user_version = 5`, foreign keys on, WAL journaling and
//! `synchronous=FULL`. Each ingest, dedup, reuse, validation, task create/claim,
//! task abandon and qualification write is one `BEGIN IMMEDIATE` transaction;
//! any error rolls the whole unit back. Every event write also allocates one
//! global `event_seq` from `event_clock` inside that transaction, so events are
//! orderable across tables.

pub mod anomalies;
pub mod dedup;
pub mod events;
pub mod gate;
pub mod inject;
pub mod model;
pub mod provenance;
pub mod qualify;
pub mod routing;
pub mod store;
pub mod task;

use std::fmt;
use std::path::Path;

pub use anomalies::{
    is_environmental, render_anomalies_text, unreviewed_total, AnomaliesReport, AnomalySignal,
    Watermark, ANOMALY_CREATE_BURST, ANOMALY_KINDS, ANOMALY_LEGACY_INVALID, ANOMALY_LONG_CLAIM,
    ANOMALY_MARKER_HIT, ANOMALY_MULTI_ACTOR, ANOMALY_NEVER_REFLECTED, ANOMALY_NEW_ACTOR,
    ANOMALY_REJECT_CLEAR_FLIP, CREATE_BURST_MIN_EVENTS, CREATE_BURST_WINDOW_MINUTES,
    LONG_CLAIM_CODEPOINTS, MULTI_ACTOR_MIN_ACTORS, MULTI_ACTOR_WINDOW_HOURS,
    QUALIFICATION_FLIP_MIN, QUALIFICATION_FLIP_WINDOW_DAYS, REFLECT_CLAIM_PREFIX,
};
pub use dedup::{DedupOutcome, DedupRequest, DEDUP_REPORT_INVALID, DISPOSITIONS};
pub use events::{
    ReuseOutcome, ReuseRequest, ValidationOutcome, ValidationRequest, EVENT_RESULTS,
    REPORT_REF_INVALID, REUSE_IDEMPOTENCY_CONFLICT, REUSE_IDEMPOTENCY_MISMATCH,
    REUSE_TASK_CONFLICT, REVIEWER_CONFLICT, TASK_MISMATCH,
};
pub use gate::{
    evaluate, GateCounts, GateItem, GateReport, E_GENERATION_MISMATCH, E_PROVENANCE_MISMATCH,
    E_REF_FORMAT, E_REF_UNRESOLVED, E_SPAN_MISMATCH, E_TAINT_POLICY, GATE_ARCHIVE_UNREADABLE,
    GATE_BLOCKED, GATE_ROOT_INVALID, LIST_CONTRADICTING, LIST_EVIDENCE, REF_PREFIX, VERDICT_FAILED,
    VERDICT_PASSED,
};
pub use inject::{
    anomaly_count_line, fold_for_markers, footer_line, inject_caps, marker_classes,
    render_source_line, InjectCaps, InjectCheckReport, InjectCheckViolation, InjectEntry,
    InjectReport, InjectSection, INJECT_ANOMALY_TEMPLATE, INJECT_DEFAULT_MAX_BYTES,
    INJECT_DEFAULT_MAX_TASKS, INJECT_NOTICE, INJECT_R14_LINE, INJECT_TEMPLATE_CLAUSE,
    MARKER_EXTERNAL_ACTION, MARKER_EXTERNAL_URL, MARKER_ORDER, MARKER_SHELL_COMMAND,
    MARKER_VERIFICATION_BYPASS,
};
pub use model::{
    claim_codepoint_count, normalize_claim_for_copy_lint, CandidateRecord, DedupState,
    EvidenceItem, Promotion, ValidatedCandidate, CANDIDATE_STATUS, CLAIM_FORBIDDEN_SEQUENCES,
    CLAIM_INVALID_TEXT, CLAIM_MAX_CODEPOINTS, CLAIM_TOO_LONG, COORDINATION_ZONE_END_LINE,
    COORDINATION_ZONE_START_LINE, DERIVED_FROM_MAX_CODEPOINTS, DERIVED_FROM_NOT_ALLOWED,
    DERIVED_FROM_VALUES, EVIDENCE_KINDS, ORIGIN_CLAIM_COPY, ORIGIN_CLASSES, ORIGIN_TARGET_NOT_TASK,
    ORIGIN_TASKS_DUPLICATE, ORIGIN_TASKS_MAX, ORIGIN_TASKS_NOT_ALLOWED, ORIGIN_TASKS_TOO_MANY,
    ORIGIN_TASK_UNKNOWN, SCHEMA_VERSION, SESSION_KINDS, TARGET_KINDS, TASK_PROPOSED_TARGET,
    TASK_TARGET_KIND, TEMPLATE_MARKER_PREFIX,
};
pub use provenance::{RecordFacts, CLAUDE_PROVIDER};
pub use qualify::{
    jaccard_similarity, normalize_for_rejection, parse_counterexample_ref,
    render_qualification_audit_text, render_qualified_text, verify_counterexample_ref,
    QualificationAudit, QualificationFinding, QualificationRow, QualificationView, QualifiedEntry,
    QualifiedReport, QualifyOutcome, QualifyRequest, RejectedMatch, RejectionOverride,
    DEFERRED_REVIEW_DAYS, FLIP_MIN, FLIP_WINDOW_DAYS, NEAR_REJECTED_THRESHOLD, NOTHING_TO_CLEAR,
    QUALIFY_STATES, REJECTED_CLAIM_MATCH, STATE_CLEARED, STATE_DEFERRED, STATE_OPEN,
    STATE_REJECTED, TASK_NOT_QUALIFIABLE,
};
pub use routing::{
    evaluate as evaluate_target, evaluate_with_qualification, review_identity, review_package,
    RelatedTask, Requirement, ReviewPackage, ReviewRouting, SourceGateSummary, TargetReport,
    BLOCKED_BY_SOURCE_GATE, BLOCKED_QUALIFICATION_DEFERRED, BLOCKED_QUALIFICATION_REJECTED,
    BLOCKED_RULE_GOVERNANCE, DECISION_REQUIRED, RELATED_TASK_MISSING, REVIEWER_REQUIREMENTS,
    RULE_PROMOTION_PATH, SOURCE_GATE_FAILED, SOURCE_GATE_PASSED, SOURCE_GATE_SKIPPED,
    STATUS_MANUAL, STATUS_MET, STATUS_SKIPPED, STATUS_UNMET, VERDICT_DEFERRED,
    VERDICT_INSUFFICIENT, VERDICT_PROPOSAL_ONLY, VERDICT_READY, VERDICT_REJECTED,
};
pub use store::{
    CandidateSummary, CandidateView, DowngradeDropped, DowngradeKept, DowngradeReport,
    IngestOutcome, LearningStore, ListReport, OverrideOutcome, RevisionInfo, StatusEntry,
    StoredIngestion, ACTOR_INVALID, ACTOR_MAX_CODEPOINTS, DB_FILE, DB_SCHEMA_VERSION,
    DOWNGRADE_BACKUP_FILE, DOWNGRADE_BLOCKED_ORIGIN_TASKS, DOWNGRADE_SIDECAR_FILE,
    UPGRADE_BACKUP_PREFIX,
};
pub use task::{
    project_task_state, TaskCreateOutcome, TaskEventOutcome, TaskEventSummary, TaskEventView,
    TaskProjection, TaskShowReport, TaskValidationSummary, WorkspaceReport, WorkspaceTaskRow,
    DEFAULT_STALE_DAYS, NOT_A_TASK, TASK_EVENT_ABANDON, TASK_EVENT_CLAIM, TASK_EVENT_REUSE,
    TASK_EVENT_VALIDATION, TASK_NOT_ROUTEABLE, TASK_REQUIRES_TASK_CREATE, TASK_STATE_ABANDONED,
    TASK_STATE_ACTIVE, TASK_STATE_DONE, TASK_STATE_STALE,
};

/// Stable exit codes shared with the rest of the CLI.
pub const EX_USAGE: u8 = 2;
pub const EX_IO: u8 = 3;
pub const EX_LOCK: u8 = 4;
pub const EX_NOTFOUND: u8 = 6;

/// Coarse failure class; it decides the stable exit code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LearningErrorKind {
    /// Bad arguments or a candidate that fails the v1 schema.
    Usage,
    /// A stable identity/state conflict (`INGEST_KEY_CONFLICT`, revision, a
    /// blocked gated ingest).
    Conflict,
    /// Filesystem or database state that the caller must inspect.
    State,
    /// I/O failure.
    Io,
    /// The database is locked by another writer.
    Lock,
    /// A missing candidate, a wrong `user_version` or a non-learning database.
    Integrity,
}

/// A learning-store error with a stable machine-readable `code`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LearningError {
    pub code: &'static str,
    pub message: String,
    pub kind: LearningErrorKind,
}

impl LearningError {
    pub fn new(code: &'static str, kind: LearningErrorKind, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            kind,
        }
    }

    pub fn usage(message: impl Into<String>) -> Self {
        Self::new("INVALID_USAGE", LearningErrorKind::Usage, message)
    }

    pub fn invalid_root(message: impl Into<String>) -> Self {
        Self::new("LEARNING_ROOT_INVALID", LearningErrorKind::Usage, message)
    }

    pub fn invalid_candidate(message: impl Into<String>) -> Self {
        Self::new("INVALID_CANDIDATE", LearningErrorKind::Usage, message)
    }

    pub fn ingest_key_conflict(message: impl Into<String>) -> Self {
        Self::new("INGEST_KEY_CONFLICT", LearningErrorKind::Conflict, message)
    }

    pub fn revision_conflict(message: impl Into<String>) -> Self {
        Self::new(
            "CANDIDATE_REVISION_CONFLICT",
            LearningErrorKind::Conflict,
            message,
        )
    }

    pub fn state(message: impl Into<String>) -> Self {
        Self::new("LEARNING_STATE", LearningErrorKind::State, message)
    }

    pub fn io(path: &Path, error: std::io::Error) -> Self {
        Self::new(
            "LEARNING_IO",
            LearningErrorKind::Io,
            format!("{}: {error}", path.display()),
        )
    }

    pub fn locked(message: impl Into<String>) -> Self {
        Self::new("LEARNING_LOCKED", LearningErrorKind::Lock, message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new("CANDIDATE_NOT_FOUND", LearningErrorKind::Integrity, message)
    }

    pub fn db_missing(message: impl Into<String>) -> Self {
        Self::new("LEARNING_DB_MISSING", LearningErrorKind::Integrity, message)
    }

    pub fn schema_version_unsupported(message: impl Into<String>) -> Self {
        Self::new(
            "SCHEMA_VERSION_UNSUPPORTED",
            LearningErrorKind::Integrity,
            message,
        )
    }

    pub fn schema_invalid(message: impl Into<String>) -> Self {
        Self::new(
            "LEARNING_SCHEMA_INVALID",
            LearningErrorKind::Integrity,
            message,
        )
    }

    /// Exit code promised by the CLI contract: usage/schema/conflict = 2,
    /// I/O/state = 3, lock = 4, not found/integrity = 6.
    pub fn exit_code(&self) -> u8 {
        match self.kind {
            LearningErrorKind::Usage | LearningErrorKind::Conflict => EX_USAGE,
            LearningErrorKind::State | LearningErrorKind::Io => EX_IO,
            LearningErrorKind::Lock => EX_LOCK,
            LearningErrorKind::Integrity => EX_NOTFOUND,
        }
    }
}

impl fmt::Display for LearningError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for LearningError {}
