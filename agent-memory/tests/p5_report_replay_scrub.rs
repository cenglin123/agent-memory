//! P5 acceptance tests: coverage report, baseline reconciliation, derived
//! replay checkpoints and periodic scrub.
//!
//! Every source, database and attachment in this file is synthetic and lives
//! under a `tempfile` root. No test points at a real session directory.

mod support;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use agent_memory::archive::{hex_sha256, Archive};
use agent_memory::attachment::{capture_attachment, AttachmentRequest};
use agent_memory::capture::capture_source;
use agent_memory::model::ContentRef;
use agent_memory::replay::{self, ReplayIdentity};
use agent_memory::report::{
    BaselineSpec, DeclaredSource, ReportConfig, CATEGORY_ATTACHMENT_MISSING, CATEGORY_CORRUPTION,
    CATEGORY_DERIVED_LAG, CATEGORY_DISABLED, CATEGORY_PARSE_FAILED, CATEGORY_READ_FAILED,
    CATEGORY_UNSUPPORTED,
};
use agent_memory::scrub;
use serde_json::{json, Value};

const DOMAIN: &str = "default";

fn make_root() -> (tempfile::TempDir, Archive) {
    let temp = tempfile::tempdir().unwrap();
    let archive = Archive::init(temp.path()).unwrap();
    (temp, archive)
}

fn cli() -> Command {
    Command::new(env!("CARGO_BIN_EXE_agent-memory"))
}

fn write(path: &Path, bytes: &[u8]) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, bytes).unwrap();
}

fn codex_line(_id: &str, role: &str, text: &str, second: u32) -> String {
    serde_json::json!({
        "timestamp": format!("2026-10-26T10:00:{second:02}Z"),
        "type": "response_item",
        "payload": {
            "type": "message",
            "role": role,
            "content": [{"type": if role == "user" {"input_text"} else {"output_text"}, "text": text}]
        }
    })
    .to_string()
}

fn codex_meta(id: &str) -> String {
    serde_json::json!({
        "timestamp": "2026-10-26T10:00:00Z",
        "type": "session_meta",
        "payload": {"id": id, "cwd": "/tmp/synthetic"}
    })
    .to_string()
}

fn write_codex(path: &Path, id: &str, messages: &[(&str, &str)]) {
    let mut lines = vec![codex_meta(id)];
    for (index, (role, text)) in messages.iter().enumerate() {
        lines.push(codex_line(id, role, text, (index + 1) as u32));
    }
    write(path, (lines.join("\n") + "\n").as_bytes());
}

fn append_codex(path: &Path, id: &str, role: &str, text: &str, second: u32) {
    let mut body = fs::read_to_string(path).unwrap();
    body.push_str(&codex_line(id, role, text, second));
    body.push('\n');
    write(path, body.as_bytes());
}

fn capture(archive: &Archive, path: &Path) -> agent_memory::capture::CaptureReport {
    capture_source(archive, path, "codex", DOMAIN, None).unwrap()
}

fn declared(
    id: &str,
    provider: &str,
    root: &Path,
    enabled: bool,
    baseline: Option<BaselineSpec>,
) -> DeclaredSource {
    DeclaredSource {
        id: id.to_string(),
        provider: provider.to_string(),
        root: Some(root.to_string_lossy().to_string()),
        enabled,
        format_generation: None,
        access: vec![DOMAIN.to_string()],
        baseline,
    }
}

fn report_config(sources: Vec<DeclaredSource>) -> ReportConfig {
    ReportConfig {
        version: 1,
        sources,
    }
}

fn row_for<'a>(
    report: &'a agent_memory::coverage::CoverageReport,
    source: &str,
) -> &'a agent_memory::coverage::SourceCoverage {
    report
        .sources
        .iter()
        .find(|row| row.source == source)
        .unwrap_or_else(|| panic!("no row for {source}: {:?}", report.sources))
}

fn has_category(row: &agent_memory::coverage::SourceCoverage, category: &str) -> bool {
    row.categories.iter().any(|value| value == category)
}

