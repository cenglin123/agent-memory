//! §G.1 / §G.2 regression matrix — capture side.
//!
//! Each scenario mutates one synthetic source, captures it, and then asserts the
//! two capture oracles:
//!
//! * `--verify` (SQLite: baseline + delta against a live scan, zero mismatches);
//! * the **ledger oracle**: `state/records.json` equals the replay of the
//!   committed manifests in capture order (first writer wins, exactly like
//!   `capture_jsonl`'s `or_insert`).
//!
//! The change classification is asserted per run too, so a "skip" that hides a
//! real change is visible.
//!
//! All fixtures are synthetic and live under a per-case temporary root.

mod support;

use std::path::{Path, PathBuf};

use agent_memory::archive::Archive;
use agent_memory::capture::{capture_source, capture_source_with_options, CaptureOptions};
use serde_json::{json, Value};
use support::matrix as m;

const SESSION: &str = "55555555-5555-4555-8555-555555555555";

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

fn append_lines(path: &Path, lines: &[String]) {
    let mut body = lines.join("\n");
    body.push('\n');
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(path)
        .expect("open for append");
    file.write_all(body.as_bytes()).expect("append");
}

struct Capture {
    archive: Archive,
    root: PathBuf,
    path: PathBuf,
}

impl Capture {
    fn new(env: &support::TestEnv, name: &str) -> Capture {
        let root = env.root(&format!("{name}-archive"));
        let archive = Archive::init(&root).expect("init archive");
        let path = env.root(&format!("{name}-source")).join("proj").join("a.jsonl");
        write_lines(
            &path,
            &[
                claude_line("u1", "2026-10-25T13:25:00Z", "user", "capture needle one"),
                claude_line("a1", "2026-10-25T13:26:00Z", "assistant", "capture needle answer"),
            ],
        );
        Capture {
            archive,
            root,
            path,
        }
    }

    fn run(&self) -> Value {
        let report = capture_source_with_options(
            &self.archive,
            &self.path,
            "claude",
            "default",
            None,
            CaptureOptions {
                verify: true,
                ..CaptureOptions::default()
            },
        )
        .expect("capture");
        m::assert_ledger_matches_manifests(&self.root, &format!("{:?}", report.change));
        serde_json::to_value(&report).expect("report")
    }
}

