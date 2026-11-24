//! Stage 4 implementation-001: schema v3 -> v4 migration and the explicit
//! v4 -> v3 downgrade (`learning migrate --to 3`).
//!
//! Every case builds a synthetic learning root from `tempfile`; nothing here
//! reads a real learning root, a real archive, `~/.memory` or any default path.
//! The v3 fixtures are written by hand so the migration has a real v3 database
//! to open, exactly as the S5 tests did for v1/v2.

use std::path::{Path, PathBuf};
use std::process::Command;

use rusqlite::types::Value as SqlValue;
use rusqlite::Connection;
use serde_json::{json, Value};

use agent_memory::archive::hex_sha256;
use agent_memory::learning::model::{
    candidate_id_for, ingest_key_for, validate_candidate, CandidateRecord,
};
use agent_memory::learning::{
    LearningStore, ReuseRequest, ValidationRequest, DB_FILE, DB_SCHEMA_VERSION,
    DOWNGRADE_BACKUP_FILE, DOWNGRADE_SIDECAR_FILE,
};

// ---------------------------------------------------------------------------
// Schema fixtures
// ---------------------------------------------------------------------------

/// The exact S5 (v3) schema: five S1/S3 tables plus the two event tables with
/// their v3 shape (`reuse_history` carries the table-level UNIQUE).
const V3_SCHEMA: &str = "
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

CREATE TABLE reuse_history (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    candidate_id TEXT NOT NULL REFERENCES candidates(candidate_id),
    task_id TEXT NOT NULL,
    outcome TEXT NOT NULL,
    notes TEXT NOT NULL,
    recorded_at TEXT NOT NULL,
    UNIQUE (candidate_id, task_id)
);

CREATE INDEX reuse_history_candidate_idx ON reuse_history(candidate_id, seq);

CREATE TABLE validation_history (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    candidate_id TEXT NOT NULL REFERENCES candidates(candidate_id),
    reviewer TEXT NOT NULL,
    verdict TEXT NOT NULL,
    report_ref TEXT NOT NULL,
    recorded_at TEXT NOT NULL
);

CREATE INDEX validation_history_candidate_idx ON validation_history(candidate_id, seq);
";

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

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

fn knowledge_candidate(claim: &str, checkpoint: &str) -> Value {
    json!({
        "schema_version": 1,
        "candidate_id": "",
        "revision": 1,
        "status": "candidate",
        "claim": claim,
        "target_kind": "rule",
        "proposed_target": "AGENTS.md#explicit-roots",
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
        "extractor": "extractor-a",
        "created_at": "2026-11-01T00:00:00Z",
        "updated_at": "2026-11-01T00:00:00Z"
    })
}

fn db_of(root: &Path) -> PathBuf {
    root.join(DB_FILE)
}

/// `learning init` now creates v5. The downgrade tests exercise the explicit
/// v4 -> v3 path, so their fixture is reduced to the exact v4 shape first
/// (drop the two stage-7 tables and set `user_version = 4`).
fn make_v4(root: &Path) {
    let db = db_of(root);
    let conn = Connection::open(&db).expect("open v5 db");
    conn.execute_batch(
        "DROP TABLE IF EXISTS qualification_history; \
         DROP TABLE IF EXISTS rejection_overrides;",
    )
    .expect("drop v5 tables");
    conn.pragma_update(None, "user_version", 4)
        .expect("user_version 4");
}

fn count(db: &Path, table: &str) -> i64 {
    let conn = Connection::open(db).expect("open learning.db");
    conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
        row.get(0)
    })
    .expect("count rows")
}

fn user_version(db: &Path) -> i64 {
    let conn = Connection::open(db).expect("open learning.db");
    conn.query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("user_version")
}

fn table_names(db: &Path) -> Vec<String> {
    let conn = Connection::open(db).expect("open learning.db");
    let mut stmt = conn
        .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
        .expect("prepare table list");
    stmt.query_map([], |row| row.get::<_, String>(0))
        .expect("query table list")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect table list")
}

fn already_has_table(db: &Path, table: &str) -> bool {
    table_names(db).iter().any(|name| name == table)
}

fn has_column(db: &Path, table: &str, column: &str) -> bool {
    let conn = Connection::open(db).expect("open learning.db");
    let present: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info(?1) WHERE name = ?2",
            rusqlite::params![table, column],
            |row| row.get(0),
        )
        .expect("pragma table_info");
    present > 0
}

/// Every row of one query, cell by cell, so two snapshots can be compared for
/// exact equality (values, not just counts).
fn table_rows(db: &Path, sql: &str) -> Vec<String> {
    let conn = Connection::open(db).expect("open learning.db");
    let mut stmt = conn.prepare(sql).expect("prepare row dump");
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
                SqlValue::Blob(bytes) => {
                    let mut hex = String::from("x'");
                    for byte in bytes {
                        hex.push_str(&format!("{byte:02x}"));
                    }
                    hex.push('\'');
                    hex
                }
            });
        }
        Ok(cells.join("\u{1f}"))
    })
    .expect("query row dump")
    .collect::<Result<Vec<_>, _>>()
    .expect("collect row dump")
}

