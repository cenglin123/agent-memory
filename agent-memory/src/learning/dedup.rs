//! Stage 3 S3: the dedup disposition record (`learning dedup`).
//!
//! S1 froze `dedup` at `pending` and S3 is the step that opens the *result* up.
//! One invocation takes a dedup tool's machine-readable report (for the KB
//! workflow: `ask.py --check --scope memory --json`; in tests: synthetic JSON),
//! the agent's triage decision and the agent's reason, and records both the
//! new `dedup` block on the candidate and one append-only `dedup_history` row.
//!
//! # What is, and is not, interpreted
//!
//! The report contract is deliberately shallow: the top level must parse as a
//! JSON object with a non-empty `query` string and a `results` array. Every
//! `results` element is stored verbatim in `matches` — no scoring, ranking,
//! merging or semantic reading. Anything else is
//! [`DEDUP_REPORT_INVALID`] (usage, exit 2).
//!
//! `--reason` is mandatory after trimming: the point of the command is that an
//! agent must classify a hit as one of [`DISPOSITIONS`] **and** say why.
//!
//! `contradicts` only records the disposition. It never writes
//! `contradicting_evidence` (counter-evidence must be a gate-approved archive
//! record) and never changes `status`; the candidate waits for human review.
//!
//! # Storage
//!
//! Exactly one `BEGIN IMMEDIATE` transaction: the candidate is read, its
//! `dedup` block is replaced with `{query, matches, disposition, checked_at}`,
//! the record is written back with a refreshed `payload_sha256` (dedup is
//! workflow state, so the payload hash moves with it), and one `dedup_history`
//! row is appended. `status`, `contradicting_evidence`, `revision`,
//! `candidate_revisions`, `ingestions` and `status_history` are never touched.
//! History rows are only appended; a repeated check never overwrites an
//! earlier row.

use rusqlite::{params, Transaction, TransactionBehavior};
use serde::Serialize;
use serde_json::Value;

use crate::archive::hex_sha256;

use super::model::{DedupState, TASK_TARGET_KIND};
use super::store::{self, LearningStore};
use super::task::not_routeable;
use super::{LearningError, LearningErrorKind};

/// The report does not satisfy the minimal dedup-report contract.
pub const DEDUP_REPORT_INVALID: &str = "DEDUP_REPORT_INVALID";

/// The four triage outcomes an agent must choose from.
pub const DISPOSITIONS: [&str; 4] = ["duplicate", "related", "contradicts", "new"];

/// One validated `learning dedup` invocation.
#[derive(Debug, Clone)]
pub struct DedupRequest {
    pub candidate_id: String,
    pub query: String,
    pub matches: Vec<Value>,
    pub disposition: String,
    pub reason: String,
}

fn invalid_report(message: impl Into<String>) -> LearningError {
    LearningError::new(DEDUP_REPORT_INVALID, LearningErrorKind::Usage, message)
}

impl DedupRequest {
    /// Validate the report, the disposition and the reason, and resolve the
    /// candidate id. Nothing is read from or written to the store here.
    pub fn parse(
        candidate_id: &str,
        report: &Value,
        disposition: &str,
        reason: Option<&str>,
    ) -> Result<Self, LearningError> {
        let candidate_id = candidate_id.trim();
        if candidate_id.is_empty() {
            return Err(LearningError::usage(
                "--candidate-id must be a non-empty candidate id",
            ));
        }

        let body = report.as_object().ok_or_else(|| {
            invalid_report(
                "--report must be a JSON object (the dedup tool's machine-readable output)",
            )
        })?;
        let query = body
            .get("query")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| invalid_report("--report must carry a non-empty \"query\" string"))?;
        let matches = body
            .get("results")
            .and_then(Value::as_array)
            .ok_or_else(|| invalid_report("--report must carry a \"results\" array"))?
            .clone();

        let disposition = disposition.trim();
        if !DISPOSITIONS.contains(&disposition) {
            return Err(LearningError::usage(format!(
                "--disposition must be one of {DISPOSITIONS:?}, got {disposition:?}"
            )));
        }

        let reason = reason
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                LearningError::usage(
                    "--reason is required: the agent must state why this hit is \
                     duplicate/related/contradicts/new",
                )
            })?;

        Ok(Self {
            candidate_id: candidate_id.to_string(),
            query: query.to_string(),
            matches,
            disposition: disposition.to_string(),
            reason: reason.to_string(),
        })
    }
}

/// Result of one dedup disposition record. Field names are the CLI contract.
#[derive(Debug, Clone, Serialize)]
pub struct DedupOutcome {
    pub ok: bool,
    pub candidate_id: String,
    pub revision: u64,
    pub status: String,
    pub disposition: String,
    pub checked_at: String,
    pub matches: usize,
    pub history_seq: i64,
    pub payload_sha256: String,
}

