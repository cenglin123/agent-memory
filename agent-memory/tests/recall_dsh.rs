//! DSH (DeepSeek Harness) live source (`dsh-jsonl`): frame-level reading,
//! generation selection, projection, exact index/incrementality, readback and
//! the `memory_recent` hot layer.
//!
//! Every fixture is synthetic and written under the per-case temporary root.
//! The zstd carriers are built here: one file is several consecutive frames,
//! exactly like the production backend's append-only frame stream.

mod support;

use agent_memory::recall::config::{load_config, RecallConfig};
use agent_memory::recall::contract::{codes, status, SCHEMA_DSH_JSONL, SOURCE_KIND_DSH};
use agent_memory::recall::fulltext::build::index_all;
use agent_memory::recall::read::{memory_read, ReadRequest, ReadResponse};
use agent_memory::recall::recent::{memory_recent, RecentRequest, RecentResponse};
use agent_memory::recall::search::{memory_search, SearchRequest, SearchResponse};
use serde_json::{json, Value};
use std::io::Write;
use std::path::{Path, PathBuf};

const INTERACTIVE: &str = "11111111-1111-4111-8111-111111111111";
const WORKER: &str = "22222222-2222-4222-8222-222222222222";
const PROJECT: &str = "--C-Users-x-Documents-GitHub--";
const CWD: &str = "C:\\Users\\x\\Documents\\GitHub";

fn ts(rfc3339: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(rfc3339)
        .expect("timestamp")
        .timestamp_millis()
}

/// One compressed frame holding the given lines (each newline terminated).
fn frame_of(lines: &[String]) -> Vec<u8> {
    let mut text = String::new();
    for line in lines {
        text.push_str(line);
        text.push('\n');
    }
    zstd::stream::encode_all(text.as_bytes(), 3).expect("zstd frame")
}

/// Write a log as `groups` consecutive frames (one frame per chunk of lines).
fn write_log(path: &Path, lines: &[String], group: usize) {
    let mut bytes = Vec::new();
    for chunk in lines.chunks(group.max(1)) {
        bytes.extend_from_slice(&frame_of(chunk));
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("session dir");
    }
    std::fs::write(path, bytes).expect("write dsh log");
}

fn header(version: u32, id: &str, extra: Value) -> String {
    let mut value = json!({
        "type": "session",
        "version": version,
        "id": id,
        "createdAt": ts("2026-11-07T08:00:00Z"),
        "cwd": CWD,
    });
    if let Value::Object(extra) = extra {
        for (key, field) in extra {
            value_merge(&mut value, &key, field);
        }
    }
    value.to_string()
}

fn value_merge(target: &mut Value, key: &str, value: Value) {
    target
        .as_object_mut()
        .expect("object")
        .insert(key.to_string(), value);
}

fn event(kind: &str, seq: u64, time: i64, data: Value) -> String {
    json!({"type": kind, "seq": seq, "time": time, "data": data}).to_string()
}

fn user_v4(text: &str) -> Value {
    json!({
        "id": "m-user",
        "role": "user",
        "source": {"kind": "user"},
        "content": [{"type": "text", "text": text}]
    })
}

fn assistant_v4(text: &str) -> Value {
    json!({
        "message": {
            "id": "m-assistant",
            "role": "assistant",
            "source": {"kind": "model"},
            "content": [
                {"type": "reasoning", "text": format!("{text} dshleakreasoning")},
                {"type": "text", "text": text},
                {"type": "tool-call", "id": "c1", "name": "shell", "arguments": "{}"}
            ]
        }
    })
}

fn source(id: &str, root: &Path) -> Value {
    json!({
        "source_id": id,
        "kind": "dsh",
        "root": root.to_string_lossy(),
        "enabled": true,
    })
}

fn config_value(sources: Vec<Value>, order: Vec<&str>, cache_root: Option<&Path>) -> Value {
    let mut value = json!({
        "version": 2,
        "sources": sources,
        "source_order": order,
        "interpreter": {"kb": {"command": "C:/python/python.exe"}},
        "domain": "default",
        "telemetry": {"enabled": false}
    });
    if let Some(cache) = cache_root {
        value["cache_root"] = json!(cache.to_string_lossy());
    }
    value
}

fn load(env: &support::TestEnv, name: &str, value: Value) -> RecallConfig {
    let path = env.write_json(&format!("{name}.json"), &value);
    load_config(&path).expect("config")
}

fn search(config: &RecallConfig, request: Value) -> SearchResponse {
    memory_search(config, &SearchRequest::parse(&request).expect("search request"))
}

fn search_value(config: &RecallConfig, request: Value) -> Value {
    serde_json::to_value(search(config, request)).expect("search value")
}

fn read(config: &RecallConfig, reference: Value) -> ReadResponse {
    memory_read(
        config,
        &ReadRequest::parse(&json!({"ref": reference})).expect("read request"),
    )
}

