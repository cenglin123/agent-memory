//! P0 configuration-contract baseline: v1/v2 key sets, in-memory v1
//! normalization, domain precedence, Archive gating, and the onboarding →
//! v2 load → synthetic search chain.
//!
//! Every root is a temporary directory; no real session, vault or Archive is
//! read.

mod support;

use agent_memory::recall::config::RecallConfig;
use agent_memory::recall::contract::{codes, status};
use agent_memory::recall::domain::{classify_access, AccessDecision, DEFAULT_DOMAIN};
use agent_memory::recall::read::{memory_read, ReadRequest};
use agent_memory::recall::search::{memory_search, SearchRequest};
use agent_memory::recall::SourceKind;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

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

fn v1_config(root: &Path) -> Value {
    json!({
        "version": 1,
        "sources": [
            {"source_id": "kb", "kind": "kb", "root": root.to_string_lossy(), "enabled": true}
        ],
        "source_order": ["kb"],
        "interpreter": {"kb": {"command": python_exe()}}
    })
}

fn v2_config(root: &Path) -> Value {
    let mut value = v1_config(root);
    value["version"] = json!(2);
    value["domain"] = json!("default");
    value["telemetry"] = json!({"enabled": false});
    value
}

fn detail_path(error: &agent_memory::recall::contract::RecallError) -> String {
    error
        .detail
        .as_ref()
        .and_then(|detail| detail.get("path"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

fn detail_reason(error: &agent_memory::recall::contract::RecallError) -> String {
    error
        .detail
        .as_ref()
        .and_then(|detail| detail.get("reason"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

#[test]
fn v1_is_normalized_in_memory_and_never_rewritten() {
    let env = support::TestEnv::new();
    let root = env.root("kb-v1");
    let path = env.write_json("v1.json", &v1_config(&root));
    let before = std::fs::read(&path).unwrap();

    let config = agent_memory::recall::load_config(&path).expect("v1 load");
    assert_eq!(config.version, 1);
    assert_eq!(config.domain, DEFAULT_DOMAIN);
    assert!(config.telemetry.is_none());
    assert!(config.interpreter.embedding.is_none());
    assert!(!config.telemetry_enabled());
    assert_eq!(std::fs::read(&path).unwrap(), before, "v1 file is not rewritten");

    // v1 rejects every v2-only concept, including Archive.
    type Mutate = fn(&mut Value);
    let cases: [(Mutate, &str, &str); 4] = [
        (
            |value: &mut Value| value["sources"][0]["kind"] = json!("archive"),
            "sources[0].kind",
            "archive_requires_v2",
        ),
        (
            |value: &mut Value| value["domain"] = json!("work"),
            "domain",
            "unknown_key",
        ),
        (
            |value: &mut Value| value["telemetry"] = json!({"enabled": false}),
            "telemetry",
            "unknown_key",
        ),
        (
            |value: &mut Value| value["interpreter"]["embedding"] = json!({}),
            "interpreter.embedding",
            "unknown_key",
        ),
    ];
    for (mutate, expected_path, expected_reason) in cases {
        let mut value = v1_config(&root);
        mutate(&mut value);
        let error = RecallConfig::from_value(&value).expect_err(expected_path);
        assert_eq!(detail_path(&error), expected_path);
        assert_eq!(detail_reason(&error), expected_reason);
    }
}

#[test]
fn v2_requires_domain_and_freezes_the_top_level_key_set() {
    let env = support::TestEnv::new();
    let root = env.root("kb-v2");

    let config = RecallConfig::from_value(&v2_config(&root)).expect("v2");
    assert_eq!(config.version, 2);
    assert_eq!(config.domain, "default");
    assert!(!config.telemetry_enabled());

    let mut missing = v2_config(&root);
    missing.as_object_mut().unwrap().remove("domain");
    let error = RecallConfig::from_value(&missing).unwrap_err();
    assert_eq!(detail_path(&error), "domain");
    assert_eq!(detail_reason(&error), "missing_key");

    let mut unknown = v2_config(&root);
    unknown["extra"] = json!(1);
    let config = RecallConfig::from_value(&unknown).expect("unknown top-level key is ignored");
    assert_eq!(config.unsupported_keys, vec!["extra".to_string()]);

    // A spelling variant of a known key is still frozen out.
    let mut variant = v2_config(&root);
    variant["cache-root"] = json!("C:/cache");
    let error = RecallConfig::from_value(&variant).unwrap_err();
    assert_eq!(detail_path(&error), "cache-root");
    assert_eq!(detail_reason(&error), "unknown_key");

    for domain in ["", "Work", "-work", "work space", &"a".repeat(65)] {
        let mut value = v2_config(&root);
        value["domain"] = json!(domain);
        let error = RecallConfig::from_value(&value).expect_err(domain);
        assert_eq!(detail_path(&error), "domain");
    }

    let mut unsupported = v2_config(&root);
    unsupported["version"] = json!(3);
    let error = RecallConfig::from_value(&unsupported).unwrap_err();
    assert_eq!(detail_reason(&error), "unsupported_version");
}

#[test]
fn v2_telemetry_and_embedding_are_optional_but_validated() {
    let env = support::TestEnv::new();
    let root = env.root("kb-v2-optional");

    let mut value = v2_config(&root);
    value.as_object_mut().unwrap().remove("telemetry");
    let config = RecallConfig::from_value(&value).expect("telemetry is optional");
    assert!(config.telemetry.is_none());

    let mut enabled = v2_config(&root);
    enabled["telemetry"] = json!({"enabled": true});
    let error = RecallConfig::from_value(&enabled).unwrap_err();
    assert_eq!(detail_path(&error), "telemetry.path");
    assert_eq!(detail_reason(&error), "required_when_enabled");

    let mut bad_ttl = v2_config(&root);
    bad_ttl["telemetry"] = json!({
        "enabled": true,
        "path": "C:/state/retrieval_events.sqlite",
        "query_text_ttl_days": 91
    });
    let error = RecallConfig::from_value(&bad_ttl).unwrap_err();
    assert_eq!(detail_path(&error), "telemetry.query_text_ttl_days");

    let mut embedding = v2_config(&root);
    embedding["interpreter"]["embedding"] = json!({
        "model_id": "bge-small-zh-v1.5",
        "dimension": 512,
        "normalization": "l2",
        "backend": "python",
        "model_path": "C:/models/bge-small-zh-v1.5"
    });
    let config = RecallConfig::from_value(&embedding).expect("embedding spec");
    assert_eq!(
        config
            .interpreter
            .embedding
            .as_ref()
            .unwrap()
            .generation_key(),
        "embed-v1;model=bge-small-zh-v1.5;dim=512;norm=l2;backend=python"
    );
}

#[test]
fn domain_precedence_is_request_then_configuration_then_default() {
    let env = support::TestEnv::new();
    let root = env.root("kb-domain");

    let mut value = v2_config(&root);
    value["domain"] = json!("work");
    let config = RecallConfig::from_value(&value).expect("work domain");

    let request = SearchRequest::parse(&json!({"query": "needle"})).unwrap();
    assert_eq!(request.effective_domain(&config), "work");
    assert!(request.domain_is_explicit(&config));

    let request =
        SearchRequest::parse(&json!({"query": "needle", "domain": "private"})).unwrap();
    assert_eq!(request.effective_domain(&config), "private");

    let v1 = RecallConfig::from_value(&v1_config(&root)).unwrap();
    let request = SearchRequest::parse(&json!({"query": "needle"})).unwrap();
    assert_eq!(request.effective_domain(&v1), DEFAULT_DOMAIN);
    assert!(!request.domain_is_explicit(&v1));

    for invalid in [json!(""), json!("Work"), json!("a/b"), json!(7)] {
        let error = SearchRequest::parse(&json!({"query": "needle", "domain": invalid}))
            .expect_err("invalid domain");
        assert_eq!(detail_path(&error), "domain");
    }

    let read = ReadRequest::parse(&json!({
        "ref": {
            "source_id": "kb", "source_kind": "kb", "root_sha256": "a".repeat(64),
            "rel_path": "notes/a.md", "schema_gen": "md-v1", "locator": {},
            "fingerprint": {"range_sha256": "b".repeat(64)},
            "text_range": {"line_start": 0, "line_end": 1, "byte_start": 0, "byte_end": 1}
        },
        "domain": "work"
    }))
    .unwrap();
    assert_eq!(read.effective_domain(&config), "work");

    // A configured non-default domain is an explicit selection, but it only
    // gates Archive access: live sources keep their normal status and are never
    // silently replaced by a DOMAIN_* refusal.
    let request = SearchRequest::parse(&json!({"query": "needle"})).unwrap();
    let response = memory_search(&config, &request);
    assert!(response
        .source_status
        .iter()
        .all(|entry| entry.code != Some(codes::DOMAIN_UNREACHABLE)));
    assert!(response
        .source_status
        .iter()
        .all(|entry| entry.code != Some(codes::DOMAIN_DENIED)));
}

#[test]
fn archive_access_predicate_is_fail_closed() {
    let labels = |values: &[&str]| -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    };

    assert_eq!(
        classify_access(&[], "default", false),
        AccessDecision::Denied(codes::DOMAIN_DENIED)
    );
    assert_eq!(
        classify_access(&labels(&["work"]), "default", false),
        AccessDecision::Unreachable(codes::DOMAIN_UNREACHABLE)
    );
    assert_eq!(
        classify_access(&labels(&["work"]), "work", true),
        AccessDecision::Readable
    );
    assert_eq!(
        classify_access(&labels(&["work"]), "private", true),
        AccessDecision::Denied(codes::DOMAIN_DENIED)
    );

    assert_eq!(
        AccessDecision::Denied(codes::DOMAIN_DENIED).rejection(),
        Some((status::UNAVAILABLE, codes::DOMAIN_DENIED))
    );
    assert_eq!(
        AccessDecision::Unreachable(codes::DOMAIN_UNREACHABLE).rejection(),
        Some((status::UNAVAILABLE, codes::DOMAIN_UNREACHABLE))
    );
}

#[test]
fn archive_is_v2_only_and_an_uninitialized_root_is_explicit() {
    let env = support::TestEnv::new();
    let root = env.root("archive-root");

    let mut value = v2_config(&root);
    value["sources"][0] = json!({
        "source_id": "ar", "kind": "archive",
        "root": root.to_string_lossy(), "enabled": true
    });
    value["source_order"] = json!(["ar"]);
    let config = RecallConfig::from_value(&value).expect("archive in v2");
    assert_eq!(config.sources[0].kind, SourceKind::Archive);

    // P1: the provider exists, so an uninitialized archive root is an explicit
    // root failure — still never a healthy empty result.
    let request = SearchRequest::parse(&json!({"query": "needle"})).unwrap();
    let response = memory_search(&config, &request);
    assert!(response.results.is_empty());
    assert_eq!(response.exit_code, 4, "no results + unavailable source");
    assert_eq!(response.source_status[0].status, status::UNAVAILABLE);
    assert_eq!(
        response.source_status[0].code,
        Some(codes::SOURCE_ROOT_INVALID)
    );
    assert_eq!(response.errors[0].code, codes::SOURCE_ROOT_INVALID);

    let read = ReadRequest::parse(&json!({
        "ref": {
            "source_id": "ar", "source_kind": "archive",
            "root_sha256": agent_memory::recall::hash::root_sha256(
                &root.to_string_lossy()),
            "rel_path": format!("archive/objects/aa/aa/{}", "a".repeat(64)),
            "schema_gen": "archive-record-v1",
            "locator": {"record_sha256": "a".repeat(64), "projection_offset": 0},
            "fingerprint": {"range_sha256": "b".repeat(64)},
            "text_range": {"line_start": 0, "line_end": 1, "byte_start": 0, "byte_end": 1}
        }
    }))
    .unwrap();
    let response = memory_read(&config, &read);
    assert!(!response.ok);
    // The record/object cannot be located in an uninitialized archive.
    assert_eq!(response.code, Some(codes::REF_LOCATOR_MISSING));
    assert_eq!(response.exit_code, 5);
}

// ---------------------------------------------------------------------------
// Onboarding → v2 load → synthetic search
// ---------------------------------------------------------------------------

fn onboarding_script() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("repository root")
        .join("assets")
        .join("scripts")
        .join("recall_onboarding.py")
}

fn run_onboarding(user_root: &Path, vault: &Path, migrate: bool) -> Output {
    let mut command = Command::new(python_exe());
    command
        .env("PYTHONUTF8", "1")
        .env("PYTHONIOENCODING", "utf-8")
        .arg(onboarding_script())
        .arg("install")
        .arg("--user-root")
        .arg(user_root)
        .arg("--vault")
        .arg(vault)
        .arg("--python-exe")
        .arg(python_exe())
        .arg("--skip-opencode")
        .arg("--skip-codex");
    if migrate {
        command.arg("--migrate-v1");
    }
    command.output().expect("run recall_onboarding.py")
}

fn fixture_vault(_env: &support::TestEnv, _name: &str, user_root: &Path) -> PathBuf {
    let vault = user_root.join("vault");
    let script = vault.join(".meta").join("scripts").join("ask.py");
    std::fs::create_dir_all(script.parent().unwrap()).unwrap();
    std::fs::copy(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/recall/kb_stub/ask.py"),
        &script,
    )
    .expect("install kb stub");
    std::fs::create_dir_all(vault.join("notes")).unwrap();
    std::fs::write(vault.join("notes").join("alpha.md"), CHUNK).unwrap();
    vault
}

#[test]
fn onboarding_generates_v2_and_the_binary_loads_and_searches_it() {
    let env = support::TestEnv::new();
    let user_root = env.path().join("home-onboard");
    std::fs::create_dir_all(user_root.join(".agent-memory").join("bin")).unwrap();
    std::fs::write(
        user_root
            .join(".agent-memory")
            .join("bin")
            .join("agent-memory.exe"),
        b"stub",
    )
    .unwrap();
    let vault = fixture_vault(&env, "onboard", &user_root);

    let output = run_onboarding(&user_root, &vault, false);
    assert!(
        output.status.success(),
        "onboarding failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let items: Value = serde_json::from_slice(&output.stdout).expect("onboarding JSON");
    assert!(
        !items
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["status"] == json!("FAIL")),
        "{items}"
    );

    let config_path = user_root.join(".agent-memory").join("recall.json");
    let raw: Value = serde_json::from_slice(&std::fs::read(&config_path).unwrap()).unwrap();
    assert_eq!(raw["version"], json!(2));
    assert_eq!(raw["domain"], json!("default"));
    assert_eq!(raw["telemetry"]["enabled"], json!(false));
    assert_eq!(
        raw.as_object().unwrap().keys().cloned().collect::<std::collections::BTreeSet<_>>(),
        // Full-text L2 adds the explicit `cache_root` for the live index cache.
        ["cache_root", "domain", "interpreter", "source_order", "sources", "telemetry", "version"]
            .iter()
            .map(|key| key.to_string())
            .collect::<std::collections::BTreeSet<_>>()
    );
    assert_eq!(
        raw["cache_root"],
        json!(user_root
            .join(".agent-memory")
            .join("cache")
            .to_string_lossy()
            .replace('\\', "/"))
    );

    let config = agent_memory::recall::load_config(&config_path).expect("load generated v2");
    assert_eq!(config.version, 2);
    let request = SearchRequest::parse(&json!({"query": "stub"})).unwrap();
    let response = memory_search(&config, &request);
    assert_eq!(response.contract_version, 2);
    assert_eq!(response.exit_code, 0, "{:?}", response.errors);
    assert_eq!(response.results.len(), 1);
    assert_eq!(response.results[0].source_id, "kb");
}

#[test]
fn onboarding_preserves_v1_byte_for_byte_until_explicit_migration() {
    let env = support::TestEnv::new();
    let user_root = env.path().join("home-v1");
    std::fs::create_dir_all(user_root.join(".agent-memory").join("bin")).unwrap();
    std::fs::write(
        user_root
            .join(".agent-memory")
            .join("bin")
            .join("agent-memory.exe"),
        b"stub",
    )
    .unwrap();
    let vault = fixture_vault(&env, "v1", &user_root);
    let config_path = user_root.join(".agent-memory").join("recall.json");
    std::fs::write(
        &config_path,
        serde_json::to_vec_pretty(&v1_config(&vault)).unwrap(),
    )
    .unwrap();
    let before = std::fs::read(&config_path).unwrap();

    let output = run_onboarding(&user_root, &vault, false);
    assert!(
        output.status.success(),
        "preserve v1 failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(std::fs::read(&config_path).unwrap(), before);

    let output = run_onboarding(&user_root, &vault, true);
    assert!(
        output.status.success(),
        "migrate v1 failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let raw: Value = serde_json::from_slice(&std::fs::read(&config_path).unwrap()).unwrap();
    assert_eq!(raw["version"], json!(2));
    assert_eq!(raw["domain"], json!("default"));
    assert_eq!(raw["telemetry"]["enabled"], json!(false));
    let config = agent_memory::recall::load_config(&config_path).expect("migrated v2");
    assert_eq!(config.domain, "default");
}
