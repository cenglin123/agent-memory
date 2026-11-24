//! P4 acceptance: provable `next` edges, turn assembly and bounded expansion.
//!
//! Every case uses temporary roots and synthetic Codex/Claude JSONL corpora. No
//! real session, knowledge base or production Archive is read, and no embedding
//! model is involved (the lexical path is sufficient for graph semantics).

mod support;

use agent_memory::archive::{Archive, Manifest};
use agent_memory::capture::capture_source;
use agent_memory::recall::config::load_config;
use agent_memory::recall::contract::codes;
use agent_memory::recall::graph::{RELATED_MAX_HOPS, RELATED_NODES_PER_HIT, RELATED_NODES_PER_SOURCE};
use agent_memory::recall::read::{memory_read, ReadRequest, ReadResponse};
use agent_memory::recall::search::{memory_search, SearchRequest, SearchResponse};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

struct Fixture {
    temp: tempfile::TempDir,
    archive: Archive,
    root: PathBuf,
}

fn fixture() -> Fixture {
    let temp = support::stable_tempdir();
    let root = temp.path().join("archive");
    let archive = Archive::init(&root).unwrap();
    Fixture {
        temp,
        archive,
        root,
    }
}

fn write_jsonl(fixture: &Fixture, name: &str, lines: &[String]) -> PathBuf {
    let path = fixture.temp.path().join(name);
    let mut text = String::new();
    for line in lines {
        text.push_str(line);
        text.push('\n');
    }
    std::fs::write(&path, text).unwrap();
    path
}

fn capture(fixture: &Fixture, path: &Path, provider: &str, domain: &str) {
    let report = capture_source(&fixture.archive, path, provider, domain, None).unwrap();
    assert_eq!(report.status, "captured", "{report:?}");
}

fn archive_source(fixture: &Fixture, source_id: &str) -> Value {
    json!({
        "source_id": source_id, "kind": "archive",
        "root": fixture.root.to_string_lossy(), "enabled": true
    })
}

fn v2_config(
    env: &support::TestEnv,
    name: &str,
    sources: Vec<Value>,
    order: Vec<&str>,
    domain: &str,
) -> PathBuf {
    let value = json!({
        "version": 2,
        "sources": sources,
        "source_order": order,
        "interpreter": {"kb": {"command": "C:/python/python.exe"}},
        "domain": domain,
        "telemetry": {"enabled": false}
    });
    assert!(
        agent_memory::recall::RecallConfig::from_value(&value).is_ok(),
        "{:?}",
        agent_memory::recall::RecallConfig::from_value(&value).err()
    );
    env.write_json(name, &value)
}

fn search(config: &Path, request: Value) -> SearchResponse {
    let config = load_config(config).unwrap();
    let request = SearchRequest::parse(&request).unwrap();
    memory_search(&config, &request)
}

fn read(config: &Path, request: Value) -> ReadResponse {
    let config = load_config(config).unwrap();
    let request = ReadRequest::parse(&request).unwrap();
    memory_read(&config, &request)
}

fn archive_detail(response: &SearchResponse, source_id: &str) -> Value {
    response
        .source_status
        .iter()
        .find(|entry| entry.source_id == source_id)
        .and_then(|entry| entry.detail.as_ref())
        .and_then(|detail| detail.get("archive"))
        .cloned()
        .unwrap_or_else(|| panic!("no archive detail for {source_id}"))
}

fn segment_ref(segment: &agent_memory::recall::read::Segment) -> Value {
    serde_json::to_value(
        segment
            .reference
            .as_ref()
            .expect("turn segment carries its own ref"),
    )
    .unwrap()
}

fn titles(hits: &[agent_memory::recall::search::SearchResult]) -> Vec<String> {
    hits.iter().map(|hit| hit.title.clone()).collect()
}

