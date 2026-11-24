//! Stage 6 §D: task-experience distillation links (`origin_tasks`).
//!
//! Every case uses a synthetic temporary learning root; nothing here touches a
//! real root, an Archive or a recall configuration. The fixture conventions are
//! the ones fixed by the stage-6 checkpoints: `tempfile` roots,
//! `LearningStore::init`, explicit actors, and direct SQL only to hand-edit a
//! stored row for the read-side (dangling id) and history-only cases.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use rusqlite::Connection;
use serde_json::{json, Value};

use agent_memory::archive::hex_sha256;
use agent_memory::learning::model::{
    candidate_id_for, normalize_claim_for_copy_lint, validate_candidate, ORIGIN_CLAIM_COPY,
    ORIGIN_TARGET_NOT_TASK, ORIGIN_TASKS_DUPLICATE, ORIGIN_TASKS_NOT_ALLOWED,
    ORIGIN_TASKS_TOO_MANY, ORIGIN_TASK_UNKNOWN,
};
use agent_memory::learning::{
    LearningStore, DB_FILE, DEFAULT_STALE_DAYS, DOWNGRADE_BACKUP_FILE,
    DOWNGRADE_BLOCKED_ORIGIN_TASKS, DOWNGRADE_SIDECAR_FILE, TASK_REQUIRES_TASK_CREATE,
};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn evidence(tag: &str) -> Value {
    json!({
        "ref": format!("claude:session-{tag}:line-1"),
        "span_sha256": "0000000000000000000000000000000000000000000000000000000000000001",
        "archive_generation": "g1",
        "observed_at": "2026-11-01T00:00:00Z",
        "actor": "user",
        "session_kind": "interactive",
        "origin_class": "owner",
        "evidence_kind": "user_statement",
        "recall_taint": false
    })
}

/// One knowledge-candidate payload in the v1 shape (no `origin_tasks` key).
fn knowledge_payload(
    claim: &str,
    target_kind: &str,
    proposed_target: &str,
    checkpoint: &str,
) -> Value {
    json!({
        "schema_version": 1,
        "candidate_id": "",
        "revision": 1,
        "status": "candidate",
        "claim": claim,
        "target_kind": target_kind,
        "proposed_target": proposed_target,
        "applicability": ["agent-memory"],
        "counterexamples": [],
        "unknowns": [],
        "evidence": [evidence("e1")],
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
        "source_checkpoint": checkpoint,
        "extractor": "extractor-a",
        "created_at": "2026-11-01T00:00:00Z",
        "updated_at": "2026-11-01T00:00:00Z"
    })
}

fn linked<S: AsRef<str>>(mut payload: Value, origin: &[S]) -> Value {
    let ids: Vec<&str> = origin.iter().map(AsRef::as_ref).collect();
    payload["origin_tasks"] = json!(ids);
    payload
}

fn with_revision(mut payload: Value, revision: u64, updated_at: &str) -> Value {
    payload["revision"] = json!(revision);
    payload["updated_at"] = json!(updated_at);
    payload
}

/// A task-shaped payload, used only to prove that `learning ingest` still
/// refuses it and that a non-empty `origin_tasks` on it is rejected at the
/// validation layer.
fn task_shaped_payload<S: AsRef<str>>(claim: &str, origin: &[S]) -> Value {
    let ids: Vec<&str> = origin.iter().map(AsRef::as_ref).collect();
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
        "source_checkpoint": "task:fixture",
        "extractor": "task",
        "created_at": "2026-11-01T00:00:00Z",
        "updated_at": "2026-11-01T00:00:00Z",
        "origin_tasks": ids
    })
}

fn new_root() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("temp root");
    let root = dir.path().join("learning");
    LearningStore::init(&root).expect("init learning root");
    (dir, root)
}

fn ingest(store: &mut LearningStore, payload: &Value) -> String {
    store
        .ingest(payload)
        .unwrap_or_else(|error| panic!("ingest failed: {error}"))
        .candidate_id
}

fn create_task(store: &mut LearningStore, claim: &str) -> String {
    store
        .create_task(claim, "claude-code:abc")
        .unwrap_or_else(|error| panic!("task create failed: {error}"))
        .candidate_id
}

fn db_of(root: &Path) -> PathBuf {
    root.join(DB_FILE)
}

