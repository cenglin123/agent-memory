//! §G.1 / §G.2 regression matrix — Archive fallback (archive-only sessions).
//!
//! A captured carrier is removed from the live root, so its session is
//! archive-only and the index projects it from the committed evidence. The
//! incremental path and a rebuild over the same frozen evidence must agree, and
//! the §A counters must show that a registered empty session is not re-read.
//!
//! The multi-carrier cases are the verify-001 P2-1 regression: a session id
//! held by two carriers must be projected from both of them in one run,
//! whatever order the carriers sort in.
//!
//! All fixtures are synthetic and live under a per-case temporary root.

mod support;

use std::path::{Path, PathBuf};

use agent_memory::archive::Archive;
use agent_memory::capture::capture_source;
use agent_memory::recall::config::{load_config, RecallConfig};
use serde_json::{json, Value};
use support::matrix as m;

const SESSION: &str = "44444444-4444-4444-8444-444444444444";

fn source(id: &str, kind: &str, root: &Path, enabled: bool) -> Value {
    json!({
        "source_id": id,
        "kind": kind,
        "root": root.to_string_lossy(),
        "enabled": enabled,
    })
}

fn claude_line(uuid: &str, time: &str, kind: &str, text: &str) -> String {
    json!({
        "type": kind,
        "message": {"role": kind, "content": text},
        "timestamp": time,
        "sessionId": SESSION,
        "uuid": uuid,
        "isSidechain": false
    })
    .to_string()
}

fn write_lines(path: &Path, lines: &[String]) {
    let mut body = lines.join("\n");
    body.push('\n');
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("parent");
    }
    std::fs::write(path, body).expect("write fixture");
}

struct Setup {
    live_root: PathBuf,
    archive_root: PathBuf,
    archive: Archive,
    incremental: RecallConfig,
    rebuilt: RecallConfig,
}

impl Setup {
    fn new(env: &support::TestEnv, name: &str) -> Setup {
        let live_root = env.root(&format!("{name}-live"));
        let archive_root = env.root(&format!("{name}-archive"));
        let archive = Archive::init(&archive_root).expect("init archive");
        let pair = m::Pair::new(env, name);
        let sources = vec![
            source("cc", "claude_code", &live_root, true),
            source("archive", "archive", &archive_root, false),
        ];
        let value = |cache: &Path| {
            json!({
                "version": 2,
                "sources": sources,
                "source_order": ["cc"],
                "interpreter": {"kb": {"command": "C:/python/python.exe"}},
                "domain": "default",
                "telemetry": {"enabled": false},
                "cache_root": cache.to_string_lossy(),
            })
        };
        let incremental = load_config(
            &env.write_json(&format!("{name}-incremental.json"), &value(&pair.incremental_root)),
        )
        .expect("config");
        let rebuilt = load_config(
            &env.write_json(&format!("{name}-rebuild.json"), &value(&pair.rebuild_root)),
        )
        .expect("config");
        Setup {
            live_root,
            archive_root,
            archive,
            incremental,
            rebuilt,
        }
    }

    /// Capture a carrier under the live root, then remove it from the live root
    /// so its session becomes archive-only.
    fn capture_and_remove(&self, rel: &str, lines: &[String]) -> PathBuf {
        let path = self.live_root.join(rel);
        write_lines(&path, lines);
        let report = capture_source(&self.archive, &path, "claude", "default", None)
            .expect("capture carrier");
        assert!(report.ok, "capture failed: {report:?}");
        std::fs::remove_file(&path).expect("remove live carrier");
        path
    }

    fn round(&self) -> Vec<Value> {
        let reports = agent_memory::recall::fulltext::build::index_all(&self.incremental, None, false)
            .expect("incremental index");
        serde_json::to_value(&reports)
            .expect("reports")
            .as_array()
            .expect("array")
            .clone()
    }

    fn assert_equivalent(&self, what: &str) {
        m::index_rebuild(&self.rebuilt);
        m::assert_equivalent(&self.incremental, &self.rebuilt, what);
    }
}

#[test]
fn archive_only_session_is_projected_and_registered() {
    let env = support::TestEnv::new();
    let setup = Setup::new(&env, "archive-only");
    setup.capture_and_remove(
        "proj/a.jsonl",
        &[
            claude_line("u1", "2026-10-25T13:25:00Z", "user", "archive needle one"),
            claude_line("a1", "2026-10-25T13:26:00Z", "assistant", "archive needle answer"),
        ],
    );

    let first = setup.round();
    let detail = &first[0]["detail"]["archive"];
    assert_eq!(detail["archive_only_sessions"], json!(1), "{first:?}");
    assert!(first[0]["rows"].as_u64().unwrap_or(0) >= 2, "{first:?}");

    // Second round: the rows are present, so the carrier is not rebuilt.
    let second = setup.round();
    let cost = &second[0]["cost"];
    assert!(
        !m::phase(cost, "archive_carrier_rebuild"),
        "a session with rows present is not re-read: {cost}"
    );
    setup.assert_equivalent("archive-only session");
}

