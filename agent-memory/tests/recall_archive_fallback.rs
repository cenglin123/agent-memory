//! Stage-2 S5 archive fallback: sessions that are gone from a live source stay
//! searchable and readable from the committed evidence ("live first, archive
//! fills the gaps").
//!
//! Every fixture is synthetic and lives under a per-test temporary root: the
//! sources are written by the test, captured through the same `capture-all`
//! engine production uses, and indexed through `index_all`. No test points at
//! a real Claude, Codex or OpenCode session directory, database or Archive.

use agent_memory::archive::Archive;
use agent_memory::recall::config::load_config;
use agent_memory::recall::fulltext::build::index_all;
use agent_memory::recall::read::{memory_read, ReadRequest};
use agent_memory::recall::search::{memory_search, SearchRequest};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

mod support;

use support::TestEnv;

const CLAUDE_TEXT_B: &str = "archived claude session 唯一句 needleclaudearchive";
const CODEX_TEXT_A: &str = "deleted codex rollout needlecodexarchive";
const CODEX_TEXT_B_ZST: &str = "compressed codex rollout needlecodexzst";
const OPENCODE_TEXT_B: &str = "opencode V2 已删除会话 needleopencodearchive";

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn claude_line(session: &str, uuid: &str, text: &str) -> String {
    json!({
        "type": "user",
        "message": {"role": "user", "content": [{"type": "text", "text": text}]},
        "timestamp": "2026-10-25T13:25:00Z",
        "sessionId": session,
        "uuid": uuid,
        "isSidechain": false
    })
    .to_string()
}

fn codex_meta(id: &str) -> String {
    json!({
        "timestamp": "2026-10-25T10:00:00Z",
        "type": "session_meta",
        "payload": {"id": id, "source": "cli"}
    })
    .to_string()
}

fn codex_message(i: usize, text: &str) -> String {
    json!({
        "timestamp": "2026-10-25T10:00:00Z",
        "type": "response_item",
        "payload": {
            "type": "message",
            "role": "assistant",
            "content": [{"type": "output_text", "text": text}]
        },
        "seq": i
    })
    .to_string()
}

fn write_lines(path: &Path, lines: &[String]) {
    let mut body = lines.join("\n");
    body.push('\n');
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("fixture parent dir");
    }
    fs::write(path, body).expect("write fixture file");
}

fn write_zst(path: &Path, text: &str) {
    let encoded = zstd::stream::encode_all(text.as_bytes(), 3).expect("zstd encode");
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("fixture parent dir");
    }
    fs::write(path, encoded).expect("write zst fixture");
}

/// One synthetic three-source world plus its archive and cache roots.
struct World {
    env: TestEnv,
    archive_root: PathBuf,
    cache_root: PathBuf,
    claude_root: PathBuf,
    claude_b: PathBuf,
    codex_root: PathBuf,
    codex_a: PathBuf,
    opencode_root: PathBuf,
    opencode_db: PathBuf,
}

fn build_world(name: &str) -> World {
    let env = TestEnv::new();
    let archive_root = env.root(name);
    Archive::init(&archive_root).expect("init runtime root");
    let cache_root = env.root("cache");

    // Claude Code: two transcripts, each its own session.
    let claude_root = env.root("claude-projects");
    let project = claude_root.join("proj");
    let claude_a = project.join("aaaaaaaa-1111-4111-8111-111111111111.jsonl");
    let claude_b = project.join("bbbbbbbb-2222-4222-8222-222222222222.jsonl");
    write_lines(
        &claude_a,
        &[claude_line(
            "cla-ses-a",
            "u-a1",
            "live claude session 只在活源 needleclaudelive",
        )],
    );
    write_lines(&claude_b, &[claude_line("cla-ses-b", "u-b1", CLAUDE_TEXT_B)]);

    // Codex: one plain rollout, one compressed-only rollout.
    let codex_root = env.root("codex-home");
    let codex_a = codex_root
        .join("sessions")
        .join("2026")
        .join("10")
        .join("25")
        .join("rollout-a.jsonl");
    write_lines(
        &codex_a,
        &[codex_meta("codex-ses-a"), codex_message(1, CODEX_TEXT_A)],
    );
    write_zst(
        &codex_root
            .join("sessions")
            .join("2026")
            .join("10")
            .join("24")
            .join("rollout-b.jsonl.zst"),
        &format!(
            "{}\n{}\n",
            codex_meta("codex-ses-b"),
            codex_message(1, CODEX_TEXT_B_ZST)
        ),
    );

    // OpenCode: a V2 database that also carries V1 tables (both generations
    // coexist in the real database; V2 wins the probe).
    let opencode_root = env.root("opencode-home");
    let opencode_db = opencode_root.join("opencode.db");
    {
        let conn = rusqlite::Connection::open(&opencode_db).expect("opencode db");
        conn.execute_batch(support::V1_SCHEMA).expect("v1 tables");
        conn.execute_batch(support::V2_SCHEMA).expect("v2 tables");
        for (session, message, text) in [
            (
                "oc-ses-a",
                "sm-a1",
                "opencode V2 活源会话 needleopencodelive",
            ),
            ("oc-ses-b", "sm-b1", OPENCODE_TEXT_B),
        ] {
            conn.execute(
                "INSERT INTO session_v2 (id, title, directory, time_created, time_updated, parent_id) \
                 VALUES (?1, ?2, 'C:/tmp', 1, 2, NULL)",
                rusqlite::params![session, session],
            )
            .expect("insert session");
            conn.execute(
                "INSERT INTO session_message (id, session_id, seq, type, time_created, data) \
                 VALUES (?1, ?2, 1, 'user', 3, ?3)",
                rusqlite::params![message, session, json!({"text": text}).to_string()],
            )
            .expect("insert message");
        }
        // ses-both: the V2 side of a migrated session (live; the live index
        // covers it).
        conn.execute(
            "INSERT INTO session_v2 (id, title, directory, time_created, time_updated, parent_id)              VALUES ('ses-both', 'both', 'C:/tmp', 1, 2, NULL)",
            [],
        )
        .expect("insert both session");
        conn.execute(
            "INSERT INTO session_message (id, session_id, seq, type, time_created, data)              VALUES ('sm-both', 'ses-both', 1, 'user', 3, ?1)",
            rusqlite::params![json!({"text": "both gen 会话 needleopencodebothv2"}).to_string()],
        )
        .expect("insert both message");

        // V1 rows: one V1-only session (never migrated, invisible to the live
        // V2 index) with text and tool parts, plus the V1 copy of a session
        // that ALSO exists in V2 (migrated; V2 must win).
        for (sql, params) in [
            (
                "INSERT INTO session (id, parent_id, title, time_created, time_updated) \
                 VALUES (?1, NULL, ?2, 10, 20)",
                rusqlite::params!["v1-solo", "v1 only session"],
            ),
            (
                "INSERT INTO message (id, session_id, time_created, time_updated, data) \
                 VALUES (?1, 'v1-solo', 30, 40, ?2)",
                rusqlite::params![
                    "vm-1",
                    json!({"role": "user", "id": "vm-1"}).to_string()
                ],
            ),
            (
                "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data) \
                 VALUES (?1, 'vm-1', 'v1-solo', 30, 40, ?2)",
                rusqlite::params![
                    "vp-1",
                    json!({"type": "text", "text": "V1-only 会话 needleopencodevone"}).to_string()
                ],
            ),
            (
                "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data) \
                 VALUES (?1, 'vm-1', 'v1-solo', 31, 41, ?2)",
                rusqlite::params![
                    "vp-2",
                    json!({"type": "tool", "tool": "bash",
                           "state": {"input": {"cmd": "ls"},
                                     "output": "v1 tool 输出 needleopencodevtwo"}}).to_string()
                ],
            ),
            (
                "INSERT INTO session (id, parent_id, title, time_created, time_updated) \
                 VALUES ('ses-both', NULL, 'both', 10, 20)",
                &[],
            ),
            (
                "INSERT INTO message (id, session_id, time_created, time_updated, data) \
                 VALUES ('vm-both', 'ses-both', 30, 40, ?1)",
                rusqlite::params![json!({"role": "user"}).to_string()],
            ),
            (
                "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data) \
                 VALUES ('vp-both', 'vm-both', 'ses-both', 30, 40, ?1)",
                rusqlite::params![
                    json!({"type": "text", "text": "V1 副本 needleopencodebothv1"}).to_string()
                ],
            ),
        ] {
            conn.execute(sql, params).expect("insert v1 fixture row");
        }
    }

    World {
        env,
        archive_root,
        cache_root,
        claude_root,
        claude_b,
        codex_root,
        codex_a,
        opencode_root,
        opencode_db,
    }
}

