//! Process documents: a recursively-indexed markdown source with heading
//! chunks, read back by line address.
//!
//! The docs source exists because process manuals live outside the vault and
//! outside the dated device notes; without it a manual is not on any retrieval
//! surface. Every fixture is synthetic and lives under a temporary root.

use agent_memory::recall::config::load_config;
use agent_memory::recall::contract::INDEX_REQUIRED;
use agent_memory::recall::fulltext::build::index_all;
use agent_memory::recall::read::{memory_read, ReadRequest, ReadResponse};
use agent_memory::recall::search::{memory_search, SearchRequest, SearchResponse};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

mod support;

fn docs_config(env: &support::TestEnv, root: &Path, cache: &Path, source: Value) -> PathBuf {
    let value = json!({
        "version": 2,
        "sources": [source],
        "source_order": ["docs"],
        "interpreter": {"kb": {"command": "C:/python/python.exe"}},
        "domain": "default",
        "telemetry": {"enabled": false},
        "cache_root": cache.to_string_lossy(),
    });
    let _ = root;
    env.write_json("recall.json", &value)
}

fn docs_source(root: &Path, include: Value, exclude: Value) -> Value {
    json!({
        "source_id": "docs",
        "kind": "docs",
        "root": root.to_string_lossy(),
        "enabled": true,
        "include": include,
        "exclude": exclude,
    })
}

fn write(root: &Path, rel: &str, text: &str) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn search(config_path: &Path, request: Value) -> SearchResponse {
    let config = load_config(config_path).unwrap();
    let parsed = SearchRequest::parse(&request).unwrap();
    memory_search(&config, &parsed)
}

fn read(config_path: &Path, reference: Value) -> ReadResponse {
    let config = load_config(config_path).unwrap();
    let request = ReadRequest::parse(&json!({
        "ref": reference,
        "context_before": 0,
        "context_after": 0,
        "max_bytes": 8192,
    }))
    .unwrap();
    memory_read(&config, &request)
}

const MANUAL: &str = "# Manual title\nintro needleintro\n## Section one\nbody needleone\n## Section two\nbody needletwo\n";

#[test]
fn nested_markdown_is_indexed_by_heading_chunks_and_reads_back() {
    let env = support::TestEnv::new();
    let cache = env.root("cache");
    let root = env.root("docs");
    write(&root, "a/b/manual.md", MANUAL);
    let config_path = docs_config(
        &env,
        &root,
        &cache,
        docs_source(&root, json!([]), json!([])),
    );
    let loaded = load_config(&config_path).unwrap();
    let reports = index_all(&loaded, None, false).unwrap();
    assert_eq!(reports[0].status, "built", "{:?}", reports[0]);
    assert!(reports[0].rows >= 3, "one unit per section: {:?}", reports[0]);
    assert_eq!(reports[0].detail.as_ref().unwrap()["files"], json!(1));

    let response = search(
        &config_path,
        json!({"query": "needleone", "mode": "exact", "sources": ["docs"], "limit": 10}),
    );
    assert_eq!(response.results.len(), 1, "{response:?}");
    let hit = &response.results[0];
    assert!(
        hit.title.contains("Manual title") && hit.title.contains("Section one"),
        "title carries the heading path: {}",
        hit.title
    );

    let read_back = read(&config_path, serde_json::to_value(&hit.reference).unwrap());
    assert_eq!(read_back.segments.len(), 1);
    let segment = &read_back.segments[0];
    assert!(segment.text.contains("needleone"), "{}", segment.text);
    assert!(
        !segment.text.contains("needletwo"),
        "the read is the chunk, not the file: {}",
        segment.text
    );
    // "# Manual title" is line 0, "intro" line 1, "## Section one" line 2.
    assert_eq!(segment.text_range.line_start, 2, "{:?}", segment.text_range);
}

#[test]
fn include_and_exclude_globs_decide_the_file_set() {
    let env = support::TestEnv::new();
    let cache = env.root("cache");
    let root = env.root("docs");
    write(&root, "keep/a.md", "# Keep\nbody needlekeep\n");
    write(&root, "_archive/b.md", "# Archive\nbody needlearchive\n");
    let config_path = docs_config(
        &env,
        &root,
        &cache,
        docs_source(
            &root,
            json!(["**/*.md"]),
            json!(["_archive/**"]),
        ),
    );
    let loaded = load_config(&config_path).unwrap();
    index_all(&loaded, None, false).unwrap();

    let kept = search(
        &config_path,
        json!({"query": "needlekeep", "mode": "exact", "sources": ["docs"], "limit": 10}),
    );
    assert_eq!(kept.results.len(), 1, "{kept:?}");
    let excluded = search(
        &config_path,
        json!({"query": "needlearchive", "mode": "exact", "sources": ["docs"], "limit": 10}),
    );
    assert_eq!(excluded.results.len(), 0, "{excluded:?}");
}

