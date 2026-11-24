//! Stage 3 S1: candidate store in an explicit learning root.
//!
//! Every case uses a synthetic temporary root; nothing here touches an Archive,
//! a recall configuration or a real learning root.

use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Barrier};
use std::thread;

use rusqlite::Connection;
use serde_json::{json, Map, Value};

use agent_memory::learning::model::{candidate_id_for, ingest_key_for, validate_candidate};
use agent_memory::learning::{LearningStore, DB_FILE};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn span_sha256(tag: &str) -> String {
    let mut acc: u64 = 14_695_981_039_346_656_037;
    for byte in tag.as_bytes() {
        acc ^= u64::from(*byte);
        acc = acc.wrapping_mul(1_099_511_628_211);
    }
    format!("{acc:064x}")
}

fn evidence(tag: &str) -> Value {
    json!({
        "ref": format!("claude:session-{tag}:line-1"),
        "span_sha256": span_sha256(tag),
        "archive_generation": "g1",
        "observed_at": "2026-11-01T00:00:00Z",
        "actor": "user",
        "session_kind": "interactive",
        "origin_class": "owner",
        "evidence_kind": "user_statement",
        "recall_taint": false
    })
}

fn candidate() -> Value {
    json!({
        "schema_version": 1,
        "candidate_id": "",
        "revision": 1,
        "status": "candidate",
        "claim": "Prefer explicit roots for every store",
        "target_kind": "rule",
        "proposed_target": "AGENTS.md#explicit-roots",
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
        "source_checkpoint": "checkpoint-a",
        "extractor": "extractor-a",
        "created_at": "2026-11-01T00:00:00Z",
        "updated_at": "2026-11-01T00:00:00Z"
    })
}

fn init_root(root: &Path) -> LearningStore {
    LearningStore::init(root).expect("init learning root")
}

fn count(db: &Path, table: &str) -> i64 {
    let conn = Connection::open(db).expect("open learning.db");
    conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
        row.get(0)
    })
    .expect("count rows")
}

fn table_names(db: &Path) -> Vec<String> {
    let conn = Connection::open(db).expect("open learning.db");
    let mut stmt = conn
        .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
        .expect("prepare table list");
    stmt.query_map([], |row| row.get::<_, String>(0))
        .expect("query table list")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect table list")
}

fn cli() -> Command {
    Command::new(env!("CARGO_BIN_EXE_agent-memory"))
}

fn stdout_json(out: &std::process::Output) -> Value {
    serde_json::from_slice(&out.stdout).unwrap_or_else(|error| {
        panic!(
            "stdout is not JSON ({error}): {}",
            String::from_utf8_lossy(&out.stdout)
        )
    })
}

fn stderr_json(out: &std::process::Output) -> Value {
    serde_json::from_slice(&out.stderr).unwrap_or_else(|error| {
        panic!(
            "stderr is not JSON ({error}): {}",
            String::from_utf8_lossy(&out.stderr)
        )
    })
}

/// Minimal RFC 3339 shape check (the crate's `chrono` is not a dev-dependency).
fn looks_rfc3339(value: &str) -> bool {
    let bytes = value.as_bytes();
    value.len() >= 20
        && bytes.get(4) == Some(&b'-')
        && bytes.get(7) == Some(&b'-')
        && bytes.get(10) == Some(&b'T')
        && bytes.get(13) == Some(&b':')
}

// ---------------------------------------------------------------------------
// 1. Positive path: init + ingest + show + list (library and CLI)
// ---------------------------------------------------------------------------

#[test]
fn init_ingest_show_list_roundtrip() {
    let dir = tempfile::tempdir().expect("temp root");
    let root = dir.path().join("learning");
    let mut store = init_root(&root);

    let outcome = store.ingest(&candidate()).expect("ingest");
    assert!(outcome.ok);
    assert!(outcome.candidate_id.starts_with("cand_"));
    assert_eq!(outcome.candidate_id.len(), "cand_".len() + 32);
    assert!(outcome.candidate_id[5..]
        .bytes()
        .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()));
    assert!(outcome.ingest_key.starts_with("ing_"));
    assert_eq!(outcome.ingest_key.len(), "ing_".len() + 32);
    assert_eq!(outcome.payload_sha256.len(), 64);
    assert_eq!(outcome.revision, 1);
    assert_eq!(outcome.status, "candidate");
    assert!(!outcome.replayed);
    assert_eq!(outcome.evidence_added, 1);

    let view = store.show(&outcome.candidate_id).expect("show");
    assert!(view.ok);
    assert_eq!(view.candidate.candidate_id, outcome.candidate_id);
    assert_eq!(view.candidate.revision, 1);
    assert_eq!(view.candidate.status, "candidate");
    assert_eq!(
        view.candidate.claim,
        "Prefer explicit roots for every store"
    );
    assert_eq!(view.payload_sha256, outcome.payload_sha256);
    assert_eq!(view.ingestions.len(), 1);
    assert_eq!(view.ingestions[0].ingest_key, outcome.ingest_key);
    assert_eq!(view.ingestions[0].extractor, "extractor-a");
    assert_eq!(view.ingestions[0].source_checkpoint, "checkpoint-a");
    assert_eq!(view.revisions.len(), 1);
    assert_eq!(view.revisions[0].payload_sha256, outcome.payload_sha256);
    assert_eq!(view.status_history.len(), 1);
    assert_eq!(view.status_history[0].status, "candidate");
    assert_eq!(view.status_history[0].revision, 1);
    assert!(looks_rfc3339(&view.status_history[0].recorded_at));

    let list = store.list(None).expect("list");
    assert!(list.ok);
    assert_eq!(list.count, 1);
    assert_eq!(list.candidates[0].candidate_id, outcome.candidate_id);
    assert_eq!(list.candidates[0].target_kind, "rule");
    assert_eq!(list.candidates[0].revision, 1);

    let filtered = store.list(Some("candidate")).expect("filtered list");
    assert_eq!(filtered.count, 1);
}

