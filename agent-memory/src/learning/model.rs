//! Candidate v1 schema, identity normalization and payload hashing.
//!
//! The v1 input contract is deliberately narrow: every field is required,
//! unknown fields are rejected at every level, and S1 understands exactly one
//! status (`candidate`). Validation is structural only — an `owner` label in an
//! evidence item is *not* treated as a verified fact (source gating is S2).

use chrono::DateTime;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::archive::hex_sha256;

use super::{LearningError, LearningErrorKind};

pub const SCHEMA_VERSION: u32 = 1;
pub const CANDIDATE_STATUS: &str = "candidate";

/// Stage 5 §A: hard upper bound of a task `claim`, counted in Unicode code
/// points after trimming (never bytes, never UTF-16 units).
pub const CLAIM_MAX_CODEPOINTS: usize = 500;
/// Stage 5 §A: hard upper bound of `derived_from` (64 code points).
pub const DERIVED_FROM_MAX_CODEPOINTS: usize = 64;
/// Stage 5 §A: the only accepted `derived_from` values.
pub const DERIVED_FROM_VALUES: [&str; 3] = ["external", "internal", "unknown"];

/// Stage 6 §D: hard upper bound of `origin_tasks` (task candidate ids linked
/// from one knowledge candidate).
pub const ORIGIN_TASKS_MAX: usize = 8;

/// Stage 6 §D: `origin_tasks` has more than [`ORIGIN_TASKS_MAX`] entries.
/// Usage class.
pub const ORIGIN_TASKS_TOO_MANY: &str = "ORIGIN_TASKS_TOO_MANY";
/// Stage 6 §D: `origin_tasks` repeats an id (after trimming). Usage class.
pub const ORIGIN_TASKS_DUPLICATE: &str = "ORIGIN_TASKS_DUPLICATE";
/// Stage 6 §D: an `origin_tasks` id names no candidate in this store. Usage
/// class; the read side never re-checks (dangling ids are allowed there).
pub const ORIGIN_TASK_UNKNOWN: &str = "ORIGIN_TASK_UNKNOWN";
/// Stage 6 §D: an `origin_tasks` id exists but its `target_kind` is not
/// `"task"`. Usage class.
pub const ORIGIN_TARGET_NOT_TASK: &str = "ORIGIN_TARGET_NOT_TASK";
/// Stage 6 §D: a `target_kind = "task"` payload carries a non-empty
/// `origin_tasks` (only knowledge candidates are distilled from tasks). Usage
/// class.
pub const ORIGIN_TASKS_NOT_ALLOWED: &str = "ORIGIN_TASKS_NOT_ALLOWED";
/// Stage 6 §D copy lint: the candidate claim equals an origin task claim after
/// [`normalize_claim_for_copy_lint`]. Usage class; it is a lint, not a
/// boundary — a rewritten copy is left to the reviewer.
pub const ORIGIN_CLAIM_COPY: &str = "ORIGIN_CLAIM_COPY";

/// `claim` exceeded [`CLAIM_MAX_CODEPOINTS`] after trimming. Usage class.
pub const CLAIM_TOO_LONG: &str = "CLAIM_TOO_LONG";
/// `claim` contains a forbidden Unicode class, a fence/separator sequence or a
/// template literal line. Usage class; the message names the violated class.
pub const CLAIM_INVALID_TEXT: &str = "CLAIM_INVALID_TEXT";
/// `derived_from` is not allowed for this candidate (knowledge kind, bad value
/// or over the length bound). Usage class.
pub const DERIVED_FROM_NOT_ALLOWED: &str = "DERIVED_FROM_NOT_ALLOWED";

/// The two boundary lines of the stage 5 injection block, verbatim. A task
/// claim may never reproduce either of them (nor start with `=== `): the block
/// is a system template, and a claim is data that must not be able to forge
/// template structure. implementation-002 reuses these constants for the
/// `learning inject` header/footer.
pub const COORDINATION_ZONE_START_LINE: &str = "=== 任务协调区（agent-memory，只读投影） ===";
pub const COORDINATION_ZONE_END_LINE: &str = "=== 任务协调区结束 ===";
/// A claim starting with this prefix can forge a template marker line.
pub const TEMPLATE_MARKER_PREFIX: &str = "=== ";
/// Markdown fence / horizontal-rule sequences that must never appear in a
/// single-line claim: the injection block is line-oriented Markdown.
pub const CLAIM_FORBIDDEN_SEQUENCES: [&str; 4] = ["```", "---", "***", "___"];