/// `learning init` creates v5; the downgrade-guard tests exercise the explicit
/// v4 -> v3 path, so reduce the fixture to the exact v4 shape first.
fn make_v4(root: &Path) {
    let conn = Connection::open(db_of(root)).expect("open v5 db");
    conn.execute_batch(
        "DROP TABLE IF EXISTS qualification_history; \
         DROP TABLE IF EXISTS rejection_overrides;",
    )
    .expect("drop v5 tables");
    conn.pragma_update(None, "user_version", 4)
        .expect("user_version 4");
}

fn user_version(db: &Path) -> i64 {
    Connection::open(db)
        .expect("open db")
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("user_version")
}

fn count(db: &Path, table: &str) -> i64 {
    Connection::open(db)
        .expect("open db")
        .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
            row.get(0)
        })
        .expect("count rows")
}

/// Directory listing with file contents: name, byte length and SHA-256 of the
/// bytes. Used to prove the refused downgrade changes nothing at all.
fn snapshot(root: &Path) -> Vec<(String, u64, String)> {
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(root).expect("read dir") {
        let entry = entry.expect("dir entry");
        let name = entry.file_name().to_string_lossy().into_owned();
        let bytes = std::fs::read(entry.path()).expect("read file");
        entries.push((name, bytes.len() as u64, hex_sha256(&bytes)));
    }
    entries.sort();
    entries
}

fn cli() -> Command {
    Command::new(env!("CARGO_BIN_EXE_agent-memory"))
}

fn run(args: &[&str]) -> Output {
    cli().args(args).output().expect("run agent-memory")
}

fn text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn stderr_code(out: &Output) -> String {
    let value: Value = serde_json::from_slice(&out.stderr).unwrap_or_else(|error| {
        panic!(
            "stderr is not JSON ({error}): {}",
            String::from_utf8_lossy(&out.stderr)
        )
    });
    value["error"]["code"]
        .as_str()
        .unwrap_or_else(|| panic!("no error code in {value}"))
        .to_string()
}

// ===========================================================================
// C7 — payload shape, identity and the write-path checks
// ===========================================================================

#[test]
fn c7_absent_origin_tasks_keeps_the_v1_bytes_identity_and_ingest_key() {
    let payload = knowledge_payload(
        "Prefer explicit roots for every store",
        "rule",
        "AGENTS.md#explicit-roots",
        "checkpoint-a",
    );
    let plain = validate_candidate(&payload).expect("v1 knowledge candidate");
    assert!(plain.record.origin_tasks.is_empty());
    assert!(
        !plain.payload_json.contains("origin_tasks"),
        "skip_serializing_if must omit the key: {}",
        plain.payload_json
    );

    // Byte-for-byte the v1 record shape, frozen at the golden identity.
    let expected = r#"{"schema_version":1,"candidate_id":"cand_bbe43ae2ef5dee078244344f0eb7d7a6","revision":1,"status":"candidate","claim":"Prefer explicit roots for every store","target_kind":"rule","proposed_target":"AGENTS.md#explicit-roots","applicability":["agent-memory"],"counterexamples":[],"unknowns":[],"evidence":[{"ref":"claude:session-e1:line-1","span_sha256":"0000000000000000000000000000000000000000000000000000000000000001","archive_generation":"g1","observed_at":"2026-11-01T00:00:00Z","actor":"user","session_kind":"interactive","origin_class":"owner","evidence_kind":"user_statement","recall_taint":false}],"contradicting_evidence":[],"dedup":{"query":"","matches":[],"disposition":"pending","checked_at":null},"reuse":[],"validation":[],"promotion":{"approval":null,"apply":null,"verification":null},"source_checkpoint":"checkpoint-a","extractor":"extractor-a","created_at":"2026-11-01T00:00:00Z","updated_at":"2026-11-01T00:00:00Z"}"#;
    assert_eq!(plain.payload_json, expected, "canonical v1 bytes");
    assert_eq!(
        plain.record.candidate_id, "cand_bbe43ae2ef5dee078244344f0eb7d7a6",
        "golden candidate_id must not move"
    );
    assert_eq!(
        plain.record.candidate_id,
        candidate_id_for(
            "Prefer explicit roots for every store",
            "rule",
            "AGENTS.md#explicit-roots"
        )
    );

    // Adding the links changes neither the identity nor the ingest key; the
    // only byte difference is the new key right after `proposed_target`
    // (`derived_from` is absent too, so its slot is skipped as well).
    let explicit_empty =
        validate_candidate(&linked(payload.clone(), &[] as &[&str])).expect("explicit empty list");
    assert_eq!(
        explicit_empty.payload_json, plain.payload_json,
        "an explicit empty list is the same value as an absent key"
    );
    let task_origin = "cand_505808a8d3c5cddfc1b1f2298387fbd9";
    let with_links = linked(payload, &[task_origin]);
    let stored = validate_candidate(&with_links).expect("linked candidate");
    assert_eq!(stored.record.origin_tasks, vec![task_origin.to_string()]);
    assert_eq!(stored.record.candidate_id, plain.record.candidate_id);
    assert_eq!(stored.ingest_key, plain.ingest_key);
    assert_eq!(
        stored.payload_json,
        plain.payload_json.replace(
            r#""proposed_target":"AGENTS.md#explicit-roots","#,
            &format!(
                r#""proposed_target":"AGENTS.md#explicit-roots","origin_tasks":["{task_origin}"],"#
            )
        ),
        "the new key is the only change"
    );
}

