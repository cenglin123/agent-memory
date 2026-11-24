//! Full-text L2/L3: live-source index building, incrementality, exact search,
//! explicit degradation and MCP read-only behavior. All fixtures are synthetic
//! and live under a per-test temporary root.

use agent_memory::recall::config::load_config;
use agent_memory::recall::fulltext::build::index_all;
use agent_memory::recall::fulltext::index_path;
use agent_memory::recall::search::{memory_search, SearchRequest};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

mod support;

const CACHE: &str = "cache";

fn v2_config(sources: Vec<Value>, order: Vec<&str>, cache_root: Option<&Path>) -> Value {
    let mut value = json!({
        "version": 2,
        "sources": sources,
        "source_order": order,
        "interpreter": {"kb": {"command": "C:/python/python.exe"}},
        "domain": "default",
        "telemetry": {"enabled": false}
    });
    if let Some(cache_root) = cache_root {
        value["cache_root"] = json!(cache_root.to_string_lossy());
    }
    value
}

fn source(id: &str, kind: &str, root: &Path) -> Value {
    json!({
        "source_id": id,
        "kind": kind,
        "root": root.to_string_lossy(),
        "enabled": true,
    })
}

fn cache_of(env: &support::TestEnv) -> PathBuf {
    env.root(CACHE)
}

fn search(config: &Path, request: Value) -> (i32, Value) {
    let parsed = load_config(config).expect("config loads");
    let request = SearchRequest::parse(&request).expect("request parses");
    let response = memory_search(&parsed, &request);
    (response.exit_code, serde_json::to_value(&response).unwrap())
}

fn search_request(config: &Path, request: Value) -> Value {
    search(config, request).1
}

fn exact_request(query: &str, sources: Vec<&str>) -> Value {
    json!({"query": query, "mode": "exact", "sources": sources, "limit": 20})
}

fn status_of(response: &Value, source_id: &str) -> String {
    response["source_status"]
        .as_array()
        .expect("source_status")
        .iter()
        .find(|entry| entry["source_id"] == json!(source_id))
        .map(|entry| entry["status"].as_str().unwrap_or_default().to_string())
        .unwrap_or_default()
}

fn detail_of(response: &Value, source_id: &str) -> Value {
    response["source_status"]
        .as_array()
        .expect("source_status")
        .iter()
        .find(|entry| entry["source_id"] == json!(source_id))
        .map(|entry| entry["detail"].clone())
        .unwrap_or(Value::Null)
}

fn hit_ids(response: &Value) -> Vec<String> {
    response["results"]
        .as_array()
        .expect("results")
        .iter()
        .map(|hit| hit["hit_id"].as_str().unwrap_or_default().to_string())
        .collect()
}



// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

const OPENCODE_SCHEMA: &str = "
CREATE TABLE session_v2 (
    id TEXT PRIMARY KEY,
    title TEXT,
    directory TEXT,
    time_created INTEGER,
    time_updated INTEGER,
    parent_id TEXT
);
CREATE TABLE session_message (
    id TEXT PRIMARY KEY,
    session_id TEXT,
    seq INTEGER,
    type TEXT,
    time_created INTEGER,
    time_updated INTEGER,
    data TEXT
);
CREATE INDEX session_message_session_idx ON session_message(session_id, seq);
";

struct OpenCodeFixture {
    root: PathBuf,
    db: PathBuf,
}

fn build_opencode(env: &support::TestEnv) -> OpenCodeFixture {
    let root = env.root("oc");
    let db = root.join("opencode.db");
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute_batch(OPENCODE_SCHEMA).unwrap();
    let sessions = [
        ("s-main", "项目讨论", None, 1_000i64, 9_000i64),
        ("s-worker", "child session", Some("s-main"), 900, 8_000),
        ("s-ocsr", "ocsr-dispatch", None, 800, 7_000),
        ("s-sub", "review (subagent)", None, 700, 6_000),
    ];
    for (id, title, parent, created, updated) in sessions {
        conn.execute(
            "INSERT INTO session_v2(id, title, directory, time_created, time_updated, parent_id) \
             VALUES (?1, ?2, 'C:/work', ?3, ?4, ?5)",
            rusqlite::params![id, title, created, updated, parent],
        )
        .unwrap();
    }
    let messages: Vec<(&str, &str, i64, i64, &str, Value)> = vec![
        (
            "sm-1",
            "s-main",
            1,
            1_700_000_000_000,
            "user",
            json!({"text": "PrependPath=1 装回 Python 3.13.17"}),
        ),
        (
            "sm-2",
            "s-main",
            2,
            1_700_000_001_000,
            "assistant",
            json!({"content": [
                {"type": "text", "text": "关键是 interpreter.kb 指向 Python313"},
                {"type": "tool", "tool": "bash", "state": {"input": {"command": "python -V"}, "output": "Python 3.13.17"}},
                {"type": "reasoning", "text": "internal reasoning marker"}
            ]}),
        ),
        (
            "sm-3",
            "s-main",
            3,
            1_700_000_002_000,
            "assistant",
            json!({"content": [
                {"type": "tool", "tool": "bash", "state": {"input": {"command": "dump"}, "output": format!("{}{}", "longoutput ".repeat(2_500), "TAILMARK")}}
            ]}),
        ),
        (
            "sm-worker",
            "s-worker",
            1,
            1_700_000_003_000,
            "user",
            json!({"text": "PrependPath=1 worker echo"}),
        ),
        (
            "sm-ocsr",
            "s-ocsr",
            1,
            1_700_000_004_000,
            "user",
            json!({"text": "PrependPath=1 ocsr echo"}),
        ),
        (
            "sm-sub",
            "s-sub",
            1,
            1_700_000_005_000,
            "user",
            json!({"text": "PrependPath=1 subagent echo"}),
        ),
    ];
    for (id, session, seq, created, kind, data) in messages {
        conn.execute(
            "INSERT INTO session_message(id, session_id, seq, type, time_created, time_updated, data) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?5, ?6)",
            rusqlite::params![id, session, seq, kind, created, serde_json::to_string(&data).unwrap()],
        )
        .unwrap();
    }
    drop(conn);
    OpenCodeFixture { root, db }
}

fn insert_message(db: &Path, id: &str, session: &str, seq: i64, created: i64, text: &str) {
    let conn = rusqlite::Connection::open(db).unwrap();
    conn.execute(
        "INSERT INTO session_message(id, session_id, seq, type, time_created, time_updated, data) \
         VALUES (?1, ?2, ?3, 'user', ?4, ?4, ?5)",
        rusqlite::params![
            id,
            session,
            seq,
            created,
            serde_json::to_string(&json!({"text": text})).unwrap()
        ],
    )
    .unwrap();
}

struct ClaudeFixture {
    root: PathBuf,
    main: PathBuf,
    sub: PathBuf,
}

fn claude_line(
    session: &str,
    uuid: &str,
    time: &str,
    kind: &str,
    content: Value,
    sidechain: bool,
) -> String {
    json!({
        "type": kind,
        "message": {"role": kind, "content": content},
        "timestamp": time,
        "sessionId": session,
        "uuid": uuid,
        "isSidechain": sidechain
    })
    .to_string()
}

fn build_claude(env: &support::TestEnv) -> ClaudeFixture {
    let root = env.root("claude");
    let project = root.join("proj");
    std::fs::create_dir_all(&project).unwrap();
    let session = "11111111-1111-4111-8111-111111111111";
    let main = project.join(format!("{session}.jsonl"));
    let lines = vec![
        claude_line(
            session,
            "u1",
            "2026-10-25T13:25:00Z",
            "user",
            json!("PrependPath=1 装回 Python 3.13.17"),
            false,
        ),
        claude_line(
            session,
            "a1",
            "2026-10-25T13:26:00Z",
            "assistant",
            json!([
                {"type": "text", "text": "interpreter.kb 指到 Python313"},
                {"type": "tool_use", "id": "t1", "name": "Bash", "input": {"command": "python -V"}}
            ]),
            false,
        ),
        claude_line(
            session,
            "u2",
            "2026-10-25T13:27:00Z",
            "user",
            json!([
                {"type": "tool_result", "tool_use_id": "t1", "content": "Python 3.13.17"}
            ]),
            false,
        ),
        claude_line(
            session,
            "m1",
            "2026-10-25T13:28:00Z",
            "user",
            json!("<system-reminder>injected reminder text</system-reminder>"),
            false,
        ),
    ];
    std::fs::write(&main, lines.join("\n") + "\n").unwrap();

    let sub_dir = project.join(session).join("subagents");
    std::fs::create_dir_all(&sub_dir).unwrap();
    let sub = sub_dir.join("agent-abc.jsonl");
    let sub_lines = vec![
        claude_line(
            session,
            "s1",
            "2026-10-25T13:25:30Z",
            "user",
            json!("PrependPath=1 sidechain worker echo"),
            true,
        ),
    ];
    std::fs::write(&sub, sub_lines.join("\n") + "\n").unwrap();
    ClaudeFixture { root, main, sub }
}