/// One explicitly enumerated Unicode range a claim (and, through the same
/// predicate, an actor id) may never contain. These are *code* constants, not
/// properties read from an external table: the boundary is audit-readable and
/// changes only by editing this list.
#[derive(Debug, Clone, Copy)]
pub struct ForbiddenRange {
    pub start: u32,
    pub end: u32,
    /// Reported class name: `Cc/C0`, `Cc/DEL`, `Cf`, `Zl`, `Zp`.
    pub class: &'static str,
}

/// Stage 5 §A: every rejected code point interval.
///
/// Covers all of C0 (U+0000–U+001F, which includes LF, CR and TAB), DEL, the
/// whole Cf set required by the spec (including the bidi controls, the zero
/// width set and the Arabic letter mark), and the Zl/Zp line/paragraph
/// separators. `U+2028`/`U+2029` are listed explicitly even though Rust's
/// `str::trim` treats them as whitespace: the raw input is scanned *before*
/// trimming, so trailing/leading separators can never be swallowed.
pub const FORBIDDEN_CODEPOINT_RANGES: &[ForbiddenRange] = &[
    // Cc: all C0 controls.
    ForbiddenRange {
        start: 0x0000,
        end: 0x001F,
        class: "Cc/C0",
    },
    // Cc: DEL.
    ForbiddenRange {
        start: 0x007F,
        end: 0x007F,
        class: "Cc/DEL",
    },
    // Cf: soft hyphen.
    ForbiddenRange {
        start: 0x00AD,
        end: 0x00AD,
        class: "Cf",
    },
    // Cf: Arabic letter mark.
    ForbiddenRange {
        start: 0x061C,
        end: 0x061C,
        class: "Cf",
    },
    // Cf: Mongolian vowel separator.
    ForbiddenRange {
        start: 0x180E,
        end: 0x180E,
        class: "Cf",
    },
    // Cf: zero width space / non-joiner / joiner.
    ForbiddenRange {
        start: 0x200B,
        end: 0x200D,
        class: "Cf",
    },
    // Cf: left-to-right mark.
    ForbiddenRange {
        start: 0x200E,
        end: 0x200E,
        class: "Cf",
    },
    // Cf: right-to-left mark.
    ForbiddenRange {
        start: 0x200F,
        end: 0x200F,
        class: "Cf",
    },
    // Zl: line separator.
    ForbiddenRange {
        start: 0x2028,
        end: 0x2028,
        class: "Zl",
    },
    // Zp: paragraph separator.
    ForbiddenRange {
        start: 0x2029,
        end: 0x2029,
        class: "Zp",
    },
    // Cf: bidi embedding / override controls.
    ForbiddenRange {
        start: 0x202A,
        end: 0x202E,
        class: "Cf",
    },
    // Cf: word joiner and invisible operators.
    ForbiddenRange {
        start: 0x2060,
        end: 0x2064,
        class: "Cf",
    },
    // Cf: bidi isolates.
    ForbiddenRange {
        start: 0x2066,
        end: 0x2069,
        class: "Cf",
    },
    // Cf: zero width no-break space / BOM.
    ForbiddenRange {
        start: 0xFEFF,
        end: 0xFEFF,
        class: "Cf",
    },
    // Cf: interlinear annotation anchors.
    ForbiddenRange {
        start: 0xFFF9,
        end: 0xFFFB,
        class: "Cf",
    },
];

/// The class of `ch` if it falls in one of the enumerated ranges. This is the
/// single predicate shared by claim validation and actor validation.
pub fn forbidden_codepoint_class(ch: char) -> Option<&'static str> {
    let code = ch as u32;
    FORBIDDEN_CODEPOINT_RANGES
        .iter()
        .find(|range| range.start <= code && code <= range.end)
        .map(|range| range.class)
}

fn first_forbidden(value: &str) -> Option<(char, &'static str)> {
    value
        .chars()
        .find_map(|ch| forbidden_codepoint_class(ch).map(|class| (ch, class)))
}