impl World {
    fn config_value(&self, archive_enabled: bool, with_archive: bool, embedding: Option<Value>) -> Value {
        let mut sources = vec![
            json!({"source_id": "claude_code", "kind": "claude_code", "root": self.claude_root.to_string_lossy(), "enabled": true}),
            json!({"source_id": "codex", "kind": "codex", "root": self.codex_root.to_string_lossy(), "enabled": true}),
            json!({"source_id": "opencode", "kind": "opencode", "root": self.opencode_root.to_string_lossy(), "enabled": true}),
        ];
        let mut order = vec!["claude_code", "codex", "opencode"];
        if with_archive {
            sources.push(json!({
                "source_id": "archive",
                "kind": "archive",
                "root": self.archive_root.to_string_lossy(),
                "enabled": archive_enabled
            }));
            // v2 validation: source_order covers the *enabled* source ids only;
            // an archive source kept for capture (enabled:false) stays outside.
            if archive_enabled {
                order.push("archive");
            }
        }
        let mut value = json!({
            "version": 2,
            "sources": sources,
            "source_order": order,
            "interpreter": {"kb": {"command": "C:/python/python.exe"}},
            "domain": "default",
            "telemetry": {"enabled": false},
            "cache_root": self.cache_root.to_string_lossy()
        });
        if let Some(spec) = embedding {
            value["interpreter"]["embedding"] = spec;
        }
        value
    }

    fn write_config(&self, value: &Value) -> PathBuf {
        self.env.write_json("recall.json", value)
    }

    fn capture_all(&self, config: &Path) {
        self.capture_all_with_root(config, None)
    }

    fn capture_all_with_root(&self, config: &Path, root: Option<&Path>) {
        let parsed = load_config(config).expect("config loads");
        let options = agent_memory::capture_all::CaptureAllOptions {
            root: root.map(|path| path.to_string_lossy().to_string()),
            domain: "default".to_string(),
            only_source: None,
            session_since: None,
            plan: false,
            skip_if_locked: false,
        };
        let (report, code) =
            agent_memory::capture_all::capture_all(&parsed, &options).expect("capture-all runs");
        assert_eq!(
            code, 0,
            "capture-all failed: {}",
            serde_json::to_string(&report).unwrap()
        );
        assert_eq!(report.ok, true, "capture-all not ok");
    }

    fn index(&self, config: &Path) -> Vec<Value> {
        let parsed = load_config(config).expect("config loads");
        let reports = index_all(&parsed, None, false).expect("index runs");
        reports
            .iter()
            .map(|report| serde_json::to_value(report).expect("report json"))
            .collect()
    }

    fn search(&self, config: &Path, request: Value) -> Value {
        let parsed = load_config(config).expect("config loads");
        let request = SearchRequest::parse(&request).expect("request parses");
        let response = memory_search(&parsed, &request);
        assert_eq!(response.exit_code, 0, "search failed");
        serde_json::to_value(&response).expect("response json")
    }

    fn read(&self, config: &Path, reference: &Value) -> Value {
        let parsed = load_config(config).expect("config loads");
        let request =
            ReadRequest::parse(&json!({"ref": reference, "max_bytes": 65536})).expect("read parses");
        let response = memory_read(&parsed, &request);
        assert_eq!(response.exit_code, 0, "read failed");
        serde_json::to_value(&response).expect("response json")
    }

    /// Same, with a context window — the archive-fallback readback must place
    /// the hit inside its session's part list exactly like a live ref (the
    /// coordinate comparison ignores the `archive_fallback` marker).
    fn read_with_context(&self, config: &Path, reference: &Value, before: i64, after: i64) -> Value {
        let parsed = load_config(config).expect("config loads");
        let request = ReadRequest::parse(&json!({
            "ref": reference,
            "max_bytes": 65536,
            "context_before": before,
            "context_after": after,
        }))
        .expect("read parses");
        let response = memory_read(&parsed, &request);
        assert_eq!(response.exit_code, 0, "context read failed");
        serde_json::to_value(&response).expect("response json")
    }

    fn index_report<'a>(&'a self, reports: &'a [Value], source_id: &str) -> &'a Value {
        reports
            .iter()
            .find(|report| report["source_id"] == json!(source_id))
            .unwrap_or_else(|| panic!("source {source_id} missing from index report"))
    }

    /// Deterministic hash over every file under `root` (path + bytes).
    fn tree_hash(root: &Path) -> u64 {
        let mut paths: Vec<PathBuf> = Vec::new();
        fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
            for entry in fs::read_dir(dir).expect("read_dir").flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, out);
                } else {
                    out.push(path);
                }
            }
        }
        walk(root, &mut paths);
        paths.sort();
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for path in paths {
            let rel = path.strip_prefix(root).expect("relative").to_string_lossy();
            for byte in rel.as_bytes() {
                hash = (hash ^ *byte as u64).wrapping_mul(0x1000_0000_01b3);
            }
            let bytes = fs::read(&path).expect("read for hash");
            for byte in bytes {
                hash = (hash ^ byte as u64).wrapping_mul(0x1000_0000_01b3);
            }
        }
        hash
    }
}

fn hits_of(response: &Value) -> Vec<&Value> {
    response["results"]
        .as_array()
        .expect("results")
        .iter()
        .collect()
}

fn hit_by_needle<'a>(response: &'a Value, needle: &str) -> &'a Value {
    hits_of(response)
        .into_iter()
        .find(|hit| hit["snippet"].as_str().is_some_and(|text| text.contains(needle)))
        .unwrap_or_else(|| panic!("no hit mentions {needle}: {response}"))
}

fn first_ref(hit: &Value) -> &Value {
    &hit["ref"]
}

fn embedding() -> Value {
    json!({
        "model_id": "fake-hash-v1",
        "dimension": 32,
        "normalization": "l2",
        "backend": "fake"
    })
}

