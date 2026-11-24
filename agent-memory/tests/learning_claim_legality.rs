//! Stage 5 implementation-001: write-side data legality (§A).
//!
//! Acceptance matrix B1–B4 (the rendering half of B4 belongs to
//! implementation-002, which owns `learning inject` and the `show` text form):
//!
//! - B1 claim length: 499/500 pass, 501 is `CLAIM_TOO_LONG`; 500 Chinese code
//!   points and 500 single-code-point emoji pass; 251 emoji (502 code points)
//!   fail; the stored claim is the trimmed value and the golden identity is
//!   unchanged.
//! - B2 claim character classes: every enumerated range is rejected with the
//!   class name in the message; fences/separators and template lines are
//!   rejected; `C#` and `#123` pass; a leading/trailing LF is rejected before
//!   trimming.
//! - B3 actor id: `[A-Za-z0-9._:@/-]{1,64}` on all four write paths; 65 code
//!   points, whitespace, `=`, brackets, parentheses, full-width colon, control
//!   characters and `来源：` are rejected; `claude-code:abc` and `migrated:v3`
//!   pass.
//! - B4 derived_from: task-only, enum + length, knowledge carrying the key is
//!   rejected, absent = unknown and byte-compatible with v1, identity does not
//!   include it, and the first write wins on the duplicate path.
//!
//! Every test builds a synthetic learning root from `tempfile`; nothing here
//! reads a real learning root, a real archive, `~/.memory` or any default path.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use rusqlite::Connection;
use serde_json::{json, Value};

use agent_memory::learning::model::{
    candidate_id_for, validate_candidate, validate_derived_from, validate_task_claim,
    CLAIM_INVALID_TEXT, CLAIM_MAX_CODEPOINTS, CLAIM_TOO_LONG, DERIVED_FROM_MAX_CODEPOINTS,
    DERIVED_FROM_NOT_ALLOWED,
};
use agent_memory::learning::{LearningStore, ACTOR_INVALID, ACTOR_MAX_CODEPOINTS};

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

fn error_message(out: &Output) -> String {
    stderr_json(out)["error"]["message"]
        .as_str()
        .unwrap_or_default()
        .to_string()
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

fn stored_record(root: &Path, candidate_id: &str) -> (Value, String) {
    let json: String = connect(root)
        .query_row(
            "SELECT record_json FROM candidates WHERE candidate_id = ?1",
            [candidate_id],
            |row| row.get(0),
        )
        .expect("candidate row");
    (serde_json::from_str(&json).expect("record json"), json)
}

fn create_task_cli(root: &Path, claim: &str, actor: &str, derived_from: Option<&str>) -> Output {
    let root_text = text(root);
    let mut args = vec![
        "learning", "task", "create", "--root", &root_text, "--claim", claim, "--actor", actor,
    ];
    if let Some(value) = derived_from {
        args.push("--derived-from");
        args.push(value);
    }
    run(&args)
}

fn create_task_ok(root: &Path, claim: &str, actor: &str) -> Value {
    assert_ok(&create_task_cli(root, claim, actor, None), "task create")
}

fn span_sha256(tag: &str) -> String {
    let mut acc: u64 = 14_695_981_039_346_656_037;
    for byte in tag.as_bytes() {
        acc ^= u64::from(*byte);
        acc = acc.wrapping_mul(1_099_511_628_211);
    }
    format!("{acc:064x}")
}

/// A knowledge candidate shaped exactly like the S1 input contract.
fn knowledge_payload(claim: &str) -> Value {
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
        "source_checkpoint": format!("checkpoint:{claim}"),
        "extractor": "test",
        "created_at": "2026-11-01T00:00:00Z",
        "updated_at": "2026-11-01T00:00:00Z"
    })
}

