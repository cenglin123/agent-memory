//! §G.1 / §G.2 regression matrix — index side.
//!
//! Every scenario runs two paths over the **same frozen input**:
//!
//! 1. *incremental*: a cache refreshed round by round while the input changes;
//! 2. *rebuild*: a second cache built with `rebuild = true` over the final
//!    input.
//!
//! The scenario passes when the normalised digests agree (units, doc_keys,
//! conversation_units, sessions, archive_sessions, vectors). It also asserts
//! the §A cause counters stay proportional to the change (never to the corpus).
//!
//! Scenarios the current code cannot satisfy are listed in [`KNOWN_GAPS`] and
//! assert that the two paths still differ: a later batch that fixes one must
//! remove it from the list, and this test then fails until it does.
//!
//! All fixtures are synthetic and live under a per-case temporary root.

mod support;

use std::path::{Path, PathBuf};

use agent_memory::recall::config::{load_config, RecallConfig};
use serde_json::{json, Value};
use support::matrix as m;

/// Scenarios where the incremental path is knowingly not equivalent to a
/// rebuild under the current code. Each entry names the scenario, not the test:
/// the test asserts the difference still exists.
///
/// Empty since implementation-004b: the rolling per-session reconcile reads each
/// session once per rotation, so an edit whose clock did not move is seen within
/// the declared window (see `README.md`, "OpenCode rounds").
pub const KNOWN_GAPS: [&str; 0] = [];

pub fn is_known_gap(name: &str) -> bool {
    KNOWN_GAPS.contains(&name)
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

const SESSION: &str = "11111111-1111-4111-8111-111111111111";
const SESSION_B: &str = "22222222-2222-4222-8222-222222222222";
const DSH_SESSION: &str = "33333333-3333-4333-8333-333333333333";
const PROJECT: &str = "--C-work-proj--";

fn source(id: &str, kind: &str, root: &Path) -> Value {
    json!({
        "source_id": id,
        "kind": kind,
        "root": root.to_string_lossy(),
        "enabled": true,
    })
}

fn config_value(sources: Vec<Value>, order: Vec<&str>, cache_root: &Path, embedding: bool) -> Value {
    let mut value = json!({
        "version": 2,
        "sources": sources,
        "source_order": order,
        "interpreter": {"kb": {"command": "C:/python/python.exe"}},
        "domain": "default",
        "telemetry": {"enabled": false},
        "cache_root": cache_root.to_string_lossy(),
    });
    if embedding {
        value["interpreter"]["embedding"] = json!({
            "model_id": "fake-hash-v1",
            "dimension": 32,
            "normalization": "l2",
            "backend": "fake"
        });
    }
    value
}

fn load(env: &support::TestEnv, name: &str, value: Value) -> RecallConfig {
    let path = env.write_json(&format!("{name}.json"), &value);
    load_config(&path).expect("config")
}

/// The incremental and the rebuild configuration over one frozen input.
fn pair_configs(
    env: &support::TestEnv,
    name: &str,
    sources: &[Value],
    order: &[&str],
    pair: &m::Pair,
    embedding: bool,
) -> (RecallConfig, RecallConfig) {
    let incremental = load(
        env,
        &format!("{name}-incremental"),
        config_value(
            sources.to_vec(),
            order.to_vec(),
            &pair.incremental_root,
            embedding,
        ),
    );
    let rebuilt = load(
        env,
        &format!("{name}-rebuild"),
        config_value(
            sources.to_vec(),
            order.to_vec(),
            &pair.rebuild_root,
            embedding,
        ),
    );
    (incremental, rebuilt)
}

fn claude_line(session: &str, uuid: &str, time: &str, kind: &str, text: &str) -> String {
    json!({
        "type": kind,
        "message": {"role": kind, "content": text},
        "timestamp": time,
        "sessionId": session,
        "uuid": uuid,
        "isSidechain": false
    })
    .to_string()
}

fn write_lines(path: &Path, lines: &[String]) {
    let mut body = lines.join("\n");
    body.push('\n');
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("fixture parent");
    }
    std::fs::write(path, body).expect("write fixture");
}

fn append_lines(path: &Path, lines: &[String]) {
    let mut body = lines.join("\n");
    body.push('\n');
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(path)
        .expect("open for append");
    file.write_all(body.as_bytes()).expect("append fixture");
}

/// One index round of the incremental path; returns the serialized reports.
fn round(config: &RecallConfig) -> Vec<Value> {
    let reports = agent_memory::recall::fulltext::build::index_all(config, None, false)
        .expect("incremental index");
    serde_json::to_value(&reports)
        .expect("reports serialize")
        .as_array()
        .expect("array")
        .clone()
}

fn cost_of(reports: &[Value], source_id: &str) -> Value {
    reports
        .iter()
        .find(|report| report["source_id"] == json!(source_id))
        .unwrap_or_else(|| panic!("source {source_id} missing: {reports:?}"))["cost"]
        .clone()
}

