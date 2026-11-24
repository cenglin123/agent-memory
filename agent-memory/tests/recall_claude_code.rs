//! Claude Code live source (`claude-jsonl-v1`): lexical search, ref readback and
//! the subagent/sidechain policy. Synthetic transcripts only.

use agent_memory::recall::config::load_config;
use agent_memory::recall::read::{memory_read, ReadRequest};
use agent_memory::recall::search::{memory_search, SearchRequest};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

mod support;

fn claude_source(id: &str, root: &Path) -> Value {
    json!({
        "source_id": id,
        "kind": "claude_code",
        "root": root.to_string_lossy(),
        "enabled": true,
    })
}

fn config(env: &support::TestEnv, root: &Path) -> PathBuf {
    env.write_json(
        "claude-config.json",
        &json!({
            "version": 2,
            "sources": [claude_source("cc", root)],
            "source_order": ["cc"],
            "interpreter": {"kb": {"command": "C:/python/python.exe"}},
            "domain": "default",
            "telemetry": {"enabled": false}
        }),
    )
}

fn line(
    session: &str,
    uuid: &str,
    time: &str,
    kind: &str,
    content: Value,
    sidechain: bool,
) -> String {
    json!({
        "type": kind,
        "message": {"role": kind, "content": content},
        "timestamp": time,
        "sessionId": session,
        "uuid": uuid,
        "isSidechain": sidechain
    })
    .to_string()
}

struct Fixture {
    root: PathBuf,
    main: PathBuf,
    sidechain_record_file: PathBuf,
    subagent: PathBuf,
}

const SESSION: &str = "11111111-1111-4111-8111-111111111111";

fn build(env: &support::TestEnv) -> Fixture {
    let root = env.root("claude");
    let project = root.join("proj");
    std::fs::create_dir_all(&project).unwrap();
    let main = project.join(format!("{SESSION}.jsonl"));
    let lines = vec![
        line(SESSION, "u1", "2026-10-25T13:25:00Z", "user", json!("装回 Python 3.13.17 needlecc"), false),
        line(
            SESSION,
            "a1",
            "2026-10-25T13:26:00Z",
            "assistant",
            json!([
                {"type": "text", "text": "interpreter.kb needlecc 指向 Python313"},
                {"type": "tool_use", "id": "t1", "name": "Bash", "input": {"command": "python -V needlecc"}}
            ]),
            false,
        ),
        line(
            SESSION,
            "u2",
            "2026-10-25T13:27:00Z",
            "user",
            json!([{"type": "tool_result", "tool_use_id": "t1", "content": "Python 3.13.17 needlecc"}]),
            false,
        ),
        // Injected record: isMeta, dropped by the projection.
        json!({
            "type": "user",
            "isMeta": true,
            "message": {"role": "user", "content": "needlecc meta record"},
            "timestamp": "2026-10-25T13:28:00Z",
            "sessionId": SESSION,
            "uuid": "m1"
        })
        .to_string(),
        // System record: ignored by type literal.
        json!({
            "type": "system",
            "subtype": "stop_hook_summary",
            "content": "needlecc system record",
            "timestamp": "2026-10-25T13:29:00Z",
            "sessionId": SESSION,
            "uuid": "s1"
        })
        .to_string(),
        // A sidechain record inside an interactive file.
        line(
            SESSION,
            "sc1",
            "2026-10-25T13:30:00Z",
            "user",
            json!("needlecc sidechain record"),
            true,
        ),
        // Malformed interior line.
        "{not json".to_string(),
        // Half-written final line (no trailing newline).
        line(
            SESSION,
            "u3",
            "2026-10-25T13:31:00Z",
            "user",
            json!("needlecc partial record"),
            false,
        ),
    ];
    let mut text = String::new();
    for (index, entry) in lines.iter().enumerate() {
        text.push_str(entry);
        if index + 1 < lines.len() {
            text.push('\n');
        }
    }
    std::fs::write(&main, text).unwrap();

    let sub_dir = project.join(SESSION).join("subagents");
    std::fs::create_dir_all(&sub_dir).unwrap();
    let subagent = sub_dir.join("agent-abc.jsonl");
    std::fs::write(
        &subagent,
        line(SESSION, "sc2", "2026-10-25T13:25:30Z", "user", json!("needlecc subagent echo"), true) + "\n",
    )
    .unwrap();

    Fixture {
        root,
        main,
        sidechain_record_file: subagent.clone(),
        subagent,
    }
}