/// The same payload `task create` builds, without going through the store.
fn task_payload(claim: &str, derived_from: Option<&str>) -> Value {
    let candidate_id = candidate_id_for(claim, "task", "task");
    let mut payload = json!({
        "schema_version": 1,
        "candidate_id": candidate_id,
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
    });
    if let Some(value) = derived_from {
        payload["derived_from"] = json!(value);
    }
    payload
}

// ===========================================================================
// B1 — claim length (code points, after trimming)
// ===========================================================================

#[test]
fn b1_claim_length_boundaries_are_code_points() {
    let (_dir, root) = new_root();

    for length in [499usize, 500usize] {
        let claim = "a".repeat(length);
        let created = check_create_ok(&root, &claim, &format!("len-{length}"));
        assert_eq!(created["claim"], json!(claim));
    }

    let over = "a".repeat(CLAIM_MAX_CODEPOINTS + 1);
    let out = create_task_cli(&root, &over, "claude-code:abc", None);
    assert_error(&out, 2, CLAIM_TOO_LONG);
    assert!(
        error_message(&out).contains("501"),
        "message must state the actual length: {}",
        error_message(&out)
    );

    // 500 Chinese code points (500 * 3 UTF-8 bytes) pass.
    let chinese = "汉".repeat(500);
    let created = check_create_ok(&root, &chinese, "chinese-500");
    assert_eq!(created["claim"], json!(chinese));

    // 500 single-code-point emoji (U+1F642) pass: 500 code points, 2000 bytes.
    let emoji = "\u{1F642}".repeat(500);
    let created = check_create_ok(&root, &emoji, "emoji-500");
    assert_eq!(created["claim"], json!(emoji));

    // `chars().count()` counts Unicode scalar values, so one U+1F642 is one
    // code point: 501 of them is the first failing boundary. (The plan's
    // "251 emoji (502 code points)" arithmetic assumes UTF-16 units; the
    // normative rule in §A1 is `chars().count()`, asserted here.)
    let emoji_over = "\u{1F642}".repeat(501);
    let out = create_task_cli(&root, &emoji_over, "claude-code:abc", None);
    assert_error(&out, 2, CLAIM_TOO_LONG);

    // A 502-code-point claim is rejected whichever code points it is made of:
    // 251 two-code-point sequences also cross the bound.
    let pair_over = "\u{1F642}\u{FE0F}".repeat(251);
    assert_eq!(pair_over.chars().count(), 502);
    let out = create_task_cli(&root, &pair_over, "claude-code:abc", None);
    assert_error(&out, 2, CLAIM_TOO_LONG);

    assert_eq!(
        count(&root, "task_events"),
        4,
        "only the four legal creates wrote rows"
    );
}

fn check_create_ok(root: &Path, claim: &str, tag: &str) -> Value {
    let out = create_task_cli(root, claim, "claude-code:abc", None);
    assert_ok(&out, tag)
}

#[test]
fn b1_stored_claim_is_trimmed_and_golden_identity_is_unchanged() {
    let (_dir, root) = new_root();
    let raw = "  Ship   the task write path  ";
    let trimmed = "Ship   the task write path";

    let created = assert_ok(
        &create_task_cli(&root, raw, "claude-code:abc", None),
        "create",
    );
    assert_eq!(created["claim"], json!(trimmed));

    // Identity normalization already trims and collapses whitespace, so the
    // stage 5 rules must not move the golden id.
    assert_eq!(
        created["candidate_id"],
        json!("cand_16a90a4cda60a0fd3dfd870d9be56866"),
        "golden candidate_id for `Ship the task write path`"
    );
    assert_eq!(
        created["candidate_id"],
        json!(candidate_id_for(raw, "task", "task"))
    );
    assert_eq!(
        created["candidate_id"],
        json!(candidate_id_for(trimmed, "task", "task"))
    );

    let (record, _) = stored_record(&root, created["candidate_id"].as_str().unwrap());
    assert_eq!(record["claim"], json!(trimmed), "storage value is trimmed");

    // The same claim without surrounding whitespace is a duplicate: trimming is
    // identity normalization, not a new identity.
    let duplicate = assert_ok(
        &create_task_cli(&root, trimmed, "opencode:other", None),
        "duplicate",
    );
    assert_eq!(duplicate["duplicate"], json!(true));
    assert_eq!(duplicate["candidate_id"], created["candidate_id"]);
}

