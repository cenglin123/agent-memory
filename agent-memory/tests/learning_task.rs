//! Stage 4 implementation-002: the task write path (`task create/claim/abandon`),
//! the `learning reuse` idempotency contract, the task guards, and the
//! read-side projection core.
//!
//! Every case builds a synthetic learning root from `tempfile`; nothing here
//! reads a real learning root, a real archive, `~/.memory` or any default path.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use chrono::{DateTime, FixedOffset};
use rusqlite::Connection;
use serde_json::{json, Value};

use agent_memory::learning::model::validate_candidate;
use agent_memory::learning::{
    project_task_state, LearningStore, ReuseRequest, TaskEventView, TASK_STATE_ABANDONED,
    TASK_STATE_ACTIVE, TASK_STATE_DONE, TASK_STATE_STALE,
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

/// `learning init` creates v5; the explicit v4 -> v3 downgrade test needs the
/// exact v4 shape, so drop the stage-7 tables and set `user_version = 4`.
fn make_v4(root: &Path) {
    let conn = connect(root);
    conn.execute_batch(
        "DROP TABLE IF EXISTS qualification_history; \
         DROP TABLE IF EXISTS rejection_overrides;",
    )
    .expect("drop v5 tables");
    conn.pragma_update(None, "user_version", 4)
        .expect("user_version 4");
}

fn count(root: &Path, table: &str) -> i64 {
    connect(root)
        .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
            row.get(0)
        })
        .expect("count rows")
}

fn count_where(root: &Path, sql: &str) -> i64 {
    connect(root)
        .query_row(sql, [], |row| row.get(0))
        .expect("count rows")
}

fn candidate_id_of(claim: &str) -> String {
    agent_memory::learning::model::candidate_id_for(claim, "task", "task")
}

fn create_task(root: &Path, claim: &str, actor: &str) -> Value {
    let root_text = text(root);
    let out = run(&[
        "learning", "task", "create", "--root", &root_text, "--claim", claim, "--actor", actor,
    ]);
    assert_ok(&out, "task create")
}

// ---------------------------------------------------------------------------
// Knowledge fixtures (a task-adjacent candidate must be a real one)
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

/// The payload `learning ingest` must refuse: a well-formed task candidate
/// shaped exactly like `task create` builds it.
fn task_payload(claim: &str) -> Value {
    let candidate_id = candidate_id_of(claim);
    json!({
        "schema_version": 1,
        "candidate_id": "",
        "revision": 1,
        "status": "candidate",
        "claim": claim,
        "target_kind": "task",
        "proposed_target": "task",
        "applicability": [],
        "counterexamples": [],
        "unknowns": [],
        "evidence": [],
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
        "source_checkpoint": format!("task:{candidate_id}"),
        "extractor": "task",
        "created_at": "2026-11-01T00:00:00Z",
        "updated_at": "2026-11-01T00:00:00Z"
    })
}

// ===========================================================================
// A11 — repeated create is a command-level no-op
// ===========================================================================

#[test]
fn a11_repeated_create_is_a_command_level_noop() {
    let (_dir, root) = new_root();
    let root_text = text(&root);
    let claim = "Ship the task write path";

    let first = create_task(&root, claim, "claude-code:creator");
    assert_eq!(first["ok"], json!(true));
    assert_eq!(first["created"], json!(true));
    assert_eq!(first["duplicate"], json!(false));
    assert_eq!(first["claim"], json!(claim));
    assert_eq!(first["actor"], json!("claude-code:creator"));
    assert_eq!(
        first["state"],
        json!(TASK_STATE_ACTIVE),
        "a fresh task projects as active (implementation-003)"
    );
    let candidate_id = first["candidate_id"]
        .as_str()
        .expect("candidate id")
        .to_string();
    assert_eq!(candidate_id, candidate_id_of(claim));

    let ingestions = count(&root, "ingestions");
    let events = count(&root, "task_events");
    let candidates = count(&root, "candidates");
    assert_eq!((ingestions, events, candidates), (1, 1, 1));

    // Same claim, different surrounding whitespace: the identity normalization
    // folds it, so this is the same task and writes nothing at all.
    let second = create_task(&root, "  Ship   the task write path  ", "opencode:other");
    assert_eq!(second["created"], json!(false));
    assert_eq!(second["duplicate"], json!(true));
    assert_eq!(second["candidate_id"], json!(candidate_id));
    assert_eq!(second["claim"], json!(claim));
    assert_eq!(
        (
            count(&root, "ingestions"),
            count(&root, "task_events"),
            count(&root, "candidates")
        ),
        (ingestions, events, candidates),
        "a duplicate create must not add an ingestion, an event or a candidate"
    );

    // A different claim is a different task.
    let other = create_task(&root, "Ship the task write path!", "claude-code:creator");
    assert_eq!(other["created"], json!(true));
    assert_ne!(other["candidate_id"], json!(candidate_id));
    assert_eq!(count(&root, "candidates"), 2);

    // The stored payload is the task shape: no evidence, revision 1, fixed
    // proposed target, stable ingest identity.
    let store = LearningStore::open(&root).expect("open");
    let view = store.show(&candidate_id).expect("show");
    assert_eq!(view.candidate.target_kind, "task");
    assert_eq!(view.candidate.proposed_target, "task");
    assert_eq!(view.candidate.revision, 1);
    assert_eq!(view.candidate.status, "candidate");
    assert!(view.candidate.evidence.is_empty());
    assert!(view.candidate.contradicting_evidence.is_empty());
    assert_eq!(view.candidate.dedup.disposition, "pending");
    assert_eq!(view.candidate.extractor, "task");
    assert_eq!(
        view.candidate.source_checkpoint,
        format!("task:{candidate_id}")
    );
    assert_eq!(view.ingestions.len(), 1);

    // The first claim event belongs to the creator.
    let actor: String = connect(&root)
        .query_row(
            "SELECT actor_id FROM task_events WHERE candidate_id = ?1 AND event_kind = 'claim'",
            [&candidate_id],
            |row| row.get(0),
        )
        .expect("first claim row");
    assert_eq!(actor, "claude-code:creator");

    // Exit code of the duplicate-create CLI call was already asserted as 0.
    let out = run(&[
        "learning", "task", "create", "--root", &root_text, "--claim", claim, "--actor", "a:b",
    ]);
    assert_eq!(out.status.code(), Some(0));
}