fn search(config: &Path, request: Value) -> Value {
    let parsed = load_config(config).expect("config");
    let request = SearchRequest::parse(&request).expect("request");
    serde_json::to_value(memory_search(&parsed, &request)).unwrap()
}

fn read(config: &Path, request: Value) -> Value {
    let parsed = load_config(config).expect("config");
    let request = ReadRequest::parse(&request).expect("request");
    serde_json::to_value(memory_read(&parsed, &request)).unwrap()
}

#[test]
fn lexical_search_projects_text_tool_input_and_tool_result() {
    let env = support::TestEnv::new();
    let fixture = build(&env);
    let config = config(&env, &fixture.root);

    let response = search(&config, json!({"query": "needlecc", "limit": 20}));
    let results = response["results"].as_array().unwrap();
    // text (u1), assistant text + tool input (a1) and the tool result (u2).
    assert_eq!(results.len(), 4, "{response}");
    for hit in results {
        assert_eq!(hit["ref"]["schema_gen"], json!("claude-jsonl-v1"));
        assert_eq!(hit["source_kind"], json!("claude_code"));
        assert_eq!(hit["ref"]["rel_path"], json!(format!("proj/{SESSION}.jsonl")));
        assert!(hit["ref"]["locator"]["record_index"].is_number());
        assert!(hit["ref"]["locator"]["pointer"].as_str().unwrap().starts_with('/'));
    }
    // The tool result is its own unit under the tool_result pointer.
    assert!(results.iter().any(|hit| {
        hit["ref"]["locator"]["pointer"] == json!("/message/content/0")
            && hit["snippet"].as_str().unwrap().contains("Python 3.13.17")
    }));

    // Injected (`isMeta`), system and sidechain records never reach the lexical
    // path, and neither do subagent transcripts.
    let meta = search(&config, json!({"query": "meta record"}));
    assert!(meta["results"].as_array().unwrap().is_empty(), "{meta}");
    let system = search(&config, json!({"query": "system record"}));
    assert!(system["results"].as_array().unwrap().is_empty(), "{system}");
    let sidechain = search(&config, json!({"query": "sidechain record"}));
    assert!(sidechain["results"].as_array().unwrap().is_empty(), "{sidechain}");
    let subagent = search(&config, json!({"query": "subagent echo"}));
    assert!(subagent["results"].as_array().unwrap().is_empty(), "{subagent}");

    // The half-written final line is not a record yet.
    let partial = search(&config, json!({"query": "partial record"}));
    assert!(partial["results"].as_array().unwrap().is_empty(), "{partial}");
    assert_eq!(response["source_status"][0]["status"], json!("partial"));
    assert_eq!(
        response["source_status"][0]["detail"]["ignored_types"]["system"],
        json!(1)
    );
}

#[test]
fn session_id_filter_matches_the_filename_and_the_record() {
    let env = support::TestEnv::new();
    let fixture = build(&env);
    let config = config(&env, &fixture.root);

    let filtered = search(
        &config,
        json!({"query": "needlecc", "session_id": SESSION, "limit": 20}),
    );
    assert_eq!(filtered["results"].as_array().unwrap().len(), 4, "{filtered}");
    assert_eq!(
        filtered["source_status"][0]["detail"]["coverage"]["session_filter"],
        json!(SESSION)
    );

    let other = search(&config, json!({"query": "needlecc", "session_id": "no-such-session"}));
    assert!(other["results"].as_array().unwrap().is_empty(), "{other}");
    assert_eq!(other["source_status"][0]["status"], json!("empty"));
}

