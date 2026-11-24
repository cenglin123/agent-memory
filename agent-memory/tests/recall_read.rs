//! Readback tests: exact ranges, context windows, multi-segment parts, byte
//! budgets, stale detection and fail-closed REF_* errors.

mod support;

use agent_memory::recall::config::load_config;
use agent_memory::recall::contract::{codes, WARN_FILE_STALE};
use agent_memory::recall::hash::{root_sha256, sha256_hex, TextRange};
use agent_memory::recall::read::{memory_read, ReadRequest, ReadResponse};
use agent_memory::recall::search::{memory_search, SearchRequest, SearchResult};
use agent_memory::recall::snapshot::{classify_changes, Change, ChangeKind, Window};
use rusqlite::Connection;
use serde_json::{json, Value};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

fn read(
    config_path: &Path,
    reference: Value,
    context: (i64, i64),
    max_bytes: usize,
) -> ReadResponse {
    let config = load_config(config_path).expect("config");
    let request = ReadRequest::parse(&json!({
        "ref": reference,
        "context_before": context.0,
        "context_after": context.1,
        "max_bytes": max_bytes,
    }))
    .expect("read request");
    memory_read(&config, &request)
}

fn text_ref(
    root: &Path,
    source_id: &str,
    kind: &str,
    rel_path: &str,
    file_bytes: &[u8],
    start: usize,
    end: usize,
) -> Value {
    let range = TextRange::from_span(file_bytes, start, end);
    json!({
        "source_id": source_id,
        "source_kind": kind,
        "root_sha256": root_sha256(&root.to_string_lossy()),
        "rel_path": rel_path,
        "schema_gen": "md-v1",
        "locator": {},
        "fingerprint": {
            "range_sha256": sha256_hex(&file_bytes[start..end]),
            "file_sha256": sha256_hex(file_bytes),
        },
        "text_range": range,
    })
}

fn docs_ref(root: &Path, rel_path: &str, file_bytes: &[u8], start: usize, end: usize) -> Value {
    let mut reference = text_ref(root, "docs", "docs", rel_path, file_bytes, start, end);
    reference["schema_gen"] = json!("docs-md-v1");
    reference["locator"] = json!({"pointer": format!("/chunk/{start}")});
    reference
}

fn search_hit(config_path: &Path, query: &str) -> SearchResult {
    let config = load_config(config_path).expect("config");
    let request = SearchRequest::parse(&json!({"query": query, "limit": 50})).expect("request");
    let response = memory_search(&config, &request);
    assert!(
        !response.results.is_empty(),
        "query {query} must produce a hit (status {})",
        response.source_status[0].status
    );
    response.results.into_iter().next().unwrap()
}

fn ref_json(result: &SearchResult) -> Value {
    serde_json::to_value(&result.reference).expect("ref json")
}

#[test]
fn text_ref_reads_exact_range_and_convenience_fields() {
    let env = support::TestEnv::new();
    let root = env.root("notes");
    let content = "alpha\r\nbeta 中文\r\ngamma\r\ndelta";
    env.write("notes/2026-10-25_event.md", content.as_bytes());
    let config = env.write_json(
        "notes-config.json",
        &env.config_value(vec![env.md_source("notes", "device", &root)], vec!["notes"]),
    );

    let start = content.find("beta").unwrap();
    let reference = text_ref(
        &root,
        "notes",
        "device",
        "2026-10-25_event.md",
        content.as_bytes(),
        start,
        start + 4,
    );
    let response = read(&config, reference.clone(), (0, 0), 8192);
    assert!(response.ok, "{:?}", response.code);
    assert_eq!(response.exit_code, 0);
    assert_eq!(response.segments.len(), 1);
    assert_eq!(response.text.as_deref(), Some("beta"));
    assert_eq!(response.segments[0].text, "beta");
    assert_eq!(response.text_range, Some(response.segments[0].text_range));
    assert_eq!(
        response.range_sha256.as_deref(),
        Some(response.segments[0].range_sha256.as_str())
    );
    assert_eq!(
        response.segments[0].text_range,
        TextRange::new(start as u64, (start + 4) as u64, 1, 2)
    );
    assert!(!response.truncated);
    assert!(response.warnings.is_empty());
    for effect in &response.side_effects {
        assert_eq!(effect.level, "C");
        assert_eq!(effect.note, Some("window_only"));
    }
}

#[test]
fn context_expands_by_whole_lines_and_roundtrips() {
    let env = support::TestEnv::new();
    let root = env.root("notes-ctx");
    let content = "alpha\r\nbeta 中文\r\ngamma\r\ndelta";
    env.write("notes-ctx/2026-10-25_event.md", content.as_bytes());
    let config = env.write_json(
        "ctx-config.json",
        &env.config_value(vec![env.md_source("notes", "device", &root)], vec!["notes"]),
    );
    let start = content.find("beta").unwrap();
    let reference = text_ref(
        &root,
        "notes",
        "device",
        "2026-10-25_event.md",
        content.as_bytes(),
        start,
        start + 4,
    );

    let response = read(&config, reference, (1, 1), 8192);
    assert!(response.ok, "{:?}", response.code);
    let expected = "alpha\r\nbeta 中文\r\ngamma\r\n";
    assert_eq!(response.text.as_deref(), Some(expected));
    let range = response.text_range.unwrap();
    assert_eq!((range.line_start, range.line_end), (0, 3));
    assert_eq!(range.byte_end - range.byte_start, expected.len() as u64);

    // Compose a directly readable ref from the returned segment.
    let mut composed = json!({
        "source_id": "notes",
        "source_kind": "device",
        "root_sha256": root_sha256(&root.to_string_lossy()),
        "rel_path": "2026-10-25_event.md",
        "schema_gen": "md-v1",
        "locator": {},
        "fingerprint": {"range_sha256": response.range_sha256.clone().unwrap()},
        "text_range": serde_json::to_value(range).unwrap(),
    });
    composed["fingerprint"]["file_sha256"] = json!(sha256_hex(content.as_bytes()));
    let again = read(&config, composed, (0, 0), 8192);
    assert!(again.ok);
    assert_eq!(again.text.as_deref(), Some(expected));
}

