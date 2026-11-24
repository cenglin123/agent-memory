//! Codex `rollout-*.jsonl[.zst]` recall: search projection, `session_id`
//! filtering, compressed/plain identity and fail-closed readback.
//!
//! Every fixture is synthetic and lives under the per-case temporary root.

mod support;

use agent_memory::recall::config::RecallConfig;
use agent_memory::recall::contract::{codes, status, SCHEMA_CODEX_ROLLOUT_V1, SOURCE_KIND_CODEX};
use agent_memory::recall::hash::root_sha256;
use agent_memory::recall::read::{
    memory_read, memory_read_with, ReadOptions, ReadRequest, ReadResponse,
};
use agent_memory::recall::search::{
    memory_search, memory_search_with, SearchOptions, SearchRequest, SearchResponse,
};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

const SESSION_A: &str = "11111111-1111-4111-8111-111111111111";
const SESSION_B: &str = "22222222-2222-4222-8222-222222222222";

fn load(env: &support::TestEnv, name: &str, sources: Vec<Value>, order: Vec<&str>) -> RecallConfig {
    let path = env.write_json(&format!("{name}.json"), &env.config_value(sources, order));
    agent_memory::recall::load_config(&path).expect("explicit codex config")
}

fn search(config: &RecallConfig, request: Value) -> SearchResponse {
    memory_search(config, &SearchRequest::parse(&request).expect("request"))
}

fn read(
    config: &RecallConfig,
    reference: Value,
    before: usize,
    after: usize,
    max_bytes: Option<usize>,
) -> ReadResponse {
    let mut request = json!({
        "ref": reference,
        "context_before": before,
        "context_after": after,
    });
    if let Some(max_bytes) = max_bytes {
        request["max_bytes"] = json!(max_bytes);
    }
    memory_read(config, &ReadRequest::parse(&request).expect("read request"))
}

fn status_of<'a>(response: &'a SearchResponse, source_id: &str) -> &'a agent_memory::recall::contract::SourceStatus {
    response
        .source_status
        .iter()
        .find(|entry| entry.source_id == source_id)
        .expect("source status")
}

fn detail<'a>(response: &'a SearchResponse, source_id: &str, key: &str) -> Option<&'a Value> {
    status_of(response, source_id)
        .detail
        .as_ref()
        .and_then(|detail| detail.get(key))
}

#[cfg(windows)]
fn symlink_file(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::windows::fs::symlink_file(target, link)
}

