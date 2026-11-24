//! BOM hardening: every `@file` payload and every stdin body is parsed after
//! stripping exactly one leading UTF-8 BOM (U+FEFF). PowerShell 5.1 writes BOM
//! by default, so a BOM-prefixed request must behave exactly like a plain one.

use serde_json::{json, Value};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

mod support;

const SESSION: &str = "11111111-1111-4111-8111-111111111111";

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_agent-memory")
}

fn claude_line(uuid: &str, time: &str, kind: &str, content: Value) -> String {
    json!({
        "type": kind,
        "message": {"role": kind, "content": content},
        "timestamp": time,
        "sessionId": SESSION,
        "uuid": uuid,
        "isSidechain": false
    })
    .to_string()
}

fn config(env: &support::TestEnv) -> PathBuf {
    let root = env.root("claude");
    let project = root.join("proj");
    std::fs::create_dir_all(&project).unwrap();
    let lines = vec![
        claude_line(
            "u1",
            "2026-10-20T10:00:00Z",
            "user",
            json!("bom needle alpha"),
        ),
        claude_line(
            "a1",
            "2026-10-20T10:01:00Z",
            "assistant",
            json!([{"type": "text", "text": "bom answer beta"}]),
        ),
    ];
    env.write(
        &format!("claude/proj/{SESSION}.jsonl"),
        (lines.join("\n") + "\n").as_bytes(),
    );
    env.write_json(
        "bom-recall.json",
        &json!({
            "version": 2,
            "sources": [{
                "source_id": "cc",
                "kind": "claude_code",
                "root": root.to_string_lossy(),
                "enabled": true
            }],
            "source_order": ["cc"],
            "interpreter": {"kb": {"command": "C:/python/python.exe"}},
            "domain": "default",
            "telemetry": {"enabled": false}
        }),
    )
}

fn bom_bytes(text: &str) -> Vec<u8> {
    let mut bytes = vec![0xEF, 0xBB, 0xBF];
    bytes.extend_from_slice(text.as_bytes());
    bytes
}

fn run_request_file(config: &Path, op: &str, path: &Path) -> Vec<u8> {
    let output = Command::new(bin())
        .arg(op)
        .arg("--config")
        .arg(config)
        .arg("--request")
        .arg(format!("@{}", path.display()))
        .output()
        .expect("run with request file");
    assert_eq!(
        output.status.code(),
        Some(0),
        "{op} failed for {}: {}",
        path.display(),
        String::from_utf8_lossy(&output.stdout)
    );
    output.stdout
}