// ---------------------------------------------------------------------------
// 1. Claude: deleted transcript answered from the archive (exact + hybrid +
//    byte-exact readback); the live session stays origin=live.
// ---------------------------------------------------------------------------

#[test]
fn claude_deleted_session_is_indexed_from_archive_and_reads_back() {
    let world = build_world("s5-claude");
    let config = world.write_config(&world.config_value(false, true, Some(embedding())));
    world.capture_all(&config);

    // The session disappears from the live source (Claude cleanup).
    fs::remove_file(&world.claude_b).expect("delete live transcript");
    let reports = world.index(&config);
    let claude = world.index_report(&reports, "claude_code");
    assert_eq!(claude["detail"]["archive"]["status"], "ok");
    assert_eq!(claude["detail"]["archive"]["archive_only_sessions"], 1);

    // Exact: the deleted session is hit, marked origin=archive.
    let response = world.search(
        &config,
        json!({"query": "needleclaudearchive", "mode": "exact", "sources": ["claude_code"], "limit": 10}),
    );
    let hit = hit_by_needle(&response, "needleclaudearchive");
    assert_eq!(hit["origin"], "archive");
    assert_eq!(hit["ref"]["locator"]["session_id"], "cla-ses-b");

    // The live session is marked origin=live.
    let response = world.search(
        &config,
        json!({"query": "needleclaudelive", "mode": "exact", "sources": ["claude_code"], "limit": 10}),
    );
    let hit = hit_by_needle(&response, "needleclaudelive");
    assert_eq!(hit["origin"], "live");

    // Default mode (auto → hybrid with the fake embedding) finds it too.
    let response = world.search(
        &config,
        json!({"query": "needleclaudearchive", "sources": ["claude_code"], "limit": 10}),
    );
    let hit = hit_by_needle(&response, "needleclaudearchive");
    assert_eq!(hit["origin"], "archive");

    // Readback is byte-exact against the original record projection, and the
    // range fingerprint check passes (memory_read verifies it fail-closed).
    let read = world.read(&config, first_ref(hit));
    assert_eq!(read["text"], json!(CLAUDE_TEXT_B));
    assert_eq!(read["range_sha256"], hit["ref"]["fingerprint"]["range_sha256"]);

    // Context window across the splice boundary works for an archive ref
    // (rework 3: the position check ignores the archive_fallback marker).
    let context_read = world.read_with_context(&config, first_ref(hit), 1, 1);
    let context_text: String = context_read["segments"]
        .as_array()
        .expect("segments")
        .iter()
        .map(|segment| segment["text"].as_str().unwrap_or_default())
        .collect::<Vec<_>>()
        .join("");
    assert!(
        context_text.contains(CLAUDE_TEXT_B),
        "context read must contain the referenced text"
    );

    // Coverage carries the fallback facts.
    let response = world.search(
        &config,
        json!({"query": "needleclaudearchive", "mode": "exact", "sources": ["claude_code"], "limit": 10}),
    );
    let detail = &response["source_status"][0]["detail"];
    assert_eq!(detail["coverage"]["archive_only_sessions"], 1);
    assert_eq!(
        detail["coverage"]["archive_root"],
        json!(world.archive_root.to_string_lossy())
    );
}

// ---------------------------------------------------------------------------
// 2. Codex: a deleted plain rollout and a compressed-only rollout both come
//    back from the archive, exact and readable.
// ---------------------------------------------------------------------------

#[test]
fn codex_deleted_session_comes_from_archive_and_compressed_stays_live() {
    let world = build_world("s5-codex");
    let config = world.write_config(&world.config_value(false, true, None));
    world.capture_all(&config);

    fs::remove_file(&world.codex_a).expect("delete live rollout");
    let reports = world.index(&config);
    let codex = world.index_report(&reports, "codex");
    // codex-ses-a is gone from the live source; codex-ses-b still exists as a
    // `.jsonl.zst` carrier and is indexed live (a compression is not a deletion).
    assert_eq!(codex["detail"]["archive"]["archive_only_sessions"], 1);

    let deleted = world.search(
        &config,
        json!({"query": "needlecodexarchive", "mode": "exact", "sources": ["codex"], "limit": 10}),
    );
    let deleted_hit = hit_by_needle(&deleted, "needlecodexarchive");
    assert_eq!(deleted_hit["origin"], "archive");
    let read = world.read(&config, first_ref(deleted_hit));
    let text = read["text"].as_str().expect("readback text");
    assert!(text.contains("needlecodexarchive"), "readback must contain the needle: {text}");

    // The compressed session is answered by the live source, not the archive.
    let compressed = world.search(
        &config,
        json!({"query": "needlecodexzst", "mode": "exact", "sources": ["codex"], "limit": 10}),
    );
    let compressed_hit = hit_by_needle(&compressed, "needlecodexzst");
    assert_eq!(compressed_hit["origin"], "live");
    let read = world.read(&config, first_ref(compressed_hit));
    let text = read["text"].as_str().expect("readback text");
    assert!(text.contains("needlecodexzst"), "readback must contain the needle: {text}");
}

// ---------------------------------------------------------------------------
// 3. OpenCode: a session deleted from the V2 database (rows removed, V1 tables
//    untouched) is answered from the evidence; the SQLite delta record `read`
//    fix (N5) works on the same archive.
// ---------------------------------------------------------------------------