fn tree_state(root: &Path) -> Vec<(String, u64, u128)> {
    fn walk(root: &Path, current: &Path, out: &mut Vec<(String, u64, u128)>) {
        let Ok(entries) = fs::read_dir(current) else {
            return;
        };
        for entry in entries.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
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
            out.push((
                path.strip_prefix(root)
                    .map(|rel| rel.to_string_lossy().replace('\\', "/"))
                    .unwrap_or_default(),
                metadata.len(),
                mtime,
            ));
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out.sort();
    out
}

fn artifact_path(root: &Path, generation: u64, source: &str) -> PathBuf {
    root.join("derived")
        .join("generations")
        .join(format!("g{generation}"))
        .join("sources")
        .join(format!("{}.json", replay::source_key(source)))
}

fn state_path(root: &Path) -> PathBuf {
    root.join("derived").join("state.json")
}

fn only_source(archive: &Archive) -> String {
    archive.manifests().unwrap()[0].source.path.clone()
}

fn read_state(root: &Path) -> Value {
    serde_json::from_slice(&fs::read(state_path(root)).unwrap()).unwrap()
}

fn write_state(root: &Path, value: &Value) {
    write(
        &state_path(root),
        serde_json::to_string_pretty(value).unwrap().as_bytes(),
    );
}

// ---------------------------------------------------------------------------
// 1. Baseline reconciliation: Codex session_index + directory enumeration
// ---------------------------------------------------------------------------

#[test]
fn codex_index_and_directory_baselines_report_mismatch_explicitly() {
    let (temp, archive) = make_root();
    let dir = temp.path().join("codex");
    fs::create_dir_all(&dir).unwrap();
    let first = dir.join("rollout-11111111-1111-4111-8111-111111111111.jsonl");
    let second = dir.join("rollout-22222222-2222-4222-8222-222222222222.jsonl");
    write_codex(&first, "11111111-1111-4111-8111-111111111111", &[("user", "hello")]);
    write_codex(&second, "22222222-2222-4222-8222-222222222222", &[("user", "world")]);
    assert_eq!(capture(&archive, &first).status, "captured");
    assert_eq!(capture(&archive, &second).status, "captured");

    let index = temp.path().join("session_index.jsonl");
    let extra = "33333333-3333-4333-8333-333333333333";
    write(
        &index,
        format!(
            "{}\n{}\n{}\n",
            json!({"id": "11111111-1111-4111-8111-111111111111", "thread_name": "one"}),
            json!({"id": "22222222-2222-4222-8222-222222222222", "thread_name": "two"}),
            json!({"id": extra, "thread_name": "not archived"})
        )
        .as_bytes(),
    );

    let config = report_config(vec![declared(
        "codex-main",
        "codex",
        &dir,
        true,
        Some(BaselineSpec {
            kind: "codex_session_index".to_string(),
            path: Some(index.to_string_lossy().to_string()),
            pattern: None,
            recursive: true,
        }),
    )]);
    let report = archive.coverage_with(&config).unwrap();
    assert!(!report.ok, "a baseline mismatch is an anomaly");
    assert_eq!(report.reconciliation.baselines.len(), 1);
    let baseline = &report.reconciliation.baselines[0];
    assert_eq!(baseline.status, "mismatch");
    assert_eq!(baseline.baseline_count, 3);
    assert_eq!(baseline.archive_count, 2);
    assert!(baseline.missing_from_archive.iter().any(|id| id == extra));
    assert!(report
        .anomalies
        .iter()
        .any(|line| line.contains("session_index lists")));
    assert_eq!(report.categories.covered, 2);

    // Directory enumeration sees a third rollout that was never captured.
    let third = dir.join("rollout-44444444-4444-4444-8444-444444444444.jsonl");
    write_codex(
        &third,
        "44444444-4444-4444-8444-444444444444",
        &[("user", "uncovered")],
    );
    let mut directory_decl = declared("codex-dir", "codex", &dir, true, Some(BaselineSpec {
        kind: "directory_enumeration".to_string(),
        path: Some(dir.to_string_lossy().to_string()),
        pattern: Some("rollout-*.jsonl".to_string()),
        recursive: true,
    }));
    directory_decl.format_generation = Some("rollout-jsonl".to_string());
    let config = report_config(vec![directory_decl]);
    let report = archive.coverage_with(&config).unwrap();
    let baseline = &report.reconciliation.baselines[0];
    assert_eq!(baseline.status, "mismatch");
    assert_eq!(baseline.baseline_count, 3);
    assert_eq!(baseline.archive_count, 2);
    assert_eq!(baseline.missing_from_archive.len(), 1);
    assert!(!report.reconciliation.ok);

    // CLI: the report is still on stdout, the exit code is the anomaly signal.
    let config_path = temp.path().join("config.local.json");
    write(
        &config_path,
        serde_json::to_string_pretty(&config).unwrap().as_bytes(),
    );
    let output = cli()
        .arg("coverage")
        .arg("--root")
        .arg(temp.path())
        .arg("--config")
        .arg(&config_path)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(8));
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["ok"], false);
    assert_eq!(value["reconciliation"]["baselines"][0]["status"], "mismatch");
}

// ---------------------------------------------------------------------------
// 2. OpenCode session-table baseline against the archived observation
// ---------------------------------------------------------------------------

const V2_SCHEMA: &str = "
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

fn build_opencode_db(path: &Path) {
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.execute_batch(V2_SCHEMA).unwrap();
    conn.execute(
        "INSERT INTO session_v2 (id, title, directory, time_created, time_updated, parent_id) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params!["ses_one", "one", "C:/work", 1000i64, 2000i64, Option::<String>::None],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO session_message (id, session_id, seq, type, time_created, data) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![
            "sm_one",
            "ses_one",
            1i64,
            "assistant",
            1100i64,
            json!({"content": [{"type": "text", "text": "opencode body"}]}).to_string()
        ],
    )
    .unwrap();
}

