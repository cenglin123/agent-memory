//! Stage 7 §A: rejection / deferral qualification, anti-rebirth, the read-only
//! `learning qualified` surface, the audit feed and MCP `memory_qualified`.
//!
//! Every fixture is synthetic and lives under a `tempfile` root. Nothing here
//! reads a real learning root, a real Archive, a real recall configuration or
//! any host session data.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use chrono::{Duration, Utc};
use rusqlite::types::Value as SqlValue;
use rusqlite::Connection;
use serde_json::{json, Value};

use agent_memory::archive::Archive;
use agent_memory::capture::capture_source;
use agent_memory::learning::model::{candidate_id_for, validate_candidate};
use agent_memory::learning::{
    LearningStore, QualifyRequest, DB_FILE, DB_SCHEMA_VERSION, E_REF_FORMAT, NOTHING_TO_CLEAR,
    REJECTED_CLAIM_MATCH, STATE_CLEARED, STATE_OPEN, STATE_REJECTED, TASK_NOT_QUALIFIABLE,
};

const DOMAIN: &str = "default";

// ---------------------------------------------------------------------------
// CLI helpers
// ---------------------------------------------------------------------------

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_agent-memory")
}

fn cli() -> Command {
    Command::new(bin())
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

fn text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

// ---------------------------------------------------------------------------
// Candidate fixtures
// ---------------------------------------------------------------------------

fn evidence(tag: &str) -> Value {
    json!({
        "ref": format!("claude:session-{tag}:line-1"),
        "span_sha256": format!("{:064x}", 1u8),
        "archive_generation": "g1",
        "observed_at": "2026-11-01T00:00:00Z",
        "actor": "user",
        "session_kind": "interactive",
        "origin_class": "owner",
        "evidence_kind": "user_statement",
        "recall_taint": false
    })
}

fn knowledge_payload(claim: &str, target_kind: &str, checkpoint: &str) -> Value {
    json!({
        "schema_version": 1,
        "candidate_id": "",
        "revision": 1,
        "status": "candidate",
        "claim": claim,
        "target_kind": target_kind,
        "proposed_target": format!("{target_kind}:{checkpoint}"),
        "applicability": ["agent-memory"],
        "counterexamples": [],
        "unknowns": [],
        "evidence": [evidence(checkpoint)],
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
        "source_checkpoint": checkpoint,
        "extractor": "extractor-q",
        "created_at": "2026-11-01T00:00:00Z",
        "updated_at": "2026-11-01T00:00:00Z"
    })
}

fn new_root() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("temp root");
    let root = dir.path().join("learning");
    LearningStore::init(&root).expect("init learning root");
    (dir, root)
}

fn ingest_cli(root: &Path, payload: &Value) -> (Value, i32) {
    let out = cli()
        .args(["learning", "ingest", "--root", &text(root), "--candidate"])
        .arg(payload.to_string())
        .output()
        .expect("run learning ingest");
    let code = out.status.code().expect("exit code");
    let value = if code == 0 {
        stdout_json(&out)
    } else {
        stderr_json(&out)
    };
    (value, code)
}

fn ingest_ok(root: &Path, payload: &Value) -> String {
    let (value, code) = ingest_cli(root, payload);
    assert_eq!(code, 0, "ingest failed: {value}");
    value["candidate_id"].as_str().unwrap().to_string()
}

fn qualify_cli(
    root: &Path,
    candidate_id: &str,
    state: &str,
    extra: &[&str],
) -> (Value, i32) {
    qualify_as(
        root,
        candidate_id,
        state,
        "opencode:test-qualifier",
        extra,
    )
}

fn qualify_as(
    root: &Path,
    candidate_id: &str,
    state: &str,
    actor: &str,
    extra: &[&str],
) -> (Value, i32) {
    let mut command = cli();
    command.args([
        "learning",
        "qualify",
        "--root",
        &text(root),
        "--candidate-id",
        candidate_id,
        "--state",
        state,
        "--actor",
        actor,
    ]);
    command.args(extra);
    let out = command.output().expect("run learning qualify");
    let code = out.status.code().expect("exit code");
    let value = if code == 0 {
        stdout_json(&out)
    } else {
        stderr_json(&out)
    };
    (value, code)
}

fn qualify_ok(root: &Path, candidate_id: &str, state: &str, extra: &[&str]) -> Value {
    let (value, code) = qualify_cli(root, candidate_id, state, extra);
    assert_eq!(code, 0, "qualify failed: {value}");
    value
}

// ---------------------------------------------------------------------------
// Schema fixtures (v1 .. v4)
// ---------------------------------------------------------------------------

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
    PRIMARY KEY (candidate_id, revision)
);
CREATE TABLE ingestions (
    ingest_key TEXT PRIMARY KEY NOT NULL,
    candidate_id TEXT NOT NULL,
    revision INTEGER NOT NULL,
    payload_sha256 TEXT NOT NULL,
    extractor TEXT NOT NULL,
    source_checkpoint TEXT NOT NULL,
    created_at TEXT NOT NULL
);
CREATE TABLE status_history (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    candidate_id TEXT NOT NULL,
    revision INTEGER NOT NULL,
    status TEXT NOT NULL,
    recorded_at TEXT NOT NULL
);
CREATE INDEX status_history_candidate_idx ON status_history(candidate_id, seq);
";

const V2_SCHEMA_EXTRA: &str = "
CREATE TABLE dedup_history (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    candidate_id TEXT NOT NULL,
    query TEXT NOT NULL,
    matches_json TEXT NOT NULL,
    disposition TEXT NOT NULL,
    reason TEXT NOT NULL,
    checked_at TEXT NOT NULL
);
CREATE INDEX dedup_history_candidate_idx ON dedup_history(candidate_id, seq);
";

const V3_SCHEMA_EXTRA: &str = "
CREATE TABLE reuse_history (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    candidate_id TEXT NOT NULL,
    task_id TEXT NOT NULL,
    outcome TEXT NOT NULL,
    notes TEXT NOT NULL,
    recorded_at TEXT NOT NULL,
    UNIQUE (candidate_id, task_id)
);
CREATE INDEX reuse_history_candidate_idx ON reuse_history(candidate_id, seq);
CREATE TABLE validation_history (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    candidate_id TEXT NOT NULL,
    reviewer TEXT NOT NULL,
    verdict TEXT NOT NULL,
    report_ref TEXT NOT NULL,
    recorded_at TEXT NOT NULL
);
CREATE INDEX validation_history_candidate_idx ON validation_history(candidate_id, seq);
";

const V4_SCHEMA_EXTRA: &str = "
CREATE TABLE event_clock (seq INTEGER PRIMARY KEY AUTOINCREMENT);
CREATE TABLE task_events (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    event_seq INTEGER NOT NULL,
    candidate_id TEXT NOT NULL,
    event_kind TEXT NOT NULL CHECK (event_kind IN ('claim', 'abandon')),
    actor_id TEXT NOT NULL,
    note TEXT NOT NULL DEFAULT '',
    recorded_at TEXT NOT NULL
);
CREATE INDEX task_events_candidate_idx ON task_events(candidate_id, seq);
";