#[test]
fn a10_concurrent_creates_of_one_claim_commit_exactly_one_task() {
    let (_dir, root) = new_root();
    let claim = "One task, several creators";

    let mut handles = Vec::new();
    for index in 0..4 {
        let root = root.clone();
        handles.push(std::thread::spawn(move || {
            let mut store = LearningStore::open(&root).expect("open");
            store
                .create_task(claim, &format!("worker:{index}"))
                .expect("create")
        }));
    }
    let outcomes: Vec<_> = handles
        .into_iter()
        .map(|handle| handle.join().expect("join"))
        .collect();

    assert_eq!(
        outcomes.iter().filter(|outcome| outcome.created).count(),
        1,
        "exactly one create wins the write lock"
    );
    assert_eq!(
        outcomes.iter().filter(|outcome| outcome.duplicate).count(),
        3
    );
    assert_eq!(count(&root, "candidates"), 1);
    assert_eq!(count(&root, "ingestions"), 1);
    assert_eq!(count(&root, "task_events"), 1);
    let ids: std::collections::BTreeSet<&str> = outcomes
        .iter()
        .map(|outcome| outcome.candidate_id.as_str())
        .collect();
    assert_eq!(ids.len(), 1, "every caller converges on one candidate id");
}

// ===========================================================================
// A8 — the acting identity is required on every write path
// ===========================================================================
#[test]
fn a8_actor_is_required_on_every_write_path() {
    let (_dir, root) = new_root();
    let root_text = text(&root);
    let candidate_id = create_task(&root, "Audit the actor field", "claude-code:creator")
        ["candidate_id"]
        .as_str()
        .expect("candidate id")
        .to_string();

    let claim_before = count(&root, "task_events");
    let reuse_before = count(&root, "reuse_history");

    // task create
    let out = run(&[
        "learning",
        "task",
        "create",
        "--root",
        &root_text,
        "--claim",
        "No actor here",
    ]);
    assert_error(&out, 2, "INVALID_USAGE");
    assert_eq!(
        count(&root, "candidates"),
        1,
        "a refused create writes no candidate"
    );

    // blank actor is the same failure
    let out = run(&[
        "learning",
        "task",
        "create",
        "--root",
        &root_text,
        "--claim",
        "Blank actor",
        "--actor",
        "   ",
    ]);
    assert_error(&out, 2, "INVALID_USAGE");

    // task claim
    let out = run(&[
        "learning",
        "task",
        "claim",
        "--root",
        &root_text,
        "--candidate-id",
        &candidate_id,
    ]);
    assert_error(&out, 2, "INVALID_USAGE");

    // task abandon
    let out = run(&[
        "learning",
        "task",
        "abandon",
        "--root",
        &root_text,
        "--candidate-id",
        &candidate_id,
        "--reason",
        "no longer needed",
    ]);
    assert_error(&out, 2, "INVALID_USAGE");

    // learning reuse
    let out = run(&[
        "learning",
        "reuse",
        "--root",
        &root_text,
        "--candidate-id",
        &candidate_id,
        "--task",
        &candidate_id,
        "--outcome",
        "pass",
    ]);
    assert_error(&out, 2, "INVALID_USAGE");

    assert_eq!(
        count(&root, "task_events"),
        claim_before,
        "no event written"
    );
    assert_eq!(count(&root, "reuse_history"), reuse_before, "no reuse row");
}

// ===========================================================================
// A7 — task-only guards
// ===========================================================================

#[test]
fn a7_task_payload_cannot_be_ingested_directly() {
    let (_dir, root) = new_root();
    let root_text = text(&root);
    let payload = task_payload("Bypass the task create path").to_string();

    let out = run(&[
        "learning",
        "ingest",
        "--root",
        &root_text,
        "--candidate",
        &payload,
    ]);
    assert_error(&out, 2, "TASK_REQUIRES_TASK_CREATE");
    assert_eq!(count(&root, "candidates"), 0);
    assert_eq!(count(&root, "task_events"), 0);
    assert_eq!(count(&root, "ingestions"), 0);
}

#[test]
fn a7_claim_and_abandon_refuse_knowledge_candidates() {
    let (_dir, root) = new_root();
    let root_text = text(&root);
    let knowledge_id = ingest_knowledge(&root, "Keep explicit roots");
    let events_before = count(&root, "task_events");

    for action in ["claim", "abandon"] {
        let out = run(&[
            "learning",
            "task",
            action,
            "--root",
            &root_text,
            "--candidate-id",
            &knowledge_id,
            "--actor",
            "claude-code:worker",
            "--reason",
            "attempted abandon",
        ]);
        assert_error(&out, 2, "NOT_A_TASK");
    }
    assert_eq!(
        count(&root, "task_events"),
        events_before,
        "a refused claim/abandon writes nothing"
    );

    // A missing candidate keeps the existing not-found semantics (exit 6).
    for action in ["claim", "abandon"] {
        let out = run(&[
            "learning",
            "task",
            action,
            "--root",
            &root_text,
            "--candidate-id",
            "cand_missing",
            "--actor",
            "claude-code:worker",
            "--reason",
            "attempted abandon",
        ]);
        assert_error(&out, 6, "CANDIDATE_NOT_FOUND");
    }
}