#[test]
fn jsonl_capture_change_classes_and_ledger() {
    let env = support::TestEnv::new();
    let fixture = Capture::new(&env, "capture-jsonl");

    let initial = fixture.run();
    assert_eq!(initial["change"], json!("initial"), "{initial}");

    // Append.
    append_lines(
        &fixture.path,
        &[claude_line(
            "u2",
            "2026-10-25T13:27:00Z",
            "user",
            "capture needle two",
        )],
    );
    let append = fixture.run();
    assert_eq!(append["change"], json!("append"), "{append}");
    assert_eq!(append["recordsNew"], json!(1), "{append}");

    // Mtime only: no content change, no read of the body.
    let at = m::mtime(&fixture.path);
    m::set_mtime(&fixture.path, at + std::time::Duration::from_secs(10));
    let untouched = fixture.run();
    assert_eq!(untouched["change"], json!("skipped_unchanged"), "{untouched}");

    // Same length rewrite.
    let text = std::fs::read_to_string(&fixture.path).expect("read");
    let replaced = text.replace("capture needle answer", "capture needle ANSWER");
    assert_eq!(replaced.len(), text.len());
    std::fs::write(&fixture.path, replaced.as_bytes()).expect("rewrite");
    m::set_mtime(&fixture.path, m::mtime(&fixture.path) + std::time::Duration::from_secs(11));
    let rewrite = fixture.run();
    assert_eq!(rewrite["change"], json!("forced_recapture"), "{rewrite}");
    assert_eq!(
        rewrite["changeReason"],
        json!("same_length_rewrite"),
        "{rewrite}"
    );

    // Prefix rewrite with growth.
    let text = std::fs::read_to_string(&fixture.path).expect("read");
    let replaced = text.replace("capture needle one", "CAPTURE needle one");
    assert_eq!(replaced.len(), text.len());
    let mut grown = replaced;
    grown.push_str(&claude_line(
        "u3",
        "2026-10-25T13:31:00Z",
        "user",
        "capture needle three",
    ));
    grown.push('\n');
    std::fs::write(&fixture.path, grown.as_bytes()).expect("rewrite + grow");
    m::set_mtime(&fixture.path, m::mtime(&fixture.path) + std::time::Duration::from_secs(12));
    let prefix = fixture.run();
    assert_eq!(prefix["change"], json!("forced_recapture"), "{prefix}");
    assert_eq!(prefix["changeReason"], json!("prefix_rewritten"), "{prefix}");

    // Truncation.
    write_lines(
        &fixture.path,
        &[claude_line(
            "u9",
            "2026-10-25T13:40:00Z",
            "user",
            "capture needle truncated",
        )],
    );
    let truncated = fixture.run();
    assert_eq!(truncated["change"], json!("forced_recapture"), "{truncated}");
    assert_eq!(
        truncated["changeReason"],
        json!("length_regression"),
        "{truncated}"
    );

    // A backwards timestamp with growth after a truncation: the capture engine
    // does not use mtime at all (its probes decide), so this is a plain append
    // and the committed bytes stay correct. The index side owns the mtime
    // branch (see `regression_matrix.rs`).
    append_lines(
        &fixture.path,
        &[claude_line(
            "u10",
            "2026-10-25T13:41:00Z",
            "user",
            "capture needle regressed",
        )],
    );
    m::set_mtime(&fixture.path, m::ago(7_200));
    let regressed = fixture.run();
    assert_eq!(regressed["change"], json!("append"), "{regressed}");

    // Removal: the source state, never a ledger entry drop.
    std::fs::remove_file(&fixture.path).expect("delete source");
    let missing = fixture.run();
    assert_eq!(missing["status"], json!("source_missing"), "{missing}");
}

#[test]
fn jsonl_capture_legacy_watermark_without_probes() {
    let env = support::TestEnv::new();
    let fixture = Capture::new(&env, "capture-legacy");
    fixture.run();
    // Strip the probes from the persisted watermark: an upgraded build must not
    // resume on an unverifiable offset.
    let sources = fixture.root.join("state").join("sources.json");
    let mut value: Value =
        serde_json::from_str(&std::fs::read_to_string(&sources).expect("sources")).expect("json");
    for entry in value["sources"]
        .as_object_mut()
        .expect("sources map")
        .values_mut()
    {
        if let Some(watermark) = entry.get_mut("watermark").and_then(Value::as_object_mut) {
            watermark.insert("head_probe".into(), json!(""));
            watermark.insert("anchor_probe".into(), json!(""));
            watermark.insert("probe_len".into(), json!(0));
        }
    }
    std::fs::write(&sources, serde_json::to_vec_pretty(&value).expect("json")).expect("write");
    let report = fixture.run();
    assert_eq!(report["change"], json!("forced_recapture"), "{report}");
    assert_eq!(
        report["changeReason"],
        json!("legacy_watermark_without_probes"),
        "{report}"
    );
}

#[test]
fn jsonl_capture_rename_captures_the_new_path_and_keeps_the_old_state() {
    let env = support::TestEnv::new();
    let mut fixture = Capture::new(&env, "capture-rename");
    fixture.run();
    let moved = fixture.path.with_file_name("b.jsonl");
    std::fs::rename(&fixture.path, &moved).expect("rename");
    fixture.path = moved;
    let renamed = fixture.run();
    assert_eq!(renamed["change"], json!("initial"), "{renamed}");
    m::assert_ledger_matches_manifests(&fixture.root, "renamed source");
    // Both captures are in the ledger, keyed by their own record hashes; the
    // replay oracle above proves nothing was lost or invented.
}

