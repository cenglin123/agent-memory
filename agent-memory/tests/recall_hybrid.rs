//! Full-text L5b: conversation vector cache and hybrid retrieval.
//!
//! Every fixture is synthetic and lives under a per-test temporary root; the
//! embedding backend is the deterministic `fake` one (no model, no network, no
//! subprocess). Cases cover incremental vector reuse, whole-source re-embedding
//! on an identity change, byte-identical rebuild, two-branch fusion with channel
//! labels, session dedupe, filters applied before semantic candidates,
//! explicit semantic degradation, the default-mode switch and MCP read-only
//! behavior.

use agent_memory::recall::config::load_config;
use agent_memory::recall::fulltext::build::index_all;
use agent_memory::recall::fulltext::{index_path, vectors_path};
use agent_memory::recall::search::{memory_search, memory_search_with, SearchRequest};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

mod support;

const CACHE: &str = "cache";

fn embedding(dimension: u64, model: &str) -> Value {
    json!({
        "model_id": model,
        "dimension": dimension,
        "normalization": "l2",
        "backend": "fake"
    })
}

fn v2_config(
    sources: Vec<Value>,
    order: Vec<&str>,
    cache_root: Option<&Path>,
    embedding_spec: Option<Value>,
) -> Value {
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
    if let Some(spec) = embedding_spec {
        value["interpreter"]["embedding"] = spec;
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

fn search(config: &Path, request: Value) -> (i32, Value) {
    let parsed = load_config(config).expect("config loads");
    let request = SearchRequest::parse(&request).expect("request parses");
    let response = memory_search(&parsed, &request);
    (response.exit_code, serde_json::to_value(&response).unwrap())
}

fn search_request(config: &Path, request: Value) -> Value {
    search(config, request).1
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

fn detail_of<'a>(response: &'a Value, source_id: &str) -> &'a Value {
    let entry = response["source_status"]
        .as_array()
        .expect("source_status")
        .iter()
        .find(|entry| entry["source_id"] == json!(source_id))
        .unwrap_or_else(|| panic!("no source {source_id}: {response}"));
    &entry["detail"]
}

fn sessions(response: &Value) -> Vec<String> {
    response["results"]
        .as_array()
        .expect("results")
        .iter()
        .map(|hit| hit["ref"]["locator"]["session_id"].as_str().unwrap_or_default().to_string())
        .collect()
}

fn channels_of(response: &Value, session_id: &str) -> Value {
    response["results"]
        .as_array()
        .expect("results")
        .iter()
        .find(|hit| hit["ref"]["locator"]["session_id"] == json!(session_id))
        .map(|hit| hit["channels"].clone())
        .unwrap_or_else(|| panic!("no hit for {session_id}: {response}"))
}

// ---------------------------------------------------------------------------
// OpenCode V2 fixture
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

struct Oc {
    root: PathBuf,
    db: PathBuf,
}

fn opencode(env: &support::TestEnv, name: &str) -> Oc {
    let root = env.root(name);
    let db = root.join("opencode.db");
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute_batch(OPENCODE_SCHEMA).unwrap();
    drop(conn);
    Oc { root, db }
}

fn add_session(db: &Path, id: &str, title: &str, parent: Option<&str>, created: i64, updated: i64) {
    let conn = rusqlite::Connection::open(db).unwrap();
    conn.execute(
        "INSERT INTO session_v2(id, title, directory, time_created, time_updated, parent_id) \
         VALUES (?1, ?2, 'C:/work', ?3, ?4, ?5)",
        rusqlite::params![id, title, created, updated, parent],
    )
    .unwrap();
}

fn add_message(db: &Path, id: &str, session: &str, seq: i64, ts: i64, kind: &str, data: Value) {
    let conn = rusqlite::Connection::open(db).unwrap();
    conn.execute(
        "INSERT INTO session_message(id, session_id, seq, type, time_created, time_updated, data) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?5, ?6)",
        rusqlite::params![id, session, seq, kind, ts, serde_json::to_string(&data).unwrap()],
    )
    .unwrap();
}

fn add_text(db: &Path, id: &str, session: &str, seq: i64, ts: i64, kind: &str, text: &str) {
    add_message(db, id, session, seq, ts, kind, json!({"text": text}));
}

fn add_tool_output(db: &Path, id: &str, session: &str, seq: i64, ts: i64, output: &str) {
    add_message(
        db,
        id,
        session,
        seq,
        ts,
        "assistant",
        json!({"content": [{
            "type": "tool",
            "tool": "bash",
            "state": {"input": {"command": "dump"}, "output": output}
        }]}),
    );
}

/// The shared three-session fixture:
/// * `s-both` — conversation text that both branches find;
/// * `s-sem` — conversation text only the semantic branch finds (no long term);
/// * `s-lex` — a tool output only the lexical branch indexes.
fn channels_fixture(env: &support::TestEnv) -> (Oc, PathBuf) {
    let oc = opencode(env, "oc");
    add_session(&oc.db, "s-both", "both branches", None, 1_000, 9_000);
    add_session(&oc.db, "s-sem", "semantic only", None, 900, 8_000);
    add_session(&oc.db, "s-lex", "lexical only", None, 800, 7_000);
    add_text(
        &oc.db,
        "m-both",
        "s-both",
        1,
        1_700_000_000_000,
        "user",
        "缓存失效 py3",
    );
    add_text(
        &oc.db,
        "m-sem",
        "s-sem",
        1,
        1_700_000_001_000,
        "user",
        "缓存失效",
    );
    add_tool_output(
        &oc.db,
        "m-lex",
        "s-lex",
        1,
        1_700_000_002_000,
        "py3 runtime dump marker",
    );
    let config = env.write_json(
        "hybrid-config.json",
        &v2_config(
            vec![source("oc", "opencode", &oc.root)],
            vec!["oc"],
            Some(&env.root(CACHE)),
            Some(embedding(32, "fake-hash-v1")),
        ),
    );
    (oc, config)
}

fn hybrid_request(query: &str) -> Value {
    json!({"query": query, "mode": "hybrid", "sources": ["oc"], "limit": 20})
}

// ---------------------------------------------------------------------------
// A. Vector cache
// ---------------------------------------------------------------------------

#[test]
fn vectors_are_reused_incrementally_and_reembedded_on_model_change() {
    let env = support::TestEnv::new();
    let cache = env.root(CACHE);
    let oc = opencode(&env, "oc");
    add_session(&oc.db, "s1", "conversation", None, 1_000, 9_000);
    add_text(&oc.db, "m1", "s1", 1, 1_700_000_000_000, "user", "第一个问题");
    add_text(&oc.db, "m2", "s1", 2, 1_700_000_001_000, "assistant", "第一个回答");
    add_tool_output(&oc.db, "m3", "s1", 3, 1_700_000_002_000, "tool output text");
    let config_path = env.write_json(
        "vectors-config.json",
        &v2_config(
            vec![source("oc", "opencode", &oc.root)],
            vec!["oc"],
            Some(&cache),
            Some(embedding(32, "fake-hash-v1")),
        ),
    );
    let config = load_config(&config_path).unwrap();

    let first = index_all(&config, None, false).unwrap();
    let report = &first[0];
    assert_eq!(report.status, "built");
    let vectors = &report.detail.as_ref().unwrap()["vectors"];
    assert_eq!(vectors["status"], json!("built"), "{vectors}");
    // Only the two conversation text units are embedded; the tool output is not.
    assert_eq!(vectors["units"], json!(2), "{vectors}");
    assert_eq!(vectors["embedded"], json!(2), "{vectors}");
    assert_eq!(vectors["reused"], json!(0), "{vectors}");
    let path = vectors_path(&cache, "oc");
    assert!(path.is_file(), "{}", path.display());
    let first_bytes = std::fs::read(&path).unwrap();

    // Nothing changed: the whole cache is reused and the file is not rewritten.
    let second = index_all(&config, None, false).unwrap();
    let vectors = &second[0].detail.as_ref().unwrap()["vectors"];
    assert_eq!(vectors["status"], json!("unchanged"), "{vectors}");
    assert_eq!(vectors["reused"], json!(2), "{vectors}");
    assert_eq!(vectors["embedded"], json!(0), "{vectors}");
    assert_eq!(std::fs::read(&path).unwrap(), first_bytes);

    // One new unit: only that text is embedded.
    add_text(&oc.db, "m4", "s1", 4, 1_700_000_003_000, "user", "第二个问题");
    let third = index_all(&config, None, false).unwrap();
    let vectors = &third[0].detail.as_ref().unwrap()["vectors"];
    assert_eq!(vectors["status"], json!("updated"), "{vectors}");
    assert_eq!(vectors["units"], json!(3), "{vectors}");
    assert_eq!(vectors["reused"], json!(2), "{vectors}");
    assert_eq!(vectors["embedded"], json!(1), "{vectors}");

    // A different model is a whole-source re-embed: nothing may be mixed.
    let changed = env.write_json(
        "vectors-config-changed.json",
        &v2_config(
            vec![source("oc", "opencode", &oc.root)],
            vec!["oc"],
            Some(&cache),
            Some(embedding(32, "other-hash-v2")),
        ),
    );
    let changed_config = load_config(&changed).unwrap();
    let fourth = index_all(&changed_config, None, false).unwrap();
    let vectors = &fourth[0].detail.as_ref().unwrap()["vectors"];
    assert_eq!(vectors["status"], json!("built"), "{vectors}");
    assert_eq!(vectors["reused"], json!(0), "{vectors}");
    assert_eq!(vectors["embedded"], json!(3), "{vectors}");
    assert!(
        vectors["generation_key"]
            .as_str()
            .unwrap()
            .contains("model=other-hash-v2"),
        "{vectors}"
    );
}

#[test]
fn deleting_the_vector_cache_and_rebuilding_gives_identical_bytes() {
    let env = support::TestEnv::new();
    let cache = env.root(CACHE);
    let (_oc, config_path) = channels_fixture(&env);
    let config = load_config(&config_path).unwrap();
    index_all(&config, None, false).unwrap();
    let path = vectors_path(&cache, "oc");
    let first = std::fs::read(&path).unwrap();
    assert!(!first.is_empty());

    std::fs::remove_file(&path).unwrap();
    assert!(!path.exists());
    let again = index_all(&config, None, false).unwrap();
    let vectors = &again[0].detail.as_ref().unwrap()["vectors"];
    assert_eq!(vectors["status"], json!("built"), "{vectors}");
    assert_eq!(std::fs::read(&path).unwrap(), first, "rebuild must byte-match");

    // The full-text index is rebuilt identically too.
    assert!(index_path(&cache, "oc").is_file());
}

#[test]
fn an_unavailable_model_degrades_only_the_vector_report() {
    let env = support::TestEnv::new();
    let cache = env.root(CACHE);
    let (_oc, _) = channels_fixture(&env);
    let mut spec = embedding(32, "bge-small-zh-v1.5");
    spec["backend"] = json!("python");
    let config_path = env.write_json(
        "python-config.json",
        &v2_config(
            vec![source("oc", "opencode", &env.root("oc"))],
            vec!["oc"],
            Some(&cache),
            Some(spec),
        ),
    );
    let config = load_config(&config_path).unwrap();
    let reports = index_all(&config, None, false).unwrap();
    let report = &reports[0];
    assert_eq!(report.status, "built", "{report:?}");
    assert!(report.rows > 0, "{report:?}");
    let vectors = &report.detail.as_ref().unwrap()["vectors"];
    assert_eq!(vectors["status"], json!("degraded"), "{vectors}");
    let reason = vectors["reason"].as_str().unwrap();
    assert!(
        reason.contains("unavailable"),
        "expected an explicit unavailability reason: {reason}"
    );
    assert!(!vectors_path(&cache, "oc").exists(), "no partial cache is written");

    // The CLI still exits 0: the full-text index succeeded.
    let output = std::process::Command::new(bin())
        .arg("index")
        .arg("--config")
        .arg(&config_path)
        .output()
        .expect("run index");
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["ok"], json!(true));
    assert_eq!(
        report["sources"][0]["detail"]["vectors"]["status"],
        json!("degraded")
    );
}