fn reports_status(reports: &[Value], source_id: &str) -> String {
    reports
        .iter()
        .find(|report| report["source_id"] == json!(source_id))
        .unwrap_or_else(|| panic!("source {source_id} missing: {reports:?}"))["status"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

fn claude_fixture(env: &support::TestEnv) -> (PathBuf, PathBuf) {
    let root = env.root("claude-root");
    let path = root.join("proj").join(format!("{SESSION}.jsonl"));
    write_lines(
        &path,
        &[
            claude_line(SESSION, "u1", "2026-10-25T13:25:00Z", "user", "needle one"),
            claude_line(SESSION, "a1", "2026-10-25T13:26:00Z", "assistant", "needle answer one"),
        ],
    );
    (root, path)
}

// ---------------------------------------------------------------------------
// JSONL (Claude Code): append, timestamps, rewrites, truncation, deletion
// ---------------------------------------------------------------------------

#[test]
fn jsonl_append_is_equivalent_and_does_not_rescan() {
    let env = support::TestEnv::new();
    let (root, path) = claude_fixture(&env);
    let pair = m::Pair::new(&env, "cc-append");
    let (incremental, rebuilt) = pair_configs(
        &env,
        "cc-append",
        &[source("cc", "claude_code", &root)],
        &["cc"],
        &pair,
        false,
    );

    round(&incremental);
    append_lines(
        &path,
        &[claude_line(
            SESSION,
            "u2",
            "2026-10-25T13:27:00Z",
            "user",
            "needle two",
        )],
    );
    let reports = round(&incremental);
    let cost = cost_of(&reports, "cc");
    assert_eq!(
        m::counter_sum(&cost, "index.full_rescan."),
        0,
        "an append never re-reads the file: {cost}"
    );
    m::index_rebuild(&rebuilt);
    m::assert_equivalent(&incremental, &rebuilt, "jsonl append");
}

#[test]
fn jsonl_mtime_only_refresh_keeps_the_content() {
    let env = support::TestEnv::new();
    let (root, path) = claude_fixture(&env);
    let pair = m::Pair::new(&env, "cc-mtime");
    let (incremental, rebuilt) = pair_configs(
        &env,
        "cc-mtime",
        &[source("cc", "claude_code", &root)],
        &["cc"],
        &pair,
        false,
    );

    round(&incremental);
    let before = m::mtime(&path);
    m::set_mtime(&path, before + std::time::Duration::from_secs(5));
    let reports = round(&incremental);
    let cost = cost_of(&reports, "cc");
    // A refreshed timestamp over identical bytes is a recorded fact, not a
    // rescan: the head and anchor probes prove the prefix did not move.
    assert_eq!(
        m::counter_sum(&cost, "index.full_rescan."),
        0,
        "a touch is not a rewrite: {cost}"
    );
    assert_eq!(
        m::counter(&cost, "index.mtime_only_refresh"),
        1,
        "the refresh is named and counted: {cost}"
    );
    assert_eq!(
        m::counter(&cost, "index.probe_reads"),
        1,
        "only two probe windows are read: {cost}"
    );
    m::index_rebuild(&rebuilt);
    m::assert_equivalent(&incremental, &rebuilt, "jsonl mtime-only refresh");
}

#[test]
fn jsonl_same_length_rewrite_is_reread_and_equivalent() {
    let env = support::TestEnv::new();
    let (root, path) = claude_fixture(&env);
    let pair = m::Pair::new(&env, "cc-samelength");
    let (incremental, rebuilt) = pair_configs(
        &env,
        "cc-samelength",
        &[source("cc", "claude_code", &root)],
        &["cc"],
        &pair,
        false,
    );

    round(&incremental);
    let bytes = std::fs::read(&path).expect("read fixture");
    let text = String::from_utf8(bytes).expect("utf8");
    let replaced = text.replace("needle answer one", "needle answer two");
    assert_eq!(replaced.len(), text.len(), "same length rewrite");
    std::fs::write(&path, replaced.as_bytes()).expect("rewrite");
    m::set_mtime(&path, m::mtime(&path) + std::time::Duration::from_secs(9));

    let reports = round(&incremental);
    let cost = cost_of(&reports, "cc");
    assert_eq!(
        m::counter(&cost, "index.full_rescan.probe_mismatch"),
        1,
        "the probe comparison catches the rewrite: {cost}"
    );
    assert_eq!(
        m::counter_sum(&cost, "index.full_rescan."),
        1,
        "exactly one carrier is re-read: {cost}"
    );
    m::index_rebuild(&rebuilt);
    m::assert_equivalent(&incremental, &rebuilt, "jsonl same-length rewrite");
}

#[test]
fn jsonl_tail_window_rewrite_is_detected() {
    // A rewrite that only touches the window ending at the committed offset: the
    // head is identical, so only the anchor probe can catch it.
    let env = support::TestEnv::new();
    let root = env.root("claude-tail-root");
    let path = root.join("proj").join(format!("{SESSION}.jsonl"));
    let long = format!("needle answer {}TAILMARK", "x".repeat(30_000));
    write_lines(
        &path,
        &[
            claude_line(SESSION, "u1", "2026-10-25T13:25:00Z", "user", "needle one"),
            claude_line(SESSION, "a1", "2026-10-25T13:26:00Z", "assistant", &long),
        ],
    );
    let pair = m::Pair::new(&env, "cc-tail");
    let (incremental, rebuilt) = pair_configs(
        &env,
        "cc-tail",
        &[source("cc", "claude_code", &root)],
        &["cc"],
        &pair,
        false,
    );

    round(&incremental);
    let bytes = std::fs::read(&path).expect("read fixture");
    let text = String::from_utf8(bytes).expect("utf8");
    let replaced = text.replace("TAILMARK", "tailmark");
    assert_eq!(replaced.len(), text.len(), "the tail keeps its length");
    std::fs::write(&path, replaced.as_bytes()).expect("rewrite the tail");
    m::set_mtime(&path, m::mtime(&path) + std::time::Duration::from_secs(11));

    let reports = round(&incremental);
    let cost = cost_of(&reports, "cc");
    assert_eq!(
        m::counter(&cost, "index.full_rescan.probe_mismatch"),
        1,
        "the anchor probe catches a tail-only rewrite: {cost}"
    );
    m::index_rebuild(&rebuilt);
    m::assert_equivalent(&incremental, &rebuilt, "jsonl tail window rewrite");
}

#[test]
fn jsonl_truncation_is_rebuilt_and_equivalent() {
    let env = support::TestEnv::new();
    let (root, path) = claude_fixture(&env);
    let pair = m::Pair::new(&env, "cc-truncate");
    let (incremental, rebuilt) = pair_configs(
        &env,
        "cc-truncate",
        &[source("cc", "claude_code", &root)],
        &["cc"],
        &pair,
        false,
    );

    round(&incremental);
    write_lines(
        &path,
        &[claude_line(
            SESSION,
            "u1",
            "2026-10-25T13:25:00Z",
            "user",
            "needle one",
        )],
    );
    let reports = round(&incremental);
    let cost = cost_of(&reports, "cc");
    assert_eq!(
        m::counter(&cost, "index.full_rescan.size_regression"),
        1,
        "a shrinking file is rebuilt: {cost}"
    );
    m::index_rebuild(&rebuilt);
    m::assert_equivalent(&incremental, &rebuilt, "jsonl truncation");
}

#[test]
fn jsonl_deletion_and_rename_are_equivalent() {
    let env = support::TestEnv::new();
    let (root, path) = claude_fixture(&env);
    let extra = root.join("proj").join(format!("{SESSION_B}.jsonl"));
    write_lines(
        &extra,
        &[claude_line(
            SESSION_B,
            "b1",
            "2026-10-25T14:00:00Z",
            "user",
            "needle b",
        )],
    );
    let pair = m::Pair::new(&env, "cc-delete");
    let (incremental, rebuilt) = pair_configs(
        &env,
        "cc-delete",
        &[source("cc", "claude_code", &root)],
        &["cc"],
        &pair,
        false,
    );

    round(&incremental);
    // Rename one carrier (same content, new relative path): the new path is a
    // file the watermark has never seen, so exactly one whole-file read.
    let moved = root.join("proj").join("renamed.jsonl");
    std::fs::rename(&extra, &moved).expect("rename fixture");
    let renamed = round(&incremental);
    let cost = cost_of(&renamed, "cc");
    assert_eq!(
        m::counter(&cost, "index.full_rescan.new_file"),
        1,
        "a rename re-reads exactly the new path once: {cost}"
    );
    assert_eq!(
        m::counter_sum(&cost, "index.full_rescan."),
        1,
        "no other file is re-read by the rename: {cost}"
    );
    // ... then delete it: rows are dropped from the watermark, nothing is read.
    std::fs::remove_file(&moved).expect("delete fixture");
    let deleted = round(&incremental);
    let cost = cost_of(&deleted, "cc");
    assert_eq!(
        m::counter_sum(&cost, "index.full_rescan."),
        0,
        "deleting a carrier reads no file: {cost}"
    );
    // And delete the main session's carrier too.
    std::fs::remove_file(&path).expect("delete main fixture");
    let empty = round(&incremental);
    let cost = cost_of(&empty, "cc");
    assert_eq!(
        m::counter_sum(&cost, "index.full_rescan."),
        0,
        "the last deletion reads no file: {cost}"
    );

    m::index_rebuild(&rebuilt);
    m::assert_equivalent(&incremental, &rebuilt, "jsonl deletion + rename");
}

#[test]
fn jsonl_mtime_regression_with_growth_is_reread() {
    let env = support::TestEnv::new();
    let (root, path) = claude_fixture(&env);
    let pair = m::Pair::new(&env, "cc-mtime-back");
    let (incremental, rebuilt) = pair_configs(
        &env,
        "cc-mtime-back",
        &[source("cc", "claude_code", &root)],
        &["cc"],
        &pair,
        false,
    );

    round(&incremental);
    append_lines(
        &path,
        &[claude_line(
            SESSION,
            "u2",
            "2026-10-25T13:28:00Z",
            "user",
            "needle grown",
        )],
    );
    let old = m::ago(3_600);
    m::set_mtime(&path, old);
    let reports = round(&incremental);
    let cost = cost_of(&reports, "cc");
    assert_eq!(
        m::counter(&cost, "index.full_rescan.mtime_regression"),
        1,
        "a backwards timestamp with growth is re-read: {cost}"
    );
    m::index_rebuild(&rebuilt);
    m::assert_equivalent(&incremental, &rebuilt, "jsonl mtime regression + growth");
}

#[test]
fn jsonl_upgrade_round_without_watermark_fields_rebuilds() {
    let env = support::TestEnv::new();
    let (root, _path) = claude_fixture(&env);
    let pair = m::Pair::new(&env, "cc-upgrade");
    let (incremental, rebuilt) = pair_configs(
        &env,
        "cc-upgrade",
        &[source("cc", "claude_code", &root)],
        &["cc"],
        &pair,
        false,
    );

    round(&incremental);
    // Simulate a watermark written by an older build: the file entry lacks the
    // fields this build requires, so the whole watermark is untrusted.
    let index = agent_memory::recall::fulltext::index_path(&pair.incremental_root, "cc");
    {
        let conn = rusqlite::Connection::open(&index).expect("open index");
        conn.execute(
            "UPDATE meta SET value = ?2 WHERE key = 'watermark'",
            rusqlite::params!["watermark", r#"{"files":{"proj/11111111-1111-4111-8111-111111111111.jsonl":{"size":1}}}"#],
        )
        .expect("rewrite watermark");
    }
    let reports = round(&incremental);
    let cost = cost_of(&reports, "cc");
    assert!(
        m::counter_sum(&cost, "index.full_rescan.") >= 1,
        "an untrusted watermark forces a full read: {cost}"
    );
    m::index_rebuild(&rebuilt);
    m::assert_equivalent(&incremental, &rebuilt, "jsonl upgrade round");
}

#[test]
fn jsonl_upgrade_round_fills_the_probes_without_a_rescan() {
    let env = support::TestEnv::new();
    let (root, _path) = claude_fixture(&env);
    let pair = m::Pair::new(&env, "cc-upgrade-fill");
    let (incremental, rebuilt) = pair_configs(
        &env,
        "cc-upgrade-fill",
        &[source("cc", "claude_code", &root)],
        &["cc"],
        &pair,
        false,
    );

    round(&incremental);
    // The realistic upgrade: a watermark written by the pre-004a build carries
    // `size` / `mtime_ns` / `offset` / `lines` and nothing else. Nothing about
    // the file changed, so the round must simply complete the entry.
    let index = agent_memory::recall::fulltext::index_path(&pair.incremental_root, "cc");
    let key = format!("proj/{SESSION}.jsonl");
    let legacy = {
        let conn = rusqlite::Connection::open(&index).expect("open index");
        let raw: String = conn
            .query_row("SELECT value FROM meta WHERE key = 'watermark'", [], |row| {
                row.get(0)
            })
            .expect("watermark");
        let mut value: Value = serde_json::from_str(&raw).expect("watermark json");
        let entry = value["files"][&key].clone();
        let stripped = json!({
            "size": entry["size"],
            "mtime_ns": entry["mtime_ns"],
            "offset": entry["offset"],
            "lines": entry["lines"],
        });
        value["files"] = json!({ key.clone(): stripped });
        conn.execute(
            "UPDATE meta SET value = ?2 WHERE key = 'watermark'",
            rusqlite::params!["watermark", value.to_string()],
        )
        .expect("rewrite watermark");
        entry
    };

    let reports = round(&incremental);
    let cost = cost_of(&reports, "cc");
    assert_eq!(
        m::counter_sum(&cost, "index.full_rescan."),
        0,
        "a pre-004a watermark is completed, not distrusted: {cost}"
    );
    assert_eq!(
        m::counter(&cost, "index.mtime_only_refresh"),
        0,
        "nothing was refreshed, only completed: {cost}"
    );
    assert_eq!(
        m::counter(&cost, "index.probe_reads"),
        1,
        "the missing probes cost two windows: {cost}"
    );
    // The next round can use them: the entry now carries both probes and the
    // file identity.
    {
        let conn = rusqlite::Connection::open(&index).expect("open index");
        let raw: String = conn
            .query_row("SELECT value FROM meta WHERE key = 'watermark'", [], |row| {
                row.get(0)
            })
            .expect("watermark");
        let value: Value = serde_json::from_str(&raw).expect("watermark json");
        let entry = &value["files"][&key];
        assert_eq!(entry["size"], legacy["size"]);
        assert_eq!(entry["offset"], legacy["offset"]);
        assert_eq!(entry["lines"], legacy["lines"]);
        assert!(
            entry["head_probe"].as_str().map(|value| value.len()) == Some(64),
            "the head probe was filled in: {entry}"
        );
        assert!(
            entry["anchor_probe"].as_str().map(|value| value.len()) == Some(64),
            "the anchor probe was filled in: {entry}"
        );
        if cfg!(windows) {
            assert!(
                entry["file_identity"]
                    .as_str()
                    .map(|value| !value.is_empty())
                    .unwrap_or(false),
                "the file identity was filled in: {entry}"
            );
        }
    }
    m::index_rebuild(&rebuilt);
    m::assert_equivalent(&incremental, &rebuilt, "jsonl probe fill round");
}

// ---------------------------------------------------------------------------
// Formerly known gaps (index side): closed by the probes / file identity
// ---------------------------------------------------------------------------

#[test]
fn jsonl_same_size_same_mtime_replacement_is_detected_and_equivalent() {
    // A rotated / replaced carrier: same length, same timestamp, different file.
    // Without the file identity this is indistinguishable from an unchanged file.
    if !cfg!(windows) {
        // The identity is a Windows signal (volume serial + file id); on another
        // platform this scenario stays the declared blind spot of
        // `fulltext::stamp`.
        return;
    }
    let env = support::TestEnv::new();
    let (root, path) = claude_fixture(&env);
    let pair = m::Pair::new(&env, "cc-replaced");
    let (incremental, rebuilt) = pair_configs(
        &env,
        "cc-replaced",
        &[source("cc", "claude_code", &root)],
        &["cc"],
        &pair,
        false,
    );

    round(&incremental);
    let at = m::mtime(&path);
    let size = std::fs::metadata(&path).expect("stat").len();
    let bytes = std::fs::read(&path).expect("read");
    let text = String::from_utf8(bytes).expect("utf8");
    let replaced = text.replace("needle answer one", "needle answer two");
    assert_eq!(replaced.len(), text.len());
    // Write a new file and rename it over the old one: same path, same length,
    // same mtime, different file identity.
    let staged = path.with_extension("staged");
    std::fs::write(&staged, replaced.as_bytes()).expect("stage the replacement");
    m::set_mtime(&staged, at);
    std::fs::rename(&staged, &path).expect("replace the carrier");
    assert_eq!(std::fs::metadata(&path).expect("stat").len(), size);
    assert_eq!(m::mtime(&path), at, "the scenario keeps the timestamp");

    let reports = round(&incremental);
    let cost = cost_of(&reports, "cc");
    assert_eq!(
        m::counter(&cost, "index.full_rescan.file_identity_changed"),
        1,
        "the replacement is recognised: {cost}"
    );
    assert_eq!(
        m::counter_sum(&cost, "index.full_rescan."),
        1,
        "exactly one carrier is re-read: {cost}"
    );
    m::index_rebuild(&rebuilt);
    m::assert_equivalent(&incremental, &rebuilt, "jsonl same-size same-mtime replacement");
}

#[test]
fn jsonl_prefix_rewrite_with_growth_is_reread_and_equivalent() {
    let env = support::TestEnv::new();
    let (root, path) = claude_fixture(&env);
    let pair = m::Pair::new(&env, "cc-prefix");
    let (incremental, rebuilt) = pair_configs(
        &env,
        "cc-prefix",
        &[source("cc", "claude_code", &root)],
        &["cc"],
        &pair,
        false,
    );

    round(&incremental);
    let bytes = std::fs::read(&path).expect("read");
    let text = String::from_utf8(bytes).expect("utf8");
    let replaced = text.replace("needle answer one", "REWRITTEN prefix!");
    assert_eq!(replaced.len(), text.len(), "the prefix keeps its length");
    let mut grown = replaced;
    grown.push_str(
        &claude_line(SESSION, "u9", "2026-10-25T13:40:00Z", "user", "needle tail"),
    );
    grown.push('\n');
    std::fs::write(&path, grown.as_bytes()).expect("rewrite + grow");
    m::set_mtime(&path, m::mtime(&path) + std::time::Duration::from_secs(30));

    let reports = round(&incremental);
    let cost = cost_of(&reports, "cc");
    assert_eq!(
        m::counter(&cost, "index.full_rescan.probe_mismatch"),
        1,
        "a rewritten prefix is not an append: {cost}"
    );
    assert_eq!(
        m::counter_sum(&cost, "index.full_rescan."),
        1,
        "exactly one carrier is re-read: {cost}"
    );
    m::index_rebuild(&rebuilt);
    m::assert_equivalent(&incremental, &rebuilt, "jsonl prefix rewrite with growth");
}

// ---------------------------------------------------------------------------
// Codex + DSH
// ---------------------------------------------------------------------------

fn codex_source_entry(root: &Path) -> Value {
    json!({
        "source_id": "codex",
        "kind": "codex",
        "root": root.to_string_lossy(),
        "enabled": true,
    })
}

fn codex_rollout_path(root: &Path, session: &str) -> PathBuf {
    root.join("sessions")
        .join("2026")
        .join("03")
        .join("06")
        .join(format!("rollout-2026-03-06T21-50-12-{session}.jsonl"))
}

#[test]
fn codex_append_is_equivalent() {
    let env = support::TestEnv::new();
    let root = env.root("codex-root");
    let path = codex_rollout_path(&root, SESSION);
    write_lines(
        &path,
        &[
            support::codex_meta(SESSION),
            support::codex_user("needle codex one"),
            support::codex_assistant("needle codex answer"),
        ],
    );
    let pair = m::Pair::new(&env, "codex-append");
    let (incremental, rebuilt) = pair_configs(
        &env,
        "codex-append",
        &[codex_source_entry(&root)],
        &["codex"],
        &pair,
        false,
    );

    round(&incremental);
    append_lines(&path, &[support::codex_user("needle codex two")]);
    let reports = round(&incremental);
    assert_eq!(
        m::counter_sum(&cost_of(&reports, "codex"), "index.full_rescan."),
        0,
        "an append never re-reads the rollout"
    );
    m::index_rebuild(&rebuilt);
    m::assert_equivalent(&incremental, &rebuilt, "codex append");
}

fn dsh_log_path(root: &Path) -> PathBuf {
    root.join(PROJECT)
        .join(format!("session-{DSH_SESSION}"))
        .join("session.v3.jsonl.zstd")
}

fn dsh_frame(lines: &[String]) -> Vec<u8> {
    let mut text = String::new();
    for line in lines {
        text.push_str(line);
        text.push('\n');
    }
    zstd::stream::encode_all(text.as_bytes(), 3).expect("zstd frame")
}

fn dsh_header(version: u32) -> String {
    json!({
        "type": "session",
        "version": version,
        "id": DSH_SESSION,
        "createdAt": 1_782_000_000_000i64,
        "cwd": "C:\\work\\proj"
    })
    .to_string()
}

fn dsh_event(seq: u64, text: &str) -> String {
    json!({
        "type": "user/message",
        "seq": seq,
        "time": 1_782_000_000_000i64 + seq as i64,
        "data": {"id": format!("m-{seq}"), "role": "user",
                 "source": {"kind": "user"},
                 "content": [{"type": "text", "text": text}]}
    })
    .to_string()
}

fn dsh_title_event(seq: u64, title: &str) -> String {
    json!({
        "type": "session/title",
        "seq": seq,
        "time": 1_782_000_000_000i64 + seq as i64,
        "data": {"title": title}
    })
    .to_string()
}

fn write_dsh(path: &Path, frames: &[Vec<u8>]) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("dsh dir");
    }
    let mut bytes = Vec::new();
    for frame in frames {
        bytes.extend_from_slice(frame);
    }
    std::fs::write(path, bytes).expect("write dsh log");
}

#[test]
fn dsh_append_and_generation_switch_are_equivalent() {
    let env = support::TestEnv::new();
    let root = env.root("dsh-root");
    let path = dsh_log_path(&root);
    write_dsh(
        &path,
        &[dsh_frame(&[dsh_header(3), dsh_event(1, "needle dsh one")])],
    );
    let pair = m::Pair::new(&env, "dsh-append");
    let (incremental, rebuilt) = pair_configs(
        &env,
        "dsh-append",
        &[source("dsh", "dsh", &root)],
        &["dsh"],
        &pair,
        false,
    );

    round(&incremental);
    // Append: one more frame, same generation. The checkpoint resumes from the
    // recorded line, so no carrier is re-read.
    {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("open dsh log");
        file.write_all(&dsh_frame(&[dsh_event(2, "needle dsh two")]))
            .expect("append frame");
    }
    let appended = round(&incremental);
    let cost = cost_of(&appended, "dsh");
    assert_eq!(
        m::counter_sum(&cost, "index.full_rescan."),
        0,
        "an appended frame is read from the checkpoint, not re-scanned: {cost}"
    );
    // A touched carrier (same bytes, new mtime) is a recorded fact, not a
    // re-decode: the probes cover the prefix and the anchor at the old length.
    m::set_mtime(&path, m::mtime(&path) + std::time::Duration::from_secs(7));
    let touched = round(&incremental);
    let cost = cost_of(&touched, "dsh");
    assert_eq!(
        m::counter_sum(&cost, "index.full_rescan."),
        0,
        "a touched carrier is not re-read: {cost}"
    );
    assert_eq!(
        m::counter(&cost, "index.mtime_only_refresh"),
        1,
        "the touch is named and counted: {cost}"
    );
    // A newer generation replaces the session's rows: the carrier identity
    // changed, so exactly one whole-carrier read.
    write_dsh(
        &root
            .join(PROJECT)
            .join(format!("session-{DSH_SESSION}"))
            .join("session.v4.jsonl.zstd"),
        &[dsh_frame(&[dsh_header(4), dsh_event(3, "needle dsh v4")])],
    );
    let switched = round(&incremental);
    let cost = cost_of(&switched, "dsh");
    assert_eq!(
        m::counter(&cost, "index.full_rescan.file_identity_changed"),
        1,
        "the generation switch re-reads exactly the new carrier: {cost}"
    );
    assert_eq!(
        m::counter_sum(&cost, "index.full_rescan."),
        1,
        "no other carrier is re-read in the generation-switch round: {cost}"
    );
    // Non-vacuity guard: the scenario must index real units, so a broken DSH
    // fixture cannot make the equivalence check compare two empty indexes.
    let units = m::digest_one(&incremental, "dsh")
        .units
        .expect("the DSH index holds units")
        .count;
    assert!(units >= 1, "the DSH scenario indexes real units: {units}");
    m::index_rebuild(&rebuilt);
    m::assert_equivalent(&incremental, &rebuilt, "dsh append + generation switch");
}

// ---------------------------------------------------------------------------
// OpenCode (synthetic SQLite)
// ---------------------------------------------------------------------------

const OC_SCHEMA: &str = "
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
";

fn opencode_db(env: &support::TestEnv, name: &str) -> (PathBuf, PathBuf) {
    let root = env.root(name);
    let db = root.join("opencode.db");
    let conn = rusqlite::Connection::open(&db).expect("open db");
    conn.execute_batch(OC_SCHEMA).expect("schema");
    conn.execute(
        "INSERT INTO session_v2 (id, title, directory, time_created, time_updated, parent_id) \
         VALUES ('s-1', 'open code session', 'C:/work', 1000, 2000, NULL)",
        [],
    )
    .expect("session");
    for (id, seq, text) in [
        ("sm-1", 1, "needle opencode first"),
        ("sm-2", 2, "needle opencode second"),
    ] {
        conn.execute(
            "INSERT INTO session_message (id, session_id, seq, type, time_created, data) \
             VALUES (?1, 's-1', ?2, 'user', ?3, ?4)",
            rusqlite::params![
                id,
                seq,
                1000 + seq,
                json!({"content": [{"type": "text", "text": text}]}).to_string()
            ],
        )
        .expect("message");
    }
    drop(conn);
    (root, db)
}

fn opencode_configs(
    env: &support::TestEnv,
    name: &str,
    root: &Path,
    embedding: bool,
) -> (RecallConfig, RecallConfig, m::Pair) {
    let pair = m::Pair::new(env, name);
    let (incremental, rebuilt) = pair_configs(
        env,
        name,
        &[source("oc", "opencode", root)],
        &["oc"],
        &pair,
        embedding,
    );
    (incremental, rebuilt, pair)
}

#[test]
fn opencode_new_row_title_change_and_deletes_are_equivalent() {
    let env = support::TestEnv::new();
    let (root, db) = opencode_db(&env, "oc-basic");
    let (incremental, rebuilt, _pair) = opencode_configs(&env, "oc-basic", &root, false);

    round(&incremental);
    {
        let conn = rusqlite::Connection::open(&db).expect("db");
        conn.execute(
            "INSERT INTO session_message (id, session_id, seq, type, time_created, data) \
             VALUES ('sm-3', 's-1', 3, 'assistant', 2000, ?1)",
            rusqlite::params![json!({"content": [{"type": "text", "text": "needle opencode third"}]})
                .to_string()],
        )
        .expect("new row");
        // Title change (the title is denormalised into every unit).
        conn.execute("UPDATE session_v2 SET title = 'renamed session' WHERE id = 's-1'", [])
            .expect("rename");
    }
    let inserted = round(&incremental);
    let cost = cost_of(&inserted, "oc");
    assert_eq!(
        m::counter_sum(&cost, "index.full_rescan."),
        0,
        "the SQLite index never whole-file-scans a carrier: {cost}"
    );
    assert!(
        m::counter(&cost, "manifest_parse_count") <= 1,
        "no archive is configured, so at most one manifest projection: {cost}"
    );
    {
        let conn = rusqlite::Connection::open(&db).expect("db");
        conn.execute("DELETE FROM session_message WHERE id = 'sm-2'", [])
            .expect("delete child row");
    }
    let child_deleted = round(&incremental);
    let cost = cost_of(&child_deleted, "oc");
    assert_eq!(
        m::counter_sum(&cost, "index.full_rescan."),
        0,
        "a deleted child row costs no file read: {cost}"
    );
    assert!(
        m::counter(&cost, "manifest_parse_count") <= 1,
        "a deleted child row parses no extra manifest: {cost}"
    );
    {
        let conn = rusqlite::Connection::open(&db).expect("db");
        conn.execute("DELETE FROM session_message WHERE session_id = 's-1'", [])
            .expect("delete child rows");
        conn.execute("DELETE FROM session_v2 WHERE id = 's-1'", [])
            .expect("delete session");
    }
    let session_deleted = round(&incremental);
    let cost = cost_of(&session_deleted, "oc");
    assert_eq!(
        m::counter_sum(&cost, "index.full_rescan."),
        0,
        "a deleted session costs no file read: {cost}"
    );
    assert!(
        m::counter(&cost, "manifest_parse_count") <= 1,
        "a deleted session parses no extra manifest: {cost}"
    );

    m::index_rebuild(&rebuilt);
    m::assert_equivalent(&incremental, &rebuilt, "opencode new row/title/delete");
}

#[test]
fn opencode_rowid_reuse_vacuum_and_touch_are_equivalent() {
    let env = support::TestEnv::new();
    let (root, db) = opencode_db(&env, "oc-rowid");
    let (incremental, rebuilt, _pair) = opencode_configs(&env, "oc-rowid", &root, false);

    round(&incremental);
    {
        let conn = rusqlite::Connection::open(&db).expect("db");
        // The source table has a TEXT primary key; rowid is only the implicit
        // insertion counter. Reuse it explicitly after a delete.
        conn.execute("DELETE FROM session_message WHERE id = 'sm-2'", [])
            .expect("delete");
        conn.execute(
            "INSERT INTO session_message (rowid, id, session_id, seq, type, time_created, data) \
             VALUES (2, 'sm-4', 's-1', 4, 'user', 3000, ?1)",
            rusqlite::params![json!({"content": [{"type": "text", "text": "needle opencode reused"}]})
                .to_string()],
        )
        .expect("reuse rowid");
        conn.execute_batch("VACUUM;").expect("vacuum");
        // A watermark touch with no content change: re-projected, same units.
        conn.execute(
            "UPDATE session_message SET time_created = 4000 WHERE id = 'sm-1'",
            [],
        )
        .expect("touch");
    }
    let touched = round(&incremental);
    let cost = cost_of(&touched, "oc");
    assert_eq!(
        m::counter_sum(&cost, "index.full_rescan."),
        0,
        "rowid reuse, VACUUM and a watermark touch cost no file read: {cost}"
    );
    assert!(
        m::counter(&cost, "manifest_parse_count") <= 1,
        "at most one manifest projection for this source: {cost}"
    );
    m::index_rebuild(&rebuilt);
    m::assert_equivalent(&incremental, &rebuilt, "opencode rowid reuse/vacuum/touch");
}

/// `sessions` sessions with `rows_per_session` messages each, written in one
/// transaction. Ids sort in creation order (`s-000`, `s-001`, …), which makes the
/// rolling slice's order observable.
fn opencode_sessions_fixture(
    env: &support::TestEnv,
    name: &str,
    sessions: usize,
    rows_per_session: usize,
    body_bytes: usize,
) -> (PathBuf, PathBuf) {
    let root = env.root(name);
    let db = root.join("opencode.db");
    let conn = rusqlite::Connection::open(&db).expect("open db");
    conn.execute_batch(OC_SCHEMA).expect("schema");
    conn.execute_batch("BEGIN").expect("begin");
    let filler = "z".repeat(body_bytes);
    let mut created = 1_000i64;
    for session in 0..sessions {
        conn.execute(
            "INSERT INTO session_v2 (id, title, directory, time_created, time_updated, parent_id) \
             VALUES (?1, ?2, 'C:/work', 1000, 2000, NULL)",
            rusqlite::params![format!("s-{session:03}"), format!("session {session}")],
        )
        .expect("session");
        for row in 0..rows_per_session {
            created += 1;
            conn.execute(
                "INSERT INTO session_message (id, session_id, seq, type, time_created, data) \
                 VALUES (?1, ?2, ?3, 'user', ?4, ?5)",
                rusqlite::params![
                    format!("sm-{session:03}-{row:03}"),
                    format!("s-{session:03}"),
                    row as i64,
                    created,
                    json!({"content": [{"type": "text", "text": format!("needle s{session} r{row} {filler}")}]})
                        .to_string()
                ],
            )
            .expect("message");
        }
    }
    conn.execute_batch("COMMIT").expect("commit");
    drop(conn);
    (root, db)
}

/// Rewrite the stored OpenCode watermark into its pre-004b shape (no tail
/// pointer, no source stamp, no reconcile cursor), the state an upgrade round
/// finds after a batch boundary.
fn downgrade_opencode_watermark(config: &RecallConfig, source_id: &str) {
    let path = index_path_of(config, source_id);
    let conn = rusqlite::Connection::open(&path).expect("index");
    let raw: String = conn
        .query_row("SELECT value FROM meta WHERE key = 'watermark'", [], |row| {
            row.get(0)
        })
        .expect("watermark row");
    let mut value: Value = serde_json::from_str(&raw).expect("watermark json");
    let map = value.as_object_mut().expect("watermark object");
    for key in [
        "max_rowid",
        "reconcile_cursor",
        "reconcile_round",
        "reconcile_window",
        "db_stamp",
        "wal_stamp",
        "wal_salt",
        "rotation_open",
    ] {
        map.remove(key);
    }
    conn.execute(
        "UPDATE meta SET value = ?1 WHERE key = 'watermark'",
        rusqlite::params![value.to_string()],
    )
    .expect("watermark rewrite");
}

/// A row appended to an idle session is above the recorded `max_rowid`, so the
/// tail sees it in the same round — however old its clock value is.
#[test]
fn opencode_tail_insert_is_visible_in_one_round() {
    let env = support::TestEnv::new();
    let (root, db) = opencode_sessions_fixture(&env, "oc-tail-insert", 48, 1, 64);
    let (incremental, rebuilt, _pair) = opencode_configs(&env, "oc-tail-insert", &root, false);

    round(&incremental);
    {
        let conn = rusqlite::Connection::open(&db).expect("db");
        // `time_created` is older than every row already there: only the rowid
        // makes this row visible, so the round must not rely on the clock.
        conn.execute(
            "INSERT INTO session_message (id, session_id, seq, type, time_created, data) \
             VALUES ('sm-new', 's-000', 99, 'user', 1, ?1)",
            rusqlite::params![json!({"content": [{"type": "text", "text": "needle appended"}]})
                .to_string()],
        )
        .expect("append");
    }

    let reports = round(&incremental);
    let cost = cost_of(&reports, "oc");
    assert_eq!(
        reports[0]["status"],
        json!("updated"),
        "the appended row is visible now: {reports:?}"
    );
    assert_eq!(
        m::counter(&cost, "index.opencode_tail_rows"),
        1,
        "the tail read one row: {cost}"
    );
    assert_eq!(
        m::counter(&cost, "index.opencode_tail_sessions"),
        1,
        "the tail named its session: {cost}"
    );
    assert_eq!(
        m::counter(&cost, "index.opencode_unchanged"),
        0,
        "a written source is never the unchanged path: {cost}"
    );
    assert_eq!(
        m::counter_sum(&cost, "index.full_rescan."),
        0,
        "nothing was re-read in full: {cost}"
    );
    assert!(
        m::counter(&cost, "index.opencode_slice_sessions") <= 1,
        "48 sessions spend one session on the slice: {cost}"
    );

    m::index_rebuild(&rebuilt);
    m::assert_equivalent(&incremental, &rebuilt, "opencode tail insert");
}

/// A child row deleted without any append (so nothing shows above the tail) is
/// found by the rolling slice, and the round is not a rebuild.
#[test]
fn opencode_sub_row_delete_without_new_row_is_caught_by_the_rolling_slice() {
    let env = support::TestEnv::new();
    let (root, db) = opencode_sessions_fixture(&env, "oc-sub-delete", 1, 3, 64);
    let (incremental, rebuilt, _pair) = opencode_configs(&env, "oc-sub-delete", &root, false);

    round(&incremental);
    {
        let conn = rusqlite::Connection::open(&db).expect("db");
        // The first row: the highest rowid is untouched, so `MAX(rowid)` does not
        // move and the tail stays empty.
        conn.execute("DELETE FROM session_message WHERE id = 'sm-000-000'", [])
            .expect("delete");
    }

    let reports = round(&incremental);
    let cost = cost_of(&reports, "oc");
    assert_eq!(reports[0]["status"], json!("updated"), "{reports:?}");
    assert!(
        !reports[0]["rebuilt"].as_bool().unwrap_or(false),
        "a middle-row delete is not a rebuild: {reports:?}"
    );
    assert_eq!(
        m::counter(&cost, "index.opencode_tail_rows"),
        0,
        "nothing was appended: {cost}"
    );
    assert_eq!(
        m::counter(&cost, "index.opencode_stale_rows_removed"),
        1,
        "the slice removed the deleted row's unit: {cost}"
    );
    assert_eq!(
        m::counter(&cost, "index.opencode_reconcile_sessions"),
        1,
        "the single-session corpus is its own slice: {cost}"
    );

    m::index_rebuild(&rebuilt);
    m::assert_equivalent(&incremental, &rebuilt, "opencode sub-row delete");
}

/// `MAX(rowid)` moving backwards (rows removed from the end, a VACUUM that
/// renumbers) invalidates the tail pointer: the round rebuilds instead of
/// resuming in the middle.
#[test]
fn opencode_max_rowid_regression_rebuilds_conservatively() {
    let env = support::TestEnv::new();
    let (root, db) = opencode_sessions_fixture(&env, "oc-rowid-regression", 2, 2, 64);
    let (incremental, rebuilt, _pair) = opencode_configs(&env, "oc-rowid-regression", &root, false);

    round(&incremental);
    {
        let conn = rusqlite::Connection::open(&db).expect("db");
        // The highest rowid goes away: every rowid below it now names a different
        // position than the watermark recorded.
        conn.execute(
            "DELETE FROM session_message WHERE id = 'sm-001-001'",
            [],
        )
        .expect("delete the tail row");
    }
    let reports = round(&incremental);
    assert_eq!(
        reports[0]["status"],
        json!("built"),
        "a rowid regression is a full rebuild: {reports:?}"
    );
    assert_eq!(
        reports[0]["reason"],
        json!("watermark_rowid_regression"),
        "{reports:?}"
    );
    let cost = cost_of(&reports, "oc");
    assert_eq!(
        m::counter(&cost, "index.opencode_max_rowid_queries"),
        1,
        "the decision costs one index dive: {cost}"
    );

    m::index_rebuild(&rebuilt);
    m::assert_equivalent(&incremental, &rebuilt, "opencode rowid regression");

    // The rowid space also moves when a VACUUM renumbers it: the same guard
    // applies (or the session's slice repairs the row), and the round never
    // claims the source is unchanged.
    {
        let conn = rusqlite::Connection::open(&db).expect("db");
        conn.execute("DELETE FROM session_message WHERE id = 'sm-000-000'", [])
            .expect("delete");
        conn.execute_batch("VACUUM;").expect("vacuum");
    }
    let after_vacuum = round(&incremental);
    assert_ne!(
        reports_status(&after_vacuum, "oc"),
        "unchanged",
        "a VACUUM is a visible source change: {after_vacuum:?}"
    );
    m::index_rebuild(&rebuilt);
    m::assert_equivalent(&incremental, &rebuilt, "opencode vacuum");
}

/// A NULL `data`, a row above the per-record ceiling and a session with no
/// messages are all facts the round must survive without inventing or losing
/// index rows.
#[test]
fn opencode_null_data_oversize_and_empty_session_are_equivalent() {
    let env = support::TestEnv::new();
    let (root, db) = opencode_sessions_fixture(&env, "oc-edge-rows", 2, 1, 64);
    let (incremental, rebuilt, _pair) = opencode_configs(&env, "oc-edge-rows", &root, false);

    round(&incremental);
    {
        let conn = rusqlite::Connection::open(&db).expect("db");
        // An empty session: the host knows it, it has no message.
        conn.execute(
            "INSERT INTO session_v2 (id, title, directory, time_created, time_updated, parent_id) \
             VALUES ('s-empty', 'empty session', 'C:/work', 1000, 2000, NULL)",
            [],
        )
        .expect("empty session");
        // A row without a body.
        conn.execute(
            "INSERT INTO session_message (id, session_id, seq, type, time_created, data) \
             VALUES ('sm-null', 's-empty', 1, 'user', 5000, NULL)",
            [],
        )
        .expect("null data row");
        // A row above the 64 MiB ceiling: counted, never indexed.
        conn.execute(
            "INSERT INTO session_message (id, session_id, seq, type, time_created, data) \
             VALUES ('sm-oversize', 's-empty', 2, 'user', 6000, ?1)",
            rusqlite::params![json!({"content": [{"type": "text", "text": "x".repeat(64 * 1024 * 1024 + 1_024)}]})
                .to_string()],
        )
        .expect("oversize row");
    }

    let reports = round(&incremental);
    assert_eq!(
        reports[0]["detail"]["skipped_oversize"],
        json!(1),
        "the row above the ceiling is counted: {reports:?}"
    );
    assert_eq!(
        reports[0]["detail"]["sessions"],
        json!(3),
        "the empty session is mirrored even though it carries no unit: {reports:?}"
    );
    let cost = cost_of(&reports, "oc");
    assert_eq!(
        m::counter(&cost, "index.opencode_tail_rows"),
        0,
        "neither edge row is indexable: {cost}"
    );

    // The empty session is a real session: it is in the slice, its read is empty
    // and it removes nothing.
    let next = round(&incremental);
    let cost = cost_of(&next, "oc");
    assert_eq!(
        m::counter(&cost, "index.opencode_stale_rows_removed"),
        0,
        "an empty session has no stale rows: {cost}"
    );

    m::index_rebuild(&rebuilt);
    m::assert_equivalent(&incremental, &rebuilt, "opencode NULL/oversize/empty session");
}

/// A message emptied **in place** — rowid and clock unchanged, only `data`
/// rewritten — is the form the message-id diff cannot see: the host still holds
/// the row, so nothing is "stale", yet a rebuild would derive no unit from it.
///
/// Every form the projector rejects must therefore drop the live units the index
/// still holds (NULL body, invalid JSON, `content: []`, an empty text block), and
/// it must happen on the rolling slice's ordinary one-session-per-round read.
#[test]
fn opencode_in_place_emptied_projection_is_equivalent() {
    let env = support::TestEnv::new();
    // Two rows per session: the emptied row is the one whose unit must go, the
    // sibling keeps the index non-empty (an empty index would compare trivially).
    let (root, db) = opencode_sessions_fixture(&env, "oc-empty-in-place", 4, 2, 64);
    let (incremental, rebuilt, _pair) =
        opencode_configs(&env, "oc-empty-in-place", &root, false);

    round(&incremental);
    assert_eq!(
        m::digest_one(&incremental, "oc").units.expect("units").count,
        8,
        "the fixture indexes eight units before the emptying"
    );

    {
        let conn = rusqlite::Connection::open(&db).expect("db");
        // Not one timestamp is advanced: only `data` changes, in place.
        conn.execute(
            "UPDATE session_message SET data = NULL WHERE id = 'sm-000-000'",
            [],
        )
        .expect("null body");
        conn.execute(
            "UPDATE session_message SET data = '{not json' WHERE id = 'sm-001-000'",
            [],
        )
        .expect("invalid json");
        conn.execute(
            "UPDATE session_message SET data = '{\"content\":[]}' WHERE id = 'sm-002-000'",
            [],
        )
        .expect("empty content");
        conn.execute(
            "UPDATE session_message SET data = '{\"content\":[{\"type\":\"text\",\"text\":\"\"}]}' \
             WHERE id = 'sm-003-000'",
            [],
        )
        .expect("empty text block");
    }
    m::index_rebuild(&rebuilt);
    let target = m::digest_one(&rebuilt, "oc");
    assert_eq!(
        target.units.expect("units").count,
        4,
        "a rebuild derives no unit from an emptied message"
    );

    let mut pruned = 0u64;
    for index in 0..4u32 {
        let reports = round(&incremental);
        let cost = cost_of(&reports, "oc");
        let units = m::counter(&cost, "index.opencode_unprojectable_units_removed");
        assert_eq!(
            units, 1,
            "round {} drops exactly the emptied message's unit: {cost}",
            index + 1
        );
        assert_eq!(
            m::counter(&cost, "index.opencode_unprojectable_rows"),
            1,
            "round {} names the unprojectable host row: {cost}",
            index + 1
        );
        // The row still exists in the host, so the id-set diff must not be what
        // removes it — this is the prune doing the work.
        assert_eq!(
            m::counter(&cost, "index.opencode_stale_rows_removed"),
            0,
            "round {}: the row is not stale, only unprojectable: {cost}",
            index + 1
        );
        // Cost shape: one sliced session, read through the session_id index.
        assert_eq!(
            m::counter(&cost, "index.opencode_reconcile_sessions"),
            1,
            "round {} reconciles one session: {cost}",
            index + 1
        );
        assert_eq!(
            m::counter(&cost, "index.opencode_host_rows_read"),
            2,
            "round {} reads that session's two rows and nothing else: {cost}",
            index + 1
        );
        assert_eq!(
            m::counter(&cost, "index.opencode_tail_rows"),
            0,
            "round {} appends nothing: {cost}",
            index + 1
        );
        pruned += units;
    }
    assert_eq!(pruned, 4, "every emptied message lost its unit once");

    m::assert_equivalent(
        &incremental,
        &rebuilt,
        "opencode in-place emptied projection (NULL/invalid/empty)",
    );
}

/// A message that grows **above the per-record ceiling** in place: a rebuild
/// skips it, so the increment must drop its old units too — and the ceiling
/// count stays a *current* count instead of a per-round witness.
#[test]
fn opencode_row_grown_oversize_in_place_is_equivalent_and_counted_currently() {
    let env = support::TestEnv::new();
    let (root, db) = opencode_sessions_fixture(&env, "oc-oversize-in-place", 2, 1, 64);
    let (incremental, rebuilt, _pair) =
        opencode_configs(&env, "oc-oversize-in-place", &root, false);

    let first = round(&incremental);
    assert!(
        first[0]["detail"].get("skipped_oversize").is_none(),
        "a normal corpus has nothing above the ceiling: {first:?}"
    );

    {
        let conn = rusqlite::Connection::open(&db).expect("db");
        conn.execute(
            "UPDATE session_message SET data = ?1 WHERE id = 'sm-000-000'",
            rusqlite::params![json!({"content": [{"type": "text", "text": "x".repeat(64 * 1024 * 1024 + 1_024)}]})
                .to_string()],
        )
        .expect("grow in place");
    }

    let grown = round(&incremental);
    assert_eq!(
        grown[0]["detail"]["skipped_oversize"],
        json!(1),
        "the row above the ceiling is counted: {grown:?}"
    );
    let cost = cost_of(&grown, "oc");
    assert_eq!(
        m::counter(&cost, "index.opencode_unprojectable_units_removed"),
        1,
        "the oversized message drops the unit the index still held: {cost}"
    );
    assert_eq!(
        stored_watermark(&incremental, "oc")["oversize_rows"]
            .as_array()
            .map(|rows| rows.len()),
        Some(1),
        "the watermark records the row it witnessed"
    );

    // The next round visits the other session: the count is the *table's*, not
    // this round's witness list, so it must not drop to zero.
    let other = round(&incremental);
    assert_eq!(
        other[0]["detail"]["skipped_oversize"],
        json!(1),
        "a round that reaches another session still reports the oversized row: {other:?}"
    );
    m::index_rebuild(&rebuilt);
    m::assert_equivalent(&incremental, &rebuilt, "opencode row grown above the ceiling");

    // Shrinking it back in place clears the entry once its session is read.
    {
        let conn = rusqlite::Connection::open(&db).expect("db");
        conn.execute(
            "UPDATE session_message SET data = ?1 WHERE id = 'sm-000-000'",
            rusqlite::params![json!({"content": [{"type": "text", "text": "needle back under the ceiling"}]})
                .to_string()],
        )
        .expect("shrink in place");
    }
    m::index_rebuild(&rebuilt);
    let mut cleared = false;
    for _ in 0..3u32 {
        let reports = round(&incremental);
        if reports[0]["detail"].get("skipped_oversize").is_none() {
            cleared = true;
            break;
        }
    }
    assert!(
        cleared,
        "the count follows the table: a shrunk row stops being counted within the window"
    );
    assert_eq!(
        stored_watermark(&incremental, "oc")["oversize_rows"]
            .as_array()
            .map(|rows| rows.len()),
        Some(0),
        "the watermark forgets the row that is no longer above the ceiling"
    );
    m::assert_equivalent(&incremental, &rebuilt, "opencode row shrunk back under the ceiling");
}

/// The prune only ever deletes `origin = 'live'` units. A session that comes back
/// from the archive can hold archive-origin rows for the same message while it is
/// already being reconciled; emptying the host row must not delete that evidence.
/// (The sanctioned removal of archive rows is `remove_archive_session`, not the
/// live projection.)
#[test]
fn opencode_prune_never_drops_archive_origin_rows() {
    let env = support::TestEnv::new();
    let (root, db) = opencode_sessions_fixture(&env, "oc-prune-archive", 2, 1, 64);
    let (incremental, _rebuilt, _pair) =
        opencode_configs(&env, "oc-prune-archive", &root, false);

    round(&incremental);
    // Stand in for "this session is archive-backed and live again": the index
    // holds archive-origin rows for `s-000`'s message.
    {
        let conn =
            rusqlite::Connection::open(index_path_of(&incremental, "oc")).expect("index");
        for table in ["doc_keys", "units"] {
            let changed = conn
                .execute(
                    &format!(
                        "UPDATE {table} SET origin = 'archive' \
                         WHERE session_id = 's-000' AND message_id = 'sm-000-000'"
                    ),
                    [],
                )
                .expect("mark archive origin");
            assert!(changed >= 1, "{table}: the fixture row exists");
        }
    }
    {
        let conn = rusqlite::Connection::open(&db).expect("db");
        // Empty the message in place: the live projection now yields nothing.
        conn.execute(
            "UPDATE session_message SET data = '{\"content\":[]}' WHERE id = 'sm-000-000'",
            [],
        )
        .expect("empty in place");
    }

    // The slice starts at `s-000` (the cursor is unset after the full build).
    let reports = round(&incremental);
    let cost = cost_of(&reports, "oc");
    assert_eq!(
        m::counter(&cost, "index.opencode_unprojectable_rows"),
        1,
        "the emptied row is seen: {cost}"
    );
    assert_eq!(
        m::counter(&cost, "index.opencode_unprojectable_units_removed"),
        0,
        "the prune leaves archive-origin rows alone: {cost}"
    );
    let conn = rusqlite::Connection::open(index_path_of(&incremental, "oc")).expect("index");
    let archive_rows: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM doc_keys \
             WHERE session_id = 's-000' AND message_id = 'sm-000-000' AND origin = 'archive'",
            [],
            |row| row.get(0),
        )
        .expect("count archive rows");
    assert_eq!(archive_rows, 1, "the archive-origin row survived the prune");
}