fn insert_candidate(conn: &Connection, payload: &Value) -> String {
    let validated = validate_candidate(payload).expect("valid fixture candidate");
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
    .expect("insert candidates");
    conn.execute(
        "INSERT INTO candidate_revisions \
         (candidate_id, revision, status, record_json, payload_sha256, recorded_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, '2026-11-01T00:00:00.000Z')",
        rusqlite::params![
            record.candidate_id,
            record.revision as i64,
            record.status,
            validated.payload_json,
            validated.payload_sha256
        ],
    )
    .expect("insert revisions");
    conn.execute(
        "INSERT INTO ingestions \
         (ingest_key, candidate_id, revision, payload_sha256, extractor, source_checkpoint, \
          created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, '2026-11-01T00:00:00.000Z')",
        rusqlite::params![
            validated.ingest_key,
            record.candidate_id,
            record.revision as i64,
            validated.payload_sha256,
            record.extractor,
            record.source_checkpoint
        ],
    )
    .expect("insert ingestions");
    conn.execute(
        "INSERT INTO status_history (candidate_id, revision, status, recorded_at) \
         VALUES (?1, ?2, 'candidate', '2026-11-01T00:00:00.000Z')",
        rusqlite::params![record.candidate_id, record.revision as i64],
    )
    .expect("insert status");
    record.candidate_id.clone()
}

/// Build a real old-version database. `version` is 1, 2, 3 or 4.
fn write_versioned_database(root: &Path, version: i64) -> String {
    std::fs::create_dir_all(root).expect("root");
    let db = root.join(DB_FILE);
    let conn = Connection::open(&db).expect("open fixture db");
    conn.execute_batch("PRAGMA foreign_keys=ON;").expect("fk");
    conn.execute_batch(V1_SCHEMA).expect("v1 schema");
    if version >= 2 {
        conn.execute_batch(V2_SCHEMA_EXTRA).expect("v2 schema");
    }
    if version >= 3 {
        conn.execute_batch(V3_SCHEMA_EXTRA).expect("v3 schema");
    }
    if version >= 4 {
        // v4 rebuilds both event tables with `event_seq` and adds the clock.
        conn.execute_batch(
            "DROP TABLE reuse_history; DROP TABLE validation_history;",
        )
        .expect("drop v3 event tables");
        conn.execute_batch(V4_SCHEMA_EXTRA).expect("v4 extra");
        conn.execute_batch(
            "CREATE TABLE reuse_history (
                 seq INTEGER PRIMARY KEY AUTOINCREMENT,
                 event_seq INTEGER NOT NULL,
                 candidate_id TEXT NOT NULL,
                 task_id TEXT NOT NULL,
                 outcome TEXT NOT NULL,
                 notes TEXT NOT NULL,
                 recorded_at TEXT NOT NULL,
                 actor_id TEXT NOT NULL DEFAULT 'migrated:v3',
                 client_seq INTEGER NOT NULL DEFAULT 0
             );
             CREATE INDEX reuse_history_candidate_idx ON reuse_history(candidate_id, seq);
             CREATE UNIQUE INDEX reuse_history_idem_idx
                 ON reuse_history(task_id, actor_id, client_seq);
             CREATE TABLE validation_history (
                 seq INTEGER PRIMARY KEY AUTOINCREMENT,
                 event_seq INTEGER NOT NULL,
                 candidate_id TEXT NOT NULL,
                 reviewer TEXT NOT NULL,
                 verdict TEXT NOT NULL,
                 report_ref TEXT NOT NULL,
                 recorded_at TEXT NOT NULL
             );
             CREATE INDEX validation_history_candidate_idx
                 ON validation_history(candidate_id, seq);",
        )
        .expect("v4 event tables");
    }
    conn.pragma_update(None, "user_version", version)
        .expect("user_version");

    let candidate_id = insert_candidate(
        &conn,
        &knowledge_payload("Keep every row across the migration", "note", "cp-migrate"),
    );
    if version >= 4 {
        // One reuse and one validation row, with their global event sequence.
        for _ in 0..2 {
            conn.execute("INSERT INTO event_clock DEFAULT VALUES", [])
                .expect("clock row");
        }
        conn.execute(
            "INSERT INTO reuse_history \
             (event_seq, candidate_id, task_id, outcome, notes, recorded_at, actor_id, client_seq) \
             VALUES (1, ?1, 'task-migrate', 'pass', 'kept', '2026-11-01T00:00:01.000Z', \
              'test:actor', 1)",
            rusqlite::params![candidate_id],
        )
        .expect("reuse row");
        conn.execute(
            "INSERT INTO validation_history \
             (event_seq, candidate_id, reviewer, verdict, report_ref, recorded_at) \
             VALUES (2, ?1, 'reviewer-a', 'pass', 'report-1', '2026-11-01T00:00:02.000Z')",
            rusqlite::params![candidate_id],
        )
        .expect("validation row");
    }
    candidate_id
}

fn table_rows(conn: &Connection, sql: &str) -> Vec<String> {
    let mut stmt = conn.prepare(sql).expect("prepare dump");
    let columns = stmt.column_count();
    stmt.query_map([], |row| {
        let mut cells = Vec::with_capacity(columns);
        for index in 0..columns {
            let value: SqlValue = row.get(index)?;
            cells.push(match value {
                SqlValue::Null => "NULL".to_string(),
                SqlValue::Integer(number) => number.to_string(),
                SqlValue::Real(number) => number.to_string(),
                SqlValue::Text(text) => text,
                SqlValue::Blob(bytes) => format!("blob:{}", bytes.len()),
            });
        }
        Ok(cells.join("\u{1f}"))
    })
    .expect("query dump")
    .collect::<Result<Vec<_>, _>>()
    .expect("collect dump")
}