// ---------------------------------------------------------------------------
// A2. Contextual embedding input (L5b rework)
// ---------------------------------------------------------------------------

fn rename_session(db: &Path, session: &str, title: &str) {
    let conn = rusqlite::Connection::open(db).unwrap();
    conn.execute(
        "UPDATE session_v2 SET title = ?2 WHERE id = ?1",
        rusqlite::params![session, title],
    )
    .unwrap();
}

/// Rewrite one message and move its clock, so the incremental watermark re-reads
/// it (an in-place edit with an unchanged clock is the documented blind spot).
fn edit_text(db: &Path, message: &str, text: &str) {
    let conn = rusqlite::Connection::open(db).unwrap();
    conn.execute(
        "UPDATE session_message SET data = ?2, time_updated = time_updated + 1000000          WHERE id = ?1",
        rusqlite::params![message, serde_json::to_string(&json!({"text": text})).unwrap()],
    )
    .unwrap();
}

#[test]
fn the_embedding_input_carries_the_title_and_the_preceding_user_turn() {
    let env = support::TestEnv::new();
    let cache = env.root(CACHE);
    let oc = opencode(&env, "oc");
    add_session(&oc.db, "s1", "会话标题一", None, 1_000, 9_000);
    add_text(&oc.db, "u1", "s1", 1, 1_700_000_000_000, "user", "第一问");
    add_text(&oc.db, "a1", "s1", 2, 1_700_000_001_000, "assistant", "第一答");
    add_text(&oc.db, "a2", "s1", 3, 1_700_000_002_000, "assistant", "第二答");
    let config_path = env.write_json(
        "ctx-config.json",
        &v2_config(
            vec![source("oc", "opencode", &oc.root)],
            vec!["oc"],
            Some(&cache),
            Some(embedding(32, "fake-hash-v1")),
        ),
    );
    let config = load_config(&config_path).unwrap();
    let path = vectors_path(&cache, "oc");

    index_all(&config, None, false).unwrap();
    let baseline = std::fs::read(&path).unwrap();
    let vectors = &index_all(&config, None, false).unwrap()[0]
        .detail
        .as_ref()
        .unwrap()["vectors"]
        .clone();
    // Stable context ⇒ nothing is re-embedded and the bytes do not move.
    assert_eq!(vectors["reused"], json!(3), "{vectors}");
    assert_eq!(std::fs::read(&path).unwrap(), baseline);

    // Renaming the session changes the input of *every* unit in it (the title
    // leads the input), so the whole session re-embeds.
    rename_session(&oc.db, "s1", "会话标题二");
    let renamed = index_all(&config, None, false).unwrap();
    let vectors = &renamed[0].detail.as_ref().unwrap()["vectors"];
    assert_eq!(vectors["reused"], json!(0), "{vectors}");
    assert_eq!(vectors["embedded"], json!(3), "{vectors}");
    let after_rename = std::fs::read(&path).unwrap();
    assert_ne!(after_rename, baseline, "the title must enter the vector");

    // Editing the user turn changes that unit *and* the assistant turns that
    // carry it as context: three re-embeds, not one.
    edit_text(&oc.db, "u1", "第一问（改写）");
    let edited = index_all(&config, None, false).unwrap();
    let vectors = &edited[0].detail.as_ref().unwrap()["vectors"];
    assert_eq!(vectors["reused"], json!(0), "{vectors}");
    assert_eq!(
        vectors["embedded"],
        json!(3),
        "assistant turns carry the preceding user turn: {vectors}"
    );

    // Editing an assistant body re-embeds only that unit.
    let before = std::fs::read(&path).unwrap();
    edit_text(&oc.db, "a2", "第二答（改写）");
    let edited = index_all(&config, None, false).unwrap();
    let vectors = &edited[0].detail.as_ref().unwrap()["vectors"];
    assert_eq!(vectors["reused"], json!(2), "{vectors}");
    assert_eq!(vectors["embedded"], json!(1), "{vectors}");
    assert_ne!(std::fs::read(&path).unwrap(), before);
}

