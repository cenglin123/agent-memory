//! A configuration written by a newer installer can name a source `kind` this
//! binary does not know. A long-running older process (an MCP server started
//! before the binary was replaced) must keep serving every other source and
//! disclose the skipped one, instead of failing every request.

mod support;

use agent_memory::recall::config::load_config;
use agent_memory::recall::contract::{codes, WARN_CONFIG_KEY_UNSUPPORTED, WARN_SOURCE_KIND_UNSUPPORTED};
use agent_memory::recall::recent::{memory_recent, RecentRequest};
use agent_memory::recall::search::{memory_search, SearchRequest};
use serde_json::json;

fn write_config(env: &support::TestEnv, name: &str, order: serde_json::Value) -> std::path::PathBuf {
    let cc = env.path().join("cc");
    std::fs::create_dir_all(cc.join("proj")).unwrap();
    std::fs::write(
        cc.join("proj").join("11111111-1111-4111-8111-111111111111.jsonl"),
        concat!(
            r#"{"type":"user","message":{"role":"user","content":"forward compat needle"},"#,
            r#""timestamp":"2026-11-07T00:00:00Z","sessionId":"11111111-1111-4111-8111-111111111111","uuid":"u1","cwd":"C:/w","entrypoint":"cli"}"#,
            "\n"
        ),
    )
    .unwrap();
    env.write_json(
        name,
        &json!({
            "version": 2,
            "sources": [
                {"source_id": "cc", "kind": "claude_code", "root": cc.to_string_lossy(), "enabled": true},
                {"source_id": "future", "kind": "future_kind", "root": "C:/nowhere", "enabled": true, "extra_key": 1}
            ],
            "source_order": order,
            "interpreter": {"kb": {"command": "C:/python/python.exe"}},
            "domain": "default"
        }),
    )
}

#[test]
fn an_unknown_source_kind_is_skipped_and_disclosed() {
    let env = support::TestEnv::new();
    for (name, order) in [
        ("with-order.json", json!(["cc", "future"])),
        ("without-order.json", json!(["cc"])),
    ] {
        let config = load_config(&write_config(&env, name, order)).expect(name);
        assert_eq!(config.enabled_sources(), vec!["cc".to_string()], "{name}");
        assert_eq!(config.source_order, vec!["cc".to_string()], "{name}");
        assert_eq!(config.unsupported_sources.len(), 1);
        assert_eq!(config.unsupported_sources[0].source_id, "future");
        assert_eq!(config.unsupported_sources[0].kind, "future_kind");

        let recent = memory_recent(&config, &RecentRequest::parse(&json!({"tail_turns": 1})).unwrap());
        assert!(recent.errors.is_empty(), "{name}: {:?}", recent.errors);
        assert_eq!(recent.sessions.len(), 1);
        assert!(recent
            .warnings
            .iter()
            .any(|w| w.code == WARN_SOURCE_KIND_UNSUPPORTED && w.source_id.as_deref() == Some("future")));

        let search = memory_search(
            &config,
            &SearchRequest::parse(&json!({"query": "forward compat needle"})).unwrap(),
        );
        assert!(search.errors.is_empty(), "{name}: {:?}", search.errors);
        assert!(!search.results.is_empty());
        assert!(search.warnings.iter().any(|w| w.code == WARN_SOURCE_KIND_UNSUPPORTED));
    }
}

#[test]
fn malformed_entries_keep_their_existing_errors() {
    let env = support::TestEnv::new();
    for (name, source) in [
        ("kind-not-string.json", json!({"source_id": "x", "kind": 7, "root": "C:/r", "enabled": true})),
        ("no-source-id.json", json!({"kind": "future_kind", "root": "C:/r", "enabled": true})),
    ] {
        let path = env.write_json(
            name,
            &json!({
                "version": 2,
                "sources": [source],
                "source_order": ["x"],
                "interpreter": {"kb": {"command": "C:/python/python.exe"}},
                "domain": "default"
            }),
        );
        let error = load_config(&path).expect_err(name);
        assert_eq!(error.code, codes::INVALID_REQUEST, "{name}");
    }

    // Every source unknown: nothing left to serve is still a configuration error.
    let path = env.write_json(
        "all-unknown.json",
        &json!({
            "version": 2,
            "sources": [{"source_id": "f", "kind": "future_kind", "root": "C:/r", "enabled": true}],
            "source_order": ["f"],
            "interpreter": {"kb": {"command": "C:/python/python.exe"}},
            "domain": "default"
        }),
    );
    assert_eq!(load_config(&path).expect_err("all unknown").code, codes::INVALID_REQUEST);
}