#[test]
fn max_bytes_truncates_and_recomputes_the_range() {
    let env = support::TestEnv::new();
    let root = env.root("notes-max");
    let content = "abcdefghij中文";
    env.write("notes-max/2026-10-24_event.md", content.as_bytes());
    let config = env.write_json(
        "max-config.json",
        &env.config_value(vec![env.md_source("notes", "device", &root)], vec!["notes"]),
    );
    let reference = text_ref(
        &root,
        "notes",
        "device",
        "2026-10-24_event.md",
        content.as_bytes(),
        0,
        content.len(),
    );

    let response = read(&config, reference, (0, 0), 11);
    assert!(response.ok, "{:?}", response.code);
    assert!(response.truncated);
    assert_eq!(response.text.as_deref(), Some("abcdefghij"));
    let range = response.text_range.unwrap();
    assert_eq!(range.byte_start, 0);
    assert_eq!(range.byte_end, 10);
    assert_eq!(
        response.range_sha256.as_deref(),
        Some(sha256_hex(b"abcdefghij").as_str())
    );

    // The narrowed range is itself readable.
    let composed = json!({
        "source_id": "notes",
        "source_kind": "device",
        "root_sha256": root_sha256(&root.to_string_lossy()),
        "rel_path": "2026-10-24_event.md",
        "schema_gen": "md-v1",
        "locator": {},
        "fingerprint": {"range_sha256": response.range_sha256.clone().unwrap()},
        "text_range": serde_json::to_value(range).unwrap(),
    });
    let again = read(&config, composed, (0, 0), 8192);
    assert!(again.ok);
    assert_eq!(again.text.as_deref(), Some("abcdefghij"));
}

#[test]
fn range_changes_are_stale_and_file_changes_only_warn() {
    let env = support::TestEnv::new();
    let root = env.root("notes-stale");
    let content = "first line\nsecond line\nthird line";
    env.write("notes-stale/2026-10-23_event.md", content.as_bytes());
    let config = env.write_json(
        "stale-config.json",
        &env.config_value(vec![env.md_source("notes", "device", &root)], vec!["notes"]),
    );
    let start = content.find("second").unwrap();
    let reference = text_ref(
        &root,
        "notes",
        "device",
        "2026-10-23_event.md",
        content.as_bytes(),
        start,
        start + 6,
    );

    // Change bytes outside the range: readback still succeeds, warning only.
    let appended = format!("{content}\nappended");
    env.write("notes-stale/2026-10-23_event.md", appended.as_bytes());
    let response = read(&config, reference.clone(), (0, 0), 8192);
    assert!(response.ok, "{:?}", response.code);
    assert_eq!(response.text.as_deref(), Some("second"));
    assert_eq!(response.warnings.len(), 1);
    assert_eq!(response.warnings[0].code, WARN_FILE_STALE);
    assert_eq!(response.exit_code, 0);

    // Change bytes inside the range: fail closed.
    let mutated = content.replace("second", "SECOND");
    env.write("notes-stale/2026-10-23_event.md", mutated.as_bytes());
    let stale = read(&config, reference, (0, 0), 8192);
    assert!(!stale.ok);
    assert_eq!(stale.code, Some(codes::REF_STALE));
    assert_eq!(stale.exit_code, 5);
    assert!(stale.segments.is_empty());
}

