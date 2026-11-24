//! §D zero-row rescan fix: the archive-fallback registry records the rule
//! version and the projected unit count of every session, so a session whose
//! projection is empty is not re-read (and its carrier not re-decompressed) on
//! every `index` run.
//!
//! Every fixture is synthetic and lives under a per-test temporary root: the
//! sources are written by the test, captured through the same `capture-all`
//! engine production uses, and indexed through `index_all`. No test points at a
//! real Claude, Codex or DSH session directory or Archive.

use agent_memory::archive::Archive;
use agent_memory::recall::config::load_config;
use agent_memory::recall::fulltext::build::index_all;
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};

mod support;

use support::TestEnv;

/// Marker text of the session that does project rows.
const CONTENT_TEXT: &str = "registry needle content session 3b71";

// ---------------------------------------------------------------------------
// Fixture helpers
// ---------------------------------------------------------------------------

fn write_lines(path: &Path, lines: &[String]) {
    let mut body = lines.join("\n");
    body.push('\n');
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("fixture parent dir");
    }
    fs::write(path, body).expect("write fixture file");
}

/// One DSH carrier: consecutive zstd frames, each holding complete lines.
fn write_dsh_log(path: &Path, lines: &[String]) {
    let mut bytes = Vec::new();
    for line in lines {
        let frame = zstd::stream::encode_all(format!("{line}\n").as_bytes(), 3).expect("frame");
        bytes.extend_from_slice(&frame);
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("dsh session dir");
    }
    fs::write(path, bytes).expect("write dsh log");
}

fn claude_line(session: &str, uuid: &str, text: &str) -> String {
    json!({
        "type": "user",
        "message": {"role": "user", "content": [{"type": "text", "text": text}]},
        "timestamp": "2026-10-25T13:25:00Z",
        "sessionId": session,
        "uuid": uuid,
        "isSidechain": false
    })
    .to_string()
}

fn codex_meta(id: &str) -> String {
    json!({
        "timestamp": "2026-10-25T10:00:00Z",
        "type": "session_meta",
        "payload": {"id": id, "source": "cli"}
    })
    .to_string()
}

fn codex_message(i: usize, text: &str) -> String {
    json!({
        "timestamp": "2026-10-25T10:00:00Z",
        "type": "response_item",
        "payload": {
            "type": "message",
            "role": "assistant",
            "content": [{"type": "output_text", "text": text}]
        },
        "seq": i
    })
    .to_string()
}

fn dsh_header(id: &str) -> String {
    json!({
        "type": "session",
        "version": 4,
        "id": id,
        "createdAt": 1_782_000_000_000i64,
        "cwd": "C:\\Users\\x\\Documents\\GitHub"
    })
    .to_string()
}

fn dsh_event(kind: &str, seq: u64, data: Value) -> String {
    json!({"type": kind, "seq": seq, "time": 1_782_000_000_000i64 + seq as i64, "data": data}).to_string()
}

struct World {
    env: TestEnv,
    archive_root: PathBuf,
    cache_root: PathBuf,
    claude_root: PathBuf,
    claude_content: PathBuf,
    codex_root: PathBuf,
    codex_content: PathBuf,
    dsh_root: PathBuf,
    dsh_content: PathBuf,
    opencode_root: PathBuf,
    opencode_db: PathBuf,
}