fn joined_segments(response: &ReadResponse) -> String {
    response
        .segments
        .iter()
        .map(|segment| segment.text.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Four turns of two records each; only turn 2 contains `needletarget`.
fn four_turn_lines() -> Vec<String> {
    vec![
        support::codex_meta("s-graph"),
        support::codex_user("turnone 问题"),
        support::codex_assistant("turnone 回答"),
        support::codex_user("turntwo needletarget"),
        support::codex_assistant("turntwo 回答"),
        support::codex_user("turnthree 问题"),
        support::codex_assistant("turnthree 回答"),
        support::codex_user("turnfour 问题"),
        support::codex_assistant("turnfour 回答"),
    ]
}

/// Rewrite the access label of every committed record whose bytes contain
/// `marker`. The object keeps its readable label, so only the record-level
/// predicate can stop the record.
fn set_record_access(fixture: &Fixture, marker: &str, labels: &[&str]) -> usize {
    let dir = fixture.root.join("archive").join("manifests");
    let mut rewritten = 0usize;
    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let mut manifest: Manifest = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let mut changed = false;
        for record in &mut manifest.records {
            let (_, object) = support::read_object_file(&fixture.root, &record.object);
            let slice = &object[record.offset as usize..(record.offset + record.len) as usize];
            if std::str::from_utf8(slice).unwrap().contains(marker) {
                record.access = labels.iter().map(|label| label.to_string()).collect();
                rewritten += 1;
                changed = true;
            }
        }
        if changed {
            std::fs::write(&path, serde_json::to_vec(&manifest).unwrap()).unwrap();
        }
    }
    rewritten
}

// ---------------------------------------------------------------------------
// 1. `context: "turn"` returns the whole turn, in order, each segment readable
// ---------------------------------------------------------------------------

#[test]
fn turn_context_returns_the_whole_turn_and_every_segment_reads_back() {
    let fixture = fixture();
    let lines = vec![
        support::codex_meta("s-turn"),
        support::codex_user("firstquestion 第一轮问题"),
        support::codex_assistant("第一轮回答"),
        support::codex_user("第二轮问题"),
        support::codex_assistant("targetonly 第二轮答案"),
        support::codex_function_call("c1", "shell", "{\"command\":\"echo toolcallonly\"}"),
        support::codex_function_output("c1", json!("工具结果 toolresultonly")),
        support::codex_user("第三轮问题"),
        support::codex_assistant("第三轮回答"),
    ];
    let path = write_jsonl(&fixture, "rollout-turn.jsonl", &lines);
    capture(&fixture, &path, "codex", "default");
    let env = support::TestEnv::new();
    let config = v2_config(
        &env,
        "turn.json",
        vec![archive_source(&fixture, "ar")],
        vec!["ar"],
        "default",
    );

    let page = search(&config, json!({"query": "targetonly"}));
    assert_eq!(page.results.len(), 1, "{:?}", titles(&page.results));
    assert!(page.results[0].title.ends_with("line 5"), "{}", page.results[0].title);
    let as_json = serde_json::to_value(&page).unwrap();
    assert!(as_json["results"][0].get("related").is_none());

    let response = read(
        &config,
        json!({"ref": page.results[0].reference, "context": "turn"}),
    );
    assert!(response.ok, "{:?}", response.code);
    assert_eq!(response.segments.len(), 4, "{:?}", response.segments);
    // Original order: the user record that opened the turn, the answer, the tool
    // call and the tool result; the previous and the next turn stay out.
    let texts: Vec<&str> = response.segments.iter().map(|s| s.text.as_str()).collect();
    assert!(texts[0].contains("第二轮问题"), "{texts:?}");
    assert!(texts[1].contains("targetonly"), "{texts:?}");
    assert!(texts[2].contains("toolcallonly"), "{texts:?}");
    assert!(texts[3].contains("toolresultonly"), "{texts:?}");
    let joined = texts.join("\n");
    assert!(!joined.contains("第一轮"), "{joined}");
    assert!(!joined.contains("第三轮"), "{joined}");
    assert!(!response.truncated);

    // Every segment is a complete record: its own ref reads back to exactly the
    // same bytes and hash.
    for segment in &response.segments {
        let reference = segment_ref(segment);
        let back = read(&config, json!({"ref": reference}));
        assert!(back.ok, "{:?}", back.code);
        assert_eq!(back.text.as_deref(), Some(segment.text.as_str()));
        assert_eq!(back.range_sha256.as_ref(), Some(&segment.range_sha256));
        assert_eq!(
            back.text_range, Some(segment.text_range),
            "a segment ref must address exactly its own text range"
        );
    }
}

// ---------------------------------------------------------------------------
// 2. Budget: the referenced record is kept, the rest is truncated
// ---------------------------------------------------------------------------

#[test]
fn turn_context_keeps_the_target_when_the_byte_budget_is_tight() {
    let fixture = fixture();
    let lines = vec![
        support::codex_meta("s-budget"),
        support::codex_user("firstquestion 第一轮问题"),
        support::codex_assistant("第一轮回答"),
        support::codex_user("第二轮问题"),
        support::codex_assistant("targetonly 第二轮答案"),
        support::codex_user("第三轮问题"),
    ];
    let path = write_jsonl(&fixture, "rollout-turn-budget.jsonl", &lines);
    capture(&fixture, &path, "codex", "default");
    let env = support::TestEnv::new();
    let config = v2_config(
        &env,
        "turn-budget.json",
        vec![archive_source(&fixture, "ar")],
        vec!["ar"],
        "default",
    );

    let page = search(&config, json!({"query": "targetonly"}));
    let hit = &page.results[0];
    let target_len =
        (hit.reference.text_range.byte_end - hit.reference.text_range.byte_start) as usize;
    let response = read(
        &config,
        json!({
            "ref": hit.reference,
            "context": "turn",
            "max_bytes": target_len + 8
        }),
    );
    assert!(response.ok, "{:?}", response.code);
    assert!(response.truncated, "dropping turn records must be reported");
    assert!(
        response
            .segments
            .iter()
            .any(|segment| segment.text.contains("targetonly")),
        "the target record is never pushed out by its context"
    );
    assert!(response.segments.len() < 3, "{:?}", response.segments);
    let total: usize = response.segments.iter().map(|s| s.text.len()).sum();
    assert!(total <= target_len + 8);
}

// ---------------------------------------------------------------------------
// 3. Turn assembly never crosses a physical source (session) boundary
// ---------------------------------------------------------------------------

#[test]
fn turn_context_never_stitches_two_sessions() {
    let fixture = fixture();
    let first = write_jsonl(
        &fixture,
        "rollout-a.jsonl",
        &[
            support::codex_meta("s-a"),
            support::codex_user("targetonly 第一会话问题"),
            support::codex_assistant("第一会话回答"),
        ],
    );
    capture(&fixture, &first, "codex", "default");
    let second = write_jsonl(
        &fixture,
        "rollout-b.jsonl",
        &[
            support::codex_meta("s-b"),
            support::codex_user("SECONDFILEONLY 其他会话"),
            support::codex_assistant("SECONDFILEONLY 其他回答"),
        ],
    );
    capture(&fixture, &second, "codex", "default");
    let env = support::TestEnv::new();
    let config = v2_config(
        &env,
        "turn-sessions.json",
        vec![archive_source(&fixture, "ar")],
        vec!["ar"],
        "default",
    );

    let page = search(&config, json!({"query": "targetonly"}));
    assert_eq!(page.results.len(), 1);
    let response = read(
        &config,
        json!({"ref": page.results[0].reference, "context": "turn"}),
    );
    assert!(response.ok, "{:?}", response.code);
    assert_eq!(response.segments.len(), 2, "{:?}", response.segments);
    let joined = joined_segments(&response);
    assert!(!joined.contains("SECONDFILEONLY"), "{joined}");
    for segment in &response.segments {
        let reference = segment.reference.as_ref().unwrap();
        assert_eq!(reference.source_id, "ar");
        assert!(
            reference.locator.record_sha256.is_some(),
            "every segment is a record ref"
        );
        assert_eq!(reference.locator.projection_offset, Some(0));
    }
}

// ---------------------------------------------------------------------------
// 4. `related_hops` walks 1 and 2 provable `next` edges
// ---------------------------------------------------------------------------

#[test]
fn related_reports_adjacent_turns_for_one_and_two_hops() {
    let fixture = fixture();
    let path = write_jsonl(&fixture, "rollout-graph.jsonl", &four_turn_lines());
    capture(&fixture, &path, "codex", "default");
    let env = support::TestEnv::new();
    let config = v2_config(
        &env,
        "graph-hops.json",
        vec![archive_source(&fixture, "ar")],
        vec!["ar"],
        "default",
    );

    let one = search(&config, json!({"query": "needletarget", "related_hops": 1}));
    assert_eq!(one.results.len(), 1);
    let related = &one.results[0].related;
    let names: Vec<&str> = related.iter().map(|item| item.title.as_str()).collect();
    assert_eq!(related.len(), 3, "{names:?}");
    assert!(names.iter().any(|title| title.ends_with("line 2")), "{names:?}");
    assert!(names.iter().any(|title| title.ends_with("line 5")), "{names:?}");
    assert!(names.iter().any(|title| title.ends_with("line 6")), "{names:?}");
    assert!(!names.iter().any(|title| title.ends_with("line 8")), "{names:?}");
    for item in related {
        assert_eq!(item.edge, "next");
        assert_eq!(item.hops, 1);
        assert_eq!(item.hit_id.len(), 64);
    }

    let two = search(&config, json!({"query": "needletarget", "related_hops": 2}));
    let related = &two.results[0].related;
    let names: Vec<&str> = related.iter().map(|item| item.title.as_str()).collect();
    assert!(
        related
            .iter()
            .any(|item| item.title.ends_with("line 8") && item.hops == 2),
        "{names:?}"
    );
    assert!(related.iter().all(|item| item.hops <= RELATED_MAX_HOPS));
    assert!(!names.iter().any(|title| title.ends_with("line 1")));
    let detail = archive_detail(&two, "ar");
    assert_eq!(detail["related"]["edge"], json!("next"));
    assert_eq!(detail["related"]["hops"], json!(2));
    assert_eq!(detail["related"]["limits"]["maxHops"], json!(RELATED_MAX_HOPS));

    // Every related ref reads back on its own.
    for item in &two.results[0].related {
        let back = read(&config, json!({"ref": item.reference}));
        assert!(back.ok, "{:?}", back.code);
        assert_eq!(
            back.range_sha256.as_ref(),
            Some(&item.reference.fingerprint.range_sha256)
        );
    }
}

#[test]
fn related_hops_outside_one_to_two_are_rejected() {
    for bad in [json!(0), json!(3), json!(-1), json!("2"), json!(1.5)] {
        let error = SearchRequest::parse(&json!({"query": "x", "related_hops": bad}))
            .expect_err("related_hops must be an integer in 1..=2");
        assert_eq!(error.code, codes::INVALID_REQUEST, "{bad}");
    }
    assert!(SearchRequest::parse(&json!({"query": "x", "related_hops": 1})).is_ok());
    assert!(SearchRequest::parse(&json!({"query": "x", "related_hops": 2})).is_ok());
}

// ---------------------------------------------------------------------------
// 5. Node budgets: per hit and per source
// ---------------------------------------------------------------------------

#[test]
fn related_expansion_is_bounded_per_hit() {
    let fixture = fixture();
    let lines = vec![
        support::codex_meta("s-cap"),
        support::codex_user("turnone 问题"),
        support::codex_assistant("turnone 回答"),
        support::codex_user("turntwo 问题"),
        support::codex_assistant("turntwo 记录a"),
        support::codex_assistant("turntwo 记录b"),
        support::codex_assistant("turntwo 记录c"),
        support::codex_assistant("needletarget turntwo 目标"),
        support::codex_assistant("turntwo 记录d"),
        support::codex_assistant("turntwo 记录e"),
        support::codex_assistant("turntwo 记录f"),
        support::codex_user("turnthree 问题"),
        support::codex_assistant("turnthree 回答"),
        support::codex_user("turnfour 问题"),
        support::codex_assistant("turnfour 回答"),
    ];
    let path = write_jsonl(&fixture, "rollout-cap.jsonl", &lines);
    capture(&fixture, &path, "codex", "default");
    let env = support::TestEnv::new();
    let config = v2_config(
        &env,
        "graph-cap.json",
        vec![archive_source(&fixture, "ar")],
        vec!["ar"],
        "default",
    );

    let page = search(&config, json!({"query": "needletarget", "related_hops": 2}));
    assert_eq!(page.results.len(), 1);
    assert!(
        page.results[0].related.len() <= RELATED_NODES_PER_HIT,
        "{:?}",
        page.results[0].related
    );
    let detail = archive_detail(&page, "ar");
    assert!(
        detail["related"]["truncated"].as_u64().unwrap() >= 1,
        "per-hit truncation must be visible: {detail}"
    );
}

#[test]
fn related_expansion_is_bounded_per_source() {
    let fixture = fixture();
    let mut lines = vec![support::codex_meta("s-source-cap")];
    for turn in 0..10 {
        lines.push(support::codex_user(&format!("needlealpha turn {turn} 问题")));
        lines.push(support::codex_assistant(&format!("turn {turn} 回答")));
    }
    let path = write_jsonl(&fixture, "rollout-source-cap.jsonl", &lines);
    capture(&fixture, &path, "codex", "default");
    let env = support::TestEnv::new();
    let config = v2_config(
        &env,
        "graph-source-cap.json",
        vec![archive_source(&fixture, "ar")],
        vec!["ar"],
        "default",
    );

    let page = search(
        &config,
        json!({"query": "needlealpha", "limit": 20, "related_hops": 1}),
    );
    assert_eq!(page.results.len(), 10);
    let total: usize = page
        .results
        .iter()
        .map(|result| result.related.len())
        .sum();
    assert_eq!(
        total, RELATED_NODES_PER_SOURCE,
        "the per-source node budget caps the whole expansion"
    );
    let detail = archive_detail(&page, "ar");
    assert!(detail["related"]["truncated"].as_u64().unwrap() >= 1, "{detail}");
}

// ---------------------------------------------------------------------------
// 6. Expansion is additive: ordering, scores and the gate do not move
// ---------------------------------------------------------------------------

#[test]
fn related_does_not_change_the_main_result_order_or_the_gate() {
    let fixture = fixture();
    let mut lines = vec![support::codex_meta("s-order")];
    for turn in 0..5 {
        lines.push(support::codex_user(&format!("needlealpha turn {turn} 问题")));
        lines.push(support::codex_assistant(&format!("needlealpha turn {turn} 回答")));
    }
    let path = write_jsonl(&fixture, "rollout-order.jsonl", &lines);
    capture(&fixture, &path, "codex", "default");
    let env = support::TestEnv::new();
    let config = v2_config(
        &env,
        "graph-order.json",
        vec![archive_source(&fixture, "ar")],
        vec!["ar"],
        "default",
    );

    let plain = search(&config, json!({"query": "needlealpha", "limit": 20}));
    let expanded = search(
        &config,
        json!({"query": "needlealpha", "limit": 20, "related_hops": 2}),
    );
    assert_eq!(plain.results.len(), expanded.results.len());
    for (left, right) in plain.results.iter().zip(expanded.results.iter()) {
        assert_eq!(left.hit_id, right.hit_id);
        assert_eq!(left.score, right.score);
        assert_eq!(left.rank_basis, right.rank_basis);
        assert_eq!(left.source_rank, right.source_rank);
        assert_eq!(left.snippet, right.snippet);
    }
    assert!(expanded
        .results
        .iter()
        .any(|result| !result.related.is_empty()));

    let mut plain_detail = archive_detail(&plain, "ar");
    let mut expanded_detail = archive_detail(&expanded, "ar");
    plain_detail.as_object_mut().unwrap().remove("related");
    expanded_detail.as_object_mut().unwrap().remove("related");
    assert_eq!(plain_detail, expanded_detail, "counters and gate are unchanged");
}

// ---------------------------------------------------------------------------
// 7. Default-off response keeps exactly the P3 shape
// ---------------------------------------------------------------------------

#[test]
fn related_off_keeps_the_p3_response_shape() {
    let fixture = fixture();
    let path = write_jsonl(&fixture, "rollout-shape.jsonl", &four_turn_lines());
    capture(&fixture, &path, "codex", "default");
    let env = support::TestEnv::new();
    let config = v2_config(
        &env,
        "graph-shape.json",
        vec![archive_source(&fixture, "ar")],
        vec!["ar"],
        "default",
    );

    let page = search(&config, json!({"query": "needletarget"}));
    let value = serde_json::to_value(&page).unwrap();
    let text = serde_json::to_string(&page).unwrap();
    assert!(!text.contains("\"related\""), "no related key without opt-in");
    let result_keys: std::collections::BTreeSet<&str> = value["results"][0]
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
            "ref",
        ]
        .into_iter()
        .collect::<std::collections::BTreeSet<_>>()
    );
    let detail = value["source_status"][0]["detail"]["archive"]
        .as_object()
        .unwrap();
    assert_eq!(
        detail
            .keys()
            .map(String::as_str)
            .collect::<std::collections::BTreeSet<_>>(),
        [
            "records",
            "searched",
            "denied",
            "unreachable",
            "unmapped",
            "thresholdsVersion",
            "session_mapping",
        ]
        .into_iter()
        .collect::<std::collections::BTreeSet<_>>(),
        "the P3 detail key set is unchanged when related is off"
    );

    // The read envelope keeps its P3 keys unless `context: "turn"` is requested.
    let reference = value["results"][0]["ref"].clone();
    let back = serde_json::to_value(read(&config, json!({"ref": reference}))).unwrap();
    assert!(back["segments"][0].get("ref").is_none());
}