impl LearningStore {
    /// Record one dedup disposition atomically (see the module docs).
    ///
    /// A `target_kind = "task"` candidate is not a dedup subject (R11): it is
    /// refused with [`super::task::TASK_NOT_ROUTEABLE`] before the write
    /// transaction opens, so a task candidate is never mutated here.
    pub fn record_dedup(&mut self, request: &DedupRequest) -> Result<DedupOutcome, LearningError> {
        if let Some((record, _)) = store::load_current(self.connection(), &request.candidate_id)? {
            if record.target_kind == TASK_TARGET_KIND {
                return Err(not_routeable(&record.candidate_id, "learning dedup"));
            }
        }
        let tx = self
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(store::db_error)?;
        let outcome = record_in_transaction(&tx, request)?;
        tx.commit().map_err(store::db_error)?;
        Ok(outcome)
    }
}

fn record_in_transaction(
    tx: &Transaction<'_>,
    request: &DedupRequest,
) -> Result<DedupOutcome, LearningError> {
    let Some((mut record, _)) = store::load_current(tx, &request.candidate_id)? else {
        return Err(LearningError::not_found(format!(
            "no candidate {} in this learning root",
            request.candidate_id
        )));
    };

    let checked_at = store::storage_now();
    let matches_json = serde_json::to_string(&request.matches).map_err(|error| {
        LearningError::state(format!("dedup matches cannot be serialized: {error}"))
    })?;

    record.dedup = DedupState {
        query: request.query.clone(),
        matches: request.matches.clone(),
        disposition: request.disposition.clone(),
        checked_at: Some(checked_at.clone()),
    };
    let record_json = record.canonical_json();
    let payload_sha256 = hex_sha256(record_json.as_bytes());

    tx.execute(
        "UPDATE candidates SET record_json = ?2, payload_sha256 = ?3 WHERE candidate_id = ?1",
        params![record.candidate_id, record_json, payload_sha256],
    )
    .map_err(store::db_error)?;

    tx.execute(
        "INSERT INTO dedup_history \
         (candidate_id, query, matches_json, disposition, reason, checked_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            record.candidate_id,
            request.query,
            matches_json,
            request.disposition,
            request.reason,
            checked_at
        ],
    )
    .map_err(store::db_error)?;
    let history_seq = tx.last_insert_rowid();

    Ok(DedupOutcome {
        ok: true,
        candidate_id: record.candidate_id,
        revision: record.revision,
        status: record.status,
        disposition: request.disposition.clone(),
        checked_at,
        matches: request.matches.len(),
        history_seq,
        payload_sha256,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn report_contract_is_shallow_but_enforced() {
        let ok = json!({
            "query": "  explicit roots ",
            "results": [{"id": 1, "score": 0.5, "text": "keep me verbatim"}],
            "extra": {"ignored": true}
        });
        let request = DedupRequest::parse("cand_x", &ok, "duplicate", Some(" same rule "))
            .expect("well-formed request");
        assert_eq!(request.query, "explicit roots");
        assert_eq!(
            request.matches,
            vec![json!({"id": 1, "score": 0.5, "text": "keep me verbatim"})]
        );
        assert_eq!(request.disposition, "duplicate");
        assert_eq!(request.reason, "same rule");

        for report in [
            json!({"results": []}),
            json!({"query": "", "results": []}),
            json!({"query": "   ", "results": []}),
            json!({"query": "q"}),
            json!({"query": "q", "results": {}}),
            json!({"query": 7, "results": []}),
            json!([]),
            json!("query"),
        ] {
            let error = DedupRequest::parse("cand_x", &report, "new", Some("reason"))
                .expect_err("report must be rejected");
            assert_eq!(error.code, DEDUP_REPORT_INVALID, "report: {report}");
            assert_eq!(error.exit_code(), 2);
        }
    }

    #[test]
    fn disposition_and_reason_are_mandatory() {
        let report = json!({"query": "q", "results": []});
        for disposition in ["", "pending", "Duplicate", "new "] {
            let expected = disposition.trim();
            let outcome = DedupRequest::parse("cand_x", &report, disposition, Some("why"));
            if DISPOSITIONS.contains(&expected) {
                assert!(outcome.is_ok(), "{disposition:?}");
            } else {
                let error = outcome.expect_err("disposition must be rejected");
                assert_eq!(error.code, "INVALID_USAGE", "{disposition:?}");
                assert_eq!(error.exit_code(), 2);
            }
        }

        for reason in [None, Some(""), Some("  \t ")] {
            let error = DedupRequest::parse("cand_x", &report, "new", reason)
                .expect_err("a blank reason is a usage error");
            assert_eq!(error.code, "INVALID_USAGE");
            assert_eq!(error.exit_code(), 2);
        }

        let error = DedupRequest::parse("   ", &report, "new", Some("why"))
            .expect_err("a blank candidate id is a usage error");
        assert_eq!(error.exit_code(), 2);
    }
}
