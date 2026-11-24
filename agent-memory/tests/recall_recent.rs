//! `memory_recent` (implementation-001): the live, index-free session hot
//! layer. Every fixture is synthetic and lives under the per-case temporary
//! root; no real host directory is touched.

mod support;

use agent_memory::recall::config::{load_config, RecallConfig};
use agent_memory::recall::contract::{
    codes, status, SESSION_KIND_INTERACTIVE, SESSION_KIND_WORKER, WARN_RECENT_TAIL_BUDGET,
};
use agent_memory::recall::fulltext::{
    claude_entrypoint_kind, claude_session_kind_with_entrypoint, CLAUDE_HEADLESS_ENTRYPOINTS,
    CLAUDE_INTERACTIVE_ENTRYPOINTS,
};
use agent_memory::recall::hash::sha256_hex;
use agent_memory::recall::read::{memory_read, ReadRequest};
use agent_memory::recall::recent::{
    memory_recent, memory_recent_with, RecentOptions, RecentRequest, RecentResponse,
    OPENCODE_TAIL_PLAN_SQL,
};
use rusqlite::Connection;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

const T0: i64 = 1_760_000_000_000;

// Session ids used by every assertion.
const OC_MAIN: &str = "oc-main";
const OC_CHILD: &str = "oc-child";
const OC_OCSR: &str = "oc-ocsr";
const OC_OLD: &str = "oc-old";
const CC_MAIN: &str = "11111111-1111-4111-8111-111111111111";
const CC_HEADLESS: &str = "22222222-2222-4222-8222-222222222222";
const CC_SIDECHAIN: &str = "33333333-3333-4333-8333-333333333333";
const CC_HALF: &str = "44444444-4444-4444-8444-444444444444";
const CX_MAIN: &str = "55555555-5555-4555-8555-555555555555";
const CX_EXEC: &str = "66666666-6666-4666-8666-666666666666";
const CX_VSCODE: &str = "77777777-7777-4777-8777-777777777777";

const CJK_TAIL: &str = "中文字符截断测试用例";

struct Fixture {
    root: PathBuf,
    config: PathBuf,
    opencode: PathBuf,
    claude: PathBuf,
    codex: PathBuf,
}

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_agent-memory")
}

fn set_mtime(path: &Path, seconds: u64) {
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .expect("open for mtime");
    file.set_modified(std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(seconds))
        .expect("set mtime");
}

// ---------------------------------------------------------------------------
// OpenCode V2 fixture
// ---------------------------------------------------------------------------

fn build_opencode(root: &Path) -> PathBuf {
    std::fs::create_dir_all(root).unwrap();
    let db = root.join("opencode.db");
    let conn = Connection::open(&db).unwrap();
    conn.execute_batch(support::V2_SCHEMA).unwrap();

    let mut sessions: Vec<(&str, Option<&str>, &str, i64, &str)> = vec![
        (OC_MAIN, None, "主会话", T0 + 300_000, "C:/work/main"),
        (
            OC_CHILD,
            Some(OC_MAIN),
            "child session",
            T0 + 250_000,
            "C:/work/child",
        ),
        (OC_OCSR, None, "ocsr-dispatch", T0 + 200_000, "C:/work/ocsr"),
        (OC_OLD, None, "old session", T0 + 10_000, "C:/work/old"),
    ];
    sessions.sort_by_key(|entry| entry.0);
    for (id, parent, title, updated, directory) in sessions {
        conn.execute(
            "INSERT INTO session_v2 (id, title, directory, time_created, time_updated, parent_id) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![id, title, directory, T0, updated, parent],
        )
        .unwrap();
    }

    let insert = |session: &str, message: &str, seq: i64, kind: &str, created: i64, data: Value| {
        conn.execute(
            "INSERT INTO session_message (id, session_id, seq, type, time_created, data) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![message, session, seq, kind, created, data.to_string()],
        )
        .unwrap();
    };

    // A large message table: the tail query must still touch only the indexed
    // `(session_id, seq)` range, never scan this table.
    conn.execute_batch("BEGIN;").unwrap();
    for index in 1..=2_000i64 {
        insert(
            OC_MAIN,
            &format!("oc-filler-{index}"),
            index,
            if index % 2 == 0 { "user" } else { "assistant" },
            T0 + index,
            json!({"content": [{"type": "text", "text": format!("filler message {index}")}]}),
        );
    }
    conn.execute_batch("COMMIT;").unwrap();

    insert(
        OC_MAIN,
        "oc-m1",
        2_001,
        "user",
        T0 + 200_001,
        json!({"content": [{"type": "text", "text": "第一轮问题 alpha"}]}),
    );
    insert(
        OC_MAIN,
        "oc-m2",
        2_002,
        "assistant",
        T0 + 200_002,
        json!({"content": [{"type": "text", "text": "第一轮回答 beta"}]}),
    );
    insert(
        OC_MAIN,
        "oc-m3",
        2_003,
        "user",
        T0 + 200_003,
        json!({"content": [{"type": "text", "text": "第二轮问题 ocrecentneedle"}]}),
    );
    insert(
        OC_MAIN,
        "oc-m4",
        2_004,
        "assistant",
        T0 + 200_004,
        json!({"content": [{"type": "text", "text": "第二轮回答 gamma"}]}),
    );
    insert(
        OC_MAIN,
        "oc-m5",
        2_005,
        "assistant",
        T0 + 200_005,
        json!({"content": [{
            "type": "tool",
            "name": "bash",
            "state": {"input": {"command": "echo hi"}, "output": "octooloutputneedle"}
        }]}),
    );
    insert(
        OC_MAIN,
        "oc-m6",
        2_006,
        "user",
        T0 + 200_006,
        json!({"content": [{"type": "text", "text": CJK_TAIL}]}),
    );
    insert(
        OC_CHILD,
        "oc-child-1",
        1,
        "user",
        T0 + 250_001,
        json!({"content": [{"type": "text", "text": "occhildneedle"}]}),
    );
    insert(
        OC_OCSR,
        "oc-ocsr-1",
        1,
        "user",
        T0 + 200_101,
        json!({"content": [{"type": "text", "text": "ococsrneedle"}]}),
    );
    insert(
        OC_OLD,
        "oc-old-1",
        1,
        "user",
        T0 + 10_001,
        json!({"content": [{"type": "text", "text": "ocoldneedle"}]}),
    );
    drop(conn);
    root.to_path_buf()
}

// ---------------------------------------------------------------------------
// Claude Code fixture
// ---------------------------------------------------------------------------

fn claude_line(
    session: &str,
    uuid: &str,
    timestamp: &str,
    kind: &str,
    content: Value,
    sidechain: bool,
    entrypoint: Option<&str>,
    cwd: &str,
) -> String {
    let mut value = json!({
        "type": kind,
        "message": {"role": kind, "content": content},
        "timestamp": timestamp,
        "sessionId": session,
        "uuid": uuid,
        "isSidechain": sidechain,
        "cwd": cwd,
    });
    if let Some(entrypoint) = entrypoint {
        value["entrypoint"] = json!(entrypoint);
    }
    value.to_string()
}

fn rfc(ms: i64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ms)
        .unwrap()
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn write_lines(path: &Path, lines: &[String]) {
    let mut text = String::new();
    for line in lines {
        text.push_str(line);
        text.push('\n');
    }
    std::fs::write(path, text).unwrap();
}

fn build_claude(root: &Path) -> PathBuf {
    std::fs::create_dir_all(root).unwrap();
    let alpha = root.join("proj-alpha");
    let beta = root.join("proj-beta");
    std::fs::create_dir_all(&alpha).unwrap();
    std::fs::create_dir_all(&beta).unwrap();

    let main_path = alpha.join(format!("{CC_MAIN}.jsonl"));
    write_lines(
        &main_path,
        &[
            claude_line(
                CC_MAIN,
                "cc-u1",
                &rfc(T0 + 400_001),
                "user",
                json!("claude main needle ccneedle"),
                false,
                Some("cli"),
                "C:/work/alpha",
            ),
            claude_line(
                CC_MAIN,
                "cc-a1",
                &rfc(T0 + 400_002),
                "assistant",
                json!([{"type": "text", "text": "claude answer ccneedle"}]),
                false,
                Some("cli"),
                "C:/work/alpha",
            ),
            claude_line(
                CC_MAIN,
                "cc-u2",
                &rfc(T0 + 400_003),
                "user",
                json!("claude latest cclatestneedle"),
                false,
                Some("cli"),
                "C:/work/alpha",
            ),
        ],
    );
    // Deliberately older mtime than the worker transcripts: the record clock,
    // not the file mtime, decides the final order.
    set_mtime(&main_path, 1_700_000_000);

    let headless_path = beta.join(format!("{CC_HEADLESS}.jsonl"));
    write_lines(
        &headless_path,
        &[
            claude_line(
                CC_HEADLESS,
                "hl-u1",
                &rfc(T0 + 350_001),
                "user",
                json!("headless worker ccheadlessneedle"),
                false,
                Some("sdk-cli"),
                "C:/work/beta",
            ),
            claude_line(
                CC_HEADLESS,
                "hl-a1",
                &rfc(T0 + 350_002),
                "assistant",
                json!("headless answer"),
                false,
                Some("sdk-cli"),
                "C:/work/beta",
            ),
        ],
    );
    set_mtime(&headless_path, 1_700_000_200);

    let sidechain_path = alpha.join(format!("{CC_SIDECHAIN}.jsonl"));
    write_lines(
        &sidechain_path,
        &[claude_line(
            CC_SIDECHAIN,
            "sc-u1",
            &rfc(T0 + 300_001),
            "user",
            json!("sidechain worker ccsidechainneedle"),
            true,
            Some("cli"),
            "C:/work/alpha",
        )],
    );
    set_mtime(&sidechain_path, 1_700_000_300);

    let half_path = beta.join(format!("{CC_HALF}.jsonl"));
    write_lines(
        &half_path,
        &[claude_line(
            CC_HALF,
            "hf-u1",
            &rfc(T0 + 100_001),
            "user",
            json!("half complete cchalfneedle"),
            false,
            Some("cli"),
            "C:/work/beta",
        )],
    );
    let mut bytes = std::fs::read(&half_path).unwrap();
    bytes.extend_from_slice(b"{\"type\":\"user\",\"message\":{\"content\":\"halfwrittenneedle\"}");
    std::fs::write(&half_path, bytes).unwrap();
    set_mtime(&half_path, 1_700_000_400);

    // A subagent transcript is not a session candidate at all.
    let subagents = alpha.join(CC_MAIN).join("subagents");
    std::fs::create_dir_all(&subagents).unwrap();
    write_lines(
        &subagents.join("agent-worker.jsonl"),
        &[claude_line(
            CC_MAIN,
            "sa-u1",
            &rfc(T0 + 900_000),
            "user",
            json!("subagentneedle"),
            true,
            Some("sdk-cli"),
            "C:/work/alpha",
        )],
    );
    set_mtime(&subagents.join("agent-worker.jsonl"), 1_700_000_900);
    root.to_path_buf()
}