#[test]
fn stable_identity_normalizes_case_and_whitespace() {
    assert_eq!(
        candidate_id_for("  Prefer   EXPLICIT\troots ", "rule", "AGENTS.md#Roots"),
        candidate_id_for("prefer explicit roots", "rule", "agents.md#roots")
    );
    assert_ne!(
        candidate_id_for("prefer explicit roots", "rule", "agents.md#roots"),
        candidate_id_for("prefer explicit roots", "note", "agents.md#roots")
    );
    assert_eq!(
        ingest_key_for("  extractor-a ", " checkpoint-a  "),
        ingest_key_for("extractor-a", "checkpoint-a")
    );
    assert_ne!(
        ingest_key_for("extractor-a", "checkpoint-a"),
        ingest_key_for("extractor-a", "checkpoint-b")
    );
    // A supplied id must equal the computed one.
    let mut value = candidate();
    value["candidate_id"] = json!(candidate_id_for(
        "Prefer explicit roots for every store",
        "rule",
        "AGENTS.md#explicit-roots"
    ));
    assert!(validate_candidate(&value).is_ok());
}

#[test]
fn payload_hash_ignores_object_key_order() {
    let dir = tempfile::tempdir().expect("temp root");
    let root = dir.path().join("learning");
    let mut store = init_root(&root);

    let mut reordered_evidence = Map::new();
    for key in [
        "recall_taint",
        "evidence_kind",
        "origin_class",
        "session_kind",
        "actor",
        "observed_at",
        "archive_generation",
        "span_sha256",
        "ref",
    ] {
        let source = evidence("e1");
        reordered_evidence.insert(key.to_string(), source[key].clone());
    }
    let mut value = candidate();
    value["evidence"] = json!([Value::Object(reordered_evidence)]);

    let first = store.ingest(&candidate()).expect("ingest canonical");
    let second = store.ingest(&value).expect("ingest reordered");
    assert_eq!(second.candidate_id, first.candidate_id);
    assert_eq!(second.payload_sha256, first.payload_sha256);
    assert!(second.replayed);
    assert_eq!(count(&root.join(DB_FILE), "candidates"), 1);
}

#[test]
fn cli_roundtrip_with_at_file() {
    let dir = tempfile::tempdir().expect("temp root");
    let root = dir.path().join("learning");
    let root_text = root.to_string_lossy().to_string();

    let help = cli().arg("--help").output().expect("help");
    assert!(help.status.success());
    assert!(String::from_utf8_lossy(&help.stdout).contains("learning"));

    let init = cli()
        .args(["learning", "init", "--root", &root_text])
        .output()
        .expect("cli init");
    assert_eq!(init.status.code(), Some(0), "{init:?}");
    let init_json = stdout_json(&init);
    assert_eq!(init_json["ok"], true);
    assert_eq!(init_json["schema_version"], 1);
    assert!(root.join(DB_FILE).is_file());

    let candidate_path = dir.path().join("candidate.json");
    std::fs::write(
        &candidate_path,
        serde_json::to_vec_pretty(&candidate()).expect("serialize candidate"),
    )
    .expect("write candidate");

    let ingest = cli()
        .args([
            "learning",
            "ingest",
            "--root",
            &root_text,
            "--candidate",
            &format!("@{}", candidate_path.display()),
        ])
        .output()
        .expect("cli ingest");
    assert_eq!(ingest.status.code(), Some(0), "{ingest:?}");
    assert!(ingest.stderr.is_empty(), "success writes no stderr");
    let ingest_json = stdout_json(&ingest);
    let candidate_id = ingest_json["candidate_id"]
        .as_str()
        .expect("candidate id")
        .to_string();
    assert_eq!(ingest_json["replayed"], false);
    assert_eq!(ingest_json["evidence_added"], 1);

    let show = cli()
        .args([
            "learning",
            "show",
            "--root",
            &root_text,
            "--candidate-id",
            &candidate_id,
        ])
        .output()
        .expect("cli show");
    assert_eq!(show.status.code(), Some(0), "{show:?}");
    let show_json = stdout_json(&show);
    assert_eq!(
        show_json["candidate"]["candidate_id"],
        candidate_id.as_str()
    );
    assert_eq!(
        show_json["ingestions"]
            .as_array()
            .expect("ingestions")
            .len(),
        1
    );

    let list = cli()
        .args([
            "learning",
            "list",
            "--root",
            &root_text,
            "--status",
            "candidate",
        ])
        .output()
        .expect("cli list");
    assert_eq!(list.status.code(), Some(0), "{list:?}");
    let list_json = stdout_json(&list);
    assert_eq!(list_json["count"], 1);
    assert_eq!(
        list_json["candidates"][0]["candidate_id"],
        candidate_id.as_str()
    );
}

// ---------------------------------------------------------------------------
// 2. Strict v1 schema: every deviation is rejected
// ---------------------------------------------------------------------------