fn recent(config: &RecallConfig, request: Value) -> RecentResponse {
    memory_recent(config, &RecentRequest::parse(&request).expect("recent request"))
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

fn status_of(response: &Value, source_id: &str) -> Value {
    response["source_status"]
        .as_array()
        .expect("source_status")
        .iter()
        .find(|entry| entry["source_id"] == json!(source_id))
        .map(|entry| entry["status"].clone())
        .unwrap_or(Value::Null)
}

fn code_of(response: &Value, source_id: &str) -> Value {
    response["source_status"]
        .as_array()
        .expect("source_status")
        .iter()
        .find(|entry| entry["source_id"] == json!(source_id))
        .map(|entry| entry["code"].clone())
        .unwrap_or(Value::Null)
}

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

struct Fixture {
    root: PathBuf,
    interactive_dir: PathBuf,
    worker_dir: PathBuf,
    legacy_dir: PathBuf,
}

/// The full synthetic corpus:
///
/// * `session-<uuid>`: a v4 session with every projected and skipped envelope;
/// * a bare UUID dir: a v3 worker session;
/// * `session-<uuid>`: a legacy `session.jsonl.zstd` (header version 1).
fn build(env: &support::TestEnv) -> Fixture {
    let root = env.root("dsh");
    let interactive_dir = root.join(PROJECT).join(format!("session-{INTERACTIVE}"));
    let worker_dir = root.join(PROJECT).join(WORKER);
    let legacy_dir = root.join("--C-Users-x-Other--").join("session-33333333-3333-4333-8333-333333333333");

    let t0 = ts("2026-11-07T09:00:00Z");
    let lines = vec![
        header(4, INTERACTIVE, json!({"agentPreset": "standard"})),
        event("user/message", 1, t0 + 1_000, user_v4("dshneedle user text")),
        event("assistant/message", 2, t0 + 2_000, assistant_v4("dshneedle assistant text")),
        event(
            "tool/call",
            3,
            t0 + 3_000,
            json!({"callId": "c1", "name": "shell", "arguments": "{\"cmd\":\"dshneedle\"}"}),
        ),
        event(
            "tool/result",
            4,
            t0 + 4_000,
            json!({
                "message": {
                    "id": "m-tool",
                    "role": "tool",
                    "toolCallId": "c1",
                    "source": {"kind": "tool", "callId": "c1"},
                    "content": [{"type": "text", "text": "dshneedle tool output"}]
                }
            }),
        ),
        event(
            "system/message",
            5,
            t0 + 5_000,
            json!({"message": {"id": "m-system", "role": "system", "source": {"kind": "system"},
                "content": [{"type": "text", "text": "dshleaksystem"}]}}),
        ),
        event(
            "agent/inbox/spliced",
            6,
            t0 + 6_000,
            json!({"inserted": [{"id": "m-spliced", "role": "user", "source": {"kind": "user"},
                "content": [{"type": "text", "text": "dshleakspliced"}]}]}),
        ),
        event(
            "assistant/chunk",
            7,
            t0 + 7_000,
            json!({"text": "dshleakchunk"}),
        ),
        event(
            "user/message",
            8,
            t0 + 8_000,
            json!({"id": "m-synthetic", "role": "user", "source": {"kind": "system"},
                "content": [{"type": "text", "text": "dshleaksynthetic"}]}),
        ),
        event("session/title", 9, t0 + 9_000, json!({"title": "DSH 主会话"})),
    ];
    write_log(&interactive_dir.join("session.v4.jsonl.zstd"), &lines, 3);

    let t1 = ts("2026-11-07T10:00:00Z");
    let worker_lines = vec![
        header(3, "id-worker", json!({})),
        event(
            "user/message",
            1,
            t1,
            json!({"id": "w1", "role": "user", "source": {"kind": "user"},
                "content": [{"type": "text", "text": "dshworkerneedle question"}]}),
        ),
        event(
            "assistant/message",
            2,
            t1 + 1_000,
            json!({"message": {"id": "w2", "role": "assistant", "source": {"kind": "model"},
                "content": [{"type": "text", "text": "dshworkerneedle answer"}]}}),
        ),
        event(
            "subagent/descriptor",
            3,
            t1 + 2_000,
            json!({"label": "research worker", "agentModel": "deepseek-v4"}),
        ),
        // v3 tool result carries the `tool-result` wrapper.
        event(
            "tool/result",
            4,
            t1 + 3_000,
            json!({"message": {"id": "w3", "role": "user", "source": {"kind": "tool", "callId": "wc1"},
                "content": [{"type": "tool-result", "toolCallId": "wc1",
                    "content": [{"type": "text", "text": "dshworkerneedle tool output"}]}]}}),
        ),
    ];
    write_log(&worker_dir.join("session.v3.jsonl.zstd"), &worker_lines, 5);

    let t2 = ts("2026-11-07T07:00:00Z");
    let legacy_lines = vec![
        header(1, "id-legacy", json!({})),
        event("user/message", 1, t2, user_v4("dshneedlelegacy user")),
        event("assistant/message", 2, t2 + 1_000, assistant_v4("dshneedlelegacy answer")),
    ];
    write_log(&legacy_dir.join("session.jsonl.zstd"), &legacy_lines, 2);

    Fixture {
        root,
        interactive_dir,
        worker_dir,
        legacy_dir,
    }
}

// ---------------------------------------------------------------------------
// Projection / generation selection
// ---------------------------------------------------------------------------

#[test]
fn v4_v3_and_legacy_sessions_project_user_assistant_and_tool_units() {
    let env = support::TestEnv::new();
    let fixture = build(&env);
    let config = load(&env, "dsh-projection", config_value(vec![source("dsh", &fixture.root)], vec!["dsh"], None));

    let response = search_value(&config, json!({"query": "dshneedle", "limit": 50}));
    assert_eq!(status_of(&response, "dsh"), json!("ok"), "{response}");
    let results = response["results"].as_array().unwrap();
    // v4: user + assistant + tool call + tool result; legacy: user +
    // assistant. The v3 carrier is a bare-UUID worker directory, which the
    // lexical path never returns (it is indexed, so exact mode can include it).
    assert_eq!(results.len(), 6, "{response}");
    for hit in results {
        assert_eq!(hit["source_kind"], json!(SOURCE_KIND_DSH));
        assert_eq!(hit["ref"]["schema_gen"], json!(SCHEMA_DSH_JSONL));
        assert!(hit["ref"]["locator"]["record_index"].is_number());
        assert!(hit["ref"]["locator"]["event_seq"].is_number());
        assert_eq!(hit["ref"]["locator"]["carrier"], hit["ref"]["rel_path"]);
        assert!(hit["ref"]["rel_path"].as_str().unwrap().ends_with(".jsonl.zstd"));
    }
    // Skipped envelopes never appear.
    for needle in [
        "dshleakreasoning",
        "dshleaksystem",
        "dshleakspliced",
        "dshleakchunk",
        "dshleaksynthetic",
    ] {
        let skipped = search_value(&config, json!({"query": needle}));
        assert!(
            skipped["results"].as_array().unwrap().is_empty(),
            "{needle} leaked: {skipped}"
        );
    }
    // The tool call and tool result are their own tool units.
    let pointers: Vec<&str> = results
        .iter()
        .map(|hit| hit["ref"]["locator"]["pointer"].as_str().unwrap())
        .collect();
    assert!(pointers.contains(&"/data/arguments"), "{pointers:?}");
    assert!(
        pointers.contains(&"/data/message/content/0"),
        "{pointers:?}"
    );
    // The v4 session title comes from `session/title` (the worker descriptor
    // label is covered by the recent test, since the scan hides workers).
    let interactive = search_value(
        &config,
        json!({"query": "dshneedle assistant text", "session_id": INTERACTIVE}),
    );
    assert_eq!(interactive["results"][0]["title"], json!("DSH 主会话"));
    // Legacy (header version 1) is readable under the same schema generation.
    let legacy = search_value(&config, json!({"query": "dshneedlelegacy"}));
    assert_eq!(legacy["results"].as_array().unwrap().len(), 2, "{legacy}");
    let _ = (&fixture.worker_dir, &fixture.legacy_dir);
}

#[test]
fn lexical_path_never_returns_workers() {
    let env = support::TestEnv::new();
    let fixture = build(&env);
    let config = load(&env, "dsh-worker", config_value(vec![source("dsh", &fixture.root)], vec!["dsh"], None));

    let response = search_value(&config, json!({"query": "dshworkerneedle"}));
    assert!(
        response["results"].as_array().unwrap().is_empty(),
        "a bare-UUID subagent dir is a worker and the scan excludes it: {response}"
    );
    assert_eq!(status_of(&response, "dsh"), json!("empty"), "{response}");
    // The index still stores workers so exact mode can include them.
}

#[test]
fn highest_generation_wins_and_a_future_generation_is_unsupported() {
    let env = support::TestEnv::new();
    let root = env.root("dsh-generations");
    let dir = root.join(PROJECT).join(format!("session-{INTERACTIVE}"));
    let t = ts("2026-11-07T09:00:00Z");
    let v3 = vec![
        header(3, INTERACTIVE, json!({})),
        event("user/message", 1, t, user_v4("dshneedle from v3 only")),
    ];
    let v4 = vec![
        header(4, INTERACTIVE, json!({})),
        event("user/message", 1, t + 1_000, user_v4("dshneedle from v4 only")),
    ];
    write_log(&dir.join("session.v3.jsonl.zstd"), &v3, 2);
    write_log(&dir.join("session.v4.jsonl.zstd"), &v4, 2);

    let config = load(&env, "dsh-generations", config_value(vec![source("dsh", &root)], vec!["dsh"], None));
    let response = search_value(&config, json!({"query": "dshneedle"}));
    assert_eq!(response["results"].as_array().unwrap().len(), 1, "{response}");
    assert_eq!(
        response["results"][0]["snippet"],
        json!("dshneedle from v4 only"),
        "only the highest generation is read"
    );

    // A future generation in the same directory makes that session unsupported.
    let future_dir = root.join(PROJECT).join(format!("session-{WORKER}"));
    std::fs::create_dir_all(&future_dir).unwrap();
    std::fs::write(future_dir.join("session.v5.jsonl.zstd"), frame_of(&[header(
        5,
        WORKER,
        json!({}),
    )]))
    .unwrap();
    let response = search_value(&config, json!({"query": "dshneedle"}));
    assert_eq!(code_of(&response, "dsh"), json!(codes::SCHEMA_UNSUPPORTED), "{response}");
    assert_eq!(
        detail_of(&response, "dsh")["coverage"]["unsupported_sessions"],
        json!(1)
    );
    // The readable session still answers (partial, not lost).
    assert_eq!(response["results"].as_array().unwrap().len(), 1, "{response}");
}

// ---------------------------------------------------------------------------
// Frames and torn tails
// ---------------------------------------------------------------------------

#[test]
fn multi_frame_logs_read_every_frame_and_a_torn_tail_is_disclosed() {
    let env = support::TestEnv::new();
    let root = env.root("dsh-frames");
    let dir = root.join(PROJECT).join(format!("session-{INTERACTIVE}"));
    let t = ts("2026-11-07T09:00:00Z");
    let lines = vec![
        header(4, INTERACTIVE, json!({})),
        event("user/message", 1, t, user_v4("dshneedle frame one")),
        event("user/message", 2, t + 1_000, user_v4("dshneedle frame two")),
        event("user/message", 3, t + 2_000, user_v4("dshneedle frame three")),
    ];
    let path = dir.join("session.v4.jsonl.zstd");
    write_log(&path, &lines, 1); // one frame per line: four frames

    let config = load(&env, "dsh-frames", config_value(vec![source("dsh", &root)], vec!["dsh"], None));
    let response = search_value(&config, json!({"query": "dshneedle"}));
    assert_eq!(response["results"].as_array().unwrap().len(), 3, "{response}");
    assert!(detail_of(&response, "dsh")["coverage"].get("partial_tail").is_none());

    // Truncate the last frame in half: earlier complete frames stay readable,
    // the torn frame contributes nothing and `partial_tail` is disclosed.
    let bytes = std::fs::read(&path).unwrap();
    let mut offsets = Vec::new();
    let mut position = 0usize;
    while position < bytes.len() {
        let size = zstd::zstd_safe::find_frame_compressed_size(&bytes[position..]).unwrap();
        offsets.push((position, size));
        position += size;
    }
    assert_eq!(offsets.len(), 4);
    let (last_start, last_size) = *offsets.last().unwrap();
    let cut = last_start + last_size / 2;
    std::fs::write(&path, &bytes[..cut]).unwrap();
    let response = search_value(&config, json!({"query": "dshneedle"}));
    let count = response["results"].as_array().unwrap().len();
    assert!(count >= 1 && count < 3, "torn tail keeps the complete prefix: {response}");
    assert_eq!(
        detail_of(&response, "dsh")["coverage"]["partial_tail"],
        json!(1),
        "{response}"
    );
    assert_eq!(status_of(&response, "dsh"), json!("ok"), "{response}");
}

#[test]
fn an_interior_corrupt_frame_is_record_skipped() {
    let env = support::TestEnv::new();
    let root = env.root("dsh-corrupt");
    let dir = root.join(PROJECT).join(format!("session-{INTERACTIVE}"));
    let t = ts("2026-11-07T09:00:00Z");
    let lines = vec![
        header(4, INTERACTIVE, json!({})),
        event("user/message", 1, t, user_v4("dshneedle before damage")),
        event("user/message", 2, t + 1_000, user_v4("dshneedle inside damage")),
        event("user/message", 3, t + 2_000, user_v4("dshneedle after damage")),
    ];
    let path = dir.join("session.v4.jsonl.zstd");
    write_log(&path, &lines, 1);
    let mut bytes = std::fs::read(&path).unwrap();
    // Corrupt the second frame's payload (checksum failure), keeping the frame
    // structure and the later frames intact.
    let mut offsets = Vec::new();
    let mut position = 0usize;
    while position < bytes.len() {
        let size = zstd::zstd_safe::find_frame_compressed_size(&bytes[position..]).unwrap();
        offsets.push((position, size));
        position += size;
    }
    assert_eq!(offsets.len(), 4);
    let (start, _size) = offsets[2];
    bytes[start] ^= 0xFF; // break the frame magic: no valid frame at this offset
    std::fs::write(&path, &bytes).unwrap();

    let config = load(&env, "dsh-corrupt", config_value(vec![source("dsh", &root)], vec!["dsh"], None));
    let response = search_value(&config, json!({"query": "dshneedle"}));
    assert_eq!(status_of(&response, "dsh"), json!("partial"), "{response}");
    assert_eq!(code_of(&response, "dsh"), json!(codes::RECORD_SKIPPED), "{response}");
    assert_eq!(detail_of(&response, "dsh")["coverage"]["corrupt_sessions"], json!(1));
    let hits = response["results"].as_array().unwrap();
    let snippets: Vec<&str> = hits
        .iter()
        .map(|hit| hit["snippet"].as_str().unwrap())
        .collect();
    assert!(snippets.contains(&"dshneedle before damage"), "{snippets:?}");
    assert!(snippets.contains(&"dshneedle after damage"), "{snippets:?}");
    assert!(!snippets.contains(&"dshneedle inside damage"), "{snippets:?}");
}

// ---------------------------------------------------------------------------
// Readback
// ---------------------------------------------------------------------------

#[test]
fn every_hit_reads_back_and_tampering_fails_closed() {
    let env = support::TestEnv::new();
    let fixture = build(&env);
    let config = load(&env, "dsh-read", config_value(vec![source("dsh", &fixture.root)], vec!["dsh"], None));
    let response = search_value(&config, json!({"query": "dshneedle", "limit": 50}));
    for hit in response["results"].as_array().unwrap() {
        let read = serde_json::to_value(read(&config, hit["ref"].clone())).unwrap();
        assert_eq!(read["ok"], json!(true), "{read}");
        assert!(read["text"].as_str().unwrap().contains("dshneedle"));
    }

    let target = response["results"]
        .as_array()
        .unwrap()
        .iter()
        .find(|hit| {
            hit["ref"]["rel_path"].as_str().unwrap().contains("session.v4")
                && hit["ref"]["locator"]["record_index"] == json!(1)
        })
        .expect("v4 user hit")
        .clone();
    let reference = target["ref"].clone();
    let contextual = memory_read(
        &config,
        &ReadRequest::parse(&json!({
            "ref": reference,
            "context_before": 1,
            "context_after": 1
        }))
        .unwrap(),
    );
    let contextual = serde_json::to_value(contextual).unwrap();
    assert_eq!(contextual["ok"], json!(true), "{contextual}");
    assert!(contextual["segments"].as_array().unwrap().len() >= 2);
    assert!(contextual["text"].is_null());

    // Forged range: fail closed.
    let mut forged = target["ref"].clone();
    forged["text_range"]["byte_end"] = json!(1);
    assert_eq!(
        serde_json::to_value(read(&config, forged)).unwrap()["code"],
        json!("REF_STALE")
    );

    // Tampered projection: the addressed line is rewritten with different
    // text while keeping its position and pointer.
    let path = fixture.interactive_dir.join("session.v4.jsonl.zstd");
    let original = std::fs::read(&path).unwrap();
    let t = ts("2026-11-07T09:00:00Z");
    let tampered_lines = vec![
        header(4, INTERACTIVE, json!({"agentPreset": "standard"})),
        event(
            "user/message",
            1,
            t + 1_000,
            user_v4("dshneedle TAMPERED text"),
        ),
        event(
            "assistant/message",
            2,
            t + 2_000,
            assistant_v4("dshneedle assistant text"),
        ),
        event(
            "tool/call",
            3,
            t + 3_000,
            json!({"callId": "c1", "name": "shell", "arguments": "{\"cmd\":\"dshneedle\"}"}),
        ),
        event(
            "tool/result",
            4,
            t + 4_000,
            json!({
                "message": {
                    "id": "m-tool",
                    "role": "tool",
                    "toolCallId": "c1",
                    "source": {"kind": "tool", "callId": "c1"},
                    "content": [{"type": "text", "text": "dshneedle tool output"}]
                }
            }),
        ),
    ];
    write_log(&path, &tampered_lines, 2);
    assert_eq!(
        serde_json::to_value(read(&config, target["ref"].clone())).unwrap()["code"],
        json!("REF_STALE"),
        "changed bytes must not read back"
    );
    std::fs::write(&path, original).unwrap();

    // Truncation: a ref past the readable log is a missing locator.
    let last = response["results"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|hit| hit["ref"]["rel_path"].as_str().unwrap().contains("session.v4"))
        .max_by_key(|hit| hit["ref"]["locator"]["record_index"].as_u64().unwrap_or(0))
        .unwrap()
        .clone();
    let path = fixture.interactive_dir.join("session.v4.jsonl.zstd");
    let bytes = std::fs::read(&path).unwrap();
    let first_frame = zstd::zstd_safe::find_frame_compressed_size(&bytes).unwrap();
    std::fs::write(&path, &bytes[..first_frame]).unwrap();
    let missing = serde_json::to_value(read(&config, last["ref"].clone())).unwrap();
    assert_eq!(missing["code"], json!("REF_LOCATOR_MISSING"), "{missing}");

    // A locator whose carrier names another generation fails closed.
    let mut wrong_carrier = target["ref"].clone();
    wrong_carrier["locator"]["carrier"] = json!("proj/other/session.v4.jsonl.zstd");
    assert_eq!(
        serde_json::to_value(read(&config, wrong_carrier)).unwrap()["code"],
        json!("REF_LOCATOR_MISSING")
    );

    // A ref from another root fails closed.
    let mut foreign = target["ref"].clone();
    foreign["rel_path"] = json!("../outside.jsonl.zstd");
    assert_eq!(
        serde_json::to_value(read(&config, foreign)).unwrap()["code"],
        json!("REF_LOCATOR_MISSING")
    );
}

// ---------------------------------------------------------------------------
// Exact index / incrementality / generation switch
// ---------------------------------------------------------------------------

#[test]
fn the_index_is_incremental_and_a_generation_switch_rebuilds_the_session() {
    let env = support::TestEnv::new();
    let root = env.root("dsh-index");
    let dir = root.join(PROJECT).join(format!("session-{INTERACTIVE}"));
    let cache = env.root("dsh-index-cache");
    let t = ts("2026-11-07T09:00:00Z");
    let lines = vec![
        header(3, INTERACTIVE, json!({})),
        event("user/message", 1, t, user_v4("dshneedle indexed user")),
        event("assistant/message", 2, t + 1_000, assistant_v4("dshneedle indexed answer")),
    ];
    let path = dir.join("session.v3.jsonl.zstd");
    write_log(&path, &lines, 2);

    let config = load(
        &env,
        "dsh-index",
        config_value(vec![source("dsh", &root)], vec!["dsh"], Some(&cache)),
    );
    let first = index_all(&config, None, false).expect("index");
    assert_eq!(first[0].status, "built", "{:?}", first[0]);
    assert_eq!(first[0].rows, 2);

    // Exact mode uses the index and skips reasoning/chunks by construction.
    let exact = search_value(
        &config,
        json!({"query": "dshneedle", "mode": "exact", "limit": 20}),
    );
    assert_eq!(status_of(&exact, "dsh"), json!("ok"), "{exact}");
    assert_eq!(exact["results"].as_array().unwrap().len(), 2, "{exact}");
    let indexed_ref = exact["results"][0]["ref"].clone();
    assert_eq!(
        serde_json::to_value(read(&config, indexed_ref.clone())).unwrap()["ok"],
        json!(true)
    );

    // Append one frame: only the new line is inserted.
    let mut file = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
    file.write_all(&frame_of(&[event(
        "user/message",
        3,
        t + 2_000,
        user_v4("dshneedle appended later"),
    )]))
    .unwrap();
    drop(file);
    let second = index_all(&config, None, false).expect("index");
    assert_eq!(second[0].status, "updated", "{:?}", second[0]);
    assert_eq!(second[0].added, 1, "{:?}", second[0]);
    assert_eq!(second[0].removed, 0, "{:?}", second[0]);
    let after = search_value(
        &config,
        json!({"query": "dshneedle appended", "mode": "exact"}),
    );
    assert_eq!(after["results"].as_array().unwrap().len(), 1, "{after}");
    // The default mode resolves to the indexed path once the index is ready.
    let default_mode = search_value(
        &config,
        json!({"query": "dshneedle appended", "limit": 10}),
    );
    assert_eq!(
        default_mode["results"].as_array().unwrap().len(),
        1,
        "{default_mode}"
    );
    assert_eq!(status_of(&default_mode, "dsh"), json!("ok"), "{default_mode}");

    // Generation switch: a v4 carrier replaces the v3 rows (the old generation
    // file stays on disk untouched).
    let v4 = vec![
        header(4, INTERACTIVE, json!({})),
        event("user/message", 1, t + 5_000, user_v4("dshneedle v4 replacement")),
    ];
    write_log(&dir.join("session.v4.jsonl.zstd"), &v4, 1);
    let third = index_all(&config, None, false).expect("index");
    assert_eq!(third[0].status, "built", "{:?}", third[0]);
    assert!(third[0].rebuilt, "{:?}", third[0]);
    let old = search_value(
        &config,
        json!({"query": "dshneedle indexed", "mode": "exact"}),
    );
    assert!(old["results"].as_array().unwrap().is_empty(), "{old}");
    let new = search_value(
        &config,
        json!({"query": "dshneedle v4 replacement", "mode": "exact"}),
    );
    assert_eq!(new["results"].as_array().unwrap().len(), 1, "{new}");
    assert!(new["results"][0]["ref"]["rel_path"]
        .as_str()
        .unwrap()
        .ends_with("session.v4.jsonl.zstd"));
}

// ---------------------------------------------------------------------------
// memory_recent
// ---------------------------------------------------------------------------

fn build_opencode(root: &Path, text: &str, at_ms: i64) -> PathBuf {
    let db = root.join("opencode.db");
    let conn = rusqlite::Connection::open(&db).expect("opencode db");
    conn.execute_batch(support::V2_SCHEMA).expect("schema");
    conn.execute(
        "INSERT INTO session_v2 (id, title, directory, time_created, time_updated, parent_id) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params!["oc-main", "OC 会话", "C:/work", at_ms - 1_000, at_ms, Option::<String>::None],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO session_message (id, session_id, seq, type, time_created, data) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![
            "oc-m1",
            "oc-main",
            1i64,
            "user",
            at_ms,
            serde_json::to_string(&json!({"text": text})).unwrap()
        ],
    )
    .unwrap();
    db
}