#[test]
fn a7_task_reuse_requires_its_own_candidate_id() {
    let (_dir, root) = new_root();
    let root_text = text(&root);
    let candidate_id = create_task(&root, "Reuse must name the task itself", "claude-code:a")
        ["candidate_id"]
        .as_str()
        .expect("candidate id")
        .to_string();

    // A different task id is a usage error, before any row is considered.
    let out = run(&[
        "learning",
        "reuse",
        "--root",
        &root_text,
        "--candidate-id",
        &candidate_id,
        "--task",
        "some-other-task",
        "--outcome",
        "pass",
        "--actor",
        "claude-code:a",
    ]);
    assert_error(&out, 2, "TASK_MISMATCH");
    assert_eq!(count(&root, "reuse_history"), 0);

    // A blank --task keeps the v3 REUSE_TASK_CONFLICT semantics.
    let out = run(&[
        "learning",
        "reuse",
        "--root",
        &root_text,
        "--candidate-id",
        &candidate_id,
        "--task",
        "   ",
        "--outcome",
        "pass",
        "--actor",
        "claude-code:a",
    ]);
    assert_error(&out, 2, "REUSE_TASK_CONFLICT");
    assert_eq!(count(&root, "reuse_history"), 0);

    // Matching the candidate id works.
    let out = run(&[
        "learning",
        "reuse",
        "--root",
        &root_text,
        "--candidate-id",
        &candidate_id,
        "--task",
        &candidate_id,
        "--outcome",
        "pass",
        "--actor",
        "claude-code:a",
    ]);
    let json = assert_ok(&out, "task reuse");
    assert_eq!(json["duplicate"], json!(false));
    assert_eq!(count(&root, "reuse_history"), 1);
}

#[test]
fn a7_abandon_requires_a_reason() {
    let (_dir, root) = new_root();
    let root_text = text(&root);
    let candidate_id = create_task(&root, "Abandon needs a reason", "claude-code:a")
        ["candidate_id"]
        .as_str()
        .expect("candidate id")
        .to_string();
    let before = count(&root, "task_events");

    let out = run(&[
        "learning",
        "task",
        "abandon",
        "--root",
        &root_text,
        "--candidate-id",
        &candidate_id,
        "--actor",
        "claude-code:a",
    ]);
    assert_error(&out, 2, "INVALID_USAGE");

    let out = run(&[
        "learning",
        "task",
        "abandon",
        "--root",
        &root_text,
        "--candidate-id",
        &candidate_id,
        "--actor",
        "claude-code:a",
        "--reason",
        "   ",
    ]);
    assert_error(&out, 2, "INVALID_USAGE");
    assert_eq!(count(&root, "task_events"), before);
}

// ===========================================================================
// A4 — idempotency key (task, actor, client_seq)
// ===========================================================================

#[test]
fn a4_reuse_idempotency_contract() {
    let (_dir, root) = new_root();
    let root_text = text(&root);
    let candidate_id = create_task(&root, "Record reuse idempotently", "claude-code:a")
        ["candidate_id"]
        .as_str()
        .expect("candidate id")
        .to_string();

    let reuse = |seq: Option<&str>, idempotent: bool, actor: &str, notes: &str| {
        let mut args = vec![
            "learning".to_string(),
            "reuse".to_string(),
            "--root".to_string(),
            root_text.clone(),
            "--candidate-id".to_string(),
            candidate_id.clone(),
            "--task".to_string(),
            candidate_id.clone(),
            "--outcome".to_string(),
            "pass".to_string(),
            "--notes".to_string(),
            notes.to_string(),
            "--actor".to_string(),
            actor.to_string(),
        ];
        if let Some(seq) = seq {
            args.push("--client-seq".to_string());
            args.push(seq.to_string());
        }
        if idempotent {
            args.push("--idempotent".to_string());
        }
        let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
        run(&borrowed)
    };

    // 1. A pinned sequence is written as given.
    let first = assert_ok(
        &reuse(Some("5"), false, "claude-code:a", "first"),
        "pinned seq",
    );
    assert_eq!(first["client_seq"], json!(5));
    assert_eq!(first["duplicate"], json!(false));
    assert_eq!(first["actor"], json!("claude-code:a"));
    assert!(first["event_seq"].as_i64().expect("event seq") > 0);
    let first_seq = first["history_seq"].clone();
    let first_event_seq = first["event_seq"].clone();
    assert_eq!(count(&root, "reuse_history"), 1);

    // 2. The same key without --idempotent is a conflict and writes nothing.
    let out = reuse(Some("5"), false, "claude-code:a", "first");
    assert_error(&out, 2, "REUSE_IDEMPOTENCY_CONFLICT");
    assert_eq!(count(&root, "reuse_history"), 1);
    assert_eq!(count(&root, "event_clock"), 2, "one clock row per event");

    // 3. --idempotent without --client-seq is a usage error.
    let out = reuse(None, true, "claude-code:a", "first");
    assert_error(&out, 2, "INVALID_USAGE");
    assert_eq!(count(&root, "reuse_history"), 1);

    // 4. Idempotent replay of an identical event returns the stored row.
    let replay = assert_ok(&reuse(Some("5"), true, "claude-code:a", "first"), "replay");
    assert_eq!(replay["duplicate"], json!(true));
    assert_eq!(replay["history_seq"], first_seq);
    assert_eq!(replay["event_seq"], first_event_seq);
    assert_eq!(replay["recorded_at"], first["recorded_at"]);
    assert_eq!(count(&root, "reuse_history"), 1, "replay writes no row");
    assert_eq!(count(&root, "event_clock"), 2, "replay allocates no event");

    // 5. Same key, different notes -> mismatch.
    let out = reuse(Some("5"), true, "claude-code:a", "different notes");
    assert_error(&out, 2, "REUSE_IDEMPOTENCY_MISMATCH");
    assert_eq!(count(&root, "reuse_history"), 1);

    // 6. A non-numeric --client-seq is a usage error.
    let out = reuse(Some("abc"), false, "claude-code:a", "first");
    assert_error(&out, 2, "INVALID_USAGE");
    assert_eq!(count(&root, "reuse_history"), 1);

    // 7. A different actor may reuse the same sequence value.
    let other_actor = assert_ok(
        &reuse(Some("5"), false, "opencode:b", "first"),
        "other actor same seq",
    );
    assert_eq!(other_actor["client_seq"], json!(5));
    assert_eq!(count(&root, "reuse_history"), 2);

    // 8. The same actor may use a different sequence value.
    let next_seq = assert_ok(
        &reuse(Some("6"), false, "claude-code:a", "second round"),
        "same actor next seq",
    );
    assert_eq!(next_seq["client_seq"], json!(6));
    assert_eq!(count(&root, "reuse_history"), 3);

    // 9. Without --client-seq the server allocates max+1 per (task, actor).
    let auto = assert_ok(&reuse(None, false, "claude-code:a", "auto"), "auto seq");
    assert_eq!(auto["client_seq"], json!(7));
    let auto_again = assert_ok(
        &reuse(None, false, "claude-code:a", "auto again"),
        "auto seq 2",
    );
    assert_eq!(auto_again["client_seq"], json!(8));

    // The stored rows carry the real actor and a unique event sequence.
    let conn = connect(&root);
    let mut stmt = conn
        .prepare("SELECT actor_id, client_seq, event_seq FROM reuse_history ORDER BY client_seq, actor_id")
        .expect("prepare");
    let rows: Vec<(String, i64, i64)> = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("collect");
    assert!(rows.iter().all(|(actor, _, _)| actor != "migrated:v3"));
    let distinct: std::collections::BTreeSet<i64> = rows.iter().map(|(_, _, s)| *s).collect();
    assert_eq!(distinct.len(), rows.len(), "event_seq is unique per event");
}

