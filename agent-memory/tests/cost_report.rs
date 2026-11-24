//! §A measurement and §E background mode: the CLI reports carry a `cost` block
//! (process I/O counters and per-stage times) and, only when `--background` was
//! requested, a `background` block.
//!
//! Every fixture is synthetic and lives under a per-test temporary root; no
//! test points at a real session directory, database or Archive.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use agent_memory::archive::Archive;
use serde_json::{json, Value};

mod support;

use support::TestEnv;

fn cli() -> Command {
    Command::new(env!("CARGO_BIN_EXE_agent-memory"))
}

struct RawRun {
    code: i32,
    stdout: String,
    stderr: String,
}

fn run_raw(args: &[&str]) -> RawRun {
    let out = cli().args(args).output().expect("run agent-memory");
    RawRun {
        code: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8(out.stdout).expect("stdout utf-8"),
        stderr: String::from_utf8(out.stderr).expect("stderr utf-8"),
    }
}

struct Run {
    code: i32,
    report: Value,
    stdout: String,
    stderr: String,
}

fn run(args: &[&str]) -> Run {
    let out = cli().args(args).output().expect("run agent-memory");
    let stdout = String::from_utf8(out.stdout).expect("stdout utf-8");
    let stderr = String::from_utf8(out.stderr).expect("stderr utf-8");
    let report = serde_json::from_str(stdout.trim()).unwrap_or_else(|error| {
        panic!("report is not JSON ({error}): {stdout} / {stderr}")
    });
    Run {
        code: out.status.code().unwrap_or(-1),
        report,
        stdout,
        stderr,
    }
}

fn write_lines(path: &Path, lines: &[String]) {
    let mut body = lines.join("\n");
    body.push('\n');
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("fixture parent dir");
    }
    fs::write(path, body).expect("write fixture file");
}

fn write_dsh_log(path: &Path, lines: &[String]) {
    let mut bytes = Vec::new();
    for line in lines {
        let frame = zstd::stream::encode_all(format!("{line}\n").as_bytes(), 3).expect("frame");
        bytes.extend_from_slice(&frame);
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("dsh session dir");
    }
    fs::write(path, bytes).expect("write dsh log");
}

fn claude_line(session: &str, text: &str) -> String {
    json!({
        "type": "user",
        "message": {"role": "user", "content": [{"type": "text", "text": text}]},
        "timestamp": "2026-10-25T13:25:00Z",
        "sessionId": session,
        "uuid": "u-1",
        "isSidechain": false
    })
    .to_string()
}

struct Fixture {
    env: TestEnv,
    config: PathBuf,
    archive_root: PathBuf,
    kb_root: PathBuf,
    opencode_db: PathBuf,
}