fn build_claude(root: &Path, text: &str, at_rfc3339: &str) -> PathBuf {
    let session = "44444444-4444-4444-8444-444444444444";
    let project = root.join("proj");
    std::fs::create_dir_all(&project).unwrap();
    let path = project.join(format!("{session}.jsonl"));
    let lines = [
        json!({"type": "user", "message": {"role": "user", "content": text},
            "timestamp": at_rfc3339, "sessionId": session, "uuid": "cu1"}),
        json!({"type": "assistant", "message": {"role": "assistant", "content": format!("{text} answer")},
            "timestamp": at_rfc3339, "sessionId": session, "uuid": "ca1"}),
    ];
    let mut body = String::new();
    for line in lines {
        body.push_str(&line.to_string());
        body.push('\n');
    }
    std::fs::write(&path, body).unwrap();
    path
}

#[test]
fn recent_orders_across_sources_excludes_workers_and_tail_refs_match_search_hits() {
    let env = support::TestEnv::new();
    let fixture = build(&env);
    let opencode_root = env.root("dsh-recent-oc");
    let claude_root = env.root("dsh-recent-cc");
    build_opencode(&opencode_root, "dshneedle opencode turn", ts("2026-11-07T11:00:00Z"));
    build_claude(&claude_root, "dshneedle claude turn", "2026-11-07T12:00:00Z");

    let config = load(
        &env,
        "dsh-recent",
        config_value(
            vec![
                source("dsh", &fixture.root),
                json!({"source_id": "oc", "kind": "opencode", "root": opencode_root.to_string_lossy(), "enabled": true}),
                json!({"source_id": "cc", "kind": "claude_code", "root": claude_root.to_string_lossy(), "enabled": true}),
            ],
            vec!["dsh", "oc", "cc"],
            None,
        ),
    );

    let response = recent(&config, json!({"limit": 20, "tail_turns": 3}));
    assert_eq!(response.exit_code, 0, "{:?}", response.errors);
    let sessions: Vec<(String, String)> = response
        .sessions
        .iter()
        .map(|session| (session.source.clone(), session.session_id.clone()))
        .collect();
    assert_eq!(
        sessions,
        vec![
            ("cc".to_string(), "44444444-4444-4444-8444-444444444444".to_string()),
            ("oc".to_string(), "oc-main".to_string()),
            ("dsh".to_string(), INTERACTIVE.to_string()),
            ("dsh".to_string(), "id-legacy".to_string()),
        ],
        "one cross-source clock order, worker excluded"
    );
    let dsh = &response.sessions[2];
    assert_eq!(dsh.title.as_deref(), Some("DSH 主会话"));
    assert_eq!(dsh.cwd.as_deref(), Some(CWD));
    // The tail keeps user/assistant text only and reads backwards.
    assert!(dsh.tail.iter().all(|turn| turn.role != "tool"));

    // A direct `session_id` lookup escapes the candidate window and includes
    // the worker.
    let worker = recent(
        &config,
        json!({"session_id": "id-worker", "tail_turns": 2, "limit": 5}),
    );
    assert_eq!(worker.sessions.len(), 1, "{:?}", worker.errors);
    assert_eq!(worker.sessions[0].session_kind, "worker");
    assert_eq!(worker.sessions[0].title.as_deref(), Some("research worker"));
    assert!(worker.sessions[0]
        .tail
        .iter()
        .any(|turn| turn.text.contains("dshworkerneedle answer")));

    // `tail_roles: ["tool"]` includes tool units, default does not.
    let tools = recent(
        &config,
        json!({"session_id": INTERACTIVE, "tail_turns": 10, "tail_roles": ["tool"], "limit": 1}),
    );
    assert!(!tools.sessions.is_empty());

    // A tail ref is byte-identical to the search hit ref for the same unit.
    let latest = dsh.tail.last().expect("dsh tail");
    let needle = latest.text.split_whitespace().last().unwrap_or("dshneedle");
    let hits = search_value(&config, json!({"query": needle, "sources": ["dsh"], "limit": 50}));
    let mut matched = false;
    for hit in hits["results"].as_array().unwrap() {
        if hit["ref"] == serde_json::to_value(&latest.reference).unwrap() {
            matched = true;
        }
    }
    assert!(matched, "no search hit carried the tail ref: {hits}");
}