// ---------------------------------------------------------------------------
// 8. Different sources never link, even with identical content
// ---------------------------------------------------------------------------

#[test]
fn identical_content_in_two_archive_roots_never_links_across_sources() {
    let first = fixture();
    let second = fixture();
    let lines = four_turn_lines();
    let a = write_jsonl(&first, "rollout-copy.jsonl", &lines);
    capture(&first, &a, "codex", "default");
    let b = write_jsonl(&second, "rollout-copy.jsonl", &lines);
    capture(&second, &b, "codex", "default");
    let env = support::TestEnv::new();
    let config = v2_config(
        &env,
        "graph-roots.json",
        vec![
            archive_source(&first, "archive-a"),
            archive_source(&second, "archive-b"),
        ],
        vec!["archive-a", "archive-b"],
        "default",
    );

    let page = search(&config, json!({"query": "needletarget", "related_hops": 2}));
    assert_eq!(page.results.len(), 2, "{:?}", titles(&page.results));
    let hits: std::collections::BTreeSet<&str> =
        page.results.iter().map(|hit| hit.hit_id.as_str()).collect();
    assert_eq!(hits.len(), 2, "the source id is part of hit identity");
    for result in &page.results {
        assert!(!result.related.is_empty(), "{}", result.source_id);
        for item in &result.related {
            assert_eq!(item.reference.source_id, result.source_id);
            assert_eq!(item.reference.root_sha256, result.reference.root_sha256);
            assert_eq!(item.reference.source_kind, result.reference.source_kind);
        }
    }
}

