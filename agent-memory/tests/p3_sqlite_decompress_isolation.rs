//! N4 acceptance: decompression-cost isolation for incremental capture.
//! Own binary on purpose: the decompression metric is process-global, so a
//! concurrent test in a shared binary would pollute the counter.

use std::collections::BTreeMap;
use std::path::Path;

use agent_memory::archive::Archive;
use agent_memory::capture::capture_source;
use agent_memory::capture_sqlite::replay_sqlite;
use rusqlite::Connection;
use serde_json::Value;

const DOMAIN: &str = "default";

fn v1_fixture(path: &Path) {
    let conn = Connection::open(path).unwrap();
    conn.execute_batch(
        r#"
        CREATE TABLE session (
            id TEXT PRIMARY KEY, title TEXT, parent_id TEXT,
            time_created INTEGER, time_updated INTEGER
        );
        CREATE TABLE message (
            id TEXT PRIMARY KEY, session_id TEXT,
            time_created INTEGER, time_updated INTEGER, data TEXT
        );
        CREATE TABLE part (
            id TEXT PRIMARY KEY, message_id TEXT, session_id TEXT,
            time_created INTEGER, time_updated INTEGER, data TEXT
        );
        CREATE TABLE project (id TEXT PRIMARY KEY, directory TEXT);
        CREATE TABLE todo (id TEXT PRIMARY KEY, session_id TEXT, content TEXT);
        INSERT INTO session VALUES ('ses_1', 'synthetic', NULL, 1, 2);
        INSERT INTO message VALUES ('msg_1', 'ses_1', 10, 10, '{"role":"user"}');
        INSERT INTO project VALUES ('proj_1', 'C:/synthetic/work');
        "#,
    )
    .unwrap();
    for index in 1..=24 {
        conn.execute(
            "INSERT INTO part VALUES (?1, 'msg_1', 'ses_1', ?2, ?2, ?3)",
            rusqlite::params![
                format!("prt_{index:02}"),
                100 + index,
                serde_json::json!({"type":"text","text":format!("part body {index}")}).to_string()
            ],
        )
        .unwrap();
    }
}

fn exec(path: &Path, sql: &str) {
    Connection::open(path).unwrap().execute_batch(sql).unwrap();
}

fn rows_by_table(path: &Path) -> BTreeMap<String, BTreeMap<String, Value>> {
    agent_memory::capture_sqlite::observe_database(path).unwrap().tables
}

fn make_root() -> (tempfile::TempDir, Archive) {
    let temp = tempfile::tempdir().unwrap();
    let archive = Archive::init(temp.path()).unwrap();
    (temp, archive)
}

fn capture(archive: &Archive, src: &Path) -> agent_memory::capture::CaptureReport {
    capture_source(archive, src, "opencode", DOMAIN, None).unwrap()
}

fn source_key(archive: &Archive) -> String {
    let sources = archive.load_sources().unwrap();
    assert_eq!(sources.sources.len(), 1);
    sources.sources.keys().next().unwrap().clone()
}

#[test]
fn observations_without_before_images_decompress_no_evidence() {
    use agent_memory::capture_sqlite::{
        evidence_decompressed_bytes, reset_evidence_decompressed_bytes,
    };
    // This test lives in its own binary: the decompression metric is a
    // process-global counter and must not be polluted by concurrent tests.
    let (temp, archive) = make_root();
    let db = temp.path().join("opencode.db");
    v1_fixture(&db);
    let baseline = capture(&archive, &db);
    let baseline_bytes = baseline.sqlite.as_ref().unwrap().baseline_bytes;

    // No change: nothing in the evidence store may be decompressed.
    reset_evidence_decompressed_bytes();
    let unchanged = capture(&archive, &db);
    assert_eq!(unchanged.change, "skipped_unchanged");
    assert_eq!(
        evidence_decompressed_bytes(),
        0,
        "an unchanged observation must not read evidence content"
    );

    // Insert-only: no before-image lookups, still zero decompression.
    exec(
        &db,
        "INSERT INTO part VALUES ('prt_40','msg_1','ses_1',12,400,'{\"text\":\"b\"}');",
    );
    reset_evidence_decompressed_bytes();
    let insert = capture(&archive, &db);
    assert_eq!(insert.change, "delta");
    assert_eq!(
        evidence_decompressed_bytes(),
        0,
        "an insert-only delta must not read evidence content"
    );

    // One content update: the before-lookup touches only the chunk(s) whose
    // key range contains the key — bounded, far below the baseline size.
    exec(
        &db,
        "UPDATE part SET data = '{\"text\":\"edited\"}', time_updated = 401 WHERE id = 'prt_01';",
    );
    reset_evidence_decompressed_bytes();
    let update = capture(&archive, &db);
    assert_eq!(update.change, "delta");
    let decompressed = evidence_decompressed_bytes();
    // The fixture holds a single part chunk, so the touched-chunk bound is
    // effectively the whole table here; the multi-chunk bound (far below the
    // baseline total) is asserted in the scale test.
    assert!(
        decompressed > 0 && decompressed <= baseline_bytes,
        "before-lookup decompressed {decompressed} B, baseline is {baseline_bytes} B"
    );

    // After a delta exists, an update of an unrelated key must not read the
    // whole chain either: the batch ranges are checked before decompression.
    exec(
        &db,
        "UPDATE part SET data = '{\"text\":\"edited2\"}', time_updated = 402 WHERE id = 'prt_02';",
    );
    reset_evidence_decompressed_bytes();
    let update2 = capture(&archive, &db);
    assert_eq!(update2.change, "delta");
    // Chunk (touched key) + the one previous delta batch (touched range):
    // still bounded by the whole store, not the store *plus* history.
    assert!(
        evidence_decompressed_bytes() <= baseline_bytes,
        "chain batches must be range-checked"
    );
    assert_eq!(
        replay_sqlite(&archive, &source_key(&archive), None, DOMAIN)
            .unwrap()
            .tables,
        rows_by_table(&db)
    );
}