#[test]
fn a4_reuse_idempotency_detects_an_outcome_mismatch() {
    let (_dir, root) = new_root();
    let root_text = text(&root);
    let candidate_id = create_task(&root, "Outcome mismatch is a mismatch", "claude-code:a")
        ["candidate_id"]
        .as_str()
        .expect("candidate id")
        .to_string();

    let out = run(&[
        "learning",
        "reuse",
        "--root",
        &root_text,
        "--candidate-id",
        &candidate_id,
        "--task",
        &candidate_id,
        "--outcome",
        "pass",
        "--actor",
        "claude-code:a",
        "--client-seq",
        "3",
    ]);
    assert_ok(&out, "initial reuse");

    let out = run(&[
        "learning",
        "reuse",
        "--root",
        &root_text,
        "--candidate-id",
        &candidate_id,
        "--task",
        &candidate_id,
        "--outcome",
        "fail",
        "--actor",
        "claude-code:a",
        "--client-seq",
        "3",
        "--idempotent",
    ]);
    assert_error(&out, 2, "REUSE_IDEMPOTENCY_MISMATCH");
    assert_eq!(count(&root, "reuse_history"), 1);

    // A task candidate is not subject to the knowledge-candidate
    // (candidate, task) uniqueness check: a new sequence records a new event.
    let out = run(&[
        "learning",
        "reuse",
        "--root",
        &root_text,
        "--candidate-id",
        &candidate_id,
        "--task",
        &candidate_id,
        "--outcome",
        "fail",
        "--actor",
        "claude-code:a",
        "--client-seq",
        "4",
    ]);
    assert_ok(&out, "second reuse of the same task");
    assert_eq!(count(&root, "reuse_history"), 2);
}

// ===========================================================================
// A5 — the S5 knowledge contract survives v4
// ===========================================================================

#[test]
fn a5_knowledge_candidate_records_one_task_once() {
    let (_dir, root) = new_root();
    let root_text = text(&root);
    let first = ingest_knowledge(&root, "Knowledge candidate one");
    let second = ingest_knowledge(&root, "Knowledge candidate two");

    let reuse = |candidate: &str, task: &str, outcome: &str| {
        run(&[
            "learning",
            "reuse",
            "--root",
            &root_text,
            "--candidate-id",
            candidate,
            "--task",
            task,
            "--outcome",
            outcome,
            "--actor",
            "claude-code:a",
        ])
    };

    assert_ok(&reuse(&first, "task-1", "pass"), "first application");
    assert_eq!(count(&root, "reuse_history"), 1);

    // An explicit, pinned retry is a declared replay: it returns the stored
    // event instead of conflicting.
    let replay = run(&[
        "learning",
        "reuse",
        "--root",
        &root_text,
        "--candidate-id",
        &first,
        "--task",
        "task-1",
        "--outcome",
        "pass",
        "--actor",
        "claude-code:a",
        "--client-seq",
        "1",
        "--idempotent",
    ]);
    let replay = assert_ok(&replay, "idempotent knowledge replay");
    assert_eq!(replay["duplicate"], json!(true));
    assert_eq!(count(&root, "reuse_history"), 1);

    // The same pinned key without --idempotent is the idempotency conflict.
    let out = run(&[
        "learning",
        "reuse",
        "--root",
        &root_text,
        "--candidate-id",
        &first,
        "--task",
        "task-1",
        "--outcome",
        "pass",
        "--actor",
        "claude-code:a",
        "--client-seq",
        "1",
    ]);
    assert_error(&out, 2, "REUSE_IDEMPOTENCY_CONFLICT");
    assert_eq!(count(&root, "reuse_history"), 1);

    // An unpinned repeat of the same (candidate, task) keeps the S5 contract.
    let out = reuse(&first, "task-1", "fail");
    assert_error(&out, 2, "REUSE_TASK_CONFLICT");
    assert_eq!(count(&root, "reuse_history"), 1);

    // A second candidate may be applied to the same task by the same actor:
    // the idempotency index is per (task, actor, client_seq), so the server
    // allocates the next sequence instead of colliding.
    let shared = assert_ok(&reuse(&second, "task-1", "pass"), "shared task");
    assert_eq!(shared["client_seq"], json!(2));
    assert_eq!(count(&root, "reuse_history"), 2);

    let conn = connect(&root);
    let mut stmt = conn
        .prepare("SELECT candidate_id, client_seq FROM reuse_history WHERE task_id = 'task-1' ORDER BY client_seq")
        .expect("prepare");
    let rows: Vec<(String, i64)> = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("collect");
    assert_eq!(
        rows,
        vec![(first.clone(), 1), (second.clone(), 2)],
        "two candidates, one task, distinct client sequences"
    );
}