// ---------------------------------------------------------------------------
// Codex fixture
// ---------------------------------------------------------------------------

fn codex_line(session: &str, cwd: &str, time: &str, value: Value) -> String {
    let mut object = value;
    object["timestamp"] = json!(time);
    if object.get("type").and_then(Value::as_str) == Some("session_meta") {
        object["payload"]["id"] = json!(session);
        object["payload"]["cwd"] = json!(cwd);
    }
    object.to_string()
}

fn codex_message(role: &str, text: &str) -> Value {
    json!({"type": "response_item", "payload": {"type": "message", "role": role, "content": text}})
}

fn write_rollout(home: &Path, session: &str, lines: &[String]) -> PathBuf {
    let dir = home.join("sessions").join("2026").join("01").join("01");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("rollout-2026-01-01T00-00-00-{session}.jsonl"));
    write_lines(&path, lines);
    path
}

fn build_codex(root: &Path) -> PathBuf {
    std::fs::create_dir_all(root).unwrap();
    let main_path = write_rollout(
        root,
        CX_MAIN,
        &[
            codex_line(
                CX_MAIN,
                "C:/work/codex",
                &rfc(T0 + 450_001),
                json!({"type": "session_meta", "payload": {"source": "cli"}}),
            ),
            codex_line(
                CX_MAIN,
                "C:/work/codex",
                &rfc(T0 + 450_002),
                codex_message("user", "codex main cxneedle"),
            ),
            codex_line(
                CX_MAIN,
                "C:/work/codex",
                &rfc(T0 + 450_003),
                codex_message("assistant", "codex answer"),
            ),
            codex_line(
                CX_MAIN,
                "C:/work/codex",
                &rfc(T0 + 450_004),
                codex_message("user", "codex latest cxlatestneedle"),
            ),
        ],
    );
    set_mtime(&main_path, 1_700_001_000);

    let exec_path = write_rollout(
        root,
        CX_EXEC,
        &[
            codex_line(
                CX_EXEC,
                "C:/work/exec",
                &rfc(T0 + 500_001),
                json!({"type": "session_meta", "payload": {"source": "exec"}}),
            ),
            codex_line(
                CX_EXEC,
                "C:/work/exec",
                &rfc(T0 + 500_002),
                codex_message("user", "codex exec worker cxexecneedle"),
            ),
        ],
    );
    set_mtime(&exec_path, 1_700_001_100);

    let vscode_path = write_rollout(
        root,
        CX_VSCODE,
        &[
            codex_line(
                CX_VSCODE,
                "C:/work/vscode",
                &rfc(T0 + 50_001),
                json!({"type": "session_meta", "payload": {"source": "vscode"}}),
            ),
            codex_line(
                CX_VSCODE,
                "C:/work/vscode",
                &rfc(T0 + 50_002),
                codex_message("user", "codex vscode cxvscodeneedle"),
            ),
        ],
    );
    set_mtime(&vscode_path, 1_700_000_500);

    let db = root.join("state_5.sqlite");
    let conn = Connection::open(&db).unwrap();
    conn.execute_batch(
        "CREATE TABLE threads (
            id TEXT PRIMARY KEY,
            title TEXT NOT NULL,
            first_user_message TEXT,
            cwd TEXT,
            rollout_path TEXT,
            updated_at INTEGER
        );",
    )
    .unwrap();
    let seconds = |ms: i64| ms / 1_000;
    for (id, title, path, updated) in [
        (CX_MAIN, "codex main", &main_path, T0 + 450_000),
        (CX_EXEC, "codex exec", &exec_path, T0 + 500_000),
        (CX_VSCODE, "codex vscode", &vscode_path, T0 + 50_000),
    ] {
        conn.execute(
            "INSERT INTO threads (id, title, first_user_message, cwd, rollout_path, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                id,
                title,
                title,
                "C:/work/codex",
                path.to_string_lossy(),
                seconds(updated)
            ],
        )
        .unwrap();
    }
    drop(conn);
    root.to_path_buf()
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

fn build(env: &support::TestEnv) -> Fixture {
    let root = env.path().to_path_buf();
    let opencode = build_opencode(&root.join("oc"));
    let claude = build_claude(&root.join("cc"));
    let codex = build_codex(&root.join("cx"));
    let config = env.write_json(
        "recent-config.json",
        &json!({
            "version": 1,
            "sources": [
                {"source_id": "oc", "kind": "opencode", "root": opencode.to_string_lossy(), "enabled": true},
                {"source_id": "cx", "kind": "codex", "root": codex.to_string_lossy(), "enabled": true},
                {"source_id": "cc", "kind": "claude_code", "root": claude.to_string_lossy(), "enabled": true}
            ],
            "source_order": ["oc", "cx", "cc"],
            "interpreter": {"kb": {"command": "C:/python/python.exe"}}
        }),
    );
    Fixture {
        root,
        config,
        opencode,
        claude,
        codex,
    }
}

fn load(fixture: &Fixture) -> RecallConfig {
    load_config(&fixture.config).expect("recent config")
}

fn recent(config: &RecallConfig, request: Value) -> RecentResponse {
    let request = RecentRequest::parse(&request).expect("recent request");
    memory_recent(config, &request)
}

fn recent_with_offset(
    config: &RecallConfig,
    request: Value,
    utc_offset_minutes: Option<i32>,
) -> RecentResponse {
    let request = RecentRequest::parse(&request).expect("recent request");
    memory_recent_with(
        config,
        &request,
        &RecentOptions {
            utc_offset_minutes,
            ..RecentOptions::default()
        },
    )
}

fn local_rfc(ms: i64, offset_minutes: i32) -> String {
    let offset = chrono::FixedOffset::east_opt(offset_minutes * 60).expect("offset");
    chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ms)
        .expect("stamp")
        .with_timezone(&offset)
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, false)
}

fn epoch(rfc3339: &str) -> u64 {
    chrono::DateTime::parse_from_rfc3339(rfc3339)
        .expect("fixture timestamp")
        .timestamp() as u64
}

fn read_back(config: &RecallConfig, reference: &Value) -> agent_memory::recall::read::ReadResponse {
    let request = ReadRequest::parse(&json!({"ref": reference})).expect("read request");
    memory_read(config, &request)
}

fn status_of<'a>(response: &'a RecentResponse, source_id: &str) -> &'a str {
    response
        .source_status
        .iter()
        .find(|entry| entry.source_id == source_id)
        .map(|entry| entry.status)
        .expect("source status")
}

fn ids(response: &RecentResponse) -> Vec<String> {
    response
        .sessions
        .iter()
        .map(|session| format!("{}:{}", session.source, session.session_id))
        .collect()
}

// ---------------------------------------------------------------------------
// R1: realtime
// ---------------------------------------------------------------------------

