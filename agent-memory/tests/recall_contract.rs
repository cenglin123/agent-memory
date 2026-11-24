//! P0 contract tests: configuration, request validation, frozen tables and
//! exit-code derivation.

mod support;

use agent_memory::recall::config::RecallConfig;
// Through the module re-export: `recall::load_config` is part of the documented
// API surface used by the README example.
use agent_memory::recall::load_config;
use agent_memory::recall::contract::{
    codes, is_incomplete, search_exit_code, status, status_priority, RecallError,
    ALL_ERROR_CODES, DEGRADED_STATUSES, DETAIL_KEYS, INCOMPLETE_STATUSES, READ_CONTEXT_TURN,
    READ_REQUEST_KEY_CONTEXT, READ_RESPONSE_KEYS, READ_WARNING_CODES, REF_KEYS,
    SEARCH_REQUEST_KEY_RELATED_HOPS, SEARCH_WARNING_CODES, STATUS_PRIORITY,
    RECENT_WARNING_CODES, WARN_CONFIG_KEY_UNSUPPORTED,
};
use agent_memory::recall::read::ReadRequest;
use agent_memory::recall::search::SearchRequest;
use serde_json::{json, Value};
use std::collections::BTreeSet;

fn base_config() -> Value {
    json!({
        "version": 1,
        "sources": [
            {"source_id": "oc", "kind": "opencode", "root": "C:/work/opencode", "enabled": true}
        ],
        "source_order": ["oc"],
        "interpreter": {
            "kb": {
                "command": "C:/python/python.exe",
                "args": ["-B"],
                "env": {"PYTHONUTF8": "1"}
            }
        }
    })
}

