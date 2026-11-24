//! Stage 3 S3: dedup dispositions, schema v1 -> v2 migration, supersession
//! ordering, the gate as an ingest precondition, and the read-only connection
//! to an existing candidate list.
//!
//! Every case uses synthetic temporary archive/learning roots and synthetic
//! Claude JSONL. Nothing here reads a real session root, a real archive, a real
//! learning root, `~/.memory` or a candidate list outside the fixture.

use std::path::{Path, PathBuf};
use std::process::Command;

use rusqlite::Connection;
use serde_json::{json, Value};

use agent_memory::archive::{hex_sha256, Archive};
use agent_memory::capture::capture_source;
use agent_memory::learning::dedup::{DedupRequest, DISPOSITIONS};
use agent_memory::learning::model::validate_candidate;
use agent_memory::learning::{LearningStore, DB_FILE, DB_SCHEMA_VERSION};

const DOMAIN: &str = "default";

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// The S1 (v1) schema, written by hand so the migration has a real v1 database
/// to open. It is the exact shape S1 shipped.
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

fn span_sha256(tag: &str) -> String {
    let mut acc: u64 = 14_695_981_039_346_656_037;
    for byte in tag.as_bytes() {
        acc ^= u64::from(*byte);
        acc = acc.wrapping_mul(1_099_511_628_211);
    }
    format!("{acc:064x}")
}

fn evidence(tag: &str) -> Value {
    json!({
        "ref": format!("claude:session-{tag}:line-1"),
        "span_sha256": span_sha256(tag),
        "archive_generation": "g1",
        "observed_at": "2026-11-01T00:00:00Z",
        "actor": "user",
        "session_kind": "interactive",
        "origin_class": "owner",
        "evidence_kind": "user_statement",
        "recall_taint": false
    })
}