#[test]
fn codex_capture_append_and_ledger() {
    let env = support::TestEnv::new();
    let root = env.root("capture-codex-archive");
    let archive = Archive::init(&root).expect("init");
    let source_root = env.root("capture-codex-source");
    let path = source_root
        .join("sessions")
        .join("2026")
        .join("03")
        .join("06")
        .join(format!("rollout-2026-03-06T21-50-12-{SESSION}.jsonl"));
    write_lines(
        &path,
        &[
            support::codex_meta(SESSION),
            support::codex_user("codex capture needle"),
        ],
    );
    let run = |path: &Path| {
        let report = capture_source(&archive, path, "codex", "default", None).expect("capture");
        assert!(report.ok, "{report:?}");
        m::assert_ledger_matches_manifests(&root, "codex capture");
        serde_json::to_value(report).expect("report")
    };
    let first = run(&path);
    assert_eq!(first["change"], json!("initial"), "{first}");
    append_lines(&path, &[support::codex_assistant("codex capture answer")]);
    let second = run(&path);
    assert_eq!(second["change"], json!("append"), "{second}");
    assert_eq!(second["recordsNew"], json!(1), "{second}");
}

// ---------------------------------------------------------------------------
// OpenCode (synthetic SQLite) with --verify
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

fn opencode_capture(env: &support::TestEnv, name: &str) -> (Archive, PathBuf, PathBuf) {
    let root = env.root(&format!("{name}-archive"));
    let archive = Archive::init(&root).expect("init");
    let source = env.root(&format!("{name}-source"));
    let db = source.join("opencode.db");
    let conn = rusqlite::Connection::open(&db).expect("db");
    conn.execute_batch(OC_SCHEMA).expect("schema");
    conn.execute(
        "INSERT INTO session_v2 (id, title, directory, time_created, time_updated, parent_id) \
         VALUES ('s-1', 'capture session', 'C:/work', 1000, 2000, NULL)",
        [],
    )
    .expect("session");
    for (id, seq, text) in [
        ("sm-1", 1, "opencode capture one"),
        ("sm-2", 2, "opencode capture two"),
        ("sm-3", 3, "opencode capture three"),
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
        .expect("row");
    }
    // Padding rows keep the baseline large relative to a delta, so the
    // designed `delta_chain_threshold` policy does not fire on every small
    // edit (a real corpus has thousands of rows).
    for seq in 10..30 {
        conn.execute(
            "INSERT INTO session_message (id, session_id, seq, type, time_created, data) \
             VALUES (?1, 's-1', ?2, 'user', ?3, ?4)",
            rusqlite::params![
                format!("pad-{seq}"),
                seq,
                1000 + seq,
                json!({"content": [{"type": "text", "text": format!("padding row {seq} {}", "x".repeat(200))}]})
                    .to_string()
            ],
        )
        .expect("padding row");
    }
    drop(conn);
    (archive, root, db)
}

fn capture_verified(archive: &Archive, root: &Path, db: &Path) -> Value {
    let report = capture_source_with_options(
        archive,
        db,
        "opencode",
        "default",
        None,
        CaptureOptions {
            verify: true,
            ..CaptureOptions::default()
        },
    )
    .expect("capture");
    assert!(report.ok, "capture failed: {report:?}");
    let info = report.sqlite.as_ref().expect("sqlite facts");
    // The verification pass runs on incremental units (and on a due round), not
    // on the initial baseline.
    if report.change == "delta" {
        assert!(info.verify, "the verification pass ran: {report:?}");
        let changed = info.rows_inserted + info.rows_updated + info.rows_deleted;
        assert!(
            info.verify_mismatches <= changed,
            "mismatches stay bounded by the delta: {report:?}"
        );
    }
    m::assert_ledger_matches_manifests(root, "opencode capture");
    // A verification mismatch is self-healing: the pass commits a correction
    // delta, so the next pass over the same state must find nothing. (The
    // current merge reports one mismatch when a row moves in the table's
    // watermark order; the correction is idempotent.)
    if info.verify_mismatches > 0 {
        let settle = capture_source_with_options(
            archive,
            db,
            "opencode",
            "default",
            None,
            CaptureOptions {
                verify: true,
                ..CaptureOptions::default()
            },
        )
        .expect("settling capture");
        assert!(
            settle.sqlite.as_ref().map(|sqlite| sqlite.verify_mismatches) == Some(0),
            "the correction converges: {settle:?}"
        );
        m::assert_ledger_matches_manifests(root, "opencode capture (settled)");
    }
    serde_json::to_value(report).expect("report")
}

