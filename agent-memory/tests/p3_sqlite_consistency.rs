//! P3 acceptance tests (stage-2 S1): the SQLite evidence model is a **chunked
//! whitelist baseline + watermark delta** chain.
//!
//! Every database and file in this file is synthetic and lives under a
//! `tempfile` root. No test points at a real OpenCode (or any other real
//! session) directory, and no real session data is read.
//!
//! Fixture shapes follow the real OpenCode schema columns
//! (`part(id, message_id, session_id, time_created, time_updated, data)`,
//! `message(id, session_id, time_created, time_updated, data)`,
//! `session_message(id, session_id, type, seq, time_created, time_updated,
//! data)`) so the watermark strategy is exercised on realistic columns.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::panic::AssertUnwindSafe;
use std::path::Path;
use std::process::Command;

use agent_memory::archive::{Archive, ArchiveError, SqliteState};
use agent_memory::capture::{
    capture_source, capture_source_with_options, CaptureOptions, CaptureReport, Failpoint,
};
use agent_memory::capture_sqlite::{
    observe_database, read_baseline_rows, replay_sqlite, set_lock_timeout_for_tests,
    sqlite_baselines, sqlite_deltas,
};
use rusqlite::Connection;
use serde_json::{json, Value};

const DOMAIN: &str = "default";

// ---------------------------------------------------------------------------
// Synthetic OpenCode fixtures (shape only; content is invented)
// ---------------------------------------------------------------------------

/// V1 generation: `session`/`message`/`part` plus the whitelisted extras
/// (`project`, `todo`). 24 parts so several small deltas stay below the
/// 50%-of-baseline chain threshold.
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
        CREATE TABLE project (
            id TEXT PRIMARY KEY, directory TEXT
        );
        CREATE TABLE todo (
            id TEXT PRIMARY KEY, session_id TEXT, content TEXT
        );
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
                json!({"type":"text","text":format!("part body {index}")}).to_string()
            ],
        )
        .unwrap();
    }
}

/// V2 generation with a *real* foreign key so replay materialization is
/// exercised against enforced references.
fn v2_fixture(path: &Path) {
    let conn = Connection::open(path).unwrap();
    conn.execute_batch(
        r#"
        CREATE TABLE session_v2 (
            id TEXT PRIMARY KEY, title TEXT, parent_id TEXT, directory TEXT,
            time_created INTEGER, time_updated INTEGER
        );
        CREATE TABLE session_message (
            id TEXT PRIMARY KEY, session_id TEXT, seq INTEGER, type TEXT,
            time_created INTEGER, time_updated INTEGER, data TEXT,
            FOREIGN KEY (session_id) REFERENCES session_v2(id) ON DELETE CASCADE
        );
        INSERT INTO session_v2 VALUES ('ses_v2', 'synthetic-v2', NULL, 'C:/w', 1, 2);
        INSERT INTO session_message VALUES
            ('sm_1', 'ses_v2', 1, 'user', 10, 10, '{"text":"hello"}');
        "#,
    )
    .unwrap();
}

/// V1 and V2 tables coexisting (during a migration), including one payload that
/// is byte-identical in both generations.
fn coexisting_fixture(path: &Path) {
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
        CREATE TABLE session_v2 (
            id TEXT PRIMARY KEY, title TEXT, parent_id TEXT,
            time_created INTEGER, time_updated INTEGER
        );
        CREATE TABLE session_message (
            id TEXT PRIMARY KEY, session_id TEXT, seq INTEGER, type TEXT,
            time_created INTEGER, time_updated INTEGER, data TEXT
        );
        INSERT INTO session VALUES ('ses_1', 'synthetic', NULL, 1, 2);
        INSERT INTO message VALUES ('msg_1', 'ses_1', 10, 10, '{"role":"user"}');
        INSERT INTO part VALUES ('prt_1', 'msg_1', 'ses_1', 11, 11, '{"type":"text","text":"same"}');
        INSERT INTO session_v2 VALUES ('ses_1', 'synthetic', NULL, 1, 2);
        INSERT INTO session_message VALUES
            ('sm_1', 'ses_1', 1, 'user', 10, 10, '{"type":"text","text":"same"}');
        "#,
    )
    .unwrap();
}

/// Non-whitelisted tables with recognizable sentinel secrets: the evidence
/// store must never contain a single byte of them.
fn credential_tables(conn: &Connection) {
    conn.execute_batch(
        r#"
        CREATE TABLE credential (
            id TEXT PRIMARY KEY, name TEXT, secret TEXT
        );
        CREATE TABLE account (
            id TEXT PRIMARY KEY, email TEXT, token TEXT
        );
        CREATE TABLE kv (
            key TEXT PRIMARY KEY, value TEXT
        );
        CREATE TABLE event (
            id TEXT PRIMARY KEY, aggregate_id TEXT, seq INTEGER, type TEXT,
            data TEXT, created INTEGER
        );
        INSERT INTO credential VALUES
            ('cred_1', 'zhipu', 'SENTINEL-SECRET-credential-1'),
            ('cred_2', 'github', 'SENTINEL-SECRET-credential-2');
        INSERT INTO account VALUES
            ('acc_1', 'user@example.com', 'SENTINEL-SECRET-account-token');
        INSERT INTO kv VALUES ('api_key', 'SENTINEL-SECRET-kv-value');
        INSERT INTO event VALUES
            ('evt_1', 'ses_1', 1, 'message.updated', '{"secret":"SENTINEL-SECRET-event"}', 5);
        "#,
    )
    .unwrap();
}

/// Three sessions spread over time, each with one message and one part, for
/// the `--session-since` filter tests.
fn session_spread_fixture(path: &Path) {
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
        CREATE TABLE project (
            id TEXT PRIMARY KEY, directory TEXT
        );
        INSERT INTO project VALUES ('proj_1', 'C:/synthetic/work');
        "#,
    )
    .unwrap();
    for (id, updated) in [("ses_old", 100), ("ses_mid", 5_000), ("ses_new", 6_000)] {
        conn.execute(
            "INSERT INTO session VALUES (?1, ?1, NULL, ?2, ?2)",
            rusqlite::params![id, updated],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO message VALUES (?1, ?2, ?3, ?3, '{\"role\":\"user\"}')",
            rusqlite::params![format!("msg_{id}"), id, updated],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO part VALUES (?1, ?2, ?3, ?4, ?4, '{\"type\":\"text\"}')",
            rusqlite::params![format!("prt_{id}"), format!("msg_{id}"), id, updated],
        )
        .unwrap();
    }
}

fn exec(path: &Path, sql: &str) {
    let conn = Connection::open(path).unwrap();
    conn.execute_batch(sql).unwrap();
}

fn make_root() -> (tempfile::TempDir, Archive) {
    let temp = tempfile::tempdir().unwrap();
    let archive = Archive::init(temp.path()).unwrap();
    (temp, archive)
}

fn capture(archive: &Archive, src: &Path) -> CaptureReport {
    capture_source(archive, src, "opencode", DOMAIN, None).unwrap()
}

fn capture_opts(archive: &Archive, src: &Path, options: CaptureOptions) -> CaptureReport {
    capture_source_with_options(archive, src, "opencode", DOMAIN, None, options).unwrap()
}

fn options_session_since(ms: i64) -> CaptureOptions {
    CaptureOptions {
        session_since_ms: Some(ms),
        ..CaptureOptions::default()
    }
}

fn source_key(archive: &Archive) -> String {
    let sources = archive.load_sources().unwrap();
    assert_eq!(sources.sources.len(), 1, "one synthetic source per test");
    sources.sources.keys().next().unwrap().clone()
}

fn sqlite_state(archive: &Archive) -> SqliteState {
    let key = source_key(archive);
    archive
        .load_sources()
        .unwrap()
        .sources
        .get(&key)
        .and_then(|entry| entry.sqlite.clone())
        .expect("sqlite state present")
}

fn rows_by_table(path: &Path) -> BTreeMap<String, BTreeMap<String, Value>> {
    observe_database(path).unwrap().tables
}

fn cli() -> Command {
    Command::new(env!("CARGO_BIN_EXE_agent-memory"))
}

/// Byte-scan every file under `root` for the needle (used by the credential
/// sentinel test: zstd output of non-containing input cannot contain it).
fn root_contains(root: &Path, needle: &[u8]) -> bool {
    fn walk(dir: &Path, needle: &[u8]) -> bool {
        for entry in fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if path.is_dir() {
                if walk(&path, needle) {
                    return true;
                }
            } else if fs::read(&path)
                .map(|bytes| {
                    bytes
                        .windows(needle.len())
                        .any(|window| window == needle)
                })
                .unwrap_or(false)
            {
                return true;
            }
        }
        false
    }
    walk(root, needle)
}

// ---------------------------------------------------------------------------
// 1. First capture: chunked whitelist baseline, no whole-file copy
// ---------------------------------------------------------------------------
#[test]
fn initial_capture_is_a_chunked_whitelist_baseline() {
    let (temp, archive) = make_root();
    let db = temp.path().join("opencode.db");
    v1_fixture(&db);
    let before = fs::read(&db).unwrap();

    let report = capture(&archive, &db);
    assert!(report.ok, "report: {report:?}");
    assert_eq!(report.status, "captured");
    assert_eq!(report.change, "initial");
    assert_eq!(report.source_kind.as_deref(), Some("sqlite"));
    // 1 session + 1 message + 24 parts + 1 project (todo is empty).
    assert_eq!(report.records_discovered, 27);
    assert_eq!(report.records_new, 27);
    let info = report.sqlite.as_ref().expect("sqlite facts");
    assert_eq!(info.mode, "baseline");
    assert_eq!(info.snapshot_trigger, "initial");
    assert_eq!(info.capture_seq, 1);
    assert_eq!(info.baseline_generation, 1);
    assert_eq!(info.rows_discovered, 27);
    assert_eq!(info.baseline_chunks, 4, "one non-empty chunk per table");
    assert!(info.baseline_bytes > 0);
    assert!(info.schema_tables.contains(&"part".to_string()));
    assert!(info.schema_tables.contains(&"project".to_string()));
    assert_eq!(info.delta_chain_length, 0);

    // Exactly one manifest; one object per baseline chunk.
    assert_eq!(archive.manifests().unwrap().len(), 1);
    let reconcile = archive.reconcile(&[]).unwrap();
    assert_eq!(reconcile.objects_on_disk, 4);
    let baselines = sqlite_baselines(&archive, &source_key(&archive)).unwrap();
    assert_eq!(baselines.len(), 1);
    assert_eq!(baselines[0].capture_seq, 1);
    assert_eq!(baselines[0].chunks.len(), 4);
    assert_eq!(baselines[0].tables["part"], 24);
    assert_eq!(baselines[0].tables["session"], 1);
    assert!(baselines[0].ddl.contains_key("part"));

    // The archived rows are the exact whitelisted rows, chunk-verified; the
    // empty `todo` table is present with an empty row map.
    let rows = read_baseline_rows(&archive, &baselines[0], DOMAIN).unwrap();
    assert_eq!(rows.len(), 5, "all whitelisted tables (todo empty)");
    assert!(rows["todo"].is_empty());
    let part_key = "pk:[\"prt_01\"]";
    assert_eq!(
        rows["part"][&part_key.to_string()]["data"],
        json!("{\"type\":\"text\",\"text\":\"part body 1\"}")
    );

    // Read-only source: bytes unchanged by the capture.
    assert_eq!(fs::read(&db).unwrap(), before);
}

// ---------------------------------------------------------------------------
// 2. Repeated capture / repeated baseline dedupe
// ---------------------------------------------------------------------------
#[test]
fn repeated_capture_and_repeated_baseline_are_deduped() {
    let (temp, archive) = make_root();
    let db = temp.path().join("opencode.db");
    v1_fixture(&db);

    let first = capture(&archive, &db);
    assert_eq!(first.change, "initial");

    // No change: cheap watermark observation, nothing committed.
    let second = capture(&archive, &db);
    assert_eq!(second.status, "skipped_unchanged");
    assert_eq!(second.change, "skipped_unchanged");
    assert_eq!(second.sqlite.as_ref().unwrap().mode, "unchanged");
    assert_eq!(archive.manifests().unwrap().len(), 1);
    assert_eq!(archive.reconcile(&[]).unwrap().objects_on_disk, 4);

    // Force a baseline trigger by losing the observation state: the export is
    // identical (same chunk digests), so it must not become a second baseline —
    // but the run must report degraded (incremental capture was not safe), so
    // `report.ok` agrees with `coverage.ok`.
    let state = sqlite_state(&archive);
    fs::remove_file(archive.root().join(&state.observation_file)).unwrap();
    let third = capture(&archive, &db);
    assert_eq!(third.change, "degraded");
    assert_eq!(third.status, "degraded");
    assert!(!third.ok);
    assert_eq!(third.sqlite.as_ref().unwrap().mode, "unchanged");
    assert_eq!(
        third.sqlite.as_ref().unwrap().snapshot_trigger,
        "snapshot_deduplicated:observation_state_missing",
        "dedupe must stay visible as a capture fact"
    );
    assert!(third
        .anomalies
        .iter()
        .any(|anomaly| anomaly == "sqlite_snapshot:observation_state_missing"));
    assert_eq!(
        third.ok,
        archive.coverage().unwrap().ok,
        "report.ok and coverage.ok must agree"
    );
    assert_eq!(
        sqlite_baselines(&archive, &source_key(&archive)).unwrap().len(),
        1
    );
    assert_eq!(archive.reconcile(&[]).unwrap().objects_on_disk, 4);

    // The observation state was rewritten, so the next change is a delta again.
    exec(
        &db,
        "UPDATE part SET data = '{\"type\":\"text\",\"text\":\"bye\"}',
                time_updated = 900 WHERE id = 'prt_01';",
    );
    let fourth = capture(&archive, &db);
    assert_eq!(fourth.change, "delta");
    assert_eq!(fourth.sqlite.as_ref().unwrap().rows_updated, 1);
    assert!(fourth.ok);
    assert!(archive.coverage().unwrap().ok);
}

// ---------------------------------------------------------------------------
// 3. Watermark deltas (insert / update / delete) + replay of every point
// ---------------------------------------------------------------------------
#[test]
fn watermark_deltas_replay_to_each_observation_point() {
    let (temp, archive) = make_root();
    let db = temp.path().join("opencode.db");
    v1_fixture(&db);
    capture(&archive, &db);
    let key = source_key(&archive);

    // Insert: a new part with a watermark value past the current one.
    exec(
        &db,
        "INSERT INTO part VALUES ('prt_25', 'msg_1', 'ses_1', 900, 900, '{\"type\":\"text\",\"text\":\"second\"}');",
    );
    let insert = capture(&archive, &db);
    let insert_info = insert.sqlite.as_ref().unwrap();
    assert_eq!(insert.change, "delta");
    assert_eq!(insert_info.delta_rows, 1);
    assert_eq!(insert_info.rows_inserted, 1);
    assert_eq!(insert_info.capture_seq, 2);
    assert_eq!(insert.records_new, 1);

    // Update: content change with a bumped watermark column.
    exec(
        &db,
        "UPDATE part SET data = '{\"type\":\"text\",\"text\":\"second-edited\"}',
                time_updated = 950 WHERE id = 'prt_25';",
    );
    let update = capture(&archive, &db);
    let update_info = update.sqlite.as_ref().unwrap();
    assert_eq!(update_info.rows_updated, 1);
    assert_eq!(update_info.capture_seq, 3);
    assert!(update_info.delta_rows == 1);

    // Delete: found by primary-key set comparison, before-image from evidence.
    exec(&db, "DELETE FROM part WHERE id = 'prt_25';");
    let delete = capture(&archive, &db);
    let delete_info = delete.sqlite.as_ref().unwrap();
    assert_eq!(delete_info.rows_deleted, 1);
    assert_eq!(delete_info.capture_seq, 4);

    // Replay of the latest observation point == the live database, row by row.
    let live = rows_by_table(&db);
    let latest = replay_sqlite(&archive, &key, None, DOMAIN).unwrap();
    assert_eq!(latest.capture_seq, 4);
    assert_eq!(latest.tables, live);
    assert_eq!(latest.deltas_applied, 3);

    // Replay of earlier observation points.
    let at2 = replay_sqlite(&archive, &key, Some(2), DOMAIN).unwrap();
    assert_eq!(at2.capture_seq, 2);
    let part2 = at2.tables.get("part").unwrap();
    assert!(part2.contains_key("pk:[\"prt_01\"]"));
    assert!(part2.contains_key("pk:[\"prt_25\"]"));
    assert_eq!(
        part2[&"pk:[\"prt_25\"]".to_string()]["data"],
        json!("{\"type\":\"text\",\"text\":\"second\"}")
    );

    let at3 = replay_sqlite(&archive, &key, Some(3), DOMAIN).unwrap();
    assert_eq!(
        at3.tables.get("part").unwrap()[&"pk:[\"prt_25\"]".to_string()]["data"],
        json!("{\"type\":\"text\",\"text\":\"second-edited\"}")
    );

    let at4 = replay_sqlite(&archive, &key, Some(4), DOMAIN).unwrap();
    assert!(!at4.tables.get("part").unwrap().contains_key("pk:[\"prt_25\"]"));

    // Raw delta lines carry only observable facts (format @2 keeps the field
    // set; the version tag lives in the manifest's deltaSchema).
    let batches = sqlite_deltas(&archive, &key, DOMAIN).unwrap();
    assert_eq!(batches.len(), 3);
    assert_eq!(batches[0].lines[0].op, "insert");
    assert!(batches[0].lines[0].before.is_none());
    assert!(batches[0].lines[0].observed_at_ms > 0);
    // The update's before image comes from the evidence, not from state.
    let update_line = &batches[1].lines[0];
    assert_eq!(update_line.op, "update");
    assert_eq!(
        update_line.before.as_ref().unwrap()["data"],
        json!("{\"type\":\"text\",\"text\":\"second\"}")
    );
    // The delete's before image is the latest archived row content.
    let delete_line = &batches[2].lines[0];
    assert_eq!(delete_line.op, "delete");
    assert_eq!(
        delete_line.before.as_ref().unwrap()["data"],
        json!("{\"type\":\"text\",\"text\":\"second-edited\"}")
    );
    let line = serde_json::to_value(&batches[0].lines[0]).unwrap();
    let mut keys: Vec<String> = line.as_object().unwrap().keys().cloned().collect();
    keys.sort();
    assert_eq!(
        keys,
        vec![
            "after",
            "before",
            "capture_seq",
            "key",
            "observed_at_ms",
            "op",
            "table"
        ]
    );
}

