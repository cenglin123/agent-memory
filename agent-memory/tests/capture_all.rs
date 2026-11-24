//! `capture-all` acceptance tests: multi-source enumeration, skip ledger,
//! failure isolation, locking, plan and session-since. Every source here is a
//! synthetic fixture under a `tempfile` root; no test points at a real Claude,
//! Codex or OpenCode session directory, database or Archive.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

use agent_memory::archive::Archive;
use serde_json::{json, Value};

mod support;

use support::TestEnv;

/// A body-only marker: the driver report must never carry session text.
const SENTINEL: &str = "capture-all-sentinel-9f2e";

// ---------------------------------------------------------------------------
// CLI plumbing
// ---------------------------------------------------------------------------

fn cli() -> Command {
    Command::new(env!("CARGO_BIN_EXE_agent-memory"))
}

struct Run {
    code: i32,
    stdout: String,
    stderr: String,
}

fn run_capture_all(extra: &[&str]) -> Run {
    let out = cli()
        .arg("capture-all")
        .args(extra)
        .output()
        .expect("run agent-memory capture-all");
    Run {
        code: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8(out.stdout).expect("stdout utf-8"),
        stderr: String::from_utf8(out.stderr).expect("stderr utf-8"),
    }
}

fn report_of(run: &Run) -> Value {
    serde_json::from_str(run.stdout.trim()).expect("driver report is valid JSON")
}

fn source<'a>(value: &'a Value, source_id: &str) -> &'a Value {
    value["sources"]
        .as_array()
        .expect("sources array")
        .iter()
        .find(|entry| entry["source_id"] == source_id)
        .unwrap_or_else(|| panic!("source {source_id} missing from report: {value}"))
}

// ---------------------------------------------------------------------------
// Fixture
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