#[test]
fn a5_migrated_store_keeps_writing_multiple_candidates_for_one_task() {
    let (_dir, root) = new_root();
    let root_text = text(&root);
    let first = ingest_knowledge(&root, "Pre-migration candidate");

    // A v3-era reuse row: it is written through the v4 API, then the store is
    // explicitly downgraded and reopened, so the row is exactly the migrated
    // shape (`actor_id = migrated:v3`, `client_seq = old seq`).
    {
        let mut store = LearningStore::open(&root).expect("open");
        let request = ReuseRequest::parse(&first, "task-shared", "pass", None, "claude-code:a")
            .expect("request");
        store.record_reuse(&request).expect("record reuse");
    }
    make_v4(&root);
    let out = run(&["learning", "migrate", "--root", &root_text, "--to", "3"]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "migrate: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    drop(LearningStore::open(&root).expect("reopen migrates back to v4"));

    let actor: String = connect(&root)
        .query_row("SELECT actor_id FROM reuse_history", [], |row| row.get(0))
        .expect("migrated actor");
    assert_eq!(actor, "migrated:v3");

    // Two fresh candidates record the same task for the same actor: the
    // migrated row (actor migrated:v3, client_seq 1) cannot collide with them.
    for claim in ["Post-migration candidate A", "Post-migration candidate B"] {
        let id = ingest_knowledge(&root, claim);
        let out = run(&[
            "learning",
            "reuse",
            "--root",
            &root_text,
            "--candidate-id",
            &id,
            "--task",
            "task-shared",
            "--outcome",
            "pass",
            "--actor",
            "claude-code:a",
        ]);
        let json = assert_ok(&out, "post-migration reuse");
        assert!(json["client_seq"].as_i64().expect("seq") >= 1);
    }
    assert_eq!(count(&root, "reuse_history"), 3);
    let distinct: i64 = count_where(
        &root,
        "SELECT COUNT(DISTINCT client_seq) FROM reuse_history WHERE task_id = 'task-shared' AND actor_id = 'claude-code:a'",
    );
    assert_eq!(distinct, 2, "the two new rows take distinct sequences");
}

// ===========================================================================
// A11 — payload branch: the task shape is strict, the six knowledge kinds
// keep the S1 rules verbatim
// ===========================================================================

#[test]
fn task_payload_branch_is_strict_and_knowledge_kinds_are_unchanged() {
    // The happy path: a task payload derives its identity from
    // (claim, "task", "task") and stores no evidence.
    let validated = validate_candidate(&task_payload("Payload branch")).expect("task payload");
    assert_eq!(validated.record.target_kind, "task");
    assert_eq!(validated.record.revision, 1);
    assert_eq!(validated.record.proposed_target, "task");
    assert!(validated.record.evidence.is_empty());
    assert_eq!(
        validated.record.candidate_id,
        candidate_id_of("Payload branch")
    );
    assert_eq!(
        validated.ingest_key,
        agent_memory::learning::model::ingest_key_for(
            "task",
            &format!("task:{}", validated.record.candidate_id)
        )
    );

    let mut cases: Vec<(&str, Value)> = Vec::new();

    let mut evidence = task_payload("Task with evidence");
    evidence["evidence"] = json!([knowledge_candidate("x")["evidence"][0].clone()]);
    cases.push(("task evidence", evidence));

    let mut contradicting = task_payload("Task with contradicting evidence");
    contradicting["contradicting_evidence"] =
        json!([knowledge_candidate("y")["evidence"][0].clone()]);
    cases.push(("task contradicting evidence", contradicting));

    let mut proposed = task_payload("Task with another target");
    proposed["proposed_target"] = json!("some/other/target");
    cases.push(("task proposed_target", proposed));

    let mut revision = task_payload("Task at revision two");
    revision["revision"] = json!(2);
    cases.push(("task revision", revision));

    let mut status = task_payload("Task with a settled status");
    status["status"] = json!("accepted");
    cases.push(("task status", status));

    let mut dedup = task_payload("Task with a dedup result");
    dedup["dedup"]["disposition"] = json!("new");
    cases.push(("task dedup", dedup));

    let mut promotion = task_payload("Task with a promotion");
    promotion["promotion"]["approval"] = json!({"by": "someone"});
    cases.push(("task promotion", promotion));

    let mut reuse = task_payload("Task with a reuse array");
    reuse["reuse"] = json!([{"task_id": "t", "outcome": "pass"}]);
    cases.push(("task reuse array", reuse));

    let mut blank_claim = task_payload("   ");
    blank_claim["claim"] = json!("   ");
    cases.push(("task blank claim", blank_claim));

    for (name, value) in cases {
        let error = validate_candidate(&value).expect_err(name);
        assert_eq!(error.code, "INVALID_CANDIDATE", "{name}");
    }

    // A knowledge kind still requires evidence: the task branch must not have
    // relaxed the six existing kinds.
    let mut knowledge = knowledge_candidate("Knowledge needs evidence");
    knowledge["evidence"] = json!([]);
    let error = validate_candidate(&knowledge).expect_err("knowledge needs evidence");
    assert_eq!(error.code, "INVALID_CANDIDATE");
    assert!(
        error
            .message
            .contains("evidence must contain at least one item"),
        "{}",
        error.message
    );
}

// ===========================================================================
// Projection core
// ===========================================================================

fn at(value: &str) -> DateTime<FixedOffset> {
    DateTime::parse_from_rfc3339(value).expect("timestamp")
}

fn now() -> DateTime<FixedOffset> {
    at("2026-11-04T12:00:00Z")
}

fn event(kind: &str, event_seq: i64, recorded_at: &str, actor: &str) -> TaskEventView {
    let source = match kind {
        "reuse" => "reuse",
        "validation" => "validation",
        _ => "task",
    };
    TaskEventView {
        source: source.to_string(),
        kind: kind.to_string(),
        event_seq,
        recorded_at: recorded_at.to_string(),
        actor: actor.to_string(),
        outcome: match kind {
            "reuse" => "pass",
            "validation" => "pass",
            _ => "",
        }
        .to_string(),
        note: String::new(),
    }
}

fn validation(event_seq: i64, recorded_at: &str, reviewer: &str, verdict: &str) -> TaskEventView {
    TaskEventView {
        source: "validation".to_string(),
        kind: "validation".to_string(),
        event_seq,
        recorded_at: recorded_at.to_string(),
        actor: reviewer.to_string(),
        outcome: verdict.to_string(),
        note: "report-1".to_string(),
    }
}

const RECENT: &str = "2026-11-03T12:00:00Z";
const OLD: &str = "2026-09-01T12:00:00Z";

#[test]
fn projection_covers_every_plan_rule() {
    struct Case {
        name: &'static str,
        events: Vec<TaskEventView>,
        state: &'static str,
        owner: Option<&'static str>,
        created_by: Option<&'static str>,
        reuse_total: usize,
        event_total: usize,
    }

    let cases = vec![
        Case {
            name: "only the first claim",
            events: vec![event("claim", 1, RECENT, "a")],
            state: TASK_STATE_ACTIVE,
            owner: Some("a"),
            created_by: Some("a"),
            reuse_total: 0,
            event_total: 1,
        },
        Case {
            name: "claim and reuse stay active",
            events: vec![
                event("claim", 1, RECENT, "a"),
                event("reuse", 2, RECENT, "a"),
            ],
            state: TASK_STATE_ACTIVE,
            owner: Some("a"),
            created_by: Some("a"),
            reuse_total: 1,
            event_total: 2,
        },
        Case {
            name: "a passing validation is done",
            events: vec![
                event("claim", 1, RECENT, "a"),
                validation(2, RECENT, "reviewer", "pass"),
            ],
            state: TASK_STATE_DONE,
            owner: Some("a"),
            created_by: Some("a"),
            reuse_total: 0,
            event_total: 2,
        },
        Case {
            name: "a failing validation is active",
            events: vec![
                event("claim", 1, RECENT, "a"),
                validation(2, RECENT, "reviewer", "fail"),
            ],
            state: TASK_STATE_ACTIVE,
            owner: Some("a"),
            created_by: Some("a"),
            reuse_total: 0,
            event_total: 2,
        },
        Case {
            name: "fail then pass is done",
            events: vec![
                event("claim", 1, RECENT, "a"),
                validation(2, RECENT, "reviewer", "fail"),
                validation(3, RECENT, "reviewer", "pass"),
            ],
            state: TASK_STATE_DONE,
            owner: Some("a"),
            created_by: Some("a"),
            reuse_total: 0,
            event_total: 3,
        },
        Case {
            name: "reuse after done keeps done and does not move the owner",
            events: vec![
                event("claim", 1, RECENT, "a"),
                validation(2, RECENT, "reviewer", "pass"),
                event("reuse", 3, RECENT, "b"),
            ],
            state: TASK_STATE_DONE,
            owner: Some("a"),
            created_by: Some("a"),
            reuse_total: 1,
            event_total: 3,
        },
        Case {
            name: "claim after done keeps done and moves the owner",
            events: vec![
                event("claim", 1, RECENT, "a"),
                validation(2, RECENT, "reviewer", "pass"),
                event("claim", 3, RECENT, "b"),
            ],
            state: TASK_STATE_DONE,
            owner: Some("b"),
            created_by: Some("a"),
            reuse_total: 0,
            event_total: 3,
        },
        Case {
            name: "the newest abandon wins",
            events: vec![
                event("claim", 1, RECENT, "a"),
                event("abandon", 2, RECENT, "b"),
            ],
            state: TASK_STATE_ABANDONED,
            owner: Some("a"),
            created_by: Some("a"),
            reuse_total: 0,
            event_total: 2,
        },
        Case {
            name: "an abandon after a passing validation is abandoned",
            events: vec![
                event("claim", 1, RECENT, "a"),
                validation(2, RECENT, "reviewer", "pass"),
                event("abandon", 3, RECENT, "b"),
            ],
            state: TASK_STATE_ABANDONED,
            owner: Some("a"),
            created_by: Some("a"),
            reuse_total: 0,
            event_total: 3,
        },
        Case {
            name: "an abandon then a reuse is active again",
            events: vec![
                event("claim", 1, RECENT, "a"),
                event("abandon", 2, RECENT, "b"),
                event("reuse", 3, RECENT, "b"),
            ],
            state: TASK_STATE_ACTIVE,
            owner: Some("a"),
            created_by: Some("a"),
            reuse_total: 1,
            event_total: 3,
        },
        Case {
            name: "active past the TTL is stale",
            events: vec![event("claim", 1, OLD, "a")],
            state: TASK_STATE_STALE,
            owner: Some("a"),
            created_by: Some("a"),
            reuse_total: 0,
            event_total: 1,
        },
        Case {
            name: "a failing validation past the TTL is stale",
            events: vec![
                event("claim", 1, OLD, "a"),
                validation(2, OLD, "reviewer", "fail"),
            ],
            state: TASK_STATE_STALE,
            owner: Some("a"),
            created_by: Some("a"),
            reuse_total: 0,
            event_total: 2,
        },
        Case {
            name: "done never decays",
            events: vec![event("claim", 1, OLD, "a"), validation(2, OLD, "r", "pass")],
            state: TASK_STATE_DONE,
            owner: Some("a"),
            created_by: Some("a"),
            reuse_total: 0,
            event_total: 2,
        },
        Case {
            name: "abandoned never decays",
            events: vec![event("claim", 1, OLD, "a"), event("abandon", 2, OLD, "a")],
            state: TASK_STATE_ABANDONED,
            owner: Some("a"),
            created_by: Some("a"),
            reuse_total: 0,
            event_total: 2,
        },
        Case {
            name: "an unknown event kind is an ordinary event",
            events: vec![
                event("claim", 1, RECENT, "a"),
                event("note-added-by-hand", 2, RECENT, "b"),
            ],
            state: TASK_STATE_ACTIVE,
            owner: Some("a"),
            created_by: Some("a"),
            reuse_total: 0,
            event_total: 2,
        },
        Case {
            name: "an unknown event kind does not undo done",
            events: vec![
                event("claim", 1, RECENT, "a"),
                validation(2, RECENT, "r", "pass"),
                event("note-added-by-hand", 3, RECENT, "b"),
            ],
            state: TASK_STATE_DONE,
            owner: Some("a"),
            created_by: Some("a"),
            reuse_total: 0,
            event_total: 3,
        },
    ];

    for case in cases {
        let projection = project_task_state(&case.events, now(), 30);
        assert_eq!(projection.state, case.state, "case: {}", case.name);
        assert_eq!(
            projection.owner.as_deref(),
            case.owner,
            "case: {} (owner)",
            case.name
        );
        assert_eq!(
            projection.created_by.as_deref(),
            case.created_by,
            "case: {} (created_by)",
            case.name
        );
        assert_eq!(
            projection.reuse_total, case.reuse_total,
            "case: {} (reuse_total)",
            case.name
        );
        assert_eq!(
            projection.event_total, case.event_total,
            "case: {} (event_total)",
            case.name
        );
    }
}

#[test]
fn projection_orders_by_event_seq_not_by_input_order() {
    let ordered = vec![
        event("claim", 1, RECENT, "a"),
        validation(2, RECENT, "reviewer", "pass"),
        event("claim", 3, RECENT, "b"),
    ];
    let mut shuffled = ordered.clone();
    shuffled.reverse();
    shuffled.swap(0, 1);

    let expected = project_task_state(&ordered, now(), 30);
    let actual = project_task_state(&shuffled, now(), 30);
    assert_eq!(actual, expected);
    assert_eq!(actual.state, TASK_STATE_DONE);
    assert_eq!(actual.owner.as_deref(), Some("b"));
    assert_eq!(
        actual.last_event.expect("last event").event_seq,
        3,
        "the last event is the highest event_seq"
    );
}

#[test]
fn projection_cross_table_interleaving_follows_the_write_order() {
    // validate(pass) -> abandon
    let events = vec![
        event("claim", 1, RECENT, "a"),
        validation(2, RECENT, "r", "pass"),
        event("abandon", 3, RECENT, "a"),
    ];
    assert_eq!(
        project_task_state(&events, now(), 30).state,
        TASK_STATE_ABANDONED
    );

    // abandon -> reuse
    let events = vec![
        event("claim", 1, RECENT, "a"),
        event("abandon", 2, RECENT, "a"),
        event("reuse", 3, RECENT, "a"),
    ];
    assert_eq!(
        project_task_state(&events, now(), 30).state,
        TASK_STATE_ACTIVE
    );

    // claim -> validate(pass)
    let events = vec![
        event("claim", 1, RECENT, "a"),
        validation(2, RECENT, "r", "pass"),
    ];
    assert_eq!(
        project_task_state(&events, now(), 30).state,
        TASK_STATE_DONE
    );

    // validate(pass) -> claim (done is kept, the owner moves)
    let events = vec![
        event("claim", 1, RECENT, "a"),
        validation(2, RECENT, "r", "pass"),
        event("claim", 3, RECENT, "b"),
    ];
    let projection = project_task_state(&events, now(), 30);
    assert_eq!(projection.state, TASK_STATE_DONE);
    assert_eq!(projection.owner.as_deref(), Some("b"));
    assert_eq!(projection.created_by.as_deref(), Some("a"));
}

#[test]
fn projection_ttl_boundary_and_explicit_now() {
    let created = at("2026-10-05T12:00:00Z");
    let events = vec![TaskEventView {
        source: "task".to_string(),
        kind: "claim".to_string(),
        event_seq: 1,
        recorded_at: created.to_rfc3339(),
        actor: "a".to_string(),
        outcome: String::new(),
        note: String::new(),
    }];

    // Exactly 30 days: not stale (the comparison is a strict "older than").
    assert_eq!(
        project_task_state(&events, created + chrono::Duration::days(30), 30).state,
        TASK_STATE_ACTIVE
    );
    // One second past the TTL: stale.
    assert_eq!(
        project_task_state(
            &events,
            created + chrono::Duration::days(30) + chrono::Duration::seconds(1),
            30
        )
        .state,
        TASK_STATE_STALE
    );
    // A relaxed TTL keeps it active.
    assert_eq!(
        project_task_state(&events, created + chrono::Duration::days(31), 60).state,
        TASK_STATE_ACTIVE
    );
}

#[test]
fn projection_of_a_fresh_task_is_active_and_owner_is_the_creator() {
    let (_dir, root) = new_root();
    create_task(&root, "Project the fresh task", "claude-code:creator");
    let store = LearningStore::open(&root).expect("open");
    let candidate_id = candidate_id_of("Project the fresh task");

    let events = store.task_event_view(&candidate_id).expect("event view");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].kind, "claim");
    assert_eq!(events[0].source, "task");
    assert_eq!(events[0].actor, "claude-code:creator");

    let projection = store
        .project_task(candidate_id.as_str(), at("2026-11-10T00:00:00Z"), 30)
        .expect("project");
    assert_eq!(projection.state, TASK_STATE_ACTIVE);
    assert_eq!(projection.owner.as_deref(), Some("claude-code:creator"));
    assert_eq!(
        projection.created_by.as_deref(),
        Some("claude-code:creator")
    );
    assert_eq!(projection.last_validation, None);
    assert_eq!(projection.reuse_total, 0);
    assert_eq!(projection.event_total, 1);
}

