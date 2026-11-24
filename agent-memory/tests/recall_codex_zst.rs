//! Codex `.jsonl.zst` rollout carriers are indexed like their plain form.
//!
//! Codex compresses rollouts older than its threshold into `rollout-*.jsonl.zst`
//! and deletes the plain file. The retrieval path already reads both forms; the
//! index used to skip a lone compressed carrier, so a compression migration
//! silently dropped history from hybrid search. Every fixture here is synthetic
//! and lives under a per-test temporary root.

use agent_memory::recall::config::load_config;
use agent_memory::recall::fulltext::build::index_all;
use agent_memory::recall::search::{memory_search, SearchRequest, SearchResponse};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

mod support;

const SESSION: &str = "44444444-4444-4444-8444-444444444444";
const SECOND_SESSION: &str = "55555555-5555-4555-8555-555555555555";

fn rollout_rel(session: &str, day: u8) -> String {
    format!("sessions/2026/03/{day:02}/rollout-2026-03-{day:02}T21-50-12-{session}.jsonl")
}

fn rollout_text(session: &str, user_text: &str) -> String {
    let lines = [
        json!({
            "timestamp": "2026-03-06T21:50:12Z",
            "type": "session_meta",
            "payload": {"id": session, "cwd": "C:/work", "source": "cli"}
        })
        .to_string(),
        json!({
            "timestamp": "2026-03-06T21:50:13Z",
            "type": "response_item",
            "payload": {"type": "message", "role": "user", "content": [{"type": "input_text", "text": user_text}]}
        })
        .to_string(),
    ];
    lines.join("\n") + "\n"
}

fn write_plain(root: &Path, rel: &str, text: &str) -> PathBuf {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, text).unwrap();
    path
}

fn write_zst(root: &Path, rel: &str, text: &str) -> PathBuf {
    let path = root.join(format!("{rel}.zst"));
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let encoded = zstd::stream::encode_all(text.as_bytes(), 3).unwrap();
    std::fs::write(&path, encoded).unwrap();
    path
}

fn config(env: &support::TestEnv, sources: Vec<Value>, order: Vec<&str>, cache: &Path) -> PathBuf {
    let value = json!({
        "version": 2,
        "sources": sources,
        "source_order": order,
        "interpreter": {"kb": {"command": "C:/python/python.exe"}},
        "domain": "default",
        "telemetry": {"enabled": false},
        "cache_root": cache.to_string_lossy(),
    });
    env.write_json("recall.json", &value)
}

fn source(id: &str, root: &Path) -> Value {
    json!({
        "source_id": id,
        "kind": "codex",
        "root": root.to_string_lossy(),
        "enabled": true,
    })
}

fn search(config_path: &Path, request: Value) -> SearchResponse {
    let config = load_config(config_path).unwrap();
    let parsed = SearchRequest::parse(&request).unwrap();
    memory_search(&config, &parsed)
}

fn exit_status(response: &SearchResponse) -> String {
    response
        .source_status
        .iter()
        .map(|status| status.status.to_string())
        .collect::<Vec<_>>()
        .join(",")
}

#[test]
fn compressing_a_rollout_keeps_the_same_rows_and_hit_ids() {
    let env = support::TestEnv::new();
    let cache = env.root("cache");
    let root = env.root("codex");
    let rel = rollout_rel(SESSION, 6);
    let text = rollout_text(SESSION, "PrependPath=1 codex interactive");
    let plain_path = write_plain(&root, &rel, &text);
    let config_path = config(&env, vec![source("cx", &root)], vec!["cx"], &cache);
    let loaded = load_config(&config_path).unwrap();

    let before = index_all(&loaded, None, false).unwrap();
    assert_eq!(before[0].status, "built", "{:?}", before[0]);
    let plain_rows = before[0].rows;
    let plain_hits = search(
        &config_path,
        json!({"query": "PrependPath=1", "mode": "exact", "sources": ["cx"], "limit": 10}),
    );
    assert_eq!(plain_hits.results.len(), 1, "{plain_hits:?}");
    let plain_hit_id = plain_hits.results[0].hit_id.clone();

    // Codex compresses the rollout: the plain file disappears, the `.zst`
    // carries the same bytes (its session keeps the same logical path).
    std::fs::remove_file(&plain_path).unwrap();
    write_zst(&root, &rel, &text);
    let after = index_all(&loaded, None, false).unwrap();
    assert_eq!(after[0].status, "updated", "{:?}", after[0]);
    assert_eq!(after[0].rows, plain_rows, "same rows after compression");
    let detail = after[0].detail.as_ref().unwrap();
    assert_eq!(detail["compressed_carriers"], json!(1), "{detail}");
    assert!(detail["compressed_rows"].as_u64().unwrap_or(0) >= 1, "{detail}");

    let zst_hits = search(
        &config_path,
        json!({"query": "PrependPath=1", "mode": "exact", "sources": ["cx"], "limit": 10}),
    );
    assert_eq!(zst_hits.results.len(), 1, "{zst_hits:?}");
    assert_eq!(
        zst_hits.results[0].reference.locator.session_id.as_deref(),
        Some(SESSION)
    );
    assert_eq!(zst_hits.results[0].hit_id, plain_hit_id, "hit ids survive compression");
}