#[test]
fn ref_validation_is_fail_closed() {
    let env = support::TestEnv::new();
    let root = env.root("notes-ref");
    let content = "one\ntwo\nthree";
    env.write("notes-ref/2026-10-22_event.md", content.as_bytes());
    let config = env.write_json(
        "ref-config.json",
        &env.config_value(vec![env.md_source("notes", "device", &root)], vec!["notes"]),
    );
    let start = content.find("two").unwrap();
    let valid = text_ref(
        &root,
        "notes",
        "device",
        "2026-10-22_event.md",
        content.as_bytes(),
        start,
        start + 3,
    );

    let mut wrong_root = valid.clone();
    wrong_root["root_sha256"] = json!("0".repeat(64));
    assert_eq!(
        read(&config, wrong_root, (0, 0), 8192).code,
        Some(codes::REF_ROOT_MISMATCH)
    );

    let mut wrong_source = valid.clone();
    wrong_source["source_id"] = json!("other");
    assert_eq!(
        read(&config, wrong_source, (0, 0), 8192).code,
        Some(codes::REF_SOURCE_MISMATCH)
    );

    let mut wrong_kind = valid.clone();
    wrong_kind["source_kind"] = json!("kb");
    assert_eq!(
        read(&config, wrong_kind, (0, 0), 8192).code,
        Some(codes::REF_SOURCE_MISMATCH)
    );

    let mut wrong_schema = valid.clone();
    wrong_schema["schema_gen"] = json!("sqlite-v1");
    wrong_schema["locator"] = json!({"session_id": "s", "message_id": "m", "part_id": "p"});
    assert_eq!(
        read(&config, wrong_schema, (0, 0), 8192).code,
        Some(codes::REF_SOURCE_MISMATCH)
    );

    let mut out_of_range = valid.clone();
    out_of_range["text_range"]["byte_end"] = json!(content.len() as u64 + 10);
    assert_eq!(
        read(&config, out_of_range, (0, 0), 8192).code,
        Some(codes::REF_OUT_OF_RANGE)
    );

    let mut missing = valid.clone();
    missing["rel_path"] = json!("2026-01-01_missing.md");
    assert_eq!(
        read(&config, missing, (0, 0), 8192).code,
        Some(codes::REF_LOCATOR_MISSING)
    );

    let mut traversal = valid;
    traversal["rel_path"] = json!("../escape.md");
    let escaped = read(&config, traversal, (0, 0), 8192);
    assert_eq!(escaped.code, Some(codes::REF_LOCATOR_MISSING));
    assert_eq!(escaped.exit_code, 5);
}

#[test]
fn opencode_v1_ref_reads_the_full_projection() {
    let env = support::TestEnv::new();
    let fixture = support::build_v1(&env);
    let configured = support::configured(&env, "readv1", &fixture.root);
    let hit = search_hit(&configured.config, "endoutput");
    let range = hit.reference.text_range;

    let response = read(&configured.config, ref_json(&hit), (0, 0), 1_048_576);
    assert!(response.ok, "{:?}", response.code);
    assert_eq!(response.segments.len(), 1);
    let text = response.text.clone().unwrap();
    assert!(text.starts_with("bash\n"));
    assert!(text.contains("endoutput"));
    assert_eq!(text.len() as u64, range.byte_len());
    assert_eq!(
        response.text_range.unwrap(),
        TextRange::from_span(text.as_bytes(), 0, text.len())
    );
    assert_eq!(
        response.range_sha256.as_deref(),
        Some(sha256_hex(text.as_bytes()).as_str())
    );
    assert_eq!(response.segments[0].locator, hit.reference.locator);
}

#[test]
fn opencode_row_change_warns_and_projection_change_is_stale() {
    let env = support::TestEnv::new();
    let fixture = support::build_v1(&env);
    let configured = support::configured(&env, "readv1stale", &fixture.root);
    let hit = search_hit(&configured.config, "endoutput");
    let reference = ref_json(&hit);

    // Add a key that does not participate in the projection: row hash changes,
    // range stays byte-identical -> warning only.
    let conn = Connection::open(&fixture.db).unwrap();
    let data: String = conn
        .query_row("SELECT data FROM part WHERE id = 'p-tool'", [], |row| {
            row.get(0)
        })
        .unwrap();
    let mut value: Value = serde_json::from_str(&data).unwrap();
    value["extra_flag"] = json!(true);
    conn.execute(
        "UPDATE part SET data = ?1 WHERE id = 'p-tool'",
        [serde_json::to_string(&value).unwrap()],
    )
    .unwrap();
    drop(conn);

    let warned = read(&configured.config, reference.clone(), (0, 0), 1_048_576);
    assert!(warned.ok, "{:?}", warned.code);
    assert_eq!(warned.warnings.len(), 1);
    assert_eq!(warned.warnings[0].code, WARN_FILE_STALE);
    assert_eq!(warned.warnings[0].scope, "ref");

    // Change the projection itself: fail closed.
    let conn = Connection::open(&fixture.db).unwrap();
    let data: String = conn
        .query_row("SELECT data FROM part WHERE id = 'p-tool'", [], |row| {
            row.get(0)
        })
        .unwrap();
    let mut value: Value = serde_json::from_str(&data).unwrap();
    let output = value["state"]["output"].as_str().unwrap().to_string();
    value["state"]["output"] = json!(format!("PREFIX {output}"));
    conn.execute(
        "UPDATE part SET data = ?1 WHERE id = 'p-tool'",
        [serde_json::to_string(&value).unwrap()],
    )
    .unwrap();
    drop(conn);

    let stale = read(&configured.config, reference, (0, 0), 1_048_576);
    assert!(!stale.ok);
    assert_eq!(stale.code, Some(codes::REF_STALE));
    assert_eq!(stale.exit_code, 5);
}