#[test]
fn opencode_deleted_session_and_sqlite_record_read() {
    let world = build_world("s5-opencode");
    let config = world.write_config(&world.config_value(false, true, Some(embedding())));
    // First capture: the baseline of the whole database.
    world.capture_all(&config);

    // A second capture records a delta batch (new rows for session A) — the
    // evidence shape behind the `read --sha256 <record>` defect.
    {
        let conn = rusqlite::Connection::open(&world.opencode_db).expect("reopen db");
        conn.execute(
            "INSERT INTO session_message (id, session_id, seq, type, time_created, data) \
             VALUES ('sm-a2', 'oc-ses-a', 2, 'user', 9, ?1)",
            rusqlite::params![json!({"text": "opencode V2 增量 needleopencodelive"}).to_string()],
        )
        .expect("insert delta message");
    }
    world.capture_all(&config);

    // The session then disappears from the live source (row deletion, like the
    // real OpenCode cleanup) and the deletion itself is captured: the delete
    // delta carries the full `before` rows, so the mirror keeps the
    // pre-deletion state and the session stays recoverable.
    {
        let conn = rusqlite::Connection::open(&world.opencode_db).expect("reopen db");
        conn.execute("DELETE FROM session_message WHERE session_id = 'oc-ses-b'", [])
            .expect("delete messages");
        conn.execute("DELETE FROM session_v2 WHERE id = 'oc-ses-b'", [])
            .expect("delete session");
    }
    world.capture_all(&config);

    let reports = world.index(&config);
    let opencode = world.index_report(&reports, "opencode");
    assert_eq!(opencode["detail"]["archive"]["status"], "ok");
    // oc-ses-b (deleted, kept) + v1-solo (V1-only). ses-both and oc-ses-a are
    // live, so they are not counted.
    assert_eq!(opencode["detail"]["archive"]["archive_only_sessions"], 2);

    // Rework 1: the deletion was captured (a delete delta is committed), yet
    // the session stays recoverable: the fallback serves its pre-deletion
    // state with origin=archive and the deletion timestamp.
    for mode in ["exact", "hybrid"] {
        let response = world.search(
            &config,
            json!({"query": "needleopencodearchive", "mode": mode, "sources": ["opencode"], "limit": 10}),
        );
        let hit = hit_by_needle(&response, "needleopencodearchive");
        assert_eq!(hit["origin"], "archive", "mode {mode}");
        assert_eq!(hit["ref"]["locator"]["session_id"], "oc-ses-b");
        assert!(
            hit.get("deleted_at").is_some_and(|value| value.is_string()),
            "deleted session hit must carry deleted_at: {hit}"
        );
        let read = world.read(&config, first_ref(hit));
        assert_eq!(read["text"], json!(OPENCODE_TEXT_B));
    }

    // deleted_at equals the deleting delta's observation time (ISO, second
    // precision, UTC).
    let archive = Archive::open(&world.archive_root).expect("open archive");
    let delete_observed_at_ms = archive
        .manifests()
        .expect("manifests")
        .iter()
        .filter_map(|manifest| manifest.source.sqlite.as_ref())
        .filter(|sqlite| sqlite.capture_seq >= 3)
        .map(|sqlite| sqlite.observed_at_ms)
        .min()
        .expect("delete delta batch exists");
    let response = world.search(
        &config,
        json!({"query": "needleopencodearchive", "mode": "exact", "sources": ["opencode"], "limit": 10}),
    );
    let hit = hit_by_needle(&response, "needleopencodearchive");
    let expected = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(delete_observed_at_ms)
        .expect("valid stamp")
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    assert_eq!(hit["deleted_at"], json!(expected));

    // The session re-created in the live source under the same id wins.
    {
        let conn = rusqlite::Connection::open(&world.opencode_db).expect("reopen db");
        conn.execute(
            "INSERT INTO session_v2 (id, title, directory, time_created, time_updated, parent_id) \
             VALUES ('oc-ses-b', 'recreated', 'C:/tmp', 50, 60, NULL)",
            [],
        )
        .expect("recreate session");
        conn.execute(
            "INSERT INTO session_message (id, session_id, seq, type, time_created, data) \
             VALUES ('sm-b2', 'oc-ses-b', 1, 'user', 61, ?1)",
            rusqlite::params![json!({"text": OPENCODE_TEXT_B}).to_string()],
        )
        .expect("recreate message");
    }
    world.index(&config);
    let response = world.search(
        &config,
        json!({"query": "needleopencodearchive", "mode": "exact", "sources": ["opencode"], "limit": 20}),
    );
    let hits = hits_of(&response);
    assert_eq!(hits.len(), 1, "one session, one hit after recreation");
    assert_eq!(hits[0]["origin"], "live");
    assert!(hits[0].get("deleted_at").is_none(), "live hit has no deleted_at");

    // The live session is untouched.
    let response = world.search(
        &config,
        json!({"query": "needleopencodelive", "mode": "exact", "sources": ["opencode"], "limit": 10}),
    );
    let hit = hit_by_needle(&response, "needleopencodelive");
    assert_eq!(hit["origin"], "live");

    // N5: `agent-memory read --sha256 <record>` resolves an OpenCode SQLite
    // delta record (the offset addresses the decompressed batch payload).
    let archive = Archive::open(&world.archive_root).expect("open archive");
    let manifests = archive.manifests().expect("manifests");
    let mut delta_record = None;
    for manifest in manifests.iter() {
        if manifest.source.kind == "sqlite" && !manifest.records.is_empty() {
            delta_record = Some(manifest.records[0].clone());
        }
    }
    let record = delta_record.expect("sqlite delta records exist after the deletion capture");
    let out_dir = world.env.root("read-out");
    let out_file = out_dir.join("record.json");
    let cli = std::process::Command::new(env!("CARGO_BIN_EXE_agent-memory"))
        .args([
            "read",
            "--root",
            &world.archive_root.to_string_lossy(),
            "--domain",
            "default",
            "--sha256",
            &record.sha256,
            "--out",
            &out_file.to_string_lossy(),
        ])
        .output()
        .expect("run agent-memory read");
    assert!(
        cli.status.success(),
        "read --sha256 failed: {}",
        String::from_utf8_lossy(&cli.stderr)
    );
    let bytes = fs::read(&out_file).expect("record bytes");
    assert_eq!(
        agent_memory::archive::hex_sha256(&bytes),
        record.sha256,
        "record readback is hash-verified"
    );
    let line: Value = serde_json::from_slice(&bytes).expect("delta line is JSON");
    assert_eq!(line["table"], json!("session_message"));
}

// ---------------------------------------------------------------------------
// 3b. V1-only sessions (never migrated to V2, invisible to the live index)
//     are covered by the fallback; a V1/V2 same-id session is served from V2
//     only.
// ---------------------------------------------------------------------------

#[test]
fn opencode_v1_only_sessions_are_covered() {
    let world = build_world("s5-opencode-v1");
    let config = world.write_config(&world.config_value(false, true, Some(embedding())));
    world.capture_all(&config);
    let reports = world.index(&config);
    let opencode = world.index_report(&reports, "opencode");
    assert_eq!(opencode["detail"]["archive"]["status"], "ok");
    // v1-solo is the only never-migrated session without a live V2 row;
    // ses-both and oc-ses-a are live.
    assert_eq!(opencode["detail"]["archive"]["archive_only_sessions"], 1);

    // V1-only text: hit from the evidence, origin=archive, V1 locator.
    let response = world.search(
        &config,
        json!({"query": "needleopencodevone", "mode": "exact", "sources": ["opencode"], "limit": 10}),
    );
    let hit = hit_by_needle(&response, "needleopencodevone");
    assert_eq!(hit["origin"], "archive");
    assert_eq!(hit["ref"]["schema_gen"], "sqlite-v1");
    assert_eq!(hit["ref"]["locator"]["session_id"], "v1-solo");
    assert_eq!(hit["ref"]["locator"]["message_id"], "vm-1");
    assert_eq!(hit["ref"]["locator"]["part_id"], "vp-1");
    let read = world.read(&config, first_ref(hit));
    assert_eq!(read["text"], json!("V1-only 会话 needleopencodevone"));

    // V1-only tool output: separate channel, same unit shape as V2, readable.
    let response = world.search(
        &config,
        json!({"query": "needleopencodevtwo", "mode": "exact", "sources": ["opencode"], "limit": 10}),
    );
    let hit = hit_by_needle(&response, "needleopencodevtwo");
    assert_eq!(hit["origin"], "archive");
    assert_eq!(hit["kind"], "tool_output");
    assert_eq!(hit["ref"]["locator"]["part_id"], "vp-2");
    let read = world.read(&config, first_ref(hit));
    assert!(
        read["text"]
            .as_str()
            .is_some_and(|text| text.contains("needleopencodevtwo")),
        "tool output readback: {read}"
    );

    // The V1 copy of a migrated session never surfaces: the session answers
    // from its V2 (live) rows only.
    let response = world.search(
        &config,
        json!({"query": "needleopencodebothv2", "mode": "exact", "sources": ["opencode"], "limit": 10}),
    );
    let hit = hit_by_needle(&response, "needleopencodebothv2");
    assert_eq!(hit["origin"], "live");
    let response = world.search(
        &config,
        json!({"query": "needleopencodebothv1", "mode": "exact", "sources": ["opencode"], "limit": 10}),
    );
    assert!(
        hits_of(&response).is_empty(),
        "the V1 copy of a migrated session must not be indexed: {response}"
    );

    // Hybrid covers the V1-only session too.
    let response = world.search(
        &config,
        json!({"query": "needleopencodevone", "sources": ["opencode"], "limit": 10}),
    );
    let hit = hit_by_needle(&response, "needleopencodevone");
    assert_eq!(hit["origin"], "archive");
}