#[test]
fn a_bookkeeping_only_session_uses_the_event_clock_and_has_no_title() {
    let env = support::TestEnv::new();
    let root = env.root("dsh-clock");
    let dir = root.join(PROJECT).join(format!("session-{INTERACTIVE}"));
    let t = ts("2026-11-07T09:30:00Z");
    let lines = vec![
        header(4, INTERACTIVE, json!({})),
        event("system/message", 1, t, json!({"message": {"id": "s1", "role": "system",
            "source": {"kind": "system"}, "content": [{"type": "text", "text": "boot"}]}}),
        ),
        event("turn/start", 2, t + 1_000, json!({"turn": 1})),
        event("turn/end", 3, t + 2_000, json!({"turn": 1, "reason": {"kind": "completed"}})),
    ];
    write_log(&dir.join("session.v4.jsonl.zstd"), &lines, 4);

    let config = load(&env, "dsh-clock", config_value(vec![source("dsh", &root)], vec!["dsh"], None));
    let response = recent(&config, json!({"limit": 5}));
    assert_eq!(response.sessions.len(), 1, "{:?}", response.errors);
    let session = &response.sessions[0];
    assert_eq!(session.session_id, INTERACTIVE);
    assert_eq!(session.title, None, "no title event and not a worker");
    assert_eq!(session.session_kind, "interactive");
    assert_eq!(
        session.last_activity_at.as_deref(),
        Some("2026-11-07T09:30:02Z"),
        "no conversation event: fall back to the newest event of any type"
    );
    assert!(session.tail.is_empty());

    // A search of the bookkeeping text finds nothing.
    let search = search_value(&config, json!({"query": "boot"}));
    assert!(search["results"].as_array().unwrap().is_empty(), "{search}");
}