fn claim_invalid(message: impl Into<String>) -> LearningError {
    LearningError::new(CLAIM_INVALID_TEXT, LearningErrorKind::Usage, message)
}

/// The claim length measure of the write path: Unicode scalar values of the
/// trimmed claim (`chars().count()`, never bytes, never UTF-16 units).
///
/// Stage 6 §A `long_claim` must use this same counter, so the threshold and the
/// write-side bound are always measured the same way.
pub fn claim_codepoint_count(raw: &str) -> usize {
    raw.trim().chars().count()
}

/// Stage 5 §A claim validation for the task branch. Order is fixed:
///
/// 1. reject forbidden Unicode classes in the **raw** input (before trimming,
///    so a leading/trailing LF, CR, U+2028 or U+2029 is never swallowed);
/// 2. trim;
/// 3. count code points (`chars().count()`, never bytes) against
///    [`CLAIM_MAX_CODEPOINTS`];
/// 4. reject fence/separator sequences and template literal lines;
/// 5. re-check the forbidden classes on the trimmed value;
/// 6. the caller derives `candidate_id` from the trimmed claim.
///
/// A blank claim keeps the pre-stage-5 [`LearningError::invalid_candidate`]
/// (`INVALID_CANDIDATE`) result, so existing callers see the same code.
pub fn validate_task_claim(raw: &str) -> Result<(), LearningError> {
    if let Some((ch, class)) = first_forbidden(raw) {
        return Err(claim_invalid(format!(
            "claim contains the forbidden Unicode class {class} (U+{:04X}); the claim is a \
             single line and must not carry control or format characters",
            ch as u32
        )));
    }

    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(LearningError::invalid_candidate(
            "claim must be a non-empty string",
        ));
    }

    let count = claim_codepoint_count(trimmed);
    if count > CLAIM_MAX_CODEPOINTS {
        return Err(LearningError::new(
            CLAIM_TOO_LONG,
            LearningErrorKind::Usage,
            format!(
                "claim must be at most {CLAIM_MAX_CODEPOINTS} Unicode code points (got {count})"
            ),
        ));
    }

    if let Some(sequence) = CLAIM_FORBIDDEN_SEQUENCES
        .iter()
        .find(|sequence| trimmed.contains(**sequence))
    {
        return Err(claim_invalid(format!(
            "claim must not contain the fence/separator sequence {sequence:?}: the claim is a \
             single data line, never block markup"
        )));
    }
    if trimmed.starts_with(TEMPLATE_MARKER_PREFIX) {
        return Err(claim_invalid(format!(
            "claim must not start with the template marker prefix {TEMPLATE_MARKER_PREFIX:?}"
        )));
    }
    if trimmed == COORDINATION_ZONE_START_LINE || trimmed == COORDINATION_ZONE_END_LINE {
        return Err(claim_invalid(
            "claim must not reproduce a coordination-zone boundary line; those lines are the \
             system template, a claim is data",
        ));
    }

    if let Some((ch, class)) = first_forbidden(trimmed) {
        return Err(claim_invalid(format!(
            "claim contains the forbidden Unicode class {class} (U+{:04X})",
            ch as u32
        )));
    }

    Ok(())
}

/// Stage 5 §A `derived_from` validation. `None` (field absent) is the v1
/// compatible shape and serializes to nothing; `Some` must be one of
/// [`DERIVED_FROM_VALUES`] and at most [`DERIVED_FROM_MAX_CODEPOINTS`] code
/// points. Every rejection uses [`DERIVED_FROM_NOT_ALLOWED`].
pub fn validate_derived_from(raw: Option<&str>) -> Result<Option<String>, LearningError> {
    let Some(value) = raw else {
        return Ok(None);
    };
    let trimmed = value.trim();
    let count = trimmed.chars().count();
    if count > DERIVED_FROM_MAX_CODEPOINTS || !DERIVED_FROM_VALUES.contains(&trimmed) {
        return Err(LearningError::new(
            DERIVED_FROM_NOT_ALLOWED,
            LearningErrorKind::Usage,
            format!(
                "derived_from must be one of {DERIVED_FROM_VALUES:?} with at most \
                 {DERIVED_FROM_MAX_CODEPOINTS} code points, got {value:?}"
            ),
        ));
    }
    Ok(Some(trimmed.to_string()))
}