fn immovable_snapshot(db: &Path) -> Vec<(&'static str, Vec<String>)> {
    vec![
        (
            "candidates",
            table_rows(
                db,
                "SELECT candidate_id, revision, status, record_json, payload_sha256, created_at, \
                 updated_at FROM candidates ORDER BY candidate_id",
            ),
        ),
        (
            "candidate_revisions",
            table_rows(
                db,
                "SELECT candidate_id, revision, status, record_json, payload_sha256, recorded_at \
                 FROM candidate_revisions ORDER BY candidate_id, revision",
            ),
        ),
        (
            "ingestions",
            table_rows(
                db,
                "SELECT ingest_key, candidate_id, revision, payload_sha256, extractor, \
                 source_checkpoint, created_at FROM ingestions ORDER BY ingest_key",
            ),
        ),
        (
            "status_history",
            table_rows(
                db,
                "SELECT seq, candidate_id, revision, status, recorded_at FROM status_history \
                 ORDER BY seq",
            ),
        ),
        (
            "dedup_history",
            table_rows(
                db,
                "SELECT seq, candidate_id, query, matches_json, disposition, reason, checked_at \
                 FROM dedup_history ORDER BY seq",
            ),
        ),
    ]
}

fn legacy_reuse_rows(db: &Path) -> Vec<String> {
    table_rows(
        db,
        "SELECT seq, candidate_id, task_id, outcome, notes, recorded_at FROM reuse_history \
         ORDER BY seq",
    )
}

fn legacy_validation_rows(db: &Path) -> Vec<String> {
    table_rows(
        db,
        "SELECT seq, candidate_id, reviewer, verdict, report_ref, recorded_at \
         FROM validation_history ORDER BY seq",
    )
}

/// Write a hand-built v3 database with two committed knowledge candidates and a
/// known event mix: three reuse rows and two validation rows whose
/// `recorded_at` values deliberately interleave, so the backfill has to use the
/// `(recorded_at, table_rank, seq)` order.
fn write_v3_database(root: &Path) -> (String, String) {
    std::fs::create_dir_all(root).expect("v3 root");
    let db = db_of(root);
    let conn = Connection::open(&db).expect("open v3 db");
    conn.execute_batch("PRAGMA foreign_keys=ON;")
        .expect("foreign keys");
    conn.execute_batch(V3_SCHEMA).expect("v3 schema");
    conn.pragma_update(None, "user_version", 3)
        .expect("user_version 3");

    let mut ids = Vec::new();
    for (claim, checkpoint) in [
        ("Prefer explicit roots for every store", "checkpoint-a"),
        ("Keep event order globally comparable", "checkpoint-b"),
    ] {
        let validated = validate_candidate(&knowledge_candidate(claim, checkpoint))
            .expect("valid v3 candidate");
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
        .expect("insert v3 candidate");
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
        .expect("insert v3 revision");
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
        .expect("insert v3 ingestion");
        conn.execute(
            "INSERT INTO status_history (candidate_id, revision, status, recorded_at) \
             VALUES (?1, ?2, 'candidate', '2026-11-01T00:00:00.000Z')",
            rusqlite::params![record.candidate_id, record.revision as i64],
        )
        .expect("insert v3 status");
        ids.push(record.candidate_id.clone());
    }

    // reuse seq 1 (a/task-1) recorded last; seq 2 (b/task-2) and seq 3
    // (a/task-3) share the same instant, so only seq can order them.
    for (candidate_id, task_id, recorded_at) in [
        (&ids[0], "task-1", "2026-11-01T00:00:03.000Z"),
        (&ids[1], "task-2", "2026-11-01T00:00:01.000Z"),
        (&ids[0], "task-3", "2026-11-01T00:00:01.000Z"),
    ] {
        conn.execute(
            "INSERT INTO reuse_history (candidate_id, task_id, outcome, notes, recorded_at) \
             VALUES (?1, ?2, 'pass', '', ?3)",
            rusqlite::params![candidate_id, task_id, recorded_at],
        )
        .expect("insert v3 reuse");
    }

    // validation seq 1 (a) at ...02; seq 2 (b) at ...01, i.e. the same instant
    // as the two reuse rows: table rank must place reuse first.
    for (candidate_id, recorded_at) in [
        (&ids[0], "2026-11-01T00:00:02.000Z"),
        (&ids[1], "2026-11-01T00:00:01.000Z"),
    ] {
        conn.execute(
            "INSERT INTO validation_history \
             (candidate_id, reviewer, verdict, report_ref, recorded_at) \
             VALUES (?1, 'reviewer-a', 'pass', 'report-1', ?2)",
            rusqlite::params![candidate_id, recorded_at],
        )
        .expect("insert v3 validation");
    }

    (ids[0].clone(), ids[1].clone())
}

/// A full v4-shaped candidate payload with `target_kind='task'`, inserted
/// directly because `validate_candidate` only accepts the six knowledge kinds
/// (task creation belongs to implementation-002). It is still a valid
/// `CandidateRecord` so `show`/`record_reuse` can read it back.
fn task_record_json(candidate_id: &str, claim: &str) -> String {
    serde_json::to_string(&json!({
        "schema_version": 1,
        "candidate_id": candidate_id,
        "revision": 1,
        "status": "candidate",
        "claim": claim,
        "target_kind": "task",
        "proposed_target": "task",
        "applicability": [],
        "counterexamples": [],
        "unknowns": [],
        "evidence": [],
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
        "source_checkpoint": format!("task:{candidate_id}"),
        "extractor": "task",
        "created_at": "2026-11-01T00:00:00Z",
        "updated_at": "2026-11-01T00:00:00Z"
    }))
    .expect("task record json")
}

