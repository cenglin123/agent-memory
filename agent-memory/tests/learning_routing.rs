//! Stage 3 S5: schema v3, reuse/validation event records, per-target
//! requirement reports and the review-package export.
//!
//! Every case uses synthetic temporary archive/learning roots and synthetic
//! Claude JSONL. Nothing here reads a real session root, a real archive, a real
//! learning root, `~/.memory` or a recall configuration.

use std::path::{Path, PathBuf};
use std::process::Command;

use rusqlite::Connection;
use serde_json::{json, Value};

use agent_memory::archive::{hex_sha256, Archive};
use agent_memory::capture::capture_source;
use agent_memory::learning::model::validate_candidate;
use agent_memory::learning::routing;
use agent_memory::learning::{
    DedupRequest, LearningStore, ReuseRequest, ValidationRequest, DB_FILE, DB_SCHEMA_VERSION,
    REUSE_TASK_CONFLICT,
};

const DOMAIN: &str = "default";

// ---------------------------------------------------------------------------
// Schema fixtures
// ---------------------------------------------------------------------------

/// The S1 (v1) schema, written by hand.
const V1_SCHEMA: &str = "
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
";

/// The S3 (v2) schema: v1 plus `dedup_history`.
const V2_SCHEMA_TAIL: &str = "
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
";

// ---------------------------------------------------------------------------
// Candidate fixtures
// ---------------------------------------------------------------------------

fn span_sha256(tag: &str) -> String {
    let mut acc: u64 = 14_695_981_039_346_656_037;
    for byte in tag.as_bytes() {
        acc ^= u64::from(*byte);
        acc = acc.wrapping_mul(1_099_511_628_211);
    }
    format!("{acc:064x}")
}

fn evidence_decl(tag: &str, origin_class: &str, session_kind: &str, evidence_kind: &str) -> Value {
    json!({
        "ref": format!("claude:session-{tag}:line-1"),
        "span_sha256": span_sha256(tag),
        "archive_generation": "1",
        "observed_at": "2026-11-01T00:00:00Z",
        "actor": "user",
        "session_kind": session_kind,
        "origin_class": origin_class,
        "evidence_kind": evidence_kind,
        "recall_taint": false
    })
}

fn evidence(tag: &str) -> Value {
    evidence_decl(tag, "owner", "interactive", "user_statement")
}

fn candidate(target_kind: &str, claim: &str) -> Value {
    json!({
        "schema_version": 1,
        "candidate_id": "",
        "revision": 1,
        "status": "candidate",
        "claim": claim,
        "target_kind": target_kind,
        "proposed_target": format!("AGENTS.md#{target_kind}"),
        "applicability": ["agent-memory"],
        "counterexamples": [],
        "unknowns": [],
        "evidence": [evidence("e1")],
        "contradicting_evidence": [],
        "dedup": {
            "query": "",
            "matches": [],
            "disposition": "pending",
            "checked_at": null
        },
        "reuse": [],
        "validation": [],
        "promotion": {
            "approval": null,
            "apply": null,
            "verification": null
        },
        "ingestions": [],
        "source_checkpoint": "checkpoint-a",
        "extractor": "extractor-a",
        "created_at": "2026-11-01T00:00:00Z",
        "updated_at": "2026-11-01T00:00:00Z"
    })
}

fn candidate_with(
    target_kind: &str,
    claim: &str,
    evidence_list: Vec<Value>,
    contradicting: Vec<Value>,
) -> Value {
    let mut value = candidate(target_kind, claim);
    value["evidence"] = json!(evidence_list);
    value["contradicting_evidence"] = json!(contradicting);
    value
}

fn named(value: &Value, claim: &str) -> Value {
    let mut value = value.clone();
    value["claim"] = json!(claim);
    value
}

// ---------------------------------------------------------------------------
// Store helpers
// ---------------------------------------------------------------------------

fn cli() -> Command {
    Command::new(env!("CARGO_BIN_EXE_agent-memory"))
}

fn stdout_json(out: &std::process::Output) -> Value {
    serde_json::from_slice(&out.stdout).unwrap_or_else(|error| {
        panic!(
            "stdout is not JSON ({error}): {}",
            String::from_utf8_lossy(&out.stdout)
        )
    })
}

fn stderr_json(out: &std::process::Output) -> Value {
    serde_json::from_slice(&out.stderr).unwrap_or_else(|error| {
        panic!(
            "stderr is not JSON ({error}): {}",
            String::from_utf8_lossy(&out.stderr)
        )
    })
}

fn init_root(root: &Path) -> LearningStore {
    LearningStore::init(root).expect("init learning root")
}

fn db_of(root: &Path) -> PathBuf {
    root.join(DB_FILE)
}

fn count(root: &Path, table: &str) -> i64 {
    let conn = Connection::open(db_of(root)).expect("open learning.db");
    conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
        row.get(0)
    })
    .expect("count rows")
}

fn all_counts(root: &Path) -> [i64; 7] {
    [
        count(root, "candidates"),
        count(root, "candidate_revisions"),
        count(root, "ingestions"),
        count(root, "status_history"),
        count(root, "dedup_history"),
        count(root, "reuse_history"),
        count(root, "validation_history"),
    ]
}

fn table_names(root: &Path) -> Vec<String> {
    let conn = Connection::open(db_of(root)).expect("open learning.db");
    let mut stmt = conn
        .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
        .expect("prepare table list");
    stmt.query_map([], |row| row.get::<_, String>(0))
        .expect("query table list")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect table list")
}

fn user_version(root: &Path) -> i64 {
    let conn = Connection::open(db_of(root)).expect("open learning.db");
    conn.query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("user_version")
}

#[derive(Debug)]
struct ReuseRow {
    seq: i64,
    candidate_id: String,
    task_id: String,
    outcome: String,
    notes: String,
    recorded_at: String,
}

fn reuse_rows(root: &Path, candidate_id: &str) -> Vec<ReuseRow> {
    let conn = Connection::open(db_of(root)).expect("open learning.db");
    let mut stmt = conn
        .prepare(
            "SELECT seq, candidate_id, task_id, outcome, notes, recorded_at \
             FROM reuse_history WHERE candidate_id = ?1 ORDER BY seq",
        )
        .expect("prepare reuse history");
    stmt.query_map([candidate_id], |row| {
        Ok(ReuseRow {
            seq: row.get(0)?,
            candidate_id: row.get(1)?,
            task_id: row.get(2)?,
            outcome: row.get(3)?,
            notes: row.get(4)?,
            recorded_at: row.get(5)?,
        })
    })
    .expect("query reuse history")
    .collect::<Result<Vec<_>, _>>()
    .expect("collect reuse history")
}

#[derive(Debug)]
struct ValidationRow {
    seq: i64,
    candidate_id: String,
    reviewer: String,
    verdict: String,
    report_ref: String,
    recorded_at: String,
}

fn validation_rows(root: &Path, candidate_id: &str) -> Vec<ValidationRow> {
    let conn = Connection::open(db_of(root)).expect("open learning.db");
    let mut stmt = conn
        .prepare(
            "SELECT seq, candidate_id, reviewer, verdict, report_ref, recorded_at \
             FROM validation_history WHERE candidate_id = ?1 ORDER BY seq",
        )
        .expect("prepare validation history");
    stmt.query_map([candidate_id], |row| {
        Ok(ValidationRow {
            seq: row.get(0)?,
            candidate_id: row.get(1)?,
            reviewer: row.get(2)?,
            verdict: row.get(3)?,
            report_ref: row.get(4)?,
            recorded_at: row.get(5)?,
        })
    })
    .expect("query validation history")
    .collect::<Result<Vec<_>, _>>()
    .expect("collect validation history")
}

