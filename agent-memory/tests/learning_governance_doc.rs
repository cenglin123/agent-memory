//! Stage 6 C10: the governance document is executable and consistent with the
//! crate constants.
//!
//! Two assertions:
//!
//! * every stage-6 anomaly kind and every stage-6 error-code constant (read
//!   from the crate re-exports, never from a second hand-written list) appears
//!   in `docs/GOVERNANCE.md`;
//! * the three documented command shapes (`anomalies`, `export`, `validate`)
//!   are extracted from the document's "文档命令抽跑样例" block, placeholders
//!   are filled with a synthetic root, and the exit codes match the document.
//!
//! The fixture is a `tempfile` root driven through the real CLI; no real
//! learning root, Archive or recall configuration is read.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::{json, Value};

use agent_memory::learning::{
    LearningStore, ANOMALY_KINDS, DOWNGRADE_BLOCKED_ORIGIN_TASKS, NOTHING_TO_CLEAR,
    ORIGIN_CLAIM_COPY, ORIGIN_TARGET_NOT_TASK, ORIGIN_TASKS_DUPLICATE, ORIGIN_TASKS_NOT_ALLOWED,
    ORIGIN_TASKS_TOO_MANY, ORIGIN_TASK_UNKNOWN, REFLECT_CLAIM_PREFIX, REJECTED_CLAIM_MATCH,
    REPORT_REF_INVALID, REVIEWER_CONFLICT, TASK_NOT_QUALIFIABLE,
};

const DOC_SECTION: &str = "## 文档命令抽跑样例";

fn doc_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("docs")
        .join("GOVERNANCE.md")
}

fn doc() -> String {
    std::fs::read_to_string(doc_path()).expect("read docs/GOVERNANCE.md")
}

fn cli() -> Command {
    Command::new(env!("CARGO_BIN_EXE_agent-memory"))
}

fn run(args: &[String]) -> Output {
    cli().args(args).output().expect("run agent-memory")
}

fn text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn create_task(root: &Path, claim: &str, actor: &str) -> String {
    let root_text = text(root);
    let out = run(&[
        "learning".into(),
        "task".into(),
        "create".into(),
        "--root".into(),
        root_text,
        "--claim".into(),
        claim.into(),
        "--actor".into(),
        actor.into(),
    ]);
    assert_eq!(out.status.code(), Some(0), "task create");
    let value: Value = serde_json::from_slice(&out.stdout).expect("task create JSON");
    value["candidate_id"].as_str().expect("id").to_string()
}

fn knowledge_payload(origin: &str, extractor: &str) -> Value {
    json!({
        "schema_version": 1,
        "candidate_id": "",
        "revision": 1,
        "status": "candidate",
        "claim": "治理文档命令样例的经验条目",
        "target_kind": "workflow",
        "proposed_target": "workflow:governance-sample",
        "applicability": ["agent-memory"],
        "counterexamples": [],
        "unknowns": [],
        "evidence": [{
            "ref": "claude:session-c10:line-1",
            "span_sha256": "0000000000000000000000000000000000000000000000000000000000000002",
            "archive_generation": "g1",
            "observed_at": "2026-11-01T00:00:00Z",
            "actor": extractor,
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
        "source_checkpoint": "checkpoint-c10",
        "extractor": extractor,
        "created_at": "2026-11-01T00:00:00Z",
        "updated_at": "2026-11-01T00:00:00Z",
        "origin_tasks": [origin]
    })
}

fn ingest(root: &Path, payload: &Value) -> String {
    let root_text = text(root);
    let out = run(&[
        "learning".into(),
        "ingest".into(),
        "--root".into(),
        root_text,
        "--candidate".into(),
        payload.to_string(),
    ]);
    assert_eq!(out.status.code(), Some(0), "ingest");
    let value: Value = serde_json::from_slice(&out.stdout).expect("ingest JSON");
    value["candidate_id"].as_str().expect("id").to_string()
}

/// Command shapes extracted from the documented sample block: only lines that
/// start with `agent-memory ` inside the fence count.
fn documented_commands(document: &str) -> Vec<Vec<String>> {
    let start = document
        .find(DOC_SECTION)
        .unwrap_or_else(|| panic!("missing section {DOC_SECTION}"));
    let mut in_fence = false;
    let mut commands = Vec::new();
    for line in document[start..].lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if !in_fence {
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("agent-memory ") {
            commands.push(rest.split_whitespace().map(str::to_string).collect());
        }
    }
    commands
}

fn find_command<'a>(commands: &'a [Vec<String>], subcommand: &str) -> &'a Vec<String> {
    commands
        .iter()
        .find(|args| args.len() >= 2 && args[0] == "learning" && args[1] == subcommand)
        .unwrap_or_else(|| panic!("documented command for learning {subcommand}"))
}

fn substituted(command: &[String], values: &[(&str, String)]) -> Vec<String> {
    command
        .iter()
        .map(|token| {
            values
                .iter()
                .find(|(placeholder, _)| token == placeholder)
                .map(|(_, value)| value.clone())
                .unwrap_or_else(|| token.clone())
        })
        .collect()
}