fn build_codex(env: &support::TestEnv) -> PathBuf {
    let home = env.root("codex");
    let dir = home.join("sessions").join("2026").join("03").join("06");
    std::fs::create_dir_all(&dir).unwrap();
    let session = "22222222-2222-4222-8222-222222222222";
    let path = dir.join(format!("rollout-2026-03-06T21-50-12-{session}.jsonl"));
    let lines = vec![
        json!({
            "timestamp": "2026-03-06T21:50:12Z",
            "type": "session_meta",
            "payload": {"id": session, "cwd": "C:/work", "source": "cli"}
        })
        .to_string(),
        json!({
            "timestamp": "2026-03-06T21:50:13Z",
            "type": "response_item",
            "payload": {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "PrependPath=1 codex interactive"}]}
        })
        .to_string(),
        json!({
            "timestamp": "2026-03-06T21:50:14Z",
            "type": "response_item",
            "payload": {"type": "function_call", "name": "shell", "arguments": "{\"command\":\"python -V\"}", "call_id": "c1"}
        })
        .to_string(),
        json!({
            "timestamp": "2026-03-06T21:50:15Z",
            "type": "response_item",
            "payload": {"type": "function_call_output", "call_id": "c1", "output": "Python 3.13.17 codex output"}
        })
        .to_string(),
    ];
    std::fs::write(&path, lines.join("\n") + "\n").unwrap();

    // A worker rollout: `session_meta.payload.source = "exec"`.
    let worker = dir.join("rollout-2026-03-06T21-51-00-33333333-3333-4333-8333-333333333333.jsonl");
    let worker_lines = vec![
        json!({
            "timestamp": "2026-03-06T21:51:00Z",
            "type": "session_meta",
            "payload": {"id": "33333333-3333-4333-8333-333333333333", "source": "exec"}
        })
        .to_string(),
        json!({
            "timestamp": "2026-03-06T21:51:01Z",
            "type": "response_item",
            "payload": {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "PrependPath=1 codex worker"}]}
        })
        .to_string(),
    ];
    std::fs::write(&worker, worker_lines.join("\n") + "\n").unwrap();
    home
}

fn build_device(env: &support::TestEnv) -> PathBuf {
    let root = env.root("device");
    std::fs::write(
        root.join("2026-10-25_event.md"),
        "标题行\nPrependPath=1 装回 Python 3.13.17\n",
    )
    .unwrap();
    std::fs::write(root.join("2026-10-26_other.md"), "无关内容 marker\n").unwrap();
    root
}

fn all_fixture(env: &support::TestEnv, cache: Option<&Path>) -> (PathBuf, OpenCodeFixture, ClaudeFixture, PathBuf, PathBuf) {
    let opencode = build_opencode(env);
    let claude = build_claude(env);
    let codex = build_codex(env);
    let device = build_device(env);
    let config = env.write_json(
        "all-config.json",
        &v2_config(
            vec![
                source("oc", "opencode", &opencode.root),
                source("cx", "codex", &codex),
                source("cc", "claude_code", &claude.root),
                source("dev", "device", &device),
            ],
            vec!["oc", "cx", "cc", "dev"],
            cache,
        ),
    );
    (config, opencode, claude, codex, device)
}

// ---------------------------------------------------------------------------
// Build + exact search
// ---------------------------------------------------------------------------

#[test]
fn index_reports_every_enabled_source_and_builds_rows() {
    let env = support::TestEnv::new();
    let cache = cache_of(&env);
    let (config_path, _oc, _cc, _cx, _dev) = all_fixture(&env, Some(&cache));
    let config = load_config(&config_path).unwrap();
    let reports = index_all(&config, None, false).unwrap();
    assert_eq!(reports.len(), 4);
    for report in &reports {
        assert_eq!(report.status, "built", "{report:?}");
        assert!(report.rows > 0, "{report:?}");
        assert!(report.path.as_deref().unwrap().contains("live"));
    }
    // The index lives exactly where the plan says.
    assert!(index_path(&cache, "oc").is_file());
    assert!(index_path(&cache, "cc").is_file());

    // Re-running is idempotent.
    let again = index_all(&config, None, false).unwrap();
    for report in &again {
        assert_eq!(report.status, "unchanged", "{report:?}");
    }
}

#[test]
fn exact_mode_matches_identifiers_and_reports_hit_metadata() {
    let env = support::TestEnv::new();
    let cache = cache_of(&env);
    let (config_path, _oc, _cc, _cx, _dev) = all_fixture(&env, Some(&cache));
    let config = load_config(&config_path).unwrap();
    index_all(&config, None, false).unwrap();

    let response = search_request(
        &config_path,
        json!({
            "query": "PrependPath=1", "mode": "exact", "sources": ["oc"],
            "session_kinds": ["interactive"], "limit": 20
        }),
    );
    assert_eq!(status_of(&response, "oc"), "ok", "{response}");
    let results = response["results"].as_array().unwrap();
    assert!(!results.is_empty());
    let first = &results[0];
    assert_eq!(first["source_kind"], json!("opencode"));
    assert_eq!(first["role"], json!("user"));
    assert_eq!(first["kind"], json!("text"));
    assert_eq!(first["session_kind"], json!("interactive"));
    assert!(first["timestamp"].as_str().unwrap().starts_with("2023-11-14"));
    assert!(first["snippet"].as_str().unwrap().contains("PrependPath=1"));
    assert_eq!(first["ref"]["schema_gen"], json!("sqlite-v2"));
    assert_eq!(response["source_status"][0]["detail"]["exact"]["mode"], json!("index"));
}

#[test]
fn exact_terms_are_anded_and_phrases_stay_contiguous() {
    let env = support::TestEnv::new();
    let cache = cache_of(&env);
    let (config_path, _oc, _cc, _cx, _dev) = all_fixture(&env, Some(&cache));
    let config = load_config(&config_path).unwrap();
    index_all(&config, None, false).unwrap();

    // Both terms occur in sm-1 and sm-2.
    let both = search_request(&config_path, exact_request("PrependPath Python", vec!["oc"]));
    assert!(both["results"] == json!([]) || !both["results"].as_array().unwrap().is_empty());
    // A phrase that never appears contiguously returns nothing.
    let phrase = search_request(&config_path, exact_request("\"PrependPath Python313\"", vec!["oc"]));
    assert!(phrase["results"].as_array().unwrap().is_empty(), "{phrase}");

    // AND: "PrependPath" and "interpreter.kb" never occur in the same unit.
    let impossible = search_request(
        &config_path,
        exact_request("PrependPath interpreter.kb", vec!["oc"]),
    );
    assert!(impossible["results"].as_array().unwrap().is_empty(), "{impossible}");
    // ...while each alone does.
    let alone = search_request(&config_path, exact_request("interpreter.kb", vec!["oc"]));
    assert!(!alone["results"].as_array().unwrap().is_empty(), "{alone}");
}

