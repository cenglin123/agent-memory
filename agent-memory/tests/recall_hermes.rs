//! Hermes live-source integration tests (`hermes-sqlite-v1`).
//!
//! Synthetic `state.db` fixtures only: the shape follows the cc-switch hermes
//! provider schema (see `p3_sqlite_profiles::hermes_db`). No real Hermes home,
//! no real session content. Cover the plan's contract: source registration
//! (default off / explicit on / disabled), WAL visibility, time units and
//! ordering, exact + read round-trips, stale refs, corrupt/missing db and
//! non-session tables excluded.

use agent_memory::recall::config::{load_config, SourceKind};
use agent_memory::recall::fulltext::build::index_all;
use agent_memory::recall::ops::{execute, ExecutionOptions, Op};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

struct Fixture {
    root: PathBuf,
    _dir: tempfile::TempDir,
}

impl Fixture {
    fn db(&self) -> PathBuf {
        self.root.join("state.db")
    }
}

fn write_config(fixture: &Fixture, enabled: bool, db: Option<&Path>) -> PathBuf {
    let mut source = json!({
        "source_id": "hermes",
        "kind": "hermes",
        "root": fixture.root.to_string_lossy().replace('\\', "/"),
        "enabled": enabled,
    });
    if let Some(db) = db {
        source["db"] = json!(db.to_string_lossy().replace('\\', "/"));
    }
    // A config needs at least one enabled source: the device anchor keeps the
    // disabled-hermes case valid without answering for it.
    std::fs::create_dir_all(fixture.root.join("notes")).unwrap();
    let anchor = json!({
        "source_id": "device",
        "kind": "device",
        "root": fixture.root.join("notes").to_string_lossy().replace('\\', "/"),
        "enabled": true,
    });
    let mut source_order = vec!["device"];
    if enabled {
        source_order.insert(0, "hermes");
    }
    let config = json!({
        "version": 2,
        "sources": [source, anchor],
        "source_order": source_order,
        "interpreter": {"kb": {"command": "python", "args": ["-B"], "env": {}}},
        "domain": "default",
        "telemetry": {"enabled": false},
    });
    let path = fixture.root.join("recall.json");
    std::fs::write(&path, serde_json::to_string_pretty(&config).unwrap()).unwrap();
    path
}

fn write_indexed_config(fixture: &Fixture, mode: &str) -> PathBuf {
    let path = write_config(fixture, true, Some(&fixture.db()));
    let mut config: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let cache = fixture.root.parent().unwrap().join(format!("cache-{mode}"));
    config["cache_root"] = json!(cache.to_string_lossy().replace('\\', "/"));
    if mode == "hybrid" {
        config["interpreter"]["embedding"] = json!({
            "model_id": "hermes-test-v1",
            "dimension": 32,
            "normalization": "l2",
            "backend": "fake"
        });
    }
    std::fs::write(&path, serde_json::to_string_pretty(&config).unwrap()).unwrap();
    path
}

/// One session with a unique phrase plus two plain messages, and a second
/// session sharing a phrase to exercise session grouping. The `auth_tokens`
/// table carries a secret that must never surface.
fn hermes_db(path: &Path) {
    let conn = rusqlite::Connection::open(path).unwrap();
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
             display_metadata TEXT, reasoning TEXT);
        CREATE TABLE auth_tokens (
             id INTEGER PRIMARY KEY, label TEXT, token TEXT NOT NULL);
        INSERT INTO sessions VALUES
             ('sess-a', '/work/alpha', 'alpha session', 100.0, NULL, 500.0),
             ('sess-b', '/work/beta',  'beta session',  200.0, NULL, 900.0);
        INSERT INTO messages (session_id, role, content, timestamp) VALUES
             ('sess-a', 'user',      'first user question',          110.0),
             ('sess-a', 'assistant', 'an answer without the phrase', 120.0),
             ('sess-a', 'user',      'hits UNIQUE_PHRASE once',      130.0),
             ('sess-a', 'assistant', 'model-only note',              140.0),
             ('sess-b', 'user',      'beta mentions UNIQUE_PHRASE too', 850.0);
        UPDATE messages
            SET content = 'model-only MODEL_ONLY_SENTINEL',
                display_metadata = '{"model_only":true}'
            WHERE id = 4;
        INSERT INTO auth_tokens VALUES (1, 'ci', 'sentinel-hermes-token-never-archive');
        "#,
    )
    .unwrap();
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("hermes-home");
    std::fs::create_dir_all(&root).unwrap();
    let fixture = Fixture { root, _dir: dir };
    hermes_db(&fixture.db());
    fixture
}

fn run(op: Op, request: &Value, config: &Path) -> Value {
    let config = load_config(config).unwrap();
    let executed = execute(&config, op, request, &ExecutionOptions::default());
    serde_json::to_value(&executed.value).unwrap()
}

fn search(request: Value, config: &Path) -> Value {
    run(Op::Search, &request, config)
}