#[test]
fn projection_reads_the_event_tables_not_the_payload() {
    let (_dir, root) = new_root();
    let root_text = text(&root);
    let candidate_id = create_task(&root, "Project from the tables", "claude-code:creator")
        ["candidate_id"]
        .as_str()
        .expect("candidate id")
        .to_string();

    for (action, extra) in [("claim", "second round"), ("abandon", "done for now")] {
        let mut args = vec![
            "learning",
            "task",
            action,
            "--root",
            &root_text,
            "--candidate-id",
            &candidate_id,
            "--actor",
            "opencode:worker",
        ];
        if action == "abandon" {
            args.extend(["--reason", extra]);
        } else {
            args.extend(["--note", extra]);
        }
        assert_ok(&run(&args), "task event via cli");
    }

    let store = LearningStore::open(&root).expect("open");
    // The payload keeps its pristine task shape: no reuse/validation arrays are
    // appended, so the projection can only come from the event tables (S8).
    let view = store.show(&candidate_id).expect("show");
    assert!(view.candidate.reuse.is_empty());
    assert!(view.candidate.validation.is_empty());
    assert!(view.candidate.evidence.is_empty());

    let events = store.task_event_view(&candidate_id).expect("event view");
    assert_eq!(events.len(), 3);
    let seqs: Vec<i64> = events.iter().map(|event| event.event_seq).collect();
    let mut sorted = seqs.clone();
    sorted.sort_unstable();
    assert_eq!(seqs, sorted, "the view is ordered by event_seq");

    let projection = store
        .project_task(candidate_id.as_str(), at("2026-11-10T00:00:00Z"), 30)
        .expect("project");
    assert_eq!(projection.state, TASK_STATE_ABANDONED);
    assert_eq!(
        projection.owner.as_deref(),
        Some("opencode:worker"),
        "the newest claim moved ownership"
    );
    assert_eq!(
        projection.created_by.as_deref(),
        Some("claude-code:creator")
    );
    assert_eq!(
        projection.last_event.as_ref().expect("last").kind,
        "abandon"
    );
    assert_eq!(projection.event_total, 3);
}

