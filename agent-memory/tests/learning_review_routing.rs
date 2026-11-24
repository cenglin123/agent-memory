//! Stage 6 implementation-004: review routing (§C).
//!
//! Acceptance coverage: C6 — every conflict-actor source enters the set and is
//! normalized/deduped, an origin task's validation reviewer never enters it,
//! `related_tasks` carries the current projection plus `missing`, the three
//! related-candidate paths, the rule package's `promotion_path`, the task
//! candidate's `TASK_NOT_ROUTEABLE`, the knowledge-candidate
//! `REVIEWER_CONFLICT` guard (positive and negative, zero writes), the
//! `[self-reflect]` task guard (`REVIEWER_CONFLICT`, `REPORT_REF_INVALID`), and
//! the derived `self_validated` flag.
//!
//! Every case builds a synthetic learning root under `tempfile`; no real
//! learning root, Archive or recall configuration is read. Fixture conventions
//! follow the stage-6 checkpoints: explicit actors, direct SQL only to hand-edit
//! a stored row that the write path cannot produce (a dangling origin id).

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use rusqlite::{params, Connection};
use serde_json::{json, Value};

use agent_memory::learning::{
    LearningStore, DB_FILE, DEFAULT_STALE_DAYS, REFLECT_CLAIM_PREFIX, RELATED_TASK_MISSING,
    REPORT_REF_INVALID, REVIEWER_CONFLICT, REVIEWER_REQUIREMENTS, TASK_NOT_ROUTEABLE,
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

fn new_root() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("temp dir");
    let root = dir.path().join("learning");
    drop(LearningStore::init(&root).expect("init learning root"));
    (dir, root)
}

fn text(root: &Path) -> String {
    root.to_string_lossy().to_string()
}

fn db_of(root: &Path) -> PathBuf {
    root.join(DB_FILE)
}

fn connect(root: &Path) -> Connection {
    Connection::open(db_of(root)).expect("open learning.db")
}

fn count(root: &Path, table: &str) -> i64 {
    connect(root)
        .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
            row.get(0)
        })
        .expect("count rows")
}

fn payload_sha256(root: &Path, candidate_id: &str) -> String {
    connect(root)
        .query_row(
            "SELECT payload_sha256 FROM candidates WHERE candidate_id = ?1",
            params![candidate_id],
            |row| row.get(0),
        )
        .expect("payload sha")
}

// ---------------------------------------------------------------------------
// Write helpers
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

fn abandon_task(root: &Path, candidate_id: &str, actor: &str, reason: &str) {
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
            reason,
        ]),
        "task abandon",
    );
}

fn reuse(root: &Path, candidate_id: &str, task: &str, actor: &str) {
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
            task,
            "--outcome",
            "pass",
            "--actor",
            actor,
        ]),
        "reuse",
    );
}

fn validate_out(
    root: &Path,
    candidate_id: &str,
    reviewer: &str,
    verdict: &str,
    report_ref: &str,
) -> Output {
    let root_text = text(root);
    run(&[
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
    ])
}

fn validate_ok(root: &Path, candidate_id: &str, reviewer: &str, report_ref: &str) -> Value {
    let out = validate_out(root, candidate_id, reviewer, "pass", report_ref);
    assert_ok(&out, "validate")
}

fn evidence(tag: &str, actor: &str) -> Value {
    json!({
        "ref": format!("claude:session-{tag}:line-1"),
        "span_sha256": "0".repeat(64),
        "archive_generation": "g1",
        "observed_at": "2026-11-01T00:00:00Z",
        "actor": actor,
        "session_kind": "interactive",
        "origin_class": "owner",
        "evidence_kind": "user_statement",
        "recall_taint": false
    })
}