fn looks_rfc3339(value: &str) -> bool {
    let bytes = value.as_bytes();
    value.len() >= 20
        && bytes.get(4) == Some(&b'-')
        && bytes.get(7) == Some(&b'-')
        && bytes.get(10) == Some(&b'T')
        && bytes.get(13) == Some(&b':')
}

fn record_dedup(store: &mut LearningStore, candidate_id: &str) {
    let report = json!({"query": "synthetic dedup query", "results": []});
    let request = DedupRequest::parse(candidate_id, &report, "new", Some("synthetic check"))
        .expect("dedup request");
    store.record_dedup(&request).expect("record dedup");
}

fn ingest(store: &mut LearningStore, value: &Value) -> String {
    // One ingest key per test candidate: the checkpoint follows the claim so
    // several drafts can live in one synthetic root.
    let mut value = value.clone();
    let claim = value["claim"].as_str().unwrap_or_default();
    value["source_checkpoint"] = json!(format!("checkpoint-{claim}"));
    store.ingest(&value).expect("ingest candidate").candidate_id
}

fn requirement<'a>(report: &'a routing::TargetReport, name: &str) -> &'a routing::Requirement {
    report
        .requirements
        .iter()
        .find(|item| item.requirement == name)
        .unwrap_or_else(|| panic!("missing requirement {name}: {:?}", report.requirements))
}

fn blocking_has(report: &routing::TargetReport, code: &str) -> bool {
    report.blocking.iter().any(|item| item == code)
}

// ---------------------------------------------------------------------------
// 1. schema v2 -> v3
// ---------------------------------------------------------------------------

/// Write a v2 learning database by hand with one committed candidate and one
/// dedup history row.
fn write_v2_database(root: &Path) -> (String, String) {
    std::fs::create_dir_all(root).expect("v2 root");
    let db = db_of(root);
    let conn = Connection::open(&db).expect("open v2 db");
    conn.execute_batch(&format!("{V1_SCHEMA}{V2_SCHEMA_TAIL}"))
        .expect("v2 schema");
    conn.pragma_update(None, "user_version", 2)
        .expect("user_version 2");

    let value = candidate("rule", "Prefer explicit roots for every store");
    let validated = validate_candidate(&value).expect("valid v1 candidate");
    let record = &validated.record;
    conn.execute(
        "INSERT INTO candidates \
         (candidate_id, revision, status, record_json, payload_sha256, created_at, updated_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        rusqlite::params![
            record.candidate_id,
            record.revision as i64,
            record.status,
            validated.payload_json,
            validated.payload_sha256,
            record.created_at,
            record.updated_at
        ],
    )
    .expect("insert v2 candidate");
    conn.execute(
        "INSERT INTO candidate_revisions \
         (candidate_id, revision, status, record_json, payload_sha256, recorded_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, '2026-11-01T00:00:01.000Z')",
        rusqlite::params![
            record.candidate_id,
            record.revision as i64,
            record.status,
            validated.payload_json,
            validated.payload_sha256
        ],
    )
    .expect("insert v2 revision");
    conn.execute(
        "INSERT INTO ingestions \
         (ingest_key, candidate_id, revision, payload_sha256, extractor, source_checkpoint, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, '2026-11-01T00:00:01.000Z')",
        rusqlite::params![
            validated.ingest_key,
            record.candidate_id,
            record.revision as i64,
            validated.payload_sha256,
            record.extractor,
            record.source_checkpoint
        ],
    )
    .expect("insert v2 ingestion");
    conn.execute(
        "INSERT INTO status_history (candidate_id, revision, status, recorded_at) \
         VALUES (?1, ?2, 'candidate', '2026-11-01T00:00:01.000Z')",
        rusqlite::params![record.candidate_id, record.revision as i64],
    )
    .expect("insert v2 status");
    conn.execute(
        "INSERT INTO dedup_history \
         (candidate_id, query, matches_json, disposition, reason, checked_at) \
         VALUES (?1, 'q', '[]', 'new', 'kept across the migration', '2026-11-01T00:00:02.000Z')",
        rusqlite::params![record.candidate_id],
    )
    .expect("insert v2 dedup history");
    (
        record.candidate_id.clone(),
        validated.payload_sha256.clone(),
    )
}

#[test]
fn v2_database_migrates_to_v3_reentrantly_and_foreign_databases_fail_closed() {
    let dir = tempfile::tempdir().expect("temp root");
    let root = dir.path().join("v2");
    let (candidate_id, payload_sha256) = write_v2_database(&root);

    assert_eq!(user_version(&root), 2);
    assert!(!table_names(&root).contains(&"reuse_history".to_string()));
    assert!(!table_names(&root).contains(&"validation_history".to_string()));

    let mut store = LearningStore::open(&root).expect("open migrates v2 to v3");
    assert_eq!(user_version(&root), i64::from(DB_SCHEMA_VERSION));
    assert!(table_names(&root).contains(&"reuse_history".to_string()));
    assert!(table_names(&root).contains(&"validation_history".to_string()));

    // Every v2 row survives and is readable through the normal path.
    let view = store.show(&candidate_id).expect("show after migration");
    assert_eq!(view.payload_sha256, payload_sha256);
    assert_eq!(
        view.candidate.claim,
        "Prefer explicit roots for every store"
    );
    assert_eq!(view.candidate.dedup.disposition, "pending");
    assert_eq!(view.ingestions.len(), 1);
    assert_eq!(view.revisions.len(), 1);
    assert_eq!(view.status_history.len(), 1);
    assert_eq!(count(&root, "dedup_history"), 1);
    assert_eq!(count(&root, "reuse_history"), 0);
    assert_eq!(count(&root, "validation_history"), 0);

    // The migrated store accepts the new event tables.
    let request = ReuseRequest::parse(
        &candidate_id,
        "task-after-migration",
        "pass",
        None,
        "test:actor",
    )
    .expect("reuse request");
    store.record_reuse(&request).expect("reuse after migration");
    assert_eq!(count(&root, "reuse_history"), 1);
    drop(store);

    // Re-opening a v3 store is a no-op migration.
    let store = LearningStore::open(&root).expect("re-open v3");
    assert_eq!(user_version(&root), i64::from(DB_SCHEMA_VERSION));
    assert_eq!(count(&root, "candidates"), 1);
    assert_eq!(count(&root, "reuse_history"), 1);
    drop(store);

    // `init` creates v3 directly.
    let fresh = dir.path().join("fresh");
    let fresh_store = init_root(&fresh);
    assert_eq!(user_version(&fresh), i64::from(DB_SCHEMA_VERSION));
    assert!(table_names(&fresh).contains(&"reuse_history".to_string()));
    assert!(table_names(&fresh).contains(&"validation_history".to_string()));
    assert_eq!(fresh_store.db_path(), db_of(&fresh));
    drop(fresh_store);

    // A foreign v2 database is rejected before any migration.
    let foreign = dir.path().join("foreign");
    std::fs::create_dir_all(&foreign).expect("foreign root");
    {
        let conn = Connection::open(db_of(&foreign)).expect("foreign db");
        conn.execute_batch(
            "CREATE TABLE archive_objects (sha256 TEXT PRIMARY KEY, bytes INTEGER);",
        )
        .expect("foreign schema");
        conn.pragma_update(None, "user_version", 2)
            .expect("user_version 2");
    }
    let error = LearningStore::open(&foreign).expect_err("foreign v2");
    assert_eq!(error.code, "LEARNING_SCHEMA_INVALID");
    assert_eq!(error.exit_code(), 6);
    assert_eq!(user_version(&foreign), 2);
    assert!(!table_names(&foreign).contains(&"reuse_history".to_string()));

    // A partial v2 database is rejected without being migrated.
    let partial = dir.path().join("partial");
    std::fs::create_dir_all(&partial).expect("partial root");
    {
        let conn = Connection::open(db_of(&partial)).expect("partial db");
        conn.execute_batch("CREATE TABLE candidates (candidate_id TEXT PRIMARY KEY);")
            .expect("partial schema");
        conn.pragma_update(None, "user_version", 2)
            .expect("user_version 2");
    }
    let error = LearningStore::open(&partial).expect_err("partial v2");
    assert_eq!(error.code, "LEARNING_SCHEMA_INVALID");
    assert_eq!(user_version(&partial), 2);
    assert!(!table_names(&partial).contains(&"reuse_history".to_string()));

    // A future version is not silently migrated.
    let future = dir.path().join("future");
    init_root(&future);
    {
        let conn = Connection::open(db_of(&future)).expect("future db");
        conn.pragma_update(None, "user_version", 6)
            .expect("user_version 6");
    }
    let error = LearningStore::open(&future).expect_err("future version");
    assert_eq!(error.code, "SCHEMA_VERSION_UNSUPPORTED");
    assert_eq!(error.exit_code(), 6);
    assert_eq!(user_version(&future), 6);
}

