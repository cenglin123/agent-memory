//! P1 OpenCode tests: body hits, projection, injection/metadata handling,
//! coverage, budgets, schema generations and fail-closed source states.

mod support;

use agent_memory::recall::budget::Budget;
use agent_memory::recall::config::load_config;
use agent_memory::recall::contract::{codes, status, SourceStatus};
use agent_memory::recall::providers::opencode;
use agent_memory::recall::search::{memory_search, SearchRequest, SearchResponse};
use agent_memory::recall::text::tokenize;
use rusqlite::Connection;
use serde_json::{json, Value};
use std::path::Path;

fn search(config_path: &Path, request: Value) -> SearchResponse {
    let config = load_config(config_path).expect("config");
    let request = SearchRequest::parse(&request).expect("request");
    memory_search(&config, &request)
}

fn source_status<'a>(response: &'a SearchResponse, source_id: &str) -> &'a SourceStatus {
    response
        .source_status
        .iter()
        .find(|entry| entry.source_id == source_id)
        .expect("source status present")
}

fn detail_value<'a>(response: &'a SearchResponse, source_id: &str, key: &str) -> Option<&'a Value> {
    source_status(response, source_id)
        .detail
        .as_ref()
        .and_then(|detail| detail.get(key))
}

#[test]
fn v1_body_and_tool_projection_hits() {
    let env = support::TestEnv::new();
    let fixture = support::build_v1(&env);
    let configured = support::configured(&env, "v1", &fixture.root);
    let response = search(
        &configured.config,
        json!({"query": support::NEEDLE, "limit": 50}),
    );

    assert_eq!(source_status(&response, "oc").status, status::OK);
    assert_eq!(response.exit_code, 0);
    assert_eq!(response.results.len(), 4, "text + tool + two non-injected variants");
    assert!(response.errors.is_empty());

    for result in &response.results {
        assert_eq!(result.hit_id.len(), 64);
        assert!(result.hit_id.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_eq!(result.score, None);
        assert_eq!(result.score_kind, "none");
        assert_eq!(result.rank_basis, "lexical_window");
        assert_eq!(result.title, "项目讨论");
        assert_eq!(result.source_kind, "opencode");
        assert_eq!(result.reference.schema_gen, "sqlite-v1");
        assert_eq!(result.reference.rel_path, "opencode.db");
        assert_eq!(
            result.reference.root_sha256,
            agent_memory::recall::hash::root_sha256(&fixture.root.to_string_lossy())
        );
        let locator = serde_json::to_value(&result.reference.locator).unwrap();
        assert!(locator.get("table").is_none());
        assert!(locator.get("rowid").is_none());
        assert!(locator.get("session_id").is_some());
    }

    let unique: std::collections::BTreeSet<&str> =
        response.results.iter().map(|result| result.hit_id.as_str()).collect();
    assert_eq!(unique.len(), response.results.len());

    let snippets: Vec<&str> = response
        .results
        .iter()
        .map(|result| result.snippet.as_str())
        .collect();
    assert!(snippets.iter().any(|snippet| snippet.contains("beginoutput")));
    assert!(snippets.iter().any(|snippet| snippet.contains("synthetic-string")));
    assert!(snippets.iter().any(|snippet| snippet.contains("user-origin")));
    assert!(!snippets.iter().any(|snippet| snippet.contains("injected")));

    let ignored = detail_value(&response, "oc", "ignored_types").expect("ignored_types");
    assert_eq!(
        ignored,
        &json!({
            "snapshot": 1,
            "patch": 1,
            "file": 1,
            "agent": 1,
            "compaction": 1,
            "retry": 1,
            "subtask": 1,
            "widget": 1
        })
    );
    let coverage = detail_value(&response, "oc", "coverage").unwrap();
    assert_eq!(coverage["scanned_sessions"], 1);
    assert_eq!(coverage["total_sessions"], 1);
}

#[test]
fn tool_projection_keeps_full_output_and_input_middle() {
    let env = support::TestEnv::new();
    let fixture = support::build_v1(&env);
    let configured = support::configured(&env, "v1tool", &fixture.root);

    let by_input = search(
        &configured.config,
        json!({"query": "midneedle", "limit": 10}),
    );
    assert_eq!(by_input.results.len(), 1);
    assert_eq!(by_input.results[0].reference.schema_gen, "sqlite-v1");

    let by_tail = search(
        &configured.config,
        json!({"query": "endoutput", "limit": 10}),
    );
    assert_eq!(by_tail.results.len(), 1);
    let range = by_tail.results[0].reference.text_range;
    assert!(
        range.byte_len() > 20_000,
        "full tool output must be addressable, got {} bytes",
        range.byte_len()
    );
    assert_eq!(range.byte_start, 0);
    assert!(
        by_tail.results[0].snippet.contains("endoutput"),
        "a hit at the end of a long single line must stay visible in the snippet"
    );
    assert!(by_tail.results[0].snippet_truncated);
    assert!(by_tail.results[0].snippet.len() <= 400);
}

#[test]
fn snippet_bytes_changes_display_only() {
    let env = support::TestEnv::new();
    let fixture = support::build_v1(&env);
    let configured = support::configured(&env, "v1snip", &fixture.root);

    let small = search(
        &configured.config,
        json!({"query": support::NEEDLE, "limit": 50, "snippet_bytes": 40}),
    );
    let large = search(
        &configured.config,
        json!({"query": support::NEEDLE, "limit": 50, "snippet_bytes": 800}),
    );
    let ids = |response: &SearchResponse| {
        let mut pairs: Vec<(String, String)> = response
            .results
            .iter()
            .map(|result| (result.hit_id.clone(), format!("{:?}", result.reference.text_range)))
            .collect();
        pairs.sort();
        pairs
    };
    assert_eq!(ids(&small), ids(&large));
    assert!(small.results.iter().any(|result| result.snippet_truncated));
    assert!(!large.results.iter().all(|result| result.snippet_truncated));
    assert!(small.results.iter().all(|result| result.snippet.len() <= 40));
}

#[test]
fn root_sha_and_hit_id_are_stable_across_config_spellings() {
    let env = support::TestEnv::new();
    let fixture = support::build_v1(&env);
    let lower = fixture.root.to_string_lossy().to_string();
    let upper = lower.to_uppercase().replace('\\', "/");

    let lowercase_config = env.write_json(
        "lower-config.json",
        &env.config_value(vec![env.opencode_source("oc", &fixture.root)], vec!["oc"]),
    );
    let uppercase_config = env.write_json(
        "upper-config.json",
        &env.config_value(
            vec![json!({
                "source_id": "oc",
                "kind": "opencode",
                "root": upper,
                "enabled": true,
            })],
            vec!["oc"],
        ),
    );

    let first = search(&lowercase_config, json!({"query": support::NEEDLE, "limit": 50}));
    let second = search(&uppercase_config, json!({"query": support::NEEDLE, "limit": 50}));
    assert_eq!(first.results.len(), second.results.len());
    let first_ids: Vec<&str> = first.results.iter().map(|r| r.hit_id.as_str()).collect();
    let second_ids: Vec<&str> = second.results.iter().map(|r| r.hit_id.as_str()).collect();
    assert_eq!(first_ids, second_ids);
    assert_eq!(
        first.results[0].reference.root_sha256,
        second.results[0].reference.root_sha256
    );
}

#[test]
fn crlf_text_is_preserved_in_the_projection() {
    let env = support::TestEnv::new();
    let fixture = support::build_v1(&env);
    let configured = support::configured(&env, "v1crlf", &fixture.root);
    let response = search(
        &configured.config,
        json!({"query": "needlecrlf", "limit": 10}),
    );
    assert_eq!(response.results.len(), 1);
    assert_eq!(
        response.results[0].reference.text_range.byte_len(),
        "line1\r\nneedlecrlf\r\nline3".len() as u64
    );
}

#[test]
fn sub_sessions_and_injected_parts_are_not_addressable() {
    let env = support::TestEnv::new();
    let fixture = support::build_v1(&env);
    let configured = support::configured(&env, "v1excl", &fixture.root);

    for query in ["needlechildonly", "needleocsronly"] {
        let response = search(&configured.config, json!({"query": query, "limit": 10}));
        assert!(response.results.is_empty(), "{query} must not be reachable");
        assert_eq!(source_status(&response, "oc").status, status::EMPTY);
        assert_eq!(response.exit_code, 0);
    }
}

#[test]
fn v2_content_and_text_pointer_branches() {
    let env = support::TestEnv::new();
    let fixture = support::build_v2(&env);
    let configured = support::configured(&env, "v2", &fixture.root);
    let response = search(
        &configured.config,
        json!({"query": support::NEEDLE_V2, "limit": 50}),
    );

    assert_eq!(source_status(&response, "oc").status, status::OK);
    let observed: Vec<(String, String)> = response
        .results
        .iter()
        .map(|result| {
            (
                result.reference.locator.pointer.clone().unwrap_or_default(),
                result.snippet.chars().take(40).collect(),
            )
        })
        .collect();
    assert_eq!(response.results.len(), 3, "observed: {observed:?}");
    let mut pointers: Vec<String> = response
        .results
        .iter()
        .map(|result| result.reference.locator.pointer.clone().unwrap())
        .collect();
    pointers.sort();
    assert_eq!(pointers, vec!["/content/0", "/content/1", "/text"]);
    for result in &response.results {
        assert_eq!(result.reference.schema_gen, "sqlite-v2");
        assert_eq!(result.reference.rel_path, "opencode.db");
        assert!(result.reference.locator.part_id.is_none());
        assert!(result.reference.locator.session_message_id.is_some());
    }
    assert_eq!(
        detail_value(&response, "oc", "ignored_types").unwrap(),
        &json!({"reasoning": 1})
    );
    let coverage = detail_value(&response, "oc", "coverage").unwrap();
    assert_eq!(coverage["total_sessions"], 1, "child session excluded");
}

#[test]
fn v2_injected_metadata_long_input_and_broken_text() {
    let env = support::TestEnv::new();
    let fixture = support::build_v2_extra(&env);
    let configured = support::configured(&env, "v2extra", &fixture.root);
    let response = search(
        &configured.config,
        json!({"query": "needleextra", "limit": 50}),
    );

    // text + tool input (the injected/snapshot units are excluded) + /text.
    assert_eq!(source_status(&response, "oc").status, status::PARTIAL);
    assert_eq!(
        source_status(&response, "oc").code,
        Some(codes::CONTENT_UNRESOLVED)
    );
    assert_eq!(
        detail_value(&response, "oc", "CONTENT_UNRESOLVED").unwrap(),
        &json!(1)
    );
    assert_eq!(
        detail_value(&response, "oc", "ignored_types").unwrap(),
        &json!({"snapshot": 1})
    );
    let mut pointers: Vec<String> = response
        .results
        .iter()
        .map(|result| result.reference.locator.pointer.clone().unwrap())
        .collect();
    pointers.sort();
    assert_eq!(pointers, vec!["/content/0", "/content/3", "/text"]);
    let tool_hit = response
        .results
        .iter()
        .find(|result| result.reference.locator.pointer.as_deref() == Some("/content/3"))
        .unwrap();
    assert!(
        tool_hit.snippet.contains("needleextra"),
        "a hit in a long tool input must stay visible"
    );
}

#[test]
fn v2_without_parent_column_still_works() {
    let env = support::TestEnv::new();
    let fixture = support::build_v2_without_parent(&env);
    let configured = support::configured(&env, "v2np", &fixture.root);
    let response = search(
        &configured.config,
        json!({"query": support::NEEDLE_V2, "limit": 10}),
    );
    assert_eq!(source_status(&response, "oc").status, status::OK);
    assert_eq!(response.results.len(), 1);
    assert_eq!(response.results[0].reference.locator.pointer.as_deref(), Some("/text"));
}

#[test]
fn v1_fallback_reports_session_v2_state() {
    let env = support::TestEnv::new();

    // v1 complete, no session_v2 at all.
    let root = env.root("v1-fallback");
    let db = root.join("opencode.db");
    let conn = Connection::open(&db).unwrap();
    conn.execute_batch(support::V1_SCHEMA).unwrap();
    conn.execute(
        "INSERT INTO session (id, parent_id, title, time_created, time_updated) VALUES ('s1', NULL, 'fallback', 1, 2)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO message (id, session_id, time_created, time_updated, data) VALUES ('m1', 's1', 3, 4, '{}')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data) VALUES ('p1', 'm1', 's1', 5, 6, ?1)",
        [serde_json::to_string(&json!({"type": "text", "text": "fallbackneedle"})).unwrap()],
    )
    .unwrap();
    drop(conn);
    let configured = support::configured(&env, "fallback", &root);
    let response = search(&configured.config, json!({"query": "fallbackneedle"}));
    assert_eq!(source_status(&response, "oc").status, status::OK);
    assert_eq!(
        detail_value(&response, "oc", "session_v2").unwrap(),
        &json!("absent")
    );

    // v1 complete with an incomplete session_v2 table.
    let root = env.root("v1-incomplete");
    let db = root.join("opencode.db");
    let conn = Connection::open(&db).unwrap();
    conn.execute_batch(support::V1_SCHEMA).unwrap();
    conn.execute(
        "CREATE TABLE session_v2 (id TEXT PRIMARY KEY)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO session (id, parent_id, title, time_created, time_updated) VALUES ('s1', NULL, 'fallback', 1, 2)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO message (id, session_id, time_created, time_updated, data) VALUES ('m1', 's1', 3, 4, '{}')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data) VALUES ('p1', 'm1', 's1', 5, 6, ?1)",
        [serde_json::to_string(&json!({"type": "text", "text": "fallbackneedle"})).unwrap()],
    )
    .unwrap();
    drop(conn);
    let configured = support::configured(&env, "incomplete", &root);
    let response = search(&configured.config, json!({"query": "fallbackneedle"}));
    assert_eq!(source_status(&response, "oc").status, status::OK);
    assert_eq!(
        detail_value(&response, "oc", "session_v2").unwrap(),
        &json!("incomplete")
    );
}

#[test]
fn unavailable_unsupported_and_corrupt_are_distinct() {
    let env = support::TestEnv::new();

    // No database and no storage.
    let empty_root = env.root("empty-source");
    let configured = support::configured(&env, "empty", &empty_root);
    let response = search(&configured.config, json!({"query": "anything"}));
    assert_eq!(source_status(&response, "oc").status, status::UNAVAILABLE);
    assert_eq!(
        source_status(&response, "oc").code,
        Some(codes::DB_UNAVAILABLE)
    );
    assert_eq!(response.exit_code, 4);

    // A database with unrelated tables.
    let schema_root = env.root("bad-schema");
    let conn = Connection::open(schema_root.join("opencode.db")).unwrap();
    conn.execute_batch("CREATE TABLE foo (id TEXT);").unwrap();
    drop(conn);
    let configured = support::configured(&env, "schema", &schema_root);
    let response = search(&configured.config, json!({"query": "anything"}));
    assert_eq!(source_status(&response, "oc").status, status::UNSUPPORTED);
    assert_eq!(
        source_status(&response, "oc").code,
        Some(codes::SCHEMA_UNSUPPORTED)
    );
    let missing = detail_value(&response, "oc", "reason").unwrap();
    assert!(
        missing.as_str().unwrap_or("").contains("table:"),
        "missing tables/columns must be listed: {missing:?}"
    );

    // A corrupt database file.
    let corrupt_root = env.root("corrupt");
    env.write("corrupt/opencode.db", b"this is not a sqlite database");
    let configured = support::configured(&env, "corrupt", &corrupt_root);
    let response = search(&configured.config, json!({"query": "anything"}));
    assert_eq!(source_status(&response, "oc").status, status::UNAVAILABLE);
    assert_eq!(
        source_status(&response, "oc").code,
        Some(codes::DB_CORRUPT)
    );

    // A missing root.
    let missing_root = env.path().join("does-not-exist");
    let configured = support::configured(&env, "missing", &missing_root);
    let response = search(&configured.config, json!({"query": "anything"}));
    assert_eq!(
        source_status(&response, "oc").code,
        Some(codes::DB_UNAVAILABLE)
    );
}

#[test]
fn locked_database_is_not_an_empty_source() {
    let env = support::TestEnv::new();
    let fixture = support::build_v1(&env);
    let configured = support::configured(&env, "locked", &fixture.root);

    let holder = Connection::open(&fixture.db).unwrap();
    holder.execute_batch("BEGIN EXCLUSIVE").unwrap();
    let response = search(&configured.config, json!({"query": support::NEEDLE}));
    holder.execute_batch("ROLLBACK").unwrap();

    assert_eq!(source_status(&response, "oc").status, status::UNAVAILABLE);
    assert_eq!(
        source_status(&response, "oc").code,
        Some(codes::DB_LOCKED)
    );
    assert!(response.results.is_empty());
    assert_eq!(response.exit_code, 4);
}

#[test]
fn damaged_rows_are_partial_and_visible() {
    let env = support::TestEnv::new();
    let fixture = support::build_v1_damaged(&env);
    let configured = support::configured(&env, "damaged", &fixture.root);
    let response = search(
        &configured.config,
        json!({"query": support::NEEDLE, "limit": 50}),
    );
    assert_eq!(source_status(&response, "oc").status, status::PARTIAL);
    assert_eq!(
        source_status(&response, "oc").code,
        Some(codes::RECORD_SKIPPED)
    );
    assert_eq!(response.results.len(), 4, "good hits are still returned");
    assert_eq!(
        detail_value(&response, "oc", "CONTENT_UNRESOLVED").unwrap(),
        &json!(2)
    );
    assert_eq!(response.exit_code, 10);
    assert_eq!(response.errors.len(), 1);
    assert_eq!(response.errors[0].source_id.as_deref(), Some("oc"));

    let none = search(
        &configured.config,
        json!({"query": "zzznosuchterm", "limit": 50}),
    );
    assert!(none.results.is_empty());
    assert_eq!(source_status(&none, "oc").status, status::PARTIAL);
    assert_eq!(none.exit_code, 4);
}

#[test]
fn budget_progress_reports_coverage_and_truncation() {
    let env = support::TestEnv::new();
    let fixture = support::build_v1_hits(&env, "scale-small", 20, 5);
    let configured = support::configured(&env, "budget", &fixture.root);

    let truncated = search(
        &configured.config,
        json!({
            "query": support::NEEDLE,
            "limit": 100,
            "budget": {"max_bytes_per_source": 8192}
        }),
    );
    assert_eq!(source_status(&truncated, "oc").status, status::TRUNCATED);
    assert_eq!(
        source_status(&truncated, "oc").code,
        Some(codes::BUDGET_BYTES)
    );
    let budget = detail_value(&truncated, "oc", "budget").unwrap();
    assert_eq!(budget["reason"], codes::BUDGET_BYTES);
    assert!(budget["consumed_bytes"].as_u64().unwrap() >= 8192);
    let coverage = detail_value(&truncated, "oc", "coverage").unwrap();
    assert!(
        coverage["scanned_sessions"].as_u64().unwrap()
            < coverage["total_sessions"].as_u64().unwrap()
    );
    assert_eq!(truncated.next_offset, None);
    assert_eq!(truncated.exit_code, 10);
    assert!(truncated.pagination.sources_incomplete.contains(&"oc".to_string()));

    let complete = search(
        &configured.config,
        json!({
            "query": support::NEEDLE,
            "limit": 100,
            "budget": {"max_bytes_per_source": 67108864}
        }),
    );
    assert_eq!(source_status(&complete, "oc").status, status::OK);
    let coverage = detail_value(&complete, "oc", "coverage").unwrap();
    assert_eq!(
        coverage["scanned_sessions"].as_u64().unwrap(),
        coverage["total_sessions"].as_u64().unwrap()
    );
    assert_eq!(complete.results.len(), 100);
}

#[test]
fn time_budget_is_reported_by_the_provider() {
    let env = support::TestEnv::new();
    let fixture = support::build_v1_hits(&env, "scale-time", 20, 3);
    let config = load_config(&support::configured(&env, "time", &fixture.root).config).unwrap();
    let source = config.source("oc").unwrap();
    let budget = Budget {
        max_bytes_per_source: 64 * 1024 * 1024,
        max_elapsed_ms_per_source: 0,
        kb_top_k: 20,
    };
    let outcome = opencode::search(source, &tokenize(support::NEEDLE), &budget, 400);
    assert_eq!(outcome.status, status::TRUNCATED);
    assert_eq!(outcome.code, Some(codes::BUDGET_TIME));
    let detail = outcome.detail.unwrap();
    assert_eq!(detail["budget"]["reason"], codes::BUDGET_TIME);
    assert_eq!(detail["coverage"]["scanned_sessions"], 0);
}

#[test]
fn oversize_object_is_not_budget_bytes() {
    let env = support::TestEnv::new();
    let root = env.root("oversize");
    let db = root.join("opencode.db");
    let conn = Connection::open(&db).unwrap();
    conn.execute_batch(support::V1_SCHEMA).unwrap();
    conn.execute(
        "INSERT INTO session (id, parent_id, title, time_created, time_updated) VALUES ('s1', NULL, 'big', 1, 2)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO message (id, session_id, time_created, time_updated, data) VALUES ('m1', 's1', 3, 4, '{}')",
        [],
    )
    .unwrap();
    let big = json!({"type": "tool", "tool": "bash", "state": {"output": "z".repeat(40_000) + support::NEEDLE}});
    conn.execute(
        "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data) VALUES ('p1', 'm1', 's1', 5, 6, ?1)",
        [serde_json::to_string(&big).unwrap()],
    )
    .unwrap();
    drop(conn);
    let configured = support::configured(&env, "oversize", &root);
    let response = search(
        &configured.config,
        json!({
            "query": support::NEEDLE,
            "limit": 10,
            "budget": {"max_bytes_per_source": 8192}
        }),
    );
    assert_eq!(source_status(&response, "oc").status, status::PARTIAL);
    assert_eq!(
        source_status(&response, "oc").code,
        Some(codes::RECORD_SKIPPED)
    );
    assert_eq!(
        detail_value(&response, "oc", "reason").unwrap(),
        &json!("oversize")
    );
    assert!(response.results.is_empty());
    assert_eq!(response.exit_code, 4);
}

#[test]
fn unconfigured_sources_and_pagination() {
    let env = support::TestEnv::new();
    let fixture = support::build_v1_hits(&env, "page", 20, 1);
    let config_value = env.config_value(
        vec![
            env.opencode_source("oc", &fixture.root),
            json!({"source_id": "device", "kind": "device", "root": "C:/device-notes", "enabled": false}),
        ],
        vec!["oc"],
    );
    let config_path = env.write_json("page-config.json", &config_value);

    let named = search(
        &config_path,
        json!({"query": support::NEEDLE, "sources": ["oc", "device"], "limit": 10}),
    );
    assert_eq!(named.results.len(), 10);
    assert_eq!(named.next_offset, None);
    assert_eq!(named.pagination.sources_incomplete, vec!["device"]);
    assert_eq!(named.source_status[0].source_id, "device");
    assert_eq!(named.source_status[0].status, status::UNCONFIGURED);
    assert_eq!(
        named.source_status[0].code,
        Some(codes::SOURCE_NOT_CONFIGURED)
    );
    assert_eq!(source_status(&named, "oc").status, status::OK);
    assert_eq!(named.exit_code, 0);

    let first_page = search(&config_path, json!({"query": support::NEEDLE, "limit": 10}));
    assert_eq!(first_page.next_offset, Some(10));
    assert!(first_page.pagination.sources_incomplete.is_empty());

    let last_page = search(&config_path, json!({"query": support::NEEDLE, "limit": 20}));
    assert_eq!(last_page.results.len(), 20);
    assert_eq!(last_page.next_offset, None);

    let beyond = search(
        &config_path,
        json!({"query": support::NEEDLE, "limit": 10, "offset": 100}),
    );
    assert!(beyond.results.is_empty());
    assert_eq!(beyond.next_offset, None);

    let unknown = search(&config_path, json!({"query": support::NEEDLE, "sources": ["nope"]}));
    assert!(unknown.results.is_empty());
    assert_eq!(unknown.exit_code, 3);
    assert_eq!(unknown.pagination.sources_incomplete, vec!["nope"]);
}

#[test]
fn one_empty_source_keeps_pagination_alive() {
    let env = support::TestEnv::new();
    let fixture = support::build_v1_hits(&env, "page-mixed", 20, 1);

    // A second, valid but empty OpenCode source.
    let empty_root = env.root("page-empty");
    let conn = Connection::open(empty_root.join("opencode.db")).unwrap();
    conn.execute_batch(support::V1_SCHEMA).unwrap();
    drop(conn);

    let config_value = env.config_value(
        vec![
            env.opencode_source("oc_a", &fixture.root),
            env.opencode_source("oc_b", &empty_root),
        ],
        vec!["oc_b", "oc_a"],
    );
    let config_path = env.write_json("page-mixed.json", &config_value);
    let response = search(
        &config_path,
        json!({"query": support::NEEDLE, "sources": ["oc_a", "oc_b"], "limit": 10}),
    );
    assert_eq!(response.results.len(), 10);
    assert!(response.pagination.sources_incomplete.is_empty());
    assert_eq!(response.next_offset, Some(10));
    assert_eq!(source_status(&response, "oc_b").status, status::EMPTY);
    assert_eq!(source_status(&response, "oc_a").status, status::OK);
    assert!(response.errors.is_empty());
    assert_eq!(response.exit_code, 0);
}

#[test]
fn empty_results_and_missing_terms() {
    let env = support::TestEnv::new();
    let fixture = support::build_v1(&env);
    let configured = support::configured(&env, "emptyresult", &fixture.root);

    let empty = search(&configured.config, json!({"query": "zzznotthere"}));
    assert!(empty.results.is_empty());
    assert_eq!(source_status(&empty, "oc").status, status::EMPTY);
    assert!(empty.pagination.sources_incomplete.is_empty());
    assert_eq!(empty.exit_code, 0);

    let invalid = search(&configured.config, json!({"query": "!!!"}));
    assert_eq!(invalid.exit_code, 2);
    assert_eq!(invalid.errors.len(), 1);
    assert_eq!(invalid.errors[0].code, codes::INVALID_REQUEST);
    assert!(invalid.source_status.is_empty());
    assert!(invalid.pagination.sources_incomplete.is_empty());
    assert_eq!(invalid.next_offset, None);
}

#[test]
fn adapters_report_real_statuses_instead_of_a_placeholder() {
    let env = support::TestEnv::new();

    // A KB root without `.meta/scripts/ask.py` is an explicit unavailable
    // source, never an empty result and never a silent success.
    let kb_root = env.root("kb-root");
    let config_value = env.config_value(vec![env.md_source("kb", "kb", &kb_root)], vec!["kb"]);
    let config_path = env.write_json("kb-config.json", &config_value);
    let response = search(&config_path, json!({"query": "anything"}));
    assert_eq!(source_status(&response, "kb").status, status::UNAVAILABLE);
    assert_eq!(
        source_status(&response, "kb").code,
        Some(codes::SOURCE_ROOT_INVALID)
    );
    assert_eq!(
        detail_value(&response, "kb", "reason").unwrap(),
        &json!("ask_script_missing")
    );
    assert_eq!(response.exit_code, 4);
    assert!(response.results.is_empty());

    // A device root with no whitelisted event is a healthy empty source.
    let device_root = env.root("device-root");
    env.write("device-root/README.md", b"not an event");
    let config_value =
        env.config_value(vec![env.md_source("device", "device", &device_root)], vec!["device"]);
    let config_path = env.write_json("device-config.json", &config_value);
    let response = search(&config_path, json!({"query": "anything"}));
    assert_eq!(source_status(&response, "device").status, status::EMPTY);
    assert_eq!(source_status(&response, "device").code, None);
    assert_eq!(response.exit_code, 0);
    assert!(response.pagination.sources_incomplete.is_empty());
}

#[test]
#[ignore = "scale fixture (1000 sessions / 100000 parts); run explicitly"]
fn scale_fixture_full_scan_within_latency_target() {
    let env = support::TestEnv::new();
    let fixture = support::build_v1_scale(&env);
    let configured = support::configured(&env, "scale", &fixture.root);
    let config = load_config(&configured.config).unwrap();

    // The contrast budget must be smaller than the fixture, otherwise the
    // "truncated" expectation would be unreachable (B4).
    let data_bytes = support::v1_data_bytes(&fixture.db);
    eprintln!("scale fixture data bytes: {data_bytes}");
    assert!(
        data_bytes > 33_554_432,
        "scale fixture must exceed the 32 MiB contrast budget, got {data_bytes}"
    );

    let mut timings = Vec::new();
    let mut response = None;
    for _ in 0..3 {
        let request = SearchRequest::parse(&json!({
            "query": support::NEEDLE,
            "limit": 10,
            "budget": {
                "max_bytes_per_source": 67108864,
                "max_elapsed_ms_per_source": 600000
            }
        }))
        .unwrap();
        let started = std::time::Instant::now();
        let result = memory_search(&config, &request);
        timings.push(started.elapsed());
        response = Some(result);
    }
    let response = response.unwrap();
    let coverage = detail_value(&response, "oc", "coverage").unwrap();
    assert_eq!(
        coverage["scanned_sessions"].as_u64().unwrap(),
        coverage["total_sessions"].as_u64().unwrap()
    );
    assert_eq!(coverage["total_sessions"], 1_000);
    assert_eq!(source_status(&response, "oc").status, status::OK);
    let worst = timings.iter().max().unwrap();
    eprintln!("scale scan timings: {timings:?}");
    assert!(
        worst.as_secs_f64() <= 10.0,
        "full scan must stay under 10s, worst was {worst:?}"
    );

    // 32 MiB contrast: the fixture is larger than the budget, so the scan must
    // stop early and say so.
    let contrast = memory_search(
        &config,
        &SearchRequest::parse(&json!({
            "query": support::NEEDLE,
            "limit": 10,
            "budget": {
                "max_bytes_per_source": 33554432,
                "max_elapsed_ms_per_source": 600000
            }
        }))
        .unwrap(),
    );
    assert_eq!(source_status(&contrast, "oc").status, status::TRUNCATED);
    assert_eq!(
        source_status(&contrast, "oc").code,
        Some(codes::BUDGET_BYTES)
    );
    let contrast_coverage = detail_value(&contrast, "oc", "coverage").unwrap();
    assert!(
        contrast_coverage["scanned_sessions"].as_u64().unwrap()
            < contrast_coverage["total_sessions"].as_u64().unwrap(),
        "32 MiB contrast must scan fewer sessions than the full corpus"
    );
    assert_eq!(
        detail_value(&contrast, "oc", "budget").unwrap()["reason"],
        codes::BUDGET_BYTES
    );
    assert_eq!(contrast.next_offset, None);
}

#[test]
fn search_envelope_has_exactly_the_frozen_keys() {
    let env = support::TestEnv::new();
    let fixture = support::build_v1(&env);
    let configured = support::configured(&env, "envelope", &fixture.root);
    let response = search(
        &configured.config,
        json!({"query": support::NEEDLE, "limit": 5}),
    );
    let value = serde_json::to_value(&response).unwrap();
    let keys: std::collections::BTreeSet<&str> = value
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        [
            "contract_version",
            "results",
            "next_offset",
            "pagination",
            "source_status",
            "errors",
            "warnings",
            "side_effects"
        ]
        .into_iter()
        .collect::<std::collections::BTreeSet<_>>()
    );

    let result = &value["results"][0];
    let result_keys: std::collections::BTreeSet<&str> = result
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        result_keys,
        [
            "hit_id",
            "source_id",
            "source_kind",
            "title",
            "snippet",
            "snippet_truncated",
            "score",
            "score_kind",
            "rank_basis",
            "source_rank",
            "ref"
        ]
        .into_iter()
        .collect::<std::collections::BTreeSet<_>>()
    );
    let reference = &result["ref"];
    let ref_keys: std::collections::BTreeSet<&str> = reference
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        ref_keys,
        [
            "source_id",
            "source_kind",
            "root_sha256",
            "rel_path",
            "schema_gen",
            "locator",
            "fingerprint",
            "text_range"
        ]
        .into_iter()
        .collect::<std::collections::BTreeSet<_>>()
    );
    let range_keys: std::collections::BTreeSet<&str> = reference["text_range"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        range_keys,
        ["line_start", "line_end", "byte_start", "byte_end"]
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>()
    );

    let status = &value["source_status"][0];
    for key in status.as_object().unwrap().keys() {
        assert!(
            ["source_id", "status", "code", "detail"].contains(&key.as_str()),
            "unexpected source_status key {key}"
        );
    }
    let empty = search(&configured.config, json!({"query": "zzznotthere"}));
    let empty_value = serde_json::to_value(&empty).unwrap();
    assert_eq!(
        empty_value["source_status"][0]
            .as_object()
            .unwrap()
            .keys()
            .collect::<Vec<_>>(),
        vec!["source_id", "status", "detail"]
    );
    assert!(empty_value["errors"].as_array().unwrap().is_empty());
    assert_eq!(empty_value["pagination"]["sources_incomplete"], json!([]));
    assert_eq!(empty_value["next_offset"], json!(null));
}