/// Every pre-v5 table, row for row. The v4 -> v5 migration must leave all of
/// them byte-identical.
fn full_snapshot(db: &Path) -> Vec<(&'static str, Vec<String>)> {
    let conn = Connection::open(db).expect("open db");
    vec![
        (
            "candidates",
            table_rows(
                &conn,
                "SELECT candidate_id, revision, status, record_json, payload_sha256, created_at, \
                 updated_at FROM candidates ORDER BY candidate_id",
            ),
        ),
        (
            "candidate_revisions",
            table_rows(
                &conn,
                "SELECT candidate_id, revision, status, record_json, payload_sha256, recorded_at \
                 FROM candidate_revisions ORDER BY candidate_id, revision",
            ),
        ),
        (
            "ingestions",
            table_rows(
                &conn,
                "SELECT ingest_key, candidate_id, revision, payload_sha256, extractor, \
                 source_checkpoint, created_at FROM ingestions ORDER BY ingest_key",
            ),
        ),
        (
            "status_history",
            table_rows(
                &conn,
                "SELECT seq, candidate_id, revision, status, recorded_at FROM status_history \
                 ORDER BY seq",
            ),
        ),
        (
            "dedup_history",
            table_rows(
                &conn,
                "SELECT seq, candidate_id, query, matches_json, disposition, reason, checked_at \
                 FROM dedup_history ORDER BY seq",
            ),
        ),
        (
            "reuse_history",
            table_rows(
                &conn,
                "SELECT seq, event_seq, candidate_id, task_id, outcome, notes, recorded_at, \
                 actor_id, client_seq FROM reuse_history ORDER BY seq",
            ),
        ),
        (
            "validation_history",
            table_rows(
                &conn,
                "SELECT seq, event_seq, candidate_id, reviewer, verdict, report_ref, recorded_at \
                 FROM validation_history ORDER BY seq",
            ),
        ),
        (
            "task_events",
            table_rows(
                &conn,
                "SELECT seq, event_seq, candidate_id, event_kind, actor_id, note, recorded_at \
                 FROM task_events ORDER BY seq",
            ),
        ),
        (
            "event_clock",
            table_rows(&conn, "SELECT seq FROM event_clock ORDER BY seq"),
        ),
    ]
}

fn user_version(db: &Path) -> i64 {
    Connection::open(db)
        .expect("open db")
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("user_version")
}

fn count(db: &Path, table: &str) -> i64 {
    Connection::open(db)
        .expect("open db")
        .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
            row.get(0)
        })
        .expect("count")
}

fn has_table(db: &Path, table: &str) -> bool {
    Connection::open(db)
        .expect("open db")
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name = ?1",
            rusqlite::params![table],
            |row| row.get::<_, i64>(0),
        )
        .expect("table count")
        > 0
}

// ---------------------------------------------------------------------------
// 1. Schema v5 and the v1..v4 chain
// ---------------------------------------------------------------------------

#[test]
fn init_creates_v5_with_the_qualification_tables() {
    let (_dir, root) = new_root();
    let db = root.join(DB_FILE);
    assert_eq!(user_version(&db), 5);
    assert_eq!(i64::from(DB_SCHEMA_VERSION), 5);
    assert!(has_table(&db, "qualification_history"));
    assert!(has_table(&db, "rejection_overrides"));
    let conn = Connection::open(&db).expect("open db");
    // The v5 tables carry the global event sequence, like every other event
    // stream, so the anomaly scan can anchor on them.
    for (table, columns) in [
        (
            "qualification_history",
            "seq, event_seq, candidate_id, revision, state, basis, counterexample, \
             counterexample_ref, owner, trigger, actor, recorded_at",
        ),
        (
            "rejection_overrides",
            "seq, event_seq, new_candidate_id, matched_candidate_id, reason, actor, recorded_at",
        ),
    ] {
        conn.prepare(&format!("SELECT {columns} FROM {table} LIMIT 0"))
            .unwrap_or_else(|error| panic!("{table} shape: {error}"));
    }
}

#[test]
fn v4_to_v5_is_lossless_and_writes_the_pre_v5_backup() {
    let dir = tempfile::tempdir().expect("temp root");
    let root = dir.path().join("learning");
    let candidate = write_versioned_database(&root, 4);
    let db = root.join(DB_FILE);
    let before = full_snapshot(&db);
    assert_eq!(count(&db, "reuse_history"), 1);
    assert_eq!(count(&db, "validation_history"), 1);

    let store = LearningStore::open(&root).expect("v4 migrates to v5");
    assert_eq!(user_version(&db), 5);
    assert!(has_table(&db, "qualification_history"));
    assert!(has_table(&db, "rejection_overrides"));
    assert_eq!(count(&db, "qualification_history"), 0);
    assert_eq!(count(&db, "rejection_overrides"), 0);

    // Every pre-v5 table is byte-identical, row for row.
    assert_eq!(full_snapshot(&db), before);
    let view = store.show(&candidate).expect("show after migration");
    assert_eq!(view.qualification.state, STATE_OPEN);
    assert!(view.qualification.history.is_empty());
    drop(store);

    // The automatic backup is a usable v4 snapshot.
    let mut backups: Vec<PathBuf> = std::fs::read_dir(&root)
        .expect("read root")
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| {
            path.file_name()
                .map(|name| name.to_string_lossy().starts_with("learning.db.bak-v4-"))
                .unwrap_or(false)
        })
        .collect();
    backups.sort();
    assert_eq!(backups.len(), 1, "exactly one pre-v5 snapshot");
    assert_eq!(user_version(&backups[0]), 4);
    assert_eq!(count(&backups[0], "candidates"), 1);
    assert_eq!(count(&backups[0], "reuse_history"), 1);

    // Re-opening a migrated store is a no-op: no second snapshot, no rewrite.
    drop(LearningStore::open(&root).expect("reopen v5"));
    let backups_after = std::fs::read_dir(&root)
        .expect("read root")
        .filter(|entry| {
            entry
                .as_ref()
                .map(|entry| {
                    entry
                        .file_name()
                        .to_string_lossy()
                        .starts_with("learning.db.bak-v4-")
                })
                .unwrap_or(false)
        })
        .count();
    assert_eq!(backups_after, 1);
}

#[test]
fn v1_v2_v3_databases_chain_to_v5_and_keep_the_candidate() {
    for version in [1_i64, 2, 3] {
        let dir = tempfile::tempdir().expect("temp root");
        let root = dir.path().join(format!("learning-v{version}"));
        let candidate = write_versioned_database(&root, version);
        let db = root.join(DB_FILE);
        let store = LearningStore::open(&root)
            .unwrap_or_else(|error| panic!("v{version} must migrate: {error}"));
        assert_eq!(user_version(&db), 5, "v{version}");
        assert!(has_table(&db, "qualification_history"), "v{version}");
        assert!(has_table(&db, "rejection_overrides"), "v{version}");
        let view = store.show(&candidate).expect("candidate survives");
        assert_eq!(view.candidate.candidate_id, candidate, "v{version}");
        assert_eq!(view.qualification.state, STATE_OPEN, "v{version}");
        drop(store);
    }
}

#[test]
fn a_newer_v6_database_is_refused() {
    let dir = tempfile::tempdir().expect("temp root");
    let root = dir.path().join("learning");
    write_versioned_database(&root, 4);
    let db = root.join(DB_FILE);
    Connection::open(&db)
        .expect("open db")
        .pragma_update(None, "user_version", 6)
        .expect("user_version 6");
    let error = LearningStore::open(&root).expect_err("v6 must be refused");
    assert_eq!(error.code, "SCHEMA_VERSION_UNSUPPORTED");
    assert_eq!(error.exit_code(), 6);
    assert_eq!(user_version(&db), 6, "a refused open never writes");
}