#[test]
fn missing_root_is_unavailable_not_empty() {
    let env = support::TestEnv::new();
    let missing = env.path().join("no-dsh-here");
    let config = load(&env, "dsh-missing", config_value(vec![source("dsh", &missing)], vec!["dsh"], None));
    let response = search_value(&config, json!({"query": "dshneedle"}));
    assert_eq!(status_of(&response, "dsh"), json!("unavailable"));
    assert_eq!(code_of(&response, "dsh"), json!(codes::SOURCE_ROOT_INVALID));
    assert_eq!(detail_of(&response, "dsh")["reason"], json!("root_unreadable"));

    let recent = recent(&config, json!({"limit": 5}));
    assert_eq!(recent.source_status[0].status, status::UNAVAILABLE);
    assert_eq!(recent.source_status[0].code, Some(codes::SOURCE_ROOT_INVALID));
}

// ---------------------------------------------------------------------------
// Reviewer rework: readback context counts logical lines (P2-1)
// ---------------------------------------------------------------------------

/// An assistant message with several text blocks in one logical line.
fn assistant_blocks(id: &str, texts: &[&str]) -> Value {
    let content: Vec<Value> = texts
        .iter()
        .map(|text| json!({"type": "text", "text": text}))
        .collect();
    json!({
        "message": {
            "id": id,
            "role": "assistant",
            "source": {"kind": "model"},
            "content": content
        }
    })
}