#[test]
fn opencode_ref_rel_path_must_match_the_resolved_carrier() {
    let env = support::TestEnv::new();

    // SQLite V1: the carrier is the configured db.
    let v1 = support::build_v1(&env);
    let v1_config = support::configured(&env, "relpath-v1", &v1.root);
    let v1_hit = search_hit(&v1_config.config, support::NEEDLE);
    assert!(read(&v1_config.config, ref_json(&v1_hit), (0, 0), 8192).ok);
    let mut tampered = ref_json(&v1_hit);
    tampered["rel_path"] = json!("storage/part/m1/p1.json");
    let response = read(&v1_config.config, tampered, (0, 0), 8192);
    assert_eq!(response.code, Some(codes::REF_LOCATOR_MISSING));
    assert_eq!(response.exit_code, 5);
    assert!(response.segments.is_empty());

    // SQLite V2: same rule with the V2 db.
    let v2 = support::build_v2(&env);
    let v2_config = support::configured(&env, "relpath-v2", &v2.root);
    let v2_hit = search_hit(&v2_config.config, support::NEEDLE_V2);
    assert!(read(&v2_config.config, ref_json(&v2_hit), (0, 0), 8192).ok);
    let mut tampered = ref_json(&v2_hit);
    tampered["rel_path"] = json!("nested/db.sqlite");
    assert_eq!(
        read(&v2_config.config, tampered, (0, 0), 8192).code,
        Some(codes::REF_LOCATOR_MISSING)
    );

    // Legacy JSON: the carrier is the part file derived from the locator.
    let legacy = support::build_legacy(&env);
    let legacy_config = support::configured(&env, "relpath-json", &legacy.root);
    let legacy_hit = search_hit(&legacy_config.config, "unicode");
    assert!(read(&legacy_config.config, ref_json(&legacy_hit), (0, 0), 8192).ok);
    for wrong in [
        "storage/part/m1/p2.json",
        "storage/part/m1/does-not-exist.json",
        "../escape.json",
    ] {
        let mut tampered = ref_json(&legacy_hit);
        tampered["rel_path"] = json!(wrong);
        let response = read(&legacy_config.config, tampered, (0, 0), 8192);
        assert_eq!(
            response.code,
            Some(codes::REF_LOCATOR_MISSING),
            "rel_path {wrong} must be rejected"
        );
    }
}

#[test]
fn opencode_context_returns_one_segment_per_part() {
    let env = support::TestEnv::new();
    let fixture = support::build_v1(&env);
    let configured = support::configured(&env, "readctx", &fixture.root);
    let hit = search_hit(&configured.config, "midneedle");

    let response = read(&configured.config, ref_json(&hit), (1, 1), 1_048_576);
    assert!(response.ok, "{:?}", response.code);
    assert_eq!(response.segments.len(), 3, "one part before and after");
    assert!(
        response.text.is_none(),
        "multi-segment responses must omit convenience text"
    );
    assert!(response.text_range.is_none());
    assert!(response.range_sha256.is_none());

    let locators: Vec<String> = response
        .segments
        .iter()
        .map(|segment| serde_json::to_string(&segment.locator).unwrap())
        .collect();
    let unique: std::collections::BTreeSet<&String> = locators.iter().collect();
    assert_eq!(unique.len(), 3);

    let serialized = serde_json::to_value(&response).unwrap();
    let keys: std::collections::BTreeSet<&str> = serialized
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        ["ok", "segments", "truncated", "warnings", "side_effects"]
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>()
    );

    for segment in &response.segments {
        assert_eq!(segment.range_sha256, sha256_hex(segment.text.as_bytes()));
        assert_eq!(
            segment.text_range,
            TextRange::from_span(segment.text.as_bytes(), 0, segment.text.len())
        );
        // Each segment composes a directly readable ref.
        let composed = json!({
            "source_id": hit.reference.source_id,
            "source_kind": hit.reference.source_kind,
            "root_sha256": hit.reference.root_sha256,
            "rel_path": hit.reference.rel_path,
            "schema_gen": hit.reference.schema_gen,
            "locator": serde_json::to_value(&segment.locator).unwrap(),
            "fingerprint": {
                "range_sha256": segment.range_sha256.clone(),
                "row_sha256": hit.reference.fingerprint.row_sha256,
            },
            "text_range": serde_json::to_value(segment.text_range).unwrap(),
        });
        let single = read(&configured.config, composed, (0, 0), 1_048_576);
        assert!(single.ok, "segment must be readable: {:?}", single.code);
        assert_eq!(single.segments.len(), 1);
        assert_eq!(single.text.as_deref(), Some(segment.text.as_str()));
    }
}

#[test]
fn max_bytes_across_segments_narrows_the_last_one() {
    let env = support::TestEnv::new();
    let fixture = support::build_v1(&env);
    let configured = support::configured(&env, "readmax", &fixture.root);
    let hit = search_hit(&configured.config, "midneedle");

    let previous = read(&configured.config, ref_json(&hit), (1, 0), 1_048_576);
    assert_eq!(previous.segments.len(), 2);
    let first_len = previous.segments[0].text.len();
    let budget = first_len + 100;

    let response = read(&configured.config, ref_json(&hit), (1, 0), budget);
    assert!(response.ok, "{:?}", response.code);
    assert!(response.truncated);
    assert_eq!(response.segments.len(), 2);
    let last = &response.segments[1];
    assert_eq!(last.text.len(), 100);
    assert_eq!(
        last.text_range,
        TextRange::from_span(last.text.as_bytes(), 0, 100)
    );
    assert_eq!(last.range_sha256, sha256_hex(last.text.as_bytes()));
    assert!(
        response.text.is_none(),
        "two segments -> no convenience keys"
    );
}