#[test]
fn c10_governance_document_lists_every_stage6_constant() {
    let document = doc();

    // Signal kinds come from the crate's fixed table.
    for kind in ANOMALY_KINDS {
        assert!(
            document.contains(kind),
            "GOVERNANCE.md must list the signal kind {kind}"
        );
    }

    // Stage-6 error-code constants, read from the crate re-exports.
    let codes = [
        REVIEWER_CONFLICT,
        REPORT_REF_INVALID,
        ORIGIN_TASKS_TOO_MANY,
        ORIGIN_TASKS_DUPLICATE,
        ORIGIN_TASK_UNKNOWN,
        ORIGIN_TARGET_NOT_TASK,
        ORIGIN_TASKS_NOT_ALLOWED,
        ORIGIN_CLAIM_COPY,
        DOWNGRADE_BLOCKED_ORIGIN_TASKS,
        TASK_NOT_QUALIFIABLE,
        NOTHING_TO_CLEAR,
        REJECTED_CLAIM_MATCH,
    ];
    for code in codes {
        assert!(
            document.contains(code),
            "GOVERNANCE.md must list the error code {code}"
        );
    }

    // Fixed prefixes and thresholds the document states as current rules.
    assert!(document.contains(REFLECT_CLAIM_PREFIX));
    assert!(document.contains("400"));
    assert!(document.contains("60"));
    assert!(document.contains("24"));

    // Placeholders, never a real home path.
    assert!(document.contains("<UserRoot>"));
    assert!(document.contains("<R>"));
    assert!(!document.contains("C:\\Users\\"));
    assert!(!document.contains("C:/Users/"));
}

#[test]
fn c10_documented_commands_run_on_a_synthetic_root() {
    let document = doc();
    let commands = documented_commands(&document);
    let dir = tempfile::tempdir().expect("temp dir");
    let root = dir.path().join("learning");
    drop(LearningStore::init(&root).expect("init learning root"));

    // Origin task (completed by an independent reviewer) and the knowledge
    // candidate that documents the experience.
    let origin = create_task(&root, "文档样例来源任务", "jia");
    let report = dir.path().join("reflect").join("c10").join("report.md");
    std::fs::create_dir_all(report.parent().expect("parent")).expect("audit package");
    std::fs::write(&report, "# audit\n").expect("write report");
    let report_text = text(&report);
    let validated = run(&[
        "learning".into(),
        "validate".into(),
        "--root".into(),
        text(&root),
        "--candidate-id".into(),
        origin.clone(),
        "--reviewer".into(),
        "bing".into(),
        "--verdict".into(),
        "pass".into(),
        "--report-ref".into(),
        report_text.clone(),
    ]);
    assert_eq!(validated.status.code(), Some(0), "origin validate");

    let knowledge = ingest(&root, &knowledge_payload(&origin, "yi"));
    let reflect = create_task(&root, &format!("{REFLECT_CLAIM_PREFIX} 文档样例"), "runner");

    let values = [
        ("<R>", text(&root)),
        ("<knowledge_candidate_id>", knowledge.clone()),
        ("<reflect_task_id>", reflect.clone()),
        ("<report_path>", report_text.clone()),
    ];

    // 1) anomalies exits 0 and prints the JSON envelope.
    let anomalies = substituted(find_command(&commands, "anomalies"), &values);
    let out = run(&anomalies);
    assert_eq!(
        out.status.code(),
        Some(0),
        "documented anomalies: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let report_json: Value = serde_json::from_slice(&out.stdout).expect("anomalies JSON");
    assert_eq!(report_json["ok"], json!(true));
    assert!(report_json["signals"].is_array());

    // 2) export exits 0 and carries the review routing material.
    let export = substituted(find_command(&commands, "export"), &values);
    let out = run(&export);
    assert_eq!(
        out.status.code(),
        Some(0),
        "documented export: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let package: Value = serde_json::from_slice(&out.stdout).expect("export JSON");
    assert_eq!(package["ok"], json!(true));
    assert!(package["review_routing"]["conflict_actors"].is_array());

    // 3) validate exits 0 for an existing absolute report path.
    let validate = substituted(find_command(&commands, "validate"), &values);
    let out = run(&validate);
    assert_eq!(
        out.status.code(),
        Some(0),
        "documented validate: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // 4) qualified exits 0 and prints the read-only qualification envelope.
    let qualified = substituted(find_command(&commands, "qualified"), &values);
    let out = run(&qualified);
    assert_eq!(
        out.status.code(),
        Some(0),
        "documented qualified: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let report_json: Value =
        serde_json::from_slice(&out.stdout).expect("qualified JSON");
    assert_eq!(report_json["ok"], json!(true));
    assert!(report_json["qualified"].is_array());

    // 5) The documented failure shape: a missing report path is exit 2 with
    //    REPORT_REF_INVALID (documented in the same section).
    let missing = dir.path().join("reflect").join("missing").join("report.md");
    let broken = substituted(
        find_command(&commands, "validate"),
        &[
            ("<R>", text(&root)),
            ("<reflect_task_id>", reflect.clone()),
            ("<report_path>", text(&missing)),
        ],
    );
    let out = run(&broken);
    assert_eq!(out.status.code(), Some(2));
    let error: Value = serde_json::from_slice(&out.stderr).expect("error JSON");
    assert_eq!(error["error"]["code"], json!(REPORT_REF_INVALID));
}