// ---------------------------------------------------------------------------
// 2. reuse events
// ---------------------------------------------------------------------------

#[test]
fn reuse_records_append_history_and_refresh_the_record() {
    let dir = tempfile::tempdir().expect("temp root");
    let root = dir.path().join("learning");
    let root_text = root.to_string_lossy().to_string();
    let mut store = init_root(&root);
    let ingested = store
        .ingest(&candidate("workflow", "Reuse the explicit-root workflow"))
        .expect("ingest");
    let candidate_id = ingested.candidate_id.clone();

    let first = ReuseRequest::parse(
        &candidate_id,
        " task-1 ",
        "pass",
        Some(" first run "),
        "test:actor",
    )
    .expect("first reuse");
    let first_outcome = store.record_reuse(&first).expect("record first reuse");
    assert!(first_outcome.ok);
    assert_eq!(first_outcome.task_id, "task-1");
    assert_eq!(first_outcome.outcome, "pass");
    assert_eq!(first_outcome.notes, "first run");
    assert_eq!(first_outcome.reuse_total, 1);
    assert_eq!(first_outcome.revision, 1);
    assert_eq!(first_outcome.status, "candidate");
    assert!(looks_rfc3339(&first_outcome.recorded_at));
    assert_ne!(first_outcome.payload_sha256, ingested.payload_sha256);

    let second =
        ReuseRequest::parse(&candidate_id, "task-2", "fail", None, "test:actor").expect("second");
    let second_outcome = store.record_reuse(&second).expect("record second reuse");
    assert_eq!(second_outcome.reuse_total, 2);
    assert_eq!(second_outcome.notes, "");
    assert!(second_outcome.history_seq > first_outcome.history_seq);

    let view = store.show(&candidate_id).expect("show");
    assert_eq!(view.candidate.revision, 1);
    assert_eq!(view.candidate.status, "candidate");
    assert_eq!(view.candidate.evidence.len(), 1);
    assert_eq!(view.candidate.reuse.len(), 2);
    assert_eq!(view.candidate.reuse[0]["task_id"], "task-1");
    assert_eq!(view.candidate.reuse[0]["outcome"], "pass");
    assert_eq!(
        view.candidate.reuse[0]["recorded_at"].as_str(),
        Some(first_outcome.recorded_at.as_str())
    );
    assert_eq!(view.candidate.reuse[1]["task_id"], "task-2");
    assert_eq!(view.candidate.reuse[1]["outcome"], "fail");
    assert_eq!(
        view.candidate.reuse[1]["recorded_at"].as_str(),
        Some(second_outcome.recorded_at.as_str())
    );
    assert_eq!(
        view.payload_sha256, second_outcome.payload_sha256,
        "the stored payload hash follows the record"
    );
    assert_eq!(
        view.revisions[0].payload_sha256, ingested.payload_sha256,
        "the revision snapshot is never rewritten"
    );
    assert_eq!(view.status_history.len(), 1);
    assert_eq!(view.ingestions.len(), 1);

    // A recorded reuse event never freezes a same-revision evidence append
    // (reuse is workflow state, exactly like dedup).
    let mut appended = candidate("workflow", "Reuse the explicit-root workflow");
    appended["source_checkpoint"] = json!("checkpoint-b");
    appended["updated_at"] = json!("2026-11-01T01:00:00Z");
    appended["evidence"] = json!([evidence("e1"), evidence("e2")]);
    let merge = store.ingest(&appended).expect("append after reuse");
    assert!(!merge.replayed);
    assert_eq!(merge.evidence_added, 1);
    let view = store.show(&candidate_id).expect("show after append");
    assert_eq!(view.candidate.evidence.len(), 2);
    assert_eq!(
        view.candidate.reuse.len(),
        2,
        "an append keeps the recorded reuse events"
    );
    assert_eq!(view.candidate.reuse[0]["task_id"], "task-1");

    let rows = reuse_rows(&root, &candidate_id);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].candidate_id, candidate_id);
    assert_eq!(rows[0].task_id, "task-1");
    assert_eq!(rows[0].outcome, "pass");
    assert_eq!(rows[0].notes, "first run");
    assert_eq!(rows[0].recorded_at, first_outcome.recorded_at);
    assert_eq!(rows[1].task_id, "task-2");
    assert_eq!(rows[1].outcome, "fail");
    assert_eq!(rows[1].notes, "");
    assert!(rows[1].seq > rows[0].seq);

    // A repeated task is a conflict and writes nothing.
    let duplicate =
        ReuseRequest::parse(&candidate_id, "task-1", "fail", Some("again"), "test:actor")
            .expect("request validates");
    let error = store
        .record_reuse(&duplicate)
        .expect_err("duplicate task must be refused");
    assert_eq!(error.code, REUSE_TASK_CONFLICT);
    assert_eq!(error.exit_code(), 2);
    assert_eq!(count(&root, "reuse_history"), 2);
    assert_eq!(
        store
            .show(&candidate_id)
            .expect("show")
            .candidate
            .reuse
            .len(),
        2
    );

    // A blank task cannot name a task; an invalid outcome is a usage error.
    let blank = ReuseRequest::parse(&candidate_id, "   ", "pass", None, "test:actor")
        .expect_err("blank task must be refused");
    assert_eq!(blank.code, REUSE_TASK_CONFLICT);
    let invalid = ReuseRequest::parse(&candidate_id, "task-3", "maybe", None, "test:actor")
        .expect_err("invalid outcome must be refused");
    assert_eq!(invalid.code, "INVALID_USAGE");

    // An unknown candidate is not found and writes nothing.
    let missing =
        ReuseRequest::parse("cand_missing", "task-1", "pass", None, "test:actor").expect("request");
    let error = store.record_reuse(&missing).expect_err("unknown candidate");
    assert_eq!(error.code, "CANDIDATE_NOT_FOUND");
    assert_eq!(error.exit_code(), 6);
    assert_eq!(count(&root, "reuse_history"), 2);
    drop(store);

    // CLI success path.
    let out = cli()
        .args(["learning", "reuse", "--root", &root_text])
        .args(["--candidate-id", &candidate_id, "--task", "task-3"])
        .args(["--outcome", "pass", "--notes", "through the cli"])
        .args(["--actor", "test:actor"])
        .output()
        .expect("cli reuse");
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let json = stdout_json(&out);
    assert_eq!(json["task_id"], "task-3");
    assert_eq!(json["reuse_total"], 3);
    assert_eq!(count(&root, "reuse_history"), 3);

    // CLI refusals: duplicate task, invalid outcome, unknown candidate.
    for (id, task, outcome, code, error_code) in [
        (
            candidate_id.as_str(),
            "task-1",
            "pass",
            2,
            REUSE_TASK_CONFLICT,
        ),
        (candidate_id.as_str(), "task-4", "maybe", 2, "INVALID_USAGE"),
        ("cand_missing", "task-4", "pass", 6, "CANDIDATE_NOT_FOUND"),
    ] {
        let out = cli()
            .args(["learning", "reuse", "--root", &root_text])
            .args(["--candidate-id", id, "--task", task, "--outcome", outcome])
            .args(["--actor", "test:actor"])
            .output()
            .expect("cli reuse failure");
        assert_eq!(out.status.code(), Some(code), "{id}/{task}/{outcome}");
        assert_eq!(stderr_json(&out)["error"]["code"], error_code);
    }
    assert_eq!(count(&root, "reuse_history"), 3);
}