/// The **tail** half of the same rule, isolated: a message is deleted and
/// re-inserted under the same id (a new rowid, so the tail sees it) with a body
/// that cannot project, in a session the slice does not reach this round. The
/// id is still in the host, so only the tail's prune can drop the old units.
#[test]
fn opencode_tail_row_that_cannot_project_drops_the_reused_message_units() {
    let env = support::TestEnv::new();
    let (root, db) = opencode_sessions_fixture(&env, "oc-tail-unprojectable", 2, 1, 64);
    let (incremental, rebuilt, _pair) =
        opencode_configs(&env, "oc-tail-unprojectable", &root, false);

    round(&incremental);
    // Walk one slice so the cursor sits on `s-000`: the next round starts after
    // it, which keeps the repaired session out of the slice. The touch is a real
    // content change above the tail (rowid unchanged), so it opens the rotation
    // and only the slice can see it.
    {
        let conn = rusqlite::Connection::open(&db).expect("db");
        conn.execute(
            "UPDATE session_message SET time_created = 1500 WHERE id = 'sm-001-000'",
            [],
        )
        .expect("touch");
    }
    round(&incremental);
    assert_eq!(
        stored_watermark(&incremental, "oc")["reconcile_cursor"],
        json!("s-000"),
        "the rotation's cursor is observable"
    );
    {
        let conn = rusqlite::Connection::open(&db).expect("db");
        conn.execute("DELETE FROM session_message WHERE id = 'sm-000-000'", [])
            .expect("delete");
        conn.execute(
            "INSERT INTO session_message (id, session_id, seq, type, time_created, data) \
             VALUES ('sm-000-000', 's-000', 0, 'user', 1001, NULL)",
            [],
        )
        .expect("re-insert without a body");
    }
    m::index_rebuild(&rebuilt);

    let reports = round(&incremental);
    let cost = cost_of(&reports, "oc");
    assert_eq!(
        m::counter(&cost, "index.opencode_unprojectable_rows"),
        1,
        "the tail row is seen as unprojectable: {cost}"
    );
    assert_eq!(
        m::counter(&cost, "index.opencode_unprojectable_units_removed"),
        1,
        "the tail drops the units the reused message id still owned: {cost}"
    );
    assert_eq!(
        m::counter(&cost, "index.opencode_tail_rows"),
        0,
        "an unprojectable tail row is not handed to the projector: {cost}"
    );
    assert_eq!(
        m::counter(&cost, "index.opencode_reconcile_sessions"),
        1,
        "the slice visits the other session: {cost}"
    );
    assert_eq!(
        m::counter(&cost, "index.opencode_stale_rows_removed"),
        0,
        "the id is still in the host, so the id-set diff cannot be the remover: {cost}"
    );
    m::assert_equivalent(
        &incremental,
        &rebuilt,
        "opencode tail row that cannot project (reused message id)",
    );
}