fn detail_path(error: &RecallError) -> String {
    error
        .detail
        .as_ref()
        .and_then(|detail| detail.get("path"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

fn detail_reason(error: &RecallError) -> String {
    error
        .detail
        .as_ref()
        .and_then(|detail| detail.get("reason"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

fn sample_ref() -> Value {
    json!({
        "source_id": "oc",
        "source_kind": "opencode",
        "root_sha256": "a".repeat(64),
        "rel_path": "opencode.db",
        "schema_gen": "sqlite-v1",
        "locator": {"session_id": "s", "message_id": "m", "part_id": "p"},
        "fingerprint": {"range_sha256": "b".repeat(64), "row_sha256": "c".repeat(64)},
        "text_range": {"line_start": 0, "line_end": 1, "byte_start": 0, "byte_end": 4}
    })
}

type Mutate = Box<dyn Fn(&mut Value)>;

fn case(
    mutate: impl Fn(&mut Value) + 'static,
    path: &'static str,
    reason: &'static str,
) -> (Mutate, &'static str, &'static str) {
    (Box::new(mutate), path, reason)
}

#[test]
fn frozen_tables_have_no_duplicates_and_consistent_sets() {
    assert_eq!(ALL_ERROR_CODES.len(), 27);
    let unique: BTreeSet<&&str> = ALL_ERROR_CODES.iter().collect();
    assert_eq!(unique.len(), ALL_ERROR_CODES.len(), "error codes repeat");

    // Full-text L3 adds `degraded`: an exact-mode source that answered through
    // a scan fallback (INDEX_MISSING/STALE/CORRUPT) is neither healthy nor an
    // `errors[]` entry, so it sits in STATUS_PRIORITY and DEGRADED_STATUSES but
    // deliberately stays out of INCOMPLETE_STATUSES.
    assert_eq!(STATUS_PRIORITY.len(), 9);
    for status_value in STATUS_PRIORITY {
        assert_eq!(status_priority(status_value) < STATUS_PRIORITY.len(), true);
    }
    assert_eq!(STATUS_PRIORITY[0], status::UNCONFIGURED);
    assert_eq!(*STATUS_PRIORITY.last().unwrap(), status::OK);

    assert_eq!(INCOMPLETE_STATUSES.len(), 6);
    for value in INCOMPLETE_STATUSES {
        assert!(STATUS_PRIORITY.contains(&value), "{value} is not a status");
        assert!(is_incomplete(value));
    }
    assert!(!is_incomplete(status::OK));
    assert!(!is_incomplete(status::EMPTY));

    assert_eq!(DEGRADED_STATUSES.len(), 6);
    assert!(DEGRADED_STATUSES.contains(&status::DEGRADED));
    assert!(!INCOMPLETE_STATUSES.contains(&status::DEGRADED));
    assert!(!is_incomplete(status::DEGRADED));
    assert!(DEGRADED_STATUSES.contains(&status::ERROR));
    assert!(!DEGRADED_STATUSES.contains(&status::UNCONFIGURED));

    assert_eq!(SEARCH_WARNING_CODES.len(), 5);
    assert_eq!(RECENT_WARNING_CODES.len(), 4);
    assert_eq!(READ_WARNING_CODES.len(), 1);
    assert!(SEARCH_WARNING_CODES.contains(&WARN_CONFIG_KEY_UNSUPPORTED));
    assert!(RECENT_WARNING_CODES.contains(&WARN_CONFIG_KEY_UNSUPPORTED));
    assert!(!READ_WARNING_CODES.contains(&WARN_CONFIG_KEY_UNSUPPORTED));
    for code in SEARCH_WARNING_CODES {
        assert!(!READ_WARNING_CODES.contains(&code), "warning sets must not cross");
    }
    assert!(READ_WARNING_CODES.contains(&"WARN_FILE_STALE"));
    assert!(!SEARCH_WARNING_CODES.contains(&"WARN_FILE_STALE"));

    // Full-text L3 adds the `exact` block (index state / fallback reason);
    // L5b adds the `hybrid` block (branches, fusion, thresholds version) and
    // `resolved_mode` (which path answered this source).
    assert_eq!(DETAIL_KEYS.len(), 14);
    assert!(DETAIL_KEYS.contains(&"exact"));
    assert!(DETAIL_KEYS.contains(&"hybrid"));
    assert!(DETAIL_KEYS.contains(&"resolved_mode"));
    assert!(DETAIL_KEYS.contains(&"ignored_types"));
    assert!(DETAIL_KEYS.contains(&"CONTENT_UNRESOLVED"));
    assert!(DETAIL_KEYS.contains(&"archive"));
    assert!(DETAIL_KEYS.contains(&"semantic"));
    assert_eq!(REF_KEYS.len(), 8);
    assert_eq!(READ_RESPONSE_KEYS.len(), 9);
}

#[test]
fn p4_request_keys_are_frozen() {
    // The two opt-in request keys and the single non-default context mode are
    // contract constants: a transport cannot invent a second spelling.
    assert_eq!(SEARCH_REQUEST_KEY_RELATED_HOPS, "related_hops");
    assert_eq!(READ_REQUEST_KEY_CONTEXT, "context");
    assert_eq!(READ_CONTEXT_TURN, "turn");
    assert_eq!(agent_memory::recall::graph::RELATED_MAX_HOPS, 2);
    assert!(agent_memory::recall::graph::RELATED_NODES_PER_HIT <= 8);
    assert!(agent_memory::recall::graph::RELATED_NODES_PER_SOURCE >= 1);
}

#[test]
fn exit_codes_follow_the_frozen_table() {
    assert_eq!(search_exit_code(true, &[status::OK]), 0);
    assert_eq!(search_exit_code(true, &[status::OK, status::EMPTY]), 0);
    assert_eq!(search_exit_code(true, &[status::UNCONFIGURED, status::OK]), 0);
    assert_eq!(search_exit_code(true, &[status::PARTIAL]), 10);
    assert_eq!(search_exit_code(true, &[status::UNAVAILABLE, status::OK]), 10);
    assert_eq!(search_exit_code(true, &[status::TRUNCATED, status::EMPTY]), 10);

    assert_eq!(search_exit_code(false, &[status::UNCONFIGURED]), 3);
    assert_eq!(
        search_exit_code(false, &[status::UNCONFIGURED, status::UNCONFIGURED]),
        3
    );
    assert_eq!(search_exit_code(false, &[status::UNAVAILABLE]), 4);
    assert_eq!(search_exit_code(false, &[status::PARTIAL, status::OK]), 4);
    assert_eq!(search_exit_code(false, &[status::EMPTY]), 0);
    assert_eq!(
        search_exit_code(false, &[status::UNCONFIGURED, status::OK]),
        0
    );
}

#[test]
fn config_accepts_the_contract_example() {
    let config = RecallConfig::from_value(&base_config()).expect("valid config");
    assert_eq!(config.version, 1);
    assert_eq!(config.source_order, vec!["oc".to_string()]);
    assert_eq!(config.interpreter.kb.args, vec!["-B".to_string()]);
    assert_eq!(
        config.interpreter.kb.env.get("PYTHONUTF8").map(String::as_str),
        Some("1")
    );
}

#[test]
fn config_rejects_unknown_and_missing_keys_with_paths() {
    for (mutate, path, reason) in [
        case(|value: &mut Value| value["extra"] = json!(1), "extra", "unknown_key"),
        case(
            |value: &mut Value| {
                value.as_object_mut().unwrap().remove("version");
            },
            "version",
            "missing_key",
        ),
        case(
            |value: &mut Value| {
                value.as_object_mut().unwrap().remove("sources");
            },
            "sources",
            "missing_key",
        ),
        case(
            |value: &mut Value| {
                value.as_object_mut().unwrap().remove("source_order");
            },
            "source_order",
            "missing_key",
        ),
        case(
            |value: &mut Value| {
                value.as_object_mut().unwrap().remove("interpreter");
            },
            "interpreter",
            "missing_key",
        ),
        case(
            |value: &mut Value| value["interpreter"]["kb"] = json!({}),
            "interpreter.kb.command",
            "missing_key",
        ),
        case(
            |value: &mut Value| value["sources"][0]["extra"] = json!(1),
            "sources[0].extra",
            "unknown_key",
        ),
        case(
            |value: &mut Value| value["interpreter"]["other"] = json!({}),
            "interpreter.other",
            "unknown_key",
        ),
        case(
            |value: &mut Value| value["interpreter"]["kb"]["args"] = json!([1]),
            "interpreter.kb.args[0]",
            "invalid_type",
        ),
        case(
            |value: &mut Value| value["interpreter"]["kb"]["env"] = json!({"A": 1}),
            "interpreter.kb.env.A",
            "invalid_type",
        ),
    ] {
        let mut value = base_config();
        mutate(&mut value);
        let error = RecallConfig::from_value(&value).expect_err("must be rejected");
        assert_eq!(error.code, codes::INVALID_REQUEST);
        assert_eq!(detail_path(&error), path);
        assert_eq!(detail_reason(&error), reason);
        assert_eq!(error.exit_code(), 2);
    }
}

#[test]
fn config_rejects_versions_order_roots_and_source_shapes() {
    let mut value = base_config();
    value["version"] = json!(3);
    let error = RecallConfig::from_value(&value).unwrap_err();
    assert_eq!(detail_path(&error), "version");

    let mut value = base_config();
    value["version"] = json!("1");
    assert_eq!(
        detail_path(&RecallConfig::from_value(&value).unwrap_err()),
        "version"
    );

    let mut value = base_config();
    value["sources"] = json!([]);
    value["source_order"] = json!([]);
    let error = RecallConfig::from_value(&value).unwrap_err();
    assert_eq!(detail_reason(&error), "empty_sources");

    let mut value = base_config();
    value["sources"][0]["enabled"] = json!(false);
    value["source_order"] = json!([]);
    let error = RecallConfig::from_value(&value).unwrap_err();
    assert_eq!(detail_reason(&error), "no_enabled_source");

    let mut value = base_config();
    value["source_order"] = json!(["oc", "device"]);
    let error = RecallConfig::from_value(&value).unwrap_err();
    assert_eq!(detail_path(&error), "source_order");
    assert_eq!(detail_reason(&error), "source_order_mismatch");

    let mut value = base_config();
    value["source_order"] = json!(["oc", "oc"]);
    let error = RecallConfig::from_value(&value).unwrap_err();
    assert_eq!(detail_path(&error), "source_order[1]");

    for root in [r"\\server\share", "//server/share", "relative/x", r"\\?\C:\x"] {
        let mut value = base_config();
        value["sources"][0]["root"] = json!(root);
        let error = RecallConfig::from_value(&value).unwrap_err();
        assert_eq!(
            detail_path(&error),
            "sources[0].root",
            "root {root} must be rejected"
        );
    }

    let mut value = base_config();
    value["sources"][0]["source_id"] = json!("");
    let error = RecallConfig::from_value(&value).unwrap_err();
    assert_eq!(detail_path(&error), "sources[0].source_id");

    let mut value = base_config();
    value["sources"]
        .as_array_mut()
        .unwrap()
        .push(json!({"source_id": "oc", "kind": "device", "root": "C:/d", "enabled": false}));
    let error = RecallConfig::from_value(&value).unwrap_err();
    assert_eq!(detail_reason(&error), "duplicate_source_id");

    let mut value = base_config();
    value["sources"][0]["kind"] = json!("unknown");
    let error = RecallConfig::from_value(&value).unwrap_err();
    assert_eq!(detail_path(&error), "sources[0].kind");
}

#[test]
fn config_checks_db_and_storage_containment_case_insensitively() {
    let mut value = base_config();
    value["sources"][0]["db"] = json!("c:/WORK/opencode/opencode.db");
    value["sources"][0]["storage"] = json!(r"c:\work\opencode\storage");
    RecallConfig::from_value(&value).expect("case-insensitive containment");

    let mut value = base_config();
    value["sources"][0]["db"] = json!("C:/work/other.db");
    let error = RecallConfig::from_value(&value).unwrap_err();
    assert_eq!(detail_path(&error), "sources[0].db");
    assert_eq!(detail_reason(&error), "outside_root");

    let mut value = base_config();
    value["sources"][0]["storage"] = json!("C:/work/opencode/../escape");
    let error = RecallConfig::from_value(&value).unwrap_err();
    assert_eq!(detail_path(&error), "sources[0].storage");
}

#[test]
fn load_config_never_discovers_a_default_file() {
    let env = support::TestEnv::new();
    let missing = env.path().join("no-such-config.json");
    let error = load_config(&missing).unwrap_err();
    assert_eq!(error.code, codes::INVALID_REQUEST);
    assert_eq!(error.exit_code(), 2);

    let broken = env.write("broken.json", b"{not json");
    let error = load_config(&broken).unwrap_err();
    assert_eq!(detail_reason(&error), "config_invalid_json");
}

#[test]
fn search_request_defaults_and_validation() {
    let request = SearchRequest::parse(&json!({"query": "needle"})).expect("defaults");
    assert_eq!(request.limit, 10);
    assert_eq!(request.offset, 0);
    assert_eq!(request.snippet_bytes, 400);
    assert_eq!(request.budget.max_bytes_per_source, 33_554_432);
    assert_eq!(request.budget.max_elapsed_ms_per_source, 20_000);
    assert_eq!(request.budget.kb_top_k, 20);

    for (mutate, path, reason) in [
        case(
            |value: &mut Value| value["time_range"] = json!({"from": 1}),
            "time_range",
            "unknown_key",
        ),
        case(
            |value: &mut Value| value["project"] = json!("x"),
            "project",
            "unknown_key",
        ),
        case(
            |value: &mut Value| {
                value.as_object_mut().unwrap().remove("query");
            },
            "query",
            "missing_key",
        ),
        case(|value: &mut Value| value["query"] = json!(""), "query", "empty_query"),
        case(|value: &mut Value| value["query"] = json!(7), "query", "invalid_type"),
        case(
            |value: &mut Value| value["sources"] = json!([]),
            "sources",
            "empty_sources",
        ),
        case(
            |value: &mut Value| value["sources"] = json!(["oc", "oc"]),
            "sources[1]",
            "duplicate_source_id",
        ),
        case(
            |value: &mut Value| value["sources"] = json!([1]),
            "sources[0]",
            "invalid_type",
        ),
        case(
            |value: &mut Value| value["limit"] = json!(0),
            "limit",
            "out_of_range",
        ),
        case(
            |value: &mut Value| value["limit"] = json!(101),
            "limit",
            "out_of_range",
        ),
        case(
            |value: &mut Value| value["limit"] = json!(true),
            "limit",
            "out_of_range",
        ),
        case(
            |value: &mut Value| value["limit"] = json!("10"),
            "limit",
            "out_of_range",
        ),
        case(
            |value: &mut Value| value["snippet_bytes"] = json!(0),
            "snippet_bytes",
            "out_of_range",
        ),
        case(
            |value: &mut Value| value["snippet_bytes"] = json!(8193),
            "snippet_bytes",
            "out_of_range",
        ),
        case(
            |value: &mut Value| value["budget"] = json!(null),
            "budget",
            "invalid_type",
        ),
        case(
            |value: &mut Value| value["budget"] = json!({"extra": 1}),
            "budget.extra",
            "unknown_key",
        ),
        case(
            |value: &mut Value| value["budget"] = json!({"max_bytes_per_source": 8_191}),
            "budget.max_bytes_per_source",
            "out_of_range",
        ),
        case(
            |value: &mut Value| value["budget"] = json!({"max_elapsed_ms_per_source": 3_999}),
            "budget.max_elapsed_ms_per_source",
            "out_of_range",
        ),
        case(
            |value: &mut Value| value["budget"] = json!({"max_elapsed_ms_per_source": 600_001}),
            "budget.max_elapsed_ms_per_source",
            "out_of_range",
        ),
        case(
            |value: &mut Value| value["budget"] = json!({"kb_top_k": 0}),
            "budget.kb_top_k",
            "out_of_range",
        ),
        case(
            |value: &mut Value| value["budget"] = json!({"kb_top_k": 51}),
            "budget.kb_top_k",
            "out_of_range",
        ),
    ] {
        let mut value = json!({"query": "needle"});
        mutate(&mut value);
        let error = SearchRequest::parse(&value).expect_err(path);
        assert_eq!(detail_path(&error), path, "path for {path}");
        assert_eq!(detail_reason(&error), reason, "reason for {path}");
        assert_eq!(error.exit_code(), 2);
    }

    for offset in [json!(-1), json!(10_001), json!(1.5), json!(true), json!("0")] {
        let error = SearchRequest::parse(&json!({"query": "needle", "offset": offset}))
            .expect_err("invalid offset");
        assert_eq!(error.code, codes::INVALID_OFFSET);
        assert_eq!(detail_path(&error), "offset");
        assert_eq!(error.exit_code(), 2);
    }
}

#[test]
fn read_request_freezes_ref_locator_fingerprint_and_range() {
    let valid = json!({"ref": sample_ref()});
    let request = ReadRequest::parse(&valid).expect("valid read request");
    assert_eq!(request.context_before, 0);
    assert_eq!(request.max_bytes, 8_192);

    let cases: Vec<(Value, &str)> = vec![
        (json!({}), "ref"),
        (
            json!({"ref": sample_ref(), "extra": 1}),
            "extra",
        ),
        (json!({"ref": {}}), "ref.source_id"),
        (
            {
                let mut reference = sample_ref();
                reference["extra"] = json!(1);
                json!({"ref": reference})
            },
            "ref.extra",
        ),
        (
            {
                let mut reference = sample_ref();
                reference.as_object_mut().unwrap().remove("text_range");
                json!({"ref": reference})
            },
            "ref.text_range",
        ),
        (
            {
                let mut reference = sample_ref();
                reference["locator"] = json!({"session_id": "s", "extra": "x"});
                json!({"ref": reference})
            },
            "ref.locator.extra",
        ),
        (
            {
                let mut reference = sample_ref();
                reference["fingerprint"] =
                    json!({"range_sha256": "a", "file_sha256": "b", "row_sha256": "c"});
                json!({"ref": reference})
            },
            "ref.fingerprint",
        ),
        (
            {
                let mut reference = sample_ref();
                reference["text_range"]["byte_start"] = json!(9);
                json!({"ref": reference})
            },
            "ref.text_range",
        ),
        (
            {
                let mut reference = sample_ref();
                reference["schema_gen"] = json!("md-v9");
                json!({"ref": reference})
            },
            "ref.schema_gen",
        ),
        (
            json!({"ref": sample_ref(), "context_before": -1}),
            "context_before",
        ),
        (
            json!({"ref": sample_ref(), "context_after": 1001}),
            "context_after",
        ),
        (json!({"ref": sample_ref(), "max_bytes": 0}), "max_bytes"),
        (
            json!({"ref": sample_ref(), "max_bytes": 1_048_577}),
            "max_bytes",
        ),
    ];
    for (value, path) in cases {
        let error = ReadRequest::parse(&value).expect_err(path);
        assert_eq!(error.code, codes::INVALID_REQUEST, "code for {path}");
        assert_eq!(detail_path(&error), path);
        assert_eq!(error.exit_code(), 2);
    }

    // md-v1 locators must be an empty object.
    let mut reference = sample_ref();
    reference["schema_gen"] = json!("md-v1");
    reference["locator"] = json!({"session_id": "s"});
    let error = ReadRequest::parse(&json!({"ref": reference})).unwrap_err();
    assert_eq!(detail_path(&error), "ref.locator.session_id");

    // V2 pointers are validated.
    let mut reference = sample_ref();
    reference["schema_gen"] = json!("sqlite-v2");
    reference["locator"] = json!({
        "session_id": "s",
        "session_message_id": "sm",
        "pointer": "/content/x"
    });
    let error = ReadRequest::parse(&json!({"ref": reference})).unwrap_err();
    assert_eq!(detail_path(&error), "ref.locator.pointer");
}

// ---------------------------------------------------------------------------
// Codex contract additions (`codex-rollout-v1`, session_id)
// ---------------------------------------------------------------------------

#[test]
fn schema_generation_table_includes_codex_rollout() {
    use agent_memory::recall::contract::{
        ALL_SCHEMA_GENS, SCHEMA_ARCHIVE_RECORD_V1, SCHEMA_CODEX_ROLLOUT_V1, SOURCE_KIND_CODEX,
    };
    use agent_memory::recall::refs::locator_schema;

    // Full-text L2 adds the Claude Code transcript generation; the DSH source
    // adds its frame-logical generation; the docs source adds its chunk
    // generation; Hermes adds its SQLite generation.
    assert_eq!(ALL_SCHEMA_GENS.len(), 10);
    let unique: BTreeSet<&&str> = ALL_SCHEMA_GENS.iter().collect();
    assert_eq!(unique.len(), ALL_SCHEMA_GENS.len());
    assert!(ALL_SCHEMA_GENS.contains(&SCHEMA_CODEX_ROLLOUT_V1));
    assert!(ALL_SCHEMA_GENS.contains(&SCHEMA_ARCHIVE_RECORD_V1));
    assert!(ALL_SCHEMA_GENS.contains(&"dsh-jsonl"));
    assert!(ALL_SCHEMA_GENS.contains(&"hermes-sqlite-v1"));
    assert!(ALL_SCHEMA_GENS.contains(&"docs-md-v1"));
    assert_eq!(SCHEMA_CODEX_ROLLOUT_V1, "codex-rollout-v1");
    assert_eq!(SOURCE_KIND_CODEX, "codex");

    let keys = locator_schema(SOURCE_KIND_CODEX, SCHEMA_CODEX_ROLLOUT_V1).expect("codex locator");
    assert_eq!(
        keys,
        &["session_id", "message_id", "record_index", "pointer"]
    );
    assert!(locator_schema("opencode", SCHEMA_CODEX_ROLLOUT_V1).is_none());

    let keys = locator_schema(
        agent_memory::recall::contract::SOURCE_KIND_DSH,
        "dsh-jsonl",
    )
    .expect("dsh locator");
    assert_eq!(keys, &["session_id", "carrier", "record_index", "pointer"]);

    let keys = locator_schema("hermes", "hermes-sqlite-v1").expect("hermes locator");
    assert_eq!(
        keys,
        &["session_id", "message_id", "record_index", "pointer"]
    );
}

#[test]
fn config_accepts_codex_and_checks_its_path_boundaries() {
    let mut value = json!({
        "version": 1,
        "sources": [{
            "source_id": "cx", "kind": "codex", "root": "C:/codex",
            "db": "C:/codex/state_5.sqlite",
            "storage": "C:/codex/sessions",
            "enabled": true
        }],
        "source_order": ["cx"],
        "interpreter": {"kb": {"command": "C:/python/python.exe"}}
    });
    let config = RecallConfig::from_value(&value).expect("codex source");
    assert_eq!(config.sources[0].kind, agent_memory::recall::SourceKind::Codex);
    assert_eq!(config.sources[0].db.as_deref(), Some("C:/codex/state_5.sqlite"));
    assert_eq!(config.sources[0].storage.as_deref(), Some("C:/codex/sessions"));

    // Unknown keys stay rejected for codex too.
    value["sources"][0]["extra"] = json!(1);
    let error = RecallConfig::from_value(&value).unwrap_err();
    assert_eq!(detail_path(&error), "sources[0].extra");
    assert_eq!(detail_reason(&error), "unknown_key");

    let mut value = json!({
        "version": 1,
        "sources": [{
            "source_id": "cx", "kind": "codex", "root": "C:/codex",
            "db": "C:/other/state_5.sqlite", "enabled": true
        }],
        "source_order": ["cx"],
        "interpreter": {"kb": {"command": "C:/python/python.exe"}}
    });
    let error = RecallConfig::from_value(&value).unwrap_err();
    assert_eq!(detail_path(&error), "sources[0].db");
    assert_eq!(detail_reason(&error), "outside_root");

    value["sources"][0]["db"] = json!("C:/codex/state_5.sqlite");
    value["sources"][0]["storage"] = json!("C:/elsewhere/sessions");
    let error = RecallConfig::from_value(&value).unwrap_err();
    assert_eq!(detail_path(&error), "sources[0].storage");

    // `source_order` must cover the enabled codex source.
    value["sources"][0]
        .as_object_mut()
        .unwrap()
        .remove("storage");
    value["source_order"] = json!([]);
    let error = RecallConfig::from_value(&value).unwrap_err();
    assert_eq!(detail_reason(&error), "source_order_mismatch");

    // The kind literal is one of four.
    let mut value = json!({
        "version": 1,
        "sources": [{"source_id": "cx", "kind": "codexx", "root": "C:/codex", "enabled": true}],
        "source_order": ["cx"],
        "interpreter": {"kb": {"command": "C:/python/python.exe"}}
    });
    let error = RecallConfig::from_value(&value).unwrap_err();
    assert_eq!(detail_path(&error), "sources[0].kind");
    value["sources"][0]["kind"] = json!("codex");
    RecallConfig::from_value(&value).expect("fourth kind is accepted");
}

#[test]
fn search_request_validates_and_preserves_session_id() {
    let parsed = SearchRequest::parse(&json!({"query": "needle", "session_id": "ses_abc"}))
        .expect("session id");
    assert_eq!(parsed.session_id.as_deref(), Some("ses_abc"));

    // The value is never trimmed: trimming is only used to reject blanks.
    let parsed = SearchRequest::parse(&json!({"query": "needle", "session_id": " id "}))
        .expect("kept verbatim");
    assert_eq!(parsed.session_id.as_deref(), Some(" id "));

    // The 200 limit is characters (as in the MCP schema), not bytes.
    let multibyte = "界".repeat(200);
    let parsed = SearchRequest::parse(&json!({"query": "needle", "session_id": multibyte.clone()}))
        .expect("200 multibyte characters");
    assert_eq!(parsed.session_id.as_deref(), Some(multibyte.as_str()));
    let error = SearchRequest::parse(&json!({
        "query": "needle", "session_id": "界".repeat(201)
    }))
    .unwrap_err();
    assert_eq!(detail_reason(&error), "too_long");

    assert_eq!(
        SearchRequest::parse(&json!({"query": "needle"})).unwrap().session_id,
        None
    );

    for (value, reason) in [
        (json!(""), "empty_value"),
        (json!("   "), "empty_value"),
        (json!("a".repeat(201)), "too_long"),
        (json!("bad\u{7}id"), "control_char"),
        (json!(42), "invalid_type"),
        (json!(null), "invalid_type"),
    ] {
        let error = SearchRequest::parse(&json!({"query": "needle", "session_id": value}))
            .expect_err(reason);
        assert_eq!(error.code, codes::INVALID_REQUEST);
        assert_eq!(detail_path(&error), "session_id");
        assert_eq!(detail_reason(&error), reason);
    }
}

#[test]
fn read_request_parses_the_codex_locator_shape_only() {
    let codex_ref = |locator: Value| {
        json!({
            "source_id": "cx",
            "source_kind": "codex",
            "root_sha256": "a".repeat(64),
            "rel_path": "sessions/2026/03/06/rollout-a.jsonl",
            "schema_gen": "codex-rollout-v1",
            "locator": locator,
            "fingerprint": {"range_sha256": "b".repeat(64), "row_sha256": "c".repeat(64)},
            "text_range": {"line_start": 0, "line_end": 1, "byte_start": 0, "byte_end": 4}
        })
    };

    let request = ReadRequest::parse(&json!({"ref": codex_ref(json!({
        "session_id": "s1",
        "message_id": "msg-1",
        "record_index": 3,
        "pointer": "/payload/output"
    }))}))
    .expect("codex locator");
    let locator = &request.reference.locator;
    assert_eq!(locator.session_id.as_deref(), Some("s1"));
    assert_eq!(locator.message_id.as_deref(), Some("msg-1"));
    assert_eq!(locator.record_index, Some(3));
    assert_eq!(locator.pointer.as_deref(), Some("/payload/output"));
    assert!(!locator.is_empty());

    // Optional message_id.
    ReadRequest::parse(&json!({"ref": codex_ref(json!({
        "session_id": "s1", "record_index": 0, "pointer": "/payload/content"
    }))}))
    .expect("message_id is optional");

    for (locator, path, reason) in [
        (
            json!({"session_id": "s1", "record_index": 0, "pointer": "payload/content"}),
            "ref.locator.pointer",
            "invalid_pointer",
        ),
        (
            json!({"session_id": "s1", "record_index": 0, "pointer": "/a~2b"}),
            "ref.locator.pointer",
            "invalid_pointer",
        ),
        (
            json!({"session_id": "s1", "pointer": "/payload/content"}),
            "ref.locator.record_index",
            "missing_key",
        ),
        (
            json!({"session_id": "s1", "record_index": "3", "pointer": "/payload/content"}),
            "ref.locator.record_index",
            "invalid_type",
        ),
        (
            json!({"record_index": 0, "pointer": "/payload/content"}),
            "ref.locator.session_id",
            "missing_key",
        ),
        (
            json!({"session_id": "s1", "record_index": 0, "pointer": "/payload/content", "extra": 1}),
            "ref.locator.extra",
            "unknown_key",
        ),
    ] {
        let error = ReadRequest::parse(&json!({"ref": codex_ref(locator)})).unwrap_err();
        assert_eq!(detail_path(&error), path);
        assert_eq!(detail_reason(&error), reason);
    }
}
