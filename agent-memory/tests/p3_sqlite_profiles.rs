//! S4.5 acceptance: the SQLite evidence engine under non-OpenCode capture
//! profiles — synthetic Hermes-style and MCode-style databases.
//!
//! Every database in this file is synthetic and lives under a `tempfile`
//! root. The table shapes follow the cc-switch provider sources verbatim
//! (`hermes.rs` `state.db` schema, `mcode.rs` `local_runtime_*` fixture SQL);
//! no real provider database, session directory, or production archive is
//! read. The two profiles are defined and registered from test code — the
//! same registration entry point (`register_sqlite_capture_profile`) a real
//! "cc-switch provider + capture profile" integration will use after stage 2.
//!
//! Captures run with provider `"opencode"` because provider validation is part
//! of the unchanged production surface; the profile under test is selected
//! explicitly through `CaptureOptions::sqlite_profile` (an unknown id fails
//! the capture instead of falling back).

use std::path::{Path, PathBuf};

use agent_memory::archive::Archive;
use agent_memory::capture::{capture_source_with_options, CaptureOptions, CaptureReport};
use agent_memory::capture_sqlite::{
    read_baseline_rows, register_sqlite_capture_profile, replay_sqlite, sqlite_baselines,
    SqliteBaseline, SqliteCaptureProfile, SqliteReplay,
};
use rusqlite::types::ValueRef;
use rusqlite::Connection;
use serde_json::{json, Value};

const DOMAIN: &str = "default";
const HERMES_SENTINEL: &str = "sentinel-hermes-token-9f2c-never-archive";
const MCODE_SENTINEL: &str = "sentinel-mcode-secret-7a1d-never-archive";

// ---------------------------------------------------------------------------
// The two synthetic capture profiles (the future integration shape)
// ---------------------------------------------------------------------------

/// Hermes-style profile: `state.db` with `sessions(id, …)` keyed by `id` and
/// `messages(session_id, …)`. Both tables carry REAL (epoch-second) timestamps
/// only, so no integer watermark column exists — the engine's declared
/// full-scan hash path applies to every table of this profile.
const HERMES_PROFILE: SqliteCaptureProfile = SqliteCaptureProfile {
    profile_id: "hermes-synthetic",
    tables: &["sessions", "messages"],
    replay_order: &["sessions", "messages"],
    session_tables: &[("sessions", "id")],
    session_time_column: "last_activity_at",
    child_session_columns: &[("messages", "session_id")],
    watermark_columns: &["time_updated", "seq", "time_created", "created"],
    table_watermark_overrides: &[],
    excluded_tables: &["auth_tokens"],
};

/// MCode-style profile: `runtime-state.sqlite` with `local_runtime_sessions`
/// keyed by `session_id` (no declared primary key, epoch-millisecond
/// timestamps), the display blob table and the display row table as children,
/// and the migration bookkeeping table explicitly excluded.
const MCODE_PROFILE: SqliteCaptureProfile = SqliteCaptureProfile {
    profile_id: "mcode-synthetic",
    tables: &[
        "local_runtime_sessions",
        "local_runtime_messages",
        "local_runtime_message_rows",
    ],
    replay_order: &[
        "local_runtime_sessions",
        "local_runtime_messages",
        "local_runtime_message_rows",
    ],
    session_tables: &[("local_runtime_sessions", "session_id")],
    session_time_column: "updated_at_ms",
    child_session_columns: &[
        ("local_runtime_messages", "session_id"),
        ("local_runtime_message_rows", "session_id"),
    ],
    watermark_columns: &["updated_at_ms", "created_at_ms"],
    table_watermark_overrides: &[("local_runtime_message_rows", &["created_at_ms"])],
    excluded_tables: &["local_runtime_message_row_migrations", "mcode_credentials"],
};

/// Identical shape to [`HERMES_PROFILE`] with a different profile id — used to
/// prove that switching the profile on one source never diffs across profiles.
const HERMES_PROFILE_V2: SqliteCaptureProfile = SqliteCaptureProfile {
    profile_id: "hermes-synthetic-v2",
    ..HERMES_PROFILE
};

fn register_profiles() {
    register_sqlite_capture_profile("hermes-synthetic", &HERMES_PROFILE);
    register_sqlite_capture_profile("hermes-synthetic-v2", &HERMES_PROFILE_V2);
    register_sqlite_capture_profile("mcode-synthetic", &MCODE_PROFILE);
}