fn insert_task_candidate(root: &Path, claim: &str) -> String {
    let candidate_id = candidate_id_for(claim, "task", "task");
    let record_json = task_record_json(&candidate_id, claim);
    let payload_sha256 = hex_sha256(record_json.as_bytes());
    let ingest_key = ingest_key_for("task", &format!("task:{candidate_id}"));
    let db = db_of(root);
    let conn = Connection::open(&db).expect("open v4 db");
    conn.execute_batch("PRAGMA foreign_keys=ON;")
        .expect("foreign keys");
    conn.execute(
        "INSERT INTO candidates \
         (candidate_id, revision, status, record_json, payload_sha256, created_at, updated_at) \
         VALUES (?1, 1, 'candidate', ?2, ?3, '2026-11-01T00:00:00Z', '2026-11-01T00:00:00Z')",
        rusqlite::params![candidate_id, record_json, payload_sha256],
    )
    .expect("insert task candidate");
    conn.execute(
        "INSERT INTO candidate_revisions \
         (candidate_id, revision, status, record_json, payload_sha256, recorded_at) \
         VALUES (?1, 1, 'candidate', ?2, ?3, '2026-11-01T00:00:00.000Z')",
        rusqlite::params![candidate_id, record_json, payload_sha256],
    )
    .expect("insert task revision");
    conn.execute(
        "INSERT INTO ingestions \
         (ingest_key, candidate_id, revision, payload_sha256, extractor, source_checkpoint, \
          created_at) VALUES (?1, ?2, 1, ?3, 'task', ?4, '2026-11-01T00:00:00.000Z')",
        rusqlite::params![
            ingest_key,
            candidate_id,
            payload_sha256,
            format!("task:{candidate_id}")
        ],
    )
    .expect("insert task ingestion");
    conn.execute(
        "INSERT INTO status_history (candidate_id, revision, status, recorded_at) \
         VALUES (?1, 1, 'candidate', '2026-11-01T00:00:00.000Z')",
        rusqlite::params![candidate_id],
    )
    .expect("insert task status");
    candidate_id
}

fn insert_task_event(root: &Path, candidate_id: &str) {
    let db = db_of(root);
    let conn = Connection::open(&db).expect("open v4 db");
    let event_seq: i64 = conn
        .query_row(
            "SELECT COALESCE(MAX(seq), 0) + 1 FROM event_clock",
            [],
            |row| row.get(0),
        )
        .expect("next clock seq");
    conn.execute(
        "INSERT INTO event_clock (seq) VALUES (?1)",
        rusqlite::params![event_seq],
    )
    .expect("clock row");
    conn.execute(
        "INSERT INTO task_events (event_seq, candidate_id, event_kind, actor_id, note, recorded_at) \
         VALUES (?1, ?2, 'claim', 'migrated:v3', '', '2026-11-01T00:00:04.000Z')",
        rusqlite::params![event_seq, candidate_id],
    )
    .expect("insert task event");
}

fn ingest(store: &mut LearningStore, claim: &str, checkpoint: &str) -> String {
    store
        .ingest(&knowledge_candidate(claim, checkpoint))
        .expect("ingest")
        .candidate_id
}

fn reuse(store: &mut LearningStore, candidate_id: &str, task_id: &str) {
    let request = ReuseRequest::parse(candidate_id, task_id, "pass", None, "test:actor")
        .expect("reuse request");
    store.record_reuse(&request).expect("record reuse");
}

fn validate(store: &mut LearningStore, candidate_id: &str) {
    let request = ValidationRequest::parse(candidate_id, "reviewer-a", "pass", "report-1", None)
        .expect("validation request");
    store
        .record_validation(&request)
        .expect("record validation");
}

fn event_seq_of(db: &Path, table: &str, seq: i64) -> i64 {
    let conn = Connection::open(db).expect("open learning.db");
    conn.query_row(
        &format!("SELECT event_seq FROM {table} WHERE seq = ?1"),
        rusqlite::params![seq],
        |row| row.get(0),
    )
    .expect("event_seq lookup")
}

fn last_event_seq(db: &Path, table: &str) -> i64 {
    let conn = Connection::open(db).expect("open learning.db");
    conn.query_row(
        &format!("SELECT event_seq FROM {table} ORDER BY seq DESC LIMIT 1"),
        [],
        |row| row.get(0),
    )
    .expect("last event_seq")
}