#[test]
fn rejects_every_schema_deviation() {
    let mut cases: Vec<(&str, Value)> = Vec::new();

    let mut value = candidate();
    value["schema_version"] = json!(2);
    cases.push(("schema_version 2", value));

    let mut value = candidate();
    value["schema_version"] = json!("1");
    cases.push(("schema_version as string", value));

    let mut value = candidate();
    value["revision"] = json!(0);
    cases.push(("revision 0", value));

    let mut value = candidate();
    value["revision"] = json!("1");
    cases.push(("revision as string", value));

    let mut value = candidate();
    value["status"] = json!("submitted");
    cases.push(("status submitted", value));

    let mut value = candidate();
    value["status"] = json!("promoted");
    cases.push(("status promoted", value));

    let mut value = candidate();
    value["target_kind"] = json!("policy");
    cases.push(("unknown target_kind", value));

    let mut value = candidate();
    value["claim"] = json!("   ");
    cases.push(("blank claim", value));

    let mut value = candidate();
    value["proposed_target"] = json!("\t");
    cases.push(("blank proposed_target", value));

    let mut value = candidate();
    value["source_checkpoint"] = json!(" ");
    cases.push(("blank source_checkpoint", value));

    let mut value = candidate();
    value["extractor"] = json!("");
    cases.push(("blank extractor", value));

    let mut value = candidate();
    value["unexpected"] = json!(1);
    cases.push(("unknown top-level field", value));

    let mut value = candidate();
    value["evidence"][0]["unexpected"] = json!(1);
    cases.push(("unknown evidence field", value));

    let mut value = candidate();
    value["dedup"]["unexpected"] = json!(1);
    cases.push(("unknown dedup field", value));

    let mut value = candidate();
    value["promotion"]["unexpected"] = json!(1);
    cases.push(("unknown promotion field", value));

    let mut value = candidate();
    value.as_object_mut().expect("object").remove("dedup");
    cases.push(("missing dedup", value));

    let mut value = candidate();
    value["evidence"][0]
        .as_object_mut()
        .expect("evidence object")
        .remove("actor");
    cases.push(("missing evidence.actor", value));

    let mut value = candidate();
    value["applicability"] = json!("agent-memory");
    cases.push(("applicability not an array", value));

    let mut value = candidate();
    value["unknowns"] = json!([1, 2]);
    cases.push(("unknowns not strings", value));

    let mut value = candidate();
    value["evidence"] = json!([]);
    cases.push(("empty evidence", value));

    let mut value = candidate();
    value["evidence"][0]["span_sha256"] = json!("A".repeat(64));
    cases.push(("uppercase span_sha256", value));

    let mut value = candidate();
    value["evidence"][0]["span_sha256"] = json!("a".repeat(63));
    cases.push(("short span_sha256", value));

    let mut value = candidate();
    value["evidence"][0]["span_sha256"] = json!("z".repeat(64));
    cases.push(("non-hex span_sha256", value));

    let mut value = candidate();
    value["evidence"][0]["observed_at"] = json!("yesterday");
    cases.push(("bad observed_at", value));

    let mut value = candidate();
    value["evidence"][0]["recall_taint"] = json!("true");
    cases.push(("recall_taint as string", value));

    let mut value = candidate();
    value["evidence"][0]["session_kind"] = json!("manual");
    cases.push(("bad session_kind", value));

    let mut value = candidate();
    value["evidence"][0]["origin_class"] = json!("third_party");
    cases.push(("bad origin_class", value));

    let mut value = candidate();
    value["evidence"][0]["evidence_kind"] = json!("rumor");
    cases.push(("bad evidence_kind", value));

    let mut value = candidate();
    value["evidence"][0]["ref"] = json!("  ");
    cases.push(("blank evidence ref", value));

    let mut value = candidate();
    value["created_at"] = json!("2026-11-01T00:00:00Z");
    value["updated_at"] = json!("2026-10-31T23:59:59Z");
    cases.push(("created_at after updated_at", value));

    let mut value = candidate();
    value["created_at"] = json!("not-a-time");
    cases.push(("bad created_at", value));

    let mut value = candidate();
    value["promotion"]["approval"] = json!({"by": "owner"});
    cases.push(("non-null promotion.approval", value));

    let mut value = candidate();
    value["ingestions"] = json!([{"ingest_key": "forged"}]);
    cases.push(("non-empty ingestions", value));

    let mut value = candidate();
    value["reuse"] = json!([{"mode": "xor"}]);
    cases.push(("non-empty reuse", value));

    let mut value = candidate();
    value["validation"] = json!([{"check": "ok"}]);
    cases.push(("non-empty validation", value));

    let mut value = candidate();
    value["dedup"]["query"] = json!("explicit roots");
    cases.push(("dedup query filled", value));

    let mut value = candidate();
    value["dedup"]["disposition"] = json!("duplicate");
    cases.push(("dedup disposition", value));

    let mut value = candidate();
    value["dedup"]["checked_at"] = json!("2026-11-01T00:00:00Z");
    cases.push(("dedup checked_at", value));

    let mut value = candidate();
    value["candidate_id"] = json!("cand_00000000000000000000000000000000");
    cases.push(("candidate_id mismatch", value));

    let mut value = candidate();
    value["contradicting_evidence"] = json!([{"ref": "x"}]);
    cases.push(("incomplete contradicting evidence", value));

    for (name, value) in cases {
        let error = validate_candidate(&value).expect_err(name);
        assert_eq!(error.code, "INVALID_CANDIDATE", "{name}: {error}");
        assert_eq!(error.exit_code(), 2, "{name}");
    }
}

#[test]
fn rejects_non_explicit_learning_roots() {
    for raw in ["", "relative/learning", "./learning", "~/learning"] {
        let error = LearningStore::init(Path::new(raw)).expect_err(raw);
        assert_eq!(error.code, "LEARNING_ROOT_INVALID", "{raw}: {error}");
        assert_eq!(error.exit_code(), 2, "{raw}");
    }

    let dir = tempfile::tempdir().expect("temp root");
    let out = cli()
        .args(["learning", "init", "--root", "relative/learning"])
        .current_dir(dir.path())
        .output()
        .expect("cli relative root");
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(stderr_json(&out)["error"]["code"], "LEARNING_ROOT_INVALID");
    assert!(!dir.path().join("relative/learning").exists());
}

#[test]
fn init_refuses_non_empty_foreign_directory() {
    let dir = tempfile::tempdir().expect("temp root");
    let root = dir.path().join("occupied");
    std::fs::create_dir_all(&root).expect("occupied root");
    std::fs::write(root.join("notes.txt"), b"user data").expect("write foreign file");

    let error = LearningStore::init(&root).expect_err("must refuse");
    assert_eq!(error.code, "LEARNING_STATE");
    assert_eq!(error.exit_code(), 3);
    assert!(!root.join(DB_FILE).exists());
    assert!(root.join("notes.txt").is_file());
}

#[test]
fn init_is_idempotent_and_preserves_data() {
    let dir = tempfile::tempdir().expect("temp root");
    let root = dir.path().join("learning");
    let mut store = init_root(&root);
    let outcome = store.ingest(&candidate()).expect("ingest");
    drop(store);

    let reopened = init_root(&root);
    let view = reopened
        .show(&outcome.candidate_id)
        .expect("show after re-init");
    assert_eq!(view.ingestions.len(), 1);
    assert_eq!(count(&root.join(DB_FILE), "candidates"), 1);
}