fn capture(
    archive: &Archive,
    db: &Path,
    profile: &'static SqliteCaptureProfile,
    options: CaptureOptions,
) -> CaptureReport {
    capture_source_with_options(
        archive,
        db,
        "opencode",
        DOMAIN,
        None,
        CaptureOptions {
            sqlite_profile: Some(profile.profile_id),
            ..options
        },
    )
    .unwrap()
}

fn source_key(archive: &Archive) -> String {
    archive
        .load_sources()
        .unwrap()
        .sources
        .keys()
        .next()
        .unwrap()
        .clone()
}

// ---------------------------------------------------------------------------
// Synthetic fixtures (shapes per cc-switch sources; content invented)
// ---------------------------------------------------------------------------

/// `state.db` per the cc-switch hermes provider schema, plus one synthetic
/// credential table carrying the sentinel.
fn hermes_db(path: &Path) {
    let conn = Connection::open(path).unwrap();
    conn.execute_batch(
        r#"
        CREATE TABLE sessions (
             id TEXT PRIMARY KEY, cwd TEXT, title TEXT,
             started_at REAL NOT NULL, ended_at REAL, last_activity_at REAL);
        CREATE TABLE messages (
             id INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT NOT NULL,
             role TEXT NOT NULL, content TEXT, tool_call_id TEXT, tool_calls TEXT,
             tool_name TEXT, timestamp REAL NOT NULL,
             active INTEGER NOT NULL DEFAULT 1, compacted INTEGER NOT NULL DEFAULT 0,
             display_metadata TEXT);
        CREATE TABLE auth_tokens (
             id INTEGER PRIMARY KEY, label TEXT, token TEXT NOT NULL);
        INSERT INTO sessions VALUES ('sess-1', '/work', 'hermes session', 100.0, NULL, 200.0);
        INSERT INTO messages (session_id, role, content, timestamp) VALUES
             ('sess-1', 'user', 'fix the flaky test', 110.0),
             ('sess-1', 'assistant', 'rerunning with serial 1', 120.0);
        INSERT INTO auth_tokens VALUES (1, 'ci', 'sentinel-hermes-token-9f2c-never-archive');
        "#,
    )
    .unwrap();
}

/// `runtime-state.sqlite` per the cc-switch mcode fixture SQL (sessions without
/// a declared primary key, epoch-millisecond timestamps), plus the migration
/// bookkeeping table and one synthetic credential table. Session a carries a
/// block of filler rows so the baseline bytes dwarf the delta bytes of the
/// tests (the 50%-of-baseline chain threshold must not fire on a miniature
/// fixture; the scale test exercises that trigger).
const MCODE_FILLER_ROWS: usize = 80;

fn mcode_db(path: &Path) {
    let conn = Connection::open(path).unwrap();
    let mut batch = String::from(
        r#"
        CREATE TABLE local_runtime_sessions (
            session_id TEXT, title TEXT, workspace_dir TEXT, created_at_ms INTEGER,
            updated_at_ms INTEGER, visibility TEXT, parent_session_id TEXT,
            session_kind TEXT, archived INTEGER);
        CREATE TABLE local_runtime_messages (session_id TEXT, display_messages_json TEXT);
        CREATE TABLE local_runtime_message_row_migrations (session_id TEXT);
        CREATE TABLE local_runtime_message_rows (
            id INTEGER, session_id TEXT, role TEXT, data_json TEXT, created_at_ms INTEGER);
        CREATE TABLE mcode_credentials (
            id INTEGER PRIMARY KEY, label TEXT, secret TEXT NOT NULL);
        INSERT INTO local_runtime_sessions VALUES
            ('mvs_a', 'session a', '/work', 10, 1000, 'visible', NULL, 'conversation', 0),
            ('mvs_b', 'session b', '/work', 10, 500, 'visible', NULL, 'conversation', 0);
        INSERT INTO local_runtime_message_rows VALUES
            (1, 'mvs_a', 'user', '{"msg_content":"question a"}', 100),
            (2, 'mvs_a', 'assistant', '{"msg_content":"answer a"}', 101),
            (3, 'mvs_b', 'user', '{"msg_content":"question b"}', 50);
        INSERT INTO local_runtime_messages VALUES
            ('mvs_b', '[{"role":"user","msg_content":"legacy b"}]');
        INSERT INTO local_runtime_message_row_migrations VALUES ('mvs_a');
        INSERT INTO mcode_credentials VALUES (1, 'cli', 'sentinel-mcode-secret-7a1d-never-archive');
        "#,
    );
    for index in 0..MCODE_FILLER_ROWS {
        batch.push_str(&format!(
            "INSERT INTO local_runtime_message_rows VALUES ({}, 'mvs_a', 'tool', \
             '{{\"msg_content\":\"filler {index:03}\"}}', {});\n",
            10 + index,
            200 + index as i64,
        ));
    }
    conn.execute_batch(&batch).unwrap();
}