// ---------------------------------------------------------------------------
// 3. validation events
// ---------------------------------------------------------------------------

#[test]
fn validation_rounds_are_all_kept_and_invalid_input_is_rejected() {
    let dir = tempfile::tempdir().expect("temp root");
    let root = dir.path().join("learning");
    let root_text = root.to_string_lossy().to_string();
    let mut store = init_root(&root);
    let ingested = store
        .ingest(&candidate("skill", "Keep every validation round"))
        .expect("ingest");
    let candidate_id = ingested.candidate_id.clone();

    let rounds = [
        ("reviewer-a", "pass", "report-1", Some("first round")),
        ("reviewer-a", "fail", "report-2", None),
        ("reviewer-b", "pass", "report-3", Some("second reviewer")),
    ];
    let mut outcomes = Vec::new();
    for (reviewer, verdict, report_ref, notes) in rounds {
        let request = ValidationRequest::parse(&candidate_id, reviewer, verdict, report_ref, notes)
            .expect("request");
        outcomes.push(
            store
                .record_validation(&request)
                .expect("record validation"),
        );
    }
    assert_eq!(outcomes[0].validation_total, 1);
    assert_eq!(outcomes[1].validation_total, 2);
    assert_eq!(outcomes[2].validation_total, 3);
    assert_eq!(outcomes[0].reviewer, "reviewer-a");
    assert_eq!(outcomes[1].verdict, "fail");
    assert_eq!(outcomes[2].report_ref, "report-3");
    assert_eq!(outcomes[2].revision, 1);
    assert_eq!(outcomes[2].status, "candidate");
    assert!(looks_rfc3339(&outcomes[2].recorded_at));
    assert!(outcomes[2].history_seq > outcomes[1].history_seq);

    let view = store.show(&candidate_id).expect("show");
    assert_eq!(view.candidate.validation.len(), 3);
    assert_eq!(view.candidate.validation[0]["reviewer"], "reviewer-a");
    assert_eq!(view.candidate.validation[1]["verdict"], "fail");
    assert_eq!(view.candidate.validation[2]["report_ref"], "report-3");
    assert_eq!(
        view.candidate.validation[2]["recorded_at"].as_str(),
        Some(outcomes[2].recorded_at.as_str())
    );
    assert_eq!(view.payload_sha256, outcomes[2].payload_sha256);
    assert_eq!(view.revisions[0].payload_sha256, ingested.payload_sha256);
    assert_eq!(view.status_history.len(), 1);

    let rows = validation_rows(&root, &candidate_id);
    assert_eq!(
        rows.len(),
        3,
        "every round is kept, including the same reviewer"
    );
    assert_eq!(rows[0].candidate_id, candidate_id);
    assert_eq!(rows[0].verdict, "pass");
    assert_eq!(rows[0].recorded_at, outcomes[0].recorded_at);
    assert_eq!(rows[1].verdict, "fail");
    assert_eq!(rows[1].report_ref, "report-2");
    assert_eq!(rows[2].reviewer, "reviewer-b");
    assert!(rows[2].seq > rows[1].seq);

    // Invalid input is refused before anything is written.
    let error = ValidationRequest::parse(&candidate_id, "reviewer-a", "passed", "report-4", None)
        .expect_err("invalid verdict");
    assert_eq!(error.code, "INVALID_USAGE");
    assert_eq!(error.exit_code(), 2);
    let error = ValidationRequest::parse(&candidate_id, "  ", "pass", "report-4", None)
        .expect_err("blank reviewer");
    assert_eq!(error.code, "INVALID_USAGE");
    let error = ValidationRequest::parse(&candidate_id, "reviewer-a", "pass", " ", None)
        .expect_err("blank report ref");
    assert_eq!(error.code, "INVALID_USAGE");
    let missing = ValidationRequest::parse("cand_missing", "reviewer-a", "pass", "report-4", None)
        .expect("request");
    let error = store
        .record_validation(&missing)
        .expect_err("unknown candidate");
    assert_eq!(error.code, "CANDIDATE_NOT_FOUND");
    assert_eq!(count(&root, "validation_history"), 3);
    drop(store);

    // CLI success and refusal.
    let out = cli()
        .args(["learning", "validate", "--root", &root_text])
        .args(["--candidate-id", &candidate_id, "--reviewer", "reviewer-c"])
        .args(["--verdict", "pass", "--report-ref", "report-4"])
        .output()
        .expect("cli validate");
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(stdout_json(&out)["validation_total"], 4);
    assert_eq!(count(&root, "validation_history"), 4);

    let out = cli()
        .args(["learning", "validate", "--root", &root_text])
        .args(["--candidate-id", &candidate_id, "--reviewer", "reviewer-c"])
        .args(["--verdict", "maybe", "--report-ref", "report-5"])
        .output()
        .expect("cli validate failure");
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(stderr_json(&out)["error"]["code"], "INVALID_USAGE");
    assert_eq!(count(&root, "validation_history"), 4);
}

// ---------------------------------------------------------------------------
// 4. gate-target: the six-target matrix
// ---------------------------------------------------------------------------

const PLAIN_USER_1: &str = r#"{"type":"user","message":{"role":"user","content":"Always use explicit absolute roots"},"sessionId":"s-1"}"#;
const PLAIN_USER_2: &str = r#"{"type":"user","message":{"role":"user","content":"Prefer one writer per store"},"sessionId":"s-1"}"#;