#[test]
fn concurrent_init_never_deletes_the_committed_schema() {
    let dir = tempfile::tempdir().expect("temp root");
    let root = dir.path().join("learning");
    let barrier = Arc::new(Barrier::new(8));
    let outcomes = thread::scope(|scope| {
        let mut handles = Vec::new();
        for _ in 0..8 {
            let barrier = Arc::clone(&barrier);
            let root = &root;
            handles.push(scope.spawn(move || {
                barrier.wait();
                LearningStore::init(root).map(|store| store.db_path().to_path_buf())
            }));
        }
        handles
            .into_iter()
            .map(|handle| handle.join().expect("initializer thread"))
            .collect::<Vec<_>>()
    });

    assert_eq!(outcomes.len(), 8);
    for outcome in outcomes {
        let db = outcome.expect("every initializer observes a valid store");
        assert!(db.is_file());
    }
    let mut store = LearningStore::open(&root).expect("open committed schema");
    store
        .ingest(&candidate())
        .expect("ingest after concurrent init");
    assert_eq!(count(&root.join(DB_FILE), "candidates"), 1);
}

// ---------------------------------------------------------------------------
// 3./4. Idempotency, key conflicts and concurrency
// ---------------------------------------------------------------------------

#[test]
fn same_key_same_content_replays_without_new_rows() {
    let dir = tempfile::tempdir().expect("temp root");
    let root = dir.path().join("learning");
    let db = root.join(DB_FILE);
    let mut store = init_root(&root);

    let first = store.ingest(&candidate()).expect("first ingest");
    let second = store.ingest(&candidate()).expect("replay");
    assert!(second.replayed);
    assert_eq!(second.evidence_added, 0);
    assert_eq!(second.candidate_id, first.candidate_id);
    assert_eq!(second.revision, first.revision);
    assert_eq!(second.ingest_key, first.ingest_key);
    assert_eq!(second.payload_sha256, first.payload_sha256);
    assert_eq!(second.status, "candidate");

    assert_eq!(count(&db, "candidates"), 1);
    assert_eq!(count(&db, "candidate_revisions"), 1);
    assert_eq!(count(&db, "ingestions"), 1);
    assert_eq!(count(&db, "status_history"), 1);
}

#[test]
fn same_key_different_content_conflicts_without_writing() {
    let dir = tempfile::tempdir().expect("temp root");
    let root = dir.path().join("learning");
    let db = root.join(DB_FILE);
    let mut store = init_root(&root);
    let first = store.ingest(&candidate()).expect("first ingest");

    let mut changed = candidate();
    changed["claim"] = json!("A different claim under the same ingest key");
    let error = store.ingest(&changed).expect_err("conflict");
    assert_eq!(error.code, "INGEST_KEY_CONFLICT");
    assert_eq!(error.exit_code(), 2);

    assert_eq!(count(&db, "candidates"), 1);
    assert_eq!(count(&db, "candidate_revisions"), 1);
    assert_eq!(count(&db, "ingestions"), 1);
    assert_eq!(count(&db, "status_history"), 1);
    let view = store.show(&first.candidate_id).expect("show");
    assert_eq!(view.payload_sha256, first.payload_sha256);
    assert_eq!(view.candidate.evidence.len(), 1);
}

#[test]
fn concurrent_same_key_same_content_commits_once() {
    let dir = tempfile::tempdir().expect("temp root");
    let root = dir.path().join("learning");
    let db = root.join(DB_FILE);
    init_root(&root);

    let barrier = Arc::new(Barrier::new(6));
    let mut handles = Vec::new();
    for _ in 0..6 {
        let root = root.clone();
        let barrier = Arc::clone(&barrier);
        handles.push(thread::spawn(move || {
            let candidate = candidate();
            let mut store = LearningStore::open(&root).expect("open store");
            barrier.wait();
            store.ingest(&candidate)
        }));
    }
    let outcomes: Vec<_> = handles
        .into_iter()
        .map(|handle| handle.join().expect("thread"))
        .collect();

    for outcome in &outcomes {
        let outcome = outcome.as_ref().expect("every concurrent replay succeeds");
        assert_eq!(outcome.status, "candidate");
        assert_eq!(
            outcome.candidate_id,
            outcomes[0].as_ref().unwrap().candidate_id
        );
    }
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| !outcome.as_ref().unwrap().replayed)
            .count(),
        1,
        "exactly one writer performs the write"
    );
    assert_eq!(count(&db, "candidates"), 1);
    assert_eq!(count(&db, "candidate_revisions"), 1);
    assert_eq!(count(&db, "ingestions"), 1);
    assert_eq!(count(&db, "status_history"), 1);
}

#[test]
fn concurrent_same_key_different_content_conflicts() {
    let dir = tempfile::tempdir().expect("temp root");
    let root = dir.path().join("learning");
    let db = root.join(DB_FILE);
    init_root(&root);

    let barrier = Arc::new(Barrier::new(4));
    let mut handles = Vec::new();
    for index in 0..4 {
        let root = root.clone();
        let barrier = Arc::clone(&barrier);
        handles.push(thread::spawn(move || {
            let mut candidate = candidate();
            candidate["claim"] = json!(format!("Concurrent claim number {index}"));
            let mut store = LearningStore::open(&root).expect("open store");
            barrier.wait();
            store.ingest(&candidate)
        }));
    }
    let outcomes: Vec<_> = handles
        .into_iter()
        .map(|handle| handle.join().expect("thread"))
        .collect();

    let successes: Vec<_> = outcomes.iter().filter(|outcome| outcome.is_ok()).collect();
    assert_eq!(successes.len(), 1, "only one payload wins the key");
    for outcome in &outcomes {
        if let Err(error) = outcome {
            assert_eq!(error.code, "INGEST_KEY_CONFLICT", "{error}");
        }
    }
    assert_eq!(count(&db, "candidates"), 1);
    assert_eq!(count(&db, "candidate_revisions"), 1);
    assert_eq!(count(&db, "ingestions"), 1);
    assert_eq!(count(&db, "status_history"), 1);
}