// ---------------------------------------------------------------------------
// Helpers: live rows, replay rows, evidence scan
// ---------------------------------------------------------------------------

fn json_value(value: ValueRef<'_>) -> Value {
    match value {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(number) => Value::from(number),
        ValueRef::Real(number) => serde_json::Number::from_f64(number)
            .map(Value::Number)
            .unwrap_or(Value::Null),
        ValueRef::Text(text) => Value::String(String::from_utf8_lossy(text).to_string()),
        ValueRef::Blob(_) => panic!("fixtures contain no blobs"),
    }
}

/// Every live row of `table` as canonical JSON strings (order-independent).
fn live_rows(db: &Path, table: &str) -> Vec<String> {
    let conn =
        Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let mut statement = conn.prepare(&format!("SELECT * FROM \"{}\"", table)).unwrap();
    let names: Vec<String> = statement
        .column_names()
        .into_iter()
        .map(|name| name.to_string())
        .collect();
    let mut rows = statement.query([]).unwrap();
    let mut out = Vec::new();
    while let Some(row) = rows.next().unwrap() {
        let mut object = serde_json::Map::new();
        for (index, name) in names.iter().enumerate() {
            object.insert(name.clone(), json_value(row.get_ref(index).unwrap()));
        }
        out.push(serde_json::to_string(&Value::Object(object)).unwrap());
    }
    out.sort();
    out
}

fn replay_rows(replay: &SqliteReplay, table: &str) -> Vec<String> {
    let mut out: Vec<String> = replay.tables[table]
        .values()
        .map(|row| serde_json::to_string(row).unwrap())
        .collect();
    out.sort();
    out
}

fn assert_replay_matches_source(archive: &Archive, source: &str, db: &Path, profile: &SqliteCaptureProfile) {
    let replay = replay_sqlite(archive, source, None, DOMAIN).unwrap();
    for table in profile.tables {
        assert_eq!(
            replay_rows(&replay, table),
            live_rows(db, table),
            "replay of table {table} must match the live source row for row"
        );
    }
}

fn collect_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        if path.is_dir() {
            collect_files(&path, out);
        } else {
            out.push(path);
        }
    }
}

/// The credential sentinel must appear in no runtime-root file — neither in
/// the raw bytes nor in any decompressed object or state file.
fn assert_sentinel_absent(archive: &Archive, sentinel: &str) {
    let mut files = Vec::new();
    collect_files(archive.root(), &mut files);
    assert!(files.len() > 3, "the archive must have content to scan");
    for file in files {
        let bytes = std::fs::read(&file).unwrap();
        assert!(
            !windower_contains(&bytes, sentinel.as_bytes()),
            "raw {} contains the credential sentinel",
            file.display()
        );
        if let Ok(raw) = zstd::stream::decode_all(bytes.as_slice()) {
            assert!(
                !windower_contains(&raw, sentinel.as_bytes()),
                "decompressed {} contains the credential sentinel",
                file.display()
            );
        }
    }
}

fn windower_contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|window| window == needle)
}

fn baseline_projection(archive: &Archive) -> SqliteBaseline {
    sqlite_baselines(archive, &source_key(archive))
        .unwrap()
        .into_iter()
        .next_back()
        .unwrap()
}

fn manifest_profile_at_seq(archive: &Archive, seq: u64) -> String {
    archive
        .manifests()
        .unwrap()
        .iter()
        .find(|manifest| {
            manifest
                .source
                .sqlite
                .as_ref()
                .map(|sqlite| sqlite.capture_seq == seq)
                .unwrap_or(false)
        })
        .expect("manifest at seq")
        .source
        .sqlite
        .clone()
        .unwrap()
        .profile_id
}