// ---------------------------------------------------------------------------
// 3c. Deleting only some child rows of a live session never mixes the
//     deletion back in: the session stays live-first (no archive rows, no
//     deleted_at).
// ---------------------------------------------------------------------------

#[test]
fn opencode_partial_child_deletion_keeps_live_priority() {
    let world = build_world("s5-opencode-partial");
    let config = world.write_config(&world.config_value(false, true, Some(embedding())));
    world.capture_all(&config);

    // Delete one child row of a session that stays in the live source, then
    // capture: the delete delta is committed, but the session itself is still
    // live, so the fallback must not touch it at all.
    {
        let conn = rusqlite::Connection::open(&world.opencode_db).expect("reopen db");
        conn.execute("DELETE FROM session_message WHERE id = 'sm-a1'", [])
            .expect("delete one message");
    }
    world.capture_all(&config);
    world.index(&config);

    let reports = world.index(&config);
    let opencode = world.index_report(&reports, "opencode");
    assert_eq!(opencode["detail"]["archive"]["status"], "ok");
    // Only the never-migrated V1 session is archive-only; oc-ses-a stays live
    // (its session_v2 row exists even though its only message was deleted).
    assert_eq!(opencode["detail"]["archive"]["archive_only_sessions"], 1);

    let response = world.search(
        &config,
        json!({"query": "needleopencodelive", "mode": "exact", "sources": ["opencode"], "limit": 10}),
    );
    for hit in hits_of(&response) {
        assert_eq!(hit["origin"], "live", "live session stays live: {hit}");
        assert!(
            hit.get("deleted_at").is_none(),
            "a live session carries no deleted_at: {hit}"
        );
    }
}

// ---------------------------------------------------------------------------
// 3d. Rework 2 / B1: an incremental run must notice deletions even when the
//     total row count does not drop (other sessions grow).
// ---------------------------------------------------------------------------

#[test]
fn opencode_incremental_session_deletion_with_growth() {
    let world = build_world("s5-r2-del-session");
    let config = world.write_config(&world.config_value(false, true, Some(embedding())));
    world.capture_all(&config);
    world.index(&config);
    let response = world.search(
        &config,
        json!({"query": "needleopencodearchive", "mode": "exact", "sources": ["opencode"], "limit": 10}),
    );
    assert_eq!(hit_by_needle(&response, "needleopencodearchive")["origin"], "live");

    // Delete oc-ses-b while oc-ses-a grows: the total row count rises, so the
    // old full-rebuild trigger never fires.
    {
        let conn = rusqlite::Connection::open(&world.opencode_db).expect("reopen db");
        conn.execute("DELETE FROM session_message WHERE session_id = 'oc-ses-b'", [])
            .expect("delete messages");
        conn.execute("DELETE FROM session_v2 WHERE id = 'oc-ses-b'", [])
            .expect("delete session");
        for (id, seq) in [("sm-a2", 2), ("sm-a3", 3)] {
            conn.execute(
                "INSERT INTO session_message (id, session_id, seq, type, time_created, data) \
                 VALUES (?1, 'oc-ses-a', ?2, 'user', 9, ?3)",
                rusqlite::params![
                    id,
                    seq,
                    json!({"text": format!("opencode V2 growth {id}")}).to_string()
                ],
            )
            .expect("insert growth message");
        }
    }
    world.capture_all(&config);
    let reports = world.index(&config);
    let opencode = world.index_report(&reports, "opencode");
    assert_eq!(opencode["status"], "updated");
    // oc-ses-b (deleted, kept) + v1-solo (V1-only).
    assert_eq!(opencode["detail"]["archive"]["archive_only_sessions"], 2);

    // The deleted session is served from the archive, not as a dead live hit.
    let response = world.search(
        &config,
        json!({"query": "needleopencodearchive", "mode": "exact", "sources": ["opencode"], "limit": 10}),
    );
    let hit = hit_by_needle(&response, "needleopencodearchive");
    assert_eq!(hit["origin"], "archive");
    assert!(hit.get("deleted_at").is_some());
    let read = world.read(&config, first_ref(hit));
    assert_eq!(read["text"], json!(OPENCODE_TEXT_B));

    // The growing session stays live, its new rows are indexed.
    let response = world.search(
        &config,
        json!({"query": "growth sm-a3", "mode": "exact", "sources": ["opencode"], "limit": 10}),
    );
    let hit = hit_by_needle(&response, "sm-a3");
    assert_eq!(hit["origin"], "live");
}

#[test]
fn opencode_incremental_child_row_deletion() {
    let world = build_world("s5-r2-del-child");
    let config = world.write_config(&world.config_value(false, true, Some(embedding())));
    world.capture_all(&config);
    world.index(&config);

    // Delete one child row of a live session while another session grows: the
    // total row count stays constant.
    {
        let conn = rusqlite::Connection::open(&world.opencode_db).expect("reopen db");
        conn.execute("DELETE FROM session_message WHERE id = 'sm-a1'", [])
            .expect("delete one message");
        conn.execute(
            "INSERT INTO session_message (id, session_id, seq, type, time_created, data) \
             VALUES ('sm-b2', 'oc-ses-b', 2, 'user', 9, ?1)",
            rusqlite::params![json!({"text": "oc-ses-b growth row"}).to_string()],
        )
        .expect("insert growth message");
    }
    world.capture_all(&config);
    let reports = world.index(&config);
    let opencode = world.index_report(&reports, "opencode");
    assert_eq!(opencode["status"], "updated");
    // The live session stays live; only the never-migrated V1 session is
    // archive-only.
    assert_eq!(opencode["detail"]["archive"]["archive_only_sessions"], 1);

    // The deleted row is gone from the index entirely: no dead live hit.
    let response = world.search(
        &config,
        json!({"query": "needleopencodelive", "mode": "exact", "sources": ["opencode"], "limit": 10}),
    );
    assert!(
        hits_of(&response).is_empty(),
        "the deleted row must not be returned at all: {response}"
    );
    // Coverage counts the live sessions only (N4, strict pin: the archive
    // session is reported separately, so 3 live ≠ 4 archived − live union).
    let detail = &response["source_status"][0]["detail"];
    let sessions = detail["coverage"]["sessions"].as_u64().expect("sessions count");
    assert_eq!(sessions, 3, "live sessions counted, archive excluded: {detail}");
}

// ---------------------------------------------------------------------------
// 3e. Rework 2 / B2: a session that returns to the live source with its
//     original (pre-watermark) timestamps switches back to origin=live.
// ---------------------------------------------------------------------------