// ---------------------------------------------------------------------------
// 2. Required-field matrix and exit codes
// ---------------------------------------------------------------------------

#[test]
fn qualify_required_matrix_and_exit_codes() {
    let (_dir, root) = new_root();
    let candidate = ingest_ok(
        &root,
        &knowledge_payload("A rule that will be qualified", "rule", "cp-matrix"),
    );

    // rejected: basis and counterexample are both required.
    for args in [
        vec!["--counterexample", "case"],
        vec!["--basis", "why"],
        vec![],
    ] {
        let (value, code) = qualify_cli(&root, &candidate, "rejected", &args);
        assert_eq!(code, 2, "rejected {args:?}: {value}");
        assert_eq!(value["error"]["code"], "INVALID_USAGE");
    }
    // deferred: owner and trigger are both required.
    for args in [
        vec!["--trigger", "when new evidence arrives"],
        vec!["--owner", "user"],
        vec![],
    ] {
        let (value, code) = qualify_cli(&root, &candidate, "deferred", &args);
        assert_eq!(code, 2, "deferred {args:?}: {value}");
        assert_eq!(value["error"]["code"], "INVALID_USAGE");
    }
    // cleared: basis is required.
    let (value, code) = qualify_cli(&root, &candidate, "cleared", &[]);
    assert_eq!(code, 2);
    assert_eq!(value["error"]["code"], "INVALID_USAGE");

    // Unknown state.
    let (value, code) = qualify_cli(&root, &candidate, "accepted", &["--basis", "x"]);
    assert_eq!(code, 2);
    assert_eq!(value["error"]["code"], "INVALID_USAGE");

    // Unknown candidate.
    let (value, code) = qualify_cli(
        &root,
        "cand_00000000000000000000000000000000",
        "rejected",
        &["--basis", "x", "--counterexample", "y"],
    );
    assert_eq!(code, 6);
    assert_eq!(value["error"]["code"], "CANDIDATE_NOT_FOUND");

    // cleared on a candidate with no qualification row.
    let (value, code) = qualify_cli(&root, &candidate, "cleared", &["--basis", "none"]);
    assert_eq!(code, 2);
    assert_eq!(value["error"]["code"], NOTHING_TO_CLEAR);

    // A task candidate is not a qualification subject.
    let mut store = LearningStore::open(&root).expect("open");
    let task = store
        .create_task("A task that cannot be qualified", "opencode:t")
        .expect("create task")
        .candidate_id;
    drop(store);
    let (value, code) = qualify_cli(
        &root,
        &task,
        "rejected",
        &["--basis", "x", "--counterexample", "y"],
    );
    assert_eq!(code, 2);
    assert_eq!(value["error"]["code"], TASK_NOT_QUALIFIABLE);
    assert_eq!(count(&root.join(DB_FILE), "qualification_history"), 0);
}

// ---------------------------------------------------------------------------
// 3. Idempotency, clearing and the conflict signal
// ---------------------------------------------------------------------------

#[test]
fn qualify_is_idempotent_only_on_the_same_state_and_basis() {
    let (_dir, root) = new_root();
    let candidate = ingest_ok(
        &root,
        &knowledge_payload("Idempotency is per newest row", "note", "cp-idem"),
    );

    let first = qualify_ok(
        &root,
        &candidate,
        "rejected",
        &["--basis", "disproved", "--counterexample", "case one"],
    );
    assert_eq!(first["duplicate"], false);
    assert_eq!(first["prior_state"], STATE_OPEN);
    assert_eq!(first["history_seq"], 1);

    // Same state + same basis: no row is appended even when the other fields
    // differ (the contract compares exactly those two values).
    let duplicate = qualify_ok(
        &root,
        &candidate,
        "rejected",
        &["--basis", "disproved", "--counterexample", "different case"],
    );
    assert_eq!(duplicate["duplicate"], true);
    assert_eq!(duplicate["history_seq"], 1);
    assert_eq!(
        count(&root.join(DB_FILE), "qualification_history"),
        1,
        "the duplicate appends nothing"
    );

    // cleared, then the same rejected basis again: a new row, not a replay.
    let cleared = qualify_ok(&root, &candidate, "cleared", &["--basis", "new evidence"]);
    assert_eq!(cleared["duplicate"], false);
    assert_eq!(cleared["prior_state"], STATE_REJECTED);
    assert_eq!(cleared["cleared_conflict"], false);
    let again = qualify_ok(
        &root,
        &candidate,
        "rejected",
        &["--basis", "disproved", "--counterexample", "case one"],
    );
    assert_eq!(again["duplicate"], false);
    assert_eq!(again["prior_state"], STATE_CLEARED);
    assert_eq!(again["history_seq"], 3);
    assert_eq!(count(&root.join(DB_FILE), "qualification_history"), 3);

    // A different actor clearing a rejection carries the conflict signal.
    let (conflict, code) = qualify_as(
        &root,
        &candidate,
        "cleared",
        "opencode:other",
        &["--basis", "withdrawn"],
    );
    assert_eq!(code, 0, "{conflict}");
    assert_eq!(conflict["cleared_conflict"], true);
    let show = cli()
        .args([
            "learning",
            "show",
            "--root",
            &text(&root),
            "--candidate-id",
            &candidate,
        ])
        .output()
        .expect("show");
    let view = stdout_json(&show);
    assert_eq!(view["qualification"]["state"], STATE_CLEARED);
    assert_eq!(view["qualification"]["history"].as_array().unwrap().len(), 4);
}

// ---------------------------------------------------------------------------
// 4. counterexample_ref shape and archive readback
// ---------------------------------------------------------------------------

struct ArchiveFixture {
    temp: tempfile::TempDir,
    archive: Archive,
    reference: String,
}

fn archive_fixture() -> ArchiveFixture {
    let temp = tempfile::tempdir().expect("temp archive root");
    let archive = Archive::init(temp.path()).expect("archive init");
    let source = temp.path().join("session.jsonl");
    let line = json!({
        "type": "user",
        "message": {"role": "user", "content": "the counterexample is here"},
        "timestamp": "2026-11-01T00:00:00Z",
        "sessionId": "11111111-1111-4111-8111-111111111111",
        "uuid": "u1",
        "isSidechain": false
    })
    .to_string();
    std::fs::write(&source, format!("{line}\n")).expect("write source");
    let report = capture_source(&archive, &source, "claude", DOMAIN, None).expect("capture");
    assert!(report.ok, "capture must succeed: {:?}", report.error);
    let records = archive.load_records().expect("records");
    let sha = records
        .records
        .keys()
        .next()
        .expect("one captured record")
        .clone();
    ArchiveFixture {
        temp,
        archive,
        reference: format!("archive-record:{sha}"),
    }
}

