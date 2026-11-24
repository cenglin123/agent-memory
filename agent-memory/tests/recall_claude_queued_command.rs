//! Claude Code mid-turn prompts (`queued_command` attachments).
//!
//! The owner's message while the agent is working is an `attachment` record,
//! not a `type: "user"` record. These cases pin the projection (index, exact
//! search, ref readback, recent tail), the peer/task-notification exclusions,
//! the ordinary-user-record dedup and the per-source rebuild scope. Every
//! fixture is synthetic and lives under the per-case temporary root.

mod support;

use agent_memory::recall::config::load_config;
use agent_memory::recall::fulltext::build::index_all;
use agent_memory::recall::fulltext::{index_path, vectors_path};
use agent_memory::recall::read::{memory_read, ReadRequest};
use agent_memory::recall::recent::{memory_recent, RecentRequest};
use agent_memory::recall::search::{memory_search, SearchRequest};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

const CACHE: &str = "cache";
const SESSION: &str = "11111111-1111-4111-8111-111111111111";
const CODEX_SESSION: &str = "22222222-2222-4222-8222-222222222222";

fn embedding() -> Value {
    json!({
        "model_id": "fake-hash-v1",
        "dimension": 32,
        "normalization": "l2",
        "backend": "fake"
    })
}

fn config(env: &support::TestEnv, sources: Vec<Value>, order: Vec<&str>) -> PathBuf {
    let cache = env.root(CACHE);
    env.write_json(
        "recall.json",
        &json!({
            "version": 2,
            "sources": sources,
            "source_order": order,
            "interpreter": {"kb": {"command": "C:/python/python.exe"}, "embedding": embedding()},
            "domain": "default",
            "telemetry": {"enabled": false},
            "cache_root": cache.to_string_lossy()
        }),
    )
}

fn source(id: &str, kind: &str, root: &Path) -> Value {
    json!({
        "source_id": id,
        "kind": kind,
        "root": root.to_string_lossy(),
        "enabled": true,
    })
}

fn record(uuid: &str, time: &str, kind: &str, content: Value) -> Value {
    json!({
        "type": kind,
        "message": {"role": kind, "content": content},
        "timestamp": time,
        "sessionId": SESSION,
        "uuid": uuid,
        "isSidechain": false
    })
}

/// A `queued_command` attachment exactly as Claude Code writes a mid-turn
/// message. `time` is the attachment clock; the record-level `timestamp` is
/// only written when `record_time` says so.
fn queued(
    uuid: &str,
    time: &str,
    prompt: Value,
    origin: &str,
    command_mode: &str,
    with_record_time: bool,
) -> Value {
    let mut value = json!({
        "parentUuid": "parent-1",
        "isSidechain": false,
        "type": "attachment",
        "uuid": uuid,
        "attachment": {
            "type": "queued_command",
            "prompt": prompt,
            "source_uuid": "source-1",
            "delivery_id": "delivery-1",
            "commandMode": command_mode,
            "origin": {"kind": origin},
            "timestamp": time,
            "humanTurn": origin == "human"
        },
        "rendered": [],
        "renderedRole": "system"
    });
    if with_record_time {
        value["timestamp"] = json!(time);
    }
    value
}

/// The transcript: one human mid-turn prompt (no record-level timestamp, so the
/// attachment clock is the only time), a peer message, a task notification,
/// the bookkeeping copies of the same text, a duplicated ordinary record and a
/// sidechain record.
fn claude_lines() -> Vec<Value> {
    vec![
        record("u1", "2026-10-31T10:00:00Z", "user", json!("开始重构")),
        record(
            "a1",
            "2026-10-31T10:00:10Z",
            "assistant",
            json!([{"type": "text", "text": "我先看一下代码"}]),
        ),
        queued(
            "q1",
            "2026-10-31T10:00:20Z",
            json!("不要使用 model-x 模型"),
            "human",
            "prompt",
            false,
        ),
        record(
            "a2",
            "2026-10-31T10:00:30Z",
            "assistant",
            json!([{"type": "text", "text": "好，改用 model-y"}]),
        ),
        record("u2", "2026-10-31T10:00:40Z", "user", json!("用 pnpm 安装")),
        // The same text also arrives as an attachment: keep the ordinary record.
        queued(
            "q2",
            "2026-10-31T10:00:41Z",
            json!("用 pnpm 安装"),
            "human",
            "prompt",
            true,
        ),
        queued(
            "q3",
            "2026-10-31T10:00:50Z",
            json!("另一会话：顺便清理缓存"),
            "peer",
            "prompt",
            true,
        ),
        queued(
            "q4",
            "2026-10-31T10:00:55Z",
            json!("后台任务完成"),
            "task-notification",
            "task-notification",
            true,
        ),
        json!({
            "type": "queue-operation",
            "operation": "enqueue",
            "uuid": "op1",
            "timestamp": "2026-10-31T10:00:56Z",
            "sessionId": SESSION,
            "content": "另一会话：顺便清理缓存"
        }),
        json!({
            "type": "api-request-blob",
            "uuid": "blob1",
            "timestamp": "2026-10-31T10:00:57Z",
            "sessionId": SESSION,
            "request": {"messages": [{"role": "user", "content": "另一会话：顺便清理缓存"}]}
        }),
        // A sidechain flag belongs to a subagent transcript, not the main one.
        record(
            "a3",
            "2026-10-31T10:01:00Z",
            "assistant",
            json!([{"type": "text", "text": "收尾"}]),
        ),
    ]
}

