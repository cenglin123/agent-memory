//! Stage 6 implementation-001: `learning anomalies` (plan §A) and the
//! `[self-reflect]` watermark.
//!
//! Acceptance coverage: C1 (every signal kind, positive and negative), C2
//! (watermark inclusion rules, filtering, resurfacing, tie-break), C3
//! (read-only four-piece, JSON schema, text golden, deterministic order).
//!
//! Every case builds a synthetic learning root under `tempfile`; no real
//! learning root is read. Event times are rewritten with direct SQL on the
//! synthetic store (the stage-6 fixture convention: only the test fixture, never
//! a production store, is edited this way).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use chrono::{DateTime, Duration, FixedOffset, SecondsFormat};
use rusqlite::{Connection, OpenFlags};
use serde_json::{json, Value};

use agent_memory::learning::{
    claim_codepoint_count, marker_classes, LearningStore, ANOMALY_CREATE_BURST,
    ANOMALY_LEGACY_INVALID, ANOMALY_LONG_CLAIM, ANOMALY_MARKER_HIT, ANOMALY_MULTI_ACTOR,
    ANOMALY_NEVER_REFLECTED, ANOMALY_NEW_ACTOR, ANOMALY_REJECT_CLEAR_FLIP,
    CREATE_BURST_MIN_EVENTS, LONG_CLAIM_CODEPOINTS, MULTI_ACTOR_MIN_ACTORS,
};

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

const BASE: &str = "2026-01-01T00:00:00.000Z";

fn cli() -> Command {
    Command::new(env!("CARGO_BIN_EXE_agent-memory"))
}

fn run(args: &[&str]) -> Output {
    cli().args(args).output().expect("run agent-memory")
}

fn stdout_json(out: &Output) -> Value {
    serde_json::from_slice(&out.stdout).unwrap_or_else(|error| {
        panic!(
            "stdout is not JSON ({error}): {}",
            String::from_utf8_lossy(&out.stdout)
        )
    })
}

fn stderr_json(out: &Output) -> Value {
    serde_json::from_slice(&out.stderr).unwrap_or_else(|error| {
        panic!(
            "stderr is not JSON ({error}): {}",
            String::from_utf8_lossy(&out.stderr)
        )
    })
}

fn assert_ok(out: &Output, context: &str) -> Value {
    assert_eq!(
        out.status.code(),
        Some(0),
        "{context}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    stdout_json(out)
}

fn new_root() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("temp dir");
    let root = dir.path().join("learning");
    drop(LearningStore::init(&root).expect("init learning root"));
    (dir, root)
}

fn text(root: &Path) -> String {
    root.to_string_lossy().to_string()
}

fn connect(root: &Path) -> Connection {
    Connection::open(root.join("learning.db")).expect("open learning.db")
}

fn at(value: &str) -> DateTime<FixedOffset> {
    DateTime::parse_from_rfc3339(value).expect("timestamp")
}

