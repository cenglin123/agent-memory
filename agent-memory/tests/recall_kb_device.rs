//! P2 adapters: KB (`ask.py --json` via `kb_stub`), device notes, and the
//! three-source aggregation rules. Everything runs on temporary roots; the real
//! vault, real OpenCode store and real credentials are never touched.

mod support;

use agent_memory::recall::config::load_config;
use agent_memory::recall::contract::{codes, status, SourceStatus, SEARCH_WARNING_CODES};
use agent_memory::recall::read::{memory_read, memory_read_with, ReadOptions, ReadRequest};
use agent_memory::recall::refs::HitRef;
use agent_memory::recall::search::{SearchOptions, SearchRequest, SearchResponse};
use agent_memory::recall::snapshot::StrictSnapshot;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

const CHUNK: &str = "stub chunk";

fn python_exe() -> String {
    if let Ok(path) = std::env::var("PATH") {
        for dir in path.split(';') {
            for name in ["python.exe", "python3.exe"] {
                let candidate = Path::new(dir).join(name);
                if candidate.is_file() {
                    return candidate.to_string_lossy().to_string();
                }
            }
        }
    }
    "python".to_string()
}

fn stub_source() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/recall/kb_stub/ask.py")
}

/// Copy the stub into `<root>/.meta/scripts/ask.py` so the "script must live in
/// the root" rule holds without any real index.
fn install_stub(root: &Path) {
    let target = root.join(".meta").join("scripts").join("ask.py");
    std::fs::create_dir_all(target.parent().unwrap()).unwrap();
    std::fs::copy(stub_source(), &target).expect("install kb stub");
}

fn interpreter(stub: &str) -> Value {
    json!({
        "kb": {
            "command": python_exe(),
            "args": ["-B"],
            "env": {
                "PYTHONUTF8": "1",
                "PYTHONDONTWRITEBYTECODE": "1",
                "KB_STUB": stub
            }
        }
    })
}

fn config_with(
    env: &support::TestEnv,
    name: &str,
    sources: Vec<Value>,
    order: Vec<&str>,
    stub: &str,
) -> PathBuf {
    env.write_json(
        &format!("{name}.json"),
        &json!({
            "version": 1,
            "sources": sources,
            "source_order": order,
            "interpreter": interpreter(stub),
        }),
    )
}

struct KbCase {
    config: PathBuf,
}

fn kb_case(env: &support::TestEnv, name: &str, stub: &str) -> KbCase {
    let root = env.root(name);
    install_stub(&root);
    env.write(&format!("{name}/notes/alpha.md"), CHUNK.as_bytes());
    let source = env.md_source("kb", "kb", &root);
    let config = config_with(env, name, vec![source], vec!["kb"], stub);
    KbCase { config }
}

fn search(config_path: &Path, request: Value) -> SearchResponse {
    search_with(config_path, request, &SearchOptions::default())
}