fn build_claude(env: &support::TestEnv) -> PathBuf {
    let root = env.root("claude");
    let project = root.join("proj");
    std::fs::create_dir_all(&project).unwrap();
    let lines: Vec<String> = claude_lines().iter().map(Value::to_string).collect();
    std::fs::write(
        project.join(format!("{SESSION}.jsonl")),
        lines.join("\n") + "\n",
    )
    .unwrap();

    // A subagent transcript: `isSidechain: true` records stay indexed as worker
    // rows (the lexical path never returns them).
    let subagents = project.join(SESSION).join("subagents");
    std::fs::create_dir_all(&subagents).unwrap();
    let mut worker = queued(
        "w1",
        "2026-10-31T10:00:58Z",
        json!("子代理中途消息"),
        "human",
        "prompt",
        true,
    );
    worker["isSidechain"] = json!(true);
    std::fs::write(
        subagents.join("agent-abc.jsonl"),
        worker.to_string() + "\n",
    )
    .unwrap();
    root
}

fn build_codex(env: &support::TestEnv) -> PathBuf {
    let home = env.root("codex");
    let dir = home.join("sessions").join("2026").join("10").join("31");
    std::fs::create_dir_all(&dir).unwrap();
    let lines = [
        json!({
            "timestamp": "2026-10-31T09:00:00Z",
            "type": "session_meta",
            "payload": {"id": CODEX_SESSION, "cwd": "C:/work", "source": "cli"}
        })
        .to_string(),
        json!({
            "timestamp": "2026-10-31T09:00:01Z",
            "type": "response_item",
            "payload": {"type": "message", "role": "user",
                        "content": [{"type": "input_text", "text": "codex unrelated marker"}]}
        })
        .to_string(),
    ];
    std::fs::write(
        dir.join(format!("rollout-2026-10-31T09-00-00-{CODEX_SESSION}.jsonl")),
        lines.join("\n") + "\n",
    )
    .unwrap();
    home
}

fn build_device(env: &support::TestEnv) -> PathBuf {
    let root = env.root("device");
    std::fs::write(root.join("2026-10-31_note.md"), "device note marker\n").unwrap();
    root
}

fn search(config: &Path, request: Value) -> Value {
    let parsed = load_config(config).expect("config");
    let request = SearchRequest::parse(&request).expect("request");
    serde_json::to_value(memory_search(&parsed, &request)).unwrap()
}

fn read(config: &Path, request: Value) -> Value {
    let parsed = load_config(config).expect("config");
    let request = ReadRequest::parse(&request).expect("request");
    serde_json::to_value(memory_read(&parsed, &request)).unwrap()
}

fn recent(config: &Path, request: Value) -> Value {
    let parsed = load_config(config).expect("config");
    let request = RecentRequest::parse(&request).expect("request");
    serde_json::to_value(memory_recent(&parsed, &request)).unwrap()
}

fn exact(config: &Path, query: &str, sources: Vec<&str>) -> Value {
    search(
        config,
        json!({"query": query, "mode": "exact", "sources": sources, "limit": 20}),
    )
}

/// Content digest of one index's `units` table: every projected row in rowid
/// order. Proves a source's indexed content (not just its report) is unchanged.
fn units_digest(index: &Path) -> String {
    let conn = rusqlite::Connection::open(index).unwrap();
    let mut statement = conn
        .prepare("SELECT role, kind, session_id, CAST(ts AS TEXT), text, locator FROM units ORDER BY rowid")
        .unwrap();
    let rows = statement
        .query_map([], |row| {
            let mut parts: Vec<String> = Vec::new();
            for column in 0..6 {
                parts.push(row.get::<_, Option<String>>(column)?.unwrap_or_default());
            }
            Ok(parts.join("\u{1f}"))
        })
        .unwrap();
    rows.map(|row| row.unwrap()).collect::<Vec<_>>().join("\u{1e}")
}