#[test]
fn a_live_source_never_reports_related() {
    let fixture = fixture();
    let path = write_jsonl(&fixture, "rollout-live.jsonl", &four_turn_lines());
    capture(&fixture, &path, "codex", "default");
    let env = support::TestEnv::new();
    let notes = env.root("graph-notes");
    env.write(
        "graph-notes/2026-10-21_notes.md",
        b"needletarget device note\n",
    );
    let config = v2_config(
        &env,
        "graph-live.json",
        vec![
            archive_source(&fixture, "ar"),
            env.md_source("notes", "device", &notes),
        ],
        vec!["ar", "notes"],
        "default",
    );

    let page = search(&config, json!({"query": "needletarget", "related_hops": 2}));
    assert!(page.results.iter().any(|hit| hit.source_kind == "archive"));
    assert!(page.results.iter().any(|hit| hit.source_kind == "device"));
    for result in &page.results {
        if result.source_kind == "device" {
            assert!(result.related.is_empty());
        }
    }
}

// ---------------------------------------------------------------------------
// 9. Subagent / sidechain sessions never appear through expansion or assembly
// ---------------------------------------------------------------------------

#[test]
fn a_codex_subagent_session_is_never_expanded() {
    let fixture = fixture();
    let main = write_jsonl(
        &fixture,
        "rollout-main.jsonl",
        &[
            support::codex_meta("s-main"),
            support::codex_user("needleparent 主问题"),
            support::codex_assistant("needleparent 主回答"),
        ],
    );
    capture(&fixture, &main, "codex", "default");
    let subagent = write_jsonl(
        &fixture,
        "rollout-subagent.jsonl",
        &[
            json!({
                "type": "session_meta",
                "payload": {
                    "id": "s-sub",
                    "cwd": "C:/synthetic",
                    "source": {"subagent": {"thread_spawn": {"parent_thread_id": "s-main"}}}
                }
            })
            .to_string(),
            support::codex_user("subagentonly 子代理问题"),
            support::codex_assistant("subagentonly 子代理回答"),
        ],
    );
    capture(&fixture, &subagent, "codex", "default");
    let env = support::TestEnv::new();
    let config = v2_config(
        &env,
        "graph-subagent.json",
        vec![archive_source(&fixture, "ar")],
        vec!["ar"],
        "default",
    );

    let parent = search(&config, json!({"query": "needleparent", "related_hops": 2}));
    assert_eq!(parent.results.len(), 2);
    for result in &parent.results {
        for item in &result.related {
            assert!(
                item.title.contains("rollout-main.jsonl"),
                "a subagent session must never be related: {}",
                item.title
            );
        }
    }

    let child = search(&config, json!({"query": "subagentonly", "related_hops": 2}));
    assert_eq!(child.results.len(), 2);
    for result in &child.results {
        assert!(
            result.related.is_empty(),
            "a subagent session is never expanded: {:?}",
            result.related
        );
    }
    let detail = archive_detail(&child, "ar");
    assert!(
        detail["related"]["sidechain"].as_u64().unwrap() >= 1,
        "the refusal is counted, not silent: {detail}"
    );
}