fn search_with(config_path: &Path, request: Value, options: &SearchOptions) -> SearchResponse {
    let config = load_config(config_path).expect("config");
    let request = SearchRequest::parse(&request).expect("request");
    agent_memory::recall::search::memory_search_with(&config, &request, options)
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

fn read_ref(config_path: &Path, reference: &agent_memory::recall::refs::HitRef) -> Value {
    let config = load_config(config_path).expect("config");
    let request = ReadRequest::parse(&json!({"ref": reference})).expect("read request");
    serde_json::to_value(memory_read(&config, &request)).expect("read response")
}

fn read_ref_strict(config_path: &Path, reference: &HitRef) -> Value {
    let config = load_config(config_path).expect("config");
    let request = ReadRequest::parse(&json!({"ref": reference})).expect("read request");
    serde_json::to_value(memory_read_with(
        &config,
        &request,
        &ReadOptions {
            strict_snapshot: true,
        },
    ))
    .expect("read response")
}

fn search_strict(config_path: &Path, request: Value) -> SearchResponse {
    search_with(
        config_path,
        request,
        &SearchOptions {
            strict_snapshot: true,
            eval_freeze_decay: false,
        },
    )
}

/// Every `source_status[].detail` key must belong to the frozen set, and `code`
/// must appear exactly for the six incomplete states.
fn assert_status_shape(response: &SearchResponse) {
    use agent_memory::recall::contract::{is_incomplete, DETAIL_KEYS};
    for entry in &response.source_status {
        if let Some(detail) = &entry.detail {
            for key in detail.as_object().expect("detail object").keys() {
                assert!(
                    DETAIL_KEYS.contains(&key.as_str()),
                    "unfrozen detail key: {key}"
                );
            }
        }
        if is_incomplete(entry.status) {
            assert!(entry.code.is_some(), "{} needs a code", entry.source_id);
        } else {
            assert_eq!(entry.code, None, "{} must not carry a code", entry.source_id);
        }
    }
}

// ---------------------------------------------------------------------------
// KB
// ---------------------------------------------------------------------------

#[test]
fn kb_ok_hit_carries_score_kind_range_and_round_trips() {
    let env = support::TestEnv::new();
    let case = kb_case(&env, "kb-ok", "ok");
    let response = search(&case.config, json!({"query": "stub"}));
    assert_status_shape(&response);

    assert_eq!(source_status(&response, "kb").status, status::OK);
    assert_eq!(response.exit_code, 0);
    assert_eq!(response.results.len(), 1);
    let result = &response.results[0];
    assert_eq!(result.source_kind, "kb");
    assert_eq!(result.source_id, "kb");
    assert_eq!(result.title, "alpha.md");
    assert_eq!(result.source_rank, 1);
    assert_eq!(result.score, Some(0.75));
    assert_eq!(result.score_kind, "native_dense");
    assert_eq!(result.rank_basis, "native_dense");
    assert_eq!(result.reference.schema_gen, "md-v1");
    assert_eq!(result.reference.rel_path, "notes/alpha.md");
    assert!(result.reference.locator.is_empty());
    assert_eq!(result.reference.text_range.byte_start, 0);
    assert_eq!(result.reference.text_range.byte_end, CHUNK.len() as u64);
    assert_eq!(result.snippet, CHUNK);
    assert!(!result.snippet_truncated);
    assert_eq!(
        detail_value(&response, "kb", "kb_contract_version").unwrap(),
        &json!(1)
    );

    let read = read_ref(&case.config, &result.reference);
    assert_eq!(read["ok"], json!(true), "read: {read}");
    assert_eq!(read["text"], json!(CHUNK));
    assert_eq!(read["range_sha256"], json!(result.reference.fingerprint.range_sha256));
    assert!(read["warnings"].as_array().unwrap().is_empty());
}

#[test]
fn kb_empty_envelope_is_a_healthy_source() {
    let env = support::TestEnv::new();
    let case = kb_case(&env, "kb-empty", "ok-empty");
    let response = search(&case.config, json!({"query": "stub"}));
    assert_eq!(source_status(&response, "kb").status, status::EMPTY);
    assert!(response.results.is_empty());
    assert_eq!(response.exit_code, 0);
    assert!(response.pagination.sources_incomplete.is_empty());
}

#[test]
fn kb_degraded_and_fallbacks_only_produce_warnings() {
    let cases = [
        ("kb-degraded", "degraded", "WARN_RERANK_UNAVAILABLE"),
        ("kb-rerank-timeout", "rerank-timeout", "WARN_RERANK_TIMEOUT"),
        ("kb-graph", "graph-fallback", "WARN_GRAPH_FALLBACK"),
    ];
    for (name, stub, expected) in cases {
        let env = support::TestEnv::new();
        let case = kb_case(&env, name, stub);
        let response = search(&case.config, json!({"query": "stub"}));
        assert_status_shape(&response);
        assert_eq!(source_status(&response, "kb").status, status::OK, "{stub}");
        assert_eq!(source_status(&response, "kb").code, None, "{stub}");
        assert_eq!(response.exit_code, 0, "{stub}");
        assert!(response.pagination.sources_incomplete.is_empty(), "{stub}");
        let codes_seen: Vec<&str> = response.warnings.iter().map(|warning| warning.code).collect();
        assert_eq!(codes_seen, vec![expected], "{stub}");
        assert!(
            SEARCH_WARNING_CODES.contains(&expected),
            "warning code must belong to the memory_search set"
        );
        assert!(!codes_seen.contains(&"WARN_FILE_STALE"));
        // Warnings never change pagination or the degraded status.
        assert_eq!(response.results.len(), 1, "{stub}");
    }
}

#[test]
fn kb_escalation_is_reported_in_detail_and_maps_to_hybrid() {
    let env = support::TestEnv::new();
    let case = kb_case(&env, "kb-escalated", "escalated");
    env.write("kb-escalated/notes/beta.md", CHUNK.as_bytes());
    let response = search(&case.config, json!({"query": "stub"}));
    assert_eq!(source_status(&response, "kb").status, status::OK);
    assert_eq!(
        detail_value(&response, "kb", "escalated").unwrap(),
        &json!(["hybrid", "deep"])
    );
    assert_eq!(response.exit_code, 0, "escalation must not degrade");
    assert_eq!(response.results.len(), 2);
    for result in &response.results {
        assert_eq!(result.score_kind, "native_hybrid");
    }
}

#[test]
fn kb_failures_map_to_the_documented_codes() {
    let cases = [
        ("kb-error", "error", codes::DB_UNAVAILABLE, status::UNAVAILABLE, 4),
        (
            "kb-profile",
            "profile",
            codes::SCHEMA_UNSUPPORTED,
            status::UNSUPPORTED,
            4,
        ),
        (
            "kb-auth",
            "auth",
            codes::KB_QUERY_FAILED,
            status::ERROR,
            4,
        ),
        (
            "kb-unknown-exit",
            "unknown-exit",
            codes::KB_QUERY_FAILED,
            status::ERROR,
            4,
        ),
    ];
    for (name, stub, expected_code, expected_status, expected_exit) in cases {
        let env = support::TestEnv::new();
        let case = kb_case(&env, name, stub);
        let response = search(&case.config, json!({"query": "stub"}));
        assert_status_shape(&response);
        assert_eq!(source_status(&response, "kb").status, expected_status, "{stub}");
        assert_eq!(source_status(&response, "kb").code, Some(expected_code), "{stub}");
        assert_eq!(response.exit_code, expected_exit, "{stub}");
        assert!(response.results.is_empty(), "{stub}");
        assert_eq!(
            detail_value(&response, "kb", "reason").unwrap(),
            &json!(match stub {
                "error" => "LOCAL_UNAVAILABLE",
                "profile" => "PROFILE_MISMATCH",
                "auth" => "REMOTE_AUTH_REQUIRED",
                _ => "ACTIVATION_ABORTED",
            }),
            "{stub}"
        );
        assert_eq!(
            detail_value(&response, "kb", "code").unwrap(),
            &json!(expected_code),
            "{stub}"
        );
    }
}

#[test]
fn kb_contract_violations_are_unsupported_or_failed() {
    // `--json` unknown to ask.py: exit 2, no stdout document.
    {
        let env = support::TestEnv::new();
        let case = kb_case(&env, "kb-exit2", "exit2-empty");
        let response = search(&case.config, json!({"query": "stub"}));
        assert_eq!(source_status(&response, "kb").status, status::UNSUPPORTED);
        assert_eq!(
            source_status(&response, "kb").code,
            Some(codes::KB_CONTRACT_UNSUPPORTED)
        );
        assert_eq!(
            detail_value(&response, "kb", "reason").unwrap(),
            &json!("ask_json_unsupported")
        );
    }

    // Missing / incompatible contract_version.
    for (name, stub, observed, reason) in [
        (
            "kb-no-version",
            "no-version",
            Value::Null,
            "contract_version_required_1_observed_missing",
        ),
        (
            "kb-bad-version",
            "bad-version",
            json!(2),
            "contract_version_required_1_observed_2",
        ),
    ] {
        let env = support::TestEnv::new();
        let case = kb_case(&env, name, stub);
        let response = search(&case.config, json!({"query": "stub"}));
        assert_eq!(source_status(&response, "kb").status, status::UNSUPPORTED);
        assert_eq!(
            source_status(&response, "kb").code,
            Some(codes::KB_CONTRACT_UNSUPPORTED)
        );
        assert_eq!(
            detail_value(&response, "kb", "kb_contract_version").unwrap(),
            &observed,
            "{stub}"
        );
        // §3.3 asks for required/observed; the frozen detail key set has no
        // such pair, so both numbers are carried by `reason`.
        assert_eq!(
            detail_value(&response, "kb", "reason").unwrap(),
            &json!(reason),
            "{stub}"
        );
    }

    // Non-JSON stdout / two documents / missing mode / root mismatch.
    let cases = [
        ("kb-text", "plain-text", codes::KB_QUERY_FAILED, status::ERROR),
        ("kb-two", "two-documents", codes::KB_QUERY_FAILED, status::ERROR),
        ("kb-mode", "mode-missing", codes::KB_QUERY_FAILED, status::ERROR),
        (
            "kb-root",
            "root-mismatch",
            codes::KB_ROOT_MISMATCH,
            status::ERROR,
        ),
    ];
    for (name, stub, expected_code, expected_status) in cases {
        let env = support::TestEnv::new();
        let case = kb_case(&env, name, stub);
        let response = search(&case.config, json!({"query": "stub"}));
        assert_eq!(source_status(&response, "kb").status, expected_status, "{stub}");
        assert_eq!(source_status(&response, "kb").code, Some(expected_code), "{stub}");
        assert!(response.results.is_empty(), "{stub}");
        if stub == "mode-missing" {
            assert_eq!(
                detail_value(&response, "kb", "reason").unwrap(),
                &json!("ask_mode_invalid")
            );
        }
    }
}

#[test]
fn kb_unresolvable_chunks_are_dropped_and_reported_as_partial() {
    for (name, stub) in [
        ("kb-chunk-missing", "chunk-missing"),
        ("kb-chunk-zero", "chunk-zero"),
        ("kb-chunk-two", "chunk-two"),
    ] {
        let env = support::TestEnv::new();
        let case = kb_case(&env, name, stub);
        if stub == "chunk-two" {
            env.write(
                "kb-chunk-two/notes/alpha.md",
                b"dupchunk and again dupchunk",
            );
        }
        let response = search(&case.config, json!({"query": "stub"}));
        assert_status_shape(&response);
        assert!(response.results.is_empty(), "{stub}: no pseudo range");
        assert_eq!(source_status(&response, "kb").status, status::PARTIAL, "{stub}");
        assert_eq!(
            source_status(&response, "kb").code,
            Some(codes::CONTENT_UNRESOLVED),
            "{stub}"
        );
        assert_eq!(
            detail_value(&response, "kb", "CONTENT_UNRESOLVED").unwrap(),
            &json!(1),
            "{stub}"
        );
        assert_eq!(response.exit_code, 4, "{stub}");
        assert_eq!(response.pagination.sources_incomplete, vec!["kb"]);
        assert_eq!(response.next_offset, None);
    }
}

#[test]
fn kb_hard_timeout_is_truncated_not_empty() {
    let env = support::TestEnv::new();
    let case = kb_case(&env, "kb-timeout", "timeout");
    let response = search(
        &case.config,
        json!({"query": "stub", "budget": {"max_elapsed_ms_per_source": 4000}}),
    );
    assert_eq!(source_status(&response, "kb").status, status::TRUNCATED);
    assert_eq!(source_status(&response, "kb").code, Some(codes::KB_TIMEOUT));
    assert!(response.results.is_empty());
    assert_eq!(response.exit_code, 4);
    let detail = source_status(&response, "kb").detail.as_ref().unwrap();
    assert_eq!(detail["budget"]["reason"], codes::BUDGET_TIME);
    assert_eq!(detail["code"], codes::KB_TIMEOUT);
    assert!(
        detail.get("CONTENT_UNRESOLVED").is_none(),
        "a hard timeout must not stack partial"
    );
}

#[test]
fn kb_telemetry_writes_are_contract_violations() {
    // With results: exit 10 (degraded return), kb becomes `error`.
    {
        let env = support::TestEnv::new();
        let case = kb_case(&env, "kb-leak", "writes-log");
        let response = search(&case.config, json!({"query": "stub"}));
        assert_eq!(source_status(&response, "kb").status, status::ERROR);
        assert_eq!(
            source_status(&response, "kb").code,
            Some(codes::SIDE_EFFECT_VIOLATION)
        );
        assert!(!response.results.is_empty(), "already produced hits stay");
        assert_eq!(response.exit_code, 10);
        let entry = response
            .errors
            .iter()
            .find(|error| error.code == codes::SIDE_EFFECT_VIOLATION)
            .expect("violation entry");
        assert_eq!(entry.source_id.as_deref(), Some("kb"));
        assert!(response
            .side_effects
            .iter()
            .any(|effect| effect.target.as_deref() == Some(".meta/usage-log.jsonl")));
    }

    // Without results: exit 4, never 0.
    {
        let env = support::TestEnv::new();
        let case = kb_case(&env, "kb-leak-empty", "writes-log-empty");
        let response = search(&case.config, json!({"query": "stub"}));
        assert_eq!(source_status(&response, "kb").status, status::ERROR);
        assert_eq!(response.exit_code, 4);
    }
}

#[test]
fn kb_runtime_cache_is_only_a_c_level_side_effect() {
    let env = support::TestEnv::new();
    let case = kb_case(&env, "kb-cache", "writes-cache");
    let response = search(&case.config, json!({"query": "stub"}));
    assert_eq!(source_status(&response, "kb").status, status::OK);
    assert_eq!(response.exit_code, 0);
    assert!(response.errors.is_empty());
    assert!(!response.side_effects.is_empty());
    for effect in &response.side_effects {
        assert_eq!(effect.level, "C");
        assert_eq!(effect.note, Some("window_only"));
    }
    assert!(response
        .side_effects
        .iter()
        .all(|effect| !effect.target.as_deref().unwrap_or("").ends_with(".meta/usage-log.jsonl")));
}

// ---------------------------------------------------------------------------
// Device
// ---------------------------------------------------------------------------

#[test]
fn device_reads_only_the_dated_whitelist() {
    let env = support::TestEnv::new();
    let root = env.root("device-list");
    env.write(
        "device-list/2026-10-25_断电事件.md",
        "今天上午断电了\n后续记录\n".as_bytes(),
    );
    env.write("device-list/2026-13-01_bad.md", "断电".as_bytes());
    env.write("device-list/README.md", b"not an event");
    env.write("device-list/.hidden-2026-10-25_event.md", b"hidden");
    env.write("device-list/2026-10-24_nested/2026-10-24_event.md", b"nested body");
    let config = device_case_from(&env, "device-list-config", &root);

    let response = search(&config, json!({"query": "断电"}));
    assert_eq!(response.results.len(), 1, "invalid date/hidden/nested excluded");
    let result = &response.results[0];
    assert_eq!(result.title, "2026-10-25_断电事件.md");
    assert_eq!(result.source_kind, "device");
    assert_eq!(result.score, None);
    assert_eq!(result.score_kind, "none");
    assert_eq!(result.rank_basis, "lexical_window");
    // Body hit: the line containing the first occurrence, without the newline.
    assert_eq!(result.reference.text_range.line_start, 0);
    assert_eq!(result.reference.text_range.line_end, 1);
    assert_eq!(result.reference.text_range.byte_end, "今天上午断电了".len() as u64);
    assert_eq!(source_status(&response, "device").status, status::PARTIAL);
    assert_eq!(
        source_status(&response, "device").code,
        Some(codes::RECORD_SKIPPED)
    );
    assert_eq!(
        detail_value(&response, "device", "reason").unwrap(),
        &json!("invalid_date")
    );
    assert_eq!(response.exit_code, 10, "results + a partial source");
    assert_eq!(response.pagination.sources_incomplete, vec!["device"]);
    assert_eq!(response.next_offset, None);

    let read = read_ref(&config, &result.reference);
    assert_eq!(read["ok"], json!(true));
    assert_eq!(read["text"], json!("今天上午断电了"));
}

fn device_case_from(env: &support::TestEnv, name: &str, root: &Path) -> PathBuf {
    let source = env.md_source("device", "device", root);
    config_with(env, name, vec![source], vec!["device"], "ok")
}

#[test]
fn device_filename_only_hit_takes_the_first_body_block() {
    let env = support::TestEnv::new();
    let root = env.root("device-filename");
    env.write(
        "device-filename/2026-10-25_outage.md",
        "first body line\nsecond line\n".as_bytes(),
    );
    let config = device_case_from(&env, "device-filename-config", &root);
    let response = search(&config, json!({"query": "outage"}));
    assert_eq!(response.results.len(), 1);
    assert_eq!(source_status(&response, "device").status, status::OK);
    let result = &response.results[0];
    assert_eq!(result.reference.text_range.byte_start, 0);
    assert_eq!(
        result.reference.text_range.byte_end,
        "first body line".len() as u64
    );

    // The ref must read back the exact block bytes.
    let read = read_ref(&config, &result.reference);
    assert_eq!(read["ok"], json!(true));
    assert_eq!(read["text"], json!("first body line"));
}

#[test]
fn device_unrelated_terms_and_empty_roots_are_empty() {
    let env = support::TestEnv::new();
    let root = env.root("device-empty");
    env.write("device-empty/2026-10-25_quiet.md", b"nothing relevant here");
    let config = device_case_from(&env, "device-empty-config", &root);
    let response = search(&config, json!({"query": "zzznotthere"}));
    assert!(response.results.is_empty(), "no date-ordered fallback");
    assert_eq!(source_status(&response, "device").status, status::EMPTY);
    assert_eq!(response.exit_code, 0);
}

#[test]
fn device_crlf_and_multibyte_ranges_are_exact() {
    let env = support::TestEnv::new();
    let root = env.root("device-crlf");
    let body = "前言\r\n关键命中行中文\r\n结尾\r\n";
    env.write("device-crlf/2026-10-23_event.md", body.as_bytes());
    let config = device_case_from(&env, "device-crlf-config", &root);
    let response = search(&config, json!({"query": "关键命中"}));
    assert_eq!(response.results.len(), 1);
    let range = response.results[0].reference.text_range;
    let expected = "关键命中行中文";
    let start = body.find(expected).unwrap() as u64;
    assert_eq!(range.byte_start, start);
    assert_eq!(range.byte_end, start + expected.len() as u64);
    assert_eq!(range.line_start, 1);
    assert_eq!(range.line_end, 2);
    let read = read_ref(&config, &response.results[0].reference);
    assert_eq!(read["text"], json!(expected));
}

// ---------------------------------------------------------------------------
// Three-source aggregation
// ---------------------------------------------------------------------------

fn three_source_case(env: &support::TestEnv) -> PathBuf {
    let kb_root = env.root("agg-kb");
    install_stub(&kb_root);
    env.write("agg-kb/notes/alpha.md", CHUNK.as_bytes());
    env.write("agg-kb/notes/beta.md", CHUNK.as_bytes());

    let device_root = env.root("agg-device");
    env.write(
        "agg-device/2026-10-20_event.md",
        format!("{} in the body\n", support::NEEDLE).as_bytes(),
    );
    env.write(
        "agg-device/2026-10-21_event.md",
        format!("another {} line\n", support::NEEDLE).as_bytes(),
    );

    // Four OpenCode hits (2 sessions x 2 parts), all containing NEEDLE.
    let oc = support::build_v1_hits(env, "agg-oc", 2, 2);

    config_with(
        env,
        "agg",
        vec![
            env.md_source("kb", "kb", &kb_root),
            env.md_source("device", "device", &device_root),
            env.opencode_source("oc", &oc.root),
        ],
        vec!["kb", "device", "oc"],
        "escalated",
    )
}

#[test]
fn three_sources_interleave_by_source_rank() {
    let env = support::TestEnv::new();
    let config = three_source_case(&env);
    let response = search(&config, json!({"query": support::NEEDLE, "limit": 20}));

    let ids: Vec<&str> = response
        .results
        .iter()
        .map(|result| result.source_id.as_str())
        .collect();
    assert_eq!(
        &ids[..3],
        &["kb", "device", "oc"],
        "rank-1 round follows source_order"
    );
    assert_eq!(ids.len(), 8, "2 KB + 2 device + 4 opencode");
    assert_eq!(&ids[3..6], &["kb", "device", "oc"], "rank-2 round");
    assert_eq!(&ids[6..], &["oc", "oc"], "exhausted sources are skipped");
    let ranks: Vec<usize> = response
        .results
        .iter()
        .map(|result| result.source_rank)
        .collect();
    assert_eq!(ranks, vec![1, 1, 1, 2, 2, 2, 3, 4]);
    assert_eq!(response.exit_code, 0);
    assert!(response.pagination.sources_incomplete.is_empty());
}

#[test]
fn pagination_is_stable_across_pages() {
    let env = support::TestEnv::new();
    let config = three_source_case(&env);
    let full = search(&config, json!({"query": support::NEEDLE, "limit": 50}));
    assert_eq!(full.results.len(), 8);
    let expected: Vec<String> = full.results.iter().map(|r| r.hit_id.clone()).collect();

    let mut all: Vec<String> = Vec::new();
    let mut offset = 0usize;
    loop {
        let page = search(
            &config,
            json!({"query": support::NEEDLE, "limit": 2, "offset": offset}),
        );
        assert!(page.pagination.sources_incomplete.is_empty());
        all.extend(page.results.iter().map(|result| result.hit_id.clone()));
        offset += page.results.len();
        match page.next_offset {
            Some(next) => {
                assert_eq!(next, offset);
                assert!(page.results.len() == 2, "only a full page may page on");
            }
            None => {
                assert_eq!(offset, 8, "the last page is exactly full");
                break;
            }
        }
    }
    assert_eq!(all, expected, "pages must not repeat or drop hits");

    let beyond = search(
        &config,
        json!({"query": support::NEEDLE, "limit": 2, "offset": 8}),
    );
    assert!(beyond.results.is_empty());
    assert_eq!(beyond.next_offset, None);
}

#[test]
fn a_partial_source_stops_pagination_and_is_listed() {
    let env = support::TestEnv::new();
    let kb_root = env.root("partial-kb");
    install_stub(&kb_root);
    let device_root = env.root("partial-device");
    env.write(
        "partial-device/2026-10-20_event.md",
        "stub chunk line\n".as_bytes(),
    );
    let config = config_with(
        &env,
        "partial",
        vec![
            env.md_source("kb", "kb", &kb_root),
            env.md_source("device", "device", &device_root),
        ],
        vec!["kb", "device"],
        "error",
    );
    let response = search(&config, json!({"query": "stub", "limit": 1}));
    assert_eq!(response.results.len(), 1, "device results still returned");
    assert_eq!(source_status(&response, "kb").status, status::UNAVAILABLE);
    assert_eq!(source_status(&response, "device").status, status::OK);
    assert_eq!(response.pagination.sources_incomplete, vec!["kb"]);
    assert_eq!(response.next_offset, None);
    assert_eq!(response.exit_code, 10);
    assert_eq!(response.errors.len(), 1);
    assert_eq!(response.errors[0].source_id.as_deref(), Some("kb"));
}

// ---------------------------------------------------------------------------
// Strict snapshot
// ---------------------------------------------------------------------------

#[test]
fn strict_snapshot_reads_a_copy_and_leaves_the_root_alone() {
    let env = support::TestEnv::new();
    let root = env.root("strict-device");
    env.write(
        "strict-device/2026-10-25_event.md",
        "strict body line\n".as_bytes(),
    );
    let config = device_case_from(&env, "strict-config", &root);

    let before = support::tree_state(&root);
    let strict = search_with(
        &config,
        json!({"query": "strict"}),
        &SearchOptions {
            strict_snapshot: true,
            eval_freeze_decay: false,
        },
    );
    let after = support::tree_state(&root);
    assert_eq!(before, after, "strict mode must not touch the original root");
    assert_eq!(strict.results.len(), 1);
    assert_eq!(strict.results[0].reference.root_sha256.len(), 64);
    assert!(
        strict.side_effects.is_empty(),
        "a copy is not an observed root"
    );

    let normal = search(&config, json!({"query": "strict"}));
    assert_eq!(
        normal.results[0].hit_id, strict.results[0].hit_id,
        "refs are identical in both modes"
    );
}

// ---------------------------------------------------------------------------
// CRLF chunk classes (KB) — the index is LF-normalized, the file is not
// ---------------------------------------------------------------------------

#[test]
fn kb_newline_chunk_cannot_match_a_crlf_file_but_single_line_chunk_can() {
    {
        // A chunk that contains a newline is stored LF-normalized, so it can
        // never be found byte-for-byte in a CRLF file: the hit is dropped.
        let env = support::TestEnv::new();
        let case = kb_case(&env, "kb-crlf-multi", "chunk-crlf-multiline");
        env.write("kb-crlf-multi/notes/alpha.md", b"line1\r\nneedle\r\n");
        let response = search(&case.config, json!({"query": "needle"}));
        assert_status_shape(&response);
        assert!(response.results.is_empty(), "no fabricated range");
        assert_eq!(source_status(&response, "kb").status, status::PARTIAL);
        assert_eq!(
            source_status(&response, "kb").code,
            Some(codes::CONTENT_UNRESOLVED)
        );
        assert_eq!(
            detail_value(&response, "kb", "CONTENT_UNRESOLVED").unwrap(),
            &json!(1)
        );
        assert_eq!(response.exit_code, 4);
        assert_eq!(response.next_offset, None);
    }
    {
        // A single-line chunk is unaffected by CRLF normalization.
        let env = support::TestEnv::new();
        let case = kb_case(&env, "kb-crlf-single", "chunk-crlf-single");
        env.write("kb-crlf-single/notes/alpha.md", b"line1\r\nneedle\r\n");
        let response = search(&case.config, json!({"query": "needle"}));
        assert_status_shape(&response);
        assert_eq!(source_status(&response, "kb").status, status::OK);
        assert_eq!(response.exit_code, 0);
        assert_eq!(response.results.len(), 1);
        let result = &response.results[0];
        assert_eq!(result.reference.text_range.byte_start, 7);
        assert_eq!(result.reference.text_range.byte_end, 13);
        assert_eq!(result.reference.text_range.line_start, 1);
        assert_eq!(result.reference.text_range.line_end, 2);
        let read = read_ref(&case.config, &result.reference);
        assert_eq!(read["ok"], json!(true));
        assert_eq!(read["text"], json!("needle"));
    }
}

// ---------------------------------------------------------------------------
// Query starting with `-`
// ---------------------------------------------------------------------------

#[test]
fn kb_query_starting_with_a_dash_reaches_the_child_positional() {
    let env = support::TestEnv::new();
    let case = kb_case(&env, "kb-dash", "dash-query");
    let response = search(&case.config, json!({"query": "-foo"}));
    assert_status_shape(&response);
    assert_eq!(
        source_status(&response, "kb").status,
        status::OK,
        "a legal dash query must not look like an unsupported child: {:?}",
        source_status(&response, "kb")
    );
    assert_eq!(response.exit_code, 0);
    assert_eq!(response.results.len(), 1);
}

// ---------------------------------------------------------------------------
// S5.1: explicit exact on KB runs the child's BM25 lexical path
// ---------------------------------------------------------------------------

/// Install the lexical stub with note files whose chunk texts resolve to a
/// unique byte range: `notes/alpha.md` carries the full `AlphaBETA gamma`
/// chunk, `notes/beta.md` the partial `only alpha` chunk.
fn kb_lexical_case(env: &support::TestEnv, name: &str, stub: &str) -> KbCase {
    let root = env.root(name);
    install_stub(&root);
    env.write(&format!("{name}/notes/alpha.md"), b"AlphaBETA gamma body\n");
    env.write(&format!("{name}/notes/beta.md"), b"only alpha here\n");
    let source = env.md_source("kb", "kb", &root);
    let config = config_with(env, name, vec![source], vec!["kb"], stub);
    KbCase { config }
}

#[test]
fn kb_exact_filters_bm25_candidates_to_full_term_matches() {
    let env = support::TestEnv::new();
    let case = kb_lexical_case(&env, "kb-exact-lexical", "bm25-lexical");
    let response = search(
        &case.config,
        json!({"query": "alphabeta gamma", "mode": "exact", "sources": ["kb"]}),
    );
    assert_status_shape(&response);

    // The partial candidate (`only alpha` — no "gamma") is filtered away.
    assert_eq!(response.results.len(), 1, "{response:?}");
    let result = &response.results[0];
    assert_eq!(result.title, "alpha.md");
    assert_eq!(result.score, Some(0.9));
    // The child's own BM25 mode maps to the explicit native_bm25 producer.
    assert_eq!(result.score_kind, "native_bm25");
    assert_eq!(result.rank_basis, "native_bm25");
    assert_eq!(result.snippet, "AlphaBETA gamma");
    assert!(!result.snippet_truncated);

    // The exact block and resolved mode name the lexical path, not direct.
    let detail = source_status(&response, "kb").detail.as_ref().unwrap();
    assert_eq!(detail["resolved_mode"], json!("kb_lexical"), "{detail}");
    assert_eq!(
        detail["exact"],
        json!({"mode": "kb_lexical", "reason": "not_indexed"}),
        "{detail}"
    );
    assert_eq!(source_status(&response, "kb").status, status::OK);
    assert_eq!(response.exit_code, 0);
    assert!(response.pagination.sources_incomplete.is_empty());

    // A kept ref still reads back through the unique whole-byte range.
    let read = read_ref(&case.config, &result.reference);
    assert_eq!(read["ok"], json!(true), "read: {read}");
    assert_eq!(read["text"], json!("AlphaBETA gamma"));
}

#[test]
fn kb_exact_filtered_to_nothing_is_a_healthy_empty_source() {
    let env = support::TestEnv::new();
    let case = kb_lexical_case(&env, "kb-exact-filtered-empty", "bm25-empty");
    let response = search(
        &case.config,
        json!({"query": "alphabeta gamma", "mode": "exact", "sources": ["kb"]}),
    );
    assert_status_shape(&response);
    assert!(response.results.is_empty(), "{response:?}");
    assert_eq!(source_status(&response, "kb").status, status::EMPTY);
    assert_eq!(source_status(&response, "kb").code, None);
    assert_eq!(response.exit_code, 0);
    let detail = source_status(&response, "kb").detail.as_ref().unwrap();
    assert_eq!(detail["resolved_mode"], json!("kb_lexical"), "{detail}");
    assert_eq!(
        detail["exact"],
        json!({"mode": "kb_lexical", "reason": "not_indexed"}),
        "{detail}"
    );
    assert!(detail.get("CONTENT_UNRESOLVED").is_none());
    assert!(response.pagination.sources_incomplete.is_empty());
}

#[test]
fn kb_exact_cjk_terms_require_the_whole_segment() {
    let env = support::TestEnv::new();
    let root = env.root("kb-exact-cjk");
    install_stub(&root);
    env.write("kb-exact-cjk/notes/alpha.md", "前文统一回忆后文\n".as_bytes());
    env.write("kb-exact-cjk/notes/beta.md", "只有统一没有回忆\n".as_bytes());
    let source = env.md_source("kb", "kb", &root);
    let config = config_with(&env, "kb-exact-cjk", vec![source], vec!["kb"], "bm25-cjk");

    // "统一回忆" tokenizes to the whole segment plus adjacent bigrams; only the
    // chunk containing the full contiguous segment carries every term.
    let response = search(
        &config,
        json!({"query": "统一回忆", "mode": "exact", "sources": ["kb"]}),
    );
    assert_status_shape(&response);
    assert_eq!(response.results.len(), 1, "{response:?}");
    let result = &response.results[0];
    assert_eq!(result.title, "alpha.md");
    assert_eq!(result.score_kind, "native_bm25");
    let detail = source_status(&response, "kb").detail.as_ref().unwrap();
    assert_eq!(detail["resolved_mode"], json!("kb_lexical"), "{detail}");
    let read = read_ref(&config, &result.reference);
    assert_eq!(read["text"], json!("统一回忆"));
}

#[test]
fn kb_non_exact_requests_never_append_bm25() {
    // The lexical stub refuses any call without `--bm25`, so an error here
    // proves the caller did not append the flag.
    for (name, request, expected_resolved) in [
        (
            "kb-nobm25-auto",
            json!({"query": "alphabeta gamma", "sources": ["kb"]}),
            "legacy",
        ),
        (
            "kb-nobm25-hybrid",
            json!({"query": "alphabeta gamma", "mode": "hybrid", "sources": ["kb"]}),
            "hybrid",
        ),
        (
            "kb-nobm25-legacy",
            json!({"query": "alphabeta gamma", "mode": "legacy", "sources": ["kb"]}),
            "legacy",
        ),
    ] {
        let env = support::TestEnv::new();
        let case = kb_lexical_case(&env, name, "bm25-lexical");
        let response = search(&case.config, request);
        assert_status_shape(&response);
        assert_eq!(
            source_status(&response, "kb").status,
            status::ERROR,
            "{name}: the stub must reject a call without --bm25"
        );
        assert_eq!(
            source_status(&response, "kb").code,
            Some(codes::KB_QUERY_FAILED),
            "{name}"
        );
        let detail = source_status(&response, "kb").detail.as_ref().unwrap();
        assert_eq!(detail["resolved_mode"], json!(expected_resolved), "{name}");
        assert!(response.results.is_empty(), "{name}");
    }
}

#[test]
fn kb_exact_dash_query_reaches_the_child_with_bm25() {
    let env = support::TestEnv::new();
    let root = env.root("kb-exact-dash");
    install_stub(&root);
    env.write("kb-exact-dash/notes/alpha.md", b"dash-foo-marker\n");
    let source = env.md_source("kb", "kb", &root);
    let config = config_with(&env, "kb-exact-dash", vec![source], vec!["kb"], "bm25-dash");
    let response = search(
        &config,
        json!({"query": "-foo", "mode": "exact", "sources": ["kb"]}),
    );
    assert_status_shape(&response);
    assert_eq!(
        source_status(&response, "kb").status,
        status::OK,
        "--bm25 must not disturb the `--` terminator: {:?}",
        source_status(&response, "kb")
    );
    assert_eq!(response.exit_code, 0);
    assert_eq!(response.results.len(), 1);
    assert_eq!(response.results[0].snippet, "dash-foo-marker");
    let detail = source_status(&response, "kb").detail.as_ref().unwrap();
    assert_eq!(detail["resolved_mode"], json!("kb_lexical"), "{detail}");
}

#[test]
fn kb_exact_session_filter_stays_the_not_applicable_corner() {
    // A `session_id` on a KB source short-circuits before any retrieval: no
    // lexical path ran, so the previous direct marking stays (no false
    // kb_lexical claim).
    let env = support::TestEnv::new();
    let case = kb_case(&env, "kb-exact-session", "ok");
    let response = search(
        &case.config,
        json!({"query": "stub", "mode": "exact", "sources": ["kb"],
               "session_id": "ses_somebody"}),
    );
    assert_status_shape(&response);
    assert_eq!(source_status(&response, "kb").status, status::EMPTY);
    let detail = source_status(&response, "kb").detail.as_ref().unwrap();
    assert_eq!(detail["reason"], json!("session_filter_not_applicable"));
    assert_eq!(
        detail["exact"],
        json!({"mode": "direct", "reason": "not_indexed"}),
        "{detail}"
    );
    assert_eq!(detail["resolved_mode"], json!("exact"), "{detail}");
}

#[test]
fn kb_lexical_is_not_a_request_mode() {
    let error = SearchRequest::parse(&json!({"query": "x", "mode": "kb_lexical"}))
        .expect_err("kb_lexical is a resolved path, not a request mode");
    assert_eq!(error.code, codes::INVALID_REQUEST);
    let detail = error.detail.unwrap();
    assert_eq!(detail["path"], "mode");
    assert_eq!(detail["reason"], "unsupported_mode");
}

// ---------------------------------------------------------------------------
// Strict snapshot: OpenCode (SQLite v1 + legacy JSON) and readback
// ---------------------------------------------------------------------------

#[test]
fn strict_snapshot_serves_sqlite_search_and_read() {
    let env = support::TestEnv::new();
    let fixture = support::build_v1(&env);
    let configured = support::configured(&env, "strict-sqlite", &fixture.root);
    let request = json!({"query": support::NEEDLE, "limit": 50});

    let normal = search(&configured.config, request.clone());
    let strict = search_strict(&configured.config, request);
    assert_eq!(source_status(&normal, "oc").status, status::OK);
    assert_eq!(
        source_status(&strict, "oc").status,
        status::OK,
        "strict mode must not degrade a healthy SQLite source: {:?}",
        source_status(&strict, "oc")
    );
    assert_eq!(strict.exit_code, 0);
    let mut normal_ids: Vec<&str> = normal.results.iter().map(|r| r.hit_id.as_str()).collect();
    let mut strict_ids: Vec<&str> = strict.results.iter().map(|r| r.hit_id.as_str()).collect();
    normal_ids.sort_unstable();
    strict_ids.sort_unstable();
    assert_eq!(strict_ids, normal_ids, "refs are identical in both modes");
    assert!(strict.side_effects.is_empty());

    let reference = &strict.results[0].reference;
    assert_eq!(reference.schema_gen, "sqlite-v1");
    let strict_read = read_ref_strict(&configured.config, reference);
    assert_eq!(strict_read["ok"], json!(true), "read: {strict_read}");
    let normal_read = read_ref(&configured.config, reference);
    assert_eq!(strict_read["text"], normal_read["text"]);
    assert_eq!(strict_read["range_sha256"], normal_read["range_sha256"]);

    // The original store is untouched by the strict run.
    assert!(fixture.db.is_file());
}

#[test]
fn strict_snapshot_serves_legacy_json_search_and_read() {
    let env = support::TestEnv::new();
    let fixture = support::build_legacy(&env);
    let configured = support::configured(&env, "strict-legacy", &fixture.root);
    let request = json!({"query": support::NEEDLE_LEGACY, "limit": 50});

    let normal = search(&configured.config, request.clone());
    let normal_status = source_status(&normal, "oc").status;
    // The legacy fixture contains one deliberately broken part, so the healthy
    // status is `partial`; what matters is that strict mode preserves it.
    assert!(
        matches!(normal_status, status::OK | status::PARTIAL),
        "unexpected baseline status {normal_status}"
    );
    let strict = search_strict(&configured.config, request);
    let strict_status = source_status(&strict, "oc").status;
    assert_eq!(
        strict_status, normal_status,
        "strict mode must preserve the legacy JSON source status: {:?}",
        source_status(&strict, "oc")
    );
    assert_ne!(strict_status, status::UNAVAILABLE);
    let mut normal_ids: Vec<&str> = normal.results.iter().map(|r| r.hit_id.as_str()).collect();
    let mut strict_ids: Vec<&str> = strict.results.iter().map(|r| r.hit_id.as_str()).collect();
    normal_ids.sort_unstable();
    strict_ids.sort_unstable();
    assert_eq!(strict_ids, normal_ids);
    for result in &strict.results {
        assert_eq!(result.reference.schema_gen, "json-legacy-v1");
    }

    let strict_read = read_ref_strict(&configured.config, &strict.results[0].reference);
    assert_eq!(strict_read["ok"], json!(true), "read: {strict_read}");
    assert!(strict_read["text"]
        .as_str()
        .unwrap()
        .contains(support::NEEDLE_LEGACY));
}

#[test]
fn memory_read_strict_snapshot_reads_device_without_touching_the_root() {
    let env = support::TestEnv::new();
    let root = env.root("strict-read-device");
    env.write(
        "strict-read-device/2026-10-25_event.md",
        "strict read body line\n".as_bytes(),
    );
    let config = device_case_from(&env, "strict-read-config", &root);
    let response = search(&config, json!({"query": "strict"}));
    assert_eq!(response.results.len(), 1);
    let reference = response.results[0].reference.clone();

    let before = support::tree_state(&root);
    let strict_read = read_ref_strict(&config, &reference);
    let after = support::tree_state(&root);
    assert_eq!(strict_read["ok"], json!(true), "read: {strict_read}");
    assert_eq!(strict_read["text"], json!("strict read body line"));
    assert_eq!(before, after, "strict read must not modify the original root");
    assert!(
        strict_read["side_effects"].as_array().unwrap().is_empty(),
        "a copy is not an observed root"
    );
}

#[test]
fn strict_snapshot_failure_removes_its_temporary_directory() {
    let env = support::TestEnv::new();
    let missing = env.path().join("missing-source-root");
    let error = match StrictSnapshot::create(&missing) {
        Ok(_) => panic!("copy of a missing root must fail"),
        Err(error) => error,
    };
    assert!(
        error.contains("strict snapshot"),
        "unexpected error: {error}"
    );
    let leftovers: Vec<String> = std::fs::read_dir(env.path())
        .unwrap()
        .flatten()
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| name.starts_with("agent-memory-strict-"))
        .collect();
    assert!(
        leftovers.is_empty(),
        "a failed snapshot must not leave temporary directories: {leftovers:?}"
    );
}

// ---------------------------------------------------------------------------
// Device: an unreadable (empty) range must not become a result
// ---------------------------------------------------------------------------

#[test]
fn device_empty_body_filename_hit_is_dropped_not_emitted_unreadable() {
    let env = support::TestEnv::new();
    let root = env.root("device-zero-body");
    env.write("device-zero-body/2026-10-25_calendar.md", b"");
    let config = device_case_from(&env, "device-zero-config", &root);

    let response = search(&config, json!({"query": "calendar"}));
    assert_status_shape(&response);
    assert!(
        response.results.is_empty(),
        "an unreadable [0,0) ref must never be emitted"
    );
    assert_eq!(source_status(&response, "device").status, status::PARTIAL);
    assert_eq!(
        source_status(&response, "device").code,
        Some(codes::CONTENT_UNRESOLVED)
    );
    assert_eq!(
        detail_value(&response, "device", "CONTENT_UNRESOLVED").unwrap(),
        &json!(1)
    );
    assert_eq!(response.exit_code, 4);
    assert_eq!(response.pagination.sources_incomplete, vec!["device"]);
    assert_eq!(response.next_offset, None);
}