fn knowledge_payload(
    claim: &str,
    extractor: &str,
    checkpoint: &str,
    evidence_list: Vec<Value>,
    origin_tasks: Option<&[&str]>,
) -> Value {
    let mut payload = json!({
        "schema_version": 1,
        "candidate_id": "",
        "revision": 1,
        "status": "candidate",
        "claim": claim,
        "target_kind": "workflow",
        "proposed_target": "AGENTS.md#workflow",
        "applicability": ["agent-memory"],
        "counterexamples": [],
        "unknowns": [],
        "evidence": evidence_list,
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
        "extractor": extractor,
        "created_at": "2026-11-01T00:00:00Z",
        "updated_at": "2026-11-01T00:00:00Z"
    });
    if let Some(origin) = origin_tasks {
        payload["origin_tasks"] = json!(origin);
    }
    payload
}

fn ingest(root: &Path, payload: &Value) -> String {
    let mut store = LearningStore::open(root).expect("open learning root");
    store
        .ingest(payload)
        .unwrap_or_else(|error| panic!("ingest failed: {error}"))
        .candidate_id
}

/// Append one `origin_tasks` entry directly to a stored record. The ingest path
/// validates links, so a dangling id is only reachable through a hand-edited
/// row (the read side must not fail on it).
/// Insert one validation row directly, bypassing the §C guard. Only the
/// read-side watermark fold is exercised with it.
fn insert_validation_row(root: &Path, candidate_id: &str, reviewer: &str) {
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
        params![event_seq],
    )
    .expect("clock row");
    conn.execute(
        "INSERT INTO validation_history \
         (event_seq, candidate_id, reviewer, verdict, report_ref, recorded_at) \
         VALUES (?1, ?2, ?3, 'pass', 'report.md', '2026-01-01T00:00:00.000Z')",
        params![event_seq, candidate_id, reviewer],
    )
    .expect("insert validation row");
}

fn push_origin_id(root: &Path, candidate_id: &str, origin: &str) {
    let conn = connect(root);
    let record_json: String = conn
        .query_row(
            "SELECT record_json FROM candidates WHERE candidate_id = ?1",
            params![candidate_id],
            |row| row.get(0),
        )
        .expect("stored record");
    let mut value: Value = serde_json::from_str(&record_json).expect("record json");
    let list = value
        .get_mut("origin_tasks")
        .and_then(Value::as_array_mut)
        .expect("origin_tasks array");
    list.push(Value::String(origin.to_string()));
    conn.execute(
        "UPDATE candidates SET record_json = ?2 WHERE candidate_id = ?1",
        params![candidate_id, value.to_string()],
    )
    .expect("hand-edited record");
}

fn export(root: &Path, candidate_id: &str) -> Value {
    let root_text = text(root);
    assert_ok(
        &run(&[
            "learning",
            "export",
            "--root",
            &root_text,
            "--candidate-id",
            candidate_id,
        ]),
        "export",
    )
}

fn routing_of(package: &Value) -> &Value {
    package
        .get("review_routing")
        .unwrap_or_else(|| panic!("review package has no review_routing: {package}"))
}

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .unwrap_or_else(|| panic!("expected an array: {value}"))
        .iter()
        .map(|item| item.as_str().expect("string item").to_string())
        .collect()
}

// ---------------------------------------------------------------------------
// C6: conflict_actors
// ---------------------------------------------------------------------------