#[test]
fn a_deleted_file_drops_its_chunks() {
    let env = support::TestEnv::new();
    let cache = env.root("cache");
    let root = env.root("docs");
    write(&root, "gone.md", "# Gone\nbody needlegone\n");
    let config_path = docs_config(
        &env,
        &root,
        &cache,
        docs_source(&root, json!([]), json!([])),
    );
    let loaded = load_config(&config_path).unwrap();
    index_all(&loaded, None, false).unwrap();
    std::fs::remove_file(root.join("gone.md")).unwrap();
    let after = index_all(&loaded, None, false).unwrap();
    assert_eq!(after[0].status, "updated", "{:?}", after[0]);
    assert_eq!(after[0].rows, 0, "{:?}", after[0]);

    let response = search(
        &config_path,
        json!({"query": "needlegone", "mode": "exact", "sources": ["docs"], "limit": 10}),
    );
    assert_eq!(response.results.len(), 0, "{response:?}");
}

#[test]
fn without_an_index_the_source_scans_and_reports_index_required() {
    let env = support::TestEnv::new();
    let cache = env.root("cache");
    let root = env.root("docs");
    write(&root, "manual.md", MANUAL);
    let config_path = docs_config(
        &env,
        &root,
        &cache,
        docs_source(&root, json!([]), json!([])),
    );
    let response = search(
        &config_path,
        json!({"query": "needleone", "mode": "auto", "sources": ["docs"], "limit": 10}),
    );
    // The scan answers, but the degradation stays visible: an index is still
    // what makes ordinary queries cheap.
    assert_eq!(response.results.len(), 1, "{response:?}");
    let status = &response.source_status[0];
    assert_eq!(status.code, Some(INDEX_REQUIRED), "{status:?}");
    assert_eq!(status.status, "degraded", "{status:?}");
}

#[test]
fn an_oversized_section_is_split_at_line_boundaries() {
    let env = support::TestEnv::new();
    let cache = env.root("cache");
    let root = env.root("docs");
    let mut body = String::from("# Long\n## Big section\n");
    for index in 0..200 {
        body.push_str(&format!("line {index} needletail{}\n", if index % 50 == 0 { "x" } else { "" }));
    }
    write(&root, "long.md", &body);
    let config_path = docs_config(
        &env,
        &root,
        &cache,
        docs_source(&root, json!([]), json!([])),
    );
    let loaded = load_config(&config_path).unwrap();
    let reports = index_all(&loaded, None, false).unwrap();
    assert!(reports[0].rows >= 2, "one big section becomes several units: {:?}", reports[0]);

    let response = search(
        &config_path,
        json!({"query": "needletailx", "mode": "exact", "sources": ["docs"], "limit": 10}),
    );
    assert!(!response.results.is_empty(), "{response:?}");
}

// ---------------------------------------------------------------------------
// Blocker regressions (independent review, 2026-11-18)
// ---------------------------------------------------------------------------

fn indexed(env: &support::TestEnv, root: &Path, cache: &Path, source: Value) -> PathBuf {
    let config_path = docs_config(env, root, cache, source);
    let loaded = load_config(&config_path).unwrap();
    index_all(&loaded, None, false).unwrap();
    config_path
}

#[test]
fn the_default_mode_returns_every_matching_chunk_of_one_file() {
    // Hybrid used to collapse hits by session_id; a docs "session" is the file,
    // so only the best chunk survived. Both sections must be reachable.
    let env = support::TestEnv::new();
    let cache = env.root("cache");
    let root = env.root("docs");
    write(
        &root,
        "manual.md",
        "# Manual\n## One\nsharedneedle alpha\n## Two\nsharedneedle beta\n",
    );
    let config_path = indexed(&env, &root, &cache, docs_source(&root, json!([]), json!([])));

    // No `mode`: auto → hybrid (the index is readable).
    let response = search(
        &config_path,
        json!({"query": "sharedneedle", "sources": ["docs"], "limit": 10}),
    );
    assert_eq!(response.results.len(), 2, "{response:?}");
    let titles: Vec<&str> = response.results.iter().map(|hit| hit.title.as_str()).collect();
    assert!(titles.iter().any(|title| title.contains("One")), "{titles:?}");
    assert!(titles.iter().any(|title| title.contains("Two")), "{titles:?}");
}