#[test]
fn a_claude_sidechain_record_never_joins_its_parent_turn() {
    let fixture = fixture();
    let lines = vec![
        json!({"type": "user", "message": {"role": "user", "content": "needleparent 主问题"}})
            .to_string(),
        json!({"type": "assistant", "message": {"role": "assistant", "content": [
            {"type": "text", "text": "主回答 needleparent"}
        ]}})
        .to_string(),
        json!({
            "type": "user",
            "isSidechain": true,
            "message": {"role": "user", "content": "subagentonly 子代理问题"}
        })
        .to_string(),
        json!({"type": "assistant", "message": {"role": "assistant", "content": [
            {"type": "text", "text": "主回答二 needleparent"}
        ]}})
        .to_string(),
    ];
    let path = write_jsonl(&fixture, "rollout-sidechain.jsonl", &lines);
    capture(&fixture, &path, "claude", "default");
    let env = support::TestEnv::new();
    let config = v2_config(
        &env,
        "graph-sidechain.json",
        vec![archive_source(&fixture, "ar")],
        vec!["ar"],
        "default",
    );

    // The parent turn is assembled without the sidechain record.
    let parent = search(&config, json!({"query": "needleparent"}));
    let first = parent
        .results
        .iter()
        .find(|result| result.title.ends_with("line 1"))
        .expect("opening user message");
    let assembled = read(&config, json!({"ref": first.reference, "context": "turn"}));
    assert!(assembled.ok, "{:?}", assembled.code);
    let joined = joined_segments(&assembled);
    assert!(!joined.contains("subagentonly"), "{joined}");
    assert!(
        assembled.truncated,
        "the invisible record is reported as a cut"
    );

    // The sidechain record itself is readable, but never expanded.
    let sidechain = search(&config, json!({"query": "subagentonly", "related_hops": 2}));
    let hit = sidechain
        .results
        .iter()
        .find(|result| result.title.ends_with("line 3"))
        .expect("sidechain hit");
    assert!(hit.related.is_empty());
    let detail = archive_detail(&sidechain, "ar");
    assert!(
        detail["related"]["sidechain"].as_u64().unwrap() >= 1,
        "{detail}"
    );
    let only_target = read(&config, json!({"ref": hit.reference, "context": "turn"}));
    assert!(only_target.ok);
    assert_eq!(only_target.segments.len(), 1);
    assert!(only_target.truncated);
}