#[test]
fn c6_conflict_actors_cover_every_source_and_fold_variants() {
    let (_dir, root) = new_root();

    // The origin task: created by one actor, taken over by another, applied by
    // a third, and reviewed by a fourth (the reviewer must stay out).
    let origin = create_task(&root, "origin task one", "Creator-A");
    claim_task(&root, &origin, "Owner-B");
    reuse(&root, &origin, &origin, "Task-Reuse-Actor");
    validate_ok(&root, &origin, "Origin-Reviewer", "origin-report.md");

    // The knowledge candidate. Two ingestions carry two different extractors;
    // the second evidence item only differs by fullwidth characters and case.
    let first = knowledge_payload(
        "A workflow distilled from the origin task",
        "Extractor-A",
        "checkpoint-a",
        vec![evidence("e1", "Evidence-Actor")],
        Some(&[origin.as_str()]),
    );
    let id = ingest(&root, &first);
    let mut second = first.clone();
    second["source_checkpoint"] = json!("checkpoint-b");
    second["extractor"] = json!("Second-Extractor");
    second["updated_at"] = json!("2026-11-01T01:00:00Z");
    second["evidence"] = json!([
        evidence("e1", "Evidence-Actor"),
        evidence("e2", "ｅｖｉｄｅｎｃｅ－ａｃｔｏｒ"),
    ]);
    ingest(&root, &second);
    assert_eq!(
        count(&root, "ingestions"),
        3,
        "task + two knowledge ingestions"
    );

    // Two reuse events by two case variants of the same actor.
    reuse(&root, &id, "task-a", "Reuse-Actor");
    reuse(&root, &id, "task-b", "reuse-actor");

    let package = export(&root, &id);
    let routing = routing_of(&package);
    let actors = strings(&routing["conflict_actors"]);
    assert_eq!(
        actors,
        vec![
            "creator-a".to_string(),
            "evidence-actor".to_string(),
            "extractor-a".to_string(),
            "owner-b".to_string(),
            "reuse-actor".to_string(),
            "second-extractor".to_string(),
            "task-reuse-actor".to_string(),
        ],
        "every source enters once, normalized and sorted: {actors:?}"
    );
    assert!(
        !actors.iter().any(|actor| actor == "origin-reviewer"),
        "an origin task's validation reviewer is not a task participant: {actors:?}"
    );

    // The exported set is the same single definition the write guard uses.
    let store = LearningStore::open(&root).expect("reopen");
    let direct = store
        .review_routing(
            &id,
            chrono::DateTime::parse_from_rfc3339("2026-11-01T02:00:00Z")
                .expect("now")
                .fixed_offset(),
            DEFAULT_STALE_DAYS,
        )
        .expect("direct routing");
    assert_eq!(direct.conflict_actors, actors);
    assert_eq!(
        direct.reviewer_requirements.to_vec(),
        REVIEWER_REQUIREMENTS.to_vec()
    );
}

// ---------------------------------------------------------------------------
// C6: related_tasks
// ---------------------------------------------------------------------------

#[test]
fn c6_related_tasks_carry_state_claim_and_missing() {
    let (_dir, root) = new_root();
    let done = create_task(&root, "a finished task", "claude-code:creator");
    validate_ok(&root, &done, "reviewer:one", "report-1");
    let active = create_task(&root, "an active task", "claude-code:creator");
    let abandoned = create_task(&root, "an abandoned task", "claude-code:creator");
    abandon_task(&root, &abandoned, "claude-code:creator", "scope moved");

    let payload = knowledge_payload(
        "A workflow linked to several tasks",
        "Extractor-A",
        "checkpoint-a",
        vec![evidence("e1", "user")],
        Some(&[done.as_str(), abandoned.as_str()]),
    );
    let id = ingest(&root, &payload);
    // A reuse on a *task* candidate is listed even without an origin link; a
    // free-text reuse task id is historical and must not be listed.
    reuse(&root, &id, &active, "applier:one");
    reuse(&root, &id, "release-1", "applier:one");

    // A dangling origin id is only reachable through a hand-edited row.
    push_origin_id(&root, &id, "cand_dangling_origin");

    let package = export(&root, &id);
    let rows = routing_of(&package)["related_tasks"]
        .as_array()
        .expect("related_tasks")
        .clone();
    let ids: Vec<&str> = rows
        .iter()
        .map(|row| row["task_id"].as_str().expect("task_id"))
        .collect();
    assert_eq!(ids.len(), 4, "done, active, abandoned and the dangling id");
    let mut sorted = ids.clone();
    sorted.sort();
    assert_eq!(ids, sorted, "sorted by task id");
    for wanted in [done.as_str(), active.as_str(), abandoned.as_str()] {
        assert!(ids.contains(&wanted), "missing {wanted}: {ids:?}");
    }
    assert!(ids.contains(&"cand_dangling_origin"));
    assert!(
        !ids.iter().any(|id| *id == "release-1"),
        "free-text reuse tasks are not listed"
    );
    let by_id = |wanted: &str| {
        rows.iter()
            .find(|row| row["task_id"] == wanted)
            .unwrap_or_else(|| panic!("missing related task {wanted}"))
    };
    assert_eq!(by_id(&done)["state"], json!("done"));
    assert_eq!(by_id(&done)["claim"], json!("a finished task"));
    assert_eq!(by_id(&active)["state"], json!("active"));
    assert_eq!(by_id(&active)["claim"], json!("an active task"));
    assert_eq!(by_id(&abandoned)["state"], json!("abandoned"));
    let missing = by_id("cand_dangling_origin");
    assert_eq!(missing["state"], json!(RELATED_TASK_MISSING));
    assert!(
        missing.get("claim").is_some() && missing["claim"].is_null(),
        "a missing id still carries claim: null: {missing}"
    );
    assert!(
        !rows.iter().any(|row| row["task_id"] == "release-1"),
        "free-text reuse tasks are not listed"
    );
}