#[test]
fn an_appended_turn_is_visible_without_any_index() {
    let env = support::TestEnv::new();
    let fixture = build(&env);
    assert!(
        !fixture.root.join("cache").join("live").exists(),
        "the fixture has no full-text index at all"
    );
    let config = load(&fixture);
    assert!(config.cache_root.is_none());

    for (source, needle, request) in [
        (
            "oc",
            "ocappendedneedle",
            json!({"sources": ["oc"], "tail_turns": 1}),
        ),
        (
            "cx",
            "cxappendedneedle",
            json!({"sources": ["cx"], "tail_turns": 1}),
        ),
        (
            "cc",
            "ccappendedneedle",
            json!({"sources": ["cc"], "tail_turns": 1}),
        ),
    ] {
        let before = recent(&config, request.clone());
        assert_eq!(status_of(&before, source), status::OK);
        assert!(
            before
                .sessions
                .iter()
                .all(|session| session.tail.iter().all(|turn| !turn.text.contains(needle))),
            "{source}: new turn must not exist yet"
        );
    }

    // Append one turn to each live carrier, exactly as the host would.
    let conn = Connection::open(fixture.opencode.join("opencode.db")).unwrap();
    conn.execute(
        "INSERT INTO session_message (id, session_id, seq, type, time_created, data) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![
            "oc-appended",
            OC_MAIN,
            2_007i64,
            "user",
            T0 + 600_001,
            json!({"content": [{"type": "text", "text": "ocappendedneedle"}]}).to_string()
        ],
    )
    .unwrap();
    conn.execute(
        "UPDATE session_v2 SET time_updated = ?1 WHERE id = ?2",
        rusqlite::params![T0 + 600_000, OC_MAIN],
    )
    .unwrap();
    drop(conn);

    let cc_main = fixture
        .claude
        .join("proj-alpha")
        .join(format!("{CC_MAIN}.jsonl"));
    let mut cc_bytes = std::fs::read(&cc_main).unwrap();
    cc_bytes.extend_from_slice(
        format!(
            "{}\n",
            claude_line(
                CC_MAIN,
                "cc-appended",
                &rfc(T0 + 700_001),
                "user",
                json!("ccappendedneedle"),
                false,
                Some("cli"),
                "C:/work/alpha",
            )
        )
        .as_bytes(),
    );
    std::fs::write(&cc_main, cc_bytes).unwrap();

    let cx_main = fixture
        .codex
        .join("sessions")
        .join("2026")
        .join("01")
        .join("01")
        .join(format!("rollout-2026-01-01T00-00-00-{CX_MAIN}.jsonl"));
    let mut cx_bytes = std::fs::read(&cx_main).unwrap();
    cx_bytes.extend_from_slice(
        format!(
            "{}\n",
            codex_line(
                CX_MAIN,
                "C:/work/codex",
                &rfc(T0 + 800_001),
                codex_message("user", "cxappendedneedle"),
            )
        )
        .as_bytes(),
    );
    std::fs::write(&cx_main, cx_bytes).unwrap();
    let conn = Connection::open(fixture.codex.join("state_5.sqlite")).unwrap();
    conn.execute(
        "UPDATE threads SET updated_at = ?1 WHERE id = ?2",
        rusqlite::params![(T0 + 800_000) / 1_000, CX_MAIN],
    )
    .unwrap();
    drop(conn);

    for (source, needle, request) in [
        (
            "oc",
            "ocappendedneedle",
            json!({"sources": ["oc"], "tail_turns": 1}),
        ),
        (
            "cx",
            "cxappendedneedle",
            json!({"sources": ["cx"], "tail_turns": 1}),
        ),
        (
            "cc",
            "ccappendedneedle",
            json!({"sources": ["cc"], "tail_turns": 1}),
        ),
    ] {
        let after = recent(&config, request);
        assert_eq!(status_of(&after, source), status::OK);
        let first = after.sessions.first().expect("the appended session");
        let expected_id = match source {
            "oc" => OC_MAIN,
            "cx" => CX_MAIN,
            _ => CC_MAIN,
        };
        assert_eq!(first.session_id, expected_id);
        let turn = first.tail.last().expect("appended tail turn");
        assert!(
            turn.text.contains(needle),
            "{source}: appended turn must be visible immediately: {:?}",
            turn.text
        );
    }
}

// ---------------------------------------------------------------------------
// R2: ordering and filters
// ---------------------------------------------------------------------------

#[test]
fn sessions_order_by_last_activity_across_sources() {
    let env = support::TestEnv::new();
    let fixture = build(&env);
    let config = load(&fixture);

    let response = recent(&config, json!({"limit": 10}));
    assert_eq!(status_of(&response, "oc"), status::OK);
    assert_eq!(status_of(&response, "cx"), status::OK);
    assert_eq!(status_of(&response, "cc"), status::OK);
    assert_eq!(
        ids(&response),
        vec![
            format!("cx:{CX_MAIN}"),
            format!("cc:{CC_MAIN}"),
            format!("oc:{OC_MAIN}"),
            format!("cc:{CC_HALF}"),
            format!("cx:{CX_VSCODE}"),
            format!("oc:{OC_OLD}"),
        ],
        "worker sessions are excluded and the rest is newest first"
    );

    let limited = recent(&config, json!({"limit": 3}));
    assert_eq!(limited.sessions.len(), 3);
    assert_eq!(
        ids(&limited),
        vec![
            format!("cx:{CX_MAIN}"),
            format!("cc:{CC_MAIN}"),
            format!("oc:{OC_MAIN}")
        ]
    );
}

#[test]
fn since_exclude_and_session_kinds_are_independent_filters() {
    let env = support::TestEnv::new();
    let fixture = build(&env);
    let config = load(&fixture);

    let since = recent(&config, json!({"limit": 20, "since": rfc(T0 + 250_000)}));
    assert_eq!(
        ids(&since),
        vec![
            format!("cx:{CX_MAIN}"),
            format!("cc:{CC_MAIN}"),
            format!("oc:{OC_MAIN}")
        ],
        "everything older than the threshold is gone"
    );

    let excluded = recent(
        &config,
        json!({"limit": 20, "exclude_session_ids": [OC_MAIN, CC_MAIN]}),
    );
    assert!(!excluded
        .sessions
        .iter()
        .any(|session| session.session_id == OC_MAIN || session.session_id == CC_MAIN));
    assert!(excluded
        .sessions
        .iter()
        .any(|session| session.session_id == CX_MAIN));

    let workers = recent(&config, json!({"limit": 20, "session_kinds": ["worker"]}));
    let mut worker_ids: Vec<String> = workers
        .sessions
        .iter()
        .map(|session| session.session_id.to_string())
        .collect();
    worker_ids.sort();
    let mut expected = vec![
        OC_CHILD.to_string(),
        OC_OCSR.to_string(),
        CC_HEADLESS.to_string(),
        CC_SIDECHAIN.to_string(),
        CX_EXEC.to_string(),
    ];
    expected.sort();
    assert_eq!(worker_ids, expected);
    assert!(workers
        .sessions
        .iter()
        .all(|session| session.session_kind == SESSION_KIND_WORKER));

    let unknown = recent(&config, json!({"limit": 20, "session_kinds": ["unknown"]}));
    assert!(unknown.sessions.is_empty());
}

#[test]
fn workers_are_excluded_by_default_in_all_three_sources() {
    let env = support::TestEnv::new();
    let fixture = build(&env);
    let config = load(&fixture);

    let response = recent(&config, json!({"limit": 20}));
    assert!(response
        .sessions
        .iter()
        .all(|session| session.session_kind == SESSION_KIND_INTERACTIVE));
    for worker in [OC_CHILD, OC_OCSR, CC_HEADLESS, CC_SIDECHAIN, CX_EXEC] {
        assert!(
            !response
                .sessions
                .iter()
                .any(|session| session.session_id == worker),
            "{worker} must not be listed by default"
        );
    }
    // The subagent transcript is not a candidate at all, in any mode.
    let workers = recent(&config, json!({"limit": 20, "session_kinds": ["worker"]}));
    assert!(!workers
        .sessions
        .iter()
        .any(|session| session.title.as_deref() == Some("agent-worker")));
}

#[test]
fn headless_entrypoint_rule_is_named_and_conservative() {
    // Synthetic records express the rule: SDK/print entry points are workers,
    // `cli` is interactive and an unknown value keeps the old classification.
    assert_eq!(SESSION_KIND_WORKER, "worker");
    assert_eq!(
        claude_session_kind_with_entrypoint("p/s.jsonl", false, Some("cli")),
        SESSION_KIND_INTERACTIVE
    );
    assert_eq!(
        claude_session_kind_with_entrypoint("p/s.jsonl", false, Some("sdk-cli")),
        SESSION_KIND_WORKER
    );
    assert_eq!(
        claude_session_kind_with_entrypoint("p/s.jsonl", false, Some("sdk-ts")),
        SESSION_KIND_WORKER
    );
    assert_eq!(
        claude_session_kind_with_entrypoint("p/s.jsonl", false, Some("print")),
        SESSION_KIND_WORKER
    );
    assert_eq!(
        claude_session_kind_with_entrypoint("p/s.jsonl", false, None),
        SESSION_KIND_INTERACTIVE,
        "no marker keeps the pre-existing rule"
    );
    assert_eq!(
        claude_session_kind_with_entrypoint("p/s.jsonl", false, Some("some-other-client")),
        SESSION_KIND_INTERACTIVE,
        "an unreliable value never guesses a worker"
    );
    assert_eq!(
        claude_session_kind_with_entrypoint("p/s/subagents/a.jsonl", false, Some("cli")),
        SESSION_KIND_WORKER
    );
    for marker in CLAUDE_HEADLESS_ENTRYPOINTS {
        assert_eq!(
            claude_entrypoint_kind(Some(marker)),
            Some(SESSION_KIND_WORKER)
        );
    }
    for marker in CLAUDE_INTERACTIVE_ENTRYPOINTS {
        assert_eq!(
            claude_entrypoint_kind(Some(marker)),
            Some(SESSION_KIND_INTERACTIVE)
        );
    }
}

// ---------------------------------------------------------------------------
// R3: tail construction
// ---------------------------------------------------------------------------

#[test]
fn tail_turns_roles_truncation_budget_and_half_lines() {
    let env = support::TestEnv::new();
    let fixture = build(&env);
    let config = load(&fixture);

    let two = recent(&config, json!({"sources": ["oc"], "tail_turns": 2}));
    let main = &two.sessions[0];
    assert_eq!(main.session_id, OC_MAIN);
    let texts: Vec<&str> = main.tail.iter().map(|turn| turn.text.as_str()).collect();
    assert_eq!(
        texts,
        vec!["第二轮回答 gamma", CJK_TAIL],
        "the newest two text turns, oldest first"
    );
    assert_eq!(
        main.tail.iter().map(|turn| turn.role).collect::<Vec<_>>(),
        vec!["assistant", "user"]
    );
    assert!(main.tail.iter().all(|turn| !turn.truncated));

    let truncated = recent(
        &config,
        json!({"sources": ["oc"], "tail_turns": 1, "max_chars_per_turn": 5}),
    );
    let turn = &truncated.sessions[0].tail[0];
    assert_eq!(turn.text, "中文字符截");
    assert_eq!(turn.text.chars().count(), 5);
    assert!(turn.truncated);

    let tools = recent(
        &config,
        json!({"sources": ["oc"], "tail_turns": 2, "tail_roles": ["tool"]}),
    );
    let main = &tools.sessions[0];
    assert_eq!(main.tail.len(), 2, "tool input + tool output");
    assert!(main.tail.iter().all(|turn| turn.role == "tool"));
    assert!(main
        .tail
        .iter()
        .any(|turn| turn.text.contains("octooloutputneedle")));

    let none = recent(&config, json!({"sources": ["oc"]}));
    assert!(none.sessions[0].tail.is_empty(), "tail_turns defaults to 0");

    // A half-written final line is not a record and is not an error.
    let half = recent(&config, json!({"sources": ["cc"], "tail_turns": 3}));
    assert_eq!(status_of(&half, "cc"), status::OK);
    assert!(!half.sessions.iter().any(|session| session
        .tail
        .iter()
        .any(|turn| turn.text.contains("halfwrittenneedle"))));

    // Budget: only the newest turn survives, the oldest is dropped and named.
    let budget = recent(
        &config,
        json!({
            "sources": ["oc"],
            "tail_turns": 2,
            "max_bytes": 40
        }),
    );
    let main = &budget.sessions[0];
    assert_eq!(main.tail.len(), 1, "the oldest turn is dropped first");
    assert_eq!(main.tail[0].text, CJK_TAIL);
    let warning = budget
        .warnings
        .iter()
        .find(|entry| entry.code == WARN_RECENT_TAIL_BUDGET)
        .expect("budget warning");
    assert_eq!(warning.detail.as_ref().unwrap()["dropped_turns"], json!(1));
}

