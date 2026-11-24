//! Stage 3 S2: the read-only source and evidence gate (gate v1).
//!
//! The gate answers one question per evidence item: *can this declaration be
//! read back from committed archive evidence, and is it consistent with what
//! that evidence structurally is?* It only reports. It never writes
//! `learning.db`, never writes the archive, never takes the capture lock and
//! never promotes a candidate.
//!
//! # Reference contract (gate v1)
//!
//! Exactly one reference shape is accepted: `archive-record:<64 lowercase
//! hex>`, the `sha256` of a record committed in a manifest. Every other shape
//! (including S1's illustrative `claude:session-…:line-1` style) is
//! [`E_REF_FORMAT`].
//!
//! A span is the complete decoded record. `span_sha256` must equal the SHA-256
//! of those bytes; finer-grained sub-record spans are not pretended to exist.
//!
//! # Check order
//!
//! Per item, in this order, the first failure decides the recorded code:
//!
//! 1. [`E_REF_FORMAT`] — reference shape.
//! 2. [`E_REF_UNRESOLVED`] — the record cannot be read back through the
//!    existing archive read path (`records.json` index + object hash
//!    verification). This covers forged references, records whose access
//!    mapping denies the requested domain, and live-source tails: an
//!    uncommitted line is not evidence.
//! 3. [`E_REF_UNRESOLVED`] again — the sha must appear in the `records` of a
//!    committed manifest that also holds the record's object, and that object
//!    must be the one the record index points at. The oldest such manifest is
//!    the authority for the provider and the generation.
//! 4. [`E_SPAN_MISMATCH`] — `span_sha256` differs from the committed bytes.
//! 5. [`E_GENERATION_MISMATCH`] — `archive_generation` differs from the
//!    holding object's committed generation (decimal).
//! 6. [`E_PROVENANCE_MISMATCH`] — see [`super::provenance::adjudicate`].
//! 7. [`E_TAINT_POLICY`] — `recall_taint == true` requires
//!    `evidence_kind == "recalled_material"` and forbids `origin_class ==
//!    "owner"` (recalled material is never owner testimony).
//!
//! Failures are per item: one failed item never stops the remaining items.
//!
//! # Report
//!
//! `ok` is the gate verdict (every item passed), not a process status: the
//! `learning gate` command always exits `0` when a report is produced, and the
//! learning error table (`2`/`3`/`6`) only covers arguments and unreadable
//! state.
//!
//! # S3: the ingest precondition
//!
//! [`require_passed`] is the same evaluation, used as an optional precondition
//! of `learning ingest` (`--archive` + `--domain`). A report whose `ok` is
//! false stops the ingest with [`GATE_BLOCKED`] (conflict, exit 2) before any
//! `BEGIN IMMEDIATE`; the message carries the failed count and the first
//! failing code. The gate and the ingest are two independent archive/store
//! reads, so they are **not** atomic: the gate proves the archive state as of
//! its own read, not as of the commit.

use std::path::Path;
use std::sync::Arc;

use serde::Serialize;

use crate::archive::{hex_sha256, Archive, ArchiveError, ManifestSnapshot, RecordsFile};

use super::model::{CandidateRecord, EvidenceItem};
use super::provenance::{self, RecordFacts};
use super::{store, LearningError, LearningErrorKind};

/// The only accepted reference prefix in gate v1.
pub const REF_PREFIX: &str = "archive-record:";

pub const E_REF_FORMAT: &str = "E_REF_FORMAT";
pub const E_REF_UNRESOLVED: &str = "E_REF_UNRESOLVED";
pub const E_SPAN_MISMATCH: &str = "E_SPAN_MISMATCH";
pub const E_GENERATION_MISMATCH: &str = "E_GENERATION_MISMATCH";
pub const E_PROVENANCE_MISMATCH: &str = "E_PROVENANCE_MISMATCH";
pub const E_TAINT_POLICY: &str = "E_TAINT_POLICY";