// ---------------------------------------------------------------------------
// C6: related_candidates
// ---------------------------------------------------------------------------

#[test]
fn c6_related_candidates_use_origin_reuse_and_evidence_paths() {
    let (_dir, root) = new_root();
    let shared_task = create_task(&root, "the shared origin task", "claude-code:creator");

    let target = ingest(
        &root,
        &knowledge_payload(
            "The candidate under review",
            "extractor-a",
            "checkpoint-target",
            vec![evidence("shared-ref", "user")],
            Some(&[shared_task.as_str()]),
        ),
    );
    let same_origin = ingest(
        &root,
        &knowledge_payload(
            "A neighbour sharing the origin task",
            "extractor-a",
            "checkpoint-origin",
            vec![evidence("origin-only", "user")],
            Some(&[shared_task.as_str()]),
        ),
    );
    let same_evidence = ingest(
        &root,
        &knowledge_payload(
            "A neighbour sharing an evidence ref",
            "extractor-a",
            "checkpoint-evidence",
            vec![evidence("shared-ref", "user"), evidence("other", "user")],
            None,
        ),
    );
    let same_reuse = ingest(
        &root,
        &knowledge_payload(
            "A neighbour sharing a reuse task",
            "extractor-a",
            "checkpoint-reuse",
            vec![evidence("reuse-only", "user")],
            None,
        ),
    );
    // A task candidate is never a related candidate.
    let unrelated = ingest(
        &root,
        &knowledge_payload(
            "A completely unrelated candidate",
            "extractor-a",
            "checkpoint-unrelated",
            vec![evidence("unrelated", "user")],
            None,
        ),
    );

    reuse(&root, &target, "shared-reuse-run", "applier:one");
    reuse(&root, &same_reuse, "shared-reuse-run", "applier:one");

    let package = export(&root, &target);
    let related = strings(&routing_of(&package)["related_candidates"]);
    let mut expected = vec![
        same_origin.clone(),
        same_evidence.clone(),
        same_reuse.clone(),
    ];
    expected.sort();
    assert_eq!(related, expected, "all three relatedness paths, sorted");
    assert!(!related.contains(&target), "self is excluded");
    assert!(
        !related.contains(&shared_task),
        "task candidates are excluded"
    );
    assert!(
        !related.contains(&unrelated),
        "unrelated candidates stay out"
    );
}

// ---------------------------------------------------------------------------
// C6: rule package and task candidate
// ---------------------------------------------------------------------------