#[test]
fn explicit_db_path_sets_rel_path_and_is_used() {
    let env = support::TestEnv::new();
    let fixture = support::build_v1(&env);
    let custom = fixture.root.join("nested");
    std::fs::create_dir_all(&custom).unwrap();
    std::fs::rename(&fixture.db, custom.join("db.sqlite")).unwrap();

    let config_value = env.config_value(
        vec![json!({
            "source_id": "oc",
            "kind": "opencode",
            "root": fixture.root.to_string_lossy(),
            "db": custom.join("db.sqlite").to_string_lossy(),
            "enabled": true,
        })],
        vec!["oc"],
    );
    let config_path = env.write_json("custom-db.json", &config_value);
    let response = search(
        &config_path,
        json!({"query": support::NEEDLE, "limit": 50}),
    );
    assert_eq!(source_status(&response, "oc").status, status::OK);
    assert!(!response.results.is_empty());
    for result in &response.results {
        assert_eq!(result.reference.rel_path, "nested/db.sqlite");
    }
}

// ---------------------------------------------------------------------------
// `session_id` filtering (V1 / V2 / legacy)
// ---------------------------------------------------------------------------

fn filtered_ids(response: &SearchResponse, session_id: &str) -> Vec<String> {
    let mut ids: Vec<String> = response
        .results
        .iter()
        .filter(|hit| hit.reference.locator.session_id.as_deref() == Some(session_id))
        .map(|hit| hit.hit_id.clone())
        .collect();
    ids.sort();
    ids
}