#[test]
fn when_both_forms_exist_only_the_plain_file_is_indexed() {
    let env = support::TestEnv::new();
    let cache = env.root("cache");
    let both_root = env.root("codex-both");
    let plain_root = env.root("codex-plain");
    let rel = rollout_rel(SESSION, 6);
    let text = rollout_text(SESSION, "PrependPath=1 codex interactive");
    write_plain(&plain_root, &rel, &text);
    write_plain(&both_root, &rel, &text);
    write_zst(&both_root, &rel, &text);

    let config_path = config(
        &env,
        vec![source("cx-both", &both_root), source("cx-plain", &plain_root)],
        vec!["cx-both", "cx-plain"],
        &cache,
    );
    let loaded = load_config(&config_path).unwrap();
    let reports = index_all(&loaded, None, false).unwrap();
    assert_eq!(reports[0].rows, reports[1].rows, "siblings must not duplicate rows");
    let carriers = reports[0]
        .detail
        .as_ref()
        .and_then(|detail| detail.get("compressed_carriers"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    assert_eq!(carriers, 0, "the zst sibling is deduped away: {:?}", reports[0].detail);
}

#[test]
fn a_rebuilt_archive_is_rescanned_whole() {
    let env = support::TestEnv::new();
    let cache = env.root("cache");
    let root = env.root("codex-zst");
    let rel = rollout_rel(SESSION, 6);
    write_zst(&root, &rel, &rollout_text(SESSION, "PrependPath=1 first-marker"));
    let config_path = config(&env, vec![source("cx", &root)], vec!["cx"], &cache);
    let loaded = load_config(&config_path).unwrap();
    index_all(&loaded, None, false).unwrap();

    // Recompress with one more record: a different size, so the plan is a full
    // re-read (a compressed carrier never appends in place).
    write_zst(&root, &rel, &rollout_text(SESSION, "PrependPath=1 second-marker"));
    let again = index_all(&loaded, None, false).unwrap();
    assert_eq!(again[0].status, "updated", "{:?}", again[0]);
    assert!(again[0].added >= 1, "{:?}", again[0]);

    let hits = search(
        &config_path,
        json!({"query": "second-marker", "mode": "exact", "sources": ["cx"], "limit": 10}),
    );
    assert_eq!(hits.results.len(), 1, "{hits:?}");
}

#[test]
fn a_corrupt_archive_is_isolated_and_counted() {
    let env = support::TestEnv::new();
    let cache = env.root("cache");
    let root = env.root("codex-zst");
    write_zst(&root, &rollout_rel(SESSION, 6), &rollout_text(SESSION, "PrependPath=1 good-marker"));
    let broken = root.join(format!("{}.zst", rollout_rel(SECOND_SESSION, 7)));
    std::fs::create_dir_all(broken.parent().unwrap()).unwrap();
    std::fs::write(&broken, b"not a zstd frame").unwrap();

    let config_path = config(&env, vec![source("cx", &root)], vec!["cx"], &cache);
    let loaded = load_config(&config_path).unwrap();
    let reports = index_all(&loaded, None, false).unwrap();
    let report = &reports[0];
    assert_eq!(report.status, "built", "{report:?}");
    assert!(report.rows > 0, "the healthy carrier still indexes: {report:?}");
    let detail = report.detail.as_ref().unwrap();
    assert_eq!(detail["compressed_carriers"], json!(2), "{detail}");
    assert_eq!(detail["compressed_errors"], json!(1), "{detail}");

    let hits = search(
        &config_path,
        json!({"query": "good-marker", "mode": "exact", "sources": ["cx"], "limit": 10}),
    );
    assert_eq!(hits.results.len(), 1, "{hits:?}");
}

#[test]
fn an_unchanged_archive_is_not_rescanned() {
    let env = support::TestEnv::new();
    let cache = env.root("cache");
    let root = env.root("codex-zst");
    write_zst(&root, &rollout_rel(SESSION, 6), &rollout_text(SESSION, "PrependPath=1 stable-marker"));
    let config_path = config(&env, vec![source("cx", &root)], vec!["cx"], &cache);
    let loaded = load_config(&config_path).unwrap();
    let first = index_all(&loaded, None, false).unwrap();
    assert_eq!(first[0].status, "built", "{:?}", first[0]);

    let second = index_all(&loaded, None, false).unwrap();
    assert_eq!(second[0].status, "unchanged", "{:?}", second[0]);
}