fn state_sqlite(archive: &Archive) -> agent_memory::archive::SqliteState {
    archive
        .load_sources()
        .unwrap()
        .sources
        .values()
        .next()
        .unwrap()
        .sqlite
        .clone()
        .unwrap()
}

fn observation_profile_id(archive: &Archive) -> String {
    let state = state_sqlite(archive);
    let bytes = std::fs::read(archive.root().join(&state.observation_file)).unwrap();
    let raw = zstd::stream::decode_all(bytes.as_slice()).unwrap();
    let json: Value = serde_json::from_slice(&raw).unwrap();
    json["profile_id"].as_str().unwrap().to_string()
}

// ---------------------------------------------------------------------------
// Hermes-style profile: hash-path tables (REAL timestamps, no integer watermark)
// ---------------------------------------------------------------------------

#[test]
fn hermes_profile_baseline_delta_and_credential_sentinel() {
    register_profiles();
    let temp = tempfile::tempdir().unwrap();
    let archive = Archive::init(&temp.path().join("evidence")).unwrap();
    let db = temp.path().join("state.db");
    hermes_db(&db);

    // First baseline.
    let report = capture(&archive, &db, &HERMES_PROFILE, CaptureOptions::default());
    assert!(report.ok, "{report:?}");
    let info = report.sqlite.as_ref().unwrap();
    assert_eq!(info.mode, "baseline");
    assert_eq!(info.rows_discovered, 3);

    // The profile id is recorded in the manifest, the collector state and the
    // observation state.
    assert_eq!(baseline_projection(&archive).profile_id, "hermes-synthetic");
    assert_eq!(manifest_profile_at_seq(&archive, 1), "hermes-synthetic");
    assert_eq!(state_sqlite(&archive).profile_id, "hermes-synthetic");
    assert_eq!(observation_profile_id(&archive), "hermes-synthetic");

    // Baseline content.
    let baseline = baseline_projection(&archive);
    let rows = read_baseline_rows(&archive, &baseline, DOMAIN).unwrap();
    assert_eq!(rows["sessions"].len(), 1);
    assert_eq!(rows["messages"].len(), 2);
    // The excluded credential table is a `{table, row_count}` fact only.
    assert!(info
        .excluded_tables
        .iter()
        .any(|fact| fact.table == "auth_tokens" && fact.row_count == 1));

    // Delta: one insert, one content-only update, one delete (the hash path —
    // no watermark column exists in this schema).
    {
        let conn = Connection::open(&db).unwrap();
        conn.execute(
            "INSERT INTO messages (session_id, role, content, timestamp) \
             VALUES ('sess-1', 'user', 'still failing', 130.0)",
            [],
        )
        .unwrap();
        conn.execute(
            "UPDATE messages SET content = 'rerunning with serial 2' WHERE id = 2",
            [],
        )
        .unwrap();
        conn.execute("DELETE FROM messages WHERE id = 1", []).unwrap();
    }
    let report = capture(&archive, &db, &HERMES_PROFILE, CaptureOptions::default());
    assert!(report.ok, "{report:?}");
    let info = report.sqlite.as_ref().unwrap();
    assert_eq!(info.mode, "delta");
    assert_eq!(info.rows_inserted, 1);
    assert_eq!(info.rows_updated, 1);
    assert_eq!(info.rows_deleted, 1);
    assert_eq!(manifest_profile_at_seq(&archive, 2), "hermes-synthetic");

    // No sentinel byte ever entered the evidence (raw or decompressed).
    assert_sentinel_absent(&archive, HERMES_SENTINEL);
}