fn rollout_line(i: usize, text: &str) -> String {
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

fn set_mtime(path: &Path, at: SystemTime) {
    let file = fs::OpenOptions::new()
        .write(true)
        .open(path)
        .expect("open for mtime");
    file.set_times(fs::FileTimes::new().set_modified(at))
        .expect("set mtime");
}

/// Deterministic tree fingerprint (FNV-1a over every file's relative path and
/// bytes) used to prove `--plan` writes nothing at all.
fn tree_hash(root: &Path) -> u64 {
    let mut paths: Vec<PathBuf> = Vec::new();
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in fs::read_dir(dir).expect("read_dir") {
            let entry = entry.expect("dir entry");
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
        hash = (hash ^ 0u64).wrapping_mul(0x1000_0000_01b3);
        let bytes = fs::read(&path).expect("read for hash");
        for byte in bytes {
            hash = (hash ^ byte as u64).wrapping_mul(0x1000_0000_01b3);
        }
    }
    hash
}

struct Fixture {
    env: TestEnv,
    archive_root: PathBuf,
    config: PathBuf,
    claude_root: PathBuf,
    codex_root: PathBuf,
    main_jsonl: PathBuf,
}

fn build_fixture(name: &str) -> Fixture {
    let env = TestEnv::new();
    let archive_root = env.root(name);
    Archive::init(&archive_root).expect("init runtime root");

    // Claude Code: main transcript + subagent transcript + tool-results + a
    // non-JSONL file that enumeration must ignore.
    let claude_root = env.root("claude-projects");
    let project = claude_root.join("proj");
    let main_jsonl = project.join("11111111-1111-4111-8111-111111111111.jsonl");
    write_lines(
        &main_jsonl,
        &[
            claude_line("s1", "u1", &format!("{SENTINEL} claude main question")),
            claude_line("s1", "u2", "claude second record"),
        ],
    );
    write_lines(
        &project.join("11111111-1111-4111-8111-111111111111").join("subagents").join("worker.jsonl"),
        &[claude_line("s1-worker", "w1", &format!("{SENTINEL} claude subagent"))],
    );
    write_lines(
        &project.join("tool-results").join("tr-1.jsonl"),
        &[json!({"tool_use_id": "t1", "content": "claude tool result"}).to_string()],
    );
    fs::write(project.join("notes.txt"), "not a transcript").expect("txt fixture");

    // Codex: plain rollout + compressed rollout + a non-rollout jsonl that
    // enumeration must ignore + archived_sessions.
    let codex_root = env.root("codex-home");
    write_lines(
        &codex_root.join("sessions").join("2026").join("10").join("25").join("rollout-a.jsonl"),
        &[
            json!({"timestamp": "2026-10-25T10:00:00Z", "type": "session_meta", "payload": {"id": "cs-a"}}).to_string(),
            rollout_line(1, &format!("{SENTINEL} codex rollout")),
        ],
    );
    write_zst(
        &codex_root.join("sessions").join("2026").join("10").join("24").join("rollout-b.jsonl.zst"),
        "{\"timestamp\":\"2026-10-24T10:00:00Z\",\"type\":\"session_meta\",\"payload\":{\"id\":\"cs-b\"}}\n",
    );
    write_lines(
        &codex_root.join("sessions").join("2026").join("10").join("23").join("history-c.jsonl"),
        &[rollout_line(1, "not a rollout prefix")],
    );
    write_lines(
        &codex_root.join("archived_sessions").join("rollout-old.jsonl"),
        &[json!({"timestamp": "2025-09-21T10:00:00Z", "type": "session_meta", "payload": {"id": "cs-old"}}).to_string()],
    );

    // OpenCode: SQLite V1 fixture + legacy JSON storage (fixed layout).
    let v1 = support::build_v1(&env);
    let oc_root = v1.root.clone();
    let storage = oc_root.join("storage");
    fs::create_dir_all(storage.join("session")).expect("storage dirs");
    fs::create_dir_all(storage.join("message").join("s-main")).expect("storage dirs");
    fs::create_dir_all(storage.join("part").join("m-main")).expect("storage dirs");
    fs::write(
        storage.join("session").join("proj.json"),
        format!("{{\"id\":\"proj\",\"directory\":\"{SENTINEL}\"}}"),
    )
    .expect("storage session");
    fs::write(
        storage.join("message").join("s-main").join("m-main.json"),
        "{\"id\":\"m-main\"}",
    )
    .expect("storage message");
    fs::write(
        storage.join("part").join("m-main").join("p-text.json"),
        "{\"id\":\"p-text\",\"type\":\"text\"}",
    )
    .expect("storage part");

    let config_value = json!({
            "version": 2,
            "sources": [
                {"source_id": "kb", "kind": "kb", "root": env.path().join("unused-vault"), "enabled": true},
                {"source_id": "claude_code", "kind": "claude_code", "root": claude_root, "enabled": true},
                {"source_id": "codex", "kind": "codex", "root": codex_root, "enabled": true},
                {"source_id": "opencode", "kind": "opencode", "root": oc_root, "enabled": true},
                {"source_id": "archive", "kind": "archive", "root": archive_root, "enabled": true}
            ],
            "source_order": ["kb", "claude_code", "codex", "opencode", "archive"],
            "interpreter": {"kb": {"command": "C:/python/python.exe"}},
            "domain": "default",
            "telemetry": {"enabled": false}
        });
    let config = env.write_json(&format!("{name}-recall.json"), &config_value);

    Fixture {
        env,
        archive_root,
        config,
        claude_root,
        codex_root,
        main_jsonl,
    }
}

impl Fixture {
    fn config_arg(&self) -> String {
        self.config.to_string_lossy().to_string()
    }

    fn ledger_path(&self) -> PathBuf {
        self.archive_root.join("state").join("capture-all.json")
    }

    fn manifest_count(&self) -> usize {
        let archive = Archive::open(&self.archive_root).expect("open archive");
        archive.manifests().expect("manifests").len()
    }
}

// ---------------------------------------------------------------------------
// 1. First run captures every enumerated file; report carries metadata only.
// ---------------------------------------------------------------------------

#[test]
fn first_run_captures_every_enumerated_file() {
    let fx = build_fixture("capall-first");
    let run = run_capture_all(&["--config", &fx.config_arg()]);
    assert_eq!(run.code, 0, "stdout: {} stderr: {}", run.stdout, run.stderr);
    let report = report_of(&run);
    assert_eq!(report["ok"], true);
    assert_eq!(report["plan"], false);
    assert_eq!(report["skipped"], Value::Null);
    assert_eq!(report["ledger"], "missing");

    // claude_code: 3 JSONL files (main + subagents + tool-results); notes.txt ignored.
    let claude = source(&report, "claude_code");
    assert_eq!(claude["provider"], "claude");
    assert_eq!(claude["files_total"], 3);
    assert_eq!(claude["files_captured"], 3);
    assert_eq!(claude["files_skipped_unchanged"], 0);
    assert_eq!(claude["errors"], json!([]));

    // codex: 3 rollout files (plain + zst + archived); history-c.jsonl ignored.
    let codex = source(&report, "codex");
    assert_eq!(codex["provider"], "codex");
    assert_eq!(codex["files_total"], 3);
    assert_eq!(codex["files_captured"], 3);

    // opencode: 1 SQLite db + 3 legacy storage files.
    let opencode = source(&report, "opencode");
    assert_eq!(opencode["provider"], "opencode");
    assert_eq!(opencode["files_total"], 4);
    assert_eq!(opencode["files_captured"], 4);

    assert_eq!(report["totals"]["files_captured"], 10);
    assert_eq!(report["totals"]["errors"], 0);

    // The skip ledger exists and the report never carries session bodies.
    assert!(fx.ledger_path().is_file());
    let ledger: Value =
        serde_json::from_str(&fs::read_to_string(fx.ledger_path()).expect("ledger")).expect("ledger json");
    assert_eq!(ledger["version"], 1);
    assert_eq!(ledger["sources"]["claude_code"]["files"].as_object().expect("files").len(), 3);
    assert!(
        !run.stdout.contains(SENTINEL),
        "driver report must not contain session bodies"
    );
    assert!(!run.stderr.contains(SENTINEL));
    // Not a single .jsonl fixture body made it into the report either.
    assert!(!run.stdout.contains("claude main question"));
}

// ---------------------------------------------------------------------------
// 2. Second run skips every unchanged file; SQLite is still observed.
// ---------------------------------------------------------------------------

#[test]
fn second_run_skips_unchanged_files_but_still_observes_sqlite() {
    let fx = build_fixture("capall-second");
    let first = run_capture_all(&["--config", &fx.config_arg()]);
    assert_eq!(first.code, 0, "{} {}", first.stdout, first.stderr);
    let manifests_after_first = fx.manifest_count();

    let second = run_capture_all(&["--config", &fx.config_arg()]);
    assert_eq!(second.code, 0, "{} {}", second.stdout, second.stderr);
    let report = report_of(&second);
    assert_eq!(report["ledger"], "ok");

    let claude = source(&report, "claude_code");
    assert_eq!(claude["files_captured"], 0);
    assert_eq!(claude["files_skipped_unchanged"], 3);
    let codex = source(&report, "codex");
    assert_eq!(codex["files_captured"], 0);
    assert_eq!(codex["files_skipped_unchanged"], 3);
    let opencode = source(&report, "opencode");
    assert_eq!(opencode["files_captured"], 1, "SQLite is observed every run");
    assert_eq!(opencode["files_skipped_unchanged"], 3);

    // Neither the ledger-skipped files nor the unchanged SQLite observation
    // commit a manifest on the second run.
    assert_eq!(
        fx.manifest_count() - manifests_after_first,
        0,
        "an unchanged second run must not commit manifests"
    );
}

// ---------------------------------------------------------------------------
// 3. Touching one file re-captures exactly that file.
// ---------------------------------------------------------------------------

#[test]
fn only_the_changed_file_is_recaptured() {
    let fx = build_fixture("capall-append");
    assert_eq!(run_capture_all(&["--config", &fx.config_arg()]).code, 0);

    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(&fx.main_jsonl)
        .expect("append");
    writeln!(file, "{}", claude_line("s1", "u3", "claude appended record")).expect("append line");
    drop(file);

    let run = run_capture_all(&["--config", &fx.config_arg()]);
    assert_eq!(run.code, 0, "{} {}", run.stdout, run.stderr);
    let report = report_of(&run);
    let claude = source(&report, "claude_code");
    assert_eq!(claude["files_total"], 3);
    assert_eq!(claude["files_captured"], 1);
    assert_eq!(claude["files_skipped_unchanged"], 2);
    assert_eq!(claude["records_added"], 1);
}

// ---------------------------------------------------------------------------
// 4. A new file is picked up on the next run.
// ---------------------------------------------------------------------------

#[test]
fn new_files_are_picked_up() {
    let fx = build_fixture("capall-new");
    assert_eq!(run_capture_all(&["--config", &fx.config_arg()]).code, 0);

    let project = fx.claude_root.join("proj");
    write_lines(
        &project.join("22222222-2222-4222-8222-222222222222.jsonl"),
        &[claude_line("s2", "v1", "claude new session")],
    );

    let run = run_capture_all(&["--config", &fx.config_arg()]);
    assert_eq!(run.code, 0, "{} {}", run.stdout, run.stderr);
    let report = report_of(&run);
    let claude = source(&report, "claude_code");
    assert_eq!(claude["files_total"], 4);
    assert_eq!(claude["files_captured"], 1);
    assert_eq!(claude["files_skipped_unchanged"], 3);
}

// ---------------------------------------------------------------------------
// 5. A corrupt ledger forces a full rescan and is rebuilt.
// ---------------------------------------------------------------------------

#[test]
fn corrupt_ledger_forces_full_rescan() {
    let fx = build_fixture("capall-corrupt");
    assert_eq!(run_capture_all(&["--config", &fx.config_arg()]).code, 0);
    fs::write(fx.ledger_path(), "not json at all").expect("corrupt ledger");

    let run = run_capture_all(&["--config", &fx.config_arg()]);
    assert_eq!(run.code, 0, "{} {}", run.stdout, run.stderr);
    let report = report_of(&run);
    assert_eq!(report["ledger"], "corrupt");
    let claude = source(&report, "claude_code");
    assert_eq!(claude["files_skipped_unchanged"], 0, "corrupt ledger must not skip");
    assert_eq!(claude["files_captured"], 3);

    // The ledger was rebuilt and is valid JSON again.
    let ledger: Value = serde_json::from_str(&fs::read_to_string(fx.ledger_path()).expect("ledger"))
        .expect("rebuilt ledger is valid JSON");
    assert_eq!(ledger["version"], 1);
}

// ---------------------------------------------------------------------------
// 6. One failing file is isolated: reported, other files still captured.
// ---------------------------------------------------------------------------

#[test]
fn single_file_failure_is_isolated() {
    let fx = build_fixture("capall-isolate");
    assert_eq!(run_capture_all(&["--config", &fx.config_arg()]).code, 0);

    // A rollout whose .zst payload is garbage fails decode; nothing else fails.
    let broken = fx
        .codex_root
        .join("sessions")
        .join("2026")
        .join("10")
        .join("25")
        .join("rollout-broken.jsonl.zst");
    fs::write(&broken, b"\x28\xb5\x2f\xfd-not-really-zstd").expect("broken zst");

    let run = run_capture_all(&["--config", &fx.config_arg()]);
    assert_eq!(run.code, 7, "a source error must exit 7");
    let report = report_of(&run);
    assert_eq!(report["ok"], false);
    let codex = source(&report, "codex");
    assert_eq!(codex["files_failed"], 1);
    assert_eq!(codex["files_captured"], 0);
    assert_eq!(codex["files_skipped_unchanged"], 3);
    let errors = codex["errors"].as_array().expect("errors");
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0]["code"], "io");
    assert!(
        errors[0]["path"]
            .as_str()
            .expect("error path")
            .replace('\\', "/")
            .ends_with("rollout-broken.jsonl.zst"),
        "error must name the failing file: {errors:?}"
    );
    // Other sources are untouched by the failure.
    let claude = source(&report, "claude_code");
    assert_eq!(claude["errors"], json!([]));
    assert_eq!(claude["files_skipped_unchanged"], 3);

    // The failed file has no ledger entry, so repairing it retries cleanly.
    write_zst(&broken, "{\"timestamp\":\"2026-10-25T10:00:00Z\"}\n");
    let run = run_capture_all(&["--config", &fx.config_arg()]);
    assert_eq!(run.code, 0, "{} {}", run.stdout, run.stderr);
    assert_eq!(source(&report_of(&run), "codex")["files_captured"], 1);
}