/// Four source sessions per kind: one that only carries bookkeeping records
/// (zero projected rows) and one with real conversation text.
fn build_world(name: &str) -> World {
    let env = TestEnv::new();
    let archive_root = env.root(name);
    Archive::init(&archive_root).expect("init runtime root");
    let cache_root = env.root("cache");

    let claude_root = env.root("claude-projects");
    let project = claude_root.join("proj");
    // The live Claude scan keys a transcript by its file name, so the fixture
    // uses the same session id in the file name and in every record.
    let claude_zero = project.join("00000000-1111-4111-8111-111111111111.jsonl");
    write_lines(
        &claude_zero,
        &[
            json!({"type": "mode", "mode": "plan",
                   "sessionId": "00000000-1111-4111-8111-111111111111"}).to_string(),
            json!({"type": "permission-mode", "permissionMode": "accept",
                   "sessionId": "00000000-1111-4111-8111-111111111111"}).to_string(),
            json!({"type": "system", "subtype": "init",
                   "sessionId": "00000000-1111-4111-8111-111111111111"}).to_string(),
        ],
    );
    let claude_content = project.join("00000000-2222-4222-8222-222222222222.jsonl");
    write_lines(
        &claude_content,
        &[claude_line(
            "00000000-2222-4222-8222-222222222222",
            "u-a1",
            CONTENT_TEXT,
        )],
    );

    let codex_root = env.root("codex-home");
    let codex_zero = codex_root
        .join("sessions")
        .join("2026")
        .join("10")
        .join("25")
        .join("rollout-zero.jsonl");
    write_lines(
        &codex_zero,
        &[
            codex_meta("zero-codex"),
            json!({"timestamp": "2026-10-25T10:00:00Z", "type": "event_msg",
                   "payload": {"type": "token_count", "total": 12}})
            .to_string(),
        ],
    );
    let codex_content = codex_root
        .join("sessions")
        .join("2026")
        .join("10")
        .join("25")
        .join("rollout-content.jsonl");
    write_lines(
        &codex_content,
        &[codex_meta("content-codex"), codex_message(1, CONTENT_TEXT)],
    );

    let dsh_root = env.root("dsh-sessions");
    let dsh_project = dsh_root.join("--C-Users-x-Documents-GitHub--");
    let dsh_zero = dsh_project.join("session-zero").join("session.jsonl.zstd");
    write_dsh_log(
        &dsh_zero,
        &[
            dsh_header("zero-dsh"),
            dsh_event("permission/preset", 1, json!({"preset": "default"})),
            dsh_event("agent-preset/selected", 2, json!({"preset": "default"})),
        ],
    );
    let dsh_content = dsh_project.join("session-content").join("session.jsonl.zstd");
    write_dsh_log(
        &dsh_content,
        &[
            dsh_header("content-dsh"),
            dsh_event(
                "user/message",
                1,
                json!({"id": "m-1", "role": "user", "source": {"kind": "user"},
                       "content": [{"type": "text", "text": CONTENT_TEXT}]}),
            ),
        ],
    );

    // OpenCode: a V2 database whose zero-row session carries no message at
    // all; a message is inserted only for the carrier-version case.
    let opencode_root = env.root("opencode-home");
    let opencode_db = opencode_root.join("opencode.db");
    {
        let conn = rusqlite::Connection::open(&opencode_db).expect("opencode db");
        conn.execute_batch(support::V2_SCHEMA).expect("v2 schema");
        conn.execute(
            "INSERT INTO session_v2 (id, title, directory, time_created, time_updated, parent_id)              VALUES ('zero-oc', 'zero', 'C:/tmp', 1, 2, NULL)",
            [],
        )
        .expect("insert zero session");
    }

    World {
        env,
        archive_root,
        cache_root,
        claude_root,
        claude_content,
        codex_root,
        codex_content,
        dsh_root,
        dsh_content,
        opencode_root,
        opencode_db,
    }
}

impl World {
    fn config(&self) -> PathBuf {
        let value = json!({
            "version": 2,
            "sources": [
                {"source_id": "claude_code", "kind": "claude_code",
                 "root": self.claude_root.to_string_lossy(), "enabled": true},
                {"source_id": "codex", "kind": "codex",
                 "root": self.codex_root.to_string_lossy(), "enabled": true},
                {"source_id": "dsh", "kind": "dsh",
                 "root": self.dsh_root.to_string_lossy(), "enabled": true},
                {"source_id": "opencode", "kind": "opencode",
                 "root": self.opencode_root.to_string_lossy(), "enabled": true},
                {"source_id": "archive", "kind": "archive",
                 "root": self.archive_root.to_string_lossy(), "enabled": false}
            ],
            "source_order": ["claude_code", "codex", "dsh", "opencode"],
            "interpreter": {"kb": {"command": "C:/python/python.exe"}},
            "domain": "default",
            "telemetry": {"enabled": false},
            "cache_root": self.cache_root.to_string_lossy()
        });
        self.env.write_json("recall.json", &value)
    }