#[test]
fn archive_zero_row_session_is_registered_and_not_rescanned() {
    let env = support::TestEnv::new();
    let setup = Setup::new(&env, "archive-zero");
    // Only an injected line: the projection yields zero units.
    setup.capture_and_remove(
        "proj/zero.jsonl",
        &[json!({
            "type": "system",
            "content": "no projectable unit here",
            "timestamp": "2026-10-25T13:25:00Z",
            "sessionId": SESSION,
            "uuid": "u0",
            "isSidechain": false
        })
        .to_string()],
    );

    let first = setup.round();
    assert_eq!(first[0]["detail"]["archive"]["archive_only_sessions"], json!(1));
    let second = setup.round();
    let detail = &second[0]["detail"]["archive"];
    assert!(
        detail["skipped_registered_empty"].as_u64().unwrap_or(0) >= 1,
        "a registered empty session is skipped, not re-read: {second:?}"
    );
    assert!(
        !m::phase(&second[0]["cost"], "archive_carrier_rebuild"),
        "the zero-row carrier is not decompressed again: {second:?}"
    );
    setup.assert_equivalent("archive zero-row session");
}

fn two_carriers(setup: &Setup, empty_first: bool) {
    let empty = vec![json!({
        "type": "system",
        "content": "no projectable unit here",
        "timestamp": "2026-10-25T13:20:00Z",
        "sessionId": SESSION,
        "uuid": "u0",
        "isSidechain": false
    })
    .to_string()];
    let real = vec![
        claude_line("u1", "2026-10-25T13:25:00Z", "user", "carrier needle question"),
        claude_line("a1", "2026-10-25T13:26:00Z", "assistant", "carrier needle answer"),
    ];
    if empty_first {
        setup.capture_and_remove("proj/a-empty.jsonl", &empty);
        setup.capture_and_remove("proj/b-real.jsonl", &real);
    } else {
        setup.capture_and_remove("proj/a-real.jsonl", &real);
        setup.capture_and_remove("proj/b-empty.jsonl", &empty);
    }
}

#[test]
fn archive_multi_carrier_session_projects_every_carrier() {
    for empty_first in [true, false] {
        let env = support::TestEnv::new();
        let name = if empty_first {
            "archive-multi-empty-first"
        } else {
            "archive-multi-empty-last"
        };
        let setup = Setup::new(&env, name);
        two_carriers(&setup, empty_first);

        let reports = setup.round();
        let detail = &reports[0]["detail"]["archive"];
        assert_eq!(
            detail["archive_only_sessions"],
            json!(1),
            "{name}: one session, two carriers: {reports:?}"
        );
        // The real content of the later carrier must be present (P2-1).
        let conn = rusqlite::Connection::open(agent_memory::recall::fulltext::index_path(
            &setup.incremental.cache_root.clone().map(PathBuf::from).expect("cache"),
            "cc",
        ))
        .expect("index");
        let units: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM units WHERE text LIKE '%carrier needle%'",
                [],
                |row| row.get(0),
            )
            .expect("count");
        assert!(units >= 2, "{name}: later carrier content is projected: {units}");
        drop(conn);
        // Cost: the manifest set is parsed once per manifest, never once per
        // carrier; the two carriers hold exactly two manifests.
        let cost = &reports[0]["cost"];
        assert!(
            m::counter(cost, "manifest_parse_count") <= 2,
            "{name}: one manifest parse per manifest, not per carrier: {cost}"
        );
        setup.assert_equivalent(name);
    }
}

#[test]
fn archive_lost_rows_are_rebuilt_from_the_frozen_evidence() {
    let env = support::TestEnv::new();
    let setup = Setup::new(&env, "archive-lost-rows");
    setup.capture_and_remove(
        "proj/a.jsonl",
        &[
            claude_line("u1", "2026-10-25T13:25:00Z", "user", "lost need needle"),
            claude_line("a1", "2026-10-25T13:26:00Z", "assistant", "lost needle answer"),
        ],
    );
    setup.round();

    let cache = setup
        .incremental
        .cache_root
        .clone()
        .map(PathBuf::from)
        .expect("cache_root");
    let index = agent_memory::recall::fulltext::index_path(&cache, "cc");
    {
        let conn = rusqlite::Connection::open(&index).expect("open index");
        // A consistent loss of the archive projection: ownership table first
        // (its rowid references `units`), then the keys and the units.
        conn.execute(
            "DELETE FROM conversation_units WHERE rowid IN \
             (SELECT rowid FROM units WHERE origin = 'archive')",
            [],
        )
        .expect("drop archive ownership rows");
        conn.execute("DELETE FROM doc_keys WHERE origin = 'archive'", [])
            .expect("drop archive keys");
        conn.execute("DELETE FROM units WHERE origin = 'archive'", [])
            .expect("drop archive units");
    }
    let reports = setup.round();
    assert!(
        reports[0]["detail"]["archive"]["added"].as_u64().unwrap_or(0) >= 1,
        "lost rows are rebuilt: {reports:?}"
    );
    // Cost: rebuilding lost rows re-reads the carrier exactly once, and the
    // manifest set is parsed once.
    assert!(
        m::phase(&reports[0]["cost"], "archive_carrier_rebuild"),
        "the lost rows come from a single carrier rebuild: {reports:?}"
    );
    assert!(
        m::counter(&reports[0]["cost"], "manifest_parse_count") <= 1,
        "the manifest set is parsed once: {reports:?}"
    );
    setup.assert_equivalent("archive lost rows");
}

// Keep the archive root field meaningful (the fallback reads it from config).
#[test]
fn archive_root_is_the_configured_runtime_root() {
    let env = support::TestEnv::new();
    let setup = Setup::new(&env, "archive-root");
    assert_eq!(
        setup
            .incremental
            .sources
            .iter()
            .find(|source| source.kind == agent_memory::recall::config::SourceKind::Archive)
            .map(|source| source.root.clone()),
        Some(setup.archive_root.to_string_lossy().to_string())
    );
}