// ===========================================================================
// B2 — claim character classes
// ===========================================================================

#[test]
fn b2_every_enumerated_unicode_range_is_rejected_with_its_class() {
    let (_dir, root) = new_root();

    let cases: Vec<(&str, String, &str)> = vec![
        ("LF", "a\nb".to_string(), "C0"),
        ("CR", "a\rb".to_string(), "C0"),
        ("TAB", "a\tb".to_string(), "C0"),
        ("DEL", "a\u{007F}b".to_string(), "DEL"),
        ("U+061C", "a\u{061C}b".to_string(), "Cf"),
        ("U+200B", "a\u{200B}b".to_string(), "Cf"),
        ("U+200C", "a\u{200C}b".to_string(), "Cf"),
        ("U+200D", "a\u{200D}b".to_string(), "Cf"),
        ("U+200E", "a\u{200E}b".to_string(), "Cf"),
        ("U+200F", "a\u{200F}b".to_string(), "Cf"),
        ("U+202A", "a\u{202A}b".to_string(), "Cf"),
        ("U+202B", "a\u{202B}b".to_string(), "Cf"),
        ("U+202C", "a\u{202C}b".to_string(), "Cf"),
        ("U+202D", "a\u{202D}b".to_string(), "Cf"),
        ("U+202E", "a\u{202E}b".to_string(), "Cf"),
        ("U+2060", "a\u{2060}b".to_string(), "Cf"),
        ("U+2061", "a\u{2061}b".to_string(), "Cf"),
        ("U+2062", "a\u{2062}b".to_string(), "Cf"),
        ("U+2063", "a\u{2063}b".to_string(), "Cf"),
        ("U+2064", "a\u{2064}b".to_string(), "Cf"),
        ("U+2066", "a\u{2066}b".to_string(), "Cf"),
        ("U+2067", "a\u{2067}b".to_string(), "Cf"),
        ("U+2068", "a\u{2068}b".to_string(), "Cf"),
        ("U+2069", "a\u{2069}b".to_string(), "Cf"),
        ("U+FEFF", "a\u{FEFF}b".to_string(), "Cf"),
        ("U+00AD", "a\u{00AD}b".to_string(), "Cf"),
        ("U+180E", "a\u{180E}b".to_string(), "Cf"),
        ("U+FFF9", "a\u{FFF9}b".to_string(), "Cf"),
        ("U+FFFA", "a\u{FFFA}b".to_string(), "Cf"),
        ("U+FFFB", "a\u{FFFB}b".to_string(), "Cf"),
        ("Zl U+2028", "a\u{2028}b".to_string(), "Zl"),
        ("Zp U+2029", "a\u{2029}b".to_string(), "Zp"),
    ];

    for (label, claim, class) in cases {
        let out = create_task_cli(&root, &claim, "claude-code:abc", None);
        assert_error(&out, 2, CLAIM_INVALID_TEXT);
        assert!(
            error_message(&out).contains(class),
            "{label}: message must name class {class}: {}",
            error_message(&out)
        );
    }

    assert_eq!(count(&root, "candidates"), 0, "no illegal claim was stored");
}

#[test]
fn b2_newlines_are_rejected_before_trimming() {
    let (_dir, root) = new_root();

    for claim in [
        "foo\n",
        "\nfoo",
        "foo\r",
        "foo\u{2028}",
        "foo\u{2029}",
        "\n",
    ] {
        let out = create_task_cli(&root, claim, "claude-code:abc", None);
        assert_error(&out, 2, CLAIM_INVALID_TEXT);
    }
    assert_eq!(count(&root, "candidates"), 0);
}