    fn capture(&self, config: &Path) {
        let parsed = load_config(config).expect("config loads");
        let options = agent_memory::capture_all::CaptureAllOptions {
            root: None,
            domain: "default".to_string(),
            only_source: None,
            session_since: None,
            plan: false,
            skip_if_locked: false,
        };
        let (report, code) =
            agent_memory::capture_all::capture_all(&parsed, &options).expect("capture-all runs");
        assert_eq!(code, 0, "capture-all failed: {report:?}");
        assert!(report.ok, "capture-all not ok");
    }

    /// Drop the sessions with content from the live roots so all of them are
    /// archive-only in the index.
    fn remove_content_sessions(&self) {
        fs::remove_file(&self.claude_content).expect("delete claude transcript");
        fs::remove_file(&self.codex_content).expect("delete codex rollout");
        fs::remove_dir_all(self.dsh_content.parent().expect("dsh dir")).expect("delete dsh dir");
    }

    fn index(&self, config: &Path, only: Option<&str>, rebuild: bool) -> Vec<Value> {
        let parsed = load_config(config).expect("config loads");
        index_all(&parsed, only, rebuild)
            .expect("index runs")
            .iter()
            .map(|report| serde_json::to_value(report).expect("report json"))
            .collect()
    }

    fn report<'a>(&self, reports: &'a [Value], source_id: &str) -> &'a Value {
        reports
            .iter()
            .find(|report| report["source_id"] == json!(source_id))
            .unwrap_or_else(|| panic!("source {source_id} missing from report"))
    }

    fn index_db(&self, source_id: &str) -> PathBuf {
        self.cache_root.join("live").join(source_id).join("fts.sqlite")
    }

    fn registry(&self, source_id: &str) -> Vec<RegistryRow> {
        let conn = rusqlite::Connection::open(self.index_db(source_id)).expect("index db");
        let mut statement = conn
            .prepare(
                "SELECT session_id, rule_version, projected_units, carrier_fingerprint \
                 FROM archive_sessions ORDER BY session_id",
            )
            .expect("prepare");
        let rows = statement
            .query_map([], |row| {
                Ok(RegistryRow {
                    session_id: row.get::<_, String>(0)?,
                    rule_version: row.get::<_, Option<String>>(1)?,
                    projected_units: row.get::<_, Option<i64>>(2)?,
                    carrier_fingerprint: row.get::<_, Option<String>>(3)?,
                })
            })
            .expect("query");
        rows.collect::<Result<Vec<_>, _>>().expect("rows")
    }

    fn registry_row(&self, source_id: &str, session: &str) -> RegistryRow {
        self.registry(source_id)
            .into_iter()
            .find(|row| row.session_id == session)
            .unwrap_or_else(|| panic!("{source_id}: {session} not registered"))
    }

    /// Number of archive-fallback rows the index holds for one session.
    fn archive_rows(&self, source_id: &str, session: &str) -> i64 {
        let conn = rusqlite::Connection::open(self.index_db(source_id)).expect("index db");
        conn.query_row(
            "SELECT COUNT(*) FROM doc_keys WHERE session_id = ?1 AND origin = 'archive'",
            [session],
            |row| row.get::<_, i64>(0),
        )
        .expect("count")
    }

    /// Simulate lost index rows of one session (a damaged/copied index).
    fn drop_archive_rows(&self, source_id: &str, session: &str) {
        let conn = rusqlite::Connection::open(self.index_db(source_id)).expect("index db");
        let mut statement = conn
            .prepare("SELECT rowid FROM doc_keys WHERE session_id = ?1 AND origin = 'archive'")
            .expect("prepare");
        let rowids: Vec<i64> = statement
            .query_map([session], |row| row.get::<_, i64>(0))
            .expect("query")
            .collect::<Result<Vec<_>, _>>()
            .expect("rowids");
        for rowid in &rowids {
            conn.execute("DELETE FROM units WHERE rowid = ?1", [rowid])
                .expect("delete unit");
            conn.execute("DELETE FROM conversation_units WHERE rowid = ?1", [rowid])
                .expect("delete conversation unit");
        }
        conn.execute(
            "DELETE FROM doc_keys WHERE session_id = ?1 AND origin = 'archive'",
            [session],
        )
        .expect("delete doc keys");
    }

    fn phases(report: &Value) -> Vec<String> {
        report["cost"]["phases"]
            .as_object()
            .map(|map| map.keys().cloned().collect())
            .unwrap_or_default()
    }
}