// ---------------------------------------------------------------------------
// 5. Same revision: deduplicated evidence append only
// ---------------------------------------------------------------------------

#[test]
fn same_revision_appends_deduplicated_evidence() {
    let dir = tempfile::tempdir().expect("temp root");
    let root = dir.path().join("learning");
    let mut store = init_root(&root);
    let first = store.ingest(&candidate()).expect("first ingest");

    let mut next = candidate();
    next["source_checkpoint"] = json!("checkpoint-b");
    next["extractor"] = json!("extractor-b");
    next["updated_at"] = json!("2026-11-01T01:00:00Z");
    next["evidence"] = json!([evidence("e1"), evidence("e2"), evidence("e2")]);
    next["contradicting_evidence"] = json!([evidence("c1"), evidence("c1")]);
    let second = store.ingest(&next).expect("append evidence");
    assert!(!second.replayed);
    assert_eq!(second.candidate_id, first.candidate_id);
    assert_eq!(second.revision, 1);
    assert_eq!(
        second.evidence_added, 2,
        "one new body item plus one counterexample"
    );

    let view = store.show(&first.candidate_id).expect("show");
    assert_eq!(view.candidate.revision, 1);
    assert_eq!(view.candidate.evidence.len(), 2);
    assert_eq!(view.candidate.contradicting_evidence.len(), 1);
    assert_eq!(view.candidate.updated_at, "2026-11-01T01:00:00Z");
    assert_eq!(
        view.candidate.source_checkpoint, "checkpoint-a",
        "an append keeps the record's own ingest provenance"
    );
    assert_eq!(view.candidate.extractor, "extractor-a");
    assert_eq!(
        view.revisions.len(),
        1,
        "the revision-1 snapshot is never rewritten"
    );
    assert_eq!(view.revisions[0].payload_sha256, first.payload_sha256);
    assert_eq!(view.ingestions.len(), 2);
    let appending = view
        .ingestions
        .iter()
        .find(|ingestion| ingestion.ingest_key == second.ingest_key)
        .expect("the appending batch keeps its own ingestion row");
    assert_eq!(appending.extractor, "extractor-b");
    assert_eq!(appending.source_checkpoint, "checkpoint-b");
    assert!(view
        .ingestions
        .iter()
        .any(|ingestion| ingestion.ingest_key == first.ingest_key
            && ingestion.extractor == "extractor-a"
            && ingestion.source_checkpoint == "checkpoint-a"));
    assert_eq!(view.status_history.len(), 1);

    let replay = store.ingest(&next).expect("replay the append");
    assert!(replay.replayed);
    assert_eq!(replay.evidence_added, 0);
}

#[test]
fn same_revision_field_conflict_does_not_partially_commit() {
    let dir = tempfile::tempdir().expect("temp root");
    let root = dir.path().join("learning");
    let db = root.join(DB_FILE);
    let mut store = init_root(&root);
    let first = store.ingest(&candidate()).expect("first ingest");

    let mut conflicting = candidate();
    conflicting["source_checkpoint"] = json!("checkpoint-b");
    conflicting["updated_at"] = json!("2026-11-01T02:00:00Z");
    conflicting["evidence"] = json!([evidence("e1"), evidence("e9")]);
    conflicting["applicability"] = json!(["a-different-scope"]);
    let error = store.ingest(&conflicting).expect_err("field conflict");
    assert_eq!(error.code, "CANDIDATE_REVISION_CONFLICT");
    assert_eq!(error.exit_code(), 2);

    assert_eq!(count(&db, "ingestions"), 1);
    let view = store.show(&first.candidate_id).expect("show");
    assert_eq!(
        view.candidate.evidence.len(),
        1,
        "no partial evidence append"
    );
    assert_eq!(view.candidate.updated_at, "2026-11-01T00:00:00Z");
    assert_eq!(view.payload_sha256, first.payload_sha256);
    assert_eq!(json!(view.candidate.applicability), json!(["agent-memory"]));
}

// ---------------------------------------------------------------------------
// 6. Revision history
// ---------------------------------------------------------------------------

#[test]
fn revision_increment_preserves_old_snapshot() {
    let dir = tempfile::tempdir().expect("temp root");
    let root = dir.path().join("learning");
    let db = root.join(DB_FILE);
    let mut store = init_root(&root);
    let first = store.ingest(&candidate()).expect("revision 1");

    let mut revision_two = candidate();
    revision_two["revision"] = json!(2);
    revision_two["source_checkpoint"] = json!("checkpoint-b");
    revision_two["updated_at"] = json!("2026-11-02T00:00:00Z");
    revision_two["evidence"] = json!([evidence("e1"), evidence("e3")]);
    let second = store.ingest(&revision_two).expect("revision 2");
    assert_eq!(second.revision, 2);
    assert_eq!(second.candidate_id, first.candidate_id);
    assert_eq!(second.evidence_added, 2);

    let view = store.show(&first.candidate_id).expect("show");
    assert_eq!(view.candidate.revision, 2);
    assert_eq!(view.candidate.evidence.len(), 2);
    assert_eq!(view.revisions.len(), 2);
    assert_eq!(view.revisions[0].revision, 1);
    assert_eq!(view.revisions[0].payload_sha256, first.payload_sha256);
    assert_eq!(view.revisions[1].revision, 2);
    assert_eq!(view.revisions[1].payload_sha256, second.payload_sha256);
    assert_eq!(view.status_history.len(), 2);
    assert_eq!(view.status_history[1].revision, 2);
    assert_eq!(view.ingestions.len(), 2);
    assert_eq!(count(&db, "candidate_revisions"), 2);

    let replay = store.ingest(&candidate()).expect("replay revision 1");
    assert!(replay.replayed);
    assert_eq!(replay.revision, 1);
    let view = store.show(&first.candidate_id).expect("show");
    assert_eq!(
        view.candidate.revision, 2,
        "replay never rolls the candidate back"
    );
}