#[test]
fn opencode_table_baseline_is_compared_with_the_archived_observation() {
    let (temp, archive) = make_root();
    let db = temp.path().join("opencode.db");
    build_opencode_db(&db);
    let captured = capture_source(&archive, &db, "opencode", DOMAIN, None).unwrap();
    assert_eq!(captured.status, "captured", "{}", captured.error.unwrap_or_default());

    let spec = BaselineSpec {
        kind: "opencode_session_table".to_string(),
        path: Some(db.to_string_lossy().to_string()),
        pattern: None,
        recursive: true,
    };
    let config = report_config(vec![declared("oc", "opencode", &db, true, Some(spec.clone()))]);
    let report = archive.coverage_with(&config).unwrap();
    let baseline = &report.reconciliation.baselines[0];
    assert_eq!(baseline.status, "ok", "{:?}", baseline.anomalies);
    assert_eq!(baseline.baseline_count, 1);
    assert_eq!(baseline.archive_count, 1);

    // A session created after the observation is a baseline mismatch, not a
    // silent success.
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute(
        "INSERT INTO session_v2 (id, title, directory, time_created, time_updated, parent_id) VALUES ('ses_two', 'two', 'C:/work', 3000, 4000, NULL)",
        [],
    )
    .unwrap();
    drop(conn);
    let report = archive.coverage_with(&config).unwrap();
    let baseline = &report.reconciliation.baselines[0];
    assert_eq!(baseline.status, "mismatch");
    assert_eq!(baseline.baseline_count, 2);
    assert_eq!(baseline.archive_count, 1);
    assert!(baseline
        .missing_from_archive
        .iter()
        .any(|id| id == "ses_two"));
    assert!(!report.ok);
}

// ---------------------------------------------------------------------------
// 3. Explicit source categories: unsupported / disabled / read_failed /
//    attachment_missing / parse_failed
// ---------------------------------------------------------------------------

#[test]
fn unsupported_and_disabled_declarations_are_explicit() {
    let (temp, archive) = make_root();
    let dir = temp.path().join("codex");
    fs::create_dir_all(&dir).unwrap();
    let source = dir.join("rollout-aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa.jsonl");
    write_codex(&source, "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa", &[("user", "hi")]);
    capture(&archive, &source);

    let mut unknown = declared("unknown-gen", "codex", &dir, true, None);
    unknown.format_generation = Some("rollout-jsonl@v99".to_string());
    let report = archive.coverage_with(&report_config(vec![unknown])).unwrap();
    let row = row_for(&report, &report.sources[0].source);
    assert!(has_category(row, CATEGORY_UNSUPPORTED));
    assert_eq!(row.category, CATEGORY_UNSUPPORTED);
    assert_eq!(report.categories.unsupported, 1);
    assert!(!report.ok);
    assert!(report
        .anomalies
        .iter()
        .any(|line| line.contains("unknown format generation")));

    // A provider outside the first-batch matrix is unsupported too (explicit),
    // even without a declared format generation.
    let unknown_provider = declared("pi", "pi", &dir, true, None);
    let report = archive.coverage_with(&report_config(vec![unknown_provider])).unwrap();
    assert!(!report.ok);
    assert_eq!(report.categories.unsupported, 1);
    assert!(report
        .anomalies
        .iter()
        .any(|line| line.contains("is not supported by this crate")));

    // Disabled: explicitly visible, not an anomaly, evidence still counted.
    let disabled = declared("disabled", "codex", &dir, false, None);
    let report = archive.coverage_with(&report_config(vec![disabled])).unwrap();
    let row = row_for(&report, &report.sources[0].source);
    assert!(has_category(row, CATEGORY_DISABLED));
    assert_eq!(row.status, "disabled");
    assert!(row.configured);
    assert_eq!(row.records_committed, 2);
    assert_eq!(report.categories.disabled, 1);
    assert!(report.ok, "a disabled source is a state, not a failure");

    // Enabled but never captured: explicit `not_captured`, not `covered`.
    let empty = temp.path().join("empty");
    fs::create_dir_all(&empty).unwrap();
    let never = declared("never", "codex", &empty, true, None);
    let report = archive.coverage_with(&report_config(vec![never])).unwrap();
    assert_eq!(report.categories.not_captured, 1);
    assert!(report.ok);
}

#[test]
fn read_failed_and_missing_attachment_are_categorized() {
    let (temp, archive) = make_root();
    let failed = capture_source(
        &archive,
        &temp.path().join("missing.jsonl"),
        "codex",
        DOMAIN,
        None,
    )
    .unwrap();
    assert_eq!(failed.status, "failed");
    let report = archive.coverage_with(&ReportConfig::default()).unwrap();
    assert!(!report.ok);
    assert_eq!(report.categories.read_failed, 1);
    assert!(report
        .sources
        .iter()
        .all(|row| has_category(row, CATEGORY_READ_FAILED)));

    // A missing attachment is recorded in the manifest and categorized.
    let source = temp.path().join("rollout-bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb.jsonl");
    write_codex(&source, "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb", &[("user", "att")]);
    capture(&archive, &source);
    let request = AttachmentRequest {
        provider: "codex".to_string(),
        source: source.to_string_lossy().to_string(),
        content_ref: ContentRef::File {
            rel_path: "does-not-exist.png".to_string(),
            pointer: String::new(),
        },
        target: "local_file".to_string(),
        roots: vec![temp.path().to_string_lossy().to_string()],
        access: vec![DOMAIN.to_string()],
        max_bytes: None,
        record_sha256: None,
        media_type: None,
    };
    let attached = capture_attachment(&archive, &request).unwrap();
    assert_eq!(attached.status, "missing");
    let report = archive.coverage_with(&ReportConfig::default()).unwrap();
    assert!(!report.ok);
    assert!(report.categories.attachment_missing >= 1);
    assert!(report
        .sources
        .iter()
        .any(|row| has_category(row, CATEGORY_ATTACHMENT_MISSING)));
    assert_eq!(report.totals.attachments.missing, 1);
}