// ---------------------------------------------------------------------------
// 4. Changed-and-reverted rows (watermark included): reported as a blind window
// ---------------------------------------------------------------------------
#[test]
fn reverted_changes_are_reported_as_a_blind_window() {
    let (temp, archive) = make_root();
    let db = temp.path().join("opencode.db");
    v1_fixture(&db);
    capture(&archive, &db);

    // Change and change back entirely between two observations — including the
    // watermark column, which is what the watermark method keys on.
    exec(
        &db,
        "UPDATE part SET data = '{\"type\":\"text\",\"text\":\"B\"}', time_updated = 999 WHERE id = 'prt_01';",
    );
    exec(
        &db,
        "UPDATE part SET data = '{\"type\":\"text\",\"text\":\"part body 1\"}', time_updated = 101 WHERE id = 'prt_01';",
    );

    let report = capture(&archive, &db);
    assert_eq!(report.change, "skipped_unchanged");
    assert_eq!(report.sqlite.as_ref().unwrap().mode, "unchanged");
    assert_eq!(report.sqlite.as_ref().unwrap().delta_rows, 0);
    assert!(sqlite_deltas(&archive, &source_key(&archive), DOMAIN)
        .unwrap()
        .is_empty());
    // The observation gap is reported instead of claiming the change was seen.
    let interval = report
        .sqlite
        .as_ref()
        .unwrap()
        .observation_interval_ms
        .expect("observation interval is reported");
    assert!(interval >= 0);

    let coverage = archive.coverage().unwrap();
    let source = &coverage.sources[0];
    assert!(source.anomalies.is_empty(), "no false alarm either");
    assert_eq!(source.status, "skipped_unchanged");
    let sqlite = source.sqlite.as_ref().expect("sqlite coverage");
    assert_eq!(sqlite.observation_seq, 1, "no new evidence point invented");
    assert!(sqlite.blind_window.contains("revert"));
    assert!(sqlite.observation_interval_ms.is_some());

    // CLI JSON contract: the blind window and observation watermark are output.
    let output = cli()
        .arg("coverage")
        .arg("--root")
        .arg(archive.root())
        .output()
        .unwrap();
    assert!(output.status.success());
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    let source = &value["sources"][0];
    assert_eq!(source["sqlite"]["observationSeq"], 1);
    assert_eq!(source["sqlite"]["deltaChainLength"], 0);
    assert!(source["sqlite"]["blindWindow"]
        .as_str()
        .unwrap()
        .contains("revert"));
}

// ---------------------------------------------------------------------------
// 5. Whitelist boundary: unknown columns verbatim, unknown tables counted only
// ---------------------------------------------------------------------------
#[test]
fn unknown_columns_are_verbatim_and_unknown_tables_are_never_read() {
    let (temp, archive) = make_root();
    let db = temp.path().join("opencode.db");
    v1_fixture(&db);
    credential_tables(&Connection::open(&db).unwrap());
    exec(
        &db,
        "ALTER TABLE part ADD COLUMN zz TEXT;
         UPDATE part SET zz = 'unknown-column-value' WHERE id = 'prt_01';",
    );
    let report = capture(&archive, &db);
    assert!(report.ok, "{report:?}");
    let key = source_key(&archive);
    let info = report.sqlite.as_ref().unwrap();

    // Non-whitelisted tables appear only as {table, row_count} facts.
    let excluded = &info.excluded_tables;
    assert!(excluded.iter().any(|fact| fact.table == "credential" && fact.row_count == 2));
    assert!(excluded.iter().any(|fact| fact.table == "account" && fact.row_count == 1));
    assert!(excluded.iter().any(|fact| fact.table == "kv" && fact.row_count == 1));
    assert!(excluded.iter().any(|fact| fact.table == "event" && fact.row_count == 1));

    // Unknown columns on whitelisted tables are stored verbatim.
    let replay = replay_sqlite(&archive, &key, None, DOMAIN).unwrap();
    assert_eq!(
        replay.tables["part"][&"pk:[\"prt_01\"]".to_string()]["zz"],
        json!("unknown-column-value")
    );

    // A change in a non-whitelisted table produces no delta: its rows are
    // never read, only counted.
    exec(&db, "INSERT INTO credential VALUES ('cred_3', 'x', 'SENTINEL-SECRET-more');");
    let after = capture(&archive, &db);
    assert_eq!(after.change, "skipped_unchanged");
    assert_eq!(
        after.sqlite.as_ref().unwrap().excluded_tables.iter()
            .find(|fact| fact.table == "credential").unwrap()
            .row_count,
        3,
        "the row-count fact moved"
    );
}

// ---------------------------------------------------------------------------
// 6. Credentials never enter the evidence store (byte-level sentinel)
// ---------------------------------------------------------------------------
#[test]
fn credential_sentinels_never_reach_the_runtime_root() {
    let temp = tempfile::tempdir().unwrap();
    // The evidence root is a dedicated directory and the synthetic source
    // lives outside it, so the byte scan below covers exactly the evidence.
    let archive = Archive::init(&temp.path().join("evidence")).unwrap();
    let source_dir = temp.path().join("source");
    fs::create_dir_all(&source_dir).unwrap();
    let db = source_dir.join("opencode.db");
    v1_fixture(&db);
    credential_tables(&Connection::open(&db).unwrap());
    let report = capture(&archive, &db);
    assert!(report.ok, "{report:?}");

    // Baseline + a delta round-trip, then scan the whole runtime root.
    exec(
        &db,
        "UPDATE credential SET secret = 'SENTINEL-SECRET-rotated' WHERE id = 'cred_1';",
    );
    capture(&archive, &db);
    assert!(
        !root_contains(archive.root(), b"SENTINEL-SECRET"),
        "a credential sentinel reached the runtime root"
    );

    // Replay works and still contains no credential rows.
    let replay = replay_sqlite(&archive, &source_key(&archive), None, DOMAIN).unwrap();
    assert!(!replay.tables.contains_key("credential"));
    assert!(!replay.tables.contains_key("account"));
    assert!(!replay.tables.contains_key("kv"));
    assert_eq!(replay.tables["session"].len(), 1);
}

// ---------------------------------------------------------------------------
// 7. Whitelist schema change triggers a new baseline
// ---------------------------------------------------------------------------
#[test]
fn schema_change_triggers_a_new_baseline() {
    let (temp, archive) = make_root();
    let db = temp.path().join("opencode.db");
    v1_fixture(&db);
    capture(&archive, &db);
    let key = source_key(&archive);

    exec(&db, "ALTER TABLE part ADD COLUMN extra TEXT;");
    let added = capture(&archive, &db);
    assert_eq!(added.change, "baseline");
    let info = added.sqlite.as_ref().unwrap();
    assert_eq!(info.baseline_generation, 2);
    assert_eq!(info.snapshot_trigger, "unknown_table_or_column");
    assert_eq!(info.capture_seq, 2);
    let baselines = sqlite_baselines(&archive, &key).unwrap();
    assert_eq!(baselines.len(), 2);
    assert!(
        archive.coverage().unwrap().ok,
        "a schema-change baseline is a capture fact, not ill health"
    );
    let rows = read_baseline_rows(&archive, &baselines[1], DOMAIN).unwrap();
    assert!(rows["part"]
        .values()
        .next()
        .unwrap()
        .as_object()
        .unwrap()
        .contains_key("extra"));
    assert!(replay_sqlite(&archive, &key, None, DOMAIN)
        .unwrap()
        .columns["part"]
        .contains(&"extra".to_string()));

    exec(&db, "ALTER TABLE part DROP COLUMN extra;");
    let dropped = capture(&archive, &db);
    assert_eq!(dropped.change, "baseline");
    assert_eq!(
        dropped.sqlite.as_ref().unwrap().snapshot_trigger,
        "schema_change"
    );
    assert_eq!(
        sqlite_baselines(&archive, &key).unwrap().len(),
        3,
        "each schema generation is a baseline, and old chunks stay readable"
    );
    // Old generations are still readable evidence.
    let old = read_baseline_rows(&archive, &sqlite_baselines(&archive, &key).unwrap()[0], DOMAIN)
        .unwrap();
    assert_eq!(old.values().map(|t| t.len()).sum::<usize>(), 27);
}

// ---------------------------------------------------------------------------
// 8. Active WAL writes: observed, without a baseline per write
// ---------------------------------------------------------------------------
#[test]
fn wal_active_writes_are_observed_without_extra_baselines() {
    use std::time::Duration;
    let (temp, archive) = make_root();
    let db = temp.path().join("opencode.db");
    {
        let writer = Connection::open(&db).unwrap();
        writer.pragma_update(None, "journal_mode", "WAL").unwrap();
        writer.execute_batch("PRAGMA wal_autocheckpoint = 0;").unwrap();
        writer.execute_batch(
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
            CREATE TABLE session_v2 (
                id TEXT PRIMARY KEY, title TEXT, parent_id TEXT,
                time_created INTEGER, time_updated INTEGER
            );
            CREATE TABLE session_message (
                id TEXT PRIMARY KEY, session_id TEXT, seq INTEGER, type TEXT,
                time_created INTEGER, time_updated INTEGER, data TEXT
            );
            INSERT INTO session VALUES ('ses_1', 'wal', NULL, 1, 2);
            INSERT INTO message VALUES ('msg_1', 'ses_1', 10, 10, '{"role":"user"}');
            INSERT INTO part VALUES ('prt_1', 'msg_1', 'ses_1', 11, 11, '{"text":"a"}');
            "#,
        )
        .unwrap();
        for index in 2..=24 {
            writer
                .execute(
                    "INSERT INTO part VALUES (?1, 'msg_1', 'ses_1', ?2, ?2, ?3)",
                    rusqlite::params![
                        format!("prt_{index:02}"),
                        100 + index,
                        json!({"text": format!("body {index}")}).to_string()
                    ],
                )
                .unwrap();
        }
        // The writer stays open: WAL frames are live during the captures.
        let first = capture(&archive, &db);
        assert_eq!(first.change, "initial");
        let wal = Path::new(&format!("{}-wal", db.display())).to_path_buf();
        assert!(wal.exists(), "WAL frames are live during the capture");
        assert!(first.sqlite.as_ref().unwrap().wal_bytes > 0);
        std::thread::sleep(Duration::from_millis(5));

        writer
            .execute_batch(
                "INSERT INTO part VALUES ('prt_2', 'msg_1', 'ses_1', 12, 500, '{\"text\":\"b\"}');
                 UPDATE part SET data = '{\"text\":\"a-grown\"}', time_updated = 600 WHERE id = 'prt_1';",
            )
            .unwrap();

        let second = capture(&archive, &db);
        assert_eq!(
            second.change,
            "delta",
            "reason={:?} trigger={:?} status={}",
            second.change_reason,
            second.sqlite.as_ref().map(|i| i.snapshot_trigger.clone()),
            second.status
        );
        assert_eq!(second.sqlite.as_ref().unwrap().rows_inserted, 1);
        assert_eq!(second.sqlite.as_ref().unwrap().rows_updated, 1);

        writer
            .execute_batch(
                "INSERT INTO session_message VALUES ('sm_1','ses_1',1,'user',10,700,'{\"text\":\"c\"}');",
            )
            .unwrap();
        let third = capture(&archive, &db);
        assert_eq!(third.change, "delta");

        // WAL growth alone never caused a baseline.
        assert_eq!(
            sqlite_baselines(&archive, &source_key(&archive))
                .unwrap()
                .len(),
            1
        );
        let replay = replay_sqlite(&archive, &source_key(&archive), None, DOMAIN).unwrap();
        assert_eq!(replay.tables, rows_by_table(&db));
    }
}

// ---------------------------------------------------------------------------
// 9. Part growth is a watermark delta, not a baseline
// ---------------------------------------------------------------------------
#[test]
fn part_growth_is_a_row_delta() {
    let (temp, archive) = make_root();
    let db = temp.path().join("opencode.db");
    v1_fixture(&db);
    capture(&archive, &db);
    let key = source_key(&archive);

    exec(
        &db,
        r#"
        INSERT INTO part VALUES ('prt_90', 'msg_1', 'ses_1', 900, 901, '{"type":"text","text":"p90"}');
        INSERT INTO part VALUES ('prt_91', 'msg_1', 'ses_1', 910, 911, '{"type":"text","text":"p91"}');
        INSERT INTO part VALUES ('prt_92', 'msg_1', 'ses_1', 920, 921, '{"type":"text","text":"p92"}');
        UPDATE part SET data = '{"type":"text","text":"part body 1 grown"}', time_updated = 999 WHERE id = 'prt_01';
        "#,
    );
    let report = capture(&archive, &db);
    let info = report.sqlite.as_ref().unwrap();
    assert_eq!(report.change, "delta");
    assert_eq!(info.rows_inserted, 3);
    assert_eq!(info.rows_updated, 1);
    assert_eq!(info.rows_deleted, 0);
    assert_eq!(
        sqlite_baselines(&archive, &key).unwrap().len(),
        1,
        "part growth is a delta, not a new baseline"
    );
    let replay = replay_sqlite(&archive, &key, None, DOMAIN).unwrap();
    let parts = replay.tables.get("part").unwrap();
    assert_eq!(parts.len(), 27);
    assert_eq!(
        parts[&"pk:[\"prt_01\"]".to_string()]["data"],
        json!("{\"type\":\"text\",\"text\":\"part body 1 grown\"}")
    );
}

// ---------------------------------------------------------------------------
// 10. V1/V2 coexistence: both generations kept, duplicate payloads counted
// ---------------------------------------------------------------------------
#[test]
fn v1_v2_coexistence_keeps_both_generations_and_counts_duplicates() {
    let (temp, archive) = make_root();
    let db = temp.path().join("opencode.db");
    coexisting_fixture(&db);
    let report = capture(&archive, &db);
    let info = report.sqlite.as_ref().unwrap();
    assert!(info.schema_tables.contains(&"part".to_string()));
    assert!(info.schema_tables.contains(&"session_message".to_string()));
    assert_eq!(
        info.duplicate_payload_rows, 1,
        "one payload is byte-identical in the V1 and V2 tables"
    );
    let key = source_key(&archive);
    let baselines = sqlite_baselines(&archive, &key).unwrap();
    let rows = read_baseline_rows(&archive, &baselines[0], DOMAIN).unwrap();
    assert!(rows.contains_key("part"));
    assert!(rows.contains_key("session_message"));

    // Migration writes one message into both generations.
    exec(
        &db,
        r#"
        INSERT INTO part VALUES ('prt_2', 'msg_1', 'ses_1', 12, 12, '{"type":"text","text":"migrated"}');
        INSERT INTO session_message VALUES
            ('sm_2', 'ses_1', 2, 'assistant', 12, 12, '{"type":"text","text":"migrated"}');
        "#,
    );
    let migrated = capture(&archive, &db);
    let info = migrated.sqlite.as_ref().unwrap();
    assert_eq!(migrated.change, "delta");
    assert_eq!(info.rows_inserted, 2);
    // Delta units count duplicates among the changed rows only.
    assert_eq!(info.duplicate_payload_rows, 1);

    // Both generations are kept as separate raw rows; replay is per table.
    let replay = replay_sqlite(&archive, &key, None, DOMAIN).unwrap();
    assert_eq!(
        replay.tables["part"][&"pk:[\"prt_2\"]".to_string()]["data"],
        json!("{\"type\":\"text\",\"text\":\"migrated\"}")
    );
    assert_eq!(
        replay.tables["session_message"][&"pk:[\"sm_2\"]".to_string()]["data"],
        json!("{\"type\":\"text\",\"text\":\"migrated\"}")
    );
    let batches = sqlite_deltas(&archive, &key, DOMAIN).unwrap();
    let tables: Vec<&str> = batches[0]
        .lines
        .iter()
        .map(|line| line.table.as_str())
        .collect();
    assert!(tables.contains(&"part"));
    assert!(tables.contains(&"session_message"));
}