#[test]
fn hermes_profile_verify_pass_and_replay_match_the_source() {
    register_profiles();
    let temp = tempfile::tempdir().unwrap();
    let archive = Archive::init(&temp.path().join("evidence")).unwrap();
    let db = temp.path().join("state.db");
    hermes_db(&db);
    capture(&archive, &db, &HERMES_PROFILE, CaptureOptions::default());

    {
        let conn = Connection::open(&db).unwrap();
        conn.execute(
            "INSERT INTO messages (session_id, role, content, timestamp) \
             VALUES ('sess-1', 'assistant', 'done', 140.0)",
            [],
        )
        .unwrap();
        conn.execute(
            "UPDATE sessions SET title = 'renamed', last_activity_at = 300.0 WHERE id = 'sess-1'",
            [],
        )
        .unwrap();
    }
    // --verify after the changes: the delta observation records them, the
    // verification pass confirms the evidence (0 false mismatches).
    let report = capture(
        &archive,
        &db,
        &HERMES_PROFILE,
        CaptureOptions {
            verify: true,
            ..CaptureOptions::default()
        },
    );
    assert!(report.ok, "{report:?}");
    let info = report.sqlite.as_ref().unwrap();
    assert!(info.verify);
    assert_eq!(info.verify_mismatches, 0);
    assert_eq!(manifest_profile_at_seq(&archive, 2), "hermes-synthetic");

    let source = source_key(&archive);
    assert_replay_matches_source(&archive, &source, &db, &HERMES_PROFILE);
}

// ---------------------------------------------------------------------------
// MCode-style profile: watermark tables with a non-`id` session key
// ---------------------------------------------------------------------------

#[test]
fn mcode_profile_session_since_baseline_and_credential_sentinel() {
    register_profiles();
    let temp = tempfile::tempdir().unwrap();
    let archive = Archive::init(&temp.path().join("evidence")).unwrap();
    let db = temp.path().join("runtime-state.sqlite");
    mcode_db(&db);

    // `--session-since` on the profile's session time column (updated_at_ms):
    // only session a (updated 1000) enters the evidence; session b stays out.
    let report = capture(
        &archive,
        &db,
        &MCODE_PROFILE,
        CaptureOptions {
            session_since_ms: Some(900),
            ..CaptureOptions::default()
        },
    );
    assert!(report.ok, "{report:?}");
    let info = report.sqlite.as_ref().unwrap();
    assert_eq!(info.mode, "baseline");
    assert_eq!(info.session_since_ms, Some(900));

    let baseline = baseline_projection(&archive);
    let rows = read_baseline_rows(&archive, &baseline, DOMAIN).unwrap();
    assert_eq!(rows["local_runtime_sessions"].len(), 1);
    assert!(rows["local_runtime_sessions"]
        .values()
        .all(|row| row["session_id"] == json!("mvs_a")));
    assert_eq!(rows["local_runtime_message_rows"].len(), 2 + MCODE_FILLER_ROWS);
    assert_eq!(rows["local_runtime_messages"].len(), 0);
    // Excluded tables are facts only — including the credential sentinel.
    assert!(info
        .excluded_tables
        .iter()
        .any(|fact| fact.table == "local_runtime_message_row_migrations"
            && fact.row_count == 1));
    assert!(info
        .excluded_tables
        .iter()
        .any(|fact| fact.table == "mcode_credentials" && fact.row_count == 1));

    assert_eq!(baseline.profile_id, "mcode-synthetic");
    assert_eq!(manifest_profile_at_seq(&archive, 1), "mcode-synthetic");
    assert_eq!(state_sqlite(&archive).profile_id, "mcode-synthetic");
    assert_eq!(observation_profile_id(&archive), "mcode-synthetic");
    assert_sentinel_absent(&archive, MCODE_SENTINEL);
}