#[test]
fn derived_parse_failure_is_a_parse_category_not_a_hidden_skip() {
    let (temp, archive) = make_root();
    // A whole-file OpenCode source (not a database, not a JSONL rollout) is a
    // valid capture unit, but the provider cannot parse it as a message tree.
    let source = temp.path().join("opencode-part.json");
    write(
        &source,
        br#"{"id":"part-1","type":"text","text":"synthetic"}"#,
    );
    let captured = capture_source(&archive, &source, "opencode", DOMAIN, None).unwrap();
    assert_eq!(captured.status, "captured");

    let identity = ReplayIdentity::current();
    let run = replay::replay(&archive, &identity, DOMAIN).unwrap();
    assert!(!run.ok);
    assert_eq!(run.failed_sources, 1);
    assert_eq!(run.sources[0].status, "parse_failed");
    assert!(run.sources[0].error.is_some());

    let coverage = archive.coverage_with(&ReportConfig::default()).unwrap();
    assert!(!coverage.ok);
    assert!(coverage.categories.parse_failed >= 1);
    assert!(coverage
        .sources
        .iter()
        .any(|row| has_category(row, CATEGORY_PARSE_FAILED)));
}

#[test]
fn cli_replay_partial_failure_exits_nine() {
    let (temp, archive) = make_root();
    let source = temp.path().join("opencode-part.json");
    write(
        &source,
        br#"{"id":"part-cli","type":"text","text":"synthetic"}"#,
    );
    let captured = capture_source(&archive, &source, "opencode", DOMAIN, None).unwrap();
    assert_eq!(captured.status, "captured");

    // The CLI contract: a partial/failed replay prints its JSON report on
    // stdout and exits 9, never 0.
    let output = cli()
        .arg("replay")
        .arg("--root")
        .arg(temp.path())
        .arg("--domain")
        .arg(DOMAIN)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(9), "replay partial must exit 9");
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["ok"], false);
    assert_eq!(value["failedSources"], 1);
    assert_eq!(value["sources"][0]["status"], "parse_failed");

    // The same current-view failure is a coverage anomaly (exit 8).
    let output = cli()
        .arg("coverage")
        .arg("--root")
        .arg(temp.path())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(8));
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["ok"], false);
    assert!(value["categories"]["parseFailed"].as_u64().unwrap() >= 1);
}

// ---------------------------------------------------------------------------
// 4. Derived replay: generations, checkpoint reuse, rebuild, fail-closed read
// ---------------------------------------------------------------------------