#[test]
fn b2_fences_separators_and_template_lines_are_rejected() {
    let (_dir, root) = new_root();

    let cases = [
        "use ```code``` here",
        "a --- b",
        "a *** b",
        "a ___ b",
        "=== injection",
        agent_memory::learning::COORDINATION_ZONE_START_LINE,
        agent_memory::learning::COORDINATION_ZONE_END_LINE,
    ];
    for claim in cases {
        let out = create_task_cli(&root, claim, "claude-code:abc", None);
        assert_error(&out, 2, CLAIM_INVALID_TEXT);
    }

    // `#` is deliberately not forbidden: only line-leading markup was a risk,
    // and a claim is a single line.
    for claim in ["C#", "#123", "C# and #123", "正常的：中英标点，ok"] {
        check_create_ok(&root, claim, claim);
    }
    assert_eq!(count(&root, "candidates"), 4);
}

#[test]
fn b2_direct_validator_matches_the_cli() {
    for (claim, code) in [
        ("plain claim", None),
        ("a\u{2028}b", Some(CLAIM_INVALID_TEXT)),
        ("a\u{007F}b", Some(CLAIM_INVALID_TEXT)),
        ("a\nb", Some(CLAIM_INVALID_TEXT)),
        // NUL cannot travel through a Windows command line; it is covered at
        // the validator layer only.
        ("a\0b", Some(CLAIM_INVALID_TEXT)),
        ("a```b", Some(CLAIM_INVALID_TEXT)),
    ] {
        let result = validate_candidate(&task_payload(claim, None));
        match code {
            None => assert!(result.is_ok(), "{claim:?} should pass"),
            Some(code) => assert_eq!(result.expect_err(claim).code, code),
        }
    }

    assert_eq!(
        validate_task_claim(&"a".repeat(501))
            .expect_err("too long")
            .code,
        CLAIM_TOO_LONG
    );
    assert!(validate_task_claim(&"a".repeat(500)).is_ok());
}

// ===========================================================================
// B3 — actor whitelist on all four write paths
// ===========================================================================

#[test]
fn b3_invalid_actor_values_are_rejected() {
    let (_dir, root) = new_root();

    let invalid = [
        "a".repeat(ACTOR_MAX_CODEPOINTS + 1),
        "a b".to_string(),
        "a=b".to_string(),
        "a[b".to_string(),
        "a]b".to_string(),
        "a(b".to_string(),
        "a)b".to_string(),
        "a\u{FF1A}b".to_string(), // full-width colon
        "a\u{0007}b".to_string(), // control character
        "来源：x".to_string(),
        "a\u{200B}b".to_string(),
    ];

    for actor in invalid {
        let out = create_task_cli(&root, "Actor whitelist", &actor, None);
        assert_error(&out, 2, ACTOR_INVALID);
    }
    assert_eq!(count(&root, "candidates"), 0);

    // Existing identities stay valid.
    for actor in ["claude-code:abc", "migrated:v3"] {
        let out = create_task_cli(&root, &format!("Actor ok {actor}"), actor, None);
        assert_ok(&out, "valid actor");
    }
}

