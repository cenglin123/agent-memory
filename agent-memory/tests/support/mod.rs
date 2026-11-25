//! Test harness: temporary roots, an explicit configuration per case, the
//! Windows environment whitelist, and fixture builders that write only under
//! `<temp>`.

#![allow(dead_code)]

pub mod matrix;

use rusqlite::Connection;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};

/// Serializes every case that mutates the process environment.
fn env_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Variables passed through unchanged.
pub const PASSTHROUGH_ENV: [&str; 3] = ["PATH", "SystemRoot", "ComSpec"];

/// Variables redirected to the per-case temporary root.
pub const REDIRECTED_ENV: [&str; 15] = [
    "HOME",
    "USERPROFILE",
    "APPDATA",
    "LOCALAPPDATA",
    "TEMP",
    "TMP",
    "HOMEDRIVE",
    "HOMEPATH",
    "OneDrive",
    "OneDriveConsumer",
    "OneDriveCommercial",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_STATE_HOME",
    "XDG_CACHE_HOME",
];

pub struct TestEnv {
    dir: tempfile::TempDir,
    base: PathBuf,
    saved: BTreeMap<String, String>,
    _guard: MutexGuard<'static, ()>,
}

/// The system temporary directory as it was before any case redirected
/// `TEMP`/`TMP`. [`TestEnv::new`] records it before mutating the environment,
/// so a later read never observes another case's redirected value.
fn original_temp() -> &'static Path {
    static TEMP: OnceLock<PathBuf> = OnceLock::new();
    TEMP.get_or_init(|| {
        // One spelling for every fixture root: the canonical long form, with
        // the verbatim prefix removed so config validation accepts it.
        let raw = std::env::temp_dir();
        let canonical = raw.canonicalize().unwrap_or_else(|_| raw.clone());
        match canonical.to_string_lossy().strip_prefix(r"\?\") {
            Some(rest) => PathBuf::from(rest),
            None => canonical,
        }
    })
}

/// A temporary directory that never lands inside another case's redirected
/// root (which that case deletes when it drops), even when created by a case
/// that does not hold the environment lock.
pub fn stable_tempdir() -> tempfile::TempDir {
    tempfile::tempdir_in(original_temp()).expect("temporary directory")
}