/// A watermark written before this batch (no tail pointer) upgrades by running
/// the old conservative rule exactly once: nothing is skipped, and the next
/// round is a tail round.
#[test]
fn opencode_watermark_upgrade_round_is_conservative_and_equivalent() {
    let env = support::TestEnv::new();
    let (root, db) = opencode_sessions_fixture(&env, "oc-wm-upgrade", 4, 2, 64);
    let (incremental, rebuilt, _pair) = opencode_configs(&env, "oc-wm-upgrade", &root, false);

    round(&incremental);
    assert!(
        stored_watermark(&incremental, "oc")["max_rowid"].is_i64(),
        "the tail pointer is recorded"
    );
    downgrade_opencode_watermark(&incremental, "oc");
    assert!(
        stored_watermark(&incremental, "oc")["max_rowid"].is_null(),
        "the fixture is the pre-004b shape"
    );

    {
        let conn = rusqlite::Connection::open(&db).expect("db");
        // Both an append and an invisible in-place edit: the legacy round has to
        // cover the first through the clock watermark and the second through the
        // full key-set reconcile, as it did before the batch.
        conn.execute(
            "INSERT INTO session_message (id, session_id, seq, type, time_created, data) \
             VALUES ('sm-upgrade-new', 's-000', 9, 'user', 9000, ?1)",
            rusqlite::params![json!({"content": [{"type": "text", "text": "needle upgrade append"}]})
                .to_string()],
        )
        .expect("append");
        conn.execute(
            "UPDATE session_message SET data = ?1 WHERE id = 'sm-000-000'",
            rusqlite::params![json!({"content": [{"type": "text", "text": "needle upgrade edit"}]})
                .to_string()],
        )
        .expect("in-place edit");
    }

    let upgrade = round(&incremental);
    let cost = cost_of(&upgrade, "oc");
    assert_eq!(
        m::counter(&cost, "index.opencode_legacy_rounds"),
        1,
        "the upgrade round runs the old rule once: {cost}"
    );
    assert_eq!(
        reports_status(&upgrade, "oc"),
        "updated",
        "the upgrade round applies the append: {upgrade:?}"
    );
    assert!(
        m::counter(&cost, "index.opencode_host_rows_read") >= 9,
        "the legacy round reads the whole table (8 rows + 1 append): {cost}"
    );
    assert_eq!(
        m::counter(&cost, "index.opencode_rotation_open"),
        1,
        "a rotation follows the upgrade round, because the old rule cannot see an in-place edit: {cost}"
    );
    assert!(
        stored_watermark(&incremental, "oc")["max_rowid"].is_i64(),
        "the upgrade round records the tail pointer"
    );

    // The old rule never saw the in-place edit — that was the known gap. The
    // rotation that follows closes it, which is why the upgrade round opens one.
    m::index_rebuild(&rebuilt);
    let target = m::digest_one(&rebuilt, "oc");
    assert!(
        !agent_memory::recall::fulltext::digest::diff(&m::digest_one(&incremental, "oc"), &target)
            .is_empty(),
        "the legacy round still cannot see an in-place text edit"
    );
    let mut converged = None;
    for index in 0..4u32 {
        let reports = round(&incremental);
        let cost = cost_of(&reports, "oc");
        assert_eq!(
            m::counter(&cost, "index.opencode_legacy_rounds"),
            0,
            "the legacy rule ran once: {cost}"
        );
        assert_eq!(
            m::counter(&cost, "index.opencode_slice_sessions"),
            1,
            "the rotation walks one session per round: {cost}"
        );
        if converged.is_none()
            && agent_memory::recall::fulltext::digest::diff(
                &m::digest_one(&incremental, "oc"),
                &target,
            )
            .is_empty()
        {
            converged = Some(index + 1);
        }
    }
    assert!(
        converged.is_some_and(|rounds| rounds <= 4),
        "the edit surfaces inside the first rotation: {converged:?}"
    );

    // The rotation is closed: the next quiet round takes the fast path again.
    let idle = round(&incremental);
    let cost = cost_of(&idle, "oc");
    assert_eq!(
        m::counter(&cost, "index.opencode_unchanged"),
        1,
        "nothing changed since the rotation closed: {cost}"
    );
    assert_eq!(
        m::counter(&cost, "index.opencode_legacy_rounds"),
        0,
        "the upgrade path is never taken twice: {cost}"
    );

    m::assert_equivalent(&incremental, &rebuilt, "opencode watermark upgrade");
}

/// The unchanged fast path: no host write means no host read at all. A write
/// into the WAL and a checkpoint are both seen, so the path cannot mistake them
/// for "nothing happened".
#[test]
fn opencode_unchanged_round_skips_the_host_and_wal_changes_are_seen() {
    let env = support::TestEnv::new();
    let (root, db) = opencode_sessions_fixture(&env, "oc-unchanged", 3, 2, 64);
    {
        let conn = rusqlite::Connection::open(&db).expect("db");
        let mode: String = conn
            .query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))
            .expect("wal mode");
        assert_eq!(mode.to_lowercase(), "wal");
    }
    let (incremental, rebuilt, _pair) = opencode_configs(&env, "oc-unchanged", &root, false);

    round(&incremental);
    let idle = round(&incremental);
    let cost = cost_of(&idle, "oc");
    assert_eq!(reports_status(&idle, "oc"), "unchanged", "{idle:?}");
    assert_eq!(
        m::counter(&cost, "index.opencode_unchanged"),
        1,
        "an untouched source takes the fast path: {cost}"
    );
    assert_eq!(
        m::counter(&cost, "index.opencode_host_rows_read"),
        0,
        "the fast path reads no host table: {cost}"
    );
    assert_eq!(
        m::counter(&cost, "index.opencode_max_rowid_queries"),
        1,
        "one index dive decides the tail position: {cost}"
    );
    let before = stored_watermark(&incremental, "oc");
    assert!(
        before["db_stamp"]["size"].is_u64() || before["db_stamp"]["mtime_ns"].is_u64(),
        "the source stamp is recorded: {before}"
    );

    // A write lands in the WAL: size and mtime of the WAL move, so the round is
    // not the unchanged path and the tail sees the row.
    {
        let conn = rusqlite::Connection::open(&db).expect("db");
        conn.execute(
            "INSERT INTO session_message (id, session_id, seq, type, time_created, data) \
             VALUES ('sm-wal', 's-000', 7, 'user', 7000, ?1)",
            rusqlite::params![json!({"content": [{"type": "text", "text": "needle wal append"}]})
                .to_string()],
        )
        .expect("append in wal mode");
    }
    let written = round(&incremental);
    let cost = cost_of(&written, "oc");
    assert_eq!(
        m::counter(&cost, "index.opencode_unchanged"),
        0,
        "a WAL write is not mistaken for 'unchanged': {cost}"
    );
    assert_eq!(
        m::counter(&cost, "index.opencode_tail_rows"),
        1,
        "the appended row is read: {cost}"
    );

    // Checkpoint: the WAL is folded into the database. Its content did not
    // change, but the files did, and the round must not claim 'unchanged'.
    {
        let conn = rusqlite::Connection::open(&db).expect("db");
        conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
            .expect("checkpoint");
    }
    let after_checkpoint = round(&incremental);
    let cost = cost_of(&after_checkpoint, "oc");
    assert_eq!(
        m::counter(&cost, "index.opencode_unchanged"),
        0,
        "a checkpoint is not mistaken for 'unchanged': {cost}"
    );
    assert_eq!(
        reports_status(&after_checkpoint, "oc"),
        "unchanged",
        "a checkpoint changes no content: {after_checkpoint:?}"
    );
    assert_eq!(
        m::counter(&cost, "index.opencode_tail_rows"),
        0,
        "a checkpoint appends nothing: {cost}"
    );
    let after = stored_watermark(&incremental, "oc");
    assert_ne!(
        before["db_stamp"]["mtime_ns"], after["db_stamp"]["mtime_ns"],
        "the database stamp followed the checkpoint: {before} / {after}"
    );

    m::index_rebuild(&rebuilt);
    m::assert_equivalent(&incremental, &rebuilt, "opencode unchanged/wal/checkpoint");
}

/// The slice is a rotation: every session is visited exactly once per window and
/// an update or a delete inside the window is found, at a cost that is the slice
/// and not the corpus.
#[test]
fn opencode_rolling_slice_covers_every_session_within_the_window() {
    let env = support::TestEnv::new();
    let sessions = 96usize;
    let (root, db) = opencode_sessions_fixture(&env, "oc-rotation", sessions, 1, 64);
    let (incremental, rebuilt, _pair) = opencode_configs(&env, "oc-rotation", &root, false);

    round(&incremental);
    // Start a rotation with one append: a round that observed a write keeps the
    // rotation open, so the following (quiet) rounds walk one slice each.
    {
        let conn = rusqlite::Connection::open(&db).expect("db");
        conn.execute(
            "INSERT INTO session_message (id, session_id, seq, type, time_created, data) \
             VALUES ('sm-rotation', 's-095', 9, 'user', 9000, ?1)",
            rusqlite::params![json!({"content": [{"type": "text", "text": "needle rotation start"}]})
                .to_string()],
        )
        .expect("append");
        // An invisible edit and an invisible delete, in two late sessions, so the
        // window has to be walked to find them.
        conn.execute(
            "UPDATE session_message SET data = ?1 WHERE id = 'sm-090-000'",
            rusqlite::params![json!({"content": [{"type": "text", "text": "needle rotated edit"}]})
                .to_string()],
        )
        .expect("edit");
        conn.execute("DELETE FROM session_message WHERE id = 'sm-091-000'", [])
            .expect("delete");
    }

    m::index_rebuild(&rebuilt);
    let target = m::digest_one(&rebuilt, "oc");
    let mut converged = None;
    let mut stale_removed = 0u64;
    let mut reconcile_sessions = 0u64;
    let mut reconcile_rows = 0u64;
    for index in 0..48u32 {
        let reports = round(&incremental);
        let cost = cost_of(&reports, "oc");
        assert_eq!(
            m::counter(&cost, "index.opencode_slice_sessions"),
            2,
            "the slice stays at two sessions: {cost}"
        );
        assert!(
            m::counter(&cost, "index.opencode_reconcile_sessions") >= 2,
            "the round reconciles its slice: {cost}"
        );
        assert_eq!(
            m::counter_sum(&cost, "index.full_rescan."),
            0,
            "a rotation never re-reads a carrier: {cost}"
        );
        let tail_rows = m::counter(&cost, "index.opencode_tail_rows");
        if index == 0 {
            assert_eq!(tail_rows, 1, "the appended row is read in the first round: {cost}");
        } else {
            assert_eq!(tail_rows, 0, "nothing else was appended: {cost}");
        }
        reconcile_sessions += m::counter(&cost, "index.opencode_reconcile_sessions");
        reconcile_rows += m::counter(&cost, "index.opencode_reconcile_rows");
        stale_removed += m::counter(&cost, "index.opencode_stale_rows_removed");
        if converged.is_none()
            && agent_memory::recall::fulltext::digest::diff(
                &m::digest_one(&incremental, "oc"),
                &target,
            )
            .is_empty()
        {
            converged = Some(index + 1);
        }
    }
    assert_eq!(
        converged,
        Some(46),
        "the edit in slice 45 and the delete in slice 46 surface in round 46"
    );
    assert_eq!(stale_removed, 1, "the deleted row is removed exactly once");
    assert!(
        (96..=96 + 2).contains(&reconcile_sessions),
        "one rotation reads every session once (plus the tail's session): {reconcile_sessions}"
    );
    assert!(
        reconcile_rows <= 96 + 2,
        "one rotation reads one row per session: {reconcile_rows}"
    );

    // The rotation closed on the last round; one more round confirms it and the
    // next one is free again.
    round(&incremental);
    let idle = round(&incremental);
    let cost = cost_of(&idle, "oc");
    assert_eq!(
        m::counter(&cost, "index.opencode_unchanged"),
        1,
        "the fast path returns once the rotation closes: {cost}"
    );
    assert_eq!(
        m::counter(&cost, "index.opencode_host_rows_read"),
        0,
        "the closed rotation costs no host read: {cost}"
    );
    m::assert_equivalent(&incremental, &rebuilt, "opencode rotation");

    // A second corpus size: 192 sessions spend 4 per slice instead of 2, so the
    // per-round rows follow the slice (1/48 of the sessions), not the corpus.
    let (big_root, big_db) = opencode_sessions_fixture(&env, "oc-rotation-large", 192, 1, 64);
    let big_pair = m::Pair::new(&env, "oc-rotation-large");
    let (big, _big_rebuilt) = pair_configs(
        &env,
        "oc-rotation-large",
        &[source("oc", "opencode", &big_root)],
        &["oc"],
        &big_pair,
        false,
    );
    round(&big);
    {
        let conn = rusqlite::Connection::open(&big_db).expect("db");
        conn.execute(
            "INSERT INTO session_message (id, session_id, seq, type, time_created, data) \
             VALUES ('sm-big', 's-000', 9, 'user', 9500, ?1)",
            rusqlite::params![json!({"content": [{"type": "text", "text": "needle big rotation"}]})
                .to_string()],
        )
        .expect("append");
    }
    let cost = cost_of(&round(&big), "oc");
    assert_eq!(
        m::counter(&cost, "index.opencode_slice_sessions"),
        4,
        "192 sessions spend four per slice: {cost}"
    );
    assert_eq!(
        m::counter(&cost, "index.opencode_reconcile_rows"),
        5,
        "the four slice sessions plus the appended row (it belongs to a slice session): {cost}"
    );
    assert_eq!(
        m::counter(&cost, "index.opencode_reconcile_window"),
        48,
        "the rotation stays one day: {cost}"
    );
    assert_eq!(
        m::counter(&cost, "index.opencode_tail_rows"),
        1,
        "the append is still a tail row: {cost}"
    );
}