// ---------------------------------------------------------------------------
// 11. Lock timeout is an explicit health error; the watermark never advances
// ---------------------------------------------------------------------------
#[test]
fn lock_timeout_is_an_explicit_health_error() {
    let (temp, archive) = make_root();
    let db = temp.path().join("opencode.db");
    v1_fixture(&db);
    let writer = Connection::open(&db).unwrap();
    writer.execute_batch("BEGIN EXCLUSIVE").unwrap();
    set_lock_timeout_for_tests(Some(50));

    let report = capture(&archive, &db);
    set_lock_timeout_for_tests(None);

    assert!(!report.ok);
    assert_eq!(report.status, "failed");
    assert_eq!(report.change, "sqlite_locked");
    let error = report.error.as_deref().unwrap_or_default();
    assert!(
        error.contains("sqlite_locked"),
        "explicit health error, got {error:?}"
    );
    assert_eq!(archive.manifests().unwrap().len(), 0);

    let coverage = archive.coverage().unwrap();
    assert!(!coverage.ok);
    assert_eq!(coverage.sources.len(), 1, "not an empty report");
    let source = &coverage.sources[0];
    assert_eq!(source.status, "failed");
    assert!(source
        .sqlite
        .as_ref()
        .unwrap()
        .last_unavailable
        .as_deref()
        .unwrap()
        .contains("locked"));

    writer.execute_batch("ROLLBACK").unwrap();
    drop(writer);

    let recovered = capture(&archive, &db);
    assert!(recovered.ok);
    assert_eq!(recovered.change, "initial");
    let coverage = archive.coverage().unwrap();
    assert!(coverage.ok);
    assert!(coverage.sources[0].sqlite.as_ref().unwrap().last_unavailable.is_none());
}

// ---------------------------------------------------------------------------
// 12. Unavailable database: explicit failure, no fabricated empty history
// ---------------------------------------------------------------------------
#[test]
fn unavailable_database_is_not_an_empty_success() {
    let (temp, archive) = make_root();
    let db = temp.path().join("opencode.db");
    let mut bytes = Vec::from(*b"SQLite format 3\0");
    bytes.extend(std::iter::repeat(0u8).take(4096));
    fs::write(&db, &bytes).unwrap();

    let report = capture(&archive, &db);
    assert!(!report.ok);
    assert_eq!(report.status, "failed");
    assert_eq!(report.change, "sqlite_unavailable");
    assert!(report
        .anomalies
        .iter()
        .any(|anomaly| anomaly == "sqlite:sqlite_unavailable"));
    let coverage = archive.coverage().unwrap();
    assert!(!coverage.ok);
    assert_eq!(coverage.sources.len(), 1);
    assert_eq!(coverage.sources[0].status, "failed");
    assert!(coverage
        .anomalies
        .iter()
        .any(|anomaly| anomaly.contains("sqlite_unavailable")));
    assert_eq!(archive.manifests().unwrap().len(), 0);
}

// ---------------------------------------------------------------------------
// 13. High-frequency writes: the 50%-of-baseline chain rule re-baselines
// ---------------------------------------------------------------------------
#[test]
fn chain_bytes_over_half_the_baseline_trigger_a_baseline() {
    let (temp, archive) = make_root();
    let db = temp.path().join("opencode.db");
    v1_fixture(&db);
    capture(&archive, &db);
    let key = source_key(&archive);

    let runs = 40;
    for i in 0..runs {
        exec(
            &db,
            &format!(
                "UPDATE part SET data = '{{\"type\":\"text\",\"text\":\"tick-{i}\"}}',
                        time_updated = {} WHERE id = 'prt_01';",
                200 + i
            ),
        );
        let report = capture(&archive, &db);
        assert!(
            report.ok,
            "run {i}: {report:?}"
        );
        assert!(
            report.change == "delta" || report.change == "baseline",
            "run {i}: unexpected change {}",
            report.change
        );
    }

    let state = sqlite_state(&archive);
    assert_eq!(state.capture_seq, runs + 1);
    assert!(
        state.baselines_total >= 2,
        "the chain-byte threshold must actually fire, got {}",
        state.baselines_total
    );
    assert!(
        state.baselines_total <= runs / 3 + 1,
        "baselines must stay proportional to the change volume ({} baselines for {runs} writes)",
        state.baselines_total
    );
    assert_eq!(state.deltas_total, runs + 1 - state.baselines_total);
    assert_eq!(archive.manifests().unwrap().len() as u64, runs + 1);
    assert_eq!(
        sqlite_baselines(&archive, &key).unwrap().len() as u64,
        state.baselines_total
    );
    assert_eq!(
        replay_sqlite(&archive, &key, None, DOMAIN).unwrap().tables,
        rows_by_table(&db)
    );
}

// ---------------------------------------------------------------------------
// 14. Source deletion: evidence stays readable, source state only changes
// ---------------------------------------------------------------------------
#[test]
fn source_deletion_keeps_the_archived_evidence_readable() {
    let (temp, archive) = make_root();
    let db = temp.path().join("opencode.db");
    v1_fixture(&db);
    capture(&archive, &db);
    exec(
        &db,
        "INSERT INTO part VALUES ('prt_30','msg_1','ses_1',12,300,'{\"text\":\"b\"}');",
    );
    capture(&archive, &db);
    let key = source_key(&archive);
    let before = rows_by_table(&db);

    fs::remove_file(&db).unwrap();
    let report = capture(&archive, &db);
    assert!(report.ok, "source removal is a source state, not a capture failure");
    assert_eq!(report.status, "source_missing");
    assert_eq!(report.change, "source_missing");

    let coverage = archive.coverage().unwrap();
    assert!(coverage.ok);
    assert_eq!(
        report.ok, coverage.ok,
        "report.ok must equal coverage.ok for source_missing"
    );
    let sqlite = coverage.sources[0].sqlite.as_ref().unwrap();
    assert_eq!(sqlite.observation_seq, 2);

    let replay = replay_sqlite(&archive, &key, None, DOMAIN).unwrap();
    assert_eq!(replay.tables, before);
    let baselines = sqlite_baselines(&archive, &key).unwrap();
    let rows = read_baseline_rows(&archive, &baselines[0], DOMAIN).unwrap();
    assert_eq!(rows["session"].len(), 1);
}

// ---------------------------------------------------------------------------
// 15. Single writer: second instance refused
// ---------------------------------------------------------------------------
#[test]
fn second_capture_instance_is_refused() {
    let (temp, archive) = make_root();
    let db = temp.path().join("opencode.db");
    v1_fixture(&db);
    let guard = archive.acquire_lock().unwrap();
    let error = capture_source(&archive, &db, "opencode", DOMAIN, None).unwrap_err();
    assert!(matches!(error, ArchiveError::LockHeld(_)), "{error:?}");
    drop(guard);
    assert!(capture(&archive, &db).ok);
}

// ---------------------------------------------------------------------------
// 16. Crash before the manifest commit: no watermark, no duplicate objects
// ---------------------------------------------------------------------------
#[test]
fn crash_before_manifest_commit_leaves_no_watermark() {
    let (temp, archive) = make_root();
    let db = temp.path().join("opencode.db");
    v1_fixture(&db);

    let crashed = std::panic::catch_unwind(AssertUnwindSafe(|| {
        let _ = capture_source(
            &archive,
            &db,
            "opencode",
            DOMAIN,
            Some(Failpoint::BeforeManifestCommit),
        );
    }));
    assert!(crashed.is_err());
    assert!(archive.manifests().unwrap().is_empty());
    assert!(archive.load_sources().unwrap().sources.is_empty());
    // The chunk objects are orphaned (reported, never auto-deleted)…
    let orphans = archive.reconcile(&[]).unwrap();
    assert_eq!(orphans.objects_on_disk, 4);
    assert_eq!(orphans.orphan_objects.len(), 4);

    // …and the rerun reuses them instead of writing second copies.
    let report = capture(&archive, &db);
    assert_eq!(report.change, "initial");
    assert_eq!(report.objects_deduped, 4);
    assert_eq!(report.objects_new, 0);
    assert_eq!(archive.reconcile(&[]).unwrap().objects_on_disk, 4);
    assert_eq!(
        sqlite_baselines(&archive, &source_key(&archive)).unwrap().len(),
        1
    );
}

// ---------------------------------------------------------------------------
// 17. Crash after the manifest commit, before state: adopt the committed unit
// ---------------------------------------------------------------------------
#[test]
fn crash_after_manifest_adopts_the_committed_unit() {
    let (temp, archive) = make_root();
    let db = temp.path().join("opencode.db");
    v1_fixture(&db);
    capture(&archive, &db);
    exec(
        &db,
        "INSERT INTO part VALUES ('prt_30','msg_1','ses_1',12,300,'{\"text\":\"b\"}');",
    );

    let crashed = std::panic::catch_unwind(AssertUnwindSafe(|| {
        let _ = capture_source(
            &archive,
            &db,
            "opencode",
            DOMAIN,
            Some(Failpoint::AfterManifestBeforeState),
        );
    }));
    assert!(crashed.is_err());
    assert_eq!(archive.manifests().unwrap().len(), 2, "delta is committed");
    assert_eq!(
        sqlite_state(&archive).capture_seq,
        1,
        "state was not advanced"
    );

    let recovered = capture(&archive, &db);
    assert_eq!(recovered.change, "recovered_after_crash");
    assert_eq!(recovered.sqlite.as_ref().unwrap().capture_seq, 2);
    assert_eq!(
        archive.manifests().unwrap().len(),
        2,
        "no duplicate unit for the same change"
    );
    assert_eq!(sqlite_state(&archive).capture_seq, 2);
    assert_eq!(
        replay_sqlite(&archive, &source_key(&archive), None, DOMAIN)
            .unwrap()
            .tables,
        rows_by_table(&db)
    );
}

// ---------------------------------------------------------------------------
// 18. Legacy JSON storage: whole-file capture, content dedupe, readback
// ---------------------------------------------------------------------------
#[test]
fn legacy_json_storage_is_archived_and_deduped_by_content() {
    let (temp, archive) = make_root();
    let one = temp.path().join("storage/message/ses_1/m1.json");
    let two = temp.path().join("storage/message/ses_1-copy/m1.json");
    fs::create_dir_all(one.parent().unwrap()).unwrap();
    fs::create_dir_all(two.parent().unwrap()).unwrap();
    let body = r#"{"id":"m1","role":"user","parts":[{"type":"text","text":"legacy"}]}"#;
    fs::write(&one, body).unwrap();
    fs::write(&two, body).unwrap();

    let first = capture(&archive, &one);
    assert_eq!(first.status, "captured");
    assert_eq!(first.source_kind.as_deref(), Some("whole_file"));
    assert_eq!(first.objects_new, 1);
    let second = capture(&archive, &two);
    assert_eq!(second.status, "captured");
    assert_eq!(second.objects_new, 0);
    assert_eq!(
        second.objects_deduped, 1,
        "identical legacy JSON content is one object"
    );
    assert_eq!(archive.reconcile(&[]).unwrap().objects_on_disk, 1);

    // Both physical sources stay tracked; duplicate content is not two copies.
    assert_eq!(archive.load_sources().unwrap().sources.len(), 2);
    // Readback through the access-tagged handle returns the exact bytes.
    let sources = archive.load_sources().unwrap();
    let state = sources
        .sources
        .get(&source_key_of(&archive, &one))
        .unwrap();
    let sha = state.watermark.as_ref().unwrap().sha256.clone();
    let bytes = archive.reader(DOMAIN).read_object(&sha).unwrap();
    assert_eq!(bytes, body.as_bytes());

    // Repeated capture is a no-op.
    let repeat = capture(&archive, &one);
    assert_eq!(repeat.status, "skipped_unchanged");
    assert_eq!(archive.manifests().unwrap().len(), 2);
}

// ---------------------------------------------------------------------------
// 19. V2-only database (current default write generation), FK-safe delta order
// ---------------------------------------------------------------------------
#[test]
fn v2_only_database_is_captured_and_replays() {
    let (temp, archive) = make_root();
    let db = temp.path().join("opencode.db");
    v2_fixture(&db);
    let first = capture(&archive, &db);
    assert_eq!(first.change, "initial");
    let info = first.sqlite.as_ref().unwrap();
    assert!(info.schema_tables.contains(&"session_v2".to_string()));
    assert!(info.schema_tables.contains(&"session_message".to_string()));
    assert_eq!(info.rows_discovered, 2);

    // Insert the child row BEFORE its parent inside one observation window:
    // the (table, key) line order puts session_message first, so the delta
    // carries a child row whose parent does not exist yet. Replay must apply
    // it without any foreign-key failure (row-state replay) and the derived
    // materialization inserts parent-first with foreign_keys OFF.
    exec(
        &db,
        "INSERT INTO session_message VALUES ('sm_2','ses_v2',2,'assistant',11,11,'{\"text\":\"reply\"}');
         INSERT INTO session_v2 VALUES ('ses_v2b','second',NULL,'C:/w',3,11);",
    );
    let second = capture(&archive, &db);
    assert_eq!(second.change, "delta");
    assert_eq!(second.sqlite.as_ref().unwrap().rows_inserted, 2);
    let batches = sqlite_deltas(&archive, &source_key(&archive), DOMAIN).unwrap();
    let lines = &batches[0].lines;
    assert_eq!(lines[0].table, "session_message", "child line precedes parent");
    assert_eq!(lines[1].table, "session_v2");
    assert_eq!(
        replay_sqlite(&archive, &source_key(&archive), None, DOMAIN)
            .unwrap()
            .tables,
        rows_by_table(&db)
    );
}

// ---------------------------------------------------------------------------
// 20. Readback is fail-closed across access domains
// ---------------------------------------------------------------------------
#[test]
fn sqlite_readback_is_fail_closed_across_access_domains() {
    let (temp, archive) = make_root();
    let db = temp.path().join("opencode.db");
    v1_fixture(&db);
    let report = capture_source(&archive, &db, "opencode", "alpha", None).unwrap();
    assert!(report.ok);
    exec(
        &db,
        "INSERT INTO part VALUES ('prt_30','msg_1','ses_1',12,300,'{\"text\":\"b\"}');",
    );
    capture_source(&archive, &db, "opencode", "alpha", None).unwrap();
    let key = source_key(&archive);

    // Same domain: readable.
    assert!(replay_sqlite(&archive, &key, None, "alpha").is_ok());
    let baselines = sqlite_baselines(&archive, &key).unwrap();
    assert!(read_baseline_rows(&archive, &baselines[0], "alpha").is_ok());

    // Other domain: denied, not silently partial.
    assert!(matches!(
        replay_sqlite(&archive, &key, None, "beta"),
        Err(ArchiveError::AccessDenied(_))
    ));
    assert!(matches!(
        sqlite_deltas(&archive, &key, "beta"),
        Err(ArchiveError::AccessDenied(_))
    ));
    assert!(matches!(
        read_baseline_rows(&archive, &baselines[0], "beta"),
        Err(ArchiveError::AccessDenied(_))
    ));
}

// ---------------------------------------------------------------------------
// 21. The 600 KiB single-delta case still re-baselines on the next run
// ---------------------------------------------------------------------------
#[test]
fn delta_chain_byte_threshold_triggers_a_baseline() {
    let (temp, archive) = make_root();
    let db = temp.path().join("opencode.db");
    v1_fixture(&db);
    capture(&archive, &db);

    // A ~600 KiB payload change alone exceeds 50% of the small baseline.
    let big = "x".repeat(600 * 1024);
    exec(
        &db,
        &format!(
            "UPDATE part SET data = '{{\"type\":\"text\",\"text\":\"{big}\"}}',
                    time_updated = 900 WHERE id = 'prt_01';"
        ),
    );
    let grown = capture(&archive, &db);
    assert_eq!(grown.change, "delta");
    let state = sqlite_state(&archive);
    assert!(state.delta_chain_bytes >= 512 * 1024);

    exec(
        &db,
        "UPDATE part SET data = '{\"type\":\"text\",\"text\":\"small\"}',
                time_updated = 901 WHERE id = 'prt_01';",
    );
    let next = capture(&archive, &db);
    assert_eq!(next.change, "baseline");
    assert_eq!(
        next.sqlite.as_ref().unwrap().snapshot_trigger,
        "delta_chain_threshold"
    );
    assert_eq!(sqlite_state(&archive).delta_chain_length, 0);
    assert_eq!(
        replay_sqlite(&archive, &source_key(&archive), None, DOMAIN)
            .unwrap()
            .tables,
        rows_by_table(&db)
    );
}