#[test]
fn tool_input_and_tool_output_are_separate_channels_and_output_is_truncated() {
    let env = support::TestEnv::new();
    let cache = cache_of(&env);
    let (config_path, _oc, _cc, _cx, _dev) = all_fixture(&env, Some(&cache));
    let config = load_config(&config_path).unwrap();
    index_all(&config, None, false).unwrap();

    let input = search_request(&config_path, exact_request("python -V", vec!["oc"]));
    let kinds: Vec<&str> = input["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|hit| hit["kind"].as_str().unwrap())
        .collect();
    assert!(kinds.contains(&"tool_input"), "{input}");

    // The 20 000-char ceiling: the head is indexed, the tail is not.
    let head = search_request(&config_path, exact_request("longoutput", vec!["oc"]));
    assert!(!head["results"].as_array().unwrap().is_empty(), "{head}");
    let tail = search_request(&config_path, exact_request("TAILMARK", vec!["oc"]));
    assert!(tail["results"].as_array().unwrap().is_empty(), "{tail}");
}

// ---------------------------------------------------------------------------
// Incremental OpenCode
// ---------------------------------------------------------------------------

#[test]
fn opencode_incremental_add_update_and_delete() {
    let env = support::TestEnv::new();
    let cache = cache_of(&env);
    let opencode = build_opencode(&env);
    let config = env.write_json(
        "oc-config.json",
        &v2_config(
            vec![source("oc", "opencode", &opencode.root)],
            vec!["oc"],
            Some(&cache),
        ),
    );
    let parsed = load_config(&config).unwrap();
    let first = index_all(&parsed, None, false).unwrap();
    assert_eq!(first[0].status, "built");
    let rows_after_build = first[0].rows;

    // A new message is appended with a higher watermark.
    insert_message(
        &opencode.db,
        "sm-new",
        "s-main",
        9,
        1_700_000_100_000,
        "新增标记增量词",
    );
    let second = index_all(&parsed, None, false).unwrap();
    assert_eq!(second[0].status, "updated", "{:?}", second[0]);
    assert!(second[0].rows > rows_after_build);
    let found = search_request(&config, exact_request("增量词", vec!["oc"]));
    assert!(!found["results"].as_array().unwrap().is_empty(), "{found}");

    // An existing message edited in place replaces its rows instead of
    // duplicating them. The row keeps its rowid, so the rolling per-session
    // reconcile is what sees the edit: walk the rotation (four sessions, one per
    // round) until it has been applied.
    {
        let conn = rusqlite::Connection::open(&opencode.db).unwrap();
        conn.execute(
            "UPDATE session_message SET data = ?, time_updated = ? WHERE id = 'sm-new'",
            rusqlite::params![
                serde_json::to_string(&json!({"text": "改写后的标记"})).unwrap(),
                1_700_000_200_000i64
            ],
        )
        .unwrap();
    }
    let mut third = Vec::new();
    let mut opcode_mode = None;
    for _ in 0..4 {
        third = index_all(&parsed, None, false).unwrap();
        opcode_mode = third[0]
            .detail
            .as_ref()
            .and_then(|detail| detail["opencode"]["mode"].as_str())
            .map(str::to_string);
        if third[0].added > 0 {
            break;
        }
    }
    assert_eq!(third[0].status, "updated", "{:?}", third[0]);
    assert_eq!(
        opcode_mode.as_deref(),
        Some("incremental"),
        "the edit is applied by a rolling round: {third:?}"
    );
    assert_eq!(third[0].rows, second[0].rows, "an edit must not add rows");
    let old = search_request(&config, exact_request("增量词", vec!["oc"]));
    assert!(old["results"].as_array().unwrap().is_empty(), "{old}");
    let new = search_request(&config, exact_request("改写后的标记", vec!["oc"]));
    assert!(!new["results"].as_array().unwrap().is_empty(), "{new}");

    // Deleting a row that is not the last one leaves the tail empty: the rolling
    // slice removes it, and the round is not a rebuild.
    {
        let conn = rusqlite::Connection::open(&opencode.db).unwrap();
        conn.execute("DELETE FROM session_message WHERE id = 'sm-3'", [])
            .unwrap();
    }
    let mut fourth = Vec::new();
    for _ in 0..4 {
        fourth = index_all(&parsed, None, false).unwrap();
        if fourth[0].removed > 0 {
            break;
        }
    }
    assert!(
        !fourth[0].rebuilt,
        "a middle-row delete is caught by the slice, not a rebuild: {:?}",
        fourth[0]
    );
    assert!(fourth[0].removed > 0, "{:?}", fourth[0]);
    let gone = search_request(&config, exact_request("longoutput", vec!["oc"]));
    assert!(gone["results"].as_array().unwrap().is_empty(), "{gone}");

    // The watermark is stored and describes the tail, not a row count.
    let detail = fourth[0].detail.as_ref().unwrap();
    assert_eq!(detail["ts_column"], json!("time_updated"));
    assert!(
        detail["max_rowid"].as_i64().unwrap_or(0) > 0,
        "{detail}"
    );
    assert_eq!(detail["opencode"]["mode"], json!("incremental"), "{detail}");
    assert!(fourth[0].rows > 0);
}

#[test]
fn opencode_schema_change_triggers_a_full_rebuild() {
    let env = support::TestEnv::new();
    let cache = cache_of(&env);
    let opencode = build_opencode(&env);
    // Build with the `time_created` fallback (no `time_updated` column).
    {
        let conn = rusqlite::Connection::open(&opencode.db).unwrap();
        conn.execute_batch(
            "ALTER TABLE session_message DROP COLUMN time_updated;",
        )
        .unwrap();
    }
    let config = env.write_json(
        "oc-schema.json",
        &v2_config(
            vec![source("oc", "opencode", &opencode.root)],
            vec!["oc"],
            Some(&cache),
        ),
    );
    let parsed = load_config(&config).unwrap();
    let first = index_all(&parsed, None, false).unwrap();
    assert_eq!(first[0].detail.as_ref().unwrap()["ts_column"], json!("time_created"));

    // Re-adding the column changes the detected schema: rebuild.
    {
        let conn = rusqlite::Connection::open(&opencode.db).unwrap();
        conn.execute_batch("ALTER TABLE session_message ADD COLUMN time_updated INTEGER;")
            .unwrap();
    }
    let second = index_all(&parsed, None, false).unwrap();
    assert!(second[0].rebuilt, "{:?}", second[0]);
    assert_eq!(second[0].detail.as_ref().unwrap()["ts_column"], json!("time_updated"));
}

// ---------------------------------------------------------------------------
// Incremental JSONL / device files
// ---------------------------------------------------------------------------

#[test]
fn jsonl_append_change_shrink_and_delete() {
    let env = support::TestEnv::new();
    let cache = cache_of(&env);
    let claude = build_claude(&env);
    let config = env.write_json(
        "cc-config.json",
        &v2_config(
            vec![source("cc", "claude_code", &claude.root)],
            vec!["cc"],
            Some(&cache),
        ),
    );
    let parsed = load_config(&config).unwrap();
    let first = index_all(&parsed, None, false).unwrap();
    assert_eq!(first[0].status, "built");

    // Append one record.
    {
        let mut text = std::fs::read_to_string(&claude.main).unwrap();
        text.push_str(
            &claude_line(
                "11111111-1111-4111-8111-111111111111",
                "u3",
                "2026-10-25T13:29:00Z",
                "user",
                json!("appended record marker"),
                false,
            ),
        );
        text.push('\n');
        std::fs::write(&claude.main, text).unwrap();
    }
    let second = index_all(&parsed, None, false).unwrap();
    assert_eq!(second[0].status, "updated", "{:?}", second[0]);
    let appended = search_request(&config, exact_request("appended record marker", vec!["cc"]));
    assert!(!appended["results"].as_array().unwrap().is_empty(), "{appended}");

    // Rewrite the file with the same size but different content: rebuild the
    // file (mtime changed, size equal).
    std::thread::sleep(std::time::Duration::from_millis(20));
    {
        let text = std::fs::read_to_string(&claude.main).unwrap();
        let replaced = text.replace("appended record marker", "rewritten record marke");
        assert_eq!(replaced.len(), text.len());
        std::fs::write(&claude.main, replaced).unwrap();
    }
    let third = index_all(&parsed, None, false).unwrap();
    assert_eq!(third[0].status, "updated", "{:?}", third[0]);
    let old = search_request(&config, exact_request("appended record marker", vec!["cc"]));
    assert!(old["results"].as_array().unwrap().is_empty(), "{old}");
    let new = search_request(&config, exact_request("rewritten record marke", vec!["cc"]));
    assert!(!new["results"].as_array().unwrap().is_empty(), "{new}");

    // Shrink the file: the extra record disappears.
    {
        let text = std::fs::read_to_string(&claude.main).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        std::fs::write(&claude.main, lines[..2].join("\n") + "\n").unwrap();
    }
    let fourth = index_all(&parsed, None, false).unwrap();
    assert_eq!(fourth[0].status, "updated", "{:?}", fourth[0]);
    let gone = search_request(&config, exact_request("rewritten record marke", vec!["cc"]));
    assert!(gone["results"].as_array().unwrap().is_empty(), "{gone}");
    let kept = search_request(&config, exact_request("PrependPath=1", vec!["cc"]));
    assert!(!kept["results"].as_array().unwrap().is_empty(), "{kept}");

    // Deleting the worker transcript removes its rows.
    std::fs::remove_file(&claude.sub).unwrap();
    let fifth = index_all(&parsed, None, false).unwrap();
    assert_eq!(fifth[0].status, "updated", "{:?}", fifth[0]);
    let worker = search_request(&config, exact_request("sidechain worker echo", vec!["cc"]));
    assert!(worker["results"].as_array().unwrap().is_empty(), "{worker}");
}

#[test]
fn device_file_change_and_delete_update_the_index() {
    let env = support::TestEnv::new();
    let cache = cache_of(&env);
    let device = build_device(&env);
    let config = env.write_json(
        "dev-config.json",
        &v2_config(
            vec![source("dev", "device", &device)],
            vec!["dev"],
            Some(&cache),
        ),
    );
    let parsed = load_config(&config).unwrap();
    let first = index_all(&parsed, None, false).unwrap();
    assert_eq!(first[0].rows, 2, "{:?}", first[0]);

    std::thread::sleep(std::time::Duration::from_millis(20));
    std::fs::write(
        device.join("2026-10-26_other.md"),
        "修改后的内容 changedmark\n",
    )
    .unwrap();
    let second = index_all(&parsed, None, false).unwrap();
    assert_eq!(second[0].status, "updated", "{:?}", second[0]);
    let changed = search_request(&config, exact_request("changedmark", vec!["dev"]));
    assert!(!changed["results"].as_array().unwrap().is_empty(), "{changed}");
    assert_eq!(changed["results"][0]["session_kind"], json!("unknown"));
    assert_eq!(changed["results"][0]["ref"]["schema_gen"], json!("md-v1"));

    std::fs::remove_file(device.join("2026-10-25_event.md")).unwrap();
    let third = index_all(&parsed, None, false).unwrap();
    assert_eq!(third[0].rows, 1, "{:?}", third[0]);
    let gone = search_request(&config, exact_request("PrependPath", vec!["dev"]));
    assert!(gone["results"].as_array().unwrap().is_empty(), "{gone}");
}

#[test]
fn deleting_the_index_and_rebuilding_produces_identical_hits() {
    let env = support::TestEnv::new();
    let cache = cache_of(&env);
    let (config_path, _oc, _cc, _cx, _dev) = all_fixture(&env, Some(&cache));
    let parsed = load_config(&config_path).unwrap();
    index_all(&parsed, None, false).unwrap();

    let query = || exact_request("PrependPath=1", vec!["oc", "cx", "cc", "dev"]);
    let before = search_request(&config_path, query());
    let before_ids = hit_ids(&before);

    for id in ["oc", "cx", "cc", "dev"] {
        std::fs::remove_file(index_path(&cache, id)).unwrap();
    }
    let rebuilt = index_all(&parsed, None, false).unwrap();
    for report in &rebuilt {
        assert_eq!(report.status, "built");
    }
    let after = search_request(&config_path, query());
    assert_eq!(before_ids, hit_ids(&after));
    assert_eq!(
        serde_json::to_string(&before["results"]).unwrap(),
        serde_json::to_string(&after["results"]).unwrap(),
        "a rebuild must reproduce the same hits byte for byte"
    );
}

// ---------------------------------------------------------------------------
// Short terms, filters, order, grouping
// ---------------------------------------------------------------------------

#[test]
fn short_terms_fall_back_and_long_terms_are_verified_on_candidates() {
    let env = support::TestEnv::new();
    let cache = cache_of(&env);
    let (config_path, _oc, _cc, _cx, _dev) = all_fixture(&env, Some(&cache));
    let parsed = load_config(&config_path).unwrap();
    index_all(&parsed, None, false).unwrap();

    // Every term is shorter than the trigram minimum: the scan answers and the
    // degradation is named.
    let short = search_request(
        &config_path,
        json!({"query": "Py 31", "mode": "exact", "sources": ["oc"]}),
    );
    assert_eq!(status_of(&short, "oc"), "ok", "{short}");
    assert_eq!(detail_of(&short, "oc")["reason"], json!("short_terms_fallback"));
    assert_eq!(detail_of(&short, "oc")["exact"]["mode"], json!("scan_fallback"));
    assert!(!short["results"].as_array().unwrap().is_empty(), "{short}");

    // A mix: the long term narrows with MATCH, the short term is verified as a
    // substring on the candidates.
    let mixed = search_request(
        &config_path,
        json!({"query": "PrependPath Py", "mode": "exact", "sources": ["oc"]}),
    );
    assert_eq!(status_of(&mixed, "oc"), "ok", "{mixed}");
    assert!(!mixed["results"].as_array().unwrap().is_empty(), "{mixed}");
    for hit in mixed["results"].as_array().unwrap() {
        let snippet = hit["snippet"].as_str().unwrap();
        assert!(snippet.contains("PrependPath"), "{snippet}");
    }
    let absent = search_request(
        &config_path,
        json!({"query": "PrependPath zq", "mode": "exact", "sources": ["oc"]}),
    );
    assert!(absent["results"].as_array().unwrap().is_empty(), "{absent}");
}

#[test]
fn exact_filters_cover_time_role_kind_session_kind_and_session_id() {
    let env = support::TestEnv::new();
    let cache = cache_of(&env);
    let (config_path, _oc, _cc, _cx, _dev) = all_fixture(&env, Some(&cache));
    let parsed = load_config(&config_path).unwrap();
    index_all(&parsed, None, false).unwrap();

    let base = json!({"query": "PrependPath=1", "mode": "exact", "sources": ["oc"], "limit": 20});
    let all = search_request(&config_path, base.clone());
    assert!(all["results"].as_array().unwrap().len() >= 3, "{all}");

    let mut filtered = base.clone();
    filtered["session_kinds"] = json!(["interactive"]);
    let interactive = search_request(&config_path, filtered);
    let kinds: Vec<&str> = interactive["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|hit| hit["session_kind"].as_str().unwrap())
        .collect();
    assert!(kinds.iter().all(|kind| *kind == "interactive"), "{interactive}");
    assert_eq!(interactive["results"].as_array().unwrap().len(), 1);

    let mut worker = base.clone();
    worker["session_kinds"] = json!(["worker"]);
    let workers = search_request(&config_path, worker);
    let count = workers["results"].as_array().unwrap().len();
    assert_eq!(
        count, 3,
        "parent_id plus ocsr and subagent titles are workers: {workers}"
    );

    let mut roles = base.clone();
    roles["roles"] = json!(["tool"]);
    let tools = search_request(&config_path, roles);
    assert!(tools["results"].as_array().unwrap().is_empty(), "{tools}");

    let mut kinds_filter = base.clone();
    kinds_filter["kinds"] = json!(["text"]);
    let texts = search_request(&config_path, kinds_filter);
    assert!(texts["results"]
        .as_array()
        .unwrap()
        .iter()
        .all(|hit| hit["kind"] == json!("text")));

    let mut since = base.clone();
    since["since"] = json!("2023-11-14T22:13:23Z");
    let after = search_request(&config_path, since);
    assert!(after["results"].as_array().unwrap().len() < all["results"].as_array().unwrap().len());

    let mut until = base.clone();
    until["until"] = json!("2023-11-14T22:13:20Z");
    let before = search_request(&config_path, until);
    assert!(before["results"].as_array().unwrap().len() <= 2, "{before}");

    let mut session = base.clone();
    session["session_id"] = json!("s-sub");
    let one = search_request(&config_path, session);
    assert_eq!(one["results"].as_array().unwrap().len(), 1, "{one}");
    assert_eq!(one["results"][0]["ref"]["locator"]["session_id"], json!("s-sub"));

    // L5b rework 2: the default mode accepts filters (it resolves per source),
    // so the rejection is only about the explicit scan mode.
    let parsed = load_config(&config_path).unwrap();
    assert!(
        SearchRequest::parse(&json!({"query": "x", "since": "2026-01-01T00:00:00Z"})).is_ok(),
        "the default mode must accept a time filter"
    );
    let error = SearchRequest::parse(&json!({
        "query": "x", "mode": "legacy", "since": "2026-01-01T00:00:00Z"
    }))
    .unwrap_err();
    let _ = parsed;
    assert_eq!(error.code, "INVALID_REQUEST");
    assert_eq!(error.detail.unwrap()["reason"], "exact_only");
}

#[test]
fn orders_are_newest_oldest_and_bm25_relevance() {
    let env = support::TestEnv::new();
    let cache = cache_of(&env);
    let (config_path, _oc, _cc, _cx, _dev) = all_fixture(&env, Some(&cache));
    let parsed = load_config(&config_path).unwrap();
    index_all(&parsed, None, false).unwrap();

    let newest = search_request(
        &config_path,
        json!({"query": "PrependPath=1", "mode": "exact", "sources": ["oc"], "order": "newest"}),
    );
    let oldest = search_request(
        &config_path,
        json!({"query": "PrependPath=1", "mode": "exact", "sources": ["oc"], "order": "oldest"}),
    );
    let newest_ids = hit_ids(&newest);
    let mut oldest_ids = hit_ids(&oldest);
    oldest_ids.reverse();
    assert_eq!(newest_ids, oldest_ids, "the same set in the opposite order");
    assert_eq!(
        newest["results"][0]["timestamp"], json!("2023-11-14T22:13:25Z"),
        "newest first: {newest}"
    );

    let relevance = search_request(
        &config_path,
        json!({"query": "PrependPath=1", "mode": "exact", "sources": ["oc"], "order": "relevance"}),
    );
    assert_eq!(relevance["results"][0]["score_kind"], json!("bm25"));
    assert!(relevance["results"][0]["score"].as_f64().is_some());

    // limit/offset keep working on the exact path: a page-2 request returns the
    // next hit and only the last page has no `next_offset`.
    let page = search_request(
        &config_path,
        json!({"query": "PrependPath=1", "mode": "exact", "sources": ["oc"], "limit": 1}),
    );
    assert_eq!(page["results"].as_array().unwrap().len(), 1);
    assert_eq!(page["next_offset"], json!(1), "{page}");
    let second = search_request(
        &config_path,
        json!({"query": "PrependPath=1", "mode": "exact", "sources": ["oc"], "limit": 1, "offset": 1}),
    );
    assert_ne!(hit_ids(&page), hit_ids(&second), "{second}");
    let last = search_request(
        &config_path,
        json!({"query": "PrependPath=1", "mode": "exact", "sources": ["oc"], "limit": 20, "offset": 3}),
    );
    assert!(last["next_offset"].is_null(), "{last}");
}

#[test]
fn group_by_session_keeps_one_hit_per_session_with_counts() {
    let env = support::TestEnv::new();
    let cache = cache_of(&env);
    let (config_path, _oc, _cc, _cx, _dev) = all_fixture(&env, Some(&cache));
    let parsed = load_config(&config_path).unwrap();
    index_all(&parsed, None, false).unwrap();

    let grouped = search_request(
        &config_path,
        json!({
            "query": "Python", "mode": "exact", "sources": ["oc"],
            "group_by_session": true, "limit": 20
        }),
    );
    let sessions: Vec<&str> = grouped["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|hit| hit["ref"]["locator"]["session_id"].as_str().unwrap())
        .collect();
    let mut unique = sessions.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(sessions.len(), unique.len(), "one hit per session: {grouped}");

    let counts: Vec<u64> = grouped["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|hit| hit["session_hits"].as_u64().unwrap())
        .collect();
    assert!(counts.iter().all(|count| *count >= 1), "{grouped}");
    assert!(counts.iter().any(|count| *count > 1), "a session repeats: {grouped}");
}

#[test]
fn session_kind_is_derived_for_all_three_sources() {
    let env = support::TestEnv::new();
    let cache = cache_of(&env);
    let (config_path, _oc, _cc, _cx, _dev) = all_fixture(&env, Some(&cache));
    let parsed = load_config(&config_path).unwrap();
    index_all(&parsed, None, false).unwrap();

    let response = search_request(&config_path, exact_request("PrependPath=1", vec!["cc", "cx", "oc"]));
    let results = response["results"].as_array().unwrap();
    let of = |kind: &str| -> Vec<(String, String)> {
        results
            .iter()
            .filter(|hit| hit["source_kind"] == json!(kind))
            .map(|hit| {
                (
                    hit["session_kind"].as_str().unwrap().to_string(),
                    hit["ref"]["rel_path"].as_str().unwrap().to_string(),
                )
            })
            .collect()
    };

    let claude = of("claude_code");
    assert!(claude.iter().any(|(kind, path)| kind == "interactive" && path.ends_with(".jsonl") && !path.contains("subagents")));
    assert!(claude.iter().any(|(kind, path)| kind == "worker" && path.contains("subagents")));

    let codex = of("codex");
    assert!(codex.iter().any(|(kind, _)| kind == "interactive"), "{codex:?}");
    assert!(codex.iter().any(|(kind, _)| kind == "worker"), "{codex:?}");

    let opencode = of("opencode");
    assert!(opencode.iter().any(|(kind, _)| kind == "interactive"));
    assert_eq!(
        opencode.iter().filter(|(kind, _)| kind == "worker").count(),
        3
    );
}

// ---------------------------------------------------------------------------
// Explicit degradation
// ---------------------------------------------------------------------------

#[test]
fn missing_stale_and_corrupt_indexes_degrade_explicitly() {
    let env = support::TestEnv::new();
    let cache = cache_of(&env);
    let (config_path, oc, _cc, _cx, _dev) = all_fixture(&env, Some(&cache));
    let parsed = load_config(&config_path).unwrap();

    // Missing: exact mode still answers through the scan, marked degraded.
    let missing = search_request(&config_path, exact_request("PrependPath=1", vec!["oc"]));
    assert_eq!(status_of(&missing, "oc"), "degraded", "{missing}");
    assert_eq!(detail_of(&missing, "oc")["reason"], json!("INDEX_MISSING"));
    assert_eq!(detail_of(&missing, "oc")["exact"]["mode"], json!("scan_fallback"));
    assert!(!missing["results"].as_array().unwrap().is_empty());
    // ...and the exit code is non-zero because a source is degraded.
    let (code, _) = search(&config_path, exact_request("PrependPath=1", vec!["oc"]));
    assert_eq!(code, 10);

    // Build, then repoint the source root: the stored identity no longer matches.
    index_all(&parsed, None, false).unwrap();
    let moved = env.root("oc-moved");
    std::fs::copy(oc.db, moved.join("opencode.db")).unwrap();
    let stale_config = env.write_json(
        "oc-stale.json",
        &v2_config(
            vec![source("oc", "opencode", &moved)],
            vec!["oc"],
            Some(&cache),
        ),
    );
    let stale = search_request(&stale_config, exact_request("PrependPath=1", vec!["oc"]));
    assert_eq!(status_of(&stale, "oc"), "degraded", "{stale}");
    assert_eq!(detail_of(&stale, "oc")["reason"], json!("INDEX_STALE"));
    assert_eq!(
        detail_of(&stale, "oc")["exact"]["cause"],
        json!("source_identity_mismatch")
    );

    // Corrupt: a file that is not an index.
    std::fs::write(index_path(&cache, "oc"), b"not a sqlite database").unwrap();
    let corrupt = search_request(&config_path, exact_request("PrependPath=1", vec!["oc"]));
    assert_eq!(status_of(&corrupt, "oc"), "degraded", "{corrupt}");
    assert_eq!(detail_of(&corrupt, "oc")["reason"], json!("INDEX_CORRUPT"));
}

#[test]
fn exact_hits_read_back_through_memory_read() {
    let env = support::TestEnv::new();
    let cache = cache_of(&env);
    let (config_path, _oc, _cc, _cx, _dev) = all_fixture(&env, Some(&cache));
    let parsed = load_config(&config_path).unwrap();
    index_all(&parsed, None, false).unwrap();

    for source_id in ["oc", "cx", "cc", "dev"] {
        let response = search_request(&config_path, exact_request("PrependPath=1", vec![source_id]));
        let hit = response["results"]
            .as_array()
            .unwrap()
            .first()
            .unwrap_or_else(|| panic!("no hit for {source_id}: {response}"));
        let reference = hit["ref"].clone();
        let read_request = json!({"ref": reference});
        let request = agent_memory::recall::read::ReadRequest::parse(&read_request)
            .unwrap_or_else(|error| panic!("{source_id}: ref does not parse: {error:?}"));
        let read = agent_memory::recall::read::memory_read(&parsed, &request);
        assert!(read.ok, "{source_id}: read failed: {:?}", serde_json::to_value(&read).unwrap());
        let text = serde_json::to_value(&read).unwrap();
        assert!(
            text["text"]
                .as_str()
                .or_else(|| text["segments"][0]["text"].as_str())
                .unwrap()
                .contains("PrependPath"),
            "{source_id}: {text}"
        );
    }
}

/// L5b changes the *default* path only when a readable index exists: without a
/// cache the response is byte-identical to the pre-L3 scan, and `mode="legacy"`
/// keeps that response available even after an index is built. This replaces the
/// L2/L3 assertion that "building an index must never change the default
/// response", which the L5b specification intentionally supersedes.
#[test]
fn auto_mode_uses_hybrid_only_after_an_index_exists_and_rejects_exact_keys() {
    let env = support::TestEnv::new();
    let cache = cache_of(&env);
    let (config_path, _oc, _cc, _cx, _dev) = all_fixture(&env, Some(&cache));
    let parsed = load_config(&config_path).unwrap();

    let request = json!({"query": "PrependPath", "sources": ["oc", "cc"], "limit": 5});
    let before = serialized_search(&parsed, &request);
    // No index yet: auto is the legacy scan, and an explicit `legacy` matches it
    // byte for byte.
    let explicit_legacy = serialized_search(&parsed, &json!({
        "query": "PrependPath", "sources": ["oc", "cc"], "limit": 5, "mode": "legacy"
    }));
    assert_eq!(before, explicit_legacy);
    for forbidden in ["\"exact\"", "\"hybrid\"", "\"session_kind\"", "\"kind\"", "\"timestamp\"", "\"channels\""] {
        assert!(!before.contains(forbidden), "{forbidden} leaked: {before}");
    }

    index_all(&parsed, None, false).unwrap();
    let after = serialized_search(&parsed, &request);
    assert_ne!(
        before, after,
        "with a readable index the default path is hybrid"
    );
    assert!(after.contains("\"hybrid\""), "missing hybrid detail: {after}");
    assert!(after.contains("\"channels\""), "missing channels: {after}");
    // The explicit legacy path is still exactly the pre-index response.
    let legacy_after = serialized_search(&parsed, &json!({
        "query": "PrependPath", "sources": ["oc", "cc"], "limit": 5, "mode": "legacy"
    }));
    assert_eq!(before, legacy_after);
    // `auto` and an omitted mode are the same request.
    let explicit_auto = serialized_search(&parsed, &json!({
        "query": "PrependPath", "sources": ["oc", "cc"], "limit": 5, "mode": "auto"
    }));
    assert_eq!(after, explicit_auto);

    // L5b rework 2: the default mode accepts filters and ordering hints (they
    // resolve per source); only an explicit `legacy` rejects them.
    let error = SearchRequest::parse(&json!({
        "query": "x", "mode": "legacy", "roles": ["user"]
    }))
    .unwrap_err();
    assert_eq!(error.code, "INVALID_REQUEST");
    assert_eq!(error.detail.unwrap()["path"], "roles");
    let error = SearchRequest::parse(&json!({"query": "x", "mode": "legacy", "order": "newest"}))
        .unwrap_err();
    assert_eq!(error.detail.unwrap()["path"], "order");
    let error = SearchRequest::parse(&json!({
        "query": "x", "mode": "legacy", "group_by_session": true
    }))
    .unwrap_err();
    assert_eq!(error.detail.unwrap()["path"], "group_by_session");
    // Unknown values are rejected inside exact mode too.
    let error = SearchRequest::parse(&json!({"query": "x", "mode": "exact", "order": "sideways"}))
        .unwrap_err();
    assert_eq!(error.detail.unwrap()["reason"], "unsupported_order");
    let error = SearchRequest::parse(&json!({"query": "x", "mode": "exact", "since": "yesterday"}))
        .unwrap_err();
    assert_eq!(error.detail.unwrap()["reason"], "invalid_rfc3339");
}

fn serialized_search(config: &agent_memory::recall::RecallConfig, request: &Value) -> String {
    let request = SearchRequest::parse(request).expect("request parses");
    serde_json::to_string(&memory_search(config, &request)).unwrap()
}

// ---------------------------------------------------------------------------
// CLI + MCP surface
// ---------------------------------------------------------------------------

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_agent-memory")
}

#[test]
fn cli_index_builds_and_mcp_never_writes_the_cache() {
    let env = support::TestEnv::new();
    let cache = cache_of(&env);
    let (config_path, _oc, _cc, _cx, _dev) = all_fixture(&env, Some(&cache));

    let output = std::process::Command::new(bin())
        .arg("index")
        .arg("--config")
        .arg(&config_path)
        .output()
        .expect("run index");
    assert_eq!(output.status.code(), Some(0), "{}", String::from_utf8_lossy(&output.stdout));
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["ok"], json!(true));
    assert_eq!(report["sources"].as_array().unwrap().len(), 4);

    let before = support::tree_state(&cache);

    // A read-only MCP session must not create or touch anything.
    use std::io::{BufRead, BufReader, Write};
    let mut child = std::process::Command::new(bin())
        .arg("mcp")
        .arg("--config")
        .arg(&config_path)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("start mcp");
    {
        let stdin = child.stdin.as_mut().unwrap();
        for message in [
            json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}}),
            json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {
                "name": "memory_search",
                "arguments": {"query": "PrependPath=1", "mode": "exact", "sources": ["oc"]}
            }}),
        ] {
            writeln!(stdin, "{message}").unwrap();
            stdin.flush().unwrap();
        }
    }
    let mut reader = BufReader::new(child.stdout.as_mut().unwrap());
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    line.clear();
    reader.read_line(&mut line).unwrap();
    let response: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(
        response["result"]["structuredContent"]["results"]
            .as_array()
            .unwrap()
            .len() > 0,
        true,
        "{response}"
    );
    let _ = child.kill();
    let _ = child.wait();
    assert_eq!(before, support::tree_state(&cache), "MCP must not write the cache");
}