#[cfg(unix)]
fn symlink_file(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
fn symlink_dir(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::windows::fs::symlink_dir(target, link)
}

#[cfg(unix)]
fn symlink_dir(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
struct AclDeny {
    path: PathBuf,
}

#[cfg(windows)]
impl Drop for AclDeny {
    fn drop(&mut self) {
        let _ = std::process::Command::new("icacls")
            .arg(&self.path)
            .arg("/remove:d")
            .arg("*S-1-1-0")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
}

/// Deny directory listing to Everyone and verify the denial is effective.
/// Returns `None` when the host cannot apply/verify the ACL (test skips).
#[cfg(windows)]
fn deny_directory_listing(path: &Path) -> Option<AclDeny> {
    let status = std::process::Command::new("icacls")
        .arg(path)
        .arg("/deny")
        .arg("*S-1-1-0:(RD)")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .ok()?;
    if !status.success() {
        return None;
    }
    if std::fs::read_dir(path).is_ok() {
        let _ = std::process::Command::new("icacls")
            .arg(path)
            .arg("/remove:d")
            .arg("*S-1-1-0")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        return None;
    }
    Some(AclDeny {
        path: path.to_path_buf(),
    })
}

#[cfg(windows)]
struct ExclusiveRollout {
    _file: std::fs::File,
}

/// Hold a Windows file open with no sharing, so every later open fails.
#[cfg(windows)]
fn lock_exclusive(path: &Path) -> Option<ExclusiveRollout> {
    use std::os::windows::fs::OpenOptionsExt;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(path)
        .ok()?;
    if std::fs::File::open(path).is_ok() {
        return None;
    }
    Some(ExclusiveRollout { _file: file })
}

fn write_rollout_at(dir: &Path, file_name: &str, lines: &[String]) -> PathBuf {
    std::fs::create_dir_all(dir).expect("rollout dir");
    let path = dir.join(file_name);
    let mut text = String::new();
    for line in lines {
        text.push_str(line);
        text.push('\n');
    }
    std::fs::write(&path, text).expect("write rollout");
    path
}

fn single_source(env: &support::TestEnv, name: &str, home: &Path) -> RecallConfig {
    let source = support::codex_source("cx", home, None, None);
    load(env, name, vec![source], vec!["cx"])
}

fn full_corpus(env: &support::TestEnv, name: &str) -> (PathBuf, RecallConfig) {
    let home = env.root(name);
    let lines = vec![
        support::codex_meta(SESSION_A),
        support::codex_user("codexneedle question"),
        support::codex_assistant("codexneedle answer"),
        support::codex_function_call("call-1", "shell", "{\"cmd\":\"codexneedle\"}"),
        support::codex_function_output("call-1", json!("codexneedle output")),
        support::codex_custom_call("call-2", "apply_patch", "codexneedle input"),
        support::codex_local_shell("call-3", "codexneedle shell"),
        support::codex_command_completed("call-3", "codexneedle command"),
        support::codex_mcp_completed("call-4", "codexneedle mcp"),
    ];
    support::build_codex_rollout(&home, SESSION_A, &lines);
    let config = single_source(env, name, &home);
    (home, config)
}

#[test]
fn user_assistant_and_tool_records_are_projected() {
    let env = support::TestEnv::new();
    let (_home, config) = full_corpus(&env, "codex-full");
    let response = search(&config, json!({"query": "codexneedle", "limit": 50}));

    assert_eq!(response.exit_code, 0, "{:?}", response.errors);
    assert_eq!(status_of(&response, "cx").status, status::OK);
    assert_eq!(response.results.len(), 8, "one hit per projected record");
    let pointers: Vec<&str> = response
        .results
        .iter()
        .map(|result| {
            result.reference.locator.pointer.as_deref().expect("pointer")
        })
        .collect();
    for expected in [
        "/payload/content",
        "/payload/arguments",
        "/payload/output",
        "/payload/input",
        "/payload/action",
        "/payload/item/aggregated_output",
        "/payload/item/result/content/0/text",
    ] {
        assert!(pointers.contains(&expected), "missing {expected}: {pointers:?}");
    }
    for result in &response.results {
        assert_eq!(result.source_kind, SOURCE_KIND_CODEX);
        assert_eq!(result.reference.schema_gen, SCHEMA_CODEX_ROLLOUT_V1);
        assert!(!result.reference.rel_path.ends_with(".zst"));
        assert_eq!(
            result.reference.locator.session_id.as_deref(),
            Some(SESSION_A)
        );
        assert!(result.reference.locator.record_index.is_some());
        assert!(result.reference.locator.part_id.is_none());
        assert!(result.reference.fingerprint.file_sha256.is_none());
        assert!(result.reference.fingerprint.row_sha256.is_some());
        assert_eq!(result.reference.text_range.byte_start, 0);
    }
    let locator = &response.results[0].reference.locator;
    assert_eq!(locator.session_id.as_deref(), Some(SESSION_A));
    assert_eq!(response.results[0].title, "codexneedle question");
}

#[test]
fn injected_context_developer_roles_and_subagents_are_excluded() {
    let env = support::TestEnv::new();
    let home = env.root("codex-injected");
    let lines = vec![
        support::codex_meta(SESSION_A),
        support::codex_user("# AGENTS.md instructions\ncodexneedle injected"),
        support::codex_user("<environment_context>codexneedle env</environment_context>"),
        support::codex_user("<user_instructions>codexneedle instructions</user_instructions>"),
        support::codex_user("<turn_aborted>codexneedle aborted</turn_aborted>"),
        support::codex_user("<recommended_plugins>codexneedle plugins</recommended_plugins>"),
        support::codex_message("developer", json!("codexneedle developer")),
        support::codex_message("system", json!("codexneedle system")),
        support::codex_user(
            "# Context from my IDE setup:\nopen file codexneedle selection\n\
             ## My request for Codex:\nrealneedle request",
        ),
        support::codex_assistant("codexneedle answer"),
    ];
    support::build_codex_rollout(&home, SESSION_A, &lines);
    // A subagent rollout is not a searchable session at all.
    let subagent = vec![
        json!({
            "timestamp": "2026-03-06T21:50:12Z",
            "type": "session_meta",
            "payload": {"id": SESSION_B, "cwd": "C:/work", "source": {"subagent": {"parent": SESSION_A}}}
        })
        .to_string(),
        support::codex_user("codexneedle subagent"),
    ];
    support::build_codex_rollout(&home, SESSION_B, &subagent);

    let config = single_source(&env, "codex-injected", &home);
    let response = search(&config, json!({"query": "codexneedle", "limit": 50}));
    assert_eq!(status_of(&response, "cx").status, status::OK);
    assert_eq!(response.results.len(), 1, "only the assistant body is indexable");
    assert_eq!(detail(&response, "cx", "coverage").unwrap()["total_sessions"], json!(1));
    assert!(response.results[0].snippet.contains("codexneedle answer"));

    // The IDE wrapper contributes only the real prompt.
    let prompt = search(&config, json!({"query": "realneedle"}));
    assert_eq!(prompt.results.len(), 1);
    assert!(prompt.results[0].snippet.contains("realneedle request"));
    let ide_selection = search(&config, json!({"query": "selection"}));
    assert!(
        ide_selection.results.is_empty(),
        "IDE context before the request marker must not be indexed"
    );
}

#[test]
fn unknown_record_types_are_counted_not_fatal() {
    let env = support::TestEnv::new();
    let home = env.root("codex-ignored");
    let lines = vec![
        support::codex_meta(SESSION_A),
        json!({"timestamp": "2026-03-06T21:50:13Z", "type": "turn_context",
               "payload": {"cwd": "C:/work"}})
        .to_string(),
        json!({"timestamp": "2026-03-06T21:50:14Z", "type": "response_item",
               "payload": {"type": "reasoning", "summary": []}})
        .to_string(),
        json!({"timestamp": "2026-03-06T21:50:15Z", "type": "event_msg",
               "payload": {"type": "token_count"}})
        .to_string(),
        support::codex_assistant("codexneedle answer"),
    ];
    support::build_codex_rollout(&home, SESSION_A, &lines);
    let config = single_source(&env, "codex-ignored", &home);
    let response = search(&config, json!({"query": "codexneedle"}));
    assert_eq!(status_of(&response, "cx").status, status::OK);
    assert_eq!(response.results.len(), 1);
    let ignored = detail(&response, "cx", "ignored_types").expect("ignored_types");
    for literal in [
        "session_meta",
        "turn_context",
        "reasoning",
        "event_msg:token_count",
    ] {
        assert!(ignored.get(literal).is_some(), "missing {literal}: {ignored}");
    }
    assert!(detail(&response, "cx", "CONTENT_UNRESOLVED").is_none());
}

#[test]
fn plain_and_compressed_corpora_share_identity() {
    let env = support::TestEnv::new();
    let lines = vec![
        support::codex_meta(SESSION_A),
        support::codex_user("codexneedle question"),
        support::codex_assistant("codexneedle answer"),
    ];

    let plain_home = env.root("codex-plain");
    support::build_codex_rollout(&plain_home, SESSION_A, &lines);
    let plain_config = single_source(&env, "codex-plain", &plain_home);

    let zst_home = env.root("codex-zst");
    let zst_plain = support::build_codex_rollout(&zst_home, SESSION_A, &lines);
    support::build_codex_zst(&zst_plain);
    std::fs::remove_file(&zst_plain).expect("compressed-only carrier");
    let zst_config = single_source(&env, "codex-zst", &zst_home);

    let plain = search(&plain_config, json!({"query": "codexneedle", "limit": 50}));
    let zst = search(&zst_config, json!({"query": "codexneedle", "limit": 50}));
    assert_eq!(status_of(&plain, "cx").status, status::OK);
    assert_eq!(status_of(&zst, "cx").status, status::OK);

    let ids = |response: &SearchResponse| {
        let mut ids: Vec<String> = response.results.iter().map(|hit| hit.hit_id.clone()).collect();
        ids.sort();
        ids
    };
    assert_eq!(ids(&plain), ids(&zst), "compression must not change hit_id");
    for (plain_hit, zst_hit) in plain.results.iter().zip(zst.results.iter()) {
        assert_eq!(plain_hit.reference.locator, zst_hit.reference.locator);
        assert_eq!(plain_hit.reference.rel_path, zst_hit.reference.rel_path);
        assert_eq!(
            plain_hit.reference.fingerprint.range_sha256,
            zst_hit.reference.fingerprint.range_sha256
        );
        assert_eq!(
            plain_hit.reference.fingerprint.row_sha256,
            zst_hit.reference.fingerprint.row_sha256
        );
        assert_ne!(
            plain_hit.reference.root_sha256,
            zst_hit.reference.root_sha256
        );
    }
    for (config, response) in [(&plain_config, &plain), (&zst_config, &zst)] {
        for result in &response.results {
            let readback = read(config, serde_json::to_value(&result.reference).unwrap(), 0, 0, None);
            assert!(readback.ok, "{:?}", readback.code);
            assert!(readback.text.unwrap().contains("codexneedle"));
        }
    }
}

#[test]
fn duplicate_siblings_are_deduped_to_the_plain_carrier() {
    let env = support::TestEnv::new();
    let home = env.root("codex-siblings");
    let lines = vec![
        support::codex_meta(SESSION_A),
        support::codex_user("codexneedle question"),
    ];
    let plain = support::build_codex_rollout(&home, SESSION_A, &lines);
    let zst = support::build_codex_zst(&plain);
    let config = single_source(&env, "codex-siblings", &home);

    let both = search(&config, json!({"query": "codexneedle"}));
    assert_eq!(both.results.len(), 1, "plain+zst is one logical session");
    let first_id = both.results[0].hit_id.clone();

    std::fs::remove_file(&plain).expect("remove plain");
    assert!(zst.is_file());
    let compressed_only = search(&config, json!({"query": "codexneedle"}));
    assert_eq!(compressed_only.results.len(), 1);
    assert_eq!(
        compressed_only.results[0].hit_id, first_id,
        "a compressed-only carrier keeps the logical identity"
    );
    let readback = read(
        &config,
        serde_json::to_value(&compressed_only.results[0].reference).unwrap(),
        0,
        0,
        None,
    );
    assert!(readback.ok, "{:?}", readback.code);
}

#[test]
fn every_search_hit_reads_back() {
    let env = support::TestEnv::new();
    let (_home, config) = full_corpus(&env, "codex-roundtrip");
    let response = search(&config, json!({"query": "codexneedle", "limit": 50}));
    assert!(!response.results.is_empty());
    for result in &response.results {
        let readback = read(
            &config,
            serde_json::to_value(&result.reference).unwrap(),
            0,
            0,
            None,
        );
        assert!(readback.ok, "{:?} for {:?}", readback.code, result.reference.locator);
        let text = readback.text.expect("single segment");
        assert!(text.contains("codexneedle"), "readback text: {text}");
        assert_eq!(
            readback.range_sha256.as_deref(),
            Some(result.reference.fingerprint.range_sha256.as_str())
        );
    }
}

#[test]
fn context_windows_span_adjacent_records() {
    let env = support::TestEnv::new();
    let home = env.root("codex-context");
    let lines = vec![
        support::codex_meta(SESSION_A),
        support::codex_user("alpha first"),
        support::codex_assistant("codexneedle middle"),
        support::codex_user("omega last"),
    ];
    support::build_codex_rollout(&home, SESSION_A, &lines);
    let config = single_source(&env, "codex-context", &home);

    let response = search(&config, json!({"query": "codexneedle"}));
    assert_eq!(response.results.len(), 1);
    let reference = serde_json::to_value(&response.results[0].reference).unwrap();
    assert_eq!(response.results[0].reference.locator.record_index, Some(2));

    let windowed = read(&config, reference.clone(), 1, 1, None);
    assert!(windowed.ok, "{:?}", windowed.code);
    let texts: Vec<&str> = windowed.segments.iter().map(|segment| segment.text.as_str()).collect();
    assert_eq!(texts, vec!["alpha first", "codexneedle middle", "omega last"]);
    let indices: Vec<Option<u64>> = windowed
        .segments
        .iter()
        .map(|segment| segment.locator.record_index)
        .collect();
    assert_eq!(indices, vec![Some(1), Some(2), Some(3)]);
    assert!(windowed.text.is_none(), "multi-segment omits convenience keys");
    assert!(windowed.text_range.is_none());
    assert!(windowed.range_sha256.is_none());
    assert!(!windowed.truncated);

    let single = read(&config, reference.clone(), 0, 0, None);
    assert_eq!(single.segments.len(), 1);
    assert!(single.text.unwrap().contains("codexneedle"));

    let narrow = read(&config, reference, 0, 0, Some(8));
    assert!(narrow.truncated);
    let text = narrow.text.expect("single segment");
    assert!(text.len() <= 8);
    assert!("codexneedle middle".starts_with(&text));
}

#[test]
fn wrong_pointer_wrong_line_and_changed_bytes_fail_closed() {
    let env = support::TestEnv::new();
    let (home, config) = full_corpus(&env, "codex-stale");
    let response = search(&config, json!({"query": "answer"}));
    assert_eq!(response.results.len(), 1);
    assert_eq!(response.results[0].reference.locator.record_index, Some(2));
    let reference = serde_json::to_value(&response.results[0].reference).unwrap();

    let mut wrong_pointer = reference.clone();
    wrong_pointer["locator"]["pointer"] = json!("/payload/content/0/text");
    let error = read(&config, wrong_pointer, 0, 0, None);
    assert_eq!(error.code, Some(codes::REF_LOCATOR_MISSING));

    // A different line with the same pointer is not silently accepted.
    let mut wrong_line = reference.clone();
    wrong_line["locator"]["record_index"] = json!(1);
    let error = read(&config, wrong_line, 0, 0, None);
    assert_eq!(error.code, Some(codes::REF_STALE));

    let mut past_eof = reference.clone();
    past_eof["locator"]["record_index"] = json!(999);
    let error = read(&config, past_eof, 0, 0, None);
    assert_eq!(error.code, Some(codes::REF_LOCATOR_MISSING));

    // A zero-byte range would otherwise hash to the empty-string digest and
    // pass while returning another line's text.
    let mut empty_range = reference.clone();
    empty_range["locator"]["record_index"] = json!(1);
    empty_range["text_range"] = json!({
        "line_start": 0, "line_end": 0, "byte_start": 0, "byte_end": 0
    });
    empty_range["fingerprint"]["range_sha256"] =
        json!(agent_memory::recall::hash::sha256_hex(b""));
    let error = read(&config, empty_range, 0, 0, None);
    assert_eq!(error.code, Some(codes::REF_OUT_OF_RANGE));

    let mut out_of_range = reference.clone();
    out_of_range["text_range"]["byte_end"] = json!(10_000);
    let error = read(&config, out_of_range, 0, 0, None);
    assert_eq!(error.code, Some(codes::REF_OUT_OF_RANGE));

    let mut wrong_schema = reference.clone();
    wrong_schema["schema_gen"] = json!("sqlite-v2");
    wrong_schema["locator"] = json!({
        "session_id": SESSION_A,
        "session_message_id": "sm",
        "pointer": "/text"
    });
    let error = read(&config, wrong_schema, 0, 0, None);
    assert_eq!(error.code, Some(codes::REF_SOURCE_MISMATCH));

    let mut wrong_root = reference.clone();
    wrong_root["root_sha256"] = json!("00".repeat(32));
    let error = read(&config, wrong_root, 0, 0, None);
    assert_eq!(error.code, Some(codes::REF_ROOT_MISMATCH));

    let mut missing_index = reference.clone();
    missing_index["locator"]
        .as_object_mut()
        .unwrap()
        .remove("record_index");
    let parse_error = ReadRequest::parse(&json!({"ref": missing_index})).unwrap_err();
    assert_eq!(parse_error.code, codes::INVALID_REQUEST);
    assert_eq!(parse_error.detail.unwrap()["path"], "ref.locator.record_index");

    let mut bad_pointer = reference.clone();
    bad_pointer["locator"]["pointer"] = json!("payload/content");
    let parse_error = ReadRequest::parse(&json!({"ref": bad_pointer})).unwrap_err();
    assert_eq!(parse_error.detail.unwrap()["path"], "ref.locator.pointer");

    // Change one byte of the target line: the range fingerprint must reject it.
    let rollout = std::fs::read_dir(home.join("sessions/2026/03/06"))
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .find(|path| path.extension().and_then(|ext| ext.to_str()) == Some("jsonl"))
        .expect("rollout file");
    let text = std::fs::read_to_string(&rollout).unwrap();
    let tampered = text.replacen("codexneedle answer", "codexneedle answed", 1);
    assert_ne!(tampered, text);
    std::fs::write(&rollout, tampered).unwrap();
    let error = read(&config, reference, 0, 0, None);
    assert_eq!(error.code, Some(codes::REF_STALE));
}

#[test]
fn a_half_written_final_line_is_a_race_not_an_error() {
    let env = support::TestEnv::new();
    let home = env.root("codex-half-line");
    let dir = home.join("sessions/2026/03/06");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("rollout-2026-03-06T21-50-12-{SESSION_A}.jsonl"));
    let mut text = String::new();
    text.push_str(&support::codex_meta(SESSION_A));
    text.push('\n');
    text.push_str(&support::codex_user("codexneedle question"));
    text.push('\n');
    // A live writer is mid-record: no trailing newline and invalid JSON.
    text.push_str("{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",");
    std::fs::write(&path, text).unwrap();

    let config = single_source(&env, "codex-half-line", &home);
    let response = search(&config, json!({"query": "codexneedle"}));
    assert_eq!(status_of(&response, "cx").status, status::OK);
    assert_eq!(response.results.len(), 1);
    assert!(detail(&response, "cx", "CONTENT_UNRESOLVED").is_none());
    let readback = read(
        &config,
        serde_json::to_value(&response.results[0].reference).unwrap(),
        0,
        0,
        None,
    );
    assert!(readback.ok, "{:?}", readback.code);
}

#[test]
fn compressed_readback_fails_at_the_decompressed_scan_limit() {
    use std::io::Write;

    let env = support::TestEnv::new();
    let home = env.root("codex-zst-limit");
    let config = single_source(&env, "codex-zst-limit", &home);
    let dir = home.join("sessions/2026/03/06");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("rollout-2026-03-06T21-50-12-{SESSION_A}.jsonl.zst"));
    let file = std::fs::File::create(&path).unwrap();
    let mut encoder = zstd::stream::write::Encoder::new(file, 3).unwrap();
    let chunk = vec![b'x'; 512 * 1024];
    // > 64 MiB decompressed: the reader must stop before the record.
    for _ in 0..130 {
        encoder.write_all(&chunk).unwrap();
    }
    encoder.finish().unwrap();

    let reference = json!({
        "source_id": "cx",
        "source_kind": "codex",
        "root_sha256": root_sha256(&home.to_string_lossy()),
        "rel_path": format!("sessions/2026/03/06/rollout-2026-03-06T21-50-12-{SESSION_A}.jsonl"),
        "schema_gen": SCHEMA_CODEX_ROLLOUT_V1,
        "locator": {
            "session_id": SESSION_A,
            "record_index": 200,
            "pointer": "/payload/content"
        },
        "fingerprint": {"range_sha256": "00".repeat(32), "row_sha256": "00".repeat(32)},
        "text_range": {"line_start": 0, "line_end": 1, "byte_start": 0, "byte_end": 0}
    });
    let response = read(&config, reference, 0, 0, None);
    assert!(!response.ok);
    assert_eq!(response.code, Some(codes::REF_READ_FAILED));
}

#[test]
fn session_filter_enumerates_then_reads_only_the_target() {
    let env = support::TestEnv::new();
    let home = env.root("codex-filter");
    // Session A carries an interior damaged line; the target session B is clean.
    let lines_a = vec![
        support::codex_meta(SESSION_A),
        support::codex_user("codexneedle a"),
        "{not valid json}".to_string(),
    ];
    support::build_codex_rollout(&home, SESSION_A, &lines_a);
    let lines_b = vec![
        support::codex_meta(SESSION_B),
        support::codex_user("codexneedle b"),
        support::codex_assistant("codexneedle b answer"),
    ];
    support::build_codex_rollout(&home, SESSION_B, &lines_b);
    let config = single_source(&env, "codex-filter", &home);

    let filtered = search(
        &config,
        json!({"query": "codexneedle", "session_id": SESSION_B}),
    );
    assert_eq!(status_of(&filtered, "cx").status, status::OK);
    assert_eq!(filtered.results.len(), 2);
    assert!(filtered
        .results
        .iter()
        .all(|hit| hit.reference.locator.session_id.as_deref() == Some(SESSION_B)));
    let coverage = detail(&filtered, "cx", "coverage").expect("coverage");
    assert_eq!(coverage["total_sessions"], json!(1));
    assert_eq!(coverage["scanned_sessions"], json!(1));
    assert_eq!(coverage["session_filter"], json!(SESSION_B));
    assert_eq!(filtered.exit_code, 0);

    // The damaged non-target session must not poison the filtered read.
    assert!(!filtered
        .pagination
        .sources_incomplete
        .contains(&"cx".to_string()));
    assert!(filtered.errors.is_empty());

    // Filtering to the damaged session still reports its damage.
    let damaged = search(
        &config,
        json!({"query": "codexneedle", "session_id": SESSION_A}),
    );
    assert_eq!(status_of(&damaged, "cx").status, status::PARTIAL);
    assert_eq!(status_of(&damaged, "cx").code, Some(codes::RECORD_SKIPPED));

    // Zero matches: empty + exit 0, no budget detail.
    let zero = search(
        &config,
        json!({"query": "codexneedle", "session_id": "no-such-session"}),
    );
    assert_eq!(status_of(&zero, "cx").status, status::EMPTY);
    assert!(zero.results.is_empty());
    assert_eq!(zero.exit_code, 0);
    assert!(detail(&zero, "cx", "budget").is_none());
    assert_eq!(
        detail(&zero, "cx", "coverage").unwrap()["total_sessions"],
        json!(0)
    );
}

#[test]
fn session_filter_does_not_exempt_the_budget() {
    let env = support::TestEnv::new();
    let home = env.root("codex-filter-budget");
    let mut huge = vec![support::codex_meta(SESSION_A)];
    for index in 0..100 {
        huge.push(support::codex_user(&format!(
            "codexneedle line {index} {}",
            "z".repeat(160)
        )));
    }
    support::build_codex_rollout(&home, SESSION_A, &huge);
    support::build_codex_rollout(
        &home,
        SESSION_B,
        &[
            support::codex_meta(SESSION_B),
            support::codex_user("codexneedle target one"),
            support::codex_assistant("codexneedle target two"),
        ],
    );
    let config = single_source(&env, "codex-filter-budget", &home);

    // The huge non-target session neither consumes bytes nor is scanned.
    let target = search(
        &config,
        json!({
            "query": "codexneedle",
            "session_id": SESSION_B,
            "budget": {"max_bytes_per_source": 8192}
        }),
    );
    assert_eq!(status_of(&target, "cx").status, status::OK);
    assert_eq!(target.results.len(), 2);
    let coverage = detail(&target, "cx", "coverage").unwrap();
    assert_eq!(coverage["total_sessions"], json!(1));
    assert_eq!(coverage["scanned_sessions"], json!(1));
    assert!(detail(&target, "cx", "budget").is_none());

    // The target itself is still subject to the budget.
    let truncated = search(
        &config,
        json!({
            "query": "codexneedle",
            "session_id": SESSION_A,
            "budget": {"max_bytes_per_source": 8192}
        }),
    );
    assert_eq!(status_of(&truncated, "cx").status, status::TRUNCATED);
    assert_eq!(status_of(&truncated, "cx").code, Some(codes::BUDGET_BYTES));
    let budget = detail(&truncated, "cx", "budget").expect("budget detail");
    assert_eq!(budget["reason"], json!(codes::BUDGET_BYTES));
    assert_eq!(detail(&truncated, "cx", "coverage").unwrap()["total_sessions"], json!(1));
}

#[test]
fn session_filter_never_changes_hit_ids() {
    let env = support::TestEnv::new();
    let (_home, config) = full_corpus(&env, "codex-filter-ids");
    let unfiltered = search(&config, json!({"query": "codexneedle", "limit": 50}));
    let filtered = search(
        &config,
        json!({"query": "codexneedle", "limit": 50, "session_id": SESSION_A}),
    );
    let ids = |response: &SearchResponse| {
        let mut ids: Vec<String> = response.results.iter().map(|hit| hit.hit_id.clone()).collect();
        ids.sort();
        ids
    };
    assert_eq!(ids(&unfiltered), ids(&filtered));
}

#[test]
fn titles_come_from_the_explicit_db_then_index() {
    let env = support::TestEnv::new();
    let home = env.root("codex-titles");
    let lines = vec![
        support::codex_meta(SESSION_A),
        support::codex_user("message title codexneedle"),
    ];
    support::build_codex_rollout(&home, SESSION_A, &lines);
    support::build_session_index(
        &home.join("session_index.jsonl"),
        &[(SESSION_A, "index title")],
    );
    support::build_state_db(
        &home.join("custom_state.sqlite"),
        &[(SESSION_A, "custom db title", "message title codexneedle")],
    );
    support::build_state_db(
        &home.join("state_5.sqlite"),
        &[(SESSION_A, "default db title", "message title codexneedle")],
    );

    // An explicit `db` path is honored (not only the default filename).
    let explicit_source = support::codex_source(
        "cx",
        &home,
        Some(&home.join("custom_state.sqlite")),
        None,
    );
    let explicit = load(&env, "codex-titles-explicit", vec![explicit_source], vec!["cx"]);
    let response = search(&explicit, json!({"query": "codexneedle"}));
    assert_eq!(response.results[0].title, "custom db title");

    // Without `db`, the default `<root>/state_5.sqlite` participates.
    let config = single_source(&env, "codex-titles", &home);
    let response = search(&config, json!({"query": "codexneedle"}));
    assert_eq!(response.results[0].title, "default db title");

    // Without a state DB, only the index participates.
    std::fs::remove_file(home.join("state_5.sqlite")).unwrap();
    let response = search(&config, json!({"query": "codexneedle"}));
    assert_eq!(response.results[0].title, "index title");

    // Neither: first user message.
    std::fs::remove_file(home.join("session_index.jsonl")).unwrap();
    let response = search(&config, json!({"query": "codexneedle"}));
    assert_eq!(response.results[0].title, "message title codexneedle");
}

#[test]
fn archived_sessions_are_read_only_when_explicitly_configured() {
    let env = support::TestEnv::new();
    let home = env.root("codex-archived");
    support::build_codex_rollout(
        &home,
        SESSION_A,
        &[
            support::codex_meta(SESSION_A),
            support::codex_user("codexneedle active"),
        ],
    );
    support::build_codex_rollout_in(
        &home,
        "archived_sessions",
        SESSION_B,
        &[
            support::codex_meta(SESSION_B),
            support::codex_user("codexneedle archived"),
        ],
    );

    let default_config = single_source(&env, "codex-archived", &home);
    let default = search(&default_config, json!({"query": "codexneedle"}));
    assert_eq!(default.results.len(), 1);
    assert_eq!(
        default.results[0].reference.locator.session_id.as_deref(),
        Some(SESSION_A)
    );

    let source = support::codex_source(
        "cx-archived",
        &home,
        None,
        Some(&home.join("archived_sessions")),
    );
    let explicit = load(
        &env,
        "codex-archived-explicit",
        vec![source],
        vec!["cx-archived"],
    );
    let response = search(&explicit, json!({"query": "codexneedle"}));
    assert_eq!(response.results.len(), 1);
    assert_eq!(
        response.results[0].reference.locator.session_id.as_deref(),
        Some(SESSION_B)
    );
}

#[test]
fn missing_storage_is_unavailable_not_an_empty_source() {
    let env = support::TestEnv::new();
    let home = env.root("codex-no-storage");
    let config = single_source(&env, "codex-no-storage", &home);
    let response = search(&config, json!({"query": "codexneedle"}));
    assert_eq!(status_of(&response, "cx").status, status::UNAVAILABLE);
    assert_eq!(status_of(&response, "cx").code, Some(codes::DB_UNAVAILABLE));
    assert_eq!(response.exit_code, 4);
    assert!(detail(&response, "cx", "reason").is_some());
}

#[test]
fn strict_snapshot_is_refused_without_copying_the_home() {
    let env = support::TestEnv::new();
    let (home, config) = full_corpus(&env, "codex-strict");
    std::fs::write(home.join("auth.json"), b"{\"token\":\"secret\"}").unwrap();
    let before = support::tree_state(env.path());
    let request = SearchRequest::parse(&json!({"query": "codexneedle"})).unwrap();

    let response = memory_search_with(
        &config,
        &request,
        &SearchOptions {
            strict_snapshot: true,
            eval_freeze_decay: false,
        },
    );
    assert_eq!(status_of(&response, "cx").status, status::UNAVAILABLE);
    assert_eq!(status_of(&response, "cx").code, Some(codes::DB_UNAVAILABLE));
    assert_eq!(
        detail(&response, "cx", "reason"),
        Some(&json!("strict_snapshot_unsupported_for_codex_home"))
    );
    assert!(response.results.is_empty());
    assert_eq!(support::tree_state(env.path()), before, "nothing may be copied");

    let normal = search(&config, json!({"query": "codexneedle"}));
    let reference = serde_json::to_value(&normal.results[0].reference).unwrap();
    let read_request = ReadRequest::parse(&json!({"ref": reference})).unwrap();
    let readback = memory_read_with(
        &config,
        &read_request,
        &ReadOptions {
            strict_snapshot: true,
        },
    );
    assert!(!readback.ok);
    assert_eq!(readback.code, Some(codes::DB_UNAVAILABLE));
    assert_eq!(support::tree_state(env.path()), before);
}

#[test]
fn live_append_search_reports_no_side_effects() {
    let env = support::TestEnv::new();
    let (_home, config) = full_corpus(&env, "codex-live");
    let response = search(&config, json!({"query": "codexneedle", "limit": 50}));
    assert!(response.side_effects.is_empty());
    assert_eq!(status_of(&response, "cx").status, status::OK);
}

#[test]
fn device_and_kb_report_session_filter_not_applicable() {
    let env = support::TestEnv::new();
    let device_root = env.root("filter-device");
    env.write("filter-device/2026-01-01_note.md", b"codexneedle device body");
    let kb_root = env.root("filter-kb");
    let source_device = env.md_source("dev", "device", &device_root);
    let source_kb = env.md_source("kb", "kb", &kb_root);
    let config = load(
        &env,
        "filter-nonsession",
        vec![source_device, source_kb],
        vec!["dev", "kb"],
    );
    let response = search(
        &config,
        json!({"query": "codexneedle", "session_id": SESSION_A}),
    );
    assert!(response.results.is_empty());
    assert_eq!(response.exit_code, 0);
    for source_id in ["dev", "kb"] {
        assert_eq!(status_of(&response, source_id).status, status::EMPTY);
        assert_eq!(
            detail(&response, source_id, "reason"),
            Some(&json!("session_filter_not_applicable"))
        );
        assert!(status_of(&response, source_id).code.is_none());
    }
}

#[test]
fn three_sources_interleave_and_paginate() {
    let env = support::TestEnv::new();
    let device_root = env.root("interleave-device");
    env.write(
        "interleave-device/2026-01-01_first.md",
        format!("{} device one", support::NEEDLE).as_bytes(),
    );
    env.write(
        "interleave-device/2026-01-02_second.md",
        format!("{} device two", support::NEEDLE).as_bytes(),
    );
    let opencode = support::build_v1_hits(&env, "interleave-open", 1, 2);

    let home = env.root("interleave-codex");
    support::build_codex_rollout(
        &home,
        SESSION_A,
        &[
            support::codex_meta(SESSION_A),
            support::codex_user(&format!("{} codex one", support::NEEDLE)),
            support::codex_user(&format!("{} codex two", support::NEEDLE)),
        ],
    );

    let config = load(
        &env,
        "interleave",
        vec![
            env.md_source("dev", "device", &device_root),
            env.opencode_source("oc", &opencode.root),
            support::codex_source("cx", &home, None, None),
        ],
        vec!["dev", "oc", "cx"],
    );

    let first = search(&config, json!({"query": support::NEEDLE, "limit": 3}));
    assert_eq!(first.exit_code, 0, "{:?}", first.errors);
    let order: Vec<&str> = first
        .results
        .iter()
        .map(|result| result.source_id.as_str())
        .collect();
    assert_eq!(order, vec!["dev", "oc", "cx"]);
    assert!(first.pagination.sources_incomplete.is_empty());
    assert_eq!(first.next_offset, Some(3));

    let second = search(
        &config,
        json!({"query": support::NEEDLE, "limit": 3, "offset": 3}),
    );
    let order: Vec<&str> = second
        .results
        .iter()
        .map(|result| result.source_id.as_str())
        .collect();
    assert_eq!(order, vec!["dev", "oc", "cx"]);
    assert!(!second.results.is_empty());
    assert!(second.pagination.sources_incomplete.is_empty());
    // Each source contributed exactly two hits.
    assert_eq!(second.next_offset, None);
}

#[test]
fn row_fingerprint_drift_is_a_warning_not_a_failure() {
    let env = support::TestEnv::new();
    let home = env.root("codex-row-drift");
    let lines = vec![
        support::codex_meta(SESSION_A),
        support::codex_user("codexneedle question"),
    ];
    let rollout = support::build_codex_rollout(&home, SESSION_A, &lines);
    let config = single_source(&env, "codex-row-drift", &home);

    let response = search(&config, json!({"query": "codexneedle"}));
    assert_eq!(response.results.len(), 1);
    let reference = serde_json::to_value(&response.results[0].reference).unwrap();

    // Formatting-only rewrite: the projection and its range hash are unchanged,
    // only the raw row bytes differ.
    let text = std::fs::read_to_string(&rollout).unwrap();
    let rewritten = text.replacen(
        r#""content":"codexneedle question""#,
        r#""content": "codexneedle question""#,
        1,
    );
    assert_ne!(rewritten, text);
    std::fs::write(&rollout, rewritten).unwrap();

    let readback = read(&config, reference, 0, 0, None);
    assert!(readback.ok, "{:?}", readback.code);
    assert_eq!(
        readback.warnings.len(),
        1,
        "row drift is a warning: {:?}",
        readback.warnings
    );
    assert_eq!(
        readback.warnings[0].code,
        agent_memory::recall::contract::WARN_FILE_STALE
    );
}

#[test]
fn readback_refuses_a_symlinked_carrier() {
    let env = support::TestEnv::new();
    let home = env.root("codex-symlink");
    let rollout = support::build_codex_rollout(
        &home,
        SESSION_A,
        &[
            support::codex_meta(SESSION_A),
            support::codex_user("codexneedle symlink"),
        ],
    );
    let config = single_source(&env, "codex-symlink", &home);
    let response = search(&config, json!({"query": "codexneedle"}));
    assert_eq!(response.results.len(), 1);
    let reference = serde_json::to_value(&response.results[0].reference).unwrap();

    // Move the carrier outside the configured home and put a link in its place.
    let outside = env.path().join("outside-rollout.jsonl");
    std::fs::rename(&rollout, &outside).expect("move carrier outside root");
    if symlink_file(&outside, &rollout).is_err() {
        // Windows without Developer Mode / symlink privilege: the reverse path
        // cannot be exercised on this host.
        return;
    }

    let readback = read(&config, reference, 0, 0, None);
    assert!(!readback.ok);
    assert_eq!(readback.code, Some(codes::REF_LOCATOR_MISSING));
}

#[test]
fn an_oversize_record_counts_the_bytes_it_read() {
    let env = support::TestEnv::new();
    let home = env.root("codex-oversize");
    let huge = format!("codexneedle {}", "z".repeat(20_000));
    support::build_codex_rollout(
        &home,
        SESSION_A,
        &[
            support::codex_meta(SESSION_A),
            support::codex_user(&huge),
            support::codex_assistant("codexneedle after the oversize record"),
        ],
    );
    let config = single_source(&env, "codex-oversize", &home);
    let response = search(
        &config,
        json!({
            "query": "codexneedle",
            "budget": {"max_bytes_per_source": 8192}
        }),
    );
    assert_eq!(status_of(&response, "cx").status, status::PARTIAL);
    assert_eq!(status_of(&response, "cx").code, Some(codes::RECORD_SKIPPED));
    assert_eq!(detail(&response, "cx", "reason"), Some(&json!("oversize")));
    // The drained record consumes the byte budget, so the scan stops instead of
    // silently under-reporting the bytes it read.
    assert!(response.results.is_empty());
    let budget = detail(&response, "cx", "budget").expect("budget detail");
    assert_eq!(budget["reason"], json!(codes::BUDGET_BYTES));
    assert!(
        budget["consumed_bytes"].as_u64().unwrap_or(0) >= 20_000,
        "drained bytes must be charged: {budget}"
    );
}

#[test]
fn readback_refuses_a_symlinked_parent_directory() {
    let env = support::TestEnv::new();
    let home = env.root("codex-dir-symlink");
    support::build_codex_rollout(
        &home,
        SESSION_A,
        &[
            support::codex_meta(SESSION_A),
            support::codex_user("codexneedle parent link"),
        ],
    );
    let config = single_source(&env, "codex-dir-symlink", &home);
    let response = search(&config, json!({"query": "codexneedle"}));
    assert_eq!(response.results.len(), 1);
    let reference = serde_json::to_value(&response.results[0].reference).unwrap();

    // Replace an intermediate directory with a link to the same tree outside
    // the configured root.
    let year = home.join("sessions/2026");
    let outside = env.path().join("outside-2026");
    std::fs::rename(&year, &outside).expect("move year dir outside root");
    if symlink_dir(&outside, &year).is_err() {
        return;
    }

    let readback = read(&config, reference, 0, 0, None);
    assert!(!readback.ok);
    assert_eq!(readback.code, Some(codes::REF_LOCATOR_MISSING));
}

#[cfg(windows)]
#[test]
fn unreadable_storage_root_is_unavailable_with_the_os_reason() {
    let env = support::TestEnv::new();
    let home = env.root("codex-acl-root");
    support::build_codex_rollout(
        &home,
        SESSION_A,
        &[
            support::codex_meta(SESSION_A),
            support::codex_user("codexneedle unreadable root"),
        ],
    );
    let storage = home.join("sessions");
    let Some(_deny) = deny_directory_listing(&storage) else {
        return;
    };
    let config = single_source(&env, "codex-acl-root", &home);
    let response = search(&config, json!({"query": "codexneedle"}));
    assert_eq!(status_of(&response, "cx").status, status::UNAVAILABLE);
    assert_eq!(status_of(&response, "cx").code, Some(codes::DB_UNAVAILABLE));
    assert_eq!(
        detail(&response, "cx", "reason"),
        Some(&json!("codex_storage_unreadable"))
    );
    assert!(response.results.is_empty());
    assert_eq!(response.exit_code, 4);
    assert!(
        response
            .errors
            .iter()
            .any(|entry| entry.message.contains("cannot be enumerated")),
        "the OS reason must be surfaced: {:?}",
        response.errors
    );
}

#[cfg(windows)]
#[test]
fn unreadable_subdirectory_is_partial_and_other_carriers_still_scan() {
    let env = support::TestEnv::new();
    let home = env.root("codex-acl-subdir");
    // Session A (denied) and session B (readable) live in separate years.
    support::build_codex_rollout(
        &home,
        SESSION_A,
        &[
            support::codex_meta(SESSION_A),
            support::codex_user("codexneedle denied"),
        ],
    );
    write_rollout_at(
        &home.join("sessions/2027/03/06"),
        &format!("rollout-2027-03-06T21-50-12-{SESSION_B}.jsonl"),
        &[
            support::codex_meta(SESSION_B),
            support::codex_user("codexneedle readable"),
        ],
    );
    let denied_dir = home.join("sessions/2026");
    let Some(_deny) = deny_directory_listing(&denied_dir) else {
        return;
    };
    let config = single_source(&env, "codex-acl-subdir", &home);
    let response = search(&config, json!({"query": "codexneedle"}));
    assert_eq!(status_of(&response, "cx").status, status::PARTIAL);
    assert_eq!(status_of(&response, "cx").code, Some(codes::RECORD_SKIPPED));
    assert!(!response.results.is_empty());
    assert!(response
        .results
        .iter()
        .all(|hit| hit.reference.locator.session_id.as_deref() == Some(SESSION_B)));
    let coverage = detail(&response, "cx", "coverage").expect("coverage");
    assert_eq!(coverage["scanned_sessions"], json!(1));
    assert_eq!(coverage["total_sessions"], json!(1));
}

#[cfg(windows)]
#[test]
fn locked_non_target_rollout_is_skipped_before_metadata_read() {
    let env = support::TestEnv::new();
    let home = env.root("codex-lock-nontarget");
    let locked = support::build_codex_rollout(
        &home,
        SESSION_A,
        &[
            support::codex_meta(SESSION_A),
            support::codex_user("codexneedle locked a"),
        ],
    );
    support::build_codex_rollout(
        &home,
        SESSION_B,
        &[
            support::codex_meta(SESSION_B),
            support::codex_user("codexneedle target one"),
            support::codex_assistant("codexneedle target two"),
        ],
    );
    let Some(_lock) = lock_exclusive(&locked) else {
        return;
    };
    let config = single_source(&env, "codex-lock-nontarget", &home);
    let response = search(
        &config,
        json!({"query": "codexneedle", "session_id": SESSION_B}),
    );
    assert_eq!(status_of(&response, "cx").status, status::OK);
    assert_eq!(response.exit_code, 0);
    assert_eq!(response.results.len(), 2);
    assert!(response
        .results
        .iter()
        .all(|hit| hit.reference.locator.session_id.as_deref() == Some(SESSION_B)));
    let coverage = detail(&response, "cx", "coverage").unwrap();
    assert_eq!(coverage["total_sessions"], json!(1));
    assert_eq!(coverage["scanned_sessions"], json!(1));
}

#[cfg(windows)]
#[test]
fn locked_target_rollout_fails_loud_instead_of_empty() {
    let env = support::TestEnv::new();
    let home = env.root("codex-lock-target");
    let locked = support::build_codex_rollout(
        &home,
        SESSION_B,
        &[
            support::codex_meta(SESSION_B),
            support::codex_user("codexneedle locked target"),
        ],
    );
    let Some(_lock) = lock_exclusive(&locked) else {
        return;
    };
    let config = single_source(&env, "codex-lock-target", &home);
    let response = search(
        &config,
        json!({"query": "codexneedle", "session_id": SESSION_B}),
    );
    assert_eq!(status_of(&response, "cx").status, status::PARTIAL);
    assert_eq!(status_of(&response, "cx").code, Some(codes::RECORD_SKIPPED));
    assert_ne!(status_of(&response, "cx").status, status::EMPTY);
    assert!(response.results.is_empty());
    assert_eq!(response.exit_code, 4);
    assert_eq!(
        detail(&response, "cx", "coverage").unwrap()["total_sessions"],
        json!(0)
    );
}

#[test]
fn non_target_without_filename_uuid_still_uses_metadata_filtering() {
    let env = support::TestEnv::new();
    let home = env.root("codex-no-uuid");
    write_rollout_at(
        &home.join("sessions/2026/03/06"),
        "rollout-no-uuid.jsonl",
        &[
            support::codex_meta(SESSION_A),
            support::codex_user("codexneedle no uuid"),
        ],
    );
    support::build_codex_rollout(
        &home,
        SESSION_B,
        &[
            support::codex_meta(SESSION_B),
            support::codex_user("codexneedle target"),
        ],
    );
    let config = single_source(&env, "codex-no-uuid", &home);
    let response = search(
        &config,
        json!({"query": "codexneedle", "session_id": SESSION_B}),
    );
    assert_eq!(status_of(&response, "cx").status, status::OK);
    assert_eq!(response.results.len(), 1);
    assert_eq!(
        response.results[0].reference.locator.session_id.as_deref(),
        Some(SESSION_B)
    );
    assert_eq!(
        detail(&response, "cx", "coverage").unwrap()["total_sessions"],
        json!(1)
    );
}

#[test]
fn records_beyond_the_shared_read_ceiling_are_not_emitted() {
    use std::io::Write;

    let env = support::TestEnv::new();
    let home = env.root("codex-ceiling-search");
    let dir = home.join("sessions/2027/03/06");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("rollout-2027-03-06T21-50-12-{SESSION_A}.jsonl"));
    let mut file = std::fs::File::create(&path).unwrap();
    writeln!(file, "{}", support::codex_meta(SESSION_A)).unwrap();
    let filler = "z".repeat(1_000_000);
    for index in 0..70 {
        writeln!(
            file,
            "{}",
            support::codex_user(&format!("filler {index} {filler}"))
        )
        .unwrap();
    }
    writeln!(file, "{}", support::codex_user("codexneedle beyond ceiling")).unwrap();
    drop(file);

    let config = single_source(&env, "codex-ceiling-search", &home);
    let response = search(
        &config,
        json!({
            "query": "codexneedle",
            "budget": {"max_bytes_per_source": 134_217_728}
        }),
    );
    assert_eq!(status_of(&response, "cx").status, status::PARTIAL);
    assert_eq!(status_of(&response, "cx").code, Some(codes::RECORD_SKIPPED));
    assert_eq!(detail(&response, "cx", "reason"), Some(&json!("read_limit")));
    assert!(response.results.is_empty(), "a hit past 64 MiB is unreadable");
    assert!(detail(&response, "cx", "budget").is_none());
}

#[test]
fn readback_is_confined_to_the_configured_storage_tree() {
    let env = support::TestEnv::new();
    let home = env.root("codex-storage-boundary");
    let rollout = support::build_codex_rollout(
        &home,
        SESSION_A,
        &[
            support::codex_meta(SESSION_A),
            support::codex_user("codexneedle storage boundary"),
        ],
    );
    let config = single_source(&env, "codex-storage-boundary", &home);
    let response = search(&config, json!({"query": "codexneedle"}));
    assert_eq!(response.results.len(), 1);
    let reference = serde_json::to_value(&response.results[0].reference).unwrap();
    assert!(read(&config, reference.clone(), 0, 0, None).ok);

    // A byte-identical copy under an unconfigured `archived_sessions` tree must
    // not be reachable through a hand-written ref.
    let file_name = rollout.file_name().unwrap().to_string_lossy().to_string();
    let archived_dir = home.join("archived_sessions/2026/03/06");
    std::fs::create_dir_all(&archived_dir).unwrap();
    let archived_path = archived_dir.join(&file_name);
    std::fs::copy(&rollout, &archived_path).unwrap();
    let archived_ref = {
        let mut value = reference.clone();
        let rel = reference["rel_path"].as_str().unwrap();
        value["rel_path"] = json!(rel.replacen("sessions/", "archived_sessions/", 1));
        value
    };
    let denied = read(&config, archived_ref.clone(), 0, 0, None);
    assert!(!denied.ok);
    assert_eq!(denied.code, Some(codes::REF_LOCATOR_MISSING));

    // The same applies when only the compressed sibling exists.
    let zst = support::build_codex_zst(&archived_path);
    std::fs::remove_file(&archived_path).unwrap();
    assert!(zst.is_file());
    let denied_zst = read(&config, archived_ref.clone(), 0, 0, None);
    assert!(!denied_zst.ok);
    assert_eq!(denied_zst.code, Some(codes::REF_LOCATOR_MISSING));

    // Explicitly configuring that tree makes it readable.
    let source = support::codex_source(
        "cx-archived",
        &home,
        None,
        Some(&home.join("archived_sessions")),
    );
    let archived_config = load(&env, "codex-storage-boundary-arch", vec![source], vec!["cx-archived"]);
    let archived_search = search(&archived_config, json!({"query": "codexneedle"}));
    assert_eq!(archived_search.results.len(), 1);
    let archived_read = read(
        &archived_config,
        serde_json::to_value(&archived_search.results[0].reference).unwrap(),
        0,
        0,
        None,
    );
    assert!(archived_read.ok, "{:?}", archived_read.code);
}