// ---------------------------------------------------------------------------
// 7. Lock handling: hard error by default, skipped=locked with the flag.
// ---------------------------------------------------------------------------

#[test]
fn held_lock_is_reported_and_skip_if_locked_exits_zero() {
    let fx = build_fixture("capall-lock");
    let lock = fx.archive_root.join("locks").join("capture.lock");
    fs::write(&lock, b"pid=0 held").expect("hold the lock");

    let run = run_capture_all(&["--config", &fx.config_arg()]);
    assert_eq!(run.code, 4, "a held lock without --skip-if-locked exits 4");
    assert!(run.stderr.contains("lock"), "stderr: {}", run.stderr);
    assert!(run.stdout.is_empty(), "no report on the hard-error path");

    let run = run_capture_all(&["--config", &fx.config_arg(), "--skip-if-locked"]);
    assert_eq!(run.code, 0);
    let report = report_of(&run);
    assert_eq!(report["ok"], true);
    assert_eq!(report["skipped"], "locked");
    assert_eq!(report["sources"], json!([]));

    fs::remove_file(&lock).expect("release the lock");
    let run = run_capture_all(&["--config", &fx.config_arg(), "--skip-if-locked"]);
    assert_eq!(run.code, 0);
    assert_eq!(report_of(&run)["skipped"], Value::Null);
}