struct GateFixture {
    temp: tempfile::TempDir,
    archive: Archive,
    learning_root: PathBuf,
    sha1: String,
    sha2: String,
}

impl GateFixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().expect("temp root");
        let archive = Archive::init(temp.path()).expect("archive init");
        let source = temp.path().join("projects").join("session.jsonl");
        std::fs::create_dir_all(source.parent().expect("parent")).expect("create source dir");
        std::fs::write(&source, format!("{PLAIN_USER_1}\n{PLAIN_USER_2}\n")).expect("write source");
        let report = capture_source(&archive, &source, "claude", DOMAIN, None).expect("capture");
        assert!(report.ok, "capture must succeed: {:?}", report.error);
        let learning_root = temp.path().join("learning");
        Self {
            sha1: hex_sha256(PLAIN_USER_1.as_bytes()),
            sha2: hex_sha256(PLAIN_USER_2.as_bytes()),
            temp,
            archive,
            learning_root,
        }
    }

    fn archive_root(&self) -> &Path {
        self.temp.path()
    }

    fn archived_evidence(&self, sha: &str) -> Value {
        json!({
            "ref": format!("archive-record:{sha}"),
            "span_sha256": sha,
            "archive_generation": "1",
            "observed_at": "2026-11-01T00:00:00Z",
            "actor": "owner",
            "session_kind": "interactive",
            "origin_class": "owner",
            "evidence_kind": "user_statement",
            "recall_taint": false
        })
    }

    fn gate(
        &self,
        record: &agent_memory::learning::model::CandidateRecord,
    ) -> routing::TargetReport {
        let report =
            agent_memory::learning::gate::evaluate(record, &self.archive, DOMAIN).expect("gate");
        routing::evaluate(record, Some(&report))
    }
}

#[test]
fn feedback_requires_gated_owner_evidence_and_a_dedup_record() {
    let fixture = GateFixture::new();
    let mut store = init_root(&fixture.learning_root);
    let value = candidate_with(
        "feedback",
        "Prefer explicit roots in every answer",
        vec![fixture.archived_evidence(&fixture.sha1)],
        vec![],
    );
    let id = ingest(&mut store, &value);
    record_dedup(&mut store, &id);
    let view = store.show(&id).expect("show");

    let report = fixture.gate(&view.candidate);
    assert_eq!(report.target_kind, "feedback");
    assert_eq!(report.verdict, routing::VERDICT_READY);
    assert!(report.ok);
    assert!(report.blocking.is_empty(), "{:?}", report.blocking);
    assert_eq!(report.source_gate.status, routing::SOURCE_GATE_PASSED);
    assert_eq!(requirement(&report, "owner_backed_evidence").status, "met");
    assert_eq!(requirement(&report, "dedup_recorded").status, "met");
    assert_eq!(
        requirement(&report, "contradicting_evidence_listed").status,
        "met"
    );

    // Missing dedup -> insufficient with the unmet requirement named.
    let pending = named(&value, "Prefer explicit roots in every review");
    let pending_id = ingest(&mut store, &pending);
    let view = store.show(&pending_id).expect("show pending");
    let report = fixture.gate(&view.candidate);
    assert_eq!(report.verdict, routing::VERDICT_INSUFFICIENT);
    assert!(!report.ok);
    assert!(
        blocking_has(&report, "dedup_recorded"),
        "{:?}",
        report.blocking
    );
    assert_eq!(requirement(&report, "dedup_recorded").status, "unmet");

    // Without a source gate the owner requirement is skipped and blocks.
    let report = routing::evaluate(&view.candidate, None);
    assert_eq!(report.source_gate.status, routing::SOURCE_GATE_SKIPPED);
    assert!(report.source_gate.report.is_none());
    assert_eq!(
        requirement(&report, "owner_backed_evidence").status,
        "skipped"
    );
    assert!(blocking_has(&report, "owner_backed_evidence"));
    assert_eq!(report.verdict, routing::VERDICT_INSUFFICIENT);
}

#[test]
fn user_profile_requires_every_item_to_be_owner_interactive() {
    let fixture = GateFixture::new();
    let mut store = init_root(&fixture.learning_root);
    let value = candidate_with(
        "user_profile",
        "The owner prefers explicit roots",
        vec![
            fixture.archived_evidence(&fixture.sha1),
            fixture.archived_evidence(&fixture.sha2),
        ],
        vec![],
    );
    let id = ingest(&mut store, &value);
    record_dedup(&mut store, &id);
    let view = store.show(&id).expect("show");

    let report = fixture.gate(&view.candidate);
    assert_eq!(report.verdict, routing::VERDICT_READY);
    assert_eq!(
        requirement(&report, "all_evidence_owner_interactive").status,
        "met"
    );
    assert_eq!(
        requirement(&report, "owner_statement_or_correction").status,
        "met"
    );
    assert_eq!(requirement(&report, "dedup_recorded").status, "met");

    // One non-owner item fails the structural requirement (evaluated without
    // a gate, which is enough to show the declaration is checked).
    let mixed = candidate_with(
        "user_profile",
        "The owner prefers one writer",
        vec![
            fixture.archived_evidence(&fixture.sha1),
            evidence_decl("derived", "agent_derived", "interactive", "unknown"),
        ],
        vec![],
    );
    let mixed_id = ingest(&mut store, &mixed);
    record_dedup(&mut store, &mixed_id);
    let view = store.show(&mixed_id).expect("show mixed");
    let report = routing::evaluate(&view.candidate, None);
    assert_eq!(report.verdict, routing::VERDICT_INSUFFICIENT);
    assert_eq!(
        requirement(&report, "all_evidence_owner_interactive").status,
        "unmet"
    );
    assert!(blocking_has(&report, "all_evidence_owner_interactive"));
}