fn plus_minutes(base: &str, minutes: i64) -> String {
    (at(base) + Duration::minutes(minutes)).to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn plus_days(base: &str, days: i64) -> String {
    (at(base) + Duration::days(days)).to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn plus_millis(stamp: &str, millis: i64) -> String {
    (at(stamp) + Duration::milliseconds(millis)).to_rfc3339_opts(SecondsFormat::Millis, true)
}

// ---------------------------------------------------------------------------
// Command wrappers and fixture helpers
// ---------------------------------------------------------------------------

fn create_task(root: &Path, claim: &str, actor: &str) -> String {
    let root_text = text(root);
    assert_ok(
        &run(&[
            "learning", "task", "create", "--root", &root_text, "--claim", claim, "--actor", actor,
        ]),
        "task create",
    )["candidate_id"]
        .as_str()
        .expect("candidate id")
        .to_string()
}

fn claim_task(root: &Path, candidate_id: &str, actor: &str) {
    let root_text = text(root);
    assert_ok(
        &run(&[
            "learning",
            "task",
            "claim",
            "--root",
            &root_text,
            "--candidate-id",
            candidate_id,
            "--actor",
            actor,
        ]),
        "task claim",
    );
}

fn abandon_task(root: &Path, candidate_id: &str, actor: &str) {
    let root_text = text(root);
    assert_ok(
        &run(&[
            "learning",
            "task",
            "abandon",
            "--root",
            &root_text,
            "--candidate-id",
            candidate_id,
            "--actor",
            actor,
            "--reason",
            "scope moved",
        ]),
        "task abandon",
    );
}

/// An existing absolute report path for a validation fixture (stage 6 §C
/// requires one for a `[self-reflect]` task, and accepts it for any task).
fn report_path(root: &Path) -> String {
    let path = root.join("reflect-report.md");
    if !path.exists() {
        std::fs::write(&path, "# synthetic audit report\n").expect("write report fixture");
    }
    path.to_string_lossy().to_string()
}

fn validate(root: &Path, candidate_id: &str, reviewer: &str, verdict: &str) {
    let root_text = text(root);
    let report = report_path(root);
    assert_ok(
        &run(&[
            "learning",
            "validate",
            "--root",
            &root_text,
            "--candidate-id",
            candidate_id,
            "--reviewer",
            reviewer,
            "--verdict",
            verdict,
            "--report-ref",
            &report,
        ]),
        "validate",
    );
}

/// Insert a validation row directly. Used only where the write path now refuses
/// the row (a `[self-reflect]` creator review, stage 6 §C) but the read-side
/// projection still has to be exercised.
fn write_validation_row(root: &Path, candidate_id: &str, reviewer: &str, verdict: &str) {
    let conn = connect(root);
    let event_seq: i64 = conn
        .query_row(
            "SELECT COALESCE(MAX(seq), 0) + 1 FROM event_clock",
            [],
            |row| row.get(0),
        )
        .expect("next clock seq");
    conn.execute(
        "INSERT INTO event_clock (seq) VALUES (?1)",
        rusqlite::params![event_seq],
    )
    .expect("clock row");
    conn.execute(
        "INSERT INTO validation_history \
         (event_seq, candidate_id, reviewer, verdict, report_ref, recorded_at) \
         VALUES (?1, ?2, ?3, ?4, 'report.md', '2026-01-01T00:00:00.000Z')",
        rusqlite::params![event_seq, candidate_id, reviewer, verdict],
    )
    .expect("insert validation row");
}

fn reuse_task(root: &Path, candidate_id: &str, actor: &str) {
    let root_text = text(root);
    assert_ok(
        &run(&[
            "learning",
            "reuse",
            "--root",
            &root_text,
            "--candidate-id",
            candidate_id,
            "--task",
            candidate_id,
            "--outcome",
            "pass",
            "--actor",
            actor,
        ]),
        "task reuse",
    );
}

fn anomalies(root: &Path, extra: &[&str]) -> Output {
    let root_text = text(root);
    let mut args = vec!["learning", "anomalies", "--root", &root_text];
    args.extend_from_slice(extra);
    run(&args)
}

fn anomalies_json(root: &Path, extra: &[&str]) -> Value {
    let mut with_json = vec!["--json"];
    with_json.extend_from_slice(extra);
    assert_ok(&anomalies(root, &with_json), "anomalies --json")
}

fn anomalies_text(root: &Path, extra: &[&str]) -> String {
    let out = anomalies(root, extra);
    assert_eq!(
        out.status.code(),
        Some(0),
        "anomalies: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("stdout is UTF-8")
}

fn signals_of<'a>(report: &'a Value, kind: &str) -> Vec<&'a Value> {
    report["signals"]
        .as_array()
        .expect("signals")
        .iter()
        .filter(|signal| signal["kind"] == json!(kind))
        .collect()
}

fn ids_of(report: &Value, kind: &str) -> Vec<String> {
    let mut ids: Vec<String> = signals_of(report, kind)
        .iter()
        .map(|signal| {
            signal["candidate_id"]
                .as_str()
                .expect("candidate id")
                .to_string()
        })
        .collect();
    ids.sort();
    ids
}

fn create_event_seq(root: &Path, candidate_id: &str) -> i64 {
    connect(root)
        .query_row(
            "SELECT MIN(event_seq) FROM task_events WHERE candidate_id = ?1 AND event_kind = 'claim'",
            [candidate_id],
            |row| row.get(0),
        )
        .expect("create event seq")
}

fn last_event_seq(root: &Path, table: &str) -> i64 {
    connect(root)
        .query_row(&format!("SELECT MAX(event_seq) FROM {table}"), [], |row| {
            row.get(0)
        })
        .expect("max event seq")
}

fn set_recorded_at(root: &Path, table: &str, event_seq: i64, stamp: &str) {
    connect(root)
        .execute(
            &format!("UPDATE {table} SET recorded_at = ?1 WHERE event_seq = ?2"),
            rusqlite::params![stamp, event_seq],
        )
        .expect("rewrite recorded_at");
}

fn set_create_time(root: &Path, candidate_id: &str, stamp: &str) {
    let seq = create_event_seq(root, candidate_id);
    set_recorded_at(root, "task_events", seq, stamp);
}

fn set_last_validation_time(root: &Path, stamp: &str) {
    let seq = last_event_seq(root, "validation_history");
    set_recorded_at(root, "validation_history", seq, stamp);
}

fn set_last_reuse_time(root: &Path, stamp: &str) {
    let seq = last_event_seq(root, "reuse_history");
    set_recorded_at(root, "reuse_history", seq, stamp);
}

fn rewrite_claim(root: &Path, candidate_id: &str, claim: &str) {
    let conn = connect(root);
    let record_json: String = conn
        .query_row(
            "SELECT record_json FROM candidates WHERE candidate_id = ?1",
            [candidate_id],
            |row| row.get(0),
        )
        .expect("record_json");
    let mut record: Value = serde_json::from_str(&record_json).expect("parse record");
    record["claim"] = Value::String(claim.to_string());
    conn.execute(
        "UPDATE candidates SET record_json = ?1 WHERE candidate_id = ?2",
        [record.to_string(), candidate_id.to_string()],
    )
    .expect("rewrite record");
}

fn rewrite_actor(root: &Path, candidate_id: &str, actor: &str) {
    connect(root)
        .execute(
            "UPDATE task_events SET actor_id = ?1 WHERE candidate_id = ?2",
            [actor, candidate_id],
        )
        .expect("rewrite actor");
}

// ===========================================================================
// C1 — long_claim
// ===========================================================================

#[test]
fn c1_long_claim_boundary_399_400_for_ascii_cjk_and_emoji() {
    let (_dir, root) = new_root();
    let groups: [(&str, &str); 3] = [("ascii", "a"), ("cjk", "任"), ("emoji", "🚀")];
    let mut expected: Vec<String> = Vec::new();
    let mut below: Vec<String> = Vec::new();
    for (label, unit) in groups {
        let short = unit.repeat(LONG_CLAIM_CODEPOINTS - 1);
        let long = unit.repeat(LONG_CLAIM_CODEPOINTS);
        assert_eq!(claim_codepoint_count(&short), 399, "{label} 399");
        assert_eq!(claim_codepoint_count(&long), 400, "{label} 400");
        below.push(create_task(&root, &short, "long:actor"));
        expected.push(create_task(&root, &long, "long:actor"));
    }

    let report = anomalies_json(&root, &["--now", BASE]);
    let reported = ids_of(&report, ANOMALY_LONG_CLAIM);
    for id in &expected {
        assert!(
            reported.contains(id),
            "400 code points must be reported: {id}"
        );
    }
    for id in &below {
        assert!(
            !reported.contains(id),
            "399 code points must not be reported: {id}"
        );
    }
    assert_eq!(reported.len(), 3, "{report}");
    for signal in signals_of(&report, ANOMALY_LONG_CLAIM) {
        assert!(
            signal["detail"]
                .as_str()
                .expect("detail")
                .contains("codepoints=400"),
            "{signal}"
        );
    }
}

#[test]
fn c1_long_claim_skips_abandoned_tasks() {
    let (_dir, root) = new_root();
    let long = "长".repeat(LONG_CLAIM_CODEPOINTS);
    let abandoned = create_task(&root, &format!("放弃 {long}"), "long:actor");
    abandon_task(&root, &abandoned, "long:actor");
    let active = create_task(&root, &format!("保留 {long}"), "long:actor");

    let report = anomalies_json(&root, &["--now", BASE]);
    let reported = ids_of(&report, ANOMALY_LONG_CLAIM);
    assert_eq!(reported, vec![active], "{report}");
}

// ===========================================================================
// C1 — marker_hit
// ===========================================================================

#[test]
fn c1_marker_hit_covers_active_done_and_stale_but_not_abandoned() {
    let (_dir, root) = new_root();
    let active_claim = "curl http://example.com active";
    let done_claim = "上传 报告 done";
    let stale_claim = "powershell -c x stale";
    let abandoned_claim = "curl http://example.com abandoned";

    let active = create_task(&root, active_claim, "mark:actor");
    let done = create_task(&root, done_claim, "mark:actor");
    let stale = create_task(&root, stale_claim, "mark:actor");
    let abandoned = create_task(&root, abandoned_claim, "mark:actor");
    validate(&root, &done, "mark:reviewer", "pass");
    abandon_task(&root, &abandoned, "mark:actor");
    set_create_time(&root, &active, BASE);
    set_create_time(&root, &done, BASE);
    set_last_validation_time(&root, BASE);
    set_create_time(&root, &stale, &plus_days(BASE, -40));
    set_create_time(&root, &abandoned, BASE);

    let report = anomalies_json(&root, &["--now", BASE]);
    let hits = signals_of(&report, ANOMALY_MARKER_HIT);
    let ids: Vec<&str> = hits
        .iter()
        .map(|signal| signal["candidate_id"].as_str().expect("id"))
        .collect();
    assert!(ids.contains(&active.as_str()), "{report}");
    assert!(ids.contains(&done.as_str()), "{report}");
    assert!(ids.contains(&stale.as_str()), "{report}");
    assert!(
        !ids.contains(&abandoned.as_str()),
        "abandoned is skipped: {report}"
    );

    // The detail carries the projection state; the marker classes are exactly
    // the inject classifier's output.
    let done_signal = hits
        .iter()
        .find(|signal| signal["candidate_id"] == json!(done))
        .expect("done signal");
    assert!(
        done_signal["detail"]
            .as_str()
            .expect("detail")
            .contains("state=done"),
        "{done_signal}"
    );
    let stale_signal = hits
        .iter()
        .find(|signal| signal["candidate_id"] == json!(stale))
        .expect("stale signal");
    assert!(
        stale_signal["detail"]
            .as_str()
            .expect("detail")
            .contains("state=stale"),
        "{stale_signal}"
    );
    for (claim, signal) in [
        (
            active_claim,
            hits.iter().find(|s| s["candidate_id"] == json!(active)),
        ),
        (done_claim, Some(&done_signal)),
        (stale_claim, Some(&stale_signal)),
    ] {
        let classes = marker_classes(claim);
        assert!(!classes.is_empty());
        let detail = signal.expect("signal")["detail"].as_str().expect("detail");
        assert!(
            detail.contains(&format!("markers={}", classes.join(","))),
            "detail {detail:?} must carry the inject classifier classes {classes:?}"
        );
    }

    // Same judgement as the inject section for the tasks inject renders.
    let inject = assert_ok(
        &run(&[
            "learning",
            "inject",
            "--root",
            &text(&root),
            "--json",
            "--now",
            BASE,
        ]),
        "inject",
    );
    for section in inject["sections"].as_array().expect("sections") {
        let id = section["candidate_id"].as_str().expect("id");
        assert!(
            ids.contains(&id),
            "an inject section with markers must be a marker_hit: {section}"
        );
    }
}

// ===========================================================================
// C1 — create_burst
// ===========================================================================

#[test]
fn c1_create_burst_four_vs_five_boundary_outside_and_same_timestamp() {
    let (_dir, root) = new_root();

    // Four creates never burst.
    for index in 0..4 {
        let id = create_task(&root, &format!("four {index}"), "burst:four");
        set_create_time(&root, &id, &plus_minutes(BASE, index as i64));
    }

    // Five creates, the fifth just inside the 60-minute window (one
    // millisecond short of the boundary): the first create is still in
    // `(t - 60m, t]`.
    for index in 0..5 {
        let id = create_task(&root, &format!("inside {index}"), "burst:inside");
        let stamp = if index < 4 {
            plus_minutes(BASE, index as i64)
        } else {
            plus_millis(&plus_minutes(BASE, 60), -1)
        };
        set_create_time(&root, &id, &stamp);
    }

    // Five creates with the fifth exactly on the boundary: the window is
    // half-open, so `t - 60m` itself is excluded and this is not a burst.
    for index in 0..5 {
        let id = create_task(&root, &format!("boundary {index}"), "burst:boundary");
        let stamp = if index < 4 {
            plus_minutes(BASE, index as i64)
        } else {
            plus_minutes(BASE, 60)
        };
        set_create_time(&root, &id, &stamp);
    }

    // Five creates, the fifth one millisecond outside the window.
    for index in 0..5 {
        let id = create_task(&root, &format!("outside {index}"), "burst:outside");
        let stamp = if index < 4 {
            plus_minutes(BASE, index as i64)
        } else {
            plus_millis(&plus_minutes(BASE, 60), 1)
        };
        set_create_time(&root, &id, &stamp);
    }

    // Five creates at the very same `recorded_at`: the tie is ordered by
    // `event_seq` and all five are inside the window.
    let mut same: Vec<String> = Vec::new();
    for index in 0..5 {
        let id = create_task(&root, &format!("same {index}"), "burst:same");
        set_create_time(&root, &id, BASE);
        same.push(id);
    }

    // Ten creates in one window re-anchor at the 10th ("every full five").
    for index in 0..10 {
        let id = create_task(&root, &format!("ten {index}"), "burst:ten");
        set_create_time(&root, &id, &plus_minutes(BASE, index as i64));
    }

    let report = anomalies_json(&root, &["--now", BASE]);
    let bursts = signals_of(&report, ANOMALY_CREATE_BURST);
    let actors: Vec<&str> = bursts
        .iter()
        .map(|signal| signal["actor"].as_str().expect("actor"))
        .collect();
    assert!(
        !actors.contains(&"burst:four"),
        "four creates never burst: {report}"
    );
    assert!(
        !actors.contains(&"burst:boundary"),
        "the half-open window excludes the exact boundary: {report}"
    );
    assert!(
        actors.contains(&"burst:inside"),
        "one millisecond inside the window bursts: {report}"
    );
    assert!(
        !actors.contains(&"burst:outside"),
        "one millisecond outside is not inside the window: {report}"
    );
    assert!(
        actors.contains(&"burst:same"),
        "the same timestamp is inside the window: {report}"
    );
    assert_eq!(
        bursts
            .iter()
            .filter(|signal| signal["actor"] == json!("burst:ten"))
            .count(),
        2,
        "ten creates in one window re-anchor at 5 and 10: {report}"
    );
    assert_eq!(
        bursts
            .iter()
            .filter(|signal| signal["actor"] == json!("burst:inside"))
            .count(),
        1
    );
    let same_signal = bursts
        .iter()
        .find(|signal| signal["actor"] == json!("burst:same"))
        .expect("same-time burst");
    // The anchor is the fifth create event (`event_seq` tie-break), which is
    // the highest create seq of the five.
    let same_create_seqs: Vec<i64> = same.iter().map(|id| create_event_seq(&root, id)).collect();
    let max_seq = *same_create_seqs.iter().max().expect("max");
    assert_eq!(same_signal["anchor_seq"], json!(max_seq));
    assert_eq!(same_signal["signal_seq"], json!(max_seq));
    assert_eq!(
        same_signal["detail"],
        json!(format!(
            "creates={} window_minutes=60",
            CREATE_BURST_MIN_EVENTS
        ))
    );
}

// ===========================================================================
// C1 — multi_actor
// ===========================================================================

#[test]
fn c1_multi_actor_needs_three_and_never_counts_a_reviewer() {
    let (_dir, root) = new_root();
    let task = create_task(&root, "多人任务", "multi:a");
    claim_task(&root, &task, "multi:b");
    set_create_time(&root, &task, BASE);
    let claim_seq = last_event_seq(&root, "task_events");
    set_recorded_at(&root, "task_events", claim_seq, BASE);

    validate(&root, &task, "multi:reviewer", "pass");
    set_last_validation_time(&root, BASE);

    let two = anomalies_json(&root, &["--now", BASE]);
    assert!(
        signals_of(&two, ANOMALY_MULTI_ACTOR).is_empty(),
        "a reviewer is not an actor: {two}"
    );

    reuse_task(&root, &task, "multi:c");
    set_last_reuse_time(&root, BASE);
    let reuse_seq = last_event_seq(&root, "reuse_history");

    let three = anomalies_json(&root, &["--now", BASE]);
    let signals = signals_of(&three, ANOMALY_MULTI_ACTOR);
    assert_eq!(signals.len(), 1, "{three}");
    assert_eq!(signals[0]["candidate_id"], json!(task));
    assert_eq!(signals[0]["actor"], json!("multi:c"));
    assert_eq!(signals[0]["anchor_seq"], json!(reuse_seq));
    assert_eq!(
        signals[0]["detail"],
        json!(format!("actors={} window_hours=24", MULTI_ACTOR_MIN_ACTORS))
    );
}

// ===========================================================================
// C1 — new_actor
// ===========================================================================

#[test]
fn c1_new_actor_covers_create_claim_abandon_and_reuse_and_skips_reviewer() {
    let (_dir, root) = new_root();
    let base = create_task(&root, "base task", "na:base");
    create_task(&root, "created by the actor", "na:create");

    let claimed = create_task(&root, "claimed by the actor", "na:base");
    claim_task(&root, &claimed, "na:claim");

    let abandoned = create_task(&root, "abandoned by the actor", "na:base");
    abandon_task(&root, &abandoned, "na:abandon");

    let reused = create_task(&root, "reused by the actor", "na:base");
    reuse_task(&root, &reused, "na:reuse");

    validate(&root, &base, "na:reviewer", "pass");

    let report = anomalies_json(&root, &["--now", BASE]);
    let signals = signals_of(&report, ANOMALY_NEW_ACTOR);
    let mut by_actor: BTreeMap<String, String> = BTreeMap::new();
    for signal in &signals {
        let actor = signal["actor"].as_str().expect("actor").to_string();
        let detail = signal["detail"].as_str().expect("detail").to_string();
        assert!(
            by_actor.insert(actor.clone(), detail).is_none(),
            "one new_actor signal per actor: {actor}"
        );
    }
    assert_eq!(
        by_actor.get("na:create").map(String::as_str),
        Some("first_seen=create")
    );
    assert_eq!(
        by_actor.get("na:claim").map(String::as_str),
        Some("first_seen=claim")
    );
    assert_eq!(
        by_actor.get("na:abandon").map(String::as_str),
        Some("first_seen=abandon")
    );
    assert_eq!(
        by_actor.get("na:reuse").map(String::as_str),
        Some("first_seen=reuse")
    );
    assert_eq!(
        by_actor.get("na:base").map(String::as_str),
        Some("first_seen=create")
    );
    assert!(
        !by_actor.contains_key("na:reviewer"),
        "validation reviewers are never actors: {report}"
    );
    assert_eq!(signals.len(), 5, "{report}");
}

// ===========================================================================
// C1 — legacy_invalid
// ===========================================================================

#[test]
fn c1_legacy_invalid_matches_the_inject_legacy_gate() {
    let (_dir, root) = new_root();
    let bad_claim = create_task(&root, "原本合法的任务甲", "legacy:actor");
    let bad_actor = create_task(&root, "原本合法的任务乙", "legacy:actor");
    let done_bad = create_task(&root, "原本合法的任务丙", "legacy:actor");
    rewrite_claim(&root, &bad_claim, "两行\nclaim");
    rewrite_actor(&root, &bad_actor, "bad actor");
    rewrite_claim(&root, &done_bad, "已完成\n坏 claim");
    validate(&root, &done_bad, "legacy:reviewer", "pass");

    let check = assert_ok(
        &run(&[
            "learning",
            "inject",
            "--root",
            &text(&root),
            "--json",
            "--check",
            "--now",
            BASE,
        ]),
        "inject --check",
    );
    let mut inject_reasons: BTreeMap<String, String> = BTreeMap::new();
    for violation in check["violations"].as_array().expect("violations") {
        inject_reasons.insert(
            violation["candidate_id"].as_str().expect("id").to_string(),
            violation["reason"].as_str().expect("reason").to_string(),
        );
    }
    assert_eq!(inject_reasons.len(), 2, "{check}");

    let report = anomalies_json(&root, &["--now", BASE]);
    let signals = signals_of(&report, ANOMALY_LEGACY_INVALID);
    assert_eq!(signals.len(), 3, "{report}");
    for signal in &signals {
        let id = signal["candidate_id"].as_str().expect("id").to_string();
        let detail = signal["detail"].as_str().expect("detail");
        if let Some(reason) = inject_reasons.get(&id) {
            assert_eq!(
                detail, reason,
                "the shared predicate must produce the inject reason verbatim"
            );
        } else {
            assert_eq!(
                id, done_bad,
                "only the done task is outside the inject active/stale scope"
            );
            assert!(detail.starts_with("CLAIM_INVALID_TEXT: "), "{detail}");
        }
    }
    assert!(inject_reasons.contains_key(&bad_claim));
    assert!(inject_reasons.contains_key(&bad_actor));
}

// ===========================================================================
// C1 — never_reflected
// ===========================================================================

#[test]
fn c1_never_reflected_is_silent_when_empty_and_after_a_counted_reflect() {
    let (_dir, root) = new_root();
    let empty = anomalies_json(&root, &["--now", BASE]);
    assert!(signals_of(&empty, ANOMALY_NEVER_REFLECTED).is_empty());
    assert_eq!(empty["signals"], json!([]));
    assert_eq!(empty["unreviewed_total"], json!(0));

    create_task(&root, "一个任务", "nr:actor");
    let with_task = anomalies_json(&root, &["--now", BASE]);
    assert_eq!(signals_of(&with_task, ANOMALY_NEVER_REFLECTED).len(), 1);

    let reflect = create_task(&root, "[self-reflect] 第一次审计", "nr:actor");
    validate(&root, &reflect, "user", "pass");
    let reflected = anomalies_json(&root, &["--now", BASE]);
    assert!(
        signals_of(&reflected, ANOMALY_NEVER_REFLECTED).is_empty(),
        "a counted reflect task answers never_reflected: {reflected}"
    );

    // A reflect-only library has no non-reflect task, so it stays silent too.
    let (_dir2, only_reflect) = new_root();
    let reflect_only = create_task(&only_reflect, "[self-reflect] 只有审计", "nr:actor");
    validate(&only_reflect, &reflect_only, "user", "pass");
    let report = anomalies_json(&only_reflect, &["--now", BASE]);
    assert!(
        signals_of(&report, ANOMALY_NEVER_REFLECTED).is_empty(),
        "{report}"
    );
}

// ===========================================================================
// C2 — watermark inclusion rules
// ===========================================================================

#[test]
fn c2_watermark_requires_done_pass_and_a_different_reviewer() {
    // Not done.
    let (_dir, root) = new_root();
    let reflect = create_task(&root, "[self-reflect] 未完成", "refl:creator");
    let report = anomalies_json(&root, &["--now", BASE]);
    assert_eq!(report["watermark"], json!(null), "{report}");
    assert_eq!(report["since_seq"], json!(0));

    // Done, but the newest verdict is fail.
    validate(&root, &reflect, "user", "fail");
    let failed = anomalies_json(&root, &["--now", BASE]);
    assert_eq!(failed["watermark"], json!(null), "{failed}");

    // Pass, but reviewed by the creator itself. Stage 6 §C refuses that write
    // (`REVIEWER_CONFLICT`), so the row is inserted by hand to keep proving the
    // watermark ignores it; the refusal itself is covered by
    // tests/learning_review_routing.rs.
    let (_dir2, self_root) = new_root();
    let self_task = create_task(&self_root, "[self-reflect] 自审", "refl:creator");
    write_validation_row(&self_root, &self_task, "refl:creator", "pass");
    let self_reviewed = anomalies_json(&self_root, &["--now", BASE]);
    assert_eq!(
        self_reviewed["watermark"],
        json!(null),
        "a self-review never moves the watermark: {self_reviewed}"
    );

    // Pass, reviewer differs only by surrounding space and case. The §C guard
    // also refuses this write, so the row is inserted by hand: the read-side
    // fold must still identify the creator.
    let (_dir3, fold_root) = new_root();
    let fold_task = create_task(&fold_root, "[self-reflect] 折叠", "User");
    write_validation_row(&fold_root, &fold_task, "  user  ", "pass");
    let folded = anomalies_json(&fold_root, &["--now", BASE]);
    assert_eq!(
        folded["watermark"],
        json!(null),
        "trim + case fold identifies the creator: {folded}"
    );

    // Pass with an independent reviewer: the creation event seq is the
    // watermark.
    let (_dir4, ok_root) = new_root();
    let ok_task = create_task(&ok_root, "[self-reflect] 独立验收", "refl:creator");
    validate(&ok_root, &ok_task, "user", "pass");
    let expected_seq = create_event_seq(&ok_root, &ok_task);
    let ok = anomalies_json(&ok_root, &["--now", BASE]);
    assert_eq!(
        ok["watermark"],
        json!({"event_seq": expected_seq, "task_id": ok_task}),
        "{ok}"
    );
    assert_eq!(ok["since_seq"], json!(expected_seq));
}

#[test]
fn c2_tie_break_prefers_the_smallest_task_id() {
    let (_dir, root) = new_root();
    let first = create_task(&root, "[self-reflect] 平手甲", "refl:creator");
    let second = create_task(&root, "[self-reflect] 平手乙", "refl:creator");
    validate(&root, &first, "user", "pass");
    validate(&root, &second, "other", "pass");

    // Force an exact tie on the creation event sequence (only reachable through
    // a hand-edited store).
    let tie = 9_000i64;
    let conn = connect(&root);
    for id in [&first, &second] {
        conn.execute(
            "UPDATE task_events SET event_seq = ?1 \
             WHERE candidate_id = ?2 AND event_kind = 'claim' \
               AND event_seq = (SELECT MIN(event_seq) FROM task_events \
                                WHERE candidate_id = ?2 AND event_kind = 'claim')",
            rusqlite::params![tie, id],
        )
        .expect("tie event seq");
    }
    drop(conn);

    let mut expected = vec![first.clone(), second.clone()];
    expected.sort();
    let report = anomalies_json(&root, &["--now", BASE]);
    assert_eq!(
        report["watermark"],
        json!({"event_seq": tie, "task_id": expected[0]}),
        "{report}"
    );
}

// ===========================================================================
// C2 — filtering, resurfacing, reflect self-absorption
// ===========================================================================

#[test]
fn c2_reflect_tasks_produce_no_signal_at_all() {
    let (_dir, root) = new_root();
    let reflect = create_task(&root, "[self-reflect] 只跑审计", "reflect:only");
    claim_task(&root, &reflect, "reflect:helper");
    validate(&root, &reflect, "user", "pass");

    let report = anomalies_json(&root, &["--now", BASE]);
    // A reflect-only library: no never_reflected (no non-reflect task), and the
    // reflect task's own creation/handover/validation never appear.
    assert_eq!(report["signals"], json!([]), "{report}");
    assert_eq!(report["unreviewed_total"], json!(0));
    for kind in [
        ANOMALY_NEW_ACTOR,
        ANOMALY_MARKER_HIT,
        ANOMALY_LONG_CLAIM,
        ANOMALY_CREATE_BURST,
        ANOMALY_MULTI_ACTOR,
        ANOMALY_LEGACY_INVALID,
        ANOMALY_NEVER_REFLECTED,
        ANOMALY_REJECT_CLEAR_FLIP,
    ] {
        assert!(signals_of(&report, kind).is_empty(), "{kind}: {report}");
    }
}

#[test]
fn c2_state_signal_resurfaces_after_a_new_event() {
    let (_dir, root) = new_root();
    let task = create_task(&root, "curl http://example.com 任务", "res:a");
    let reflect = create_task(&root, "[self-reflect] 吸收", "reflect:runner");
    validate(&root, &reflect, "user", "pass");
    let watermark = create_event_seq(&root, &reflect);

    let absorbed = anomalies_json(&root, &["--now", BASE]);
    assert!(
        signals_of(&absorbed, ANOMALY_MARKER_HIT).is_empty(),
        "the watermark absorbs the earlier state signal: {absorbed}"
    );

    claim_task(&root, &task, "res:b");
    let new_seq = last_event_seq(&root, "task_events");
    assert!(new_seq > watermark);

    let resurfaced = anomalies_json(&root, &["--now", BASE]);
    let hits = signals_of(&resurfaced, ANOMALY_MARKER_HIT);
    assert_eq!(hits.len(), 1, "{resurfaced}");
    assert_eq!(hits[0]["candidate_id"], json!(task));
    assert_eq!(hits[0]["signal_seq"], json!(new_seq));
    assert_eq!(hits[0]["anchor_seq"], json!(new_seq));
}

#[test]
fn c2_since_seq_overrides_the_default_and_unreviewed_total_stays_default() {
    let (_dir, root) = new_root();
    create_task(&root, "第一个任务", "since:first");
    let reflect = create_task(&root, "[self-reflect] 水位线", "since:runner");
    validate(&root, &reflect, "user", "pass");
    let watermark = create_event_seq(&root, &reflect);
    let later = create_task(&root, "水位线之后的新人任务", "since:later");
    let later_seq = create_event_seq(&root, &later);
    assert!(later_seq > watermark);

    let default = anomalies_json(&root, &["--now", BASE]);
    assert_eq!(default["since_seq"], json!(watermark));
    assert_eq!(ids_of(&default, ANOMALY_NEW_ACTOR), vec![later.clone()]);
    assert_eq!(default["unreviewed_total"], json!(1));

    let wide = anomalies_json(&root, &["--now", BASE, "--since-seq", "0"]);
    assert_eq!(wide["since_seq"], json!(0));
    let wide_ids = ids_of(&wide, ANOMALY_NEW_ACTOR);
    assert_eq!(wide_ids.len(), 2, "{wide}");
    assert!(wide_ids.contains(&later));
    assert_eq!(
        wide["unreviewed_total"],
        json!(1),
        "--since-seq does not change the pending count: {wide}"
    );

    let narrow = anomalies_json(&root, &["--now", BASE, "--since-seq", "100000"]);
    assert_eq!(narrow["since_seq"], json!(100000));
    assert!(
        signals_of(&narrow, ANOMALY_NEW_ACTOR).is_empty(),
        "{narrow}"
    );
    assert_eq!(narrow["unreviewed_total"], json!(1));

    // `--since-seq` is a non-negative integer.
    let bad = anomalies(&root, &["--since-seq", "-1"]);
    assert_eq!(bad.status.code(), Some(2));
    assert_eq!(stderr_json(&bad)["error"]["code"], json!("INVALID_USAGE"));
    let bad_text = anomalies(&root, &["--since-seq", "abc"]);
    assert_eq!(bad_text.status.code(), Some(2));
}

// ===========================================================================
// C3 — read-only, JSON schema, text golden, ordering
// ===========================================================================

const READ_ONLY_TABLES: [&str; 9] = [
    "candidates",
    "candidate_revisions",
    "ingestions",
    "status_history",
    "dedup_history",
    "event_clock",
    "reuse_history",
    "task_events",
    "validation_history",
];

fn count(root: &Path, table: &str) -> i64 {
    connect(root)
        .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
            row.get(0)
        })
        .expect("count rows")
}

