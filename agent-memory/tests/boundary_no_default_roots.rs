//! Boundary reverse tests: default real-session-root channels are closed.
//!
//! These tests exist to fail against the pre-fix implementation and to stay
//! red if a default real-root channel is ever reintroduced:
//!
//! * `bare_env_home_resolution_fails_closed` — with no environment variables
//!   and no explicit roots, home resolution must return `None` (the pre-fix
//!   code fell back to `dirs::home_dir()`, i.e. the real user profile).
//! * `decoy_home_titles_are_read_only_through_explicit_injection` — decoy
//!   `session_index.jsonl` / `state_5.sqlite` data planted under a *synthetic*
//!   home is only ever consumed when explicitly injected; ambient scanning
//!   must not pick it up. (Decoys live under `CC_SWITCH_TEST_HOME`, which is
//!   the explicit authorization for a synthetic home; they never sit at, and
//!   must never be confused with, real agent data.)
//! * `tilde_sqlite_home_resolves_only_against_authorized_synthetic_home` —
//!   `~` in an injected `config.toml` resolves against the authorized
//!   synthetic home and fails closed (skips the DB) without it.
//! * `opencode_decoy_storage_is_read_only_through_explicit_injection` — the
//!   OpenCode base-dir bypass is gone; decoy storage/db is reachable only via
//!   the explicit-path scan API.
//! * `source_tree_contains_no_real_home_resolution` — source-level tripwire:
//!   no `dirs::home_dir()`, no `HOME`/`USERPROFILE` reads in `src/` code.
//!
//! The compile-time counterparts (deleted no-arg scan entries must not
//! compile) live as `compile_fail` doctests in `src/lib.rs`.

// env::set_var / env::remove_var are deprecated-but-safe on edition 2021;
// every mutation here is serialized by `env_lock()`.
#![allow(deprecated)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};

use agent_memory::config::get_home_dir;
use agent_memory::providers::codex::{
    load_thread_titles_from_config_dir, scan_sessions_in_roots_with_titles,
};
use agent_memory::providers::opencode::{scan_sessions_in, scan_sessions_sqlite};
use agent_memory::recall::load_config;
use agent_memory::recall::search::{memory_search, SearchRequest};
use serde_json::json;

/// Env vars that must never point this suite at real agent data. Removing them
/// (under the lock) models the bare `cargo test` environment.
const ROOT_ENV_VARS: [&str; 6] = [
    "CC_SWITCH_TEST_HOME",
    "XDG_DATA_HOME",
    "OPENCODE_DB",
    "CODEX_SQLITE_HOME",
    "CODEX_HOME",
    "HOME",
];