#[test]
fn c6_rule_package_keeps_promotion_path_and_tasks_stay_unrouteable() {
    let (_dir, root) = new_root();
    let rule = knowledge_payload(
        "A governance rule proposal",
        "extractor-a",
        "checkpoint-rule",
        vec![evidence("rule-ref", "user")],
        None,
    );
    let mut rule = rule;
    rule["target_kind"] = json!("rule");
    rule["proposed_target"] = json!("AGENTS.md#rule");
    let rule_id = ingest(&root, &rule);

    let package = export(&root, &rule_id);
    assert_eq!(package["decision_required"], json!("human approval"));
    assert_eq!(package["promotion_path"], json!("none — proposal only"));
    assert_eq!(package["target_report"]["verdict"], json!("proposal_only"));
    let routing = routing_of(&package);
    for key in [
        "conflict_actors",
        "prior_reviewers",
        "related_tasks",
        "related_candidates",
        "context_refs",
        "reviewer_requirements",
    ] {
        assert!(
            routing.get(key).is_some(),
            "review_routing is missing {key}: {routing}"
        );
    }
    assert_eq!(
        routing["reviewer_requirements"],
        json!(REVIEWER_REQUIREMENTS)
    );
    assert_eq!(
        routing["context_refs"],
        json!(vec!["claude:session-rule-ref:line-1"])
    );

    // A task candidate still has no review package at all (R11).
    let task = create_task(&root, "a task that is never routed", "claude-code:creator");
    let task_export = export(&root, &task);
    assert_eq!(task_export["ok"], json!(false));
    assert_eq!(task_export["blocking"], json!([TASK_NOT_ROUTEABLE]));
    assert!(
        task_export.get("candidate").is_none(),
        "a task candidate has no review package: {task_export}"
    );
    assert!(
        task_export.get("review_routing").is_none(),
        "no routing material is produced for a task candidate"
    );
}

// ---------------------------------------------------------------------------
// C6: knowledge-candidate self-review guard
// ---------------------------------------------------------------------------

#[test]
fn c6_knowledge_reviewer_conflicts_are_refused_with_zero_writes() {
    let (_dir, root) = new_root();
    let origin = create_task(&root, "origin task for the guard", "Creator-A");
    claim_task(&root, &origin, "Owner-B");
    reuse(&root, &origin, &origin, "Task-Reuse-Actor");
    validate_ok(&root, &origin, "Origin-Reviewer", "origin-report.md");

    let first = knowledge_payload(
        "A guard fixture workflow",
        "Extractor-A",
        "checkpoint-a",
        vec![evidence("e1", "Evidence-Actor")],
        Some(&[origin.as_str()]),
    );
    let id = ingest(&root, &first);
    let mut second = first.clone();
    second["source_checkpoint"] = json!("checkpoint-b");
    second["extractor"] = json!("Second-Extractor");
    second["updated_at"] = json!("2026-11-01T01:00:00Z");
    ingest(&root, &second);
    reuse(&root, &id, "task-a", "Reuse-Actor");

    let before_sha = payload_sha256(&root, &id);
    let before_rows = count(&root, "validation_history");
    let before_candidates = count(&root, "candidates");

    // Every conflict-actor source, including trim/case/fullwidth variants.
    let refused = [
        "extractor-a",
        " Extractor-A ",
        "ｅｘｔｒａｃｔｏｒ－ａ",
        "second-extractor",
        "evidence-actor",
        "ＥＶＩＤＥＮＣＥ－ＡＣＴＯＲ",
        "reuse-actor",
        "creator-a",
        "owner-b",
        "task-reuse-actor",
    ];
    for reviewer in refused {
        let out = validate_out(&root, &id, reviewer, "pass", "report.md");
        assert_eq!(
            out.status.code(),
            Some(2),
            "reviewer {reviewer:?} must be refused: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(
            stderr_json(&out)["error"]["code"],
            json!(REVIEWER_CONFLICT),
            "reviewer {reviewer:?}"
        );
    }
    assert_eq!(
        count(&root, "validation_history"),
        before_rows,
        "zero writes"
    );
    assert_eq!(count(&root, "candidates"), before_candidates);
    assert_eq!(payload_sha256(&root, &id), before_sha);

    // The origin task's own reviewer is *not* a participant of this candidate:
    // that review is accepted.
    validate_ok(&root, &id, "Origin-Reviewer", "report-origin.md");
    assert_eq!(count(&root, "validation_history"), before_rows + 1);
    // An unrelated reviewer passes too.
    validate_ok(&root, &id, "independent-reviewer", "report-independent.md");
    assert_eq!(count(&root, "validation_history"), before_rows + 2);

    let package = export(&root, &id);
    assert_eq!(
        strings(&routing_of(&package)["prior_reviewers"]),
        vec![
            "Origin-Reviewer".to_string(),
            "independent-reviewer".to_string()
        ]
    );
}