/// One `archive_sessions` row (all bookkeeping columns).
#[derive(Debug, Clone, PartialEq, Eq)]
struct RegistryRow {
    session_id: String,
    rule_version: Option<String>,
    projected_units: Option<i64>,
    carrier_fingerprint: Option<String>,
}

fn archive_of(report: &Value) -> &Value {
    &report["detail"]["archive"]
}

// ---------------------------------------------------------------------------
// 1. Zero-row sessions are processed once, then skipped with a counter.
// ---------------------------------------------------------------------------

#[test]
fn zero_row_sessions_are_registered_and_skipped_on_the_next_run() {
    let world = build_world("registry-zero");
    let config = world.config();
    world.capture(&config);
    world.remove_content_sessions();

    // First run: every archive-only session is projected (one empty, one with
    // rows) and the carrier is rebuilt for real.
    let reports = world.index(&config, None, false);
    for source_id in ["claude_code", "codex", "dsh"] {
        let report = world.report(&reports, source_id);
        assert_eq!(archive_of(report)["status"], "ok", "{source_id}");
        assert_eq!(archive_of(report)["added"], 2, "{source_id}: {report}");
        assert_eq!(
            archive_of(report)["skipped_registered_empty"], 0,
            "{source_id}"
        );
        assert!(
            World::phases(report).iter().any(|phase| phase == "archive_carrier_rebuild"),
            "{source_id} rebuilt a carrier: {report}"
        );
    }

    // Second run: the empty sessions are skipped without touching the evidence,
    // and the counter says so.
    let reports = world.index(&config, None, false);
    let zero_sessions = [
        ("claude_code", "00000000-1111-4111-8111-111111111111"),
        ("codex", "zero-codex"),
        ("dsh", "zero-dsh"),
    ];
    for (source_id, zero_session) in zero_sessions {
        let report = world.report(&reports, source_id);
        assert_eq!(archive_of(report)["added"], 0, "{source_id}: {report}");
        assert_eq!(
            archive_of(report)["skipped_registered_empty"], 1,
            "{source_id}: {report}"
        );
        assert!(
            !World::phases(report)
                .iter()
                .any(|phase| phase == "archive_carrier_rebuild"),
            "{source_id} must not re-decompress the carrier: {report}"
        );
        let entry = world.registry_row(source_id, zero_session);
        assert_eq!(entry.rule_version.as_deref(), Some("fallback-1"), "{source_id}");
        assert_eq!(
            entry.projected_units,
            Some(0),
            "{source_id}: the empty projection is recorded"
        );
        assert!(
            entry
                .carrier_fingerprint
                .as_deref()
                .is_some_and(|fingerprint| !fingerprint.is_empty()),
            "{source_id}: the carrier version is recorded: {entry:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// 2. A session whose archive rows vanished is rebuilt; empty ones stay skipped.
// ---------------------------------------------------------------------------

#[test]
fn lost_archive_rows_are_rebuilt_while_empty_sessions_stay_skipped() {
    let world = build_world("registry-loss");
    let config = world.config();
    world.capture(&config);
    world.remove_content_sessions();
    world.index(&config, None, false);

    world.drop_archive_rows(
        "claude_code",
        "00000000-2222-4222-8222-222222222222",
    );
    let reports = world.index(&config, Some("claude_code"), false);
    let report = world.report(&reports, "claude_code");
    assert_eq!(archive_of(report)["added"], 1, "{report}");
    assert_eq!(archive_of(report)["skipped_registered_empty"], 1, "{report}");
    assert!(
        World::phases(report)
            .iter()
            .any(|phase| phase == "archive_carrier_rebuild"),
        "the rows were rebuilt from the evidence: {report}"
    );

    let content = world.registry_row("claude_code", "00000000-2222-4222-8222-222222222222");
    assert!(
        content.projected_units.unwrap_or(0) > 0,
        "the projected unit count is recorded: {content:?}"
    );
    assert!(
        content
            .carrier_fingerprint
            .as_deref()
            .is_some_and(|fingerprint| !fingerprint.is_empty()),
        "the carrier version is recorded: {content:?}"
    );
}

// ---------------------------------------------------------------------------
// 3. A registration written by the old layout (both columns NULL) is processed
//    once and completed.
// ---------------------------------------------------------------------------

#[test]
fn legacy_registration_rows_are_processed_once_and_completed() {
    let world = build_world("registry-legacy");
    let config = world.config();
    world.capture(&config);
    world.remove_content_sessions();
    world.index(&config, None, false);

    {
        let conn = rusqlite::Connection::open(world.index_db("codex")).expect("index db");
        conn.execute(
            "UPDATE archive_sessions SET rule_version = NULL, projected_units = NULL, \
             carrier_fingerprint = NULL WHERE session_id = 'zero-codex'",
            [],
        )
        .expect("legacy rows");
    }

    let reports = world.index(&config, Some("codex"), false);
    let report = world.report(&reports, "codex");
    // Only the session whose registration is NULL is re-projected; the other
    // (current version, rows present) is left alone.
    assert_eq!(archive_of(report)["added"], 1, "{report}");
    assert_eq!(archive_of(report)["skipped_registered_empty"], 0, "{report}");

    let zero = world.registry_row("codex", "zero-codex");
    assert_eq!(zero.rule_version.as_deref(), Some("fallback-1"), "{zero:?}");
    assert_eq!(zero.projected_units, Some(0), "{zero:?}");
    assert!(
        zero.carrier_fingerprint
            .as_deref()
            .is_some_and(|fingerprint| !fingerprint.is_empty()),
        "the third column is completed in place: {zero:?}"
    );

    // And the completed registration is skipped on the next run.
    let reports = world.index(&config, Some("codex"), false);
    let report = world.report(&reports, "codex");
    assert_eq!(archive_of(report)["added"], 0, "{report}");
    assert_eq!(archive_of(report)["skipped_registered_empty"], 1, "{report}");
}

// ---------------------------------------------------------------------------
// 4. A full rebuild clears the registry, so empty sessions are processed once
//    again before being re-registered (Writer::reset behaviour unchanged).
// ---------------------------------------------------------------------------

#[test]
fn a_full_rebuild_processes_empty_sessions_once_more() {
    let world = build_world("registry-rebuild");
    let config = world.config();
    world.capture(&config);
    world.remove_content_sessions();
    world.index(&config, None, false);

    let reports = world.index(&config, Some("dsh"), true);
    let report = world.report(&reports, "dsh");
    assert_eq!(report["status"], "built", "{report}");
    assert_eq!(archive_of(report)["added"], 2, "{report}");

    let reports = world.index(&config, Some("dsh"), false);
    let report = world.report(&reports, "dsh");
    assert_eq!(archive_of(report)["added"], 0, "{report}");
    assert_eq!(archive_of(report)["skipped_registered_empty"], 1, "{report}");
}

// ---------------------------------------------------------------------------
// 5. A new carrier version re-opens an empty registration (JSONL / OpenCode /
//    DSH): the host wrote real content, the Archive captured it, and the
//    session disappeared from the live source before the next index run. Only
//    the carrier fingerprint can tell that case apart from the unchanged one.
// ---------------------------------------------------------------------------

#[test]
fn a_new_claude_carrier_version_rebuilds_the_empty_registration() {
    let world = build_world("registry-carrier-claude");
    let config = world.config();
    world.capture(&config);
    fs::remove_file(
        world
            .claude_root
            .join("proj")
            .join("00000000-1111-4111-8111-111111111111.jsonl"),
    )
    .expect("delete the zero-row transcript");
    let reports = world.index(&config, Some("claude_code"), false);
    let report = world.report(&reports, "claude_code");
    assert_eq!(archive_of(report)["added"], 1, "{report}");
    let before = world.registry_row("claude_code", "00000000-1111-4111-8111-111111111111");
    assert_eq!(before.projected_units, Some(0), "{before:?}");

    // The host now writes a real message into the same session and the next
    // capture commits a new carrier version.
    write_lines(
        &world
            .claude_root
            .join("proj")
            .join("00000000-1111-4111-8111-111111111111.jsonl"),
        &[
            json!({"type": "mode", "mode": "plan",
                   "sessionId": "00000000-1111-4111-8111-111111111111"}).to_string(),
            claude_line(
                "00000000-1111-4111-8111-111111111111",
                "u-late",
                "late claude needle carrier",
            ),
        ],
    );
    world.capture(&config);
    fs::remove_file(
        world
            .claude_root
            .join("proj")
            .join("00000000-1111-4111-8111-111111111111.jsonl"),
    )
    .expect("delete the transcript again");

    let reports = world.index(&config, Some("claude_code"), false);
    let report = world.report(&reports, "claude_code");
    assert_eq!(archive_of(report)["added"], 1, "{report}");
    assert!(
        World::phases(report)
            .iter()
            .any(|phase| phase == "archive_carrier_rebuild"),
        "the new version is re-read: {report}"
    );
    let after = world.registry_row("claude_code", "00000000-1111-4111-8111-111111111111");
    assert!(
        after.projected_units.unwrap_or(0) > 0,
        "the new content is projected: {after:?}"
    );
    assert_ne!(
        before.carrier_fingerprint, after.carrier_fingerprint,
        "the registration follows the carrier version"
    );
    assert!(
        world.archive_rows("claude_code", "00000000-1111-4111-8111-111111111111") > 0,
        "archive rows exist for the rebuilt session"
    );
}

#[test]
fn a_new_opencode_carrier_version_rebuilds_the_empty_registration() {
    let world = build_world("registry-carrier-opencode");
    let config = world.config();
    world.capture(&config);
    // The OpenCode session leaves the live database with no messages at all.
    {
        let conn = rusqlite::Connection::open(&world.opencode_db).expect("opencode db");
        conn.execute("DELETE FROM session_v2 WHERE id = 'zero-oc'", [])
            .expect("delete live session");
    }
    // `added` counts projected units for the OpenCode path (zero here), so the
    // empty registration is asserted through the registry and the counter.
    let reports = world.index(&config, Some("opencode"), false);
    let report = world.report(&reports, "opencode");
    assert_eq!(archive_of(report)["archive_only_sessions"], 1, "{report}");
    assert_eq!(archive_of(report)["skipped_registered_empty"], 0, "{report}");
    let before = world.registry_row("opencode", "zero-oc");
    assert_eq!(before.projected_units, Some(0), "{before:?}");

    // The session comes back with a real message; the next capture commits a
    // new SQLite evidence version, then the live rows disappear again.
    {
        let conn = rusqlite::Connection::open(&world.opencode_db).expect("opencode db");
        conn.execute(
            "INSERT INTO session_v2 (id, title, directory, time_created, time_updated, parent_id) \
             VALUES ('zero-oc', 'zero', 'C:/tmp', 3, 4, NULL)",
            [],
        )
        .expect("reinsert session");
        conn.execute(
            "INSERT INTO session_message (id, session_id, seq, type, time_created, data) \
             VALUES ('sm-late', 'zero-oc', 1, 'user', 5, ?1)",
            rusqlite::params![
                json!({"text": "late opencode needle carrier"}).to_string()
            ],
        )
        .expect("insert message");
    }
    world.capture(&config);
    {
        let conn = rusqlite::Connection::open(&world.opencode_db).expect("opencode db");
        conn.execute("DELETE FROM session_message WHERE session_id = 'zero-oc'", [])
            .expect("delete live message");
        conn.execute("DELETE FROM session_v2 WHERE id = 'zero-oc'", [])
            .expect("delete live session");
    }

    let reports = world.index(&config, Some("opencode"), false);
    let report = world.report(&reports, "opencode");
    assert!(
        archive_of(report)["added"].as_u64().unwrap_or(0) >= 1,
        "the new message is projected: {report}"
    );
    let after = world.registry_row("opencode", "zero-oc");
    assert!(
        after.projected_units.unwrap_or(0) > 0,
        "the new content is projected: {after:?}"
    );
    assert_ne!(
        before.carrier_fingerprint, after.carrier_fingerprint,
        "the registration follows the carrier version"
    );
    assert!(
        world.archive_rows("opencode", "zero-oc") > 0,
        "archive rows exist for the rebuilt session"
    );
}

#[test]
fn a_new_dsh_carrier_version_rebuilds_the_empty_registration() {
    let world = build_world("registry-carrier-dsh");
    let config = world.config();
    world.capture(&config);
    let zero_dir = world
        .dsh_root
        .join("--C-Users-x-Documents-GitHub--")
        .join("session-zero");
    fs::remove_dir_all(&zero_dir).expect("delete the zero-row session dir");
    let reports = world.index(&config, Some("dsh"), false);
    let report = world.report(&reports, "dsh");
    assert_eq!(archive_of(report)["added"], 1, "{report}");
    let before = world.registry_row("dsh", "zero-dsh");
    assert_eq!(before.projected_units, Some(0), "{before:?}");

    // The host writes a real message; the next capture commits a new carrier
    // generation, then the session directory disappears again.
    write_dsh_log(
        &zero_dir.join("session.jsonl.zstd"),
        &[
            dsh_header("zero-dsh"),
            dsh_event("permission/preset", 1, json!({"preset": "default"})),
            dsh_event(
                "user/message",
                2,
                json!({"id": "m-late", "role": "user", "source": {"kind": "user"},
                       "content": [{"type": "text", "text": "late dsh needle carrier"}]}),
            ),
        ],
    );
    world.capture(&config);
    fs::remove_dir_all(&zero_dir).expect("delete the session dir again");

    let reports = world.index(&config, Some("dsh"), false);
    let report = world.report(&reports, "dsh");
    assert_eq!(archive_of(report)["added"], 1, "{report}");
    let after = world.registry_row("dsh", "zero-dsh");
    assert!(
        after.projected_units.unwrap_or(0) > 0,
        "the new content is projected: {after:?}"
    );
    assert_ne!(
        before.carrier_fingerprint, after.carrier_fingerprint,
        "the registration follows the carrier version"
    );
    assert!(
        world.archive_rows("dsh", "zero-dsh") > 0,
        "archive rows exist for the rebuilt session"
    );
}

// ---------------------------------------------------------------------------
// 5. The live source keeps priority: a session that is still live is neither
//    archive-projected nor registered, so its carrier is irrelevant here.
// ---------------------------------------------------------------------------

#[test]
fn live_sessions_with_content_are_never_registered_as_archive_only() {
    let world = build_world("registry-live");
    let config = world.config();
    world.capture(&config);

    // A live transcript that projects **zero** units is not in the live row
    // set either (the registry counts sessions backed by rows), so it is
    // registered as an empty archive session once and then skipped — never
    // re-decompressed. A live session with content is not registered at all.
    let reports = world.index(&config, None, false);
    for (source_id, zero_session) in [
        ("claude_code", "00000000-1111-4111-8111-111111111111"),
        ("codex", "zero-codex"),
        ("dsh", "zero-dsh"),
    ] {
        let report = world.report(&reports, source_id);
        assert_eq!(archive_of(report)["added"], 1, "{report}");
        let registry = world.registry(source_id);
        assert_eq!(registry.len(), 1, "{source_id}: {registry:?}");
        assert_eq!(registry[0].session_id, zero_session, "{source_id}: {registry:?}");
        assert_eq!(
            registry[0].projected_units,
            Some(0),
            "{source_id}: {registry:?}"
        );
    }

    let reports = world.index(&config, None, false);
    for source_id in ["claude_code", "codex", "dsh"] {
        let report = world.report(&reports, source_id);
        assert_eq!(archive_of(report)["added"], 0, "{report}");
        assert_eq!(
            archive_of(report)["skipped_registered_empty"], 1,
            "{report}"
        );
    }

    // Only the content transcript's session becomes a real archive session
    // once it disappears from the live source.
    fs::remove_file(&world.claude_content).expect("delete claude transcript");
    let reports = world.index(&config, Some("claude_code"), false);
    let report = world.report(&reports, "claude_code");
    assert_eq!(archive_of(report)["added"], 1, "{report}");
    let registry = world.registry("claude_code");
    assert_eq!(registry.len(), 2, "{registry:?}");
    let content = world.registry_row("claude_code", "00000000-2222-4222-8222-222222222222");
    assert!(content.projected_units.unwrap_or(0) > 0, "{registry:?}");
}

// ---------------------------------------------------------------------------
// 6. One session id held by two carriers: the zero-row carrier sorts first and
//    must not hide the real content of the later one (P2-1 regression).
// ---------------------------------------------------------------------------

const DUO_SESSION: &str = "11111111-2222-4333-8444-555555555555";
const DUO_NEEDLE: &str = "duo-carrier needle zeta 9f31";

fn bookkeeping_line(session: &str, mode: &str) -> String {
    json!({"type": "mode", "mode": mode, "sessionId": session}).to_string()
}

/// Exact phrase hits of one needle in a source's full-text index.
fn needle_hits(cache_root: &Path, source_id: &str, needle: &str) -> i64 {
    let db = cache_root.join("live").join(source_id).join("fts.sqlite");
    let conn = rusqlite::Connection::open(db).expect("index db");
    conn.query_row(
        "SELECT COUNT(*) FROM units WHERE units MATCH ?1",
        [format!("\"{needle}\"")],
        |row| row.get::<_, i64>(0),
    )
    .expect("needle count")
}

#[test]
fn one_session_across_two_carriers_projects_the_real_content() {
    let env = TestEnv::new();
    let archive_root = env.root("duo-archive");
    Archive::init(&archive_root).expect("init runtime root");
    let cache_root = env.root("duo-cache");
    let claude_root = env.root("duo-claude");
    let project = claude_root.join("proj");
    // `a-zero` sorts before `b-real`: the carrier without projectable content
    // is processed first, the one that holds the message second.
    write_lines(
        &project.join("a-zero.jsonl"),
        &[
            bookkeeping_line(DUO_SESSION, "plan"),
            bookkeeping_line(DUO_SESSION, "accept"),
        ],
    );
    write_lines(
        &project.join("b-real.jsonl"),
        &[claude_line(DUO_SESSION, "u-duo", DUO_NEEDLE)],
    );
    let config = env.write_json(
        "duo-recall.json",
        &json!({
            "version": 2,
            "sources": [
                {"source_id": "claude_code", "kind": "claude_code",
                 "root": claude_root.to_string_lossy(), "enabled": true},
                {"source_id": "archive", "kind": "archive",
                 "root": archive_root.to_string_lossy(), "enabled": false}
            ],
            "source_order": ["claude_code"],
            "interpreter": {"kb": {"command": "C:/python/python.exe"}},
            "domain": "default",
            "telemetry": {"enabled": false},
            "cache_root": cache_root.to_string_lossy()
        }),
    );
    {
        let parsed = load_config(&config).expect("config loads");
        let options = agent_memory::capture_all::CaptureAllOptions {
            root: None,
            domain: "default".to_string(),
            only_source: None,
            session_since: None,
            plan: false,
            skip_if_locked: false,
        };
        let (report, code) =
            agent_memory::capture_all::capture_all(&parsed, &options).expect("capture-all runs");
        assert_eq!(code, 0, "capture-all failed: {report:?}");
        assert!(report.ok, "capture-all not ok");
    }
    fs::remove_file(project.join("a-zero.jsonl")).expect("delete the zero-row carrier");
    fs::remove_file(project.join("b-real.jsonl")).expect("delete the real carrier");

    let run = || -> Vec<Value> {
        let parsed = load_config(&config).expect("config loads");
        index_all(&parsed, Some("claude_code"), false)
            .expect("index runs")
            .iter()
            .map(|report| serde_json::to_value(report).expect("report json"))
            .collect()
    };

    // Round 1: the session is projected from both carriers in the same run and
    // registered once; the message is searchable.
    let reports = run();
    let report = reports
        .iter()
        .find(|report| report["source_id"] == json!("claude_code"))
        .expect("claude report");
    let archive = archive_of(report);
    assert_eq!(archive["added"], 1, "{report}");
    assert_eq!(archive["skipped_registered_empty"], 0, "{report}");
    assert_eq!(
        needle_hits(&cache_root, "claude_code", DUO_NEEDLE),
        1,
        "the real carrier's content is indexed: {report}"
    );

    // Round 2: the registration counts the session once and it is Present (not
    // an empty registration), so it is not counted as skipped-empty and its
    // content stays searchable.
    let reports = run();
    let report = reports
        .iter()
        .find(|report| report["source_id"] == json!("claude_code"))
        .expect("claude report");
    let archive = archive_of(report);
    assert_eq!(archive["added"], 0, "{report}");
    assert_eq!(archive["skipped_registered_empty"], 0, "{report}");
    assert_eq!(
        needle_hits(&cache_root, "claude_code", DUO_NEEDLE),
        1,
        "the content survives the second round: {report}"
    );
}