// ---------------------------------------------------------------------------
// 22. Duplicate/NULL primary keys never overwrite each other
// ---------------------------------------------------------------------------
#[test]
fn nullable_primary_keys_do_not_overwrite_rows() {
    let (temp, archive) = make_root();
    let db = temp.path().join("opencode.db");
    let conn = Connection::open(&db).unwrap();
    conn.execute_batch(
        r#"
        CREATE TABLE part (
            id TEXT PRIMARY KEY, message_id TEXT, session_id TEXT,
            time_created INTEGER, time_updated INTEGER, data TEXT
        );
        INSERT INTO part VALUES (NULL, 'm1', 's1', 1, 1, '{"text":"first"}');
        INSERT INTO part VALUES (NULL, 'm1', 's1', 2, 2, '{"text":"second"}');
        "#,
    )
    .unwrap();
    drop(conn);

    capture(&archive, &db);
    let replay = replay_sqlite(&archive, &source_key(&archive), None, DOMAIN).unwrap();
    let rows = replay.tables.get("part").unwrap();
    assert_eq!(rows.len(), 2, "both NULL-key rows are kept");
    let texts: Vec<&str> = rows
        .values()
        .map(|row| row["data"].as_str().unwrap())
        .collect();
    assert!(texts.contains(&"{\"text\":\"first\"}"));
    assert!(texts.contains(&"{\"text\":\"second\"}"));
}

// ---------------------------------------------------------------------------
// 23. A transient lock failure is cleared by the next successful observation
// ---------------------------------------------------------------------------
#[test]
fn health_flag_is_cleared_by_a_successful_unchanged_observation() {
    let (temp, archive) = make_root();
    let db = temp.path().join("opencode.db");
    v1_fixture(&db);
    assert!(capture(&archive, &db).ok);
    assert!(archive.coverage().unwrap().ok);

    let writer = Connection::open(&db).unwrap();
    writer.execute_batch("BEGIN EXCLUSIVE").unwrap();
    set_lock_timeout_for_tests(Some(50));
    let failed = capture(&archive, &db);
    set_lock_timeout_for_tests(None);
    assert!(!failed.ok);
    assert!(!archive.coverage().unwrap().ok);
    // The watermark did not move: state still points at the baseline unit.
    assert_eq!(sqlite_state(&archive).capture_seq, 1);

    writer.execute_batch("ROLLBACK").unwrap();
    drop(writer);

    // No row changed: a successful observation must clear the stale health flag.
    let recovered = capture(&archive, &db);
    assert!(recovered.ok, "report: {recovered:?}");
    assert_eq!(recovered.change, "skipped_unchanged");
    let coverage = archive.coverage().unwrap();
    assert!(coverage.ok, "coverage: {coverage:?}");
    assert!(coverage.sources[0]
        .sqlite
        .as_ref()
        .unwrap()
        .last_unavailable
        .is_none());
    assert!(coverage.anomalies.is_empty());

    // Phase 2: a failure followed by source deletion must not keep `ok=false`
    // forever. The failure stays counted in `errorsTotal`, but current health is
    // `source_missing` on both sides.
    let writer = Connection::open(&db).unwrap();
    writer.execute_batch("BEGIN EXCLUSIVE").unwrap();
    set_lock_timeout_for_tests(Some(50));
    assert!(!capture(&archive, &db).ok);
    set_lock_timeout_for_tests(None);
    writer.execute_batch("ROLLBACK").unwrap();
    drop(writer);
    fs::remove_file(&db).unwrap();
    let missing = capture(&archive, &db);
    assert_eq!(missing.status, "source_missing");
    assert!(missing.ok);
    let coverage = archive.coverage().unwrap();
    assert!(coverage.ok, "coverage: {coverage:?}");
    assert_eq!(
        missing.ok, coverage.ok,
        "report.ok must equal coverage.ok after a failure + deletion"
    );
    assert!(coverage.sources[0]
        .sqlite
        .as_ref()
        .unwrap()
        .last_unavailable
        .is_none());
    assert!(
        coverage.totals.errors_total >= 1,
        "the historical failure stays counted in errorsTotal: {:?}",
        coverage.totals
    );
}

// ---------------------------------------------------------------------------
// 24. A lock taken after the preflight read is a health error, not a hang
// ---------------------------------------------------------------------------
#[test]
fn lock_taken_after_preflight_fails_as_a_health_error() {
    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    static HOLDER: Mutex<Option<Connection>> = Mutex::new(None);
    static TARGET: Mutex<Option<PathBuf>> = Mutex::new(None);

    fn hook(path: &Path) {
        let target = TARGET.lock().unwrap_or_else(|p| p.into_inner()).clone();
        if target.as_deref() != Some(path) {
            return;
        }
        let conn = Connection::open(path).unwrap();
        conn.execute_batch("BEGIN EXCLUSIVE").unwrap();
        *HOLDER.lock().unwrap_or_else(|p| p.into_inner()) = Some(conn);
    }

    let (temp, archive) = make_root();
    let db = temp.path().join("opencode.db");
    v1_fixture(&db);
    *TARGET.lock().unwrap() = Some(db.clone());
    agent_memory::capture_sqlite::set_pre_snapshot_hook(Some(hook));
    set_lock_timeout_for_tests(Some(50));

    let started = Instant::now();
    let report = capture(&archive, &db);
    let elapsed = started.elapsed();

    agent_memory::capture_sqlite::set_pre_snapshot_hook(None);
    set_lock_timeout_for_tests(None);
    *TARGET.lock().unwrap() = None;
    if let Some(conn) = HOLDER.lock().unwrap().take() {
        let _ = conn.execute_batch("ROLLBACK");
    }

    assert!(
        elapsed < Duration::from_secs(10),
        "the baseline retry must be bounded, took {elapsed:?}"
    );
    assert!(!report.ok, "report: {report:?}");
    assert_eq!(report.change, "sqlite_locked");
    assert!(report
        .anomalies
        .iter()
        .any(|anomaly| anomaly == "sqlite:sqlite_locked"));
    assert!(archive.manifests().unwrap().is_empty());

    // The next run succeeds normally.
    let recovered = capture(&archive, &db);
    assert!(recovered.ok);
    assert_eq!(recovered.change, "initial");
}

// ---------------------------------------------------------------------------
// 25. Corrupted observation state is reported (degraded) and healed
// ---------------------------------------------------------------------------
#[test]
fn corrupted_observation_state_is_reported_and_healed() {
    let (temp, archive) = make_root();
    let db = temp.path().join("opencode.db");
    v1_fixture(&db);
    capture(&archive, &db);
    let state = sqlite_state(&archive);
    let path = archive.root().join(&state.observation_file);

    // Flip the compressed bytes: the state can no longer be trusted for diffing.
    let mut bytes = fs::read(&path).unwrap();
    let last = bytes.len() - 1;
    bytes[last] ^= 0xff;
    fs::write(&path, &bytes).unwrap();

    let report = capture(&archive, &db);
    assert_eq!(report.change, "degraded");
    assert!(!report.ok, "unsafe incremental capture is not a success");
    assert!(report
        .anomalies
        .iter()
        .any(|anomaly| anomaly.starts_with("sqlite:observation_state_unreadable")));
    assert!(report
        .anomalies
        .iter()
        .any(|anomaly| anomaly == "sqlite_snapshot:observation_state_missing"));
    // The identical baseline is deduplicated (no second generation) and the
    // state is rewritten, so the source heals without losing evidence.
    assert_eq!(
        report.sqlite.as_ref().unwrap().snapshot_trigger,
        "snapshot_deduplicated:observation_state_missing"
    );
    assert_eq!(
        sqlite_baselines(&archive, &source_key(&archive)).unwrap().len(),
        1
    );
    let coverage = archive.coverage().unwrap();
    assert!(!coverage.ok, "the corruption was reported");
    assert_eq!(report.ok, coverage.ok, "report.ok must equal coverage.ok");

    let healed = capture(&archive, &db);
    assert!(healed.ok);
    assert_eq!(healed.change, "skipped_unchanged");
    assert!(healed.anomalies.is_empty());
    assert!(archive.coverage().unwrap().ok);

    // The CLI must exit non-zero for the same degraded scenario.
    let mut bytes = fs::read(&path).unwrap();
    let last = bytes.len() - 1;
    bytes[last] ^= 0xff;
    fs::write(&path, &bytes).unwrap();
    let output = cli()
        .arg("capture")
        .arg("--root")
        .arg(archive.root())
        .arg("--source")
        .arg(&db)
        .arg("--provider")
        .arg("opencode")
        .arg("--domain")
        .arg(DOMAIN)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(7), "capture must fail closed");
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["ok"], false);
    assert_eq!(value["status"], "degraded");

    // And watermark diffs work again after the heal.
    let healed = capture(&archive, &db);
    assert!(healed.ok);
    exec(
        &db,
        "INSERT INTO part VALUES ('prt_30','msg_1','ses_1',12,300,'{\"text\":\"b\"}');",
    );
    let diffed = capture(&archive, &db);
    assert_eq!(diffed.change, "delta");
    assert_eq!(diffed.sqlite.as_ref().unwrap().rows_inserted, 1);
}

// ---------------------------------------------------------------------------
// 26. A lost baseline chunk cannot be "deduplicated" away
// ---------------------------------------------------------------------------
#[test]
fn lost_baseline_object_is_rebuilt_and_reported() {
    let (temp, archive) = make_root();
    let db = temp.path().join("opencode.db");
    v1_fixture(&db);
    capture(&archive, &db);
    let key = source_key(&archive);
    let baseline = sqlite_baselines(&archive, &key).unwrap()[0].clone();
    let chunk = &baseline.chunks[0];
    let object_path = archive
        .root()
        .join("archive/objects")
        .join(&chunk.sha256[0..2])
        .join(&chunk.sha256[2..4])
        .join(&chunk.sha256);
    assert!(object_path.exists());
    fs::remove_file(&object_path).unwrap();

    // The database itself did not change: an identical export must NOT be
    // deduplicated against a missing chunk.
    let rebuilt = capture(&archive, &db);
    assert_eq!(rebuilt.change, "baseline");
    let info = rebuilt.sqlite.as_ref().unwrap();
    assert_eq!(info.snapshot_trigger, "baseline_unavailable");
    assert_eq!(info.baseline_generation, 2);
    assert!(!rebuilt.ok, "lost evidence is a health condition");
    assert_eq!(rebuilt.status, "degraded");
    assert!(rebuilt
        .anomalies
        .iter()
        .any(|anomaly| anomaly == "sqlite_snapshot:baseline_unavailable"));

    let coverage = archive.coverage().unwrap();
    assert!(!coverage.ok);
    assert_eq!(
        coverage.sources[0].sqlite.as_ref().unwrap().baseline_generation,
        2
    );
    // The new baseline is readable and replay works.
    let baselines = sqlite_baselines(&archive, &key).unwrap();
    assert_eq!(baselines.len(), 2);
    let rows = read_baseline_rows(&archive, &baselines[1], DOMAIN).unwrap();
    assert_eq!(rows["session"].len(), 1);
    assert_eq!(
        replay_sqlite(&archive, &key, None, DOMAIN).unwrap().tables,
        rows_by_table(&db)
    );

    // The next unchanged run is healthy again.
    let healed = capture(&archive, &db);
    assert!(healed.ok);
    assert_eq!(healed.change, "skipped_unchanged");
    assert!(archive.coverage().unwrap().ok);
}

// ---------------------------------------------------------------------------
// 27. Observation state is bound to its capture sequence
// ---------------------------------------------------------------------------
#[test]
fn observation_state_with_a_foreign_capture_seq_is_rejected() {
    use std::io::Write;
    let (temp, archive) = make_root();
    let db = temp.path().join("opencode.db");
    v1_fixture(&db);
    capture(&archive, &db);
    exec(
        &db,
        "INSERT INTO part VALUES ('prt_30','msg_1','ses_1',12,300,'{\"text\":\"b\"}');",
    );
    capture(&archive, &db); // seq 2 committed, state at seq 2

    // Rewrite the compressed observation state with a stale capture sequence.
    let state = sqlite_state(&archive);
    let path = archive.root().join(&state.observation_file);
    let decoded = zstd::stream::decode_all(fs::read(&path).unwrap().as_slice()).unwrap();
    let mut value: Value = serde_json::from_slice(&decoded).unwrap();
    value["capture_seq"] = json!(99);
    let reencoded =
        zstd::stream::encode_all(serde_json::to_vec(&value).unwrap().as_slice(), 3).unwrap();
    let mut file = fs::File::create(&path).unwrap();
    file.write_all(&reencoded).unwrap();
    file.sync_all().unwrap();
    drop(file);

    let report = capture(&archive, &db);
    assert!(
        report
            .anomalies
            .iter()
            .any(|anomaly| anomaly.starts_with("sqlite:observation_state_unreadable")),
        "a foreign capture seq must be rejected: {:?}",
        report.anomalies
    );
    // The state cannot be trusted for diffing, so a new baseline is taken from
    // the (changed) database; the previous delta evidence is still replayable.
    assert_eq!(report.change, "baseline");
    assert_eq!(
        report.sqlite.as_ref().unwrap().snapshot_trigger,
        "observation_state_missing"
    );
    assert!(!report.ok);
    assert_eq!(report.status, "degraded");
    assert!(
        report
            .anomalies
            .iter()
            .any(|anomaly| anomaly == "sqlite_snapshot:observation_state_missing")
    );
    let coverage = archive.coverage().unwrap();
    assert!(!coverage.ok);
    assert_eq!(report.ok, coverage.ok, "report.ok must equal coverage.ok");
    assert_eq!(sqlite_state(&archive).capture_seq, 3);
    assert_eq!(
        replay_sqlite(&archive, &source_key(&archive), None, DOMAIN)
            .unwrap()
            .tables,
        rows_by_table(&db)
    );
    let healed = capture(&archive, &db);
    assert!(healed.ok);
    assert_eq!(healed.change, "skipped_unchanged");
    assert!(healed.anomalies.is_empty());
}

// ---------------------------------------------------------------------------
// 28. A damaged OpenCode database header is a health error, not a silent copy
// ---------------------------------------------------------------------------
#[test]
fn damaged_database_header_is_a_health_error() {
    let (temp, archive) = make_root();
    let db = temp.path().join("opencode.db");
    fs::write(&db, b"not a sqlite database at all, but named .db").unwrap();
    let report = capture(&archive, &db);
    assert!(!report.ok, "report: {report:?}");
    assert_eq!(report.status, "failed");
    assert_eq!(report.change, "sqlite_unavailable");
    assert_eq!(report.source_kind.as_deref(), Some("sqlite"));
    assert!(archive.manifests().unwrap().is_empty());
    assert!(!archive.coverage().unwrap().ok);
}

// ---------------------------------------------------------------------------
// 29. A cancelled-out delta chain is reset at the deduplicated threshold baseline
// ---------------------------------------------------------------------------
#[test]
fn deduplicated_threshold_baseline_resets_the_delta_chain() {
    let (temp, archive) = make_root();
    let db = temp.path().join("opencode.db");
    v1_fixture(&db);
    capture(&archive, &db);

    // One update large enough to push the chain past 50% of the small baseline…
    let big = "y".repeat(64 * 1024);
    exec(
        &db,
        &format!(
            "UPDATE part SET data = '{{\"type\":\"text\",\"text\":\"{big}\"}}',
                    time_updated = 900 WHERE id = 'prt_01';"
        ),
    );
    let grown = capture(&archive, &db);
    assert_eq!(grown.change, "delta");
    assert!(sqlite_state(&archive).delta_chain_bytes > 0);

    // …then revert the content (watermark included) *before* the next capture:
    // the threshold baseline fires, finds content identical to the baseline,
    // and deduplicates instead of writing a second generation.
    exec(
        &db,
        "UPDATE part SET data = '{\"type\":\"text\",\"text\":\"part body 1\"}',
                time_updated = 101 WHERE id = 'prt_01';",
    );
    let deduped = capture(&archive, &db);
    assert_eq!(deduped.change, "skipped_unchanged");
    let info = deduped.sqlite.as_ref().unwrap();
    assert_eq!(
        info.snapshot_trigger,
        "snapshot_deduplicated:delta_chain_threshold"
    );
    assert_eq!(info.delta_chain_length, 0, "the chain must be reset");
    let state = sqlite_state(&archive);
    assert_eq!(state.delta_chain_length, 0);
    assert_eq!(state.delta_chain_bytes, 0);
    assert_eq!(
        sqlite_baselines(&archive, &source_key(&archive)).unwrap().len(),
        1,
        "no second baseline for identical content"
    );

    // The next run neither baselines again nor rewrites the chain.
    let next = capture(&archive, &db);
    assert_eq!(next.change, "skipped_unchanged");
    assert!(next.sqlite.as_ref().unwrap().snapshot_trigger.is_empty());
    assert_eq!(next.sqlite.as_ref().unwrap().delta_chain_length, 0);
}