#[test]
fn context_before_counts_logical_lines_not_projected_units() {
    let env = support::TestEnv::new();
    let root = env.root("dsh-context-lines");
    let dir = root.join(PROJECT).join(format!("session-{INTERACTIVE}"));
    let t = ts("2026-11-07T09:00:00Z");
    let lines = vec![
        header(4, INTERACTIVE, json!({})),
        event("user/message", 1, t + 1_000, user_v4("ctxfirst-only")),
        event(
            "assistant/message",
            2,
            t + 2_000,
            assistant_blocks("m-dual", &["ctxdual-block-A", "ctxdual-block-B"]),
        ),
        event("user/message", 3, t + 3_000, user_v4("ctxtarget-needle")),
    ];
    write_log(&dir.join("session.v4.jsonl.zstd"), &lines, 1);
    let config = load(
        &env,
        "dsh-context-lines",
        config_value(vec![source("dsh", &root)], vec!["dsh"], None),
    );

    let response = search_value(&config, json!({"query": "ctxtarget-needle", "limit": 5}));
    let hit = response["results"]
        .as_array()
        .expect("results")
        .iter()
        .find(|hit| {
            hit["snippet"]
                .as_str()
                .unwrap_or_default()
                .contains("ctxtarget-needle")
        })
        .expect("target hit")
        .clone();
    assert_eq!(hit["ref"]["locator"]["record_index"], json!(3));

    let read_value = serde_json::to_value(memory_read(
        &config,
        &ReadRequest::parse(&json!({
            "ref": hit["ref"].clone(),
            "context_before": 1
        }))
        .expect("read request"),
    ))
    .expect("read response");
    assert_eq!(read_value["ok"], json!(true), "{read_value}");
    let text = read_value["text"].as_str().map(str::to_string).unwrap_or_else(|| {
        read_value["segments"]
            .as_array()
            .expect("segments")
            .iter()
            .filter_map(|segment| segment["text"].as_str())
            .collect::<Vec<_>>()
            .join("")
    });
    assert!(
        text.contains("ctxdual-block-A") && text.contains("ctxdual-block-B"),
        "the whole preceding logical line must be included: {read_value}"
    );
    assert!(
        !text.contains("ctxfirst-only"),
        "context_before=1 must not reach a second preceding line: {read_value}"
    );
    let indexes: Vec<u64> = read_value["segments"]
        .as_array()
        .expect("segments")
        .iter()
        .map(|segment| {
            segment["locator"]["record_index"]
                .as_u64()
                .expect("record_index")
        })
        .collect();
    assert_eq!(indexes, vec![2, 2, 3], "both blocks of line 2 then line 3");

    // context_after mirrors the rule: the next single line is one slot.
    let read_after = serde_json::to_value(memory_read(
        &config,
        &ReadRequest::parse(&json!({
            "ref": hit["ref"].clone(),
            "context_before": 1,
            "context_after": 1
        }))
        .expect("read request"),
    ))
    .expect("read response");
    assert_eq!(read_after["ok"], json!(true), "{read_after}");
}