#[test]
fn replay_generations_checkpoints_and_rebuild_are_consistent() {
    let (temp, archive) = make_root();
    let dir = temp.path().join("codex");
    fs::create_dir_all(&dir).unwrap();
    let first = dir.join("rollout-10101010-1010-4010-8010-101010101010.jsonl");
    let second = dir.join("rollout-20202020-2020-4020-8020-202020202020.jsonl");
    write_codex(&first, "10101010-1010-4010-8010-101010101010", &[("user", "one")]);
    write_codex(&second, "20202020-2020-4020-8020-202020202020", &[("user", "two")]);
    capture(&archive, &first);
    capture(&archive, &second);
    let manifests_before = archive.manifests().unwrap();
    let manifest_digest_before = hex_sha256(serde_json::to_vec(manifests_before.as_slice()).unwrap().as_slice());

    let v1 = ReplayIdentity {
        parser_version: "v1".to_string(),
        model_version: 1,
        archive_schema_version: 1,
    };
    let first_run = replay::replay(&archive, &v1, DOMAIN).unwrap();
    assert!(first_run.ok, "{:?}", first_run.anomalies);
    assert_eq!(first_run.mode, "new_generation");
    assert_eq!(first_run.generation, 1);
    assert_eq!(first_run.replayed_sources, 2);
    assert_eq!(first_run.reused_sources, 0);
    assert!(first_run.messages >= 2);
    assert_eq!(first_run.manifest_digest_before, first_run.manifest_digest_after);
    assert_eq!(
        manifest_digest_before,
        hex_sha256(serde_json::to_vec(archive.manifests().unwrap().as_slice()).unwrap().as_slice()),
        "replay must not rewrite capture facts"
    );
    let entry = first_run
        .sources
        .iter()
        .find(|entry| entry.source == first.to_string_lossy().replace('\\', "/"))
        .or_else(|| first_run.sources.iter().find(|entry| entry.source.contains("1010")))
        .expect("first source report");
    assert_eq!(entry.status, "replayed");
    assert!(entry.records_replayed > 0);
    let derived_ref = entry.derived_ref.clone().expect("derived ref");
    let artifact_a = artifact_path(temp.path(), 1, &entry.source);
    let _digest_a = hex_sha256(&fs::read(&artifact_a).unwrap());

    // Every source is byte-exact on readback through the reference handle.
    let (bytes, meta) = replay::read_derived(&archive, &derived_ref, DOMAIN).unwrap();
    assert!(meta.verified);
    assert_eq!(meta.sha256, hex_sha256(&bytes));
    assert_eq!(meta.source, entry.source);
    assert_eq!(
        serde_json::from_slice::<Value>(&bytes).unwrap()["schema"],
        "agent-memory/derived-source@1"
    );

    // Same parser again: no source is re-read or re-parsed.
    let second_run = replay::replay(&archive, &v1, DOMAIN).unwrap();
    assert_eq!(second_run.mode, "resumed");
    assert_eq!(second_run.reused_sources, 2);
    assert_eq!(second_run.replayed_sources, 0);
    assert!(second_run.sources.iter().all(|entry| entry.checkpoint_reused));
    assert_eq!(second_run.messages, first_run.messages);

    // Append to only one source: that source is replayed, the other reused.
    append_codex(
        &first,
        "10101010-1010-4010-8010-101010101010",
        "assistant",
        "one more",
        30,
    );
    capture(&archive, &first);
    let third_run = replay::replay(&archive, &v1, DOMAIN).unwrap();
    assert_eq!(third_run.replayed_sources, 1);
    assert_eq!(third_run.reused_sources, 1);
    assert!(third_run
        .sources
        .iter()
        .any(|entry| entry.checkpoint_reused && entry.records_replayed == 0));
    assert!(third_run
        .sources
        .iter()
        .any(|entry| entry.status == "replayed" && entry.records_replayed > 0));

    // A new parser version gets a new generation; the old generation and its
    // artifacts are left untouched.
    let v2 = ReplayIdentity {
        parser_version: "v2".to_string(),
        ..v1.clone()
    };
    let v2_run = replay::replay(&archive, &v2, DOMAIN).unwrap();
    assert_eq!(v2_run.mode, "new_generation");
    assert_eq!(v2_run.generation, 2);
    assert_eq!(v2_run.replayed_sources, 2);
    assert_eq!(v2_run.reused_sources, 0);
    assert!(artifact_a.exists(), "old derived generation stays on disk");
    let state = read_state(temp.path());
    assert_eq!(state["generations"].as_array().unwrap().len(), 2);

    // Old derived artifact is wrong/truncated: replay with the same identity
    // detects the inconsistent checkpoint and rebuilds from raw evidence.
    let v2_entry = v2_run
        .sources
        .iter()
        .find(|entry| entry.source == entry_source(&v2_run))
        .expect("v2 entry");
    let v2_artifact = artifact_path(temp.path(), 2, &v2_entry.source);
    let wrong = serde_json::json!({
        "schema": "agent-memory/derived-source@1",
        "source": v2_entry.source,
        "sessions": [],
    });
    write(&v2_artifact, &serde_json::to_vec_pretty(&wrong).unwrap());
    let repaired = replay::replay(&archive, &v2, DOMAIN).unwrap();
    assert!(repaired.replayed_sources >= 1);
    assert!(repaired
        .anomalies
        .iter()
        .any(|line| line.contains("checkpoint did not match")));
    let rebuilt: Value = serde_json::from_slice(&fs::read(&v2_artifact).unwrap()).unwrap();
    assert!(!rebuilt["sessions"].as_array().unwrap().is_empty());
    let expected_bytes = fs::read(&v2_artifact).unwrap();

    // Delete the whole derived layer: the same parser rebuilds byte-identical
    // artifacts from the archive alone.
    fs::remove_dir_all(temp.path().join("derived")).unwrap();
    let rebuilt_run = replay::replay(&archive, &v2, DOMAIN).unwrap();
    assert_eq!(rebuilt_run.generation, 1);
    assert_eq!(
        fs::read(artifact_path(temp.path(), 1, &v2_entry.source)).unwrap(),
        expected_bytes
    );
}

fn entry_source(run: &replay::ReplayReport) -> String {
    run.sources
        .iter()
        .find(|entry| entry.source.contains("1010"))
        .map(|entry| entry.source.clone())
        .expect("source containing 1010")
}

#[test]
fn derived_read_is_fail_closed_on_missing_access_and_rebuilding() {
    let (temp, archive) = make_root();
    let source = temp.path().join("rollout-dddddddd-dddd-4ddd-8ddd-dddddddddddd.jsonl");
    write_codex(&source, "dddddddd-dddd-4ddd-8ddd-dddddddddddd", &[("user", "one")]);
    capture(&archive, &source);
    let identity = ReplayIdentity::current();
    let run = replay::replay(&archive, &identity, DOMAIN).unwrap();
    let derived_ref = run.sources[0].derived_ref.clone().unwrap();

    // Empty access labels deny (manifest-derived permission mapping missing).
    let mut state = read_state(temp.path());
    state["generations"][0]["sources"][0]["access"] = json!([]);
    write_state(temp.path(), &state);
    let denied = replay::read_derived(&archive, &derived_ref, DOMAIN).unwrap_err();
    assert!(matches!(
        denied,
        agent_memory::archive::ArchiveError::AccessDenied(_)
    ));

    // Rebuilding generation denies even a valid entry.
    state["generations"][0]["sources"][0]["access"] = json!(["default"]);
    state["generations"][0]["status"] = json!("rebuilding");
    write_state(temp.path(), &state);
    let denied = replay::read_derived(&archive, &derived_ref, DOMAIN).unwrap_err();
    assert!(matches!(
        denied,
        agent_memory::archive::ArchiveError::AccessDenied(_)
    ));

    // A rebuilding generation is an anomaly in the coverage report, not a
    // normal `ok=true` state, and the CLI must use the anomaly exit code 8.
    let coverage = archive.coverage_with(&ReportConfig::default()).unwrap();
    assert!(!coverage.ok, "rebuilding derived layer must not report ok");
    assert_eq!(
        coverage.derived.as_ref().expect("derived report").status,
        "rebuilding"
    );
    assert!(coverage
        .anomalies
        .iter()
        .any(|line| line.contains("mid-rebuild")));
    let output = cli()
        .arg("coverage")
        .arg("--root")
        .arg(temp.path())
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(8),
        "rebuilding coverage must exit 8"
    );
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["ok"], false);
    assert_eq!(value["derived"]["status"], "rebuilding");

    // An unknown reference is not found, never a fallback read.
    let missing = replay::read_derived(&archive, "derived:99:deadbeef", DOMAIN).unwrap_err();
    assert!(matches!(
        missing,
        agent_memory::archive::ArchiveError::NotFound(_)
    ));
}