impl TestEnv {
    pub fn new() -> TestEnv {
        let guard = env_lock();
        original_temp();
        // Create the root before touching TEMP so `tempdir()` still resolves.
        let dir = tempfile::tempdir().expect("temporary root");
        // 8.3 short names (RUNNER~1) would make fixture path strings differ from
        // the canonical spelling the capture layer stores; use the long form
        // everywhere so comparisons describe one directory.
        let base = dir
            .path()
            .canonicalize()
            .unwrap_or_else(|_| dir.path().to_path_buf());
        // `canonicalize` returns a verbatim path (\\?\C:\...); the config
        // validator rejects extended-length spellings, so strip the prefix.
        let base = match base.to_string_lossy().strip_prefix(r"\\?\") {
            Some(rest) => PathBuf::from(rest),
            None => base,
        };
        let saved: BTreeMap<String, String> = std::env::vars().collect();
        for key in saved.keys() {
            std::env::remove_var(key);
        }
        // Windows variable names are case-insensitive; a shell may export
        // `SYSTEMROOT`, which must still reach the child as `SystemRoot`.
        for key in PASSTHROUGH_ENV {
            let found = saved
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case(key))
                .map(|(_, value)| value.clone());
            if let Some(value) = found {
                std::env::set_var(key, value);
            }
        }
        let temp = base.to_string_lossy().to_string();
        let drive = temp.chars().next().map(|ch| ch.to_string()).unwrap_or_default();
        for key in REDIRECTED_ENV {
            let value = match key {
                "HOMEDRIVE" => format!("{drive}:"),
                "HOMEPATH" => temp
                    .chars()
                    .skip(2)
                    .collect::<String>()
                    .trim_start_matches(['/', '\\'])
                    .to_string(),
                _ => temp.clone(),
            };
            std::env::set_var(key, value);
        }
        TestEnv {
            dir,
            base,
            saved,
            _guard: guard,
        }
    }

    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    pub fn root(&self, name: &str) -> PathBuf {
        let path = self.base.join(name);
        std::fs::create_dir_all(&path).expect("source root");
        path
    }

    pub fn write(&self, rel: &str, bytes: &[u8]) -> PathBuf {
        let path = self.base.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("parent dir");
        }
        std::fs::write(&path, bytes).expect("write fixture");
        path
    }

    pub fn write_json(&self, rel: &str, value: &Value) -> PathBuf {
        self.write(rel, serde_json::to_string_pretty(value).unwrap().as_bytes())
    }

    /// Configuration with one interpreter and the given sources.
    pub fn config_value(&self, sources: Vec<Value>, order: Vec<&str>) -> Value {
        json!({
            "version": 1,
            "sources": sources,
            "source_order": order,
            "interpreter": {
                "kb": {
                    "command": "C:/python/python.exe",
                    "args": ["-B"],
                    "env": {"PYTHONUTF8": "1", "PYTHONDONTWRITEBYTECODE": "1"}
                }
            }
        })
    }

    pub fn write_config(&self, value: &Value) -> PathBuf {
        self.write_json("kb-recall-config.json", value)
    }

    /// A minimal valid OpenCode source entry.
    pub fn opencode_source(&self, source_id: &str, root: &Path) -> Value {
        json!({
            "source_id": source_id,
            "kind": "opencode",
            "root": root.to_string_lossy(),
            "enabled": true,
        })
    }

    pub fn md_source(&self, source_id: &str, kind: &str, root: &Path) -> Value {
        json!({
            "source_id": source_id,
            "kind": kind,
            "root": root.to_string_lossy(),
            "enabled": true,
        })
    }
}

impl Drop for TestEnv {
    fn drop(&mut self) {
        for key in std::env::vars().map(|(key, _)| key).collect::<Vec<_>>() {
            std::env::remove_var(key);
        }
        for (key, value) in &self.saved {
            std::env::set_var(key, value);
        }
    }
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

pub const NEEDLE: &str = "needlealpha";
pub const NEEDLE_V2: &str = "needlev2";
pub const NEEDLE_LEGACY: &str = "needlelegacy";

pub const V1_SCHEMA: &str = "
CREATE TABLE session (
    id TEXT PRIMARY KEY,
    parent_id TEXT,
    title TEXT,
    time_created INTEGER,
    time_updated INTEGER
);
CREATE TABLE message (
    id TEXT PRIMARY KEY,
    session_id TEXT,
    time_created INTEGER,
    time_updated INTEGER,
    data TEXT
);
CREATE TABLE part (
    id TEXT PRIMARY KEY,
    message_id TEXT,
    session_id TEXT,
    time_created INTEGER,
    time_updated INTEGER,
    data TEXT
);
CREATE INDEX message_session_id_idx ON message(session_id);
CREATE INDEX part_session_id_idx ON part(session_id);
CREATE INDEX part_message_id_idx ON part(message_id);
";

pub const V2_SCHEMA: &str = "
CREATE TABLE session_v2 (
    id TEXT PRIMARY KEY,
    title TEXT,
    directory TEXT,
    time_created INTEGER,
    time_updated INTEGER,
    parent_id TEXT
);
CREATE TABLE session_message (
    id TEXT PRIMARY KEY,
    session_id TEXT,
    seq INTEGER,
    type TEXT,
    time_created INTEGER,
    data TEXT
);
CREATE INDEX session_message_session_id_seq_idx ON session_message(session_id, seq);
";

pub fn insert_v1(
    conn: &Connection,
    session: &str,
    parent: Option<&str>,
    title: &str,
    updated: i64,
    message: &str,
    parts: &[(&str, i64, Value)],
) {
    conn.execute(
        "INSERT INTO session (id, parent_id, title, time_created, time_updated) VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![session, parent, title, updated - 100, updated],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO message (id, session_id, time_created, time_updated, data) VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![message, session, updated - 50, updated, "{}"],
    )
    .unwrap();
    for (part_id, created, block) in parts {
        conn.execute(
            "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                part_id,
                message,
                session,
                created,
                created,
                serde_json::to_string(block).unwrap()
            ],
        )
        .unwrap();
    }
}