fn read(request: Value, config: &Path) -> Value {
    run(Op::Read, &request, config)
}

fn recent(request: Value, config: &Path) -> Value {
    run(Op::Recent, &request, config)
}

fn first_hit(response: &Value) -> Value {
    response["results"][0].clone()
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

#[test]
fn default_config_does_not_discover_hermes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("recall.json");
    std::fs::write(
        &path,
        json!({
            "version": 2,
            "sources": [{"source_id": "kb", "kind": "kb",
                         "root": dir.path().to_string_lossy().replace('\\', "/"),
                         "enabled": true}],
            "source_order": ["kb"],
            "interpreter": {"kb": {"command": "python", "args": ["-B"], "env": {}}},
            "domain": "default",
            "telemetry": {"enabled": false},
        })
        .to_string(),
    )
    .unwrap();
    let config = load_config(&path).unwrap();
    assert!(config
        .sources
        .iter()
        .all(|source| source.kind != SourceKind::Hermes));
}

#[test]
fn explicit_source_registers_and_disabled_source_skips() {
    let fixture = fixture();
    let enabled = write_config(&fixture, true, Some(&fixture.db()));
    let config = load_config(&enabled).unwrap();
    assert_eq!(config.sources[0].kind, SourceKind::Hermes);
    assert!(config.sources[0].enabled);

    let disabled = write_config(&fixture, false, Some(&fixture.db()));
    let config = load_config(&disabled).unwrap();
    assert!(!config.sources[0].enabled);
    let response = search(
        json!({"query": "unique phrase", "sources": ["hermes"]}),
        &disabled,
    );
    let status = &response["source_status"][0];
    assert_ne!(
        status["status"], "ok",
        "a disabled source must not answer ok"
    );
}

#[test]
fn db_outside_the_explicit_root_is_rejected() {
    let fixture = fixture();
    let outside = fixture.root.parent().unwrap().join("other-profile.db");
    let config = write_config(&fixture, true, Some(&outside));
    let error = load_config(&config).unwrap_err();
    assert_eq!(error.detail.unwrap()["reason"], "outside_root");
}

// ---------------------------------------------------------------------------
// Search: exact hit, exclusion of non-projected rows, budget detail
// ---------------------------------------------------------------------------

#[test]
fn exact_search_hits_the_unique_phrase_and_reads_back_verbatim() {
    let fixture = fixture();
    let config = write_config(&fixture, true, Some(&fixture.db()));
    let response = search(
        json!({"query": "hits unique phrase", "sources": ["hermes"]}),
        &config,
    );
    let status = response["source_status"][0]["status"].as_str().unwrap();
    assert!(status == "ok" || status == "degraded", "status: {status}");
    let hit = first_hit(&response);
    assert_eq!(hit["source_kind"], "hermes");
    assert_eq!(hit["ref"]["schema_gen"], "hermes-sqlite-v1");
    assert_eq!(hit["ref"]["source_kind"], "hermes");

    let text = hit["snippet"].as_str().unwrap();
    assert!(
        text.contains("UNIQUE_PHRASE"),
        "snippet carries the phrase: {text}"
    );
    let content = "hits UNIQUE_PHRASE once";
    let hit_session = hit["ref"]["locator"]["session_id"].as_str().unwrap();
    let read_response = read(json!({"ref": hit["ref"], "max_bytes": 4096}), &config);
    assert_eq!(read_response["ok"], true, "{}", read_response);
    let read_text = read_response["text"].as_str().unwrap();
    assert_eq!(read_text, content, "readback is the whole row content");
    assert_eq!(hit_session, "sess-a");
    assert_eq!(
        read_response["range_sha256"],
        read_response["segments"][0]["range_sha256"]
    );
}

#[test]
fn exact_index_round_trips_without_writing_the_source() {
    let fixture = fixture();
    let before = std::fs::read(fixture.db()).unwrap();
    let config_path = write_indexed_config(&fixture, "exact");
    let config = load_config(&config_path).unwrap();
    let reports = index_all(&config, Some("hermes"), false).unwrap();
    assert_eq!(reports[0].status, "built", "{:?}", reports[0]);
    assert_eq!(reports[0].rows, 4, "{:?}", reports[0]);

    let response = search(
        json!({"query": "hits unique phrase", "mode": "exact", "sources": ["hermes"]}),
        &config_path,
    );
    let detail = &response["source_status"][0]["detail"];
    assert_eq!(detail["resolved_mode"], "exact", "{response}");
    assert_eq!(detail["coverage"]["rows"], 4, "{response}");
    assert_ne!(detail["exact"]["mode"], "scan_fallback", "{response}");
    let hit = first_hit(&response);
    assert_eq!(hit["title"], "alpha session", "{response}");
    let read_response = read(json!({"ref": hit["ref"], "max_bytes": 4096}), &config_path);
    assert_eq!(
        read_response["text"], "hits UNIQUE_PHRASE once",
        "{read_response}"
    );
    assert_eq!(std::fs::read(fixture.db()).unwrap(), before);
}