#[test]
fn opencode_return_to_live_with_old_timestamps() {
    let world = build_world("s5-r2-return");
    let config = world.write_config(&world.config_value(false, true, Some(embedding())));
    world.capture_all(&config);
    world.index(&config);

    // Delete + capture + index: the session goes archive-only.
    {
        let conn = rusqlite::Connection::open(&world.opencode_db).expect("reopen db");
        conn.execute("DELETE FROM session_message WHERE session_id = 'oc-ses-b'", [])
            .expect("delete messages");
        conn.execute("DELETE FROM session_v2 WHERE id = 'oc-ses-b'", [])
            .expect("delete session");
    }
    world.capture_all(&config);
    world.index(&config);
    let response = world.search(
        &config,
        json!({"query": "needleopencodearchive", "mode": "exact", "sources": ["opencode"], "limit": 10}),
    );
    assert_eq!(hit_by_needle(&response, "needleopencodearchive")["origin"], "archive");

    // Restore the original rows (time_created = 3, at/below the watermark).
    {
        let conn = rusqlite::Connection::open(&world.opencode_db).expect("reopen db");
        conn.execute(
            "INSERT INTO session_v2 (id, title, directory, time_created, time_updated, parent_id) \
             VALUES ('oc-ses-b', 'restored', 'C:/tmp', 1, 2, NULL)",
            [],
        )
        .expect("restore session");
        conn.execute(
            "INSERT INTO session_message (id, session_id, seq, type, time_created, data) \
             VALUES ('sm-b1', 'oc-ses-b', 1, 'user', 3, ?1)",
            rusqlite::params![json!({"text": OPENCODE_TEXT_B}).to_string()],
        )
        .expect("restore message");
    }
    world.capture_all(&config);
    world.index(&config);

    let response = world.search(
        &config,
        json!({"query": "needleopencodearchive", "mode": "exact", "sources": ["opencode"], "limit": 20}),
    );
    let hits = hits_of(&response);
    assert_eq!(hits.len(), 1, "exactly one hit after the return: {response}");
    assert_eq!(hits[0]["origin"], "live");
    assert!(hits[0].get("deleted_at").is_none());
}

// ---------------------------------------------------------------------------
// 3f. Rework 2 / B3: a carrier holding two session ids, only one of which
//     disappears - the surviving session must never gain archive rows.
// ---------------------------------------------------------------------------

#[test]
fn multi_session_carrier_only_missing_session_archived() {
    let world = build_world("s5-r2-multi");
    let config = world.write_config(&world.config_value(false, true, None));
    let project = world.claude_root.join("proj");
    write_lines(
        &project.join("shared.jsonl"),
        &[
            claude_line("sha-ses-a", "u-sha1", "shared a needle multi-shared-a"),
            claude_line("sha-ses-b", "u-shb1", "shared b needle multi-shared-b"),
        ],
    );
    write_lines(
        &project.join("solo.jsonl"),
        &[claude_line("sha-ses-a", "u-solo1", "solo a needle multi-solo-a")],
    );
    world.capture_all(&config);
    world.index(&config);

    // The shared carrier disappears; sha-ses-a stays live via solo.jsonl.
    fs::remove_file(project.join("shared.jsonl")).expect("delete shared carrier");
    world.capture_all(&config);
    world.index(&config);

    // The missing session is archived.
    let response = world.search(
        &config,
        json!({"query": "multi-shared-b", "mode": "exact", "sources": ["claude_code"], "limit": 10}),
    );
    let hit = hit_by_needle(&response, "multi-shared-b");
    assert_eq!(hit["origin"], "archive");

    // The surviving session: its carrier copy is gone, its live copy remains -
    // exactly one hit, origin=live, and no archive row at all (B3 invariant).
    let response = world.search(
        &config,
        json!({"query": "multi-shared-a", "mode": "exact", "sources": ["claude_code"], "limit": 10}),
    );
    assert!(
        hits_of(&response).is_empty(),
        "the deleted carrier copy of a live session must not come back: {response}"
    );
    let response = world.search(
        &config,
        json!({"query": "multi-solo-a", "mode": "exact", "sources": ["claude_code"], "limit": 10}),
    );
    let hit = hit_by_needle(&response, "multi-solo-a");
    assert_eq!(hit["origin"], "live");

    // Direct index check: sha-ses-a must have no archive rows.
    let conn = rusqlite::Connection::open(
        world.cache_root.join("live").join("claude_code").join("fts.sqlite"),
    )
    .expect("open index");
    let mixed: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM doc_keys WHERE session_id = 'sha-ses-a' AND origin = 'archive'",
            [],
            |row| row.get(0),
        )
        .expect("count");
    assert_eq!(mixed, 0, "no archive rows for the surviving session");
}

// ---------------------------------------------------------------------------
// 3g. Rework 2 / B4: mirror build and mirror-missing readback stay in the
//     seconds range on a 50k-row corpus.
// ---------------------------------------------------------------------------

#[test]
fn opencode_row_mirror_scale_is_seconds() {
    let env = TestEnv::new();
    let archive_root = env.root("scale-archive");
    Archive::init(&archive_root).expect("init runtime root");
    let cache_root = env.root("cache");
    let opencode_root = env.root("opencode-scale");
    let opencode_db = opencode_root.join("opencode.db");
    {
        let conn = rusqlite::Connection::open(&opencode_db).expect("scale db");
        conn.execute_batch(support::V2_SCHEMA).expect("v2 tables");
        // 50 sessions x 1000 messages = 50 000 rows.
        conn.execute_batch("BEGIN").expect("begin");
        for session_index in 0..50u32 {
            let session = format!("ses-scale-{session_index:02}");
            conn.execute(
                "INSERT INTO session_v2 (id, title, directory, time_created, time_updated, parent_id) \
                 VALUES (?1, ?2, 'C:/tmp', 1, 2, NULL)",
                rusqlite::params![session, session],
            )
            .expect("insert session");
            for row_index in 0..1000u32 {
                conn.execute(
                    "INSERT INTO session_message (id, session_id, seq, type, time_created, data) \
                     VALUES (?1, ?2, ?3, 'user', 3, ?4)",
                    rusqlite::params![
                        format!("sm-{session_index:02}-{row_index:04}"),
                        session,
                        row_index,
                        json!({"text": format!("scale row {session_index} {row_index}")}).to_string()
                    ],
                )
                .expect("insert message");
            }
        }
        conn.execute_batch("COMMIT").expect("commit");
    }
    let value = json!({
        "version": 2,
        "sources": [
            {"source_id": "opencode", "kind": "opencode", "root": opencode_root.to_string_lossy(), "enabled": true},
            {"source_id": "archive", "kind": "archive", "root": archive_root.to_string_lossy(), "enabled": false}
        ],
        "source_order": ["opencode"],
        "interpreter": {"kb": {"command": "C:/python/python.exe"}},
        "domain": "default",
        "telemetry": {"enabled": false},
        "cache_root": cache_root.to_string_lossy()
    });
    let config = env.write_json("recall.json", &value);
    let parsed = load_config(&config).expect("config loads");
    let options = agent_memory::capture_all::CaptureAllOptions {
        root: None,
        domain: "default".to_string(),
        only_source: None,
        session_since: None,
        plan: false,
        skip_if_locked: false,
    };
    let (report, code) =
        agent_memory::capture_all::capture_all(&parsed, &options).expect("baseline capture");
    assert_eq!(code, 0);
    assert_eq!(report.ok, true);

    // Delete one session (its rows leave the live source) and capture the
    // delete delta; the other sessions keep the row count up.
    {
        let conn = rusqlite::Connection::open(&opencode_db).expect("reopen db");
        conn.execute("DELETE FROM session_message WHERE session_id = 'ses-scale-00'", [])
            .expect("delete messages");
        conn.execute("DELETE FROM session_v2 WHERE id = 'ses-scale-00'", [])
            .expect("delete session");
    }
    let (report, code) =
        agent_memory::capture_all::capture_all(&parsed, &options).expect("delta capture");
    assert_eq!(code, 0);
    assert_eq!(report.ok, true);

    // First index: builds the row mirror (50k rows).
    let started = std::time::Instant::now();
    let reports = index_all(&parsed, None, false).expect("index runs");
    let index_ms = started.elapsed().as_millis() as u64;
    let opencode = reports
        .iter()
        .find(|report| report.source_id == "opencode")
        .expect("opencode report");
    assert_eq!(opencode.status, "built");
    eprintln!("B4 scale: first index (mirror build) = {index_ms} ms");

    let response = {
        let request = SearchRequest::parse(&json!(
            {"query": "\"scale row 0 500\"", "mode": "exact", "sources": ["opencode"], "limit": 10}
        ))
        .expect("request parses");
        let response = memory_search(&parsed, &request);
        assert_eq!(response.exit_code, 0);
        serde_json::to_value(&response).unwrap()
    };
    let hit = hit_by_needle(&response, "scale row 0 500");
    assert_eq!(hit["origin"], "archive");
    assert!(hit.get("deleted_at").is_some());
    let reference = first_ref(hit).clone();

    // Mirror-missing readback: materializes the delete-keeping state into a
    // temporary database.
    fs::remove_dir_all(cache_root.join("archive-inventory").join("rows"))
        .expect("remove mirror");
    let read_started = std::time::Instant::now();
    let request =
        ReadRequest::parse(&json!({"ref": reference, "max_bytes": 65536})).expect("read parses");
    let outcome = memory_read(&parsed, &request);
    let read_ms = read_started.elapsed().as_millis() as u64;
    assert_eq!(outcome.exit_code, 0, "readback failed");
    eprintln!("B4 scale: mirror-missing readback = {read_ms} ms");

    // Seconds, not minutes (verifier measured 363 s / 41.7 s before).
    assert!(index_ms < 60_000, "first index took {index_ms} ms");
    assert!(read_ms < 60_000, "mirror-missing readback took {read_ms} ms");
}