// ---------------------------------------------------------------------------
// A3. Projector version and coverage (L5b rework)
// ---------------------------------------------------------------------------

fn tamper_projector_version(index: &Path, value: &str) {
    let conn = rusqlite::Connection::open(index).unwrap();
    conn.execute(
        "UPDATE meta SET value = ?2 WHERE key = ?1",
        rusqlite::params!["projector_version", value],
    )
    .unwrap();
}

#[test]
fn a_newer_projector_version_rebuilds_the_index_and_marks_queries_stale() {
    let env = support::TestEnv::new();
    let cache = env.root(CACHE);
    let (_oc, config_path) = channels_fixture(&env);
    let config = load_config(&config_path).unwrap();
    index_all(&config, None, false).unwrap();
    let index = index_path(&cache, "oc");

    // An index from older projection rules is stale, not merely old: the query
    // says so and falls back, and the coverage block is still there.
    tamper_projector_version(&index, "projector-1");
    let response = search_request(&config_path, hybrid_request("缓存 失效 py3"));
    let detail = &detail_of(&response, "oc");
    assert_eq!(status_of(&response, "oc"), "degraded", "{response}");
    assert_eq!(detail["reason"], json!("INDEX_STALE"), "{detail}");
    assert_eq!(
        detail["hybrid"]["cause"],
        json!("projector_version_changed"),
        "{detail}"
    );
    assert!(detail["coverage"]["rows"].as_u64().unwrap() > 0, "{detail}");
    let exact = search_request(
        &config_path,
        json!({"query": "py3", "mode": "exact", "sources": ["oc"]}),
    );
    assert_eq!(
        detail_of(&exact, "oc")["exact"]["cause"],
        json!("projector_version_changed")
    );

    // On a fallback the block merges the scan's own coverage with the index
    // snapshot instead of replacing it: the answer came from the scan.
    let filtered = search_request(
        &config_path,
        json!({"query": "py3", "mode": "exact", "sources": ["oc"],
               "session_id": "s-both"}),
    );
    let coverage = &detail_of(&filtered, "oc")["coverage"];
    assert_eq!(
        coverage["session_filter"],
        json!("s-both"),
        "scan coverage must survive: {coverage}"
    );
    assert!(coverage["total_sessions"].as_u64().is_some(), "{coverage}");
    assert!(coverage["rows"].as_u64().unwrap() > 0, "{coverage}");

    // A default-mode request that needs the index reports INDEX_REQUIRED and
    // keeps the precise cause next to it.
    let default_mode = search_request(
        &config_path,
        json!({"query": "缓存失效 py3", "sources": ["oc"], "roles": ["user"]}),
    );
    let detail = detail_of(&default_mode, "oc");
    assert_eq!(detail["reason"], json!("INDEX_REQUIRED"), "{detail}");
    assert_eq!(detail["resolved_mode"], json!("hybrid"), "{detail}");
    assert_eq!(
        detail["hybrid"]["cause"],
        json!("projector_version_changed"),
        "{detail}"
    );
    assert_eq!(status_of(&default_mode, "oc"), "degraded");

    // `index` rebuilds and names the reason.
    let report = index_all(&config, None, false).unwrap();
    assert_eq!(report[0].status, "built", "{:?}", report[0]);
    assert!(report[0].rebuilt);
    assert_eq!(
        report[0].reason.as_deref(),
        Some("projector_version_changed"),
        "{:?}",
        report[0]
    );
    // The vector cache follows the *units*, not the index file: the projector
    // rebuild produced the same conversation units, so the vectors are reused
    // instead of being re-embedded for nothing.
    let vectors = &report[0].detail.as_ref().unwrap()["vectors"];
    assert_eq!(vectors["status"], json!("unchanged"), "{vectors}");
    assert_eq!(vectors["reused"], json!(2), "{vectors}");
    assert_eq!(vectors["projector_version"], json!("projector-2"));

    // A rebuilt index answers again.
    let after = search_request(&config_path, hybrid_request("缓存 失效 py3"));
    assert_eq!(status_of(&after, "oc"), "ok", "{after}");
    assert!(detail_of(&after, "oc")["hybrid"]["semantic"] == json!("available"));
}

