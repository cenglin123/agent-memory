//! OpenCode SQLite V2 side-table regressions, using only synthetic data.
//!
//! The fixture mirrors the shipped schema: `session_part` has no `id` column
//! and identifies a part by (message_id, session_id, ordinal, type).

mod support;

use rusqlite::{params, Connection};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::process::Command;
use support::{configured, TestEnv};

const QUESTION: &str = "Synthetic question";
const FIRST: &str = "partfirstneedle ";
const SECOND: &str = "partsecondneedle";

struct Fixture {
    db: PathBuf,
    config: PathBuf,
}

impl Fixture {
    fn new(env: &TestEnv, assistant: Value) -> Self {
        let root = env.root("opencode-session-parts");
        let db = root.join("opencode.db");
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(support::V2_SCHEMA).unwrap();
        conn.execute_batch(
            "CREATE TABLE session_part (
                 message_id TEXT NOT NULL, session_id TEXT NOT NULL,
                 ordinal INTEGER NOT NULL, type TEXT NOT NULL, call_id TEXT,
                 time_created INTEGER, time_updated INTEGER, data TEXT,
                 PRIMARY KEY (message_id, session_id, ordinal, type)
             );
             CREATE INDEX session_part_by_message ON session_part(message_id, ordinal);
             INSERT INTO session_v2 (id, title, directory, time_created, time_updated)
                 VALUES ('ses_parts', 'Synthetic session', 'C:/work', 1000, 3000);",
        )
        .unwrap();
        for (id, seq, role, data) in [
            ("sm-user", 1, "user", json!({"text": QUESTION})),
            ("sm-assistant", 2, "assistant", assistant),
        ] {
            conn.execute(
                "INSERT INTO session_message (id, session_id, seq, type, time_created, data)
                 VALUES (?1, 'ses_parts', ?2, ?3, 1000 + ?2, ?4)",
                params![id, seq, role, data.to_string()],
            )
            .unwrap();
        }
        // Insert backwards, with ordinals that also sort backwards. Include a
        // non-text control and a mismatched session to catch broad joins.
        for (session, ordinal, kind, text) in [
            ("ses_parts", 1, "text", SECOND),
            ("ses_parts", 0, "text", FIRST),
            ("ses_parts", 0, "reasoning", "hiddenneedle"),
            ("other-session", 0, "text", "foreignneedle"),
        ] {
            conn.execute(
                "INSERT INTO session_part
                     (message_id, session_id, ordinal, type, call_id, time_created, time_updated, data)
                 VALUES ('sm-assistant', ?1, ?2, ?3, NULL, 2000, 2000, ?4)",
                params![
                    session,
                    ordinal,
                    kind,
                    json!({"type": "text", "text": text}).to_string()
                ],
            )
            .unwrap();
        }
        drop(conn);
        let config = configured(env, "session-parts", &root).config;
        Self { db, config }
    }

    fn tail(&self) -> Vec<Value> {
        let reply = Command::new(env!("CARGO_BIN_EXE_agent-memory"))
            .args(["memory_recent", "--config"])
            .arg(&self.config)
            .args([
                "--request",
                r#"{"session_id":"ses_parts","tail_turns":2,"sources":["oc"]}"#,
            ])
            .output()
            .unwrap();
        assert!(reply.status.success(), "{:?}", reply);
        let envelope: Value = serde_json::from_slice(&reply.stdout).unwrap();
        assert_eq!(envelope["contract_version"], 2);
        envelope["sessions"][0]["tail"].as_array().unwrap().clone()
    }
}

#[test]
fn recent_restores_an_assistant_turn_in_ordinal_order() {
    let env = TestEnv::new();
    let fixture = Fixture::new(&env, json!({"content": []}));
    let tail = fixture.tail();
    assert_eq!(tail.len(), 2, "{tail:#?}");
    assert_eq!(tail[0]["role"], "user");
    assert_eq!(tail[0]["text"], QUESTION);
    assert_eq!(tail[1]["role"], "assistant");
    assert_eq!(tail[1]["text"], format!("{FIRST}\n\n{SECOND}"));

    let source = format!("sqlite:{}:ses_parts", fixture.db.display());
    let messages = agent_memory::providers::opencode::load_messages_sqlite(&source).unwrap();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[1].content, format!("{FIRST}\n\n{SECOND}"));
}