#[test]
fn mcode_profile_watermark_delta_insert_update_delete_and_replay() {
    register_profiles();
    let temp = tempfile::tempdir().unwrap();
    let archive = Archive::init(&temp.path().join("evidence")).unwrap();
    let db = temp.path().join("runtime-state.sqlite");
    mcode_db(&db);
    capture(&archive, &db, &MCODE_PROFILE, CaptureOptions::default());

    {
        let conn = Connection::open(&db).unwrap();
        // Insert: a new session above the watermark, with its row above the
        // child watermark (below it would be the declared blind window).
        conn.execute_batch(
            r#"
            INSERT INTO local_runtime_sessions VALUES
                ('mvs_c', 'session c', '/work', 20, 2000, 'visible', NULL, 'conversation', 0);
            INSERT INTO local_runtime_message_rows VALUES
                (4, 'mvs_c', 'user', '{"msg_content":"question c"}', 2000);
            "#,
        )
        .unwrap();
        // Update: title change with an updated_at_ms bump (watermark moves).
        conn.execute(
            "UPDATE local_runtime_sessions SET title = 'session a renamed', \
             updated_at_ms = 2100 WHERE session_id = 'mvs_a'",
            [],
        )
        .unwrap();
        // Update: row content with a created_at_ms bump past the watermark
        // (the profile's per-table override column decides).
        conn.execute(
            "UPDATE local_runtime_message_rows SET data_json = '{\"msg_content\":\"answer a v2\"}', \
             created_at_ms = 2101 WHERE id = 2",
            [],
        )
        .unwrap();
        // Delete: the newest row (rowid keys must not be reused by this test).
        conn.execute("DELETE FROM local_runtime_message_rows WHERE id = 3", []).unwrap();
    }
    let report = capture(&archive, &db, &MCODE_PROFILE, CaptureOptions::default());
    assert!(report.ok, "{report:?}");
    let info = report.sqlite.as_ref().unwrap();
    assert_eq!(info.mode, "delta");
    assert_eq!(
        info.rows_inserted, 2,
        "session c + its row are inserts"
    );
    assert_eq!(info.rows_updated, 2, "session a + its bumped row");
    assert_eq!(info.rows_deleted, 1);
    assert_eq!(manifest_profile_at_seq(&archive, 2), "mcode-synthetic");

    let source = source_key(&archive);
    assert_replay_matches_source(&archive, &source, &db, &MCODE_PROFILE);
}

#[test]
fn mcode_profile_session_reentry_backfills_child_rows_without_false_deletes() {
    register_profiles();
    let temp = tempfile::tempdir().unwrap();
    let archive = Archive::init(&temp.path().join("evidence")).unwrap();
    let db = temp.path().join("runtime-state.sqlite");
    mcode_db(&db);

    // Baseline scoped to session a (updated_at_ms 1000 >= 900).
    capture(
        &archive,
        &db,
        &MCODE_PROFILE,
        CaptureOptions {
            session_since_ms: Some(900),
            ..CaptureOptions::default()
        },
    );

    // Session b enters the scope: its session row is an insert and its
    // pre-existing child rows (created_at_ms 50, below the child watermark)
    // are backfilled (N2).
    {
        let conn = Connection::open(&db).unwrap();
        conn.execute(
            "UPDATE local_runtime_sessions SET updated_at_ms = 1200 \
             WHERE session_id = 'mvs_b'",
            [],
        )
        .unwrap();
    }
    let report = capture(
        &archive,
        &db,
        &MCODE_PROFILE,
        CaptureOptions {
            session_since_ms: Some(900),
            ..CaptureOptions::default()
        },
    );
    assert!(report.ok, "{report:?}");
    let info = report.sqlite.as_ref().unwrap();
    assert_eq!(info.mode, "delta");
    assert_eq!(info.rows_inserted, 3, "session b + its two child rows");
    assert_eq!(
        info.rows_deleted, 0,
        "session a must not be reported deleted (key spaces agree)"
    );
    assert_eq!(info.rows_updated, 0);

    // A later re-touch of session a (already selected, children archived)
    // re-backfills nothing: the archived rows are skipped (R1).
    {
        let conn = Connection::open(&db).unwrap();
        conn.execute(
            "UPDATE local_runtime_sessions SET title = 'session a retouched', \
             updated_at_ms = 1300 WHERE session_id = 'mvs_a'",
            [],
        )
        .unwrap();
    }
    let report = capture(
        &archive,
        &db,
        &MCODE_PROFILE,
        CaptureOptions {
            session_since_ms: Some(900),
            ..CaptureOptions::default()
        },
    );
    assert!(report.ok, "{report:?}");
    let info = report.sqlite.as_ref().unwrap();
    assert_eq!(info.mode, "delta");
    assert_eq!(info.rows_updated, 1, "only the session row itself");
    assert_eq!(info.rows_inserted, 0);

    let source = source_key(&archive);
    assert_replay_matches_source(&archive, &source, &db, &MCODE_PROFILE);
}