pub fn tool_output(len: usize) -> String {
    let mut output = String::from("beginoutput ");
    output.push_str(NEEDLE);
    output.push(' ');
    output.push_str(&"x".repeat(len));
    output.push(' ');
    output.push_str(NEEDLE);
    output.push_str(" endoutput");
    output
}

pub struct V1Fixture {
    pub db: PathBuf,
    pub root: PathBuf,
}

/// V1 corpus: body hits, tool projection, injected variants, metadata types,
/// CRLF, empty and damaged parts, sub-sessions.
pub fn build_v1(env: &TestEnv) -> V1Fixture {
    let root = env.root("opencode-v1");
    let db = root.join("opencode.db");
    let conn = Connection::open(&db).unwrap();
    conn.execute_batch(V1_SCHEMA).unwrap();

    let long_output = tool_output(20_000);
    let parts = vec![
        (
            "p-text",
            10i64,
            json!({"type": "text", "text": format!("开头 {NEEDLE} 结尾")}),
        ),
        (
            "p-tool",
            11,
            json!({
                "type": "tool",
                "tool": "bash",
                "state": {
                    "input": {"z": 1, "a": "midneedle", "nested": {"y": 2, "x": 3}},
                    "output": long_output
                }
            }),
        ),
        ("p-tool-default", 12, json!({"type": "tool", "tool": "read"})),
        ("p-empty-text", 13, json!({"type": "text", "text": ""})),
        (
            "p-crlf",
            14,
            json!({"type": "text", "text": "line1\r\nneedlecrlf\r\nline3"}),
        ),
        (
            "p-injected-type",
            15,
            json!({"type": "injected", "text": format!("{NEEDLE} injected")}),
        ),
        (
            "p-injected-synthetic",
            16,
            json!({"type": "text", "text": format!("{NEEDLE} synthetic"), "synthetic": true}),
        ),
        (
            "p-injected-origin",
            17,
            json!({"type": "text", "text": format!("{NEEDLE} system"), "origin": "system"}),
        ),
        (
            "p-synthetic-string",
            18,
            json!({"type": "text", "text": format!("{NEEDLE} synthetic-string"), "synthetic": "true"}),
        ),
        (
            "p-origin-user",
            19,
            json!({"type": "text", "text": format!("{NEEDLE} user-origin"), "origin": "user"}),
        ),
        ("p-snapshot", 20, json!({"type": "snapshot", "text": NEEDLE})),
        ("p-patch", 21, json!({"type": "patch", "text": NEEDLE})),
        ("p-file", 22, json!({"type": "file", "url": NEEDLE})),
        ("p-agent", 23, json!({"type": "agent", "name": NEEDLE})),
        ("p-compaction", 24, json!({"type": "compaction", "text": NEEDLE})),
        ("p-retry", 25, json!({"type": "retry", "text": NEEDLE})),
        ("p-subtask", 26, json!({"type": "subtask", "text": NEEDLE})),
        ("p-widget", 27, json!({"type": "widget", "text": NEEDLE})),
    ];
    insert_v1(
        &conn,
        "s-main",
        None,
        "项目讨论",
        3_000,
        "m-main",
        &parts,
    );

    insert_v1(
        &conn,
        "s-child",
        Some("s-main"),
        "child session",
        2_900,
        "m-child",
        &[(
            "p-child",
            10,
            json!({"type": "text", "text": "needlechildonly"}),
        )],
    );
    insert_v1(
        &conn,
        "s-ocsr",
        None,
        "ocsr-subsession",
        2_800,
        "m-ocsr",
        &[(
            "p-ocsr",
            10,
            json!({"type": "text", "text": "needleocsronly"}),
        )],
    );
    drop(conn);
    V1Fixture { db, root }
}