#[test]
fn new_candidate_must_start_at_revision_one() {
    let dir = tempfile::tempdir().expect("temp root");
    let root = dir.path().join("learning");
    let mut store = init_root(&root);
    let mut invalid = candidate();
    invalid["revision"] = json!(2);

    let error = store
        .ingest(&invalid)
        .expect_err("a new candidate must not start at revision 2");
    assert_eq!(error.code, "CANDIDATE_REVISION_CONFLICT");
    let db = root.join(DB_FILE);
    for table in [
        "candidates",
        "candidate_revisions",
        "ingestions",
        "status_history",
    ] {
        assert_eq!(count(&db, table), 0, "failed ingest left {table} rows");
    }

    invalid["revision"] = json!(1);
    invalid["source_checkpoint"] = json!("checkpoint-b");
    store
        .ingest(&invalid)
        .expect("revision 1 succeeds after the rejected attempt");
}

#[test]
fn lower_and_skipped_revisions_are_rejected() {
    let dir = tempfile::tempdir().expect("temp root");
    let root = dir.path().join("learning");
    let mut store = init_root(&root);
    let first = store.ingest(&candidate()).expect("revision 1");

    let mut revision_two = candidate();
    revision_two["revision"] = json!(2);
    revision_two["source_checkpoint"] = json!("checkpoint-b");
    revision_two["updated_at"] = json!("2026-11-02T00:00:00Z");
    store.ingest(&revision_two).expect("revision 2");

    let mut lower = candidate();
    lower["source_checkpoint"] = json!("checkpoint-c");
    lower["updated_at"] = json!("2026-11-03T00:00:00Z");
    lower["evidence"] = json!([evidence("e4")]);
    let error = store.ingest(&lower).expect_err("lower revision");
    assert_eq!(error.code, "CANDIDATE_REVISION_CONFLICT");

    let mut skipped = candidate();
    skipped["revision"] = json!(4);
    skipped["source_checkpoint"] = json!("checkpoint-d");
    skipped["updated_at"] = json!("2026-11-04T00:00:00Z");
    let error = store.ingest(&skipped).expect_err("skipped revision");
    assert_eq!(error.code, "CANDIDATE_REVISION_CONFLICT");

    let mut zero = candidate();
    zero["revision"] = json!(0);
    let error = store.ingest(&zero).expect_err("revision zero");
    assert_eq!(error.code, "INVALID_CANDIDATE");

    let view = store.show(&first.candidate_id).expect("show");
    assert_eq!(view.candidate.revision, 2);
    assert_eq!(view.revisions.len(), 2);
    assert_eq!(view.ingestions.len(), 2);
}

// ---------------------------------------------------------------------------
// 7. Atomicity and reopen
// ---------------------------------------------------------------------------

#[test]
fn failed_ingest_rolls_back_and_reopen_sees_nothing() {
    let dir = tempfile::tempdir().expect("temp root");
    let root = dir.path().join("learning");
    let db = root.join(DB_FILE);
    init_root(&root);

    {
        let conn = Connection::open(&db).expect("open fixture db");
        conn.execute_batch(
            "CREATE TRIGGER fail_status_history BEFORE INSERT ON status_history \
             BEGIN SELECT RAISE(ABORT, 'synthetic mid-ingest failure'); END;",
        )
        .expect("create failure trigger");
    }

    let mut store = LearningStore::open(&root).expect("open store");
    let error = store
        .ingest(&candidate())
        .expect_err("trigger aborts the ingest");
    assert_eq!(error.exit_code(), 3, "{error}");
    drop(store);

    for table in [
        "candidates",
        "candidate_revisions",
        "ingestions",
        "status_history",
    ] {
        assert_eq!(count(&db, table), 0, "{table} must be empty after rollback");
    }

    {
        let conn = Connection::open(&db).expect("reopen fixture db");
        conn.execute_batch("DROP TRIGGER fail_status_history")
            .expect("drop trigger");
    }

    let mut store = LearningStore::open(&root).expect("reopen store");
    let outcome = store.ingest(&candidate()).expect("ingest after recovery");
    assert!(!outcome.replayed);
    for table in [
        "candidates",
        "candidate_revisions",
        "ingestions",
        "status_history",
    ] {
        assert_eq!(
            count(&db, table),
            1,
            "{table} holds exactly the committed ingest"
        );
    }
    let view = store
        .show(&outcome.candidate_id)
        .expect("show after reopen");
    assert_eq!(view.ingestions.len(), 1);
}

#[test]
fn committed_state_survives_reopen() {
    let dir = tempfile::tempdir().expect("temp root");
    let root = dir.path().join("learning");
    let mut store = init_root(&root);
    let outcome = store.ingest(&candidate()).expect("ingest");
    drop(store);

    let store = LearningStore::open(&root).expect("reopen");
    let view = store.show(&outcome.candidate_id).expect("show");
    assert_eq!(
        view.candidate.claim,
        "Prefer explicit roots for every store"
    );
    assert_eq!(view.payload_sha256, outcome.payload_sha256);
    assert_eq!(view.ingestions.len(), 1);
    assert_eq!(store.list(None).expect("list").count, 1);
}

// ---------------------------------------------------------------------------
// 8. Fail-closed database checks
// ---------------------------------------------------------------------------

#[test]
fn open_requires_an_existing_learning_database() {
    let dir = tempfile::tempdir().expect("temp root");
    let error = LearningStore::open(&dir.path().join("missing")).expect_err("no database");
    assert_eq!(error.code, "LEARNING_DB_MISSING");
    assert_eq!(error.exit_code(), 6);

    let empty = dir.path().join("empty");
    std::fs::create_dir_all(&empty).expect("empty root");
    let error = LearningStore::open(&empty).expect_err("no database in empty dir");
    assert_eq!(error.code, "LEARNING_DB_MISSING");
}