fn env_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    // A panicking env test must not cascade-fail the others: recover from
    // poisoning (each BareEnv still saves/restores the vars it touches).
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Save + clear the root-resolution env vars for the duration of a test.
/// Holds the env mutex until drop (env-mutating tests must not interleave with
/// each other) and restores the previous values on drop.
struct BareEnv {
    _guard: MutexGuard<'static, ()>,
    saved: Vec<(&'static str, Option<std::ffi::OsString>)>,
}

impl BareEnv {
    fn clear() -> Self {
        let guard = env_lock();
        let saved = ROOT_ENV_VARS
            .iter()
            .map(|&key| (key, std::env::var_os(key)))
            .collect::<Vec<_>>();
        for (key, _) in &saved {
            std::env::remove_var(key);
        }
        Self {
            _guard: guard,
            saved,
        }
    }
}

impl Drop for BareEnv {
    fn drop(&mut self) {
        for (key, value) in &self.saved {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }
}

/// WSL shares cannot hold SQLite locks; mirror the unit-test guard locally
/// (that helper is crate-private).
fn sqlite_unsupported_in_temp_dir() -> bool {
    std::env::temp_dir().to_string_lossy().starts_with(r"\\wsl")
}

fn write_codex_session(path: &Path, session_id: &str, message: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).expect("create session root");
    std::fs::write(
        path,
        format!(
            "{{\"timestamp\":\"2026-03-06T21:50:12Z\",\"type\":\"session_meta\",\"payload\":{{\"id\":\"{session_id}\",\"cwd\":\"/tmp/project\"}}}}\n\
             {{\"timestamp\":\"2026-03-06T21:50:13Z\",\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"user\",\"content\":\"{message}\"}}}}\n",
        ),
    )
    .expect("write session");
}

fn seed_codex_decoy(synthetic_home: &Path) -> (PathBuf, PathBuf) {
    let config_dir = synthetic_home.join(".codex");
    std::fs::create_dir_all(&config_dir).expect("create synthetic config dir");
    // Decoy titles: whatever happens, these must never leak into a scan that
    // did not explicitly inject them.
    std::fs::write(
        config_dir.join("session_index.jsonl"),
        "{\"id\":\"decoy-id\",\"thread_name\":\"PROBE-DECOY-INDEX-TITLE\"}\n",
    )
    .expect("write decoy session index");
    std::fs::write(config_dir.join("config.toml"), "sqlite_home = '~/.codex-alt'\n")
        .expect("write synthetic config.toml");
    let alt_dir = synthetic_home.join(".codex-alt");
    std::fs::create_dir_all(&alt_dir).expect("create alt state dir");
    let alt_db = alt_dir.join("state_5.sqlite");
    {
        let conn = rusqlite::Connection::open(&alt_db).expect("open decoy state db");
        conn.execute(
            "CREATE TABLE threads (id TEXT PRIMARY KEY, title TEXT NOT NULL, first_user_message TEXT NOT NULL)",
            [],
        )
        .expect("create threads table");
        conn.execute(
            "INSERT INTO threads (id, title, first_user_message) VALUES (?1, ?2, ?3)",
            ("decoy-id", "PROBE-DECOY-DB-TITLE", "Decoy session body"),
        )
        .expect("insert decoy thread");
    }
    let sessions_root = synthetic_home.join("sessions");
    write_codex_session(
        &sessions_root.join("rollout-decoy-id.jsonl"),
        "decoy-id",
        "Decoy session body",
    );
    (config_dir, sessions_root)
}

// ---------------------------------------------------------------------------
// Reverse test 1: bare environment (no env vars, no explicit roots) must fail
// closed. Against the pre-fix code this returns the REAL user profile.
// ---------------------------------------------------------------------------
#[test]
fn bare_env_home_resolution_fails_closed() {
    let _bare = BareEnv::clear();
    assert_eq!(
        get_home_dir(),
        None,
        "home resolution must fail closed without an explicit synthetic root; \
         a real-home fallback (pre-fix `dirs::home_dir()`) would surface here"
    );
    // And the synthetic override is still an explicit, opt-in authorization.
    let synthetic = tempfile::tempdir().expect("tempdir");
    std::env::set_var("CC_SWITCH_TEST_HOME", synthetic.path());
    assert_eq!(get_home_dir(), Some(synthetic.path().to_path_buf()));
    std::env::remove_var("CC_SWITCH_TEST_HOME");
}

// ---------------------------------------------------------------------------
// Reverse test 2: decoy titles under a synthetic home are read ONLY through
// explicit injection. The first scan passes no titles and must show the
// file-derived title — if a home-fallback title loader is reintroduced into
// the scan path, the decoy title leaks and this test fails (this is exactly
// how the pre-fix bug was demonstrated).
// ---------------------------------------------------------------------------
#[test]
fn decoy_home_titles_are_read_only_through_explicit_injection() {
    if sqlite_unsupported_in_temp_dir() {
        return;
    }
    let synthetic = tempfile::tempdir().expect("tempdir");
    let (config_dir, sessions_root) = seed_codex_decoy(synthetic.path());

    let _bare = BareEnv::clear();
    // Explicit authorization: the synthetic home (never a real one).
    std::env::set_var("CC_SWITCH_TEST_HOME", synthetic.path());

    // 1. Scan with explicit roots and NO title injection: the decoy titles
    //    planted under the synthetic home must not reach the result.
    let sessions =
        scan_sessions_in_roots_with_titles(std::slice::from_ref(&sessions_root), &HashMap::new());
    assert_eq!(sessions.len(), 1);
    assert_eq!(
        sessions[0].title.as_deref(),
        Some("Decoy session body"),
        "ambient (home-fallback) title loading leaked into an explicit scan"
    );

    // 2. Explicit injection of the same synthetic config DOES surface the
    //    titles (state db overrides the session index): the decoy data is
    //    reachable, but only when explicitly asked for.
    let titles = load_thread_titles_from_config_dir(&config_dir);
    assert_eq!(
        titles.get("decoy-id").map(String::as_str),
        Some("PROBE-DECOY-DB-TITLE")
    );
    let sessions = scan_sessions_in_roots_with_titles(std::slice::from_ref(&sessions_root), &titles);
    assert_eq!(
        sessions[0].title.as_deref(),
        Some("PROBE-DECOY-DB-TITLE"),
        "explicitly injected titles must still apply"
    );
}

// ---------------------------------------------------------------------------
// Reverse test 3: `~` inside an injected config only expands against the
// authorized synthetic home and fails closed (state DB skipped) without it.
// ---------------------------------------------------------------------------
#[test]
fn tilde_sqlite_home_resolves_only_against_authorized_synthetic_home() {
    if sqlite_unsupported_in_temp_dir() {
        return;
    }
    let synthetic = tempfile::tempdir().expect("tempdir");
    let (config_dir, _) = seed_codex_decoy(synthetic.path());

    let _bare = BareEnv::clear();

    // No synthetic home authorized: `~/.codex-alt` is unresolvable, the state
    // DB is skipped, and only the session index (explicitly injected dir) is
    // read. The decoy DB title must NOT appear.
    let titles = load_thread_titles_from_config_dir(&config_dir);
    assert_eq!(
        titles.get("decoy-id").map(String::as_str),
        Some("PROBE-DECOY-INDEX-TITLE"),
        "unresolvable `~` must fail closed instead of guessing the real profile"
    );

    // Authorized synthetic home: `~/.codex-alt` resolves inside it and the
    // state DB titles apply.
    std::env::set_var("CC_SWITCH_TEST_HOME", synthetic.path());
    let titles = load_thread_titles_from_config_dir(&config_dir);
    assert_eq!(
        titles.get("decoy-id").map(String::as_str),
        Some("PROBE-DECOY-DB-TITLE")
    );
}

// ---------------------------------------------------------------------------
// Reverse test 4: OpenCode decoy storage/db is reachable only through the
// explicit-path scan API (the ambient base-dir resolution is gone; the
// no-arg scan entry is pinned as compile-unreachable in src/lib.rs).
// ---------------------------------------------------------------------------
#[test]
fn opencode_decoy_storage_is_read_only_through_explicit_injection() {
    if sqlite_unsupported_in_temp_dir() {
        return;
    }
    let synthetic = tempfile::tempdir().expect("tempdir");
    // Decoy legacy JSON storage under the synthetic home.
    let storage = synthetic.path().join(".local/share/opencode/storage");
    let msg_dir = storage.join("message").join("ses_decoy");
    let part_dir = storage.join("part").join("msg_decoy_1");
    std::fs::create_dir_all(storage.join("session")).expect("create session dir");
    std::fs::create_dir_all(&msg_dir).expect("create message dir");
    std::fs::create_dir_all(&part_dir).expect("create part dir");
    std::fs::write(
        storage.join("session").join("ses_decoy.json"),
        r#"{"id":"ses_decoy","title":"PROBE-DECOY-OC-TITLE","directory":"/tmp/decoy","time":{"created":1771061953033,"updated":1771061954033}}"#,
    )
    .expect("write decoy session json");
    std::fs::write(
        msg_dir.join("msg_decoy_1.json"),
        r#"{"id":"msg_decoy_1","role":"user","time":{"created":1771061953033}}"#,
    )
    .expect("write decoy message");
    std::fs::write(part_dir.join("prt_1.json"), r#"{"id":"prt_1","type":"text","text":"Decoy"}"#)
        .expect("write decoy part");

    // Decoy SQLite db beside it (explicit path in the scan call below).
    let db_path = synthetic
        .path()
        .join(".local/share/opencode/opencode.db");
    {
        let conn = rusqlite::Connection::open(&db_path).expect("open decoy db");
        conn.execute_batch(
            "CREATE TABLE session (
                id TEXT PRIMARY KEY,
                title TEXT NOT NULL,
                directory TEXT NOT NULL,
                time_created INTEGER NOT NULL,
                time_updated INTEGER NOT NULL
            );
            CREATE TABLE message (
                id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                time_created INTEGER NOT NULL,
                data TEXT NOT NULL
            );
            CREATE TABLE part (
                id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                message_id TEXT NOT NULL,
                time_created INTEGER NOT NULL,
                data TEXT NOT NULL
            );",
        )
        .expect("create decoy schema");
        conn.execute(
            "INSERT INTO session (id, title, directory, time_created, time_updated) VALUES (?1, ?2, ?3, ?4, ?5)",
            ("ses_db_decoy", "PROBE-DECOY-OC-DB-TITLE", "/tmp/decoy-db", 1_771_061_953_033_i64, 1_771_061_954_033_i64),
        )
        .expect("insert decoy db session");
    }

    let _bare = BareEnv::clear();
    // Explicit authorization of the synthetic home; even with every ambient
    // knob pointed at the decoys, no API reads them without explicit paths.
    std::env::set_var("CC_SWITCH_TEST_HOME", synthetic.path());
    std::env::set_var("XDG_DATA_HOME", synthetic.path().join(".local/share"));

    // Explicit injection is the only way the decoys become visible.
    let sessions = scan_sessions_in(&storage, &db_path);
    let mut ids = sessions
        .iter()
        .map(|s| s.session_id.as_str())
        .collect::<Vec<_>>();
    ids.sort_unstable();
    assert_eq!(ids, vec!["ses_db_decoy", "ses_decoy"]);

    // And the explicit SQLite path form works on its own.
    let sqlite_only = scan_sessions_sqlite(&db_path);
    assert_eq!(sqlite_only.len(), 1);
    assert_eq!(
        sqlite_only[0].title.as_deref(),
        Some("PROBE-DECOY-OC-DB-TITLE")
    );
}

// ---------------------------------------------------------------------------
// Source-level tripwire: no real-home resolution may exist in `src/` code
// (comment lines are stripped before the check; docs may mention the pattern).
// ---------------------------------------------------------------------------
#[test]
fn source_tree_contains_no_real_home_resolution() {
    let src_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut pending = vec![src_root.clone()];
    let mut offenders = Vec::new();
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).expect("read src dir").flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
                continue;
            }
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            let text = std::fs::read_to_string(&path).expect("read source file");
            for (idx, line) in text.lines().enumerate() {
                let code = line.trim_start();
                if code.starts_with("//") {
                    continue; // doc/comment mentions are allowed
                }
                for needle in [
                    "dirs::home_dir",
                    "var(\"HOME\")",
                    "var_os(\"HOME\")",
                    "var(\"USERPROFILE\")",
                    "var_os(\"USERPROFILE\")",
                    "OPENCODE_DB",
                    "CODEX_SQLITE_HOME",
                ] {
                    if code.contains(needle) {
                        offenders.push(format!(
                            "{}:{}: {needle} -> {code}",
                            path.display(),
                            idx + 1
                        ));
                    }
                }
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "real-root / ambient redirection must not exist in code:\n{}",
        offenders.join("\n")
    );
}