#[test]
fn workflow_requires_two_distinct_passing_reuses() {
    let dir = tempfile::tempdir().expect("temp root");
    let root = dir.path().join("learning");
    let mut store = init_root(&root);

    let ready_value = candidate("workflow", "A reusable release checklist");
    let ready_id = ingest(&mut store, &ready_value);
    record_dedup(&mut store, &ready_id);
    for task in ["release-1", "release-2"] {
        let request =
            ReuseRequest::parse(&ready_id, task, "pass", None, "test:actor").expect("request");
        store.record_reuse(&request).expect("reuse");
    }
    let view = store.show(&ready_id).expect("show");
    let report = routing::evaluate(&view.candidate, None);
    assert_eq!(report.verdict, routing::VERDICT_READY);
    assert!(report.blocking.is_empty(), "{:?}", report.blocking);
    assert_eq!(requirement(&report, "reuse_distinct_tasks").status, "met");
    assert_eq!(requirement(&report, "reuse_all_passed").status, "met");
    assert_eq!(requirement(&report, "applicability_present").status, "met");
    assert_eq!(requirement(&report, "dedup_recorded").status, "met");

    // One reuse is not enough.
    let one_value = candidate("workflow", "A checklist used only once");
    let one_id = ingest(&mut store, &one_value);
    record_dedup(&mut store, &one_id);
    let request =
        ReuseRequest::parse(&one_id, "release-1", "pass", None, "test:actor").expect("request");
    store.record_reuse(&request).expect("reuse");
    let view = store.show(&one_id).expect("show");
    let report = routing::evaluate(&view.candidate, None);
    assert_eq!(report.verdict, routing::VERDICT_INSUFFICIENT);
    assert_eq!(requirement(&report, "reuse_distinct_tasks").status, "unmet");
    assert!(blocking_has(&report, "reuse_distinct_tasks"));

    // A failed outcome blocks even with two tasks.
    let failed_value = candidate("workflow", "A checklist with a failed run");
    let failed_id = ingest(&mut store, &failed_value);
    record_dedup(&mut store, &failed_id);
    let pass =
        ReuseRequest::parse(&failed_id, "release-1", "pass", None, "test:actor").expect("request");
    store.record_reuse(&pass).expect("reuse");
    let fail =
        ReuseRequest::parse(&failed_id, "release-2", "fail", None, "test:actor").expect("request");
    store.record_reuse(&fail).expect("reuse");
    let view = store.show(&failed_id).expect("show");
    let report = routing::evaluate(&view.candidate, None);
    assert_eq!(report.verdict, routing::VERDICT_INSUFFICIENT);
    assert_eq!(requirement(&report, "reuse_all_passed").status, "unmet");
    assert!(blocking_has(&report, "reuse_all_passed"));

    // A duplicated task id counts once (the store refuses to create this, so
    // the record is assembled in memory to exercise the evaluator).
    let mut duplicated = view.candidate.clone();
    let entry = json!({
        "task_id": "release-1",
        "outcome": "pass",
        "recorded_at": "2026-11-01T00:00:00Z"
    });
    duplicated.reuse = vec![entry.clone(), entry];
    let report = routing::evaluate(&duplicated, None);
    assert_eq!(report.verdict, routing::VERDICT_INSUFFICIENT);
    assert_eq!(requirement(&report, "reuse_distinct_tasks").status, "unmet");

    // Empty applicability blocks.
    let mut no_scope = view.candidate.clone();
    no_scope.reuse = vec![
        json!({"task_id": "a", "outcome": "pass", "recorded_at": "2026-11-01T00:00:00Z"}),
        json!({"task_id": "b", "outcome": "pass", "recorded_at": "2026-11-01T00:00:00Z"}),
    ];
    no_scope.applicability = Vec::new();
    let report = routing::evaluate(&no_scope, None);
    assert_eq!(
        requirement(&report, "applicability_present").status,
        "unmet"
    );
    assert!(blocking_has(&report, "applicability_present"));
}

#[test]
fn skill_adds_validation_and_manual_process_items() {
    let dir = tempfile::tempdir().expect("temp root");
    let root = dir.path().join("learning");
    let mut store = init_root(&root);
    let value = candidate("skill", "An explicit-root helper skill");
    let id = ingest(&mut store, &value);
    record_dedup(&mut store, &id);
    for task in ["skill-run-1", "skill-run-2"] {
        let request = ReuseRequest::parse(&id, task, "pass", None, "test:actor").expect("request");
        store.record_reuse(&request).expect("reuse");
    }
    let validation =
        ValidationRequest::parse(&id, "independent-reviewer", "pass", "report-1", None)
            .expect("validation");
    store.record_validation(&validation).expect("validate");
    let view = store.show(&id).expect("show");

    let report = routing::evaluate(&view.candidate, None);
    assert_eq!(report.verdict, routing::VERDICT_READY);
    assert_eq!(
        requirement(&report, "validation_present_and_passed").status,
        "met"
    );
    for manual in ["cc_switch_single_source", "host_copy_not_written"] {
        assert_eq!(requirement(&report, manual).status, "manual", "{manual}");
    }
    assert!(
        !report
            .blocking
            .iter()
            .any(|item| item == "cc_switch_single_source"),
        "manual items do not block human approval: {:?}",
        report.blocking
    );

    // A failed validation blocks.
    let failed_value = candidate("skill", "An explicit-root helper skill v2");
    let failed_id = ingest(&mut store, &failed_value);
    record_dedup(&mut store, &failed_id);
    for task in ["skill-run-1", "skill-run-2"] {
        let request =
            ReuseRequest::parse(&failed_id, task, "pass", None, "test:actor").expect("request");
        store.record_reuse(&request).expect("reuse");
    }
    let validation =
        ValidationRequest::parse(&failed_id, "independent-reviewer", "fail", "report-2", None)
            .expect("validation");
    store.record_validation(&validation).expect("validate");
    let view = store.show(&failed_id).expect("show");
    let report = routing::evaluate(&view.candidate, None);
    assert_eq!(report.verdict, routing::VERDICT_INSUFFICIENT);
    assert_eq!(
        requirement(&report, "validation_present_and_passed").status,
        "unmet"
    );
    assert!(blocking_has(&report, "validation_present_and_passed"));

    // No validation at all blocks as well.
    let no_validation_value = candidate("skill", "An explicit-root helper skill v3");
    let no_validation_id = ingest(&mut store, &no_validation_value);
    record_dedup(&mut store, &no_validation_id);
    for task in ["skill-run-1", "skill-run-2"] {
        let request = ReuseRequest::parse(&no_validation_id, task, "pass", None, "test:actor")
            .expect("request");
        store.record_reuse(&request).expect("reuse");
    }
    let view = store.show(&no_validation_id).expect("show");
    let report = routing::evaluate(&view.candidate, None);
    assert_eq!(report.verdict, routing::VERDICT_INSUFFICIENT);
    assert!(blocking_has(&report, "validation_present_and_passed"));
}

#[test]
fn note_and_rule_targets_follow_their_own_paths() {
    let fixture = GateFixture::new();
    let mut store = init_root(&fixture.learning_root);

    let note = candidate_with(
        "note",
        "A note about explicit roots",
        vec![fixture.archived_evidence(&fixture.sha1)],
        vec![],
    );
    let note_id = ingest(&mut store, &note);
    record_dedup(&mut store, &note_id);
    let view = store.show(&note_id).expect("show note");
    let report = fixture.gate(&view.candidate);
    assert_eq!(report.verdict, routing::VERDICT_READY);
    assert_eq!(requirement(&report, "evidence_present").status, "met");
    assert_eq!(requirement(&report, "dedup_recorded").status, "met");

    // A note without a dedup record is insufficient.
    let pending_note = named(&note, "A second note about explicit roots");
    let pending_id = ingest(&mut store, &pending_note);
    let view = store.show(&pending_id).expect("show pending note");
    let report = fixture.gate(&view.candidate);
    assert_eq!(report.verdict, routing::VERDICT_INSUFFICIENT);
    assert!(blocking_has(&report, "dedup_recorded"));

    // A note whose evidence cannot pass the source gate is blocked by it.
    let forged = candidate_with(
        "note",
        "A note with forgotten evidence",
        vec![evidence("forged-note")],
        vec![],
    );
    let forged_id = ingest(&mut store, &forged);
    record_dedup(&mut store, &forged_id);
    let view = store.show(&forged_id).expect("show forged note");
    let report = fixture.gate(&view.candidate);
    assert_eq!(report.verdict, routing::VERDICT_INSUFFICIENT);
    assert_eq!(report.source_gate.status, routing::SOURCE_GATE_FAILED);
    assert!(blocking_has(&report, routing::BLOCKED_BY_SOURCE_GATE));
    assert_eq!(requirement(&report, "source_gate").status, "unmet");

    // `rule` never has a mechanical ready path.
    let rule = candidate("rule", "Always use explicit roots");
    let rule_id = ingest(&mut store, &rule);
    record_dedup(&mut store, &rule_id);
    let view = store.show(&rule_id).expect("show rule");
    let report = routing::evaluate(&view.candidate, None);
    assert_eq!(report.target_kind, "rule");
    assert_eq!(report.verdict, routing::VERDICT_PROPOSAL_ONLY);
    assert!(!report.ok);
    assert_eq!(
        requirement(&report, "mechanical_ready_path").status,
        "unmet"
    );
    assert!(blocking_has(&report, routing::BLOCKED_RULE_GOVERNANCE));
    assert!(
        requirement(&report, "mechanical_ready_path")
            .detail
            .contains("绝不直接部署"),
        "the rule report must state the boundary: {:?}",
        requirement(&report, "mechanical_ready_path").detail
    );

    // Even a rule with every optional record is still proposal-only.
    let mut loaded = view.candidate.clone();
    loaded.applicability = vec!["AGENTS.md".to_string()];
    loaded.reuse = vec![
        json!({"task_id": "a", "outcome": "pass", "recorded_at": "2026-11-01T00:00:00Z"}),
        json!({"task_id": "b", "outcome": "pass", "recorded_at": "2026-11-01T00:00:00Z"}),
    ];
    let report = routing::evaluate(&loaded, None);
    assert_eq!(report.verdict, routing::VERDICT_PROPOSAL_ONLY);
}

