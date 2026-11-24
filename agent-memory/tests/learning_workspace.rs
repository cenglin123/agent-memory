//! Stage 4 implementation-003: the read-side workspace surface
//! (`learning workspace`, `learning task show`) and the task isolation guards
//! (plan §5, §7 and acceptance A6 / A7 / A12 / A13).
//!
//! Every case builds a synthetic learning root from `tempfile`; nothing here
//! reads a real learning root, a real archive, `~/.memory` or any default path.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use chrono::{DateTime, FixedOffset};
use rusqlite::{Connection, OpenFlags};
use serde_json::{json, Value};

use agent_memory::learning::{
    LearningStore, TaskEventView, TASK_NOT_ROUTEABLE, TASK_STATE_ABANDONED, TASK_STATE_ACTIVE,
    TASK_STATE_DONE, TASK_STATE_STALE,
};

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

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

fn assert_error(out: &Output, exit: u8, code: &str) {
    assert_eq!(
        out.status.code(),
        Some(i32::from(exit)),
        "expected {code} (exit {exit}), stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        stderr_json(out)["error"]["code"],
        Value::String(code.to_string())
    );
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

fn count(root: &Path, table: &str) -> i64 {
    connect(root)
        .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
            row.get(0)
        })
        .expect("count rows")
}

fn candidate_id_of(claim: &str) -> String {
    agent_memory::learning::model::candidate_id_for(claim, "task", "task")
}

fn at(value: &str) -> DateTime<FixedOffset> {
    DateTime::parse_from_rfc3339(value).expect("timestamp")
}

fn now() -> DateTime<FixedOffset> {
    at("2026-11-04T12:00:00Z")
}

// ---------------------------------------------------------------------------
// Command wrappers
// ---------------------------------------------------------------------------

fn create_task(root: &Path, claim: &str, actor: &str) -> Value {
    let root_text = text(root);
    let out = run(&[
        "learning", "task", "create", "--root", &root_text, "--claim", claim, "--actor", actor,
    ]);
    assert_ok(&out, "task create")
}

fn claim_task(root: &Path, candidate_id: &str, actor: &str, note: Option<&str>) {
    let root_text = text(root);
    let mut args = vec![
        "learning",
        "task",
        "claim",
        "--root",
        &root_text,
        "--candidate-id",
        candidate_id,
        "--actor",
        actor,
    ];
    if let Some(note) = note {
        args.push("--note");
        args.push(note);
    }
    let out = run(&args);
    assert_ok(&out, "task claim");
}

fn abandon_task(root: &Path, candidate_id: &str, actor: &str, reason: &str) {
    let root_text = text(root);
    let out = run(&[
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
        reason,
    ]);
    assert_ok(&out, "task abandon");
}

fn reuse(root: &Path, candidate_id: &str, task: &str, outcome: &str, actor: &str) {
    let root_text = text(root);
    let out = run(&[
        "learning",
        "reuse",
        "--root",
        &root_text,
        "--candidate-id",
        candidate_id,
        "--task",
        task,
        "--outcome",
        outcome,
        "--actor",
        actor,
    ]);
    assert_ok(&out, "reuse");
}

fn validate(root: &Path, candidate_id: &str, reviewer: &str, verdict: &str, report_ref: &str) {
    let root_text = text(root);
    let out = run(&[
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
        report_ref,
    ]);
    assert_ok(&out, "validate");
}

fn workspace(root: &Path, extra: &[&str]) -> Value {
    let root_text = text(root);
    let mut args = vec!["learning", "workspace", "--root", &root_text];
    args.extend_from_slice(extra);
    let out = run(&args);
    assert_ok(&out, "workspace")
}

fn task_show(root: &Path, candidate_id: &str, extra: &[&str]) -> Value {
    let root_text = text(root);
    let mut args = vec![
        "learning",
        "task",
        "show",
        "--root",
        &root_text,
        "--candidate-id",
        candidate_id,
    ];
    args.extend_from_slice(extra);
    let out = run(&args);
    assert_ok(&out, "task show")
}