/// Stage 6 §D shape validation of `origin_tasks` (the store-dependent checks —
/// the id exists, is a task, and is not copied — live in the ingest
/// transaction, which is the only place with a database connection):
///
/// - a `target_kind = "task"` payload may not carry a non-empty list
///   ([`ORIGIN_TASKS_NOT_ALLOWED`]);
/// - at most [`ORIGIN_TASKS_MAX`] entries ([`ORIGIN_TASKS_TOO_MANY`]);
/// - no repeated id after trimming ([`ORIGIN_TASKS_DUPLICATE`]).
///
/// A knowledge candidate without the key — and one with an empty list — keeps
/// the exact v1 bytes, because the field is skipped when empty.
pub fn validate_origin_tasks_shape(
    raw: &[String],
    is_task: bool,
) -> Result<Vec<String>, LearningError> {
    if is_task {
        if raw.is_empty() {
            return Ok(Vec::new());
        }
        return Err(LearningError::new(
            ORIGIN_TASKS_NOT_ALLOWED,
            LearningErrorKind::Usage,
            "origin_tasks is only allowed on a knowledge candidate; a task is the source of \
             distillation, never its target, and `learning task create` never writes the key",
        ));
    }
    if raw.len() > ORIGIN_TASKS_MAX {
        return Err(LearningError::new(
            ORIGIN_TASKS_TOO_MANY,
            LearningErrorKind::Usage,
            format!(
                "origin_tasks must contain at most {ORIGIN_TASKS_MAX} task ids, got {}",
                raw.len()
            ),
        ));
    }
    let mut trimmed = Vec::with_capacity(raw.len());
    let mut seen = std::collections::BTreeSet::new();
    for value in raw {
        let id = value.trim().to_string();
        if !seen.insert(id.clone()) {
            return Err(LearningError::new(
                ORIGIN_TASKS_DUPLICATE,
                LearningErrorKind::Usage,
                format!("origin_tasks repeats the task id {id:?}; list each origin task once"),
            ));
        }
        trimmed.push(id);
    }
    Ok(trimmed)
}

/// The `target_kind` of a workspace task candidate (stage 4). A task is a
/// first-class candidate whose state is a read-side projection of the event
/// tables; it is never exported and never leaves `status = candidate`.
pub const TASK_TARGET_KIND: &str = "task";
/// The only `proposed_target` a task candidate may carry. Because the identity
/// hash covers `(claim, target_kind, proposed_target)`, the constant makes two
/// creates with the same claim converge on one `candidate_id` (R13).
pub const TASK_PROPOSED_TARGET: &str = "task";

pub const TARGET_KINDS: [&str; 7] = [
    "feedback",
    "user_profile",
    "workflow",
    "skill",
    "note",
    "rule",
    "task",
];
pub const SESSION_KINDS: [&str; 6] = [
    "interactive",
    "cron",
    "heartbeat",
    "subagent",
    "other",
    "unknown",
];
pub const ORIGIN_CLASSES: [&str; 5] = ["owner", "agent_derived", "external", "runtime", "unknown"];
pub const EVIDENCE_KINDS: [&str; 8] = [
    "user_statement",
    "correction",
    "approval",
    "execution_result",
    "test_result",
    "external_material",
    "recalled_material",
    "unknown",
];

const SPAN_SHA256_LEN: usize = 64;

/// One evidence item. Field order is the canonical serialization order used by
/// the evidence dedupe hash.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EvidenceItem {
    #[serde(rename = "ref")]
    pub reference: String,
    pub span_sha256: String,
    pub archive_generation: String,
    pub observed_at: String,
    pub actor: String,
    pub session_kind: String,
    pub origin_class: String,
    pub evidence_kind: String,
    pub recall_taint: bool,
}

/// The pre-dedup state fixed by S1. S3 is the step that opens the result up.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct DedupState {
    pub query: String,
    pub matches: Vec<Value>,
    pub disposition: String,
    pub checked_at: Option<String>,
}

/// Placeholder for the S2 gate; S1 requires all three to be null.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Promotion {
    pub approval: Option<Value>,
    pub apply: Option<Value>,
    pub verification: Option<Value>,
}