#[test]
fn c7_origin_link_error_codes_are_mapped_and_task_payloads_stay_refused() {
    let (_dir, root) = new_root();
    let mut store = LearningStore::open(&root).expect("open");
    let task = create_task(&mut store, "A task with reusable experience");
    let other_task = create_task(&mut store, "Another task");
    let knowledge = ingest(
        &mut store,
        &knowledge_payload("Distilled workflow", "workflow", "docs/workflow.md", "cp-a"),
    );

    let unknown = store
        .ingest(&linked(
            knowledge_payload("A distilled note", "note", "notes/a", "cp-unknown"),
            &["cand_deadbeefdeadbeefdeadbeefdeadbeef"],
        ))
        .expect_err("unknown origin");
    assert_eq!(unknown.code, ORIGIN_TASK_UNKNOWN);
    assert_eq!(unknown.exit_code(), 2);
    assert!(unknown
        .message
        .contains("cand_deadbeefdeadbeefdeadbeefdeadbeef"));

    let not_task = store
        .ingest(&linked(
            knowledge_payload("A distilled note", "note", "notes/a", "cp-not-task"),
            &[&knowledge],
        ))
        .expect_err("origin is not a task");
    assert_eq!(not_task.code, ORIGIN_TARGET_NOT_TASK);
    assert_eq!(not_task.exit_code(), 2);
    assert!(not_task.message.contains(&knowledge));

    let too_many = store
        .ingest(&linked(
            knowledge_payload("A distilled note", "note", "notes/a", "cp-too-many"),
            &[
                &task, &task, &task, &task, &task, &task, &task, &task, &task,
            ],
        ))
        .expect_err("nine origins");
    assert_eq!(too_many.code, ORIGIN_TASKS_TOO_MANY);
    assert_eq!(too_many.exit_code(), 2);
    assert!(too_many.message.contains("at most 8"));

    let duplicate = store
        .ingest(&linked(
            knowledge_payload("A distilled note", "note", "notes/a", "cp-duplicate"),
            &[&task, &other_task, &task],
        ))
        .expect_err("duplicate origins");
    assert_eq!(duplicate.code, ORIGIN_TASKS_DUPLICATE);
    assert_eq!(duplicate.exit_code(), 2);
    assert!(duplicate.message.contains(&task));

    // A task payload may never carry the field: the shape check rejects it…
    let task_payload = task_shaped_payload("A task that carries links", &[&task]);
    let not_allowed = validate_candidate(&task_payload).expect_err("task payload with links");
    assert_eq!(not_allowed.code, ORIGIN_TASKS_NOT_ALLOWED);
    assert_eq!(not_allowed.exit_code(), 2);
    let store_error = store
        .ingest(&task_payload)
        .expect_err("task payload via ingest");
    assert_eq!(store_error.code, ORIGIN_TASKS_NOT_ALLOWED);

    // …while the CLI keeps its existing, earlier refusal for any task payload
    // (`learning ingest` never creates tasks, with or without the field).
    let out = run(&[
        "learning",
        "ingest",
        "--root",
        &text(&root),
        "--candidate",
        &task_payload.to_string(),
    ]);
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(stderr_code(&out), TASK_REQUIRES_TASK_CREATE);
    assert_eq!(
        count(&db_of(&root), "candidates"),
        3,
        "no refused ingest wrote a row"
    );
}