// ---------------------------------------------------------------------------
// R4: every tail ref reads back through memory_read
// ---------------------------------------------------------------------------

#[test]
fn every_tail_ref_reads_back_through_memory_read() {
    let env = support::TestEnv::new();
    let fixture = build(&env);
    let config = load(&fixture);

    let response = recent(
        &config,
        json!({"limit": 20, "tail_turns": 2, "tail_roles": ["user", "assistant", "tool"]}),
    );
    assert!(!response.sessions.is_empty());
    for session in &response.sessions {
        for turn in &session.tail {
            let reference = serde_json::to_value(&turn.reference).unwrap();
            let read = read_back(&config, &reference);
            assert!(
                read.ok,
                "{}:{} {:?} must read back: {:?}",
                session.source, session.session_id, turn.reference.locator, read.code
            );
            let text = read.text.as_deref().unwrap_or_default();
            assert!(
                text.starts_with(&turn.text) || text.len() >= turn.text.len(),
                "readback returns the whole projection"
            );
            assert!(!text.is_empty());
        }
    }

    // The truncated turn still reads back in full.
    let truncated = recent(
        &config,
        json!({"sources": ["oc"], "tail_turns": 1, "max_chars_per_turn": 3}),
    );
    let turn = &truncated.sessions[0].tail[0];
    assert_eq!(turn.text.chars().count(), 3);
    let read = read_back(&config, &serde_json::to_value(&turn.reference).unwrap());
    assert!(read.ok);
    assert_eq!(read.text.as_deref(), Some(CJK_TAIL));
}

// ---------------------------------------------------------------------------
// R5: bounded OpenCode query
// ---------------------------------------------------------------------------

#[test]
fn opencode_tail_query_uses_the_session_seq_index() {
    let env = support::TestEnv::new();
    let fixture = build(&env);
    let conn = Connection::open(fixture.opencode.join("opencode.db")).unwrap();
    let mut statement = conn.prepare(OPENCODE_TAIL_PLAN_SQL).unwrap();
    let rows: Vec<String> = statement
        .query_map(
            rusqlite::params![OC_MAIN, 64i64 * 1024 * 1024, 200i64],
            |row| row.get::<_, String>(3),
        )
        .unwrap()
        .map(|row| row.unwrap())
        .collect();
    let plan = rows.join(" | ");
    assert!(
        plan.contains("USING INDEX"),
        "the tail query must use an index: {plan}"
    );
    assert!(
        !plan.contains("SCAN session_message"),
        "the tail query must not scan the message table: {plan}"
    );

    // A loose regression bound on the live path: 2 000 filler messages must
    // not turn into a per-call table read.
    let config = load(&fixture);
    let started = std::time::Instant::now();
    let response = recent(&config, json!({"sources": ["oc"], "tail_turns": 2}));
    let elapsed = started.elapsed();
    assert_eq!(response.sessions.len(), 2);
    assert!(
        elapsed < std::time::Duration::from_secs(5),
        "a bounded tail query must stay fast, took {elapsed:?}"
    );

    // A schema without the `(session_id, seq)` index is disclosed instead of
    // silently claiming the same bound.
    let unindexed = env.root("oc-unindexed");
    let db = unindexed.join("opencode.db");
    let conn = Connection::open(&db).unwrap();
    conn.execute_batch(
        "CREATE TABLE session_v2 (id TEXT PRIMARY KEY, title TEXT, directory TEXT, \
         time_created INTEGER, time_updated INTEGER, parent_id TEXT);\n\
         CREATE TABLE session_message (id TEXT PRIMARY KEY, session_id TEXT, seq INTEGER, \
         type TEXT, time_created INTEGER, data TEXT);",
    )
    .unwrap();
    conn.execute(
        "INSERT INTO session_v2 (id, title, directory, time_created, time_updated, parent_id) \
         VALUES ('ocu-main', 'unindexed', 'C:/work', 1, 2, NULL)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO session_message (id, session_id, seq, type, time_created, data) \
         VALUES ('ocu-m1', 'ocu-main', 1, 'user', 2, ?1)",
        [json!({"content": [{"type": "text", "text": "unindexedneedle"}]}).to_string()],
    )
    .unwrap();
    drop(conn);
    let unindexed_config = env.write_json(
        "unindexed-config.json",
        &json!({
            "version": 1,
            "sources": [{"source_id": "ocu", "kind": "opencode", "root": unindexed.to_string_lossy(), "enabled": true}],
            "source_order": ["ocu"],
            "interpreter": {"kb": {"command": "C:/python/python.exe"}}
        }),
    );
    let unindexed_config = load_config(&unindexed_config).unwrap();
    let disclosed = recent(
        &unindexed_config,
        json!({"sources": ["ocu"], "tail_turns": 1}),
    );
    assert_eq!(status_of(&disclosed, "ocu"), status::OK);
    let detail = disclosed.source_status[0].detail.as_ref().unwrap();
    assert_eq!(
        detail["reason"],
        json!("tail_query_unindexed"),
        "an unindexed schema is named: {detail}"
    );
}

// ---------------------------------------------------------------------------
// implementation-003: conversation clock, local time, source ids
// ---------------------------------------------------------------------------

const CC_QUEUE: &str = "88888888-8888-4888-8888-888888888888";
const CC_META: &str = "99999999-9999-4999-8999-999999999999";
const CC_NOCLOCK: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
const CC_NEWEST: &str = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
const CC_TIE_A: &str = "cccccccc-cccc-4ccc-8ccc-cccccccccccc";
const CC_TIE_B: &str = "dddddddd-dddd-4ddd-8ddd-dddddddddddd";
const CC_TIE_C: &str = "eeeeeeee-eeee-4eee-8eee-eeeeeeeeeeee";

fn meta_line(session: &str, kind: &str, timestamp: Option<&str>) -> String {
    let mut value = json!({
        "type": kind,
        "sessionId": session,
        "uuid": format!("uuid-{kind}-{}", timestamp.unwrap_or("none")),
        "cwd": "C:/work/alpha",
        "entrypoint": "cli",
    });
    if let Some(timestamp) = timestamp {
        value["timestamp"] = json!(timestamp);
    }
    value.to_string()
}

fn write_claude_file(
    root: &Path,
    project: &str,
    id: &str,
    lines: &[String],
    mtime_rfc3339: &str,
) -> PathBuf {
    let dir = root.join(project);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{id}.jsonl"));
    write_lines(&path, lines);
    set_mtime(&path, epoch(mtime_rfc3339));
    path
}

fn one_source_config(
    env: &support::TestEnv,
    name: &str,
    source_id: &str,
    kind: &str,
    root: &Path,
) -> PathBuf {
    env.write_json(
        &format!("{name}-config.json"),
        &json!({
            "version": 1,
            "sources": [
                {"source_id": source_id, "kind": kind, "root": root.to_string_lossy(), "enabled": true}
            ],
            "source_order": [source_id],
            "interpreter": {"kb": {"command": "C:/python/python.exe"}}
        }),
    )
}

#[test]
fn claude_last_activity_is_the_newest_conversation_record() {
    let env = support::TestEnv::new();
    let root = env.path().join("cc-clock");
    write_claude_file(
        &root,
        "proj",
        CC_QUEUE,
        &[
            claude_line(
                CC_QUEUE,
                "u1",
                "2026-11-06T15:57:00Z",
                "user",
                json!("synthetic question"),
                false,
                Some("cli"),
                "C:/work/alpha",
            ),
            claude_line(
                CC_QUEUE,
                "a1",
                "2026-11-06T15:57:30Z",
                "assistant",
                json!([{"type": "text", "text": "synthetic answer"}]),
                false,
                Some("cli"),
                "C:/work/alpha",
            ),
            // Every record below lands after the last turn and is bookkeeping:
            // it must never move `last_activity_at`.
            meta_line(CC_QUEUE, "queue-operation", Some("2026-11-06T17:10:00Z")),
            meta_line(CC_QUEUE, "system", Some("2026-11-06T17:10:05Z")),
            meta_line(CC_QUEUE, "attachment", Some("2026-11-06T17:10:10Z")),
            meta_line(CC_QUEUE, "summary", Some("2026-11-06T17:10:15Z")),
            meta_line(
                CC_QUEUE,
                "file-history-snapshot",
                Some("2026-11-06T17:10:20Z"),
            ),
        ],
        "2026-11-06T17:10:20Z",
    );
    let config = load_config(&one_source_config(
        &env,
        "clock",
        "ccx",
        "claude_code",
        &root,
    ))
    .unwrap();

    let response = recent_with_offset(&config, json!({"limit": 20}), Some(8 * 60));
    let session = response.sessions.first().expect("the session is listed");
    assert_eq!(
        session.last_activity_at.as_deref(),
        Some("2026-11-06T15:57:30Z"),
        "the last turn, not the later queue-operation record"
    );
    assert_eq!(
        session.last_activity_local.as_deref(),
        Some("2026-11-06T23:57:30+08:00"),
        "the same instant in the host zone"
    );

    // `since` shares the conversation clock: 16:00 is after the last turn and
    // before the bookkeeping records, so the session is filtered out.
    let after_turn = recent(
        &config,
        json!({"limit": 20, "since": "2026-11-06T16:00:00Z"}),
    );
    assert!(
        after_turn.sessions.is_empty(),
        "the queue-operation record must not keep the session inside `since`"
    );
    let at_turn = recent(
        &config,
        json!({"limit": 20, "since": "2026-11-06T15:57:30Z"}),
    );
    assert_eq!(at_turn.sessions.len(), 1);
}