#[test]
fn v2_pointer_refs_are_readable() {
    let env = support::TestEnv::new();
    let fixture = support::build_v2(&env);
    let configured = support::configured(&env, "readv2", &fixture.root);
    let config = load_config(&configured.config).unwrap();
    let response = memory_search(
        &config,
        &SearchRequest::parse(&json!({"query": support::NEEDLE_V2, "limit": 50})).unwrap(),
    );
    assert_eq!(response.results.len(), 3);

    for result in &response.results {
        let pointer = result.reference.locator.pointer.clone().unwrap();
        let readback = read(&configured.config, ref_json(result), (0, 0), 1_048_576);
        assert!(readback.ok, "{pointer}: {:?}", readback.code);
        let text = readback.text.unwrap();
        if pointer == "/text" {
            assert!(text.starts_with("text branch"));
        } else if pointer == "/content/1" {
            assert!(text.starts_with("grep\n"));
        } else {
            assert!(text.starts_with("v2 body"));
        }
        assert_eq!(
            readback.segments[0].locator.pointer.as_deref(),
            Some(pointer.as_str())
        );
    }
}

#[test]
fn legacy_json_refs_preserve_escapes_and_crlf() {
    let env = support::TestEnv::new();
    let fixture = support::build_legacy(&env);
    let configured = support::configured(&env, "readjson", &fixture.root);

    let escaped = search_hit(&configured.config, "unicode");
    assert_eq!(escaped.reference.schema_gen, "json-legacy-v1");
    let response = read(&configured.config, ref_json(&escaped), (0, 0), 1_048_576);
    assert!(response.ok, "{:?}", response.code);
    let text = response.text.unwrap();
    assert!(text.contains("quote \" backslash \\ unicode 中"));
    assert_eq!(escaped.reference.rel_path, "storage/part/m1/p1.json");

    let crlf = search_hit(&configured.config, "needlecrlf");
    let crlf_read = read(&configured.config, ref_json(&crlf), (0, 0), 1_048_576);
    assert!(crlf_read.ok, "{:?}", crlf_read.code);
    assert_eq!(
        crlf_read.text.as_deref(),
        Some("first\r\nsecond needlecrlf\r\nthird")
    );

    let mut missing = ref_json(&crlf);
    missing["locator"]["part_id"] = json!("nope");
    let missing = read(&configured.config, missing, (0, 0), 8192);
    assert_eq!(missing.code, Some(codes::REF_LOCATOR_MISSING));
}

#[test]
fn read_envelope_has_exactly_the_frozen_keys() {
    let env = support::TestEnv::new();
    let root = env.root("notes-keys");
    let content = "alpha\nbeta\ngamma";
    env.write("notes-keys/2026-10-21_event.md", content.as_bytes());
    let config = env.write_json(
        "keys-config.json",
        &env.config_value(vec![env.md_source("notes", "device", &root)], vec!["notes"]),
    );
    let start = content.find("beta").unwrap();
    let reference = text_ref(
        &root,
        "notes",
        "device",
        "2026-10-21_event.md",
        content.as_bytes(),
        start,
        start + 4,
    );

    let single = serde_json::to_value(read(&config, reference.clone(), (0, 0), 8192)).unwrap();
    let keys: std::collections::BTreeSet<&str> = single
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        [
            "ok",
            "segments",
            "text",
            "text_range",
            "range_sha256",
            "truncated",
            "warnings",
            "side_effects"
        ]
        .into_iter()
        .collect::<std::collections::BTreeSet<_>>()
    );
    let segment_keys: std::collections::BTreeSet<&str> = single["segments"][0]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        segment_keys,
        ["locator", "text_range", "range_sha256", "text"]
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>()
    );

    // Failure shape: ok + code, no text fields.
    let mut stale = reference;
    stale["fingerprint"]["range_sha256"] = json!("0".repeat(64));
    let failed = serde_json::to_value(read(&config, stale, (0, 0), 8192)).unwrap();
    let keys: std::collections::BTreeSet<&str> = failed
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        [
            "ok",
            "code",
            "segments",
            "truncated",
            "warnings",
            "side_effects"
        ]
        .into_iter()
        .collect::<std::collections::BTreeSet<_>>()
    );
    assert_eq!(failed["ok"], json!(false));
    assert_eq!(failed["code"], codes::REF_STALE);
    assert_eq!(failed["segments"], json!([]));
}

#[test]
fn side_effect_classifier_distinguishes_runtime_artifacts() {
    let runtime = vec![Change {
        rel_path: "opencode.db-wal".into(),
        kind: ChangeKind::Added,
    }];
    let (effects, violations) = classify_changes(&runtime);
    assert_eq!(effects.len(), 1);
    assert_eq!(effects[0].level, "C");
    assert_eq!(effects[0].note, Some("window_only"));
    assert!(violations.is_empty());

    // A brand-new file of unknown provenance is a window change, not a
    // violation (the core never writes).
    let added = vec![Change {
        rel_path: "intruder.tmp".into(),
        kind: ChangeKind::Added,
    }];
    let (effects, violations) = classify_changes(&added);
    assert_eq!(effects.len(), 1);
    assert_eq!(effects[0].note, Some("window_only"));
    assert!(violations.is_empty());

    let telemetry = vec![Change {
        rel_path: ".meta/usage-log.jsonl".into(),
        kind: ChangeKind::Modified,
    }];
    let (effects, violations) = classify_changes(&telemetry);
    assert_eq!(effects.len(), 1);
    assert_eq!(violations, vec![".meta/usage-log.jsonl"]);

    let body = vec![Change {
        rel_path: "storage/part/m1/p1.json".into(),
        kind: ChangeKind::Modified,
    }];
    let (_, violations) = classify_changes(&body);
    assert_eq!(violations.len(), 1);

    let removed = vec![Change {
        rel_path: "storage/part/m1/p2.json".into(),
        kind: ChangeKind::Removed,
    }];
    let (_, violations) = classify_changes(&removed);
    assert_eq!(violations.len(), 1);
}