#[test]
fn open_rejects_unknown_user_version() {
    let dir = tempfile::tempdir().expect("temp root");
    let root = dir.path().join("learning");
    init_root(&root);
    let db = root.join(DB_FILE);
    {
        let conn = Connection::open(&db).expect("open db");
        conn.pragma_update(None, "user_version", 6)
            .expect("bump user_version");
    }
    let error = LearningStore::open(&root).expect_err("unknown version");
    assert_eq!(error.code, "SCHEMA_VERSION_UNSUPPORTED");
    assert_eq!(error.exit_code(), 6);
}

#[test]
fn open_rejects_foreign_and_corrupt_databases() {
    let dir = tempfile::tempdir().expect("temp root");

    let foreign = dir.path().join("foreign");
    std::fs::create_dir_all(&foreign).expect("foreign root");
    {
        let conn = Connection::open(foreign.join(DB_FILE)).expect("foreign db");
        conn.execute_batch(
            "CREATE TABLE archive_objects (sha256 TEXT PRIMARY KEY, bytes INTEGER);",
        )
        .expect("foreign schema");
        conn.pragma_update(None, "user_version", 1)
            .expect("set user_version");
    }
    let error = LearningStore::open(&foreign).expect_err("foreign schema");
    assert_eq!(error.code, "LEARNING_SCHEMA_INVALID");
    assert_eq!(error.exit_code(), 6);

    let partial = dir.path().join("partial");
    std::fs::create_dir_all(&partial).expect("partial root");
    {
        let conn = Connection::open(partial.join(DB_FILE)).expect("partial db");
        conn.execute_batch("CREATE TABLE candidates (candidate_id TEXT PRIMARY KEY);")
            .expect("partial schema");
        conn.pragma_update(None, "user_version", 1)
            .expect("set user_version");
    }
    let error = LearningStore::open(&partial).expect_err("incomplete schema");
    assert_eq!(error.code, "LEARNING_SCHEMA_INVALID");

    let corrupt = dir.path().join("corrupt");
    std::fs::create_dir_all(&corrupt).expect("corrupt root");
    std::fs::write(corrupt.join(DB_FILE), b"this is not a sqlite database").expect("corrupt db");
    let error = LearningStore::open(&corrupt).expect_err("corrupt database");
    assert_eq!(error.code, "LEARNING_SCHEMA_INVALID");
    assert_eq!(error.exit_code(), 6);
}

#[test]
fn schema_is_v5_with_the_eleven_audit_tables() {
    let dir = tempfile::tempdir().expect("temp root");
    let root = dir.path().join("learning");
    let mut store = init_root(&root);
    let outcome = store.ingest(&candidate()).expect("ingest");
    drop(store);

    let db = root.join(DB_FILE);
    let conn = Connection::open(&db).expect("open db");
    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("user_version");
    assert_eq!(version, 5);
    let mode: String = conn
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .expect("journal mode");
    assert_eq!(mode.to_ascii_lowercase(), "wal");

    let tables = table_names(&db);
    for table in [
        "candidates",
        "candidate_revisions",
        "ingestions",
        "status_history",
        "dedup_history",
        "reuse_history",
        "validation_history",
        "task_events",
        "event_clock",
        "qualification_history",
        "rejection_overrides",
    ] {
        assert!(
            tables.contains(&table.to_string()),
            "missing {table}: {tables:?}"
        );
    }

    // (candidate_id, revision) is unique.
    let duplicate_revision = conn.execute(
        "INSERT INTO candidate_revisions \
         (candidate_id, revision, status, record_json, payload_sha256, recorded_at) \
         VALUES (?1, 1, 'candidate', '{}', ?2, '2026-11-01T00:00:00Z')",
        rusqlite::params![outcome.candidate_id, outcome.payload_sha256],
    );
    assert!(
        matches!(
            duplicate_revision,
            Err(rusqlite::Error::SqliteFailure(ref failure, _))
                if failure.code == rusqlite::ErrorCode::ConstraintViolation
        ),
        "duplicate (candidate_id, revision) must be rejected: {duplicate_revision:?}"
    );

    // ingest_key is unique.
    let duplicate_ingestion = conn.execute(
        "INSERT INTO ingestions (ingest_key, candidate_id, revision, payload_sha256, created_at) \
         VALUES (?1, ?2, 1, ?3, '2026-11-01T00:00:00Z')",
        rusqlite::params![
            outcome.ingest_key,
            outcome.candidate_id,
            outcome.payload_sha256
        ],
    );
    assert!(
        matches!(
            duplicate_ingestion,
            Err(rusqlite::Error::SqliteFailure(ref failure, _))
                if failure.code == rusqlite::ErrorCode::ConstraintViolation
        ),
        "duplicate ingest_key must be rejected: {duplicate_ingestion:?}"
    );
}

// ---------------------------------------------------------------------------
// 9. Isolation: the learning root is not an archive/recall root
// ---------------------------------------------------------------------------

#[test]
fn learning_root_holds_only_the_learning_database() {
    let dir = tempfile::tempdir().expect("temp root");
    let root = dir.path().join("learning");
    let mut store = init_root(&root);
    store.ingest(&candidate()).expect("ingest");
    drop(store);

    let mut names: Vec<String> = std::fs::read_dir(&root)
        .expect("read learning root")
        .map(|entry| {
            entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .to_string()
        })
        .collect();
    names.sort();
    for name in &names {
        assert!(
            name == DB_FILE
                || name == &format!("{DB_FILE}-wal")
                || name == &format!("{DB_FILE}-shm"),
            "unexpected file in the learning root: {name} ({names:?})"
        );
    }
    assert!(names.contains(&DB_FILE.to_string()));

    for forbidden in [
        "archive",
        "state",
        "derived",
        "index",
        "config.local.json",
        "recall.json",
        ".agent-memory",
        "memory",
    ] {
        assert!(
            !root.join(forbidden).exists(),
            "learning root must not create {forbidden}"
        );
    }
}