fn table_counts(root: &Path) -> Vec<(&'static str, i64)> {
    READ_ONLY_TABLES
        .iter()
        .map(|table| (*table, count(root, table)))
        .collect()
}

fn data_version(conn: &Connection) -> i64 {
    conn.query_row("PRAGMA data_version", [], |row| row.get(0))
        .expect("PRAGMA data_version")
}

fn file_state(root: &Path) -> (u64, Option<std::time::SystemTime>) {
    let meta = std::fs::metadata(root.join("learning.db")).expect("learning.db metadata");
    (meta.len(), meta.modified().ok())
}

#[test]
fn c3_anomalies_is_read_only() {
    let (_dir, root) = new_root();
    let task = create_task(&root, "只读任务", "ro:actor");
    claim_task(&root, &task, "ro:other");
    let reflect = create_task(&root, "[self-reflect] 只读", "ro:runner");
    validate(&root, &reflect, "user", "pass");

    let before_counts = table_counts(&root);
    let before_file = file_state(&root);

    let store = LearningStore::open_read_only(&root).expect("open read-only");
    let changes_before = store.total_changes();
    let report = store
        .anomalies(None, at(BASE))
        .expect("anomalies via the store");
    assert!(report.ok);
    let total = agent_memory::learning::unreviewed_total(&store, at(BASE)).expect("unreviewed");
    assert_eq!(total, report.unreviewed_total);
    let changes_after = store.total_changes();
    assert_eq!(
        changes_before, changes_after,
        "anomalies must not write on the connection it reads through"
    );
    drop(store);

    assert_eq!(table_counts(&root), before_counts);
    assert_eq!(file_state(&root), before_file);

    let control = connect(&root);
    let version_before = data_version(&control);
    let file_before = file_state(&root);
    let _ = anomalies(&root, &[]);
    let _ = anomalies(&root, &["--json"]);
    assert_eq!(data_version(&control), version_before);
    assert_eq!(table_counts(&root), before_counts);
    assert_eq!(file_state(&root), file_before);

    let read_only =
        Connection::open_with_flags(root.join("learning.db"), OpenFlags::SQLITE_OPEN_READ_ONLY)
            .expect("open read-only");
    assert!(
        read_only
            .execute(
                "INSERT INTO status_history (candidate_id, revision, status, recorded_at) \
                 VALUES ('cand_x', 1, 'candidate', '2026-01-01T00:00:00Z')",
                [],
            )
            .is_err(),
        "a read-only connection rejects writes"
    );
}