// ---------------------------------------------------------------------------
// C6: [self-reflect] task guard
// ---------------------------------------------------------------------------

#[test]
fn c6_reflect_task_refuses_self_review_and_invalid_report_paths() {
    let (dir, root) = new_root();
    let claim = format!("{REFLECT_CLAIM_PREFIX} 协调层审计");
    let reflect = create_task(&root, &claim, "reflect:creator");
    let before_rows = count(&root, "validation_history");

    let valid_report = dir.path().join("reflect-report.md");
    std::fs::write(&valid_report, "# synthetic reflect report\n").expect("write report");
    let valid_text = valid_report.to_string_lossy().to_string();

    // Self-review, including the folded variant.
    for reviewer in ["reflect:creator", "  REFLECT:CREATOR  "] {
        let out = validate_out(&root, &reflect, reviewer, "pass", &valid_text);
        assert_eq!(out.status.code(), Some(2), "reviewer {reviewer:?}");
        assert_eq!(
            stderr_json(&out)["error"]["code"],
            json!(REVIEWER_CONFLICT),
            "reviewer {reviewer:?}"
        );
    }

    // Three invalid report paths (relative, `~`, absolute but missing).
    let missing = dir.path().join("does-not-exist.md");
    let missing_text = missing.to_string_lossy().to_string();
    for report_ref in [
        "reports/reflect.md".to_string(),
        "~/reports/reflect.md".to_string(),
        missing_text,
    ] {
        let out = validate_out(&root, &reflect, "reflect:reviewer", "pass", &report_ref);
        assert_eq!(
            out.status.code(),
            Some(2),
            "report_ref {report_ref:?} must be refused: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(
            stderr_json(&out)["error"]["code"],
            json!(REPORT_REF_INVALID),
            "report_ref {report_ref:?}"
        );
    }
    assert_eq!(
        count(&root, "validation_history"),
        before_rows,
        "no refused round is written"
    );

    // An existing absolute path from a different reviewer passes.
    validate_ok(&root, &reflect, "reflect:reviewer", &valid_text);
    assert_eq!(count(&root, "validation_history"), before_rows + 1);

    let show = assert_ok(
        &run(&[
            "learning",
            "task",
            "show",
            "--root",
            &text(&root),
            "--candidate-id",
            &reflect,
        ]),
        "task show",
    );
    assert_eq!(show["state"], json!("done"));
    assert_eq!(show["self_validated"], json!(false));

    // A different task kind keeps the old semantics: `report_ref` is free.
    let plain = create_task(&root, "a plain task", "claude-code:creator");
    validate_ok(&root, &plain, "reviewer:one", "report-1");
}

// ---------------------------------------------------------------------------
// C6: self_validated on a normal task
// ---------------------------------------------------------------------------