/// Two corpora that differ only in body size: an idle round is equal (the fast
/// path touches no host table) and a change round costs the change, not the
/// corpus.
#[test]
fn opencode_round_cost_depends_on_the_change_not_the_corpus() {
    let env = support::TestEnv::new();
    let mut idle = Vec::new();
    let mut changed = Vec::new();
    let mut rebuilt_costs = Vec::new();
    for (name, body) in [("oc-cost-small", 1_000usize), ("oc-cost-large", 200_000)] {
        let (root, db) = opencode_sessions_fixture(&env, name, 48, 1, body);
        let pair = m::Pair::new(&env, name);
        let (incremental, rebuilt) = pair_configs(
            &env,
            name,
            &[source("oc", "opencode", &root)],
            &["oc"],
            &pair,
            false,
        );
        round(&incremental);
        let idle_cost = cost_of(&round(&incremental), "oc");
        assert_eq!(
            m::counter(&idle_cost, "index.opencode_unchanged"),
            1,
            "{name}: an untouched source is the fast path: {idle_cost}"
        );
        assert_eq!(
            m::counter(&idle_cost, "index.opencode_host_rows_read"),
            0,
            "{name}: the fast path reads no host row: {idle_cost}"
        );

        {
            let conn = rusqlite::Connection::open(&db).expect("db");
            conn.execute(
                "INSERT INTO session_message (id, session_id, seq, type, time_created, data) \
                 VALUES ('sm-change', 's-000', 5, 'user', 9000, ?1)",
                rusqlite::params![json!({"content": [{"type": "text", "text": "needle one change"}]})
                    .to_string()],
            )
            .expect("append");
        }
        let change_cost = cost_of(&round(&incremental), "oc");
        assert_eq!(
            m::counter(&change_cost, "index.opencode_tail_rows"),
            1,
            "{name}: one appended row: {change_cost}"
        );
        assert!(
            m::counter(&change_cost, "index.opencode_host_rows_read") <= 3,
            "{name}: the change round reads the change, not the corpus: {change_cost}"
        );
        // For contrast: a full rebuild reads the whole host table. The two
        // steady rounds above must stay far below it.
        let rebuild_reports = agent_memory::recall::fulltext::build::index_all(&rebuilt, None, true)
            .expect("rebuild");
        let rebuild_values = serde_json::to_value(&rebuild_reports).expect("reports");
        let rebuild_cost = cost_of(rebuild_values.as_array().expect("array"), "oc");
        idle.push(idle_cost);
        changed.push(change_cost);
        rebuilt_costs.push(rebuild_cost);
    }
    for (index, name) in ["1 KB", "200 KB"].iter().enumerate() {
        eprintln!(
            "opencode round read_bytes ({name} bodies): idle {} B, one append {} B, rebuild {} B",
            idle[index]["read_bytes"],
            changed[index]["read_bytes"],
            rebuilt_costs[index]["read_bytes"],
        );
    }
    // The 9.6 MB corpus makes the contrast explicit: a rebuild reads the table,
    // the steady rounds do not.
    if let (Some(rebuild), Some(change)) = (
        rebuilt_costs[1]["read_bytes"].as_u64(),
        changed[1]["read_bytes"].as_u64(),
    ) {
        assert!(
            rebuild > 4 * change,
            "a rebuild reads the corpus, a change round does not: {rebuild} B vs {change} B"
        );
    }

    // The two idle rounds read the same thing whatever the body volume: the
    // index bookkeeping, never the host database body.
    if let (Some(small), Some(large)) = (
        idle[0]["read_bytes"].as_u64(),
        idle[1]["read_bytes"].as_u64(),
    ) {
        eprintln!(
            "opencode no-change round read_bytes: 1 KB bodies {small} B, 200 KB bodies {large} B"
        );
        assert!(
            large <= small + 2 * 1024 * 1024,
            "an idle round must not scale with the corpus: {small} B vs {large} B"
        );
    }
    for cost in &changed {
        assert_eq!(
            m::counter_sum(cost, "index.full_rescan."),
            0,
            "a change round re-reads no carrier: {cost}"
        );
    }
}

/// A child row that moves to another session keeps its `rowid`, so only the
/// rolling reconcile sees it: the old session loses the units, the new one gains
/// them, and the increment equals a rebuild.
#[test]
fn opencode_child_row_session_migration_is_equivalent() {
    let env = support::TestEnv::new();
    let (root, db) = opencode_two_sessions(&env, "oc-migrate");
    let (incremental, rebuilt, _pair) = opencode_configs(&env, "oc-migrate", &root, false);

    round(&incremental);
    {
        let conn = rusqlite::Connection::open(&db).expect("db");
        // The row (and its rowid) stays, its session changes: no tail row, no
        // clock change.
        conn.execute(
            "UPDATE session_message SET session_id = 's-2' WHERE id = 'sm-1'",
            [],
        )
        .expect("move the row");
    }

    m::index_rebuild(&rebuilt);
    let target = m::digest_one(&rebuilt, "oc");
    let mut converged = None;
    let mut stale_removed = 0u64;
    let mut reconcile_sessions = 0u64;
    for index in 0..2u32 {
        let reports = round(&incremental);
        let cost = cost_of(&reports, "oc");
        assert_eq!(
            m::counter(&cost, "index.opencode_tail_rows"),
            0,
            "a moved row is not a tail row: {cost}"
        );
        assert_eq!(
            m::counter(&cost, "index.opencode_slice_sessions"),
            1,
            "one session per slice: {cost}"
        );
        reconcile_sessions += m::counter(&cost, "index.opencode_reconcile_sessions");
        stale_removed += m::counter(&cost, "index.opencode_stale_rows_removed");
        if converged.is_none()
            && agent_memory::recall::fulltext::digest::diff(
                &m::digest_one(&incremental, "oc"),
                &target,
            )
            .is_empty()
        {
            converged = Some(index + 1);
        }
    }
    assert_eq!(
        converged,
        Some(2),
        "the move is visible at the end of the first rotation"
    );
    assert_eq!(reconcile_sessions, 2, "both sessions were read once");
    assert_eq!(
        stale_removed, 1,
        "the old session's unit is removed exactly once: {stale_removed}"
    );
    m::assert_equivalent(&incremental, &rebuilt, "opencode child row migration");
}

/// The closed gap list stays closed: no scenario is registered as unfixable.
#[test]
fn no_scenario_is_a_known_gap_anymore() {
    assert!(KNOWN_GAPS.is_empty(), "{KNOWN_GAPS:?}");
    assert!(!is_known_gap("opencode.in_place_text_update_without_timestamp"));
}

/// The closed known gap: an in-place text edit that does not move the row's
/// clock (and therefore never appears above the rowid tail) is the case only the
/// rolling per-session reconcile can see. It is visible after one rotation, not
/// later, and the increment then equals a rebuild.
#[test]
fn opencode_in_place_text_update_without_timestamp_is_equivalent() {
    let env = support::TestEnv::new();
    let (root, db) = opencode_two_sessions(&env, "oc-in-place");
    let (incremental, rebuilt, _pair) = opencode_configs(&env, "oc-in-place", &root, false);

    round(&incremental);
    {
        let conn = rusqlite::Connection::open(&db).expect("db");
        // `sm-3` belongs to the *second* session and keeps its `time_created`
        // (and its rowid), so only the slice can see the edit. The slice starts
        // at the first session, so the edit is invisible for one round and
        // visible in the second — the rotation bound, stated as a test.
        conn.execute(
            "UPDATE session_message SET data = ?1 WHERE id = 'sm-3'",
            rusqlite::params![json!({"content": [{"type": "text", "text": "needle beta EDITED"}]})
                .to_string()],
        )
        .expect("in-place edit");
    }

    // The rebuild side reads the same frozen input (the edit already happened).
    m::index_rebuild(&rebuilt);
    let target = m::digest_one(&rebuilt, "oc");

    // Two sessions, one session per slice: the rotation is two rounds.
    let mut reconcile_rows = 0u64;
    let mut reconcile_sessions = 0u64;
    let mut stale_removed = 0u64;
    let mut converged = None;
    for index in 0..2u32 {
        let reports = round(&incremental);
        let cost = cost_of(&reports, "oc");
        assert_eq!(
            m::counter_sum(&cost, "index.full_rescan."),
            0,
            "an in-place edit costs no file re-read: {cost}"
        );
        assert_eq!(
            m::counter(&cost, "index.opencode_tail_rows"),
            0,
            "nothing was appended, so the tail is empty: {cost}"
        );
        assert_eq!(
            m::counter(&cost, "index.opencode_slice_sessions"),
            1,
            "the slice is one session of the two: {cost}"
        );
        assert_eq!(
            m::counter(&cost, "index.opencode_reconcile_window"),
            2,
            "a two-session corpus rotates in two rounds: {cost}"
        );
        assert_eq!(
            m::counter(&cost, "index.opencode_reconcile_sessions"),
            1,
            "the round reconciles exactly its slice: {cost}"
        );
        reconcile_rows += m::counter(&cost, "index.opencode_reconcile_rows");
        reconcile_sessions += m::counter(&cost, "index.opencode_reconcile_sessions");
        stale_removed += m::counter(&cost, "index.opencode_stale_rows_removed");
        let visible = agent_memory::recall::fulltext::digest::diff(
            &m::digest_one(&incremental, "oc"),
            &target,
        )
        .is_empty();
        if index == 0 {
            assert!(!visible, "the first slice does not include the edited session");
        }
        if visible {
            converged = Some(index + 1);
            break;
        }
    }

    assert_eq!(
        converged,
        Some(2),
        "the edit is visible at the end of the first rotation, never later"
    );
    assert_eq!(reconcile_sessions, 2, "both sessions were read once");
    assert_eq!(reconcile_rows, 4, "each of the four rows was read once");
    assert_eq!(stale_removed, 0, "an edit removes no row");
    m::assert_equivalent(&incremental, &rebuilt, "opencode in-place text update");
}


// ---------------------------------------------------------------------------
// Vectors (fake embedding backend)
// ---------------------------------------------------------------------------

/// Two sessions, so "only the affected session is recomputed" is observable.
fn opencode_two_sessions(env: &support::TestEnv, name: &str) -> (PathBuf, PathBuf) {
    let root = env.root(name);
    let db = root.join("opencode.db");
    let conn = rusqlite::Connection::open(&db).expect("open db");
    conn.execute_batch(OC_SCHEMA).expect("schema");
    for (id, title) in [("s-1", "first session"), ("s-2", "second session")] {
        conn.execute(
            "INSERT INTO session_v2 (id, title, directory, time_created, time_updated, parent_id) \
             VALUES (?1, ?2, 'C:/work', 1000, 2000, NULL)",
            rusqlite::params![id, title],
        )
        .expect("session");
    }
    for (id, session, seq, kind, ts, text) in [
        ("sm-1", "s-1", 1i64, "user", 1_000i64, "needle alpha question"),
        ("sm-2", "s-1", 2, "assistant", 2_000, "needle alpha answer"),
        ("sm-3", "s-2", 1, "user", 3_000, "needle beta question"),
        ("sm-4", "s-2", 2, "assistant", 4_000, "needle beta answer"),
    ] {
        conn.execute(
            "INSERT INTO session_message (id, session_id, seq, type, time_created, data) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                id,
                session,
                seq,
                kind,
                ts,
                json!({"content": [{"type": "text", "text": text}]}).to_string()
            ],
        )
        .expect("message");
    }
    drop(conn);
    (root, db)
}

#[test]
fn vectors_follow_unit_text_title_and_context_changes() {
    let env = support::TestEnv::new();
    let (root, db) = opencode_two_sessions(&env, "oc-vectors");
    let (incremental, rebuilt, _pair) = opencode_configs(&env, "oc-vectors", &root, true);

    let vector_cost = |reports: &[Value]| cost_of(reports, "oc");
    let conversations = |config: &RecallConfig| {
        m::digest_one(config, "oc")
            .conversation_units
            .expect("the index holds conversation units")
            .count
    };

    // 1. Initial build: nothing is cached, so both sessions are recomputed once.
    let first = round(&incremental);
    let cost = vector_cost(&first);
    assert_eq!(
        m::counter(&cost, "vectors.sessions_recomputed"),
        2,
        "a cold cache recomputes every session: {cost}"
    );
    assert_eq!(m::counter(&cost, "vectors.sessions_reused"), 0);
    assert_eq!(
        m::counter(&cost, "vectors.units_read"),
        conversations(&incremental),
        "a cold cache reads every unit once: {cost}"
    );

    // 2. Nothing changed: no body is read and no session is recomputed.
    let idle = round(&incremental);
    let cost = vector_cost(&idle);
    assert_eq!(
        m::counter(&cost, "vectors.sessions_recomputed"),
        0,
        "an unchanged round recomputes nothing: {cost}"
    );
    assert_eq!(m::counter(&cost, "vectors.sessions_reused"), 2);
    assert_eq!(
        m::counter(&cost, "vectors.units_read"),
        0,
        "an unchanged round reads no body: {cost}"
    );

    // 3. One unit's text changes (visible to the index: newer timestamp).
    {
        let conn = rusqlite::Connection::open(&db).expect("db");
        conn.execute(
            "UPDATE session_message SET data = ?1, time_created = 5000 WHERE id = 'sm-2'",
            rusqlite::params![json!({"content": [{"type": "text", "text": "needle alpha EDITED"}]})
                .to_string()],
        )
        .expect("text update");
    }
    let edited = round(&incremental);
    let cost = vector_cost(&edited);
    assert_eq!(
        m::counter(&cost, "vectors.sessions_recomputed"),
        1,
        "only the edited session is recomputed: {cost}"
    );
    assert_eq!(m::counter(&cost, "vectors.sessions_reused"), 1);
    assert_eq!(
        m::counter(&cost, "vectors.units_read"),
        2,
        "one session's units are read, not the corpus: {cost}"
    );

    // 4. A session rename changes the title every one of its inputs leads with.
    {
        let conn = rusqlite::Connection::open(&db).expect("db");
        conn.execute("UPDATE session_v2 SET title = 'vector title' WHERE id = 's-2'", [])
            .expect("title");
    }
    let renamed = round(&incremental);
    let cost = vector_cost(&renamed);
    assert_eq!(
        m::counter(&cost, "vectors.sessions_recomputed"),
        1,
        "the rename recomputes exactly its own session: {cost}"
    );
    assert_eq!(m::counter(&cost, "vectors.sessions_reused"), 1);

    // 5. The preceding user turn changes: the assistant unit's context drifts.
    {
        let conn = rusqlite::Connection::open(&db).expect("db");
        conn.execute(
            "UPDATE session_message SET data = ?1, time_created = 6000 WHERE id = 'sm-1'",
            rusqlite::params![json!({"content": [{"type": "text", "text": "needle alpha question v2"}]})
                .to_string()],
        )
        .expect("context change");
    }
    let drifted = round(&incremental);
    let cost = vector_cost(&drifted);
    assert_eq!(
        m::counter(&cost, "vectors.sessions_recomputed"),
        1,
        "context drift is covered by a whole-session recompute: {cost}"
    );
    assert_eq!(
        m::counter(&cost, "vectors.units_read"),
        2,
        "the whole session is re-read, not just the changed unit: {cost}"
    );
    assert_eq!(m::counter(&cost, "vectors.sessions_reused"), 1);

    // 6. A unit disappears: the session's unit set changed, so it is recomputed.
    {
        let conn = rusqlite::Connection::open(&db).expect("db");
        conn.execute("DELETE FROM session_message WHERE id = 'sm-3'", [])
            .expect("delete a unit");
    }
    let shrunk = round(&incremental);
    let cost = vector_cost(&shrunk);
    assert_eq!(
        m::counter(&cost, "vectors.sessions_recomputed"),
        1,
        "a removed unit recomputes its session: {cost}"
    );
    assert_eq!(
        m::counter(&cost, "vectors.units_read"),
        1,
        "the shrunk session's remaining unit: {cost}"
    );
    assert_eq!(m::counter(&cost, "vectors.sessions_reused"), 1);

    let units = conversations(&incremental);
    assert_eq!(
        m::counter(&cost, "vectors.rows_reused") + m::counter(&cost, "vectors.embedded"),
        units,
        "every unit is either reused or embedded once: {cost}"
    );
    assert!(
        cost["counters"]["vectors.cache_bytes_read"].is_number(),
        "the previous cache size is reported: {cost}"
    );
    assert_eq!(
        m::counter_sum(&cost, "index.full_rescan."),
        0,
        "a unit text/title/context change costs no file re-read: {cost}"
    );

    m::index_rebuild(&rebuilt);
    m::assert_equivalent(&incremental, &rebuilt, "vector inputs and rows");
}