#[test]
fn inline_content_takes_precedence_over_side_table_text() {
    let env = TestEnv::new();
    let fixture = Fixture::new(
        &env,
        json!({"content": [{"type": "text", "text": "inline answer"}]}),
    );
    let tail = fixture.tail();
    assert_eq!(tail.len(), 2, "{tail:#?}");
    assert_eq!(tail[0]["text"], QUESTION);
    assert_eq!(tail[1]["text"], "inline answer");
    assert_eq!(tail[1]["ref"]["locator"]["pointer"], "/content/0");
    assert!(tail[1]["ref"]["locator"].get("part_id").is_none());
    assert!(tail[1].get("additional_refs").is_none());

    let source = format!("sqlite:{}:ses_parts", fixture.db.display());
    let messages = agent_memory::providers::opencode::load_messages_sqlite(&source).unwrap();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[1].content, "inline answer");
    let config = agent_memory::recall::config::load_config(&fixture.config).unwrap();
    let reference = agent_memory::recall::read::ReadRequest::parse(&json!({
        "ref": tail[1]["ref"],
    }))
    .unwrap();
    let reply = agent_memory::recall::read::memory_read(&config, &reference);
    assert_eq!(reply.text.as_deref(), Some("inline answer"), "{reply:?}");
}

#[test]
fn part_ref_reads_one_row_and_detects_raw_data_changes() {
    use agent_memory::recall::config::load_config;
    use agent_memory::recall::contract::codes;
    use agent_memory::recall::hash::sha256_hex;
    use agent_memory::recall::read::{memory_read, ReadRequest};

    let env = TestEnv::new();
    let fixture = Fixture::new(&env, json!({}));
    let tail = fixture.tail();
    let first = &tail[1]["ref"];
    let second = &tail[1]["additional_refs"][0];
    assert_eq!(tail[1]["additional_refs"].as_array().unwrap().len(), 1);
    let conn = Connection::open(&fixture.db).unwrap();
    let config = load_config(&fixture.config).unwrap();
    for (reference, ordinal, expected) in [(first, "0", FIRST), (second, "1", SECOND)] {
        assert_eq!(reference["schema_gen"], "sqlite-v2");
        assert_eq!(
            reference["locator"],
            json!({"session_id":"ses_parts", "session_message_id":"sm-assistant",
                   "part_id":ordinal, "pointer":"/text"})
        );
        let data: String = conn
            .query_row(
                "SELECT data FROM session_part WHERE message_id = 'sm-assistant' AND ordinal = ?1 AND type = 'text'",
                [ordinal.parse::<i64>().unwrap()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            reference["fingerprint"]["row_sha256"],
            sha256_hex(data.as_bytes())
        );
        let request = ReadRequest::parse(&json!({"ref": reference})).unwrap();
        let reply = memory_read(&config, &request);
        assert_eq!(reply.text.as_deref(), Some(expected), "{reply:?}");
    }

    let context =
        ReadRequest::parse(&json!({"ref": first, "context_before": 1, "context_after": 1}))
            .unwrap();
    let reply = memory_read(&config, &context);
    let texts: Vec<&str> = reply
        .segments
        .iter()
        .map(|segment| segment.text.as_str())
        .collect();
    assert_eq!(texts, [QUESTION, FIRST, SECOND]);

    let mut wrong_parent = first.clone();
    wrong_parent["locator"]["session_message_id"] = json!("sm-user");
    let wrong_parent = ReadRequest::parse(&json!({"ref": wrong_parent})).unwrap();
    assert_eq!(
        memory_read(&config, &wrong_parent).code,
        Some(codes::REF_LOCATOR_MISSING)
    );
    let mut unknown_key = first.clone();
    unknown_key["locator"]["extra"] = json!("unexpected");
    let error = ReadRequest::parse(&json!({"ref": unknown_key})).unwrap_err();
    assert_eq!(error.code, codes::INVALID_REQUEST);
    let mut wrong_pointer = first.clone();
    wrong_pointer["locator"]["pointer"] = json!("/content/0");
    let error = ReadRequest::parse(&json!({"ref": wrong_pointer})).unwrap_err();
    assert_eq!(error.code, codes::INVALID_REQUEST);
    let mut missing_hash = first.clone();
    missing_hash["fingerprint"]
        .as_object_mut()
        .unwrap()
        .remove("row_sha256");
    let error = ReadRequest::parse(&json!({"ref": missing_hash})).unwrap_err();
    assert_eq!(error.code, codes::INVALID_REQUEST);

    let request = ReadRequest::parse(&json!({"ref": first})).unwrap();
    for changed in [
        json!({"type":"text", "text": FIRST, "metadata":"changed"}).to_string(),
        json!({"type":"text", "text":"shorter"}).to_string(),
        "invalid JSON".to_string(),
    ] {
        conn.execute(
            "UPDATE session_part SET data = ?1
               WHERE message_id = 'sm-assistant' AND ordinal = 0 AND type = 'text'",
            [&changed],
        )
        .unwrap();
        assert_eq!(memory_read(&config, &request).code, Some(codes::REF_STALE));
    }
    let unaffected = ReadRequest::parse(&json!({"ref": second})).unwrap();
    assert_eq!(
        memory_read(&config, &unaffected).text.as_deref(),
        Some(SECOND)
    );
}

#[test]
fn session_part_units_survive_full_and_part_only_incremental_indexing() {
    use agent_memory::recall::config::load_config;
    use agent_memory::recall::fulltext::build::index_all;
    use agent_memory::recall::read::{memory_read, ReadRequest};
    use agent_memory::recall::search::{memory_search, SearchRequest};

    let env = TestEnv::new();
    let fixture = Fixture::new(&env, json!({"content": []}));
    let config = load_config(&env.write_json(
        "part-index.json",
        &json!({
            "version": 2,
            "sources": [env.opencode_source("oc", fixture.db.parent().unwrap())],
            "source_order": ["oc"],
            "cache_root": env.root("part-index-cache"),
            "interpreter": {"kb": {"command": "C:/python/python.exe"}},
            "domain": "default",
            "telemetry": {"enabled": false}
        }),
    ))
    .unwrap();
    let search = |query: &str, mode: &str| {
        let request = SearchRequest::parse(&json!({
            "query": query, "mode": mode, "sources": ["oc"], "limit": 10,
        }))
        .unwrap();
        memory_search(&config, &request).results
    };
    let check_hit = |query: &str, mode: &str, ordinal: &str, expected: &str| {
        let hits = search(query, mode);
        assert_eq!(hits.len(), 1, "{mode} {query}: {hits:?}");
        let reference = &hits[0].reference;
        assert_eq!(reference.locator.part_id.as_deref(), Some(ordinal));
        let request = ReadRequest::parse(&json!({"ref": reference})).unwrap();
        let read = memory_read(&config, &request);
        assert_eq!(read.text.as_deref(), Some(expected), "{read:?}");
        reference.clone()
    };

    let report = index_all(&config, None, false).unwrap();
    assert_eq!(report[0].status, "built", "{report:?}");
    assert_eq!(report[0].rows, 3);
    for mode in ["exact", "legacy"] {
        let first = check_hit("partfirstneedle", mode, "0", FIRST);
        let second = check_hit(SECOND, mode, "1", SECOND);
        let tail = fixture.tail();
        assert_eq!(serde_json::to_value(first).unwrap(), tail[1]["ref"]);
        assert_eq!(
            serde_json::to_value(second).unwrap(),
            tail[1]["additional_refs"][0]
        );
    }
    assert!(search("hiddenneedle", "exact").is_empty());
    assert!(search("foreignneedle", "exact").is_empty());

    // Only session_part changes; the parent rows and their timestamps do not.
    let conn = Connection::open(&fixture.db).unwrap();
    conn.execute_batch(
        r#"UPDATE session_part SET data = '{"type":"text","text":"parteditedneedle"}'
               WHERE message_id = 'sm-assistant' AND ordinal = 1 AND type = 'text';
           DELETE FROM session_part
               WHERE message_id = 'sm-assistant' AND ordinal = 0 AND type = 'text';
           INSERT INTO session_part
               (message_id, session_id, ordinal, type, call_id, time_created, time_updated, data)
               VALUES ('sm-assistant', 'ses_parts', 2, 'text', NULL, 2000, 2000,
                       '{"type":"text","text":"partaddedneedle"}');"#,
    )
    .unwrap();
    drop(conn);
    let report = index_all(&config, None, false).unwrap();
    assert_eq!(report[0].status, "updated", "{report:?}");
    assert!(!report[0].rebuilt);
    assert_eq!(
        report[0].detail.as_ref().unwrap()["opencode"]["mode"],
        "incremental"
    );
    assert_eq!(report[0].rows, 3);
    assert!(search("partfirstneedle", "exact").is_empty());
    assert!(search(SECOND, "exact").is_empty());
    check_hit("parteditedneedle", "exact", "1", "parteditedneedle");
    check_hit("partaddedneedle", "exact", "2", "partaddedneedle");

    // When an inline projection appears, the still-present side rows must
    // disappear from the index without retaining duplicate text units.
    let conn = Connection::open(&fixture.db).unwrap();
    conn.execute(
        r#"UPDATE session_message SET data = '{"content":[{"type":"text","text":"inlineindexedneedle"}]}'
           WHERE id = 'sm-assistant'"#,
        [],
    )
    .unwrap();
    drop(conn);
    let report = index_all(&config, None, false).unwrap();
    assert_eq!(report[0].status, "updated", "{report:?}");
    assert_eq!(report[0].rows, 2);
    assert!(search("parteditedneedle", "exact").is_empty());
    assert!(search("partaddedneedle", "exact").is_empty());
    let hits = search("inlineindexedneedle", "exact");
    assert_eq!(hits.len(), 1);
    assert!(hits[0].reference.locator.part_id.is_none());
}
