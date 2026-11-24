//! P3 acceptance: Archive embedding hybrid recall.
//!
//! Every case is synthetic: temporary roots, synthetic Codex JSONL captures and
//! the deterministic `fake` embedding backend (no model, no network, no
//! subprocess). No real session, knowledge base or production Archive is read.

mod support;

use agent_memory::archive::{hex_sha256, Archive, Manifest};
use agent_memory::capture::capture_source;
use agent_memory::recall::config::load_config;
use agent_memory::recall::contract::{codes, status};
use agent_memory::recall::read::{memory_read, ReadRequest, ReadResponse};
use agent_memory::recall::search::{memory_search, SearchRequest, SearchResponse};
use agent_memory::recall::semantic::embed::EmbeddingPlan;
use agent_memory::recall::semantic::index::quantized_row_bytes;
use agent_memory::recall::semantic::model::{
    EmbeddingBackend, EmbeddingModelSpec, Normalization,
};
use agent_memory::replay::{replay, replay_with_plan, ReplayIdentity};
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

fn fake_spec(model_id: &str, dimension: u64) -> EmbeddingModelSpec {
    EmbeddingModelSpec {
        model_id: model_id.to_string(),
        dimension,
        normalization: Normalization::L2,
        backend: EmbeddingBackend::Fake,
        model_path: None,
        endpoint: None,
    }
}

fn fake_plan(model_id: &str, dimension: u64) -> EmbeddingPlan {
    EmbeddingPlan::for_spec(fake_spec(model_id, dimension))
}