/// The clean corpus plus a bad JSON row and two structural failures.
pub fn build_v1_damaged(env: &TestEnv) -> V1Fixture {
    let fixture = build_v1(env);
    let conn = Connection::open(&fixture.db).unwrap();
    // A raw non-JSON part row: counted as a skipped record.
    conn.execute(
        "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params!["p-broken-json", "m-main", "s-main", 30i64, 30i64, "not json"],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![
            "p-broken-output",
            "m-main",
            "s-main",
            31i64,
            31i64,
            serde_json::to_string(&json!({"type": "tool", "tool": "bash", "state": {"output": 42}})).unwrap()
        ],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![
            "p-broken-text",
            "m-main",
            "s-main",
            32i64,
            32i64,
            serde_json::to_string(&json!({"type": "text", "text": 7})).unwrap()
        ],
    )
    .unwrap();
    drop(conn);
    fixture
}

/// A corpus with `hits` distinct text parts, used for pagination and budget
/// cases. Every part is ~160 bytes so budgets stay predictable.
/// `parts_per_session` text parts per session, each with a ~350-byte JSON body
/// (`data` column), used for pagination/budget/scale cases.
pub fn build_v1_hits(env: &TestEnv, name: &str, sessions: usize, parts_per_session: usize) -> V1Fixture {
    let root = env.root(name);
    let db = root.join("opencode.db");
    let conn = Connection::open(&db).unwrap();
    conn.execute_batch(V1_SCHEMA).unwrap();
    conn.execute_batch("BEGIN;").unwrap();
    for session in 0..sessions {
        let session_id = format!("s-{session:03}");
        let parts: Vec<(String, i64, Value)> = (0..parts_per_session)
            .map(|part| {
                (
                    format!("p-{session:03}-{part:03}"),
                    part as i64,
                    json!({
                        "type": "text",
                        "text": format!("{NEEDLE} session {session} part {part} {}", "y".repeat(300))
                    }),
                )
            })
            .collect();
        let borrowed: Vec<(&str, i64, Value)> = parts
            .iter()
            .map(|(id, created, value)| (id.as_str(), *created, value.clone()))
            .collect();
        insert_v1(
            &conn,
            &session_id,
            None,
            &format!("session {session}"),
            (sessions - session) as i64 * 100,
            &format!("m-{session:03}"),
            &borrowed,
        );
    }
    conn.execute_batch("COMMIT;").unwrap();
    drop(conn);
    V1Fixture { db, root }
}

/// Total raw `data` bytes of a V1 fixture (used to keep budget contrasts
/// self-consistent instead of hard-coding an unverified size).
pub fn v1_data_bytes(db: &Path) -> u64 {
    let conn = Connection::open(db).unwrap();
    conn.query_row("SELECT COALESCE(SUM(LENGTH(data)), 0) FROM part", [], |row| {
        row.get::<_, i64>(0)
    })
    .unwrap() as u64
}

/// Scale fixture for the P1 latency target: 1000 sessions / 100000 parts with
/// a ~350-byte `data` row each, i.e. ≈ 35 MiB of part JSON (> the 32 MiB
/// contrast budget).
pub fn build_v1_scale(env: &TestEnv) -> V1Fixture {
    build_v1_hits(env, "scale-large", 1_000, 100)
}