// ---------------------------------------------------------------------------
// 5. gate-target together with the S2 gate (and the CLI surface)
// ---------------------------------------------------------------------------

#[test]
fn gate_target_degrades_on_a_failed_source_gate_and_skips_without_one() {
    let fixture = GateFixture::new();
    let mut store = init_root(&fixture.learning_root);
    let good = candidate_with(
        "feedback",
        "Explicit roots are the owner preference",
        vec![fixture.archived_evidence(&fixture.sha1)],
        vec![],
    );
    let good_id = ingest(&mut store, &good);
    record_dedup(&mut store, &good_id);
    let bad = candidate_with(
        "feedback",
        "A forged feedback claim",
        vec![evidence("forged")],
        vec![],
    );
    let bad_id = ingest(&mut store, &bad);
    record_dedup(&mut store, &bad_id);

    let root_text = fixture.learning_root.to_string_lossy().to_string();
    let archive_text = fixture.archive_root().to_string_lossy().to_string();

    // Passing gate.
    let out = cli()
        .args(["learning", "gate-target", "--root", &root_text])
        .args(["--candidate-id", &good_id])
        .args(["--archive", &archive_text, "--domain", DOMAIN])
        .output()
        .expect("cli gate-target good");
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let report = stdout_json(&out);
    assert_eq!(report["verdict"], "ready_for_approval");
    assert_eq!(report["ok"], true);
    assert_eq!(report["source_gate"]["status"], "passed");
    assert_eq!(report["source_gate"]["report"]["ok"], true);

    // Failing gate degrades to insufficient and names the blocking code.
    let out = cli()
        .args(["learning", "gate-target", "--root", &root_text])
        .args(["--candidate-id", &bad_id])
        .args(["--archive", &archive_text, "--domain", DOMAIN])
        .output()
        .expect("cli gate-target bad");
    assert_eq!(out.status.code(), Some(0));
    let report = stdout_json(&out);
    assert_eq!(report["verdict"], "insufficient_evidence");
    assert_eq!(report["ok"], false);
    assert_eq!(report["source_gate"]["status"], "failed");
    assert_eq!(report["source_gate"]["report"]["ok"], false);
    assert!(
        report["blocking"]
            .as_array()
            .expect("blocking")
            .iter()
            .any(|item| item == "blocked_by_source_gate"),
        "{report}"
    );

    // Without an archive the source gate is skipped (still exit 0).
    let out = cli()
        .args(["learning", "gate-target", "--root", &root_text])
        .args(["--candidate-id", &good_id])
        .output()
        .expect("cli gate-target without archive");
    assert_eq!(out.status.code(), Some(0));
    let report = stdout_json(&out);
    assert_eq!(report["source_gate"]["status"], "skipped");
    assert!(
        report["source_gate"].get("report").is_none() || report["source_gate"]["report"].is_null()
    );
    assert_eq!(report["verdict"], "insufficient_evidence");

    // One of the pair is a usage error.
    let out = cli()
        .args(["learning", "gate-target", "--root", &root_text])
        .args(["--candidate-id", &good_id, "--domain", DOMAIN])
        .output()
        .expect("cli gate-target one flag");
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(stderr_json(&out)["error"]["code"], "INVALID_USAGE");
}

// ---------------------------------------------------------------------------
// 6. export packages
// ---------------------------------------------------------------------------

#[test]
fn export_writes_the_review_package_to_stdout_or_an_explicit_file() {
    let fixture = GateFixture::new();
    let mut store = init_root(&fixture.learning_root);
    let value = candidate_with(
        "feedback",
        "The owner wants explicit roots everywhere",
        vec![fixture.archived_evidence(&fixture.sha1)],
        vec![],
    );
    let id = ingest(&mut store, &value);
    record_dedup(&mut store, &id);
    let rule = candidate("rule", "A rule that must never auto-deploy");
    let rule_id = ingest(&mut store, &rule);
    let before_counts = all_counts(&fixture.learning_root);
    drop(store);

    let root_text = fixture.learning_root.to_string_lossy().to_string();
    let archive_text = fixture.archive_root().to_string_lossy().to_string();

    // stdout is the default destination.
    let out = cli()
        .args(["learning", "export", "--root", &root_text])
        .args(["--candidate-id", &id])
        .args(["--archive", &archive_text, "--domain", DOMAIN])
        .output()
        .expect("cli export stdout");
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let package = stdout_json(&out);
    assert_eq!(package["decision_required"], "human approval");
    assert!(
        package.get("promotion_path").is_none(),
        "only a rule carries a promotion_path: {package}"
    );
    assert_eq!(package["candidate_id"], id.as_str());
    assert_eq!(package["candidate"]["candidate_id"], id.as_str());
    assert_eq!(package["candidate"]["claim"], value["claim"]);
    assert_eq!(package["candidate"]["revision"], 1);
    assert_eq!(package["candidate"]["dedup"]["disposition"], "new");
    assert_eq!(package["dedup"]["disposition"], "new");
    assert_eq!(
        package["ingestions"].as_array().expect("ingestions").len(),
        1
    );
    assert_eq!(package["revisions"].as_array().expect("revisions").len(), 1);
    assert_eq!(
        package["status_history"]
            .as_array()
            .expect("status_history")
            .len(),
        1
    );
    assert!(package["reuse"].as_array().expect("reuse").is_empty());
    assert!(package["validation"]
        .as_array()
        .expect("validation")
        .is_empty());
    assert_eq!(package["target_report"]["verdict"], "ready_for_approval");
    assert_eq!(package["target_report"]["target_kind"], "feedback");
    assert!(looks_rfc3339(
        package["exported_at"].as_str().expect("exported_at")
    ));
    assert_eq!(
        package["tool_version"].as_str(),
        Some(env!("CARGO_PKG_VERSION"))
    );

    // A rule package always carries the proposal-only promotion path.
    let out = cli()
        .args(["learning", "export", "--root", &root_text])
        .args(["--candidate-id", &rule_id])
        .output()
        .expect("cli export rule");
    assert_eq!(out.status.code(), Some(0));
    let package = stdout_json(&out);
    assert_eq!(package["decision_required"], "human approval");
    assert_eq!(package["promotion_path"], "none — proposal only");
    assert_eq!(package["target_report"]["verdict"], "proposal_only");

    // `--out` writes the package to an explicit absolute file.
    let out_path = fixture.temp.path().join("review-package.json");
    let out = cli()
        .args(["learning", "export", "--root", &root_text])
        .args(["--candidate-id", &id])
        .args(["--archive", &archive_text, "--domain", DOMAIN])
        .arg("--out")
        .arg(&out_path)
        .output()
        .expect("cli export file");
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.stdout.is_empty(),
        "with --out the package must not be duplicated on stdout: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    let written: Value =
        serde_json::from_slice(&std::fs::read(&out_path).expect("read package")).expect("json");
    assert_eq!(written["decision_required"], "human approval");
    assert_eq!(written["candidate_id"], id.as_str());
    assert_eq!(written["target_report"]["verdict"], "ready_for_approval");

    // A relative --out is a usage error.
    let out = cli()
        .args(["learning", "export", "--root", &root_text])
        .args(["--candidate-id", &id, "--out", "relative.json"])
        .output()
        .expect("cli export relative out");
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(stderr_json(&out)["error"]["code"], "INVALID_USAGE");

    // Export is read-only: every table count is unchanged.
    assert_eq!(all_counts(&fixture.learning_root), before_counts);
}