fn build_fixture(name: &str) -> Fixture {
    let env = TestEnv::new();
    let archive_root = env.root(name);
    Archive::init(&archive_root).expect("init runtime root");
    let cache_root = env.root("cache");

    let claude_root = env.root("claude-projects");
    write_lines(
        &claude_root.join("proj").join("11111111-1111-4111-8111-111111111111.jsonl"),
        &[claude_line(
            "11111111-1111-4111-8111-111111111111",
            "cost needle claude",
        )],
    );

    let codex_root = env.root("codex-home");
    write_lines(
        &codex_root
            .join("sessions")
            .join("2026")
            .join("10")
            .join("25")
            .join("rollout-a.jsonl"),
        &[
            json!({"timestamp": "2026-10-25T10:00:00Z", "type": "session_meta",
                   "payload": {"id": "cost-codex"}})
            .to_string(),
            json!({"timestamp": "2026-10-25T10:00:00Z", "type": "response_item",
                   "payload": {"type": "message", "role": "assistant",
                               "content": [{"type": "output_text", "text": "cost needle codex"}]},
                   "seq": 1})
            .to_string(),
        ],
    );

    let opencode_fixture = support::build_v1(&env);
    let opencode_root = opencode_fixture.root.clone();
    let opencode_db = opencode_fixture.db.clone();

    let dsh_root = env.root("dsh-sessions");
    write_dsh_log(
        &dsh_root
            .join("--C-Users-x--")
            .join("session-cost")
            .join("session.jsonl.zstd"),
        &[
            json!({"type": "session", "version": 4, "id": "cost-dsh",
                   "createdAt": 1_782_000_000_000i64, "cwd": "C:\\Users\\x"})
            .to_string(),
            json!({"type": "user/message", "seq": 1, "time": 1_782_000_000_001i64,
                   "data": {"id": "m-1", "role": "user", "source": {"kind": "user"},
                            "content": [{"type": "text", "text": "cost needle dsh"}]}})
            .to_string(),
        ],
    );

    let kb_root = env.root("kb-summaries");
    fs::create_dir_all(kb_root.join(".meta").join("summaries")).expect("kb summaries dir");

    let config = env.write_json(
        "recall.json",
        &json!({
            "version": 2,
            "sources": [
                {"source_id": "kb", "kind": "kb", "root": kb_root.to_string_lossy(), "enabled": true},
                {"source_id": "claude_code", "kind": "claude_code",
                 "root": claude_root.to_string_lossy(), "enabled": true},
                {"source_id": "codex", "kind": "codex",
                 "root": codex_root.to_string_lossy(), "enabled": true},
                {"source_id": "opencode", "kind": "opencode",
                 "root": opencode_root.to_string_lossy(), "enabled": true},
                {"source_id": "dsh", "kind": "dsh",
                 "root": dsh_root.to_string_lossy(), "enabled": true},
                {"source_id": "archive", "kind": "archive",
                 "root": archive_root.to_string_lossy(), "enabled": false}
            ],
            "source_order": ["kb", "claude_code", "codex", "opencode", "dsh"],
            "interpreter": {"kb": {"command": "C:/python/python.exe"}},
            "domain": "default",
            "telemetry": {"enabled": false},
            "cache_root": cache_root.to_string_lossy()
        }),
    );

    Fixture {
        env,
        config,
        archive_root,
        kb_root,
        opencode_db,
    }
}

impl Fixture {
    fn config_arg(&self) -> String {
        self.config.to_string_lossy().to_string()
    }

    /// A configuration that keeps only the named sources (plus their order).
    fn index_config(&self, keep: &[&str], embedding: bool) -> PathBuf {
        let mut value: Value =
            serde_json::from_str(&fs::read_to_string(&self.config).expect("config"))
                .expect("config JSON");
        if embedding {
            // The deterministic in-process backend: no model call, but the
            // vector stage still runs and is measured.
            value["interpreter"]["embedding"] = json!({
                "model_id": "fake-hash-v1",
                "dimension": 32,
                "normalization": "l2",
                "backend": "fake"
            });
        }
        if let Some(sources) = value["sources"].as_array_mut() {
            sources.retain(|source| {
                source["source_id"]
                    .as_str()
                    .map(|id| keep.contains(&id))
                    .unwrap_or(false)
            });
        }
        let order: Vec<String> = keep
            .iter()
            .filter(|id| **id != "archive")
            .map(|id| id.to_string())
            .collect();
        value["source_order"] = json!(order);
        self.env.write_json("index-recall.json", &value)
    }

    fn source<'a>(&self, report: &'a Value, source_id: &str) -> &'a Value {
        report["sources"]
            .as_array()
            .expect("sources array")
            .iter()
            .find(|entry| entry["source_id"] == json!(source_id))
            .unwrap_or_else(|| panic!("source {source_id} missing: {report}"))
    }

    fn phases(report: &Value) -> Vec<String> {
        report["cost"]["phases"]
            .as_object()
            .map(|map| map.keys().cloned().collect())
            .unwrap_or_default()
    }

    fn has_phase(report: &Value, name: &str) -> bool {
        Self::phases(report).iter().any(|phase| phase == name)
    }
}