#[test]
fn exact_search_hits_the_midturn_prompt_and_the_ref_reads_back() {
    let env = support::TestEnv::new();
    let claude = build_claude(&env);
    let config = config(&env, vec![source("cc", "claude_code", &claude)], vec!["cc"]);
    let parsed = load_config(&config).unwrap();
    let reports = index_all(&parsed, None, false).unwrap();
    assert_eq!(reports[0].status, "built", "{:?}", reports[0]);

    let response = exact(&config, "model-x", vec!["cc"]);
    let results = response["results"].as_array().unwrap();
    assert_eq!(results.len(), 1, "{response}");
    let hit = &results[0];
    assert_eq!(hit["role"], json!("user"));
    assert_eq!(hit["kind"], json!("text"));
    assert_eq!(hit["ref"]["locator"]["pointer"], json!("/attachment/prompt"));
    // The record carries no `timestamp`: the attachment clock is the time.
    assert_eq!(hit["timestamp"], json!("2026-10-31T10:00:20Z"));
    assert!(hit["snippet"].as_str().unwrap().contains("model-x"));

    let read_back = read(&config, json!({"ref": hit["ref"].clone(), "max_bytes": 4096}));
    assert_eq!(read_back["ok"], json!(true), "{read_back}");
    let text = read_back["segments"][0]["text"].as_str().unwrap_or_default();
    assert_eq!(text.trim(), "不要使用 model-x 模型");

    // The peer message, the task notification and the bookkeeping copies are
    // not user text.
    for query in ["另一会话", "后台任务完成"] {
        let other = exact(&config, query, vec!["cc"]);
        assert!(
            other["results"].as_array().unwrap().is_empty(),
            "{query} must not be indexed: {other}"
        );
    }

    // A subagent transcript keeps its sidechain worker marking, so an
    // interactive-only query never returns it.
    let worker = search(
        &config,
        json!({"query": "子代理中途消息", "mode": "exact", "sources": ["cc"],
               "session_kinds": ["interactive"], "limit": 20}),
    );
    assert!(worker["results"].as_array().unwrap().is_empty(), "{worker}");
    let any = exact(&config, "子代理中途消息", vec!["cc"]);
    assert_eq!(any["results"][0]["session_kind"], json!("worker"), "{any}");

    // The duplicated text is one unit, and it is the ordinary user record.
    let duplicate = exact(&config, "pnpm", vec!["cc"]);
    let results = duplicate["results"].as_array().unwrap();
    assert_eq!(results.len(), 1, "{duplicate}");
    assert_eq!(
        results[0]["ref"]["locator"]["pointer"],
        json!("/message/content")
    );
}

#[test]
fn exact_search_sees_a_block_prompt_and_hides_its_base64() {
    let env = support::TestEnv::new();
    let claude = build_claude(&env);
    let mut lines: Vec<String> = claude_lines().iter().map(Value::to_string).collect();
    lines.push(
        queued(
            "q6",
            "2026-10-31T10:01:10Z",
            json!([
                {"type": "text", "text": "截图里的 blast-radius 报错"},
                {"type": "image", "source": {"type": "base64", "media_type": "image/png",
                                            "data": "iVBORw0KGgoAAAANSUhEUg"}}
            ]),
            "human",
            "prompt",
            true,
        )
        .to_string(),
    );
    std::fs::write(
        claude.join("proj").join(format!("{SESSION}.jsonl")),
        lines.join("\n") + "\n",
    )
    .unwrap();
    let config = config(&env, vec![source("cc", "claude_code", &claude)], vec!["cc"]);
    let parsed = load_config(&config).unwrap();
    index_all(&parsed, None, false).unwrap();

    let response = exact(&config, "blast-radius", vec!["cc"]);
    let results = response["results"].as_array().unwrap();
    assert_eq!(results.len(), 1, "{response}");
    assert_eq!(
        results[0]["ref"]["locator"]["pointer"],
        json!("/attachment/prompt/0/text")
    );
    let read_back = read(
        &config,
        json!({"ref": results[0]["ref"].clone(), "max_bytes": 4096}),
    );
    assert_eq!(read_back["ok"], json!(true), "{read_back}");
    assert_eq!(
        read_back["segments"][0]["text"].as_str().unwrap().trim(),
        "截图里的 blast-radius 报错"
    );

    // The base64 payload is not an indexed unit.
    let image = exact(&config, "iVBORw0KGgo", vec!["cc"]);
    assert!(image["results"].as_array().unwrap().is_empty(), "{image}");
}