#[test]
fn c7_changing_origin_tasks_requires_revision_plus_one() {
    let (_dir, root) = new_root();
    let mut store = LearningStore::open(&root).expect("open");
    let task_a = create_task(&mut store, "Task A with a lesson");
    let task_b = create_task(&mut store, "Task B with a lesson");

    let rev1 = linked(
        knowledge_payload("Distilled workflow", "workflow", "docs/workflow.md", "cp-1"),
        &[&task_a],
    );
    let id = ingest(&mut store, &rev1);
    assert_eq!(
        store.show(&id).expect("show").candidate.origin_tasks,
        vec![task_a.clone()]
    );

    // Same revision, different list → the existing same-revision conflict.
    let same_revision = linked(
        knowledge_payload("Distilled workflow", "workflow", "docs/workflow.md", "cp-2"),
        &[&task_b],
    );
    let conflict = store.ingest(&same_revision).expect_err("same revision");
    assert_eq!(conflict.code, "CANDIDATE_REVISION_CONFLICT");
    assert_eq!(conflict.exit_code(), 2);
    assert_eq!(
        store.show(&id).expect("show").candidate.origin_tasks,
        vec![task_a.clone()],
        "the refused write changed nothing"
    );

    // Revision + 1 is the sanctioned way to move the link.
    let rev2 = with_revision(
        linked(
            knowledge_payload("Distilled workflow", "workflow", "docs/workflow.md", "cp-3"),
            &[&task_b],
        ),
        2,
        "2026-11-01T00:00:05Z",
    );
    store.ingest(&rev2).expect("revision 2 accepted");
    let view = store.show(&id).expect("show");
    assert_eq!(view.candidate.revision, 2);
    assert_eq!(view.candidate.origin_tasks, vec![task_b.clone()]);
    assert_eq!(view.revisions.len(), 2);
}

#[test]
fn c7_claim_copy_lint_hits_normalization_variants_and_spares_rewrites() {
    let (_dir, root) = new_root();
    let mut store = LearningStore::open(&root).expect("open");
    let task = create_task(&mut store, "Deploy  the Full-Width Check");

    let variants = [
        // exact
        "Deploy  the Full-Width Check",
        // case + whitespace collapse
        "deploy the full-width check",
        // fullwidth ASCII + ideographic space + repeated spaces
        "Ｄｅｐｌｏｙ\u{3000}ｔｈｅ  Ｆｕｌｌ－Ｗｉｄｔｈ   Ｃｈｅｃｋ",
        // leading/trailing whitespace only
        "  Deploy the Full-Width Check  ",
    ];
    for (index, claim) in variants.iter().enumerate() {
        let payload = linked(
            knowledge_payload(
                claim,
                "workflow",
                &format!("docs/copy-{index}.md"),
                &format!("cp-copy-{index}"),
            ),
            &[&task],
        );
        let error = store.ingest(&payload).expect_err("claim copy");
        assert_eq!(error.code, ORIGIN_CLAIM_COPY, "variant {index}: {claim:?}");
        assert_eq!(error.exit_code(), 2);
        assert!(error.message.contains(&task));
    }

    // The lint is exactly the documented normalization, not something broader.
    assert_eq!(
        normalize_claim_for_copy_lint("  Ｄｅｐｌｏｙ\u{3000} TASK  "),
        normalize_claim_for_copy_lint("deploy task")
    );

    // A rewritten claim (same meaning, different sentence) does not hit.
    let rewritten = linked(
        knowledge_payload(
            "Roll out the release checklist before tagging",
            "workflow",
            "docs/release.md",
            "cp-rewrite",
        ),
        &[&task],
    );
    store.ingest(&rewritten).expect("a rewrite is not a copy");

    // The negative case is visible from the store too: only the rewrite exists
    // as a knowledge candidate pointing at the task.
    let workspace = store
        .workspace(None, chrono::Utc::now().fixed_offset(), DEFAULT_STALE_DAYS)
        .expect("workspace");
    let row = workspace
        .tasks
        .iter()
        .find(|row| row.candidate_id == task)
        .expect("task row");
    assert_eq!(row.distilled_into.len(), 1);
}

// ===========================================================================
// C7 — read side: `distilled_into`
// ===========================================================================