#[test]
fn counterexample_ref_shape_and_archive_readback() {
    let (_dir, root) = new_root();
    let candidate = ingest_ok(
        &root,
        &knowledge_payload("Rejections cite a counterexample record", "rule", "cp-ref"),
    );

    // Shape failure is E_REF_FORMAT, exit 2.
    let (value, code) = qualify_cli(
        &root,
        &candidate,
        "rejected",
        &[
            "--basis",
            "disproved",
            "--counterexample",
            "case",
            "--counterexample-ref",
            "claude:session-x:line-1",
        ],
    );
    assert_eq!(code, 2);
    assert_eq!(value["error"]["code"], E_REF_FORMAT);

    // Shape only: accepted with ref_verified=false.
    let shape_only = qualify_ok(
        &root,
        &candidate,
        "rejected",
        &[
            "--basis",
            "disproved",
            "--counterexample",
            "case",
            "--counterexample-ref",
            &format!("archive-record:{}", "a".repeat(64)),
        ],
    );
    assert_eq!(shape_only["ref_verified"], false);
    assert_eq!(
        shape_only["counterexample_ref"],
        format!("archive-record:{}", "a".repeat(64))
    );

    // With an explicit archive/domain the ref is read back and verified.
    let fixture = archive_fixture();
    let verified = qualify_ok(
        &root,
        &candidate,
        "cleared",
        &[
            "--basis",
            "fresh evidence",
            "--archive",
            &text(fixture.temp.path()),
            "--domain",
            DOMAIN,
            "--counterexample-ref",
            &fixture.reference,
        ],
    );
    assert_eq!(verified["ref_verified"], true);
    assert_eq!(verified["counterexample_ref"], fixture.reference);

    // An unresolvable record is refused (and writes nothing).
    let rows_before = count(&root.join(DB_FILE), "qualification_history");
    let (value, code) = qualify_cli(
        &root,
        &candidate,
        "rejected",
        &[
            "--basis",
            "disproved",
            "--counterexample",
            "case",
            "--archive",
            &text(fixture.temp.path()),
            "--domain",
            DOMAIN,
            "--counterexample-ref",
            &format!("archive-record:{}", "b".repeat(64)),
        ],
    );
    assert_eq!(code, 6);
    assert_eq!(value["error"]["code"], "E_REF_UNRESOLVED");
    assert_eq!(
        count(&root.join(DB_FILE), "qualification_history"),
        rows_before
    );
    let _ = &fixture.archive;
}

/// `verify_counterexample_ref` reads the merged ledger (canonical + journal), so
/// a record appended after the last compaction resolves without one.
#[test]
fn counterexample_ref_resolves_a_record_that_only_lives_in_the_journal() {
    let temp = tempfile::tempdir().expect("temp archive root");
    let archive = Archive::init(temp.path()).expect("archive init");
    let source = temp.path().join("session.jsonl");
    let first = json!({
        "type": "user",
        "message": {"role": "user", "content": "the canonical record"},
        "sessionId": "11111111-1111-4111-8111-111111111111",
        "uuid": "u1",
    })
    .to_string();
    let second = json!({
        "type": "assistant",
        "message": {"role": "assistant", "content": "the journal-only record"},
        "sessionId": "11111111-1111-4111-8111-111111111111",
        "uuid": "u2",
    })
    .to_string();
    std::fs::write(&source, format!("{first}\n")).expect("write source");
    let report = capture_source(&archive, &source, "claude", DOMAIN, None).expect("capture");
    assert!(report.ok, "capture must succeed: {:?}", report.error);
    std::fs::write(&source, format!("{first}\n{second}\n")).expect("grow source");
    let report = capture_source(&archive, &source, "claude", DOMAIN, None).expect("capture");
    assert!(report.ok, "capture must succeed: {:?}", report.error);

    let sha = agent_memory::archive::hex_sha256(second.as_bytes());
    let canonical = std::fs::read_to_string(temp.path().join("state").join("records.json"))
        .expect("canonical ledger");
    assert!(
        !canonical.contains(&sha),
        "the fixture record must live in the journal only"
    );

    let verified = agent_memory::learning::verify_counterexample_ref(
        &archive,
        DOMAIN,
        &format!("archive-record:{sha}"),
    )
    .expect("a journal-only record resolves");
    assert!(verified);
}

// ---------------------------------------------------------------------------
// 5. Anti-rebirth and explicit overrides
// ---------------------------------------------------------------------------

#[test]
fn ingest_blocks_rejected_claims_and_records_an_override() {
    let (_dir, root) = new_root();
    let rejected = ingest_ok(
        &root,
        &knowledge_payload(
            "Always use explicit learning roots",
            "rule",
            "cp-rebirth-base",
        ),
    );
    qualify_ok(
        &root,
        &rejected,
        "rejected",
        &["--basis", "disproved", "--counterexample", "case"],
    );

    // (b) Another candidate whose claim normalizes to the same string is
    // refused. A different target kind is enough to mint a different id.
    let mut second = knowledge_payload(
        "Always use explicit learning roots",
        "note",
        "cp-rebirth-other",
    );
    second["proposed_target"] = json!("notes:rebirth");
    let (value, code) = ingest_cli(&root, &second);
    assert_eq!(code, 2, "{value}");
    assert_eq!(value["error"]["code"], REJECTED_CLAIM_MATCH);
    assert!(value["error"]["message"]
        .as_str()
        .unwrap()
        .contains(&rejected));

    // The explicit override writes one row, echoes it, and `show` reads it back.
    let out = cli()
        .args([
            "learning",
            "ingest",
            "--root",
            &text(&root),
            "--candidate",
        ])
        .arg(second.to_string())
        .args([
            "--override-rejected",
            "user reviewed the distinct note",
            "--actor",
            "opencode:override",
        ])
        .output()
        .expect("override ingest");
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    let overridden = stdout_json(&out);
    let new_id = overridden["candidate_id"].as_str().unwrap().to_string();
    assert_eq!(overridden["override"]["reason"], "user reviewed the distinct note");
    assert_eq!(overridden["override"]["actor"], "opencode:override");
    assert_eq!(overridden["override"]["matched_candidate_id"], rejected);

    let show = cli()
        .args([
            "learning",
            "show",
            "--root",
            &text(&root),
            "--candidate-id",
            &new_id,
        ])
        .output()
        .expect("show");
    let view = stdout_json(&show);
    assert_eq!(
        view["qualification"]["overrides"][0]["matched_candidate_id"],
        rejected
    );
    assert_eq!(
        view["qualification"]["overrides"][0]["reason"],
        "user reviewed the distinct note"
    );

    // (a) The target candidate itself is rejected: a new revision of the same
    // claim is refused too.
    let mut revision = knowledge_payload(
        "Always use explicit learning roots",
        "rule",
        "cp-rebirth-base",
    );
    revision["source_checkpoint"] = json!("cp-rebirth-base-r2");
    revision["revision"] = json!(2);
    revision["updated_at"] = json!("2026-11-02T00:00:00Z");
    revision["evidence"] = json!([evidence("cp-rebirth-base-2")]);
    let (value, code) = ingest_cli(&root, &revision);
    assert_eq!(code, 2, "{value}");
    assert_eq!(value["error"]["code"], REJECTED_CLAIM_MATCH);

    // (c) A near (not equal) claim is accepted with a warning.
    let near = knowledge_payload(
        "Always use explicit learning root",
        "note",
        "cp-rebirth-near",
    );
    let (value, code) = ingest_cli(&root, &near);
    assert_eq!(code, 0, "{value}");
    let matches = value["rejected_matches"].as_array().expect("near warning");
    assert_eq!(matches[0]["candidate_id"], rejected);
    assert!(matches[0]["similarity"].as_f64().unwrap() >= 0.6);
}