#[test]
fn session_filter_applies_to_every_generation() {
    let env = support::TestEnv::new();

    // V1
    let v1 = support::build_v1(&env);
    let v1_config = env.write_json(
        "filter-v1.json",
        &env.config_value(vec![env.opencode_source("oc", &v1.root)], vec!["oc"]),
    );
    let response = search(
        &v1_config,
        json!({"query": support::NEEDLE, "limit": 100, "session_id": "s-main"}),
    );
    assert_eq!(source_status(&response, "oc").status, status::OK);
    assert!(!response.results.is_empty());
    assert!(response
        .results
        .iter()
        .all(|hit| hit.reference.locator.session_id.as_deref() == Some("s-main")));
    let coverage = detail_value(&response, "oc", "coverage").expect("coverage");
    assert_eq!(coverage["total_sessions"], json!(1));
    assert_eq!(coverage["scanned_sessions"], json!(1));
    assert_eq!(coverage["session_filter"], json!("s-main"));

    // V2
    let v2 = support::build_v2(&env);
    let v2_config = env.write_json(
        "filter-v2.json",
        &env.config_value(vec![env.opencode_source("oc", &v2.root)], vec!["oc"]),
    );
    let response = search(
        &v2_config,
        json!({"query": support::NEEDLE_V2, "limit": 100, "session_id": "v2-main"}),
    );
    assert_eq!(source_status(&response, "oc").status, status::OK);
    assert!(response
        .results
        .iter()
        .all(|hit| hit.reference.locator.session_id.as_deref() == Some("v2-main")));
    assert_eq!(
        detail_value(&response, "oc", "coverage").unwrap()["total_sessions"],
        json!(1)
    );

    // Legacy JSON storage
    let legacy = support::build_legacy(&env);
    let legacy_config = env.write_json(
        "filter-legacy.json",
        &env.config_value(
            vec![env.opencode_source("oc", &legacy.root)],
            vec!["oc"],
        ),
    );
    let response = search(
        &legacy_config,
        json!({"query": support::NEEDLE_LEGACY, "limit": 100, "session_id": "s-main"}),
    );
    // `build_legacy` deliberately keeps one corrupt part inside `s-main`.
    assert_eq!(source_status(&response, "oc").status, status::PARTIAL);
    assert_eq!(
        source_status(&response, "oc").code,
        Some(codes::RECORD_SKIPPED)
    );
    assert!(response
        .results
        .iter()
        .all(|hit| hit.reference.locator.session_id.as_deref() == Some("s-main")));
    assert_eq!(
        detail_value(&response, "oc", "coverage").unwrap()["total_sessions"],
        json!(1)
    );
}