// ---------------------------------------------------------------------------
// 10. Cross-domain: denied records never appear, and the omission is counted
// ---------------------------------------------------------------------------

#[test]
fn a_record_denied_by_its_own_label_never_appears_in_context() {
    let fixture = fixture();
    let lines = vec![
        support::codex_meta("s-denied"),
        support::codex_user("firstquestion 第一轮问题"),
        support::codex_assistant("第一轮回答"),
        support::codex_user("第二轮问题"),
        support::codex_assistant("DENIEDNEIGHBOUR 第二轮中间回答"),
        support::codex_assistant("targetonly 第二轮答案"),
        support::codex_user("第三轮问题"),
    ];
    let path = write_jsonl(&fixture, "rollout-denied.jsonl", &lines);
    capture(&fixture, &path, "codex", "default");
    assert_eq!(set_record_access(&fixture, "DENIEDNEIGHBOUR", &["work"]), 1);
    let env = support::TestEnv::new();
    let config = v2_config(
        &env,
        "graph-denied.json",
        vec![archive_source(&fixture, "ar")],
        vec!["ar"],
        "default",
    );

    let page = search(&config, json!({"query": "targetonly"}));
    assert_eq!(page.results.len(), 1);
    let reference = page.results[0].reference.clone();

    // N2 regression: a `context_before` neighbour denied by its own label is
    // omitted, never leaked through the object-level intersection. The window
    // mode keeps its P3 semantics (a best-effort neighbour is dropped without a
    // new status); turn assembly is the mode that reports the cut.
    let window = read(&config, json!({"ref": reference, "context_before": 2}));
    assert!(window.ok, "{:?}", window.code);
    let text = joined_segments(&window);
    assert!(!text.contains("DENIEDNEIGHBOUR"), "{text}");
    assert!(text.contains("第二轮问题"), "{text}");
    assert!(text.contains("targetonly"));

    // The same record is a hard boundary for turn assembly.
    let turn = read(&config, json!({"ref": reference, "context": "turn"}));
    assert!(turn.ok, "{:?}", turn.code);
    let text = joined_segments(&turn);
    assert!(!text.contains("DENIEDNEIGHBOUR"), "{text}");
    assert!(!text.contains("第二轮问题"), "{text}");
    assert!(turn.truncated);
}