#[test]
fn ingest_override_without_a_match_writes_no_row() {
    let (_dir, root) = new_root();
    let out = cli()
        .args(["learning", "ingest", "--root", &text(&root), "--candidate"])
        .arg(
            knowledge_payload("An unrelated claim", "note", "cp-unrelated").to_string(),
        )
        .args(["--override-rejected", "not needed", "--actor", "opencode:o"])
        .output()
        .expect("ingest");
    assert_eq!(out.status.code(), Some(0));
    let value = stdout_json(&out);
    assert!(value.get("override").is_none());
    assert!(value.get("rejected_matches").is_none());
    assert_eq!(count(&root.join(DB_FILE), "rejection_overrides"), 0);
}

// ---------------------------------------------------------------------------
// 6. Readiness, show and export
// ---------------------------------------------------------------------------

#[test]
fn gate_target_show_and_export_carry_the_qualification() {
    let (_dir, root) = new_root();
    let candidate = ingest_ok(
        &root,
        &knowledge_payload("Qualification changes readiness", "note", "cp-ready"),
    );

    // Open: the normal mechanical verdict.
    let open = cli()
        .args([
            "learning",
            "gate-target",
            "--root",
            &text(&root),
            "--candidate-id",
            &candidate,
        ])
        .output()
        .expect("gate-target open");
    let open = stdout_json(&open);
    assert_eq!(open["verdict"], "insufficient_evidence");
    assert_eq!(open["qualification"]["state"], STATE_OPEN);

    qualify_ok(
        &root,
        &candidate,
        "deferred",
        &["--owner", "user", "--trigger", "when the new evidence arrives"],
    );
    let deferred = cli()
        .args([
            "learning",
            "gate-target",
            "--root",
            &text(&root),
            "--candidate-id",
            &candidate,
        ])
        .output()
        .expect("gate-target deferred");
    let deferred = stdout_json(&deferred);
    assert_eq!(deferred["verdict"], "deferred");
    assert_eq!(deferred["ok"], false);
    assert_eq!(
        deferred["qualification"]["latest"]["owner"],
        "user"
    );
    assert!(deferred["blocking"]
        .as_array()
        .unwrap()
        .iter()
        .any(|code| code == "qualification_deferred"));

    qualify_ok(
        &root,
        &candidate,
        "cleared",
        &["--basis", "the missing evidence arrived"],
    );
    qualify_ok(
        &root,
        &candidate,
        "rejected",
        &["--basis", "disproved", "--counterexample", "case"],
    );
    let rejected = cli()
        .args([
            "learning",
            "gate-target",
            "--root",
            &text(&root),
            "--candidate-id",
            &candidate,
        ])
        .output()
        .expect("gate-target rejected");
    let rejected = stdout_json(&rejected);
    assert_eq!(rejected["verdict"], "rejected");
    assert_eq!(rejected["ok"], false);
    assert!(rejected["requirements"][0]["detail"]
        .as_str()
        .unwrap()
        .contains("disproved"));

    let export = cli()
        .args([
            "learning",
            "export",
            "--root",
            &text(&root),
            "--candidate-id",
            &candidate,
        ])
        .output()
        .expect("export");
    assert_eq!(export.status.code(), Some(0));
    let package = stdout_json(&export);
    assert_eq!(package["qualification"]["state"], STATE_REJECTED);
    assert_eq!(
        package["qualification"]["history"].as_array().unwrap().len(),
        3
    );
    assert_eq!(package["target_report"]["verdict"], "rejected");
}

// ---------------------------------------------------------------------------
// 7. `learning qualified`
// ---------------------------------------------------------------------------

#[test]
fn qualified_lists_filters_and_searches_claim_basis_and_counterexample() {
    let (_dir, root) = new_root();
    let rejected = ingest_ok(
        &root,
        &knowledge_payload("The rejected conclusion", "note", "cp-list-r"),
    );
    let deferred = ingest_ok(
        &root,
        &knowledge_payload("The deferred conclusion", "workflow", "cp-list-d"),
    );
    qualify_ok(
        &root,
        &rejected,
        "rejected",
        &[
            "--basis",
            "measured counterexample",
            "--counterexample",
            "16 MiB overflow",
        ],
    );
    qualify_ok(
        &root,
        &deferred,
        "deferred",
        &["--owner", "user", "--trigger", "after the next release"],
    );

    let all = cli()
        .args(["learning", "qualified", "--root", &text(&root), "--json"])
        .output()
        .expect("qualified");
    let all = stdout_json(&all);
    assert_eq!(all["total"], 2);
    assert_eq!(all["count"], 2);

    let only_rejected = cli()
        .args([
            "learning",
            "qualified",
            "--root",
            &text(&root),
            "--state",
            "rejected",
            "--json",
        ])
        .output()
        .expect("qualified state");
    let only_rejected = stdout_json(&only_rejected);
    assert_eq!(only_rejected["total"], 1);
    assert_eq!(only_rejected["qualified"][0]["candidate_id"], rejected);

    for (query, expected) in [
        ("rejected conclusion", rejected.clone()),
        ("measured counterexample", rejected.clone()),
        ("overflow", rejected.clone()),
        ("deferred conclusion", deferred.clone()),
    ] {
        let out = cli()
            .args([
                "learning",
                "qualified",
                "--root",
                &text(&root),
                "--query",
                query,
                "--json",
            ])
            .output()
            .expect("qualified query");
        let out = stdout_json(&out);
        assert_eq!(out["total"], 1, "query {query:?}: {out}");
        assert_eq!(out["qualified"][0]["candidate_id"], expected, "{query}");
    }

    // Text mode is also available and names the state.
    let text_out = cli()
        .args(["learning", "qualified", "--root", &text(&root)])
        .output()
        .expect("qualified text");
    let stdout = String::from_utf8_lossy(&text_out.stdout);
    assert!(stdout.contains("rejected"));
    assert!(stdout.contains("deferred"));
    assert!(stdout.contains("qualified: 2 shown"));

    // A withdrawn rejection stays findable through its historical basis.
    qualify_ok(&root, &rejected, "cleared", &["--basis", "new evidence"]);
    let historical = cli()
        .args([
            "learning",
            "qualified",
            "--root",
            &text(&root),
            "--query",
            "measured counterexample",
            "--json",
        ])
        .output()
        .expect("qualified historical query");
    let historical = stdout_json(&historical);
    assert_eq!(historical["total"], 1, "{historical}");
    assert_eq!(historical["qualified"][0]["candidate_id"], rejected);
    assert_eq!(historical["qualified"][0]["state"], STATE_CLEARED);
}