#[test]
fn spelling_variants_of_known_kinds_still_fail_and_disabled_unknowns_are_silent() {
    let env = support::TestEnv::new();
    for (name, kind) in [("typo.json", "claude-code"), ("pad.json", " Claude_Code ")] {
        let path = env.write_json(
            name,
            &json!({
                "version": 2,
                "sources": [
                    {"source_id": "k", "kind": "kb", "root": "C:/vault", "enabled": true},
                    {"source_id": "x", "kind": kind, "root": "C:/r", "enabled": true}
                ],
                "source_order": ["k", "x"],
                "interpreter": {"kb": {"command": "C:/python/python.exe"}},
                "domain": "default"
            }),
        );
        let error = load_config(&path).expect_err(name);
        assert_eq!(error.code, codes::INVALID_REQUEST, "{name}");
    }

    let config = load_config(&write_config_with(
        &env,
        "disabled-unknown.json",
        json!({"source_id": "future", "kind": "future_kind", "root": "C:/nowhere", "enabled": false}),
    ))
    .unwrap();
    assert_eq!(config.unsupported_sources.len(), 1);
    let recent = memory_recent(&config, &RecentRequest::parse(&json!({})).unwrap());
    assert!(recent.errors.is_empty());
    assert!(
        !recent.warnings.iter().any(|w| w.code == WARN_SOURCE_KIND_UNSUPPORTED),
        "a disabled unknown source is skipped silently"
    );
}

fn write_config_with(
    env: &support::TestEnv,
    name: &str,
    extra: serde_json::Value,
) -> std::path::PathBuf {
    let cc = env.path().join("cc2");
    std::fs::create_dir_all(cc.join("proj")).unwrap();
    env.write_json(
        name,
        &json!({
            "version": 2,
            "sources": [
                {"source_id": "cc", "kind": "claude_code", "root": cc.to_string_lossy(), "enabled": true},
                extra
            ],
            "source_order": ["cc"],
            "interpreter": {"kb": {"command": "C:/python/python.exe"}},
            "domain": "default"
        }),
    )
}

// ---------------------------------------------------------------------------
// Stage 7 §B: top-level unknown keys are ignored and disclosed
// ---------------------------------------------------------------------------

fn write_v2_with_top_level(
    env: &support::TestEnv,
    name: &str,
    key: &str,
    value: serde_json::Value,
) -> std::path::PathBuf {
    let cc = env.path().join("top-level-cc");
    std::fs::create_dir_all(cc.join("proj")).unwrap();
    std::fs::write(
        cc.join("proj")
            .join("11111111-1111-4111-8111-111111111111.jsonl"),
        concat!(
            r#"{"type":"user","message":{"role":"user","content":"top level key needle"},"#,
            r#""timestamp":"2026-11-07T00:00:00Z","sessionId":"11111111-1111-4111-8111-111111111111","uuid":"u1","cwd":"C:/w","entrypoint":"cli"}"#,
            "\n"
        ),
    )
    .unwrap();
    let mut config = json!({
        "version": 2,
        "sources": [{"source_id": "cc", "kind": "claude_code", "root": cc.to_string_lossy(), "enabled": true}],
        "source_order": ["cc"],
        "interpreter": {"kb": {"command": "C:/python/python.exe"}},
        "domain": "default"
    });
    config[key] = value;
    env.write_json(name, &config)
}

#[test]
fn an_unknown_top_level_key_is_ignored_and_disclosed() {
    let env = support::TestEnv::new();
    let path = write_v2_with_top_level(
        &env,
        "top-level-unknown.json",
        "future_retrieval",
        json!({"mode": "future"}),
    );
    let config = load_config(&path).expect("an unknown top-level key is ignored");
    assert_eq!(
        config.unsupported_keys,
        vec!["future_retrieval".to_string()]
    );

    let search = memory_search(
        &config,
        &SearchRequest::parse(&json!({"query": "top level key needle"})).unwrap(),
    );
    assert!(search.errors.is_empty(), "{:?}", search.errors);
    assert!(!search.results.is_empty());
    let warning = search
        .warnings
        .iter()
        .find(|warning| warning.code == WARN_CONFIG_KEY_UNSUPPORTED)
        .expect("search must disclose the ignored key");
    assert_eq!(
        warning.detail.as_ref().unwrap()["keys"],
        json!(["future_retrieval"])
    );

    let recent = memory_recent(&config, &RecentRequest::parse(&json!({"tail_turns": 1})).unwrap());
    assert!(recent.errors.is_empty(), "{:?}", recent.errors);
    assert_eq!(recent.sessions.len(), 1);
    assert!(recent
        .warnings
        .iter()
        .any(|warning| warning.code == WARN_CONFIG_KEY_UNSUPPORTED));
}

#[test]
fn spelling_variants_of_known_keys_are_still_errors_and_v1_stays_strict() {
    let env = support::TestEnv::new();
    for (name, key) in [
        ("variant-cache-root.json", "cache-root"),
        ("variant-source-order.json", "Source_Order"),
    ] {
        let path = write_v2_with_top_level(&env, name, key, json!("C:/cache"));
        let error = load_config(&path).expect_err(name);
        assert_eq!(error.code, codes::INVALID_REQUEST, "{name}");
        assert_eq!(error.detail.unwrap()["path"], key);
    }

    let cc = env.path().join("v1-cc");
    std::fs::create_dir_all(&cc).unwrap();
    let path = env.write_json(
        "v1-unknown.json",
        &json!({
            "version": 1,
            "sources": [{"source_id": "cc", "kind": "claude_code", "root": cc.to_string_lossy(), "enabled": true}],
            "source_order": ["cc"],
            "interpreter": {"kb": {"command": "C:/python/python.exe"}},
            "future_retrieval": 1
        }),
    );
    let error = load_config(&path).expect_err("v1 keeps the strict behavior");
    assert_eq!(error.code, codes::INVALID_REQUEST);
    assert_eq!(error.detail.unwrap()["path"], "future_retrieval");
}