#[test]
fn c3_json_schema_is_complete_and_the_order_is_deterministic() {
    let (_dir, root) = new_root();
    let long = create_task(&root, &"长".repeat(LONG_CLAIM_CODEPOINTS), "schema:actor");
    let marked = create_task(&root, "curl http://example.com", "schema:actor");
    let reflect = create_task(&root, "[self-reflect] 架构", "schema:runner");
    validate(&root, &reflect, "user", "pass");
    // Two more creates after the watermark stay visible.
    let later_a = create_task(&root, "之后甲", "schema:later");
    let later_b = create_task(&root, "之后乙", "schema:later");
    let after_marker = create_task(&root, "powershell after", "schema:marker");
    let _ = (long, marked, later_a, later_b, after_marker);

    let report = anomalies_json(&root, &["--now", BASE]);
    for key in [
        "ok",
        "root",
        "watermark",
        "since_seq",
        "signals",
        "unreviewed_total",
        "by_kind",
    ] {
        assert!(report.get(key).is_some(), "anomalies JSON is missing {key}");
    }
    assert!(report.get("text").is_none());
    assert_eq!(report["ok"], json!(true));
    assert_eq!(report["root"].as_str().expect("root"), text(&root));
    assert_eq!(report["watermark"]["task_id"], json!(reflect));

    let signals = report["signals"].as_array().expect("signals");
    let mut ordered = signals.clone();
    ordered.sort_by(|left, right| {
        left["kind"]
            .as_str()
            .expect("kind")
            .cmp(right["kind"].as_str().expect("kind"))
            .then_with(|| {
                left["anchor_seq"]
                    .as_i64()
                    .expect("seq")
                    .cmp(&right["anchor_seq"].as_i64().expect("seq"))
            })
            .then_with(|| {
                left["candidate_id"]
                    .as_str()
                    .expect("candidate")
                    .cmp(right["candidate_id"].as_str().expect("candidate"))
            })
    });
    assert_eq!(signals, &ordered, "signals are sorted (kind, anchor, id)");

    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for signal in signals {
        for key in [
            "kind",
            "signal_seq",
            "anchor_seq",
            "candidate_id",
            "actor",
            "detail",
        ] {
            assert!(signal.get(key).is_some(), "signal is missing {key}");
        }
        *counts
            .entry(signal["kind"].as_str().expect("kind").to_string())
            .or_insert(0) += 1;
    }
    let by_kind: BTreeMap<String, usize> = report["by_kind"]
        .as_object()
        .expect("by_kind")
        .iter()
        .map(|(key, value)| (key.clone(), value.as_u64().expect("count") as usize))
        .collect();
    assert_eq!(by_kind, counts);
    assert_eq!(
        report["unreviewed_total"].as_u64().expect("unreviewed") as usize,
        signals.len(),
        "a default report's pending count is its signal count"
    );

    // Same fixture, same bytes.
    let again = anomalies_json(&root, &["--now", BASE]);
    assert_eq!(report, again);
}

