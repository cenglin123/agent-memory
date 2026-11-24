//! P1 acceptance: Archive provider, access labels, session mapping, readback
//! and the derived retrieval skeleton.
//!
//! Every root is a temporary directory and every corpus is synthetic. No real
//! session, knowledge base or production Archive is read.

mod support;

use agent_memory::archive::{
    hex_sha256, Archive, Manifest, ManifestObject, ManifestRecord, ManifestSource,
    SourceFingerprint, MANIFEST_SCHEMA,
};
use agent_memory::capture::capture_source;
use agent_memory::recall::config::load_config;
use agent_memory::recall::contract::{codes, status};
use agent_memory::recall::hash::{root_sha256, sha256_hex};
use agent_memory::recall::read::{memory_read, ReadRequest, ReadResponse};
use agent_memory::recall::search::{memory_search, SearchRequest, SearchResponse};
use agent_memory::recall::semantic::model::{
    EmbeddingBackend, EmbeddingModelSpec, Normalization,
};
use agent_memory::replay::{
    replay, replay_with_generator, replay_with_plan, ReplayIdentity, RetrievalGenerator,
    RETRIEVAL_INDEX_SCHEMA, RETRIEVAL_UNIT_PARSER,
};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

const NEEDLE: &str = "needlealpha";

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