#[test]
fn projection_stale_is_decided_per_candidate() {
    let (_dir, root) = new_root();
    let stale_id = create_task(&root, "An old untouched task", "claude-code:a")["candidate_id"]
        .as_str()
        .expect("candidate id")
        .to_string();
    let live_id = create_task(&root, "A recently touched task", "claude-code:a")["candidate_id"]
        .as_str()
        .expect("candidate id")
        .to_string();

    // Pin both timelines so the case does not depend on the machine clock.
    {
        let conn = connect(&root);
        conn.execute(
            "UPDATE task_events SET recorded_at = ?2 WHERE candidate_id = ?1",
            rusqlite::params![stale_id, "2026-09-01T00:00:00.000Z"],
        )
        .expect("age task A");
        conn.execute(
            "UPDATE task_events SET recorded_at = ?2 WHERE candidate_id = ?1",
            rusqlite::params![live_id, "2026-11-15T00:00:00.000Z"],
        )
        .expect("date task B");
    }

    let clock = at("2026-11-20T00:00:00Z");
    let store = LearningStore::open(&root).expect("open");
    assert_eq!(
        store.project_task(&stale_id, clock, 30).expect("A").state,
        TASK_STATE_STALE
    );
    assert_eq!(
        store.project_task(&live_id, clock, 30).expect("B").state,
        TASK_STATE_ACTIVE
    );
    drop(store);

    // Writing to B must not make A fresh: TTL is measured on the candidate's own
    // unified view.
    let root_text = text(&root);
    assert_ok(
        &run(&[
            "learning",
            "task",
            "claim",
            "--root",
            &root_text,
            "--candidate-id",
            &live_id,
            "--actor",
            "opencode:worker",
        ]),
        "claim B",
    );
    let store = LearningStore::open(&root).expect("open");
    assert_eq!(
        store.project_task(&stale_id, clock, 30).expect("A2").state,
        TASK_STATE_STALE
    );
    let live = store.project_task(&live_id, clock, 30).expect("B2");
    assert_eq!(live.state, TASK_STATE_ACTIVE);
    assert_eq!(live.owner.as_deref(), Some("opencode:worker"));
    assert_eq!(live.event_total, 2);
}