#[test]
fn c3_text_mode_is_byte_exact() {
    let (_dir, root) = new_root();
    let task = create_task(&root, "普通任务", "golden:actor");
    set_create_time(&root, &task, BASE);

    let out = anomalies(&root, &["--now", BASE]);
    assert_eq!(out.status.code(), Some(0));
    let expected = format!(
        "never_reflected seq=0 anchor=0 candidate=- actor=- tasks=1 qualifying_reflect=0\n\
         new_actor seq=1 anchor=1 candidate={task} actor=golden:actor first_seen=create\n\
         anomalies: 2 signal(s), unreviewed 2, since_seq 0, watermark none, \
         by_kind never_reflected=1,new_actor=1\n"
    );
    assert_eq!(out.stdout, expected.as_bytes(), "byte-for-byte golden");

    let rendered = String::from_utf8(out.stdout).expect("UTF-8");
    assert!(!rendered.starts_with('\u{feff}'), "no BOM");
    assert!(!rendered.contains('\r'), "LF only");
    for line in rendered.lines() {
        assert!(!line.ends_with(' '), "no trailing space: {line:?}");
    }
    assert!(rendered.ends_with('\n'));
    assert!(!rendered.ends_with("\n\n"), "exactly one final newline");

    // The empty library keeps exactly the summary line.
    let (_dir2, empty) = new_root();
    assert_eq!(
        anomalies_text(&empty, &["--now", BASE]),
        "anomalies: 0 signal(s), unreviewed 0, since_seq 0, watermark none, by_kind none\n"
    );
}