// ---------------------------------------------------------------------------
// 30. `state.seq+1` without a committed unit is not the crash window
// ---------------------------------------------------------------------------
#[test]
fn forged_next_sequence_state_is_rejected_without_a_committed_unit() {
    use std::io::Write;
    let (temp, archive) = make_root();
    let db = temp.path().join("opencode.db");
    v1_fixture(&db);
    capture(&archive, &db); // seq 1 = baseline, no seq 2 manifest exists

    // Forge the observation state: one sequence ahead, with a recomputed state
    // digest so only the sequence gate can reject it. The @2 state carries no
    // row content, so a forged file cannot smuggle a "before" image at all.
    let state = sqlite_state(&archive);
    let path = archive.root().join(&state.observation_file);
    let decoded = zstd::stream::decode_all(fs::read(&path).unwrap().as_slice()).unwrap();
    let mut value: Value = serde_json::from_slice(&decoded).unwrap();
    value["capture_seq"] = json!(2);
    let tables = value["tables"].clone();
    let excluded = value["excluded"].clone();
    let selected = value["selected_sessions"].clone();
    let digest = agent_memory::archive::hex_sha256(
        &serde_json::to_vec(&(&tables, &excluded, &selected)).unwrap(),
    );
    value["state_digest"] = json!(digest);
    let reencoded =
        zstd::stream::encode_all(serde_json::to_vec(&value).unwrap().as_slice(), 3).unwrap();
    let mut file = fs::File::create(&path).unwrap();
    file.write_all(&reencoded).unwrap();
    file.sync_all().unwrap();
    drop(file);

    let report = capture(&archive, &db);
    assert!(
        report
            .anomalies
            .iter()
            .any(|anomaly| anomaly.starts_with("sqlite:observation_state_unreadable")),
        "an unverifiable +1 state must be rejected: {:?}",
        report.anomalies
    );
    // No delta based on the forged state may be written at all.
    assert!(
        sqlite_deltas(&archive, &source_key(&archive), DOMAIN)
            .unwrap()
            .is_empty(),
        "forged state must never produce evidence"
    );
    // The database itself did not change, so the state is healed by dedupe.
    assert_eq!(report.change, "degraded");
    assert_eq!(
        report.sqlite.as_ref().unwrap().snapshot_trigger,
        "snapshot_deduplicated:observation_state_missing"
    );
    assert!(!report.ok);
    assert_eq!(report.ok, archive.coverage().unwrap().ok);
    assert_eq!(sqlite_state(&archive).capture_seq, 1);

    // Also check that a legitimate crash window (+1 with a committed unit) is
    // still accepted: crash after the manifest commit, then re-run.
    exec(
        &db,
        "INSERT INTO part VALUES ('prt_30','msg_1','ses_1',12,300,'{\"text\":\"b\"}');",
    );
    let crashed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = capture_source(
            &archive,
            &db,
            "opencode",
            DOMAIN,
            Some(Failpoint::AfterManifestBeforeState),
        );
    }));
    assert!(crashed.is_err());
    let recovered = capture(&archive, &db);
    assert_eq!(recovered.change, "recovered_after_crash");
    assert_eq!(recovered.sqlite.as_ref().unwrap().capture_seq, 2);
    assert!(
        !recovered.ok,
        "a crash-recovered acquisition is degraded, not a clean success"
    );
    assert_eq!(recovered.ok, archive.coverage().unwrap().ok);
    assert_eq!(
        replay_sqlite(&archive, &source_key(&archive), None, DOMAIN)
            .unwrap()
            .tables,
        rows_by_table(&db)
    );
    // And one clean run afterwards restores health.
    let clean = capture(&archive, &db);
    assert!(clean.ok);
    assert!(archive.coverage().unwrap().ok);
}

// ---------------------------------------------------------------------------
// 31. Schema drift between the preflight and the observation is degraded
// ---------------------------------------------------------------------------
#[test]
fn schema_drift_during_observation_is_degraded() {
    use std::path::PathBuf;
    use std::sync::Mutex;

    static TARGET: Mutex<Option<PathBuf>> = Mutex::new(None);

    fn hook(path: &Path) {
        let target = TARGET.lock().unwrap_or_else(|p| p.into_inner()).clone();
        if target.as_deref() != Some(path) {
            return;
        }
        // DDL after the preflight read: the full observation now sees a schema
        // the plan did not.
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            "ALTER TABLE part ADD COLUMN drift TEXT;
             INSERT INTO part VALUES ('prt_30','msg_1','ses_1',12,300,'{\"text\":\"b\"}','x');",
        )
        .unwrap();
    }

    let (temp, archive) = make_root();
    let db = temp.path().join("opencode.db");
    v1_fixture(&db);
    capture(&archive, &db);
    let manifests_before = archive.manifests().unwrap().len();

    *TARGET.lock().unwrap() = Some(db.clone());
    agent_memory::capture_sqlite::set_pre_observation_hook(Some(hook));
    let report = capture(&archive, &db);
    agent_memory::capture_sqlite::set_pre_observation_hook(None);
    *TARGET.lock().unwrap() = None;

    assert_eq!(report.change, "degraded");
    assert_eq!(report.status, "degraded");
    assert!(!report.ok, "report: {report:?}");
    assert_eq!(
        report.change_reason.as_deref(),
        Some("schema_changed_during_observation")
    );
    assert!(report
        .anomalies
        .iter()
        .any(|anomaly| anomaly == "sqlite:schema_changed_during_observation"));
    assert_eq!(
        archive.manifests().unwrap().len(),
        manifests_before,
        "no delta may be written for a schema the plan did not see"
    );
    let coverage = archive.coverage().unwrap();
    assert!(!coverage.ok);
    assert_eq!(report.ok, coverage.ok, "report.ok must equal coverage.ok");

    // The next run takes the schema-change baseline and is healthy again.
    let next = capture(&archive, &db);
    assert_eq!(next.change, "baseline");
    assert!(next.ok);
    assert_eq!(
        replay_sqlite(&archive, &source_key(&archive), None, DOMAIN)
            .unwrap()
            .tables,
        rows_by_table(&db)
    );
    assert!(archive.coverage().unwrap().ok);
}

// ---------------------------------------------------------------------------
// 32. Unclassifiable collector state commits a baseline and reports degraded
// ---------------------------------------------------------------------------
#[test]
fn unclassifiable_state_is_degraded_and_repaired() {
    let (temp, archive) = make_root();
    let db = temp.path().join("opencode.db");
    v1_fixture(&db);
    capture(&archive, &db);
    let key = source_key(&archive);

    // Strip the schema knowledge the diff needs (simulating a partial/older
    // state written by another version).
    let sources_path = archive.root().join("state").join("sources.json");
    let mut value: Value = serde_json::from_slice(&fs::read(&sources_path).unwrap()).unwrap();
    value["sources"][&key]["sqlite"]["known_columns"] = json!({});
    fs::write(&sources_path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();

    let report = capture(&archive, &db);
    assert_eq!(report.change, "baseline");
    assert_eq!(report.status, "degraded");
    assert!(!report.ok, "report: {report:?}");
    assert_eq!(
        report.sqlite.as_ref().unwrap().snapshot_trigger,
        "unclassifiable_state"
    );
    assert!(report
        .anomalies
        .iter()
        .any(|anomaly| anomaly == "sqlite_snapshot:unclassifiable_state"));
    assert_eq!(report.sqlite.as_ref().unwrap().baseline_generation, 2);
    let coverage = archive.coverage().unwrap();
    assert!(!coverage.ok);
    assert_eq!(report.ok, coverage.ok, "report.ok must equal coverage.ok");
    assert!(
        !sqlite_state(&archive).known_columns.is_empty(),
        "the baseline must restore the known schema"
    );

    let healed = capture(&archive, &db);
    assert!(healed.ok);
    assert_eq!(healed.change, "skipped_unchanged");
    assert!(healed.anomalies.is_empty());
    assert!(archive.coverage().unwrap().ok);
    assert_eq!(
        replay_sqlite(&archive, &key, None, DOMAIN).unwrap().tables,
        rows_by_table(&db)
    );
}

// ---------------------------------------------------------------------------
// 33. A rejected observation state is degraded even without usable prior state
// ---------------------------------------------------------------------------
#[test]
fn orphaned_corrupt_observation_state_is_degraded_without_state() {
    let (temp, archive) = make_root();
    let db = temp.path().join("opencode.db");
    v1_fixture(&db);
    capture(&archive, &db);
    let state = sqlite_state(&archive);
    let path = archive.root().join(&state.observation_file);

    // Corrupt the observation state, then lose `state/sources.json` entirely:
    // there is no usable prior state, but the orphaned state file is still junk.
    let mut bytes = fs::read(&path).unwrap();
    let last = bytes.len() - 1;
    bytes[last] ^= 0xff;
    fs::write(&path, &bytes).unwrap();
    fs::remove_file(archive.root().join("state").join("sources.json")).unwrap();

    let report = capture(&archive, &db);
    assert_eq!(report.change, "baseline");
    assert_eq!(
        report.sqlite.as_ref().unwrap().snapshot_trigger,
        "unclassifiable_state"
    );
    assert!(
        report
            .anomalies
            .iter()
            .any(|anomaly| anomaly.starts_with("sqlite:observation_state_unreadable")),
        "the junk state must be reported: {:?}",
        report.anomalies
    );
    assert!(!report.ok, "report: {report:?}");
    assert_eq!(
        report.ok,
        archive.coverage().unwrap().ok,
        "report.ok must equal coverage.ok even without prior state"
    );

    let healed = capture(&archive, &db);
    assert!(healed.ok);
    assert_eq!(healed.change, "skipped_unchanged");
    assert!(archive.coverage().unwrap().ok);
}

// ---------------------------------------------------------------------------
// 34. A crash followed by more writes: no adoption, no recovery anomaly
// ---------------------------------------------------------------------------
#[test]
fn crash_then_new_writes_falls_back_to_a_clean_delta() {
    let (temp, archive) = make_root();
    let db = temp.path().join("opencode.db");
    v1_fixture(&db);
    capture(&archive, &db);
    exec(
        &db,
        "INSERT INTO part VALUES ('prt_30','msg_1','ses_1',12,300,'{\"text\":\"b\"}');",
    );

    let crashed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = capture_source(
            &archive,
            &db,
            "opencode",
            DOMAIN,
            Some(Failpoint::AfterManifestBeforeState),
        );
    }));
    assert!(crashed.is_err());
    // The writer keeps writing after the crash: adoption can no longer verify
    // the committed delta against the live database.
    exec(
        &db,
        "INSERT INTO part VALUES ('prt_31','msg_1','ses_1',13,301,'{\"text\":\"c\"}');",
    );

    let report = capture(&archive, &db);
    assert_eq!(report.change, "delta");
    assert!(
        report.ok,
        "a normal delta after a stale committed unit is still a clean run"
    );
    assert!(
        report.anomalies.is_empty(),
        "no recovery anomaly when adoption was rejected: {:?}",
        report.anomalies
    );
    let coverage = archive.coverage().unwrap();
    assert!(coverage.ok);
    assert_eq!(report.ok, coverage.ok, "report.ok must equal coverage.ok");
    // Neither a duplicate unit for seq 2 nor a lost change: seq 2 stays as
    // committed, seq 3 carries the post-crash insert.
    let state = sqlite_state(&archive);
    assert_eq!(state.capture_seq, 3);
    assert_eq!(archive.manifests().unwrap().len(), 3);
    let key = source_key(&archive);
    let at2 = replay_sqlite(&archive, &key, Some(2), DOMAIN).unwrap();
    assert!(at2.tables["part"].contains_key("pk:[\"prt_30\"]"));
    assert!(!at2.tables["part"].contains_key("pk:[\"prt_31\"]"));
    assert_eq!(
        replay_sqlite(&archive, &key, None, DOMAIN).unwrap().tables,
        rows_by_table(&db)
    );
}

// ---------------------------------------------------------------------------
// 35. `--rebaseline`: a fresh baseline generation, on demand, not degraded
// ---------------------------------------------------------------------------
#[test]
fn explicit_rebaseline_commits_a_new_generation() {
    let (temp, archive) = make_root();
    let db = temp.path().join("opencode.db");
    v1_fixture(&db);
    capture(&archive, &db);

    let rebaselined = capture_opts(
        &archive,
        &db,
        CaptureOptions {
            rebaseline: true,
            ..CaptureOptions::default()
        },
    );
    assert_eq!(rebaselined.change, "baseline");
    assert_eq!(
        rebaselined.sqlite.as_ref().unwrap().snapshot_trigger,
        "rebaseline_requested"
    );
    assert_eq!(rebaselined.sqlite.as_ref().unwrap().baseline_generation, 2);
    assert!(
        rebaselined.ok,
        "an explicit rebaseline is a designed policy, not ill health"
    );
    assert_eq!(
        sqlite_baselines(&archive, &source_key(&archive)).unwrap().len(),
        2
    );
    assert!(archive.coverage().unwrap().ok);
    assert_eq!(
        replay_sqlite(&archive, &source_key(&archive), None, DOMAIN)
            .unwrap()
            .tables,
        rows_by_table(&db)
    );
}

// ---------------------------------------------------------------------------
// 36. `--session-since`: children scoped to selected sessions; project full
// ---------------------------------------------------------------------------
#[test]
fn session_since_filter_scopes_children_and_project_stays_full() {
    let (temp, archive) = make_root();
    let db = temp.path().join("opencode.db");
    session_spread_fixture(&db);
    credential_tables(&Connection::open(&db).unwrap());

    // Only sessions updated at/after 4000 (ses_mid, ses_new) plus their
    // children; `project` is always full; excluded tables only counted.
    let filtered = capture_opts(&archive, &db, options_session_since(4_000));
    assert!(filtered.ok, "{filtered:?}");
    assert_eq!(filtered.sqlite.as_ref().unwrap().mode, "baseline");
    assert_eq!(
        filtered.sqlite.as_ref().unwrap().session_since_ms,
        Some(4_000)
    );
    // 2 sessions + 2 messages + 2 parts + 1 project = 7 rows.
    assert_eq!(filtered.sqlite.as_ref().unwrap().rows_discovered, 7);
    let replay = replay_sqlite(&archive, &source_key(&archive), None, DOMAIN).unwrap();
    assert_eq!(replay.tables["session"].len(), 2);
    assert!(!replay.tables["session"].contains_key("pk:[\"ses_old\"]"));
    assert_eq!(replay.tables["message"].len(), 2);
    assert_eq!(replay.tables["part"].len(), 2);
    assert_eq!(replay.tables["project"].len(), 1);

    // The filter is recorded in state and coverage.
    assert_eq!(sqlite_state(&archive).session_since_ms, Some(4_000));
    let coverage = archive.coverage().unwrap();
    assert_eq!(coverage.sources[0].sqlite.as_ref().unwrap().session_since_ms, Some(4_000));

    // A write inside an out-of-scope session is not tracked.
    exec(
        &db,
        "INSERT INTO part VALUES ('prt_old2','msg_ses_old','ses_old',9,900,'{\"x\":1}');",
    );
    let unchanged = capture_opts(&archive, &db, options_session_since(4_000));
    assert_eq!(unchanged.change, "skipped_unchanged");
    assert!(sqlite_deltas(&archive, &source_key(&archive), DOMAIN)
        .unwrap()
        .is_empty());

    // A write inside a selected session is a watermark delta (monotonic
    // timestamps put it above the child watermark).
    exec(
        &db,
        "INSERT INTO part VALUES ('prt_new2','msg_ses_new','ses_new',9,9500,'{\"x\":1}');",
    );
    let delta = capture_opts(&archive, &db, options_session_since(4_000));
    assert_eq!(delta.change, "delta");
    assert_eq!(delta.sqlite.as_ref().unwrap().rows_inserted, 1);
}