// ---------------------------------------------------------------------------
// 7. read-only evaluation and supersession preservation
// ---------------------------------------------------------------------------

#[test]
fn gate_target_and_export_never_rewrite_revisions_status_or_counters() {
    let fixture = GateFixture::new();
    let mut store = init_root(&fixture.learning_root);
    let value = candidate_with(
        "feedback",
        "Contradictions are listed but never required to be empty",
        vec![fixture.archived_evidence(&fixture.sha1)],
        vec![fixture.archived_evidence(&fixture.sha2)],
    );
    let id = ingest(&mut store, &value);
    record_dedup(&mut store, &id);

    // Publish a second revision so supersession history is non-trivial.
    let mut second = value.clone();
    second["revision"] = json!(2);
    second["source_checkpoint"] = json!("checkpoint-b");
    second["updated_at"] = json!("2026-11-01T02:00:00Z");
    second["evidence"] = json!([
        fixture.archived_evidence(&fixture.sha1),
        fixture.archived_evidence(&fixture.sha2)
    ]);
    store.ingest(&second).expect("second revision");
    let before = store.show(&id).expect("show before");
    assert_eq!(before.candidate.revision, 2);
    assert_eq!(before.revisions.len(), 2);
    assert_eq!(before.status_history.len(), 2);
    assert_eq!(before.candidate.contradicting_evidence.len(), 1);
    drop(store);

    let before_counts = all_counts(&fixture.learning_root);
    let before_mtime = std::fs::metadata(db_of(&fixture.learning_root))
        .expect("db metadata")
        .modified()
        .expect("mtime");
    let root_text = fixture.learning_root.to_string_lossy().to_string();
    let archive_text = fixture.archive_root().to_string_lossy().to_string();

    let out = cli()
        .args(["learning", "gate-target", "--root", &root_text])
        .args(["--candidate-id", &id])
        .args(["--archive", &archive_text, "--domain", DOMAIN])
        .output()
        .expect("cli gate-target");
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let report = stdout_json(&out);
    // Contradicting evidence is listed explicitly and is not a failure.
    assert_eq!(
        report["requirements"]
            .as_array()
            .expect("requirements")
            .iter()
            .find(|item| item["requirement"] == "contradicting_evidence_listed")
            .expect("contradicting requirement")["status"],
        "met"
    );
    assert_ne!(report["verdict"], "proposal_only");

    let out = cli()
        .args(["learning", "export", "--root", &root_text])
        .args(["--candidate-id", &id])
        .args(["--archive", &archive_text, "--domain", DOMAIN])
        .output()
        .expect("cli export");
    assert_eq!(out.status.code(), Some(0));

    let store = LearningStore::open(&fixture.learning_root).expect("re-open");
    let after = store.show(&id).expect("show after");
    assert_eq!(after.payload_sha256, before.payload_sha256);
    assert_eq!(after.candidate.revision, 2);
    assert_eq!(after.revisions.len(), before.revisions.len());
    assert_eq!(after.status_history.len(), before.status_history.len());
    assert_eq!(after.candidate.contradicting_evidence.len(), 1);
    assert_eq!(all_counts(&fixture.learning_root), before_counts);
    drop(store);
    let after_mtime = std::fs::metadata(db_of(&fixture.learning_root))
        .expect("db metadata")
        .modified()
        .expect("mtime");
    assert_eq!(before_mtime, after_mtime, "learning.db mtime must not move");
}

// ---------------------------------------------------------------------------
// 8. ingest input still rejects reuse/validation; the events are CLI-only
// ---------------------------------------------------------------------------

#[test]
fn ingest_still_rejects_non_empty_reuse_and_validation_arrays() {
    let dir = tempfile::tempdir().expect("temp root");
    let root = dir.path().join("learning");
    let root_text = root.to_string_lossy().to_string();
    let mut store = init_root(&root);

    let mut with_reuse = candidate("workflow", "A workflow drafted directly");
    with_reuse["reuse"] = json!([{
        "task_id": "task-1",
        "outcome": "pass",
        "recorded_at": "2026-11-01T00:00:00Z"
    }]);
    let error = validate_candidate(&with_reuse).expect_err("reuse must stay empty on input");
    assert_eq!(error.code, "INVALID_CANDIDATE");
    assert_eq!(error.exit_code(), 2);
    let error = store.ingest(&with_reuse).expect_err("ingest refuses reuse");
    assert_eq!(error.code, "INVALID_CANDIDATE");

    let mut with_validation = candidate("skill", "A skill drafted directly");
    with_validation["validation"] = json!([{
        "reviewer": "reviewer-a",
        "verdict": "pass",
        "report_ref": "report-1",
        "recorded_at": "2026-11-01T00:00:00Z"
    }]);
    let error = store
        .ingest(&with_validation)
        .expect_err("ingest refuses validation");
    assert_eq!(error.code, "INVALID_CANDIDATE");
    assert_eq!(error.exit_code(), 2);
    assert_eq!(all_counts(&root), [0, 0, 0, 0, 0, 0, 0]);
    drop(store);

    // The CLI surface keeps the same rejection.
    for field in ["reuse", "validation"] {
        let mut value = candidate("skill", "A CLI draft with an event");
        let entry = if field == "reuse" {
            json!([{"task_id": "t", "outcome": "pass", "recorded_at": "2026-11-01T00:00:00Z"}])
        } else {
            json!([{"reviewer": "r", "verdict": "pass", "report_ref": "x",
                    "recorded_at": "2026-11-01T00:00:00Z"}])
        };
        value[field] = entry;
        let out = cli()
            .args(["learning", "ingest", "--root", &root_text])
            .args(["--candidate", &serde_json::to_string(&value).expect("json")])
            .output()
            .expect("cli ingest");
        assert_eq!(out.status.code(), Some(2), "{field}");
        assert_eq!(stderr_json(&out)["error"]["code"], "INVALID_CANDIDATE");
    }
    assert_eq!(all_counts(&root), [0, 0, 0, 0, 0, 0, 0]);
}