#[test]
fn claude_last_activity_falls_back_to_any_record_then_mtime() {
    let env = support::TestEnv::new();
    let root = env.path().join("cc-fallback");
    // Only bookkeeping records: the newest record of any type is the fallback.
    write_claude_file(
        &root,
        "proj",
        CC_META,
        &[
            meta_line(CC_META, "system", Some("2026-11-06T10:00:00Z")),
            meta_line(CC_META, "queue-operation", Some("2026-11-06T10:05:00Z")),
        ],
        "2026-11-06T10:05:00Z",
    );
    // No timestamp anywhere: the file mtime is the last resort.
    write_claude_file(
        &root,
        "proj",
        CC_NOCLOCK,
        &[meta_line(CC_NOCLOCK, "user", None)],
        "2026-11-06T09:30:00Z",
    );
    let config = load_config(&one_source_config(
        &env,
        "fallback",
        "ccf",
        "claude_code",
        &root,
    ))
    .unwrap();

    let response = recent(&config, json!({"limit": 20}));
    let by_id = |id: &str| {
        response
            .sessions
            .iter()
            .find(|session| session.session_id == id)
            .unwrap_or_else(|| panic!("{id} listed"))
    };
    assert_eq!(
        by_id(CC_META).last_activity_at.as_deref(),
        Some("2026-11-06T10:05:00Z"),
        "a transcript without turns falls back to its newest record"
    );
    assert_eq!(
        by_id(CC_NOCLOCK).last_activity_at.as_deref(),
        Some(rfc(epoch("2026-11-06T09:30:00Z") as i64 * 1_000).as_str()),
        "a transcript without timestamps falls back to the file mtime"
    );
}

#[test]
fn claude_candidates_are_ranked_by_conversation_time_not_mtime() {
    // The newest conversation carries the oldest mtime; the three older
    // conversations have newer mtimes. The fixed mtime window contains every
    // file, so only the ranking rule decides the two survivors.
    let env = support::TestEnv::new();
    let root = env.path().join("cc-window");
    write_claude_file(
        &root,
        "proj",
        CC_NEWEST,
        &[claude_line(
            CC_NEWEST,
            "u1",
            "2026-11-06T12:00:00Z",
            "user",
            json!("newest conversation"),
            false,
            Some("cli"),
            "C:/work/alpha",
        )],
        "2026-11-06T10:00:00Z",
    );
    for (id, at, mtime) in [
        (CC_TIE_A, "2026-11-06T11:00:03Z", "2026-11-06T11:30:00Z"),
        (CC_TIE_B, "2026-11-06T11:00:02Z", "2026-11-06T11:20:00Z"),
        (CC_TIE_C, "2026-11-06T11:00:01Z", "2026-11-06T11:10:00Z"),
    ] {
        write_claude_file(
            &root,
            "proj",
            id,
            &[claude_line(
                id,
                "u1",
                at,
                "user",
                json!("older conversation"),
                false,
                Some("cli"),
                "C:/work/alpha",
            )],
            mtime,
        );
    }
    let config = load_config(&one_source_config(
        &env,
        "window",
        "ccw",
        "claude_code",
        &root,
    ))
    .unwrap();

    let response = recent(&config, json!({"limit": 2}));
    assert_eq!(
        ids(&response),
        vec![format!("ccw:{CC_NEWEST}"), format!("ccw:{CC_TIE_A}"),],
        "the limit is applied after ranking by the conversation clock"
    );
}

#[test]
fn last_activity_local_is_the_same_instant_in_the_host_zone() {
    let env = support::TestEnv::new();
    let fixture = build(&env);
    let config = load(&fixture);

    let east = recent_with_offset(&config, json!({"limit": 20, "tail_turns": 1}), Some(8 * 60));
    let main = east
        .sessions
        .iter()
        .find(|session| session.session_id == CC_MAIN)
        .expect("claude session");
    assert_eq!(
        main.last_activity_at.as_deref(),
        Some(rfc(T0 + 400_003).as_str())
    );
    assert_eq!(
        main.last_activity_local.as_deref(),
        Some(local_rfc(T0 + 400_003, 8 * 60).as_str())
    );
    assert!(main
        .last_activity_local
        .as_deref()
        .unwrap()
        .ends_with("+08:00"));
    assert!(
        main.tail
            .iter()
            .all(|turn| turn.timestamp.as_deref().unwrap_or("Z").ends_with('Z')),
        "tail timestamps stay UTC"
    );

    let west = recent_with_offset(&config, json!({"limit": 20}), Some(-5 * 60));
    for session in &west.sessions {
        let utc = session.last_activity_at.as_deref().expect("activity");
        let local = session.last_activity_local.as_deref().expect("local");
        assert!(utc.ends_with('Z'));
        assert!(local.ends_with("-05:00"), "{local}");
        assert_eq!(
            chrono::DateTime::parse_from_rfc3339(utc)
                .unwrap()
                .timestamp_millis(),
            chrono::DateTime::parse_from_rfc3339(local)
                .unwrap()
                .timestamp_millis(),
            "both fields describe one instant"
        );
    }
}

#[test]
fn source_not_configured_names_the_enabled_session_sources() {
    let env = support::TestEnv::new();
    let fixture = build(&env);
    let opencode = fixture.opencode.to_string_lossy().to_string();
    let codex = fixture.codex.to_string_lossy().to_string();
    let claude = fixture.claude.to_string_lossy().to_string();
    let canonical = env.write_json(
        "canonical-config.json",
        &json!({
            "version": 1,
            "sources": [
                {"source_id": "claude_code", "kind": "claude_code", "root": claude, "enabled": true},
                {"source_id": "opencode", "kind": "opencode", "root": opencode, "enabled": true},
                {"source_id": "codex", "kind": "codex", "root": codex, "enabled": true}
            ],
            "source_order": ["claude_code", "opencode", "codex"],
            "interpreter": {"kb": {"command": "C:/python/python.exe"}}
        }),
    );
    let config = load_config(&canonical).unwrap();

    let response = recent(&config, json!({"sources": ["claude-code"]}));
    assert_eq!(response.errors.len(), 1);
    assert_eq!(response.errors[0].code, codes::SOURCE_NOT_CONFIGURED);
    assert_eq!(
        response.errors[0].message,
        "source is not configured or is disabled; configured session sources: \
         claude_code, codex, opencode",
        "the hyphenated id is answered with the real ids, sorted"
    );

    // A disabled source is not offered as an available id.
    let mut document: Value = serde_json::from_slice(&std::fs::read(&canonical).unwrap()).unwrap();
    document["sources"][2]["enabled"] = json!(false);
    document["source_order"] = json!(["claude_code", "opencode"]);
    let disabled = env.write_json("disabled-config.json", &document);
    let config = load_config(&disabled).unwrap();
    let response = recent(&config, json!({"sources": ["codex"]}));
    assert_eq!(
        response.errors[0].message,
        "source is not configured or is disabled; configured session sources: \
         claude_code, opencode"
    );
}

// ---------------------------------------------------------------------------
// R7: MCP surface
// ---------------------------------------------------------------------------

struct McpSession {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: i64,
}

impl McpSession {
    fn start(config: &Path) -> McpSession {
        let mut child = Command::new(bin())
            .arg("mcp")
            .arg("--config")
            .arg(config)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("start mcp");
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        let mut session = McpSession {
            child,
            stdin,
            stdout,
            next_id: 1,
        };
        let initialize = session.request("initialize", json!({"protocolVersion": "2024-11-05"}));
        assert_eq!(initialize["result"]["protocolVersion"], "2024-11-05");
        session.notify("notifications/initialized", json!({}));
        session
    }

    fn send(&mut self, value: Value) {
        writeln!(self.stdin, "{value}").expect("write request");
        self.stdin.flush().expect("flush request");
    }

    fn notify(&mut self, method: &str, params: Value) {
        self.send(json!({"jsonrpc": "2.0", "method": method, "params": params}));
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        let mut line = String::new();
        let read = self.stdout.read_line(&mut line).expect("read response");
        assert!(read > 0, "mcp closed its stdout");
        let response: Value = serde_json::from_str(&line).expect("response is JSON");
        assert_eq!(response["id"], json!(id));
        response
    }

    fn call(&mut self, tool: &str, arguments: Value) -> Value {
        self.request("tools/call", json!({"name": tool, "arguments": arguments}))
    }
}

impl Drop for McpSession {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn mcp_lists_four_tools_with_a_recent_and_qualified_tool_and_unchanged_neighbours() {
    let env = support::TestEnv::new();
    let fixture = build(&env);
    let mut mcp = McpSession::start(&fixture.config);
    let response = mcp.request("tools/list", json!({}));
    let tools = response["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 4);
    let names: Vec<&str> = tools
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        vec![
            "memory_search",
            "memory_read",
            "memory_recent",
            "memory_qualified"
        ]
    );
    for tool in tools {
        let name = tool["name"].as_str().unwrap();
        let expected = if name == "memory_read" {
            json!(false)
        } else {
            json!(true)
        };
        assert_eq!(tool["annotations"]["readOnlyHint"], expected, "{name}");
    }