#[test]
fn hybrid_index_round_trips_with_the_fake_embedding_backend() {
    let fixture = fixture();
    let config_path = write_indexed_config(&fixture, "hybrid");
    let config = load_config(&config_path).unwrap();
    let reports = index_all(&config, Some("hermes"), false).unwrap();
    assert_eq!(reports[0].status, "built", "{:?}", reports[0]);
    assert_eq!(reports[0].rows, 4, "{:?}", reports[0]);

    let response = search(
        json!({"query": "hits unique phrase", "mode": "hybrid", "sources": ["hermes"]}),
        &config_path,
    );
    let detail = &response["source_status"][0]["detail"];
    assert_eq!(detail["resolved_mode"], "hybrid", "{response}");
    assert_eq!(detail["hybrid"]["semantic"], "available", "{response}");
    assert_eq!(detail["coverage"]["rows"], 4, "{response}");
    let hit = first_hit(&response);
    assert!(hit["channels"]
        .as_array()
        .unwrap()
        .contains(&json!("lexical")));
    let read_response = read(json!({"ref": hit["ref"], "max_bytes": 4096}), &config_path);
    assert_eq!(
        read_response["text"], "hits UNIQUE_PHRASE once",
        "{read_response}"
    );
}

#[test]
fn indexed_exact_applies_seconds_as_milliseconds_at_filter_boundaries() {
    let fixture = fixture();
    let conn = rusqlite::Connection::open(fixture.db()).unwrap();
    conn.execute(
        "INSERT INTO messages (session_id, role, content, timestamp) VALUES \
         ('sess-a', 'user', 'fractional SECOND_BOUNDARY marker', 130.1234)",
        [],
    )
    .unwrap();
    drop(conn);
    let config_path = write_indexed_config(&fixture, "exact");
    let config = load_config(&config_path).unwrap();
    index_all(&config, Some("hermes"), false).unwrap();

    let included = search(
        json!({
            "query": "fractional second boundary", "mode": "exact", "sources": ["hermes"],
            "since": "1970-01-01T00:02:10.123Z",
            "until": "1970-01-01T00:02:10.124Z"
        }),
        &config_path,
    );
    assert_eq!(
        included["results"].as_array().unwrap().len(),
        1,
        "{included}"
    );
    let excluded = search(
        json!({
            "query": "fractional second boundary", "mode": "exact", "sources": ["hermes"],
            "until": "1970-01-01T00:02:10.123Z"
        }),
        &config_path,
    );
    assert!(
        excluded["results"].as_array().unwrap().is_empty(),
        "{excluded}"
    );
}