#[test]
fn the_vector_cache_reports_a_projector_mismatch_on_its_own() {
    let env = support::TestEnv::new();
    let cache = env.root(CACHE);
    let (_oc, config_path) = channels_fixture(&env);
    let config = load_config(&config_path).unwrap();
    index_all(&config, None, false).unwrap();

    // Rewrite the projector string in the vectors header in place (same length,
    // so the file stays a valid cache with a different projection generation).
    let path = vectors_path(&cache, "oc");
    let bytes = std::fs::read(&path).unwrap();
    let marker = b"projector-2";
    let position = bytes
        .windows(marker.len())
        .position(|window| window == marker)
        .expect("projector version in the header");
    let mut tampered = bytes.clone();
    tampered[position + marker.len() - 1] = b'9';
    std::fs::write(&path, &tampered).unwrap();

    let response = search_request(&config_path, hybrid_request("缓存 失效 py3"));
    let detail = &detail_of(&response, "oc");
    assert_eq!(detail["hybrid"]["semantic"], json!("degraded"), "{detail}");
    assert_eq!(
        detail["hybrid"]["reason"],
        json!("projector_version_changed"),
        "{detail}"
    );
    assert_eq!(status_of(&response, "oc"), "ok");
    assert!(!response["results"].as_array().unwrap().is_empty());
    // Coverage cannot vouch for a cache from another projection generation.
    assert_eq!(detail["coverage"]["vectors"], Value::Null, "{detail}");

    // The next `index` re-embeds from scratch.
    let report = index_all(&config, None, false).unwrap();
    let vectors = &report[0].detail.as_ref().unwrap()["vectors"];
    assert_eq!(vectors["status"], json!("built"), "{vectors}");
    assert_eq!(vectors["reused"], json!(0), "{vectors}");
    assert_eq!(std::fs::read(&path).unwrap(), bytes, "back to the same cache");
}

#[test]
fn coverage_reports_the_index_snapshot_and_marks_a_lagging_source() {
    let env = support::TestEnv::new();
    let (oc, config_path) = channels_fixture(&env);
    let config = load_config(&config_path).unwrap();
    index_all(&config, None, false).unwrap();

    let response = search_request(&config_path, hybrid_request("缓存 失效 py3"));
    let coverage = &detail_of(&response, "oc")["coverage"];
    assert!(coverage["indexed_at"].as_str().is_some(), "{coverage}");
    assert!(coverage["newest_unit_ts"].as_str().is_some(), "{coverage}");
    assert_eq!(coverage["rows"], json!(4), "{coverage}");
    assert_eq!(coverage["sessions"], json!(3), "{coverage}");
    // Two conversation units are embedded; the tool output is not.
    assert_eq!(coverage["vectors"], json!(2), "{coverage}");
    assert!(coverage.get("stale_since").is_none(), "{coverage}");

    // A source write that the last index has not seen yet. The freshness check
    // is the host file stamp, so the label is when the file changed, not a unit
    // time.
    add_text(
        &oc.db,
        "m-new",
        "s-both",
        9,
        1_700_000_009_000,
        "user",
        "索引之后的新消息",
    );
    let stale = search_request(&config_path, hybrid_request("缓存 失效 py3"));
    let coverage = &detail_of(&stale, "oc")["coverage"];
    assert!(
        coverage["stale_since"].as_str().is_some(),
        "a write after the last index makes the source stale: {coverage}"
    );
    assert_eq!(status_of(&stale, "oc"), "ok", "coverage never changes status");

    // Re-indexing clears it.
    index_all(&config, None, false).unwrap();
    let fresh = search_request(&config_path, hybrid_request("缓存 失效 py3"));
    assert!(detail_of(&fresh, "oc")["coverage"]
        .get("stale_since")
        .is_none());
    assert_eq!(detail_of(&fresh, "oc")["coverage"]["rows"], json!(5));
}