    // The search tool definition changed only by naming the DSH source in its
    // description (and the `sources` hint of `memory_recent`). The read tool
    // gained the optional reuse session id and the non-read-only annotation
    // (`readOnlyHint: false`) because a device read appends a local feedback
    // line; its digest is updated deliberately. Digest of
    // `serde_json::to_string(tool)`.
    assert_eq!(
        sha256_hex(serde_json::to_string(&tools[0]).unwrap().as_bytes()),
        "17f61c153110706a3be24932dd79cf5130d6358f59aeb5e69cc81593604fc3aa"
    );
    assert_eq!(
        sha256_hex(serde_json::to_string(&tools[1]).unwrap().as_bytes()),
        "92f509564546c0d0a745b32e8fbd55163c0d57b164fd0a9c8fdb103d8f5ce23f"
    );

    let schema = &tools[2]["inputSchema"];
    let properties = schema["properties"].as_object().unwrap();
    let mut keys: Vec<&str> = properties.keys().map(String::as_str).collect();
    keys.sort();
    assert_eq!(
        keys,
        vec![
            "exclude_session_ids",
            "limit",
            "max_bytes",
            "max_chars_per_turn",
            "session_id",
            "session_kinds",
            "since",
            "sources",
            "tail_roles",
            "tail_turns",
        ],
        "the MCP schema declares exactly the CLI request fields"
    );
    assert_eq!(schema["additionalProperties"], json!(false));
    assert_eq!(properties["limit"]["maximum"], json!(20));
    assert_eq!(properties["tail_turns"]["maximum"], json!(20));
    assert_eq!(properties["max_chars_per_turn"]["maximum"], json!(8000));
    assert!(schema.get("required").is_none());

    // F2: the schema names the real ids instead of leaving `sources` opaque.
    let sources_description = properties["sources"]["description"]
        .as_str()
        .expect("the sources field documents its values");
    for id in ["claude_code", "opencode", "codex", "dsh"] {
        assert!(
            sources_description.contains(id),
            "the sources description names {id}: {sources_description}"
        );
    }
    let tool_description = tools[2]["description"].as_str().unwrap();
    assert!(tool_description.contains("last_activity_local"));
    assert!(tool_description.contains("Z are UTC"));

    // Stage 7 §A: the fourth tool is read-only and declares exactly its three
    // fields with no required key. The first three definitions above keep their
    // frozen digests.
    let qualified_schema = &tools[3]["inputSchema"];
    let qualified_properties = qualified_schema["properties"].as_object().unwrap();
    let mut qualified_keys: Vec<&str> = qualified_properties.keys().map(String::as_str).collect();
    qualified_keys.sort();
    assert_eq!(qualified_keys, vec!["limit", "query", "state"]);
    assert_eq!(qualified_schema["additionalProperties"], json!(false));
    assert!(qualified_schema.get("required").is_none());
    assert_eq!(qualified_properties["limit"]["minimum"], json!(1));
    assert_eq!(qualified_properties["limit"]["maximum"], json!(200));
    assert_eq!(
        qualified_properties["state"]["enum"],
        json!(["rejected", "deferred", "cleared"])
    );
    assert!(
        tools[3]["description"]
            .as_str()
            .unwrap()
            .contains("rejected"),
        "the qualified description explains the rejected state"
    );

    // And the tool is callable through the same envelope.
    let call = mcp.call(
        "memory_recent",
        json!({"sources": ["cc"], "tail_turns": 1, "limit": 1}),
    );
    assert_eq!(call["result"]["isError"], json!(false));
    let envelope = &call["result"]["structuredContent"];
    assert_eq!(envelope["contract_version"], json!(2));
    assert_eq!(envelope["sessions"][0]["source"], json!("cc"));
    assert_eq!(envelope["sessions"][0]["tail"][0]["role"], json!("user"));
}

#[test]
fn cli_memory_recent_accepts_an_omitted_request() {
    let env = support::TestEnv::new();
    let fixture = build(&env);
    let output = Command::new(bin())
        .arg("memory_recent")
        .arg("--config")
        .arg(&fixture.config)
        .stdin(Stdio::null())
        .output()
        .expect("run memory_recent");
    assert_eq!(output.status.code(), Some(0));
    let value: Value = serde_json::from_slice(&output.stdout).expect("json envelope");
    assert_eq!(value["contract_version"], json!(2));
    assert_eq!(value["sessions"].as_array().unwrap().len(), 5);
    assert!(value.get("side_effects").is_none());
}

#[test]
fn unknown_request_keys_are_rejected() {
    let error = RecentRequest::parse(&json!({"turns": 3})).expect_err("unknown key");
    assert_eq!(error.code, codes::INVALID_REQUEST);
    assert_eq!(error.detail.unwrap()["path"], json!("turns"));
}

#[test]
fn session_id_returns_only_that_session_whatever_its_kind_or_age() {
    let env = support::TestEnv::new();
    let fixture = build(&env);
    let config = load(&fixture);
    for (target, source) in [
        (OC_OLD, "oc"),
        (OC_OCSR, "oc"),
        (CC_HEADLESS, "cc"),
        (CX_EXEC, "cx"),
        (CC_MAIN, "cc"),
    ] {
        let response = recent(
            &config,
            json!({"session_id": target, "limit": 1, "tail_turns": 1}),
        );
        assert!(
            response.errors.is_empty(),
            "{target}: {:?}",
            response.errors
        );
        assert_eq!(
            ids(&response),
            vec![format!("{source}:{target}")],
            "{target}"
        );
        assert_eq!(response.sessions[0].source, source, "{target}");
    }
    let response = recent(&config, json!({"session_id": "no-such-session"}));
    assert!(response.sessions.is_empty());
    assert!(response.errors.is_empty());

    for bad in [
        json!({"session_id": ""}),
        json!({"session_id": "  "}),
        json!({"session_id": 7}),
    ] {
        let error = RecentRequest::parse(&bad).expect_err("invalid session_id");
        assert_eq!(error.code, codes::INVALID_REQUEST);
        assert_eq!(error.detail.unwrap()["path"], json!("session_id"));
    }
}

#[test]
fn claude_session_id_is_found_outside_the_mtime_candidate_window() {
    let env = support::TestEnv::new();
    let root = env.path().join("cc-many");
    let target = "aaaaaaaa-0000-4000-8000-000000000000";
    write_claude_file(
        &root,
        "proj",
        target,
        &[claude_line(
            target,
            "u-target",
            "2026-01-01T00:00:00Z",
            "user",
            json!("oldest conversation"),
            false,
            Some("cli"),
            "C:/work/old",
        )],
        "2026-01-01T00:00:00Z",
    );
    for index in 0..12 {
        let id = format!("bbbbbbbb-0000-4000-8000-{index:012}");
        write_claude_file(
            &root,
            "proj",
            &id,
            &[claude_line(
                &id,
                &format!("u-{index}"),
                &format!("2026-02-{:02}T00:00:00Z", index + 1),
                "user",
                json!("newer conversation"),
                false,
                Some("cli"),
                "C:/work/new",
            )],
            &format!("2026-02-{:02}T00:00:00Z", index + 1),
        );
    }
    let config = load_config(&one_source_config(&env, "many", "cc", "claude_code", &root)).unwrap();
    let response = recent(
        &config,
        json!({"session_id": target, "limit": 1, "tail_turns": 1}),
    );
    assert_eq!(ids(&response), vec![format!("cc:{target}")]);
    assert_eq!(response.sessions[0].tail.len(), 1);
    assert_eq!(response.sessions[0].tail[0].text, "oldest conversation");
}

#[test]
fn claude_tail_window_grows_past_large_bookkeeping_records() {
    let env = support::TestEnv::new();
    let root = env.path().join("cc-big");
    let id = "cccccccc-0000-4000-8000-000000000000";
    let blob = "x".repeat(700 * 1024);
    let mut lines = vec![
        claude_line(
            id,
            "u-1",
            "2026-11-06T10:00:00Z",
            "user",
            json!("question before the blobs"),
            false,
            Some("cli"),
            "C:/work/big",
        ),
        claude_line(
            id,
            "a-1",
            "2026-11-06T10:01:00Z",
            "assistant",
            json!([{"type": "text", "text": "answer before the blobs"}]),
            false,
            Some("cli"),
            "C:/work/big",
        ),
    ];
    // Bookkeeping records far larger than the first window, newer than the
    // turns: neither the tail nor the clock may come from them.
    for (index, kind) in ["api-request-blob", "attachment", "api-request-blob"]
        .iter()
        .enumerate()
    {
        lines.push(
            json!({
                "type": kind,
                "sessionId": id,
                "timestamp": format!("2026-11-06T10:0{}:00Z", index + 2),
                "payload": blob,
            })
            .to_string(),
        );
    }
    write_claude_file(&root, "proj", id, &lines, "2026-11-06T10:05:00Z");
    let config = load_config(&one_source_config(&env, "big", "cc", "claude_code", &root)).unwrap();

    let response = recent(&config, json!({"tail_turns": 2}));
    let session = &response.sessions[0];
    let texts: Vec<&str> = session.tail.iter().map(|turn| turn.text.as_str()).collect();
    assert_eq!(
        texts,
        vec!["question before the blobs", "answer before the blobs"]
    );
    assert!(response.warnings.is_empty(), "{:?}", response.warnings);
    assert_eq!(
        session.last_activity_at.as_deref(),
        Some("2026-11-06T10:01:00Z")
    );

    // tail_turns 0 still finds the conversation clock behind the blobs.
    let response = recent(&config, json!({"tail_turns": 0}));
    assert_eq!(
        response.sessions[0].last_activity_at.as_deref(),
        Some("2026-11-06T10:01:00Z")
    );
}

#[test]
fn opencode_tail_skips_injected_and_bookkeeping_rows() {
    let env = support::TestEnv::new();
    let root = env.path().join("oc-synthetic");
    std::fs::create_dir_all(&root).unwrap();
    let conn = Connection::open(root.join("opencode.db")).unwrap();
    conn.execute_batch(support::V2_SCHEMA).unwrap();
    conn.execute(
        "INSERT INTO session_v2 (id, title, directory, time_created, time_updated, parent_id) \
         VALUES ('oc-s', 'synthetic test', 'C:/work/s', ?1, ?2, NULL)",
        rusqlite::params![T0, T0 + 100],
    )
    .unwrap();
    let rows: [(i64, &str, Value); 6] = [
        (1, "user", json!({"text": "real question"})),
        (
            2,
            "synthetic",
            json!({"metadata": {"instruction": {"paths": ["C:/x/AGENTS.md"]}}, "text": "Instructions from: C:/x/AGENTS.md"}),
        ),
        (
            3,
            "system",
            json!({"metadata": {"notice": "instructions"}, "text": "host notice"}),
        ),
        (
            4,
            "assistant",
            json!({"content": [{"type": "text", "text": "real answer"}]}),
        ),
        (
            5,
            "compaction",
            json!({"status": "completed", "text": "summary text"}),
        ),
        (6, "idle", json!({"outcome": "succeeded"})),
    ];
    for (seq, kind, data) in rows {
        conn.execute(
            "INSERT INTO session_message (id, session_id, seq, type, time_created, data) \
             VALUES (?1, 'oc-s', ?2, ?3, ?4, ?5)",
            rusqlite::params![format!("m{seq}"), seq, kind, T0 + seq, data.to_string()],
        )
        .unwrap();
    }
    drop(conn);
    let config = load_config(&one_source_config(&env, "syn", "oc", "opencode", &root)).unwrap();
    let response = recent(&config, json!({"tail_turns": 6}));
    let texts: Vec<&str> = response.sessions[0]
        .tail
        .iter()
        .map(|turn| turn.text.as_str())
        .collect();
    assert_eq!(texts, vec!["real question", "real answer"]);
}

#[test]
fn fast_scan_is_not_a_request_key() {
    let error = RecentRequest::parse(&json!({"fast_scan": true})).expect_err("unknown key");
    assert!(error.message.contains("fast_scan"), "{}", error.message);
}

fn single_codex(env: &support::TestEnv, name: &str) -> (RecallConfig, PathBuf, PathBuf) {
    let root = env.root(name);
    let rollout = write_rollout(
        &root,
        CX_MAIN,
        &[
            codex_line(
                CX_MAIN,
                "C:/work/codex",
                "2026-01-01T00:00:00Z",
                json!({"type": "session_meta", "payload": {"source": "cli"}}),
            ),
            codex_line(
                CX_MAIN,
                "C:/work/codex",
                "2026-01-01T00:00:01Z",
                codex_message("user", "synthetic codex fallback"),
            ),
        ],
    );
    let config = load_config(&one_source_config(env, name, "cx", "codex", &root)).unwrap();
    (config, root, rollout)
}

fn create_codex_threads(root: &Path) -> Connection {
    let conn = Connection::open(root.join("state_5.sqlite")).unwrap();
    conn.execute_batch(
        "CREATE TABLE threads (
            id TEXT PRIMARY KEY,
            title TEXT,
            cwd TEXT,
            rollout_path TEXT,
            updated_at INTEGER
        );",
    )
    .unwrap();
    conn
}