#[test]
fn vectors_previous_layout_is_upgraded_without_re_embedding() {
    let env = support::TestEnv::new();
    let (root, _db) = opencode_two_sessions(&env, "oc-vectors-upgrade");
    let (incremental, rebuilt, pair) = opencode_configs(&env, "oc-vectors-upgrade", &root, true);

    round(&incremental);
    let units = m::digest_one(&incremental, "oc")
        .conversation_units
        .expect("conversation units")
        .count;

    // Rewrite the cache into the previous layout: same rows, no session block.
    // That is what a cache written before implementation-004a looks like.
    let path = agent_memory::recall::fulltext::vectors_path(&pair.incremental_root, "oc");
    m::downgrade_vectors_to_previous_layout(&path);

    // The upgrade round recomputes every input hash once (no session block to
    // compare) but must not re-embed a single row: rows are content-addressed.
    let upgraded = round(&incremental);
    let cost = cost_of(&upgraded, "oc");
    assert_eq!(
        m::counter(&cost, "vectors.sessions_recomputed"),
        2,
        "without a session block every session is recomputed: {cost}"
    );
    assert_eq!(
        m::counter(&cost, "vectors.units_read"),
        units,
        "the input hashes are recomputed from the bodies once: {cost}"
    );
    assert_eq!(
        m::counter(&cost, "vectors.embedded"),
        0,
        "every row is reused by its input hash: {cost}"
    );
    assert_eq!(
        m::counter(&cost, "vectors.rows_reused"),
        units,
        "every unit keeps its stored row: {cost}"
    );
    let vectors = &upgraded
        .iter()
        .find(|report| report["source_id"] == json!("oc"))
        .expect("oc report")["detail"]["vectors"];
    assert_eq!(vectors["status"], json!("updated"), "{vectors}");

    // The upgraded file is byte-identical to a rebuild over the same input.
    m::index_rebuild(&rebuilt);
    m::assert_equivalent(&incremental, &rebuilt, "vector cache upgrade");

    // ... and the next round is a no-op again.
    let idle = round(&incremental);
    let cost = cost_of(&idle, "oc");
    assert_eq!(m::counter(&cost, "vectors.sessions_reused"), 2, "{cost}");
    assert_eq!(m::counter(&cost, "vectors.units_read"), 0, "{cost}");
}

// ---------------------------------------------------------------------------
// Steady-state cost: the FTS5 body is never read by a no-change round
// ---------------------------------------------------------------------------

fn index_path_of(config: &RecallConfig, source_id: &str) -> PathBuf {
    let root = config
        .cache_root
        .as_deref()
        .map(Path::new)
        .expect("cache root");
    agent_memory::recall::fulltext::index_path(root, source_id)
}

/// The `watermark` JSON stored in one index's `meta`.
fn stored_watermark(config: &RecallConfig, source_id: &str) -> Value {
    let conn = rusqlite::Connection::open(index_path_of(config, source_id)).expect("open index");
    let raw: String = conn
        .query_row("SELECT value FROM meta WHERE key = 'watermark'", [], |row| {
            row.get(0)
        })
        .expect("watermark");
    serde_json::from_str(&raw).expect("watermark json")
}

fn store_watermark(config: &RecallConfig, source_id: &str, value: &Value) {
    let conn = rusqlite::Connection::open(index_path_of(config, source_id)).expect("open index");
    conn.execute(
        "UPDATE meta SET value = ?2 WHERE key = 'watermark'",
        rusqlite::params!["watermark", value.to_string()],
    )
    .expect("rewrite watermark");
}

/// Rewrite one index into the layout a pre-004a binary would have left: no
/// narrow columns, no narrow indexes, `user_version = 0`.
fn downgrade_narrow_layout(config: &RecallConfig, source_id: &str) {
    let conn = rusqlite::Connection::open(index_path_of(config, source_id)).expect("index");
    conn.execute_batch(
        "DROP INDEX IF EXISTS doc_keys_unset;
         DROP INDEX IF EXISTS doc_keys_ts;
         DROP INDEX IF EXISTS conversation_units_unset;
         ALTER TABLE doc_keys DROP COLUMN ts;
         ALTER TABLE conversation_units DROP COLUMN title;
         PRAGMA user_version = 0;",
    )
    .expect("downgrade the layout");
}

/// Page size of one index file (the unit of `index.narrow_backfill_pages_read`).
fn index_page_size(config: &RecallConfig, source_id: &str) -> u64 {
    let conn = rusqlite::Connection::open(index_path_of(config, source_id)).expect("index");
    conn.query_row("PRAGMA page_size", [], |row| row.get::<_, i64>(0))
        .expect("page size") as u64
}

/// Size of one index file, the bound the cost assertions divide by.
fn index_file_bytes(config: &RecallConfig, source_id: &str) -> u64 {
    std::fs::metadata(index_path_of(config, source_id))
        .expect("stat")
        .len()
}

/// The `EXPLAIN QUERY PLAN` detail lines of one statement, joined. Positional
/// parameters are bound to `0`: the planner needs every slot filled, the values
/// are irrelevant.
fn plan_of(conn: &rusqlite::Connection, sql: &str) -> String {
    let mut statement = conn
        .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
        .expect("explain");
    let mut plan = String::new();
    let placeholders = sql.matches('?').count();
    let mut rows = statement
        .query(rusqlite::params_from_iter(vec![0i64; placeholders]))
        .expect("plan");
    while let Some(row) = rows.next().expect("plan row") {
        let detail: String = row.get(3).expect("plan detail");
        plan.push_str(&detail);
        plan.push('\n');
    }
    plan
}

/// A statement's plan must never name the FTS5 table: every column of one FTS5
/// row drags its text along, so a plan that reads `units` costs the whole index
/// in I/O. Tokenised so `conversation_units` (a narrow table) does not match.
fn assert_plan_stays_narrow(conn: &rusqlite::Connection, sql: &str) {
    let plan = plan_of(conn, sql);
    assert!(!plan.is_empty(), "the plan is empty: {sql}");
    let tokens: Vec<&str> = plan
        .split(|ch: char| !(ch.is_alphanumeric() || ch == '_'))
        .filter(|token| !token.is_empty())
        .collect();
    assert!(
        !tokens.iter().any(|token| token.eq_ignore_ascii_case("units")),
        "the plan reads the FTS5 table ({sql}): {plan}"
    );
    assert!(
        plan.contains("doc_keys") || plan.contains("conversation_units"),
        "unexpected plan for {sql}: {plan}"
    );
}

/// One session with `units` user messages, each carrying `body_bytes` of text.
fn opencode_bulk(
    env: &support::TestEnv,
    name: &str,
    units: usize,
    body_bytes: usize,
) -> (PathBuf, PathBuf) {
    let root = env.root(name);
    let db = root.join("opencode.db");
    let conn = rusqlite::Connection::open(&db).expect("open db");
    conn.execute_batch(OC_SCHEMA).expect("schema");
    conn.execute(
        "INSERT INTO session_v2 (id, title, directory, time_created, time_updated, parent_id) \
         VALUES ('s-1', 'bulk session', 'C:/work', 1000, 2000, NULL)",
        [],
    )
    .expect("session");
    let filler = "z".repeat(body_bytes);
    conn.execute_batch("BEGIN").expect("begin");
    for index in 0..units {
        conn.execute(
            "INSERT INTO session_message (id, session_id, seq, type, time_created, data) \
             VALUES (?1, 's-1', ?2, 'user', ?3, ?4)",
            rusqlite::params![
                format!("sm-{index}"),
                index as i64,
                1_000 + index as i64,
                json!({"content": [{"type": "text", "text": format!("needle bulk {index} {filler}")}]})
                    .to_string()
            ],
        )
        .expect("message");
    }
    conn.execute_batch("COMMIT").expect("commit");
    drop(conn);
    (root, db)
}