#[test]
fn index_without_cache_root_is_an_explicit_error() {
    let env = support::TestEnv::new();
    let opencode = build_opencode(&env);
    let config = env.write_json(
        "no-cache.json",
        &v2_config(vec![source("oc", "opencode", &opencode.root)], vec!["oc"], None),
    );
    let output = std::process::Command::new(bin())
        .arg("index")
        .arg("--config")
        .arg(&config)
        .output()
        .expect("run index");
    assert_eq!(output.status.code(), Some(2));
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["ok"], json!(false));
    assert_eq!(report["detail"]["path"], json!("cache_root"));

    // The library path fails the same way instead of guessing a home directory.
    let parsed = load_config(&config).unwrap();
    let error = index_all(&parsed, None, false).unwrap_err();
    assert_eq!(error.detail.unwrap()["path"], "cache_root");
}

#[test]
fn index_source_selection_and_kb_are_reported_verbatim() {
    let env = support::TestEnv::new();
    let cache = cache_of(&env);
    let opencode = build_opencode(&env);
    let vault = env.root("vault");
    let config = env.write_json(
        "select-config.json",
        &v2_config(
            vec![
                source("kb", "kb", &vault),
                source("oc", "opencode", &opencode.root),
            ],
            vec!["kb", "oc"],
            Some(&cache),
        ),
    );
    let parsed = load_config(&config).unwrap();
    let reports = index_all(&parsed, Some("oc"), false).unwrap();
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0].source_id, "oc");

    let all = index_all(&parsed, None, false).unwrap();
    let kb = all.iter().find(|report| report.source_id == "kb").unwrap();
    assert_eq!(kb.status, "skipped_not_indexable");

    let unknown = index_all(&parsed, Some("nope"), false).unwrap_err();
    assert_eq!(unknown.detail.unwrap()["reason"], "unknown_source");
}