#[test]
fn coverage_marks_a_jsonl_source_whose_file_grew() {
    let env = support::TestEnv::new();
    let cache = env.root(CACHE);
    let root = env.root("claude-cov");
    let project = root.join("proj");
    std::fs::create_dir_all(&project).unwrap();
    let session = "77777777-7777-4777-8777-777777777777";
    let path = project.join(format!("{session}.jsonl"));
    let line = |uuid: &str, text: &str| {
        json!({
            "type": "user",
            "message": {"role": "user", "content": text},
            "timestamp": "2026-10-25T13:25:00Z",
            "sessionId": session,
            "uuid": uuid
        })
        .to_string()
    };
    std::fs::write(&path, format!("{}\n", line("u1", "初始记录"))).unwrap();
    let config_path = env.write_json(
        "coverage-jsonl.json",
        &v2_config(
            vec![source("cc", "claude_code", &root)],
            vec!["cc"],
            Some(&cache),
            None,
        ),
    );
    let config = load_config(&config_path).unwrap();
    index_all(&config, None, false).unwrap();

    let request = json!({"query": "初始记录", "mode": "exact", "sources": ["cc"]});
    let response = search_request(&config_path, request.clone());
    let coverage = &detail_of(&response, "cc")["coverage"];
    assert!(coverage.get("stale_since").is_none(), "{coverage}");
    assert_eq!(coverage["vectors"], Value::Null, "no embedding configured");

    std::fs::write(
        &path,
        format!("{}\n{}\n", line("u1", "初始记录"), line("u2", "追加记录")),
    )
    .unwrap();
    let stale = search_request(&config_path, request);
    let coverage = &detail_of(&stale, "cc")["coverage"];
    assert!(coverage["stale_since"].as_str().is_some(), "{coverage}");
}

// ---------------------------------------------------------------------------
// B. Hybrid retrieval
// ---------------------------------------------------------------------------

#[test]
fn hybrid_fuses_both_branches_and_labels_channels() {
    let env = support::TestEnv::new();
    let (_oc, config_path) = channels_fixture(&env);
    let config = load_config(&config_path).unwrap();
    index_all(&config, None, false).unwrap();

    let response = search_request(&config_path, hybrid_request("缓存 失效 py3"));
    assert_eq!(status_of(&response, "oc"), "ok", "{response}");
    let detail = &detail_of(&response, "oc")["hybrid"];
    assert_eq!(detail["mode"], json!("hybrid"));
    assert_eq!(detail["semantic"], json!("available"), "{detail}");
    assert_eq!(detail["thresholdsVersion"], json!("archive-relevance-v4"));
    assert_eq!(detail["fusion"], json!("rrf"));
    assert_eq!(detail["rrf_k"], json!(60));

    // `s-both` is in both branches; `s-sem` only in the semantic one; `s-lex`
    // only in the lexical one (a tool output is never embedded).
    assert_eq!(channels_of(&response, "s-both"), json!(["lexical", "semantic"]));
    assert_eq!(channels_of(&response, "s-sem"), json!(["semantic"]));
    assert_eq!(channels_of(&response, "s-lex"), json!(["lexical"]));
    // A hit found by both branches outranks a single-branch hit.
    assert_eq!(sessions(&response).first().unwrap(), "s-both");

    let first = &response["results"][0];
    assert_eq!(first["score_kind"], json!("rrf"));
    assert_eq!(first["rank_basis"], json!("rrf_lexical_semantic"));
    assert!(first["score"].as_f64().unwrap() > 0.0);
    assert_eq!(first["session_hits"], json!(1));
    assert_eq!(first["kind"], json!("text"));
    assert!(first["timestamp"].as_str().is_some());
}

#[test]
fn hybrid_dedupes_by_session_and_counts_its_candidates() {
    let env = support::TestEnv::new();
    let (oc, config_path) = channels_fixture(&env);
    let config = load_config(&config_path).unwrap();
    index_all(&config, None, false).unwrap();

    // A second matching turn in `s-both`.
    add_text(
        &oc.db,
        "m-both-2",
        "s-both",
        2,
        1_700_000_003_000,
        "assistant",
        "缓存失效 py3 的进一步说明",
    );
    index_all(&config, None, false).unwrap();

    let response = search_request(&config_path, hybrid_request("缓存 失效 py3"));
    let session_ids = sessions(&response);
    let mut unique = session_ids.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), session_ids.len(), "one hit per session: {session_ids:?}");
    let both = response["results"]
        .as_array()
        .unwrap()
        .iter()
        .find(|hit| hit["ref"]["locator"]["session_id"] == json!("s-both"))
        .unwrap();
    assert_eq!(both["session_hits"], json!(2), "{both}");
}