/// The exact input shape. Anything outside this struct is rejected.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCandidate {
    schema_version: u32,
    candidate_id: String,
    revision: u64,
    status: String,
    claim: String,
    target_kind: String,
    proposed_target: String,
    /// Stage 5 §A: task-only provenance label. `#[serde(default)]` keeps every
    /// pre-stage-5 payload (which has no such key) readable; a knowledge
    /// candidate that carries the key is rejected in `validate_candidate`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    derived_from: Option<String>,
    /// Stage 6 §D: knowledge-only distillation links (task candidate ids).
    /// Absent and empty are the same value and serialize to nothing, so a
    /// payload without the key keeps the exact v1 bytes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    origin_tasks: Vec<String>,
    applicability: Vec<String>,
    counterexamples: Vec<String>,
    unknowns: Vec<String>,
    evidence: Vec<EvidenceItem>,
    contradicting_evidence: Vec<EvidenceItem>,
    dedup: DedupState,
    reuse: Vec<Value>,
    validation: Vec<Value>,
    promotion: Promotion,
    ingestions: Vec<Value>,
    source_checkpoint: String,
    extractor: String,
    created_at: String,
    updated_at: String,
}

/// A normalized candidate record: the payload hash covers exactly this struct
/// (never the storage-generated `ingestions` list).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CandidateRecord {
    pub schema_version: u32,
    pub candidate_id: String,
    pub revision: u64,
    pub status: String,
    pub claim: String,
    pub target_kind: String,
    pub proposed_target: String,
    /// Stage 5 §A: task-only provenance label. `None` is omitted from the
    /// serialized record, so a task created without the field keeps the exact
    /// v1/v4 bytes (and the golden `candidate_id`). It is deliberately not
    /// part of `candidate_id_for` or the ingest key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub derived_from: Option<String>,
    /// Stage 6 §D: task ids this *knowledge* candidate distills experience
    /// from. `Vec::is_empty` is skipped, so a candidate without links keeps the
    /// exact v1 bytes; the field is part of the same-revision comparison set
    /// but never part of `candidate_id_for` or the ingest key.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub origin_tasks: Vec<String>,
    pub applicability: Vec<String>,
    pub counterexamples: Vec<String>,
    pub unknowns: Vec<String>,
    pub evidence: Vec<EvidenceItem>,
    pub contradicting_evidence: Vec<EvidenceItem>,
    pub dedup: DedupState,
    pub reuse: Vec<Value>,
    pub validation: Vec<Value>,
    pub promotion: Promotion,
    pub source_checkpoint: String,
    pub extractor: String,
    pub created_at: String,
    pub updated_at: String,
}

impl CandidateRecord {
    /// Fields that may not differ between two submissions of the same
    /// revision. `evidence`, `contradicting_evidence` and `updated_at` are the
    /// only append/refresh points.
    ///
    /// `source_checkpoint` and `extractor` are the ingest-key inputs: a
    /// different batch reaches a same-revision append only through a different
    /// (extractor, checkpoint) pair, so requiring them to be equal would make
    /// the append path unreachable (the same key is either a replay or an
    /// `INGEST_KEY_CONFLICT`). They are therefore provenance, not candidate
    /// content: the append keeps the stored record's provenance and the batch
    /// provenance stays in `ingestions`.
    pub fn same_revision_fields(&self, other: &CandidateRecord) -> bool {
        self.schema_version == other.schema_version
            && self.candidate_id == other.candidate_id
            && self.revision == other.revision
            && self.status == other.status
            && self.claim == other.claim
            && self.target_kind == other.target_kind
            && self.proposed_target == other.proposed_target
            && self.derived_from == other.derived_from
            && self.origin_tasks == other.origin_tasks
            && self.applicability == other.applicability
            && self.counterexamples == other.counterexamples
            && self.unknowns == other.unknowns
            && self.dedup == other.dedup
            && self.reuse == other.reuse
            && self.validation == other.validation
            && self.promotion == other.promotion
            && self.created_at == other.created_at
    }

    /// Compact JSON of the record; stable because serde serializes struct
    /// fields in declaration order (no object key ordering involved).
    pub fn canonical_json(&self) -> String {
        serde_json::to_string(self).expect("candidate record is serializable")
    }
}