#[test]
fn codex_without_a_state_db_reads_the_rollout() {
    let env = support::TestEnv::new();
    let (config, _, _) = single_codex(&env, "codex-no-db");
    let response = recent(&config, json!({"limit": 10}));
    assert_eq!(ids(&response), vec![format!("cx:{CX_MAIN}")]);
    assert_eq!(status_of(&response, "cx"), status::OK);
}

#[test]
fn codex_with_an_empty_threads_table_reads_the_rollout() {
    let env = support::TestEnv::new();
    let (config, root, _) = single_codex(&env, "codex-empty-threads");
    drop(create_codex_threads(&root));
    let response = recent(&config, json!({"limit": 10}));
    assert_eq!(ids(&response), vec![format!("cx:{CX_MAIN}")]);
    assert_eq!(status_of(&response, "cx"), status::OK);
}

#[test]
fn codex_with_a_matching_thread_does_not_duplicate_the_rollout() {
    let env = support::TestEnv::new();
    let (config, root, rollout) = single_codex(&env, "codex-matching-thread");
    let conn = create_codex_threads(&root);
    conn.execute(
        "INSERT INTO threads (id, title, cwd, rollout_path, updated_at) VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![
            CX_MAIN,
            "database title",
            "C:/work/codex",
            rollout.to_string_lossy(),
            1_767_225_600i64
        ],
    )
    .unwrap();
    drop(conn);
    let response = recent(&config, json!({"limit": 10}));
    assert_eq!(ids(&response), vec![format!("cx:{CX_MAIN}")]);
    assert_eq!(
        response.sessions[0].title.as_deref(),
        Some("database title")
    );
}

#[test]
fn codex_directed_lookup_falls_back_when_the_thread_row_is_missing() {
    let env = support::TestEnv::new();
    let (config, root, _) = single_codex(&env, "codex-missing-directed-row");
    let conn = create_codex_threads(&root);
    conn.execute(
        "INSERT INTO threads (id, title, updated_at) VALUES ('another-session', 'other', 1)",
        [],
    )
    .unwrap();
    drop(conn);
    let response = recent(&config, json!({"session_id": CX_MAIN, "limit": 10}));
    assert_eq!(ids(&response), vec![format!("cx:{CX_MAIN}")]);
}

#[test]
fn codex_rollouts_supplement_a_short_thread_list_without_duplicates() {
    let env = support::TestEnv::new();
    let root = build_codex(&env.path().join("codex-short-list"));
    let conn = Connection::open(root.join("state_5.sqlite")).unwrap();
    conn.execute(
        "DELETE FROM threads WHERE id IN (?1, ?2)",
        rusqlite::params![CX_EXEC, CX_VSCODE],
    )
    .unwrap();
    drop(conn);
    let config = load_config(&one_source_config(&env, "short-list", "cx", "codex", &root)).unwrap();
    let response = recent(&config, json!({"limit": 10}));
    assert_eq!(
        ids(&response),
        vec![format!("cx:{CX_MAIN}"), format!("cx:{CX_VSCODE}")]
    );
}

#[test]
fn codex_normalizes_numeric_clocks_before_applying_limit() {
    let env = support::TestEnv::new();
    let root = env.root("codex-mixed-clocks");
    let a = "aaaaaaaa-1111-4111-8111-111111111111";
    let b = "bbbbbbbb-2222-4222-8222-222222222222";
    let real = "cccccccc-3333-4333-8333-333333333333";
    let text = "dddddddd-4444-4444-8444-444444444444";
    for id in [a, b, real, text] {
        write_rollout(
            &root,
            id,
            &[codex_line(
                id,
                "C:/work/codex",
                "2025-12-30T00:00:00Z",
                json!({"type": "session_meta", "payload": {"source": "cli"}}),
            )],
        );
    }
    let conn = Connection::open(root.join("state_5.sqlite")).unwrap();
    conn.execute_batch("CREATE TABLE threads (id TEXT PRIMARY KEY, title TEXT, updated_at);")
        .unwrap();
    let a_seconds = chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
        .unwrap()
        .timestamp();
    let b_ms = chrono::DateTime::parse_from_rfc3339("2025-12-31T00:00:00Z")
        .unwrap()
        .timestamp_millis();
    let real_ms = chrono::DateTime::parse_from_rfc3339("2025-12-30T00:00:00Z")
        .unwrap()
        .timestamp_millis() as f64
        + 0.5;
    for (id, value) in [
        (a, rusqlite::types::Value::Integer(a_seconds)),
        (b, rusqlite::types::Value::Integer(b_ms)),
        (real, rusqlite::types::Value::Real(real_ms)),
        (
            text,
            rusqlite::types::Value::Text("2025-12-29T00:00:00Z".to_string()),
        ),
    ] {
        conn.execute(
            "INSERT INTO threads (id, title, updated_at) VALUES (?1, ?1, ?2)",
            rusqlite::params![id, value],
        )
        .unwrap();
    }
    drop(conn);
    let config = load_config(&one_source_config(
        &env,
        "mixed-clocks",
        "cx",
        "codex",
        &root,
    ))
    .unwrap();

    let one = recent(&config, json!({"limit": 1}));
    let two = recent(&config, json!({"limit": 2}));
    assert_eq!(ids(&one), vec![format!("cx:{a}")]);
    assert_eq!(ids(&two), vec![format!("cx:{a}"), format!("cx:{b}")]);
    assert_eq!(ids(&one)[0], ids(&two)[0], "limit prefix consistency");

    let all = recent(&config, json!({"limit": 10}));
    assert_eq!(
        all.sessions.len(),
        4,
        "REAL and RFC3339 clocks are readable"
    );
    assert_eq!(status_of(&all, "cx"), status::OK);
}