// ---------------------------------------------------------------------------
// 8. Audit: FAIL rows, overdue deferred, flips
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn insert_raw_qualification(
    root: &Path,
    candidate_id: &str,
    state: &str,
    basis: &str,
    counterexample: &str,
    owner: &str,
    trigger: &str,
    recorded_at: &str,
) {
    let conn = Connection::open(root.join(DB_FILE)).expect("open db");
    conn.execute("INSERT INTO event_clock DEFAULT VALUES", [])
        .expect("clock");
    let event_seq = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO qualification_history \
         (event_seq, candidate_id, revision, state, basis, counterexample, counterexample_ref, \
          owner, trigger, actor, recorded_at) \
         VALUES (?1, ?2, 1, ?3, ?4, ?5, '', ?6, ?7, 'direct:db', ?8)",
        rusqlite::params![
            event_seq,
            candidate_id,
            state,
            basis,
            counterexample,
            owner,
            trigger,
            recorded_at
        ],
    )
    .expect("raw qualification row");
}

#[test]
fn audit_grades_fail_rows_overdue_deferred_and_flips() {
    let (_dir, root) = new_root();
    let candidate = ingest_ok(
        &root,
        &knowledge_payload("Audit fixture", "note", "cp-audit"),
    );
    let now = Utc::now();
    let old = (now - Duration::days(40)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let fresh = now.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);

    // FAIL rows that only a direct database edit can produce.
    insert_raw_qualification(&root, &candidate, "rejected", "", "case", "", "", &fresh);
    insert_raw_qualification(&root, &candidate, "deferred", "", "", "", "", &fresh);

    let audit = cli()
        .args([
            "learning",
            "qualified",
            "--root",
            &text(&root),
            "--audit",
            "--json",
        ])
        .output()
        .expect("audit");
    assert_eq!(audit.status.code(), Some(0));
    let audit = stdout_json(&audit);
    assert_eq!(audit["ok"], false);
    assert_eq!(audit["rejected_missing_basis"], 1);
    assert_eq!(audit["deferred_missing_owner"], 1);
    assert_eq!(audit["deferred_missing_trigger"], 1);
    assert!(audit["findings"]
        .as_array()
        .unwrap()
        .iter()
        .any(|finding| finding["level"] == "FAIL"));

    // A separate library: an overdue deferred newest row is WARN, not FAIL.
    let (_dir2, root2) = new_root();
    let candidate2 = ingest_ok(
        &root2,
        &knowledge_payload("Overdue deferred", "note", "cp-overdue"),
    );
    insert_raw_qualification(
        &root2,
        &candidate2,
        "deferred",
        "",
        "",
        "user",
        "after the release",
        &old,
    );
    let audit2 = cli()
        .args([
            "learning",
            "qualified",
            "--root",
            &text(&root2),
            "--audit",
            "--json",
        ])
        .output()
        .expect("audit overdue");
    let audit2 = stdout_json(&audit2);
    assert_eq!(audit2["ok"], true);
    assert_eq!(audit2["deferred_overdue"], 1);
    assert!(audit2["findings"]
        .as_array()
        .unwrap()
        .iter()
        .any(|finding| finding["code"] == "QUAL_DEFERRED_OVERDUE"));

    // Flips: rejected -> cleared -> rejected -> cleared ... within the window.
    let (_dir3, root3) = new_root();
    let candidate3 = ingest_ok(
        &root3,
        &knowledge_payload("Flip fixture", "note", "cp-flip"),
    );
    for (state, basis, counterexample) in [
        ("rejected", "r1", Some("c1")),
        ("cleared", "c1", None),
        ("rejected", "r2", Some("c2")),
        ("cleared", "c2", None),
        ("rejected", "r3", Some("c3")),
        ("cleared", "c3", None),
    ] {
        let mut extra = vec!["--basis", basis];
        if let Some(counterexample) = counterexample {
            extra.push("--counterexample");
            extra.push(counterexample);
        }
        let (value, code) = qualify_as(
            &root3,
            &candidate3,
            state,
            if state == "cleared" {
                "opencode:other"
            } else {
                "opencode:test-qualifier"
            },
            &extra,
        );
        assert_eq!(code, 0, "{value}");
    }
    let audit3 = cli()
        .args([
            "learning",
            "qualified",
            "--root",
            &text(&root3),
            "--audit",
            "--json",
        ])
        .output()
        .expect("audit flips");
    let audit3 = stdout_json(&audit3);
    assert_eq!(audit3["ok"], true);
    assert!(audit3["recent_flips"].as_u64().unwrap() >= 1);
    assert!(audit3["cleared_conflicts"].as_u64().unwrap() >= 1);
}

#[test]
fn anomalies_reports_the_rejected_cleared_flip_burst() {
    let (_dir, root) = new_root();
    let candidate = ingest_ok(
        &root,
        &knowledge_payload("Anomaly flip fixture", "note", "cp-anomaly"),
    );
    for (state, basis, counterexample) in [
        ("rejected", "r1", Some("c1")),
        ("cleared", "c1", None),
        ("rejected", "r2", Some("c2")),
        ("cleared", "c2", None),
        ("rejected", "r3", Some("c3")),
        ("cleared", "c3", None),
    ] {
        let mut extra = vec!["--basis", basis];
        if let Some(counterexample) = counterexample {
            extra.push("--counterexample");
            extra.push(counterexample);
        }
        qualify_ok(&root, &candidate, state, &extra);
    }
    let out = cli()
        .args(["learning", "anomalies", "--root", &text(&root), "--json"])
        .output()
        .expect("anomalies");
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let report = stdout_json(&out);
    let flips: Vec<&Value> = report["signals"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|signal| signal["kind"] == "reject_clear_flip")
        .collect();
    assert_eq!(flips.len(), 1, "{report}");
    assert_eq!(flips[0]["candidate_id"], candidate);
    assert!(flips[0]["detail"]
        .as_str()
        .unwrap()
        .contains("window_days=7"));
}

// ---------------------------------------------------------------------------
// 9. MCP `memory_qualified`
// ---------------------------------------------------------------------------

struct McpSession {
    child: std::process::Child,
    stdin: std::process::ChildStdin,
    stdout: BufReader<std::process::ChildStdout>,
    next_id: u64,
}