// ---------------------------------------------------------------------------
// 4. Identity: a session on both sides appears once (origin=live), and a
//    session returning to the live source replaces its archive rows.
// ---------------------------------------------------------------------------

#[test]
fn session_on_both_sides_appears_once_and_replacement_replaces() {
    let world = build_world("s5-dedupe");
    let config = world.write_config(&world.config_value(false, true, Some(embedding())));
    world.capture_all(&config);

    // Archive-only first.
    fs::remove_file(&world.claude_b).expect("delete live transcript");
    world.index(&config);
    let response = world.search(
        &config,
        json!({"query": "needleclaudearchive", "mode": "exact", "sources": ["claude_code"], "limit": 20}),
    );
    let hits = hits_of(&response);
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0]["origin"], "archive");

    // The session returns to the live source (same content).
    write_lines(&world.claude_b, &[claude_line("cla-ses-b", "u-b1", CLAUDE_TEXT_B)]);
    world.index(&config);
    let response = world.search(
        &config,
        json!({"query": "needleclaudearchive", "mode": "exact", "sources": ["claude_code"], "limit": 20}),
    );
    let hits = hits_of(&response);
    assert_eq!(hits.len(), 1, "one session, one hit: {response}");
    assert_eq!(hits[0]["origin"], "live");

    // The archive-only count follows the replacement.
    let reports = world.index(&config);
    let claude = world.index_report(&reports, "claude_code");
    assert_eq!(claude["detail"]["archive"]["archive_only_sessions"], 0);
}

// ---------------------------------------------------------------------------
// 5. An appended (append_range) Claude session reads back completely, with
//    context spanning the append boundary.
// ---------------------------------------------------------------------------