#[test]
fn search_answers_from_both_sessions_and_reports_details() {
    let fixture = fixture();
    let config = write_config(&fixture, true, Some(&fixture.db()));
    let response = search(
        json!({"query": "unique phrase", "sources": ["hermes"]}),
        &config,
    );
    let sessions: Vec<&str> = response["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|hit| hit["ref"]["locator"]["session_id"].as_str().unwrap())
        .collect();
    assert!(!sessions.is_empty(), "response: {}", response);
    let detail = &response["source_status"][0]["detail"];
    assert!(detail["sessions"].as_u64().unwrap() >= 2);
    assert!(detail["messages"].as_u64().unwrap() >= 4);
}

#[test]
fn secrets_and_non_projected_rows_never_surface() {
    let fixture = fixture();
    let config = write_config(&fixture, true, Some(&fixture.db()));
    for query in ["sentinel-hermes-token-never-archive", "model_only_sentinel"] {
        let response = search(json!({"query": query, "sources": ["hermes"]}), &config);
        let body = serde_json::to_string(&response).unwrap();
        assert!(!body.contains(query), "{body}");
        assert!(response["results"].as_array().unwrap().is_empty());
    }

    let config_path = write_indexed_config(&fixture, "exact");
    let parsed = load_config(&config_path).unwrap();
    index_all(&parsed, Some("hermes"), false).unwrap();
    let indexed = search(
        json!({"query": "model_only_sentinel", "mode": "exact", "sources": ["hermes"]}),
        &config_path,
    );
    assert!(
        indexed["results"].as_array().unwrap().is_empty(),
        "{indexed}"
    );
    assert_eq!(indexed["source_status"][0]["detail"]["coverage"]["rows"], 4);

    let recent_response = recent(json!({"sources": ["hermes"], "tail_turns": 10}), &config);
    assert!(!serde_json::to_string(&recent_response)
        .unwrap()
        .contains("MODEL_ONLY_SENTINEL"));

    let response = search(
        json!({"query": "hits unique phrase", "sources": ["hermes"]}),
        &config,
    );
    let mut model_only_ref = first_hit(&response)["ref"].clone();
    model_only_ref["locator"]["record_index"] = json!(4);
    model_only_ref["locator"]["message_id"] = json!("4");
    let read_response = read(json!({"ref": model_only_ref, "max_bytes": 4096}), &config);
    assert_eq!(read_response["ok"], false, "{read_response}");
    assert_eq!(
        read_response["code"], "REF_LOCATOR_MISSING",
        "{read_response}"
    );
}

#[test]
fn session_filter_scopes_to_one_session() {
    let fixture = fixture();
    let config = write_config(&fixture, true, Some(&fixture.db()));
    let response = search(
        json!({"query": "unique phrase", "sources": ["hermes"], "session_id": "sess-b"}),
        &config,
    );
    let sessions: Vec<&str> = response["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|hit| hit["ref"]["locator"]["session_id"].as_str().unwrap())
        .collect();
    assert_eq!(sessions, vec!["sess-b"], "{response}");
}

#[test]
fn scan_fallback_keeps_all_requested_scope_filters() {
    let fixture = fixture();
    let config = write_config(&fixture, true, Some(&fixture.db()));
    for extra in [
        json!({"roles": ["assistant"]}),
        json!({"kinds": ["tool_input"]}),
        json!({"session_kinds": ["worker"]}),
        json!({"until": "1970-01-01T00:02:00Z"}),
    ] {
        let mut request = json!({
            "query": "unique", "mode": "exact", "sources": ["hermes"]
        });
        request
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        let response = search(request, &config);
        assert!(
            response["results"].as_array().unwrap().is_empty(),
            "{response}"
        );
        assert_eq!(response["source_status"][0]["status"], "degraded");
    }

    let response = search(
        json!({
            "query": "unique phrase", "mode": "exact", "sources": ["hermes"],
            "roles": ["user"], "kinds": ["text"], "session_kinds": ["interactive"],
            "since": "1970-01-01T00:02:10Z", "until": "1970-01-01T00:14:11Z"
        }),
        &config,
    );
    assert_eq!(
        response["results"].as_array().unwrap().len(),
        2,
        "{response}"
    );
}

#[test]
fn budget_exhaustion_keeps_the_budget_error_code() {
    let fixture = fixture();
    let conn = rusqlite::Connection::open(fixture.db()).unwrap();
    let content = format!("BUDGET_SENTINEL {}", "x".repeat(9_000));
    conn.execute(
        "INSERT INTO messages (session_id, role, content, timestamp) VALUES \
         ('sess-a', 'user', ?1, 160.0)",
        [content],
    )
    .unwrap();
    drop(conn);
    let config = write_config(&fixture, true, Some(&fixture.db()));
    let response = search(
        json!({
            "query": "budget_sentinel", "sources": ["hermes"],
            "budget": {"max_bytes_per_source": 8192}
        }),
        &config,
    );
    assert_eq!(
        response["source_status"][0]["status"], "truncated",
        "{response}"
    );
    assert_eq!(
        response["source_status"][0]["code"], "BUDGET_BYTES",
        "{response}"
    );
    assert!(response["errors"]
        .as_array()
        .unwrap()
        .iter()
        .any(|error| error["code"] == "BUDGET_BYTES"));
}

// ---------------------------------------------------------------------------
// Recent: ordering, tail, session filter
// ---------------------------------------------------------------------------

#[test]
fn recent_lists_sessions_newest_first_with_tails() {
    let fixture = fixture();
    let config = write_config(&fixture, true, Some(&fixture.db()));
    let response = recent(json!({"limit": 5, "tail_turns": 2}), &config);
    let sessions = response["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 2, "{}", response);
    assert_eq!(sessions[0]["session_id"], "sess-b", "900 s > 500 s");
    assert_eq!(sessions[1]["session_id"], "sess-a");
    assert_eq!(sessions[0]["source"], "hermes");
    assert_eq!(sessions[0]["session_kind"], "interactive");
    assert!(sessions[0]["last_activity_at"]
        .as_str()
        .unwrap()
        .contains("1970-01-01"));
    let tail = sessions[1]["tail"].as_array().unwrap();
    assert!(!tail.is_empty());
    assert_eq!(tail[0]["text"], "an answer without the phrase");
    assert_eq!(tail[1]["text"], "hits UNIQUE_PHRASE once");
    let read_response = read(json!({"ref": tail[1]["ref"], "max_bytes": 4096}), &config);
    assert_eq!(
        read_response["text"], "hits UNIQUE_PHRASE once",
        "{read_response}"
    );

    let no_tail = recent(json!({"sources": ["hermes"], "tail_turns": 0}), &config);
    assert!(no_tail["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .all(|session| session["tail"].as_array().unwrap().is_empty()));
}

#[test]
fn recent_session_filter_returns_one_session() {
    let fixture = fixture();
    let conn = rusqlite::Connection::open(fixture.db()).unwrap();
    conn.execute(
        "UPDATE sessions SET last_activity_at = NULL WHERE id = 'sess-a'",
        [],
    )
    .unwrap();
    drop(conn);
    let config = write_config(&fixture, true, Some(&fixture.db()));
    let response = recent(
        json!({
            "sources": ["hermes"], "session_id": "sess-a", "tail_turns": 3,
            "since": "1970-01-01T00:01:40Z"
        }),
        &config,
    );
    let sessions = response["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 1, "{response}");
    assert_eq!(sessions[0]["session_id"], "sess-a");
    assert_eq!(sessions[0]["last_activity_at"], "1970-01-01T00:01:40Z");
}

#[test]
fn recent_equal_timestamps_use_a_stable_session_id_tiebreak() {
    let fixture = fixture();
    let conn = rusqlite::Connection::open(fixture.db()).unwrap();
    conn.execute("UPDATE sessions SET last_activity_at = 500.0", [])
        .unwrap();
    drop(conn);
    let config = write_config(&fixture, true, Some(&fixture.db()));
    let response = recent(
        json!({"sources": ["hermes"], "limit": 1, "tail_turns": 0}),
        &config,
    );
    let sessions = response["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 1, "{response}");
    assert_eq!(sessions[0]["session_id"], "sess-a", "{response}");
}

#[test]
fn recent_honors_exclusions_roles_and_character_limits() {
    let fixture = fixture();
    let config = write_config(&fixture, true, Some(&fixture.db()));
    let excluded = recent(
        json!({"sources": ["hermes"], "exclude_session_ids": ["sess-b"]}),
        &config,
    );
    let sessions = excluded["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 1, "{excluded}");
    assert_eq!(sessions[0]["session_id"], "sess-a");

    let response = recent(
        json!({
            "sources": ["hermes"], "session_id": "sess-a", "tail_turns": 3,
            "tail_roles": ["assistant"], "max_chars_per_turn": 5
        }),
        &config,
    );
    let tail = response["sessions"][0]["tail"].as_array().unwrap();
    assert_eq!(tail.len(), 1, "{response}");
    assert_eq!(tail[0]["role"], "assistant");
    assert_eq!(tail[0]["text"], "an an", "{response}");
    assert_eq!(tail[0]["truncated"], true, "{response}");
}

// ---------------------------------------------------------------------------
// WAL visibility and lock behaviour
// ---------------------------------------------------------------------------

#[test]
fn wal_commit_becomes_visible_to_the_next_query() {
    let fixture = fixture();
    let config = write_config(&fixture, true, Some(&fixture.db()));
    // A writer connection holds the WAL; the reader still sees committed rows.
    let writer = rusqlite::Connection::open(fixture.db()).unwrap();
    writer.execute_batch("PRAGMA journal_mode=WAL;").unwrap();
    writer
        .execute(
            "INSERT INTO messages (session_id, role, content, timestamp) VALUES \
             ('sess-a', 'user', 'wal-written UNIQUE_PHRASE tail', 150.0)",
            [],
        )
        .unwrap();
    let response = search(
        json!({"query": "wal-written UNIQUE_PHRASE tail", "sources": ["hermes"]}),
        &config,
    );
    let snippets: Vec<&str> = response["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|hit| hit["snippet"].as_str().unwrap())
        .collect();
    assert!(
        snippets
            .iter()
            .any(|snippet| snippet.contains("wal-written")),
        "the WAL commit is visible to the reader: {snippets:?}"
    );
    drop(writer);
}

#[test]
fn wal_commit_invalidates_and_updates_the_exact_index() {
    let fixture = fixture();
    let writer = rusqlite::Connection::open(fixture.db()).unwrap();
    writer.execute_batch("PRAGMA journal_mode=WAL;").unwrap();
    let config_path = write_indexed_config(&fixture, "exact");
    let config = load_config(&config_path).unwrap();
    let first = index_all(&config, Some("hermes"), false).unwrap();
    assert_eq!(first[0].rows, 4, "{:?}", first[0]);

    writer
        .execute(
            "INSERT INTO messages (session_id, role, content, timestamp) VALUES \
             ('sess-a', 'user', 'wal-indexed UNIQUE_PHRASE tail', 150.0)",
            [],
        )
        .unwrap();
    let stale = search(
        json!({"query": "unique phrase", "mode": "exact", "sources": ["hermes"]}),
        &config_path,
    );
    assert!(stale["source_status"][0]["detail"]["coverage"]["stale_since"].is_string());
    let second = index_all(&config, Some("hermes"), false).unwrap();
    assert_eq!(second[0].rows, 5, "{:?}", second[0]);
    assert_eq!(second[0].added, 5, "{:?}", second[0]);
    assert!(second[0].rebuilt, "{:?}", second[0]);
    let third = index_all(&config, Some("hermes"), false).unwrap();
    assert_eq!(third[0].status, "unchanged", "{:?}", third[0]);
    assert_eq!(third[0].added, 0, "{:?}", third[0]);
    assert!(!third[0].rebuilt, "{:?}", third[0]);
    let response = search(
        json!({"query": "wal-indexed unique phrase", "mode": "exact", "sources": ["hermes"]}),
        &config_path,
    );
    assert_eq!(
        response["results"].as_array().unwrap().len(),
        1,
        "{response}"
    );
    assert_eq!(
        response["source_status"][0]["detail"]["coverage"]["rows"], 5,
        "{response}"
    );
    assert!(response["source_status"][0]["detail"]["coverage"]
        .get("stale_since")
        .is_none());
    let read_response = read(
        json!({"ref": response["results"][0]["ref"], "max_bytes": 4096}),
        &config_path,
    );
    assert_eq!(
        read_response["text"], "wal-indexed UNIQUE_PHRASE tail",
        "{read_response}"
    );
    let recent_response = recent(
        json!({"sources": ["hermes"], "session_id": "sess-a", "tail_turns": 1}),
        &config_path,
    );
    assert_eq!(
        recent_response["sessions"][0]["tail"][0]["text"],
        "wal-indexed UNIQUE_PHRASE tail"
    );
    drop(writer);
}

#[test]
fn failed_rebuild_rolls_back_and_preserves_the_previous_index() {
    let fixture = fixture();
    let config_path = write_indexed_config(&fixture, "exact");
    let config = load_config(&config_path).unwrap();
    let first = index_all(&config, Some("hermes"), false).unwrap();
    assert_eq!(first[0].rows, 4, "{:?}", first[0]);

    let conn = rusqlite::Connection::open(fixture.db()).unwrap();
    conn.execute(
        "INSERT INTO messages (session_id, role, content, timestamp) VALUES \
         ('sess-a', 'user', X'00', 160.0)",
        [],
    )
    .unwrap();
    drop(conn);
    let failed = index_all(&config, Some("hermes"), false).unwrap();
    assert!(failed[0].error.is_some(), "{:?}", failed[0]);

    let response = search(
        json!({"query": "hits unique phrase", "mode": "exact", "sources": ["hermes"]}),
        &config_path,
    );
    assert_eq!(
        response["results"].as_array().unwrap().len(),
        1,
        "{response}"
    );
    assert_eq!(response["results"][0]["snippet"], "hits UNIQUE_PHRASE once");
    assert_eq!(
        response["source_status"][0]["detail"]["coverage"]["rows"],
        4
    );
    assert_ne!(
        response["source_status"][0]["detail"]["exact"]["mode"],
        "scan_fallback"
    );
}

// ---------------------------------------------------------------------------
// Stale and missing refs fail closed
// ---------------------------------------------------------------------------

#[test]
fn stale_ref_after_row_change_is_rejected() {
    let fixture = fixture();
    let config = write_config(&fixture, true, Some(&fixture.db()));
    let response = search(
        json!({"query": "hits unique phrase", "sources": ["hermes"]}),
        &config,
    );
    let hit = first_hit(&response);
    let stale_ref = hit["ref"].clone();
    let session_id = stale_ref["locator"]["session_id"].as_str().unwrap();
    let message_id = stale_ref["locator"]["record_index"].as_i64().unwrap();
    let mut bad_row_hash = stale_ref.clone();
    bad_row_hash["fingerprint"]["row_sha256"] = json!("0".repeat(64));
    let read_response = read(json!({"ref": bad_row_hash, "max_bytes": 4096}), &config);
    assert_eq!(read_response["code"], "REF_STALE", "{read_response}");
    assert_eq!(stale_ref["text_range"]["byte_end"], 23);
    assert_eq!("the source row changed!".len(), 23);

    let conn = rusqlite::Connection::open(fixture.db()).unwrap();
    conn.execute(
        "UPDATE messages SET content = 'the source row changed!' \
         WHERE session_id = ?1 AND id = ?2",
        rusqlite::params![session_id, message_id],
    )
    .unwrap();
    drop(conn);

    let read_response = read(json!({"ref": stale_ref, "max_bytes": 4096}), &config);
    assert_eq!(read_response["ok"], false, "{}", read_response);
    assert_eq!(read_response["code"], "REF_STALE", "{}", read_response);
}

#[test]
fn read_honors_max_bytes_and_recomputes_the_returned_range() {
    let fixture = fixture();
    let config = write_config(&fixture, true, Some(&fixture.db()));
    let response = search(
        json!({"query": "hits unique phrase", "sources": ["hermes"]}),
        &config,
    );
    let reference = first_hit(&response)["ref"].clone();
    let read_response = read(json!({"ref": reference.clone(), "max_bytes": 4}), &config);
    assert_eq!(read_response["ok"], true, "{read_response}");
    assert_eq!(read_response["text"], "hits", "{read_response}");
    assert_eq!(read_response["truncated"], true, "{read_response}");
    assert_eq!(
        read_response["text_range"]["byte_end"], 4,
        "{read_response}"
    );
    assert_eq!(
        read_response["range_sha256"],
        read_response["segments"][0]["range_sha256"]
    );
    assert_eq!(
        read_response["range_sha256"],
        agent_memory::recall::hash::sha256_hex(b"hits")
    );
    assert_ne!(
        read_response["range_sha256"],
        reference["fingerprint"]["range_sha256"]
    );
}

#[test]
fn read_context_uses_projected_neighbours_and_range_is_fail_closed() {
    let fixture = fixture();
    let config = write_config(&fixture, true, Some(&fixture.db()));
    let response = search(
        json!({"query": "an answer without the phrase", "sources": ["hermes"]}),
        &config,
    );
    let reference = first_hit(&response)["ref"].clone();
    let read_response = read(
        json!({
            "ref": reference.clone(), "max_bytes": 4096,
            "context_before": 1, "context_after": 1
        }),
        &config,
    );
    let texts: Vec<&str> = read_response["segments"]
        .as_array()
        .unwrap()
        .iter()
        .map(|segment| segment["text"].as_str().unwrap())
        .collect();
    assert_eq!(
        texts,
        vec![
            "first user question",
            "an answer without the phrase",
            "hits UNIQUE_PHRASE once"
        ],
        "{read_response}"
    );

    let mut bad_range = reference;
    bad_range["text_range"]["byte_end"] =
        json!(bad_range["text_range"]["byte_end"].as_u64().unwrap() + 1);
    let read_response = read(json!({"ref": bad_range, "max_bytes": 4096}), &config);
    assert_eq!(read_response["ok"], false, "{read_response}");
    assert_eq!(read_response["code"], "REF_STALE", "{read_response}");
}

#[test]
fn ref_to_a_missing_message_fails_closed() {
    let fixture = fixture();
    let config = write_config(&fixture, true, Some(&fixture.db()));
    let response = search(
        json!({"query": "unique phrase", "sources": ["hermes"]}),
        &config,
    );
    let mut missing = first_hit(&response)["ref"].clone();
    missing["locator"]["record_index"] = json!(99_999);
    missing["locator"]["message_id"] = json!("99999");
    let read_response = read(json!({"ref": missing, "max_bytes": 4096}), &config);
    assert_eq!(read_response["ok"], false, "{}", read_response);
}

#[test]
fn ref_from_another_root_is_rejected() {
    let fixture = fixture();
    let config = write_config(&fixture, true, Some(&fixture.db()));
    let response = search(
        json!({"query": "unique phrase", "sources": ["hermes"]}),
        &config,
    );
    let mut forged = first_hit(&response)["ref"].clone();
    forged["root_sha256"] = json!("2".repeat(64));
    let read_response = read(json!({"ref": forged, "max_bytes": 4096}), &config);
    assert_eq!(read_response["ok"], false, "{}", read_response);
}

#[test]
fn ref_must_stay_bound_to_the_configured_db_and_message_id() {
    let fixture = fixture();
    let config = write_config(&fixture, true, Some(&fixture.db()));
    let response = search(
        json!({"query": "hits unique phrase", "sources": ["hermes"]}),
        &config,
    );
    let reference = first_hit(&response)["ref"].clone();

    let mut wrong_db = reference.clone();
    wrong_db["rel_path"] = json!("other-profile.db");
    let read_response = read(json!({"ref": wrong_db, "max_bytes": 4096}), &config);
    assert_eq!(read_response["ok"], false, "{read_response}");

    let mut wrong_message = reference;
    wrong_message["locator"]["message_id"] = json!("999999");
    let read_response = read(json!({"ref": wrong_message, "max_bytes": 4096}), &config);
    assert_eq!(read_response["ok"], false, "{read_response}");
}

// ---------------------------------------------------------------------------
// Missing / corrupt / locked db
// ---------------------------------------------------------------------------

#[test]
fn missing_db_is_source_root_invalid() {
    let fixture = fixture();
    let phantom = fixture.root.join("absent.db");
    let config = write_config(&fixture, true, Some(&phantom));
    let response = search(
        json!({"query": "unique phrase", "sources": ["hermes"]}),
        &config,
    );
    assert_eq!(response["source_status"][0]["status"], "unavailable");
}

#[test]
fn missing_db_also_fails_read_and_recent() {
    let fixture = fixture();
    let config = write_config(&fixture, true, Some(&fixture.db()));
    let response = search(
        json!({"query": "hits unique phrase", "sources": ["hermes"]}),
        &config,
    );
    let reference = first_hit(&response)["ref"].clone();
    std::fs::remove_file(fixture.db()).unwrap();

    let read_response = read(json!({"ref": reference, "max_bytes": 4096}), &config);
    assert_eq!(read_response["ok"], false, "{read_response}");
    assert_eq!(read_response["code"], "DB_UNAVAILABLE", "{read_response}");
    let recent_response = recent(json!({"sources": ["hermes"], "tail_turns": 1}), &config);
    assert_eq!(recent_response["source_status"][0]["status"], "unavailable");
    assert_eq!(
        recent_response["source_status"][0]["code"],
        "SOURCE_ROOT_INVALID"
    );
}

#[test]
fn corrupt_db_is_db_unavailable() {
    let fixture = fixture();
    std::fs::write(fixture.db(), b"this is not a sqlite database").unwrap();
    let config = write_config(&fixture, true, Some(&fixture.db()));
    let response = search(
        json!({"query": "unique phrase", "sources": ["hermes"]}),
        &config,
    );
    assert_eq!(response["source_status"][0]["status"], "unavailable");
    assert_eq!(response["source_status"][0]["code"], "DB_UNAVAILABLE");
}

#[test]
fn existing_index_does_not_hide_a_missing_or_corrupt_source() {
    for corrupt in [false, true] {
        let fixture = fixture();
        let config_path = write_indexed_config(&fixture, "exact");
        let parsed = load_config(&config_path).unwrap();
        index_all(&parsed, Some("hermes"), false).unwrap();
        if corrupt {
            std::fs::write(fixture.db(), b"not sqlite after indexing").unwrap();
        } else {
            std::fs::remove_file(fixture.db()).unwrap();
        }
        let response = search(
            json!({"query": "unique phrase", "mode": "exact", "sources": ["hermes"]}),
            &config_path,
        );
        assert_eq!(
            response["source_status"][0]["status"], "unavailable",
            "{response}"
        );
        assert_eq!(
            response["source_status"][0]["code"], "DB_UNAVAILABLE",
            "{response}"
        );
        assert!(
            response["results"].as_array().unwrap().is_empty(),
            "{response}"
        );
    }
}

#[test]
fn existing_index_does_not_hide_a_schema_missing_projection_columns() {
    for missing in ["started_at", "active", "session_id"] {
        let fixture = fixture();
        let config_path = write_indexed_config(&fixture, "exact");
        let parsed = load_config(&config_path).unwrap();
        index_all(&parsed, Some("hermes"), false).unwrap();
        std::fs::remove_file(fixture.db()).unwrap();
        let conn = rusqlite::Connection::open(fixture.db()).unwrap();
        let sessions = if missing == "started_at" {
            "CREATE TABLE sessions (id TEXT PRIMARY KEY, cwd TEXT, title TEXT, last_activity_at REAL)"
        } else {
            "CREATE TABLE sessions (id TEXT PRIMARY KEY, cwd TEXT, title TEXT, started_at REAL, last_activity_at REAL)"
        };
        let messages = match missing {
            "active" => {
                "CREATE TABLE messages (id INTEGER PRIMARY KEY, session_id TEXT, role TEXT, content TEXT, timestamp REAL, display_metadata TEXT)"
            }
            "session_id" => {
                "CREATE TABLE messages (id INTEGER PRIMARY KEY, role TEXT, content TEXT, timestamp REAL, active INTEGER, display_metadata TEXT)"
            }
            _ => {
                "CREATE TABLE messages (id INTEGER PRIMARY KEY, session_id TEXT, role TEXT, content TEXT, timestamp REAL, active INTEGER, display_metadata TEXT)"
            }
        };
        conn.execute_batch(&format!("{sessions}; {messages};"))
            .unwrap();
        drop(conn);

        let response = search(
            json!({"query": "unique phrase", "mode": "exact", "sources": ["hermes"]}),
            &config_path,
        );
        assert_eq!(
            response["source_status"][0]["status"], "unavailable",
            "{response}"
        );
        assert_eq!(
            response["source_status"][0]["code"], "DB_UNAVAILABLE",
            "{response}"
        );
        assert!(
            response["results"].as_array().unwrap().is_empty(),
            "{response}"
        );
    }
}

#[test]
fn exclusive_writer_lock_degrades_the_request() {
    let fixture = fixture();
    let config = write_config(&fixture, true, Some(&fixture.db()));
    let initial = search(
        json!({"query": "hits unique phrase", "sources": ["hermes"]}),
        &config,
    );
    let reference = first_hit(&initial)["ref"].clone();
    let writer = rusqlite::Connection::open(fixture.db()).unwrap();
    writer
        .execute_batch("PRAGMA journal_mode=DELETE; BEGIN EXCLUSIVE;")
        .unwrap();
    let response = search(
        json!({"query": "unique phrase", "sources": ["hermes"]}),
        &config,
    );
    assert_eq!(
        response["source_status"][0]["status"], "degraded",
        "{response}"
    );
    assert_eq!(
        response["source_status"][0]["code"], "DB_LOCKED",
        "{response}"
    );
    let read_response = read(json!({"ref": reference, "max_bytes": 4096}), &config);
    assert_eq!(read_response["ok"], false, "{read_response}");
    assert_eq!(read_response["code"], "DB_LOCKED", "{read_response}");
    let recent_response = recent(json!({"sources": ["hermes"], "tail_turns": 1}), &config);
    assert_eq!(recent_response["source_status"][0]["status"], "degraded");
    assert_eq!(recent_response["source_status"][0]["code"], "DB_LOCKED");
    writer.execute_batch("ROLLBACK;").unwrap();
}