#[test]
fn a_two_character_cjk_query_falls_back_to_the_scan() {
    let env = support::TestEnv::new();
    let cache = env.root("cache");
    let root = env.root("docs");
    write(&root, "approval.md", "# 审批\n提交审批后由主管确认。\n");
    let config_path = indexed(&env, &root, &cache, docs_source(&root, json!([]), json!([])));

    let response = search(
        &config_path,
        json!({"query": "审批", "sources": ["docs"], "limit": 10}),
    );
    assert_eq!(response.results.len(), 1, "{response:?}");
    assert!(response.results[0].title.contains("审批"));
    // The degradation is visible, not a silent empty answer.
    assert_eq!(response.source_status[0].status, "degraded", "{response:?}");
}

#[test]
fn the_default_excludes_keep_archived_manuals_out() {
    let env = support::TestEnv::new();
    let cache = env.root("cache");
    let root = env.root("docs");
    write(&root, "keep/a.md", "# Keep\ndocneedle kept\n");
    write(&root, "_archive/b.md", "# Archive\ndocneedle archived\n");
    // No exclude in the configuration: the engine's defaults apply.
    let config_path = indexed(&env, &root, &cache, docs_source(&root, json!([]), json!([])));

    let response = search(
        &config_path,
        json!({"query": "docneedle", "mode": "exact", "sources": ["docs"], "limit": 10}),
    );
    assert_eq!(response.results.len(), 1, "{response:?}");
    assert!(response.results[0].snippet.contains("kept") || response.results[0].title.contains("Keep"));
}

#[test]
fn deleting_a_file_makes_the_coverage_stale() {
    let env = support::TestEnv::new();
    let cache = env.root("cache");
    let root = env.root("docs");
    write(&root, "gone.md", "# Gone\nstaleneedle body\n");
    let config_path = indexed(&env, &root, &cache, docs_source(&root, json!([]), json!([])));
    std::fs::remove_file(root.join("gone.md")).unwrap();

    // No reindex: a deleted carrier must show up as stale, as device/dsh do.
    let response = search(
        &config_path,
        json!({"query": "staleneedle", "sources": ["docs"], "limit": 10}),
    );
    let detail = response.source_status[0].detail.as_ref().expect("detail");
    let stale = detail.get("coverage").and_then(|coverage| coverage.get("stale_since"));
    assert!(stale.is_some(), "deletion must be visible in coverage: {detail}");
}

#[test]
fn legacy_mode_scans_instead_of_answering_empty() {
    let env = support::TestEnv::new();
    let cache = env.root("cache");
    let root = env.root("docs");
    write(&root, "manual.md", "# Manual\n## Section\nlegacyneedle here\n");
    let config_path = indexed(&env, &root, &cache, docs_source(&root, json!([]), json!([])));

    let response = search(
        &config_path,
        json!({"query": "legacyneedle", "mode": "legacy", "sources": ["docs"], "limit": 10}),
    );
    assert_eq!(response.results.len(), 1, "{response:?}");
}

#[test]
fn the_default_include_matches_uppercase_extensions() {
    let env = support::TestEnv::new();
    let cache = env.root("cache");
    let root = env.root("docs");
    write(&root, "upper.MD", "# Upper\ncaseneedle body\n");
    let config_path = indexed(&env, &root, &cache, docs_source(&root, json!([]), json!([])));
    let response = search(
        &config_path,
        json!({"query": "caseneedle", "mode": "exact", "sources": ["docs"], "limit": 10}),
    );
    assert_eq!(response.results.len(), 1, "{response:?}");
}

#[test]
fn a_heading_inside_a_fenced_block_does_not_split() {
    let env = support::TestEnv::new();
    let cache = env.root("cache");
    let root = env.root("docs");
    write(
        &root,
        "fenced.md",
        "# Fenced\n```txt\n## not a heading\n```\n## Real\nfenceneedle\n",
    );
    let config_path = docs_config(&env, &root, &cache, docs_source(&root, json!([]), json!([])));
    let loaded = load_config(&config_path).unwrap();
    let reports = index_all(&loaded, None, false).unwrap();
    assert_eq!(reports[0].rows, 2, "one unit before and one after the heading: {:?}", reports[0]);
}

#[test]
fn a_docs_ref_without_a_chunk_pointer_is_rejected() {
    // The locator schema for docs-md-v1 requires /chunk/<byte>; a ref that
    // omits it is a malformed request, not a read of offset zero.
    let error = ReadRequest::parse(&json!({
        "ref": {
            "source_id": "docs",
            "source_kind": "docs",
            "root_sha256": "0".repeat(64),
            "rel_path": "manual.md",
            "schema_gen": "docs-md-v1",
            "locator": {},
            "fingerprint": {"range_sha256": "0".repeat(64), "file_sha256": null, "row_sha256": null},
            "text_range": {"byte_start": 0, "byte_end": 1, "line_start": 0, "line_end": 0},
        },
        "max_bytes": 1024,
    }))
    .expect_err("a docs ref without a pointer must be rejected");
    assert_eq!(error.code, "INVALID_REQUEST", "{error:?}");
}