#[test]
fn hybrid_applies_filters_before_semantic_candidates() {
    let env = support::TestEnv::new();
    let cache = env.root(CACHE);
    let oc = opencode(&env, "oc");
    add_session(&oc.db, "s-main", "main", None, 1_000, 9_000);
    // A worker session holding the exact query text: it is the most similar
    // unit in the corpus, so only a filter applied before candidate generation
    // can keep it out.
    add_session(&oc.db, "s-worker", "ocsr-dispatch", None, 900, 8_000);
    add_text(
        &oc.db,
        "m-main",
        "s-main",
        1,
        1_700_000_000_000,
        "user",
        "缓存 失效 py3 的处理记录",
    );
    add_text(
        &oc.db,
        "m-worker",
        "s-worker",
        1,
        1_700_000_001_000,
        "assistant",
        "缓存 失效 py3",
    );
    let config_path = env.write_json(
        "filter-config.json",
        &v2_config(
            vec![source("oc", "opencode", &oc.root)],
            vec!["oc"],
            Some(&cache),
            Some(embedding(32, "fake-hash-v1")),
        ),
    );
    let config = load_config(&config_path).unwrap();
    index_all(&config, None, false).unwrap();

    let all = search_request(&config_path, hybrid_request("缓存 失效 py3"));
    assert_eq!(
        sessions(&all).first().unwrap(),
        "s-worker",
        "the worker is the most similar by construction: {all}"
    );

    let interactive = search_request(
        &config_path,
        json!({
            "query": "缓存 失效 py3", "mode": "hybrid", "sources": ["oc"],
            "session_kinds": ["interactive"], "limit": 20
        }),
    );
    assert!(!sessions(&interactive).contains(&"s-worker".to_string()), "{interactive}");
    assert!(sessions(&interactive).contains(&"s-main".to_string()), "{interactive}");

    let scoped = search_request(
        &config_path,
        json!({
            "query": "缓存 失效 py3", "mode": "hybrid", "sources": ["oc"],
            "session_id": "s-main", "limit": 20
        }),
    );
    assert_eq!(sessions(&scoped), vec!["s-main".to_string()], "{scoped}");

    // A role filter removes the assistant turn from the semantic branch too.
    let user_only = search_request(
        &config_path,
        json!({
            "query": "缓存 失效 py3", "mode": "hybrid", "sources": ["oc"],
            "roles": ["user"], "limit": 20
        }),
    );
    assert!(!sessions(&user_only).contains(&"s-worker".to_string()), "{user_only}");
}

#[test]
fn hybrid_degrades_to_the_lexical_branch_explicitly() {
    let env = support::TestEnv::new();
    let cache = env.root(CACHE);
    let (_oc, config_path) = channels_fixture(&env);
    let config = load_config(&config_path).unwrap();
    index_all(&config, None, false).unwrap();

    // (a) A cache without vectors: the configured model is gone.
    let no_embedding = env.write_json(
        "no-embedding.json",
        &v2_config(
            vec![source("oc", "opencode", &env.root("oc"))],
            vec!["oc"],
            Some(&cache),
            None,
        ),
    );
    let response = search_request(&no_embedding, hybrid_request("缓存 失效 py3"));
    let detail = &detail_of(&response, "oc")["hybrid"];
    assert_eq!(detail["semantic"], json!("degraded"), "{detail}");
    assert_eq!(detail["reason"], json!("embedding_not_configured"));
    assert_eq!(status_of(&response, "oc"), "ok", "{response}");
    assert!(!response["results"].as_array().unwrap().is_empty());
    for hit in response["results"].as_array().unwrap() {
        assert_eq!(hit["channels"], json!(["lexical"]), "{hit}");
    }

    // (b) A corrupt cache file is reported, not partially read.
    let path = vectors_path(&cache, "oc");
    std::fs::write(&path, b"not a vector cache").unwrap();
    let response = search_request(&config_path, hybrid_request("缓存 失效 py3"));
    let detail = &detail_of(&response, "oc")["hybrid"];
    assert_eq!(detail["semantic"], json!("degraded"), "{detail}");
    assert_eq!(detail["reason"], json!("vectors_corrupt"));
    assert!(!response["results"].as_array().unwrap().is_empty());

    // (c) A trimmed cache is rejected as corrupt as well.
    let good = {
        index_all(&config, None, false).unwrap();
        std::fs::read(&path).unwrap()
    };
    std::fs::write(&path, &good[..good.len() - 1]).unwrap();
    let response = search_request(&config_path, hybrid_request("缓存 失效 py3"));
    assert_eq!(detail_of(&response, "oc")["hybrid"]["reason"], json!("vectors_corrupt"));
}

#[test]
fn default_mode_switches_to_hybrid_only_when_a_cache_exists() {
    let env = support::TestEnv::new();
    let (_oc, config_path) = channels_fixture(&env);
    let parsed = load_config(&config_path).unwrap();
    let query = json!({"query": "缓存 失效 py3", "sources": ["oc"], "limit": 20});

    let before = serialized(&parsed, &query);
    let legacy_explicit = serialized(&parsed, &json!({
        "query": "缓存 失效 py3", "sources": ["oc"], "limit": 20, "mode": "legacy"
    }));
    assert_eq!(before, legacy_explicit, "without an index auto is the legacy scan");

    index_all(&parsed, None, false).unwrap();
    let after = serialized(&parsed, &query);
    assert_ne!(before, after, "with an index auto must change path");
    let response = search_request(&config_path, query.clone());
    assert!(
        detail_of(&response, "oc").get("hybrid").is_some(),
        "auto must report the hybrid block: {response}"
    );
    assert!(response["results"][0].get("channels").is_some());

    // `legacy` keeps answering exactly as before the index existed.
    let legacy_after = serialized(&parsed, &json!({
        "query": "缓存 失效 py3", "sources": ["oc"], "limit": 20, "mode": "legacy"
    }));
    assert_eq!(before, legacy_after);
    // `exact` stays the pure full-text path: no channels, its own block.
    let exact = search_request(&config_path, json!({
        "query": "py3", "mode": "exact", "sources": ["oc"], "limit": 20
    }));
    assert!(exact["results"][0].get("channels").is_none(), "{exact}");
    assert!(detail_of(&exact, "oc").get("exact").is_some());
    assert!(detail_of(&exact, "oc").get("hybrid").is_none());

    // Filter keys are shared by exact and hybrid; order/group_by_session are not.
    let error = SearchRequest::parse(&json!({
        "query": "x", "mode": "hybrid", "order": "newest"
    }))
    .unwrap_err();
    assert_eq!(error.detail.unwrap()["reason"], "hybrid_fixed_order");
    let error = SearchRequest::parse(&json!({
        "query": "x", "mode": "legacy", "roles": ["user"]
    }))
    .unwrap_err();
    assert_eq!(error.detail.unwrap()["path"], "roles");
    let error =
        SearchRequest::parse(&json!({"query": "x", "mode": "hybrid", "since": "yesterday"}))
            .unwrap_err();
    assert_eq!(error.detail.unwrap()["reason"], "invalid_rfc3339");
}