#[test]
fn opencode_capture_scenarios_verify_and_ledger() {
    let env = support::TestEnv::new();
    let (archive, root, db) = opencode_capture(&env, "capture-opencode");
    let initial = capture_verified(&archive, &root, &db);
    assert_eq!(initial["change"], json!("initial"), "{initial}");

    // 1. new row.
    {
        let conn = rusqlite::Connection::open(&db).expect("db");
        conn.execute(
            "INSERT INTO session_message (id, session_id, seq, type, time_created, data) \
             VALUES ('sm-4', 's-1', 4, 'assistant', 2000, ?1)",
            rusqlite::params![json!({"content": [{"type": "text", "text": "opencode capture four"}]})
                .to_string()],
        )
        .expect("new row");
    }
    let new_row = capture_verified(&archive, &root, &db);
    assert_eq!(new_row["change"], json!("delta"), "{new_row}");
    assert_eq!(new_row["sqlite"]["rowsInserted"], json!(1), "{new_row}");

    // 2. in-place unit text update (timestamp bumped: the delta path sees it).
    {
        let conn = rusqlite::Connection::open(&db).expect("db");
        conn.execute(
            "UPDATE session_message SET data = ?1, time_created = 3000 WHERE id = 'sm-2'",
            rusqlite::params![json!({"content": [{"type": "text", "text": "opencode capture EDITED"}]})
                .to_string()],
        )
        .expect("edit");
    }
    let edited = capture_verified(&archive, &root, &db);
    assert_eq!(edited["change"], json!("delta"), "{edited}");
    assert_eq!(edited["sqlite"]["rowsUpdated"], json!(1), "{edited}");

    // 3. session title change.
    {
        let conn = rusqlite::Connection::open(&db).expect("db");
        conn.execute(
            "UPDATE session_v2 SET title = 'renamed capture', time_updated = 3000 WHERE id = 's-1'",
            [],
        )
        .expect("rename");
    }
    let renamed = capture_verified(&archive, &root, &db);
    assert_eq!(renamed["sqlite"]["rowsUpdated"], json!(1), "{renamed}");

    // 4. delete a child row.
    {
        let conn = rusqlite::Connection::open(&db).expect("db");
        conn.execute("DELETE FROM session_message WHERE id = 'sm-3'", [])
            .expect("delete row");
    }
    let deleted = capture_verified(&archive, &root, &db);
    assert_eq!(deleted["sqlite"]["rowsDeleted"], json!(1), "{deleted}");

    // 5. rowid reuse: delete and re-insert with an explicit rowid.
    {
        let conn = rusqlite::Connection::open(&db).expect("db");
        conn.execute("DELETE FROM session_message WHERE id = 'sm-4'", [])
            .expect("delete");
        // Free rowid 4 (the first padding row) and reuse it explicitly.
        conn.execute("DELETE FROM session_message WHERE id = 'pad-10'", [])
            .expect("free rowid 4");
        conn.execute(
            "INSERT INTO session_message (rowid, id, session_id, seq, type, time_created, data) \
             VALUES (4, 'sm-5', 's-1', 5, 'user', 4000, ?1)",
            rusqlite::params![json!({"content": [{"type": "text", "text": "opencode capture reused"}]})
                .to_string()],
        )
        .expect("reuse rowid");
    }
    let reused = capture_verified(&archive, &root, &db);
    assert_eq!(reused["change"], json!("delta"), "{reused}");

    // 6. VACUUM: rewrites the file, no logical change.
    {
        let conn = rusqlite::Connection::open(&db).expect("db");
        conn.execute_batch("VACUUM;").expect("vacuum");
    }
    let vacuumed = capture_verified(&archive, &root, &db);
    assert_eq!(vacuumed["change"], json!("skipped_unchanged"), "{vacuumed}");

    // 7a. A write with identical content (the row is rewritten, nothing
    //     changes): the observation sees no change and the verification pass
    //     agrees.
    {
        let conn = rusqlite::Connection::open(&db).expect("db");
        conn.execute("UPDATE session_message SET seq = seq WHERE id = 'sm-1'", [])
            .expect("content-free touch");
    }
    let touched = capture_verified(&archive, &root, &db);
    assert_eq!(touched["change"], json!("skipped_unchanged"), "{touched}");

    // 7b. A content change the watermark cannot see (a non-watermark column):
    //     the verification pass catches it and commits a correction, and the
    //     next pass is clean again (the settle inside `capture_verified`).
    {
        let conn = rusqlite::Connection::open(&db).expect("db");
        conn.execute(
            "UPDATE session_message SET time_created = 5100 WHERE id = 'sm-1'",
            [],
        )
        .expect("content-only touch");
    }
    let corrected = capture_verified(&archive, &root, &db);
    assert_eq!(corrected["change"], json!("delta"), "{corrected}");
    assert!(
        corrected["sqlite"]["verifyMismatches"].as_u64().unwrap_or(0) >= 1,
        "the verification pass names the content-only change: {corrected}"
    );

    // 8. delete the session (session row + its child rows). A tiny synthetic
    //    baseline makes the delta chain cross its byte threshold here, so the
    //    run may legitimately re-baseline instead of writing a delta; both are
    //    correct as long as the deleted session is gone from the evidence.
    {
        let conn = rusqlite::Connection::open(&db).expect("db");
        conn.execute("DELETE FROM session_message WHERE session_id = 's-1'", [])
            .expect("delete rows");
        conn.execute("DELETE FROM session_v2 WHERE id = 's-1'", [])
            .expect("delete session");
    }
    let gone = capture_verified(&archive, &root, &db);
    assert!(
        matches!(gone["change"].as_str(), Some("delta") | Some("baseline")),
        "{gone}"
    );
    if gone["change"] == json!("delta") {
        assert!(
            gone["sqlite"]["rowsDeleted"].as_u64().unwrap_or(0) >= 2,
            "{gone}"
        );
    } else {
        // Read the committed baseline back and prove the session is absent.
        let baselines = agent_memory::capture_sqlite::sqlite_baselines(
            &archive,
            gone["source"].as_str().expect("source"),
        )
        .expect("baselines");
        let baseline = baselines.last().expect("a committed baseline");
        let rows = agent_memory::capture_sqlite::read_baseline_rows(
            &archive,
            baseline,
            "default",
        )
        .expect("baseline rows");
        assert!(
            !rows
                .get("session_v2")
                .map(|table| table.contains_key("s-1"))
                .unwrap_or(false),
            "the deleted session is absent from the new baseline"
        );
    }
}