/// Reverse tripwire: a decoy `~/.codex/sessions` tree is only reachable when a
/// codex source explicitly points at it. An explicit empty home must stay empty.
///
/// The decoy is planted under the authorized synthetic home
/// (`CC_SWITCH_TEST_HOME`) and `CODEX_HOME` is pointed at it too, so a
/// home/env fallback in the codex resolver would surface here.
#[test]
fn codex_decoy_sessions_are_not_discovered_by_default() {
    let synthetic = tempfile::tempdir().expect("tempdir");
    let synthetic_home = synthetic.path();
    let decoy_home = synthetic_home.join(".codex");
    let decoy_dir = decoy_home.join("sessions").join("2026").join("03").join("06");
    std::fs::create_dir_all(&decoy_dir).expect("decoy dir");
    std::fs::write(
        decoy_dir.join("rollout-2026-03-06T21-50-12-99999999-9999-4999-8999-999999999999.jsonl"),
        "{\"timestamp\":\"2026-03-06T21:50:12Z\",\"type\":\"session_meta\",\"payload\":{\"id\":\"99999999-9999-4999-8999-999999999999\",\"cwd\":\"C:/decoy\"}}\n\
         {\"timestamp\":\"2026-03-06T21:50:13Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":\"needlealpha decoy codex\"}}\n",
    )
    .expect("decoy rollout");

    let _bare = BareEnv::clear();
    std::env::set_var("CC_SWITCH_TEST_HOME", synthetic_home);
    std::env::set_var("CODEX_HOME", &decoy_home);

    let explicit_home = synthetic_home.join("explicit-codex");
    std::fs::create_dir_all(explicit_home.join("sessions")).expect("explicit sessions");
    let config_path = synthetic_home.join("recall.json");
    std::fs::write(
        &config_path,
        serde_json::to_string_pretty(&json!({
            "version": 1,
            "sources": [{
                "source_id": "cx",
                "kind": "codex",
                "root": explicit_home.to_string_lossy(),
                "enabled": true
            }],
            "source_order": ["cx"],
            "interpreter": {"kb": {"command": "C:/python/python.exe"}}
        }))
        .expect("config json"),
    )
    .expect("config");

    let config = load_config(&config_path).expect("explicit config");
    let request = SearchRequest::parse(&json!({"query": "needlealpha"})).expect("request");
    let response = memory_search(&config, &request);
    assert!(response.results.is_empty(), "decoy sessions must not be read");
    assert_eq!(response.source_status.len(), 1);
    assert_eq!(response.source_status[0].status, "empty");
    assert_eq!(response.exit_code, 0);
    assert_eq!(
        response.source_status[0]
            .detail
            .as_ref()
            .and_then(|detail| detail.get("coverage"))
            .and_then(|coverage| coverage.get("total_sessions")),
        Some(&json!(0))
    );
}