/// The archive root handed to `learning gate` is not an explicit absolute path.
/// It reuses [`store::resolve_root`] and joins the learning error table as a
/// usage (exit 2) code.
pub const GATE_ROOT_INVALID: &str = "GATE_ROOT_INVALID";
/// Committed archive state (manifests / record index) cannot be read at all.
/// Fail-closed: the gate refuses to report a verdict it cannot prove.
pub const GATE_ARCHIVE_UNREADABLE: &str = "GATE_ARCHIVE_UNREADABLE";
/// S3: a gated `learning ingest` was refused because the gate verdict was not
/// `ok`. Conflict class, exit 2, nothing written.
pub const GATE_BLOCKED: &str = "GATE_BLOCKED";

pub const VERDICT_PASSED: &str = "passed";
pub const VERDICT_FAILED: &str = "failed";
pub const LIST_EVIDENCE: &str = "evidence";
pub const LIST_CONTRADICTING: &str = "contradicting_evidence";

/// Evidence kinds that can make an item owner-backed. `owner_backed` is a
/// report flag only; it never changes the candidate's status.
const OWNER_BACKED_KINDS: [&str; 3] = ["user_statement", "correction", "approval"];

/// Per-verdict counters.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct GateCounts {
    pub passed: usize,
    pub failed: usize,
}

/// One adjudicated evidence item.
#[derive(Debug, Clone, Serialize)]
pub struct GateItem {
    pub list: &'static str,
    pub index: usize,
    #[serde(rename = "ref")]
    pub reference: String,
    pub verdict: &'static str,
    pub error_code: Option<&'static str>,
    pub facts: RecordFacts,
}

impl GateItem {
    fn passed(&self) -> bool {
        self.verdict == VERDICT_PASSED
    }

    /// A passed item whose declaration is exactly owner testimony.
    fn owner_backed(&self, item: &EvidenceItem) -> bool {
        self.passed()
            && item.origin_class == "owner"
            && item.session_kind == "interactive"
            && OWNER_BACKED_KINDS.contains(&item.evidence_kind.as_str())
    }
}

/// The single JSON report of `learning gate`.
#[derive(Debug, Clone, Serialize)]
pub struct GateReport {
    pub ok: bool,
    pub candidate_id: String,
    pub owner_backed: bool,
    pub counts: GateCounts,
    pub items: Vec<GateItem>,
}

/// The committed manifest facts that decide provider and generation.
#[derive(Debug, Clone)]
struct CommittedRef {
    provider: String,
    generation: u64,
}

/// Read-only evaluation context: the record index, the committed manifests and
/// one domain-bound read handle.
struct GateContext<'a> {
    archive: &'a Archive,
    domain: &'a str,
    records: RecordsFile,
    manifests: Arc<ManifestSnapshot>,
}

impl<'a> GateContext<'a> {
    fn handle(&self) -> crate::archive::ReadHandle<'a> {
        self.archive.reader(self.domain)
    }

    /// Find the committed proof of one record: the oldest manifest that lists
    /// the record sha, holds the record's object and references it.
    fn committed(&self, sha: &str, object: &str) -> Option<CommittedRef> {
        for manifest in self.manifests.iter() {
            let holds_record = manifest
                .records
                .iter()
                .any(|record| record.sha256 == sha && record.object == object);
            if !holds_record {
                continue;
            }
            if let Some(holding) = manifest.objects.iter().find(|entry| entry.sha256 == object) {
                return Some(CommittedRef {
                    provider: manifest.provider.clone(),
                    generation: holding.generation,
                });
            }
        }
        None
    }

    fn evaluate_item(&self, list: &'static str, index: usize, item: &EvidenceItem) -> GateItem {
        let fail = |error_code: &'static str, facts: RecordFacts| GateItem {
            list,
            index,
            reference: item.reference.clone(),
            verdict: VERDICT_FAILED,
            error_code: Some(error_code),
            facts,
        };
        let pass = |facts: RecordFacts| GateItem {
            list,
            index,
            reference: item.reference.clone(),
            verdict: VERDICT_PASSED,
            error_code: None,
            facts,
        };

        // 1. Reference shape.
        let Some(sha) = parse_reference(&item.reference) else {
            return fail(E_REF_FORMAT, RecordFacts::unknown());
        };

        // 2. Readback through the existing fail-closed read path. A forged
        //    reference, an uncommitted live tail and a denied access mapping are
        //    all "this item cannot be resolved".
        let Some(record_ref) = self.records.records.get(&sha) else {
            return fail(E_REF_UNRESOLVED, RecordFacts::unknown());
        };
        let bytes = match self.handle().read_record(&sha) {
            Ok(bytes) => bytes,
            Err(_) => return fail(E_REF_UNRESOLVED, RecordFacts::unknown()),
        };

        // 3. Committed proof (manifest records + holding object).
        let Some(committed) = self.committed(&sha, &record_ref.object) else {
            return fail(E_REF_UNRESOLVED, RecordFacts::unknown());
        };
        let facts = provenance::facts_for(&committed.provider, &bytes);

        // 4. Span.
        if item.span_sha256 != hex_sha256(&bytes) {
            return fail(E_SPAN_MISMATCH, facts);
        }

        // 5. Committed generation of the holding object.
        if item.archive_generation != committed.generation.to_string() {
            return fail(E_GENERATION_MISMATCH, facts);
        }

        // 6. Structured provenance declaration.
        if provenance::adjudicate(&facts, item).is_err() {
            return fail(E_PROVENANCE_MISMATCH, facts);
        }

        // 7. Recall-taint policy.
        if taint_violation(item) {
            return fail(E_TAINT_POLICY, facts);
        }

        pass(facts)
    }
}