fn listed_ids(report: &Value) -> Vec<String> {
    report["tasks"]
        .as_array()
        .expect("tasks array")
        .iter()
        .map(|row| {
            row["candidate_id"]
                .as_str()
                .expect("candidate id")
                .to_string()
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Knowledge fixture
// ---------------------------------------------------------------------------

fn span_sha256(tag: &str) -> String {
    let mut acc: u64 = 14_695_981_039_346_656_037;
    for byte in tag.as_bytes() {
        acc ^= u64::from(*byte);
        acc = acc.wrapping_mul(1_099_511_628_211);
    }
    format!("{acc:064x}")
}

fn knowledge_candidate(claim: &str) -> Value {
    json!({
        "schema_version": 1,
        "candidate_id": "",
        "revision": 1,
        "status": "candidate",
        "claim": claim,
        "target_kind": "workflow",
        "proposed_target": "AGENTS.md#explicit-roots",
        "applicability": ["agent-memory"],
        "counterexamples": [],
        "unknowns": [],
        "evidence": [{
            "ref": "claude:session-s1:line-1",
            "span_sha256": span_sha256("s1"),
            "archive_generation": "g1",
            "observed_at": "2026-11-01T00:00:00Z",
            "actor": "user",
            "session_kind": "interactive",
            "origin_class": "owner",
            "evidence_kind": "user_statement",
            "recall_taint": false
        }],
        "contradicting_evidence": [],
        "dedup": {
            "query": "",
            "matches": [],
            "disposition": "pending",
            "checked_at": null
        },
        "reuse": [],
        "validation": [],
        "promotion": {
            "approval": null,
            "apply": null,
            "verification": null
        },
        "ingestions": [],
        "source_checkpoint": format!("checkpoint:{}", claim),
        "extractor": "test",
        "created_at": "2026-11-01T00:00:00Z",
        "updated_at": "2026-11-01T00:00:00Z"
    })
}

fn ingest_knowledge(root: &Path, claim: &str) -> String {
    let root_text = text(root);
    let payload = knowledge_candidate(claim).to_string();
    let out = run(&[
        "learning",
        "ingest",
        "--root",
        &root_text,
        "--candidate",
        &payload,
    ]);
    assert_ok(&out, "ingest knowledge candidate")["candidate_id"]
        .as_str()
        .expect("candidate id")
        .to_string()
}

// ===========================================================================
// A6 — the read-only surface writes nothing
// ===========================================================================

const READ_ONLY_TABLES: [&str; 8] = [
    "candidates",
    "candidate_revisions",
    "ingestions",
    "status_history",
    "dedup_history",
    "event_clock",
    "reuse_history",
    "task_events",
];

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
fn a6_workspace_and_task_show_are_read_only() {
    let (_dir, root) = new_root();
    let task = create_task(&root, "Keep the read side read-only", "claude-code:creator")
        ["candidate_id"]
        .as_str()
        .expect("candidate id")
        .to_string();
    claim_task(&root, &task, "opencode:owner", Some("picked up"));
    validate(&root, &task, "reviewer:one", "pass", "report-1");
    let knowledge = ingest_knowledge(&root, "A workflow applied to the task");
    reuse(&root, &knowledge, &task, "pass", "claude-code:applier");
    let other = create_task(&root, "An abandoned task", "claude-code:creator")["candidate_id"]
        .as_str()
        .expect("candidate id")
        .to_string();
    abandon_task(&root, &other, "claude-code:creator", "no longer needed");

    let before_counts = table_counts(&root);
    let before_file = file_state(&root);

    // Same connection: the library read path must not change `total_changes`.
    let store = LearningStore::open(&root).expect("open store");
    let changes_before = store.total_changes();
    let report = store.workspace(None, now(), 30).expect("workspace");
    assert_eq!(report.total, 2);
    let show = store.task_show(&task, now(), 30).expect("task show");
    assert_eq!(show.event_total, 4);
    let show_other = store.task_show(&other, now(), 30).expect("task show other");
    assert_eq!(show_other.state, TASK_STATE_ABANDONED);
    let changes_after = store.total_changes();
    assert_eq!(
        changes_before, changes_after,
        "workspace/task show must not write on the connection they read through"
    );
    drop(store);
    assert_eq!(table_counts(&root), before_counts);
    assert_eq!(file_state(&root), before_file);

    // Independent second connection: `data_version` only moves when another
    // connection commits, so both CLI calls must leave it alone, as they must
    // leave the per-table row counts and the database file untouched.
    let control = connect(&root);
    let version_before = data_version(&control);
    let file_before = file_state(&root);

    let _ = workspace(&root, &[]);
    let _ = task_show(&root, &task, &[]);

    assert_eq!(data_version(&control), version_before);
    assert_eq!(table_counts(&root), before_counts);
    assert_eq!(file_state(&root), file_before);

    // A `mode=ro` connection refuses every write.
    let read_only =
        Connection::open_with_flags(root.join("learning.db"), OpenFlags::SQLITE_OPEN_READ_ONLY)
            .expect("open read-only");
    let write = read_only.execute(
        "INSERT INTO status_history (candidate_id, revision, status, recorded_at) \
         VALUES ('cand_x', 1, 'candidate', '2026-01-01T00:00:00Z')",
        [],
    );
    assert!(write.is_err(), "a read-only connection must reject writes");
}

// ===========================================================================
// A12 — task show renders the complete unified event view
// ===========================================================================

fn assert_event_shape(event: &Value) {
    for key in [
        "source",
        "kind",
        "event_seq",
        "recorded_at",
        "actor",
        "outcome",
        "note",
    ] {
        assert!(event.get(key).is_some(), "event {event} is missing {key}");
    }
}

#[test]
fn a12_task_show_renders_the_full_unified_event_view() {
    let (_dir, root) = new_root();
    let task = create_task(
        &root,
        "Reconstruct the whole event stream",
        "claude-code:creator",
    )["candidate_id"]
        .as_str()
        .expect("candidate id")
        .to_string();
    let knowledge = ingest_knowledge(&root, "A workflow that advanced the task");
    // event 2: a knowledge candidate applied to task T (reuse with task_id=T).
    reuse(&root, &knowledge, &task, "pass", "claude-code:applier");
    // event 3: the task's own reuse.
    reuse(&root, &task, &task, "pass", "claude-code:creator");
    // event 4: independent validation.
    validate(&root, &task, "reviewer:one", "pass", "report-1");
    // event 5: ownership handoff.
    claim_task(&root, &task, "opencode:owner2", Some("handoff"));
    // event 6: abandonment.
    abandon_task(&root, &task, "opencode:owner2", "scope moved");

    let show = task_show(
        &root,
        &task,
        &["--stale-days", "30", "--now", "2026-11-04T12:00:00Z"],
    );

    assert_eq!(show["ok"], json!(true));
    assert_eq!(show["candidate_id"], json!(task));
    assert_eq!(show["claim"], json!("Reconstruct the whole event stream"));
    assert_eq!(show["target_kind"], json!("task"));
    assert_eq!(show["state"], json!(TASK_STATE_ABANDONED));
    assert_eq!(show["created_by"], json!("claude-code:creator"));
    assert_eq!(show["owner"], json!("opencode:owner2"));
    assert_eq!(show["reuse_total"], json!(2));
    assert_eq!(show["event_total"], json!(6));
    assert_eq!(show["last_event"]["kind"], json!("abandon"));
    assert_eq!(show["last_validation"]["reviewer"], json!("reviewer:one"));
    assert_eq!(show["last_validation"]["verdict"], json!("pass"));

    let events = show["events"].as_array().expect("events array");
    assert_eq!(events.len(), 6);
    let kinds: Vec<&str> = events
        .iter()
        .map(|event| event["kind"].as_str().expect("kind"))
        .collect();
    assert_eq!(
        kinds,
        vec!["claim", "reuse", "reuse", "validation", "claim", "abandon"]
    );
    let sources: Vec<&str> = events
        .iter()
        .map(|event| event["source"].as_str().expect("source"))
        .collect();
    assert_eq!(
        sources,
        vec!["task", "reuse", "reuse", "validation", "task", "task"]
    );
    let seqs: Vec<i64> = events
        .iter()
        .map(|event| event["event_seq"].as_i64().expect("event_seq"))
        .collect();
    assert_eq!(seqs, vec![1, 2, 3, 4, 5, 6], "strictly ascending event_seq");

    // The knowledge candidate's `task_id = T` row is present and attributed.
    let knowledge_row = &events[1];
    assert_eq!(knowledge_row["actor"], json!("claude-code:applier"));
    assert_eq!(knowledge_row["outcome"], json!("pass"));
    assert_eq!(events[3]["actor"], json!("reviewer:one"));
    assert_eq!(events[3]["outcome"], json!("pass"));
    assert_eq!(events[3]["note"], json!("report-1"));
    assert_eq!(events[4]["actor"], json!("opencode:owner2"));
    assert_eq!(events[5]["note"], json!("scope moved"));
    for event in events {
        assert_event_shape(event);
    }

    // A `pass` validation projects `done`; `claim`/`reuse` never lift it.
    let done_task = create_task(&root, "A reviewed task", "claude-code:creator")["candidate_id"]
        .as_str()
        .expect("candidate id")
        .to_string();
    validate(&root, &done_task, "reviewer:one", "pass", "report-2");
    let done_show = task_show(&root, &done_task, &[]);
    assert_eq!(done_show["state"], json!(TASK_STATE_DONE));
    assert_eq!(done_show["event_total"], json!(2));
}

#[test]
fn a12_task_show_refuses_unknown_and_knowledge_candidates() {
    let (_dir, root) = new_root();
    let knowledge = ingest_knowledge(&root, "A knowledge candidate is not a task");
    let root_text = text(&root);

    let unknown = run(&[
        "learning",
        "task",
        "show",
        "--root",
        &root_text,
        "--candidate-id",
        "cand_unknown",
    ]);
    assert_error(&unknown, 6, "CANDIDATE_NOT_FOUND");

    let not_a_task = run(&[
        "learning",
        "task",
        "show",
        "--root",
        &root_text,
        "--candidate-id",
        &knowledge,
    ]);
    assert_error(&not_a_task, 2, "NOT_A_TASK");
}

// ===========================================================================
// A13 — `--state` maps the four projection states and sorting is descending
// ===========================================================================

#[test]
fn a13_state_filter_covers_the_four_states_and_sorts_descending() {
    let (_dir, root) = new_root();
    let active_id = create_task(&root, "An active task", "claude-code:creator")["candidate_id"]
        .as_str()
        .expect("candidate id")
        .to_string();
    let done_id = create_task(&root, "A done task", "claude-code:creator")["candidate_id"]
        .as_str()
        .expect("candidate id")
        .to_string();
    let abandoned_id = create_task(&root, "An abandoned task", "claude-code:creator")
        ["candidate_id"]
        .as_str()
        .expect("candidate id")
        .to_string();
    let stale_id = create_task(&root, "A stale task", "claude-code:creator")["candidate_id"]
        .as_str()
        .expect("candidate id")
        .to_string();

    validate(&root, &done_id, "reviewer:one", "pass", "report-done");
    abandon_task(&root, &abandoned_id, "claude-code:creator", "stopped");
    // Synthetic fixture: age the stale task's only event far past the TTL. The
    // read side decides staleness from this timestamp, never from a stored
    // state; `event_seq` is untouched.
    connect(&root)
        .execute(
            "UPDATE task_events SET recorded_at = '2020-01-01T00:00:00Z' WHERE candidate_id = ?1",
            [&stale_id],
        )
        .expect("age the stale task");

    let all = workspace(&root, &[]);
    assert_eq!(all["ok"], json!(true));
    assert_eq!(all["total"], json!(4));
    for key in [
        "root",
        "generated_at",
        "stale_days",
        "now",
        "total",
        "tasks",
    ] {
        assert!(all.get(key).is_some(), "workspace output is missing {key}");
    }
    assert_eq!(all["stale_days"], json!(30));

    let states: Vec<&str> = all["tasks"]
        .as_array()
        .expect("tasks")
        .iter()
        .map(|row| row["state"].as_str().expect("state"))
        .collect();
    assert_eq!(
        states,
        vec![
            TASK_STATE_ABANDONED,
            TASK_STATE_DONE,
            TASK_STATE_STALE,
            TASK_STATE_ACTIVE
        ]
    );
    // Newest `event_seq` first: abandon (6), validation (5), stale claim (4),
    // active claim (1).
    assert_eq!(
        listed_ids(&all),
        vec![
            abandoned_id.clone(),
            done_id.clone(),
            stale_id.clone(),
            active_id.clone()
        ]
    );
    let seqs: Vec<i64> = all["tasks"]
        .as_array()
        .expect("tasks")
        .iter()
        .map(|row| row["last_event"]["event_seq"].as_i64().expect("seq"))
        .collect();
    assert_eq!(seqs, vec![6, 5, 4, 1]);
    assert!(seqs.windows(2).all(|pair| pair[0] >= pair[1]));

    for (filter, expected) in [
        ("active", &active_id),
        ("done", &done_id),
        ("abandoned", &abandoned_id),
        ("stale", &stale_id),
    ] {
        let report = workspace(&root, &["--state", filter]);
        assert_eq!(report["total"], json!(1), "--state {filter}");
        assert_eq!(
            listed_ids(&report),
            vec![expected.clone()],
            "--state {filter}"
        );
        assert_eq!(
            report["tasks"][0]["state"],
            json!(filter),
            "--state {filter}"
        );
    }
    let explicit_all = workspace(&root, &["--state", "all"]);
    assert_eq!(explicit_all["total"], json!(4));
    assert_eq!(listed_ids(&explicit_all), listed_ids(&all));

    // `--now` and `--stale-days` are explicit inputs to the pure projection.
    let future = workspace(&root, &["--now", "2030-01-01T00:00:00Z"]);
    assert_eq!(future["total"], json!(4));
    assert_eq!(future["now"], json!("2030-01-01T00:00:00.000Z"));
    let future_states: Vec<&str> = future["tasks"]
        .as_array()
        .expect("tasks")
        .iter()
        .map(|row| row["state"].as_str().expect("state"))
        .collect();
    assert_eq!(
        future_states,
        vec![
            TASK_STATE_ABANDONED,
            TASK_STATE_DONE,
            TASK_STATE_STALE,
            TASK_STATE_STALE
        ],
        "a future --now ages the active task; done/abandoned never decay"
    );
    let relaxed = workspace(
        &root,
        &["--now", "2030-01-01T00:00:00Z", "--stale-days", "100000"],
    );
    let relaxed_states: Vec<&str> = relaxed["tasks"]
        .as_array()
        .expect("tasks")
        .iter()
        .map(|row| row["state"].as_str().expect("state"))
        .collect();
    assert_eq!(
        relaxed_states,
        vec![
            TASK_STATE_ABANDONED,
            TASK_STATE_DONE,
            TASK_STATE_ACTIVE,
            TASK_STATE_ACTIVE
        ],
        "a wide TTL keeps both active tasks active (the aged one included)"
    );

    for extra in [
        vec!["--state", "parked"],
        vec!["--stale-days", "abc"],
        vec!["--now", "not-a-timestamp"],
    ] {
        let root_text = text(&root);
        let mut args = vec!["learning", "workspace", "--root", &root_text];
        args.extend_from_slice(&extra);
        let out = run(&args);
        assert_error(&out, 2, "INVALID_USAGE");
    }
}

// ===========================================================================
// A7 — list isolation and the four non-routeable commands
// ===========================================================================

#[test]
fn a7_list_hides_tasks_unless_include_tasks_is_given() {
    let (_dir, root) = new_root();
    let task_a = create_task(&root, "First task", "claude-code:creator")["candidate_id"]
        .as_str()
        .expect("candidate id")
        .to_string();
    let task_b = create_task(&root, "Second task", "claude-code:creator")["candidate_id"]
        .as_str()
        .expect("candidate id")
        .to_string();
    let knowledge = ingest_knowledge(&root, "A knowledge candidate");

    let root_text = text(&root);
    let default_list = assert_ok(
        &run(&["learning", "list", "--root", &root_text]),
        "list default",
    );
    assert_eq!(default_list["count"], json!(1));
    assert_eq!(
        default_list["candidates"][0]["candidate_id"],
        json!(knowledge)
    );
    assert_eq!(
        default_list["candidates"][0]["target_kind"],
        json!("workflow")
    );
    for row in default_list["candidates"].as_array().expect("candidates") {
        assert_ne!(row["target_kind"], json!("task"));
    }

    let with_tasks = assert_ok(
        &run(&["learning", "list", "--root", &root_text, "--include-tasks"]),
        "list --include-tasks",
    );
    assert_eq!(with_tasks["count"], json!(3));
    let ids: Vec<&str> = with_tasks["candidates"]
        .as_array()
        .expect("candidates")
        .iter()
        .map(|row| row["candidate_id"].as_str().expect("id"))
        .collect();
    assert!(ids.contains(&task_a.as_str()));
    assert!(ids.contains(&task_b.as_str()));
    assert!(ids.contains(&knowledge.as_str()));

    let filtered = assert_ok(
        &run(&[
            "learning",
            "list",
            "--root",
            &root_text,
            "--status",
            "candidate",
            "--include-tasks",
        ]),
        "list --status --include-tasks",
    );
    assert_eq!(filtered["count"], json!(3));

    // The library entry point follows the same default.
    let store = LearningStore::open(&root).expect("open");
    assert_eq!(store.list(None).expect("list").count, 1);
    assert_eq!(
        store.list_with_tasks(None, true).expect("list all").count,
        3
    );
}

#[test]
fn a7_task_candidates_are_not_routeable() {
    let (_dir, root) = new_root();
    let root_text = text(&root);
    let task = create_task(&root, "A task with no route", "claude-code:creator")["candidate_id"]
        .as_str()
        .expect("candidate id")
        .to_string();
    let task_payload = {
        let candidate_id = candidate_id_of("A task with no route");
        json!({
            "schema_version": 1,
            "candidate_id": "",
            "revision": 1,
            "status": "candidate",
            "claim": "A task with no route",
            "target_kind": "task",
            "proposed_target": "task",
            "applicability": [],
            "counterexamples": [],
            "unknowns": [],
            "evidence": [],
            "contradicting_evidence": [],
            "dedup": {"query": "", "matches": [], "disposition": "pending", "checked_at": null},
            "reuse": [],
            "validation": [],
            "promotion": {"approval": null, "apply": null, "verification": null},
            "ingestions": [],
            "source_checkpoint": format!("task:{candidate_id}"),
            "extractor": "task",
            "created_at": "2026-11-01T00:00:00Z",
            "updated_at": "2026-11-01T00:00:00Z"
        })
        .to_string()
    };
    let missing_archive = root.join("no-archive").to_string_lossy().to_string();

    // gate refuses before the archive is even opened.
    let gate_by_id = run(&[
        "learning",
        "gate",
        "--root",
        &root_text,
        "--candidate-id",
        &task,
        "--archive",
        &missing_archive,
        "--domain",
        "test-domain",
    ]);
    assert_error(&gate_by_id, 2, TASK_NOT_ROUTEABLE);

    let gate_by_payload = run(&[
        "learning",
        "gate",
        "--root",
        &root_text,
        "--candidate",
        &task_payload,
        "--archive",
        &missing_archive,
        "--domain",
        "test-domain",
    ]);
    assert_error(&gate_by_payload, 2, TASK_NOT_ROUTEABLE);

    // dedup refuses with zero writes: the record and history stay untouched.
    let record_before: String = connect(&root)
        .query_row(
            "SELECT record_json FROM candidates WHERE candidate_id = ?1",
            [&task],
            |row| row.get(0),
        )
        .expect("record_json before dedup");
    let dedup = run(&[
        "learning",
        "dedup",
        "--root",
        &root_text,
        "--candidate-id",
        &task,
        "--report",
        "{\"query\":\"q\",\"results\":[]}",
        "--disposition",
        "new",
        "--reason",
        "checked",
    ]);
    assert_error(&dedup, 2, TASK_NOT_ROUTEABLE);
    assert_eq!(count(&root, "dedup_history"), 0);
    let record_after: String = connect(&root)
        .query_row(
            "SELECT record_json FROM candidates WHERE candidate_id = ?1",
            [&task],
            |row| row.get(0),
        )
        .expect("record_json after dedup");
    assert_eq!(record_after, record_before);

    // gate-target reports the refusal in the report shape.
    let target = assert_ok(
        &run(&[
            "learning",
            "gate-target",
            "--root",
            &root_text,
            "--candidate-id",
            &task,
        ]),
        "gate-target task",
    );
    assert_eq!(target["ok"], json!(false));
    assert_eq!(target["target_kind"], json!("task"));
    assert_eq!(target["verdict"], json!("insufficient_evidence"));
    assert_eq!(target["blocking"], json!([TASK_NOT_ROUTEABLE]));

    // export produces the same report and never a package or an --out file.
    let out_path = root.join("task-package.json");
    let out_text = out_path.to_string_lossy().to_string();
    let exported = assert_ok(
        &run(&[
            "learning",
            "export",
            "--root",
            &root_text,
            "--candidate-id",
            &task,
            "--out",
            &out_text,
        ]),
        "export task",
    );
    assert_eq!(exported["ok"], json!(false));
    assert_eq!(exported["blocking"], json!([TASK_NOT_ROUTEABLE]));
    assert!(
        exported.get("candidate").is_none(),
        "a task candidate has no review package"
    );
    assert!(!out_path.exists(), "export must not write a task package");

    // Knowledge candidates keep their v3/v4 behaviour: no isolation code.
    let knowledge = ingest_knowledge(&root, "A routeable knowledge candidate");
    let knowledge_target = assert_ok(
        &run(&[
            "learning",
            "gate-target",
            "--root",
            &root_text,
            "--candidate-id",
            &knowledge,
        ]),
        "gate-target knowledge",
    );
    assert_ne!(
        knowledge_target["blocking"],
        json!([TASK_NOT_ROUTEABLE]),
        "the isolation code is task-specific"
    );
    let knowledge_export = assert_ok(
        &run(&[
            "learning",
            "export",
            "--root",
            &root_text,
            "--candidate-id",
            &knowledge,
        ]),
        "export knowledge",
    );
    assert_eq!(knowledge_export["ok"], json!(true));
    assert_eq!(knowledge_export["candidate_id"], json!(knowledge));

    let knowledge_dedup = assert_ok(
        &run(&[
            "learning",
            "dedup",
            "--root",
            &root_text,
            "--candidate-id",
            &knowledge,
            "--report",
            "{\"query\":\"q\",\"results\":[]}",
            "--disposition",
            "new",
            "--reason",
            "checked",
        ]),
        "dedup knowledge",
    );
    assert_eq!(knowledge_dedup["ok"], json!(true));
    assert_eq!(count(&root, "dedup_history"), 1);

    // `gate` on a knowledge candidate reaches the archive step instead of the
    // isolation guard.
    let knowledge_gate = run(&[
        "learning",
        "gate",
        "--root",
        &root_text,
        "--candidate-id",
        &knowledge,
        "--archive",
        &missing_archive,
        "--domain",
        "test-domain",
    ]);
    assert_ne!(knowledge_gate.status.code(), Some(0));
    assert_ne!(
        stderr_json(&knowledge_gate)["error"]["code"],
        Value::String(TASK_NOT_ROUTEABLE.to_string())
    );
}

#[test]
fn create_reports_the_projected_state_on_both_paths() {
    let (_dir, root) = new_root();
    let claim = "Report the state on create";
    let created = create_task(&root, claim, "claude-code:creator");
    assert_eq!(created["created"], json!(true));
    assert_eq!(created["state"], json!(TASK_STATE_ACTIVE));
    let candidate_id = created["candidate_id"]
        .as_str()
        .expect("candidate id")
        .to_string();

    let duplicate = create_task(&root, claim, "opencode:other");
    assert_eq!(duplicate["created"], json!(false));
    assert_eq!(duplicate["duplicate"], json!(true));
    assert_eq!(duplicate["candidate_id"], json!(candidate_id));
    assert_eq!(duplicate["state"], json!(TASK_STATE_ACTIVE));

    validate(&root, &candidate_id, "reviewer:one", "pass", "report-1");
    let after_pass = create_task(&root, claim, "opencode:other");
    assert_eq!(after_pass["duplicate"], json!(true));
    assert_eq!(after_pass["state"], json!(TASK_STATE_DONE));

    abandon_task(&root, &candidate_id, "claude-code:creator", "done enough");
    let after_abandon = create_task(&root, claim, "opencode:other");
    assert_eq!(after_abandon["duplicate"], json!(true));
    assert_eq!(after_abandon["state"], json!(TASK_STATE_ABANDONED));
}

#[test]
fn task_show_events_serialize_every_unified_view_field() {
    // Guard against a serde rename/skip silently dropping a field the CLI
    // contract promises (source/kind/event_seq/recorded_at/actor/outcome/note).
    let event = TaskEventView {
        source: "reuse".to_string(),
        kind: "reuse".to_string(),
        event_seq: 7,
        recorded_at: "2026-11-04T00:00:00Z".to_string(),
        actor: "claude-code:applier".to_string(),
        outcome: "pass".to_string(),
        note: "applied".to_string(),
    };
    let value = serde_json::to_value(&event).expect("serialize event");
    assert_event_shape(&value);
}