fn serialized(config: &agent_memory::recall::RecallConfig, request: &Value) -> String {
    let request = SearchRequest::parse(request).expect("request parses");
    serde_json::to_string(&memory_search(config, &request)).unwrap()
}

#[test]
fn strict_snapshot_answers_from_the_index_like_exact_mode() {
    let env = support::TestEnv::new();
    let (_oc, config_path) = channels_fixture(&env);
    let config = load_config(&config_path).unwrap();
    index_all(&config, None, false).unwrap();

    // The index is keyed by the configured source identity, exactly like exact
    // mode: `--strict-snapshot` changes the scan path, not which cache answers.
    let request = SearchRequest::parse(&hybrid_request("缓存 失效 py3")).unwrap();
    let response = memory_search_with(
        &config,
        &request,
        &agent_memory::recall::search::SearchOptions {
            strict_snapshot: true,
            eval_freeze_decay: false,
        },
    );
    let value = serde_json::to_value(&response).unwrap();
    assert_eq!(value["source_status"][0]["status"], json!("ok"), "{value}");
    assert_eq!(
        value["source_status"][0]["detail"]["hybrid"]["semantic"],
        json!("available"),
        "{value}"
    );
    assert!(!value["results"].as_array().unwrap().is_empty(), "{value}");

    let exact = SearchRequest::parse(&json!({
        "query": "py3", "mode": "exact", "sources": ["oc"]
    }))
    .unwrap();
    let exact_response = memory_search_with(
        &config,
        &exact,
        &agent_memory::recall::search::SearchOptions {
            strict_snapshot: true,
            eval_freeze_decay: false,
        },
    );
    let exact_value = serde_json::to_value(&exact_response).unwrap();
    assert_eq!(
        exact_value["source_status"][0]["status"],
        json!("ok"),
        "exact keeps its index answer under strict snapshot: {exact_value}"
    );
}

// ---------------------------------------------------------------------------
// D. Default mode: filters, ordering and CJK terms (L5b rework 2)
// ---------------------------------------------------------------------------

#[test]
fn the_default_mode_accepts_filters_and_uses_hybrid() {
    let env = support::TestEnv::new();
    let (_oc, config_path) = channels_fixture(&env);
    let config = load_config(&config_path).unwrap();
    index_all(&config, None, false).unwrap();

    // No `mode`, but a time filter: the most natural agent request.
    let response = search_request(
        &config_path,
        json!({
            "query": "缓存失效 py3", "sources": ["oc"], "limit": 20,
            "since": "2023-11-14T22:13:21Z"
        }),
    );
    let detail = detail_of(&response, "oc");
    assert_eq!(detail["resolved_mode"], json!("hybrid"), "{detail}");
    assert_eq!(status_of(&response, "oc"), "ok", "{response}");
    assert!(detail.get("hybrid").is_some(), "{detail}");
    // `s-both` is at 22:13:20 and is filtered out before candidates exist; the
    // same request without the filter does contain it.
    let unfiltered = search_request(
        &config_path,
        json!({"query": "缓存失效 py3", "sources": ["oc"], "limit": 20}),
    );
    assert!(sessions(&unfiltered).contains(&"s-both".to_string()), "{unfiltered}");
    let found = sessions(&response);
    assert!(!found.contains(&"s-both".to_string()), "{found:?}");
    assert!(found.contains(&"s-sem".to_string()), "{found:?}");
}

#[test]
fn the_default_mode_degrades_only_the_source_without_an_index() {
    let env = support::TestEnv::new();
    let cache = env.root(CACHE);
    let (oc, _) = channels_fixture(&env);
    let device = env.root("device");
    std::fs::write(device.join("2026-10-25_note.md"), "缓存失效 py3 的设备笔记\n").unwrap();
    let config_path = env.write_json(
        "two-source.json",
        &v2_config(
            vec![
                source("oc", "opencode", &oc.root),
                source("dev", "device", &device),
            ],
            vec!["oc", "dev"],
            Some(&cache),
            Some(embedding(32, "fake-hash-v1")),
        ),
    );
    let config = load_config(&config_path).unwrap();
    // Only the OpenCode source is indexed.
    index_all(&config, Some("oc"), false).unwrap();

    let response = search_request(
        &config_path,
        json!({"query": "缓存失效 py3", "sources": ["oc", "dev"], "limit": 20,
               "roles": ["user"]}),
    );
    assert_eq!(status_of(&response, "oc"), "ok", "{response}");
    assert_eq!(detail_of(&response, "oc")["resolved_mode"], json!("hybrid"));
    assert_eq!(status_of(&response, "dev"), "degraded", "{response}");
    let degraded = detail_of(&response, "dev");
    assert_eq!(degraded["reason"], json!("INDEX_REQUIRED"), "{degraded}");
    assert_eq!(degraded["resolved_mode"], json!("hybrid"), "{degraded}");
    assert_eq!(
        degraded["hybrid"]["hint"],
        json!("run agent-memory index"),
        "{degraded}"
    );
    // The request as a whole still answered from the indexed source.
    assert!(!response["results"].as_array().unwrap().is_empty(), "{response}");
    assert_eq!(response["errors"], json!([]), "{response}");
}

#[test]
fn order_and_group_by_session_resolve_the_default_mode_to_exact() {
    let env = support::TestEnv::new();
    let (_oc, config_path) = channels_fixture(&env);
    let config = load_config(&config_path).unwrap();
    index_all(&config, None, false).unwrap();

    let with_order = json!({"query": "py3", "sources": ["oc"], "limit": 20,
                            "order": "newest"});
    let response = search_request(&config_path, with_order.clone());
    assert_eq!(
        detail_of(&response, "oc")["resolved_mode"],
        json!("exact"),
        "{response}"
    );
    let mut explicit = with_order.clone();
    explicit["mode"] = json!("exact");
    assert_eq!(
        serde_json::to_string(&response).unwrap(),
        serde_json::to_string(&search_request(&config_path, explicit)).unwrap(),
        "the resolved default must be exactly the explicit exact path"
    );

    let grouped = json!({"query": "py3", "sources": ["oc"], "limit": 20,
                         "group_by_session": true});
    let response = search_request(&config_path, grouped);
    assert_eq!(detail_of(&response, "oc")["resolved_mode"], json!("exact"));
    assert!(response["results"][0]["session_hits"].as_u64().is_some());

    // Without those keys the same request stays hybrid.
    let plain = search_request(
        &config_path,
        json!({"query": "py3", "sources": ["oc"], "limit": 20}),
    );
    assert_eq!(detail_of(&plain, "oc")["resolved_mode"], json!("hybrid"));
}