#[test]
fn related_never_reports_a_denied_record_and_counts_the_omission() {
    let fixture = fixture();
    let lines = vec![
        support::codex_meta("s-related-denied"),
        support::codex_user("turnone 问题"),
        support::codex_assistant("turnone 回答"),
        support::codex_user("turntwo 问题"),
        support::codex_assistant("DENIEDNEIGHBOUR turntwo 中间记录"),
        support::codex_assistant("needletarget turntwo 目标"),
        support::codex_user("turnthree 问题"),
        support::codex_assistant("turnthree 回答"),
    ];
    let path = write_jsonl(&fixture, "rollout-related-denied.jsonl", &lines);
    capture(&fixture, &path, "codex", "default");
    assert_eq!(set_record_access(&fixture, "DENIEDNEIGHBOUR", &["work"]), 1);
    let env = support::TestEnv::new();
    let config = v2_config(
        &env,
        "graph-related-denied.json",
        vec![archive_source(&fixture, "ar")],
        vec!["ar"],
        "default",
    );

    let page = search(&config, json!({"query": "needletarget", "related_hops": 2}));
    assert_eq!(page.results.len(), 1);
    let names: Vec<String> = page.results[0]
        .related
        .iter()
        .map(|item| item.title.clone())
        .collect();
    assert!(
        !names.iter().any(|title| title.ends_with("line 5")),
        "the denied record must not be related: {names:?}"
    );
    let detail = archive_detail(&page, "ar");
    assert!(
        detail["related"]["denied"].as_u64().unwrap() >= 1,
        "the blocked edge is counted: {detail}"
    );
    // The readable neighbouring turn still links; only the denied side is cut.
    assert!(
        names.iter().any(|title| title.ends_with("line 2")),
        "{names:?}"
    );
}