#[test]
fn the_recent_tail_contains_the_midturn_prompt_between_its_neighbours() {
    let env = support::TestEnv::new();
    let claude = build_claude(&env);
    let config = config(&env, vec![source("cc", "claude_code", &claude)], vec!["cc"]);

    let response = recent(
        &config,
        json!({"sources": ["cc"], "session_id": SESSION, "limit": 1, "tail_turns": 8}),
    );
    let sessions = response["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 1, "{response}");
    let tail: Vec<(String, String, String)> = sessions[0]["tail"]
        .as_array()
        .unwrap()
        .iter()
        .map(|turn| {
            (
                turn["role"].as_str().unwrap_or_default().to_string(),
                turn["timestamp"].as_str().unwrap_or_default().to_string(),
                turn["text"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect();
    let rows: Vec<(&str, &str, &str)> = tail
        .iter()
        .map(|(role, time, text)| (role.as_str(), time.as_str(), text.as_str()))
        .collect();
    assert_eq!(
        rows,
        [
            ("user", "2026-10-31T10:00:00Z", "开始重构"),
            ("assistant", "2026-10-31T10:00:10Z", "我先看一下代码"),
            // Mid-turn: between the two assistant turns, at the attachment clock.
            ("user", "2026-10-31T10:00:20Z", "不要使用 model-x 模型"),
            ("assistant", "2026-10-31T10:00:30Z", "好，改用 model-y"),
            // The duplicate attachment does not add a second turn.
            ("user", "2026-10-31T10:00:40Z", "用 pnpm 安装"),
            ("assistant", "2026-10-31T10:01:00Z", "收尾"),
        ]
    );
    // The newest conversation turn is the closing assistant record: the peer,
    // task-notification and bookkeeping records never move the clock.
    assert_eq!(
        sessions[0]["last_activity_at"],
        json!("2026-10-31T10:01:00Z")
    );
}

#[test]
fn the_hook_recent_view_reports_the_same_session() {
    let env = support::TestEnv::new();
    let claude = build_claude(&env);
    let config = config(&env, vec![source("cc", "claude_code", &claude)], vec!["cc"]);

    // `--hook-session-start` uses the same projection; a 24 h window that
    // contains the mid-turn prompt lists the session, not the message.
    let response = recent(
        &config,
        json!({"sources": ["cc"], "limit": 3, "since": "2026-10-31T09:00:00Z"}),
    );
    let ids: Vec<&str> = response["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|session| session["session_id"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(ids, [SESSION]);
}

#[test]
fn only_the_claude_index_rebuilds_when_the_projection_generation_changes() {
    let env = support::TestEnv::new();
    let claude = build_claude(&env);
    let codex = build_codex(&env);
    let device = build_device(&env);
    let config = config(
        &env,
        vec![
            source("cc", "claude_code", &claude),
            source("cx", "codex", &codex),
            source("dev", "device", &device),
        ],
        vec!["cc", "cx", "dev"],
    );
    let parsed = load_config(&config).unwrap();
    let first = index_all(&parsed, None, false).unwrap();
    assert!(first.iter().all(|report| report.status == "built"));

    let cache = env.root(CACHE);
    let untouched: Vec<(&str, String, Vec<u8>)> = ["cx", "dev"]
        .iter()
        .map(|id| {
            let index = index_path(&cache, id);
            let vectors = vectors_path(&cache, id);
            let digest = units_digest(&index);
            let vector_bytes = std::fs::read(&vectors).unwrap();
            (*id, digest, vector_bytes)
        })
        .collect();

    // A Claude index built by the previous generation (projector-2). Codex still
    // shares projector-2, and Device has its own projector generation: only the
    // Claude row is stale, so only the Claude index may rebuild.
    for id in ["cc"] {
        let conn = rusqlite::Connection::open(index_path(&cache, id)).unwrap();
        conn.execute(
            "UPDATE meta SET value = 'projector-2' WHERE key = 'projector_version'",
            [],
        )
        .unwrap();
    }

    let second = index_all(&parsed, None, false).unwrap();
    let claude_report = &second[0];
    assert_eq!(claude_report.source_id, "cc");
    assert_eq!(claude_report.status, "built", "{claude_report:?}");
    assert!(claude_report.rebuilt);
    assert_eq!(claude_report.reason.as_deref(), Some("projector_version_changed"));
    // Every conversation row was reusable: the projection fix adds a channel,
    // it does not change existing embeddings.
    let vectors = &claude_report.detail.as_ref().unwrap()["vectors"];
    assert_eq!(vectors["status"], json!("unchanged"), "{vectors}");
    assert_eq!(vectors["reused"], vectors["units"], "{vectors}");
    assert!(vectors["units"].as_u64().unwrap() > 0, "{vectors}");

    for (report, (id, digest, vector_bytes)) in second[1..].iter().zip(untouched) {
        assert_eq!(report.source_id, id);
        assert_eq!(report.status, "unchanged", "{report:?}");
        assert!(!report.rebuilt);
        assert!(report.reason.is_none(), "{report:?}");
        assert_eq!(report.added, 0);
        assert_eq!(report.removed, 0);
        assert_eq!(
            units_digest(&index_path(&cache, id)),
            digest,
            "{id}: index content must not change"
        );
        assert_eq!(
            std::fs::read(vectors_path(&cache, id)).unwrap(),
            vector_bytes,
            "{id}: vector cache must not be rewritten"
        );
    }

    // A meta row without the field is stale for that source (never silently
    // reused): the documented forward-compat behaviour.
    let conn = rusqlite::Connection::open(index_path(&cache, "cc")).unwrap();
    conn.execute("DELETE FROM meta WHERE key = 'projector_version'", [])
        .unwrap();
    drop(conn);
    let third = index_all(&parsed, None, false).unwrap();
    assert_eq!(third[0].status, "built");
    assert_eq!(third[0].reason.as_deref(), Some("projector_version_changed"));
    assert_eq!(third[1].status, "unchanged");
    assert_eq!(third[2].status, "unchanged");
}

#[test]
fn a_new_midturn_prompt_reuses_every_unchanged_vector() {
    let env = support::TestEnv::new();
    let claude = build_claude(&env);
    // Drop the subagent transcript: this case counts the main session's vectors.
    let _ = std::fs::remove_dir_all(claude.join("proj").join(SESSION).join("subagents"));
    // Start without the mid-turn prompt or the answer that follows it.
    let path = claude.join("proj").join(format!("{SESSION}.jsonl"));
    let kept = [
        record("u1", "2026-10-31T10:00:00Z", "user", json!("开始重构")),
        record(
            "a1",
            "2026-10-31T10:00:10Z",
            "assistant",
            json!([{"type": "text", "text": "我先看一下代码"}]),
        ),
    ];
    let lines: Vec<String> = kept.iter().map(Value::to_string).collect();
    std::fs::write(&path, lines.join("\n") + "\n").unwrap();

    let config = config(&env, vec![source("cc", "claude_code", &claude)], vec!["cc"]);
    let parsed = load_config(&config).unwrap();
    let first = index_all(&parsed, None, false).unwrap();
    let before = &first[0].detail.as_ref().unwrap()["vectors"];
    assert_eq!(before["units"], json!(2), "{before}");

    // The conversation continues with a mid-turn prompt and the answer.
    let mut lines: Vec<String> = kept.iter().map(Value::to_string).collect();
    lines.push(
        queued(
            "q1",
            "2026-10-31T10:00:20Z",
            json!("不要使用 model-x 模型"),
            "human",
            "prompt",
            true,
        )
        .to_string(),
    );
    lines.push(
        record(
            "a2",
            "2026-10-31T10:00:30Z",
            "assistant",
            json!([{"type": "text", "text": "好，改用 model-y"}]),
        )
        .to_string(),
    );
    std::fs::write(&path, lines.join("\n") + "\n").unwrap();

    let second = index_all(&parsed, None, false).unwrap();
    assert_eq!(second[0].status, "updated", "{:?}", second[0]);
    let vectors = &second[0].detail.as_ref().unwrap()["vectors"];
    assert_eq!(vectors["units"], json!(4), "{vectors}");
    assert_eq!(vectors["reused"], json!(2), "{vectors}");
    assert_eq!(vectors["embedded"], json!(2), "{vectors}");
}

#[test]
fn the_index_stays_idempotent_for_midturn_prompts() {
    let env = support::TestEnv::new();
    let claude = build_claude(&env);
    let config = config(&env, vec![source("cc", "claude_code", &claude)], vec!["cc"]);
    let parsed = load_config(&config).unwrap();
    let first = index_all(&parsed, None, false).unwrap();
    let rows = first[0].rows;
    let again = index_all(&parsed, None, false).unwrap();
    assert_eq!(again[0].status, "unchanged", "{:?}", again[0]);
    assert_eq!(again[0].rows, rows);
    assert_eq!(again[0].added, 0);
}