// ---------------------------------------------------------------------------
// 8. --plan enumerates only: the runtime root is byte-identical before/after.
// ---------------------------------------------------------------------------

#[test]
fn plan_writes_nothing_and_reports_planned_bytes() {
    let fx = build_fixture("capall-plan");
    // Populate once so the plan exercises the skip-candidates path too.
    assert_eq!(run_capture_all(&["--config", &fx.config_arg()]).code, 0);
    let before = tree_hash(&fx.archive_root);

    let run = run_capture_all(&["--config", &fx.config_arg(), "--plan"]);
    assert_eq!(run.code, 0, "{} {}", run.stdout, run.stderr);
    let report = report_of(&run);
    assert_eq!(report["plan"], true);
    assert_eq!(report["ok"], true);
    assert_eq!(report["ledger"], "ok", "the plan reports the real ledger state");
    let claude = source(&report, "claude_code");
    assert_eq!(claude["files_total"], 3);
    assert!(claude["bytes_total"].as_u64().expect("bytes_total") > 0);
    assert_eq!(claude["files_captured"], 0);

    assert_eq!(
        tree_hash(&fx.archive_root),
        before,
        "--plan must not write anything (ledger included)"
    );

    // --plan works on a runtime root that was never initialized: it neither
    // opens nor creates the root.
    let bare = fx.env.root("capall-plan-bare-root");
    let run = run_capture_all(&[
        "--config",
        &fx.config_arg(),
        "--root",
        &bare.to_string_lossy(),
        "--plan",
    ]);
    assert_eq!(run.code, 0, "{} {}", run.stdout, run.stderr);
    assert_eq!(report_of(&run)["plan"], true);
    assert_eq!(tree_hash(&bare), tree_hash(&bare), "bare root stays empty");
    assert!(
        fs::read_dir(&bare).expect("bare root").count() == 0,
        "--plan must not initialize the runtime root"
    );
}