#[test]
fn c7_distilled_into_is_a_sorted_reverse_index_on_both_read_surfaces() {
    let (_dir, root) = new_root();
    let mut store = LearningStore::open(&root).expect("open");
    let task_a = create_task(&mut store, "Task A");
    let task_b = create_task(&mut store, "Task B");
    let task_c = create_task(&mut store, "Task C with no distillation");

    // Deliberately unsorted link lists; the read side sorts.
    let first = ingest(
        &mut store,
        &linked(
            knowledge_payload(
                "Distilled workflow one",
                "workflow",
                "docs/one.md",
                "cp-one",
            ),
            &[&task_b, &task_a],
        ),
    );
    let second = ingest(
        &mut store,
        &linked(
            knowledge_payload(
                "Distilled workflow two",
                "workflow",
                "docs/two.md",
                "cp-two",
            ),
            &[&task_a],
        ),
    );
    let mut expected_a = vec![first.clone(), second.clone()];
    expected_a.sort();

    let now = chrono::Utc::now().fixed_offset();
    let workspace = store
        .workspace(None, now, DEFAULT_STALE_DAYS)
        .expect("workspace");
    let row = |id: &str| {
        workspace
            .tasks
            .iter()
            .find(|row| row.candidate_id == id)
            .unwrap_or_else(|| panic!("no workspace row for {id}"))
    };
    assert_eq!(row(&task_a).distilled_into, expected_a);
    assert_eq!(row(&task_b).distilled_into, vec![first.clone()]);
    assert_eq!(row(&task_c).distilled_into, Vec::<String>::new());

    let show_a = store
        .task_show(&task_a, now, DEFAULT_STALE_DAYS)
        .expect("task show");
    assert_eq!(show_a.distilled_into, expected_a);
    let show_c = store
        .task_show(&task_c, now, DEFAULT_STALE_DAYS)
        .expect("task show");
    assert!(show_c.distilled_into.is_empty());

    // Text surfaces: `task show --text` gains the optional line, `workspace
    // --text` is unchanged (no distilled line at all).
    let root_text = text(&root);
    let show_out = run(&[
        "learning",
        "task",
        "show",
        "--root",
        &root_text,
        "--candidate-id",
        &task_a,
        "--text",
    ]);
    assert_eq!(show_out.status.code(), Some(0));
    let show_text = String::from_utf8_lossy(&show_out.stdout).into_owned();
    assert!(
        show_text.contains(&format!("distilled_into: {}\n", expected_a.join(", "))),
        "task show text: {show_text}"
    );

    let workspace_out = run(&["learning", "workspace", "--root", &root_text, "--text"]);
    assert_eq!(workspace_out.status.code(), Some(0));
    let workspace_text = String::from_utf8_lossy(&workspace_out.stdout).into_owned();
    assert!(!workspace_text.contains("distilled_into"));
}

#[test]
fn c7_distilled_into_scales_to_one_hundred_tasks_and_fifty_linked_candidates() {
    let (_dir, root) = new_root();
    let mut store = LearningStore::open(&root).expect("open");

    let tasks: Vec<String> = (0..100)
        .map(|index| create_task(&mut store, &format!("Scale task {index:03}")))
        .collect();

    // 50 knowledge candidates; candidate i names tasks (i, (i + 37) % 100) and
    // the first ten also name task 0.
    let mut expected: std::collections::BTreeMap<String, Vec<String>> =
        std::collections::BTreeMap::new();
    for index in 0..50usize {
        let mut origin = vec![tasks[index].clone(), tasks[(index + 37) % 100].clone()];
        if index < 10 {
            origin.push(tasks[0].clone());
        }
        origin.sort();
        origin.dedup();
        let refs: Vec<&str> = origin.iter().map(String::as_str).collect();
        let id = ingest(
            &mut store,
            &linked(
                knowledge_payload(
                    &format!("Scale distillation {index:03}"),
                    "workflow",
                    &format!("docs/scale-{index:03}.md"),
                    &format!("cp-scale-{index:03}"),
                ),
                &refs,
            ),
        );
        for task in origin {
            expected.entry(task).or_default().push(id.clone());
        }
    }
    for ids in expected.values_mut() {
        ids.sort();
    }

    let report = store
        .workspace(None, chrono::Utc::now().fixed_offset(), DEFAULT_STALE_DAYS)
        .expect("workspace");
    assert_eq!(report.total, 100);
    for row in &report.tasks {
        let want = expected.get(&row.candidate_id).cloned().unwrap_or_default();
        assert_eq!(
            row.distilled_into, want,
            "distilled_into for {}",
            row.candidate_id
        );
    }
    assert_eq!(
        report
            .tasks
            .iter()
            .map(|row| row.distilled_into.len())
            .sum::<usize>(),
        expected.values().map(Vec::len).sum::<usize>()
    );
}