#[test]
fn session_filter_zero_hits_is_empty_and_damage_outside_the_target_is_ignored() {
    let env = support::TestEnv::new();
    let fixture = support::build_v1_damaged(&env);
    {
        let conn = Connection::open(&fixture.db).unwrap();
        support::insert_v1(
            &conn,
            "s-clean",
            None,
            "clean session",
            2_000,
            "m-clean",
            &[(
                "p-clean",
                10,
                json!({"type": "text", "text": format!("{} clean", support::NEEDLE)}),
            )],
        );
    }
    let config = env.write_json(
        "filter-damaged.json",
        &env.config_value(vec![env.opencode_source("oc", &fixture.root)], vec!["oc"]),
    );

    // The damaged rows live in `s-main`; filtering to `s-clean` never reads them.
    let response = search(
        &config,
        json!({"query": support::NEEDLE, "session_id": "s-clean"}),
    );
    assert_eq!(source_status(&response, "oc").status, status::OK);
    assert_eq!(response.results.len(), 1);
    assert!(detail_value(&response, "oc", "CONTENT_UNRESOLVED").is_none());
    assert!(response.errors.is_empty());

    let zero = search(
        &config,
        json!({"query": support::NEEDLE, "session_id": "no-such-session"}),
    );
    assert_eq!(source_status(&zero, "oc").status, status::EMPTY);
    assert!(zero.results.is_empty());
    assert_eq!(zero.exit_code, 0);
    assert!(detail_value(&zero, "oc", "budget").is_none());
    assert_eq!(
        detail_value(&zero, "oc", "coverage").unwrap()["total_sessions"],
        json!(0)
    );
}