#[test]
fn capture_cli_runs_verify_and_reports_the_ledger_oracle() {
    // The CLI surface: `capture --verify` reaches the engine option.
    let env = support::TestEnv::new();
    let (archive, root, db) = opencode_capture(&env, "capture-cli");
    drop(archive);
    let run = m::run(&[
        "capture",
        "--root",
        &root.to_string_lossy(),
        "--source",
        &db.to_string_lossy(),
        "--provider",
        "opencode",
        "--domain",
        "default",
        "--verify",
    ]);
    assert_eq!(run.code, 0, "{} {}", run.stdout, run.stderr);
    // The first capture is the baseline unit: the verification pass belongs to
    // incremental units. The option reached the engine either way, and the
    // report says which unit ran it.
    assert!(run.report["sqlite"]["verify"].is_boolean(), "{}", run.stdout);
    assert_eq!(run.report["sqlite"]["verifyMismatches"], json!(0));
    // A second capture with `--verify` runs the delta path and the verification.
    let second = m::run(&[
        "capture",
        "--root",
        &root.to_string_lossy(),
        "--source",
        &db.to_string_lossy(),
        "--provider",
        "opencode",
        "--domain",
        "default",
        "--verify",
    ]);
    assert_eq!(second.code, 0, "{} {}", second.stdout, second.stderr);
    assert_eq!(second.report["sqlite"]["verify"], true, "{}", second.stdout);
    assert_eq!(second.report["sqlite"]["verifyMismatches"], json!(0));
    m::assert_ledger_matches_manifests(&root, "capture --verify");
}