fn candidate() -> Value {
    json!({
        "schema_version": 1,
        "candidate_id": "",
        "revision": 1,
        "status": "candidate",
        "claim": "Prefer explicit roots for every store",
        "target_kind": "rule",
        "proposed_target": "AGENTS.md#explicit-roots",
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

fn named(claim: &str) -> Value {
    let mut value = candidate();
    value["claim"] = json!(claim);
    value
}

fn report(counter: usize) -> Value {
    json!({
        "query": format!("explicit roots {counter}"),
        "results": [
            {"rank": 1, "score": 0.91, "text": "prefer explicit roots", "extra": {"keep": [1, 2, 3]}},
            {"rank": 2, "text": "the store takes an absolute path"}
        ],
        "scope": "memory",
        "checked": counter
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

fn all_counts(root: &Path) -> [i64; 5] {
    [
        count(root, "candidates"),
        count(root, "candidate_revisions"),
        count(root, "ingestions"),
        count(root, "status_history"),
        count(root, "dedup_history"),
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
struct HistoryRow {
    seq: i64,
    candidate_id: String,
    query: String,
    matches_json: String,
    disposition: String,
    reason: String,
    checked_at: String,
}

fn history(root: &Path, candidate_id: &str) -> Vec<HistoryRow> {
    let conn = Connection::open(db_of(root)).expect("open learning.db");
    let mut stmt = conn
        .prepare(
            "SELECT seq, candidate_id, query, matches_json, disposition, reason, checked_at \
             FROM dedup_history WHERE candidate_id = ?1 ORDER BY seq",
        )
        .expect("prepare history");
    stmt.query_map([candidate_id], |row| {
        Ok(HistoryRow {
            seq: row.get(0)?,
            candidate_id: row.get(1)?,
            query: row.get(2)?,
            matches_json: row.get(3)?,
            disposition: row.get(4)?,
            reason: row.get(5)?,
            checked_at: row.get(6)?,
        })
    })
    .expect("query history")
    .collect::<Result<Vec<_>, _>>()
    .expect("collect history")
}

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

/// Minimal RFC 3339 shape check (the crate's `chrono` is not a dev-dependency).
fn looks_rfc3339(value: &str) -> bool {
    let bytes = value.as_bytes();
    value.len() >= 20
        && bytes.get(4) == Some(&b'-')
        && bytes.get(7) == Some(&b'-')
        && bytes.get(10) == Some(&b'T')
        && bytes.get(13) == Some(&b':')
}

/// A v1 database written directly, holding one committed candidate.
fn write_v1_database(root: &Path) -> (String, String) {
    std::fs::create_dir_all(root).expect("v1 root");
    let db = db_of(root);
    let conn = Connection::open(&db).expect("open v1 db");
    conn.execute_batch(V1_SCHEMA).expect("v1 schema");
    conn.pragma_update(None, "user_version", 1)
        .expect("user_version 1");

    let value = candidate();
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
    .expect("insert v1 candidate");
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
    .expect("insert v1 revision");
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
    .expect("insert v1 ingestion");
    conn.execute(
        "INSERT INTO status_history (candidate_id, revision, status, recorded_at) \
         VALUES (?1, ?2, 'candidate', '2026-11-01T00:00:01.000Z')",
        rusqlite::params![record.candidate_id, record.revision as i64],
    )
    .expect("insert v1 status");
    (
        record.candidate_id.clone(),
        validated.payload_sha256.clone(),
    )
}

// ---------------------------------------------------------------------------
// 1. Schema v1 -> v2 migration, v2 init, and fail-closed foreign/future DBs
// ---------------------------------------------------------------------------

#[test]
fn v1_database_is_migrated_to_v2_on_open_and_stays_reentrant() {
    let dir = tempfile::tempdir().expect("temp root");
    let root = dir.path().join("learning");
    let (candidate_id, payload_sha256) = write_v1_database(&root);

    assert_eq!(user_version(&root), 1);
    assert!(!table_names(&root).contains(&"dedup_history".to_string()));

    let mut store = LearningStore::open(&root).expect("open migrates v1 to v2");
    assert_eq!(user_version(&root), i64::from(DB_SCHEMA_VERSION));
    assert!(table_names(&root).contains(&"dedup_history".to_string()));

    // The v1 data is untouched and still readable through the normal path.
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

    // The migrated store accepts a dedup record (the v2 table really exists).
    let request = DedupRequest::parse(&candidate_id, &report(1), "duplicate", Some("same rule"))
        .expect("req");
    let outcome = store.record_dedup(&request).expect("dedup after migration");
    assert_eq!(outcome.disposition, "duplicate");
    assert_eq!(count(&root, "dedup_history"), 1);
    drop(store);

    // Re-open is a no-op migration: same version, same rows, no new history.
    let store = LearningStore::open(&root).expect("re-open migrated store");
    assert_eq!(user_version(&root), i64::from(DB_SCHEMA_VERSION));
    assert_eq!(count(&root, "dedup_history"), 1);
    assert_eq!(count(&root, "candidates"), 1);
    drop(store);

    // `init` creates v2 directly.
    let fresh = dir.path().join("fresh");
    let fresh_store = LearningStore::init(&fresh).expect("init fresh store");
    assert_eq!(user_version(&fresh), i64::from(DB_SCHEMA_VERSION));
    assert!(table_names(&fresh).contains(&"dedup_history".to_string()));
    assert_eq!(fresh_store.db_path(), db_of(&fresh));
}

#[test]
fn foreign_partial_and_future_schemas_stay_fail_closed_without_migration() {
    let dir = tempfile::tempdir().expect("temp root");

    // A foreign v1 database is rejected as-is: no table is added, no version
    // bump happens (the v1 probes run before the migration).
    let foreign = dir.path().join("foreign");
    std::fs::create_dir_all(&foreign).expect("foreign root");
    {
        let conn = Connection::open(db_of(&foreign)).expect("foreign db");
        conn.execute_batch(
            "CREATE TABLE archive_objects (sha256 TEXT PRIMARY KEY, bytes INTEGER);",
        )
        .expect("foreign schema");
        conn.pragma_update(None, "user_version", 1)
            .expect("user_version 1");
    }
    let error = LearningStore::open(&foreign).expect_err("foreign schema");
    assert_eq!(error.code, "LEARNING_SCHEMA_INVALID");
    assert_eq!(error.exit_code(), 6);
    assert_eq!(user_version(&foreign), 1);
    assert!(!table_names(&foreign).contains(&"dedup_history".to_string()));

    // A partial v1 database is likewise rejected without migration.
    let partial = dir.path().join("partial");
    std::fs::create_dir_all(&partial).expect("partial root");
    {
        let conn = Connection::open(db_of(&partial)).expect("partial db");
        conn.execute_batch("CREATE TABLE candidates (candidate_id TEXT PRIMARY KEY);")
            .expect("partial schema");
        conn.pragma_update(None, "user_version", 1)
            .expect("user_version 1");
    }
    let error = LearningStore::open(&partial).expect_err("partial schema");
    assert_eq!(error.code, "LEARNING_SCHEMA_INVALID");
    assert_eq!(user_version(&partial), 1);
    assert!(!table_names(&partial).contains(&"dedup_history".to_string()));

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

    // A corrupt file still fails closed.
    let corrupt = dir.path().join("corrupt");
    std::fs::create_dir_all(&corrupt).expect("corrupt root");
    std::fs::write(db_of(&corrupt), b"this is not a sqlite database").expect("corrupt db");
    let error = LearningStore::open(&corrupt).expect_err("corrupt db");
    assert_eq!(error.code, "LEARNING_SCHEMA_INVALID");
}

// ---------------------------------------------------------------------------
// 2. Every disposition is recorded: candidate dedup, history, checked_at
// ---------------------------------------------------------------------------

#[test]
fn every_disposition_updates_the_record_and_appends_history() {
    let dir = tempfile::tempdir().expect("temp root");
    let root = dir.path().join("learning");
    let mut store = init_root(&root);

    for (index, disposition) in DISPOSITIONS.iter().enumerate() {
        let mut value = named(&format!("Disposition {disposition}"));
        value["source_checkpoint"] = json!(format!("checkpoint-{index}"));
        let ingested = store.ingest(&value).expect("ingest candidate");

        let report = report(index + 1);
        let request = DedupRequest::parse(
            &ingested.candidate_id,
            &report,
            disposition,
            Some("  the hit is the same rule  "),
        )
        .expect("well-formed dedup request");
        let outcome = store
            .record_dedup(&request)
            .unwrap_or_else(|error| panic!("{disposition}: {error}"));
        assert!(outcome.ok);
        assert_eq!(outcome.disposition, *disposition);
        assert_eq!(outcome.matches, 2);
        assert_eq!(outcome.revision, 1);
        assert_eq!(outcome.status, "candidate");
        assert!(looks_rfc3339(&outcome.checked_at));

        let view = store.show(&ingested.candidate_id).expect("show");
        assert_eq!(view.candidate.dedup.disposition, *disposition);
        assert_eq!(
            view.candidate.dedup.query,
            report["query"].as_str().expect("query")
        );
        assert_eq!(
            json!(view.candidate.dedup.matches),
            report["results"],
            "matches must be stored verbatim"
        );
        assert_eq!(
            view.candidate.dedup.checked_at.as_deref(),
            Some(outcome.checked_at.as_str())
        );
        assert_eq!(
            view.payload_sha256, outcome.payload_sha256,
            "the stored payload hash is refreshed with the record"
        );
        assert_ne!(
            view.payload_sha256, ingested.payload_sha256,
            "recording a dedup result changes the payload"
        );
        // Workflow state only: nothing else moved.
        assert_eq!(view.candidate.revision, 1);
        assert_eq!(view.candidate.evidence.len(), 1);
        assert_eq!(view.candidate.contradicting_evidence.len(), 0);
        assert_eq!(view.revisions.len(), 1);
        assert_eq!(view.revisions[0].payload_sha256, ingested.payload_sha256);
        assert_eq!(view.status_history.len(), 1);
        assert_eq!(view.ingestions.len(), 1);

        let rows = history(&root, &ingested.candidate_id);
        assert_eq!(rows.len(), 1, "{disposition}");
        assert_eq!(rows[0].candidate_id, ingested.candidate_id);
        assert_eq!(rows[0].query, report["query"].as_str().expect("query"));
        assert_eq!(
            rows[0].matches_json,
            serde_json::to_string(&report["results"]).expect("serialize results")
        );
        assert_eq!(rows[0].disposition, *disposition);
        assert_eq!(rows[0].reason, "the hit is the same rule");
        assert_eq!(rows[0].checked_at, outcome.checked_at);
        assert!(rows[0].seq >= 1);
    }

    assert_eq!(count(&root, "candidates"), DISPOSITIONS.len() as i64);
    assert_eq!(count(&root, "dedup_history"), DISPOSITIONS.len() as i64);
}

// ---------------------------------------------------------------------------
// 3. History is append-only; the CLI and the library reject bad input
// ---------------------------------------------------------------------------

#[test]
fn repeated_dedup_appends_history_and_invalid_input_is_rejected() {
    let dir = tempfile::tempdir().expect("temp root");
    let root = dir.path().join("learning");
    let root_text = root.to_string_lossy().to_string();
    let mut store = init_root(&root);
    let ingested = store.ingest(&candidate()).expect("ingest base candidate");
    let candidate_id = ingested.candidate_id.clone();

    let first = DedupRequest::parse(&candidate_id, &report(1), "related", Some("first look"))
        .expect("first request");
    let first_outcome = store.record_dedup(&first).expect("first dedup");
    let second = DedupRequest::parse(&candidate_id, &report(2), "new", Some("second look"))
        .expect("second request");
    let second_outcome = store.record_dedup(&second).expect("second dedup");
    assert!(second_outcome.history_seq > first_outcome.history_seq);

    let rows = history(&root, &candidate_id);
    assert_eq!(rows.len(), 2, "a repeat appends, it never overwrites");
    assert_eq!(rows[0].query, "explicit roots 1");
    assert_eq!(rows[0].disposition, "related");
    assert_eq!(rows[0].reason, "first look");
    assert_eq!(rows[0].checked_at, first_outcome.checked_at);
    assert_eq!(rows[1].query, "explicit roots 2");
    assert_eq!(rows[1].disposition, "new");
    assert_eq!(rows[1].reason, "second look");

    let view = store.show(&candidate_id).expect("show");
    assert_eq!(
        view.candidate.dedup.disposition, "new",
        "the record carries the latest result"
    );
    assert_eq!(view.status_history.len(), 1);

    // Library-level refusals.
    let error = DedupRequest::parse(&candidate_id, &report(3), "duplicate", None)
        .expect_err("a missing reason is a usage error");
    assert_eq!(error.code, "INVALID_USAGE");
    assert_eq!(error.exit_code(), 2);
    let error = DedupRequest::parse(&candidate_id, &report(3), "duplicate", Some("   \t "))
        .expect_err("a blank reason is a usage error");
    assert_eq!(error.code, "INVALID_USAGE");
    for bad in [
        json!({"results": []}),
        json!({"query": "", "results": []}),
        json!({"query": "   ", "results": []}),
        json!({"query": "q"}),
        json!({"query": "q", "results": {"not": "an array"}}),
        json!([]),
    ] {
        let error = DedupRequest::parse(&candidate_id, &bad, "duplicate", Some("why"))
            .expect_err("a malformed report is rejected");
        assert_eq!(error.code, "DEDUP_REPORT_INVALID", "report: {bad}");
        assert_eq!(error.exit_code(), 2);
    }
    let missing = DedupRequest::parse("cand_missing", &report(4), "new", Some("why"))
        .expect("request for a missing candidate validates");
    let error = store
        .record_dedup(&missing)
        .expect_err("unknown candidate is not found");
    assert_eq!(error.code, "CANDIDATE_NOT_FOUND");
    assert_eq!(error.exit_code(), 6);
    assert_eq!(
        count(&root, "dedup_history"),
        2,
        "a refused dedup appends nothing"
    );
    drop(store);

    // CLI success path with `--report @file`.
    let report_path = dir.path().join("dedup-report.json");
    std::fs::write(
        &report_path,
        serde_json::to_vec_pretty(&report(5)).expect("serialize report"),
    )
    .expect("write report");
    let out = cli()
        .args([
            "learning",
            "dedup",
            "--root",
            &root_text,
            "--candidate-id",
            &candidate_id,
            "--report",
            &format!("@{}", report_path.display()),
            "--disposition",
            "contradicts",
            "--reason",
            "the owner said otherwise",
        ])
        .output()
        .expect("cli dedup");
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let json = stdout_json(&out);
    assert_eq!(json["ok"], true);
    assert_eq!(json["candidate_id"], candidate_id.as_str());
    assert_eq!(json["disposition"], "contradicts");
    assert_eq!(json["matches"], 2);
    assert!(looks_rfc3339(
        json["checked_at"].as_str().expect("checked_at")
    ));
    assert_eq!(count(&root, "dedup_history"), 3);

    // CLI refusals: blank reason, bad reports, unknown id, invalid disposition.
    let valid_report = serde_json::to_string(&report(6)).expect("report json");
    let cases: Vec<(&str, &str, &str, &str, i32, &str)> = vec![
        (
            candidate_id.as_str(),
            valid_report.as_str(),
            "new",
            "   ",
            2,
            "INVALID_USAGE",
        ),
        (
            candidate_id.as_str(),
            "{\"results\":[]}",
            "new",
            "why",
            2,
            "DEDUP_REPORT_INVALID",
        ),
        (
            candidate_id.as_str(),
            "{\"query\":\"q\"}",
            "new",
            "why",
            2,
            "DEDUP_REPORT_INVALID",
        ),
        (
            candidate_id.as_str(),
            "{not json",
            "new",
            "why",
            2,
            "DEDUP_REPORT_INVALID",
        ),
        (
            candidate_id.as_str(),
            valid_report.as_str(),
            "pending",
            "why",
            2,
            "INVALID_USAGE",
        ),
        (
            "cand_missing",
            valid_report.as_str(),
            "new",
            "why",
            6,
            "CANDIDATE_NOT_FOUND",
        ),
    ];
    for (id, report_text, disposition, reason, expected_code, expected_error) in cases {
        let args = vec![
            "learning".to_string(),
            "dedup".into(),
            "--root".into(),
            root_text.clone(),
            "--candidate-id".into(),
            id.to_string(),
            "--report".into(),
            report_text.to_string(),
            "--disposition".into(),
            disposition.to_string(),
            "--reason".into(),
            reason.to_string(),
        ];
        let out = cli().args(&args).output().expect("cli dedup failure");
        assert_eq!(out.status.code(), Some(expected_code), "args: {args:?}");
        assert_eq!(
            stderr_json(&out)["error"]["code"],
            expected_error,
            "args: {args:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// 4. `contradicts` records the disposition only
// ---------------------------------------------------------------------------

#[test]
fn contradicts_never_writes_counter_evidence_or_changes_status() {
    let dir = tempfile::tempdir().expect("temp root");
    let root = dir.path().join("learning");
    let mut store = init_root(&root);
    let ingested = store.ingest(&candidate()).expect("ingest");

    let request = DedupRequest::parse(
        &ingested.candidate_id,
        &report(1),
        "contradicts",
        Some("a later session disagrees"),
    )
    .expect("request");
    let outcome = store.record_dedup(&request).expect("dedup");
    assert_eq!(outcome.disposition, "contradicts");

    let view = store.show(&ingested.candidate_id).expect("show");
    assert!(
        view.candidate.contradicting_evidence.is_empty(),
        "dedup never mints counter-evidence"
    );
    assert_eq!(view.candidate.evidence.len(), 1);
    assert_eq!(view.candidate.status, "candidate");
    assert_eq!(view.status_history.len(), 1);
    assert_eq!(view.status_history[0].status, "candidate");
    assert_eq!(view.revisions.len(), 1);
    assert_eq!(view.candidate.dedup.disposition, "contradicts");

    let rows = history(&root, &ingested.candidate_id);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].disposition, "contradicts");
    assert_eq!(rows[0].reason, "a later session disagrees");
}

// ---------------------------------------------------------------------------
// 5. A dedup result does not block same-revision appends or original replays
// ---------------------------------------------------------------------------

#[test]
fn dedup_keeps_same_revision_appends_and_original_payload_replays() {
    let dir = tempfile::tempdir().expect("temp root");
    let root = dir.path().join("learning");
    let mut store = init_root(&root);
    let first = store.ingest(&candidate()).expect("ingest base");

    let request = DedupRequest::parse(
        &first.candidate_id,
        &report(1),
        "duplicate",
        Some("already known as a rule"),
    )
    .expect("request");
    let dedup = store.record_dedup(&request).expect("dedup");
    let after_dedup = store
        .show(&first.candidate_id)
        .expect("show")
        .payload_sha256;
    assert_eq!(after_dedup, dedup.payload_sha256);
    assert_ne!(after_dedup, first.payload_sha256);

    // The same revision still accepts a deduplicated evidence append from a
    // different batch, whose own payload carries dedup = pending.
    let mut appended = candidate();
    appended["source_checkpoint"] = json!("checkpoint-b");
    appended["extractor"] = json!("extractor-b");
    appended["updated_at"] = json!("2026-11-01T01:00:00Z");
    appended["evidence"] = json!([evidence("e1"), evidence("e2")]);
    let second = store.ingest(&appended).expect("append after dedup");
    assert!(!second.replayed);
    assert_eq!(second.evidence_added, 1);

    let view = store.show(&first.candidate_id).expect("show");
    assert_eq!(view.candidate.revision, 1);
    assert_eq!(view.candidate.evidence.len(), 2);
    assert_eq!(
        view.candidate.dedup.disposition, "duplicate",
        "an append never resets the recorded dedup result"
    );
    assert_eq!(
        view.candidate.dedup.checked_at.as_deref(),
        Some(dedup.checked_at.as_str())
    );
    assert_eq!(view.ingestions.len(), 2);

    // Replaying the original payload is still a replay of the ingestion row:
    // no new ingestion, no evidence, and no rollback of the dedup result.
    let replay = store.ingest(&candidate()).expect("original payload replay");
    assert!(replay.replayed);
    assert_eq!(replay.evidence_added, 0);
    assert_eq!(replay.revision, 1);
    assert_eq!(
        replay.payload_sha256, first.payload_sha256,
        "the replay is judged against the ingestion's recorded payload hash"
    );
    assert_eq!(count(&root, "ingestions"), 2);
    assert_eq!(count(&root, "candidates"), 1);
    assert_eq!(count(&root, "dedup_history"), 1);
    let view = store.show(&first.candidate_id).expect("show after replay");
    assert_eq!(view.candidate.evidence.len(), 2);
    assert_eq!(view.candidate.dedup.disposition, "duplicate");
}

// ---------------------------------------------------------------------------
// 6. Supersession: revision + 1 must not go back in time
// ---------------------------------------------------------------------------

#[test]
fn revision_supersession_requires_a_non_decreasing_updated_at() {
    let dir = tempfile::tempdir().expect("temp root");
    let root = dir.path().join("learning");
    let mut store = init_root(&root);
    let first = store
        .ingest(&candidate())
        .expect("revision 1 at 2026-11-01T00:00:00Z");

    // revision + 1 with an earlier updated_at is out of order: nothing written.
    let mut out_of_order = candidate();
    out_of_order["revision"] = json!(2);
    out_of_order["source_checkpoint"] = json!("checkpoint-early");
    out_of_order["created_at"] = json!("2026-10-31T22:00:00Z");
    out_of_order["updated_at"] = json!("2026-10-31T23:30:00Z");
    let error = store
        .ingest(&out_of_order)
        .expect_err("an earlier updated_at must be refused");
    assert_eq!(error.code, "CANDIDATE_REVISION_CONFLICT");
    assert_eq!(error.exit_code(), 2);
    assert!(
        error.message.contains("out-of-order"),
        "message must say out-of-order: {}",
        error.message
    );

    // No half-write: the stored revision, payload, evidence and audit rows are
    // exactly what revision 1 committed.
    let view = store.show(&first.candidate_id).expect("show");
    assert_eq!(view.candidate.revision, 1);
    assert_eq!(view.candidate.updated_at, "2026-11-01T00:00:00Z");
    assert_eq!(view.payload_sha256, first.payload_sha256);
    assert_eq!(view.candidate.evidence.len(), 1);
    assert_eq!(view.revisions.len(), 1);
    assert_eq!(view.ingestions.len(), 1);
    assert_eq!(view.status_history.len(), 1);

    // The same instant expressed with another offset is not "earlier".
    let mut same_instant = candidate();
    same_instant["revision"] = json!(2);
    same_instant["source_checkpoint"] = json!("checkpoint-eq");
    same_instant["updated_at"] = json!("2026-11-01T01:00:00+01:00");
    same_instant["evidence"] = json!([evidence("e1"), evidence("e2")]);
    let second = store
        .ingest(&same_instant)
        .expect("equal instant is accepted");
    assert_eq!(second.revision, 2);
    assert_eq!(second.evidence_added, 2);

    // A later revision that is earlier than the *new* stored revision is
    // refused with the same code and again writes nothing.
    let mut late_old = candidate();
    late_old["revision"] = json!(3);
    late_old["source_checkpoint"] = json!("checkpoint-late");
    late_old["created_at"] = json!("2026-10-31T22:00:00Z");
    late_old["updated_at"] = json!("2026-10-31T23:45:00Z");
    let error = store
        .ingest(&late_old)
        .expect_err("earlier than the stored revision 2");
    assert_eq!(error.code, "CANDIDATE_REVISION_CONFLICT");
    assert!(error.message.contains("out-of-order"), "{}", error.message);
    let view = store.show(&first.candidate_id).expect("show");
    assert_eq!(view.candidate.revision, 2);
    assert_eq!(view.revisions.len(), 2);
    assert_eq!(view.ingestions.len(), 2);

    // A later updated_at is accepted.
    late_old["created_at"] = json!("2026-11-01T00:00:00Z");
    late_old["updated_at"] = json!("2026-11-01T05:00:00Z");
    let third = store.ingest(&late_old).expect("later revision accepted");
    assert_eq!(third.revision, 3);

    // Lower and skipped revisions are still refused.
    let mut lower = candidate();
    lower["revision"] = json!(2);
    lower["source_checkpoint"] = json!("checkpoint-lower");
    lower["updated_at"] = json!("2026-11-02T00:00:00Z");
    let error = store.ingest(&lower).expect_err("lower revision");
    assert_eq!(error.code, "CANDIDATE_REVISION_CONFLICT");

    let mut skipped = candidate();
    skipped["revision"] = json!(5);
    skipped["source_checkpoint"] = json!("checkpoint-skip");
    skipped["updated_at"] = json!("2026-11-03T00:00:00Z");
    let error = store.ingest(&skipped).expect_err("skipped revision");
    assert_eq!(error.code, "CANDIDATE_REVISION_CONFLICT");

    let mut zero = candidate();
    zero["revision"] = json!(0);
    let error = store.ingest(&zero).expect_err("revision zero");
    assert_eq!(error.code, "INVALID_CANDIDATE");

    let view = store.show(&first.candidate_id).expect("show");
    assert_eq!(view.candidate.revision, 3);
    assert_eq!(view.revisions.len(), 3);
    assert_eq!(view.ingestions.len(), 3);
}

// ---------------------------------------------------------------------------
// 7. The gate as an optional ingest precondition
// ---------------------------------------------------------------------------

struct GateFixture {
    temp: tempfile::TempDir,
    archive: Archive,
    learning_root: PathBuf,
    record_sha: String,
}

const PLAIN_USER: &str = r#"{"type":"user","message":{"role":"user","content":"Always use explicit absolute roots"},"sessionId":"s-1"}"#;

impl GateFixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().expect("temp root");
        let archive = Archive::init(temp.path()).expect("archive init");
        let source = temp.path().join("projects").join("session.jsonl");
        std::fs::create_dir_all(source.parent().expect("parent")).expect("create source dir");
        std::fs::write(&source, format!("{PLAIN_USER}\n")).expect("write source");
        let report = capture_source(&archive, &source, "claude", DOMAIN, None).expect("capture");
        assert!(report.ok, "capture must succeed: {:?}", report.error);
        let learning_root = temp.path().join("learning");
        Self {
            record_sha: hex_sha256(PLAIN_USER.as_bytes()),
            temp,
            archive,
            learning_root,
        }
    }

    fn archive_root(&self) -> &Path {
        self.temp.path()
    }

    fn archived_evidence(&self) -> Value {
        json!({
            "ref": format!("archive-record:{}", self.record_sha),
            "span_sha256": self.record_sha,
            "archive_generation": "1",
            "observed_at": "2026-11-01T00:00:00Z",
            "actor": "owner",
            "session_kind": "interactive",
            "origin_class": "owner",
            "evidence_kind": "user_statement",
            "recall_taint": false
        })
    }

    fn candidate(&self, claim: &str, checkpoint: &str) -> Value {
        let mut value = named(claim);
        value["evidence"] = json!([self.archived_evidence()]);
        value["source_checkpoint"] = json!(checkpoint);
        value
    }
}

#[test]
fn gated_ingest_blocks_bad_evidence_and_keeps_s1_without_flags() {
    let fixture = GateFixture::new();
    let root_text = fixture.learning_root.to_string_lossy().to_string();
    let archive_text = fixture.archive_root().to_string_lossy().to_string();
    let records = fixture.archive.load_records().expect("record index");
    assert!(
        records.records.contains_key(&fixture.record_sha),
        "the synthetic record must be committed"
    );

    let init = cli()
        .args(["learning", "init", "--root", &root_text])
        .output()
        .expect("cli init");
    assert_eq!(init.status.code(), Some(0), "{init:?}");
    assert_eq!(
        stdout_json(&init)["db_schema_version"],
        i64::from(DB_SCHEMA_VERSION)
    );

    // A candidate whose evidence cannot be read back from the archive.
    let mut bad = candidate();
    bad["source_checkpoint"] = json!("checkpoint-ungated");
    let bad_path = fixture.temp.path().join("bad.json");
    std::fs::write(&bad_path, serde_json::to_vec(&bad).expect("serialize")).expect("write bad");
    let bad_arg = format!("@{}", bad_path.display());

    let blocked = cli()
        .args(["learning", "ingest", "--root", &root_text])
        .args(["--archive", &archive_text, "--domain", DOMAIN])
        .args(["--candidate", &bad_arg])
        .output()
        .expect("gated ingest of bad evidence");
    assert_eq!(
        blocked.status.code(),
        Some(2),
        "stderr: {}",
        String::from_utf8_lossy(&blocked.stderr)
    );
    let error = stderr_json(&blocked);
    assert_eq!(error["error"]["code"], "GATE_BLOCKED");
    let message = error["error"]["message"].as_str().expect("message");
    assert!(
        message.contains("1"),
        "message names the failed count: {message}"
    );
    assert!(
        message.contains("E_REF_FORMAT"),
        "message names the first failing code: {message}"
    );
    assert_eq!(
        all_counts(&fixture.learning_root),
        [0, 0, 0, 0, 0],
        "a blocked ingest writes nothing"
    );

    // Missing one of the two flags is a usage error, never a silent ungated run.
    for args in [
        vec!["--archive".to_string(), archive_text.clone()],
        vec!["--domain".to_string(), DOMAIN.to_string()],
    ] {
        let out = cli()
            .args(["learning", "ingest", "--root", &root_text])
            .args(&args)
            .args(["--candidate", &bad_arg])
            .output()
            .expect("gated ingest with one flag");
        assert_eq!(out.status.code(), Some(2), "args: {args:?}");
        assert_eq!(stderr_json(&out)["error"]["code"], "INVALID_USAGE");
        assert_eq!(all_counts(&fixture.learning_root), [0, 0, 0, 0, 0]);
    }

    // Good evidence passes the gate and lands normally.
    let good = fixture.candidate(
        "Gated evidence must resolve in the archive",
        "checkpoint-good",
    );
    let good_path = fixture.temp.path().join("good.json");
    std::fs::write(&good_path, serde_json::to_vec(&good).expect("serialize")).expect("write good");
    let accepted = cli()
        .args(["learning", "ingest", "--root", &root_text])
        .args(["--archive", &archive_text, "--domain", DOMAIN])
        .args(["--candidate", &format!("@{}", good_path.display())])
        .output()
        .expect("gated ingest of good evidence");
    assert_eq!(
        accepted.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&accepted.stderr)
    );
    let accepted_json = stdout_json(&accepted);
    assert_eq!(accepted_json["replayed"], false);
    let good_id = accepted_json["candidate_id"]
        .as_str()
        .expect("candidate id")
        .to_string();
    assert_eq!(
        all_counts(&fixture.learning_root),
        [1, 1, 1, 1, 0],
        "the gated ingest commits the normal S1 rows"
    );

    // The same candidate through an archive domain that cannot see the record
    // is blocked as unresolved.
    let hidden = fixture.candidate("A second gated candidate", "checkpoint-hidden");
    let hidden_path = fixture.temp.path().join("hidden.json");
    std::fs::write(
        &hidden_path,
        serde_json::to_vec(&hidden).expect("serialize"),
    )
    .expect("write hidden");
    let blocked = cli()
        .args(["learning", "ingest", "--root", &root_text])
        .args(["--archive", &archive_text, "--domain", "other-domain"])
        .args(["--candidate", &format!("@{}", hidden_path.display())])
        .output()
        .expect("gated ingest with a denied domain");
    assert_eq!(blocked.status.code(), Some(2));
    let message = stderr_json(&blocked)["error"]["message"]
        .as_str()
        .expect("message")
        .to_string();
    assert_eq!(stderr_json(&blocked)["error"]["code"], "GATE_BLOCKED");
    assert!(message.contains("E_REF_UNRESOLVED"), "{message}");
    assert_eq!(count(&fixture.learning_root, "candidates"), 1);

    // Without the flags the behaviour is exactly S1: the same bad evidence is
    // stored, ungated.
    let ungated = cli()
        .args(["learning", "ingest", "--root", &root_text])
        .args(["--candidate", &bad_arg])
        .output()
        .expect("ungated ingest");
    assert_eq!(
        ungated.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&ungated.stderr)
    );
    let ungated_json = stdout_json(&ungated);
    assert_ne!(ungated_json["candidate_id"], good_id.as_str());
    assert_eq!(
        all_counts(&fixture.learning_root),
        [2, 2, 2, 2, 0],
        "an ungated ingest behaves exactly like S1"
    );

    // A gate run stays read-only.
    let before_gate = all_counts(&fixture.learning_root);
    let report = cli()
        .args(["learning", "gate", "--root", &root_text])
        .args(["--archive", &archive_text, "--domain", DOMAIN])
        .args(["--candidate-id", &good_id])
        .output()
        .expect("gate report");
    assert_eq!(report.status.code(), Some(0));
    assert_eq!(stdout_json(&report)["ok"], true);
    assert_eq!(stdout_json(&report)["owner_backed"], true);
    assert_eq!(all_counts(&fixture.learning_root), before_gate);
}

// ---------------------------------------------------------------------------
// 8. An existing candidate list is never copied or rewritten
// ---------------------------------------------------------------------------

#[test]
fn existing_candidate_list_file_is_byte_identical_after_the_whole_workflow() {
    let dir = tempfile::tempdir().expect("temp root");
    let existing = dir.path().join("CANDIDATES.md");
    let body =
        "# 既有候选\n\n- claim: 显式根\n  target: AGENTS.md#explicit-roots\n  status: pending\n\
                - claim: 单写者\n  target: agent-memory#single-writer\n";
    std::fs::write(&existing, body).expect("write existing candidate list");
    let before_bytes = std::fs::read(&existing).expect("read existing list");
    let before_meta = std::fs::metadata(&existing).expect("stat existing list");

    let root = dir.path().join("learning");
    let root_text = root.to_string_lossy().to_string();

    let init = cli()
        .args(["learning", "init", "--root", &root_text])
        .output()
        .expect("init");
    assert_eq!(init.status.code(), Some(0));

    let value = candidate();
    let text = serde_json::to_string(&value).expect("candidate json");
    let ingest = cli()
        .args(["learning", "ingest", "--root", &root_text])
        .args(["--candidate", &text])
        .output()
        .expect("ingest");
    assert_eq!(ingest.status.code(), Some(0), "{ingest:?}");
    let candidate_id = stdout_json(&ingest)["candidate_id"]
        .as_str()
        .expect("candidate id")
        .to_string();

    for args in [
        vec![
            "learning".to_string(),
            "show".into(),
            "--root".into(),
            root_text.clone(),
        ],
        vec![
            "learning".to_string(),
            "list".into(),
            "--root".into(),
            root_text.clone(),
        ],
    ] {
        let mut args = args;
        if args[1] == "show" {
            args.extend(["--candidate-id".to_string(), candidate_id.clone()]);
        }
        let out = cli().args(&args).output().expect("run learning command");
        assert_eq!(out.status.code(), Some(0), "args: {args:?}");
    }

    let dedup = cli()
        .args(["learning", "dedup", "--root", &root_text])
        .args(["--candidate-id", &candidate_id])
        .args([
            "--report",
            &serde_json::to_string(&report(1)).expect("report"),
        ])
        .args([
            "--disposition",
            "related",
            "--reason",
            "drafted from the manual list",
        ])
        .output()
        .expect("dedup");
    assert_eq!(
        dedup.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&dedup.stderr)
    );

    let after_bytes = std::fs::read(&existing).expect("re-read existing list");
    let after_meta = std::fs::metadata(&existing).expect("re-stat existing list");
    assert_eq!(
        before_bytes, after_bytes,
        "the candidate list is never rewritten"
    );
    assert_eq!(
        before_meta.len(),
        after_meta.len(),
        "the candidate list keeps its size"
    );

    // Nothing was copied into the learning root: it stays a single database.
    let mut names: Vec<String> = std::fs::read_dir(&root)
        .expect("read learning root")
        .map(|entry| {
            entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .to_string()
        })
        .collect();
    names.sort();
    for name in &names {
        assert!(
            name == DB_FILE
                || name == &format!("{DB_FILE}-wal")
                || name == &format!("{DB_FILE}-shm"),
            "unexpected file in the learning root: {name} ({names:?})"
        );
    }
    assert!(!root.join("CANDIDATES.md").exists());
    assert_eq!(count(&root, "candidates"), 1);
    assert_eq!(count(&root, "dedup_history"), 1);
}