// ---------------------------------------------------------------------------
// 9. --session-since filters JSONL by mtime and is passed to the SQLite
//    engine as its session-time filter.
// ---------------------------------------------------------------------------

#[test]
fn session_since_filters_old_files_and_reaches_the_sqlite_engine() {
    let fx = build_fixture("capall-since");
    assert_eq!(run_capture_all(&["--config", &fx.config_arg()]).code, 0);

    let old = SystemTime::UNIX_EPOCH + Duration::from_secs(100);
    set_mtime(&fx.main_jsonl, old);

    let run = run_capture_all(&[
        "--config",
        &fx.config_arg(),
        "--session-since",
        "1970-01-02T00:00:00Z",
    ]);
    assert_eq!(run.code, 0, "{} {}", run.stdout, run.stderr);
    let report = report_of(&run);
    assert_eq!(report["session_since"], "1970-01-02T00:00:00Z");
    let claude = source(&report, "claude_code");
    assert_eq!(claude["files_skipped_since"], 1, "the old-mtime file is skipped");
    assert_eq!(claude["files_captured"], 0, "the two fresh files are ledger-skipped");
    assert_eq!(claude["files_skipped_unchanged"], 2);
    let opencode = source(&report, "opencode");
    assert_eq!(
        opencode["unsupported_option"],
        json!([]),
        "the SQLite engine honours --session-since"
    );
    assert_eq!(opencode["errors"], json!([]));

    let bad = run_capture_all(&[
        "--config",
        &fx.config_arg(),
        "--session-since",
        "not-a-timestamp",
    ]);
    assert_eq!(bad.code, 2);
    assert!(bad.stderr.contains("RFC3339"), "stderr: {}", bad.stderr);
}