/// A validated submission together with its derived stable identities.
#[derive(Debug, Clone)]
pub struct ValidatedCandidate {
    pub record: CandidateRecord,
    pub ingest_key: String,
    pub payload_sha256: String,
    pub payload_json: String,
}

/// Identity normalization: trim, fold ASCII case, collapse consecutive Unicode
/// whitespace into one space.
pub fn normalize_identity(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut pending_space = false;
    for ch in value.trim().chars() {
        if ch.is_whitespace() {
            pending_space = true;
            continue;
        }
        if pending_space && !out.is_empty() {
            out.push(' ');
        }
        pending_space = false;
        out.push(ch.to_ascii_lowercase());
    }
    out
}

/// Stage 6 §D copy-lint normalization: trim, fold fullwidth ASCII and the
/// ideographic space (the exact fold `inject` uses for visibility markers, via
/// [`super::inject::fold_for_markers`], so the logic is never duplicated),
/// lower-case, and collapse consecutive whitespace into one space.
///
/// This is a lint key only: it never changes a stored claim.
pub fn normalize_claim_for_copy_lint(value: &str) -> String {
    let folded = super::inject::fold_for_markers(value.trim());
    let mut out = String::with_capacity(folded.len());
    let mut pending_space = false;
    for ch in folded.chars() {
        if ch.is_whitespace() {
            pending_space = true;
            continue;
        }
        if pending_space && !out.is_empty() {
            out.push(' ');
        }
        pending_space = false;
        out.push(ch);
    }
    out
}

/// `cand_` + first 32 lowercase hex of
/// `SHA256(compact_json([normalized_claim, target_kind, normalized_proposed_target]))`.
pub fn candidate_id_for(claim: &str, target_kind: &str, proposed_target: &str) -> String {
    let identity = serde_json::json!([
        normalize_identity(claim),
        target_kind,
        normalize_identity(proposed_target)
    ]);
    let digest = hex_sha256(identity.to_string().as_bytes());
    format!("cand_{}", &digest[..32])
}

/// `ing_` + first 32 lowercase hex of
/// `SHA256(compact_json([trimmed_extractor, trimmed_source_checkpoint]))`.
pub fn ingest_key_for(extractor: &str, source_checkpoint: &str) -> String {
    let identity = serde_json::json!([extractor.trim(), source_checkpoint.trim()]);
    let digest = hex_sha256(identity.to_string().as_bytes());
    format!("ing_{}", &digest[..32])
}

/// Canonical evidence hash used for per-list dedupe.
pub fn evidence_sha256(item: &EvidenceItem) -> String {
    serde_json::to_string(item)
        .map(|text| hex_sha256(text.as_bytes()))
        .unwrap_or_default()
}

fn is_lower_hex_sha256(value: &str) -> bool {
    value.len() == SPAN_SHA256_LEN
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn parse_timestamp(
    field: &str,
    value: &str,
) -> Result<DateTime<chrono::FixedOffset>, LearningError> {
    DateTime::parse_from_rfc3339(value).map_err(|error| {
        LearningError::invalid_candidate(format!("{field} must be an RFC 3339 timestamp ({error})"))
    })
}

fn required_text(field: &str, value: &str) -> Result<String, LearningError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(LearningError::invalid_candidate(format!(
            "{field} must be a non-empty string"
        )));
    }
    Ok(trimmed.to_string())
}

fn require_enum(field: &str, value: &str, allowed: &[&str]) -> Result<(), LearningError> {
    if allowed.contains(&value) {
        return Ok(());
    }
    Err(LearningError::invalid_candidate(format!(
        "{field} must be one of {allowed:?}, got {value:?}"
    )))
}