/// `archive-record:<64 lowercase hex>` and nothing else. Shared with the
/// stage-7 `counterexample_ref` shape check so the two cannot drift.
pub(crate) fn parse_reference(reference: &str) -> Option<String> {
    let sha = reference.strip_prefix(REF_PREFIX)?;
    let is_lower_hex = sha.len() == 64
        && sha
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
    if is_lower_hex {
        Some(sha.to_string())
    } else {
        None
    }
}

/// Recalled material is never owner testimony.
fn taint_violation(item: &EvidenceItem) -> bool {
    if !item.recall_taint {
        return false;
    }
    if item.evidence_kind != "recalled_material" {
        return true;
    }
    item.origin_class == "owner"
}

/// Resolve the explicit `--archive` root. It reuses the learning root rules
/// (absolute, no `~`, no empty) under a gate-specific code so the error names
/// the argument that was wrong.
pub fn resolve_archive_root(raw: &str) -> Result<std::path::PathBuf, LearningError> {
    store::resolve_root(Path::new(raw)).map_err(|_| {
        LearningError::new(
            GATE_ROOT_INVALID,
            LearningErrorKind::Usage,
            format!("--archive must be an explicit absolute path, got {raw:?}"),
        )
    })
}

fn archive_unreadable(error: ArchiveError) -> LearningError {
    LearningError::new(
        GATE_ARCHIVE_UNREADABLE,
        LearningErrorKind::Integrity,
        format!("committed archive state is unreadable: {error}"),
    )
}

/// Adjudicate every evidence item of `record` against `archive`.
///
/// `domain` is the access domain used for readback; an empty domain denies
/// every read (the CLI rejects it as usage before calling this).
///
/// The only `Err` outcomes are argument/state failures: an unreadable committed
/// archive (manifests or record index), never an item verdict.
pub fn evaluate(
    record: &CandidateRecord,
    archive: &Archive,
    domain: &str,
) -> Result<GateReport, LearningError> {
    let context = GateContext {
        archive,
        domain,
        records: archive.load_records().map_err(archive_unreadable)?,
        manifests: archive.manifests().map_err(archive_unreadable)?,
    };

    let mut items = Vec::with_capacity(record.evidence.len() + record.contradicting_evidence.len());
    let mut passed = 0usize;
    let mut owner_backed = false;
    for (index, item) in record.evidence.iter().enumerate() {
        let evaluated = context.evaluate_item(LIST_EVIDENCE, index, item);
        if evaluated.passed() {
            passed += 1;
        }
        owner_backed |= evaluated.owner_backed(item);
        items.push(evaluated);
    }
    for (index, item) in record.contradicting_evidence.iter().enumerate() {
        let evaluated = context.evaluate_item(LIST_CONTRADICTING, index, item);
        if evaluated.passed() {
            passed += 1;
        }
        owner_backed |= evaluated.owner_backed(item);
        items.push(evaluated);
    }

    let failed = items.len() - passed;
    Ok(GateReport {
        ok: failed == 0,
        candidate_id: record.candidate_id.clone(),
        owner_backed,
        counts: GateCounts { passed, failed },
        items,
    })
}