#[test]
fn snapshot_window_reports_real_changes_without_guessing_the_writer() {
    let env = support::TestEnv::new();
    let root = env.root("observed");
    std::fs::write(root.join("note.md"), b"original").unwrap();

    // Unknown new file + runtime artifact: C-level window changes only.
    let window = Window::open(&root);
    std::fs::write(root.join("intruder.tmp"), b"x").unwrap();
    std::fs::write(root.join("opencode.db-wal"), b"x").unwrap();
    let (effects, violations) = window.close();
    assert_eq!(effects.len(), 2);
    for effect in &effects {
        assert_eq!(effect.level, "C");
        assert_eq!(effect.note, Some("window_only"));
    }
    assert!(violations.is_empty());

    // Known A/B telemetry path: contract violation.
    let window = Window::open(&root);
    std::fs::create_dir_all(root.join(".meta")).unwrap();
    std::fs::write(root.join(".meta/usage-log.jsonl"), b"{}\n").unwrap();
    let (effects, violations) = window.close();
    assert_eq!(effects.len(), 1);
    assert_eq!(violations, vec![".meta/usage-log.jsonl"]);

    // A pre-existing carrier changing under a read-only window: violation.
    let window = Window::open(&root);
    std::fs::write(root.join("note.md"), b"changed").unwrap();
    let (_, violations) = window.close();
    assert_eq!(violations, vec!["note.md"]);
}

#[test]
fn kb_stub_fixture_covers_the_envelope_branches() {
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/recall/kb_stub/ask.py");
    let source = std::fs::read_to_string(&path).expect("kb_stub fixture");
    for marker in [
        "contract_version",
        "degraded",
        "LOCAL_UNAVAILABLE",
        "timeout",
        "no-version",
        "bad-version",
        "mode-missing",
    ] {
        assert!(source.contains(marker), "kb_stub must cover {marker}");
    }
}

#[test]
fn memory_read_reports_a_non_empty_window() {
    let env = support::TestEnv::new();
    let root = env.root("window-read");
    // Large enough that the read (range + file hash) outlives several writer
    // iterations; the 64 MiB block hashing is in the read path, not the test.
    let content = vec![b'a'; 16 * 1024 * 1024];
    env.write("window-read/2026-10-19_event.md", &content);
    let config = env.write_json(
        "window-config.json",
        &env.config_value(vec![env.md_source("notes", "device", &root)], vec!["notes"]),
    );
    let reference = text_ref(
        &root,
        "notes",
        "device",
        "2026-10-19_event.md",
        &content,
        0,
        content.len(),
    );

    let stop = Arc::new(AtomicBool::new(false));
    let writer = {
        let stop = stop.clone();
        let root = root.clone();
        std::thread::spawn(move || {
            let mut index = 0usize;
            while !stop.load(Ordering::Relaxed) {
                let _ = std::fs::write(root.join(format!("intruder-{index}.tmp")), b"x");
                index += 1;
                std::thread::sleep(Duration::from_millis(1));
            }
        })
    };
    let response = read(&config, reference, (0, 0), 1024);
    stop.store(true, Ordering::Relaxed);
    writer.join().unwrap();

    assert!(
        response.ok,
        "code={:?} effects={:?}",
        response.code, response.side_effects
    );
    assert!(
        !response.side_effects.is_empty(),
        "a file created during the read window must be reported"
    );
    for effect in &response.side_effects {
        assert_eq!(effect.level, "C");
        assert_eq!(effect.note, Some("window_only"));
    }
}

#[test]
fn memory_read_fails_closed_when_a_known_ab_path_changes_in_the_window() {
    let env = support::TestEnv::new();
    let root = env.root("window-ab");
    let content = vec![b'a'; 16 * 1024 * 1024];
    env.write("window-ab/2026-10-18_event.md", &content);
    std::fs::create_dir_all(root.join(".meta")).unwrap();
    let config = env.write_json(
        "ab-config.json",
        &env.config_value(vec![env.md_source("notes", "device", &root)], vec!["notes"]),
    );
    let reference = text_ref(
        &root,
        "notes",
        "device",
        "2026-10-18_event.md",
        &content,
        0,
        content.len(),
    );

    let stop = Arc::new(AtomicBool::new(false));
    let writer = {
        let stop = stop.clone();
        let path = root.join(".meta/escalation-stats.jsonl");
        std::thread::spawn(move || {
            use std::io::Write;
            let mut index = 0usize;
            while !stop.load(Ordering::Relaxed) {
                let mut file = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&path)
                    .unwrap();
                let _ = writeln!(file, "{{\"n\":{index}}}");
                index += 1;
                std::thread::sleep(Duration::from_millis(1));
            }
        })
    };
    let response = read(&config, reference, (0, 0), 1024);
    stop.store(true, Ordering::Relaxed);
    writer.join().unwrap();

    assert!(!response.ok);
    assert_eq!(response.code, Some(codes::SIDE_EFFECT_VIOLATION));
    assert!(response.segments.is_empty());
    assert!(response
        .side_effects
        .iter()
        .any(|effect| effect.target.as_deref() == Some(".meta/escalation-stats.jsonl")));
}