#[test]
fn b3_actor_whitelist_applies_to_all_four_write_paths() {
    let (_dir, root) = new_root();
    let task = create_task_ok(&root, "Four write paths", "claude-code:abc");
    let id = task["candidate_id"].as_str().expect("id").to_string();
    let root_text = text(&root);

    // create
    let out = create_task_cli(&root, "Another task", "bad actor", None);
    assert_error(&out, 2, ACTOR_INVALID);
    assert_eq!(count(&root, "candidates"), 1);

    // claim
    let out = run(&[
        "learning",
        "task",
        "claim",
        "--root",
        &root_text,
        "--candidate-id",
        &id,
        "--actor",
        "bad actor",
    ]);
    assert_error(&out, 2, ACTOR_INVALID);
    assert_eq!(count(&root, "task_events"), 1, "claim wrote nothing");

    // abandon
    let out = run(&[
        "learning",
        "task",
        "abandon",
        "--root",
        &root_text,
        "--candidate-id",
        &id,
        "--actor",
        "bad actor",
        "--reason",
        "why not",
    ]);
    assert_error(&out, 2, ACTOR_INVALID);
    assert_eq!(count(&root, "task_events"), 1, "abandon wrote nothing");

    // reuse
    let out = run(&[
        "learning",
        "reuse",
        "--root",
        &root_text,
        "--candidate-id",
        &id,
        "--task",
        &id,
        "--outcome",
        "pass",
        "--actor",
        "bad actor",
    ]);
    assert_error(&out, 2, ACTOR_INVALID);
    assert_eq!(count(&root, "reuse_history"), 0, "reuse wrote nothing");

    // The same four commands with a legal actor still work.
    let out = run(&[
        "learning",
        "task",
        "claim",
        "--root",
        &root_text,
        "--candidate-id",
        &id,
        "--actor",
        "opencode:owner2",
    ]);
    assert_ok(&out, "legal claim");
    let out = run(&[
        "learning",
        "reuse",
        "--root",
        &root_text,
        "--candidate-id",
        &id,
        "--task",
        &id,
        "--outcome",
        "pass",
        "--actor",
        "opencode:owner2",
    ]);
    assert_ok(&out, "legal reuse");
}

// ===========================================================================
// B4 — derived_from
// ===========================================================================

#[test]
fn b4_task_accepts_the_three_enum_values_and_stores_them() {
    let (_dir, root) = new_root();

    for value in ["external", "internal", "unknown"] {
        let created = assert_ok(
            &create_task_cli(
                &root,
                &format!("Derived {value}"),
                "claude-code:abc",
                Some(value),
            ),
            value,
        );
        let id = created["candidate_id"].as_str().expect("id");
        let (record, raw) = stored_record(&root, id);
        assert_eq!(record["derived_from"], json!(value));
        assert!(raw.contains("\"derived_from\":\""));
    }
}

#[test]
fn b4_knowledge_candidates_may_not_carry_derived_from() {
    let (_dir, root) = new_root();
    let root_text = text(&root);

    let mut payload = knowledge_payload("Knowledge must not carry derived_from");
    payload["derived_from"] = json!("external");
    let error = validate_candidate(&payload).expect_err("knowledge derived_from");
    assert_eq!(error.code, DERIVED_FROM_NOT_ALLOWED);
    assert!(error.message.contains("task"));

    // Presence alone is rejected, even when the value is null: the v1 knowledge
    // shape must not grow the key at all.
    let mut null_payload = knowledge_payload("Knowledge with null derived_from");
    null_payload["derived_from"] = Value::Null;
    let error = validate_candidate(&null_payload).expect_err("knowledge null derived_from");
    assert_eq!(error.code, DERIVED_FROM_NOT_ALLOWED);

    // The CLI path reports the same code.
    let out = run(&[
        "learning",
        "ingest",
        "--root",
        &root_text,
        "--candidate",
        &payload.to_string(),
    ]);
    assert_error(&out, 2, DERIVED_FROM_NOT_ALLOWED);
    assert_eq!(count(&root, "candidates"), 0);
}

#[test]
fn b4_bad_values_and_length_are_rejected() {
    let (_dir, root) = new_root();

    let too_long = "a".repeat(DERIVED_FROM_MAX_CODEPOINTS + 1);
    for value in ["into", "", too_long.as_str()] {
        let out = create_task_cli(&root, "Bad derived_from", "claude-code:abc", Some(value));
        assert_error(&out, 2, DERIVED_FROM_NOT_ALLOWED);
    }
    assert_eq!(count(&root, "candidates"), 0);
    assert_eq!(validate_derived_from(None).expect("none"), None);
    assert_eq!(
        validate_derived_from(Some(" internal ")).expect("trimmed"),
        Some("internal".to_string())
    );
}