// ---------------------------------------------------------------------------
// 37. A changed filter re-baselines (`filter_changed`)
// ---------------------------------------------------------------------------
#[test]
fn filter_change_rebaselines() {
    let (temp, archive) = make_root();
    let db = temp.path().join("opencode.db");
    session_spread_fixture(&db);
    capture_opts(&archive, &db, options_session_since(4_000));
    let key = source_key(&archive);

    // Tighten the filter: only ses_new is in scope now.
    let tightened = capture_opts(&archive, &db, options_session_since(5_500));
    assert_eq!(tightened.change, "baseline");
    let info = tightened.sqlite.as_ref().unwrap();
    assert_eq!(info.snapshot_trigger, "filter_changed");
    assert_eq!(info.baseline_generation, 2);
    assert!(tightened.ok);
    assert_eq!(info.rows_discovered, 4, "1 session + 1 message + 1 part + project");
    let replay = replay_sqlite(&archive, &key, None, DOMAIN).unwrap();
    assert_eq!(replay.tables["session"].len(), 1);
    assert!(replay.tables["session"].contains_key("pk:[\"ses_new\"]"));
    assert_eq!(
        sqlite_state(&archive).session_since_ms,
        Some(5_500)
    );

    // Dropping the filter goes back to full scope (another baseline).
    let unfiltered = capture(&archive, &db);
    assert_eq!(unfiltered.change, "baseline");
    assert_eq!(
        unfiltered.sqlite.as_ref().unwrap().snapshot_trigger,
        "filter_changed"
    );
    assert_eq!(unfiltered.sqlite.as_ref().unwrap().baseline_generation, 3);
    assert_eq!(
        replay_sqlite(&archive, &key, None, DOMAIN)
            .unwrap()
            .tables["session"]
            .len(),
        3
    );
}

// ---------------------------------------------------------------------------
// 38. `--verify` finds watermark-blind edits and records correction deltas
// ---------------------------------------------------------------------------
#[test]
fn verify_finds_watermark_blind_changes_and_corrects_them() {
    let (temp, archive) = make_root();
    let db = temp.path().join("opencode.db");
    v1_fixture(&db);
    capture(&archive, &db);
    let key = source_key(&archive);

    // Content-only edit: the watermark column does not move, so the periodic
    // watermark observation is blind to it.
    exec(
        &db,
        "UPDATE part SET data = '{\"type\":\"text\",\"text\":\"secretly edited\"}' WHERE id = 'prt_02';",
    );
    let blind = capture(&archive, &db);
    assert_eq!(blind.change, "skipped_unchanged");
    assert!(sqlite_deltas(&archive, &key, DOMAIN).unwrap().is_empty());

    // `--verify` re-hashes every whitelisted row against the archived content
    // and commits the correction as its own delta unit.
    let verified = capture_opts(
        &archive,
        &db,
        CaptureOptions {
            verify: true,
            ..CaptureOptions::default()
        },
    );
    let info = verified.sqlite.as_ref().unwrap();
    assert!(info.verify);
    assert_eq!(info.verify_mismatches, 1);
    assert_eq!(info.rows_updated, 1);
    assert!(verified.ok);
    assert_eq!(sqlite_state(&archive).verify_mismatches_total, 1);
    assert!(sqlite_state(&archive).last_verify_at_ms > 0);
    // Replay now matches the live database.
    assert_eq!(
        replay_sqlite(&archive, &key, None, DOMAIN).unwrap().tables,
        rows_by_table(&db)
    );

    // The next run without --verify is a clean unchanged observation; the
    // verification timestamp holds the 7-day cycle off.
    let next = capture(&archive, &db);
    assert_eq!(next.change, "skipped_unchanged");
    assert_eq!(next.sqlite.as_ref().unwrap().verify, false);

    // Deletes are never blind: `project` has no watermark column, so the
    // regular hash observation catches the delete as its own delta; the forced
    // verification then finds nothing further to correct.
    exec(&db, "DELETE FROM project WHERE id = 'proj_1';");
    let deleted = capture_opts(
        &archive,
        &db,
        CaptureOptions {
            verify: true,
            ..CaptureOptions::default()
        },
    );
    assert_eq!(deleted.change, "delta");
    assert_eq!(deleted.sqlite.as_ref().unwrap().rows_deleted, 1);
    assert!(deleted.sqlite.as_ref().unwrap().verify);
    assert_eq!(deleted.sqlite.as_ref().unwrap().verify_mismatches, 0);
    assert_eq!(
        replay_sqlite(&archive, &key, None, DOMAIN).unwrap().tables,
        rows_by_table(&db)
    );

    // A content-only edit on a second watermark table is likewise invisible to
    // the watermark path and recovered by verification.
    exec(
        &db,
        "UPDATE message SET data = '{\"role\":\"user\",\"edited\":true}' WHERE id = 'msg_1';",
    );
    let blind2 = capture(&archive, &db);
    assert_eq!(blind2.change, "skipped_unchanged");
    let verified3 = capture_opts(
        &archive,
        &db,
        CaptureOptions {
            verify: true,
            ..CaptureOptions::default()
        },
    );
    assert!(verified3.sqlite.as_ref().unwrap().verify);
    assert_eq!(verified3.sqlite.as_ref().unwrap().verify_mismatches, 1);
    assert_eq!(verified3.sqlite.as_ref().unwrap().rows_updated, 1);
    assert_eq!(sqlite_state(&archive).verify_mismatches_total, 2);
    assert_eq!(
        replay_sqlite(&archive, &key, None, DOMAIN).unwrap().tables,
        rows_by_table(&db)
    );
}

// ---------------------------------------------------------------------------
// 39. A @1 observation state is explicitly untrusted and forces a baseline
// ---------------------------------------------------------------------------
#[test]
fn v1_observation_state_is_rejected_and_rebaselined() {
    use std::io::Write;
    let (temp, archive) = make_root();
    let db = temp.path().join("opencode.db");
    v1_fixture(&db);
    capture(&archive, &db);
    let state = sqlite_state(&archive);
    let path = archive.root().join(&state.observation_file);

    // Replace the @2 state with a @1-shaped file (legacy row-map format).
    let v1_state = json!({
        "schema": "agent-memory/sqlite-observation@1",
        "source": source_key(&archive),
        "capture_seq": 1,
        "observed_at_ms": 1,
        "schema_fingerprint": "legacy",
        "row_digest": "legacy",
        "tables": {}
    });
    let compressed = zstd::stream::encode_all(serde_json::to_vec(&v1_state).unwrap().as_slice(), 3)
        .unwrap();
    let mut file = fs::File::create(&path).unwrap();
    file.write_all(&compressed).unwrap();
    file.sync_all().unwrap();
    drop(file);

    let report = capture(&archive, &db);
    assert!(
        report.anomalies.iter().any(|anomaly| {
            anomaly.contains("sqlite-observation@1") && anomaly.contains("not trusted")
        }),
        "the @1 state must be named explicitly as untrusted: {:?}",
        report.anomalies
    );
    assert_eq!(report.change, "degraded");
    assert!(!report.ok);
    assert_eq!(
        report.sqlite.as_ref().unwrap().snapshot_trigger,
        "snapshot_deduplicated:observation_state_missing"
    );
    // The state is rewritten in the current format and the source heals.
    let healed = capture(&archive, &db);
    assert!(healed.ok);
    assert_eq!(healed.change, "skipped_unchanged");
}

// ---------------------------------------------------------------------------
// 40. No watermark column: full-scan row-hash path (project/todo)
// ---------------------------------------------------------------------------
#[test]
fn tables_without_a_watermark_column_use_row_hashes() {
    let (temp, archive) = make_root();
    let db = temp.path().join("opencode.db");
    v1_fixture(&db);
    capture(&archive, &db);
    let key = source_key(&archive);

    // `project` has no timestamp columns: every observation reads it in full
    // and compares row hashes. Inserts/updates/deletes are found that way.
    exec(&db, "INSERT INTO project VALUES ('proj_2', 'C:/synthetic/other');");
    let insert = capture(&archive, &db);
    assert_eq!(insert.change, "delta");
    assert_eq!(insert.sqlite.as_ref().unwrap().rows_inserted, 1);

    exec(&db, "UPDATE project SET directory = 'C:/moved' WHERE id = 'proj_1';");
    let update = capture(&archive, &db);
    assert_eq!(update.change, "delta");
    assert_eq!(update.sqlite.as_ref().unwrap().rows_updated, 1);

    exec(&db, "DELETE FROM project WHERE id = 'proj_2';");
    let delete = capture(&archive, &db);
    assert_eq!(delete.change, "delta");
    assert_eq!(delete.sqlite.as_ref().unwrap().rows_deleted, 1);

    assert_eq!(
        replay_sqlite(&archive, &key, None, DOMAIN).unwrap().tables,
        rows_by_table(&db)
    );
    // No false delta when nothing changed.
    let unchanged = capture(&archive, &db);
    assert_eq!(unchanged.change, "skipped_unchanged");
}


// ---------------------------------------------------------------------------
// 41. N1: same-watermark-value inserts with smaller keys are new rows
// ---------------------------------------------------------------------------
#[test]
fn same_watermark_value_smaller_keys_are_captured() {
    let (temp, archive) = make_root();
    let db = temp.path().join("opencode.db");
    // 24 rows sharing ONE watermark value; keys p100..p123.
    let conn = Connection::open(&db).unwrap();
    conn.execute_batch(
        r#"
        CREATE TABLE session (
            id TEXT PRIMARY KEY, title TEXT,
            time_created INTEGER, time_updated INTEGER
        );
        CREATE TABLE part (
            id TEXT PRIMARY KEY, message_id TEXT, session_id TEXT,
            time_created INTEGER, time_updated INTEGER, data TEXT
        );
        INSERT INTO session VALUES ('ses_1', 'same-value', 1, 2);
        "#,
    )
    .unwrap();
    for index in 100..124 {
        conn.execute(
            "INSERT INTO part VALUES (?1, 'm1', 'ses_1', 1000, 1000, 'x')",
            rusqlite::params![format!("p{index}")],
        )
        .unwrap();
    }
    drop(conn);
    capture(&archive, &db);
    let key = source_key(&archive);

    // Insert 100 rows AFTER the observation: every key sorts below the
    // smallest existing key (p100) and shares its watermark value 1000.
    exec(
        &db,
        &(0..100)
            .map(|index| {
                format!(
                    "INSERT INTO part VALUES ('p{index:03}', 'm1', 'ses_1', 1000, 1000, 'x');"
                )
            })
            .collect::<Vec<_>>()
            .join("\n"),
    );

    let report = capture(&archive, &db);
    let info = report.sqlite.as_ref().unwrap();
    assert_eq!(report.change, "delta", "{report:?}");
    assert_eq!(info.rows_inserted, 100, "unseen same-value keys are inserts");
    assert_eq!(info.rows_updated, 0);
    let replay = replay_sqlite(&archive, &key, None, DOMAIN).unwrap();
    assert_eq!(replay.tables["part"].len(), 124);
    assert_eq!(replay.tables["part"], rows_by_table(&db)["part"]);
}

// ---------------------------------------------------------------------------
// 42. N2: a session re-entering the filter scope backfills its child rows
// ---------------------------------------------------------------------------
#[test]
fn session_reentering_scope_backfills_child_rows() {
    let (temp, archive) = make_root();
    let db = temp.path().join("opencode.db");
    session_spread_fixture(&db);
    capture_opts(&archive, &db, options_session_since(4_000));
    let key = source_key(&archive);

    // The old session (t=100) becomes active again and re-enters the scope.
    exec(&db, "UPDATE session SET time_updated = 7000 WHERE id = 'ses_old';");
    let report = capture_opts(&archive, &db, options_session_since(4_000));
    let info = report.sqlite.as_ref().unwrap();
    assert_eq!(report.change, "delta", "{report:?}");
    assert_eq!(
        info.rows_inserted, 3,
        "session + its existing message + its existing part, one shot"
    );
    assert_eq!(info.rows_deleted, 0, "no false deletes for scope changes");

    let replay = replay_sqlite(&archive, &key, None, DOMAIN).unwrap();
    assert!(replay.tables["session"].contains_key("pk:[\"ses_old\"]"));
    assert_eq!(replay.tables["message"].len(), 3);
    assert_eq!(replay.tables["part"].len(), 3);
    assert_eq!(replay.tables["session"].len(), 3);
}