fn v2_config(
    env: &support::TestEnv,
    name: &str,
    sources: Vec<Value>,
    order: Vec<&str>,
    domain: &str,
    embedding: Option<Value>,
) -> PathBuf {
    let mut interpreter = json!({"kb": {"command": "C:/python/python.exe"}});
    if let Some(embedding) = embedding {
        interpreter["embedding"] = embedding;
    }
    let value = json!({
        "version": 2,
        "sources": sources,
        "source_order": order,
        "interpreter": interpreter,
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

fn archive_source(fixture: &Fixture) -> Value {
    json!({
        "source_id": "ar", "kind": "archive",
        "root": fixture.root.to_string_lossy(), "enabled": true
    })
}

fn embedding_value(spec: &EmbeddingModelSpec) -> Value {
    let mut value = json!({
        "model_id": spec.model_id,
        "dimension": spec.dimension,
        "normalization": spec.normalization.as_str(),
        "backend": spec.backend.as_str(),
    });
    if let Some(path) = &spec.model_path {
        value["model_path"] = json!(path);
    }
    value
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

fn semantic_detail(response: &SearchResponse, source_id: &str) -> Value {
    let entry = response
        .source_status
        .iter()
        .find(|entry| entry.source_id == source_id)
        .expect("source status");
    entry
        .detail
        .as_ref()
        .and_then(|detail| detail.get("semantic"))
        .cloned()
        .expect("semantic detail")
}

// ---------------------------------------------------------------------------
// 1. Vectors live in the derived generation with identity and access
// ---------------------------------------------------------------------------

#[test]
fn vectors_are_written_into_the_derived_generation_with_identity_and_access() {
    let fixture = fixture();
    let lines = vec![
        support::codex_meta("s-p3"),
        support::codex_user(NEEDLE),
        support::codex_assistant("缓存失效策略与预热方案"),
        support::codex_function_call("c1", "shell", "{\"command\":\"cargo test\"}"),
        support::codex_function_output("c1", json!("12 passed")),
    ];
    let path = write_jsonl(&fixture, "rollout-p3.jsonl", &lines);
    capture(&fixture, &path, "default");

    let identity = ReplayIdentity::current();
    let spec = fake_spec("fake-hash-v1", 32);
    let plan = fake_plan("fake-hash-v1", 32);
    let report = replay_with_plan(&fixture.archive, &identity, "default", &plan).unwrap();
    assert!(report.ok, "{report:?}");
    assert_eq!(report.semantic.as_ref().unwrap().status, "available");
    let entry = report.retrieval.clone().expect("retrieval index");
    let vectors = entry.vectors.clone().expect("vector artifact");
    assert_eq!(vectors.quantization, "int4");
    assert_eq!(vectors.dimension, 32);
    assert!(vectors.count >= 3, "{vectors:?}");

    let index_path = fixture
        .root
        .join(entry.path.replace('/', std::path::MAIN_SEPARATOR_STR));
    let document: Value = serde_json::from_slice(&std::fs::read(&index_path).unwrap()).unwrap();
    assert_eq!(
        document["generator"]["generationKey"],
        json!("embed-v1;model=fake-hash-v1;dim=32;norm=l2;backend=fake")
    );
    assert_eq!(document["generator"]["backend"], json!("fake"));
    assert_eq!(document["generator"]["quantization"], json!("int4"));
    assert_eq!(document["access"], json!(["default"]));
    assert_eq!(document["vectors"]["path"], json!(vectors.path));
    assert_eq!(
        document["thresholdsVersion"],
        json!(agent_memory::recall::semantic::ARCHIVE_RELEVANCE_VERSION)
    );
    assert_eq!(
        document["scoreFusion"],
        json!("semantic_cosine_lexical_bonus")
    );
    assert_eq!(document["lexicalBonus"], json!(0.0));
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
    let units = agent_memory::recall::semantic::index::decode_unit_table(&units_bytes).unwrap();
    assert_eq!(units.len() as u64, vectors.count);
    assert!(units.iter().all(|unit| unit.access == vec!["default".to_string()]));
    assert!(units.iter().all(|unit| {
        unit.record_sha256.len() == 64
            && unit.record_sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
    }));
    assert!(units.iter().any(|unit| unit.unit_type == "message:user"));
    assert!(units.iter().any(|unit| unit.unit_type == "tool_call"));
    assert!(units.iter().any(|unit| unit.unit_type == "tool_result"));

    let vector_path = fixture
        .root
        .join(vectors.path.replace('/', std::path::MAIN_SEPARATOR_STR));
    let bytes = std::fs::read(&vector_path).unwrap();
    assert_eq!(bytes.len() as u64, vectors.bytes);
    assert_eq!(hex_sha256(&bytes), vectors.sha256);
    assert_eq!(
        bytes.len() as u64,
        vectors.count * quantized_row_bytes(spec.dimension as usize) as u64
    );

    // The configured generation is usable: a lexical query is fused and reads
    // back through the original record reference.
    let env = support::TestEnv::new();
    let config = v2_config(
        &env,
        "unused.json",
        vec![archive_source(&fixture)],
        vec!["ar"],
        "default",
        Some(embedding_value(&spec)),
    );
    let response = search(&config, json!({"query": NEEDLE}));
    assert!(!response.results.is_empty(), "{}", serde_json::to_string(&response).unwrap());
    assert_eq!(response.results[0].title, "codex rollout-p3.jsonl line 2");
    assert_eq!(response.results[0].rank_basis, "semantic_cosine_lexical_bonus");
    assert_eq!(response.results[0].score_kind, "semantic_plus_lexical_bonus");
    let detail = semantic_detail(&response, "ar");
    assert_eq!(detail["status"], json!("available"));
    assert_eq!(detail["fusion"], json!("semantic_cosine_lexical_bonus"));
    assert_eq!(detail["scoreFusion"], json!("semantic_cosine_lexical_bonus"));
    assert_eq!(detail["lexicalBonus"], json!(0.0));
    assert_eq!(detail["generation"], json!(1));
    let back = read(&config, json!({"ref": response.results[0].reference}));
    assert!(back.ok, "{:?}", back.code);
    assert!(back.text.unwrap().contains(NEEDLE));
}

// ---------------------------------------------------------------------------
// 2. Switching the model allocates a new generation instead of mixing vectors
// ---------------------------------------------------------------------------

#[test]
fn changing_the_model_allocates_a_new_generation_and_keeps_the_old_one() {
    let fixture = fixture();
    let path = write_jsonl(&fixture, "rollout-model.jsonl", &[
        support::codex_meta("s-model"),
        support::codex_user(NEEDLE),
    ]);
    capture(&fixture, &path, "default");
    let identity = ReplayIdentity::current();

    let first = replay_with_plan(
        &fixture.archive,
        &identity,
        "default",
        &fake_plan("fake-hash-v1", 32),
    )
    .unwrap();
    assert_eq!(first.generation, 1);
    let first_entry = first.retrieval.clone().unwrap();
    let first_index = first_entry.path.clone();
    let first_vectors = first_entry.vectors.clone().unwrap().path;
    let first_index_bytes =
        std::fs::read(fixture.root.join(first_index.replace('/', std::path::MAIN_SEPARATOR_STR)))
            .unwrap();
    let first_vector_bytes = std::fs::read(
        fixture
            .root
            .join(first_vectors.replace('/', std::path::MAIN_SEPARATOR_STR)),
    )
    .unwrap();

    let second = replay_with_plan(
        &fixture.archive,
        &identity,
        "default",
        &fake_plan("fake-hash-v2", 48),
    )
    .unwrap();
    assert_eq!(second.generation, 2, "a new model gets a new generation");
    let second_entry = second.retrieval.clone().unwrap();
    assert!(second_entry.path.starts_with("derived/generations/g2/retrieval/"));
    assert_eq!(
        second_entry.generation_key,
        "embed-v1;model=fake-hash-v2;dim=48;norm=l2;backend=fake"
    );
    // The first generation is untouched, not overwritten: both vector files exist
    // with their own dimension.
    assert_eq!(
        first_index_bytes,
        std::fs::read(fixture.root.join(first_index.replace('/', std::path::MAIN_SEPARATOR_STR)))
            .unwrap()
    );
    assert_eq!(
        first_vector_bytes.len(),
        first_entry.vectors.as_ref().unwrap().count as usize * quantized_row_bytes(32)
    );
    let second_vectors = second_entry.vectors.as_ref().unwrap();
    assert_eq!(
        std::fs::read(
            fixture
                .root
                .join(second_vectors.path.replace('/', std::path::MAIN_SEPARATOR_STR))
        )
        .unwrap()
        .len() as u64,
        second_vectors.count * quantized_row_bytes(48) as u64
    );

    // A query configured with model A uses generation 1 even though generation 2
    // is newer; a query configured with model B uses generation 2.
    let env = support::TestEnv::new();
    let config_a = v2_config(
        &env,
        "model-a.json",
        vec![archive_source(&fixture)],
        vec!["ar"],
        "default",
        Some(embedding_value(&fake_spec("fake-hash-v1", 32))),
    );
    let response_a = search(&config_a, json!({"query": NEEDLE}));
    assert_eq!(semantic_detail(&response_a, "ar")["generation"], json!(1));
    let config_b = v2_config(
        &env,
        "model-b.json",
        vec![archive_source(&fixture)],
        vec!["ar"],
        "default",
        Some(embedding_value(&fake_spec("fake-hash-v2", 48))),
    );
    let response_b = search(&config_b, json!({"query": NEEDLE}));
    assert_eq!(semantic_detail(&response_b, "ar")["generation"], json!(2));
}

// ---------------------------------------------------------------------------
// 3. Quantization change forces a new generation (int8 legacy is not mixed)
// ---------------------------------------------------------------------------

#[test]
fn quantization_change_allocates_a_new_generation_instead_of_reusing_int8() {
    let fixture = fixture();
    let path = write_jsonl(&fixture, "rollout-quant.jsonl", &[
        support::codex_meta("s-quant"),
        support::codex_user(NEEDLE),
    ]);
    capture(&fixture, &path, "default");
    let identity = ReplayIdentity::current();
    let plan = fake_plan("fake-hash-v1", 32);

    let first = replay_with_plan(&fixture.archive, &identity, "default", &plan).unwrap();
    assert_eq!(first.generation, 1);
    assert_eq!(
        first.retrieval.as_ref().unwrap().vectors.as_ref().unwrap().quantization,
        "int4"
    );

    // Simulate a generation written by the int8 build: same parser identity and
    // generation key, older vector encoding in the generation metadata.
    let state_path = fixture.root.join("derived").join("state.json");
    let mut state: Value = serde_json::from_slice(&std::fs::read(&state_path).unwrap()).unwrap();
    state["generations"][0]["retrieval"]["vectors"]["quantization"] = json!("int8");
    std::fs::write(&state_path, serde_json::to_vec(&state).unwrap()).unwrap();

    let second = replay_with_plan(&fixture.archive, &identity, "default", &plan).unwrap();
    assert_eq!(
        second.generation, 2,
        "a quantization change must allocate a full new generation"
    );
    assert_eq!(
        second.retrieval.as_ref().unwrap().vectors.as_ref().unwrap().quantization,
        "int4"
    );

    let env = support::TestEnv::new();
    let config = v2_config(
        &env,
        "quant.json",
        vec![archive_source(&fixture)],
        vec!["ar"],
        "default",
        Some(embedding_value(&fake_spec("fake-hash-v1", 32))),
    );
    let response = search(&config, json!({"query": NEEDLE}));
    assert_eq!(semantic_detail(&response, "ar")["generation"], json!(2));
}

// ---------------------------------------------------------------------------
// 4. An int8 generation is reported as a generation mismatch, never scored
// ---------------------------------------------------------------------------

#[test]
fn int8_generation_metadata_is_reported_as_a_generation_mismatch() {
    let fixture = fixture();
    let path = write_jsonl(&fixture, "rollout-int8.jsonl", &[
        support::codex_meta("s-int8"),
        support::codex_user(NEEDLE),
    ]);
    capture(&fixture, &path, "default");
    let identity = ReplayIdentity::current();
    let spec = fake_spec("fake-hash-v1", 32);
    let first = replay_with_plan(&fixture.archive, &identity, "default", &fake_plan("fake-hash-v1", 32))
        .unwrap();
    let entry = first.retrieval.clone().unwrap();

    // Rewrite the generation document as the previous int8 build would have
    // written it: same generation key and dimension, old encoding metadata.
    let index_path = fixture
        .root
        .join(entry.path.replace('/', std::path::MAIN_SEPARATOR_STR));
    let mut document: Value = serde_json::from_slice(&std::fs::read(&index_path).unwrap()).unwrap();
    document["generator"]["quantization"] = json!("int8");
    document["vectors"]["quantization"] = json!("int8");
    std::fs::write(&index_path, serde_json::to_vec(&document).unwrap()).unwrap();

    let env = support::TestEnv::new();
    let config = v2_config(
        &env,
        "int8.json",
        vec![archive_source(&fixture)],
        vec!["ar"],
        "default",
        Some(embedding_value(&spec)),
    );
    let response = search(&config, json!({"query": NEEDLE}));
    let detail = semantic_detail(&response, "ar");
    assert_eq!(detail["status"], json!("degraded"));
    assert_eq!(detail["reason"], json!("semantic_generation_mismatch"));
    // The lexical generation is shared with the int8 index, so search still
    // answers instead of failing closed.
    assert!(!response.results.is_empty());
}

// ---------------------------------------------------------------------------
// 5. Deleting the derived tree rebuilds identical synthetic vectors
// ---------------------------------------------------------------------------

#[test]
fn deleting_the_derived_tree_rebuilds_identical_vectors() {
    let fixture = fixture();
    let path = write_jsonl(&fixture, "rollout-rebuild.jsonl", &[
        support::codex_meta("s-rebuild"),
        support::codex_user("缓存失效策略"),
        support::codex_assistant("部署流水线并行执行"),
    ]);
    capture(&fixture, &path, "default");
    let identity = ReplayIdentity::current();
    let plan = fake_plan("fake-hash-v1", 32);

    let first = replay_with_plan(&fixture.archive, &identity, "default", &plan).unwrap();
    let entry = first.retrieval.clone().unwrap();
    let index_rel = entry.path.replace('/', std::path::MAIN_SEPARATOR_STR);
    let vector_rel = entry
        .vectors
        .as_ref()
        .unwrap()
        .path
        .replace('/', std::path::MAIN_SEPARATOR_STR);
    let units_rel = entry
        .units_table
        .as_ref()
        .unwrap()
        .path
        .replace('/', std::path::MAIN_SEPARATOR_STR);
    let first_index = std::fs::read(fixture.root.join(&index_rel)).unwrap();
    let first_vectors = std::fs::read(fixture.root.join(&vector_rel)).unwrap();
    let first_units = std::fs::read(fixture.root.join(&units_rel)).unwrap();

    std::fs::remove_dir_all(fixture.root.join("derived")).unwrap();
    let second = replay_with_plan(&fixture.archive, &identity, "default", &plan).unwrap();
    let rebuilt = second.retrieval.clone().unwrap();
    assert_eq!(rebuilt.path, entry.path);
    assert_eq!(
        first_index,
        std::fs::read(fixture.root.join(&index_rel)).unwrap(),
        "index bytes must be reproducible"
    );
    assert_eq!(
        first_vectors,
        std::fs::read(fixture.root.join(&vector_rel)).unwrap(),
        "fake vectors must be byte-identical after a rebuild"
    );
    assert_eq!(
        first_units,
        std::fs::read(fixture.root.join(&units_rel)).unwrap(),
        "binary unit table must be byte-identical after a rebuild"
    );
}

// ---------------------------------------------------------------------------
// 4. Permission filtering happens before vector candidate generation
// ---------------------------------------------------------------------------

#[test]
fn denied_units_never_reach_the_vector_candidate_list() {
    let fixture = fixture();
    let target = "甲乙丙丁";
    let lines = vec![
        support::codex_meta("s-perm"),
        support::codex_user(target),
        support::codex_assistant("unrelated body zzz"),
    ];
    let path = write_jsonl(&fixture, "rollout-perm.jsonl", &lines);
    capture(&fixture, &path, "default");
    let identity = ReplayIdentity::current();
    replay_with_plan(
        &fixture.archive,
        &identity,
        "default",
        &fake_plan("fake-hash-v1", 32),
    )
    .unwrap();

    let env = support::TestEnv::new();
    let config = v2_config(
        &env,
        "perm.json",
        vec![archive_source(&fixture)],
        vec!["ar"],
        "default",
        Some(embedding_value(&fake_spec("fake-hash-v1", 32))),
    );

    // The query shares every character of the target unit but no CJK bigram, so
    // only the vector path can find it.
    let baseline = search(&config, json!({"query": "甲丙乙丁"}));
    assert!(!baseline.results.is_empty(), "{}", serde_json::to_string(&baseline).unwrap());
    assert_eq!(baseline.results[0].title, "codex rollout-perm.jsonl line 2");
    let hit_id = baseline.results[0].hit_id.clone();
    assert!(
        semantic_detail(&baseline, "ar")["candidates"]
            .as_u64()
            .unwrap()
            >= 1
    );
    let back = read(&config, json!({"ref": baseline.results[0].reference}));
    assert!(back.ok);
    assert!(back.text.unwrap().contains(target));

    // Move only that record's access label to another domain. The object keeps
    // its readable label, so the record-level predicate is what has to stop it.
    let manifests_dir = fixture.root.join("archive").join("manifests");
    let mut rewritten = 0usize;
    for entry in std::fs::read_dir(&manifests_dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let mut manifest: Manifest = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        for record in &mut manifest.records {
            let (_, object) = support::read_object_file(&fixture.root, &record.object);
            let slice = &object[record.offset as usize..(record.offset + record.len) as usize];
            if std::str::from_utf8(slice).unwrap().contains(target) {
                record.access = vec!["work".to_string()];
                rewritten += 1;
            }
        }
        std::fs::write(&path, serde_json::to_vec(&manifest).unwrap()).unwrap();
    }
    assert_eq!(rewritten, 1, "exactly the target record becomes work-only");

    let denied = search(&config, json!({"query": "甲丙乙丁"}));
    assert!(
        denied.results.is_empty(),
        "denied unit must not surface, even as the nearest vector: {}",
        serde_json::to_string(&denied).unwrap()
    );
    assert!(
        !denied
            .results
            .iter()
            .any(|result| result.hit_id == hit_id),
        "the denied hit id must never appear"
    );
    let detail = semantic_detail(&denied, "ar");
    assert_eq!(detail["status"], json!("available"));
    let archive_detail = denied.source_status[0]
        .detail
        .as_ref()
        .unwrap()["archive"]
        .clone();
    // A non-default label under an implicit default is "unreachable" (P1
    // semantics); an explicit default request would call it "denied". Either way
    // the unit was rejected before candidate generation.
    assert_eq!(
        archive_detail["denied"].as_u64().unwrap() + archive_detail["unreachable"].as_u64().unwrap(),
        1
    );
}

// ---------------------------------------------------------------------------
// 5. A missing model degrades explicitly and lexical results do not move
// ---------------------------------------------------------------------------

#[test]
fn missing_model_degrades_and_lexical_results_match_the_lexical_baseline() {
    let fixture = fixture();
    let path = write_jsonl(&fixture, "rollout-degrade.jsonl", &[
        support::codex_meta("s-degrade"),
        support::codex_user(NEEDLE),
        support::codex_assistant(&format!("{NEEDLE} answer")),
    ]);
    capture(&fixture, &path, "default");
    let identity = ReplayIdentity::current();

    // The derived layer only has the lexical generation (the environment has no
    // model): search must keep the P1 lexical results and report the degradation.
    replay(&fixture.archive, &identity, "default").unwrap();
    let env = support::TestEnv::new();
    let lexical = v2_config(
        &env,
        "lexical.json",
        vec![archive_source(&fixture)],
        vec!["ar"],
        "default",
        None,
    );
    let mut unavailable = fake_spec("bge-small-zh-v1.5", 512);
    unavailable.backend = EmbeddingBackend::Python;
    unavailable.model_path = None;
    let degraded_config = v2_config(
        &env,
        "degraded.json",
        vec![archive_source(&fixture)],
        vec!["ar"],
        "default",
        Some(embedding_value(&unavailable)),
    );

    let baseline = search(&lexical, json!({"query": NEEDLE}));
    let degraded = search(&degraded_config, json!({"query": NEEDLE}));
    assert_eq!(degraded.exit_code, 0);
    let baseline_ids: Vec<&str> = baseline.results.iter().map(|hit| hit.hit_id.as_str()).collect();
    let degraded_ids: Vec<&str> = degraded.results.iter().map(|hit| hit.hit_id.as_str()).collect();
    assert_eq!(baseline_ids, degraded_ids, "lexical hits must not move");
    assert_eq!(degraded.results[0].rank_basis, "lexical_window");
    let detail = semantic_detail(&degraded, "ar");
    assert_eq!(detail["status"], json!("degraded"));
    assert!(
        detail["reason"].as_str().is_some(),
        "a degraded semantic path must carry a reason: {detail:?}"
    );
    assert_eq!(detail["fusion"], json!("none"));

    // Replay with an unavailable model fails as `Embedding` and leaves the
    // archive usable; a lexical replay heals the interrupted generation marker.
    let mut plan = EmbeddingPlan::for_spec(unavailable.clone());
    plan.interpreter = "C:/definitely-not-a-python/python.exe".to_string();
    plan.script = Some(fixture.temp.path().join("missing-script.py"));
    let error = replay_with_plan(&fixture.archive, &identity, "default", &plan).unwrap_err();
    assert!(
        matches!(error, agent_memory::archive::ArchiveError::Embedding(_)),
        "{error}"
    );
    let healed = replay(&fixture.archive, &identity, "default").unwrap();
    assert!(healed.ok);
    let after = search(&degraded_config, json!({"query": NEEDLE}));
    let after_ids: Vec<&str> = after.results.iter().map(|hit| hit.hit_id.as_str()).collect();
    assert_eq!(baseline_ids, after_ids);
}

// ---------------------------------------------------------------------------
// 6. A semantic-only hit is fused, ranked and read back through its record
// ---------------------------------------------------------------------------

#[test]
fn semantic_hits_fuse_with_lexical_ranks_and_read_back() {
    let fixture = fixture();
    let path = write_jsonl(&fixture, "rollout-fuse.jsonl", &[
        support::codex_meta("s-fuse"),
        support::codex_user("甲乙丙丁"),
        support::codex_assistant("缓存失效策略"),
    ]);
    capture(&fixture, &path, "default");
    replay_with_plan(
        &fixture.archive,
        &ReplayIdentity::current(),
        "default",
        &fake_plan("fake-hash-v1", 32),
    )
    .unwrap();
    let env = support::TestEnv::new();
    let config = v2_config(
        &env,
        "fuse.json",
        vec![archive_source(&fixture)],
        vec!["ar"],
        "default",
        Some(embedding_value(&fake_spec("fake-hash-v1", 32))),
    );

    // Lexical and semantic both see this record: the fused score is the sum of
    // two reciprocal ranks, and the hit reads back as the original record.
    let fused = search(&config, json!({"query": "缓存失效策略"}));
    assert!(!fused.results.is_empty());
    assert_eq!(fused.results[0].title, "codex rollout-fuse.jsonl line 3");
    // Round-4 fusion: the exact match scores its semantic cosine plus the
    // bounded lexical bonus (0 in the calibrated configuration), plus the tiny
    // turn tie-break.
    assert!(
        fused.results[0].score.unwrap() > 0.9,
        "{:?}",
        fused.results[0].score
    );
    let back = read(&config, json!({"ref": fused.results[0].reference}));
    assert!(back.ok);
    assert!(back.text.unwrap().contains("缓存失效策略"));

    // Only the vector path finds this one (shared characters, no shared bigram).
    let semantic_only = search(&config, json!({"query": "甲丙乙丁"}));
    assert!(!semantic_only.results.is_empty());
    assert_eq!(semantic_only.results[0].title, "codex rollout-fuse.jsonl line 2");
    assert_eq!(semantic_only.results[0].rank_basis, "semantic_cosine_lexical_bonus");
    assert!(
        semantic_only.results[0].score.unwrap() > 0.0,
        "{:?}",
        semantic_only.results[0].score
    );
    let back = read(&config, json!({"ref": semantic_only.results[0].reference}));
    assert!(back.ok);
    assert!(back.text.unwrap().contains("甲乙丙丁"));
}

// ---------------------------------------------------------------------------
// 7. N4: the index identity and access labels gate mapping and vectors
// ---------------------------------------------------------------------------

#[test]
fn session_mapping_and_vectors_require_index_identity_and_access() {
    let fixture = fixture();
    let path = write_jsonl(&fixture, "rollout-n4.jsonl", &[
        support::codex_meta("s-n4"),
        support::codex_user(NEEDLE),
    ]);
    capture(&fixture, &path, "default");
    let report = replay_with_plan(
        &fixture.archive,
        &ReplayIdentity::current(),
        "default",
        &fake_plan("fake-hash-v1", 32),
    )
    .unwrap();
    let index_path = fixture
        .root
        .join(report.retrieval.unwrap().path.replace('/', std::path::MAIN_SEPARATOR_STR));
    let original: Value = serde_json::from_slice(&std::fs::read(&index_path).unwrap()).unwrap();
    let env = support::TestEnv::new();
    let config = v2_config(
        &env,
        "n4.json",
        vec![archive_source(&fixture)],
        vec!["ar"],
        "default",
        Some(embedding_value(&fake_spec("fake-hash-v1", 32))),
    );

    // Access mismatch: the mapping and the vectors are both refused, with an
    // explicit status; the record-level predicate is not loosened to compensate.
    let mut access_changed = original.clone();
    access_changed["access"] = json!(["work"]);
    std::fs::write(&index_path, serde_json::to_vec(&access_changed).unwrap()).unwrap();
    let denied = search(&config, json!({"query": NEEDLE, "session_id": "s-n4"}));
    assert!(denied.results.is_empty());
    let status_entry = &denied.source_status[0];
    assert_eq!(
        status_entry.detail.as_ref().unwrap()["archive"]["session_mapping"],
        json!("access_mismatch")
    );
    let detail = semantic_detail(&denied, "ar");
    assert_eq!(detail["status"], json!("degraded"));
    assert_eq!(detail["reason"], json!("semantic_access_restricted"));

    // Identity mismatch inside the index (same generation directory): session
    // mapping and vectors are refused even though the state identity matches.
    let mut identity_changed = original.clone();
    identity_changed["identity"]["parserVersion"] = json!("9.9.9");
    std::fs::write(&index_path, serde_json::to_vec(&identity_changed).unwrap()).unwrap();
    let stale = search(&config, json!({"query": NEEDLE}));
    // Lexical recall does not depend on the derived layer, so the text hit stays;
    // the mapping and the vectors must not be used.
    assert_eq!(stale.results.len(), 1);
    assert_eq!(stale.results[0].rank_basis, "lexical_window");
    let detail = semantic_detail(&stale, "ar");
    assert_eq!(detail["status"], json!("degraded"));
    assert_eq!(detail["reason"], json!("index_identity_mismatch"));

    // Restoring the index restores hybrid recall (proves the refusal was the
    // mismatch, not a permanently unusable source).
    std::fs::write(&index_path, serde_json::to_vec(&original).unwrap()).unwrap();
    let ok = search(&config, json!({"query": NEEDLE}));
    assert_eq!(ok.results.len(), 1, "{}", serde_json::to_string(&ok).unwrap());
    assert_eq!(ok.source_status[0].status, status::OK);
    assert_eq!(semantic_detail(&ok, "ar")["status"], json!("available"));
    assert_ne!(ok.source_status[0].code, Some(codes::DOMAIN_DENIED));
}

// ---------------------------------------------------------------------------
// 8. Relevance floor: unrelated topics and a single incidental bigram
// ---------------------------------------------------------------------------

#[test]
fn unrelated_queries_return_empty_and_one_incidental_bigram_is_not_a_hit() {
    let fixture = fixture();
    let path = write_jsonl(&fixture, "rollout-unrelated.jsonl", &[
        support::codex_meta("s-unrelated"),
        support::codex_user("怎么排查缓存命中率"),
        support::codex_assistant("先看缓存键的粒度和失效策略，再检查命中率。"),
    ]);
    capture(&fixture, &path, "default");
    replay_with_plan(
        &fixture.archive,
        &ReplayIdentity::current(),
        "default",
        &fake_plan("fake-hash-v1", 32),
    )
    .unwrap();

    let env = support::TestEnv::new();
    let lexical = v2_config(
        &env,
        "unrelated-lexical.json",
        vec![archive_source(&fixture)],
        vec!["ar"],
        "default",
        None,
    );
    let semantic = v2_config(
        &env,
        "unrelated-semantic.json",
        vec![archive_source(&fixture)],
        vec!["ar"],
        "default",
        Some(embedding_value(&fake_spec("fake-hash-v1", 32))),
    );

    // With the semantic path available, unrelated topics are gated out; with the
    // path unavailable (--mode none / degraded) lexical behavior is exactly P1,
    // including the incidental-bigram hit.
    for query in ["明天北京天气怎么样", "怎么申请年假", "打印机驱动安装失败"] {
        let response = search(&semantic, json!({"query": query}));
        assert!(
            response.results.is_empty(),
            "{query}: {}",
            serde_json::to_string(&response).unwrap()
        );
        assert_eq!(response.exit_code, 0, "{query}");
        assert_eq!(response.source_status[0].status, status::EMPTY);
        assert!(
            response.source_status[0]
                .detail
                .as_ref()
                .unwrap()["archive"]["gated_out"]
                .as_u64()
                .unwrap_or(0)
                >= 1,
            "{query}: gate should account for the P1 lexical hit"
        );
    }
    // P1 lexical behavior without the gate (degraded semantic path).
    let p1_hit = search(&lexical, json!({"query": "怎么申请年假"}));
    assert_eq!(p1_hit.results.len(), 1, "{}", serde_json::to_string(&p1_hit).unwrap());
    assert_eq!(p1_hit.exit_code, 0);
    assert_eq!(
        p1_hit.source_status[0]
            .detail
            .as_ref()
            .unwrap()["archive"]["thresholdsVersion"],
        json!(agent_memory::recall::semantic::ARCHIVE_RELEVANCE_VERSION)
    );

    // A genuinely relevant query still works in both modes.
    for config in [&lexical, &semantic] {
        let response = search(config, json!({"query": "缓存命中率"}));
        assert!(!response.results.is_empty(), "{}", serde_json::to_string(&response).unwrap());
        assert_eq!(response.exit_code, 0);
        assert!(response
            .results
            .iter()
            .all(|hit| hit.rank_basis == "semantic_cosine_lexical_bonus" || hit.rank_basis == "lexical_window"));
    }
    // The gate is applied before pagination and preserves the retained order.
    let all = search(&semantic, json!({"query": "缓存命中率", "limit": 10}));
    let page = search(&semantic, json!({"query": "缓存命中率", "limit": 2}));
    let all_ids: Vec<&str> = all.results.iter().take(page.results.len())
        .map(|hit| hit.hit_id.as_str()).collect();
    let page_ids: Vec<&str> = page.results.iter().map(|hit| hit.hit_id.as_str()).collect();
    assert_eq!(all_ids, page_ids);
}

// ---------------------------------------------------------------------------
// 9. A strong lexical hit is not displaced by a weak semantic neighbour
// ---------------------------------------------------------------------------

#[test]
fn semantic_target_stays_above_a_high_coverage_lexical_neighbour() {
    let fixture = fixture();
    let path = write_jsonl(&fixture, "rollout-strong.jsonl", &[
        support::codex_meta("s-strong"),
        support::codex_user("缓存失效策略"),
        support::codex_assistant("缓存失效策略：按记录键缓存并用版本号标记。"),
        support::codex_user("缓存失效"),
    ]);
    capture(&fixture, &path, "default");
    replay_with_plan(
        &fixture.archive,
        &ReplayIdentity::current(),
        "default",
        &fake_plan("fake-hash-v1", 32),
    )
    .unwrap();
    let env = support::TestEnv::new();
    let config = v2_config(
        &env,
        "strong.json",
        vec![archive_source(&fixture)],
        vec!["ar"],
        "default",
        Some(embedding_value(&fake_spec("fake-hash-v1", 32))),
    );

    let response = search(&config, json!({"query": "缓存失效策略"}));
    assert!(!response.results.is_empty());
    let top = &response.results[0];
    assert!(
        top.title.contains("line 2") || top.title.contains("line 3"),
        "strong lexical turn first, got {}",
        top.title
    );
    assert_eq!(top.rank_basis, "semantic_cosine_lexical_bonus");
    let back = read(&config, json!({"ref": top.reference}));
    assert!(back.ok);
}

// ---------------------------------------------------------------------------
// 10. Equal semantic similarity: higher lexical coverage wins the tie-break
// ---------------------------------------------------------------------------

#[test]
fn high_coverage_lexical_candidate_wins_a_semantic_tie() {
    let fixture = fixture();
    let lines = vec![
        json!({"type":"user","message":{"role":"user","content":"帮我看看缓存"}}).to_string(),
        json!({"type":"assistant","message":{"role":"assistant","content":[
            {"type":"text","text":"缓存失效策略"}]}}).to_string(),
        // Same text unit (as a tool result) plus one extra query term: the
        // semantic cosine is identical, coverage is higher.
        json!({"type":"assistant","message":{"role":"assistant","content":[
            {"type":"tool_result","content":"缓存失效策略"},
            {"type":"text","text":"版本号"}]}}).to_string(),
    ];
    let path = write_jsonl(&fixture, "rollout-tie.jsonl", &lines);
    let report = capture_source(&fixture.archive, &path, "claude", "default", None).unwrap();
    assert_eq!(report.status, "captured", "{report:?}");
    replay_with_plan(
        &fixture.archive,
        &ReplayIdentity::current(),
        "default",
        &fake_plan("fake-hash-v1", 32),
    )
    .unwrap();
    let env = support::TestEnv::new();
    let config = v2_config(
        &env,
        "tie.json",
        vec![archive_source(&fixture)],
        vec!["ar"],
        "default",
        Some(embedding_value(&fake_spec("fake-hash-v1", 32))),
    );

    let response = search(&config, json!({"query": "缓存失效策略 版本号"}));
    assert!(response.results.len() >= 2, "{}", serde_json::to_string(&response).unwrap());
    assert_eq!(response.results[0].title, "claude rollout-tie.jsonl line 3");
    assert_eq!(
        response.results[0].rank_basis,
        "semantic_cosine_lexical_bonus"
    );
    let back = read(&config, json!({"ref": response.results[0].reference}));
    assert!(back.ok);
    assert!(back.text.unwrap().contains("版本号"));
}

// ---------------------------------------------------------------------------
// 11. Turn aggregation lifts a multi-unit turn and returns a readable ref
// ---------------------------------------------------------------------------

#[test]
fn turn_aggregation_lifts_a_multi_unit_turn_and_returns_a_readable_ref() {
    let fixture = fixture();
    let path = write_jsonl(&fixture, "rollout-turn.jsonl", &[
        support::codex_meta("s-turn"),
        support::codex_user("needlealpha one"),
        support::codex_assistant("needlealpha two"),
        support::codex_user("needlealpha three"),
    ]);
    capture(&fixture, &path, "default");
    let report = replay_with_plan(
        &fixture.archive,
        &ReplayIdentity::current(),
        "default",
        &fake_plan("fake-hash-v1", 32),
    )
    .unwrap();
    let env = support::TestEnv::new();
    let config = v2_config(
        &env,
        "turn.json",
        vec![archive_source(&fixture)],
        vec!["ar"],
        "default",
        Some(embedding_value(&fake_spec("fake-hash-v1", 32))),
    );
    let response = search(&config, json!({"query": "needlealpha"}));
    assert!(response.results.len() >= 2, "{}", serde_json::to_string(&response).unwrap());
    // Turn 1 contains two matched records; its representative must come first and
    // its score carries the aggregation bonus.
    let top = &response.results[0];
    assert!(
        top.title.contains("line 2") || top.title.contains("line 3"),
        "multi-unit turn first, got {}",
        top.title
    );
    assert!(top.score.unwrap() > 0.5, "{:?}", top.score);
    let back = read(&config, json!({"ref": top.reference}));
    assert!(back.ok);
    assert!(back.text.unwrap().contains("needlealpha"));

    // The threshold version is part of the reported semantic state.
    let detail = semantic_detail(&response, "ar");
    assert_eq!(
        detail["thresholdsVersion"],
        json!(agent_memory::recall::semantic::ARCHIVE_RELEVANCE_VERSION)
    );
    assert!(detail["thresholds"]["semanticAcceptSimilarity"].as_f64().unwrap() > 0.6);
    assert!(report.retrieval.unwrap().thresholds_version.is_empty() == false);
}