impl McpSession {
    fn start(config: &Path) -> McpSession {
        let mut child = Command::new(bin())
            .arg("mcp")
            .arg("--config")
            .arg(config)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("start mcp");
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        let mut session = McpSession {
            child,
            stdin,
            stdout,
            next_id: 1,
        };
        let initialize = session.request("initialize", json!({}));
        assert_eq!(initialize["result"]["protocolVersion"], "2024-11-05");
        session
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        writeln!(
            self.stdin,
            "{}",
            json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
        )
        .expect("write");
        self.stdin.flush().expect("flush");
        let mut line = String::new();
        self.stdout.read_line(&mut line).expect("read");
        let response: Value = serde_json::from_str(&line).expect("response JSON");
        assert_eq!(response["id"], json!(id));
        response
    }

    fn call(&mut self, tool: &str, arguments: Value) -> Value {
        self.request("tools/call", json!({"name": tool, "arguments": arguments}))
    }
}

impl Drop for McpSession {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn mcp_memory_qualified_lists_and_reports_a_missing_learning_root() {
    let dir = tempfile::tempdir().expect("temp root");
    let learning_root = dir.path().join("learning");
    drop(LearningStore::init(&learning_root).expect("init"));
    let candidate = {
        let mut store = LearningStore::open(&learning_root).expect("open");
        let payload = knowledge_payload("MCP visible rejection", "note", "cp-mcp");
        let id = store.ingest(&payload).expect("ingest").candidate_id;
        let request = QualifyRequest::parse(
            &id,
            "rejected",
            "opencode:mcp",
            Some("disproved"),
            Some("case"),
            None,
            None,
            None,
        )
        .expect("request");
        store.qualify(&request, false).expect("qualify");
        id
    };
    let config = dir.path().join("recall.json");
    std::fs::write(&config, "{}").expect("config");

    let mut mcp = McpSession::start(&config);
    let list = mcp.request("tools/list", json!({}));
    let tools = list["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 4);
    assert_eq!(tools[3]["name"], "memory_qualified");
    assert_eq!(tools[3]["annotations"]["readOnlyHint"], json!(true));

    let call = mcp.call(
        "memory_qualified",
        json!({"state": "rejected", "query": "mcp visible", "limit": 5}),
    );
    assert_eq!(call["result"]["isError"], json!(false), "{call}");
    let report = &call["result"]["structuredContent"];
    assert_eq!(report["ok"], true);
    assert_eq!(report["total"], 1);
    assert_eq!(report["qualified"][0]["candidate_id"], candidate);

    // Unknown field and an invalid state are tool errors, not protocol errors.
    let bad_field = mcp.call("memory_qualified", json!({"bogus": true}));
    assert_eq!(bad_field["result"]["isError"], json!(true));
    assert_eq!(
        bad_field["result"]["structuredContent"]["error"]["code"],
        "INVALID_USAGE"
    );
    let bad_state = mcp.call("memory_qualified", json!({"state": "accepted"}));
    assert_eq!(bad_state["result"]["isError"], json!(true));

    // Missing learning root: a stable, explicit error; no other path is guessed.
    let bare = tempfile::tempdir().expect("bare root");
    let bare_config = bare.path().join("recall.json");
    std::fs::write(&bare_config, "{}").expect("bare config");
    let mut bare_mcp = McpSession::start(&bare_config);
    let missing = bare_mcp.call("memory_qualified", json!({}));
    assert_eq!(missing["result"]["isError"], json!(true));
    assert_eq!(
        missing["result"]["structuredContent"]["error"]["code"],
        "LEARNING_ROOT_MISSING"
    );
    assert!(missing["result"]["structuredContent"]["error"]["message"]
        .as_str()
        .unwrap()
        .contains("never guesses"));
}

// ---------------------------------------------------------------------------
// 10. A v5 store has no downgrade path
// ---------------------------------------------------------------------------

#[test]
fn migrate_to_three_refuses_a_v5_store() {
    let (_dir, root) = new_root();
    let out = cli()
        .args(["learning", "migrate", "--root", &text(&root), "--to", "3"])
        .output()
        .expect("migrate");
    assert_eq!(out.status.code(), Some(2));
    let error = stderr_json(&out);
    assert_eq!(error["error"]["code"], "INVALID_USAGE");
    assert!(error["error"]["message"]
        .as_str()
        .unwrap()
        .contains("bak-v4"));
    assert_eq!(user_version(&root.join(DB_FILE)), 5);
}

// ---------------------------------------------------------------------------
// Unit-level checks of the normalization / similarity surface
// ---------------------------------------------------------------------------

#[test]
fn normalization_and_similarity_are_single_sourced() {
    use agent_memory::learning::{jaccard_similarity, normalize_for_rejection};
    assert_eq!(
        normalize_for_rejection("  Ｒｕｌｅ：  Always   Use  Explicit-Roots！ "),
        "rulealwaysuseexplicitroots"
    );
    assert_eq!(
        normalize_for_rejection("规则：始终使用显式根！"),
        "规则始终使用显式根"
    );
    let left = normalize_for_rejection("Always use explicit learning roots");
    let right = normalize_for_rejection("Always use explicit learning root");
    assert!(jaccard_similarity(&left, &right) >= 0.6);
    assert!(jaccard_similarity(&left, &normalize_for_rejection("archive capture")) < 0.6);

    // The identity helper used by the store is the same one the library exports.
    let a = candidate_id_for("Rule: Always  use roots", "note", "n:1");
    let b = candidate_id_for("ｒｕｌｅ： always use roots", "note", "n:1");
    assert_ne!(a, b, "candidate identity keeps its own S1 normalization");
    let _: BTreeMap<String, usize> = BTreeMap::new();
}

#[test]
fn reassigning_a_deferral_is_recorded() {
    let (_dir, root) = new_root();
    let candidate = ingest_ok(
        &root,
        &knowledge_payload("Deferral owner can change", "note", "cp-defer"),
    );
    let first = qualify_ok(
        &root,
        &candidate,
        "deferred",
        &["--owner", "alice", "--trigger", "after the next release"],
    );
    assert_eq!(first["duplicate"], false);
    let same = qualify_ok(
        &root,
        &candidate,
        "deferred",
        &["--owner", "alice", "--trigger", "after the next release"],
    );
    assert_eq!(same["duplicate"], true, "identical deferral is idempotent");
    let reassigned = qualify_ok(
        &root,
        &candidate,
        "deferred",
        &["--owner", "bob", "--trigger", "after the next release"],
    );
    assert_eq!(reassigned["duplicate"], false, "a new owner appends a row");
    let retriggered = qualify_ok(
        &root,
        &candidate,
        "deferred",
        &["--owner", "bob", "--trigger", "when the vendor replies"],
    );
    assert_eq!(retriggered["duplicate"], false, "a new trigger appends a row");
    assert_eq!(count(&root.join(DB_FILE), "qualification_history"), 3);
}