#[test]
fn opencode_derived_replay_comes_from_the_archived_observation() {
    let (temp, archive) = make_root();
    let db = temp.path().join("opencode.db");
    build_opencode_db(&db);
    let captured = capture_source(&archive, &db, "opencode", DOMAIN, None).unwrap();
    assert_eq!(captured.status, "captured");
    // Remove the live source: replay must use the baseline snapshot + deltas.
    fs::remove_file(&db).unwrap();
    let identity = ReplayIdentity {
        parser_version: "v1".to_string(),
        model_version: 1,
        archive_schema_version: 1,
    };
    let run = replay::replay(&archive, &identity, DOMAIN).unwrap();
    assert!(run.ok, "{:?}", run.anomalies);
    assert_eq!(run.replayed_sources, 1);
    assert_eq!(run.sources[0].sessions, 1);
    assert!(run.sources[0].messages >= 1);
    assert!(run.sources[0].artifact.is_some());
}

// ---------------------------------------------------------------------------
// 5. Scrub: tampering, staging skip, writer lock, corrupt manifest
// ---------------------------------------------------------------------------

#[test]
fn scrub_detects_tampering_and_coverage_reports_the_damage() {
    let (temp, archive) = make_root();
    let source = temp.path().join("rollout-eeeeeeee-eeee-4eee-8eee-eeeeeeeeeeee.jsonl");
    write_codex(&source, "eeeeeeee-eeee-4eee-8eee-eeeeeeeeeeee", &[("user", "one")]);
    capture(&archive, &source);

    let clean = scrub::scrub(&archive, false).unwrap();
    assert!(clean.ok);
    assert_eq!(clean.status, "clean");
    assert!(clean.objects_checked >= 1);
    assert!(clean.corrupted.is_empty());
    let report = archive.coverage_with(&ReportConfig::default()).unwrap();
    assert!(report.ok);
    assert_eq!(report.scrub.as_ref().unwrap().status, "clean");
    assert_eq!(report.categories.corruption, 0);

    // A staging file is an in-flight write, explicitly skipped, not corruption.
    write(&temp.path().join("archive/staging/pending-object"), b"in flight");
    let staged = scrub::scrub(&archive, false).unwrap();
    assert!(staged.ok);
    assert_eq!(staged.staging_skipped.len(), 1);

    let manifests = archive.manifests().unwrap();
    let sha = manifests[0].objects[0].sha256.clone();
    let (object, mut bytes) = support::read_object_file(temp.path(), &sha);
    bytes[0] ^= 0xff;
    fs::write(&object, &bytes).unwrap();

    let corrupted = scrub::scrub(&archive, false).unwrap();
    assert!(!corrupted.ok);
    assert_eq!(corrupted.status, "corrupted");
    assert_eq!(corrupted.corrupted.len(), 1);
    assert_eq!(corrupted.corrupted[0].sha256, sha);
    assert!(!corrupted.corrupted[0].references.is_empty());
    assert!(!corrupted.corrupted[0].sources.is_empty());

    let report = archive.coverage_with(&ReportConfig::default()).unwrap();
    assert!(!report.ok);
    assert_eq!(report.categories.corruption, 1);
    assert!(report
        .sources
        .iter()
        .any(|row| has_category(row, CATEGORY_CORRUPTION)));
    assert_eq!(report.scrub.as_ref().unwrap().corrupted, 1);
    assert!(report
        .anomalies
        .iter()
        .any(|line| line.contains("scrub: object")));

    // CLI reports the payload on stdout with the dedicated corruption code.
    let output = cli()
        .arg("scrub")
        .arg("--root")
        .arg(temp.path())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(10));
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["status"], "corrupted");
    let output = cli()
        .arg("coverage")
        .arg("--root")
        .arg(temp.path())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(8));
}