/// On Windows the counters are numbers; elsewhere the fields are `null` and the
/// reason is named (never an error).
fn assert_cost_shape(cost: &Value, what: &str) {
    assert!(
        cost.is_object(),
        "{what}: cost must be an object: {cost}"
    );
    assert!(
        cost["elapsed_ms"].is_number(),
        "{what}: elapsed_ms must be measured: {cost}"
    );
    let read = &cost["read_bytes"];
    let write = &cost["write_bytes"];
    let ops = &cost["read_ops"];
    if cfg!(windows) {
        assert!(read.is_number(), "{what}: read_bytes must be numeric: {cost}");
        assert!(write.is_number(), "{what}: write_bytes must be numeric: {cost}");
        assert!(ops.is_number(), "{what}: read_ops must be numeric: {cost}");
    } else {
        assert!(read.is_null() && write.is_null() && ops.is_null(), "{cost}");
        assert!(
            cost["cost_unavailable"].is_string(),
            "{what}: the reason must be named: {cost}"
        );
    }
    if let Some(phases) = cost["phases"].as_object() {
        for (name, phase) in phases {
            assert!(
                phase["elapsed_ms"].is_number(),
                "{what}: phase {name} must carry elapsed_ms: {phase}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// 1. capture-all: whole-run and per-source cost, with the documented stages.
// ---------------------------------------------------------------------------

#[test]
fn capture_all_reports_cost_and_per_source_phases() {
    let fixture = build_fixture("cost-capture");
    let run = run(&[
        "capture-all",
        "--config",
        &fixture.config_arg(),
        "--background",
    ]);
    assert_eq!(run.code, 0, "{} {}", run.stdout, run.stderr);
    let report = &run.report;
    assert_cost_shape(&report["cost"], "capture-all");

    for source_id in ["claude_code", "codex", "opencode", "dsh"] {
        assert_cost_shape(&fixture.source(report, source_id)["cost"], source_id);
    }

    // JSONL / DSH stages: enumeration, read, parse, hash, compressed write.
    for source_id in ["claude_code", "codex", "dsh"] {
        let source = fixture.source(report, source_id);
        for phase in ["enumerate", "read", "parse", "hash", "compress_write"] {
            assert!(
                Fixture::has_phase(source, phase),
                "{source_id}: missing {phase}: {source}"
            );
        }
    }
    // implementation-005: the record ledger is read once and written once per
    // capture-all round, so its stages belong to the round, not to a file.
    // `records_read` still shows up where the load happened (the first capturing
    // source); the write happens after the last source and is the round's.
    assert!(
        fixture.source(report, "claude_code")["cost"]["counters"]["records_read.calls"]
            .as_u64()
            .unwrap_or(0)
            >= 1,
        "the first capturing source loads the ledger: {report}"
    );
    let ledgers = &report["ledgers"];
    assert_eq!(ledgers["recordsLoads"], 1, "one ledger read per round");
    assert_eq!(ledgers["recordsFlushes"], 1, "one ledger write per round");
    assert_eq!(
        ledgers["sourcesFlushes"], 1,
        "one source-state write per round"
    );
    assert_eq!(
        report["cost"]["counters"]["reconcile.calls"], 1,
        "one reconcile per round"
    );
    assert!(
        Fixture::has_phase(report, "ledger_flush"),
        "the round reports its ledger flush: {report}"
    );
    assert!(
        Fixture::has_phase(report, "reconcile"),
        "the round reports its reconcile: {report}"
    );
    assert!(
        report["ledgers"]["recordsFlushes"].as_u64().unwrap_or(0) == 1,
        "the round ledger block agrees: {report}"
    );
    // The whole-file parse probe belongs to the plain-JSONL engine only.
    for source_id in ["claude_code", "codex"] {
        let source = fixture.source(report, source_id);
        assert!(
            Fixture::has_phase(source, "parse_probe"),
            "{source_id}: missing parse_probe: {source}"
        );
        assert!(
            source["cost"]["counters"]["parse_probe.full_reads"]
                .as_u64()
                .unwrap_or(0)
                >= 1,
            "{source_id}: the parse probe is counted: {source}"
        );
    }

    // SQLite stages: the baseline export read and the evidence write, plus the
    // paths implementation-003 brought into the measurement (observation state,
    // manifest projection, ledger, reconciliation) and the per-table watermark
    // sub-phases.
    let opencode = fixture.source(report, "opencode");
    for phase in [
        "watermark_read",
        "evidence_write",
        "manifest_read",
        "observation_state",
    ] {
        assert!(
            Fixture::has_phase(opencode, phase),
            "opencode: missing {phase}: {opencode}"
        );
    }
    assert!(
        opencode["cost"]["counters"]["observation_state.writes"]
            .as_u64()
            .unwrap_or(0)
            >= 1,
        "opencode: the observation state write is counted: {opencode}"
    );
    assert!(
        opencode["cost"]["counters"]["manifest_read.calls"]
            .as_u64()
            .unwrap_or(0)
            >= 1,
        "opencode: the manifest projection is counted: {opencode}"
    );
    // The baseline export also names every whitelisted table: the per-table
    // sub-phases and their row counters must be present on the baseline path
    // too, not only on the delta observation.
    for table in ["session", "message", "part"] {
        let phase = format!("watermark_read.tables.{table}");
        assert!(
            Fixture::has_phase(opencode, &phase),
            "opencode: missing baseline {phase}: {opencode}"
        );
        assert!(
            opencode["cost"]["counters"][format!("watermark_rows.{table}")]
                .as_u64()
                .unwrap_or(0)
                > 0,
            "opencode: baseline rows scanned for {table}: {opencode}"
        );
    }

    // Per-source change classification (one file per class) and the forced
    // recapture byte count.
    for source_id in ["claude_code", "codex", "opencode", "dsh"] {
        let source = fixture.source(report, source_id);
        assert!(
            source["changes"]["initial"].as_u64().unwrap_or(0) >= 1,
            "{source_id}: the first capture is classified: {source}"
        );
        assert_eq!(
            source["changes"]["forced_recapture_bytes"], json!(0),
            "{source_id}: an initial capture is not a forced re-read: {source}"
        );
    }
    // The report still carries no session text.
    assert!(!run.report.to_string().contains("cost needle"));
}

// ---------------------------------------------------------------------------
// 2. §E background: only with the flag, and reported either way.
// ---------------------------------------------------------------------------

#[test]
fn background_is_reported_and_absent_without_the_flag() {
    let fixture = build_fixture("cost-background");
    let with = run(&[
        "capture-all",
        "--config",
        &fixture.config_arg(),
        "--background",
    ]);
    assert_eq!(with.code, 0, "{} {}", with.stdout, with.stderr);
    assert_eq!(with.report["background"]["requested"], true);
    assert!(
        with.report["background"]["applied"].is_boolean(),
        "applied is reported either way: {}",
        with.report["background"]
    );
    if !with.report["background"]["applied"].as_bool().unwrap_or(false) {
        assert!(
            with.report["background"]["error"].is_string(),
            "a failed begin names the reason: {}",
            with.report["background"]
        );
    }

    let without = run(&["capture-all", "--config", &fixture.config_arg()]);
    assert_eq!(without.code, 0, "{} {}", without.stdout, without.stderr);
    assert!(
        without.report.get("background").is_none(),
        "a run without --background behaves exactly as before: {}",
        without.report
    );
    assert_cost_shape(&without.report["cost"], "capture-all without flag");
}

// ---------------------------------------------------------------------------
// 3. index: whole-run cost plus per-source stages (`live_scan` at least).
// ---------------------------------------------------------------------------

#[test]
fn index_reports_cost_with_live_scan_phase() {
    let fixture = build_fixture("cost-index");
    // The V1 OpenCode fixture is captured (SQLite evidence) but not live
    // indexed; this case is about the stage measurement, so only the JSONL
    // source has to build.
    let config = fixture.index_config(&["claude_code", "archive"], true);
    let arg = config.to_string_lossy().to_string();
    let run = run(&["index", "--config", &arg, "--background"]);
    assert_eq!(run.code, 0, "{} {}", run.stdout, run.stderr);
    let report = &run.report;
    assert_cost_shape(&report["cost"], "index");
    assert_eq!(report["background"]["requested"], true);

    let sources = report["sources"].as_array().expect("sources");
    let claude = sources
        .iter()
        .find(|entry| entry["source_id"] == json!("claude_code"))
        .expect("claude_code report");
    assert_cost_shape(&claude["cost"], "index claude_code");
    assert!(
        Fixture::has_phase(claude, "live_scan"),
        "the live scan stage is reported: {claude}"
    );
    assert!(
        Fixture::has_phase(claude, "archive_manifest_parse"),
        "the archive inventory stage is reported: {claude}"
    );
    assert!(
        Fixture::has_phase(claude, "vectors"),
        "the vector stage is reported when embedding is configured: {claude}"
    );
    // §A completeness counters: the manifest projection count (reused from the
    // archive instrumentation) and the units the vector stage read.
    let counters = &claude["cost"]["counters"];
    assert!(
        counters["manifest_parse_count"].is_number(),
        "the manifest parse count is reported: {claude}"
    );
    assert!(
        counters["vectors.units_read"].as_u64().unwrap_or(0) >= 1,
        "the vector stage names the units it read: {claude}"
    );
    assert!(
        counters["vectors.rows_reused"].is_number(),
        "the reuse count is reported: {claude}"
    );
}

// ---------------------------------------------------------------------------
// 4. audit-grounding: the report carries its own cost sample.
// ---------------------------------------------------------------------------

#[test]
fn audit_grounding_reports_cost() {
    let fixture = build_fixture("cost-grounding");
    let kb = fixture.kb_root.to_string_lossy().to_string();
    let measured = run(&[
        "audit-grounding",
        "--kb-summaries",
        &kb,
        "--background",
    ]);
    assert_eq!(measured.code, 0, "{} {}", measured.stdout, measured.stderr);
    assert_cost_shape(&measured.report["cost"], "audit-grounding");
    assert_eq!(measured.report["background"]["requested"], true);

    let without = run(&["audit-grounding", "--kb-summaries", &kb]);
    assert_eq!(without.code, 0, "{} {}", without.stdout, without.stderr);
    assert!(without.report.get("background").is_none());
}

// ---------------------------------------------------------------------------
// 5. An uninitialized/unknown command surface stays a usage error: the flag
//    never turns a broken invocation into a success.
// ---------------------------------------------------------------------------

#[test]
fn background_does_not_mask_a_usage_error() {
    let fixture = build_fixture("cost-usage");
    let run = run_raw(&[
        "capture-all",
        "--config",
        &fixture.config_arg(),
        "--source",
        "does-not-exist",
        "--background",
    ]);
    assert_eq!(run.code, 2, "{} {}", run.stdout, run.stderr);
    let stderr: Value = serde_json::from_str(run.stderr.trim()).expect("error JSON");
    assert_eq!(stderr["ok"], false);
    assert_eq!(stderr["background"]["requested"], true);
}

// ---------------------------------------------------------------------------
// 6. The measurement is additive: the evidence store and ledger are identical
//    with and without the counters (same files, same manifest count).
// ---------------------------------------------------------------------------

#[test]
fn measurement_changes_no_evidence() {
    let fixture = build_fixture("cost-additive");
    let quiet = run(&["capture-all", "--config", &fixture.config_arg()]);
    assert_eq!(quiet.code, 0, "{} {}", quiet.stdout, quiet.stderr);
    let archive = Archive::open(&fixture.archive_root).expect("open archive");
    let manifests_quiet = archive.manifests().expect("manifests").len();
    drop(archive);
    let ledger_quiet = fs::read_to_string(
        fixture.archive_root.join("state").join("capture-all.json"),
    )
    .expect("ledger");
    let records_quiet = quiet.report["totals"]["records_added"].clone();

    // A second run with the counters sees the same unchanged files.
    let measured = run(&[
        "capture-all",
        "--config",
        &fixture.config_arg(),
        "--background",
    ]);
    assert_eq!(measured.code, 0, "{} {}", measured.stdout, measured.stderr);
    let archive = Archive::open(&fixture.archive_root).expect("open archive");
    assert_eq!(archive.manifests().expect("manifests").len(), manifests_quiet);
    drop(archive);
    assert_eq!(
        measured.report["totals"]["records_added"],
        0,
        "the unchanged ledger skips everything: {}",
        measured.report
    );
    for source_id in ["claude_code", "codex", "dsh"] {
        let source = fixture.source(&measured.report, source_id);
        assert!(
            source["changes"]["skipped_unchanged"].as_u64().unwrap_or(0) >= 1,
            "{source_id}: the unchanged round is classified: {source}"
        );
        assert_eq!(
            source["changes"]["forced_recapture_files"], json!(0),
            "{source_id}: nothing is re-read: {source}"
        );
    }
    // The delta observation path carries the per-table watermark sub-phases and
    // their row counters (the first capture is a baseline and observes nothing).
    let opencode = fixture.source(&measured.report, "opencode");
    for table in ["session", "message", "part"] {
        let phase = format!("watermark_read.tables.{table}");
        assert!(
            Fixture::has_phase(opencode, &phase),
            "opencode: missing {phase}: {opencode}"
        );
        assert!(
            opencode["cost"]["counters"][format!("watermark_rows.{table}")]
                .as_u64()
                .unwrap_or(0)
                > 0,
            "opencode: rows scanned for {table}: {opencode}"
        );
    }
    let ledger_measured = fs::read_to_string(
        fixture.archive_root.join("state").join("capture-all.json"),
    )
    .expect("ledger");
    // The ledger describes the same files with the same size and mtime; only
    // the bookkeeping timestamps may differ.
    let stamps = |text: &str| -> Value {
        let value: Value = serde_json::from_str(text).expect("ledger json");
        let mut out = serde_json::Map::new();
        for (source, entry) in value["sources"].as_object().expect("sources") {
            let mut files = serde_json::Map::new();
            for (path, file) in entry["files"].as_object().expect("files") {
                files.insert(
                    path.clone(),
                    json!({"size": file["size"], "mtime_ms": file["mtime_ms"]}),
                );
            }
            out.insert(source.clone(), Value::Object(files));
        }
        Value::Object(out)
    };
    assert_eq!(stamps(&ledger_quiet), stamps(&ledger_measured));
    assert!(
        records_quiet.as_u64().unwrap_or(0) > 0,
        "the first capture adds records: {records_quiet}"
    );
    // No fixture path or body leaks into either report.
    assert!(!measured.stdout.contains("cost needle"));
}


// ---------------------------------------------------------------------------
// 7. SQLite verification rounds are measured as their own stage.
// ---------------------------------------------------------------------------

#[test]
fn sqlite_verification_is_measured_as_its_own_phase() {
    use agent_memory::capture::{capture_source_with_options, CaptureOptions};

    let fixture = build_fixture("cost-verify");
    let archive = Archive::open(&fixture.archive_root).expect("open archive");
    // First run commits the baseline; the second one is a delta round that is
    // due for verification (`--verify`).
    capture_source_with_options(
        &archive,
        &fixture.opencode_db,
        "opencode",
        "default",
        None,
        CaptureOptions::default(),
    )
    .expect("baseline capture");
    agent_memory::cost::begin_recording();
    let report = capture_source_with_options(
        &archive,
        &fixture.opencode_db,
        "opencode",
        "default",
        None,
        CaptureOptions {
            verify: true,
            ..CaptureOptions::default()
        },
    )
    .expect("sqlite capture");
    assert!(report.ok, "capture failed: {report:?}");
    let cost = agent_memory::cost::finish_recording().expect("recording was active");
    for phase in ["watermark_read", "key_compare", "excluded_table_count", "verify"] {
        assert!(
            cost.phases.contains_key(phase),
            "missing {phase}: {:?}",
            cost.phases.keys().collect::<Vec<_>>()
        );
    }
}