#[test]
fn explicit_legacy_and_hybrid_keep_rejecting_unsupported_keys() {
    let error = SearchRequest::parse(&json!({
        "query": "x", "mode": "legacy", "since": "2026-01-01T00:00:00Z"
    }))
    .unwrap_err();
    assert_eq!(error.code, "INVALID_REQUEST");
    let detail = error.detail.unwrap();
    assert_eq!(detail["path"], "since");
    assert_eq!(detail["reason"], "exact_only");

    for key in ["order", "group_by_session"] {
        let mut request = json!({"query": "x", "mode": "hybrid"});
        request[key] = if key == "order" {
            json!("newest")
        } else {
            json!(true)
        };
        let error = SearchRequest::parse(&request).unwrap_err();
        assert_eq!(error.detail.unwrap()["reason"], "hybrid_fixed_order", "{key}");
    }

    // The default mode accepts every one of them.
    for request in [
        json!({"query": "x", "since": "2026-01-01T00:00:00Z"}),
        json!({"query": "x", "roles": ["user"]}),
        json!({"query": "x", "order": "oldest"}),
        json!({"query": "x", "group_by_session": true}),
    ] {
        assert!(SearchRequest::parse(&request).is_ok(), "{request}");
    }
}

/// A Chinese question without spaces reaches the lexical branch through its
/// trigrams: no part is the whole sentence, several parts occur in the target
/// message, and the coverage gate keeps the hit even though the cosine is low.
#[test]
fn a_chinese_question_without_spaces_reaches_the_lexical_branch() {
    let env = support::TestEnv::new();
    let cache = env.root(CACHE);
    let oc = opencode(&env, "oc");
    add_session(&oc.db, "s-target", "阶段讨论", None, 1_000, 9_000);
    add_session(&oc.db, "s-other", "无关会话", None, 900, 8_000);
    add_text(
        &oc.db,
        "m-target",
        "s-target",
        1,
        1_700_000_000_000,
        "user",
        "为什么当时说第二阶段还不能算完成",
    );
    add_text(
        &oc.db,
        "m-other",
        "s-other",
        1,
        1_700_000_001_000,
        "user",
        "无关内容：缓存清理与部署脚本",
    );
    let config_path = env.write_json(
        "cjk-config.json",
        &v2_config(
            vec![source("oc", "opencode", &oc.root)],
            vec!["oc"],
            Some(&cache),
            Some(embedding(32, "fake-hash-v1")),
        ),
    );
    let config = load_config(&config_path).unwrap();
    index_all(&config, None, false).unwrap();

    // Same words, different order, one long sentence, a trailing question mark.
    let response = search_request(
        &config_path,
        json!({"query": "当时为什么说第二阶段还不能完成？", "sources": ["oc"], "limit": 20}),
    );
    let detail = detail_of(&response, "oc");
    assert_eq!(detail["resolved_mode"], json!("hybrid"), "{detail}");
    assert!(
        detail["hybrid"]["lexical_candidates"].as_u64().unwrap() > 0,
        "the whole sentence must not be the only MATCH part: {detail}"
    );
    assert_eq!(
        sessions(&response).first().unwrap(),
        "s-target",
        "{response}"
    );
    // The hit survives the gate through lexical coverage, not a high cosine.
    assert!(detail["hybrid"]["gated_out"].as_u64().is_some(), "{detail}");
}

// ---------------------------------------------------------------------------
// C. Surfaces
// ---------------------------------------------------------------------------

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_agent-memory")
}

#[test]
fn mcp_hybrid_search_never_writes_the_cache() {
    let env = support::TestEnv::new();
    let cache = env.root(CACHE);
    let (_oc, config_path) = channels_fixture(&env);
    let config = load_config(&config_path).unwrap();
    index_all(&config, None, false).unwrap();
    let before = support::tree_state(&cache);

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
                "arguments": {"query": "缓存 失效 py3", "sources": ["oc"]}
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
    let results = response["result"]["structuredContent"]["results"]
        .as_array()
        .unwrap();
    assert!(!results.is_empty(), "{response}");
    assert!(results[0].get("channels").is_some(), "{response}");
    let _ = child.kill();
    let _ = child.wait();
    assert_eq!(before, support::tree_state(&cache), "MCP must not write the cache");
}

#[test]
fn cli_and_mcp_agree_on_hybrid_envelopes() {
    let env = support::TestEnv::new();
    let (_oc, config_path) = channels_fixture(&env);
    let config = load_config(&config_path).unwrap();
    index_all(&config, None, false).unwrap();
    let request = json!({"query": "缓存 失效 py3", "mode": "hybrid", "sources": ["oc"]});
    let cli = std::process::Command::new(bin())
        .arg("memory_search")
        .arg("--config")
        .arg(&config_path)
        .arg("--request")
        .arg(request.to_string())
        .output()
        .expect("run cli");
    let cli_value: Value = serde_json::from_slice(&cli.stdout).unwrap();
    assert_eq!(cli.status.code(), Some(0), "{cli_value}");

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
        writeln!(
            stdin,
            "{}",
            json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {
                "name": "memory_search", "arguments": request
            }})
        )
        .unwrap();
        stdin.flush().unwrap();
    }
    let mut reader = BufReader::new(child.stdout.as_mut().unwrap());
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    let response: Value = serde_json::from_str(&line).unwrap();
    let _ = child.kill();
    let _ = child.wait();
    assert_eq!(
        response["result"]["structuredContent"], cli_value,
        "CLI and MCP must share one contract"
    );
}