#[test]
fn refs_read_back_with_exact_range_and_context() {
    let env = support::TestEnv::new();
    let fixture = build(&env);
    let config = config(&env, &fixture.root);

    let response = search(&config, json!({"query": "interpreter.kb"}));
    let reference = response["results"][0]["ref"].clone();

    let single = read(&config, json!({"ref": reference.clone()}));
    assert_eq!(single["ok"], json!(true), "{single}");
    assert!(single["text"].as_str().unwrap().contains("interpreter.kb"));

    let contextual = read(
        &config,
        json!({"ref": reference.clone(), "context_before": 1, "context_after": 1}),
    );
    assert_eq!(contextual["ok"], json!(true), "{contextual}");
    assert!(contextual["segments"].as_array().unwrap().len() >= 2);
    assert!(contextual["text"].is_null(), "multi-segment omits the convenience keys");

    // A forged range is rejected (fail-closed), never widened. A shrunk span
    // no longer matches the projection coordinates...
    let mut forged = reference.clone();
    forged["text_range"]["byte_end"] = json!(1);
    let rejected = read(&config, json!({"ref": forged}));
    assert_eq!(rejected["ok"], json!(false));
    assert_eq!(rejected["code"], json!("REF_STALE"));

    // ...and a span past the projection is out of range.
    let mut out_of_range = reference.clone();
    out_of_range["text_range"]["byte_end"] = json!(10_000);
    let rejected = read(&config, json!({"ref": out_of_range}));
    assert_eq!(rejected["code"], json!("REF_OUT_OF_RANGE"));

    // A ref from inside the right root but with an unsafe rel_path fails closed.
    let mut escape = reference.clone();
    escape["rel_path"] = json!("../outside.jsonl");
    let rejected = read(&config, json!({"ref": escape}));
    assert_eq!(rejected["code"], json!("REF_LOCATOR_MISSING"));

    // A stale pointer is a locator mismatch.
    let mut pointer = reference;
    pointer["locator"]["pointer"] = json!("/message/content/9");
    let rejected = read(&config, json!({"ref": pointer}));
    assert_eq!(rejected["code"], json!("REF_LOCATOR_MISSING"));
}

#[test]
fn missing_root_is_unavailable_and_never_empty() {
    let env = support::TestEnv::new();
    let missing = env.path().join("no-claude-here");
    let config = config(&env, &missing);
    let response = search(&config, json!({"query": "needlecc"}));
    assert_eq!(response["source_status"][0]["status"], json!("unavailable"));
    assert_eq!(response["source_status"][0]["code"], json!("SOURCE_ROOT_INVALID"));
    assert_eq!(response["source_status"][0]["detail"]["reason"], json!("root_unreadable"));
}

#[test]
fn strict_snapshot_is_refused_for_claude_code() {
    let env = support::TestEnv::new();
    let fixture = build(&env);
    let config = config(&env, &fixture.root);
    let parsed = load_config(&config).unwrap();
    let request = SearchRequest::parse(&json!({"query": "needlecc"})).unwrap();
    let response = agent_memory::recall::search::memory_search_with(
        &parsed,
        &request,
        &agent_memory::recall::SearchOptions {
            strict_snapshot: true,
            eval_freeze_decay: false,
        },
    );
    let value = serde_json::to_value(&response).unwrap();
    assert_eq!(value["source_status"][0]["status"], json!("unavailable"));
    assert_eq!(
        value["source_status"][0]["detail"]["reason"],
        json!("strict_snapshot_unsupported_for_claude_code_root")
    );
    let _ = &fixture.main;
    let _ = &fixture.sidechain_record_file;
    let _ = &fixture.subagent;
}