#[test]
fn text_read_rejects_traversal_and_absolute_paths_outside_the_root() {
    let env = support::TestEnv::new();
    let root = env.root("boundary/nested/root");
    let outside = env.write("outside.md", b"outside secret");
    let outside_bytes = std::fs::read(&outside).unwrap();
    let config = env.write_json(
        "outside-config.json",
        &env.config_value(vec![env.md_source("notes", "device", &root)], vec!["notes"]),
    );

    for rel_path in [
        "..\\..\\..\\outside.md".to_string(),
        outside.to_string_lossy().to_string(),
    ] {
        let reference = text_ref(
            &root,
            "notes",
            "device",
            &rel_path,
            &outside_bytes,
            0,
            outside_bytes.len(),
        );
        let response = read(&config, reference, (0, 0), 8192);
        assert!(!response.ok, "{rel_path} must not be read");
        assert_eq!(response.code, Some(codes::REF_LOCATOR_MISSING));
        assert!(response.segments.is_empty());
    }
}

#[test]
fn docs_readback_uses_the_same_admission_rules_as_search() {
    let env = support::TestEnv::new();
    let root = env.root("docs-admission");
    let allowed = "# Allowed\nallowedneedle\n";
    let excluded = "# Excluded\nexcludedneedle\n";
    let hidden = "# Hidden\nhiddenneedle\n";
    env.write("docs-admission/allowed.md", allowed.as_bytes());
    env.write("docs-admission/excluded.md", excluded.as_bytes());
    env.write("docs-admission/.hidden/hidden.md", hidden.as_bytes());
    let source = json!({
        "source_id": "docs",
        "kind": "docs",
        "root": root.to_string_lossy(),
        "enabled": true,
        "include": ["**/*.md"],
        "exclude": ["excluded.md"],
    });
    let config = env.write_json(
        "docs-admission-config.json",
        &env.config_value(vec![source], vec!["docs"]),
    );

    for (rel_path, body, needle) in [
        ("excluded.md", excluded, "excludedneedle"),
        (".hidden/hidden.md", hidden, "hiddenneedle"),
    ] {
        let loaded = load_config(&config).unwrap();
        let search = memory_search(
            &loaded,
            &SearchRequest::parse(&json!({
                "query": needle,
                "mode": "exact",
                "sources": ["docs"],
                "limit": 10,
            }))
            .unwrap(),
        );
        assert!(search.results.is_empty(), "search admitted {rel_path}");

        let reference = docs_ref(&root, rel_path, body.as_bytes(), 0, body.len());
        let response = read(&config, reference, (0, 0), 8192);
        assert!(!response.ok, "readback admitted {rel_path}");
        assert_eq!(response.code, Some(codes::REF_LOCATOR_MISSING));
    }

    let reference = docs_ref(&root, "allowed.md", allowed.as_bytes(), 0, allowed.len());
    let response = read(&config, reference, (0, 0), 8192);
    assert!(
        response.ok,
        "normal docs file was rejected: {:?}",
        response.code
    );
    assert_eq!(response.text.as_deref(), Some(allowed));
}