/// N1: a file that grew while its timestamp moved backwards is re-read whole.
/// An append-offset reuse would keep the old prefix rows, which this case makes
/// observable: the prefix text changed.
#[test]
fn a_growing_file_with_an_older_timestamp_is_rescanned_whole() {
    use std::time::{Duration, SystemTime};
    let env = support::TestEnv::new();
    let cache = cache_of(&env);
    let root = env.root("claude-mtime");
    let project = root.join("proj");
    std::fs::create_dir_all(&project).unwrap();
    let session = "55555555-5555-4555-8555-555555555555";
    let path = project.join(format!("{session}.jsonl"));
    let first = claude_line(
        session,
        "u1",
        "2026-10-25T13:25:00Z",
        "user",
        json!("old prefix marker"),
        false,
    );
    std::fs::write(&path, format!("{first}\n")).unwrap();
    let config = env.write_json(
        "mtime-config.json",
        &v2_config(
            vec![source("cc", "claude_code", &root)],
            vec!["cc"],
            Some(&cache),
        ),
    );
    let parsed = load_config(&config).unwrap();
    let first_report = index_all(&parsed, None, false).unwrap();
    assert_eq!(first_report[0].rows, 1, "{:?}", first_report[0]);

    // Rewrite the whole file (bigger, different prefix) and move its timestamp
    // behind the recorded watermark.
    let replacement = [
        claude_line(
            session,
            "u2",
            "2026-10-25T13:26:00Z",
            "user",
            json!("new prefix marker"),
            false,
        ),
        claude_line(
            session,
            "a1",
            "2026-10-25T13:27:00Z",
            "assistant",
            json!("appended tail marker"),
            false,
        ),
    ]
    .join("\n");
    std::fs::write(&path, format!("{replacement}\n")).unwrap();
    let backdated = SystemTime::now() - Duration::from_secs(3_600);
    let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
    file.set_times(std::fs::FileTimes::new().set_modified(backdated))
        .unwrap();
    drop(file);

    let second = index_all(&parsed, None, false).unwrap();
    assert_eq!(second[0].status, "updated", "{:?}", second[0]);

    let gone = search_request(&config, exact_request("old prefix marker", vec!["cc"]));
    assert!(gone["results"].as_array().unwrap().is_empty(), "{gone}");
    let fresh = search_request(&config, exact_request("new prefix marker", vec!["cc"]));
    assert_eq!(fresh["results"].as_array().unwrap().len(), 1, "{fresh}");
    let tail = search_request(&config, exact_request("appended tail marker", vec!["cc"]));
    assert_eq!(tail["results"].as_array().unwrap().len(), 1, "{tail}");
}