#[test]
fn scrub_refuses_a_concurrent_writer_unless_explicitly_skipping() {
    let (temp, archive) = make_root();
    let source = temp.path().join("rollout-ffffffff-ffff-4fff-8fff-ffffffffffff.jsonl");
    write_codex(&source, "ffffffff-ffff-4fff-8fff-ffffffffffff", &[("user", "one")]);
    capture(&archive, &source);

    let lock = temp.path().join("locks/capture.lock");
    write(&lock, b"pid=1 acquired_at_unix_ms=0\n");
    let refused = scrub::scrub(&archive, false).unwrap_err();
    assert!(matches!(
        refused,
        agent_memory::archive::ArchiveError::LockHeld(_)
    ));
    let output = cli()
        .arg("scrub")
        .arg("--root")
        .arg(temp.path())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(4));

    let skipped = scrub::scrub(&archive, true).unwrap();
    assert_eq!(skipped.status, "skipped_writer_active");
    assert_eq!(skipped.writer, "skipped_writer_active");
    assert_eq!(skipped.objects_checked, 0);
    assert!(!skipped.anomalies.is_empty());
    let output = cli()
        .arg("scrub")
        .arg("--root")
        .arg(temp.path())
        .arg("--skip-if-locked")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0));
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["status"], "skipped_writer_active");
    std::fs::remove_file(&lock).unwrap();
}

#[test]
fn corrupt_manifest_is_an_error_not_an_empty_report() {
    let (temp, archive) = make_root();
    let source = temp.path().join("rollout-99999999-9999-4999-8999-999999999999.jsonl");
    write_codex(&source, "99999999-9999-4999-8999-999999999999", &[("user", "one")]);
    capture(&archive, &source);
    write(
        &temp
            .path()
            .join("archive/manifests/99999999-corrupt-manifest.json"),
        b"{not valid json",
    );
    assert!(archive.coverage().is_err());
    assert!(archive.coverage_with(&ReportConfig::default()).is_err());
    assert!(replay::replay(
        &archive,
        &ReplayIdentity {
            parser_version: "v1".to_string(),
            model_version: 1,
            archive_schema_version: 1,
        },
        DOMAIN,
    )
    .is_err());
    let output = cli()
        .arg("coverage")
        .arg("--root")
        .arg(temp.path())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(3));
    let value: Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(value["ok"], false);
}

// ---------------------------------------------------------------------------
// 6. CLI JSON contract, observation window and reference readback
// ---------------------------------------------------------------------------

#[test]
fn cli_json_exit_codes_and_reference_readback_are_stable() {
    let (temp, archive) = make_root();
    let source = temp.path().join("rollout-77777777-7777-4777-8777-777777777777.jsonl");
    write_codex(&source, "77777777-7777-4777-8777-777777777777", &[("user", "cli")]);
    let captured = capture(&archive, &source);
    assert_eq!(captured.records_new, 2);
    append_codex(
        &source,
        "77777777-7777-4777-8777-777777777777",
        "assistant",
        "second run",
        20,
    );
    capture(&archive, &source);

    let report = archive.coverage_with(&ReportConfig::default()).unwrap();
    let row = &report.sources[0];
    let observation = row.observation.as_ref().expect("observation window");
    assert_eq!(observation.kind, "records");
    assert!(observation.watermark_at_ms > 0);
    assert!(observation.interval_ms.is_some());
    assert!(observation.blind_window.contains("not observed"));
    assert!(report.derived.as_ref().unwrap().sources_lagging >= 1);
    assert!(has_category(row, CATEGORY_DERIVED_LAG));

    // Coverage CLI: JSON on stdout, exit 0 when only derived lag is present.
    let output = cli()
        .arg("coverage")
        .arg("--root")
        .arg(temp.path())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0));
    let coverage: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(coverage["ok"], true);
    assert!(coverage["categories"]["derivedLag"].as_u64().unwrap() >= 1);
    assert_eq!(coverage["sources"][0]["observation"]["kind"], "records");
    assert!(coverage["providers"].as_array().unwrap().iter().any(|provider| {
        provider["provider"] == "codex" && provider["archivedSources"].as_u64().unwrap() >= 1
    }));

    // Record readback: the returned reference is the record sha256.
    let raw = fs::read(&source).unwrap();
    let first_line = raw.split(|byte| *byte == b'\n').next().unwrap().to_vec();
    let record_sha = hex_sha256(&first_line);
    let out_path = temp.path().join("record.out");
    let output = cli()
        .arg("read")
        .arg("--root")
        .arg(temp.path())
        .arg("--sha256")
        .arg(&record_sha)
        .arg("--domain")
        .arg(DOMAIN)
        .arg("--out")
        .arg(&out_path)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(fs::read(&out_path).unwrap(), first_line);
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["kind"], "record");
    assert_eq!(value["sha256"], record_sha);

    // Replay CLI + derived reference readback.
    let output = cli()
        .arg("replay")
        .arg("--root")
        .arg(temp.path())
        .arg("--domain")
        .arg(DOMAIN)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0));
    let replay_value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(replay_value["ok"], true);
    let derived_ref = replay_value["sources"][0]["derivedRef"]
        .as_str()
        .expect("derived ref");
    let derived_out = temp.path().join("derived.out");
    let output = cli()
        .arg("derived-read")
        .arg("--root")
        .arg(temp.path())
        .arg("--derived-ref")
        .arg(derived_ref)
        .arg("--domain")
        .arg(DOMAIN)
        .arg("--out")
        .arg(&derived_out)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0));
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["kind"], "derived");
    assert_eq!(value["derived"]["verified"], true);
    let artifact: Value = serde_json::from_slice(&fs::read(&derived_out).unwrap()).unwrap();
    assert_eq!(artifact["schema"], "agent-memory/derived-source@1");

    // Unknown derived ref: not found (6), never a fallback.
    let output = cli()
        .arg("derived-read")
        .arg("--root")
        .arg(temp.path())
        .arg("--derived-ref")
        .arg("derived:9:not-a-source-key")
        .arg("--domain")
        .arg(DOMAIN)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(6));
    // Missing --domain is a usage error.
    let output = cli()
        .arg("replay")
        .arg("--root")
        .arg(temp.path())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
}