fn codex_lines(extra: &[String]) -> Vec<String> {
    let mut lines = vec![support::codex_meta("s-p1"), support::codex_user(NEEDLE)];
    lines.extend(extra.iter().cloned());
    lines
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

fn capture(fixture: &Fixture, path: &Path, domain: &str) {
    let report = capture_source(&fixture.archive, path, "codex", domain, None).unwrap();
    assert_eq!(report.status, "captured", "{report:?}");
}

fn v2_config(env: &support::TestEnv, name: &str, sources: Vec<Value>, order: Vec<&str>, domain: &str) -> PathBuf {
    let value = json!({
        "version": 2,
        "sources": sources,
        "source_order": order,
        "interpreter": {"kb": {"command": "C:/python/python.exe"}},
        "domain": domain,
        "telemetry": {"enabled": false}
    });
    let config = load_config_value(&value);
    assert!(config.is_ok(), "{:?}", config.err());
    env.write_json(name, &value)
}

fn load_config_value(value: &Value) -> Result<agent_memory::recall::RecallConfig, agent_memory::recall::contract::RecallError> {
    agent_memory::recall::RecallConfig::from_value(value)
}

fn archive_source(fixture: &Fixture) -> Value {
    json!({
        "source_id": "ar", "kind": "archive",
        "root": fixture.root.to_string_lossy(), "enabled": true
    })
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

// ---------------------------------------------------------------------------
// 1. Archive and live sources hit and read back independently
// ---------------------------------------------------------------------------

#[test]
fn archive_and_live_source_hit_and_read_back_independently() {
    let env = support::TestEnv::new();
    let fixture = fixture();
    let path = write_jsonl(&fixture, "rollout-live.jsonl", &codex_lines(&[]));
    capture(&fixture, &path, "default");

    let notes = env.root("device-notes");
    std::fs::write(notes.join("2026-10-29_note.md"), format!("{NEEDLE} device body\n")).unwrap();

    let config = v2_config(
        &env,
        "archive-live.json",
        vec![
            archive_source(&fixture),
            env.md_source("dev", "device", &notes),
        ],
        vec!["ar", "dev"],
        "default",
    );

    let response = search(&config, json!({"query": NEEDLE}));
    assert_eq!(response.exit_code, 0, "{}", serde_json::to_string(&response).unwrap());
    assert_eq!(response.results.len(), 2);
    let archive_hit = response
        .results
        .iter()
        .find(|hit| hit.source_kind == "archive")
        .expect("archive hit");
    let live_hit = response
        .results
        .iter()
        .find(|hit| hit.source_kind == "device")
        .expect("device hit");
    assert_eq!(archive_hit.reference.schema_gen, "archive-record-v1");
    assert_eq!(archive_hit.hit_id.len(), 64);
    assert_ne!(archive_hit.hit_id, live_hit.hit_id);

    // rel_path is the real object address; the locator carries only the record
    // hash and the projection offset.
    let rel_path = &archive_hit.reference.rel_path;
    let sha = rel_path.rsplit('/').next().unwrap().to_string();
    assert_eq!(
        rel_path,
        &format!("archive/objects/{}/{}/{}", &sha[0..2], &sha[2..4], sha)
    );
    assert!(archive_hit.reference.locator.record_sha256.is_some());
    assert_eq!(archive_hit.reference.locator.projection_offset, Some(0));
    assert!(archive_hit.reference.locator.session_id.is_none());
    assert!(archive_hit.reference.locator.pointer.is_none());

    for hit in &response.results {
        let back = read(&config, json!({"ref": hit.reference}));
        assert!(back.ok, "{:?}", back.code);
        assert!(back.text.as_deref().unwrap().contains(NEEDLE));
    }
}

// ---------------------------------------------------------------------------
// 2. Same content captured twice keeps hit_id and range_sha256
// ---------------------------------------------------------------------------

#[test]
fn same_content_captured_twice_keeps_hit_id_and_range_sha() {
    let env = support::TestEnv::new();
    let fixture = fixture();
    let path = write_jsonl(&fixture, "rollout-a.jsonl", &codex_lines(&[]));
    capture(&fixture, &path, "default");
    let config = v2_config(
        &env,
        "archive-twice.json",
        vec![archive_source(&fixture)],
        vec!["ar"],
        "default",
    );

    let first = search(&config, json!({"query": NEEDLE}));
    assert_eq!(first.results.len(), 1);
    let hit_id = first.results[0].hit_id.clone();
    let range_sha = first.results[0].reference.fingerprint.range_sha256.clone();

    // The same bytes under another source path: a second capture of identical
    // content must not create a second identity.
    let copy = write_jsonl(&fixture, "rollout-b.jsonl", &codex_lines(&[]));
    capture(&fixture, &copy, "default");

    let second = search(&config, json!({"query": NEEDLE}));
    assert_eq!(second.results.len(), 1, "identical content is one hit");
    assert_eq!(second.results[0].hit_id, hit_id);
    assert_eq!(second.results[0].reference.fingerprint.range_sha256, range_sha);
}

// ---------------------------------------------------------------------------
// 3. `.jsonl.zst` decodes to the same object baseline as the plain carrier
// ---------------------------------------------------------------------------

#[test]
fn compressed_and_plain_capture_share_identity_and_readback() {
    let env = support::TestEnv::new();
    let lines = codex_lines(&[]);

    let plain_fixture = fixture();
    let plain_path = write_jsonl(&plain_fixture, "rollout-plain.jsonl", &lines);
    capture(&plain_fixture, &plain_path, "default");
    let plain_config = v2_config(
        &env,
        "archive-plain.json",
        vec![archive_source(&plain_fixture)],
        vec!["ar"],
        "default",
    );

    let zst_fixture = fixture();
    let zst_source = write_jsonl(&zst_fixture, "rollout-zst.jsonl", &lines);
    let decoded = std::fs::read(&zst_source).unwrap();
    let encoded = zstd::stream::encode_all(decoded.as_slice(), 3).unwrap();
    let zst_path = zst_fixture.temp.path().join("rollout-zst.jsonl.zst");
    std::fs::write(&zst_path, encoded).unwrap();
    std::fs::remove_file(&zst_source).unwrap();
    capture(&zst_fixture, &zst_path, "default");
    let zst_config = v2_config(
        &env,
        "archive-zst.json",
        vec![archive_source(&zst_fixture)],
        vec!["ar"],
        "default",
    );

    let plain = search(&plain_config, json!({"query": NEEDLE}));
    let compressed = search(&zst_config, json!({"query": NEEDLE}));
    assert_eq!(plain.results.len(), 1);
    assert_eq!(compressed.results.len(), 1);
    // rel_path is the decoded object address, so both carriers share identity.
    assert_eq!(plain.results[0].hit_id, compressed.results[0].hit_id);
    assert_eq!(
        plain.results[0].reference.fingerprint.range_sha256,
        compressed.results[0].reference.fingerprint.range_sha256
    );

    let plain_back = read(&plain_config, json!({"ref": plain.results[0].reference}));
    let zst_back = read(&zst_config, json!({"ref": compressed.results[0].reference}));
    assert!(plain_back.ok && zst_back.ok);
    assert_eq!(plain_back.text, zst_back.text);
    assert!(plain_back.text.unwrap().contains(NEEDLE));
}

// ---------------------------------------------------------------------------
// 4. Access labels gate Archive records only
// ---------------------------------------------------------------------------

#[test]
fn domain_labels_gate_archive_records_but_never_live_sources() {
    let env = support::TestEnv::new();
    let fixture = fixture();
    let path = write_jsonl(&fixture, "rollout-work.jsonl", &codex_lines(&[]));
    capture(&fixture, &path, "work");

    let notes = env.root("device-domain");
    std::fs::write(notes.join("2026-10-29_domain.md"), format!("{NEEDLE} live\n")).unwrap();

    let config = v2_config(
        &env,
        "archive-domain.json",
        vec![
            archive_source(&fixture),
            env.md_source("dev", "device", &notes),
        ],
        vec!["ar", "dev"],
        "default",
    );

    // Implicit default + non-default-only labels: unreachable, never a healthy
    // empty result, and no default-domain substitution.
    let response = search(&config, json!({"query": NEEDLE}));
    assert_eq!(response.exit_code, 10, "live hit + unavailable archive");
    assert_eq!(response.results.len(), 1);
    assert_eq!(response.results[0].source_kind, "device");
    let archive_status = response
        .source_status
        .iter()
        .find(|entry| entry.source_id == "ar")
        .unwrap();
    assert_eq!(archive_status.status, status::UNAVAILABLE);
    assert_eq!(archive_status.code, Some(codes::DOMAIN_UNREACHABLE));
    assert_eq!(
        archive_status.detail.as_ref().unwrap()["reason"],
        json!("archive_records_not_readable")
    );

    // Explicit matching domain: both sources answer.
    let matching = search(&config, json!({"query": NEEDLE, "domain": "work"}));
    assert_eq!(matching.exit_code, 0);
    assert_eq!(matching.results.len(), 2);
    let archive_hit = matching
        .results
        .iter()
        .find(|hit| hit.source_kind == "archive")
        .unwrap();
    let back = read(&config, json!({"ref": archive_hit.reference, "domain": "work"}));
    assert!(back.ok);

    // A different explicit domain is denied, not silently emptied.
    let denied = search(&config, json!({"query": NEEDLE, "domain": "private"}));
    assert_eq!(denied.exit_code, 10);
    let archive_status = denied
        .source_status
        .iter()
        .find(|entry| entry.source_id == "ar")
        .unwrap();
    assert_eq!(archive_status.status, status::UNAVAILABLE);
    assert_eq!(archive_status.code, Some(codes::DOMAIN_DENIED));

    // Readback obeys the same predicate.
    let implicit_read = read(&config, json!({"ref": archive_hit.reference}));
    assert!(!implicit_read.ok);
    assert_eq!(implicit_read.code, Some(codes::DOMAIN_UNREACHABLE));
    assert_eq!(implicit_read.exit_code, 5);
    let denied_read = read(
        &config,
        json!({"ref": archive_hit.reference, "domain": "private"}),
    );
    assert_eq!(denied_read.code, Some(codes::DOMAIN_DENIED));
    assert_eq!(denied_read.exit_code, 5);
}

// ---------------------------------------------------------------------------
// 5. Empty access labels fail closed
// ---------------------------------------------------------------------------

#[test]
fn empty_access_labels_fail_closed() {
    let env = support::TestEnv::new();
    let fixture = fixture();
    let record = format!("{{\"type\":\"response_item\",\"payload\":{{\"text\":\"{NEEDLE}\"}}}}");
    let bytes = record.as_bytes();
    let (object_sha, _, _) = fixture.archive.put_object(bytes).unwrap();
    fixture
        .archive
        .commit_manifest(&manifest_with_access("cap-empty", bytes, &object_sha, None))
        .unwrap();
    let config = v2_config(
        &env,
        "archive-empty-labels.json",
        vec![archive_source(&fixture)],
        vec!["ar"],
        "default",
    );

    let response = search(&config, json!({"query": NEEDLE}));
    assert!(response.results.is_empty());
    assert_eq!(response.exit_code, 4);
    let entry = &response.source_status[0];
    assert_eq!(entry.status, status::UNAVAILABLE);
    assert_eq!(entry.code, Some(codes::DOMAIN_DENIED));

    let ref_value = json!({
        "source_id": "ar", "source_kind": "archive",
        "root_sha256": root_sha256(&fixture.root.to_string_lossy()),
        "rel_path": format!("archive/objects/{}/{}/{}", &object_sha[0..2], &object_sha[2..4], object_sha),
        "schema_gen": "archive-record-v1",
        "locator": {"record_sha256": hex_sha256(bytes), "projection_offset": 0},
        "fingerprint": {"range_sha256": sha256_hex(bytes)},
        "text_range": {"line_start": 0, "line_end": 1, "byte_start": 0, "byte_end": bytes.len()}
    });
    let back = read(&config, json!({"ref": ref_value}));
    assert!(!back.ok);
    assert_eq!(back.code, Some(codes::DOMAIN_DENIED));
}

fn manifest_with_access(
    capture_id: &str,
    record: &[u8],
    object_sha: &str,
    access: Option<Vec<String>>,
) -> Manifest {
    let access = access.unwrap_or_default();
    Manifest {
        schema: MANIFEST_SCHEMA.to_string(),
        capture_id: capture_id.to_string(),
        captured_at_ms: 1_700_000_000_000,
        device_id: "device-test".to_string(),
        provider: "codex".to_string(),
        source: ManifestSource {
            kind: "jsonl_file".to_string(),
            path: "C:/synthetic/rollout.jsonl".to_string(),
            compressed: false,
            fingerprint: SourceFingerprint {
                mtime_ms: 1,
                len: record.len() as u64,
                head_sha256: hex_sha256(record),
                tail_sha256: hex_sha256(record),
            },
            source_state_at_capture: "captured".to_string(),
            capture_reason: "initial".to_string(),
            generation: 1,
            range_start: 0,
            carrier: None,
            sqlite: None,
        },
        objects: vec![ManifestObject {
            sha256: object_sha.to_string(),
            bytes: record.len() as u64,
            role: "whole_file".to_string(),
            access: access.clone(),
            source_offset: Some(0),
            generation: 1,
        }],
        records: vec![ManifestRecord {
            seq: 0,
            offset: 0,
            len: record.len() as u64,
            sha256: hex_sha256(record),
            object: object_sha.to_string(),
            access,
            source_offset: Some(0),
            source_line: Some(1),
        }],
        attachments: Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// 6. Missing object / schema generation mismatch are explicit
// ---------------------------------------------------------------------------

#[test]
fn missing_object_and_generation_mismatch_are_explicit() {
    let env = support::TestEnv::new();
    let fixture = fixture();
    let path = write_jsonl(&fixture, "rollout-missing.jsonl", &codex_lines(&[]));
    capture(&fixture, &path, "default");
    let config = v2_config(
        &env,
        "archive-missing.json",
        vec![archive_source(&fixture)],
        vec!["ar"],
        "default",
    );

    let hit = search(&config, json!({"query": NEEDLE}));
    assert_eq!(hit.results.len(), 1);
    let object_sha = hit.results[0]
        .reference
        .rel_path
        .rsplit('/')
        .next()
        .unwrap()
        .to_string();
    let reference = hit.results[0].reference.clone();
    let (object_path, _) = support::read_object_file(&fixture.root, &object_sha);
    std::fs::remove_file(&object_path).unwrap();

    let missing = search(&config, json!({"query": NEEDLE}));
    assert!(missing.results.is_empty());
    assert_eq!(missing.exit_code, 4);
    assert_eq!(missing.source_status[0].status, status::UNAVAILABLE);
    assert_eq!(missing.source_status[0].code, Some(codes::DB_UNAVAILABLE));
    assert_eq!(
        missing.source_status[0].detail.as_ref().unwrap()["reason"],
        json!("archive_object_missing")
    );
    let back = read(&config, json!({"ref": reference}));
    assert!(!back.ok);
    assert_eq!(back.code, Some(codes::REF_LOCATOR_MISSING));

    // A schema/derived generation written by another version is refused instead
    // of being read with the current rules.
    std::fs::write(
        fixture.root.join("state").join("schema.json"),
        json!({"schema_version": 99, "derived": {"parser_version": "x", "model_version": 1}})
            .to_string(),
    )
    .unwrap();
    let mismatch = search(&config, json!({"query": NEEDLE}));
    assert_eq!(mismatch.source_status[0].code, Some(codes::SCHEMA_UNSUPPORTED));
    assert_eq!(
        mismatch.source_status[0].detail.as_ref().unwrap()["reason"],
        json!("archive_schema_generation_mismatch")
    );
}

// ---------------------------------------------------------------------------
// 7. Session mapping: unknown is unmapped, then derived; identity mismatch is
//    explicit
// ---------------------------------------------------------------------------

#[test]
fn session_mapping_is_unmapped_until_the_derived_layer_says_otherwise() {
    let env = support::TestEnv::new();
    let fixture = fixture();
    let path = write_jsonl(&fixture, "rollout-session.jsonl", &codex_lines(&[]));
    capture(&fixture, &path, "default");
    let config = v2_config(
        &env,
        "archive-session.json",
        vec![archive_source(&fixture)],
        vec!["ar"],
        "default",
    );

    // No derived layer: a filter cannot be satisfied and is never guessed.
    let unmapped = search(&config, json!({"query": NEEDLE, "session_id": "rollout-session"}));
    assert!(unmapped.results.is_empty());
    assert_eq!(unmapped.exit_code, 0);
    assert_eq!(unmapped.source_status[0].status, status::EMPTY);
    assert_eq!(
        unmapped.source_status[0].detail.as_ref().unwrap()["reason"],
        json!("archive_session_unmapped")
    );
    assert_eq!(
        unmapped.source_status[0].detail.as_ref().unwrap()["archive"]["session_mapping"],
        json!("absent")
    );

    // The derived replay states the ownership; the filter now matches.
    let identity = ReplayIdentity::current();
    let report = replay(&fixture.archive, &identity, "default").unwrap();
    assert!(report.ok, "{report:?}");
    let mapped = search(&config, json!({"query": NEEDLE, "session_id": "rollout-session"}));
    assert_eq!(mapped.results.len(), 1, "{}", serde_json::to_string(&mapped).unwrap());
    assert_eq!(
        mapped.source_status[0].detail.as_ref().unwrap()["archive"]["session_mapping"],
        json!("ok")
    );

    // Another parser generation is an explicit mismatch, not a silent reuse.
    let derived_state = fixture.root.join("derived").join("state.json");
    let mut state: Value = serde_json::from_slice(&std::fs::read(&derived_state).unwrap()).unwrap();
    state["generations"][0]["identity"]["parserVersion"] = json!("9.9.9");
    std::fs::write(&derived_state, serde_json::to_vec(&state).unwrap()).unwrap();
    let stale = search(&config, json!({"query": NEEDLE, "session_id": "rollout-session"}));
    assert!(stale.results.is_empty());
    assert_eq!(
        stale.source_status[0].detail.as_ref().unwrap()["archive"]["session_mapping"],
        json!("identity_mismatch")
    );
}

// ---------------------------------------------------------------------------
// 8. Derived retrieval skeleton: identity, access, byte-identical rebuild
// ---------------------------------------------------------------------------

fn embedding_spec() -> EmbeddingModelSpec {
    // The test-only fake backend: deterministic vectors with no model, no
    // network and no subprocess. P3 requires a real embedding plan for an
    // embedding generation, so the P1 skeleton assertion now runs through
    // `replay_with_plan` (the identity/access/rebuild contract is unchanged).
    EmbeddingModelSpec {
        model_id: "fake-hash-v1".to_string(),
        dimension: 64,
        normalization: Normalization::L2,
        backend: EmbeddingBackend::Fake,
        model_path: None,
        endpoint: None,
    }
}

#[test]
fn retrieval_index_carries_generator_identity_and_rebuilds_identically() {
    let fixture = fixture();
    let path = write_jsonl(
        &fixture,
        "rollout-index.jsonl",
        &codex_lines(&[support::codex_assistant(&format!("{NEEDLE} answer"))]),
    );
    capture(&fixture, &path, "default");

    let identity = ReplayIdentity::current();
    let spec = embedding_spec();
    let plan = agent_memory::recall::semantic::EmbeddingPlan::for_spec(spec.clone());
    let generator = RetrievalGenerator::embedding(&spec);
    let first = replay_with_plan(&fixture.archive, &identity, "default", &plan).unwrap();
    let entry = first.retrieval.clone().expect("retrieval index");
    assert!(entry.path.starts_with("derived/generations/g1/retrieval/"));
    assert!(entry.units >= 2, "{entry:?}");
    assert!(entry.vectors.is_some(), "embedding generation writes vectors");
    assert_eq!(first.semantic.as_ref().unwrap().status, "available");

    let index_path = fixture.root.join(entry.path.replace('/', std::path::MAIN_SEPARATOR_STR));
    let first_bytes = std::fs::read(&index_path).unwrap();
    let document: Value = serde_json::from_slice(&first_bytes).unwrap();
    assert_eq!(document["schema"], json!(RETRIEVAL_INDEX_SCHEMA));
    assert_eq!(document["unitParser"], json!(RETRIEVAL_UNIT_PARSER));
    assert_eq!(document["generator"]["modelId"], json!("fake-hash-v1"));
    assert_eq!(document["generator"]["dimension"], json!(64));
    assert_eq!(document["generator"]["normalization"], json!("l2"));
    assert_eq!(
        document["generator"]["generationKey"],
        json!("embed-v1;model=fake-hash-v1;dim=64;norm=l2;backend=fake")
    );
    assert_eq!(document["access"], json!(["default"]));
    assert_eq!(document["vectors"]["quantization"], json!("int4"));
    assert_eq!(document["vectors"]["dimension"], json!(64));
    assert_eq!(document["vectors"]["count"], json!(entry.units));
    assert_eq!(document["sources"][0]["sessionMapping"], json!("mapped"));
    assert_eq!(document["unitsTable"]["count"], json!(entry.units));
    let units_path = fixture.root.join(
        document["unitsTable"]["path"]
            .as_str()
            .unwrap()
            .replace('/', std::path::MAIN_SEPARATOR_STR),
    );
    let units_bytes = std::fs::read(&units_path).unwrap();
    assert_eq!(
        agent_memory::archive::hex_sha256(&units_bytes),
        document["unitsTable"]["sha256"].as_str().unwrap()
    );
    let rows = agent_memory::recall::semantic::index::decode_unit_table(&units_bytes).unwrap();
    assert_eq!(rows.len() as u64, entry.units);
    assert!(rows.iter().all(|unit| unit.access == vec!["default".to_string()]));
    assert!(rows.iter().any(|unit| unit.turn >= 1));
    assert!(rows.iter().all(|unit| unit.unit_sha256.len() == 64));
    // The index never stores a timestamp, capture id or generation number, so
    // deleting the derived tree cannot change the rebuilt bytes.
    let text = String::from_utf8(first_bytes.clone()).unwrap();
    assert!(!text.contains("createdAtMs"));
    assert!(!text.contains("capturedAtMs"));
    assert!(!text.contains("captureId"));

    let vectors_path = fixture
        .root
        .join(entry.vectors.as_ref().unwrap().path.replace('/', std::path::MAIN_SEPARATOR_STR));
    let first_vectors = std::fs::read(&vectors_path).unwrap();

    std::fs::remove_dir_all(fixture.root.join("derived")).unwrap();
    // The generator identity must match the plan: the two-argument P1 entry point
    // now refuses an embedding generation instead of writing a vectorless index.
    let refused = replay_with_generator(&fixture.archive, &identity, "default", &generator);
    assert!(matches!(
        refused,
        Err(agent_memory::archive::ArchiveError::Embedding(_))
    ));
    let second = replay_with_plan(&fixture.archive, &identity, "default", &plan).unwrap();
    let entry = second.retrieval.expect("retrieval index after rebuild");
    assert_eq!(entry.path, "derived/generations/g1/retrieval/index.json");
    let second_bytes = std::fs::read(&index_path).unwrap();
    assert_eq!(first_bytes, second_bytes, "rebuild must be byte-identical");
    let second_vectors = std::fs::read(&vectors_path).unwrap();
    assert_eq!(
        first_vectors, second_vectors,
        "fake vectors must be byte-identical after a rebuild"
    );
}

// ---------------------------------------------------------------------------
// 9. Context, max_bytes, stale fail-closed and pagination
// ---------------------------------------------------------------------------

#[test]
fn context_max_bytes_stale_and_pagination_follow_the_read_contract() {
    let env = support::TestEnv::new();
    let fixture = fixture();
    let lines = vec![
        support::codex_meta("s-ctx"),
        support::codex_user(&format!("{NEEDLE} first")),
        support::codex_assistant(&format!("{NEEDLE} second")),
    ];
    let path = write_jsonl(&fixture, "rollout-ctx.jsonl", &lines);
    capture(&fixture, &path, "default");
    let config = v2_config(
        &env,
        "archive-ctx.json",
        vec![archive_source(&fixture)],
        vec!["ar"],
        "default",
    );

    let page1 = search(&config, json!({"query": NEEDLE, "limit": 1}));
    assert_eq!(page1.results.len(), 1);
    assert_eq!(page1.next_offset, Some(1));
    let page2 = search(&config, json!({"query": NEEDLE, "limit": 1, "offset": 1}));
    assert_eq!(page2.results.len(), 1);
    assert_ne!(page1.results[0].hit_id, page2.results[0].hit_id);

    let reference = page1.results[0].reference.clone();
    let window = read(
        &config,
        json!({"ref": reference, "context_before": 1, "context_after": 1}),
    );
    assert!(window.ok, "{:?}", window.code);
    assert_eq!(window.segments.len(), 3, "{window:?}");
    assert!(window.text.is_none(), "multi-segment omits the shortcut keys");

    let narrow = read(&config, json!({"ref": reference, "max_bytes": 8}));
    assert!(narrow.ok);
    assert!(narrow.truncated);
    assert!(narrow.text.as_deref().unwrap().len() <= 8);

    // A stale range is refused instead of returning different bytes.
    let mut stale = page1.results[0].reference.clone();
    stale.source_id = "ar".to_string();
    let mut value = serde_json::to_value(&stale).unwrap();
    value["fingerprint"]["range_sha256"] = json!("0".repeat(64));
    let stale_read = read(&config, json!({"ref": value}));
    assert_eq!(stale_read.code, Some(codes::REF_STALE));
    assert_eq!(stale_read.exit_code, 5);

    // rel_path must be the real object address of the record's object.
    let mut foreign = serde_json::to_value(&page1.results[0].reference).unwrap();
    foreign["rel_path"] = json!(format!("archive/objects/00/00/{}", "0".repeat(64)));
    let foreign_read = read(&config, json!({"ref": foreign}));
    assert_eq!(foreign_read.code, Some(codes::REF_LOCATOR_MISSING));
}

// ---------------------------------------------------------------------------
// 10. A record without a UTF-8 projection is counted, never a silent skip
// ---------------------------------------------------------------------------

#[test]
fn non_utf8_records_are_reported_as_unresolved() {
    let env = support::TestEnv::new();
    let fixture = fixture();
    let path = fixture.temp.path().join("rollout-binary.jsonl");
    // A record whose bytes are not UTF-8 cannot be projected; the raw bytes are
    // still archived and counted explicitly.
    std::fs::write(&path, b"\xff\xfe needlealpha\n").unwrap();
    let report = capture_source(&fixture.archive, &path, "codex", "default", None).unwrap();
    assert_eq!(report.status, "captured", "{report:?}");
    let config = v2_config(
        &env,
        "archive-binary.json",
        vec![archive_source(&fixture)],
        vec!["ar"],
        "default",
    );

    let response = search(&config, json!({"query": NEEDLE}));
    assert!(response.results.is_empty());
    assert_eq!(response.exit_code, 4);
    assert_eq!(response.source_status[0].status, status::PARTIAL);
    assert_eq!(response.source_status[0].code, Some(codes::CONTENT_UNRESOLVED));
    assert_eq!(
        response.source_status[0].detail.as_ref().unwrap()["CONTENT_UNRESOLVED"],
        json!(1)
    );
}

// ---------------------------------------------------------------------------
// 10. A hand-written ref can never panic or return a foreign span
// ---------------------------------------------------------------------------

#[test]
fn malformed_ranges_and_carriers_are_rejected_without_panicking() {
    let env = support::TestEnv::new();
    let fixture = fixture();
    let lines = vec![
        support::codex_meta("s-range"),
        // A multibyte first character makes a mid-character byte range possible.
        support::codex_user(&format!("中{NEEDLE} 范围")),
    ];
    let path = write_jsonl(&fixture, "rollout-range.jsonl", &lines);
    capture(&fixture, &path, "default");
    let config = v2_config(
        &env,
        "archive-range.json",
        vec![archive_source(&fixture)],
        vec!["ar"],
        "default",
    );

    let hit = search(&config, json!({"query": NEEDLE}));
    assert_eq!(hit.results.len(), 1);
    let base = serde_json::to_value(&hit.results[0].reference).unwrap();
    let text_bytes = {
        let back = read(&config, json!({"ref": hit.results[0].reference}));
        back.text.unwrap().into_bytes()
    };
    let cjk = String::from_utf8(text_bytes.clone())
        .unwrap()
        .find('中')
        .expect("multibyte character present");

    // Empty span with the trivial empty-range hash: must be REF_OUT_OF_RANGE.
    let mut empty = base.clone();
    empty["locator"]["projection_offset"] = json!(1);
    empty["text_range"]["byte_start"] = json!(1);
    empty["text_range"]["byte_end"] = json!(1);
    empty["fingerprint"]["range_sha256"] = json!(sha256_hex(b""));
    let response = read(&config, json!({"ref": empty}));
    assert!(!response.ok);
    assert_eq!(response.code, Some(codes::REF_OUT_OF_RANGE));

    // A range that splits the multibyte character: rejected, never a panic.
    let mut split = base.clone();
    split["locator"]["projection_offset"] = json!(cjk + 1);
    split["text_range"]["byte_start"] = json!(cjk + 1);
    split["text_range"]["byte_end"] = json!(cjk + 2);
    split["fingerprint"]["range_sha256"] = json!(sha256_hex(&text_bytes[cjk + 1..cjk + 2]));
    let response = read(&config, json!({"ref": split}));
    assert!(!response.ok);
    assert_eq!(response.code, Some(codes::REF_OUT_OF_RANGE));

    // A ref pointing at another object address is refused.
    let mut foreign = base.clone();
    foreign["rel_path"] = json!(format!("archive/objects/00/00/{}", "0".repeat(64)));
    let response = read(&config, json!({"ref": foreign}));
    assert_eq!(response.code, Some(codes::REF_LOCATOR_MISSING));
}

// ---------------------------------------------------------------------------
// 11. The referenced record always gets budget room
// ---------------------------------------------------------------------------

#[test]
fn context_cannot_push_the_target_past_the_byte_budget() {
    let env = support::TestEnv::new();
    let fixture = fixture();
    let lines = vec![
        support::codex_meta("s-budget"),
        support::codex_user(&format!("{NEEDLE} first record")),
        support::codex_assistant(&format!("{NEEDLE} target record")),
    ];
    let path = write_jsonl(&fixture, "rollout-budget.jsonl", &lines);
    capture(&fixture, &path, "default");
    let config = v2_config(
        &env,
        "archive-budget.json",
        vec![archive_source(&fixture)],
        vec!["ar"],
        "default",
    );

    let page = search(&config, json!({"query": NEEDLE, "limit": 100}));
    let target = page
        .results
        .iter()
        .find(|hit| hit.snippet.contains("target record"))
        .expect("target hit");
    // Budget only large enough for the target: the preceding context must be
    // dropped instead of consuming the target's room.
    let max_bytes = target
        .reference
        .text_range
        .byte_end
        .saturating_sub(target.reference.text_range.byte_start) as usize;
    let response = read(
        &config,
        json!({"ref": target.reference, "context_before": 1, "max_bytes": max_bytes}),
    );
    assert!(response.ok, "{:?}", response.code);
    assert!(response.truncated);
    let first = &response.segments[0];
    assert_eq!(first.locator, target.reference.locator);
    assert!(first.text.contains("target record"));
}

// ---------------------------------------------------------------------------
// 12. Two carriers of the same record are two refs and both read back
// ---------------------------------------------------------------------------

#[test]
fn duplicate_record_in_a_second_carrier_reads_back_from_its_own_object() {
    let env = support::TestEnv::new();
    let fixture = fixture();
    // Two different carriers of the same record line: object A also holds the
    // session meta line, object B holds only the shared line, so the carriers
    // are distinct objects with identical record bytes.
    let shared = support::codex_user(&format!("{NEEDLE} shared line"));
    let first = write_jsonl(
        &fixture,
        "rollout-carrier-a.jsonl",
        &[support::codex_meta("s-carrier"), shared.clone()],
    );
    capture(&fixture, &first, "default");
    let second = write_jsonl(&fixture, "rollout-carrier-b.jsonl", &[shared]);
    capture(&fixture, &second, "default");
    let config = v2_config(
        &env,
        "archive-carriers.json",
        vec![archive_source(&fixture)],
        vec!["ar"],
        "default",
    );

    let response = search(&config, json!({"query": NEEDLE}));
    assert_eq!(response.results.len(), 2, "two carriers, two object addresses");
    let mut rel_paths: Vec<String> = response
        .results
        .iter()
        .map(|hit| hit.reference.rel_path.clone())
        .collect();
    rel_paths.sort();
    rel_paths.dedup();
    assert_eq!(rel_paths.len(), 2);
    for hit in &response.results {
        let back = read(&config, json!({"ref": hit.reference}));
        assert!(back.ok, "{:?}", back.code);
        assert!(back.text.unwrap().contains(NEEDLE));
    }
}

// ---------------------------------------------------------------------------
// 13. Out-of-scope labels are normal filtering; missing labels are an anomaly
// ---------------------------------------------------------------------------

#[test]
fn out_of_scope_records_are_filtered_and_missing_labels_are_an_anomaly() {
    let env = support::TestEnv::new();
    let fixture = fixture();
    let default_file = write_jsonl(&fixture, "rollout-default.jsonl", &codex_lines(&[]));
    capture(&fixture, &default_file, "default");
    let work_file = write_jsonl(
        &fixture,
        "rollout-work.jsonl",
        &[support::codex_user("workneedle only")],
    );
    capture(&fixture, &work_file, "work");
    let config = v2_config(
        &env,
        "archive-mixed-labels.json",
        vec![archive_source(&fixture)],
        vec!["ar"],
        "default",
    );

    // A match in the readable domain: healthy hit, the out-of-scope record is
    // counted but does not degrade the source.
    let response = search(&config, json!({"query": NEEDLE}));
    assert_eq!(response.exit_code, 0);
    assert_eq!(response.results.len(), 1);
    assert_eq!(response.source_status[0].status, status::OK);
    assert!(
        response.source_status[0].detail.as_ref().unwrap()["archive"]["unreachable"]
            .as_u64()
            .unwrap()
            >= 1
    );

    // A query that only the out-of-scope record would answer stays empty and is
    // not reported as a source failure (domain scoping is normal), but the
    // rejected record count remains visible.
    let scoped = search(&config, json!({"query": "workneedle"}));
    assert_eq!(scoped.exit_code, 0);
    assert!(scoped.results.is_empty());
    assert_eq!(scoped.source_status[0].status, status::EMPTY);
    assert!(
        scoped.source_status[0].detail.as_ref().unwrap()["archive"]["unreachable"]
            .as_u64()
            .unwrap()
            >= 1
    );

    // A record without any access label is a missing permission mapping: the
    // source degrades instead of pretending a clean empty result.
    let bytes = format!("{{\"text\":\"{NEEDLE} unlabeled\"}}").into_bytes();
    let (object_sha, _, _) = fixture.archive.put_object(&bytes).unwrap();
    fixture
        .archive
        .commit_manifest(&manifest_with_access("cap-unlabeled", &bytes, &object_sha, None))
        .unwrap();
    let anomaly = search(&config, json!({"query": NEEDLE}));
    assert_eq!(anomaly.exit_code, 10, "hit + partial record loss");
    assert_eq!(anomaly.source_status[0].status, status::PARTIAL);
    assert_eq!(anomaly.source_status[0].code, Some(codes::RECORD_SKIPPED));
    assert_eq!(
        anomaly.source_status[0].detail.as_ref().unwrap()["archive"]["labels_missing"],
        json!(1)
    );
}

// ---------------------------------------------------------------------------
// 14. An archive read never writes and never captures
// ---------------------------------------------------------------------------

#[test]
fn archive_search_and_read_are_read_only() {
    let env = support::TestEnv::new();
    let fixture = fixture();
    let path = write_jsonl(&fixture, "rollout-ro.jsonl", &codex_lines(&[]));
    capture(&fixture, &path, "default");
    let config = v2_config(
        &env,
        "archive-ro.json",
        vec![archive_source(&fixture)],
        vec!["ar"],
        "default",
    );

    let before = support::tree_state(&fixture.root);
    let response = search(&config, json!({"query": NEEDLE}));
    assert_eq!(response.results.len(), 1);
    let back = read(&config, json!({"ref": response.results[0].reference}));
    assert!(back.ok);
    let after = support::tree_state(&fixture.root);
    assert_eq!(before, after, "recall must not write to the archive root");
    assert!(
        !fixture.root.join("locks").join("capture.lock").exists(),
        "a read path never takes the capture lock"
    );
}

// ---------------------------------------------------------------------------
// 11. Onboarding generates and preserves a v2 archive source
// ---------------------------------------------------------------------------

#[test]
fn onboarding_writes_and_preserves_an_archive_source() {
    let env = support::TestEnv::new();
    let user_root = env.path().join("home-archive");
    std::fs::create_dir_all(user_root.join(".agent-memory").join("bin")).unwrap();
    std::fs::write(
        user_root
            .join(".agent-memory")
            .join("bin")
            .join("agent-memory.exe"),
        b"stub",
    )
    .unwrap();
    let vault = user_root.join("vault");
    std::fs::create_dir_all(vault.join(".meta").join("scripts")).unwrap();
    std::fs::copy(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/recall/kb_stub/ask.py"),
        vault.join(".meta").join("scripts").join("ask.py"),
    )
    .unwrap();
    std::fs::create_dir_all(vault.join("notes")).unwrap();
    std::fs::write(vault.join("notes").join("a.md"), "stub chunk").unwrap();
    let archive_root = user_root.join("archive-root");
    std::fs::create_dir_all(&archive_root).unwrap();
    let archive = Archive::init(&archive_root).unwrap();
    let source_file = user_root.join("rollout-onboard.jsonl");
    let mut text = String::new();
    for line in codex_lines(&[]) {
        text.push_str(&line);
        text.push('\n');
    }
    std::fs::write(&source_file, text).unwrap();
    capture_source(&archive, &source_file, "codex", "default", None).unwrap();

    let script = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("assets")
        .join("scripts")
        .join("recall_onboarding.py");
    let run = |extra: &[&str]| {
        let mut command = std::process::Command::new(python_exe());
        command
            .env("PYTHONUTF8", "1")
            .env("PYTHONIOENCODING", "utf-8")
            .arg(&script)
            .arg("install")
            .arg("--user-root")
            .arg(&user_root)
            .arg("--vault")
            .arg(&vault)
            .arg("--python-exe")
            .arg(python_exe())
            .arg("--skip-opencode")
            .arg("--skip-codex");
        for arg in extra {
            command.arg(arg);
        }
        command.output().expect("run onboarding")
    };

    let output = run(&["--archive-root", &archive_root.to_string_lossy()]);
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let recall_path = user_root.join(".agent-memory").join("recall.json");
    let config = load_config(&recall_path).unwrap();
    let archive = config
        .sources
        .iter()
        .find(|source| source.kind == agent_memory::recall::SourceKind::Archive)
        .expect("archive source generated");
    assert_eq!(
        archive.root.replace('\\', "/"),
        archive_root.to_string_lossy().replace('\\', "/")
    );
    assert!(config.source_order.iter().any(|id| id == &archive.source_id));

    // Re-running without the flag keeps the existing archive source untouched.
    let output = run(&[]);
    assert!(output.status.success());
    let config = load_config(&recall_path).unwrap();
    assert_eq!(
        config
            .sources
            .iter()
            .filter(|source| source.kind == agent_memory::recall::SourceKind::Archive)
            .count(),
        1
    );
    assert_eq!(
        config.source("archive").map(|source| source.root.clone()),
        Some(
            archive_root
                .to_string_lossy()
                .replace('\\', "/")
        )
    );

    // Synthetic smoke: the generated v2 configuration loads and the Archive
    // source answers a search with a readable hit.
    let response = {
        let request = SearchRequest::parse(&json!({
            "query": NEEDLE, "sources": ["archive"]
        }))
        .expect("search request");
        agent_memory::recall::memory_search(&config, &request)
    };
    assert_eq!(response.exit_code, 0, "{}", serde_json::to_string(&response).unwrap());
    assert_eq!(response.results.len(), 1);
    assert_eq!(response.results[0].source_kind, "archive");
    let back = read(&recall_path, json!({"ref": response.results[0].reference}));
    assert!(back.ok, "{:?}", back.code);
}

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