fn validate_evidence(list: &str, index: usize, item: &EvidenceItem) -> Result<(), LearningError> {
    let where_ = format!("{list}[{index}]");
    if item.reference.trim().is_empty() {
        return Err(LearningError::invalid_candidate(format!(
            "{where_}.ref must be a non-empty string"
        )));
    }
    if !is_lower_hex_sha256(&item.span_sha256) {
        return Err(LearningError::invalid_candidate(format!(
            "{where_}.span_sha256 must be 64 lowercase hex characters"
        )));
    }
    if item.archive_generation.trim().is_empty() {
        return Err(LearningError::invalid_candidate(format!(
            "{where_}.archive_generation must be a non-empty string"
        )));
    }
    if item.actor.trim().is_empty() {
        return Err(LearningError::invalid_candidate(format!(
            "{where_}.actor must be a non-empty string"
        )));
    }
    parse_timestamp(&format!("{where_}.observed_at"), &item.observed_at)?;
    require_enum(
        &format!("{where_}.session_kind"),
        &item.session_kind,
        &SESSION_KINDS,
    )?;
    require_enum(
        &format!("{where_}.origin_class"),
        &item.origin_class,
        &ORIGIN_CLASSES,
    )?;
    require_enum(
        &format!("{where_}.evidence_kind"),
        &item.evidence_kind,
        &EVIDENCE_KINDS,
    )?;
    Ok(())
}

fn validate_dedup(dedup: &DedupState) -> Result<(), LearningError> {
    if dedup.query != "" || !dedup.matches.is_empty() || dedup.checked_at.is_some() {
        return Err(LearningError::invalid_candidate(
            "dedup must be exactly {\"query\":\"\",\"matches\":[],\"disposition\":\"pending\",\"checked_at\":null}; \
             S1 stores pre-dedup state only",
        ));
    }
    if dedup.disposition != "pending" {
        return Err(LearningError::invalid_candidate(
            "dedup.disposition must be \"pending\" in S1",
        ));
    }
    Ok(())
}

fn validate_promotion(promotion: &Promotion) -> Result<(), LearningError> {
    if promotion.approval.is_some() || promotion.apply.is_some() || promotion.verification.is_some()
    {
        return Err(LearningError::invalid_candidate(
            "promotion.approval/apply/verification must all be null in S1",
        ));
    }
    Ok(())
}