pub fn build_v2(env: &TestEnv) -> V1Fixture {
    let root = env.root("opencode-v2");
    let db = root.join("opencode.db");
    let conn = Connection::open(&db).unwrap();
    conn.execute_batch(V2_SCHEMA).unwrap();
    let long_output = format!("pre {NEEDLE_V2} {}", "y".repeat(20_000));
    conn.execute(
        "INSERT INTO session_v2 (id, title, directory, time_created, time_updated, parent_id) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params!["v2-main", "V2 会话", "C:/work", 1_000i64, 5_000i64, Option::<String>::None],
    )
    .unwrap();
    let row_one = json!({
        "content": [
            {"type": "text", "text": format!("v2 body {NEEDLE_V2} tail")},
            {"type": "tool", "tool": "grep", "state": {"input": {"pattern": "mid"}, "output": long_output}},
            {"type": "reasoning", "text": NEEDLE_V2}
        ]
    });
    conn.execute(
        "INSERT INTO session_message (id, session_id, seq, type, time_created, data) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params!["sm-1", "v2-main", 1i64, "assistant", 1_100i64, serde_json::to_string(&row_one).unwrap()],
    )
    .unwrap();
    let row_two = json!({"text": format!("text branch {NEEDLE_V2} value")});
    conn.execute(
        "INSERT INTO session_message (id, session_id, seq, type, time_created, data) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params!["sm-2", "v2-main", 2i64, "user", 1_200i64, serde_json::to_string(&row_two).unwrap()],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO session_v2 (id, title, directory, time_created, time_updated, parent_id) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params!["v2-child", "child", "C:/work", 900i64, 4_000i64, "v2-main"],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO session_message (id, session_id, seq, type, time_created, data) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params!["sm-child", "v2-child", 1i64, "user", 950i64, serde_json::to_string(&json!({"text": NEEDLE_V2})).unwrap()],
    )
    .unwrap();
    drop(conn);
    V1Fixture { db, root }
}

/// V2 corpus with injected/metadata parts, a long tool input and a non-string
/// `data.text` row.
pub fn build_v2_extra(env: &TestEnv) -> V1Fixture {
    let root = env.root("opencode-v2-extra");
    let db = root.join("opencode.db");
    let conn = Connection::open(&db).unwrap();
    conn.execute_batch(V2_SCHEMA).unwrap();
    conn.execute(
        "INSERT INTO session_v2 (id, title, directory, time_created, time_updated, parent_id) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params!["v2-extra", "V2 extra", "C:/work", 1i64, 9_000i64, Option::<String>::None],
    )
    .unwrap();
    let long_input = format!("{}needleextra", "q".repeat(20_000));
    let row_one = json!({
        "content": [
            {"type": "text", "text": "body needleextra value"},
            {"type": "injected", "text": "needleextra injected"},
            {"type": "snapshot", "text": "needleextra snapshot"},
            {"type": "tool", "tool": "bash", "state": {"input": {"blob": long_input}, "output": ""}}
        ]
    });
    conn.execute(
        "INSERT INTO session_message (id, session_id, seq, type, time_created, data) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params!["sm-x1", "v2-extra", 1i64, "assistant", 1_100i64, serde_json::to_string(&row_one).unwrap()],
    )
    .unwrap();
    // Non-string data.text: a known /text structural failure.
    conn.execute(
        "INSERT INTO session_message (id, session_id, seq, type, time_created, data) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params!["sm-x2", "v2-extra", 2i64, "user", 1_200i64, serde_json::to_string(&json!({"text": 42})).unwrap()],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO session_message (id, session_id, seq, type, time_created, data) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params!["sm-x3", "v2-extra", 3i64, "user", 1_300i64, serde_json::to_string(&json!({"text": "text branch needleextra"})).unwrap()],
    )
    .unwrap();
    drop(conn);
    V1Fixture { db, root }
}