/// Open an explicitly rooted archive for a gate run (the `learning gate`
/// command and the S3 ingest precondition share this exact path).
///
/// The root string obeys [`resolve_archive_root`] (`GATE_ROOT_INVALID`, usage,
/// exit 2); an archive that cannot be opened is a state error (exit 3), and an
/// unreadable committed index/manifest set surfaces later as
/// [`GATE_ARCHIVE_UNREADABLE`] from [`evaluate`].
pub fn open_archive(raw: &str) -> Result<Archive, LearningError> {
    let root = resolve_archive_root(raw)?;
    Archive::open(&root).map_err(|error| {
        LearningError::state(format!(
            "archive root {} is not usable: {error}",
            root.display()
        ))
    })
}

/// S3: the gate as an ingest precondition.
///
/// Returns the full report when every item passed. Otherwise `Err(GATE_BLOCKED)`
/// (conflict, exit 2) whose message names the failed count and the first
/// failing code, in gate order (`evidence` before `contradicting_evidence`).
/// An unreadable committed archive still surfaces as [`GATE_ARCHIVE_UNREADABLE`]
/// (integrity, exit 6) — the precondition never invents a verdict.
pub fn require_passed(
    record: &CandidateRecord,
    archive: &Archive,
    domain: &str,
) -> Result<GateReport, LearningError> {
    let report = evaluate(record, archive, domain)?;
    if report.ok {
        return Ok(report);
    }
    let (first_code, first_list, first_index) = report
        .items
        .iter()
        .find_map(|item| item.error_code.map(|code| (code, item.list, item.index)))
        .unwrap_or((E_REF_UNRESOLVED, LIST_EVIDENCE, 0));
    Err(LearningError::new(
        GATE_BLOCKED,
        LearningErrorKind::Conflict,
        format!(
            "gate blocked ingest: {} of {} evidence item(s) failed; first failure {first_code} \
             at {first_list}[{first_index}]",
            report.counts.failed,
            report.items.len()
        ),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_archive_record_refs_with_lowercase_hex_are_accepted() {
        let sha = "a".repeat(64);
        assert_eq!(
            parse_reference(&format!("{REF_PREFIX}{sha}")),
            Some(sha.clone())
        );
        assert_eq!(parse_reference("claude:session-x:line-1"), None);
        assert_eq!(
            parse_reference(&format!("{REF_PREFIX}{}", sha.to_uppercase())),
            None
        );
        assert_eq!(
            parse_reference(&format!("{REF_PREFIX}{}", "a".repeat(63))),
            None
        );
        assert_eq!(
            parse_reference(&format!("{REF_PREFIX}{}g", "a".repeat(63))),
            None
        );
        assert_eq!(parse_reference(&format!("object:{sha}")), None);
    }

    #[test]
    fn taint_requires_recalled_material_and_forbids_owner() {
        let mut item = EvidenceItem {
            reference: format!("{REF_PREFIX}{}", "a".repeat(64)),
            span_sha256: "a".repeat(64),
            archive_generation: "1".to_string(),
            observed_at: "2026-11-01T00:00:00Z".to_string(),
            actor: "user".to_string(),
            session_kind: "interactive".to_string(),
            origin_class: "runtime".to_string(),
            evidence_kind: "recalled_material".to_string(),
            recall_taint: true,
        };
        assert!(!taint_violation(&item));
        item.origin_class = "owner".to_string();
        assert!(taint_violation(&item));
        item.origin_class = "runtime".to_string();
        item.evidence_kind = "user_statement".to_string();
        assert!(taint_violation(&item));
        item.recall_taint = false;
        assert!(!taint_violation(&item));
    }

    #[test]
    fn relative_and_tilde_archive_roots_are_usage_errors() {
        for raw in ["", "archive", "./archive", "~/archive"] {
            let error = resolve_archive_root(raw).expect_err("root must be rejected");
            assert_eq!(error.code, GATE_ROOT_INVALID);
            assert_eq!(error.exit_code(), 2);
        }
    }
}