#[cfg(windows)]
#[test]
fn text_read_rejects_a_directory_junction_escape() {
    let env = support::TestEnv::new();
    let root = env.root("junction-root");
    let outside = env.root("junction-outside");
    let body = b"junction secret";
    std::fs::write(outside.join("escape.md"), body).unwrap();
    let output = std::process::Command::new("cmd")
        .args(["/c", "mklink", "/J"])
        .arg(root.join("linked"))
        .arg(&outside)
        .output()
        .expect("run cmd mklink /J");
    if !output.status.success() {
        // Some Windows environments disable junction creation. Skip loudly rather
        // than pretending that the boundary was exercised.
        eprintln!(
            "SKIP junction escape: mklink /J failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }

    let config = env.write_json(
        "junction-config.json",
        &env.config_value(vec![env.md_source("notes", "device", &root)], vec!["notes"]),
    );
    let reference = text_ref(
        &root,
        "notes",
        "device",
        "linked/escape.md",
        body,
        0,
        body.len(),
    );
    let response = read(&config, reference, (0, 0), 8192);
    assert!(!response.ok, "junction target outside the root was read");
    assert_eq!(response.code, Some(codes::REF_LOCATOR_MISSING));
}

#[test]
fn context_budget_never_drops_the_target_range() {
    let env = support::TestEnv::new();
    let root = env.root("target-priority");
    let content = format!("{}\n{}", "p".repeat(1022), "t".repeat(45));
    env.write("target-priority/note.md", content.as_bytes());
    let config = env.write_json(
        "target-priority-config.json",
        &env.config_value(vec![env.md_source("notes", "device", &root)], vec!["notes"]),
    );
    let start = 1023;
    let end = 1068;
    let reference = text_ref(
        &root,
        "notes",
        "device",
        "note.md",
        content.as_bytes(),
        start,
        end,
    );
    let response = read(&config, reference, (6, 0), 64);

    assert!(response.ok, "{:?}", response.code);
    assert!(response.truncated);
    let range = response.text_range.unwrap();
    assert!(range.byte_start <= start as u64, "{range:?}");
    assert!(range.byte_end >= end as u64, "{range:?}");
    assert!(response.text.as_deref().unwrap().contains(&"t".repeat(45)));
}

#[test]
fn docs_context_budget_never_drops_the_target_range() {
    let env = support::TestEnv::new();
    let root = env.root("docs-target-priority");
    let content = format!("{}\n{}", "p".repeat(1022), "t".repeat(45));
    env.write("docs-target-priority/note.md", content.as_bytes());
    let source = json!({
        "source_id": "docs",
        "kind": "docs",
        "root": root.to_string_lossy(),
        "enabled": true,
    });
    let config = env.write_json(
        "docs-target-priority-config.json",
        &env.config_value(vec![source], vec!["docs"]),
    );
    let start = 1023;
    let end = 1068;
    let reference = docs_ref(&root, "note.md", content.as_bytes(), start, end);
    // The audited reproduction: a docs chunk read with `max_bytes=64` and
    // `context_before=6` used to return `[0, 64)` and drop `[1023, 1068)`.
    let response = read(&config, reference, (6, 0), 64);

    assert!(response.ok, "{:?}", response.code);
    assert!(response.truncated);
    let range = response.text_range.unwrap();
    assert!(range.byte_start <= start as u64, "{range:?}");
    assert!(range.byte_end >= end as u64, "{range:?}");
    assert!(response.text.as_deref().unwrap().contains(&"t".repeat(45)));
}

#[test]
fn context_budget_splits_the_remainder_and_preserves_the_middle_target() {
    let env = support::TestEnv::new();
    let root = env.root("target-middle");
    let before = "before line with a long payload\n".repeat(8);
    let target = "TARGET-0123456789";
    let after = "after line with a long payload\n".repeat(8);
    let content = format!("{before}{target}{after}");
    env.write("target-middle/note.md", content.as_bytes());
    let config = env.write_json(
        "target-middle-config.json",
        &env.config_value(vec![env.md_source("notes", "device", &root)], vec!["notes"]),
    );
    let start = before.len();
    let end = start + target.len();
    let reference = text_ref(
        &root,
        "notes",
        "device",
        "note.md",
        content.as_bytes(),
        start,
        end,
    );
    let response = read(&config, reference, (8, 8), 64);

    assert!(response.ok, "{:?}", response.code);
    assert!(response.truncated);
    let range = response.text_range.unwrap();
    assert!(
        range.byte_start > 0,
        "leading context must be trimmed: {range:?}"
    );
    assert!(range.byte_start <= start as u64 && range.byte_end >= end as u64);
    assert!(
        range.byte_end < content.len() as u64,
        "trailing context must be trimmed"
    );
    assert!(response.text.as_deref().unwrap().contains(target));
}

#[test]
fn an_oversized_target_is_truncated_from_its_start_at_a_utf8_boundary() {
    let env = support::TestEnv::new();
    let root = env.root("target-utf8");
    let content = "prefix\n中文目标abcdef\nsuffix";
    env.write("target-utf8/note.md", content.as_bytes());
    let config = env.write_json(
        "target-utf8-config.json",
        &env.config_value(vec![env.md_source("notes", "device", &root)], vec!["notes"]),
    );
    let start = content.find('中').unwrap();
    let end = start + "中文目标abcdef".len();
    let reference = text_ref(
        &root,
        "notes",
        "device",
        "note.md",
        content.as_bytes(),
        start,
        end,
    );
    let response = read(&config, reference, (1, 1), 4);

    assert!(response.ok, "{:?}", response.code);
    assert!(response.truncated);
    assert_eq!(response.text.as_deref(), Some("中"));
    let range = response.text_range.unwrap();
    assert_eq!(range.byte_start, start as u64);
    assert_eq!(range.byte_end, start as u64 + 3);
    assert!(std::str::from_utf8(response.text.unwrap().as_bytes()).is_ok());
}

#[test]
fn a_target_range_inside_one_codepoint_fails_instead_of_returning_an_empty_segment() {
    let env = support::TestEnv::new();
    let root = env.root("empty-segment");
    let content = "a😀Z";
    env.write("empty-segment/note.md", content.as_bytes());
    let config = env.write_json(
        "empty-segment-config.json",
        &env.config_value(vec![env.md_source("notes", "device", &root)], vec!["notes"]),
    );
    // [2, 5) is entirely inside the four-byte emoji, so every candidate byte is
    // a continuation byte and a small budget cannot produce a valid segment.
    let reference = text_ref(
        &root,
        "notes",
        "device",
        "note.md",
        content.as_bytes(),
        2,
        5,
    );
    let response = read(&config, reference, (0, 0), 1);
    assert!(!response.ok, "empty segment must not report success");
    assert_eq!(response.code, Some(codes::REF_OUT_OF_RANGE));
}

#[test]
fn a_read_that_reaches_the_files_invalid_utf8_tail_fails() {
    let env = support::TestEnv::new();
    let root = env.root("bad-tail");
    let content = b"word\n\xe4\xb8";
    env.write("bad-tail/note.md", content);
    let config = env.write_json(
        "bad-tail-config.json",
        &env.config_value(vec![env.md_source("notes", "device", &root)], vec!["notes"]),
    );
    // The target is valid; the requested trailing context pulls in a dangling
    // UTF-8 sequence that belongs to the file itself.
    let reference = text_ref(&root, "notes", "device", "note.md", content, 0, 4);
    let response = read(&config, reference, (0, 1), 64);
    assert!(!response.ok, "invalid trailing UTF-8 must not be clipped silently");
    assert_eq!(response.code, Some(codes::REF_READ_FAILED));
}