#[test]
fn c3_missing_root_keeps_normal_cli_error_semantics() {
    let dir = tempfile::tempdir().expect("temp dir");
    let missing = dir.path().join("no-learning-root");
    let out = anomalies(&missing, &[]);
    assert_eq!(out.status.code(), Some(6), "integrity class");
    assert!(out.stdout.is_empty());
    assert_eq!(
        stderr_json(&out)["error"]["code"],
        json!("LEARNING_DB_MISSING")
    );
    let json_out = anomalies(&missing, &["--json"]);
    assert_eq!(json_out.status.code(), Some(6));
    assert_eq!(
        stderr_json(&json_out)["error"]["code"],
        json!("LEARNING_DB_MISSING")
    );
}

/// Integration finding (2026-11-06): `candidate_id` is derived from the claim,
/// so a second Self-Reflect run must register with a distinct claim (the run
/// timestamp). Re-using the first run's claim lands on the finished task and
/// can never advance the watermark again.
#[test]
fn c2_consecutive_reflect_runs_need_distinct_claims_and_advance_the_watermark() {
    let (_dir, root) = new_root();
    create_task(&root, "first workload", "host:a");
    let first = create_task(
        &root,
        "[self-reflect] 协调层审计 2026-11-06T08:23Z",
        "host:runner",
    );
    validate(&root, &first, "user", "pass");
    let after_first = anomalies_json(&root, &[]);
    assert_eq!(after_first["unreviewed_total"], json!(0));
    assert_eq!(after_first["watermark"]["task_id"], json!(first));

    create_task(&root, "second workload", "host:b");
    assert_eq!(anomalies_json(&root, &[])["unreviewed_total"], json!(1));

    // Same claim: same candidate, already done -- no new run is registered.
    let root_text = text(&root);
    let repeat = assert_ok(
        &run(&[
            "learning",
            "task",
            "create",
            "--root",
            &root_text,
            "--claim",
            "[self-reflect] 协调层审计 2026-11-06T08:23Z",
            "--actor",
            "host:runner",
        ]),
        "repeat create",
    );
    assert_eq!(repeat["candidate_id"], json!(first));
    assert_eq!(repeat["duplicate"], json!(true));

    // Distinct claim: a new run that advances the watermark past the new signal.
    let second = create_task(
        &root,
        "[self-reflect] 协调层审计 2026-11-07T09:00Z",
        "host:runner",
    );
    assert_ne!(second, first);
    validate(&root, &second, "user", "pass");
    let after_second = anomalies_json(&root, &[]);
    assert_eq!(after_second["unreviewed_total"], json!(0));
    assert_eq!(after_second["watermark"]["task_id"], json!(second));
    assert!(
        after_second["watermark"]["event_seq"].as_i64().unwrap()
            > after_first["watermark"]["event_seq"].as_i64().unwrap()
    );
}