fn run_stdin(config: &Path, op: &str, body: &[u8]) -> Vec<u8> {
    let mut child = Command::new(bin())
        .arg(op)
        .arg("--config")
        .arg(config)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn with stdin");
    child.stdin.take().unwrap().write_all(body).unwrap();
    let output = child.wait_with_output().expect("collect stdin run");
    assert_eq!(
        output.status.code(),
        Some(0),
        "{op} failed for stdin: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    output.stdout
}

fn assert_equivalent(env: &support::TestEnv, config: &Path, op: &str, request: &str) {
    let plain = env.write(&format!("{op}-plain.json"), request.as_bytes());
    let with_bom = env.write(&format!("{op}-bom.json"), &bom_bytes(request));
    let from_plain_file = run_request_file(config, op, &plain);
    let from_bom_file = run_request_file(config, op, &with_bom);
    let from_stdin = run_stdin(config, op, request.as_bytes());
    let from_bom_stdin = run_stdin(config, op, &bom_bytes(request));
    assert_eq!(
        from_plain_file, from_bom_file,
        "{op}: a BOM-prefixed request file must answer like a plain one"
    );
    assert_eq!(
        from_plain_file, from_stdin,
        "{op}: stdin must answer like a request file"
    );
    assert_eq!(
        from_plain_file, from_bom_stdin,
        "{op}: BOM-prefixed stdin must answer like a plain request"
    );
    let value: Value = serde_json::from_slice(&from_bom_file).expect("JSON stdout");
    assert_eq!(value["errors"], json!([]), "{op}: unexpected errors");
}

#[test]
fn memory_search_accepts_bom_file_and_bom_stdin() {
    let env = support::TestEnv::new();
    let config = config(&env);
    assert_equivalent(
        &env,
        &config,
        "memory_search",
        &json!({"query": "needle"}).to_string(),
    );
}

#[test]
fn memory_recent_accepts_bom_file_and_bom_stdin() {
    let env = support::TestEnv::new();
    let config = config(&env);
    assert_equivalent(
        &env,
        &config,
        "memory_recent",
        &json!({"limit": 1, "tail_turns": 1}).to_string(),
    );
}

fn candidate() -> Value {
    fn span_sha256(tag: &str) -> String {
        let mut acc: u64 = 14_695_981_039_346_656_037;
        for byte in tag.as_bytes() {
            acc ^= u64::from(*byte);
            acc = acc.wrapping_mul(1_099_511_628_211);
        }
        format!("{acc:064x}")
    }
    json!({
        "schema_version": 1,
        "candidate_id": "",
        "revision": 1,
        "status": "candidate",
        "claim": "BOM prefixed candidates are still valid candidates",
        "target_kind": "rule",
        "proposed_target": "AGENTS.md#bom",
        "applicability": ["agent-memory"],
        "counterexamples": [],
        "unknowns": [],
        "evidence": [{
            "ref": "claude:session-bom:line-1",
            "span_sha256": span_sha256("bom"),
            "archive_generation": "g1",
            "observed_at": "2026-11-01T00:00:00Z",
            "actor": "user",
            "session_kind": "interactive",
            "origin_class": "owner",
            "evidence_kind": "user_statement",
            "recall_taint": false
        }],
        "contradicting_evidence": [],
        "dedup": {"query": "", "matches": [], "disposition": "pending", "checked_at": null},
        "reuse": [],
        "validation": [],
        "promotion": {"approval": null, "apply": null, "verification": null},
        "ingestions": [],
        "source_checkpoint": "checkpoint-bom",
        "extractor": "extractor-bom",
        "created_at": "2026-11-01T00:00:00Z",
        "updated_at": "2026-11-01T00:00:00Z"
    })
}

#[test]
fn learning_ingest_accepts_a_bom_prefixed_candidate_file() {
    let env = support::TestEnv::new();
    let root = env.path().join("learning");
    let init = Command::new(bin())
        .arg("learning")
        .arg("init")
        .arg("--root")
        .arg(&root)
        .output()
        .expect("init learning root");
    assert!(init.status.success());
    let path = env.write(
        "candidate.json",
        &bom_bytes(&serde_json::to_string_pretty(&candidate()).unwrap()),
    );
    let output = Command::new(bin())
        .arg("learning")
        .arg("ingest")
        .arg("--root")
        .arg(&root)
        .arg("--candidate")
        .arg(format!("@{}", path.display()))
        .output()
        .expect("ingest candidate");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        output.status.code(),
        Some(0),
        "learning ingest rejected a BOM-prefixed candidate: {stdout} {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = serde_json::from_str(&stdout).expect("ingest JSON");
    assert_eq!(value["ok"], json!(true), "{value}");
}

#[test]
fn recent_hook_reads_a_bom_prefixed_session_start_payload() {
    let env = support::TestEnv::new();
    let config = config(&env);
    let payload = json!({"session_id": SESSION}).to_string();
    let mut child = Command::new(bin())
        .arg("memory_recent")
        .arg("--config")
        .arg(&config)
        .arg("--hook-session-start")
        .arg("--now")
        .arg("2026-10-20T12:00:00Z")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn hook");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(&bom_bytes(&payload))
        .unwrap();
    let output = child.wait_with_output().expect("hook output");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(0));
    assert!(
        stdout.contains(&format!("本会话：claude_code {SESSION}")),
        "BOM-prefixed SessionStart payload lost the session id:\n{stdout}"
    );
}