#[test]
fn compressed_codex_rollout_replays_from_the_decoded_archive_object() {
    let (temp, archive) = make_root();
    let path = temp
        .path()
        .join("rollout-12121212-1212-4212-8212-121212121212.jsonl.zst");
    let mut lines = vec![codex_meta("12121212-1212-4212-8212-121212121212")];
    lines.push(codex_line(
        "12121212-1212-4212-8212-121212121212",
        "user",
        "compressed hello",
        1,
    ));
    let compressed = zstd::stream::encode_all((lines.join("\n") + "\n").as_bytes(), 3).unwrap();
    write(&path, &compressed);
    let captured = capture_source(&archive, &path, "codex", DOMAIN, None).unwrap();
    assert_eq!(captured.status, "captured");
    assert!(archive
        .manifests()
        .unwrap()
        .iter()
        .any(|manifest| manifest.objects.iter().any(|object| object.role == "decoded")));

    let identity = ReplayIdentity {
        parser_version: "v1".to_string(),
        model_version: 1,
        archive_schema_version: 1,
    };
    let run = replay::replay(&archive, &identity, DOMAIN).unwrap();
    assert!(run.ok, "{:?}", run.anomalies);
    assert_eq!(run.sources[0].kind, "compressed_jsonl_file");
    assert!(run.sources[0].messages >= 1);
    let again = replay::replay(&archive, &identity, DOMAIN).unwrap();
    assert_eq!(again.reused_sources, 1);
}

#[test]
fn corrupt_derived_index_and_scrub_ledger_are_reported_without_hiding_coverage() {
    let (temp, archive) = make_root();
    let source = temp.path().join("rollout-13131313-1313-4313-8313-131313131313.jsonl");
    write_codex(&source, "13131313-1313-4313-8313-131313131313", &[("user", "one")]);
    capture(&archive, &source);
    let identity = ReplayIdentity::current();
    replay::replay(&archive, &identity, DOMAIN).unwrap();

    // A missing derived artifact is an index inconsistency, not "up to date".
    let artifact = artifact_path(temp.path(), 1, &only_source(&archive));
    fs::remove_file(&artifact).unwrap();
    let report = archive.coverage_with(&ReportConfig::default()).unwrap();
    assert!(!report.ok);
    assert!(report.categories.derived_lag >= 1);
    assert!(report
        .anomalies
        .iter()
        .any(|line| line.contains("derived artifact") && line.contains("missing")));
    let repaired = replay::replay(&archive, &identity, DOMAIN).unwrap();
    assert_eq!(repaired.replayed_sources, 1);
    assert!(artifact.exists());

    // Corrupt derived index: coverage still reports raw capture evidence, with
    // an explicit derived anomaly and `ok=false`; replay itself fails closed.
    write(&state_path(temp.path()), b"{not json");
    let report = archive.coverage_with(&ReportConfig::default()).unwrap();
    assert!(!report.ok);
    assert_eq!(report.derived.as_ref().unwrap().status, "corrupt");
    assert!(report
        .anomalies
        .iter()
        .any(|line| line.contains("derived checkpoint index is unreadable")));
    assert_eq!(report.categories.covered, 1);
    assert!(replay::replay(&archive, &identity, DOMAIN).is_err());

    // Corrupt scrub ledger: reportable, never "no damage".
    write(&temp.path().join("state/scrub.json"), b"{not json");
    let report = archive.coverage_with(&ReportConfig::default()).unwrap();
    assert!(!report.ok);
    assert_eq!(report.scrub.as_ref().unwrap().status, "ledger_corrupt");
    assert!(report
        .anomalies
        .iter()
        .any(|line| line.contains("scrub ledger is unreadable")));
    assert!(scrub::scrub(&archive, false).is_err());
}

#[test]
fn report_replay_and_scrub_leave_the_source_untouched() {
    let (temp, archive) = make_root();
    let source = temp.path().join("rollout-88888888-8888-4888-8888-888888888888.jsonl");
    write_codex(&source, "88888888-8888-4888-8888-888888888888", &[("user", "ro")]);
    capture(&archive, &source);
    let before = tree_state(temp.path());
    let _ = archive.coverage_with(&ReportConfig::default()).unwrap();
    let _ = replay::replay(
        &archive,
        &ReplayIdentity {
            parser_version: "v1".to_string(),
            model_version: 1,
            archive_schema_version: 1,
        },
        DOMAIN,
    )
    .unwrap();
    let _ = scrub::scrub(&archive, false).unwrap();
    // The archive root itself grows (derived/state written by design); the
    // *source* tree must be byte- and mtime-identical.
    let source_before: Vec<_> = before
        .into_iter()
        .filter(|(path, _, _)| path.starts_with("rollout-"))
        .collect();
    let after = tree_state(temp.path());
    let source_after: Vec<_> = after
        .into_iter()
        .filter(|(path, _, _)| path.starts_with("rollout-"))
        .collect();
    assert_eq!(source_before, source_after);
}