// ---------------------------------------------------------------------------
// 43. N3: a child table without session_id is captured in full and reported
// ---------------------------------------------------------------------------
#[test]
fn child_table_without_session_id_is_captured_in_full_and_reported() {
    let (temp, archive) = make_root();
    let db = temp.path().join("opencode.db");
    let conn = Connection::open(&db).unwrap();
    conn.execute_batch(
        r#"
        CREATE TABLE session (
            id TEXT PRIMARY KEY, title TEXT,
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
        -- Whitelist child table WITHOUT session_id (weird shape on purpose).
        CREATE TABLE todo (
            id TEXT PRIMARY KEY, content TEXT
        );
        INSERT INTO session VALUES ('ses_new', 'n', 1, 5000);
        INSERT INTO message VALUES ('msg_new', 'ses_new', 1, 5000, 'm');
        INSERT INTO part VALUES ('prt_new', 'msg_new', 'ses_new', 1, 5000, 'p');
        INSERT INTO todo VALUES ('todo_1', 'unscoped');
        "#,
    )
    .unwrap();
    drop(conn);

    let report = capture_opts(&archive, &db, options_session_since(4_000));
    assert!(report.ok, "{report:?}");
    let info = report.sqlite.as_ref().unwrap();
    assert_eq!(
        info.unfiltered_tables,
        vec!["todo".to_string()],
        "the session-less child table is named, never silently scoped"
    );
    // todo rows are fully captured although the filter is in force.
    let replay = replay_sqlite(&archive, &source_key(&archive), None, DOMAIN).unwrap();
    assert!(replay.tables["todo"].contains_key("pk:[\"todo_1\"]"));
    assert_eq!(replay.tables["session"].len(), 1);
    // Coverage reports the same fact.
    let coverage = archive.coverage().unwrap();
    assert_eq!(
        coverage.sources[0].sqlite.as_ref().unwrap().unfiltered_tables,
        vec!["todo".to_string()]
    );

    // A later write to the unfiltered table is still a delta.
    exec(&db, "INSERT INTO todo VALUES ('todo_2', 'more');");
    let delta = capture_opts(&archive, &db, options_session_since(4_000));
    assert_eq!(delta.change, "delta");
    assert_eq!(delta.sqlite.as_ref().unwrap().rows_inserted, 1);
}

// ---------------------------------------------------------------------------
// 44. B1: --verify reuses the --session-since scope
// ---------------------------------------------------------------------------
#[test]
fn verify_honors_session_since_filter() {
    let (temp, archive) = make_root();
    let db = temp.path().join("opencode.db");
    let conn = Connection::open(&db).unwrap();
    conn.execute_batch(
        r#"
        CREATE TABLE session (
            id TEXT PRIMARY KEY, title TEXT,
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
        INSERT INTO project VALUES ('proj_1', 'C:/w');
        INSERT INTO session VALUES ('ses_old', 'o', 1, 100);
        INSERT INTO session VALUES ('ses_new', 'n', 1, 5000);
        INSERT INTO message VALUES ('msg_o1', 'ses_old', 1, 100, 'o1');
        INSERT INTO message VALUES ('msg_o2', 'ses_old', 2, 100, 'o2');
        INSERT INTO message VALUES ('msg_n1', 'ses_new', 1, 5000, 'n1');
        INSERT INTO part VALUES ('prt_o1', 'msg_o1', 'ses_old', 1, 100, 'o');
        INSERT INTO part VALUES ('prt_o2', 'msg_o2', 'ses_old', 2, 100, 'o');
        INSERT INTO part VALUES ('prt_n1', 'msg_n1', 'ses_new', 1, 5000, 'n');
        "#,
    )
    .unwrap();
    drop(conn);

    // Baseline under the filter: only ses_new + its children + project.
    let filtered = capture_opts(&archive, &db, options_session_since(4_000));
    assert_eq!(filtered.sqlite.as_ref().unwrap().rows_discovered, 4);
    let key = source_key(&archive);
    let scope_counts = |archive: &Archive| {
        let replay = replay_sqlite(archive, &key, None, DOMAIN).unwrap();
        (
            replay.tables["session"].len(),
            replay.tables["message"].len(),
            replay.tables["part"].len(),
        )
    };
    assert_eq!(scope_counts(&archive), (1, 1, 1));

    // Forced verification inside the filter: the out-of-scope rows must NOT
    // be "corrected" into the evidence (B1 regression), and the in-scope rows
    // are consistent, so nothing is written at all.
    // The verify run passes the SAME filter (N6: omitting it would be a
    // filter change and re-baseline).
    let verified = capture_opts(
        &archive,
        &db,
        CaptureOptions {
            session_since_ms: Some(4_000),
            verify: true,
            ..CaptureOptions::default()
        },
    );
    assert!(verified.ok, "{verified:?}");
    let info = verified.sqlite.as_ref().unwrap();
    assert!(
        info.verify,
        "verify must run: change={} trigger={:?}",
        verified.change,
        verified.sqlite.as_ref().map(|i| i.snapshot_trigger.clone())
    );
    assert_eq!(info.verify_mismatches, 0, "no out-of-scope hallucination");
    assert_eq!(info.mode, "unchanged", "no correction unit for scope change");
    assert_eq!(scope_counts(&archive), (1, 1, 1), "old rows stay out");
    assert!(sqlite_deltas(&archive, &key, DOMAIN).unwrap().is_empty());

    // A blind (watermark-invisible) content edit inside the scope is found by
    // verification and recorded as a correction delta.
    exec(&db, "UPDATE message SET data = 'n1-edited' WHERE id = 'msg_n1';");
    let blind = capture_opts(&archive, &db, options_session_since(4_000));
    assert_eq!(blind.change, "skipped_unchanged");
    let corrected = capture_opts(
        &archive,
        &db,
        CaptureOptions {
            session_since_ms: Some(4_000),
            verify: true,
            ..CaptureOptions::default()
        },
    );
    let info = corrected.sqlite.as_ref().unwrap();
    assert!(info.verify, "{corrected:?}");
    assert_eq!(info.verify_mismatches, 1);
    assert_eq!(info.rows_updated, 1);
    assert_eq!(scope_counts(&archive), (1, 1, 1));

    // A regular in-scope insert afterwards is a normal watermark delta — and
    // no out-of-scope row ever enters the evidence, and no out-of-scope delete
    // is recorded although the source holds rows the archive does not.
    exec(
        &db,
        "INSERT INTO message VALUES ('msg_n2', 'ses_new', 2, 6000, 'n2');",
    );
    let delta = capture_opts(&archive, &db, options_session_since(4_000));
    assert_eq!(delta.change, "delta");
    assert_eq!(delta.sqlite.as_ref().unwrap().rows_inserted, 1);
    assert_eq!(delta.sqlite.as_ref().unwrap().rows_deleted, 0);

    // Replay equals the source under the same filter, row by row.
    let replay = replay_sqlite(&archive, &key, None, DOMAIN).unwrap();
    let live = rows_by_table(&db);
    let selected: BTreeSet<String> = live["session"]
        .values()
        .filter(|row| row["time_updated"].as_i64().unwrap_or(0) >= 4_000)
        .filter_map(|row| row["id"].as_str().map(str::to_string))
        .collect();
    let scoped = |table: &str| -> BTreeMap<String, Value> {
        live[table]
            .iter()
            .filter(|(_, row)| {
                if table == "session" || table == "session_v2" {
                    row.get("time_updated").and_then(Value::as_i64).unwrap_or(0) >= 4_000
                } else if table == "project" {
                    true
                } else {
                    row.get("session_id")
                        .and_then(Value::as_str)
                        .map(|id| selected.contains(id))
                        .unwrap_or(false)
                }
            })
            .map(|(key, row)| (key.clone(), row.clone()))
            .collect()
    };
    assert_eq!(replay.tables["session"], scoped("session"));
    assert_eq!(replay.tables["message"], scoped("message"));
    assert_eq!(replay.tables["part"], scoped("part"));
    assert_eq!(replay.tables["project"], live["project"]);
    for batch in sqlite_deltas(&archive, &key, DOMAIN).unwrap() {
        for line in &batch.lines {
            assert!(
                !line.key.contains("ses_old")
                    && !line.key.contains("msg_o")
                    && !line.key.contains("prt_o"),
                "out-of-scope keys must never appear in deltas: {line:?}"
            );
        }
    }
}


// ---------------------------------------------------------------------------
// 46. R1: backfill never duplicates rows the regular read already collected
// ---------------------------------------------------------------------------
/// Shared shape for the three R1 scenarios: `old` (out of scope, t=1000) and
/// `cur` (in scope, t=5000) sessions, each with `children` messages+parts.
fn r1_fixture(path: &Path, cur_children: usize) {
    let conn = Connection::open(path).unwrap();
    conn.execute_batch(
        r#"
        CREATE TABLE session (
            id TEXT PRIMARY KEY, title TEXT,
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
        INSERT INTO session VALUES ('ses_old', 'o', 1, 1000);
        INSERT INTO session VALUES ('ses_cur', 'c', 1, 5000);
        "#,
    )
    .unwrap();
    for index in 0..cur_children {
        conn.execute(
            "INSERT INTO message VALUES (?1, 'ses_cur', ?2, ?2, 'c')",
            rusqlite::params![format!("m_cur_{index:03}"), 5000 + index as i64],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO part VALUES (?1, ?2, 'ses_cur', ?3, ?3, 'c')",
            rusqlite::params![format!("p_cur_{index:03}"), format!("m_cur_{index:03}"), 5000 + index as i64],
        )
        .unwrap();
    }
    // Two out-of-scope children of the old session (below the watermark).
    conn.execute("INSERT INTO message VALUES ('m_old_1', 'ses_old', 1000, 1000, 'o1')", []).unwrap();
    conn.execute("INSERT INTO part VALUES ('p_old_1', 'm_old_1', 'ses_old', 1000, 1000, 'o1')", []).unwrap();
}

fn assert_no_phantom_keys(archive: &Archive, key: &str) {
    let replay = replay_sqlite(archive, key, None, DOMAIN).unwrap();
    for (table, rows) in &replay.tables {
        for row_key in rows.keys() {
            assert!(
                !row_key.contains("#rowid:") && !row_key.contains("#dup:"),
                "phantom disambiguated key in {table}: {row_key}"
            );
        }
    }
    for batch in sqlite_deltas(archive, key, DOMAIN).unwrap() {
        for line in &batch.lines {
            assert!(
                !line.key.contains("#rowid:") && !line.key.contains("#dup:"),
                "phantom disambiguated key in delta: {line:?}"
            );
        }
    }
}

#[test]
fn backfill_reentry_with_new_child_rows_has_no_phantoms() {
    let (temp, archive) = make_root();
    let db = temp.path().join("opencode.db");
    r1_fixture(&db, 30);
    capture_opts(&archive, &db, options_session_since(4_000));
    let key = source_key(&archive);

    // Re-entry: the old session gets new (above-watermark) children AND its
    // row crosses the threshold in the same window.
    exec(
        &db,
        "INSERT INTO message VALUES ('m_old_new', 'ses_old', 9000, 9000, 'new');
         INSERT INTO part VALUES ('p_old_new', 'm_old_new', 'ses_old', 9000, 9000, 'new');
         UPDATE session SET time_updated = 9000 WHERE id = 'ses_old';",
    );
    let report = capture_opts(&archive, &db, options_session_since(4_000));
    let info = report.sqlite.as_ref().unwrap();
    assert_eq!(report.change, "delta", "{report:?}");
    assert_eq!(
        info.rows_inserted, 5,
        "session + 2 new children (watermark) + 2 old children (backfill)"
    );

    // Replay matches the source under the same filter, row by row, with no
    // phantom disambiguated keys.
    let replay = replay_sqlite(&archive, &key, None, DOMAIN).unwrap();
    let live = rows_by_table(&db);
    assert_eq!(replay.tables["session"], live["session"]);
    assert_eq!(replay.tables["message"], live["message"]);
    assert_eq!(replay.tables["part"], live["part"]);
    assert_no_phantom_keys(&archive, &key);

    // Next cycle: no insert/delete jitter.
    let next = capture_opts(&archive, &db, options_session_since(4_000));
    assert_eq!(next.change, "skipped_unchanged", "{next:?}");
    assert_eq!(next.sqlite.as_ref().unwrap().delta_rows, 0);
}

#[test]
fn backfill_recreated_same_id_session_has_no_phantoms() {
    // Self-contained shape: both sessions are inside the scope, so the final
    // replay must equal the whole live database.
    let (temp, archive) = make_root();
    let db = temp.path().join("opencode.db");
    let conn = Connection::open(&db).unwrap();
    conn.execute_batch(
        r#"
        CREATE TABLE session (
            id TEXT PRIMARY KEY, title TEXT,
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
        INSERT INTO session VALUES ('ses_cur', 'c', 1, 5000);
        INSERT INTO session VALUES ('ses_x', 'x', 1, 5000);
        INSERT INTO message VALUES ('m_x1', 'ses_x', 5000, 5000, 'x1');
        "#,
    )
    .unwrap();
    for index in 0..5 {
        conn.execute(
            "INSERT INTO message VALUES (?1, 'ses_cur', ?2, ?2, 'c')",
            rusqlite::params![format!("m_cur_{index:03}"), 5000 + index as i64],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO part VALUES (?1, ?2, 'ses_cur', ?3, ?3, 'c')",
            rusqlite::params![
                format!("p_cur_{index:03}"),
                format!("m_cur_{index:03}"),
                5000 + index as i64
            ],
        )
        .unwrap();
    }
    drop(conn);
    capture_opts(&archive, &db, options_session_since(4_000));
    let key = source_key(&archive);

    // Delete the session (its child is deleted with it) and recreate it under
    // the same id with one above-watermark child and one below-watermark child.
    exec(
        &db,
        "DELETE FROM session WHERE id = 'ses_x';
         DELETE FROM message WHERE session_id = 'ses_x';
         INSERT INTO session VALUES ('ses_x', 'recreated', 2, 6000);
         INSERT INTO message VALUES ('m_x2', 'ses_x', 6000, 6000, 'x2');
         INSERT INTO message VALUES ('m_x3', 'ses_x', 100, 100, 'x3');",
    );
    let report = capture_opts(&archive, &db, options_session_since(4_000));
    let info = report.sqlite.as_ref().unwrap();
    assert_eq!(report.change, "delta", "{report:?}");
    assert_eq!(info.rows_deleted, 1, "the old m_x1 row leaves the archive");
    assert_eq!(
        info.rows_inserted, 2,
        "both new messages (one above the watermark, one via backfill)"
    );
    assert_eq!(info.rows_updated, 1, "the recreated session row is an update");

    let replay = replay_sqlite(&archive, &key, None, DOMAIN).unwrap();
    let live = rows_by_table(&db);
    assert_eq!(replay.tables["session"], live["session"]);
    assert_eq!(replay.tables["message"], live["message"]);
    assert_eq!(replay.tables["part"], live["part"]);
    assert_no_phantom_keys(&archive, &key);

    let next = capture_opts(&archive, &db, options_session_since(4_000));
    assert_eq!(next.change, "skipped_unchanged", "{next:?}");
}

#[test]
fn backfill_child_modified_while_session_out_of_scope_has_no_phantoms() {
    let (temp, archive) = make_root();
    let db = temp.path().join("opencode.db");
    r1_fixture(&db, 5);
    capture_opts(&archive, &db, options_session_since(4_000));
    let key = source_key(&archive);

    // While the old session is out of scope, one of its children is modified
    // (content and watermark); afterwards the session itself enters scope.
    exec(
        &db,
        "UPDATE part SET data = 'changed', time_updated = 8000 WHERE id = 'p_old_1';
         UPDATE session SET time_updated = 8000 WHERE id = 'ses_old';",
    );
    let report = capture_opts(&archive, &db, options_session_since(4_000));
    let info = report.sqlite.as_ref().unwrap();
    assert_eq!(report.change, "delta", "{report:?}");
    assert_eq!(
        info.rows_inserted, 3,
        "session + modified part (watermark read) + old message (backfill)"
    );

    let replay = replay_sqlite(&archive, &key, None, DOMAIN).unwrap();
    let live = rows_by_table(&db);
    assert_eq!(replay.tables["session"], live["session"]);
    assert_eq!(replay.tables["message"], live["message"]);
    assert_eq!(replay.tables["part"], live["part"]);
    assert_eq!(
        replay.tables["part"][&"pk:[\"p_old_1\"]".to_string()]["data"],
        json!("changed")
    );
    assert_no_phantom_keys(&archive, &key);

    let next = capture_opts(&archive, &db, options_session_since(4_000));
    assert_eq!(next.change, "skipped_unchanged", "{next:?}");
}

// ---------------------------------------------------------------------------
// 47. R2: the streaming merge compares keys in SQLite order
// ---------------------------------------------------------------------------
/// One blind insert (id=9, older watermark value) into a fresh table shape;
/// verification must report exactly one mismatch and correct exactly that row.
fn assert_single_blind_insert_is_one_correction(verify_options: CaptureOptions) {
    // 1. INTEGER primary key with cross-digit ids (1..=30, missing 9).
    let (temp, archive) = make_root();
    let db = temp.path().join("opencode.db");
    let conn = Connection::open(&db).unwrap();
    conn.execute_batch(
        r#"
        CREATE TABLE session (
            id TEXT PRIMARY KEY, title TEXT,
            time_created INTEGER, time_updated INTEGER
        );
        CREATE TABLE message (
            id INTEGER PRIMARY KEY, session_id TEXT,
            time_created INTEGER, time_updated INTEGER, data TEXT
        );
        INSERT INTO session VALUES ('ses_1', 's', 1, 2);
        "#,
    )
    .unwrap();
    for id in 1..=30i64 {
        if id == 9 {
            continue;
        }
        conn.execute(
            "INSERT INTO message VALUES (?1, 'ses_1', 100, 100, 'x')",
            rusqlite::params![id],
        )
        .unwrap();
    }
    drop(conn);
    capture(&archive, &db);
    exec(
        &db,
        "INSERT INTO message VALUES (9, 'ses_1', 50, 50, 'late-nine');",
    );
    let blind = capture(&archive, &db);
    assert_eq!(blind.change, "skipped_unchanged");
    let verified = capture_opts(&archive, &db, verify_options);
    let info = verified.sqlite.as_ref().unwrap();
    assert!(info.verify);
    assert_eq!(info.verify_mismatches, 1, "int pk: {verified:?}");
    assert_eq!(info.rows_inserted, 1);
    assert_eq!(
        replay_sqlite(&archive, &source_key(&archive), None, DOMAIN)
            .unwrap()
        .tables["message"]
            .len(),
        30
    );
}

#[test]
fn verify_merge_matches_sqlite_order_for_integer_keys() {
    assert_single_blind_insert_is_one_correction(CaptureOptions {
        verify: true,
        ..CaptureOptions::default()
    });
}

#[test]
fn verify_merge_matches_sqlite_order_for_composite_keys() {
    // (session_id, position) composite INTEGER pk, OpenCode `todo` shape:
    // rows (s,0..=24) minus (s,9); one blind insert of (s,9).
    let (temp, archive) = make_root();
    let db = temp.path().join("opencode.db");
    let conn = Connection::open(&db).unwrap();
    conn.execute_batch(
        r#"
        CREATE TABLE session (
            id TEXT PRIMARY KEY, title TEXT,
            time_created INTEGER, time_updated INTEGER
        );
        CREATE TABLE todo (
            session_id TEXT NOT NULL,
            position INTEGER NOT NULL,
            time_updated INTEGER,
            content TEXT,
            PRIMARY KEY (session_id, position)
        );
        INSERT INTO session VALUES ('ses_1', 's', 1, 2);
        "#,
    )
    .unwrap();
    for position in 0..=24i64 {
        if position == 9 {
            continue;
        }
        conn.execute(
            "INSERT INTO todo VALUES ('ses_1', ?1, 100, 'x')",
            rusqlite::params![position],
        )
        .unwrap();
    }
    drop(conn);
    capture(&archive, &db);
    exec(&db, "INSERT INTO todo VALUES ('ses_1', 9, 50, 'late-nine');");
    let blind = capture(&archive, &db);
    assert_eq!(blind.change, "skipped_unchanged");
    let verified = capture_opts(
        &archive,
        &db,
        CaptureOptions {
            verify: true,
            ..CaptureOptions::default()
        },
    );
    let info = verified.sqlite.as_ref().unwrap();
    assert!(info.verify);
    assert_eq!(info.verify_mismatches, 1, "composite pk: {verified:?}");
    assert_eq!(info.rows_inserted, 1);
    // 24 baseline rows (0..=24 minus 9) + the corrected insert = 25.
    assert_eq!(
        replay_sqlite(&archive, &source_key(&archive), None, DOMAIN)
            .unwrap()
        .tables["todo"]
            .len(),
        25
    );
}

#[test]
fn verify_merge_matches_sqlite_order_for_rowid_tables() {
    // No declared primary key: canonical keys are `rowid:N`; ids 1..=25 as an
    // INTEGER `id` column (not a pk) minus 9, one blind insert of 9.
    let (temp, archive) = make_root();
    let db = temp.path().join("opencode.db");
    let conn = Connection::open(&db).unwrap();
    conn.execute_batch(
        r#"
        CREATE TABLE session (
            id TEXT PRIMARY KEY, title TEXT,
            time_created INTEGER, time_updated INTEGER
        );
        CREATE TABLE message (
            id INTEGER, session_id TEXT,
            time_created INTEGER, time_updated INTEGER, data TEXT
        );
        INSERT INTO session VALUES ('ses_1', 's', 1, 2);
        "#,
    )
    .unwrap();
    for id in 1..=25i64 {
        if id == 9 {
            continue;
        }
        conn.execute(
            "INSERT INTO message VALUES (?1, 'ses_1', 100, 100, 'x')",
            rusqlite::params![id],
        )
        .unwrap();
    }
    drop(conn);
    capture(&archive, &db);
    exec(&db, "INSERT INTO message VALUES (9, 'ses_1', 50, 50, 'late-nine');");
    let blind = capture(&archive, &db);
    assert_eq!(blind.change, "skipped_unchanged");
    let verified = capture_opts(
        &archive,
        &db,
        CaptureOptions {
            verify: true,
            ..CaptureOptions::default()
        },
    );
    let info = verified.sqlite.as_ref().unwrap();
    assert!(info.verify);
    assert_eq!(info.verify_mismatches, 1, "rowid keys: {verified:?}");
    assert_eq!(info.rows_inserted, 1);
    assert_eq!(
        replay_sqlite(&archive, &source_key(&archive), None, DOMAIN)
            .unwrap()
        .tables["message"]
            .len(),
        25
    );
}

#[test]
fn verify_merge_matches_sqlite_order_for_non_ascii_text_keys() {
    let (temp, archive) = make_root();
    let db = temp.path().join("opencode.db");
    let conn = Connection::open(&db).unwrap();
    conn.execute_batch(
        r#"
        CREATE TABLE session (
            id TEXT PRIMARY KEY, title TEXT,
            time_created INTEGER, time_updated INTEGER
        );
        CREATE TABLE part (
            id TEXT PRIMARY KEY, message_id TEXT, session_id TEXT,
            time_created INTEGER, time_updated INTEGER, data TEXT
        );
        INSERT INTO session VALUES ('ses_1', 's', 1, 2);
        "#,
    )
    .unwrap();
    // Non-ASCII and tricky ids; every one sorts before 'é…' in BINARY order.
    for id in ["a", "A", "a b", "z", "中", "éprior"] {
        conn.execute(
            "INSERT INTO part VALUES (?1, 'm1', 'ses_1', 100, 100, 'x')",
            rusqlite::params![id],
        )
        .unwrap();
    }
    drop(conn);
    capture(&archive, &db);
    exec(&db, "INSERT INTO part VALUES ('élate', 'm1', 'ses_1', 50, 50, 'late');");
    let blind = capture(&archive, &db);
    assert_eq!(blind.change, "skipped_unchanged");
    let verified = capture_opts(
        &archive,
        &db,
        CaptureOptions {
            verify: true,
            ..CaptureOptions::default()
        },
    );
    let info = verified.sqlite.as_ref().unwrap();
    assert!(info.verify);
    assert_eq!(info.verify_mismatches, 1, "text pk: {verified:?}");
    assert_eq!(info.rows_inserted, 1);
}

#[test]
fn verify_merge_matches_sqlite_order_for_mixed_type_keys() {
    // Composite (kind TEXT, position INTEGER) pk; ids (x,0..=24) minus (x,9);
    // one blind insert of (x,9) plus one of (中,9) to cross type ranks.
    let (temp, archive) = make_root();
    let db = temp.path().join("opencode.db");
    let conn = Connection::open(&db).unwrap();
    conn.execute_batch(
        r#"
        CREATE TABLE session (
            id TEXT PRIMARY KEY, title TEXT,
            time_created INTEGER, time_updated INTEGER
        );
        CREATE TABLE todo (
            kind TEXT NOT NULL,
            position INTEGER NOT NULL,
            time_updated INTEGER,
            content TEXT,
            PRIMARY KEY (kind, position)
        );
        INSERT INTO session VALUES ('ses_1', 's', 1, 2);
        "#,
    )
    .unwrap();
    for position in 0..=24i64 {
        if position == 9 {
            continue;
        }
        conn.execute(
            "INSERT INTO todo VALUES ('x', ?1, 100, 'x')",
            rusqlite::params![position],
        )
        .unwrap();
    }
    drop(conn);
    capture(&archive, &db);
    exec(
        &db,
        "INSERT INTO todo VALUES ('x', 9, 50, 'late-nine');
         INSERT INTO todo VALUES ('中', 9, 50, 'late-kanji');",
    );
    let blind = capture(&archive, &db);
    assert_eq!(blind.change, "skipped_unchanged");
    let verified = capture_opts(
        &archive,
        &db,
        CaptureOptions {
            verify: true,
            ..CaptureOptions::default()
        },
    );
    let info = verified.sqlite.as_ref().unwrap();
    assert!(info.verify);
    assert_eq!(info.verify_mismatches, 2, "mixed pk: {verified:?}");
    assert_eq!(info.rows_inserted, 2);
    // 24 baseline rows (0..=24 minus 9) + both corrected inserts = 26.
    assert_eq!(
        replay_sqlite(&archive, &source_key(&archive), None, DOMAIN)
            .unwrap()
        .tables["todo"]
            .len(),
        26
    );
}

#[test]
fn verify_blind_update_is_one_mismatch_for_integer_keys() {
    // Same table shape as the integer case, but a blind content UPDATE of an
    // existing row (id 10): one mismatch, one update correction.
    let (temp, archive) = make_root();
    let db = temp.path().join("opencode.db");
    let conn = Connection::open(&db).unwrap();
    conn.execute_batch(
        r#"
        CREATE TABLE session (
            id TEXT PRIMARY KEY, title TEXT,
            time_created INTEGER, time_updated INTEGER
        );
        CREATE TABLE message (
            id INTEGER PRIMARY KEY, session_id TEXT,
            time_created INTEGER, time_updated INTEGER, data TEXT
        );
        INSERT INTO session VALUES ('ses_1', 's', 1, 2);
        "#,
    )
    .unwrap();
    for id in 1..=30i64 {
        conn.execute(
            "INSERT INTO message VALUES (?1, 'ses_1', 100, 100, 'x')",
            rusqlite::params![id],
        )
        .unwrap();
    }
    drop(conn);
    capture(&archive, &db);
    exec(&db, "UPDATE message SET data = 'blind-edit' WHERE id = 10;");
    let blind = capture(&archive, &db);
    assert_eq!(blind.change, "skipped_unchanged");
    let verified = capture_opts(
        &archive,
        &db,
        CaptureOptions {
            verify: true,
            ..CaptureOptions::default()
        },
    );
    let info = verified.sqlite.as_ref().unwrap();
    assert!(info.verify);
    assert_eq!(info.verify_mismatches, 1, "blind update: {verified:?}");
    assert_eq!(info.rows_updated, 1);
    assert_eq!(
        replay_sqlite(&archive, &source_key(&archive), None, DOMAIN)
            .unwrap()
        .tables["message"][&"pk:[10]".to_string()]["data"],
        json!("blind-edit")
    );
}

#[test]
fn verify_non_binary_collation_table_takes_bounded_path() {
    // A NOCASE primary key orders differently from BINARY bytes: the collation
    // detector routes the table to the bounded path and names it in the report.
    let (temp, archive) = make_root();
    let db = temp.path().join("opencode.db");
    let conn = Connection::open(&db).unwrap();
    conn.execute_batch(
        r#"
        CREATE TABLE session (
            id TEXT PRIMARY KEY, title TEXT,
            time_created INTEGER, time_updated INTEGER
        );
        CREATE TABLE message (
            id TEXT PRIMARY KEY COLLATE NOCASE, session_id TEXT,
            time_created INTEGER, time_updated INTEGER, data TEXT
        );
        INSERT INTO session VALUES ('ses_1', 's', 1, 2);
        "#,
    )
    .unwrap();
    for id in ["alpha", "Beta", "gamma"] {
        conn.execute(
            "INSERT INTO message VALUES (?1, 'ses_1', 100, 100, 'x')",
            rusqlite::params![id],
        )
        .unwrap();
    }
    drop(conn);
    capture(&archive, &db);
    exec(&db, "INSERT INTO message VALUES ('late', 'ses_1', 50, 50, 'late');");
    let blind = capture(&archive, &db);
    assert_eq!(blind.change, "skipped_unchanged");
    let verified = capture_opts(
        &archive,
        &db,
        CaptureOptions {
            verify: true,
            ..CaptureOptions::default()
        },
    );
    let info = verified.sqlite.as_ref().unwrap();
    assert!(info.verify);
    assert_eq!(info.verify_mismatches, 1, "{verified:?}");
    assert_eq!(info.rows_inserted, 1);
    assert_eq!(
        info.verify_bounded_tables,
        vec!["message".to_string()],
        "the NOCASE table is named as bounded: {verified:?}"
    );
    assert_eq!(
        replay_sqlite(&archive, &source_key(&archive), None, DOMAIN)
            .unwrap()
        .tables["message"]
            .len(),
        4
    );
}

// ---------------------------------------------------------------------------
// 48. Crash-adopt window: the automatic post-adopt verification heals deletes
// and low-key same-value inserts (R2 data-loss path + N-R4)
// ---------------------------------------------------------------------------
#[test]
fn adopt_window_deletes_and_low_key_inserts_are_verified() {
    let (temp, archive) = make_root();
    let db = temp.path().join("opencode.db");
    v1_fixture(&db);
    capture(&archive, &db);
    let key = source_key(&archive);

    // Force a rebaseline that commits but crashes before the state write; the
    // rerun adopts the committed unit (degraded).
    let crashed = std::panic::catch_unwind(AssertUnwindSafe(|| {
        let _ = capture_source_with_options(
            &archive,
            &db,
            "opencode",
            DOMAIN,
            Some(Failpoint::AfterManifestBeforeState),
            CaptureOptions {
                rebaseline: true,
                ..CaptureOptions::default()
            },
        );
    }));
    assert!(crashed.is_err(), "the failpoint must fire");
    let adopted = capture(&archive, &db);
    assert_eq!(adopted.change, "recovered_after_crash", "{adopted:?}");
    assert!(!adopted.ok);

    // Inside the adopt window: delete one archived row and the highest-key
    // part (whose delete correction only the merge's drain can produce), and
    // insert a low-key same-value row. The regular observation cannot see any
    // of this: the adopted state's key sets are empty and untrusted.
    exec(
        &db,
        "DELETE FROM part WHERE id IN ('prt_05', 'prt_24');
         INSERT INTO part VALUES ('prt_000','msg_1','ses_1',100,100,'{\"text\":\"early\"}');",
    );
    let incr = capture(&archive, &db);
    assert_eq!(incr.change, "delta", "the verification runs automatically: {incr:?}");
    let info = incr.sqlite.as_ref().unwrap();
    assert!(info.verify, "N-R4: the post-adopt capture verifies itself");
    assert_eq!(info.verify_mismatches, 3, "{incr:?}");
    assert_eq!(info.rows_deleted, 2);
    assert_eq!(info.rows_inserted, 1);
    // Replay equals the live database after the healing verification.
    assert_eq!(
        replay_sqlite(&archive, &key, None, DOMAIN).unwrap().tables,
        rows_by_table(&db)
    );
    assert_no_phantom_keys(&archive, &key);

    // The next cycle is clean again.
    let next = capture(&archive, &db);
    assert_eq!(next.change, "skipped_unchanged", "{next:?}");
}

// ---------------------------------------------------------------------------
// 49. N-R3: delta batches without recorded ranges are always read
// ---------------------------------------------------------------------------
#[test]
fn legacy_delta_batches_without_ranges_still_provide_before_images() {
    use std::io::Write;
    let (temp, archive) = make_root();
    let db = temp.path().join("opencode.db");
    v1_fixture(&db);
    capture(&archive, &db);
    let key = source_key(&archive);

    // gen2: first content update of prt_05.
    exec(
        &db,
        "UPDATE part SET data = 'v1-5-unique', time_updated = 401 WHERE id = 'prt_05';",
    );
    capture(&archive, &db);

    // Strip `source.sqlite.delta_table_ranges` from every committed manifest (legacy shape).
    let manifests_dir = archive.root().join("archive").join("manifests");
    let mut cleared = 0;
    for entry in std::fs::read_dir(&manifests_dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let mut value: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        let mut touched = false;
        let ranges = &mut value["source"]["sqlite"]["delta_table_ranges"];
        if ranges.as_object().is_some_and(|map| !map.is_empty()) {
            *ranges = json!({});
            touched = true;
        }
        if touched {
            cleared += 1;
            let mut file = fs::File::create(&path).unwrap();
            file.write_all(serde_json::to_string_pretty(&value).unwrap().as_bytes())
                .unwrap();
        }
    }

    assert!(cleared > 0, "the test must really clear at least one manifest's delta ranges");

    // gen3: second update of the same row. Its `before` must come from the
    // range-less gen2 batch (v1 value), not from the baseline (v0 value).
    exec(
        &db,
        "UPDATE part SET data = 'v2-5-unique', time_updated = 402 WHERE id = 'prt_05';",
    );
    let report = capture(&archive, &db);
    assert_eq!(report.change, "delta", "{report:?}");
    let batches = sqlite_deltas(&archive, &key, DOMAIN).unwrap();
    let last = batches.last().unwrap();
    let update = last
        .lines
        .iter()
        .find(|line| line.op == "update")
        .expect("an update line");
    assert_eq!(
        update.before.as_ref().unwrap()["data"],
        json!("v1-5-unique"),
        "the before image must come from the previous delta batch, not the baseline"
    );
    assert_eq!(
        replay_sqlite(&archive, &key, None, DOMAIN).unwrap().tables,
        rows_by_table(&db)
    );
}

#[test]
fn verify_merge_is_idempotent_for_integer_keys() {
    // The integer-pk table runs the streaming merge (typed comparator keeps
    // both streams monotone, so no bounded fallback is named); running
    // --verify twice must report zero mismatches the second time and leave
    // the replay complete.
    let (temp, archive) = make_root();
    let db = temp.path().join("opencode.db");
    let conn = Connection::open(&db).unwrap();
    conn.execute_batch(
        r#"
        CREATE TABLE session (
            id TEXT PRIMARY KEY, title TEXT,
            time_created INTEGER, time_updated INTEGER
        );
        CREATE TABLE message (
            id INTEGER PRIMARY KEY, session_id TEXT,
            time_created INTEGER, time_updated INTEGER, data TEXT
        );
        INSERT INTO session VALUES ('ses_1', 's', 1, 2);
        "#,
    )
    .unwrap();
    for id in 1..=30i64 {
        if id == 9 {
            continue;
        }
        conn.execute(
            "INSERT INTO message VALUES (?1, 'ses_1', 100, 100, 'x')",
            rusqlite::params![id],
        )
        .unwrap();
    }
    drop(conn);
    capture(&archive, &db);
    exec(&db, "INSERT INTO message VALUES (9, 'ses_1', 100, 100, 'late-nine');");
    capture(&archive, &db);
    for _ in 0..2 {
        let verified = capture_opts(
            &archive,
            &db,
            CaptureOptions {
                verify: true,
                ..CaptureOptions::default()
            },
        );
        let info = verified.sqlite.as_ref().unwrap();
        assert!(info.verify);
        assert_eq!(info.verify_mismatches, 0, "idempotent: {verified:?}");
    }
    assert_eq!(
        replay_sqlite(&archive, &source_key(&archive), None, DOMAIN)
            .unwrap()
        .tables["message"]
            .len(),
        30
    );
}

#[test]
fn sqlite_locked_pends_watermark_and_evidence_unchanged() {
    // (kept from the original acceptance list: lock failures never advance the
    // watermark or write evidence)
    let (temp, archive) = make_root();
    let db = temp.path().join("opencode.db");
    v1_fixture(&db);
    assert!(capture(&archive, &db).ok);
    let writer = Connection::open(&db).unwrap();
    writer.execute_batch("BEGIN EXCLUSIVE").unwrap();
    set_lock_timeout_for_tests(Some(50));
    let failed = capture(&archive, &db);
    set_lock_timeout_for_tests(None);
    writer.execute_batch("ROLLBACK").unwrap();
    drop(writer);
    assert!(!failed.ok);
    assert_eq!(failed.change, "sqlite_locked");
    assert_eq!(sqlite_state(&archive).capture_seq, 1);
    assert!(archive.manifests().unwrap().len() == 1);
}

/// Canonical source key helper for tests that hold more than one source.
fn source_key_of(archive: &Archive, path: &Path) -> String {
    let canonical = path.canonicalize().unwrap();
    let raw = canonical.to_string_lossy().to_string();
    let normalized = raw.strip_prefix(r"\\?\").unwrap_or(&raw).to_string();
    let sources = archive.load_sources().unwrap();
    sources
        .sources
        .keys()
        .find(|key| **key == normalized)
        .cloned()
        .unwrap_or_else(|| {
            panic!(
                "source key for {} not found in {:?}",
                path.display(),
                sources.sources.keys().collect::<Vec<_>>()
            )
        })
}