#[test]
fn mcode_profile_blind_edit_is_corrected_by_verify() {
    register_profiles();
    let temp = tempfile::tempdir().unwrap();
    let archive = Archive::init(&temp.path().join("evidence")).unwrap();
    let db = temp.path().join("runtime-state.sqlite");
    mcode_db(&db);
    capture(&archive, &db, &MCODE_PROFILE, CaptureOptions::default());

    // Blind edit: content changes while the watermark column stays — the
    // watermark observation cannot see it (declared blind window).
    {
        let conn = Connection::open(&db).unwrap();
        conn.execute(
            "UPDATE local_runtime_message_rows SET data_json = '{\"msg_content\":\"answer a edited\"}' \
             WHERE id = 1",
            [],
        )
        .unwrap();
    }
    let report = capture(&archive, &db, &MCODE_PROFILE, CaptureOptions::default());
    assert!(report.ok, "{report:?}");
    assert_eq!(report.sqlite.as_ref().unwrap().mode, "unchanged");

    // --verify re-hashes every whitelisted row and records the correction.
    let report = capture(
        &archive,
        &db,
        &MCODE_PROFILE,
        CaptureOptions {
            verify: true,
            ..CaptureOptions::default()
        },
    );
    assert!(report.ok, "{report:?}");
    let info = report.sqlite.as_ref().unwrap();
    assert!(info.verify);
    assert_eq!(info.verify_mismatches, 1);
    assert_eq!(info.rows_updated, 1);
    // The unchanged run consumed no sequence number: the correction delta is
    // sequence 2 (baseline=1), and it records the profile.
    assert_eq!(manifest_profile_at_seq(&archive, 2), "mcode-synthetic");

    let source = source_key(&archive);
    assert_replay_matches_source(&archive, &source, &db, &MCODE_PROFILE);
}

// ---------------------------------------------------------------------------
// Profile switching and fallbacks
// ---------------------------------------------------------------------------

#[test]
fn switching_profiles_never_diffs_across_profiles() {
    register_profiles();
    let temp = tempfile::tempdir().unwrap();
    let archive = Archive::init(&temp.path().join("evidence")).unwrap();
    let db = temp.path().join("state.db");
    hermes_db(&db);
    capture(&archive, &db, &HERMES_PROFILE, CaptureOptions::default());
    assert_eq!(observation_profile_id(&archive), "hermes-synthetic");

    // The same source under another profile with the same schema: the
    // observation state names a different profile, so it is rejected and the
    // run re-baselines degraded — and because the export under the identical
    // shape is content-identical, the baseline deduplicates (no duplicate
    // evidence bytes) while the collector state is re-pointed to the new
    // profile.
    let report = capture(&archive, &db, &HERMES_PROFILE_V2, CaptureOptions::default());
    assert!(!report.ok, "a profile switch is a degraded run: {report:?}");
    assert_eq!(report.status, "degraded");
    let info = report.sqlite.as_ref().unwrap();
    assert_eq!(info.mode, "unchanged");
    assert_eq!(
        info.snapshot_trigger,
        "snapshot_deduplicated:observation_state_missing"
    );
    // The identical evidence unit keeps its original profile id, and the
    // observation state now belongs to the new profile.
    assert_eq!(
        baseline_projection(&archive).profile_id,
        "hermes-synthetic"
    );
    assert_eq!(observation_profile_id(&archive), "hermes-synthetic-v2");

    // The next run under the new profile diffs normally.
    {
        let conn = Connection::open(&db).unwrap();
        conn.execute(
            "INSERT INTO messages (session_id, role, content, timestamp) \
             VALUES ('sess-1', 'user', 'after the switch', 150.0)",
            [],
        )
        .unwrap();
    }
    let report = capture(&archive, &db, &HERMES_PROFILE_V2, CaptureOptions::default());
    assert!(report.ok, "{report:?}");
    let info = report.sqlite.as_ref().unwrap();
    assert_eq!(info.mode, "delta");
    assert_eq!(info.rows_inserted, 1);
}

#[test]
fn unknown_profile_override_fails_the_capture() {
    register_profiles();
    let temp = tempfile::tempdir().unwrap();
    let archive = Archive::init(&temp.path().join("evidence")).unwrap();
    let db = temp.path().join("state.db");
    hermes_db(&db);
    let error = capture_source_with_options(
        &archive,
        &db,
        "opencode",
        DOMAIN,
        None,
        CaptureOptions {
            sqlite_profile: Some("never-registered"),
            ..CaptureOptions::default()
        },
    )
    .unwrap_err();
    assert!(
        error.to_string().contains("never-registered"),
        "unknown profile ids fail loudly, got: {error}"
    );
}