#[test]
fn c7_a_dangling_origin_id_never_breaks_the_read_side() {
    let (_dir, root) = new_root();
    let mut store = LearningStore::open(&root).expect("open");
    let task = create_task(&mut store, "A task whose knowledge will dangle");
    let knowledge = ingest(
        &mut store,
        &linked(
            knowledge_payload("Distilled workflow", "workflow", "docs/w.md", "cp-dangle"),
            &[&task],
        ),
    );
    drop(store);

    // Hand-edit the stored row so the link points nowhere (the shape the read
    // side must tolerate; the write path would refuse it).
    {
        let db = Connection::open(db_of(&root)).expect("open db");
        let raw: String = db
            .query_row(
                "SELECT record_json FROM candidates WHERE candidate_id = ?1",
                rusqlite::params![knowledge],
                |row| row.get(0),
            )
            .expect("record_json");
        let mut value: Value = serde_json::from_str(&raw).expect("parse");
        value["origin_tasks"] = json!(["cand_00000000000000000000000000000000"]);
        db.execute(
            "UPDATE candidates SET record_json = ?2 WHERE candidate_id = ?1",
            rusqlite::params![knowledge, value.to_string()],
        )
        .expect("update");
    }

    let root_text = text(&root);
    let workspace_out = run(&["learning", "workspace", "--root", &root_text, "--json"]);
    assert_eq!(workspace_out.status.code(), Some(0));
    let workspace: Value = serde_json::from_slice(&workspace_out.stdout).expect("workspace json");
    assert_eq!(workspace["tasks"].as_array().expect("tasks").len(), 1);
    assert_eq!(workspace["tasks"][0]["candidate_id"], json!(task));
    assert_eq!(
        workspace["tasks"][0]["distilled_into"],
        json!([]),
        "the dangling id maps to no task row"
    );

    let show_out = run(&[
        "learning",
        "task",
        "show",
        "--root",
        &root_text,
        "--candidate-id",
        &task,
    ]);
    assert_eq!(show_out.status.code(), Some(0));

    let candidate_out = run(&[
        "learning",
        "show",
        "--root",
        &root_text,
        "--candidate-id",
        &knowledge,
    ]);
    assert_eq!(candidate_out.status.code(), Some(0));
    let shown: Value = serde_json::from_slice(&candidate_out.stdout).expect("show json");
    assert_eq!(
        shown["candidate"]["origin_tasks"],
        json!(["cand_00000000000000000000000000000000"])
    );

    let export_out = run(&[
        "learning",
        "export",
        "--root",
        &root_text,
        "--candidate-id",
        &knowledge,
    ]);
    assert_eq!(
        export_out.status.code(),
        Some(0),
        "export must not fail on a dangling origin id: {}",
        String::from_utf8_lossy(&export_out.stderr)
    );
}

// ===========================================================================
// C11 — the downgrade guard
// ===========================================================================