#[test]
fn b4_absent_field_is_unknown_and_byte_compatible_with_v1() {
    let (_dir, root) = new_root();

    let validated = validate_candidate(&task_payload("Absent provenance field", None))
        .expect("task payload without derived_from");
    assert_eq!(validated.record.derived_from, None);
    let parsed: Value = serde_json::from_str(&validated.payload_json).expect("payload json");
    assert!(
        parsed.get("derived_from").is_none(),
        "skip_serializing_if must omit the key: {}",
        validated.payload_json
    );
    // Byte-for-byte v1 shape: field order, compact separators, no key.
    let expected = r#"{"schema_version":1,"candidate_id":"cand_c2eac60b1f7b8461b1f5d675c26791bf","revision":1,"status":"candidate","claim":"Absent provenance field","target_kind":"task","proposed_target":"task","applicability":[],"counterexamples":[],"unknowns":[],"evidence":[],"contradicting_evidence":[],"dedup":{"query":"","matches":[],"disposition":"pending","checked_at":null},"reuse":[],"validation":[],"promotion":{"approval":null,"apply":null,"verification":null},"source_checkpoint":"task:cand_c2eac60b1f7b8461b1f5d675c26791bf","extractor":"task","created_at":"2026-11-01T00:00:00Z","updated_at":"2026-11-01T00:00:00Z"}"#;
    assert_eq!(validated.payload_json, expected, "canonical v1 bytes");

    // A knowledge record also keeps the exact v1 bytes.
    let knowledge = validate_candidate(&knowledge_payload("Knowledge bytes stay v1"))
        .expect("knowledge payload");
    let parsed: Value = serde_json::from_str(&knowledge.payload_json).expect("knowledge json");
    assert!(parsed.get("derived_from").is_none());

    // The stored task record has no key either; absent reads as unknown.
    let created = create_task_ok(&root, "Absent provenance field", "claude-code:abc");
    let id = created["candidate_id"].as_str().expect("id");
    let (record, raw) = stored_record(&root, id);
    assert_eq!(record.get("derived_from"), None);
    assert!(!raw.contains("derived_from"));
}

#[test]
fn b4_identity_ignores_derived_from_and_first_write_wins() {
    let (_dir, root) = new_root();
    let claim = "Same claim different provenance";

    let first = assert_ok(
        &create_task_cli(&root, claim, "claude-code:abc", Some("external")),
        "first create",
    );
    let first_id = first["candidate_id"].as_str().expect("id").to_string();
    assert_eq!(first["created"], json!(true));

    // Same claim, different derived_from: same candidate id, duplicate path,
    // nothing rewritten.
    let second = assert_ok(
        &create_task_cli(&root, claim, "opencode:other", Some("internal")),
        "duplicate create",
    );
    assert_eq!(second["duplicate"], json!(true));
    assert_eq!(second["candidate_id"], json!(first_id.clone()));
    assert_eq!(first_id, candidate_id_for(claim, "task", "task"));

    let (record, _) = stored_record(&root, &first_id);
    assert_eq!(
        record["derived_from"],
        json!("external"),
        "first write wins"
    );
    assert_eq!(count(&root, "candidates"), 1);
    assert_eq!(count(&root, "task_events"), 1);
}

#[test]
fn b4_derived_from_does_not_change_the_identity_hash() {
    let plain = candidate_id_for("Identity ignores provenance", "task", "task");
    let with_external = validate_candidate(&task_payload(
        "Identity ignores provenance",
        Some("external"),
    ))
    .expect("external");
    let with_unknown = validate_candidate(&task_payload(
        "Identity ignores provenance",
        Some("unknown"),
    ))
    .expect("unknown");

    assert_eq!(plain, with_external.record.candidate_id);
    assert_eq!(plain, with_unknown.record.candidate_id);
    assert_eq!(with_external.ingest_key, with_unknown.ingest_key);
    assert_ne!(
        with_external.payload_sha256, with_unknown.payload_sha256,
        "the payload hash does change: it is the record content, not the identity"
    );
}