/// N3: a record above the per-record ceiling is skipped **and counted**, both in
/// the `index` report and in the query detail of the source.
#[test]
fn oversize_lines_are_counted_in_the_index_report_and_query_detail() {
    let env = support::TestEnv::new();
    let cache = cache_of(&env);
    let root = env.root("claude-oversize");
    let project = root.join("proj");
    std::fs::create_dir_all(&project).unwrap();
    let session = "66666666-6666-4666-8666-666666666666";
    let path = project.join(format!("{session}.jsonl"));
    let mut body = String::new();
    body.push_str(&claude_line(
        session,
        "u1",
        "2026-10-25T13:25:00Z",
        "user",
        json!("oversize marker kept"),
        false,
    ));
    body.push('\n');
    body.push_str("{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"");
    body.push_str(&"x".repeat(64 * 1024 * 1024 + 1_024));
    body.push_str("\"}}\n");
    std::fs::write(&path, body).unwrap();
    let config = env.write_json(
        "oversize-config.json",
        &v2_config(
            vec![source("cc", "claude_code", &root)],
            vec!["cc"],
            Some(&cache),
        ),
    );
    let parsed = load_config(&config).unwrap();
    let reports = index_all(&parsed, None, false).unwrap();
    let report = &reports[0];
    assert_eq!(report.status, "built", "{report:?}");
    assert_eq!(report.rows, 1, "{report:?}");
    assert_eq!(
        report.detail.as_ref().unwrap()["skipped_oversize"],
        json!(1),
        "{report:?}"
    );

    let response = search_request(&config, exact_request("oversize marker kept", vec!["cc"]));
    assert_eq!(
        detail_of(&response, "cc")["exact"]["skipped_oversize"],
        json!(1),
        "{response}"
    );
}

/// N6 (a): `newest`/`oldest` order the merged page by time across sources, not
/// per source. The fixture holds `PrependPath=1` hits in four sources at four
/// different times, so a per-source round-robin would fail the monotonic check.
#[test]
fn exact_newest_and_oldest_order_across_sources() {
    let env = support::TestEnv::new();
    let cache = cache_of(&env);
    let (config_path, _oc, _cc, _cx, _dev) = all_fixture(&env, Some(&cache));
    let parsed = load_config(&config_path).unwrap();
    index_all(&parsed, None, false).unwrap();

    let request = |order: &str| {
        json!({
            "query": "PrependPath=1", "mode": "exact",
            "sources": ["oc", "cc", "cx", "dev"], "order": order, "limit": 20
        })
    };
    let newest = search_request(&config_path, request("newest"));
    let oldest = search_request(&config_path, request("oldest"));
    let stamps = |response: &Value| -> Vec<String> {
        response["results"]
            .as_array()
            .unwrap()
            .iter()
            .map(|hit| hit["timestamp"].as_str().unwrap_or_default().to_string())
            .collect()
    };
    let descending = stamps(&newest);
    let ascending = stamps(&oldest);
    assert!(descending.len() >= 4, "{newest}");
    assert!(
        descending.windows(2).all(|pair| pair[0] >= pair[1]),
        "newest must be globally non-increasing: {descending:?}"
    );
    assert!(
        ascending.windows(2).all(|pair| pair[0] <= pair[1]),
        "oldest must be globally non-decreasing: {ascending:?}"
    );
    let mut forward = hit_ids(&newest);
    let mut backward = hit_ids(&oldest);
    forward.sort();
    backward.sort();
    assert_eq!(forward, backward, "both orders see the same hits");

    let sources: std::collections::BTreeSet<String> = newest["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|hit| hit["source_id"].as_str().unwrap_or_default().to_string())
        .collect();
    assert!(
        sources.len() >= 3,
        "the ordering check needs several sources: {sources:?}"
    );
}

/// N6 (b): a tool output longer than the index ceiling is indexed truncated, but
/// its ref still spans the whole projection and reads back with the range
/// fingerprint intact.
#[test]
fn a_truncated_tool_output_reads_back_over_its_full_range() {
    let env = support::TestEnv::new();
    let cache = cache_of(&env);
    let (config_path, _oc, _cc, _cx, _dev) = all_fixture(&env, Some(&cache));
    let parsed = load_config(&config_path).unwrap();
    index_all(&parsed, None, false).unwrap();

    let response = search_request(&config_path, exact_request("longoutput", vec!["oc"]));
    let hit = response["results"]
        .as_array()
        .unwrap()
        .first()
        .unwrap_or_else(|| panic!("no truncated-output hit: {response}"));
    assert_eq!(hit["kind"], json!("tool_output"), "{hit}");
    let byte_end = hit["ref"]["text_range"]["byte_end"].as_u64().unwrap();
    assert!(
        byte_end > 20_000,
        "the ref must span the whole projection, not the indexed prefix: {hit}"
    );

    // The whole projection is readable when the budget allows it.
    let request = agent_memory::recall::read::ReadRequest::parse(&json!({
        "ref": hit["ref"].clone(), "max_bytes": 1_048_576
    }))
    .unwrap();
    let read = agent_memory::recall::read::memory_read(&parsed, &request);
    let value = serde_json::to_value(&read).unwrap();
    assert!(read.ok, "{value}");
    let text = value["text"].as_str().expect("full text");
    assert!(text.contains("longoutput"), "{}", &text[..40.min(text.len())]);
    assert!(text.contains("TAILMARK"), "the tail must be reachable");
    assert_eq!(
        value["range_sha256"],
        hit["ref"]["fingerprint"]["range_sha256"],
        "readback must verify the range the index recorded"
    );
    assert_eq!(value["truncated"], json!(false));

    // The default byte budget still truncates instead of failing.
    let request = agent_memory::recall::read::ReadRequest::parse(&json!({
        "ref": hit["ref"].clone()
    }))
    .unwrap();
    let small = agent_memory::recall::read::memory_read(&parsed, &request);
    assert!(small.ok);
    assert!(serde_json::to_value(&small).unwrap()["truncated"] == json!(true));
}

/// N6 (c) + 543dbcf: an OpenCode V2 tool part (`name` + `state.content` text
/// blocks) is projected into both channels, answered by the legacy scan and by
/// exact mode, and reads back.
#[test]
fn opencode_v2_tool_blocks_are_indexed_by_legacy_and_exact_and_read_back() {
    let env = support::TestEnv::new();
    let cache = cache_of(&env);
    let root = env.root("oc-v2");
    let db = root.join("opencode.db");
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute_batch(OPENCODE_SCHEMA).unwrap();
    conn.execute(
        "INSERT INTO session_v2(id, title, directory, time_created, time_updated, parent_id) \
         VALUES ('s-v2', 'v2 tools', 'C:/work', 1000, 9000, NULL)",
        [],
    )
    .unwrap();
    let messages: Vec<(&str, i64, &str, Value)> = vec![
        (
            "sm-v2-text",
            1_700_000_000_000,
            "user",
            json!({"text": "V2 工具块测试"}),
        ),
        (
            "sm-v2-tool",
            1_700_000_001_000,
            "assistant",
            json!({"content": [{
                "type": "tool",
                "name": "bash",
                "state": {
                    "input": {"command": "V2CMD=1 --flag"},
                    "content": [{"type": "text", "text": "V2OUT=2 stdout marker"}]
                }
            }]}),
        ),
    ];
    for (index, (id, ts, kind, data)) in messages.iter().enumerate() {
        conn.execute(
            "INSERT INTO session_message(id, session_id, seq, type, time_created, time_updated, data) \
             VALUES (?1, 's-v2', ?2, ?3, ?4, ?4, ?5)",
            rusqlite::params![
                id,
                index as i64 + 1,
                kind,
                ts,
                serde_json::to_string(data).unwrap()
            ],
        )
        .unwrap();
    }
    drop(conn);
    let config_path = env.write_json(
        "v2-config.json",
        &v2_config(
            vec![source("oc", "opencode", &root)],
            vec!["oc"],
            Some(&cache),
        ),
    );

    // Legacy lexical retrieval (explicit `legacy`, so the assertion is about the
    // scan and not about the default-mode switch).
    for (query, marker) in [("V2CMD=1", "V2CMD=1"), ("V2OUT=2", "V2OUT=2")] {
        let response = search_request(
            &config_path,
            json!({"query": query, "mode": "legacy", "sources": ["oc"], "limit": 10}),
        );
        let results = response["results"].as_array().unwrap();
        assert!(!results.is_empty(), "legacy missed {marker}: {response}");
        assert!(
            results
                .iter()
                .any(|hit| hit["snippet"].as_str().unwrap_or_default().contains(marker)),
            "legacy snippet lacks {marker}: {response}"
        );
    }

    let parsed = load_config(&config_path).unwrap();
    index_all(&parsed, None, false).unwrap();

    // Exact retrieval separates the two channels.
    let input = search_request(&config_path, exact_request("V2CMD=1", vec!["oc"]));
    let input_hit = input["results"]
        .as_array()
        .unwrap()
        .first()
        .unwrap_or_else(|| panic!("exact missed the tool input: {input}"))
        .clone();
    assert_eq!(input_hit["kind"], json!("tool_input"), "{input_hit}");
    let output = search_request(&config_path, exact_request("V2OUT=2", vec!["oc"]));
    let output_hit = output["results"]
        .as_array()
        .unwrap()
        .first()
        .unwrap_or_else(|| panic!("exact missed the tool output: {output}"))
        .clone();
    assert_eq!(output_hit["kind"], json!("tool_output"), "{output_hit}");

    // Both refs read back to the same projection, which carries input and output.
    for hit in [input_hit, output_hit] {
        let request =
            agent_memory::recall::read::ReadRequest::parse(&json!({"ref": hit["ref"].clone()}))
                .unwrap();
        let read = agent_memory::recall::read::memory_read(&parsed, &request);
        let value = serde_json::to_value(&read).unwrap();
        assert!(read.ok, "{value}");
        let text = value["text"].as_str().expect("text");
        assert!(text.contains("V2CMD=1"), "{text}");
        assert!(text.contains("V2OUT=2"), "{text}");
    }
}

// ---------------------------------------------------------------------------
// Performance (large synthetic fixture; run with `--ignored --release`)
// ---------------------------------------------------------------------------

#[test]
#[ignore = "synthetic 200k-row build + query latency; run explicitly in release"]
fn large_fixture_exact_query_latency() {
    use std::time::Instant;
    let env = support::TestEnv::new();
    let cache = cache_of(&env);
    let root = env.root("big-claude");
    let project = root.join("proj");
    std::fs::create_dir_all(&project).unwrap();
    let session = "44444444-4444-4444-8444-444444444444";
    let path = project.join(format!("{session}.jsonl"));
    let mut out = String::new();
    for index in 0..200_000u64 {
        let record = json!({
            "type": if index % 2 == 0 { "user" } else { "assistant" },
            "message": {"role": "user", "content": format!(
                "unit {index} token{:05} 参数=--flag={} payload {}", index % 997, index % 131, "x".repeat(80)
            )},
            "timestamp": format!("2026-10-25T13:{:02}:{:02}Z", (index / 60) % 60, index % 60),
            "sessionId": session,
            "uuid": format!("u{index}")
        });
        out.push_str(&record.to_string());
        out.push('\n');
    }
    std::fs::write(&path, out).unwrap();
    let config = env.write_json(
        "big-config.json",
        &v2_config(vec![source("big", "claude_code", &root)], vec!["big"], Some(&cache)),
    );
    let parsed = load_config(&config).unwrap();
    let started = Instant::now();
    let reports = index_all(&parsed, None, false).unwrap();
    let build_ms = started.elapsed().as_millis();
    let index_bytes = std::fs::metadata(index_path(&cache, "big")).unwrap().len();

    let mut timings = Vec::new();
    for index in 0..20u64 {
        let query = format!("token{:05}", (index * 37) % 997);
        let started = Instant::now();
        let _ = search_request(&config, exact_request(&query, vec!["big"]));
        timings.push(started.elapsed().as_millis() as u64);
    }
    timings.sort_unstable();
    let p95 = timings[(timings.len() as f64 * 0.95).ceil() as usize - 1];
    println!(
        "PERF rows={} build_ms={} index_bytes={} p95_ms={} max_ms={}",
        reports[0].rows,
        build_ms,
        index_bytes,
        p95,
        timings.last().unwrap()
    );
    assert_eq!(reports[0].rows, 200_000);
    assert!(p95 <= 1_000, "p95 {p95} ms exceeds 1 s");
}