#[test]
fn index_no_change_round_does_not_read_the_fts5_body() {
    let env = support::TestEnv::new();
    // Same unit count, very different body volume: a no-change round must not
    // cost more on the larger source, because none of its work touches the
    // FTS5 table (count comes from the writer's arithmetic, the newest time from
    // the narrow `doc_keys.ts` index, the vector metadata from
    // `conversation_units`).
    let (small_root, _) = opencode_bulk(&env, "oc-bulk-small", 64, 8_000);
    let (large_root, _) = opencode_bulk(&env, "oc-bulk-large", 64, 380_000);
    let small_bytes_on_disk = support::tree_state(&small_root)
        .iter()
        .map(|(_, size, _)| *size)
        .sum::<u64>();
    let large_bytes_on_disk = support::tree_state(&large_root)
        .iter()
        .map(|(_, size, _)| *size)
        .sum::<u64>();
    assert!(
        large_bytes_on_disk > 20 * 1024 * 1024,
        "the large fixture must be a >20 MB body: {large_bytes_on_disk}"
    );
    assert!(
        small_bytes_on_disk * 10 < large_bytes_on_disk,
        "the fixtures must differ by an order of magnitude: {small_bytes_on_disk} / {large_bytes_on_disk}"
    );

    let mut idle_costs = Vec::new();
    let mut configs = Vec::new();
    for (label, root) in [("oc-bulk-small", &small_root), ("oc-bulk-large", &large_root)] {
        let pair = m::Pair::new(&env, label);
        let (incremental, _rebuilt) = pair_configs(
            &env,
            label,
            &[source("oc", "opencode", root)],
            &["oc"],
            &pair,
            true,
        );
        round(&incremental);
        let reports = round(&incremental);
        let cost = cost_of(&reports, "oc");
        assert_eq!(
            m::counter(&cost, "vectors.units_read"),
            0,
            "{label}: a no-change round reads no body: {cost}"
        );
        assert_eq!(
            m::counter(&cost, "vectors.sessions_reused"),
            1,
            "{label}: the session is taken from the cache: {cost}"
        );
        assert_eq!(
            m::counter_sum(&cost, "index.full_rescan."),
            0,
            "{label}: no carrier is re-read: {cost}"
        );
        assert!(
            m::phase(&cost, "finalize"),
            "{label}: the finalize stage is named so the blind zone is visible: {cost}"
        );
        idle_costs.push(cost);
        configs.push(incremental);
    }

    // The two reported counters are still content facts.
    for (config, rows) in configs.iter().zip([64u64, 64]) {
        let counts = m::meta_counts(config, "oc").expect("meta");
        assert_eq!(counts.0, rows, "the unit count is exact");
        assert!(counts.1 > 0, "the newest unit time is real");
    }

    // No statement a refresh runs may read the FTS5 body.
    for config in &configs {
        let index = index_path_of(config, "oc");
        let conn = rusqlite::Connection::open_with_flags(
            &index,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .expect("index");
        assert_plan_stays_narrow(
            &conn,
            agent_memory::recall::fulltext::build::COUNT_UNITS_SQL,
        );
        assert_plan_stays_narrow(
            &conn,
            agent_memory::recall::fulltext::build::NEWEST_UNIT_TS_SQL,
        );
        assert_plan_stays_narrow(
            &conn,
            agent_memory::recall::fulltext::vectors::CONVERSATION_METADATA_SQL,
        );
    }

    // And the measured read volume does not grow with the body (Windows only:
    // the process I/O counters are what §A measures). The small residual comes
    // from the OpenCode **host** database, not from the index: every round reads
    // `COUNT(*)` / `MAX(time_created)` out of `session_message` to carry the
    // source watermark, which is a foreign read-only database (implementation-004b
    // replaces it). The FTS5 side contributes nothing.
    if let (Some(small), Some(large)) = (
        idle_costs[0]["read_bytes"].as_u64(),
        idle_costs[1]["read_bytes"].as_u64(),
    ) {
        eprintln!(
            "no-change round read_bytes: small fixture {small} B ({small_bytes_on_disk} B on disk), \
             large fixture {large} B ({large_bytes_on_disk} B on disk)"
        );
        assert!(
            large <= small + 2 * 1024 * 1024,
            "a no-change round must not scale with the body: {small} B (small) vs {large} B (large)"
        );
        assert!(
            large < 32 * 1024 * 1024,
            "a no-change round stays cheap in absolute terms: {large} B"
        );
    }
}

#[test]
fn jsonl_no_change_round_reads_no_carrier_bytes() {
    // The same two-scale comparison for a JSONL source, where nothing else reads
    // a foreign database: a no-change round must not consume a carrier byte.
    let env = support::TestEnv::new();
    let mut idle = Vec::new();
    for (name, body) in [("cc-bulk-small", 8_000usize), ("cc-bulk-large", 380_000)] {
        let root = env.root(name);
        let path = root.join("proj").join(format!("{SESSION}.jsonl"));
        let long = format!("needle answer {}", "y".repeat(body));
        let mut lines = Vec::new();
        for index in 0..32 {
            lines.push(claude_line(
                SESSION,
                &format!("u{index}"),
                "2026-10-25T13:25:00Z",
                "user",
                &long,
            ));
        }
        write_lines(&path, &lines);
        let pair = m::Pair::new(&env, name);
        let (incremental, _rebuilt) = pair_configs(
            &env,
            name,
            &[source("cc", "claude_code", &root)],
            &["cc"],
            &pair,
            false,
        );
        round(&incremental);
        let reports = round(&incremental);
        let cost = cost_of(&reports, "cc");
        assert_eq!(
            m::counter(&cost, "index.scan_bytes"),
            0,
            "{name}: an unchanged carrier is not read at all: {cost}"
        );
        assert_eq!(
            m::counter_sum(&cost, "index.full_rescan."),
            0,
            "{name}: no rescan: {cost}"
        );
        idle.push((cost, std::fs::metadata(&path).expect("stat").len()));
    }
    if let (Some(small), Some(large)) = (
        idle[0].0["read_bytes"].as_u64(),
        idle[1].0["read_bytes"].as_u64(),
    ) {
        eprintln!(
            "jsonl no-change round read_bytes: small carrier {} B -> {small} B read, \
             large carrier {} B -> {large} B read",
            idle[0].1, idle[1].1
        );
        assert!(
            large <= small + 512 * 1024,
            "a no-change JSONL round is independent of the carrier size: {small} B vs {large} B"
        );
    }
}

// ---------------------------------------------------------------------------
// Narrow-table layout: the one-time backfill of an older index file
// ---------------------------------------------------------------------------

#[test]
fn index_narrow_columns_are_backfilled_once() {
    let env = support::TestEnv::new();
    let (root, _db) = opencode_two_sessions(&env, "oc-backfill");
    let (incremental, rebuilt, _pair) = opencode_configs(&env, "oc-backfill", &root, true);

    round(&incremental);
    let before = m::meta_counts(&incremental, "oc").expect("meta");

    // Simulate a file written before the narrow columns existed.
    downgrade_narrow_layout(&incremental, "oc");

    let reports = round(&incremental);
    let cost = cost_of(&reports, "oc");
    assert!(
        m::phase(&cost, "index_migration"),
        "the one-time backfill is its own stage: {cost}"
    );
    assert!(
        !m::phase(&cost, "index_repair"),
        "the migration leaves a consistent file, so the repair does not restage it \
         a second time: {cost}"
    );
    assert_eq!(
        m::counter_sum(&cost, "index.full_rescan."),
        0,
        "widening the bookkeeping tables is not a rescan: {cost}"
    );
    assert_eq!(
        m::counter(&cost, "vectors.sessions_reused"),
        2,
        "the backfilled title keeps every session reusable: {cost}"
    );
    assert_eq!(
        m::counter(&cost, "vectors.units_read"),
        0,
        "no body is read after the backfill: {cost}"
    );
    assert_eq!(
        m::meta_counts(&incremental, "oc"),
        Some(before),
        "the counts survive the migration unchanged"
    );

    m::index_rebuild(&rebuilt);
    m::assert_equivalent(&incremental, &rebuilt, "narrow-column backfill");

    // ... and the next round does not migrate again.
    let idle = round(&incremental);
    assert!(
        !m::phase(&cost_of(&idle, "oc"), "index_migration"),
        "the backfill runs exactly once"
    );
}

/// The one-time backfill is a **linear** read of the index file.
///
/// Regression for the second-round defect: the backfill stepped a `units` cursor
/// and issued one `UPDATE doc_keys … WHERE rowid = ?` per row. `doc_keys` names a
/// column `rowid`, shadowing the implicit one, and the statement carries no
/// `ts = 0` predicate, so no index could serve it: every row scanned the whole
/// `doc_keys` table. `index_migration` on a 197 MB index read ~240 GB — rows ×
/// table size, quadratic.
///
/// The fixture is sized so the quadratic term is unmistakable: 5000 rows of
/// ≥ 1 KiB text. The assertion counts **page references** the pager served
/// (`index.narrow_backfill_pages_read`: cache hits + misses) rather than process
/// I/O bytes: a warm machine serves a fixture this size from the OS cache, so
/// `read_bytes` reports near zero even for the defect while the page traffic is
/// unchanged — and the miss counter alone is zero too, because a `doc_keys` table
/// that fits the page cache is re-read from memory on every row. At the defect's
/// 321 × the file, `≤ 4 ×` the file cannot pass.
#[test]
fn index_migration_read_volume_is_proportional_to_the_file() {
    const UNITS: usize = 5_000;
    const BODY: usize = 1_024;
    let env = support::TestEnv::new();
    let (root, _db) = opencode_bulk(&env, "oc-migration-cost", UNITS, BODY);
    let pair = m::Pair::new(&env, "oc-migration-cost");
    let (incremental, _rebuilt) = pair_configs(
        &env,
        "oc-migration-cost",
        &[source("oc", "opencode", &root)],
        &["oc"],
        &pair,
        true,
    );

    round(&incremental);
    downgrade_narrow_layout(&incremental, "oc");
    let index = index_path_of(&incremental, "oc");
    let file_bytes = std::fs::metadata(&index).expect("stat").len();
    assert!(
        file_bytes > 4 * 1024 * 1024,
        "the fixture must be large enough for the quadratic term to dominate: {file_bytes} B"
    );

    let reports = round(&incremental);
    let cost = cost_of(&reports, "oc");
    assert!(
        m::phase(&cost, "index_migration"),
        "the widening is its own stage: {cost}"
    );
    assert_eq!(
        m::counter(&cost, "index.narrow_backfill_rows"),
        UNITS as u64,
        "every unit is staged once: {cost}"
    );
    let pages = m::counter(&cost, "index.narrow_backfill_pages_read");
    let page_size = index_page_size(&incremental, "oc");
    let read_bytes = pages * page_size;
    eprintln!(
        "migration backfill: {UNITS} rows × {BODY} B → index {file_bytes} B ({page_size} B pages), \
         pages read {pages} = {read_bytes} B, ratio {:.2}×, phase {} ms, phase read_bytes {:?}",
        read_bytes as f64 / file_bytes as f64,
        cost["phases"]["index_migration"]["elapsed_ms"],
        cost["phases"]["index_migration"]["read_bytes"]
    );
    assert!(
        read_bytes <= 4 * file_bytes,
        "the backfill must read the index about once, not once per row: {read_bytes} B read \
         ({pages} pages) for a {file_bytes} B index"
    );
    // The same bound on the platform counter, when this machine issued real
    // reads at all (they are a subset of the pager's page loads).
    if let Some(bytes) = cost["phases"]["index_migration"]["read_bytes"].as_u64() {
        assert!(
            bytes <= 4 * file_bytes,
            "process reads of the migration stay linear: {bytes} B for a {file_bytes} B index"
        );
    }

    // The staged values are real, and the next round neither migrates nor
    // repairs.
    assert_eq!(narrow_ts(&incremental, "oc", "sm-0"), 1_000);
    assert_eq!(
        narrow_ts(&incremental, "oc", &format!("sm-{}", UNITS - 1)),
        1_000 + UNITS as i64 - 1
    );
    assert_eq!(narrow_title(&incremental, "oc", "sm-0"), "bulk session");
    let idle = round(&incremental);
    let idle_cost = cost_of(&idle, "oc");
    assert!(
        !m::phase(&idle_cost, "index_migration") && !m::phase(&idle_cost, "index_repair"),
        "the backfill runs exactly once: {idle_cost}"
    );
}

// ---------------------------------------------------------------------------
// Watermark edge cases found by the independent verification (verify-004a)
// ---------------------------------------------------------------------------

/// P3-1 / E7: a transient identity failure must not erase the recorded identity.
#[test]
fn jsonl_a_failed_identity_query_keeps_the_recorded_identity() {
    if !cfg!(windows) {
        // The identity is a Windows signal; off Windows it is always empty.
        return;
    }
    let env = support::TestEnv::new();
    let (root, path) = claude_fixture(&env);
    let pair = m::Pair::new(&env, "cc-identity-blip");
    let (incremental, rebuilt) = pair_configs(
        &env,
        "cc-identity-blip",
        &[source("cc", "claude_code", &root)],
        &["cc"],
        &pair,
        false,
    );
    let key = format!("proj/{SESSION}.jsonl");

    round(&incremental);
    let recorded = stored_watermark(&incremental, "cc")["files"][&key]["file_identity"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(!recorded.is_empty(), "the first round records an identity");

    // A single round in which the handle cannot be queried (transient failure,
    // network share, …) must not erase that fact.
    agent_memory::recall::fulltext::stamp::set_identity_unavailable(true);
    m::set_mtime(&path, m::mtime(&path) + std::time::Duration::from_secs(5));
    let reports = round(&incremental);
    agent_memory::recall::fulltext::stamp::set_identity_unavailable(false);

    let cost = cost_of(&reports, "cc");
    assert_eq!(
        m::counter(&cost, "index.mtime_only_refresh"),
        1,
        "the touch is still a refresh: {cost}"
    );
    assert_eq!(
        m::counter_sum(&cost, "index.full_rescan."),
        0,
        "and still not a rescan: {cost}"
    );
    let after = stored_watermark(&incremental, "cc")["files"][&key]["file_identity"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert_eq!(
        after, recorded,
        "the recorded identity survives a failed query"
    );

    m::index_rebuild(&rebuilt);
    m::assert_equivalent(&incremental, &rebuilt, "identity blip");
}

/// P3-2 / E7: an uncommitted tail that becomes a complete line without changing
/// the file length is read back — only the tail, never the file.
#[test]
fn jsonl_incomplete_tail_completion_is_equivalent() {
    let env = support::TestEnv::new();
    let root = env.root("claude-tail-completion");
    let path = root.join("proj").join(format!("{SESSION}.jsonl"));
    let long = format!("needle answer {}", "y".repeat(30_000));
    write_lines(
        &path,
        &[
            claude_line(SESSION, "u1", "2026-10-25T13:25:00Z", "user", "needle one"),
            claude_line(SESSION, "a1", "2026-10-25T13:26:00Z", "assistant", &long),
        ],
    );
    // A final record that is still being written: the bytes are there, the
    // terminating newline is not (a space stands in for it).
    let tail = claude_line(SESSION, "u2", "2026-10-25T13:27:00Z", "user", "needle tail");
    {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("open for append");
        file.write_all(tail.as_bytes()).expect("append tail");
        file.write_all(b" ").expect("stand-in for the newline");
    }

    let pair = m::Pair::new(&env, "cc-tail-completion");
    let (incremental, rebuilt) = pair_configs(
        &env,
        "cc-tail-completion",
        &[source("cc", "claude_code", &root)],
        &["cc"],
        &pair,
        false,
    );

    let first = round(&incremental);
    assert_eq!(
        m::counter_sum(&cost_of(&first, "cc"), "index.full_rescan."),
        1,
        "the first round reads the carrier once"
    );
    let size = std::fs::metadata(&path).expect("stat").len();
    let watermark = stored_watermark(&incremental, "cc");
    let key = format!("proj/{SESSION}.jsonl");
    let offset = watermark["files"][&key]["offset"].as_u64().expect("offset");
    assert!(
        offset < size,
        "the incomplete tail is not committed: offset {offset} / size {size}"
    );

    // Complete the tail in place: the stand-in byte becomes the newline, so the
    // file keeps its length while the last line becomes a record.
    let bytes = std::fs::read(&path).expect("read");
    let mut completed = bytes.clone();
    let last = completed.len() - 1;
    assert_eq!(completed[last], b' ');
    completed[last] = b'\n';
    std::fs::write(&path, &completed).expect("complete the tail");
    assert_eq!(std::fs::metadata(&path).expect("stat").len(), size);
    m::set_mtime(&path, m::mtime(&path) + std::time::Duration::from_secs(3));

    let reports = round(&incremental);
    let cost = cost_of(&reports, "cc");
    assert_eq!(
        m::counter(&cost, "index.tail_completion"),
        1,
        "the completed tail is read back: {cost}"
    );
    assert_eq!(
        m::counter_sum(&cost, "index.full_rescan."),
        0,
        "and the file is not re-read: {cost}"
    );
    let tail_bytes = (size - offset) as u64;
    assert_eq!(
        m::counter(&cost, "index.scan_bytes"),
        tail_bytes,
        "exactly the tail is read, not the file: {cost}"
    );
    assert!(
        tail_bytes * 20 < size,
        "the fixture makes the contrast visible: tail {tail_bytes} / file {size}"
    );

    m::index_rebuild(&rebuilt);
    m::assert_equivalent(&incremental, &rebuilt, "jsonl incomplete tail completion");
}

/// P3-6: the DSH "grew but lost logical lines" rebuild is counted.
#[test]
fn dsh_line_regression_is_a_counted_rescan() {
    let env = support::TestEnv::new();
    let root = env.root("dsh-line-regression");
    let path = dsh_log_path(&root);
    write_dsh(
        &path,
        &[dsh_frame(&[dsh_header(3), dsh_event(1, "needle dsh one")])],
    );
    let pair = m::Pair::new(&env, "dsh-line-regression");
    let (incremental, rebuilt) = pair_configs(
        &env,
        "dsh-line-regression",
        &[source("dsh", "dsh", &root)],
        &["dsh"],
        &pair,
        false,
    );
    round(&incremental);

    // A watermark that records more logical lines than the carrier holds (a
    // legacy entry, or one written by a build that counted differently). Only
    // the `lines` field matters: the absent probes keep the append branch.
    let rel_dir = format!("{PROJECT}/session-{DSH_SESSION}");
    let mut watermark = stored_watermark(&incremental, "dsh");
    let entry = watermark["files"][&rel_dir].clone();
    let lines = entry["lines"].as_u64().expect("lines");
    watermark["files"][&rel_dir] = json!({
        "file": entry["file"],
        "size": entry["size"],
        "mtime_ns": entry["mtime_ns"],
        "lines": lines + 100,
    });
    store_watermark(&incremental, "dsh", &watermark);

    {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("open dsh log");
        file.write_all(&dsh_frame(&[dsh_event(2, "needle dsh two")]))
            .expect("append frame");
    }
    let reports = round(&incremental);
    let cost = cost_of(&reports, "dsh");
    assert_eq!(
        m::counter(&cost, "index.full_rescan.dsh_line_regression"),
        1,
        "the forced rebuild is counted with a stable reason: {cost}"
    );
    assert_eq!(
        m::counter_sum(&cost, "index.full_rescan."),
        1,
        "and it is the only rescan: {cost}"
    );

    m::index_rebuild(&rebuilt);
    m::assert_equivalent(&incremental, &rebuilt, "dsh line regression");
}
// ---------------------------------------------------------------------------
// A downgraded writer (verify-004a-b P1) and the label round (P3-1)
// ---------------------------------------------------------------------------

/// One `meta` value, read straight from the index (the fixture manipulates the
/// file to imitate another binary).
fn stored_meta_value(config: &RecallConfig, source_id: &str, key: &str) -> Option<String> {
    let conn = rusqlite::Connection::open(index_path_of(config, source_id)).expect("index");
    conn.query_row("SELECT value FROM meta WHERE key = ?1", [key], |row| {
        row.get::<_, String>(0)
    })
    .ok()
}

fn narrow_ts(config: &RecallConfig, source_id: &str, message_id: &str) -> i64 {
    let conn = rusqlite::Connection::open(index_path_of(config, source_id)).expect("index");
    conn.query_row(
        "SELECT ts FROM doc_keys WHERE message_id = ?1",
        rusqlite::params![message_id],
        |row| row.get::<_, i64>(0),
    )
    .expect("narrow ts")
}

/// The narrow title stored for the units of one carrier. DSH units have no
/// `message_id` (their locator is a record index), so the row is found through
/// `doc_keys.rel_path`.
fn narrow_title_of_rel_path(config: &RecallConfig, source_id: &str, rel_path: &str) -> String {
    let conn = rusqlite::Connection::open(index_path_of(config, source_id)).expect("index");
    conn.query_row(
        "SELECT c.title FROM conversation_units AS c
           JOIN doc_keys AS d ON d.doc_key = c.doc_key
          WHERE d.rel_path = ?1 LIMIT 1",
        rusqlite::params![rel_path],
        |row| row.get::<_, String>(0),
    )
    .expect("narrow title")
}

fn narrow_title(config: &RecallConfig, source_id: &str, message_id: &str) -> String {
    let conn = rusqlite::Connection::open(index_path_of(config, source_id)).expect("index");
    conn.query_row(
        "SELECT title FROM conversation_units WHERE message_id = ?1",
        rusqlite::params![message_id],
        |row| row.get::<_, String>(0),
    )
    .expect("narrow title")
}

/// P1 (verify-004a-b) + P2 (verify-004a-c): a writer that does not know the
/// narrow columns leaves two kinds of damage behind, and the next build must
/// restore both with one linear pass.
///
/// * **Insert damage**: its `INSERT`s write the column defaults (`ts = 0`,
///   `title = ''`).
/// * **Rename damage**: its in-place rename (`UPDATE units SET title = …
///   WHERE session_id = ?`) leaves the *old*, non-default title in
///   `conversation_units.title`, and a `doc_keys.digest` that still describes the
///   old label. A default-value candidate can never find that row, so the repair
///   restages the whole narrow state from `units` (`index_repair`, one pass) and
///   hands the renamed sessions to the builder for re-projection.
///
/// The downgraded write is imitated with SQL, in the exact state an old binary
/// leaves: the row is projected normally (so the index content equals what the
/// source produces), then the narrow columns are blanked — or, for a rename, the
/// source label and `units.title` move while the narrow copies and the digest
/// stay behind — and the fingerprint is either rolled back to the previous shape
/// or removed.
#[test]
fn index_repairs_narrow_columns_after_a_downgraded_writer() {
    let env = support::TestEnv::new();
    let (root, db) = opencode_bulk(&env, "oc-downgrade", 5_000, 1_024);
    let pair = m::Pair::new(&env, "oc-downgrade");
    let (incremental, rebuilt) = pair_configs(
        &env,
        "oc-downgrade",
        &[source("oc", "opencode", &root)],
        &["oc"],
        &pair,
        true,
    );

    round(&incremental);
    let before = m::meta_counts(&incremental, "oc").expect("meta");
    let old_fingerprint = stored_meta_value(&incremental, "oc", "narrow_fingerprint")
        .expect("a build records the narrow fingerprint");
    // The reference this repair is measured against: a round with nothing to do
    // (the fixture's host database dominates both numbers).
    let idle = round(&incremental);
    let idle_cost = cost_of(&idle, "oc");
    eprintln!(
        "baseline no-change round: read_bytes={:?} narrow_repair_rows={}",
        idle_cost["read_bytes"],
        m::counter(&idle_cost, "index.narrow_repair_rows")
    );
    assert_eq!(
        m::counter(&idle_cost, "index.narrow_repair_rows"),
        0,
        "a matching fingerprint repairs nothing: {idle_cost}"
    );

    // Two shapes of the same event: the fingerprint survived but no longer
    // describes the table (a writer that kept it), and the fingerprint is gone
    // (what an older binary's `DELETE FROM meta` leaves).
    for (case, keep_fingerprint) in [("shape mismatch", true), ("missing record", false)] {
        let message_id = format!("sm-downgraded-{case}");
        {
            let conn = rusqlite::Connection::open(&db).expect("db");
            conn.execute(
                "INSERT INTO session_message (id, session_id, seq, type, time_created, data) \
                 VALUES (?1, 's-1', 900, 'user', ?2, ?3)",
                rusqlite::params![
                    message_id,
                    9_000_000i64,
                    json!({"content": [{"type": "text", "text": format!("needle downgraded {case}")}]})
                        .to_string()
                ],
            )
            .expect("source row");
        }
        round(&incremental);

        {
            let conn =
                rusqlite::Connection::open(index_path_of(&incremental, "oc")).expect("index");
            conn.execute(
                "UPDATE doc_keys SET ts = 0 WHERE message_id = ?1",
                rusqlite::params![message_id],
            )
            .expect("blank the narrow ts");
            conn.execute(
                "UPDATE conversation_units SET title = '' WHERE message_id = ?1",
                rusqlite::params![message_id],
            )
            .expect("blank the narrow title");
            if keep_fingerprint {
                conn.execute(
                    "UPDATE meta SET value = ?2 WHERE key = 'narrow_fingerprint'",
                    rusqlite::params!["narrow_fingerprint", old_fingerprint.clone()],
                )
                .expect("restore the older shape");
            } else {
                conn.execute("DELETE FROM meta WHERE key = 'narrow_fingerprint'", [])
                    .expect("drop the record");
            }
        }
        assert_eq!(narrow_ts(&incremental, "oc", &message_id), 0, "{case}");
        assert_eq!(narrow_title(&incremental, "oc", &message_id), "", "{case}");

        let reports = round(&incremental);
        let cost = cost_of(&reports, "oc");
        let file_bytes = index_file_bytes(&incremental, "oc");
        let page_size = index_page_size(&incremental, "oc");
        let repair_pages = m::counter(&cost, "index.narrow_repair_pages_read");
        eprintln!(
            "repair round ({case}): read_bytes={:?} narrow_repair_rows={} \
             narrow_repair_pages_read={} index={} B, phase {} ms, units_in_index={}",
            cost["read_bytes"],
            m::counter(&cost, "index.narrow_repair_rows"),
            repair_pages,
            file_bytes,
            cost["phases"]["index_repair"]["elapsed_ms"],
            m::meta_counts(&incremental, "oc")
                .map(|(rows, _)| rows)
                .unwrap_or(0)
        );
        assert!(
            m::phase(&cost, "index_repair"),
            "{case}: the repair is its own stage: {cost}"
        );
        assert_eq!(
            m::counter(&cost, "index.narrow_repair_rows"),
            m::meta_counts(&incremental, "oc").expect("meta").0,
            "{case}: the repair restages every narrow row (the damage is not \
             confined to the defaults): {cost}"
        );
        // The repair is one linear pass over the index, the same measure as the
        // migration: it cannot depend on how many rows the other writer touched.
        assert!(
            // Linear in the file, the same measure as the migration: the repair
            // cannot depend on how many rows the other writer touched. The constant
            // is larger than the migration's because the repair adds the relabel
            // detection pass and every statement probes the staged rows once per
            // row (`SCAN <narrow> / SEARCH narrow_fill`): measured 4.7x the file at
            // this size, 4.5x at 15000 units, 1.0x at 63.
            repair_pages * page_size <= 8 * file_bytes,
            "{case}: the repair reads the index about once: {repair_pages} pages for a \
             {file_bytes} B index: {cost}"
        );
        assert_eq!(
            m::counter_sum(&cost, "index.full_rescan."),
            0,
            "{case}: a repair is not a rescan: {cost}"
        );
        assert_eq!(
            m::counter(&cost, "vectors.sessions_recomputed"),
            0,
            "{case}: the repair runs before the vector stage, so no session is recomputed: {cost}"
        );

        // The values are back, and the reported newest time includes the new row.
        let ts = narrow_ts(&incremental, "oc", &message_id);
        assert_eq!(ts, 9_000_000, "{case}: the narrow ts was repaired");
        assert_eq!(
            narrow_title(&incremental, "oc", &message_id),
            "bulk session",
            "{case}: the narrow title was repaired"
        );
        let after = m::meta_counts(&incremental, "oc").expect("meta");
        assert!(
            after.1 > before.1,
            "{case}: newest_unit_ts must see the new row and never regress: {after:?} vs {before:?}"
        );
    }

    // Third shape (verify-004a-c P2): the older writer renamed a session **in
    // place** — `UPDATE units SET title = …`, narrow tables untouched, digest
    // untouched. Every row keeps a *non-default* narrow title, so the old
    // default-value candidate could never find it.
    let renamed = "renamed during downgrade";
    let relabel_message = "sm-downgraded-relabel";
    {
        let conn = rusqlite::Connection::open(&db).expect("db");
        conn.execute(
            "INSERT INTO session_message (id, session_id, seq, type, time_created, data) \
             VALUES (?1, 's-1', 901, 'user', ?2, ?3)",
            rusqlite::params![
                relabel_message,
                9_100_000i64,
                json!({"content": [{"type": "text", "text": "needle downgraded relabel"}]})
                    .to_string()
            ],
        )
        .expect("source row");
    }
    round(&incremental);
    {
        // What the old binary's round leaves when the source title changed:
        // `units` and `sessions` carry the new label (its `refresh_sessions`), the
        // narrow `title` copy and every digest still carry the old one, and its
        // `meta` write dropped the fingerprint.
        let conn = rusqlite::Connection::open(&db).expect("db");
        conn.execute(
            "UPDATE session_v2 SET title = ?1 WHERE id = 's-1'",
            rusqlite::params![renamed],
        )
        .expect("rename in the source");
        let conn = rusqlite::Connection::open(index_path_of(&incremental, "oc")).expect("index");
        conn.execute(
            "UPDATE units SET title = ?1 WHERE session_id = 's-1'",
            rusqlite::params![renamed],
        )
        .expect("relabel units in place");
        conn.execute(
            "UPDATE sessions SET title = ?1 WHERE session_id = 's-1'",
            rusqlite::params![renamed],
        )
        .expect("relabel the session row in place");
        conn.execute("DELETE FROM meta WHERE key = 'narrow_fingerprint'", [])
            .expect("drop the record");
    }
    assert_eq!(
        narrow_title(&incremental, "oc", relabel_message),
        "bulk session",
        "the narrow title still holds the old label"
    );

    let reports = round(&incremental);
    let cost = cost_of(&reports, "oc");
    let file_bytes = index_file_bytes(&incremental, "oc");
    let page_size = index_page_size(&incremental, "oc");
    let repair_pages = m::counter(&cost, "index.narrow_repair_pages_read");
    eprintln!(
        "repair round (in-place relabel): narrow_repair_rows={} narrow_relabel_sessions={} \
         session_reprojections={} pages={} index={} B, phase {} ms",
        m::counter(&cost, "index.narrow_repair_rows"),
        m::counter(&cost, "index.narrow_relabel_sessions"),
        m::counter(&cost, "index.session_reprojections"),
        repair_pages,
        file_bytes,
        cost["phases"]["index_repair"]["elapsed_ms"]
    );
    assert!(
        repair_pages * page_size <= 8 * file_bytes,
        "relabel: the repair reads the index about once: {repair_pages} pages for a \
         {file_bytes} B index: {cost}"
    );
    assert_eq!(
        m::counter(&cost, "index.narrow_relabel_sessions"),
        1,
        "the repair spots the session whose narrow title disagrees with units: {cost}"
    );
    assert_eq!(
        m::counter(&cost, "index.session_reprojections"),
        1,
        "and the builder re-projects it, so the digests describe the new label: {cost}"
    );
    assert_eq!(
        m::counter_sum(&cost, "index.full_rescan."),
        0,
        "a relabel repair is not a carrier rescan: {cost}"
    );
    assert_eq!(
        narrow_title(&incremental, "oc", relabel_message),
        renamed,
        "the narrow title follows units"
    );
    assert_eq!(
        narrow_ts(&incremental, "oc", relabel_message),
        9_100_000,
        "and so does the narrow ts"
    );

    m::index_rebuild(&rebuilt);
    m::assert_equivalent(
        &incremental,
        &rebuilt,
        "narrow column repair after a downgraded writer",
    );
}

/// P3-1: a round that only renames a session must leave a digest that matches a
/// rebuild (the title is part of every unit's content digest).
#[test]
fn opencode_rename_only_is_equivalent() {
    let env = support::TestEnv::new();
    let (root, db) = opencode_two_sessions(&env, "oc-rename-only");
    let (incremental, rebuilt, _pair) = opencode_configs(&env, "oc-rename-only", &root, false);

    round(&incremental);
    {
        let conn = rusqlite::Connection::open(&db).expect("db");
        // Only the label moves: no `session_message` row changes.
        conn.execute(
            "UPDATE session_v2 SET title = 'renamed only' WHERE id = 's-1'",
            [],
        )
        .expect("rename");
    }
    let reports = round(&incremental);
    let cost = cost_of(&reports, "oc");
    assert_eq!(
        m::counter(&cost, "index.session_reprojections"),
        1,
        "the renamed session is re-projected, not relabelled: {cost}"
    );
    assert_eq!(
        m::counter(&cost, "index.session_reprojection_rows"),
        2,
        "only that session's rows are read: {cost}"
    );
    assert_eq!(
        m::counter_sum(&cost, "index.full_rescan."),
        0,
        "a rename is not a carrier rescan: {cost}"
    );

    m::index_rebuild(&rebuilt);
    m::assert_equivalent(&incremental, &rebuilt, "opencode rename only");
}

/// P3-1 for DSH: the title arrives after the messages (`session/title`), so a
/// rename has to re-project the session for the digests to match a rebuild.
#[test]
fn dsh_title_change_reprojects_the_session() {
    let env = support::TestEnv::new();
    let root = env.root("dsh-title-change");
    let path = dsh_log_path(&root);
    write_dsh(
        &path,
        &[dsh_frame(&[dsh_header(3), dsh_event(1, "needle dsh one")])],
    );
    let pair = m::Pair::new(&env, "dsh-title-change");
    let (incremental, rebuilt) = pair_configs(
        &env,
        "dsh-title-change",
        &[source("dsh", "dsh", &root)],
        &["dsh"],
        &pair,
        false,
    );
    round(&incremental);

    {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("open dsh log");
        file.write_all(&dsh_frame(&[
            dsh_title_event(2, "renamed dsh session"),
            dsh_event(3, "needle dsh two"),
        ]))
        .expect("append a title change");
    }
    let reports = round(&incremental);
    let cost = cost_of(&reports, "dsh");
    assert_eq!(
        m::counter(&cost, "index.full_rescan.dsh_title_changed"),
        1,
        "the rename re-projects the session and is counted: {cost}"
    );
    assert_eq!(
        m::counter_sum(&cost, "index.full_rescan."),
        1,
        "it is the only rescan of that round: {cost}"
    );

    m::index_rebuild(&rebuilt);
    m::assert_equivalent(&incremental, &rebuilt, "dsh title change");
}

/// P2 for the DSH source: the same in-place rename an older writer leaves —
/// `units.title` moved, while the narrow title copy and every digest still
/// describe the old label and the carrier has not changed since that round.
/// Nothing in the change table can see it, so the repair's recorded session is
/// the only trigger that re-projects the carrier
/// (`index.full_rescan.narrow_relabel`).
#[test]
fn dsh_repairs_a_title_an_older_writer_changed_in_place() {
    let env = support::TestEnv::new();
    let root = env.root("dsh-relabel");
    let path = dsh_log_path(&root);
    write_dsh(
        &path,
        &[dsh_frame(&[dsh_header(3), dsh_event(1, "needle dsh one")])],
    );
    let pair = m::Pair::new(&env, "dsh-relabel");
    let (incremental, rebuilt) = pair_configs(
        &env,
        "dsh-relabel",
        &[source("dsh", "dsh", &root)],
        &["dsh"],
        &pair,
        false,
    );
    round(&incremental);
    let rel_dir = format!("{PROJECT}/session-{DSH_SESSION}");
    let rel_path = format!("{PROJECT}/session-{DSH_SESSION}/session.v3.jsonl.zstd");
    // No `session/title` yet: the units carry the session id as their label.
    assert_eq!(narrow_title_of_rel_path(&incremental, "dsh", &rel_path), DSH_SESSION);
    // The digests of the pre-rename content, to be restored as the stale state.
    let old_digests: Vec<(String, String)> = {
        let conn = rusqlite::Connection::open(index_path_of(&incremental, "dsh"))
            .expect("index");
        let mut statement = conn
            .prepare("SELECT doc_key, digest FROM doc_keys")
            .expect("digests");
        let rows = statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .expect("digests");
        rows.collect::<rusqlite::Result<Vec<_>>>().expect("digests")
    };

    // The title arrives in the carrier; this build re-projects the session, which
    // is the state a *current* binary leaves.
    {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("open dsh log");
        file.write_all(&dsh_frame(&[
            dsh_title_event(2, "renamed in place"),
            dsh_event(3, "needle dsh two"),
        ]))
        .expect("append a title change");
    }
    let reports = round(&incremental);
    assert_eq!(
        m::counter(&cost_of(&reports, "dsh"), "index.full_rescan.dsh_title_changed"),
        1,
        "the rename re-projects the carrier"
    );
    assert_eq!(
        narrow_title_of_rel_path(&incremental, "dsh", &rel_path),
        "renamed in place"
    );

    // What the older writer leaves behind: its `set_session_title` moved
    // `units.title` only, so the narrow copy and every digest still describe the
    // old label, and its `meta` write dropped the fingerprint.
    {
        let conn =
            rusqlite::Connection::open(index_path_of(&incremental, "dsh")).expect("index");
        conn.execute(
            "UPDATE conversation_units SET title = ?1",
            rusqlite::params![DSH_SESSION],
        )
        .expect("leave the old narrow title");
        {
            let mut update = conn
                .prepare("UPDATE doc_keys SET digest = ?2 WHERE doc_key = ?1")
                .expect("digest update");
            for (doc_key, digest) in &old_digests {
                update
                    .execute(rusqlite::params![doc_key, digest])
                    .expect("restore the older digest");
            }
        }
        conn.execute("DELETE FROM meta WHERE key = 'narrow_fingerprint'", [])
            .expect("drop the record");
    }
    assert_eq!(
        narrow_title_of_rel_path(&incremental, "dsh", &rel_path),
        DSH_SESSION,
        "the narrow title still holds the old label"
    );
    // The carrier has not changed since the last round (its watermark entry
    // describes it), so the narrow title disagreeing with `units` is the only
    // signal left.
    assert!(stored_watermark(&incremental, "dsh")["files"][&rel_dir]["size"].is_number());

    let reports = round(&incremental);
    let cost = cost_of(&reports, "dsh");
    eprintln!(
        "dsh relabel repair: narrow_relabel_sessions={} full_rescan={}",
        m::counter(&cost, "index.narrow_relabel_sessions"),
        m::counter_sum(&cost, "index.full_rescan.")
    );
    assert_eq!(
        m::counter(&cost, "index.narrow_relabel_sessions"),
        1,
        "the repair spots the carrier whose narrow title disagrees with units: {cost}"
    );
    assert_eq!(
        m::counter(&cost, "index.full_rescan.narrow_relabel"),
        1,
        "and re-projects that carrier for the recorded session: {cost}"
    );
    assert_eq!(
        m::counter_sum(&cost, "index.full_rescan."),
        1,
        "it is the only rescan of the round: {cost}"
    );
    assert_eq!(
        narrow_title_of_rel_path(&incremental, "dsh", &rel_path),
        "renamed in place",
        "the narrow title follows units"
    );

    m::index_rebuild(&rebuilt);
    m::assert_equivalent(&incremental, &rebuilt, "dsh in-place relabel repair");

    // The record is consumed: the next round does not rescan again.
    let idle = round(&incremental);
    assert_eq!(
        m::counter_sum(&cost_of(&idle, "dsh"), "index.full_rescan."),
        0,
        "the forced re-projection happens once"
    );
}

/// P3-2: a cache file that cannot be read is `unreadable`, not `corrupt`.
#[test]
#[cfg(windows)]
fn vector_digest_distinguishes_unreadable_from_corrupt() {
    use agent_memory::recall::fulltext::vectors::digest_file;
    use std::os::windows::fs::OpenOptionsExt;

    let env = support::TestEnv::new();
    let path = env.write("locked-vectors.bin", b"AMLV-not-a-real-cache");

    // Readable bytes that are not this crate's cache: a parsing failure.
    assert_eq!(digest_file(&path).status, "corrupt");
    assert_eq!(
        digest_file(&env.path().join("absent-vectors.bin")).status,
        "missing"
    );

    // An exclusive handle (no share mode) makes the read fail twice.
    let guard = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .share_mode(0)
        .open(&path)
        .expect("exclusive handle");
    assert_eq!(digest_file(&path).status, "unreadable");
    drop(guard);
    assert_eq!(digest_file(&path).status, "corrupt");
}
// ---------------------------------------------------------------------------
// `index-digest` CLI surface (read-only)
// ---------------------------------------------------------------------------

#[test]
fn index_digest_cli_is_read_only_and_reports_the_same_digest() {
    let env = support::TestEnv::new();
    let (root, _path) = claude_fixture(&env);
    let pair = m::Pair::new(&env, "cc-digest-cli");
    let (incremental, rebuilt) = pair_configs(
        &env,
        "cc-digest-cli",
        &[source("cc", "claude_code", &root)],
        &["cc"],
        &pair,
        false,
    );
    round(&incremental);
    m::index_rebuild(&rebuilt);
    let config_path = env
        .path()
        .join("cc-digest-cli-incremental.json")
        .to_string_lossy()
        .to_string();

    let before = support::tree_state(&pair.incremental_root);
    let run = m::run(&["index-digest", "--config", &config_path, "--json"]);
    assert_eq!(run.code, 0, "{} {}", run.stdout, run.stderr);
    assert_eq!(run.report["sources"][0]["status"], json!("ready"), "{}", run.stdout);
    assert!(
        run.report["exemptions"]
            .as_array()
            .is_some_and(|list| !list.is_empty()),
        "the exemption list is part of the output"
    );
    assert_eq!(
        support::tree_state(&pair.incremental_root),
        before,
        "index-digest writes nothing"
    );

    let text = m::run_raw(&["index-digest", "--config", &config_path]);
    assert_eq!(text.code, 0);
    assert!(text.stdout.contains("units_digest="), "{}", text.stdout);

    // The same command against the rebuild cache yields the same digest.
    let other = env
        .path()
        .join("cc-digest-cli-rebuild.json")
        .to_string_lossy()
        .to_string();
    let other_run = m::run(&["index-digest", "--config", &other, "--json"]);
    assert_eq!(
        other_run.report["sources"][0]["units"]["digest"],
        run.report["sources"][0]["units"]["digest"]
    );
}