#[test]
fn session_filter_does_not_exempt_the_budget_and_keeps_hit_ids() {
    let env = support::TestEnv::new();
    let fixture = support::build_v1_hits(&env, "filter-budget-oc", 3, 100);
    let config = env.write_json(
        "filter-budget-oc.json",
        &env.config_value(vec![env.opencode_source("oc", &fixture.root)], vec!["oc"]),
    );

    let truncated = search(
        &config,
        json!({
            "query": support::NEEDLE,
            "session_id": "s-001",
            "budget": {"max_bytes_per_source": 8192}
        }),
    );
    assert_eq!(source_status(&truncated, "oc").status, status::TRUNCATED);
    assert_eq!(source_status(&truncated, "oc").code, Some(codes::BUDGET_BYTES));
    assert_eq!(
        detail_value(&truncated, "oc", "budget").unwrap()["reason"],
        json!(codes::BUDGET_BYTES)
    );
    assert_eq!(
        detail_value(&truncated, "oc", "coverage").unwrap()["total_sessions"],
        json!(1)
    );

    // Hit identity is compared on a one-page corpus: every id from both runs.
    let small = support::build_v1_hits(&env, "filter-ids-oc", 3, 10);
    let small_config = env.write_json(
        "filter-ids-oc.json",
        &env.config_value(vec![env.opencode_source("oc", &small.root)], vec!["oc"]),
    );
    let unfiltered = search(&small_config, json!({"query": support::NEEDLE, "limit": 100}));
    let filtered = search(
        &small_config,
        json!({"query": support::NEEDLE, "limit": 100, "session_id": "s-001"}),
    );
    assert_eq!(filtered.results.len(), 10);
    assert_eq!(
        filtered_ids(&unfiltered, "s-001"),
        filtered_ids(&filtered, "s-001"),
        "filtering must not change hit identity"
    );
}