#[test]
fn cli_has_no_default_learning_root() {
    let dir = tempfile::tempdir().expect("temp root");
    let home = dir.path().join("fake-home");
    std::fs::create_dir_all(&home).expect("fake home");

    let out = cli()
        .args(["learning", "init"])
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("AGENT_MEMORY_LEARNING_ROOT", &home)
        .env("LEARNING_ROOT", &home)
        .output()
        .expect("cli without root");
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(stderr_json(&out)["error"]["code"], "INVALID_USAGE");
    assert!(!home.join(DB_FILE).exists());
}

// ---------------------------------------------------------------------------
// CLI error surface
// ---------------------------------------------------------------------------

#[test]
fn cli_errors_are_structured_and_use_stable_exit_codes() {
    let dir = tempfile::tempdir().expect("temp root");
    let root = dir.path().join("learning");
    let root_text = root.to_string_lossy().to_string();

    let no_subcommand = cli().arg("learning").output().expect("cli learning");
    assert_eq!(no_subcommand.status.code(), Some(2));
    assert_eq!(stderr_json(&no_subcommand)["ok"], false);
    assert_eq!(
        stderr_json(&no_subcommand)["error"]["code"],
        "INVALID_USAGE"
    );

    let unknown = cli()
        .args(["learning", "promote", "--root", &root_text])
        .output()
        .expect("cli unknown subcommand");
    assert_eq!(unknown.status.code(), Some(2));
    assert_eq!(stderr_json(&unknown)["error"]["code"], "INVALID_USAGE");

    let missing_db = cli()
        .args(["learning", "list", "--root", &root_text])
        .output()
        .expect("cli missing db");
    assert_eq!(missing_db.status.code(), Some(6));
    assert_eq!(
        stderr_json(&missing_db)["error"]["code"],
        "LEARNING_DB_MISSING"
    );

    let init = cli()
        .args(["learning", "init", "--root", &root_text])
        .output()
        .expect("cli init");
    assert_eq!(init.status.code(), Some(0));

    let missing_candidate = cli()
        .args([
            "learning",
            "show",
            "--root",
            &root_text,
            "--candidate-id",
            "cand_unknown",
        ])
        .output()
        .expect("cli show missing");
    assert_eq!(missing_candidate.status.code(), Some(6));
    assert_eq!(
        stderr_json(&missing_candidate)["error"]["code"],
        "CANDIDATE_NOT_FOUND"
    );

    let not_json = cli()
        .args([
            "learning",
            "ingest",
            "--root",
            &root_text,
            "--candidate",
            "{not json",
        ])
        .output()
        .expect("cli invalid json");
    assert_eq!(not_json.status.code(), Some(2));
    assert_eq!(stderr_json(&not_json)["error"]["code"], "INVALID_CANDIDATE");

    let mut bad = candidate();
    bad["schema_version"] = json!(3);
    let bad_path = dir.path().join("bad.json");
    std::fs::write(&bad_path, serde_json::to_vec(&bad).unwrap()).expect("write bad candidate");
    let bad_schema = cli()
        .args([
            "learning",
            "ingest",
            "--root",
            &root_text,
            "--candidate",
            &format!("@{}", bad_path.display()),
        ])
        .output()
        .expect("cli bad schema");
    assert_eq!(bad_schema.status.code(), Some(2));
    assert_eq!(
        stderr_json(&bad_schema)["error"]["code"],
        "INVALID_CANDIDATE"
    );

    let bad_status = cli()
        .args([
            "learning", "list", "--root", &root_text, "--status", "promoted",
        ])
        .output()
        .expect("cli bad status");
    assert_eq!(bad_status.status.code(), Some(2));
    assert_eq!(
        stderr_json(&bad_status)["error"]["code"],
        "INVALID_CANDIDATE"
    );
}

#[test]
fn cli_reports_key_conflicts_with_exit_code_two() {
    let dir = tempfile::tempdir().expect("temp root");
    let root = dir.path().join("learning");
    let root_text = root.to_string_lossy().to_string();
    let init = cli()
        .args(["learning", "init", "--root", &root_text])
        .output()
        .expect("cli init");
    assert_eq!(init.status.code(), Some(0), "{init:?}");

    let first_path = dir.path().join("first.json");
    std::fs::write(&first_path, serde_json::to_vec(&candidate()).unwrap()).expect("write first");
    let first = cli()
        .args([
            "learning",
            "ingest",
            "--root",
            &root_text,
            "--candidate",
            &format!("@{}", first_path.display()),
        ])
        .output()
        .expect("cli first ingest");
    assert_eq!(first.status.code(), Some(0));

    let mut conflicting = candidate();
    conflicting["claim"] = json!("Conflicting payload under the same ingest key");
    let conflicting_path = dir.path().join("conflicting.json");
    std::fs::write(&conflicting_path, serde_json::to_vec(&conflicting).unwrap())
        .expect("write conflicting");
    let conflict = cli()
        .args([
            "learning",
            "ingest",
            "--root",
            &root_text,
            "--candidate",
            &format!("@{}", conflicting_path.display()),
        ])
        .output()
        .expect("cli conflicting ingest");
    assert_eq!(conflict.status.code(), Some(2));
    assert_eq!(
        stderr_json(&conflict)["error"]["code"],
        "INGEST_KEY_CONFLICT"
    );

    let list = cli()
        .args(["learning", "list", "--root", &root_text])
        .output()
        .expect("cli list");
    assert_eq!(list.status.code(), Some(0));
    assert_eq!(stdout_json(&list)["count"], 1);
}

#[test]
fn cli_accepts_inline_json() {
    let dir = tempfile::tempdir().expect("temp root");
    let root = dir.path().join("learning");
    let root_text = root.to_string_lossy().to_string();
    let init = cli()
        .args(["learning", "init", "--root", &root_text])
        .output()
        .expect("cli init");
    assert_eq!(init.status.code(), Some(0), "{init:?}");
    let inline = serde_json::to_string(&candidate()).expect("inline json");
    let out = cli()
        .args([
            "learning",
            "ingest",
            "--root",
            &root_text,
            "--candidate",
            &inline,
        ])
        .output()
        .expect("cli inline ingest");
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(stdout_json(&out)["replayed"], false);
}