// ---------------------------------------------------------------------------
// 11. `context` validation and non-archive sources
// ---------------------------------------------------------------------------

#[test]
fn turn_context_still_verifies_the_reference_range() {
    let fixture = fixture();
    let path = write_jsonl(&fixture, "rollout-turn-verify.jsonl", &four_turn_lines());
    capture(&fixture, &path, "codex", "default");
    let env = support::TestEnv::new();
    let config = v2_config(
        &env,
        "turn-verify.json",
        vec![archive_source(&fixture, "ar")],
        vec!["ar"],
        "default",
    );

    let page = search(&config, json!({"query": "needletarget"}));
    let mut reference = serde_json::to_value(&page.results[0].reference).unwrap();
    reference["fingerprint"]["range_sha256"] = json!("0".repeat(64));
    let response = read(&config, json!({"ref": reference, "context": "turn"}));
    assert!(!response.ok);
    assert_eq!(response.code, Some(codes::REF_STALE));

    // An empty range is refused as well, never widened into the whole record.
    let mut reference = serde_json::to_value(&page.results[0].reference).unwrap();
    reference["locator"]["projection_offset"] = json!(1);
    reference["text_range"]["byte_start"] = json!(1);
    reference["text_range"]["byte_end"] = json!(1);
    reference["fingerprint"]["range_sha256"] = json!(agent_memory::archive::hex_sha256(b""));
    let response = read(&config, json!({"ref": reference, "context": "turn"}));
    assert!(!response.ok);
    assert_eq!(response.code, Some(codes::REF_OUT_OF_RANGE));
}

#[test]
fn turn_context_is_archive_only_and_rejects_conflicting_keys() {
    let reference = json!({
        "source_id": "ar",
        "source_kind": "archive",
        "root_sha256": "0".repeat(64),
        "rel_path": format!("archive/objects/aa/bb/{}", "a".repeat(64)),
        "schema_gen": "archive-record-v1",
        "locator": {"record_sha256": "a".repeat(64), "projection_offset": 0},
        "fingerprint": {"range_sha256": "b".repeat(64)},
        "text_range": {"line_start": 0, "line_end": 1, "byte_start": 0, "byte_end": 1}
    });
    let error = ReadRequest::parse(&json!({"ref": reference, "context": "window"}))
        .expect_err("only \"turn\" is a valid context value");
    assert_eq!(error.code, codes::INVALID_REQUEST);
    let error = ReadRequest::parse(&json!({
        "ref": reference,
        "context": "turn",
        "context_before": 1
    }))
    .expect_err("turn assembly and a record window cannot be combined");
    assert_eq!(error.code, codes::INVALID_REQUEST);
    assert!(ReadRequest::parse(&json!({"ref": reference, "context": "turn"})).is_ok());

    // A non-archive source has no turn assembly: the request fails explicitly
    // instead of silently returning a record window.
    let env = support::TestEnv::new();
    let notes = env.root("graph-device-turn");
    env.write(
        "graph-device-turn/2026-10-21_note.md",
        b"needlealpha note\n",
    );
    let config = v2_config(
        &env,
        "graph-device-turn.json",
        vec![env.md_source("notes", "device", &notes)],
        vec!["notes"],
        "default",
    );
    let page = search(&config, json!({"query": "needlealpha"}));
    assert_eq!(page.results.len(), 1);
    let response = read(
        &config,
        json!({"ref": page.results[0].reference, "context": "turn"}),
    );
    assert!(!response.ok);
    assert_eq!(response.code, Some(codes::INVALID_REQUEST));
}