#[test]
fn c6_normal_task_self_validation_is_accepted_and_flagged() {
    let (_dir, root) = new_root();
    let creator = "claude-code:creator";
    let by_creator = create_task(&root, "finished by its creator", creator);
    validate_ok(&root, &by_creator, creator, "report-1");

    let by_reviewer = create_task(&root, "finished by a reviewer", creator);
    validate_ok(&root, &by_reviewer, "reviewer:one", "report-2");

    let by_owner = create_task(&root, "taken over and finished", creator);
    claim_task(&root, &by_owner, "opencode:owner2");
    validate_ok(&root, &by_owner, "opencode:owner2", "report-3");

    let open = create_task(&root, "still open", creator);

    let workspace = assert_ok(
        &run(&["learning", "workspace", "--root", &text(&root)]),
        "workspace",
    );
    let row = |wanted: &str| {
        workspace["tasks"]
            .as_array()
            .expect("tasks")
            .iter()
            .find(|row| row["candidate_id"] == wanted)
            .unwrap_or_else(|| panic!("missing workspace row {wanted}"))
            .clone()
    };
    assert_eq!(row(&by_creator)["self_validated"], json!(true));
    assert_eq!(row(&by_reviewer)["self_validated"], json!(false));
    assert_eq!(
        row(&by_owner)["self_validated"],
        json!(true),
        "the current owner counts as a self-review"
    );
    assert_eq!(
        row(&open)["self_validated"],
        json!(false),
        "no validation means false"
    );

    for (task, expected) in [
        (by_creator.as_str(), true),
        (by_reviewer.as_str(), false),
        (by_owner.as_str(), true),
    ] {
        let show = assert_ok(
            &run(&[
                "learning",
                "task",
                "show",
                "--root",
                &text(&root),
                "--candidate-id",
                task,
            ]),
            "task show",
        );
        assert_eq!(show["self_validated"], json!(expected), "{task}");
    }
}

// ---------------------------------------------------------------------------
// C6: routing shape and empty lists
// ---------------------------------------------------------------------------

#[test]
fn c6_review_routing_is_always_present_with_empty_lists_as_arrays() {
    let (_dir, root) = new_root();
    let id = ingest(
        &root,
        &knowledge_payload(
            "A candidate with no relationships",
            "extractor-a",
            "checkpoint-a",
            vec![evidence("only-ref", "user")],
            None,
        ),
    );

    let package = export(&root, &id);
    let routing = routing_of(&package);
    assert_eq!(routing["conflict_actors"], json!(["extractor-a", "user"]));
    assert_eq!(routing["prior_reviewers"], json!([]));
    assert_eq!(routing["related_tasks"], json!([]));
    assert_eq!(routing["related_candidates"], json!([]));
    assert_eq!(
        routing["context_refs"],
        json!(["claude:session-only-ref:line-1"])
    );
    assert_eq!(
        routing["reviewer_requirements"],
        json!(REVIEWER_REQUIREMENTS)
    );

    // After one independent round the prior reviewer is reported verbatim.
    validate_ok(&root, &id, "reviewer:one", "report-1");
    let package = export(&root, &id);
    assert_eq!(
        routing_of(&package)["prior_reviewers"],
        json!(["reviewer:one"])
    );
}

// ---------------------------------------------------------------------------
// §C/§A consistency: the watermark uses the same review identity fold
// ---------------------------------------------------------------------------

#[test]
fn c6_watermark_uses_the_same_review_identity_fold() {
    let (_dir, root) = new_root();
    let reflect = create_task(&root, &format!("{REFLECT_CLAIM_PREFIX} 折叠水位线"), "User");

    // A fullwidth/space variant of the creator: the §C guard would refuse this
    // write, so the row is inserted by hand and the watermark must ignore it.
    insert_validation_row(&root, &reflect, "  ｕｓｅｒ  ");
    let report = assert_ok(
        &run(&["learning", "anomalies", "--root", &text(&root), "--json"]),
        "anomalies",
    );
    assert_eq!(
        report["watermark"],
        json!(null),
        "the read-side fold identifies the creator: {report}"
    );

    // A later independent verdict moves the watermark.
    insert_validation_row(&root, &reflect, "reviewer-one");
    let report = assert_ok(
        &run(&["learning", "anomalies", "--root", &text(&root), "--json"]),
        "anomalies",
    );
    assert_eq!(report["watermark"]["task_id"], json!(reflect));
}