#[test]
fn projection_includes_reuse_rows_of_knowledge_candidates_for_the_task() {
    let (_dir, root) = new_root();
    let root_text = text(&root);
    let task_id = create_task(&root, "A task with knowledge reuse", "claude-code:a")
        ["candidate_id"]
        .as_str()
        .expect("candidate id")
        .to_string();
    let knowledge_id = ingest_knowledge(&root, "Applied to the task");

    assert_ok(
        &run(&[
            "learning",
            "reuse",
            "--root",
            &root_text,
            "--candidate-id",
            &knowledge_id,
            "--task",
            &task_id,
            "--outcome",
            "pass",
            "--actor",
            "opencode:worker",
        ]),
        "knowledge reuse on the task",
    );

    let store = LearningStore::open(&root).expect("open");
    let events = store.task_event_view(&task_id).expect("event view");
    assert_eq!(events.len(), 2, "claim plus the knowledge reuse row");
    assert_eq!(events[1].source, "reuse");
    assert_eq!(events[1].kind, "reuse");
    assert_eq!(events[1].actor, "opencode:worker");

    let projection = store
        .project_task(task_id.as_str(), at("2026-11-10T00:00:00Z"), 30)
        .expect("project");
    assert_eq!(projection.state, TASK_STATE_ACTIVE);
    assert_eq!(
        projection.owner.as_deref(),
        Some("claude-code:a"),
        "reuse never moves ownership"
    );
    assert_eq!(projection.reuse_total, 1);
}