/// V1 database without the `parent_id` column, to exercise the V2 column
/// predicate (the column is optional in that generation).
pub fn build_v2_without_parent(env: &TestEnv) -> V1Fixture {
    let root = env.root("opencode-v2-noparent");
    let db = root.join("opencode.db");
    let conn = Connection::open(&db).unwrap();
    conn.execute_batch(
        "CREATE TABLE session_v2 (id TEXT PRIMARY KEY, title TEXT, directory TEXT, time_created INTEGER, time_updated INTEGER);
         CREATE TABLE session_message (id TEXT PRIMARY KEY, session_id TEXT, seq INTEGER, type TEXT, time_created INTEGER, data TEXT);",
    )
    .unwrap();
    conn.execute(
        "INSERT INTO session_v2 (id, title, directory, time_created, time_updated) VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params!["v2-a", "no parent column", "C:/work", 1_000i64, 2_000i64],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO session_message (id, session_id, seq, type, time_created, data) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params!["sm-a", "v2-a", 1i64, "user", 1_100i64, serde_json::to_string(&json!({"text": NEEDLE_V2})).unwrap()],
    )
    .unwrap();
    drop(conn);
    V1Fixture { db, root }
}

pub struct LegacyFixture {
    pub root: PathBuf,
}

/// Legacy JSON corpus with escapes, CRLF, a tool output and a corrupt part.
pub fn build_legacy(env: &TestEnv) -> LegacyFixture {
    let root = env.root("opencode-json");
    env.write_json(
        "opencode-json/storage/session/project/s-main.json",
        &json!({
            "id": "s-main",
            "title": "JSON 会话",
            "time": {"created": 1000, "updated": 9000},
            "parentID": null
        }),
    );
    env.write_json(
        "opencode-json/storage/session/project/s-child.json",
        &json!({
            "id": "s-child",
            "title": "json child",
            "time": {"created": 900, "updated": 8000},
            "parentID": "s-main"
        }),
    );
    env.write_json(
        "opencode-json/storage/message/s-main/m1.json",
        &json!({"id": "m1", "sessionID": "s-main", "time": {"created": 1010, "updated": 1020}}),
    );
    env.write_json(
        "opencode-json/storage/message/s-main/m2.json",
        &json!({"id": "m2", "sessionID": "s-main", "time": {"created": 1030, "updated": 1040}}),
    );
    env.write_json(
        "opencode-json/storage/message/s-child/mc.json",
        &json!({"id": "mc", "sessionID": "s-child", "time": {"created": 910, "updated": 920}}),
    );
    env.write_json(
        "opencode-json/storage/part/m1/p1.json",
        &json!({
            "id": "p1",
            "messageID": "m1",
            "sessionID": "s-main",
            "type": "text",
            "time": {"created": 1011},
            "text": format!("quote \" backslash \\ unicode 中 {NEEDLE_LEGACY}")
        }),
    );
    env.write_json(
        "opencode-json/storage/part/m1/p2.json",
        &json!({
            "id": "p2",
            "messageID": "m1",
            "sessionID": "s-main",
            "type": "tool",
            "time": {"created": 1012},
            "tool": "bash",
            "state": {"input": {"command": "echo needlelegacy"}, "output": tool_output(5_000)}
        }),
    );
    env.write_json(
        "opencode-json/storage/part/m2/p3.json",
        &json!({
            "id": "p3",
            "messageID": "m2",
            "sessionID": "s-main",
            "type": "text",
            "time": {"created": 1031},
            "text": "first\r\nsecond needlecrlf\r\nthird"
        }),
    );
    env.write(
        "opencode-json/storage/part/m2/p-broken.json",
        b"{not valid json",
    );
    env.write_json(
        "opencode-json/storage/part/mc/pc.json",
        &json!({"id": "pc", "messageID": "mc", "sessionID": "s-child", "type": "text", "text": NEEDLE_LEGACY}),
    );
    LegacyFixture { root }
}

/// A configuration for one OpenCode source plus its explicit config path.
pub struct Configured {
    pub root: PathBuf,
    pub config: PathBuf,
}

pub fn configured(env: &TestEnv, name: &str, fixture_root: &Path) -> Configured {
    let source = env.opencode_source("oc", fixture_root);
    let value = env.config_value(vec![source], vec!["oc"]);
    let config = env.write_json(&format!("{name}-config.json"), &value);
    Configured {
        root: fixture_root.to_path_buf(),
        config,
    }
}