#[test]
fn c11_downgrade_is_refused_before_any_write_when_a_record_carries_origin_tasks() {
    let (_dir, root) = new_root();
    let mut store = LearningStore::open(&root).expect("open");
    let task = create_task(&mut store, "A task that was distilled");
    let knowledge = ingest(
        &mut store,
        &linked(
            knowledge_payload("Distilled workflow", "workflow", "docs/w.md", "cp-guard"),
            &[&task],
        ),
    );
    drop(store);
    make_v4(&root);

    let db = db_of(&root);
    let before_dir = snapshot(&root);
    let before_bytes = std::fs::read(&db).expect("read db");
    let before_candidates = count(&db, "candidates");
    let before_revisions = count(&db, "candidate_revisions");
    assert_eq!(user_version(&db), 4);

    let error = LearningStore::downgrade_to_v3(&root).expect_err("downgrade must be refused");
    assert_eq!(error.code, DOWNGRADE_BLOCKED_ORIGIN_TASKS);
    assert_eq!(error.exit_code(), 2);
    assert!(
        error.message.contains('1') && error.message.contains(&knowledge),
        "message lists the count and the candidate id: {}",
        error.message
    );

    // Zero change: same files with the same bytes, no backup pair, same version
    // and row counts.
    assert_eq!(snapshot(&root), before_dir, "the root directory changed");
    assert_eq!(
        std::fs::read(&db).expect("read db"),
        before_bytes,
        "learning.db changed"
    );
    assert!(!root.join(DOWNGRADE_BACKUP_FILE).exists());
    assert!(!root.join(DOWNGRADE_SIDECAR_FILE).exists());
    assert_eq!(user_version(&db), 4);
    assert_eq!(count(&db, "candidates"), before_candidates);
    assert_eq!(count(&db, "candidate_revisions"), before_revisions);

    // CLI surface: same code, same refusal, still nothing written.
    let out = run(&["learning", "migrate", "--root", &text(&root), "--to", "3"]);
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(stderr_code(&out), DOWNGRADE_BLOCKED_ORIGIN_TASKS);
    assert_eq!(snapshot(&root), before_dir);
    assert_eq!(std::fs::read(&db).expect("read db"), before_bytes);
}

#[test]
fn c11_downgrade_is_blocked_even_when_only_a_revision_snapshot_carries_the_key() {
    let (_dir, root) = new_root();
    let mut store = LearningStore::open(&root).expect("open");
    let task = create_task(&mut store, "A task distilled in revision one");
    let rev1 = linked(
        knowledge_payload("Distilled workflow", "workflow", "docs/w.md", "cp-rev1"),
        &[&task],
    );
    let id = ingest(&mut store, &rev1);
    // Revision two drops the link: the *current* record no longer carries the
    // key, only the historical snapshot does.
    let rev2 = with_revision(
        knowledge_payload("Distilled workflow", "workflow", "docs/w.md", "cp-rev2"),
        2,
        "2026-11-01T00:00:05Z",
    );
    store.ingest(&rev2).expect("revision 2 without links");
    drop(store);
    make_v4(&root);

    let db = db_of(&root);
    let current: String = Connection::open(&db)
        .expect("open db")
        .query_row(
            "SELECT record_json FROM candidates WHERE candidate_id = ?1",
            rusqlite::params![id],
            |row| row.get(0),
        )
        .expect("current record");
    assert!(!current.contains("origin_tasks"));
    let historical: i64 = Connection::open(&db)
        .expect("open db")
        .query_row(
            "SELECT COUNT(*) FROM candidate_revisions WHERE record_json LIKE '%origin_tasks%'",
            [],
            |row| row.get(0),
        )
        .expect("historical count");
    assert_eq!(historical, 1);

    let before = snapshot(&root);
    let error = LearningStore::downgrade_to_v3(&root).expect_err("history blocks the downgrade");
    assert_eq!(error.code, DOWNGRADE_BLOCKED_ORIGIN_TASKS);
    assert!(error.message.contains(&id), "{}", error.message);
    assert_eq!(snapshot(&root), before);
}

#[test]
fn c11_downgrade_without_origin_tasks_keeps_the_existing_behaviour() {
    let (_dir, root) = new_root();
    let mut store = LearningStore::open(&root).expect("open");
    let knowledge = ingest(
        &mut store,
        &knowledge_payload("Kept on downgrade", "workflow", "docs/kept.md", "cp-kept"),
    );
    let task = create_task(&mut store, "A task dropped by the downgrade");
    drop(store);
    make_v4(&root);

    let db = db_of(&root);
    let report = LearningStore::downgrade_to_v3(&root).expect("a store without links downgrades");
    assert!(report.ok);
    assert_eq!(report.from_version, 4);
    assert_eq!(report.to_version, 3);
    assert_eq!(report.dropped.candidates, 1);
    assert_eq!(report.kept.candidates, 1);
    assert!(root.join(DOWNGRADE_BACKUP_FILE).is_file());
    assert!(root.join(DOWNGRADE_SIDECAR_FILE).is_file());
    assert_eq!(user_version(&db), 3);
    let survivor: String = Connection::open(&db)
        .expect("open downgraded db")
        .query_row("SELECT candidate_id FROM candidates", [], |row| row.get(0))
        .expect("surviving candidate");
    assert_eq!(survivor, knowledge);
    assert_ne!(survivor, task);
}