/// Count stored candidates whose payload says `target_kind='task'` (Rust-side
/// JSON parse, never JSON1 — the same rule the downgrade follows).
fn count_task_candidates(db: &Path) -> usize {
    let conn = Connection::open(db).expect("open learning.db");
    let mut stmt = conn
        .prepare("SELECT record_json FROM candidates")
        .expect("prepare candidate list");
    let rows = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .expect("query candidate list")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect candidate list");
    rows.iter()
        .filter(|json| {
            serde_json::from_str::<Value>(json)
                .ok()
                .and_then(|value| {
                    value
                        .get("target_kind")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                })
                .as_deref()
                == Some("task")
        })
        .count()
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

// ---------------------------------------------------------------------------
// A1: v3 -> v4 forward migration
// ---------------------------------------------------------------------------

#[test]
fn v3_database_migrates_to_v4_with_ordered_backfill_and_clock_continuity() {
    let dir = tempfile::tempdir().expect("temp root");
    let root = dir.path().join("learning");
    let (candidate_a, candidate_b) = write_v3_database(&root);
    let db = db_of(&root);

    // Snapshot the five physically untouched tables before opening.
    let immovable_before = immovable_snapshot(&db);
    let reuse_before = legacy_reuse_rows(&db);
    let validation_before = legacy_validation_rows(&db);
    assert_eq!(reuse_before.len(), 3);
    assert_eq!(validation_before.len(), 2);

    let mut store = LearningStore::open(&root).expect("v3 migrates to v4");

    assert_eq!(user_version(&db), i64::from(DB_SCHEMA_VERSION));
    assert!(already_has_table(&db, "event_clock"));
    assert!(already_has_table(&db, "task_events"));
    assert!(has_column(&db, "reuse_history", "event_seq"));
    assert!(has_column(&db, "reuse_history", "actor_id"));
    assert!(has_column(&db, "reuse_history", "client_seq"));
    assert!(has_column(&db, "validation_history", "event_seq"));

    // A1: the five immovable tables are byte-for-byte equal, row for row.
    assert_eq!(
        immovable_before,
        immovable_snapshot(&db),
        "candidates/revisions/ingestions/status/dedup must not be rewritten"
    );

    // The legacy columns of both event tables survive in `seq` order.
    assert_eq!(legacy_reuse_rows(&db), reuse_before);
    assert_eq!(legacy_validation_rows(&db), validation_before);

    // Backfill order: (recorded_at, table_rank, seq) with reuse=1,
    // validation=2:
    //   reuse seq 2 (00:00:01) -> 1
    //   reuse seq 3 (00:00:01) -> 2
    //   validation seq 2 (00:00:01) -> 3
    //   validation seq 1 (00:00:02) -> 4
    //   reuse seq 1 (00:00:03) -> 5
    assert_eq!(event_seq_of(&db, "reuse_history", 2), 1);
    assert_eq!(event_seq_of(&db, "reuse_history", 3), 2);
    assert_eq!(event_seq_of(&db, "validation_history", 2), 3);
    assert_eq!(event_seq_of(&db, "validation_history", 1), 4);
    assert_eq!(event_seq_of(&db, "reuse_history", 1), 5);

    // v3 attribution: one known actor and the original table seq as client seq.
    let conn = Connection::open(&db).expect("open migrated db");
    for seq in [1_i64, 2, 3] {
        let (actor_id, client_seq): (String, i64) = conn
            .query_row(
                "SELECT actor_id, client_seq FROM reuse_history WHERE seq = ?1",
                rusqlite::params![seq],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("reuse attribution");
        assert_eq!(actor_id, "migrated:v3");
        assert_eq!(client_seq, seq);
    }
    drop(conn);

    // event_clock is advanced past every backfilled event (n = 5).
    assert_eq!(count(&db, "event_clock"), 5);
    let clock_max: i64 = {
        let conn = Connection::open(&db).expect("open migrated db");
        conn.query_row("SELECT MAX(seq) FROM event_clock", [], |row| row.get(0))
            .expect("clock max")
    };
    assert_eq!(clock_max, 5);

    // The first post-migration event continues at n + 1, cross-table.
    validate(&mut store, &candidate_a);
    assert_eq!(last_event_seq(&db, "validation_history"), 6);
    reuse(&mut store, &candidate_b, "task-after-migration");
    assert_eq!(last_event_seq(&db, "reuse_history"), 7);
    drop(store);

    // Re-opening a migrated v4 store is a no-op: no double backfill, no new
    // clock rows, no rewritten evidence.
    let reopen = LearningStore::open(&root).expect("reopen v4");
    assert_eq!(user_version(&db), i64::from(DB_SCHEMA_VERSION));
    assert_eq!(count(&db, "event_clock"), 7);
    assert_eq!(count(&db, "reuse_history"), 4);
    assert_eq!(count(&db, "validation_history"), 3);
    assert_eq!(event_seq_of(&db, "reuse_history", 1), 5);
    drop(reopen);
}

#[test]
fn cross_table_event_seq_is_monotonic_in_write_order() {
    let dir = tempfile::tempdir().expect("temp root");
    let root = dir.path().join("learning");
    let (candidate_a, candidate_b) = write_v3_database(&root);
    let db = db_of(&root);

    let mut store = LearningStore::open(&root).expect("migrate v3 -> v4");
    // Backfill occupied event_seq 1..=5.
    reuse(&mut store, &candidate_a, "task-mono-1");
    let first = last_event_seq(&db, "reuse_history");
    validate(&mut store, &candidate_b);
    let validation = last_event_seq(&db, "validation_history");
    reuse(&mut store, &candidate_b, "task-mono-2");
    let conn = Connection::open(&db).expect("open db");
    let second: i64 = conn
        .query_row(
            "SELECT event_seq FROM reuse_history WHERE task_id = 'task-mono-2'",
            [],
            |row| row.get(0),
        )
        .expect("second reuse seq");
    assert_eq!(first, 6);
    assert_eq!(validation, 7);
    assert_eq!(second, 8);
    assert!(first < validation && validation < second);

    // event_clock is the single source: every allocated event has a distinct
    // clock row at the same value.
    let allocated: i64 = conn
        .query_row("SELECT COUNT(*) FROM event_clock", [], |row| row.get(0))
        .expect("clock count");
    assert_eq!(allocated, 8);
    drop(conn);
    drop(store);
}

#[test]
fn damaged_v3_database_is_rejected_without_migrating() {
    let dir = tempfile::tempdir().expect("temp root");
    let root = dir.path().join("learning");
    std::fs::create_dir_all(&root).expect("damaged root");
    let db = db_of(&root);
    {
        // A v3 database missing `validation_history`: PROBES_V3 must fire before
        // the v3 -> v4 migration writes anything.
        let conn = Connection::open(&db).expect("open damaged db");
        conn.execute_batch(
            "CREATE TABLE candidates (
                 candidate_id TEXT PRIMARY KEY NOT NULL,
                 revision INTEGER NOT NULL,
                 status TEXT NOT NULL,
                 record_json TEXT NOT NULL,
                 payload_sha256 TEXT NOT NULL,
                 created_at TEXT NOT NULL,
                 updated_at TEXT NOT NULL
             );
             CREATE TABLE candidate_revisions (
                 candidate_id TEXT NOT NULL, revision INTEGER NOT NULL, status TEXT NOT NULL,
                 record_json TEXT NOT NULL, payload_sha256 TEXT NOT NULL, recorded_at TEXT NOT NULL,
                 PRIMARY KEY (candidate_id, revision)
             );
             CREATE TABLE ingestions (
                 ingest_key TEXT PRIMARY KEY NOT NULL, candidate_id TEXT NOT NULL,
                 revision INTEGER NOT NULL, payload_sha256 TEXT NOT NULL, extractor TEXT NOT NULL,
                 source_checkpoint TEXT NOT NULL, created_at TEXT NOT NULL
             );
             CREATE TABLE status_history (
                 seq INTEGER PRIMARY KEY AUTOINCREMENT, candidate_id TEXT NOT NULL,
                 revision INTEGER NOT NULL, status TEXT NOT NULL, recorded_at TEXT NOT NULL
             );
             CREATE TABLE dedup_history (
                 seq INTEGER PRIMARY KEY AUTOINCREMENT, candidate_id TEXT NOT NULL,
                 query TEXT NOT NULL, matches_json TEXT NOT NULL, disposition TEXT NOT NULL,
                 reason TEXT NOT NULL, checked_at TEXT NOT NULL
             );
             CREATE TABLE reuse_history (
                 seq INTEGER PRIMARY KEY AUTOINCREMENT, candidate_id TEXT NOT NULL,
                 task_id TEXT NOT NULL, outcome TEXT NOT NULL, notes TEXT NOT NULL,
                 recorded_at TEXT NOT NULL, UNIQUE (candidate_id, task_id)
             );",
        )
        .expect("damaged v3 schema");
        conn.pragma_update(None, "user_version", 3)
            .expect("user_version 3");
    }

    let error = LearningStore::open(&root).expect_err("probe must reject the damaged v3 db");
    assert_eq!(error.code, "LEARNING_SCHEMA_INVALID");
    assert_eq!(error.exit_code(), 6);
    assert_eq!(
        user_version(&db),
        3,
        "a failed probe never bumps the version"
    );
    assert!(!already_has_table(&db, "event_clock"));
    assert!(!already_has_table(&db, "task_events"));
    assert!(!has_column(&db, "reuse_history", "event_seq"));
}

// ---------------------------------------------------------------------------
// A9: v4 -> v3 downgrade
// ---------------------------------------------------------------------------

#[test]
fn migrate_to_3_snapshots_and_drops_only_task_rows() {
    let dir = tempfile::tempdir().expect("temp root");
    let root = dir.path().join("learning");
    let db = db_of(&root);
    let mut store = LearningStore::init(&root).expect("init v4");

    let knowledge_a = ingest(&mut store, "Keep knowledge rows on downgrade", "cp-a");
    let knowledge_b = ingest(&mut store, "Keep validation rows on downgrade", "cp-b");
    drop(store);
    let task_id = insert_task_candidate(&root, "Finish the stage four migration");
    insert_task_event(&root, &task_id);

    let mut store = LearningStore::open(&root).expect("open v4");
    reuse(&mut store, &knowledge_a, "task-kept");
    reuse(&mut store, &knowledge_a, &task_id); // orphaned by the task deletion
    reuse(&mut store, &task_id, &task_id);
    validate(&mut store, &knowledge_a);
    validate(&mut store, &task_id);
    drop(store);
    make_v4(&root);

    let knowledge_reuse_before: Vec<String> = table_rows(
        &db,
        &format!(
            "SELECT seq, candidate_id, task_id, outcome, notes, recorded_at FROM reuse_history \
             WHERE candidate_id = '{knowledge_a}' AND task_id = 'task-kept' ORDER BY seq"
        ),
    );
    let knowledge_validation_before: Vec<String> = table_rows(
        &db,
        &format!(
            "SELECT seq, candidate_id, reviewer, verdict, report_ref, recorded_at \
             FROM validation_history WHERE candidate_id = '{knowledge_a}' ORDER BY seq"
        ),
    );
    assert_eq!(knowledge_reuse_before.len(), 1);
    assert_eq!(knowledge_validation_before.len(), 1);

    let report = LearningStore::downgrade_to_v3(&root).expect("downgrade to v3");
    assert!(report.ok);
    assert_eq!(report.from_version, 4);
    assert_eq!(report.to_version, 3);
    assert_eq!(report.dropped.candidates, 1);
    assert_eq!(report.dropped.candidate_revisions, 1);
    assert_eq!(report.dropped.ingestions, 1);
    assert_eq!(report.dropped.status_history, 1);
    assert_eq!(report.dropped.dedup_history, 0);
    assert_eq!(report.dropped.task_events, 1);
    assert_eq!(report.dropped.reuse, 2);
    assert_eq!(report.dropped.validation, 1);
    assert_eq!(report.dropped.orphan_task_refs, 1);
    assert_eq!(report.kept.candidates, 2);
    assert_eq!(report.kept.reuse, 1);
    assert_eq!(report.kept.validation, 1);
    assert_eq!(report.kept.event_seq_max, 6);

    // Backup + sidecar exist and the backup is a usable v4 snapshot.
    let backup = root.join(DOWNGRADE_BACKUP_FILE);
    let sidecar = root.join(DOWNGRADE_SIDECAR_FILE);
    assert!(backup.is_file(), "backup must exist: {}", backup.display());
    assert!(
        sidecar.is_file(),
        "sidecar must exist: {}",
        sidecar.display()
    );
    assert_eq!(user_version(&backup), 4);
    assert_eq!(
        count(&backup, "candidates"),
        3,
        "the backup keeps task rows"
    );
    let meta: Value =
        serde_json::from_slice(&std::fs::read(&sidecar).expect("read sidecar")).expect("json");
    assert_eq!(meta["ok"], true);
    assert_eq!(meta["from_version"], 4);
    assert_eq!(meta["source"]["user_version"], 4);
    assert_eq!(
        meta["backup"]["sha256"],
        hex_sha256(&std::fs::read(&backup).expect("read backup"))
    );

    // The live database is an exact v3 store.
    assert_eq!(user_version(&db), 3);
    assert!(!already_has_table(&db, "task_events"));
    assert!(!already_has_table(&db, "event_clock"));
    assert!(!has_column(&db, "reuse_history", "event_seq"));
    assert!(!has_column(&db, "reuse_history", "actor_id"));
    assert!(!has_column(&db, "reuse_history", "client_seq"));
    assert!(!has_column(&db, "validation_history", "event_seq"));
    assert_eq!(count(&db, "candidates"), 2);
    assert_eq!(
        count_task_candidates(&db),
        0,
        "no task candidate survives the downgrade"
    );

    // Knowledge rows keep their seq and every legacy column.
    assert_eq!(
        table_rows(
            &db,
            &format!(
                "SELECT seq, candidate_id, task_id, outcome, notes, recorded_at FROM reuse_history \
                 WHERE candidate_id = '{knowledge_a}' ORDER BY seq"
            )
        ),
        knowledge_reuse_before
    );
    assert_eq!(
        table_rows(
            &db,
            &format!(
                "SELECT seq, candidate_id, reviewer, verdict, report_ref, recorded_at \
                 FROM validation_history WHERE candidate_id = '{knowledge_a}' ORDER BY seq"
            )
        ),
        knowledge_validation_before
    );

    // The v3 table-level UNIQUE is back, so S5's one-task-per-candidate check is
    // enforced by SQL again (and the v4 idempotency index is gone).
    let conn = Connection::open(&db).expect("open downgraded db");
    let duplicate = conn.execute(
        "INSERT INTO reuse_history (candidate_id, task_id, outcome, notes, recorded_at) \
         VALUES (?1, 'task-kept', 'fail', '', '2026-11-01T00:00:09.000Z')",
        rusqlite::params![knowledge_a],
    );
    assert!(
        matches!(
            duplicate,
            Err(rusqlite::Error::SqliteFailure(ref failure, _))
                if failure.code == rusqlite::ErrorCode::ConstraintViolation
        ),
        "the restored v3 UNIQUE(candidate_id, task_id) must reject a duplicate: {duplicate:?}"
    );
    let indexes: Vec<String> = {
        let mut stmt = conn
            .prepare(
                "SELECT name FROM sqlite_master WHERE type='index' AND tbl_name='reuse_history'",
            )
            .expect("index list");
        stmt.query_map([], |row| row.get::<_, String>(0))
            .expect("query indexes")
            .collect::<Result<Vec<_>, _>>()
            .expect("collect indexes")
    };
    assert!(!indexes.contains(&"reuse_history_idem_idx".to_string()));
    assert!(indexes.contains(&"reuse_history_candidate_idx".to_string()));

    // AUTOINCREMENT continuity: the next insert continues past every restored
    // seq instead of reusing a rowid.
    let max_seq: i64 = conn
        .query_row("SELECT MAX(seq) FROM reuse_history", [], |row| row.get(0))
        .expect("max seq");
    conn.execute(
        "INSERT INTO reuse_history (candidate_id, task_id, outcome, notes, recorded_at) \
         VALUES (?1, 'task-after-downgrade', 'pass', '', '2026-11-01T00:00:10.000Z')",
        rusqlite::params![knowledge_b],
    )
    .expect("post-downgrade insert");
    let new_seq = conn.last_insert_rowid();
    assert!(new_seq > max_seq, "seq {new_seq} must exceed {max_seq}");
    assert!(report.kept.event_seq_max >= 5);
}

#[test]
fn migrate_to_3_refuses_an_existing_backup_without_touching_it() {
    let dir = tempfile::tempdir().expect("temp root");
    let root = dir.path().join("learning");
    let db = db_of(&root);
    let mut store = LearningStore::init(&root).expect("init v4");
    ingest(&mut store, "Keep the existing backup", "cp-keep");
    drop(store);
    let task_id = insert_task_candidate(&root, "A task that must not be dropped");
    insert_task_event(&root, &task_id);
    make_v4(&root);

    let backup = root.join(DOWNGRADE_BACKUP_FILE);
    let sentinel = b"sentinel backup bytes, not a sqlite file";
    std::fs::write(&backup, sentinel).expect("write sentinel backup");

    let error = LearningStore::downgrade_to_v3(&root).expect_err("backup exists");
    assert_eq!(error.code, "MIGRATION_BACKUP_EXISTS");
    assert_ne!(error.exit_code(), 0);
    assert_eq!(std::fs::read(&backup).expect("read backup"), sentinel);
    assert!(!root.join(DOWNGRADE_SIDECAR_FILE).exists());
    assert_eq!(user_version(&db), 4);
    assert_eq!(count(&db, "candidates"), 2, "nothing was deleted");
    assert_eq!(count(&db, "task_events"), 1);
}

#[test]
fn migrate_to_3_refuses_a_v3_store_as_invalid_usage() {
    let dir = tempfile::tempdir().expect("temp root");
    let root = dir.path().join("learning");
    write_v3_database(&root);
    let db = db_of(&root);

    let error = LearningStore::downgrade_to_v3(&root).expect_err("already v3");
    assert_eq!(error.code, "INVALID_USAGE");
    assert_eq!(error.exit_code(), 2);
    assert_eq!(user_version(&db), 3);
    assert!(!root.join(DOWNGRADE_BACKUP_FILE).exists());

    // CLI surface: `learning migrate --to 3` on a v3 root is a usage error and
    // writes nothing.
    let root_text = root.to_string_lossy().to_string();
    let out = cli()
        .args(["learning", "migrate", "--root", &root_text, "--to", "3"])
        .output()
        .expect("cli migrate on v3");
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(stderr_json(&out)["error"]["code"], "INVALID_USAGE");
    assert!(!root.join(DOWNGRADE_BACKUP_FILE).exists());
    assert_eq!(user_version(&db), 3);
}

#[test]
fn migrate_to_3_precheck_refuses_duplicate_knowledge_rows_and_rolls_back() {
    let dir = tempfile::tempdir().expect("temp root");
    let root = dir.path().join("learning");
    let db = db_of(&root);
    let mut store = LearningStore::init(&root).expect("init v4");
    let knowledge = ingest(
        &mut store,
        "Two rows for the same task are a v4 defect",
        "cp-dup",
    );
    drop(store);
    let task_id = insert_task_candidate(&root, "A task that must survive the rollback");
    insert_task_event(&root, &task_id);
    make_v4(&root);

    // Inject the defect the v4 idempotency index does not forbid: same
    // (candidate_id, task_id) under two different actors.
    {
        let conn = Connection::open(&db).expect("open db");
        for (actor, client_seq) in [("actor-a", 1_i64), ("actor-b", 1_i64)] {
            let event_seq: i64 = conn
                .query_row(
                    "SELECT COALESCE(MAX(seq), 0) + 1 FROM event_clock",
                    [],
                    |row| row.get(0),
                )
                .expect("clock seq");
            conn.execute(
                "INSERT INTO event_clock (seq) VALUES (?1)",
                rusqlite::params![event_seq],
            )
            .expect("clock row");
            conn.execute(
                "INSERT INTO reuse_history \
                 (event_seq, candidate_id, task_id, outcome, notes, recorded_at, actor_id, \
                  client_seq) VALUES (?1, ?2, 'task-dup', 'pass', '', \
                  '2026-11-01T00:00:05.000Z', ?3, ?4)",
                rusqlite::params![event_seq, knowledge, actor, client_seq],
            )
            .expect("duplicate reuse row");
        }
    }
    let candidates_before = count(&db, "candidates");
    let reuse_before = count(&db, "reuse_history");
    let task_events_before = count(&db, "task_events");

    let error = LearningStore::downgrade_to_v3(&root).expect_err("precheck must refuse");
    assert_eq!(error.code, "MIGRATION_PRECHECK_FAILED");
    assert_ne!(error.exit_code(), 0);

    // The write transaction rolled back: no task row, no event row and no backup
    // content was rewritten past the pre-transaction snapshot.
    assert_eq!(user_version(&db), 4);
    assert_eq!(count(&db, "candidates"), candidates_before);
    assert_eq!(count(&db, "reuse_history"), reuse_before);
    assert_eq!(count(&db, "task_events"), task_events_before);
    assert!(already_has_table(&db, "event_clock"));
    assert!(root.join(DOWNGRADE_BACKUP_FILE).is_file());
}

#[test]
fn downgrade_then_open_round_trips_back_to_v5() {
    let dir = tempfile::tempdir().expect("temp root");
    let root = dir.path().join("learning");
    let db = db_of(&root);
    let mut store = LearningStore::init(&root).expect("init v4");
    let knowledge = ingest(&mut store, "Round trip a knowledge candidate", "cp-round");
    reuse(&mut store, &knowledge, "task-round-1");
    validate(&mut store, &knowledge);
    drop(store);
    let task_id = insert_task_candidate(&root, "A task lost by the downgrade");
    insert_task_event(&root, &task_id);
    make_v4(&root);

    LearningStore::downgrade_to_v3(&root).expect("downgrade");
    assert_eq!(count(&db, "candidates"), 1);

    // Opening the v3 store migrates it forward again; the knowledge trunk is
    // intact and the dropped actor columns come back as the v3 attribution.
    let mut store = LearningStore::open(&root).expect("re-open migrates v3 -> v4");
    assert_eq!(user_version(&db), i64::from(DB_SCHEMA_VERSION));
    assert_eq!(count(&db, "candidates"), 1);
    assert_eq!(count(&db, "reuse_history"), 1);
    assert_eq!(count(&db, "validation_history"), 1);
    assert_eq!(event_seq_of(&db, "reuse_history", 1), 1);
    assert_eq!(event_seq_of(&db, "validation_history", 1), 2);
    assert_eq!(count(&db, "event_clock"), 2);
    let conn = Connection::open(&db).expect("open db");
    let actor: String = conn
        .query_row(
            "SELECT actor_id FROM reuse_history WHERE seq = 1",
            [],
            |row| row.get(0),
        )
        .expect("actor after round trip");
    assert_eq!(actor, "migrated:v3");

    // The migrated store still writes events, continuing the clock at 3.
    reuse(&mut store, &knowledge, "task-round-2");
    assert_eq!(last_event_seq(&db, "reuse_history"), 3);
    assert_eq!(count(&db, "event_clock"), 3);
}

#[test]
fn cli_migrate_to_3_reports_json_and_rejects_other_targets() {
    let dir = tempfile::tempdir().expect("temp root");
    let root = dir.path().join("learning");
    let root_text = root.to_string_lossy().to_string();
    {
        let mut store = LearningStore::init(&root).expect("init v4");
        ingest(
            &mut store,
            "A knowledge candidate for the CLI downgrade",
            "cp-cli",
        );
    }
    let task_id = insert_task_candidate(&root, "A task for the CLI downgrade");
    insert_task_event(&root, &task_id);
    make_v4(&root);

    let wrong_target = cli()
        .args(["learning", "migrate", "--root", &root_text, "--to", "4"])
        .output()
        .expect("cli migrate --to 4");
    assert_eq!(wrong_target.status.code(), Some(2));
    assert_eq!(stderr_json(&wrong_target)["error"]["code"], "INVALID_USAGE");
    assert_eq!(user_version(&db_of(&root)), 4);

    let out = cli()
        .args(["learning", "migrate", "--root", &root_text, "--to", "3"])
        .output()
        .expect("cli migrate --to 3");
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let report = stdout_json(&out);
    assert_eq!(report["ok"], true);
    assert_eq!(report["from_version"], 4);
    assert_eq!(report["to_version"], 3);
    assert_eq!(report["dropped"]["candidates"], 1);
    assert_eq!(report["dropped"]["task_events"], 1);
    assert_eq!(report["dropped"]["orphan_task_refs"], 0);
    assert_eq!(report["kept"]["candidates"], 1);
    assert_eq!(
        report["backup"].as_str(),
        Some(root.join(DOWNGRADE_BACKUP_FILE).to_string_lossy().as_ref())
    );
    assert!(root.join(DOWNGRADE_BACKUP_FILE).is_file());
    assert!(root.join(DOWNGRADE_SIDECAR_FILE).is_file());

    // A second run is refused without touching the existing backup pair.
    let backup_before = std::fs::read(root.join(DOWNGRADE_BACKUP_FILE)).expect("read backup");
    let again = cli()
        .args(["learning", "migrate", "--root", &root_text, "--to", "3"])
        .output()
        .expect("cli migrate again");
    assert_eq!(again.status.code(), Some(2));
    assert_eq!(stderr_json(&again)["error"]["code"], "INVALID_USAGE");
    assert_eq!(
        std::fs::read(root.join(DOWNGRADE_BACKUP_FILE)).expect("read backup"),
        backup_before
    );
}

// ---------------------------------------------------------------------------
// The read paths keep working on a migrated store
// ---------------------------------------------------------------------------

#[test]
fn migrated_store_keeps_show_and_list_readable() {
    let dir = tempfile::tempdir().expect("temp root");
    let root = dir.path().join("learning");
    let (candidate_a, _candidate_b) = write_v3_database(&root);

    let mut store = LearningStore::open(&root).expect("migrate v3 -> v4");
    let view = store.show(&candidate_a).expect("show after migration");
    assert_eq!(view.candidate.candidate_id, candidate_a);
    assert_eq!(view.ingestions.len(), 1);
    assert_eq!(view.revisions.len(), 1);
    assert_eq!(view.status_history.len(), 1);
    assert_eq!(store.list(None).expect("list").count, 2);

    // `record_reuse` still refuses a task that the candidate already recorded
    // before the migration: v4 keeps S5's (candidate, task) contract in the
    // write transaction rather than in a table constraint.
    let duplicate =
        ReuseRequest::parse(&candidate_a, "task-1", "fail", None, "test:actor").expect("request");
    let error = store
        .record_reuse(&duplicate)
        .expect_err("pre-migration reuse row still blocks a repeat");
    assert_eq!(error.code, agent_memory::learning::REUSE_TASK_CONFLICT);

    // Different candidates may share one task (the S5 "multi candidate" case).
    let listed = store.list(None).expect("list");
    let candidate_b = listed
        .candidates
        .iter()
        .map(|summary| summary.candidate_id.clone())
        .find(|id| id != &candidate_a)
        .expect("second candidate");
    let shared =
        ReuseRequest::parse(&candidate_b, "task-1", "pass", None, "test:actor").expect("request");
    store
        .record_reuse(&shared)
        .expect("same task, other candidate");
    assert_eq!(count(&db_of(&root), "reuse_history"), 4);
}

#[test]
fn payload_sha_and_record_json_are_not_rewritten_by_the_migration() {
    let dir = tempfile::tempdir().expect("temp root");
    let root = dir.path().join("learning");
    let (candidate_a, _) = write_v3_database(&root);
    let db = db_of(&root);
    let before = {
        let conn = Connection::open(&db).expect("open v3 db");
        let record: (String, String) = conn
            .query_row(
                "SELECT record_json, payload_sha256 FROM candidates WHERE candidate_id = ?1",
                rusqlite::params![candidate_a],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("v3 record");
        record
    };

    let store = LearningStore::open(&root).expect("migrate");
    let after = {
        let view = store.show(&candidate_a).expect("show");
        (view.candidate.canonical_json(), view.payload_sha256)
    };
    assert_eq!(
        before, after,
        "the migration must not rewrite candidate payloads"
    );
    // The v3 payload still has the empty reuse/validation arrays: projections
    // read the event tables, never the payload arrays.
    let record: CandidateRecord = serde_json::from_str(&before.0).expect("record");
    assert!(record.reuse.is_empty());
    assert!(record.validation.is_empty());
}