/// `(relative path, size, mtime)` for every file under `root`, sorted. Used to
/// prove that a read path (or `--strict-snapshot`) leaves a tree untouched.
pub fn tree_state(root: &Path) -> Vec<(String, u64, u128)> {
    fn walk(root: &Path, current: &Path, out: &mut Vec<(String, u64, u128)>) {
        let Ok(entries) = std::fs::read_dir(current) else {
            return;
        };
        for entry in entries.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_symlink() {
                continue;
            }
            let path = entry.path();
            if file_type.is_dir() {
                walk(root, &path, out);
                continue;
            }
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            let mtime = metadata
                .modified()
                .ok()
                .and_then(|value| value.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|value| value.as_nanos())
                .unwrap_or(0);
            let rel = path
                .strip_prefix(root)
                .map(|rel| rel.to_string_lossy().replace('\\', "/"))
                .unwrap_or_default();
            out.push((rel, metadata.len(), mtime));
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out.sort();
    out
}

// ---------------------------------------------------------------------------
// Codex rollout fixtures (`codex-rollout-v1` recall)
// ---------------------------------------------------------------------------

/// An explicit Codex source entry: `root` is the Codex home; `storage` and
/// `db` are optional absolute paths inside it.
pub fn codex_source(
    source_id: &str,
    root: &Path,
    db: Option<&Path>,
    storage: Option<&Path>,
) -> Value {
    let mut source = json!({
        "source_id": source_id,
        "kind": "codex",
        "root": root.to_string_lossy(),
        "enabled": true,
    });
    if let Some(db) = db {
        source["db"] = json!(db.to_string_lossy());
    }
    if let Some(storage) = storage {
        source["storage"] = json!(storage.to_string_lossy());
    }
    source
}

pub fn codex_meta(session_id: &str) -> String {
    json!({
        "timestamp": "2026-03-06T21:50:12Z",
        "type": "session_meta",
        "payload": {"id": session_id, "cwd": "C:/work/project"}
    })
    .to_string()
}

pub fn codex_message(role: &str, content: Value) -> String {
    json!({
        "timestamp": "2026-03-06T21:50:13Z",
        "type": "response_item",
        "payload": {"type": "message", "role": role, "content": content}
    })
    .to_string()
}

pub fn codex_user(text: &str) -> String {
    codex_message("user", json!(text))
}

pub fn codex_assistant(text: &str) -> String {
    codex_message("assistant", json!(text))
}

pub fn codex_function_call(call_id: &str, name: &str, arguments: &str) -> String {
    json!({
        "timestamp": "2026-03-06T21:50:14Z",
        "type": "response_item",
        "payload": {
            "type": "function_call",
            "id": format!("fc-{call_id}"),
            "call_id": call_id,
            "name": name,
            "arguments": arguments
        }
    })
    .to_string()
}

pub fn codex_function_output(call_id: &str, output: Value) -> String {
    json!({
        "timestamp": "2026-03-06T21:50:15Z",
        "type": "response_item",
        "payload": {"type": "function_call_output", "call_id": call_id, "output": output}
    })
    .to_string()
}

pub fn codex_custom_call(call_id: &str, name: &str, input: &str) -> String {
    json!({
        "timestamp": "2026-03-06T21:50:14Z",
        "type": "response_item",
        "payload": {
            "type": "custom_tool_call",
            "id": format!("ctc-{call_id}"),
            "call_id": call_id,
            "name": name,
            "input": input
        }
    })
    .to_string()
}

pub fn codex_local_shell(call_id: &str, command: &str) -> String {
    json!({
        "timestamp": "2026-03-06T21:50:14Z",
        "type": "response_item",
        "payload": {
            "type": "local_shell_call",
            "id": format!("lsc-{call_id}"),
            "call_id": call_id,
            "action": {"type": "exec", "command": command}
        }
    })
    .to_string()
}

pub fn codex_command_completed(call_id: &str, output: &str) -> String {
    json!({
        "timestamp": "2026-03-06T21:50:16Z",
        "type": "event_msg",
        "payload": {
            "type": "item_completed",
            "item": {
                "type": "CommandExecution",
                "id": call_id,
                "aggregated_output": output
            }
        }
    })
    .to_string()
}

pub fn codex_mcp_completed(call_id: &str, text: &str) -> String {
    json!({
        "timestamp": "2026-03-06T21:50:16Z",
        "type": "event_msg",
        "payload": {
            "type": "item_completed",
            "item": {
                "type": "McpToolCall",
                "id": call_id,
                "result": {"content": [{"type": "text", "text": text}]}
            }
        }
    })
    .to_string()
}

/// Write a rollout under `<home>/<folder>/2026/03/06/`.
pub fn build_codex_rollout_in(
    home: &Path,
    folder: &str,
    session_id: &str,
    lines: &[String],
) -> PathBuf {
    let dir = home.join(folder).join("2026").join("03").join("06");
    std::fs::create_dir_all(&dir).expect("codex rollout dir");
    let path = dir.join(format!("rollout-2026-03-06T21-50-12-{session_id}.jsonl"));
    let mut bytes = String::new();
    for line in lines {
        bytes.push_str(line);
        bytes.push('\n');
    }
    std::fs::write(&path, bytes).expect("write codex rollout");
    path
}

pub fn build_codex_rollout(home: &Path, session_id: &str, lines: &[String]) -> PathBuf {
    build_codex_rollout_in(home, "sessions", session_id, lines)
}

/// Compressed sibling (`<path>.zst`). The plain file is left in place; callers
/// that model a compressed-only carrier remove it themselves.
pub fn build_codex_zst(path: &Path) -> PathBuf {
    let bytes = std::fs::read(path).expect("read plain rollout");
    let encoded = zstd::stream::encode_all(bytes.as_slice(), 3).expect("zstd encode");
    let zst = PathBuf::from(format!("{}.zst", path.display()));
    std::fs::write(&zst, encoded).expect("write zst rollout");
    zst
}

pub fn build_state_db(path: &Path, rows: &[(&str, &str, &str)]) {
    let conn = Connection::open(path).expect("state db");
    conn.execute_batch(
        "CREATE TABLE threads (
            id TEXT PRIMARY KEY,
            title TEXT NOT NULL,
            first_user_message TEXT NOT NULL
        );",
    )
    .expect("threads table");
    for (id, title, first_user_message) in rows {
        conn.execute(
            "INSERT INTO threads (id, title, first_user_message) VALUES (?1, ?2, ?3)",
            rusqlite::params![id, title, first_user_message],
        )
        .expect("insert thread");
    }
}