/// Parse and strictly validate one v1 candidate, then derive its stable
/// identities and payload hash.
pub fn validate_candidate(value: &Value) -> Result<ValidatedCandidate, LearningError> {
    let raw: RawCandidate = serde_json::from_value(value.clone()).map_err(|error| {
        LearningError::invalid_candidate(format!("candidate does not match the v1 schema: {error}"))
    })?;

    if raw.schema_version != SCHEMA_VERSION {
        return Err(LearningError::invalid_candidate(format!(
            "schema_version must be {SCHEMA_VERSION}, got {}",
            raw.schema_version
        )));
    }
    if raw.revision == 0 {
        return Err(LearningError::invalid_candidate(
            "revision must be a positive integer",
        ));
    }
    if raw.status != CANDIDATE_STATUS {
        return Err(LearningError::invalid_candidate(format!(
            "status must be {CANDIDATE_STATUS:?} in S1; other statuses are rejected",
        )));
    }
    require_enum("target_kind", &raw.target_kind, &TARGET_KINDS)?;

    // Stage 5 §A ordering: the raw claim is rejected for forbidden character
    // classes *before* trimming, so a leading/trailing LF, CR, U+2028 or
    // U+2029 can never be swallowed by `trim()`. A blank claim keeps the
    // pre-stage-5 `INVALID_CANDIDATE` result.
    let is_task = raw.target_kind == TASK_TARGET_KIND;
    if is_task {
        validate_task_claim(&raw.claim)?;
    }

    let claim = required_text("claim", &raw.claim)?;
    let proposed_target = required_text("proposed_target", &raw.proposed_target)?;
    let source_checkpoint = required_text("source_checkpoint", &raw.source_checkpoint)?;
    let extractor = required_text("extractor", &raw.extractor)?;

    let created_at = required_text("created_at", &raw.created_at)?;
    let updated_at = required_text("updated_at", &raw.updated_at)?;
    let created = parse_timestamp("created_at", &created_at)?;
    let updated = parse_timestamp("updated_at", &updated_at)?;
    if created > updated {
        return Err(LearningError::invalid_candidate(
            "created_at must not be later than updated_at",
        ));
    }

    if !raw.reuse.is_empty() {
        return Err(LearningError::invalid_candidate(
            "reuse must be an empty array in S1",
        ));
    }
    if !raw.validation.is_empty() {
        return Err(LearningError::invalid_candidate(
            "validation must be an empty array in S1",
        ));
    }
    if !raw.ingestions.is_empty() {
        return Err(LearningError::invalid_candidate(
            "ingestions is generated by the store and must be an empty array on input",
        ));
    }
    validate_dedup(&raw.dedup)?;
    validate_promotion(&raw.promotion)?;

    // Stage 4: a `task` candidate is a workspace task, not a knowledge claim.
    // It is always revision 1, carries no evidence (its evidence lives in the
    // Archive session and is linked through the event tables), and its
    // `proposed_target` is the fixed constant so equal claims converge on one
    // identity. The six knowledge kinds keep the S1 evidence rules verbatim.
    //
    // Stage 5 §A adds the write-side legality rules on *new* task writes: the
    // raw claim is checked before trimming, the trimmed claim is bounded and
    // template-checked, and `derived_from` is accepted only here. A knowledge
    // payload that merely carries the key is rejected so the v1 knowledge
    // shape stays exactly what it was.
    let derived_from = if is_task {
        validate_derived_from(raw.derived_from.as_deref())?
    } else {
        if value.get("derived_from").is_some() {
            return Err(LearningError::new(
                DERIVED_FROM_NOT_ALLOWED,
                LearningErrorKind::Usage,
                "derived_from is only allowed on a target_kind \"task\" candidate; a knowledge \
                 payload must not carry the key at all (its v1 shape stays byte-identical)",
            ));
        }
        None
    };

    // Stage 6 §D: shape-only checks here (count, duplicates, task payload);
    // existence / kind / copy lint need the store and run inside the ingest
    // transaction.
    let origin_tasks = validate_origin_tasks_shape(&raw.origin_tasks, is_task)?;

    if raw.target_kind == TASK_TARGET_KIND {
        if raw.revision != 1 {
            return Err(LearningError::invalid_candidate(format!(
                "a task candidate is always revision 1 (got {}); task state is a read-side \
                 projection, never a new revision",
                raw.revision
            )));
        }
        if proposed_target != TASK_PROPOSED_TARGET {
            return Err(LearningError::invalid_candidate(format!(
                "a task candidate must use the fixed proposed_target {TASK_PROPOSED_TARGET:?}, \
                 got {proposed_target:?}"
            )));
        }
        if !raw.evidence.is_empty() || !raw.contradicting_evidence.is_empty() {
            return Err(LearningError::invalid_candidate(
                "a task candidate carries no evidence: evidence and contradicting_evidence must \
                 both be empty arrays; task evidence stays in the Archive and is linked through \
                 task_events/reuse_history rather than the payload",
            ));
        }
    } else {
        if raw.evidence.is_empty() {
            return Err(LearningError::invalid_candidate(
                "evidence must contain at least one item",
            ));
        }
        for (index, item) in raw.evidence.iter().enumerate() {
            validate_evidence("evidence", index, item)?;
        }
        for (index, item) in raw.contradicting_evidence.iter().enumerate() {
            validate_evidence("contradicting_evidence", index, item)?;
        }
    }

    let candidate_id = candidate_id_for(&claim, &raw.target_kind, &proposed_target);
    let provided_id = raw.candidate_id.trim();
    if !provided_id.is_empty() && provided_id != candidate_id {
        return Err(LearningError::invalid_candidate(format!(
            "candidate_id {provided_id:?} does not match the computed id {candidate_id:?}"
        )));
    }

    let record = CandidateRecord {
        schema_version: raw.schema_version,
        candidate_id,
        revision: raw.revision,
        status: raw.status,
        claim,
        target_kind: raw.target_kind,
        proposed_target,
        derived_from,
        origin_tasks,
        applicability: raw.applicability,
        counterexamples: raw.counterexamples,
        unknowns: raw.unknowns,
        evidence: raw.evidence,
        contradicting_evidence: raw.contradicting_evidence,
        dedup: raw.dedup,
        reuse: raw.reuse,
        validation: raw.validation,
        promotion: raw.promotion,
        source_checkpoint: source_checkpoint.clone(),
        extractor: extractor.clone(),
        created_at,
        updated_at,
    };

    let ingest_key = ingest_key_for(&extractor, &source_checkpoint);
    let payload_json = record.canonical_json();
    let payload_sha256 = hex_sha256(payload_json.as_bytes());

    Ok(ValidatedCandidate {
        record,
        ingest_key,
        payload_sha256,
        payload_json,
    })
}