// ---------------------------------------------------------------------------
// 10. Runtime-root resolution and --source selection.
// ---------------------------------------------------------------------------

#[test]
fn no_archive_source_and_bad_source_selection_exit_two() {
    let fx = build_fixture("capall-usage");

    // Without an archive source and without --root: usage error.
    let config_value = json!({
            "version": 2,
            "sources": [
                {"source_id": "kb", "kind": "kb", "root": fx.env.path().join("unused-vault"), "enabled": true}
            ],
            "source_order": ["kb"],
            "interpreter": {"kb": {"command": "C:/python/python.exe"}},
            "domain": "default",
            "telemetry": {"enabled": false}
        });
    let config = fx.env.write_json("capall-no-archive.json", &config_value);
    let run = run_capture_all(&["--config", &config.to_string_lossy()]);
    assert_eq!(run.code, 2);
    assert!(
        run.stderr.contains("archive"),
        "the error must hint at the archive source: {}",
        run.stderr
    );

    // Unknown / non-capturable / disabled --source values are usage errors.
    for id in ["missing", "kb", "archive"] {
        let run = run_capture_all(&["--config", &fx.config_arg(), "--source", id]);
        assert_eq!(run.code, 2, "--source {id} must be a usage error");
    }

    // A valid --source restricts the report to that source.
    let run = run_capture_all(&["--config", &fx.config_arg(), "--source", "claude_code"]);
    assert_eq!(run.code, 0, "{} {}", run.stdout, run.stderr);
    let report = report_of(&run);
    let ids: Vec<&str> = report["sources"]
        .as_array()
        .expect("sources")
        .iter()
        .map(|entry| entry["source_id"].as_str().expect("source_id"))
        .collect();
    assert_eq!(ids, vec!["claude_code"]);

    // --root wins over the configured archive source.
    let other = fx.env.root("capall-other-root");
    Archive::init(&other).expect("init other root");
    let run = run_capture_all(&[
        "--config",
        &fx.config_arg(),
        "--root",
        &other.to_string_lossy(),
        "--source",
        "claude_code",
    ]);
    assert_eq!(run.code, 0, "{} {}", run.stdout, run.stderr);
    let report = report_of(&run);
    assert!(
        report["root"]
            .as_str()
            .expect("root")
            .replace('\\', "/")
            .ends_with("capall-other-root"),
        "explicit --root must win: {}",
        report["root"]
    );
    assert!(other.join("state").join("capture-all.json").is_file());
}

// ---------------------------------------------------------------------------
// 11. Ledger pruning: a deleted source file disappears from the ledger.
// ---------------------------------------------------------------------------

#[test]
fn deleted_files_are_pruned_from_the_ledger() {
    let fx = build_fixture("capall-prune");
    assert_eq!(run_capture_all(&["--config", &fx.config_arg()]).code, 0);
    let ledger: Value =
        serde_json::from_str(&fs::read_to_string(fx.ledger_path()).expect("ledger")).expect("json");
    assert_eq!(ledger["sources"]["claude_code"]["files"].as_object().expect("files").len(), 3);

    let project = fx.claude_root.join("proj");
    fs::remove_file(project.join("tool-results").join("tr-1.jsonl")).expect("delete");

    let run = run_capture_all(&["--config", &fx.config_arg()]);
    assert_eq!(run.code, 0, "{} {}", run.stdout, run.stderr);
    let ledger: Value =
        serde_json::from_str(&fs::read_to_string(fx.ledger_path()).expect("ledger")).expect("json");
    let files = ledger["sources"]["claude_code"]["files"].as_object().expect("files");
    assert_eq!(files.len(), 2, "deleted files must leave the ledger: {files:?}");
}