// ---------------------------------------------------------------------------
// Reviewer rework: bounded streaming decode (P2-2) and header-only reads (P3-3)
// ---------------------------------------------------------------------------

#[test]
fn a_high_ratio_frame_is_capped_while_decoding() {
    use agent_memory::recall::providers::dsh;

    let env = support::TestEnv::new();
    let root = env.root("dsh-huge-frame");
    let dir = root.join(PROJECT).join(format!("session-{INTERACTIVE}"));
    let path = dir.join("session.v4.jsonl.zstd");
    std::fs::create_dir_all(&dir).expect("session dir");
    // One frame whose decompressed payload exceeds the logical ceiling. The
    // filler is highly compressible, so the physical file stays tiny while the
    // decoder would produce > 64 MiB if it were materialized whole.
    let mut text = String::new();
    text.push_str(&header(4, INTERACTIVE, json!({})));
    text.push('\n');
    text.push_str(&"a".repeat(72 * 1024 * 1024));
    text.push('\n');
    let frame = zstd::stream::encode_all(text.as_bytes(), 3).expect("zstd frame");
    std::fs::write(&path, &frame).expect("write carrier");
    assert!(
        (frame.len() as u64) < dsh::DSH_SCAN_LIMIT_BYTES,
        "the physical frame is far below the logical ceiling"
    );

    let log = dsh::open_log(&path, "carrier").expect("bounded decode");
    assert!(log.truncated, "the logical ceiling must be disclosed");
    assert_eq!(log.state, dsh::DshReadState::Corrupt);
    assert_eq!(log.frames, 1);
    assert_eq!(
        log.logical_bytes().len() as u64,
        dsh::DSH_SCAN_LIMIT_BYTES,
        "the retained buffer is exactly the ceiling, never the decoded total"
    );
}