#[test]
fn a_zero_candidate_partial_source_keeps_its_error_and_coverage() {
    let env = support::TestEnv::new();
    let root = env.root("dsh-broken-recent");
    env.write(
        "dsh-broken-recent/project/session-broken/session.jsonl",
        b"not a json header\n",
    );
    let config = load_config(&one_source_config(
        &env,
        "broken-recent",
        "dsh",
        "dsh",
        &root,
    ))
    .unwrap();
    let response = recent(&config, json!({"limit": 10}));

    assert!(response.sessions.is_empty());
    assert_eq!(status_of(&response, "dsh"), status::PARTIAL);
    assert_eq!(response.errors.len(), 1);
    assert_eq!(response.errors[0].code, codes::RECORD_SKIPPED);
    let detail = response.source_status[0].detail.as_ref().unwrap();
    assert_eq!(detail["coverage_complete"], json!(false));
    assert_eq!(detail["scanned_candidates"], json!(1));
    assert_eq!(detail["coverage"]["sessions_total"], json!(0));
}

#[test]
fn a_healthy_empty_source_reports_complete_zero_coverage() {
    let env = support::TestEnv::new();
    let root = env.root("dsh-empty-recent");
    let config = load_config(&one_source_config(
        &env,
        "empty-recent",
        "dsh",
        "dsh",
        &root,
    ))
    .unwrap();
    let response = recent(&config, json!({"limit": 10}));

    assert!(response.sessions.is_empty());
    assert!(response.errors.is_empty());
    assert_eq!(status_of(&response, "dsh"), status::EMPTY);
    let detail = response.source_status[0].detail.as_ref().unwrap();
    assert_eq!(detail["coverage_complete"], json!(true));
    assert_eq!(detail["scanned_candidates"], json!(0));
    assert_eq!(detail["coverage"]["sessions_total"], json!(0));
}

#[test]
fn codex_rollout_supplements_rank_by_activity_across_missing_thread_rows() {
    let env = support::TestEnv::new();
    let root = env.root("codex-missing-row-order");
    let old_id = "aaaaaaaa-1111-4111-8111-111111111111";
    let new_id = "99999999-9999-4999-8999-999999999999";
    let old_path = write_rollout(
        &root,
        old_id,
        &[
            codex_line(
                old_id,
                "C:/work/codex",
                &rfc(T0 - 500_000),
                json!({"type": "session_meta", "payload": {"source": "cli"}}),
            ),
            codex_line(
                old_id,
                "C:/work/codex",
                &rfc(T0 - 499_000),
                codex_message("user", "old rollout"),
            ),
        ],
    );
    set_mtime(&old_path, 1_759_000_000);
    let new_path = write_rollout(
        &root,
        new_id,
        &[
            codex_line(
                new_id,
                "C:/work/codex",
                &rfc(T0 + 500_000),
                json!({"type": "session_meta", "payload": {"source": "cli"}}),
            ),
            codex_line(
                new_id,
                "C:/work/codex",
                &rfc(T0 + 501_000),
                codex_message("user", "new rollout"),
            ),
        ],
    );
    set_mtime(&new_path, 1_760_000_500);
    // The state DB carries only the old row, with an old clock: the newest
    // conversation has no thread row at all. The rollout supplement must win
    // `limit=1`, and `limit=2` must start with it (prefix consistency).
    let conn = create_codex_threads(&root);
    conn.execute(
        "INSERT INTO threads (id, title, cwd, rollout_path, updated_at) VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![
            old_id,
            "old row",
            "C:/work/codex",
            old_path.to_string_lossy(),
            (T0 - 500_000) / 1_000
        ],
    )
    .unwrap();
    drop(conn);
    let config = load_config(&one_source_config(
        &env,
        "missing-row-order",
        "cx",
        "codex",
        &root,
    ))
    .unwrap();

    let one = recent(&config, json!({"limit": 1}));
    assert_eq!(ids(&one), vec![format!("cx:{new_id}")]);
    let two = recent(&config, json!({"limit": 2}));
    assert_eq!(
        ids(&two),
        vec![format!("cx:{new_id}"), format!("cx:{old_id}")]
    );
    assert_eq!(ids(&one)[0], ids(&two)[0], "limit prefix consistency");
}

#[test]
fn codex_text_clocks_rank_by_parsed_time_not_by_id() {
    let env = support::TestEnv::new();
    let root = env.root("codex-text-clocks");
    // The id order and the RFC3339 order disagree on purpose: sorting text
    // clocks by their raw value (id ASC here) would pick the older session.
    let older = "aaaaaaaa-1111-4111-8111-111111111111";
    let newer = "bbbbbbbb-2222-4222-8222-222222222222";
    for id in [older, newer] {
        write_rollout(
            &root,
            id,
            &[codex_line(
                id,
                "C:/work/codex",
                "2025-12-30T00:00:00Z",
                json!({"type": "session_meta", "payload": {"source": "cli"}}),
            )],
        );
    }
    let conn = create_codex_threads(&root);
    for (id, stamp) in [
        (older, "2026-01-01T00:00:00Z"),
        (newer, "2026-01-02T00:00:00Z"),
    ] {
        conn.execute(
            "INSERT INTO threads (id, title, updated_at) VALUES (?1, ?1, ?2)",
            rusqlite::params![id, stamp],
        )
        .unwrap();
    }
    drop(conn);
    let config = load_config(&one_source_config(
        &env,
        "text-clocks",
        "cx",
        "codex",
        &root,
    ))
    .unwrap();

    let one = recent(&config, json!({"limit": 1}));
    assert_eq!(ids(&one), vec![format!("cx:{newer}")]);
    let two = recent(&config, json!({"limit": 2}));
    assert_eq!(
        ids(&two),
        vec![format!("cx:{newer}"), format!("cx:{older}")]
    );
}

fn set_rollout_mtime(path: &Path, ms: i64) {
    let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
    file.set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_millis(ms as u64))
        .unwrap();
}

#[test]
fn codex_backup_rollouts_are_not_resurrected() {
    let env = support::TestEnv::new();
    let (config, root, _) = single_codex(&env, "codex-backup");
    drop(create_codex_threads(&root));
    let retired = "bbbbbbbb-9999-4999-8999-999999999999";
    let backup = root.join("sessions").join("backup");
    std::fs::create_dir_all(&backup).unwrap();
    write_lines(
        &backup.join(format!("rollout-2026-01-01T00-00-00-{retired}.jsonl")),
        &[
            codex_line(
                retired,
                "C:/work/codex",
                &rfc(T0 + 900_000),
                json!({"type": "session_meta", "payload": {"source": "cli"}}),
            ),
            codex_line(
                retired,
                "C:/work/codex",
                &rfc(T0 + 900_001),
                codex_message("user", "rotated copy"),
            ),
        ],
    );
    let response = recent(&config, json!({"limit": 10}));
    assert_eq!(ids(&response), vec![format!("cx:{CX_MAIN}")]);
}

#[test]
fn codex_missing_row_supplement_survives_a_full_sql_window() {
    let env = support::TestEnv::new();
    let root = env.root("codex-antijoin-window");
    let config =
        load_config(&one_source_config(&env, "antijoin-window", "cx", "codex", &root)).unwrap();
    let conn = create_codex_threads(&root);
    for index in 1..=37 {
        let id = format!("{index:08x}-1111-4111-8111-111111111111");
        let path = write_rollout(
            &root,
            &id,
            &[
                codex_line(
                    &id,
                    "C:/work/codex",
                    &rfc(T0 + index),
                    json!({"type": "session_meta", "payload": {"source": "cli"}}),
                ),
                codex_line(
                    &id,
                    "C:/work/codex",
                    &rfc(T0 + index),
                    codex_message("user", "known session"),
                ),
            ],
        );
        // Known rollouts are newer on disk than the missing one, so the old
        // `take(pool)`-then-anti-join order dropped the missing session.
        set_rollout_mtime(&path, T0 + 2_000 + index);
        conn.execute(
            "INSERT INTO threads (id, title, updated_at) VALUES (?1, 'db', ?2)",
            rusqlite::params![id, T0 + index],
        )
        .unwrap();
    }
    let missing = "000001f4-1111-4111-8111-111111111111";
    let missing_path = write_rollout(
        &root,
        missing,
        &[
            codex_line(
                missing,
                "C:/work/codex",
                &rfc(T0 + 500),
                json!({"type": "session_meta", "payload": {"source": "cli"}}),
            ),
            codex_line(
                missing,
                "C:/work/codex",
                &rfc(T0 + 501),
                codex_message("user", "missing thread row"),
            ),
        ],
    );
    set_rollout_mtime(&missing_path, T0 + 1_500);
    drop(conn);

    let response = recent(&config, json!({"limit": 1}));
    assert_eq!(ids(&response), vec![format!("cx:{missing}")]);
}

#[test]
fn codex_text_clocks_keep_the_newest_row_outside_the_default_window() {
    let env = support::TestEnv::new();
    let root = env.root("codex-text-window");
    let config =
        load_config(&one_source_config(&env, "text-window", "cx", "codex", &root)).unwrap();
    let conn = create_codex_threads(&root);
    let mut newest = String::new();
    for index in 1..=19 {
        let id = format!("{index:08x}-3333-4333-8333-333333333333");
        let clock = if index == 19 {
            newest = id.clone();
            "2026-01-19T00:00:00+00:00"
        } else {
            "2026-01-01T00:00:00Z"
        };
        conn.execute(
            "INSERT INTO threads (id, title, updated_at) VALUES (?1, 'db', ?2)",
            rusqlite::params![id, clock],
        )
        .unwrap();
        if index == 19 {
            write_rollout(
                &root,
                &id,
                &[
                    codex_line(
                        &id,
                        "C:/work/codex",
                        &rfc(T0 + 700_000),
                        json!({"type": "session_meta", "payload": {"source": "cli"}}),
                    ),
                    codex_line(
                        &id,
                        "C:/work/codex",
                        &rfc(T0 + 700_001),
                        codex_message("user", "newest text clock"),
                    ),
                ],
            );
        }
    }
    drop(conn);

    let response = recent(&config, json!({"limit": 1}));
    assert_eq!(ids(&response), vec![format!("cx:{newest}")]);
}