pub fn build_session_index(path: &Path, entries: &[(&str, &str)]) {
    let mut text = String::new();
    for (id, title) in entries {
        text.push_str(
            &json!({"id": id, "thread_name": title}).to_string(),
        );
        text.push('\n');
    }
    std::fs::write(path, text).expect("session index");
}

/// Resolve an object's on-disk file by its explicit storage encoding (S2.5:
/// `<sha>` plain vs `<sha>.zst`) and return its *decoded* bytes plus the path.
/// Test-side counterpart of `Archive::read_object_bytes` for cases that must
/// touch the file directly (tamper/label/missing-object fixtures).
pub fn read_object_file(root: &Path, sha: &str) -> (PathBuf, Vec<u8>) {
    let dir = root
        .join("archive")
        .join("objects")
        .join(&sha[0..2])
        .join(&sha[2..4]);
    let zstd_path = dir.join(format!("{sha}.zst"));
    if zstd_path.exists() {
        let compressed = std::fs::read(&zstd_path).expect("read zstd object");
        let decoded = zstd::stream::decode_all(compressed.as_slice())
            .expect("decode zstd object");
        (zstd_path, decoded)
    } else {
        let plain_path = dir.join(sha);
        let bytes = std::fs::read(&plain_path).expect("read plain object");
        (plain_path, bytes)
    }
}