#[test]
fn header_read_touches_only_the_first_frame() {
    use agent_memory::recall::providers::dsh;

    let env = support::TestEnv::new();
    let root = env.root("dsh-header-only");
    let dir = root.join(PROJECT).join(format!("session-{INTERACTIVE}"));
    let path = dir.join("session.v4.jsonl.zstd");
    std::fs::create_dir_all(&dir).expect("session dir");
    let header_frame = frame_of(&[header(4, INTERACTIVE, json!({"agentPreset": "standard"}))]);
    std::fs::write(&path, &header_frame).expect("header frame");
    // Extend the file past the physical read ceiling with a sparse tail: the
    // header reader must not depend on the file length, while a full read still
    // refuses an oversized carrier.
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .expect("open carrier");
    file.set_len(dsh::DSH_PHYSICAL_LIMIT_BYTES + 4096)
        .expect("sparse tail");
    drop(file);

    let header = dsh::read_header(&path, "carrier").expect("header from the first frame");
    assert_eq!(header.session_id, INTERACTIVE);
    assert_eq!(header.version, 4);
    assert!(
        dsh::open_log(&path, "carrier").is_err(),
        "a full read still refuses a carrier above the physical ceiling"
    );
}

#[test]
fn canonical_v1_and_v2_names_are_explicitly_unsupported() {
    let env = support::TestEnv::new();
    let root = env.root("dsh-v1v2");
    let dir = root.join(PROJECT).join(format!("session-{INTERACTIVE}"));
    std::fs::create_dir_all(&dir).expect("session dir");
    // Canonical names this build does not implement: never read with the
    // v0/v1 rules and never guessed from the header shape.
    let legacy_shape = frame_of(&[header(1, INTERACTIVE, json!({}))]);
    std::fs::write(dir.join("session.v1.jsonl.zstd"), &legacy_shape).expect("v1 carrier");
    std::fs::write(dir.join("session.v2.jsonl.zstd"), &legacy_shape).expect("v2 carrier");

    let config = load(
        &env,
        "dsh-v1v2",
        config_value(vec![source("dsh", &root)], vec!["dsh"], None),
    );
    let response = search_value(&config, json!({"query": "anything", "limit": 5}));
    assert_eq!(status_of(&response, "dsh"), json!("unsupported"));
    assert_eq!(code_of(&response, "dsh"), json!(codes::SCHEMA_UNSUPPORTED));
    assert_eq!(
        detail_of(&response, "dsh")["coverage"]["unsupported_sessions"],
        json!(1)
    );
    assert!(response["results"].as_array().expect("results").is_empty());
}