#[test]
fn appended_session_reads_back_across_the_append_boundary() {
    let world = build_world("s5-append");
    let config = world.write_config(&world.config_value(false, true, None));
    let claude_c = world
        .claude_root
        .join("proj")
        .join("cccccccc-3333-4333-8333-333333333333.jsonl");
    write_lines(
        &claude_c,
        &[claude_line("cla-ses-c", "u-c1", "first part needleappendone")],
    );
    world.capture_all(&config);
    // The session grows; the second capture archives an append_range.
    {
        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(&claude_c)
            .expect("append to transcript");
        writeln!(
            file,
            "{}",
            claude_line("cla-ses-c", "u-c2", "second part needleappendtwo")
        )
        .expect("append line");
    }
    world.capture_all(&config);

    fs::remove_file(&claude_c).expect("delete live transcript");
    world.index(&config);
    let parsed = load_config(&config).expect("config loads");
    for (needle, key, count) in [
        ("needleappendone", "context_after", 5),
        ("needleappendtwo", "context_before", 5),
    ] {
        let response = world.search(
            &config,
            json!({"query": needle, "mode": "exact", "sources": ["claude_code"], "limit": 10}),
        );
        let hit = hit_by_needle(&response, needle);
        assert_eq!(hit["origin"], "archive");
        let request = ReadRequest::parse(&json!({
            "ref": first_ref(hit),
            key: count,
            "max_bytes": 65536
        }))
        .expect("read request");
        let outcome = memory_read(&parsed, &request);
        assert_eq!(outcome.exit_code, 0, "context read failed");
        let value = serde_json::to_value(&outcome).unwrap();
        let text: String = value["segments"]
            .as_array()
            .map(|segments| {
                segments
                    .iter()
                    .filter_map(|segment| segment["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default();
        assert!(
            text.contains("needleappendone") && text.contains("needleappendtwo"),
            "context must span the append boundary: {text}"
        );
    }
}

// ---------------------------------------------------------------------------
// 6. Without an archive source the build and answers are unchanged: no origin
//    key, no coverage archive keys, no detail.archive.
// ---------------------------------------------------------------------------

#[test]
fn without_archive_source_behavior_is_unchanged() {
    let world = build_world("s5-noarchive");
    let config = world.write_config(&world.config_value(false, false, None));
    // Without an archive source the driver needs an explicit --root.
    world.capture_all_with_root(&config, Some(&world.archive_root));
    fs::remove_file(&world.claude_b).expect("delete live transcript");
    let reports = world.index(&config);
    let claude = world.index_report(&reports, "claude_code");
    assert!(
        claude["detail"].get("archive").is_none(),
        "no detail.archive without an archive source: {claude}"
    );
    let response = world.search(
        &config,
        json!({"query": "needleclaudelive", "mode": "exact", "sources": ["claude_code"], "limit": 10}),
    );
    for hit in hits_of(&response) {
        assert!(hit.get("origin").is_none(), "no origin key: {hit}");
    }
    let detail = &response["source_status"][0]["detail"];
    assert!(
        detail["coverage"].get("archive_only_sessions").is_none(),
        "no archive coverage without an archive source: {detail}"
    );
}

// ---------------------------------------------------------------------------
// 7. The archive source's enabled flag does not matter: fallback applies the
//    same way for enabled:false (capture-only) and enabled:true.
// ---------------------------------------------------------------------------

#[test]
fn archive_source_enabled_flag_does_not_change_fallback() {
    let world = build_world("s5-enabled");
    let config_disabled = world.write_config(&world.config_value(false, true, None));
    world.capture_all(&config_disabled);
    fs::remove_file(&world.claude_b).expect("delete live transcript");
    let reports = world.index(&config_disabled);
    let disabled = world
        .index_report(&reports, "claude_code")["detail"]["archive"]
        .clone();
    assert_eq!(disabled["status"], "ok");
    assert_eq!(disabled["archive_only_sessions"], 1);

    let config_enabled = world.write_config(&world.config_value(true, true, None));
    let reports = world.index(&config_enabled);
    let enabled = world
        .index_report(&reports, "claude_code")["detail"]["archive"]
        .clone();
    assert_eq!(enabled["status"], "ok");
    assert_eq!(enabled["archive_only_sessions"], 1);
    assert_eq!(disabled["archive_only_sessions"], enabled["archive_only_sessions"]);
}

// ---------------------------------------------------------------------------
// 8. Deleting the cache and rebuilding produces the same hits (fake embedding,
//    deterministic bytes) and the same inventory fingerprint.
// ---------------------------------------------------------------------------

#[test]
fn cache_rebuild_is_byte_identical_under_the_fake_backend() {
    let world = build_world("s5-rebuild");
    let config = world.write_config(&world.config_value(false, true, Some(embedding())));
    world.capture_all(&config);
    fs::remove_file(&world.claude_b).expect("delete live transcript");
    let first = world.index(&config);
    let first_fingerprint = world.index_report(&first, "claude_code")["detail"]["archive"]
        ["inventory_fingerprint"]
        .clone();
    let first_hits = world.search(
        &config,
        json!({"query": "needleclaudearchive", "mode": "exact", "sources": ["claude_code"], "limit": 10}),
    );

    // Delete the whole cache root; the rebuild must converge to the same rows.
    fs::remove_dir_all(&world.cache_root).expect("delete cache root");
    let second = world.index(&config);
    assert_eq!(
        world.index_report(&second, "claude_code")["detail"]["archive"]["inventory_fingerprint"],
        first_fingerprint,
        "the manifest-set fingerprint is stable"
    );
    let second_hits = world.search(
        &config,
        json!({"query": "needleclaudearchive", "mode": "exact", "sources": ["claude_code"], "limit": 10}),
    );
    assert_eq!(first_hits["results"], second_hits["results"]);
}

// ---------------------------------------------------------------------------
// 9. A new capture changes the manifest-set fingerprint (the inventory cache
//    invalidation condition).
// ---------------------------------------------------------------------------

#[test]
fn new_capture_changes_the_inventory_fingerprint() {
    let world = build_world("s5-fingerprint");
    let config = world.write_config(&world.config_value(false, true, None));
    world.capture_all(&config);
    let reports = world.index(&config);
    let before = world.index_report(&reports, "claude_code")["detail"]["archive"]
        ["inventory_fingerprint"]
        .clone();

    // A brand-new capture (a new file) commits new manifests.
    write_lines(
        &world.claude_root.join("proj").join("dddddddd-4444-4444-8444-444444444444.jsonl"),
        &[claude_line("cla-ses-d", "u-d1", "another session needlenewcapture")],
    );
    world.capture_all(&config);
    let reports = world.index(&config);
    let after = world.index_report(&reports, "claude_code")["detail"]["archive"]
        ["inventory_fingerprint"]
        .clone();
    assert_ne!(before, after, "a new capture must change the fingerprint");
}

// ---------------------------------------------------------------------------
// 10. MCP read-only: memory_search and memory_read leave the cache root
//     byte-identical.
// ---------------------------------------------------------------------------

#[test]
fn mcp_calls_never_write_the_cache_root() {
    let world = build_world("s5-readonly");
    let config = world.write_config(&world.config_value(false, true, Some(embedding())));
    world.capture_all(&config);
    fs::remove_file(&world.claude_b).expect("delete live transcript");
    world.index(&config);

    let response = world.search(
        &config,
        json!({"query": "needleclaudearchive", "mode": "exact", "sources": ["claude_code"], "limit": 10}),
    );
    let hit = hit_by_needle(&response, "needleclaudearchive");
    let before = World::tree_hash(&world.cache_root);
    let _ = world.read(&config, first_ref(hit));
    let _ = world.search(
        &config,
        json!({"query": "needleclaudearchive", "sources": ["claude_code"], "limit": 10}),
    );
    let after = World::tree_hash(&world.cache_root);
    assert_eq!(before, after, "search/read must not write the cache root");
}

// ---------------------------------------------------------------------------
// 11. Fail-closed: a hand-written fallback ref whose carrier does not hold the
//     record is rejected, and the live root is never consulted.
// ---------------------------------------------------------------------------

#[test]
fn forged_fallback_ref_fails_closed() {
    let world = build_world("s5-forged");
    let config = world.write_config(&world.config_value(false, true, None));
    world.capture_all(&config);
    fs::remove_file(&world.claude_b).expect("delete live transcript");
    world.index(&config);

    let response = world.search(
        &config,
        json!({"query": "needleclaudearchive", "mode": "exact", "sources": ["claude_code"], "limit": 10}),
    );
    let hit = hit_by_needle(&response, "needleclaudearchive");
    let mut forged = first_ref(hit).clone();
    // A record address the named carrier never committed.
    forged["locator"]["archive_fallback"]["record_sha256"] =
        json!("0000000000000000000000000000000000000000000000000000000000000000");
    let parsed = load_config(&config).expect("config loads");
    let request = ReadRequest::parse(&json!({"ref": forged, "max_bytes": 1024})).expect("parses");
    let outcome = memory_read(&parsed, &request);
    assert_ne!(outcome.exit_code, 0, "a forged record address must fail");
    let value = serde_json::to_value(&outcome).unwrap();
    assert_eq!(value["code"], json!("REF_LOCATOR_MISSING"));
}

// ---------------------------------------------------------------------------
// 12. Origin coverage of every answering path: exact, hybrid and the archive
//     registry stay consistent over several index runs (no duplicates).
// ---------------------------------------------------------------------------

#[test]
fn repeated_index_runs_stay_deduplicated() {
    let world = build_world("s5-reruns");
    let config = world.write_config(&world.config_value(false, true, Some(embedding())));
    world.capture_all(&config);
    fs::remove_file(&world.claude_b).expect("delete live transcript");
    for _ in 0..3 {
        world.index(&config);
        let response = world.search(
            &config,
            json!({"query": "needleclaudearchive", "mode": "exact", "sources": ["claude_code"], "limit": 20}),
        );
        let mut seen: BTreeMap<String, usize> = BTreeMap::new();
        for hit in hits_of(&response) {
            let key = hit["hit_id"].as_str().expect("hit_id").to_string();
            *seen.entry(key).or_insert(0) += 1;
        }
        assert!(
            seen.values().all(|count| *count == 1),
            "no duplicated hit ids across runs: {seen:?}"
        );
        assert_eq!(seen.len(), 1, "exactly one hit for the archived session");
    }
}
